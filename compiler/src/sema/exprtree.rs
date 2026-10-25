//! Expression trees (DESIGN.md "Деревья выражений"): a lambda checked
//! where `expr<F>` is expected becomes an `expr.Expr<F>` value
//! (`stdlib_src/expr.zl`) instead of a closure.
//!
//! The lambda is first checked as an ordinary lambda of type `F` (lifted
//! into a function, parameter types inferred). Its typed body is then
//! validated against the node set ([`TREE_CALLS`] is the call whitelist)
//! and encoded in prefix order into the constant `expr.Site` of the lambda
//! site (an [`ExprKind::AttrConst`], immortal, in read-only data). Every
//! maximal subexpression that does not mention a parameter is a *slot*: a
//! literal stays a constant slot; anything else (a captured variable, a
//! field path of one, a global) is a *captured* slot, evaluated once when
//! the tree is built. The body of the lifted lambda is rewritten to read
//! the captured slots from its environment, so `Compile()` sees the same
//! values as the tree ("values, not variables").
//!
//! The environment holds the captured values themselves (a tuple in one
//! heap box, `hir::Function::value_env`): building a tree allocates that
//! box only, nothing when nothing is captured. Two synthesized lambdas over
//! the same environment give the library the slot values (`k -> any`) and
//! the static types of the nodes (`k -> *reflect.TypeInfo`); all three
//! closures are made by one [`ExprKind::EnvClosures`].

use super::coerce::Ctx;
use super::*;
use crate::hir::{BinOp, Builtin, Callee, ConstValue, ExprKind, ReturnValue, StmtKind, UnOp};

/// The functions an expression tree may call, by full name (`len` is the
/// builtin). One table so that it can grow; the `expr` printer and the
/// translators of libraries see the same names.
pub(crate) const TREE_CALLS: &[&str] = &["strings.HasPrefix", "strings.HasSuffix", "strings.Contains", "strings.ToLower", "strings.ToUpper", "len", "slices.Contains"];

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

/// Encoding state of one lambda site.
#[derive(Default)]
struct Tb {
    ops: Vec<i128>,
    strs: Vec<String>,
    types: Vec<TypeId>,
    slots: Vec<Slot>,
    /// Types of the captured slots, in environment order.
    caps: Vec<TypeId>,
    nparams: u32,
    /// Capture `i` of the checked lambda: where the creating function
    /// finds it, and its name.
    outer: Vec<(hir::CaptureSource, TypeId, String)>,
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

/// The subexpressions of an (already validated) tree expression.
fn children(e: &hir::Expr) -> Vec<&hir::Expr> {
    match &e.kind {
        ExprKind::Field { base, .. } => vec![base],
        ExprKind::Deref(x) | ExprKind::Unary { operand: x, .. } | ExprKind::IsNil { value: x, .. } => vec![x],
        ExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        ExprKind::Call { args, .. } | ExprKind::Builtin { args, .. } | ExprKind::Tuple(args) | ExprKind::EnumLit { args, .. } => args.iter().collect(),
        ExprKind::StructLit { fields, .. } => fields.iter().map(|(_, x)| x).collect(),
        _ => Vec::new(),
    }
}

fn children_mut(e: &mut hir::Expr) -> Vec<&mut hir::Expr> {
    match &mut e.kind {
        ExprKind::Field { base, .. } => vec![base],
        ExprKind::Deref(x) | ExprKind::Unary { operand: x, .. } | ExprKind::IsNil { value: x, .. } => vec![x],
        ExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        ExprKind::Call { args, .. } | ExprKind::Builtin { args, .. } | ExprKind::Tuple(args) | ExprKind::EnumLit { args, .. } => args.iter_mut().collect(),
        ExprKind::StructLit { fields, .. } => fields.iter_mut().map(|(_, x)| x).collect(),
        _ => Vec::new(),
    }
}

/// `e` reads a parameter of the lambda (locals `0..n`).
fn mentions_param(e: &hir::Expr, n: u32) -> bool {
    matches!(e.kind, ExprKind::Local(l) if l.0 < n) || children(e).into_iter().any(|c| mentions_param(c, n))
}

/// A literal: stays a constant of the tree.
fn is_literal(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Const(_) | ExprKind::Nil | ExprKind::Zero | ExprKind::AttrConst { .. } => true,
        ExprKind::EnumLit { args, .. } => args.is_empty(),
        _ => false,
    }
}

