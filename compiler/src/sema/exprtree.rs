//! Expression trees (DESIGN.md "Деревья выражений"): a lambda checked
//! where `expr<F>` is expected becomes an `expr.Expr<F>` value
//! (`stdlib_src/expr.zl`) instead of a closure.
//!
//! The lambda is first checked as an ordinary lambda of type `F` (lifted
//! into a function, parameter types inferred; in a generic call the type
//! parameters of `F`'s result are inferred from the body). Its typed body
//! is then validated against the node set ([`TREE_CALLS`] and functions
//! marked `@expr.call` are the calls allowed) and encoded in prefix order
//! into the constant `expr.Site` of the lambda site (an
//! [`ExprKind::AttrConst`], immortal, in read-only data). Every maximal
//! subexpression that does not mention a parameter is a *slot*: a literal
//! stays a constant slot; anything else (a captured variable, a field path
//! of one, a global, a call of them) is a *captured* slot, evaluated once
//! when the tree is built. The body of the lifted lambda is rewritten to
//! read the captured slots from its environment, so `Compile()` sees the
//! same values as the tree ("values, not variables").
//!
//! The environment holds the captured values themselves (a tuple in one
//! heap box, `hir::Function::value_env`): building a tree allocates that
//! box only, nothing when nothing is captured. Two synthesized lambdas over
//! the same environment give the library the slot values (`k -> any`) and
//! the static types of the nodes (`k -> *reflect.TypeInfo`); all three
//! closures are made by one [`ExprKind::EnvClosures`].
//!
//! A lambda passed as an argument of an allowed call inside the tree
//! (`data.Any(u.Orders, o => o.Total > min)`) is a `Lambda` node: its
//! parameters continue the numbering of the outer ones, and its lifted
//! function gets a value environment of its own (the slots and outer
//! parameters it reads), built by the enclosing lambda when it runs.

use super::coerce::Ctx;
use super::*;
use crate::hir::{BinOp, Builtin, Callee, CaptureSource, ConstValue, ExprKind, ReturnValue, StmtKind, UnOp};

/// The functions an expression tree may call, by full name (`len` is the
/// builtin), besides the functions and methods marked `@expr.call`. One
/// table so that it can grow; the `expr` printer and the translators of
/// libraries see the same names.
pub(crate) const TREE_CALLS: &[&str] =
    &["strings.HasPrefix", "strings.HasSuffix", "strings.Contains", "strings.ToLower", "strings.ToUpper", "strings.Trim", "len", "slices.Contains", "math.Abs"];

// Node tags of the site encoding (`expr.zl`, `decoder.node`).
const T_PARAM: i128 = 0;
const T_FIELD: i128 = 1;
const T_CONST: i128 = 2;
const T_CAPTURED: i128 = 3;
const T_BINARY: i128 = 4;
const T_UNARY: i128 = 5;
const T_CALL: i128 = 6;
const T_ISNIL: i128 = 7;
const T_TUPLE: i128 = 8;
const T_STRUCT: i128 = 9;
const T_CONVERT: i128 = 10;
const T_INDEX: i128 = 11;
const T_LAMBDA: i128 = 12;

/// Upvalue markers left in the bodies of nested lambdas until their
/// environments are built: a captured slot (`MARK_SLOT + c`) or a
/// parameter of an enclosing lambda (`MARK_PARAM + p`, global index).
const MARK_SLOT: u32 = 1 << 30;
const MARK_PARAM: u32 = 1 << 29;

/// One value of a lambda site.
struct Slot {
    /// Captured: the expression evaluated in the creating function; a
    /// constant: the literal itself.
    value: hir::Expr,
    /// Index in the environment (captured slots only).
    cap: Option<u32>,
    /// Source text of a captured slot (`n`, `req.MinAge`); a second
    /// occurrence of the same text reuses the slot.
    name: String,
}

/// A lambda of the tree: the site's own (frame 0) or a nested one.
struct Frame {
    fid: FuncId,
    nparams: u32,
    /// Global index of its first parameter.
    base: u32,
    parent: usize,
    /// Its captures in the parent frame's terms, and their names (nested
    /// frames only).
    caps: Vec<CaptureSource>,
    names: Vec<String>,
}

/// What an upvalue of a frame stands for.
enum Res {
    /// A parameter of an enclosing lambda (global index).
    Param(u32),
    /// Capture `i` of the site's lambda: a variable of the creating code.
    Outer(u32),
}

/// Encoding state of one lambda site.
#[derive(Default)]
struct Tb {
    ops: Vec<i128>,
    strs: Vec<String>,
    types: Vec<TypeId>,
    slots: Vec<Slot>,
    /// Types of the captured slots, in environment order.
    caps: Vec<TypeId>,
    /// Capture `i` of the checked lambda: where the creating function
    /// finds it, and its name.
    outer: Vec<(CaptureSource, TypeId, String)>,
    frames: Vec<Frame>,
    /// Type of every parameter, by global index.
    ptypes: Vec<TypeId>,
    /// Locals of the creating function captured (by reference) by a
    /// closure inside a slot value: they stay captured.
    keep: Vec<LocalId>,
    errors: Vec<(String, Span)>,
}

impl Tb {
    fn ty(&mut self, t: TypeId) -> i128 {
        match self.types.iter().position(|x| *x == t) {
            Some(i) => i as i128,
            None => {
                self.types.push(t);
                (self.types.len() - 1) as i128
            }
        }
    }

    fn str(&mut self, s: &str) -> i128 {
        match self.strs.iter().position(|x| x == s) {
            Some(i) => i as i128,
            None => {
                self.strs.push(s.to_string());
                (self.strs.len() - 1) as i128
            }
        }
    }

    fn bad(&mut self, what: impl Into<String>, span: Span) {
        self.errors.push((format!("{} is not allowed in an expression tree", what.into()), span));
    }

    /// Upvalue `j` of frame `f`; `None` for a local of an enclosing lambda
    /// that is not a parameter.
    fn resolve_up(&self, f: usize, j: u32) -> Option<Res> {
        if f == 0 {
            return Some(Res::Outer(j));
        }
        let fr = &self.frames[f];
        match *fr.caps.get(j as usize)? {
            CaptureSource::Local(l) => {
                let p = &self.frames[fr.parent];
                (l.0 < p.nparams).then_some(Res::Param(p.base + l.0))
            }
            CaptureSource::Upvalue(u) => self.resolve_up(fr.parent, u),
        }
    }

