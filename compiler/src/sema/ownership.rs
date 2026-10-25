//! Ownership rules checked on the finished HIR (see `hir::ownership` and
//! DESIGN.md "Память"):
//!
//! * interior pointers (`&s.f`, `&a[i]`, `&slice[i]`, `&global`) and slices
//!   of arrays that are not variables are borrowed-only: they may be read
//!   through or passed to a parameter that does not escape;
//! * a promoted method reached through an interface must not let `this`
//!   escape (its receiver is an interior pointer);
//! * `func Drop()` destructors: signature, `this` must not escape, no
//!   explicit calls;
//! * values whose type has a destructor are move-only: a place of such a
//!   type cannot be copied, and such values cannot be slice/map elements,
//!   tuple elements, enum payloads or task results.

use crate::diagnostics::{Diagnostic, Diagnostics};
use crate::hir::ownership::{drop_method, has_dtor, interior_pin, interior_vars, is_interior_addr, moved_return, param_facts, pointer_pin, walk_function, ParamFacts, Pin, Pos};
use crate::hir::*;
use crate::span::Span;
use crate::types::{Type, TypeId};
use std::collections::HashSet;

pub(crate) fn check(p: &Program, diags: &mut Diagnostics) {
    let facts = param_facts(p);
    let mut c = Checker { p, facts: &facts, diags, file: 0, seen: HashSet::new(), seen_msgs: HashSet::new(), scope: usize::MAX };
    c.decls();
    for (i, f) in p.funcs.iter().enumerate() {
        c.func(FuncId(i as u32), f);
    }
}

struct Checker<'a, 'd> {
    p: &'a Program,
    facts: &'a ParamFacts,
    diags: &'d mut Diagnostics,
    file: FileId,
    seen: HashSet<(FileId, u32, u32)>,
    seen_msgs: HashSet<(FileId, usize, String)>,
    /// Current function (or usize::MAX for declarations).
    scope: usize,
}

/// The `return x` operands of `stmts` that move a local (see
/// [`moved_return`]).
fn moved_returns(p: &Program, f: &Function, stmts: &[Stmt], out: &mut HashSet<*const Expr>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Return(ReturnValue::Value(e)) if moved_return(p, f, e).is_some() => {
                out.insert(e as *const Expr);
            }
            StmtKind::If { then, else_, .. } => {
                moved_returns(p, f, &then.stmts, out);
                if let Some(b) = else_ {
                    moved_returns(p, f, &b.stmts, out);
                }
            }
            StmtKind::Loop { body, .. }
            | StmtKind::ForRange { body, .. }
            | StmtKind::ForSlice { body, .. }
            | StmtKind::ForString { body, .. }
            | StmtKind::ForMap { body, .. }
            | StmtKind::Block(body) => moved_returns(p, f, &body.stmts, out),
            StmtKind::TryBlock { body, handler, .. } => {
                moved_returns(p, f, &body.stmts, out);
                moved_returns(p, f, &handler.stmts, out);
            }
            StmtKind::Switch(sw) => {
                for c in &sw.cases {
                    moved_returns(p, f, &c.body.stmts, out);
                }
                if let Some(d) = &sw.default {
                    moved_returns(p, f, &d.stmts, out);
                }
            }
            _ => {}
        }
    }
}

/// [`crate::hir::dump::DeclNames`] relative to namespace `ns`.
struct InNs<'a> {
    p: &'a Program,
    ns: Option<NamespaceId>,
}

impl InNs<'_> {
    fn pick(&self, ns: NamespaceId, name: &str, full: &str) -> String {
        if self.ns == Some(ns) && !crate::sema::stdlib::is_std_namespace(&full) { name.to_string() } else { full.to_string() }
    }
}

impl crate::hir::dump::DeclNames for InNs<'_> {
    fn struct_name(&self, id: u32) -> String {
        self.p.structs.get(id as usize).map(|s| self.pick(s.namespace, &s.name, &s.full_name)).unwrap_or_default()
    }
    fn interface_name(&self, id: u32) -> String {
        self.p.interfaces.get(id as usize).map(|s| self.pick(s.namespace, &s.name, &s.full_name)).unwrap_or_default()
    }
    fn enum_name(&self, id: u32) -> String {
        self.p.enums.get(id as usize).map(|s| self.pick(s.namespace, &s.name, &s.full_name)).unwrap_or_default()
    }
}

