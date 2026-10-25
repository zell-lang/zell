//! Parser: tokens -> AST.
//!
//! A hand-written recursive-descent parser; binary expressions use
//! precedence climbing driven by [`BinaryOp::precedence`].
//!
//! Notable decisions:
//! - **Struct literals** follow Go's rule: inside the header of
//!   `if`/`for`/`switch` a `{` after a name opens the block, not a literal
//!   (`no_struct_lit`, reset inside parentheses, brackets, call arguments and
//!   blocks). Elsewhere `Name{` / `path.Name{` is a struct literal (any case,
//!   matching grammar/zell.bnf; attribute structs like `range` are lowercase).
//! - **`await` and `?`**: `await f()?` parses as `(await f())?`, so the error
//!   of the awaited result is propagated (see DESIGN.md, «Асинхронность»).
//! - **Fallible types**: `!` applies to the whole preceding type, so
//!   `[]User!` is `([]User)!` and `*T!` is `(*T)!`. In `func(..) T!` the `!`
//!   belongs to the return type.
//! - **Lambdas**: `(` starts a short lambda when its matching `)` is followed
//!   by `=>`, or when it begins with `name type`. `name =>` is a lambda with
//!   one untyped parameter (`=>` has no other meaning).
//! - **Patterns**: `case T x:` is detected by speculatively parsing a type
//!   followed by a name and `:`/`,`; everything else is an expression, or a
//!   range `a..b`.
//! - `else`/`catch` may also start the line after `}`/the `try` operand.
//! - `a, b = b, a` is an assignment whose target and value are `Tuple`s.
//!
//! ## Error recovery
//!
//! Errors are reported as "expected X, found Y". The parser never gives
//! up: [`parse_file`] always returns a [`File`], with error nodes where
//! input was missing or skipped, so tools (LSP, completion) work on
//! half-written code:
//! - **Expressions**: a missing operand becomes [`ExprKind::Error`]
//!   (zero-width, nothing consumed); `x.` with no name yet becomes
//!   `Field { base, name: "" }`; an unclosed `f(` becomes a `Call` whose
//!   last argument is an error node.
//! - **Expression lists** (call arguments, slice literals): a bad element
//!   is skipped up to the next `,` or the closing bracket; a missing
//!   closing bracket before `}`, end of line or end of file is reported
//!   and the list is closed there.
//! - **Types**: a missing type becomes [`TypeKind::Error`].
//! - **Statements**: a statement that fails is skipped to the next `;` (or
//!   the `}` of its block) and recorded as [`StmtKind::Error`]. A
//!   declaration keyword at statement start (`func Name`, `struct`, ...)
//!   means a `}` is missing: the block is closed there.
//! - **Blocks**: a missing `}` at end of file closes the block (and every
//!   enclosing one).
//! - **Declarations**: a failed top-level declaration is skipped to the
//!   next declaration keyword and recorded as [`Decl::Error`].
//!
//! Speculative parses (`silent` mode) never build error nodes: they fail,
//! so the caller backtracks.

use crate::ast::*;
use crate::diagnostics::{Diagnostic, Diagnostics, Fix};
use crate::span::Span;
use crate::token::{Token, TokenKind};

mod recovery;

/// Parses a whole file and returns its AST even when there are syntax
/// errors (see the module docs on recovery). Every syntax error is
/// reported to `diags`.
pub fn parse_file(tokens: Vec<Token>, diags: &mut Diagnostics) -> File {
    parse_impl(tokens, diags).0
}

/// Compatibility wrapper over [`parse_file`] for phases that only accept
/// a clean AST: `None` if any syntax error was found.
pub fn parse(tokens: Vec<Token>, diags: &mut Diagnostics) -> Option<File> {
    let (file, errors) = parse_impl(tokens, diags);
    (errors == 0).then_some(file)
}

/// Lexes and parses `src`: the (possibly partial) AST and all lexer and
/// parser diagnostics.
pub fn parse_source(src: &str) -> (File, Diagnostics) {
    let mut diags = Diagnostics::new();
    let tokens = crate::lexer::lex(src, &mut diags);
    let file = parse_file(tokens, &mut diags);
    (file, diags)
}

fn parse_impl(mut tokens: Vec<Token>, diags: &mut Diagnostics) -> (File, usize) {
    if tokens.last().map(|t| &t.kind) != Some(&TokenKind::Eof) {
        let end = tokens.last().map(|t| t.span.end).unwrap_or(0);
        tokens.push(Token { kind: TokenKind::Eof, span: Span::new(end, end) });
    }
    let mut p = Parser {
        tokens,
        pos: 0,
        prev_span: Span::DUMMY,
        no_struct_lit: false,
        attr_args: false,
        depth: 0,
        silent: false,
        error_count: 0,
        last_error_at: None,
        closed_early: false,
        splits: Vec::new(),
        diags,
    };
    let file = p.file();
    (file, p.error_count)
}

/// Marker for "a diagnostic has already been recorded".
#[derive(Debug)]
struct ParseError;

type PResult<T> = Result<T, ParseError>;

/// Maximum nesting of expressions/blocks/types, to avoid stack overflow.
const MAX_DEPTH: u32 = 128;

struct Parser<'d> {
    tokens: Vec<Token>,
    pos: usize,
    /// Span of the last consumed non-`Semi` token; used to end node spans.
    prev_span: Span,
    /// Inside an `if`/`for`/`switch` header: `Name{` is not a struct literal.
    no_struct_lit: bool,
    depth: u32,
    /// Speculative parsing: errors are not reported.
    silent: bool,
    error_count: usize,
    /// Start offset of the last reported error, to avoid duplicates.
    last_error_at: Option<u32>,
    /// A body was closed without its `}` (before a declaration or at end
    /// of file), already reported; the declaration needs no terminator.
    closed_early: bool,
    /// `>>` tokens split into two `>` (for nested generics), so a
    /// speculative parse can undo them: (index, original token).
    splits: Vec<(usize, Token)>,
    /// Inside attribute arguments: calls take `name: value` arguments
    /// (`@cfg(not(os: "windows"))`).
    attr_args: bool,
    diags: &'d mut Diagnostics,
}

/// Saved parser position for backtracking.
struct Checkpoint {
    pos: usize,
    prev_span: Span,
    splits: usize,
}

