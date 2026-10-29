//! Type hierarchy (`textDocument/prepareTypeHierarchy`,
//! `typeHierarchy/supertypes`, `typeHierarchy/subtypes`) from the
//! satisfaction table of the analysis ([`Analysis::types`]): supertypes
//! of a struct or enum are the interfaces it satisfies and the structs it
//! embeds; subtypes of an interface are the types satisfying it and the
//! interfaces with all of its methods; subtypes of a struct are the
//! structs embedding it. Items carry the full declaration name as `data`.

use crate::range;
use lsp_types::{SymbolKind as L, TypeHierarchyItem, Url};
use zell::sema::index::{strip_type_args, SymKey};
use zell::sema::shapes::ShapeKind;
use zell::session::Analysis;

/// The item of type `t` of the analysis' type graph.
fn item(a: &Analysis, t: u32, uri_of: &impl Fn(usize) -> Option<Url>) -> Option<TypeHierarchyItem> {
    let sh = a.types.types.get(t as usize)?;
    let loc = sh.loc?;
    let f = a.files.get(loc.file)?;
    let short = sh.name.rsplit('.').next().unwrap_or(&sh.name).to_string();
    Some(TypeHierarchyItem {
        name: short,
        kind: match sh.kind {
            ShapeKind::Struct => L::STRUCT,
            ShapeKind::Enum => L::ENUM,
            ShapeKind::Interface => L::INTERFACE,
        },
        tags: None,
        detail: Some(sh.name.clone()),
        uri: uri_of(loc.file)?,
        range: range(f, loc.full_span),
        selection_range: range(f, loc.name_span),
        data: Some(serde_json::json!({ "type": sh.name })),
    })
}

/// The type at `off`: a declaration name (generic ones included) or a
/// use of a struct, enum or interface.
fn type_at(a: &Analysis, file: usize, off: u32) -> Option<u32> {
    let g = &a.types;
    if let Some(t) = g.types.iter().position(|s| s.loc.is_some_and(|l| l.file == file && l.name_span.start <= off && off <= l.name_span.end)) {
        return Some(t as u32);
    }
    let s = a.index.symbol(a.symbol_at(file, off)?);
    match s.key {
        SymKey::Struct(_) | SymKey::Enum(_) | SymKey::Interface(_) | SymKey::Std(_) => g.find(&strip_type_args(&s.full_name)),
        _ => None,
    }
}

/// `prepareTypeHierarchy`.
pub(crate) fn prepare(a: &Analysis, file: usize, off: u32, uri_of: &impl Fn(usize) -> Option<Url>) -> Option<Vec<TypeHierarchyItem>> {
    Some(vec![item(a, type_at(a, file, off)?, uri_of)?])
}

/// The type an item names.
fn target(a: &Analysis, it: &TypeHierarchyItem) -> Option<u32> {
    let name = it.data.as_ref().and_then(|d| d.get("type")).and_then(|v| v.as_str()).map(str::to_string).or_else(|| it.detail.clone())?;
    a.types.find(&name)
}

/// `typeHierarchy/supertypes` (`up`) or `typeHierarchy/subtypes`.
pub(crate) fn related(a: &Analysis, it: &TypeHierarchyItem, up: bool, uri_of: &impl Fn(usize) -> Option<Url>) -> Option<Vec<TypeHierarchyItem>> {
    let t = target(a, it)?;
    let ids = if up { a.types.supertypes(t) } else { a.types.subtypes(t) };
    Some(ids.iter().filter_map(|&r| item(a, r, uri_of)).collect())
}
