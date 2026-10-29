//! The MCP tools: their schemas ([`list`]) and implementations ([`call`]).
//!
//! Every tool returns text for an LLM: `path:line:col` locations, the
//! source line, unified diffs, at most a few hundred entries.

use crate::shell;
use crate::workspace::{self, offset_of, Workspace};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use zell::diagnostics::{Severity, TextEdit};
use zell::sema::index::{SymbolId, SymbolKind};
use zell::session::{apply_edits, Analysis, DocumentSymbol};
use zell::span::Span;

/// A tool's result text, or the reason it failed (`isError`).
pub type ToolResult = Result<String, String>;

/// Most diagnostics, references or symbols listed in one result.
const MAX_ITEMS: usize = 200;

/// Lines of a declaration shown by `definition`.
const SNIPPET_LINES: usize = 12;

fn target_props(what: &str) -> Value {
    json!({
        "path": { "type": "string", "description": format!("File containing the {what} (relative to the project root or absolute)") },
        "line": { "type": "integer", "minimum": 1, "description": "1-based line" },
        "col": { "type": "integer", "minimum": 1, "description": "1-based column (characters); optional when `name` is given" },
        "name": { "type": "string", "description": "The identifier on `line` (used when `col` is missing or off)" },
        "symbol": { "type": "string", "description": "Instead of a position: a symbol name such as `User`, `User.Hello`, `app.models.User` or `strings.Split`" }
    })
}

fn with(mut base: Value, extra: Value) -> Value {
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    base
}

fn tool(name: &str, title: &str, description: &str, props: Value, required: &[&str], read_only: bool) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required, "additionalProperties": false },
        "annotations": { "title": title, "readOnlyHint": read_only, "destructiveHint": false, "openWorldHint": false }
    })
}

