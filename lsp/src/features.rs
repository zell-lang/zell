//! Feature logic on top of an [`Analysis`]: hover text, outline
//! conversion, semantic tokens, folding ranges and completion.

use crate::range;
use lsp_types::{
    CompletionItem, CompletionItemKind, FoldingRange, FoldingRangeKind, Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensEdit,
    SemanticTokensLegend,
};
use zell::diagnostics::Diagnostics;
use zell::lexer;
use zell::sema::index::{SymKey, Symbol, SymbolId, SymbolKind};
use zell::session::{Analysis, DocumentSymbol};
use zell::span::{SourceFile, Span};
use zell::token::TokenKind;
use zell::types::{Type, TypeId};
use std::collections::{HashMap, HashSet};

/// Semantic token types, in legend order.
pub const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "namespace", "type", "struct", "enum", "enumMember", "interface", "function", "method", "property", "variable", "parameter", "keyword", "string", "number", "comment",
    "operator",
];

/// Semantic token modifiers, in legend order (bit i = entry i).
pub const SEMANTIC_TOKEN_MODIFIERS: &[&str] = &["declaration", "static", "async", "readonly"];

const T_NAMESPACE: u32 = 0;
const T_TYPE: u32 = 1;
const T_STRUCT: u32 = 2;
const T_ENUM: u32 = 3;
const T_ENUM_MEMBER: u32 = 4;
const T_INTERFACE: u32 = 5;
const T_FUNCTION: u32 = 6;
const T_METHOD: u32 = 7;
const T_PROPERTY: u32 = 8;
const T_VARIABLE: u32 = 9;
const T_PARAMETER: u32 = 10;
const T_KEYWORD: u32 = 11;
const T_STRING: u32 = 12;
const T_NUMBER: u32 = 13;
const T_COMMENT: u32 = 14;
const T_OPERATOR: u32 = 15;

const M_DECLARATION: u32 = 1;
const M_STATIC: u32 = 2;
const M_ASYNC: u32 = 4;
const M_READONLY: u32 = 8;

pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: SEMANTIC_TOKEN_TYPES.iter().map(|t| SemanticTokenType::new(t)).collect(),
        token_modifiers: SEMANTIC_TOKEN_MODIFIERS.iter().map(|t| SemanticTokenModifier::new(t)).collect(),
    }
}

const KEYWORDS: &[&str] = &[
    "namespace", "import", "as", "struct", "interface", "enum", "func", "static", "async", "await", "var", "const", "if", "else", "for", "in", "switch", "case", "default",
    "return", "break", "continue", "defer", "try", "catch", "new", "nil", "true", "false", "map", "this",
];

const BUILTIN_FUNCS: &[(&str, &str)] = &[
    ("len", "func len(v) int"),
    ("append", "func append(s []T, v T) []T"),
    ("delete", "func delete(m map[K]V, k K)"),
    ("panic", "func panic(msg string)"),
    ("error", "func error(msg string) error"),
];

const BUILTIN_TYPES: &[&str] = &["int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32", "uint64", "byte", "float", "float32", "float64", "bool", "char", "string", "any", "error", "void"];

/// True for a valid, non-keyword identifier.
pub(crate) fn is_identifier(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && cs.all(|c| c.is_ascii_alphanumeric() || c == '_') && s != "_" && !KEYWORDS.contains(&s)
}

fn kind_word(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Namespace => "namespace",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::Variant => "enum variant",
        SymbolKind::Field => "field",
        SymbolKind::Method => "method",
        SymbolKind::Func => "function",
        SymbolKind::Const => "constant",
        SymbolKind::Global => "global variable",
        SymbolKind::Local => "local variable",
        SymbolKind::Param => "parameter",
        SymbolKind::BuiltinType => "builtin type",
        SymbolKind::BuiltinFunc => "builtin function",
    }
}

