//! Ownership facts shared by the checker (`sema::ownership`) and code
//! generation: destructors, the position of every expression (does its
//! consumer store the value or only read through it?), and the per-parameter
//! escape analysis.
//!
//! # Rules (see DESIGN.md "Память")
//!
//! * A `*T` value always points at the payload of a whole refcounted heap
//!   object (`new`, `&x` of a variable, which then lives in a heap cell,
//!   `&T{...}`) and owns one reference to it.
//! * An *interior pointer* (`&s.f`, `&a[i]`, `&slice[i]`, `&global`) has no
//!   header of its own. It may only be read through (dereferenced, compared,
//!   field access) or passed to a parameter that does not escape. The caller
//!   keeps the owner alive for the call. Stored in a local variable
//!   (`var p = &xs[i]`, an *interior pointer variable*) it pins its owner
//!   ([`Pin`]) and is then subject to the same read-only rule.
//! * A parameter *escapes* when the callee stores it anywhere (variable,
//!   field, element, closure capture, interface, `weak`), returns it,
//!   reassigns it, takes its address, passes it to an escaping parameter or
//!   to an unknown callee, or when the function is `async` or a lambda.
//! * A parameter is *owned* (retained on entry, released at exit) when the
//!   function is `async` or reassigns it; a parameter of a type with a
//!   destructor is *moved* in. All other parameters are *borrowed*: the
//!   caller guarantees the value stays alive during the call.

use super::*;
use crate::types::Type;

/// How the consumer of an expression uses its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pos {
    /// Copied, stored or consumed: needs an owning value.
    Store,
    /// Only read through (dereferenced, compared, printed, field or index
    /// base, runtime-call argument).
    Read,
    /// Argument `i` of a direct call of a function.
    Arg(FuncId, usize),
    /// Operand of `&`: the place itself, not its value.
    AddrOf,
    /// Assignment target.
    Target,
}

/// The method `Drop` of struct `sid` (non-static), if declared.
pub fn drop_method(p: &Program, sid: u32) -> Option<FuncId> {
    let s = &p.structs[sid as usize];
    s.methods.iter().find(|(n, f)| n == "Drop" && matches!(p.funcs[f.0 as usize].kind, FuncKind::Method { is_static: false, .. })).map(|(_, f)| *f)
}

/// `return x` of a local variable `x` of function `f` holding a value with
/// a destructor moves it to the caller (spec decision 31): the local is not
/// dropped at exit. Only plain variables qualify (not parameters, captured
/// or address-taken locals). Returns the local.
pub fn moved_return(p: &Program, f: &Function, e: &Expr) -> Option<LocalId> {
    let ExprKind::Local(l) = e.kind else { return None };
    let loc = f.locals.get(l.0 as usize)?;
    (loc.kind == LocalKind::Var && !loc.captured && !loc.address_taken && has_dtor(p, loc.ty)).then_some(l)
}

/// True if a value of type `t` holds a struct with a destructor by value
/// (directly, in a field, array element, `T!` or enum payload).
pub fn has_dtor(p: &Program, t: TypeId) -> bool {
    match p.types.get(t) {
        Type::Struct(d) => drop_method(p, d.0).is_some() || p.structs[d.0 as usize].fields.iter().any(|f| has_dtor(p, f.ty)),
        Type::Array(e, n) => *n > 0 && has_dtor(p, *e),
        Type::Tuple(ts) => ts.iter().any(|t| has_dtor(p, *t)),
        Type::Fallible(t) => has_dtor(p, *t),
        Type::Enum(d) => p.enums[d.0 as usize].variants.iter().any(|v| v.fields.iter().any(|f| has_dtor(p, f.1))),
        _ => false,
    }
}