/// The `tools/list` entries.
pub fn list() -> Vec<Value> {
    let path_only = |d: &str| json!({ "path": { "type": "string", "description": d } });
    vec![
        tool(
            "diagnostics",
            "Zell diagnostics",
            "Type-check Zell code as it is on disk and list errors and warnings as `path:line:col: error[CODE]: message`, with the source line, notes and available quick fixes. Without `path`, checks the whole project. Run it after every edit of .zl files.",
            path_only("A .zl file; omit for the whole project"),
            &[],
            true,
        ),
        tool("definition", "Go to definition", "Where a Zell symbol is declared: location, signature and the start of the declaration. Give a position (`path` + `line` + `col` or `name`) or a `symbol` name.", target_props("use of the symbol"), &[], true),
        tool(
            "references",
            "Find references",
            "Every use of a Zell symbol in the project (and the declaration), one `path:line:col: source line` per use.",
            with(target_props("symbol"), json!({ "include_declaration": { "type": "boolean", "description": "List the declaration too (default true)" } })),
            &[],
            true,
        ),
        tool("hover", "Symbol info", "Signature, kind, container, type and doc comment of a Zell symbol (and a std example when there is one).", target_props("symbol"), &[], true),
        tool("document_symbols", "File outline", "The outline of a .zl file: namespace, types, functions, fields and methods with their lines and signatures.", path_only("A .zl file"), &["path"], true),
        tool(
            "workspace_symbols",
            "Search symbols",
            "Find declarations in the project by name (case-insensitive fuzzy match: `usrnew` finds `UserNew`).",
            json!({
                "query": { "type": "string", "description": "Name or part of it" },
                "limit": { "type": "integer", "minimum": 1, "description": "Most results (default 50)" },
                "include_libraries": { "type": "boolean", "description": "Also search std and dependencies (default false)" }
            }),
            &["query"],
            true,
        ),
        tool(
            "rename",
            "Rename symbol",
            "Rename a Zell symbol everywhere it is used, semantically (not by text). A dry run by default: returns the edits as unified diffs; pass `apply: true` to write the files. Std and dependency symbols cannot be renamed.",
            with(
                target_props("symbol"),
                json!({
                    "new_name": { "type": "string", "description": "The new identifier" },
                    "apply": { "type": "boolean", "description": "Write the changes (default false: dry run)" }
                }),
            ),
            &["new_name"],
            false,
        ),
        tool(
            "format",
            "Format code",
            "Format .zl files in the canonical style (`zell fmt`, no options). Rewrites the files and returns what changed as unified diffs; `apply: false` only reports. Without `path`, formats the whole project.",
            json!({
                "path": { "type": "string", "description": "A .zl file or a directory; omit for the whole project" },
                "apply": { "type": "boolean", "description": "Write the formatted files (default true)" }
            }),
            &[],
            false,
        ),
        tool(
            "run_tests",
            "Run tests",
            "Run the `@test` functions and examples with `zell test --json` and summarise: counts, then each failure with its output. Builds the code, so it also reports compile errors.",
            json!({
                "path": { "type": "string", "description": "Package directory, directory or file (default: the project)" },
                "filter": { "type": "string", "description": "Regular expression on test names (`^TestParse`, `Sum$`)" },
                "timeout_sec": { "type": "integer", "minimum": 1, "description": "Kill after this many seconds (default 300)" }
            }),
            &[],
            false,
        ),
        tool(
            "build",
            "Build",
            "Build the project, a directory or a file with `zell build` and report errors (or the output path).",
            json!({
                "path": { "type": "string", "description": "Project directory, directory or file (default: the project)" },
                "release": { "type": "boolean", "description": "Optimised build (default false)" }
            }),
            &[],
            false,
        ),
        tool(
            "run",
            "Run program",
            "Build and run a program with `zell run`; returns its exit code, stdout and stderr. stdin is closed.",
            json!({
                "path": { "type": "string", "description": "Project directory, directory or file (default: the project)" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Program arguments" },
                "leak_check": { "type": "boolean", "description": "Report objects still alive at exit (--leak-check)" },
                "timeout_sec": { "type": "integer", "minimum": 1, "description": "Kill after this many seconds (default 60)" }
            }),
            &[],
            false,
        ),
        tool(
            "doc",
            "Documentation",
            "Documentation from doc comments, like `zell doc`: a namespace (`strings`, `net.http`), a symbol (`strings.Split`, `task.Channel.Send`), a project namespace, or `std` for the list of std namespaces. Without `symbol`, the project's own API.",
            json!({
                "symbol": { "type": "string", "description": "Namespace or symbol (default: the project)" },
                "private": { "type": "boolean", "description": "Include unexported names (default false)" }
            }),
            &[],
            true,
        ),
        tool(
            "std_index",
            "Search the stdlib",
            "Search the standard library declarations by name: kind, full name, signature, summary and source location. An empty query lists the std namespaces.",
            json!({
                "query": { "type": "string", "description": "Name or part of it (`Split`, `strings.Trim`, `http.Get`)" },
                "limit": { "type": "integer", "minimum": 1, "description": "Most results (default 30)" }
            }),
            &[],
            true,
        ),
    ]
}

/// Runs tool `name`; `None` when there is no such tool.
pub fn call(ws: &mut Workspace, name: &str, args: &Value) -> Option<ToolResult> {
    Some(match name {
        "diagnostics" => diagnostics(ws, args),
        "definition" => definition(ws, args),
        "references" => references(ws, args),
        "hover" => hover(ws, args),
        "document_symbols" => document_symbols(ws, args),
        "workspace_symbols" => workspace_symbols(ws, args),
        "rename" => rename(ws, args),
        "format" => format(ws, args),
        "run_tests" => shell::run_tests(ws, args),
        "build" => shell::build(ws, args),
        "run" => shell::run(ws, args),
        "doc" => doc(ws, args),
        "std_index" => std_index(ws, args),
        _ => return None,
    })
}

pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

pub(crate) fn int_arg(args: &Value, key: &str) -> Option<usize> {
    let v = args.get(key)?;
    v.as_u64().map(|n| n as usize).or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

pub(crate) fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true"))).unwrap_or(default)
}

fn kind_word(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Namespace => "namespace",
        SymbolKind::Struct => "struct",
        SymbolKind::TypeAlias => "type alias",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::Variant => "enum variant",
        SymbolKind::Field => "field",
        SymbolKind::Method => "method",
        SymbolKind::Func => "function",
        SymbolKind::Const => "constant",
        SymbolKind::Global => "global variable",
        SymbolKind::Local => "local variable",
        SymbolKind::Param => "parameter",
        SymbolKind::BuiltinType => "builtin type",
        SymbolKind::BuiltinFunc => "builtin function",
    }
}

