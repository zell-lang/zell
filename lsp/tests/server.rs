//! Drives the language server in-process over an in-memory JSON-RPC
//! connection, like an editor would.

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::notification::{self, Notification as _};
use lsp_types::request::{self, Request as _};
use lsp_types::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

struct Client {
    conn: Connection,
    server: Option<JoinHandle<()>>,
    next_id: i32,
    /// Notifications received while waiting for responses.
    notes: Vec<Notification>,
    /// Requests the server sent to the client (progress, refresh).
    server_requests: Vec<Request>,
    /// Responses received while waiting for another one.
    responses: Vec<lsp_server::Response>,
    capabilities: ServerCapabilities,
}

impl Client {
    fn start(root: &Path) -> Client {
        Client::start_with(root, |_| {})
    }

    /// Starts a server on `root` with initialize params adjusted by `adjust`.
    fn start_with(root: &Path, adjust: impl FnOnce(&mut InitializeParams)) -> Client {
        let (server_conn, client_conn) = Connection::memory();
        let server = std::thread::spawn(move || zell_lsp::run(server_conn).expect("server failed"));
        let mut c = Client { conn: client_conn, server: Some(server), next_id: 0, notes: Vec::new(), server_requests: Vec::new(), responses: Vec::new(), capabilities: Default::default() };
        let root_uri = Url::from_file_path(root).unwrap();
        #[allow(deprecated)]
        let mut params = InitializeParams {
            root_uri: Some(root_uri.clone()),
            workspace_folders: Some(vec![WorkspaceFolder { uri: root_uri, name: "test".into() }]),
            ..Default::default()
        };
        adjust(&mut params);
        let init: InitializeResult = serde_json::from_value(c.request_raw(request::Initialize::METHOD, serde_json::to_value(params).unwrap())).unwrap();
        assert!(init.capabilities.semantic_tokens_provider.is_some());
        c.capabilities = init.capabilities;
        c.notify::<notification::Initialized>(InitializedParams {});
        c
    }

    /// Sends a request without waiting; returns its id.
    fn send_request(&mut self, method: &str, params: serde_json::Value) -> RequestId {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.conn.sender.send(Message::Request(Request { id: id.clone(), method: method.into(), params })).unwrap();
        id
    }

    /// Waits for the response to `id`, collecting everything else.
    fn response(&mut self, id: &RequestId) -> lsp_server::Response {
        if let Some(i) = self.responses.iter().position(|r| &r.id == id) {
            return self.responses.remove(i);
        }
        loop {
            match self.conn.receiver.recv_timeout(Duration::from_secs(30)).expect("no response from server") {
                Message::Response(r) if &r.id == id => return r,
                Message::Response(r) => self.responses.push(r),
                Message::Notification(n) => self.notes.push(n),
                Message::Request(r) => self.server_requests.push(r),
            }
        }
    }

    fn request_raw(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.send_request(method, params);
        let r = self.response(&id);
        if let Some(e) = r.error {
            panic!("{method} failed: {} {}", e.code, e.message);
        }
        r.result.unwrap_or(serde_json::Value::Null)
    }

    fn request<R: request::Request>(&mut self, params: R::Params) -> R::Result {
        let v = self.request_raw(R::METHOD, serde_json::to_value(params).unwrap());
        serde_json::from_value(v).unwrap()
    }

    /// Sends a request expecting an error response; returns its message.
    fn request_err<R: request::Request>(&mut self, params: R::Params) -> String {
        let id = self.send_request(R::METHOD, serde_json::to_value(params).unwrap());
        self.response(&id).error.expect("expected an error").message
    }

    fn notify<N: notification::Notification>(&self, params: N::Params) {
        self.conn.sender.send(Message::Notification(Notification::new(N::METHOD.into(), params))).unwrap();
    }

    fn open(&self, uri: &Url, text: &str) {
        self.notify::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem { uri: uri.clone(), language_id: "zell".into(), version: 1, text: text.into() },
        });
    }

    fn change(&self, uri: &Url, version: i32, text: &str) {
        self.notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version },
            content_changes: vec![TextDocumentContentChangeEvent { range: None, range_length: None, text: text.into() }],
        });
    }

    /// Diagnostics most recently published for `uri`, after forcing the
    /// server to process everything sent so far.
    fn diagnostics(&mut self, uri: &Url) -> Vec<Diagnostic> {
        self.request::<request::DocumentSymbolRequest>(DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        });
        self.notes
            .iter()
            .rev()
            .filter(|n| n.method == notification::PublishDiagnostics::METHOD)
            .map(|n| serde_json::from_value::<PublishDiagnosticsParams>(n.params.clone()).unwrap())
            .find(|p| &p.uri == uri)
            .map(|p| p.diagnostics)
            .expect("no diagnostics published for the file")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.conn.sender.send(Message::Request(Request { id: RequestId::from(9999), method: request::Shutdown::METHOD.into(), params: serde_json::Value::Null }));
        let _ = self.conn.sender.send(Message::Notification(Notification::new(notification::Exit::METHOD.into(), ())));
        if let Some(h) = self.server.take() {
            let _ = h.join();
        }
    }
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// A fresh temporary project with copies of the given example files.
fn project(files: &[&str]) -> (PathBuf, Vec<(Url, String)>) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("zell-lsp-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(repo().join("examples").join(f)).unwrap();
        let name = Path::new(f).file_name().unwrap();
        let p = dir.join(name);
        std::fs::write(&p, &text).unwrap();
        out.push((Url::from_file_path(&p).unwrap(), text));
    }
    (dir, out)
}

fn pos_of(text: &str, needle: &str, nth: usize, delta: usize) -> Position {
    let off = text.match_indices(needle).nth(nth).unwrap_or_else(|| panic!("{needle} not found")).0 + delta;
    let line = text[..off].matches('\n').count() as u32;
    let col = (off - text[..off].rfind('\n').map_or(0, |i| i + 1)) as u32;
    Position { line, character: col }
}

fn tdp(uri: &Url, pos: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams { text_document: TextDocumentIdentifier { uri: uri.clone() }, position: pos }
}

fn line_text(text: &str, line: u32) -> &str {
    text.lines().nth(line as usize).unwrap()
}

