//! Rename checks that need block scopes ([`Analysis::scopes`]): would the
//! new name be captured by, or capture, another declaration?
//!
//! * A local renamed to the name of another local of the same block (a
//!   redeclaration), or used where a local of that name declared in an
//!   inner block is visible (the use would mean that local), or visible
//!   where another declaration of that name is used (it would hide it).
//! * A global (function, type, variable, constant, alias) renamed to the
//!   name of a local visible at one of its unqualified uses, in any
//!   function of any file.

use zell::sema::index::{SymKey, Symbol, SymbolId, SymbolKind as K};
use zell::session::Analysis;
use zell::span::Span;

fn is_local(s: &Symbol) -> bool {
    matches!(s.kind, K::Local | K::Param) || matches!(s.key, SymKey::LocalConst(..))
}

/// Members are reached through a value or a type (`x.N`, `T.N`): plain
/// names never mean them.
fn is_member(s: &Symbol) -> bool {
    matches!(s.kind, K::Field | K::Method | K::Variant)
}

/// The name at `span` of `file` is qualified (`a.name`).
fn qualified(a: &Analysis, file: usize, span: Span) -> bool {
    let text = &a.files[file].text;
    text.get(..span.start as usize).is_some_and(|t| t.trim_end_matches([' ', '\t']).ends_with('.'))
}

/// The locals of `file` named `name` with their scope and region.
fn locals_named(a: &Analysis, file: usize, name: &str) -> Vec<(SymbolId, u32, Span)> {
    let Some(sc) = a.scopes(file) else { return Vec::new() };
    a.index
        .symbols
        .iter()
        .filter(|s| is_local(s) && *s.name == *name)
        .filter_map(|s| {
            let l = s.loc.filter(|l| l.file == file)?;
            let (scope, region) = sc.region(l.name_span)?;
            Some((s.id, scope, region))
        })
        .collect()
}

fn contains(region: Span, at: u32) -> bool {
    region.start <= at && at < region.end
}

/// A declaration named `new` that renaming `sym` (declared and used at
/// `spans`) would capture or be captured by.
pub(crate) fn conflict(a: &Analysis, sym: SymbolId, spans: &[(usize, Span)], new: &str) -> Option<SymbolId> {
    let s = a.index.symbol(sym);
    if !is_local(s) {
        // A global: a local `new` visible at an unqualified use hides it.
        let mut by_file: std::collections::HashMap<usize, Vec<(SymbolId, u32, Span)>> = Default::default();
        for &(f, sp) in spans {
            if qualified(a, f, sp) {
                continue;
            }
            let locals = by_file.entry(f).or_insert_with(|| locals_named(a, f, new));
            if let Some(o) = locals.iter().find(|(_, _, r)| contains(*r, sp.start)) {
                return Some(o.0);
            }
        }
        return None;
    }
    let l = s.loc?;
    let sc = a.scopes(l.file)?;
    let (own, region) = sc.region(l.name_span)?;
    let others: Vec<(SymbolId, u32, Span)> = locals_named(a, l.file, new).into_iter().filter(|o| o.0 != sym).collect();
    // Another local of the same block.
    if let Some(o) = others.iter().find(|o| o.1 == own) {
        return Some(o.0);
    }
    // A use inside an inner local `new`'s region would mean that local.
    for &(_, sp) in spans.iter().filter(|(f, _)| *f == l.file) {
        if let Some(o) = others.iter().find(|o| o.1 != own && sc.within(o.1, own) && contains(o.2, sp.start)) {
            return Some(o.0);
        }
    }
    // A use of another declaration `new` in this local's region would mean
    // this local, unless an inner local `new` is the one visible there.
    for r in &a.index.references {
        if r.file != l.file || !contains(region, r.span.start) || r.symbol == sym {
            continue;
        }
        let x = a.index.symbol(r.symbol);
        if *x.name != *new || is_member(x) || qualified(a, r.file, r.span) || a.files[r.file].text.get(r.span.start as usize..r.span.end as usize) != Some(new) {
            continue;
        }
        let inner = others.iter().any(|o| o.1 != own && sc.within(o.1, own) && contains(o.2, r.span.start));
        if !inner {
            return Some(r.symbol);
        }
    }
    None
}
