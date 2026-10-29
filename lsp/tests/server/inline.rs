//! Workspace refactorings: inline function (`lsp/src/inline_fn.rs`) and
//! change signature (`lsp/src/signature.rs`).

use super::inspect::span_range;
use super::*;
use std::collections::HashMap;

/// A started server on a project of open `files` (name, text), the
/// client listing refactorings lazily and disabled ones with a reason.
pub(super) struct Ws {
    pub c: Client,
    pub docs: Vec<(Url, String)>,
    version: i32,
}

impl Ws {
    pub fn start(files: &[(&str, &str)]) -> Ws {
        let fmt: Vec<(String, String)> = files.iter().map(|(n, t)| (n.to_string(), zell::fmt::format_source(t).unwrap())).collect();
        let (dir, uri) = project_with(&fmt[0].0, &fmt[0].1);
        let mut docs = vec![(uri, fmt[0].1.clone())];
        for (name, text) in &fmt[1..] {
            std::fs::write(dir.join(name), text).unwrap();
            docs.push((Url::from_file_path(dir.join(name)).unwrap(), text.clone()));
        }
        let mut c = Client::start_with(&dir, |p| {
            p.capabilities.text_document = Some(TextDocumentClientCapabilities {
                code_action: Some(CodeActionClientCapabilities {
                    resolve_support: Some(CodeActionCapabilityResolveSupport { properties: vec!["edit".into()] }),
                    disabled_support: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
        });
        for (u, t) in &docs {
            c.open(u, t);
        }
        for (u, _) in &docs {
            let errs: Vec<_> = c.diagnostics(u).into_iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR)).collect();
            assert!(errs.is_empty(), "{u}: {errs:#?}");
        }
        Ws { c, docs, version: 1 }
    }

    /// The action titled `title` (prefix) at `from`..`to` of file `i`,
    /// resolved.
    pub fn action(&mut self, i: usize, from: &str, to: &str, title: &str) -> CodeAction {
        let (uri, text) = self.docs[i].clone();
        let actions = code_actions(&mut self.c, &uri, span_range(&text, from, to));
        let a = actions.iter().find(|a| a.title.starts_with(title)).unwrap_or_else(|| panic!("no action {title:?} in {:?}", actions.iter().map(|a| &a.title).collect::<Vec<_>>())).clone();
        if a.disabled.is_some() || a.edit.is_some() {
            return a;
        }
        self.c.request::<request::CodeActionResolveRequest>(a)
    }

    /// The reason the action is refused.
    pub fn refused(&mut self, i: usize, from: &str, to: &str, title: &str) -> String {
        let a = self.action(i, from, to, title);
        a.disabled.unwrap_or_else(|| panic!("{title} is not refused: {:?}", a.edit)).reason
    }

    /// Applies the action, checks every file compiles and is formatted,
    /// returns the new texts and restores the old ones.
    pub fn apply(&mut self, i: usize, from: &str, to: &str, title: &str) -> Vec<String> {
        let a = self.action(i, from, to, title);
        assert!(a.disabled.is_none(), "{title}: {:?}", a.disabled);
        self.apply_edit(&a.edit.expect("edit"), title)
    }

    pub fn apply_edit(&mut self, edit: &WorkspaceEdit, what: &str) -> Vec<String> {
        let mut changes: HashMap<Url, Vec<TextEdit>> = edit.changes.clone().unwrap_or_default();
        if let Some(DocumentChanges::Edits(es)) = &edit.document_changes {
            for e in es {
                changes.entry(e.text_document.uri.clone()).or_default().extend(e.edits.iter().map(|x| match x {
                    OneOf::Left(t) => t.clone(),
                    OneOf::Right(t) => t.text_edit.clone(),
                }));
            }
        }
        let new: Vec<String> = self.docs.iter().map(|(u, t)| changes.get(u).map_or_else(|| t.clone(), |e| apply_edits(t, e))).collect();
        self.version += 1;
        for ((u, _), t) in self.docs.iter().zip(&new) {
            self.c.change(u, self.version, t);
        }
        for ((u, _), t) in self.docs.clone().iter().zip(&new) {
            let errs: Vec<_> = self.c.diagnostics(u).into_iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR)).collect();
            assert!(errs.is_empty(), "{what}: {errs:#?}\n{t}");
            assert_eq!(&zell::fmt::format_source(t).unwrap(), t, "{what}: not formatted");
        }
        self.version += 1;
        for (u, t) in &self.docs {
            self.c.change(u, self.version, t);
        }
        for (u, _) in self.docs.clone() {
            let _ = self.c.diagnostics(&u);
        }
        new
    }
}

