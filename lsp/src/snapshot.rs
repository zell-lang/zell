//! Requests off the main thread (docs/TOOLING.md "Concurrency").
//!
//! The main loop owns the mutable state (session, document texts, the
//! analysis in flight). A request that needs the analysis is answered on a
//! small thread pool against a [`Snap`]: the immutable [`Analysis`] and the
//! tables it is read with (URIs, document versions), all behind `Arc`, so
//! taking a snapshot copies a few pointers. Meanwhile the main loop keeps
//! reading notifications, starting analyses and answering other requests.
//!
//! Every request on the pool has a [`ReqState`] that the main loop flips:
//! `$/cancelRequest` makes it answer `RequestCancelled` (-32800), an edit
//! that invalidates its result makes it answer `ContentModified` (-32801).
//! The worker checks the state before it starts, in the long loops
//! ([`Snap::stopped`]) and once more before it replies, so a request is
//! answered exactly once, always by its worker.

use crate::{assist, range};
use crossbeam_channel::Sender;
use lsp_server::{Message, RequestId, Response};
use lsp_types::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use zell::session::Analysis;

/// `ContentModified`: the result was invalidated by an edit.
pub(crate) const CONTENT_MODIFIED: i32 = -32801;

const LIVE: u8 = 0;
const CANCELLED: u8 = 1;
const MODIFIED: u8 = 2;

/// The state of a request on the pool, shared with the main loop.
#[derive(Clone, Default)]
pub(crate) struct ReqState(Arc<AtomicU8>);

impl ReqState {
    pub(crate) fn cancel(&self) {
        let _ = self.0.compare_exchange(LIVE, CANCELLED, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub(crate) fn modified(&self) {
        let _ = self.0.compare_exchange(LIVE, MODIFIED, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed) == CANCELLED
    }

    /// The error a stopped request answers with.
    fn error(&self) -> Option<(i32, String)> {
        match self.0.load(Ordering::Relaxed) {
            CANCELLED => Some((lsp_server::ErrorCode::RequestCanceled as i32, "cancelled".into())),
            MODIFIED => Some((CONTENT_MODIFIED, "the document changed".into())),
            _ => None,
        }
    }
}

/// Last semantic tokens per document (for `full/delta`), shared by the
/// workers.
#[derive(Default)]
pub(crate) struct Tokens {
    seq: u64,
    map: HashMap<String, (String, Vec<SemanticToken>)>,
}

impl Tokens {
    /// Remembers `data` as the last tokens of `key`; returns the result id
    /// and, when `previous` names the tokens remembered before, those.
    pub(crate) fn remember(&mut self, key: String, data: Vec<SemanticToken>, previous: Option<&str>) -> (String, Option<Vec<SemanticToken>>) {
        self.seq += 1;
        let id = self.seq.to_string();
        let old = self.map.insert(key, (id.clone(), data)).filter(|(old_id, _)| Some(old_id.as_str()) == previous).map(|(_, d)| d);
        (id, old)
    }

    pub(crate) fn forget(&mut self, key: &str) {
        self.map.remove(key);
    }
}

/// A read-only view of the server for one request (cheap to make: `Arc`s).
#[derive(Clone)]
pub(crate) struct Snap {
    pub(crate) analysis: Arc<Analysis>,
    /// The server version the analysis was made for.
    pub(crate) version: u64,
    pub(crate) uris: Arc<HashMap<String, Url>>,
    pub(crate) doc_versions: Arc<HashMap<String, i32>>,
    pub(crate) caps: crate::ClientCaps,
    pub(crate) hints: assist::HintSettings,
    pub(crate) tokens: Arc<Mutex<Tokens>>,
    pub(crate) state: ReqState,
    /// The negotiated position encoding (thread-local: set by the worker).
    pub(crate) encoding: crate::encoding::Encoding,
}

/// `Snap` crosses threads: everything in it is immutable or synchronised.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Snap>();
};

impl Snap {
    /// Whether the request was cancelled or its result invalidated: long
    /// loops stop early (the worker then answers with the error).
    pub(crate) fn stopped(&self) -> bool {
        self.state.0.load(Ordering::Relaxed) != LIVE
    }

    pub(crate) fn uri_of(&self, a: &Analysis, file: usize) -> Option<Url> {
        let path = &a.files.get(file)?.path;
        self.uris.get(path).cloned().or_else(|| Url::from_file_path(path).ok())
    }

    /// The LSP diagnostics of file `i` (none for library files).
    pub(crate) fn file_diagnostics(&self, a: &Analysis, i: usize) -> Vec<Diagnostic> {
        if a.is_read_only(i) {
            return Vec::new();
        }
        a.diagnostics_for(i).map(|d| self.to_lsp_diag(a, d)).collect()
    }

