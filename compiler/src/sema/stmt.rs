//! Statements: declarations, assignments, control flow, `switch`, `defer`,
//! `return`, `try`, and the terminating-statement analysis.

use super::coerce::Ctx;
use super::*;
use crate::hir::{BinOp, Builtin, Callee, ExprKind, Pattern, ReturnValue, Stmt, StmtKind, TryFallback};

impl<'a> Sema<'a> {
    /// A block with its own scope.
    pub(crate) fn block(&mut self, b: &ast::Block) -> hir::Block {
        self.push_scope();
        // A label pending for the enclosing `for`/`switch` is not for the
        // loops of a block in its header (`for x in try f() catch { ... }`).
        let pending = self.fns.last_mut().and_then(|f| f.pending_label.take());
        let stmts = self.stmt_list(&b.stmts);
        if let Some(f) = self.fns.last_mut() {
            f.pending_label = pending;
        }
        self.pop_scope();
        hir::Block { stmts, span: b.span }
    }

    /// Statements in the current scope.
    pub(crate) fn stmt_list(&mut self, stmts: &[ast::Stmt]) -> Vec<Stmt> {
        let mut out = Vec::new();
        let mut bound = Vec::new();
        let mut muted = false;
        for (i, s) in stmts.iter().enumerate() {
            let (n, folds) = (out.len(), self.refl.folds);
            self.stmt(s, &mut out);
            if !muted && super::reflect_ct::maybe_dead_after(s, &out, n, self.refl.abstract_stmt) {
                muted = true;
                self.gen.mute += 1;
            }
            if let ast::StmtKind::Var(v) = &s.kind {
                bound.extend(self.ct_bind_local(v, &stmts[i + 1..], &out, folds));
            }
            // A static `if` / `switch` (lowered to the block of the branch
            // taken) whose branch terminates: the rest of the list is dead
            // in this instantiation and is not checked, like a dropped
            // branch (DESIGN.md "Статическая рефлексия").
            if super::reflect_ct::dead_after(s, &out, n) {
                break;
            }
        }
        if muted {
            self.gen.mute -= 1;
        }
        for k in bound {
            self.refl.ct_consts.remove(&k);
        }
        out
    }

    /// A case body: its own scope, as a block.
    pub(crate) fn case_body(&mut self, stmts: &[ast::Stmt], span: Span) -> hir::Block {
        self.push_scope();
        let stmts = self.stmt_list(stmts);
        self.pop_scope();
        hir::Block { stmts, span }
    }

    fn stmt(&mut self, s: &ast::Stmt, out: &mut Vec<Stmt>) {
        let span = s.span;
        let push = |out: &mut Vec<Stmt>, kind| out.push(Stmt { kind, span });
        match &s.kind {
            ast::StmtKind::Var(v) => self.var_decl(v, out),
            ast::StmtKind::Const(c) => self.local_const(c),
            ast::StmtKind::If(i) => {
                let k = self.if_stmt(i);
                push(out, k);
            }
            ast::StmtKind::For(f) => {
                let k = self.for_stmt(f);
                push(out, k);
            }
            ast::StmtKind::Switch(sw) => {
                let k = self.switch_stmt(sw);
                push(out, k);
            }
            ast::StmtKind::Return(es) => {
                let r = self.return_value(es, span);
                push(out, StmtKind::Return(r));
            }
            ast::StmtKind::Break(Some(name)) => {
                if let Some(l) = self.named_target(name, false) {
                    push(out, StmtKind::Break(l))
                }
            }
            ast::StmtKind::Continue(Some(name)) => {
                if let Some(l) = self.named_target(name, true) {
                    push(out, StmtKind::Continue(l))
                }
            }
            ast::StmtKind::Break(None) => match self.break_target() {
                Some(l) => push(out, StmtKind::Break(l)),
                None => self.err("break outside of a loop or switch", span),
            },
            ast::StmtKind::Continue(None) => match self.continue_target() {
                Some(l) => push(out, StmtKind::Continue(l)),
                None => self.err("continue outside of a loop", span),
            },
            ast::StmtKind::Labeled { label, stmt } => {
                let f = self.fcx();
                if f.declared_labels.contains(&label.name) {
                    self.err(format!("label {} already defined", label.name), label.span);
                } else {
                    f.declared_labels.push(label.name.clone());
                    f.pending_label = Some((label.name.clone(), label.span));
                }
                self.stmt(stmt, out);
                // Not taken: a static `switch` folded to one branch.
                self.fcx().pending_label = None;
            }
            ast::StmtKind::Defer(e) => self.defer_stmt(e, span, out),
            ast::StmtKind::Expr(e) => {
                let k = self.expr_stmt(e);
                push(out, k);
            }
            ast::StmtKind::Assign { target, op, value } => {
                let k = self.assign(target, *op, value, span);
                push(out, k);
            }
            ast::StmtKind::Block(b) => {
                let b = self.block(b);
                push(out, StmtKind::Block(b));
            }
            ast::StmtKind::TryBlock { body, err, handler } => {
                self.fcx().try_depth += 1;
                let body = self.block(body);
                self.fcx().try_depth -= 1;
                self.push_scope();
                let err = if err.name == "_" { None } else { Some(self.declare_local(&err.name, self.tt.error, LocalKind::CatchVar, err.span)) };
                let handler = self.block(handler);
                self.pop_scope();
                push(out, StmtKind::TryBlock { body, err, handler });
            }
            // Already reported by the parser.
            ast::StmtKind::Error => {}
        }
    }

    fn var_decl(&mut self, v: &ast::VarDecl, out: &mut Vec<Stmt>) {
        if let Some(a) = v.attrs.first() {
            self.err("attributes are not allowed on local variables", a.span);
        }
        let declared = v.ty.as_ref().map(|t| self.resolve_type(t));
        if v.names.len() == 1 {
            let init = match (&v.init, declared) {
                (Some(e), Some(t)) => Some(self.expr_to(e, t, Ctx::Init)),
                (Some(e), None) => {
                    let h = self.value(e, None);
                    Some(self.default_type(h))
                }
                (None, Some(t)) => match *self.tt.get(t) {
                    // `var c Config` applies the field defaults.
                    Type::Struct(d) => {
                        let fields = self.struct_defaults(d.0, v.span);
                        (!fields.is_empty()).then(|| mk(ExprKind::StructLit { struct_id: d.0, fields }, t, v.span))
                    }
                    _ => None,
                },
                (None, None) => {
                    self.err("variable needs a type or an initializer", v.span);
                    None
                }
            };
            let ty = declared.or(init.as_ref().map(|e| e.ty)).unwrap_or(self.tt.invalid);
            let name = &v.names[0];
            if name.name == "_" {
                if let Some(e) = init {
                    out.push(Stmt { kind: StmtKind::Discard(e), span: v.span });
                }
                return;
            }
            let local = self.declare_local(&name.name, ty, LocalKind::Var, name.span);
            out.push(Stmt { kind: StmtKind::Let { local, init }, span: v.span });
            return;
        }
        let Some(e) = &v.init else {
            self.err("tuple destructuring needs an initializer", v.span);
            return;
        };
        let init = self.value(e, None);
        let init = self.comma_ok(init, v.names.len(), out);
        let tys = match self.tt.get(init.ty).clone() {
            Type::Tuple(ts) if ts.len() == v.names.len() => ts,
            Type::Invalid => vec![self.tt.invalid; v.names.len()],
            _ => {
                let n = if let Type::Tuple(ts) = self.tt.get(init.ty) { ts.len() } else { 1 };
                self.err(format!("assignment mismatch: {} variables but the expression returns {n} value(s)", v.names.len()), v.span);
                vec![self.tt.invalid; v.names.len()]
            }
        };
        if declared.is_some() {
            self.err("tuple destructuring cannot have an explicit type", v.span);
        }
        let locals = v
            .names
            .iter()
            .zip(tys)
            .map(|(n, t)| if n.name == "_" { None } else { Some(self.declare_local(&n.name, t, LocalKind::Var, n.span)) })
            .collect();
        out.push(Stmt { kind: StmtKind::LetTuple { locals, init }, span: v.span });
    }

