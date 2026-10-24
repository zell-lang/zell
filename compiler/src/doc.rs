//! `zell doc`: documentation from the doc comments of Zell sources
//! (docs/TOOLING.md, "Прочие инструменты").
//!
//! Conventions (the same as hover in the LSP, which renders doc comments
//! through [`markdown`]):
//!
//! * the `//` block directly above a declaration (attributes may sit in
//!   between) is its documentation; the comment above `namespace` documents
//!   the namespace;
//! * the first sentence is the summary (index tables, search);
//! * the body is Markdown ([`markdown`]): paragraphs, lists, headings,
//!   fenced or indented code, `` `symbol` `` cross-links, and a paragraph
//!   starting with `Deprecated:`;
//! * `func ExampleSplit()` in a `*_test.zl` file is an example for `Split`
//!   of the namespace of its directory (`ExampleBuilder_WriteString` for a
//!   method, `Example` for the namespace, `_suffix` starting with a lower
//!   case letter for more than one); a trailing `// Output:` (or
//!   `// Unordered output:`) comment block makes `zell test` run it and
//!   compare its standard output.
//!
//! Only exported (upper-case) names are documented unless `--private`.
//! The stdlib is read from its sources on disk ([`crate::sema::stdfs`]),
//! extern declaration files included, plus the functions the compiler
//! lowers itself and the constant table of `sema/stdlib.rs` (documented in
//! `sema/stdlib_docs.rs`).

pub mod html;
pub mod markdown;
pub mod nav;
pub mod site;

use crate::ast;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// One runnable (or compile-only) example.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Example {
    /// `ExampleSplit_second`.
    pub name: String,
    /// The documented symbol: `Split`, `Builder.WriteString`, or `""` for
    /// the namespace.
    pub target: String,
    /// `second` for `ExampleSplit_second`.
    pub suffix: String,
    /// The body of the example function, dedented, without the output comment.
    pub code: String,
    /// The expected standard output (`None`: compiled, not run).
    pub output: Option<String>,
    /// `// Unordered output:`: lines are compared as a multiset.
    pub unordered: bool,
    pub file: String,
    pub line: u32,
    /// `async func ExampleX()` (the runner awaits it).
    #[serde(skip)]
    pub is_async: bool,
    /// `func ExampleX() void!` (an error fails the example).
    #[serde(skip)]
    pub fallible: bool,
}

/// A documented declaration.
#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub name: String,
    /// `func`, `struct`, `interface`, `enum`, `const`, `var`, `field`,
    /// `method`, `variant`.
    pub kind: &'static str,
    pub signature: String,
    /// Markdown (empty: undocumented).
    pub doc: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
    /// Display path (`std/strings/strings.zl`), empty for compiler builtins.
    pub file: String,
    #[serde(skip)]
    pub path: Option<PathBuf>,
    pub line: u32,
    pub exported: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Item>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Example>,
}

impl Item {
    fn new(name: &str, kind: &'static str, signature: String, doc: Option<String>) -> Item {
        let doc = doc.map(|d| clean_doc(&d)).unwrap_or_default();
        Item {
            name: name.to_string(),
            kind,
            signature,
            summary: markdown::summary(&doc),
            deprecated: markdown::is_deprecated(&doc),
            doc,
            file: String::new(),
            path: None,
            line: 0,
            exported: is_exported(name),
            members: Vec::new(),
            examples: Vec::new(),
        }
    }

    pub fn is_type(&self) -> bool {
        matches!(self.kind, "struct" | "interface" | "enum")
    }
}

/// A namespace and its declarations.
#[derive(Clone, Debug, Serialize)]
pub struct Namespace {
    pub name: String,
    pub doc: String,
    pub summary: String,
    pub std: bool,
    pub files: Vec<String>,
    pub items: Vec<Item>,
    /// Examples for the namespace itself and those whose target is unknown.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Example>,
}

impl Namespace {
    /// The item `Split` or member `Builder.WriteString`.
    pub fn find(&self, path: &str) -> Option<&Item> {
        let (head, rest) = match path.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (path, None),
        };
        let it = self.items.iter().find(|i| i.name == head)?;
        match rest {
            None => Some(it),
            Some(m) => it.members.iter().find(|x| x.name == m),
        }
    }

    fn find_mut(&mut self, path: &str) -> Option<&mut Item> {
        let (head, rest) = match path.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (path, None),
        };
        let it = self.items.iter_mut().find(|i| i.name == head)?;
        match rest {
            None => Some(it),
            Some(m) => it.members.iter_mut().find(|x| x.name == m),
        }
    }
}

/// A set of documented namespaces.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Docs {
    pub namespaces: Vec<Namespace>,
}

/// What to include.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Also unexported names.
    pub private: bool,
}

/// One source file to document.
pub struct Source {
    pub path: PathBuf,
    /// The path shown in output (`std/strings/strings.zl`, `src/main.zl`).
    pub display: String,
    pub text: String,
    /// A stdlib file.
    pub std: bool,
}

pub fn is_exported(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase())
}

