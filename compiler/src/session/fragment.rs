//! The cached result of one analysis unit (the files of one namespace):
//! its symbols, references, diagnostics and types, independent of the ids
//! of the run that produced it. Symbols of other units are referred to by
//! stable keys ([`SKey`]) and resolved when the fragments are assembled
//! into an [`super::Analysis`] (see `assemble`).

use super::remap::{Copier, DeclStore, Keys};
use crate::diagnostics::Diagnostic;
use crate::sema::index::{ImportInfo, SymKey, Symbol, SymbolId, SymbolKind};
use crate::sema::units::{MainInfo, RunOutput};
use crate::span::Span;
use crate::types::TypeId;
use std::collections::HashMap;

/// Identity of a symbol across analyses.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SKey {
    Ns(String),
    Std(String),
    Builtin(String),
    /// A declaration by kind (see [`decl_kind`]) and full name.
    Decl(u8, String),
    /// Parameter `n` of the function with this full name.
    Param(String, u32),
}

/// A symbol of the same fragment, or of anywhere else (an index into
/// [`Fragment::ext`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SymRef {
    Own(u32),
    Ext(u32),
}

/// The key of an owned symbol, with ids local to the fragment (types and
/// declarations index [`Fragment::mini`], functions and globals are
/// numbered per fragment).
#[derive(Clone, Debug)]
pub(crate) enum FKey {
    Struct(u32),
    Interface(u32),
    Enum(u32),
    Variant(u32, u32),
    Field(u32, u32),
    IfaceMethod(u32, u32),
    Func(u32),
    Global(u32),
    Local(u32, u32),
    /// (index into [`Fragment::paths`], start of the name)
    LocalConst(u32, u32),
}

/// A symbol declared in the fragment's files.
#[derive(Clone, Debug)]
pub(crate) struct FSym {
    /// `id`, `key`, `container` and `ty` are set on assembly; `loc.file`
    /// indexes [`Fragment::paths`].
    pub sym: Symbol,
    pub key: FKey,
    pub container: Option<SymRef>,
    /// Type in [`Fragment::mini`].
    pub ty: Option<TypeId>,
    /// How other fragments refer to it.
    pub skey: Option<SKey>,
}

#[derive(Clone, Debug)]
pub(crate) struct FRef {
    /// Index into [`Fragment::paths`] (always a file of the unit).
    pub file: u32,
    pub span: Span,
    pub sym: SymRef,
}

#[derive(Clone, Debug)]
pub(crate) enum FImport {
    Glob(String),
    Named(String, SymRef),
}

#[derive(Clone, Debug)]
pub(crate) struct FFileInfo {
    pub namespace: String,
    pub namespace_span: Span,
    pub span: Span,
    pub imports: Vec<FImport>,
}

/// The analysis result of one unit.
pub(crate) struct Fragment {
    /// The unit's files (first [`Fragment::n_own`]), then other files its
    /// diagnostics are in (generic code of other units).
    pub paths: Vec<String>,
    pub n_own: usize,
    pub symbols: Vec<FSym>,
    /// Keys of the symbols of other fragments it refers to.
    pub ext: Vec<SKey>,
    pub refs: Vec<FRef>,
    /// Receivers of member accesses: (index into `paths`, name span, type
    /// in [`Fragment::mini`]); see `SymbolIndex::member_recvs`.
    pub recvs: Vec<(u32, Span, TypeId)>,
    /// Per own file.
    pub file_infos: Vec<Option<FFileInfo>>,
    /// Member lists of the namespaces of the unit's files.
    pub ns_members: Vec<(String, Vec<(String, SymRef)>)>,
    /// `file` indexes `paths`; fixes are complete.
    pub diags: Vec<Diagnostic>,
    pub unused_imports: Vec<Diagnostic>,
    pub mains: Vec<MainInfo>,
    /// Types of the symbols and the declarations they name.
    pub mini: DeclStore,
    pub mini_params: Vec<String>,
    pub n_funcs: u32,
    pub n_globals: u32,
    /// Total lines of the unit's files (cost estimates).
    pub lines: usize,
}

/// Kind number of a declaration key in [`SKey::Decl`].
fn decl_kind(k: &SymKey) -> Option<u8> {
    Some(match k {
        SymKey::Struct(_) => 0,
        SymKey::Interface(_) => 1,
        SymKey::Enum(_) => 2,
        SymKey::Variant(..) => 3,
        SymKey::Field(..) => 4,
        SymKey::IfaceMethod(..) => 5,
        SymKey::Func(_) => 6,
        SymKey::Global(_) => 7,
        _ => return None,
    })
}

/// The stable key of a symbol of a run.
pub(crate) fn skey_of(sym: &Symbol, program: &crate::hir::Program) -> Option<SKey> {
    Some(match &sym.key {
        SymKey::Ns(p) => SKey::Ns(p.clone()),
        SymKey::Std(p) => SKey::Std(p.clone()),
        SymKey::Builtin(n) => SKey::Builtin(n.clone()),
        SymKey::Local(f, l) if sym.kind == SymbolKind::Param => SKey::Param(program.funcs.get(*f as usize)?.full_name.clone(), *l),
        k => SKey::Decl(decl_kind(k)?, sym.full_name.clone()),
    })
}

