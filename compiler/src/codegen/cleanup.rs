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
//! * In a coroutine a held temporary is also stored into a frame slot (one
//!   per cleanup index and type, zeroed or flagged off when consumed),
//!   which the destroy path of a panic releases (ABI.md 8.1). Synchronous
//!   functions need no slot: the landing pad of each call releases the
//!   temporaries held at that call (`unwind.rs`).

use super::{Cleanup, CodeGen, TempSlot, Val};
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
        let i = self.f.as_ref()?.cleanups.len();
        let slot = if self.fs().spill_temps && !self.terminated_block() { Some(self.temp_slot(i, t)) } else { None };
        if let Some(s) = slot {
            self.b.build_store(s.ptr, x).unwrap();
            if let Some(fl) = s.flag {
                self.b.build_store(fl, self.ctx.bool_type().const_int(1, false)).unwrap();
            }
        }
        let f = self.f.as_mut()?;
        f.cleanups.push(Cleanup::Temp(t, x, slot));
        f.pins.remove(&i);
        Some(i)
    }

    /// The frame slot of a temporary held at cleanup index `i`.
    fn temp_slot(&mut self, i: usize, t: TypeId) -> TempSlot<'ctx> {
        if let Some(k) = self.fs().temp_slot_of.get(&(i, t)).copied() {
            return self.fs().temp_slots[k];
        }
        let ty = self.llty(t);
        let ptr = self.entry_alloca(ty, "tmp.slot", true);
        let flag = if self.has_dtor(t) { Some(self.entry_alloca(self.ctx.bool_type().into(), "tmp.live", true)) } else { None };
        let s = TempSlot { ptr, flag, ty: t };
        let f = self.fs_mut();
        f.temp_slots.push(s);
        f.temp_slot_of.insert((i, t), f.temp_slots.len() - 1);
        s
    }

    /// Marks a temporary's slot empty (on the current path): it was
    /// consumed or is about to be released.
    fn disarm_temp(&mut self, s: Option<TempSlot<'ctx>>) {
        let Some(s) = s else { return };
        if self.terminated_block() {
            return;
        }
        match s.flag {
            Some(fl) => self.b.build_store(fl, self.ctx.bool_type().const_zero()).unwrap(),
            None => {
                let z = self.llty(s.ty).const_zero();
                self.b.build_store(s.ptr, z).unwrap()
            }
        };
    }

    /// The temporary was consumed (moved or released by the caller).
    pub(crate) fn unhold(&mut self, h: Hold) {
        let Some(i) = h else { return };
        if let Some(Some(Cleanup::Temp(_, _, s))) = self.f.as_ref().map(|f| f.cleanups.get(i).cloned()) {
            self.disarm_temp(s);
        }
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
                Cleanup::Temp(_, x, s) if pin.is_some() => {
                    self.disarm_temp(s);
                    self.release_pin(x, pin.unwrap())
                }
                Cleanup::Temp(t, x, s) => {
                    self.disarm_temp(s);
                    self.release_val(t, x)
                }
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
        let slots: Vec<Option<TempSlot<'ctx>>> = match self.f.as_ref() {
            Some(f) => f.cleanups.iter().skip(depth).filter_map(|c| if let Cleanup::Temp(_, _, s) = c { Some(*s) } else { None }).collect(),
            None => Vec::new(),
        };
        for s in slots {
            self.disarm_temp(s);
        }
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
                Cleanup::Temp(t, x, s) => {
                    self.disarm_temp(s);
                    self.release_val(t, x)
                }
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
