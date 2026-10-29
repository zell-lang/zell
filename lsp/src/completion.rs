//! Completion: members after `.` (typed by sema), names in scope,
//! auto-import items and keywords / statement snippets.
//!
//! Every item carries the exact replaced range (`textEdit`, or
//! `InsertReplaceEdit` when the client supports it) and a `sortText` from
//! the server's relevance order: prefix matches before fuzzy ones, then
//! locals > parameters > fields > other members > own-namespace globals >
//! imported / std / builtin names > auto-import items > keywords; within a
//! rank, members keep declaration order, locals the nearest declaration
//! first, the rest sort by label. The list is filtered by the word prefix
//! (case-insensitive subsequence) and cut to [`MAX_ITEMS`]
//! (`isIncomplete` then). Calls get snippets (`name(${1:a}, ${2:b})`,
//! `name()$0`) only when the client advertises `snippetSupport`.

use crate::features::{hover_markdown, BUILTIN_FUNCS, BUILTIN_TYPES, KEYWORDS};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionList, CompletionTextEdit, InsertReplaceEdit, InsertTextFormat, InsertTextMode, TextEdit,
};
use std::collections::{HashMap, HashSet};
use zell::sema::index::{SymKey, SymbolId, SymbolKind};
use zell::session::Analysis;
use zell::span::Span;
use zell::types::{Type, TypeId};

/// The longest list sent; longer lists are cut after sorting and marked
/// incomplete, so the client asks again as the prefix grows.
pub(crate) const MAX_ITEMS: usize = 200;

/// What the client accepts in completion items (from `initialize`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CompletionCaps {
    /// `completionItem.snippetSupport`.
    pub snippets: bool,
    /// `completionItem.insertReplaceSupport`.
    pub insert_replace: bool,
}

// Relevance ranks (lower sorts first).
const R_LOCAL: u8 = 0;
const R_PARAM: u8 = 1;
const R_FIELD: u8 = 2;
const R_MEMBER: u8 = 3;
const R_OWN_NS: u8 = 4;
const R_IMPORTED: u8 = 5;
const R_AUTO_IMPORT: u8 = 6;
const R_KEYWORD: u8 = 7;

/// A candidate item with its relevance.
struct Cand {
    item: CompletionItem,
    rank: u8,
    /// Tie-break inside a rank (declaration order, nearest local first).
    order: u32,
}

impl Cand {
    fn new(item: CompletionItem, rank: u8) -> Cand {
        Cand { item, rank, order: 0 }
    }
}


