//! Whole-program **may-suspend** analysis of `async` functions (ABI.md
//! 10.3 "Never-suspending functions", DESIGN.md "Асинхронность").
//!
//! An async function *may suspend* if its body awaits anything other than
//! a direct call of an async function that is itself proven never to
//! suspend: a task extern (`Delay`, `Yield`, channel, mutex, I/O ...), a
//! `Task` value not produced by a direct call (a variable, a field,
//! `task.All` / `task.Any`, the result of a closure, interface or function
//! value call), or a direct call of a may-suspend async function. The
//! analysis starts from "never suspends" for every function and iterates
//! to the greatest fixpoint, so self- and mutual recursion that only
//! awaits itself stays never-suspending. HIR has no generics, so every
//! instantiation is decided on its own.
//!
//! A never-suspending function is *plain* (codegen emits it as an
//! ordinary function with the coroutine's external ABI) when, in addition,
//! its body has no `defer` and no local or temporary of a type with a
//! destructor: synchronous frames do not run `defer` / `Drop` when a
//! panic unwinds them (DESIGN.md "Паника"), coroutine frames do.

use super::ownership::{has_dtor, walk_function, walk_function_stmts};
use super::{Callee, ExprKind, FuncId, Program, StmtKind};
use crate::types::{Type, TypeId};
use std::collections::HashSet;

/// The async functions codegen lowers as plain functions.
#[derive(Debug, Clone, Default)]
pub struct PlainAsync {
    /// Per function: an async function that never suspends and has no
    /// `defer` and no local or temporary owning a value with a destructor.
    /// `false` for every synchronous function.
    pub plain: Vec<bool>,
    /// Per function: its body can never change the current task context
    /// (10.8): every call in it is a direct call of an async function
    /// (which gives the context back itself) or of a synchronous function
    /// with the same property, and nothing in it runs a `Drop` or a
    /// user `String()`. A plain function with it needs no context save.
    pub ctx_neutral: Vec<bool>,
}

/// True if a value of type `t` owns, directly or through pointers,
/// slices, maps, tasks and fields, a value with a destructor (so that
/// releasing it may run `Drop`). Interfaces and `any` are not followed.
pub fn owns_drop(p: &Program, t: TypeId) -> bool {
    fn go(p: &Program, t: TypeId, seen: &mut HashSet<TypeId>) -> bool {
        if !seen.insert(t) {
            return false;
        }
        if has_dtor(p, t) {
            return true;
        }
        match p.types.get(t) {
            Type::Pointer(e) | Type::Slice(e) | Type::Array(e, _) | Type::Fallible(e) | Type::Task(e) => go(p, *e, seen),
            Type::Map(k, v) => go(p, *k, seen) || go(p, *v, seen),
            Type::Tuple(ts) => ts.iter().any(|t| go(p, *t, seen)),
            Type::Struct(d) => p.structs[d.0 as usize].fields.iter().any(|f| go(p, f.ty, seen)),
            Type::Enum(d) => p.enums[d.0 as usize].variants.iter().any(|v| v.fields.iter().any(|f| go(p, f.1, seen))),
            _ => false,
        }
    }
    go(p, t, &mut HashSet::new())
}

/// Runs the analysis over the whole program.
pub fn plain_async(p: &Program) -> PlainAsync {
    let n = p.funcs.len();
    // Per async function: suspends on its own, and the async callees it
    // awaits directly. Per function: touches the context on its own, and
    // the synchronous functions it calls directly.
    let mut suspends = vec![false; n];
    let mut deps: Vec<Vec<FuncId>> = vec![Vec::new(); n];
    let mut eligible = vec![false; n];
    let mut dirty = vec![false; n];
    let mut sync_deps: Vec<Vec<FuncId>> = vec![Vec::new(); n];
    for (i, f) in p.funcs.iter().enumerate() {
        let mut own = false;
        let mut drops = f.locals.iter().any(|l| owns_drop(p, l.ty));
        let mut ctx = false;
        walk_function(p, f, &mut |e, _| {
            match &e.kind {
                ExprKind::Await(t) => match &t.kind {
                    ExprKind::Call { callee: Callee::Func(g), .. } if p.funcs[g.0 as usize].is_async => deps[i].push(*g),
                    _ => own = true,
                },
                ExprKind::Call { callee: Callee::Func(g), .. } => {
                    if !p.funcs[g.0 as usize].is_async {
                        sync_deps[i].push(*g);
                    }
                }
                ExprKind::Call { .. } | ExprKind::ToString(_) | ExprKind::Print(_) => ctx = true,
                _ => {}
            }
            if !drops && owns_drop(p, e.ty) {
                drops = true;
            }
        });
        let mut defer = false;
        walk_function_stmts(p, f, &mut |s| {
            if matches!(s.kind, StmtKind::Defer(_)) {
                defer = true;
            }
        });
        dirty[i] = ctx || drops || defer;
        if f.is_async {
            suspends[i] = own;
            eligible[i] = !defer && !drops;
        }
    }
    // Greatest fixpoints of "never suspends" and "context-neutral":
    // propagate the opposite from callees to callers until nothing changes.
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..n {
            if !suspends[i] && deps[i].iter().any(|g| suspends[g.0 as usize]) {
                suspends[i] = true;
                changed = true;
            }
            if !dirty[i] && sync_deps[i].iter().any(|g| dirty[g.0 as usize]) {
                dirty[i] = true;
                changed = true;
            }
        }
    }
    let plain: Vec<bool> = (0..n).map(|i| p.funcs[i].is_async && !suspends[i] && eligible[i]).collect();
    let ctx_neutral = (0..n).map(|i| !dirty[i]).collect();
    PlainAsync { plain, ctx_neutral }
}
