//! Reference-cycle analysis of the type graph (DESIGN.md "Память" →
//! "Циклы").
//!
//! The nodes are the program's types; an edge `A → B` means a value of `A`
//! can hold a strong reference to (or contain) a value of `B`:
//!
//! - `*T → T`; `weak *T` has no edge;
//! - structs → their field types, data enums → their payload types,
//!   `[]T`, `[N]T`, `T!`, `Task<T>` → `T`, `map[K]V` → `K`, `V`, tuples →
//!   their element types;
//! - `func(...)` → the captured variable types of every lambda of that type
//!   (closures capture by reference; `defer` thunks are not values and are
//!   left out);
//! - an interface or `any` → every type converted to it somewhere in the
//!   program (whole-program, so this is exact rather than "any type");
//!   `error` has no edges: error values are runtime objects the collector
//!   does not walk.
//!
//! A type is *cyclic* if it lies in a strongly connected component with an
//! edge (two or more types, or one with an edge to itself): only values of
//! such types can be part of a reference cycle. [`check`] reports warning
//! W0001 once per user struct or enum of such a component (silenced by
//! `@allowCycle`; `@acyclic` removes the type's out-edges altogether: a
//! promise checked by `--leak-check`); code generation uses [`Cycles`] to allocate objects of
//! cyclic types with the cycle-collector flag and to emit their field
//! visitors (runtime/ABI.md §3.8).

use crate::diagnostics::{codes, Diagnostic, Diagnostics};
use crate::hir::dump::{type_name, DeclNames};
use crate::hir::{ExprKind, FuncId, FuncKind, Program, StmtKind};
use crate::types::{DeclId, Type, TypeId};
use std::collections::HashSet;

/// Result of the analysis, indexed by `TypeId`.
#[derive(Debug, Default, Clone)]
pub struct Cycles {
    /// The type lies on a cycle of the type graph.
    pub cyclic: Vec<bool>,
    /// A value of the type may hold a strong reference to an object of a
    /// cyclic type (so a cycle-collector visitor of it is not empty).
    pub holds: Vec<bool>,
    /// Strongly connected component of each type.
    pub scc: Vec<u32>,
    /// Lambdas whose environment holds a cell of a cyclic type.
    pub lambdas: HashSet<FuncId>,
}

impl Cycles {
    pub fn is_cyclic(&self, t: TypeId) -> bool {
        self.cyclic.get(t.0 as usize).copied().unwrap_or(false)
    }

    pub fn holds_cyclic(&self, t: TypeId) -> bool {
        self.holds.get(t.0 as usize).copied().unwrap_or(false)
    }

    /// True if the program has any cyclic type.
    pub fn any(&self) -> bool {
        self.cyclic.iter().any(|c| *c)
    }
}

