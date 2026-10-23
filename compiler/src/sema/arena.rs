//! `arena` intrinsics: `rtArenaNew<T>(h)` and `rtArenaAlloc<T>(h, n)`,
//! used by `Arena.New<T>` / `Arena.Alloc<T>` in `stdlib_src/arena.zl`
//! (DESIGN.md "Арены", runtime/ABI.md 3.6), and the sized-slice intrinsic
//! `encoding.json.rtMake<T>(n)` of the typed JSON decoder
//! (`reflect_src/json.zl`), lowered the same way.

use super::coerce::Ctx;
use super::stdlib::Special;
use super::*;
use crate::hir::{Builtin, ExprKind};

impl<'a> Sema<'a> {
    /// A call of a special function with explicit type arguments, or `None`
    /// if `sp` takes none.
    pub(crate) fn typed_special(&mut self, sp: Special, targs: &[ast::TypeExpr], args: &[ast::Expr], span: Span) -> Option<hir::Expr> {
        if !matches!(sp, Special::ArenaNew | Special::ArenaAlloc | Special::JsonMake) {
            return None;
        }
        if targs.len() != 1 {
            self.err(format!("wrong number of type arguments: want 1, got {}", targs.len()), span);
            self.check_args_loose(args);
            return Some(self.invalid(span));
        }
        let t = self.resolve_type(&targs[0]);
        let want = if sp == Special::ArenaAlloc { 2 } else { 1 };
        if args.len() != want {
            self.err(format!("wrong number of arguments: want {want}, got {}", args.len()), span);
            self.check_args_loose(args);
            return Some(self.invalid(span));
        }
        let int = self.tt.int;
        let hargs: Vec<hir::Expr> = args.iter().map(|a| self.expr_to(a, int, Ctx::Other)).collect();
        if self.tt.is_invalid(t) {
            return Some(self.invalid(span));
        }
        let (op, ty) = match sp {
            Special::ArenaNew => (Builtin::ArenaNew, self.tt.pointer(t)),
            Special::JsonMake => (Builtin::SliceMake, self.tt.slice(t)),
            _ => (Builtin::ArenaSlice, self.tt.slice(t)),
        };
        Some(mk(ExprKind::Builtin { op, args: hargs }, ty, span))
    }
}
