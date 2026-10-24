//! Analysis runs for incremental IDE analysis (`crate::session`).
//!
//! The language server analyses a project as *units* (the files of one
//! namespace): a unit is re-analysed together with the declarations of the
//! files it can reach, and the results of the units are combined. To make
//! the combination equal to one analysis of the whole project, a run in
//! *unit mode* ([`UnitState`]) differs from a compiler run in a few ways:
//!
//! * files are either *full* (bodies checked) or *declaration-only* (their
//!   declarations, globals and generic templates are analysed, function
//!   bodies are not);
//! * every diagnostic is attributed to the files whose analysis produces it
//!   ([`RunOutput::diag_origins`]): its own file, the file whose body was
//!   being checked, or — for code shared through a cache (generic
//!   instances, the `data` model) — every file that asked for it;
//! * checks over the whole program are left to the caller: unused imports
//!   (reported only when the program has no other error) and the `main`
//!   signature;
//! * bodies of stdlib functions that are not instances are not checked
//!   (they cannot report anything about user code).
//!
//! `reflect.TypesWith<A>()` depends on the whole program, but only its
//! value does (its type is always `[]*TypeInfo` and it reports nothing
//! program-wide), so a unit run needs no special handling: its list may be
//! partial, and the list that matters is the one computed by the compiler
//! run over all packages (`Sema::types_with_finish`).

use super::generics::Inst;
use super::index::{self, SymbolIndex};
use super::*;
use crate::hir::Program;
use std::collections::BTreeSet;

/// Who asked for a cached computation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Req {
    File(usize),
    Node(u32),
}

/// A cached computation whose diagnostics belong to its requesters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MemoKey {
    Inst(InstKey),
    OrmUniverse,
    OrmModel(TypeId),
}

/// [`Inst`] as a hashable key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum InstKey {
    Func(u32),
    Struct(u32),
    Enum(u32),
}

impl From<Inst> for InstKey {
    fn from(i: Inst) -> InstKey {
        match i {
            Inst::Func(f) => InstKey::Func(f.0),
            Inst::Struct(s) => InstKey::Struct(s),
            Inst::Enum(e) => InstKey::Enum(e),
        }
    }
}

/// The `main` function of a namespace.
#[derive(Clone, Debug)]
pub struct MainInfo {
    pub ns: String,
    pub file: usize,
    pub span: Span,
    /// It takes no parameters and returns nothing (or `void!`).
    pub ok: bool,
}

/// Unit-mode state of a run (see the module docs).
#[derive(Default)]
pub(crate) struct UnitState {
    /// Per file: function bodies are checked.
    pub full: Vec<bool>,
    /// The function bodies phase is running.
    pub in_bodies: bool,
    /// File of the function body being checked.
    pub body_file: usize,
    /// Requesters of each node.
    pub nodes: Vec<Vec<Req>>,
    pub memo: HashMap<MemoKey, u32>,
    /// Instance function -> its node.
    pub func_nodes: HashMap<u32, u32>,
    /// The node whose code is being analysed.
    pub cur: Option<u32>,
    /// (index in the diagnostics, node) for diagnostics of node code.
    pub diag_nodes: Vec<(usize, Req)>,
    pub unused_imports: Vec<Diagnostic>,
}

impl UnitState {
    /// Drops the nodes of instances rolled back by an abstract check.
    pub(crate) fn rollback(&mut self, funcs: usize, structs: usize, enums: usize) {
        self.memo.retain(|k, _| match k {
            MemoKey::Inst(InstKey::Func(f)) => (*f as usize) < funcs,
            MemoKey::Inst(InstKey::Struct(s)) => (*s as usize) < structs,
            MemoKey::Inst(InstKey::Enum(e)) => (*e as usize) < enums,
            _ => true,
        });
        self.func_nodes.retain(|f, _| (*f as usize) < funcs);
    }
}

impl<'a> Sema<'a> {
    /// The requester of whatever is computed now.
    fn unit_req(&self) -> Option<Req> {
        let u = self.unit.as_ref()?;
        Some(match u.cur {
            Some(n) => Req::Node(n),
            None if u.in_bodies => Req::File(u.body_file),
            None => Req::File(self.cur_file),
        })
    }

    /// Registers a request for the cached computation `key`; returns its
    /// node (created when new).
    pub(crate) fn unit_memo(&mut self, key: MemoKey) -> Option<u32> {
        let req = self.unit_req()?;
        let u = self.unit.as_mut()?;
        let n = match u.memo.get(&key) {
            Some(&n) => n,
            None => {
                let n = u.nodes.len() as u32;
                u.nodes.push(Vec::new());
                u.memo.insert(key, n);
                n
            }
        };
        if !u.nodes[n as usize].contains(&req) {
            u.nodes[n as usize].push(req);
        }
        Some(n)
    }

