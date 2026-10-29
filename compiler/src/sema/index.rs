//! Symbol index for IDE tooling: every declaration (with its definition
//! span, kind, signature, doc comment and container) and every resolved
//! name use (reference span -> declaration).
//!
//! Sema records raw definitions and references through the small `ide_*`
//! hooks below while it resolves names; recording is off unless analysis is
//! started through [`analyze_indexed`], so ordinary compilation pays only a
//! `None` check per hook. After analysis the raw records are turned into a
//! [`SymbolIndex`] using the HIR program for types and signatures.

use super::resolve::PathRes;
use super::*;
use crate::hir::dump::{type_name, DeclNames};
use crate::hir::{FuncKind, Program};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------

/// Stable identity of a declaration inside one analysis.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SymKey {
    /// Namespace by full path (`app.models`, `io`).
    Ns(String),
    Struct(u32),
    Interface(u32),
    Enum(u32),
    /// (enum id, variant index)
    Variant(u32, u32),
    /// (struct id, field index in `hir::Struct::fields`)
    Field(u32, u32),
    /// (interface id, method index)
    IfaceMethod(u32, u32),
    /// Free function or method (`hir::FuncId`).
    Func(u32),
    Global(u32),
    /// (owning function, local id) — params, vars, loop vars, bindings.
    Local(u32, u32),
    /// Local `const`: (file, start offset of its name).
    LocalConst(usize, u32),
    /// Stdlib member by full path (`strings.Split`).
    Std(String),
    /// Builtin function or type (`len`, `int`).
    Builtin(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Namespace,
    Struct,
    Interface,
    Enum,
    Variant,
    Field,
    Method,
    Func,
    Const,
    Global,
    Local,
    Param,
    BuiltinType,
    BuiltinFunc,
}

/// Index into [`SymbolIndex::symbols`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId(pub u32);

/// Where a declaration lives in source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: usize,
    /// The declared name.
    pub name_span: Span,
    /// The whole declaration (with attributes and body).
    pub full_span: Span,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub id: SymbolId,
    pub key: SymKey,
    pub kind: SymbolKind,
    pub name: String,
    /// `app.models.User.Display`, `strings.Split`, `x` for locals.
    pub full_name: String,
    /// `None` for stdlib members and builtins (no user source).
    pub loc: Option<Location>,
    /// Source-like signature: `func Display() string`, `var x int`.
    pub signature: String,
    /// Value type (locals, params, fields, globals) in the analysis' program.
    pub ty: Option<TypeId>,
    /// The `//` comment block directly above the declaration.
    pub doc: Option<String>,
    pub container: Option<SymbolId>,
    pub is_static: bool,
    pub is_async: bool,
    pub is_readonly: bool,
    /// Stdlib member or builtin: no editable source.
    pub is_std: bool,
}

/// One resolved use of a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reference {
    pub file: usize,
    pub span: Span,
    pub symbol: SymbolId,
}

/// One `import` line after resolution.
#[derive(Clone, Debug)]
pub enum ImportInfo {
    /// `import a.b`: exported members of namespace `a.b` by short name.
    Glob(String),
    /// `import a.b as x` / `import a.b.T [as x]`: one name.
    Named(String, SymbolId),
}

/// Per-file facts.
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub namespace: String,
    pub namespace_span: Span,
    pub span: Span,
    pub imports: Vec<ImportInfo>,
}

/// The symbol table and reference index of one analysis.
#[derive(Clone, Debug, Default)]
pub struct SymbolIndex {
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
    /// Indexed by file; `None` for files that were not analysed.
    pub files: Vec<Option<FileInfo>>,
    /// Members of every namespace (user and std) by name.
    pub ns_members: HashMap<String, Vec<(String, SymbolId)>>,
    by_key: HashMap<SymKey, SymbolId>,
    refs_by_symbol: HashMap<SymbolId, Vec<usize>>,
    /// Receivers of member accesses, sorted by (file, name start): the
    /// type of `x` in `x.name`, `x.name(...)` and `x.` being typed (empty
    /// name right after the dot). Member completion lists the members of
    /// this type ([`SymbolIndex::member_receiver`]).
    pub member_recvs: Vec<MemberRecv>,
}

/// The receiver type of one member access (`base.name`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberRecv {
    pub file: usize,
    /// Span of the member name (empty for `x.` being typed).
    pub name_span: Span,
    /// Type of `base` (before auto-deref).
    pub ty: TypeId,
}

impl SymbolIndex {
    pub fn symbol(&self, id: SymbolId) -> &Symbol {
        &self.symbols[id.0 as usize]
    }

    pub fn lookup(&self, key: &SymKey) -> Option<SymbolId> {
        self.by_key.get(key).copied()
    }

    /// The receiver type of the member access whose name starts at `start`
    /// in `file` (see [`SymbolIndex::member_recvs`]).
    pub fn member_receiver(&self, file: usize, start: u32) -> Option<TypeId> {
        let i = self.member_recvs.partition_point(|r| (r.file, r.name_span.start) < (file, start));
        self.member_recvs.get(i).filter(|r| r.file == file && r.name_span.start == start).map(|r| r.ty)
    }

