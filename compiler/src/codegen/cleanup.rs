//! The cleanup stack: owned temporaries that are live while later
//! subexpressions are evaluated, and block scopes.
//!
//! * An expression that holds an owned value while it evaluates further
//!   operands [`hold`](CodeGen::hold)s it and [`unhold`](CodeGen::unhold)s
//!   it once consumed. Holding is free at run time: code is only emitted
//!   when an early exit leaves the expression.
//! * `return` and error returns (`?`, `try ... else return`) release every
//!   held temporary of the function; `break`/`continue` release the
//!   temporaries and block scopes above the loop's depth. Locals are
//!   released by the exit block (after the deferred calls) on returns.
//! * At the end of every statement the temporaries it left on the stack
//!   (owners pinned for `&slice[i]`/`&p.f`, owned bases of places) are
//!   released.
//! * A block scope releases the locals declared by its `Let`s at its end, in
//!   reverse order, and zeroes their slots (or clears their drop flags), so
//!   the release at function exit is a no-op for them.

use super::{Cleanup, CodeGen, Val};
use crate::hir::LocalId;
use crate::types::TypeId;
use inkwell::values::BasicValueEnum;

/// Index of a held temporary.
pub(crate) type Hold = Option<usize>;

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    pub(crate) fn depth(&self) -> usize {
        self.f.as_ref().map(|f| f.cleanups.len()).unwrap_or(0)
    }

    /// Registers `v` (if it is an owned managed value) as a live temporary.
    pub(crate) fn hold(&mut self, t: TypeId, v: Val<'ctx>) -> Hold {
        match v.v {
            Some(x) if v.owned && self.needs_rc(t) => self.hold_raw(t, x),
            _ => None,
        }
    }

    /// Registers an owned value of type `t`.
    pub(crate) fn hold_raw(&mut self, t: TypeId, x: BasicValueEnum<'ctx>) -> Hold {
        let f = self.f.as_mut()?;
        f.cleanups.push(Cleanup::Temp(t, x));
        f.pins.remove(&(f.cleanups.len() - 1));
        Some(f.cleanups.len() - 1)
    }

    /// The temporary was consumed (moved or released by the caller).
    pub(crate) fn unhold(&mut self, h: Hold) {
        let Some(i) = h else { return };
        let Some(f) = self.f.as_mut() else { return };
        if i < f.cleanups.len() {
            f.cleanups[i] = Cleanup::Done;
        }
        f.pins.remove(&i);
        while matches!(f.cleanups.last(), Some(Cleanup::Done)) {
            f.cleanups.pop();
        }
    }

    /// Releases (on the current path) and pops every temporary above
    /// `depth`.
    pub(crate) fn release_temps_from(&mut self, depth: usize) {
        let Some(f) = self.f.as_mut() else { return };
        if f.cleanups.len() <= depth {
            return;
        }
        let tail: Vec<Cleanup<'ctx>> = f.cleanups.drain(depth..).collect();
        let pins: Vec<Option<inkwell::values::IntValue<'ctx>>> = (depth..depth + tail.len()).map(|i| f.pins.remove(&i)).collect();
        if self.terminated_block() {
            return;
        }
        for (c, pin) in tail.into_iter().zip(pins).rev() {
            match c {
                // A pin's release cannot leave a new cycle behind: no
                // reference was stored meanwhile (cycles.rs, ABI.md 3.8).
                Cleanup::Temp(_, x) if pin.is_some() => self.release_pin(x, pin.unwrap()),
                Cleanup::Temp(t, x) => self.release_val(t, x),
                Cleanup::Scope(ls) => {
                    for l in ls.into_iter().rev() {
                        self.release_local_final(l, true);
                    }
                }
                Cleanup::Done => {}
            }
        }
    }

    /// Pops the entries above `depth` without releasing them (their values
    /// were moved into an aggregate or container).
    pub(crate) fn forget_from(&mut self, depth: usize) {
        if let Some(f) = self.f.as_mut() {
            f.cleanups.truncate(depth.min(f.cleanups.len()));
            f.pins.retain(|i, _| *i < depth);
        }
    }

    /// Emits the releases for an early exit to cleanup depth `depth`
    /// (without popping: the stack still describes the code after the
    /// exit point). Block scopes are released only when `scopes`.
    pub(crate) fn exit_cleanups(&mut self, depth: usize, scopes: bool) {
        let Some(f) = self.f.as_ref() else { return };
        if f.cleanups.len() <= depth {
            return;
        }
        let tail: Vec<Cleanup<'ctx>> = f.cleanups[depth..].to_vec();
        for c in tail.into_iter().rev() {
            match c {
                Cleanup::Temp(t, x) => self.release_val(t, x),
                Cleanup::Scope(ls) if scopes => {
                    for l in ls.into_iter().rev() {
                        self.release_local(l, true);
                    }
                }
                _ => {}
            }
        }
    }

    /// Opens a block scope.
    pub(crate) fn push_scope(&mut self) {
        if let Some(f) = self.f.as_mut() {
            f.cleanups.push(Cleanup::Scope(Vec::new()));
        }
    }

    /// Closes the innermost block scope (whose entry is at `depth`),
    /// releasing its locals on the current path.
    pub(crate) fn pop_scope(&mut self, depth: usize) {
        self.release_temps_from(depth);
    }

    /// Records local `l` in the innermost block scope.
    pub(crate) fn scope_local(&mut self, l: LocalId) {
        let Some(f) = self.f.as_mut() else { return };
        if let Some(Cleanup::Scope(ls)) = f.cleanups.iter_mut().rev().find(|c| matches!(c, Cleanup::Scope(_))) {
            if !ls.contains(&l) {
                ls.push(l);
            }
        }
    }

    pub(crate) fn terminated_block(&self) -> bool {
        self.b.get_insert_block().and_then(|b| b.get_terminator()).is_some()
    }
}
