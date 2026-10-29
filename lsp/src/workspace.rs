//! Client-driven workspace state: file watchers registered by the server
//! (`client/registerCapability`), workspace folders added and removed at
//! run time, settings pulled with `workspace/configuration`, `$/logTrace`
//! output, and the refresh requests (`workspace/semanticTokens/refresh`,
//! `workspace/inlayHint/refresh`, `workspace/codeLens/refresh`) sent after
//! an analysis that changed what other open documents show.

use crate::Server;
use lsp_server::Response;
use lsp_types::*;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use zell::sema::index::SymbolKind;
use zell::session::Analysis;

/// Files whose changes the server needs to hear about.
const WATCHED: &[&str] = &["**/*.zl", "**/zell.toml", "**/zell.lock"];

impl Server {
    /// After `initialized`: registers the file watchers and pulls the
    /// settings, each when the client supports it.
    pub(crate) fn after_initialized(&mut self) {
        if self.caps.watcher_registration {
            let watchers = WATCHED.iter().map(|g| FileSystemWatcher { glob_pattern: GlobPattern::String((*g).into()), kind: None }).collect();
            let options = DidChangeWatchedFilesRegistrationOptions { watchers };
            let registration = Registration {
                id: "zell-lsp/watched-files".into(),
                method: <notification::DidChangeWatchedFiles as notification::Notification>::METHOD.into(),
                register_options: serde_json::to_value(options).ok(),
            };
            self.server_request::<request::RegisterCapability>(RegistrationParams { registrations: vec![registration] });
        }
        self.pull_configuration();
    }

    /// Asks the client for the `zell` settings section (when it supports
    /// `workspace/configuration`); the answer arrives in [`Server::on_response`].
    pub(crate) fn pull_configuration(&mut self) {
        if !self.caps.configuration {
            return;
        }
        let item = ConfigurationItem { scope_uri: None, section: Some("zell".into()) };
        let id = self.server_request::<request::WorkspaceConfiguration>(ConfigurationParams { items: vec![item] });
        self.config_requests.insert(id);
    }

    /// Responses to the server's own requests.
    pub(crate) fn on_response(&mut self, r: Response) {
        if !self.config_requests.remove(&r.id) {
            return;
        }
        let first = r.result.as_ref().and_then(|v| v.as_array()).and_then(|a| a.first().cloned());
        if let Some(v) = first.filter(|v| v.is_object()) {
            self.apply_settings(&v);
        }
    }

    /// Applies a `zell` settings object (from `workspace/configuration`,
    /// `didChangeConfiguration` or `initializationOptions`); inlay hints of
    /// open documents are refreshed when their settings changed.
    pub(crate) fn apply_settings(&mut self, v: &serde_json::Value) {
        let before = self.hints;
        self.hints.update(v);
        if self.hints != before && self.caps.hints_refresh {
            self.server_request::<request::InlayHintRefreshRequest>(());
        }
    }

    /// `workspace/didChangeWorkspaceFolders`: loads added folders' projects
    /// and unloads removed ones.
    pub(crate) fn folders_changed(&mut self, e: WorkspaceFoldersChangeEvent) {
        for f in e.removed {
            if let Ok(root) = f.uri.to_file_path() {
                self.remove_root(&root);
            }
        }
        for f in e.added {
            if let Ok(root) = f.uri.to_file_path() {
                self.add_root(root);
            }
        }
    }

    /// Unloads the files under `root` (open documents stay: the editor
    /// still shows them).
    fn remove_root(&mut self, root: &Path) {
        let Some(i) = self.roots.iter().position(|r| r == root) else { return };
        self.roots.remove(i);
        let keys: Vec<String> = self.session.paths().filter(|k| Path::new(k).starts_with(root) && !self.docs.contains_key(*k)).map(str::to_string).collect();
        for k in keys {
            self.session.remove_file(&k);
            self.uris.remove(&k);
            self.tokens.remove(&k);
        }
        self.touch();
    }

    /// A manifest or `zell.lock` of a loaded root changed: its dependency
    /// sources are reloaded.
    pub(crate) fn manifests_changed(&mut self, paths: &[PathBuf]) {
        let mut roots: Vec<PathBuf> = paths.iter().filter_map(|p| p.parent()).filter(|d| self.roots.iter().any(|r| r == d)).map(Path::to_path_buf).collect();
        roots.sort();
        roots.dedup();
        for root in roots {
            let libs = [root.join(".zell/deps"), root.join("vendor")];
            let keys: Vec<String> = self.session.paths().filter(|k| libs.iter().any(|l| Path::new(k).starts_with(l)) && self.session.is_read_only(k)).map(str::to_string).collect();
            for k in keys {
                self.session.remove_file(&k);
                self.uris.remove(&k);
            }
            self.add_dependencies(&root);
        }
    }

    /// Sends `$/logTrace` when tracing is on (`verbose` only at `verbose`).
    pub(crate) fn log_trace(&self, message: &str, verbose: Option<String>) {
        if self.trace == TraceValue::Off {
            return;
        }
        let verbose = verbose.filter(|_| self.trace == TraceValue::Verbose);
        self.notify::<notification::LogTrace>(LogTraceParams { message: message.into(), verbose });
    }

    /// After analysis `a`: asks the client to re-request semantic tokens and
    /// inlay hints when declarations changed, and code lenses when
    /// declarations or reference counts changed, if a document other than
    /// the edited ones is open (the edited one is re-requested anyway).
    /// One analysis sends at most one request of each kind.
    pub(crate) fn refresh_after(&mut self, a: &Analysis) {
        let changed = std::mem::take(&mut self.changed_docs);
        let fp = fingerprint(a);
        let Some(prev) = self.fingerprint.replace(fp) else { return };
        if prev == fp || !self.docs.keys().any(|k| !changed.contains(k)) {
            return;
        }
        if prev.0 != fp.0 {
            if self.caps.tokens_refresh {
                self.server_request::<request::SemanticTokensRefresh>(());
            }
            if self.caps.hints_refresh {
                self.server_request::<request::InlayHintRefreshRequest>(());
            }
        }
        if self.caps.lens_refresh {
            self.server_request::<request::CodeLensRefresh>(());
        }
    }
}

/// (declarations, reference counts) of an analysis: what other files'
/// tokens and hints (declarations: kinds, names, signatures) and lenses
/// (reference counts) depend on.
fn fingerprint(a: &Analysis) -> (u64, u64) {
    let mut counts = vec![0u32; a.index.symbols.len()];
    for r in &a.index.references {
        if let Some(c) = counts.get_mut(r.symbol.0 as usize) {
            *c += 1;
        }
    }
    let (mut decls, mut refs) = (DefaultHasher::new(), DefaultHasher::new());
    for (s, n) in a.index.symbols.iter().zip(counts) {
        if matches!(s.kind, SymbolKind::Local | SymbolKind::Param) {
            continue;
        }
        (s.kind as u8, &s.full_name, &s.signature).hash(&mut decls);
        (&s.full_name, n).hash(&mut refs);
    }
    (decls.finish(), refs.finish())
}