fn range_text<'t>(text: &'t str, r: &Range) -> &'t str {
    assert_eq!(r.start.line, r.end.line);
    let l = line_text(text, r.start.line);
    &l[r.start.character as usize..r.end.character as usize]
}

const MULTI: &[&str] = &["multi/models.zl", "multi/namespaces.zl", "multi/report.zl"];

#[test]
fn examples_have_no_diagnostics() {
    let (dir, files) = project(&["bank.zl"]);
    let mut c = Client::start(&dir);
    c.open(&files[0].0, &files[0].1);
    assert_eq!(c.diagnostics(&files[0].0), vec![]);
    drop(c);

    let (dir, files) = project(MULTI);
    let mut c = Client::start(&dir);
    for (uri, text) in &files {
        c.open(uri, text);
    }
    for (uri, _) in &files {
        assert_eq!(c.diagnostics(uri), vec![], "{uri}");
    }
}

#[test]
fn workspace_files_are_discovered_without_opening() {
    let (dir, files) = project(MULTI);
    let mut c = Client::start(&dir);
    // Only open one file: names from the other two must still resolve.
    let (uri, text) = &files[1];
    c.open(uri, text);
    assert_eq!(c.diagnostics(uri), vec![]);
    let def: Option<GotoDefinitionResponse> = c.request::<request::GotoDefinition>(GotoDefinitionParams {
        text_document_position_params: tdp(uri, pos_of(text, "Rename", 0, 1)),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let Some(GotoDefinitionResponse::Scalar(loc)) = def else { panic!("no definition") };
    assert_eq!(loc.uri, files[0].0);
}

#[test]
fn error_is_reported_with_range_and_cleared() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    assert!(c.diagnostics(uri).is_empty());
    // Semantic error: unknown identifier.
    let broken = text.replacen("var frac = cents % 100", "var frac = centz % 100", 1);
    c.change(uri, 2, &broken);
    let d = c.diagnostics(uri);
    assert_eq!(d.len(), 1, "{d:?}");
    assert!(d[0].message.contains("unknown identifier centz"), "{}", d[0].message);
    assert_eq!(d[0].range.start, pos_of(&broken, "centz", 0, 0));
    assert_eq!(d[0].range.end, pos_of(&broken, "centz", 0, 5));
    assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
    // Syntax error.
    let broken = text.replacen("func Money(cents int) string {", "func Money(cents int string {", 1);
    c.change(uri, 3, &broken);
    let d = c.diagnostics(uri);
    assert!(!d.is_empty());
    assert_eq!(d[0].range.start.line, pos_of(&broken, "func Money", 0, 0).line);
    // Fixed again.
    c.change(uri, 4, text);
    assert!(c.diagnostics(uri).is_empty());
}

#[test]
fn definition_of_method_call() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let def: Option<GotoDefinitionResponse> = c.request::<request::GotoDefinition>(GotoDefinitionParams {
        text_document_position_params: tdp(uri, pos_of(text, "].Deposit(amount)", 0, 3)),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let Some(GotoDefinitionResponse::Scalar(loc)) = def else { panic!("no definition") };
    assert_eq!(&loc.uri, uri);
    assert_eq!(loc.range.start, pos_of(text, "func Deposit(", 0, 5));
    assert_eq!(range_text(text, &loc.range), "Deposit");
}

#[test]
fn references_and_rename_across_files() {
    let (dir, files) = project(MULTI);
    let mut c = Client::start(&dir);
    for (uri, text) in &files {
        c.open(uri, text);
    }
    let (models_uri, models) = &files[0];
    let at = tdp(models_uri, pos_of(models, "struct User", 0, 7));
    let refs: Option<Vec<Location>> = c.request::<request::References>(ReferenceParams {
        text_document_position: at.clone(),
        context: ReferenceContext { include_declaration: false },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let refs = refs.unwrap();
    // models.zl: User{...} in New and the return type; namespaces.zl:
    // User.New x2, app.models.User.New, []User; report.zl: app.models.User.
    let count = |u: &Url| refs.iter().filter(|l| &l.uri == u).count();
    assert_eq!(count(&files[0].0), 2, "{refs:?}");
    assert_eq!(count(&files[1].0), 4, "{refs:?}");
    assert_eq!(count(&files[2].0), 1, "{refs:?}");
    for l in &refs {
        let text = &files.iter().find(|f| f.0 == l.uri).unwrap().1;
        assert_eq!(range_text(text, &l.range), "User");
    }

    let prep: Option<PrepareRenameResponse> = c.request::<request::PrepareRenameRequest>(at.clone());
    assert!(matches!(prep, Some(PrepareRenameResponse::RangeWithPlaceholder { ref placeholder, .. }) if placeholder == "User"));
    let edit: Option<WorkspaceEdit> = c.request::<request::Rename>(RenameParams { text_document_position: at, new_name: "Member".into(), work_done_progress_params: Default::default() });
    let changes = edit.unwrap().changes.unwrap();
    assert_eq!(changes.len(), 3, "{changes:?}");
    assert_eq!(changes[&files[0].0].len(), 3); // declaration + 2 uses
    assert_eq!(changes[&files[1].0].len(), 4);
    // Apply the edits and check that the project still type-checks.
    for (uri, text) in &files {
        let mut edits = changes[uri].clone();
        edits.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        for e in edits {
            let l = &mut lines[e.range.start.line as usize];
            l.replace_range(e.range.start.character as usize..e.range.end.character as usize, &e.new_text);
        }
        c.change(uri, 2, &(lines.join("\n") + "\n"));
    }
    for (uri, _) in &files {
        assert_eq!(c.diagnostics(uri), vec![], "{uri}");
    }
}

#[test]
fn rename_refuses_std_symbols() {
    let (dir, files) = project(MULTI);
    let (uri, text) = &files[2];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let msg = c.request_err::<request::PrepareRenameRequest>(tdp(uri, pos_of(text, "println", 0, 2)));
    assert!(msg.contains("standard library"), "{msg}");
}

#[test]
fn hover_shows_signature_and_doc() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let hover = |c: &mut Client, pos| -> String {
        let h: Option<Hover> = c.request::<request::HoverRequest>(HoverParams { text_document_position_params: tdp(uri, pos), work_done_progress_params: Default::default() });
        match h.expect("no hover").contents {
            HoverContents::Markup(m) => m.value,
            other => panic!("{other:?}"),
        }
    };
    // A call of a documented function.
    let h = hover(&mut c, pos_of(text, "Money(this.Balance)", 0, 1));
    assert!(h.contains("func Money(cents int) string"), "{h}");
    assert!(h.contains("Amounts are integer cents."), "{h}");
    // A method.
    let h = hover(&mut c, pos_of(text, "].Deposit(amount)", 0, 3));
    assert!(h.contains("func Deposit(amount int) void!"), "{h}");
    assert!(h.contains("*method* of `examples.bank.Account`"), "{h}");
    // A stdlib function.
    let h = hover(&mut c, pos_of(text, "fmt.Format", 0, 5));
    assert!(h.contains("func Format(template string, args ...any) string"), "{h}");
}

#[test]
fn document_symbols_are_hierarchical() {
    let (dir, files) = project(MULTI);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let resp: Option<DocumentSymbolResponse> = c.request::<request::DocumentSymbolRequest>(DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let Some(DocumentSymbolResponse::Nested(syms)) = resp else { panic!("expected nested symbols") };
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].name, "app.models");
    assert_eq!(syms[0].kind, SymbolKind::NAMESPACE);
    let kids = syms[0].children.as_ref().unwrap();
    let names: Vec<&str> = kids.iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names, ["User", "MaxUsers"]);
    assert_eq!(kids[0].kind, SymbolKind::STRUCT);
    let members: Vec<(&str, SymbolKind)> = kids[0].children.as_ref().unwrap().iter().map(|k| (k.name.as_str(), k.kind)).collect();
    assert_eq!(
        members,
        [
            ("Name", SymbolKind::FIELD),
            ("Email", SymbolKind::FIELD),
            ("id", SymbolKind::FIELD),
            ("ID", SymbolKind::METHOD),
            ("Display", SymbolKind::METHOD),
            ("Rename", SymbolKind::METHOD),
            ("New", SymbolKind::METHOD)
        ]
    );
    assert_eq!(range_text(text, &kids[0].selection_range), "User");
}

#[test]
fn semantic_tokens_are_classified_and_stable() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let get = |c: &mut Client| -> Vec<SemanticToken> {
        let r: Option<SemanticTokensResult> = c.request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        });
        match r.unwrap() {
            SemanticTokensResult::Tokens(t) => t.data,
            other => panic!("{other:?}"),
        }
    };
    let a = get(&mut c);
    assert!(a.len() > 100, "{}", a.len());
    assert_eq!(a, get(&mut c));
    // Decode and look at a few tokens.
    let types = zell_lsp::SEMANTIC_TOKEN_TYPES;
    let (mut line, mut col) = (0u32, 0u32);
    let mut decoded = Vec::new();
    for t in &a {
        if t.delta_line > 0 {
            line += t.delta_line;
            col = t.delta_start;
        } else {
            col += t.delta_start;
        }
        let l = line_text(text, line);
        decoded.push((l[col as usize..(col + t.length) as usize].to_string(), types[t.token_type as usize], t.token_modifiers_bitset));
    }
    let find = |s: &str| decoded.iter().find(|d| d.0 == s).unwrap_or_else(|| panic!("{s} not highlighted"));
    assert_eq!(find("struct").1, "keyword");
    // The first `Account` is a field of InsufficientFunds; the struct is declared later.
    assert_eq!(find("Account").1, "property");
    assert!(decoded.iter().any(|d| d.0 == "Account" && d.1 == "struct" && d.2 & 1 == 1), "struct declaration");
    assert!(decoded.iter().any(|d| d.0 == "Account" && d.1 == "struct" && d.2 & 1 == 0), "struct use");
    assert_eq!(find("Money").1, "function");
    assert_eq!(find("Deposit").1, "enumMember");
    assert_eq!(find("// A transaction request.").1, "comment");
    assert_eq!(find("ErrUnknownAccount").1, "variable");
    assert_eq!(find("ErrUnknownAccount").2 & 8, 8, "readonly modifier on const");
    assert!(decoded.iter().any(|d| d.0 == "Balance" && d.1 == "method"));
    assert!(decoded.iter().any(|d| d.0 == "amount" && d.1 == "parameter"));
    assert!(decoded.iter().any(|d| d.0 == "balance" && d.1 == "property"));
}

