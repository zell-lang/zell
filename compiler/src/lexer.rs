//! Lexer: source text -> tokens.
//!
//! Besides ordinary tokenization the lexer performs Go-style automatic
//! semicolon insertion: at a newline (or at end of file) a `Semi` token is
//! emitted when the previous token can end a statement
//! (see [`TokenKind::ends_statement`]). Unlike Go, no semicolons are inserted
//! while directly inside `( )` or `[ ]`, so argument lists and slice literals
//! may span lines without trailing commas. Inside `{ }` the normal rule applies.
//! A line whose first token is `.` continues the previous line: the
//! semicolon inserted at the newline before it is dropped, so method chains
//! may break before the dot.
//!
//! **Trivia** ([`lex_with_trivia`]): comments and runs of newlines are
//! recorded and attached to the following real token as its *leading
//! trivia* (automatic semicolons are zero-width and get none; trivia at the
//! end of the file belongs to `Eof`). The formatter uses it to keep
//! comments and blank lines; the token stream is the same as [`lex`]'s.

use crate::diagnostics::Diagnostics;
use crate::span::Span;
use crate::token::{Token, TokenKind};

/// Tokenizes `src`. Lexical errors are reported to `diags` and lexing
/// continues; the result always ends with an `Eof` token.
pub fn lex(src: &str, diags: &mut Diagnostics) -> Vec<Token> {
    lex_with_trivia(src, diags).tokens
}

/// Like [`lex`], but also returns the spans of all comments (`// ...` up to
/// the end of the line, and `/* ... */`), in source order. The token stream
/// is identical to [`lex`]'s.
pub fn lex_with_comments(src: &str, diags: &mut Diagnostics) -> (Vec<Token>, Vec<Span>) {
    let l = lex_with_trivia(src, diags);
    let comments = l.trivia.iter().filter(|t| t.is_comment()).map(|t| t.span).collect();
    (l.tokens, comments)
}

/// A piece of source between tokens that tools must preserve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriviaKind {
    /// `// ...` up to (not including) the end of the line.
    LineComment,
    /// `/* ... */` (nestable).
    BlockComment,
    /// A run of whitespace containing `n >= 1` newlines (`n >= 2` means at
    /// least one blank line). Whitespace without newlines is not recorded.
    Newlines(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trivia {
    pub kind: TriviaKind,
    pub span: Span,
}

impl Trivia {
    pub fn is_comment(&self) -> bool {
        matches!(self.kind, TriviaKind::LineComment | TriviaKind::BlockComment)
    }
}

/// Tokens plus their leading trivia (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct Lexed {
    pub tokens: Vec<Token>,
    /// All trivia in source order.
    pub trivia: Vec<Trivia>,
    /// `leading[i]` is the range of `trivia` that precedes `tokens[i]`.
    pub leading: Vec<std::ops::Range<usize>>,
}

impl Lexed {
    /// Leading trivia of token `i`.
    pub fn leading(&self, i: usize) -> &[Trivia] {
        self.leading.get(i).map_or(&[], |r| &self.trivia[r.clone()])
    }

    /// Index of the first token starting at or after `offset` (binary search).
    pub fn token_at(&self, offset: u32) -> usize {
        self.tokens.partition_point(|t| t.span.start < offset)
    }
}

/// Like [`lex`], also recording trivia (comments and newline runs) as the
/// leading trivia of each token.
pub fn lex_with_trivia(src: &str, diags: &mut Diagnostics) -> Lexed {
    let mut lexer = Lexer {
        src,
        pos: 0,
        tokens: Vec::new(),
        delims: Vec::new(),
        diags,
        trivia: Vec::new(),
        pending_from: 0,
        leading: Vec::new(),
        newlines: None,
    };
    lexer.run();
    Lexed { tokens: lexer.tokens, trivia: lexer.trivia, leading: lexer.leading }
}