/// What the expression left of a `.` denotes.
#[derive(Clone, Debug)]
enum Target {
    Ns(String),
    /// A struct, enum or interface used by name (static members, variants).
    Type(SymbolId),
    /// A value of struct type (or pointer to struct).
    StructValue(u32),
    IfaceValue(u32),
    /// A value of enum type: its methods.
    EnumValue(u32),
    /// A tuple: its elements `0`, `1`, ...
    TupleValue(Vec<TypeId>),
    ErrorValue,
    /// A value of a std struct or of a builtin type documented by an
    /// `@intrinsic` std struct (`Task<T>`: `task.Task`): the members of
    /// that std symbol (session/stdblock.rs).
    StdValue(SymbolId),
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn completion_kind(k: SymbolKind) -> CompletionItemKind {
    match k {
        SymbolKind::Namespace => CompletionItemKind::MODULE,
        SymbolKind::Struct => CompletionItemKind::STRUCT,
        SymbolKind::Interface => CompletionItemKind::INTERFACE,
        SymbolKind::Enum => CompletionItemKind::ENUM,
        SymbolKind::Variant => CompletionItemKind::ENUM_MEMBER,
        SymbolKind::Field => CompletionItemKind::FIELD,
        SymbolKind::Method => CompletionItemKind::METHOD,
        SymbolKind::Func | SymbolKind::BuiltinFunc => CompletionItemKind::FUNCTION,
        SymbolKind::Const => CompletionItemKind::CONSTANT,
        SymbolKind::Global | SymbolKind::Local | SymbolKind::Param => CompletionItemKind::VARIABLE,
        SymbolKind::BuiltinType => CompletionItemKind::TYPE_PARAMETER,
    }
}

/// A completion item for a symbol: light (label, kind, one-line detail);
/// `data` names the symbol so `completionItem/resolve` can add its
/// documentation ([`resolve_completion`]).
fn item(a: &Analysis, label: &str, id: SymbolId) -> CompletionItem {
    let s = a.index.symbol(id);
    CompletionItem {
        label: label.to_string(),
        kind: Some(completion_kind(s.kind)),
        detail: s.signature.lines().next().map(|l| l.to_string()),
        data: Some(crate::symbol_data(a, id)),
        ..Default::default()
    }
}

/// `completionItem/resolve`: the detail (signature) and the Markdown
/// documentation of the symbol named by the item's `data`.
pub(crate) fn resolve_completion(a: &Analysis, generation: u64, mut item: CompletionItem) -> CompletionItem {
    let Some(id) = item.data.as_ref().and_then(|d| crate::symbol_from_data(a, generation, d)) else { return item };
    let s = a.index.symbol(id);
    item.detail = s.signature.lines().next().map(|l| l.to_string());
    item.documentation = Some(lsp_types::Documentation::MarkupContent(lsp_types::MarkupContent { kind: lsp_types::MarkupKind::Markdown, value: hover_markdown(a, id) }));
    item
}

fn plain(label: &str, kind: CompletionItemKind, detail: Option<&str>) -> CompletionItem {
    CompletionItem { label: label.to_string(), kind: Some(kind), detail: detail.map(|d| d.to_string()), ..Default::default() }
}

fn exported(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// The completion list at `off`: members after `.`, else names in scope,
/// auto-import items and keywords. Unknown left-hand sides give no items
/// rather than noise.
pub(crate) fn completions(a: &Analysis, file: usize, off: u32, caps: CompletionCaps) -> CompletionList {
    let empty = CompletionList { is_incomplete: false, items: Vec::new() };
    let text = a.files[file].text.as_str();
    let off = (off as usize).min(text.len());
    if !text.is_char_boundary(off) {
        return empty;
    }
    let bytes = text.as_bytes();
    let line_start = text[..off].rfind('\n').map_or(0, |i| i + 1);
    if in_comment_or_string(&text[line_start..off]) {
        return empty;
    }
    let mut start = off;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = off;
    while end < bytes.len() && is_ident_byte(bytes[end]) {
        end += 1;
    }
    let after_dot = start > 0 && bytes[start - 1] == b'.' && !(start >= 2 && bytes[start - 2] == b'.');
    let cands = if after_dot {
        let items = member_items(a, file, start);
        // Fields before methods, each in declaration order (own members
        // before promoted ones).
        items.into_iter().enumerate().map(|(i, it)| Cand { rank: if it.kind == Some(CompletionItemKind::FIELD) { R_FIELD } else { R_MEMBER }, item: it, order: i as u32 }).collect()
    } else {
        let before = &text[line_start..start];
        let line_prefix = before.trim_end();
        let stmt_pos = line_prefix.is_empty() || line_prefix.ends_with('{') || line_prefix.ends_with(';');
        let indent = &before[..before.len() - before.trim_start().len()];
        scope_cands(a, file, start as u32, &text[start..off], stmt_pos, indent, caps)
    };
    let rest = text[end..].trim_start_matches([' ', '\t']);
    let call_ok = !rest.starts_with('(');
    finish(a, file, cands, &text[start..off], Span::new(start as u32, off as u32), end as u32, call_ok, caps)
}

/// Members after the `.` before `start`.
fn member_items(a: &Analysis, file: usize, start: usize) -> Vec<CompletionItem> {
    let text = a.files[file].text.as_str();
    let bytes = text.as_bytes();
    // Typed path: sema recorded the receiver of the member access whose
    // name starts here (`f().`, `xs[0].`, `(await g()).`, ...).
    if let Some(target) = a.index.member_receiver(file, start as u32).and_then(|ty| value_target(a, ty)) {
        let items = target_members(a, file, target);
        if !items.is_empty() {
            return items;
        }
    }
    // Fallback (namespaces, types, a stale analysis): a chain of names.
    let mut segs: Vec<&str> = Vec::new();
    let mut end = start - 1;
    loop {
        let mut s = end;
        while s > 0 && is_ident_byte(bytes[s - 1]) {
            s -= 1;
        }
        if s == end || bytes[s].is_ascii_digit() {
            return Vec::new();
        }
        segs.push(&text[s..end]);
        if s > 0 && bytes[s - 1] == b'.' && !(s >= 2 && bytes[s - 2] == b'.') {
            end = s - 1;
            continue;
        }
        break;
    }
    segs.reverse();
    member_completions(a, file, start as u32, &segs)
}

/// How well `label` matches the typed `prefix`: 0 prefix, 1 prefix
/// ignoring case, 2 subsequence ignoring case; `None` if it does not.
fn match_quality(label: &str, prefix: &str) -> Option<u8> {
    if label.starts_with(prefix) {
        return Some(0);
    }
    let (l, p) = (label.to_lowercase(), prefix.to_lowercase());
    if l.starts_with(&p) {
        return Some(1);
    }
    let mut it = l.chars();
    p.chars().all(|c| it.any(|x| x == c)).then_some(2)
}

/// Filters by `prefix`, sorts, cuts to [`MAX_ITEMS`] and sets `sortText`,
/// the edit of the replaced range and call snippets.
#[allow(clippy::too_many_arguments)]
fn finish(a: &Analysis, file: usize, cands: Vec<Cand>, prefix: &str, insert: Span, word_end: u32, call_ok: bool, caps: CompletionCaps) -> CompletionList {
    let mut v: Vec<((bool, u8, u8, u32, String), Cand)> = cands
        .into_iter()
        .filter_map(|c| {
            let q = match_quality(&c.item.label, prefix)?;
            Some(((q == 2, c.rank, q, c.order, c.item.label.clone()), c))
        })
        .collect();
    v.sort_by(|x, y| x.0.cmp(&y.0));
    let is_incomplete = v.len() > MAX_ITEMS;
    v.truncate(MAX_ITEMS);
    let f = &a.files[file];
    let insert_range = crate::range(f, insert);
    let replace_range = crate::range(f, Span::new(insert.start, word_end));
    let items = v
        .into_iter()
        .enumerate()
        .map(|(i, (_, c))| {
            let mut it = c.item;
            it.sort_text = Some(format!("{i:04}"));
            if it.insert_text.is_none() && caps.snippets && call_ok && matches!(it.kind, Some(CompletionItemKind::FUNCTION | CompletionItemKind::METHOD)) {
                if let Some(s) = it.detail.as_deref().and_then(|d| call_snippet(&it.label, d)) {
                    it.insert_text = Some(s);
                    it.insert_text_format = Some(InsertTextFormat::SNIPPET);
                }
            }
            let new_text = it.insert_text.take().unwrap_or_else(|| it.label.clone());
            if it.filter_text.is_none() && new_text != it.label {
                it.filter_text = Some(it.label.clone());
            }
            it.text_edit = Some(if caps.insert_replace {
                CompletionTextEdit::InsertAndReplace(InsertReplaceEdit { new_text, insert: insert_range, replace: replace_range })
            } else {
                CompletionTextEdit::Edit(TextEdit { range: insert_range, new_text })
            });
            it
        })
        .collect();
    CompletionList { is_incomplete, items }
}

/// The parameter names of the first parameter list in a signature
/// (`func Max<T: Ordered>(a T, b T) T` -> `a`, `b`).
fn signature_params(sig: &str) -> Option<Vec<String>> {
    let rest = &sig[sig.find("func ")? + 5..];
    let rest = rest.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_' || c == '.');
    let mut chars = rest.char_indices();
    let mut depth = 0i32;
    let mut open = None;
    for (i, c) in chars.by_ref() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            '(' if depth == 0 => {
                open = Some(i + 1);
                break;
            }
            _ => {}
        }
    }
    let open = open?;
    let mut depth = 0i32;
    let mut params = Vec::new();
    let mut cur = open;
    let mut prev = '(';
    for (i, c) in chars {
        let arrow = prev == '=';
        prev = c;
        match c {
            '>' if arrow => {}
            '(' | '[' | '<' | '{' => depth += 1,
            ')' if depth == 0 => {
                let p = rest[cur..i].trim();
                if !p.is_empty() {
                    params.push(p.split_whitespace().next()?.to_string());
                }
                return Some(params);
            }
            ')' | ']' | '>' | '}' => depth -= 1,
            ',' if depth == 0 => {
                params.push(rest[cur..i].split_whitespace().next()?.to_string());
                cur = i + 1;
            }
            _ => {}
        }
    }
    None
}

