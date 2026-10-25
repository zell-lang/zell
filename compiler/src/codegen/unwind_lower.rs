//! Lowering of landing pads to frame-chain nodes (runtime/ABI.md 8.1 item
//! 4a), run on the optimised module ([`lower_unwind`]).
//!
//! Code generation emits the calls that may panic in synchronous functions
//! as `invoke`s with cleanup landing pads (`unwind.rs`), so the optimiser
//! sees the real control flow. Afterwards no unwinder is used: every
//! function that still has an `invoke` gets, in a new entry block,
//!
//! * a *sync node* `{ prev, tag = 1, jmpbuf }` pushed on `zell_coro_chain`
//!   and popped before every `ret`, and a call-site slot (`i32`, -1);
//! * a setjmp into the node's buffer (`llvm.eh.sjlj.setjmp` on x86-64
//!   POSIX, `_setjmp` elsewhere, as `zell__plat_setjmp`) whose second return
//!   goes to a *dispatch* block that switches on the call-site slot to the
//!   landing pad, or (-1) straight to `zell_unwind_resume`.
//!
//! Each `invoke` becomes a call preceded by a volatile store of its landing
//! pad's index; every other call that may unwind stores -1 (its panic is
//! not this frame's to clean up). `landingpad` goes, `resume` becomes
//! `zell_unwind_resume()`. The landing pads are now reached from the
//! dispatch only, so the values they use that are defined in the body (and
//! the phis of the landing pads) are demoted to volatile stack slots, and
//! the stack slots they read are accessed volatile everywhere: the code
//! generator then keeps every store the landing pad may read.

use inkwell::llvm_sys::core as llc;
use inkwell::llvm_sys::debuginfo as lld;
use inkwell::llvm_sys::prelude::{LLVMBasicBlockRef, LLVMBuilderRef, LLVMContextRef, LLVMModuleRef, LLVMTypeRef, LLVMValueRef};
use inkwell::llvm_sys::LLVMOpcode;
use inkwell::module::Module;
use std::collections::{HashMap, HashSet};

/// Bytes of a sync node's jump buffer where it is a C `jmp_buf` (unwind.rs).
const JMPBUF_BYTES: u32 = 320;
/// `LLVMAttributeFunctionIndex` / `LLVMAttributeReturnIndex`.
const FN_INDEX: u32 = u32::MAX;
const RET_INDEX: u32 = 0;

/// Lowers the landing pads of every function of `module` and drops the
/// personalities of `unwind.rs`.
pub fn lower_unwind(module: &Module) {
    let m = module.as_mut_ptr();
    unsafe {
        let ctx = llc::LLVMGetModuleContext(m);
        let triple = std::ffi::CStr::from_ptr(llc::LLVMGetTarget(m)).to_string_lossy().into_owned();
        let mut f = llc::LLVMGetFirstFunction(m);
        while !f.is_null() {
            let next = llc::LLVMGetNextFunction(f);
            if llc::LLVMHasPersonalityFn(f) != 0 {
                let p = llc::LLVMGetPersonalityFn(f);
                let name = value_name(p);
                if name == super::unwind::PERSONALITY || name == super::unwind::PERSONALITY_CORO {
                    let mut l = Lower { m, ctx, f, b: llc::LLVMCreateBuilderInContext(ctx), builtin: triple.starts_with("x86_64") && !triple.contains("windows"), windows: triple.contains("windows"), x86_64: triple.starts_with("x86_64") };
                    l.run();
                    llc::LLVMDisposeBuilder(l.b);
                    llc::LLVMSetPersonalityFn(f, std::ptr::null_mut());
                }
            }
            f = next;
        }
    }
}

/// The function whose frame an outlined cleanup recovers (the first
/// operand of its first `llvm.localrecover`), for code generation that
/// must keep the two in one object.
pub fn unwind_parent(cleanup: inkwell::values::FunctionValue) -> Option<LLVMValueRef> {
    use inkwell::values::AsValueRef;
    unsafe {
        let entry = llc::LLVMGetFirstBasicBlock(cleanup.as_value_ref());
        if entry.is_null() {
            return None;
        }
        for i in insts(entry) {
            if opcode(i) == LLVMOpcode::LLVMCall && value_name(llc::LLVMGetCalledValue(i)).starts_with("llvm.localrecover") {
                return Some(llc::LLVMGetOperand(i, 0));
            }
        }
    }
    None
}

unsafe fn value_name(v: LLVMValueRef) -> String {
    let mut n = 0usize;
    let p = llc::LLVMGetValueName2(v, &mut n);
    String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n)).into_owned()
}

unsafe fn blocks(f: LLVMValueRef) -> Vec<LLVMBasicBlockRef> {
    let mut out = Vec::new();
    let mut b = llc::LLVMGetFirstBasicBlock(f);
    while !b.is_null() {
        out.push(b);
        b = llc::LLVMGetNextBasicBlock(b);
    }
    out
}

unsafe fn insts(b: LLVMBasicBlockRef) -> Vec<LLVMValueRef> {
    let mut out = Vec::new();
    let mut i = llc::LLVMGetFirstInstruction(b);
    while !i.is_null() {
        out.push(i);
        i = llc::LLVMGetNextInstruction(i);
    }
    out
}

unsafe fn succs(b: LLVMBasicBlockRef) -> Vec<LLVMBasicBlockRef> {
    let t = llc::LLVMGetBasicBlockTerminator(b);
    if t.is_null() {
        return Vec::new();
    }
    (0..llc::LLVMGetNumSuccessors(t)).map(|k| llc::LLVMGetSuccessor(t, k)).collect()
}

