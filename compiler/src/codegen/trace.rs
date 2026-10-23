//! Error return traces (DESIGN.md "Трасса ошибки", ABI.md §7.4): static
//! `zell_error_site` records `{file, func, line}` and the calls that store
//! them in errors.
//!
//! * Creation sites (`error(..)`, `errors.New`, `errors.Wrap`, a struct
//!   converted to `error`, and `return err` of an error that has no site yet,
//!   such as a sentinel) call `zell_error_created`.
//! * Every `?` and every `try ... catch err` that binds the error call
//!   `zell_error_returned` (which also wraps for `?("ctx")`).
//! * With [`super::Options::no_error_trace`] no site is emitted and the calls
//!   are elided (`?("ctx")` then calls `zell_error_wrap` directly).
//!
//! Sites are only recorded inside HIR functions (not in global
//! initialisers, so `return Sentinel` records the `return`).

use super::CodeGen;
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
}