thread_local! {
    static DENY_WARNINGS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `--deny-warnings`: every warning of the front end becomes an error (for
/// analyses run on this thread afterwards; the CLI analyses on its main
/// thread).
pub fn set_deny_warnings(on: bool) {
    DENY_WARNINGS.with(|d| d.set(on));
}

pub fn deny_warnings() -> bool {
    DENY_WARNINGS.with(|d| d.get())
}

/// Builds the type reference graph of `p` and finds its cycles.
pub fn analyze(p: &Program) -> Cycles {
    let n = p.types.len();
    let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (i, edges) in adj.iter_mut().enumerate() {
        let t = TypeId(i as u32);
        match p.types.get(t) {
            Type::Pointer(x) | Type::Slice(x) | Type::Array(x, _) | Type::Fallible(x) | Type::Task(x) => edges.push(x.0),
            Type::Map(k, v) => edges.extend([k.0, v.0]),
            Type::Tuple(ts) => edges.extend(ts.iter().map(|t| t.0)),
            // `@acyclic` promises that values of the type never lie on a
            // cycle: it has no out-edges, so it (and what only it would
            // close) is not cyclic, never flagged, recorded or visited.
            Type::Struct(d) => {
                if let Some(s) = p.structs.get(d.0 as usize).filter(|s| !has_attr(&s.attrs, "acyclic")) {
                    edges.extend(s.fields.iter().map(|f| f.ty.0));
                }
            }
            Type::Enum(d) => {
                if let Some(e) = p.enums.get(d.0 as usize).filter(|e| !has_attr(&e.attrs, "acyclic")) {
                    edges.extend(e.variants.iter().flat_map(|v| v.fields.iter().map(|f| f.1 .0)));
                }
            }
            _ => {}
        }
    }
    // Closures and interface conversions found in the bodies.
    let mut defer_thunks: HashSet<FuncId> = HashSet::new();
    for f in &p.funcs {
        crate::hir::ownership::walk_function_stmts(p, f, &mut |s| {
            if let StmtKind::Defer(e) = &s.kind {
                if let ExprKind::Closure { func, .. } = &e.kind {
                    defer_thunks.insert(*func);
                }
            }
        });
    }
    let mut closures: Vec<(TypeId, FuncId)> = Vec::new();
    let mut convs: Vec<(TypeId, TypeId)> = Vec::new();
    let mut visit = |e: &crate::hir::Expr| match &e.kind {
        ExprKind::Closure { func, .. } if !defer_thunks.contains(func) => closures.push((e.ty, *func)),
        ExprKind::EnvClosures { funcs, .. } => {
            if let Type::Tuple(tys) = p.types.get(e.ty) {
                closures.extend(tys.iter().copied().zip(funcs.iter().copied()));
            }
        }
        ExprKind::ToInterface { value } => convs.push((e.ty, value.ty)),
        _ => {}
    };
    for f in &p.funcs {
        crate::hir::ownership::walk_function(p, f, &mut |e, _| visit(e));
    }
    for g in &p.globals {
        if let Some(init) = &g.init {
            crate::hir::ownership::walk_expr(p, init, crate::hir::ownership::Pos::Store, &mut |e, _| visit(e));
        }
    }
    for (fty, func) in &closures {
        if let Some(f) = p.funcs.get(func.0 as usize) {
            if let Some(edges) = adj.get_mut(fty.0 as usize) {
                edges.extend(f.captures.iter().map(|c| c.ty.0));
            }
        }
    }
    for (to, from) in &convs {
        // `error` values are runtime error objects the collector does not
        // walk (ABI.md 3.8): no edges, so they never make a type cyclic.
        if matches!(p.types.get(*to), Type::Error) {
            continue;
        }
        if let Some(edges) = adj.get_mut(to.0 as usize) {
            edges.push(from.0);
        }
    }
    for e in adj.iter_mut() {
        e.sort_unstable();
        e.dedup();
    }
    let scc = tarjan(&adj);
    let mut size = vec![0u32; n];
    for c in &scc {
        size[*c as usize] += 1;
    }
    let cyclic: Vec<bool> = (0..n).map(|i| size[scc[i] as usize] > 1 || adj[i].contains(&(i as u32))).collect();
    let lambdas: HashSet<FuncId> = p
        .funcs
        .iter()
        .enumerate()
        .filter(|(_, f)| matches!(f.kind, FuncKind::Lambda { .. }) && f.captures.iter().any(|c| cyclic[c.ty.0 as usize]))
        .map(|(i, _)| FuncId(i as u32))
        .collect();
    let func_holds: HashSet<TypeId> = closures.iter().filter(|(_, f)| lambdas.contains(f)).map(|(t, _)| *t).collect();
    let any_cyclic = cyclic.iter().any(|c| *c);
    // `holds` is the least fixpoint of the component rule.
    let mut holds = vec![false; n];
    loop {
        let mut changed = false;
        for i in 0..n {
            if holds[i] {
                continue;
            }
            let t = TypeId(i as u32);
            let h = match p.types.get(t) {
                Type::Pointer(x) => cyclic[x.0 as usize],
                Type::Slice(x) | Type::Array(x, _) | Type::Fallible(x) => holds[x.0 as usize],
                Type::Map(k, v) => holds[k.0 as usize] || holds[v.0 as usize],
                Type::Tuple(ts) => ts.iter().any(|t| holds[t.0 as usize]),
                Type::Struct(d) => p.structs.get(d.0 as usize).is_some_and(|s| s.fields.iter().any(|f| holds[f.ty.0 as usize])),
                Type::Enum(d) => p.enums.get(d.0 as usize).is_some_and(|e| e.variants.iter().any(|v| v.fields.iter().any(|f| holds[f.1 .0 as usize]))),
                Type::Func(_) => func_holds.contains(&t),
                Type::Interface(_) | Type::Any => any_cyclic,
                _ => false,
            };
            if h {
                holds[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Cycles { cyclic, holds, scc, lambdas }
}

/// Iterative Tarjan: the component index of every node.
fn tarjan(adj: &[Vec<u32>]) -> Vec<u32> {
    let n = adj.len();
    const UNSET: u32 = u32::MAX;
    let mut index = vec![UNSET; n];
    let mut low = vec![0u32; n];
    let mut on = vec![false; n];
    let mut comp = vec![UNSET; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut next = 0u32;
    let mut ncomp = 0u32;
    for root in 0..n {
        if index[root] != UNSET {
            continue;
        }
        let mut call: Vec<(u32, usize)> = vec![(root as u32, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root as u32);
        on[root] = true;
        while let Some(&mut (v, ref mut ei)) = call.last_mut() {
            let vu = v as usize;
            if *ei < adj[vu].len() {
                let w = adj[vu][*ei] as usize;
                *ei += 1;
                if index[w] == UNSET {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w as u32);
                    on[w] = true;
                    call.push((w as u32, 0));
                } else if on[w] {
                    low[vu] = low[vu].min(index[w]);
                }
                continue;
            }
            call.pop();
            if let Some(&(parent, _)) = call.last() {
                let pu = parent as usize;
                low[pu] = low[pu].min(low[vu]);
            }
            if low[vu] == index[vu] {
                loop {
                    let w = stack.pop().unwrap() as usize;
                    on[w] = false;
                    comp[w] = ncomp;
                    if w == vu {
                        break;
                    }
                }
                ncomp += 1;
            }
        }
    }
    comp
}

/// Short declaration names (`Node`, not `app.models.Node`) for messages.
struct Short<'a>(&'a Program);

impl DeclNames for Short<'_> {
    fn struct_name(&self, id: u32) -> String {
        self.0.structs[id as usize].name.clone()
    }
    fn interface_name(&self, id: u32) -> String {
        self.0.interfaces[id as usize].name.clone()
    }
    fn enum_name(&self, id: u32) -> String {
        self.0.enums[id as usize].name.clone()
    }
}

pub fn has_attr(attrs: &[crate::hir::Attr], name: &str) -> bool {
    attrs.iter().any(|a| a.kind == crate::hir::AttrKind::Builtin(name.into()))
}

/// The suggestion of a warning about a field of type `t`.
fn advice(p: &Program, t: TypeId) -> String {
    let names = Short(p);
    match p.types.get(t) {
        Type::Pointer(x) => format!("consider weak *{} or a Drop that breaks the cycle", type_name(&p.types, *x, &names)),
        Type::Slice(e) | Type::Array(e, _) if matches!(p.types.get(*e), Type::Pointer(_)) => {
            let Type::Pointer(x) = p.types.get(*e) else { unreachable!() };
            format!("consider weak *{} elements or a Drop that breaks the cycle", type_name(&p.types, *x, &names))
        }
        Type::Func(_) => "consider capturing a weak *T instead, or a Drop that clears the closure".into(),
        _ => "consider a weak reference or a Drop that breaks the cycle".into(),
    }
}

/// Reports W0001 for every user struct and enum that can form a reference
/// cycle (once per type, at its first field on the cycle), unless it has
/// `@allowCycle` or a type of its component has a `weak` reference into
/// the component (`--deny-warnings` turns them into errors, see
/// [`super::analyze_with_policy`]).
pub fn check(p: &Program, diags: &mut Diagnostics) {
    let c = analyze(p);
    if !c.any() {
        return;
    }
    let names = Short(p);
    // Components whose types already hold a `weak` reference into the
    // component (a tree with weak parent pointers, an observer list): the
    // ownership direction is designed, so they are not reported.
    let weak_of = |t: TypeId| -> Option<TypeId> {
        match p.types.get(t) {
            Type::Weak(x) => Some(*x),
            Type::Slice(e) | Type::Array(e, _) => match p.types.get(*e) {
                Type::Weak(x) => Some(*x),
                _ => None,
            },
            Type::Map(_, v) => match p.types.get(*v) {
                Type::Weak(x) => Some(*x),
                _ => None,
            },
            _ => None,
        }
    };
    let mut designed: HashSet<u32> = HashSet::new();
    for (sid, s) in p.structs.iter().enumerate() {
        let Some(t) = p.types.lookup(&Type::Struct(DeclId(sid as u32))) else { continue };
        let comp = c.scc[t.0 as usize];
        if s.fields.iter().filter_map(|f| weak_of(f.ty)).any(|x| c.scc[x.0 as usize] == comp) {
            designed.insert(comp);
        }
    }
    let mut seen = HashSet::new();
    for (sid, s) in p.structs.iter().enumerate() {
        if crate::sema::stdlib::is_std_namespace(&s.full_name) || has_attr(&s.attrs, "allowCycle") {
            continue;
        }
        let Some(t) = p.types.lookup(&Type::Struct(DeclId(sid as u32))) else { continue };
        if !c.is_cyclic(t) {
            continue;
        }
        let comp = c.scc[t.0 as usize];
        if designed.contains(&comp) {
            continue;
        }
        let Some(f) = s.fields.iter().find(|f| c.scc[f.ty.0 as usize] == comp && c.is_cyclic(f.ty)) else { continue };
        if !seen.insert((s.file, f.span.start, f.span.end)) {
            continue;
        }
        let ft = type_name(&p.types, f.ty, &names);
        let msg = format!("{} type {} can form a reference cycle through field {} ({ft}); {}", codes::REFERENCE_CYCLE, s.name, f.name, advice(p, f.ty));
        diags.push(Diagnostic::warning(msg, f.span).with_code(codes::REFERENCE_CYCLE).in_file(s.file).with_help("the cycle collector frees such cycles at run time; @allowCycle on the type silences this warning"));
    }
    for (eid, e) in p.enums.iter().enumerate() {
        if crate::sema::stdlib::is_std_namespace(&e.full_name) || has_attr(&e.attrs, "allowCycle") {
            continue;
        }
        let Some(t) = p.types.lookup(&Type::Enum(DeclId(eid as u32))) else { continue };
        if !c.is_cyclic(t) {
            continue;
        }
        let comp = c.scc[t.0 as usize];
        if designed.contains(&comp) {
            continue;
        }
        let Some((v, (fname, fty))) = e.variants.iter().find_map(|v| v.fields.iter().find(|f| c.scc[f.1 .0 as usize] == comp && c.is_cyclic(f.1)).map(|f| (v, f))) else { continue };
        if !seen.insert((e.file, v.span.start, v.span.end)) {
            continue;
        }
        let ft = type_name(&p.types, *fty, &names);
        let field = if fname.is_empty() || fname.chars().all(|ch| ch.is_ascii_digit()) { v.name.clone() } else { format!("{}.{fname}", v.name) };
        let msg = format!("{} type {} can form a reference cycle through field {field} ({ft}); {}", codes::REFERENCE_CYCLE, e.name, advice(p, *fty));
        diags.push(Diagnostic::warning(msg, v.span).with_code(codes::REFERENCE_CYCLE).in_file(e.file));
    }
}
