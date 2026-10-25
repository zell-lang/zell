//! Borrowed locals (DESIGN.md "Память", "Заимствованные локальные"):
//! local variables that hold a value read from a place without retaining
//! it, because nothing can release that value while the variable lives.
//!
//! A function is *heap-pure* when running it can neither overwrite a
//! reference stored in the heap (or in a captured, address-taken or global
//! variable) nor run code that might: it is not `async` or a lambda, has no
//! `defer`, assigns only to plain local variables, calls only heap-pure
//! functions and runtime functions from a fixed list of read-only ones (no
//! interface methods, closures, `append`, `delete`, `await`), and handles no
//! value that may own an object whose `Drop` is not heap-pure.
//!
//! In a heap-pure function, a local `x` of a pointer, string or slice type
//! is *borrowed* when it is defined exactly once and never assigned or
//! captured, and its value comes from
//!
//! * the element variable of `for x in s` over a slice (the loop keeps the
//!   slice's backing array alive), or
//! * `var x = path`, where `path` reads through fields, dereferences and
//!   slice/array elements (indexed by a variable or constant) from a *root*
//!   variable: a parameter that is never assigned, or a variable or loop
//!   variable defined once and never assigned (borrowed or not).
//!
//! The root holds (or, for a parameter, its caller holds) a reference for
//! at least as long as `x` is in scope, every link of the path is a
//! reference stored in the heap, and a heap-pure function overwrites none
//! of them, so the value of `x` stays alive without a count of its own.
//! Objects of types that may lie on reference cycles are never borrowed
//! (the cycle collector's pins expect counted references).

use super::ownership::{interior_vars, walk_function, walk_function_stmts, StackKind};
use super::*;
use crate::types::Type;
use std::collections::HashMap;

/// Runtime functions that only read their arguments (and may allocate and
/// return new values). Prefixes of C symbols; none of them takes a
/// callback or writes to a reference it was given.
const PURE_EXTERN_PREFIXES: &[&str] = &[
    "zell_string_",
    "zell_strings_",
    "zell_strconv_",
    "zell_math_",
    "zell_unicode_",
];

/// What each function does that the heap-purity fixpoint needs.
struct FnSummary {
    /// Impure regardless of its callees.
    local_impure: bool,
    callees: Vec<FuncId>,
    /// Types of the values it handles (expressions and locals).
    types: Vec<TypeId>,
}

/// A parameter type through which a runtime function could reach user code
/// (callbacks, `Message()` of errors, methods of interface values) or write.
fn opaque_param(p: &Program, t: TypeId) -> bool {
    match p.types.get(t) {
        Type::Func(_) | Type::Pointer(_) | Type::Any | Type::Error | Type::Interface(_) | Type::Struct(_) => true,
        Type::Slice(e) | Type::Array(e, _) => opaque_param(p, *e),
        _ => false,
    }
}

fn extern_pure(p: &Program, x: ExternId) -> bool {
    let e = &p.externs[x.0 as usize];
    PURE_EXTERN_PREFIXES.iter().any(|pre| e.symbol.starts_with(pre)) && !e.params.iter().any(|t| opaque_param(p, *t))
}

