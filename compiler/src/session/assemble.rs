//! Combining the fragments of all units into one [`Analysis`]: symbols get
//! their ids, stable keys are resolved, namespace declarations are recorded
//! over the whole file list, and the checks over the whole program (lints
//! and unused imports only without errors, the entry point) are made.

use super::fragment::{FImport, FKey, Fragment, SKey, SymRef};
use super::stdblock::StdBlock;
use super::{complete_fixes, Analysis};
use crate::ast;
use crate::diagnostics::{Diagnostic, Severity};
use crate::hir::Program;
use crate::sema::index::{self, FileInfo, ImportInfo, Location, MemberRecv, Reference, SymKey, Symbol, SymbolId, SymbolIndex, SymbolKind};
use crate::span::{SourceFile, Span};
use crate::types::TypeId;
use crate::fxhash::FxHashMap;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// One session file.
pub(crate) struct FileView<'a> {
    pub source: &'a Arc<SourceFile>,
    /// After `@cfg`.
    pub ast: &'a Arc<ast::File>,
    pub parse_diags: &'a [Diagnostic],
    pub cfg_diags: &'a [Diagnostic],
    pub read_only: bool,
    /// Index into the unit list.
    pub unit: usize,
}

/// Where the mini program of a fragment went in the merged program.
#[derive(Clone, Debug, Default)]
pub(crate) struct FragMap {
    pub types: Vec<TypeId>,
    pub structs: Vec<u32>,
    pub ifaces: Vec<u32>,
    pub enums: Vec<u32>,
}

/// One unit: its fragment and the global indices of its files (in the
/// order of [`Fragment::paths`]).
pub(crate) struct UnitView<'a> {
    pub frag: &'a Fragment,
    pub map: &'a FragMap,
    pub files: Vec<usize>,
}

/// Identity of a diagnostic for de-duplication.
fn diag_key(d: &Diagnostic) -> (usize, u32, u32, String, bool, Option<&'static str>, Vec<(String, Option<(u32, u32)>)>) {
    (d.file, d.span.start, d.span.end, d.message.clone(), d.severity == Severity::Error, d.code, d.notes.iter().map(|(n, s)| (n.clone(), s.map(|s| (s.start, s.end)))).collect())
}