    /// A map index destructured into names (DESIGN.md, comma-ok): when the
    /// value type V is a tuple of as many elements as names, the names take
    /// the value apart; two names over any other V are the comma-ok lookup
    /// `(V, bool)` (Go semantics); N + 1 names over an N-tuple V are its
    /// elements plus `ok` (the lookup and the value go through temporaries
    /// pushed to `out`, the result is a flat tuple of them). In a generic
    /// body whose declared V is a type parameter two names are comma-ok in
    /// every instance (decided by the abstract check, `comma_ok_param`).
    fn comma_ok(&mut self, h: hir::Expr, n: usize, out: &mut Vec<Stmt>) -> hir::Expr {
        let elems = match self.tt.get(h.ty) {
            Type::Tuple(ts) => Some(ts.clone()),
            _ => None,
        };
        let site = (self.cur_file, h.span);
        if matches!(self.tt.get(h.ty), Type::Param(..)) && matches!(h.kind, ExprKind::MapGet { .. }) {
            self.gen.comma_ok_param.insert(site);
        }
        let param = n == 2 && self.gen.comma_ok_param.contains(&site);
        let flat = match &elems {
            _ if param => false,
            Some(ts) if ts.len() == n => return h,
            Some(ts) => ts.len() + 1 == n,
            None => false,
        };
        if n != 2 && !flat {
            return h;
        }
        let ExprKind::MapGet { map, key } = h.kind else { return h };
        let (vty, span) = (h.ty, h.span);
        let ty = self.tt.tuple(vec![vty, self.tt.bool_]);
        let lookup = mk(ExprKind::Builtin { op: hir::Builtin::MapLookup, args: vec![*map, *key] }, ty, span);
        let Some(ts) = elems.filter(|_| flat) else { return lookup };
        let (tv, tok) = (self.temp(vty, span), self.temp(self.tt.bool_, span));
        out.push(Stmt { kind: StmtKind::LetTuple { locals: vec![Some(tv), Some(tok)], init: lookup }, span });
        let parts: Vec<LocalId> = ts.iter().map(|t| self.temp(*t, span)).collect();
        out.push(Stmt { kind: StmtKind::LetTuple { locals: parts.iter().map(|l| Some(*l)).collect(), init: mk(ExprKind::Local(tv), vty, span) }, span });
        let mut vals: Vec<hir::Expr> = parts.iter().zip(&ts).map(|(l, t)| mk(ExprKind::Local(*l), *t, span)).collect();
        vals.push(mk(ExprKind::Local(tok), self.tt.bool_, span));
        let mut all = ts;
        all.push(self.tt.bool_);
        let fty = self.tt.tuple(all);
        mk(ExprKind::Tuple(vals), fty, span)
    }

    fn local_const(&mut self, c: &ast::ConstDecl) {
        let declared = c.ty.as_ref().map(|t| self.resolve_type(t));
        let Some(f) = self.fold(&c.value) else {
            self.err("const initializer is not a constant expression", c.value.span);
            self.value(&c.value, None);
            return;
        };
        let folded = match declared {
            Some(t) => {
                let e = self.const_expr(f, Some(t), c.value.span);
                let e = self.coerce(e, t, Ctx::Init);
                match e.kind {
                    ExprKind::Const(v) => Folded { value: v, ty: Some(e.ty) },
                    _ => return,
                }
            }
            None => f,
        };
        if self.fns.last().unwrap().scopes.last().unwrap().iter().any(|(n, _)| *n == c.name.name) {
            self.err(format!("{} redeclared in this block", c.name.name), c.name.span);
        }
        let ty = folded.ty;
        self.fcx().scopes.last_mut().unwrap().push((c.name.name.clone(), ScopeEntry::Const(folded)));
        self.ide_def_local_const(c, ty);
    }

    pub(crate) fn cond(&mut self, e: &ast::Expr, what: &str) -> hir::Expr {
        let c = self.value(e, Some(self.tt.bool_));
        if c.ty != self.tt.bool_ && !self.tt.is_invalid(c.ty) {
            self.err(format!("non-boolean condition in {what}: found {}", self.tn(c.ty)), c.span);
        }
        c
    }

    pub(crate) fn if_stmt(&mut self, i: &ast::IfStmt) -> StmtKind {
        if let Some(k) = self.static_if(i) {
            return k;
        }
        self.folded_if(i)
    }

    fn for_stmt(&mut self, f: &ast::ForStmt) -> StmtKind {
        match &f.kind {
            ast::ForKind::Infinite => {
                let label = self.push_label(LabelKind::Loop);
                let body = self.block(&f.body);
                self.pop_label();
                StmtKind::Loop { label, cond: None, body }
            }
            ast::ForKind::While(c) => {
                let cond = self.cond(c, "for");
                let label = self.push_label(LabelKind::Loop);
                let body = self.block(&f.body);
                self.pop_label();
                StmtKind::Loop { label, cond: Some(cond), body }
            }
            ast::ForKind::In { vars, iter } => self.for_in(vars, iter, &f.body, f.span),
        }
    }

    fn loop_var(&mut self, v: Option<&ast::Ident>, ty: TypeId) -> Option<LocalId> {
        let v = v?;
        if v.name == "_" {
            return None;
        }
        Some(self.declare_local(&v.name, ty, LocalKind::LoopVar, v.span))
    }

