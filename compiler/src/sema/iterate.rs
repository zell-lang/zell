//! The iteration protocol of `for x in v` for user and library types
//! (DESIGN.md "Управление потоком"):
//!
//! 1. `v` has `func Iter() It`: the loop calls it once and then iterates
//!    the iterator `It`;
//! 2. otherwise `v` itself is an iterator if it has `Next()`;
//! 3. otherwise `v` has `func Items() []T`: the loop runs over that slice.
//!
//! An iterator has `func Next() (T, bool)` (the builtin interface
//! `Iterator<T>`, matched structurally), `func Next() (K, V, bool)` (two
//! loop variables: key and value) or `func Next() *T` (`nil` ends the
//! loop). `async func Next()` iterators, and `func Next() Task<...>` ones
//! (`Channel<T>`), are awaited, so the
//! loop must be inside an async function. With one-value iterators,
//! `for i, x in v` numbers the elements from 0.
//!
//! Lowering (HIR, no runtime support):
//!
//! ```text
//! { $seq = v; $it = $seq.Iter(); $i = -1
//!   loop { ($x, $ok) = [await] $it.Next(); if !$ok { break }; $i += 1; body } }
//! ```

use super::*;
use crate::hir::{ExprKind, Stmt, StmtKind, UnOp};

/// Shape of an iterator's `Next` result.
enum NextShape {
    /// `(T, bool)`.
    One(TypeId),
    /// `(K, V, bool)`.
    Two(TypeId, TypeId),
    /// `*T`, `nil` at the end.
    Ptr,
}

impl<'a> Sema<'a> {
    /// Signature of non-generic method `name` of a value of type `ty`
    /// (struct, pointer to struct, enum, interface or interface-constrained
    /// type parameter): (parameters, result, async).
    pub(crate) fn value_method_sig(&self, ty: TypeId, name: &str) -> Option<(Vec<TypeId>, TypeId, bool)> {
        let t = match self.tt.get(ty) {
            Type::Pointer(inner) if matches!(self.tt.get(*inner), Type::Struct(_)) => *inner,
            _ => ty,
        };
        match self.tt.get(t) {
            Type::Struct(d) => {
                let (fid, _) = self.find_method(d.0, name)?;
                let s = &self.sigs[fid.0 as usize];
                Some((s.params.clone(), s.ret, s.is_async))
            }
            Type::Enum(d) => {
                let fid = self.enum_methods.get(&(d.0, name.to_string()))?;
                let s = &self.sigs[fid.0 as usize];
                Some((s.params.clone(), s.ret, s.is_async))
            }
            Type::Param(_) => {
                let m = self.param_methods(t)?.into_iter().find(|m| m.name == name)?;
                Some((m.params, m.ret, false))
            }
            _ if self.tt.is_interface(t) => {
                let m = self.iface_methods(t).into_iter().find(|m| m.name == name)?;
                Some((m.params, m.ret, false))
            }
            _ => None,
        }
    }

