//! Client-driven workspace protocol: dynamic registration, workspace
//! folders, pulled configuration, refresh requests, on-type formatting,
//! document links, diagnostic details (version, `data`, cross-file related
//! information, partial results), tracing and stale changes.

use super::*;

fn doc(uri: &Url) -> TextDocumentIdentifier {
    TextDocumentIdentifier { uri: uri.clone() }
}

/// Makes the server process everything sent so far.
fn sync(c: &mut Client) {
    c.request_raw(zell_lsp::ANALYZER_STATUS, serde_json::Value::Null);
}

/// Waits for a fresh analysis (requests that need one queue until it lands).
fn analysed(c: &mut Client, uri: &Url) {
    c.request::<request::DocumentSymbolRequest>(DocumentSymbolParams { text_document: doc(uri), work_done_progress_params: Default::default(), partial_result_params: Default::default() });
}

fn sent(c: &Client, method: &str) -> usize {
    c.server_requests.iter().filter(|r| r.method == method).count()
}

fn ws_caps(adjust: impl FnOnce(&mut WorkspaceClientCapabilities)) -> impl FnOnce(&mut InitializeParams) {
    move |p| {
        let mut w = WorkspaceClientCapabilities::default();
        adjust(&mut w);
        p.capabilities.workspace = Some(w);
    }
}

#[test]
fn file_watchers_are_registered_dynamically() {
    let (dir, _) = project(&[]);
    let mut c = Client::start_with(&dir, ws_caps(|w| w.did_change_watched_files = Some(DidChangeWatchedFilesClientCapabilities { dynamic_registration: Some(true), relative_pattern_support: None })));
    sync(&mut c);
    let reg = c.server_requests.iter().find(|r| r.method == request::RegisterCapability::METHOD).expect("registerCapability");
    let p: RegistrationParams = serde_json::from_value(reg.params.clone()).unwrap();
    assert_eq!(p.registrations[0].method, notification::DidChangeWatchedFiles::METHOD);
    let opts: DidChangeWatchedFilesRegistrationOptions = serde_json::from_value(p.registrations[0].register_options.clone().unwrap()).unwrap();
    let globs: Vec<String> = opts.watchers.iter().map(|w| match &w.glob_pattern { GlobPattern::String(s) => s.clone(), other => panic!("{other:?}") }).collect();
    assert_eq!(globs, ["**/*.zl", "**/zell.toml", "**/zell.lock"]);
    // Without the client capability nothing is registered.
    let mut c = Client::start(&dir);
    sync(&mut c);
    assert_eq!(sent(&c, request::RegisterCapability::METHOD), 0);
}

#[test]
fn workspace_folders_are_added_and_removed() {
    let (a, _) = project_with("main.zl", "namespace app\n\nfunc main() {\n}\n");
    let (b, b_uri) = project_with("lib.zl", "namespace other\n\nfunc FarAwayHelper() int {\n    return 1\n}\n");
    let mut c = Client::start(&a);
    let caps = c.capabilities.workspace.as_ref().and_then(|w| w.workspace_folders.as_ref()).unwrap();
    assert_eq!(caps.change_notifications, Some(OneOf::Left(true)));
    let find = |c: &mut Client| -> Vec<String> {
        let r = c.request::<request::WorkspaceSymbolRequest>(WorkspaceSymbolParams { query: "FarAwayHelper".into(), work_done_progress_params: Default::default(), partial_result_params: Default::default() });
        match r {
            Some(WorkspaceSymbolResponse::Flat(v)) => v.into_iter().map(|s| s.location.uri.to_string()).collect(),
            Some(WorkspaceSymbolResponse::Nested(v)) => v.into_iter().map(|s| s.name).collect(),
            None => Vec::new(),
        }
    };
    assert!(find(&mut c).is_empty());
    let folder = WorkspaceFolder { uri: Url::from_file_path(&b).unwrap(), name: "b".into() };
    c.notify::<notification::DidChangeWorkspaceFolders>(DidChangeWorkspaceFoldersParams { event: WorkspaceFoldersChangeEvent { added: vec![folder.clone()], removed: vec![] } });
    assert_eq!(find(&mut c), [b_uri.to_string()]);
    c.notify::<notification::DidChangeWorkspaceFolders>(DidChangeWorkspaceFoldersParams { event: WorkspaceFoldersChangeEvent { added: vec![], removed: vec![folder] } });
    assert!(find(&mut c).is_empty());
}

