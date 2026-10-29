//! Compile-time constant folding (Go-style untyped numeric constants) and
//! lazy typing of globals and constants.

use super::resolve::PathRes;
use super::*;
use crate::hir::{ConstValue, ExprKind};

/// A folded constant. `ty == None` means an untyped numeric constant whose
/// type is chosen by the context (default `int` / `float`).
#[derive(Clone, Debug)]
pub struct Folded {
    pub value: ConstValue,
    pub ty: Option<TypeId>,
}

impl<'a> Sema<'a> {
    /// Flattens `a.b.c` (identifiers joined by field access) into segments.
    pub(crate) fn flatten_path(e: &ast::Expr) -> Option<Vec<&ast::Ident>> {
        match &e.kind {
            ast::ExprKind::Ident(i) => Some(vec![i]),
            ast::ExprKind::Field { base, name } => {
                let mut v = Self::flatten_path(base)?;
                v.push(name);
                Some(v)
            }
            _ => None,
        }
    }

    /// Folds `e` if it is a compile-time constant; never reports errors
    /// about `e` itself.
    pub(crate) fn fold(&mut self, e: &ast::Expr) -> Option<Folded> {
        use ast::ExprKind as K;
        let untyped = |value| Some(Folded { value, ty: None });
        match &e.kind {
            K::Int(n) => untyped(ConstValue::Int(*n as i128)),
            K::Float(f) => untyped(ConstValue::Float(*f)),
            K::Str(s) => Some(Folded { value: ConstValue::Str(s.clone()), ty: Some(self.tt.string) }),
            K::Char(c) => Some(Folded { value: ConstValue::Char(*c), ty: Some(self.tt.char_) }),
            K::Bool(b) => Some(Folded { value: ConstValue::Bool(*b), ty: Some(self.tt.bool_) }),
            K::Paren(x) => self.fold(x),
            K::Unary { op: ast::UnaryOp::Neg, expr } => {
                let f = self.fold(expr)?;
                let value = match f.value {
                    ConstValue::Int(i) => ConstValue::Int(-i),
                    ConstValue::Float(x) => ConstValue::Float(-x),
                    _ => return None,
                };
                if let Some(t) = f.ty {
                    if !self.tt.is_numeric(t) || (self.tt.is_integer(t) && !self.tt.is_signed(t)) {
                        return None;
                    }
                }
                Some(Folded { value, ty: f.ty })
            }
            K::Unary { op: ast::UnaryOp::Not, expr } => match self.fold(expr)? {
                Folded { value: ConstValue::Bool(b), ty } => Some(Folded { value: ConstValue::Bool(!b), ty }),
                _ => None,
            },
            K::Binary { op, left, right } => {
                let a = self.fold(left)?;
                let b = self.fold(right)?;
                self.fold_binary(*op, a, b)
            }
            K::Ident(_) | K::Field { .. } => {
                let segs = Self::flatten_path(e)?;
                if !self.fns.is_empty() {
                    match self.lookup_local(&segs[0].name, false) {
                        Some(Err(c)) if segs.len() == 1 => {
                            self.ide_local_ref(segs[0]);
                            return Some(c);
                        }
                        Some(_) => return None,
                        None => {}
                    }
                }
                let (res, n) = self.resolve_segments(&segs, false)?;
                if n != segs.len() {
                    return None;
                }
                if matches!(res, PathRes::Member(Member::Global(_) | Member::StdConst(_))) {
                    self.report_ambiguous(segs[0]);
                }
                match res {
                    PathRes::Member(Member::Global(g)) if self.globals[g.0 as usize].is_const => {
                        self.ensure_global(g);
                        self.global_folded.get(&g.0).cloned()
                    }
                    PathRes::Member(Member::StdConst(i)) => match stdlib::CONSTS[i].2 {
                        stdlib::StdConst::Float(v) => untyped(ConstValue::Float(v)),
                        stdlib::StdConst::Int(v) => untyped(ConstValue::Int(v)),
                        stdlib::StdConst::Error(_) => None,
                        stdlib::StdConst::Target(t) => Some(Folded { value: ConstValue::Str(t.value()), ty: Some(self.tt.string) }),
                    },
                    PathRes::Variant(en, v) if self.enums[en as usize].variants[v as usize].fields.is_empty() => {
                        let ty = self.enum_ty(en);
                        Some(Folded { value: ConstValue::Enum(en, v), ty: Some(ty) })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Converts an untyped constant to the kind of `t` if possible.
    fn adapt_untyped(&self, v: &ConstValue, t: TypeId) -> Option<ConstValue> {
        match v {
            ConstValue::Int(i) if self.tt.is_integer(t) => Some(ConstValue::Int(*i)),
            ConstValue::Int(i) if self.tt.is_float(t) => Some(ConstValue::Float(*i as f64)),
            ConstValue::Float(f) if self.tt.is_float(t) => Some(ConstValue::Float(*f)),
            ConstValue::Float(f) if self.tt.is_integer(t) && f.fract() == 0.0 => Some(ConstValue::Int(*f as i128)),
            _ => None,
        }
    }

    fn fold_binary(&mut self, op: ast::BinaryOp, a: Folded, b: Folded) -> Option<Folded> {
        use ast::BinaryOp as B;
        let is_shift = matches!(op, B::Shl | B::Shr);
        // Result type and operands in a common representation.
        let (ty, av, bv) = if is_shift {
            (a.ty, a.value, b.value)
        } else {
            match (a.ty, b.ty) {
                (None, None) => match (&a.value, &b.value) {
                    (ConstValue::Float(_), ConstValue::Int(i)) => (None, a.value, ConstValue::Float(*i as f64)),
                    (ConstValue::Int(i), ConstValue::Float(_)) => (None, ConstValue::Float(*i as f64), b.value),
                    _ => (None, a.value, b.value),
                },
                (Some(t), None) => {
                    let bv = self.adapt_untyped(&b.value, t)?;
                    (Some(t), a.value, bv)
                }
                (None, Some(t)) => {
                    let av = self.adapt_untyped(&a.value, t)?;
                    (Some(t), av, b.value)
                }
                (Some(x), Some(y)) if x == y => (Some(x), a.value, b.value),
                _ => return None,
            }
        };
        let boolean = |b: bool| Some(Folded { value: ConstValue::Bool(b), ty: Some(self.tt.bool_) });
        let value = match (av, bv) {
            (ConstValue::Int(x), ConstValue::Int(y)) => match op {
                B::Add => ConstValue::Int(x.checked_add(y)?),
                B::Sub => ConstValue::Int(x.checked_sub(y)?),
                B::Mul => ConstValue::Int(x.checked_mul(y)?),
                B::Div => ConstValue::Int(x.checked_div(y)?),
                B::Rem => ConstValue::Int(x.checked_rem(y)?),
                B::BitAnd => ConstValue::Int(x & y),
                B::BitOr => ConstValue::Int(x | y),
                B::BitXor => ConstValue::Int(x ^ y),
                B::Shl if (0..64).contains(&y) => ConstValue::Int(x.checked_shl(y as u32)?),
                B::Shr if (0..64).contains(&y) => ConstValue::Int(x >> y),
                B::Eq => return boolean(x == y),
                B::Ne => return boolean(x != y),
                B::Lt => return boolean(x < y),
                B::Le => return boolean(x <= y),
                B::Gt => return boolean(x > y),
                B::Ge => return boolean(x >= y),
                _ => return None,
            },
            (ConstValue::Float(x), ConstValue::Float(y)) => match op {
                B::Add => ConstValue::Float(x + y),
                B::Sub => ConstValue::Float(x - y),
                B::Mul => ConstValue::Float(x * y),
                B::Div if y != 0.0 => ConstValue::Float(x / y),
                B::Eq => return boolean(x == y),
                B::Ne => return boolean(x != y),
                B::Lt => return boolean(x < y),
                B::Le => return boolean(x <= y),
                B::Gt => return boolean(x > y),
                B::Ge => return boolean(x >= y),
                _ => return None,
            },
            (ConstValue::Str(x), ConstValue::Str(y)) => match op {
                B::Add => ConstValue::Str(x + &y),
                B::Eq => return boolean(x == y),
                B::Ne => return boolean(x != y),
                B::Lt => return boolean(x < y),
                B::Le => return boolean(x <= y),
                B::Gt => return boolean(x > y),
                B::Ge => return boolean(x >= y),
                _ => return None,
            },
            (ConstValue::Bool(x), ConstValue::Bool(y)) => match op {
                B::And => ConstValue::Bool(x && y),
                B::Or => ConstValue::Bool(x || y),
                B::Eq => return boolean(x == y),
                B::Ne => return boolean(x != y),
                _ => return None,
            },
            (ConstValue::Char(x), ConstValue::Char(y)) => match op {
                B::Eq => return boolean(x == y),
                B::Ne => return boolean(x != y),
                B::Lt => return boolean(x < y),
                B::Le => return boolean(x <= y),
                B::Gt => return boolean(x > y),
                B::Ge => return boolean(x >= y),
                _ => return None,
            },
            _ => return None,
        };
        if let (Some(t), ConstValue::Int(i)) = (ty, &value) {
            if !self.int_fits(*i, t) {
                return None;
            }
        }
        Some(Folded { value, ty })
    }

    /// True if `i` is representable in integer type `t`.
    pub(crate) fn int_fits(&self, i: i128, t: TypeId) -> bool {
        match self.tt.get(t) {
            Type::Int(k) => {
                let bits = k.bits();
                if k.signed() {
                    let lim = 1i128 << (bits - 1);
                    (-lim..lim).contains(&i)
                } else {
                    (0..(1i128 << bits)).contains(&i)
                }
            }
            _ => true,
        }
    }

    /// Materialises a folded constant with the type the context expects.
    pub(crate) fn const_expr(&mut self, f: Folded, expected: Option<TypeId>, span: Span) -> hir::Expr {
        let (value, ty) = match f.ty {
            Some(t) => (f.value, t),
            None => {
                let target = expected.filter(|t| self.tt.is_numeric(*t)).and_then(|t| self.adapt_untyped(&f.value, t).map(|v| (v, t)));
                match target {
                    Some(vt) => vt,
                    None => match f.value {
                        ConstValue::Float(_) => (f.value, self.tt.float),
                        v => (v, self.tt.int),
                    },
                }
            }
        };
        if let ConstValue::Int(i) = value {
            if !self.int_fits(i, ty) {
                self.err(format!("constant {i} overflows {}", self.tn(ty)), span);
            }
        }
        match value {
            ConstValue::Enum(e, v) => mk(ExprKind::EnumLit { enum_id: e, variant: v, args: Vec::new() }, ty, span),
            v => mk(ExprKind::Const(v), ty, span),
        }
    }

    /// Types (and folds or checks the initialiser of) a global, once.
    /// Reports `name` if two imports export it (folding resolves quietly
    /// and would take the first), once per use even if folded repeatedly.
    fn report_ambiguous(&mut self, name: &ast::Ident) {
        let before = self.diags.items.len();
        self.lookup_global(name, true);
        if self.diags.items.len() > before {
            let d = &self.diags.items[before];
            if self.diags.items[..before].iter().any(|x| x.message == d.message && x.span == d.span && x.file == d.file) {
                self.diags.items.truncate(before);
            }
        }
    }

    /// Whether `e` may initialise a top-level `const`: a constant
    /// expression, another constant, `nil`, an error sentinel
    /// (`error(c)`, `errors.New(c)`) or a composite literal of such values.
    /// Any other call is run-time work and belongs in a `var`.
    fn const_init_ok(&mut self, e: &ast::Expr) -> bool {
        use ast::ExprKind as K;
        if self.fold(e).is_some() {
            return true;
        }
        match &e.kind {
            K::Nil => true,
            K::Paren(x) => self.const_init_ok(x),
            K::SliceLit(es) | K::Tuple(es) => es.iter().all(|x| self.const_init_ok(x)),
            K::MapLit { entries, .. } => entries.iter().all(|(k, v)| self.const_init_ok(k) && self.const_init_ok(v)),
            K::StructLit { fields, .. } => fields.iter().all(|f| self.const_init_ok(&f.value)),
            K::CompositeLit { elems, .. } => elems.iter().all(|(k, v)| k.as_ref().is_none_or(|k| self.const_init_ok(k)) && self.const_init_ok(v)),
            K::Ident(_) | K::Field { .. } => {
                let Some(segs) = Self::flatten_path(e) else { return false };
                matches!(self.resolve_segments(&segs, false), Some((PathRes::Member(Member::Global(g)), n)) if n == segs.len() && self.globals[g.0 as usize].is_const)
            }
            K::Call { callee, args } => {
                let name: Option<Vec<&str>> = Self::flatten_path(callee).map(|v| v.iter().map(|i| i.name.as_str()).collect());
                matches!(name.as_deref(), Some(["error"]) | Some(["errors", "New"])) && args.iter().all(|a| self.const_init_ok(a))
            }
            _ => false,
        }
    }

    pub(crate) fn ensure_global(&mut self, g: GlobalId) {
        let gi = g.0 as usize;
        match self.global_state[gi] {
            GState::Done => return,
            GState::InProgress => {
                if !self.cycle_reported.insert(g.0) {
                    return;
                }
                let name = self.globals[gi].name.clone();
                let span = self.globals[gi].span;
                let file = self.globals[gi].file;
                self.in_file(file, |s| s.err(format!("initialization cycle: {name} refers to itself"), span));
                return;
            }
            GState::Pending => {}
        }
        self.global_state[gi] = GState::InProgress;
        let file = self.globals[gi].file;
        let src: &'a ast::Expr;
        let declared: Option<&'a ast::TypeExpr>;
        let is_const;
        match self.global_src[gi] {
            GlobalSrc::Const(c) => {
                src = &c.value;
                declared = c.ty.as_ref();
                is_const = true;
            }
            GlobalSrc::Var(v) => {
                declared = v.ty.as_ref();
                is_const = false;
                match &v.init {
                    Some(e) => src = e,
                    None => {
                        let ty = self.in_file(file, |s| match declared {
                            Some(t) => s.resolve_type(t),
                            None => {
                                s.err("variable needs a type or an initializer", v.span);
                                s.tt.invalid
                            }
                        });
                        self.globals[gi].ty = ty;
                        self.global_state[gi] = GState::Done;
                        self.global_order.push(g);
                        return;
                    }
                }
            }
        }
        self.in_file(file, |s| {
            let decl_ty = declared.map(|t| s.resolve_type(t));
            if is_const {
                if let Some(f) = s.fold(src) {
                    let e = s.const_expr(f.clone(), decl_ty, src.span);
                    let e = match decl_ty {
                        Some(t) => s.coerce(e, t, coerce::Ctx::Init),
                        None => e,
                    };
                    let folded = if decl_ty.is_some() || f.ty.is_some() { Folded { value: f.value.clone(), ty: Some(e.ty) } } else { f };
                    let value = match &e.kind {
                        ExprKind::Const(v) => v.clone(),
                        _ => folded.value.clone(),
                    };
                    let folded = Folded { value: value.clone(), ty: folded.ty };
                    s.global_folded.insert(g.0, folded);
                    s.globals[gi].ty = e.ty;
                    s.globals[gi].value = Some(value);
                    return;
                }
            }
            if is_const && !s.const_init_ok(src) {
                s.err("const initializer is not a constant expression (use var for a value computed at startup)", src.span);
            }
            s.fns.push(FnCtx::new(None, s.tt.void, false, "global initializer".into()));
            if let Some(f) = s.fns.last_mut() {
                f.diag_start = s.diags.items.len();
            }
            let e = s.value(src, decl_ty);
            let e = match decl_ty {
                Some(t) => s.coerce(e, t, coerce::Ctx::Init),
                None => s.default_type(e),
            };
            let ctx = s.fns.pop().unwrap();
            if !ctx.locals.is_empty() {
                s.err("this initializer is too complex for a global (it needs local variables); initialise it in a function", src.span);
            }
            s.globals[gi].ty = e.ty;
            s.globals[gi].init = Some(e);
        });
        self.global_state[gi] = GState::Done;
        self.global_order.push(g);
    }
}
