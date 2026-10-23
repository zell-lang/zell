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

#[test]
fn call_hierarchy_incoming_and_outgoing() {
    let (dir, uri) = project_with("main.zl", SRC);
    let mut c = Client::start(&dir);
    assert!(c.capabilities.call_hierarchy_provider.is_some());
    c.open(&uri, SRC);
    let items: Option<Vec<CallHierarchyItem>> = c.request::<request::CallHierarchyPrepare>(CallHierarchyPrepareParams { text_document_position_params: tdp(&uri, pos_of(SRC, "helper()", 0, 1)), work_done_progress_params: Default::default() });
    let helper = items.unwrap().pop().unwrap();
    assert_eq!((helper.name.as_str(), helper.selection_range.start), ("helper", pos_of(SRC, "helper()", 0, 0)));
    let inc: Option<Vec<CallHierarchyIncomingCall>> = c.request::<request::CallHierarchyIncomingCalls>(CallHierarchyIncomingCallsParams { item: helper, work_done_progress_params: Default::default(), partial_result_params: Default::default() });
    let inc = inc.unwrap();
    assert_eq!(inc.len(), 1, "{inc:?}");
    assert_eq!(inc[0].from.name, "main");
    assert_eq!(inc[0].from_ranges, vec![Range { start: col_in(SRC, "helper()", 1, "utf-16"), end: Position { character: col_in(SRC, "helper()", 1, "utf-16").character + 6, ..col_in(SRC, "helper()", 1, "utf-16") } }]);
    let out: Option<Vec<CallHierarchyOutgoingCall>> = c.request::<request::CallHierarchyOutgoingCalls>(CallHierarchyOutgoingCallsParams { item: inc[0].from.clone(), work_done_progress_params: Default::default(), partial_result_params: Default::default() });
    let out = out.unwrap();
    assert!(out.iter().any(|o| o.to.name == "helper" && o.from_ranges.len() == 1), "{out:?}");
    // Not a function: nothing to prepare.
    let none: Option<Vec<CallHierarchyItem>> = c.request::<request::CallHierarchyPrepare>(CallHierarchyPrepareParams { text_document_position_params: tdp(&uri, pos_of(SRC, "total", 0, 1)), work_done_progress_params: Default::default() });
    assert!(none.is_none());
}

/// Big files, edit storms and broken syntax at every nesting level.
#[test]
fn robustness_large_files_edit_storms_and_broken_syntax() {
    // 20k lines: parse, analysis and full semantic tokens in reasonable time.
    let mut big = String::from("namespace app\n\nimport io\n\n");
    for i in 0..2500 {
        big.push_str(&format!("func f{i}(a int) int {{\n    var b = a + {i}\n    if b > 3 {{\n        return b\n    }}\n    return a\n}}\n\n"));
    }
    big.push_str("func main() {\n    println(f1(2))\n}\n");
    assert!(big.lines().count() >= 20_000);
    let (dir, uri) = project_with("main.zl", &big);
    let mut c = Client::start(&dir);
    c.open(&uri, &big);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let t0 = std::time::Instant::now();
    let toks: Option<SemanticTokensResult> = c.request::<request::SemanticTokensFullRequest>(SemanticTokensParams { text_document: TextDocumentIdentifier { uri: uri.clone() }, work_done_progress_params: Default::default(), partial_result_params: Default::default() });
    let elapsed = t0.elapsed();
    assert!(matches!(toks, Some(SemanticTokensResult::Tokens(ref t)) if t.data.len() > 20_000));
    assert!(elapsed < Duration::from_secs(1), "semantic tokens took {elapsed:?}");
    eprintln!("20k-line semantic tokens: {elapsed:?}");

    // 200 rapid changes: at most a few analyses start, none piles up.
    for v in 0..200 {
        c.change(&uri, v + 2, &big.replacen("var b = a + 0", &format!("var b = a + {v}"), 1));
    }
    let _ = definition(&mut c, &uri, pos_of(&big, "f1(2)", 0, 0));
    let st = c.request_raw(zell_lsp::ANALYZER_STATUS, serde_json::Value::Null);
    assert_eq!(st["queued"], 0, "{st}");
    assert!(st["stats"]["started"].as_u64().unwrap() < 20, "{st}");

    // A syntax error at each nesting level: no panic, positioned diagnostics.
    let src = "namespace app\n\nfunc main() {\n    if true {\n        for i := 0; i < 3; i++ {\n            var x = 1\n        }\n    }\n}\n";
    let (dir, uri) = project_with("main.zl", src);
    let mut c = Client::start(&dir);
    c.open(&uri, src);
    let lines: Vec<&str> = src.lines().collect();
    for (n, l) in lines.iter().enumerate().skip(2) {
        let broken = src.replacen(l, &format!("{l} )("), 1);
        c.change(&uri, n as i32 + 2, &broken);
        let d = c.diagnostics(&uri);
        assert!(!d.is_empty(), "line {n}");
        let last = broken.lines().count() as u32;
        assert!(d.iter().all(|d| d.range.start.line <= last && d.range.start <= d.range.end), "{d:?}");
        for m in [request::HoverRequest::METHOD, request::Completion::METHOD, request::DocumentHighlightRequest::METHOD] {
            let id = c.send_request(m, serde_json::json!({ "textDocument": { "uri": uri }, "position": { "line": n, "character": 6 } }));
            assert!(c.response(&id).error.is_none(), "{m} at line {n}");
        }
    }
}

#[test]
fn unused_code_is_tagged_unnecessary() {
    let src = "namespace app\n\nfunc main() {\n    var unused = 1\n}\n";
    let (dir, uri) = project_with("main.zl", src);
    let mut c = Client::start(&dir);
    c.open(&uri, src);
    let d = c.diagnostics(&uri);
    assert!(!d.is_empty(), "{d:?}");
    for d in &d {
        assert_eq!(d.tags, Some(vec![DiagnosticTag::UNNECESSARY]), "{d:?}");
    }
}
