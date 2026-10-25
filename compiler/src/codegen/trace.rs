//! Error return traces (DESIGN.md "Трасса ошибки", ABI.md §7.4): static
//! `zell_error_site` records `{file, func, line}` and the calls that store
//! them in errors.
//!
//! * Creation sites (`error(..)`, `errors.New`, `errors.Wrap`, a struct
//!   converted to `error`, and `return err` of an error that has no site yet,
//!   such as a sentinel) call `zell_error_created`.
//! * Every `?` calls `zell_error_returned` (which also wraps for `?("ctx")`).
//! * Where a propagation ends the runtime settles the *pending* trace of a
//!   shared error (a sentinel propagates without being copied; its frames
//!   live in a thread-local record, ABI.md §7.4): `try ... catch err` calls
//!   `zell_error_caught` with its frame and `keep` = whether the handler can
//!   observe the trace ([`CodeGen::handler_keeps_trace`]); a catch without a
//!   binding and `try ... else` call `zell_error_discarded`; the result of
//!   an async function that suspended is settled with `keep` before it is
//!   published to the task.
//! * With [`super::Options::no_error_trace`] no site is emitted and the calls
//!   are elided (`?("ctx")` then calls `zell_error_wrap` directly).
//!
//! Sites are only recorded inside HIR functions (not in global
//! initialisers, so `return Sentinel` records the `return`).