#[test]
fn folding_ranges_cover_blocks_and_comments() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let r: Option<Vec<FoldingRange>> = c.request::<request::FoldingRangeRequest>(FoldingRangeParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let r = r.unwrap();
    let account = pos_of(text, "struct Account {", 0, 0).line;
    assert!(r.iter().any(|f| f.start_line == account && f.end_line > account + 5), "{r:?}");
    // Two or more import lines fold as one imports range.
    let src = "namespace app\n\nimport io\nimport strings\n\nfunc main() {\n    println(ToUpper(\"x\"))\n}\n";
    let (dir, uri) = project_with("imports.zl", src);
    let mut c = Client::start(&dir);
    c.open(&uri, src);
    let r: Option<Vec<FoldingRange>> = c.request::<request::FoldingRangeRequest>(FoldingRangeParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let r = r.unwrap();
    assert!(r.iter().any(|f| f.kind == Some(FoldingRangeKind::Imports) && f.start_line == 2 && f.end_line == 3), "{r:?}");
}

#[test]
fn completion_members_and_scope() {
    let (dir, files) = project(&["structs.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    assert!(c.diagnostics(uri).is_empty());
    // The user types `c.` on a new line: the file no longer parses.
    let anchor = "    var c = Counter{}\n";
    let edited = text.replacen(anchor, &format!("{anchor}    c.\n"), 1);
    c.change(uri, 2, &edited);
    assert!(!c.diagnostics(uri).is_empty());
    let complete = |c: &mut Client, pos| -> Vec<CompletionItem> {
        let r: Option<CompletionResponse> = c.request::<request::Completion>(CompletionParams {
            text_document_position: tdp(uri, pos),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        });
        match r {
            Some(CompletionResponse::Array(items)) => items,
            Some(CompletionResponse::List(l)) => l.items,
            None => Vec::new(),
        }
    };
    let items = complete(&mut c, pos_of(&edited, "    c.\n", 0, 6));
    let mut labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    labels.sort();
    assert_eq!(labels, ["Inc", "Value", "count"], "{labels:?}");
    assert_eq!(items.iter().find(|i| i.label == "Inc").unwrap().kind, Some(CompletionItemKind::METHOD));

    // Static members after a type name, namespace members after a namespace.
    let edited2 = text.replacen(anchor, &format!("{anchor}    Point.\n"), 1);
    c.change(uri, 3, &edited2);
    let items = complete(&mut c, pos_of(&edited2, "    Point.\n", 0, 10));
    assert!(items.iter().any(|i| i.label == "Origin"), "{items:?}");
    let edited3 = text.replacen(anchor, &format!("{anchor}    io.\n"), 1);
    c.change(uri, 4, &edited3);
    let items = complete(&mut c, pos_of(&edited3, "    io.\n", 0, 7));
    assert!(items.iter().any(|i| i.label == "println"), "{items:?}");

    // Names in scope: locals declared before the cursor, declarations, keywords.
    c.change(uri, 5, text);
    let items = complete(&mut c, pos_of(text, "    c.Inc()", 0, 4));
    let has = |l: &str| items.iter().any(|i| i.label == l);
    assert!(has("c") && has("Counter") && has("var") && has("len") && has("println"));
    assert!(!has("heap"), "locals declared later are not offered");
    // Unknown left-hand side: nothing rather than noise.
    let edited4 = text.replacen(anchor, &format!("{anchor}    nosuch.\n"), 1);
    c.change(uri, 6, &edited4);
    assert!(complete(&mut c, pos_of(&edited4, "    nosuch.\n", 0, 11)).is_empty());
}

#[test]
fn every_example_survives_all_queries() {
    // The whole examples tree as one workspace, including files with
    // deliberate compile errors: no request may fail or crash the server.
    let root = repo().join("examples");
    let mut c = Client::start(&root);
    let mut paths = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "zl") {
                paths.push(p);
            }
        }
    }
    assert!(paths.len() > 10);
    for p in paths {
        let uri = Url::from_file_path(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        c.open(&uri, &text);
        let doc = TextDocumentIdentifier { uri: uri.clone() };
        let _: Option<SemanticTokensResult> = c.request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            text_document: doc.clone(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        });
        let _: Option<Vec<FoldingRange>> = c.request::<request::FoldingRangeRequest>(FoldingRangeParams {
            text_document: doc.clone(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        });
        let lenses: Option<Vec<CodeLens>> = c.request::<request::CodeLensRequest>(CodeLensParams { text_document: doc.clone(), work_done_progress_params: Default::default(), partial_result_params: Default::default() });
        for l in lenses.unwrap_or_default().into_iter().filter(|l| l.command.is_none()) {
            assert!(c.request::<request::CodeLensResolve>(l).command.is_some());
        }
        let everything = Range { start: Position { line: 0, character: 0 }, end: Position { line: 100_000, character: 0 } };
        let hints: Option<Vec<InlayHint>> = c.request::<request::InlayHintRequest>(InlayHintParams { text_document: doc.clone(), range: everything, work_done_progress_params: Default::default() });
        for h in hints.unwrap_or_default().into_iter().take(20) {
            let _: InlayHint = c.request::<request::InlayHintResolveRequest>(h);
        }
        let _: Option<SemanticTokensRangeResult> = c.request::<request::SemanticTokensRangeRequest>(SemanticTokensRangeParams {
            text_document: doc.clone(),
            range: everything,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        });
        let _: DocumentDiagnosticReportResult = c.request::<request::DocumentDiagnosticRequest>(DocumentDiagnosticParams {
            text_document: doc.clone(),
            identifier: None,
            previous_result_id: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        });
        let positions: Vec<Position> = (0..text.lines().count() as u32).step_by(3).map(|line| Position { line, character: 4 }).collect();
        let _: Option<Vec<SelectionRange>> = c.request::<request::SelectionRangeRequest>(SelectionRangeParams {
            text_document: doc.clone(),
            positions,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        });
        for (line, l) in text.lines().enumerate() {
            let mut col = 0;
            for word in l.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.') {
                let at = Position { line: line as u32, character: (col + word.len()) as u32 };
                col += word.len() + 1;
                if word.is_empty() || !l.is_char_boundary(at.character as usize) {
                    continue;
                }
                let _: Option<Hover> = c.request::<request::HoverRequest>(HoverParams { text_document_position_params: tdp(&uri, at), work_done_progress_params: Default::default() });
                let _: Option<GotoDefinitionResponse> = c.request::<request::GotoDefinition>(GotoDefinitionParams {
                    text_document_position_params: tdp(&uri, at),
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                });
                let _: Option<CompletionResponse> = c.request::<request::Completion>(CompletionParams {
                    text_document_position: tdp(&uri, at),
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                    context: None,
                });
                for method in [request::GotoImplementation::METHOD, request::GotoTypeDefinition::METHOD] {
                    let p = GotoDefinitionParams { text_document_position_params: tdp(&uri, at), work_done_progress_params: Default::default(), partial_result_params: Default::default() };
                    let _ = c.request_raw(method, serde_json::to_value(p).unwrap());
                }
            }
        }
    }
}