/// `name(${1:a}, ${2:b})` for a call of a function with signature `sig`,
/// `name()$0` without parameters.
fn call_snippet(name: &str, sig: &str) -> Option<String> {
    let ps = signature_params(sig)?;
    if ps.is_empty() {
        return Some(format!("{name}()$0"));
    }
    let args: Vec<String> = ps.iter().enumerate().map(|(i, p)| format!("${{{}:{}}}", i + 1, p.trim_start_matches("..."))).collect();
    Some(format!("{name}({})", args.join(", ")))
}

/// The innermost function or method whose declaration contains `off`.
pub(crate) fn enclosing_fn(a: &Analysis, file: usize, off: u32) -> Option<SymbolId> {
    a.index
        .symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Func | SymbolKind::Method))
        .filter_map(|s| s.loc.filter(|l| l.file == file && l.full_span.start <= off && off <= l.full_span.end).map(|l| (s.id, l.full_span.end - l.full_span.start)))
        .min_by_key(|x| x.1)
        .map(|x| x.0)
}

/// Locals, parameters and local constants of the enclosing function that
/// are declared before `off` (latest declaration per name).
fn visible_locals(a: &Analysis, file: usize, off: u32) -> Vec<(String, SymbolId)> {
    let Some(func) = enclosing_fn(a, file, off) else { return Vec::new() };
    let mut best: HashMap<String, (u32, SymbolId)> = HashMap::new();
    for s in &a.index.symbols {
        if s.container != Some(func) || !matches!(s.kind, SymbolKind::Local | SymbolKind::Param | SymbolKind::Const) {
            continue;
        }
        let Some(l) = s.loc.filter(|l| l.file == file && l.name_span.end <= off) else { continue };
        let e = best.entry(s.name.to_string()).or_insert((l.name_span.start, s.id));
        if l.name_span.start >= e.0 {
            *e = (l.name_span.start, s.id);
        }
    }
    let mut v: Vec<(String, SymbolId)> = best.into_iter().map(|(n, (_, id))| (n, id)).collect();
    v.sort();
    v
}

