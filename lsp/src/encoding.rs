//! The position encoding negotiated at `initialize`
//! (`general.positionEncodings`, LSP 3.17): the unit LSP columns count in.
//!
//! The client lists the encodings it supports in order of preference; the
//! server takes the first of UTF-8, UTF-16 and UTF-32 in that list, else
//! UTF-16 (which every client must support). The choice is per server
//! server: it is kept in a thread-local set by [`set`] on the thread that
//! called [`crate::run`], and by each pool worker before it runs a request
//! of that server ([`crate::snapshot::Snap`] carries it), so servers
//! running side by side in tests do not share it.

use lsp_types::{InitializeParams, Position, PositionEncodingKind};
use std::cell::Cell;
use zell::span::SourceFile;

/// A supported position encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

thread_local! {
    static CURRENT: Cell<Encoding> = const { Cell::new(Encoding::Utf16) };
}

impl Encoding {
    /// The client's most preferred encoding that the server supports.
    pub(crate) fn negotiate(p: &InitializeParams) -> Encoding {
        let offered = p.capabilities.general.as_ref().and_then(|g| g.position_encodings.as_ref());
        offered
            .into_iter()
            .flatten()
            .find_map(|k| match k.as_str() {
                "utf-8" => Some(Encoding::Utf8),
                "utf-16" => Some(Encoding::Utf16),
                "utf-32" => Some(Encoding::Utf32),
                _ => None,
            })
            .unwrap_or(Encoding::Utf16)
    }

    pub(crate) fn kind(self) -> PositionEncodingKind {
        match self {
            Encoding::Utf8 => PositionEncodingKind::UTF8,
            Encoding::Utf16 => PositionEncodingKind::UTF16,
            Encoding::Utf32 => PositionEncodingKind::UTF32,
        }
    }
}

/// Makes `e` the encoding of this thread's server.
/// The encoding of this thread's server.
pub(crate) fn get() -> Encoding {
    CURRENT.with(Cell::get)
}

pub(crate) fn set(e: Encoding) {
    CURRENT.with(|c| c.set(e));
}

/// Code units of `c` in the current encoding.
pub(crate) fn width(c: char) -> u32 {
    match CURRENT.with(|c| c.get()) {
        Encoding::Utf8 => c.len_utf8() as u32,
        Encoding::Utf16 => c.len_utf16() as u32,
        Encoding::Utf32 => 1,
    }
}

/// Length of `s` in code units of the current encoding.
pub(crate) fn len(s: &str) -> u32 {
    s.chars().map(width).sum()
}

/// (line, column) of byte offset `off` in `f`.
pub(crate) fn line_col(f: &SourceFile, off: u32) -> (u32, u32) {
    f.position_in(off, width)
}

/// LSP position of byte offset `off` in `f`.
pub(crate) fn position(f: &SourceFile, off: u32) -> Position {
    let (line, character) = line_col(f, off);
    Position { line, character }
}

/// Byte offset of LSP position `p` in `f` (clamped like the spec asks).
pub(crate) fn offset(f: &SourceFile, p: Position) -> u32 {
    f.offset_in(p.line, p.character, width)
}