/// Strips what is not documentation: the `C: zell_x (runtime/...)` line
/// of generated extern declarations, `Code generated` headers.
pub fn clean_doc(d: &str) -> String {
    if d.starts_with("Code generated") {
        return String::new();
    }
    let mut lines: Vec<&str> = d.lines().filter(|l| !(l.starts_with("C: ") && l.contains("runtime/src"))).collect();
    if lines.len() == 1 && lines[0].starts_with("Internal runtime primitive") {
        lines.clear();
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    lines.join("\n")
}

fn line_of(text: &str, off: u32) -> u32 {
    text[..(off as usize).min(text.len())].bytes().filter(|&b| b == b'\n').count() as u32 + 1
}

/// Collapses a multi-line declaration header into one line.
fn one_line(s: &str) -> String {
    let mut o = String::new();
    for (i, l) in s.lines().enumerate() {
        let l = l.trim();
        if l.is_empty() {
            continue;
        }
        if i > 0 && !o.ends_with('(') && !o.ends_with('[') && !l.starts_with(')') && !l.starts_with(']') {
            o.push(' ');
        }
        o.push_str(l);
    }
    o.replace(",)", ")").replace(", )", ")").replace(",]", "]")
}

/// The header of a declaration: from the line of its name to `end`
/// (exclusive), on one line, without a trailing `{`, comment or `extern`.
fn header(text: &str, span_start: u32, name: &ast::Ident, end: u32) -> String {
    let ns = name.span.start as usize;
    let start = text[..ns].rfind('\n').map_or(0, |i| i + 1).max(span_start as usize);
    let end = (end as usize).clamp(ns, text.len());
    let mut s = one_line(&text[start..end]);
    if let Some(i) = s.find(" //") {
        s.truncate(i);
    }
    let s = s.trim().trim_end_matches('{').trim_end();
    s.strip_prefix("extern ").unwrap_or(s).to_string()
}

/// The first line of a span (const, var, field, variant).
fn first_line(text: &str, span: ast_span::Span, name: &ast::Ident) -> String {
    let ns = name.span.start as usize;
    let start = text[..ns].rfind('\n').map_or(0, |i| i + 1).max(span.start as usize);
    let end = text[ns..].find('\n').map_or(text.len(), |i| ns + i).min((span.end as usize).max(ns));
    let mut s = text[start..end].trim().to_string();
    if let Some(i) = s.find(" //") {
        s.truncate(i);
    }
    if s.chars().count() > 120 {
        s = s.chars().take(117).collect::<String>() + "...";
    }
    s.trim_end_matches('{').trim_end().to_string()
}

mod ast_span {
    pub use crate::span::Span;
}

fn doc_at(text: &str, off: u32) -> Option<String> {
    crate::sema::index::doc_comment(text, off)
}

/// The output comment of an example body: (code, output, unordered).
pub fn split_example(text: &str, body: &ast::Block) -> (String, Option<String>, bool) {
    let (s, e) = (body.span.start as usize + 1, (body.span.end as usize).saturating_sub(1));
    let inner = if s <= e && e <= text.len() { &text[s..e] } else { "" };
    let lines: Vec<&str> = inner.lines().collect();
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && lines[start - 1].trim().starts_with("//") {
        start -= 1;
    }
    let mut out = None;
    let mut unordered = false;
    let mut code_end = end;
    for i in start..end {
        let c = lines[i].trim().trim_start_matches("//").trim();
        let (rest, un) = if let Some(r) = c.strip_prefix("Output:") {
            (r, false)
        } else if let Some(r) = c.strip_prefix("Unordered output:") {
            (r, true)
        } else {
            continue;
        };
        let mut o: Vec<String> = Vec::new();
        if !rest.trim().is_empty() {
            o.push(rest.trim().to_string());
        }
        for l in &lines[i + 1..end] {
            let l = l.trim().strip_prefix("//").unwrap_or("");
            o.push(l.strip_prefix(' ').unwrap_or(l).trim_end().to_string());
        }
        out = Some(o.join("\n").trim().to_string());
        unordered = un;
        code_end = i;
        break;
    }
    let code = markdown_dedent(&lines[..code_end]);
    (code, out, unordered)
}

fn markdown_dedent(lines: &[&str]) -> String {
    let n = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let mut v: Vec<&str> = lines.iter().map(|l| if l.len() >= n { &l[n..] } else { l.trim_start() }).collect();
    while v.first().is_some_and(|l| l.trim().is_empty()) {
        v.remove(0);
    }
    while v.last().is_some_and(|l| l.trim().is_empty()) {
        v.pop();
    }
    v.join("\n")
}

/// `ExampleBuilder_WriteString_second` -> (`Builder.WriteString`, `second`).
pub fn example_target(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("Example")?;
    if rest.is_empty() {
        return Some((String::new(), String::new()));
    }
    let rest = rest.strip_prefix('_').map_or(rest, |r| if r.starts_with(|c: char| c.is_lowercase()) { name.strip_prefix("Example").unwrap() } else { r });
    if rest.starts_with('_') {
        // `Example_suffix`: a namespace example
        return Some((String::new(), rest[1..].to_string()));
    }
    if !rest.starts_with(|c: char| c.is_uppercase()) {
        return None;
    }
    let mut parts: Vec<&str> = rest.split('_').collect();
    let mut suffix = String::new();
    if parts.len() > 1 && parts.last().is_some_and(|p| p.starts_with(|c: char| c.is_lowercase())) {
        suffix = parts.pop().unwrap().to_string();
    }
    if parts.iter().any(|p| p.is_empty()) || parts.len() > 2 {
        return None;
    }
    Some((parts.join("."), suffix))
}

/// The examples of a parsed test file.
pub fn examples_of(file: &ast::File, text: &str, display: &str) -> Vec<Example> {
    let mut out = Vec::new();
    for d in &file.decls {
        let ast::Decl::Func(f) = d else { continue };
        let fallible = match &f.ret {
            None => false,
            Some(t) => match &t.kind {
                ast::TypeKind::Fallible(inner) if matches!(&inner.kind, ast::TypeKind::Named { path, .. } if path.to_string() == "void") => true,
                _ => continue,
            },
        };
        if !f.params.is_empty() || !f.generics.is_empty() {
            continue;
        }
        let (Some((target, suffix)), Some(body)) = (example_target(&f.name.name), &f.body) else { continue };
        let (code, output, unordered) = split_example(text, body);
        out.push(Example {
            name: f.name.name.clone(),
            target,
            suffix,
            code,
            output,
            unordered,
            file: display.to_string(),
            line: line_of(text, f.name.span.start),
            is_async: f.is_async,
            fallible,
        });
    }
    out
}

pub fn is_test_file(p: &str) -> bool {
    p.ends_with("_test.zl")
}

fn parse(text: &str) -> ast::File {
    let mut diags = crate::diagnostics::Diagnostics::new();
    let tokens = crate::lexer::lex(text, &mut diags);
    crate::parser::parse_file(tokens, &mut diags)
}

fn func_item(text: &str, f: &ast::FuncDecl, kind: &'static str) -> Item {
    let end = f.body.as_ref().map_or(f.span.end, |b| b.span.start);
    let mut it = Item::new(&f.name.name, kind, header(text, f.span.start, &f.name, end), doc_at(text, f.span.start));
    it.line = line_of(text, f.name.span.start);
    it
}

/// The items of one parsed file.
fn items_of(file: &ast::File, text: &str, opts: Options) -> Vec<Item> {
    let mut out = Vec::new();
    let keep = |n: &str| opts.private || is_exported(n);
    let at = |it: &mut Item, off: u32| it.line = line_of(text, off);
    for d in &file.decls {
        match d {
            ast::Decl::Func(f) if keep(&f.name.name) && f.name.name != "main" || opts.private && f.name.name == "main" => out.push(func_item(text, f, "func")),
            ast::Decl::Struct(s) if keep(&s.name.name) => {
                let end = text[s.name.span.end as usize..s.span.end as usize].find('{').map_or(s.span.end, |i| s.name.span.end + i as u32);
                let mut it = Item::new(&s.name.name, "struct", header(text, s.span.start, &s.name, end), doc_at(text, s.span.start));
                at(&mut it, s.name.span.start);
                for fl in s.fields.iter().filter(|f| keep(&f.name.name)) {
                    let mut m = Item::new(&fl.name.name, "field", first_line(text, fl.span, &fl.name), doc_at(text, fl.span.start));
                    at(&mut m, fl.name.span.start);
                    it.members.push(m);
                }
                for f in s.methods.iter().filter(|f| keep(&f.name.name)) {
                    it.members.push(func_item(text, f, "method"));
                }
                out.push(it);
            }
            ast::Decl::Interface(i) if keep(&i.name.name) => {
                let end = text[i.name.span.end as usize..i.span.end as usize].find('{').map_or(i.span.end, |x| i.name.span.end + x as u32);
                let mut it = Item::new(&i.name.name, "interface", header(text, i.span.start, &i.name, end), doc_at(text, i.span.start));
                at(&mut it, i.name.span.start);
                for f in i.methods.iter().filter(|f| keep(&f.name.name)) {
                    it.members.push(func_item(text, f, "method"));
                }
                out.push(it);
            }
            ast::Decl::Enum(e) if keep(&e.name.name) => {
                let end = text[e.name.span.end as usize..e.span.end as usize].find('{').map_or(e.span.end, |x| e.name.span.end + x as u32);
                let mut it = Item::new(&e.name.name, "enum", header(text, e.span.start, &e.name, end), doc_at(text, e.span.start));
                at(&mut it, e.name.span.start);
                for v in &e.variants {
                    let mut m = Item::new(&v.name.name, "variant", first_line(text, v.span, &v.name), doc_at(text, v.span.start));
                    m.exported = true;
                    at(&mut m, v.name.span.start);
                    it.members.push(m);
                }
                for f in e.methods.iter().filter(|f| keep(&f.name.name)) {
                    it.members.push(func_item(text, f, "method"));
                }
                out.push(it);
            }
            ast::Decl::Const(c) if keep(&c.name.name) => {
                let mut it = Item::new(&c.name.name, "const", first_line(text, c.span, &c.name), doc_at(text, c.span.start));
                at(&mut it, c.name.span.start);
                out.push(it);
            }
            ast::Decl::Var(v) => {
                if let Some(n) = v.names.first().filter(|n| keep(&n.name)) {
                    let mut it = Item::new(&n.name, "var", first_line(text, v.span, n), doc_at(text, v.span.start));
                    at(&mut it, n.span.start);
                    out.push(it);
                }
            }
            _ => {}
        }
    }
    out
}

/// The doc comment above the `namespace` line.
fn namespace_doc(text: &str) -> Option<String> {
    let mut off = 0;
    for l in text.split_inclusive('\n') {
        if l.trim_start().starts_with("namespace ") {
            return doc_at(text, off as u32).map(|d| clean_doc(&d)).filter(|d| !d.is_empty());
        }
        off += l.len();
    }
    None
}

fn kind_order(k: &str) -> u8 {
    match k {
        "const" => 0,
        "var" => 1,
        "func" => 2,
        _ => 3,
    }
}

impl Docs {
    /// Documents `sources`; `*_test.zl` files contribute only examples.
    pub fn build(sources: &[Source], opts: Options) -> Docs {
        let mut by_ns: BTreeMap<String, Namespace> = BTreeMap::new();
        // directory -> namespace of its non-test files
        let mut dir_ns: HashMap<PathBuf, String> = HashMap::new();
        let mut pending: Vec<(PathBuf, String, Vec<Example>)> = Vec::new();
        for src in sources {
            let file = parse(&src.text);
            let ns = file.namespace.to_string();
            if ns.is_empty() {
                continue;
            }
            let dir = src.path.parent().map(Path::to_path_buf).unwrap_or_default();
            if is_test_file(&src.display) {
                pending.push((dir, ns, examples_of(&file, &src.text, &src.display)));
                continue;
            }
            dir_ns.entry(dir).or_insert_with(|| ns.clone());
            let n = by_ns.entry(ns.clone()).or_insert_with(|| Namespace {
                name: ns.clone(),
                doc: String::new(),
                summary: String::new(),
                std: src.std,
                files: Vec::new(),
                items: Vec::new(),
                examples: Vec::new(),
            });
            n.files.push(src.display.clone());
            if let Some(d) = namespace_doc(&src.text) {
                // the file named like the namespace wins
                let stem = src.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if n.doc.is_empty() || ns.rsplit('.').next() == Some(stem) {
                    n.doc = d;
                }
            }
            for mut it in items_of(&file, &src.text, opts) {
                it.file = src.display.clone();
                it.path = Some(src.path.clone());
                for m in &mut it.members {
                    m.file = src.display.clone();
                    m.path = Some(src.path.clone());
                }
                // `@cfg` variants share a name: the first one wins
                if !n.items.iter().any(|x| x.name == it.name) {
                    n.items.push(it);
                }
            }
        }
        if sources.iter().any(|s| s.std) {
            add_builtins(&mut by_ns);
        }
        for (dir, ns, exs) in pending {
            let target = dir_ns.get(&dir).cloned().unwrap_or_else(|| fallback_ns(&display_of(sources, &dir), &ns));
            let n = by_ns.entry(target.clone()).or_insert_with(|| Namespace {
                name: target.clone(),
                doc: String::new(),
                summary: String::new(),
                std: crate::sema::stdlib::is_std_namespace(&target),
                files: Vec::new(),
                items: Vec::new(),
                examples: Vec::new(),
            });
            for ex in exs {
                match n.find_mut(&ex.target) {
                    Some(it) if !ex.target.is_empty() => it.examples.push(ex),
                    _ => n.examples.push(ex),
                }
            }
        }
        let mut namespaces: Vec<Namespace> = by_ns.into_values().collect();
        for n in &mut namespaces {
            if n.doc.is_empty() && n.std {
                n.doc = crate::sema::stdlib_docs::namespace_doc(&n.name).unwrap_or_default().to_string();
            }
            n.summary = markdown::summary(&n.doc);
            n.items.sort_by(|a, b| kind_order(a.kind).cmp(&kind_order(b.kind)).then_with(|| a.name.cmp(&b.name)));
            for it in &mut n.items {
                it.examples.sort_by(|a, b| a.suffix.cmp(&b.suffix));
                for m in &mut it.members {
                    m.examples.sort_by(|a, b| a.suffix.cmp(&b.suffix));
                }
            }
        }
        Docs { namespaces }
    }

    pub fn namespace(&self, name: &str) -> Option<&Namespace> {
        self.namespaces.iter().find(|n| n.name == name)
    }

    /// Resolves `text` (`Split`, `strings.Split`, `strings.Builder.Len`,
    /// `net.http`) seen in namespace `from` to (namespace index, item
    /// path or `""` for the namespace itself).
    pub fn resolve(&self, from: Option<&str>, text: &str) -> Option<(usize, String)> {
        let text = text.trim().trim_end_matches("()");
        if text.is_empty() || !text.split('.').all(|s| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')) {
            return None;
        }
        if let Some(f) = from.and_then(|f| self.namespaces.iter().position(|n| n.name == f)) {
            if self.namespaces[f].find(text).is_some() {
                return Some((f, text.to_string()));
            }
        }
        let segs: Vec<&str> = text.split('.').collect();
        for cut in (1..=segs.len()).rev() {
            let ns = segs[..cut].join(".");
            let rest = segs[cut..].join(".");
            if let Some(i) = self.ns_index(&ns) {
                if rest.is_empty() {
                    return Some((i, String::new()));
                }
                if self.namespaces[i].find(&rest).is_some() {
                    return Some((i, rest));
                }
            }
        }
        // last-segment match: `http.Get` -> net.http
        if segs.len() > 1 {
            let rest = rest_of(&segs);
            let suffix = format!(".{}", segs[0]);
            let mut cands: Vec<usize> = (0..self.namespaces.len()).filter(|&i| self.namespaces[i].name.ends_with(&suffix)).collect();
            cands.sort_by_key(|&i| (!self.namespaces[i].std, self.namespaces[i].name.len()));
            for i in cands {
                if self.namespaces[i].find(&rest).is_some() {
                    return Some((i, rest));
                }
            }
        }
        None
    }

    fn ns_index(&self, name: &str) -> Option<usize> {
        self.namespaces.iter().position(|n| n.name == name)
    }

    /// Documentation coverage of exported names: (documented, total, the
    /// missing ones as (namespace, item path, file, line)).
    pub fn coverage(&self) -> (usize, usize, Vec<(String, String, String, u32)>) {
        let (mut done, mut total, mut missing) = (0, 0, Vec::new());
        for n in &self.namespaces {
            for it in n.items.iter().filter(|i| i.exported) {
                total += 1;
                if it.doc.is_empty() {
                    missing.push((n.name.clone(), it.name.clone(), it.file.clone(), it.line));
                } else {
                    done += 1;
                }
                for m in it.members.iter().filter(|m| m.exported && m.kind == "method") {
                    total += 1;
                    if m.doc.is_empty() {
                        missing.push((n.name.clone(), format!("{}.{}", it.name, m.name), m.file.clone(), m.line));
                    } else {
                        done += 1;
                    }
                }
            }
        }
        (done, total, missing)
    }

    /// Every example, with its namespace.
    pub fn examples(&self) -> Vec<(&str, &Example)> {
        let mut out = Vec::new();
        for n in &self.namespaces {
            for e in &n.examples {
                out.push((n.name.as_str(), e));
            }
            for it in &n.items {
                for e in &it.examples {
                    out.push((n.name.as_str(), e));
                }
                for m in &it.members {
                    for e in &m.examples {
                        out.push((n.name.as_str(), e));
                    }
                }
            }
        }
        out
    }
}

/// The display path of a directory, from any of its sources.
fn display_of(sources: &[Source], dir: &Path) -> String {
    sources.iter().find(|s| s.path.parent() == Some(dir)).map(|s| s.display.rsplit_once('/').map_or(String::new(), |x| x.0.to_string())).unwrap_or_default()
}

/// The namespace of a test file's directory without other sources:
/// `std/fmt` -> `fmt`, else the test namespace without `_test`.
fn fallback_ns(dir_display: &str, test_ns: &str) -> String {
    match dir_display.strip_prefix("std/") {
        Some(rest) if !rest.is_empty() => rest.replace('/', "."),
        _ => test_ns.strip_suffix("_test").unwrap_or(test_ns).to_string(),
    }
}

fn rest_of(segs: &[&str]) -> String {
    segs[1..].join(".")
}

/// The functions the compiler lowers itself, the constants and builtin
/// structs of `sema/stdlib.rs` (they have no source file).
fn add_builtins(by_ns: &mut BTreeMap<String, Namespace>) {
    use crate::sema::stdlib_docs as sd;
    fn get<'a>(by_ns: &'a mut BTreeMap<String, Namespace>, ns: &str) -> &'a mut Namespace {
        by_ns.entry(ns.to_string()).or_insert_with(|| Namespace {
            name: ns.to_string(),
            doc: String::new(),
            summary: String::new(),
            std: true,
            files: Vec::new(),
            items: Vec::new(),
            examples: Vec::new(),
        })
    }
    for (path, sig, doc) in sd::SPECIAL_DOCS {
        let (ns, name) = path.rsplit_once('.').unwrap();
        let n = get(by_ns, ns);
        if n.items.iter().any(|i| i.name == name) {
            continue;
        }
        let mut it = Item::new(name, "func", sig.to_string(), Some(doc.to_string()));
        it.exported = true;
        n.items.push(it);
    }
    for (ns, name, fields) in crate::sema::stdlib::STRUCTS {
        let n = get(by_ns, ns);
        if n.items.iter().any(|i| i.name == *name) {
            continue;
        }
        let mut it = Item::new(name, "struct", format!("struct {name}"), sd::const_doc(&format!("{ns}.{name}")).map(str::to_string));
        for (f, t) in fields.iter() {
            it.members.push(Item::new(f, "field", format!("{f} {t}"), sd::const_doc(&format!("{ns}.{name}.{f}")).map(str::to_string)));
        }
        n.items.push(it);
    }
    for (ns, name, value) in crate::sema::stdlib::CONSTS {
        let n = get(by_ns, ns);
        if n.items.iter().any(|i| i.name == *name) {
            continue;
        }
        let sig = match value {
            crate::sema::stdlib::StdConst::Error(_) => format!("const {name} error"),
            crate::sema::stdlib::StdConst::Target(crate::sema::stdlib::TargetConst::Str(v)) => format!("const {name} = {v:?}"),
            crate::sema::stdlib::StdConst::Target(_) => format!("const {name} string  // target-dependent"),
            crate::sema::stdlib::StdConst::Int(v) => format!("const {name} = {v}"),
            crate::sema::stdlib::StdConst::Float(v) => format!("const {name} = {v}"),
        };
        let it = Item::new(name, "const", sig, sd::const_doc(&format!("{ns}.{name}")).map(str::to_string));
        n.items.push(it);
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                walk(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "zl") {
            out.push(p);
        }
    }
}