/// The local whose own storage the place `e` is part of: `x`, `x.f`,
/// `x.arr[i]` (fields of struct values and elements of arrays, not through
/// a pointer or a slice).
pub fn value_place_root(p: &Program, e: &Expr) -> Option<LocalId> {
    match &e.kind {
        ExprKind::Local(l) => Some(*l),
        ExprKind::Field { base, .. } if matches!(p.types.get(base.ty), Type::Struct(_) | Type::Tuple(_)) => value_place_root(p, base),
        ExprKind::Index { base, .. } if matches!(p.types.get(base.ty), Type::Array(..)) => value_place_root(p, base),
        _ => None,
    }
}

/// True for an `&x` whose result is an interior pointer (no header).
pub fn is_interior_addr(e: &Expr) -> bool {
    let ExprKind::AddrOf(inner) = &e.kind else { return false };
    match &inner.kind {
        ExprKind::Field { .. } | ExprKind::Index { .. } | ExprKind::Global(_) => true,
        ExprKind::Deref(y) => is_interior_addr(y),
        _ => false,
    }
}

/// What keeps the target of a pointer stored in an *interior pointer
/// variable* alive (DESIGN.md "Память", interior pointers in locals): the
/// variable holds a hidden strong reference to it, its *pin*.
#[derive(Clone, Copy, Debug)]
pub enum Pin<'e> {
    /// Nothing needed: static storage (a global), or a pointer parameter
    /// that does not escape (the caller keeps its target alive for the
    /// call, which the variable cannot outlive).
    None,
    /// The heap cell of this local or upvalue (`&s.f` of a variable `s`,
    /// which lives in a cell because its address is taken).
    Root(&'e Expr),
    /// A copy of this (side-effect-free) owner expression: the slice of
    /// `&slice[i]` (its backing array), the pointer of `&p.f`.
    Value(&'e Expr),
    /// The pins of this other interior pointer variable (`&q.f`, `q`).
    Interior(LocalId),
    /// The value stored itself, evaluated once and kept (`p = new T{}`, a
    /// call result, a plain pointer variable).
    Owned,
}

/// True for an owner expression that can be evaluated twice (once for the
/// pin, once for the address) with the same result: a path of variables,
/// fields, dereferences and indexes, without calls.
pub fn simple_path(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Local(_) | ExprKind::Upvalue(_) | ExprKind::Global(_) | ExprKind::Const(_) => true,
        ExprKind::Field { base, .. } => simple_path(base),
        ExprKind::Deref(x) => simple_path(x),
        ExprKind::Index { base, index } => simple_path(base) && simple_path(index),
        _ => false,
    }
}

/// The pin of `&place` (an interior place) stored in a variable of
/// function `fid`, or `None` if it cannot be kept (the owner is a
/// temporary or is computed by a call). `interior(l)` tells whether local
/// `l` is itself an interior pointer variable.
pub fn interior_pin<'e>(p: &Program, fid: FuncId, f: &Function, facts: &ParamFacts, place: &'e Expr, interior: &dyn Fn(LocalId) -> bool) -> Option<Pin<'e>> {
    match &place.kind {
        ExprKind::Field { base, .. } => interior_pin(p, fid, f, facts, base, interior),
        ExprKind::Index { base, .. } => match p.types.get(base.ty) {
            Type::Array(..) => interior_pin(p, fid, f, facts, base, interior),
            Type::Slice(_) if simple_path(base) => Some(Pin::Value(base)),
            _ => None,
        },
        ExprKind::Deref(ptr) => pointer_pin(fid, f, facts, ptr, interior).filter(|k| !matches!(k, Pin::Owned)).or_else(|| simple_path(ptr).then_some(Pin::Value(ptr))),
        ExprKind::Local(_) | ExprKind::Upvalue(_) => Some(Pin::Root(place)),
        ExprKind::Global(_) => Some(Pin::None),
        _ => None,
    }
}