fn file_ns(a: &Analysis, file: usize) -> Option<&str> {
    a.index.files.get(file)?.as_ref().map(|f| f.namespace.as_str())
}

/// The symbol a name written at `off` refers to: a visible local, else a
/// global name (own namespace, imports, root namespaces).
pub(crate) fn resolve_name(a: &Analysis, file: usize, off: u32, name: &str) -> Option<SymbolId> {
    if let Some((_, id)) = visible_locals(a, file, off).into_iter().find(|(n, _)| n == name) {
        return Some(id);
    }
    global_name(a, file, name)
}

/// Resolves a non-local name like sema's global lookup: own namespace,
/// imports, root namespaces.
fn global_name(a: &Analysis, file: usize, name: &str) -> Option<SymbolId> {
    let info = a.index.files.get(file)?.as_ref()?;
    if let Some(m) = a.index.ns_members.get(&info.namespace).and_then(|ms| ms.iter().find(|(n, _)| n == name)) {
        return Some(m.1);
    }
    for imp in &info.imports {
        match imp {
            zell::sema::index::ImportInfo::Named(alias, id) if alias == name => return Some(*id),
            zell::sema::index::ImportInfo::Glob(ns) => {
                let is_std = zell::sema::stdlib::is_std_namespace(ns);
                if let Some((_, id)) = a.index.ns_members.get(ns).and_then(|ms| ms.iter().find(|(n, id)| n == name && (is_std || exported(n) || a.index.symbol(*id).kind == SymbolKind::Namespace))) {
                    return Some(*id);
                }
            }
            _ => {}
        }
    }
    a.index.lookup(&SymKey::Ns(name.to_string()))
}