/// Markdown hover: signature block, kind and container, doc comment.
pub(crate) fn hover_markdown(a: &Analysis, id: SymbolId) -> String {
    let s = a.index.symbol(id);
    let mut out = format!("```zell\n{}\n```\n", s.signature);
    let mut line = format!("*{}*", kind_word(s.kind));
    if let Some(c) = s.container {
        let c = a.index.symbol(c);
        let what = match c.kind {
            SymbolKind::Namespace => "in",
            SymbolKind::Func | SymbolKind::Method => "in function",
            _ => "of",
        };
        line.push_str(&format!(" {what} `{}`", c.full_name));
    } else if s.is_std && s.full_name != s.name {
        line.push_str(&format!(" `{}`", s.full_name));
    }
    out.push_str(&line);
    out.push('\n');
    if let Some(doc) = &s.doc {
        // the `zell doc` pipeline: fenced code, Deprecated tag, symbol links
        let link = |t: &str| symbol_link(a, s, t);
        out.push_str("\n---\n\n");
        out.push_str(&zell::doc::markdown::to_markdown(&zell::doc::markdown::parse(doc), &link));
        out.push('\n');
    }
    // the first example of a std symbol (std/**/*_test.zl), collapsed
    if s.is_std {
        if let Some(e) = zell::doc::std_examples().get(&s.full_name).and_then(|v| v.first()) {
            out.push('\n');
            out.push_str(&zell::doc::example_markdown(e));
        }
    }
    out
}

/// `[`text`](file:///path#L12)` target of a code span in the doc of `s`
/// that names a symbol with a source location: tried as written, in the
/// namespace of `s` and under `std.`.
fn symbol_link(a: &Analysis, s: &Symbol, text: &str) -> Option<String> {
    let text = text.trim_end_matches("()");
    if text.is_empty() || !text.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
        return None;
    }
    let ns = s.full_name.rsplit_once('.').map_or("", |x| x.0);
    let parent_ns = ns.rsplit_once('.').map_or("", |x| x.0);
    let cands = [text.to_string(), format!("{ns}.{text}"), format!("{parent_ns}.{text}")];
    let sym = cands.iter().find_map(|c| a.index.symbols.iter().find(|x| x.full_name == *c && x.loc.is_some() && x.kind != SymbolKind::Local))?;
    let loc = sym.loc.as_ref()?;
    let f = a.files.get(loc.file)?;
    let (line, _) = f.line_col(loc.name_span.start);
    let url = lsp_types::Url::from_file_path(&f.path).ok()?;
    Some(format!("{url}#L{line}"))
}

pub(crate) fn lsp_kind(k: SymbolKind) -> lsp_types::SymbolKind {
    use lsp_types::SymbolKind as L;
    match k {
        SymbolKind::Namespace => L::NAMESPACE,
        SymbolKind::Struct => L::STRUCT,
        SymbolKind::Interface => L::INTERFACE,
        SymbolKind::Enum => L::ENUM,
        SymbolKind::Variant => L::ENUM_MEMBER,
        SymbolKind::Field => L::FIELD,
        SymbolKind::Method => L::METHOD,
        SymbolKind::Func | SymbolKind::BuiltinFunc => L::FUNCTION,
        SymbolKind::Const => L::CONSTANT,
        SymbolKind::Global | SymbolKind::Local | SymbolKind::Param => L::VARIABLE,
        SymbolKind::BuiltinType => L::CLASS,
    }
}

#[allow(deprecated)]
pub(crate) fn doc_symbol(f: &SourceFile, s: &DocumentSymbol) -> lsp_types::DocumentSymbol {
    let full = if s.full_span.start <= s.name_span.start && s.name_span.end <= s.full_span.end { s.full_span } else { s.name_span };
    lsp_types::DocumentSymbol {
        name: s.name.clone(),
        detail: (!s.detail.is_empty()).then(|| s.detail.clone()),
        kind: lsp_kind(s.kind),
        tags: None,
        deprecated: None,
        range: range(f, full),
        selection_range: range(f, s.name_span),
        children: Some(s.children.iter().map(|c| doc_symbol(f, c)).collect()),
    }
}

// ---------------------------------------------------------------------------
// Semantic tokens
// ---------------------------------------------------------------------------

fn token_class(s: &Symbol, decl: bool) -> (u32, u32) {
    let t = match s.kind {
        SymbolKind::Namespace => T_NAMESPACE,
        SymbolKind::Struct => T_STRUCT,
        SymbolKind::Interface => T_INTERFACE,
        SymbolKind::Enum => T_ENUM,
        SymbolKind::Variant => T_ENUM_MEMBER,
        SymbolKind::Field => T_PROPERTY,
        SymbolKind::Method => T_METHOD,
        SymbolKind::Func | SymbolKind::BuiltinFunc => T_FUNCTION,
        SymbolKind::Const | SymbolKind::Global | SymbolKind::Local => T_VARIABLE,
        SymbolKind::Param => T_PARAMETER,
        SymbolKind::BuiltinType => T_TYPE,
    };
    let mut m = 0;
    if decl {
        m |= M_DECLARATION;
    }
    if s.is_static {
        m |= M_STATIC;
    }
    if s.is_async {
        m |= M_ASYNC;
    }
    if s.is_readonly {
        m |= M_READONLY;
    }
    (t, m)
}

