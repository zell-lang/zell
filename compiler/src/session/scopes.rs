//! Block scopes of a file, from its syntax tree, as sema opens them: a
//! function's parameters, its body block, every nested block, a `for`
//! with its loop variables, a `catch` with its error variable, a `switch`
//! case with its binding and statements, a lambda with its parameters.
//! Rename uses them to find where a local is visible (from its name to
//! the end of its scope) and which locals share a block.
//!
//! Scopes are derived from one file's syntax only, so they are computed on
//! demand per analysis ([`super::Analysis::scopes`]) and are as current as
//! the file's AST.

use crate::ast::{self, ElseBranch, ExprKind, ForKind, LambdaBody, PatternKind, StmtKind, TryHandler};
use crate::span::Span;
use std::collections::HashMap;

/// One scope: its span and the enclosing scope.
#[derive(Clone, Debug)]
pub struct Scope {
    pub span: Span,
    pub parent: Option<u32>,
}

/// The scopes of a file and the scope each local name is declared in.
#[derive(Clone, Debug, Default)]
pub struct FileScopes {
    pub scopes: Vec<Scope>,
    /// Start of a declared name -> its scope and where it becomes visible
    /// (after its declaration statement for `var` and `const`).
    binders: HashMap<u32, (u32, u32)>,
}

impl FileScopes {
    /// The scopes of `f`.
    pub fn of(f: &ast::File) -> FileScopes {
        let mut w = FileScopes::default();
        for d in &f.decls {
            match d {
                ast::Decl::Func(fd) => w.func(fd, None),
                ast::Decl::Struct(s) => s.methods.iter().for_each(|m| w.func(m, None)),
                ast::Decl::Enum(e) => e.methods.iter().for_each(|m| w.func(m, None)),
                ast::Decl::Interface(i) => i.methods.iter().for_each(|m| w.func(m, None)),
                ast::Decl::Var(v) => {
                    if let Some(e) = &v.init {
                        let s = w.push(v.span, None);
                        w.expr(e, s);
                    }
                }
                ast::Decl::Const(c) => {
                    let s = w.push(c.span, None);
                    w.expr(&c.value, s);
                }
                _ => {}
            }
        }
        w
    }

    /// The scope of the local whose name starts at `start` (`name` is its
    /// span): where it was declared, else the innermost scope around it.
    pub fn scope_of(&self, name: Span) -> Option<u32> {
        if let Some(&(s, _)) = self.binders.get(&name.start) {
            return Some(s);
        }
        self.scopes.iter().enumerate().filter(|(_, s)| s.span.start <= name.start && name.end <= s.span.end).min_by_key(|(_, s)| s.span.end - s.span.start).map(|(i, _)| i as u32)
    }

    /// Where a local declared at `name` is visible: from its name (after
    /// its initializer, for `var` and `const`) to the end of its scope.
    pub fn region(&self, name: Span) -> Option<(u32, Span)> {
        let s = self.scope_of(name)?;
        let from = self.binders.get(&name.start).map_or(name.start, |b| b.1);
        Some((s, Span::new(from, self.scopes[s as usize].span.end)))
    }

    /// Whether scope `a` is `b` or nested in it.
    pub fn within(&self, mut a: u32, b: u32) -> bool {
        loop {
            if a == b {
                return true;
            }
            match self.scopes.get(a as usize).and_then(|s| s.parent) {
                Some(p) => a = p,
                None => return false,
            }
        }
    }

    fn push(&mut self, span: Span, parent: Option<u32>) -> u32 {
        self.scopes.push(Scope { span, parent });
        self.scopes.len() as u32 - 1
    }

    fn bind(&mut self, id: &ast::Ident, s: u32) {
        self.bind_from(id, s, id.span.start);
    }

    fn bind_from(&mut self, id: &ast::Ident, s: u32, from: u32) {
        if id.span != Span::DUMMY {
            self.binders.insert(id.span.start, (s, from));
        }
    }

    fn func(&mut self, d: &ast::FuncDecl, parent: Option<u32>) {
        let s = self.push(d.span, parent);
        for p in &d.params {
            self.bind(&p.name, s);
        }
        if let Some(b) = &d.body {
            self.block(b, s);
        }
    }

    fn block(&mut self, b: &ast::Block, parent: u32) {
        let s = self.push(b.span, Some(parent));
        self.stmts(&b.stmts, s);
    }

    fn stmts(&mut self, ss: &[ast::Stmt], s: u32) {
        for st in ss {
            self.stmt(st, s);
        }
    }

    fn if_stmt(&mut self, i: &ast::IfStmt, s: u32) {
        self.expr(&i.cond, s);
        self.block(&i.then, s);
        match i.else_.as_deref() {
            Some(ElseBranch::If(e)) => self.if_stmt(e, s),
            Some(ElseBranch::Block(b)) => self.block(b, s),
            None => {}
        }
    }