/// The sources under `dir` (recursively), displayed relative to
/// `display_root` with `prefix`.
pub fn load_dir(dir: &Path, display_root: &Path, prefix: &str, std: bool) -> Vec<Source> {
    let mut paths = Vec::new();
    if dir.is_file() {
        paths.push(dir.to_path_buf());
    } else {
        walk(dir, &mut paths);
    }
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let rel = p.strip_prefix(display_root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        out.push(Source { display: format!("{prefix}{rel}"), path: p, text, std });
    }
    out
}

/// The whole standard library (`std/` on disk).
pub fn load_std() -> Vec<Source> {
    match crate::sema::stdfs::std_dir() {
        Some(d) => load_dir(&d, &d, "std/", true),
        None => Vec::new(),
    }
}

/// Examples of the std test files by the full name of their symbol
/// (`strings.Split`, `strings.Builder.Len`), for hover.
pub fn std_examples() -> &'static HashMap<String, Vec<Example>> {
    static EX: std::sync::OnceLock<HashMap<String, Vec<Example>>> = std::sync::OnceLock::new();
    EX.get_or_init(|| {
        let srcs: Vec<Source> = load_std();
        // only the files that matter: tests, and one non-test file per directory for its namespace
        let mut seen = std::collections::HashSet::new();
        let srcs: Vec<Source> = srcs
            .into_iter()
            .filter(|s| is_test_file(&s.display) || seen.insert(s.path.parent().map(Path::to_path_buf)))
            .collect();
        let mut out: HashMap<String, Vec<Example>> = HashMap::new();
        for s in &srcs {
            if !is_test_file(&s.display) {
                continue;
            }
            let dir = s.path.parent();
            let file = parse(&s.text);
            let ns = srcs
                .iter()
                .find(|o| !is_test_file(&o.display) && o.path.parent() == dir)
                .map(|o| parse(&o.text).namespace.to_string())
                .unwrap_or_else(|| fallback_ns(s.display.rsplit_once('/').map_or("", |x| x.0), &file.namespace.to_string()));
            for e in examples_of(&file, &s.text, &s.display) {
                let key = if e.target.is_empty() { ns.clone() } else { format!("{ns}.{}", e.target) };
                out.entry(key).or_default().push(e);
            }
        }
        for v in out.values_mut() {
            v.sort_by(|a, b| a.suffix.cmp(&b.suffix));
        }
        out
    })
}

