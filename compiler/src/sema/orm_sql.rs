//! Compile-time translation of `data` query lambdas to SQL
//! (docs/FRAMEWORK.md section 3; DESIGN.md "Перевод лямбд в SQL").
//!
//! A lambda passed to `Where`, `OrderBy`, `Select`, `Sum`, ... is never
//! compiled as a closure: its body is walked here, with each parameter
//! bound to a table alias (`t0` for the query's entity, `sN` for the
//! entities of correlated subqueries, a group for `GroupBy`). Member
//! accesses become columns, operators and the supported calls become SQL
//! for both dialects (PostgreSQL and SQLite, [`S::D`]), and every
//! subexpression that does not mention a lambda parameter (captured
//! variables, calls, `time.Now()`) is evaluated once, in the enclosing
//! scope, as a statement argument (`{?i}` in the fragment, `{*i}` for a
//! list). Anything else is a compile error `expression cannot be
//! translated to SQL: ...` with a hint to evaluate it in memory after
//! `AsEnumerable()`.

use super::orm_model::{ColModel, EntModel, NavKind, NavModel};
use super::*;
use std::rc::Rc;

/// SQL text for both dialects with argument markers.
#[derive(Clone, Debug)]
pub(crate) enum S {
    T(String),
    /// `{?i}`: argument i.
    Arg(usize),
    /// `{*i}`: list argument i (expanded to one placeholder per element).
    List(usize),
    Seq(Vec<S>),
    /// (PostgreSQL, SQLite)
    D(Box<S>, Box<S>),
}

impl S {
    pub fn t(s: impl Into<String>) -> S {
        S::T(s.into())
    }

    pub fn d(pg: S, lite: S) -> S {
        S::D(Box::new(pg), Box::new(lite))
    }

    pub fn render(&self, pg: bool) -> String {
        let mut out = String::new();
        self.write(pg, &mut out);
        out
    }

    fn write(&self, pg: bool, out: &mut String) {
        match self {
            S::T(s) => out.push_str(s),
            S::Arg(i) => out.push_str(&format!("{{?{i}}}")),
            S::List(i) => out.push_str(&format!("{{*{i}}}")),
            S::Seq(xs) => xs.iter().for_each(|x| x.write(pg, out)),
            S::D(a, b) => (if pg { a } else { b }).write(pg, out),
        }
    }
}

/// `S::Seq` of the parts.
macro_rules! seq {
    ($($x:expr),* $(,)?) => { S::Seq(vec![$(S::from($x)),*]) };
}
pub(crate) use seq;

impl From<&str> for S {
    fn from(s: &str) -> S {
        S::T(s.to_string())
    }
}

impl From<String> for S {
    fn from(s: String) -> S {
        S::T(s)
    }
}

impl From<&S> for S {
    fn from(s: &S) -> S {
        s.clone()
    }
}

/// Untyped literal kinds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Lit {
    No,
    Int,
    Float,
    Str,
    Bool,
    Nil,
}

/// What a translated expression is.
#[derive(Clone, Debug)]
pub(crate) enum VK {
    Scalar,
    /// A lambda parameter standing for an entity row.
    Ent(Rc<EntModel>, String),
    /// A collection navigation of the entity at the alias.
    Coll(NavModel, Rc<EntModel>, String),
    /// A reference navigation of the entity at the alias.
    Ref(NavModel, Rc<EntModel>, String),
    /// A group (scope index).
    Group(usize),
    /// A composite group key.
    Tuple(Vec<V>),
}

/// A translated expression.
#[derive(Clone, Debug)]
pub(crate) struct V {
    pub s: S,
    /// `None` for untyped literals.
    pub ty: Option<TypeId>,
    pub lit: Lit,
    pub kind: VK,
}

impl V {
    fn scalar(s: S, ty: TypeId) -> V {
        V { s, ty: Some(ty), lit: Lit::No, kind: VK::Scalar }
    }
}

/// What a lambda parameter is bound to.
#[derive(Clone, Debug)]
pub(crate) enum Bind {
    Ent(Rc<EntModel>, String),
    /// GROUP BY: key values, their type, the grouped entity at an alias.
    Group(Vec<V>, TypeId, Rc<EntModel>, String),
}

/// Translation context: parameter scopes and the arguments collected.
pub(crate) struct Cx {
    pub scopes: Vec<(String, Bind)>,
    pub args: Vec<hir::Expr>,
    pub sub: usize,
}

impl Cx {
    pub fn new() -> Cx {
        Cx { scopes: Vec::new(), args: Vec::new(), sub: 0 }
    }

    fn names(&self) -> Vec<String> {
        self.scopes.iter().map(|s| s.0.clone()).collect()
    }

    fn lookup(&self, n: &str) -> Option<(usize, Bind)> {
        self.scopes.iter().enumerate().rev().find(|(_, s)| s.0 == n).map(|(i, s)| (i, s.1.clone()))
    }
}

/// `Err(Some((why, span)))`: not translatable; `Err(None)`: already reported.
pub(crate) type Tr<T> = Result<T, Option<(String, Span)>>;

fn no<T>(why: impl Into<String>, span: Span) -> Tr<T> {
    Err(Some((why.into(), span)))
}