const MAIN: &str = "namespace app

import io
import app.util

struct Counter {
    N int

    func Add(k int) int {
        this.N += k
        return this.N
    }

    func Double() int {
        return this.N * 2
    }
}

func square(x int) int {
    return x * x
}

func greet(name string, times int) {
    var msg = \"hi \" + name
    for i in 0..times {
        io.println(msg, i)
    }
}

func sum3(a int, b int, c int) int {
    var total = a + b
    total += c
    return total
}

func bump(n int) int {
    n += 1
    return n
}

func early(n int) int {
    if n > 0 {
        return 1
    }
    return 0
}

func withDefer() {
    defer io.println(\"bye\")
    io.println(\"hi\")
}

func fact(n int) int {
    if n < 2 {
        return 1
    }
    return n * fact(n - 1)
}

func parse(s string) int! {
    var n = ToInt(s)?
    return n + 1
}

async func load(n int) int {
    return n + 1
}

async func run() int {
    var v = await load(2)
    var t = load(3)
    return v + await t
}

func first<T>(xs []T) T {
    var x T = xs[0]
    return x
}

func next() int {
    return 7
}

func newCounter() Counter {
    return Counter{N: 5}
}

func check(n int) bool {
    return n > 0
}

interface Doubler {
    func Double() int
}

func viaIface(d Doubler) int {
    return d.Double()
}

func countdown(n int) {
    if n > 0 {
        countdown(n - 1)
    }
}

func tw(n int) int {
    return next() + n
}

func shadow() int {
    var next = 2
    return tw(next)
}

func main() void! {
    var total = 1
    var c = Counter{N: 1}
    io.println(square(total + 1) + 1)
    greet(\"bob\", 2)
    var s = sum3(1, next(), total)
    io.println(s, bump(total), early(total), fact(3))
    withDefer()
    var p = parse(\"4\")?
    io.println(p, c.Double(), c.Add(2), newCounter().Double())
    io.println(first<int>([1, 2]))
    var f = square
    io.println(f(2), total > 0 && check(next()))
    for square(total) < 3 {
        total += 1
    }
    var cs = [Counter{N: 2}]
    var q = try parse(\"x\") else 0
    io.println(cs[0].Double(), q, viaIface(c))
    countdown(2)
}
";

const UTIL: &str = "namespace app.util

func ToInt(s string) int! {
    if s == \"\" {
        return error(\"empty\")
    }
    return len(s)
}

func Twice(n int) int {
    return n * 2
}

func Pub(n int) int {
    return Twice(n) + 1
}

func Hidden(n int) int {
    return helper(n)
}

func helper(n int) int {
    return n
}
";

const OTHER: &str = "namespace app.other

import io

func Use() {
    io.println(app.util.Pub(3), app.util.Hidden(1))
}
";

fn ws() -> Ws {
    Ws::start(&[("main.zl", MAIN), ("util.zl", UTIL), ("other.zl", OTHER)])
}

#[test]
fn inline_single_expression_and_statements() {
    let mut w = ws();
    // One `return e`: in an expression, parenthesized; the argument is
    // not trivial and used twice: bound to a temporary (renamed: `x`
    // occurs in `main`).
    let t = &w.apply(0, "square(total + 1)", "square", "Inline call to square")[0];
    assert!(t.contains("    var x2 = total + 1\n    io.println((x2 * x2) + 1)\n"), "{t}");
    // Several statements at a call statement; locals renamed when the
    // caller uses the name (`total`? no: `msg` is free, `i` is free).
    let t = &w.apply(0, "greet(\"bob\"", "greet", "Inline call to greet")[0];
    assert!(t.contains("    var msg = \"hi \" + \"bob\"\n    for i in 0..2 {\n        io.println(msg, i)\n    }\n    var s = sum3"), "{t}");
    // `var v = f(...)`: the local `total` clashes with the caller's.
    let t = &w.apply(0, "sum3(1", "sum3", "Inline call to sum3")[0];
    assert!(t.contains("    var b = next()\n    var total2 = 1 + b\n    total2 += total\n    var s = total2\n"), "{t}");
    // Several statements in an expression.
    let r = w.refused(0, "bump(total)", "bump", "Inline call to bump");
    assert!(r.contains("several statements"), "{r}");
}

#[test]
fn inline_methods_async_fallible_and_imports() {
    let mut w = ws();
    // `this` becomes the receiver name; a call result is bound first.
    let t = &w.apply(0, "Double(), c.Add", "Double", "Inline call to Double")[0];
    assert!(t.contains("io.println(p, c.N * 2, c.Add(2), newCounter().Double())"), "{t}");
    let t = &w.apply(0, "Double())\n", "Double", "Inline call to Double")[0];
    assert!(t.contains("    var recv = newCounter()\n    io.println(p, c.Double(), c.Add(2), recv.N * 2)\n"), "{t}");
    // `?` in the body at a propagated call; `await` of an async call.
    let t = &w.apply(0, "parse(\"4\")", "parse", "Inline call to parse")[0];
    assert!(t.contains("    var n = ToInt(\"4\")?\n    var p = n + 1\n"), "{t}");
    let t = &w.apply(0, "load(2)", "load", "Inline call to load")[0];
    assert!(t.contains("    var v = 2 + 1\n"), "{t}");
    // Another namespace: the body's `Twice` needs `import app.util`.
    let t = &w.apply(2, "Pub(3)", "Pub", "Inline call to Pub")[2];
    assert!(t.contains("import io\n\nimport app.util\n") && t.contains("io.println(Twice(3) + 1, app.util.Hidden(1))"), "{t}");
    // A loop condition with trivial arguments needs no temporary.
    let t = &w.apply(0, "square(total) < 3", "square", "Inline call to square")[0];
    assert!(t.contains("    for (total * total) < 3 {\n"), "{t}");
}

#[test]
fn inline_all_calls() {
    let mut w = ws();
    // Every use is a call: the declaration goes.
    let t = &w.apply(0, "next() int {", "next", "Inline function next")[0];
    assert!(!t.contains("func next") && t.contains("var s = sum3(1, 7, total)") && t.contains("check(7)") && t.contains("return 7 + n"), "{t}");
    let t = &w.apply(0, "sum3(a", "sum3", "Inline function sum3")[0];
    assert!(!t.contains("func sum3") && t.contains("    var b = next()\n    var total2 = 1 + b\n    total2 += total\n    var s = total2\n"), "{t}");
    let t = &w.apply(1, "Twice(n int)", "Twice", "Inline function Twice")[1];
    assert!(!t.contains("func Twice") && t.contains("    return (n * 2) + 1\n"), "{t}");
    // `square` is also used as a value: inlined at the calls, kept.
    let t = &w.apply(0, "square(x int)", "square", "Inline function square")[0];
    assert!(t.contains("func square") && t.contains("    var x2 = total + 1\n    io.println((x2 * x2) + 1)\n") && t.contains("for (total * total) < 3"), "{t}");
}

#[test]
fn inline_refusals() {
    let mut w = ws();
    for (f, from, to, title, why) in [
        (0, "Add(2)", "Add", "Inline call to Add", "several statements"),
        (0, "bump(total)", "bump", "Inline call to bump", "several statements"),
        (0, "check(next())", "check", "Inline call to check", "right of `&&`/`||`"),
        (0, "early(total)", "early", "Inline call to early", "returns early"),
        (0, "withDefer()\n    var p", "withDefer", "Inline call to withDefer", "uses `defer`"),
        (0, "countdown(2)", "countdown", "Inline call to countdown", "recursive"),
        (0, "load(3)", "load", "Inline call to load", "Task value"),
        (0, "parse(\"x\")", "parse", "Inline call to parse", "propagated with `?`"),
        (0, "f(2)", "f", "Inline call to f", "function value"),
        (0, "Double(), q", "Double", "Inline call to Double", "receiver `cs[0]`"),
        (0, "Double()\n}\n\nfunc countdown", "Double", "Inline call to Double", "through an interface"),
        (0, "tw(next)", "tw", "Inline call to tw", "hidden by a local"),
        (2, "Hidden(1)", "Hidden", "Inline call to Hidden", "private to namespace app.util"),
    ] {
        let r = w.refused(f, from, to, title);
        assert!(r.contains(why), "{from}: {r}");
    }
}
