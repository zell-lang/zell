//! LSP 3.17 conformance: position encodings, CRLF and clamped positions,
//! the lifecycle (before `initialize`, after `shutdown`, `exit`), error
//! codes, closed documents, versioned rename edits and rename conflicts
//! (docs/LSP_CONFORMANCE.md lists what each test covers).

use super::*;

const SRC: &str = "namespace app\n\nimport io\n\nfunc helper() int {\n    return 1\n}\n\nfunc main() {\n    var total = 1\n    var count = 2\n    println(\"é😀中文\", total, count, helper())\n}\n";

/// Column of the `nth` `needle` on its line, in units of `enc`.
fn col_in(text: &str, needle: &str, nth: usize, enc: &str) -> Position {
    let off = text.match_indices(needle).nth(nth).unwrap().0;
    let line_start = text[..off].rfind('\n').map_or(0, |i| i + 1);
    let prefix = &text[line_start..off];
    let character = match enc {
        "utf-8" => prefix.len(),
        "utf-32" => prefix.chars().count(),
        _ => prefix.encode_utf16().count(),
    } as u32;
    Position { line: text[..off].matches('\n').count() as u32, character }
}

fn start_enc(dir: &Path, offered: Option<Vec<&str>>) -> Client {
    Client::start_with(dir, |p| {
        if let Some(o) = offered {
            p.capabilities.general = Some(GeneralClientCapabilities { position_encodings: Some(o.into_iter().map(|e| PositionEncodingKind::from(e.to_string())).collect()), ..Default::default() });
        }
    })
}

fn rename_req(c: &mut Client, uri: &Url, pos: Position, to: &str) -> lsp_server::Response {
    let id = c.send_request(request::Rename::METHOD, serde_json::to_value(RenameParams { text_document_position: tdp(uri, pos), new_name: to.into(), work_done_progress_params: Default::default() }).unwrap());
    c.response(&id)
}

#[test]
fn positions_follow_the_negotiated_encoding() {
    for (offered, enc) in [(None, "utf-16"), (Some(vec!["utf-8", "utf-16"]), "utf-8"), (Some(vec!["utf-32"]), "utf-32"), (Some(vec!["utf-16", "utf-8"]), "utf-16"), (Some(vec!["latin-1"]), "utf-16")] {
        let (dir, uri) = project_with("main.zl", SRC);
        let mut c = start_enc(&dir, offered);
        assert_eq!(c.capabilities.position_encoding.as_ref().map(|k| k.as_str()), Some(enc));
        c.open(&uri, SRC);
        assert_eq!(c.diagnostics(&uri), vec![], "{enc}");
        let use_pos = col_in(SRC, "total", 1, enc);
        // Definition from a use after non-ASCII text on the same line.
        let def = definition(&mut c, &uri, Position { character: use_pos.character + 2, ..use_pos });
        assert_eq!(def.range.start, col_in(SRC, "total", 0, enc), "{enc}");
        // Hover range.
        let h: Option<Hover> = c.request::<request::HoverRequest>(HoverParams { text_document_position_params: tdp(&uri, use_pos), work_done_progress_params: Default::default() });
        let r = h.and_then(|h| h.range).expect("hover range");
        assert_eq!((r.start, r.end.character - r.start.character), (use_pos, 5), "{enc}");
        // Rename edits.
        let edit: WorkspaceEdit = serde_json::from_value(rename_req(&mut c, &uri, use_pos, "sum").result.unwrap()).unwrap();
        let mut starts: Vec<Position> = edit.changes.unwrap()[&uri].iter().map(|e| e.range.start).collect();
        starts.sort_by_key(|p| (p.line, p.character));
        assert_eq!(starts, vec![col_in(SRC, "total", 0, enc), use_pos], "{enc}");
        // Semantic tokens: one of length 5 at the use.
        let toks: Option<SemanticTokensResult> = c.request::<request::SemanticTokensFullRequest>(SemanticTokensParams { text_document: TextDocumentIdentifier { uri: uri.clone() }, work_done_progress_params: Default::default(), partial_result_params: Default::default() });
        let Some(SemanticTokensResult::Tokens(t)) = toks else { panic!() };
        let (mut line, mut col, mut abs) = (0, 0, Vec::new());
        for t in &t.data {
            if t.delta_line > 0 {
                col = 0;
            }
            line += t.delta_line;
            col += t.delta_start;
            abs.push((line, col, t.length));
        }
        assert!(abs.contains(&(use_pos.line, use_pos.character, 5)), "{enc}: {abs:?}");
        // The string token covers the non-ASCII text in the right units.
        let s = col_in(SRC, "\"é", 0, enc);
        let len = col_in(SRC, ", total", 0, enc).character - s.character;
        assert!(abs.contains(&(s.line, s.character, len)), "{enc}: {abs:?}");
        // Incremental sync in the same units: append `!` to the string.
        let in_str = col_in(SRC, "\", total", 0, enc);
        c.notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: 2 },
            content_changes: vec![TextDocumentContentChangeEvent { range: Some(Range { start: in_str, end: in_str }), range_length: None, text: "!".into() }],
        });
        let text: Option<String> = serde_json::from_value(c.request_raw(zell_lsp::DOCUMENT_TEXT, serde_json::json!({ "uri": uri }))).unwrap();
        assert!(text.unwrap().contains("\"é😀中文!\""), "{enc}");
    }
}

