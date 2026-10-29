//! Zell language server (LSP over stdio), built on the compiler library's
//! [`zell::session::Session`] API (docs/TOOLING.md, stages 1-5):
//! diagnostics (pushed, or pulled with `textDocument/diagnostic` and
//! `workspace/diagnostic`), outline, folding, selection ranges,
//! definition, declaration, type definition, implementation, references,
//! rename (also of import paths when files move), hover, highlights,
//! semantic tokens (full, delta, range), completion (with lazy resolve),
//! signature help, inlay hints, code actions, code lenses, formatting
//! (`zell::fmt`, also on type), document links and workspace symbols;
//! `.zl` files created, deleted or renamed on disk are picked up through
//! `workspace/didChangeWatchedFiles` (watchers registered by the server)
//! and `workspace/didRenameFiles`; workspace folders, pulled settings,
//! tracing and refresh requests are in `workspace.rs`.
//!
//! Library sources are part of the project, read-only (docs/TOOLING.md):
//! the standard library from `std/` ([`zell::sema::stdfs::std_dir`],
//! indexed by [`zell::stdindex::StdIndex`]) and the module dependencies
//! that `zell.lock` lists, from `.zell/deps/` (or `vendor/`). Definition,
//! hover, references and workspace symbols reach into them; rename refuses.
//!
//! # Scheduling
//!
//! One thread reads messages; analysis runs on a worker thread over a
//! snapshot of the session ([`Session`] clones share parsed files). Edits
//! (`didChange`, incremental or full) only update the server's copy of the
//! document text and bump the session version; the text is parsed when an
//! analysis starts, so a burst of changes costs one parse and one
//! analysis. Analysis starts [`DEBOUNCE`] after the last change, or at once
//! when a request is waiting. A change cancels the analysis in flight (an
//! `AtomicBool` that sema checks between phases and function bodies, see
//! [`Session::analyze_cancellable`]). Requests are answered against an
//! analysis of the latest text: while it is not ready they wait in a
//! queue, from which `$/cancelRequest` removes them (answered with
//! `RequestCancelled`). Requests that only need the text and its syntax
//! (formatting, folding, selection ranges) are answered at once on the
//! main thread. The others run on a worker pool against a read-only
//! snapshot (see [`snapshot`]), so a long request does not block the main
//! loop; `$/cancelRequest` stops them (`RequestCancelled`), and an edit
//! stops those whose result it invalidates (`ContentModified`).

mod assist;
mod calls;
mod completion;
mod encoding;
mod features;
mod errconv;
mod files;
mod generate;
mod handlers;
mod hierarchy;
mod inline_fn;
mod typehier;
mod scoping;
mod signature;
mod lens;
mod links;
mod nav;
mod refactor;
mod snapshot;
mod sync;
mod workspace;

use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::*;
use zell::session::{Analysis, Session};
use zell::span::Span;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use features::{SEMANTIC_TOKEN_MODIFIERS, SEMANTIC_TOKEN_TYPES};

/// Delay between the last edit and re-analysis.
pub const DEBOUNCE: Duration = Duration::from_millis(100);

/// Upper bound on files loaded by workspace discovery.
const MAX_FILES: usize = 5000;

/// Manifest file names that mark a project root (docs/PACKAGES.md).
const MANIFESTS: &[&str] = &["zell.toml", "zell.json", "zell.yaml", "zell.yml"];

/// `zell/analyzerStatus`: counters of the analysis scheduler (for
/// diagnosing the server and for tests).
pub const ANALYZER_STATUS: &str = "zell/analyzerStatus";

/// `zell/documentText`: the server's copy of an open document's text,
/// `{"uri": ...}` -> string or null (for diagnosing sync problems, and
/// for tests).
pub const DOCUMENT_TEXT: &str = "zell/documentText";

/// `workspace/executeCommand` `zell.changeSignature`: arguments
/// `[{"uri", "position", "params": [{"from": i, "name"?} | {"name", "type",
/// "default"}]}]`; the result is the `WorkspaceEdit` (see `signature.rs`).
pub const CHANGE_SIGNATURE: &str = "zell.changeSignature";

/// Runs the server on stdin/stdout until the client disconnects.
pub fn run_stdio() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (connection, io_threads) = Connection::stdio();
    run(connection)?;
    io_threads.join()?;
    Ok(())
}

/// How diagnostics reach the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagMode {
    /// `textDocument/publishDiagnostics` after every analysis.
    Push,
    /// Only on request (`textDocument/diagnostic`, `workspace/diagnostic`),
    /// with `workspace/diagnostic/refresh` after analyses.
    Pull,
}

/// What the client supports (from `initialize`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ClientCaps {
    work_done_progress: bool,
    pull_diagnostics: bool,
    diagnostic_refresh: bool,
    /// `workspace.workspaceEdit.documentChanges`: edits carry document versions.
    document_changes: bool,
    /// `codeAction.resolveSupport` covers `edit`: refactorings are listed
    /// without their edits and computed by `codeAction/resolve`.
    resolve_edits: bool,
    /// `codeAction.disabledSupport`: refused refactorings are listed
    /// disabled with the reason.
    disabled_actions: bool,
    /// `completionItem.snippetSupport` / `insertReplaceSupport`.
    completion: completion::CompletionCaps,
    /// `workspace.configuration`: settings are pulled.
    configuration: bool,
    /// `workspace.didChangeWatchedFiles.dynamicRegistration`: the server
    /// registers its file watchers.
    watcher_registration: bool,
    /// `workspace.{semanticTokens,inlayHint,codeLens}.refreshSupport`.
    tokens_refresh: bool,
    hints_refresh: bool,
    lens_refresh: bool,
}

