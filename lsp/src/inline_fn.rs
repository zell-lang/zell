//! Inline function (`refactor.inline`): "Inline call to f" on a call and
//! "Inline function f" on the declaration (every call inlined; the
//! declaration is removed when every use is a call).
//!
//! A body of one `return e` replaces the call anywhere in an expression
//! (parenthesized when needed). A longer body is inlined only at a call
//! statement (`f(x)`, `await f(x)`, `f(x)?`) or `var v = f(x)`, and only
//! when its one `return` (if any) is its last statement: the statements
//! go before, the returned value takes the call's place.
//!
//! Arguments: a literal, a name or `this` is substituted for its
//! parameter; any other argument (or one whose parameter the body
//! assigns, takes the address of, calls a method on or captures in a
//! closure) is bound to a `var` first, in argument order; an unused
//! argument with side effects is kept as `_ = arg`. The receiver of a
//! method replaces `this` (a call result is bound to a `var`). Locals of
//! the body (and the temporaries) are renamed when their name is used in
//! the calling function. Names the body uses must mean the same at the
//! call site: missing namespaces are imported, private names of another
//! namespace and names hidden by a caller's local are refused.
//!
//! Refused (the code action is listed disabled with the reason): no body,
//! early or several `return`s, `defer`, recursion, an interface call or a
//! function value, an `async` function called without `await`, a fallible
//! function (or one using `?`) whose call is not propagated with `?`,
//! variadic functions, generic functions whose type parameters the body
//! names but the call does not write, a `this` receiver that is not a
//! name, field or call, and calls where temporaries cannot be placed (a
//! loop header, `else if`, the right side of `&&`/`||`, a lambda).

use crate::calls::{self, WsEdits};
use crate::completion::global_name;
use crate::refactor::{enclosing_func, enclosing_stmt, import_edit, indent_of, is_word, slice, Edits};
use std::collections::{HashMap, HashSet};
use zell::ast::{self, BinaryOp, Expr, ExprKind, StmtKind, TypeKind, UnaryOp};
use zell::sema::index::{SymKey, SymbolId, SymbolKind as K};
use zell::session::Analysis;
use zell::span::Span;

/// The inline refactoring at `sel`: on a call (`all == false`) or on a
/// function's declaration (`all`). `None` when not applicable; the title
/// with the edits or the reason it is refused.
pub(crate) fn inline(a: &Analysis, file: usize, sel: Span, all: bool) -> Option<(String, Result<WsEdits, String>)> {
    let sym_id = if all { a.index.declaration_at(file, sel.start)? } else { a.index.reference_at(file, sel.start)?.symbol };
    let sym = a.index.symbol(sym_id);
    let name = sym.name.to_string();
    let title = if all { format!("Inline function {name}") } else { format!("Inline call to {name}") };
    match sym.key {
        SymKey::Func(_) => {}
        SymKey::IfaceMethod(..) if !all => return Some((title, Err(format!("`{name}` is called through an interface: the implementation is chosen at run time")))),
        SymKey::Local(..) if !all && sym.ty.is_some_and(|t| zell::sema::index::short_type_name(&a.hir, t).starts_with("func")) => {
            return Some((title, Err(format!("`{name}` is a function value: only declared functions can be inlined"))))
        }
        _ => return None,
    }
    let loc = sym.loc?;
    let decl = calls::find_func(a.asts.get(loc.file)?, loc.name_span)?;
    let callee = match Callee::new(a, sym_id, loc.file, decl) {
        Ok(c) => c,
        Err(e) => return Some((title, Err(e))),
    };
    let r = if all { inline_all(a, &callee, loc.full_span) } else { inline_one(a, &callee, file, sel) };
    Some((title, r))
}

fn inline_one(a: &Analysis, c: &Callee, file: usize, sel: Span) -> Result<WsEdits, String> {
    let r = a.index.reference_at(file, sel.start).ok_or("no call here")?;
    let exprs = calls::exprs(&a.asts[file]);
    let call = calls::call_named(&exprs, r.span).ok_or_else(|| format!("`{}` is used as a value here, not called", c.name))?;
    let mut taken = HashMap::new();
    let mut out = WsEdits::new();
    let (edits, imports) = c.at_call(a, file, &exprs, call, &mut taken)?;
    calls::push(&mut out, file, with_imports(a, file, edits, imports));
    Ok(finish(a, out))
}