fn plural(n: usize, word: &str) -> String {
    let suffix = match () {
        _ if n == 1 => "",
        _ if word.ends_with("ch") || word.ends_with('s') => "es",
        _ => "s",
    };
    format!("{n} {word}{suffix}")
}

/// A unified diff of an edit (not a formatting) of `path`.
fn edit_diff(path: &str, old: &str, new: &str) -> String {
    zell::fmt::unified_diff(path, old, new).replacen(&format!("+++ {path} (formatted)"), &format!("+++ {path}"), 1)
}

/// Whether a std declaration is public API: an exported name, and for a
/// member, of an exported type.
fn std_public(d: &zell::stdindex::StdDecl) -> bool {
    let segs: Vec<&str> = d.full_name.split('.').collect();
    let member = matches!(d.kind, SymbolKind::Field | SymbolKind::Method | SymbolKind::Variant);
    zell::doc::is_exported(&d.name) && (!member || segs.len() < 2 || zell::doc::is_exported(segs[segs.len() - 2]))
}

// -- targets -----------------------------------------------------------------

/// A symbol a tool works on.
struct Found {
    a: Arc<Analysis>,
    id: SymbolId,
    /// Other symbols the name could mean (for a `symbol` query).
    others: Vec<SymbolId>,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offsets (within `line`) of whole-word occurrences of `name`.
fn word_offsets(line: &str, name: &str) -> Vec<usize> {
    line.match_indices(name)
        .filter(|(i, _)| {
            let before = line[..*i].chars().next_back();
            let after = line[i + name.len()..].chars().next();
            !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char)
        })
        .map(|(i, _)| i)
        .collect()
}

/// The symbol named by `args`: `symbol`, or `path` + `line` + `col`/`name`.
fn target(ws: &mut Workspace, args: &Value) -> Result<Found, String> {
    if let Some(q) = str_arg(args, "symbol") {
        return by_name(ws, q);
    }
    let path = str_arg(args, "path").ok_or("pass `path` with `line` and `col` (or `name`), or a `symbol` name")?;
    let line = int_arg(args, "line").ok_or("`line` is required with `path` (or pass `symbol`)")?;
    let col = int_arg(args, "col");
    let name = str_arg(args, "name");
    if col.is_none() && name.is_none() {
        return Err("pass `col` or `name` with `line`".into());
    }
    let (a, file) = ws.file(path)?;
    let f = &a.files[file];
    let mut tries: Vec<u32> = Vec::new();
    if let Some(c) = col {
        let off = offset_of(f, line, c)?;
        tries.push(off);
        if c > 1 {
            // just after the name, where a cursor would be
            tries.push(off - 1);
        }
    }
    if let Some(n) = name {
        let text = f.line_text(offset_of(f, line, 1).map(|_| line)?);
        let start = f.line_start(line - 1) as usize;
        let want = col.map(|c| text.char_indices().nth(c.saturating_sub(1)).map_or(text.len(), |(i, _)| i)).unwrap_or(0);
        let mut offs = word_offsets(text, n);
        offs.sort_by_key(|o| o.abs_diff(want));
        // a dotted name (`u.Hello`): its last segment
        let last = n.rsplit('.').next().unwrap_or(n).len();
        tries.extend(offs.into_iter().map(|o| (start + o + n.len() - last) as u32));
    }
    for off in tries {
        if let Some(id) = a.symbol_at(file, off) {
            return Ok(Found { a: a.clone(), id, others: Vec::new() });
        }
    }
    let at = col.map_or(String::new(), |c| format!(":{c}"));
    Err(format!("no symbol at {}:{line}{at}{}: `{}`", ws.rel(&f.path), name.map_or(String::new(), |n| format!(" named `{n}`")), f.line_text(line).trim()))
}