#[test]
fn crlf_documents_and_clamped_positions() {
    let crlf = SRC.replace('\n', "\r\n");
    let (dir, uri) = project_with("main.zl", &crlf);
    let mut c = Client::start(&dir);
    c.open(&uri, &crlf);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let def = definition(&mut c, &uri, col_in(&crlf, "total", 1, "utf-16"));
    assert_eq!(def.range.start, col_in(&crlf, "total", 0, "utf-16"));
    assert_eq!(def.range.end.character - def.range.start.character, 5);
    // A character past the end of a CRLF line means before its `\r`; a
    // line past the end means the end of the text.
    let line = col_in(&crlf, "var total", 0, "utf-16").line;
    let past = Position { line, character: 999 };
    let end = Position { line: 999, character: 0 };
    c.notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: 2 },
        content_changes: vec![
            TextDocumentContentChangeEvent { range: Some(Range { start: past, end: past }), range_length: None, text: " // x".into() },
            TextDocumentContentChangeEvent { range: Some(Range { start: end, end }), range_length: None, text: "// end\r\n".into() },
            // A change spanning lines: join `var count` into the line above.
            TextDocumentContentChangeEvent { range: Some(Range { start: Position { line, character: 999 }, end: Position { line: line + 1, character: 4 } }), range_length: None, text: "; ".into() },
        ],
    });
    let text: Option<String> = serde_json::from_value(c.request_raw(zell_lsp::DOCUMENT_TEXT, serde_json::json!({ "uri": uri }))).unwrap();
    let text = text.unwrap();
    assert!(text.contains("var total = 1 // x; var count = 2\r\n"), "{text:?}");
    assert!(text.ends_with("}\r\n// end\r\n"), "{text:?}");
    // didSave with the text replaces the server's copy.
    c.notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams { text_document: TextDocumentIdentifier { uri: uri.clone() }, text: Some(crlf.clone()) });
    let text: Option<String> = serde_json::from_value(c.request_raw(zell_lsp::DOCUMENT_TEXT, serde_json::json!({ "uri": uri }))).unwrap();
    assert_eq!(text.as_deref(), Some(crlf.as_str()));
    assert_eq!(c.diagnostics(&uri), vec![]);
}

/// A server on an in-memory connection, without the test client's
/// automatic initialize/shutdown.
fn raw_server() -> (Connection, JoinHandle<Result<(), String>>) {
    let (server, client) = Connection::memory();
    (client, std::thread::spawn(move || zell_lsp::run(server).map_err(|e| e.to_string())))
}

fn raw_call(c: &Connection, id: i32, method: &str, params: serde_json::Value) -> lsp_server::Response {
    c.sender.send(Message::Request(Request { id: id.into(), method: method.into(), params })).unwrap();
    loop {
        match c.receiver.recv_timeout(Duration::from_secs(30)).expect("no response") {
            Message::Response(r) if r.id == id.into() => return r,
            _ => {}
        }
    }
}

fn raw_init(c: &Connection, dir: &Path) {
    let root = Url::from_file_path(dir).unwrap();
    #[allow(deprecated)]
    let p = InitializeParams { root_uri: Some(root), ..Default::default() };
    assert!(raw_call(c, 1, request::Initialize::METHOD, serde_json::to_value(p).unwrap()).result.is_some());
    c.sender.send(Message::Notification(Notification::new(notification::Initialized::METHOD.into(), InitializedParams {}))).unwrap();
}

#[test]
fn lifecycle_follows_the_specification() {
    let (dir, _) = project(&[]);
    let hover = serde_json::json!({ "textDocument": { "uri": "file:///x.zl" }, "position": { "line": 0, "character": 0 } });
    // Before initialize: ServerNotInitialized.
    let (c, h) = raw_server();
    let r = raw_call(&c, 100, request::HoverRequest::METHOD, hover.clone());
    assert_eq!(r.error.map(|e| e.code), Some(-32002));
    raw_init(&c, &dir);
    // Shutdown, then requests are InvalidRequest; exit after shutdown is clean.
    assert!(raw_call(&c, 2, request::Shutdown::METHOD, serde_json::Value::Null).error.is_none());
    let r = raw_call(&c, 3, request::HoverRequest::METHOD, hover);
    assert_eq!(r.error.map(|e| e.code), Some(lsp_server::ErrorCode::InvalidRequest as i32));
    c.sender.send(Message::Notification(Notification::new(notification::Exit::METHOD.into(), ()))).unwrap();
    assert_eq!(h.join().unwrap(), Ok(()));
    // Exit without shutdown: an error (zell-lsp exits with code 1).
    let (c, h) = raw_server();
    raw_init(&c, &dir);
    c.sender.send(Message::Notification(Notification::new(notification::Exit::METHOD.into(), ()))).unwrap();
    assert!(h.join().unwrap().is_err());
}

