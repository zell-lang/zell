//! Incremental analysis of a [`Session`] (docs/TOOLING.md, "Performance").
//!
//! The files are grouped into *units*: the files of one namespace (all
//! files of std namespaces form one unit). A unit depends on the units it
//! can reach: descendants of its namespace, what its files import, and the
//! namespaces under a root named by one of its identifiers (roots are
//! visible without an import), transitively. The units using `data`
//! also depend on each other (its model covers every context).
//!
//! The result of a unit (a [`Fragment`]) is cached under a key made of the
//! texts of its files and the *export signatures* of the files of the units
//! it depends on (see [`super::facts`]): editing a function body changes
//! only the key of the edited unit; changing a declaration also changes the
//! keys of the units that can see it. Dirty units are re-analysed one by
//! one, each together with its dependencies as declaration-only files (see
//! [`crate::sema::units`]); when that would cost more than analysing the
//! whole project (the first analysis, or an edit visible everywhere), one
//! run over all files produces the fragments of every unit.
//!
//! The types of the fragments are merged into one program (declarations by
//! full name, see [`super::remap`]) that grows only with new declarations
//! and is rebuilt when it holds too many stale ones.

use super::assemble::{self, FileView, FragMap, UnitView};
use super::fragment::{self, Fragment, RunFile};
use super::remap::{Copier, DeclStore};
use super::{complete_fixes, Analysis, FileState, Session};
use crate::ast;
use crate::sema::units;
use crate::types::TypeId;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Lines of source a run costs on top of its files (stdlib registration).
const RUN_OVERHEAD_LINES: usize = 500;

struct UnitEntry {
    key: u64,
    /// Files of other units the fragment has diagnostics in (code of
    /// theirs it instantiated), with their text hashes: positions there
    /// are not part of `key`.
    foreign: Vec<(String, u64)>,
    frag: Arc<Fragment>,
    /// (merged program version, where the fragment's types went)
    map: Option<(u64, Arc<FragMap>)>,
}

/// The merged types and declarations of all fragments.
struct Merged {
    store: DeclStore,
    version: u64,
    /// Rebuild when the type table grows past this size.
    rebuild_at: usize,
}

/// Results kept between analyses (shared by clones of a session).
pub(crate) struct Cache {
    units: HashMap<String, UnitEntry>,
    merged: Merged,
    /// An update is in progress (a panic leaves it set: start over).
    busy: bool,
    graph: Option<Graph>,
    /// Statistics of the last analysis, for tests and tuning.
    pub(crate) stats: Stats,
}

/// What the last analysis did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Units whose cached result was out of date.
    pub dirty: usize,
    /// Units analysed one by one.
    pub unit_runs: usize,
    /// A run over the whole project was made.
    pub full_run: bool,
    /// Units in the project.
    pub units: usize,
}

impl Default for Cache {
    fn default() -> Cache {
        Cache { units: HashMap::new(), merged: Merged { store: DeclStore::default(), version: 0, rebuild_at: 0 }, busy: false, graph: None, stats: Stats::default() }
    }
}

/// The unit a namespace belongs to.
fn unit_name(ns: &str) -> &str {
    if crate::sema::stdlib::is_std_namespace(ns) {
        "std"
    } else {
        ns
    }
}

#[derive(Clone)]
struct Unit {
    name: String,
    /// Global file indices, in session order.
    files: Vec<usize>,
    /// Units it depends on (itself excluded), in unit order.
    closure: Vec<usize>,
    key: u64,
    lines: usize,
}

/// The units and their dependencies, valid while no file is added,
/// removed or changes its namespace, imports or identifiers.
pub(crate) struct Graph {
    /// Hash of what the graph depends on.
    shape: u64,
    units: Vec<Unit>,
    unit_of_file: Vec<usize>,
}

/// The hash a [`Graph`] of `files` is valid for.
fn graph_shape(files: &[(&str, &FileState)]) -> u64 {
    let mut h = DefaultHasher::new();
    for (p, f) in files {
        p.hash(&mut h);
        f.facts.deps_hash.hash(&mut h);
    }
    h.finish()
}

