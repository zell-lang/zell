//! Interior pointer variables (DESIGN.md "Память", `hir::ownership::Pin`).
//!
//! `var p = &xs[i]` stores an interior pointer (no header) in a local. The
//! local itself is never retained or released (like a borrowed parameter);
//! instead every statement that stores into it (a *site*) also stores
//! owned references to what keeps the target alive — its *pins* — in hidden
//! slots of the local: the slice of `&slice[i]` (its backing array), the
//! pointer of `&p.f`, the heap cell of `&s.f`, the pins of another interior
//! variable, or the stored pointer itself. The pins are released when the
//! local is (end of its block, function exit, re-execution of its `var`).
//! Sema (`sema::ownership`) guarantees that such a local is only read
//! through, compared, reassigned or passed to non-escaping parameters.

use super::{CodeGen, Val, R};
use crate::hir::ownership::{interior_pin, interior_vars, is_interior_addr, pointer_pin, Pin};
use crate::hir::{self, ExprKind, FuncId, LocalId};
use crate::types::TypeId;
use inkwell::values::{BasicValueEnum, PointerValue};
use std::collections::HashMap;

/// What a pin slot holds.
#[derive(Clone, Copy)]
pub(crate) enum PinTy {
    /// A heap object (a cell): `zell_retain`/`zell_release`.
    Obj,
    /// An owned value of this type (slice, pointer).
    Val(TypeId),
}

#[derive(Clone, Copy)]
pub(crate) struct PinSlot<'ctx> {
    ptr: PointerValue<'ctx>,
    ty: PinTy,
}

/// The interior pointer variables of the current function.
#[derive(Default)]
pub(crate) struct InteriorState<'a, 'ctx> {
    /// Per local: its pin slots (empty unless it is an interior variable
    /// whose sites need pins).
    pins: Vec<Vec<PinSlot<'ctx>>>,
    /// Per local: is it an interior pointer variable.
    is: Vec<bool>,
    /// Per site (stored value): the pin and the indices of the local's
    /// slots it fills.
    sites: HashMap<*const hir::Expr, (Pin<'a>, Vec<usize>)>,
}

impl<'a, 'ctx> CodeGen<'a, 'ctx> {
    /// Finds the interior pointer variables of `f` and allocates their pin
    /// slots (zeroed at entry). Their own slots are marked borrowed.
    pub(crate) fn setup_interior(&mut self, fid: FuncId, f: &'a hir::Function) {
        let iv = interior_vars(self.prog, f);
        if !iv.is.iter().any(|b| *b) {
            return;
        }
        let mut st = InteriorState { pins: vec![Vec::new(); f.locals.len()], is: iv.is.clone(), sites: HashMap::new() };
        let is = iv.is.clone();
        let is_iv = |l: LocalId| is.get(l.0 as usize).copied().unwrap_or(false);
        let mut sites = iv.sites.clone();
        // `Pin::Interior(q)` needs q's slots first (sema: q is declared earlier).
        sites.sort_by_key(|(l, _)| l.0);
        for (l, v) in sites {
            let pin = match &v.kind {
                ExprKind::AddrOf(inner) if is_interior_addr(v) => interior_pin(self.prog, fid, f, &self.facts, inner, &is_iv),
                _ => pointer_pin(fid, f, &self.facts, v, &is_iv),
            }
            .unwrap_or(Pin::None);
            let tys: Vec<PinTy> = match pin {
                Pin::None => Vec::new(),
                Pin::Root(r) => match &r.kind {
                    ExprKind::Local(s) if self.fs().locals[s.0 as usize].is_some_and(|s| s.cell) => vec![PinTy::Obj],
                    ExprKind::Upvalue(_) => vec![PinTy::Obj],
                    _ => Vec::new(),
                },
                Pin::Value(e) if self.needs_rc(e.ty) => vec![PinTy::Val(e.ty)],
                Pin::Owned if self.needs_rc(v.ty) => vec![PinTy::Val(v.ty)],
                Pin::Value(_) | Pin::Owned => Vec::new(),
                Pin::Interior(q) => st.pins[q.0 as usize].iter().map(|s| s.ty).collect(),
            };
            let mut idxs = Vec::new();
            for t in tys {
                let lt = match t {
                    PinTy::Obj => self.ptr_ty().into(),
                    PinTy::Val(t) => self.llty(t),
                };
                let ptr = self.entry_alloca(lt, "pin", true);
                idxs.push(st.pins[l.0 as usize].len());
                st.pins[l.0 as usize].push(PinSlot { ptr, ty: t });
            }
            st.sites.insert(v as *const hir::Expr, (pin, idxs));
        }
        for (i, b) in iv.is.iter().enumerate() {
            if *b {
                self.fs_mut().borrowed[i] = true;
            }
        }
        self.fs_mut().interior = Some(Box::new(st));
    }