/// The pin of a pointer value `e` (not an interior address) stored in an
/// interior pointer variable: another interior variable shares its pins,
/// a non-escaping pointer parameter needs none, anything else is kept
/// itself ([`Pin::Owned`]).
pub fn pointer_pin<'e>(fid: FuncId, f: &Function, facts: &ParamFacts, e: &'e Expr, interior: &dyn Fn(LocalId) -> bool) -> Option<Pin<'e>> {
    match &e.kind {
        ExprKind::Nil | ExprKind::Zero | ExprKind::AttrConst { .. } => Some(Pin::None),
        ExprKind::Local(l) if interior(*l) => Some(Pin::Interior(*l)),
        ExprKind::Local(l) => match f.params.iter().position(|x| x == l) {
            Some(k) if !facts.escapes(fid, k) && !f.locals[l.0 as usize].address_taken && !f.locals[l.0 as usize].captured => Some(Pin::None),
            _ => Some(Pin::Owned),
        },
        _ => Some(Pin::Owned),
    }
}

/// Calls `f(expr, position)` for every expression of `func`'s body, parents
/// before children.
pub fn walk_function(p: &Program, func: &Function, f: &mut dyn FnMut(&Expr, Pos)) {
    let mut w = Walker { p, f, s: None };
    w.block(&func.body);
}

/// Calls `f` for every statement of `func`'s body (also those nested in
/// blocks and in expressions such as `try ... catch`), outer first.
pub fn walk_function_stmts(p: &Program, func: &Function, f: &mut dyn FnMut(&Stmt)) {
    let mut w = Walker { p, f: &mut |_, _| {}, s: Some(f) };
    w.block(&func.body);
}

/// Calls `f` for every expression of block `b` (nested blocks included).
pub fn walk_block(p: &Program, b: &Block, f: &mut dyn FnMut(&Expr, Pos)) {
    let mut w = Walker { p, f, s: None };
    w.block(b);
}

/// Calls `f` for `e` (in position `pos`) and all its subexpressions.
pub fn walk_expr(p: &Program, e: &Expr, pos: Pos, f: &mut dyn FnMut(&Expr, Pos)) {
    let mut w = Walker { p, f, s: None };
    w.expr(e, pos);
}

struct Walker<'p, 'f> {
    p: &'p Program,
    f: &'f mut dyn FnMut(&Expr, Pos),
    s: Option<&'f mut dyn FnMut(&Stmt)>,
}

/// The interior pointer variables of a function and the statements that
/// store into them (DESIGN.md "Память"): a `var` initialised with an
/// interior address (`var p = &xs[i]`) is one; every `Let`/`Assign` of it
/// is a *site* whose value is pinned.
#[derive(Debug, Default)]
pub struct InteriorVars<'e> {
    /// Per local: is it an interior pointer variable.
    pub is: Vec<bool>,
    /// (local, stored value) in source order.
    pub sites: Vec<(LocalId, &'e Expr)>,
}

/// Collects [`InteriorVars`] of `func`.
pub fn interior_vars<'e>(p: &Program, func: &'e Function) -> InteriorVars<'e> {
    let mut is = vec![false; func.locals.len()];
    walk_function_stmts(p, func, &mut |s| {
        if let StmtKind::Let { local, init: Some(e) } = &s.kind {
            if is_interior_addr(e) && func.locals[local.0 as usize].kind == LocalKind::Var {
                is[local.0 as usize] = true;
            }
        }
    });
    let mut sites = Vec::new();
    // Re-walk with the same lifetime as `func` for the site references.
    fn collect<'e>(b: &'e Block, is: &[bool], out: &mut Vec<(LocalId, &'e Expr)>) {
        for s in &b.stmts {
            collect_stmt(s, is, out);
        }
    }
    fn collect_expr<'e>(e: &'e Expr, is: &[bool], out: &mut Vec<(LocalId, &'e Expr)>) {
        // Blocks inside expressions (try/catch handlers, try-else values).
        match &e.kind {
            ExprKind::TryCatch { value, handler, .. } => {
                collect_expr(value, is, out);
                collect(handler, is, out);
            }
            ExprKind::TryElse { value, .. } => collect_expr(value, is, out),
            _ => {}
        }
    }
    fn collect_stmt<'e>(s: &'e Stmt, is: &[bool], out: &mut Vec<(LocalId, &'e Expr)>) {
        match &s.kind {
            StmtKind::Let { local, init: Some(e) } => {
                if is[local.0 as usize] {
                    out.push((*local, e));
                }
                collect_expr(e, is, out);
            }
            StmtKind::Assign { target, value } => {
                if let ExprKind::Local(l) = &target.kind {
                    if is[l.0 as usize] {
                        out.push((*l, value));
                    }
                }
                collect_expr(value, is, out);
            }
            StmtKind::Expr(e) | StmtKind::Discard(e) => collect_expr(e, is, out),
            StmtKind::If { then, else_, .. } => {
                collect(then, is, out);
                if let Some(e) = else_ {
                    collect(e, is, out);
                }
            }
            StmtKind::Loop { body, .. } | StmtKind::ForRange { body, .. } | StmtKind::ForSlice { body, .. } | StmtKind::ForString { body, .. } | StmtKind::ForMap { body, .. } => collect(body, is, out),
            StmtKind::Switch(sw) => {
                for c in &sw.cases {
                    collect(&c.body, is, out);
                }
                if let Some(d) = &sw.default {
                    collect(d, is, out);
                }
            }
            StmtKind::Block(b) => collect(b, is, out),
            StmtKind::TryBlock { body, handler, .. } => {
                collect(body, is, out);
                collect(handler, is, out);
            }
            _ => {}
        }
    }
    collect(&func.body, &is, &mut sites);
    InteriorVars { is, sites }
}