/// The member target of a value of type `ty`. A `T!` value offers the
/// members of `T` (the user adds `?` or `try`; no edit is made since `?`
/// is only valid in a fallible function), a `weak *T` those of `T`.
fn value_target(a: &Analysis, ty: TypeId) -> Option<Target> {
    let tt = &a.hir.types;
    let mut t = tt.get(ty);
    for _ in 0..4 {
        match t {
            Type::Pointer(inner) | Type::Weak(inner) | Type::Fallible(inner) => t = tt.get(*inner),
            _ => break,
        }
    }
    let std = |path: &str| a.index.lookup(&SymKey::Std(path.to_string())).map(Target::StdValue);
    match t {
        Type::Struct(d) if struct_symbol(a, d.0).is_none() => a.hir.structs.get(d.0 as usize).and_then(|st| std(&zell::sema::index::strip_type_args(&st.full_name))),
        Type::Task(_) => std("task.Task"),
        Type::Struct(d) => Some(Target::StructValue(d.0)),
        Type::Interface(d) => Some(Target::IfaceValue(d.0)),
        Type::Enum(d) => Some(Target::EnumValue(d.0)),
        Type::Tuple(elems) => Some(Target::TupleValue(elems.clone())),
        Type::Error => Some(Target::ErrorValue),
        _ => None,
    }
}

/// The symbol of user struct `sid`: its own, or for an instance of a
/// generic struct (`Box<int>`) the symbol of the generic declaration.
fn struct_symbol(a: &Analysis, sid: u32) -> Option<SymbolId> {
    if let Some(id) = a.index.lookup(&SymKey::Struct(sid)) {
        return Some(id);
    }
    let st = a.hir.structs.get(sid as usize)?;
    if !st.full_name.contains('<') {
        return None;
    }
    let name = zell::sema::index::strip_type_args(&st.full_name);
    a.index.symbols.iter().find(|s| s.kind == SymbolKind::Struct && !s.is_std && *s.full_name == *name).map(|s| s.id)
}

/// Same lookup for interfaces (generic instances by their declaration).
fn iface_symbol(a: &Analysis, iid: u32) -> Option<SymbolId> {
    if let Some(id) = a.index.lookup(&SymKey::Interface(iid)) {
        return Some(id);
    }
    let name = zell::sema::index::strip_type_args(&a.hir.interfaces.get(iid as usize)?.full_name);
    a.index.symbols.iter().find(|s| s.kind == SymbolKind::Interface && *s.full_name == *name).map(|s| s.id)
}

fn symbol_target(a: &Analysis, id: SymbolId) -> Option<Target> {
    let s = a.index.symbol(id);
    match s.kind {
        SymbolKind::Namespace => Some(Target::Ns(s.full_name.to_string())),
        SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Interface => Some(Target::Type(id)),
        SymbolKind::Local | SymbolKind::Param | SymbolKind::Field | SymbolKind::Global | SymbolKind::Const => value_target(a, s.ty?),
        _ => None,
    }
}

/// Field `name` of struct `sid`, following embedded structs.
fn struct_field_ty(a: &Analysis, sid: u32, name: &str, depth: usize) -> Option<TypeId> {
    let st = a.hir.structs.get(sid as usize)?;
    if let Some(f) = st.fields.iter().find(|f| f.name == name) {
        return Some(f.ty);
    }
    if depth > 8 {
        return None;
    }
    st.fields.iter().filter(|f| f.embedded).find_map(|f| match a.hir.types.get(f.ty) {
        Type::Struct(d) => struct_field_ty(a, d.0, name, depth + 1),
        _ => None,
    })
}

fn member_completions(a: &Analysis, file: usize, off: u32, segs: &[&str]) -> Vec<CompletionItem> {
    let head = segs[0];
    let mut target = if head == "this" {
        enclosing_fn(a, file, off).and_then(|f| a.index.symbol(f).container).and_then(|c| match a.index.symbol(c).key {
            SymKey::Struct(s) => Some(Target::StructValue(s)),
            _ => None,
        })
    } else if let Some((_, id)) = visible_locals(a, file, off).into_iter().find(|(n, _)| n == head) {
        symbol_target(a, id)
    } else {
        global_name(a, file, head).and_then(|id| symbol_target(a, id))
    };
    for seg in &segs[1..] {
        target = match target {
            Some(Target::Ns(path)) => a.index.ns_members.get(&path).and_then(|ms| ms.iter().find(|(n, _)| n == seg)).and_then(|(_, id)| symbol_target(a, *id)),
            Some(Target::StructValue(sid)) => struct_field_ty(a, sid, seg, 0).and_then(|t| value_target(a, t)),
            _ => None,
        };
    }
    match target {
        Some(target) => target_members(a, file, target),
        None => Vec::new(),
    }
}