    /// Is local `l` an interior pointer variable?
    pub(crate) fn is_interior_local(&self, l: LocalId) -> bool {
        self.fs().interior.as_ref().is_some_and(|st| st.is.get(l.0 as usize).copied().unwrap_or(false))
    }

    /// Releases (and zeroes) the pins of interior variable `l`.
    /// Every pin slot of the function (the unwind path reads them).
    pub(crate) fn interior_slot_ptrs(&self) -> Vec<PointerValue<'ctx>> {
        self.fs().interior.as_ref().map(|st| st.pins.iter().flatten().map(|s| s.ptr).collect()).unwrap_or_default()
    }

    pub(crate) fn release_pins(&mut self, l: LocalId) {
        let Some(slots) = self.fs().interior.as_ref().map(|st| st.pins[l.0 as usize].clone()) else { return };
        for s in slots {
            self.release_pin_slot(s);
        }
    }

    fn release_pin_slot(&mut self, s: PinSlot<'ctx>) {
        match s.ty {
            PinTy::Obj => {
                let v = self.b.build_load(self.ptr_ty(), s.ptr, "pin").unwrap();
                self.call_rt("zell_release", &[v.into()]);
                self.b.build_store(s.ptr, self.ptr_ty().const_null()).unwrap();
            }
            PinTy::Val(t) => {
                let lt = self.llty(t);
                let v = self.b.build_load(lt, s.ptr, "pin").unwrap();
                self.release_val(t, v);
                self.b.build_store(s.ptr, lt.const_zero()).unwrap();
            }
        }
    }

    /// `var l = value` / `l = value` for an interior pointer variable `l`.
    pub(crate) fn store_interior(&mut self, l: LocalId, value: &hir::Expr, fresh: bool) -> R<()> {
        let (pin, idxs) = self.fs().interior.as_ref().and_then(|st| st.sites.get(&(value as *const hir::Expr)).cloned()).expect("codegen: interior site without pins");
        // The pointer, then owned references to its owners.
        let (mut ptr, val): (BasicValueEnum<'ctx>, Option<Val<'ctx>>) = match &value.kind {
            ExprKind::AddrOf(inner) if is_interior_addr(value) => (self.place(inner)?.into(), None),
            _ => {
                let v = self.expr(value)?;
                (v.get(), Some(v))
            }
        };
        let mut news: Vec<BasicValueEnum<'ctx>> = Vec::new();
        match pin {
            Pin::None => {}
            Pin::Root(r) if !idxs.is_empty() => {
                let cell = match &r.kind {
                    ExprKind::Local(s) => {
                        let slot = self.fs().locals[s.0 as usize].expect("codegen: local without slot");
                        self.b.build_load(self.ptr_ty(), slot.ptr, "cell").unwrap()
                    }
                    ExprKind::Upvalue(i) => self.upvalue_cell_ptr(*i).into(),
                    _ => unreachable!("codegen: interior root"),
                };
                self.call_rt("zell_retain", &[cell.into()]);
                news.push(cell);
            }
            Pin::Root(_) => {}
            Pin::Value(e) if !idxs.is_empty() => {
                let v = self.expr(e)?;
                news.push(self.to_owned(e.ty, v));
            }
            Pin::Value(_) => {}
            Pin::Owned => {
                let v = val.expect("codegen: owned pin without value");
                if !idxs.is_empty() {
                    let o = self.to_owned(value.ty, v);
                    ptr = o;
                    news.push(o);
                }
            }
            Pin::Interior(q) => {
                let qs = self.fs().interior.as_ref().map(|st| st.pins[q.0 as usize].clone()).unwrap_or_default();
                for s in qs {
                    match s.ty {
                        PinTy::Obj => {
                            let v = self.b.build_load(self.ptr_ty(), s.ptr, "pin").unwrap();
                            self.call_rt("zell_retain", &[v.into()]);
                            news.push(v);
                        }
                        PinTy::Val(t) => {
                            let lt = self.llty(t);
                            let v = self.b.build_load(lt, s.ptr, "pin").unwrap();
                            self.retain_val(t, v);
                            news.push(v);
                        }
                    }
                }
            }
        }
        let slots = self.fs().interior.as_ref().map(|st| st.pins[l.0 as usize].clone()).unwrap_or_default();
        if fresh {
            self.dbg_declare(l);
            self.release_pins(l);
        } else {
            for &k in &idxs {
                self.release_pin_slot(slots[k]);
            }
        }
        for (&k, v) in idxs.iter().zip(news) {
            self.b.build_store(slots[k].ptr, v).unwrap();
        }
        let slot = self.fs().locals[l.0 as usize].expect("codegen: local without slot");
        self.b.build_store(slot.ptr, ptr).unwrap();
        Ok(())
    }
}
