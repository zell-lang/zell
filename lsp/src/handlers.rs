//! Request handlers. Those that need the analysis run on the worker pool
//! against a [`Snap`] (see [`crate::snapshot`]); requests that arrive
//! while the analysis is stale wait in the queue first (see the crate
//! docs). The text-only ones (formatting, folding, selection ranges),
//! `workspace/willRenameFiles` and the server's own requests run on the
//! main thread against the current text.

use crate::snapshot::{Answer, Done, Snap};
use crate::{assist, features, files, hierarchy, lens, nav, path_of, range, typehier, Running, Server};
use lsp_server::{Request, RequestId};
use lsp_types::request::Request as _;
use lsp_types::*;
use zell::session::Analysis;
use zell::span::Span;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

/// A handler's answer: the result, or an LSP error code and message.
type Reply<T> = Result<T, (i32, String)>;

const REQUEST_FAILED: i32 = -32803;

/// Requests whose result spans the project: any edit invalidates them
/// (`ContentModified`), not only an edit of their document.
const PROJECT_WIDE: &[&str] = &[
    request::References::METHOD,
    request::Rename::METHOD,
    request::WorkspaceSymbolRequest::METHOD,
    request::CallHierarchyIncomingCalls::METHOD,
    request::CallHierarchyOutgoingCalls::METHOD,
    request::TypeHierarchySupertypes::METHOD,
    request::TypeHierarchySubtypes::METHOD,
    request::CodeLensResolve::METHOD,
];

impl Server {
    /// Runs the handler of `req`: on the pool, or here for the main-thread
    /// ones, which reply at once.
    pub(crate) fn dispatch(&mut self, req: Request) {
        macro_rules! route {
            ($how:ident: $($R:ty => $f:expr),* $(,)?) => {
                match req.method.as_str() {
                    $(<$R>::METHOD => return self.$how::<$R>(req, $f),)*
                    _ => {}
                }
            };
        }
        route! { spawn:
            request::HoverRequest => Snap::hover,
            request::GotoDefinition => Snap::definition,
            request::GotoDeclaration => Snap::definition,
            request::GotoTypeDefinition => Snap::type_definition,
            request::GotoImplementation => Snap::implementation,
            request::References => Snap::references,
            request::DocumentSymbolRequest => Snap::document_symbols,
            request::PrepareRenameRequest => Snap::prepare_rename,
            request::Rename => Snap::rename,
            request::SemanticTokensFullRequest => Snap::semantic_tokens,
            request::SemanticTokensFullDeltaRequest => Snap::semantic_tokens_delta,
            request::SemanticTokensRangeRequest => Snap::semantic_tokens_range,
            request::Completion => Snap::completion,
            request::ResolveCompletionItem => Snap::resolve_completion,
            request::SignatureHelpRequest => Snap::signature_help,
            request::InlayHintRequest => Snap::inlay_hints,
            request::InlayHintResolveRequest => Snap::resolve_inlay_hint,
            request::CodeActionRequest => Snap::code_actions,
            request::CodeActionResolveRequest => Snap::resolve_code_action,
            request::CodeLensRequest => Snap::code_lenses,
            request::CodeLensResolve => Snap::resolve_code_lens,
            request::DocumentLinkRequest => Snap::document_links,
            request::WorkspaceSymbolRequest => Snap::workspace_symbols,
            request::DocumentHighlightRequest => Snap::highlights,
            request::DocumentDiagnosticRequest => Snap::document_diagnostic,
            request::CallHierarchyPrepare => Snap::prepare_call_hierarchy,
            request::CallHierarchyIncomingCalls => Snap::incoming_calls,
            request::CallHierarchyOutgoingCalls => Snap::outgoing_calls,
            request::TypeHierarchyPrepare => Snap::prepare_type_hierarchy,
            request::TypeHierarchySupertypes => Snap::supertypes,
            request::TypeHierarchySubtypes => Snap::subtypes,
        }
        route! { call:
            request::FoldingRangeRequest => Self::folding,
            request::SelectionRangeRequest => Self::selection_ranges,
            request::Formatting => Self::formatting,
            request::RangeFormatting => Self::range_formatting,
            request::OnTypeFormatting => Self::on_type_formatting,
            request::WillRenameFiles => Self::will_rename_files,
        }
        match req.method.as_str() {
            request::WorkspaceDiagnosticRequest::METHOD => match serde_json::from_value::<WorkspaceDiagnosticParams>(req.params) {
                Ok(p) => self.start(req.id, req.method, None, move |s, sender| s.workspace_diagnostic(p, sender)),
                Err(e) => self.reply_err(req.id, lsp_server::ErrorCode::InvalidParams as i32, e.to_string()),
            },
            crate::ANALYZER_STATUS => {
                let mut stats = self.stats;
                stats.cancelled_requests += self.worker_cancels.load(std::sync::atomic::Ordering::Relaxed);
                let status = serde_json::json!({
                    "version": self.version,
                    "analysisVersion": self.analysis_version,
                    "inFlight": self.inflight.is_some(),
                    "queued": self.pending.len(),
                    "running": self.running.len(),
                    "stats": stats,
                });
                self.reply(req.id, status)
            }
            crate::DOCUMENT_TEXT => {
                let text = req.params.get("uri").and_then(|u| serde_json::from_value::<Url>(u.clone()).ok()).and_then(|u| self.docs.get(&path_of(&u)).cloned());
                self.reply(req.id, text)
            }
            _ => self.reply_err(req.id, lsp_server::ErrorCode::MethodNotFound as i32, format!("unsupported request {}", req.method)),
        }
    }