fn inline_all(a: &Analysis, c: &Callee, full: Span) -> Result<WsEdits, String> {
    let mut out = WsEdits::new();
    let mut taken = HashMap::new();
    let mut only_calls = true;
    let mut by_file: Vec<(usize, Vec<Span>)> = Vec::new();
    for r in a.references_of(c.sym) {
        match by_file.iter_mut().find(|(f, _)| *f == r.file) {
            Some((_, v)) => v.push(r.span),
            None => by_file.push((r.file, vec![r.span])),
        }
    }
    let mut calls_found = 0;
    for (file, spans) in by_file {
        let exprs = calls::exprs(&a.asts[file]);
        let mut edits = Edits::new();
        let mut imports = HashSet::new();
        for sp in spans {
            let Some(call) = calls::call_named(&exprs, sp) else {
                only_calls = false;
                continue;
            };
            if a.is_read_only(file) {
                return Err(format!("a call in a read-only file ({})", calls::place(a, file, sp.start)));
            }
            let (e, i) = c.at_call(a, file, &exprs, call, &mut taken).map_err(|e| format!("{e} ({})", calls::place(a, file, sp.start)))?;
            edits.extend(e);
            imports.extend(i);
            calls_found += 1;
        }
        if !edits.is_empty() {
            calls::push(&mut out, file, with_imports(a, file, edits, imports));
        }
    }
    if calls_found == 0 {
        return Err(format!("`{}` is never called", c.name));
    }
    if only_calls {
        calls::push(&mut out, c.file, vec![(decl_lines(&a.files[c.file].text, full), String::new())]);
    }
    if let Some((f, _)) = out.iter().find(|(_, e)| calls::overlapping(e)) {
        return Err(format!("calls of `{}` are nested in {}", c.name, calls::place(a, *f, 0).split(':').next().unwrap_or("")));
    }
    Ok(finish(a, out))
}

fn with_imports(a: &Analysis, file: usize, mut edits: Edits, imports: HashSet<String>) -> Edits {
    let mut imports: Vec<String> = imports.into_iter().collect();
    imports.sort();
    for ns in imports {
        edits.push(import_edit(&a.asts[file], &a.files[file].text, &ns));
    }
    edits
}

fn finish(a: &Analysis, out: WsEdits) -> WsEdits {
    out.into_iter().map(|(f, e)| (f, crate::refactor::formatted(&a.files[f].text, e))).collect()
}

/// The lines of a declaration with its doc comment and one blank line
/// around it.
pub(crate) fn decl_lines(text: &str, full: Span) -> Span {
    let mut start = text[..full.start as usize].rfind('\n').map_or(0, |i| i + 1);
    // Doc comment lines directly above.
    while start > 0 {
        let prev = text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        if text[prev..start].trim_start().starts_with("//") {
            start = prev;
        } else {
            break;
        }
    }
    let mut end = text[full.end as usize..].find('\n').map_or(text.len(), |i| full.end as usize + i + 1);
    if text[end..].starts_with('\n') {
        end += 1;
    } else if start >= 2 && &text[start - 2..start] == "\n\n" {
        start -= 1;
    }
    Span::new(start as u32, end as u32)
}

/// What is known about the function being inlined, independent of the
/// call.
struct Callee<'a> {
    sym: SymbolId,
    name: String,
    file: usize,
    text: &'a str,
    decl: &'a ast::FuncDecl,
    body: &'a ast::Block,
    fallible: bool,
    /// The trailing `return` values (empty: none or `return`).
    ret: Vec<&'a Expr>,
    /// Per parameter: (reference spans, mutated, captured by a closure).
    params: Vec<(Vec<Span>, bool, bool)>,
    /// Locals declared in the body: name and every span (declaration too).
    locals: Vec<(String, Vec<Span>)>,
    /// Names of other declarations the body uses unqualified.
    globals: Vec<(Span, SymbolId)>,
    /// `this` in the body.
    this: Vec<Span>,
    /// Generic parameter names the body mentions.
    generics: Vec<String>,
}