/// The best match for a symbol name: exact full name, then a full-name
/// suffix (`User.Hello`), then a short name; project before libraries.
fn by_name(ws: &mut Workspace, q: &str) -> Result<Found, String> {
    let a = ws.analysis();
    let q = q.trim().trim_end_matches("()");
    let suffix = format!(".{q}");
    let mut hits: Vec<(usize, SymbolId)> = Vec::new();
    for s in &a.index.symbols {
        if matches!(s.kind, SymbolKind::Local | SymbolKind::Param) {
            continue;
        }
        let rank = if &*s.full_name == q {
            0
        } else if s.full_name.ends_with(&suffix) {
            1
        } else if &*s.name == q {
            2
        } else {
            continue;
        };
        let library = s.is_std || s.loc.is_none_or(|l| a.is_read_only(l.file));
        hits.push((rank * 2 + usize::from(library), s.id));
    }
    hits.sort_by_key(|h| (h.0, a.index.symbol(h.1).full_name.len()));
    let Some(&(best, id)) = hits.first() else {
        return Err(format!("no symbol named `{q}` in the project or the analysed std namespaces (try `workspace_symbols` or `std_index`)"));
    };
    let others = hits.iter().skip(1).filter(|h| h.0 == best).map(|h| h.1).take(5).collect();
    Ok(Found { a, id, others })
}

fn others_note(ws: &Workspace, f: &Found) -> String {
    if f.others.is_empty() {
        return String::new();
    }
    let mut s = String::from("\nOther matches (pass a fuller name or a position):\n");
    for id in &f.others {
        let o = f.a.index.symbol(*id);
        let at = o.loc.map(|l| format!("  {}", ws.loc(&f.a, l.file, l.name_span))).unwrap_or_default();
        let _ = writeln!(s, "  {} {}{at}", kind_word(o.kind), o.full_name);
    }
    s
}

// -- tools -------------------------------------------------------------------

fn diagnostics(ws: &mut Workspace, args: &Value) -> ToolResult {
    let (a, files) = match str_arg(args, "path") {
        Some(p) => {
            let (a, f) = ws.file(p)?;
            (a, vec![f])
        }
        None => {
            let a = ws.analysis();
            let fs = ws.project_files(&a);
            if fs.is_empty() {
                return Err(format!("no .zl files under {}", ws.root.display()));
            }
            (a, fs)
        }
    };
    let mut ds: Vec<&zell::diagnostics::Diagnostic> = a.diagnostics.iter().filter(|d| files.contains(&d.file)).collect();
    ds.sort_by_key(|d| (d.file, d.span.start, d.severity == Severity::Warning));
    let errors = ds.iter().filter(|d| d.severity == Severity::Error).count();
    if ds.is_empty() {
        return Ok(format!("No errors or warnings in {}.", plural(files.len(), "file")));
    }
    let nfiles = ds.iter().map(|d| d.file).collect::<std::collections::BTreeSet<_>>().len();
    let mut out = format!("{}, {} in {}:\n", plural(errors, "error"), plural(ds.len() - errors, "warning"), plural(nfiles, "file"));
    for d in ds.iter().take(MAX_ITEMS) {
        let sev = if d.severity == Severity::Error { "error" } else { "warning" };
        let code = d.code.map(|c| format!("[{c}]")).unwrap_or_default();
        let _ = writeln!(out, "{}: {sev}{code}: {}", ws.loc(&a, d.file, d.span), d.message);
        let _ = writeln!(out, "    | {}", Workspace::line_of(&a, d.file, d.span));
        for (note, span) in &d.notes {
            match span {
                Some(s) => {
                    let _ = writeln!(out, "    note: {note} ({})", ws.loc(&a, d.file, *s));
                }
                None => {
                    let _ = writeln!(out, "    note: {note}");
                }
            }
        }
        for fix in &d.fixes {
            let _ = writeln!(out, "    fix: {}", fix.title);
        }
    }
    if ds.len() > MAX_ITEMS {
        let _ = writeln!(out, "... and {} more", ds.len() - MAX_ITEMS);
    }
    Ok(out)
}