    /// Sets [`SymbolIndex::member_recvs`] (sorted, first record per name).
    pub(crate) fn set_member_recvs(&mut self, mut v: Vec<MemberRecv>) {
        v.sort_by_key(|r| (r.file, r.name_span.start));
        v.dedup_by_key(|r| (r.file, r.name_span.start));
        self.member_recvs = v;
    }

    /// References to `id` (not including the declaration itself).
    pub fn references_of(&self, id: SymbolId) -> Vec<Reference> {
        self.refs_by_symbol.get(&id).map(|v| v.iter().map(|&i| self.references[i]).collect()).unwrap_or_default()
    }

    /// Symbols whose container is `id`, in declaration order.
    pub fn children(&self, id: SymbolId) -> Vec<SymbolId> {
        self.symbols.iter().filter(|s| s.container == Some(id)).map(|s| s.id).collect()
    }

    /// The reference whose span contains `offset` (end inclusive, so a
    /// cursor right after a name still finds it); exact containment wins.
    pub fn reference_at(&self, file: usize, offset: u32) -> Option<&Reference> {
        let mut best: Option<&Reference> = None;
        for r in self.references.iter().filter(|r| r.file == file && r.span.start <= offset && offset <= r.span.end) {
            let strict = offset < r.span.end;
            match best {
                Some(b) if b.span.start <= offset && offset < b.span.end => {}
                _ if strict || best.is_none() => best = Some(r),
                _ => {}
            }
        }
        best
    }

    /// The declaration whose name span contains `offset` (end inclusive).
    pub fn declaration_at(&self, file: usize, offset: u32) -> Option<SymbolId> {
        let mut best = None;
        for s in &self.symbols {
            if let Some(l) = s.loc {
                if l.file == file && l.name_span.start <= offset && offset <= l.name_span.end && l.name_span.start < l.name_span.end {
                    if offset < l.name_span.end {
                        return Some(s.id);
                    }
                    best = Some(s.id);
                }
            }
        }
        best
    }

    /// Rewrites file indices with `map[old] = new` (used when only some of
    /// a session's files were analysed); `n` is the new number of files.
    pub fn remap_files(&mut self, map: &[usize], n: usize) {
        for s in &mut self.symbols {
            if let Some(l) = &mut s.loc {
                l.file = map[l.file];
            }
            if let SymKey::LocalConst(f, _) = &mut s.key {
                *f = map[*f];
            }
        }
        for r in &mut self.references {
            r.file = map[r.file];
        }
        let mut files = vec![None; n];
        for (i, f) in std::mem::take(&mut self.files).into_iter().enumerate() {
            files[map[i]] = f;
        }
        self.files = files;
        self.by_key = self.symbols.iter().map(|s| (s.key.clone(), s.id)).collect();
    }

    /// An index of `symbols` (ids are their positions) and `references`.
    pub(crate) fn from_parts(symbols: Vec<Symbol>, references: Vec<Reference>, files: Vec<Option<FileInfo>>, ns_members: HashMap<String, Vec<(String, SymbolId)>>) -> SymbolIndex {
        let mut by_key = HashMap::with_capacity(symbols.len());
        for s in &symbols {
            by_key.entry(s.key.clone()).or_insert(s.id);
        }
        let mut refs_by_symbol: HashMap<SymbolId, Vec<usize>> = HashMap::new();
        for (i, r) in references.iter().enumerate() {
            refs_by_symbol.entry(r.symbol).or_default().push(i);
        }
        SymbolIndex { symbols, references, files, ns_members, by_key, refs_by_symbol, member_recvs: Vec::new() }
    }

    fn add(&mut self, mut s: Symbol) -> SymbolId {
        if let Some(&id) = self.by_key.get(&s.key) {
            return id;
        }
        let id = SymbolId(self.symbols.len() as u32);
        s.id = id;
        self.by_key.insert(s.key.clone(), id);
        self.symbols.push(s);
        id
    }
}

/// Analyses `files` like [`analyze`], always returning the (possibly
/// partial) HIR program together with the symbol index. `texts[i]` is the
/// source of `files[i]` (used for doc comments).
pub fn analyze_indexed(files: &[ast::File], texts: &[&str], diags: &mut Diagnostics) -> (Program, SymbolIndex) {
    analyze_indexed_cancellable(files, texts, diags, None).expect("analysis without a cancel flag completes")
}

/// [`analyze_indexed`] that gives up (returning `None`) once `cancel` is
/// set: checked between the analysis phases and before each function
/// body, so a superseded analysis stops within one function's checking.
pub fn analyze_indexed_cancellable(files: &[ast::File], texts: &[&str], diags: &mut Diagnostics, cancel: Option<&std::sync::atomic::AtomicBool>) -> Option<(Program, SymbolIndex)> {
    let files: Vec<&ast::File> = files.iter().collect();
    let mut s = Sema::new(&files, diags);
    s.ide = Some(Box::default());
    s.cancel = cancel;
    s.run();
    if s.cancelled() {
        return None;
    }
    let (extra, names) = s.ide_collect_extras_for(&|_| true, true);
    let rec = *s.ide.take().unwrap();
    let program = s.finish();
    let index = build_with(rec, extra, &program, texts, &names);
    Some((program, index))
}