impl Checker<'_, '_> {
    fn err(&mut self, msg: String, span: Span, note: Option<String>) {
        if !self.seen.insert((self.file, span.start, span.end)) {
            return;
        }
        let mut d = Diagnostic::error(msg, span).in_file(self.file);
        if let Some(n) = note {
            d = d.with_note(n, None);
        }
        self.diags.push(d);
    }

    /// Type names as sema prints them: short for types of the current
    /// function's namespace, full paths otherwise.
    fn tn(&self, t: TypeId) -> String {
        let ns = self.p.funcs.get(self.scope).map(|f| f.namespace);
        crate::hir::dump::type_name(&self.p.types, t, &InNs { p: self.p, ns })
    }

    /// A type that may not hold a value with a destructor in this position,
    /// if any part of `t` does so.
    fn bad_dtor_container(&self, t: TypeId) -> Option<String> {
        let p = self.p;
        match p.types.get(t) {
            Type::Slice(e) => {
                if has_dtor(p, *e) {
                    return Some(format!("a slice of {}", self.tn(*e)));
                }
                self.bad_dtor_container(*e)
            }
            Type::Map(k, v) => {
                if has_dtor(p, *k) || has_dtor(p, *v) {
                    return Some(format!("a map with values of {}", self.tn(*v)));
                }
                self.bad_dtor_container(*k).or_else(|| self.bad_dtor_container(*v))
            }
            Type::Task(r) => {
                if has_dtor(p, *r) {
                    return Some(format!("an async result of type {}", self.tn(*r)));
                }
                self.bad_dtor_container(*r)
            }
            Type::Tuple(ts) => {
                if ts.iter().any(|t| has_dtor(p, *t)) {
                    return Some("a tuple element".to_string());
                }
                ts.iter().find_map(|t| self.bad_dtor_container(*t))
            }
            Type::Array(e, _) | Type::Fallible(e) | Type::Pointer(e) | Type::Weak(e) => self.bad_dtor_container(*e),
            Type::Func(sig) => sig.params.iter().chain(std::iter::once(&sig.ret)).find_map(|t| self.bad_dtor_container(*t)),
            _ => None,
        }
    }

    fn check_type(&mut self, t: TypeId, span: Span) {
        if let Some(what) = self.bad_dtor_container(t) {
            // One report per function (or declaration) is enough.
            if !self.seen_msgs.insert((self.file, self.scope, what.clone())) {
                return;
            }
            self.err(
                format!("a value with a destructor (Drop) cannot be stored as {what}"),
                span,
                Some("values with a destructor are move-only; store pointers instead (e.g. []*T)".into()),
            );
        }
    }

    fn decls(&mut self) {
        let p = self.p;
        for (sid, s) in p.structs.iter().enumerate() {
            self.file = s.file;
            for f in &s.fields {
                self.check_type(f.ty, f.span);
            }
            if let Some((_, fid)) = s.methods.iter().find(|(n, _)| n == "Drop") {
                let f = &p.funcs[fid.0 as usize];
                let ok_kind = matches!(f.kind, FuncKind::Method { is_static: false, .. });
                if !ok_kind || f.params.len() != 1 || f.ret != p.types.void || f.is_async {
                    self.file = f.file;
                    self.err(
                        format!("destructor {}.Drop must be declared as `func Drop()` (not static, no parameters, no result, not async)", s.name),
                        f.span,
                        None,
                    );
                } else if self.facts.escapes(*fid, 0) {
                    self.file = f.file;
                    self.err(
                        format!("destructor {}.Drop must not let `this` escape (store, return or capture it)", s.name),
                        f.span,
                        Some("Drop runs while the object is being freed; `this` may point into another object".into()),
                    );
                }
                let _ = sid;
            }
        }
        for e in &p.enums {
            self.file = e.file;
            for v in &e.variants {
                for (_, t) in &v.fields {
                    if has_dtor(p, *t) {
                        self.err(format!("enum payload of type {} has a destructor (Drop); use a pointer", self.tn(*t)), v.span, None);
                    }
                    self.check_type(*t, v.span);
                }
            }
        }
    }

