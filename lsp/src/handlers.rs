//! Request handlers. Each runs on the main thread against the latest
//! analysis (requests that arrive while it is stale wait in the queue,
//! see the crate docs), or against the current text for the text-only
//! ones, and returns its result or an error.

use crate::{assist, features, files, hierarchy, lens, nav, path_of, range, Server};
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

impl Server {
    /// Runs the handler of `req` and sends its response.
    pub(crate) fn dispatch(&mut self, req: Request) {
        macro_rules! route {
            ($($R:ty => $f:expr),* $(,)?) => {
                match req.method.as_str() {
                    $(<$R>::METHOD => return self.call::<$R>(req, $f),)*
                    _ => {}
                }
            };
        }
        route! {
            request::HoverRequest => Self::hover,
            request::GotoDefinition => Self::definition,
            request::GotoDeclaration => Self::definition,
            request::GotoTypeDefinition => Self::type_definition,
            request::GotoImplementation => Self::implementation,
            request::References => Self::references,
            request::DocumentSymbolRequest => Self::document_symbols,
            request::PrepareRenameRequest => Self::prepare_rename,
            request::Rename => Self::rename,
            request::SemanticTokensFullRequest => Self::semantic_tokens,
            request::SemanticTokensFullDeltaRequest => Self::semantic_tokens_delta,
            request::SemanticTokensRangeRequest => Self::semantic_tokens_range,
            request::FoldingRangeRequest => Self::folding,
            request::SelectionRangeRequest => Self::selection_ranges,
            request::Completion => Self::completion,
            request::ResolveCompletionItem => Self::resolve_completion,
            request::SignatureHelpRequest => Self::signature_help,
            request::InlayHintRequest => Self::inlay_hints,
            request::InlayHintResolveRequest => Self::resolve_inlay_hint,
            request::CodeActionRequest => Self::code_actions,
            request::CodeActionResolveRequest => Self::resolve_code_action,
            request::CodeLensRequest => Self::code_lenses,
            request::CodeLensResolve => Self::resolve_code_lens,
            request::Formatting => Self::formatting,
            request::RangeFormatting => Self::range_formatting,
            request::WorkspaceSymbolRequest => Self::workspace_symbols,
            request::DocumentHighlightRequest => Self::highlights,
            request::DocumentDiagnosticRequest => Self::document_diagnostic,
            request::WillRenameFiles => Self::will_rename_files,
            request::CallHierarchyPrepare => Self::prepare_call_hierarchy,
            request::CallHierarchyIncomingCalls => Self::incoming_calls,
            request::CallHierarchyOutgoingCalls => Self::outgoing_calls,
        }
        match req.method.as_str() {
            request::WorkspaceDiagnosticRequest::METHOD => match serde_json::from_value::<WorkspaceDiagnosticParams>(req.params) {
                Ok(p) => self.workspace_diagnostic(req.id, p),
                Err(e) => self.reply_err(req.id, lsp_server::ErrorCode::InvalidParams as i32, e.to_string()),
            },
            crate::ANALYZER_STATUS => {
                let status = serde_json::json!({
                    "version": self.version,
                    "analysisVersion": self.analysis_version,
                    "inFlight": self.inflight.is_some(),
                    "queued": self.pending.len(),
                    "stats": self.stats,
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

    fn call<R: request::Request>(&mut self, req: Request, f: fn(&mut Self, R::Params) -> Reply<R::Result>) {
        match serde_json::from_value::<R::Params>(req.params) {
            // A handler bug answers its request with `InternalError`
            // instead of taking the server down.
            Ok(p) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self, p))) {
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

    /// The analysis plus the file index and byte offset of a position.
    fn locate(&self, doc: &TextDocumentIdentifier, pos: Option<Position>) -> Option<(Arc<Analysis>, usize, u32)> {
        let a = self.analysis.clone()?;
        let file = a.file_index(&path_of(&doc.uri))?;
        let offset = pos.map(|p| crate::encoding::offset(&a.files[file], p)).unwrap_or(0);
        Some((a, file, offset))
    }

    fn location(&self, a: &Analysis, (file, span): nav::Target) -> Option<Location> {
        Some(Location { uri: self.uri_of(a, file)?, range: range(&a.files[file], span) })
    }

    // -- navigation ----------------------------------------------------------

    fn hover(&mut self, p: HoverParams) -> Reply<Option<Hover>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| {
            let sym = a.symbol_at(file, off)?;
            let value = features::hover_markdown(&a, sym);
            let r = a.name_span_at(file, off).map(|s| range(&a.files[file], s));
            Some(Hover { contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }), range: r })
        }))
    }

