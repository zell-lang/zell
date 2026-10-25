//! Compile-time conditions of static reflection (DESIGN.md "Статическая
//! рефлексия"): what folds to a constant inside an unrolled
//! `reflect.Fields<T>()` loop or a generic instance, and the statements
//! that pick a branch by such a constant.
//!
//! * Type tests: `reflect.TypeOf<T>()` and `f.Type` of a compile-time field,
//!   optionally followed by `.Elem` / `.Key`, compared with `==` / `!=`
//!   to each other or (through `.Kind`) to a `reflect.Kind` variant, are
//!   constants ([`Sema::ct_type_test`]); `f.Type.Name`, `.FullName` and
//!   `.Kind` are constants too ([`Sema::ct_type_path`]).
//! * Every fold made by compile-time reflection (`f.Name`,
//!   `f.Attribute<A>()`, `tag.field`, the type tests, a local bound to one
//!   of these and never assigned) bumps `ReflState::folds`. An `if` whose
//!   condition folds to a constant through at least one of them is static
//!   ([`Sema::folded_if`]): only the branch taken is checked. In the
//!   abstract check of a generic body (the field unknown) such a condition
//!   bumps `ReflState::abstract_ct` instead, and both branches are checked
//!   with diagnostics muted.
//! * A `switch` over a value of known static type with only type patterns
//!   is a static type switch in a generic body: it keeps the first case
//!   whose type matches (or the `default`); a `switch` whose subject is a
//!   compile-time reflection constant keeps the case with the equal
//!   constant ([`Sema::static_switch`]).
//! * `f.Set(p, v)`: a direct store `p.<field> = v` with the field's static
//!   type ([`Sema::ct_field_set`]).

use super::coerce::Ctx;
use super::reflect::Ct;
use super::*;
use crate::hir::{ConstValue, ExprKind, Stmt, StmtKind};

impl<'a> Sema<'a> {
    /// The compile-time field variable named `name` (bound by an unrolled
    /// `reflect.Fields<T>()` loop of the current function), marked used:
    /// (local, struct type, field index or `None` in an abstract check).
    pub(crate) fn ct_field_named(&mut self, name: &str) -> Option<(LocalId, TypeId, Option<u32>)> {
        if self.refl.ct_fields.is_empty() {
            return None;
        }
        let Some(Ok(VarRef::Local(l))) = self.lookup_local(name, false) else { return None };
        let depth = self.fns.len();
        let (_, _, sty, k) = self.refl.ct_fields.iter().rev().find(|c| c.0 == depth && c.1 == l).copied()?;
        self.fcx().used[l.0 as usize] = true;
        Some((l, sty, k))
    }

    /// [`ct_field_named`](Self::ct_field_named) for an identifier expression.
    fn ct_field_var(&mut self, e: &ast::Expr) -> Option<(LocalId, TypeId, Option<u32>)> {
        match &expr::strip_parens(e).kind {
            ast::ExprKind::Ident(id) => self.ct_field_named(&id.name),
            _ => None,
        }
    }

    /// Type of field `k` of struct type `sty`.
    pub(crate) fn ct_field_ty(&self, sty: TypeId, k: u32) -> Option<TypeId> {
        self.ct_member(sty, k).map(|m| m.1)
    }

    /// Field `k` of the struct or tuple type `sty` of an unrolled
    /// `reflect.Fields<T>()`: (name, type, exported, embedded); tuple
    /// elements are `_0`, `_1`, ...
    pub(crate) fn ct_member(&self, sty: TypeId, k: u32) -> Option<(String, TypeId, bool, bool)> {
        match self.tt.get(sty) {
            Type::Struct(d) => self.structs[d.0 as usize].fields.get(k as usize).map(|f| (f.name.clone(), f.ty, f.exported, f.embedded)),
            Type::Tuple(xs) => xs.get(k as usize).map(|t| (format!("_{k}"), *t, true, false)),
            _ => None,
        }
    }

