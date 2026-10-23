//! Front-end driver: loading source files and running lexer, parser and
//! semantic analysis over a whole project.

use crate::diagnostics::Diagnostics;
use crate::hir::Program;
use crate::span::SourceFile;
use crate::{ast, lexer, parser, sema};
use std::path::{Path, PathBuf};

/// The `.zl` files of `path`: the file itself, or every `.zl` file under a
/// directory (recursively), sorted by path for deterministic output. Files
/// of a directory whose name suffix excludes them on the current target
/// (`x_windows.zl` on Linux, see [`crate::target::Target::excludes_file`])
/// are left out; a file named explicitly is always taken.
pub fn collect_files(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let target = crate::target::current();
    let mut out = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "zl") && !target.excludes_file(&p.to_string_lossy()) {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Reads `paths` into source files.
pub fn load(paths: &[PathBuf]) -> std::io::Result<Vec<SourceFile>> {
    paths.iter().map(|p| Ok(SourceFile::new(p.display().to_string(), std::fs::read_to_string(p)?))).collect()
}

/// Lexes and parses every file; diagnostics get their file index.
/// Returns `None` if any file failed to parse.
pub fn parse_all(files: &[SourceFile], diags: &mut Diagnostics) -> Option<Vec<ast::File>> {
    let mut asts = Vec::new();
    let mut ok = true;
    for (i, f) in files.iter().enumerate() {
        let start = diags.items.len();
        let tokens = lexer::lex(&f.text, diags);
        let parsed = if diags.items[start..].iter().any(|d| d.severity == crate::diagnostics::Severity::Error) {
            None
        } else {
            parser::parse(tokens, diags)
        };
        for d in &mut diags.items[start..] {
            d.file = i;
        }
        match parsed {
            Some(a) => asts.push(a),
            None => ok = false,
        }
    }
    ok.then_some(asts)
}

/// Applies conditional compilation ([`crate::cfg`]) to parsed files;
/// false if a `@cfg` predicate was invalid.
pub fn apply_cfg(asts: &mut [ast::File], files: &[SourceFile], env: &crate::cfg::CfgEnv, diags: &mut Diagnostics) -> bool {
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let before = diags.has_errors();
    crate::cfg::apply(asts, &paths, env, diags);
    before || !diags.has_errors()
}

/// Parses and analyses a project (for the current target, no features).
pub fn check(files: &[SourceFile], diags: &mut Diagnostics) -> Option<Program> {
    let mut asts = {
        let _t = crate::build::phase("parse");
        parse_all(files, diags)?
    };
    if !apply_cfg(&mut asts, files, &crate::cfg::CfgEnv::current(), diags) {
        return None;
    }
    let paths = files.iter().map(|f| f.path.clone()).collect();
    let p = sema::stdlib::with_source_paths(paths, || sema::analyze(&asts, diags)).map(|p| with_sources(p, files));
    lint(p.as_ref(), files, &|_| true, diags);
    p
}

/// Runs the lints ([`sema::lint`]) over a clean program's files that
/// `wanted` selects.
pub fn lint(p: Option<&Program>, files: &[SourceFile], wanted: &dyn Fn(usize) -> bool, diags: &mut Diagnostics) {
    if let Some(p) = p {
        let _t = crate::build::phase("lint");
        let texts: Vec<&str> = files.iter().map(|f| f.text.as_str()).collect();
        sema::lint::check(p, &texts, wanted, diags);
    }
}

/// Records the source paths and line tables (error-trace sites).
fn with_sources(mut p: Program, files: &[SourceFile]) -> Program {
    p.sources = files.iter().map(|f| crate::hir::SourceInfo::new(&f.path, &f.text)).collect();
    p
}

/// Like [`check`] for a package graph: `prepare` sees the parsed files
/// first (package rules such as namespace roots) and may report errors;
/// `policy` limits which namespaces each file may use; `env` gives the
/// features of each file's package for `@cfg(feature: ...)`.
pub fn check_with_policy(
    files: &[SourceFile],
    diags: &mut Diagnostics,
    policy: &sema::ImportPolicy,
    env: &crate::cfg::CfgEnv,
    prepare: impl FnOnce(&[ast::File], &mut Diagnostics),
    lint_files: &dyn Fn(usize) -> bool,
) -> Option<Program> {
    let mut asts = {
        let _t = crate::build::phase("parse");
        parse_all(files, diags)?
    };
    prepare(&asts, diags);
    if !apply_cfg(&mut asts, files, env, diags) {
        return None;
    }
    if diags.has_errors() {
        return None;
    }
    let paths = files.iter().map(|f| f.path.clone()).collect();
    let p = sema::stdlib::with_source_paths(paths, || sema::analyze_with_policy(&asts, diags, Some(policy))).map(|p| with_sources(p, files));
    lint(p.as_ref(), files, lint_files, diags);
    p
}