impl Parser<'_> {
    // =====================================================================
    // Token helpers
    // =====================================================================

    fn peek(&self) -> &TokenKind {
        self.peek_n(0)
    }

    fn peek_n(&self, n: usize) -> &TokenKind {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].kind
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn at(&self, kind: &TokenKind) -> bool {
        self.peek() == kind
    }

    fn at_eof(&self) -> bool {
        self.at(&TokenKind::Eof)
    }

    fn bump(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if tok.kind != TokenKind::Eof {
            self.pos += 1;
        }
        if tok.kind != TokenKind::Semi {
            self.prev_span = tok.span;
        }
        tok
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Consumes `kind` or reports "expected `kind`, found ...".
    fn expect(&mut self, kind: &TokenKind) -> PResult<Span> {
        if self.at(kind) {
            Ok(self.bump().span)
        } else {
            Err(self.unexpected(&format!("`{}`", kind.text())))
        }
    }

    fn skip_semis(&mut self) {
        while self.at(&TokenKind::Semi) {
            self.bump();
        }
    }

    /// Span from `start` to the end of the last consumed token.
    fn span_from(&self, start: Span) -> Span {
        if self.prev_span.end < start.start {
            start
        } else {
            start.to(self.prev_span)
        }
    }

    fn checkpoint(&self) -> Checkpoint {
        Checkpoint { pos: self.pos, prev_span: self.prev_span, splits: self.splits.len() }
    }

    fn restore(&mut self, cp: Checkpoint) {
        while self.splits.len() > cp.splits {
            let (i, tok) = self.splits.pop().unwrap();
            self.tokens.remove(i + 1);
            self.tokens[i] = tok;
        }
        self.pos = cp.pos;
        self.prev_span = cp.prev_span;
    }

    // =====================================================================
    // Names and paths
    // =====================================================================

    fn ident(&mut self) -> PResult<Ident> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                Ok(Ident { name, span })
            }
            _ => Err(self.unexpected("identifier")),
        }
    }

    /// A field name in a struct declaration, a struct literal or an
    /// attribute argument: an identifier or any keyword (`enum`, `default`).
    fn field_decl_name(&mut self) -> PResult<Ident> {
        if is_keyword(self.peek()) {
            let name = self.peek().text().to_string();
            let span = self.bump().span;
            return Ok(Ident { name, span });
        }
        self.ident()
    }

    /// At `name:` where `name` is an identifier or a keyword (named
    /// attribute arguments, struct literal fields).
    fn at_field_name_colon(&self) -> bool {
        (matches!(self.peek(), TokenKind::Ident(_)) || is_keyword(self.peek())) && self.peek_n(1) == &TokenKind::Colon
    }

    /// An identifier or `_`.
    fn binding(&mut self) -> PResult<Ident> {
        if self.at(&TokenKind::Underscore) {
            let span = self.bump().span;
            return Ok(Ident { name: "_".into(), span });
        }
        self.ident()
    }

    fn at_binding(&self) -> bool {
        matches!(self.peek(), TokenKind::Ident(_) | TokenKind::Underscore)
    }

    /// `a.b.c`. Segments after a dot may be keywords (`examples.async`).
    fn path(&mut self) -> PResult<Path> {
        let first = self.ident()?;
        self.path_rest(first)
    }

    /// A namespace or import path, where every segment may be a keyword.
    fn module_path(&mut self) -> PResult<Path> {
        let first = self.name_or_keyword()?;
        self.path_rest(first)
    }

    fn path_rest(&mut self, first: Ident) -> PResult<Path> {
        let start = first.span;
        let mut segments = vec![first];
        while self.eat(&TokenKind::Dot) {
            segments.push(self.name_or_keyword()?);
        }
        Ok(Path { segments, span: self.span_from(start) })
    }

    /// An identifier, or a keyword used as a name (`async`, `default`).
    fn name_or_keyword(&mut self) -> PResult<Ident> {
        if let TokenKind::Int(_) = self.peek() {
            return Err(self.unexpected("identifier"));
        }
        self.field_name()
    }

    // =====================================================================
    // File and declarations
    // =====================================================================

    fn file(&mut self) -> File {
        self.skip_semis();
        let start = self.span();
        let attrs = self.attributes().unwrap_or_else(|_| {
            self.sync_decl();
            Vec::new()
        });
        let namespace = if self.at(&TokenKind::Namespace) {
            self.bump();
            match self.module_path().and_then(|p| self.expect_terminator().map(|_| p)) {
                Ok(p) => p,
                Err(_) => {
                    self.sync_decl();
                    Path { segments: vec![], span: start }
                }
            }
        } else {
            self.unexpected("`namespace`");
            Path { segments: vec![], span: Span::new(start.start, start.start) }
        };

        let mut imports = Vec::new();
        let mut decls = Vec::new();
        loop {
            self.skip_semis();
            if self.at_eof() {
                break;
            }
            let before = self.pos;
            if self.at(&TokenKind::Import) {
                if decls.iter().any(|d| !matches!(d, Decl::Error(_))) {
                    self.error_at(self.span(), "imports must come before declarations");
                }
                match self.import() {
                    Ok(i) => imports.push(i),
                    Err(_) => self.sync_decl(),
                }
            } else {
                let start = self.span();
                match self.decl() {
                    Ok(d) => {
                        decls.push(d);
                        // After a body closed early (missing `}`), the next
                        // declaration follows directly.
                        let early = std::mem::take(&mut self.closed_early);
                        if !early && self.expect_terminator().is_err() {
                            let skip = self.span();
                            self.sync_decl();
                            decls.push(Decl::Error(self.skipped_from(skip)));
                        }
                    }
                    Err(_) => {
                        self.sync_decl();
                        decls.push(Decl::Error(self.skipped_from(start)));
                    }
                }
            }
            if self.pos == before && !self.at_eof() {
                self.bump();
            }
        }
        File { attrs, namespace, imports, decls, span: self.span_from(start) }
    }

    /// `import a.b [as c]`
    fn import(&mut self) -> PResult<Import> {
        let start = self.expect(&TokenKind::Import)?;
        let path = self.module_path()?;
        let alias = if self.eat(&TokenKind::As) { Some(self.ident()?) } else { None };
        let span = self.span_from(start);
        self.expect_terminator()?;
        Ok(Import { path, alias, span })
    }

    /// One top-level declaration, with its leading attributes.
    fn decl(&mut self) -> PResult<Decl> {
        let attrs = self.attributes()?;
        let mut decl = match self.peek() {
            TokenKind::Struct => Decl::Struct(self.struct_decl()?),
            TokenKind::Interface => Decl::Interface(self.interface_decl()?),
            // `sealed` is a contextual keyword: only before `interface`.
            TokenKind::Ident(s) if s == "sealed" && self.peek_n(1) == &TokenKind::Interface => {
                let start = self.bump().span;
                let mut d = self.interface_decl()?;
                d.is_sealed = true;
                d.span = start.to(d.span);
                Decl::Interface(d)
            }
            TokenKind::Enum => Decl::Enum(self.enum_decl()?),
            TokenKind::Func | TokenKind::Async => Decl::Func(self.func_decl(true)?),
            // `extern` is a contextual keyword: only before `func`.
            TokenKind::Ident(s) if s == "extern" && self.peek_n(1) == &TokenKind::Func => {
                let start = self.bump().span;
                let mut d = self.func_decl(false)?;
                if d.body.is_some() {
                    return Err(self.error_at(d.name.span, "an `extern func` has no body"));
                }
                d.is_extern = true;
                d.span = start.to(d.span);
                Decl::Func(d)
            }
            TokenKind::Var => Decl::Var(self.var_decl()?),
            TokenKind::Const => Decl::Const(self.const_decl()?),
            // `type` is a contextual keyword: only in `type Name = T`.
            TokenKind::Ident(s) if s == "type" && matches!(self.peek_n(1), TokenKind::Ident(_)) && self.peek_n(2) == &TokenKind::Assign => {
                let start = self.bump().span;
                let name = self.ident()?;
                self.bump(); // `=`
                let ty = self.parse_type()?;
                Decl::TypeAlias(TypeAliasDecl { attrs: Vec::new(), name, ty, span: self.span_from(start) })
            }
            TokenKind::Static => {
                return Err(self.error_at(self.span(), "`static` is only allowed on methods inside a struct"))
            }
            _ => return Err(self.unexpected("declaration")),
        };
        if let Some(first) = attrs.first() {
            let span = first.span.to(decl.span());
            match &mut decl {
                Decl::Struct(d) => (d.attrs, d.span) = (attrs, span),
                Decl::Interface(d) => (d.attrs, d.span) = (attrs, span),
                Decl::Enum(d) => (d.attrs, d.span) = (attrs, span),
                Decl::Func(d) => (d.attrs, d.span) = (attrs, span),
                Decl::Var(d) => (d.attrs, d.span) = (attrs, span),
                Decl::Const(d) => (d.attrs, d.span) = (attrs, span),
                Decl::TypeAlias(d) => (d.attrs, d.span) = (attrs, span),
                Decl::Error(_) => {}
            }
        }
        Ok(decl)
    }

    /// Zero or more `@path[(args)]`, each optionally followed by newlines.
    fn attributes(&mut self) -> PResult<Vec<Attribute>> {
        let mut attrs = Vec::new();
        while self.at(&TokenKind::At) {
            let start = self.bump().span;
            // Attribute names may be keywords: `@default`.
            let path = self.module_path()?;
            let mut args = Vec::new();
            if self.eat(&TokenKind::LParen) {
                self.with_struct_lit(true, |p| {
                    while !p.at(&TokenKind::RParen) && !p.at_eof() {
                        let arg_start = p.span();
                        // Keywords name parameters too: `@cli.flag(default: "x")`, `@Conf(enum: "a,b")`.
                        let name = if p.at_field_name_colon() {
                            let name = p.field_decl_name()?;
                            p.bump(); // `:`
                            Some(name)
                        } else {
                            None
                        };
                        let saved = std::mem::replace(&mut p.attr_args, true);
                        let value = p.parse_expr();
                        p.attr_args = saved;
                        let value = value?;
                        args.push(AttrArg { name, value, span: p.span_from(arg_start) });
                        if !p.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    Ok(())
                })?;
                self.expect(&TokenKind::RParen)?;
            }
            attrs.push(Attribute { path, args, span: self.span_from(start) });
            self.skip_semis();
        }
        Ok(attrs)
    }

    /// Optional `<T, U: Bound>` after a declaration name.
    fn generic_params(&mut self) -> PResult<Vec<GenericParam>> {
        let mut params = Vec::new();
        if !self.eat(&TokenKind::Lt) {
            return Ok(params);
        }
        loop {
            let name = self.ident()?;
            let bound = if self.eat(&TokenKind::Colon) { Some(self.parse_type()?) } else { None };
            params.push(GenericParam { span: self.span_from(name.span), name, bound });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_gt()?;
        Ok(params)
    }

    /// `struct Name { fields, embeds, methods }`
    fn struct_decl(&mut self) -> PResult<StructDecl> {
        let start = self.expect(&TokenKind::Struct)?;
        // `default` is allowed as a name for attribute structs (`@default("now()")`).
        let name = self.field_decl_name()?;
        let generics = self.generic_params()?;
        let implements = self.implements_clause()?;
        self.expect(&TokenKind::LBrace)?;
        let (mut fields, mut embeds, mut methods) = (Vec::new(), Vec::new(), Vec::new());
        self.member_list(|p| {
            let attrs = p.attributes()?;
            let attr_span = attrs.first().map(|a| a.span);
            match p.peek() {
                TokenKind::Func | TokenKind::Static | TokenKind::Async => {
                    let mut method = p.func_decl(true)?;
                    method.span = attr_span.map_or(method.span, |s| s.to(method.span));
                    method.attrs = attrs;
                    methods.push(method);
                }
                TokenKind::Ident(_) if p.peek_n(1) == &TokenKind::Comma || starts_type(p.peek_n(1)) => {
                    fields.extend(p.fields(attrs)?)
                }
                // A field named by a keyword (`default`, `enum`; attribute
                // structs: `@flag(default: ...)`); not one that starts a method.
                k if is_keyword(k)
                    && !matches!(k, TokenKind::Func | TokenKind::Static | TokenKind::Async)
                    && (p.peek_n(1) == &TokenKind::Comma || starts_type(p.peek_n(1))) =>
                {
                    fields.extend(p.fields(attrs)?)
                }
                TokenKind::Ident(_) | TokenKind::Star => {
                    if let Some(span) = attr_span {
                        p.diags.warning("attributes on embedded types are ignored", span);
                    }
                    embeds.push(p.parse_type()?)
                }
                _ => return Err(p.unexpected("field, method or `}`")),
            }
            Ok(())
        })?;
        Ok(StructDecl { attrs: Vec::new(), name, generics, fields, embeds, methods, implements, span: self.span_from(start) })
    }

    /// Optional `implements A, B` before the body of a struct or enum
    /// (`implements` is a contextual keyword).
    fn implements_clause(&mut self) -> PResult<Vec<TypeExpr>> {
        let mut out = Vec::new();
        if !matches!(self.peek(), TokenKind::Ident(s) if s == "implements") {
            return Ok(out);
        }
        self.bump();
        loop {
            out.push(self.parse_type()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(out)
    }

    /// `X, Y float [= default]`: one or more field names sharing a type.
    fn fields(&mut self, attrs: Vec<Attribute>) -> PResult<Vec<Field>> {
        let start = attrs.first().map(|a| a.span);
        let mut names = vec![self.field_decl_name()?];
        while self.eat(&TokenKind::Comma) {
            names.push(self.field_decl_name()?);
        }
        let ty = self.parse_type()?;
        let default = if self.eat(&TokenKind::Assign) { Some(self.parse_expr()?) } else { None };
        Ok(names
            .into_iter()
            .map(|name| Field {
                span: self.span_from(start.unwrap_or(name.span)),
                attrs: attrs.clone(),
                name,
                ty: ty.clone(),
                default: default.clone(),
            })
            .collect())
    }

    /// At a declaration keyword used as a field name (`enum string`), not
    /// at a nested declaration (`struct T {`, `enum E<T>`), which means the
    /// enclosing body lacks its `}`.
    fn keyword_field_ahead(&self) -> bool {
        match self.peek_n(1) {
            TokenKind::Comma => true,
            TokenKind::Ident(_) => {
                !matches!(self.peek_n(2), TokenKind::LBrace | TokenKind::Lt)
                    && !matches!(self.peek_n(2), TokenKind::Ident(s) if s == "implements")
            }
            k => starts_type(k),
        }
    }

    /// Parses members until the closing `}` (consumed), with recovery.
    fn member_list(&mut self, mut member: impl FnMut(&mut Self) -> PResult<()>) -> PResult<()> {
        loop {
            self.skip_semis();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            if matches!(self.peek(), TokenKind::Struct | TokenKind::Interface | TokenKind::Enum | TokenKind::Import | TokenKind::Namespace)
                && !self.keyword_field_ahead()
            {
                // A nested declaration: this body lacks its `}`.
                break;
            }
            let before = self.pos;
            if member(self).and_then(|_| self.expect_terminator()).is_err() {
                self.sync_stmt();
            }
            if self.pos == before && !self.at(&TokenKind::RBrace) && !self.at_eof() {
                self.bump();
            }
        }
        self.close_brace()
    }

    /// `interface Name { func M(..) T }`
    fn interface_decl(&mut self) -> PResult<InterfaceDecl> {
        let start = self.expect(&TokenKind::Interface)?;
        let name = self.ident()?;
        let generics = self.generic_params()?;
        self.expect(&TokenKind::LBrace)?;
        let mut methods = Vec::new();
        let mut embeds = Vec::new();
        self.member_list(|p| {
            let attrs = p.attributes()?;
            if attrs.is_empty() && matches!(p.peek(), TokenKind::Ident(_)) {
                embeds.push(p.parse_type()?);
                return Ok(());
            }
            if !matches!(p.peek(), TokenKind::Func | TokenKind::Async) {
                return Err(p.unexpected("method signature, embedded interface or `}`"));
            }
            let mut method = p.func_decl(false)?;
            if let Some(first) = attrs.first() {
                method.span = first.span.to(method.span);
            }
            method.attrs = attrs;
            methods.push(method);
            Ok(())
        })?;
        Ok(InterfaceDecl { attrs: Vec::new(), name, generics, methods, embeds, is_sealed: false, span: self.span_from(start) })
    }

    /// `enum Name { A, B }` or `enum Name { Ok(value int) \n Fail(code int) }`
    fn enum_decl(&mut self) -> PResult<EnumDecl> {
        let start = self.expect(&TokenKind::Enum)?;
        let name = self.ident()?;
        let generics = self.generic_params()?;
        let implements = self.implements_clause()?;
        self.expect(&TokenKind::LBrace)?;
        let mut variants = Vec::new();
        let mut methods = Vec::new();
        let mut last = usize::MAX;
        loop {
            while self.eat(&TokenKind::Semi) || self.eat(&TokenKind::Comma) {}
            if self.at(&TokenKind::RBrace) || self.at_eof() || matches!(self.peek(), TokenKind::Struct | TokenKind::Interface | TokenKind::Enum | TokenKind::Import) {
                break;
            }
            if self.pos == last {
                self.bump();
                continue;
            }
            last = self.pos;
            if matches!(self.peek(), TokenKind::Func | TokenKind::Static | TokenKind::Async) || (self.at(&TokenKind::At) && !methods.is_empty()) {
                let attrs = self.attributes()?;
                match self.func_decl(true) {
                    Ok(mut f) => {
                        f.attrs = attrs;
                        methods.push(f);
                    }
                    Err(_) => self.sync_stmt(),
                }
                continue;
            }
            if !methods.is_empty() {
                self.unexpected("a method (variants come before the methods of an enum)");
                self.sync_stmt();
                continue;
            }
            match self.variant() {
                Ok(v) => variants.push(v),
                Err(_) => {
                    self.sync_stmt();
                    continue;
                }
            }
            if !matches!(self.peek(), TokenKind::Comma | TokenKind::Semi | TokenKind::RBrace) {
                self.unexpected("`,`, end of line or `}`");
                self.sync_stmt();
            }
        }
        self.close_brace()?;
        Ok(EnumDecl { attrs: Vec::new(), name, generics, variants, methods, implements, span: self.span_from(start) })
    }

    fn variant(&mut self) -> PResult<Variant> {
        let attrs = self.attributes()?;
        let name = self.ident()?;
        let start = attrs.first().map_or(name.span, |a| a.span);
        let fields = if self.at(&TokenKind::LParen) {
            self.params()?
                .into_iter()
                .map(|p| Field { attrs: p.attrs, name: p.name, ty: p.ty, default: None, span: p.span })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Variant { span: self.span_from(start), attrs, name, fields })
    }

    /// `[static] [async] func Name(params) [ret] body`. With `body == false`
    /// (interfaces) the body is optional: a method with one is a default.
    fn func_decl(&mut self, body: bool) -> PResult<FuncDecl> {
        let start = self.span();
        let (mut is_async, mut is_static) = (false, false);
        loop {
            match self.peek() {
                TokenKind::Async if !is_async => is_async = true,
                TokenKind::Static if !is_static => is_static = true,
                _ => break,
            }
            self.bump();
        }
        self.expect(&TokenKind::Func)?;
        let name = self.ident()?;
        let generics = self.generic_params()?;
        let params = self.params()?;
        let ret = if starts_type(self.peek()) { Some(self.parse_type()?) } else { None };
        let body = if body {
            if !self.at(&TokenKind::LBrace) {
                return Err(self.unexpected("`{`"));
            }
            Some(self.block()?)
        } else if self.at(&TokenKind::LBrace) {
            Some(self.block()?)
        } else {
            None
        };
        Ok(FuncDecl {
            attrs: Vec::new(),
            name,
            generics,
            params,
            ret,
            body,
            is_async,
            is_static,
            is_extern: false,
            span: self.span_from(start),
        })
    }

    /// `(a int, b, c string)`; names without a type take the next type (Go style).
    fn params(&mut self) -> PResult<Vec<Param>> {
        self.params_ext(false)
    }

    /// Parameters; with `untyped` (short lambdas), trailing names without
    /// a type (`(u) =>`, `(a, b) =>`) get [`TypeKind::Infer`].
    fn params_ext(&mut self, untyped: bool) -> PResult<Vec<Param>> {
        self.expect(&TokenKind::LParen)?;
        let mut params = Vec::new();
        // Names (with their attributes and start span) still waiting for a type.
        let mut pending: Vec<(Vec<Attribute>, Ident, Span)> = Vec::new();
        while !self.at(&TokenKind::RParen) && !self.at_eof() {
            let attrs = self.attributes()?;
            let name = self.binding()?;
            let start = attrs.first().map_or(name.span, |a| a.span);
            if self.eat(&TokenKind::Comma) {
                pending.push((attrs, name, start));
                continue;
            }
            if self.at(&TokenKind::RParen) {
                pending.push((attrs, name, start));
                break;
            }
            let ty = if self.at(&TokenKind::Ellipsis) {
                let dots = self.bump().span;
                let inner = self.parse_type()?;
                let span = dots.to(inner.span);
                TypeExpr { kind: TypeKind::Variadic(Box::new(inner)), span }
            } else {
                self.parse_type()?
            };
            for (attrs, name, start) in pending.drain(..).chain(std::iter::once((attrs, name, start))) {
                params.push(Param { span: start.to(ty.span), attrs, name, ty: ty.clone() });
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        if !pending.is_empty() {
            if !untyped || !params.is_empty() {
                return Err(self.unexpected("parameter type"));
            }
            for (attrs, name, start) in pending.drain(..) {
                let ty = TypeExpr { kind: TypeKind::Infer, span: name.span };
                params.push(Param { span: start.to(name.span), attrs, name, ty });
            }
        }
        self.expect(&TokenKind::RParen)?;
        Ok(params)
    }

    /// `var x [T] [= e]` or `var (a, b) [T] = e`.
    fn var_decl(&mut self) -> PResult<VarDecl> {
        let start = self.expect(&TokenKind::Var)?;
        let names = if self.eat(&TokenKind::LParen) {
            let mut names = vec![self.binding()?];
            while self.eat(&TokenKind::Comma) {
                if self.at(&TokenKind::RParen) {
                    break;
                }
                names.push(self.binding()?);
            }
            self.expect(&TokenKind::RParen)?;
            names
        } else {
            // `var v, ok = m[k]` is `var (v, ok) = m[k]` (Go's spelling).
            let mut names = vec![self.binding()?];
            while self.eat(&TokenKind::Comma) {
                names.push(self.binding()?);
            }
            names
        };
        let ty = if starts_type(self.peek()) { Some(self.parse_type()?) } else { None };
        let init = if self.eat(&TokenKind::Assign) {
            Some(self.parse_expr()?)
        } else if ty.is_none() {
            return Err(self.unexpected("type or `=`"));
        } else {
            None
        };
        Ok(VarDecl { attrs: Vec::new(), names, ty, init, span: self.span_from(start) })
    }

    /// `const Name [T] = e`
    fn const_decl(&mut self) -> PResult<ConstDecl> {
        let start = self.expect(&TokenKind::Const)?;
        let name = self.ident()?;
        let ty = if starts_type(self.peek()) { Some(self.parse_type()?) } else { None };
        self.expect(&TokenKind::Assign)?;
        let value = self.parse_expr()?;
        Ok(ConstDecl { attrs: Vec::new(), name, ty, value, span: self.span_from(start) })
    }

    // =====================================================================
    // Types
    // =====================================================================

    /// A type, with an optional trailing `!` applying to all of it.
    fn parse_type(&mut self) -> PResult<TypeExpr> {
        let ty = self.type_base()?;
        if self.at(&TokenKind::Bang) {
            self.bump();
            let span = self.span_from(ty.span);
            return Ok(TypeExpr { kind: TypeKind::Fallible(Box::new(ty)), span });
        }
        Ok(ty)
    }

    fn type_base(&mut self) -> PResult<TypeExpr> {
        self.nested(|p| {
            let start = p.span();
            let kind = match p.peek() {
                // `weak *T` (`weak` is contextual: only before `*` in a type).
                TokenKind::Ident(w) if w == "weak" && *p.peek_n(1) == TokenKind::Star => {
                    p.bump();
                    p.bump();
                    TypeKind::Weak(Box::new(p.type_base()?))
                }
                TokenKind::Ident(_) => {
                    let path = p.path()?;
                    let mut generics = Vec::new();
                    if p.eat(&TokenKind::Lt) {
                        loop {
                            generics.push(p.parse_type()?);
                            if !p.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                        p.expect_gt()?;
                    }
                    TypeKind::Named { path, generics }
                }
                TokenKind::Star => {
                    p.bump();
                    TypeKind::Pointer(Box::new(p.type_base()?))
                }
                TokenKind::LBracket => {
                    p.bump();
                    if p.eat(&TokenKind::RBracket) {
                        TypeKind::Slice(Box::new(p.type_base()?))
                    } else {
                        if let (TokenKind::Int(text), TokenKind::RBracket) = (p.peek().clone(), p.peek_n(1).clone()) {
                            let span = p.bump().span;
                            let len = p.int_value(&text, span);
                            p.expect(&TokenKind::RBracket)?;
                            TypeKind::Array(Box::new(p.type_base()?), len)
                        } else {
                            let len = p.with_struct_lit(false, |p| p.parse_expr())?;
                            p.expect(&TokenKind::RBracket)?;
                            TypeKind::ArrayExpr(Box::new(p.type_base()?), Box::new(len))
                        }
                    }
                }
                TokenKind::Map => {
                    p.bump();
                    p.expect(&TokenKind::LBracket)?;
                    let key = p.parse_type()?;
                    p.expect(&TokenKind::RBracket)?;
                    TypeKind::Map(Box::new(key), Box::new(p.type_base()?))
                }
                TokenKind::Func => {
                    p.bump();
                    p.expect(&TokenKind::LParen)?;
                    let params = p.type_list(&TokenKind::RParen)?;
                    p.expect(&TokenKind::RParen)?;
                    let ret = if starts_type(p.peek()) { Some(Box::new(p.parse_type()?)) } else { None };
                    TypeKind::Func { params, ret }
                }
                TokenKind::LParen => {
                    p.bump();
                    let mut types = p.type_list(&TokenKind::RParen)?;
                    let trailing_comma = p.tokens[p.pos.saturating_sub(1)].kind == TokenKind::Comma;
                    p.expect(&TokenKind::RParen)?;
                    if types.len() == 1 && !trailing_comma {
                        // `(T)` is just a parenthesized type.
                        let mut ty = types.pop().unwrap();
                        ty.span = p.span_from(start);
                        return Ok(ty);
                    }
                    TypeKind::Tuple(types)
                }
                _ => {
                    let here = p.here();
                    return p.recover("type", TypeExpr { kind: TypeKind::Error, span: here });
                }
            };
            Ok(TypeExpr { kind, span: p.span_from(start) })
        })
    }

    /// Comma-separated types up to (not including) `close`; trailing comma allowed.
    fn type_list(&mut self, close: &TokenKind) -> PResult<Vec<TypeExpr>> {
        let mut types = Vec::new();
        while !self.at(close) && !self.at_eof() {
            types.push(self.parse_type()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(types)
    }

    /// Consumes a `>` closing generic arguments, splitting `>>` if needed.
    fn expect_gt(&mut self) -> PResult<()> {
        if self.eat(&TokenKind::Gt) {
            return Ok(());
        }
        if self.at(&TokenKind::Shr) {
            let tok = self.tokens[self.pos].clone();
            let mid = tok.span.start + 1;
            self.tokens[self.pos] = Token { kind: TokenKind::Gt, span: Span::new(tok.span.start, mid) };
            self.tokens.insert(self.pos + 1, Token { kind: TokenKind::Gt, span: Span::new(mid, tok.span.end) });
            self.splits.push((self.pos, tok));
            self.bump();
            return Ok(());
        }
        Err(self.unexpected("`>`"))
    }

    // =====================================================================
    // Statements
    // =====================================================================

    /// `{ stmts }`
    fn block(&mut self) -> PResult<Block> {
        self.nested(|p| {
            let start = p.expect(&TokenKind::LBrace)?;
            let stmts = p.with_struct_lit(true, |p| p.stmt_list(|k| matches!(k, TokenKind::RBrace)));
            p.close_brace()?;
            Ok(Block { stmts, span: p.span_from(start) })
        })
    }

    /// Statements until a token matching `stop` (not consumed), with recovery.
    fn stmt_list(&mut self, stop: fn(&TokenKind) -> bool) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        loop {
            self.skip_semis();
            if stop(self.peek()) || self.at_eof() {
                break;
            }
            if self.at_decl_start() {
                // A declaration inside a body: the body lacks its `}`.
                break;
            }
            let start = self.span();
            let before = self.pos;
            match self.stmt() {
                Ok(s) => {
                    stmts.push(s);
                    if !stop(self.peek()) && self.expect_terminator().is_err() {
                        let skip = self.span();
                        self.sync_stmt();
                        stmts.push(Stmt { kind: StmtKind::Error, span: self.skipped_from(skip) });
                    }
                }
                Err(_) => {
                    self.sync_stmt();
                    stmts.push(Stmt { kind: StmtKind::Error, span: self.skipped_from(start) });
                }
            }
            if self.pos == before && !stop(self.peek()) && !self.at_eof() {
                self.bump();
            }
        }
        stmts
    }

    /// The optional label of `break` / `continue` (on the same line).
    fn jump_label(&mut self) -> PResult<Option<Ident>> {
        if matches!(self.peek(), TokenKind::Ident(_)) {
            return Ok(Some(self.ident()?));
        }
        Ok(None)
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let kind = match self.peek() {
            TokenKind::Var => StmtKind::Var(self.var_decl()?),
            TokenKind::Const => StmtKind::Const(self.const_decl()?),
            TokenKind::If => StmtKind::If(self.if_stmt()?),
            TokenKind::For => StmtKind::For(self.for_stmt()?),
            TokenKind::Switch => StmtKind::Switch(self.switch_stmt()?),
            TokenKind::Return => {
                self.bump();
                StmtKind::Return(self.return_values()?)
            }
            TokenKind::Break => {
                self.bump();
                StmtKind::Break(self.jump_label()?)
            }
            TokenKind::Continue => {
                self.bump();
                StmtKind::Continue(self.jump_label()?)
            }
            TokenKind::Ident(_) if self.peek_n(1) == &TokenKind::Colon => {
                let label = self.ident()?;
                self.bump();
                while self.eat(&TokenKind::Semi) {}
                if !matches!(self.peek(), TokenKind::For | TokenKind::Switch) {
                    let sp = self.span();
                    return Err(self.error_at(sp, format!("label {} must be followed by a for or switch statement", label.name)));
                }
                let stmt = self.stmt()?;
                StmtKind::Labeled { label, stmt: Box::new(stmt) }
            }
            TokenKind::Defer => {
                self.bump();
                StmtKind::Defer(self.parse_expr()?)
            }
            TokenKind::LBrace => StmtKind::Block(self.block()?),
            TokenKind::Try if self.peek_n(1) == &TokenKind::LBrace => self.try_block()?,
            _ => self.simple_stmt()?,
        };
        Ok(Stmt { kind, span: self.span_from(start) })
    }

    /// `try { ... } catch err { ... }` (no `else` form).
    fn try_block(&mut self) -> PResult<StmtKind> {
        self.expect(&TokenKind::Try)?;
        let body = self.block()?;
        if self.at(&TokenKind::Semi) && matches!(self.peek_n(1), TokenKind::Catch | TokenKind::Else) {
            self.bump();
        }
        if self.at(&TokenKind::Else) {
            return Err(self.error_at(self.span(), "a block try has no else form; use catch err { ... }"));
        }
        self.expect(&TokenKind::Catch)?;
        let err = self.binding()?;
        let handler = self.block()?;
        Ok(StmtKind::TryBlock { body, err, handler })
    }

    /// Values after `return`, if any on this line.
    fn return_values(&mut self) -> PResult<Vec<Expr>> {
        if at_expr_end(self.peek()) {
            return Ok(Vec::new());
        }
        self.expr_list()
    }

    /// Expression statement or (compound) assignment; `a, b = b, a` uses tuples.
    fn simple_stmt(&mut self) -> PResult<StmtKind> {
        let targets = self.expr_list()?;
        let target = tuple_of(targets);
        if self.at_incdec() {
            return Ok(self.incdec(target));
        }
        let Some(op) = assign_op(self.peek()) else {
            if let ExprKind::Tuple(_) = target.kind {
                return Err(self.unexpected("`=`"));
            }
            return Ok(StmtKind::Expr(target));
        };
        if op != AssignOp::Assign && matches!(target.kind, ExprKind::Tuple(_)) {
            return Err(self.error_at(self.span(), "compound assignment needs a single target"));
        }
        self.bump();
        let value = tuple_of(self.expr_list()?);
        Ok(StmtKind::Assign { target, op, value })
    }

    /// At `++` / `--` (two adjacent `+` or `-`) that ends the statement.
    fn at_incdec(&self) -> bool {
        let (a, b) = (&self.tokens[self.pos], self.tokens.get(self.pos + 1));
        matches!(a.kind, TokenKind::Plus | TokenKind::Minus)
            && b.is_some_and(|b| b.kind == a.kind && b.span.start == a.span.end)
            && matches!(self.peek_n(2), TokenKind::Semi | TokenKind::RBrace | TokenKind::Eof)
    }

    /// `i++` / `i--`: Zell has no such operator (DESIGN.md «Операторы»).
    /// Reported with a fix to `i += 1`, and parsed as that assignment so
    /// nothing else is reported.
    fn incdec(&mut self, target: Expr) -> StmtKind {
        let first = self.bump();
        let second = self.bump();
        let (op, sym, word) = match first.kind {
            TokenKind::Plus => (AssignOp::Add, "++", "+="),
            _ => (AssignOp::Sub, "--", "-="),
        };
        let span = first.span.to(second.span);
        let name = path_text(&target).unwrap_or_else(|| "x".into());
        if !self.silent {
            let msg = format!("Zell has no {sym}; write {name} {word} 1");
            let mut d = Diagnostic::error(msg, span).with_code(crate::diagnostics::codes::SYNTAX);
            d.fixes.push(Fix::edit(format!("Replace with `{word} 1`"), span, format!(" {word} 1")));
            self.diags.push(d);
            self.error_count += 1;
            self.last_error_at = Some(span.start);
        }
        let value = Expr { kind: ExprKind::Int(1), span };
        StmtKind::Assign { target, op, value }
    }

    fn expr_list(&mut self) -> PResult<Vec<Expr>> {
        let mut exprs = vec![self.parse_expr()?];
        while self.eat(&TokenKind::Comma) {
            exprs.push(self.parse_expr()?);
        }
        Ok(exprs)
    }

    /// `if cond { } [else if ... | else { }]`
    fn if_stmt(&mut self) -> PResult<IfStmt> {
        let start = self.expect(&TokenKind::If)?;
        let cond = self.with_struct_lit(false, |p| p.parse_expr())?;
        let then = self.block()?;
        if self.at(&TokenKind::Semi) && self.peek_n(1) == &TokenKind::Else {
            self.bump();
        }
        let else_ = if self.eat(&TokenKind::Else) {
            Some(Box::new(if self.at(&TokenKind::If) {
                ElseBranch::If(self.if_stmt()?)
            } else {
                ElseBranch::Block(self.block()?)
            }))
        } else {
            None
        };
        Ok(IfStmt { cond, then, else_, span: self.span_from(start) })
    }

    /// `for {}`, `for cond {}`, `for x in it {}`, `for i, x in it {}`.
    fn for_stmt(&mut self) -> PResult<ForStmt> {
        let start = self.expect(&TokenKind::For)?;
        let is_binding = |k: &TokenKind| matches!(k, TokenKind::Ident(_) | TokenKind::Underscore);
        // `in` is a contextual keyword: an identifier spelled `in` right
        // after the loop variables (`for in in ins` is a loop over `ins`).
        let is_in = |k: &TokenKind| matches!(k, TokenKind::Ident(s) if s == "in");
        let kind = if self.at(&TokenKind::LBrace) {
            ForKind::Infinite
        } else if self.at_binding()
            && (is_in(self.peek_n(1))
                || (self.peek_n(1) == &TokenKind::Comma && is_binding(self.peek_n(2)) && is_in(self.peek_n(3))))
        {
            let mut vars = vec![self.binding()?];
            if self.eat(&TokenKind::Comma) {
                vars.push(self.binding()?);
            }
            if !is_in(self.peek()) {
                return Err(self.unexpected("`in`"));
            }
            self.bump();
            let iter = self.with_struct_lit(false, |p| p.range_expr())?;
            ForKind::In { vars, iter }
        } else {
            ForKind::While(self.with_struct_lit(false, |p| p.parse_expr())?)
        };
        let body = self.block()?;
        Ok(ForStmt { kind, body, span: self.span_from(start) })
    }

    /// `switch [subject] { case p, q: stmts ... default: stmts }`
    fn switch_stmt(&mut self) -> PResult<SwitchStmt> {
        let start = self.expect(&TokenKind::Switch)?;
        let subject =
            if self.at(&TokenKind::LBrace) { None } else { Some(self.with_struct_lit(false, |p| p.parse_expr())?) };
        self.expect(&TokenKind::LBrace)?;
        let case_end = |k: &TokenKind| matches!(k, TokenKind::Case | TokenKind::Default | TokenKind::RBrace);
        let mut cases = Vec::new();
        let mut default: Option<Vec<Stmt>> = None;
        loop {
            self.skip_semis();
            match self.peek() {
                TokenKind::RBrace | TokenKind::Eof => break,
                TokenKind::Case => {
                    let case_start = self.bump().span;
                    match self.case_patterns() {
                        Ok(patterns) => {
                            let body = self.with_struct_lit(true, |p| p.stmt_list(case_end));
                            cases.push(Case { patterns, body, span: self.span_from(case_start) });
                        }
                        Err(_) => {
                            // Skip the broken header; still parse the body for its own errors.
                            while !matches!(self.peek(), TokenKind::Colon | TokenKind::Case | TokenKind::Default | TokenKind::RBrace | TokenKind::Eof) {
                                self.bump();
                            }
                            if self.eat(&TokenKind::Colon) {
                                self.with_struct_lit(true, |p| p.stmt_list(case_end));
                            }
                        }
                    }
                }
                TokenKind::Default => {
                    let span = self.bump().span;
                    if self.expect(&TokenKind::Colon).is_err() {
                        self.sync_stmt();
                        continue;
                    }
                    let body = self.with_struct_lit(true, |p| p.stmt_list(case_end));
                    if default.is_some() {
                        self.error_at(span, "multiple `default` clauses in switch");
                    } else {
                        default = Some(body);
                    }
                }
                _ if self.at_decl_start() => break,
                _ => {
                    let before = self.pos;
                    self.unexpected("`case`, `default` or `}`");
                    self.sync_stmt();
                    if self.pos == before && !self.at(&TokenKind::RBrace) {
                        self.bump();
                    }
                }
            }
        }
        self.close_brace()?;
        Ok(SwitchStmt { subject, cases, default, span: self.span_from(start) })
    }

    /// `p {, p} :`
    fn case_patterns(&mut self) -> PResult<Vec<Pattern>> {
        let mut patterns = vec![self.pattern()?];
        while self.eat(&TokenKind::Comma) {
            patterns.push(self.pattern()?);
        }
        self.expect(&TokenKind::Colon)?;
        Ok(patterns)
    }

    /// `expr`, `a..b`, or `Type name`.
    fn pattern(&mut self) -> PResult<Pattern> {
        let start = self.span();
        if starts_type(self.peek()) {
            if let Some((ty, name)) = self.try_type_bind() {
                return Ok(Pattern { kind: PatternKind::TypeBind { ty, name }, span: self.span_from(start) });
            }
        }
        let kind = match self.with_struct_lit(true, |p| p.range_parts())? {
            Ok(expr) => PatternKind::Expr(expr),
            Err((start, end)) => PatternKind::Range(start, end),
        };
        Ok(Pattern { kind, span: self.span_from(start) })
    }

    /// Speculatively parses `Type name` followed by `:` or `,`.
    fn try_type_bind(&mut self) -> Option<(TypeExpr, Ident)> {
        let cp = self.checkpoint();
        let silent = std::mem::replace(&mut self.silent, true);
        let ty = self.parse_type().ok().filter(|_| {
            self.at_binding() && matches!(self.peek_n(1), TokenKind::Colon | TokenKind::Comma)
        });
        let result = match ty {
            Some(ty) => self.binding().ok().map(|name| (ty, name)),
            None => None,
        };
        self.silent = silent;
        if result.is_none() {
            self.restore(cp);
        }
        result
    }

    // =====================================================================
    // Expressions
    // =====================================================================

    /// A full expression (no ranges).
    fn parse_expr(&mut self) -> PResult<Expr> {
        self.binary(1)
    }

    /// An expression or a range `a..b`, `a..`, `..b`, `..`.
    fn range_expr(&mut self) -> PResult<Expr> {
        let first = self.span();
        let (start, end) = match self.range_parts()? {
            Ok(expr) => return Ok(expr),
            Err(parts) => parts,
        };
        let span = self.span_from(first);
        Ok(Expr { kind: ExprKind::Range(start.map(Box::new), end.map(Box::new)), span })
    }

    /// Parses an expression, or the two (optional) ends of a range.
    #[allow(clippy::type_complexity)]
    fn range_parts(&mut self) -> PResult<Result<Expr, (Option<Expr>, Option<Expr>)>> {
        let start = if self.at(&TokenKind::DotDot) { None } else { Some(self.parse_expr()?) };
        if !self.eat(&TokenKind::DotDot) {
            return Ok(Ok(start.expect("parsed above")));
        }
        let end = if at_expr_end(self.peek()) || self.at(&TokenKind::LBrace) { None } else { Some(self.parse_expr()?) };
        Ok(Err((start, end)))
    }

    /// Precedence climbing over binary operators with precedence >= `min_prec`.
    fn binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut left = self.unary()?;
        while let Some(op) = binary_op(self.peek()) {
            let prec = op.precedence();
            if prec < min_prec || self.at_incdec() {
                break;
            }
            self.bump();
            let right = self.binary(prec + 1)?;
            let span = left.span.to(right.span);
            left = Expr { kind: ExprKind::Binary { op, left: Box::new(left), right: Box::new(right) }, span };
        }
        Ok(left)
    }

    /// Prefix operators, `await` and `try`, then postfix expressions.
    fn unary(&mut self) -> PResult<Expr> {
        self.nested(|p| {
            let start = p.span();
            let op = match p.peek() {
                TokenKind::Minus => Some(UnaryOp::Neg),
                TokenKind::Bang => Some(UnaryOp::Not),
                TokenKind::Amp => Some(UnaryOp::AddrOf),
                TokenKind::Star => Some(UnaryOp::Deref),
                _ => None,
            };
            if let Some(op) = op {
                p.bump();
                let expr = p.unary()?;
                return Ok(Expr { kind: ExprKind::Unary { op, expr: Box::new(expr) }, span: p.span_from(start) });
            }
            match p.peek() {
                TokenKind::Await => {
                    p.bump();
                    // `await f()?` is `(await f())?`: the operand stops before `?`.
                    let operand = if matches!(
                        p.peek(),
                        TokenKind::Await | TokenKind::Minus | TokenKind::Bang | TokenKind::Amp | TokenKind::Star
                    ) {
                        p.unary()?
                    } else {
                        let primary = p.primary()?;
                        p.postfix(primary, true)?
                    };
                    let awaited = Expr { kind: ExprKind::Await(Box::new(operand)), span: p.span_from(start) };
                    p.postfix(awaited, false)
                }
                TokenKind::Try => p.try_expr(),
                _ => {
                    let primary = p.primary()?;
                    p.postfix(primary, false)
                }
            }
        })
    }

    /// `try e catch name { }` / `try e else value|continue|break|return [vals]`
    fn try_expr(&mut self) -> PResult<Expr> {
        let start = self.expect(&TokenKind::Try)?;
        let expr = self.parse_expr()?;
        if self.at(&TokenKind::Semi) && matches!(self.peek_n(1), TokenKind::Catch | TokenKind::Else) {
            self.bump();
        }
        let handler = match self.peek() {
            TokenKind::Catch => {
                self.bump();
                let name = self.binding()?;
                let body = self.block()?;
                TryHandler::Catch { name, body }
            }
            TokenKind::Else => {
                self.bump();
                let kw = self.span();
                let value = match self.peek() {
                    TokenKind::Continue => {
                        self.bump();
                        ElseValue::Continue(kw)
                    }
                    TokenKind::Break => {
                        self.bump();
                        ElseValue::Break(kw)
                    }
                    TokenKind::Return => {
                        self.bump();
                        let values = self.return_values()?;
                        ElseValue::Return(values, self.span_from(kw))
                    }
                    _ => ElseValue::Expr(self.parse_expr()?),
                };
                TryHandler::Else(Box::new(value))
            }
            _ => return Err(self.unexpected("`catch` or `else`")),
        };
        Ok(Expr { kind: ExprKind::Try { expr: Box::new(expr), handler }, span: self.span_from(start) })
    }

    /// Postfix operators: `.name`, calls, indexing, `?`/`?(ctx)`, struct
    /// literals. With `stop_at_question`, stops before the first `?`.
    fn postfix(&mut self, mut expr: Expr, stop_at_question: bool) -> PResult<Expr> {
        loop {
            let start = expr.span;
            let kind = match self.peek() {
                TokenKind::Dot => {
                    let dot = self.bump().span;
                    let name = if self.silent || self.field_name_ahead(dot.end) {
                        self.field_name()?
                    } else {
                        // `x.` being typed: an empty name right after the dot.
                        self.unexpected("field name");
                        Ident { name: String::new(), span: Span::new(dot.end, dot.end) }
                    };
                    ExprKind::Field { base: Box::new(expr), name }
                }
                TokenKind::LParen => {
                    let args = self.call_args()?;
                    ExprKind::Call { callee: Box::new(expr), args }
                }
                TokenKind::LBracket => {
                    self.bump();
                    let index = self.with_struct_lit(true, |p| p.range_expr())?;
                    self.expect(&TokenKind::RBracket)?;
                    ExprKind::Index { base: Box::new(expr), index: Box::new(index) }
                }
                TokenKind::Question if !stop_at_question => {
                    self.bump();
                    let context = if self.eat(&TokenKind::LParen) {
                        let ctx = self.with_struct_lit(true, |p| p.parse_expr())?;
                        self.expect(&TokenKind::RParen)?;
                        Some(Box::new(ctx))
                    } else {
                        None
                    };
                    ExprKind::Propagate { expr: Box::new(expr), context }
                }
                TokenKind::LBrace if !self.no_struct_lit => match struct_lit_path(&expr) {
                    Some(path) => self.lit_body(path, Vec::new())?,
                    None => return Ok(expr),
                },
                TokenKind::Lt if is_name_path(&expr) => {
                    match self.try_type_args() {
                        Some(args) if self.at(&TokenKind::LBrace) => match struct_lit_path(&expr) {
                            Some(path) if !self.no_struct_lit => {
                                self.lit_body(path, args)?
                            }
                            _ => ExprKind::TypeArgs { base: Box::new(expr), args },
                        },
                        Some(args) => ExprKind::TypeArgs { base: Box::new(expr), args },
                        None => return Ok(expr),
                    }
                }
                _ => return Ok(expr),
            };
            expr = Expr { kind, span: self.span_from(start) };
        }
    }

    /// Speculatively parses explicit type arguments `<T, U>`; succeeds only
    /// if they are followed by `(` (a generic call), `{` (a struct
    /// literal), `.` (a member of a generic type) or a token that ends an
    /// operand (a generic function value), as in C#. Otherwise restores the position (`a < b` stays a
    /// comparison).
    fn try_type_args(&mut self) -> Option<Vec<TypeExpr>> {
        let cp = self.checkpoint();
        let silent = std::mem::replace(&mut self.silent, true);
        let ok = (|| {
            self.expect(&TokenKind::Lt).ok()?;
            let args = self.type_list(&TokenKind::Gt).ok()?;
            if args.is_empty() {
                return None;
            }
            self.expect_gt().ok()?;
            // C#-style disambiguation: `F<int>(x)`, `S<int>{}`, `E<int>.V`,
            // and a generic function value `F<int>` before `)`, `]`, `,`,
            // `}` or the end of the statement.
            matches!(
                self.peek(),
                TokenKind::LParen | TokenKind::LBrace | TokenKind::Dot | TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace | TokenKind::Comma | TokenKind::Semi | TokenKind::Eof
            )
            .then_some(args)
        })();
        self.silent = silent;
        if ok.is_none() {
            self.restore(cp);
        }
        ok
    }

    /// Can [`Parser::field_name`] succeed at the current token?
    fn field_name_ahead(&self, dot_end: u32) -> bool {
        match self.peek() {
            TokenKind::Ident(_) | TokenKind::Int(_) => true,
            // Statement keywords are member names (fields named by keywords)
            // only right after the dot: `c.` typed at a line end before
            // `var d = 2` recovers as an empty Field (`c.` + newline +
            // `Inc()` still joins, as in Go).
            TokenKind::Var | TokenKind::Const | TokenKind::If | TokenKind::For | TokenKind::Switch | TokenKind::Return | TokenKind::Defer | TokenKind::Break | TokenKind::Continue => {
                self.span().start == dot_end
            }
            k => k.text().chars().all(|c| c.is_ascii_lowercase()),
        }
    }

    /// Name after `.`: an identifier, a keyword used as a name, or a tuple index.
    fn field_name(&mut self) -> PResult<Ident> {
        let tok = self.peek().clone();
        let name = match &tok {
            TokenKind::Ident(name) | TokenKind::Int(name) => name.clone(),
            k if k.text().chars().all(|c| c.is_ascii_lowercase()) => k.text().to_string(),
            _ => return Err(self.unexpected("field name")),
        };
        let span = self.bump().span;
        Ok(Ident { name, span })
    }

    /// `(a, b, c)` with optional trailing comma.
    fn call_args(&mut self) -> PResult<Vec<Expr>> {
        self.expect(&TokenKind::LParen)?;
        self.with_struct_lit(true, |p| {
            p.recovering_list(
                &TokenKind::RParen,
                |p| {
                    if p.attr_args && p.at_field_name_colon() {
                        let name = p.field_decl_name()?;
                        p.bump(); // `:`
                        let value = p.parse_expr()?;
                        let span = name.span.to(value.span);
                        return Ok(Expr { kind: ExprKind::NamedArg { name, value: Box::new(value) }, span });
                    }
                    let e = p.parse_expr()?;
                    if p.at(&TokenKind::Ellipsis) {
                        let end = p.bump().span;
                        let span = e.span.to(end);
                        return Ok(Expr { kind: ExprKind::Spread(Box::new(e)), span });
                    }
                    Ok(e)
                },
                Expr::error,
            )
        })
    }

    /// `{ Name: value, ... }` after a struct literal's path.
    /// The `{...}` after a type name: `Name: value` fields (a struct
    /// literal, or a map literal through an alias with variable keys), or
    /// else `key: value` / `value` elements of a map or slice literal
    /// through an alias. A malformed element is skipped up to the next `,`
    /// or the closing `}`, so the rest of the function still parses.
    fn lit_body(&mut self, path: Path, generics: Vec<TypeExpr>) -> PResult<ExprKind> {
        self.expect(&TokenKind::LBrace)?;
        let mut fields: Vec<FieldInit> = Vec::new();
        let mut elems: Vec<(Option<Expr>, Expr)> = Vec::new();
        let mut keyed_only = true;
        self.with_struct_lit(true, |p| {
            loop {
                p.skip_semis();
                if p.at(&TokenKind::RBrace) || p.at_eof() {
                    break;
                }
                let elem = (|| -> PResult<()> {
                    if p.at_field_name_colon() {
                        let name = p.field_decl_name()?;
                        p.expect(&TokenKind::Colon)?;
                        let value = p.parse_expr()?;
                        fields.push(FieldInit { span: name.span.to(value.span), name: name.clone(), value: value.clone() });
                        elems.push((Some(Expr { span: name.span, kind: ExprKind::Ident(name) }), value));
                    } else {
                        keyed_only = false;
                        let first = p.parse_expr()?;
                        if p.eat(&TokenKind::Colon) {
                            let value = p.parse_expr()?;
                            elems.push((Some(first), value));
                        } else {
                            elems.push((None, first));
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = elem {
                    if p.silent {
                        return Err(e);
                    }
                    p.skip_list_elem(&TokenKind::RBrace);
                }
                p.skip_semis();
                if p.eat(&TokenKind::Comma) {
                    continue;
                }
                if p.at(&TokenKind::RBrace) || p.at_eof() {
                    break;
                }
                let e = p.unexpected("`,` or `}`");
                if p.silent {
                    return Err(e);
                }
                p.skip_list_elem(&TokenKind::RBrace);
                if !p.eat(&TokenKind::Comma) && !p.at(&TokenKind::RBrace) {
                    // a line break inside the literal: go on with the next line
                    if !p.eat(&TokenKind::Semi) {
                        break;
                    }
                }
            }
            p.skip_semis();
            Ok(())
        })?;
        self.expect(&TokenKind::RBrace)?;
        Ok(if keyed_only { ExprKind::StructLit { path, generics, fields } } else { ExprKind::CompositeLit { path, generics, elems } })
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let kind = match self.peek().clone() {
            TokenKind::Int(text) => {
                self.bump();
                ExprKind::Int(self.int_value(&text, start))
            }
            TokenKind::Float(text) => {
                self.bump();
                match text.parse::<f64>() {
                    Ok(v) => ExprKind::Float(v),
                    Err(_) => {
                        self.error_at(start, format!("invalid float literal `{text}`"));
                        ExprKind::Float(0.0)
                    }
                }
            }
            TokenKind::Str(s) => {
                self.bump();
                ExprKind::Str(s)
            }
            TokenKind::Char(c) => {
                self.bump();
                ExprKind::Char(c)
            }
            TokenKind::True | TokenKind::False => ExprKind::Bool(self.bump().kind == TokenKind::True),
            TokenKind::Nil => {
                self.bump();
                ExprKind::Nil
            }
            TokenKind::This => {
                self.bump();
                ExprKind::This
            }
            TokenKind::Ident(_) | TokenKind::Underscore if self.peek_n(1) == &TokenKind::Arrow => return self.bare_lambda(),
            TokenKind::Ident(_) | TokenKind::Underscore => ExprKind::Ident(self.binding()?),
            TokenKind::LParen => return self.paren_or_lambda(),
            TokenKind::LBracket => {
                self.bump();
                let elems = self.with_struct_lit(true, |p| p.recovering_list(&TokenKind::RBracket, |p| p.parse_expr(), Expr::error))?;
                ExprKind::SliceLit(elems)
            }
            TokenKind::Map => self.map_lit()?,
            TokenKind::New => {
                self.bump();
                ExprKind::New(Box::new(self.new_operand()?))
            }
            TokenKind::Func => {
                self.bump();
                let params = self.params()?;
                let ret = if starts_type(self.peek()) { Some(Box::new(self.parse_type()?)) } else { None };
                if !self.at(&TokenKind::LBrace) {
                    return Err(self.unexpected("`{`"));
                }
                let body = Box::new(LambdaBody::Block(self.block()?));
                ExprKind::Lambda { params, ret, body, is_async: false }
            }
            TokenKind::Async
                if matches!(self.peek_n(1), TokenKind::Func | TokenKind::LParen)
                    || (matches!(self.peek_n(1), TokenKind::Ident(_) | TokenKind::Underscore) && self.peek_n(2) == &TokenKind::Arrow) =>
            {
                self.bump();
                let mut e = self.primary()?;
                match &mut e.kind {
                    ExprKind::Lambda { is_async, .. } => *is_async = true,
                    _ => return Err(self.error_at(start, "async must be followed by a lambda (async func(..) { }, async (..) => e or async x => e)")),
                }
                e.kind
            }
            _ => {
                let here = self.here();
                return self.recover("expression", Expr::error(here));
            }
        };
        Ok(Expr { kind, span: self.span_from(start) })
    }

    /// Operand of `new`: a (dotted) type name, optionally followed by a struct
    /// literal body (always allowed here), or any other primary expression.
    fn new_operand(&mut self) -> PResult<Expr> {
        if !matches!(self.peek(), TokenKind::Ident(_)) {
            return self.primary();
        }
        let path = self.path()?;
        let generics = if self.at(&TokenKind::Lt) { self.try_type_args().unwrap_or_default() } else { Vec::new() };
        if self.at(&TokenKind::LBrace) {
            let start = path.span;
            let kind = self.lit_body(path, generics)?;
            return Ok(Expr { span: self.span_from(start), kind });
        }
        if !generics.is_empty() {
            return Err(self.unexpected("`{`"));
        }
        let mut segments = path.segments.into_iter();
        let first = segments.next().expect("path has a segment");
        let mut expr = Expr { span: first.span, kind: ExprKind::Ident(first) };
        for name in segments {
            let span = expr.span.to(name.span);
            expr = Expr { kind: ExprKind::Field { base: Box::new(expr), name }, span };
        }
        Ok(expr)
    }

    /// `map[K]V{k: v, ...}`
    fn map_lit(&mut self) -> PResult<ExprKind> {
        self.expect(&TokenKind::Map)?;
        self.expect(&TokenKind::LBracket)?;
        let key_ty = self.parse_type()?;
        self.expect(&TokenKind::RBracket)?;
        let val_ty = self.parse_type()?;
        self.expect(&TokenKind::LBrace)?;
        let entries = self.with_struct_lit(true, |p| {
            let mut entries = Vec::new();
            loop {
                p.skip_semis();
                if p.at(&TokenKind::RBrace) || p.at_eof() {
                    break;
                }
                let key = p.parse_expr()?;
                p.expect(&TokenKind::Colon)?;
                let value = p.parse_expr()?;
                entries.push((key, value));
                p.skip_semis();
                if !p.eat(&TokenKind::Comma) {
                    break;
                }
            }
            p.skip_semis();
            Ok(entries)
        })?;
        self.expect(&TokenKind::RBrace)?;
        Ok(ExprKind::MapLit { key_ty: Box::new(key_ty), val_ty: Box::new(val_ty), entries })
    }

    /// `x => body`: a short lambda with one untyped parameter, no parentheses.
    fn bare_lambda(&mut self) -> PResult<Expr> {
        let start = self.span();
        let name = self.binding()?;
        let ty = TypeExpr { kind: TypeKind::Infer, span: name.span };
        let params = vec![Param { span: name.span, attrs: Vec::new(), name, ty }];
        self.expect(&TokenKind::Arrow)?;
        let body = if self.at(&TokenKind::LBrace) { LambdaBody::Block(self.block()?) } else { LambdaBody::Expr(self.parse_expr()?) };
        Ok(Expr { kind: ExprKind::Lambda { params, ret: None, body: Box::new(body), is_async: false }, span: self.span_from(start) })
    }

    /// `(e)`, `(a, b)` or a short lambda `(x int) => body`.
    fn paren_or_lambda(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.lambda_ahead() {
            let params = self.params_ext(true)?;
            let ret = if !self.at(&TokenKind::Arrow) && starts_type(self.peek()) {
                Some(Box::new(self.parse_type()?))
            } else {
                None
            };
            self.expect(&TokenKind::Arrow)?;
            let body = if self.at(&TokenKind::LBrace) {
                LambdaBody::Block(self.block()?)
            } else {
                LambdaBody::Expr(self.parse_expr()?)
            };
            return Ok(Expr {
                kind: ExprKind::Lambda { params, ret, body: Box::new(body), is_async: false },
                span: self.span_from(start),
            });
        }
        self.expect(&TokenKind::LParen)?;
        let mut elems = self.with_struct_lit(true, |p| {
            let mut elems = vec![p.parse_expr()?];
            while p.eat(&TokenKind::Comma) {
                if p.at(&TokenKind::RParen) {
                    break;
                }
                elems.push(p.parse_expr()?);
            }
            Ok(elems)
        })?;
        self.expect(&TokenKind::RParen)?;
        let kind = if elems.len() == 1 {
            ExprKind::Paren(Box::new(elems.pop().unwrap()))
        } else {
            ExprKind::Tuple(elems)
        };
        Ok(Expr { kind, span: self.span_from(start) })
    }

    /// At `(`: does a short lambda start here? True if the matching `)` is
    /// followed by `=>`, or if the parenthesis starts with `name type`.
    fn lambda_ahead(&self) -> bool {
        let is_name = |k: &TokenKind| matches!(k, TokenKind::Ident(_) | TokenKind::Underscore);
        if is_name(self.peek_n(1)) && matches!(self.peek_n(2), TokenKind::Ident(_) | TokenKind::Map | TokenKind::Func) {
            return true;
        }
        let mut depth = 0usize;
        for i in self.pos..self.tokens.len() {
            match self.tokens[i].kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        return self.tokens.get(i + 1).map(|t| &t.kind) == Some(&TokenKind::Arrow);
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
        }
        false
    }

    /// Value of an integer literal's text (`123`, `0xff`, `0b1`, `0o7`).
    fn int_value(&mut self, text: &str, span: Span) -> u64 {
        let lower = text.to_ascii_lowercase();
        let (digits, radix) = match lower.get(..2) {
            Some("0x") => (&text[2..], 16),
            Some("0b") => (&text[2..], 2),
            Some("0o") => (&text[2..], 8),
            _ => (text, 10),
        };
        match u64::from_str_radix(digits, radix) {
            Ok(v) => v,
            Err(_) => {
                self.error_at(span, format!("integer literal `{text}` is too large"));
                0
            }
        }
    }
}

// =========================================================================
// Token classification
// =========================================================================

/// Is this token a keyword (spelled like an identifier)?
fn is_keyword(kind: &TokenKind) -> bool {
    !matches!(kind, TokenKind::Ident(_) | TokenKind::Underscore) && TokenKind::keyword(kind.text()).is_some()
}

/// Can this token begin a type?
fn starts_type(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident(_) | TokenKind::Star | TokenKind::LBracket | TokenKind::Map | TokenKind::Func | TokenKind::LParen
    )
}

/// Tokens after which an optional expression list (e.g. after `return`) is absent.
fn at_expr_end(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Semi
            | TokenKind::RBrace
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::Comma
            | TokenKind::Colon
            | TokenKind::Case
            | TokenKind::Default
            | TokenKind::Eof
    )
}

fn binary_op(kind: &TokenKind) -> Option<BinaryOp> {
    use BinaryOp::*;
    Some(match kind {
        TokenKind::Plus => Add,
        TokenKind::Minus => Sub,
        TokenKind::Star => Mul,
        TokenKind::Slash => Div,
        TokenKind::Percent => Rem,
        // `@` between operands; attributes only start declarations.
        TokenKind::At => MatMul,
        TokenKind::Amp => BitAnd,
        TokenKind::Pipe => BitOr,
        TokenKind::Caret => BitXor,
        TokenKind::Shl => Shl,
        TokenKind::Shr => Shr,
        TokenKind::EqEq => Eq,
        TokenKind::NotEq => Ne,
        TokenKind::Lt => Lt,
        TokenKind::Le => Le,
        TokenKind::Gt => Gt,
        TokenKind::Ge => Ge,
        TokenKind::AndAnd => And,
        TokenKind::OrOr => Or,
        _ => return None,
    })
}

/// `a.b.c` of a dotted name (`this.n`), for messages.
fn path_text(e: &Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Ident(id) => Some(id.name.clone()),
        ExprKind::This => Some("this".into()),
        ExprKind::Field { base, name } => Some(format!("{}.{}", path_text(base)?, name.name)),
        _ => None,
    }
}

fn assign_op(kind: &TokenKind) -> Option<AssignOp> {
    use AssignOp::*;
    Some(match kind {
        TokenKind::Assign => Assign,
        TokenKind::PlusEq => Add,
        TokenKind::MinusEq => Sub,
        TokenKind::StarEq => Mul,
        TokenKind::SlashEq => Div,
        TokenKind::PercentEq => Rem,
        TokenKind::AtEq => MatMul,
        TokenKind::AmpEq => BitAnd,
        TokenKind::PipeEq => BitOr,
        TokenKind::CaretEq => BitXor,
        TokenKind::ShlEq => Shl,
        TokenKind::ShrEq => Shr,
        _ => return None,
    })
}

/// If `expr` is a dotted name (`Point`, `app.models.User`, `tag`), the path
/// of a struct literal. Lowercase names are allowed too (attribute structs
/// such as `range` are lowercase, and grammar/zell.bnf accepts them); a `{`
/// after a name can only start a literal outside `if`/`for`/`switch` headers.
fn struct_lit_path(expr: &Expr) -> Option<Path> {
    fn collect(expr: &Expr, out: &mut Vec<Ident>) -> bool {
        match &expr.kind {
            ExprKind::Ident(id) if id.name != "_" => {
                out.push(id.clone());
                true
            }
            ExprKind::Field { base, name } => {
                collect(base, out) && {
                    out.push(name.clone());
                    true
                }
            }
            _ => false,
        }
    }
    let mut segments = Vec::new();
    if !collect(expr, &mut segments) {
        return None;
    }
    Some(Path { segments, span: expr.span })
}

/// Is `expr` a plain dotted name (`f`, `reflect.TypeOf`, `x.Get`)?
fn is_name_path(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Ident(id) => id.name != "_",
        // `x.Fields[1].Attribute<A>()`, `parse(s).Get<T>()`: any postfix
        // chain before the last `.name` (try_type_args still decides).
        ExprKind::Field { base, .. } => {
            matches!(base.kind, ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Call { .. } | ExprKind::This | ExprKind::Index { .. } | ExprKind::TypeArgs { .. } | ExprKind::Paren(_))
        }
        _ => false,
    }
}

/// One expression stays as is; several become a `Tuple`.
fn tuple_of(mut exprs: Vec<Expr>) -> Expr {
    if exprs.len() == 1 {
        return exprs.pop().unwrap();
    }
    let span = exprs[0].span.to(exprs[exprs.len() - 1].span);
    Expr { kind: ExprKind::Tuple(exprs), span }
}