impl ClientCaps {
    fn of(p: &InitializeParams) -> ClientCaps {
        let c = &p.capabilities;
        ClientCaps {
            work_done_progress: c.window.as_ref().and_then(|w| w.work_done_progress).unwrap_or(false),
            pull_diagnostics: c.text_document.as_ref().is_some_and(|t| t.diagnostic.is_some()),
            diagnostic_refresh: c.workspace.as_ref().and_then(|w| w.diagnostic.as_ref()).and_then(|d| d.refresh_support).unwrap_or(false),
            document_changes: c.workspace.as_ref().and_then(|w| w.workspace_edit.as_ref()).and_then(|e| e.document_changes).unwrap_or(false),
            resolve_edits: c
                .text_document
                .as_ref()
                .and_then(|t| t.code_action.as_ref())
                .and_then(|ca| ca.resolve_support.as_ref())
                .is_some_and(|r| r.properties.iter().any(|p| p == "edit")),
            disabled_actions: c.text_document.as_ref().and_then(|t| t.code_action.as_ref()).and_then(|ca| ca.disabled_support).unwrap_or(false),
            completion: {
                let ci = c.text_document.as_ref().and_then(|t| t.completion.as_ref()).and_then(|co| co.completion_item.as_ref());
                completion::CompletionCaps {
                    snippets: ci.and_then(|i| i.snippet_support).unwrap_or(false),
                    insert_replace: ci.and_then(|i| i.insert_replace_support).unwrap_or(false),
                }
            },
            configuration: c.workspace.as_ref().and_then(|w| w.configuration).unwrap_or(false),
            watcher_registration: c.workspace.as_ref().and_then(|w| w.did_change_watched_files.as_ref()).and_then(|d| d.dynamic_registration).unwrap_or(false),
            tokens_refresh: c.workspace.as_ref().and_then(|w| w.semantic_tokens.as_ref()).and_then(|d| d.refresh_support).unwrap_or(false),
            hints_refresh: c.workspace.as_ref().and_then(|w| w.inlay_hint.as_ref()).and_then(|d| d.refresh_support).unwrap_or(false),
            lens_refresh: c.workspace.as_ref().and_then(|w| w.code_lens.as_ref()).and_then(|d| d.refresh_support).unwrap_or(false),
        }
    }
}

/// Diagnostics mode: `initializationOptions.diagnostics.mode` = "push" |
/// "pull" | "auto" (default: pull when the client supports it, so it
/// never gets both).
fn diag_mode(opts: Option<&serde_json::Value>, caps: ClientCaps) -> (DiagMode, bool) {
    let asked = opts.and_then(|o| o.pointer("/diagnostics/mode")).and_then(|v| v.as_str()).unwrap_or("auto");
    match asked {
        "push" => (DiagMode::Push, false),
        "pull" => (DiagMode::Pull, true),
        _ if caps.pull_diagnostics => (DiagMode::Pull, true),
        _ => (DiagMode::Push, true),
    }
}

/// Runs the server on `connection` (stdio or an in-memory pair in tests).
pub fn run(connection: Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (id, params) = connection.initialize_start()?;
    let params: InitializeParams = serde_json::from_value(params)?;
    let caps = ClientCaps::of(&params);
    let (mode, advertise_pull) = diag_mode(params.initialization_options.as_ref(), caps);
    let enc = encoding::Encoding::negotiate(&params);
    encoding::set(enc);
    let mut server_caps = capabilities();
    server_caps.position_encoding = Some(enc.kind());
    if !advertise_pull {
        server_caps.diagnostic_provider = None;
    }
    let result = InitializeResult { capabilities: server_caps, server_info: Some(ServerInfo { name: "zell-lsp".into(), version: Some(env!("CARGO_PKG_VERSION").into()) }) };
    let mut result = serde_json::to_value(result)?;
    // Not a field of `ServerCapabilities` in lsp-types 0.95.
    result["capabilities"]["typeHierarchyProvider"] = true.into();
    connection.initialize_finish(id, result)?;
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let mut server = Server::new(connection.sender.clone(), events_tx, caps, mode);
    if let Some(opts) = &params.initialization_options {
        server.hints.update(opts);
        if let Some(ms) = opts.pointer("/debug/analysisDelayMs").and_then(|v| v.as_u64()) {
            server.analysis_delay = Duration::from_millis(ms);
        }
        // Test hook: {"method": ms} slows the handlers of those requests.
        if let Some(m) = opts.pointer("/debug/requestDelayMs").and_then(|v| v.as_object()) {
            server.request_delay = m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_u64()?))).collect();
        }
    }
    server.trace = params.trace.unwrap_or(TraceValue::Off);
    server.initial_index(&params);
    server.after_initialized();
    let result = event_loop(&connection, &mut server, &events_rx);
    if let Some(f) = &server.inflight {
        f.cancel.store(true, Ordering::Relaxed);
    }
    result
}

