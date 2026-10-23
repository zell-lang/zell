//! `async` functions as LLVM switched-resume coroutines (ABI.md §10.3,
//! modelled on `runtime/tests/coro_lowering.ll`).
//!
//! An `async func f(params) T` is `void @f(params, ptr %res)` with
//! `%res -> { ptr task; T value }` ([`CodeGen::coro_res_ty`]). Ramp layout:
//! `entry` (allocas) -> `init` (`coro.id`, `coro.alloc`) -> `coro.alloc`
//! (frame from `zell_coro_frame_alloc`, skipped when LLVM proves the
//! coroutine never suspends) -> `coro.begin` (`coro.begin`, the caller's
//! context saved by `zell_task_get_ctx`, then zeroing of locals) -> body.
//! The coroutine makes **no task until its first real suspension**: the
//! body runs as part of the caller's current task, and an `await t` that
//! has to suspend first calls `zell_task_lazy` (which makes the task,
//! attaches it under the current task and makes it current), stores it
//! into `res.task` and registers `zell_coro_resume(hdl)` as a continuation
//! of `t`. `return` goes through the common exit block (defers, release of
//! locals) and then [`CodeGen::coro_return`]: with no task the result is
//! moved into `res.value` under a NULL `res.task` after `zell_coro_leave`
//! restored the caller's context (the synchronous fast path: no task, and
//! no frame unless the body has a suspend point); with a task it is
//! completed as before. Both branch to `cleanup` (frees the frame if one
//! was allocated; there is no final suspend) and `done` (`coro.end`, `ret
//! void`). Callers that await a direct call read `res.value` when
//! `res.task` is NULL ([`CodeGen::await_expr`]); callers that keep the
//! `Task<T>` materialise a completed one ([`CodeGen::coro_res_task`]).
//! Outside a coroutine (C `main`), `await` blocks with `zell_task_await`.

