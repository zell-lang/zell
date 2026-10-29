//! Change signature (`lsp/src/signature.rs`): the parameter-free code
//! actions and the `zell.changeSignature` command.

use super::inline::Ws;
use super::inspect::span_range;
use super::*;

const MAIN: &str = "namespace app

import io

interface Shape {
    func Scale(f int, label string) int
}

struct Sq {
    S int

    func Scale(f int, label string) int {
        return this.S * f
    }
}

struct Ci {
    R int

    func Scale(k int, name string) int {
        return this.R + k
    }
}

func area(w int, h int, unused string) int {
    return w * h
}

func fact(n int, acc int) int {
    if n < 2 {
        return acc
    }
    return fact(n - 1, acc * n)
}

func next() int {
    return 1
}

func tag(n int, s string) string {
    return s
}

func pair(a int, b int) int {
    return a - b
}

func useShape(s Shape) int {
    return s.Scale(2, \"x\")
}

func sum(base int, xs ...int) int {
    var t = base
    for x in xs {
        t += x
    }
    return t
}

func main() {
    var total = 3
    var sq = Sq{S: 2}
    io.println(area(2, 3, \"a\"), area(next(), total, \"b\"), tag(next(), \"t\"), pair(next(), next()))
    io.println(fact(5, 1), useShape(sq), sq.Scale(3, \"y\"), Ci{R: 1}.Scale(1, \"z\"))
    io.println(sum(1, 2, 3), sum(4))
    var f = next
    io.println(f())
}
";

const B: &str = "namespace app

import io

func more() {
    io.println(area(4, 5, \"c\"))
}
";

fn ws() -> Ws {
    Ws::start(&[("main.zl", MAIN), ("b.zl", B)])
}

#[test]
fn change_signature_actions() {
    let mut w = ws();
    // Move: declaration, every call (in both files); one argument with
    // side effects may move past pure ones.
    let t = w.apply(0, "area(w", "area", "Change signature: move parameter w right");
    assert!(t[0].contains("func area(h int, w int, unused string) int {") && t[0].contains("area(3, 2, \"a\"), area(total, next(), \"b\")"), "{}", t[0]);
    assert!(t[1].contains("io.println(area(5, 4, \"c\"))"), "{}", t[1]);
    // Remove an unused parameter; from a call site.
    let t = w.apply(1, "area(4", "area", "Change signature: remove parameter unused");
    assert!(t[0].contains("func area(w int, h int) int {") && t[0].contains("area(2, 3), area(next(), total)") && t[1].contains("area(4, 5)"), "{}", t[0]);
    // Add: `param int`, calls pass 0.
    let t = w.apply(0, "area(w", "area", "Change signature: add parameter");
    assert!(t[0].contains("func area(w int, h int, unused string, param int) int {") && t[0].contains("area(2, 3, \"a\", 0)") && t[1].contains("area(4, 5, \"c\", 0)"), "{}", t[0]);
    // Recursive calls nest: inner arguments are moved too.
    let t = w.apply(0, "fact(n int", "fact", "Change signature: move parameter n right");
    assert!(t[0].contains("func fact(acc int, n int) int {") && t[0].contains("return fact(acc * n, n - 1)") && t[0].contains("fact(1, 5)"), "{}", t[0]);
    // Variadic: a new parameter goes before it.
    let t = w.apply(0, "sum(base", "sum", "Change signature: add parameter");
    assert!(t[0].contains("func sum(base int, param int, xs ...int) int {") && t[0].contains("sum(1, 0, 2, 3), sum(4, 0)"), "{}", t[0]);
}

#[test]
fn change_signature_interface_family() {
    let mut w = ws();
    // From an implementation: the interface, the other implementation
    // and calls through the interface and on each type change.
    let t = &w.apply(0, "Scale(f int, label string) int {", "Scale", "Change signature: remove parameter label")[0];
    assert!(t.contains("    func Scale(f int) int\n}"), "{t}");
    assert!(t.contains("    func Scale(f int) int {\n        return this.S * f") && t.contains("    func Scale(k int) int {\n        return this.R + k"), "{t}");
    assert!(t.contains("return s.Scale(2)\n") && t.contains("sq.Scale(3), Ci{R: 1}.Scale(1)"), "{t}");
}

#[test]
fn change_signature_refusals() {
    let mut w = ws();
    for (from, to, title, why) in [
        ("area(w", "area", "Change signature: remove parameter w", "parameter `w` is used in `area`"),
        ("tag(n", "tag", "Change signature: remove parameter n", "side effects"),
        ("pair(a", "pair", "Change signature: move parameter a right", "another order"),
        ("next() int", "next", "Change signature: add parameter", "function value"),
        ("sum(base", "sum", "Change signature: move parameter base right", "must stay last"),
    ] {
        let r = w.refused(0, from, to, title);
        assert!(r.contains(why), "{title}: {r}");
    }
}

#[test]
fn change_signature_command() {
    let mut w = ws();
    assert!(w.c.capabilities.execute_command_provider.as_ref().is_some_and(|o| o.commands.contains(&"zell.changeSignature".to_string())));
    let (uri, text) = w.docs[0].clone();
    let pos = span_range(&text, "area(w", "area").start;
    let params = |p: serde_json::Value| ExecuteCommandParams {
        command: "zell.changeSignature".into(),
        arguments: vec![serde_json::json!({ "uri": uri, "position": pos, "params": p })],
        work_done_progress_params: Default::default(),
    };
    // Rename, reorder, remove and add in one go.
    let r = w.c.request::<request::ExecuteCommand>(params(serde_json::json!([{ "from": 1, "name": "height" }, { "from": 0 }, { "name": "scale", "type": "int", "default": "1" }])));
    let edit: WorkspaceEdit = serde_json::from_value(r.expect("edit")).unwrap();
    let t = w.apply_edit(&edit, "changeSignature");
    assert!(t[0].contains("func area(height int, w int, scale int) int {\n    return w * height\n}") && t[0].contains("area(3, 2, 1), area(total, next(), 1)"), "{}", t[0]);
    assert!(t[1].contains("area(5, 4, 1)"), "{}", t[1]);
    // Refusals are errors with the reason.
    let m = w.c.request_err::<request::ExecuteCommand>(params(serde_json::json!([{ "from": 1 }])));
    assert!(m.contains("parameter `w` is used"), "{m}");
    let m = w.c.request_err::<request::ExecuteCommand>(params(serde_json::json!([{ "from": 0, "name": "h" }, { "from": 1 }, { "from": 2 }])));
    assert!(m.contains("already used"), "{m}");
}
