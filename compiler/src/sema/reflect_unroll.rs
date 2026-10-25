//! Static unrolling of descriptor loops (DESIGN.md "Статическая
//! рефлексия", "Развёртывание типов, методов и параметров"):
//!
//! * `for t in reflect.TypesWith<A>() { body }` is unrolled when `body`
//!   needs a compile-time descriptor ([`needs_unroll`]): `t` is used as a
//!   type, `m.CallOn`/`m.Apply`/`p.Get`/`p.Set` are called on a descriptor
//!   reached from it, or `body` calls `reflect.Error`. Otherwise the loop
//!   stays an ordinary run-time loop over the slice. The copies follow the
//!   `TypesWith` order; generic types marked with `A` cannot be listed
//!   before the end of the analysis and are an error in this form.
//! * Inside an unrolled copy, `for m in t.Methods` and `for p in m.Params`
//!   are unrolled too (every method of the struct in declaration order,
//!   async ones included; every parameter without the receiver).
//! * A descriptor ([`CtDesc`]) folds its members to constants (`t.Name`,
//!   `m.Name`, `m.Static`, `m.IsAsync`, `len(m.Params)`, `p.Name`,
//!   `p.Index`, `x.Attribute<B>()`), and names types: `t`, `p.Type`,
//!   `m.Result`, `m.Args` (a struct with one field per parameter) and
//!   `f.Type` of a compile-time field may be written where a type is
//!   expected ([`Sema::ct_desc_type`]).
//! * `m.CallOn(recv, args...)` / `m.Apply(recv, args)` are direct calls
//!   of the method; `p.Get(args)` / `p.Set(&args, v)` read and store the
//!   parameter's field of an `m.Args` value.
//!
//! A copy whose descriptor variable is only used in folded positions
//! binds it to nil; otherwise it is bound to the run-time table entry
//! (`TypeOf<t>()`, `.Methods[k]`, `.Params[j]`).

use super::coerce::Ctx;
use super::*;
use crate::hir::{ConstValue, ExprKind, Stmt, StmtKind};
use std::sync::OnceLock;

/// A compile-time descriptor bound to the variable of an unrolled loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum CtDesc {
    /// `t` of `TypesWith<A>()`.
    Type(TypeId),
    /// `m` of `t.Methods`: the struct type and the method.
    Method(TypeId, FuncId),
    /// `p` of `m.Params`: struct type, method, parameter index.
    Param(TypeId, FuncId, u32),
    /// Unknown (the abstract check of a generic body).
    Abstract,
}

/// What a descriptor loop iterates over.
enum Over {
    Types(TypeId),
    Methods(CtDesc),
    Params(CtDesc),
}

/// Descriptor members that fold to constants or are only compile-time.
const TYPE_CONSTS: &[&str] = &["Name", "FullName", "Package", "Kind"];

impl<'a> Sema<'a> {
    /// The descriptor bound to local `name` of the current function, if any
    /// (marks it used).
    pub(crate) fn ct_desc_named(&mut self, name: &str) -> Option<(LocalId, CtDesc)> {
        if self.refl.ct_descs.is_empty() {
            return None;
        }
        let Some(Ok(VarRef::Local(l))) = self.lookup_local(name, false) else { return None };
        let depth = self.fns.len();
        let d = self.refl.ct_descs.iter().rev().find(|c| c.0 == depth && c.1 == l).map(|c| c.2)?;
        self.fcx().used[l.0 as usize] = true;
        Some((l, d))
    }

    fn ct_desc_ident(&mut self, e: &ast::Expr) -> Option<(LocalId, CtDesc)> {
        match &expr::strip_parens(e).kind {
            ast::ExprKind::Ident(id) => self.ct_desc_named(&id.name),
            _ => None,
        }
    }

    /// The descriptor of local `l` (already resolved).
    fn ct_desc_of(&self, l: LocalId) -> Option<CtDesc> {
        let depth = self.fns.len();
        self.refl.ct_descs.iter().rev().find(|c| c.0 == depth && c.1 == l).map(|c| c.2)
    }

    /// Declared parameter names of method `fid` (receiver excluded).
    fn param_names(&self, fid: FuncId) -> Vec<String> {
        match self.func_src.get(fid.0 as usize) {
            Some(FuncSrc::Free(d) | FuncSrc::Method(d, _)) => d.params.iter().map(|p| p.name.name.clone()).collect(),
            _ => Vec::new(),
        }
    }

    fn method_is_static(&self, fid: FuncId) -> bool {
        matches!(self.funcs[fid.0 as usize].kind, hir::FuncKind::Method { is_static: true, .. })
    }

    /// `S.M` of a method, for diagnostics.
    fn method_label(&self, sty: TypeId, fid: FuncId) -> String {
        format!("{}.{}", self.tn(sty), self.funcs[fid.0 as usize].name)
    }

    // -- the loop ---------------------------------------------------------------