pub(crate) fn assemble(files: &[FileView], units: &[UnitView], merged: &Program, std: &StdBlock, std_files: &[Arc<SourceFile>]) -> Analysis {
    let n = files.len();
    let own_count: usize = units.iter().map(|u| u.frag.symbols.len()).sum();
    let mut symbols: Vec<Symbol> = Vec::with_capacity(std.symbols.len() + own_count + files.len());
    symbols.extend_from_slice(&std.symbols);
    for s in &mut symbols {
        if let Some(l) = &mut s.loc {
            l.file += n;
        }
    }
    let mut by_key: FxHashMap<SymKey, SymbolId> = FxHashMap::with_capacity_and_hasher(symbols.capacity(), Default::default());
    by_key.extend(std.by_key.iter().map(|(k, v)| (k.clone(), *v)));
    let mut recvs: Vec<MemberRecv> = Vec::new();
    let mut refs: Vec<Reference> = Vec::with_capacity(units.iter().map(|u| u.frag.refs.len()).sum::<usize>() + files.len());
    // (A file belongs to one unit and its run de-duplicated its references.)
    let push_ref = |refs: &mut Vec<Reference>, file: usize, span: Span, symbol: SymbolId| {
        if span != Span::DUMMY {
            refs.push(Reference { file, span, symbol });
        }
    };
    // Namespace declarations: the first file of a namespace defines it,
    // the others refer to it (as `Sema::ide_namespace_decl` records them).
    let mut defined: HashSet<String> = HashSet::new();
    let mut ns_order: HashMap<String, usize> = HashMap::new();
    let mut ns_containers: Vec<(SymbolId, String)> = Vec::new();
    let mut ns_refs: Vec<(usize, Span, String)> = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        let segs = &f.ast.namespace.segments;
        // A user file declaring a std namespace records nothing for it
        // (the std namespaces keep their std symbols).
        let in_std = segs.first().is_some_and(|s| crate::sema::stdlib::is_std_namespace(&s.name));
        for i in 0..segs.len() {
            let path = segs[..=i].iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(".");
            let o = ns_order.len();
            ns_order.entry(path.clone()).or_insert(o);
            if in_std {
                defined.insert(path);
                continue;
            }
            if !defined.insert(path.clone()) {
                ns_refs.push((fi, segs[i].span, path));
                continue;
            }
            if segs[i].span == Span::DUMMY {
                continue;
            }
            let full_span = if i + 1 == segs.len() { f.ast.span } else { segs[i].span };
            let sym = Symbol {
                id: SymbolId(0),
                key: SymKey::Ns(path.clone()),
                kind: SymbolKind::Namespace,
                name: segs[i].name.clone().into(),
                full_name: path.clone().into(),
                loc: Some(Location { file: fi, name_span: segs[i].span, full_span }),
                signature: format!("namespace {path}").into(),
                ty: None,
                doc: None,
                container: None,
                is_static: false,
                is_async: false,
                is_readonly: false,
                is_std: false,
            };
            let id = match by_key.get(&sym.key) {
                // A std namespace declared by a user file.
                Some(&id) => {
                    symbols[id.0 as usize] = Symbol { id, ..sym };
                    id
                }
                None => {
                    let id = SymbolId(symbols.len() as u32);
                    by_key.insert(sym.key.clone(), id);
                    symbols.push(Symbol { id, ..sym });
                    id
                }
            };
            if i > 0 {
                ns_containers.push((id, segs[..i].iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(".")));
            }
        }
    }
    // Owned symbols of every unit.
    let mut skeys: FxHashMap<&SKey, SymbolId> = FxHashMap::default();
    let mut own: Vec<Vec<SymbolId>> = Vec::with_capacity(units.len());
    let (mut fbase, mut gbase) = (0u32, 0u32);
    for u in units {
        let frag = u.frag;
        let gfile = |p: usize| u.files.get(p).copied().unwrap_or(0);
        let mut ids = Vec::with_capacity(frag.symbols.len());
        for fs in &frag.symbols {
            let m = u.map;
            let key = match &fs.key {
                &FKey::Struct(s) => SymKey::Struct(m.structs[s as usize]),
                &FKey::Interface(i) => SymKey::Interface(m.ifaces[i as usize]),
                &FKey::Enum(e) => SymKey::Enum(m.enums[e as usize]),
                &FKey::Variant(e, v) => SymKey::Variant(m.enums[e as usize], v),
                &FKey::Field(s, f) => SymKey::Field(m.structs[s as usize], f),
                &FKey::IfaceMethod(i, k) => SymKey::IfaceMethod(m.ifaces[i as usize], k),
                &FKey::Func(f) => SymKey::Func(fbase + f),
                &FKey::Global(g) => SymKey::Global(gbase + g),
                &FKey::Local(f, l) => SymKey::Local(fbase + f, l),
                &FKey::LocalConst(p, start) => SymKey::LocalConst(gfile(p as usize), start),
                FKey::Alias(n) => SymKey::Alias(n.clone()),
            };
            let id = SymbolId(symbols.len() as u32);
            let slot = match by_key.entry(key) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    ids.push(*e.get());
                    continue;
                }
                std::collections::hash_map::Entry::Vacant(e) => e,
            };
            let f = &fs.sym;
            symbols.push(Symbol {
                id,
                key: slot.key().clone(),
                kind: f.kind,
                name: f.name.clone(),
                full_name: f.full_name.clone(),
                loc: f.loc.map(|l| Location { file: gfile(l.file), ..l }),
                signature: f.signature.clone(),
                ty: fs.ty.and_then(|t| m.types.get(t.0 as usize).copied()),
                doc: f.doc.clone(),
                container: f.container,
                is_static: f.is_static,
                is_async: f.is_async,
                is_readonly: f.is_readonly,
                is_std: f.is_std,
            });
            slot.insert(id);
            if let Some(k) = &fs.skey {
                skeys.entry(k).or_insert(id);
            }
            ids.push(id);
        }
        fbase += frag.n_funcs;
        gbase += frag.n_globals;
        own.push(ids);
    }
    // The symbols each fragment refers to in others.
    let mut ext: Vec<Vec<Option<SymbolId>>> = Vec::with_capacity(units.len());
    for u in units {
        let ids = u
            .frag
            .ext
            .iter()
            .map(|k| match k {
                SKey::Ns(p) => ensure(&mut symbols, &mut by_key, SymKey::Ns(p.clone())),
                SKey::Std(p) => ensure(&mut symbols, &mut by_key, SymKey::Std(p.clone())),
                SKey::Builtin(b) => ensure(&mut symbols, &mut by_key, SymKey::Builtin(b.clone())),
                k => skeys.get(k).copied(),
            })
            .collect();
        ext.push(ids);
    }
    let resolve = |u: usize, r: &SymRef| -> Option<SymbolId> {
        match *r {
            SymRef::Own(i) => own[u].get(i as usize).copied(),
            SymRef::Ext(e) => ext[u].get(e as usize).copied().flatten(),
        }
    };
    for (id, parent) in ns_containers {
        let c = ensure(&mut symbols, &mut by_key, SymKey::Ns(parent));
        if c != Some(id) {
            symbols[id.0 as usize].container = c;
        }
    }
    for (fi, span, path) in ns_refs {
        if let Some(id) = ensure(&mut symbols, &mut by_key, SymKey::Ns(path)) {
            push_ref(&mut refs, fi, span, id);
        }
    }
    let mut infos: Vec<Option<FileInfo>> = vec![None; n + std_files.len()];
    let mut ns_members: HashMap<String, Vec<(String, SymbolId)>> = std.ns_members.clone();
    for (ui, u) in units.iter().enumerate() {
        let frag = u.frag;
        for (i, fs) in frag.symbols.iter().enumerate() {
            let Some(c) = &fs.container else { continue };
            let c = resolve(ui, c);
            let id = own[ui][i];
            if c != Some(id) {
                symbols[id.0 as usize].container = c;
            }
        }
        for r in &frag.refs {
            if let Some(id) = resolve(ui, &r.sym) {
                push_ref(&mut refs, u.files[r.file as usize], r.span, id);
            }
        }
        for &(file, name_span, ty) in &frag.recvs {
            if let Some(&ty) = u.map.types.get(ty.0 as usize) {
                recvs.push(MemberRecv { file: u.files[file as usize], name_span, ty });
            }
        }
        for (i, info) in frag.file_infos.iter().enumerate() {
            let Some(info) = info else { continue };
            let imports = info
                .imports
                .iter()
                .filter_map(|imp| match imp {
                    FImport::Glob(p) => Some(ImportInfo::Glob(p.clone())),
                    FImport::Named(n, r) => resolve(ui, r).map(|id| ImportInfo::Named(n.clone(), id)),
                })
                .collect();
            infos[u.files[i]] = Some(FileInfo { namespace: info.namespace.clone(), namespace_span: info.namespace_span, span: info.span, imports });
        }
        for (path, list) in &frag.ns_members {
            let list = list.iter().filter_map(|(name, r)| resolve(ui, r).map(|id| (name.clone(), id))).collect();
            ns_members.insert(path.clone(), list);
        }
    }
    // Namespaces without files list their child namespaces.
    let mut paths: Vec<&String> = ns_order.keys().collect();
    paths.sort();
    for p in &paths {
        if ns_members.contains_key(p.as_str()) {
            continue;
        }
        let prefix = format!("{p}.");
        let mut list: Vec<(String, SymbolId)> = Vec::new();
        for c in paths.iter().filter(|c| c.starts_with(&prefix) && !c[prefix.len()..].contains('.')) {
            if let Some(id) = ensure(&mut symbols, &mut by_key, SymKey::Ns(c.to_string())) {
                list.push((c[prefix.len()..].to_string(), id));
            }
        }
        list.sort_by(|a, b| a.0.cmp(&b.0));
        ns_members.insert(p.to_string(), list);
    }
    // Diagnostics.
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        diagnostics.extend(f.parse_diags.iter().cloned().map(|d| d.in_file(fi)));
    }
    let mut sema: Vec<Diagnostic> = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        sema.extend(f.cfg_diags.iter().cloned().map(|d| d.in_file(fi)));
    }
    let mut foreign: Vec<Diagnostic> = Vec::new();
    for (ui, u) in units.iter().enumerate() {
        for d in &u.frag.diags {
            let Some(&file) = u.files.get(d.file) else { continue };
            let d = Diagnostic { file, ..d.clone() };
            if files.get(file).is_some_and(|f| f.unit == ui) {
                sema.push(d);
            } else {
                foreign.push(d);
            }
        }
    }
    {
        let mut keys: HashSet<_> = sema.iter().map(diag_key).collect();
        for d in foreign {
            if keys.insert(diag_key(&d)) {
                sema.push(d);
            }
        }
    }
    // Lints are made only for a program without errors (as by a compiler
    // run); a unit run saw no errors in its part, but others may have some.
    let lint = crate::sema::lint::is_lint;
    if units.iter().any(|u| u.frag.diags.iter().any(|d| d.severity == Severity::Error && !lint(d.code))) {
        sema.retain(|d| !lint(d.code));
    }
    // Unused imports count only in a program without other errors.
    if !sema.iter().any(|d| d.severity == Severity::Error) {
        let mut unused: Vec<Diagnostic> = Vec::new();
        for u in units {
            for d in &u.frag.unused_imports {
                let Some(&file) = u.files.get(d.file) else { continue };
                unused.push(Diagnostic { file, ..d.clone() });
            }
        }
        complete_fixes(&mut unused, &|f| files.get(f).map(|x| x.source.text.as_str()), &|f| files.get(f).map(|x| &**x.ast));
        sema.extend(unused);
    }
    // The entry point: `main` of the namespace with the shortest path.
    let mut best: Option<(usize, usize, usize, Span, bool)> = None;
    for u in units {
        for m in &u.frag.mains {
            let depth = m.ns.split('.').count();
            let order = ns_order.get(&m.ns).copied().unwrap_or(usize::MAX);
            let Some(&file) = u.files.get(m.file) else { continue };
            if best.is_none_or(|b| (depth, order) < (b.0, b.1)) {
                best = Some((depth, order, file, m.span, m.ok));
            }
        }
    }
    if let Some((_, _, file, span, false)) = best {
        sema.push(Diagnostic::error("main must take no parameters and return nothing (or void!)", span).in_file(file));
    }
    diagnostics.extend(sema);
    let ok = !diagnostics.iter().any(|d| d.severity == Severity::Error);
    let mut index = SymbolIndex::from_parts(symbols, refs, infos, ns_members, by_key);
    index.set_member_recvs(recvs);
    let mut all_files: Vec<Arc<SourceFile>> = files.iter().map(|f| f.source.clone()).collect();
    all_files.extend(std_files.iter().cloned());
    let mut read_only: Vec<bool> = files.iter().map(|f| f.read_only).collect();
    read_only.extend(std::iter::repeat_n(true, std_files.len()));
    let mut a = Analysis::new(all_files, files.iter().map(|f| f.ast.clone()).collect(), diagnostics, index, merged.clone(), ok, read_only);
    a.types = Arc::new(type_graph(units, std, n));
    a
}