/// Hover text of an example: collapsed code and output.
pub fn example_markdown(e: &Example) -> String {
    let title = if e.suffix.is_empty() { "Example".to_string() } else { format!("Example ({})", e.suffix) };
    let mut s = format!("<details><summary>{title}</summary>\n\n```zell\n{}\n```\n", e.code);
    if let Some(o) = &e.output {
        s.push_str(&format!("\nOutput:\n\n```text\n{o}\n```\n"));
    }
    s.push_str("\n</details>\n");
    s
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const USAGE: &str = "usage: zell doc [path|namespace|symbol] [--html <dir>] [--json] [--check [--min N]] [--private] [--source-url URL]\n       zell doc --site <dir> [--base /zell/] [--repo-url URL] [repo]";

/// What `zell doc <arg>` documents.
struct Target {
    docs: Docs,
    /// Restrict output to this namespace (and item).
    ns: Option<String>,
    item: Option<String>,
}

fn project_sources(root: &Path, opts: Options) -> Vec<Source> {
    let _ = opts;
    let manifest = ["zell.toml", "zell.json", "zell.yaml"].iter().any(|m| root.join(m).is_file());
    let src = if manifest && root.join("src").is_dir() { root.join("src") } else { root.to_path_buf() };
    load_dir(&src, root, "", false)
}

fn dep_sources(root: &Path) -> Vec<Source> {
    let mut out = Vec::new();
    // path dependencies of the manifest (displayed relative to the project)
    if let Ok(m) = crate::package::manifest::load_dir(root) {
        for dep in m.manifest.dependencies.values().chain(m.manifest.dev_dependencies.values()) {
            if let crate::package::manifest::Dependency::Detailed(d) = dep {
                if let Some(p) = &d.path {
                    let dir = root.join(p);
                    let srcs = project_sources(&dir, Options::default());
                    out.extend(srcs.into_iter().map(|mut s| {
                        s.display = format!("{p}/{}", s.display);
                        s
                    }));
                }
            }
        }
    }
    for base in [root.join(".zell/deps"), root.join("vendor")] {
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for d in dirs {
            let name = d.file_name().unwrap().to_string_lossy().to_string();
            out.extend(load_dir(&d, &base, &format!("{}/", base.file_name().unwrap().to_string_lossy()), false).into_iter().map(|mut s| {
                if !s.display.contains(&name) {
                    s.display = format!("{name}/{}", s.display);
                }
                s
            }));
        }
    }
    out
}

fn find_dep(root: &Path, module: &str) -> Option<PathBuf> {
    let esc = crate::package::store::escape_path(module);
    let deps = root.join(".zell/deps");
    let (parent, last) = match esc.rsplit_once('/') {
        Some((p, l)) => (deps.join(p), l.to_string()),
        None => (deps.clone(), esc.clone()),
    };
    if let Ok(rd) = std::fs::read_dir(&parent) {
        let mut hits: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&format!("{last}@")))).collect();
        hits.sort();
        if let Some(h) = hits.pop() {
            return Some(h);
        }
    }
    let v = root.join("vendor").join(module);
    v.is_dir().then_some(v)
}