/// The units of `files` with their keys (the graph of `cache` is reused
/// when still valid).
fn units_of_cached(files: &[(&str, &FileState)], cache: &mut Option<Graph>) -> (Vec<Unit>, Vec<usize>) {
    let shape = graph_shape(files);
    if cache.as_ref().is_none_or(|g| g.shape != shape) {
        let (units, unit_of_file) = units_of(files);
        *cache = Some(Graph { shape, units, unit_of_file });
    }
    let g = cache.as_ref().unwrap();
    let mut units = g.units.clone();
    for ui in 0..units.len() {
        let mut h = DefaultHasher::new();
        let mut lines = 0;
        for &fi in &units[ui].files {
            files[fi].0.hash(&mut h);
            files[fi].1.facts.text_hash.hash(&mut h);
            lines += files[fi].1.facts.lines;
        }
        0xffu8.hash(&mut h);
        for &u in &units[ui].closure {
            for &fi in &g.units[u].files {
                files[fi].0.hash(&mut h);
                files[fi].1.facts.sig_hash.hash(&mut h);
            }
        }
        units[ui].key = h.finish();
        units[ui].lines = lines;
    }
    (units, g.unit_of_file.clone())
}

/// Groups the files into units and computes their dependencies.
fn units_of(files: &[(&str, &FileState)]) -> (Vec<Unit>, Vec<usize>) {
    let mut units: Vec<Unit> = Vec::new();
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    let mut unit_of_file = Vec::with_capacity(files.len());
    for (i, (_, f)) in files.iter().enumerate() {
        let name = unit_name(&f.facts.namespace);
        let u = *by_name.entry(name).or_insert_with(|| {
            units.push(Unit { name: name.to_string(), files: Vec::new(), closure: Vec::new(), key: 0, lines: 0 });
            units.len() - 1
        });
        units[u].files.push(i);
        units[u].lines += f.facts.lines;
        unit_of_file.push(u);
    }
    // Namespaces with files, sorted (prefix queries) and by name.
    let mut ns_units: Vec<(&str, usize)> = files.iter().enumerate().map(|(i, (_, f))| (f.facts.namespace.as_str(), unit_of_file[i])).collect();
    ns_units.sort();
    ns_units.dedup();
    let ns_set: HashMap<&str, usize> = ns_units.iter().copied().collect();
    let roots: BTreeSet<&str> = ns_units.iter().map(|(n, _)| n.split('.').next().unwrap_or("")).collect();
    // Units of the namespaces `p` and below.
    let under = |p: &str, out: &mut Vec<usize>| {
        if let Some(&u) = ns_set.get(p) {
            out.push(u);
        }
        let prefix = format!("{p}.");
        let start = ns_units.partition_point(|(n, _)| *n < prefix.as_str());
        for (n, u) in &ns_units[start..] {
            if !n.starts_with(&prefix) {
                break;
            }
            out.push(*u);
        }
    };
    // The `data` model is built over the whole program (one model of
    // every context): the units using it see each other.
    let uses_orm = |u: &Unit| u.files.iter().any(|&fi| {
        let f = &files[fi].1.facts;
        f.has_ident("data") || f.imports.iter().any(|i| i == "data" || i.starts_with("data."))
    });
    let orm_units: Vec<usize> = (0..units.len()).filter(|&u| uses_orm(&units[u])).collect();
    let mut direct: Vec<Vec<usize>> = vec![Vec::new(); units.len()];
    for (ui, unit) in units.iter().enumerate() {
        let mut out = Vec::new();
        let mut done: HashSet<&str> = HashSet::new();
        for &fi in &unit.files {
            let facts = &files[fi].1.facts;
            if done.insert(facts.namespace.as_str()) {
                under(&facts.namespace, &mut out);
            }
            for imp in &facts.imports {
                under(imp, &mut out);
                let mut p = imp.as_str();
                while let Some(i) = p.rfind('.') {
                    p = &p[..i];
                    if let Some(&u) = ns_set.get(p) {
                        out.push(u);
                    }
                }
            }
            for r in &roots {
                if facts.has_ident(r) {
                    under(r, &mut out);
                }
            }
        }
        if orm_units.binary_search(&ui).is_ok() {
            out.extend(&orm_units);
        }
        out.sort();
        out.dedup();
        out.retain(|&u| u != ui);
        direct[ui] = out;
    }
    for ui in 0..units.len() {
        let mut seen = vec![false; units.len()];
        seen[ui] = true;
        let mut stack = direct[ui].clone();
        let mut closure = Vec::new();
        while let Some(u) = stack.pop() {
            if seen[u] {
                continue;
            }
            seen[u] = true;
            closure.push(u);
            stack.extend(direct[u].iter().copied().filter(|&v| !seen[v]));
        }
        closure.sort();
        units[ui].closure = closure;
    }
    (units, unit_of_file)
}

