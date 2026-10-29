//! Protocol features: incremental sync, cancellation and scheduling,
//! semantic token deltas and ranges, implementation / type definition /
//! declaration, resolve requests, code lenses, selection ranges,
//! progress, pull diagnostics and file renames.

use super::*;
use std::time::Instant;

fn doc(uri: &Url) -> TextDocumentIdentifier {
    TextDocumentIdentifier { uri: uri.clone() }
}

fn status(c: &mut Client) -> serde_json::Value {
    c.request_raw(zell_lsp::ANALYZER_STATUS, serde_json::Value::Null)
}

fn server_text(c: &mut Client, uri: &Url) -> Option<String> {
    serde_json::from_value(c.request_raw(zell_lsp::DOCUMENT_TEXT, serde_json::json!({ "uri": uri }))).unwrap()
}

fn change_range(c: &Client, uri: &Url, version: i32, changes: Vec<TextDocumentContentChangeEvent>) {
    c.notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams { text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version }, content_changes: changes });
}

fn goto<R: request::Request<Params = GotoDefinitionParams, Result = Option<GotoDefinitionResponse>>>(c: &mut Client, uri: &Url, pos: Position) -> Vec<Location> {
    match c.request::<R>(GotoDefinitionParams { text_document_position_params: tdp(uri, pos), work_done_progress_params: Default::default(), partial_result_params: Default::default() }) {
        Some(GotoDefinitionResponse::Scalar(l)) => vec![l],
        Some(GotoDefinitionResponse::Array(v)) => v,
        Some(GotoDefinitionResponse::Link(v)) => v.into_iter().map(|l| Location { uri: l.target_uri, range: l.target_selection_range }).collect(),
        None => Vec::new(),
    }
}

#[test]
fn capabilities_advertise_the_protocol_features() {
    let (dir, _) = project(&[]);
    let c = Client::start(&dir);
    let caps = &c.capabilities;
    assert!(matches!(&caps.text_document_sync, Some(TextDocumentSyncCapability::Options(o)) if o.change == Some(TextDocumentSyncKind::INCREMENTAL)));
    match &caps.semantic_tokens_provider {
        Some(SemanticTokensServerCapabilities::SemanticTokensOptions(o)) => {
            assert_eq!(o.range, Some(true));
            assert_eq!(o.full, Some(SemanticTokensFullOptions::Delta { delta: Some(true) }));
        }
        other => panic!("{other:?}"),
    }
    assert!(caps.implementation_provider.is_some() && caps.type_definition_provider.is_some() && caps.declaration_provider.is_some());
    assert_eq!(caps.completion_provider.as_ref().and_then(|c| c.resolve_provider), Some(true));
    assert_eq!(caps.code_lens_provider.as_ref().and_then(|c| c.resolve_provider), Some(true));
    assert!(matches!(&caps.inlay_hint_provider, Some(OneOf::Right(InlayHintServerCapabilities::Options(o))) if o.resolve_provider == Some(true)));
    assert!(caps.selection_range_provider.is_some());
    assert!(matches!(&caps.diagnostic_provider, Some(DiagnosticServerCapabilities::Options(o)) if o.workspace_diagnostics && o.inter_file_dependencies));
    let ops = caps.workspace.as_ref().and_then(|w| w.file_operations.as_ref()).unwrap();
    assert!(ops.will_rename.is_some() && ops.did_rename.is_some());
}

/// xorshift64*: deterministic randomness for the fuzz test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// UTF-16 position of char index `i` in `chars` (the reference model).
fn model_position(chars: &[char], i: usize) -> Position {
    let (mut line, mut col) = (0u32, 0u32);
    for c in &chars[..i] {
        if *c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf16() as u32;
        }
    }
    Position { line, character: col }
}

#[test]
fn incremental_sync_matches_a_reference_model() {
    let text = "namespace app\n\nimport io\n\n// Привет, 𝄞!\nfunc main() {\n    println(\"ü\")\n}\n";
    let (dir, uri) = project_with("main.zl", text);
    let mut c = Client::start(&dir);
    c.open(&uri, text);
    let mut model: Vec<char> = text.chars().collect();
    let alphabet: Vec<char> = "ab x(){}\n.;\"пр𝄞é\t".chars().collect();
    let mut rng = Rng(0x5eed_1234_abcd_0042);
    let mut version = 1;
    for round in 0..300 {
        version += 1;
        let mut changes = Vec::new();
        for _ in 0..1 + rng.below(4) {
            let insert: String = (0..rng.below(6)).map(|_| alphabet[rng.below(alphabet.len())]).collect();
            if rng.below(40) == 0 {
                // A full-text change in the middle of incremental ones.
                model = insert.chars().chain("namespace app\n".chars()).collect();
                changes.push(TextDocumentContentChangeEvent { range: None, range_length: None, text: model.iter().collect() });
                continue;
            }
            let a = rng.below(model.len() + 1);
            let b = (a + rng.below(8)).min(model.len());
            let range = Range { start: model_position(&model, a), end: model_position(&model, b) };
            model.splice(a..b, insert.chars());
            changes.push(TextDocumentContentChangeEvent { range: Some(range), range_length: None, text: insert });
        }
        change_range(&c, &uri, version, changes);
        if round % 10 == 0 || round == 299 {
            let want: String = model.iter().collect();
            assert_eq!(server_text(&mut c, &uri).as_deref(), Some(want.as_str()), "round {round}");
        }
    }
    // The analysis sees the synced text: replace it with a valid program.
    let start = Position { line: 0, character: 0 };
    let end = model_position(&model, model.len());
    change_range(&c, &uri, version + 1, vec![TextDocumentContentChangeEvent { range: Some(Range { start, end }), range_length: None, text: text.into() }]);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let edited = text.replacen("println(\"ü\")", "println(\"ü\", 1)", 1);
    let at = pos_of(text, "\")", 0, 1);
    let at = Position { line: at.line, character: at.character - 1 }; // after the UTF-16 `ü"`
    change_range(&c, &uri, version + 2, vec![TextDocumentContentChangeEvent { range: Some(Range { start: at, end: at }), range_length: None, text: ", 1".into() }]);
    assert_eq!(server_text(&mut c, &uri).as_deref(), Some(edited.as_str()));
    assert_eq!(c.diagnostics(&uri), vec![]);
}