fn resolve_target(arg: Option<&str>, opts: Options) -> Result<Target, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    resolve_target_in(&cwd, arg, opts)
}

/// [`resolve_target`] for the project at `cwd` (paths in `arg` are
/// relative to it).
fn resolve_target_in(cwd: &Path, arg: Option<&str>, opts: Options) -> Result<Target, String> {
    let cwd = cwd.to_path_buf();
    let Some(arg) = arg else {
        let srcs = project_sources(&cwd, opts);
        if srcs.is_empty() {
            return Err(format!("no .zl files in {}", cwd.display()));
        }
        return Ok(Target { docs: Docs::build(&srcs, opts), ns: None, item: None });
    };
    if arg == "std" {
        let srcs = load_std();
        if srcs.is_empty() {
            return Err("the standard library sources were not found (set ZELL_STD)".into());
        }
        return Ok(Target { docs: Docs::build(&srcs, opts), ns: None, item: None });
    }
    let joined = cwd.join(arg);
    let p = joined.as_path();
    if p.exists() && (arg.contains('/') || arg.ends_with(".zl") || arg == "." || p.is_dir() && !arg.contains('.')) {
        let root = if p.is_file() { p.parent().unwrap_or(Path::new(".")).to_path_buf() } else { p.to_path_buf() };
        let srcs = if p.is_file() { load_dir(p, &root, "", false) } else { project_sources(p, opts) };
        if srcs.is_empty() {
            return Err(format!("no .zl files in {arg}"));
        }
        return Ok(Target { docs: Docs::build(&srcs, opts), ns: None, item: None });
    }
    if arg.contains('/') {
        if let Some(d) = find_dep(&cwd, arg) {
            let srcs = load_dir(&d, d.parent().unwrap(), "", false);
            return Ok(Target { docs: Docs::build(&srcs, opts), ns: None, item: None });
        }
        return Err(format!("{arg}: no such path or dependency (run `zell install`?)"));
    }
    // a namespace or symbol: the project, its dependencies and std
    // the current project only when there is one (a manifest)
    let is_project = ["zell.toml", "zell.json", "zell.yaml"].iter().any(|m| cwd.join(m).is_file());
    let mut srcs = if is_project { project_sources(&cwd, opts) } else { Vec::new() };
    srcs.retain(|s| !s.text.lines().any(|l| l.trim_start().strip_prefix("namespace ").is_some_and(|n| crate::sema::stdlib::is_std_namespace(n.trim()))));
    srcs.extend(dep_sources(&cwd));
    srcs.extend(load_std());
    let docs = Docs::build(&srcs, opts);
    let (ni, item) = match docs.resolve(None, arg) {
        Some(r) => r,
        None => {
            // a bare namespace: `strings`, `http`
            let segs = arg.split('.').count();
            let mut cands: Vec<usize> = (0..docs.namespaces.len())
                .filter(|&i| {
                    let n = &docs.namespaces[i].name;
                    n == arg || n.ends_with(&format!(".{arg}")) && n.split('.').count() > segs
                })
                .collect();
            cands.sort_by_key(|&i| (!docs.namespaces[i].std, docs.namespaces[i].name.len()));
            match cands.first() {
                Some(&i) => (i, String::new()),
                None => return Err(format!("{arg}: no such namespace or symbol")),
            }
        }
    };
    let ns = docs.namespaces[ni].name.clone();
    Ok(Target { docs, ns: Some(ns), item: (!item.is_empty()).then_some(item) })
}

