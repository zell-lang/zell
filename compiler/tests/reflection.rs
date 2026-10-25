//! Reflection: the HIR summary for code generation (`Program::reflect`),
//! attribute values, compile-time `reflect.Fields<T>()` unrolling, static
//! `if`, diagnostics, and end-to-end behaviour of the tables (type
//! identity, `Value.Set` type checks, `--leak-check`). The acceptance tests
//! are `tests/spec/reflection` and `tests/spec/stdlib/json_*`.

use zell::diagnostics::{Diagnostics, Severity};
use zell::driver;
use zell::hir::*;
use zell::span::SourceFile;
use zell::types::Type;
use std::path::{Path, PathBuf};
use std::process::Command;

fn analyze(src: &str) -> (Option<Program>, Vec<String>) {
    let files = vec![SourceFile::new("r.zl".to_string(), src.to_string())];
    let mut diags = Diagnostics::new();
    let p = driver::check(&files, &mut diags);
    let errors = diags.items.iter().filter(|d| d.severity == Severity::Error).map(|d| d.message.clone()).collect();
    (p, errors)
}

/// A program: namespace `t`, `import io`, then `body`.
fn prog(body: &str) -> String {
    format!("namespace t\nimport io\n{body}\n")
}

fn ok(body: &str) -> Program {
    let (p, errors) = analyze(&prog(body));
    assert!(errors.is_empty(), "unexpected errors: {errors:#?}");
    p.expect("program")
}

fn err(body: &str, needle: &str) {
    let (p, errors) = analyze(&prog(body));
    assert!(p.is_none(), "expected failure");
    assert!(errors.iter().any(|e| e.contains(needle)), "no error containing {needle:?} in {errors:#?}");
}

fn struct_ty(p: &Program, name: &str) -> zell::types::TypeId {
    let sid = p.structs.iter().position(|s| s.name == name).expect("struct") as u32;
    (0..p.types.len() as u32).map(zell::types::TypeId).find(|t| matches!(p.types.get(*t), Type::Struct(d) if d.0 == sid)).expect("struct type")
}

const USER: &str = "struct User {\n    @json(name: \"id\") Id int\n    Email string\n    @json(skip: true) cache string\n}\n";

#[test]
fn no_reflection_without_use() {
    let p = ok("struct P {\n    X int\n}\nfunc main() {\n    var x any = P{X: 1}\n    println(x)\n}");
    assert!(p.reflect.is_none(), "reflect must not be loaded");
    assert!(!p.funcs.iter().any(|f| f.full_name.starts_with("reflect.")));
}

#[test]
fn type_info_is_one_table_per_type() {
    let p = ok(&format!(
        "{USER}func Same<T>(x any) bool {{\n    return typeof(x) == reflect.TypeOf<T>()\n}}\nfunc main() {{\n    var u = User{{Id: 1}}\n    println(reflect.TypeOf<User>() == typeof(u), Same<User>(u), reflect.TypeOf<User>().Name)\n}}"
    ));
    let r = p.reflect.as_ref().expect("reflection on");
    let user = struct_ty(&p, "User");
    let n = r.intrinsics.iter().filter(|(_, k)| *k == Intrinsic::TypeOf(user)).count();
    assert_eq!(n, 1, "TypeOf<User> must be one function (one table)");
    assert!(r.full.contains(&user), "a type named in TypeOf keeps its methods");
    // The pointer type used by TypeInfo.New is interned.
    assert!(r.pointers.iter().any(|(t, _)| *t == user));
}

#[test]
fn attribute_values_are_constants_with_defaults() {
    let p = ok("@attribute(targets: [reflect.Target.Field])\nstruct column {\n    name string\n    primary bool = false\n    width int = 8\n}\nstruct Row {\n    @column(\"id\", primary: true) Id int\n    @column(\"n\") N int\n}\nfunc main() {\n    var f = reflect.TypeOf<Row>().Fields[1]\n    var c = f.Attribute<column>()\n    println(c.name, c.primary, c.width)\n}");
    let row = p.structs.iter().find(|s| s.name == "Row").unwrap();
    let a = &row.fields[0].attrs[0];
    assert!(matches!(a.kind, AttrKind::User(_)));
    assert_eq!(a.args, vec![("name".into(), ConstValue::Str("id".into())), ("primary".into(), ConstValue::Bool(true)), ("width".into(), ConstValue::Int(8))]);
    let b = &row.fields[1].attrs[0];
    assert_eq!(b.args[1], ("primary".into(), ConstValue::Bool(false)));
    // The attribute struct's pointer type exists for the boxed values.
    let column = struct_ty(&p, "column");
    assert!(p.reflect.as_ref().unwrap().pointers.iter().any(|(t, _)| *t == column));
}

