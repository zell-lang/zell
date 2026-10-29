//! Error-handling rewrites (`refactor.rewrite`, DESIGN.md "Ошибки"):
//! `x?` -> `try x catch err { return err }` and back.
//!
//! Both forms propagate the same error from the same function; the only
//! difference is the line of the trace record (`returned via`): the `?`
//! line, or the `return err` line. They are offered only where that is
//! the whole difference: never inside the body of a block `try` (there
//! `?` jumps to its `catch`, while `return` leaves the function), the
//! `?` form only in a function or lambda declared `T!`, and `?` with a
//! context (`?("ctx")`) is left alone.

use crate::assist::Walker;
use crate::refactor::{enclosing_func, enclosing_stmt, indent_of, is_word, slice, Edits};
use zell::ast::{self, Expr, ExprKind, StmtKind, TryHandler, TypeKind};
use zell::span::Span;

fn inside(outer: Span, s: Span) -> bool {
    outer.start <= s.start && s.end <= outer.end
}

/// Where a `?` at `at` goes: `None` inside the body of a block `try`,
/// else whether the function (or lambda) it returns from is `T!`.
fn returns_fallible(ast: &ast::File, at: Span) -> Option<bool> {
    // (span, Some(fallible) for a lambda, None for a block-try body)
    let (mut tries, mut lambdas): (Vec<(Span, Option<bool>)>, Vec<(Span, Option<bool>)>) = (Vec::new(), Vec::new());
    Walker {
        on_stmt: &mut |s| {
            if let StmtKind::TryBlock { body, .. } = &s.kind {
                if inside(body.span, at) {
                    tries.push((body.span, None));
                }
            }
        },
        on_expr: &mut |e| {
            if let ExprKind::Lambda { ret, .. } = &e.kind {
                if inside(e.span, at) {
                    lambdas.push((e.span, Some(ret.as_ref().is_some_and(|r| matches!(r.kind, TypeKind::Fallible(_))))));
                }
            }
        },
        on_block: &mut |_| {},
    }
    .file(ast);
    let scopes = tries.into_iter().chain(lambdas);
    match scopes.min_by_key(|(s, _)| s.end - s.start) {
        Some((_, kind)) => kind,
        None => Some(enclosing_func(ast, at)?.ret.as_ref().is_some_and(|r| matches!(r.kind, TypeKind::Fallible(_)))),
    }
}

/// The innermost expression around `sel` that `pick` accepts.
fn innermost<'a>(ast: &'a ast::File, sel: Span, pick: impl Fn(&Expr) -> bool) -> Option<&'a Expr> {
    let mut found: Option<&Expr> = None;
    Walker {
        on_stmt: &mut |_| {},
        on_expr: &mut |e| {
            if inside(e.span, sel) && pick(e) {
                found = Some(e);
            }
        },
        on_block: &mut |_| {},
    }
    .file(ast);
    found
}

/// `var v = f()?` / `v = f()?` / `f()?` -> the same with
/// `try f() catch err { return err }`.
pub(crate) fn to_try(ast: &ast::File, text: &str, sel: Span) -> Option<(String, Edits)> {
    let e = innermost(ast, sel, |e| matches!(e.kind, ExprKind::Propagate { context: None, .. }))?;
    let ExprKind::Propagate { expr: inner, .. } = &e.kind else { return None };
    let stmt = enclosing_stmt(ast, e.span)?;
    let whole = match &stmt.kind {
        StmtKind::Var(v) => v.init.as_ref().is_some_and(|i| i.span == e.span),
        StmtKind::Assign { value, .. } => value.span == e.span,
        StmtKind::Expr(x) => x.span == e.span,
        _ => false,
    };
    if !whole {
        return None;
    }
    returns_fallible(ast, e.span)?;
    let scope = enclosing_func(ast, e.span).map_or(text, |f| slice(text, f.span));
    let mut name = "err".to_string();
    let mut i = 2;
    while is_word(scope, &name) {
        name = format!("err{i}");
        i += 1;
    }
    let (_, ind) = indent_of(text, stmt.span.start);
    let new = format!("try {} catch {name} {{\n{ind}    return {name}\n{ind}}}", slice(text, inner.span));
    Some(("Convert `?` to try/catch".into(), vec![(e.span, new)]))
}

/// Whether `x?` parses as `(x)?` for `x` of this kind.
fn postfix_safe(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Ident(_) | ExprKind::Call { .. } | ExprKind::Field { .. } | ExprKind::Index { .. } | ExprKind::Paren(_) | ExprKind::TypeArgs { .. })
}

/// `try x catch err { return err }` -> `x?`, in a `T!` function.
pub(crate) fn to_propagate(ast: &ast::File, text: &str, sel: Span) -> Option<(String, Edits)> {
    let only_returns = |e: &Expr| match &e.kind {
        ExprKind::Try { handler: TryHandler::Catch { name, body }, .. } => {
            name.name != "_"
                && matches!(&body.stmts[..], [s] if matches!(&s.kind, StmtKind::Return(v) if matches!(&v[..], [r] if matches!(&r.kind, ExprKind::Ident(i) if i.name == name.name))))
        }
        _ => false,
    };
    let e = innermost(ast, sel, only_returns)?;
    let ExprKind::Try { expr, .. } = &e.kind else { return None };
    if !returns_fallible(ast, e.span)? {
        return None;
    }
    let src = slice(text, expr.span);
    let new = if postfix_safe(expr) { format!("{src}?") } else { format!("({src})?") };
    Some(("Convert try/catch to `?`".into(), vec![(e.span, new)]))
}
