//! Unwinding of synchronous frames on a panic (DESIGN.md "Паника",
//! runtime/ABI.md 8.1 item 4a), code generation side.
//!
//! A synchronous function (a plain `func`, a closure, a never-suspending
//! `async func` lowered plain) emits every call that may panic while it has
//! something to clean up as an `invoke` whose landing pad releases the held
//! temporaries live at that call (the cleanup stack, `cleanup.rs`) and then
//! branches to the function's shared *unwind block*: the armed defers, the
//! locals (as at the normal exit), the saved context of a plain async
//! function, `resume`. The optimiser sees the exact control flow of a panic:
//! it keeps values in registers, inlines, and turns the `invoke` of a callee
//! that cannot panic into a plain call, dropping the landing pad.
//!
//! No DWARF or SEH unwinder is involved: after optimisation
//! [`super::unwind_lower`] rewrites the landing pads of every function that
//! still has `invoke`s into a setjmp-based frame on the thread's frame chain.
//!
//! Releases never get a landing pad (a `Drop` that panics during one
//! leaves the frames below it to the frame chain without their cleanup, as
//! a release run twice would be worse). Coroutines keep plain calls and
//! their own destroy path (`async_.rs`); they carry a personality of their
//! own so that the inliner never moves a landing pad into them.

use super::{CodeGen, R};
use crate::hir::LocalId;
use inkwell::basic_block::BasicBlock;
use inkwell::llvm_sys::core as llc;
use inkwell::llvm_sys::prelude::LLVMValueRef;
use inkwell::types::{AsTypeRef, FunctionType};
use inkwell::values::{AsValueRef, BasicMetadataValueEnum, CallSiteValue, FunctionValue};

/// Personality of the functions with landing pads (never called: the
/// landing pads are lowered away, `unwind_lower.rs`).
pub(crate) const PERSONALITY: &str = "zell.personality";
/// Personality of coroutines, different from [`PERSONALITY`] so that the
/// inliner refuses to inline a function with landing pads into them.
pub(crate) const PERSONALITY_CORO: &str = "zell.personality.coro";