    /// `for vars in seq` through the iteration protocol; gives `seq` back
    /// if its type has neither `Iter`, `Next` nor `Items`.
    pub(crate) fn protocol_for(&mut self, vars: &[ast::Ident], seq: hir::Expr, body: &ast::Block, span: Span) -> Result<StmtKind, hir::Expr> {
        let ty = seq.ty;
        let nullary = |s: &Self, n: &str| s.value_method_sig(ty, n).filter(|(p, _, _)| p.is_empty());
        let iter = nullary(self, "Iter");
        let next = nullary(self, "Next");
        let items = nullary(self, "Items").filter(|(_, r, _)| matches!(self.tt.get(*r), Type::Slice(_)));
        if iter.is_none() && next.is_none() && items.is_none() {
            return Err(seq);
        }
        if vars.len() > 2 || vars.is_empty() {
            self.err("for ... in takes one or two variables", span);
        }
        let sp = seq.span;
        let tname = self.tn(ty);
        let mut stmts = Vec::new();
        let seq_tmp = self.temp(ty, sp);
        stmts.push(Stmt { kind: StmtKind::Let { local: seq_tmp, init: Some(seq) }, span: sp });
        let seq_val = mk(ExprKind::Local(seq_tmp), ty, sp);
        let ident = |n: &str| ast::Ident { name: n.into(), span: sp };
        // The iterator: `$seq.Iter()`, `$seq` itself, or the Items() slice.
        let it = if let Some((_, it_ty, is_async)) = iter {
            if is_async {
                self.err(format!("{tname}.Iter must not be async (for ... in calls it once to get the iterator)"), sp);
                return Ok(StmtKind::Block(hir::Block { stmts, span }));
            }
            let call = self.method_call(seq_val, &ident("Iter"), &[], sp);
            let call = self.require_value(call);
            let t = self.temp(it_ty, sp);
            stmts.push(Stmt { kind: StmtKind::Let { local: t, init: Some(call) }, span: sp });
            mk(ExprKind::Local(t), it_ty, sp)
        } else if next.is_some() {
            seq_val
        } else {
            let items = self.method_call(seq_val, &ident("Items"), &[], sp);
            let items = self.require_value(items);
            let Type::Slice(elem_ty) = *self.tt.get(items.ty) else { unreachable!("Items returns a slice") };
            self.push_scope();
            let (index, elem) = if vars.len() == 1 {
                (None, self.for_var_decl(vars.first(), elem_ty))
            } else {
                let i = self.for_var_decl(vars.first(), self.tt.int);
                (i, self.for_var_decl(vars.get(1), elem_ty))
            };
            let label = self.push_label(LabelKind::Loop);
            let hbody = self.block(body);
            self.pop_label();
            self.pop_scope();
            stmts.push(Stmt { kind: StmtKind::ForSlice { label, index, elem, seq: items, body: hbody }, span });
            return Ok(StmtKind::Block(hir::Block { stmts, span }));
        };
        let it_ty = it.ty;
        let itn = self.tn(it_ty);
        let Some((params, ret, is_async)) = self.value_method_sig(it_ty, "Next") else {
            self.err_help(format!("cannot iterate over {tname}: its iterator {itn} has no method Next"), sp, "give the iterator `func Next() (T, bool)`");
            return Ok(StmtKind::Block(hir::Block { stmts, span }));
        };
        // `func Next() Task<(T, bool)>` is an async Next too (the task is
        // awaited), like a `Task<T>` method of an interface.
        let (ret, is_async) = match *self.tt.get(ret) {
            Type::Task(inner) if !is_async => (inner, true),
            _ => (ret, is_async),
        };
        let shape = match self.tt.get(ret).clone() {
            Type::Tuple(ts) if ts.len() == 2 && ts[1] == self.tt.bool_ => Some(NextShape::One(ts[0])),
            Type::Tuple(ts) if ts.len() == 3 && ts[2] == self.tt.bool_ => Some(NextShape::Two(ts[0], ts[1])),
            Type::Pointer(_) => Some(NextShape::Ptr),
            _ => None,
        };
        let Some(shape) = shape.filter(|_| params.is_empty()) else {
            let found = format!("func Next{}", &self.sig_string(&params, ret)[4..]);
            self.err_help(
                format!("cannot iterate over {tname}: {itn}.Next must return (T, bool), (K, V, bool) or *T, found `{found}`"),
                sp,
                "declare `func Next() (T, bool)` (the Iterator<T> protocol)",
            );
            return Ok(StmtKind::Block(hir::Block { stmts, span }));
        };
        if is_async && !self.fns.last().is_some_and(|f| f.is_async) {
            let name = self.fns.last().map(|f| f.name.clone()).unwrap_or_default();
            self.err_help(format!("for ... in over {tname} awaits {itn}.Next, which is async; {name} is not declared async"), sp, format!("declare {name} as `async func`"));
        }
        let it_tmp = match it.kind {
            ExprKind::Local(l) => l,
            _ => unreachable!("iterator is a local"),
        };
        let int = self.tt.int;
        let bool_ = self.tt.bool_;
        let counter = match shape {
            NextShape::One(_) | NextShape::Ptr if vars.len() == 2 => {
                let c = self.temp(int, sp);
                stmts.push(Stmt { kind: StmtKind::Let { local: c, init: Some(mk(ExprKind::Const(hir::ConstValue::Int(-1)), int, sp)) }, span: sp });
                Some(c)
            }
            _ => None,
        };
        self.push_scope();
        // Loop variables.
        let (first, second) = match &shape {
            NextShape::One(t) => {
                if vars.len() == 2 {
                    (self.for_var_decl(vars.first(), int), self.for_var_decl(vars.get(1), *t))
                } else {
                    (self.for_var_decl(vars.first(), *t), None)
                }
            }
            NextShape::Two(k, v) => {
                if vars.len() == 1 {
                    self.err(format!("for ... in over {tname} takes two variables (key, value): {itn}.Next returns two values"), span);
                }
                (self.for_var_decl(vars.first(), *k), self.for_var_decl(vars.get(1), *v))
            }
            NextShape::Ptr => {
                if vars.len() == 2 {
                    (self.for_var_decl(vars.first(), int), self.for_var_decl(vars.get(1), ret))
                } else {
                    (self.for_var_decl(vars.first(), ret), None)
                }
            }
        };
        let label = self.push_label(LabelKind::Loop);
        let hbody = self.block(body);
        self.pop_label();
        self.pop_scope();
        // `[await] $it.Next()`
        let call = self.method_call(mk(ExprKind::Local(it_tmp), it_ty, sp), &ident("Next"), &[], sp);
        let call = if is_async {
            match *self.tt.get(call.ty) {
                Type::Task(t) => mk(ExprKind::Await(Box::new(call)), t, sp),
                _ => call,
            }
        } else {
            call
        };
        let brk = hir::Block { stmts: vec![Stmt { kind: StmtKind::Break(label), span: sp }], span: sp };
        let mut lbody = Vec::new();
        let (value_local, elem_local) = match (&shape, counter) {
            (NextShape::Two(..), _) => (first, second),
            (_, Some(_)) => (second, first),
            (_, None) => (first, None),
        };
        match shape {
            NextShape::One(_) | NextShape::Two(..) => {
                let ok = self.temp(bool_, sp);
                let locals = match shape {
                    NextShape::Two(..) => vec![value_local, elem_local, Some(ok)],
                    _ => vec![value_local, Some(ok)],
                };
                lbody.push(Stmt { kind: StmtKind::LetTuple { locals, init: call }, span: sp });
                let not_ok = mk(ExprKind::Unary { op: UnOp::Not, operand: Box::new(mk(ExprKind::Local(ok), bool_, sp)) }, bool_, sp);
                lbody.push(Stmt { kind: StmtKind::If { cond: not_ok, then: brk, else_: None }, span: sp });
            }
            NextShape::Ptr => {
                let v = match value_local {
                    Some(l) => l,
                    None => self.temp(ret, sp),
                };
                lbody.push(Stmt { kind: StmtKind::Let { local: v, init: Some(call) }, span: sp });
                let is_nil = mk(ExprKind::IsNil { value: Box::new(mk(ExprKind::Local(v), ret, sp)), negated: false }, bool_, sp);
                lbody.push(Stmt { kind: StmtKind::If { cond: is_nil, then: brk, else_: None }, span: sp });
            }
        }
        if let Some(c) = counter {
            let one = mk(ExprKind::Const(hir::ConstValue::Int(1)), int, sp);
            lbody.push(Stmt { kind: StmtKind::CompoundAssign { target: mk(ExprKind::Local(c), int, sp), op: hir::BinOp::Add, value: one }, span: sp });
            if let Some(i) = first {
                lbody.push(Stmt { kind: StmtKind::Let { local: i, init: Some(mk(ExprKind::Local(c), int, sp)) }, span: sp });
            }
        }
        lbody.push(Stmt { kind: StmtKind::Block(hbody), span });
        stmts.push(Stmt { kind: StmtKind::Loop { label, cond: None, body: hir::Block { stmts: lbody, span } }, span });
        Ok(StmtKind::Block(hir::Block { stmts, span }))
    }

    /// A loop variable (`None` for `_` or a missing name).
    fn for_var_decl(&mut self, v: Option<&ast::Ident>, ty: TypeId) -> Option<LocalId> {
        let v = v?;
        if v.name == "_" {
            return None;
        }
        Some(self.declare_local(&v.name, ty, LocalKind::LoopVar, v.span))
    }
}
