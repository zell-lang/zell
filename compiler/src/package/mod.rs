//! The package manager (docs/PACKAGES.md): manifests in TOML, JSON or
//! YAML with one schema ([`manifest`]), project discovery, workspaces and
//! `path` dependencies ([`graph`]), module dependencies by path
//! (`"github.com/org/pkg" = "^1.2"`, git pins and aliases) selected by MVS
//! ([`resolve`], [`semver`]) and fetched through proxies or git
//! ([`fetch`], [`git`], [`http`]) with `h1:` checksums and the checksum
//! log ([`sum`]), the content-addressable store ([`store`]),
//! `zell vendor` ([`vendor`]), the module proxy server ([`proxy`]),
//! `zell.lock` ([`lock`]), `zell init` ([`init`]) and its `--agents`
//! configuration for AI coding agents ([`agents`]), the package commands
//! ([`cli`]) and the package-aware front end ([`check`]).
//!
//! Rules enforced for package builds:
//!
//! - every package owns one namespace root, unique in the graph, and all
//!   its files declare namespaces under it ([`check_namespaces`]);
//! - strict isolation: a file may use only the standard library, its own root and the
//!   roots of its package's declared dependencies (enforced inside sema by
//!   [`crate::sema::ImportPolicy`]).

pub mod agents;
pub mod cli;
pub mod fetch;
pub mod git;
pub mod graph;
pub mod http;
pub mod init;
pub mod lock;
pub mod manifest;
pub mod proxy;
pub mod resolve;
pub mod semver;
pub mod store;
pub mod sum;
pub mod vendor;

pub use graph::{PackageNode, PkgId, Project, Source};
pub use manifest::{Format, LoadedManifest, Manifest};

use crate::ast;
use crate::diagnostics::{Diagnostic, Diagnostics};
use crate::hir::Program;
use crate::span::SourceFile;
use std::path::Path;

/// The front-end result for one build target (a member and its closure).
pub struct Checked {
    /// Packages of the build, dependencies first; the target is last.
    pub closure: Vec<PkgId>,
    pub files: Vec<SourceFile>,
    /// For each file: position of its package in `closure`.
    pub file_pkg: Vec<usize>,
    /// `None` if there were errors (already in the diagnostics).
    pub program: Option<Program>,
}

/// Display path of `p`: relative to the current directory when inside it.
pub(crate) fn display_path(p: &Path) -> String {
    let cwd = std::env::current_dir().ok().and_then(|c| c.canonicalize().ok());
    match cwd.as_ref().and_then(|c| p.strip_prefix(c).ok()) {
        Some(r) => r.display().to_string(),
        None => p.display().to_string(),
    }
}

/// Loads, parses and analyses `target` with all its dependencies.
/// `dev` makes the target's dev-dependencies visible (for `zell test`).
pub fn check(project: &Project, target: PkgId, dev: bool, diags: &mut Diagnostics) -> Result<Checked, String> {
    let closure = project.closure(target, dev);
    let mut files = Vec::new();
    let mut file_pkg = Vec::new();
    for (i, &id) in closure.iter().enumerate() {
        let p = &project.packages[id];
        let paths = p.source_files()?;
        if paths.is_empty() {
            return Err(format!("package `{}` has no source files (expected {}/src/**/*.zl)", p.name, display_path(&p.dir)));
        }
        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            files.push(SourceFile::new(display_path(&path), text));
            file_pkg.push(i);
        }
    }
    let policy = project.policy(&closure, file_pkg.clone(), dev);
    let scopes: Vec<(String, String)> = closure.iter().map(|&id| (project.packages[id].name.clone(), project.packages[id].namespace.clone())).collect();
    let features = project.enabled_features(&closure);
    let feature_of = |i: usize| features[file_pkg[i]].clone();
    let env = crate::cfg::CfgEnv { target: crate::target::current(), features: Some(&feature_of) };
    // Lints: the target package's files, with its `[lints]` levels.
    let overrides = crate::sema::lint::parse_overrides(&project.packages[target].manifest.manifest.lints)?;
    crate::sema::lint::update_config(|c| c.overrides.extend(overrides));
    let target_pos = closure.iter().position(|&id| id == target);
    let lint_files = |i: usize| Some(file_pkg[i]) == target_pos;
    let mut program = crate::driver::check_with_policy(&files, diags, &policy, &env, |asts, d| check_namespaces(asts, &file_pkg, &scopes, d), &lint_files);
    let t = &project.packages[target];
    if let (Some(prog), Some(entry)) = (&program, t.entry()) {
        let entry = entry.canonicalize().unwrap_or(entry);
        let entry_idx = files.iter().position(|f| Path::new(&f.path).canonicalize().ok().as_ref() == Some(&entry));
        let main_file = prog.entry.map(|f| prog.funcs[f.0 as usize].file);
        if main_file.is_none() || main_file != entry_idx {
            let found = main_file.map(|i| format!(" (found one in {}, package `{}`)", files[i].path, scopes[file_pkg[i]].0)).unwrap_or_default();
            diags.push(Diagnostic::error(format!("entry file of package `{}` must declare `func main()` in its root namespace{found}", t.name), crate::span::Span::DUMMY).in_file(entry_idx.unwrap_or(0)));
            program = None;
        }
    }
    Ok(Checked { closure, files, file_pkg, program })
}

/// Reports files whose `namespace` is not under their package's root.
/// `scopes` are (package name, namespace root) per closure position.
pub fn check_namespaces(asts: &[ast::File], file_pkg: &[usize], scopes: &[(String, String)], diags: &mut Diagnostics) {
    for (i, f) in asts.iter().enumerate() {
        let (name, root) = &scopes[file_pkg[i]];
        let ns = f.namespace.to_string();
        let ok = ns == *root || ns.strip_prefix(root.as_str()).is_some_and(|r| r.starts_with('.'));
        if !ok {
            diags.push(
                Diagnostic::error(format!("namespace `{ns}` is outside package `{name}`: its files must declare `namespace {root}` or `namespace {root}.<sub>`"), f.namespace.span)
                    .with_note(format!("the namespace root of a package is set by `namespace` in its manifest (here `{root}`)"), None)
                    .in_file(i),
            );
        }
    }
}

/// The project for a CLI invocation: `path` (a directory with or below a
/// manifest) or the current directory.
pub fn discover(path: Option<&Path>) -> Result<Option<Project>, String> {
    let start = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    Project::discover(&start)
}

/// Helper for messages: `name (dir)`.
pub fn describe(p: &PackageNode) -> String {
    format!("{} v{} ({})", p.name, p.version, display_path(&p.dir))
}