// ---------------------------------------------------------------------------
// Recording (called from sema)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct RawDef {
    key: SymKey,
    file: usize,
    name: String,
    name_span: Span,
    full_span: Span,
    container: Option<SymKey>,
    detail: Option<String>,
}

#[derive(Default)]
pub(crate) struct Recorder {
    defs: Vec<RawDef>,
    refs: Vec<(usize, Span, SymKey)>,
    recvs: Vec<MemberRecv>,
    ns_defined: HashSet<String>,
    /// Local constants by scope position (fn depth, scope, entry).
    local_consts: HashMap<(usize, usize, usize), SymKey>,
}

/// Facts gathered from sema after analysis.
pub(crate) struct Extras {
    files: Vec<FileInfo>,
    imports: Vec<Vec<(Option<String>, Option<String>, SymKey)>>,
    ns_members: Vec<(String, Vec<(String, SymKey)>)>,
    std_info: HashMap<SymKey, (SymbolKind, String)>,
}

/// Signatures of the compiler-lowered stdlib functions (by path, so new
/// specials only lose their parameter list until listed here).
fn special_sig(ns: &str, name: &str) -> String {
    match (ns, name) {
        ("io", "println") => "func println(args ...any)".into(),
        ("io", "print") => "func print(args ...any)".into(),
        ("fmt", "Format") => "func Format(template string, args ...any) string".into(),
        ("task", "All") => "func All(tasks ...Task<T>) Task<void>".into(),
        ("errors", "New") => "func New(msg string) error".into(),
        ("errors", "As") => "func As(err error, target *T) bool".into(),
        _ => format!("func {name}(...)"),
    }
}

impl<'a> Sema<'a> {
    fn ide_on(&self) -> bool {
        self.ide.is_some()
    }

    /// Checking stdlib code written in Zell (`collections`, `net`,
    /// ...): its spans are not in any user file, so nothing is recorded.
    fn ide_in_std(&self) -> bool {
        let std = self.nss.get(self.cur_ns).is_some_and(|n| n.is_std);
        // Unit mode: a user file declaring a std namespace (editing the
        // stdlib sources) is recorded like any user file.
        std && !(self.unit.is_some() && self.cur_file < self.files.len() && self.file_scopes.get(self.cur_file).map(|s| s.ns) == Some(self.cur_ns))
    }

    /// Records a use of `key` at `span` in the current file.
    pub(crate) fn ide_ref(&mut self, span: Span, key: SymKey) {
        let file = self.cur_file;
        if self.ide_in_std() {
            return;
        }
        if let Some(r) = &mut self.ide {
            if span != Span::DUMMY {
                r.refs.push((file, span, key));
            }
        }
    }

    /// Records the receiver type of the member access `base.name` (`name`
    /// may be empty: `x.` being typed), for member completion.
    pub(crate) fn ide_member_recv(&mut self, name: Span, ty: TypeId) {
        let file = self.cur_file;
        if self.ide_in_std() || name == Span::DUMMY || self.tt.is_invalid(ty) {
            return;
        }
        if let Some(r) = &mut self.ide {
            r.recvs.push(MemberRecv { file, name_span: name, ty });
        }
    }

    /// Records a call of method `fid`: a method of a stdlib struct by its
    /// path (`testing.T.Fatal`, `task.Channel.Send`), which the std block
    /// declares from the `std/` sources (session/stdblock.rs).
    pub(crate) fn ide_method_ref(&mut self, span: Span, fid: FuncId) {
        if !self.ide_on() {
            return;
        }
        let f = &self.funcs[fid.0 as usize];
        // Instances of std templates live in the caller's namespace.
        let std_owner = match f.kind {
            FuncKind::Method { struct_id, .. } => self.gen.inst_structs.get(&struct_id).map(|(t, _)| &self.gen.templates[*t as usize]).filter(|t| t.is_std),
            _ => None,
        };
        let key = match std_owner {
            Some(t) => SymKey::Std(format!("{}.{}.{}", self.nss[t.ns].path, t.name, f.name)),
            None => SymKey::Func(fid.0),
        };
        self.ide_ref(span, key);
    }

    /// Records a declaration in the current file.
    pub(crate) fn ide_def(&mut self, key: SymKey, name: &ast::Ident, full_span: Span, container: Option<SymKey>) {
        // Synthesized std declarations (`errors.Frame`) are not in any file.
        if name.span == Span::DUMMY || self.ide_in_std() {
            return;
        }
        let file = self.cur_file;
        if let Some(r) = &mut self.ide {
            r.defs.push(RawDef { key, file, name: name.name.clone(), name_span: name.span, full_span, container, detail: None });
        }
    }