    /// `for x in reflect.TypesWith<A>()` / `t.Methods` / `m.Params` unrolled
    /// at compile time; `None` for any other loop (also for a `TypesWith`
    /// loop whose body needs no compile-time descriptor).
    pub(crate) fn desc_for(&mut self, vars: &[ast::Ident], iter: &ast::Expr, body: &ast::Block, span: Span) -> Option<StmtKind> {
        let over = self.desc_over(vars, iter, body)?;
        let empty = || StmtKind::Block(hir::Block::default());
        if vars.len() != 1 {
            self.err("an unrolled descriptor loop (for t in reflect.TypesWith<A>(), t.Methods, m.Params) takes one variable", span);
            return Some(empty());
        }
        let (ptr_name, items) = match over {
            Over::Types(a) => ("TypeInfo", self.desc_types(a, iter.span)),
            Over::Methods(CtDesc::Type(t)) => {
                let items = match *self.tt.get(t) {
                    Type::Struct(d) => self.structs[d.0 as usize].methods.iter().map(|m| CtDesc::Method(t, m.1)).collect(),
                    _ => Vec::new(),
                };
                ("MethodInfo", Some(items))
            }
            Over::Params(CtDesc::Method(t, fid)) => {
                let n = self.sigs[fid.0 as usize].params.len() as u32;
                ("ParamInfo", Some((0..n).map(|j| CtDesc::Param(t, fid, j)).collect()))
            }
            Over::Methods(_) => ("MethodInfo", None),
            Over::Params(_) => ("ParamInfo", None),
        };
        let ptr = {
            let s = self.refl_struct_ty(ptr_name);
            self.tt.pointer(s)
        };
        let var = &vars[0];
        let depth = self.fns.len();
        let items = match items {
            Some(v) if !v.is_empty() => v,
            other => {
                // Abstract or empty: checked once with an unknown
                // descriptor, diagnostics muted; nothing is emitted.
                self.gen.mute += 1;
                self.push_scope();
                let _label = self.push_label(LabelKind::Loop);
                let l = self.declare_local(&var.name, ptr, LocalKind::LoopVar, var.span);
                self.fcx().used[l.0 as usize] = true;
                self.refl.ct_descs.push((depth, l, CtDesc::Abstract));
                if other.is_none() {
                    self.refl.abstract_ct += 1;
                }
                self.block(body);
                self.pop_label();
                self.pop_scope();
                self.refl.ct_descs.pop();
                self.gen.mute -= 1;
                if self.refl.ct_descs.is_empty() {
                    self.refl.copy_used.clear();
                    self.refl.copy_unused.clear();
                }
                return Some(empty());
            }
        };
        let int = self.tt.int;
        let idx = self.temp(int, span);
        let label = self.push_label(LabelKind::Loop);
        let mut copies = Vec::new();
        for d in items.iter().copied() {
            self.push_scope();
            let l = self.declare_local(&var.name, ptr, LocalKind::LoopVar, var.span);
            self.refl.ct_descs.push((depth, l, d));
            self.refl.ct_uses.remove(&(depth, l));
            let b = self.block(body);
            let dynamic_uses = self.refl.ct_uses.remove(&(depth, l)).unwrap_or(0);
            self.fcx().used[l.0 as usize] = true;
            self.pop_scope();
            self.refl.ct_descs.pop();
            let init = if dynamic_uses == 0 {
                mk(ExprKind::Nil, ptr, var.span)
            } else {
                match self.desc_runtime(d, var.span) {
                    Some(e) => e,
                    None => {
                        let what = match d {
                            CtDesc::Method(t, f) | CtDesc::Param(t, f, _) => self.method_label(t, f),
                            _ => String::new(),
                        };
                        self.err(format!("{} of the async method {what} has no run-time descriptor (async methods are not in TypeInfo.Methods); use only its compile-time members", var.name), var.span);
                        mk(ExprKind::Nil, ptr, var.span)
                    }
                }
            };
            let stmts = vec![Stmt { kind: StmtKind::Let { local: l, init: Some(init) }, span: var.span }, Stmt { kind: StmtKind::Block(b), span: body.span }];
            copies.push(hir::Block { stmts, span: body.span });
        }
        self.pop_label();
        if self.refl.ct_descs.is_empty() {
            self.report_copy_unused();
        }
        Some(self.unrolled_loop(label, idx, copies, span))
    }

    /// After the outermost unrolled loop: the locals of its copies that no
    /// copy used.
    fn report_copy_unused(&mut self) {
        let used = std::mem::take(&mut self.refl.copy_used);
        let mut unused = std::mem::take(&mut self.refl.copy_unused);
        unused.retain(|(f, s, _)| !used.contains(&(*f, *s)));
        unused.sort_by_key(|(f, s, _)| (*f, s.start));
        unused.dedup_by_key(|(f, s, _)| (*f, *s));
        let file = self.cur_file;
        for (f, s, name) in unused {
            self.cur_file = f;
            self.report_unused(&name, s);
        }
        self.cur_file = file;
    }

    /// `for idx in 0..n { if idx == 0 { copy0 } else if idx == 1 { ... } }`:
    /// the copies of an unrolled loop, dispatched by a counter so that
    /// `break` and `continue` keep their meaning.
    pub(crate) fn unrolled_loop(&mut self, label: hir::Label, idx: LocalId, copies: Vec<hir::Block>, span: Span) -> StmtKind {
        let int = self.tt.int;
        let n = copies.len();
        let mut chain: Option<hir::Block> = None;
        for (k, copy) in copies.into_iter().enumerate().rev() {
            let l = mk(ExprKind::Local(idx), int, span);
            let r = mk(ExprKind::Const(ConstValue::Int(k as i128)), int, span);
            let cond = mk(ExprKind::Binary { op: hir::BinOp::Eq, lhs: Box::new(l), rhs: Box::new(r) }, self.tt.bool_, span);
            let st = Stmt { kind: StmtKind::If { cond, then: copy, else_: chain.take() }, span };
            chain = Some(hir::Block { stmts: vec![st], span });
        }
        let start = mk(ExprKind::Const(ConstValue::Int(0)), int, span);
        let end = mk(ExprKind::Const(ConstValue::Int(n as i128)), int, span);
        StmtKind::ForRange { label, var: Some(idx), start, end, body: chain.unwrap_or_default() }
    }

