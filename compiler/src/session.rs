//! Compilation session for tools (LSP, `zell fmt`, ...): a set of source
//! files kept in memory and re-analysed on demand (docs/TOOLING.md,
//! "Session API").
//!
//! Analysis is incremental (see [`incr`]): files are parsed (and their
//! `@cfg` applied) once per text, the project is split into units (the
//! files of one namespace) whose results are cached, and an edit re-runs
//! only the units it can affect. The result is the same as analysing the
//! whole project again ([`Session::analyze_full`]).
//!
//! Error resilience: the parser always produces an AST (with error nodes
//! where the text is incomplete, see [`crate::parser`]), so every file
//! takes part in semantic analysis with its *current* text and all
//! positions (diagnostics, symbols, references) refer to it.
//!
//! Read-only files: dependency sources (`.zell/deps/**`) are ordinary
//! session files marked with [`Session::set_read_only`]; the standard
//! library sources on disk come from a [`StdIndex`]
//! ([`Session::set_std_index`]): they are not analysed (the compiler has
//! its own copy of the stdlib) but appended to [`Analysis::files`], and
//! every std symbol of the analysis gets its location and doc comment
//! there, so navigation, hover and workspace symbols reach into `std/`.

mod assemble;
mod compare;
mod facts;
mod fragment;
mod incr;
mod remap;
mod stdblock;
mod synthetic;
pub mod scopes;
pub mod typegraph;

pub use incr::Stats;
#[doc(hidden)]
pub use synthetic::synthetic_file;

use crate::ast;
use crate::diagnostics::{codes, Diagnostic, Diagnostics, Fix, TextEdit};
use crate::hir::Program;
use crate::sema::index::{self, Location, Reference, SymKey, SymbolId, SymbolIndex, SymbolKind};
use crate::span::{SourceFile, Span};
use crate::stdindex::StdIndex;
use crate::{lexer, parser};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

/// A parsed file.
struct FileState {
    source: Arc<SourceFile>,
    parse_diags: Vec<Diagnostic>,
    /// The AST as parsed.
    ast: Arc<ast::File>,
    /// The AST with the declarations `@cfg` excludes on this target removed
    /// (the same allocation as `ast` when nothing is excluded).
    cfg_ast: Arc<ast::File>,
    /// Diagnostics of `@cfg` (file index 0).
    cfg_diags: Vec<Diagnostic>,
    facts: facts::FileFacts,
}

/// In-memory project: path -> source text.
///
/// Cloning is cheap (parsed files are shared, and so are the cached
/// analysis results), so a tool can analyse a snapshot on another thread
/// while it keeps editing the session.
#[derive(Clone, Default)]
pub struct Session {
    files: BTreeMap<String, Arc<FileState>>,
    read_only: BTreeSet<String>,
    std: Option<Arc<StdIndex>>,
    cache: Arc<Mutex<incr::Cache>>,
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// Adds or replaces a file and parses it.
    pub fn update_file(&mut self, path: impl Into<String>, text: impl Into<String>) {
        let path = path.into();
        let text = text.into();
        if let Some(f) = self.files.get(&path) {
            if f.source.text == text {
                return;
            }
        }
        let mut diags = Diagnostics::new();
        let tokens = lexer::lex(&text, &mut diags);
        let ast = Arc::new(parser::parse_file(tokens.clone(), &mut diags));
        // Declarations excluded by `@cfg` (or files by their suffix) on
        // this target are not analysed, as in a build.
        let env = crate::cfg::CfgEnv::current();
        let mut cfg_diags = Diagnostics::new();
        let cfg_ast = if text.contains("@cfg") || env.target.excludes_file(&path) {
            let mut one = [(*ast).clone()];
            crate::cfg::apply(&mut one, &[path.as_str()], &env, &mut cfg_diags);
            let [a] = one;
            Arc::new(a)
        } else {
            ast.clone()
        };
        let facts = facts::facts(&text, &tokens, &cfg_ast);
        let state = FileState { source: Arc::new(SourceFile::new(path.clone(), text)), parse_diags: diags.items, ast, cfg_ast, cfg_diags: cfg_diags.items, facts };
        self.files.insert(path, Arc::new(state));
    }

