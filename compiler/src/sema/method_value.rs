//! Method values and method expressions (DESIGN.md «Структуры и методы»).
//!
//! `x.M` without a call is a closure bound to the receiver, built from
//! ordinary lambdas: `((r) => (a0, ..) => r.M(a0, ..))(recv)`, so the
//! receiver is evaluated once, where the method value is written, and the
//! inner closure keeps it (retained by its capture, released with it). A
//! struct variable is bound by address (`&x`, its cell: like Go's `(&x).M`,
//! the method sees later changes of `x` and its changes are visible in
//! `x`); a pointer or an interface value is bound as it is; any other
//! struct value (a call result) is a copy. `T.M` is a function taking the
//! receiver `*T` first: `(r *T, a0, ..) => r.M(a0, ..)`.

use super::*;
use crate::hir::{Callee, ExprKind};

/// Synthetic names (`$` cannot start a user identifier).
const RECV: &str = "$recv";

impl<'a> Sema<'a> {
    /// `(params, result)` of method `name` of a value of type `ty` (the
    /// result is `Task<T>` for an async method), or `None` when `name` is
    /// not a method (a field, or no member at all).
    fn method_value_sig(&mut self, ty: TypeId, name: &ast::Ident) -> Option<(Vec<TypeId>, TypeId)> {
        if let Some(sid) = self.struct_of(ty) {
            if self.find_field(sid, &name.name).is_some() || self.gen.methods.contains_key(&(sid, name.name.clone())) {
                return None;
            }
            let (fid, _) = self.find_method(sid, &name.name)?;
            return Some(self.method_sig_parts(fid));
        }
        if let Type::Enum(d) = *self.tt.get(ty) {
            let fid = *self.enum_methods.get(&(d.0, name.name.clone()))?;
            return Some(self.method_sig_parts(fid));
        }
        let methods = match self.param_methods(ty) {
            Some(m) => m,
            None if self.tt.is_interface(ty) => self.iface_methods(ty),
            None => return None,
        };
        let m = methods.iter().find(|m| m.name == name.name)?;
        Some((m.params.clone(), m.ret))
    }

    /// Parameters (without the receiver) and result (`Task<T>` when async).
    fn method_sig_parts(&mut self, f: FuncId) -> (Vec<TypeId>, TypeId) {
        let sig = self.sigs[f.0 as usize].clone();
        let ret = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
        (sig.params, ret)
    }

    /// `base.name` as a method value, or `Err(base)` when `name` is not a
    /// method of `base`.
    pub(crate) fn method_value(&mut self, base: hir::Expr, name: &ast::Ident) -> Result<hir::Expr, hir::Expr> {
        let Some((params, ret)) = self.method_value_sig(base.ty, name) else { return Err(base) };
        let span = base.span.to(name.span);
        let recv = if matches!(self.tt.get(base.ty), Type::Struct(_)) {
            match base.kind {
                ExprKind::Local(_) | ExprKind::Upvalue(_) => self.addr_of(base),
                _ if self.is_place(&base, false) => {
                    self.err_help(
                        format!("cannot take the method value {} of a struct field or element (it would keep an interior pointer)", name.name),
                        name.span,
                        "take it of a pointer or a variable, or write a lambda: `() => x.f.M()`",
                    );
                    return Ok(self.invalid(span));
                }
                _ => base,
            }
        } else {
            base
        };
        let n = params.len();
        let inner_ty = self.tt.func(params, ret);
        let outer_ty = self.tt.func(vec![recv.ty], inner_ty);
        let sp = name.span;
        let inner = self.forwarding_lambda(RECV, &[], name, n, ret, sp);
        let outer = ast::Expr {
            kind: ast::ExprKind::Lambda { params: vec![param(RECV, sp)], ret: None, body: Box::new(ast::LambdaBody::Expr(inner)), is_async: false },
            span: sp,
        };
        let closure = self.expr(&outer, Some(outer_ty));
        if self.tt.is_invalid(closure.ty) {
            return Ok(self.invalid(span));
        }
        let ty = match self.tt.get(closure.ty) {
            Type::Func(sig) => sig.ret,
            _ => return Ok(self.invalid(span)),
        };
        Ok(hir::Expr { kind: ExprKind::Call { callee: Callee::Value(Box::new(closure)), args: vec![recv] }, ty, span })
    }

    /// `T.M` with `M` a method of struct `sid`: a function taking the
    /// receiver `*T` first.
    pub(crate) fn method_expr(&mut self, sid: u32, f: FuncId, name: &ast::Ident, span: Span) -> hir::Expr {
        let (params, ret) = self.method_sig_parts(f);
        let st = self.struct_ty(sid);
        let recv = self.tt.pointer(st);
        let n = params.len();
        let mut ps = vec![recv];
        ps.extend(params);
        let ty = self.tt.func(ps, ret);
        let l = self.forwarding_lambda(RECV, &[RECV], name, n, ret, name.span);
        let mut e = self.expr(&l, Some(ty));
        e.span = span;
        e
    }

    /// `(lead.., a0, .., a{n-1}) => recv.name(a0, ..)`: untyped parameters
    /// (typed by the expected function type); a fallible synchronous
    /// result is passed through with `?` in a block body.
    fn forwarding_lambda(&mut self, recv: &str, lead: &[&str], name: &ast::Ident, n: usize, ret: TypeId, sp: Span) -> ast::Expr {
        let id = |s: &str| ast::Expr { kind: ast::ExprKind::Ident(ast::Ident { name: s.to_string(), span: sp }), span: sp };
        let args: Vec<String> = (0..n).map(|i| format!("$a{i}")).collect();
        let mut params: Vec<ast::Param> = lead.iter().map(|s| param(s, sp)).collect();
        params.extend(args.iter().map(|a| param(a, sp)));
        let callee = ast::Expr { kind: ast::ExprKind::Field { base: Box::new(id(recv)), name: name.clone() }, span: sp };
        let call = ast::Expr { kind: ast::ExprKind::Call { callee: Box::new(callee), args: args.iter().map(|a| id(a)).collect() }, span: sp };
        let body = match self.tt.fallible_inner(ret) {
            Some(inner) => {
                let prop = ast::Expr { kind: ast::ExprKind::Propagate { expr: Box::new(call), context: None }, span: sp };
                let stmt = if matches!(self.tt.get(inner), Type::Void) {
                    ast::StmtKind::Expr(prop)
                } else {
                    ast::StmtKind::Return(vec![prop])
                };
                ast::LambdaBody::Block(ast::Block { stmts: vec![ast::Stmt { kind: stmt, span: sp }], span: sp })
            }
            None => ast::LambdaBody::Expr(call),
        };
        ast::Expr { kind: ast::ExprKind::Lambda { params, ret: None, body: Box::new(body), is_async: false }, span: sp }
    }
}

fn param(name: &str, sp: Span) -> ast::Param {
    ast::Param { attrs: Vec::new(), name: ast::Ident { name: name.to_string(), span: sp }, ty: ast::TypeExpr { kind: ast::TypeKind::Infer, span: sp }, span: sp }
}