    /// Classifies a loop head; `None` for loops that are not unrolled here.
    fn desc_over(&mut self, vars: &[ast::Ident], iter: &ast::Expr, body: &ast::Block) -> Option<Over> {
        let it = expr::strip_parens(iter);
        if let ast::ExprKind::Field { base, name } = &it.kind {
            let which = name.name.as_str();
            if which != "Methods" && which != "Params" {
                return None;
            }
            let (_, d) = self.ct_desc_ident(base)?;
            return match (which, d) {
                ("Methods", CtDesc::Type(_) | CtDesc::Abstract) => Some(Over::Methods(d)),
                ("Params", CtDesc::Method(..) | CtDesc::Abstract) => Some(Over::Params(d)),
                _ => None,
            };
        }
        // reflect.TypesWith<A>(): syntactic test first (no resolution, no
        // diagnostics twice when the loop stays a run-time loop).
        let ast::ExprKind::Call { callee, args } = &it.kind else { return None };
        let ast::ExprKind::TypeArgs { base, .. } = &expr::strip_parens(callee).kind else { return None };
        let segs = Self::flatten_path(base)?;
        if !args.is_empty() || segs.last().map(|s| s.name.as_str()) != Some("TypesWith") {
            return None;
        }
        let var = vars.first()?;
        if !needs_unroll(body, &var.name) {
            return None;
        }
        let a = self.reflect_targ(iter, "TypesWith")?;
        Some(Over::Types(a))
    }

    /// The types an unrolled `TypesWith<A>()` lists (`None`: abstract).
    fn desc_types(&mut self, a: TypeId, span: Span) -> Option<Vec<CtDesc>> {
        if self.tt.is_invalid(a) || self.has_param(a) {
            return None;
        }
        let Type::Struct(ad) = *self.tt.get(a) else { return Some(Vec::new()) };
        let ok = self.structs[ad.0 as usize].is_attribute || self.is_attr_value_struct(ad.0);
        if !ok {
            return Some(Vec::new());
        }
        let generic = self.marked_templates(ad.0);
        if let Some(g) = generic.first() {
            let an = self.tn(a);
            self.err(
                format!("for t in reflect.TypesWith<{an}>() cannot be unrolled at compile time: the generic type {g} is marked @{an} (its instantiations are known only after the whole program is analysed); use the loop as a run-time loop or remove the attribute"),
                span,
            );
            return Some(Vec::new());
        }
        Some(self.marked_types(ad.0, false).into_iter().map(CtDesc::Type).collect())
    }

    /// The run-time table entry of a descriptor (`None` for the methods
    /// and parameters of async methods, which have none).
    fn desc_runtime(&mut self, d: CtDesc, span: Span) -> Option<hir::Expr> {
        let int = self.tt.int;
        let field = |s: &Self, st: &str, f: &str| -> u32 {
            s.refl.structs.get(st).and_then(|x| s.structs[*x as usize].fields.iter().position(|y| y.name == f)).unwrap_or(0) as u32
        };
        let index = |s: &mut Self, base: hir::Expr, st: &str, f: &str, elem: &str, k: u32| -> hir::Expr {
            let sty = s.refl_struct_ty(st);
            let ety = {
                let e = s.refl_struct_ty(elem);
                s.tt.pointer(e)
            };
            let sl = s.tt.slice(ety);
            let deref = mk(ExprKind::Deref(Box::new(base)), sty, span);
            let fi = field(s, st, f);
            let fe = mk(ExprKind::Field { base: Box::new(deref), index: fi }, sl, span);
            let ke = mk(ExprKind::Const(ConstValue::Int(k as i128)), int, span);
            mk(ExprKind::Index { base: Box::new(fe), index: Box::new(ke) }, ety, span)
        };
        match d {
            CtDesc::Type(t) => Some(self.type_of_call(t, span)),
            CtDesc::Method(t, fid) | CtDesc::Param(t, fid, _) => {
                let Type::Struct(sd) = *self.tt.get(t) else { return None };
                if self.funcs[fid.0 as usize].is_async {
                    return None;
                }
                // Index in the run-time table: async methods are not listed.
                let k = self.structs[sd.0 as usize].methods.iter().take_while(|m| m.1 != fid).filter(|m| !self.funcs[m.1 .0 as usize].is_async).count() as u32;
                let ti = self.type_of_call(t, span);
                let m = index(self, ti, "TypeInfo", "Methods", "MethodInfo", k);
                match d {
                    CtDesc::Param(_, _, j) => Some(index(self, m, "MethodInfo", "Params", "ParamInfo", j)),
                    _ => Some(m),
                }
            }
            CtDesc::Abstract => None,
        }
    }

    // -- constants and types -----------------------------------------------------

    /// `x.<name>` of a descriptor variable `l`: a constant, or `None`.
    pub(crate) fn ct_desc_const(&mut self, l: LocalId, name: &ast::Ident) -> Option<hir::Expr> {
        let d = self.ct_desc_of(l)?;
        let span = name.span;
        let s = |v: String, tt: &crate::types::TypeTable| mk(ExprKind::Const(ConstValue::Str(v)), tt.string, span);
        let e = match (d, name.name.as_str()) {
            (CtDesc::Abstract, _) => {
                self.refl.abstract_ct += 1;
                return None;
            }
            (CtDesc::Type(t), n) if TYPE_CONSTS.contains(&n) => self.type_const(t, n, span)?,
            (CtDesc::Method(_, f), "Name") => s(self.funcs[f.0 as usize].name.clone(), &self.tt),
            (CtDesc::Method(_, f), "Static") => mk(ExprKind::Const(ConstValue::Bool(self.method_is_static(f))), self.tt.bool_, span),
            (CtDesc::Method(_, f), "IsAsync") => mk(ExprKind::Const(ConstValue::Bool(self.funcs[f.0 as usize].is_async)), self.tt.bool_, span),
            (CtDesc::Param(_, f, j), "Name") => s(self.param_names(f).get(j as usize).cloned().unwrap_or_default(), &self.tt),
            (CtDesc::Param(_, _, j), "Index") => mk(ExprKind::Const(ConstValue::Int(j as i128)), self.tt.int, span),
            _ => return None,
        };
        self.fcx().used[l.0 as usize] = true;
        self.ct_field_use(l, -1);
        self.refl.folds += 1;
        Some(e)
    }