    pub fn remove_file(&mut self, path: &str) -> bool {
        self.read_only.remove(path);
        self.files.remove(path).is_some()
    }

    /// Marks `path` as a read-only library file (a dependency): analysed,
    /// navigable, never renamed or edited by tools.
    pub fn set_read_only(&mut self, path: &str, read_only: bool) {
        if read_only {
            self.read_only.insert(path.to_string());
        } else {
            self.read_only.remove(path);
        }
    }

    pub fn is_read_only(&self, path: &str) -> bool {
        self.read_only.contains(path) || self.std.as_ref().is_some_and(|s| s.files.iter().any(|f| f.path == path))
    }

    /// The standard library sources to navigate into (see the module docs).
    pub fn set_std_index(&mut self, std: Option<Arc<StdIndex>>) {
        self.std = std;
    }

    pub fn std_index(&self) -> Option<&Arc<StdIndex>> {
        self.std.as_ref()
    }

    pub fn contains(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Paths in analysis order (sorted).
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(|s| s.as_str())
    }

    pub fn text(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(|f| f.source.text.as_str())
    }

    /// The parsed (possibly partial) AST of `path`.
    pub fn ast(&self, path: &str) -> Option<&ast::File> {
        self.files.get(path).map(|f| &*f.ast)
    }

    /// Analyses all files (incrementally: see the module docs).
    pub fn analyze(&self) -> Analysis {
        self.analyze_cancellable(None).expect("analysis without a cancel flag completes")
    }

    /// [`Session::analyze`] that returns `None` as soon as it notices that
    /// `cancel` was set (between units, phases and function bodies). The
    /// units finished before are kept for the next analysis.
    pub fn analyze_cancellable(&self, cancel: Option<&std::sync::atomic::AtomicBool>) -> Option<Analysis> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        incr::analyze(self, &mut cache, cancel)
    }

    /// Analyses all files from scratch, without the cache (the reference
    /// result of [`Session::analyze`]).
    pub fn analyze_full(&self) -> Analysis {
        incr::analyze(self, &mut incr::Cache::default(), None).expect("analysis without a cancel flag completes")
    }

    /// What the last [`Session::analyze`] re-analysed.
    pub fn last_stats(&self) -> Stats {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).stats.clone()
    }
}

/// Finishes the quick fixes of semantic diagnostics with what needs the
/// source text and syntax: deletions that leave a line blank remove the
/// whole line, and an unused `var x = ...` can also be removed.
fn complete_fixes<'f>(diags: &mut [Diagnostic], text_of: &dyn Fn(usize) -> Option<&'f str>, ast_of: &dyn Fn(usize) -> Option<&'f ast::File>) {
    for d in diags.iter_mut() {
        let Some(text) = text_of(d.file) else { continue };
        if d.code == Some(codes::UNUSED_VARIABLE) {
            if let Some(stmt) = ast_of(d.file).and_then(|a| var_stmt_declaring(a, d.span)) {
                let name = text.get(d.span.start as usize..d.span.end as usize).unwrap_or("variable").to_string();
                d.fixes.push(Fix::edit(format!("Remove unused variable {name}"), stmt, ""));
            }
        }
        for fix in &mut d.fixes {
            for e in &mut fix.edits {
                if e.new_text.is_empty() {
                    e.span = whole_lines(text, e.span);
                }
            }
        }
    }
}

/// `span` widened to whole lines (with the newline) when the rest of
/// its first and last line is blank.
pub(crate) fn whole_lines(text: &str, span: Span) -> Span {
    let (s, e) = (span.start as usize, (span.end as usize).min(text.len()));
    let ls = text[..s].rfind('\n').map_or(0, |i| i + 1);
    let le = text[e..].find('\n').map_or(text.len(), |i| e + i + 1);
    if text[ls..s].trim().is_empty() && text[e..le].trim().is_empty() {
        Span::new(ls as u32, le as u32)
    } else {
        span
    }
}