const SLOW: fn(&mut InitializeParams) = |p| p.initialization_options = Some(serde_json::json!({"debug": {"analysisDelayMs": 1500}}));

#[test]
fn cancel_request_answers_a_waiting_request() {
    let text = "namespace app\n\nfunc F(a int) int {\n    return a\n}\n";
    let (dir, uri) = project_with("main.zl", text);
    let mut c = Client::start_with(&dir, SLOW);
    c.open(&uri, text);
    let _ = status(&mut c); // the server is past loading std and the workspace
    let t0 = Instant::now();
    let params = serde_json::to_value(HoverParams { text_document_position_params: tdp(&uri, pos_of(text, "F(", 0, 0)), work_done_progress_params: Default::default() }).unwrap();
    let id = c.send_request(request::HoverRequest::METHOD, params.clone());
    let id_num = match serde_json::to_value(&id).unwrap() {
        serde_json::Value::Number(n) => n.as_i64().unwrap() as i32,
        other => panic!("{other}"),
    };
    c.notify::<notification::Cancel>(CancelParams { id: NumberOrString::Number(id_num) });
    let r = c.response(&id);
    let e = r.error.expect("a cancelled request gets an error");
    assert_eq!(e.code, lsp_server::ErrorCode::RequestCanceled as i32, "{}", e.message);
    assert!(t0.elapsed() < Duration::from_millis(1200), "the cancellation did not wait for the analysis: {:?}", t0.elapsed());
    let st = status(&mut c);
    assert_eq!(st["stats"]["cancelledRequests"], 1, "{st}");
    // The same request without cancelling waits for the analysis.
    let h = c.request_raw(request::HoverRequest::METHOD, params);
    assert!(h.to_string().contains("func F(a int) int"), "{h}");
    // Cancelling an answered request is harmless.
    c.notify::<notification::Cancel>(CancelParams { id: NumberOrString::Number(id_num) });
    assert_eq!(status(&mut c)["stats"]["cancelledRequests"], 1);
}

#[test]
fn edits_supersede_the_analysis_in_flight() {
    let v1 = "namespace app\n\nfunc F(a int) int {\n    return a\n}\n";
    let v2 = v1.replace("a int) int", "b int) int").replace("return a", "return b");
    let v3 = v1.replace("a int) int", "c int) int").replace("return a", "return c");
    let (dir, uri) = project_with("main.zl", v1);
    let mut c = Client::start_with(&dir, |p| p.initialization_options = Some(serde_json::json!({"debug": {"analysisDelayMs": 300}})));
    c.open(&uri, v1);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let before = status(&mut c)["stats"].clone();
    let at = pos_of(v1, "F(", 0, 0);
    let hover = serde_json::to_value(HoverParams { text_document_position_params: tdp(&uri, at), work_done_progress_params: Default::default() }).unwrap();
    // v2, a request (its analysis starts at once), v3 before it finishes.
    c.change(&uri, 2, &v2);
    let first = c.send_request(request::HoverRequest::METHOD, hover.clone());
    c.change(&uri, 3, &v3);
    let second = c.send_request(request::HoverRequest::METHOD, hover);
    for id in [first, second] {
        let r = c.response(&id);
        let h = r.result.unwrap().to_string();
        assert!(h.contains("func F(c int) int"), "answered against the latest text: {h}");
    }
    let after = status(&mut c)["stats"].clone();
    let n = |v: &serde_json::Value, k: &str| v[k].as_u64().unwrap();
    assert!(n(&after, "cancelled") > n(&before, "cancelled"), "{before} -> {after}");
    assert_eq!(n(&after, "completed"), n(&before, "completed") + 1, "one analysis for the burst: {before} -> {after}");
    assert!(n(&after, "queuedRequests") >= 2);
}