    fn for_in(&mut self, vars: &[ast::Ident], iter: &ast::Expr, body: &ast::Block, span: Span) -> StmtKind {
        if let Some(k) = self.fields_for(vars, iter, body, span) {
            return k;
        }
        if let Some(k) = self.desc_for(vars, iter, body, span) {
            return k;
        }
        if vars.len() > 2 || vars.is_empty() {
            self.err("for ... in takes one or two variables", span);
        }
        if let ast::ExprKind::Range(lo, hi) = &expr::strip_parens(iter).kind {
            let (Some(lo), Some(hi)) = (lo, hi) else {
                self.err("a for range needs both bounds (a..b)", iter.span);
                return StmtKind::Block(hir::Block::default());
            };
            let (start, end) = self.operands(lo, hi, None);
            if start.ty != end.ty && !self.tt.is_invalid(start.ty) && !self.tt.is_invalid(end.ty) {
                self.err(format!("range bounds have different types {} and {}", self.tn(start.ty), self.tn(end.ty)), iter.span);
            } else if !self.tt.is_integer(start.ty) && !self.tt.is_invalid(start.ty) {
                self.err(format!("range bounds must be integers, found {}", self.tn(start.ty)), iter.span);
            }
            if vars.len() == 2 {
                self.err("a for over a range takes one variable", span);
            }
            let ty = start.ty;
            self.push_scope();
            let var = self.loop_var(vars.first(), ty);
            let label = self.push_label(LabelKind::Loop);
            let body = self.block(body);
            self.pop_label();
            self.pop_scope();
            return StmtKind::ForRange { label, var, start, end, body };
        }
        let seq = self.value(iter, None);
        let seq = match self.collection_for(vars, seq, body) {
            Ok(k) => return k,
            Err(seq) => seq,
        };
        // Iter() / Next() / Items() (iterate.rs).
        let seq = match self.protocol_for(vars, seq, body, span) {
            Ok(k) => return k,
            Err(seq) => seq,
        };
        let (first_ty, second_ty) = match self.tt.get(seq.ty).clone() {
            Type::Slice(t) | Type::Array(t, _) => {
                if vars.len() == 1 {
                    (t, None)
                } else {
                    (self.tt.int, Some(t))
                }
            }
            Type::String => {
                if vars.len() == 1 {
                    (self.tt.char_, None)
                } else {
                    (self.tt.int, Some(self.tt.char_))
                }
            }
            Type::Map(k, v) => (k, Some(v)),
            Type::Invalid => (self.tt.invalid, Some(self.tt.invalid)),
            _ => {
                self.err_help(
                    format!("cannot iterate over value of type {}", self.tn(seq.ty)),
                    seq.span,
                    "for ... in works on ranges, slices, arrays, strings, maps, collections and types with `Iter()`, `Next()` or `Items()` (the iteration protocol)",
                );
                (self.tt.invalid, Some(self.tt.invalid))
            }
        };
        self.push_scope();
        let a = self.loop_var(vars.first(), first_ty);
        let b = match second_ty {
            Some(t) => self.loop_var(vars.get(1), t),
            None => None,
        };
        let label = self.push_label(LabelKind::Loop);
        let hbody = self.block(body);
        self.pop_label();
        self.pop_scope();
        match self.tt.get(seq.ty) {
            Type::String => {
                let (index, ch) = if vars.len() == 1 { (None, a) } else { (a, b) };
                StmtKind::ForString { label, index, ch, string: seq, body: hbody }
            }
            Type::Map(..) => StmtKind::ForMap { label, key: a, value: b, map: seq, body: hbody },
            _ => {
                let (index, elem) = if vars.len() == 1 { (None, a) } else { (a, b) };
                StmtKind::ForSlice { label, index, elem, seq, body: hbody }
            }
        }
    }

    // -- switch -----------------------------------------------------------

    fn switch_stmt(&mut self, sw: &ast::SwitchStmt) -> StmtKind {
        let folds = self.refl.folds;
        let subject = sw.subject.as_ref().map(|s| {
            let v = self.value(s, None);
            let v = self.default_type(v);
            let l = self.temp(v.ty, s.span);
            (l, v)
        });
        if let Some((l, v)) = &subject {
            if let Some(k) = self.static_switch(sw, *l, v, self.refl.folds > folds) {
                return k;
            }
        }
        let subj = subject.as_ref().map(|(l, v)| (*l, v.ty, v.span));
        let label = self.push_label(LabelKind::Switch);
        let mut cases = Vec::new();
        let mut seen: Vec<hir::ConstValue> = Vec::new();
        for c in &sw.cases {
            self.push_scope();
            let multi = c.patterns.len() > 1;
            let patterns: Vec<Pattern> = c.patterns.iter().map(|p| self.pattern(p, subj, multi)).collect();
            for (p, ap) in patterns.iter().zip(&c.patterns) {
                // Duplicate constant cases are errors (Go).
                let key = match p {
                    Pattern::Cond(hir::Expr { kind: ExprKind::Binary { op: BinOp::Eq | BinOp::StrEq, lhs, rhs }, .. }) => match (&lhs.kind, &rhs.kind) {
                        (ExprKind::Local(l), ExprKind::Const(v)) if subj.is_some_and(|s| s.0 == *l) => Some(v.clone()),
                        _ => None,
                    },
                    Pattern::Variant { variant, bindings } if bindings.is_empty() => Some(hir::ConstValue::Enum(u32::MAX, *variant)),
                    _ => None,
                };
                if let Some(k) = key {
                    if seen.contains(&k) {
                        self.err("duplicate case in switch: this value is already matched by an earlier case", ap.span);
                    } else {
                        seen.push(k);
                    }
                }
            }
            let body = self.case_body(&c.body, c.span);
            self.pop_scope();
            cases.push(hir::Case { patterns, body, span: c.span });
        }
        let mut default = sw.default.as_ref().map(|d| self.case_body(d, sw.span));
        if let (None, Some((_, v))) = (&default, &subject) {
            default = self.exhaustive_default(v.ty, &cases, sw.span);
        }
        self.pop_label();
        StmtKind::Switch(hir::Switch { label, subject, cases, default, span: sw.span })
    }

    fn subject_expr(&self, l: LocalId, ty: TypeId, span: Span) -> hir::Expr {
        mk(ExprKind::Local(l), ty, span)
    }

    fn pattern_mismatch(&mut self, found: TypeId, subject: TypeId, span: Span) {
        if !self.tt.is_invalid(found) && !self.tt.is_invalid(subject) {
            let (f, s) = (self.tn(found), self.tn(subject));
            self.err(format!("switch pattern type mismatch: cannot match {f} against a subject of type {s}"), span);
        }
    }