struct Lexer<'a, 'd> {
    src: &'a str,
    /// Current byte offset.
    pos: usize,
    tokens: Vec<Token>,
    /// Stack of currently open `(`, `[` and `{`.
    delims: Vec<TokenKind>,
    diags: &'d mut Diagnostics,
    /// Trivia recorded so far.
    trivia: Vec<Trivia>,
    /// Start of the trivia not yet attached to a token.
    pending_from: usize,
    /// Leading trivia range of each token.
    leading: Vec<std::ops::Range<usize>>,
    /// Current newline run: (start offset, count, end offset).
    newlines: Option<(usize, u32, usize)>,
}

impl Lexer<'_, '_> {
    fn run(&mut self) {
        loop {
            self.skip_trivia();
            let Some(c) = self.peek() else { break };
            let start = self.pos;
            let kind = if is_ident_start(c) {
                self.ident_or_keyword()
            } else if c.is_ascii_digit() {
                self.number()
            } else if c == '"' {
                self.string()
            } else if c == '`' {
                self.raw_string()
            } else if c == '\'' {
                self.char_lit()
            } else {
                match self.punct() {
                    Some(k) => k,
                    None => continue,
                }
            };
            self.track_delims(&kind);
            self.push(kind, start);
        }
        self.insert_semi();
        let end = self.src.len() as u32;
        self.attach_trivia();
        self.tokens.push(Token { kind: TokenKind::Eof, span: Span::new(end, end) });
    }

    /// Attaches the pending trivia to the token about to be pushed.
    fn attach_trivia(&mut self) {
        self.flush_newlines();
        self.leading.push(self.pending_from..self.trivia.len());
        self.pending_from = self.trivia.len();
    }

    fn flush_newlines(&mut self) {
        if let Some((start, n, end)) = self.newlines.take() {
            self.trivia.push(Trivia { kind: TriviaKind::Newlines(n), span: Span::new(start as u32, end as u32) });
        }
    }

