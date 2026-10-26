//! Uses of generic declarations in expressions: calls of generic functions
//! and methods (explicit type arguments or inference from the argument
//! types), generic function values (`F<int>`), members of instantiated
//! types (`Option<int>.Some(1)`, `List<int>.New()`) and patterns.
//!
//! # Inference
//!
//! Without explicit type arguments, each type parameter is inferred from
//! the arguments by unifying the declared parameter types (as written)
//! with the argument types, structurally (`[]T` against `[]int`,
//! `func(T) U` against `func(int) string`, `Pair<A, B>` against an
//! instance of `Pair`). Arguments that are untyped constants are typed
//! last: a parameter of type `T` already inferred from another argument
//! gives them its type (`Max(x, 1)` with `x float`), otherwise they take
//! their default type (`Max(1, 2)` is `Max<int>`). Two different types for
//! one parameter, or a parameter that no argument determines, are errors.

use super::coerce::Ctx;
use super::generics::{Inst, TmplDecl};
use super::resolve::PathRes;
use super::*;
use crate::hir::{Callee, ExprKind};

/// True if type expression `p` mentions one of the type parameters `names`.
fn mentions(p: &ast::TypeExpr, names: &[String]) -> bool {
    use ast::TypeKind as K;
    match &p.kind {
        K::Named { path, generics } => (path.segments.len() == 1 && generics.is_empty() && names.contains(&path.segments[0].name)) || generics.iter().any(|g| mentions(g, names)),
        K::Fallible(x) | K::Pointer(x) | K::Weak(x) | K::Slice(x) | K::Variadic(x) | K::Array(x, _) | K::ArrayExpr(x, _) => mentions(x, names),
        K::Map(k, v) => mentions(k, names) || mentions(v, names),
        K::Func { params, ret } => params.iter().any(|x| mentions(x, names)) || ret.as_ref().is_some_and(|r| mentions(r, names)),
        K::Tuple(ts) => ts.iter().any(|x| mentions(x, names)),
        K::Error | K::Infer => false,
    }
}

/// Index of the type parameter if `p` is exactly one (`T`).
fn bare_param(p: &ast::TypeExpr, names: &[String]) -> Option<usize> {
    match &p.kind {
        ast::TypeKind::Named { path, generics } if path.segments.len() == 1 && generics.is_empty() => names.iter().position(|n| *n == path.segments[0].name),
        _ => None,
    }
}

/// True for a short lambda with an untyped parameter (`(u) => ...`).
fn untyped_lambda(e: &ast::Expr) -> bool {
    matches!(&expr::strip_parens(e).kind, ast::ExprKind::Lambda { params, .. } if params.iter().any(|p| matches!(p.ty.kind, ast::TypeKind::Infer)))
}

impl<'a> Sema<'a> {
    /// The expected type of an untyped lambda argument `a` for parameter
    /// type `p` (`func(T) U`) once the type parameters in its parameter
    /// types are inferred: a function type whose parameters are known (the
    /// result is inferred from the lambda's body).
    fn lambda_hint(&mut self, a: &ast::Expr, p: &ast::TypeExpr, names: &[String], binds: &[Option<TypeId>], outer: &[(String, TypeId)], scope: (usize, NsIdx)) -> Option<TypeId> {
        if !untyped_lambda(a) {
            return None;
        }
        // `expr<func(T) bool>`: the lambda becomes a tree of that function type.
        let p = match &p.kind {
            ast::TypeKind::Named { path, generics } if path.segments.len() == 1 && path.segments[0].name == "expr" && generics.len() == 1 => &generics[0],
            _ => p,
        };
        let ast::TypeKind::Func { params, .. } = &p.kind else { return None };
        if params.iter().any(|x| mentions(x, names) && names.iter().zip(binds).any(|(n, b)| b.is_none() && mentions(x, std::slice::from_ref(n)))) {
            return None;
        }
        let n = self.gen.tenv.len();
        // a method of a generic struct instance: the struct's bindings
        self.gen.tenv.extend(outer.iter().cloned());
        for (name, b) in names.iter().zip(binds) {
            if let Some(b) = b {
                self.gen.tenv.push((name.clone(), *b));
            }
        }
        // the parameter types are written in the template's scope
        let saved = (self.cur_file, self.cur_ns);
        (self.cur_file, self.cur_ns) = scope;
        self.gen.mute += 1;
        let ps: Vec<TypeId> = params.iter().map(|x| self.resolve_type(x)).collect();
        self.gen.mute -= 1;
        (self.cur_file, self.cur_ns) = saved;
        self.gen.tenv.truncate(n);
        if ps.iter().any(|t| self.tt.is_invalid(*t)) {
            return None;
        }
        let void = self.tt.void;
        Some(self.tt.func(ps, void))
    }