/// The main loop. Lifecycle per LSP 3.17: after `shutdown` every request
/// is answered with `InvalidRequest` and notifications are dropped; `exit`
/// ends the loop, with an error (process exit code 1) when no `shutdown`
/// came first.
fn event_loop(connection: &Connection, server: &mut Server, events: &Receiver<Event>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut shut_down = false;
    loop {
        if !shut_down {
            server.maybe_start_analysis();
        }
        let tick = match server.next_deadline() {
            Some(d) => crossbeam_channel::at(d),
            None => crossbeam_channel::never(),
        };
        crossbeam_channel::select! {
            recv(connection.receiver) -> msg => {
                let Ok(msg) = msg else { return Ok(()) };
                match msg {
                    Message::Request(req) if shut_down => {
                        server.reply_err(req.id, lsp_server::ErrorCode::InvalidRequest as i32, "the server is shutting down");
                    }
                    Message::Request(req) if req.method == request::Shutdown::METHOD => {
                        if let Some(f) = &server.inflight {
                            f.cancel.store(true, Ordering::Relaxed);
                        }
                        for r in std::mem::take(&mut server.pending) {
                            server.reply_err(r.id, lsp_server::ErrorCode::RequestCanceled as i32, "the server is shutting down");
                        }
                        for r in server.running.values() {
                            r.state.cancel();
                        }
                        server.reply(req.id, ());
                        shut_down = true;
                    }
                    Message::Request(req) => server.on_request(req),
                    Message::Notification(n) if n.method == notification::Exit::METHOD => {
                        return if shut_down { Ok(()) } else { Err("exit without shutdown".into()) };
                    }
                    Message::Notification(_) if shut_down => {}
                    Message::Notification(n) => server.handle_notification(n),
                    Message::Response(r) => server.on_response(r),
                }
            }
            recv(events) -> ev => {
                if let (Ok(ev), false) = (ev, shut_down) {
                    server.on_event(ev);
                }
            }
            recv(tick) -> _ => {}
        }
    }
}

/// What the server supports.
pub fn capabilities() -> ServerCapabilities {
    let sl_files = FileOperationRegistrationOptions {
        filters: vec![
            FileOperationFilter { scheme: Some("file".into()), pattern: FileOperationPattern { glob: "**/*.zl".into(), matches: Some(FileOperationPatternKind::File), options: None } },
            FileOperationFilter { scheme: Some("file".into()), pattern: FileOperationPattern { glob: "**".into(), matches: Some(FileOperationPatternKind::Folder), options: None } },
        ],
    };
    ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::INCREMENTAL),
            save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions { include_text: Some(true) })),
            ..Default::default()
        })),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions { trigger_characters: Some(vec![".".into()]), resolve_provider: Some(true), ..Default::default() }),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".into(), ",".into()]),
            retrigger_characters: Some(vec![",".into()]),
            work_done_progress_options: Default::default(),
        }),
        inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(InlayHintOptions { resolve_provider: Some(true), work_done_progress_options: Default::default() }))),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![CodeActionKind::QUICKFIX, CodeActionKind::REFACTOR_EXTRACT, CodeActionKind::REFACTOR_INLINE, CodeActionKind::REFACTOR_REWRITE, CodeActionKind::SOURCE_ORGANIZE_IMPORTS]),
            resolve_provider: Some(true),
            ..Default::default()
        })),
        code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(true) }),
        execute_command_provider: Some(ExecuteCommandOptions { commands: vec![CHANGE_SIGNATURE.to_string()], ..Default::default() }),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions { first_trigger_character: "}".into(), more_trigger_character: Some(vec!["\n".into()]) }),
        document_link_provider: Some(DocumentLinkOptions { resolve_provider: Some(false), work_done_progress_options: Default::default() }),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions { prepare_provider: Some(true), work_done_progress_options: Default::default() })),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
            legend: features::legend(),
            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
            range: Some(true),
            ..Default::default()
        })),
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("zell".into()),
            inter_file_dependencies: true,
            workspace_diagnostics: true,
            work_done_progress_options: Default::default(),
        })),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities { supported: Some(true), change_notifications: Some(OneOf::Left(true)) }),
            file_operations: Some(WorkspaceFileOperationsServerCapabilities { will_rename: Some(sl_files.clone()), did_rename: Some(sl_files), ..Default::default() }),
        }),
        ..Default::default()
    }
}

/// Messages from analysis and request workers to the main loop.
pub(crate) enum Event {
    Analyzed { version: u64, outcome: Outcome },
    /// A request on the pool was answered (or is to be parked: see
    /// [`Server::finished`]).
    Finished(RequestId, Option<(u64, WorkspaceDiagnosticParams)>),
}

/// A request on the pool: its state, and the document whose edit
/// invalidates it (`None`: any edit of the project; empty: none).
pub(crate) struct Running {
    state: snapshot::ReqState,
    doc: Option<String>,
}

pub(crate) enum Outcome {
    Done(Box<Analysis>),
    Cancelled,
    Panicked,
}