/// Whether `e` mentions one of `names` (free identifiers).
pub(crate) fn mentions(e: &ast::Expr, names: &[String]) -> bool {
    use ast::ExprKind as K;
    match &e.kind {
        K::Ident(id) => names.contains(&id.name),
        K::Field { base, .. } => mentions(base, names),
        K::Call { callee, args } => mentions(callee, names) || args.iter().any(|a| mentions(a, names)),
        K::Index { base, index } => mentions(base, names) || mentions(index, names),
        K::Unary { expr, .. } | K::Await(expr) | K::Paren(expr) | K::New(expr) => mentions(expr, names),
        K::Binary { left, right, .. } => mentions(left, names) || mentions(right, names),
        K::Propagate { expr, .. } => mentions(expr, names),
        K::Try { expr, .. } => mentions(expr, names),
        K::StructLit { fields, .. } => fields.iter().any(|f| mentions(&f.value, names)),
        K::TypeArgs { base, .. } => mentions(base, names),
        K::SliceLit(xs) | K::Tuple(xs) => xs.iter().any(|x| mentions(x, names)),
        K::MapLit { entries, .. } => entries.iter().any(|(k, v)| mentions(k, names) || mentions(v, names)),
        K::CompositeLit { elems, .. } => elems.iter().any(|(k, v)| k.as_ref().is_some_and(|k| mentions(k, names)) || mentions(v, names)),
        K::Range(a, b) => a.as_ref().is_some_and(|x| mentions(x, names)) || b.as_ref().is_some_and(|x| mentions(x, names)),
        K::Lambda { params, body, .. } => {
            let inner: Vec<String> = names.iter().filter(|n| !params.iter().any(|p| p.name.name == **n)).cloned().collect();
            match &**body {
                ast::LambdaBody::Expr(x) => mentions(x, &inner),
                // statements are not translated anyway
                ast::LambdaBody::Block(_) => !inner.is_empty(),
            }
        }
        K::NamedArg { value, .. } => mentions(value, names),
        _ => false,
    }
}

/// The body expression of a lambda (`=> e` or `{ return e }`).
pub(crate) fn lambda_body(body: &ast::LambdaBody) -> Option<&ast::Expr> {
    match body {
        ast::LambdaBody::Expr(x) => Some(x),
        ast::LambdaBody::Block(b) => match b.stmts.as_slice() {
            [ast::Stmt { kind: ast::StmtKind::Return(v), .. }] if v.len() == 1 => Some(&v[0]),
            _ => None,
        },
    }
}

impl<'a> Sema<'a> {
    /// Reports an untranslatable expression.
    pub(crate) fn orm_report(&mut self, e: Option<(String, Span)>) {
        if let Some((why, span)) = e {
            self.err_help(
                format!("expression cannot be translated to SQL: {why}"),
                span,
                "query lambdas are translated at compile time; to evaluate it in memory, load the rows first with .AsEnumerable() (or ToList) and filter them with ordinary code",
            );
        }
    }