/// The completion items of the members of `target` as seen from `file`.
fn target_members(a: &Analysis, file: usize, target: Target) -> Vec<CompletionItem> {
    let own_ns = file_ns(a, file).unwrap_or("").to_string();
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    match target {
        Target::Ns(path) => {
            let is_std = zell::sema::stdlib::is_std_namespace(&path);
            for (name, id) in a.index.ns_members.get(&path).into_iter().flatten() {
                if is_std || path == own_ns || exported(name) || a.index.symbol(*id).kind == SymbolKind::Namespace {
                    out.push(item(a, name, *id));
                }
            }
        }
        Target::Type(id) => {
            let s = a.index.symbol(id);
            for c in a.index.children(id) {
                let cs = a.index.symbol(c);
                let ok = match s.kind {
                    SymbolKind::Struct => cs.kind == SymbolKind::Method && cs.is_static,
                    SymbolKind::Enum => cs.kind == SymbolKind::Variant,
                    _ => false,
                };
                if ok && seen.insert(cs.name.to_string()) {
                    out.push(item(a, &cs.name, c));
                }
            }
        }
        Target::StructValue(sid) => struct_members(a, sid, &own_ns, &mut out, &mut seen, 0),
        Target::IfaceValue(iid) => {
            if let Some(id) = iface_symbol(a, iid) {
                for c in a.index.children(id) {
                    out.push(item(a, &a.index.symbol(c).name, c));
                }
            }
        }
        Target::EnumValue(eid) => {
            if let Some(id) = a.index.lookup(&SymKey::Enum(eid)) {
                for c in a.index.children(id) {
                    let cs = a.index.symbol(c);
                    if matches!(cs.kind, SymbolKind::Method | SymbolKind::Func) && !cs.is_static && seen.insert(cs.name.to_string()) {
                        out.push(item(a, &cs.name, c));
                    }
                }
            }
        }
        Target::TupleValue(elems) => {
            for (i, t) in elems.iter().enumerate() {
                out.push(plain(&i.to_string(), CompletionItemKind::FIELD, Some(&zell::sema::index::short_type_name(&a.hir, *t))));
            }
        }
        Target::ErrorValue => out.push(plain("Message", CompletionItemKind::METHOD, Some("func Message() string"))),
        Target::StdValue(id) => {
            for c in a.index.children(id) {
                let cs = a.index.symbol(c);
                if matches!(cs.kind, SymbolKind::Field | SymbolKind::Method) && !cs.is_static && exported(&cs.name) && seen.insert(cs.name.to_string()) {
                    out.push(item(a, &cs.name, c));
                }
            }
        }
    }
    out
}

/// Fields and non-static methods of struct `sid`, then promoted members.
/// For an instance of a generic struct, field details show the
/// substituted types (`Value int` of `Box<int>`).
fn struct_members(a: &Analysis, sid: u32, own_ns: &str, out: &mut Vec<CompletionItem>, seen: &mut HashSet<String>, depth: usize) {
    let Some(id) = struct_symbol(a, sid) else { return };
    let s = a.index.symbol(id);
    let ns = s.full_name.strip_suffix(&format!(".{}", s.name)).unwrap_or("");
    let instance = a.index.lookup(&SymKey::Struct(sid)).is_none();
    for c in a.index.children(id) {
        let cs = a.index.symbol(c);
        let wanted = cs.kind == SymbolKind::Field || (cs.kind == SymbolKind::Method && !cs.is_static);
        if wanted && (ns == own_ns || exported(&cs.name)) && seen.insert(cs.name.to_string()) {
            let mut it = item(a, &cs.name, c);
            if instance && cs.kind == SymbolKind::Field {
                if let Some(f) = a.hir.structs.get(sid as usize).and_then(|st| st.fields.iter().find(|f| *f.name == *cs.name)) {
                    it.detail = Some(format!("{} {}", cs.name, zell::sema::index::short_type_name(&a.hir, f.ty)));
                }
            }
            out.push(it);
        }
    }
    if depth > 8 {
        return;
    }
    if let Some(st) = a.hir.structs.get(sid as usize) {
        for f in st.fields.iter().filter(|f| f.embedded) {
            if let Type::Struct(d) = a.hir.types.get(f.ty) {
                struct_members(a, d.0, own_ns, out, seen, depth + 1);
            }
        }
    }
}