/// The forms rejected before the lambda is checked: `(message, span)`.
fn forbidden_syntax(e: &ast::Expr, out: &mut Vec<(String, Span)>) {
    use ast::ExprKind as K;
    let bad = match &e.kind {
        K::Await(_) => Some("await is not allowed in an expression tree"),
        K::Lambda { .. } => Some("a nested lambda is not allowed in an expression tree"),
        K::Propagate { .. } => Some("? is not allowed in an expression tree"),
        K::Try { .. } => Some("try is not allowed in an expression tree"),
        _ => None,
    };
    if let Some(m) = bad {
        out.push((m.to_string(), e.span));
        return;
    }
    let mut go = |x: &ast::Expr| forbidden_syntax(x, out);
    match &e.kind {
        K::Field { base, .. } | K::TypeArgs { base, .. } => go(base),
        K::Call { callee, args } => {
            go(callee);
            args.iter().for_each(&mut go);
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
        let span = e.span;
        let ast::ExprKind::Lambda { params, ret, body, is_async } = &e.kind else { return self.invalid(span) };
        let fsig = match self.tt.get(f).clone() {
            Type::Func(sig) => sig,
            _ => return self.invalid(span),
        };
        if *is_async || matches!(self.tt.get(fsig.ret), Type::Task(_)) {
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
        forbidden_syntax(x, &mut bad);
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
        if clo.ty != f {
            if !self.tt.is_invalid(clo.ty) {
                let (s, t) = (self.tn(clo.ty), self.tn(f));
                self.err(format!("cannot use lambda of type {s} as expression tree expr<{t}>"), span);
            }
            return self.invalid(span);
        }
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
        let mut tb = Tb { nparams: fsig.params.len() as u32, ..Tb::default() };
        tb.outer = fdecl.captures.iter().zip(&captures).map(|(c, s)| (*s, c.ty, c.name.clone())).collect();
        for (i, p) in params.iter().enumerate() {
            tb.str(&p.name.name);
            tb.ty(fsig.params[i]);
        }
        self.tree_check(&mut tb, root);
        if !tb.errors.is_empty() {
            for (m, s) in std::mem::take(&mut tb.errors) {
                self.err(m, s);
            }
            self.funcs[fid.0 as usize].body = fbody;
            return self.invalid(span);
        }
        self.tree_emit(&mut tb, root);
        self.funcs[fid.0 as usize].body = fbody;
        // The creating function's locals read by the lambda are evaluated
        // now, not captured by reference.
        for (i, was) in before.into_iter().enumerate() {
            if !was {
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
                ("params".into(), ConstValue::Int(tb.nparams as i128)),
            ],
            span,
        };
        let site = mk(ExprKind::AttrConst { attr: Box::new(attr), sid: site_sid }, site_ty, span);
        mk(ExprKind::StructLit { struct_id: sid, fields: vec![(fi_site, site), (fi_fns, fns)] }, tree_ty, span)
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

    /// Reports every sub-expression of the checked body `e` that a tree
    /// cannot hold.
    fn tree_check(&mut self, tb: &mut Tb, e: &hir::Expr) {
        let bad = |what: String| Some(format!("{what} is not allowed in an expression tree"));
        let err = match &e.kind {
            ExprKind::Local(l) if l.0 < tb.nparams => None,
            ExprKind::Const(_) | ExprKind::Nil | ExprKind::Zero | ExprKind::AttrConst { .. } | ExprKind::Upvalue(_) | ExprKind::Global(_) => None,
            ExprKind::EnumLit { .. } | ExprKind::Field { .. } | ExprKind::Deref(_) | ExprKind::IsNil { .. } | ExprKind::Tuple(_) | ExprKind::StructLit { .. } => None,
            ExprKind::Unary { op: UnOp::Neg | UnOp::Not, .. } => None,
            ExprKind::Binary { op, .. } => match op_code(*op) {
                Some(_) => None,
                None => bad(format!("operator {}", bit_op_symbol(*op))),
            },
            ExprKind::Builtin { op: Builtin::LenString | Builtin::LenSlice | Builtin::LenArray | Builtin::LenMap, .. } => None,
            ExprKind::Call { callee, .. } => match self.tree_call_name(callee) {
                Ok(n) if TREE_CALLS.contains(&n.as_str()) => None,
                Ok(n) => Some(format!("call of {n} is not allowed in an expression tree (allowed: {})", TREE_CALLS.join(", "))),
                Err(what) => bad(what),
            },
            ExprKind::Local(_) => bad("a local variable".into()),
            ExprKind::Builtin { .. } => bad("this builtin".into()),
            ExprKind::Convert { .. } => bad("a conversion".into()),
            ExprKind::ToInterface { .. } => bad("a conversion to an interface".into()),
            ExprKind::Index { .. } | ExprKind::MapGet { .. } | ExprKind::SubSlice { .. } => bad("indexing".into()),
            ExprKind::SliceLit(_) | ExprKind::MapLit(_) => bad("a slice or map literal".into()),
            ExprKind::AddrOf(_) | ExprKind::New(_) => bad("taking an address".into()),
            ExprKind::Closure { .. } | ExprKind::EnvClosures { .. } | ExprKind::FuncRef(_) => bad("a function value".into()),
            ExprKind::Await(_) | ExprKind::TaskAll(_) => bad("await".into()),
            ExprKind::Propagate { .. } | ExprKind::TryCatch { .. } | ExprKind::TryElse { .. } => bad("error handling".into()),
            _ => bad("this expression".into()),
        };
        if let Some(m) = err {
            tb.errors.push((m, e.span));
            return;
        }
        for c in children(e) {
            self.tree_check(tb, c);
        }
    }

    /// Name of a call target for the whitelist, or what it is when it is
    /// not a function of a namespace.
    fn tree_call_name(&self, callee: &Callee) -> Result<String, String> {
        match callee {
            Callee::Extern(id) => Ok(self.externs[id.0 as usize].path.clone()),
            Callee::Func(fid) => {
                let f = &self.funcs[fid.0 as usize];
                match f.kind {
                    hir::FuncKind::Free => Ok(f.full_name.split('<').next().unwrap_or("").to_string()),
                    hir::FuncKind::Method { .. } => Err(format!("method call {}", f.full_name.rsplit('.').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("."))),
                    hir::FuncKind::Lambda { .. } => Err("a call of a lambda".into()),
                }
            }
            Callee::Interface { .. } => Err("an interface method call".into()),
            Callee::Value(_) => Err("a call of a function value".into()),
        }
    }

    /// Source text of a captured slot (`n`, `req.MinAge`).
    fn tree_slot_name(&self, tb: &Tb, e: &hir::Expr) -> String {
        match &e.kind {
            ExprKind::Upvalue(i) => tb.outer.get(*i as usize).map(|o| o.2.clone()).unwrap_or_default(),
            ExprKind::Global(g) => self.globals[g.0 as usize].name.clone(),
            ExprKind::Deref(x) => self.tree_slot_name(tb, x),
            ExprKind::Field { base, index } => {
                let b = self.tree_slot_name(tb, base);
                let f = match self.tt.get(base.ty) {
                    Type::Struct(d) => self.structs[d.0 as usize].fields.get(*index as usize).map(|f| f.name.clone()).unwrap_or_default(),
                    Type::Tuple(_) => index.to_string(),
                    _ => String::new(),
                };
                if b.is_empty() {
                    String::new()
                } else {
                    format!("{b}.{f}")
                }
            }
            // `strings.ToLower(prefix)`, `len(names)`: whitelisted calls of
            // named values.
            ExprKind::Call { args, .. } | ExprKind::Builtin { args, .. } => {
                let f = match &e.kind {
                    ExprKind::Call { callee, .. } => self.tree_call_name(callee).unwrap_or_default(),
                    _ => "len".to_string(),
                };
                let parts: Vec<String> = args.iter().map(|a| self.tree_slot_name(tb, a)).collect();
                if parts.iter().any(|p| p.is_empty()) {
                    String::new()
                } else {
                    format!("{f}({})", parts.join(", "))
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                const SYMS: [&str; 13] = ["+", "-", "*", "/", "%", "==", "!=", "<", "<=", ">", ">=", "&&", "||"];
                let (l, r) = (self.tree_slot_name(tb, lhs), self.tree_slot_name(tb, rhs));
                match op_code(*op) {
                    Some(c) if !l.is_empty() && !r.is_empty() => format!("({l} {} {r})", SYMS[c as usize]),
                    _ => String::new(),
                }
            }
            ExprKind::Const(ConstValue::Str(s)) => format!("{s:?}"),
            ExprKind::Const(ConstValue::Int(i)) => i.to_string(),
            _ => String::new(),
        }
    }

    /// Encodes the validated expression `e`, turning parameter-free
    /// subexpressions into slots (captured ones are replaced by a read of
    /// the environment).
    fn tree_emit(&mut self, tb: &mut Tb, e: &mut hir::Expr) {
        let ty = tb.ty(e.ty);
        if !mentions_param(e, tb.nparams) {
            let k = tb.slots.len() as i128;
            if is_literal(e) {
                tb.ops.extend([T_CONST, ty, k]);
                tb.slots.push(Slot { value: e.clone(), cap: None, name: String::new() });
                return;
            }
            let mut name = self.tree_slot_name(tb, e);
            if let Some((k, s)) = tb.slots.iter().enumerate().find(|(_, s)| !name.is_empty() && s.name == name && s.cap.is_some()) {
                let (c, n) = (s.cap.unwrap(), tb.str(&name));
                tb.ops.extend([T_CAPTURED, ty, k as i128, n]);
                *e = mk(ExprKind::Upvalue(c), e.ty, e.span);
                return;
            }
            let named = !name.is_empty();
            if name.is_empty() {
                name = format!("${k}");
            }
            let n = tb.str(&name);
            tb.ops.extend([T_CAPTURED, ty, k, n]);
            let mut value = e.clone();
            outer_reads(&mut value, &tb.outer);
            let c = tb.caps.len() as u32;
            tb.caps.push(e.ty);
            tb.slots.push(Slot { value, cap: Some(c), name: if named { name } else { String::new() } });
            *e = mk(ExprKind::Upvalue(c), e.ty, e.span);
            return;
        }
        match &mut e.kind {
            ExprKind::Local(l) => tb.ops.extend([T_PARAM, ty, l.0 as i128]),
            ExprKind::Deref(x) => self.tree_emit(tb, x),
            ExprKind::Field { base, index } => {
                let owner = base.ty;
                let name = match self.tt.get(owner) {
                    Type::Struct(d) => self.structs[d.0 as usize].fields.get(*index as usize).map(|f| f.name.clone()).unwrap_or_default(),
                    _ => index.to_string(),
                };
                let n = tb.str(&name);
                let o = tb.ty(owner);
                tb.ops.extend([T_FIELD, ty, *index as i128, n, o]);
                self.tree_emit(tb, base);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                tb.ops.extend([T_BINARY, ty, op_code(*op).unwrap_or(0)]);
                self.tree_emit(tb, lhs);
                self.tree_emit(tb, rhs);
            }
            ExprKind::Unary { op, operand } => {
                tb.ops.extend([T_UNARY, ty, if *op == UnOp::Not { 13 } else { 14 }]);
                self.tree_emit(tb, operand);
            }
            ExprKind::IsNil { value, negated } => {
                tb.ops.extend([T_ISNIL, ty, *negated as i128]);
                self.tree_emit(tb, value);
            }
            ExprKind::Builtin { args, .. } => {
                let n = tb.str("len");
                tb.ops.extend([T_CALL, ty, n, args.len() as i128]);
                for a in args {
                    self.tree_emit(tb, a);
                }
            }
            ExprKind::Call { callee, args } => {
                let name = self.tree_call_name(callee).unwrap_or_default();
                let n = tb.str(&name);
                tb.ops.extend([T_CALL, ty, n, args.len() as i128]);
                for a in args {
                    self.tree_emit(tb, a);
                }
            }
            ExprKind::Tuple(xs) => {
                tb.ops.extend([T_TUPLE, ty, xs.len() as i128]);
                for x in xs {
                    self.tree_emit(tb, x);
                }
            }
            ExprKind::StructLit { struct_id, fields } => {
                tb.ops.extend([T_STRUCT, ty, fields.len() as i128]);
                let sid = *struct_id;
                for (i, x) in fields {
                    let name = self.structs[sid as usize].fields.get(*i as usize).map(|f| f.name.clone()).unwrap_or_default();
                    let n = tb.str(&name);
                    tb.ops.extend([*i as i128, n]);
                    self.tree_emit(tb, x);
                }
            }
            _ => {
                for c in children_mut(e) {
                    self.tree_emit(tb, c);
                }
            }
        }
    }
}

fn ret_stmt(v: hir::Expr) -> hir::Stmt {
    let span = v.span;
    hir::Stmt { kind: StmtKind::Return(ReturnValue::Value(v)), span }
}

/// Rewrites the lambda's capture reads in `e` into reads of the creating
/// function (its locals or its own upvalues).
fn outer_reads(e: &mut hir::Expr, outer: &[(hir::CaptureSource, TypeId, String)]) {
    if let ExprKind::Upvalue(i) = e.kind {
        if let Some((src, _, _)) = outer.get(i as usize) {
            e.kind = match src {
                hir::CaptureSource::Local(l) => ExprKind::Local(*l),
                hir::CaptureSource::Upvalue(u) => ExprKind::Upvalue(*u),
            };
        }
        return;
    }
    for c in children_mut(e) {
        outer_reads(c, outer);
    }
}