    /// A new node for code about to be created (an instance whose id is
    /// not known yet); see [`Sema::unit_bind`].
    pub(crate) fn unit_new_node(&mut self) -> Option<u32> {
        let req = self.unit_req()?;
        let u = self.unit.as_mut()?;
        u.nodes.push(vec![req]);
        Some(u.nodes.len() as u32 - 1)
    }

    /// A node requested by `file` itself (abstract checks of templates).
    pub(crate) fn unit_file_node(&mut self, file: usize) -> Option<u32> {
        let u = self.unit.as_mut()?;
        u.nodes.push(vec![Req::File(file)]);
        Some(u.nodes.len() as u32 - 1)
    }

    /// Records the cache key of node `n`.
    pub(crate) fn unit_bind(&mut self, n: Option<u32>, key: MemoKey) {
        if let (Some(n), Some(u)) = (n, self.unit.as_mut()) {
            u.memo.insert(key, n);
        }
    }

    /// Records that instance function `f` belongs to node `n`.
    pub(crate) fn unit_bind_func(&mut self, n: Option<u32>, f: FuncId) {
        if let (Some(n), Some(u)) = (n, self.unit.as_mut()) {
            u.func_nodes.insert(f.0, n);
        }
    }

    /// Makes `n` the current node; returns the previous one for
    /// [`Sema::unit_leave`].
    pub(crate) fn unit_enter(&mut self, n: Option<u32>) -> Option<u32> {
        match (n, self.unit.as_mut()) {
            (Some(n), Some(u)) => u.cur.replace(n),
            (_, Some(u)) => u.cur,
            _ => None,
        }
    }

    pub(crate) fn unit_leave(&mut self, prev: Option<u32>) {
        if let Some(u) = self.unit.as_mut() {
            u.cur = prev;
        }
    }

    /// The node of instance function `f`, if any.
    pub(crate) fn unit_func_node(&self, f: FuncId) -> Option<u32> {
        self.unit.as_ref()?.func_nodes.get(&f.0).copied()
    }

    /// Called before a diagnostic is pushed: remembers its origin when it
    /// is not simply its own file.
    pub(crate) fn unit_note_diag(&mut self) {
        let idx = self.diags.items.len();
        let Some(u) = self.unit.as_mut() else { return };
        match u.cur {
            Some(n) => u.diag_nodes.push((idx, Req::Node(n))),
            None if u.in_bodies => u.diag_nodes.push((idx, Req::File(u.body_file))),
            None => {}
        }
    }

    /// In unit mode: the body of non-instance function `fid` is not
    /// checked (a declaration-only file, or stdlib code).
    pub(crate) fn unit_skips_body(&self, fid: FuncId) -> bool {
        let Some(u) = &self.unit else { return false };
        if self.gen.insts.contains_key(&fid.0) {
            return false;
        }
        let f = &self.funcs[fid.0 as usize];
        // Stdlib code: default methods of std interfaces and the functions
        // of library modules (their file index is past the user files).
        self.std_fn_ns.contains_key(&fid.0) || !u.full.get(f.file).copied().unwrap_or(false)
    }

    /// In unit mode: the body of instance `fid` is not checked when it is
    /// library code instantiated with stdlib and builtin types only (it
    /// cannot report anything about user code).
    pub(crate) fn unit_skips_instance(&self, fid: FuncId) -> bool {
        let Some(info) = self.gen.insts.get(&fid.0) else { return false };
        let tm = &self.gen.templates[info.root as usize];
        tm.is_std && !tm.generated && info.env.iter().all(|(_, t)| self.std_only_type(*t, 0))
    }

    /// `t` names no user declaration (and no type parameter).
    fn std_only_type(&self, t: TypeId, depth: usize) -> bool {
        if depth > 16 {
            return false;
        }
        let d = depth + 1;
        match self.tt.get(t) {
            Type::Pointer(x) | Type::Weak(x) | Type::Slice(x) | Type::Array(x, _) | Type::Fallible(x) | Type::Task(x) => self.std_only_type(*x, d),
            Type::Map(k, v) => self.std_only_type(*k, d) && self.std_only_type(*v, d),
            Type::Tuple(ts) => ts.iter().all(|x| self.std_only_type(*x, d)),
            Type::Func(sig) => sig.params.iter().all(|x| self.std_only_type(*x, d)) && self.std_only_type(sig.ret, d),
            Type::Struct(s) => crate::sema::stdlib::is_std_namespace(&self.structs[s.0 as usize].full_name),
            Type::Interface(i) => crate::sema::stdlib::is_std_namespace(&self.interfaces[i.0 as usize].full_name),
            Type::Enum(e) => crate::sema::stdlib::is_std_namespace(&self.enums[e.0 as usize].full_name),
            Type::Param(_) | Type::Infer(_) => false,
            _ => true,
        }
    }

