//! Editing assistance on top of an [`Analysis`] (docs/TOOLING.md, LSP
//! stages 3-5): signature help, inlay hints, quick fixes from structured
//! diagnostics, formatting edits (via `zell::fmt`), workspace symbols and
//! document highlights.

use crate::range;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, DocumentHighlight, DocumentHighlightKind, Documentation, InlayHint, InlayHintKind, InlayHintLabel, Location,
    MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Range, SignatureHelp, SignatureInformation, SymbolInformation, TextEdit, Url,
    WorkspaceEdit,
};
use zell::ast::{self, Expr, ExprKind, Stmt, StmtKind};
use zell::sema::index::{short_type_name, SymbolId, SymbolKind};
use zell::session::Analysis;
use zell::span::{SourceFile, Span};
use zell::types::Type;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// AST walking
// ---------------------------------------------------------------------------

/// Calls `on_stmt` / `on_expr` / `on_block` for every statement,
/// expression and block of a file, outer before inner.
pub(crate) struct Walker<'a, 'f> {
    pub on_stmt: &'f mut dyn FnMut(&'a Stmt),
    pub on_expr: &'f mut dyn FnMut(&'a Expr),
    pub on_block: &'f mut dyn FnMut(&'a ast::Block),
}

impl<'a> Walker<'a, '_> {
    pub fn file(&mut self, f: &'a ast::File) {
        for d in &f.decls {
            match d {
                ast::Decl::Func(func) => self.func(func),
                ast::Decl::Struct(s) => {
                    for m in &s.methods {
                        self.func(m);
                    }
                    for fl in &s.fields {
                        if let Some(e) = &fl.default {
                            self.expr(e);
                        }
                    }
                }
                ast::Decl::Enum(e) => {
                    for m in &e.methods {
                        self.func(m);
                    }
                }
                ast::Decl::Var(v) => {
                    if let Some(e) = &v.init {
                        self.expr(e);
                    }
                }
                ast::Decl::Const(c) => self.expr(&c.value),
                ast::Decl::Interface(_) | ast::Decl::TypeAlias(_) | ast::Decl::Error(_) => {}
            }
        }
    }

    pub fn func(&mut self, f: &'a ast::FuncDecl) {
        if let Some(b) = &f.body {
            self.block(b);
        }
    }

    fn block(&mut self, b: &'a ast::Block) {
        (self.on_block)(b);
        for s in &b.stmts {
            self.stmt(s);
        }
    }

    pub fn stmt(&mut self, s: &'a Stmt) {
        (self.on_stmt)(s);
        match &s.kind {
            StmtKind::Var(v) => {
                if let Some(e) = &v.init {
                    self.expr(e);
                }
            }
            StmtKind::Const(c) => self.expr(&c.value),
            StmtKind::If(i) => self.if_stmt(i),
            StmtKind::For(f) => {
                match &f.kind {
                    ast::ForKind::While(c) => self.expr(c),
                    ast::ForKind::In { iter, .. } => self.expr(iter),
                    ast::ForKind::Infinite => {}
                }
                self.block(&f.body);
            }
            StmtKind::Switch(sw) => {
                if let Some(e) = &sw.subject {
                    self.expr(e);
                }
                for c in &sw.cases {
                    for p in &c.patterns {
                        match &p.kind {
                            ast::PatternKind::Expr(e) => self.expr(e),
                            ast::PatternKind::Range(a, b) => {
                                a.iter().chain(b.iter()).for_each(|e| self.expr(e));
                            }
                            ast::PatternKind::TypeBind { .. } => {}
                        }
                    }
                    c.body.iter().for_each(|s| self.stmt(s));
                }
                sw.default.iter().flatten().for_each(|s| self.stmt(s));
            }
            StmtKind::Return(es) => es.iter().for_each(|e| self.expr(e)),
            StmtKind::Defer(e) | StmtKind::Expr(e) => self.expr(e),
            StmtKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Block(b) => self.block(b),
            StmtKind::TryBlock { body, handler, .. } => {
                self.block(body);
                self.block(handler);
            }
            StmtKind::Break | StmtKind::Continue | StmtKind::Error => {}
        }
    }

    fn if_stmt(&mut self, i: &'a ast::IfStmt) {
        self.expr(&i.cond);
        self.block(&i.then);
        match i.else_.as_deref() {
            Some(ast::ElseBranch::If(e)) => self.if_stmt(e),
            Some(ast::ElseBranch::Block(b)) => self.block(b),
            None => {}
        }
    }

    pub fn expr(&mut self, e: &'a Expr) {
        (self.on_expr)(e);
        match &e.kind {
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Unary { expr, .. } | ExprKind::Await(expr) | ExprKind::New(expr) | ExprKind::Paren(expr) => self.expr(expr),
            ExprKind::TypeArgs { base, .. } => self.expr(base),
            ExprKind::Binary { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            ExprKind::Propagate { expr, context } => {
                self.expr(expr);
                if let Some(c) = context {
                    self.expr(c);
                }
            }
            ExprKind::Try { expr, handler } => {
                self.expr(expr);
                match handler {
                    ast::TryHandler::Catch { body, .. } => self.block(body),
                    ast::TryHandler::Else(v) => match v.as_ref() {
                        ast::ElseValue::Expr(x) => self.expr(x),
                        ast::ElseValue::Return(vs, _) => vs.iter().for_each(|x| self.expr(x)),
                        _ => {}
                    },
                }
            }
            ExprKind::StructLit { fields, .. } => fields.iter().for_each(|f| self.expr(&f.value)),
            ExprKind::SliceLit(es) | ExprKind::Tuple(es) => es.iter().for_each(|x| self.expr(x)),
            ExprKind::MapLit { entries, .. } => entries.iter().for_each(|(k, v)| {
                self.expr(k);
                self.expr(v);
            }),
            ExprKind::Range(a, b) => a.iter().chain(b.iter()).for_each(|x| self.expr(x)),
            ExprKind::Lambda { body, .. } => match body.as_ref() {
                ast::LambdaBody::Block(b) => self.block(b),
                ast::LambdaBody::Expr(x) => self.expr(x),
            },
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

/// Byte ranges of the parameters inside a signature such as
/// `func Money(cents int) string` (`cents int`), split at top-level commas.
fn param_ranges(sig: &str) -> Vec<(usize, usize)> {
    let Some(open) = sig.find('(') else { return Vec::new() };
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = open + 1;
    for (i, c) in sig.char_indices().skip_while(|(i, _)| *i <= open) {
        match c {
            '(' | '[' | '<' => depth += 1,
            ')' | ']' | '>' if depth > 0 => depth -= 1,
            ')' => {
                if sig[start..i].trim().is_empty() {
                    return out;
                }
                out.push(trim_range(sig, start, i));
                return out;
            }
            ',' if depth == 0 => {
                out.push(trim_range(sig, start, i));
                start = i + 1;
            }
            _ => {}
        }
    }
    out
}

fn trim_range(s: &str, a: usize, b: usize) -> (usize, usize) {
    let t = &s[a..b];
    let lead = t.len() - t.trim_start().len();
    let trail = t.len() - t.trim_end().len();
    (a + lead, b - trail)
}

/// The parameter names of a function symbol's signature.
fn param_names(sig: &str) -> Vec<String> {
    param_ranges(sig).into_iter().map(|(a, b)| sig[a..b].split_whitespace().next().unwrap_or("").to_string()).collect()
}

fn label_len(s: &str) -> u32 {
    crate::encoding::len(s)
}

/// The innermost unclosed `(` before `off` and the number of top-level
/// commas after it (a rough lexical scan that skips strings and comments
/// on the way).
fn open_call(text: &str, off: usize) -> Option<(usize, u32)> {
    let bytes = text.as_bytes();
    // Mask out strings, chars and comments first (forward scan).
    let mut code = vec![true; off];
    let mut i = 0;
    while i < off {
        match bytes[i] {
            b'"' | b'\'' => {
                let q = bytes[i];
                code[i] = false;
                i += 1;
                while i < off && bytes[i] != q && bytes[i] != b'\n' {
                    if bytes[i] == b'\\' && i + 1 < off {
                        code[i] = false;
                        i += 1;
                    }
                    code[i] = false;
                    i += 1;
                }
                if i < off {
                    code[i] = false;
                }
                i += 1;
            }
            b'/' if i + 1 < off && bytes[i + 1] == b'/' => {
                while i < off && bytes[i] != b'\n' {
                    code[i] = false;
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    let mut depth = 0i32;
    let mut commas = 0u32;
    let lower = off.saturating_sub(4000);
    let mut j = off;
    while j > lower {
        j -= 1;
        if !code[j] {
            continue;
        }
        match bytes[j] {
            b')' | b']' | b'}' => depth += 1,
            b'[' | b'{' if depth > 0 => depth -= 1,
            b'{' => return None,
            b'[' => {
                // Inside a slice literal or index, not call arguments.
                return None;
            }
            b'(' if depth > 0 => depth -= 1,
            b'(' => return Some((j, commas)),
            b',' if depth == 0 => commas += 1,
            b';' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Signature help at `off`: the call whose argument list contains it.
pub(crate) fn signature_help(a: &Analysis, file: usize, off: u32) -> Option<SignatureHelp> {
    let text = a.files[file].text.as_str();
    let off = (off as usize).min(text.len());
    let (paren, commas) = open_call(text, off)?;
    let bytes = text.as_bytes();
    // The callee name right before `(` (skipping explicit type arguments).
    let mut end = paren;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end > 0 && bytes[end - 1] == b'>' {
        let mut depth = 0;
        while end > 0 {
            end -= 1;
            match bytes[end] {
                b'>' => depth += 1,
                b'<' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    let mut start = end;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    if start == end {
        return None;
    }
    let name = &text[start..end];
    let id = a
        .index
        .reference_at(file, start as u32)
        .map(|r| r.symbol)
        .filter(|id| a.index.symbol(*id).name == name)
        .or_else(|| crate::features::resolve_name(a, file, start as u32, name))?;
    let sym = a.index.symbol(id);
    if !matches!(sym.kind, SymbolKind::Func | SymbolKind::Method | SymbolKind::BuiltinFunc) {
        return None;
    }
    let label = sym.signature.lines().next().unwrap_or("").to_string();
    let ranges = param_ranges(&label);
    let parameters: Vec<ParameterInformation> = ranges
        .iter()
        .map(|&(s, e)| ParameterInformation { label: ParameterLabel::LabelOffsets([label_len(&label[..s]), label_len(&label[..e])]), documentation: None })
        .collect();
    let variadic = ranges.last().is_some_and(|&(s, e)| label[s..e].contains("..."));
    let mut active = commas;
    if variadic && !ranges.is_empty() {
        active = active.min(ranges.len() as u32 - 1);
    }
    let documentation = sym.doc.as_ref().map(|d| Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value: d.clone() }));
    Some(SignatureHelp {
        signatures: vec![SignatureInformation { label, documentation, parameters: Some(parameters), active_parameter: Some(active) }],
        active_signature: Some(0),
        active_parameter: Some(active),
    })
}

// ---------------------------------------------------------------------------
// Inlay hints
// ---------------------------------------------------------------------------

/// Which inlay hints to show (`zell.inlayHints.*` settings).
#[derive(Clone, Copy, Debug)]
pub struct HintSettings {
    pub types: bool,
    pub parameter_names: bool,
}

impl Default for HintSettings {
    fn default() -> Self {
        HintSettings { types: true, parameter_names: true }
    }
}

impl HintSettings {
    /// Reads `{"inlayHints": {"types": {"enabled": b}, "parameterNames":
    /// {"enabled": b}}}` (also accepting plain booleans); missing keys keep
    /// their current value.
    pub fn update(&mut self, v: &serde_json::Value) {
        let Some(h) = v.get("inlayHints") else { return };
        let flag = |x: Option<&serde_json::Value>| x.and_then(|x| x.as_bool().or_else(|| x.get("enabled").and_then(|e| e.as_bool())));
        if let Some(b) = flag(h.get("types")) {
            self.types = b;
        }
        if let Some(b) = flag(h.get("parameterNames")) {
            self.parameter_names = b;
        }
    }
}

fn is_literal_arg(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Char(_) | ExprKind::Bool(_) | ExprKind::Nil => true,
        ExprKind::Unary { op: ast::UnaryOp::Neg, expr } => matches!(expr.kind, ExprKind::Int(_) | ExprKind::Float(_)),
        _ => false,
    }
}

/// The span of the name a callee expression ends with.
fn callee_name(e: &Expr) -> Option<&ast::Ident> {
    match &e.kind {
        ExprKind::Ident(i) => Some(i),
        ExprKind::Field { name, .. } => Some(name),
        ExprKind::TypeArgs { base, .. } | ExprKind::Paren(base) => callee_name(base),
        _ => None,
    }
}

/// Inlay hints inside `within`: inferred types after `var x = ...` (and
/// `for x in ...`) names, parameter names before literal arguments.
pub(crate) fn inlay_hints(a: &Analysis, file: usize, within: Span, settings: HintSettings) -> Vec<InlayHint> {
    let f = &a.files[file];
    let Some(ast) = a.asts.get(file) else { return Vec::new() };
    let mut type_spots: Vec<&ast::Ident> = Vec::new();
    let mut calls: Vec<(&ast::Ident, Vec<&Expr>)> = Vec::new();
    let mut stmts: Vec<&Stmt> = Vec::new();
    let mut exprs: Vec<&Expr> = Vec::new();
    Walker { on_stmt: &mut |s| stmts.push(s), on_expr: &mut |e| exprs.push(e), on_block: &mut |_| {} }.file(ast);
    for s in &stmts {
        match &s.kind {
            StmtKind::Var(v) if v.ty.is_none() && v.init.is_some() => type_spots.extend(v.names.iter()),
            StmtKind::For(fs) => {
                if let ast::ForKind::In { vars, .. } = &fs.kind {
                    type_spots.extend(vars.iter());
                }
            }
            _ => {}
        }
    }
    for d in &ast.decls {
        if let ast::Decl::Var(v) = d {
            if v.ty.is_none() && v.init.is_some() {
                type_spots.extend(v.names.iter());
            }
        }
    }
    for e in &exprs {
        if let ExprKind::Call { callee, args } = &e.kind {
            if args.iter().any(is_literal_arg) {
                if let Some(n) = callee_name(callee) {
                    calls.push((n, args.iter().collect()));
                }
            }
        }
    }
    let inside = |s: Span| s.start >= within.start && s.end <= within.end;
    let pos = |o: u32| crate::encoding::position(f, o);
    let mut out = Vec::new();
    if settings.types {
        for id in type_spots {
            if id.name == "_" || id.name.is_empty() || !inside(id.span) {
                continue;
            }
            let Some(sym) = a.index.declaration_at(file, id.span.start).map(|s| a.index.symbol(s)) else { continue };
            let Some(ty) = sym.ty else { continue };
            if matches!(a.hir.types.get(ty), Type::Invalid) {
                continue;
            }
            let mut data = crate::symbol_data(a, sym.id);
            data["kind"] = "type".into();
            out.push(InlayHint {
                position: pos(id.span.end),
                label: InlayHintLabel::String(short_type_name(&a.hir, ty)),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: None,
                data: Some(data),
            });
        }
    }
    if settings.parameter_names {
        for (name, args) in calls {
            let Some(r) = a.index.reference_at(file, name.span.start).filter(|r| r.span == name.span) else { continue };
            let sym = a.index.symbol(r.symbol);
            if !matches!(sym.kind, SymbolKind::Func | SymbolKind::Method | SymbolKind::BuiltinFunc) {
                continue;
            }
            let sig = sym.signature.lines().next().unwrap_or("");
            let names = param_names(sig);
            if names.len() != args.len() || sig.contains("...") {
                continue;
            }
            for (arg, pname) in args.iter().zip(names) {
                if !is_literal_arg(arg) || pname.is_empty() || !inside(arg.span) {
                    continue;
                }
                let mut data = crate::symbol_data(a, r.symbol);
                data["kind"] = "parameter".into();
                out.push(InlayHint {
                    position: pos(arg.span.start),
                    label: InlayHintLabel::String(format!("{pname}:")),
                    kind: Some(InlayHintKind::PARAMETER),
                    text_edits: None,
                    tooltip: None,
                    padding_left: None,
                    padding_right: Some(true),
                    data: Some(data),
                });
            }
        }
    }
    out.sort_by_key(|h| (h.position.line, h.position.character));
    out
}

/// `inlayHint/resolve`: a Markdown tooltip. For a type hint, the
/// variable's signature and the documentation of its type's declaration;
/// for a parameter name, the called function's signature and doc.
pub(crate) fn resolve_inlay_hint(a: &Analysis, version: u64, mut hint: InlayHint) -> InlayHint {
    let Some(data) = hint.data.as_ref() else { return hint };
    let Some(id) = crate::symbol_from_data(a, version, data) else { return hint };
    let mut value = crate::features::hover_markdown(a, id);
    if data.get("kind").and_then(|k| k.as_str()) == Some("type") {
        if let Some(decl) = a.index.symbol(id).ty.and_then(|t| type_symbol(a, t)) {
            value.push_str("\n---\n\n");
            value.push_str(&crate::features::hover_markdown(a, decl));
        }
    }
    hint.tooltip = Some(lsp_types::InlayHintTooltip::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value }));
    hint
}

/// The index symbol of the struct, enum or interface behind type `t`
/// (through pointers, slices, fallible results and tasks).
fn type_symbol(a: &Analysis, mut t: zell::types::TypeId) -> Option<SymbolId> {
    use zell::sema::index::SymKey;
    for _ in 0..16 {
        t = match a.hir.types.get(t) {
            Type::Pointer(x) | Type::Weak(x) | Type::Slice(x) | Type::Array(x, _) | Type::Fallible(x) | Type::Task(x) => *x,
            Type::Struct(d) => return a.index.lookup(&SymKey::Struct(d.0)),
            Type::Enum(d) => return a.index.lookup(&SymKey::Enum(d.0)),
            Type::Interface(d) => return a.index.lookup(&SymKey::Interface(d.0)),
            _ => return None,
        };
    }
    None
}

// ---------------------------------------------------------------------------
// Code actions, formatting, symbols, highlights
// ---------------------------------------------------------------------------

fn spans_touch(a: Span, b: Span) -> bool {
    a.start <= b.end && b.start <= a.end
}

/// Quick fixes for the diagnostics of `file` overlapping `span`.
pub(crate) fn code_actions(a: &Analysis, file: usize, uri: &Url, span: Span, to_lsp: impl Fn(&zell::diagnostics::Diagnostic) -> lsp_types::Diagnostic) -> Vec<CodeActionOrCommand> {
    let f = &a.files[file];
    let mut out = Vec::new();
    for d in a.diagnostics_for(file).filter(|d| spans_touch(d.span, span)) {
        for (i, fix) in d.fixes.iter().enumerate() {
            let edits: Vec<TextEdit> = fix.edits.iter().map(|e| TextEdit { range: range(f, e.span), new_text: e.new_text.clone() }).collect();
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), edits);
            out.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: fix.title.clone(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![to_lsp(d)]),
                edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                is_preferred: Some(i == 0),
                ..Default::default()
            }));
        }
    }
    out
}

/// Text edits turning `file`'s text into its formatted form, limited to
/// hunks touching lines `lines` (inclusive) when given. `None` when the
/// file does not parse.
pub(crate) fn format_edits(f: &SourceFile, lines: Option<(u32, u32)>) -> Option<Vec<TextEdit>> {
    let formatted = zell::fmt::format_source(&f.text).ok()?;
    let mut out = Vec::new();
    for h in zell::fmt::line_hunks(&f.text, &formatted) {
        if let Some((from, to)) = lines {
            let (hs, he) = (h.old_start as u32, (h.old_start + h.old_len) as u32);
            let touches = if h.old_len == 0 { hs >= from && hs <= to + 1 } else { hs <= to && he > from };
            if !touches {
                continue;
            }
        }
        let start = f.line_start(h.old_start);
        let end = f.line_start(h.old_start + h.old_len);
        out.push(TextEdit { range: range(f, Span::new(start, end.max(start))), new_text: h.new_lines.concat() });
    }
    Some(out)
}

fn workspace_kind(k: SymbolKind) -> bool {
    !matches!(k, SymbolKind::Local | SymbolKind::Param | SymbolKind::BuiltinFunc | SymbolKind::BuiltinType | SymbolKind::Namespace)
}

/// Case-insensitive subsequence match (`usrnew` matches `UserNew`).
fn fuzzy(query: &str, name: &str) -> bool {
    let mut cs = name.chars().flat_map(|c| c.to_lowercase());
    query.chars().flat_map(|c| c.to_lowercase()).all(|q| cs.any(|c| c == q))
}

/// Declarations of the workspace matching `query`.
#[allow(deprecated)]
pub(crate) fn workspace_symbols(a: &Analysis, query: &str, uri_of: impl Fn(usize) -> Option<Url>) -> Vec<SymbolInformation> {
    let mut out: Vec<(usize, SymbolInformation)> = Vec::new();
    for s in &a.index.symbols {
        let Some(loc) = s.loc else { continue };
        if !workspace_kind(s.kind) || matches!(s.key, zell::sema::index::SymKey::LocalConst(..)) || !fuzzy(query, &s.name) {
            continue;
        }
        let Some(uri) = uri_of(loc.file) else { continue };
        let f = &a.files[loc.file];
        let container_name = s.container.map(|c| a.index.symbol(c).full_name.clone());
        let exact = if s.name.eq_ignore_ascii_case(query) { 0 } else if s.name.to_lowercase().starts_with(&query.to_lowercase()) { 1 } else { 2 };
        // Project symbols before library (std, dependency) ones.
        let exact = exact * 2 + usize::from(a.is_read_only(loc.file));
        out.push((
            exact,
            SymbolInformation {
                name: s.name.clone(),
                kind: crate::features::lsp_kind(s.kind),
                tags: None,
                deprecated: None,
                location: Location { uri, range: range(f, loc.name_span) },
                container_name,
            },
        ));
    }
    out.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.name.cmp(&y.1.name)));
    out.into_iter().take(500).map(|x| x.1).collect()
}

/// Occurrences of the symbol at `off` in `file`: the declaration (write)
/// and the uses (read).
pub(crate) fn highlights(a: &Analysis, file: usize, off: u32) -> Option<Vec<DocumentHighlight>> {
    let id: SymbolId = a.symbol_at(file, off)?;
    let f = &a.files[file];
    let mut out = Vec::new();
    if let Some(l) = a.index.symbol(id).loc.filter(|l| l.file == file) {
        out.push(DocumentHighlight { range: range(f, l.name_span), kind: Some(DocumentHighlightKind::WRITE) });
    }
    for r in a.references_of(id).into_iter().filter(|r| r.file == file) {
        out.push(DocumentHighlight { range: range(f, r.span), kind: Some(DocumentHighlightKind::READ) });
    }
    out.sort_by_key(|h| (h.range.start.line, h.range.start.character));
    out.dedup_by_key(|h| h.range);
    Some(out)
}

/// Byte span of an LSP range.
pub(crate) fn span_of(f: &SourceFile, r: &Range) -> Span {
    let s = crate::encoding::offset(f, r.start);
    let e = crate::encoding::offset(f, r.end);
    Span::new(s.min(e), e.max(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_of_signatures() {
        let sig = "func Money(cents int, f func(int, string) bool, m map[string]int) string";
        let names = param_names(sig);
        assert_eq!(names, ["cents", "f", "m"]);
        assert!(param_ranges("func F()").is_empty());
        let r = param_ranges("func F(a int)");
        assert_eq!(&"func F(a int)"[r[0].0..r[0].1], "a int");
    }

    #[test]
    fn open_call_counts_commas() {
        let t = "f(a, g(b, c), \"x,y\", ";
        assert_eq!(open_call(t, t.len()), Some((1, 3)));
        let t = "f(a, [1, 2";
        assert_eq!(open_call(t, t.len()), None);
        let t = "x = 1; y(";
        assert_eq!(open_call(t, t.len()), Some((8, 0)));
    }

    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy("usr", "User"));
        assert!(fuzzy("", "Anything"));
        assert!(!fuzzy("xyz", "User"));
    }
}