impl<'a> Callee<'a> {
    fn new(a: &'a Analysis, sym: SymbolId, file: usize, decl: &'a ast::FuncDecl) -> Result<Callee<'a>, String> {
        let name = decl.name.name.clone();
        let body = decl.body.as_ref().ok_or_else(|| format!("`{name}` has no body"))?;
        if decl.params.last().is_some_and(|p| matches!(p.ty.kind, TypeKind::Variadic(_))) {
            return Err(format!("`{name}` is variadic"));
        }
        let text = a.files[file].text.as_str();
        let mut lambdas = Vec::new();
        let returns = std::cell::RefCell::new(Vec::new());
        let mut defers = false;
        let mut this = Vec::new();
        let mutated = std::cell::RefCell::new(Vec::new());
        {
            let mut w = crate::assist::Walker {
                on_stmt: &mut |s| match &s.kind {
                    StmtKind::Return(_) => returns.borrow_mut().push(s.span),
                    StmtKind::Defer(_) => defers = true,
                    StmtKind::Assign { target, .. } => mutated.borrow_mut().extend(root(target)),
                    _ => {}
                },
                on_expr: &mut |e| match &e.kind {
                    ExprKind::Lambda { .. } => lambdas.push(e.span),
                    ExprKind::This => this.push(e.span),
                    ExprKind::Unary { op: UnaryOp::AddrOf, expr } => mutated.borrow_mut().extend(root(expr)),
                    ExprKind::Call { callee, .. } => {
                        if let ExprKind::Field { base, .. } = &callee.kind {
                            mutated.borrow_mut().extend(root(base));
                        }
                    }
                    ExprKind::Try { handler: ast::TryHandler::Else(v), .. } if matches!(**v, ast::ElseValue::Return(..)) => returns.borrow_mut().push(e.span),
                    _ => {}
                },
                on_block: &mut |_| {},
            };
            for s in &body.stmts {
                w.stmt(s);
            }
        }
        let (mut returns, mutated) = (returns.into_inner(), mutated.into_inner());
        let in_lambda = |s: Span| lambdas.iter().any(|l| l.start <= s.start && s.end <= l.end);
        let mut qs = false;
        crate::assist::Walker {
            on_stmt: &mut |_| {},
            on_expr: &mut |e| {
                if matches!(e.kind, ExprKind::Propagate { .. }) && !in_lambda(e.span) {
                    qs = true;
                }
            },
            on_block: &mut |_| {},
        }
        .func(decl);
        returns.retain(|r| !in_lambda(*r));
        if defers {
            return Err(format!("`{name}` uses `defer`"));
        }
        let last = body.stmts.last();
        let ret: Vec<&Expr> = match last.map(|s| &s.kind) {
            Some(StmtKind::Return(es)) => es.iter().collect(),
            _ => Vec::new(),
        };
        let trailing = last.is_some_and(|s| matches!(s.kind, StmtKind::Return(_)));
        if returns.len() > 1 || (returns.len() == 1 && !trailing) {
            return Err(format!("`{name}` returns early or in several places"));
        }
        let fallible = decl.ret.as_ref().is_some_and(|t| matches!(t.kind, TypeKind::Fallible(_))) || qs;
        // References in the body.
        let mut params: Vec<(Vec<Span>, bool, bool)> = vec![(Vec::new(), false, false); decl.params.len()];
        let mut locals: Vec<(SymbolId, String, Vec<Span>)> = Vec::new();
        let mut globals = Vec::new();
        for r in a.index.references.iter().filter(|r| r.file == file && body.span.start <= r.span.start && r.span.end <= body.span.end) {
            if r.symbol == sym {
                return Err(format!("`{name}` is recursive"));
            }
            let s = a.index.symbol(r.symbol);
            if let Some(i) = s.loc.filter(|l| l.file == file).and_then(|l| decl.params.iter().position(|p| p.name.span == l.name_span)) {
                let p = &mut params[i];
                p.0.push(r.span);
                p.1 |= mutated.contains(&r.span.start);
                p.2 |= in_lambda(r.span);
                continue;
            }
            if let Some(l) = s.loc.filter(|l| l.file == file && body.span.start <= l.name_span.start && l.name_span.end <= body.span.end) {
                match locals.iter_mut().find(|x| x.0 == r.symbol) {
                    Some(x) => x.2.push(r.span),
                    None => locals.push((r.symbol, s.name.to_string(), vec![l.name_span, r.span])),
                }
                continue;
            }
            if &*s.name == "this" || calls::qualified(text, r.span) {
                continue;
            }
            match s.kind {
                K::Method => return Err(format!("`{name}` calls method `{}` without a receiver", s.name)),
                K::Field if !text[r.span.end as usize..].trim_start().starts_with(':') => return Err(format!("`{name}` uses field `{}` without a receiver", s.name)),
                K::Field | K::Variant | K::BuiltinFunc | K::BuiltinType | K::Local | K::Param => {}
                _ if matches!(s.key, SymKey::LocalConst(..) | SymKey::Builtin(_)) => {}
                _ => {
                    if slice(text, r.span) != &*s.name {
                        return Err(format!("`{name}` uses `{}` through an import alias", s.name));
                    }
                    globals.push((r.span, r.symbol));
                }
            }
        }
        let body_text = slice(text, body.span);
        let generics = decl.generics.iter().map(|g| g.name.name.clone()).filter(|g| is_word(body_text, g)).collect();
        Ok(Callee {
            sym,
            name,
            file,
            text,
            decl,
            body,
            fallible,
            ret,
            params,
            locals: locals.into_iter().map(|l| (l.1, l.2)).collect(),
            globals,
            this,
            generics,
        })
    }

    fn single_expr(&self) -> bool {
        self.body.stmts.len() == 1 && self.ret.len() == 1
    }

    /// The edits inlining `call` (in `file`, whose expressions are
    /// `exprs`) and the namespaces to import there. `taken` holds names
    /// already given in each calling function (by its start).
    fn at_call(&self, a: &Analysis, file: usize, exprs: &[&Expr], call: &Expr, taken: &mut HashMap<(usize, u32), HashSet<String>>) -> Result<(Edits, HashSet<String>), String> {
        let name = &self.name;
        let ExprKind::Call { callee, args } = &call.kind else { return Err("not a call".into()) };
        let caller_text = a.files[file].text.as_str();
        let ast = &a.asts[file];
        if args.len() != self.params.len() || args.iter().any(|x| matches!(x.kind, ExprKind::Spread(_))) {
            return Err(format!("the call of `{name}` does not pass one argument per parameter"));
        }
        // The expression the inlined code replaces: the call, `await` of
        // it, `?` of that.
        let mut site = call;
        let mut awaited = false;
        if let Some(p) = calls::parent(exprs, site.span).filter(|p| matches!(p.kind, ExprKind::Await(_))) {
            site = p;
            awaited = true;
        }
        let mut propagated = false;
        let mut up = calls::parent(exprs, site.span);
        while let Some(p) = up.filter(|p| matches!(p.kind, ExprKind::Paren(_))) {
            up = calls::parent(exprs, p.span);
        }
        if let Some(p) = up.filter(|p| matches!(p.kind, ExprKind::Propagate { context: None, .. })) {
            site = p;
            propagated = true;
        }
        if self.decl.is_async && !awaited {
            return Err(format!("`{name}` is async: without `await` the call is a Task value"));
        }
        if self.fallible && !propagated {
            return Err(format!("`{name}` can fail: only a call propagated with `?` can be inlined"));
        }
        let func = enclosing_func(ast, call.span).ok_or_else(|| format!("the call of `{name}` is not inside a function"))?;
        let fn_text = slice(caller_text, func.span);
        let taken = taken.entry((file, func.span.start)).or_default();
        let body_text = slice(self.text, self.body.span);
        let fresh = |base: &str, taken: &mut HashSet<String>| {
            let mut n = base.to_string();
            let mut i = 2;
            while is_word(fn_text, &n) || taken.contains(&n) || (n != base && is_word(body_text, &n)) {
                n = format!("{base}{i}");
                i += 1;
            }
            taken.insert(n.clone());
            n
        };
        // Names used by the body must mean the same at the call.
        let caller_ns = a.index.files.get(file).and_then(|f| f.as_ref()).map(|f| f.namespace.clone()).unwrap_or_default();
        let caller_locals: HashSet<&str> = a.index.symbols.iter().filter(|s| matches!(s.kind, K::Local | K::Param) && s.loc.is_some_and(|l| l.file == file && func.span.start <= l.name_span.start && l.name_span.end <= func.span.end)).map(|s| &*s.name).collect();
        let mut imports = HashSet::new();
        for &(_, id) in &self.globals {
            let s = a.index.symbol(id);
            if caller_locals.contains(&*s.name) {
                return Err(format!("`{}` used by `{name}` is hidden by a local of the calling function", s.name));
            }
            match global_name(a, file, &s.name) {
                Some(o) if o == id => continue,
                Some(o) => return Err(format!("`{}` used by `{name}` means `{}` at the call", s.name, a.index.symbol(o).full_name)),
                None => {}
            }
            let full = s.full_name.to_string();
            let ns = full.rsplit_once('.').map(|x| x.0.to_string()).unwrap_or_default();
            if ns.is_empty() {
                return Err(format!("`{}` used by `{name}` is not visible at the call", s.name));
            }
            if !s.is_std && ns != caller_ns && !s.name.starts_with(|c: char| c.is_uppercase()) && s.kind != K::Namespace {
                return Err(format!("`{name}` uses `{}`, private to namespace {ns}", s.name));
            }
            imports.insert(ns);
        }
        // Generic parameters the body names.
        let mut type_args: Vec<(String, String)> = Vec::new();
        if !self.generics.is_empty() {
            let ExprKind::TypeArgs { args: targs, .. } = &callee.kind else {
                return Err(format!("`{name}` is generic and the call does not write its type arguments"));
            };
            for (g, t) in self.decl.generics.iter().zip(targs) {
                type_args.push((g.name.name.clone(), slice(caller_text, t.span).to_string()));
            }
        }
        // Statement-level or expression-level.
        let stmt = enclosing_stmt(ast, site.span).ok_or("the call is not in a statement")?;
        let stmt_level = match &stmt.kind {
            StmtKind::Expr(e) => e.span == site.span,
            StmtKind::Var(v) => v.init.as_ref().is_some_and(|i| i.span == site.span),
            _ => false,
        };
        if !self.single_expr() && !stmt_level {
            return Err(format!("`{name}` has several statements: only a call statement or `var v = {name}(...)` can be inlined"));
        }
        // Arguments and the receiver.
        let mut prelude: Vec<String> = Vec::new();
        let mut reps: Vec<(Span, String)> = Vec::new();
        for (i, (arg, (uses, mutated, captured))) in args.iter().zip(&self.params).enumerate() {
            let src = slice(caller_text, arg.span);
            if uses.is_empty() {
                if !calls::is_pure(arg) {
                    prelude.push(format!("_ = {src}"));
                }
                continue;
            }
            let direct = (calls::is_literal(arg) && !mutated) || (calls::is_trivial(arg) && !mutated && !captured) || (calls::is_pure(arg) && uses.len() == 1 && !mutated && !captured && self.single_expr());
            let value = if direct {
                if calls::is_atomic(arg, src) {
                    src.to_string()
                } else {
                    format!("({src})")
                }
            } else {
                let p = &self.decl.params[i];
                let n = fresh(&p.name.name, taken);
                let ty = slice(self.text, p.ty.span);
                if calls::is_literal(arg) && !self.decl.generics.iter().any(|g| is_word(ty, &g.name.name)) {
                    prelude.push(format!("var {n} {ty} = {src}"));
                } else {
                    prelude.push(format!("var {n} = {src}"));
                }
                n
            };
            reps.extend(uses.iter().map(|s| (*s, value.clone())));
        }
        if !self.this.is_empty() {
            let base = match &callee.kind {
                ExprKind::Field { base, .. } => base,
                _ => return Err(format!("`{name}` uses `this` but the call has no receiver")),
            };
            let src = slice(caller_text, base.span);
            let recv = if receiver_ok(base) {
                src.to_string()
            } else if matches!(base.kind, ExprKind::Call { .. } | ExprKind::Await(_)) {
                let n = fresh("recv", taken);
                prelude.insert(0, format!("var {n} = {src}"));
                n
            } else {
                return Err(format!("the receiver `{src}` is not a name: bind it to a variable first"));
            };
            reps.extend(self.this.iter().map(|s| (*s, recv.clone())));
        }
        for (old, spans) in &self.locals {
            if is_word(fn_text, old) || taken.contains(old) {
                let n = fresh(old, taken);
                reps.extend(spans.iter().map(|s| (*s, n.clone())));
            } else {
                taken.insert(old.clone());
            }
        }
        if !prelude.is_empty() && !stmt_level {
            temps_allowed(exprs, stmt, site).map_err(|w| format!("the arguments of `{name}` need temporaries, which cannot be placed {w}"))?;
        }
        let render = |s: Span| subst_types(&render(self.text, s, &reps), &type_args);
        let ret_text = |e: &Expr| {
            let mut t = render(e.span);
            if self.fallible && fallible_value(a, self.file, e) {
                if !calls::is_atomic(e, &t) {
                    t = format!("({t})");
                }
                t.push('?');
            }
            t
        };
        let (_, ws) = indent_of(caller_text, stmt.span.start);
        let mut edits = Edits::new();
        if self.single_expr() && !stmt_level {
            let e = self.ret[0];
            let mut t = ret_text(e);
            if needs_parens(exprs, site) && !calls::is_atomic(e, &t) {
                t = format!("({t})");
            }
            edits.push((site.span, t));
            if !prelude.is_empty() {
                let (ls, ws) = indent_of(caller_text, stmt.span.start);
                edits.push((Span::new(ls, ls), prelude.iter().map(|l| format!("{ws}{l}\n")).collect()));
            }
            return Ok((edits, imports));
        }
        let mut lines = prelude;
        let stmts: Vec<&ast::Stmt> = self.body.stmts.iter().filter(|s| !matches!(s.kind, StmtKind::Return(_))).collect();
        if let (Some(first), Some(last)) = (stmts.first(), stmts.last()) {
            let (_, cws) = indent_of(self.text, first.span.start);
            let body = render(Span::new(first.span.start, last.span.end));
            lines.extend(body.lines().enumerate().map(|(i, l)| if i == 0 { l.to_string() } else { l.strip_prefix(cws.as_str()).unwrap_or(l).to_string() }));
        }
        let ret = match self.ret.len() {
            0 => None,
            1 => Some((ret_text(self.ret[0]), self.ret[0])),
            _ => Some((format!("({})", self.ret.iter().map(|e| render(e.span)).collect::<Vec<_>>().join(", ")), self.ret[0])),
        };
        match (&stmt.kind, ret) {
            (StmtKind::Var(_), Some((t, _))) => {
                lines.push(format!("{}{t}{}", &caller_text[stmt.span.start as usize..site.span.start as usize], &caller_text[site.span.end as usize..stmt.span.end as usize]));
            }
            (StmtKind::Var(_), None) => return Err(format!("`{name}` returns no value")),
            (_, Some((t, e))) if self.ret.len() == 1 && !calls::is_pure(e) => {
                let call_like = matches!(e.kind, ExprKind::Call { .. } | ExprKind::Await(_) | ExprKind::Propagate { .. } | ExprKind::Try { .. }) || t.ends_with('?');
                lines.push(if call_like { t } else { format!("_ = {t}") });
            }
            _ => {}
        }
        let new = if lines.is_empty() { String::new() } else { lines.join(&format!("\n{ws}")) };
        if new.is_empty() {
            edits.push((whole_line(caller_text, stmt.span), new));
        } else {
            edits.push((stmt.span, new));
        }
        Ok((edits, imports))
    }
}