/// The span of the single-name `var` statement whose name is at `name`.
fn var_stmt_declaring(file: &ast::File, name: Span) -> Option<Span> {
    fn in_block(b: &ast::Block, name: Span) -> Option<Span> {
        b.stmts.iter().find_map(|s| in_stmt(s, name))
    }
    fn in_stmt(s: &ast::Stmt, name: Span) -> Option<Span> {
        use ast::StmtKind as K;
        if s.span.start > name.start || s.span.end < name.end {
            return None;
        }
        match &s.kind {
            K::Var(v) if v.names.len() == 1 && v.names[0].span == name => Some(s.span),
            K::Block(b) => in_block(b, name),
            K::If(i) => in_if(i, name),
            K::For(f) => in_block(&f.body, name),
            K::Switch(sw) => sw.cases.iter().flat_map(|c| c.body.iter()).chain(sw.default.iter().flatten()).find_map(|s| in_stmt(s, name)),
            K::TryBlock { body, handler, .. } => in_block(body, name).or_else(|| in_block(handler, name)),
            _ => None,
        }
    }
    fn in_if(i: &ast::IfStmt, name: Span) -> Option<Span> {
        in_block(&i.then, name).or_else(|| match i.else_.as_deref() {
            Some(ast::ElseBranch::If(e)) => in_if(e, name),
            Some(ast::ElseBranch::Block(b)) => in_block(b, name),
            None => None,
        })
    }
    let funcs = file.decls.iter().flat_map(|d| -> Vec<&ast::FuncDecl> {
        match d {
            ast::Decl::Func(f) => vec![f],
            ast::Decl::Struct(s) => s.methods.iter().collect(),
            ast::Decl::Enum(e) => e.methods.iter().collect(),
            _ => Vec::new(),
        }
    });
    funcs.filter_map(|f| f.body.as_ref()).find_map(|b| in_block(b, name))
}

/// Applies non-overlapping `edits` to `text` (used by tests and tools).
pub fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut edits: Vec<&TextEdit> = edits.iter().collect();
    edits.sort_by_key(|e| std::cmp::Reverse((e.span.start, e.span.end)));
    let mut out = text.to_string();
    for e in edits {
        out.replace_range(e.span.start as usize..e.span.end as usize, &e.new_text);
    }
    out
}

/// A node of the document outline.
#[derive(Clone, Debug)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub detail: String,
    pub full_span: Span,
    pub name_span: Span,
    pub symbol: Option<SymbolId>,
    pub children: Vec<DocumentSymbol>,
}

/// Result of [`Session::analyze`].
pub struct Analysis {
    /// Current texts, in session order, then the std sources; indices are
    /// the `file` of diagnostics, locations and references. Shared with
    /// the session and the std index.
    pub files: Vec<Arc<SourceFile>>,
    /// The (possibly partial) AST of each session file after `@cfg`, same
    /// order as `files`.
    pub asts: Vec<Arc<ast::File>>,
    /// Lexer, parser and semantic diagnostics, with their file index.
    pub diagnostics: Vec<Diagnostic>,
    pub index: SymbolIndex,
    /// The types and type declarations of the program (structs with their
    /// fields, interfaces, enums; no functions): `Symbol::ty` and the
    /// struct ids of [`SymKey::Struct`] refer to it. Possibly partial (with
    /// `Invalid` types) when there were errors.
    pub hir: Program,
    ok: bool,
    /// Per file: a read-only library file (dependency or std source).
    pub read_only: Vec<bool>,
    /// Struct, enum and interface satisfaction over the whole program
    /// (user and std types; see `typegraph`).
    pub types: Arc<typegraph::TypeGraph>,
    /// The complete program, analysed on demand by [`Analysis::program`].
    full: OnceLock<Option<Program>>,
    /// Block scopes per session file, on demand ([`Analysis::scopes`]).
    scopes: Vec<OnceLock<scopes::FileScopes>>,
}