    fn func(&mut self, fid: FuncId, f: &Function) {
        let p = self.p;
        self.file = f.file;
        self.scope = fid.0 as usize;
        for l in &f.locals {
            self.check_type(l.ty, l.span);
        }
        if f.is_async && has_dtor(p, f.ret) {
            self.err(format!("async function {} cannot return a value with a destructor ({})", f.name, self.tn(f.ret)), f.span, None);
        }
        let _ = fid;
        let mut found: Vec<(String, Span, Option<String>)> = Vec::new();
        let facts = self.facts;
        let names = InNs { p, ns: Some(f.namespace) };
        let mut moved: HashSet<*const Expr> = HashSet::new();
        moved_returns(p, f, &f.body.stmts, &mut moved);
        // Interior pointer variables (`var p = &xs[i]`): the values stored
        // into them are pinned (their owner is kept alive by the variable).
        let iv = interior_vars(p, f);
        let is_iv = |l: LocalId| iv.is.get(l.0 as usize).copied().unwrap_or(false);
        let mut kept: HashSet<*const Expr> = HashSet::new();
        for (l, v) in &iv.sites {
            let name = &f.locals[l.0 as usize].name;
            let pin = match &v.kind {
                ExprKind::AddrOf(inner) if is_interior_addr(v) => interior_pin(p, fid, f, facts, inner, &is_iv),
                _ => pointer_pin(fid, f, facts, v, &is_iv),
            };
            let ok = match pin {
                // (The cell of `&s.f` is pinned, so `s` may be declared
                // anywhere; the pins of another interior variable are
                // copied, so it must be declared first.)
                Some(Pin::Interior(q)) => q.0 < l.0,
                Some(_) => true,
                None => false,
            };
            if ok {
                kept.insert(*v as *const Expr);
            } else {
                found.push((
                    format!("cannot keep this interior pointer in `{name}`: its owner is a temporary or is computed by a call (or it comes from an interior pointer variable declared after `{name}`)"),
                    v.span,
                    Some("take the address through a variable that outlives the pointer (`var xs = f(); var p = &xs[i]`)".into()),
                ));
            }
        }
        for (i, loc) in f.locals.iter().enumerate() {
            if iv.is[i] && (loc.captured || loc.address_taken) {
                found.push((
                    format!("interior pointer variable `{}` cannot be captured by a closure or have its address taken", loc.name),
                    loc.span,
                    Some("it holds a pointer into another object; copy the value instead, or use `new`".into()),
                ));
            }
        }
        walk_function(p, f, &mut |e, pos| {
            // Uses of interior pointer variables: read through, compared,
            // reassigned or passed to a non-escaping parameter only.
            if let ExprKind::Local(l) = &e.kind {
                if is_iv(*l) {
                    let ok = match pos {
                        Pos::Read | Pos::Target => true,
                        Pos::Arg(g, i) => !facts.escapes(g, i),
                        Pos::Store => kept.contains(&(e as *const Expr)),
                        Pos::AddrOf => false,
                    };
                    if !ok {
                        let name = &f.locals[l.0 as usize].name;
                        found.push((
                            format!("cannot keep interior pointer variable `{name}` elsewhere: it points into another object, so it can only be dereferenced, compared, reassigned or passed to a parameter that does not escape"),
                            e.span,
                            Some("store it only in local interior pointer variables; use `new` (or a pointer field) to share the value".into()),
                        ));
                    }
                }
            }
            // Interior pointers and slices without an owner.
            let interior = (is_interior_addr(e) && !kept.contains(&(e as *const Expr))) || unowned_array_slice(p, e);
            if interior {
                let what = if is_interior_addr(e) { "an interior pointer (into a field, an element or a global)" } else { "a slice of an array that is not a variable" };
                match pos {
                    Pos::Read | Pos::AddrOf => {}
                    Pos::Arg(g, i) if !facts.escapes(g, i) => {}
                    Pos::Arg(g, i) => {
                        let gf = &p.funcs[g.0 as usize];
                        let pname = gf.params.get(i).map(|l| gf.locals[l.0 as usize].name.clone()).unwrap_or_default();
                        found.push((
                            format!("cannot pass {what} to parameter `{pname}` of {}: the parameter escapes (it is stored, returned, reassigned or captured, or the function is async)", gf.full_name),
                            e.span,
                            Some("only a parameter that the callee merely reads through can take an interior pointer; use `new` (or a pointer field) to share the value".into()),
                        ));
                    }
                    _ => found.push((
                        format!("cannot keep {what}: it has no object of its own, so it can only be dereferenced, compared or passed to a parameter that does not escape"),
                        e.span,
                        Some("use `new` (or a pointer field) to share the value, or take the address of a whole variable".into()),
                    )),
                }
            }
            // Explicit destructor calls.
            if let ExprKind::Call { callee: Callee::Func(g), .. } = &e.kind {
                if let FuncKind::Method { struct_id, is_static: false } = p.funcs[g.0 as usize].kind {
                    if drop_method(p, struct_id) == Some(*g) {
                        found.push(("Drop cannot be called directly: it runs automatically when the last reference goes away".into(), e.span, None));
                    }
                }
            }
            // Copies of move-only values.
            if matches!(pos, Pos::Store | Pos::Arg(..)) && is_place_like(&e.kind) && has_dtor(p, e.ty) && !moved.contains(&(e as *const Expr)) {
                let t = crate::hir::dump::type_name(&p.types, e.ty, &names);
                found.push((
                    format!("cannot copy a value of type {t}: it has a destructor (Drop), so it is move-only"),
                    e.span,
                    Some("create it in place (var x = T{...}, a function result) and share it through a pointer (&x or new T{...})".into()),
                ));
            }
            // Type patterns and for-in bindings are copies too (checked
            // with the statements below).
            // Promoted methods reached through an interface.
            if let ExprKind::ToInterface { value } = &e.kind {
                if let Some(msg) = promoted_escape(p, facts, value.ty, e.ty) {
                    found.push((msg, e.span, Some("the method receives a pointer into the middle of the outer struct".into())));
                }
            }
            // Expressions of container types holding destructors.
            let _ = e.ty;
        });
        for (m, s, n) in found {
            self.err(m, s, n);
        }
        self.stmts(&f.body, f);
        let exprs = collect_types(p, f);
        for (t, s) in exprs {
            self.check_type(t, s);
        }
    }