/// Names in scope at `start` (locals, own namespace, imports, root
/// namespaces, builtins), auto-import items for `prefix`, and keywords
/// for the position (`stmt_pos`: first word of a statement).
fn scope_cands(a: &Analysis, file: usize, start: u32, prefix: &str, stmt_pos: bool, indent: &str, caps: CompletionCaps) -> Vec<Cand> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (name, id) in visible_locals(a, file, start) {
        if seen.insert(name.clone()) {
            let s = a.index.symbol(id);
            let rank = if s.kind == SymbolKind::Param { R_PARAM } else { R_LOCAL };
            let order = u32::MAX - s.loc.map_or(0, |l| l.name_span.start);
            out.push(Cand { item: item(a, &name, id), rank, order });
        }
    }
    if let Some(Some(info)) = a.index.files.get(file) {
        for (name, id) in a.index.ns_members.get(&info.namespace).into_iter().flatten() {
            if seen.insert(name.clone()) {
                out.push(Cand::new(item(a, name, *id), R_OWN_NS));
            }
        }
        for imp in &info.imports {
            match imp {
                zell::sema::index::ImportInfo::Named(alias, id) => {
                    if seen.insert(alias.clone()) {
                        out.push(Cand::new(item(a, alias, *id), R_IMPORTED));
                    }
                }
                zell::sema::index::ImportInfo::Glob(ns) => {
                    let is_std = zell::sema::stdlib::is_std_namespace(ns);
                    for (name, id) in a.index.ns_members.get(ns).into_iter().flatten() {
                        if (is_std || exported(name)) && seen.insert(name.clone()) {
                            out.push(Cand::new(item(a, name, *id), R_IMPORTED));
                        }
                    }
                }
            }
        }
    }
    for path in a.index.ns_members.keys().filter(|p| !p.contains('.')) {
        if let Some(id) = a.index.lookup(&SymKey::Ns(path.clone())) {
            if seen.insert(path.clone()) {
                out.push(Cand::new(item(a, path, id), R_IMPORTED));
            }
        }
    }
    for (name, sig) in BUILTIN_FUNCS {
        if seen.insert(name.to_string()) {
            out.push(Cand::new(plain(name, CompletionItemKind::FUNCTION, Some(sig)), R_IMPORTED));
        }
    }
    for name in BUILTIN_TYPES {
        if seen.insert(name.to_string()) {
            out.push(Cand::new(plain(name, CompletionItemKind::TYPE_PARAMETER, Some("builtin type")), R_IMPORTED));
        }
    }
    if !prefix.is_empty() {
        auto_import_cands(a, file, prefix, &seen, &mut out);
    }
    let func = enclosing_fn(a, file, start);
    keyword_cands(a, func, stmt_pos, indent, caps, &mut out);
    out
}

/// Exported types, functions, constants and globals of namespaces the file
/// does not import whose name starts with `prefix` (ignoring case), each
/// with the edit adding the `import` line. Names already visible win.
fn auto_import_cands(a: &Analysis, file: usize, prefix: &str, seen: &HashSet<String>, out: &mut Vec<Cand>) {
    let (Some(Some(info)), Some(ast)) = (a.index.files.get(file), a.asts.get(file)) else { return };
    let text = a.files[file].text.as_str();
    let imported: HashSet<&str> = info
        .imports
        .iter()
        .filter_map(|i| match i {
            zell::sema::index::ImportInfo::Glob(ns) => Some(ns.as_str()),
            _ => None,
        })
        .collect();
    let lower = prefix.to_lowercase();
    let mut nss: Vec<&String> = a.index.ns_members.keys().filter(|ns| **ns != info.namespace && !imported.contains(ns.as_str())).collect();
    nss.sort();
    for ns in nss {
        let is_std = zell::sema::stdlib::is_std_namespace(ns);
        let mut edit = None;
        for (name, id) in &a.index.ns_members[ns] {
            let kind = a.index.symbol(*id).kind;
            let wanted = matches!(kind, SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Func | SymbolKind::Const | SymbolKind::Global);
            if !wanted || !(is_std || exported(name)) || seen.contains(name) || !name.to_lowercase().starts_with(&lower) {
                continue;
            }
            let (span, new_text) = edit.get_or_insert_with(|| crate::refactor::import_edit(ast, text, ns)).clone();
            let mut it = item(a, name, *id);
            it.label_details = Some(CompletionItemLabelDetails { detail: None, description: Some(format!("import {ns}")) });
            it.additional_text_edits = Some(vec![TextEdit { range: crate::range(&a.files[file], span), new_text }]);
            out.push(Cand::new(it, R_AUTO_IMPORT));
        }
    }
}