    /// Container key for top-level declarations of the current namespace.
    pub(crate) fn ide_cur_ns(&self) -> Option<SymKey> {
        self.ide_on().then(|| SymKey::Ns(self.nss[self.cur_ns].path.clone()))
    }

    pub(crate) fn member_key(&self, m: Member) -> Option<SymKey> {
        Some(match m {
            Member::Alias(_) => return None,
            Member::Struct(i) => SymKey::Struct(i),
            Member::Interface(i) => SymKey::Interface(i),
            Member::Enum(i) => SymKey::Enum(i),
            Member::Func(f) => SymKey::Func(f.0),
            Member::Global(g) => SymKey::Global(g.0),
            Member::StdFunc(i) => SymKey::Std(stdlib::FUNCS[i].path()),
            Member::Special(sp) => {
                let (ns, name, _) = stdlib::SPECIALS.iter().find(|x| x.2 == sp)?;
                SymKey::Std(format!("{ns}.{name}"))
            }
            Member::StdConst(i) => SymKey::Std(format!("{}.{}", stdlib::CONSTS[i].0, stdlib::CONSTS[i].1)),
            Member::Ns(n) => SymKey::Ns(self.nss[n].path.clone()),
            // Stdlib templates (`http.Get`, `task.Channel`) by path, so
            // tools can find their source in `std/` (crate::stdindex).
            Member::Generic(t) => {
                let tm = &self.gen.templates[t as usize];
                if !tm.is_std {
                    return None;
                }
                SymKey::Std(format!("{}.{}", self.nss[tm.ns].path, tm.name))
            }
        })
    }

    /// Records the resolution of one path segment.
    pub(crate) fn ide_res(&mut self, seg: &ast::Ident, res: PathRes) {
        if !self.ide_on() {
            return;
        }
        let key = match res {
            PathRes::Member(m) => self.member_key(m),
            PathRes::Builtin(_) | PathRes::BuiltinType(_) => Some(SymKey::Builtin(seg.name.clone())),
            PathRes::Variant(e, v) => Some(SymKey::Variant(e, v)),
            PathRes::StaticMethod(f) | PathRes::MethodExpr(f, _) => Some(SymKey::Func(f.0)),
        };
        if let Some(k) = key {
            self.ide_ref(seg.span, k);
        }
    }

    /// Records a use of a local variable or local constant named `id`
    /// (looked up exactly like [`Sema::lookup_local`]).
    pub(crate) fn ide_local_ref(&mut self, id: &ast::Ident) {
        if !self.ide_on() {
            return;
        }
        for d in (0..self.fns.len()).rev() {
            let f = &self.fns[d];
            for (si, scope) in f.scopes.iter().enumerate().rev() {
                if let Some((ei, (_, e))) = scope.iter().enumerate().rev().find(|(_, (n, _))| *n == id.name) {
                    let key = match e {
                        ScopeEntry::Local(l) => f.func.map(|func| SymKey::Local(func.0, l.0)),
                        ScopeEntry::Const(_) => self.ide.as_ref().and_then(|r| r.local_consts.get(&(d, si, ei)).cloned()),
                    };
                    if let Some(k) = key {
                        self.ide_ref(id.span, k);
                    }
                    return;
                }
            }
        }
    }

    /// Records a local declared by [`Sema::declare_local`].
    pub(crate) fn ide_def_local(&mut self, name: &str, id: LocalId, span: Span) {
        if !self.ide_on() || name == "_" || name == "this" {
            return;
        }
        let Some(func) = self.fns.last().and_then(|f| f.func) else { return };
        let ident = ast::Ident { name: name.to_string(), span };
        self.ide_def(SymKey::Local(func.0, id.0), &ident, span, Some(SymKey::Func(func.0)));
    }

    /// Records a local constant just pushed onto the innermost scope.
    pub(crate) fn ide_def_local_const(&mut self, c: &ast::ConstDecl, ty: Option<TypeId>) {
        if !self.ide_on() || self.ide_in_std() {
            return;
        }
        let d = self.fns.len() - 1;
        let si = self.fns[d].scopes.len() - 1;
        let ei = self.fns[d].scopes[si].len() - 1;
        let key = SymKey::LocalConst(self.cur_file, c.name.span.start);
        let ty = ty.map(|t| format!(" {}", self.tn(t))).unwrap_or_default();
        let container = self.fns[d].func.map(|f| SymKey::Func(f.0));
        let file = self.cur_file;
        let r = self.ide.as_mut().unwrap();
        r.local_consts.insert((d, si, ei), key.clone());
        r.defs.push(RawDef {
            key,
            file,
            name: c.name.name.clone(),
            name_span: c.name.span,
            full_span: c.span,
            container,
            detail: Some(format!("const {}{ty}", c.name.name)),
        });
    }