/// Runs the analysis of `run_files` (global indices, session order) with
/// the bodies of `full` checked, and splits it into the fragments of the
/// units `wanted`.
fn run(files: &[(&str, &FileState)], unit_of_file: &[usize], run_files: &[usize], wanted: &dyn Fn(usize) -> bool, cancel: Option<&AtomicBool>) -> Option<HashMap<usize, Fragment>> {
    let asts: Vec<&ast::File> = run_files.iter().map(|&i| &*files[i].1.cfg_ast).collect();
    let texts: Vec<&str> = run_files.iter().map(|&i| files[i].1.source.text.as_str()).collect();
    let full: Vec<bool> = run_files.iter().map(|&i| wanted(unit_of_file[i])).collect();
    let namespaces: HashSet<&str> = run_files.iter().filter(|&&i| wanted(unit_of_file[i])).map(|&i| files[i].1.facts.namespace.as_str()).collect();
    let mut out = {
        let _t = crate::build::phase("ide-sema");
        units::run_unit(&asts, &texts, full, &|p| namespaces.contains(p), cancel)?
    };
    let _t = crate::build::phase("ide-split");
    complete_fixes(&mut out.diags, &|f| texts.get(f).copied(), &|f| asts.get(f).copied());
    let rf: Vec<RunFile> = run_files.iter().zip(&texts).map(|(&i, t)| RunFile { path: files[i].0, unit: unit_of_file[i], text: t }).collect();
    Some(fragment::split(&out, &rf, wanted))
}

/// Where the types of `frag` go in `merged`.
fn map_fragment(merged: &mut DeclStore, frag: &Fragment) -> FragMap {
    let mini = &frag.mini;
    let mut c = Copier::new(&mini.prog, &mini.keys, &frag.mini_params, true);
    let types = (0..mini.prog.types.len()).map(|t| c.ty(merged, TypeId(t as u32))).collect();
    let structs = (0..mini.prog.structs.len()).map(|s| c.strukt(merged, s as u32)).collect();
    let ifaces = (0..mini.prog.interfaces.len()).map(|s| c.iface(merged, s as u32)).collect();
    let enums = (0..mini.prog.enums.len()).map(|s| c.enm(merged, s as u32)).collect();
    FragMap { types, structs, ifaces, enums }
}

/// Analyses `sess`, reusing and updating `cache`; `None` when cancelled.
pub(crate) fn analyze(sess: &Session, cache: &mut Cache, cancel: Option<&AtomicBool>) -> Option<Analysis> {
    if cache.busy {
        *cache = Cache::default();
    }
    cache.busy = true;
    let r = analyze_inner(sess, cache, cancel);
    cache.busy = false;
    if cache.stats.full_run || cache.stats.unit_runs > 0 {
        // A run frees much more than it keeps.
        crate::sema::stdfs::release_memory();
    }
    r
}

