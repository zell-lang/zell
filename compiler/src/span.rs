//! Source positions shared by every compiler phase.

use std::fmt;

/// Byte range `[start, end)` inside a single source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const DUMMY: Span = Span { start: 0, end: 0 };

    pub fn new(start: u32, end: u32) -> Span {
        Span { start, end }
    }

    /// Smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span { start: self.start.min(other.start), end: self.end.max(other.end) }
    }
}

/// One loaded source file: path plus text, with line lookup.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: String,
    pub text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(path: impl Into<String>, text: impl Into<String>) -> SourceFile {
        let text = text.into();
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile { path: path.into(), text, line_starts }
    }

    /// 1-based (line, column) for a byte offset. Column counts chars, not bytes.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = self.line_starts[line] as usize;
        let col = self.text[line_start..(offset as usize).min(self.text.len())].chars().count();
        (line + 1, col + 1)
    }

    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line - 1] as usize;
        let end = self.line_starts.get(line).map(|&e| e as usize).unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches('\n')
    }

    pub fn slice(&self, span: Span) -> &str {
        &self.text[span.start as usize..span.end as usize]
    }

    /// Number of lines (a trailing newline starts an empty last line).
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Byte offset of the start of 0-based `line` (clamped to the text end).
    pub fn line_start(&self, line: usize) -> u32 {
        self.line_starts.get(line).copied().unwrap_or(self.text.len() as u32)
    }

    /// 0-based (line, UTF-16 column) of a byte offset, as used by LSP.
    /// Offsets past the end clamp to the end; offsets inside a multi-byte
    /// character round down to its start.
    pub fn utf16_position(&self, offset: u32) -> (u32, u32) {
        self.position_in(offset, |c| c.len_utf16() as u32)
    }

    /// Byte offset of a 0-based (line, UTF-16 column) position. Columns past
    /// the end of the line clamp to the line end (before its newline); lines
    /// past the end clamp to the end of the text.
    pub fn utf16_offset(&self, line: u32, col: u32) -> u32 {
        self.offset_in(line, col, |c| c.len_utf16() as u32)
    }

    /// Like [`Self::utf16_position`], with columns counted in the code
    /// units `width` gives each char (UTF-8 bytes, UTF-16 units or 1 for
    /// UTF-32: the LSP position encodings).
    pub fn position_in(&self, offset: u32, width: impl Fn(char) -> u32) -> (u32, u32) {
        let offset = (offset as usize).min(self.text.len());
        let line = match self.line_starts.binary_search(&(offset as u32)) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.line_starts[line] as usize;
        let mut col = 0u32;
        for (i, c) in self.text[start..].char_indices() {
            if start + i >= offset {
                break;
            }
            col += width(c);
        }
        (line as u32, col)
    }

    /// Like [`Self::utf16_offset`], with columns in `width` code units.
    /// A column inside a character rounds down to its start; the end of a
    /// `\r\n` line is before its `\r`.
    pub fn offset_in(&self, line: u32, col: u32, width: impl Fn(char) -> u32) -> u32 {
        let Some(&start) = self.line_starts.get(line as usize) else {
            return self.text.len() as u32;
        };
        let start = start as usize;
        let mut units = 0u32;
        let rest = &self.text[start..];
        for (i, c) in rest.char_indices() {
            let w = width(c);
            let eol = c == '\n' || (c == '\r' && rest[i + 1..].starts_with('\n'));
            if units >= col || eol || units + w > col {
                return (start + i) as u32;
            }
            units += w;
        }
        self.text.len() as u32
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}