fn is_keyword(k: &TokenKind) -> bool {
    !matches!(k, TokenKind::Ident(_)) && k.text().chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

fn is_operator(k: &TokenKind) -> bool {
    use TokenKind::*;
    !matches!(k, Ident(_) | Int(_) | Float(_) | Str(_) | Char(_) | LParen | RParen | LBrace | RBrace | LBracket | RBracket | Comma | Dot | Colon | Semi | At | Eof | Underscore)
        && !is_keyword(k)
}

/// A semantic token with an absolute position (UTF-16 line/column).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AbsToken {
    pub line: u32,
    pub col: u32,
    pub len: u32,
    pub ty: u32,
    pub mods: u32,
}

/// Full-document semantic tokens (relative encoding).
pub(crate) fn semantic_tokens(a: &Analysis, file: usize) -> Vec<SemanticToken> {
    encode_tokens(&absolute_tokens(a, file))
}

/// The tokens of `file` that intersect `range` (relative encoding, as
/// `textDocument/semanticTokens/range` returns them).
pub(crate) fn semantic_tokens_range(a: &Analysis, file: usize, range: &Range) -> Vec<SemanticToken> {
    let (start, end) = ((range.start.line, range.start.character), (range.end.line, range.end.character));
    let toks: Vec<AbsToken> = absolute_tokens(a, file).into_iter().filter(|t| (t.line, t.col + t.len) > start && (t.line, t.col) < end).collect();
    encode_tokens(&toks)
}

/// Relative LSP encoding of tokens sorted by position.
pub(crate) fn encode_tokens(toks: &[AbsToken]) -> Vec<SemanticToken> {
    let (mut prev_line, mut prev_col) = (0u32, 0u32);
    toks.iter()
        .map(|t| {
            let delta_line = t.line - prev_line;
            let delta_start = if delta_line == 0 { t.col - prev_col } else { t.col };
            prev_line = t.line;
            prev_col = t.col;
            SemanticToken { delta_line, delta_start, length: t.len, token_type: t.ty, token_modifiers_bitset: t.mods }
        })
        .collect()
}

/// Edits turning `old` into `new` (`textDocument/semanticTokens/full/delta`):
/// one replacement between their common prefix and suffix, in units of the
/// flat integer array (5 per token); none when they are equal.
pub(crate) fn token_edits(old: &[SemanticToken], new: &[SemanticToken]) -> Vec<SemanticTokensEdit> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    if prefix == old.len() && prefix == new.len() {
        return Vec::new();
    }
    vec![SemanticTokensEdit {
        start: (prefix * 5) as u32,
        delete_count: ((old.len() - prefix - suffix) * 5) as u32,
        data: Some(new[prefix..new.len() - suffix].to_vec()),
    }]
}

