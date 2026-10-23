//! Inspections (lints, `compiler/src/sema/lint.rs`) as published
//! diagnostics with quick fixes, and the refactoring code actions
//! (`lsp/src/refactor.rs`).

use super::*;

const LINTED: &str = "namespace app

import io

func f() int! {
    return error(\"boom\")
}

func add(a int, b int) int {
    return a
}

func helper() {
}

func run() void! {
    try f() catch err {
    }
    var x = 1
    x = x
    if 1 == 2 {
        println(\"never\")
    }
    try f() catch err {
        try f() catch err {
            println(err.Message())
        }
        println(err.Message())
    }
    println(add(x, 2))
    return
    println(\"dead\")
}

func main() {
    try run() catch err {
        println(err.Message())
    }
}
";

fn whole(text: &str) -> Range {
    Range { start: Position { line: 0, character: 0 }, end: pos_of(text, "func main", 0, 0) }
}

/// Applies the quick fix titled `title` among the actions over the whole
/// file, returns the new text.
fn fix(c: &mut Client, uri: &Url, text: &str, title: &str) -> String {
    apply_action(c, uri, text, whole(text), title)
}

#[test]
fn lints_are_published_with_codes_tags_and_fixes() {
    let (dir, uri) = project_with("main.zl", LINTED);
    let mut c = Client::start(&dir);
    c.open(&uri, LINTED);
    let diags = c.diagnostics(&uri);
    let code = |d: &Diagnostic| match &d.code {
        Some(NumberOrString::String(s)) => s.clone(),
        _ => String::new(),
    };
    let mut codes: Vec<String> = diags.iter().map(code).collect();
    codes.sort();
    assert_eq!(codes, ["L0001", "L0002", "L0003", "L0004", "L0005", "L0006", "L0007"], "{diags:#?}");
    let by = |c: &str| diags.iter().find(|d| code(d) == c).unwrap();
    assert_eq!(by("L0006").severity, Some(DiagnosticSeverity::HINT));
    assert_eq!(by("L0001").tags, Some(vec![DiagnosticTag::UNNECESSARY]));
    assert_eq!(by("L0003").severity, Some(DiagnosticSeverity::WARNING));

    let t = fix(&mut c, &uri, LINTED, "Remove unreachable code");
    assert!(!t.contains("dead") && t.contains("    return\n}"), "{t}");
    let t = fix(&mut c, &uri, LINTED, "Propagate the error with `?`");
    assert!(t.contains("    f()?\n    var x"), "{t}");
    let t = fix(&mut c, &uri, LINTED, "Remove the self-assignment");
    assert!(!t.contains("x = x"), "{t}");
    let t = fix(&mut c, &uri, LINTED, "Replace with false");
    assert!(t.contains("if false {"), "{t}");
    let t = fix(&mut c, &uri, LINTED, "Rename b to _");
    assert!(t.contains("func add(a int, _ int) int"), "{t}");
    let t = fix(&mut c, &uri, LINTED, "Remove function helper");
    assert!(!t.contains("helper"), "{t}");
    let t = fix(&mut c, &uri, LINTED, "Rename to err2");
    assert!(t.contains("catch err2 {\n            println(err2.Message())"), "{t}");
    // Every fix leaves a program that still compiles.
    for (i, title) in ["Remove unreachable code", "Propagate the error with `?`", "Replace with false", "Rename b to _", "Remove function helper", "Rename to err2"].into_iter().enumerate() {
        let t = fix(&mut c, &uri, LINTED, title);
        c.change(&uri, 2 + 2 * i as i32, &t);
        let errs: Vec<_> = c.diagnostics(&uri).into_iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR)).collect();
        assert!(errs.is_empty(), "{title}: {errs:#?}\n{t}");
        c.change(&uri, 3 + 2 * i as i32, LINTED);
        let _ = c.diagnostics(&uri);
    }
}

#[test]
fn allow_comment_suppresses_a_lint() {
    let src = LINTED.replace("    x = x\n", "    x = x // zell:allow L0004\n");
    let (dir, uri) = project_with("main.zl", &src);
    let mut c = Client::start(&dir);
    c.open(&uri, &src);
    let diags = c.diagnostics(&uri);
    assert!(!diags.iter().any(|d| d.code == Some(NumberOrString::String("L0004".into()))), "{diags:#?}");
    assert!(diags.iter().any(|d| d.code == Some(NumberOrString::String("L0001".into()))));
}

const REFAC: &str = "namespace app

import os
import io

func sum(n int) int {
    var total = 0
    for i in 0..n {
        total += i
    }
    return total
}

func main() {
    var name = \"zell\"
    var greeting = \"hi \" + name
    println(strings.ToUpper(name), greeting)
    var n = 4
    var total = 0
    for i in 0..n {
        total += i
    }
    println(total, sum(n))
}
";

fn span_range(text: &str, from: &str, to: &str) -> Range {
    let s = text.find(from).unwrap();
    let e = text[s..].find(to).unwrap() + s + to.len();
    let p = |off: usize| Position { line: text[..off].matches('\n').count() as u32, character: (off - text[..off].rfind('\n').map_or(0, |i| i + 1)) as u32 };
    Range { start: p(s), end: p(e) }
}

/// Runs the refactoring titled `title` at `at` (resolving it when listed
/// lazily), applies its edit and checks the result compiles.
fn refactor(c: &mut Client, uri: &Url, text: &str, at: Range, title: &str, version: i32) -> String {
    let actions = code_actions(c, uri, at);
    let mut a = actions.iter().find(|a| a.title.starts_with(title)).unwrap_or_else(|| panic!("no action {title:?} in {:?}", actions.iter().map(|a| &a.title).collect::<Vec<_>>())).clone();
    if a.edit.is_none() {
        assert!(a.data.is_some(), "lazy actions carry data");
        a = c.request::<request::CodeActionResolveRequest>(a);
    }
    let edits = &a.edit.as_ref().expect("edit").changes.as_ref().unwrap()[uri];
    let out = apply_edits(text, edits);
    c.change(uri, version, &out);
    let errs: Vec<_> = c.diagnostics(uri).into_iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR)).collect();
    assert!(errs.is_empty(), "{title}: {errs:#?}\n{out}");
    c.change(uri, version + 1, text);
    let _ = c.diagnostics(uri);
    out
}

