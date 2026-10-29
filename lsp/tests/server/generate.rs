//! Code-generating refactorings: fill struct fields, implement interface
//! (`lsp/src/generate.rs`) and `?` <-> try/catch (`lsp/src/errconv.rs`).

use super::inspect::{refactor, span_range};
use super::*;

/// A started server on a project of `files` (name, text); the uri of the
/// first one.
fn start(files: &[(&str, &str)]) -> (Client, Url) {
    let (dir, uri) = project_with(files[0].0, files[0].1);
    for (name, text) in &files[1..] {
        std::fs::write(dir.join(name), text).unwrap();
    }
    let mut c = Client::start(&dir);
    c.open(&uri, files[0].1);
    let _ = c.diagnostics(&uri);
    (c, uri)
}

fn fmt(src: &str) -> String {
    zell::fmt::format_source(src).unwrap()
}

fn titles(c: &mut Client, uri: &Url, at: Range) -> Vec<String> {
    code_actions(c, uri, at).into_iter().map(|a| a.title).collect()
}

fn assert_formatted(t: &str) {
    assert_eq!(zell::fmt::format_source(t).unwrap(), t, "not formatted");
}

const GEO: &str = "namespace app.geo

struct Pos {
    X int
    Y int
}

enum Kind {
    Point
    Area
}

struct Place {
    Name string
    At Pos
    Kind Kind
    Port int = 80
    secret string
}
";

const FILL: &str = "namespace app

import io

struct Point {
    X int
    Y float
    Label string
    on bool
    Tags []string
    Next *Point
    C char
}

struct Wide {
    Alpha int
    Bravo string
    Charlie float
    Delta bool
    Echo []string
    Foxtrot map[string]int
    Golf int
    Hotel string
}