    pub(crate) fn to_lsp_diag(&self, a: &Analysis, d: &zell::diagnostics::Diagnostic) -> Diagnostic {
        let file = &a.files[d.file];
        let mut message = d.message.clone();
        let mut related = Vec::new();
        for (note, span) in &d.notes {
            match (span, self.uris.get(&file.path)) {
                (Some(s), Some(uri)) => related.push(DiagnosticRelatedInformation { location: Location { uri: uri.clone(), range: range(file, *s) }, message: note.clone() }),
                _ => message.push_str(&format!("\nnote: {note}")),
            }
        }
        // Notes about other files (e.g. the other declaration of a duplicate).
        for (note, other, s) in &d.related {
            match (a.files.get(*other), self.uri_of(a, *other)) {
                (Some(f), Some(uri)) => related.push(DiagnosticRelatedInformation { location: Location { uri, range: range(f, *s) }, message: note.clone() }),
                _ => message.push_str(&format!("\nnote: {note}")),
            }
        }
        // The quick fixes travel with the diagnostic: `codeAction` uses them
        // when the document is still at this version.
        let data = (!d.fixes.is_empty()).then(|| assist::fixes_data(file, &d.fixes, self.doc_versions.get(&file.path).copied()));
        Diagnostic {
            range: range(file, d.span),
            severity: Some(match d.severity {
                zell::diagnostics::Severity::Error => DiagnosticSeverity::ERROR,
                zell::diagnostics::Severity::Warning => DiagnosticSeverity::WARNING,
                zell::diagnostics::Severity::Info => DiagnosticSeverity::INFORMATION,
                zell::diagnostics::Severity::Hint => DiagnosticSeverity::HINT,
            }),
            source: Some("zell".into()),
            code: d.code.map(|c| NumberOrString::String(c.into())),
            message,
            related_information: (!related.is_empty()).then_some(related),
            // Unused code is faded out by editors.
            tags: (matches!(d.code, Some(zell::diagnostics::codes::UNUSED_VARIABLE | zell::diagnostics::codes::UNUSED_IMPORT)) || d.code.and_then(zell::sema::lint::lint).is_some_and(|l| l.unnecessary))
                .then(|| vec![DiagnosticTag::UNNECESSARY]),
            data,
            ..Default::default()
        }
    }
}

/// A job for the pool.
pub(crate) type Job = Box<dyn FnOnce() + Send>;

/// The worker threads (at least two, so a long request never blocks a
/// short one; at most four). They end when the [`Pool`] is dropped.
pub(crate) struct Pool {
    jobs: Sender<Job>,
}

impl Pool {
    pub(crate) fn new() -> Pool {
        let n = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 4);
        let (jobs, rx) = crossbeam_channel::unbounded::<Job>();
        for i in 0..n {
            let rx = rx.clone();
            let _ = std::thread::Builder::new().name(format!("zell-lsp-worker-{i}")).spawn(move || {
                for job in rx {
                    job();
                }
            });
        }
        Pool { jobs }
    }

    pub(crate) fn run(&self, job: Job) {
        let _ = self.jobs.send(job);
    }
}

/// What a handler on the pool produced.
pub(crate) enum Done {
    /// The result to send.
    Reply(serde_json::Value),
    /// No answer yet: a `workspace/diagnostic` request with no changed
    /// report, parked by the main loop (the analysis version it saw).
    Park(u64, WorkspaceDiagnosticParams),
}

/// What a worker needs besides the snapshot to answer a request.
pub(crate) struct Answer {
    pub(crate) id: RequestId,
    pub(crate) method: String,
    pub(crate) sender: Sender<Message>,
    pub(crate) events: Sender<crate::Event>,
    /// `requestCancelled` counter shared with `zell/analyzerStatus`.
    pub(crate) cancelled: Arc<AtomicU64>,
    /// Test hook: wait this long before running the handler
    /// (`initializationOptions.debug.requestDelayMs`).
    pub(crate) delay: std::time::Duration,
    /// `$/logTrace` of the answer time.
    pub(crate) trace: bool,
}

impl Answer {
    /// Runs `f` (after the test delay, unless the request stopped) and
    /// sends the response: the stop error when the state was flipped
    /// meanwhile, `InternalError` when the handler panicked. Then tells the
    /// main loop the request is finished.
    pub(crate) fn run(self, snap: &Snap, f: impl FnOnce(&Snap) -> Result<Done, (i32, String)>) {
        crate::encoding::set(snap.encoding);
        let t = std::time::Instant::now();
        let deadline = t + self.delay;
        while !snap.stopped() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5).min(deadline.saturating_duration_since(std::time::Instant::now())));
        }
        let result = if snap.stopped() {
            Err((0, String::new()))
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(snap))).unwrap_or_else(|_| Err((lsp_server::ErrorCode::InternalError as i32, format!("internal error in {}", self.method))))
        };
        let response = match (snap.state.error(), result) {
            (Some((code, msg)), _) => {
                if code == lsp_server::ErrorCode::RequestCanceled as i32 {
                    self.cancelled.fetch_add(1, Ordering::Relaxed);
                }
                Response::new_err(self.id.clone(), code, msg)
            }
            (None, Ok(Done::Reply(v))) => Response::new_ok(self.id.clone(), v),
            (None, Ok(Done::Park(version, p))) => {
                let _ = self.events.send(crate::Event::Finished(self.id, Some((version, p))));
                return;
            }
            (None, Err((code, msg))) => Response::new_err(self.id.clone(), code, msg),
        };
        if self.trace {
            let params = LogTraceParams { message: format!("{} answered in {} ms", self.method, t.elapsed().as_millis()), verbose: None };
            let _ = self.sender.send(Message::Notification(lsp_server::Notification::new("$/logTrace".into(), params)));
        }
        let _ = self.sender.send(Message::Response(response));
        let _ = self.events.send(crate::Event::Finished(self.id, None));
    }
}