    fn pattern(&mut self, p: &ast::Pattern, subj: Option<(LocalId, TypeId, Span)>, multi: bool) -> Pattern {
        let Some((l, t, sspan)) = subj else {
            return match &p.kind {
                ast::PatternKind::Expr(e) => Pattern::Cond(self.cond(e, "case")),
                _ => {
                    self.err("this pattern needs a switch subject", p.span);
                    Pattern::Cond(self.invalid(p.span))
                }
            };
        };
        match &p.kind {
            ast::PatternKind::TypeBind { ty, name } => {
                let pt = self.resolve_type(ty);
                if !self.tt.is_interface(t) {
                    if !self.tt.is_invalid(t) {
                        self.err(format!("type pattern {} needs an interface or error subject, found {}", self.tn(pt), self.tn(t)), p.span);
                    }
                } else if !self.tt.is_interface(pt) && !self.implements(pt, t) && !self.tt.is_invalid(pt) {
                    let problems = self.missing_methods(pt, t);
                    self.err(format!("impossible type switch case: {} does not implement {} ({})", self.tn(pt), self.tn(t), problems.join(", ")), p.span);
                }
                if multi && name.name != "_" {
                    self.err("cannot bind a variable in a case with several patterns", name.span);
                }
                let binding = if name.name == "_" { None } else { Some(self.declare_local(&name.name, pt, LocalKind::PatternBinding, name.span)) };
                Pattern::Type { ty: pt, binding }
            }
            ast::PatternKind::Range(lo, hi) => {
                if !self.tt.is_numeric(t) && !matches!(self.tt.get(t), Type::Char | Type::Invalid) {
                    self.err(format!("range pattern needs a numeric or char subject, found {}", self.tn(t)), p.span);
                }
                let mut conds = Vec::new();
                for (bound, op) in [(lo, ast::BinaryOp::Ge), (hi, ast::BinaryOp::Lt)] {
                    if let Some(b) = bound {
                        let v = self.value(b, Some(t));
                        if v.ty != t {
                            self.pattern_mismatch(v.ty, t, v.span);
                            continue;
                        }
                        let s = self.subject_expr(l, t, sspan);
                        conds.push(self.make_binary(op, s, v, p.span));
                    }
                }
                let cond = conds.into_iter().reduce(|a, b| mk(ExprKind::Binary { op: BinOp::And, lhs: Box::new(a), rhs: Box::new(b) }, self.tt.bool_, p.span));
                Pattern::Cond(cond.unwrap_or_else(|| mk(ExprKind::Const(hir::ConstValue::Bool(true)), self.tt.bool_, p.span)))
            }
            ast::PatternKind::Expr(e) => {
                if let Type::Enum(d) = *self.tt.get(t) {
                    if let Some(pat) = self.variant_pattern(e, d.0, multi) {
                        return pat;
                    }
                }
                if self.tt.is_interface(t) {
                    if let Some(segs) = Self::flatten_path(e) {
                        if self.lookup_local(&segs[0].name, false).is_none() {
                            if let Some((res, n)) = self.resolve_segments(&segs, false) {
                                if let (true, Some(pt)) = (n == segs.len(), self.path_res_type(res)) {
                                    if !self.implements(pt, t) {
                                        self.pattern_mismatch(pt, t, e.span);
                                    }
                                    return Pattern::Type { ty: pt, binding: None };
                                }
                            }
                        }
                    }
                }
                let v = self.value(e, Some(t));
                if v.ty == self.tt.bool_ && t != self.tt.bool_ {
                    return Pattern::Cond(v);
                }
                if v.ty != t && !matches!(self.tt.get(v.ty), Type::Nil) {
                    self.pattern_mismatch(v.ty, t, v.span);
                    return Pattern::Cond(self.invalid(p.span));
                }
                let s = self.subject_expr(l, t, sspan);
                Pattern::Cond(self.make_binary(ast::BinaryOp::Eq, s, v, p.span))
            }
        }
    }

    /// `E.V` or `E.V(a, b)` against an enum subject of enum `eid`.
    fn variant_pattern(&mut self, e: &ast::Expr, eid: u32, multi: bool) -> Option<Pattern> {
        let (callee, args) = match &e.kind {
            ast::ExprKind::Call { callee, args } => (&**callee, Some(args)),
            _ => (e, None),
        };
        let (en, v) = self.pattern_variant(callee, eid)?;
        if en != eid {
            let found = self.enum_ty(en);
            let subject = self.enum_ty(eid);
            self.pattern_mismatch(found, subject, e.span);
            return Some(Pattern::Variant { variant: v, bindings: Vec::new() });
        }
        let fields = self.enums[eid as usize].variants[v as usize].fields.clone();
        let mut bindings = Vec::new();
        if let Some(args) = args {
            if args.len() != fields.len() {
                let vname = &self.enums[eid as usize].variants[v as usize].name;
                let msg = format!("variant {vname} has {} field(s), but the pattern binds {}", fields.len(), args.len());
                self.err(msg, e.span);
            }
            for (a, (_, fty)) in args.iter().zip(&fields) {
                match &a.kind {
                    ast::ExprKind::Ident(id) if id.name == "_" => bindings.push(None),
                    ast::ExprKind::Ident(id) => {
                        if multi {
                            self.err("cannot bind a variable in a case with several patterns", id.span);
                        }
                        bindings.push(Some(self.declare_local(&id.name, *fty, LocalKind::PatternBinding, id.span)));
                    }
                    _ => {
                        self.err("variant pattern arguments must be names to bind", a.span);
                        bindings.push(None);
                    }
                }
            }
        }
        Some(Pattern::Variant { variant: v, bindings })
    }

    // -- return / try ---------------------------------------------------------

    /// Checks `return es` against the current function's result type.
    pub(crate) fn return_value(&mut self, es: &[ast::Expr], span: Span) -> ReturnValue {
        let ret = self.fns.last().unwrap().ret;
        if self.tt.is_invalid(ret) {
            for e in es {
                self.expr(e, None);
            }
            return ReturnValue::Void;
        }
        let (inner, fallible) = match self.tt.fallible_inner(ret) {
            Some(t) => (t, true),
            None => (ret, false),
        };
        let void = matches!(self.tt.get(inner), Type::Void);
        if es.is_empty() {
            if !void {
                self.err(format!("missing return value: the function returns {}", self.tn(ret)), span);
            }
            return ReturnValue::Void;
        }
        if es.len() > 1 {
            let Type::Tuple(ts) = self.tt.get(inner).clone() else {
                self.err(format!("too many return values: the function returns {}", self.tn(ret)), span);
                return ReturnValue::Void;
            };
            if ts.len() != es.len() {
                self.err(format!("wrong number of return values: want {}, got {}", ts.len(), es.len()), span);
                return ReturnValue::Void;
            }
            let vs: Vec<hir::Expr> = es.iter().zip(&ts).map(|(e, t)| self.expr_to(e, *t, Ctx::Return)).collect();
            return ReturnValue::Value(mk(ExprKind::Tuple(vs), inner, span));
        }
        if void && !fallible {
            self.err("too many return values: the function has no result", span);
            self.expr(&es[0], None);
            return ReturnValue::Void;
        }
        let hint = if void { None } else { Some(inner) };
        let v = self.value(&es[0], hint);
        if !fallible {
            return ReturnValue::Value(self.coerce(v, inner, Ctx::Return));
        }
        if !void && self.assignable(v.ty, inner) {
            return ReturnValue::Value(self.coerce(v, inner, Ctx::Return));
        }
        if v.ty == self.tt.error || self.implements(v.ty, self.tt.error) {
            return ReturnValue::Error(self.coerce(v, self.tt.error, Ctx::Return));
        }
        if void {
            self.err(format!("a void! function can only return an error, found {}", self.tn(v.ty)), v.span);
            return ReturnValue::Void;
        }
        ReturnValue::Value(self.coerce(v, inner, Ctx::Return))
    }