fn flat(tokens: &[SemanticToken]) -> Vec<u32> {
    tokens.iter().flat_map(|t| [t.delta_line, t.delta_start, t.length, t.token_type, t.token_modifiers_bitset]).collect()
}

#[test]
fn semantic_tokens_delta_and_range() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let full = |c: &mut Client| -> SemanticTokens {
        match c.request::<request::SemanticTokensFullRequest>(SemanticTokensParams { text_document: doc(uri), work_done_progress_params: Default::default(), partial_result_params: Default::default() }) {
            Some(SemanticTokensResult::Tokens(t)) => t,
            other => panic!("{other:?}"),
        }
    };
    let delta = |c: &mut Client, prev: &str| -> SemanticTokensFullDeltaResult {
        c.request::<request::SemanticTokensFullDeltaRequest>(SemanticTokensDeltaParams {
            text_document: doc(uri),
            previous_result_id: prev.into(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .unwrap()
    };
    let first = full(&mut c);
    let id1 = first.result_id.clone().expect("a result id");
    // An edit in the middle: the delta is one edit that rebuilds the new tokens.
    let edited = text.replacen("    var frac = cents % 100\n", "    var frac = cents % 100\n    // a comment\n    var extra = frac + 1\n    frac = extra - 1\n", 1);
    c.change(uri, 2, &edited);
    let (id2, edits) = match delta(&mut c, &id1) {
        SemanticTokensFullDeltaResult::TokensDelta(d) => (d.result_id.unwrap(), d.edits),
        other => panic!("expected a delta: {other:?}"),
    };
    assert_ne!(id1, id2);
    assert_eq!(edits.len(), 1);
    assert!((edits[0].delete_count as usize) < flat(&first.data).len() / 4, "only the changed part is replaced: {:?}", edits[0].delete_count);
    let mut data = flat(&first.data);
    for e in edits.iter().rev() {
        data.splice(e.start as usize..(e.start + e.delete_count) as usize, flat(e.data.as_deref().unwrap_or(&[])));
    }
    // No change since: an empty delta from the new id.
    match delta(&mut c, &id2) {
        SemanticTokensFullDeltaResult::TokensDelta(d) => assert!(d.edits.is_empty(), "{:?}", d.edits),
        other => panic!("{other:?}"),
    }
    let second = full(&mut c);
    assert_eq!(data, flat(&second.data), "applying the delta gives the full tokens");
    // An unknown previous id: the full tokens.
    assert!(matches!(delta(&mut c, "nope"), SemanticTokensFullDeltaResult::Tokens(t) if t.data == second.data));
    // Range: the tokens of the lines asked for (relative encoding from the document start).
    let decode = |toks: &[SemanticToken]| -> Vec<(u32, u32, u32, u32)> {
        let (mut line, mut col) = (0, 0);
        toks.iter()
            .map(|t| {
                if t.delta_line > 0 {
                    line += t.delta_line;
                    col = t.delta_start;
                } else {
                    col += t.delta_start;
                }
                (line, col, t.length, t.token_type)
            })
            .collect()
    };
    let (l1, l2) = (pos_of(&edited, "func Money", 0, 0).line, pos_of(&edited, "frac = extra", 0, 0).line);
    let r = c
        .request::<request::SemanticTokensRangeRequest>(SemanticTokensRangeParams {
            text_document: doc(uri),
            range: Range { start: Position { line: l1, character: 0 }, end: Position { line: l2 + 1, character: 0 } },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .unwrap();
    let SemanticTokensRangeResult::Tokens(r) = r else { panic!() };
    let want: Vec<_> = decode(&second.data).into_iter().filter(|t| t.0 >= l1 && t.0 <= l2).collect();
    assert!(!want.is_empty());
    assert_eq!(decode(&r.data), want);
}

const SHAPES: &str = "namespace app

import io

interface Shape {
    func Area() int
    func Name() string
}

sealed interface Token {
    func Text() string
}

struct Square {
    Side int
    func Area() int { return this.Side * this.Side }
    func Name() string { return \"square\" }
}

struct Rect {
    W int
    H int
    func Area() int { return this.W * this.H }
    func Name() string { return \"rect\" }
}

struct NotShape {
    func Area() string { return \"\" }
    func Name() string { return \"\" }
}

enum Word implements Token {
    Plus
    Minus
    func Text() string { return \"w\" }
}

struct Box<T> {
    Value T
    func Get() T { return this.Value }
}

func NewRect() *Rect {
    return new Rect{W: 1, H: 2}
}

func main() {
    var s Shape = Square{Side: 2}
    var r = NewRect()
    var b = Box<int>{Value: 1}
    var t Token = Word.Plus
    var n = NotShape{}
    println(s.Area(), r.Area(), b.Get(), t.Text(), n.Name())
}
";

#[test]
fn implementation_type_definition_and_declaration() {
    let (dir, uri) = project_with("main.zl", SHAPES);
    let mut c = Client::start(&dir);
    c.open(&uri, SHAPES);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let names = |locs: &[Location]| -> Vec<String> {
        locs.iter()
            .map(|l| {
                assert_eq!(l.uri, uri);
                range_text(SHAPES, &l.range).to_string()
            })
            .collect()
    };
    let line_of = |l: &Location| line_text(SHAPES, l.range.start.line).trim().to_string();
    // An interface: the structs with all its methods (not NotShape: wrong signature).
    let imps = goto::<request::GotoImplementation>(&mut c, &uri, pos_of(SHAPES, "interface Shape", 0, 10));
    assert_eq!(names(&imps), ["Square", "Rect"]);
    // The same from a use of the interface.
    let imps = goto::<request::GotoImplementation>(&mut c, &uri, pos_of(SHAPES, "var s Shape", 0, 7));
    assert_eq!(names(&imps), ["Square", "Rect"]);
    // An interface method: the implementing methods.
    let imps = goto::<request::GotoImplementation>(&mut c, &uri, pos_of(SHAPES, "func Area() int\n", 0, 5));
    assert_eq!(imps.iter().map(line_of).collect::<Vec<_>>(), ["func Area() int { return this.Side * this.Side }", "func Area() int { return this.W * this.H }"]);
    // A sealed interface implemented by an enum.
    let imps = goto::<request::GotoImplementation>(&mut c, &uri, pos_of(SHAPES, "interface Token", 0, 10));
    assert_eq!(names(&imps), ["Word"]);
    // Nothing for a struct.
    assert!(goto::<request::GotoImplementation>(&mut c, &uri, pos_of(SHAPES, "struct Rect", 0, 7)).is_empty());

    // Type definitions: through a pointer, an interface, an enum, a generic instance.
    let td = |c: &mut Client, needle: &str, delta: usize| names(&goto::<request::GotoTypeDefinition>(c, &uri, pos_of(SHAPES, needle, 0, delta)));
    assert_eq!(td(&mut c, "r.Area()", 0), ["Rect"]);
    assert_eq!(td(&mut c, "s.Area()", 0), ["Shape"]);
    assert_eq!(td(&mut c, "t.Text()", 0), ["Token"]);
    let b = goto::<request::GotoTypeDefinition>(&mut c, &uri, pos_of(SHAPES, "b.Get()", 0, 0));
    assert_eq!(names(&b), ["Box"], "the generic declaration");
    assert_eq!(b[0].range.start, pos_of(SHAPES, "struct Box", 0, 7));
    assert_eq!(td(&mut c, "NewRect()\n", 0), ["Rect"], "a function: its result type");
    assert_eq!(td(&mut c, "Side int", 0), Vec::<String>::new(), "a builtin type has no declaration");

    // Declaration = definition.
    let decl = goto::<request::GotoDeclaration>(&mut c, &uri, pos_of(SHAPES, "r.Area()", 0, 2));
    let def = goto::<request::GotoDefinition>(&mut c, &uri, pos_of(SHAPES, "r.Area()", 0, 2));
    assert_eq!(decl, def);
    assert_eq!(line_of(&decl[0]), "func Area() int { return this.W * this.H }");
}

#[test]
fn completion_resolve_adds_documentation() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let anchor = "    var bank = Bank.New(\"Zell Savings\")\n";
    let edited = text.replacen(anchor, &format!("{anchor}    Mon\n"), 1);
    c.change(uri, 2, &edited);
    let r: Option<CompletionResponse> = c.request::<request::Completion>(CompletionParams {
        text_document_position: tdp(uri, pos_of(&edited, "    Mon\n", 0, 7)),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    });
    let Some(CompletionResponse::List(CompletionList { items, .. })) = r else { panic!("{r:?}") };
    let money = items.iter().find(|i| i.label == "Money").expect("Money").clone();
    assert!(money.documentation.is_none(), "the list stays light");
    assert!(money.data.is_some());
    let resolved: CompletionItem = c.request::<request::ResolveCompletionItem>(money.clone());
    assert_eq!(resolved.detail.as_deref(), Some("func Money(cents int) string"));
    match &resolved.documentation {
        Some(Documentation::MarkupContent(m)) => assert!(m.kind == MarkupKind::Markdown && m.value.contains("Amounts are integer cents.") && m.value.contains("```zell"), "{}", m.value),
        other => panic!("{other:?}"),
    }
    // After another analysis the item still resolves (by its full name).
    c.change(uri, 3, &edited.replacen("Mon\n", "Mo\n", 1));
    let again: CompletionItem = c.request::<request::ResolveCompletionItem>(money);
    assert!(again.documentation.is_some());
    // Keywords and builtins carry no data and come back unchanged.
    let kw = CompletionItem { label: "var".into(), kind: Some(CompletionItemKind::KEYWORD), ..Default::default() };
    let r: CompletionItem = c.request::<request::ResolveCompletionItem>(kw.clone());
    assert_eq!(r, kw);
}

#[test]
fn inlay_hint_and_code_action_resolve() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let hints: Vec<InlayHint> = c
        .request::<request::InlayHintRequest>(InlayHintParams {
            text_document: doc(uri),
            range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 10000, character: 0 } },
            work_done_progress_params: Default::default(),
        })
        .unwrap();
    let label = |h: &InlayHint| match &h.label {
        InlayHintLabel::String(s) => s.clone(),
        InlayHintLabel::LabelParts(p) => p.iter().map(|x| x.value.clone()).collect(),
    };
    let tooltip = |h: &InlayHint| match &h.tooltip {
        Some(InlayHintTooltip::MarkupContent(m)) => m.value.clone(),
        other => panic!("{other:?}"),
    };
    let bank = hints.iter().find(|h| h.position == pos_of(text, "var bank", 0, 8)).expect("type hint of bank").clone();
    assert_eq!(label(&bank), "Bank");
    assert!(bank.tooltip.is_none() && bank.data.is_some(), "tooltips are resolved lazily");
    let r: InlayHint = c.request::<request::InlayHintResolveRequest>(bank);
    let t = tooltip(&r);
    assert!(t.contains("var bank Bank") && t.contains("struct Bank"), "{t}");
    let owner = hints.iter().find(|h| label(h) == "owner:").unwrap().clone();
    let t = tooltip(&c.request::<request::InlayHintResolveRequest>(owner));
    assert!(t.contains("func Open(owner string, initial int)"), "{t}");

    // Code actions are complete already: resolve returns them as they are.
    let (dir, uri) = project_with("main.zl", FIXES);
    let mut c = Client::start(&dir);
    c.open(&uri, FIXES);
    let actions = code_actions(&mut c, &uri, Range { start: pos_of(FIXES, "unused", 0, 0), end: pos_of(FIXES, "unused", 0, 6) });
    assert!(!actions.is_empty());
    let r: CodeAction = c.request::<request::CodeActionResolveRequest>(actions[0].clone());
    assert_eq!(r, actions[0]);
}

const LENSES: &str = "namespace app

import io

// Add adds.
func Add(a int, b int) int {
    return a + b
}

func helper() int {
    return Add(2, 2)
}

@test func TestAdd(t *testing.T) {
    testing.Equal(t, Add(1, 2), 3)
}

func main() {
    println(Add(1, 2), helper())
}
";

#[test]
fn code_lenses_for_tests_main_and_references() {
    let (dir, uri) = project_with("main.zl", LENSES);
    let mut c = Client::start(&dir);
    c.open(&uri, LENSES);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let lenses: Vec<CodeLens> = c.request::<request::CodeLensRequest>(CodeLensParams { text_document: doc(&uri), work_done_progress_params: Default::default(), partial_result_params: Default::default() }).unwrap();
    let at = |l: &CodeLens| range_text(LENSES, &l.range).to_string();
    let cmds: Vec<(String, String, String)> = lenses.iter().filter_map(|l| l.command.as_ref().map(|c| (at(l), c.title.clone(), c.command.clone()))).collect();
    assert_eq!(
        cmds,
        [
            ("TestAdd".to_string(), "▶ Run test".to_string(), "zell.runTest".to_string()),
            ("TestAdd".into(), "Debug test".into(), "zell.debugTest".into()),
            ("main".into(), "▶ Run".into(), "zell.run".into()),
        ]
    );
    let run_test = lenses.iter().find(|l| l.command.as_ref().is_some_and(|c| c.command == "zell.runTest")).unwrap();
    assert_eq!(run_test.command.as_ref().unwrap().arguments, Some(vec![serde_json::json!(uri), serde_json::json!("TestAdd")]));
    // Only exported declarations get a (lazy) reference count.
    let unresolved: Vec<&CodeLens> = lenses.iter().filter(|l| l.command.is_none()).collect();
    assert_eq!(unresolved.iter().map(|l| at(l)).collect::<Vec<_>>(), ["Add"]);
    let r: CodeLens = c.request::<request::CodeLensResolve>(unresolved[0].clone());
    let cmd = r.command.expect("resolved");
    assert_eq!(cmd.title, "3 references");
    assert_eq!(cmd.command, "zell.showReferences");
    let args = cmd.arguments.unwrap();
    assert_eq!(args[0], serde_json::json!(uri));
    let locs: Vec<Location> = serde_json::from_value(args[2].clone()).unwrap();
    assert_eq!(locs.len(), 3);
    assert!(locs.iter().all(|l| range_text(LENSES, &l.range) == "Add"));
}

#[test]
fn selection_ranges_expand_from_the_token_to_the_file() {
    let (dir, files) = project(&["bank.zl"]);
    let (uri, text) = &files[0];
    let mut c = Client::start(&dir);
    c.open(uri, text);
    let pos = pos_of(text, "cents % 100", 0, 9);
    let r: Vec<SelectionRange> = c.request::<request::SelectionRangeRequest>(SelectionRangeParams {
        text_document: doc(uri),
        positions: vec![pos, Position { line: 0, character: 2 }],
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    })
    .unwrap();
    assert_eq!(r.len(), 2);
    let mut chain = Vec::new();
    let mut cur = Some(&r[0]);
    while let Some(s) = cur {
        chain.push(s.range);
        cur = s.parent.as_deref();
    }
    let slice = |r: &Range| -> String {
        let (a, b) = (offset_of(text, r.start), offset_of(text, r.end));
        text[a..b].to_string()
    };
    let texts: Vec<String> = chain.iter().map(slice).collect();
    assert_eq!(texts[0], "100");
    assert_eq!(texts[1], "cents % 100");
    assert_eq!(texts[2], "var frac = cents % 100");
    assert!(texts.iter().any(|t| t.starts_with("{\n") && t.contains("var frac")), "the block: {texts:?}");
    assert!(texts.iter().any(|t| t.starts_with("func Money(cents int) string {")), "the function: {texts:?}");
    assert_eq!(texts.last().unwrap(), text, "the file");
    for w in chain.windows(2) {
        let (inner, outer) = ((offset_of(text, w[0].start), offset_of(text, w[0].end)), (offset_of(text, w[1].start), offset_of(text, w[1].end)));
        assert!(outer.0 <= inner.0 && inner.1 <= outer.1 && inner != outer, "{texts:?}");
    }
}

#[test]
fn progress_is_reported_for_initial_indexing() {
    let (dir, files) = project(MULTI);
    let mut c = Client::start_with(&dir, |p| p.capabilities.window = Some(WindowClientCapabilities { work_done_progress: Some(true), ..Default::default() }));
    let (uri, text) = &files[0];
    c.open(uri, text);
    assert_eq!(c.diagnostics(uri), vec![]);
    let create = c.server_requests.iter().find(|r| r.method == request::WorkDoneProgressCreate::METHOD).expect("window/workDoneProgress/create");
    let token: WorkDoneProgressCreateParams = serde_json::from_value(create.params.clone()).unwrap();
    let progress: Vec<WorkDoneProgress> = c
        .notes
        .iter()
        .filter(|n| n.method == notification::Progress::METHOD)
        .map(|n| serde_json::from_value::<ProgressParams>(n.params.clone()).unwrap())
        .filter(|p| p.token == token.token)
        .map(|p| match p.value {
            ProgressParamsValue::WorkDone(w) => w,
        })
        .collect();
    assert!(matches!(progress.first(), Some(WorkDoneProgress::Begin(b)) if b.message.as_deref() == Some("loading the standard library")), "{progress:?}");
    assert!(progress.iter().any(|p| matches!(p, WorkDoneProgress::Report(r) if r.message.as_deref().is_some_and(|m| m.starts_with("analysing 3 files")))), "{progress:?}");
    assert!(matches!(progress.last(), Some(WorkDoneProgress::End(_))), "{progress:?}");
    // Without the client capability: no progress at all.
    let (dir, _) = project(MULTI);
    let mut c = Client::start(&dir);
    let _ = status(&mut c);
    let _ = c.request::<request::WorkspaceSymbolRequest>(WorkspaceSymbolParams { query: "User".into(), ..Default::default() });
    assert!(c.server_requests.is_empty() && !c.notes.iter().any(|n| n.method == notification::Progress::METHOD));
}

#[test]
fn pull_diagnostics_for_documents_and_the_workspace() {
    let main = "namespace app\n\nimport io\n\nfunc main() {\n    println(Helper())\n}\n";
    let (dir, uri) = project_with("main.zl", main);
    let helper = dir.join("helper.zl");
    std::fs::write(&helper, "namespace app\n\nfunc Other() int {\n    return 1\n}\n").unwrap();
    let helper_uri = Url::from_file_path(&helper).unwrap();
    let mut c = Client::start_with(&dir, |p| {
        p.capabilities.text_document = Some(TextDocumentClientCapabilities { diagnostic: Some(DiagnosticClientCapabilities::default()), ..Default::default() });
        p.capabilities.workspace = Some(WorkspaceClientCapabilities { diagnostic: Some(DiagnosticWorkspaceClientCapabilities { refresh_support: Some(true) }), ..Default::default() });
    });
    c.open(&uri, main);
    let pull = |c: &mut Client, prev: Option<String>| -> DocumentDiagnosticReport {
        match c.request::<request::DocumentDiagnosticRequest>(DocumentDiagnosticParams {
            text_document: doc(&uri),
            identifier: None,
            previous_result_id: prev,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }) {
            DocumentDiagnosticReportResult::Report(r) => r,
            other => panic!("{other:?}"),
        }
    };
    let DocumentDiagnosticReport::Full(full) = pull(&mut c, None) else { panic!("a full report first") };
    let report = full.full_document_diagnostic_report;
    assert_eq!(report.items.len(), 1, "{:?}", report.items);
    assert!(report.items[0].message.contains("unknown identifier Helper"));
    let id = report.result_id.unwrap();
    assert!(matches!(pull(&mut c, Some(id.clone())), DocumentDiagnosticReport::Unchanged(u) if u.unchanged_document_diagnostic_report.result_id == id));
    // A pulling client gets no pushed diagnostics.
    assert!(!c.notes.iter().any(|n| n.method == notification::PublishDiagnostics::METHOD));

    // Workspace pull: every project file; unchanged reports keep the request
    // open until something changes.
    let ws = |prev: Vec<PreviousResultId>| serde_json::to_value(WorkspaceDiagnosticParams { identifier: None, previous_result_ids: prev, work_done_progress_params: Default::default(), partial_result_params: Default::default() }).unwrap();
    let r: WorkspaceDiagnosticReportResult = serde_json::from_value(c.request_raw(request::WorkspaceDiagnosticRequest::METHOD, ws(Vec::new()))).unwrap();
    let WorkspaceDiagnosticReportResult::Report(r) = r else { panic!() };
    let mut known = Vec::new();
    for item in &r.items {
        let WorkspaceDocumentDiagnosticReport::Full(f) = item else { panic!("{item:?}") };
        known.push(PreviousResultId { uri: f.uri.clone(), value: f.full_document_diagnostic_report.result_id.clone().unwrap() });
    }
    assert_eq!(known.len(), 2);
    assert!(known.iter().any(|k| k.uri == uri) && known.iter().any(|k| k.uri == helper_uri));
    let parked = c.send_request(request::WorkspaceDiagnosticRequest::METHOD, ws(known.clone()));
    let _ = status(&mut c);
    assert!(!c.responses.iter().any(|r| r.id == parked), "nothing changed: the request waits");
    // Defining Helper fixes main.zl: the parked request is answered.
    c.change(&uri, 2, &format!("{main}\nfunc Helper() int {{\n    return 2\n}}\n"));
    let r = c.response(&parked);
    let r: WorkspaceDiagnosticReportResult = serde_json::from_value(r.result.unwrap()).unwrap();
    let WorkspaceDiagnosticReportResult::Report(r) = r else { panic!() };
    let main_report = r.items.iter().find_map(|i| match i {
        WorkspaceDocumentDiagnosticReport::Full(f) if f.uri == uri => Some(f),
        _ => None,
    });
    assert!(main_report.expect("main.zl changed").full_document_diagnostic_report.items.is_empty());
    assert!(r.items.iter().any(|i| matches!(i, WorkspaceDocumentDiagnosticReport::Unchanged(u) if u.uri == helper_uri)));
    assert!(c.server_requests.iter().any(|r| r.method == request::WorkspaceDiagnosticRefresh::METHOD), "the client is asked to refresh");
    // Up to date again: parked, and a parked request can be cancelled.
    let known: Vec<PreviousResultId> = r
        .items
        .iter()
        .map(|i| match i {
            WorkspaceDocumentDiagnosticReport::Full(f) => PreviousResultId { uri: f.uri.clone(), value: f.full_document_diagnostic_report.result_id.clone().unwrap() },
            WorkspaceDocumentDiagnosticReport::Unchanged(u) => PreviousResultId { uri: u.uri.clone(), value: u.unchanged_document_diagnostic_report.result_id.clone() },
        })
        .collect();
    let parked = c.send_request(request::WorkspaceDiagnosticRequest::METHOD, ws(known));
    let n = match serde_json::to_value(&parked).unwrap() {
        serde_json::Value::Number(n) => n.as_i64().unwrap() as i32,
        other => panic!("{other}"),
    };
    c.notify::<notification::Cancel>(CancelParams { id: NumberOrString::Number(n) });
    let e = c.response(&parked).error.expect("cancelled");
    assert_eq!(e.code, lsp_server::ErrorCode::RequestCanceled as i32);
}

/// A package `app` with `src/main.zl` and the given files under `src/`.
fn package(files: &[(&str, &str)]) -> PathBuf {
    let (dir, _) = project(&[]);
    std::fs::write(dir.join("zell.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nnamespace = \"app\"\nentry = \"src/main.zl\"\n").unwrap();
    for (p, text) in files {
        let f = dir.join("src").join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }
    dir
}

fn will_rename(c: &mut Client, from: &Path, to: &Path) -> Option<WorkspaceEdit> {
    let files = vec![FileRename { old_uri: Url::from_file_path(from).unwrap().to_string(), new_uri: Url::from_file_path(to).unwrap().to_string() }];
    c.request::<request::WillRenameFiles>(RenameFilesParams { files })
}

/// Applies `edit` to the files on disk, moves `from` to `to` and tells the server.
fn apply_and_move(c: &Client, edit: &WorkspaceEdit, from: &Path, to: &Path) {
    for (uri, edits) in edit.changes.as_ref().unwrap() {
        let p = uri.to_file_path().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, apply_edits(&text, edits)).unwrap();
    }
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::rename(from, to).unwrap();
    let files = vec![FileRename { old_uri: Url::from_file_path(from).unwrap().to_string(), new_uri: Url::from_file_path(to).unwrap().to_string() }];
    c.notify::<notification::DidRenameFiles>(RenameFilesParams { files });
}

const RENAME_MAIN: &str = "namespace app\n\nimport io\nimport app.models\n\nfunc main() {\n    var u = NewUser(\"x\")\n    println(u.Name)\n}\n";
const RENAME_USER: &str = "namespace app.models\n\nstruct User {\n    Name string\n}\n\nfunc NewUser(n string) User {\n    return User{Name: n}\n}\n";

#[test]
fn renaming_a_file_updates_its_namespace_and_imports() {
    let dir = package(&[("main.zl", RENAME_MAIN), ("models/user.zl", RENAME_USER)]);
    let main = dir.join("src/main.zl");
    let main_uri = Url::from_file_path(&main).unwrap();
    let mut c = Client::start(&dir);
    c.open(&main_uri, RENAME_MAIN);
    assert_eq!(c.diagnostics(&main_uri), vec![]);
    let (from, to) = (dir.join("src/models/user.zl"), dir.join("src/accounts/user.zl"));
    let edit = will_rename(&mut c, &from, &to).expect("edits for a path-derived namespace");
    let changes = edit.changes.as_ref().unwrap();
    assert_eq!(changes.len(), 2, "{changes:?}");
    let user_edits = &changes[&Url::from_file_path(&from).unwrap()];
    assert_eq!(apply_edits(RENAME_USER, user_edits), RENAME_USER.replace("namespace app.models", "namespace app.accounts"));
    let new_main = apply_edits(RENAME_MAIN, &changes[&main_uri]);
    assert_eq!(new_main, RENAME_MAIN.replace("import app.models", "import app.accounts"));
    // The client applies the edits (main.zl is open), moves the file and tells the server.
    let main_edits = changes[&main_uri].clone();
    let mut disk = edit.clone();
    disk.changes.as_mut().unwrap().remove(&main_uri);
    apply_and_move(&c, &disk, &from, &to);
    c.change(&main_uri, 2, &apply_edits(RENAME_MAIN, &main_edits));
    assert_eq!(c.diagnostics(&main_uri), vec![]);
    let def = definition(&mut c, &main_uri, pos_of(RENAME_MAIN, "NewUser(", 0, 1));
    assert_eq!(def.uri, Url::from_file_path(&to).unwrap(), "the moved file is indexed at its new path");
    // A rename that keeps the namespace (same directory): no edits.
    assert!(will_rename(&mut c, &to, &dir.join("src/accounts/users.zl")).is_none());
}

#[test]
fn renaming_a_file_of_a_shared_namespace_adds_imports() {
    let user = "namespace app.models\n\nstruct User {\n    Name string\n    R Role\n}\n\nfunc NewUser(n string) User {\n    return User{Name: n, R: Role{Name: \"admin\"}}\n}\n";
    let role = "namespace app.models\n\nstruct Role {\n    Name string\n}\n";
    let main = "namespace app\n\nimport io\nimport app.models\n\nfunc main() {\n    var u = NewUser(\"x\")\n    var r = Role{Name: \"r\"}\n    println(u.Name, r.Name)\n}\n";
    let dir = package(&[("main.zl", main), ("models/user.zl", user), ("models/role.zl", role)]);
    let main_uri = Url::from_file_path(dir.join("src/main.zl")).unwrap();
    let mut c = Client::start(&dir);
    c.open(&main_uri, main);
    assert_eq!(c.diagnostics(&main_uri), vec![]);
    let (from, to) = (dir.join("src/models/user.zl"), dir.join("src/accounts/user.zl"));
    let edit = will_rename(&mut c, &from, &to).expect("edits");
    let changes = edit.changes.as_ref().unwrap();
    let new_user = apply_edits(user, &changes[&Url::from_file_path(&from).unwrap()]);
    assert!(new_user.starts_with("namespace app.accounts\nimport app.models\n"), "{new_user}");
    let new_main = apply_edits(main, &changes[&main_uri]);
    assert!(new_main.contains("import app.models\nimport app.accounts\n"), "{new_main}");
    assert!(!changes.contains_key(&Url::from_file_path(dir.join("src/models/role.zl")).unwrap()));
    let mut disk = edit.clone();
    disk.changes.as_mut().unwrap().remove(&main_uri);
    apply_and_move(&c, &disk, &from, &to);
    c.change(&main_uri, 2, &new_main);
    assert_eq!(c.diagnostics(&main_uri), vec![]);
    let d = c.diagnostics(&Url::from_file_path(&to).unwrap());
    assert_eq!(d, vec![], "the moved file compiles: {new_user}");
}

#[test]
fn renaming_files_outside_the_layout_convention_gives_no_edits() {
    // Not a package: namespaces are free.
    let text = "namespace app.models\n\nfunc F() int {\n    return 1\n}\n";
    let (dir, uri) = project_with("f.zl", text);
    let mut c = Client::start(&dir);
    c.open(&uri, text);
    assert!(will_rename(&mut c, &dir.join("f.zl"), &dir.join("sub/f.zl")).is_none());
    // A package file whose namespace does not follow its path.
    let dir = package(&[("main.zl", "namespace app\n\nfunc main() {\n}\n"), ("models/odd.zl", "namespace app.other\n\nfunc G() int {\n    return 1\n}\n")]);
    let mut c = Client::start(&dir);
    let _ = status(&mut c);
    assert!(will_rename(&mut c, &dir.join("src/models/odd.zl"), &dir.join("src/x/odd.zl")).is_none());
}