/// A receiver that may be repeated for every `this`: a name, `this` or a
/// field of one.
fn receiver_ok(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Ident(_) | ExprKind::This => true,
        ExprKind::Field { base, .. } | ExprKind::Paren(base) => receiver_ok(base),
        _ => false,
    }
}

/// The name at the root of an assignment target or operand
/// (`p.X[i]` -> `p`): its start.
fn root(e: &Expr) -> Option<u32> {
    match &e.kind {
        ExprKind::Ident(i) => Some(i.span.start),
        ExprKind::Field { base, .. } | ExprKind::Index { base, .. } | ExprKind::Paren(base) => root(base),
        ExprKind::Unary { op: UnaryOp::Deref, expr } => root(expr),
        _ => None,
    }
}

/// `e` (a returned value) is itself fallible: a call of a `T!` function.
fn fallible_value(a: &Analysis, file: usize, e: &Expr) -> bool {
    let call = match &e.kind {
        ExprKind::Await(x) => x,
        _ => e,
    };
    let ExprKind::Call { callee, .. } = &call.kind else { return false };
    let Some(name) = calls::callee_name(callee) else { return false };
    a.index.references.iter().find(|r| r.file == file && r.span == name).is_some_and(|r| a.index.symbol(r.symbol).signature.trim_end().ends_with('!'))
}