    /// True if `src` converts implicitly to `target` (no diagnostics).
    pub(crate) fn assignable(&self, src: TypeId, target: TypeId) -> bool {
        src == target
            || self.tt.is_invalid(src)
            || (matches!(self.tt.get(src), Type::Nil) && self.tt.is_nilable(target))
            || (self.tt.is_interface(target) && self.implements(src, target))
    }

    /// `try e catch err {...}` / `try e else ...`. `used`: whether the
    /// result value is used (false for a statement).
    pub(crate) fn try_expr(&mut self, e: &ast::Expr, used: bool) -> hir::Expr {
        let ast::ExprKind::Try { expr, handler } = &e.kind else { unreachable!() };
        let span = e.span;
        let v = self.expr(expr, None);
        let inner = match self.tt.fallible_inner(v.ty) {
            Some(t) => t,
            None => {
                if !self.tt.is_invalid(v.ty) {
                    self.err_help(format!("try requires a fallible (T!) expression, found {}", self.tn(v.ty)), v.span, "remove `try`: this expression cannot fail");
                }
                self.tt.invalid
            }
        };
        match handler {
            ast::TryHandler::Catch { name, body } => {
                self.push_scope();
                let err = if name.name == "_" { None } else { Some(self.declare_local(&name.name, self.tt.error, LocalKind::CatchVar, name.span)) };
                let handler = self.block(body);
                self.pop_scope();
                if used && !terminates(&handler.stmts, true) {
                    self.err(
                        "catch block must return (or break, continue, panic) when the value of try is used; otherwise the variable would be uninitialized",
                        body.span,
                    );
                }
                let ty = if used { inner } else { self.tt.void };
                mk(ExprKind::TryCatch { value: Box::new(v), err, handler, result_used: used }, ty, span)
            }
            ast::TryHandler::Else(ev) => {
                let fallback = match &**ev {
                    ast::ElseValue::Expr(x) => {
                        if matches!(self.tt.get(inner), Type::Void) {
                            self.err("try ... else value needs a function with a result; use catch for void!", x.span);
                            TryFallback::Value(self.expr(x, None))
                        } else {
                            TryFallback::Value(self.expr_to(x, inner, Ctx::Other))
                        }
                    }
                    ast::ElseValue::Continue(sp) => match self.continue_target() {
                        Some(l) => TryFallback::Continue(l),
                        None => {
                            self.err("continue outside of a loop", *sp);
                            TryFallback::Return(ReturnValue::Void)
                        }
                    },
                    ast::ElseValue::Break(sp) => match self.break_target() {
                        Some(l) => TryFallback::Break(l),
                        None => {
                            self.err("break outside of a loop or switch", *sp);
                            TryFallback::Return(ReturnValue::Void)
                        }
                    },
                    ast::ElseValue::Return(es, sp) => TryFallback::Return(self.return_value(es, *sp)),
                };
                mk(ExprKind::TryElse { value: Box::new(v), fallback: Box::new(fallback) }, inner, span)
            }
        }
    }

    fn expr_stmt(&mut self, e: &ast::Expr) -> StmtKind {
        let inner = expr::strip_parens(e);
        if let ast::ExprKind::Try { .. } = inner.kind {
            return StmtKind::Expr(self.try_expr(inner, false));
        }
        if let Some(k) = self.ct_field_set(inner) {
            return k;
        }
        if let Some(k) = self.ct_param_set(inner) {
            return k;
        }
        let h = self.expr(e, None);
        if self.tt.is_fallible(h.ty) {
            self.unhandled_result(h.ty, h.span);
        } else if !matches!(inner.kind, ast::ExprKind::Call { .. } | ast::ExprKind::Await(_) | ast::ExprKind::Propagate { .. }) && !inner.is_recovered() {
            self.err_help("expression is evaluated but not used", e.span, "assign the value to a variable, or discard it explicitly with `_ = ...`");
        }
        StmtKind::Expr(h)
    }

    fn assign(&mut self, target: &ast::Expr, op: ast::AssignOp, value: &ast::Expr, span: Span) -> StmtKind {
        if let ast::ExprKind::Tuple(targets) = &expr::strip_parens(target).kind {
            return self.tuple_assign(targets, value, span);
        }
        if let ast::ExprKind::Ident(id) = &target.kind {
            if id.name == "_" {
                if op != ast::AssignOp::Assign {
                    self.err("cannot use _ as a value", target.span);
                }
                let v = self.expr(value, None);
                if matches!(self.tt.get(v.ty), Type::Void) {
                    self.err("expression of type void used as a value", v.span);
                }
                return StmtKind::Discard(v);
            }
            if self.lookup_local(&id.name, false).is_none() && self.lookup_global(id, false).is_none() {
                self.err_help(format!("cannot assign to undeclared variable {}", id.name), id.span, format!("declare it with `var {} = ...`", id.name));
                self.expr(value, None);
                return StmtKind::Block(hir::Block::default());
            }
        }
        // `b[i] = v` / `b[i] op= v` on a struct: IndexSet (opover.rs).
        let mut indexed = None;
        if let ast::ExprKind::Index { base, index } = &expr::strip_parens(target).kind {
            let b = self.value(base, None);
            if self.index_overloaded(b.ty, "IndexSet") {
                let bop = (op != ast::AssignOp::Assign).then(|| compound_binop(op));
                return self.op_index_assign(b, index, bop, value, span);
            }
            indexed = Some((b, &**index));
        }
        let t = match &target.kind {
            _ if indexed.is_some() => {
                let (b, index) = indexed.take().unwrap();
                let h = self.index_on(b, index, target.span);
                self.require_value_raw(h)
            }
            ast::ExprKind::Ident(id) => match self.lookup_local(&id.name, false) {
                Some(Ok(v)) => {
                    self.ide_local_ref(id);
                    self.var_expr(v, target.span)
                }
                _ => self.place_target(target),
            },
            _ => self.place_target(target),
        };
        if self.tt.is_invalid(t.ty) {
            self.expr(value, None);
            return StmtKind::Block(hir::Block::default());
        }
        if !self.is_place(&t, true) {
            let msg = match &t.kind {
                ExprKind::Global(_) | ExprKind::Const(_) => "cannot assign to a constant".to_string(),
                _ => "cannot assign to this expression".to_string(),
            };
            self.err(msg, target.span);
        }
        if op == ast::AssignOp::Assign {
            let v = self.expr_to(value, t.ty, Ctx::Assign);
            return StmtKind::Assign { target: t, value: v };
        }
        let bop = compound_binop(op);
        if self.binary_overloaded(bop, t.ty, Some(value)) {
            return self.op_compound_assign(bop, t, value, span);
        }
        let v = self.value(value, Some(t.ty));
        // `c += n` / `c -= n` on a char: `c = c ± n` (char arithmetic).
        if matches!(self.tt.get(t.ty), Type::Char) && self.tt.is_integer(v.ty) && matches!(bop, ast::BinaryOp::Add | ast::BinaryOp::Sub) {
            if !super::opover::simple_place(&t) {
                self.err("char += and -= need a variable or field target", target.span);
                return StmtKind::Block(hir::Block::default());
            }
            if let Some(e) = self.char_arith(bop, &t, &v, span) {
                return StmtKind::Assign { target: t, value: e };
            }
        }
        match self.binop_for(bop, t.ty, v.ty, span) {
            Some((hop, _)) => StmtKind::CompoundAssign { target: t, op: hop, value: v },
            None => StmtKind::Block(hir::Block::default()),
        }
    }
}