fn definition(ws: &mut Workspace, args: &Value) -> ToolResult {
    let found = target(ws, args)?;
    let a = &found.a;
    let s = a.index.symbol(found.id);
    let mut out = format!("{} {}\n", kind_word(s.kind), s.full_name);
    match s.loc {
        Some(l) => {
            let _ = writeln!(out, "{}", ws.loc(a, l.file, l.name_span));
            let f = &a.files[l.file];
            let (first, _) = f.line_col(l.full_span.start.min(l.name_span.start));
            let (last, _) = f.line_col(l.full_span.end.max(l.name_span.end));
            let shown = last.min(first + SNIPPET_LINES - 1);
            for n in first..=shown {
                let _ = writeln!(out, "{n:>5} | {}", f.line_text(n));
            }
            if shown < last {
                let _ = writeln!(out, "      | ... ({} more lines, to line {last})", last - shown);
            }
        }
        None => {
            let _ = writeln!(out, "no source location (a builtin or a std declaration without sources)\n{}", s.signature);
        }
    }
    out.push_str(&others_note(ws, &found));
    Ok(out)
}

fn references(ws: &mut Workspace, args: &Value) -> ToolResult {
    let found = target(ws, args)?;
    let a = &found.a;
    let s = a.index.symbol(found.id);
    let mut items: Vec<(usize, Span, bool)> = Vec::new();
    if bool_arg(args, "include_declaration", true) {
        if let Some(l) = s.loc {
            items.push((l.file, l.name_span, true));
        }
    }
    items.extend(a.references_of(found.id).into_iter().map(|r| (r.file, r.span, false)));
    items.sort_by_key(|i| (i.0, i.1.start));
    items.dedup_by_key(|i| (i.0, i.1.start));
    let uses = items.iter().filter(|i| !i.2).count();
    let nfiles = items.iter().map(|i| i.0).collect::<std::collections::BTreeSet<_>>().len();
    let mut out = format!("{} {}: {} in {}\n", kind_word(s.kind), s.full_name, plural(uses, "reference"), plural(nfiles, "file"));
    for (file, span, decl) in items.iter().take(MAX_ITEMS) {
        let tag = if *decl { " (declaration)" } else { "" };
        let _ = writeln!(out, "{}{tag}: {}", ws.loc(a, *file, *span), Workspace::line_of(a, *file, *span));
    }
    if items.len() > MAX_ITEMS {
        let _ = writeln!(out, "... and {} more", items.len() - MAX_ITEMS);
    }
    out.push_str(&others_note(ws, &found));
    Ok(out)
}

fn hover(ws: &mut Workspace, args: &Value) -> ToolResult {
    let found = target(ws, args)?;
    let a = &found.a;
    let s = a.index.symbol(found.id);
    let mut out = format!("```zell\n{}\n```\n", s.signature);
    let mut line = kind_word(s.kind).to_string();
    if let Some(c) = s.container {
        let c = a.index.symbol(c);
        let rel = match c.kind {
            SymbolKind::Namespace => "in",
            SymbolKind::Func | SymbolKind::Method => "in function",
            _ => "of",
        };
        let _ = write!(line, " {rel} {}", c.full_name);
    } else if s.full_name != s.name {
        let _ = write!(line, " {}", s.full_name);
    }
    if let Some(t) = s.ty.filter(|_| matches!(s.kind, SymbolKind::Local | SymbolKind::Param | SymbolKind::Field | SymbolKind::Global)) {
        let _ = write!(line, ", type {}", zell::sema::index::short_type_name(&a.hir, t));
    }
    if s.is_async {
        line.push_str(", async");
    }
    if s.is_static {
        line.push_str(", static");
    }
    let _ = writeln!(out, "{line}");
    if let Some(l) = s.loc {
        let _ = writeln!(out, "declared at {}", ws.loc(a, l.file, l.name_span));
    }
    if let Some(doc) = &s.doc {
        let _ = write!(out, "\n{}", zell::doc::markdown::to_text(&zell::doc::markdown::parse(&zell::doc::clean_doc(doc)), ""));
    }
    if s.is_std {
        if let Some(e) = zell::doc::std_examples().get(&*s.full_name).and_then(|v| v.first()) {
            let _ = write!(out, "\nExample:\n```zell\n{}\n```\n", e.code.trim_end());
            if let Some(o) = &e.output {
                let _ = writeln!(out, "Output:\n{}", o.trim_end());
            }
        }
    }
    out.push_str(&others_note(ws, &found));
    Ok(out)
}

