//! AST -> [`Doc`] for the formatter, with comments and blank lines taken
//! from the source by position (see the module docs of `fmt`).

use super::doc::*;
use super::Comment;
use crate::ast::*;
use crate::lexer::{Lexed, TriviaKind};
use crate::span::Span;
use crate::token::TokenKind;

pub(crate) struct Fmt<'a> {
    src: &'a str,
    lexed: &'a Lexed,
    comments: Vec<Comment>,
    /// First comment not yet emitted.
    next: usize,
}

/// Separator style of a list.
#[derive(Clone, Copy, PartialEq)]
enum Sep {
    /// `a, b` / one per line with a trailing comma.
    Comma,
    /// Enum variants: `{ A, B }` / one per line without commas.
    Variants,
}

struct ListOpts {
    open: &'static str,
    close: &'static str,
    /// Position of the closing delimiter (comments before it belong inside).
    close_pos: u32,
    force: bool,
    sep: Sep,
    /// `{ a, b }` rather than `{a, b}` when flat.
    pad: bool,
}

/// A struct or interface member, in source order.
enum Member<'x> {
    /// Fields declared together (`X, Y float`).
    Fields(Vec<&'x Field>),
    Embed(&'x TypeExpr),
    Method(&'x FuncDecl),
}

impl Member<'_> {
    fn start(&self) -> u32 {
        match self {
            Member::Fields(fs) => fs[0].attrs.first().map_or(fs[0].name.span.start, |a| a.span.start),
            Member::Embed(t) => t.span.start,
            Member::Method(f) => f.span.start,
        }
    }

    fn end(&self) -> u32 {
        match self {
            Member::Fields(fs) => fs[0].span.end,
            Member::Embed(t) => t.span.end,
            Member::Method(f) => f.span.end,
        }
    }
}

fn binop(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Rem => "%",
        MatMul => "@",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        And => "&&",
        Or => "||",
    }
}

fn assignop(op: AssignOp) -> &'static str {
    use AssignOp::*;
    match op {
        Assign => "=",
        Add => "+=",
        Sub => "-=",
        Mul => "*=",
        Div => "/=",
        Rem => "%=",
        MatMul => "@=",
        BitAnd => "&=",
        BitOr => "|=",
        BitXor => "^=",
        Shl => "<<=",
        Shr => ">>=",
    }
}

fn is_literal(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::StructLit { .. } | ExprKind::MapLit { .. } | ExprKind::SliceLit(_))
}