    fn up_is_param(&self, f: usize, j: u32) -> bool {
        !matches!(self.resolve_up(f, j), Some(Res::Outer(_)))
    }

    /// `e` (in frame `f`) reads a parameter of the tree (or a local that
    /// is not one, which is reported when it is encoded).
    fn mentions_param(&self, f: usize, e: &hir::Expr) -> bool {
        match &e.kind {
            ExprKind::Local(_) => true,
            ExprKind::Upvalue(j) => self.up_is_param(f, *j),
            ExprKind::Closure { captures, .. } => captures.iter().any(|c| match c {
                CaptureSource::Local(_) => true,
                CaptureSource::Upvalue(u) => self.up_is_param(f, *u),
            }),
            _ => children(e).into_iter().any(|c| self.mentions_param(f, c)),
        }
    }

    /// The source of capture `i` of the site's lambda in the creating
    /// function.
    fn creator(&self, f: usize, j: u32) -> Option<CaptureSource> {
        match self.resolve_up(f, j)? {
            Res::Outer(i) => self.outer.get(i as usize).map(|o| o.0),
            Res::Param(_) => None,
        }
    }

    /// Rewrites a parameter-free expression of frame `f` into an
    /// expression of the creating function.
    fn to_creator(&mut self, f: usize, e: &mut hir::Expr) {
        match &mut e.kind {
            ExprKind::Upvalue(j) => {
                if let Some(src) = self.creator(f, *j) {
                    e.kind = match src {
                        CaptureSource::Local(l) => ExprKind::Local(l),
                        CaptureSource::Upvalue(u) => ExprKind::Upvalue(u),
                    };
                }
                return;
            }
            ExprKind::Closure { captures, .. } => {
                let mut cs = std::mem::take(captures);
                for c in cs.iter_mut() {
                    if let CaptureSource::Upvalue(u) = *c {
                        if let Some(src) = self.creator(f, u) {
                            *c = src;
                        }
                    }
                    if let CaptureSource::Local(l) = *c {
                        self.keep.push(l);
                    }
                }
                if let ExprKind::Closure { captures, .. } = &mut e.kind {
                    *captures = cs;
                }
                return;
            }
            _ => {}
        }
        for c in children_mut(e) {
            self.to_creator(f, c);
        }
    }
}

/// Ordinal of an operator in `expr.Op`.
fn op_code(op: BinOp) -> Option<i128> {
    Some(match op {
        BinOp::Add | BinOp::StrConcat => 0,
        BinOp::Sub => 1,
        BinOp::Mul => 2,
        BinOp::Div => 3,
        BinOp::Rem => 4,
        BinOp::Eq | BinOp::StrEq => 5,
        BinOp::Ne | BinOp::StrNe => 6,
        BinOp::Lt | BinOp::StrLt => 7,
        BinOp::Le | BinOp::StrLe => 8,
        BinOp::Gt | BinOp::StrGt => 9,
        BinOp::Ge | BinOp::StrGe => 10,
        BinOp::And => 11,
        BinOp::Or => 12,
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => return None,
    })
}

fn bit_op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        _ => ">>",
    }
}

/// Every subexpression of `e` (not the bodies of closures, nor statement
/// blocks).
fn children(e: &hir::Expr) -> Vec<&hir::Expr> {
    match &e.kind {
        ExprKind::Field { base: x, .. }
        | ExprKind::Deref(x)
        | ExprKind::AddrOf(x)
        | ExprKind::New(x)
        | ExprKind::Unary { operand: x, .. }
        | ExprKind::IsNil { value: x, .. }
        | ExprKind::Convert { value: x, .. }
        | ExprKind::ToInterface { value: x }
        | ExprKind::Await(x)
        | ExprKind::ToString(x)
        | ExprKind::TryCatch { value: x, .. } => vec![x],
        ExprKind::Index { base, index } => vec![base, index],
        ExprKind::MapGet { map, key } => vec![map, key],
        ExprKind::SubSlice { base, lo, hi } => std::iter::once(&**base).chain(lo.as_deref()).chain(hi.as_deref()).collect(),
        ExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        ExprKind::Call { callee, args } => {
            let mut v: Vec<&hir::Expr> = match callee {
                Callee::Interface { recv, .. } => vec![recv],
                Callee::Value(f) => vec![f],
                _ => Vec::new(),
            };
            v.extend(args.iter());
            v
        }
        ExprKind::Builtin { args, .. } | ExprKind::Tuple(args) | ExprKind::EnumLit { args, .. } | ExprKind::SliceLit(args) | ExprKind::TaskAll(args) => args.iter().collect(),
        ExprKind::StructLit { fields, .. } => fields.iter().map(|(_, x)| x).collect(),
        ExprKind::MapLit(kvs) => kvs.iter().flat_map(|(k, v)| [k, v]).collect(),
        ExprKind::EnvClosures { env, .. } => env.as_deref().into_iter().collect(),
        ExprKind::Propagate { value, context } => std::iter::once(&**value).chain(context.as_deref()).collect(),
        ExprKind::TryElse { value, fallback } => match &**fallback {
            hir::TryFallback::Value(v) => vec![value, v],
            _ => vec![value],
        },
        _ => Vec::new(),
    }
}

fn children_mut(e: &mut hir::Expr) -> Vec<&mut hir::Expr> {
    match &mut e.kind {
        ExprKind::Field { base: x, .. }
        | ExprKind::Deref(x)
        | ExprKind::AddrOf(x)
        | ExprKind::New(x)
        | ExprKind::Unary { operand: x, .. }
        | ExprKind::IsNil { value: x, .. }
        | ExprKind::Convert { value: x, .. }
        | ExprKind::ToInterface { value: x }
        | ExprKind::Await(x)
        | ExprKind::ToString(x)
        | ExprKind::TryCatch { value: x, .. } => vec![x],
        ExprKind::Index { base, index } => vec![base, index],
        ExprKind::MapGet { map, key } => vec![map, key],
        ExprKind::SubSlice { base, lo, hi } => std::iter::once(&mut **base).chain(lo.as_deref_mut()).chain(hi.as_deref_mut()).collect(),
        ExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        ExprKind::Call { callee, args } => {
            let mut v: Vec<&mut hir::Expr> = match callee {
                Callee::Interface { recv, .. } => vec![&mut **recv],
                Callee::Value(f) => vec![&mut **f],
                _ => Vec::new(),
            };
            v.extend(args.iter_mut());
            v
        }
        ExprKind::Builtin { args, .. } | ExprKind::Tuple(args) | ExprKind::EnumLit { args, .. } | ExprKind::SliceLit(args) | ExprKind::TaskAll(args) => args.iter_mut().collect(),
        ExprKind::StructLit { fields, .. } => fields.iter_mut().map(|(_, x)| x).collect(),
        ExprKind::MapLit(kvs) => kvs.iter_mut().flat_map(|(k, v)| [k, v]).collect(),
        ExprKind::EnvClosures { env, .. } => env.as_deref_mut().into_iter().collect(),
        ExprKind::Propagate { value, context } => std::iter::once(&mut **value).chain(context.as_deref_mut()).collect(),
        ExprKind::TryElse { value, fallback } => match &mut **fallback {
            hir::TryFallback::Value(v) => vec![&mut **value, v],
            _ => vec![&mut **value],
        },
        _ => Vec::new(),
    }
}