/// The text `zell doc <arg>` prints when run in `cwd` (a namespace,
/// symbol, path or `std`; `None`: the project), for tools such as
/// `zell-mcp`.
pub fn describe(cwd: &Path, arg: Option<&str>, private: bool) -> Result<String, String> {
    let t = resolve_target_in(cwd, arg, Options { private })?;
    let docs = t.docs;
    Ok(match (&t.ns, &t.item) {
        (Some(ns), Some(item)) => item_text(&docs, docs.namespace(ns).ok_or("namespace not found")?, item),
        (Some(ns), None) => namespace_text(docs.namespace(ns).ok_or("namespace not found")?),
        _ => {
            let mut out = String::new();
            for n in &docs.namespaces {
                if n.items.is_empty() && n.examples.is_empty() {
                    continue;
                }
                if docs.namespaces.len() > 1 {
                    out.push_str(&format!("{:<28} {}\n", n.name, n.summary));
                } else {
                    out.push_str(&namespace_text(n));
                }
            }
            out
        }
    })
}

/// Runs `zell doc <args>`; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let mut opts = Options::default();
    let (mut html, mut json, mut check, mut min, mut arg, mut source_url) = (None::<PathBuf>, false, false, 100.0f64, None::<String>, None::<String>);
    let mut verbose = false;
    let (mut site, mut base, mut repo_url) = (None::<PathBuf>, "/".to_string(), None::<String>);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--private" | "--all" => opts.private = true,
            "--json" => json = true,
            "--check" => check = true,
            "-v" | "--verbose" => verbose = true,
            "--html" | "--min" | "--source-url" | "--site" | "--base" | "--repo-url" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("error: {a} needs a value\n{USAGE}");
                    return 2;
                };
                match a {
                    "--html" => html = Some(PathBuf::from(v)),
                    "--site" => site = Some(PathBuf::from(v)),
                    "--base" => base = v.clone(),
                    "--repo-url" => repo_url = Some(if v.ends_with('/') { v.clone() } else { format!("{v}/") }),
                    "--source-url" => source_url = Some(v.trim_end_matches('/').to_string()),
                    _ => match v.trim_end_matches('%').parse() {
                        Ok(m) => min = m,
                        Err(_) => {
                            eprintln!("error: --min needs a percentage");
                            return 2;
                        }
                    },
                }
                i += 1;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            _ if a.starts_with('-') => {
                eprintln!("error: unknown option {a}\n{USAGE}");
                return 2;
            }
            _ if arg.is_none() => arg = Some(a.to_string()),
            _ => {
                eprintln!("error: unexpected argument {a}\n{USAGE}");
                return 2;
            }
        }
        i += 1;
    }
    if let Some(out) = site {
        let root = PathBuf::from(arg.as_deref().unwrap_or("."));
        let repo_url = repo_url.unwrap_or_else(|| site::default_repo_url(&root));
        return match site::build_site(&site::SiteOptions { root, out: out.clone(), base, repo_url }) {
            Ok(n) => {
                eprintln!("wrote {n} pages to {}", out.display());
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        };
    }
    let t = match resolve_target(arg.as_deref(), opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let mut docs = t.docs;
    if check {
        if let Some(ns) = &t.ns {
            docs.namespaces.retain(|n| &n.name == ns);
        }
        return run_check(&docs, min, verbose);
    }
    if let Some(dir) = html {
        if let Some(ns) = &t.ns {
            docs.namespaces.retain(|n| &n.name == ns || n.name.starts_with(&format!("{ns}.")));
        }
        return match html::write_site(&docs, &dir, source_url.as_deref()) {
            Ok(n) => {
                eprintln!("wrote {n} pages to {}", dir.display());
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        };
    }
    if json {
        let v = match (&t.ns, &t.item) {
            (Some(ns), Some(item)) => serde_json::to_value(docs.namespace(ns).and_then(|n| n.find(item))),
            (Some(ns), None) => serde_json::to_value(docs.namespace(ns)),
            _ => serde_json::to_value(&docs),
        };
        emit(&(serde_json::to_string_pretty(&v.unwrap_or_default()).unwrap_or_default() + "\n"));
        return 0;
    }
    let mut out = String::new();
    match (&t.ns, &t.item) {
        (Some(ns), Some(item)) => out = item_text(&docs, docs.namespace(ns).unwrap(), item),
        (Some(ns), None) => out = namespace_text(docs.namespace(ns).unwrap()),
        _ => {
            for n in &docs.namespaces {
                if n.items.is_empty() && n.examples.is_empty() {
                    continue;
                }
                if docs.namespaces.len() > 1 {
                    out.push_str(&format!("{:<28} {}\n", n.name, n.summary));
                } else {
                    out.push_str(&namespace_text(n));
                }
            }
        }
    }
    emit(&out);
    0
}

/// Writes to stdout, ignoring a closed pipe (`zell doc std | head`).
fn emit(s: &str) {
    use std::io::Write;
    let _ = std::io::stdout().lock().write_all(s.as_bytes());
}

fn run_check(docs: &Docs, min: f64, verbose: bool) -> i32 {
    let (done, total, missing) = docs.coverage();
    let mut out = String::new();
    for (ns, item, file, line) in &missing {
        out.push_str(&format!("{file}:{line}: {ns}.{item}: missing doc comment"));
out.push('\n');
    }
    let mut bad = 0;
    for n in &docs.namespaces {
        for e in &n.examples {
            if !e.target.is_empty() {
                out.push_str(&format!("{}:{}: {}: no symbol {} in {}", e.file, e.line, e.name, e.target, n.name));
out.push('\n');
                bad += 1;
            }
        }
    }
    if verbose {
        let mut per: Vec<(f64, &str, usize, usize)> = Vec::new();
        for n in &docs.namespaces {
            let d = Docs { namespaces: vec![n.clone()] };
            let (a, b, _) = d.coverage();
            if b > 0 {
                per.push((a as f64 / b as f64, &n.name, a, b));
            }
        }
        per.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for (p, n, a, b) in per {
            out.push_str(&format!("{:>6.1}%  {a:>4}/{b:<4} {n}", p * 100.0));
out.push('\n');
        }
    }
    let pct = if total == 0 { 100.0 } else { done as f64 * 100.0 / total as f64 };
    let nex = docs.examples().len();
    out.push_str(&format!("documented {done}/{total} exported symbols ({pct:.1}%), {nex} examples"));
out.push('\n');
    emit(&out);
    i32::from(pct + 1e-9 < min || bad > 0)
}

fn indent(s: &str, by: &str) -> String {
    s.lines().map(|l| if l.is_empty() { "\n".to_string() } else { format!("{by}{l}\n") }).collect()
}

fn doc_text(doc: &str, ind: &str) -> String {
    markdown::to_text(&markdown::parse(doc), ind)
}

fn examples_text(exs: &[Example]) -> String {
    let mut o = String::new();
    for e in exs {
        let title = if e.suffix.is_empty() { "Example:".to_string() } else { format!("Example ({}):", e.suffix) };
        o.push_str(&format!("\n{title}\n{}", indent(&e.code, "    ")));
        if let Some(out) = &e.output {
            o.push_str(&format!("    // Output:\n{}", indent(out, "    // ")));
        }
    }
    o
}

/// `zell doc strings`: the namespace doc and one line per declaration.
pub fn namespace_text(n: &Namespace) -> String {
    let mut o = format!("namespace {}", n.name);
    if let Some(f) = n.files.first() {
        let dir = f.rsplit_once('/').map_or("", |x| x.0);
        if !dir.is_empty() {
            o.push_str(&format!("  // {dir}"));
        }
    }
    o.push_str("\n\n");
    if !n.doc.is_empty() {
        o.push_str(&doc_text(&n.doc, ""));
        o.push('\n');
    }
    for it in &n.items {
        o.push_str(&it.signature);
        o.push('\n');
        if !it.summary.is_empty() {
            o.push_str(&format!("    {}\n", it.summary));
        }
        for m in it.members.iter().filter(|m| m.kind == "method") {
            o.push_str(&format!("    {}\n", m.signature));
        }
    }
    o.push_str(&examples_text(&n.examples));
    o
}

/// `zell doc strings.Split`: signature, documentation, members, examples.
pub fn item_text(docs: &Docs, n: &Namespace, path: &str) -> String {
    let _ = docs;
    let Some(it) = n.find(path) else { return String::new() };
    let mut o = String::new();
    if it.is_type() && it.members.iter().any(|m| m.kind != "method") {
        o.push_str(&it.signature);
        o.push_str(" {\n");
        for m in it.members.iter().filter(|m| m.kind != "method") {
            o.push_str(&format!("    {}", m.signature));
            if !m.summary.is_empty() {
                o.push_str(&format!("  // {}", m.summary));
            }
            o.push('\n');
        }
        o.push_str("}\n");
    } else {
        o.push_str(&it.signature);
        o.push('\n');
    }
    if !it.doc.is_empty() {
        o.push_str(&doc_text(&it.doc, "    "));
    }
    if !it.file.is_empty() {
        o.push_str(&format!("\n    {}.{path}  {}:{}\n", n.name, it.file, it.line));
    }
    let methods: Vec<&Item> = it.members.iter().filter(|m| m.kind == "method").collect();
    if !methods.is_empty() {
        o.push('\n');
        for m in methods {
            o.push_str(&format!("{}\n", m.signature));
            if !m.summary.is_empty() {
                o.push_str(&format!("    {}\n", m.summary));
            }
        }
    }
    o.push_str(&examples_text(&it.examples));
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_names() {
        assert_eq!(example_target("ExampleSplit"), Some(("Split".into(), "".into())));
        assert_eq!(example_target("ExampleSplit_second"), Some(("Split".into(), "second".into())));
        assert_eq!(example_target("ExampleBuilder_WriteString"), Some(("Builder.WriteString".into(), "".into())));
        assert_eq!(example_target("ExampleBuilder_WriteString_x"), Some(("Builder.WriteString".into(), "x".into())));
        assert_eq!(example_target("Example"), Some(("".into(), "".into())));
        assert_eq!(example_target("Example_basic"), Some(("".into(), "basic".into())));
        assert_eq!(example_target("Examples"), None);
        assert_eq!(example_target("Other"), None);
    }

    #[test]
    fn builds_docs_and_examples() {
        let lib = "// Package doc. More.\nnamespace demo\n\n// Add returns a + b.\n//\n// Deprecated: use `Sum`.\nfunc Add(a int, b int) int {\n    return a + b\n}\n\nfunc Sum(\n    xs []int,\n) int {\n    return 0\n}\n\nfunc hidden() {}\n\n// A point.\nstruct Point {\n    // X coordinate.\n    X int\n    y int\n\n    // Len is the length.\n    func Len() float {\n        return 0.0\n    }\n}\n";
        let test = "namespace demo_test\n\nimport demo\n\nfunc ExampleAdd() {\n    println(demo.Add(1, 2))\n    // Output:\n    // 3\n}\n\nfunc ExamplePoint_Len() {\n    println(1)\n}\n";
        let srcs = vec![
            Source { path: "/x/demo/demo.zl".into(), display: "demo/demo.zl".into(), text: lib.into(), std: false },
            Source { path: "/x/demo/demo_test.zl".into(), display: "demo/demo_test.zl".into(), text: test.into(), std: false },
        ];
        let d = Docs::build(&srcs, Options::default());
        let n = d.namespace("demo").unwrap();
        assert_eq!(n.summary, "Package doc.");
        let add = n.find("Add").unwrap();
        assert_eq!(add.signature, "func Add(a int, b int) int");
        assert!(add.deprecated.is_some());
        assert_eq!(add.examples[0].output.as_deref(), Some("3"));
        assert_eq!(add.examples[0].code, "println(demo.Add(1, 2))");
        assert_eq!(n.find("Sum").unwrap().signature, "func Sum(xs []int) int");
        assert!(n.find("hidden").is_none());
        let p = n.find("Point").unwrap();
        assert_eq!(p.members.len(), 2);
        assert_eq!(n.find("Point.Len").unwrap().examples.len(), 1);
        let (done, total, missing) = d.coverage();
        assert_eq!((done, total), (3, 4), "{missing:?}");
        assert_eq!(d.resolve(Some("demo"), "Point.Len"), Some((0, "Point.Len".into())));
        assert_eq!(d.resolve(None, "demo.Add"), Some((0, "Add".into())));
        let t = item_text(&d, n, "Add");
        assert!(t.contains("DEPRECATED: use Sum.") && t.contains("// Output:\n    // 3"), "{t}");
    }
}