    /// The type a compile-time type expression denotes:
    /// `reflect.TypeOf<T>()` or `f.Type` of a compile-time field, then
    /// `.Elem` / `.Key` steps. `Some(None)` when it depends on a type
    /// parameter (abstract check); a step the type lacks gives the invalid
    /// type (kind `Invalid`). `None` if `e` is no such expression.
    pub(crate) fn ct_type_operand(&mut self, e: &ast::Expr) -> Option<Option<TypeId>> {
        let mut steps = Vec::new();
        let mut cur = expr::strip_parens(e);
        while let ast::ExprKind::Field { base, name } = &cur.kind {
            if name.name != "Elem" && name.name != "Key" {
                break;
            }
            steps.push(name.name.clone());
            cur = expr::strip_parens(base);
        }
        if let Some(d) = self.ct_desc_operand(cur) {
            return Some(match d {
                Some(t) => self.type_steps(t, steps.iter().rev().map(|s| s.as_str())),
                None => None,
            });
        }
        let t = match &cur.kind {
            ast::ExprKind::Field { base, name } if name.name == "Type" => {
                let (_, sty, k) = self.ct_field_var(base)?;
                match k {
                    Some(k) => self.ct_field_ty(sty, k)?,
                    None => return Some(None),
                }
            }
            ast::ExprKind::Call { .. } => self.reflect_targ(cur, "TypeOf")?,
            _ => return None,
        };
        Some(self.type_steps(t, steps.iter().rev().map(|s| s.as_str())))
    }