/// Words that cannot continue an expression in the middle of a line.
const STMT_ONLY: &[&str] = &["namespace", "import", "struct", "interface", "enum", "static", "var", "const", "return", "break", "continue", "defer", "for", "switch", "case", "default"];

/// Words that start a top-level declaration.
const DECL_WORDS: &[&str] = &["namespace", "import", "struct", "interface", "enum", "func", "static", "async", "const", "var"];

/// Keywords for the position, with statement snippets (`if`, `for`,
/// `switch`, `defer`, `defer await` in async functions, `try`, `return`)
/// at the start of a statement when the client takes snippets.
fn keyword_cands(a: &Analysis, func: Option<SymbolId>, stmt_pos: bool, indent: &str, caps: CompletionCaps, out: &mut Vec<Cand>) {
    let sig = func.map(|f| a.index.symbol(f).signature.lines().next().unwrap_or("").to_string());
    let words: Vec<&str> = match (&sig, stmt_pos) {
        (None, true) => DECL_WORDS.to_vec(),
        (_, false) => KEYWORDS.iter().copied().filter(|k| !STMT_ONLY.contains(k)).collect(),
        (Some(_), true) => KEYWORDS.iter().copied().filter(|k| !matches!(*k, "namespace" | "import" | "static" | "as" | "in")).collect(),
    };
    let snippets = caps.snippets && stmt_pos && sig.is_some();
    let is_async = sig.as_deref().is_some_and(|s| s.split("func ").next().is_some_and(|m| m.contains("async")));
    let void = sig.as_deref().is_some_and(|s| s.trim_end().ends_with(')'));
    let unit = if indent.contains('\t') { "\t" } else { "    " };
    let body = format!("{indent}{unit}");
    let snippet = |kw: &str| -> Option<(String, &'static str)> {
        Some(match kw {
            "if" => (format!("if ${{1:cond}} {{\n{body}$0\n{indent}}}"), "if cond { … }"),
            "for" => (format!("for ${{1:x}} in ${{2:xs}} {{\n{body}$0\n{indent}}}"), "for x in xs { … }"),
            "switch" => (format!("switch ${{1:x}} {{\n{body}case ${{2:v}}:\n{body}{unit}$0\n{indent}}}"), "switch x { case … }"),
            "defer" => ("defer $0".to_string(), "defer call"),
            "try" => (format!("try {{\n{body}$0\n{indent}}} catch ${{1:err}} {{\n{body}\n{indent}}}"), "try { … } catch err { … }"),
            "return" if !void => ("return $0".to_string(), "return value"),
            _ => return None,
        })
    };
    for kw in words {
        let mut it = plain(kw, CompletionItemKind::KEYWORD, None);
        if snippets {
            if let Some((text, detail)) = snippet(kw) {
                it.insert_text = Some(text);
                it.insert_text_format = Some(InsertTextFormat::SNIPPET);
                it.insert_text_mode = Some(InsertTextMode::AS_IS);
                it.detail = Some(detail.to_string());
            }
        }
        out.push(Cand::new(it, R_KEYWORD));
    }
    if stmt_pos && is_async {
        let mut it = plain("defer await", CompletionItemKind::KEYWORD, Some("defer await call"));
        if snippets {
            it.insert_text = Some("defer await $0".to_string());
            it.insert_text_format = Some(InsertTextFormat::SNIPPET);
        }
        out.push(Cand::new(it, R_KEYWORD));
    }
}

/// Rough check whether the line prefix ends inside a `//` comment or a string.
fn in_comment_or_string(prefix: &str) -> bool {
    let mut in_str = false;
    let mut chars = prefix.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_str => {
                chars.next();
            }
            '"' => in_str = !in_str,
            '/' if !in_str && chars.peek() == Some(&'/') => return true,
            _ => {}
        }
    }
    in_str
}