/// One file of a run.
pub(crate) struct RunFile<'a> {
    pub path: &'a str,
    /// The unit it belongs to.
    pub unit: usize,
    pub text: &'a str,
}

/// Splits the output of a run into fragments for the units `wanted`
/// (indices of [`RunFile::unit`]); `params` are the run's type parameter
/// names. The run's diagnostics must have complete fixes already.
pub(crate) fn split(run: &RunOutput, files: &[RunFile], wanted: &dyn Fn(usize) -> bool) -> HashMap<usize, Fragment> {
    let keys = Keys::of(&run.program);
    let params: &[String] = &run.tparams;
    let mut out: HashMap<usize, Fragment> = HashMap::new();
    let mut copiers: HashMap<usize, Copier> = HashMap::new();
    // Local numbering of functions and globals per unit.
    let mut funcs: HashMap<usize, HashMap<u32, u32>> = HashMap::new();
    let mut globals: HashMap<usize, HashMap<u32, u32>> = HashMap::new();
    // Own files first, in run order.
    let mut path_idx: HashMap<(usize, usize), u32> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if !wanted(f.unit) {
            continue;
        }
        let frag = out.entry(f.unit).or_insert_with(|| Fragment {
            paths: Vec::new(),
            n_own: 0,
            symbols: Vec::new(),
            ext: Vec::new(),
            refs: Vec::new(),
            recvs: Vec::new(),
            file_infos: Vec::new(),
            ns_members: Vec::new(),
            diags: Vec::new(),
            unused_imports: Vec::new(),
            mains: Vec::new(),
            mini: DeclStore::default(),
            mini_params: Vec::new(),
            n_funcs: 0,
            n_globals: 0,
            lines: 0,
        });
        path_idx.insert((f.unit, i), frag.paths.len() as u32);
        frag.paths.push(f.path.to_string());
        frag.n_own += 1;
        frag.lines += f.text.lines().count();
    }
    let unit_of = |file: usize| files.get(file).map(|f| f.unit).filter(|u| wanted(*u));
    let idx = &run.index;
    // Owner of each symbol: (unit, index in its fragment).
    let mut owner: Vec<Option<(usize, u32)>> = vec![None; idx.symbols.len()];
    for s in &idx.symbols {
        if matches!(s.key, SymKey::Ns(_)) {
            continue;
        }
        let Some(loc) = s.loc else { continue };
        let Some(u) = unit_of(loc.file) else { continue };
        let frag = out.get_mut(&u).unwrap();
        let copier = copiers.entry(u).or_insert_with(|| Copier::new(&run.program, &keys, params, false));
        let mini = &mut frag.mini;
        let num = |m: &mut HashMap<usize, HashMap<u32, u32>>, n: &mut u32, id: u32| -> u32 {
            *m.entry(u).or_default().entry(id).or_insert_with(|| {
                *n += 1;
                *n - 1
            })
        };
        let key = match &s.key {
            SymKey::Struct(i) => FKey::Struct(copier.strukt(mini, *i)),
            SymKey::Interface(i) => FKey::Interface(copier.iface(mini, *i)),
            SymKey::Enum(i) => FKey::Enum(copier.enm(mini, *i)),
            SymKey::Variant(e, v) => FKey::Variant(copier.enm(mini, *e), *v),
            SymKey::Field(st, f) => FKey::Field(copier.strukt(mini, *st), *f),
            SymKey::IfaceMethod(i, m) => FKey::IfaceMethod(copier.iface(mini, *i), *m),
            SymKey::Func(f) => FKey::Func(num(&mut funcs, &mut frag.n_funcs, *f)),
            SymKey::Global(g) => FKey::Global(num(&mut globals, &mut frag.n_globals, *g)),
            SymKey::Local(f, l) => FKey::Local(num(&mut funcs, &mut frag.n_funcs, *f), *l),
            SymKey::LocalConst(file, start) => match path_idx.get(&(u, *file)) {
                Some(&p) => FKey::LocalConst(p, *start),
                None => continue,
            },
            SymKey::Ns(_) | SymKey::Std(_) | SymKey::Builtin(_) => continue,
        };
        let ty = s.ty.map(|t| copier.ty(mini, t));
        let mut sym = s.clone();
        sym.id = SymbolId(0);
        sym.container = None;
        sym.ty = None;
        if let Some(l) = &mut sym.loc {
            l.file = path_idx[&(u, loc.file)] as usize;
        }
        owner[s.id.0 as usize] = Some((u, frag.symbols.len() as u32));
        frag.symbols.push(FSym { sym, key, container: None, ty, skey: skey_of(s, &run.program) });
    }
    let mut ext_ids: HashMap<(usize, SymbolId), u32> = HashMap::new();
    let mut ext: HashMap<usize, Vec<SKey>> = HashMap::new();
    let mut sref = |u: usize, id: SymbolId| -> Option<SymRef> {
        if let Some((ou, i)) = owner[id.0 as usize] {
            if ou == u {
                return Some(SymRef::Own(i));
            }
        }
        if let Some(&e) = ext_ids.get(&(u, id)) {
            return Some(SymRef::Ext(e));
        }
        let k = skey_of(&idx.symbols[id.0 as usize], &run.program)?;
        let table = ext.entry(u).or_default();
        table.push(k);
        let e = table.len() as u32 - 1;
        ext_ids.insert((u, id), e);
        Some(SymRef::Ext(e))
    };
    for s in &idx.symbols {
        let (Some((u, i)), Some(c)) = (owner[s.id.0 as usize], s.container) else { continue };
        let c = sref(u, c);
        out.get_mut(&u).unwrap().symbols[i as usize].container = c;
    }
    // Namespace lines are recorded by the assembly (first file wins).
    let in_ns_line = |file: usize, span: Span| idx.files.get(file).and_then(|f| f.as_ref()).is_some_and(|f| f.namespace_span.start <= span.start && span.end <= f.namespace_span.end);
    for r in &idx.references {
        let Some(u) = unit_of(r.file) else { continue };
        if matches!(idx.symbols[r.symbol.0 as usize].key, SymKey::Ns(_)) && in_ns_line(r.file, r.span) {
            continue;
        }
        let Some(sym) = sref(u, r.symbol) else { continue };
        let file = path_idx[&(u, r.file)];
        out.get_mut(&u).unwrap().refs.push(FRef { file, span: r.span, sym });
    }
    for r in &idx.member_recvs {
        let Some(u) = unit_of(r.file) else { continue };
        let file = path_idx[&(u, r.file)];
        let frag = out.get_mut(&u).unwrap();
        let copier = copiers.entry(u).or_insert_with(|| Copier::new(&run.program, &keys, params, false));
        let ty = copier.ty(&mut frag.mini, r.ty);
        frag.recvs.push((file, r.name_span, ty));
    }
    for fi in 0..files.len() {
        let Some(u) = unit_of(fi) else { continue };
        let info = idx.files.get(fi).and_then(|i| i.as_ref()).map(|i| FFileInfo {
            namespace: i.namespace.clone(),
            namespace_span: i.namespace_span,
            span: i.span,
            imports: i
                .imports
                .iter()
                .filter_map(|imp| match imp {
                    ImportInfo::Glob(p) => Some(FImport::Glob(p.clone())),
                    ImportInfo::Named(n, id) => sref(u, *id).map(|r| FImport::Named(n.clone(), r)),
                })
                .collect(),
        });
        out.get_mut(&u).unwrap().file_infos.push(info);
    }
    // Namespace member lists, by the unit declaring the namespace.
    let mut ns_unit: HashMap<&str, usize> = HashMap::new();
    for (fi, f) in files.iter().enumerate() {
        if unit_of(fi).is_some() {
            if let Some(Some(info)) = idx.files.get(fi) {
                ns_unit.entry(info.namespace.as_str()).or_insert(f.unit);
            }
        }
    }
    for (path, members) in &idx.ns_members {
        let Some(&u) = ns_unit.get(path.as_str()) else { continue };
        let list = members.iter().filter_map(|(n, id)| sref(u, *id).map(|r| (n.clone(), r))).collect();
        out.get_mut(&u).unwrap().ns_members.push((path.clone(), list));
    }
    // Diagnostics, by the files whose analysis reports them.
    let place = |frag: &mut Fragment, u: usize, file: usize| -> Option<u32> {
        if let Some(&p) = path_idx.get(&(u, file)) {
            return Some(p);
        }
        let path = files.get(file)?.path;
        Some(match frag.paths.iter().position(|p| p == path) {
            Some(p) => p as u32,
            None => {
                frag.paths.push(path.to_string());
                frag.paths.len() as u32 - 1
            }
        })
    };
    for (d, origins) in run.diags.iter().zip(&run.diag_origins) {
        let mut units: Vec<usize> = origins.iter().filter_map(|&o| unit_of(o)).collect();
        units.sort();
        units.dedup();
        for u in units {
            let frag = out.get_mut(&u).unwrap();
            let Some(p) = place(frag, u, d.file) else { continue };
            frag.diags.push(Diagnostic { file: p as usize, ..d.clone() });
        }
    }
    for d in &run.unused_imports {
        let Some(u) = unit_of(d.file) else { continue };
        let p = path_idx[&(u, d.file)];
        out.get_mut(&u).unwrap().unused_imports.push(Diagnostic { file: p as usize, ..d.clone() });
    }
    for m in &run.mains {
        let Some(u) = unit_of(m.file) else { continue };
        let p = path_idx[&(u, m.file)];
        out.get_mut(&u).unwrap().mains.push(MainInfo { file: p as usize, ..m.clone() });
    }
    for (u, frag) in out.iter_mut() {
        frag.mini_params = frag.mini.param_names();
        frag.ext = ext.remove(u).unwrap_or_default();
    }
    out
}