    /// `Name` / `FullName` / `Package` / `Kind` of type `t` as a constant.
    pub(crate) fn type_const(&mut self, t: TypeId, what: &str, span: Span) -> Option<hir::Expr> {
        Some(match what {
            "Name" => mk(ExprKind::Const(ConstValue::Str(self.tn(t))), self.tt.string, span),
            "FullName" => mk(ExprKind::Const(ConstValue::Str(self.tn_full(t))), self.tt.string, span),
            "Package" => mk(ExprKind::Const(ConstValue::Str(self.decl_package(t))), self.tt.string, span),
            "Kind" => {
                let en = *self.refl.enums.get("Kind")?;
                let kind = self.kind_name(t);
                let v = self.enums[en as usize].variants.iter().position(|v| v.name == kind)? as u32;
                let ty = self.enum_ty(en);
                mk(ExprKind::EnumLit { enum_id: en, variant: v, args: Vec::new() }, ty, span)
            }
            _ => return None,
        })
    }

    /// `t[.Elem|.Key]*.Name|FullName|Package|Kind`, and the same after
    /// `p.Type` / `m.Result`, of a descriptor: a constant. `None` for other
    /// paths.
    pub(crate) fn ct_desc_type_path(&mut self, segs: &[&ast::Ident], span: Span) -> Option<hir::Expr> {
        let last = segs[segs.len() - 1].name.as_str();
        if !TYPE_CONSTS.contains(&last) {
            return None;
        }
        let (_, d) = self.ct_desc_named(&segs[0].name)?;
        let (root, rest) = match d {
            CtDesc::Type(t) => (Some(t), &segs[1..]),
            CtDesc::Abstract => (None, &segs[1..]),
            _ if segs.len() >= 3 => match self.desc_member_type(d, &segs[1].name)? {
                Some(t) => (Some(t), &segs[2..]),
                None => (None, &segs[2..]),
            },
            _ => return None,
        };
        let steps = &rest[..rest.len() - 1];
        if steps.iter().any(|s| s.name != "Elem" && s.name != "Key") {
            return None;
        }
        let Some(root) = root else {
            self.refl.abstract_ct += 1;
            return None;
        };
        let t = self.type_steps_pub(root, steps.iter().map(|s| s.name.as_str()))?;
        let e = self.type_const(t, last, span)?;
        self.refl.folds += 1;
        Some(e)
    }

    /// The type a descriptor operand of a type test denotes: `t`,
    /// `p.Type`, `m.Result`. `Some(None)` when abstract; `None` if `e` is
    /// none of these.
    pub(crate) fn ct_desc_operand(&mut self, e: &ast::Expr) -> Option<Option<TypeId>> {
        match &expr::strip_parens(e).kind {
            ast::ExprKind::Ident(_) => match self.ct_desc_ident(e)?.1 {
                CtDesc::Type(t) => Some(Some(t)),
                CtDesc::Abstract => Some(None),
                _ => None,
            },
            ast::ExprKind::Field { base, name } => {
                let (_, d) = self.ct_desc_ident(base)?;
                self.desc_member_type(d, &name.name)
            }
            _ => None,
        }
    }

    /// The type named by `d.<member>`: `p.Type`, `m.Result`, `m.Args`.
    fn desc_member_type(&mut self, d: CtDesc, member: &str) -> Option<Option<TypeId>> {
        match (d, member) {
            (CtDesc::Abstract, "Type" | "Result" | "Args") => Some(None),
            (CtDesc::Param(_, f, j), "Type") => self.sigs[f.0 as usize].params.get(j as usize).map(|t| Some(*t)),
            (CtDesc::Method(_, f), "Result") => Some(Some(self.sigs[f.0 as usize].ret)),
            (CtDesc::Method(t, f), "Args") => Some(Some(self.args_struct(t, f))),
            _ => None,
        }
    }

