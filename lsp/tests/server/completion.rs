//! Completion quality: relevance order (`sortText`), the replaced range
//! (`textEdit` / `InsertReplaceEdit`), call and statement snippets only
//! for clients with `snippetSupport`, auto-import items, truncation, and
//! receivers of type `T!`, `weak *T` and constrained type parameters.

use super::*;

const SRC: &str = "namespace app

import net

interface Shape {
    func Area() float
    func Label() string
}

struct Node {
    Value int
    Other weak *Node

    func Show(prefix string, n int) string {
        return prefix
    }
}

func parse(s string) Node! {
    if s == \"\" {
        return error(\"empty\")
    }
    return Node{Value: 1}
}

func total<T: Shape>(xs []T) float {
    var sum = 0.0
    for x in xs {
        sum = sum + x.Area()
        // generic
    }
    return sum
}

async func fetch() int {
    return 1
}

async func work() {
    var v = await fetch()
    _ = v
    // async
}

func run(count int) {
    var counter = 1
    var n = Node{Value: 1}
    _ = counter
    _ = n
    // here
}

func serve(r *http.Request) {
}

func main() {
}
";

/// A client advertising snippets and insert/replace edits.
fn rich_client(dir: &Path) -> Client {
    Client::start_with(dir, |p| {
        p.capabilities.text_document = Some(TextDocumentClientCapabilities {
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability { snippet_support: Some(true), insert_replace_support: Some(true), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        });
    })
}

/// Inserts `line` after `anchor` and completes at `col` of it.
fn list_at(c: &mut Client, uri: &Url, version: i32, src: &str, anchor: &str, line: &str, col: usize) -> CompletionList {
    let edited = src.replacen(anchor, &format!("{anchor}\n{line}"), 1);
    c.change(uri, version, &edited);
    let off = src.find(anchor).unwrap() + anchor.len() + 1 + col;
    let pos = Position::new(edited[..off].matches('\n').count() as u32, (off - edited[..off].rfind('\n').map_or(0, |i| i + 1)) as u32);
    let r: Option<CompletionResponse> = c.request::<request::Completion>(CompletionParams {
        text_document_position: tdp(uri, pos),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    });
    match r {
        Some(CompletionResponse::List(l)) => l,
        other => panic!("{other:?}"),
    }
}

fn find<'a>(l: &'a CompletionList, label: &str) -> &'a CompletionItem {
    l.items.iter().find(|i| i.label == label).unwrap_or_else(|| panic!("no {label}: {:?}", labels(l)))
}

fn labels(l: &CompletionList) -> Vec<&str> {
    l.items.iter().map(|i| i.label.as_str()).collect()
}

fn new_text(i: &CompletionItem) -> (&str, Range) {
    match i.text_edit.as_ref().expect("textEdit") {
        CompletionTextEdit::Edit(e) => (&e.new_text, e.range),
        CompletionTextEdit::InsertAndReplace(e) => (&e.new_text, e.insert),
    }
}

const HERE: &str = "    // here";

fn here_line() -> u32 {
    SRC[..SRC.find(HERE).unwrap()].matches('\n').count() as u32
}

#[test]
fn completion_ranks_and_replaced_range() {
    let (dir, uri) = project_with("main.zl", SRC);
    let mut c = Client::start(&dir);
    c.open(&uri, SRC);
    // locals > params > keywords, and sortText follows the list order.
    let l = list_at(&mut c, &uri, 2, SRC, HERE, "    co", 6);
    let sort = |label: &str| find(&l, label).sort_text.clone().unwrap();
    assert!(sort("counter") < sort("count") && sort("count") < sort("const"), "{:?}", labels(&l));
    let sorted: Vec<String> = l.items.iter().map(|i| i.sort_text.clone().unwrap()).collect();
    assert!(sorted.windows(2).all(|w| w[0] < w[1]));
    assert!(!l.is_incomplete);
    // The edit replaces the typed prefix only (plain TextEdit client).
    let line = here_line() + 1;
    let (text, range) = new_text(find(&l, "counter"));
    assert_eq!((text, range), ("counter", Range::new(Position::new(line, 4), Position::new(line, 6))));
    assert!(find(&l, "counter").insert_text_format != Some(InsertTextFormat::SNIPPET));
    // Members: fields before methods.
    let l = list_at(&mut c, &uri, 3, SRC, HERE, "    n.", 6);
    assert_eq!(labels(&l), ["Value", "Other", "Show"]);
    // Plain client: calls and keywords are plain text.
    let (text, _) = new_text(find(&l, "Show"));
    assert_eq!(text, "Show");
    let l = list_at(&mut c, &uri, 4, SRC, HERE, "    i", 5);
    assert_eq!(new_text(find(&l, "if")).0, "if");
}