/// A literal: stays a constant of the tree.
fn is_literal(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Const(_) | ExprKind::Nil | ExprKind::Zero | ExprKind::AttrConst { .. } => true,
        ExprKind::EnumLit { args, .. } => args.is_empty(),
        _ => false,
    }
}

/// The receiver of a method call without the `&` the call added.
fn recv_mut(e: &mut hir::Expr) -> &mut hir::Expr {
    if matches!(e.kind, ExprKind::AddrOf(_)) {
        let ExprKind::AddrOf(x) = &mut e.kind else { unreachable!() };
        x
    } else {
        e
    }
}

/// `Foo<int, string>.Bar` -> `Foo.Bar`: a full name without type arguments.
fn strip_type_args(name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Visits every upvalue marker in `e` (including environments of closures
/// built for nested lambdas).
fn marks_mut(e: &mut hir::Expr, f: &mut dyn FnMut(&mut hir::Expr)) {
    if let ExprKind::Upvalue(u) = e.kind {
        if u >= MARK_PARAM {
            f(e);
        }
        return;
    }
    for c in children_mut(e) {
        marks_mut(c, f);
    }
}

fn block_marks_mut(b: &mut hir::Block, f: &mut dyn FnMut(&mut hir::Expr)) {
    for s in &mut b.stmts {
        match &mut s.kind {
            StmtKind::Return(ReturnValue::Value(v)) | StmtKind::Expr(v) => marks_mut(v, f),
            _ => {}
        }
    }
}

/// Replaces the closure of `fid` in `e` by `with` (once).
fn replace_closure(e: &mut hir::Expr, fid: FuncId, with: &mut Option<hir::Expr>) {
    if with.is_none() {
        return;
    }
    if matches!(e.kind, ExprKind::Closure { func, .. } if func == fid) {
        *e = with.take().unwrap();
        return;
    }
    for c in children_mut(e) {
        replace_closure(c, fid, with);
    }
}

/// The forms rejected before the lambda is checked: `(message, span)`.
/// `in_call`: `e` is an argument of a call (where a nested lambda may be).
fn forbidden_syntax(e: &ast::Expr, in_call: bool, out: &mut Vec<(String, Span)>) {
    use ast::ExprKind as K;
    let bad = match &e.kind {
        K::Await(_) => Some("await is not allowed in an expression tree"),
        K::Lambda { .. } if !in_call => Some("a nested lambda is only allowed as an argument of a call in an expression tree"),
        K::Propagate { .. } => Some("? is not allowed in an expression tree"),
        K::Try { .. } => Some("try is not allowed in an expression tree"),
        _ => None,
    };
    if let Some(m) = bad {
        out.push((m.to_string(), e.span));
        return;
    }
    let mut go = |x: &ast::Expr| forbidden_syntax(x, false, out);
    match &e.kind {
        K::Lambda { body, is_async, .. } => {
            if *is_async {
                out.push(("an async lambda is not allowed in an expression tree".to_string(), e.span));
                return;
            }
            match &**body {
                ast::LambdaBody::Expr(x) => go(x),
                ast::LambdaBody::Block(b) => out.push(("a nested lambda of an expression tree must be a single expression".to_string(), b.span)),
            }
        }
        K::Field { base, .. } | K::TypeArgs { base, .. } => go(base),
        K::Call { callee, args } => {
            forbidden_syntax(callee, false, out);
            for a in args {
                forbidden_syntax(expr::strip_parens(a), true, out);
            }
        }
        K::Index { base, index } => {
            go(base);
            go(index);
        }
        K::Unary { expr, .. } | K::New(expr) | K::Paren(expr) | K::Spread(expr) | K::NamedArg { value: expr, .. } => go(expr),
        K::Binary { left, right, .. } => {
            go(left);
            go(right);
        }
        K::StructLit { fields, .. } => fields.iter().for_each(|f| go(&f.value)),
        K::CompositeLit { elems, .. } => elems.iter().for_each(|(k, v)| {
            if let Some(k) = k {
                go(k);
            }
            go(v);
        }),
        K::SliceLit(xs) | K::Tuple(xs) => xs.iter().for_each(&mut go),
        K::MapLit { entries, .. } => entries.iter().for_each(|(k, v)| {
            go(k);
            go(v);
        }),
        K::Range(a, b) => [a, b].into_iter().flatten().for_each(|x| go(x)),
        _ => {}
    }
}

impl<'a> Sema<'a> {
    /// The `expr` namespace and its template `name`.
    fn expr_template(&self, name: &str) -> Option<u32> {
        let ns = *self.ns_index.get("expr")?;
        match self.nss[ns].members.get(name)? {
            (Member::Generic(t), _, _) => Some(*t),
            _ => None,
        }
    }

    /// `expr<F>`: the type `expr.Expr<F>`.
    pub(crate) fn expr_tree_type(&mut self, f: TypeId, span: Span) -> TypeId {
        if !self.tt.is_invalid(f) && !matches!(self.tt.get(f), Type::Func(_) | Type::Param(_)) {
            let t = self.tn(f);
            self.err(format!("expr<{t}>: the type argument of expr must be a function type (expr<func(*User) bool>)"), span);
            return self.tt.invalid;
        }
        let Some(t) = self.expr_template("Expr") else { return self.tt.invalid };
        match self.instantiate(t, vec![f], span) {
            Some(generics::Inst::Struct(s)) => self.struct_ty(s),
            _ => self.tt.invalid,
        }
    }

    /// `F` of an `expr<F>` type.
    pub(crate) fn expr_tree_fn(&self, t: TypeId) -> Option<TypeId> {
        let Type::Struct(d) = self.tt.get(t) else { return None };
        if !self.structs.get(d.0 as usize)?.full_name.starts_with("expr.Expr<") {
            return None;
        }
        let (tm, args) = self.gen.inst_structs.get(&d.0)?;
        (Some(*tm) == self.expr_template("Expr") && args.len() == 1).then(|| args[0])
    }

    /// A lambda `e` checked where `expr<F>` (`tree_ty`) is expected.
    pub(crate) fn expr_tree(&mut self, e: &ast::Expr, f: TypeId, tree_ty: TypeId) -> hir::Expr {
        self.expr_tree_in(e, f, Some(tree_ty))
    }

    /// A lambda `e` passed for an `expr<F>` parameter of a generic call
    /// before the type arguments are known: checked with the parameter
    /// types of `hint` (`func(P...) void`), its result inferred from the
    /// body; the tree has type `expr<func(P...) R>`.
    pub(crate) fn expr_tree_infer(&mut self, e: &ast::Expr, hint: TypeId) -> hir::Expr {
        self.expr_tree_in(e, hint, None)
    }

    fn expr_tree_in(&mut self, e: &ast::Expr, f: TypeId, tree_ty: Option<TypeId>) -> hir::Expr {
        let span = e.span;
        let ast::ExprKind::Lambda { params, ret, body, is_async } = &expr::strip_parens(e).kind else { return self.invalid(span) };
        let fsig = match self.tt.get(f).clone() {
            Type::Func(sig) => sig,
            _ => return self.invalid(span),
        };
        if *is_async || (tree_ty.is_some() && matches!(self.tt.get(fsig.ret), Type::Task(_))) {
            self.err("an expression tree cannot be async", span);
            return self.invalid(span);
        }
        let x = match &**body {
            ast::LambdaBody::Block(b) => {
                self.err("an expression tree must be a single expression: a block body is not allowed (assignments and statements cannot be translated)", b.span);
                return self.invalid(span);
            }
            ast::LambdaBody::Expr(x) => x,
        };
        let mut bad = Vec::new();
        forbidden_syntax(x, false, &mut bad);
        if !bad.is_empty() {
            for (m, s) in bad {
                self.err(m, s);
            }
            return self.invalid(span);
        }
        if self.fns.last().and_then(|c| c.func).is_none() {
            self.err("lambdas are not allowed in global initializers", span);
            return self.invalid(span);
        }
        let before: Vec<bool> = self.fcx().locals.iter().map(|l| l.captured).collect();
        let clo = self.lambda(params, ret.as_deref(), body, false, span, Some(f));
        let ExprKind::Closure { func: fid, captures } = clo.kind else { return clo };
        if self.tt.is_invalid(clo.ty) {
            return self.invalid(span);
        }
        let (f, tree_ty) = match tree_ty {
            Some(t) => (f, t),
            None => {
                let t = self.expr_tree_type(clo.ty, span);
                if self.tt.is_invalid(t) {
                    return self.invalid(span);
                }
                (clo.ty, t)
            }
        };
        if clo.ty != f && !self.tree_coerce_result(fid, clo.ty, f) {
            let (s, t) = (self.tn(clo.ty), self.tn(f));
            self.err(format!("cannot use lambda of type {s} as expression tree expr<{t}>"), span);
            return self.invalid(span);
        }
        let fsig = match self.tt.get(f).clone() {
            Type::Func(sig) => sig,
            _ => return self.invalid(span),
        };
        // The body: `return <e>`.
        let mut fbody = std::mem::take(&mut self.funcs[fid.0 as usize].body);
        let single = fbody.stmts.len() == 1;
        let root = match fbody.stmts.first_mut().map(|s| &mut s.kind) {
            Some(StmtKind::Return(ReturnValue::Value(v))) if single => v,
            _ => {
                self.err("an expression tree must produce a value", span);
                return self.invalid(span);
            }
        };
        let fdecl = &self.funcs[fid.0 as usize];
        let n = fsig.params.len() as u32;
        let mut tb = Tb { ptypes: fsig.params.clone(), ..Tb::default() };
        tb.outer = fdecl.captures.iter().zip(&captures).map(|(c, s)| (*s, c.ty, c.name.clone())).collect();
        tb.frames.push(Frame { fid, nparams: n, base: 0, parent: 0, caps: Vec::new(), names: Vec::new() });
        for (i, p) in params.iter().enumerate() {
            tb.str(&p.name.name);
            tb.ty(fsig.params[i]);
        }
        self.tree_emit(&mut tb, 0, root);
        if !tb.errors.is_empty() {
            for (m, s) in std::mem::take(&mut tb.errors) {
                self.err(m, s);
            }
            self.funcs[fid.0 as usize].body = fbody;
            return self.invalid(span);
        }
        self.funcs[fid.0 as usize].body = fbody;
        self.tree_finish_frames(&tb);
        // The creating function's locals read by the lambda are evaluated
        // now, not captured by reference (unless a closure in a slot keeps
        // them).
        for (i, was) in before.into_iter().enumerate() {
            if !was && !tb.keep.contains(&LocalId(i as u32)) {
                self.fcx().locals[i].captured = false;
            }
        }
        let env_ty = (!tb.caps.is_empty()).then(|| self.tt.tuple(tb.caps.clone()));
        let caps: Vec<hir::Capture> = tb.caps.iter().enumerate().map(|(i, t)| hir::Capture { name: format!("$cap{i}"), ty: *t, source: hir::CaptureSource::Upvalue(i as u32) }).collect();
        {
            let lf = &mut self.funcs[fid.0 as usize];
            lf.captures = caps.clone();
            lf.value_env = env_ty;
        }
        self.load_reflect();
        let parent = self.fns.last().and_then(|c| c.func).unwrap();
        let int = self.tt.int;
        let any = self.tt.any;
        // `vals(k) any`: slot k's value.
        let mut vbody = Vec::new();
        for (k, s) in tb.slots.iter().enumerate() {
            let v = match s.cap {
                Some(c) => mk(ExprKind::Upvalue(c), tb.caps[c as usize], span),
                None => s.value.clone(),
            };
            let v = self.coerce(v, any, Ctx::Return);
            vbody.push(self.tree_case(k, v, span));
        }
        vbody.push(ret_stmt(mk(ExprKind::Nil, any, span)));
        let vals = self.tree_lambda(parent, "values", int, any, vbody, &caps, env_ty, span);
        // `types(k) *reflect.TypeInfo`.
        let tip = {
            let ti = self.refl_struct_ty("TypeInfo");
            self.tt.pointer(ti)
        };
        let mut tbody = Vec::new();
        for (k, t) in tb.types.clone().into_iter().enumerate() {
            let v = self.type_of_call(t, span);
            tbody.push(self.tree_case(k, v, span));
        }
        tbody.push(ret_stmt(mk(ExprKind::Nil, tip, span)));
        let types = self.tree_lambda(parent, "types", int, tip, tbody, &caps, env_ty, span);
        // The environment: the captured values, evaluated now.
        let env = env_ty.map(|t| {
            let vs: Vec<hir::Expr> = tb.slots.iter().filter(|s| s.cap.is_some()).map(|s| s.value.clone()).collect();
            Box::new(mk(ExprKind::Tuple(vs), t, span))
        });
        let Type::Struct(d) = *self.tt.get(tree_ty) else { return self.invalid(span) };
        let sid = d.0;
        let field = |me: &Self, n: &str| me.structs[sid as usize].fields.iter().position(|f| f.name == n).map(|i| (i as u32, me.structs[sid as usize].fields[i].ty));
        let (Some((fi_site, site_ty)), Some((fi_fns, fns_ty))) = (field(self, "site"), field(self, "fns")) else { return self.invalid(span) };
        let fns = mk(ExprKind::EnvClosures { funcs: vec![fid, vals, types], env }, fns_ty, span);
        let Some(site_sid) = self.expr_template("Site").and_then(|t| self.plain_std_struct(t, span)) else { return self.invalid(span) };
        let ints = |xs: &[i128]| ConstValue::List(xs.iter().map(|i| ConstValue::Int(*i)).collect());
        let attr = hir::Attr {
            kind: hir::AttrKind::User(site_sid),
            args: vec![
                ("ops".into(), ints(&tb.ops)),
                ("strs".into(), ConstValue::List(tb.strs.iter().map(|s| ConstValue::Str(s.clone())).collect())),
                ("params".into(), ConstValue::Int(n as i128)),
            ],
            span,
        };
        let site = mk(ExprKind::AttrConst { attr: Box::new(attr), sid: site_sid }, site_ty, span);
        mk(ExprKind::StructLit { struct_id: sid, fields: vec![(fi_site, site), (fi_fns, fns)] }, tree_ty, span)
    }

    /// A lambda of type `got` where the tree's function type `want` (same
    /// parameters) returns an interface the result implements (`any` for
    /// keys and navigations): the returned value is converted. False if
    /// it does not apply.
    fn tree_coerce_result(&mut self, fid: FuncId, got: TypeId, want: TypeId) -> bool {
        let (Type::Func(g), Type::Func(w)) = (self.tt.get(got).clone(), self.tt.get(want).clone()) else { return false };
        if g.params != w.params || !self.tt.is_interface(w.ret) || self.tt.is_fallible(g.ret) || matches!(self.tt.get(g.ret), Type::Void) {
            return false;
        }
        if !matches!(self.tt.get(w.ret), Type::Any) && !self.implements(g.ret, w.ret) {
            return false;
        }
        let body = &mut self.funcs[fid.0 as usize].body;
        let Some(StmtKind::Return(ReturnValue::Value(v))) = body.stmts.first_mut().map(|s| &mut s.kind) else { return false };
        let span = v.span;
        let inner = std::mem::replace(v, mk(ExprKind::Nil, w.ret, span));
        *v = mk(ExprKind::ToInterface { value: Box::new(inner) }, w.ret, span);
        self.funcs[fid.0 as usize].ret = w.ret;
        self.sigs[fid.0 as usize].ret = w.ret;
        true
    }

    /// `if k == <k> { return <v> }` of a synthesized `(k int)` lambda.
    fn tree_case(&mut self, k: usize, v: hir::Expr, span: Span) -> hir::Stmt {
        let int = self.tt.int;
        let b = self.tt.bool_;
        let cond = mk(
            ExprKind::Binary { op: BinOp::Eq, lhs: Box::new(mk(ExprKind::Local(LocalId(0)), int, span)), rhs: Box::new(mk(ExprKind::Const(ConstValue::Int(k as i128)), int, span)) },
            b,
            span,
        );
        let then = hir::Block { stmts: vec![ret_stmt(v)], span };
        hir::Stmt { kind: StmtKind::If { cond, then, else_: None }, span }
    }

    /// A synthesized lambda `(k int) ret { body }` of `parent` over the
    /// value environment `env` of a tree site.
    #[allow(clippy::too_many_arguments)]
    fn tree_lambda(&mut self, parent: FuncId, what: &str, pty: TypeId, ret: TypeId, body: Vec<hir::Stmt>, caps: &[hir::Capture], env: Option<TypeId>, span: Span) -> FuncId {
        let fid = FuncId(self.funcs.len() as u32);
        self.lambda_count += 1;
        let name = format!("lambda{}", self.lambda_count);
        let ns = self.funcs[parent.0 as usize].namespace;
        let mut f = self.dummy_function(name.clone(), hir::FuncKind::Lambda { parent }, ns);
        f.full_name = format!("{}${name}${what}", self.funcs[parent.0 as usize].full_name);
        f.symbol = format!("zell_{}", f.full_name.replace('.', "__").replace('$', "_"));
        f.span = span;
        f.file = self.funcs[parent.0 as usize].file;
        f.locals = vec![hir::Local { name: "k".into(), ty: pty, kind: LocalKind::Param, captured: false, address_taken: false, span }];
        f.params = vec![LocalId(0)];
        f.captures = caps.to_vec();
        f.value_env = env;
        f.ret = ret;
        f.body = hir::Block { stmts: body, span };
        self.funcs.push(f);
        self.func_src.push(FuncSrc::Synthetic);
        self.sigs.push(Sig { params: vec![pty], ret, is_async: false });
        fid
    }

    /// Whether function `fid` is marked `@expr.call` (allowed in trees).
    fn tree_marked(&self, fid: FuncId) -> bool {
        let d = match self.func_src.get(fid.0 as usize) {
            Some(FuncSrc::Free(d)) | Some(FuncSrc::Method(d, _)) => *d,
            _ => return false,
        };
        d.attrs.iter().any(|a| {
            let s: Vec<&str> = a.path.segments.iter().map(|x| x.name.as_str()).collect();
            s == ["expr", "call"]
        })
    }

    /// Name of an allowed call target (`strings.HasPrefix`, `data.Any`,
    /// `data.Group.Count` for a method), or the error for another one.
    fn tree_call(&self, callee: &Callee) -> Result<String, String> {
        match callee {
            Callee::Extern(id) => {
                let n = self.externs[id.0 as usize].path.clone();
                if TREE_CALLS.contains(&n.as_str()) {
                    Ok(n)
                } else {
                    Err(format!("call of {n} is not allowed in an expression tree (allowed: {}, and functions marked @expr.call)", TREE_CALLS.join(", ")))
                }
            }
            Callee::Func(fid) => {
                let f = &self.funcs[fid.0 as usize];
                let name = strip_type_args(&f.full_name);
                let ok = TREE_CALLS.contains(&name.as_str()) || self.tree_marked(*fid);
                match f.kind {
                    _ if ok => Ok(name),
                    hir::FuncKind::Free => Err(format!("call of {name} is not allowed in an expression tree (allowed: {}, and functions marked @expr.call)", TREE_CALLS.join(", "))),
                    hir::FuncKind::Method { .. } => {
                        let short = name.rsplit('.').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(".");
                        Err(format!("method call {short} is not allowed in an expression tree (only methods marked @expr.call)"))
                    }
                    hir::FuncKind::Lambda { .. } => Err("a call of a lambda is not allowed in an expression tree".into()),
                }
            }
            Callee::Interface { .. } => Err("an interface method call is not allowed in an expression tree".into()),
            Callee::Value(_) => Err("a call of a function value is not allowed in an expression tree".into()),
        }
    }

    /// `e` is a call of method `name` with one argument (`a.Compare(b)`).
    fn is_method_call(&self, e: &hir::Expr, name: &str) -> bool {
        match &e.kind {
            ExprKind::Call { callee: Callee::Func(fid), args } if args.len() == 2 => {
                let f = &self.funcs[fid.0 as usize];
                matches!(f.kind, hir::FuncKind::Method { .. }) && f.name == name
            }
            _ => false,
        }
    }

    /// Source text of a captured slot (`n`, `req.MinAge`) in frame `f`.
    fn tree_slot_name(&self, tb: &Tb, f: usize, e: &hir::Expr) -> String {
        match &e.kind {
            ExprKind::Upvalue(i) if f == 0 => tb.outer.get(*i as usize).map(|o| o.2.clone()).unwrap_or_default(),
            ExprKind::Upvalue(i) => tb.frames[f].names.get(*i as usize).cloned().unwrap_or_default(),
            ExprKind::Global(g) => self.globals[g.0 as usize].name.clone(),
            ExprKind::Deref(x) => self.tree_slot_name(tb, f, x),
            ExprKind::Field { base, index } => {
                let b = self.tree_slot_name(tb, f, base);
                let fname = match self.tt.get(base.ty) {
                    Type::Struct(d) => self.structs[d.0 as usize].fields.get(*index as usize).map(|f| f.name.clone()).unwrap_or_default(),
                    Type::Tuple(_) => index.to_string(),
                    _ => String::new(),
                };
                if b.is_empty() {
                    String::new()
                } else {
                    format!("{b}.{fname}")
                }
            }
            // `strings.ToLower(prefix)`, `len(names)`: calls of named values.
            ExprKind::Call { args, .. } | ExprKind::Builtin { args, .. } => {
                let fname = match &e.kind {
                    ExprKind::Call { callee, .. } => match callee {
                        Callee::Extern(id) => self.externs[id.0 as usize].path.clone(),
                        Callee::Func(fid) => strip_type_args(&self.funcs[fid.0 as usize].full_name),
                        _ => return String::new(),
                    },
                    _ => "len".to_string(),
                };
                let parts: Vec<String> = args.iter().map(|a| self.tree_slot_name(tb, f, a)).collect();
                if parts.iter().any(|p| p.is_empty()) {
                    String::new()
                } else {
                    format!("{fname}({})", parts.join(", "))
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                const SYMS: [&str; 13] = ["+", "-", "*", "/", "%", "==", "!=", "<", "<=", ">", ">=", "&&", "||"];
                let (l, r) = (self.tree_slot_name(tb, f, lhs), self.tree_slot_name(tb, f, rhs));
                match op_code(*op) {
                    Some(c) if !l.is_empty() && !r.is_empty() => format!("({l} {} {r})", SYMS[c as usize]),
                    _ => String::new(),
                }
            }
            ExprKind::Index { base, index } => {
                let (b, i) = (self.tree_slot_name(tb, f, base), self.tree_slot_name(tb, f, index));
                if b.is_empty() || i.is_empty() {
                    String::new()
                } else {
                    format!("{b}[{i}]")
                }
            }
            ExprKind::Const(ConstValue::Str(s)) => format!("{s:?}"),
            ExprKind::Const(ConstValue::Int(i)) => i.to_string(),
            _ => String::new(),
        }
    }

    /// Makes the parameter-free expression `e` of frame `f` a slot and
    /// replaces it by a read of the slot.
    fn tree_slot(&mut self, tb: &mut Tb, f: usize, e: &mut hir::Expr) {
        let ty = tb.ty(e.ty);
        let k = tb.slots.len() as i128;
        if is_literal(e) {
            tb.ops.extend([T_CONST, ty, k]);
            tb.slots.push(Slot { value: e.clone(), cap: None, name: String::new() });
            return;
        }
        let read = |c: u32| if f == 0 { ExprKind::Upvalue(c) } else { ExprKind::Upvalue(MARK_SLOT + c) };
        let mut name = self.tree_slot_name(tb, f, e);
        if let Some((k, s)) = tb.slots.iter().enumerate().find(|(_, s)| !name.is_empty() && s.name == name && s.cap.is_some()) {
            let (c, n) = (s.cap.unwrap(), tb.str(&name));
            tb.ops.extend([T_CAPTURED, ty, k as i128, n]);
            *e = mk(read(c), e.ty, e.span);
            return;
        }
        let named = !name.is_empty();
        if name.is_empty() {
            name = format!("${k}");
        }
        let n = tb.str(&name);
        tb.ops.extend([T_CAPTURED, ty, k, n]);
        let mut value = e.clone();
        tb.to_creator(f, &mut value);
        self.tree_reparent(&value);
        let c = tb.caps.len() as u32;
        tb.caps.push(e.ty);
        tb.slots.push(Slot { value, cap: Some(c), name: if named { name } else { String::new() } });
        *e = mk(read(c), e.ty, e.span);
    }

    /// Closures of a slot value are created by the creating function now.
    fn tree_reparent(&mut self, e: &hir::Expr) {
        if let ExprKind::Closure { func, .. } = &e.kind {
            if let Some(parent) = self.fns.last().and_then(|c| c.func) {
                self.funcs[func.0 as usize].kind = hir::FuncKind::Lambda { parent };
            }
            return;
        }
        for c in children(e) {
            self.tree_reparent(c);
        }
    }

    /// Encodes expression `e` of frame `f`, turning parameter-free
    /// subexpressions into slots (captured ones are replaced by a read of
    /// the environment).
    fn tree_emit(&mut self, tb: &mut Tb, f: usize, e: &mut hir::Expr) {
        if !tb.mentions_param(f, e) {
            self.tree_slot(tb, f, e);
            return;
        }
        let ty = tb.ty(e.ty);
        let span = e.span;
        // Operators on types with Compare/Equals: `a.Compare(b) < 0`, `a.Equals(b)`.
        let cmp = match &e.kind {
            ExprKind::Binary { op: op @ (BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), lhs, rhs } if self.is_method_call(lhs, "Compare") && matches!(rhs.kind, ExprKind::Const(ConstValue::Int(0))) => op_code(*op),
            ExprKind::Call { .. } if self.is_method_call(e, "Equals") => Some(5),
            ExprKind::Unary { op: UnOp::Not, operand } if self.is_method_call(operand, "Equals") => Some(6),
            _ => None,
        };
        if let Some(code) = cmp {
            let call = match &mut e.kind {
                ExprKind::Binary { lhs, .. } => &mut **lhs,
                ExprKind::Unary { operand, .. } => &mut **operand,
                _ => e,
            };
            let ExprKind::Call { args, .. } = &mut call.kind else { unreachable!() };
            tb.ops.extend([T_BINARY, ty, code]);
            let (a, b) = args.split_at_mut(1);
            self.tree_emit(tb, f, recv_mut(&mut a[0]));
            self.tree_emit(tb, f, &mut b[0]);
            return;
        }
        match &mut e.kind {
            ExprKind::Local(l) => {
                if l.0 < tb.frames[f].nparams {
                    let p = tb.frames[f].base + l.0;
                    tb.ops.extend([T_PARAM, ty, p as i128]);
                } else {
                    tb.bad("a local variable", span);
                }
            }
            ExprKind::Upvalue(j) => match tb.resolve_up(f, *j) {
                Some(Res::Param(p)) => {
                    tb.ops.extend([T_PARAM, ty, p as i128]);
                    e.kind = ExprKind::Upvalue(MARK_PARAM + p);
                }
                _ => tb.bad("a local variable", span),
            },
            ExprKind::Deref(x) => self.tree_emit(tb, f, x),
            ExprKind::Field { base, index } => {
                let owner = base.ty;
                let name = match self.tt.get(owner) {
                    Type::Struct(d) => self.structs[d.0 as usize].fields.get(*index as usize).map(|f| f.name.clone()).unwrap_or_default(),
                    _ => index.to_string(),
                };
                let n = tb.str(&name);
                let o = tb.ty(owner);
                tb.ops.extend([T_FIELD, ty, *index as i128, n, o]);
                self.tree_emit(tb, f, base);
            }
            ExprKind::Binary { op, lhs, rhs } => match op_code(*op) {
                Some(c) => {
                    tb.ops.extend([T_BINARY, ty, c]);
                    self.tree_emit(tb, f, lhs);
                    self.tree_emit(tb, f, rhs);
                }
                None => tb.bad(format!("operator {}", bit_op_symbol(*op)), span),
            },
            ExprKind::Unary { op: op @ (UnOp::Neg | UnOp::Not), operand } => {
                tb.ops.extend([T_UNARY, ty, if *op == UnOp::Not { 13 } else { 14 }]);
                self.tree_emit(tb, f, operand);
            }
            ExprKind::IsNil { value, negated } => {
                tb.ops.extend([T_ISNIL, ty, *negated as i128]);
                self.tree_emit(tb, f, value);
            }
            ExprKind::Builtin { op: Builtin::LenString | Builtin::LenSlice | Builtin::LenArray | Builtin::LenMap, args } => {
                let n = tb.str("len");
                tb.ops.extend([T_CALL, ty, n, args.len() as i128]);
                for a in args {
                    self.tree_emit(tb, f, a);
                }
            }
            ExprKind::Call { callee, args } => match self.tree_call(callee) {
                Ok(name) => {
                    let method = matches!(callee, Callee::Func(fid) if matches!(self.funcs[fid.0 as usize].kind, hir::FuncKind::Method { .. }));
                    let n = tb.str(&name);
                    tb.ops.extend([T_CALL, ty, n, args.len() as i128]);
                    for (i, a) in args.iter_mut().enumerate() {
                        let a = if i == 0 && method { recv_mut(a) } else { a };
                        if matches!(a.kind, ExprKind::Closure { .. }) {
                            self.tree_lambda_node(tb, f, a);
                        } else {
                            self.tree_emit(tb, f, a);
                        }
                    }
                }
                Err(m) => tb.errors.push((m, span)),
            },
            ExprKind::Convert { value, .. } | ExprKind::ToInterface { value } => {
                tb.ops.extend([T_CONVERT, ty]);
                self.tree_emit(tb, f, value);
            }
            ExprKind::Index { base, index } => {
                tb.ops.extend([T_INDEX, ty]);
                self.tree_emit(tb, f, base);
                self.tree_emit(tb, f, index);
            }
            ExprKind::Tuple(xs) => {
                tb.ops.extend([T_TUPLE, ty, xs.len() as i128]);
                for x in xs {
                    self.tree_emit(tb, f, x);
                }
            }
            ExprKind::StructLit { struct_id, fields } => {
                tb.ops.extend([T_STRUCT, ty, fields.len() as i128]);
                let sid = *struct_id;
                for (i, x) in fields {
                    let name = self.structs[sid as usize].fields.get(*i as usize).map(|f| f.name.clone()).unwrap_or_default();
                    let n = tb.str(&name);
                    tb.ops.extend([*i as i128, n]);
                    self.tree_emit(tb, f, x);
                }
            }
            ExprKind::Closure { .. } => tb.errors.push(("a nested lambda is only allowed as an argument of a call in an expression tree".into(), span)),
            ExprKind::Builtin { .. } => tb.bad("this builtin", span),
            ExprKind::MapGet { .. } | ExprKind::SubSlice { .. } => tb.bad("indexing of a map or slicing", span),
            ExprKind::SliceLit(_) | ExprKind::MapLit(_) => tb.bad("a slice or map literal", span),
            ExprKind::AddrOf(_) | ExprKind::New(_) => tb.bad("taking an address", span),
            ExprKind::EnvClosures { .. } | ExprKind::FuncRef(_) => tb.bad("a function value", span),
            ExprKind::Await(_) | ExprKind::TaskAll(_) => tb.bad("await", span),
            ExprKind::Propagate { .. } | ExprKind::TryCatch { .. } | ExprKind::TryElse { .. } => tb.bad("error handling", span),
            ExprKind::EnumLit { .. } => tb.bad("an enum value with payload", span),
            _ => tb.bad("this expression", span),
        }
    }

    /// Encodes the lambda `arg` (an argument of an allowed call in frame
    /// `f`) as a `Lambda` node over a new frame.
    fn tree_lambda_node(&mut self, tb: &mut Tb, f: usize, arg: &mut hir::Expr) {
        let span = arg.span;
        let ExprKind::Closure { func: g, captures } = &arg.kind else { return };
        let (g, captures) = (*g, captures.clone());
        let sig = self.sigs[g.0 as usize].clone();
        if sig.is_async {
            tb.errors.push(("an async lambda is not allowed in an expression tree".into(), span));
            return;
        }
        let k = sig.params.len() as u32;
        let base = tb.ptypes.len() as u32;
        let ty = tb.ty(arg.ty);
        tb.ops.extend([T_LAMBDA, ty, base as i128, k as i128]);
        let names: Vec<String> = self.funcs[g.0 as usize].locals.iter().take(k as usize).map(|l| l.name.clone()).collect();
        for (i, p) in sig.params.iter().enumerate() {
            let n = tb.str(names.get(i).map(|s| s.as_str()).unwrap_or("_"));
            let t = tb.ty(*p);
            tb.ops.extend([n, t]);
            tb.ptypes.push(*p);
        }
        let capnames = self.funcs[g.0 as usize].captures.iter().map(|c| c.name.clone()).collect();
        let fi = tb.frames.len();
        tb.frames.push(Frame { fid: g, nparams: k, base, parent: f, caps: captures, names: capnames });
        let mut body = std::mem::take(&mut self.funcs[g.0 as usize].body);
        let single = body.stmts.len() == 1;
        match body.stmts.first_mut().map(|s| &mut s.kind) {
            Some(StmtKind::Return(ReturnValue::Value(v))) if single => self.tree_emit(tb, fi, v),
            _ => tb.errors.push(("a nested lambda of an expression tree must be a single expression with a value".into(), span)),
        }
        self.funcs[g.0 as usize].body = body;
    }

    /// Gives every nested lambda its value environment (innermost first)
    /// and replaces its closure in the enclosing lambda by one built over
    /// that environment.
    fn tree_finish_frames(&mut self, tb: &Tb) {
        for fi in (1..tb.frames.len()).rev() {
            let fr = &tb.frames[fi];
            let g = fr.fid;
            let mut body = std::mem::take(&mut self.funcs[g.0 as usize].body);
            let mut need: Vec<u32> = Vec::new();
            block_marks_mut(&mut body, &mut |x| {
                let ExprKind::Upvalue(m) = x.kind else { return };
                let i = match need.iter().position(|n| *n == m) {
                    Some(i) => i,
                    None => {
                        need.push(m);
                        need.len() - 1
                    }
                };
                x.kind = ExprKind::Upvalue(i as u32);
            });
            let mark_ty = |m: u32| if m >= MARK_SLOT { tb.caps[(m - MARK_SLOT) as usize] } else { tb.ptypes[(m - MARK_PARAM) as usize] };
            let tys: Vec<TypeId> = need.iter().map(|m| mark_ty(*m)).collect();
            let env_ty = (!tys.is_empty()).then(|| self.tt.tuple(tys.clone()));
            {
                let gf = &mut self.funcs[g.0 as usize];
                gf.body = body;
                gf.captures = tys.iter().enumerate().map(|(i, t)| hir::Capture { name: format!("$env{i}"), ty: *t, source: CaptureSource::Upvalue(i as u32) }).collect();
                gf.value_env = env_ty;
            }
            // The closure, built by the enclosing lambda over the values.
            let pf = &tb.frames[fr.parent];
            let span = self.funcs[g.0 as usize].span;
            let reads: Vec<hir::Expr> = need
                .iter()
                .map(|&m| {
                    let t = mark_ty(m);
                    let kind = if m >= MARK_SLOT {
                        if fr.parent == 0 {
                            ExprKind::Upvalue(m - MARK_SLOT)
                        } else {
                            ExprKind::Upvalue(m)
                        }
                    } else {
                        let p = m - MARK_PARAM;
                        if p >= pf.base && p < pf.base + pf.nparams {
                            ExprKind::Local(LocalId(p - pf.base))
                        } else {
                            ExprKind::Upvalue(m)
                        }
                    };
                    mk(kind, t, span)
                })
                .collect();
            let fty = {
                let s = self.sigs[g.0 as usize].clone();
                self.tt.func(s.params, s.ret)
            };
            let tup = self.tt.tuple(vec![fty]);
            let env = env_ty.map(|t| Box::new(mk(ExprKind::Tuple(reads), t, span)));
            let clo = mk(ExprKind::EnvClosures { funcs: vec![g], env }, tup, span);
            let mut with = Some(mk(ExprKind::Field { base: Box::new(clo), index: 0 }, fty, span));
            let pfid = pf.fid;
            let mut pbody = std::mem::take(&mut self.funcs[pfid.0 as usize].body);
            for s in &mut pbody.stmts {
                if let StmtKind::Return(ReturnValue::Value(v)) = &mut s.kind {
                    replace_closure(v, g, &mut with);
                }
            }
            self.funcs[pfid.0 as usize].body = pbody;
        }
    }
}

fn ret_stmt(v: hir::Expr) -> hir::Stmt {
    let span = v.span;
    hir::Stmt { kind: StmtKind::Return(ReturnValue::Value(v)), span }
}
