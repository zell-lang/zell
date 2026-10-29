//! The standard library part of every [`super::Analysis`] index: the std
//! namespaces and members, built once per process (and per std source
//! index) instead of once per analysis.

use crate::sema::index::{Location, SymKey, Symbol, SymbolId, SymbolKind};
use crate::stdindex::StdIndex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Std symbols with their member lists; `loc.file` of a symbol indexes
/// the std index files (shifted past the session files on assembly).
pub(crate) struct StdBlock {
    pub symbols: Vec<Symbol>,
    pub by_key: HashMap<SymKey, SymbolId>,
    pub ns_members: HashMap<String, Vec<(String, SymbolId)>>,
}

/// The std symbols as the analysis sees them (no source locations).
fn plain() -> &'static StdBlock {
    static BLOCK: OnceLock<StdBlock> = OnceLock::new();
    BLOCK.get_or_init(|| {
        let idx = crate::sema::units::std_symbols();
        let is_std = |s: &Symbol| match &s.key {
            SymKey::Std(_) => true,
            SymKey::Ns(p) => crate::sema::stdlib::is_std_namespace(p),
            _ => false,
        };
        let mut map: HashMap<SymbolId, SymbolId> = HashMap::new();
        let mut symbols = Vec::new();
        for s in idx.symbols.iter().filter(|s| is_std(s)) {
            map.insert(s.id, SymbolId(symbols.len() as u32));
            let mut s = s.clone();
            s.id = SymbolId(symbols.len() as u32);
            s.loc = None;
            symbols.push(s);
        }
        for s in &mut symbols {
            s.container = s.container.and_then(|c| map.get(&c).copied());
        }
        let by_key = symbols.iter().map(|s| (s.key.clone(), s.id)).collect();
        let ns_members = idx
            .ns_members
            .iter()
            .filter(|(p, _)| crate::sema::stdlib::is_std_namespace(p))
            .map(|(p, ms)| (p.clone(), ms.iter().filter_map(|(n, id)| map.get(id).map(|i| (n.clone(), *i))).collect()))
            .collect();
        StdBlock { symbols, by_key, ns_members }
    })
}

/// The std block with locations and doc comments from `std` (cached for
/// the last index seen).
pub(crate) fn block(std: Option<&Arc<StdIndex>>) -> Arc<StdBlock> {
    static LAST: std::sync::Mutex<Option<(usize, Arc<StdBlock>)>> = std::sync::Mutex::new(None);
    let key = std.map_or(0, |s| Arc::as_ptr(s) as usize);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((k, b)) = &*last {
        if *k == key {
            return b.clone();
        }
    }
    let base = plain();
    let mut symbols = base.symbols.clone();
    if let Some(std) = std {
        for sym in &mut symbols {
            let Some(d) = std.lookup(&sym.full_name) else { continue };
            sym.loc = Some(Location { file: d.file, name_span: d.name_span, full_span: d.full_span });
            sym.is_std = true;
            if sym.doc.is_none() {
                sym.doc = d.doc.clone();
            }
            // Placeholder signatures of stdlib templates (`func Get`).
            if !sym.signature.contains('(') && !sym.signature.contains('{') && matches!(sym.kind, SymbolKind::Func | SymbolKind::Method | SymbolKind::Struct | SymbolKind::Enum) {
                sym.signature = d.signature.clone();
            }
        }
    }
    let mut by_key = base.by_key.clone();
    if let Some(std) = std {
        add_members(std, &mut symbols, &mut by_key);
    }
    let b = Arc::new(StdBlock { symbols, by_key, ns_members: base.ns_members.clone() });
    *last = Some((key, b.clone()));
    b
}

/// The fields and methods of the std structs and enums (`testing.T.Fatal`,
/// keyed `SymKey::Std` by path, the key sema records for their uses), and
/// the `@intrinsic` structs that document a builtin type's methods
/// (`task.Task.Cancel` for `Task<T>`), from the `std/` sources.
fn add_members(std: &StdIndex, symbols: &mut Vec<Symbol>, by_key: &mut HashMap<SymKey, SymbolId>) {
    let add = |symbols: &mut Vec<Symbol>, by_key: &mut HashMap<SymKey, SymbolId>, d: &crate::stdindex::StdDecl, kind: SymbolKind, container: Option<SymbolId>| -> SymbolId {
        let id = SymbolId(symbols.len() as u32);
        let key = SymKey::Std(d.full_name.clone());
        symbols.push(Symbol {
            id,
            key: key.clone(),
            kind,
            name: d.name.clone(),
            full_name: d.full_name.clone(),
            loc: Some(Location { file: d.file, name_span: d.name_span, full_span: d.full_span }),
            signature: d.signature.clone(),
            ty: None,
            doc: d.doc.clone(),
            container,
            is_static: d.is_static,
            is_async: d.signature.starts_with("async "),
            is_readonly: false,
            is_std: true,
        });
        by_key.insert(key, id);
        id
    };
    for d in &std.decls {
        if !matches!(d.kind, SymbolKind::Method | SymbolKind::Field) || by_key.contains_key(&SymKey::Std(d.full_name.clone())) {
            continue;
        }
        let Some((parent, _)) = d.full_name.rsplit_once('.') else { continue };
        let pid = match by_key.get(&SymKey::Std(parent.to_string())) {
            Some(&id) => id,
            None => {
                let Some(pd) = std.lookup(parent).filter(|p| p.intrinsic && p.kind == SymbolKind::Struct) else { continue };
                let ns = parent.rsplit_once('.').and_then(|(ns, _)| by_key.get(&SymKey::Ns(ns.to_string())).copied());
                add(symbols, by_key, pd, SymbolKind::BuiltinType, ns)
            }
        };
        add(symbols, by_key, d, d.kind, Some(pid));
    }
}