#[path = "server/conformance.rs"]
mod conformance;

#[path = "server/inspect.rs"]
mod inspect;

#[path = "server/protocol.rs"]
mod protocol;

#[path = "server/intrinsics.rs"]
mod intrinsics;

#[path = "server/members.rs"]
mod members;

#[path = "server/completion.rs"]
mod completion;

#[path = "server/workspace.rs"]
mod workspace;

// ---------------------------------------------------------------------------
// Stage 3-5 features
// ---------------------------------------------------------------------------

/// Byte offset of an LSP position (ASCII-only test texts).
fn offset_of(text: &str, p: Position) -> usize {
    let mut off = 0;
    for (i, l) in text.split_inclusive('\n').enumerate() {
        if i as u32 == p.line {
            return off + (p.character as usize).min(l.trim_end_matches('\n').len());
        }
        off += l.len();
    }
    text.len()
}

fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut spans: Vec<(usize, usize, &str)> = edits.iter().map(|e| (offset_of(text, e.range.start), offset_of(text, e.range.end), e.new_text.as_str())).collect();
    spans.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = text.to_string();
    for (s, e, t) in spans {
        out.replace_range(s..e, t);
    }
    out
}

/// A project with one file `main.zl` holding `text`.
fn project_with(name: &str, text: &str) -> (PathBuf, Url) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("zell-lsp-test2-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    (dir.clone(), Url::from_file_path(&p).unwrap())
}