    /// Records the `namespace a.b.c` line of a file: the first file
    /// declaring a namespace holds its definition, the others refer to it.
    pub(crate) fn ide_namespace_decl(&mut self, file: &ast::File) {
        if !self.ide_on() {
            return;
        }
        let segs = &file.namespace.segments;
        for i in 0..segs.len() {
            let path = segs[..=i].iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(".");
            let key = SymKey::Ns(path.clone());
            let r = self.ide.as_mut().unwrap();
            if r.ns_defined.insert(path) {
                let full = if i + 1 == segs.len() { file.span } else { segs[i].span };
                let parent = (i > 0).then(|| SymKey::Ns(segs[..i].iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(".")));
                self.ide_def(key, &segs[i], full, parent);
            } else {
                self.ide_ref(segs[i].span, key);
            }
        }
    }

    /// Records the segments of an import path; `member` is the imported
    /// declaration for `import a.b.T`.
    pub(crate) fn ide_import(&mut self, path: &ast::Path, member: Option<Member>) {
        if !self.ide_on() {
            return;
        }
        let segs = &path.segments;
        let n_ns = if member.is_some() { segs.len() - 1 } else { segs.len() };
        for i in 0..n_ns {
            let p = segs[..=i].iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(".");
            if self.ns_index.contains_key(&p) {
                self.ide_ref(segs[i].span, SymKey::Ns(p));
            }
        }
        if let Some(k) = member.and_then(|m| self.member_key(m)) {
            self.ide_ref(segs[segs.len() - 1].span, k);
        }
    }

    /// File, namespace and std facts for the index: the member lists of
    /// the namespaces `want` selects, signatures of std members when
    /// `std_info`; also the names of the type parameters.
    pub(crate) fn ide_collect_extras_for(&mut self, want: &dyn Fn(&str) -> bool, with_std: bool) -> (Extras, Vec<String>) {
        let mut files = Vec::new();
        let mut imports = Vec::new();
        for (fi, f) in self.files.iter().enumerate() {
            files.push(FileInfo { namespace: f.namespace.to_string(), namespace_span: f.namespace.span, span: f.span, imports: Vec::new() });
            let mut list = Vec::new();
            if let Some(scope) = self.file_scopes.get(fi) {
                for e in &scope.imports {
                    match e {
                        ImportEntry::Glob(ns) => list.push((Some(self.nss[*ns].path.clone()), None, SymKey::Ns(self.nss[*ns].path.clone()))),
                        ImportEntry::Named(name, m, _) => {
                            if let Some(k) = self.member_key(*m) {
                                list.push((None, Some(name.clone()), k));
                            }
                        }
                    }
                }
            }
            imports.push(list);
        }
        let mut ns_members = Vec::new();
        let mut std_info = HashMap::new();
        for ns in &self.nss {
            if !want(&ns.path) && !(with_std && ns.is_std) {
                continue;
            }
            let mut members: Vec<(String, SymKey)> = Vec::new();
            for (name, (m, _, _)) in &ns.members {
                let Some(k) = self.member_key(*m) else { continue };
                if ns.is_std && with_std {
                    let info = match m {
                        Member::StdFunc(i) => {
                            let f = stdlib::FUNCS[*i];
                            let ret = if f.ret == "void" { String::new() } else { format!(" {}", f.ret) };
                            Some((SymbolKind::Func, format!("func {}({}){ret}", f.name, f.params)))
                        }
                        Member::Special(_) => Some((SymbolKind::Func, special_sig(&ns.path, name))),
                        Member::StdConst(i) => Some((SymbolKind::Const, format!("const {} = {}", name, stdlib::CONSTS[*i].2))),
                        Member::Generic(t) => Some(match self.gen.templates[*t as usize].decl {
                            super::generics::TmplDecl::Struct(_) => (SymbolKind::Struct, format!("struct {name}")),
                            super::generics::TmplDecl::Enum(_) => (SymbolKind::Enum, format!("enum {name}")),
                            _ => (SymbolKind::Func, format!("func {name}")),
                        }),
                        _ => None,
                    };
                    if let Some(info) = info {
                        std_info.insert(k.clone(), info);
                    }
                }
                members.push((name.clone(), k));
            }
            members.sort_by(|a, b| a.0.cmp(&b.0));
            if want(&ns.path) {
                ns_members.push((ns.path.clone(), members));
            }
        }
        let names = self.gen.tparams.iter().map(|p| p.name.clone()).collect();
        (Extras { files, imports, ns_members, std_info }, names)
    }
}

// ---------------------------------------------------------------------------
// Building the index
// ---------------------------------------------------------------------------

/// Declaration names without the namespace prefix, for signatures (and
/// the names of type parameters, when known).
struct ShortNames<'p>(&'p Program, &'p [String]);