fn summarize(p: &Program, f: &Function) -> FnSummary {
    let mut s = FnSummary { local_impure: f.is_async || matches!(f.kind, FuncKind::Lambda { .. }), callees: Vec::new(), types: Vec::new() };
    if s.local_impure {
        return s;
    }
    let mut impure = false;
    walk_function_stmts(p, f, &mut |st| match &st.kind {
        StmtKind::Assign { target, .. } | StmtKind::CompoundAssign { target, .. } => {
            let plain = matches!(&target.kind, ExprKind::Local(l) if { let loc = &f.locals[l.0 as usize]; !loc.captured && !loc.address_taken });
            if !plain {
                impure = true;
            }
        }
        StmtKind::Defer(_) => impure = true,
        _ => {}
    });
    let mut callees = Vec::new();
    let mut types = Vec::new();
    walk_function(p, f, &mut |e, _| {
        types.push(e.ty);
        match &e.kind {
            ExprKind::Call { callee, .. } => match callee {
                Callee::Func(g) => callees.push(*g),
                Callee::Extern(x) => {
                    if !extern_pure(p, *x) {
                        impure = true;
                    }
                }
                Callee::Interface { .. } | Callee::Value(_) => impure = true,
            },
            ExprKind::Builtin { op, .. } => {
                if matches!(op, Builtin::Append | Builtin::AppendSlice | Builtin::Delete | Builtin::ErrorsAs | Builtin::TaskAny) {
                    impure = true;
                }
            }
            ExprKind::Await(_) | ExprKind::TaskAll(_) | ExprKind::Closure { .. } | ExprKind::EnvClosures { .. } | ExprKind::Upvalue(_) => impure = true,
            // Formatting an error or a struct may run its methods.
            ExprKind::ToString(v) => {
                if !matches!(p.types.get(v.ty), Type::Bool | Type::Int(_) | Type::Float32 | Type::Float64 | Type::Char | Type::String) {
                    impure = true;
                }
            }
            _ => {}
        }
    });
    types.extend(f.locals.iter().map(|l| l.ty));
    types.sort_unstable_by_key(|t| t.0);
    types.dedup();
    callees.sort_unstable_by_key(|g| g.0);
    callees.dedup();
    s.local_impure = impure;
    s.callees = callees;
    s.types = types;
    s
}

/// Whether a value of type `t` may own an object of a struct in `bad`
/// (memoized in `memo`; interfaces, closures and tasks may own anything).
fn may_own(p: &Program, t: TypeId, bad: &[bool], memo: &mut HashMap<TypeId, bool>) -> bool {
    if let Some(r) = memo.get(&t) {
        return *r;
    }
    memo.insert(t, false); // cycles in the type graph: assume no until shown
    let r = match p.types.get(t) {
        Type::Error | Type::Any | Type::Interface(_) | Type::Func(_) | Type::Task(_) => true,
        Type::Pointer(x) | Type::Slice(x) | Type::Array(x, _) | Type::Fallible(x) => may_own(p, *x, bad, memo),
        Type::Map(k, v) => may_own(p, *k, bad, memo) || may_own(p, *v, bad, memo),
        Type::Tuple(ts) => ts.clone().iter().any(|x| may_own(p, *x, bad, memo)),
        Type::Struct(d) => bad[d.0 as usize] || p.structs[d.0 as usize].fields.iter().map(|f| f.ty).collect::<Vec<_>>().into_iter().any(|x| may_own(p, x, bad, memo)),
        Type::Enum(d) => p.enums[d.0 as usize].variants.iter().flat_map(|v| v.fields.iter().map(|f| f.1)).collect::<Vec<_>>().into_iter().any(|x| may_own(p, x, bad, memo)),
        _ => false,
    };
    memo.insert(t, r);
    r
}

/// Heap-purity of every function (greatest fixpoint over calls and the
/// `Drop` methods of the values each function handles).
pub fn pure_functions(p: &Program) -> Vec<bool> {
    let sums: Vec<FnSummary> = p.funcs.iter().map(|f| summarize(p, f)).collect();
    let mut pure: Vec<bool> = sums.iter().map(|s| !s.local_impure).collect();
    let drops: Vec<Option<FuncId>> = (0..p.structs.len() as u32).map(|sid| super::ownership::drop_method(p, sid)).collect();
    loop {
        let bad: Vec<bool> = drops.iter().map(|d| d.is_some_and(|g| !pure[g.0 as usize])).collect();
        let any_bad = bad.iter().any(|b| *b);
        let mut memo = HashMap::new();
        let mut changed = false;
        for (i, s) in sums.iter().enumerate() {
            if !pure[i] {
                continue;
            }
            let ok = s.callees.iter().all(|g| pure[g.0 as usize]) && (!any_bad || !s.types.iter().any(|t| may_own(p, *t, &bad, &mut memo)));
            if !ok {
                pure[i] = false;
                changed = true;
            }
        }
        if !changed {
            return pure;
        }
    }
}