fn code_actions(c: &mut Client, uri: &Url, range: Range) -> Vec<CodeAction> {
    let r: Option<CodeActionResponse> = c.request::<request::CodeActionRequest>(CodeActionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        range,
        context: CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    r.unwrap_or_default()
        .into_iter()
        .filter_map(|a| match a {
            CodeActionOrCommand::CodeAction(a) => Some(a),
            _ => None,
        })
        .collect()
}

/// Applies the code action titled `title` (prefix) and returns the new text.
fn apply_action(c: &mut Client, uri: &Url, text: &str, at: Range, title: &str) -> String {
    let actions = code_actions(c, uri, at);
    let a = actions.iter().find(|a| a.title.starts_with(title)).unwrap_or_else(|| panic!("no action {title:?} in {:?}", actions.iter().map(|a| &a.title).collect::<Vec<_>>()));
    assert_eq!(a.kind, Some(CodeActionKind::QUICKFIX));
    let edits = &a.edit.as_ref().unwrap().changes.as_ref().unwrap()[uri];
    apply_edits(text, edits)
}

#[test]
fn signature_help_shows_parameters_and_doc() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let help = |c: &mut Client, uri: &Url, pos| -> Option<SignatureHelp> {
        c.request::<request::SignatureHelpRequest>(SignatureHelpParams { context: None, text_document_position_params: tdp(uri, pos), work_done_progress_params: Default::default() })
    };
    let h = help(&mut c, uri, pos_of(text, "Money(this.Balance)", 0, 6)).expect("signature help");
    let sig = &h.signatures[0];
    assert_eq!(sig.label, "func Money(cents int) string");
    assert_eq!(h.active_parameter, Some(0));
    match &sig.parameters.as_ref().unwrap()[0].label {
        ParameterLabel::LabelOffsets([a, b]) => assert_eq!(&sig.label[*a as usize..*b as usize], "cents int"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(&sig.documentation, Some(Documentation::MarkupContent(m)) if m.value.contains("integer cents")));
    // After the comma of a two-parameter static method: the second one.
    let h = help(&mut c, uri, pos_of(text, "Account.Open(owner, initial)", 0, 20)).unwrap();
    assert!(h.signatures[0].label.contains("Open(owner string, initial int)"), "{:?}", h.signatures[0].label);
    assert_eq!(h.active_parameter, Some(1));
    // While typing an unfinished call (the file has a syntax error).
    let edited = text.replacen("    bank.Report()\n", "    bank.Report()\n    bank.Open(\"x\", \n", 1);
    c.change(uri, 2, &edited);
    let h = help(&mut c, uri, pos_of(&edited, "bank.Open(\"x\", \n", 0, 15)).expect("help in a broken file");
    assert!(h.signatures[0].label.contains("func Open(owner string, initial int)"), "{:?}", h.signatures[0].label);
    assert_eq!(h.active_parameter, Some(1));
    // Outside of any call: nothing.
    assert!(help(&mut c, uri, pos_of(text, "func main() {", 0, 0)).is_none());
}

#[test]
fn inlay_hints_types_and_parameter_names() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let get = |c: &mut Client| -> Vec<(Position, String)> {
        let r: Option<Vec<InlayHint>> = c.request::<request::InlayHintRequest>(InlayHintParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 10000, character: 0 } },
            work_done_progress_params: Default::default(),
        });
        r.unwrap()
            .into_iter()
            .map(|h| {
                let label = match h.label {
                    InlayHintLabel::String(s) => s,
                    InlayHintLabel::LabelParts(p) => p.into_iter().map(|x| x.value).collect(),
                };
                (h.position, label)
            })
            .collect()
    };
    let hints = get(&mut c);
    let has = |pos: Position, label: &str| hints.iter().any(|h| h.0 == pos && h.1 == label);
    assert!(has(pos_of(text, "var frac", 0, 8), "int"), "{hints:?}");
    assert!(has(pos_of(text, "var bank", 0, 8), "Bank"), "{hints:?}");
    assert!(has(pos_of(text, "bank.Open(\"alice\"", 0, 10), "owner:"), "{hints:?}");
    assert!(has(pos_of(text, "10000)", 0, 0), "initial:"), "{hints:?}");
    // Declared types get no hint.
    assert!(!hints.iter().any(|h| h.0 == pos_of(text, "var short InsufficientFunds", 0, 9)));
    // Settings turn each kind off.
    c.notify::<notification::DidChangeConfiguration>(DidChangeConfigurationParams {
        settings: serde_json::json!({"zell": {"inlayHints": {"types": {"enabled": false}, "parameterNames": {"enabled": true}}}}),
    });
    let hints = get(&mut c);
    assert!(!hints.is_empty() && hints.iter().all(|h| h.1.ends_with(':')), "{hints:?}");
    c.notify::<notification::DidChangeConfiguration>(DidChangeConfigurationParams { settings: serde_json::json!({"zell": {"inlayHints": {"parameterNames": {"enabled": false}}}}) });
    assert!(get(&mut c).is_empty());
}

const FIXES: &str = "namespace app

import io
import strings

func Parse(s string) int! {
    if s == \"\" {
        return error(\"empty\")
    }
    return 1
}

func Twice(s string) int! {
    var n = Parse(s)
    return n * 2
}