    /// A type path that starts at a descriptor (`t`, `p.Type`, `m.Result`,
    /// `m.Args`, `f.Type` of a compile-time field): its type. `None` if
    /// the path does not start at a compile-time descriptor; an error for
    /// a run-time descriptor variable (outside an unrolled loop).
    pub(crate) fn ct_desc_type(&mut self, path: &ast::Path) -> Option<TypeId> {
        if self.fns.is_empty() || path.segments.is_empty() {
            return None;
        }
        let segs = &path.segments;
        let first = &segs[0].name;
        let invalid = self.tt.invalid;
        if let Some((_, d)) = self.ct_desc_named(first) {
            let t = match (d, segs.len()) {
                (CtDesc::Type(t), 1) => Some(t),
                (CtDesc::Abstract, _) => return Some(invalid),
                (_, 2) => self.desc_member_type(d, &segs[1].name).map(|x| x.unwrap_or(invalid)),
                // `p.Type.Elem`, `m.Result.Key`, ...
                (_, n) if n > 2 && segs[2..].iter().all(|s| s.name == "Elem" || s.name == "Key") => self.desc_member_type(d, &segs[1].name).map(|x| match x {
                    Some(t) => self.type_steps_pub(t, segs[2..].iter().map(|s| s.name.as_str())).unwrap_or(invalid),
                    None => invalid,
                }),
                _ => None,
            };
            return Some(match t {
                Some(t) => t,
                None => {
                    self.err(format!("{} does not name a type (descriptor types are t, p.Type, f.Type, m.Result and m.Args)", path.to_string()), path.span);
                    invalid
                }
            });
        }
        // `f.Type`, `f.Type.Elem`, `f.Type.Elem.Key`, ...
        if segs.len() >= 2 && segs[1].name == "Type" && segs[2..].iter().all(|s| s.name == "Elem" || s.name == "Key") {
            if let Some((_, sty, k)) = self.ct_field_named(first) {
                return Some(match k.and_then(|k| self.ct_field_ty(sty, k)) {
                    Some(t) => match self.type_steps_pub(t, segs[2..].iter().map(|s| s.name.as_str())) {
                        Some(x) if !self.tt.is_invalid(x) => x,
                        Some(_) => {
                            self.err(format!("{}: the field type {} has no such element type", path.to_string(), self.tn(t)), path.span);
                            invalid
                        }
                        None => invalid,
                    },
                    None => invalid,
                });
            }
        }
        // A run-time descriptor (a variable of type *reflect.TypeInfo, ...).
        let member = segs.len() == 2 && matches!(segs[1].name.as_str(), "Type" | "Result" | "Args");
        if !self.refl.loaded_reflect() || !(member || (segs.len() == 1 && self.tt.builtin(first).is_none())) {
            return None;
        }
        let Some(Ok(VarRef::Local(l))) = self.lookup_local(first, false) else { return None };
        let lt = self.fcx().locals[l.0 as usize].ty;
        let Type::Pointer(inner) = *self.tt.get(lt) else { return None };
        let Some(sid) = self.struct_of(inner) else { return None };
        let names = ["TypeInfo", "FieldInfo", "MethodInfo", "ParamInfo"];
        if !names.iter().any(|n| self.refl.struct_id(n) == Some(sid)) {
            return None;
        }
        self.err(
            format!(
                "{} is a run-time descriptor here: a descriptor can be used as a type only inside a loop unrolled at compile time (for t in reflect.TypesWith<A>(), t.Methods, m.Params, reflect.Fields<T>())",
                path.to_string()
            ),
            path.span,
        );
        Some(invalid)
    }