impl<'a> Fmt<'a> {
    pub(crate) fn new(src: &'a str, lexed: &'a Lexed, comments: Vec<Comment>) -> Fmt<'a> {
        Fmt { src, lexed, comments, next: 0 }
    }

    // =====================================================================
    // Source queries
    // =====================================================================

    fn slice(&self, s: Span) -> &'a str {
        &self.src[s.start as usize..(s.end as usize).min(self.src.len())]
    }

    fn newline_between(&self, a: u32, b: u32) -> bool {
        a < b && self.src[a as usize..b as usize].contains('\n')
    }

    /// Start of the first real token at or after `from` with kind `kind`.
    fn tok_after(&self, from: u32, kind: &TokenKind) -> Option<u32> {
        let toks = &self.lexed.tokens;
        (self.lexed.token_at(from)..toks.len()).find(|&i| toks[i].kind == *kind && toks[i].span.start < toks[i].span.end).map(|i| toks[i].span.start)
    }

    /// Start of the first real token at or after `from`.
    fn next_tok(&self, from: u32) -> u32 {
        let toks = &self.lexed.tokens;
        (self.lexed.token_at(from)..toks.len()).find(|&i| toks[i].span.start < toks[i].span.end || toks[i].kind == TokenKind::Eof).map_or(self.src.len() as u32, |i| toks[i].span.start)
    }

    /// Position of the first token at or after `pos` (skipping whitespace,
    /// comments).
    fn skip_trivia(&self, pos: u32) -> u32 {
        let toks = &self.lexed.tokens;
        (self.lexed.token_at(pos)..toks.len())
            .find(|&i| toks[i].span.start < toks[i].span.end)
            .map_or(self.src.len() as u32, |i| toks[i].span.start)
    }

    /// The source has a blank line right before the token at `pos`.
    fn blank_before(&self, pos: u32) -> bool {
        let i = self.lexed.token_at(pos);
        if self.lexed.tokens.get(i).is_none_or(|t| t.span.start != pos) {
            return false;
        }
        matches!(self.lexed.leading(i).last().map(|t| t.kind), Some(TriviaKind::Newlines(n)) if n >= 2)
    }

    /// Is there an unemitted comment starting before `pos`?
    fn pending_before(&self, pos: u32) -> bool {
        self.comments.get(self.next).is_some_and(|c| c.start < pos)
    }

    /// Takes the unemitted comments starting before `pos`.
    fn take_before(&mut self, pos: u32) -> Vec<Comment> {
        let mut out = Vec::new();
        while let Some(c) = self.comments.get(self.next) {
            if c.start >= pos {
                break;
            }
            out.push(c.clone());
            self.next += 1;
        }
        out
    }

    /// The comment on the same line right after `end` (only punctuation in
    /// between), if it starts before `limit`.
    fn take_trailing(&mut self, end: u32, limit: u32) -> Option<String> {
        let c = self.comments.get(self.next)?;
        if c.start < end || c.start >= limit {
            return None;
        }
        let between = &self.src[end as usize..c.start as usize];
        if !between.chars().all(|ch| matches!(ch, ' ' | '\t' | ',' | ';' | ':' | '{' | '}' | '(' | ')' | '[' | ']')) {
            return None;
        }
        self.next += 1;
        Some(c.text.clone())
    }

    fn trailing(&mut self, end: u32) -> Doc {
        self.take_trailing(end, u32::MAX).map_or(Doc::Nil, Doc::Suffix)
    }

    /// Comments before `pos`, each on its own line and followed by a
    /// newline; then a blank line if the source has one before `pos`. At
    /// the first item of a body (`first`) no blank line opens it.
    fn prefix(&mut self, pos: u32, first: bool) -> Doc {
        let cs = self.take_before(pos);
        let mut out = Vec::new();
        for (i, c) in cs.iter().enumerate() {
            if c.blank_before && !(first && i == 0) {
                out.push(Doc::Hard);
            }
            out.push(text(c.text.clone()));
            out.push(Doc::Hard);
        }
        if self.blank_before(pos) && !(first && cs.is_empty()) {
            out.push(Doc::Hard);
        }
        concat(out)
    }

    /// Comments before `pos` at the end of a body, each preceded by a
    /// newline (and a blank line where the source has one, except right
    /// after the opening of an empty body).
    fn dangling(&mut self, pos: u32, after_items: bool) -> Doc {
        let cs = self.take_before(pos);
        let mut out = Vec::new();
        for (i, c) in cs.iter().enumerate() {
            out.push(Doc::Hard);
            if c.blank_before && (i > 0 || after_items) {
                out.push(Doc::Hard);
            }
            out.push(text(c.text.clone()));
        }
        concat(out)
    }

    // =====================================================================
    // File
    // =====================================================================

    pub(crate) fn file(&mut self, f: &File) -> Doc {
        let mut parts = vec![self.prefix(f.span.start, true)];
        let ns_kw = self.tok_after(f.attrs.last().map_or(f.span.start, |a| a.span.end), &TokenKind::Namespace).unwrap_or(f.namespace.span.start);
        parts.push(self.attrs(&f.attrs, ns_kw));
        parts.push(text(format!("namespace {}", f.namespace.to_string())));
        parts.push(self.trailing(f.namespace.span.end));
        if !f.imports.is_empty() {
            parts.push(Doc::Hard);
            parts.push(Doc::Hard);
            parts.push(self.imports(&f.imports));
        }
        for (i, d) in f.decls.iter().enumerate() {
            parts.push(Doc::Hard);
            let start = d.span().start;
            if i == 0 {
                parts.push(Doc::Hard);
            }
            parts.push(self.prefix(start, i == 0));
            parts.push(self.decl(d));
            parts.push(self.trailing(d.span().end));
        }
        parts.push(self.dangling(u32::MAX, true));
        concat(parts)
    }

    /// Imports sorted: standard library namespaces first, then the rest, one
    /// blank line between the groups; comments move with their import.
    fn imports(&mut self, imports: &[Import]) -> Doc {
        let mut items: Vec<(bool, String, Doc)> = Vec::new();
        for imp in imports {
            let pre = self.prefix(imp.span.start, true);
            let path = imp.path.to_string();
            let mut line = format!("import {path}");
            if let Some(a) = &imp.alias {
                line.push_str(&format!(" as {}", a.name));
            }
            let trail = self.trailing(imp.span.end);
            let is_std = crate::sema::stdlib::is_std_namespace(&path);
            let key = format!("{path} {}", imp.alias.as_ref().map_or("", |a| a.name.as_str()));
            items.push((!is_std, key, concat(vec![pre, text(line), trail])));
        }
        items.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut out = Vec::new();
        let mut prev: Option<bool> = None;
        for (group, _, d) in items {
            if let Some(g) = prev {
                out.push(Doc::Hard);
                if g != group {
                    out.push(Doc::Hard);
                }
            }
            out.push(d);
            prev = Some(group);
        }
        concat(out)
    }

    fn decl(&mut self, d: &Decl) -> Doc {
        match d {
            Decl::Struct(s) => self.struct_decl(s),
            Decl::Interface(i) => self.interface_decl(i),
            Decl::Enum(e) => self.enum_decl(e),
            Decl::Func(f) => self.func(f),
            Decl::Var(v) => self.var(v),
            Decl::Const(c) => self.const_(c),
            Decl::Error(s) => text(self.slice(*s)),
        }
    }

    // =====================================================================
    // Attributes
    // =====================================================================

    fn attr(&mut self, a: &Attribute) -> Doc {
        let mut parts = vec![text(format!("@{}", a.path.to_string()))];
        if !a.args.is_empty() {
            parts.push(text("("));
            for (i, arg) in a.args.iter().enumerate() {
                if i > 0 {
                    parts.push(text(", "));
                }
                if let Some(n) = &arg.name {
                    parts.push(text(format!("{}: ", n.name)));
                }
                parts.push(self.expr(&arg.value));
            }
            parts.push(text(")"));
        }
        concat(parts)
    }

    /// Attributes before an item starting at `next`: each on its own line
    /// unless it was written on the same line as what follows it.
    fn attrs(&mut self, attrs: &[Attribute], next: u32) -> Doc {
        let mut parts = Vec::new();
        for (i, a) in attrs.iter().enumerate() {
            let follow = attrs.get(i + 1).map_or(next, |b| b.span.start);
            parts.push(self.attr(a));
            if self.newline_between(a.span.end, follow) {
                parts.push(self.trailing(a.span.end));
                parts.push(Doc::Hard);
                parts.push(self.prefix(follow, true));
            } else {
                parts.push(text(" "));
            }
        }
        concat(parts)
    }

    fn attrs_inline(&self, attrs: &[Attribute], next: u32) -> bool {
        attrs.last().is_some_and(|a| !self.newline_between(a.span.end, next))
    }

    // =====================================================================
    // Declarations
    // =====================================================================

    fn generics(&mut self, gs: &[GenericParam]) -> Doc {
        if gs.is_empty() {
            return Doc::Nil;
        }
        let mut parts = vec![text("<")];
        for (i, g) in gs.iter().enumerate() {
            if i > 0 {
                parts.push(text(", "));
            }
            parts.push(text(g.name.name.clone()));
            if let Some(b) = &g.bound {
                parts.push(text(": "));
                parts.push(self.ty(b));
            }
        }
        parts.push(text(">"));
        concat(parts)
    }

    /// Start of the item after its attributes (modifiers or keyword).
    fn after_attrs(&self, attrs: &[Attribute], start: u32) -> u32 {
        attrs.last().map_or(start, |a| self.next_tok(a.span.end))
    }

    fn func(&mut self, f: &FuncDecl) -> Doc {
        let kw = self.after_attrs(&f.attrs, f.span.start);
        let mut parts = vec![self.attrs(&f.attrs, kw)];
        if f.is_extern {
            parts.push(text("extern "));
        }
        if f.is_static {
            parts.push(text("static "));
        }
        if f.is_async {
            parts.push(text("async "));
        }
        parts.push(text(format!("func {}", f.name.name)));
        parts.push(self.generics(&f.generics));
        let open = self.tok_after(f.generics.last().map_or(f.name.span.end, |g| g.span.end), &TokenKind::LParen).unwrap_or(f.name.span.end);
        parts.push(self.params(&f.params, open));
        if let Some(r) = &f.ret {
            parts.push(text(" "));
            parts.push(self.ty(r));
        }
        if let Some(b) = &f.body {
            parts.push(text(" "));
            parts.push(self.block(b));
        }
        concat(parts)
    }

    /// `(a, b int, c string)`: names sharing a type are kept together.
    fn params(&mut self, ps: &[Param], open: u32) -> Doc {
        let mut groups: Vec<Vec<&Param>> = Vec::new();
        for p in ps {
            match groups.last_mut() {
                Some(g) if g[0].ty.span == p.ty.span && p.attrs.is_empty() => g.push(p),
                _ => groups.push(vec![p]),
            }
        }
        let close = self.tok_after(ps.last().map_or(open + 1, |p| p.span.end), &TokenKind::RParen).unwrap_or(open + 1);
        let opts = ListOpts { open: "(", close: ")", close_pos: close, force: false, sep: Sep::Comma, pad: false };
        self.list(&groups, |g| Span::new(g[0].span.start, g.last().unwrap().span.end), |f, g| f.param_group(g), opts)
    }

    fn param_group(&mut self, g: &[&Param]) -> Doc {
        let first = g[0];
        let mut parts = vec![self.attrs(&first.attrs, first.name.span.start)];
        let names: Vec<&str> = g.iter().map(|p| p.name.name.as_str()).collect();
        if matches!(first.ty.kind, TypeKind::Infer) {
            parts.push(text(names.join(", ")));
            return concat(parts);
        }
        parts.push(text(format!("{} ", names.join(", "))));
        parts.push(self.ty(&first.ty));
        concat(parts)
    }

    fn var(&mut self, v: &VarDecl) -> Doc {
        let kw = self.after_attrs(&v.attrs, v.span.start);
        let mut parts = vec![self.attrs(&v.attrs, kw), text("var ")];
        if v.names.len() == 1 && !self.slice(Span::new(kw, v.names[0].span.start)).contains('(') {
            parts.push(text(v.names[0].name.clone()));
        } else {
            let names: Vec<&str> = v.names.iter().map(|n| n.name.as_str()).collect();
            parts.push(text(format!("({})", names.join(", "))));
        }
        if let Some(t) = &v.ty {
            parts.push(text(" "));
            parts.push(self.ty(t));
        }
        if let Some(e) = &v.init {
            parts.push(text(" = "));
            parts.push(self.expr(e));
        }
        concat(parts)
    }

    fn const_(&mut self, c: &ConstDecl) -> Doc {
        let kw = self.after_attrs(&c.attrs, c.span.start);
        let mut parts = vec![self.attrs(&c.attrs, kw), text(format!("const {}", c.name.name))];
        if let Some(t) = &c.ty {
            parts.push(text(" "));
            parts.push(self.ty(t));
        }
        parts.push(text(" = "));
        parts.push(self.expr(&c.value));
        concat(parts)
    }

    /// `name<G> {` for structs, interfaces and enums; returns the doc and
    /// the position of `{`.
    fn type_header(&mut self, kw: &str, attrs: &[Attribute], start: u32, name: &Ident, gs: &[GenericParam], implements: &[TypeExpr]) -> (Doc, u32) {
        let at = self.after_attrs(attrs, start);
        let mut parts = vec![self.attrs(attrs, at), text(format!("{kw} {}", name.name)), self.generics(gs)];
        for (i, t) in implements.iter().enumerate() {
            parts.push(text(if i == 0 { " implements " } else { ", " }));
            parts.push(self.ty(t));
        }
        parts.push(text(" {"));
        let head = concat(parts);
        let after = implements.last().map(|t| t.span.end).or(gs.last().map(|g| g.span.end)).unwrap_or(name.span.end);
        let brace = self.tok_after(after, &TokenKind::LBrace).unwrap_or(name.span.end);
        (head, brace)
    }

    fn members<'x>(fields: &'x [Field], embeds: &'x [TypeExpr], methods: &'x [FuncDecl]) -> Vec<Member<'x>> {
        let mut out: Vec<Member<'x>> = Vec::new();
        for f in fields {
            if let Some(Member::Fields(g)) = out.last_mut() {
                if g[0].span.end == f.span.end && g[0].ty.span == f.ty.span {
                    g.push(f);
                    continue;
                }
            }
            out.push(Member::Fields(vec![f]));
        }
        out.extend(embeds.iter().map(Member::Embed));
        out.extend(methods.iter().map(Member::Method));
        out.sort_by_key(|m| m.start());
        out
    }

    fn struct_decl(&mut self, s: &StructDecl) -> Doc {
        let (head, brace) = self.type_header("struct", &s.attrs, s.span.start, &s.name, &s.generics, &s.implements);
        let members = Self::members(&s.fields, &s.embeds, &s.methods);
        self.body(head, brace, s.span.end - 1, &members)
    }

    fn interface_decl(&mut self, i: &InterfaceDecl) -> Doc {
        let (head, brace) = self.type_header(if i.is_sealed { "sealed interface" } else { "interface" }, &i.attrs, i.span.start, &i.name, &i.generics, &[]);
        let members = Self::members(&[], &i.embeds, &i.methods);
        self.body(head, brace, i.span.end - 1, &members)
    }

    /// `head` + members + `}`; consecutive single-line fields are aligned
    /// in columns (names, types, trailing comments), like gofmt.
    fn body(&mut self, head: Doc, brace: u32, close: u32, members: &[Member]) -> Doc {
        if members.is_empty() && !self.pending_before(close) {
            return concat(vec![head, text("}")]);
        }
        let mut inner = vec![self.trailing(brace + 1)];
        let mut i = 0;
        while i < members.len() {
            inner.push(Doc::Hard);
            inner.push(self.prefix(members[i].start(), i == 0));
            match &members[i] {
                Member::Fields(_) => {
                    // A run of aligned fields: no blank line, comment or
                    // own-line attribute between them.
                    let mut rows: Vec<Row> = Vec::new();
                    let mut j = i;
                    while let Member::Fields(g) = &members[j] {
                        if j > i && (self.blank_before(members[j].start()) || self.pending_before(members[j].start())) {
                            break;
                        }
                        let f0 = g[0];
                        let inline = f0.attrs.is_empty() || self.attrs_inline(&f0.attrs, f0.name.span.start);
                        if j > i && (!inline || f0.attrs.is_empty() != rows[0].attrs.is_empty()) {
                            break;
                        }
                        let (attrs, col1, col2) = self.field_cols(g, inline);
                        if j > i && (col1.contains('\n') || col2.contains('\n')) {
                            break;
                        }
                        if !inline {
                            // Own-line attributes print above the row.
                            let ad = self.attrs(&f0.attrs, f0.name.span.start);
                            inner.push(ad);
                        }
                        let trail = self.take_trailing(members[j].end(), u32::MAX);
                        rows.push(Row { attrs, names: col1, ty: col2, comment: trail });
                        j += 1;
                        if j >= members.len() || col1_multiline(&rows) {
                            break;
                        }
                    }
                    inner.push(align_rows(&rows));
                    i = j;
                    continue;
                }
                Member::Embed(t) => {
                    inner.push(self.ty(t));
                    inner.push(self.trailing(t.span.end));
                }
                Member::Method(f) => {
                    inner.push(self.func(f));
                    inner.push(self.trailing(f.span.end));
                }
            }
            i += 1;
        }
        inner.push(self.dangling(close, !members.is_empty()));
        concat(vec![head, indent(concat(inner)), Doc::Hard, text("}")])
    }

    /// The columns of a field row: inline attributes, names and
    /// `type [= default]`.
    fn field_cols(&mut self, g: &[&Field], inline_attrs: bool) -> (String, String, String) {
        let f0 = g[0];
        let mut attrs = Vec::new();
        if inline_attrs {
            for a in &f0.attrs {
                let d = self.attr(a);
                attrs.push(flat(&d));
            }
        }
        let mut col1 = String::new();
        let names: Vec<&str> = g.iter().map(|f| f.name.name.as_str()).collect();
        col1.push_str(&names.join(", "));
        let mut col2 = flat(&self.ty(&f0.ty));
        if let Some(d) = &f0.default {
            let dd = self.expr(d);
            col2.push_str(" = ");
            col2.push_str(&print(&dd, super::WIDTH));
        }
        (attrs.join(" "), col1, col2)
    }

    fn enum_decl(&mut self, e: &EnumDecl) -> Doc {
        let (head, brace) = self.type_header("enum", &e.attrs, e.span.start, &e.name, &e.generics, &e.implements);
        let close = e.span.end - 1;
        let first = e.variants.first().map_or(close, |v| v.span.start);
        let multiline = !e.methods.is_empty() || self.newline_between(brace, first) || self.pending_before(close);
        if !multiline {
            let opts = ListOpts { open: "{", close: "}", close_pos: close, force: false, sep: Sep::Variants, pad: true };
            let list = self.list(&e.variants, |v| v.span, |f, v| f.variant(v), opts);
            let head = match head {
                Doc::Concat(mut v) => {
                    v.pop();
                    v.push(text(" "));
                    concat(v)
                }
                other => other,
            };
            return concat(vec![head, list]);
        }
        let mut inner = vec![self.trailing(brace + 1)];
        for (i, v) in e.variants.iter().enumerate() {
            inner.push(Doc::Hard);
            inner.push(self.prefix(v.span.start, i == 0));
            inner.push(self.variant(v));
            inner.push(self.trailing(v.span.end));
        }
        for (i, m) in e.methods.iter().enumerate() {
            inner.push(Doc::Hard);
            let first = i == 0 && e.variants.is_empty();
            inner.push(self.prefix(m.span.start, first));
            inner.push(self.func(m));
            inner.push(self.trailing(m.span.end));
        }
        inner.push(self.dangling(close, !e.variants.is_empty() || !e.methods.is_empty()));
        concat(vec![head, indent(concat(inner)), Doc::Hard, text("}")])
    }

    fn variant(&mut self, v: &Variant) -> Doc {
        let mut parts = vec![self.attrs(&v.attrs, v.name.span.start), text(v.name.name.clone())];
        if !v.fields.is_empty() || self.slice(Span::new(v.name.span.end, v.span.end)).contains('(') {
            let params: Vec<Param> = v.fields.iter().map(|f| Param { attrs: f.attrs.clone(), name: f.name.clone(), ty: f.ty.clone(), span: f.span }).collect();
            parts.push(self.params(&params, v.name.span.end));
        }
        concat(parts)
    }

    // =====================================================================
    // Lists
    // =====================================================================

    /// A bracketed, separated list; broken one item per line (with a
    /// trailing comma for [`Sep::Comma`]) when too long, forced, or when it
    /// contains comments.
    fn list<T>(&mut self, items: &[T], span: impl Fn(&T) -> Span, mut pr: impl FnMut(&mut Self, &T) -> Doc, o: ListOpts) -> Doc {
        let force = o.force || self.pending_before(o.close_pos);
        if items.is_empty() && !force {
            return text(format!("{}{}", o.open, o.close));
        }
        let edge = if o.pad { Doc::Line } else { Doc::Soft };
        let mut inner = vec![edge.clone()];
        let n = items.len();
        for (i, it) in items.iter().enumerate() {
            let sp = span(it);
            inner.push(self.prefix_list(sp.start, i == 0, force));
            inner.push(pr(self, it));
            let trail = self.take_trailing(sp.end, o.close_pos).map_or(Doc::Nil, Doc::Suffix);
            if i + 1 < n {
                inner.push(match o.sep {
                    Sep::Comma => text(","),
                    Sep::Variants => if_break(Doc::Nil, text(",")),
                });
                inner.push(trail);
                inner.push(Doc::Line);
            } else {
                if o.sep == Sep::Comma {
                    inner.push(if_break(text(","), Doc::Nil));
                }
                inner.push(trail);
            }
        }
        inner.push(self.dangling(o.close_pos, n > 0));
        group_with(concat(vec![text(o.open), nest(concat(inner)), edge, text(o.close)]), force)
    }

    /// Like [`Fmt::prefix`] inside a list: comments are only possible in a
    /// forced (broken) list; blank lines are kept only there.
    fn prefix_list(&mut self, pos: u32, first: bool, forced: bool) -> Doc {
        if !forced {
            return Doc::Nil;
        }
        self.prefix(pos, first)
    }

    /// Opening delimiter of a literal is followed by a newline in the
    /// source: the literal stays broken.
    fn written_broken(&self, open: u32, first: u32) -> bool {
        self.newline_between(open, first)
    }

    // =====================================================================
    // Statements
    // =====================================================================

    fn block(&mut self, b: &Block) -> Doc {
        let close = b.span.end.saturating_sub(1);
        if b.stmts.is_empty() && !self.pending_before(close) {
            return text("{}");
        }
        // `{ return x }` written on one line stays so while it fits.
        if b.stmts.len() == 1 && !self.newline_between(b.span.start, b.span.end) && !self.pending_before(b.span.end) && single_line_stmt(&b.stmts[0]) {
            let s = self.stmt(&b.stmts[0]);
            return group(concat(vec![text("{"), indent(concat(vec![Doc::Line, s])), Doc::Line, text("}")]));
        }
        let mut inner = vec![self.trailing(b.span.start + 1)];
        inner.push(self.stmts(&b.stmts));
        inner.push(self.dangling(close, !b.stmts.is_empty()));
        concat(vec![text("{"), indent(concat(inner)), Doc::Hard, text("}")])
    }

    /// Statements, each on its own line (preceded by a newline).
    fn stmts(&mut self, stmts: &[Stmt]) -> Doc {
        let mut out = Vec::new();
        for (i, s) in stmts.iter().enumerate() {
            out.push(Doc::Hard);
            out.push(self.prefix(s.span.start, i == 0));
            out.push(self.stmt(s));
            out.push(self.trailing(s.span.end));
        }
        concat(out)
    }

    fn stmt(&mut self, s: &Stmt) -> Doc {
        match &s.kind {
            StmtKind::Var(v) => self.var(v),
            StmtKind::Const(c) => self.const_(c),
            StmtKind::If(i) => self.if_stmt(i),
            StmtKind::For(f) => self.for_stmt(f),
            StmtKind::Switch(sw) => self.switch_stmt(sw, s.span),
            StmtKind::Return(es) => {
                if es.is_empty() {
                    text("return")
                } else {
                    concat(vec![text("return "), self.expr_list(es)])
                }
            }
            StmtKind::Break => text("break"),
            StmtKind::Continue => text("continue"),
            StmtKind::Defer(e) => concat(vec![text("defer "), self.expr(e)]),
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Assign { target, op, value } => concat(vec![self.expr(target), text(format!(" {} ", assignop(*op))), self.expr(value)]),
            StmtKind::Block(b) => self.block(b),
            StmtKind::TryBlock { body, err, handler } => {
                concat(vec![text("try "), self.block(body), text(format!(" catch {} ", err.name)), self.block(handler)])
            }
            StmtKind::Error => text(self.slice(s.span)),
        }
    }

    fn expr_list(&mut self, es: &[Expr]) -> Doc {
        let mut parts = Vec::new();
        for (i, e) in es.iter().enumerate() {
            if i > 0 {
                parts.push(text(", "));
            }
            parts.push(self.expr(e));
        }
        concat(parts)
    }

    fn if_stmt(&mut self, i: &IfStmt) -> Doc {
        let mut parts = vec![text("if "), self.expr(&i.cond), text(" "), self.block(&i.then)];
        match i.else_.as_deref() {
            Some(ElseBranch::If(e)) => {
                parts.push(text(" else "));
                parts.push(self.if_stmt(e));
            }
            Some(ElseBranch::Block(b)) => {
                parts.push(text(" else "));
                parts.push(self.block(b));
            }
            None => {}
        }
        concat(parts)
    }

    fn for_stmt(&mut self, f: &ForStmt) -> Doc {
        let head = match &f.kind {
            ForKind::Infinite => text("for "),
            ForKind::While(c) => concat(vec![text("for "), self.expr(c), text(" ")]),
            ForKind::In { vars, iter } => {
                let names: Vec<&str> = vars.iter().map(|v| v.name.as_str()).collect();
                concat(vec![text(format!("for {} in ", names.join(", "))), self.expr(iter), text(" ")])
            }
        };
        concat(vec![head, self.block(&f.body)])
    }

    fn switch_stmt(&mut self, sw: &SwitchStmt, span: Span) -> Doc {
        let mut head = vec![text("switch ")];
        let after = match &sw.subject {
            Some(e) => {
                head.push(self.expr(e));
                head.push(text(" "));
                e.span.end
            }
            None => span.start + "switch".len() as u32,
        };
        head.push(text("{"));
        let brace = self.tok_after(after, &TokenKind::LBrace).unwrap_or(after);
        let close = span.end - 1;
        // Where `default:` is, to keep it in place among the cases.
        let default_at = self.default_pos(brace, close);
        enum Clause<'x> {
            Case(&'x Case),
            Default(u32, &'x [Stmt]),
        }
        let mut clauses: Vec<(u32, Clause)> = sw.cases.iter().map(|c| (c.span.start, Clause::Case(c))).collect();
        if let (Some(d), Some(at)) = (&sw.default, default_at) {
            clauses.push((at, Clause::Default(at, d)));
        }
        clauses.sort_by_key(|c| c.0);
        let mut inner = vec![self.trailing(brace + 1)];
        for (i, (start, c)) in clauses.iter().enumerate() {
            inner.push(Doc::Hard);
            inner.push(self.prefix(*start, i == 0));
            match c {
                Clause::Case(c) => {
                    let mut line = vec![text("case ")];
                    for (k, p) in c.patterns.iter().enumerate() {
                        if k > 0 {
                            line.push(text(", "));
                        }
                        line.push(self.pattern(p));
                    }
                    line.push(text(":"));
                    let last = c.patterns.last().map_or(c.span.start, |p| p.span.end);
                    let colon = self.tok_after(last, &TokenKind::Colon).unwrap_or(last);
                    // `case 1: return "one"` written on one line stays so
                    // while it fits flat on the line; otherwise the body
                    // goes on its own line (a broken `case X: f(` would
                    // be re-read as multi-line and reformatted differently,
                    // so the choice is made by width, not by the source).
                    if let [only] = c.body.as_slice() {
                        if !self.newline_between(c.span.start, only.span.end) && single_line_stmt(only) && !self.pending_before(only.span.end) {
                            let save = self.next;
                            let mut flat_line = line.clone();
                            flat_line.push(text(" "));
                            flat_line.push(self.stmt(only));
                            flat_line.push(self.trailing(only.span.end));
                            let flat_line = concat(flat_line);
                            self.next = save;
                            let broken = concat(vec![concat(line), indent(concat(vec![Doc::Hard, self.stmt(only), self.trailing(only.span.end)]))]);
                            let variants = if has_hard(&flat_line) { vec![broken] } else { vec![flat_line, broken] };
                            inner.push(Doc::Cond(variants));
                            continue;
                        }
                    }
                    line.push(self.trailing(colon + 1));
                    inner.push(concat(line));
                    inner.push(indent(self.stmts(&c.body)));
                }
                Clause::Default(at, body) => {
                    inner.push(text("default:"));
                    let colon = self.tok_after(*at, &TokenKind::Colon).unwrap_or(*at);
                    inner.push(self.trailing(colon + 1));
                    inner.push(indent(self.stmts(body)));
                }
            }
        }
        inner.push(self.dangling(close, !clauses.is_empty()));
        concat(vec![concat(head), indent(concat(inner)), Doc::Hard, text("}")])
    }

    /// Position of the `default` keyword directly inside the switch body.
    fn default_pos(&self, brace: u32, close: u32) -> Option<u32> {
        let toks = &self.lexed.tokens;
        let mut depth = 0i32;
        for i in self.lexed.token_at(brace)..toks.len() {
            let t = &toks[i];
            if t.span.start >= close {
                break;
            }
            match t.kind {
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => depth -= 1,
                TokenKind::Default if depth == 1 && toks.get(i + 1).is_some_and(|n| n.kind == TokenKind::Colon) => return Some(t.span.start),
                _ => {}
            }
        }
        None
    }

    fn pattern(&mut self, p: &Pattern) -> Doc {
        match &p.kind {
            PatternKind::Expr(e) => self.expr(e),
            PatternKind::Range(a, b) => {
                let a = a.as_ref().map_or(Doc::Nil, |a| self.expr(a));
                let b = b.as_ref().map_or(Doc::Nil, |b| self.expr(b));
                concat(vec![a, text(".."), b])
            }
            PatternKind::TypeBind { ty, name } => concat(vec![self.ty(ty), text(format!(" {}", name.name))]),
        }
    }

    // =====================================================================
    // Expressions
    // =====================================================================

    fn expr(&mut self, e: &Expr) -> Doc {
        // Block comments written inline before this expression stay inline.
        let mut before = Vec::new();
        while let Some(c) = self.comments.get(self.next) {
            if c.start >= e.span.start || !c.text.starts_with("/*") || self.newline_between(c.start, e.span.start) {
                break;
            }
            before.push(text(format!("{} ", c.text)));
            self.next += 1;
        }
        let d = self.expr_inner(e);
        if before.is_empty() {
            d
        } else {
            before.push(d);
            concat(before)
        }
    }

    /// `base.name`. A chain broken before the dot in the source (a line
    /// starting with `.` continues the previous one) stays broken: the
    /// `.name` goes on its own line, indented, after the comments between.
    fn field(&mut self, base: &Expr, name: &Ident) -> Doc {
        let b = self.expr(base);
        let i = self.lexed.token_at(name.span.start);
        let dot = if i > 0 { self.lexed.tokens[i - 1].span.start } else { name.span.start };
        if !self.newline_between(base.span.end, dot) {
            return concat(vec![b, text(format!(".{}", name.name))]);
        }
        let mut tail = Vec::new();
        let trail = self.take_trailing(base.span.end, dot).map_or(Doc::Nil, Doc::Suffix);
        while let Some(c) = self.comments.get(self.next) {
            if c.start >= dot {
                break;
            }
            tail.push(Doc::Hard);
            tail.push(text(c.text.clone()));
            self.next += 1;
        }
        tail.push(Doc::Hard);
        tail.push(text(format!(".{}", name.name)));
        concat(vec![b, trail, indent(concat(tail))])
    }

    fn expr_inner(&mut self, e: &Expr) -> Doc {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Char(_) => text(self.slice(e.span)),
            ExprKind::Bool(b) => text(if *b { "true" } else { "false" }),
            ExprKind::Nil => text("nil"),
            ExprKind::This => text("this"),
            ExprKind::Ident(i) => text(i.name.clone()),
            ExprKind::Field { base, name } => self.field(base, name),
            ExprKind::Call { callee, args } => self.call(callee, args, e.span),
            ExprKind::Index { base, index } => concat(vec![self.expr(base), text("["), self.expr(index), text("]")]),
            ExprKind::Unary { op, expr } => {
                let o = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "!",
                    UnaryOp::AddrOf => "&",
                    UnaryOp::Deref => "*",
                };
                // `& &x` must not become `&&x`.
                let sep = if *op == UnaryOp::AddrOf && matches!(expr.kind, ExprKind::Unary { op: UnaryOp::AddrOf, .. }) { " " } else { "" };
                concat(vec![text(o), text(sep), self.expr(expr)])
            }
            ExprKind::Binary { .. } => self.binary(e),
            ExprKind::Propagate { expr, context } => {
                let mut parts = vec![self.expr(expr), text("?")];
                if let Some(c) = context {
                    parts.push(text("("));
                    parts.push(self.expr(c));
                    parts.push(text(")"));
                }
                concat(parts)
            }
            ExprKind::Try { expr, handler } => {
                let mut parts = vec![text("try "), self.expr(expr)];
                match handler {
                    TryHandler::Catch { name, body } => {
                        parts.push(text(format!(" catch {} ", name.name)));
                        parts.push(self.block(body));
                    }
                    TryHandler::Else(v) => {
                        parts.push(text(" else "));
                        parts.push(match v.as_ref() {
                            ElseValue::Expr(x) => self.expr(x),
                            ElseValue::Continue(_) => text("continue"),
                            ElseValue::Break(_) => text("break"),
                            ElseValue::Return(vs, _) if vs.is_empty() => text("return"),
                            ElseValue::Return(vs, _) => concat(vec![text("return "), self.expr_list(vs)]),
                        });
                    }
                }
                concat(parts)
            }
            ExprKind::Await(x) => concat(vec![text("await "), self.expr(x)]),
            ExprKind::StructLit { .. } | ExprKind::MapLit { .. } | ExprKind::SliceLit(_) => self.literal(e, false),
            ExprKind::TypeArgs { base, args } => concat(vec![self.expr(base), self.type_args(args)]),
            ExprKind::New(x) => concat(vec![text("new "), self.expr(x)]),
            ExprKind::Tuple(es) => {
                let bare = es.first().is_some_and(|f| f.span.start == e.span.start);
                let inner = self.expr_list(es);
                if bare {
                    inner
                } else {
                    concat(vec![text("("), inner, text(")")])
                }
            }
            ExprKind::Range(a, b) => {
                let a = a.as_ref().map_or(Doc::Nil, |a| self.expr(a));
                let b = b.as_ref().map_or(Doc::Nil, |b| self.expr(b));
                concat(vec![a, text(".."), b])
            }
            ExprKind::Lambda { params, ret, body, is_async } => self.lambda(e.span, params, ret.as_deref(), body, *is_async),
            ExprKind::Paren(x) => concat(vec![text("("), self.expr(x), text(")")]),
            ExprKind::NamedArg { name, value } => concat(vec![text(format!("{}: ", name.name)), self.expr(value)]),
            ExprKind::Error => text(self.slice(e.span)),
        }
    }

    fn type_args(&mut self, args: &[TypeExpr]) -> Doc {
        let mut parts = vec![text("<")];
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                parts.push(text(", "));
            }
            parts.push(self.ty(a));
        }
        parts.push(text(">"));
        concat(parts)
    }

    /// `a + b + c` (one precedence level) breaks after each operator.
    fn binary(&mut self, e: &Expr) -> Doc {
        let ExprKind::Binary { op, .. } = &e.kind else { unreachable!() };
        let prec = op.precedence();
        let mut operands: Vec<&Expr> = Vec::new();
        let mut ops: Vec<BinaryOp> = Vec::new();
        let mut cur = e;
        while let ExprKind::Binary { op, left, right } = &cur.kind {
            if op.precedence() != prec {
                break;
            }
            operands.push(right);
            ops.push(*op);
            cur = left;
        }
        operands.push(cur);
        operands.reverse();
        ops.reverse();
        let first = self.expr(operands[0]);
        let mut rest = Vec::new();
        for (k, (op, rhs)) in ops.iter().zip(&operands[1..]).enumerate() {
            // `a /* c */ + b`: an inline comment before the operator stays there.
            let op_at = self.skip_trivia(operands[k].span.end);
            while let Some(c) = self.comments.get(self.next) {
                if c.start >= op_at || !c.text.starts_with("/*") || c.start < operands[k].span.end {
                    break;
                }
                rest.push(text(format!(" {}", c.text)));
                self.next += 1;
            }
            rest.push(text(format!(" {}", binop(*op))));
            // `>` and `>>` can end a statement (closing generic arguments),
            // so a newline after them would insert a semicolon.
            rest.push(if matches!(op, BinaryOp::Gt | BinaryOp::Shr) { text(" ") } else { Doc::Line });
            rest.push(self.expr(rhs));
        }
        group(concat(vec![first, nest(concat(rest))]))
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Doc {
        let callee_doc = self.expr(callee);
        let close = span.end - 1;
        let opts = || ListOpts { open: "(", close: ")", close_pos: close, force: false, sep: Sep::Comma, pad: false };
        // A last-argument literal may "hug" the call: `f(a, T{` ... `})`.
        let hug = args.last().is_some_and(is_literal) && !self.pending_before(close);
        if !hug {
            let list = self.list(args, |a| a.span, |f, a| f.expr(a), opts());
            return concat(vec![callee_doc, list]);
        }
        let save = self.next;
        let n = args.len();
        let mut heads = Vec::new();
        for a in &args[..n - 1] {
            heads.push(self.expr(a));
        }
        let last_normal = self.literal(&args[n - 1], false);
        self.next = save;
        let normal = self.list(args, |a| a.span, |f, a| f.expr(a), opts());
        self.next = save;
        let mut hugged = vec![text("(")];
        for h in heads {
            hugged.push(h);
            hugged.push(text(", "));
        }
        hugged.push(self.literal(&args[n - 1], true));
        hugged.push(text(")"));
        let hugged = concat(hugged);
        let broken = match normal.clone() {
            Doc::Group { doc, .. } => Doc::Group { doc, force: true },
            d => d,
        };
        let mut variants = Vec::new();
        if !has_forced(&last_normal) {
            variants.push(normal);
        }
        variants.push(hugged);
        variants.push(broken);
        concat(vec![callee_doc, Doc::Cond(variants)])
    }

    /// Struct, map and slice literals; broken when written multi-line in
    /// the source or `force`d (hugging a call).
    fn literal(&mut self, e: &Expr, force: bool) -> Doc {
        let close = e.span.end - 1;
        match &e.kind {
            ExprKind::StructLit { path, generics, fields } => {
                let after = generics.last().map_or(path.span.end, |g| g.span.end);
                let open = self.tok_after(after, &TokenKind::LBrace).unwrap_or(after);
                let first = fields.first().map_or(close, |f| f.span.start);
                let force = force || self.written_broken(open, first);
                let head = concat(vec![text(path.to_string()), if generics.is_empty() { Doc::Nil } else { self.type_args(generics) }]);
                let opts = ListOpts { open: "{", close: "}", close_pos: close, force, sep: Sep::Comma, pad: false };
                let list = self.list(fields, |f| f.span, |f, fi| concat(vec![text(format!("{}: ", fi.name.name)), f.expr(&fi.value)]), opts);
                concat(vec![head, list])
            }
            ExprKind::MapLit { key_ty, val_ty, entries } => {
                let open = self.tok_after(val_ty.span.end, &TokenKind::LBrace).unwrap_or(val_ty.span.end);
                let first = entries.first().map_or(close, |(k, _)| k.span.start);
                let force = force || self.written_broken(open, first);
                let head = concat(vec![text("map["), self.ty(key_ty), text("]"), self.ty(val_ty)]);
                let opts = ListOpts { open: "{", close: "}", close_pos: close, force, sep: Sep::Comma, pad: false };
                let list = self.list(entries, |(k, v)| k.span.to(v.span), |f, (k, v)| concat(vec![f.expr(k), text(": "), f.expr(v)]), opts);
                concat(vec![head, list])
            }
            ExprKind::SliceLit(es) => {
                let first = es.first().map_or(close, |x| x.span.start);
                let force = force || self.written_broken(e.span.start, first);
                let opts = ListOpts { open: "[", close: "]", close_pos: close, force, sep: Sep::Comma, pad: false };
                self.list(es, |x| x.span, |f, x| f.expr(x), opts)
            }
            _ => self.expr(e),
        }
    }

    fn lambda(&mut self, span: Span, params: &[Param], ret: Option<&TypeExpr>, body: &LambdaBody, is_async: bool) -> Doc {
        let src = self.slice(span);
        let rest = src.strip_prefix("async").map_or(src, |r| r.trim_start());
        let func_style = rest.starts_with("func");
        let mut parts = Vec::new();
        if is_async {
            parts.push(text("async "));
        }
        if func_style {
            parts.push(text("func"));
        }
        let open = self.tok_after(span.start, &TokenKind::LParen).unwrap_or(span.start);
        parts.push(self.params(params, open));
        if let Some(r) = ret {
            parts.push(text(" "));
            parts.push(self.ty(r));
        }
        if !func_style {
            parts.push(text(" =>"));
        }
        parts.push(text(" "));
        parts.push(match body {
            LambdaBody::Block(b) => self.block(b),
            LambdaBody::Expr(x) => self.expr(x),
        });
        concat(parts)
    }

    // =====================================================================
    // Types
    // =====================================================================

    fn ty(&mut self, t: &TypeExpr) -> Doc {
        let parenthesized = match &t.kind {
            TypeKind::Fallible(inner) => t.span.start < inner.span.start,
            TypeKind::Tuple(_) | TypeKind::Error | TypeKind::Infer => false,
            _ => self.src.as_bytes().get(t.span.start as usize) == Some(&b'('),
        };
        let d = match &t.kind {
            TypeKind::Named { path, generics } => {
                concat(vec![text(path.to_string()), if generics.is_empty() { Doc::Nil } else { self.type_args(generics) }])
            }
            TypeKind::Fallible(x) => concat(vec![self.ty(x), text("!")]),
            TypeKind::Pointer(x) => concat(vec![text("*"), self.ty(x)]),
            TypeKind::Weak(x) => concat(vec![text("weak *"), self.ty(x)]),
            TypeKind::Slice(x) => concat(vec![text("[]"), self.ty(x)]),
            TypeKind::Array(x, n) => concat(vec![text(format!("[{n}]")), self.ty(x)]),
            TypeKind::ArrayExpr(x, n) => concat(vec![text("["), self.expr(n), text("]"), self.ty(x)]),
            TypeKind::Map(k, v) => concat(vec![text("map["), self.ty(k), text("]"), self.ty(v)]),
            TypeKind::Func { params, ret } => {
                let mut parts = vec![text("func(")];
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        parts.push(text(", "));
                    }
                    parts.push(self.ty(p));
                }
                parts.push(text(")"));
                if let Some(r) = ret {
                    parts.push(text(" "));
                    parts.push(self.ty(r));
                }
                concat(parts)
            }
            TypeKind::Tuple(ts) => {
                let mut parts = vec![text("(")];
                for (i, x) in ts.iter().enumerate() {
                    if i > 0 {
                        parts.push(text(", "));
                    }
                    parts.push(self.ty(x));
                }
                if ts.len() == 1 {
                    parts.push(text(","));
                }
                parts.push(text(")"));
                concat(parts)
            }
            TypeKind::Error => text(self.slice(t.span)),
            TypeKind::Infer => Doc::Nil,
        };
        if parenthesized {
            concat(vec![text("("), d, text(")")])
        } else {
            d
        }
    }
}

