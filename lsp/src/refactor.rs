//! Refactoring code actions: extract variable (`refactor.extract`),
//! extract function (`refactor.extract`), inline variable
//! (`refactor.inline`) and organize imports (`source.organizeImports`).
//!
//! Every refactoring is computed by [`compute`] from (kind, file,
//! selection). When the client resolves code actions lazily
//! (`codeAction.resolveSupport` lists `edit`), the listed actions carry only
//! that triple as `data` and [`resolve`] computes the edit; otherwise the
//! edit is included right away.

use crate::assist::Walker;
use crate::range;
use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, TextEdit, Url, WorkspaceEdit};
use std::collections::HashMap;
use zell::ast::{self, Expr, ExprKind, Stmt, StmtKind};
use zell::diagnostics::codes;
use zell::sema::index::{short_type_name, SymbolKind};
use zell::session::Analysis;
use zell::span::{SourceFile, Span};

/// The refactorings, by id: (id, title prefix, kind).
const KINDS: &[&str] = &["extract_var", "extract_fn", "inline_var", "organize_imports"];

fn kind_of(id: &str) -> CodeActionKind {
    match id {
        "extract_var" | "extract_fn" => CodeActionKind::REFACTOR_EXTRACT,
        "inline_var" => CodeActionKind::REFACTOR_INLINE,
        _ => CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
    }
}

/// An edit in byte offsets.
type Edits = Vec<(Span, String)>;

/// The refactorings available for `sel` in `file`, filtered by `only`
/// (the requested kinds, as prefixes).
pub(crate) fn actions(a: &Analysis, file: usize, uri: &Url, sel: Span, lazy: bool, only: Option<&[CodeActionKind]>) -> Vec<CodeActionOrCommand> {
    let mut out = Vec::new();
    for id in KINDS {
        let kind = kind_of(id);
        if let Some(only) = only {
            if !only.iter().any(|k| kind.as_str() == k.as_str() || kind.as_str().starts_with(&format!("{}.", k.as_str()))) {
                continue;
            }
        }
        let Some((title, edits)) = compute(a, file, id, sel) else { continue };
        let f = &a.files[file];
        let mut action = CodeAction { title, kind: Some(kind), ..Default::default() };
        if lazy {
            action.data = Some(serde_json::json!({ "zell": "refactor", "id": id, "uri": uri, "start": sel.start, "end": sel.end }));
        } else {
            action.edit = Some(workspace_edit(f, uri, edits));
        }
        out.push(CodeActionOrCommand::CodeAction(action));
    }
    out
}

fn workspace_edit(f: &SourceFile, uri: &Url, edits: Edits) -> WorkspaceEdit {
    let edits = edits.into_iter().map(|(s, t)| TextEdit { range: range(f, s), new_text: t }).collect();
    WorkspaceEdit { changes: Some(HashMap::from([(uri.clone(), edits)])), ..Default::default() }
}

/// Fills in the edit of a lazily listed refactoring.
pub(crate) fn resolve(a: &Analysis, file_of: impl Fn(&Url) -> Option<usize>, mut action: CodeAction) -> CodeAction {
    let Some(d) = action.data.as_ref().filter(|d| d["zell"] == "refactor") else { return action };
    let (Some(id), Some(uri), Some(start), Some(end)) = (d["id"].as_str(), d["uri"].as_str().and_then(|u| Url::parse(u).ok()), d["start"].as_u64(), d["end"].as_u64()) else { return action };
    let Some(file) = file_of(&uri) else { return action };
    if let Some((_, edits)) = compute(a, file, id, Span::new(start as u32, end as u32)) {
        action.edit = Some(workspace_edit(&a.files[file], &uri, edits));
    }
    action
}

/// Computes refactoring `id` for `sel`: its title and edits.
pub(crate) fn compute(a: &Analysis, file: usize, id: &str, sel: Span) -> Option<(String, Edits)> {
    let ast = a.asts.get(file)?;
    let text = a.files[file].text.as_str();
    let sel = trim(text, sel);
    match id {
        "extract_var" => extract_var(ast, text, sel),
        "extract_fn" => extract_fn(a, file, ast, text, sel),
        "inline_var" => inline_var(a, file, ast, text, sel),
        "organize_imports" => organize_imports(a, file, ast, text),
        _ => None,
    }
}

