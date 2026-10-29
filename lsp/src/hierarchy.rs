//! Call hierarchy (`textDocument/prepareCallHierarchy`,
//! `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`) from the
//! reference index: a call is a reference to a function or method that is
//! followed by `(`, and its caller is the innermost function or method
//! whose declaration contains it.

use crate::{features, range, symbol_data, symbol_from_data};
use lsp_types::{CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Range, Url};
use std::collections::BTreeMap;
use zell::sema::index::{Reference, SymbolId, SymbolKind};
use zell::session::Analysis;

fn callable(a: &Analysis, id: SymbolId) -> bool {
    matches!(a.index.symbol(id).kind, SymbolKind::Func | SymbolKind::Method)
}

/// Whether reference `r` is a call (its name is followed by `(`).
fn is_call(a: &Analysis, r: &Reference) -> bool {
    let text = &a.files[r.file].text;
    text.get(r.span.end as usize..).is_some_and(|rest| rest.trim_start_matches([' ', '\t']).starts_with('('))
}

/// The item of a declared function or method.
pub(crate) fn item(a: &Analysis, version: u64, id: SymbolId, uri_of: &impl Fn(usize) -> Option<Url>) -> Option<CallHierarchyItem> {
    let s = a.index.symbol(id);
    let loc = s.loc?;
    if !callable(a, id) {
        return None;
    }
    let f = &a.files[loc.file];
    let mut data = symbol_data(a, id);
    data["v"] = version.into();
    Some(CallHierarchyItem {
        name: s.name.clone(),
        kind: features::lsp_kind(s.kind),
        tags: None,
        detail: Some(s.signature.clone()).filter(|d| !d.is_empty()),
        uri: uri_of(loc.file)?,
        range: range(f, loc.full_span),
        selection_range: range(f, loc.name_span),
        data: Some(data),
    })
}

/// `prepareCallHierarchy`: the function or method named at `off`.
pub(crate) fn prepare(a: &Analysis, version: u64, file: usize, off: u32, uri_of: &impl Fn(usize) -> Option<Url>) -> Option<Vec<CallHierarchyItem>> {
    let id = a.symbol_at(file, off)?;
    Some(vec![item(a, version, id, uri_of)?])
}

/// The function an item names (by its data: id, else full name).
fn target(a: &Analysis, version: u64, it: &CallHierarchyItem) -> Option<SymbolId> {
    it.data.as_ref().and_then(|d| symbol_from_data(a, version, d)).filter(|&id| callable(a, id))
}

/// Callers of the item, each with the ranges of its calls.
pub(crate) fn incoming(a: &Analysis, version: u64, it: &CallHierarchyItem, uri_of: &impl Fn(usize) -> Option<Url>) -> Option<Vec<CallHierarchyIncomingCall>> {
    let id = target(a, version, it)?;
    let mut by_caller: BTreeMap<SymbolId, Vec<Range>> = BTreeMap::new();
    for r in a.references_of(id).iter().filter(|r| is_call(a, r)) {
        if let Some(caller) = crate::completion::enclosing_fn(a, r.file, r.span.start) {
            by_caller.entry(caller).or_default().push(range(&a.files[r.file], r.span));
        }
    }
    Some(by_caller.into_iter().filter_map(|(caller, from_ranges)| Some(CallHierarchyIncomingCall { from: item(a, version, caller, uri_of)?, from_ranges })).collect())
}

/// Functions the item calls, each with the ranges of the calls.
pub(crate) fn outgoing(a: &Analysis, version: u64, it: &CallHierarchyItem, uri_of: &impl Fn(usize) -> Option<Url>) -> Option<Vec<CallHierarchyOutgoingCall>> {
    let id = target(a, version, it)?;
    let loc = a.index.symbol(id).loc?;
    let mut by_callee: BTreeMap<SymbolId, Vec<Range>> = BTreeMap::new();
    for r in &a.index.references {
        if r.file == loc.file && loc.full_span.start <= r.span.start && r.span.end <= loc.full_span.end && callable(a, r.symbol) && is_call(a, r) && crate::completion::enclosing_fn(a, r.file, r.span.start) == Some(id) {
            by_callee.entry(r.symbol).or_default().push(range(&a.files[r.file], r.span));
        }
    }
    Some(by_callee.into_iter().filter_map(|(callee, from_ranges)| Some(CallHierarchyOutgoingCall { to: item(a, version, callee, uri_of)?, from_ranges })).collect())
}