/// The value replacing `site` binds looser than its context.
fn needs_parens(exprs: &[&Expr], site: &Expr) -> bool {
    match calls::parent(exprs, site.span).map(|p| &p.kind) {
        Some(ExprKind::Binary { .. } | ExprKind::Unary { .. } | ExprKind::Field { .. } | ExprKind::Index { .. } | ExprKind::Await(_) | ExprKind::Propagate { .. } | ExprKind::Try { .. } | ExprKind::Range(..) | ExprKind::TypeArgs { .. }) => true,
        Some(ExprKind::Call { callee, .. }) => callee.span.start <= site.span.start && site.span.end <= callee.span.end,
        _ => false,
    }
}

/// Whether statements may be inserted before `stmt` to run right before
/// `site`: `Err` names where they cannot.
fn temps_allowed(exprs: &[&Expr], stmt: &ast::Stmt, site: &Expr) -> Result<(), &'static str> {
    let inside = |s: Span| s.start <= site.span.start && site.span.end <= s.end;
    match &stmt.kind {
        StmtKind::Var(_) | StmtKind::Expr(_) | StmtKind::Assign { .. } | StmtKind::Return(_) => {}
        StmtKind::If(i) if inside(i.cond.span) => {}
        StmtKind::Switch(s) if s.subject.as_ref().is_some_and(|e| inside(e.span)) => {}
        StmtKind::For(_) => return Err("in a loop header"),
        StmtKind::If(_) => return Err("in an `else if` condition"),
        _ => return Err("in this statement"),
    }
    let mut at = site.span;
    while let Some(p) = calls::parent(exprs, at) {
        if !inside_span(stmt.span, p.span) {
            break;
        }
        match &p.kind {
            ExprKind::Lambda { .. } => return Err("in a lambda"),
            ExprKind::Binary { op: BinaryOp::And | BinaryOp::Or, right, .. } if right.span.start <= site.span.start && site.span.end <= right.span.end => return Err("on the right of `&&`/`||`"),
            ExprKind::Try { expr, .. } if !(expr.span.start <= site.span.start && site.span.end <= expr.span.end) => return Err("in a `try` handler"),
            _ => {}
        }
        at = p.span;
    }
    Ok(())
}