    /// Runs a main-thread handler and replies.
    fn call<R: request::Request>(&mut self, req: Request, f: fn(&mut Self, R::Params) -> Reply<R::Result>) {
        match serde_json::from_value::<R::Params>(req.params) {
            // A handler bug answers its request with 
            // instead of taking the server down.
            Ok(p) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let t = std::time::Instant::now();
                let r = f(self, p);
                self.log_trace(&format!("{} answered in {} ms", req.method, t.elapsed().as_millis()), None);
                r
            })) {
                Ok(Ok(r)) => self.reply(req.id, r),
                Ok(Err((code, msg))) => self.reply_err(req.id, code, msg),
                Err(_) => self.reply_err(req.id, lsp_server::ErrorCode::InternalError as i32, format!("internal error in {}", req.method)),
            },
            Err(e) => {
                eprintln!("zell-lsp: bad params for {}: {e}", req.method);
                self.reply_err(req.id, lsp_server::ErrorCode::InvalidParams as i32, e.to_string());
            }
        }
    }

    /// Sends a handler to the pool; the document it is about (if any) is
    /// recorded so that an edit of it can stop the request.
    fn spawn<R: request::Request>(&mut self, req: Request, f: fn(&Snap, R::Params) -> Reply<R::Result>) {
        let doc = req.params.pointer("/textDocument/uri").and_then(|u| u.as_str()).and_then(|u| Url::parse(u).ok()).map(|u| path_of(&u));
        let doc = if PROJECT_WIDE.contains(&req.method.as_str()) { None } else { doc.or(Some(String::new())) };
        match serde_json::from_value::<R::Params>(req.params) {
            Ok(p) => self.start(req.id, req.method, doc, move |s, _| f(s, p).map(|r| Done::Reply(serde_json::to_value(r).unwrap_or_default()))),
            Err(e) => {
                eprintln!("zell-lsp: bad params for {}: {e}", req.method);
                self.reply_err(req.id, lsp_server::ErrorCode::InvalidParams as i32, e.to_string());
            }
        }
    }

    /// Runs `f` on the pool against a snapshot of the latest analysis.
    /// `doc`: the document whose edit invalidates the request (`None`: any
    /// edit of the project; the empty string: none).
    fn start(&mut self, id: RequestId, method: String, doc: Option<String>, f: impl FnOnce(&Snap, &crossbeam_channel::Sender<lsp_server::Message>) -> Reply<Done> + Send + 'static) {
        let Some(snap) = self.snap() else {
            return self.reply_err(id, REQUEST_FAILED, "no analysis available (internal error during analysis)");
        };
        let delay = std::time::Duration::from_millis(self.request_delay.get(&method).copied().unwrap_or(0));
        let doc = if method == request::WorkspaceDiagnosticRequest::METHOD { Some(String::new()) } else { doc };
        self.running.insert(id.clone(), Running { state: snap.state.clone(), doc });
        let sender = self.sender.clone();
        let answer = Answer { id, method, sender: sender.clone(), events: self.events.clone(), cancelled: self.worker_cancels.clone(), delay, trace: self.trace != TraceValue::Off };
        self.pool.run(Box::new(move || answer.run(&snap, |s| f(s, &sender))));
    }

    /// A request on the pool finished; `park`: a `workspace/diagnostic`
    /// request with nothing new, answered when a report changes.
    pub(crate) fn finished(&mut self, id: RequestId, park: Option<(u64, WorkspaceDiagnosticParams)>) {
        let running = self.running.remove(&id);
        let Some((version, p)) = park else { return };
        if running.is_some_and(|r| r.state.is_cancelled()) {
            self.stats.cancelled_requests += 1;
            return self.reply_err(id, lsp_server::ErrorCode::RequestCanceled as i32, "cancelled");
        }
        if version != self.analysis_version && self.fresh() {
            // A newer analysis came meanwhile: report against it.
            return self.start(id, request::WorkspaceDiagnosticRequest::METHOD.into(), None, move |s, sender| s.workspace_diagnostic(p, sender));
        }
        self.parked.push((id, p));
    }

    /// Re-runs the parked `workspace/diagnostic` requests against the new
    /// analysis (answered when their reports changed, else parked again).
    pub(crate) fn wake_parked(&mut self) {
        for (id, p) in std::mem::take(&mut self.parked) {
            self.start(id, request::WorkspaceDiagnosticRequest::METHOD.into(), None, move |s, sender| s.workspace_diagnostic(p, sender));
        }
    }

    // -- main-thread handlers (the current text) ----------------------------------

    fn selection_ranges(&mut self, p: SelectionRangeParams) -> Reply<Option<Vec<SelectionRange>>> {
        let Some(f) = self.current_source(&p.text_document.uri) else { return Ok(None) };
        let parsed;
        let ast = match self.session.ast(&f.path) {
            Some(ast) => ast,
            None => {
                let mut d = zell::diagnostics::Diagnostics::new();
                parsed = zell::parser::parse_file(zell::lexer::lex(&f.text, &mut d), &mut d);
                &parsed
            }
        };
        Ok(Some(
            p.positions
                .iter()
                .map(|pos| {
                    let off = crate::encoding::offset(&f, *pos);
                    // Built from the outermost range inwards.
                    let mut sel: Option<SelectionRange> = None;
                    for s in nav::selection_chain(ast, &f.text, off).iter().rev() {
                        sel = Some(SelectionRange { range: range(&f, *s), parent: sel.map(Box::new) });
                    }
                    sel.unwrap_or(SelectionRange { range: Range { start: *pos, end: *pos }, parent: None })
                })
                .collect(),
        ))
    }

    /// `workspace/willRenameFiles`: import and namespace edits for files
    /// whose path-derived namespace changes (see [`files`]).
    fn will_rename_files(&mut self, p: RenameFilesParams) -> Reply<Option<WorkspaceEdit>> {
        let Some(s) = self.snap() else { return Ok(None) };
        let a = &s.analysis;
        let moves: Vec<(usize, PathBuf)> = self.renamed_files(&p).into_iter().filter_map(|(old, new)| Some((a.file_index(&old)?, new))).collect();
        let changes = files::rename_edits(a, &moves, files::derived_namespace, |f| s.uri_of(a, f));
        Ok((!changes.is_empty()).then(|| s.workspace_edit(changes)))
    }

    fn folding(&mut self, p: FoldingRangeParams) -> Reply<Option<Vec<FoldingRange>>> {
        Ok(self.current_source(&p.text_document.uri).map(|f| features::folding_ranges(&f)))
    }

    fn formatting(&mut self, p: DocumentFormattingParams) -> Reply<Option<Vec<TextEdit>>> {
        Ok(self.current_source(&p.text_document.uri).and_then(|f| assist::format_edits(&f, None)))
    }

    fn range_formatting(&mut self, p: DocumentRangeFormattingParams) -> Reply<Option<Vec<TextEdit>>> {
        Ok(self.current_source(&p.text_document.uri).and_then(|f| assist::format_edits(&f, Some((p.range.start.line, p.range.end.line)))))
    }

    fn on_type_formatting(&mut self, p: DocumentOnTypeFormattingParams) -> Reply<Option<Vec<TextEdit>>> {
        let tdp = p.text_document_position;
        Ok(self.current_source(&tdp.text_document.uri).and_then(|f| assist::on_type_edits(&f, tdp.position, &p.ch)))
    }
}