    /// Bindings that copy: type-switch cases and `for x in array`.
    fn stmts(&mut self, b: &Block, f: &Function) {
        for s in &b.stmts {
            self.stmt(s, f);
        }
    }

    fn stmt(&mut self, s: &Stmt, f: &Function) {
        let p = self.p;
        match &s.kind {
            StmtKind::If { then, else_, .. } => {
                self.stmts(then, f);
                if let Some(e) = else_ {
                    self.stmts(e, f);
                }
            }
            StmtKind::Loop { body, .. } | StmtKind::ForRange { body, .. } | StmtKind::ForString { body, .. } | StmtKind::ForMap { body, .. } => self.stmts(body, f),
            StmtKind::ForSlice { body, .. } => self.stmts(body, f),
            StmtKind::Switch(sw) => {
                for c in &sw.cases {
                    for pat in &c.patterns {
                        if let Pattern::Type { ty, binding: Some(_) } = pat {
                            if has_dtor(p, *ty) {
                                self.err(format!("cannot bind a copy of {}: it has a destructor (Drop); match a pointer type instead", self.tn(*ty)), c.span, None);
                            }
                        }
                    }
                    self.stmts(&c.body, f);
                }
                if let Some(d) = &sw.default {
                    self.stmts(d, f);
                }
            }
            StmtKind::Block(b) => self.stmts(b, f),
            StmtKind::TryBlock { body, handler, .. } => {
                self.stmts(body, f);
                self.stmts(handler, f);
            }
            _ => {}
        }
    }
}

/// Places whose value would be copied.
fn is_place_like(k: &ExprKind) -> bool {
    matches!(k, ExprKind::Local(_) | ExprKind::Upvalue(_) | ExprKind::Global(_) | ExprKind::Field { .. } | ExprKind::Index { .. } | ExprKind::Deref(_) | ExprKind::MapGet { .. })
}