    /// [`type_steps`](Self::type_steps) for other modules.
    pub(crate) fn type_steps_pub<'s>(&self, t: TypeId, steps: impl Iterator<Item = &'s str>) -> Option<TypeId> {
        self.type_steps(t, steps)
    }

    /// Applies `.Elem` / `.Key` steps to `t`; `None` if `t` depends on a
    /// type parameter.
    fn type_steps<'s>(&self, mut t: TypeId, steps: impl Iterator<Item = &'s str>) -> Option<TypeId> {
        if self.has_param(t) {
            return None;
        }
        for s in steps {
            match self.type_step(t, s) {
                Some(x) => t = x,
                None => return Some(self.tt.invalid),
            }
        }
        Some(t)
    }

    /// `l == r` / `l != r` (`neg`) as a compile-time type test: two type
    /// operands, or a `.Kind` of one and a `reflect.Kind` variant.
    pub(crate) fn ct_eq(&mut self, neg: bool, left: &ast::Expr, right: &ast::Expr) -> Option<Ct> {
        let fin = |b: bool| Ct::Known(b != neg);
        if let Some(a) = self.ct_type_operand(left) {
            if let Some(b) = self.ct_type_operand(right) {
                return Some(match (a, b) {
                    (Some(a), Some(b)) => fin(a == b),
                    _ => Ct::Abstract,
                });
            }
        }
        let (k, c) = match self.ct_kind(left) {
            Some(k) => (k, self.ct_kind_const(right)?),
            None => (self.ct_kind(right)?, self.ct_kind_const(left)?),
        };
        Some(match k {
            Some(k) => fin(k == c),
            None => Ct::Abstract,
        })
    }

    /// A compile-time type test used as a value (`binary`): a constant
    /// `bool`; `None` for anything else, also when it depends on a type
    /// parameter (then it is evaluated at run time in the abstract check).
    pub(crate) fn ct_type_test(&mut self, neg: bool, l: &ast::Expr, r: &ast::Expr, span: Span) -> Option<hir::Expr> {
        if !self.refl.loaded_reflect() {
            return None;
        }
        match self.ct_eq(neg, l, r)? {
            Ct::Known(b) => {
                self.refl.folds += 1;
                Some(mk(ExprKind::Const(ConstValue::Bool(b)), self.tt.bool_, span))
            }
            Ct::Abstract => {
                self.refl.abstract_ct += 1;
                None
            }
        }
    }

    /// `f.Type[.Elem|.Key]*.Name|FullName|Kind` of a compile-time field: a
    /// constant (`Kind` as the `reflect.Kind` variant).
    pub(crate) fn ct_type_path(&mut self, segs: &[&ast::Ident], span: Span) -> Option<hir::Expr> {
        if segs.len() >= 2 && !self.refl.ct_descs.is_empty() {
            if let Some(e) = self.ct_desc_type_path(segs, span) {
                return Some(e);
            }
        }
        if segs.len() < 3 || segs[1].name != "Type" || !self.refl.loaded_reflect() {
            return None;
        }
        let last = segs[segs.len() - 1].name.as_str();
        let steps = &segs[2..segs.len() - 1];
        if !matches!(last, "Name" | "FullName" | "Kind") || steps.iter().any(|s| s.name != "Elem" && s.name != "Key") {
            return None;
        }
        let (_, sty, k) = self.ct_field_named(&segs[0].name)?;
        let Some(k) = k else {
            self.refl.abstract_ct += 1;
            return None;
        };
        let t = self.ct_field_ty(sty, k)?;
        let t = self.type_steps(t, steps.iter().map(|s| s.name.as_str()))?;
        self.refl.folds += 1;
        Some(match last {
            "Name" => mk(ExprKind::Const(ConstValue::Str(self.tn(t))), self.tt.string, span),
            "FullName" => mk(ExprKind::Const(ConstValue::Str(self.tn_full(t))), self.tt.string, span),
            _ => {
                let en = *self.refl.enums.get("Kind")?;
                let kind = self.kind_name(t);
                let v = self.enums[en as usize].variants.iter().position(|v| v.name == kind)? as u32;
                let ty = self.enum_ty(en);
                mk(ExprKind::EnumLit { enum_id: en, variant: v, args: Vec::new() }, ty, span)
            }
        })
    }

    /// An `if` that is not a type test: static when its condition folds
    /// to a constant through compile-time reflection, see the module docs.
    pub(crate) fn folded_if(&mut self, i: &ast::IfStmt) -> StmtKind {
        let (f0, a0) = (self.refl.folds, self.refl.abstract_ct);
        let cond = self.cond(&i.cond, "if");
        if let ExprKind::Const(ConstValue::Bool(b)) = cond.kind {
            if self.refl.folds > f0 {
                return self.static_branches(i, Ct::Known(b));
            }
        }
        if self.refl.abstract_ct > a0 {
            return self.static_branches(i, Ct::Abstract);
        }
        let then = self.block(&i.then);
        let else_ = self.else_branch(i);
        StmtKind::If { cond, then, else_ }
    }

    /// The `else` part of an `if`, as a block.
    pub(crate) fn else_branch(&mut self, i: &ast::IfStmt) -> Option<hir::Block> {
        i.else_.as_ref().map(|e| match &**e {
            ast::ElseBranch::Block(b) => self.block(b),
            ast::ElseBranch::If(ei) => {
                let k = self.if_stmt(ei);
                hir::Block { stmts: vec![Stmt { kind: k, span: ei.span }], span: ei.span }
            }
        })
    }

    /// The statement of a static `if` with condition value `c`: the block
    /// of the branch taken (an empty one if there is none); in an abstract
    /// check both branches, checked with diagnostics muted.
    pub(crate) fn static_branches(&mut self, i: &ast::IfStmt, c: Ct) -> StmtKind {
        match c {
            Ct::Known(true) => StmtKind::Block(self.block(&i.then)),
            Ct::Known(false) => StmtKind::Block(self.else_branch(i).unwrap_or_default()),
            Ct::Abstract => {
                self.gen.mute += 1;
                let then = self.block(&i.then);
                let else_ = self.else_branch(i);
                self.gen.mute -= 1;
                self.refl.abstract_stmt = Some(i.span);
                let cond = mk(ExprKind::Const(ConstValue::Bool(false)), self.tt.bool_, i.cond.span);
                StmtKind::If { cond, then, else_ }
            }
        }
    }

    // -- static switch ----------------------------------------------------------

    /// A static `switch` (module docs) with its subject already checked
    /// into temporary `l` = `v`; `None` for an ordinary switch. The result
    /// is a block, so that reachability after it is decided as for a
    /// static `if`.
    pub(crate) fn static_switch(&mut self, sw: &ast::SwitchStmt, l: LocalId, v: &hir::Expr, folded: bool) -> Option<StmtKind> {
        let pats = || sw.cases.iter().flat_map(|c| c.patterns.iter());
        let all_types = pats().all(|p| matches!(p.kind, ast::PatternKind::TypeBind { .. }));
        let t = v.ty;
        if all_types && !sw.cases.is_empty() && !self.tt.is_interface(t) && self.in_generic_body() {
            if self.tt.is_invalid(t) || self.has_param(t) {
                // Abstract check: every case, muted.
                return Some(self.muted_switch(sw, l, v));
            }
            return Some(self.static_type_switch(sw, l, v));
        }
        let key = const_key(v)?;
        if !folded || !pats().all(|p| matches!(p.kind, ast::PatternKind::Expr(_))) {
            return None;
        }
        let label = self.push_label(LabelKind::Switch);
        let mut chosen = None;
        'cases: for c in &sw.cases {
            for p in &c.patterns {
                let ast::PatternKind::Expr(e) = &p.kind else { continue };
                let pv = self.value(e, Some(t));
                match const_key(&pv) {
                    Some(k) if k == key => {
                        chosen = Some(c);
                        break 'cases;
                    }
                    Some(_) => {}
                    None => {
                        if !self.tt.is_invalid(pv.ty) {
                            self.err("a case of a static switch (its subject is a compile-time reflection constant) must be a constant", e.span);
                        }
                    }
                }
            }
        }
        let body = match chosen {
            Some(c) => Some(self.case_body(&c.body, c.span)),
            None => sw.default.as_ref().map(|d| self.case_body(d, sw.span)),
        };
        self.pop_label();
        Some(self.switch_block(label, l, v, body, sw.span))
    }

    /// The static type switch of a concrete subject type.
    fn static_type_switch(&mut self, sw: &ast::SwitchStmt, l: LocalId, v: &hir::Expr) -> StmtKind {
        let t = v.ty;
        let label = self.push_label(LabelKind::Switch);
        let mut body = None;
        'cases: for c in &sw.cases {
            for p in &c.patterns {
                let ast::PatternKind::TypeBind { ty, name } = &p.kind else { continue };
                let pt = self.resolve_type(ty);
                if self.tt.is_invalid(pt) {
                    continue;
                }
                let matches = pt == t || (self.tt.is_interface(pt) && self.implements(t, pt));
                if !matches {
                    continue;
                }
                if c.patterns.len() > 1 && name.name != "_" {
                    self.err("cannot bind a variable in a case with several patterns", name.span);
                }
                self.push_scope();
                let mut stmts = Vec::new();
                if name.name != "_" {
                    let b = self.declare_local(&name.name, pt, LocalKind::PatternBinding, name.span);
                    let subj = mk(ExprKind::Local(l), t, v.span);
                    let init = self.coerce(subj, pt, Ctx::Init);
                    stmts.push(Stmt { kind: StmtKind::Let { local: b, init: Some(init) }, span: name.span });
                }
                let b = self.case_body(&c.body, c.span);
                stmts.push(Stmt { kind: StmtKind::Block(b), span: c.span });
                self.pop_scope();
                body = Some(hir::Block { stmts, span: c.span });
                break 'cases;
            }
        }
        if body.is_none() {
            body = sw.default.as_ref().map(|d| self.case_body(d, sw.span));
        }
        self.pop_label();
        self.switch_block(label, l, v, body, sw.span)
    }

    /// A type switch of the abstract check: each case checked with its
    /// binding, diagnostics muted; nothing is emitted.
    fn muted_switch(&mut self, sw: &ast::SwitchStmt, l: LocalId, v: &hir::Expr) -> StmtKind {
        self.gen.mute += 1;
        let label = self.push_label(LabelKind::Switch);
        let mut all_end = true;
        for c in &sw.cases {
            self.push_scope();
            for p in &c.patterns {
                if let ast::PatternKind::TypeBind { ty, name } = &p.kind {
                    let pt = self.resolve_type(ty);
                    if name.name != "_" {
                        let b = self.declare_local(&name.name, pt, LocalKind::PatternBinding, name.span);
                        self.fcx().used[b.0 as usize] = true;
                    }
                }
            }
            let b = self.case_body(&c.body, c.span);
            all_end &= super::stmt::terminates(&b.stmts, true);
            self.pop_scope();
        }
        // Every case and the default terminate: so does the switch, whichever
        // case an instantiation keeps (the abstract check is not emitted).
        let mut keep = None;
        if let Some(d) = &sw.default {
            let b = self.case_body(d, sw.span);
            if all_end && super::stmt::terminates(&b.stmts, true) {
                keep = Some(b);
            }
        }
        self.pop_label();
        self.gen.mute -= 1;
        self.refl.abstract_stmt = Some(sw.span);
        self.switch_block(label, l, v, keep, sw.span)
    }

    /// `{ switch subject { default: body } }`: the subject is evaluated
    /// once, `break` keeps its target.
    fn switch_block(&mut self, label: hir::Label, l: LocalId, v: &hir::Expr, body: Option<hir::Block>, span: Span) -> StmtKind {
        let sw = hir::Switch { label, subject: Some((l, v.clone())), cases: Vec::new(), default: body, span };
        StmtKind::Block(hir::Block { stmts: vec![Stmt { kind: StmtKind::Switch(sw), span }], span })
    }

    // -- f.Set ------------------------------------------------------------------

    /// `f.Set(p, value)` as a statement, `f` a compile-time field; `None`
    /// if `e` is no such call.
    pub(crate) fn ct_field_set(&mut self, e: &ast::Expr) -> Option<StmtKind> {
        let ast::ExprKind::Call { callee, args } = &expr::strip_parens(e).kind else { return None };
        let ast::ExprKind::Field { base, name } = &expr::strip_parens(callee).kind else { return None };
        if name.name != "Set" || !self.refl.loaded_reflect() {
            return None;
        }
        let (_, sty, k) = self.ct_field_var(base)?;
        let empty = Some(StmtKind::Block(hir::Block::default()));
        if args.len() != 2 {
            self.err(format!("f.Set takes two arguments (a pointer to the struct and the value), got {}", args.len()), e.span);
            self.check_args_loose(args);
            return empty;
        }
        let target = self.value(&args[0], None);
        let Some(k) = k else {
            self.value(&args[1], None);
            return empty;
        };
        if self.tt.is_invalid(target.ty) {
            self.value(&args[1], None);
            return empty;
        }
        if !matches!(*self.tt.get(target.ty), Type::Pointer(p) if p == sty) {
            let (want, found) = (self.tn(sty), self.tn(target.ty));
            self.err(format!("f.Set needs a *{want} (a pointer to the value of reflect.Fields<{want}>()), found {found}"), target.span);
            self.value(&args[1], None);
            return empty;
        }
        let Some((fname, fty, _, _)) = self.ct_member(sty, k) else { return empty };
        let value = self.expr_to(&args[1], fty, Ctx::Field(fname));
        let span = target.span;
        let place = match target.kind {
            ExprKind::AddrOf(inner) => *inner,
            _ => mk(ExprKind::Deref(Box::new(target)), sty, span),
        };
        let target = mk(ExprKind::Field { base: Box::new(place), index: k }, fty, span);
        Some(StmtKind::Assign { target, value })
    }

    // -- locals bound to compile-time constants ----------------------------------

    /// After `var x = init`: if `init` folded through compile-time
    /// reflection to a constant (`f.Attribute<A>()`, `f.Name`, ...) and `x`
    /// is never assigned or address-taken in the rest of its block,
    /// later reads of `x` are that constant (so `x != nil` and `x.field`
    /// fold). Returns the key to forget at the end of the block.
    pub(crate) fn ct_bind_local(&mut self, v: &ast::VarDecl, rest: &[ast::Stmt], out: &[Stmt], folds_before: u32) -> Option<(usize, LocalId)> {
        if self.refl.folds == folds_before || v.names.len() != 1 {
            return None;
        }
        let Some(Stmt { kind: StmtKind::Let { local, init: Some(init) }, .. }) = out.last() else { return None };
        if !matches!(init.kind, ExprKind::Const(_) | ExprKind::AttrConst { .. } | ExprKind::Nil) {
            return None;
        }
        let name = &v.names[0].name;
        if rest.iter().any(|s| stmt_mutates(s, name)) {
            return None;
        }
        let key = (self.fns.len(), *local);
        self.refl.ct_consts.insert(key, init.clone());
        Some(key)
    }

    /// The constant a local bound by [`ct_bind_local`](Self::ct_bind_local)
    /// reads as.
    pub(crate) fn ct_local_const(&mut self, l: LocalId, span: Span) -> Option<hir::Expr> {
        if self.refl.ct_consts.is_empty() {
            return None;
        }
        let mut e = self.refl.ct_consts.get(&(self.fns.len(), l))?.clone();
        e.span = span;
        self.refl.folds += 1;
        Some(e)
    }
}