#[test]
fn completion_snippets_and_insert_replace_with_capable_client() {
    let (dir, uri) = project_with("main.zl", SRC);
    let mut c = rich_client(&dir);
    c.open(&uri, SRC);
    // A prefix in the middle of a word: insert covers the prefix, replace the word.
    let l = list_at(&mut c, &uri, 2, SRC, HERE, "    cozz", 6);
    let line = here_line() + 1;
    match find(&l, "count").text_edit.as_ref().unwrap() {
        CompletionTextEdit::InsertAndReplace(e) => {
            assert_eq!(e.insert, Range::new(Position::new(line, 4), Position::new(line, 6)));
            assert_eq!(e.replace, Range::new(Position::new(line, 4), Position::new(line, 8)));
        }
        other => panic!("{other:?}"),
    }
    // Call snippets with parameter placeholders; `$0` after a no-arg call.
    let l = list_at(&mut c, &uri, 3, SRC, HERE, "    n.Sh", 8);
    let show = find(&l, "Show");
    assert_eq!(new_text(show).0, "Show(${1:prefix}, ${2:n})");
    assert_eq!(show.insert_text_format, Some(InsertTextFormat::SNIPPET));
    let l = list_at(&mut c, &uri, 4, SRC, HERE, "    mai", 7);
    assert_eq!(new_text(find(&l, "main")).0, "main()$0");
    // No snippet when the parentheses are already there.
    let l = list_at(&mut c, &uri, 5, SRC, HERE, "    mai()", 7);
    assert_eq!(new_text(find(&l, "main")).0, "main");
    // Statement snippets at the start of a statement.
    let l = list_at(&mut c, &uri, 6, SRC, HERE, "    i", 5);
    assert_eq!(new_text(find(&l, "if")).0, "if ${1:cond} {\n        $0\n    }");
    let l = list_at(&mut c, &uri, 7, SRC, HERE, "    tr", 6);
    assert!(new_text(find(&l, "try")).0.starts_with("try {\n"));
    assert!(!labels(&l).contains(&"defer await"));
    let l = list_at(&mut c, &uri, 8, SRC, "    // async", "    def", 7);
    assert_eq!(new_text(find(&l, "defer await")).0, "defer await $0");
    // Mid-expression: no statement keywords.
    let l = list_at(&mut c, &uri, 9, SRC, HERE, "    var y = fo", 14);
    assert!(!labels(&l).contains(&"for"), "{:?}", labels(&l));
    // Top level: declaration keywords only.
    let l = list_at(&mut c, &uri, 10, SRC, "func main() {\n}", "str", 3);
    assert!(labels(&l).contains(&"struct") && !labels(&l).contains(&"return"), "{:?}", labels(&l));
}

#[test]
fn completion_auto_import_and_truncation() {
    let (dir, uri) = project_with("main.zl", SRC);
    let mut c = Client::start(&dir);
    c.open(&uri, SRC);
    let l = list_at(&mut c, &uri, 2, SRC, HERE, "    ContainsAn", 14);
    let it = l.items.iter().find(|i| i.label == "ContainsAny" && i.label_details.as_ref().and_then(|d| d.description.as_deref()) == Some("import strings")).unwrap_or_else(|| panic!("{:?}", labels(&l)));
    let edits = it.additional_text_edits.as_ref().expect("import edit");
    // After `import net` (line 2), in sorted order.
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "import strings\n");
    assert_eq!(edits[0].range, Range::new(Position::new(3, 0), Position::new(3, 0)));
    // Names that are already visible get no auto-import duplicate.
    let l = list_at(&mut c, &uri, 3, SRC, HERE, "    Nod", 7);
    assert_eq!(l.items.iter().filter(|i| i.label == "Node").count(), 1);
    assert!(find(&l, "Node").additional_text_edits.is_none());

    // 250 locals: the list is cut to 200 and marked incomplete.
    let decls: String = (0..250).map(|i| format!("    var v{i} = {i}\n    _ = v{i}\n")).collect();
    let big = SRC.replacen(HERE, &format!("{decls}{HERE}"), 1);
    c.change(&uri, 4, &big);
    let l = list_at(&mut c, &uri, 5, &big, HERE, "    v", 5);
    assert!(l.is_incomplete);
    assert_eq!(l.items.len(), 200);
    // Nearest declaration first.
    assert_eq!(l.items[0].label, "v249");
    let l = list_at(&mut c, &uri, 6, &big, HERE, "    v24", 7);
    assert!(!l.is_incomplete);
}

#[test]
fn completion_on_fallible_weak_and_type_parameter_receivers() {
    let (dir, uri) = project_with("main.zl", SRC);
    let mut c = Client::start(&dir);
    c.open(&uri, SRC);
    let errors: Vec<_> = c.diagnostics(&uri).into_iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR)).collect();
    assert!(errors.is_empty(), "{errors:?}");
    let node = ["Value", "Other", "Show"];
    let cases: &[(&str, &str, &[&str])] = &[
        (HERE, "    parse(\"x\").", &node),
        (HERE, "    n.Other.", &node),
        ("        // generic", "        x.", &["Area", "Label"]),
    ];
    for (i, (anchor, line, want)) in cases.iter().enumerate() {
        let l = list_at(&mut c, &uri, 2 + i as i32, SRC, anchor, line, line.len());
        assert_eq!(labels(&l), *want, "{line}");
    }
}