    /// Checks that `arg` is a lambda literal with `n` parameters and binds
    /// them; returns the body.
    pub(crate) fn orm_lambda<'e>(&mut self, arg: &'e ast::Expr, binds: Vec<Bind>, what: &str, cx: &mut Cx) -> Tr<&'e ast::Expr> {
        let ast::ExprKind::Lambda { params, body, is_async, .. } = &expr::strip_parens(arg).kind else {
            // evaluated for its diagnostics
            let v = self.value(arg, None);
            if self.tt.is_invalid(v.ty) {
                return Err(None);
            }
            return no(format!("the argument of {what} must be a lambda literal such as (u) => u.Name (a function value cannot be inspected)"), arg.span);
        };
        if *is_async {
            return no(format!("the lambda of {what} cannot be async"), arg.span);
        }
        if params.len() != binds.len() {
            return no(format!("the lambda of {what} must take {} parameter{}", binds.len(), if binds.len() == 1 { "" } else { "s" }), arg.span);
        }
        for (p, b) in params.iter().zip(binds) {
            if !matches!(p.ty.kind, ast::TypeKind::Infer) {
                let t = self.resolve_type(&p.ty);
                if self.tt.is_invalid(t) {
                    return Err(None);
                }
                if let Bind::Ent(m, _) = &b {
                    let ptr = self.tt.pointer(m.ty);
                    if t != m.ty && t != ptr {
                        let (a, w) = (self.tn(t), self.tn(m.ty));
                        return no(format!("the parameter {} of the lambda of {what} has type {a}, the query is over {w}", p.name.name), p.span);
                    }
                }
            }
            cx.scopes.push((p.name.name.clone(), b));
        }
        match lambda_body(body) {
            Some(x) => Ok(x),
            None => no(format!("the lambda of {what} must be a single expression (`=> expr` or `{{ return expr }}`)"), arg.span),
        }
    }

    // -- types ---------------------------------------------------------------------

    pub(crate) fn orm_base(&self, t: TypeId) -> TypeId {
        match self.orm_scalar(t) {
            Some((_, b, _)) => b,
            None => t,
        }
    }

    fn orm_is_num(&self, t: TypeId) -> bool {
        matches!(self.tt.get(self.orm_base(t)), Type::Int(_) | Type::Float32 | Type::Float64)
    }

    fn orm_is_float(&self, t: TypeId) -> bool {
        matches!(self.tt.get(self.orm_base(t)), Type::Float32 | Type::Float64)
    }

    fn orm_is(&self, t: TypeId, k: &str) -> bool {
        self.orm_scalar(t).is_some_and(|x| x.0 == k)
    }

    fn orm_nullable(&self, t: TypeId) -> bool {
        self.orm_scalar(t).is_some_and(|x| x.2) || matches!(self.tt.get(t), Type::Pointer(_))
    }

    /// The type of a value, defaulting literals.
    pub(crate) fn orm_ty(&self, v: &V) -> TypeId {
        match (v.ty, v.lit) {
            (Some(t), _) => t,
            (None, Lit::Float) => self.tt.float,
            (None, Lit::Str) => self.tt.string,
            (None, Lit::Bool) => self.tt.bool_,
            _ => self.tt.int,
        }
    }

    fn orm_expect_bool(&self, v: &V, span: Span) -> Tr<()> {
        let ok = match v.ty {
            Some(t) => matches!(self.tt.get(self.orm_base(t)), Type::Bool),
            None => v.lit == Lit::Bool,
        };
        if ok {
            Ok(())
        } else {
            no(format!("expected a condition (bool), found {}", self.orm_desc(v)), span)
        }
    }

    fn orm_desc(&self, v: &V) -> String {
        match (&v.kind, v.ty) {
            (VK::Ent(m, _), _) => format!("the entity {}", m.name),
            (VK::Coll(n, _, _), _) | (VK::Ref(n, _, _), _) => format!("the navigation {}", n.field),
            (VK::Group(_), _) => "a group".into(),
            (VK::Tuple(_), _) => "a composite key".into(),
            (_, Some(t)) => self.tn(t),
            (_, None) => match v.lit {
                Lit::Int => "an integer constant".into(),
                Lit::Float => "a float constant".into(),
                Lit::Str => "a string".into(),
                Lit::Bool => "a bool constant".into(),
                Lit::Nil => "nil".into(),
                Lit::No => "a value".into(),
            },
        }
    }

    /// Checks that a value is a scalar (not an entity, navigation or group).
    pub(crate) fn orm_scalar_v(&self, v: V, span: Span) -> Tr<V> {
        match &v.kind {
            VK::Scalar => Ok(v),
            VK::Ent(m, _) => no(format!("the entity {} itself is not a value (use one of its columns)", m.name), span),
            VK::Coll(n, _, _) => no(format!("the collection {} is not a value (use .Any(), .Count(), .Sum(...) or len())", n.field), span),
            VK::Ref(n, _, _) => no(format!("the navigation {} is not a value (use one of its columns, or compare it with nil)", n.field), span),
            VK::Group(_) => no("a group is not a value (use g.Key, g.Count(), g.Sum(...))", span),
            VK::Tuple(_) => no("a composite key can only be selected (use one of its columns elsewhere)", span),
        }
    }

    /// Result type of an arithmetic or comparison between `l` and `r`;
    /// reports mismatches.
    pub(crate) fn orm_unify(&self, l: &V, r: &V, span: Span) -> Tr<Option<TypeId>> {
        match (l.ty, r.ty) {
            (Some(a), Some(b)) => {
                let (ba, bb) = (self.orm_base(a), self.orm_base(b));
                if ba == bb {
                    return Ok(Some(ba));
                }
                no(format!("mismatched types {} and {}", self.tn(a), self.tn(b)), span)
            }
            (Some(a), None) | (None, Some(a)) => {
                let lit = if l.ty.is_none() { l.lit } else { r.lit };
                let b = self.orm_base(a);
                let ok = match lit {
                    Lit::Int => self.orm_is_num(b),
                    Lit::Float => self.orm_is_float(b),
                    Lit::Str => matches!(self.tt.get(b), Type::String),
                    Lit::Bool => matches!(self.tt.get(b), Type::Bool),
                    Lit::Nil => self.orm_nullable(a),
                    Lit::No => true,
                };
                if ok {
                    Ok(Some(b))
                } else {
                    let d = if l.ty.is_none() { self.orm_desc(l) } else { self.orm_desc(r) };
                    no(format!("mismatched types {} and {d}", self.tn(a)), span)
                }
            }
            (None, None) => Ok(None),
        }
    }

    // -- arguments -------------------------------------------------------------------

    /// A HIR call of stdlib generic function `ns.name` instantiated with `tys`.
    pub(crate) fn orm_std_call(&mut self, ns: &str, name: &str, tys: Vec<TypeId>, args: Vec<hir::Expr>, span: Span) -> Option<hir::Expr> {
        let nsi = *self.ns_index.get(ns)?;
        let Some((Member::Generic(t), _, _)) = self.nss[nsi].members.get(name).cloned() else { return None };
        let inst = if tys.is_empty() { self.plain_std_inst(t, span)? } else { self.instantiate(t, tys, span)? };
        let generics::Inst::Func(fid) = inst else { return None };
        let sig = self.sigs[fid.0 as usize].clone();
        let fname = self.funcs[fid.0 as usize].name.clone();
        let args: Vec<hir::Expr> = args.into_iter().zip(sig.params.iter()).enumerate().map(|(i, (a, p))| self.coerce(a, *p, super::coerce::Ctx::Arg(i, fname.clone()))).collect();
        let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
        Some(mk(hir::ExprKind::Call { callee: hir::Callee::Func(fid), args }, ty, span))
    }

    /// Adds an argument (its driver value, `sql.argOf`) and returns its marker.
    fn orm_arg(&mut self, v: hir::Expr, cx: &mut Cx) -> Tr<S> {
        let span = v.span;
        let ty = v.ty;
        let Some(a) = self.orm_std_call("database.sql", "argOf", vec![ty], vec![v], span) else { return Err(None) };
        cx.args.push(a);
        Ok(S::Arg(cx.args.len() - 1))
    }

    /// A captured subexpression: evaluated in the enclosing scope.
    fn orm_captured(&mut self, e: &ast::Expr, cx: &mut Cx) -> Tr<V> {
        use ast::ExprKind as K;
        let lit = |s: S, l: Lit| V { s, ty: None, lit: l, kind: VK::Scalar };
        match &expr::strip_parens(e).kind {
            K::Nil => return Ok(lit(S::t("NULL"), Lit::Nil)),
            K::Bool(b) => return Ok(lit(S::t(if *b { "TRUE" } else { "FALSE" }), Lit::Bool)),
            K::Int(n) => return Ok(lit(S::t(n.to_string()), Lit::Int)),
            K::Float(f) => return Ok(lit(S::t(format!("{f:?}")), Lit::Float)),
            _ => {}
        }
        if let Some(f) = self.fold(e) {
            if f.ty.is_none() {
                match f.value {
                    hir::ConstValue::Int(n) => return Ok(lit(S::t(n.to_string()), Lit::Int)),
                    hir::ConstValue::Float(x) => return Ok(lit(S::t(format!("{x:?}")), Lit::Float)),
                    hir::ConstValue::Bool(b) => return Ok(lit(S::t(if b { "TRUE" } else { "FALSE" }), Lit::Bool)),
                    _ => {}
                }
            }
        }
        let v = self.value(e, None);
        let v = self.default_type(v);
        if self.tt.is_invalid(v.ty) {
            return Err(None);
        }
        let ty = v.ty;
        if let Type::Slice(el) = *self.tt.get(ty) {
            if self.orm_scalar(ty).is_none() {
                let span = v.span;
                let Some(a) = self.orm_std_call("data", "listArg", vec![el], vec![v], span) else { return Err(None) };
                cx.args.push(a);
                return Ok(V { s: S::List(cx.args.len() - 1), ty: Some(ty), lit: Lit::No, kind: VK::Scalar });
            }
        }
        if self.orm_scalar(ty).is_none() && !matches!(self.tt.get(ty), Type::Any) {
            let tn = self.tn(ty);
            return no(format!("a value of type {tn} cannot be a SQL parameter"), e.span);
        }
        // plain enums are stored by variant name
        let v = if matches!(self.tt.get(ty), Type::Enum(_)) {
            let span = v.span;
            mk(hir::ExprKind::ToString(Box::new(v)), self.tt.string, span)
        } else {
            v
        };
        let s = self.orm_arg(v, cx)?;
        Ok(V { s, ty: Some(ty), lit: Lit::No, kind: VK::Scalar })
    }

    // -- expressions ---------------------------------------------------------------------

    /// Translates `e` (a predicate: must be bool).
    pub(crate) fn orm_pred(&mut self, e: &ast::Expr, cx: &mut Cx) -> Tr<S> {
        let v = self.orm_tr(e, cx)?;
        let v = self.orm_scalar_v(v, e.span)?;
        self.orm_expect_bool(&v, e.span)?;
        Ok(v.s)
    }

    /// Translates a scalar expression.
    pub(crate) fn orm_value(&mut self, e: &ast::Expr, cx: &mut Cx) -> Tr<V> {
        let v = self.orm_tr(e, cx)?;
        self.orm_scalar_v(v, e.span)
    }

    pub(crate) fn orm_tr(&mut self, e: &ast::Expr, cx: &mut Cx) -> Tr<V> {
        use ast::ExprKind as K;
        let e = expr::strip_parens(e);
        let span = e.span;
        if !mentions(e, &cx.names()) {
            return self.orm_captured(e, cx);
        }
        match &e.kind {
            K::Ident(id) => match cx.lookup(&id.name) {
                Some((_, Bind::Ent(m, a))) => Ok(V { s: S::t(a.clone()), ty: Some(m.ty), lit: Lit::No, kind: VK::Ent(m, a) }),
                Some((i, Bind::Group(..))) => Ok(V { s: S::t(""), ty: None, lit: Lit::No, kind: VK::Group(i) }),
                None => no(format!("unknown name {}", id.name), span),
            },
            K::Field { base, name } => {
                let b = self.orm_tr(base, cx)?;
                self.orm_member(b, name, span, cx)
            }
            K::Unary { op, expr: x } => {
                let v = self.orm_value(x, cx)?;
                match op {
                    ast::UnaryOp::Not => {
                        self.orm_expect_bool(&v, x.span)?;
                        Ok(V::scalar(seq!["(NOT ", v.s, ")"], self.tt.bool_))
                    }
                    ast::UnaryOp::Neg => {
                        if !v.ty.is_none_or(|t| self.orm_is_num(t)) {
                            return no(format!("cannot negate {}", self.orm_desc(&v)), span);
                        }
                        Ok(V { s: seq!["(-", v.s, ")"], ..v })
                    }
                    // `*u.Age` on a nullable column: the column itself
                    ast::UnaryOp::Deref => {
                        let t = v.ty.map(|t| self.orm_base(t));
                        Ok(V { ty: t, ..v })
                    }
                    ast::UnaryOp::AddrOf => no("& is not supported in queries", span),
                }
            }
            K::Binary { op, left, right } => self.orm_binary(*op, left, right, span, cx),
            K::Call { callee, args } => self.orm_tr_call(callee, args, span, cx),
            K::Index { .. } => no("indexing is not supported in queries", span),
            K::Tuple(_) => no("a tuple is only allowed as the result of Select, OrderBy, GroupBy or a key", span),
            K::StructLit { .. } => no("a struct literal is only allowed as the result of Select", span),
            K::Lambda { .. } => no("a nested lambda is only allowed as the argument of Any, All, Count, Sum, Min, Max or Average", span),
            K::Await(_) => no("await is not allowed in a query", span),
            K::Propagate { .. } | K::Try { .. } => no("error handling (?, try) is not allowed in a query", span),
            _ => no("this kind of expression is not supported", span),
        }
    }

    /// `base.name`.
    fn orm_member(&mut self, b: V, name: &ast::Ident, span: Span, cx: &mut Cx) -> Tr<V> {
        match b.kind.clone() {
            VK::Ent(m, a) => self.orm_column(&m, &a, name, span),
            VK::Ref(n, owner, a) => {
                // a column of the referenced entity: a scalar subquery
                let Some(target) = self.orm_model(n.target) else { return Err(None) };
                let alias = self.orm_alias(cx);
                let col = self.orm_column(&target, &alias, name, span)?;
                if !matches!(col.kind, VK::Scalar) {
                    return no(format!("{}.{}: only columns of a reference navigation can be used (not nested navigations)", n.field, name.name), span);
                }
                let corr = self.orm_correlate(&n, &owner, &a, &target, &alias, span)?;
                let ty = col.ty.map(|t| if self.orm_nullable(t) { t } else { self.tt.pointer(t) });
                Ok(V { s: seq!["(SELECT ", col.s, " FROM ", corr, ")"], ty, lit: Lit::No, kind: VK::Scalar })
            }
            VK::Group(i) => {
                if name.name != "Key" {
                    return no(format!("a group has no member {} (use g.Key, g.Count(), g.Sum(...), g.Min, g.Max, g.Average)", name.name), span);
                }
                let Bind::Group(keys, kt, _, _) = cx.scopes[i].1.clone() else { unreachable!() };
                if keys.len() == 1 {
                    return Ok(keys[0].clone());
                }
                Ok(V { s: S::t(""), ty: Some(kt), lit: Lit::No, kind: VK::Tuple(keys) })
            }
            VK::Scalar => {
                let d = self.orm_desc(&b);
                no(format!("{d} has no member {} in SQL", name.name), span)
            }
            VK::Coll(n, ..) => no(format!("the collection {} has no member {} (use Any, All, Count, Sum, Min, Max, Average)", n.field, name.name), span),
            VK::Tuple(_) => no("members of a composite key cannot be used in SQL", span),
        }
    }

    fn orm_alias(&mut self, cx: &mut Cx) -> String {
        cx.sub += 1;
        format!("s{}", cx.sub)
    }

    /// The column (or navigation) `name` of entity `m` at alias `a`.
    pub(crate) fn orm_column(&mut self, m: &Rc<EntModel>, a: &str, name: &ast::Ident, span: Span) -> Tr<V> {
        if let Some(c) = m.col(&name.name) {
            return Ok(V::scalar(S::t(format!("{a}.{}", quote(&c.name))), c.ty));
        }
        if let Some(n) = m.nav(&name.name) {
            let n = n.clone();
            return Ok(V { s: S::t(""), ty: None, lit: Lit::No, kind: if n.many { VK::Coll(n, m.clone(), a.to_string()) } else { VK::Ref(n, m.clone(), a.to_string()) } });
        }
        let Some(sid) = self.struct_of(m.ty) else { return Err(None) };
        if self.structs[sid as usize].fields.iter().any(|f| f.name == name.name) {
            return no(format!("{}.{} is not mapped to a column (@ignore)", m.name, name.name), name.span);
        }
        no(format!("{} has no field {}", m.name, name.name), span)
    }

    /// `"target" AS alias WHERE <correlation with the owner at owner_alias>`
    /// (the FROM and WHERE of a correlated subquery over navigation `n`).
    fn orm_correlate(&mut self, n: &NavModel, owner: &Rc<EntModel>, oa: &str, target: &Rc<EntModel>, ta: &str, span: Span) -> Tr<S> {
        let key_of = |m: &EntModel| -> Option<ColModel> { m.key_col().cloned() };
        let s = match n.kind {
            NavKind::Many | NavKind::One => {
                let (Some(fk), Some(pk)) = (target.col(&n.fk).cloned(), key_of(owner)) else { return no("the navigation has no foreign key", span) };
                format!("{} AS {ta} WHERE {ta}.{} = {oa}.{}", quote(&target.table), quote(&fk.name), quote(&pk.name))
            }
            NavKind::Belongs => {
                let (Some(fk), Some(pk)) = (owner.col(&n.fk).cloned(), key_of(target)) else { return no("the navigation has no foreign key", span) };
                format!("{} AS {ta} WHERE {ta}.{} = {oa}.{}", quote(&target.table), quote(&pk.name), quote(&fk.name))
            }
            NavKind::ManyMany => {
                let (Some(ok), Some(tk)) = (key_of(owner), key_of(target)) else { return no("the navigation needs keys", span) };
                let j = format!("j{}", &ta[1..]);
                format!(
                    "{} AS {ta} JOIN {} AS {j} ON {j}.{} = {ta}.{} WHERE {j}.{} = {oa}.{}",
                    quote(&target.table),
                    quote(&n.through),
                    quote(&n.target_col),
                    quote(&tk.name),
                    quote(&n.owner_col),
                    quote(&ok.name)
                )
            }
        };
        Ok(S::t(s))
    }

    fn orm_binary(&mut self, op: ast::BinaryOp, left: &ast::Expr, right: &ast::Expr, span: Span, cx: &mut Cx) -> Tr<V> {
        use ast::BinaryOp as B;
        let boolt = self.tt.bool_;
        match op {
            B::And | B::Or => {
                let l = self.orm_pred(left, cx)?;
                let r = self.orm_pred(right, cx)?;
                let kw = if op == B::And { " AND " } else { " OR " };
                Ok(V::scalar(seq!["(", l, kw, r, ")"], boolt))
            }
            B::Eq | B::Ne => {
                let l = self.orm_tr(left, cx)?;
                let r = self.orm_tr(right, cx)?;
                let (x, other) = if r.lit == Lit::Nil { (l.clone(), r.clone()) } else { (r.clone(), l.clone()) };
                if other.lit == Lit::Nil || x.lit == Lit::Nil {
                    let x = if x.lit == Lit::Nil { other } else { x };
                    let not = if op == B::Ne { " NOT" } else { "" };
                    return match &x.kind {
                        VK::Ref(n, owner, a) => {
                            if n.kind == NavKind::Belongs {
                                let Some(fk) = owner.col(&n.fk).cloned() else { return Err(None) };
                                if !fk.nullable {
                                    return no(format!("{} is never nil (its foreign key {} is not nullable)", n.field, fk.field), span);
                                }
                                Ok(V::scalar(S::t(format!("({a}.{} IS{not} NULL)", quote(&fk.name))), boolt))
                            } else {
                                let Some(target) = self.orm_model(n.target) else { return Err(None) };
                                let alias = self.orm_alias(cx);
                                let corr = self.orm_correlate(&n.clone(), &owner.clone(), &a.clone(), &target, &alias, span)?;
                                let kw = if op == B::Ne { "EXISTS" } else { "NOT EXISTS" };
                                Ok(V::scalar(seq!["(", kw, " (SELECT 1 FROM ", corr, "))"], boolt))
                            }
                        }
                        VK::Scalar => {
                            if let Some(t) = x.ty {
                                if !self.orm_nullable(t) {
                                    return no(format!("a value of type {} is never nil", self.tn(t)), span);
                                }
                            }
                            Ok(V::scalar(seq!["(", x.s, " IS", not, " NULL)"], boolt))
                        }
                        _ => no(format!("{} cannot be compared with nil", self.orm_desc(&x)), span),
                    };
                }
                let l = self.orm_scalar_v(l, left.span)?;
                let r = self.orm_scalar_v(r, right.span)?;
                self.orm_unify(&l, &r, span)?;
                let o = if op == B::Eq { " = " } else { " <> " };
                Ok(V::scalar(seq!["(", l.s, o, r.s, ")"], boolt))
            }
            B::Lt | B::Le | B::Gt | B::Ge => {
                let l = self.orm_value(left, cx)?;
                let r = self.orm_value(right, cx)?;
                let t = self.orm_unify(&l, &r, span)?;
                if let Some(t) = t {
                    if !(self.orm_is_num(t) || matches!(self.tt.get(t), Type::String) || self.orm_is(t, "time") || self.orm_is(t, "decimal")) {
                        return no(format!("values of type {} cannot be ordered", self.tn(t)), span);
                    }
                }
                let o = match op {
                    B::Lt => " < ",
                    B::Le => " <= ",
                    B::Gt => " > ",
                    _ => " >= ",
                };
                Ok(V::scalar(seq!["(", l.s, o, r.s, ")"], boolt))
            }
            B::Add | B::Sub | B::Mul | B::Div | B::Rem => {
                let l = self.orm_value(left, cx)?;
                let r = self.orm_value(right, cx)?;
                let t = self.orm_unify(&l, &r, span)?;
                let is_str = t.is_some_and(|t| matches!(self.tt.get(t), Type::String)) || (t.is_none() && l.lit == Lit::Str);
                if op == B::Add && is_str {
                    return Ok(V { s: seq!["(", l.s, " || ", r.s, ")"], ty: t.or(Some(self.tt.string)), lit: Lit::No, kind: VK::Scalar });
                }
                if let Some(t) = t {
                    if !self.orm_is_num(t) && !self.orm_is(t, "decimal") {
                        return no(format!("operator {} is not defined on {}", expr::op_symbol(op), self.tn(t)), span);
                    }
                    if op == B::Rem && self.orm_is_float(t) {
                        return no("% is only defined on integers", span);
                    }
                }
                let o = match op {
                    B::Add => " + ",
                    B::Sub => " - ",
                    B::Mul => " * ",
                    B::Div => " / ",
                    _ => " % ",
                };
                let lit = if t.is_none() { if l.lit == Lit::Float || r.lit == Lit::Float { Lit::Float } else { Lit::Int } } else { Lit::No };
                Ok(V { s: seq!["(", l.s, o, r.s, ")"], ty: t, lit, kind: VK::Scalar })
            }
            _ => no(format!("operator {} is not supported in queries", expr::op_symbol(op)), span),
        }
    }

    /// Calls: string methods, collection aggregates, `len`, conversions,
    /// `strings.X`, `slices.Contains`.
    fn orm_tr_call(&mut self, callee: &ast::Expr, args: &[ast::Expr], span: Span, cx: &mut Cx) -> Tr<V> {
        let callee = expr::strip_parens(callee);
        let names = cx.names();
        match &callee.kind {
            ast::ExprKind::Ident(id) => match id.name.as_str() {
                "len" if args.len() == 1 => {
                    let v = self.orm_tr(&args[0], cx)?;
                    match &v.kind {
                        VK::Coll(..) => self.orm_coll_op(v, "Count", &[], span, cx),
                        VK::Scalar if v.ty.is_some_and(|t| matches!(self.tt.get(self.orm_base(t)), Type::String)) => {
                            let int = self.tt.int;
                            Ok(V::scalar(S::d(seq!["char_length(", &v.s, ")"], seq!["length(", &v.s, ")"]), int))
                        }
                        _ => no(format!("len of {} is not supported", self.orm_desc(&v)), span),
                    }
                }
                "float" | "float64" | "int" | "int64" | "int32" if args.len() == 1 => {
                    let v = self.orm_value(&args[0], cx)?;
                    if !v.ty.is_none_or(|t| self.orm_is_num(t)) {
                        return no(format!("cannot convert {} to {}", self.orm_desc(&v), id.name), span);
                    }
                    let (ty, pg, lite) = match id.name.as_str() {
                        "float" | "float64" => (self.tt.float, "double precision", "REAL"),
                        "int32" => (self.tt.builtin("int32").unwrap_or(self.tt.int), "integer", "INTEGER"),
                        _ => (self.tt.int, "bigint", "INTEGER"),
                    };
                    Ok(V::scalar(S::d(seq!["CAST(", &v.s, " AS ", pg, ")"], seq!["CAST(", &v.s, " AS ", lite, ")"]), ty))
                }
                n => no(format!("function {n} cannot be translated"), callee.span),
            },
            ast::ExprKind::Field { base, name } => {
                // namespace functions: strings.Contains(x, "a"), slices.Contains(ids, u.Id)
                if !mentions(base, &names) {
                    if let Some(segs) = Self::flatten_path(base) {
                        let head_local = self.lookup_local(&segs[0].name, false).is_some();
                        if !head_local {
                            let ns = segs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(".");
                            return self.orm_ns_call(&ns, &name.name, args, span, cx);
                        }
                    }
                    // a captured slice: ids.Contains(u.Id)
                    if name.name == "Contains" && args.len() == 1 {
                        let list = self.orm_captured(base, cx)?;
                        if matches!(list.s, S::List(_)) {
                            let x = self.orm_value(&args[0], cx)?;
                            return Ok(V::scalar(seq!["(", x.s, " IN (", list.s, "))"], self.tt.bool_));
                        }
                    }
                }
                let recv = self.orm_tr(base, cx)?;
                match &recv.kind {
                    VK::Coll(..) => self.orm_coll_op(recv, &name.name, args, span, cx),
                    VK::Group(i) => {
                        let i = *i;
                        self.orm_group_op(i, &name.name, args, span, cx)
                    }
                    VK::Scalar => self.orm_scalar_method(recv, &name.name, args, span, cx),
                    _ => no(format!("{} has no method {} in SQL", self.orm_desc(&recv), name.name), span),
                }
            }
            _ => no("this call cannot be translated", span),
        }
    }

    /// String functions shared by `x.M(...)` and `strings.M(x, ...)`.
    fn orm_string_fn(&mut self, x: V, m: &str, args: &[ast::Expr], span: Span, cx: &mut Cx) -> Tr<V> {
        if !x.ty.is_some_and(|t| matches!(self.tt.get(self.orm_base(t)), Type::String)) && x.lit != Lit::Str {
            return no(format!("{m} needs a string, found {}", self.orm_desc(&x)), span);
        }
        let (boolt, string) = (self.tt.bool_, self.tt.string);
        let arg = |s: &mut Self, cx: &mut Cx| -> Tr<V> {
            if args.len() != 1 {
                return no(format!("{m} takes one argument"), span);
            }
            let a = s.orm_value(&args[0], cx)?;
            if !a.ty.is_some_and(|t| matches!(s.tt.get(s.orm_base(t)), Type::String)) && a.lit != Lit::Str {
                return no(format!("the argument of {m} must be a string, found {}", s.orm_desc(&a)), args[0].span);
            }
            Ok(a)
        };
        match m {
            "Contains" => {
                let a = arg(self, cx)?;
                Ok(V::scalar(S::d(seq!["(strpos(", &x.s, ", ", &a.s, ") > 0)"], seq!["(instr(", &x.s, ", ", &a.s, ") > 0)"]), boolt))
            }
            "StartsWith" | "HasPrefix" => {
                let a = arg(self, cx)?;
                Ok(V::scalar(S::d(seq!["starts_with(", &x.s, ", ", &a.s, ")"], seq!["(substr(", &x.s, ", 1, length(", &a.s, ")) = ", &a.s, ")"]), boolt))
            }
            "EndsWith" | "HasSuffix" => {
                let a = arg(self, cx)?;
                Ok(V::scalar(
                    S::d(seq!["(right(", &x.s, ", length(", &a.s, ")) = ", &a.s, ")"], seq!["(length(", &a.s, ") = 0 OR substr(", &x.s, ", -length(", &a.s, ")) = ", &a.s, ")"]),
                    boolt,
                ))
            }
            "ToUpper" | "ToLower" | "Trim" | "TrimSpace" => {
                if !args.is_empty() {
                    return no(format!("{m} takes no arguments here"), span);
                }
                let f = match m {
                    "ToUpper" => "upper(",
                    "ToLower" => "lower(",
                    _ => "trim(",
                };
                Ok(V::scalar(seq![f, &x.s, ")"], string))
            }
            "Length" | "Len" if args.is_empty() => {
                let int = self.tt.int;
                Ok(V::scalar(S::d(seq!["char_length(", &x.s, ")"], seq!["length(", &x.s, ")"]), int))
            }
            _ => no(format!("string method {m} is not supported (Contains, StartsWith, EndsWith, ToUpper, ToLower, Trim, len)"), span),
        }
    }

    fn orm_scalar_method(&mut self, x: V, m: &str, args: &[ast::Expr], span: Span, cx: &mut Cx) -> Tr<V> {
        let t = x.ty.map(|t| self.orm_base(t));
        if t.is_some_and(|t| self.orm_is(t, "time")) {
            let boolt = self.tt.bool_;
            let o = match m {
                "Before" => " < ",
                "After" => " > ",
                "Equal" => " = ",
                _ => return no(format!("time method {m} is not supported (Before, After, Equal, comparison operators)"), span),
            };
            if args.len() != 1 {
                return no(format!("{m} takes one argument"), span);
            }
            let a = self.orm_value(&args[0], cx)?;
            self.orm_unify(&x, &a, span)?;
            return Ok(V::scalar(seq!["(", x.s, o, a.s, ")"], boolt));
        }
        self.orm_string_fn(x, m, args, span, cx)
    }

    fn orm_ns_call(&mut self, ns: &str, f: &str, args: &[ast::Expr], span: Span, cx: &mut Cx) -> Tr<V> {
        match (ns, f) {
            ("strings", "Contains" | "HasPrefix" | "HasSuffix") if args.len() == 2 => {
                let x = self.orm_value(&args[0], cx)?;
                self.orm_string_fn(x, f, &args[1..], span, cx)
            }
            ("strings", "ToUpper" | "ToLower" | "Trim" | "TrimSpace") if args.len() == 1 => {
                let x = self.orm_value(&args[0], cx)?;
                self.orm_string_fn(x, f, &[], span, cx)
            }
            ("slices", "Contains") if args.len() == 2 => {
                if mentions(&args[0], &cx.names()) {
                    return no("the slice of slices.Contains must not depend on the row (it is passed as a parameter list)", args[0].span);
                }
                let list = self.orm_captured(&args[0], cx)?;
                if !matches!(list.s, S::List(_)) {
                    return no("the first argument of slices.Contains must be a slice", args[0].span);
                }
                let x = self.orm_value(&args[1], cx)?;
                Ok(V::scalar(seq!["(", x.s, " IN (", list.s, "))"], self.tt.bool_))
            }
            ("math", "Abs") if args.len() == 1 => {
                let x = self.orm_value(&args[0], cx)?;
                Ok(V { s: seq!["abs(", x.s, ")"], ..x })
            }
            _ => no(format!("function {ns}.{f} cannot be translated"), span),
        }
    }

    /// Aggregate SQL for both dialects: SUM/AVG need casts on PostgreSQL
    /// (numeric results).
    pub(crate) fn orm_agg(&self, f: &str, x: &S, ty: TypeId, coalesce: bool) -> (S, TypeId) {
        let int = matches!(self.tt.get(self.orm_base(ty)), Type::Int(_));
        match f {
            "Sum" => {
                let pg = if int { seq!["CAST(SUM(", x, ") AS bigint)"] } else { seq!["SUM(", x, ")"] };
                let lite = seq!["SUM(", x, ")"];
                let s = S::d(pg, lite);
                let s = if coalesce { seq!["COALESCE(", s, ", 0)"] } else { s };
                (s, self.orm_base(ty))
            }
            "Average" => {
                let s = S::d(seq!["CAST(AVG(", x, ") AS double precision)"], seq!["AVG(", x, ")"]);
                (s, self.tt.float)
            }
            "Min" => (seq!["MIN(", x, ")"], ty),
            _ => (seq!["MAX(", x, ")"], ty),
        }
    }

    /// `u.Orders.Any()`, `.Any(pred)`, `.All(pred)`, `.Count()`, `.Count(pred)`,
    /// `.Sum(sel)`, `.Min/Max/Average(sel)`: correlated subqueries.
    fn orm_coll_op(&mut self, v: V, m: &str, args: &[ast::Expr], span: Span, cx: &mut Cx) -> Tr<V> {
        let VK::Coll(n, owner, oa) = v.kind else { unreachable!() };
        let Some(target) = self.orm_model(n.target) else { return Err(None) };
        let alias = self.orm_alias(cx);
        let corr = self.orm_correlate(&n, &owner, &oa, &target, &alias, span)?;
        let depth = cx.scopes.len();
        let pred = |s: &mut Self, cx: &mut Cx, what: &str| -> Tr<Option<S>> {
            if args.is_empty() {
                return Ok(None);
            }
            if args.len() != 1 {
                return no(format!("{what} takes one lambda"), span);
            }
            let body = s.orm_lambda(&args[0], vec![Bind::Ent(target.clone(), alias.clone())], what, cx)?;
            let p = s.orm_pred(body, cx)?;
            Ok(Some(p))
        };
        let boolt = self.tt.bool_;
        let r = match m {
            "Any" => {
                let p = pred(self, cx, "Any")?;
                let w = p.map(|p| seq![" AND (", p, ")"]).unwrap_or(S::t(""));
                V::scalar(seq!["EXISTS (SELECT 1 FROM ", corr, w, ")"], boolt)
            }
            "All" => {
                let Some(p) = pred(self, cx, "All")? else { return no("All needs a condition: All((o) => ...)", span) };
                V::scalar(seq!["NOT EXISTS (SELECT 1 FROM ", corr, " AND NOT (", p, "))"], boolt)
            }
            "Count" => {
                let p = pred(self, cx, "Count")?;
                let w = p.map(|p| seq![" AND (", p, ")"]).unwrap_or(S::t(""));
                let int = self.tt.int;
                V::scalar(seq!["(SELECT COUNT(*) FROM ", corr, w, ")"], int)
            }
            "Sum" | "Min" | "Max" | "Average" => {
                if args.len() != 1 {
                    return no(format!("{m} takes one lambda selecting the value: {m}((o) => o.Total)"), span);
                }
                let body = self.orm_lambda(&args[0], vec![Bind::Ent(target.clone(), alias.clone())], m, cx)?;
                let x = self.orm_value(body, cx)?;
                let t = self.orm_ty(&x);
                if m != "Min" && m != "Max" && !self.orm_is_num(t) && !self.orm_is(t, "decimal") {
                    return no(format!("{m} needs a number, found {}", self.tn(t)), body.span);
                }
                let (agg, ty) = self.orm_agg(m, &x.s, t, m == "Sum");
                V::scalar(seq!["(SELECT ", agg, " FROM ", corr, ")"], ty)
            }
            _ => return no(format!("the collection {} has no operation {m} in SQL (Any, All, Count, Sum, Min, Max, Average)", n.field), span),
        };
        cx.scopes.truncate(depth);
        Ok(r)
    }

    /// `g.Count()`, `g.Count(pred)`, `g.Sum(sel)`, ... inside GroupBy.
    fn orm_group_op(&mut self, i: usize, m: &str, args: &[ast::Expr], span: Span, cx: &mut Cx) -> Tr<V> {
        let Bind::Group(_, _, ent, alias) = cx.scopes[i].1.clone() else { unreachable!() };
        let depth = cx.scopes.len();
        let int = self.tt.int;
        let r = match m {
            "Count" => {
                if args.is_empty() {
                    V::scalar(S::t("COUNT(*)"), int)
                } else {
                    let body = self.orm_lambda(&args[0], vec![Bind::Ent(ent.clone(), alias.clone())], "Count", cx)?;
                    let p = self.orm_pred(body, cx)?;
                    V::scalar(seq!["SUM(CASE WHEN ", p, " THEN 1 ELSE 0 END)"], int)
                }
            }
            "Sum" | "Min" | "Max" | "Average" => {
                if args.len() != 1 {
                    return no(format!("{m} takes one lambda: g.{m}((o) => o.Total)"), span);
                }
                let body = self.orm_lambda(&args[0], vec![Bind::Ent(ent.clone(), alias.clone())], m, cx)?;
                let x = self.orm_value(body, cx)?;
                let t = self.orm_ty(&x);
                if m != "Min" && m != "Max" && !self.orm_is_num(t) && !self.orm_is(t, "decimal") {
                    return no(format!("{m} needs a number, found {}", self.tn(t)), body.span);
                }
                let (agg, ty) = self.orm_agg(m, &x.s, t, false);
                V::scalar(agg, ty)
            }
            _ => return no(format!("a group has no operation {m} (Key, Count, Sum, Min, Max, Average)"), span),
        };
        cx.scopes.truncate(depth);
        Ok(r)
    }
}

/// `"name"` (double quotes, both dialects).
pub(crate) fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