/// How a local is defined (for [`borrowed_locals`]).
#[derive(Clone, Copy)]
enum Def<'e> {
    None,
    Let(&'e Expr),
    ForElem,
    Other,
}

/// The borrowed locals of function `f` (`pure` from [`pure_functions`],
/// `stack` from [`super::ownership::stack_locals`], `cyclic(t)`: a value of
/// type `t` may reference an object of a cyclic type). `ZELL_NO_BORROW=1`
/// turns the analysis off (for measurements).
pub fn borrowed_locals(p: &Program, fid: FuncId, f: &Function, pure: &[bool], stack: &[StackKind], cyclic: &dyn Fn(TypeId) -> bool) -> Vec<bool> {
    let n = f.locals.len();
    let mut out = vec![false; n];
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !pure.get(fid.0 as usize).copied().unwrap_or(false) || *OFF.get_or_init(|| std::env::var_os("ZELL_NO_BORROW").is_some()) {
        return out;
    }
    let mut def: Vec<Def> = vec![Def::None; n];
    let mut assigned = vec![false; n];
    fn note<'e>(d: &mut Def<'e>, new: Def<'e>) {
        *d = if matches!(d, Def::None) { new } else { Def::Other };
    }
    fn collect<'e>(b: &'e Block, p: &Program, def: &mut Vec<Def<'e>>, assigned: &mut Vec<bool>) {
        for s in &b.stmts {
            stmt(s, p, def, assigned);
        }
    }
    fn expr<'e>(e: &'e Expr, p: &Program, def: &mut Vec<Def<'e>>, assigned: &mut Vec<bool>) {
        match &e.kind {
            ExprKind::TryCatch { value, handler, err, .. } => {
                if let Some(l) = err {
                    note(&mut def[l.0 as usize], Def::Other);
                }
                expr(value, p, def, assigned);
                collect(handler, p, def, assigned);
            }
            ExprKind::TryElse { value, .. } => expr(value, p, def, assigned),
            _ => {}
        }
    }
    fn stmt<'e>(s: &'e Stmt, p: &Program, def: &mut Vec<Def<'e>>, assigned: &mut Vec<bool>) {
        match &s.kind {
            StmtKind::Let { local, init } => {
                note(&mut def[local.0 as usize], init.as_ref().map_or(Def::Other, Def::Let));
                if let Some(e) = init {
                    expr(e, p, def, assigned);
                }
            }
            StmtKind::LetTuple { locals, init } => {
                for l in locals.iter().flatten() {
                    note(&mut def[l.0 as usize], Def::Other);
                }
                expr(init, p, def, assigned);
            }
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                if let ExprKind::Local(l) = &target.kind {
                    assigned[l.0 as usize] = true;
                }
                expr(value, p, def, assigned);
            }
            StmtKind::Expr(e) | StmtKind::Discard(e) => expr(e, p, def, assigned),
            StmtKind::If { then, else_, .. } => {
                collect(then, p, def, assigned);
                if let Some(e) = else_ {
                    collect(e, p, def, assigned);
                }
            }
            StmtKind::Loop { body, .. } => collect(body, p, def, assigned),
            StmtKind::ForRange { var, body, .. } => {
                if let Some(l) = var {
                    note(&mut def[l.0 as usize], Def::Other);
                }
                collect(body, p, def, assigned);
            }
            StmtKind::ForSlice { index, elem, seq, body, .. } => {
                if let Some(l) = index {
                    note(&mut def[l.0 as usize], Def::Other);
                }
                if let Some(l) = elem {
                    let slice = matches!(p.types.get(seq.ty), Type::Slice(_));
                    note(&mut def[l.0 as usize], if slice { Def::ForElem } else { Def::Other });
                }
                collect(body, p, def, assigned);
            }
            StmtKind::ForString { index, ch, body, .. } => {
                for l in [index, ch].into_iter().flatten() {
                    note(&mut def[l.0 as usize], Def::Other);
                }
                collect(body, p, def, assigned);
            }
            StmtKind::ForMap { key, value, body, .. } => {
                for l in [key, value].into_iter().flatten() {
                    note(&mut def[l.0 as usize], Def::Other);
                }
                collect(body, p, def, assigned);
            }
            StmtKind::Switch(sw) => {
                if let Some((l, _)) = &sw.subject {
                    note(&mut def[l.0 as usize], Def::Other);
                }
                for c in &sw.cases {
                    collect(&c.body, p, def, assigned);
                }
                if let Some(d) = &sw.default {
                    collect(d, p, def, assigned);
                }
            }
            StmtKind::Block(b) => collect(b, p, def, assigned),
            StmtKind::TryBlock { body, handler, err } => {
                if let Some(l) = err {
                    note(&mut def[l.0 as usize], Def::Other);
                }
                collect(body, p, def, assigned);
                collect(handler, p, def, assigned);
            }
            StmtKind::Return(_) | StmtKind::Break(_) | StmtKind::Continue(_) | StmtKind::Defer(_) => {}
        }
    }
    collect(&f.body, p, &mut def, &mut assigned);

    let interior = interior_vars(p, f).is;
    let plain = |l: usize| {
        let loc = &f.locals[l];
        !loc.captured && !loc.address_taken && !assigned[l] && stack[l] == StackKind::Normal && !interior[l]
    };
    // A root keeps its value for as long as a variable defined from it lives.
    let root_ok = |l: usize| {
        plain(l)
            && match f.locals[l].kind {
                LocalKind::Param | LocalKind::This => matches!(def[l], Def::None),
                LocalKind::Var | LocalKind::LoopVar => matches!(def[l], Def::Let(_) | Def::ForElem),
                _ => false,
            }
    };
    fn path(e: &Expr, p: &Program, root_ok: &dyn Fn(usize) -> bool) -> bool {
        match &e.kind {
            ExprKind::Local(r) => root_ok(r.0 as usize),
            ExprKind::Field { base, .. } => path(base, p, root_ok),
            ExprKind::Deref(x) => path(x, p, root_ok),
            ExprKind::Index { base, index } => {
                matches!(p.types.get(base.ty), Type::Slice(_) | Type::Array(..))
                    && matches!(index.kind, ExprKind::Local(_) | ExprKind::Const(_))
                    && path(base, p, root_ok)
            }
            _ => false,
        }
    }
    for (i, loc) in f.locals.iter().enumerate() {
        if !matches!(loc.kind, LocalKind::Var | LocalKind::LoopVar) || !plain(i) || cyclic(loc.ty) {
            continue;
        }
        if !matches!(p.types.get(loc.ty), Type::Pointer(_) | Type::String | Type::Slice(_)) {
            continue;
        }
        out[i] = match def[i] {
            Def::ForElem => true,
            Def::Let(e) => path(e, p, &root_ok),
            _ => false,
        };
    }
    out
}