    /// The `main` functions of the user namespaces of full files.
    fn unit_mains(&self) -> Vec<MainInfo> {
        let Some(u) = &self.unit else { return Vec::new() };
        let mut out = Vec::new();
        for ns in self.nss.iter().filter(|n| !n.is_std) {
            if let Some((Member::Func(f), _, _)) = ns.members.get("main") {
                let func = &self.funcs[f.0 as usize];
                if !u.full.get(func.file).copied().unwrap_or(false) {
                    continue;
                }
                let sig = &self.sigs[f.0 as usize];
                let void_ok = matches!(self.tt.get(sig.ret), Type::Void) || matches!(self.tt.fallible_inner(sig.ret).map(|t| self.tt.get(t)), Some(Type::Void));
                out.push(MainInfo { ns: ns.path.clone(), file: func.file, span: func.span, ok: sig.params.is_empty() && void_ok });
            }
        }
        out
    }
}

/// Everything a unit-mode run produces.
pub struct RunOutput {
    pub program: Program,
    pub index: SymbolIndex,
    pub diags: Vec<Diagnostic>,
    /// Per diagnostic: the files (indices of the run) whose analysis
    /// reports it.
    pub diag_origins: Vec<Vec<usize>>,
    /// Unused imports of full files, reported only if the whole program
    /// has no other error.
    pub unused_imports: Vec<Diagnostic>,
    pub mains: Vec<MainInfo>,
    /// Names of the type parameters (`Type::Param` ids).
    pub tparams: Vec<String>,
}

/// Analyses `files` in unit mode: `full[i]` tells whether the bodies of
/// `files[i]` are checked. `texts[i]` is the source of `files[i]`;
/// `index_ns` selects the namespaces whose member lists go into the index.
/// Returns `None` when `cancel` was set.
pub fn run_unit(files: &[&ast::File], texts: &[&str], full: Vec<bool>, index_ns: &dyn Fn(&str) -> bool, cancel: Option<&std::sync::atomic::AtomicBool>) -> Option<RunOutput> {
    let mut diags = Diagnostics::new();
    let mut s = Sema::new(files, &mut diags);
    s.ide = Some(Box::default());
    let lint_files = full.clone();
    s.unit = Some(Box::new(UnitState { full, ..Default::default() }));
    s.cancel = cancel;
    s.run();
    if s.cancelled() {
        return None;
    }
    let mains = s.unit_mains();
    let (extra, names) = s.ide_collect_extras_for(index_ns, false);
    let rec = *s.ide.take().unwrap();
    let unit = *s.unit.take().unwrap();
    let program = s.finish();
    let index = index::build_with(rec, extra, &program, texts, &names);
    let names: Vec<String> = names;
    if !diags.has_errors() {
        crate::sema::lint::check(&program, texts, &|f| lint_files.get(f).copied().unwrap_or(false), &mut diags);
    }
    // Origins: own file, else the requesters of the node (transitively).
    let mut node_files: Vec<Option<BTreeSet<usize>>> = vec![None; unit.nodes.len()];
    fn files_of(n: usize, nodes: &[Vec<Req>], memo: &mut [Option<BTreeSet<usize>>], stack: &mut Vec<usize>) -> BTreeSet<usize> {
        if let Some(s) = &memo[n] {
            return s.clone();
        }
        if stack.contains(&n) {
            return BTreeSet::new();
        }
        stack.push(n);
        let mut out = BTreeSet::new();
        for r in &nodes[n] {
            match *r {
                Req::File(f) => {
                    out.insert(f);
                }
                Req::Node(m) => out.extend(files_of(m as usize, nodes, memo, stack)),
            }
        }
        stack.pop();
        memo[n] = Some(out.clone());
        out
    }
    let mut origins: Vec<Vec<usize>> = diags.items.iter().map(|d| vec![d.file]).collect();
    let mut stack = Vec::new();
    for (i, r) in &unit.diag_nodes {
        let Some(o) = origins.get_mut(*i) else { continue };
        *o = match *r {
            Req::File(f) => vec![f],
            Req::Node(n) => files_of(n as usize, &unit.nodes, &mut node_files, &mut stack).into_iter().collect(),
        };
    }
    Some(RunOutput {
        program,
        index,
        diags: diags.items,
        diag_origins: origins,
        unused_imports: unit.unused_imports,
        mains,
        tparams: names,
    })
}

/// The index entries of the standard library: every std namespace and
/// member (library modules loaded), as one analysis of an empty program
/// sees them. Symbols other than std ones are dropped.
pub fn std_symbols() -> SymbolIndex {
    let mut diags = Diagnostics::new();
    let tokens = crate::lexer::lex("namespace __zell_std_index__\n", &mut diags);
    let file = crate::parser::parse_file(tokens, &mut diags);
    let files = [&file];
    let mut s = Sema::new(&files, &mut diags);
    s.ide = Some(Box::default());
    // Unit mode: no stdlib bodies are checked.
    s.unit = Some(Box::new(UnitState { full: vec![true], ..Default::default() }));
    s.run();
    s.load_all_modules();
    let (extra, names) = s.ide_collect_extras_for(&|p| crate::sema::stdlib::is_std_namespace(p), true);
    let rec = *s.ide.take().unwrap();
    let program = s.finish();
    index::build_with(rec, extra, &program, &[""], &names)
}
