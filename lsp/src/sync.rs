//! Text synchronisation: `textDocument/didChange` content changes, full
//! or incremental (`TextDocumentSyncKind::INCREMENTAL`), applied to the
//! server's copy of a document.
//!
//! Positions are in the negotiated `position_encoding` (see [`crate::encoding`]); lines end at
//! `\n` (a `\r` before it is not part of the line), as in
//! [`zell::span::SourceFile`]. Out-of-range positions are clamped like
//! the LSP specification asks: a character past the end of its line means
//! the end of the line, a line past the end of the text the end of the
//! text.

use lsp_types::{Position, TextDocumentContentChangeEvent};

/// Byte offset of the position `pos` (in the negotiated encoding) in `text`.
pub(crate) fn offset_at(text: &str, pos: Position) -> usize {
    let mut line_start = 0;
    for _ in 0..pos.line {
        match text[line_start..].find('\n') {
            Some(i) => line_start += i + 1,
            None => return text.len(),
        }
    }
    let mut col = 0u32;
    let rest = &text[line_start..];
    for (i, c) in rest.char_indices() {
        if c == '\n' || (c == '\r' && rest[i + 1..].starts_with('\n')) {
            return line_start + i;
        }
        let w = crate::encoding::width(c);
        // A position inside a surrogate pair rounds down to the character.
        if col + w > pos.character {
            return line_start + i;
        }
        col += w;
    }
    text.len()
}

/// UTF-16 position of byte offset `off` (clamped to a char boundary).
#[cfg(test)]
pub(crate) fn position_at(text: &str, off: usize) -> Position {
    let mut off = off.min(text.len());
    while !text.is_char_boundary(off) {
        off -= 1;
    }
    let line_start = text[..off].rfind('\n').map_or(0, |i| i + 1);
    let line = text[..line_start].matches('\n').count() as u32;
    Position { line, character: text[line_start..off].encode_utf16().count() as u32 }
}

/// Applies one content change: the whole text when it has no range, else
/// the replacement of its range (positions refer to `text` as left by the
/// previous change of the same notification).
pub(crate) fn apply_change(text: &mut String, change: TextDocumentContentChangeEvent) {
    match change.range {
        None => *text = change.text,
        Some(r) => {
            let a = offset_at(text, r.start);
            let b = offset_at(text, r.end);
            text.replace_range(a.min(b)..a.max(b), &change.text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Range;

    fn p(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn change(text: &mut String, a: Position, b: Position, new: &str) {
        apply_change(text, TextDocumentContentChangeEvent { range: Some(Range { start: a, end: b }), range_length: None, text: new.into() });
    }

    #[test]
    fn offsets_count_utf16_units() {
        let t = "ab\nпр𝄞x\r\nend";
        assert_eq!(offset_at(t, p(0, 0)), 0);
        assert_eq!(offset_at(t, p(0, 2)), 2);
        assert_eq!(offset_at(t, p(0, 9)), 2, "clamped to the end of the line");
        assert_eq!(offset_at(t, p(1, 2)), 3 + "пр".len());
        assert_eq!(offset_at(t, p(1, 4)), 3 + "пр𝄞".len());
        assert_eq!(offset_at(t, p(1, 3)), 3 + "пр".len(), "inside a surrogate pair");
        assert_eq!(offset_at(t, p(1, 99)), 3 + "пр𝄞x".len(), "before \\r\\n");
        assert_eq!(offset_at(t, p(2, 3)), t.len());
        assert_eq!(offset_at(t, p(7, 0)), t.len());
        for off in [0, 2, 3, 5, 7, 11, 12, t.len()] {
            assert_eq!(offset_at(t, position_at(t, off)), off, "{off}");
        }
    }

    #[test]
    fn incremental_changes_apply_in_order() {
        let mut t = String::from("namespace app\n\nfunc main() {\n}\n");
        change(&mut t, p(2, 13), p(2, 13), "\n    var x = 1");
        assert_eq!(t, "namespace app\n\nfunc main() {\n    var x = 1\n}\n");
        change(&mut t, p(3, 8), p(3, 9), "яблоко");
        change(&mut t, p(3, 14), p(3, 18), " = 2");
        assert_eq!(t, "namespace app\n\nfunc main() {\n    var яблоко = 2\n}\n");
        change(&mut t, p(2, 0), p(4, 1), "");
        assert_eq!(t, "namespace app\n\n\n");
        apply_change(&mut t, TextDocumentContentChangeEvent { range: None, range_length: None, text: "full".into() });
        assert_eq!(t, "full");
    }
}
