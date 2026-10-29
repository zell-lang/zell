//! Compiler intrinsics declared as `@intrinsic` stubs in the std sources
//! (`t.Must`, `t.MustFail`, `Task.Cancel`): completion, hover and
//! go-to-definition see them as ordinary members.

use super::*;

const SRC: &str = "namespace app

import testing

async func work() int {
    return 1
}

@test func TestX(t *testing.T) {
    var w = work()
    w.Cancel()
    t.Must(parse())
}

func parse() int! {
    return 1
}
";

fn complete_at(c: &mut Client, uri: &Url, pos: Position) -> Vec<String> {
    let r: Option<CompletionResponse> = c.request::<request::Completion>(CompletionParams {
        text_document_position: tdp(uri, pos),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    });
    let items = match r {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(l)) => l.items,
        None => Vec::new(),
    };
    items.into_iter().map(|i| i.label).collect()
}

#[test]
fn intrinsics_complete_hover_and_resolve() {
    let (dir, uri) = project_with("x_test.zl", SRC);
    let mut c = Client::start(&dir);
    c.open(&uri, SRC);
    assert!(c.diagnostics(&uri).is_empty(), "{:?}", c.diagnostics(&uri));
    // `t.` on a testing.T lists Must and MustFail (and not the stub helpers).
    let anchor = "    t.Must(parse())\n";
    let edited = SRC.replacen(anchor, &format!("{anchor}    t.\n"), 1);
    c.change(&uri, 2, &edited);
    let labels = complete_at(&mut c, &uri, pos_of(&edited, "    t.\n", 0, 6));
    assert!(labels.iter().any(|l| l == "Must") && labels.iter().any(|l| l == "MustFail") && labels.iter().any(|l| l == "Fatal"), "{labels:?}");
    assert!(!labels.iter().any(|l| l == "mustError"), "{labels:?}");
    // `w.` on a Task<int> lists Cancel.
    let anchor = "    w.Cancel()\n";
    let edited = SRC.replacen(anchor, &format!("{anchor}    w.\n"), 1);
    c.change(&uri, 3, &edited);
    let labels = complete_at(&mut c, &uri, pos_of(&edited, "    w.\n", 0, 6));
    assert!(labels.iter().any(|l| l == "Cancel"), "{labels:?}");
    c.change(&uri, 4, SRC);
    // Hover shows the stub's signature and doc comment.
    let h = hover_text(&mut c, &uri, pos_of(SRC, "Must(parse", 0, 1));
    assert!(h.contains("Must") && h.contains("ends the test"), "{h}");
    let h = hover_text(&mut c, &uri, pos_of(SRC, "Cancel()", 0, 1));
    assert!(h.contains("Cancel") && h.contains("descendants"), "{h}");
    // Go-to-definition lands on the stubs in the std sources.
    let loc = definition(&mut c, &uri, pos_of(SRC, "Must(parse", 0, 1));
    assert!(loc.uri.path().ends_with("std/testing/testing.zl"), "{loc:?}");
    let loc = definition(&mut c, &uri, pos_of(SRC, "Cancel()", 0, 1));
    assert!(loc.uri.path().ends_with("std/task/task.zl"), "{loc:?}");
}

#[test]
fn std_struct_methods_hover_and_complete() {
    // Ordinary methods of std structs: recorded by path, declared from std/.
    let src = "namespace app\n\nimport testing\n\n@test func TestX(t *testing.T) {\n    t.Fatal(\"x\")\n}\n";
    let (dir, uri) = project_with("y_test.zl", src);
    let mut c = Client::start(&dir);
    c.open(&uri, src);
    let h = hover_text(&mut c, &uri, pos_of(src, "Fatal", 0, 1));
    assert!(h.contains("func Fatal(msg string)") && h.contains("of `testing.T`") && h.contains("logs msg"), "{h}");
    let loc = definition(&mut c, &uri, pos_of(src, "Fatal", 0, 1));
    assert!(loc.uri.path().ends_with("std/testing/testing.zl"), "{loc:?}");
}
