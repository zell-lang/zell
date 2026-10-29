//! Code-generating refactorings: "Fill missing fields" on a struct
//! literal and "Implement <Iface>" on a struct (`refactor.rewrite`, a
//! `quickfix` when the struct declares `implements Iface` or a "does not
//! implement" error names it).
//!
//! Generated code spells types the way the file sees them: a type of the
//! file's namespace or of an imported one by its short name, a stdlib type
//! by its full path (stdlib roots are always visible), any other user type
//! by its short name plus an added `import`. The edits are formatted by
//! `refactor::formatted` (`zell::fmt`).

use crate::assist::Walker;
use crate::refactor::{import_edit, indent_of, Edits};
use zell::ast::{self, Expr, ExprKind};
use zell::diagnostics::Severity;
use zell::hir::Program;
use zell::sema::shapes::{MethodShape, ShapeKind, TypeShape};
use zell::session::Analysis;
use zell::span::Span;
use zell::types::{Type, TypeId};

/// Most interfaces listed by the generic "Implement interface" actions.
const MAX_ANY: usize = 15;

/// Spells full type names for one file and collects the imports needed.
pub(crate) struct Namer {
    ns: String,
    imported: Vec<String>,
    pub needed: Vec<String>,
}

impl Namer {
    pub fn new(ast: &ast::File) -> Namer {
        let imported = ast.imports.iter().filter(|i| i.alias.is_none()).map(|i| i.path.to_string()).collect();
        Namer { ns: ast.namespace.to_string(), imported, needed: Vec::new() }
    }

    /// `ty` (a type spelled with full declaration names) as the file
    /// writes it.
    pub fn ty(&mut self, ty: &str) -> String {
        let mut out = String::new();
        let b = ty.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if !(c.is_ascii_alphabetic() || c == b'_') {
                out.push(c as char);
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || (b[i] == b'.' && b.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic() || *n == b'_'))) {
                i += 1;
            }
            out.push_str(&self.name(&ty[start..i]));
        }
        out
    }

    fn name(&mut self, full: &str) -> String {
        let Some((ns, short)) = full.rsplit_once('.') else { return full.to_string() };
        if ns == self.ns || self.imported.iter().any(|i| i == ns) {
            return short.to_string();
        }
        if zell::sema::stdlib::is_std_namespace(ns) {
            return full.to_string();
        }
        if !self.needed.iter().any(|n| n == ns) {
            self.needed.push(ns.to_string());
        }
        short.to_string()
    }

    /// The `import` edits for the namespaces the spelled types need.
    pub fn import_edits(&self, ast: &ast::File, text: &str) -> Edits {
        let mut needed = self.needed.clone();
        needed.sort();
        needed.into_iter().map(|ns| import_edit(ast, text, &ns)).collect()
    }
}

// ---------------------------------------------------------------------------
// Fill missing fields
// ---------------------------------------------------------------------------

/// The zero value of `t` as a literal, `None` when it has none that can
/// be written (arrays, tuples, tasks, enums whose first variant has
/// fields).
fn zero(p: &Program, t: TypeId, names: &mut Namer) -> Option<String> {
    Some(match p.types.get(t) {
        Type::Bool => "false".into(),
        Type::Int(_) => "0".into(),
        Type::Float32 | Type::Float64 => "0.0".into(),
        Type::Char => "'\\0'".into(),
        Type::String => "\"\"".into(),
        Type::Error | Type::Any | Type::Pointer(_) | Type::Weak(_) | Type::Slice(_) | Type::Map(..) | Type::Func(_) | Type::Interface(_) => "nil".into(),
        Type::Struct(_) => format!("{}{{}}", names.ty(&p.type_name(t))),
        Type::Enum(d) => {
            let e = p.enums.get(d.0 as usize)?;
            let v = e.variants.first().filter(|v| v.fields.is_empty())?;
            format!("{}.{}", names.ty(&p.type_name(t)), v.name)
        }
        _ => return None,
    })
}

/// The innermost struct literal around `sel`.
fn literal_at(ast: &ast::File, sel: Span) -> Option<&Expr> {
    let mut found: Option<&Expr> = None;
    Walker {
        on_stmt: &mut |_| {},
        on_expr: &mut |e| {
            if matches!(e.kind, ExprKind::StructLit { .. }) && e.span.start <= sel.start && sel.end <= e.span.end {
                found = Some(e);
            }
        },
        on_block: &mut |_| {},
    }
    .file(ast);
    found
}