impl Walker<'_, '_> {
    fn block(&mut self, b: &Block) {
        for s in &b.stmts {
            self.stmt(s);
        }
    }

    fn rv(&mut self, rv: &ReturnValue) {
        match rv {
            ReturnValue::Void => {}
            ReturnValue::Value(e) | ReturnValue::Error(e) => self.expr(e, Pos::Store),
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        use Pos::*;
        if let Some(h) = self.s.as_mut() {
            h(s);
        }
        match &s.kind {
            StmtKind::Let { init, .. } => {
                if let Some(e) = init {
                    self.expr(e, Store)
                }
            }
            StmtKind::LetTuple { init, .. } => self.expr(init, Store),
            StmtKind::Assign { target, value } => {
                self.expr(target, Target);
                self.expr(value, Store);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                self.expr(target, Target);
                self.expr(value, Read);
            }
            StmtKind::Expr(e) | StmtKind::Discard(e) => self.expr(e, Read),
            StmtKind::If { cond, then, else_ } => {
                self.expr(cond, Read);
                self.block(then);
                if let Some(e) = else_ {
                    self.block(e);
                }
            }
            StmtKind::Loop { cond, body, .. } => {
                if let Some(c) = cond {
                    self.expr(c, Read);
                }
                self.block(body);
            }
            StmtKind::ForRange { start, end, body, .. } => {
                self.expr(start, Read);
                self.expr(end, Read);
                self.block(body);
            }
            StmtKind::ForSlice { seq, body, .. } => {
                self.expr(seq, Store);
                self.block(body);
            }
            StmtKind::ForString { string, body, .. } => {
                self.expr(string, Read);
                self.block(body);
            }
            StmtKind::ForMap { map, body, .. } => {
                self.expr(map, Read);
                self.block(body);
            }
            StmtKind::Switch(sw) => {
                if let Some((_, e)) = &sw.subject {
                    self.expr(e, Store);
                }
                for c in &sw.cases {
                    for p in &c.patterns {
                        if let Pattern::Cond(e) = p {
                            self.expr(e, Read);
                        }
                    }
                    self.block(&c.body);
                }
                if let Some(d) = &sw.default {
                    self.block(d);
                }
            }
            StmtKind::Return(rv) => self.rv(rv),
            StmtKind::Break(_) | StmtKind::Continue(_) => {}
            StmtKind::Defer(e) => self.expr(e, Store),
            StmtKind::Block(b) => self.block(b),
            StmtKind::TryBlock { body, handler, .. } => {
                self.block(body);
                self.block(handler);
            }
        }
    }

    fn expr(&mut self, e: &Expr, pos: Pos) {
        use Pos::*;
        (self.f)(e, pos);
        match &e.kind {
            ExprKind::Const(_) | ExprKind::Nil | ExprKind::Zero | ExprKind::AttrConst { .. } | ExprKind::Local(_) | ExprKind::Upvalue(_) | ExprKind::Global(_) | ExprKind::FuncRef(_) | ExprKind::Closure { .. } => {}
            ExprKind::Field { base, .. } => self.expr(base, Read),
            ExprKind::Deref(x) => self.expr(x, Read),
            ExprKind::AddrOf(x) => match &x.kind {
                // `&*p` is `p` itself.
                ExprKind::Deref(y) => self.expr(y, pos),
                _ => self.expr(x, AddrOf),
            },
            ExprKind::New(x) => self.expr(x, Store),
            ExprKind::Index { base, index } => {
                self.expr(base, Read);
                self.expr(index, Read);
            }
            ExprKind::MapGet { map, key } => {
                self.expr(map, Read);
                self.expr(key, Read);
            }
            ExprKind::SubSlice { base, lo, hi } => {
                // An array is sub-sliced in place: the slice points into it.
                let arr = matches!(self.p.types.get(base.ty), Type::Array(..));
                self.expr(base, if arr { AddrOf } else { Read });
                for x in [lo, hi].into_iter().flatten() {
                    self.expr(x, Read);
                }
            }
            ExprKind::Unary { operand, .. } => self.expr(operand, Read),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, Read);
                self.expr(rhs, Read);
            }
            ExprKind::IsNil { value, .. } => self.expr(value, Read),
            ExprKind::Convert { kind, value } => self.expr(value, if *kind == ConvKind::PtrToWeak { Store } else { Read }),
            ExprKind::Call { callee, args } => match callee {
                Callee::Func(fid) => {
                    for (i, a) in args.iter().enumerate() {
                        self.expr(a, Arg(*fid, i));
                    }
                }
                Callee::Extern(_) => {
                    for a in args {
                        self.expr(a, Read);
                    }
                }
                Callee::Interface { recv, .. } => {
                    self.expr(recv, Read);
                    for a in args {
                        self.expr(a, Store);
                    }
                }
                Callee::Value(c) => {
                    self.expr(c, Read);
                    for a in args {
                        self.expr(a, Store);
                    }
                }
            },
            ExprKind::StructLit { fields, .. } => {
                for (_, x) in fields {
                    self.expr(x, Store);
                }
            }
            ExprKind::SliceLit(xs) | ExprKind::Tuple(xs) | ExprKind::EnumLit { args: xs, .. } => {
                for x in xs {
                    self.expr(x, Store);
                }
            }
            ExprKind::MapLit(kvs) => {
                for (k, v) in kvs {
                    self.expr(k, Store);
                    self.expr(v, Store);
                }
            }
            ExprKind::ToInterface { value } => self.expr(value, Store),
            ExprKind::Propagate { value, context } => {
                self.expr(value, Store);
                if let Some(c) = context {
                    self.expr(c, Read);
                }
            }
            ExprKind::TryCatch { value, handler, .. } => {
                self.expr(value, Store);
                self.block(handler);
            }
            ExprKind::TryElse { value, fallback } => {
                self.expr(value, Store);
                match &**fallback {
                    TryFallback::Value(x) => self.expr(x, Store),
                    TryFallback::Return(rv) => self.rv(rv),
                    TryFallback::Break(_) | TryFallback::Continue(_) => {}
                }
            }
            ExprKind::Await(t) => self.expr(t, Read),
            ExprKind::TaskAll(ts) => {
                for t in ts {
                    self.expr(t, Read);
                }
            }
            ExprKind::Builtin { op, args } => {
                for (i, a) in args.iter().enumerate() {
                    let p = match op {
                        Builtin::Append => Store,
                        Builtin::LenArray if i == 0 => AddrOf,
                        _ => Read,
                    };
                    self.expr(a, p);
                }
            }
            ExprKind::ToString(v) => self.expr(v, Read),
            ExprKind::Print(items) => {
                for it in items {
                    if let Some(a) = &it.arg {
                        self.expr(a, Read);
                    }
                }
            }
        }
    }
}