/// Runtime functions that never panic and never run Zell code, called
/// without a landing pad (the releases: see the module doc).
const NO_PANIC_PREFIXES: &[&str] = &["zell_retain", "zell_release", "zell_ptr_retain", "zell_ptr_release", "zell_dbg_", "zell_string_eq", "zell_string_cmp", "zell_unwind_resume"];

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// The personality function `name` (declared on first use).
    pub(crate) fn personality_fn(&mut self, name: &str) -> FunctionValue<'ctx> {
        match self.module.get_function(name) {
            Some(f) => f,
            None => self.module.add_function(name, self.ctx.i32_type().fn_type(&[], true), Some(inkwell::module::Linkage::External)),
        }
    }

    /// Sets up landing pads for the synchronous function being emitted
    /// (after its local slots exist): whether any panic must clean up.
    pub(crate) fn setup_lpads(&mut self, f: &crate::hir::Function) {
        let mut defers = false;
        crate::hir::ownership::walk_function_stmts(self.prog, f, &mut |s| {
            if matches!(s.kind, crate::hir::StmtKind::Defer(_)) {
                defers = true;
            }
        });
        let locals = (0..f.locals.len()).any(|i| self.local_may_own(LocalId(i as u32)));
        let ctx = self.fs().plain.is_some_and(|(c, _)| c.is_some());
        let st = self.fs_mut();
        st.lpad_mode = true;
        st.needs_cleanup = defers || locals || ctx;
    }

    /// True if local `l` may hold something its release frees (mirrors
    /// `release_local`, before the stores of the body are known).
    fn local_may_own(&self, l: LocalId) -> bool {
        let (info, slot) = self.local_info(l);
        if self.is_interior_local(l) || slot.cell {
            return true;
        }
        if self.fs().borrowed.get(l.0 as usize).copied().unwrap_or(false) {
            return false;
        }
        if let (Some(_), crate::types::Type::Pointer(t)) = (slot.stack_obj, self.ty(info.ty).clone()) {
            return self.needs_rc(t);
        }
        self.fs().flags.get(l.0 as usize).copied().flatten().is_some() || self.needs_rc(info.ty)
    }

    /// The landing pad for a call emitted now, or None for a plain call.
    fn invoke_dest(&mut self) -> Option<BasicBlock<'ctx>> {
        let st = self.f.as_ref()?;
        if !st.lpad_mode || st.no_invoke > 0 {
            return None;
        }
        let temps: Vec<(crate::types::TypeId, inkwell::values::BasicValueEnum<'ctx>)> = st
            .cleanups
            .iter()
            .filter_map(|c| match c {
                super::Cleanup::Temp(t, x, _) => Some((*t, *x)),
                _ => None,
            })
            .collect();
        if !st.needs_cleanup && temps.is_empty() {
            return None;
        }
        if self.terminated_block() {
            return None;
        }
        let key: Vec<usize> = temps.iter().map(|(_, x)| x.as_value_ref() as usize).collect();
        if let Some(b) = self.fs().lpads.get(&key) {
            return Some(*b);
        }
        let pos = self.b.get_insert_block().unwrap();
        let lpad = self.block("lpad");
        self.b.position_at_end(lpad);
        let pers = self.personality_fn(PERSONALITY);
        unsafe { llc::LLVMSetPersonalityFn(self.fs().func.as_value_ref(), pers.as_value_ref()) };
        let lp_ty = self.ctx.struct_type(&[self.ptr_ty().into(), self.ctx.i32_type().into()], false);
        let lp = self.b.build_landing_pad(lp_ty, pers, &[], true, "lp").unwrap();
        self.fs_mut().no_invoke += 1;
        self.fs_mut().cold += 1;
        for (t, x) in temps.into_iter().rev() {
            self.release_val(t, x);
        }
        self.fs_mut().cold -= 1;
        self.fs_mut().no_invoke -= 1;
        let ub = match self.fs().unwind_bb {
            Some(b) => b,
            None => {
                let b = self.block("unwind");
                self.fs_mut().unwind_bb = Some(b);
                b
            }
        };
        let end = self.b.get_insert_block().unwrap();
        self.fs_mut().lpad_vals.push((lp, end));
        self.b.build_unconditional_branch(ub).unwrap();
        self.b.position_at_end(pos);
        self.fs_mut().lpads.insert(key, lpad);
        Some(lpad)
    }

    /// Emits a call of `callee` (of type `fty`): an `invoke` with the
    /// current landing pad when `may_panic` and there is something to clean
    /// up, else a plain call. Continues after it either way.
    pub(crate) fn call_site(&mut self, fty: FunctionType<'ctx>, callee: LLVMValueRef, args: &[BasicMetadataValueEnum<'ctx>], may_panic: bool) -> CallSiteValue<'ctx> {
        let lpad = if may_panic { self.invoke_dest() } else { None };
        let mut raw: Vec<LLVMValueRef> = args.iter().map(|a| a.as_value_ref()).collect();
        unsafe {
            let v = match lpad {
                None => llc::LLVMBuildCall2(self.b.as_mut_ptr(), fty.as_type_ref(), callee, raw.as_mut_ptr(), raw.len() as u32, c"".as_ptr()),
                Some(lp) => {
                    let cont = self.block("cont");
                    let v = llc::LLVMBuildInvoke2(self.b.as_mut_ptr(), fty.as_type_ref(), callee, raw.as_mut_ptr(), raw.len() as u32, cont.as_mut_ptr(), lp.as_mut_ptr(), c"".as_ptr());
                    self.b.position_at_end(cont);
                    v
                }
            };
            CallSiteValue::new(v)
        }
    }

    /// [`call_site`](Self::call_site) of a function, which may panic unless
    /// it is a runtime function that never does.
    pub(crate) fn call_fn(&mut self, f: FunctionValue<'ctx>, args: &[BasicMetadataValueEnum<'ctx>]) -> CallSiteValue<'ctx> {
        let name = f.get_name().to_string_lossy();
        let may = !(name.starts_with("llvm.") || NO_PANIC_PREFIXES.iter().any(|p| name.starts_with(p)));
        self.call_site(f.get_type(), f.as_value_ref(), args, may)
    }

    /// Runs `body` emitting plain calls only.
    pub(crate) fn without_invokes<T>(&mut self, body: impl FnOnce(&mut Self) -> T) -> T {
        if let Some(f) = self.f.as_mut() {
            f.no_invoke += 1;
        }
        let r = body(self);
        if let Some(f) = self.f.as_mut() {
            f.no_invoke -= 1;
        }
        r
    }

    /// Emits the shared unwind block, if a landing pad branches to it:
    /// the armed defers (each disarmed before it runs), the locals, the
    /// saved context, `resume`. `open` are the open-coded defers.
    pub(crate) fn emit_unwind_block(&mut self, open: Vec<(inkwell::values::PointerValue<'ctx>, *const crate::hir::Expr)>) -> R<()> {
        let Some(ub) = self.fs().unwind_bb else { return Ok(()) };
        self.b.position_at_end(ub);
        // The landing pad's value goes to `resume`, so that the optimiser
        // recognises (and removes) landing pads with nothing to clean up.
        let lp_ty = self.ctx.struct_type(&[self.ptr_ty().into(), self.ctx.i32_type().into()], false);
        let phi = self.b.build_phi(lp_ty, "lp").unwrap();
        for (v, b) in self.fs().lpad_vals.clone() {
            phi.add_incoming(&[(&v, b)]);
        }
        self.fs_mut().no_invoke += 1;
        self.fs_mut().cold += 1;
        self.fs_mut().open_defers = open;
        let r = self.run_defers();
        if r.is_ok() {
            self.release_locals();
            if let Some((Some(c), _)) = self.fs().plain {
                self.coro_leave_inline(c);
            }
        }
        self.fs_mut().cold -= 1;
        self.fs_mut().no_invoke -= 1;
        r?;
        self.b.build_resume(phi.as_basic_value()).unwrap();
        Ok(())
    }

    /// Releases the held temporaries that are live (their slots: the
    /// destroy path of a coroutine).
    pub(crate) fn release_temp_slots(&mut self) {
        let slots = self.fs().temp_slots.clone();
        for s in slots.into_iter().rev() {
            match s.flag {
                Some(fl) => {
                    let live = self.b.build_load(self.ctx.bool_type(), fl, "tmp.live").unwrap().into_int_value();
                    let rel = self.block("tmp.drop");
                    let done = self.block("tmp.done");
                    self.b.build_conditional_branch(live, rel, done).unwrap();
                    self.b.position_at_end(rel);
                    self.b.build_store(fl, self.ctx.bool_type().const_zero()).unwrap();
                    self.rc_slot(s.ty, s.ptr, false);
                    self.b.build_unconditional_branch(done).unwrap();
                    self.b.position_at_end(done);
                }
                None => {
                    let ty = self.llty(s.ty);
                    let v = self.b.build_load(ty, s.ptr, "tmp").unwrap();
                    self.b.build_store(s.ptr, ty.const_zero()).unwrap();
                    self.release_val(s.ty, v);
                }
            }
        }
    }
}