/// The binary operator of a compound assignment `op=`.
fn compound_binop(op: ast::AssignOp) -> ast::BinaryOp {
        match op {
            ast::AssignOp::Assign => unreachable!("plain assignment"),
            ast::AssignOp::Add => ast::BinaryOp::Add,
            ast::AssignOp::Sub => ast::BinaryOp::Sub,
            ast::AssignOp::Mul => ast::BinaryOp::Mul,
            ast::AssignOp::Div => ast::BinaryOp::Div,
            ast::AssignOp::Rem => ast::BinaryOp::Rem,
            ast::AssignOp::MatMul => ast::BinaryOp::MatMul,
            ast::AssignOp::BitAnd => ast::BinaryOp::BitAnd,
            ast::AssignOp::BitOr => ast::BinaryOp::BitOr,
            ast::AssignOp::BitXor => ast::BinaryOp::BitXor,
            ast::AssignOp::Shl => ast::BinaryOp::Shl,
            ast::AssignOp::Shr => ast::BinaryOp::Shr,
        }
}

impl<'a> Sema<'a> {

    /// `a, b = x, y` (parallel: every value is evaluated into a temporary
    /// first) and `(a, b) = f()` (tuple result). `_` discards. Lowered to a
    /// block of temporaries followed by plain assignments.
    fn tuple_assign(&mut self, targets: &[ast::Expr], value: &ast::Expr, span: Span) -> StmtKind {
        let mut places: Vec<Option<hir::Expr>> = Vec::new();
        for t in targets {
            if matches!(&t.kind, ast::ExprKind::Ident(id) if id.name == "_") {
                places.push(None);
                continue;
            }
            let h = match &t.kind {
                ast::ExprKind::Ident(id) => match self.lookup_local(&id.name, false) {
                    Some(Ok(v)) => {
                        self.ide_local_ref(id);
                        self.var_expr(v, t.span)
                    }
                    _ if self.lookup_global(id, false).is_none() => {
                        self.err_help(format!("cannot assign to undeclared variable {}", id.name), id.span, format!("declare it with `var {} = ...`", id.name));
                        self.invalid(t.span)
                    }
                    _ => self.place_target(t),
                },
                _ => self.place_target(t),
            };
            if !self.tt.is_invalid(h.ty) && !self.is_place(&h, true) {
                self.err("cannot assign to this expression", t.span);
            }
            places.push(Some(h));
        }
        self.push_scope();
        let mut stmts = Vec::new();
        let mut temps: Vec<Option<(LocalId, TypeId)>> = Vec::new();
        match &expr::strip_parens(value).kind {
            ast::ExprKind::Tuple(vals) => {
                if vals.len() != places.len() {
                    self.err(format!("assignment mismatch: {} variables but {} values", places.len(), vals.len()), span);
                }
                for (v, p) in vals.iter().zip(&places) {
                    let hv = match p {
                        Some(p) if !self.tt.is_invalid(p.ty) => self.expr_to(v, p.ty, Ctx::Assign),
                        _ => {
                            let h = self.value(v, None);
                            self.default_type(h)
                        }
                    };
                    let (ty, vspan) = (hv.ty, hv.span);
                    let l = self.temp(ty, vspan);
                    stmts.push(Stmt { kind: StmtKind::Let { local: l, init: Some(hv) }, span: vspan });
                    temps.push(p.as_ref().map(|_| (l, ty)));
                }
            }
            _ => {
                let hv = self.value(value, None);
                let hv = self.comma_ok(hv, places.len(), &mut stmts);
                let tys = match self.tt.get(hv.ty).clone() {
                    Type::Tuple(ts) if ts.len() == places.len() => ts,
                    Type::Invalid => vec![self.tt.invalid; places.len()],
                    _ => {
                        let n = if let Type::Tuple(ts) = self.tt.get(hv.ty) { ts.len() } else { 1 };
                        self.err(format!("assignment mismatch: {} variables but the expression returns {n} value(s)", places.len()), span);
                        vec![self.tt.invalid; places.len()]
                    }
                };
                let locals: Vec<Option<LocalId>> = places.iter().zip(&tys).map(|(p, t)| p.as_ref().map(|_| self.temp(*t, value.span))).collect();
                temps = locals.iter().zip(&tys).map(|(l, t)| l.map(|l| (l, *t))).collect();
                stmts.push(Stmt { kind: StmtKind::LetTuple { locals, init: hv }, span: value.span });
            }
        }
        for (p, t) in places.into_iter().zip(temps) {
            let (Some(p), Some((l, ty))) = (p, t) else { continue };
            if self.tt.is_invalid(p.ty) || self.tt.is_invalid(ty) {
                continue;
            }
            let src = mk(ExprKind::Local(l), ty, p.span);
            let v = self.coerce(src, p.ty, Ctx::Assign);
            let sp = p.span;
            stmts.push(Stmt { kind: StmtKind::Assign { target: p, value: v }, span: sp });
        }
        self.pop_scope();
        StmtKind::Block(hir::Block { stmts, span })
    }

    /// An assignment target (a `weak *T` place stays a place).
    fn place_target(&mut self, target: &ast::Expr) -> hir::Expr {
        let h = self.expr(target, None);
        self.require_value_raw(h)
    }

    // -- defer --------------------------------------------------------------