use super::{unsupported, CodeGen, Coro, Val, R};
use crate::hir::{self, Callee, Expr, ExprKind, FuncId};
use crate::types::{Type, TypeId};
use inkwell::llvm_sys::core as llc;
use inkwell::llvm_sys::prelude::{LLVMTypeRef, LLVMValueRef};
use inkwell::types::{AsTypeRef, StructType};
use inkwell::values::{AsValueRef, BasicValueEnum, IntValue, PointerValue};
use inkwell::IntPredicate;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Calls intrinsic `name` (non-overloaded) with raw arguments.
    fn intrinsic(&mut self, name: &str, args: &mut [LLVMValueRef]) -> LLVMValueRef {
        unsafe {
            // `llvm.coro.size.i64` is overloaded on its result type.
            let (base, mut tys): (&str, Vec<LLVMTypeRef>) = match name.strip_suffix(".i64") {
                Some(b) => (b, vec![self.i64_ty().as_type_ref()]),
                None => (name, vec![]),
            };
            let id = llc::LLVMLookupIntrinsicID(base.as_ptr() as *const _, base.len());
            assert!(id != 0, "codegen: unknown intrinsic {name}");
            let f = llc::LLVMGetIntrinsicDeclaration(self.module.as_mut_ptr(), id, tys.as_mut_ptr(), tys.len());
            let fty = llc::LLVMGlobalGetValueType(f);
            llc::LLVMBuildCall2(self.b.as_mut_ptr(), fty, f, args.as_mut_ptr(), args.len() as u32, c"".as_ptr())
        }
    }

    fn token_none(&self) -> LLVMValueRef {
        unsafe {
            let t: LLVMTypeRef = llc::LLVMTokenTypeInContext(self.ctx.raw());
            llc::LLVMConstNull(t)
        }
    }

    /// The result slot an async call writes: `{ ptr task; T value }`
    /// (`{ ptr task }` for `Task<void>`). `task` is the coroutine's
    /// `Task<T>` (+1) once it suspended, or NULL with the result (+1) in
    /// `value` when it completed synchronously.
    pub(crate) fn coro_res_ty(&mut self, ret: TypeId) -> StructType<'ctx> {
        let mut fs: Vec<inkwell::types::BasicTypeEnum<'ctx>> = vec![self.ptr_ty().into()];
        if let Some(t) = self.llty_opt(ret) {
            fs.push(t);
        }
        self.ctx.struct_type(&fs, false)
    }

    /// Address of `res.task` (`i == 0`) or `res.value` (`i == 1`).
    fn coro_res_field(&mut self, ret: TypeId, res: PointerValue<'ctx>, i: u32) -> PointerValue<'ctx> {
        let st = self.coro_res_ty(ret);
        self.b.build_struct_gep(st, res, i, "").unwrap()
    }

    /// Emits the ramp prologue of async function `f` into the init block.
    pub(crate) fn coro_prologue(&mut self, f: &hir::Function) -> R<()> {
        let body = self.b.get_insert_block().unwrap();
        let init = self.fs().init;
        let func = self.fs().func;
        self.b.position_at_end(init);
        let ret_size = self.i64_ty().const_int(self.size_of(f.ret), false);
        let ret_ops = self.value_ops(f.ret);
        let null = self.ptr_ty().const_null();
        let zero = self.ctx.i32_type().const_zero().as_value_ref();
        let id = self.intrinsic("llvm.coro.id", &mut [zero, null.as_value_ref(), null.as_value_ref(), null.as_value_ref()]);
        // The frame is allocated only when LLVM cannot prove that the
        // coroutine never suspends (`coro.alloc` folds to false then).
        let need = self.intrinsic("llvm.coro.alloc", &mut [id]);
        let alloc = self.block("coro.alloc");
        let begin = self.block("coro.begin");
        self.b.build_conditional_branch(unsafe { IntValue::new(need) }, alloc, begin).unwrap();
        self.b.position_at_end(alloc);
        let sz = self.intrinsic("llvm.coro.size.i64", &mut []);
        let mem = self.call_rt_v("zell_coro_frame_alloc", &[unsafe { IntValue::new(sz) }.into()]).into_pointer_value();
        self.b.build_unconditional_branch(begin).unwrap();
        self.b.position_at_end(begin);
        // Zeroing stores emitted later by `entry_alloca` follow this code.
        self.fs().init_b.position_at_end(begin);
        let memphi = self.b.build_phi(self.ptr_ty(), "coro.mem").unwrap();
        memphi.add_incoming(&[(&null, init), (&mem, alloc)]);
        let hdl = self.intrinsic("llvm.coro.begin", &mut [id, memphi.as_basic_value().as_value_ref()]);
        let hdl = unsafe { PointerValue::new(hdl) };
        // The coroutine's task (NULL until its first suspension) and the
        // caller's context (+1), put back when the body returns without
        // having suspended.
        let task_slot = self.entry_alloca(self.ptr_ty().into(), "coro.task", false);
        self.b.build_store(task_slot, null).unwrap();
        let ctx_slot = self.entry_alloca(self.ptr_ty().into(), "coro.ctx", false);
        let ctx0 = self.call_rt_v("zell_task_get_ctx", &[]);
        self.b.build_store(ctx_slot, ctx0).unwrap();
        let res = func.get_last_param().unwrap().into_pointer_value();
        let cleanup = self.block("coro.cleanup");
        let free = self.block("coro.free");
        let done = self.block("coro.done");
        self.b.position_at_end(cleanup);
        let fr = self.intrinsic("llvm.coro.free", &mut [id, hdl.as_value_ref()]);
        let frv = unsafe { PointerValue::new(fr) };
        let has = self.b.build_is_not_null(frv, "").unwrap();
        self.b.build_conditional_branch(has, free, done).unwrap();
        self.b.position_at_end(free);
        self.call_rt("zell_coro_frame_free", &[frv.into()]);
        self.b.build_unconditional_branch(done).unwrap();
        self.b.position_at_end(done);
        let fls = self.ctx.bool_type().const_zero().as_value_ref();
        let none = self.token_none();
        self.intrinsic("llvm.coro.end", &mut [hdl.as_value_ref(), fls, none]);
        self.b.build_return(None).unwrap();
        self.fs_mut().coro = Some(Coro { hdl, ret: f.ret, ret_size, ret_ops, task_slot, ctx_slot, res, cleanup, done });
        self.b.position_at_end(body);
        Ok(())
    }

    /// Leaves the coroutine with the result slot: completes the task if the
    /// body suspended, else hands the result to the caller through `res`.
    pub(crate) fn coro_return(&mut self) -> R<()> {
        let co = self.fs().coro.unwrap();
        let (slot, rt) = match self.fs().ret_slot {
            Some((p, t)) => (p, Some(t)),
            None => (self.ptr_ty().const_null(), None),
        };
        let own = self.b.build_load(self.ptr_ty(), co.task_slot, "coro.own").unwrap().into_pointer_value();
        let sync = self.b.build_is_null(own, "").unwrap();
        let fin_sync = self.block("coro.fin.sync");
        let fin_task = self.block("coro.fin.task");
        self.b.build_conditional_branch(sync, fin_sync, fin_task).unwrap();
        // Never suspended: the caller's context again, the result (moved,
        // no retain) and a NULL task into the caller's slot.
        self.b.position_at_end(fin_sync);
        let ctx0 = self.b.build_load(self.ptr_ty(), co.ctx_slot, "").unwrap();
        self.call_rt("zell_coro_leave", &[ctx0.into()]);
        let tp = self.coro_res_field(co.ret, co.res, 0);
        self.b.build_store(tp, self.ptr_ty().const_null()).unwrap();
        if let Some(t) = rt {
            let lt = self.llty(t);
            let v = self.b.build_load(lt, slot, "").unwrap();
            let vp = self.coro_res_field(co.ret, co.res, 1);
            self.b.build_store(vp, v).unwrap();
        }
        self.b.build_unconditional_branch(co.cleanup).unwrap();
        // Suspended at least once: publish the result through the task
        // (which retains it) and drop the frame's reference.
        self.b.position_at_end(fin_task);
        self.call_rt("zell_task_complete", &[own.into(), slot.into()]);
        if let Some(t) = rt {
            if self.needs_rc(t) {
                let lt = self.llty(t);
                let v = self.b.build_load(lt, slot, "").unwrap();
                self.release_val(t, v);
            }
        }
        self.call_rt("zell_release", &[own.into()]);
        self.b.build_unconditional_branch(co.cleanup).unwrap();
        Ok(())
    }

    /// Waits for task `t`; returns its (borrowed) result slot.
    fn await_slot(&mut self, t: PointerValue<'ctx>) -> PointerValue<'ctx> {
        let Some(co) = self.fs().coro else {
            return self.call_rt_v("zell_task_await", &[t.into()]).into_pointer_value();
        };
        let ready = self.block("await.ready");
        let try_ = self.block("await.try");
        let publish = self.block("await.publish");
        let save = self.block("await.save");
        let susp = self.block("await.suspend");
        let done = self.call_rt_v("zell_task_is_done", &[t.into()]).into_int_value();
        self.b.build_conditional_branch(done, ready, try_).unwrap();
        // The coroutine's own task, made now if this is its first
        // suspension; the caller then gets it through `res.task`.
        self.b.position_at_end(try_);
        let cur = self.b.build_load(self.ptr_ty(), co.task_slot, "").unwrap().into_pointer_value();
        let ctx0 = self.b.build_load(self.ptr_ty(), co.ctx_slot, "").unwrap();
        let own = self.call_rt_v("zell_task_lazy", &[cur.into(), co.ret_size.into(), co.ret_ops.into(), ctx0.into()]).into_pointer_value();
        self.b.build_store(co.task_slot, own).unwrap();
        let first = self.b.build_is_null(cur, "").unwrap();
        self.b.build_conditional_branch(first, publish, save).unwrap();
        self.b.position_at_end(publish);
        let tp = self.coro_res_field(co.ret, co.res, 0);
        self.b.build_store(tp, own).unwrap();
        self.b.build_unconditional_branch(save).unwrap();
        self.b.position_at_end(save);
        self.dbga_suspend();
        let sv = self.intrinsic("llvm.coro.save", &mut [co.hdl.as_value_ref()]);
        let resume = self.rt("zell_coro_resume").as_global_value().as_pointer_value();
        let reg = self.call_rt_v("zell_task_add_continuation", &[t.into(), resume.into(), co.hdl.into()]).into_int_value();
        self.b.build_conditional_branch(reg, susp, ready).unwrap();
        self.b.position_at_end(susp);
        let fls = self.ctx.bool_type().const_zero().as_value_ref();
        let s = self.intrinsic("llvm.coro.suspend", &mut [sv, fls]);
        let sv = unsafe { IntValue::new(s) };
        let i8t = self.ctx.i8_type();
        self.b.build_switch(sv, co.done, &[(i8t.const_zero(), ready), (i8t.const_int(1, false), co.cleanup)]).unwrap();
        self.b.position_at_end(ready);
        self.dbga_resume();
        self.call_rt_v("zell_task_result", &[t.into()]).into_pointer_value()
    }

    /// Loads an owned copy of a `T` result from `slot`.
    fn load_result(&mut self, t: TypeId, slot: PointerValue<'ctx>) -> Option<BasicValueEnum<'ctx>> {
        let lt = self.llty_opt(t)?;
        let v = self.b.build_load(lt, slot, "").unwrap();
        self.retain_val(t, v);
        Some(v)
    }

    /// The `Task<T>` (+1) of a direct async call that wrote `res`: the task
    /// it stored, or a completed one holding the moved result.
    pub(crate) fn coro_res_task(&mut self, ret: TypeId, res: PointerValue<'ctx>) -> PointerValue<'ctx> {
        let tp = self.coro_res_field(ret, res, 0);
        let t = self.b.build_load(self.ptr_ty(), tp, "res.task").unwrap().into_pointer_value();
        let sync = self.b.build_is_null(t, "").unwrap();
        let from = self.b.get_insert_block().unwrap();
        let mat = self.block("res.materialize");
        let join = self.block("res.join");
        self.b.build_conditional_branch(sync, mat, join).unwrap();
        self.b.position_at_end(mat);
        let size = self.i64_ty().const_int(self.size_of(ret), false);
        let ops = self.value_ops(ret);
        let vp: PointerValue<'ctx> = if self.llty_opt(ret).is_some() { self.coro_res_field(ret, res, 1) } else { self.ptr_ty().const_null() };
        let c = self.call_rt_v("zell_task_completed", &[size.into(), ops.into(), vp.into()]).into_pointer_value();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let phi = self.b.build_phi(self.ptr_ty(), "task").unwrap();
        phi.add_incoming(&[(&t, from), (&c, mat)]);
        phi.as_basic_value().into_pointer_value()
    }

    /// `await f(args)` of an async function: the result straight from the
    /// call's slot when `f` completed synchronously, else the usual await
    /// on the task it returned.
    fn await_direct_call(&mut self, ty: TypeId, fid: FuncId, args: &[Expr]) -> R<Val<'ctx>> {
        let res = self.call_async_direct(fid, args)?;
        let tp = self.coro_res_field(ty, res, 0);
        let t = self.b.build_load(self.ptr_ty(), tp, "res.task").unwrap().into_pointer_value();
        let sync = self.b.build_is_null(t, "").unwrap();
        let fast = self.block("await.fast");
        let slow = self.block("await.slow");
        let join = self.block("await.join");
        self.b.build_conditional_branch(sync, fast, slow).unwrap();
        self.b.position_at_end(fast);
        let fv = match self.llty_opt(ty) {
            Some(lt) => {
                let vp = self.coro_res_field(ty, res, 1);
                Some(self.b.build_load(lt, vp, "").unwrap())
            }
            None => None,
        };
        let fast_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(slow);
        let slot = self.await_slot(t);
        let sv = self.load_result(ty, slot);
        self.call_rt("zell_release", &[t.into()]);
        let slow_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        Ok(match (fv, sv) {
            (Some(a), Some(b)) => {
                let phi = self.b.build_phi(a.get_type(), "await.val").unwrap();
                phi.add_incoming(&[(&a, fast_end), (&b, slow_end)]);
                Val::owned(phi.as_basic_value())
            }
            _ => Val::void(),
        })
    }

    /// `await t` with result type `ty`.
    pub(crate) fn await_expr(&mut self, ty: TypeId, t: &Expr) -> R<Val<'ctx>> {
        if let ExprKind::TaskAll(tasks) = &t.kind {
            return self.await_all(ty, tasks);
        }
        if let ExprKind::Builtin { op: crate::hir::Builtin::TaskAny, args } = &t.kind {
            return self.await_any(ty, args);
        }
        if let ExprKind::Call { callee: Callee::Func(fid), args } = &t.kind {
            if self.prog.funcs[fid.0 as usize].is_async {
                return self.await_direct_call(ty, *fid, args);
            }
        }
        let tv = self.expr(t)?;
        let tp = tv.get().into_pointer_value();
        let slot = self.await_slot(tp);
        let r = self.load_result(ty, slot);
        self.drop_val(t.ty, tv);
        Ok(match r {
            Some(v) => Val::owned(v),
            None => Val::void(),
        })
    }

    /// `await task.Any(t1, ..., tn)`: `zell_task_any` yields the index of
    /// the first task to complete; its result is copied out (retained).
    fn await_any(&mut self, ty: TypeId, tasks: &[Expr]) -> R<Val<'ctx>> {
        let n = tasks.len() as u32;
        let arr_ty = self.ptr_ty().array_type(n);
        let arr = self.entry_alloca(arr_ty.into(), "any.tasks", false);
        let mut tvs = Vec::new();
        let d = self.depth();
        for (i, t) in tasks.iter().enumerate() {
            let v = self.expr(t)?;
            self.hold(t.ty, v);
            let p = unsafe { self.b.build_in_bounds_gep(arr_ty, arr, &[self.i64_ty().const_zero(), self.i64_ty().const_int(i as u64, false)], "") }.unwrap();
            self.b.build_store(p, v.get()).unwrap();
            tvs.push(v);
        }
        self.forget_from(d);
        let any = self.call_rt_v("zell_task_any", &[arr.into(), self.i64_ty().const_int(n as u64, false).into()]).into_pointer_value();
        let slot = self.await_slot(any);
        let idx = self.b.build_load(self.i64_ty(), slot, "any.idx").unwrap().into_int_value();
        self.call_rt("zell_release", &[any.into()]);
        let p = unsafe { self.b.build_in_bounds_gep(arr_ty, arr, &[self.i64_ty().const_zero(), idx], "") }.unwrap();
        let winner = self.b.build_load(self.ptr_ty(), p, "").unwrap().into_pointer_value();
        let rslot = self.call_rt_v("zell_task_result", &[winner.into()]).into_pointer_value();
        let r = self.load_result(ty, rslot);
        for (t, tv) in tasks.iter().zip(tvs) {
            self.drop_val(t.ty, tv);
        }
        Ok(match r {
            Some(v) => Val::owned(v),
            None => Val::void(),
        })
    }

    /// `await task.All(t1, ..., tn)`: awaits each task in order and builds
    /// the tuple (or `(..)!`, failing with the first error in order).
    fn await_all(&mut self, ty: TypeId, tasks: &[Expr]) -> R<Val<'ctx>> {
        let mut tvs = Vec::new();
        let d = self.depth();
        for t in tasks {
            let v = self.expr(t)?;
            self.hold(t.ty, v);
            tvs.push(v);
        }
        self.forget_from(d);
        let mut results = Vec::new();
        for (t, tv) in tasks.iter().zip(&tvs) {
            let Type::Task(rt) = self.ty(t.ty).clone() else { return unsupported("task.All over a non-task") };
            let slot = self.await_slot(tv.get().into_pointer_value());
            results.push((rt, self.load_result(rt, slot)));
        }
        for (t, tv) in tasks.iter().zip(tvs) {
            self.drop_val(t.ty, tv);
        }
        let (tuple_ty, fallible) = match self.ty(ty) {
            Type::Fallible(inner) => (*inner, true),
            _ => (ty, false),
        };
        let st = self.llty(tuple_ty).into_struct_type();
        let mut agg: BasicValueEnum = st.const_zero().into();
        let mut err = self.ptr_ty().const_null();
        let mut idx = 0u32;
        for (rt, v) in results {
            let Some(v) = v else { continue };
            let (val, e) = match self.ty(rt) {
                Type::Fallible(inner) if fallible => {
                    if *self.ty(*inner) == Type::Void {
                        (None, Some(v.into_pointer_value()))
                    } else {
                        let s = v.into_struct_value();
                        (Some(self.b.build_extract_value(s, 0, "").unwrap()), Some(self.b.build_extract_value(s, 1, "").unwrap().into_pointer_value()))
                    }
                }
                _ => (Some(v), None),
            };
            if let Some(e) = e {
                let first = self.b.build_is_null(err, "").unwrap();
                let keep = self.b.build_select(first, e, err, "").unwrap().into_pointer_value();
                let drop = self.b.build_select(first, self.ptr_ty().const_null(), e, "").unwrap();
                self.call_rt("zell_release", &[drop.into()]);
                err = keep;
            }
            if let Some(val) = val {
                if idx < st.count_fields() {
                    agg = self.b.build_insert_value(agg.into_struct_value(), val, idx, "").unwrap().into_struct_value().into();
                }
                idx += 1;
            }
        }
        if !fallible {
            return Ok(Val::owned(agg));
        }
        // On failure drop the partial values and return {zero, err}.
        let failed = self.b.build_is_not_null(err, "").unwrap();
        let rel = self.block("all.fail");
        let join = self.block("all.join");
        self.b.build_conditional_branch(failed, rel, join).unwrap();
        self.b.position_at_end(rel);
        self.release_val(tuple_ty, agg);
        self.b.build_unconditional_branch(join).unwrap();
        self.b.position_at_end(join);
        let z = st.const_zero();
        let tuple = self.b.build_select(failed, BasicValueEnum::from(z), agg, "").unwrap();
        let ft = self.llty(ty).into_struct_type();
        let s = self.b.build_insert_value(ft.get_undef(), tuple, 0, "").unwrap();
        let s = self.b.build_insert_value(s, err, 1, "").unwrap();
        let _ = IntPredicate::EQ;
        Ok(Val::owned(s.into_struct_value().into()))
    }
}