fn analyze_inner(sess: &Session, cache: &mut Cache, cancel: Option<&AtomicBool>) -> Option<Analysis> {
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let files: Vec<(&str, &FileState)> = sess.files.iter().map(|(p, f)| (p.as_str(), &**f)).collect();
    let (units, unit_of_file) = {
        let _t = crate::build::phase("ide-units");
        units_of_cached(&files, &mut cache.graph)
    };
    let text_of: HashMap<&str, u64> = files.iter().map(|(p, f)| (*p, f.facts.text_hash)).collect();
    let dirty: Vec<usize> = (0..units.len())
        .filter(|&u| {
            cache.units.get(&units[u].name).is_none_or(|e| e.key != units[u].key || e.foreign.iter().any(|(p, h)| text_of.get(p.as_str()) != Some(h)))
        })
        .collect();
    let mut stats = Stats { units: units.len(), dirty: dirty.len(), ..Default::default() };
    if !dirty.is_empty() {
        let total: usize = units.iter().map(|u| u.lines).sum::<usize>() + RUN_OVERHEAD_LINES;
        let one_by_one: usize = dirty.iter().map(|&u| units[u].lines + units[u].closure.iter().map(|&c| units[c].lines).sum::<usize>() / 4 + RUN_OVERHEAD_LINES).sum();
        if cache.units.is_empty() || one_by_one > total {
            let all: Vec<usize> = (0..files.len()).collect();
            let frags = run(&files, &unit_of_file, &all, &|_| true, cancel)?;
            stats.full_run = true;
            for (u, frag) in frags {
                let foreign = foreign_of(&frag, &text_of);
                cache.units.insert(units[u].name.clone(), UnitEntry { key: units[u].key, foreign, frag: Arc::new(frag), map: None });
            }
        } else {
            for &u in &dirty {
                if cancelled() {
                    return None;
                }
                let mut run_files: Vec<usize> = units[u].files.clone();
                for &c in &units[u].closure {
                    run_files.extend(&units[c].files);
                }
                run_files.sort();
                let mut frags = run(&files, &unit_of_file, &run_files, &|x| x == u, cancel)?;
                stats.unit_runs += 1;
                let frag = frags.remove(&u).unwrap_or_else(|| empty_fragment(&files, &units[u]));
                let foreign = foreign_of(&frag, &text_of);
                cache.units.insert(units[u].name.clone(), UnitEntry { key: units[u].key, foreign, frag: Arc::new(frag), map: None });
            }
        }
    }
    let live: HashSet<&str> = units.iter().map(|u| u.name.as_str()).collect();
    cache.units.retain(|k, _| live.contains(k.as_str()));
    // Units without any symbol (a file with only a namespace line) produce
    // no fragment from a whole-project run.
    for u in &units {
        if !cache.units.contains_key(&u.name) {
            cache.units.insert(u.name.clone(), UnitEntry { key: u.key, foreign: Vec::new(), frag: Arc::new(empty_fragment(&files, u)), map: None });
        }
    }
    // Merge the types; rebuild the merged program when mostly stale.
    let merge_t = crate::build::phase("ide-merge");
    let fresh = cache.merged.rebuild_at == 0 || cache.merged.store.types_len() > cache.merged.rebuild_at;
    if fresh {
        cache.merged.store = DeclStore::default();
        cache.merged.version += 1;
    }
    let version = cache.merged.version;
    for u in &units {
        let e = cache.units.get_mut(&u.name).unwrap();
        if e.map.as_ref().is_none_or(|(v, _)| *v != version) {
            e.map = Some((version, Arc::new(map_fragment(&mut cache.merged.store, &e.frag))));
        }
    }
    if fresh {
        cache.merged.rebuild_at = (cache.merged.store.types_len() * 2).max(20_000);
    }
    drop(merge_t);
    let _t = crate::build::phase("ide-assemble");
    let std = super::stdblock::block(sess.std.as_ref());
    let std_files: Vec<Arc<crate::span::SourceFile>> = sess.std.as_ref().map(|s| s.files.clone()).unwrap_or_default();
    let views: Vec<FileView> = files
        .iter()
        .enumerate()
        .map(|(i, (p, f))| FileView { source: &f.source, ast: &f.cfg_ast, parse_diags: &f.parse_diags, cfg_diags: &f.cfg_diags, read_only: sess.read_only.contains(*p), unit: unit_of_file[i] })
        .collect();
    let index_of: HashMap<&str, usize> = files.iter().enumerate().map(|(i, (p, _))| (*p, i)).collect();
    let unit_views: Vec<UnitView> = units
        .iter()
        .map(|u| {
            let e = &cache.units[&u.name];
            let files = e.frag.paths.iter().map(|p| index_of.get(p.as_str()).copied().unwrap_or(usize::MAX)).collect();
            UnitView { frag: &e.frag, map: &e.map.as_ref().unwrap().1, files }
        })
        .collect();
    let a = assemble::assemble(&views, &unit_views, &cache.merged.store.prog, &std, &std_files);
    cache.stats = stats;
    Some(a)
}

/// The files of other units `frag` has diagnostics in, with their hashes.
fn foreign_of(frag: &Fragment, text_of: &HashMap<&str, u64>) -> Vec<(String, u64)> {
    frag.paths[frag.n_own..].iter().map(|p| (p.clone(), text_of.get(p.as_str()).copied().unwrap_or(0))).collect()
}

/// The fragment of a unit whose files declare nothing.
fn empty_fragment(files: &[(&str, &FileState)], u: &Unit) -> Fragment {
    Fragment {
        paths: u.files.iter().map(|&i| files[i].0.to_string()).collect(),
        n_own: u.files.len(),
        symbols: Vec::new(),
        ext: Vec::new(),
        refs: Vec::new(),
        file_infos: vec![None; u.files.len()],
        ns_members: Vec::new(),
        diags: Vec::new(),
        unused_imports: Vec::new(),
        mains: Vec::new(),
        mini: DeclStore::default(),
        mini_params: Vec::new(),
        n_funcs: 0,
        n_globals: 0,
        lines: u.lines,
    }
}