/// Semantic tokens of `file` with absolute positions, sorted; tokens
/// spanning lines (block comments, raw strings) are split per line.
pub(crate) fn absolute_tokens(a: &Analysis, file: usize) -> Vec<AbsToken> {
    let f = &a.files[file];
    let text = f.text.as_str();
    let mut names: HashMap<(u32, u32), (SymbolId, bool)> = HashMap::new();
    for s in &a.index.symbols {
        if let Some(l) = s.loc.filter(|l| l.file == file) {
            names.insert((l.name_span.start, l.name_span.end), (s.id, true));
        }
    }
    for r in a.index.references.iter().filter(|r| r.file == file) {
        names.entry((r.span.start, r.span.end)).or_insert((r.symbol, false));
    }
    let mut diags = Diagnostics::new();
    let (tokens, comments) = lexer::lex_with_comments(text, &mut diags);
    let mut items: Vec<(Span, u32, u32)> = Vec::new();
    for t in &tokens {
        if t.span.start >= t.span.end {
            continue;
        }
        let class = match &t.kind {
            TokenKind::Ident(name) => names.get(&(t.span.start, t.span.end)).and_then(|(id, decl)| {
                let s = a.index.symbol(*id);
                // Only trust spans whose text matches the symbol (namespaces aside).
                (s.name == *name || matches!(s.key, SymKey::Ns(_))).then(|| token_class(s, *decl))
            }),
            TokenKind::Int(_) | TokenKind::Float(_) => Some((T_NUMBER, 0)),
            TokenKind::Str(_) | TokenKind::Char(_) => Some((T_STRING, 0)),
            k if is_keyword(k) => Some((T_KEYWORD, 0)),
            k if is_operator(k) => Some((T_OPERATOR, 0)),
            _ => None,
        };
        if let Some((ty, m)) = class {
            items.push((t.span, ty, m));
        }
    }
    for c in comments {
        items.push((c, T_COMMENT, 0));
    }
    items.sort_by_key(|i| i.0.start);
    let mut out = Vec::new();
    for (span, ty, mods) in items {
        // Split multi-line tokens (block comments, strings) per line.
        let mut start = span.start as usize;
        let end = (span.end as usize).min(text.len());
        while start < end {
            let line_end = text[start..end].find('\n').map_or(end, |i| start + i);
            if line_end > start {
                let (line, col) = crate::encoding::line_col(f, start as u32);
                let len: u32 = crate::encoding::len(text[start..line_end].trim_end_matches('\r'));
                if len > 0 && out.last().is_none_or(|p: &AbsToken| (p.line, p.col) < (line, col)) {
                    out.push(AbsToken { line, col, len, ty, mods });
                }
            }
            start = line_end + 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------

pub(crate) fn folding_ranges(f: &SourceFile) -> Vec<FoldingRange> {
    let mut diags = Diagnostics::new();
    let (tokens, comments) = lexer::lex_with_comments(&f.text, &mut diags);
    let line = |o: u32| crate::encoding::line_col(f, o).0;
    let mut out = Vec::new();
    let mut stack: Vec<u32> = Vec::new();
    let mut import_run: Option<(u32, u32)> = None;
    let push_run = |out: &mut Vec<FoldingRange>, run: (u32, u32), kind: FoldingRangeKind| {
        if run.1 > run.0 {
            out.push(FoldingRange { start_line: run.0, end_line: run.1, kind: Some(kind), ..Default::default() });
        }
    };
    for t in &tokens {
        match t.kind {
            TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => stack.push(line(t.span.start)),
            TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                if let Some(open) = stack.pop() {
                    let close = line(t.span.start);
                    if close > open + 1 || (close == open + 1 && t.kind != TokenKind::RBrace) {
                        let end = if close > open + 1 { close - 1 } else { close };
                        out.push(FoldingRange { start_line: open, end_line: end, ..Default::default() });
                    }
                }
            }
            TokenKind::Import => {
                let l = line(t.span.start);
                import_run = match import_run {
                    Some((s, e)) if l == e + 1 => Some((s, l)),
                    Some(run) => {
                        push_run(&mut out, run, FoldingRangeKind::Imports);
                        Some((l, l))
                    }
                    None => Some((l, l)),
                };
            }
            _ => {}
        }
    }
    if let Some(run) = import_run {
        push_run(&mut out, run, FoldingRangeKind::Imports);
    }
    let mut comment_run: Option<(u32, u32)> = None;
    for c in comments {
        let (l1, l2) = (line(c.start), line(c.end));
        if l2 > l1 {
            if let Some(run) = comment_run.take() {
                push_run(&mut out, run, FoldingRangeKind::Comment);
            }
            push_run(&mut out, (l1, l2), FoldingRangeKind::Comment);
            continue;
        }
        comment_run = match comment_run {
            Some((s, e)) if l1 == e + 1 => Some((s, l1)),
            Some(run) => {
                push_run(&mut out, run, FoldingRangeKind::Comment);
                Some((l1, l1))
            }
            None => Some((l1, l1)),
        };
    }
    if let Some(run) = comment_run {
        push_run(&mut out, run, FoldingRangeKind::Comment);
    }
    out.sort_by_key(|r| (r.start_line, r.end_line));
    out
}

// ---------------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------------

/// What the expression left of a `.` denotes.
#[derive(Clone, Debug)]
enum Target {
    Ns(String),
    /// A struct, enum or interface used by name (static members, variants).
    Type(SymbolId),
    /// A value of struct type (or pointer to struct).
    StructValue(u32),
    IfaceValue(u32),
    ErrorValue,
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

/// Completion items at `off`: members after `.`, else names in scope and
/// keywords. Unknown left-hand sides give no items rather than noise.
pub(crate) fn completions(a: &Analysis, file: usize, off: u32) -> Vec<CompletionItem> {
    let text = a.files[file].text.as_str();
    let off = (off as usize).min(text.len());
    if !text.is_char_boundary(off) {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let line_start = text[..off].rfind('\n').map_or(0, |i| i + 1);
    if in_comment_or_string(&text[line_start..off]) {
        return Vec::new();
    }
    let mut start = off;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    if start > 0 && bytes[start - 1] == b'.' && !(start >= 2 && bytes[start - 2] == b'.') {
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
        return member_completions(a, file, off as u32, &segs);
    }
    scope_completions(a, file, off as u32)
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
        let e = best.entry(s.name.clone()).or_insert((l.name_span.start, s.id));
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

fn value_target(a: &Analysis, ty: TypeId) -> Option<Target> {
    let tt = &a.hir.types;
    let mut t = tt.get(ty);
    if let Type::Pointer(inner) = t {
        t = tt.get(*inner);
    }
    match t {
        Type::Struct(d) => Some(Target::StructValue(d.0)),
        Type::Interface(d) => Some(Target::IfaceValue(d.0)),
        Type::Error => Some(Target::ErrorValue),
        _ => None,
    }
}

fn symbol_target(a: &Analysis, id: SymbolId) -> Option<Target> {
    let s = a.index.symbol(id);
    match s.kind {
        SymbolKind::Namespace => Some(Target::Ns(s.full_name.clone())),
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
    let Some(target) = target else { return Vec::new() };
    let own_ns = file_ns(a, file).unwrap_or("").to_string();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
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
                if ok && seen.insert(cs.name.clone()) {
                    out.push(item(a, &cs.name, c));
                }
            }
        }
        Target::StructValue(sid) => struct_members(a, sid, &own_ns, &mut out, &mut seen, 0),
        Target::IfaceValue(iid) => {
            if let Some(id) = a.index.lookup(&SymKey::Interface(iid)) {
                for c in a.index.children(id) {
                    out.push(item(a, &a.index.symbol(c).name, c));
                }
            }
        }
        Target::ErrorValue => out.push(plain("Message", CompletionItemKind::METHOD, Some("func Message() string"))),
    }
    out
}

/// Fields and non-static methods of struct `sid`, then promoted members.
fn struct_members(a: &Analysis, sid: u32, own_ns: &str, out: &mut Vec<CompletionItem>, seen: &mut HashSet<String>, depth: usize) {
    let Some(id) = a.index.lookup(&SymKey::Struct(sid)) else { return };
    let s = a.index.symbol(id);
    let ns = s.full_name.strip_suffix(&format!(".{}", s.name)).unwrap_or("");
    for c in a.index.children(id) {
        let cs = a.index.symbol(c);
        let wanted = cs.kind == SymbolKind::Field || (cs.kind == SymbolKind::Method && !cs.is_static);
        if wanted && (ns == own_ns || exported(&cs.name)) && seen.insert(cs.name.clone()) {
            out.push(item(a, &cs.name, c));
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

fn scope_completions(a: &Analysis, file: usize, off: u32) -> Vec<CompletionItem> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (name, id) in visible_locals(a, file, off) {
        if seen.insert(name.clone()) {
            out.push(item(a, &name, id));
        }
    }
    if let Some(Some(info)) = a.index.files.get(file) {
        for (name, id) in a.index.ns_members.get(&info.namespace).into_iter().flatten() {
            if seen.insert(name.clone()) {
                out.push(item(a, name, *id));
            }
        }
        for imp in &info.imports {
            match imp {
                zell::sema::index::ImportInfo::Named(alias, id) => {
                    if seen.insert(alias.clone()) {
                        out.push(item(a, alias, *id));
                    }
                }
                zell::sema::index::ImportInfo::Glob(ns) => {
                    let is_std = zell::sema::stdlib::is_std_namespace(ns);
                    for (name, id) in a.index.ns_members.get(ns).into_iter().flatten() {
                        if (is_std || exported(name)) && seen.insert(name.clone()) {
                            out.push(item(a, name, *id));
                        }
                    }
                }
            }
        }
    }
    for path in a.index.ns_members.keys().filter(|p| !p.contains('.')) {
        if let Some(id) = a.index.lookup(&SymKey::Ns(path.clone())) {
            if seen.insert(path.clone()) {
                out.push(item(a, path, id));
            }
        }
    }
    for (name, sig) in BUILTIN_FUNCS {
        if seen.insert(name.to_string()) {
            out.push(plain(name, CompletionItemKind::FUNCTION, Some(sig)));
        }
    }
    for name in BUILTIN_TYPES {
        if seen.insert(name.to_string()) {
            out.push(plain(name, CompletionItemKind::TYPE_PARAMETER, Some("builtin type")));
        }
    }
    for kw in KEYWORDS {
        if seen.insert(kw.to_string()) {
            out.push(plain(kw, CompletionItemKind::KEYWORD, None));
        }
    }
    out
}