/// `a[lo..hi]` of an array that is not rooted at a variable or a global
/// (e.g. an array field reached through a pointer): the slice has no
/// backing object.
fn unowned_array_slice(p: &Program, e: &Expr) -> bool {
    let ExprKind::SubSlice { base, .. } = &e.kind else { return false };
    if !matches!(p.types.get(base.ty), Type::Array(..)) {
        return false;
    }
    let mut b: &Expr = base;
    loop {
        match &b.kind {
            ExprKind::Local(_) | ExprKind::Upvalue(_) | ExprKind::Global(_) => return false,
            ExprKind::Field { base, .. } => b = base,
            ExprKind::Index { base, .. } if matches!(p.types.get(base.ty), Type::Array(..)) => b = base,
            _ => return true,
        }
    }
}

/// Finds method `name` of struct `sid` with the embedded-field path.
fn resolve_method(p: &Program, sid: u32, name: &str) -> Option<(FuncId, usize)> {
    let s = &p.structs[sid as usize];
    if let Some((_, f)) = s.methods.iter().find(|(n, _)| n == name) {
        return Some((*f, 0));
    }
    for fd in &s.fields {
        if !fd.embedded {
            continue;
        }
        if let Type::Struct(d) = p.types.get(fd.ty) {
            if let Some((f, depth)) = resolve_method(p, d.0, name) {
                return Some((f, depth + 1));
            }
        }
    }
    None
}

fn promoted_escape(p: &Program, facts: &ParamFacts, from: TypeId, to: TypeId) -> Option<String> {
    let sid = match p.types.get(from) {
        Type::Struct(d) => d.0,
        Type::Pointer(t) => match p.types.get(*t) {
            Type::Struct(d) => d.0,
            _ => return None,
        },
        _ => return None,
    };
    let names: Vec<String> = match p.types.get(to) {
        Type::Interface(d) => p.interfaces[d.0 as usize].methods.iter().map(|m| m.name.clone()).collect(),
        Type::Error => vec!["Message".into()],
        _ => return None,
    };
    for n in names {
        if let Some((f, depth)) = resolve_method(p, sid, &n) {
            if depth > 0 && facts.escapes(f, 0) {
                return Some(format!(
                    "cannot convert {} to {}: promoted method {} lets `this` escape, but through an interface it receives a pointer into the middle of {}",
                    p.type_name(from),
                    p.type_name(to),
                    p.funcs[f.0 as usize].full_name,
                    p.structs[sid as usize].name
                ));
            }
        }
    }
    None
}

/// Types of the expressions of `f` that build containers (literals,
/// `append`), with their spans.
fn collect_types(p: &Program, f: &Function) -> Vec<(TypeId, Span)> {
    let mut out = Vec::new();
    walk_function(p, f, &mut |e, _| {
        if matches!(e.kind, ExprKind::SliceLit(_) | ExprKind::MapLit(_) | ExprKind::Tuple(_) | ExprKind::Builtin { op: Builtin::Append | Builtin::AppendSlice, .. }) {
            out.push((e.ty, e.span));
        }
    });
    out
}

impl super::Sema<'_> {
    /// [`has_dtor`] during analysis (on the analyser's declarations).
    pub(crate) fn type_has_dtor(&self, t: TypeId) -> bool {
        match self.tt.get(t) {
            Type::Struct(d) => {
                let s = &self.structs[d.0 as usize];
                s.methods.iter().any(|(n, f)| n == "Drop" && matches!(self.funcs[f.0 as usize].kind, FuncKind::Method { is_static: false, .. }))
                    || s.fields.iter().any(|f| self.type_has_dtor(f.ty))
            }
            Type::Array(e, n) => *n > 0 && self.type_has_dtor(*e),
            Type::Tuple(ts) => ts.iter().any(|t| self.type_has_dtor(*t)),
            Type::Fallible(t) => self.type_has_dtor(*t),
            Type::Enum(d) => self.enums[d.0 as usize].variants.iter().any(|v| v.fields.iter().any(|f| self.type_has_dtor(f.1))),
            _ => false,
        }
    }
}