#[test]
fn configuration_is_pulled_from_the_client() {
    let text = "namespace app\n\nfunc main() {\n    var x = 1\n    println(x)\n}\n";
    let (dir, uri) = project_with("main.zl", text);
    let mut c = Client::start_with(&dir, ws_caps(|w| {
        w.configuration = Some(true);
        w.inlay_hint = Some(InlayHintWorkspaceClientCapabilities { refresh_support: Some(true) });
    }));
    c.open(&uri, text);
    let hints = |c: &mut Client| -> usize {
        let r = c.request::<request::InlayHintRequest>(InlayHintParams { text_document: doc(&uri), range: Range { start: Position::new(0, 0), end: Position::new(100, 0) }, work_done_progress_params: Default::default() });
        r.unwrap_or_default().len()
    };
    assert!(hints(&mut c) > 0);
    let answer = |c: &mut Client, nth: usize, value: serde_json::Value| {
        let reqs: Vec<&Request> = c.server_requests.iter().filter(|r| r.method == request::WorkspaceConfiguration::METHOD).collect();
        assert!(reqs.len() > nth, "workspace/configuration #{nth} not sent");
        let p: ConfigurationParams = serde_json::from_value(reqs[nth].params.clone()).unwrap();
        assert_eq!(p.items[0].section.as_deref(), Some("zell"));
        let id = reqs[nth].id.clone();
        c.conn.sender.send(Message::Response(lsp_server::Response::new_ok(id, serde_json::json!([value])))).unwrap();
    };
    let off = serde_json::json!({"inlayHints": {"types": {"enabled": false}, "parameterNames": {"enabled": false}}});
    answer(&mut c, 0, off.clone());
    assert_eq!(hints(&mut c), 0);
    assert_eq!(sent(&c, request::InlayHintRefreshRequest::METHOD), 1, "settings changed: hints refreshed");
    // didChangeConfiguration (without settings) makes the server pull again.
    c.notify::<notification::DidChangeConfiguration>(DidChangeConfigurationParams { settings: serde_json::Value::Null });
    sync(&mut c);
    answer(&mut c, 1, serde_json::json!({"inlayHints": {"types": {"enabled": true}}}));
    assert!(hints(&mut c) > 0);
    // The same settings again: no refresh.
    c.notify::<notification::DidChangeConfiguration>(DidChangeConfigurationParams { settings: serde_json::Value::Null });
    sync(&mut c);
    answer(&mut c, 2, serde_json::json!({"inlayHints": {"types": {"enabled": true}}}));
    sync(&mut c);
    assert_eq!(sent(&c, request::InlayHintRefreshRequest::METHOD), 2);
}

#[test]
fn refresh_requests_follow_changes_seen_by_other_documents() {
    let lib = "namespace app\n\nfunc Twice(n int) int {\n    return n * 2\n}\n";
    let main = "namespace app\n\nfunc main() {\n    var x = Twice(2)\n    println(x)\n}\n";
    let (dir, lib_uri) = project_with("lib.zl", lib);
    std::fs::write(dir.join("main.zl"), main).unwrap();
    let main_uri = Url::from_file_path(dir.join("main.zl")).unwrap();
    let mut c = Client::start_with(&dir, ws_caps(|w| {
        w.semantic_tokens = Some(SemanticTokensWorkspaceClientCapabilities { refresh_support: Some(true) });
        w.inlay_hint = Some(InlayHintWorkspaceClientCapabilities { refresh_support: Some(true) });
        w.code_lens = Some(CodeLensWorkspaceClientCapabilities { refresh_support: Some(true) });
    }));
    c.open(&lib_uri, lib);
    c.open(&main_uri, main);
    analysed(&mut c, &lib_uri);
    let counts = |c: &Client| {
        [request::SemanticTokensRefresh::METHOD, request::InlayHintRefreshRequest::METHOD, request::CodeLensRefresh::METHOD].map(|m| sent(c, m))
    };
    let base = counts(&c);
    // A body edit: nothing other documents show changes.
    c.change(&lib_uri, 2, &lib.replace("n * 2", "n + n"));
    analysed(&mut c, &lib_uri);
    assert_eq!(counts(&c), base);
    // A signature change: one refresh of each kind.
    c.change(&lib_uri, 3, &lib.replace("n int) int", "n int, m int) int"));
    analysed(&mut c, &lib_uri);
    assert_eq!(counts(&c), base.map(|n| n + 1));
    // A new reference in main.zl changes lib.zl's lens only.
    c.change(&main_uri, 2, &main.replace("println(x)", "println(x, Twice(1, 2))"));
    analysed(&mut c, &main_uri);
    assert_eq!(counts(&c), [base[0] + 1, base[1] + 1, base[2] + 2]);
}