    /// `m.Args` of method `fid` of struct type `t`: a struct with one field
    /// per parameter (named and typed like it), created once per method.
    pub(crate) fn args_struct(&mut self, t: TypeId, fid: FuncId) -> TypeId {
        let full = format!("{}.{}$Args", self.tn_full(t), self.funcs[fid.0 as usize].name);
        if let Some(&sid) = self.refl.args_structs.get(&fid.0) {
            if self.structs.get(sid as usize).is_some_and(|s| s.full_name == full) {
                return self.struct_ty(sid);
            }
        }
        static EMPTY: OnceLock<ast::StructDecl> = OnceLock::new();
        let decl: &'static ast::StructDecl = EMPTY.get_or_init(|| ast::StructDecl {
            attrs: Vec::new(),
            name: ast::Ident { name: "Args".into(), span: Span::DUMMY },
            generics: Vec::new(),
            fields: Vec::new(),
            embeds: Vec::new(),
            methods: Vec::new(),
            implements: Vec::new(),
            span: Span::DUMMY,
        });
        let names = self.param_names(fid);
        let tys = self.sigs[fid.0 as usize].params.clone();
        let fields = names
            .into_iter()
            .zip(tys)
            .map(|(name, ty)| hir::StructField { exported: is_exported(&name), name, ty, embedded: false, default: None, attrs: Vec::new(), span: Span::DUMMY })
            .collect();
        let f = &self.funcs[fid.0 as usize];
        let (ns, file, span) = (f.namespace, f.file, f.span);
        let sid = self.structs.len() as u32;
        let name = format!("{}.Args", self.method_label(t, fid));
        self.structs.push(hir::Struct { name, full_name: full, namespace: ns, fields, methods: Vec::new(), attrs: Vec::new(), is_attribute: false, exported: true, file, span });
        self.struct_src.push(decl);
        self.refl.args_structs.insert(fid.0, sid);
        self.struct_ty(sid)
    }

    /// `len(m.Params)` of a compile-time method: a constant.
    pub(crate) fn ct_desc_len(&mut self, e: &ast::Expr) -> Option<hir::Expr> {
        let ast::ExprKind::Field { base, name } = &expr::strip_parens(e).kind else { return None };
        if name.name != "Params" {
            return None;
        }
        let (_, CtDesc::Method(_, f)) = self.ct_desc_ident(base)? else { return None };
        self.refl.folds += 1;
        let n = self.sigs[f.0 as usize].params.len();
        Some(mk(ExprKind::Const(ConstValue::Int(n as i128)), self.tt.int, e.span))
    }

    // -- calls ------------------------------------------------------------------------

    /// Method calls on a descriptor variable `l` (already checked as
    /// `base`): `x.Attribute<B>()`, `m.CallOn`, `m.Apply`, `p.Get`. `None`
    /// for anything else.
    pub(crate) fn ct_desc_call(&mut self, l: LocalId, name: &ast::Ident, targs: Option<&[ast::TypeExpr]>, args: &[ast::Expr], span: Span) -> Option<hir::Expr> {
        let d = self.ct_desc_of(l)?;
        let n = name.name.as_str();
        if d == CtDesc::Abstract {
            if matches!(n, "CallOn" | "Apply" | "Get" | "Set") {
                self.check_args_loose(args);
                return Some(self.invalid(span));
            }
            if n == "Attribute" {
                self.refl.abstract_ct += 1;
            }
            return None;
        }
        let folded = |s: &mut Self| {
            s.fcx().used[l.0 as usize] = true;
            s.ct_field_use(l, -1);
        };
        match (d, n, targs) {
            (_, "Attribute", Some([a])) if args.is_empty() => {
                let aty = self.resolve_type(a);
                if self.tt.is_invalid(aty) {
                    return Some(self.invalid(span));
                }
                let Type::Struct(ad) = *self.tt.get(aty) else { return None };
                let attrs = match d {
                    CtDesc::Type(t) => match *self.tt.get(t) {
                        Type::Struct(x) => self.structs[x.0 as usize].attrs.clone(),
                        Type::Enum(x) => self.enums[x.0 as usize].attrs.clone(),
                        Type::Interface(x) => self.interfaces[x.0 as usize].attrs.clone(),
                        _ => Vec::new(),
                    },
                    CtDesc::Method(_, f) => self.funcs[f.0 as usize].attrs.clone(),
                    CtDesc::Param(_, f, j) => self.funcs[f.0 as usize].param_attrs.get(j as usize).cloned().unwrap_or_default(),
                    CtDesc::Abstract => Vec::new(),
                };
                folded(self);
                self.refl.folds += 1;
                Some(self.attr_const_of(attrs, ad.0, aty, span))
            }
            (CtDesc::Method(t, f), "CallOn" | "Apply", None) => {
                folded(self);
                Some(self.desc_method_call(t, f, n == "Apply", args, span))
            }
            (CtDesc::Param(t, f, j), "Get", None) => {
                folded(self);
                if args.len() != 1 {
                    self.err(format!("p.Get takes one argument (a value of type m.Args), got {}", args.len()), span);
                    self.check_args_loose(args);
                    return Some(self.invalid(span));
                }
                let at = self.args_struct(t, f);
                let v = self.value(&args[0], Some(at));
                let v = self.auto_deref(v);
                if self.tt.is_invalid(v.ty) {
                    return Some(self.invalid(span));
                }
                if v.ty != at {
                    self.err(format!("p.Get needs a value of type {} (m.Args), found {}", self.tn(at), self.tn(v.ty)), v.span);
                    return Some(self.invalid(span));
                }
                let pty = self.sigs[f.0 as usize].params[j as usize];
                Some(mk(ExprKind::Field { base: Box::new(v), index: j }, pty, span))
            }
            (CtDesc::Param(..), "Set", None) => {
                self.err("p.Set(&args, value) is a statement (it stores into the argument), not a value", span);
                self.check_args_loose(args);
                Some(self.invalid(span))
            }
            _ => None,
        }
    }

    /// `m.CallOn(recv, args...)` / `m.Apply(recv, args)`: a direct call of
    /// method `fid` of struct type `t` (no receiver for a static method).
    fn desc_method_call(&mut self, t: TypeId, fid: FuncId, apply: bool, args: &[ast::Expr], span: Span) -> hir::Expr {
        let call = if apply { "m.Apply" } else { "m.CallOn" };
        let label = self.method_label(t, fid);
        let sig = self.sigs[fid.0 as usize].clone();
        let mut hargs = Vec::new();
        let mut rest = args;
        if !self.method_is_static(fid) {
            let Some(first) = args.first() else {
                self.err(format!("{call} needs the receiver (a *{}) as its first argument for {label}", self.tn(t)), span);
                return self.invalid(span);
            };
            let r = self.value(first, None);
            let recv = match *self.tt.get(r.ty) {
                _ if self.tt.is_invalid(r.ty) => {
                    self.check_args_loose(&args[1..]);
                    return self.invalid(span);
                }
                Type::Pointer(p) if p == t => r,
                _ if r.ty == t => self.addr_of(r),
                _ => {
                    self.err(format!("{call} needs a *{} receiver for {label}, found {}", self.tn(t), self.tn(r.ty)), r.span);
                    self.check_args_loose(&args[1..]);
                    return self.invalid(span);
                }
            };
            hargs.push(recv);
            rest = &args[1..];
        }
        if apply {
            let at = self.args_struct(t, fid);
            if rest.len() != 1 {
                self.err(format!("m.Apply takes the receiver (unless the method is static) and one value of type m.Args, got {} arguments", args.len()), span);
                self.check_args_loose(rest);
                return self.invalid(span);
            }
            let v = self.value(&rest[0], Some(at));
            let v = self.auto_deref(v);
            if self.tt.is_invalid(v.ty) {
                return self.invalid(span);
            }
            if v.ty != at {
                self.err(format!("m.Apply needs a value of type {} (m.Args) for {label}, found {}", self.tn(at), self.tn(v.ty)), v.span);
                return self.invalid(span);
            }
            if !pure_place(&v) {
                self.err("m.Apply needs a variable of type m.Args (var args m.Args), not an expression", v.span);
                return self.invalid(span);
            }
            for (j, pty) in sig.params.iter().enumerate() {
                hargs.push(mk(ExprKind::Field { base: Box::new(v.clone()), index: j as u32 }, *pty, v.span));
            }
        } else {
            hargs.extend(self.check_args(&sig.params, rest, &label, span));
            if hargs.len() < sig.params.len() + usize::from(!self.method_is_static(fid)) {
                return self.invalid(span);
            }
        }
        let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
        mk(ExprKind::Call { callee: hir::Callee::Func(fid), args: hargs }, ty, span)
    }

    /// `p.Set(&args, value)` as a statement, `p` a compile-time parameter;
    /// `None` if `e` is no such call.
    pub(crate) fn ct_param_set(&mut self, e: &ast::Expr) -> Option<StmtKind> {
        let ast::ExprKind::Call { callee, args } = &expr::strip_parens(e).kind else { return None };
        let ast::ExprKind::Field { base, name } = &expr::strip_parens(callee).kind else { return None };
        if name.name != "Set" {
            return None;
        }
        let (_, d) = self.ct_desc_ident(base)?;
        let empty = Some(StmtKind::Block(hir::Block::default()));
        let (t, f, j) = match d {
            CtDesc::Param(t, f, j) => (t, f, j),
            CtDesc::Abstract => {
                self.check_args_loose(args);
                return empty;
            }
            _ => return None,
        };
        if args.len() != 2 {
            self.err(format!("p.Set takes two arguments (a pointer to the m.Args value and the argument), got {}", args.len()), e.span);
            self.check_args_loose(args);
            return empty;
        }
        let at = self.args_struct(t, f);
        let target = self.value(&args[0], None);
        if self.tt.is_invalid(target.ty) {
            self.value(&args[1], None);
            return empty;
        }
        if !matches!(*self.tt.get(target.ty), Type::Pointer(p) if p == at) {
            self.err(format!("p.Set needs a *{} (a pointer to an m.Args value), found {}", self.tn(at), self.tn(target.ty)), target.span);
            self.value(&args[1], None);
            return empty;
        }
        let pty = self.sigs[f.0 as usize].params[j as usize];
        let value = self.expr_to(&args[1], pty, Ctx::Arg(j as usize, self.method_label(t, f)));
        let span = target.span;
        let place = match target.kind {
            ExprKind::AddrOf(inner) => *inner,
            _ => mk(ExprKind::Deref(Box::new(target)), at, span),
        };
        let target = mk(ExprKind::Field { base: Box::new(place), index: j }, pty, span);
        Some(StmtKind::Assign { target, value })
    }
}