/// Adds `Name: <zero value>` for every missing field of the struct
/// literal at `sel`, in declaration order: fields the file may set
/// (exported ones of another namespace), not embedded, without a default
/// value (the literal already gets it) and with a writable zero value.
pub(crate) fn fill_fields(a: &Analysis, file: usize, ast: &ast::File, text: &str, sel: Span) -> Option<(String, Edits)> {
    let e = literal_at(ast, sel)?;
    let ExprKind::StructLit { path, generics, fields } = &e.kind else { return None };
    let last = path.segments.last()?;
    let sym = a.index.symbol(a.symbol_at(file, last.span.start)?);
    let zell::sema::index::SymKey::Struct(sid) = sym.key else { return None };
    let st = a.hir.structs.get(sid as usize)?;
    let mut names = Namer::new(ast);
    let own = st.full_name.rsplit_once('.').is_some_and(|(ns, _)| ns == names.ns);
    // Defaults from the declaration (the program's types may not carry
    // them for a struct of another namespace).
    let defaulted: Vec<&str> = sym
        .loc
        .as_ref()
        .and_then(|l| {
            a.asts.get(l.file)?.decls.iter().find_map(|d| match d {
                ast::Decl::Struct(s) if s.name.span == l.name_span => Some(s.fields.iter().filter(|f| f.default.is_some()).map(|f| f.name.name.as_str()).collect()),
                _ => None,
            })
        })
        .unwrap_or_default();
    let mut add = Vec::new();
    for f in &st.fields {
        if f.embedded || f.default.is_some() || defaulted.contains(&f.name.as_str()) || (!own && !f.exported) || fields.iter().any(|x| x.name.name == f.name) {
            continue;
        }
        if let Some(z) = zero(&a.hir, f.ty, &mut names) {
            add.push(format!("{}: {z}", f.name));
        }
    }
    if add.is_empty() {
        return None;
    }
    let mut edits: Edits = match fields.last() {
        Some(l) => vec![(Span::new(l.span.end, l.span.end), format!(", {}", add.join(", ")))],
        None => {
            let from = generics.last().map_or(path.span.end, |g| g.span.end) as usize;
            let open = (from + text[from..e.span.end as usize].find('{')? + 1) as u32;
            vec![(Span::new(open, open), add.join(", "))]
        }
    };
    edits.extend(names.import_edits(ast, text));
    Some(("Fill missing fields".into(), edits))
}

// ---------------------------------------------------------------------------
// Implement interface
// ---------------------------------------------------------------------------

/// The struct declaration whose header (attributes to the opening brace)
/// contains `sel`.
fn struct_at<'f>(ast: &'f ast::File, text: &str, sel: Span) -> Option<&'f ast::StructDecl> {
    ast.decls.iter().find_map(|d| match d {
        ast::Decl::Struct(s) => {
            let open = text[s.span.start as usize..s.span.end as usize].find('{').map_or(s.span.end, |i| s.span.start + i as u32);
            (s.span.start <= sel.start && sel.end <= open).then_some(s)
        }
        _ => None,
    })
}

fn full_name(ast: &ast::File, name: &str) -> String {
    format!("{}.{name}", ast.namespace.to_string())
}

/// The shape of type `name` as written in `ast`: by full name, else in the
/// file's namespace, else in an imported one.
fn shape_named<'a>(a: &'a Analysis, ast: &ast::File, name: &str) -> Option<(u32, &'a TypeShape)> {
    let g = &a.types;
    let mut cands = vec![name.to_string(), full_name(ast, name)];
    cands.extend(ast.imports.iter().filter(|i| i.alias.is_none()).map(|i| format!("{}.{name}", i.path.to_string())));
    cands.iter().find_map(|c| g.find(c)).map(|t| (t, &g.types[t as usize]))
}