    /// `defer call`: arguments are evaluated now (spilled into temporaries),
    /// the call runs at function exit: open-coded (the call itself, reading
    /// `DeferArg` temporaries) when [`open_defers_ok`] holds for the body,
    /// else in a synthesized thunk closure capturing the temporaries.
    fn defer_stmt(&mut self, e: &ast::Expr, span: Span, out: &mut Vec<Stmt>) {
        let ast::ExprKind::Call { callee, args } = &expr::strip_parens(e).kind else {
            self.err("defer requires a function call", e.span);
            self.expr(e, None);
            return;
        };
        let open = self.fcx().open_defers;
        if args.is_empty() {
            if let ast::ExprKind::Lambda { .. } = expr::strip_parens(callee).kind {
                let c = self.value(callee, None);
                if let Type::Func(sig) = self.tt.get(c.ty).clone() {
                    if !sig.params.is_empty() {
                        self.err("wrong number of arguments in deferred call", span);
                    }
                }
                if open && !self.tt.is_invalid(c.ty) {
                    // Open-coded: the closure value waits in a frame slot
                    // and is called at exit.
                    let (ty, cspan) = (c.ty, c.span);
                    let mut caps = Vec::new();
                    let f = self.spill(c, &mut caps, out, true);
                    let ret = match self.tt.get(ty) {
                        Type::Func(sig) => sig.ret,
                        _ => self.tt.void,
                    };
                    let call = mk(ExprKind::Call { callee: Callee::Value(Box::new(f)), args: Vec::new() }, ret, cspan);
                    out.push(Stmt { kind: StmtKind::Defer(call), span });
                    return;
                }
                out.push(Stmt { kind: StmtKind::Defer(c), span });
                return;
            }
        }
        let call = self.expr(e, None);
        if self.tt.is_invalid(call.ty) {
            return;
        }
        if self.tt.is_fallible(call.ty) {
            let msg = format!("result of type {} of a deferred call must be handled: defer a closure that handles the error (defer func() {{ try f() catch err {{ ... }} }}())", self.tn(call.ty));
            self.err(msg, call.span);
            return;
        }
        let mut caps: Vec<(LocalId, TypeId)> = Vec::new();
        let ty = call.ty;
        let cspan = call.span;
        let kind = match call.kind {
            ExprKind::Call { callee, args } => {
                let callee = match callee {
                    Callee::Value(v) => Callee::Value(Box::new(self.spill(*v, &mut caps, out, open))),
                    Callee::Interface { recv, method } => Callee::Interface { recv: Box::new(self.spill(*recv, &mut caps, out, open)), method },
                    c => c,
                };
                let args = args.into_iter().map(|a| self.spill(a, &mut caps, out, open)).collect();
                ExprKind::Call { callee, args }
            }
            ExprKind::Print(items) => ExprKind::Print(
                items
                    .into_iter()
                    .map(|mut i| {
                        i.arg = i.arg.map(|a| self.spill(a, &mut caps, out, open));
                        i
                    })
                    .collect(),
            ),
            ExprKind::Builtin { op, args } => ExprKind::Builtin { op, args: args.into_iter().map(|a| self.spill(a, &mut caps, out, open)).collect() },
            _ => {
                self.err("defer requires a function call", e.span);
                return;
            }
        };
        let body_expr = mk(kind, ty, cspan);
        if open {
            // Open-coded: the call itself, reading the `DeferArg` temporaries.
            out.push(Stmt { kind: StmtKind::Defer(body_expr), span });
            return;
        }
        let stmt = StmtKind::Expr(body_expr);
        let parent = self.fns.last().and_then(|f| f.func).expect("defer outside a function");
        let fid = FuncId(self.funcs.len() as u32);
        self.lambda_count += 1;
        let name = format!("defer{}", self.lambda_count);
        let mut f = self.dummy_function(name.clone(), hir::FuncKind::Lambda { parent }, self.funcs[parent.0 as usize].namespace);
        f.full_name = format!("{}${name}", self.funcs[parent.0 as usize].full_name);
        f.symbol = format!("zell_{}", f.full_name.replace('.', "__").replace('$', "_"));
        f.span = span;
        f.captures = caps.iter().map(|(l, t)| hir::Capture { name: "$tmp".into(), ty: *t, source: hir::CaptureSource::Local(*l) }).collect();
        f.body = hir::Block { stmts: vec![Stmt { kind: stmt, span }], span };
        self.funcs.push(f);
        self.func_src.push(FuncSrc::Synthetic);
        self.sigs.push(Sig { params: Vec::new(), ret: self.tt.void, is_async: false });
        for (l, _) in &caps {
            self.fcx().locals[l.0 as usize].captured = true;
        }
        let captures = caps.iter().map(|(l, _)| hir::CaptureSource::Local(*l)).collect();
        let fty = self.tt.func(Vec::new(), self.tt.void);
        out.push(Stmt { kind: StmtKind::Defer(mk(ExprKind::Closure { func: fid, captures }, fty, span)), span });
    }

    /// Stores a deferred argument in a temp; returns its upvalue reference
    /// for the thunk body, or, for an open-coded `defer`, the
    /// function-lifetime `DeferArg` temporary itself (constants stay inline).
    fn spill(&mut self, e: hir::Expr, caps: &mut Vec<(LocalId, TypeId)>, out: &mut Vec<Stmt>, open: bool) -> hir::Expr {
        if matches!(e.kind, ExprKind::Const(_) | ExprKind::FuncRef(_) | ExprKind::Nil | ExprKind::Zero | ExprKind::AttrConst { .. }) {
            return e;
        }
        if let ExprKind::AddrOf(inner) = &e.kind {
            if field_path_through_deref(inner) {
                // `&p.f.g` (e.g. the receiver `&this.mu` of a method on a
                // value field): the address has no object of its own, so
                // keep the pointer `p` and re-take the field address at
                // call time; `p` pins the object, as Go's evaluated
                // receiver would.
                let ExprKind::AddrOf(inner) = e.kind else { unreachable!() };
                let inner = self.spill_field_path(*inner, caps, out, open);
                return mk(ExprKind::AddrOf(Box::new(inner)), e.ty, e.span);
            }
        }
        let (ty, span) = (e.ty, e.span);
        if open {
            let l = self.declare_local("$defer", ty, LocalKind::DeferArg, span);
            out.push(Stmt { kind: StmtKind::Let { local: l, init: Some(e) }, span });
            return mk(ExprKind::Local(l), ty, span);
        }
        let l = self.temp(ty, span);
        out.push(Stmt { kind: StmtKind::Let { local: l, init: Some(e) }, span });
        caps.push((l, ty));
        mk(ExprKind::Upvalue((caps.len() - 1) as u32), ty, span)
    }
}

/// `p.f.g` as a place: a chain of struct fields ending in `*p` where `p`
/// is a pointer with an object of its own (not itself interior).
fn field_path_through_deref(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Field { base, .. } => matches!(base.kind, ExprKind::Field { .. } | ExprKind::Deref(_)) && field_path_through_deref(base),
        ExprKind::Deref(p) => !crate::hir::ownership::is_interior_addr(p),
        _ => false,
    }
}