/// A side-effect-free place expression (a variable, its fields, a
/// dereference of one): may be read once per argument.
fn pure_place(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Local(_) | ExprKind::Upvalue(_) => true,
        ExprKind::Field { base, .. } | ExprKind::Deref(base) => pure_place(base),
        _ => false,
    }
}

// -- the unrolling rule ------------------------------------------------------------------

/// True if `body` of `for <var> in reflect.TypesWith<A>()` needs `var` as a
/// compile-time descriptor: it (or a descriptor reached from it by nested
/// `for m in var.Methods` / `for p in m.Params` / `reflect.Fields<var>()`)
/// is used as a type, `CallOn`/`Apply`/`Get`/`Set` is called on one, or the
/// body calls `reflect.Error`.
pub(crate) fn needs_unroll(body: &ast::Block, var: &str) -> bool {
    let mut roots = vec![var.to_string()];
    block_needs(body, &mut roots)
}

fn block_needs(b: &ast::Block, roots: &mut Vec<String>) -> bool {
    b.stmts.iter().any(|s| stmt_needs(s, roots))
}

fn stmts_needs(ss: &[ast::Stmt], roots: &mut Vec<String>) -> bool {
    ss.iter().any(|s| stmt_needs(s, roots))
}

fn stmt_needs(s: &ast::Stmt, roots: &mut Vec<String>) -> bool {
    match &s.kind {
        ast::StmtKind::Var(v) => v.ty.as_ref().is_some_and(|t| ty_needs(t, roots)) || v.init.as_ref().is_some_and(|e| expr_needs(e, roots)),
        ast::StmtKind::Const(c) => c.ty.as_ref().is_some_and(|t| ty_needs(t, roots)) || expr_needs(&c.value, roots),
        ast::StmtKind::If(i) => if_needs(i, roots),
        ast::StmtKind::For(f) => {
            let head = match &f.kind {
                ast::ForKind::Infinite => false,
                ast::ForKind::While(c) => expr_needs(c, roots),
                ast::ForKind::In { vars, iter } => {
                    if derives(iter, roots) {
                        roots.extend(vars.iter().map(|v| v.name.clone()));
                    }
                    expr_needs(iter, roots)
                }
            };
            head || block_needs(&f.body, roots)
        }
        ast::StmtKind::Switch(sw) => {
            sw.subject.as_ref().is_some_and(|e| expr_needs(e, roots))
                || sw.cases.iter().any(|c| {
                    c.patterns.iter().any(|p| match &p.kind {
                        ast::PatternKind::Expr(x) => expr_needs(x, roots),
                        ast::PatternKind::Range(lo, hi) => lo.as_ref().is_some_and(|e| expr_needs(e, roots)) || hi.as_ref().is_some_and(|e| expr_needs(e, roots)),
                        ast::PatternKind::TypeBind { ty, .. } => ty_needs(ty, roots),
                    }) || stmts_needs(&c.body, roots)
                })
                || sw.default.as_ref().is_some_and(|d| stmts_needs(d, roots))
        }
        ast::StmtKind::Return(es) => es.iter().any(|e| expr_needs(e, roots)),
        ast::StmtKind::Defer(x) | ast::StmtKind::Expr(x) => expr_needs(x, roots),
        ast::StmtKind::Assign { target, value, .. } => expr_needs(target, roots) || expr_needs(value, roots),
        ast::StmtKind::Block(x) => block_needs(x, roots),
        ast::StmtKind::TryBlock { body, handler, .. } => block_needs(body, roots) || block_needs(handler, roots),
        ast::StmtKind::Labeled { stmt, .. } => stmt_needs(stmt, roots),
        ast::StmtKind::Break(_) | ast::StmtKind::Continue(_) | ast::StmtKind::Error => false,
    }
}