/// Interfaces `s` (shape `t`) is required to implement: its `implements`
/// list and the "S does not implement I" errors naming it.
fn required(a: &Analysis, ast: &ast::File, s: &ast::StructDecl) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in &s.implements {
        if let ast::TypeKind::Named { path, .. } = &t.kind {
            if let Some((_, sh)) = shape_named(a, ast, &path.to_string()) {
                out.push(sh.name.clone());
            }
        }
    }
    let prefix = format!("{} does not implement ", s.name.name);
    for d in a.diagnostics.iter().filter(|d| d.severity == Severity::Error) {
        if let Some(rest) = d.message.strip_prefix(&prefix) {
            let iface = rest.split(':').next().unwrap_or("");
            if let Some((_, sh)) = shape_named(a, ast, iface) {
                if !out.contains(&sh.name) {
                    out.push(sh.name.clone());
                }
            }
        }
    }
    out
}

/// The struct declared in `ast` that the action at `sel` is about: the
/// one whose header contains `sel`, else the one a "does not implement"
/// error at `sel` names.
fn target_struct<'f>(a: &Analysis, file: usize, ast: &'f ast::File, text: &str, sel: Span) -> Option<&'f ast::StructDecl> {
    if let Some(s) = struct_at(ast, text, sel) {
        return Some(s);
    }
    let d = a.diagnostics_for(file).find(|d| d.span.start <= sel.start && sel.end <= d.span.end && d.message.contains(" does not implement "))?;
    let name = d.message.split(' ').next()?;
    ast.decls.iter().find_map(|d| match d {
        ast::Decl::Struct(s) if s.name.name == name => Some(s),
        _ => None,
    })
}

/// Methods of interface `it` that struct `t` lacks (by name; a method of
/// the name with another signature cannot be added and is left out).
fn missing(a: &Analysis, t: u32, it: &TypeShape) -> Vec<MethodShape> {
    let have = a.types.methods(t);
    it.methods.iter().filter(|m| !m.default && !have.iter().any(|h| h.name == m.name)).cloned().collect()
}

fn generic(it: &TypeShape) -> bool {
    it.methods.iter().any(|m| m.sig.contains('$') || m.sig.contains('?'))
}

/// Ids of the "implement" actions at `sel`: `impl|S|I` for required
/// interfaces, `impl_any|S|I` for other user interfaces of the file's
/// namespace and its imports that `S` does not satisfy (stdlib ones only
/// when required: an `import io` would list a dozen).
pub(crate) fn impl_candidates(a: &Analysis, file: usize, sel: Span) -> Vec<String> {
    let (Some(ast), text) = (a.asts.get(file), a.files[file].text.as_str()) else { return Vec::new() };
    let Some(s) = target_struct(a, file, ast, text, sel) else { return Vec::new() };
    let sname = full_name(ast, &s.name.name);
    let Some(t) = a.types.find(&sname) else { return Vec::new() };
    let req = required(a, ast, s);
    let mut out: Vec<String> = req.iter().map(|i| format!("impl|{sname}|{i}")).collect();
    if struct_at(ast, text, sel).is_none() {
        return out;
    }
    let ns = ast.namespace.to_string();
    let visible: Vec<String> = std::iter::once(ns.clone()).chain(ast.imports.iter().filter(|i| i.alias.is_none()).map(|i| i.path.to_string())).collect();
    let supers = a.types.supertypes(t);
    let mut any: Vec<&TypeShape> = a
        .types
        .types
        .iter()
        .enumerate()
        .filter(|(i, it)| it.kind == ShapeKind::Interface && !it.is_std && visible.contains(&it.ns) && !supers.contains(&(*i as u32)) && !req.contains(&it.name))
        .filter(|(_, it)| (!it.sealed || it.ns == ns) && !generic(it) && !missing(a, t, it).is_empty())
        .map(|(_, it)| it)
        .collect();
    any.sort_by(|x, y| (x.ns != ns, &x.name).cmp(&(y.ns != ns, &y.name)));
    out.extend(any.into_iter().take(MAX_ANY).map(|it| format!("impl_any|{sname}|{}", it.name)));
    out
}

