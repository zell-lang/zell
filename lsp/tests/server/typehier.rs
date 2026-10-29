//! Type hierarchy (`lsp/src/typehier.rs`), rename refusals by block
//! scopes (`lsp/src/scoping.rs`) and navigation on type aliases.

use super::*;
use super::protocol::goto;

const SHAPES: &str = "namespace app.shapes

interface Shape {
    func Area() int
}

interface Named {
    Shape
    func Name() string
}

interface Getter {
    func Get() int
}
";

const BASE: &str = "namespace app.base

struct Base {
    func Name() string { return \"b\" }
}
";

// `app.kinds` does not import `app.shapes`: satisfaction is structural.
const KINDS: &str = "namespace app.kinds

import app.base

struct Sq {
    Base
    Side int
    func Area() int { return this.Side }
}

struct Box<T> {
    V T
    func Get() T { return this.V }
    func Area() int { return 1 }
}

struct Src {
    func Read(buf []byte) int! { return 0 }
}
";

fn hier_project() -> (PathBuf, Vec<(Url, &'static str)>) {
    let (dir, shapes) = project_with("shapes.zl", SHAPES);
    let mut out = vec![(shapes, SHAPES)];
    for (name, text) in [("base.zl", BASE), ("kinds.zl", KINDS)] {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        out.push((Url::from_file_path(&p).unwrap(), text));
    }
    (dir, out)
}

fn prepare(c: &mut Client, uri: &Url, pos: Position) -> Vec<TypeHierarchyItem> {
    let r: Option<Vec<TypeHierarchyItem>> = c.request::<request::TypeHierarchyPrepare>(TypeHierarchyPrepareParams { text_document_position_params: tdp(uri, pos), work_done_progress_params: Default::default() });
    r.unwrap_or_default()
}

fn supers(c: &mut Client, it: &TypeHierarchyItem) -> Vec<TypeHierarchyItem> {
    let r: Option<Vec<TypeHierarchyItem>> = c.request::<request::TypeHierarchySupertypes>(TypeHierarchySupertypesParams { item: it.clone(), work_done_progress_params: Default::default(), partial_result_params: Default::default() });
    r.unwrap_or_default()
}

fn subs(c: &mut Client, it: &TypeHierarchyItem) -> Vec<TypeHierarchyItem> {
    let r: Option<Vec<TypeHierarchyItem>> = c.request::<request::TypeHierarchySubtypes>(TypeHierarchySubtypesParams { item: it.clone(), work_done_progress_params: Default::default(), partial_result_params: Default::default() });
    r.unwrap_or_default()
}

fn names(items: &[TypeHierarchyItem]) -> Vec<&str> {
    items.iter().map(|i| i.name.as_str()).collect()
}

#[test]
fn type_hierarchy_supertypes_and_subtypes() {
    let (dir, files) = hier_project();
    let (shapes, kinds) = (&files[0], &files[2]);
    let mut c = Client::start(&dir);
    assert_eq!(c.raw_capabilities["typeHierarchyProvider"], serde_json::Value::Bool(true));
    c.open(&kinds.0, kinds.1);
    assert_eq!(c.diagnostics(&kinds.0), vec![]);
    c.open(&shapes.0, shapes.1);

    // A struct: the interfaces it satisfies (one through a promoted
    // method), in a namespace it does not import, and the struct it embeds.
    let sq = prepare(&mut c, &kinds.0, pos_of(KINDS, "struct Sq", 0, 8));
    assert_eq!(names(&sq), ["Sq"]);
    assert_eq!((sq[0].kind, sq[0].detail.as_deref()), (SymbolKind::STRUCT, Some("app.kinds.Sq")));
    assert_eq!(sq[0].selection_range.start, pos_of(KINDS, "struct Sq", 0, 7));
    let up = supers(&mut c, &sq[0]);
    assert_eq!(names(&up), ["Base", "Named", "Shape"]);
    assert_eq!(up[1].uri, shapes.0);
    // From a use of the embedded struct, down again.
    let base = prepare(&mut c, &kinds.0, pos_of(KINDS, "    Base\n", 0, 5));
    assert_eq!(names(&subs(&mut c, &base[0])), ["Sq"]);

    // An interface: its implementors and the interfaces with all its methods.
    let shape = prepare(&mut c, &shapes.0, pos_of(SHAPES, "interface Shape", 0, 11));
    assert_eq!(shape[0].kind, SymbolKind::INTERFACE);
    assert_eq!(names(&subs(&mut c, &shape[0])), ["Box", "Sq", "Named"]);
    let named = prepare(&mut c, &shapes.0, pos_of(SHAPES, "interface Named", 0, 11));
    assert_eq!(names(&supers(&mut c, &named[0])), ["Shape"]);
    assert_eq!(names(&subs(&mut c, &named[0])), ["Sq"]);

    // A generic struct, at its declaration: `Get() T` satisfies `Get() int`.
    let bx = prepare(&mut c, &kinds.0, pos_of(KINDS, "struct Box", 0, 8));
    assert_eq!(names(&supers(&mut c, &bx[0])), ["Getter", "Shape"]);

    // A std interface, located in the std sources.
    let src = prepare(&mut c, &kinds.0, pos_of(KINDS, "struct Src", 0, 8));
    let up = supers(&mut c, &src[0]);
    let reader = up.iter().find(|i| i.detail.as_deref() == Some("io.Reader")).unwrap_or_else(|| panic!("{:?}", names(&up)));
    assert!(reader.uri.path().ends_with("std/io/io.zl"), "{}", reader.uri);
    assert!(names(&subs(&mut c, reader)).contains(&"Src"));

    // `implementation` shares the table: structs only, promoted methods.
    let imps = goto::<request::GotoImplementation>(&mut c, &shapes.0, pos_of(SHAPES, "interface Named", 0, 11));
    assert_eq!(imps.len(), 1);
    assert_eq!(imps[0].range.start, pos_of(KINDS, "struct Sq", 0, 7));
    let imps = goto::<request::GotoImplementation>(&mut c, &shapes.0, pos_of(SHAPES, "func Name()", 0, 5));
    assert_eq!((imps[0].uri.clone(), imps[0].range.start), (files[1].0.clone(), pos_of(BASE, "func Name()", 0, 5)));

    // Nothing at a non-type.
    assert!(prepare(&mut c, &kinds.0, pos_of(KINDS, "Side int", 0, 1)).is_empty());

    // An edit of the interface updates the other namespace.
    let edited = SHAPES.replace("func Area() int", "func Area() float");
    c.change(&shapes.0, 2, &edited);
    let _ = c.diagnostics(&shapes.0);
    assert_eq!(names(&supers(&mut c, &sq[0])), ["Base"]);
}

const SCOPED: &str = "namespace app

import io

type Celsius = float
type P = Point

struct Point {
    X int
}

func helper() int {
    return 1
}

func later() int {
    var a = 1
    var r = a
    var b = 2
    return r + b
}

func inner() {
    var x = 1
    if x > 0 {
        var y = 2
        println(x, y)
    }
}

func captures() int {
    var t = 3
    return t + helper()
}

func blocks() int {
    if true {
        var tmp = 0
        println(tmp)
    }
    {
        var other = 1
        println(other)
    }
    return helper()
}

func usesTmp() int {
    var tmp = helper()
    return tmp + helper()
}

func warm(c Celsius) Celsius {
    var p = P{X: 1}
    return c + float(p.X)
}

func main() {
    println(later(), captures(), blocks(), usesTmp(), warm(1.5))
    inner()
}
";

fn rename(c: &mut Client, uri: &Url, pos: Position, to: &str) -> Result<WorkspaceEdit, String> {
    let id = c.send_request(request::Rename::METHOD, serde_json::to_value(RenameParams { text_document_position: tdp(uri, pos), new_name: to.into(), work_done_progress_params: Default::default() }).unwrap());
    let r = c.response(&id);
    match r.error {
        Some(e) => Err(e.message),
        None => Ok(serde_json::from_value(r.result.expect("rename result")).unwrap()),
    }
}

fn edits_in(edit: &WorkspaceEdit, uri: &Url) -> usize {
    edit.changes.as_ref().and_then(|c| c.get(uri)).map_or(0, |v| v.len())
}

#[test]
fn rename_refuses_every_shadowing_case() {
    let (dir, uri) = project_with("main.zl", SCOPED);
    let mut c = Client::start(&dir);
    c.open(&uri, SCOPED);
    assert_eq!(c.diagnostics(&uri), vec![]);
    let refused = |c: &mut Client, needle: &str, delta: usize, to: &str, with: &str| {
        let e = rename(c, &uri, pos_of(SCOPED, needle, 0, delta), to).expect_err(needle);
        assert!(e.contains("clash") && e.contains(with), "{needle} -> {to}: {e}");
    };
    // A local declared later in the same block.
    refused(&mut c, "var a = 1", 4, "b", "b");
    // A use inside an inner block that declares the new name.
    refused(&mut c, "var x = 1", 4, "y", "y");
    // A local that would hide a later use of a function.
    refused(&mut c, "var t = 3", 4, "helper", "helper");
    // A function renamed to a local visible at one of its uses, in another
    // function (the use in the local's own initializer is not captured).
    refused(&mut c, "func helper", 5, "tmp", "tmp");
    let e = rename(&mut c, &uri, pos_of(SCOPED, "func helper", 0, 5), "tmp").unwrap_err();
    assert!(e.contains(&format!("main.zl:{}", SCOPED[..SCOPED.find("var tmp = helper").unwrap()].lines().count())), "{e}");
    // Allowed: the same names where nothing is captured.
    let ok = |c: &mut Client, needle: &str, delta: usize, to: &str| rename(c, &uri, pos_of(SCOPED, needle, 0, delta), to).unwrap_or_else(|e| panic!("{needle} -> {to}: {e}"));
    assert_eq!(edits_in(&ok(&mut c, "var other = 1", 4, "tmp"), &uri), 2, "a sibling block's local");
    assert_eq!(edits_in(&ok(&mut c, "var r = a", 4, "helper2"), &uri), 2);
    assert_eq!(edits_in(&ok(&mut c, "var y = 2", 4, "z"), &uri), 2);
    // A local may hide a global that is not used where it is visible.
    assert_eq!(edits_in(&ok(&mut c, "var t = 3", 4, "later"), &uri), 2);
}

#[test]
fn type_aliases_navigate_and_rename() {
    let (dir, uri) = project_with("main.zl", SCOPED);
    let mut c = Client::start(&dir);
    c.open(&uri, SCOPED);
    let decl = pos_of(SCOPED, "type Celsius", 0, 5);
    // Definition and hover from a use.
    assert_eq!(definition(&mut c, &uri, pos_of(SCOPED, "c Celsius", 0, 3)).range.start, decl);
    assert!(hover_text(&mut c, &uri, pos_of(SCOPED, "Celsius) Celsius", 0, 10)).contains("type Celsius = float"));
    // An alias of a struct: the use is the alias's, not the struct's.
    assert_eq!(definition(&mut c, &uri, pos_of(SCOPED, "P{X: 1}", 0, 0)).range.start, pos_of(SCOPED, "type P", 0, 5));
    // References and rename.
    let refs: Option<Vec<Location>> = c.request::<request::References>(ReferenceParams {
        text_document_position: tdp(&uri, decl),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext { include_declaration: true },
    });
    assert_eq!(refs.unwrap_or_default().len(), 3);
    let edit = rename(&mut c, &uri, decl, "Temp").unwrap();
    assert_eq!(edits_in(&edit, &uri), 3);
    let edit = rename(&mut c, &uri, pos_of(SCOPED, "type P", 0, 5), "Pt").unwrap();
    assert_eq!(edits_in(&edit, &uri), 2);
    assert!(rename(&mut c, &uri, decl, "Point").expect_err("a sibling").contains("clash"));
}