fn outline(out: &mut String, a: &Analysis, file: usize, s: &DocumentSymbol, depth: usize) {
    let f = &a.files[file];
    let (line, _) = f.line_col(s.name_span.start);
    let detail = if s.detail.is_empty() || s.detail == s.name { String::new() } else { format!(": {}", s.detail) };
    let _ = writeln!(out, "{:>5}  {}{} {}{detail}", line, "  ".repeat(depth), kind_word(s.kind), s.name);
    for c in &s.children {
        outline(out, a, file, c, depth + 1);
    }
}

fn document_symbols(ws: &mut Workspace, args: &Value) -> ToolResult {
    let path = str_arg(args, "path").ok_or("`path` is required")?;
    let (a, file) = ws.file(path)?;
    let mut out = format!("{} (line, kind, name, signature):\n", ws.rel(&a.files[file].path));
    for s in a.document_symbols(file) {
        outline(&mut out, &a, file, &s, 0);
    }
    Ok(out)
}

/// Case-insensitive subsequence match (`usrnew` matches `UserNew`).
fn fuzzy(query: &str, name: &str) -> bool {
    let mut cs = name.chars().flat_map(|c| c.to_lowercase());
    query.chars().flat_map(|c| c.to_lowercase()).all(|q| cs.any(|c| c == q))
}

fn match_rank(query: &str, name: &str, full: &str) -> Option<usize> {
    let (q, n, f) = (query.to_lowercase(), name.to_lowercase(), full.to_lowercase());
    Some(if f == q || n == q {
        0
    } else if f.ends_with(&format!(".{q}")) || n.starts_with(&q) {
        1
    } else if n.contains(&q) || f.contains(&q) {
        2
    } else if fuzzy(&q, name) {
        3
    } else {
        return None;
    })
}

fn workspace_symbols(ws: &mut Workspace, args: &Value) -> ToolResult {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let limit = int_arg(args, "limit").unwrap_or(50).max(1);
    let libs = bool_arg(args, "include_libraries", false);
    let a = ws.analysis();
    let mut hits: Vec<(usize, &zell::sema::index::Symbol)> = Vec::new();
    for s in &a.index.symbols {
        let Some(l) = s.loc else { continue };
        if matches!(s.kind, SymbolKind::Local | SymbolKind::Param | SymbolKind::BuiltinFunc | SymbolKind::BuiltinType) || matches!(s.key, zell::sema::index::SymKey::LocalConst(..)) {
            continue;
        }
        let library = a.is_read_only(l.file);
        if library && !libs {
            continue;
        }
        let Some(rank) = (if query.is_empty() { Some(0) } else { match_rank(&query, &s.name, &s.full_name) }) else { continue };
        hits.push((rank * 2 + usize::from(library), s));
    }
    hits.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.full_name.cmp(&y.1.full_name)));
    if hits.is_empty() {
        return Ok(format!("No declarations match `{query}`{}.", if libs { "" } else { " in the project (try include_libraries or std_index)" }));
    }
    let mut out = format!("{} (showing {}):\n", plural(hits.len(), "match"), hits.len().min(limit));
    for (_, s) in hits.iter().take(limit) {
        let l = s.loc.expect("filtered");
        let sig = s.signature.lines().next().unwrap_or("");
        let _ = writeln!(out, "{} {}  {}  {sig}", kind_word(s.kind), s.full_name, ws.loc(&a, l.file, l.name_span));
    }
    Ok(out)
}