impl<'a> Sema<'a> {
    /// Rebuilds a [`field_path_through_deref`] place with its pointer
    /// spilled (see [`spill`](Self::spill)).
    fn spill_field_path(&mut self, e: hir::Expr, caps: &mut Vec<(LocalId, TypeId)>, out: &mut Vec<Stmt>, open: bool) -> hir::Expr {
        let (ty, span) = (e.ty, e.span);
        match e.kind {
            ExprKind::Field { base, index } => {
                let base = self.spill_field_path(*base, caps, out, open);
                mk(ExprKind::Field { base: Box::new(base), index }, ty, span)
            }
            ExprKind::Deref(p) => {
                let p = self.spill(*p, caps, out, open);
                mk(ExprKind::Deref(Box::new(p)), ty, span)
            }
            _ => unreachable!("not a field path"),
        }
    }
}

/// Whether the `defer` statements of a body (not counting those of nested
/// lambdas, which have their own bodies) can be open-coded: none is inside
/// a loop and there are at most [`hir::MAX_OPEN_DEFERS`] of them. Off for
/// the whole build with `ZELL_NO_OPEN_DEFER=1`.
pub(crate) fn open_defers_ok(stmts: &[ast::Stmt]) -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("ZELL_NO_OPEN_DEFER").is_some()) {
        return false;
    }
    // `n` counts the defers seen so far; false once one is in a loop or
    // the count exceeds the limit.
    fn count(stmts: &[ast::Stmt], in_loop: bool, n: &mut usize) -> bool {
        stmts.iter().all(|s| match &s.kind {
            ast::StmtKind::Defer(_) => {
                *n += 1;
                !in_loop && *n <= hir::MAX_OPEN_DEFERS
            }
            ast::StmtKind::For(f) => count(&f.body.stmts, true, n),
            ast::StmtKind::Labeled { stmt, .. } => count(std::slice::from_ref(&**stmt), in_loop, n),
            ast::StmtKind::If(i) => if_ok(i, in_loop, n),
            ast::StmtKind::Switch(sw) => sw.cases.iter().all(|c| count(&c.body, in_loop, n)) && sw.default.as_ref().is_none_or(|d| count(d, in_loop, n)),
            ast::StmtKind::Block(b) => count(&b.stmts, in_loop, n),
            ast::StmtKind::TryBlock { body, handler, .. } => count(&body.stmts, in_loop, n) && count(&handler.stmts, in_loop, n),
            _ => true,
        })
    }
    fn if_ok(i: &ast::IfStmt, in_loop: bool, n: &mut usize) -> bool {
        count(&i.then.stmts, in_loop, n)
            && match i.else_.as_deref() {
                Some(ast::ElseBranch::If(e)) => if_ok(e, in_loop, n),
                Some(ast::ElseBranch::Block(b)) => count(&b.stmts, in_loop, n),
                None => true,
            }
    }
    let mut n = 0;
    count(stmts, false, &mut n)
}

// -- terminating statements ------------------------------------------------------

/// Go's terminating-statement rule on the last statement of `stmts`. With
/// `jumps`, `break`/`continue` also count (used for `catch` blocks).
pub(crate) fn terminates(stmts: &[Stmt], jumps: bool) -> bool {
    let Some(last) = stmts.last() else { return false };
    match &last.kind {
        StmtKind::Return(_) => true,
        StmtKind::Break(_) | StmtKind::Continue(_) => jumps,
        StmtKind::Expr(e) => matches!(e.kind, ExprKind::Builtin { op: Builtin::Panic, .. }),
        StmtKind::Block(b) => terminates(&b.stmts, jumps),
        StmtKind::TryBlock { body, handler, .. } => terminates(&body.stmts, jumps) && terminates(&handler.stmts, jumps),
        StmtKind::If { then, else_: Some(e), .. } => terminates(&then.stmts, jumps) && terminates(&e.stmts, jumps),
        StmtKind::Loop { label, cond: None, body } => !breaks_to(&body.stmts, *label),
        StmtKind::Switch(sw) => {
            sw.default.as_ref().is_some_and(|d| terminates(&d.stmts, jumps) && !breaks_to(&d.stmts, sw.label))
                && sw.cases.iter().all(|c| terminates(&c.body.stmts, jumps) && !breaks_to(&c.body.stmts, sw.label))
        }
        _ => false,
    }
}

/// True if a `break` targeting `label` appears in `stmts`.
fn breaks_to(stmts: &[Stmt], label: hir::Label) -> bool {
    stmts.iter().any(|s| stmt_breaks_to(s, label))
}

fn stmt_breaks_to(s: &Stmt, label: hir::Label) -> bool {
    let blk = |b: &hir::Block| breaks_to(&b.stmts, label);
    match &s.kind {
        StmtKind::Break(l) => *l == label,
        StmtKind::Block(b) => blk(b),
        StmtKind::TryBlock { body, handler, .. } => blk(body) || blk(handler),
        StmtKind::If { cond, then, else_ } => expr_breaks_to(cond, label) || blk(then) || else_.as_ref().is_some_and(blk),
        StmtKind::Loop { body, .. }
        | StmtKind::ForRange { body, .. }
        | StmtKind::ForSlice { body, .. }
        | StmtKind::ForString { body, .. }
        | StmtKind::ForMap { body, .. } => blk(body),
        StmtKind::Switch(sw) => sw.cases.iter().any(|c| blk(&c.body)) || sw.default.as_ref().is_some_and(blk),
        StmtKind::Let { init: Some(e), .. } | StmtKind::Expr(e) | StmtKind::Discard(e) => expr_breaks_to(e, label),
        StmtKind::LetTuple { init, .. } => expr_breaks_to(init, label),
        StmtKind::Assign { value, .. } | StmtKind::CompoundAssign { value, .. } => expr_breaks_to(value, label),
        _ => false,
    }
}

/// Breaks hidden in `try ... else break` / `catch { break }` expressions.
fn expr_breaks_to(e: &hir::Expr, label: hir::Label) -> bool {
    match &e.kind {
        ExprKind::TryCatch { value, handler, .. } => expr_breaks_to(value, label) || breaks_to(&handler.stmts, label),
        ExprKind::TryElse { value, fallback } => expr_breaks_to(value, label) || matches!(**fallback, TryFallback::Break(l) if l == label),
        ExprKind::Propagate { value, .. } | ExprKind::Await(value) => expr_breaks_to(value, label),
        ExprKind::Call { args, .. } | ExprKind::Builtin { args, .. } | ExprKind::Tuple(args) | ExprKind::SliceLit(args) => {
            args.iter().any(|a| expr_breaks_to(a, label))
        }
        ExprKind::Binary { lhs, rhs, .. } => expr_breaks_to(lhs, label) || expr_breaks_to(rhs, label),
        _ => false,
    }
}