func main() {
    var unused = 1
    println(Parse(\"2\"))
}
";

#[test]
fn code_actions_fix_structured_diagnostics() {
    let (dir, uri) = project_with("main.zl", FIXES);
    let mut c = Client::start(&dir);
    c.open(&uri, FIXES);
    let d = c.diagnostics(&uri);
    let code = |d: &Diagnostic| match &d.code {
        Some(NumberOrString::String(s)) => s.clone(),
        _ => String::new(),
    };
    assert!(d.iter().any(|d| code(d) == "E0002"), "{d:?}");
    // `var n = Parse(s)` in a fallible function: append `?`.
    let at = |text: &str, needle: &str| Range { start: pos_of(text, needle, 0, 0), end: pos_of(text, needle, 0, needle.len()) };
    let mut text = apply_action(&mut c, &uri, FIXES, at(FIXES, "Parse(s)"), "Propagate");
    assert!(text.contains("var n = Parse(s)?\n"), "{text}");
    // In main (not fallible) only the try ... catch wrap is offered.
    c.change(&uri, 2, &text);
    let actions = code_actions(&mut c, &uri, at(&text, "Parse(\"2\")"));
    assert!(actions.iter().all(|a| !a.title.starts_with("Propagate")), "{:?}", actions.iter().map(|a| &a.title).collect::<Vec<_>>());
    text = apply_action(&mut c, &uri, &text, at(&text, "Parse(\"2\")"), "Handle the error");
    assert!(text.contains("println(try Parse(\"2\") catch err { panic(err.Message()) })"), "{text}");
    // Unused variable: rename to `_` or remove.
    c.change(&uri, 3, &text);
    let removed = apply_action(&mut c, &uri, &text, at(&text, "unused"), "Remove unused variable");
    assert!(!removed.contains("var unused"), "{removed}");
    assert!(removed.contains("func main() {\n    println("), "{removed}");
    text = apply_action(&mut c, &uri, &text, at(&text, "unused"), "Rename unused to _");
    assert!(text.contains("var _ = 1"), "{text}");
    // Unused import (reported once nothing else is wrong): remove the line.
    c.change(&uri, 4, &text);
    let d = c.diagnostics(&uri);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(code(&d[0]), "E0003");
    text = apply_action(&mut c, &uri, &text, d[0].range, "Remove import strings");
    assert!(!text.contains("strings") && text.contains("import io\n\nfunc Parse"), "{text}");
    c.change(&uri, 5, &text);
    assert_eq!(c.diagnostics(&uri), vec![], "the fixed file compiles");
    // Missing import of a std name: add it.
    let no_io = text.replacen("import io\n", "", 1);
    c.change(&uri, 6, &no_io);
    let d = c.diagnostics(&uri);
    let unknown = d.iter().find(|d| code(d) == "E0004").unwrap_or_else(|| panic!("{d:?}"));
    text = apply_action(&mut c, &uri, &no_io, unknown.range, "Import io");
    assert!(text.starts_with("namespace app\n\nimport io\n"), "{text}");
    c.change(&uri, 7, &text);
    assert_eq!(c.diagnostics(&uri), vec![], "{text}");
}

#[test]
fn code_action_rewrites_import_of_removed_std_root() {
    let src = "namespace app\n\nimport std.io\n\nfunc main() {\n    println(\"hi\")\n}\n";
    let (dir, uri) = project_with("main.zl", src);
    let mut c = Client::start(&dir);
    c.open(&uri, src);
    let d = c.diagnostics(&uri);
    let removed = d.iter().find(|d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "E0005")).unwrap_or_else(|| panic!("{d:?}"));
    assert!(removed.message.contains("use `io` instead of `std.io`"), "{}", removed.message);
    let text = apply_action(&mut c, &uri, src, removed.range, "Use `import io`");
    assert!(text.contains("\nimport io\n") && !text.contains("std"), "{text}");
    c.change(&uri, 2, &text);
    assert_eq!(c.diagnostics(&uri), vec![], "{text}");
}

#[test]
fn formatting_whole_document_and_range() {
    let src = "namespace app\nimport io\nfunc main() {\n    var x=1\n    println(x+1)\n}\nfunc other() {\n    var y=2\n    println(y)\n}\n";
    let (dir, uri) = project_with("main.zl", src);
    let mut c = Client::start(&dir);
    c.open(&uri, src);
    let opts = FormattingOptions { tab_size: 4, insert_spaces: true, ..Default::default() };
    let edits: Option<Vec<TextEdit>> = c.request::<request::Formatting>(DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        options: opts.clone(),
        work_done_progress_params: Default::default(),
    });
    let formatted = apply_edits(src, &edits.unwrap());
    assert_eq!(formatted, zell::fmt::format_source(src).unwrap());
    assert!(formatted.contains("    var x = 1\n    println(x + 1)\n"), "{formatted}");
    // Range formatting only touches the selected lines (`var y=2`).
    let line = pos_of(src, "var y=2", 0, 0).line;
    let edits: Option<Vec<TextEdit>> = c.request::<request::RangeFormatting>(DocumentRangeFormattingParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        range: Range { start: Position { line, character: 0 }, end: Position { line, character: 5 } },
        options: opts.clone(),
        work_done_progress_params: Default::default(),
    });
    let partial = apply_edits(src, &edits.unwrap());
    assert!(partial.contains("    var y = 2\n") && partial.contains("    var x=1\n"), "{partial}");
    // A file with syntax errors is left alone.
    let broken = "namespace app\nfunc main( {\n";
    c.change(&uri, 2, broken);
    let edits: Option<Vec<TextEdit>> = c.request::<request::Formatting>(DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        options: opts,
        work_done_progress_params: Default::default(),
    });
    assert!(edits.is_none());
}