/// The type hierarchy from the shapes of every unit and of std (whose
/// files follow the `n` session files).
fn type_graph(units: &[UnitView], std: &StdBlock, n: usize) -> super::typegraph::TypeGraph {
    let _t = crate::build::phase("ide-types");
    let mut shapes = Vec::with_capacity(std.shapes.len() + units.iter().map(|u| u.frag.shapes.len()).sum::<usize>());
    for u in units {
        for sh in &u.frag.shapes {
            let Some(file) = sh.loc.and_then(|l| u.files.get(l.file).copied()).filter(|&f| f < n) else { continue };
            let mut sh = sh.clone();
            let at = |l: &mut Location| l.file = file;
            sh.loc.as_mut().map(at);
            sh.methods.iter_mut().filter_map(|m| m.loc.as_mut()).for_each(at);
            shapes.push(sh);
        }
    }
    for sh in &std.shapes {
        let mut sh = sh.clone();
        let at = |l: &mut Location| l.file += n;
        sh.loc.as_mut().map(at);
        sh.methods.iter_mut().filter_map(|m| m.loc.as_mut()).for_each(at);
        shapes.push(sh);
    }
    super::typegraph::TypeGraph::build(shapes)
}

/// The symbol for `key`, created as an implicit symbol (namespace, std
/// member, builtin) when missing.
fn ensure(symbols: &mut Vec<Symbol>, by_key: &mut FxHashMap<SymKey, SymbolId>, key: SymKey) -> Option<SymbolId> {
    if let Some(&id) = by_key.get(&key) {
        return Some(id);
    }
    let mut s = index::implicit_symbol(&key, None)?;
    let id = SymbolId(symbols.len() as u32);
    s.id = id;
    by_key.insert(key, id);
    symbols.push(s);
    Some(id)
}