/// The constant of a folded expression (a literal constant or a plain enum
/// variant), for comparing static switch cases.
fn const_key(e: &hir::Expr) -> Option<ConstValue> {
    match &e.kind {
        ExprKind::Const(v) => Some(v.clone()),
        ExprKind::EnumLit { enum_id, variant, args } if args.is_empty() => Some(ConstValue::Enum(*enum_id, *variant)),
        _ => None,
    }
}

/// True if statement `s` was a static `if` / type switch checked
/// abstractly (`abstract_stmt`) and one of its branches terminates: the
/// statements after it are dead for some instantiations, so the abstract
/// check reads them with diagnostics muted.
pub(crate) fn maybe_dead_after(s: &ast::Stmt, out: &[Stmt], n: usize, abstract_stmt: Option<Span>) -> bool {
    let span = match &s.kind {
        ast::StmtKind::If(i) => i.span,
        ast::StmtKind::Switch(sw) => sw.span,
        _ => return false,
    };
    abstract_stmt == Some(span) && out[n..].iter().any(may_terminate)
}

/// True if some path through `s` ends in a terminating statement.
fn may_terminate(s: &Stmt) -> bool {
    let blk = |b: &hir::Block| b.stmts.last().is_some_and(may_terminate);
    match &s.kind {
        StmtKind::If { then, else_, .. } => blk(then) || else_.as_ref().is_some_and(blk),
        StmtKind::Block(b) => blk(b),
        StmtKind::Switch(sw) => sw.cases.iter().any(|c| blk(&c.body)) || sw.default.as_ref().is_some_and(blk),
        _ => super::stmt::terminates(std::slice::from_ref(s), true),
    }
}