#[test]
fn refactorings_extract_inline_and_organize_imports() {
    let src = REFAC.replace("import os\n", "");
    for lazy in [false, true] {
        let (dir, uri) = project_with("main.zl", &src);
        let mut c = Client::start_with(&dir, |p| {
            if lazy {
                p.capabilities.text_document = Some(TextDocumentClientCapabilities {
                    code_action: Some(CodeActionClientCapabilities { resolve_support: Some(CodeActionCapabilityResolveSupport { properties: vec!["edit".into()] }), ..Default::default() }),
                    ..Default::default()
                });
            }
        });
        c.open(&uri, &src);
        assert_eq!(c.diagnostics(&uri), vec![]);
        let t = refactor(&mut c, &uri, &src, span_range(&src, "strings.ToUpper(name)", ")"), "Extract variable toUpper", 2);
        assert!(t.contains("    var toUpper = strings.ToUpper(name)\n    println(toUpper, greeting)"), "{t}");
        let t = refactor(&mut c, &uri, &src, span_range(&src, "greeting =", "greeting"), "Inline variable greeting", 4);
        assert!(t.contains("println(strings.ToUpper(name), (\"hi \" + name))") && !t.contains("var greeting"), "{t}");
        let t = refactor(&mut c, &uri, &src, span_range(&src, "    var total = 0\n    for i in 0..n {\n        total += i\n    }\n    println", "    }"), "Extract function extracted", 6);
        assert!(t.contains("    var total = extracted(n)\n    println(total, sum(n))"), "{t}");
        assert!(t.contains("func extracted(n int) int {\n    var total = 0\n    for i in 0..n {\n        total += i\n    }\n    return total\n}"), "{t}");
    }
    // Organize imports: sorted, the unused `os` removed (`strings.X`
    // needs no import: roots are visible).
    let (dir, uri) = project_with("main.zl", REFAC);
    let mut c = Client::start(&dir);
    c.open(&uri, REFAC);
    let _ = c.diagnostics(&uri);
    let t = refactor(&mut c, &uri, REFAC, span_range(REFAC, "func main", "main"), "Organize imports", 2);
    assert!(t.contains("namespace app\n\nimport io\n\nfunc sum"), "{t}");
}