#[test]
fn fields_loop_is_unrolled_once_per_field() {
    let p = ok(&format!(
        "{USER}func Names<T>(v T) int {{\n    var n = 0\n    for f in reflect.Fields<T>() {{\n        n += len(f.Name)\n        _ = f.Get(v)\n    }}\n    return n\n}}\nstruct Empty {{\n}}\nfunc main() {{\n    println(Names(User{{}}), Names(Empty{{}}))\n}}"
    ));
    let user = p.funcs.iter().find(|f| f.name == "Names<User>").expect("instance");
    let copies = user.locals.iter().filter(|l| l.name == "f" && l.kind == LocalKind::LoopVar).count();
    assert_eq!(copies, 3, "one binding of f per field of User");
    // f.Get(v) is a direct field access of the field's static type.
    let dump = p.dump();
    assert!(dump.contains("%0:v:t.User.Email:string"), "{dump}");
    // A struct without fields: the body is checked once but not emitted.
    let empty = p.funcs.iter().find(|f| f.name == "Names<Empty>").expect("instance");
    assert!(!zell::hir::dump::type_name(&p.types, empty.ret, &p).is_empty());
}

#[test]
fn static_if_checks_only_the_taken_branch() {
    // `x + 1` is only valid for ints, `x + "!"` only for strings.
    let p = ok("func Bump<T>(x T) T {\n    if reflect.TypeOf<T>().Kind == reflect.Kind.Int {\n        return x + 1\n    } else if reflect.TypeOf<T>().Kind == reflect.Kind.String {\n        return x + \"!\"\n    }\n    return x\n}\nfunc main() {\n    println(Bump(1), Bump(\"a\"), Bump(true))\n}");
    assert!(p.funcs.iter().any(|f| f.name == "Bump<bool>"));
}

#[test]
fn field_conditions_fold_without_run_time_lookups() {
    // Every condition below is a compile-time constant per field, so each
    // `if` / `switch` keeps one branch: the branches that would not type
    // check for a field (`x + 1` on a string) are not checked, and the
    // instance reads no FieldInfo table and calls no Attribute lookup.
    let p = ok("@attribute(targets: [reflect.Target.Field])\nstruct num {\n    min int\n}\nstruct P {\n    @num(min: 2) A int\n    B string\n    C bool\n}\n\
func Sum<T>(v T) int {\n    var n = 0\n    for f in reflect.Fields<T>() {\n        var tag = f.Attribute<num>()\n        if tag != nil {\n            n += f.Get(v) + tag.min\n        }\n\
        if f.Attribute<num>() == nil && f.Type.Kind == reflect.Kind.String {\n            n += len(f.Get(v))\n        }\n        if f.Type == reflect.TypeOf<bool>() && f.Name == \"C\" {\n            n += 100\n        }\n\
        switch f.Get(v) {\n        case int x:\n            n += x + 1\n        case string s:\n            n += len(s + \"!\")\n        }\n    }\n    return n\n}\nfunc main() {\n    println(Sum(P{A: 1, B: \"ab\"}))\n}");
    let dump = p.dump();
    let start = dump.lines().position(|l| l.starts_with("func #") && l.contains("t.Sum<P>")).expect("instance");
    let body: String = dump.lines().skip(start).take_while({
        let mut first = true;
        move |l| std::mem::take(&mut first) || !l.starts_with("func #")
    }).collect::<Vec<_>>().join("\n");
    for needle in ["FieldInfo.", "TypeInfo.", "IsNil", "Attribute", "if Eq(%"] {
        let hits = body.matches(needle).count();
        let allowed = if needle == "if Eq(%" { 3 } else { 0 };
        assert!(hits <= allowed, "{needle} in the folded instance ({hits}x):\n{body}");
    }
    assert_eq!(body.matches(":*reflect.FieldInfo = nil").count() + body.matches("f = nil:*reflect.FieldInfo").count(), 3, "f bound to nil in every copy:\n{body}");
}

#[test]
fn compile_time_reflection_errors() {
    err("struct P {\n    X int\n}\nfunc main() {\n    for f in reflect.Fields<P>() {\n        println(f.Name)\n    }\n}", "can only be used in a generic function");
    err("func G<T>(v T) {\n    for f in reflect.Fields<T>() {\n        println(f.Name)\n    }\n}\nfunc main() {\n    G(1)\n}", "needs a struct type, int is not a struct");
    err("struct P {\n    X int\n}\nfunc main() {\n    var fs = reflect.Fields<P>()\n    println(len(fs))\n}", "can only be the sequence of a for loop");
    err("struct P {\n    X int\n}\nfunc G<T>(v T, w P) {\n    for f in reflect.Fields<T>() {\n        println(f.Get(w))\n    }\n}\nstruct Q {\n    Y int\n}\nfunc main() {\n    G(Q{}, P{})\n}", "f.Get needs a value of type");
    err("func main() {\n    println(typeof(1, 2))\n}", "typeof");
}