/// The analysis in flight.
struct Inflight {
    version: u64,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

/// Scheduler counters (`zell/analyzerStatus`).
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Stats {
    /// Analyses started.
    started: u64,
    /// Analyses that finished and were used.
    completed: u64,
    /// Analyses cancelled (superseded by an edit) or whose result was stale.
    cancelled: u64,
    /// Requests answered with `RequestCancelled`.
    cancelled_requests: u64,
    /// Requests that waited for an analysis.
    queued_requests: u64,
}

/// Server state.
struct Server {
    sender: Sender<Message>,
    events: Sender<Event>,
    caps: ClientCaps,
    diag_mode: DiagMode,
    /// The standard library sources (read-only, never analysed as project files).
    std_dir: Option<PathBuf>,
    session: Session,
    /// Bumped on every change of the project's text.
    version: u64,
    /// The latest analysis and the version it was made for (or attempted:
    /// after an internal error the previous analysis is kept).
    analysis: Option<Arc<Analysis>>,
    analysis_version: u64,
    inflight: Option<Inflight>,
    /// Time of the last change not yet analysed.
    dirty: Option<Instant>,
    /// Requests waiting for a fresh analysis.
    pending: VecDeque<Request>,
    /// Parked `workspace/diagnostic` requests: answered when a report changes.
    parked: Vec<(RequestId, WorkspaceDiagnosticParams)>,
    /// Requests running (or queued) on the worker pool.
    running: HashMap<RequestId, Running>,
    pool: snapshot::Pool,
    /// Requests cancelled on the pool (added to `stats`).
    worker_cancels: Arc<AtomicU64>,
    /// Test hook: per-method delay of the handlers (`debug.requestDelayMs`).
    request_delay: HashMap<String, u64>,
    /// Texts of open documents (the editor's copy; parsed on flush).
    docs: HashMap<String, String>,
    /// Versions of open documents (from `didOpen` / `didChange`).
    doc_versions: Arc<HashMap<String, i32>>,
    /// Open documents whose text the session has not parsed yet.
    unflushed: HashSet<String>,
    roots: Vec<PathBuf>,
    /// Session path -> URI the client knows the file by.
    uris: Arc<HashMap<String, Url>>,
    /// URIs that got non-empty diagnostics last time.
    /// URIs published last time, with a hash of what was sent (files whose
    /// diagnostics did not change are not published again).
    published: HashMap<Url, u64>,
    /// Last semantic tokens per document: (result id, tokens).
    tokens: Arc<std::sync::Mutex<snapshot::Tokens>>,
    /// Progress token of the initial indexing, ended by the first analysis.
    indexing: Option<NumberOrString>,
    next_server_request: i32,
    /// `zell.inlayHints.*` settings.
    hints: assist::HintSettings,
    /// Artificial delay of each analysis (`initializationOptions.debug.analysisDelayMs`).
    analysis_delay: Duration,
    stats: Stats,
    /// `$/setTrace` / `initialize.trace`: `$/logTrace` output when not off.
    trace: TraceValue,
    /// Ids of pending `workspace/configuration` requests.
    config_requests: HashSet<RequestId>,
    /// Open documents edited since the last analysis (see [`workspace`]'s
    /// refresh requests).
    changed_docs: HashSet<String>,
    /// Fingerprints of the last analysis: (declarations, reference counts).
    fingerprint: Option<(u64, u64)>,
}

/// Session key for a URI: the file path for `file:` URIs.
fn path_of(uri: &Url) -> String {
    match uri.to_file_path() {
        Ok(p) => p.display().to_string(),
        Err(()) => uri.to_string(),
    }
}

/// `data` of an item naming symbol `id` (completion items, inlay hints).
pub(crate) fn symbol_data(a: &Analysis, id: zell::sema::index::SymbolId) -> serde_json::Value {
    let s = a.index.symbol(id);
    serde_json::json!({"sym": id.0, "name": &*s.name, "fullName": &*s.full_name})
}

/// The symbol named by [`symbol_data`] (plus `"v"`, the analysis version
/// it was made in): by id in the same analysis, else by full name.
pub(crate) fn symbol_from_data(a: &Analysis, version: u64, data: &serde_json::Value) -> Option<zell::sema::index::SymbolId> {
    let name = data.get("name")?.as_str()?;
    let full = data.get("fullName")?.as_str()?;
    let id = data.get("sym")?.as_u64()? as usize;
    if data.get("v").and_then(|v| v.as_u64()) == Some(version) {
        if let Some(s) = a.index.symbols.get(id).filter(|s| *s.name == *name && *s.full_name == *full) {
            return Some(s.id);
        }
    }
    a.index.symbols.iter().find(|s| *s.full_name == *full && *s.name == *name).map(|s| s.id)
}

impl Server {
    fn new(sender: Sender<Message>, events: Sender<Event>, caps: ClientCaps, diag_mode: DiagMode) -> Server {
        Server {
            sender,
            events,
            caps,
            diag_mode,
            std_dir: None,
            session: Session::new(),
            version: 1,
            analysis: None,
            analysis_version: 0,
            inflight: None,
            dirty: None,
            pending: VecDeque::new(),
            parked: Vec::new(),
            running: HashMap::new(),
            pool: snapshot::Pool::new(),
            worker_cancels: Arc::default(),
            request_delay: HashMap::new(),
            docs: HashMap::new(),
            doc_versions: Arc::default(),
            unflushed: HashSet::new(),
            roots: Vec::new(),
            uris: Arc::default(),
            published: HashMap::new(),
            tokens: Arc::default(),
            indexing: None,
            next_server_request: 0,
            hints: assist::HintSettings::default(),
            analysis_delay: Duration::ZERO,
            stats: Stats::default(),
            trace: TraceValue::Off,
            config_requests: HashSet::new(),
            changed_docs: HashSet::new(),
            fingerprint: None,
        }
    }

    // -- workspace ---------------------------------------------------------

    /// Loads the std index and the workspace, reporting progress when the
    /// client supports it; the progress ends with the first analysis.
    #[allow(deprecated)]
    fn initial_index(&mut self, params: &InitializeParams) {
        let token = self.progress_begin("Zell", "loading the standard library");
        self.std_dir = zell::sema::stdfs::std_dir();
        self.session.set_std_index(self.std_dir.as_deref().map(|d| Arc::new(zell::stdindex::StdIndex::load(d))));
        if let Some(t) = &token {
            self.progress_report(t, "indexing workspace files", Some(30));
        }
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(folders) = &params.workspace_folders {
            roots.extend(folders.iter().filter_map(|f| f.uri.to_file_path().ok()));
        }
        if roots.is_empty() {
            if let Some(p) = params.root_uri.as_ref().and_then(|u| u.to_file_path().ok()) {
                roots.push(p);
            }
        }
        for r in roots {
            self.add_root(r);
        }
        if let Some(t) = &token {
            let n = self.session.paths().count();
            self.progress_report(t, &format!("analysing {n} file{}", if n == 1 { "" } else { "s" }), Some(60));
        }
        self.indexing = token;
        self.touch();
        // The first analysis starts at once, without the debounce delay.
        self.dirty = Instant::now().checked_sub(DEBOUNCE).or(self.dirty);
    }