/// A valid, non-keyword identifier (one identifier token).
fn is_identifier(s: &str) -> bool {
    let mut d = zell::diagnostics::Diagnostics::new();
    let toks = zell::lexer::lex(s, &mut d);
    let mut idents = toks.iter().filter(|t| !matches!(t.kind, zell::token::TokenKind::Semi | zell::token::TokenKind::Eof));
    s != "_" && !d.has_errors() && matches!(idents.next().map(|t| &t.kind), Some(zell::token::TokenKind::Ident(n)) if n == s) && idents.next().is_none()
}

fn rename(ws: &mut Workspace, args: &Value) -> ToolResult {
    let new_name = str_arg(args, "new_name").ok_or("`new_name` is required")?.to_string();
    if !is_identifier(&new_name) {
        return Err(format!("`{new_name}` is not a valid Zell identifier"));
    }
    let found = target(ws, args)?;
    if !found.others.is_empty() {
        return Err(format!("`{}` is ambiguous; pass a position or a fuller name.{}", str_arg(args, "symbol").unwrap_or(""), others_note(ws, &found)));
    }
    let a = &found.a;
    let s = a.index.symbol(found.id);
    let Some(decl) = s.loc.filter(|_| !s.is_std) else {
        return Err(format!("{} is part of the standard library or a builtin and cannot be renamed", s.full_name));
    };
    if a.is_read_only(decl.file) {
        return Err(format!("{} is declared in a read-only dependency and cannot be renamed", s.full_name));
    }
    let mut spans: Vec<(usize, Span)> = vec![(decl.file, decl.name_span)];
    spans.extend(a.references_of(found.id).into_iter().map(|r| (r.file, r.span)));
    let mut by_file: std::collections::BTreeMap<usize, Vec<TextEdit>> = std::collections::BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for (f, sp) in spans {
        if !seen.insert((f, sp.start)) || a.files[f].text.get(sp.start as usize..sp.end as usize) != Some(&*s.name) || a.is_read_only(f) {
            continue;
        }
        by_file.entry(f).or_default().push(TextEdit { span: sp, new_text: new_name.clone() });
    }
    let n: usize = by_file.values().map(Vec::len).sum();
    let apply = bool_arg(args, "apply", false);
    let mut out = format!("Rename {} {} to {new_name}: {} in {}\n", kind_word(s.kind), s.full_name, plural(n, "edit"), plural(by_file.len(), "file"));
    let mut writes = Vec::new();
    for (f, edits) in &by_file {
        let file = &a.files[*f];
        let new_text = apply_edits(&file.text, edits);
        out.push_str(&edit_diff(&ws.rel(&file.path), &file.text, &new_text));
        writes.push((file.path.clone(), new_text));
    }
    if apply {
        for (p, t) in writes {
            std::fs::write(&p, t).map_err(|e| format!("cannot write {p}: {e}"))?;
        }
        out.push_str("\nApplied. Run `diagnostics` to check the result.\n");
    } else {
        out.push_str("\nDry run: nothing was written. Call `rename` again with `apply: true` to write these changes.\n");
    }
    Ok(out)
}

fn format(ws: &mut Workspace, args: &Value) -> ToolResult {
    let apply = bool_arg(args, "apply", true);
    let files: Vec<std::path::PathBuf> = match str_arg(args, "path") {
        Some(p) => {
            let abs = ws.abs(p);
            if abs.is_dir() {
                let mut v = Vec::new();
                workspace::collect(&abs, &mut v);
                v
            } else if abs.is_file() {
                vec![abs]
            } else {
                return Err(format!("no such file or directory: {p}"));
            }
        }
        None => {
            let a = ws.analysis();
            ws.project_files(&a).into_iter().map(|f| a.files[f].path.clone().into()).collect()
        }
    };
    let mut files = files;
    files.sort();
    let (mut changed, mut failed) = (Vec::new(), Vec::new());
    let mut diffs = String::new();
    for p in &files {
        let rel = ws.rel(&p.display().to_string());
        let Ok(text) = std::fs::read_to_string(p) else {
            failed.push(format!("{rel}: cannot read"));
            continue;
        };
        match zell::fmt::format_source(&text) {
            Ok(new) if new != text => {
                if diffs.len() < 30_000 {
                    diffs.push_str(&zell::fmt::unified_diff(&rel, &text, &new));
                }
                if apply {
                    std::fs::write(p, &new).map_err(|e| format!("cannot write {rel}: {e}"))?;
                }
                changed.push(rel);
            }
            Ok(_) => {}
            Err(zell::fmt::FmtError::Syntax(_)) => failed.push(format!("{rel}: has syntax errors, not formatted (see `diagnostics`)")),
            Err(e) => failed.push(format!("{rel}: {e}")),
        }
    }
    let mut out = if changed.is_empty() {
        format!("All {} formatted.\n", if files.len() - failed.len() == 1 { "1 file is".to_string() } else { format!("{} files are", files.len() - failed.len()) })
    } else if apply {
        format!("Formatted {} of {}:\n{diffs}", plural(changed.len(), "file"), files.len())
    } else {
        format!("{} of {} would change (not written):\n{diffs}", plural(changed.len(), "file"), files.len())
    };
    for f in failed {
        let _ = writeln!(out, "{f}");
    }
    Ok(out)
}

