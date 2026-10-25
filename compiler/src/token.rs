//! Token definitions produced by the lexer.

use crate::span::Span;

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    // Literals and names
    Ident(String),
    /// Integer literal; stored as the source text without `_` separators,
    /// e.g. "123", "0xff", "0b101". The parser converts it.
    Int(String),
    /// Float literal source text, e.g. "1.5", "2e10".
    Float(String),
    /// String literal with escapes already processed.
    Str(String),
    /// Character literal with escapes already processed.
    Char(char),

    // Keywords
    Namespace,
    Import,
    As,
    Struct,
    Interface,
    Enum,
    Func,
    Static,
    Async,
    Await,
    Var,
    Const,
    If,
    Else,
    For,
    Switch,
    Case,
    Default,
    Return,
    Break,
    Continue,
    Defer,
    Try,
    Catch,
    New,
    Nil,
    True,
    False,
    Map,
    This,

    // Punctuation
    LParen,    // (
    RParen,    // )
    LBrace,    // {
    RBrace,    // }
    LBracket,  // [
    RBracket,  // ]
    Comma,     // ,
    Dot,       // .
    DotDot,    // ..
    Ellipsis,  // ...
    Colon,     // :
    Semi,      // ; (explicit or auto-inserted at line end)
    Question,  // ?
    Bang,      // !
    Arrow,     // =>
    At,        // @ (attributes at declaration start; matrix multiplication between operands)
    AtEq,      // @=
    Amp,       // &
    Star,      // *
    Plus,      // +
    Minus,     // -
    Slash,     // /
    Percent,   // %
    Pipe,      // |
    Caret,     // ^
    Shl,       // <<
    Shr,       // >>
    AndAnd,    // &&
    OrOr,      // ||
    Lt,        // <
    Gt,        // >
    Le,        // <=
    Ge,        // >=
    EqEq,      // ==
    NotEq,     // !=
    Assign,    // =
    PlusEq,    // +=
    MinusEq,   // -=
    StarEq,    // *=
    SlashEq,   // /=
    PercentEq, // %=
    AmpEq,     // &=
    PipeEq,    // |=
    CaretEq,   // ^=
    ShlEq,     // <<=
    ShrEq,     // >>=
    Underscore, // _

    Eof,
}

impl TokenKind {
    pub fn keyword(ident: &str) -> Option<TokenKind> {
        use TokenKind::*;
        Some(match ident {
            "namespace" => Namespace,
            "import" => Import,
            "as" => As,
            "struct" => Struct,
            "interface" => Interface,
            "enum" => Enum,
            "func" => Func,
            "static" => Static,
            "async" => Async,
            "await" => Await,
            "var" => Var,
            "const" => Const,
            "if" => If,
            "else" => Else,
            "for" => For,
            "switch" => Switch,
            "case" => Case,
            "default" => Default,
            "return" => Return,
            "break" => Break,
            "continue" => Continue,
            "defer" => Defer,
            "try" => Try,
            "catch" => Catch,
            "new" => New,
            "nil" => Nil,
            "true" => True,
            "false" => False,
            "map" => Map,
            "this" => This,
            "_" => Underscore,
            _ => return None,
        })
    }

    /// Whether a newline directly after this token ends a statement
    /// (Go-style automatic semicolon insertion).
    pub fn ends_statement(&self) -> bool {
        use TokenKind::*;
        matches!(
            self,
            Ident(_) | Int(_) | Float(_) | Str(_) | Char(_)
                | Return | Break | Continue | Nil | True | False | This | Underscore
                | RParen | RBrace | RBracket | Question | Bang
                // closing generic arguments: `var nf NotFound<string>`
                | Gt | Shr
        )
    }

    pub fn describe(&self) -> String {
        use TokenKind::*;
        match self {
            Ident(s) => format!("identifier `{s}`"),
            Int(s) | Float(s) => format!("number `{s}`"),
            Str(_) => "string literal".into(),
            Char(_) => "char literal".into(),
            Semi => "end of line".into(),
            Eof => "end of file".into(),
            other => format!("`{}`", other.text()),
        }
    }

    /// Source spelling for punctuation and keywords.
    pub fn text(&self) -> &'static str {
        use TokenKind::*;
        match self {
            Namespace => "namespace", Import => "import", As => "as", Struct => "struct",
            Interface => "interface", Enum => "enum", Func => "func", Static => "static",
            Async => "async", Await => "await", Var => "var", Const => "const", If => "if",
            Else => "else", For => "for", Switch => "switch", Case => "case",
            Default => "default", Return => "return", Break => "break", Continue => "continue",
            Defer => "defer", Try => "try", Catch => "catch", New => "new", Nil => "nil",
            True => "true", False => "false", Map => "map", This => "this",
            LParen => "(", RParen => ")", LBrace => "{", RBrace => "}", LBracket => "[",
            RBracket => "]", Comma => ",", Dot => ".", DotDot => "..", Ellipsis => "...", Colon => ":", Semi => ";",
            Question => "?", Bang => "!", Arrow => "=>", At => "@", Amp => "&", Star => "*", Plus => "+",
            Minus => "-", Slash => "/", Percent => "%", Pipe => "|", Caret => "^", Shl => "<<",
            Shr => ">>", AndAnd => "&&", OrOr => "||", Lt => "<", Gt => ">", Le => "<=", Ge => ">=",
            EqEq => "==", NotEq => "!=", Assign => "=", PlusEq => "+=", MinusEq => "-=",
            StarEq => "*=", SlashEq => "/=", PercentEq => "%=", AmpEq => "&=", PipeEq => "|=",
            CaretEq => "^=", ShlEq => "<<=", ShrEq => ">>=", AtEq => "@=", Underscore => "_", Eof => "<eof>",
            Ident(_) | Int(_) | Float(_) | Str(_) | Char(_) => "<literal>",
        }
    }
}