    /// Loads every `.zl` file of the project rooted at `root`: `src/**` when
    /// a manifest is present (and `src/` exists), else the whole tree.
    fn add_root(&mut self, root: PathBuf) {
        if self.roots.iter().any(|r| root.starts_with(r)) {
            return;
        }
        let has_manifest = MANIFESTS.iter().any(|m| root.join(m).is_file());
        let src = root.join("src");
        let dir = if has_manifest && src.is_dir() { src } else { root.clone() };
        let mut files = Vec::new();
        collect_sl(&dir, &mut files);
        files.sort();
        for p in files {
            let key = p.display().to_string();
            if self.docs.contains_key(&key) || self.session.contains(&key) {
                continue;
            }
            if let (Ok(text), Ok(uri)) = (std::fs::read_to_string(&p), Url::from_file_path(&p)) {
                Arc::make_mut(&mut self.uris).insert(key.clone(), uri);
                self.session.update_file(key, text);
            }
        }
        if has_manifest {
            self.add_dependencies(&root);
        }
        self.roots.push(root);
        self.touch();
    }

    /// Loads the sources of the modules `zell.lock` lists, from
    /// `vendor/` when vendored, else `.zell/deps/` (read-only).
    fn add_dependencies(&mut self, root: &Path) {
        let Some(lock) = std::fs::read_to_string(root.join("zell.lock")).ok().and_then(|t| zell::package::lock::LockFile::parse(&t).ok()) else { return };
        let vendored = root.join("vendor/modules.yaml").is_file();
        for p in &lock.packages {
            let Some(module) = &p.module else { continue };
            let name = format!("{}@{}", zell::package::store::escape_path(module), p.version);
            let dir = if vendored { root.join("vendor").join(&name) } else { root.join(".zell/deps").join(&name) };
            let mut files = Vec::new();
            collect_sl(&dir.join("src"), &mut files);
            files.sort();
            for f in files {
                let key = f.display().to_string();
                if let (Ok(text), Ok(uri)) = (std::fs::read_to_string(&f), Url::from_file_path(&f)) {
                    Arc::make_mut(&mut self.uris).insert(key.clone(), uri);
                    self.session.update_file(key.clone(), text);
                    self.session.set_read_only(&key, true);
                }
            }
        }
    }

    /// A file of the standard library sources (handled by the std index).
    fn is_std_file(&self, path: &Path) -> bool {
        self.std_dir.as_ref().is_some_and(|d| path.starts_with(d))
    }

    /// For a file outside every known root, loads the project whose
    /// manifest is found walking up from it.
    fn discover_for(&mut self, path: &Path) {
        if self.roots.iter().any(|r| path.starts_with(r)) {
            return;
        }
        let mut dir = path.parent();
        while let Some(d) = dir {
            if MANIFESTS.iter().any(|m| d.join(m).is_file()) {
                let token = self.progress_begin("Zell", &format!("indexing {}", d.display()));
                self.add_root(d.to_path_buf());
                if let Some(t) = token {
                    self.progress_end(&t, "done");
                }
                return;
            }
            dir = d.parent();
        }
    }

    fn in_roots(&self, path: &Path) -> bool {
        self.roots.iter().any(|r| path.starts_with(r))
    }

    // -- analysis scheduling ---------------------------------------------------

    /// Records a change of the project: a new version, analysed after the
    /// debounce delay; the analysis in flight is superseded.
    fn touch(&mut self) {
        self.version += 1;
        self.dirty = Some(Instant::now());
        if let Some(f) = &self.inflight {
            f.cancel.store(true, Ordering::Relaxed);
        }
        // Requests on the pool whose result the change invalidates answer
        // `ContentModified`: project-wide ones on any change, the others on
        // a change of their document.
        for r in self.running.values() {
            if r.doc.as_ref().is_none_or(|d| self.unflushed.contains(d)) {
                r.state.modified();
            }
        }
    }

    /// A read-only snapshot of the latest analysis for a request.
    pub(crate) fn snap(&self) -> Option<snapshot::Snap> {
        Some(snapshot::Snap {
            analysis: self.analysis.clone()?,
            version: self.analysis_version,
            uris: self.uris.clone(),
            doc_versions: self.doc_versions.clone(),
            caps: self.caps,
            hints: self.hints,
            tokens: self.tokens.clone(),
            state: snapshot::ReqState::default(),
            encoding: encoding::get(),
        })
    }

    /// Forgets the semantic tokens of `key`.
    pub(crate) fn forget_tokens(&self, key: &str) {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).forget(key);
    }

    fn fresh(&self) -> bool {
        self.analysis_version == self.version
    }

    /// When the main loop must wake up to start an analysis.
    fn next_deadline(&self) -> Option<Instant> {
        if self.fresh() || self.inflight.as_ref().is_some_and(|f| f.version == self.version) {
            return None;
        }
        if !self.pending.is_empty() {
            return Some(Instant::now());
        }
        self.dirty.map(|t| t + DEBOUNCE)
    }