/// True if statement `s` (checked into `out[n..]`) was a static `if` /
/// `switch` whose branch terminates: what follows it is dead.
pub(crate) fn dead_after(s: &ast::Stmt, out: &[Stmt], n: usize) -> bool {
    matches!(s.kind, ast::StmtKind::If(_) | ast::StmtKind::Switch(_))
        && out.len() == n + 1
        && matches!(&out[n].kind, StmtKind::Block(b) if super::stmt::terminates(&b.stmts, true))
}

// -- assignment scan ----------------------------------------------------------------

/// True if `s` may change local `name`: an assignment to it (also inside a
/// lambda) or `&name`. Conservative: shadowing declarations are ignored.
fn stmt_mutates(s: &ast::Stmt, name: &str) -> bool {
    let e = |x: &ast::Expr| expr_mutates(x, name);
    let b = |x: &ast::Block| x.stmts.iter().any(|s| stmt_mutates(s, name));
    match &s.kind {
        ast::StmtKind::Var(v) => v.init.as_ref().is_some_and(e),
        ast::StmtKind::Const(c) => e(&c.value),
        ast::StmtKind::If(i) => if_mutates(i, name),
        ast::StmtKind::For(f) => {
            let head = match &f.kind {
                ast::ForKind::Infinite => false,
                ast::ForKind::While(c) => e(c),
                ast::ForKind::In { iter, .. } => e(iter),
            };
            head || b(&f.body)
        }
        ast::StmtKind::Switch(sw) => {
            sw.subject.as_ref().is_some_and(e)
                || sw.cases.iter().any(|c| {
                    c.patterns.iter().any(|p| match &p.kind {
                        ast::PatternKind::Expr(x) => e(x),
                        ast::PatternKind::Range(lo, hi) => lo.as_ref().is_some_and(e) || hi.as_ref().is_some_and(e),
                        ast::PatternKind::TypeBind { .. } => false,
                    }) || c.body.iter().any(|s| stmt_mutates(s, name))
                })
                || sw.default.as_ref().is_some_and(|d| d.iter().any(|s| stmt_mutates(s, name)))
        }
        ast::StmtKind::Return(es) => es.iter().any(e),
        ast::StmtKind::Defer(x) | ast::StmtKind::Expr(x) => e(x),
        ast::StmtKind::Assign { target, value, .. } => target_is(target, name) || e(target) || e(value),
        ast::StmtKind::Block(x) => b(x),
        ast::StmtKind::TryBlock { body, handler, .. } => b(body) || b(handler),
        ast::StmtKind::Labeled { stmt, .. } => stmt_mutates(stmt, name),
        ast::StmtKind::Break(_) | ast::StmtKind::Continue(_) | ast::StmtKind::Error => false,
    }
}