impl Snap {
    fn workspace_diagnostic(&self, p: WorkspaceDiagnosticParams, sender: &crossbeam_channel::Sender<lsp_server::Message>) -> Reply<Done> {
        let report = |r| serde_json::to_value(WorkspaceDiagnosticReportResult::Report(r)).unwrap_or_default();
        Ok(match self.workspace_report(&p.previous_result_ids) {
            // Partial results: one  per file, then an empty answer.
            Some(r) if p.partial_result_params.partial_result_token.is_some() => {
                let token = p.partial_result_params.partial_result_token.clone().unwrap_or(NumberOrString::Number(0));
                for item in r.items {
                    let value = WorkspaceDiagnosticReportPartialResult { items: vec![item] };
                    let params = serde_json::json!({"token": token, "value": value});
                    let _ = sender.send(lsp_server::Message::Notification(lsp_server::Notification::new("$/progress".into(), params)));
                }
                Done::Reply(report(WorkspaceDiagnosticReport { items: Vec::new() }))
            }
            Some(r) => Done::Reply(report(r)),
            None => Done::Park(self.version, p),
        })
    }

    /// The analysis plus the file index and byte offset of a position.
    fn locate(&self, doc: &TextDocumentIdentifier, pos: Option<Position>) -> Option<(Arc<Analysis>, usize, u32)> {
        let a = self.analysis.clone();
        let file = a.file_index(&path_of(&doc.uri))?;
        let offset = pos.map(|p| crate::encoding::offset(&a.files[file], p)).unwrap_or(0);
        Some((a, file, offset))
    }

