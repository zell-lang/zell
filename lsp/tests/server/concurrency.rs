//! Requests on the worker pool: a long request does not block others,
//! `$/cancelRequest` stops a running request (`RequestCancelled`), an edit
//! stops in-flight requests on the edited document (`ContentModified`).
//! Handlers are slowed with the `debug.requestDelayMs` test hook.

use super::*;
use std::time::Instant;

const CONTENT_MODIFIED: i32 = -32801;

const TEXT: &str = "namespace app\n\nfunc F(a int) int {\n    return a\n}\n\nfunc G() int {\n    return F(1) + F(2)\n}\n";

/// A server whose handlers of `method` wait `ms` first, with `TEXT` open
/// and analysed.
fn slow(method: &str, ms: u64) -> (Client, Url) {
    let (dir, uri) = project_with("main.zl", TEXT);
    let opts = serde_json::json!({"debug": {"requestDelayMs": {method: ms}}});
    let mut c = Client::start_with(&dir, |p| p.initialization_options = Some(opts));
    c.open(&uri, TEXT);
    assert_eq!(c.diagnostics(&uri), vec![]);
    (c, uri)
}

fn references(uri: &Url) -> serde_json::Value {
    serde_json::to_value(ReferenceParams {
        text_document_position: tdp(uri, pos_of(TEXT, "F(", 0, 0)),
        context: ReferenceContext { include_declaration: true },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    })
    .unwrap()
}

fn hover(uri: &Url, text: &str) -> serde_json::Value {
    serde_json::to_value(HoverParams { text_document_position_params: tdp(uri, pos_of(text, "F(", 0, 0)), work_done_progress_params: Default::default() }).unwrap()
}

fn cancel(c: &Client, id: &RequestId) {
    let id = match serde_json::to_value(id).unwrap() {
        serde_json::Value::Number(n) => NumberOrString::Number(n.as_i64().unwrap() as i32),
        serde_json::Value::String(s) => NumberOrString::String(s),
        other => panic!("{other}"),
    };
    c.notify::<notification::Cancel>(CancelParams { id });
}

fn status(c: &mut Client) -> serde_json::Value {
    c.request_raw(zell_lsp::ANALYZER_STATUS, serde_json::Value::Null)
}

/// Waits until the server reports a request running on the pool.
fn wait_running(c: &mut Client) {
    let t0 = Instant::now();
    while status(c)["running"].as_u64() == Some(0) {
        assert!(t0.elapsed() < Duration::from_secs(10), "the request never started");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_long_request_does_not_block_a_hover() {
    let (mut c, uri) = slow(request::References::METHOD, 1500);
    let refs = c.send_request(request::References::METHOD, references(&uri));
    let t0 = Instant::now();
    let h = c.request_raw(request::HoverRequest::METHOD, hover(&uri, TEXT));
    assert!(h.to_string().contains("func F(a int) int"), "{h}");
    assert!(t0.elapsed() < Duration::from_millis(1000), "the hover waited for the references: {:?}", t0.elapsed());
    assert!(!c.responses.iter().any(|r| r.id == refs), "references answered before their delay");
    let r = c.response(&refs);
    let locs: Vec<Location> = serde_json::from_value(r.result.expect("references answered")).unwrap();
    assert_eq!(locs.len(), 3, "{locs:?}");
}

#[test]
fn cancelling_a_running_references_request() {
    let (mut c, uri) = slow(request::References::METHOD, 10_000);
    let refs = c.send_request(request::References::METHOD, references(&uri));
    wait_running(&mut c);
    let t0 = Instant::now();
    cancel(&c, &refs);
    let e = c.response(&refs).error.expect("a cancelled request gets an error");
    assert_eq!(e.code, lsp_server::ErrorCode::RequestCanceled as i32, "{}", e.message);
    assert!(t0.elapsed() < Duration::from_secs(2), "the cancellation waited: {:?}", t0.elapsed());
    let st = status(&mut c);
    assert_eq!(st["stats"]["cancelledRequests"], 1, "{st}");
    assert_eq!(st["running"], 0, "{st}");
}

#[test]
fn an_edit_makes_an_in_flight_request_content_modified() {
    let (mut c, uri) = slow(request::HoverRequest::METHOD, 1500);
    let other_text = "namespace app\n\nfunc H() int {\n    return F(3)\n}\n";
    let other = Url::from_file_path(uri.to_file_path().unwrap().with_file_name("other.zl")).unwrap();
    std::fs::write(other.to_file_path().unwrap(), other_text).unwrap();
    c.open(&other, other_text);
    assert_eq!(c.diagnostics(&other), vec![]);
    let first = c.send_request(request::HoverRequest::METHOD, hover(&uri, TEXT));
    let unrelated = c.send_request(request::HoverRequest::METHOD, hover(&other, other_text));
    wait_running(&mut c);
    let t0 = Instant::now();
    let v2 = TEXT.replace("a int) int", "b int) int").replace("return a", "return b");
    c.change(&uri, 2, &v2);
    let e = c.response(&first).error.expect("an invalidated request gets an error");
    assert_eq!(e.code, CONTENT_MODIFIED, "{}", e.message);
    assert!(t0.elapsed() < Duration::from_millis(1000), "{:?}", t0.elapsed());
    // A request about another document is answered from its snapshot.
    let h = c.response(&unrelated).result.expect("the other document's hover is answered").to_string();
    assert!(h.contains("func F(a int) int"), "{h}");
    // Asked again, the request waits for the analysis of the new text.
    let h = c.request_raw(request::HoverRequest::METHOD, hover(&uri, &v2));
    assert!(h.to_string().contains("func F(b int) int"), "{h}");
}

#[test]
fn every_request_is_answered_once_under_concurrent_edits() {
    let (dir, uri) = project_with("main.zl", TEXT);
    let mut c = Client::start_with(&dir, |_| {});
    c.open(&uri, TEXT);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let mut ids = Vec::new();
    for round in 0..20 {
        let text = TEXT.replace("a int) int", &format!("a{round} int) int")).replace("return a\n", &format!("return a{round}\n"));
        ids.push(c.send_request(request::References::METHOD, references(&uri)));
        ids.push(c.send_request(request::HoverRequest::METHOD, hover(&uri, &text)));
        let tokens = SemanticTokensParams { text_document: TextDocumentIdentifier { uri: uri.clone() }, work_done_progress_params: Default::default(), partial_result_params: Default::default() };
        ids.push(c.send_request(request::SemanticTokensFullRequest::METHOD, serde_json::to_value(tokens).unwrap()));
        ids.push(c.send_request(request::WorkspaceSymbolRequest::METHOD, serde_json::json!({"query": "F"})));
        c.change(&uri, round + 2, &text);
    }
    for id in &ids {
        let r = c.response(id);
        if let Some(e) = r.error {
            assert_eq!(e.code, CONTENT_MODIFIED, "{}", e.message);
        }
    }
    // Nothing is answered twice and nothing is left running.
    let st = status(&mut c);
    assert!(!c.responses.iter().any(|r| ids.contains(&r.id)), "a request was answered twice");
    assert_eq!(st["queued"], 0, "{st}");
    let last = TEXT.replace("a int) int", "a19 int) int").replace("return a\n", "return a19\n");
    let h = c.request_raw(request::HoverRequest::METHOD, hover(&uri, &last));
    assert!(h.to_string().contains("func F(a19 int) int"), "{h}");
}
