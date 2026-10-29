//! A declaration index of the standard library sources on disk (`std/`,
//! see [`crate::sema::stdfs`]) for tools: where `strings.Split` or
//! `net.http.Get` is declared, its source signature and doc comment.
//! The files are parsed, not analysed (they are the same modules the
//! compiler registers itself), so the index covers the extern declaration
//! files too.

use crate::ast;
use crate::sema::index::SymbolKind;
use crate::span::{SourceFile, Span};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// One declaration of a std file.
#[derive(Clone, Debug)]
pub struct StdDecl {
    /// `strings.Split`, `net.http.Client.Do`.
    pub full_name: String,
    pub name: String,
    pub kind: SymbolKind,
    /// Index into [`StdIndex::files`].
    pub file: usize,
    pub name_span: Span,
    pub full_span: Span,
    /// The declaration's first source line (`func Get(url string) Task<Response!>`).
    pub signature: String,
    pub doc: Option<String>,
    /// A static method.
    pub is_static: bool,
    /// Marked `@intrinsic`: a compiler-lowered member (`testing.T.Must`)
    /// or a struct documenting a builtin type's methods (`task.Task`).
    pub intrinsic: bool,
}

/// The parsed `std/` tree.
#[derive(Clone, Debug, Default)]
pub struct StdIndex {
    /// Absolute paths and texts (shared with the analyses that list them).
    pub files: Vec<Arc<SourceFile>>,
    pub decls: Vec<StdDecl>,
    by_name: HashMap<String, usize>,
}

fn signature(text: &str, span: Span, name: Span) -> String {
    // From the line of the name (after attributes) to the end of that line.
    let start = text[..name.start as usize].rfind('\n').map_or(0, |i| i + 1).max(span.start as usize);
    let end = text[name.start as usize..].find('\n').map_or(text.len(), |i| name.start as usize + i);
    let line = text[start..end].trim();
    line.strip_suffix('{').unwrap_or(line).trim_end().to_string()
}

impl StdIndex {
    /// Parses every `.zl` file under `dir`.
    pub fn load(dir: &Path) -> StdIndex {
        let mut paths = Vec::new();
        fn walk(d: &Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(d) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "zl") && !p.to_string_lossy().ends_with("_test.zl") {
                    // tests and examples are not declarations (doc::std_examples reads them)
                    out.push(p);
                }
            }
        }
        walk(dir, &mut paths);
        paths.sort();
        let mut idx = StdIndex::default();
        for p in paths {
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            idx.add_file(p.display().to_string(), text);
        }
        idx
    }

    /// Adds one file (parsed with error recovery).
    pub fn add_file(&mut self, path: String, text: String) {
        let mut diags = crate::diagnostics::Diagnostics::new();
        let tokens = crate::lexer::lex(&text, &mut diags);
        let file = crate::parser::parse_file(tokens, &mut diags);
        let fi = self.files.len();
        let ns = file.namespace.to_string();
        let push = |idx: &mut StdIndex, full: String, name: &ast::Ident, full_span: Span, kind: SymbolKind, attrs: &[ast::Attribute], is_static: bool| {
            if name.span == Span::DUMMY {
                return;
            }
            let decl = StdDecl {
                full_name: full.clone(),
                name: name.name.clone(),
                kind,
                file: fi,
                name_span: name.span,
                full_span,
                signature: signature(&text, full_span, name.span),
                doc: crate::sema::index::doc_comment(&text, full_span.start),
                is_static,
                intrinsic: attrs.iter().any(|a| a.path.to_string() == "intrinsic"),
            };
            // The first declaration wins (`@cfg` variants share a name).
            idx.by_name.entry(full).or_insert(idx.decls.len());
            idx.decls.push(decl);
        };
        for d in &file.decls {
            match d {
                ast::Decl::Func(f) => push(self, format!("{ns}.{}", f.name.name), &f.name, f.span, SymbolKind::Func, &f.attrs, false),
                ast::Decl::Struct(s) => {
                    push(self, format!("{ns}.{}", s.name.name), &s.name, s.span, SymbolKind::Struct, &s.attrs, false);
                    for fl in &s.fields {
                        push(self, format!("{ns}.{}.{}", s.name.name, fl.name.name), &fl.name, fl.span, SymbolKind::Field, &fl.attrs, false);
                    }
                    for m in &s.methods {
                        push(self, format!("{ns}.{}.{}", s.name.name, m.name.name), &m.name, m.span, SymbolKind::Method, &m.attrs, m.is_static);
                    }
                }
                ast::Decl::Interface(i) => {
                    push(self, format!("{ns}.{}", i.name.name), &i.name, i.span, SymbolKind::Interface, &i.attrs, false);
                    for m in &i.methods {
                        push(self, format!("{ns}.{}.{}", i.name.name, m.name.name), &m.name, m.span, SymbolKind::Method, &m.attrs, m.is_static);
                    }
                }
                ast::Decl::Enum(e) => {
                    push(self, format!("{ns}.{}", e.name.name), &e.name, e.span, SymbolKind::Enum, &e.attrs, false);
                    for m in &e.methods {
                        push(self, format!("{ns}.{}.{}", e.name.name, m.name.name), &m.name, m.span, SymbolKind::Method, &m.attrs, m.is_static);
                    }
                }
                ast::Decl::Const(c) => push(self, format!("{ns}.{}", c.name.name), &c.name, c.span, SymbolKind::Const, &c.attrs, false),
                ast::Decl::TypeAlias(a) => push(self, format!("{ns}.{}", a.name.name), &a.name, a.span, SymbolKind::BuiltinType, &[], false),
                ast::Decl::Var(v) => {
                    if let Some(n) = v.names.first() {
                        push(self, format!("{ns}.{}", n.name), n, v.span, SymbolKind::Global, &v.attrs, false);
                    }
                }
                ast::Decl::Error(_) => {}
            }
        }
        self.files.push(Arc::new(SourceFile::new(path, text)));
    }

    /// The declaration of `full_name` (`strings.Split`); generic
    /// instance names (`task.Channel<int>.Send`) are looked up without
    /// their type arguments.
    pub fn lookup(&self, full_name: &str) -> Option<&StdDecl> {
        if let Some(&i) = self.by_name.get(full_name) {
            return Some(&self.decls[i]);
        }
        let mut plain = String::with_capacity(full_name.len());
        let mut depth = 0;
        for c in full_name.chars() {
            match c {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ if depth == 0 => plain.push(c),
                _ => {}
            }
        }
        self.by_name.get(&plain).map(|&i| &self.decls[i])
    }
}
