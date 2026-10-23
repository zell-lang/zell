//! Statements, loops, `switch`, `return` and the error-handling
//! expressions (`?`, `try ... catch`, `try ... else`).

use super::{unsupported, CodeGen, LoopTarget, TryTarget, Val, R};
use crate::hir::{Block, Expr, ExprKind, Label, LocalId, Pattern, ReturnValue, Stmt, StmtKind, Switch, TryFallback};
use crate::types::{Type, TypeId};
use inkwell::basic_block::BasicBlock;
use inkwell::values::{BasicValueEnum, IntValue, PointerValue};
use inkwell::IntPredicate;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    pub(crate) fn block_stmts(&mut self, b: &Block) -> R<()> {
        for s in &b.stmts {
            self.stmt(s)?;
        }
        Ok(())
    }

    /// A nested block: its `Let` locals are released at its end.
    pub(crate) fn scoped_block(&mut self, b: &Block) -> R<()> {
        let d = self.depth();
        self.push_scope();
        self.dbg_push_block(b.span);
        let r = self.block_stmts(b);
        self.dbg_pop_block();
        self.pop_scope(d);
        r
    }

    /// A condition: temporaries are released before branching on it.
    pub(crate) fn cond_expr(&mut self, e: &Expr) -> R<IntValue<'ctx>> {
        let d = self.depth();
        let c = self.expr(e)?.get().into_int_value();
        self.release_temps_from(d);
        Ok(c)
    }

    fn terminated(&self) -> bool {
        self.b.get_insert_block().and_then(|b| b.get_terminator()).is_some()
    }

    /// Branch to `bb`, then continue in a dead block.
    fn jump(&mut self, bb: BasicBlock<'ctx>) {
        self.br(bb);
        self.dead_block();
    }

    fn stmt(&mut self, s: &Stmt) -> R<()> {
        if self.terminated() {
            self.dead_block();
        }
        let d = self.depth();
        self.dbg_span(s.span);
        self.dbga_stmt(s.span);
        let r = self.stmt_inner(s);
        self.release_temps_from(d);
        r
    }

    fn stmt_inner(&mut self, s: &Stmt) -> R<()> {
        match &s.kind {
            StmtKind::Let { local, init } => {
                match init {
                    Some(e) if self.is_interior_local(*local) => self.store_interior(*local, e, true)?,
                    Some(Expr { kind: ExprKind::New(inner), .. }) if self.fs().locals[local.0 as usize].and_then(|s| s.stack_obj).is_some() => {
                        // `var p = new T{...}` that does not escape: the
                        // object is the function's stack object.
                        let v = self.expr_owned(inner)?;
                        self.dbg_declare(*local);
                        self.clear_const_only(*local);
                        self.release_local(*local, false);
                        let slot = self.fs().locals[local.0 as usize].unwrap();
                        let obj = slot.stack_obj.unwrap();
                        self.b.build_store(obj, v).unwrap();
                        self.b.build_store(slot.ptr, obj).unwrap();
                    }
                    Some(e) => {
                        let v = self.expr(e)?;
                        self.assign_local(*local, v, true)?;
                    }
                    None => self.zero_local(*local)?,
                }
                // An open-coded defer's operand lives to the exit block.
                if self.fs().hir.unwrap().locals[local.0 as usize].kind != crate::hir::LocalKind::DeferArg {
                    self.scope_local(*local);
                }
                Ok(())
            }
            StmtKind::LetTuple { locals, init } => {
                let tv = self.expr_owned(init)?;
                let Type::Tuple(ts) = self.ty(init.ty).clone() else { return unsupported("tuple destructuring of a non-tuple") };
                for (i, l) in locals.iter().enumerate() {
                    let c = self.b.build_extract_value(tv.into_struct_value(), i as u32, "").unwrap();
                    match l {
                        Some(l) => {
                            self.assign_local(*l, Val::owned(c), true)?;
                            self.scope_local(*l);
                        }
                        None => self.release_val(ts[i], c),
                    }
                }
                Ok(())
            }
            StmtKind::Assign { target, value } => self.assign(target, value),
            StmtKind::CompoundAssign { target, op, value } => {
                let rv = self.expr(value)?;
                let h = self.hold(value.ty, rv);
                let p = if let ExprKind::MapGet { map, key } = &target.kind {
                    let (kt, _) = self.map_kv(map.ty);
                    let mv = self.expr(map)?;
                    self.hold(map.ty, mv);
                    let kv = self.expr(key)?;
                    self.hold(key.ty, kv);
                    // The map stays alive (held) until the end of the statement.
                    self.map_slot(kt, mv.get(), kv.get(), true)
                } else {
                    if let ExprKind::Local(l) = &target.kind {
                        self.clear_const_only(*l);
                    }
                    self.place(target)?
                };
                self.unhold(h);
                self.compound_at(p, target.ty, *op, rv, value.ty)
            }
            StmtKind::Expr(e) | StmtKind::Discard(e) => self.expr_discard(e),
            StmtKind::If { cond, then, else_ } => {
                let c = self.cond_expr(cond)?;
                let tb = self.block("then");
                let eb = self.block("else");
                let end = self.block("endif");
                self.b.build_conditional_branch(c, tb, eb).unwrap();
                self.b.position_at_end(tb);
                self.scoped_block(then)?;
                self.br(end);
                self.b.position_at_end(eb);
                if let Some(e) = else_ {
                    self.scoped_block(e)?;
                }
                self.br(end);
                self.b.position_at_end(end);
                Ok(())
            }
            StmtKind::Loop { label, cond, body } => {
                let head = self.block("loop.head");
                let bb = self.block("loop.body");
                let end = self.block("loop.end");
                self.br(head);
                self.b.position_at_end(head);
                match cond {
                    Some(c) => {
                        let cv = self.cond_expr(c)?;
                        self.b.build_conditional_branch(cv, bb, end).unwrap();
                    }
                    None => {
                        self.b.build_unconditional_branch(bb).unwrap();
                    }
                }
                self.b.position_at_end(bb);
                self.loop_body(*label, end, head, body, &[])?;
                self.b.position_at_end(end);
                Ok(())
            }
            StmtKind::ForRange { label, var, start, end, body } => {
                let sv = self.expr(start)?.get().into_int_value();
                let ev = self.expr(end)?.get().into_int_value();
                let signed = self.is_signed(start.ty);
                let ctr = self.entry_alloca(sv.get_type().into(), "i", false);
                self.b.build_store(ctr, sv).unwrap();
                let head = self.block("for.head");
                let bb = self.block("for.body");
                let cont = self.block("for.next");
                let done = self.block("for.end");
                self.br(head);
                self.b.position_at_end(head);
                let iv = self.b.build_load(sv.get_type(), ctr, "").unwrap().into_int_value();
                let c = self.b.build_int_compare(if signed { IntPredicate::SLT } else { IntPredicate::ULT }, iv, ev, "").unwrap();
                self.b.build_conditional_branch(c, bb, done).unwrap();
                self.b.position_at_end(bb);
                if let Some(v) = var {
                    self.assign_local(*v, Val::borrowed(iv.into()), true)?;
                }
                self.loop_body(*label, done, cont, body, &[*var])?;
                self.b.position_at_end(cont);
                let iv = self.b.build_load(sv.get_type(), ctr, "").unwrap().into_int_value();
                let n = self.b.build_int_add(iv, sv.get_type().const_int(1, false), "").unwrap();
                self.b.build_store(ctr, n).unwrap();
                self.b.build_unconditional_branch(head).unwrap();
                self.b.position_at_end(done);
                Ok(())
            }
            StmtKind::ForSlice { label, index, elem, seq, body } => self.for_slice(*label, *index, *elem, seq, body),
            StmtKind::ForString { label, index, ch, string, body } => self.for_string(*label, *index, *ch, string, body),
            StmtKind::ForMap { label, key, value, map, body } => self.for_map(*label, *key, *value, map, body),
            StmtKind::Switch(sw) => self.switch(sw),
            StmtKind::Return(rv) => self.emit_return(rv),
            StmtKind::Break(l) => {
                let t = self.target(*l)?;
                self.exit_cleanups(t.depth, true);
                self.jump(t.brk);
                Ok(())
            }
            StmtKind::Continue(l) => {
                let t = self.target(*l)?;
                let Some(c) = t.cont else { return unsupported("continue to a switch") };
                self.exit_cleanups(t.depth, true);
                self.jump(c);
                Ok(())
            }
            StmtKind::Defer(e) => self.emit_defer(e),
            StmtKind::Block(b) => self.scoped_block(b),
            StmtKind::TryBlock { body, err, handler } => self.try_block(body, *err, handler),
        }
    }

    fn target(&self, l: Label) -> R<LoopTarget<'ctx>> {
        match self.fs().loops.iter().rev().find(|t| t.label == l) {
            Some(t) => Ok(*t),
            None => unsupported("break/continue target outside the function"),
        }
    }

    /// Emits a loop body with its break/continue targets, then branches to
    /// `cont`.
    fn loop_body(&mut self, label: Label, brk: BasicBlock<'ctx>, cont: BasicBlock<'ctx>, body: &Block, vars: &[Option<LocalId>]) -> R<()> {
        let depth = self.depth();
        self.fs_mut().loops.push(LoopTarget { label, brk, cont: Some(cont), depth });
        // The loop variables belong to the body's scope: released at the
        // end of each iteration.
        self.push_scope();
        for v in vars.iter().flatten() {
            self.scope_local(*v);
        }
        self.dbg_push_block(body.span);
        let r = self.block_stmts(body);
        self.dbg_pop_block();
        self.pop_scope(depth);
        self.fs_mut().loops.pop();
        r?;
        self.br(cont);
        Ok(())
    }

    fn assign(&mut self, target: &Expr, value: &Expr) -> R<()> {
        if let ExprKind::MapGet { map, key } = &target.kind {
            let (kt, vt) = self.map_kv(map.ty);
            let d = self.depth();
            let later = self.may_run_code(key) || self.may_run_code(value);
            let mv = self.operand(map, later)?;
            self.hold(map.ty, mv);
            let later = self.may_run_code(value);
            let kv = self.operand(key, later)?;
            self.hold(key.ty, kv);
            let vv = self.expr(value)?;
            self.hold(value.ty, vv);
            self.map_set(kt, vt, mv.get().into_pointer_value(), kv.get(), vv.get())?;
            self.release_temps_from(d);
            return Ok(());
        }
        if let ExprKind::Local(l) = &target.kind {
            if self.is_interior_local(*l) {
                return self.store_interior(*l, value, false);
            }
            if self.is_stable(target) {
                // `s = s + x` and `xs = append(xs, x)` move the old value
                // into the operation (in place when uniquely referenced).
                let same = |e: &Expr| matches!(e.kind, ExprKind::Local(m) if m == *l);
                match &value.kind {
                    ExprKind::Binary { op: crate::hir::BinOp::StrConcat, lhs, rhs } if same(lhs) => {
                        let rv = self.expr(rhs)?;
                        self.clear_const_only(*l);
                        let p = self.local_addr(*l);
                        return self.compound_at(p, target.ty, crate::hir::BinOp::StrConcat, rv, rhs.ty);
                    }
                    ExprKind::Builtin { op: crate::hir::Builtin::Append, args } if same(&args[0]) => {
                        self.clear_const_only(*l);
                        let p = self.local_addr(*l);
                        return self.append_in_place(p, target.ty, &args[1..]);
                    }
                    _ => {}
                }
            }
        }
        let vv = self.expr(value)?;
        if let ExprKind::Local(l) = &target.kind {
            return self.assign_local(*l, vv, false);
        }
        let h = self.hold(value.ty, vv);
        let p = self.place(target)?;
        self.unhold(h);
        self.store_assign(p, target.ty, vv);
        Ok(())
    }

    /// `*p op= rv`.
    fn compound_at(&mut self, p: PointerValue<'ctx>, ty: TypeId, op: crate::hir::BinOp, rv: Val<'ctx>, rty: TypeId) -> R<()> {
        let lt = self.llty(ty);
        let old = self.b.build_load(lt, p, "").unwrap();
        if op == crate::hir::BinOp::StrConcat {
            // The place's reference moves into the concatenation, which
            // extends the string in place when nobody else holds it.
            let nv = self.call_rt_v("zell_string_append", &[old.into(), rv.get().into()]);
            self.b.build_store(p, nv).unwrap();
            self.drop_val(rty, rv);
            return Ok(());
        }
        let nv = self.binop_vals(op, ty, old, rv.get(), rty)?;
        let nv_owned = self.to_owned(ty, nv);
        self.b.build_store(p, nv_owned).unwrap();
        self.release_val(ty, old);
        self.drop_val(rty, rv);
        Ok(())
    }

    /// `xs = append(xs, x)` for a local `xs` at `p`: the variable's
    /// reference moves into the append (which grows a uniquely referenced
    /// backing in place) and the result is stored back.
    fn append_in_place(&mut self, p: PointerValue<'ctx>, ty: TypeId, xs: &[Expr]) -> R<()> {
        let et = self.elem_of(ty).unwrap();
        // All values first, then the appends (`append(xs, len(xs), len(xs))`).
        let mut vals = Vec::with_capacity(xs.len());
        for x in xs {
            vals.push(self.expr(x)?);
        }
        for xv in vals {
            self.append_fast(p, et, xv);
        }
        Ok(())
    }

    /// A zeroed slot released at function exit, holding a loop sequence.
    fn scoped_slot(&mut self, t: TypeId, v: Val<'ctx>) -> PointerValue<'ctx> {
        let lt = self.llty(t);
        let p = self.entry_alloca(lt, "seq", true);
        if self.needs_rc(t) {
            self.fs_mut().scoped.push((p, t));
        }
        self.store_assign(p, t, v);
        p
    }

    /// A function-lifetime slot holding owned value `v` (released at exit,
    /// or when the slot is stored again).
    pub(crate) fn scoped_slot_owned(&mut self, t: TypeId, v: BasicValueEnum<'ctx>) -> PointerValue<'ctx> {
        self.scoped_slot(t, Val::owned(v))
    }

    fn for_slice(&mut self, label: Label, index: Option<LocalId>, elem: Option<LocalId>, seq: &Expr, body: &Block) -> R<()> {
        let sv = self.expr(seq)?;
        let tmp = self.scoped_slot(seq.ty, sv);
        let et = self.elem_of(seq.ty).unwrap();
        let elt = self.llty(et);
        let is_array = matches!(self.ty(seq.ty), Type::Array(..));
        let (data, len): (PointerValue, IntValue) = if let Type::Array(_, n) = self.ty(seq.ty) {
            (tmp, self.i64_ty().const_int(*n, false))
        } else {
            let s = self.b.build_load(self.slice_ty(), tmp, "").unwrap().into_struct_value();
            (self.b.build_extract_value(s, 0, "").unwrap().into_pointer_value(), self.b.build_extract_value(s, 1, "").unwrap().into_int_value())
        };
        let ctr = self.entry_alloca(self.i64_ty().into(), "i", false);
        self.b.build_store(ctr, self.i64_ty().const_zero()).unwrap();
        let head = self.block("fs.head");
        let bb = self.block("fs.body");
        let cont = self.block("fs.next");
        let done = self.block("fs.end");
        self.br(head);
        self.b.position_at_end(head);
        let iv = self.b.build_load(self.i64_ty(), ctr, "").unwrap().into_int_value();
        let c = self.b.build_int_compare(IntPredicate::ULT, iv, len, "").unwrap();
        self.b.build_conditional_branch(c, bb, done).unwrap();
        self.b.position_at_end(bb);
        if let Some(l) = index {
            self.assign_local(l, Val::borrowed(iv.into()), true)?;
        }
        if let Some(l) = elem {
            let ep = if is_array {
                let at = self.llty(seq.ty);
                unsafe { self.b.build_in_bounds_gep(at, data, &[self.i64_ty().const_zero(), iv], "") }.unwrap()
            } else {
                unsafe { self.b.build_in_bounds_gep(elt, data, &[iv], "") }.unwrap()
            };
            let v = self.b.build_load(elt, ep, "").unwrap();
            self.assign_local(l, Val::borrowed(v), true)?;
        }
        self.loop_body(label, done, cont, body, &[index, elem])?;
        self.b.position_at_end(cont);
        let iv = self.b.build_load(self.i64_ty(), ctr, "").unwrap().into_int_value();
        let n = self.b.build_int_add(iv, self.i64_ty().const_int(1, false), "").unwrap();
        self.b.build_store(ctr, n).unwrap();
        self.b.build_unconditional_branch(head).unwrap();
        self.b.position_at_end(done);
        Ok(())
    }

    fn for_string(&mut self, label: Label, index: Option<LocalId>, ch: Option<LocalId>, string: &Expr, body: &Block) -> R<()> {
        let sv = self.expr(string)?;
        let tmp = self.scoped_slot(string.ty, sv);
        let s = self.b.build_load(self.ptr_ty(), tmp, "").unwrap();
        let len = self.call_rt_v("zell_string_len", &[s.into()]).into_int_value();
        let ctr = self.entry_alloca(self.i64_ty().into(), "i", false);
        let width = self.entry_alloca(self.i64_ty().into(), "w", false);
        self.b.build_store(ctr, self.i64_ty().const_zero()).unwrap();
        let head = self.block("fstr.head");
        let bb = self.block("fstr.body");
        let cont = self.block("fstr.next");
        let done = self.block("fstr.end");
        self.br(head);
        self.b.position_at_end(head);
        let iv = self.b.build_load(self.i64_ty(), ctr, "").unwrap().into_int_value();
        let c = self.b.build_int_compare(IntPredicate::SLT, iv, len, "").unwrap();
        self.b.build_conditional_branch(c, bb, done).unwrap();
        self.b.position_at_end(bb);
        let r = self.call_rt_v("zell_string_decode_rune", &[s.into(), iv.into(), width.into()]);
        if let Some(l) = index {
            self.assign_local(l, Val::borrowed(iv.into()), true)?;
        }
        if let Some(l) = ch {
            self.assign_local(l, Val::borrowed(r), true)?;
        }
        self.loop_body(label, done, cont, body, &[index, ch])?;
        self.b.position_at_end(cont);
        let iv = self.b.build_load(self.i64_ty(), ctr, "").unwrap().into_int_value();
        let w = self.b.build_load(self.i64_ty(), width, "").unwrap().into_int_value();
        let n = self.b.build_int_add(iv, w, "").unwrap();
        self.b.build_store(ctr, n).unwrap();
        self.b.build_unconditional_branch(head).unwrap();
        self.b.position_at_end(done);
        Ok(())
    }

    fn for_map(&mut self, label: Label, key: Option<LocalId>, value: Option<LocalId>, map: &Expr, body: &Block) -> R<()> {
        let (kt, vt) = self.map_kv(map.ty);
        let mv = self.expr(map)?;
        let tmp = self.scoped_slot(map.ty, mv);
        let m = self.b.build_load(self.ptr_ty(), tmp, "").unwrap();
        let it = self.entry_alloca(self.i64_ty().array_type(4).into(), "it", false);
        self.call_rt("zell_map_iter_init", &[it.into(), m.into()]);
        let kp = self.entry_alloca(self.ptr_ty().into(), "kp", false);
        let vp = self.entry_alloca(self.ptr_ty().into(), "vp", false);
        let head = self.block("fm.head");
        let bb = self.block("fm.body");
        let done = self.block("fm.end");
        self.br(head);
        self.b.position_at_end(head);
        let ok = self.call_rt_v("zell_map_iter_next", &[it.into(), kp.into(), vp.into()]).into_int_value();
        self.b.build_conditional_branch(ok, bb, done).unwrap();
        self.b.position_at_end(bb);
        if let Some(l) = key {
            let p = self.b.build_load(self.ptr_ty(), kp, "").unwrap().into_pointer_value();
            let lt = self.llty(kt);
            let k = self.b.build_load(lt, p, "").unwrap();
            self.assign_local(l, Val::borrowed(k), true)?;
        }
        if let Some(l) = value {
            let p = self.b.build_load(self.ptr_ty(), vp, "").unwrap().into_pointer_value();
            let lt = self.llty(vt);
            let v = self.b.build_load(lt, p, "").unwrap();
            self.assign_local(l, Val::borrowed(v), true)?;
        }
        self.loop_body(label, done, head, body, &[key, value])?;
        self.b.position_at_end(done);
        Ok(())
    }

    fn switch(&mut self, sw: &Switch) -> R<()> {
        let subj = match &sw.subject {
            Some((l, e)) => {
                let v = self.expr(e)?;
                self.assign_local(*l, v, true)?;
                Some((*l, e.ty))
            }
            None => None,
        };
        let end = self.block("sw.end");
        let depth = self.depth();
        self.fs_mut().loops.push(LoopTarget { label: sw.label, brk: end, cont: None, depth });
        let r = (|| -> R<()> {
            for case in &sw.cases {
                let body = self.block("case.body");
                for p in &case.patterns {
                    let next = self.block("case.next");
                    self.pattern(p, subj, body, next)?;
                    self.b.position_at_end(next);
                }
                // No pattern matched: `next` continues with the following case.
                let after = self.b.get_insert_block().unwrap();
                self.b.position_at_end(body);
                let cd = self.depth();
                self.push_scope();
                for p in &case.patterns {
                    match p {
                        Pattern::Variant { bindings, .. } => {
                            for b in bindings.iter().flatten() {
                                self.scope_local(*b);
                            }
                        }
                        Pattern::Type { binding: Some(b), .. } => self.scope_local(*b),
                        _ => {}
                    }
                }
                let known = self.switch_known_dyn(sw, case);
                if let Some(k) = known {
                    self.fs_mut().known_dyn.push(k);
                }
                let r = self.block_stmts(&case.body);
                if known.is_some() {
                    self.fs_mut().known_dyn.pop();
                }
                r?;
                self.pop_scope(cd);
                self.br(end);
                self.b.position_at_end(after);
            }
            if let Some(d) = &sw.default {
                self.scoped_block(d)?;
            }
            self.br(end);
            Ok(())
        })();
        self.fs_mut().loops.pop();
        r?;
        self.b.position_at_end(end);
        Ok(())
    }

    /// Devirtualisation: `case T x:` (a single type pattern) of a switch on
    /// an interface local that is never reassigned fixes that local's
    /// dynamic type to `T` inside the arm.
    fn switch_known_dyn(&self, sw: &Switch, case: &crate::hir::Case) -> Option<(LocalId, TypeId)> {
        let [Pattern::Type { ty, .. }] = case.patterns.as_slice() else { return None };
        let (_, e) = sw.subject.as_ref()?;
        let ExprKind::Local(x) = e.kind else { return None };
        if !matches!(self.ty(e.ty), Type::Interface(_)) || self.fs().locals.get(x.0 as usize).copied().flatten().is_none_or(|s| s.cell) {
            return None;
        }
        let dyn_ok = match self.ty(*ty) {
            Type::Struct(_) | Type::Enum(_) => true,
            Type::Pointer(p) => matches!(self.ty(*p), Type::Struct(_)),
            _ => false,
        };
        if !dyn_ok {
            return None;
        }
        let f = self.fs().hir?;
        let mut assigned = false;
        crate::hir::ownership::walk_function(self.prog, f, &mut |ex, pos| {
            if pos == crate::hir::ownership::Pos::Target {
                if let ExprKind::Local(l) = ex.kind {
                    assigned |= l == x;
                }
            }
        });
        (!assigned).then_some((x, *ty))
    }

    /// Tests pattern `p`; on a match binds and branches to `body`, else to
    /// `next`.
    fn pattern(&mut self, p: &Pattern, subj: Option<(LocalId, TypeId)>, body: BasicBlock<'ctx>, next: BasicBlock<'ctx>) -> R<()> {
        match p {
            Pattern::Cond(e) => {
                let c = self.cond_expr(e)?;
                self.b.build_conditional_branch(c, body, next).unwrap();
            }
            Pattern::Variant { variant, bindings } => {
                let Some((l, st)) = subj else { return unsupported("variant pattern without subject") };
                let Type::Enum(d) = self.ty(st).clone() else { return unsupported("variant pattern on a non-enum") };
                let sp = self.local_addr(l);
                let plain = self.prog.enums[d.0 as usize].is_plain;
                let tag = self.b.build_load(self.ctx.i32_type(), sp, "tag").unwrap().into_int_value();
                let c = self.b.build_int_compare(IntPredicate::EQ, tag, self.ctx.i32_type().const_int(*variant as u64, false), "").unwrap();
                let bind = self.block("case.bind");
                self.b.build_conditional_branch(c, bind, next).unwrap();
                self.b.position_at_end(bind);
                if !plain {
                    let et = self.enum_llty(d.0);
                    let vt = self.variant_llty(d.0, *variant);
                    let pp = self.b.build_struct_gep(et, sp, 1, "").unwrap();
                    let fts: Vec<TypeId> = self.prog.enums[d.0 as usize].variants[*variant as usize].fields.iter().map(|f| f.1).collect();
                    for (i, bl) in bindings.iter().enumerate() {
                        if let Some(bl) = bl {
                            let fp = self.b.build_struct_gep(vt, pp, i as u32, "").unwrap();
                            let ft = self.llty(fts[i]);
                            let v = self.b.build_load(ft, fp, "").unwrap();
                            self.assign_local(*bl, Val::borrowed(v), true)?;
                        }
                    }
                }
                self.b.build_unconditional_branch(body).unwrap();
            }
            Pattern::Type { ty, binding } => {
                let Some((l, st)) = subj else { return unsupported("type pattern without subject") };
                let sp = self.local_addr(l);
                let lt = self.llty(st);
                let sv = self.b.build_load(lt, sp, "").unwrap();
                let (m, dp) = self.type_test(st, sv, *ty)?;
                let bind = self.block("case.bind");
                self.b.build_conditional_branch(m, bind, next).unwrap();
                self.b.position_at_end(bind);
                if let Some(bl) = binding {
                    let bt = self.llty(*ty);
                    let v = self.b.build_load(bt, dp, "").unwrap();
                    self.assign_local(*bl, Val::borrowed(v), true)?;
                }
                self.b.build_unconditional_branch(body).unwrap();
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // return and errors
    // ------------------------------------------------------------------

    /// The value of `return e` in a borrow-returning function
    /// (`hir::borrow::borrow_returning`): a parameter path or a struct
    /// literal of parameter paths, whose references are copied without a
    /// retain (the caller holds the argument values they belong to).
    fn borrowed_return(&mut self, e: &Expr) -> R<BasicValueEnum<'ctx>> {
        if let ExprKind::StructLit { struct_id, fields } = &e.kind {
            let st = self.struct_llty(*struct_id);
            let mut agg: BasicValueEnum = st.const_zero().into();
            for (idx, fe) in fields {
                let v = self.borrowed_part(fe)?;
                agg = self.b.build_insert_value(agg.into_struct_value(), v, *idx, "").unwrap().into_struct_value().into();
            }
            return Ok(agg);
        }
        self.borrowed_part(e)
    }

    fn borrowed_part(&mut self, e: &Expr) -> R<BasicValueEnum<'ctx>> {
        let v = self.expr(e)?;
        if v.owned && self.needs_rc(e.ty) {
            return unsupported("internal: owned value in a borrowed return");
        }
        Ok(v.get())
    }

    fn ret_info(&self) -> R<(PointerValue<'ctx>, TypeId)> {
        match self.fs().ret_slot {
            Some(r) => Ok(r),
            None => unsupported("returning a value from a void function"),
        }
    }

    /// Stores error `err` (owned) as the function's failure result and
    /// jumps to the exit.
    fn return_error(&mut self, err: BasicValueEnum<'ctx>) -> R<()> {
        let (p, t) = self.ret_info()?;
        let Type::Fallible(inner) = self.ty(t).clone() else { return unsupported("error return from a non-fallible function") };
        if *self.ty(inner) == Type::Void {
            self.b.build_store(p, err).unwrap();
        } else {
            let st = self.llty(t).into_struct_type();
            let s = self.b.build_insert_value(st.const_zero(), err, 1, "").unwrap();
            self.b.build_store(p, s).unwrap();
        }
        self.exit_cleanups(0, false);
        let exit = self.fs().exit.unwrap();
        self.jump(exit);
        Ok(())
    }

    pub(crate) fn emit_return(&mut self, rv: &ReturnValue) -> R<()> {
        let exit = self.fs().exit.unwrap();
        match rv {
            ReturnValue::Void => {
                if let Some((p, t)) = self.fs().ret_slot {
                    let z = self.zero(t);
                    self.b.build_store(p, z).unwrap();
                }
            }
            ReturnValue::Value(e) => {
                let (p, t) = self.ret_info()?;
                let v = match self.fs().hir.and_then(|f| crate::hir::ownership::moved_return(self.prog, f, e)) {
                    // `return x` of a local with a destructor: moved out,
                    // its drop flag cleared so the exit does not drop it.
                    Some(l) => {
                        let v = self.expr(e)?.get();
                        if let Some(flag) = self.fs().flags.get(l.0 as usize).copied().flatten() {
                            self.b.build_store(flag, self.ctx.bool_type().const_zero()).unwrap();
                        }
                        v
                    }
                    None if self.fs().borrow_ret => self.borrowed_return(e)?,
                    None => self.expr_owned(e)?,
                };
                if let Type::Fallible(inner) = self.ty(t).clone() {
                    if *self.ty(inner) == Type::Void {
                        self.b.build_store(p, self.ptr_ty().const_null()).unwrap();
                    } else {
                        let st = self.llty(t).into_struct_type();
                        let s = self.b.build_insert_value(st.const_zero(), v, 0, "").unwrap();
                        self.b.build_store(p, s).unwrap();
                    }
                } else {
                    self.b.build_store(p, v).unwrap();
                }
            }
            ReturnValue::Error(e) => {
                let v = self.expr_owned(e)?;
                let v = self.trace_created(v, e.span.start);
                return self.return_error(v);
            }
        }
        self.exit_cleanups(0, false);
        self.jump(exit);
        Ok(())
    }

    /// Splits an owned `T!` into (value or None, error).
    fn split_fallible(&mut self, t: TypeId, v: BasicValueEnum<'ctx>) -> (Option<BasicValueEnum<'ctx>>, PointerValue<'ctx>) {
        let Type::Fallible(inner) = self.ty(t).clone() else { return (Some(v), self.ptr_ty().const_null()) };
        if *self.ty(inner) == Type::Void {
            (None, v.into_pointer_value())
        } else {
            let s = v.into_struct_value();
            (Some(self.b.build_extract_value(s, 0, "").unwrap()), self.b.build_extract_value(s, 1, "err").unwrap().into_pointer_value())
        }
    }

    /// `value?` / `value?(ctx)`.
    pub(crate) fn propagate(&mut self, value: &Expr, ctx: Option<&Expr>, span: crate::span::Span) -> R<Val<'ctx>> {
        let fv = self.expr_owned(value)?;
        let (v, err) = self.split_fallible(value.ty, fv);
        let fail = self.block("prop.fail");
        let ok = self.block("prop.ok");
        let isnull = self.b.build_is_null(err, "").unwrap();
        self.b.build_conditional_branch(isnull, ok, fail).unwrap();
        self.b.position_at_end(fail);
        let mut e: BasicValueEnum = err.into();
        let d = self.depth();
        let at = span.end.saturating_sub(1);
        match ctx {
            Some(c) => {
                let cv = self.expr(c)?;
                e = self.trace_returned(e, at, Some(cv.get()));
                self.drop_val(c.ty, cv);
            }
            None => e = self.trace_returned(e, at, None),
        }
        match self.fs().try_targets.last().copied() {
            Some(t) => {
                self.exit_cleanups(t.depth, true);
                self.b.build_store(t.slot, e).unwrap();
                self.jump(t.catch);
            }
            None => self.return_error(e)?,
        }
        // Temporaries of the failure path were released by the return.
        self.forget_from(d);
        self.b.position_at_end(ok);
        Ok(match v {
            Some(v) => Val::owned(v),
            None => Val::void(),
        })
    }

    /// Block `try { body } catch err { handler }`: `?` inside `body` stores
    /// the error in the target's slot and jumps to the handler.
    fn try_block(&mut self, body: &Block, err: Option<LocalId>, handler: &Block) -> R<()> {
        let catch = self.block("trys.catch");
        let end = self.block("trys.end");
        let slot = self.entry_alloca(self.ptr_ty().into(), "trys.err", false);
        let depth = self.depth();
        self.fs_mut().try_targets.push(TryTarget { catch, slot, depth });
        let r = self.scoped_block(body);
        self.fs_mut().try_targets.pop();
        r?;
        self.br(end);
        self.b.position_at_end(catch);
        let e = self.b.build_load(self.ptr_ty(), slot, "err").unwrap();
        let hd = self.depth();
        self.push_scope();
        match err {
            Some(l) => {
                self.assign_local(l, Val::owned(e), true)?;
                self.scope_local(l);
            }
            None => {
                self.call_rt("zell_release", &[e.into()]);
            }
        }
        self.block_stmts(handler)?;
        self.pop_scope(hd);
        self.br(end);
        self.b.position_at_end(end);
        Ok(())
    }

    pub(crate) fn try_catch(&mut self, te: &Expr, value: &Expr, err: Option<LocalId>, handler: &Block, result_used: bool) -> R<Val<'ctx>> {
        let fv = self.expr_owned(value)?;
        let (v, e) = self.split_fallible(value.ty, fv);
        let res = match (result_used, v) {
            (true, Some(v)) => {
                let p = self.entry_alloca(v.get_type(), "try", false);
                self.b.build_store(p, v.get_type().const_zero()).unwrap();
                Some(p)
            }
            _ => None,
        };
        let fail = self.block("catch");
        let ok = self.block("try.ok");
        let cont = self.block("try.end");
        let isnull = self.b.build_is_null(e, "").unwrap();
        self.b.build_conditional_branch(isnull, ok, fail).unwrap();
        self.b.position_at_end(fail);
        let hd = self.depth();
        self.push_scope();
        match err {
            Some(l) => {
                let e = self.trace_returned(e.into(), te.span.start, None);
                self.assign_local(l, Val::owned(e), true)?;
                self.scope_local(l);
            }
            None => {
                self.call_rt("zell_release", &[e.into()]);
            }
        }
        self.block_stmts(handler)?;
        self.pop_scope(hd);
        self.br(cont);
        self.b.position_at_end(ok);
        let inner = match self.ty(value.ty) {
            Type::Fallible(t) => *t,
            _ => value.ty,
        };
        if let Some(v) = v {
            match res {
                Some(p) => {
                    self.b.build_store(p, v).unwrap();
                }
                None => self.release_val(inner, v),
            }
        }
        self.br(cont);
        self.b.position_at_end(cont);
        Ok(match res {
            Some(p) => {
                let lt = self.llty(inner);
                Val::owned(self.b.build_load(lt, p, "").unwrap())
            }
            None => Val::void(),
        })
    }

    pub(crate) fn try_else(&mut self, ty: TypeId, value: &Expr, fallback: &TryFallback) -> R<Val<'ctx>> {
        let fv = self.expr_owned(value)?;
        let (v, e) = self.split_fallible(value.ty, fv);
        let res = self.llty_opt(ty).map(|t| self.entry_alloca(t, "tryelse", false));
        let fail = self.block("else");
        let ok = self.block("try.ok");
        let cont = self.block("try.end");
        let isnull = self.b.build_is_null(e, "").unwrap();
        self.b.build_conditional_branch(isnull, ok, fail).unwrap();
        self.b.position_at_end(fail);
        self.call_rt("zell_release", &[e.into()]);
        let fd = self.depth();
        match fallback {
            TryFallback::Value(x) => {
                let d = self.depth();
                let xv = self.expr(x)?;
                match (res, xv.v) {
                    (Some(p), Some(_)) => {
                        let o = self.to_owned(ty, xv);
                        self.b.build_store(p, o).unwrap();
                    }
                    _ => self.drop_val(x.ty, xv),
                }
                self.release_temps_from(d);
                self.br(cont);
            }
            TryFallback::Break(l) => {
                let t = self.target(*l)?;
                self.exit_cleanups(t.depth, true);
                self.jump(t.brk);
            }
            TryFallback::Continue(l) => {
                let t = self.target(*l)?;
                let Some(c) = t.cont else { return unsupported("continue to a switch") };
                self.exit_cleanups(t.depth, true);
                self.jump(c);
            }
            TryFallback::Return(rv) => self.emit_return(rv)?,
        }
        // The fallback's temporaries were released on its own path.
        self.forget_from(fd);
        self.b.position_at_end(ok);
        if let (Some(p), Some(v)) = (res, v) {
            self.b.build_store(p, v).unwrap();
        }
        self.br(cont);
        self.b.position_at_end(cont);
        Ok(match res {
            Some(p) => {
                let lt = self.llty(ty);
                Val::owned(self.b.build_load(lt, p, "").unwrap())
            }
            None => Val::void(),
        })
    }
}