    fn location(&self, a: &Analysis, (file, span): nav::Target) -> Option<Location> {
        Some(Location { uri: self.uri_of(a, file)?, range: range(&a.files[file], span) })
    }

    // -- navigation ----------------------------------------------------------

    fn hover(&self, p: HoverParams) -> Reply<Option<Hover>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| {
            let sym = a.symbol_at(file, off)?;
            let value = features::hover_markdown(&a, sym);
            let r = a.name_span_at(file, off).map(|s| range(&a.files[file], s));
            Some(Hover { contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }), range: r })
        }))
    }

    fn definition(&self, p: GotoDefinitionParams) -> Reply<Option<GotoDefinitionResponse>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| {
            let loc = a.definition_at(file, off)?;
            self.location(&a, (loc.file, loc.name_span)).map(GotoDefinitionResponse::Scalar)
        }))
    }

    fn type_definition(&self, p: GotoDefinitionParams) -> Reply<Option<GotoDefinitionResponse>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| {
            let t = nav::type_definition(&a, file, off)?;
            self.location(&a, t).map(GotoDefinitionResponse::Scalar)
        }))
    }

    fn implementation(&self, p: GotoDefinitionParams) -> Reply<Option<GotoDefinitionResponse>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).map(|(a, file, off)| {
            let locs = nav::implementations(&a, file, off).into_iter().filter_map(|t| self.location(&a, t)).collect();
            GotoDefinitionResponse::Array(locs)
        }))
    }

    fn references(&self, p: ReferenceParams) -> Reply<Option<Vec<Location>>> {
        let tdp = p.text_document_position;
        let include_decl = p.context.include_declaration;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| {
            let sym = a.symbol_at(file, off)?;
            let mut out = Vec::new();
            if include_decl {
                if let Some(l) = a.index.symbol(sym).loc {
                    out.push((l.file, l.name_span));
                }
            }
            out.extend(a.references_of(sym).into_iter().map(|r| (r.file, r.span)));
            Some(out.into_iter().take_while(|_| !self.stopped()).filter_map(|t| self.location(&a, t)).collect())
        }))
    }

    fn document_symbols(&self, p: DocumentSymbolParams) -> Reply<Option<DocumentSymbolResponse>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| {
            let f = &a.files[file];
            DocumentSymbolResponse::Nested(a.document_symbols(file).iter().map(|s| features::doc_symbol(f, s)).collect())
        }))
    }

    fn highlights(&self, p: DocumentHighlightParams) -> Reply<Option<Vec<DocumentHighlight>>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| assist::highlights(&a, file, off)))
    }

    fn workspace_symbols(&self, p: WorkspaceSymbolParams) -> Reply<Option<WorkspaceSymbolResponse>> {
        let a = &self.analysis;
        Ok(Some(WorkspaceSymbolResponse::Flat(assist::workspace_symbols(a, &p.query, || self.stopped(), |f| self.uri_of(a, f)))))
    }

    // -- rename ------------------------------------------------------------------

    /// The renamable symbol at a position, or a reason why there is none.
    fn rename_target(a: &Analysis, file: usize, off: u32) -> Result<(zell::sema::index::SymbolId, Span), String> {
        let sym_id = a.symbol_at(file, off).ok_or("no symbol at this position")?;
        let sym = a.index.symbol(sym_id);
        if sym.is_std || sym.loc.is_none() {
            return Err(format!("{} is part of the standard library or a builtin and cannot be renamed", sym.name));
        }
        if sym.loc.is_some_and(|l| a.is_read_only(l.file)) || a.is_read_only(file) {
            return Err(format!("{} is declared in a read-only dependency (.zell/deps) and cannot be renamed", sym.name));
        }
        let span = a.name_span_at(file, off).ok_or("no name at this position")?;
        Ok((sym_id, span))
    }

    fn prepare_call_hierarchy(&self, p: CallHierarchyPrepareParams) -> Reply<Option<Vec<CallHierarchyItem>>> {
        let tdp = p.text_document_position_params;
        let v = self.version;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| hierarchy::prepare(&a, v, file, off, &|f| self.uri_of(&a, f))))
    }

    fn incoming_calls(&self, p: CallHierarchyIncomingCallsParams) -> Reply<Option<Vec<CallHierarchyIncomingCall>>> {
        let v = self.version;
        let a = &self.analysis;
        Ok(hierarchy::incoming(a, v, &p.item, &|f| self.uri_of(a, f)))
    }

    fn outgoing_calls(&self, p: CallHierarchyOutgoingCallsParams) -> Reply<Option<Vec<CallHierarchyOutgoingCall>>> {
        let v = self.version;
        let a = &self.analysis;
        Ok(hierarchy::outgoing(a, v, &p.item, &|f| self.uri_of(a, f)))
    }

    fn prepare_type_hierarchy(&self, p: TypeHierarchyPrepareParams) -> Reply<Option<Vec<TypeHierarchyItem>>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| typehier::prepare(&a, file, off, &|f| self.uri_of(&a, f))))
    }

    fn supertypes(&self, p: TypeHierarchySupertypesParams) -> Reply<Option<Vec<TypeHierarchyItem>>> {
        let a = &self.analysis;
        Ok(typehier::related(a, &p.item, true, &|f| self.uri_of(a, f)))
    }

    fn subtypes(&self, p: TypeHierarchySubtypesParams) -> Reply<Option<Vec<TypeHierarchyItem>>> {
        let a = &self.analysis;
        Ok(typehier::related(a, &p.item, false, &|f| self.uri_of(a, f)))
    }

    fn prepare_rename(&self, p: TextDocumentPositionParams) -> Reply<Option<PrepareRenameResponse>> {
        let Some((a, file, off)) = self.locate(&p.text_document, Some(p.position)) else { return Ok(None) };
        // Nothing to rename here: null (the client says so); a symbol that
        // cannot be renamed: an error with the reason.
        if a.symbol_at(file, off).is_none() || a.name_span_at(file, off).is_none() {
            return Ok(None);
        }
        let (sym, span) = Self::rename_target(&a, file, off).map_err(|m| (REQUEST_FAILED, m))?;
        let placeholder = a.index.symbol(sym).name.to_string();
        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder { range: range(&a.files[file], span), placeholder }))
    }

    fn rename(&self, p: RenameParams) -> Reply<Option<WorkspaceEdit>> {
        let tdp = p.text_document_position;
        let Some((a, file, off)) = self.locate(&tdp.text_document, Some(tdp.position)) else { return Ok(None) };
        if !features::is_identifier(&p.new_name) {
            return Err((REQUEST_FAILED, format!("`{}` is not a valid Zell identifier", p.new_name)));
        }
        let (sym_id, _) = Self::rename_target(&a, file, off).map_err(|m| (REQUEST_FAILED, m))?;
        let sym = a.index.symbol(sym_id);
        let mut spans: Vec<(usize, Span)> = sym.loc.map(|l| (l.file, l.name_span)).into_iter().collect();
        spans.extend(a.references_of(sym_id).into_iter().map(|r| (r.file, r.span)));
        if let Some(other) = Self::rename_conflict(&a, sym_id, &spans, &p.new_name) {
            return Err((REQUEST_FAILED, other));
        }
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        let mut seen = HashSet::new();
        for (f, s) in spans {
            if self.stopped() {
                break;
            }
            if !seen.insert((f, s.start)) || a.files[f].text.get(s.start as usize..s.end as usize) != Some(&*sym.name) {
                continue;
            }
            let Some(uri) = self.uri_of(&a, f) else { continue };
            changes.entry(uri).or_default().push(TextEdit { range: range(&a.files[f], s), new_text: p.new_name.clone() });
        }
        Ok(Some(self.workspace_edit(changes)))
    }

    /// Why renaming `sym` (written at `spans`) to `new_name` would change
    /// what a name means: a sibling member, a local of the same block or
    /// one that would capture a use or be captured (`scoping.rs`, by block
    /// scopes), or for a global a name of the namespace or an import
    /// visible at a use.
    fn rename_conflict(a: &Analysis, sym: zell::sema::index::SymbolId, spans: &[(usize, Span)], new_name: &str) -> Option<String> {
        use zell::sema::index::SymbolKind as K;
        let s = a.index.symbol(sym);
        if *s.name == *new_name {
            return None;
        }
        let clash = |other: zell::sema::index::SymbolId| {
            let o = a.index.symbol(other);
            let at = o.loc.map(|l| format!(" ({}:{})", a.files[l.file].path, a.files[l.file].line_col(l.name_span.start).0)).unwrap_or_default();
            format!("renaming `{}` to `{new_name}` would clash with `{}`{at}", s.name, o.full_name)
        };
        if let Some(c) = s.container.filter(|_| !matches!(s.kind, K::Local | K::Param | K::Const)) {
            if let Some(o) = a.index.symbols.iter().find(|o| o.container == Some(c) && *o.name == *new_name && o.id != sym) {
                return Some(clash(o.id));
            }
        }
        if matches!(s.kind, K::Field | K::Method | K::Variant) {
            return None; // members are reached through a value: only siblings clash
        }
        if let Some(o) = crate::scoping::conflict(a, sym, spans, new_name) {
            return Some(clash(o));
        }
        if matches!(s.kind, K::Local | K::Param) || matches!(s.key, zell::sema::index::SymKey::LocalConst(..)) {
            return None; // a local hides globals of the name where they are not used
        }
        spans.iter().find_map(|&(f, _)| crate::completion::global_name(a, f, new_name).filter(|&o| o != sym).map(clash))
    }

    /// A workspace edit of `changes`: versioned `documentChanges` when the
    /// client supports them (open documents carry their version, others
    /// null), else plain `changes`.
    fn workspace_edit(&self, changes: HashMap<Url, Vec<TextEdit>>) -> WorkspaceEdit {
        if !self.caps.document_changes {
            return WorkspaceEdit { changes: Some(changes), ..Default::default() };
        }
        let mut docs: Vec<TextDocumentEdit> = changes
            .into_iter()
            .map(|(uri, edits)| TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { version: self.doc_versions.get(&path_of(&uri)).copied(), uri },
                edits: edits.into_iter().map(OneOf::Left).collect(),
            })
            .collect();
        docs.sort_by(|a, b| a.text_document.uri.as_str().cmp(b.text_document.uri.as_str()));
        WorkspaceEdit { document_changes: Some(DocumentChanges::Edits(docs)), ..Default::default() }
    }

    // -- semantic tokens ---------------------------------------------------------

    fn semantic_tokens(&self, p: SemanticTokensParams) -> Reply<Option<SemanticTokensResult>> {
        let Some((a, file, _)) = self.locate(&p.text_document, None) else { return Ok(None) };
        let data = features::semantic_tokens(&a, file);
        let (result_id, _) = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).remember(path_of(&p.text_document.uri), data.clone(), None);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: Some(result_id), data })))
    }

    /// Edits from the tokens with `previous_result_id` when the server still
    /// has them, else the full tokens.
    fn semantic_tokens_delta(&self, p: SemanticTokensDeltaParams) -> Reply<Option<SemanticTokensFullDeltaResult>> {
        let Some((a, file, _)) = self.locate(&p.text_document, None) else { return Ok(None) };
        let key = path_of(&p.text_document.uri);
        let data = features::semantic_tokens(&a, file);
        let (result_id, old) = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).remember(key, data.clone(), Some(&p.previous_result_id));
        let previous = old.map(|old| features::token_edits(&old, &data));
        Ok(Some(match previous {
            Some(edits) => SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta { result_id: Some(result_id), edits }),
            None => SemanticTokensFullDeltaResult::Tokens(SemanticTokens { result_id: Some(result_id), data }),
        }))
    }

    fn semantic_tokens_range(&self, p: SemanticTokensRangeParams) -> Reply<Option<SemanticTokensRangeResult>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| SemanticTokensRangeResult::Tokens(SemanticTokens { result_id: None, data: features::semantic_tokens_range(&a, file, &p.range) })))
    }

    // -- editing assistance --------------------------------------------------------

    fn completion(&self, p: CompletionParams) -> Reply<Option<CompletionResponse>> {
        let tdp = p.text_document_position;
        let version = self.version;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).map(|(a, file, off)| {
            let mut list = crate::completion::completions(&a, file, off, self.caps.completion);
            for it in &mut list.items {
                if let Some(serde_json::Value::Object(d)) = &mut it.data {
                    d.insert("v".into(), version.into());
                }
            }
            CompletionResponse::List(list)
        }))
    }

    fn resolve_completion(&self, item: CompletionItem) -> Reply<CompletionItem> {
        Ok(crate::completion::resolve_completion(&self.analysis, self.version, item))
    }

    fn signature_help(&self, p: SignatureHelpParams) -> Reply<Option<SignatureHelp>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| assist::signature_help(&a, file, off)))
    }

    fn inlay_hints(&self, p: InlayHintParams) -> Reply<Option<Vec<InlayHint>>> {
        let hints = self.hints;
        let version = self.version;
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| {
            let within = assist::span_of(&a.files[file], &p.range);
            let mut out = assist::inlay_hints(&a, file, within, hints);
            for h in &mut out {
                if let Some(serde_json::Value::Object(d)) = &mut h.data {
                    d.insert("v".into(), version.into());
                }
            }
            out
        }))
    }

    fn resolve_inlay_hint(&self, hint: InlayHint) -> Reply<InlayHint> {
        Ok(assist::resolve_inlay_hint(&self.analysis, self.version, hint))
    }

    fn code_actions(&self, p: CodeActionParams) -> Reply<Option<CodeActionResponse>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| {
            let span = assist::span_of(&a.files[file], &p.range);
            let only = p.context.only.as_deref();
            let mut out = if only.is_none_or(|o| o.iter().any(|k| k.as_str() == "quickfix")) {
                let version = self.doc_versions.get(&path_of(&p.text_document.uri)).copied();
                assist::fixes_from_data(&p.context.diagnostics, &p.text_document.uri, version)
                    .unwrap_or_else(|| assist::code_actions(&a, file, &p.text_document.uri, span, |d| self.to_lsp_diag(&a, d)))
            } else {
                Vec::new()
            };
            if !a.is_read_only(file) {
                out.extend(crate::refactor::actions(&a, file, &p.text_document.uri, span, self.caps.resolve_edits, only));
            }
            out
        }))
    }

    /// Quick fixes always carry their edit (returned as is); refactorings
    /// listed lazily get theirs here.
    fn resolve_code_action(&self, action: CodeAction) -> Reply<CodeAction> {
        let a = &self.analysis;
        Ok(crate::refactor::resolve(a, |u| a.file_index(&path_of(u)), action))
    }

    fn code_lenses(&self, p: CodeLensParams) -> Reply<Option<Vec<CodeLens>>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| lens::code_lenses(&a, file, &p.text_document.uri)))
    }

    fn resolve_code_lens(&self, l: CodeLens) -> Reply<CodeLens> {
        let a = &self.analysis;
        Ok(lens::resolve_lens(a, l, |u| a.file_index(&path_of(u)), |f| self.uri_of(a, f)))
    }

    fn document_links(&self, p: DocumentLinkParams) -> Reply<Option<Vec<DocumentLink>>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| crate::links::document_links(&a, file, |f| self.uri_of(&a, f))))
    }

    // -- pull diagnostics ------------------------------------------------------------

    /// The report of file `i`: its diagnostics and a result id that
    /// changes exactly when they do.
    fn report(&self, a: &Analysis, i: usize) -> (String, Vec<Diagnostic>) {
        let items = self.file_diagnostics(a, i);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&items).unwrap_or_default().hash(&mut h);
        (format!("{:016x}", h.finish()), items)
    }

    fn document_diagnostic(&self, p: DocumentDiagnosticParams) -> Reply<DocumentDiagnosticReportResult> {
        let (result_id, items) = match self.locate(&p.text_document, None) {
            Some((a, file, _)) => self.report(&a, file),
            None => self.report_of_nothing(),
        };
        let report = if p.previous_result_id.as_deref() == Some(result_id.as_str()) {
            DocumentDiagnosticReport::Unchanged(RelatedUnchangedDocumentDiagnosticReport { related_documents: None, unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport { result_id } })
        } else {
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport { related_documents: None, full_document_diagnostic_report: FullDocumentDiagnosticReport { result_id: Some(result_id), items } })
        };
        Ok(DocumentDiagnosticReportResult::Report(report))
    }

    fn report_of_nothing(&self) -> (String, Vec<Diagnostic>) {
        (format!("{:016x}", 0), Vec::new())
    }

    /// The workspace report against `previous`: `None` when every file is
    /// unchanged (the request then stays parked until a report changes,
    /// so clients that re-request at once do not spin).
    fn workspace_report(&self, previous: &[PreviousResultId]) -> Option<WorkspaceDiagnosticReport> {
        let a = self.analysis.clone();
        let mut items = Vec::new();
        let mut changed = false;
        for i in 0..a.files.len() {
            if self.stopped() {
                return None;
            }
            if a.is_read_only(i) {
                continue;
            }
            let Some(uri) = self.uri_of(&a, i) else { continue };
            let (result_id, diags) = self.report(&a, i);
            let version = self.doc_versions.get(&a.files[i].path).map(|&v| i64::from(v));
            let known = previous.iter().find(|p| p.uri == uri).map(|p| p.value.as_str());
            if known == Some(result_id.as_str()) {
                items.push(WorkspaceDocumentDiagnosticReport::Unchanged(WorkspaceUnchangedDocumentDiagnosticReport {
                    uri,
                    version,
                    unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport { result_id },
                }));
            } else {
                changed = true;
                items.push(WorkspaceDocumentDiagnosticReport::Full(WorkspaceFullDocumentDiagnosticReport {
                    uri,
                    version,
                    full_document_diagnostic_report: FullDocumentDiagnosticReport { result_id: Some(result_id), items: diags },
                }));
            }
        }
        // Files that disappeared get an empty report.
        for p in previous {
            if !items.iter().any(|it| matches!(it, WorkspaceDocumentDiagnosticReport::Full(f) if f.uri == p.uri) || matches!(it, WorkspaceDocumentDiagnosticReport::Unchanged(u) if u.uri == p.uri)) {
                changed = true;
                items.push(WorkspaceDocumentDiagnosticReport::Full(WorkspaceFullDocumentDiagnosticReport { uri: p.uri.clone(), version: None, full_document_diagnostic_report: FullDocumentDiagnosticReport::default() }));
            }
        }
        changed.then_some(WorkspaceDiagnosticReport { items })
    }
}