impl Analysis {
    fn new(files: Vec<Arc<SourceFile>>, asts: Vec<Arc<ast::File>>, diagnostics: Vec<Diagnostic>, index: SymbolIndex, hir: Program, ok: bool, read_only: Vec<bool>) -> Analysis {
        let scopes = asts.iter().map(|_| OnceLock::new()).collect();
        Analysis { files, asts, diagnostics, index, hir, ok, read_only, types: Arc::default(), full: OnceLock::new(), scopes }
    }

    /// The block scopes of session file `file` (none for std sources).
    pub fn scopes(&self, file: usize) -> Option<&scopes::FileScopes> {
        let ast = self.asts.get(file)?;
        Some(self.scopes.get(file)?.get_or_init(|| scopes::FileScopes::of(ast)))
    }

    /// The analysed program (with function bodies) when every file parsed
    /// and checked cleanly; analysed on the first call.
    pub fn program(&self) -> Option<&Program> {
        if !self.ok {
            return None;
        }
        Some(self.program_partial())
    }

    /// The analysed program with function bodies, possibly partial (with
    /// `Invalid` types and missing bodies) when there were errors; analysed
    /// on the first call. Its ids are its own: they do not match
    /// [`Analysis::hir`] or the symbol index, so declarations are related
    /// to it by full name, file and span.
    pub fn program_partial(&self) -> &Program {
        self.full
            .get_or_init(|| {
                let asts: Vec<ast::File> = self.asts.iter().map(|a| (**a).clone()).collect();
                let texts: Vec<&str> = self.files.iter().take(asts.len()).map(|f| f.text.as_str()).collect();
                let mut diags = Diagnostics::new();
                let (p, _) = index::analyze_indexed(&asts, &texts, &mut diags);
                Some(p)
            })
            .as_ref()
            .expect("program analysed")
    }

    pub fn file_index(&self, path: &str) -> Option<usize> {
        self.files.iter().position(|f| f.path == path)
    }

    /// Whether file `file` is a read-only library file.
    pub fn is_read_only(&self, file: usize) -> bool {
        self.read_only.get(file).copied().unwrap_or(false)
    }

    pub fn diagnostics_for(&self, file: usize) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(move |d| d.file == file)
    }

    /// The symbol named at `offset`: a reference, else a declaration name.
    pub fn symbol_at(&self, file: usize, offset: u32) -> Option<SymbolId> {
        self.index.reference_at(file, offset).map(|r| r.symbol).or_else(|| self.index.declaration_at(file, offset))
    }

    /// The span of the name at `offset` (reference or declaration).
    pub fn name_span_at(&self, file: usize, offset: u32) -> Option<Span> {
        if let Some(r) = self.index.reference_at(file, offset) {
            return Some(r.span);
        }
        let id = self.index.declaration_at(file, offset)?;
        self.index.symbol(id).loc.map(|l| l.name_span)
    }

    /// Where the symbol at `offset` is declared (`None` for std/builtins).
    pub fn definition_at(&self, file: usize, offset: u32) -> Option<Location> {
        let id = self.symbol_at(file, offset)?;
        self.index.symbol(id).loc
    }

    /// Uses of `id` (the declaration itself is not included).
    pub fn references_of(&self, id: SymbolId) -> Vec<Reference> {
        self.index.references_of(id)
    }

    /// Outline of a file: namespace > types/functions > members.
    pub fn document_symbols(&self, file: usize) -> Vec<DocumentSymbol> {
        let Some(Some(info)) = self.index.files.get(file) else { return Vec::new() };
        let ns_id = self.index.lookup(&SymKey::Ns(info.namespace.clone()));
        let mut top: Vec<DocumentSymbol> = self
            .index
            .symbols
            .iter()
            .filter(|s| s.loc.is_some_and(|l| l.file == file) && s.kind != SymbolKind::Namespace)
            .filter(|s| s.container.is_none() || s.container.map(|c| self.index.symbol(c).kind) == Some(SymbolKind::Namespace))
            .filter(|s| outline_kind(s.kind) && !matches!(s.key, SymKey::LocalConst(..)))
            .map(|s| self.outline_node(s.id, file))
            .collect();
        top.sort_by_key(|n| n.full_span.start);
        vec![DocumentSymbol {
            name: info.namespace.clone(),
            kind: SymbolKind::Namespace,
            detail: String::new(),
            full_span: info.span,
            name_span: info.namespace_span,
            symbol: ns_id,
            children: top,
        }]
    }

    fn outline_node(&self, id: SymbolId, file: usize) -> DocumentSymbol {
        let s = self.index.symbol(id);
        let loc = s.loc.unwrap();
        let mut children: Vec<DocumentSymbol> = self
            .index
            .children(id)
            .into_iter()
            .filter(|c| {
                let c = self.index.symbol(*c);
                outline_kind(c.kind) && c.loc.is_some_and(|l| l.file == file) && !matches!(c.key, SymKey::LocalConst(..))
            })
            .map(|c| self.outline_node(c, file))
            .collect();
        children.sort_by_key(|n| n.full_span.start);
        let detail = match s.kind {
            SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Enum => String::new(),
            _ => s.signature.lines().next().unwrap_or("").to_string(),
        };
        let full_span = if loc.full_span.start <= loc.name_span.start && loc.full_span.end >= loc.name_span.end { loc.full_span } else { loc.name_span };
        DocumentSymbol { name: s.name.to_string(), kind: s.kind, detail, full_span, name_span: loc.name_span, symbol: Some(id), children }
    }
}