#[test]
fn errors_use_the_protocol_codes() {
    let (dir, uri) = project_with("main.zl", SRC);
    let mut c = Client::start(&dir);
    c.open(&uri, SRC);
    let id = c.send_request("textDocument/noSuchThing", serde_json::json!({}));
    assert_eq!(c.response(&id).error.map(|e| e.code), Some(lsp_server::ErrorCode::MethodNotFound as i32));
    let id = c.send_request(request::HoverRequest::METHOD, serde_json::json!({ "textDocument": 42 }));
    assert_eq!(c.response(&id).error.map(|e| e.code), Some(lsp_server::ErrorCode::InvalidParams as i32));
    // Cancelling an unknown or answered id is ignored.
    c.notify::<notification::Cancel>(CancelParams { id: NumberOrString::Number(424242) });
    c.notify::<notification::Cancel>(CancelParams { id: NumberOrString::String("nope".into()) });
    c.notify::<notification::Cancel>(CancelParams { id: NumberOrString::Number(1) });
    assert_eq!(definition(&mut c, &uri, col_in(SRC, "total", 1, "utf-16")).range.start, col_in(SRC, "total", 0, "utf-16"));
}

#[test]
fn closed_unsaved_documents_drop_their_diagnostics_and_answer_null() {
    let (dir, _) = project(&[]);
    let uri = Url::from_file_path(dir.join("scratch.zl")).unwrap(); // never saved
    let mut c = Client::start(&dir);
    c.open(&uri, "namespace app\n\nfunc main() {\n    var x = nope\n}\n");
    assert!(!c.diagnostics(&uri).is_empty());
    c.notify::<notification::DidCloseTextDocument>(DidCloseTextDocumentParams { text_document: TextDocumentIdentifier { uri: uri.clone() } });
    assert_eq!(c.diagnostics(&uri), vec![], "cleared after close");
    let h: Option<Hover> = c.request::<request::HoverRequest>(HoverParams { text_document_position_params: tdp(&uri, Position { line: 3, character: 9 }), work_done_progress_params: Default::default() });
    assert!(h.is_none());
    let t: Option<SemanticTokensResult> = c.request::<request::SemanticTokensFullRequest>(SemanticTokensParams { text_document: TextDocumentIdentifier { uri: uri.clone() }, work_done_progress_params: Default::default(), partial_result_params: Default::default() });
    assert!(t.is_none());
    let f: Option<Vec<TextEdit>> = c.request::<request::Formatting>(DocumentFormattingParams { text_document: TextDocumentIdentifier { uri: uri.clone() }, options: Default::default(), work_done_progress_params: Default::default() });
    assert!(f.is_none_or(|e| e.is_empty()));
}

#[test]
fn rename_is_versioned_null_where_nothing_and_refuses_clashes() {
    let (dir, files) = project(MULTI);
    let mut c = Client::start_with(&dir, |p| {
        p.capabilities.workspace = Some(WorkspaceClientCapabilities { workspace_edit: Some(WorkspaceEditClientCapabilities { document_changes: Some(true), ..Default::default() }), ..Default::default() });
    });
    // Only report.zl is open: the declaration lives in an unopened file.
    let (uri, text) = &files[2];
    c.open(uri, text);
    c.change(uri, 7, text);
    let at = pos_of(text, "app.models.User", 0, 11);
    let r = rename_req(&mut c, uri, at, "Member");
    let edit: WorkspaceEdit = serde_json::from_value(r.result.expect("rename result")).unwrap();
    assert!(edit.changes.is_none());
    let Some(DocumentChanges::Edits(docs)) = edit.document_changes else { panic!("no documentChanges") };
    assert_eq!(docs.len(), 3, "{docs:?}");
    for d in &docs {
        let expect = if &d.text_document.uri == uri { Some(7) } else { None };
        assert_eq!(d.text_document.version, expect, "{}", d.text_document.uri);
    }
    // prepareRename where there is no symbol: null.
    let prep: Option<PrepareRenameResponse> = c.request::<request::PrepareRenameRequest>(tdp(uri, Position { line: 1, character: 0 }));
    assert!(prep.is_none());

    let (dir, uri) = project_with("main.zl", SRC);
    let mut c = Client::start(&dir);
    c.open(&uri, SRC);
    let total = col_in(SRC, "total", 1, "utf-16");
    let e = rename_req(&mut c, &uri, total, "count").error.expect("clash with a local");
    assert!(e.message.contains("clash") && e.message.contains("count"), "{}", e.message);
    let e = rename_req(&mut c, &uri, col_in(SRC, "helper()", 1, "utf-16"), "main").error.expect("clash with a function");
    assert!(e.message.contains("clash"), "{}", e.message);
    assert!(rename_req(&mut c, &uri, total, "sum").error.is_none());
}