#[test]
fn types_with_only_emits_tables_for_the_marked_types() {
    let p = ok("@attribute(targets: [reflect.Target.Struct])\nstruct job {}\n@job\nstruct A {\n    func Run() int {\n        return 1\n    }\n}\nstruct Other {\n    func Run() int {\n        return 2\n    }\n}\nfunc main() {\n    println(len(reflect.TypesWith<job>()), Other{}.Run())\n}");
    let r = p.reflect.as_ref().expect("reflection on");
    let (a, other) = (struct_ty(&p, "A"), struct_ty(&p, "Other"));
    assert_eq!(r.types_with.len(), 1, "one TypesWith<job> list");
    assert_eq!(r.types_with[0].1, vec![a]);
    assert!(r.full.contains(&a), "a marked type is full (methods, New)");
    assert!(!r.full.contains(&other), "an unmarked type gets no method table");
    assert_eq!(r.method_types.len(), 1, "only A.Run is listed");
}

#[test]
fn types_with_and_error_diagnostics() {
    err("struct NotAttr {}\nfunc main() {\n    println(len(reflect.TypesWith<NotAttr>()))\n}", "needs an attribute type");
    err("func main() {\n    reflect.Error(\"bad \" + \"config\")\n}", "bad config");
    err("func main() {\n    var m = \"x\"\n    reflect.Error(m)\n}", "reflect.Error needs a constant string message");
    // In a branch dropped by a static if, and in a generic function never
    // instantiated, nothing is raised.
    ok("func G<T>() {\n    reflect.Error(\"never\")\n}\nfunc main() {\n    if reflect.TypeOf<int>().Kind == reflect.Kind.String {\n        reflect.Error(\"dead\")\n    }\n    println(1)\n}");
}

#[test]
fn private_table_fields_stay_private() {
    err("func main() {\n    println(len(reflect.TypeOf<int>().attrs))\n}", "private");
}

// -- end to end ----------------------------------------------------------------

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn ensure_runtime() {
    let lib = repo().join("runtime/libzell_rt.a");
    if !lib.is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
}

/// Runs `src` with `zell run --leak-check`: (exit code, stdout, stderr).
fn run(name: &str, src: &str) -> (i32, String, String) {
    ensure_runtime();
    let dir = std::env::temp_dir().join(format!("zell-reflect-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{name}.zl"));
    std::fs::write(&file, src).unwrap();
    let r = Command::new(env!("CARGO_BIN_EXE_zell")).arg("run").arg(&file).arg("--leak-check").env("ZELL_NO_BACKTRACE", "1").output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (r.status.code().unwrap_or(-1), String::from_utf8_lossy(&r.stdout).into_owned(), String::from_utf8_lossy(&r.stderr).into_owned())
}

#[test]
fn set_with_wrong_type_panics_naming_the_field() {
    let (code, out, errs) = run("set", &prog("struct U {\n    Age int\n}\nfunc main() {\n    var u = U{}\n    reflect.ValueOf(&u).Field(\"Age\").Set(41)\n    println(u.Age)\n    reflect.ValueOf(&u).Field(\"Age\").Set(\"x\")\n}"));
    assert_eq!(out, "41\n");
    assert_eq!(code, 2, "{errs}");
    assert!(errs.contains("Age") && errs.contains("string"), "{errs}");
}

#[test]
fn tables_are_shared_and_leak_free() {
    let src = prog(&format!(
        "{USER}@reflect\nstruct Box {{\n    Items []User\n    Index map[string]*User\n    func Count() int {{\n        return len(this.Items)\n    }}\n}}\nfunc main() {{\n    var b = Box{{Items: [User{{Id: 1}}, User{{Id: 2}}]}}\n    var x any = b\n    var t = typeof(x)\n    println(t.Name, len(t.Methods), t.Fields[0].Type.Name, t.Fields[0].Type.Elem == reflect.TypeOf<User>())\n    println(t.Fields[1].Type.Key.Name, t.Fields[1].Type.Elem.Elem.Name)\n    var r = try t.Method(\"Count\").Call(&b) else nil\n    println(r)\n    var fresh = t.New()\n    println(typeof(fresh).Name)\n}}"
    ));
    let (code, out, errs) = run("shared", &src);
    assert_eq!(code, 0, "{errs}");
    assert_eq!(out, "Box 1 []User true\nstring User\n2\n*Box\n");
}