fn outline_kind(k: SymbolKind) -> bool {
    !matches!(k, SymbolKind::Local | SymbolKind::Param | SymbolKind::BuiltinFunc | SymbolKind::BuiltinType)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODELS: &str = "namespace app.models\n\n// A user.\nstruct User {\n    Name string\n\n    func Hello() string {\n        return \"hi \" + this.Name\n    }\n}\n";
    const MAIN: &str = "namespace app\n\nimport io\nimport app.models\n\nfunc main() {\n    var u = User{Name: \"a\"}\n    println(u.Hello())\n}\n";

    fn session() -> Session {
        let mut s = Session::new();
        s.update_file("models.zl", MODELS);
        s.update_file("main.zl", MAIN);
        s
    }

    #[test]
    fn analyzes_and_resolves_across_files() {
        let a = session().analyze();
        assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
        assert!(a.program().is_some());
        let main = a.file_index("main.zl").unwrap();
        let models = a.file_index("models.zl").unwrap();
        let off = MAIN.find("Hello").unwrap() as u32 + 1;
        let loc = a.definition_at(main, off).unwrap();
        assert_eq!(loc.file, models);
        assert_eq!(a.files[models].slice(loc.name_span), "Hello");
        let user = a.symbol_at(main, MAIN.find("User{").unwrap() as u32).unwrap();
        let sym = a.index.symbol(user);
        assert_eq!(sym.kind, SymbolKind::Struct);
        assert_eq!(sym.doc.as_deref(), Some("A user."));
        // `import app.models` does not name User; the struct literal does.
        assert_eq!(a.references_of(user).len(), 1);
        let local = a.symbol_at(main, MAIN.find("u.Hello").unwrap() as u32).unwrap();
        assert_eq!(a.index.symbol(local).signature.as_ref(), "var u User");
    }

    #[test]
    fn analyzes_files_with_syntax_errors() {
        let mut s = session();
        // A broken member before the method: the struct and its method
        // survive recovery, with current positions.
        let broken = MODELS.replace("Name string\n", "Name string\n    func (\n");
        s.update_file("models.zl", broken.clone());
        let a = s.analyze();
        let models = a.file_index("models.zl").unwrap();
        assert!(a.diagnostics.iter().all(|d| d.file == models), "{:?}", a.diagnostics);
        assert!(!a.diagnostics.is_empty());
        // main.zl still resolves against models.zl, at its new position.
        let main = a.file_index("main.zl").unwrap();
        let loc = a.definition_at(main, MAIN.find("Hello").unwrap() as u32).unwrap();
        assert_eq!(loc.file, models);
        assert_eq!(&broken[loc.name_span.start as usize..loc.name_span.end as usize], "Hello");
        // An unfinished body in main.zl: `u` and its members still resolve.
        let main_broken = MAIN.replace("    println(u.Hello())\n", "    println(u.Hello())\n    var z = u.\n");
        s.update_file("main.zl", main_broken.clone());
        let a = s.analyze();
        let main = a.file_index("main.zl").unwrap();
        let off = main_broken.find("u.\n").unwrap() as u32;
        let id = a.symbol_at(main, off).expect("u resolves in the broken line");
        assert_eq!(a.index.symbol(id).signature.as_ref(), "var u User");
        let errs: Vec<&Diagnostic> = a.diagnostics_for(main).collect();
        assert_eq!(errs.len(), 1, "only the syntax error: {errs:?}");
    }

    #[test]
    fn analysis_survives_every_line_prefix() {
        // Sema must tolerate whatever the recovering parser produces.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for name in ["examples/bank.zl", "examples/structs.zl", "examples/async.zl", "examples/generics.zl"] {
            let text = std::fs::read_to_string(root.join(name)).unwrap();
            let mut cuts: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
            cuts.extend(text.char_indices().map(|(i, _)| i).step_by(37));
            cuts.sort();
            for cut in cuts {
                let mut s = Session::new();
                s.update_file("f.zl", &text[..cut]);
                let a = s.analyze();
                for d in &a.diagnostics {
                    assert!(d.span.end as usize <= cut, "{name}@{cut}: {d:?}");
                }
            }
        }
    }

    #[test]
    fn diagnostics_carry_codes_and_fixes() {
        let mut s = Session::new();
        let src = "namespace app\n\nimport strings\n\nfunc main() {\n    var x = 1\n}\n";
        s.update_file("a.zl", src);
        let a = s.analyze();
        let d = a.diagnostics.iter().find(|d| d.code == Some(codes::UNUSED_VARIABLE)).expect("unused variable");
        let titles: Vec<&str> = d.fixes.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles, ["Rename x to _", "Remove unused variable x"]);
        let removed = apply_edits(src, &d.fixes[1].edits);
        assert_eq!(removed, "namespace app\n\nimport strings\n\nfunc main() {\n}\n");
        // Fixing the variable makes the unused import reportable.
        s.update_file("a.zl", apply_edits(src, &d.fixes[0].edits));
        let a = s.analyze();
        let d = a.diagnostics.iter().find(|d| d.code == Some(codes::UNUSED_IMPORT)).expect("unused import");
        assert_eq!(apply_edits(&a.files[0].text, &d.fixes[0].edits), "namespace app\n\n\nfunc main() {\n    var _ = 1\n}\n");
    }

    #[test]
    fn document_symbols_are_hierarchical() {
        let a = session().analyze();
        let models = a.file_index("models.zl").unwrap();
        let syms = a.document_symbols(models);
        assert_eq!(syms[0].name, "app.models");
        let user = &syms[0].children[0];
        assert_eq!(user.name, "User");
        let names: Vec<&str> = user.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Name", "Hello"]);
    }

    #[test]
    fn analysis_can_be_cancelled_and_snapshots_are_independent() {
        let s = session();
        let flag = std::sync::atomic::AtomicBool::new(true);
        assert!(s.analyze_cancellable(Some(&flag)).is_none(), "a set flag stops the analysis");
        flag.store(false, std::sync::atomic::Ordering::Relaxed);
        let a = s.analyze_cancellable(Some(&flag)).expect("not cancelled");
        assert!(a.diagnostics.is_empty());
        // A clone is a snapshot: later edits of the original do not reach it.
        let snapshot = s.clone();
        let mut s = s;
        s.update_file("main.zl", MAIN.replace("u.Hello()", "u.Nope()"));
        assert!(snapshot.analyze().diagnostics.is_empty());
        assert!(!s.analyze().diagnostics.is_empty());
        assert_eq!(snapshot.ast("main.zl").map(|f| f.decls.len()), Some(1));
    }

    #[test]
    fn utf16_positions_round_trip() {
        let f = SourceFile::new("x", "a\nпр𝄞x\n");
        let off = "a\nпр𝄞".len() as u32;
        assert_eq!(f.utf16_position(off), (1, 4));
        assert_eq!(f.utf16_offset(1, 4), off);
        assert_eq!(f.utf16_offset(1, 99), "a\nпр𝄞x".len() as u32);
        assert_eq!(f.utf16_offset(9, 0), f.text.len() as u32);
    }

    const LIB: &str = "namespace app.lib\n\nfunc Twice(x int) int {\n    return x * 2\n}\n";
    const APP: &str = "namespace app\n\nimport app.lib\n\nfunc main() {\n    var y int = Twice(2)\n    _ = y\n}\n";
    const OTHER: &str = "namespace other\n\nfunc main() {\n}\n";

    #[test]
    fn edits_reanalyse_only_the_units_they_reach() {
        let mut s = Session::new();
        s.update_file("lib.zl", LIB);
        s.update_file("main.zl", APP);
        s.update_file("other.zl", OTHER);
        let a = s.analyze();
        assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
        assert!(s.last_stats().full_run);
        // A body: only its own unit.
        s.update_file("lib.zl", LIB.replace("x * 2", "x + x"));
        let a = s.analyze();
        assert_eq!(s.last_stats(), Stats { dirty: 1, unit_runs: 1, full_run: false, run_files: 1, units: 3 });
        assert_eq!(a.canonical(), s.analyze_full().canonical());
        // A signature: also the importer, which now has an error.
        s.update_file("lib.zl", LIB.replace("x int) int", "x int) string").replace("x * 2", "\"\""));
        let a = s.analyze();
        assert_eq!(s.last_stats().dirty, 2);
        let main = a.file_index("main.zl").unwrap();
        assert!(a.diagnostics_for(main).count() > 0, "{:?}", a.diagnostics);
        assert_eq!(a.canonical(), s.analyze_full().canonical());
        // Nothing changed: nothing re-analysed.
        let _ = s.analyze();
        assert_eq!(s.last_stats().dirty, 0);
        // References across units resolve to the declaration.
        s.update_file("lib.zl", LIB);
        let a = s.analyze();
        let main = a.file_index("main.zl").unwrap();
        let loc = a.definition_at(main, APP.find("Twice").unwrap() as u32).unwrap();
        assert_eq!(a.files[loc.file].path, "lib.zl");
        let id = a.symbol_at(main, APP.find("Twice").unwrap() as u32).unwrap();
        assert_eq!(a.references_of(id).len(), 1);
        assert_eq!(a.index.symbol(id).signature.as_ref(), "func Twice(x int) int");
        let y = a.symbol_at(main, APP.find("y int").unwrap() as u32).unwrap();
        assert_eq!(index::short_type_name(&a.hir, a.index.symbol(y).ty.unwrap()), "int");
    }

    #[test]
    fn sessions_and_analyses_cross_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<Session>();
        shared::<Analysis>();
    }

    #[test]
    fn cancelled_analysis_returns_none_and_recovers() {
        let mut s = Session::new();
        s.update_file("lib.zl", LIB);
        s.update_file("main.zl", APP);
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert!(s.analyze_cancellable(Some(&cancel)).is_none());
        let clone = s.clone();
        let a = clone.analyze();
        assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
        // The clone shares the cache.
        let _ = s.analyze();
        assert_eq!(s.last_stats().dirty, 0);
        assert!(a.program().is_some_and(|p| p.entry.is_some()));
    }
}