use super::CodeGen;
use crate::hir::ownership::{walk_block, Pos};
use crate::hir::{Block, Callee, CaptureSource, Expr, ExprKind, LocalId};
use crate::types::Type;
use inkwell::module::Linkage;
use inkwell::values::{BasicValueEnum, PointerValue};

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// The static site record for byte offset `offset` of the current
    /// function's file, or `None` when traces are off or outside a function.
    pub(crate) fn error_site(&mut self, offset: u32) -> Option<PointerValue<'ctx>> {
        if self.opts.no_error_trace {
            return None;
        }
        let f = self.f.as_ref()?.hir?;
        // Frames inside the standard library are not recorded (like panic
        // backtraces, which have no lines for std code): the call of the
        // std function is the creation site (`trace_std_call`).
        if crate::sema::stdlib::is_std_namespace(&f.full_name) {
            return None;
        }
        let src = self.prog.sources.get(f.file);
        let file = src.map(|s| s.path.clone()).unwrap_or_else(|| "?".into());
        let line = src.map(|s| s.line_of(offset)).unwrap_or(0);
        let ns = &self.prog.namespaces[f.namespace.0 as usize].path;
        let func = f.full_name.strip_prefix(&format!("{ns}.")).unwrap_or(&f.full_name).to_string();
        let key = format!("{file}\0{func}\0{line}");
        if let Some(p) = self.error_sites.get(&key) {
            return Some(*p);
        }
        let fp = self.cstr(&file);
        let np = self.cstr(&func);
        let st = self.ctx.struct_type(&[self.ptr_ty().into(), self.ptr_ty().into(), self.i64_ty().into()], false);
        let init = st.const_named_struct(&[fp.into(), np.into(), self.i64_ty().const_int(line as u64, false).into()]);
        let k = self.fresh();
        let g = self.module.add_global(st, None, &format!("errsite.{k}"));
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_unnamed_addr(true);
        g.set_initializer(&init);
        let p = g.as_pointer_value();
        self.error_sites.insert(key, p);
        Some(p)
    }

    /// Records creation site `offset` in the owned error `err` (kept if it
    /// already has one); returns the owned (possibly copied) error.
    pub(crate) fn trace_created(&mut self, err: BasicValueEnum<'ctx>, offset: u32) -> BasicValueEnum<'ctx> {
        match self.error_site(offset) {
            Some(site) => self.when_error(err, |cg| cg.call_rt_v("zell_error_created", &[err.into(), site.into()])),
            None => err,
        }
    }

    /// The result `r` of a direct call of `callee` at `offset`: a call of a
    /// fallible std function written in Zell is a creation site (DESIGN.md
    /// «Ошибки»), as a runtime extern's is; a site the error already has
    /// (a user error passed through std code) is kept.
    /// `ty` is the call's (or the awaited call's) result type.
    pub(crate) fn trace_std_call(&mut self, callee: crate::hir::FuncId, ty: crate::types::TypeId, r: BasicValueEnum<'ctx>, offset: u32) -> BasicValueEnum<'ctx> {
        let f = &self.prog.funcs[callee.0 as usize];
        let Type::Fallible(inner) = *self.ty(ty) else { return r };
        if !crate::sema::stdlib::is_std_namespace(&f.full_name) || self.error_site(offset).is_none() {
            return r;
        }
        if *self.ty(inner) == Type::Void {
            return self.trace_created(r, offset);
        }
        let s = r.into_struct_value();
        let err = self.b.build_extract_value(s, 1, "err").unwrap();
        let err = self.trace_created(err, offset);
        self.b.build_insert_value(s, err, 1, "").unwrap().into_struct_value().into()
    }

    /// `f(err)` when `err` is not NULL, else NULL: the trace calls run only
    /// on the error path, so a `?` that succeeds costs a null test.
    fn when_error(&mut self, err: BasicValueEnum<'ctx>, f: impl FnOnce(&mut Self) -> BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        let e = err.into_pointer_value();
        let nn = self.b.build_is_not_null(e, "").unwrap();
        let from = self.b.get_insert_block().unwrap();
        let on_err = self.block("err.trace");
        let done = self.block("err.done");
        self.b.build_conditional_branch(nn, on_err, done).unwrap();
        self.b.position_at_end(on_err);
        let r = f(self);
        let on_err_end = self.b.get_insert_block().unwrap();
        self.b.build_unconditional_branch(done).unwrap();
        self.b.position_at_end(done);
        let null = self.ptr_ty().const_null();
        let phi = self.b.build_phi(self.ptr_ty(), "err").unwrap();
        phi.add_incoming(&[(&null, from), (&r, on_err_end)]);
        phi.as_basic_value()
    }

    /// Records a `returned via` frame at `offset` with optional context
    /// `ctx` (a string; the error is wrapped with it). Consumes and returns
    /// the owned error.
    pub(crate) fn trace_returned(&mut self, err: BasicValueEnum<'ctx>, offset: u32, ctx: Option<BasicValueEnum<'ctx>>) -> BasicValueEnum<'ctx> {
        match self.error_site(offset) {
            Some(site) => {
                let c = ctx.unwrap_or_else(|| self.ptr_ty().const_null().into());
                self.when_error(err, |cg| cg.call_rt_v("zell_error_returned", &[err.into(), site.into(), c.into()]))
            }
            None => match ctx {
                Some(c) => {
                    let w = self.call_rt_v("zell_error_wrap", &[err.into(), c.into()]);
                    self.call_rt("zell_release", &[err.into()]);
                    w
                }
                None => err,
            },
        }
    }

    /// Ends a propagation at a `try ... catch` binding (or where an error
    /// leaves the thread): records the frame at `offset` (if `Some`) and
    /// settles a pending trace, attaching it to the error when `keep`.
    /// Consumes and returns the owned error; unchanged without traces.
    pub(crate) fn trace_caught(&mut self, err: BasicValueEnum<'ctx>, offset: Option<u32>, keep: bool) -> BasicValueEnum<'ctx> {
        if self.opts.no_error_trace || self.f.as_ref().and_then(|f| f.hir).is_none() {
            return err;
        }
        let in_std = self.f.as_ref().and_then(|f| f.hir).is_some_and(|f| crate::sema::stdlib::is_std_namespace(&f.full_name));
        let site = match offset {
            // std code records no frame but still settles a pending trace
            Some(_) if in_std => self.ptr_ty().const_null(),
            Some(o) => match self.error_site(o) {
                Some(s) => s,
                None => return err,
            },
            None => self.ptr_ty().const_null(),
        };
        let k = self.i64_ty().const_int(keep as u64, false);
        self.when_error(err, |cg| cg.call_rt_v("zell_error_caught", &[err.into(), site.into(), k.into()]))
    }

    /// Releases an error whose propagation ends unobserved (`try ... else`,
    /// a catch without a binding), dropping its pending trace.
    pub(crate) fn trace_discard(&mut self, err: BasicValueEnum<'ctx>) {
        if self.opts.no_error_trace {
            self.call_rt("zell_release", &[err.into()]);
        } else {
            self.call_rt("zell_error_discarded", &[err.into()]);
        }
    }

    /// Can the handler of `try ... catch local { handler }` observe the
    /// error's trace? False when every use of `local` is the receiver of
    /// `Message()` or an argument of `errors.Is`: the error then never
    /// escapes the handler and its pending trace can be dropped instead of
    /// attached (which would copy a shared error).
    pub(crate) fn handler_keeps_trace(&self, handler: &Block, local: LocalId) -> bool {
        let prog = self.prog;
        let is_local = |e: &Expr| matches!(e.kind, ExprKind::Local(l) if l == local);
        let mut uses: i64 = 0;
        let mut safe: i64 = 0;
        let mut captured = false;
        walk_block(prog, handler, &mut |e, _pos: Pos| {
            if is_local(e) {
                uses += 1;
            }
            match &e.kind {
                ExprKind::Call { callee: Callee::Interface { recv, method: 0 }, .. } if is_local(recv) && *prog.types.get(recv.ty) == Type::Error => safe += 1,
                ExprKind::Call { callee: Callee::Extern(x), args } if prog.externs[x.0 as usize].path == "errors.Is" => {
                    safe += args.iter().filter(|a| is_local(a)).count() as i64;
                }
                ExprKind::Closure { captures, .. } if captures.iter().any(|c| matches!(c, CaptureSource::Local(l) if *l == local)) => captured = true,
                _ => {}
            }
        });
        captured || uses > safe
    }
}