    /// `expr<F>` written as a parameter type.
    fn is_expr_type(p: &ast::TypeExpr) -> bool {
        matches!(&p.kind, ast::TypeKind::Named { path, generics } if path.segments.len() == 1 && path.segments[0].name == "expr" && generics.len() == 1)
    }

    /// Unifies written parameter type `p` with argument type `actual`,
    /// binding type parameters; `Err((param, first, second))` on a conflict.
    fn unify(&self, p: &ast::TypeExpr, actual: TypeId, names: &[String], binds: &mut [Option<TypeId>]) -> Result<(), (usize, TypeId, TypeId)> {
        use ast::TypeKind as K;
        if matches!(self.tt.get(actual), Type::Invalid | Type::Nil) {
            return Ok(());
        }
        if let Some(k) = bare_param(p, names) {
            return match binds[k] {
                None => {
                    binds[k] = Some(actual);
                    Ok(())
                }
                Some(b) if b == actual => Ok(()),
                Some(b) => Err((k, b, actual)),
            };
        }
        let a = self.tt.get(actual).clone();
        match (&p.kind, a) {
            (K::Named { path, generics }, Type::Struct(d)) if !generics.is_empty() => {
                if let Some((t, args)) = self.gen.inst_structs.get(&d.0) {
                    let last = &path.segments.last().unwrap().name;
                    let tname = &self.gen.templates[*t as usize].name;
                    // `expr<F>` names the struct `expr.Expr<F>`.
                    if (tname == last || (last == "expr" && path.segments.len() == 1 && self.expr_tree_fn(actual).is_some())) && args.len() == generics.len() {
                        for (g, x) in generics.iter().zip(args.clone()) {
                            self.unify(g, x, names, binds)?;
                        }
                    }
                }
            }
            (K::Named { path, generics }, Type::Enum(d)) if !generics.is_empty() => {
                if let Some((t, args)) = self.gen.inst_enums.get(&d.0) {
                    if self.gen.templates[*t as usize].name == path.segments.last().unwrap().name && args.len() == generics.len() {
                        for (g, x) in generics.iter().zip(args.clone()) {
                            self.unify(g, x, names, binds)?;
                        }
                    }
                }
            }
            (K::Named { path, generics }, Type::Task(x)) if path.segments.len() == 1 && path.segments[0].name == "Task" && generics.len() == 1 => {
                self.unify(&generics[0], x, names, binds)?
            }
            (K::Pointer(p), Type::Pointer(x)) | (K::Weak(p), Type::Weak(x)) | (K::Slice(p), Type::Slice(x)) | (K::Fallible(p), Type::Fallible(x)) => self.unify(p, x, names, binds)?,
            (K::Array(p, n), Type::Array(x, m)) if *n == m => self.unify(p, x, names, binds)?,
            (K::Map(pk, pv), Type::Map(k, v)) => {
                self.unify(pk, k, names, binds)?;
                self.unify(pv, v, names, binds)?;
            }
            (K::Func { params, ret }, Type::Func(sig)) if params.len() == sig.params.len() => {
                for (p, x) in params.iter().zip(&sig.params) {
                    self.unify(p, *x, names, binds)?;
                }
                if let Some(r) = ret {
                    self.unify(r, sig.ret, names, binds)?;
                }
            }
            (K::Tuple(ps), Type::Tuple(xs)) if ps.len() == xs.len() => {
                for (p, x) in ps.iter().zip(xs) {
                    self.unify(p, x, names, binds)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Infers the type arguments of a call of `t` from `args`; typed
    /// arguments are left in `typed` for the caller.
    fn infer_type_args(&mut self, t: u32, decl: &ast::FuncDecl, args: &[ast::Expr], typed: &mut [Option<hir::Expr>], span: Span, hint: Option<TypeId>) -> Option<Vec<TypeId>> {
        let tm = &self.gen.templates[t as usize];
        let names: Vec<String> = tm.generics.iter().map(|g| g.name.name.clone()).collect();
        let fname = tm.name.clone();
        let outer = tm.outer.clone();
        let scope = (tm.file, tm.ns);
        let mut binds: Vec<Option<TypeId>> = vec![None; names.len()];
        let mut bad = false;
        for pass in 0..2 {
            for (i, (p, a)) in decl.params.iter().zip(args).enumerate() {
                if !mentions(&p.ty, &names) || typed[i].is_some() {
                    continue;
                }
                let untyped = self.is_untyped(a) || untyped_lambda(a);
                if untyped != (pass == 1) {
                    continue;
                }
                let hint = bare_param(&p.ty, &names).and_then(|k| binds[k]).or_else(|| self.lambda_hint(a, &p.ty, &names, &binds, &outer, scope));
                if hint.is_none() && matches!(expr::strip_parens(a).kind, ast::ExprKind::Nil) {
                    continue;
                }
                // `expr<F>`: the lambda becomes a tree; its result type
                // binds the type parameters of F's result.
                let v = match (hint, Self::is_expr_type(&p.ty), &expr::strip_parens(a).kind) {
                    (Some(h), true, ast::ExprKind::Lambda { .. }) => self.expr_tree_infer(a, h),
                    (None, true, ast::ExprKind::Lambda { params, .. }) if !untyped => {
                        let ps: Vec<TypeId> = params.iter().map(|x| self.resolve_type(&x.ty)).collect();
                        let void = self.tt.void;
                        let h = self.tt.func(ps, void);
                        self.expr_tree_infer(a, h)
                    }
                    // `nil` for a parameter whose type is already bound
                    (Some(h), _, ast::ExprKind::Nil) => {
                        let v = self.value(a, hint);
                        self.coerce(v, h, Ctx::Arg(i, fname.clone()))
                    }
                    _ => self.value(a, hint),
                };
                let v = self.default_type(v);
                bad |= self.tt.is_invalid(v.ty);
                if let Err((k, x, y)) = self.unify(&p.ty, v.ty, &names, &mut binds) {
                    let (x, y) = (self.tn(x), self.tn(y));
                    self.err(format!("conflicting types for type parameter {} of {fname}: {x} and {y}", names[k]), v.span);
                    bad = true;
                }
                typed[i] = Some(v);
            }
        }
        if bad {
            return None;
        }
        // Type parameters only in the result: from the expected type
        // (`return Nf()` in a function returning `Res<int>` or `Res<int>!`).
        if let (true, Some(h), Some(ret)) = (binds.iter().any(|b| b.is_none()), hint, decl.ret.as_ref()) {
            let h = match (&ret.kind, self.tt.fallible_inner(h)) {
                (ast::TypeKind::Fallible(_), _) | (_, None) => h,
                (_, Some(inner)) => inner,
            };
            let _ = self.unify(ret, h, &names, &mut binds);
        }
        if let Some(k) = binds.iter().position(|b| b.is_none()) {
            self.err(format!("cannot infer type parameter {} of {fname} from the arguments; write {fname}<...>(...)", names[k]), span);
            return None;
        }
        Some(binds.into_iter().map(|b| b.unwrap()).collect())
    }

    /// Call of generic function or method template `t`. `recv` is the
    /// receiver (`*S`) for non-static methods.
    pub(crate) fn generic_call(&mut self, t: u32, targs: Option<&[ast::TypeExpr]>, recv: Option<hir::Expr>, args: &[ast::Expr], span: Span) -> hir::Expr {
        let hint = self.gen.ret_hint.take();
        if let Some(e) = self.reflect_generic_call(t, span) {
            return e;
        }
        self.types_with_check(t, targs, span);
        // data: models, contexts and migrations generated at compile time (orm.rs).
        if recv.is_none() {
            if let Some(e) = self.orm_generic_call(t, targs, args, span) {
                return e;
            }
        }
        self.force_template(t);
        let tm = &self.gen.templates[t as usize];
        let (decl, name) = match tm.decl {
            TmplDecl::Func(d) | TmplDecl::Method(d, _) => (d, tm.name.clone()),
            _ => {
                if let Some(e) = self.std_type_call(t, targs, args, span) {
                    return e;
                }
                let tm = &self.gen.templates[t as usize];
                let n = tm.name.clone();
                self.err(format!("cannot call type {n} (use a struct literal {n}<...>{{...}})"), span);
                self.check_args_loose(args);
                return self.invalid(span);
            }
        };
        let mut typed: Vec<Option<hir::Expr>> = vec![None; args.len()];
        let types = match targs {
            Some(ta) => self.type_args(t, ta, span),
            None if args.len() != decl.params.len() => {
                self.err(format!("wrong number of arguments in call to {name}: expected {}, got {}", decl.params.len(), args.len()), span);
                None
            }
            None => self.infer_type_args(t, decl, args, &mut typed, span, hint),
        };
        let checked = types.clone();
        let fid = match types.and_then(|ts| if ts.is_empty() { self.plain_std_inst(t, span).or_else(|| self.instantiate(t, ts, span)) } else { self.instantiate(t, ts, span) }) {
            Some(Inst::Func(f)) => f,
            _ => {
                for (a, v) in args.iter().zip(&typed) {
                    if v.is_none() {
                        self.expr(a, None);
                    }
                }
                return self.invalid(span);
            }
        };
        // `regex.Pattern("...")`, `template.Parse<T>("...")`: literal checks (litcheck.rs).
        self.std_literal_check(t, checked.as_deref().unwrap_or(&[]), args);
        let sig = self.sigs[fid.0 as usize].clone();
        let fname = self.funcs[fid.0 as usize].name.clone();
        let mut hargs: Vec<hir::Expr> = recv.into_iter().collect();
        if sig.params.len() != args.len() {
            self.err(format!("wrong number of arguments in call to {fname}: expected {}, got {}", sig.params.len(), args.len()), span);
            self.check_args_loose(args);
            return self.invalid(span);
        }
        for (i, a) in args.iter().enumerate() {
            let ctx = Ctx::Arg(i, fname.clone());
            let v = match typed[i].take() {
                Some(v) => self.coerce(v, sig.params[i], ctx),
                None => self.expr_to(a, sig.params[i], ctx),
            };
            hargs.push(v);
        }
        let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
        mk(ExprKind::Call { callee: Callee::Func(fid), args: hargs }, ty, span)
    }

    /// `callee<targs>(args)`.
    pub(crate) fn call_with_type_args(&mut self, base: &ast::Expr, targs: &[ast::TypeExpr], args: &[ast::Expr], span: Span) -> hir::Expr {
        let b = expr::strip_parens(base);
        if let Some(segs) = Self::flatten_path(b) {
            let last = *segs.last().unwrap();
            if self.lookup_local(&segs[0].name, false).is_some() {
                if segs.len() == 1 {
                    self.err(format!("{} is a variable; type arguments need a generic function or method", last.name), last.span);
                    self.check_args_loose(args);
                    return self.invalid(span);
                }
                let recv = self.path_expr(&segs[..segs.len() - 1], b.span, None);
                return self.method_call_targs(recv, last, Some(targs), args, span);
            }
            let Some((res, n)) = self.resolve_segments(&segs, true) else {
                if self.lookup_global(segs[0], false).is_none() && !self.diags.has_errors_at(segs[0].span, self.cur_file) {
                    self.unknown_ident(segs[0]);
                }
                self.check_args_loose(args);
                return self.invalid(span);
            };
            if n == segs.len() {
                if let PathRes::Member(Member::Generic(t)) = res {
                    return self.generic_call(t, Some(targs), None, args, span);
                }
                if let PathRes::Member(Member::Special(sp)) = res {
                    if let Some(e) = self.typed_special(sp, targs, args, span) {
                        return e;
                    }
                }
                self.err(format!("{} is not generic (type arguments are only allowed on generic functions, methods and types)", last.name), last.span);
                self.check_args_loose(args);
                return self.invalid(span);
            }
            let sp = segs[0].span.to(segs[n - 1].span);
            let mut recv = self.path_value(res, &segs[n - 1].name, sp, None);
            for seg in &segs[n..segs.len() - 1] {
                recv = self.field_access(recv, seg);
            }
            return self.method_call_targs(recv, last, Some(targs), args, span);
        }
        if let ast::ExprKind::Field { base: inner, name } = &b.kind {
            let recv = self.value(inner, None);
            return self.method_call_targs(recv, name, Some(targs), args, span);
        }
        self.err("type arguments are only allowed on generic functions, methods and types", base.span);
        self.check_args_loose(args);
        self.invalid(span)
    }

    /// `F<int>` used as a value: the instantiated function.
    pub(crate) fn generic_value(&mut self, base: &ast::Expr, targs: &[ast::TypeExpr], span: Span) -> hir::Expr {
        let b = expr::strip_parens(base);
        if let Some(segs) = Self::flatten_path(b) {
            if self.lookup_local(&segs[0].name, false).is_none() {
                if let Some((PathRes::Member(Member::Generic(t)), n)) = self.resolve_segments(&segs, true) {
                    if n == segs.len() {
                        let is_func = match self.gen.templates[t as usize].decl {
                            TmplDecl::Func(_) => true,
                            TmplDecl::Method(d, _) => d.is_static,
                            _ => false,
                        };
                        if !is_func {
                            let name = self.gen.templates[t as usize].name.clone();
                            self.err(format!("{name}<...> is a type, not a value"), span);
                            return self.invalid(span);
                        }
                        let Some(args) = self.type_args(t, targs, span) else { return self.invalid(span) };
                        let Some(Inst::Func(f)) = self.instantiate(t, args, span) else { return self.invalid(span) };
                        let ty = self.func_value_ty(f);
                        return mk(ExprKind::FuncRef(f), ty, span);
                    }
                }
            }
        }
        if !self.diags.has_errors_at(base.span, self.cur_file) {
            self.err("type arguments are only allowed on generic functions and types (generic methods must be called)", span);
        }
        self.invalid(span)
    }

    /// The type named by `Name<args>` in an expression, if it is an
    /// instantiated generic struct or enum.
    pub(crate) fn type_args_type(&mut self, e: &ast::Expr) -> Option<TypeId> {
        let ast::ExprKind::TypeArgs { base, args } = &expr::strip_parens(e).kind else { return None };
        let segs = Self::flatten_path(base)?;
        if self.lookup_local(&segs[0].name, false).is_some() {
            return None;
        }
        let (PathRes::Member(Member::Generic(t)), n) = self.resolve_segments(&segs, false)? else { return None };
        if n != segs.len() || !matches!(self.gen.templates[t as usize].decl, TmplDecl::Struct(_) | TmplDecl::Enum(_)) {
            return None;
        }
        let Some(targs) = self.type_args(t, args, e.span) else { return Some(self.tt.invalid) };
        Some(match self.instantiate(t, targs, e.span) {
            Some(Inst::Struct(s)) => self.struct_ty(s),
            Some(Inst::Enum(en)) => self.enum_ty(en),
            _ => self.tt.invalid,
        })
    }

    /// Resolves `name` as a member (variant or static method) of an
    /// instantiated type.
    fn type_member(&mut self, ty: TypeId, name: &ast::Ident) -> Option<PathRes> {
        match *self.tt.get(ty) {
            Type::Enum(e) => match self.enums[e.0 as usize].variants.iter().position(|v| v.name == name.name) {
                Some(v) => Some(PathRes::Variant(e.0, v as u32)),
                None => {
                    let en = self.enums[e.0 as usize].name.clone();
                    self.err(format!("enum {en} has no variant {}", name.name), name.span);
                    None
                }
            },
            Type::Struct(s) => {
                let found = self.structs[s.0 as usize].methods.iter().find(|(n, _)| *n == name.name).map(|(_, f)| *f);
                match found {
                    Some(f) if matches!(self.funcs[f.0 as usize].kind, hir::FuncKind::Method { is_static: true, .. }) => Some(PathRes::StaticMethod(f)),
                    Some(f) => Some(PathRes::MethodExpr(f, s.0)),
                    _ => match self.gen.methods.get(&(s.0, name.name.clone())) {
                        Some(t) => Some(PathRes::Member(Member::Generic(*t))),
                        None => {
                            let sn = self.structs[s.0 as usize].name.clone();
                            self.err(format!("{} is not a static method of {sn}", name.name), name.span);
                            None
                        }
                    },
                }
            }
            _ => None,
        }
    }

    /// `Option<int>.Some(1)`, `List<int>.New()`.
    pub(crate) fn type_member_call(&mut self, ty: TypeId, name: &ast::Ident, args: &[ast::Expr], span: Span) -> hir::Expr {
        match self.type_member(ty, name) {
            Some(PathRes::Member(Member::Generic(t))) => self.generic_call(t, None, None, args, span),
            Some(res) => self.call_res(res, name, args, span, None),
            None => {
                self.check_args_loose(args);
                self.invalid(span)
            }
        }
    }

    /// `Option<int>.None`.
    pub(crate) fn type_member_value(&mut self, ty: TypeId, name: &ast::Ident, span: Span) -> hir::Expr {
        match self.type_member(ty, name) {
            Some(PathRes::Member(Member::Generic(_))) => {
                self.err(format!("generic method {} must be called", name.name), name.span);
                self.invalid(span)
            }
            Some(res) => self.path_value(res, &name.name, span, None),
            None => self.invalid(span),
        }
    }

    /// The variant named by a switch pattern callee against enum `eid`:
    /// `E.V`, and for instances of a generic enum also `Option.Some` (the
    /// template name) and `Option<int>.Some`.
    pub(crate) fn pattern_variant(&mut self, callee: &ast::Expr, eid: u32) -> Option<(u32, u32)> {
        if let ast::ExprKind::Field { base, name } = &callee.kind {
            if let Some(ty) = self.type_args_type(base) {
                let Type::Enum(d) = *self.tt.get(ty) else { return None };
                let v = self.enums[d.0 as usize].variants.iter().position(|v| v.name == name.name)?;
                return Some((d.0, v as u32));
            }
        }
        let segs = Self::flatten_path(callee)?;
        if self.lookup_local(&segs[0].name, false).is_some() {
            return None;
        }
        let (res, n) = self.resolve_segments(&segs, false)?;
        match res {
            PathRes::Variant(en, v) if n == segs.len() => Some((en, v)),
            PathRes::Member(Member::Generic(t)) if n + 1 == segs.len() => {
                let (it, _) = self.gen.inst_enums.get(&eid)?;
                if *it != t {
                    return None;
                }
                let v = self.enums[eid as usize].variants.iter().position(|v| v.name == segs[n].name)?;
                Some((eid, v as u32))
            }
            _ => None,
        }
    }

    /// The struct of a struct literal `path<generics>{...}`; a generic
    /// struct without type arguments takes them from the expected type.
    pub(crate) fn struct_lit_struct(&mut self, path: &ast::Path, generics: &[ast::TypeExpr], expected: Option<TypeId>, span: Span) -> Option<u32> {
        let segs: Vec<&ast::Ident> = path.segments.iter().collect();
        // `t{...}` of an unrolled `TypesWith` loop.
        if generics.is_empty() && (segs.len() <= 2 || segs[1].name == "Type") {
            if let Some(ty) = self.ct_desc_type(path) {
                if let Type::Struct(d) = *self.tt.get(ty) {
                    return Some(d.0);
                }
                if !self.tt.is_invalid(ty) {
                    self.err(format!("{} is not a struct type", self.tn(ty)), span);
                }
                return None;
            }
        }
        if self.lookup_global(segs[0], false).is_none() {
            self.unknown_type(path);
            return None;
        }
        let (res, n) = self.resolve_segments(&segs, true)?;
        match res {
            PathRes::Member(Member::Struct(s)) if n == segs.len() => {
                if !generics.is_empty() {
                    self.err(format!("type {} is not generic (it takes no type arguments)", path.to_string()), span);
                }
                Some(s)
            }
            PathRes::Member(Member::Generic(t)) if n == segs.len() && matches!(self.gen.templates[t as usize].decl, TmplDecl::Struct(_)) => {
                if generics.is_empty() {
                    if let Some(s) = self.plain_std_struct(t, span) {
                        return Some(s);
                    }
                    if let Some(Type::Struct(d)) = expected.map(|e| self.tt.get(e).clone()) {
                        if self.gen.inst_structs.get(&d.0).is_some_and(|(it, _)| *it == t) {
                            return Some(d.0);
                        }
                    }
                    let name = self.gen.templates[t as usize].name.clone();
                    self.err(format!("missing type arguments for generic type {name} (write {name}<...>{{...}})"), path.span);
                    return None;
                }
                let args = self.type_args(t, generics, span)?;
                match self.instantiate(t, args, span)? {
                    Inst::Struct(s) => Some(s),
                    _ => None,
                }
            }
            _ => {
                self.err(format!("{} is not a struct type", path.to_string()), path.span);
                None
            }
        }
    }
}