fn if_mutates(i: &ast::IfStmt, name: &str) -> bool {
    expr_mutates(&i.cond, name)
        || i.then.stmts.iter().any(|s| stmt_mutates(s, name))
        || match i.else_.as_deref() {
            Some(ast::ElseBranch::If(x)) => if_mutates(x, name),
            Some(ast::ElseBranch::Block(b)) => b.stmts.iter().any(|s| stmt_mutates(s, name)),
            None => false,
        }
}

/// `name` itself as an assignment target (also inside a tuple target).
fn target_is(t: &ast::Expr, name: &str) -> bool {
    match &expr::strip_parens(t).kind {
        ast::ExprKind::Ident(id) => id.name == name,
        ast::ExprKind::Tuple(xs) => xs.iter().any(|x| target_is(x, name)),
        _ => false,
    }
}

fn expr_mutates(x: &ast::Expr, name: &str) -> bool {
    let e = |y: &ast::Expr| expr_mutates(y, name);
    match &x.kind {
        ast::ExprKind::Unary { op: ast::UnaryOp::AddrOf, expr } => matches!(&expr::strip_parens(expr).kind, ast::ExprKind::Ident(id) if id.name == name) || e(expr),
        ast::ExprKind::Unary { expr, .. } | ast::ExprKind::Await(expr) | ast::ExprKind::New(expr) | ast::ExprKind::Paren(expr) => e(expr),
        ast::ExprKind::Field { base, .. } | ast::ExprKind::TypeArgs { base, .. } => e(base),
        ast::ExprKind::Call { callee, args } => e(callee) || args.iter().any(e),
        ast::ExprKind::Index { base, index } => e(base) || e(index),
        ast::ExprKind::Binary { left, right, .. } => e(left) || e(right),
        ast::ExprKind::Propagate { expr, context } => e(expr) || context.as_deref().is_some_and(e),
        ast::ExprKind::Try { expr, handler } => {
            e(expr)
                || match handler {
                    ast::TryHandler::Catch { body, .. } => body.stmts.iter().any(|s| stmt_mutates(s, name)),
                    ast::TryHandler::Else(v) => match &**v {
                        ast::ElseValue::Expr(y) => e(y),
                        ast::ElseValue::Return(ys, _) => ys.iter().any(e),
                        ast::ElseValue::Continue(_) | ast::ElseValue::Break(_) => false,
                    },
                }
        }
        ast::ExprKind::StructLit { fields, .. } => fields.iter().any(|f| e(&f.value)),
        ast::ExprKind::CompositeLit { elems, .. } => elems.iter().any(|(k, v)| k.as_ref().is_some_and(e) || e(v)),
        ast::ExprKind::SliceLit(xs) | ast::ExprKind::Tuple(xs) => xs.iter().any(e),
        ast::ExprKind::MapLit { entries, .. } => entries.iter().any(|(k, v)| e(k) || e(v)),
        ast::ExprKind::Range(a, b) => a.as_deref().is_some_and(e) || b.as_deref().is_some_and(e),
        ast::ExprKind::Lambda { body, .. } => match &**body {
            ast::LambdaBody::Block(b) => b.stmts.iter().any(|s| stmt_mutates(s, name)),
            ast::LambdaBody::Expr(y) => e(y),
        },
        ast::ExprKind::NamedArg { value, .. } => e(value),
        _ => false,
    }
}