fn trim(text: &str, s: Span) -> Span {
    let (mut a, mut b) = (s.start as usize, (s.end as usize).min(text.len()));
    while a < b && text.as_bytes()[a].is_ascii_whitespace() {
        a += 1;
    }
    while b > a && text.as_bytes()[b - 1].is_ascii_whitespace() {
        b -= 1;
    }
    Span::new(a as u32, b as u32)
}

fn slice(text: &str, s: Span) -> &str {
    &text[s.start as usize..s.end as usize]
}

/// Whitespace before the line of `at`.
fn indent_of(text: &str, at: u32) -> (u32, String) {
    let ls = text[..at as usize].rfind('\n').map_or(0, |i| i + 1);
    let ws: String = text[ls..].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    (ls as u32, ws)
}

/// Every block of the file's function bodies.
fn blocks(ast: &ast::File) -> Vec<&ast::Block> {
    let mut out = Vec::new();
    Walker { on_stmt: &mut |_| {}, on_expr: &mut |_| {}, on_block: &mut |b| out.push(b) }.file(ast);
    out
}

/// The innermost statement (directly in a block) containing `sel`.
fn enclosing_stmt(ast: &ast::File, sel: Span) -> Option<&Stmt> {
    blocks(ast).into_iter().flat_map(|b| b.stmts.iter()).filter(|s| s.span.start <= sel.start && sel.end <= s.span.end).min_by_key(|s| s.span.end - s.span.start)
}

/// The function whose body contains `sel`.
fn enclosing_func(ast: &ast::File, sel: Span) -> Option<&ast::FuncDecl> {
    let mut found = None;
    for d in &ast.decls {
        let fs: Vec<&ast::FuncDecl> = match d {
            ast::Decl::Func(f) => vec![f],
            ast::Decl::Struct(s) => s.methods.iter().collect(),
            ast::Decl::Enum(e) => e.methods.iter().collect(),
            _ => vec![],
        };
        for f in fs {
            if f.body.as_ref().is_some_and(|b| b.span.start <= sel.start && sel.end <= b.span.end) {
                found = Some(f);
            }
        }
    }
    found
}