impl DeclNames for ShortNames<'_> {
    fn param_name(&self, id: u32) -> String {
        self.1.get(id as usize).cloned().unwrap_or_else(|| format!("T#{id}"))
    }
    fn struct_name(&self, id: u32) -> String {
        self.0.structs.get(id as usize).map(|s| s.name.clone()).unwrap_or_default()
    }
    fn interface_name(&self, id: u32) -> String {
        self.0.interfaces.get(id as usize).map(|s| s.name.clone()).unwrap_or_default()
    }
    fn enum_name(&self, id: u32) -> String {
        self.0.enums.get(id as usize).map(|s| s.name.clone()).unwrap_or_default()
    }
}

/// Short spelling of a type (`[]User`, `*Account`).
pub fn short_type_name(p: &Program, t: TypeId) -> String {
    type_name(&p.types, t, &ShortNames(p, &[]))
}

/// Signature of a function: `func Name(a int) T`, with `static`/`async`
/// prefixes and `Struct.` qualification for methods when `qualify`.
pub fn func_signature(p: &Program, fid: u32, qualify: bool) -> String {
    func_signature_with(p, &[], fid, qualify)
}

/// [`func_signature`] with the names of type parameters.
fn func_signature_with(p: &Program, names: &[String], fid: u32, qualify: bool) -> String {
    let f = &p.funcs[fid as usize];
    let tn = |t| type_name(&p.types, t, &ShortNames(p, names));
    let mut s = String::new();
    let mut name = f.name.clone();
    if let FuncKind::Method { struct_id, is_static } = f.kind {
        if is_static {
            s.push_str("static ");
        }
        if qualify {
            name = format!("{}.{}", p.structs[struct_id as usize].name, f.name);
        }
    }
    if f.is_async {
        s.push_str("async ");
    }
    let params: Vec<String> = f
        .params
        .iter()
        .filter_map(|l| f.locals.get(l.0 as usize))
        .filter(|l| l.kind != LocalKind::This)
        .map(|l| format!("{} {}", l.name, tn(l.ty)))
        .collect();
    let ret = if matches!(p.types.get(f.ret), Type::Void) { String::new() } else { format!(" {}", tn(f.ret)) };
    s.push_str(&format!("func {name}({}){ret}", params.join(", ")));
    s
}

