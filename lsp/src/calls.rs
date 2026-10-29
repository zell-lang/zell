//! Call-site helpers shared by the workspace refactorings: inline function
//! (`inline_fn.rs`) and change signature (`signature.rs`).

use crate::assist::Walker;
use crate::refactor::Edits;
use zell::ast::{self, Expr, ExprKind, UnaryOp};
use zell::session::Analysis;
use zell::span::Span;

/// Edits of several files (file index, its edits).
pub(crate) type WsEdits = Vec<(usize, Edits)>;

/// Every expression of `f`, in pre-order.
pub(crate) fn exprs(f: &ast::File) -> Vec<&Expr> {
    let mut out = Vec::new();
    Walker { on_stmt: &mut |_| {}, on_expr: &mut |e| out.push(e), on_block: &mut |_| {} }.file(f);
    out
}

/// The name span of a callee: `f`, `x.f`, `f<T>`.
pub(crate) fn callee_name(e: &Expr) -> Option<Span> {
    match &e.kind {
        ExprKind::Ident(i) => Some(i.span),
        ExprKind::Field { name, .. } => Some(name.span),
        ExprKind::TypeArgs { base, .. } => callee_name(base),
        _ => None,
    }
}

/// The call whose callee is named at `name`.
pub(crate) fn call_named<'a>(exprs: &[&'a Expr], name: Span) -> Option<&'a Expr> {
    exprs.iter().copied().find(|e| matches!(&e.kind, ExprKind::Call { callee, .. } if callee_name(callee) == Some(name)))
}

/// The innermost expression strictly around `inner` (by span).
pub(crate) fn parent<'a>(exprs: &[&'a Expr], inner: Span) -> Option<&'a Expr> {
    exprs.iter().copied().filter(|e| e.span.start <= inner.start && inner.end <= e.span.end && e.span != inner).min_by_key(|e| e.span.end - e.span.start)
}

/// Evaluating `e` has no side effects (no calls, `await`, `?`, `try`,
/// allocation or index that may panic) and reads only names: it may be
/// dropped, repeated or moved.
pub(crate) fn is_pure(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Char(_) | ExprKind::Bool(_) | ExprKind::Nil | ExprKind::This | ExprKind::Ident(_) | ExprKind::Lambda { .. } => true,
        ExprKind::Field { base, .. } | ExprKind::Paren(base) => is_pure(base),
        ExprKind::Unary { op, expr } => *op != UnaryOp::Deref && is_pure(expr),
        ExprKind::Binary { left, right, .. } => is_pure(left) && is_pure(right),
        ExprKind::StructLit { fields, .. } => fields.iter().all(|f| is_pure(&f.value)),
        ExprKind::SliceLit(es) | ExprKind::Tuple(es) => es.iter().all(is_pure),
        _ => false,
    }
}

/// A literal, a name or `this`: substituting it for a parameter is
/// exactly passing it.
pub(crate) fn is_trivial(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Char(_) | ExprKind::Bool(_) | ExprKind::Nil | ExprKind::This | ExprKind::Ident(_))
}

/// A literal (untyped constant: a temporary needs the parameter's type).
pub(crate) fn is_literal(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Char(_) | ExprKind::Bool(_) | ExprKind::Nil)
}

/// `e` binds tighter than any operator: it needs no parentheses where an
/// operand is expected.
pub(crate) fn is_atomic(e: &Expr, src: &str) -> bool {
    matches!(
        e.kind,
        ExprKind::Ident(_) | ExprKind::This | ExprKind::Call { .. } | ExprKind::Field { .. } | ExprKind::Index { .. } | ExprKind::Paren(_) | ExprKind::StructLit { .. } | ExprKind::SliceLit(_) | ExprKind::Tuple(_)
    ) || (!src.contains(' ') && !matches!(e.kind, ExprKind::Unary { .. } | ExprKind::Await(_) | ExprKind::Lambda { .. }))
}

/// The function or method declared with name at `name` (interface
/// methods included).
pub(crate) fn find_func(f: &ast::File, name: Span) -> Option<&ast::FuncDecl> {
    for d in &f.decls {
        let fs: &[ast::FuncDecl] = match d {
            ast::Decl::Func(fd) => std::slice::from_ref(fd),
            ast::Decl::Struct(s) => &s.methods,
            ast::Decl::Enum(e) => &e.methods,
            ast::Decl::Interface(i) => &i.methods,
            _ => &[],
        };
        if let Some(fd) = fs.iter().find(|fd| fd.name.span == name) {
            return Some(fd);
        }
    }
    None
}

/// The span inside the parentheses of the first `(` at or after `from`
/// (nested brackets skipped).
pub(crate) fn inside_parens(text: &str, from: u32) -> Option<Span> {
    let open = from as usize + text.get(from as usize..)?.find('(')?;
    let mut depth = 0i32;
    for (i, c) in text[open..].char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(Span::new(open as u32 + 1, (open + i) as u32));
                }
            }
            _ => {}
        }
    }
    None
}

/// `file.zl:12` for messages.
pub(crate) fn place(a: &Analysis, file: usize, at: u32) -> String {
    let f = &a.files[file];
    let name = std::path::Path::new(&f.path).file_name().map_or_else(|| f.path.clone(), |n| n.to_string_lossy().into_owned());
    format!("{name}:{}", f.line_col(at).0)
}

/// The text before `span` ends with `.`: a member or qualified name.
pub(crate) fn qualified(text: &str, span: Span) -> bool {
    text[..span.start as usize].trim_end_matches([' ', '\t']).ends_with('.')
}

/// Adds `(file, edits)` to `out`, merging with the file's earlier edits.
pub(crate) fn push(out: &mut WsEdits, file: usize, edits: Edits) {
    match out.iter_mut().find(|(f, _)| *f == file) {
        Some((_, e)) => e.extend(edits),
        None => out.push((file, edits)),
    }
}

/// Whether two edits of one file overlap (they would garble the text).
pub(crate) fn overlapping(edits: &Edits) -> bool {
    let mut s: Vec<Span> = edits.iter().map(|e| e.0).collect();
    s.sort_by_key(|s| (s.start, s.end));
    s.windows(2).any(|w| w[1].start < w[0].end)
}