    /// Starts an analysis of the current version when one is due: the
    /// debounce delay passed, or a request waits for it.
    fn maybe_start_analysis(&mut self) {
        let due = self.next_deadline().is_some_and(|d| d <= Instant::now());
        if !due {
            return;
        }
        // The superseded analysis (already told to stop) finishes on its own.
        self.flush();
        // The previous results are dropped first: requests wait for the new
        // analysis anyway, and this keeps one analysis in memory at a time.
        // Freeing a large index takes milliseconds: another thread does it.
        if let Some(old) = self.analysis.take() {
            std::thread::spawn(move || drop(old));
        }
        let snapshot = self.session.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let version = self.version;
        self.inflight = Some(Inflight { version, cancel: cancel.clone(), started: Instant::now() });
        self.stats.started += 1;
        let events = self.events.clone();
        let delay = self.analysis_delay;
        std::thread::spawn(move || {
            let deadline = Instant::now() + delay;
            while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())));
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| snapshot.analyze_cancellable(Some(&cancel))));
            let outcome = match result {
                Ok(Some(a)) if !cancel.load(Ordering::Relaxed) => Outcome::Done(Box::new(a)),
                Ok(_) => Outcome::Cancelled,
                Err(_) => Outcome::Panicked,
            };
            let _ = events.send(Event::Analyzed { version, outcome });
        });
    }

    /// The current text of a file (project files parsed: see [`Server::flush`]).
    fn current_source(&mut self, uri: &Url) -> Option<zell::span::SourceFile> {
        self.flush();
        let key = path_of(uri);
        match self.session.text(&key) {
            Some(t) => Some(zell::span::SourceFile::new(key.clone(), t.to_string())),
            // A library file: the std sources are in the analysis only.
            None => self.analysis.as_ref().and_then(|a| a.files.iter().find(|f| f.path == key).map(|f| (**f).clone())),
        }
    }

    /// Parses the open documents changed since the last analysis.
    fn flush(&mut self) {
        for key in std::mem::take(&mut self.unflushed) {
            if let Some(text) = self.docs.get(&key) {
                self.session.update_file(key.clone(), text.clone());
            }
            self.changed_docs.insert(key);
        }
    }

    fn on_event(&mut self, ev: Event) {
        let (version, outcome) = match ev {
            Event::Analyzed { version, outcome } => (version, outcome),
            Event::Finished(id, park) => return self.finished(id, park),
        };
        let mut started = None;
        if self.inflight.as_ref().is_some_and(|f| f.version == version) {
            started = self.inflight.take().map(|f| f.started);
        }
        if version != self.version {
            self.stats.cancelled += 1;
            return;
        }
        match outcome {
            Outcome::Done(a) => {
                self.stats.completed += 1;
                if self.trace != TraceValue::Off {
                    let ms = started.map_or(0, |s| s.elapsed().as_millis());
                    self.log_trace(&format!("analysis {version}: {} files, {} diagnostics in {ms} ms", a.files.len(), a.diagnostics.len()), None);
                }
                self.refresh_after(&a);
                self.analysis = Some(Arc::from(a));
                self.analysis_version = version;
                self.dirty = None;
                match self.diag_mode {
                    DiagMode::Push => self.publish(),
                    DiagMode::Pull if self.caps.diagnostic_refresh => {
                        self.server_request::<request::WorkspaceDiagnosticRefresh>(());
                    }
                    DiagMode::Pull => {}
                }
                if let Some(t) = self.indexing.take() {
                    self.progress_end(&t, "done");
                }
            }
            Outcome::Panicked => {
                self.log(MessageType::ERROR, "zell-lsp: internal error during analysis");
                self.analysis_version = version;
                self.dirty = None;
            }
            Outcome::Cancelled => {
                self.stats.cancelled += 1;
                return;
            }
        }
        while let Some(req) = self.pending.pop_front() {
            self.dispatch(req);
        }
        self.wake_parked();
    }

    fn on_request(&mut self, req: Request) {
        // Requests that only need the text (and its syntax) never wait.
        const TEXT_ONLY: &[&str] = &[
            ANALYZER_STATUS,
            DOCUMENT_TEXT,
            request::Formatting::METHOD,
            request::RangeFormatting::METHOD,
            request::OnTypeFormatting::METHOD,
            request::FoldingRangeRequest::METHOD,
            request::SelectionRangeRequest::METHOD,
        ];
        if TEXT_ONLY.contains(&req.method.as_str()) {
            return self.dispatch(req);
        }
        if self.fresh() {
            self.dispatch(req);
        } else {
            self.stats.queued_requests += 1;
            self.pending.push_back(req);
        }
    }

    /// `$/cancelRequest`: a request still waiting (or parked) is answered
    /// with `RequestCancelled` here, one on the pool by its worker
    /// (answered ones are not affected).
    fn cancel_request(&mut self, id: RequestId) {
        let waiting = self.pending.iter().position(|r| r.id == id).map(|i| self.pending.remove(i).map(|r| r.id));
        let parked = self.parked.iter().position(|p| p.0 == id).map(|i| Some(self.parked.remove(i).0));
        if let Some(Some(id)) = waiting.or(parked) {
            self.stats.cancelled_requests += 1;
            self.reply_err(id, lsp_server::ErrorCode::RequestCanceled as i32, "cancelled");
        } else if let Some(r) = self.running.get(&id) {
            // Its worker answers (see `snapshot`).
            r.state.cancel();
        }
    }

    fn publish(&mut self) {
        use std::hash::{Hash, Hasher};
        let Some(snap) = self.snap() else { return };
        let a = &snap.analysis;
        let mut by_file: Vec<Vec<&zell::diagnostics::Diagnostic>> = vec![Vec::new(); a.files.len()];
        for d in &a.diagnostics {
            if let Some(v) = by_file.get_mut(d.file) {
                v.push(d);
            }
        }
        let mut now = HashMap::new();
        let mut out = Vec::new();
        for (i, f) in a.files.iter().enumerate() {
            if a.is_read_only(i) {
                continue; // library files: their problems are not the user's to fix
            }
            let Some(uri) = self.uris.get(&f.path) else { continue };
            // Unchanged diagnostics (of an unchanged text: their ranges) are
            // not sent again.
            let mut h = std::collections::hash_map::DefaultHasher::new();
            if !by_file[i].is_empty() {
                f.text.hash(&mut h);
            }
            for d in &by_file[i] {
                (d.span.start, d.span.end, &d.message, d.code, d.severity == zell::diagnostics::Severity::Error).hash(&mut h);
                for (n, s) in &d.notes {
                    (n, s.map(|s| (s.start, s.end))).hash(&mut h);
                }
                for (n, file, s) in &d.related {
                    (n, file, s.start, s.end).hash(&mut h);
                }
            }
            let h = h.finish();
            now.insert(uri.clone(), h);
            if self.published.get(uri) == Some(&h) {
                continue;
            }
            let diags: Vec<Diagnostic> = by_file[i].iter().map(|d| snap.to_lsp_diag(a, d)).collect();
            out.push(PublishDiagnosticsParams { uri: uri.clone(), diagnostics: diags, version: self.doc_versions.get(&f.path).copied() });
        }
        for uri in self.published.keys() {
            if !now.contains_key(uri) {
                out.push(PublishDiagnosticsParams { uri: uri.clone(), diagnostics: Vec::new(), version: None });
            }
        }
        self.published = now;
        for p in out {
            self.notify::<notification::PublishDiagnostics>(p);
        }
    }

    // -- messages ------------------------------------------------------------

    fn notify<N: notification::Notification>(&self, params: N::Params) {
        let _ = self.sender.send(Message::Notification(Notification::new(N::METHOD.to_string(), params)));
    }

    /// Sends a request to the client; returns its id (responses are
    /// ignored except those [`Server::on_response`] waits for).
    fn server_request<R: request::Request>(&mut self, params: R::Params) -> RequestId {
        self.next_server_request += 1;
        let id = RequestId::from(format!("zell-lsp/{}", self.next_server_request));
        let _ = self.sender.send(Message::Request(Request::new(id.clone(), R::METHOD.to_string(), params)));
        id
    }

    fn log(&self, typ: MessageType, message: &str) {
        self.notify::<notification::LogMessage>(LogMessageParams { typ, message: message.to_string() });
    }

    fn reply(&self, id: RequestId, result: impl serde::Serialize) {
        let _ = self.sender.send(Message::Response(Response::new_ok(id, result)));
    }

    fn reply_err(&self, id: RequestId, code: i32, msg: impl Into<String>) {
        let _ = self.sender.send(Message::Response(Response::new_err(id, code, msg.into())));
    }

    /// Creates a work-done progress (when the client supports them) and
    /// begins it; returns its token.
    fn progress_begin(&mut self, title: &str, message: &str) -> Option<NumberOrString> {
        if !self.caps.work_done_progress {
            return None;
        }
        self.next_server_request += 1;
        let token = NumberOrString::String(format!("zell-lsp/progress/{}", self.next_server_request));
        self.server_request::<request::WorkDoneProgressCreate>(WorkDoneProgressCreateParams { token: token.clone() });
        let begin = WorkDoneProgressBegin { title: title.into(), cancellable: Some(false), message: Some(message.into()), percentage: Some(0) };
        self.notify::<notification::Progress>(ProgressParams { token: token.clone(), value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(begin)) });
        Some(token)
    }

    fn progress_report(&self, token: &NumberOrString, message: &str, percentage: Option<u32>) {
        let report = WorkDoneProgressReport { cancellable: None, message: Some(message.into()), percentage };
        self.notify::<notification::Progress>(ProgressParams { token: token.clone(), value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(report)) });
    }

    fn progress_end(&self, token: &NumberOrString, message: &str) {
        let end = WorkDoneProgressEnd { message: Some(message.into()) };
        self.notify::<notification::Progress>(ProgressParams { token: token.clone(), value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(end)) });
    }

    // -- notifications -----------------------------------------------------------

    fn handle_notification(&mut self, n: Notification) {
        match n.method.as_str() {
            notification::Cancel::METHOD => {
                let Ok(p) = serde_json::from_value::<CancelParams>(n.params) else { return };
                let id = match p.id {
                    NumberOrString::Number(i) => RequestId::from(i),
                    NumberOrString::String(s) => RequestId::from(s),
                };
                self.cancel_request(id);
            }
            notification::DidOpenTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) else { return };
                let key = path_of(&p.text_document.uri);
                if let Ok(path) = p.text_document.uri.to_file_path() {
                    if self.is_std_file(&path) {
                        return;
                    }
                    self.discover_for(&path);
                }
                Arc::make_mut(&mut self.uris).insert(key.clone(), p.text_document.uri);
                Arc::make_mut(&mut self.doc_versions).insert(key.clone(), p.text_document.version);
                self.docs.insert(key.clone(), p.text_document.text);
                self.unflushed.insert(key);
                self.touch();
            }
            notification::DidChangeTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params) else { return };
                let key = path_of(&p.text_document.uri);
                // A stale or out-of-order change (lower version than the
                // recorded one) is dropped instead of corrupting the text.
                if let Some(&known) = self.doc_versions.get(&key).filter(|&&v| p.text_document.version < v) {
                    self.log_trace(&format!("dropped stale didChange of {key}: version {} < {known}", p.text_document.version), None);
                    return;
                }
                let Some(text) = self.docs.get_mut(&key) else { return };
                Arc::make_mut(&mut self.doc_versions).insert(key.clone(), p.text_document.version);
                for change in p.content_changes {
                    sync::apply_change(text, change);
                }
                self.unflushed.insert(key);
                self.touch();
            }
            notification::DidSaveTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidSaveTextDocumentParams>(n.params) else { return };
                let key = path_of(&p.text_document.uri);
                let Some(text) = p.text else { return };
                if let Some(doc) = self.docs.get_mut(&key) {
                    if *doc != text {
                        *doc = text;
                        self.unflushed.insert(key);
                        self.touch();
                    }
                } else if self.session.contains(&key) && !self.session.is_read_only(&key) {
                    self.session.update_file(key, text);
                    self.touch();
                }
            }
            notification::DidChangeWatchedFiles::METHOD => {
                let Ok(p) = serde_json::from_value::<DidChangeWatchedFilesParams>(n.params) else { return };
                let mut manifests = Vec::new();
                for ev in p.changes {
                    let Ok(path) = ev.uri.to_file_path() else { continue };
                    if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "zell.lock" || MANIFESTS.contains(&n)) {
                        manifests.push(path);
                        continue;
                    }
                    if path.extension().is_none_or(|x| x != "zl") {
                        continue;
                    }
                    let key = path.display().to_string();
                    if self.docs.contains_key(&key) {
                        continue; // the editor's text wins
                    }
                    if ev.typ == FileChangeType::DELETED {
                        self.session.remove_file(&key);
                        Arc::make_mut(&mut self.uris).remove(&key);
                    } else if self.in_roots(&path) || self.session.contains(&key) {
                        if let Ok(text) = std::fs::read_to_string(&path) {
                            Arc::make_mut(&mut self.uris).insert(key.clone(), ev.uri);
                            self.session.update_file(key, text);
                        }
                    }
                }
                self.manifests_changed(&manifests);
                self.touch();
            }
            notification::DidRenameFiles::METHOD => {
                let Ok(p) = serde_json::from_value::<RenameFilesParams>(n.params) else { return };
                self.did_rename_files(p);
            }
            notification::DidChangeConfiguration::METHOD => {
                let Ok(p) = serde_json::from_value::<DidChangeConfigurationParams>(n.params) else { return };
                if let Some(zell) = p.settings.get("zell") {
                    self.apply_settings(zell);
                }
                self.pull_configuration();
            }
            notification::DidChangeWorkspaceFolders::METHOD => {
                let Ok(p) = serde_json::from_value::<DidChangeWorkspaceFoldersParams>(n.params) else { return };
                self.folders_changed(p.event);
            }
            notification::SetTrace::METHOD => {
                if let Ok(p) = serde_json::from_value::<SetTraceParams>(n.params) {
                    self.trace = p.value;
                }
            }
            notification::DidCloseTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) else { return };
                let key = path_of(&p.text_document.uri);
                self.forget_tokens(&key);
                Arc::make_mut(&mut self.doc_versions).remove(&key);
                if self.docs.remove(&key).is_none() {
                    return;
                }
                self.unflushed.remove(&key);
                let on_disk = p.text_document.uri.to_file_path().ok().filter(|p| self.in_roots(p)).and_then(|p| std::fs::read_to_string(p).ok());
                match on_disk {
                    Some(text) => self.session.update_file(key, text),
                    None => {
                        self.session.remove_file(&key);
                    }
                }
                self.touch();
            }
            _ => {}
        }
    }

    /// The session files a rename moves: `(old key, new path)` for each
    /// renamed `.zl` file, and for each file under a renamed directory.
    fn renamed_files(&self, p: &RenameFilesParams) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for r in &p.files {
            let (Ok(old), Ok(new)) = (Url::parse(&r.old_uri), Url::parse(&r.new_uri)) else { continue };
            let (Ok(old), Ok(new)) = (old.to_file_path(), new.to_file_path()) else { continue };
            for key in self.session.paths().map(str::to_string).chain(self.docs.keys().cloned()).collect::<HashSet<_>>() {
                let kp = Path::new(&key);
                if kp == old {
                    out.push((key, new.clone()));
                } else if let Ok(rel) = kp.strip_prefix(&old) {
                    out.push((key.clone(), new.join(rel)));
                }
            }
        }
        out.sort();
        out
    }

    /// `workspace/didRenameFiles`: moves the files in the session (open
    /// documents keep their text until the client reopens them).
    fn did_rename_files(&mut self, p: RenameFilesParams) {
        for (old, new) in self.renamed_files(&p) {
            let new_key = new.display().to_string();
            // The editor's copy, else the moved file on disk (the client may
            // have applied willRenameFiles edits to it), else what we had.
            let text = self.docs.remove(&old).or_else(|| std::fs::read_to_string(&new).ok()).or_else(|| self.session.text(&old).map(str::to_string));
            let read_only = self.session.is_read_only(&old);
            self.session.remove_file(&old);
            self.unflushed.remove(&old);
            Arc::make_mut(&mut self.uris).remove(&old);
            self.forget_tokens(&old);
            if let Some(text) = text {
                if let Ok(uri) = Url::from_file_path(&new) {
                    Arc::make_mut(&mut self.uris).insert(new_key.clone(), uri);
                }
                self.session.update_file(new_key.clone(), text);
                self.session.set_read_only(&new_key, read_only);
            }
        }
        self.touch();
    }
}

/// LSP range of a byte span (in the negotiated position encoding).
pub(crate) fn range(f: &zell::span::SourceFile, s: Span) -> Range {
    Range { start: encoding::position(f, s.start), end: encoding::position(f, s.end.max(s.start)) }
}

/// Recursively collects `.zl` files, skipping hidden and build directories.
fn collect_sl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if !name.starts_with('.') && !matches!(name.as_str(), "target" | "node_modules" | "out") {
                collect_sl(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "zl") {
            out.push(p);
        }
    }
}