#[test]
fn on_type_formatting_formats_the_enclosing_block_only() {
    let text = "namespace app\n\nfunc a() {\n  var x   = 1\n    println(x)\n}\n\nfunc b() {\n  var y   = 2\n    println(y)\n}\n";
    let (dir, uri) = project_with("main.zl", text);
    let mut c = Client::start(&dir);
    c.open(&uri, text);
    let on_type = |c: &mut Client, pos: Position, ch: &str| {
        c.request::<request::OnTypeFormatting>(DocumentOnTypeFormattingParams {
            text_document_position: TextDocumentPositionParams { text_document: doc(&uri), position: pos },
            ch: ch.into(),
            options: FormattingOptions { tab_size: 4, insert_spaces: true, ..Default::default() },
        })
    };
    // `}` typed at the end of b's body: only b is formatted.
    let close_b = Position::new(10, 1);
    let edits = on_type(&mut c, close_b, "}").expect("edits");
    let out = apply_edits(text, &edits);
    assert!(out.contains("func a() {\n  var x   = 1\n"), "{out}");
    assert!(out.contains("func b() {\n    var y = 2\n    println(y)\n}\n"), "{out}");
    // A newline inside a: a is formatted, except the cursor's line.
    let edits = on_type(&mut c, Position::new(4, 4), "\n").expect("edits");
    let out = apply_edits(text, &edits);
    assert!(out.contains("func a() {\n    var x = 1\n    println(x)\n}"), "{out}");
    assert!(out.contains("  var y   = 2"), "{out}");
    // At the top level, or with a syntax error: nothing.
    assert!(on_type(&mut c, Position::new(1, 0), "\n").is_none_or(|e| e.is_empty()));
    let broken = text.replace("println(y)", "println(y");
    c.change(&uri, 2, &broken);
    assert!(on_type(&mut c, close_b, "}").is_none_or(|e| e.is_empty()));
}

#[test]
fn imports_link_to_their_namespace() {
    let main = "namespace app\n\nimport app.util\nimport strings\n\nfunc main() {\n    println(util.One(), strings.ToUpper(\"a\"))\n}\n";
    let (dir, uri) = project_with("main.zl", main);
    std::fs::create_dir_all(dir.join("util")).unwrap();
    std::fs::write(dir.join("util/one.zl"), "namespace app.util\n\nfunc One() int {\n    return 1\n}\n").unwrap();
    let mut c = Client::start(&dir);
    c.open(&uri, main);
    let links = c.request::<request::DocumentLinkRequest>(DocumentLinkParams { text_document: doc(&uri), work_done_progress_params: Default::default(), partial_result_params: Default::default() }).unwrap();
    assert_eq!(links.len(), 2, "{links:?}");
    assert_eq!(range_text(main, &links[0].range), "app.util");
    assert!(links[0].target.as_ref().unwrap().path().ends_with("/util/one.zl"), "{links:?}");
    assert_eq!(range_text(main, &links[1].range), "strings");
    assert!(links[1].target.as_ref().unwrap().path().contains("strings"), "{links:?}");
}