fn inside_span(outer: Span, inner: Span) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// `text[span]` with `reps` (spans inside it) replaced.
fn render(text: &str, span: Span, reps: &[(Span, String)]) -> String {
    let mut r: Vec<&(Span, String)> = reps.iter().filter(|(s, _)| span.start <= s.start && s.end <= span.end).collect();
    r.sort_by_key(|(s, _)| s.start);
    let mut out = String::new();
    let mut at = span.start as usize;
    for (s, t) in r {
        if (s.start as usize) < at {
            continue;
        }
        out.push_str(&text[at..s.start as usize]);
        out.push_str(t);
        at = s.end as usize;
    }
    out.push_str(&text[at..span.end as usize]);
    out
}

/// `text` with the generic parameter names replaced by the written type
/// arguments (whole words outside string literals, not after `.`).
fn subst_types(text: &str, args: &[(String, String)]) -> String {
    if args.is_empty() {
        return text.to_string();
    }
    let mut out = String::new();
    let mut word = String::new();
    let mut in_str = false;
    let mut prev = ' ';
    let flush = |word: &mut String, out: &mut String| {
        let after_dot = out.ends_with('.');
        match args.iter().find(|(g, _)| g == word).filter(|_| !after_dot) {
            Some((_, t)) => out.push_str(t),
            None => out.push_str(word),
        }
        word.clear();
    };
    for c in text.chars() {
        if in_str {
            out.push(c);
            if c == '"' && prev != '\\' {
                in_str = false;
            }
        } else if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            if c == '"' {
                in_str = true;
            }
            out.push(c);
        }
        prev = c;
    }
    flush(&mut word, &mut out);
    out
}

/// The whole line of `s` when nothing else is on it.
fn whole_line(text: &str, s: Span) -> Span {
    let ls = text[..s.start as usize].rfind('\n').map_or(0, |i| i + 1);
    let le = text[s.end as usize..].find('\n').map_or(text.len(), |i| s.end as usize + i + 1);
    if text[ls..s.start as usize].trim().is_empty() && text[s.end as usize..le].trim().is_empty() {
        Span::new(ls as u32, le as u32)
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn substitutes_type_parameters() {
        let args = [("T".to_string(), "int".to_string())];
        assert_eq!(super::subst_types("var x T = \"T\" + a.T", &args), "var x int = \"T\" + a.T");
    }
}