    // ----- helpers -------------------------------------------------------

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        // A line starting with `.` continues the previous one (method chains
        // broken before the dot): drop the automatic `;` inserted at the
        // newline. Automatic semicolons are the only zero-width `Semi`s.
        if kind == TokenKind::Dot
            && self.tokens.last().is_some_and(|t| t.kind == TokenKind::Semi && t.span.start == t.span.end)
        {
            self.tokens.pop();
            self.leading.pop();
        }
        self.attach_trivia();
        self.tokens.push(Token { kind, span: Span::new(start as u32, self.pos as u32) });
    }

    fn error(&mut self, msg: impl Into<String>, start: usize, end: usize) {
        self.diags.error(msg, Span::new(start as u32, end as u32));
    }

    fn track_delims(&mut self, kind: &TokenKind) {
        match kind {
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => self.delims.push(kind.clone()),
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                self.delims.pop();
            }
            _ => {}
        }
    }

    /// Emits an automatic `Semi` (zero-width, right after the previous token)
    /// if the previous token can end a statement and we are not inside `()`/`[]`.
    fn insert_semi(&mut self) {
        if matches!(self.delims.last(), Some(TokenKind::LParen | TokenKind::LBracket)) {
            return;
        }
        let n = self.tokens.len();
        if let Some(last) = self.tokens.last() {
            // A keyword right after `.` is used as a name (`namespace examples.async`).
            let keyword_name = n >= 2
                && self.tokens[n - 2].kind == TokenKind::Dot
                && TokenKind::keyword(last.kind.text()).is_some();
            // `i++` / `i--` at the end of a line (no such operator: the
            // parser reports it, and the next line stays a new statement).
            let incdec = n >= 2
                && matches!(last.kind, TokenKind::Plus | TokenKind::Minus)
                && self.tokens[n - 2].kind == last.kind
                && self.tokens[n - 2].span.end == last.span.start;
            if last.kind.ends_statement() || keyword_name || incdec {
                let end = last.span.end;
                // Zero-width: no trivia of its own.
                self.leading.push(self.pending_from..self.pending_from);
                self.tokens.push(Token { kind: TokenKind::Semi, span: Span::new(end, end) });
            }
        }
    }

    /// Skips whitespace and comments, inserting semicolons at newlines.
    fn skip_trivia(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                '\n' => {
                    self.insert_semi();
                    let at = self.pos;
                    self.newlines = Some(match self.newlines {
                        Some((s, n, _)) => (s, n + 1, at + 1),
                        None => (at, 1, at + 1),
                    });
                    self.pos += 1;
                }
                c if c.is_whitespace() => {
                    self.bump();
                }
                '/' if self.peek_at(1) == Some('/') => {
                    self.flush_newlines();
                    let start = self.pos;
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                    self.trivia.push(Trivia { kind: TriviaKind::LineComment, span: Span::new(start as u32, self.pos as u32) });
                }
                '/' if self.peek_at(1) == Some('*') => {
                    self.flush_newlines();
                    self.block_comment()
                }
                _ => break,
            }
        }
    }

    /// `/* ... */`, nestable. A comment spanning lines acts like a newline.
    fn block_comment(&mut self) {
        let start = self.pos;
        self.pos += 2;
        let mut depth = 1;
        let mut saw_newline = false;
        while depth > 0 {
            match self.bump() {
                None => {
                    self.error("unterminated block comment", start, start + 2);
                    break;
                }
                Some('\n') => saw_newline = true,
                Some('*') if self.eat('/') => depth -= 1,
                Some('/') if self.eat('*') => depth += 1,
                Some(_) => {}
            }
        }
        self.trivia.push(Trivia { kind: TriviaKind::BlockComment, span: Span::new(start as u32, self.pos as u32) });
        if saw_newline {
            self.insert_semi();
        }
    }

    // ----- token classes -------------------------------------------------

    fn ident_or_keyword(&mut self) -> TokenKind {
        let start = self.pos;
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        let text = &self.src[start..self.pos];
        TokenKind::keyword(text).unwrap_or_else(|| TokenKind::Ident(text.to_string()))
    }

    /// Integer (decimal, `0x`, `0b`, `0o`) or float literal; `_` separators allowed.
    fn number(&mut self) -> TokenKind {
        let start = self.pos;
        let radix = match (self.peek(), self.peek_at(1)) {
            (Some('0'), Some('x' | 'X')) => 16,
            (Some('0'), Some('b' | 'B')) => 2,
            (Some('0'), Some('o' | 'O')) => 8,
            _ => 10,
        };
        if radix != 10 {
            self.pos += 2;
            let digits_start = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                self.bump();
            }
            let digits = &self.src[digits_start..self.pos];
            let name = match radix {
                16 => "hexadecimal",
                2 => "binary",
                _ => "octal",
            };
            if !digits.chars().any(|c| c != '_') {
                self.error(format!("{name} literal has no digits"), start, self.pos);
                return TokenKind::Int("0".into());
            }
            if let Some((i, bad)) = digits.char_indices().find(|&(_, c)| c != '_' && !c.is_digit(radix)) {
                let at = digits_start + i;
                self.error(format!("invalid digit `{bad}` in {name} literal"), at, at + bad.len_utf8());
                return TokenKind::Int("0".into());
            }
            let text: String = self.src[start..self.pos].chars().filter(|&c| c != '_').collect();
            return TokenKind::Int(text);
        }

        self.digits();
        // Right after a `.` the number is a tuple element index: `t.0.1` is
        // `(t.0).1`, not `t` followed by the float `0.1`.
        if self.tokens.last().is_some_and(|t| t.kind == TokenKind::Dot) {
            return TokenKind::Int(self.src[start..self.pos].chars().filter(|&c| c != '_').collect());
        }
        let mut is_float = false;
        // `1.5` is a float, but `0..10` is a range and `x.0` stays a field access.
        if self.peek() == Some('.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
            is_float = true;
            self.bump();
            self.digits();
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let sign = matches!(self.peek_at(1), Some('+' | '-'));
            let digit_at = if sign { 2 } else { 1 };
            if self.peek_at(digit_at).is_some_and(|c| c.is_ascii_digit()) {
                is_float = true;
                self.pos += digit_at;
                self.digits();
            } else {
                let end = self.pos + digit_at;
                self.bump();
                if sign {
                    self.bump();
                }
                self.error("float exponent has no digits", start, end.min(self.src.len()));
                return TokenKind::Float("0".into());
            }
        }
        let end = self.pos;
        if self.peek().is_some_and(is_ident_start) {
            while self.peek().is_some_and(is_ident_continue) {
                self.bump();
            }
            self.error(format!("invalid suffix `{}` on number literal", &self.src[end..self.pos]), end, self.pos);
        }
        let text: String = self.src[start..end].chars().filter(|&c| c != '_').collect();
        if is_float {
            TokenKind::Float(text)
        } else {
            TokenKind::Int(text)
        }
    }

    fn digits(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
            self.bump();
        }
    }

    /// `` `...` ``: no escapes, may span lines; carriage returns are
    /// dropped (as in Go) so a file's line endings do not change the value.
    fn raw_string(&mut self) -> TokenKind {
        let start = self.pos;
        self.bump(); // opening backtick
        let mut value = String::new();
        loop {
            match self.peek() {
                None => {
                    self.error("unterminated raw string literal", start, self.pos);
                    break;
                }
                Some('`') => {
                    self.bump();
                    break;
                }
                Some(c) => {
                    self.bump();
                    if c != '\r' {
                        value.push(c);
                    }
                }
            }
        }
        TokenKind::Str(value)
    }

    fn string(&mut self) -> TokenKind {
        let start = self.pos;
        self.bump(); // opening quote
        let mut value = String::new();
        loop {
            match self.peek() {
                None | Some('\n') => {
                    self.error("unterminated string literal", start, self.pos);
                    break;
                }
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    if let Some(c) = self.escape() {
                        value.push(c);
                    }
                }
                Some(c) => {
                    self.bump();
                    value.push(c);
                }
            }
        }
        TokenKind::Str(value)
    }

    fn char_lit(&mut self) -> TokenKind {
        let start = self.pos;
        self.bump(); // opening quote
        let value = match self.peek() {
            None | Some('\n') => {
                self.error("unterminated char literal", start, self.pos);
                return TokenKind::Char('\0');
            }
            Some('\'') => {
                self.bump();
                self.error("empty char literal", start, self.pos);
                return TokenKind::Char('\0');
            }
            Some('\\') => self.escape().unwrap_or('\0'),
            Some(c) => {
                self.bump();
                c
            }
        };
        if self.eat('\'') {
            return TokenKind::Char(value);
        }
        // Too many characters or missing quote: find the closing quote on this line.
        let rest = &self.src[self.pos..];
        let line_end = rest.find('\n').unwrap_or(rest.len());
        match rest[..line_end].find('\'') {
            Some(i) => {
                self.pos += i + 1;
                self.error("char literal must contain exactly one character", start, self.pos);
            }
            None => self.error("unterminated char literal", start, self.pos),
        }
        TokenKind::Char(value)
    }

    /// Parses an escape sequence starting at `\`. Returns `None` after
    /// reporting an invalid escape.
    fn escape(&mut self) -> Option<char> {
        let start = self.pos;
        self.bump(); // backslash
        let c = match self.peek() {
            Some(c) if c != '\n' => c,
            _ => {
                self.error("incomplete escape sequence", start, self.pos);
                return None;
            }
        };
        self.bump();
        Some(match c {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '0' => '\0',
            '\\' => '\\',
            '"' => '"',
            '\'' => '\'',
            'u' => return self.unicode_escape(start),
            other => {
                self.error(format!("unknown escape sequence `\\{other}`"), start, self.pos);
                return None;
            }
        })
    }

    /// The `{XXXX}` part of `\u{XXXX}`; `start` points at the backslash.
    fn unicode_escape(&mut self, start: usize) -> Option<char> {
        if !self.eat('{') {
            self.error("expected `{` after `\\u`", start, self.pos);
            return None;
        }
        let digits_start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            self.bump();
        }
        let digits = &self.src[digits_start..self.pos];
        if !self.eat('}') {
            self.error("unterminated unicode escape, expected `}`", start, self.pos);
            return None;
        }
        if digits.is_empty() || digits.len() > 6 {
            self.error("unicode escape must have 1 to 6 hex digits", start, self.pos);
            return None;
        }
        let value = u32::from_str_radix(digits, 16).ok().and_then(char::from_u32);
        if value.is_none() {
            self.error(format!("invalid unicode scalar value `{digits}`"), start, self.pos);
        }
        value
    }

    /// Punctuation. Returns `None` (after reporting) for an unknown character.
    fn punct(&mut self) -> Option<TokenKind> {
        use TokenKind::*;
        let start = self.pos;
        let c = self.bump()?;
        Some(match c {
            '(' => LParen,
            ')' => RParen,
            '{' => LBrace,
            '}' => RBrace,
            '[' => LBracket,
            ']' => RBracket,
            ',' => Comma,
            ':' => Colon,
            ';' => Semi,
            '?' => Question,
            '@' => self.with_eq(At, AtEq),
            '.' => {
                if self.eat('.') {
                    if self.eat('.') {
                        Ellipsis
                    } else {
                        DotDot
                    }
                } else {
                    Dot
                }
            }
            '!' => {
                if self.eat('=') {
                    NotEq
                } else {
                    Bang
                }
            }
            '=' => {
                if self.eat('=') {
                    EqEq
                } else if self.eat('>') {
                    Arrow
                } else {
                    Assign
                }
            }
            '&' => {
                if self.eat('&') {
                    AndAnd
                } else if self.eat('=') {
                    AmpEq
                } else {
                    Amp
                }
            }
            '|' => {
                if self.eat('|') {
                    OrOr
                } else if self.eat('=') {
                    PipeEq
                } else {
                    Pipe
                }
            }
            '<' => {
                if self.eat('<') {
                    if self.eat('=') {
                        ShlEq
                    } else {
                        Shl
                    }
                } else if self.eat('=') {
                    Le
                } else {
                    Lt
                }
            }
            '>' => {
                if self.eat('>') {
                    if self.eat('=') {
                        ShrEq
                    } else {
                        Shr
                    }
                } else if self.eat('=') {
                    Ge
                } else {
                    Gt
                }
            }
            '+' => self.with_eq(Plus, PlusEq),
            '-' => self.with_eq(Minus, MinusEq),
            '*' => self.with_eq(Star, StarEq),
            '/' => self.with_eq(Slash, SlashEq),
            '%' => self.with_eq(Percent, PercentEq),
            '^' => self.with_eq(Caret, CaretEq),
            other => {
                self.error(format!("unexpected character `{}`", other.escape_debug()), start, self.pos);
                return None;
            }
        })
    }

    fn with_eq(&mut self, plain: TokenKind, with_eq: TokenKind) -> TokenKind {
        if self.eat('=') {
            with_eq
        } else {
            plain
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        let mut d = Diagnostics::new();
        let toks = lex(src, &mut d);
        assert!(d.is_empty(), "unexpected diagnostics: {:?}", d.items);
        toks.into_iter().map(|t| t.kind).collect()
    }

    fn errors(src: &str) -> Vec<String> {
        let mut d = Diagnostics::new();
        let toks = lex(src, &mut d);
        assert_eq!(toks.last().map(|t| &t.kind), Some(&Eof));
        d.items.into_iter().map(|d| d.message).collect()
    }

    fn ident(s: &str) -> TokenKind {
        Ident(s.into())
    }

    #[test]
    fn keywords_and_idents() {
        assert_eq!(
            kinds("namespace app import as func x _ _y привет"),
            vec![Namespace, ident("app"), Import, As, Func, ident("x"), Underscore, ident("_y"), ident("привет"), Semi, Eof]
        );
    }

    #[test]
    fn integers() {
        assert_eq!(
            kinds("123 1_000 0xff 0XFF 0b1010 0o17 0x_ff"),
            vec![
                Int("123".into()),
                Int("1000".into()),
                Int("0xff".into()),
                Int("0XFF".into()),
                Int("0b1010".into()),
                Int("0o17".into()),
                Int("0xff".into()),
                Semi,
                Eof
            ]
        );
    }

    #[test]
    fn floats_and_ranges() {
        assert_eq!(
            kinds("1.5 2e10 3.0e-2 1E+3 1_0.5"),
            vec![
                Float("1.5".into()),
                Float("2e10".into()),
                Float("3.0e-2".into()),
                Float("1E+3".into()),
                Float("10.5".into()),
                Semi,
                Eof
            ]
        );
        assert_eq!(kinds("0..10"), vec![Int("0".into()), DotDot, Int("10".into()), Semi, Eof]);
        assert_eq!(kinds("t.0"), vec![ident("t"), Dot, Int("0".into()), Semi, Eof]);
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(
            kinds(r#""a\nb\t\\\"\r\0\u{41}\u{1F600}""#),
            vec![Str("a\nb\t\\\"\r\0A\u{1F600}".into()), Semi, Eof]
        );
        assert_eq!(kinds("\"привет\""), vec![Str("привет".into()), Semi, Eof]);
    }

    #[test]
    fn raw_strings() {
        assert_eq!(kinds(r"`C:\dir\n`"), vec![Str(r"C:\dir\n".into()), Semi, Eof]);
        assert_eq!(kinds("`a\r\n\"b\"`"), vec![Str("a\n\"b\"".into()), Semi, Eof]);
        assert_eq!(kinds("x = `\n`\ny"), vec![ident("x"), Assign, Str("\n".into()), Semi, ident("y"), Semi, Eof]);
    }

    #[test]
    fn chars() {
        assert_eq!(
            kinds(r"'a' '\n' '\'' '\u{263A}' 'я'"),
            vec![Char('a'), Char('\n'), Char('\''), Char('☺'), Char('я'), Semi, Eof]
        );
    }

    #[test]
    fn punctuation() {
        assert_eq!(
            kinds("( ) { } [ ] , . .. : ; ? @ ! => & * + - / % | ^ << >> && || < > <= >= == != = += -= *= /= %= &= |= ^= <<= >>= @="),
            vec![
                LParen, RParen, LBrace, RBrace, LBracket, RBracket, Comma, Dot, DotDot, Colon, Semi,
                Question, At, Bang, Arrow, Amp, Star, Plus, Minus, Slash, Percent, Pipe, Caret, Shl, Shr,
                AndAnd, OrOr, Lt, Gt, Le, Ge, EqEq, NotEq, Assign, PlusEq, MinusEq, StarEq, SlashEq,
                PercentEq, AmpEq, PipeEq, CaretEq, ShlEq, ShrEq, AtEq, Eof
            ]
        );
    }

    #[test]
    fn comments() {
        assert_eq!(kinds("a // comment\nb"), vec![ident("a"), Semi, ident("b"), Semi, Eof]);
        assert_eq!(kinds("a /* x */ b"), vec![ident("a"), ident("b"), Semi, Eof]);
        assert_eq!(kinds("a /* x\n y */ b"), vec![ident("a"), Semi, ident("b"), Semi, Eof]);
        assert_eq!(kinds("a /* /* nested */ */ b"), vec![ident("a"), ident("b"), Semi, Eof]);
    }

    #[test]
    fn semicolon_insertion() {
        assert_eq!(
            kinds("var x = 1\nx +\n2\nreturn\n}\n"),
            vec![
                Var, ident("x"), Assign, Int("1".into()), Semi,
                ident("x"), Plus, Int("2".into()), Semi,
                Return, Semi,
                RBrace, Semi,
                Eof
            ]
        );
        // No semicolon after `{`, `,` or operators; none at EOF after `{`.
        assert_eq!(kinds("f(a,\nb)\n{"), vec![ident("f"), LParen, ident("a"), Comma, ident("b"), RParen, Semi, LBrace, Eof]);
        // Inside parentheses newlines are ignored, inside braces they count.
        assert_eq!(
            kinds("f(\na\n)\n"),
            vec![ident("f"), LParen, ident("a"), RParen, Semi, Eof]
        );
        assert_eq!(
            kinds("f(func() {\na\n})"),
            vec![ident("f"), LParen, Func, LParen, RParen, LBrace, ident("a"), Semi, RBrace, RParen, Semi, Eof]
        );
        // A keyword used as a name after `.` ends a statement too.
        assert_eq!(kinds("a.async\nb"), vec![ident("a"), Dot, Async, Semi, ident("b"), Semi, Eof]);
        // Fallible types and `?` end statements.
        // A leading `.` continues the previous line (also across comments).
        assert_eq!(kinds("a\n  .b()\n// c\n.d\nx"), vec![ident("a"), Dot, ident("b"), LParen, RParen, Dot, ident("d"), Semi, ident("x"), Semi, Eof]);
        assert_eq!(kinds("a;\n.b"), vec![ident("a"), Semi, Dot, ident("b"), Semi, Eof]);
        assert_eq!(kinds("int!\nx?\n"), vec![ident("int"), Bang, Semi, ident("x"), Question, Semi, Eof]);
    }

    #[test]
    fn spans() {
        let mut d = Diagnostics::new();
        let toks = lex("var xy = \"s\"\n", &mut d);
        let spans: Vec<(u32, u32)> = toks.iter().map(|t| (t.span.start, t.span.end)).collect();
        assert_eq!(spans, vec![(0, 3), (4, 6), (7, 8), (9, 12), (12, 12), (13, 13)]);
    }

    #[test]
    fn lexical_errors_are_reported_and_lexing_continues() {
        assert_eq!(errors("\"abc"), vec!["unterminated string literal"]);
        assert_eq!(errors("\"a\\qb\""), vec!["unknown escape sequence `\\q`"]);
        assert_eq!(errors("''"), vec!["empty char literal"]);
        assert_eq!(errors("'ab'"), vec!["char literal must contain exactly one character"]);
        assert_eq!(errors("0b102"), vec!["invalid digit `2` in binary literal"]);
        assert_eq!(errors("0x"), vec!["hexadecimal literal has no digits"]);
        assert_eq!(errors("1e"), vec!["float exponent has no digits"]);
        assert_eq!(errors("12abc"), vec!["invalid suffix `abc` on number literal"]);
        assert_eq!(errors("/* never closed"), vec!["unterminated block comment"]);
        assert_eq!(errors("\"\\u{110000}\""), vec!["invalid unicode scalar value `110000`"]);
        assert_eq!(errors("\"\\u{}\""), vec!["unicode escape must have 1 to 6 hex digits"]);
        assert_eq!(errors("a $ b # c"), vec!["unexpected character `$`", "unexpected character `#`"]);

        let mut d = Diagnostics::new();
        let toks = lex("x = $ 1", &mut d);
        assert_eq!(d.items[0].span, Span::new(4, 5));
        let k: Vec<_> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(k, vec![ident("x"), Assign, Int("1".into()), Semi, Eof]);
    }

    #[test]
    fn trivia_is_attached_to_the_next_token() {
        let src = "a // one\n\n/* two */ b\nc\n";
        let mut d = Diagnostics::new();
        let l = lex_with_trivia(src, &mut d);
        assert_eq!(l.tokens.len(), l.leading.len());
        let kinds: Vec<TokenKind> = l.tokens.iter().map(|t| t.kind.clone()).collect();
        assert_eq!(kinds, vec![ident("a"), Semi, ident("b"), Semi, ident("c"), Semi, Eof]);
        let text = |t: &Trivia| &src[t.span.start as usize..t.span.end as usize];
        // `b` carries the comment after `a`, the blank line and `/* two */`.
        let b: Vec<(TriviaKind, &str)> = l.leading(2).iter().map(|t| (t.kind, text(t))).collect();
        assert_eq!(b, vec![(TriviaKind::LineComment, "// one"), (TriviaKind::Newlines(2), "\n\n"), (TriviaKind::BlockComment, "/* two */")]);
        assert!(l.leading(1).is_empty(), "automatic semicolons have no trivia");
        assert_eq!(l.leading(4).iter().map(|t| t.kind).collect::<Vec<_>>(), vec![TriviaKind::Newlines(1)]);
        assert_eq!(l.leading(6).iter().map(|t| t.kind).collect::<Vec<_>>(), vec![TriviaKind::Newlines(1)]);
        assert_eq!(l.token_at(src.find('c').unwrap() as u32), 4);
        let (_, comments) = lex_with_comments(src, &mut d);
        assert_eq!(comments.len(), 2);
    }

    #[test]
    fn empty_input() {
        assert_eq!(kinds(""), vec![Eof]);
        assert_eq!(kinds("\n\n  // only a comment"), vec![Eof]);
    }
}