#[test]
fn workspace_symbols_and_highlights() {
    let (dir, files) = project(MULTI);
    let mut c = Client::start(&dir);
    let (uri, text) = &files[0];
    c.open(uri, text);
    #[allow(deprecated)]
    let r: Option<WorkspaceSymbolResponse> = c.request::<request::WorkspaceSymbolRequest>(WorkspaceSymbolParams { query: "user".into(), ..Default::default() });
    let Some(WorkspaceSymbolResponse::Flat(syms)) = r else { panic!("{r:?}") };
    let user = syms.iter().find(|s| s.name == "User").expect("User struct");
    assert_eq!(user.kind, SymbolKind::STRUCT);
    assert_eq!(&user.location.uri, uri);
    assert_eq!(range_text(text, &user.location.range), "User");
    assert_eq!(syms[0].name, "User", "exact matches first: {syms:?}");
    // Methods are found by fuzzy query, with their container.
    let r: Option<WorkspaceSymbolResponse> = c.request::<request::WorkspaceSymbolRequest>(WorkspaceSymbolParams { query: "rnm".into(), ..Default::default() });
    let Some(WorkspaceSymbolResponse::Flat(syms)) = r else { panic!() };
    assert!(syms.iter().any(|s| s.name == "Rename" && s.container_name.as_deref() == Some("app.models.User")), "{syms:?}");
    // Highlights of `Name`: its declaration and uses in this file.
    let h: Option<Vec<DocumentHighlight>> = c.request::<request::DocumentHighlightRequest>(DocumentHighlightParams {
        text_document_position_params: tdp(uri, pos_of(text, "    Name ", 0, 5)),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let h = h.unwrap();
    assert!(h.len() >= 3, "{h:?}");
    assert_eq!(h.iter().filter(|x| x.kind == Some(DocumentHighlightKind::WRITE)).count(), 1);
    assert!(h.iter().all(|x| range_text(text, &x.range) == "Name"));
}

#[test]
fn queries_work_in_a_file_with_syntax_errors() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    // An unfinished statement in Money and an unclosed call in main.
    let broken = text.replacen("    var frac = cents % 100\n", "    var frac = cents % 100\n    var oops = frac +\n", 1).replacen("    bank.Report()\n}", "    bank.Report(\n}", 1);
    c.change(uri, 2, &broken);
    let d = c.diagnostics(uri);
    assert!(!d.is_empty() && d.len() <= 3, "{d:?}");
    // Hover and definition use the current text.
    let h: Option<Hover> = c.request::<request::HoverRequest>(HoverParams { text_document_position_params: tdp(uri, pos_of(&broken, "frac +", 0, 1)), work_done_progress_params: Default::default() });
    match h.expect("hover in a broken file").contents {
        HoverContents::Markup(m) => assert!(m.value.contains("var frac int"), "{}", m.value),
        other => panic!("{other:?}"),
    }
    let def: Option<GotoDefinitionResponse> = c.request::<request::GotoDefinition>(GotoDefinitionParams {
        text_document_position_params: tdp(uri, pos_of(&broken, "Money(this.Balance)", 0, 1)),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let Some(GotoDefinitionResponse::Scalar(loc)) = def else { panic!("no definition") };
    assert_eq!(loc.range.start, pos_of(&broken, "func Money", 0, 5));
    // Member completion right after `.` in the broken file.
    let edited = broken.replacen("    var bank = Bank.New(\"Zell Savings\")\n", "    var bank = Bank.New(\"Zell Savings\")\n    bank.\n", 1);
    c.change(uri, 3, &edited);
    let r: Option<CompletionResponse> = c.request::<request::Completion>(CompletionParams {
        text_document_position: tdp(uri, pos_of(&edited, "    bank.\n", 0, 9)),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    });
    let items = match r {
        Some(CompletionResponse::Array(i)) => i,
        Some(CompletionResponse::List(l)) => l.items,
        None => Vec::new(),
    };
    assert!(items.iter().any(|i| i.label == "Process") && items.iter().any(|i| i.label == "Name"), "{:?}", items.iter().map(|i| &i.label).collect::<Vec<_>>());
}

#[test]
fn watched_files_are_added_and_removed() {
    let main = "namespace app\n\nimport io\n\nfunc main() {\n    println(Helper())\n}\n";
    let (dir, uri) = project_with("main.zl", main);
    let mut c = Client::start(&dir);
    c.open(&uri, main);
    assert!(c.diagnostics(&uri).iter().any(|d| d.message.contains("unknown identifier Helper")));
    let helper = dir.join("helper.zl");
    std::fs::write(&helper, "namespace app\n\nfunc Helper() int {\n    return 1\n}\n").unwrap();
    let helper_uri = Url::from_file_path(&helper).unwrap();
    c.notify::<notification::DidChangeWatchedFiles>(DidChangeWatchedFilesParams { changes: vec![FileEvent { uri: helper_uri.clone(), typ: FileChangeType::CREATED }] });
    assert_eq!(c.diagnostics(&uri), vec![]);
    std::fs::remove_file(&helper).unwrap();
    c.notify::<notification::DidChangeWatchedFiles>(DidChangeWatchedFilesParams { changes: vec![FileEvent { uri: helper_uri, typ: FileChangeType::DELETED }] });
    assert!(c.diagnostics(&uri).iter().any(|d| d.message.contains("unknown identifier Helper")));
}

fn definition(c: &mut Client, uri: &Url, pos: Position) -> Location {
    let def: Option<GotoDefinitionResponse> = c.request::<request::GotoDefinition>(GotoDefinitionParams {
        text_document_position_params: tdp(uri, pos),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    match def {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        other => panic!("no definition: {other:?}"),
    }
}

fn hover_text(c: &mut Client, uri: &Url, pos: Position) -> String {
    let h: Option<Hover> = c.request::<request::HoverRequest>(HoverParams { text_document_position_params: tdp(uri, pos), work_done_progress_params: Default::default() });
    match h.expect("no hover").contents {
        HoverContents::Markup(m) => m.value,
        other => panic!("{other:?}"),
    }
}

fn workspace_symbols(c: &mut Client, query: &str) -> Vec<SymbolInformation> {
    let r: Option<WorkspaceSymbolResponse> =
        c.request::<request::WorkspaceSymbolRequest>(WorkspaceSymbolParams { query: query.into(), work_done_progress_params: Default::default(), partial_result_params: Default::default() });
    match r {
        Some(WorkspaceSymbolResponse::Flat(v)) => v,
        other => panic!("{other:?}"),
    }
}

const STD_USER: &str = "namespace app

import io
import net

func main() {
    var parts = strings.Split(\"a,b\", \",\")
    println(len(parts))
}

async func fetch() void! {
    var r = await http.Get(\"http://localhost/\")?
    println(r.Text())
}
";

#[test]
fn definition_and_hover_reach_into_std_sources() {
    let (dir, _) = project(&[]);
    let path = dir.join("main.zl");
    std::fs::write(&path, STD_USER).unwrap();
    let uri = Url::from_file_path(&path).unwrap();
    let mut c = Client::start(&dir);
    c.open(&uri, STD_USER);
    // A C function: its generated extern declaration.
    let loc = definition(&mut c, &uri, pos_of(STD_USER, "Split(", 0, 1));
    let p = loc.uri.to_file_path().unwrap();
    assert!(p.ends_with("std/strings/strings_extern.zl"), "{}", p.display());
    let text = std::fs::read_to_string(&p).unwrap();
    assert_eq!(range_text(&text, &loc.range), "Split");
    assert!(line_text(&text, loc.range.start.line).starts_with("extern func Split("));
    // A Zell-source stdlib function (a template).
    let loc = definition(&mut c, &uri, pos_of(STD_USER, "Get(", 0, 1));
    let p = loc.uri.to_file_path().unwrap();
    assert!(p.ends_with("std/net/http/http_client.zl"), "{}", p.display());
    let text = std::fs::read_to_string(&p).unwrap();
    assert_eq!(range_text(&text, &loc.range), "Get");
    // Hover shows the doc comments of the std sources.
    let h = hover_text(&mut c, &uri, pos_of(STD_USER, "Split(", 0, 1));
    assert!(h.contains("func Split(string, string) []string") && h.contains("Split slices s into all substrings"), "{h}");
    let h = hover_text(&mut c, &uri, pos_of(STD_USER, "Get(", 0, 1));
    assert!(h.contains("func Get(url string) Task<Response!>"), "{h}");
    // Read-only: no rename; workspace symbols include std declarations.
    let msg = c.request_err::<request::PrepareRenameRequest>(tdp(&uri, pos_of(STD_USER, "Split(", 0, 1)));
    assert!(msg.contains("standard library"), "{msg}");
    let syms = workspace_symbols(&mut c, "Split");
    assert!(syms.iter().any(|s| s.name == "Split" && s.location.uri.path().ends_with("std/strings/strings_extern.zl")), "{syms:?}");
    // References of a std symbol across project files.
    let refs: Option<Vec<Location>> = c.request::<request::References>(ReferenceParams {
        text_document_position: tdp(&uri, pos_of(STD_USER, "Split(", 0, 1)),
        context: ReferenceContext { include_declaration: true },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    let refs = refs.unwrap();
    assert!(refs.iter().any(|l| l.uri == uri) && refs.iter().any(|l| l.uri.path().ends_with("strings_extern.zl")), "{refs:?}");
}

#[test]
fn dependencies_in_zell_deps_are_read_only_project_files() {
    let (dir, _) = project(&[]);
    let w = |p: &str, s: &str| {
        let f = dir.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, s).unwrap();
    };
    w("zell.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nnamespace = \"app\"\nentry = \"src/main.zl\"\n\n[dependencies]\n\"example.com/greet\" = \"^1\"\n");
    let main = "namespace app\n\nimport io\n\nfunc main() {\n    println(greet.Hello(\"x\"))\n}\n";
    w("src/main.zl", main);
    w(
        "zell.lock",
        "packages:\n- checksum: sha256:00\n  dependencies:\n  - greet\n  name: app\n  source: path:.\n  version: 0.1.0\n- checksum: h1:AA==\n  module: example.com/greet\n  name: greet\n  source: module\n  version: v1.0.0\nversion: 1\n",
    );
    let dep = "namespace greet\n\n// Hello greets who.\nfunc Hello(who string) string {\n    return \"hello \" + who\n}\n";
    w(".zell/deps/example.com/greet@v1.0.0/zell.toml", "[package]\nname = \"greet\"\nversion = \"1.0.0\"\n");
    w(".zell/deps/example.com/greet@v1.0.0/src/greet.zl", dep);
    let uri = Url::from_file_path(dir.join("src/main.zl")).unwrap();
    let mut c = Client::start(&dir);
    c.open(&uri, main);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let loc = definition(&mut c, &uri, pos_of(main, "Hello(", 0, 1));
    let p = loc.uri.to_file_path().unwrap();
    assert!(p.ends_with(".zell/deps/example.com/greet@v1.0.0/src/greet.zl"), "{}", p.display());
    assert_eq!(range_text(dep, &loc.range), "Hello");
    let h = hover_text(&mut c, &uri, pos_of(main, "Hello(", 0, 1));
    assert!(h.contains("Hello greets who."), "{h}");
    // References from the declaration in the dependency reach the project.
    let refs: Option<Vec<Location>> = c.request::<request::References>(ReferenceParams {
        text_document_position: tdp(&loc.uri, loc.range.start),
        context: ReferenceContext { include_declaration: false },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    assert!(refs.unwrap().iter().any(|l| l.uri == uri));
    let msg = c.request_err::<request::PrepareRenameRequest>(tdp(&uri, pos_of(main, "Hello(", 0, 1)));
    assert!(msg.contains("read-only"), "{msg}");
    let syms = workspace_symbols(&mut c, "Hello");
    assert!(syms.iter().any(|s| s.location.uri == loc.uri), "{syms:?}");
    // No diagnostics are published for library files.
    assert!(!c.notes.iter().any(|n| n.method == notification::PublishDiagnostics::METHOD && n.params.to_string().contains(".zell/deps")));
}

#[test]
fn hover_renders_doc_markdown_and_std_examples() {
    let text = "namespace app\n\nimport io\n\n// Words splits s. See `strings.Fields`.\n//\n// Deprecated: use strings.Fields.\nfunc Words(s string) []string {\n    return strings.Split(s, \" \")\n}\n\nfunc main() {\n    println(Words(\"a b\"))\n}\n";
    let (dir, uri) = project_with("main.zl", text);
    let mut c = Client::start(&dir);
    c.open(&uri, text);
    let hover = |c: &mut Client, pos| -> String {
        let h: Option<Hover> = c.request::<request::HoverRequest>(HoverParams { text_document_position_params: tdp(&uri, pos), work_done_progress_params: Default::default() });
        match h.expect("no hover").contents {
            HoverContents::Markup(m) => m.value,
            other => panic!("{other:?}"),
        }
    };
    // a user function: Deprecated tag and a link for the backtick symbol
    let h = hover(&mut c, pos_of(text, "Words(\"a b\")", 0, 1));
    assert!(h.contains("**~~Deprecated~~** use strings.Fields."), "{h}");
    assert!(h.contains("[`strings.Fields`](file://") && h.contains("strings_extern.zl#L"), "{h}");
    // a std function: its first example from std/strings/strings_test.zl, collapsed
    let h = hover(&mut c, pos_of(text, "Split(s", 0, 1));
    assert!(h.contains("<details><summary>Example</summary>") && h.contains("// Output") == false && h.contains("3 [a b c]"), "{h}");
}

#[path = "server/concurrency.rs"]
mod concurrency;