fn is_word(text: &str, w: &str) -> bool {
    text.match_indices(w).any(|(i, _)| {
        let before = text[..i].chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after = text[i + w.len()..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_');
        !before && !after
    })
}

/// A name for the value of `e`: from the callee or field (`GetUser()` ->
/// `user`), else `value`; made unique within `scope`.
fn name_for(e: &Expr, scope: &str) -> String {
    let base = match &e.kind {
        ExprKind::Call { callee, .. } => match &callee.kind {
            ExprKind::Ident(i) => i.name.clone(),
            ExprKind::Field { name, .. } => name.name.clone(),
            _ => String::new(),
        },
        ExprKind::Field { name, .. } => name.name.clone(),
        _ => String::new(),
    };
    let base = base.strip_prefix("Get").filter(|s| !s.is_empty()).unwrap_or(&base).to_string();
    let mut name: String = match base.chars().next() {
        Some(c) => c.to_lowercase().chain(base.chars().skip(1)).collect(),
        None => "value".into(),
    };
    if zell::token::TokenKind::keyword(&name).is_some() {
        name = "value".into();
    }
    let mut n = name.clone();
    let mut i = 2;
    while is_word(scope, &n) {
        n = format!("{name}{i}");
        i += 1;
    }
    n
}

fn extract_var(ast: &ast::File, text: &str, sel: Span) -> Option<(String, Edits)> {
    if sel.start == sel.end {
        return None;
    }
    let mut expr = None;
    Walker {
        on_stmt: &mut |_| {},
        on_expr: &mut |e| {
            if e.span == sel && expr.is_none() {
                expr = Some(e);
            }
        },
        on_block: &mut |_| {},
    }
    .file(ast);
    let e = expr?;
    if matches!(e.kind, ExprKind::Ident(_)) {
        return None;
    }
    let stmt = enclosing_stmt(ast, sel)?;
    // Not the left side of an assignment.
    if let StmtKind::Assign { target, .. } = &stmt.kind {
        if target.span.start <= sel.start && sel.end <= target.span.end {
            return None;
        }
    }
    let func = enclosing_func(ast, sel)?;
    let name = name_for(e, slice(text, func.span));
    let (ls, ws) = indent_of(text, stmt.span.start);
    let edits = vec![(Span::new(ls, ls), format!("{ws}var {name} = {}\n", slice(text, sel))), (sel, name.clone())];
    Some((format!("Extract variable {name}"), edits))
}

fn inline_var(a: &Analysis, file: usize, ast: &ast::File, text: &str, sel: Span) -> Option<(String, Edits)> {
    let id = a.symbol_at(file, sel.start)?;
    let sym = a.index.symbol(id);
    if sym.kind != SymbolKind::Local {
        return None;
    }
    let loc = sym.loc?;
    let mut decl = None;
    for b in blocks(ast) {
        for s in &b.stmts {
            if let StmtKind::Var(v) = &s.kind {
                if v.names.len() == 1 && v.names[0].span == loc.name_span {
                    decl = Some((s, v.init.as_ref()?));
                }
            }
        }
    }
    let (stmt, init) = decl?;
    let refs = a.references_of(id);
    if refs.is_empty() {
        return None;
    }
    for r in &refs {
        let after = text[r.span.end as usize..].trim_start();
        let assigned = (after.starts_with('=') && !after.starts_with("==")) || ["+=", "-=", "*=", "/=", "%=", "++", "--", "|=", "&=", "^=", "<<=", ">>="].iter().any(|op| after.starts_with(op));
        if assigned || text[..r.span.start as usize].ends_with('&') {
            return None;
        }
    }
    let src = slice(text, init.span);
    let simple = matches!(init.kind, ExprKind::Ident(_) | ExprKind::Call { .. } | ExprKind::Field { .. } | ExprKind::Index { .. }) || !src.contains(' ');
    let value = if simple { src.to_string() } else { format!("({src})") };
    let mut edits: Edits = refs.iter().map(|r| (r.span, value.clone())).collect();
    edits.push((whole_lines(text, stmt.span), String::new()));
    Some((format!("Inline variable {}", sym.name), edits))
}

fn whole_lines(text: &str, s: Span) -> Span {
    let ls = text[..s.start as usize].rfind('\n').map_or(0, |i| i + 1);
    let le = text[s.end as usize..].find('\n').map_or(text.len(), |i| s.end as usize + i + 1);
    if text[ls..s.start as usize].trim().is_empty() && text[s.end as usize..le].trim().is_empty() {
        Span::new(ls as u32, le as u32)
    } else {
        s
    }
}

fn organize_imports(a: &Analysis, file: usize, ast: &ast::File, text: &str) -> Option<(String, Edits)> {
    let first = ast.imports.first()?;
    let last = ast.imports.last()?;
    let unused: Vec<Span> = a.diagnostics_for(file).filter(|d| d.code == Some(codes::UNUSED_IMPORT)).map(|d| d.span).collect();
    let mut lines: Vec<String> = ast
        .imports
        .iter()
        .filter(|i| !unused.iter().any(|u| i.span.start <= u.start && u.end <= i.span.end))
        .map(|i| slice(text, i.span).trim().to_string())
        .collect();
    lines.sort_by(|x, y| x.trim_start_matches("import ").cmp(y.trim_start_matches("import ")));
    lines.dedup();
    let span = whole_lines(text, Span::new(first.span.start, last.span.end));
    let mut new_text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    if lines.is_empty() && text[span.end as usize..].starts_with('\n') {
        new_text.clear();
        return Some(("Organize imports".into(), vec![(Span::new(span.start, span.end + 1), new_text)]));
    }
    if new_text == slice(text, span) {
        return None;
    }
    Some(("Organize imports".into(), vec![(span, new_text)]))
}

/// Extracts the statements fully inside `sel` (in one block) into a new
/// function placed after the enclosing declaration. Parameters are the
/// locals and parameters declared outside the range that it uses;
/// locals declared in the range and used after it are returned (one as a
/// value, several as a tuple). Ranges with `return`, `break` or
/// `continue` are not offered. `await` makes the function `async` (called
/// with `await`), `?` makes it fallible (called with `?`).
fn extract_fn(a: &Analysis, file: usize, ast: &ast::File, text: &str, sel: Span) -> Option<(String, Edits)> {
    let block = blocks(ast).into_iter().filter(|b| b.span.start <= sel.start && sel.end <= b.span.end).min_by_key(|b| b.span.end - b.span.start)?;
    let stmts: Vec<&Stmt> = block.stmts.iter().filter(|s| sel.start <= s.span.start && s.span.end <= sel.end).collect();
    let (first, last) = (stmts.first()?, stmts.last()?);
    // The selection must not cut a statement.
    if block.stmts.iter().any(|s| s.span.start < sel.end && sel.start < s.span.end && !(sel.start <= s.span.start && s.span.end <= sel.end)) {
        return None;
    }
    let range = Span::new(first.span.start, last.span.end);
    let body = slice(text, range);
    let mut jumps = false;
    for s in &stmts {
        Walker {
            on_stmt: &mut |s| {
                if matches!(s.kind, StmtKind::Return(_) | StmtKind::Break(_) | StmtKind::Continue(_)) {
                    jumps = true;
                }
            },
            on_expr: &mut |_| {},
            on_block: &mut |_| {},
        }
        .stmt(s);
    }
    if jumps {
        return None;
    }
    let func = enclosing_func(ast, range)?;
    let fspan = func.span;
    let mut params: Vec<(String, String)> = Vec::new();
    let mut results: Vec<(String, String)> = Vec::new();
    let ty_of = |id| a.index.symbol(id).ty.map(|t| short_type_name(&a.hir, t)).unwrap_or_else(|| "int".into());
    for r in a.index.references.iter().filter(|r| r.file == file) {
        let sym = a.index.symbol(r.symbol);
        if !matches!(sym.kind, SymbolKind::Local | SymbolKind::Param) || sym.name == "this" {
            continue;
        }
        let Some(loc) = sym.loc else { continue };
        let inside_use = range.start <= r.span.start && r.span.end <= range.end;
        let declared_inside = range.start <= loc.name_span.start && loc.name_span.end <= range.end;
        if inside_use && !declared_inside && loc.name_span.start >= fspan.start && !params.iter().any(|p| p.0 == sym.name) {
            params.push((sym.name.clone(), ty_of(r.symbol)));
        }
        if declared_inside && r.span.start >= range.end && r.span.end <= fspan.end && !results.iter().any(|p| p.0 == sym.name) {
            results.push((sym.name.clone(), ty_of(r.symbol)));
        }
    }
    let is_async = is_word(body, "await");
    let fallible = body.contains(")?") || body.contains("?\n") || body.contains("? ");
    let mut name = "extracted".to_string();
    let mut i = 2;
    while is_word(text, &name) {
        name = format!("extracted{i}");
        i += 1;
    }
    let ret = match results.len() {
        0 => "void".to_string(),
        1 => results[0].1.clone(),
        _ => format!("({})", results.iter().map(|r| r.1.clone()).collect::<Vec<_>>().join(", ")),
    };
    let ret_sig = match (results.is_empty(), fallible) {
        (true, false) => String::new(),
        (true, true) => " void!".into(),
        (false, f) => format!(" {ret}{}", if f { "!" } else { "" }),
    };
    let (_, ws) = indent_of(text, range.start);
    let (_, fws) = indent_of(text, fspan.start);
    let body_lines: String = body
        .lines()
        .enumerate()
        .map(|(i, l)| {
            let l = if i == 0 { l.to_string() } else { l.strip_prefix(ws.as_str()).unwrap_or(l).to_string() };
            format!("{fws}    {l}\n")
        })
        .collect();
    let ret_line = match results.len() {
        0 => String::new(),
        1 => format!("{fws}    return {}\n", results[0].0),
        _ => format!("{fws}    return ({})\n", results.iter().map(|r| r.0.clone()).collect::<Vec<_>>().join(", ")),
    };
    let is_method = ast.decls.iter().any(|d| matches!(d, ast::Decl::Struct(s) if s.methods.iter().any(|m| m.span == fspan)));
    let sig_params = params.iter().map(|(n, t)| format!("{n} {t}")).collect::<Vec<_>>().join(", ");
    let new_fn = format!("\n\n{fws}{}func {name}({sig_params}){ret_sig} {{\n{body_lines}{ret_line}{fws}}}", if is_async { "async " } else { "" });
    let args = params.iter().map(|p| p.0.clone()).collect::<Vec<_>>().join(", ");
    let recv = if is_method { "this." } else { "" };
    let mut call = format!("{recv}{name}({args})");
    if is_async {
        call = format!("await {call}");
    }
    if fallible {
        call.push('?');
    }
    let call_stmt = match results.len() {
        0 => call,
        1 => format!("var {} = {call}", results[0].0),
        _ => format!("var ({}) = {call}", results.iter().map(|r| r.0.clone()).collect::<Vec<_>>().join(", ")),
    };
    Some((format!("Extract function {name}"), vec![(range, call_stmt), (Span::new(fspan.end, fspan.end), new_fn)]))
}