/// Statements that may share a line with their block's braces.
fn single_line_stmt(s: &Stmt) -> bool {
    !matches!(s.kind, StmtKind::If(_) | StmtKind::For(_) | StmtKind::Switch(_) | StmtKind::Block(_) | StmtKind::TryBlock { .. } | StmtKind::Var(_) | StmtKind::Const(_))
}

fn col1_multiline(rows: &[Row]) -> bool {
    rows.last().is_some_and(|r| r.names.contains('\n') || r.ty.contains('\n'))
}

/// One aligned field line.
struct Row {
    /// Inline attributes (`@json(name: "id")`), or empty.
    attrs: String,
    names: String,
    /// Type and default value.
    ty: String,
    comment: Option<String>,
}

/// Aligned field rows (like gofmt's tabwriter): attributes, names and
/// types in columns; trailing comments aligned within runs of commented
/// rows.
fn align_rows(rows: &[Row]) -> Doc {
    let width = |s: &str| s.chars().count();
    let w0 = rows.iter().map(|r| width(&r.attrs)).max().unwrap_or(0);
    let w1 = rows.iter().map(|r| width(&r.names)).max().unwrap_or(0);
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let mut j = i;
        while j < rows.len() && rows[j].comment.is_some() {
            j += 1;
        }
        let run_end = j.max(i + 1);
        let w2 = rows[i..run_end].iter().map(|r| width(&r.ty)).max().unwrap_or(0);
        for (k, r) in rows[i..run_end].iter().enumerate() {
            if i + k > 0 {
                out.push(Doc::Hard);
            }
            let mut line = String::new();
            if w0 > 0 {
                line.push_str(&r.attrs);
                line.push_str(&" ".repeat(w0 - width(&r.attrs) + 1));
            }
            line.push_str(&r.names);
            line.push_str(&" ".repeat(w1 - width(&r.names) + 1));
            line.push_str(&r.ty);
            if let Some(c) = &r.comment {
                line.push_str(&" ".repeat(w2 - width(&r.ty) + 1));
                line.push_str(c);
            }
            out.push(text(line));
        }
        i = run_end;
    }
    concat(out)
}
