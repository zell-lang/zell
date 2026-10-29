//! Feature logic on top of an [`Analysis`]: hover text, outline
//! conversion, semantic tokens, folding ranges and completion.

use crate::range;
use lsp_types::{
    FoldingRange, FoldingRangeKind, Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensEdit,
    SemanticTokensLegend,
};
use zell::diagnostics::Diagnostics;
use zell::lexer;
use zell::sema::index::{SymKey, Symbol, SymbolId, SymbolKind};
use zell::session::{Analysis, DocumentSymbol};
use zell::span::{SourceFile, Span};
use zell::token::TokenKind;
use std::collections::HashMap;

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

pub(crate) const KEYWORDS: &[&str] = &[
    "namespace", "import", "as", "struct", "interface", "enum", "func", "static", "async", "await", "var", "const", "if", "else", "for", "in", "switch", "case", "default",
    "return", "break", "continue", "defer", "try", "catch", "new", "nil", "true", "false", "map", "this",
];

pub(crate) const BUILTIN_FUNCS: &[(&str, &str)] = &[
    ("len", "func len(v) int"),
    ("append", "func append(s []T, v T) []T"),
    ("delete", "func delete(m map[K]V, k K)"),
    ("panic", "func panic(msg string)"),
    ("error", "func error(msg string) error"),
];

pub(crate) const BUILTIN_TYPES: &[&str] = &["int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32", "uint64", "byte", "float", "float32", "float64", "bool", "char", "string", "any", "error", "void"];

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
            })
            // `in` of `for x in xs` is a contextual keyword (an identifier
            // token); a variable named `in` resolves above.
            .or_else(|| (name == "in").then_some((T_KEYWORD, 0))),
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