func main() {
    var p = Point{X: 1}
    var q = Point{}
    var full = Point{X: 1, Y: 2.0, Label: \"a\", on: true, Tags: nil, Next: nil, C: 'c'}
    var pl = app.geo.Place{}
    var w = Wide{Alpha: 1}
    io.println(p.X, q.X, full.X, pl.Name, w.Alpha)
}
";

#[test]
fn fill_struct_fields() {
    let src = &fmt(FILL);
    let (mut c, uri) = start(&[("main.zl", src), ("geo.zl", GEO)]);
    let t = refactor(&mut c, &uri, src, span_range(src, "Point{X: 1}", "{"), "Fill missing fields", 2);
    assert!(t.contains("    var p = Point{X: 1, Y: 0.0, Label: \"\", on: false, Tags: nil, Next: nil, C: '\\0'}\n"), "{t}");
    assert_formatted(&t);
    let t = refactor(&mut c, &uri, src, span_range(src, "Point{}", "{"), "Fill missing fields", 4);
    assert!(t.contains("    var q = Point{X: 0, Y: 0.0, Label: \"\", on: false, Tags: nil, Next: nil, C: '\\0'}\n"), "{t}");
    // Another namespace: exported fields only, no defaulted ones, nested
    // types imported; too long for one line.
    let t = refactor(&mut c, &uri, src, span_range(src, "Place{}", "{"), "Fill missing fields", 6);
    assert!(t.contains("import io\n\nimport app.geo\n"), "{t}");
    assert!(t.contains("Name: \"\"") && t.contains("At: Pos{}") && t.contains("Kind: Kind.Point") && !t.contains("Port:") && !t.contains("secret:"), "{t}");
    assert_formatted(&t);
    let t = refactor(&mut c, &uri, src, span_range(src, "Wide{Alpha", "{"), "Fill missing fields", 8);
    assert!(t.contains("    var w = Wide{\n        Alpha: 1,\n        Bravo: \"\",\n        Charlie: 0.0,\n"), "{t}");
    assert!(t.contains("        Hotel: \"\",\n    }\n"), "{t}");
    assert_formatted(&t);
    // Negative: every field set, or not in a literal.
    let at = span_range(src, "Point{X: 1, Y", "{");
    assert!(!titles(&mut c, &uri, at).iter().any(|t| t.starts_with("Fill")));
    let at = span_range(src, "func main", "main");
    assert!(!titles(&mut c, &uri, at).iter().any(|t| t.starts_with("Fill")));
}

const SHAPES: &str = "namespace app.shapes

struct Unit {
    N int
}

interface Shape {
    func Area() float
    func Name(prefix string, n int) string!
    async func Load(u *Unit) []int
}
";

const IMPL: &str = "namespace app

import io
import app.shapes

struct Sq implements Shape {
    S float

    func Area() float {
        return this.S
    }
}

struct Ci {
    R float

    func Area() float {
        return this.R
    }
}

func use(s Shape) {
    io.println(s.Area())
}

func main() {
    use(Ci{R: 1.0})
    use(Sq{S: 2.0})
}
";

#[test]
fn implement_interface() {
    let src = &fmt(IMPL);
    let (mut c, uri) = start(&[("main.zl", src), ("shapes.zl", SHAPES)]);
    let actions = code_actions(&mut c, &uri, span_range(src, "struct Sq", "Sq"));
    let a = actions.iter().find(|a| a.title == "Implement Shape").expect("implement action");
    assert_eq!(a.kind, Some(CodeActionKind::QUICKFIX));
    let edits = &a.edit.as_ref().unwrap().changes.as_ref().unwrap()[&uri];
    let t = apply_edits(src, edits);
    assert!(t.contains("    func Area() float {\n        return this.S\n    }\n\n    func Name(prefix string, n int) string! {\n        panic(\"not implemented\")\n    }\n\n    async func Load(u *Unit) []int {\n        panic(\"not implemented\")\n    }\n}"), "{t}");
    assert_formatted(&t);
    // From the use site's error and from the struct header.
    let src = &t;
    c.change(&uri, 2, src);
    let _ = c.diagnostics(&uri);
    assert!(titles(&mut c, &uri, span_range(src, "struct Ci", "Ci")).contains(&"Implement Shape".to_string()));
    let t = refactor(&mut c, &uri, src, span_range(src, "Ci{R: 1.0}", "}"), "Implement Shape", 3);
    assert!(t.contains("        return this.R\n    }\n\n    func Name(prefix string, n int) string! {"), "{t}");
    // Negative: Sq now satisfies Shape.
    let sq = titles(&mut c, &uri, span_range(src, "struct Sq", "Sq"));
    assert!(!sq.iter().any(|t| t.starts_with("Implement")), "{sq:?}");
}

const IMPL_ANY: &str = "namespace app

import io

interface Named {
    func Name() string
}

struct Dog {
    func Name() string {
        return \"dog\"
    }
}

struct Box {
    N int
}

func main() {
    var d = Dog{}
    var b = Box{}
    io.println(d.Name(), b.N)
}
";

#[test]
fn implement_any_visible_interface() {
    let src = &fmt(IMPL_ANY);
    let (mut c, uri) = start(&[("main.zl", src), ("shapes.zl", SHAPES)]);
    let t = refactor(&mut c, &uri, src, span_range(src, "struct Box", "Box"), "Implement interface Named", 2);
    assert!(t.contains("struct Box {\n    N int\n\n    func Name() string {\n        panic(\"not implemented\")\n    }\n}"), "{t}");
    assert_formatted(&t);
    // Negative: Dog already satisfies Named; Shape's namespace is not
    // imported; nothing on a function.
    let dog = titles(&mut c, &uri, span_range(src, "struct Dog", "Dog"));
    assert!(!dog.iter().any(|t| t.starts_with("Implement")), "{dog:?}");
    let bx = titles(&mut c, &uri, span_range(src, "struct Box", "Box"));
    assert_eq!(bx.iter().filter(|t| t.starts_with("Implement")).collect::<Vec<_>>(), ["Implement interface Named"]);
    let main = titles(&mut c, &uri, span_range(src, "func main", "main"));
    assert!(!main.iter().any(|t| t.starts_with("Implement")), "{main:?}");
}

const ERRS: &str = "namespace app

import io

func f(n int) int! {
    if n > 0 {
        return error(\"boom\")
    }
    return n
}

func viaQ() int! {
    var v = f(1)?
    f(2)?
    return v
}

func viaTry() int! {
    var w = try f(1) catch err {
        return err
    }
    return w + await2()
}

async func ag() int! {
    return 1
}

async func viaAwait() int! {
    var x = try await ag() catch err {
        return err
    }
    return x
}

func await2() int {
    return 2
}

func inBlock() {
    try {
        var x = f(1)?
        io.println(x)
    } catch err {
        io.println(err)
    }
}

func g(n int) void! {
    if n > 0 {
        return error(\"g\")
    }
}

func wrap() void! {
    var a = f(1)?(\"ctx\")
    try g(3) catch err {
        io.println(err)
        return err
    }
    io.println(a)
}

func main() void! {
    io.println(viaQ()?, viaTry()?)
}
";

#[test]
fn convert_error_propagation() {
    let src = &fmt(ERRS);
    let (mut c, uri) = start(&[("main.zl", src)]);
    let t = refactor(&mut c, &uri, src, span_range(src, "f(1)?", "?"), "Convert `?` to try/catch", 2);
    assert!(t.contains("func viaQ() int! {\n    var v = try f(1) catch err {\n        return err\n    }\n    f(2)?\n"), "{t}");
    assert_formatted(&t);
    let t = refactor(&mut c, &uri, src, span_range(src, "f(2)?", "f"), "Convert `?` to try/catch", 4);
    assert!(t.contains("    try f(2) catch err {\n        return err\n    }\n    return v"), "{t}");
    let t = refactor(&mut c, &uri, src, span_range(src, "try f(1) catch", "try"), "Convert try/catch to `?`", 6);
    assert!(t.contains("    var w = f(1)?\n    return w"), "{t}");
    assert_formatted(&t);
    let t = refactor(&mut c, &uri, src, span_range(src, "try await", "try"), "Convert try/catch to `?`", 8);
    assert!(t.contains("    var x = (await ag())?\n"), "{t}");
    // Negative: `?` inside a block `try`, `?` with context, a catch that
    // does more than propagate.
    for (from, to) in [("f(1)?\n        io", "?"), ("f(1)?(\"ctx\")", "?"), ("try g(3)", "try")] {
        let ts = titles(&mut c, &uri, span_range(src, from, to));
        assert!(!ts.iter().any(|t| t.starts_with("Convert")), "{from}: {ts:?}");
    }
}