    fn definition(&mut self, p: GotoDefinitionParams) -> Reply<Option<GotoDefinitionResponse>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| {
            let loc = a.definition_at(file, off)?;
            self.location(&a, (loc.file, loc.name_span)).map(GotoDefinitionResponse::Scalar)
        }))
    }

    fn type_definition(&mut self, p: GotoDefinitionParams) -> Reply<Option<GotoDefinitionResponse>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| {
            let t = nav::type_definition(&a, file, off)?;
            self.location(&a, t).map(GotoDefinitionResponse::Scalar)
        }))
    }

    fn implementation(&mut self, p: GotoDefinitionParams) -> Reply<Option<GotoDefinitionResponse>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).map(|(a, file, off)| {
            let locs = nav::implementations(&a, file, off).into_iter().filter_map(|t| self.location(&a, t)).collect();
            GotoDefinitionResponse::Array(locs)
        }))
    }

    fn references(&mut self, p: ReferenceParams) -> Reply<Option<Vec<Location>>> {
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
            Some(out.into_iter().filter_map(|t| self.location(&a, t)).collect())
        }))
    }

    fn document_symbols(&mut self, p: DocumentSymbolParams) -> Reply<Option<DocumentSymbolResponse>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| {
            let f = &a.files[file];
            DocumentSymbolResponse::Nested(a.document_symbols(file).iter().map(|s| features::doc_symbol(f, s)).collect())
        }))
    }

    fn highlights(&mut self, p: DocumentHighlightParams) -> Reply<Option<Vec<DocumentHighlight>>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| assist::highlights(&a, file, off)))
    }

    fn workspace_symbols(&mut self, p: WorkspaceSymbolParams) -> Reply<Option<WorkspaceSymbolResponse>> {
        Ok(self.analysis.clone().map(|a| WorkspaceSymbolResponse::Flat(assist::workspace_symbols(&a, &p.query, |f| self.uri_of(&a, f)))))
    }

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

    fn prepare_call_hierarchy(&mut self, p: CallHierarchyPrepareParams) -> Reply<Option<Vec<CallHierarchyItem>>> {
        let tdp = p.text_document_position_params;
        let v = self.analysis_version;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| hierarchy::prepare(&a, v, file, off, &|f| self.uri_of(&a, f))))
    }

    fn incoming_calls(&mut self, p: CallHierarchyIncomingCallsParams) -> Reply<Option<Vec<CallHierarchyIncomingCall>>> {
        let v = self.analysis_version;
        Ok(self.analysis.clone().and_then(|a| hierarchy::incoming(&a, v, &p.item, &|f| self.uri_of(&a, f))))
    }

    fn outgoing_calls(&mut self, p: CallHierarchyOutgoingCallsParams) -> Reply<Option<Vec<CallHierarchyOutgoingCall>>> {
        let v = self.analysis_version;
        Ok(self.analysis.clone().and_then(|a| hierarchy::outgoing(&a, v, &p.item, &|f| self.uri_of(&a, f))))
    }

    fn prepare_rename(&mut self, p: TextDocumentPositionParams) -> Reply<Option<PrepareRenameResponse>> {
        let Some((a, file, off)) = self.locate(&p.text_document, Some(p.position)) else { return Ok(None) };
        // Nothing to rename here: null (the client says so); a symbol that
        // cannot be renamed: an error with the reason.
        if a.symbol_at(file, off).is_none() || a.name_span_at(file, off).is_none() {
            return Ok(None);
        }
        let (sym, span) = Self::rename_target(&a, file, off).map_err(|m| (REQUEST_FAILED, m))?;
        let placeholder = a.index.symbol(sym).name.clone();
        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder { range: range(&a.files[file], span), placeholder }))
    }

    fn rename(&mut self, p: RenameParams) -> Reply<Option<WorkspaceEdit>> {
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
            if !seen.insert((f, s.start)) || a.files[f].text.get(s.start as usize..s.end as usize) != Some(sym.name.as_str()) {
                continue;
            }
            let Some(uri) = self.uri_of(&a, f) else { continue };
            changes.entry(uri).or_default().push(TextEdit { range: range(&a.files[f], s), new_text: p.new_name.clone() });
        }
        Ok(Some(self.workspace_edit(changes)))
    }

    /// Why renaming `sym` (written at `spans`) to `new_name` would change
    /// what a name means: a sibling member or a name visible at one of the
    /// uses (a local, a global of the namespace or an import) already
    /// called `new_name`.
    fn rename_conflict(a: &Analysis, sym: zell::sema::index::SymbolId, spans: &[(usize, Span)], new_name: &str) -> Option<String> {
        use zell::sema::index::SymbolKind as K;
        let s = a.index.symbol(sym);
        if s.name == new_name {
            return None;
        }
        let clash = |other: zell::sema::index::SymbolId| {
            let o = a.index.symbol(other);
            let at = o.loc.map(|l| format!(" ({}:{})", a.files[l.file].path, a.files[l.file].line_col(l.name_span.start).0)).unwrap_or_default();
            format!("renaming `{}` to `{new_name}` would clash with `{}`{at}", s.name, o.full_name)
        };
        if let Some(c) = s.container.filter(|_| !matches!(s.kind, K::Local | K::Param | K::Const)) {
            if let Some(o) = a.index.symbols.iter().find(|o| o.container == Some(c) && o.name == new_name && o.id != sym) {
                return Some(clash(o.id));
            }
        }
        if matches!(s.kind, K::Field | K::Method | K::Variant) {
            return None; // members are reached through a value: only siblings clash
        }
        spans.iter().find_map(|&(f, sp)| features::resolve_name(a, f, sp.start, new_name).filter(|&o| o != sym).map(clash))
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

    /// `workspace/willRenameFiles`: import and namespace edits for files
    /// whose path-derived namespace changes (see [`files`]).
    fn will_rename_files(&mut self, p: RenameFilesParams) -> Reply<Option<WorkspaceEdit>> {
        let Some(a) = self.analysis.clone() else { return Ok(None) };
        let moves: Vec<(usize, PathBuf)> = self.renamed_files(&p).into_iter().filter_map(|(old, new)| Some((a.file_index(&old)?, new))).collect();
        let changes = files::rename_edits(&a, &moves, files::derived_namespace, |f| self.uri_of(&a, f));
        Ok((!changes.is_empty()).then(|| self.workspace_edit(changes)))
    }

    // -- semantic tokens ---------------------------------------------------------

    /// Remembers `data` as the last tokens of `key`; returns the result id.
    fn remember_tokens(&mut self, key: String, data: Vec<SemanticToken>) -> String {
        self.token_seq += 1;
        let id = self.token_seq.to_string();
        self.tokens.insert(key, (id.clone(), data));
        id
    }

    fn semantic_tokens(&mut self, p: SemanticTokensParams) -> Reply<Option<SemanticTokensResult>> {
        let Some((a, file, _)) = self.locate(&p.text_document, None) else { return Ok(None) };
        let data = features::semantic_tokens(&a, file);
        let result_id = self.remember_tokens(path_of(&p.text_document.uri), data.clone());
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: Some(result_id), data })))
    }

    /// Edits from the tokens with `previous_result_id` when the server still
    /// has them, else the full tokens.
    fn semantic_tokens_delta(&mut self, p: SemanticTokensDeltaParams) -> Reply<Option<SemanticTokensFullDeltaResult>> {
        let Some((a, file, _)) = self.locate(&p.text_document, None) else { return Ok(None) };
        let key = path_of(&p.text_document.uri);
        let data = features::semantic_tokens(&a, file);
        let previous = self.tokens.get(&key).filter(|(id, _)| *id == p.previous_result_id).map(|(_, old)| features::token_edits(old, &data));
        let result_id = self.remember_tokens(key, data.clone());
        Ok(Some(match previous {
            Some(edits) => SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta { result_id: Some(result_id), edits }),
            None => SemanticTokensFullDeltaResult::Tokens(SemanticTokens { result_id: Some(result_id), data }),
        }))
    }

    fn semantic_tokens_range(&mut self, p: SemanticTokensRangeParams) -> Reply<Option<SemanticTokensRangeResult>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| SemanticTokensRangeResult::Tokens(SemanticTokens { result_id: None, data: features::semantic_tokens_range(&a, file, &p.range) })))
    }

    // -- editing assistance --------------------------------------------------------

    fn folding(&mut self, p: FoldingRangeParams) -> Reply<Option<Vec<FoldingRange>>> {
        Ok(self.current_source(&p.text_document.uri).map(|f| features::folding_ranges(&f)))
    }

    fn completion(&mut self, p: CompletionParams) -> Reply<Option<CompletionResponse>> {
        let tdp = p.text_document_position;
        let version = self.analysis_version;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).map(|(a, file, off)| {
            let mut items = features::completions(&a, file, off);
            for it in &mut items {
                if let Some(serde_json::Value::Object(d)) = &mut it.data {
                    d.insert("v".into(), version.into());
                }
            }
            CompletionResponse::Array(items)
        }))
    }

    fn resolve_completion(&mut self, item: CompletionItem) -> Reply<CompletionItem> {
        Ok(match self.analysis.clone() {
            Some(a) => features::resolve_completion(&a, self.analysis_version, item),
            None => item,
        })
    }

    fn signature_help(&mut self, p: SignatureHelpParams) -> Reply<Option<SignatureHelp>> {
        let tdp = p.text_document_position_params;
        Ok(self.locate(&tdp.text_document, Some(tdp.position)).and_then(|(a, file, off)| assist::signature_help(&a, file, off)))
    }

    fn inlay_hints(&mut self, p: InlayHintParams) -> Reply<Option<Vec<InlayHint>>> {
        let hints = self.hints;
        let version = self.analysis_version;
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

    fn resolve_inlay_hint(&mut self, hint: InlayHint) -> Reply<InlayHint> {
        Ok(match self.analysis.clone() {
            Some(a) => assist::resolve_inlay_hint(&a, self.analysis_version, hint),
            None => hint,
        })
    }

    fn code_actions(&mut self, p: CodeActionParams) -> Reply<Option<CodeActionResponse>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| {
            let span = assist::span_of(&a.files[file], &p.range);
            assist::code_actions(&a, file, &p.text_document.uri, span, |d| self.to_lsp_diag(&a, d))
        }))
    }

    /// Quick fixes are cheap and always carry their edit: returned as is.
    fn resolve_code_action(&mut self, action: CodeAction) -> Reply<CodeAction> {
        Ok(action)
    }

    fn code_lenses(&mut self, p: CodeLensParams) -> Reply<Option<Vec<CodeLens>>> {
        Ok(self.locate(&p.text_document, None).map(|(a, file, _)| lens::code_lenses(&a, file, &p.text_document.uri)))
    }

    fn resolve_code_lens(&mut self, l: CodeLens) -> Reply<CodeLens> {
        let Some(a) = self.analysis.clone() else { return Ok(l) };
        Ok(lens::resolve_lens(&a, l, |u| a.file_index(&path_of(u)), |f| self.uri_of(&a, f)))
    }

    fn formatting(&mut self, p: DocumentFormattingParams) -> Reply<Option<Vec<TextEdit>>> {
        Ok(self.current_source(&p.text_document.uri).and_then(|f| assist::format_edits(&f, None)))
    }

    fn range_formatting(&mut self, p: DocumentRangeFormattingParams) -> Reply<Option<Vec<TextEdit>>> {
        Ok(self.current_source(&p.text_document.uri).and_then(|f| assist::format_edits(&f, Some((p.range.start.line, p.range.end.line)))))
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

    fn document_diagnostic(&mut self, p: DocumentDiagnosticParams) -> Reply<DocumentDiagnosticReportResult> {
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
        let a = self.analysis.clone()?;
        let mut items = Vec::new();
        let mut changed = false;
        for i in 0..a.files.len() {
            if a.is_read_only(i) {
                continue;
            }
            let Some(uri) = self.uri_of(&a, i) else { continue };
            let (result_id, diags) = self.report(&a, i);
            let known = previous.iter().find(|p| p.uri == uri).map(|p| p.value.as_str());
            if known == Some(result_id.as_str()) {
                items.push(WorkspaceDocumentDiagnosticReport::Unchanged(WorkspaceUnchangedDocumentDiagnosticReport {
                    uri,
                    version: None,
                    unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport { result_id },
                }));
            } else {
                changed = true;
                items.push(WorkspaceDocumentDiagnosticReport::Full(WorkspaceFullDocumentDiagnosticReport {
                    uri,
                    version: None,
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

    fn workspace_diagnostic(&mut self, id: RequestId, p: WorkspaceDiagnosticParams) {
        match self.workspace_report(&p.previous_result_ids) {
            Some(r) => self.reply(id, WorkspaceDiagnosticReportResult::Report(r)),
            None => self.parked.push((id, p)),
        }
    }

    /// Answers the parked `workspace/diagnostic` requests whose reports
    /// changed with the new analysis.
    pub(crate) fn wake_parked(&mut self) {
        for (id, p) in std::mem::take(&mut self.parked) {
            self.workspace_diagnostic(id, p);
        }
    }
}