fn if_needs(i: &ast::IfStmt, roots: &mut Vec<String>) -> bool {
    expr_needs(&i.cond, roots)
        || block_needs(&i.then, roots)
        || match i.else_.as_deref() {
            Some(ast::ElseBranch::If(x)) => if_needs(x, roots),
            Some(ast::ElseBranch::Block(b)) => block_needs(b, roots),
            None => false,
        }
}

/// `root.Methods`, `root.Params`, `reflect.Fields<root>()`: a sequence of
/// descriptors reached from a root.
fn derives(iter: &ast::Expr, roots: &[String]) -> bool {
    match &expr::strip_parens(iter).kind {
        ast::ExprKind::Field { base, name } => (name.name == "Methods" || name.name == "Params") && is_root(base, roots),
        ast::ExprKind::Call { callee, .. } => match &expr::strip_parens(callee).kind {
            ast::ExprKind::TypeArgs { args, .. } => args.iter().any(|t| ty_needs(t, roots)),
            _ => false,
        },
        _ => false,
    }
}

fn is_root(e: &ast::Expr, roots: &[String]) -> bool {
    matches!(&expr::strip_parens(e).kind, ast::ExprKind::Ident(id) if roots.contains(&id.name))
}

fn ty_needs(t: &ast::TypeExpr, roots: &[String]) -> bool {
    use ast::TypeKind as K;
    match &t.kind {
        K::Named { path, generics } => path.segments.first().is_some_and(|s| roots.contains(&s.name)) || generics.iter().any(|g| ty_needs(g, roots)),
        K::Fallible(x) | K::Pointer(x) | K::Weak(x) | K::Slice(x) | K::Variadic(x) | K::Array(x, _) | K::ArrayExpr(x, _) => ty_needs(x, roots),
        K::Map(k, v) => ty_needs(k, roots) || ty_needs(v, roots),
        K::Func { params, ret } => params.iter().any(|p| ty_needs(p, roots)) || ret.as_ref().is_some_and(|r| ty_needs(r, roots)),
        K::Tuple(xs) => xs.iter().any(|x| ty_needs(x, roots)),
        K::Error | K::Infer => false,
    }
}

fn expr_needs(x: &ast::Expr, roots: &mut Vec<String>) -> bool {
    use ast::ExprKind as E;
    match &x.kind {
        E::Call { callee, args } => {
            let special = match &expr::strip_parens(callee).kind {
                E::Field { base, name } => {
                    (matches!(name.name.as_str(), "CallOn" | "Apply" | "Get" | "Set") && is_root(base, roots))
                        || (name.name == "Error" && matches!(&expr::strip_parens(base).kind, E::Ident(id) if id.name == "reflect"))
                }
                _ => false,
            };
            special || expr_needs(callee, roots) || args.iter().any(|a| expr_needs(a, roots))
        }
        E::Unary { expr, .. } | E::Await(expr) | E::New(expr) | E::Paren(expr) => expr_needs(expr, roots),
        E::Field { base, .. } => expr_needs(base, roots),
        E::TypeArgs { base, args } => args.iter().any(|t| ty_needs(t, roots)) || expr_needs(base, roots),
        E::Index { base, index } => expr_needs(base, roots) || expr_needs(index, roots),
        E::Binary { left, right, .. } => expr_needs(left, roots) || expr_needs(right, roots),
        E::Propagate { expr, context } => expr_needs(expr, roots) || context.as_deref().is_some_and(|c| expr_needs(c, roots)),
        E::Try { expr, handler } => {
            expr_needs(expr, roots)
                || match handler {
                    ast::TryHandler::Catch { body, .. } => block_needs(body, roots),
                    ast::TryHandler::Else(v) => match &**v {
                        ast::ElseValue::Expr(y) => expr_needs(y, roots),
                        ast::ElseValue::Return(ys, _) => ys.iter().any(|y| expr_needs(y, roots)),
                        ast::ElseValue::Continue(_) | ast::ElseValue::Break(_) => false,
                    },
                }
        }
        E::StructLit { path, generics, fields } => {
            path.segments.first().is_some_and(|s| roots.contains(&s.name)) || generics.iter().any(|g| ty_needs(g, roots)) || fields.iter().any(|f| expr_needs(&f.value, roots))
        }
        E::CompositeLit { generics, elems, .. } => generics.iter().any(|g| ty_needs(g, roots)) || elems.iter().any(|(k, v)| k.as_ref().is_some_and(|k| expr_needs(k, roots)) || expr_needs(v, roots)),
        E::SliceLit(xs) | E::Tuple(xs) => xs.iter().any(|e| expr_needs(e, roots)),
        E::MapLit { key_ty, val_ty, entries } => ty_needs(key_ty, roots) || ty_needs(val_ty, roots) || entries.iter().any(|(k, v)| expr_needs(k, roots) || expr_needs(v, roots)),
        E::Range(a, b) => a.as_deref().is_some_and(|e| expr_needs(e, roots)) || b.as_deref().is_some_and(|e| expr_needs(e, roots)),
        E::Lambda { params, ret, body, .. } => {
            params.iter().any(|p| ty_needs(&p.ty, roots))
                || ret.as_deref().is_some_and(|r| ty_needs(r, roots))
                || match &**body {
                    ast::LambdaBody::Block(b) => block_needs(b, roots),
                    ast::LambdaBody::Expr(y) => expr_needs(y, roots),
                }
        }
        E::NamedArg { value, .. } => expr_needs(value, roots),
        _ => false,
    }
}