/// Functions whose result is *borrowed from their parameters* (DESIGN.md
/// "Память", "Заимствованные временные"): a non-`async`, non-lambda
/// function without `defer` that assigns only to plain local variables,
/// never assigns, captures or takes the address of a parameter, takes
/// every parameter borrowed (`param_ok`), returns a `*T`, `string`, `[]T`
/// or struct value of a non-cyclic type, and whose every `return e` has
/// `e` a parameter, or an inline field path through struct values from a
/// parameter or from the dereference of a pointer parameter (`this`,
/// `this.d`, which is `(*this).d` for a receiver), or a struct literal
/// whose every reference-holding field is such a path
/// (`Value{d: this.d, n: ...}`).
///
/// The references in its result are then copies of references held by
/// the caller's argument values (or stored in the places they point to),
/// so direct calls get the result without a count of its own: the body
/// does not retain what it returns; a caller whose arguments stay held to
/// the end of the statement treats the result as borrowed; every other
/// use (vtables, function values, reflection, a call whose argument for
/// a dereferenced parameter is not the address of a stable place) goes
/// through a retaining wrapper. For each eligible function the result
/// lists, per parameter, whether a returned path reads *through* it (the
/// caller must then pass the address of a place that no code run during
/// the statement can overwrite). `ZELL_NO_BORROW=1` turns this off too.
pub fn borrow_returning(p: &Program, param_ok: &dyn Fn(FuncId, usize) -> bool, cyclic: &dyn Fn(TypeId) -> bool) -> Vec<Option<Vec<bool>>> {
    if std::env::var_os("ZELL_NO_BORROW").is_some() {
        return vec![None; p.funcs.len()];
    }
    p.funcs.iter().enumerate().map(|(i, f)| returns_borrowed(p, FuncId(i as u32), f, param_ok, cyclic)).collect()
}

