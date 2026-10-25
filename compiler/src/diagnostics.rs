//! Compiler diagnostics: errors and warnings with source positions.

use crate::span::{SourceFile, Span};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    /// Lints only: shown, never fails a build.
    Info,
    /// Lints only: a hint editors show unobtrusively.
    Hint,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        }
    }
}

/// Stable diagnostic codes for the cases tools offer quick fixes for
/// (docs/TOOLING.md, "structured diagnostics").
pub mod codes {
    /// A local variable is declared and never used.
    pub const UNUSED_VARIABLE: &str = "E0001";
    /// A fallible result (`T!`) is neither propagated nor handled.
    pub const UNHANDLED_RESULT: &str = "E0002";
    /// An import nothing is looked up through.
    pub const UNUSED_IMPORT: &str = "E0003";
    /// An identifier that does not resolve.
    pub const UNKNOWN_IDENTIFIER: &str = "E0004";
    /// An import or namespace under the removed `std` root
    /// (`import std.io`; the fix rewrites it to `import io`).
    pub const REMOVED_STD_ROOT: &str = "E0005";
    /// A syntax error.
    pub const SYNTAX: &str = "E0100";
    /// A type that can form a strong reference cycle (DESIGN.md "Циклы").
    pub const REFERENCE_CYCLE: &str = "W0001";
}

/// A text replacement in the diagnostic's file: `span` (byte offsets in
/// the current text) becomes `new_text`. An empty span inserts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub span: Span,
    pub new_text: String,
}

/// A quick fix: a titled set of non-overlapping edits applied together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<TextEdit>,
}

impl Fix {
    pub fn new(title: impl Into<String>, edits: Vec<TextEdit>) -> Fix {
        Fix { title: title.into(), edits }
    }

    /// A fix made of one edit.
    pub fn edit(title: impl Into<String>, span: Span, new_text: impl Into<String>) -> Fix {
        Fix { title: title.into(), edits: vec![TextEdit { span, new_text: new_text.into() }] }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    /// Optional extra lines, e.g. "note: declared here".
    pub notes: Vec<(String, Option<Span>)>,
    /// Index of the source file the span refers to, for multi-file
    /// compilations (the order of files passed to `sema::analyze`).
    /// Single-file phases leave it at 0.
    pub file: usize,
    /// Stable code (see [`codes`]), when the diagnostic has one.
    pub code: Option<&'static str>,
    /// Quick fixes, alternatives in order of preference.
    pub fixes: Vec<Fix>,
    /// Notes about locations in other files: (text, file index, span),
    /// rendered as `= note: text (path:line:col)` by
    /// [`Diagnostics::render_all_files`].
    pub related: Vec<(String, usize, Span)>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic { severity: Severity::Error, message: message.into(), span, notes: vec![], file: 0, code: None, fixes: vec![], related: vec![] }
    }

    pub fn warning(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic { severity: Severity::Warning, message: message.into(), span, notes: vec![], file: 0, code: None, fixes: vec![], related: vec![] }
    }

    /// Sets the stable code (see [`codes`]).
    pub fn with_code(mut self, code: &'static str) -> Diagnostic {
        self.code = Some(code);
        self
    }

    /// Adds a quick fix.
    pub fn with_fix(mut self, fix: Fix) -> Diagnostic {
        self.fixes.push(fix);
        self
    }

    /// Sets the file index (see [`Diagnostic::file`]).
    pub fn in_file(mut self, file: usize) -> Diagnostic {
        self.file = file;
        self
    }

    pub fn with_note(mut self, note: impl Into<String>, span: Option<Span>) -> Diagnostic {
        self.notes.push((note.into(), span));
        self
    }

    /// Adds a `help:` line (a suggested fix in words); stored as a note
    /// whose text starts with `help: `.
    pub fn with_help(mut self, help: impl Into<String>) -> Diagnostic {
        self.notes.push((format!("help: {}", help.into()), None));
        self
    }

    /// Human-readable rendering:
    /// ```text
    /// error: unexpected token
    ///   --> main.zl:3:5
    ///    |
    ///  3 |     var = 1
    ///    |     ^
    /// ```
    pub fn render(&self, file: &SourceFile) -> String {
        self.render_in(file, &[])
    }

    /// [`Diagnostic::render`] with the files [`Diagnostic::related`] refers to.
    pub fn render_in(&self, file: &SourceFile, files: &[SourceFile]) -> String {
        let (line, col) = file.line_col(self.span.start);
        let sev = match self.code.filter(|c| c.starts_with('L')) {
            Some(code) => format!("{}[{code}]", self.severity.label()),
            None => self.severity.label().to_string(),
        };
        let mut out = format!("{sev}: {}\n  --> {}:{line}:{col}\n", self.message, file.path);
        let src = file.line_text(line);
        let width = line.to_string().len();
        let len = (self.span.end.saturating_sub(self.span.start)).max(1) as usize;
        let (eline, _) = file.line_col(self.span.end.max(self.span.start));
        let caret_len = if eline == line { len.min(src.len().saturating_sub(col - 1).max(1)) } else { src.len().saturating_sub(col - 1).max(1) };
        out.push_str(&format!("{:w$} |\n{line:w$} | {src}\n{:w$} | {}{}\n", "", "", " ".repeat(col - 1), "^".repeat(caret_len), w = width));
        for (note, span) in &self.notes {
            if let Some(help) = note.strip_prefix("help: ") {
                out.push_str(&format!("{:w$} = help: {help}\n", "", w = width));
                continue;
            }
            match span {
                Some(s) => {
                    let (l, c) = file.line_col(s.start);
                    out.push_str(&format!("{:w$} = note: {note} ({}:{l}:{c})\n", "", file.path, w = width));
                }
                None => out.push_str(&format!("{:w$} = note: {note}\n", "", w = width)),
            }
        }
        for (note, fi, span) in &self.related {
            let f = if files.is_empty() && *fi == self.file { Some(file) } else { files.get(*fi) };
            match f {
                Some(f) => {
                    let (l, c) = f.line_col(span.start);
                    out.push_str(&format!("{:w$} = note: {note} ({}:{l}:{c})\n", "", f.path, w = width));
                }
                None => out.push_str(&format!("{:w$} = note: {note}\n", "", w = width)),
            }
        }
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} at {}", self.severity.label(), self.message, self.span)
    }
}

/// Accumulates diagnostics for one compilation.
#[derive(Default, Debug, Clone)]
pub struct Diagnostics {
    pub items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Diagnostics {
        Diagnostics::default()
    }

    pub fn error(&mut self, message: impl Into<String>, span: Span) {
        self.items.push(Diagnostic::error(message, span));
    }

    pub fn warning(&mut self, message: impl Into<String>, span: Span) {
        self.items.push(Diagnostic::warning(message, span));
    }

    pub fn push(&mut self, d: Diagnostic) {
        self.items.push(d);
    }

    /// True if a diagnostic was already reported at exactly this span and
    /// file (lets later phases avoid duplicate messages).
    pub fn has_errors_at(&self, span: Span, file: usize) -> bool {
        self.items.iter().any(|d| d.span == span && d.file == file)
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn render_all(&self, file: &SourceFile) -> String {
        self.items.iter().map(|d| d.render(file)).collect::<Vec<_>>().join("\n")
    }

    /// Renders every diagnostic against `files[d.file]` (multi-file builds).
    pub fn render_all_files(&self, files: &[SourceFile]) -> String {
        self.items
            .iter()
            .map(|d| match files.get(d.file) {
                Some(f) => d.render_in(f, files),
                None => format!("{d}\n"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