#[test]
fn diagnostics_carry_version_fix_data_and_cross_file_notes() {
    let main = "namespace app\n\nfunc main() {\n    var unused = 1\n}\n\nfunc Dup() {\n}\n";
    let (dir, uri) = project_with("main.zl", main);
    std::fs::write(dir.join("other.zl"), "namespace app\n\nfunc Dup() {\n}\n").unwrap();
    let other_uri = Url::from_file_path(dir.join("other.zl")).unwrap();
    let mut c = Client::start(&dir);
    c.open(&uri, main);
    c.change(&uri, 7, main);
    let _ = c.diagnostics(&uri);
    let published: Vec<PublishDiagnosticsParams> = c.notes.iter().filter(|n| n.method == notification::PublishDiagnostics::METHOD).map(|n| serde_json::from_value(n.params.clone()).unwrap()).collect();
    let last = published.iter().rev().find(|p| p.uri == uri).unwrap();
    assert_eq!(last.version, Some(7));
    // A duplicate declared in another file points there.
    let (in_file, dup) = published.iter().flat_map(|p| p.diagnostics.iter().map(move |d| (&p.uri, d))).find(|(_, d)| d.message.contains("duplicate declaration of Dup")).expect("duplicate reported");
    let rel = dup.related_information.as_ref().expect("related information");
    let elsewhere = if *in_file == uri { &other_uri } else { &uri };
    assert!(rel.iter().any(|r| &r.location.uri == elsewhere && r.location.range.start.line == if *in_file == uri { 2 } else { 6 }), "{rel:?}");
    // The unused variable's fix travels in `data`; codeAction uses it.
    let unused = last.diagnostics.iter().find(|d| d.message.contains("unused")).expect("unused variable").clone();
    let data = unused.data.clone().expect("fix data");
    assert_eq!(data["v"], 7);
    let actions = |c: &mut Client, diags: Vec<Diagnostic>| -> Vec<String> {
        let r = c.request::<request::CodeActionRequest>(CodeActionParams {
            text_document: doc(&uri),
            range: unused.range,
            context: CodeActionContext { diagnostics: diags, only: Some(vec![CodeActionKind::QUICKFIX]), trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        });
        r.unwrap_or_default().into_iter().map(|a| serde_json::to_string(&a).unwrap()).collect()
    };
    let from_data = actions(&mut c, vec![unused.clone()]);
    let recomputed = actions(&mut c, vec![]);
    assert!(!from_data.is_empty());
    assert_eq!(from_data.len(), recomputed.len());
    // Stale data (another version) is ignored: fixes are recomputed.
    let mut stale = unused.clone();
    stale.data = Some(serde_json::json!({"v": 3, "fixes": [{"title": "bogus", "edits": []}]}));
    let r = actions(&mut c, vec![stale]);
    assert!(!r.iter().any(|a| a.contains("bogus")), "{r:?}");
}

#[test]
fn workspace_diagnostics_stream_partial_results() {
    let main = "namespace app\n\nfunc main() {\n    println(Nope())\n}\n";
    let (dir, uri) = project_with("main.zl", main);
    std::fs::write(dir.join("b.zl"), "namespace app\n\nfunc B() {\n}\n").unwrap();
    let mut c = Client::start_with(&dir, |p| {
        p.capabilities.text_document = Some(TextDocumentClientCapabilities { diagnostic: Some(DiagnosticClientCapabilities::default()), ..Default::default() });
    });
    c.open(&uri, main);
    let token = NumberOrString::String("partial-1".into());
    let params = WorkspaceDiagnosticParams { identifier: None, previous_result_ids: vec![], work_done_progress_params: Default::default(), partial_result_params: PartialResultParams { partial_result_token: Some(token.clone()) } };
    let r: WorkspaceDiagnosticReportResult = serde_json::from_value(c.request_raw(request::WorkspaceDiagnosticRequest::METHOD, serde_json::to_value(params).unwrap())).unwrap();
    let WorkspaceDiagnosticReportResult::Report(r) = r else { panic!() };
    assert!(r.items.is_empty(), "everything was streamed");
    let parts: Vec<&Notification> = c.notes.iter().filter(|n| n.method == "$/progress" && n.params["token"] == "partial-1").collect();
    assert_eq!(parts.len(), 2);
    assert!(parts.iter().any(|n| n.params["value"]["items"][0]["uri"] == uri.as_str() && n.params["value"]["items"][0]["version"] == 1 && n.params["value"]["items"][0]["items"].as_array().is_some_and(|d| !d.is_empty())), "{parts:?}");
}

#[test]
fn trace_and_stale_changes() {
    let text = "namespace app\n\nfunc main() {\n}\n";
    let (dir, uri) = project_with("main.zl", text);
    let mut c = Client::start(&dir);
    c.open(&uri, text);
    analysed(&mut c, &uri);
    assert!(!c.notes.iter().any(|n| n.method == notification::LogTrace::METHOD));
    c.notify::<notification::SetTrace>(SetTraceParams { value: TraceValue::Messages });
    c.change(&uri, 5, &format!("{text}// v5\n"));
    // An older version arriving late is dropped.
    c.change(&uri, 4, &format!("{text}// v4\n"));
    analysed(&mut c, &uri);
    let t = c.request_raw(zell_lsp::DOCUMENT_TEXT, serde_json::json!({"uri": uri}));
    assert!(t.as_str().unwrap().ends_with("// v5\n"), "{t}");
    let traces: Vec<String> = c.notes.iter().filter(|n| n.method == notification::LogTrace::METHOD).map(|n| n.params["message"].as_str().unwrap().to_string()).collect();
    assert!(traces.iter().any(|m| m.starts_with("dropped stale didChange")), "{traces:?}");
    assert!(traces.iter().any(|m| m.starts_with("analysis ")), "{traces:?}");
    assert!(traces.iter().any(|m| m.starts_with("textDocument/documentSymbol answered")), "{traces:?}");
}