/// A value of type `t` may hold references (conservative).
fn has_refs(p: &Program, t: TypeId) -> bool {
    !matches!(p.types.get(t), Type::Bool | Type::Int(_) | Type::Float32 | Type::Float64 | Type::Char | Type::Void)
}

fn returns_borrowed(p: &Program, fid: FuncId, f: &Function, param_ok: &dyn Fn(FuncId, usize) -> bool, cyclic: &dyn Fn(TypeId) -> bool) -> Option<Vec<bool>> {
    if f.is_async || matches!(f.kind, FuncKind::Lambda { .. }) || f.params.is_empty() {
        return None;
    }
    if !matches!(p.types.get(f.ret), Type::Pointer(_) | Type::String | Type::Slice(_) | Type::Struct(_)) || cyclic(f.ret) {
        return None;
    }
    for (k, l) in f.params.iter().enumerate() {
        let loc = &f.locals[l.0 as usize];
        if loc.captured || loc.address_taken || !param_ok(fid, k) {
            return None;
        }
    }
    let param_index = |l: LocalId| f.params.iter().position(|q| *q == l);
    // A parameter, or inline struct fields of one or of the dereference
    // of a pointer parameter: returns (parameter index, read through it).
    fn inline_path(e: &Expr, p: &Program, param_index: &dyn Fn(LocalId) -> Option<usize>) -> Option<(usize, bool)> {
        match &e.kind {
            ExprKind::Local(l) => param_index(*l).map(|k| (k, false)),
            ExprKind::Deref(x) => match &x.kind {
                ExprKind::Local(l) => param_index(*l).map(|k| (k, true)),
                _ => None,
            },
            ExprKind::Field { base, .. } if matches!(p.types.get(base.ty), Type::Struct(_)) => inline_path(base, p, param_index),
            _ => None,
        }
    }
    let mut deref = vec![false; f.params.len()];
    let mut ok = true;
    let mut returns = 0;
    walk_function_stmts(p, f, &mut |st| match &st.kind {
        StmtKind::Defer(_) => ok = false,
        StmtKind::Assign { target, .. } | StmtKind::CompoundAssign { target, .. } => {
            if !matches!(&target.kind, ExprKind::Local(l) if param_index(*l).is_none()) {
                ok = false;
            }
        }
        StmtKind::Return(ReturnValue::Value(e)) => {
            returns += 1;
            let mut note = |e: &Expr| match inline_path(e, p, &param_index) {
                Some((k, through)) => deref[k] |= through,
                None => ok = false,
            };
            match &e.kind {
                ExprKind::StructLit { fields, .. } => {
                    for (_, fe) in fields {
                        if has_refs(p, fe.ty) {
                            note(fe);
                        }
                    }
                }
                _ => note(e),
            }
        }
        _ => {}
    });
    (ok && returns > 0).then_some(deref)
}