    fn stmt(&mut self, st: &ast::Stmt, s: u32) {
        match &st.kind {
            StmtKind::Var(v) => {
                if let Some(e) = &v.init {
                    self.expr(e, s);
                }
                v.names.iter().for_each(|n| self.bind_from(n, s, st.span.end));
            }
            StmtKind::Const(c) => {
                self.expr(&c.value, s);
                self.bind_from(&c.name, s, st.span.end);
            }
            StmtKind::If(i) => self.if_stmt(i, s),
            StmtKind::For(f) => {
                let fs = self.push(f.span, Some(s));
                match &f.kind {
                    ForKind::In { vars, iter } => {
                        self.expr(iter, s);
                        vars.iter().for_each(|v| self.bind(v, fs));
                    }
                    ForKind::While(e) => self.expr(e, s),
                    ForKind::Infinite => {}
                }
                self.block(&f.body, fs);
            }
            StmtKind::Switch(sw) => {
                if let Some(e) = &sw.subject {
                    self.expr(e, s);
                }
                for c in &sw.cases {
                    let cs = self.push(c.span, Some(s));
                    for p in &c.patterns {
                        match &p.kind {
                            PatternKind::Expr(e) => self.expr(e, cs),
                            PatternKind::Range(a, b) => a.iter().chain(b.iter()).for_each(|e| self.expr(e, cs)),
                            PatternKind::TypeBind { name, .. } => self.bind(name, cs),
                        }
                    }
                    self.stmts(&c.body, cs);
                }
                if let Some(d) = sw.default.as_ref().filter(|d| !d.is_empty()) {
                    let ds = self.push(Span::new(d[0].span.start, d[d.len() - 1].span.end), Some(s));
                    self.stmts(d, ds);
                }
            }
            StmtKind::Return(es) => es.iter().for_each(|e| self.expr(e, s)),
            StmtKind::Labeled { stmt, .. } => self.stmt(stmt, s),
            StmtKind::Defer(e) | StmtKind::Expr(e) => self.expr(e, s),
            StmtKind::Assign { target, value, .. } => {
                self.expr(target, s);
                self.expr(value, s);
            }
            StmtKind::Block(b) => self.block(b, s),
            StmtKind::TryBlock { body, err, handler } => {
                self.block(body, s);
                let hs = self.push(Span::new(err.span.start.min(handler.span.start), handler.span.end), Some(s));
                self.bind(err, hs);
                self.block(handler, hs);
            }
            _ => {}
        }
    }

    fn expr(&mut self, e: &ast::Expr, s: u32) {
        let mut ex = |x: &ast::Expr| self.expr(x, s);
        match &e.kind {
            ExprKind::Field { base, .. } => ex(base),
            ExprKind::Call { callee, args } => {
                ex(callee);
                args.iter().for_each(ex);
            }
            ExprKind::Index { base, index } => {
                ex(base);
                ex(index);
            }
            ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::New(expr) | ExprKind::Paren(expr) | ExprKind::Spread(expr) | ExprKind::TypeArgs { base: expr, .. } => ex(expr),
            ExprKind::NamedArg { value, .. } => ex(value),
            ExprKind::Binary { left, right, .. } => {
                ex(left);
                ex(right);
            }
            ExprKind::Propagate { expr, context } => {
                ex(expr);
                if let Some(c) = context {
                    ex(c);
                }
            }
            ExprKind::StructLit { fields, .. } => fields.iter().for_each(|f| ex(&f.value)),
            ExprKind::CompositeLit { elems, .. } => elems.iter().for_each(|(k, v)| {
                if let Some(k) = k {
                    ex(k);
                }
                ex(v);
            }),
            ExprKind::SliceLit(es) | ExprKind::Tuple(es) => es.iter().for_each(ex),
            ExprKind::MapLit { entries, .. } => entries.iter().for_each(|(k, v)| {
                ex(k);
                ex(v);
            }),
            ExprKind::Range(a, b) => a.iter().chain(b.iter()).for_each(|x| ex(x)),
            ExprKind::Try { expr, handler } => {
                ex(expr);
                match handler {
                    TryHandler::Catch { name, body } => {
                        let hs = self.push(Span::new(name.span.start.min(body.span.start), body.span.end), Some(s));
                        self.bind(name, hs);
                        self.block(body, hs);
                    }
                    TryHandler::Else(v) => match &**v {
                        ast::ElseValue::Expr(x) => self.expr(x, s),
                        ast::ElseValue::Return(xs, _) => xs.iter().for_each(|x| self.expr(x, s)),
                        _ => {}
                    },
                }
            }
            ExprKind::Lambda { params, body, .. } => {
                let ls = self.push(e.span, Some(s));
                params.iter().for_each(|p| self.bind(&p.name, ls));
                match &**body {
                    LambdaBody::Block(b) => self.block(b, ls),
                    LambdaBody::Expr(x) => self.expr(x, ls),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locals_get_their_block() {
        let src = "namespace app\n\nfunc f(p int) {\n    var a = 1\n    {\n        var b = a\n    }\n    for i in [1] {\n        var c = i\n    }\n}\n";
        let mut d = crate::diagnostics::Diagnostics::new();
        let f = crate::parser::parse_file(crate::lexer::lex(src, &mut d), &mut d);
        let sc = FileScopes::of(&f);
        let at = |n: &str| {
            let s = src.find(n).unwrap() as u32;
            Span::new(s, s + 1)
        };
        let (pa, _) = sc.region(at("p int")).unwrap();
        let (a, ra) = sc.region(at("a = 1")).unwrap();
        let (b, _) = sc.region(at("b = a")).unwrap();
        let (i, _) = sc.region(at("i in")).unwrap();
        let (c, _) = sc.region(at("c = i")).unwrap();
        assert!(sc.within(a, pa) && a != pa, "the body is inside the parameters' scope");
        assert!(sc.within(b, a) && b != a && sc.within(c, i) && c != i && sc.within(i, a));
        assert_eq!(ra.end as usize, src.rfind('}').unwrap() + 1);
    }
}