/// Per-parameter ownership facts of every function.
#[derive(Debug, Clone, Default)]
pub struct ParamFacts {
    /// `escapes[f][k]`: parameter `k` of function `f` escapes.
    pub escapes: Vec<Vec<bool>>,
    /// `owned[f][k]`: the callee retains parameter `k` on entry.
    pub owned: Vec<Vec<bool>>,
    /// `moved[f][k]`: parameter `k` has a destructor and is moved in.
    pub moved: Vec<Vec<bool>>,
}

impl ParamFacts {
    pub fn escapes(&self, f: FuncId, k: usize) -> bool {
        self.escapes.get(f.0 as usize).and_then(|v| v.get(k)).copied().unwrap_or(true)
    }
    pub fn owned(&self, f: FuncId, k: usize) -> bool {
        self.owned.get(f.0 as usize).and_then(|v| v.get(k)).copied().unwrap_or(true)
    }
    pub fn moved(&self, f: FuncId, k: usize) -> bool {
        self.moved.get(f.0 as usize).and_then(|v| v.get(k)).copied().unwrap_or(false)
    }
}

/// Computes [`ParamFacts`] for the whole program (greatest fixpoint: a
/// parameter only passed on to non-escaping parameters does not escape,
/// also through recursion).
pub fn param_facts(p: &Program) -> ParamFacts {
    let n = p.funcs.len();
    let mut facts = ParamFacts { escapes: Vec::with_capacity(n), owned: Vec::with_capacity(n), moved: Vec::with_capacity(n) };
    // Local index -> parameter index, per function.
    let mut param_of: Vec<Vec<Option<usize>>> = Vec::with_capacity(n);
    for f in &p.funcs {
        let mut m = vec![None; f.locals.len()];
        for (k, l) in f.params.iter().enumerate() {
            m[l.0 as usize] = Some(k);
        }
        param_of.push(m);
        let base = f.is_async || matches!(f.kind, FuncKind::Lambda { .. });
        let esc: Vec<bool> = f.params.iter().map(|l| {
            let loc = &f.locals[l.0 as usize];
            base || loc.captured || loc.address_taken
        }).collect();
        facts.escapes.push(esc);
        facts.moved.push(f.params.iter().map(|l| has_dtor(p, f.locals[l.0 as usize].ty)).collect());
    }
    // Reassigned parameters (owned) and local escapes.
    for (fi, f) in p.funcs.iter().enumerate() {
        let mut owned: Vec<bool> = f.params.iter().map(|_| f.is_async).collect();
        let po = &param_of[fi];
        walk_function(p, f, &mut |e, pos| {
            // `n = v`, and also `n.f = v` / `n.arr[i] = v` on a by-value
            // parameter: the callee writes its own copy, so it must own it
            // (writing into a borrowed copy released the caller's field
            // value and leaked the new one).
            if pos == Pos::Target {
                if let Some(l) = value_place_root(p, e) {
                    if let Some(k) = po[l.0 as usize] {
                        owned[k] = true;
                    }
                }
            }
        });
        for (k, o) in owned.iter().enumerate() {
            if *o {
                facts.escapes[fi][k] = true;
            }
        }
        facts.owned.push(owned);
    }
    loop {
        let mut changed = false;
        for (fi, f) in p.funcs.iter().enumerate() {
            let po = &param_of[fi];
            let mut marks = Vec::new();
            {
                let esc = &facts.escapes;
                walk_function(p, f, &mut |e, pos| {
                    if let ExprKind::Local(l) = &e.kind {
                        if let Some(k) = po[l.0 as usize] {
                            let escapes = match pos {
                                Pos::Store | Pos::Target => true,
                                Pos::Arg(g, i) => esc.get(g.0 as usize).and_then(|v| v.get(i)).copied().unwrap_or(true),
                                Pos::Read | Pos::AddrOf => false,
                            };
                            if escapes {
                                marks.push(k);
                            }
                        }
                    }
                });
            }
            for k in marks {
                if !facts.escapes[fi][k] {
                    facts.escapes[fi][k] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    facts
}

/// Where a local lives (escape analysis, DESIGN.md "Память"): instead of a
/// heap object, a local can sit on the stack in an alloca that carries an
/// immortal object header (so a `*T` to it still has a header 16 bytes
/// before the payload, and a stray retain/release is a no-op).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackKind {
    /// The default: a plain slot, or a heap cell if captured / address-taken.
    Normal,
    /// An address-taken local (`&x`) whose address never escapes: a stack
    /// slot instead of a heap cell.
    Cell,
    /// `var p = new T{...}` where `p` never escapes: the object is an
    /// alloca, and leaving `p`'s scope releases the fields instead of the
    /// object.
    New,
}

/// Parameter escape facts that also count arguments of runtime (extern)
/// calls as escaping: the runtime may retain and keep them (channels,
/// tasks), which is harmless for heap objects but not for a stack object.
pub fn strict_param_facts(p: &Program) -> ParamFacts {
    let mut facts = param_facts(p);
    let n = p.funcs.len();
    let mut param_of: Vec<Vec<Option<usize>>> = Vec::with_capacity(n);
    let mut kept: Vec<std::collections::HashSet<*const Expr>> = Vec::with_capacity(n);
    for f in &p.funcs {
        let mut m = vec![None; f.locals.len()];
        for (k, l) in f.params.iter().enumerate() {
            m[l.0 as usize] = Some(k);
        }
        param_of.push(m);
        kept.push(kept_reads(p, f));
    }
    loop {
        let mut changed = false;
        for (fi, f) in p.funcs.iter().enumerate() {
            let po = &param_of[fi];
            let mut marks = Vec::new();
            {
                let esc = &facts.escapes;
                let kp = &kept[fi];
                walk_function(p, f, &mut |e, pos| {
                    if let ExprKind::Local(l) = &e.kind {
                        if let Some(k) = po[l.0 as usize] {
                            let escapes = match pos {
                                Pos::Store | Pos::Target => true,
                                Pos::Arg(g, i) => esc.get(g.0 as usize).and_then(|v| v.get(i)).copied().unwrap_or(true),
                                Pos::Read => kp.contains(&(e as *const Expr)),
                                Pos::AddrOf => false,
                            };
                            if escapes {
                                marks.push(k);
                            }
                        }
                    }
                });
            }
            for k in marks {
                if !facts.escapes[fi][k] {
                    facts.escapes[fi][k] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    facts
}

/// Expressions of `f` in a `Read` position that the consumer may still
/// keep: arguments of runtime (extern) calls and operands of conversions.
fn kept_reads(p: &Program, f: &Function) -> std::collections::HashSet<*const Expr> {
    let mut out = std::collections::HashSet::new();
    walk_function(p, f, &mut |e, _| match &e.kind {
        ExprKind::Call { callee: Callee::Extern(_), args } => {
            for a in args {
                out.insert(a as *const Expr);
            }
        }
        ExprKind::Convert { value, .. } => {
            out.insert(&**value as *const Expr);
        }
        _ => {}
    });
    out
}

/// The local reached from place `e` through fields, array elements and
/// dereferences (`p.f`, `(*p).a[i]`, `x.f`): the owner of an address taken
/// of `e`.
fn place_owner(e: &Expr) -> Option<LocalId> {
    match &e.kind {
        ExprKind::Local(l) => Some(*l),
        ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => place_owner(base),
        ExprKind::Deref(x) => place_owner(x),
        _ => None,
    }
}

/// [`StackKind`] of every local of `f` (`facts` from
/// [`strict_param_facts`]). A local qualifies when every use of its address
/// (or, for `new`, of the pointer) is only read through or passed to a
/// parameter that does not escape; it is never captured, stored, returned,
/// reassigned, converted, passed to the runtime, used in a `defer` or
/// sub-sliced, and no interior pointer into it is kept. `async` functions
/// keep everything on the heap.
pub fn stack_locals(p: &Program, f: &Function, facts: &ParamFacts) -> Vec<StackKind> {
    let n = f.locals.len();
    let mut kind = vec![StackKind::Normal; n];
    // ZELL_NO_ESCAPE=1 turns the analysis off (for measurements).
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if f.is_async || *OFF.get_or_init(|| std::env::var_os("ZELL_NO_ESCAPE").is_some()) {
        return kind;
    }
    for (i, l) in f.locals.iter().enumerate() {
        if l.captured {
            continue;
        }
        if l.address_taken && !matches!(l.kind, LocalKind::This | LocalKind::Temp) {
            kind[i] = StackKind::Cell;
        }
    }
    // `var p = new T{...}`: exactly one Let, no other assignment.
    let mut lets = vec![0u32; n];
    walk_function_stmts(p, f, &mut |s| match &s.kind {
        StmtKind::Let { local, init } => {
            let li = local.0 as usize;
            lets[li] += 1;
            let l = &f.locals[li];
            let is_new = matches!(init, Some(Expr { kind: ExprKind::New(_), .. }));
            if is_new && lets[li] == 1 && l.kind == LocalKind::Var && !l.captured && !l.address_taken && matches!(p.types.get(l.ty), Type::Pointer(_)) {
                kind[li] = StackKind::New;
            } else if kind[li] == StackKind::New {
                kind[li] = StackKind::Normal;
            }
        }
        StmtKind::LetTuple { locals, .. } => {
            for l in locals.iter().flatten() {
                if kind[l.0 as usize] == StackKind::New {
                    kind[l.0 as usize] = StackKind::Normal;
                }
            }
        }
        _ => {}
    });
    if kind.iter().all(|k| *k == StackKind::Normal) {
        return kind;
    }
    let kept = kept_reads(p, f);
    // Everything under a `defer` runs at function exit, after the scope.
    let mut deferred = std::collections::HashSet::new();
    walk_function_stmts(p, f, &mut |s| {
        if let StmtKind::Defer(e) = &s.kind {
            walk_expr(p, e, Pos::Store, &mut |x, _| {
                deferred.insert(x as *const Expr);
            });
        }
    });
    let ok = |e: &Expr, pos: Pos| -> bool {
        if deferred.contains(&(e as *const Expr)) {
            return false;
        }
        match pos {
            Pos::Read => !kept.contains(&(e as *const Expr)),
            Pos::Arg(g, i) => !facts.escapes(g, i),
            _ => false,
        }
    };
    let mut reject = vec![false; n];
    walk_function(p, f, &mut |e, pos| match &e.kind {
        ExprKind::AddrOf(inner) => {
            if let Some(o) = place_owner(inner) {
                let o = o.0 as usize;
                // `&x...` of a stack cell, or `&p.f...` into a stack object.
                let direct = value_place_root(p, inner).map(|r| r.0 as usize) == Some(o);
                if (kind[o] == StackKind::Cell && direct) || (kind[o] == StackKind::New && !direct) {
                    if !ok(e, pos) {
                        reject[o] = true;
                    }
                }
            }
        }
        ExprKind::SubSlice { base, .. } => {
            if let Some(o) = place_owner(base) {
                if kind[o.0 as usize] != StackKind::Normal {
                    reject[o.0 as usize] = true;
                }
            }
        }
        ExprKind::Local(l) => {
            let li = l.0 as usize;
            match kind[li] {
                StackKind::New => {
                    if !ok(e, pos) {
                        reject[li] = true;
                    }
                }
                StackKind::Cell => {
                    if deferred.contains(&(e as *const Expr)) {
                        reject[li] = true;
                    }
                }
                StackKind::Normal => {}
            }
        }
        _ => {}
    });
    for i in 0..n {
        if reject[i] {
            kind[i] = StackKind::Normal;
        }
    }
    kind
}
