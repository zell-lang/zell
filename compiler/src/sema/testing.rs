//! `t.Must(x)` and `t.MustFail(x)` on a `*testing.T` (DESIGN.md
//! «Компилятор и инструменты», the exception in «Игнорирование и обязательность»).
//!
//! These two are the only calls that take an unhandled `T!` argument; they
//! are not methods of `testing.T` but are lowered here:
//!
//! ```text
//! t.Must(x)      =>  try x catch $err { t.mustError($err, "file.zl:12"); panic(...) }
//! t.MustFail(x)  =>  testing.mustFail<T>(t, x, "file.zl:12")
//! ```
//!
//! so `x` (which may contain `await`) is evaluated in the test function
//! itself, `Must` yields the value (or nothing for `void!`) and the
//! location of the call is a compile-time string. The receiver of `Must` is
//! evaluated only when `x` fails.

use super::generics::Inst;
use super::*;
use crate::hir::{Builtin, Callee, ConstValue, ExprKind};

impl Sema<'_> {
    /// Hook of [`method_call_targs`](Self::method_call_targs): `Must` /
    /// `MustFail` on a `testing.T`. Gives `base` back if it is no such call.
    pub(crate) fn testing_method_call(&mut self, base: hir::Expr, name: &ast::Ident, targs: Option<&[ast::TypeExpr]>, args: &[ast::Expr], span: Span) -> Result<hir::Expr, hir::Expr> {
        let must = match name.name.as_str() {
            "Must" => true,
            "MustFail" => false,
            _ => return Err(base),
        };
        if targs.is_some() || !self.struct_of(base.ty).is_some_and(|sid| self.structs[sid as usize].full_name == "testing.T") {
            return Err(base);
        }
        self.ide_ref(name.span, super::index::SymKey::Std(format!("testing.T.{}", name.name)));
        let what = format!("t.{}", name.name);
        if !self.arity(&what, args, 1, span) {
            return Ok(self.invalid(span));
        }
        let v = self.expr(&args[0], None);
        let Some(inner) = self.tt.fallible_inner(v.ty) else {
            if !self.tt.is_invalid(v.ty) {
                let help = if must { "use the value directly: it cannot fail" } else { "this expression cannot fail; t.MustFail needs a call returning T!" };
                self.err_help(format!("{what} needs a fallible (T!) argument, found {}", self.tn(v.ty)), v.span, help);
            }
            return Ok(self.invalid(span));
        };
        let recv = if matches!(self.tt.get(base.ty), Type::Pointer(_)) { base } else { self.addr_of(base) };
        let at = mk(ExprKind::Const(ConstValue::Str(stdlib::source_location(self.cur_file, span.start))), self.tt.string, span);
        if must {
            let sid = self.struct_of(recv.ty).unwrap();
            let Some((fail, _)) = self.find_method(sid, "mustError") else { return Ok(self.invalid(span)) };
            self.push_scope();
            let err = self.declare_local("$err", self.tt.error, LocalKind::CatchVar, span);
            self.pop_scope();
            let report = mk(ExprKind::Call { callee: Callee::Func(fail), args: vec![recv, mk(ExprKind::Local(err), self.tt.error, span), at] }, self.tt.void, span);
            let msg = mk(ExprKind::Const(ConstValue::Str("testing: t.Must returned after a failure".into())), self.tt.string, span);
            let stop = mk(ExprKind::Builtin { op: Builtin::Panic, args: vec![msg] }, self.tt.void, span);
            let handler = hir::Block { stmts: vec![hir::Stmt { kind: hir::StmtKind::Expr(report), span }, hir::Stmt { kind: hir::StmtKind::Expr(stop), span }], span };
            return Ok(mk(ExprKind::TryCatch { value: Box::new(v), err: Some(err), handler, result_used: true }, inner, span));
        }
        let t = self.ns_index.get("testing").and_then(|&ns| match self.nss[ns].members.get("mustFail") {
            Some((Member::Generic(t), _, _)) => Some(*t),
            _ => None,
        });
        let Some(Inst::Func(fid)) = t.and_then(|t| self.instantiate(t, vec![inner], span)) else { return Ok(self.invalid(span)) };
        Ok(mk(ExprKind::Call { callee: Callee::Func(fid), args: vec![recv, v, at] }, self.tt.error, span))
    }
}