fn doc(ws: &mut Workspace, args: &Value) -> ToolResult {
    let symbol = str_arg(args, "symbol");
    let text = zell::doc::describe(&ws.root, symbol, bool_arg(args, "private", false))?;
    if text.trim().is_empty() {
        return Ok(format!("No documentation for `{}`.", symbol.unwrap_or("the project")));
    }
    Ok(text)
}

/// The first sentence-ish line of a doc comment.
fn summary(doc: &str) -> &str {
    doc.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

fn std_index(ws: &mut Workspace, args: &Value) -> ToolResult {
    let Some(idx) = ws.std_index() else {
        return Err("the standard library sources were not found (set ZELL_STD or install the toolchain)".into());
    };
    let std_root = zell::sema::stdfs::std_dir().and_then(|d| d.parent().map(Path::to_path_buf));
    let show = |p: &str| std_root.as_ref().and_then(|r| Path::new(p).strip_prefix(r).ok()).map_or(p.to_string(), |r| r.display().to_string());
    let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
    if query.is_empty() {
        let mut ns: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for d in &idx.decls {
            if std_public(d) && matches!(d.kind, SymbolKind::Func | SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Const | SymbolKind::Global) {
                let n = d.full_name.rsplit_once('.').map_or("", |x| x.0);
                *ns.entry(n.to_string()).or_default() += 1;
            }
        }
        let mut out = format!("{} (declarations):\n", plural(ns.len(), "std namespace"));
        for (n, c) in ns {
            let _ = writeln!(out, "{n} ({c})");
        }
        return Ok(out);
    }
    let limit = int_arg(args, "limit").unwrap_or(30).max(1);
    let mut hits: Vec<(usize, &zell::stdindex::StdDecl)> = idx.decls.iter().filter(|d| std_public(d)).filter_map(|d| match_rank(query, &d.name, &d.full_name).map(|r| (r, d))).collect();
    // fuzzy hits are noise once there are better ones
    if hits.iter().any(|h| h.0 < 3) {
        hits.retain(|h| h.0 < 3);
    }
    hits.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.full_name.len().cmp(&y.1.full_name.len())).then_with(|| x.1.full_name.cmp(&y.1.full_name)));
    hits.dedup_by(|x, y| x.1.full_name == y.1.full_name);
    if hits.is_empty() {
        return Ok(format!("No std declaration matches `{query}`."));
    }
    let mut out = format!("{} (showing {}):\n", plural(hits.len(), "match"), hits.len().min(limit));
    for (_, d) in hits.iter().take(limit) {
        let f = &idx.files[d.file];
        let (line, _) = f.line_col(d.name_span.start);
        let _ = writeln!(out, "{} {}: {}  ({}:{line})", kind_word(d.kind), d.full_name, d.signature, show(&f.path));
        if let Some(doc) = &d.doc {
            let doc = zell::doc::clean_doc(doc);
            let s = summary(&doc);
            if !s.is_empty() {
                let _ = writeln!(out, "    {s}");
            }
        }
    }
    Ok(out)
}
