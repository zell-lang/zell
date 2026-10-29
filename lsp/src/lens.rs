//! `textDocument/codeLens` and `codeLens/resolve`.
//!
//! Lenses (commands executed by the client, see editors/vscode):
//! - on each `@test` function: "▶ Run test" (`zell.runTest`) and
//!   "Debug test" (`zell.debugTest`), arguments `[file uri, test name]`;
//! - on `main`: "▶ Run" (`zell.run`, argument `[file uri]`);
//! - on each exported top-level declaration: "N references", computed
//!   lazily by `codeLens/resolve` (command `zell.showReferences` with
//!   `[uri, position, locations]`, like VS Code's `editor.action.showReferences`).

use crate::range;
use lsp_types::{CodeLens, Command, Location, Url};
use zell::ast;
use zell::sema::index::SymbolKind;
use zell::session::Analysis;
use zell::span::Span;

fn exported(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

fn command(title: &str, cmd: &str, args: Vec<serde_json::Value>) -> Option<Command> {
    Some(Command { title: title.to_string(), command: cmd.to_string(), arguments: Some(args) })
}

/// The lenses of `file` (reference counts unresolved: `command` is `None`
/// and `data` names the declaration).
pub(crate) fn code_lenses(a: &Analysis, file: usize, uri: &Url) -> Vec<CodeLens> {
    let Some(ast) = a.asts.get(file) else { return Vec::new() };
    let f = &a.files[file];
    let mut out = Vec::new();
    let refs_lens = |name: &ast::Ident, out: &mut Vec<CodeLens>| {
        if !exported(&name.name) {
            return;
        }
        let Some(id) = a.index.declaration_at(file, name.span.start) else { return };
        let s = a.index.symbol(id);
        out.push(CodeLens {
            range: range(f, name.span),
            command: None,
            data: Some(serde_json::json!({"uri": uri, "offset": name.span.start, "fullName": &*s.full_name})),
        });
    };
    for d in &ast.decls {
        match d {
            ast::Decl::Func(func) => {
                let is_test = func.attrs.iter().any(|x| matches!(x.path.to_string().as_str(), "test" | "bench"));
                let r = range(f, func.name.span);
                if is_test && func.generics.is_empty() {
                    let args = vec![serde_json::json!(uri), serde_json::json!(func.name.name)];
                    out.push(CodeLens { range: r, command: command("▶ Run test", "zell.runTest", args.clone()), data: None });
                    out.push(CodeLens { range: r, command: command("Debug test", "zell.debugTest", args), data: None });
                } else if func.name.name == "main" && func.params.is_empty() && func.body.is_some() {
                    out.push(CodeLens { range: r, command: command("▶ Run", "zell.run", vec![serde_json::json!(uri)]), data: None });
                }
                if !is_test {
                    refs_lens(&func.name, &mut out);
                }
            }
            ast::Decl::Struct(s) => refs_lens(&s.name, &mut out),
            ast::Decl::Interface(i) => refs_lens(&i.name, &mut out),
            ast::Decl::Enum(e) => refs_lens(&e.name, &mut out),
            ast::Decl::Const(c) => refs_lens(&c.name, &mut out),
            ast::Decl::TypeAlias(_) => {}
            ast::Decl::Var(v) => {
                if let [n] = v.names.as_slice() {
                    refs_lens(n, &mut out);
                }
            }
            ast::Decl::Error(_) => {}
        }
    }
    out
}

/// `codeLens/resolve`: counts the references of the declaration named by
/// the lens' `data` (looked up by full name, so a lens computed against an
/// older analysis still resolves). `uri_of` maps file indices to URIs.
pub(crate) fn resolve_lens(a: &Analysis, mut lens: CodeLens, file_of: impl Fn(&Url) -> Option<usize>, uri_of: impl Fn(usize) -> Option<Url>) -> CodeLens {
    let Some(data) = lens.data.clone() else { return lens };
    let Some(uri) = data.get("uri").and_then(|u| serde_json::from_value::<Url>(u.clone()).ok()) else { return lens };
    let full = data.get("fullName").and_then(|v| v.as_str()).unwrap_or("");
    let offset = data.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let file = file_of(&uri);
    let id = a
        .index
        .symbols
        .iter()
        .find(|s| *s.full_name == *full && s.kind != SymbolKind::Local && s.loc.is_some_and(|l| Some(l.file) == file))
        .map(|s| s.id)
        .or_else(|| file.and_then(|f| a.index.declaration_at(f, offset)));
    let locations: Vec<Location> = id
        .map(|id| a.references_of(id).into_iter().filter_map(|r| Some(Location { uri: uri_of(r.file)?, range: range(&a.files[r.file], r.span) })).collect())
        .unwrap_or_default();
    let n = locations.len();
    let title = if n == 1 { "1 reference".to_string() } else { format!("{n} references") };
    let at = id.and_then(|id| a.index.symbol(id).loc).map(|l| range(&a.files[l.file], Span::new(l.name_span.start, l.name_span.start)).start).unwrap_or(lens.range.start);
    lens.command = command(&title, "zell.showReferences", vec![serde_json::json!(uri), serde_json::json!(at), serde_json::json!(locations)]);
    lens
}