unsafe fn preds(f: LLVMValueRef, b: LLVMBasicBlockRef) -> Vec<LLVMBasicBlockRef> {
    blocks(f).into_iter().filter(|p| succs(*p).contains(&b)).collect()
}

/// The blocks reachable from `from` (included).
unsafe fn reachable(from: &[LLVMBasicBlockRef]) -> HashSet<LLVMBasicBlockRef> {
    let mut seen = HashSet::new();
    let mut work = from.to_vec();
    while let Some(b) = work.pop() {
        if seen.insert(b) {
            work.extend(succs(b));
        }
    }
    seen
}

/// The block before which the node is pushed: the nearest common
/// dominator of `targets`, moved up out of every cycle.
unsafe fn push_point(entry: LLVMBasicBlockRef, targets: &[LLVMBasicBlockRef]) -> LLVMBasicBlockRef {
    // Reverse post-order from the entry, then Cooper-Harvey-Kennedy.
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![(entry, 0usize)];
    seen.insert(entry);
    while let Some((b, k)) = stack.pop() {
        let ss = succs(b);
        if k < ss.len() {
            stack.push((b, k + 1));
            if seen.insert(ss[k]) {
                stack.push((ss[k], 0));
            }
        } else {
            order.push(b);
        }
    }
    order.reverse();
    let num: HashMap<LLVMBasicBlockRef, usize> = order.iter().enumerate().map(|(i, b)| (*b, i)).collect();
    let mut pred_of: HashMap<LLVMBasicBlockRef, Vec<LLVMBasicBlockRef>> = HashMap::new();
    for &b in &order {
        for s in succs(b) {
            pred_of.entry(s).or_default().push(b);
        }
    }
    let mut idom: Vec<Option<usize>> = vec![None; order.len()];
    idom[0] = Some(0);
    let intersect = |idom: &Vec<Option<usize>>, mut a: usize, mut b: usize| {
        while a != b {
            while a > b {
                a = idom[a].unwrap();
            }
            while b > a {
                b = idom[b].unwrap();
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for i in 1..order.len() {
            let mut new: Option<usize> = None;
            for p in pred_of.get(&order[i]).map(|v| v.as_slice()).unwrap_or(&[]) {
                let Some(&pi) = num.get(p) else { continue };
                if idom[pi].is_none() {
                    continue;
                }
                new = Some(match new {
                    None => pi,
                    Some(n) => intersect(&idom, pi, n),
                });
            }
            if new.is_some() && idom[i] != new {
                idom[i] = new;
                changed = true;
            }
        }
    }
    // Invokes in unreachable blocks do not count.
    let live: Vec<usize> = targets.iter().filter_map(|t| num.get(t).copied()).collect();
    let Some(&first) = live.first() else { return succs(order[0])[0] };
    let mut d = first;
    for &t in &live[1..] {
        d = intersect(&idom, d, t);
    }
    // Out of cycles: a block on a cycle reaches itself.
    loop {
        let b = order[d];
        let back = reachable(&succs(b));
        if !back.contains(&b) || d == 0 {
            break;
        }
        d = idom[d].unwrap();
    }
    if d == 0 {
        // Never the alloca block itself.
        return succs(order[0])[0];
    }
    order[d]
}

unsafe fn opcode(i: LLVMValueRef) -> LLVMOpcode {
    llc::LLVMGetInstructionOpcode(i)
}

struct Lower {
    m: LLVMModuleRef,
    ctx: LLVMContextRef,
    f: LLVMValueRef,
    b: LLVMBuilderRef,
    builtin: bool,
    windows: bool,
    x86_64: bool,
}

impl Lower {
    unsafe fn ptr(&self) -> LLVMTypeRef {
        llc::LLVMPointerTypeInContext(self.ctx, 0)
    }

    unsafe fn i32(&self) -> LLVMTypeRef {
        llc::LLVMInt32TypeInContext(self.ctx)
    }

    unsafe fn declare(&self, name: &std::ffi::CStr, ty: LLVMTypeRef) -> LLVMValueRef {
        let f = llc::LLVMGetNamedFunction(self.m, name.as_ptr());
        if !f.is_null() {
            return f;
        }
        llc::LLVMAddFunction(self.m, name.as_ptr(), ty)
    }

    unsafe fn intrinsic(&self, name: &str, tys: &mut [LLVMTypeRef]) -> (LLVMValueRef, LLVMTypeRef) {
        let id = llc::LLVMLookupIntrinsicID(name.as_ptr() as *const _, name.len());
        let f = llc::LLVMGetIntrinsicDeclaration(self.m, id, tys.as_mut_ptr(), tys.len());
        (f, llc::LLVMGlobalGetValueType(f))
    }

    unsafe fn chain(&self) -> LLVMValueRef {
        let g = llc::LLVMGetNamedGlobal(self.m, c"zell_coro_chain".as_ptr());
        if !g.is_null() {
            return g;
        }
        let g = llc::LLVMAddGlobal(self.m, self.ptr(), c"zell_coro_chain".as_ptr());
        llc::LLVMSetThreadLocal(g, 1);
        g
    }

    unsafe fn alloca_in(&self, entry: LLVMBasicBlockRef, ty: LLVMTypeRef) -> LLVMValueRef {
        let first = llc::LLVMGetFirstInstruction(entry);
        if first.is_null() {
            llc::LLVMPositionBuilderAtEnd(self.b, entry);
        } else {
            llc::LLVMPositionBuilderBefore(self.b, first);
        }
        llc::LLVMBuildAlloca(self.b, ty, c"".as_ptr())
    }

    unsafe fn store_volatile(&self, v: LLVMValueRef, p: LLVMValueRef) {
        let s = llc::LLVMBuildStore(self.b, v, p);
        llc::LLVMSetVolatile(s, 1);
    }

    unsafe fn load_volatile(&self, ty: LLVMTypeRef, p: LLVMValueRef) -> LLVMValueRef {
        let l = llc::LLVMBuildLoad2(self.b, ty, p, c"".as_ptr());
        llc::LLVMSetVolatile(l, 1);
        l
    }

    /// True if call `c` is cold: a `noreturn` or `cold` callee (a panic).
    unsafe fn is_cold(&self, c: LLVMValueRef) -> bool {
        let callee = llc::LLVMGetCalledValue(c);
        if llc::LLVMIsAFunction(callee).is_null() {
            return false;
        }
        ["noreturn", "cold"].iter().any(|n| {
            let k = llc::LLVMGetEnumAttributeKindForName(n.as_ptr() as *const _, n.len());
            !llc::LLVMGetEnumAttributeAtIndex(callee, FN_INDEX, k).is_null() || !llc::LLVMGetCallSiteEnumAttribute(c, FN_INDEX, k).is_null()
        })
    }

    /// True if call `c` may unwind (not an intrinsic, inline asm or
    /// `nounwind`).
    unsafe fn may_unwind(&self, c: LLVMValueRef) -> bool {
        let callee = llc::LLVMGetCalledValue(c);
        if !llc::LLVMIsAInlineAsm(callee).is_null() {
            return false;
        }
        let nounwind = llc::LLVMGetEnumAttributeKindForName(c"nounwind".as_ptr(), 8);
        let fi = FN_INDEX;
        if !llc::LLVMGetCallSiteEnumAttribute(c, fi, nounwind).is_null() {
            return false;
        }
        if !llc::LLVMIsAFunction(callee).is_null() {
            if value_name(callee).starts_with("llvm.") {
                return false;
            }
            if !llc::LLVMGetEnumAttributeAtIndex(callee, fi, nounwind).is_null() {
                return false;
            }
        }
        true
    }

    unsafe fn run(&mut self) {
        let f = self.f;
        let all = blocks(f);
        let mut invokes = Vec::new();
        let mut lpads: Vec<LLVMBasicBlockRef> = Vec::new();
        for &b in &all {
            for i in insts(b) {
                match opcode(i) {
                    LLVMOpcode::LLVMInvoke => {
                        let u = llc::LLVMGetUnwindDest(i);
                        if !lpads.contains(&u) {
                            lpads.push(u);
                        }
                        invokes.push(i);
                    }
                    LLVMOpcode::LLVMLandingPad if !lpads.contains(&b) => lpads.push(b),
                    _ => {}
                }
            }
        }
        if lpads.is_empty() {
            return;
        }
        // The landing region: everything reachable from a landing pad.
        let mut region: HashSet<LLVMBasicBlockRef> = HashSet::new();
        let mut work = lpads.clone();
        while let Some(b) = work.pop() {
            if !region.insert(b) {
                continue;
            }
            let t = llc::LLVMGetBasicBlockTerminator(b);
            if t.is_null() {
                continue;
            }
            for k in 0..llc::LLVMGetNumSuccessors(t) {
                work.push(llc::LLVMGetSuccessor(t, k));
            }
        }
        let ptr = self.ptr();
        let i32t = self.i32();
        let old_entry = all[0];
        let resume_ty = llc::LLVMFunctionType(llc::LLVMVoidTypeInContext(self.ctx), std::ptr::null_mut(), 0, 0);
        let resume = self.declare(c"zell_unwind_resume", resume_ty);
        // Invokes inside the landing region run while unwinding (a panic
        // there ends the process): plain calls, no node for them.
        let in_body: Vec<LLVMValueRef> = invokes.iter().copied().filter(|i| !region.contains(&llc::LLVMGetInstructionParent(*i))).collect();
        if in_body.is_empty() {
            // Dead landing pads (no invoke of the body reaches them).
            let index: HashMap<LLVMBasicBlockRef, u64> = lpads.iter().enumerate().map(|(k, b)| (*b, k as u64)).collect();
            let entry = llc::LLVMGetEntryBasicBlock(f);
            let (phi_slots, _) = self.convert_invokes(entry, &lpads, &invokes, &index, None, &region);
            self.strip_pads(&lpads, &region, Some((resume, resume_ty)), &phi_slots);
            return;
        }
        // Outlined cleanup (x86-64): the landing region becomes a function of
        // the frame, recovering its slots with `llvm.localrecover`; needs a
        // region that only the landing pads enter.
        let lpad_set: HashSet<LLVMBasicBlockRef> = lpads.iter().copied().collect();
        let closed = region.iter().all(|b| lpad_set.contains(b) || preds(f, *b).iter().all(|p| region.contains(p)));
        // `ZELL_UNWIND_SETJMP=1` forces the setjmp lowering (its tests).
        let outline = self.x86_64 && closed && std::env::var_os("ZELL_UNWIND_SETJMP").is_none();
        // New entry: the allocas, the node, the push, the setjmp.
        let entry = llc::LLVMInsertBasicBlockInContext(self.ctx, old_entry, c"unwind.entry".as_ptr());
        llc::LLVMPositionBuilderAtEnd(self.b, entry);
        for i in insts(old_entry) {
            if opcode(i) == LLVMOpcode::LLVMAlloca && llc::LLVMIsConstant(llc::LLVMGetOperand(i, 0)) != 0 {
                llc::LLVMInstructionRemoveFromParent(i);
                llc::LLVMInsertIntoBuilder(self.b, i);
            }
        }
        let buf_ty = if outline {
            llc::LLVMArrayType2(ptr, 2)
        } else if self.builtin {
            llc::LLVMArrayType2(ptr, 5)
        } else {
            llc::LLVMArrayType2(llc::LLVMInt8TypeInContext(self.ctx), JMPBUF_BYTES as u64)
        };
        let mut fields = [ptr, ptr, buf_ty];
        let node_ty = llc::LLVMStructTypeInContext(self.ctx, fields.as_mut_ptr(), 3, 0);
        let node = llc::LLVMBuildAlloca(self.b, node_ty, c"sync.node".as_ptr());
        llc::LLVMSetAlignment(node, 16);
        let cs = llc::LLVMBuildAlloca(self.b, i32t, c"sync.site".as_ptr());
        llc::LLVMBuildBr(self.b, old_entry);
        // The push point: a block that dominates every invoke and is on no
        // cycle (so it runs at most once per call), as late as possible: a
        // path that reaches no invoke (an early return) pays nothing.
        let invoke_blocks: Vec<LLVMBasicBlockRef> = in_body.iter().map(|i| llc::LLVMGetInstructionParent(*i)).collect();
        // The landing pads the body reaches; the others (reached from
        // invokes inside the region only) become unreachable.
        let mut live: Vec<LLVMBasicBlockRef> = Vec::new();
        for i in &in_body {
            let u = llc::LLVMGetUnwindDest(*i);
            if !live.contains(&u) {
                live.push(u);
            }
        }
        let push_at = push_point(entry, &invoke_blocks);
        let push = llc::LLVMInsertBasicBlockInContext(self.ctx, push_at, c"unwind.push".as_ptr());
        for p in preds(f, push_at) {
            if p == push {
                continue;
            }
            let t = llc::LLVMGetBasicBlockTerminator(p);
            for k in 0..llc::LLVMGetNumSuccessors(t) {
                if llc::LLVMGetSuccessor(t, k) == push_at {
                    llc::LLVMSetSuccessor(t, k, push);
                }
            }
        }
        llc::LLVMPositionBuilderAtEnd(self.b, push);
        for i in insts(push_at) {
            if opcode(i) != LLVMOpcode::LLVMPHI {
                break;
            }
            llc::LLVMInstructionRemoveFromParent(i);
            llc::LLVMInsertIntoBuilder(self.b, i);
        }
        self.store_volatile(llc::LLVMConstInt(i32t, u64::MAX, 1), cs);
        let chain = self.chain();
        let prev = llc::LLVMBuildLoad2(self.b, ptr, chain, c"chain".as_ptr());
        self.store_volatile(prev, node);
        let tp = llc::LLVMBuildStructGEP2(self.b, node_ty, node, 1, c"".as_ptr());
        let tag = if outline { 2 } else { 1 };
        self.store_volatile(llc::LLVMConstIntToPtr(llc::LLVMConstInt(llc::LLVMInt64TypeInContext(self.ctx), tag, 0), ptr), tp);
        let buf = llc::LLVMBuildStructGEP2(self.b, node_ty, node, 2, c"sync.jb".as_ptr());
        if outline {
            let cf = self.cleanup_fn();
            self.store_volatile(cf, buf);
            let (ss, sst) = self.intrinsic("llvm.stacksave", &mut [ptr]);
            let sp = llc::LLVMBuildCall2(self.b, sst, ss, std::ptr::null_mut(), 0, c"".as_ptr());
            let mut idx = [llc::LLVMConstInt(llc::LLVMInt64TypeInContext(self.ctx), 1, 0)];
            let p1 = llc::LLVMBuildInBoundsGEP2(self.b, ptr, buf, idx.as_mut_ptr(), 1, c"".as_ptr());
            self.store_volatile(sp, p1);
            llc::LLVMBuildStore(self.b, node, chain);
            llc::LLVMBuildBr(self.b, push_at);
            let pushed = reachable(&[push_at]);
            let index: HashMap<LLVMBasicBlockRef, u64> = live.iter().enumerate().map(|(k, b)| (*b, k as u64)).collect();
            let (phi_slots, converted) = self.convert_invokes(entry, &lpads, &invokes, &index, Some(cs), &region);
            self.strip_pads(&lpads, &region, None, &phi_slots);
            self.minus_ones_and_pops(&pushed, &region, &converted, cs, node, chain);
            self.demote(&pushed, push, &region, &converted, true);
            self.outline(cf, entry, &live, &index, &region, cs);
            return;
        }
        llc::LLVMBuildStore(self.b, node, chain);
        let r = if self.builtin {
            let (fa, fat) = self.intrinsic("llvm.frameaddress", &mut [ptr]);
            let mut a0 = [llc::LLVMConstInt(i32t, 0, 0)];
            let fp = llc::LLVMBuildCall2(self.b, fat, fa, a0.as_mut_ptr(), 1, c"".as_ptr());
            self.store_volatile(fp, buf);
            let (ss, sst) = self.intrinsic("llvm.stacksave", &mut [ptr]);
            let sp = llc::LLVMBuildCall2(self.b, sst, ss, std::ptr::null_mut(), 0, c"".as_ptr());
            let mut idx = [llc::LLVMConstInt(llc::LLVMInt64TypeInContext(self.ctx), 2, 0)];
            let p2 = llc::LLVMBuildInBoundsGEP2(self.b, ptr, buf, idx.as_mut_ptr(), 1, c"".as_ptr());
            self.store_volatile(sp, p2);
            let (sj, sjt) = self.intrinsic("llvm.eh.sjlj.setjmp", &mut []);
            let mut a = [buf];
            llc::LLVMBuildCall2(self.b, sjt, sj, a.as_mut_ptr(), 1, c"sj".as_ptr())
        } else {
            let mut ps = if self.windows { vec![ptr, ptr] } else { vec![ptr] };
            let ty = llc::LLVMFunctionType(i32t, ps.as_mut_ptr(), ps.len() as u32, 0);
            let sj = self.declare(c"_setjmp", ty);
            let rt = llc::LLVMGetEnumAttributeKindForName(c"returns_twice".as_ptr(), 13);
            let attr = llc::LLVMCreateEnumAttribute(self.ctx, rt, 0);
            llc::LLVMAddAttributeAtIndex(sj, FN_INDEX, attr);
            let mut a = if self.windows { vec![buf, llc::LLVMConstNull(ptr)] } else { vec![buf] };
            let c = llc::LLVMBuildCall2(self.b, ty, sj, a.as_mut_ptr(), a.len() as u32, c"sj".as_ptr());
            llc::LLVMAddCallSiteAttribute(c, FN_INDEX, attr);
            c
        };
        let dispatch = llc::LLVMAppendBasicBlockInContext(self.ctx, f, c"unwind.dispatch".as_ptr());
        let unw = llc::LLVMBuildICmp(self.b, inkwell::llvm_sys::LLVMIntPredicate::LLVMIntNE, r, llc::LLVMConstInt(i32t, 0, 0), c"unwound".as_ptr());
        llc::LLVMBuildCondBr(self.b, unw, dispatch, push_at);
        let pushed = reachable(&[push_at]);
        let none = llc::LLVMAppendBasicBlockInContext(self.ctx, f, c"unwind.none".as_ptr());
        llc::LLVMPositionBuilderAtEnd(self.b, none);
        llc::LLVMBuildCall2(self.b, resume_ty, resume, std::ptr::null_mut(), 0, c"".as_ptr());
        llc::LLVMBuildUnreachable(self.b);
        llc::LLVMPositionBuilderAtEnd(self.b, dispatch);
        let site = self.load_volatile(i32t, cs);
        let sw = llc::LLVMBuildSwitch(self.b, site, none, live.len() as u32);
        let index: HashMap<LLVMBasicBlockRef, u64> = live.iter().enumerate().map(|(k, b)| (*b, k as u64)).collect();
        for (b, k) in &index {
            llc::LLVMAddCase(sw, llc::LLVMConstInt(i32t, *k, 0), *b);
        }
        let (phi_slots, converted) = self.convert_invokes(entry, &lpads, &invokes, &index, Some(cs), &region);
        self.strip_pads(&lpads, &region, Some((resume, resume_ty)), &phi_slots);
        self.minus_ones_and_pops(&pushed, &region, &converted, cs, node, chain);
        self.demote(&pushed, push, &region, &converted, false);
    }

    /// Stores the landing pad phis' incoming values into slots, and turns
    /// each invoke into a call after storing its landing pad's index.
    unsafe fn convert_invokes(&self, entry: LLVMBasicBlockRef, lpads: &[LLVMBasicBlockRef], invokes: &[LLVMValueRef], index: &HashMap<LLVMBasicBlockRef, u64>, cs: Option<LLVMValueRef>, region: &HashSet<LLVMBasicBlockRef>) -> (HashMap<LLVMValueRef, LLVMValueRef>, HashMap<LLVMValueRef, LLVMBasicBlockRef>) {
        let i32t = self.i32();
        let mut phi_slots: HashMap<LLVMValueRef, LLVMValueRef> = HashMap::new();
        for &lp in lpads {
            for i in insts(lp) {
                if opcode(i) != LLVMOpcode::LLVMPHI {
                    break;
                }
                let slot = self.alloca_in(entry, llc::LLVMTypeOf(i));
                for k in 0..llc::LLVMCountIncoming(i) {
                    let pred = llc::LLVMGetIncomingBlock(i, k);
                    let t = llc::LLVMGetBasicBlockTerminator(pred);
                    llc::LLVMPositionBuilderBefore(self.b, t);
                    self.store_volatile(llc::LLVMGetIncomingValue(i, k), slot);
                }
                phi_slots.insert(i, slot);
            }
        }
        let mut converted = HashMap::new();
        for &inv in invokes {
            let lp = llc::LLVMGetUnwindDest(inv);
            let body = !region.contains(&llc::LLVMGetInstructionParent(inv));
            llc::LLVMPositionBuilderBefore(self.b, inv);
            if let (Some(cs), true) = (cs, body) {
                self.store_volatile(llc::LLVMConstInt(i32t, index[&lp], 0), cs);
            }
            let n = llc::LLVMGetNumArgOperands(inv);
            let mut args: Vec<LLVMValueRef> = (0..n).map(|a| llc::LLVMGetOperand(inv, a)).collect();
            let fty = llc::LLVMGetCalledFunctionType(inv);
            let call = llc::LLVMBuildCall2(self.b, fty, llc::LLVMGetCalledValue(inv), args.as_mut_ptr(), n, c"".as_ptr());
            llc::LLVMSetInstructionCallConv(call, llc::LLVMGetInstructionCallConv(inv));
            let mut idxs = vec![FN_INDEX, RET_INDEX];
            idxs.extend(1..=n);
            for idx in idxs {
                let cnt = llc::LLVMGetCallSiteAttributeCount(inv, idx);
                if cnt > 0 {
                    let mut attrs = vec![std::ptr::null_mut(); cnt as usize];
                    llc::LLVMGetCallSiteAttributes(inv, idx, attrs.as_mut_ptr());
                    for a in attrs {
                        llc::LLVMAddCallSiteAttribute(call, idx, a);
                    }
                }
            }
            let loc = lld::LLVMInstructionGetDebugLoc(inv);
            if !loc.is_null() {
                lld::LLVMInstructionSetDebugLoc(call, loc);
            }
            llc::LLVMBuildBr(self.b, llc::LLVMGetNormalDest(inv));
            if llc::LLVMGetTypeKind(llc::LLVMTypeOf(inv)) != inkwell::llvm_sys::LLVMTypeKind::LLVMVoidTypeKind {
                llc::LLVMReplaceAllUsesWith(inv, call);
            }
            llc::LLVMInstructionEraseFromParent(inv);
            if body {
                converted.insert(call, lp);
            }
        }
        (phi_slots, converted)
    }

    /// Before every other call that may unwind, -1 (not this frame's to
    /// clean up); before every `ret`, the pop of the node if it was pushed
    /// on the way (it is then the innermost one).
    unsafe fn minus_ones_and_pops(&self, pushed: &HashSet<LLVMBasicBlockRef>, region: &HashSet<LLVMBasicBlockRef>, converted: &HashMap<LLVMValueRef, LLVMBasicBlockRef>, cs: LLVMValueRef, node: LLVMValueRef, chain: LLVMValueRef) {
        let ptr = self.ptr();
        let minus1 = llc::LLVMConstInt(self.i32(), u64::MAX, 1);
        for &b in pushed {
            if region.contains(&b) {
                continue;
            }
            for i in insts(b) {
                if opcode(i) == LLVMOpcode::LLVMCall {
                    llc::LLVMSetTailCall(i, 0);
                    if !converted.contains_key(&i) && self.may_unwind(i) {
                        llc::LLVMPositionBuilderBefore(self.b, i);
                        self.store_volatile(minus1, cs);
                    }
                } else if opcode(i) == LLVMOpcode::LLVMRet {
                    llc::LLVMPositionBuilderBefore(self.b, i);
                    let cur = llc::LLVMBuildLoad2(self.b, ptr, chain, c"".as_ptr());
                    let prev = self.load_volatile(ptr, node);
                    let mine = llc::LLVMBuildICmp(self.b, inkwell::llvm_sys::LLVMIntPredicate::LLVMIntEQ, cur, node, c"".as_ptr());
                    let next = llc::LLVMBuildSelect(self.b, mine, prev, cur, c"".as_ptr());
                    llc::LLVMBuildStore(self.b, next, chain);
                }
            }
        }
    }

    /// The cleanup function of the frame (outlined mode): `void(ptr sp)`.
    unsafe fn cleanup_fn(&self) -> LLVMValueRef {
        let mut ps = [self.ptr()];
        let ty = llc::LLVMFunctionType(llc::LLVMVoidTypeInContext(self.ctx), ps.as_mut_ptr(), 1, 0);
        let name = format!("{}.unwind\0", value_name(self.f));
        let cf = llc::LLVMAddFunction(self.m, name.as_ptr() as *const _, ty);
        llc::LLVMSetLinkage(cf, inkwell::llvm_sys::LLVMLinkage::LLVMInternalLinkage);
        let fp = llc::LLVMCreateStringAttribute(self.ctx, c"frame-pointer".as_ptr(), 13, c"all".as_ptr(), 3);
        llc::LLVMAddAttributeAtIndex(cf, FN_INDEX, fp);
        let mark = llc::LLVMCreateStringAttribute(self.ctx, c"zell-unwind-cleanup".as_ptr(), 19, c"".as_ptr(), 0);
        llc::LLVMAddAttributeAtIndex(cf, FN_INDEX, mark);
        cf
    }

    /// Moves the landing region into `cf`: an entry that recovers the
    /// frame's stack slots (`llvm.localrecover`, the frame escaping them
    /// with `llvm.localescape`) and switches on the call-site slot; the
    /// region's uses of the slots become the recovered addresses.
    unsafe fn outline(&self, cf: LLVMValueRef, entry: LLVMBasicBlockRef, lpads: &[LLVMBasicBlockRef], index: &HashMap<LLVMBasicBlockRef, u64>, region: &HashSet<LLVMBasicBlockRef>, cs: LLVMValueRef) {
        let f = self.f;
        let ptr = self.ptr();
        let i32t = self.i32();
        // The frame's slots the region reads (and the call-site slot).
        let mut escaped: Vec<LLVMValueRef> = vec![cs];
        let ordered: Vec<LLVMBasicBlockRef> = blocks(f).into_iter().filter(|b| region.contains(b)).collect();
        let live = reachable(lpads);
        for &b in &ordered {
            for i in insts(b) {
                for k in 0..llc::LLVMGetNumOperands(i) as u32 {
                    let o = llc::LLVMGetOperand(i, k);
                    // Unreachable code (pads of invokes inside the region):
                    // its uses of the body need no value.
                    let body_value = !llc::LLVMIsAArgument(o).is_null() || (!llc::LLVMIsAInstruction(o).is_null() && !region.contains(&llc::LLVMGetInstructionParent(o)) && opcode(o) != LLVMOpcode::LLVMAlloca);
                    if !live.contains(&b) && body_value {
                        llc::LLVMSetOperand(i, k, llc::LLVMGetPoison(llc::LLVMTypeOf(o)));
                        continue;
                    }
                    if !llc::LLVMIsAInstruction(o).is_null() && !region.contains(&llc::LLVMGetInstructionParent(o)) {
                        assert!(opcode(o) == LLVMOpcode::LLVMAlloca, "unwind lowering: a landing pad uses a value of the body");
                        if !escaped.contains(&o) {
                            escaped.push(o);
                        }
                    }
                    assert!(llc::LLVMIsAArgument(o).is_null(), "unwind lowering: a landing pad uses an argument");
                }
            }
        }
        // The frame: "stackrealign" makes the escaped offsets relative to
        // the stack pointer after the prologue (what the node records).
        let ra = llc::LLVMCreateStringAttribute(self.ctx, c"stackrealign".as_ptr(), 12, c"".as_ptr(), 0);
        llc::LLVMAddAttributeAtIndex(f, FN_INDEX, ra);
        let (esc, esct) = self.intrinsic("llvm.localescape", &mut []);
        llc::LLVMPositionBuilderBefore(self.b, llc::LLVMGetBasicBlockTerminator(entry));
        llc::LLVMBuildCall2(self.b, esct, esc, escaped.as_mut_ptr(), escaped.len() as u32, c"".as_ptr());
        // The cleanup function's entry.
        let ce = llc::LLVMAppendBasicBlockInContext(self.ctx, cf, c"entry".as_ptr());
        llc::LLVMPositionBuilderAtEnd(self.b, ce);
        let sp = llc::LLVMGetParam(cf, 0);
        let (rec, rect) = self.intrinsic("llvm.localrecover", &mut []);
        let mut map: HashMap<LLVMValueRef, LLVMValueRef> = HashMap::new();
        for (k, a) in escaped.iter().enumerate() {
            let mut args = [f, sp, llc::LLVMConstInt(i32t, k as u64, 0)];
            let r = llc::LLVMBuildCall2(self.b, rect, rec, args.as_mut_ptr(), 3, c"".as_ptr());
            map.insert(*a, r);
        }
        let done = llc::LLVMAppendBasicBlockInContext(self.ctx, cf, c"done".as_ptr());
        let site = llc::LLVMBuildLoad2(self.b, i32t, map[&cs], c"site".as_ptr());
        let sw = llc::LLVMBuildSwitch(self.b, site, done, lpads.len() as u32);
        for b in lpads {
            llc::LLVMAddCase(sw, llc::LLVMConstInt(i32t, index[b], 0), *b);
        }
        llc::LLVMPositionBuilderAtEnd(self.b, done);
        llc::LLVMBuildRetVoid(self.b);
        let _ = ptr;
        // Move the region and rewrite its uses of the frame's slots.
        for &b in &ordered {
            llc::LLVMMoveBasicBlockAfter(b, llc::LLVMGetLastBasicBlock(cf));
        }
        for &b in &ordered {
            for i in insts(b) {
                if opcode(i) == LLVMOpcode::LLVMCall {
                    let callee = llc::LLVMGetCalledValue(i);
                    if !llc::LLVMIsAFunction(callee).is_null() && value_name(callee).starts_with("llvm.dbg.") {
                        llc::LLVMInstructionEraseFromParent(i);
                        continue;
                    }
                }
                lld::LLVMInstructionSetDebugLoc(i, std::ptr::null_mut());
                for k in 0..llc::LLVMGetNumOperands(i) as u32 {
                    let o = llc::LLVMGetOperand(i, k);
                    if let Some(r) = map.get(&o) {
                        llc::LLVMSetOperand(i, k, *r);
                    }
                }
            }
        }
    }

    /// Removes `landingpad` (its phis become loads of `phi_slots`) and turns
    /// `resume` into `zell_unwind_resume()`.
    unsafe fn strip_pads(&self, lpads: &[LLVMBasicBlockRef], region: &HashSet<LLVMBasicBlockRef>, resume: Option<(LLVMValueRef, LLVMTypeRef)>, phi_slots: &HashMap<LLVMValueRef, LLVMValueRef>) {
        for &lp in lpads {
            let mut phis = Vec::new();
            for i in insts(lp) {
                match opcode(i) {
                    LLVMOpcode::LLVMPHI => phis.push(i),
                    LLVMOpcode::LLVMLandingPad => {
                        llc::LLVMReplaceAllUsesWith(i, llc::LLVMGetPoison(llc::LLVMTypeOf(i)));
                        llc::LLVMInstructionEraseFromParent(i);
                    }
                    _ => {}
                }
            }
            for p in phis {
                let first = insts(lp).into_iter().find(|i| opcode(*i) != LLVMOpcode::LLVMPHI).unwrap();
                llc::LLVMPositionBuilderBefore(self.b, first);
                let v = match phi_slots.get(&p) {
                    Some(slot) => self.load_volatile(llc::LLVMTypeOf(p), *slot),
                    None => llc::LLVMGetPoison(llc::LLVMTypeOf(p)),
                };
                llc::LLVMReplaceAllUsesWith(p, v);
                llc::LLVMInstructionEraseFromParent(p);
            }
        }
        for &b in region {
            for i in insts(b) {
                if opcode(i) == LLVMOpcode::LLVMResume {
                    llc::LLVMPositionBuilderBefore(self.b, i);
                    match resume {
                        Some((r, rt)) => {
                            llc::LLVMBuildCall2(self.b, rt, r, std::ptr::null_mut(), 0, c"".as_ptr());
                            llc::LLVMBuildUnreachable(self.b);
                        }
                        // Outlined: the cleanup returns to the runtime.
                        None => {
                            llc::LLVMBuildRetVoid(self.b);
                        }
                    }
                    llc::LLVMInstructionEraseFromParent(i);
                }
            }
        }
    }

    /// Demotes the values defined in the body and used in the landing
    /// region to volatile slots, stored right before each call whose
    /// landing pad reaches a use (often a cold path: a bounds check, an
    /// allocation), and makes the accesses to the stack slots the region
    /// reads volatile. Setjmp: only values defined after it (the others
    /// dominate it); outlined: every value of the frame, the arguments
    /// included.
    unsafe fn demote(&self, pushed: &HashSet<LLVMBasicBlockRef>, push: LLVMBasicBlockRef, region: &HashSet<LLVMBasicBlockRef>, calls: &HashMap<LLVMValueRef, LLVMBasicBlockRef>, all: bool) {
        let in_region = |i: LLVMValueRef| region.contains(&llc::LLVMGetInstructionParent(i));
        let entry = llc::LLVMGetEntryBasicBlock(self.f);
        let wanted = |v: LLVMValueRef| -> bool {
            if !llc::LLVMIsAArgument(v).is_null() {
                return all;
            }
            if llc::LLVMIsAInstruction(v).is_null() {
                return false;
            }
            let b = llc::LLVMGetInstructionParent(v);
            if llc::LLVMGetBasicBlockParent(b) != self.f || b == entry || region.contains(&b) {
                return false;
            }
            all || (b != push && pushed.contains(&b))
        };
        // Per landing pad: the values its part of the region uses. Every
        // walk below goes in block and instruction order, never in hash
        // order: the slots' allocas (the frame layout) and the stores must
        // not depend on the hash seed or on debug metadata (same code with
        // and without `-g`, reproducible builds).
        let order = blocks(self.f);
        let lp_set: HashSet<LLVMBasicBlockRef> = calls.values().copied().collect();
        let lpads: Vec<LLVMBasicBlockRef> = order.iter().copied().filter(|b| lp_set.contains(b)).collect();
        let calls: Vec<(LLVMValueRef, LLVMBasicBlockRef)> = order.iter().flat_map(|&b| insts(b)).filter_map(|i| calls.get(&i).map(|lp| (i, *lp))).collect();
        let mut slots: HashMap<LLVMValueRef, LLVMValueRef> = HashMap::new();
        let mut needed: HashMap<LLVMBasicBlockRef, Vec<LLVMValueRef>> = HashMap::new();
        for &lp in &lpads {
            let mut vals = Vec::new();
            let reach = reachable(&[lp]);
            for &b in order.iter().filter(|b| reach.contains(b)) {
                for i in insts(b) {
                    for k in 0..llc::LLVMGetNumOperands(i) as u32 {
                        let o = llc::LLVMGetOperand(i, k);
                        // A phi's operand only on an edge this pad reaches.
                        if opcode(i) == LLVMOpcode::LLVMPHI && !reach.contains(&llc::LLVMGetIncomingBlock(i, k)) {
                            continue;
                        }
                        if wanted(o) && !vals.contains(&o) {
                            vals.push(o);
                        }
                    }
                }
            }
            for &v in &vals {
                if !slots.contains_key(&v) {
                    slots.insert(v, self.alloca_in(entry, llc::LLVMTypeOf(v)));
                }
            }
            needed.insert(lp, vals);
        }
        // Where to store: before a cold call (a panic) when only such calls
        // need the value; else once, right after its definition.
        let mut at_def: Vec<LLVMValueRef> = Vec::new();
        let mut at_set: HashSet<LLVMValueRef> = HashSet::new();
        for &(call, lp) in &calls {
            if !self.is_cold(call) {
                for v in &needed[&lp] {
                    if at_set.insert(*v) {
                        at_def.push(*v);
                    }
                }
            }
        }
        for &(call, lp) in &calls {
            llc::LLVMPositionBuilderBefore(self.b, call);
            for v in &needed[&lp] {
                if !at_set.contains(v) {
                    self.store_volatile(*v, slots[v]);
                }
            }
        }
        for &v in &at_def {
            let mut at = if llc::LLVMIsAArgument(v).is_null() { llc::LLVMGetNextInstruction(v) } else { llc::LLVMGetFirstInstruction(push) };
            while !at.is_null() && opcode(at) == LLVMOpcode::LLVMPHI {
                at = llc::LLVMGetNextInstruction(at);
            }
            llc::LLVMPositionBuilderBefore(self.b, at);
            self.store_volatile(v, slots[&v]);
        }
        // The region's uses read the slots.
        for b in region.iter().copied().collect::<Vec<_>>() {
            for i in insts(b) {
                for k in 0..llc::LLVMGetNumOperands(i) as u32 {
                    let o = llc::LLVMGetOperand(i, k);
                    let Some(&slot) = slots.get(&o) else { continue };
                    if opcode(i) == LLVMOpcode::LLVMPHI {
                        let ib = llc::LLVMGetIncomingBlock(i, k);
                        if !region.contains(&ib) {
                            continue;
                        }
                        llc::LLVMPositionBuilderBefore(self.b, llc::LLVMGetBasicBlockTerminator(ib));
                    } else {
                        llc::LLVMPositionBuilderBefore(self.b, i);
                    }
                    let l = self.load_volatile(llc::LLVMTypeOf(o), slot);
                    llc::LLVMSetOperand(i, k, l);
                }
            }
        }
        // Stack slots read in the region: every access volatile.
        for a in insts(entry) {
            if opcode(a) != LLVMOpcode::LLVMAlloca {
                continue;
            }
            let mut derived = vec![a];
            let mut accesses = Vec::new();
            let mut touched = false;
            while let Some(p) = derived.pop() {
                let mut u = llc::LLVMGetFirstUse(p);
                while !u.is_null() {
                    let user = llc::LLVMGetUser(u);
                    if !llc::LLVMIsAInstruction(user).is_null() {
                        touched |= in_region(user);
                        match opcode(user) {
                            LLVMOpcode::LLVMGetElementPtr | LLVMOpcode::LLVMBitCast => derived.push(user),
                            LLVMOpcode::LLVMLoad | LLVMOpcode::LLVMStore => accesses.push(user),
                            _ => {}
                        }
                    }
                    u = llc::LLVMGetNextUse(u);
                }
            }
            if touched {
                for i in accesses {
                    llc::LLVMSetVolatile(i, 1);
                }
            }
        }
    }
}