/// Parameter names of interface method `m` of `it`, from its declaration
/// (`p0`, `p1`, ... when it is not found or does not match).
fn param_names_of(a: &Analysis, it: &TypeShape, m: &str, count: usize) -> Vec<String> {
    let names = it.loc.as_ref().and_then(|loc| {
        let parsed;
        let ast = match a.asts.get(loc.file) {
            Some(ast) => &**ast,
            None => {
                parsed = zell::parser::parse_source(&a.files.get(loc.file)?.text).0;
                &parsed
            }
        };
        ast.decls.iter().find_map(|d| match d {
            ast::Decl::Interface(i) if i.name.span == loc.name_span => i.methods.iter().find(|f| f.name.name == m).map(|f| f.params.iter().map(|p| p.name.name.clone()).collect::<Vec<_>>()),
            _ => None,
        })
    });
    match names {
        Some(n) if n.len() == count => n,
        _ => (0..count).map(|i| format!("p{i}")).collect(),
    }
}

/// Splits a shape signature `async (A, B) R` into (async, params, result).
fn split_sig(sig: &str) -> Option<(bool, Vec<String>, String)> {
    let (is_async, rest) = match sig.strip_prefix("async ") {
        Some(r) => (true, r),
        None => (false, sig),
    };
    let rest = rest.strip_prefix('(')?;
    let (mut depth, mut start, mut params) = (0i32, 0usize, Vec::new());
    for (i, c) in rest.char_indices() {
        match c {
            '(' | '[' | '<' => depth += 1,
            ')' if depth == 0 => {
                if !rest[start..i].trim().is_empty() {
                    params.push(rest[start..i].trim().to_string());
                }
                return Some((is_async, params, rest[i + 1..].trim().to_string()));
            }
            ')' | ']' | '>' => depth -= 1,
            ',' if depth == 0 => {
                params.push(rest[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    None
}

/// Adds stubs (`panic("not implemented")`) for the methods of the
/// interface named in `id` that the struct lacks, after its last member.
pub(crate) fn implement(a: &Analysis, ast: &ast::File, text: &str, id: &str) -> Option<(String, Edits)> {
    let mut parts = id.split('|');
    let (kind, sname, iname) = (parts.next()?, parts.next()?, parts.next()?);
    let t = a.types.find(sname)?;
    let it = &a.types.types[a.types.find(iname)? as usize];
    let s = ast.decls.iter().find_map(|d| match d {
        ast::Decl::Struct(s) if full_name(ast, &s.name.name) == sname => Some(s),
        _ => None,
    })?;
    let missing = missing(a, t, it);
    if missing.is_empty() || generic(it) {
        return None;
    }
    let mut names = Namer::new(ast);
    let (_, base) = indent_of(text, s.span.start);
    let ind = format!("{base}    ");
    let mut stubs = Vec::new();
    for m in &missing {
        let (mut is_async, params, mut ret) = split_sig(&m.sig)?;
        // Interface methods spell an `async` result as `Task<T>`.
        if let Some(t) = ret.strip_prefix("Task<").and_then(|r| r.strip_suffix('>')).filter(|_| !is_async) {
            (is_async, ret) = (true, t.to_string());
        }
        let pn = param_names_of(a, it, &m.name, params.len());
        let ps: Vec<String> = pn.iter().zip(&params).map(|(n, t)| format!("{n} {}", names.ty(t))).collect();
        let ret = if ret == "void" { String::new() } else { format!(" {}", names.ty(&ret)) };
        stubs.push(format!("{ind}{}func {}({}){ret} {{\n{ind}    panic(\"not implemented\")\n{ind}}}\n", if is_async { "async " } else { "" }, m.name, ps.join(", ")));
    }
    let close = s.span.end - 1;
    if text.as_bytes().get(close as usize) != Some(&b'}') {
        return None;
    }
    let (line_start, _) = indent_of(text, close);
    let members = !(s.fields.is_empty() && s.methods.is_empty() && s.embeds.is_empty());
    let body = stubs.join("\n");
    let edit = if text[line_start as usize..close as usize].trim().is_empty() {
        (Span::new(line_start, line_start), if members { format!("\n{body}") } else { body })
    } else if !members {
        let open = text[..close as usize].rfind('{')? as u32;
        (Span::new(open + 1, close), format!("\n{body}{base}"))
    } else {
        (Span::new(close, close), format!("\n\n{body}{base}"))
    };
    let mut edits = vec![edit];
    edits.extend(names.import_edits(ast, text));
    let short = iname.rsplit('.').next().unwrap_or(iname);
    let title = if kind == "impl" { format!("Implement {short}") } else { format!("Implement interface {short}") };
    Some((title, edits))
}