/// The `//` comment block directly above `start` (attribute lines between
/// the comment and the declaration are skipped).
pub fn doc_comment(text: &str, start: u32) -> Option<String> {
    let start = (start as usize).min(text.len());
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let mut lines = Vec::new();
    let mut end = line_start;
    while end > 0 {
        let prev_end = end - 1; // the '\n' ending the previous line
        let prev_start = text[..prev_end].rfind('\n').map_or(0, |i| i + 1);
        let line = text[prev_start..prev_end].trim();
        if let Some(c) = line.strip_prefix("//") {
            lines.push(c.strip_prefix(' ').unwrap_or(c).trim_end().to_string());
        } else if line.starts_with('@') && lines.is_empty() {
            // attribute line above the declaration
        } else {
            break;
        }
        end = prev_start;
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

/// Narrows a declaration span to the name it starts with (lambda params
/// are recorded with their whole `name type` span).
fn narrow(text: &str, span: Span, name: &str) -> Span {
    let s = span.start as usize;
    if span.end - span.start != name.len() as u32 && text.get(s..s + name.len()) == Some(name) {
        Span::new(span.start, span.start + name.len() as u32)
    } else {
        span
    }
}

/// Builds the index from the records of an analysis; `names` are the
/// names of its type parameters.
pub(crate) fn build_with(rec: Recorder, extra: Extras, p: &Program, texts: &[&str], names: &[String]) -> SymbolIndex {
    let mut idx = SymbolIndex::default();
    let tn = |t| type_name(&p.types, t, &ShortNames(p, names));
    // Containers are resolved after all symbols exist.
    let mut containers: Vec<(SymbolId, SymKey)> = Vec::new();
    for d in &rec.defs {
        let text = texts.get(d.file).copied().unwrap_or("");
        let name_span = narrow(text, d.name_span, &d.name);
        let mut sym = Symbol {
            id: SymbolId(0),
            key: d.key.clone(),
            kind: SymbolKind::Local,
            name: d.name.clone(),
            full_name: d.name.clone(),
            loc: Some(Location { file: d.file, name_span, full_span: d.full_span }),
            signature: String::new(),
            ty: None,
            doc: None,
            container: None,
            is_static: false,
            is_async: false,
            is_readonly: false,
            is_std: false,
        };
        let mut want_doc = true;
        match &d.key {
            SymKey::Ns(path) => {
                sym.kind = SymbolKind::Namespace;
                sym.full_name = path.clone();
                sym.signature = format!("namespace {path}");
                want_doc = false;
            }
            SymKey::Struct(i) => {
                let st = &p.structs[*i as usize];
                sym.kind = SymbolKind::Struct;
                sym.full_name = st.full_name.clone();
                let mut s = format!("struct {}", st.name);
                if !st.fields.is_empty() {
                    s.push_str(" {\n");
                    for f in st.fields.iter().take(24) {
                        if f.embedded {
                            s.push_str(&format!("    {}\n", tn(f.ty)));
                        } else {
                            s.push_str(&format!("    {} {}\n", f.name, tn(f.ty)));
                        }
                    }
                    if st.fields.len() > 24 {
                        s.push_str("    ...\n");
                    }
                    s.push('}');
                }
                sym.signature = s;
            }
            SymKey::Interface(i) => {
                let it = &p.interfaces[*i as usize];
                sym.kind = SymbolKind::Interface;
                sym.full_name = it.full_name.clone();
                let mut s = format!("interface {} {{\n", it.name);
                for m in &it.methods {
                    s.push_str(&format!("    {}\n", iface_method_sig(p, names, m)));
                }
                s.push('}');
                sym.signature = s;
            }
            SymKey::Enum(i) => {
                let e = &p.enums[*i as usize];
                sym.kind = SymbolKind::Enum;
                sym.full_name = e.full_name.clone();
                let vs: Vec<String> = e.variants.iter().map(|v| variant_sig(p, names, v)).collect();
                sym.signature = format!("enum {} {{ {} }}", e.name, vs.join(", "));
            }
            SymKey::Variant(e, v) => {
                let en = &p.enums[*e as usize];
                sym.kind = SymbolKind::Variant;
                sym.full_name = format!("{}.{}", en.full_name, d.name);
                sym.is_readonly = true;
                if let Some(var) = en.variants.get(*v as usize) {
                    sym.signature = format!("{}.{}", en.name, variant_sig(p, names, var));
                }
            }
            SymKey::Field(s, f) => {
                let st = &p.structs[*s as usize];
                sym.kind = SymbolKind::Field;
                sym.full_name = format!("{}.{}", st.full_name, d.name);
                if let Some(field) = st.fields.get(*f as usize) {
                    sym.ty = Some(field.ty);
                    sym.signature = format!("{} {}", field.name, tn(field.ty));
                }
            }
            SymKey::IfaceMethod(i, m) => {
                let it = &p.interfaces[*i as usize];
                sym.kind = SymbolKind::Method;
                sym.full_name = format!("{}.{}", it.full_name, d.name);
                if let Some(ms) = it.methods.get(*m as usize) {
                    sym.signature = iface_method_sig(p, names, ms);
                }
            }
            SymKey::Func(f) => {
                let func = &p.funcs[*f as usize];
                sym.kind = if matches!(func.kind, FuncKind::Method { .. }) { SymbolKind::Method } else { SymbolKind::Func };
                sym.full_name = func.full_name.clone();
                sym.is_static = matches!(func.kind, FuncKind::Method { is_static: true, .. });
                sym.is_async = func.is_async;
                sym.signature = func_signature_with(p, names, *f, false);
            }
            SymKey::Global(g) => {
                let gl = &p.globals[*g as usize];
                sym.full_name = gl.full_name.clone();
                sym.ty = Some(gl.ty);
                if gl.is_const {
                    sym.kind = SymbolKind::Const;
                    sym.is_readonly = true;
                    let value = gl.value.as_ref().map(|v| format!(" = {}", crate::hir::dump::const_str(v))).unwrap_or_default();
                    sym.signature = format!("const {} {}{value}", gl.name, tn(gl.ty));
                } else {
                    sym.kind = SymbolKind::Global;
                    sym.signature = format!("var {} {}", gl.name, tn(gl.ty));
                }
            }
            SymKey::Local(f, l) => {
                want_doc = false;
                if let Some(local) = p.funcs.get(*f as usize).and_then(|func| func.locals.get(*l as usize)) {
                    sym.ty = Some(local.ty);
                    if local.kind == LocalKind::Param {
                        sym.kind = SymbolKind::Param;
                        sym.signature = format!("{} {}", local.name, tn(local.ty));
                    } else {
                        sym.signature = format!("var {} {}", local.name, tn(local.ty));
                    }
                }
            }
            SymKey::LocalConst(..) => {
                sym.kind = SymbolKind::Const;
                sym.is_readonly = true;
                sym.signature = d.detail.clone().unwrap_or_default();
            }
            SymKey::Std(_) | SymKey::Builtin(_) => {}
        }
        if want_doc {
            sym.doc = doc_comment(text, d.full_span.start);
        }
        let id = idx.add(sym);
        if let Some(c) = &d.container {
            containers.push((id, c.clone()));
        }
    }
    // Std/builtin symbols: namespace members first, then anything referenced.
    let ensure = |idx: &mut SymbolIndex, key: &SymKey| -> Option<SymbolId> {
        if let Some(id) = idx.lookup(key) {
            return Some(id);
        }
        Some(idx.add(implicit_symbol(key, extra.std_info.get(key).cloned())?))
    };
    for (path, members) in &extra.ns_members {
        let mut list = Vec::new();
        for (name, key) in members {
            if let Some(id) = ensure(&mut idx, key) {
                list.push((name.clone(), id));
                if let SymKey::Std(_) = key {
                    containers.push((id, SymKey::Ns(path.clone())));
                }
            }
        }
        idx.ns_members.insert(path.clone(), list);
    }
    for (id, ckey) in containers {
        let mut ckey = ckey;
        // Locals of lambdas belong to the enclosing function.
        while let SymKey::Func(f) = ckey {
            match p.funcs.get(f as usize).map(|x| x.kind) {
                Some(FuncKind::Lambda { parent }) => ckey = SymKey::Func(parent.0),
                _ => break,
            }
        }
        let c = ensure(&mut idx, &ckey).or_else(|| idx.lookup(&ckey));
        if c != Some(id) {
            idx.symbols[id.0 as usize].container = c;
        }
    }
    idx.set_member_recvs(rec.recvs);
    let mut seen = HashSet::new();
    for (file, span, key) in rec.refs {
        let Some(sym) = ensure(&mut idx, &key).or_else(|| idx.lookup(&key)) else { continue };
        if !seen.insert((file, span.start, span.end, sym)) {
            continue;
        }
        let i = idx.references.len();
        idx.references.push(Reference { file, span, symbol: sym });
        idx.refs_by_symbol.entry(sym).or_default().push(i);
    }
    for (fi, mut f) in extra.files.into_iter().enumerate() {
        for (glob, named, key) in &extra.imports[fi] {
            match (glob, named) {
                (Some(path), _) => f.imports.push(ImportInfo::Glob(path.clone())),
                (None, Some(name)) => {
                    if let Some(id) = ensure(&mut idx, key).or_else(|| idx.lookup(key)) {
                        f.imports.push(ImportInfo::Named(name.clone(), id));
                    }
                }
                _ => {}
            }
        }
        idx.files.push(Some(f));
    }
    idx
}

/// `task.Channel<int>.Send` without its type arguments.
pub fn strip_type_args(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// The symbol of a namespace, std member or builtin that no declaration
/// defines (`info`: kind and signature of a std member).
pub(crate) fn implicit_symbol(key: &SymKey, info: Option<(SymbolKind, String)>) -> Option<Symbol> {
    let (kind, name, full, sig) = match key {
        SymKey::Std(path) => {
            let name = path.rsplit('.').next().unwrap_or(path).to_string();
            let (kind, sig) = info.unwrap_or((SymbolKind::Func, format!("func {name}")));
            (kind, name, path.clone(), sig)
        }
        SymKey::Ns(path) => {
            let name = path.rsplit('.').next().unwrap_or(path).to_string();
            (SymbolKind::Namespace, name, path.clone(), format!("namespace {path}"))
        }
        SymKey::Builtin(n) => {
            let is_fn = matches!(n.as_str(), "len" | "append" | "delete" | "panic" | "typeof") || (n == "error");
            let sig = match n.as_str() {
                "len" => "func len(v) int".to_string(),
                "append" => "func append(s []T, v T) []T".to_string(),
                "delete" => "func delete(m map[K]V, k K)".to_string(),
                "panic" => "func panic(msg string)".to_string(),
                "error" => "func error(msg string) error  (also the builtin error interface)".to_string(),
                _ => format!("type {n}"),
            };
            (if is_fn { SymbolKind::BuiltinFunc } else { SymbolKind::BuiltinType }, n.clone(), n.clone(), sig)
        }
        _ => return None,
    };
    let is_std = !matches!(key, SymKey::Ns(p) if !crate::sema::stdlib::is_std_namespace(p));
    Some(Symbol {
        id: SymbolId(0),
        key: key.clone(),
        kind,
        name,
        full_name: full,
        loc: None,
        signature: sig,
        ty: None,
        doc: None,
        container: None,
        is_static: false,
        is_async: false,
        is_readonly: kind == SymbolKind::Const,
        is_std,
    })
}

fn iface_method_sig(p: &Program, names: &[String], m: &hir::MethodSig) -> String {
    let tn = |t| type_name(&p.types, t, &ShortNames(p, names));
    let ps: Vec<String> = m.params.iter().map(|t| tn(*t)).collect();
    let ret = if matches!(p.types.get(m.ret), Type::Void) { String::new() } else { format!(" {}", tn(m.ret)) };
    format!("func {}({}){ret}", m.name, ps.join(", "))
}

fn variant_sig(p: &Program, names: &[String], v: &hir::Variant) -> String {
    if v.fields.is_empty() {
        v.name.clone()
    } else {
        let fs: Vec<String> = v.fields.iter().map(|(n, t)| format!("{n} {}", type_name(&p.types, *t, &ShortNames(p, names)))).collect();
        format!("{}({})", v.name, fs.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_comment_block_above_declaration() {
        let text = "x\n\n// First line.\n// Second line.\n@inline\nfunc F() {}\n";
        let start = text.find("@inline").unwrap() as u32;
        assert_eq!(doc_comment(text, start).as_deref(), Some("First line.\nSecond line."));
        let start = text.find("func").unwrap() as u32;
        assert_eq!(doc_comment(text, start).as_deref(), Some("First line.\nSecond line."));
        assert_eq!(doc_comment("x\nfunc F() {}", 2), None);
    }
}
