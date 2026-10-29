//! Semantic analysis tests: the example corpus plus focused unit tests.

use zell::diagnostics::{Diagnostics, Severity};
use zell::driver;
use zell::hir::*;
use zell::span::SourceFile;
use zell::types::Type;
use std::path::Path;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Analyses several in-memory files.
fn analyze(srcs: &[&str]) -> (Option<Program>, Vec<String>) {
    let files: Vec<SourceFile> = srcs.iter().enumerate().map(|(i, s)| SourceFile::new(format!("f{i}.zl"), *s)).collect();
    let mut diags = Diagnostics::new();
    let p = driver::check(&files, &mut diags);
    let errors = diags.items.iter().filter(|d| d.severity == Severity::Error).map(|d| d.message.clone()).collect();
    (p, errors)
}

fn ok_files(srcs: &[&str]) -> Program {
    let (p, errors) = analyze(srcs);
    assert!(errors.is_empty(), "unexpected errors: {errors:#?}");
    p.expect("program")
}

fn ok(src: &str) -> Program {
    ok_files(&[src])
}

/// Expects an error whose message contains `needle`.
fn err_files(srcs: &[&str], needle: &str) {
    let (p, errors) = analyze(srcs);
    assert!(p.is_none(), "expected failure");
    assert!(errors.iter().any(|e| e.contains(needle)), "no error containing {needle:?} in {errors:#?}");
}

fn err(src: &str, needle: &str) {
    err_files(&[src], needle)
}

/// `namespace t` + `import io` (when the body prints with a short name:
/// unused imports are errors) + body.
fn prog(body: &str) -> String {
    let io = if body.replace("io.print", "").contains("print") { "import io\n" } else { "" };
    format!("namespace t\n{io}{body}\n")
}

/// [`prog`] that always imports `io` (the caller appends printing code).
fn prog_io(body: &str) -> String {
    format!("namespace t\nimport io\n{body}\n")
}

/// A `main` with the given statements.
fn main_body(stmts: &str) -> String {
    prog(&format!("func main() {{\n{stmts}\n}}"))
}

fn func<'p>(p: &'p Program, name: &str) -> &'p Function {
    p.funcs.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("no function {name}"))
}

fn examples_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../examples"))
}

fn check_path(path: &Path) -> (Option<Program>, Vec<String>) {
    let paths = driver::collect_files(path).unwrap();
    let files = driver::load(&paths).unwrap();
    let mut diags = Diagnostics::new();
    let p = driver::check(&files, &mut diags);
    let errors = diags.items.iter().filter(|d| d.severity == Severity::Error).map(|d| d.message.clone()).collect();
    (p, errors)
}

/// Walks all expressions of a block (shallow helper for assertions).
fn exprs_in(b: &Block, out: &mut Vec<Expr>) {
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Let { init: Some(e), .. } | StmtKind::Expr(e) | StmtKind::Discard(e) | StmtKind::Defer(e) => out.push(e.clone()),
            StmtKind::LetTuple { init, .. } => out.push(init.clone()),
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                out.push(target.clone());
                out.push(value.clone());
            }
            StmtKind::Return(ReturnValue::Value(e) | ReturnValue::Error(e)) => out.push(e.clone()),
            StmtKind::If { cond, then, else_ } => {
                out.push(cond.clone());
                exprs_in(then, out);
                if let Some(e) = else_ {
                    exprs_in(e, out);
                }
            }
            StmtKind::Block(b) | StmtKind::Loop { body: b, .. } | StmtKind::ForRange { body: b, .. } | StmtKind::ForSlice { body: b, .. } | StmtKind::ForString { body: b, .. } | StmtKind::ForMap { body: b, .. } => exprs_in(b, out),
            StmtKind::Switch(sw) => {
                if let Some((_, e)) = &sw.subject {
                    out.push(e.clone());
                }
                for c in &sw.cases {
                    exprs_in(&c.body, out);
                }
            }
            _ => {}
        }
    }
}

fn main_exprs(p: &Program) -> Vec<Expr> {
    let mut v = Vec::new();
    exprs_in(&func(p, "main").body, &mut v);
    v
}

// ---------------------------------------------------------------------------
// Example corpus
// ---------------------------------------------------------------------------

#[test]
fn every_example_checks_cleanly() {
    let mut count = 0;
    for entry in std::fs::read_dir(examples_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "zl") {
            let (p, errors) = check_path(&path);
            assert!(errors.is_empty(), "{}: {errors:#?}", path.display());
            let p = p.unwrap();
            assert!(p.entry.is_some(), "{}: no entry point", path.display());
            count += 1;
        }
    }
    assert!(count >= 14, "only {count} examples found");
}

#[test]
fn multi_file_example_checks_cleanly() {
    let (p, errors) = check_path(&examples_dir().join("multi"));
    assert!(errors.is_empty(), "{errors:#?}");
    let p = p.unwrap();
    let entry = &p.funcs[p.entry.unwrap().0 as usize];
    assert_eq!(entry.full_name, "app.main");
    assert!(p.structs.iter().any(|s| s.full_name == "app.models.User"));
}

/// Each compile-failure example and a key phrase its diagnostic must contain
/// (derived from the file's `// error:` line).
const ERROR_KEYWORDS: &[(&str, &str)] = &[
    ("await_outside_async.zl", "await outside async"),
    ("catch_falls_through.zl", "catch block must return"),
    ("copy_drop_value.zl", "cannot copy a value of type File"),
    ("extern_in_user_code.zl", "extern declarations exist only in the standard library"),
    ("import_std_removed.zl", "use `io` instead of `std.io`"),
    ("interior_pointer_escape.zl", "cannot keep interior pointer variable `p`"),
    ("log_template_args.zl", "the template has 2 placeholders ({UserId}, {Amount}) but 3 arguments are given"),
    ("missing_return.zl", "missing return"),
    ("print_data_enum.zl", "value of type Expr cannot be printed"),
    ("question_outside_fallible.zl", "? outside of fallible function"),
    ("std_namespace_declared.zl", "namespace `io` belongs to the standard library"),
    ("type_mismatch.zl", "cannot assign string to int"),
    ("unhandled_result.zl", "result of type int! must be handled"),
    ("unhandled_void_result.zl", "result of type void! must be handled"),
    ("unknown_ident.zl", "unknown identifier undefinedName"),
    ("unused_var.zl", "unused variable x"),
    ("weak_to_interior.zl", "cannot keep an interior pointer"),
];

#[test]
fn every_compile_error_example_fails_with_its_diagnostic() {
    let dir = examples_dir().join("errors_compile");
    let mut files: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| n.ends_with(".zl")).collect();
    files.sort();
    let mapped: Vec<&str> = ERROR_KEYWORDS.iter().map(|(f, _)| *f).collect();
    assert_eq!(files, mapped, "ERROR_KEYWORDS must list every errors_compile file");
    for (file, keyword) in ERROR_KEYWORDS {
        let path = dir.join(file);
        let first_line = std::fs::read_to_string(&path).unwrap().lines().next().unwrap().to_string();
        assert!(first_line.starts_with("// error:"), "{file}");
        let (p, errors) = check_path(&path);
        assert!(p.is_none(), "{file} unexpectedly compiled");
        assert!(errors.iter().any(|e| e.contains(keyword)), "{file}: no error containing {keyword:?}: {errors:#?}");
    }
}

// ---------------------------------------------------------------------------
// Imports, namespaces, visibility
// ---------------------------------------------------------------------------

#[test]
fn import_rules_follow_the_table() {
    ok("namespace t\nfunc main() { io.println(\"hi\") }");
    ok("namespace t\nfunc main() { io.println(\"hi\") }");
    ok("namespace t\nimport io\nfunc main() { println(\"hi\") }");
    ok("namespace t\nimport io as out\nfunc main() { out.println(\"hi\") }");
    err("namespace t\nfunc main() { println(\"hi\") }", "unknown identifier println");
    err("namespace t\nfunc main() { println(\"hi\") }", "unknown identifier println");
    err("namespace t\nimport io as out\nfunc main() { println(\"hi\") }", "unknown identifier println");
    err("namespace t\nimport nope\nfunc main() {}", "unknown namespace");
}

#[test]
fn namespaces_merge_across_files_and_type_imports_work() {
    let a = "namespace app.models\nstruct User { Name string }";
    let b = "namespace app.models\nfunc Make(n string) User { return User{Name: n} }";
    let c = "namespace app\nimport io\nimport app.models.User\nfunc main() { var u User = app.models.Make(\"x\")\n println(u.Name) }";
    ok_files(&[a, b, c]);
    let c2 = "namespace app\nimport io\nimport app.models.User as U\nfunc main() { var u = U{Name: \"y\"}\n println(u.Name) }";
    ok_files(&[a, b, c2]);
}

#[test]
fn ambiguous_short_names_are_errors_on_use() {
    let a = "namespace a\nfunc F() int { return 1 }";
    let b = "namespace b\nfunc F() int { return 2 }";
    let c = "namespace app\nimport a\nimport b\nimport io\nfunc main() { println(F()) }";
    err_files(&[a, b, c], "ambiguous name F");
    let c2 = "namespace app\nimport a\nimport b as bb\nimport io\nfunc main() { println(F(), bb.F()) }";
    ok_files(&[a, b, c2]);
}

#[test]
fn private_names_are_hidden_from_other_namespaces() {
    let m = "namespace m\nstruct S { Pub int\n priv int\n func get() int { return this.priv } }\nfunc helper() int { return 1 }";
    err_files(&[m, "namespace app\nimport m\nfunc main() { var s = m.S{}\n _ = s.priv }"], "priv is private to namespace m");
    err_files(&[m, "namespace app\nimport m\nfunc main() { var s = S{priv: 1}\n _ = s }"], "private to namespace m");
    err_files(&[m, "namespace app\nfunc main() { _ = m.helper() }"], "helper is private");
    err_files(&[m, "namespace app\nimport m\nfunc main() { var s = S{}\n _ = s.get() }"], "get is private");
    ok_files(&[m, "namespace app\nimport m\nfunc main() { var s = S{Pub: 1}\n _ = s.Pub }"]);
}

#[test]
fn duplicate_declarations_are_reported() {
    err(&prog("func F() {}\nfunc F() {}\nfunc main() {}"), "duplicate declaration of F");
    err_files(&["namespace a\nconst X = 1", "namespace a\nconst X = 2"], "duplicate declaration of X");
    err(&prog("struct S { A int\n A string }\nfunc main() {}"), "duplicate field A");
    err(&main_body("var x = 1\nvar x = 2\nprintln(x)"), "x redeclared");
}

#[test]
fn unknown_types_and_namespaces() {
    err(&prog("func F(x Foo) {}\nfunc main() {}"), "unknown type Foo");
    err(&main_body("var x = nope.Thing()\nprintln(x)"), "unknown identifier nope");
    err(&main_body("io.nothing()"), "unknown identifier nothing in namespace io");
}

// ---------------------------------------------------------------------------
// Types and expressions
// ---------------------------------------------------------------------------

#[test]
fn untyped_constants_adapt_to_context() {
    let p = ok(&prog("func Half(x float) float { return x / 2 }\nconst K = 3\nfunc main() { var f float = K\n var g = Half(1)\n println(f, g, 1 << 3) }"));
    let main = func(&p, "main");
    let StmtKind::Let { init: Some(e), .. } = &main.body.stmts[0].kind else { panic!() };
    assert!(matches!(e.kind, ExprKind::Const(ConstValue::Float(f)) if f == 3.0));
    assert_eq!(e.ty, p.types.float);
    let half = func(&p, "Half");
    let StmtKind::Return(ReturnValue::Value(e)) = &half.body.stmts[0].kind else { panic!() };
    let ExprKind::Binary { rhs, .. } = &e.kind else { panic!() };
    assert!(matches!(rhs.kind, ExprKind::Const(ConstValue::Float(_))));
    err(&main_body("var i = 1\nvar f = 2.5\nprintln(i + f)"), "mismatched types int and float");
    err(&main_body("var i int = 2.5\nprintln(i)"), "cannot assign float to variable of type int");
    ok(&main_body("var i int = 2.0\nvar c = int(3.99)\nprintln(i, c)"));
}

#[test]
fn type_mismatches_in_calls_returns_and_binops() {
    err(&prog("func F(a int) {}\nfunc main() { F(\"x\") }"), "cannot use string as int in argument 1 of F");
    err(&prog("func F(a int) {}\nfunc main() { F(1, 2) }"), "wrong number of arguments in call to F: expected 1, got 2");
    err(&prog("func F() int { return \"s\" }\nfunc main() {}"), "cannot return string");
    err(&main_body("var s = \"a\" - \"b\"\nprintln(s)"), "operator - is not defined on string");
    err(&main_body("var b = 1 && true\nprintln(b)"), "requires bool operands");
}

#[test]
fn nil_needs_a_nilable_type() {
    err(&main_body("var x int = nil\nprintln(x)"), "cannot use nil as int");
    err(&main_body("var x = nil\n_ = x"), "untyped nil");
    let p = ok(&prog("struct N { V int }\nfunc main() { var p *N = nil\n println(p == nil) }"));
    assert!(main_exprs(&p).iter().any(|e| matches!(&e.kind, ExprKind::Print(items) if items.iter().any(|i| matches!(i.arg.as_ref().map(|a| &a.kind), Some(ExprKind::IsNil { .. }))))));
}

#[test]
fn strings_have_distinct_ops() {
    let p = ok(&main_body("var a = \"x\"\nvar b = a + \"y\"\nprintln(b < a, b == a)"));
    let es = main_exprs(&p);
    let StmtKind::Let { init: Some(e), .. } = &func(&p, "main").body.stmts[1].kind else { panic!() };
    assert!(matches!(e.kind, ExprKind::Binary { op: BinOp::StrConcat, .. }));
    let ExprKind::Print(items) = &es.last().unwrap().kind else { panic!() };
    let ops: Vec<BinOp> = items.iter().filter_map(|i| match &i.arg.as_ref()?.kind {
        ExprKind::Binary { op, .. } => Some(*op),
        _ => None,
    }).collect();
    assert_eq!(ops, vec![BinOp::StrLt, BinOp::StrEq]);
}

#[test]
fn println_is_lowered_per_type() {
    let p = ok(&main_body("println(1, 2.5, true, 'c', \"s\", [1, 2])"));
    let ExprKind::Print(items) = &main_exprs(&p)[0].kind else { panic!() };
    let ops: Vec<PrintOp> = items.iter().map(|i| i.op).collect();
    use PrintOp::*;
    assert_eq!(ops, vec![Int, Space, Float, Space, Bool, Space, Char, Space, Str, Space, Str, Newline]);
    assert!(matches!(items[10].arg.as_ref().unwrap().kind, ExprKind::ToString(_)));
    err(&prog("struct S {}\nfunc main() { println(S{}) }"), "cannot be printed");
}

// ---------------------------------------------------------------------------
// Structs, methods, interfaces
// ---------------------------------------------------------------------------

#[test]
fn methods_take_this_pointer_and_receivers_are_implicitly_addressed() {
    let p = ok(&prog("struct C { n int\n func Inc() { this.n += 1 }\n func Get() int { return this.n } }\nfunc main() { var c = C{}\n c.Inc()\n var p = &c\n p.Inc()\n println(c.Get()) }"));
    let inc = func(&p, "Inc");
    assert_eq!(inc.params.len(), 1);
    let this = &inc.locals[0];
    assert_eq!(this.kind, LocalKind::This);
    assert!(matches!(p.types.get(this.ty), Type::Pointer(_)));
    let main = func(&p, "main");
    let StmtKind::Expr(call) = &main.body.stmts[1].kind else { panic!() };
    let ExprKind::Call { args, .. } = &call.kind else { panic!() };
    assert!(matches!(args[0].kind, ExprKind::AddrOf(_)));
    let StmtKind::Expr(call) = &main.body.stmts[3].kind else { panic!() };
    let ExprKind::Call { args, .. } = &call.kind else { panic!() };
    assert!(matches!(args[0].kind, ExprKind::Local(_)), "pointer receiver passed as is");
    assert!(main.locals[0].address_taken);
    err(&main_body("var x = 1\nx.Foo()"), "cannot call method Foo on value of type int");
    err(&main_body("println(this)"), "this used outside of a method");
    err(&prog("struct S { static func New() S { return this } }\nfunc main() {}"), "this used outside of a method");
}

#[test]
fn auto_deref_and_embedding() {
    let p = ok(&prog("struct A { Name string\n func Hi() string { return this.Name } }\nstruct D { A\n Age int }\nfunc main() { var d = new D{A: A{Name: \"x\"}}\n println(d.Name, d.Hi(), d.A.Name) }"));
    let ExprKind::Print(items) = &main_exprs(&p)[1].kind else { panic!() };
    let ExprKind::Field { base, index: 0 } = &items[0].arg.as_ref().unwrap().kind else { panic!("{:?}", items[0]) };
    let ExprKind::Field { base, index: 0 } = &base.kind else { panic!() };
    assert!(matches!(base.kind, ExprKind::Deref(_)));
}

#[test]
fn struct_literal_checks() {
    err(&prog("struct P { X int }\nfunc main() { var p = P{Y: 1}\n _ = p }"), "unknown field Y in struct literal");
    err(&prog("struct P { X int }\nfunc main() { var p = P{X: \"s\"}\n _ = p }"), "cannot use string as int for field X");
}

#[test]
fn structural_interfaces() {
    let src = prog_io("interface Shape { func Area() float }\nstruct Sq { S float\n func Area() float { return this.S * this.S } }\nstruct Bad { func Area() int { return 1 } }\nfunc Use(s Shape) float { return s.Area() }");
    let p = ok(&format!("{src}\nfunc main() {{ var q = Sq{{S: 2}}\n println(Use(q), Use(&q)) }}"));
    let ExprKind::Print(items) = &main_exprs(&p)[1].kind else { panic!() };
    let ExprKind::Call { args, .. } = &items[0].arg.as_ref().unwrap().kind else { panic!() };
    assert!(matches!(args[0].kind, ExprKind::ToInterface { .. }));
    let uses = func(&p, "Use");
    let StmtKind::Return(ReturnValue::Value(e)) = &uses.body.stmts[0].kind else { panic!() };
    assert!(matches!(e.kind, ExprKind::Call { callee: Callee::Interface { method: 0, .. }, .. }));
    err(&format!("{src}\nfunc main() {{ _ = Use(Bad{{}}) }}"), "Bad does not implement Shape: method Area has type func() int, want func() float");
    err(&format!("{src}\nstruct None {{}}\nfunc main() {{ _ = Use(None{{}}) }}"), "missing method Area");
}

#[test]
fn errors_as_interfaces() {
    ok(&prog("struct E { func Message() string { return \"e\" } }\nfunc F() int! { return E{} }\nfunc main() { var x = try F() else 0\n println(x) }"));
    let p = ok(&prog("struct E { func Message() string { return \"e\" } }\nfunc F() int! { return E{} }\nfunc main() { _ = F() }"));
    let StmtKind::Return(ReturnValue::Error(e)) = &func(&p, "F").body.stmts[0].kind else { panic!() };
    assert!(matches!(e.kind, ExprKind::ToInterface { .. }));
    assert_eq!(e.ty, p.types.error);
}

// ---------------------------------------------------------------------------
// Errors: ?, try, unhandled results
// ---------------------------------------------------------------------------

#[test]
fn propagate_typing() {
    let p = ok(&prog("func P(s string) int! { return strings.ToInt(s) ? }\nfunc Q(s string) int! { var n = P(s)?(\"ctx\")\n return n + 1 }\nfunc main() { _ = Q(\"1\") }"));
    let q = func(&p, "Q");
    let StmtKind::Let { init: Some(e), .. } = &q.body.stmts[0].kind else { panic!() };
    let ExprKind::Propagate { value, context } = &e.kind else { panic!() };
    assert_eq!(e.ty, p.types.int);
    assert!(matches!(p.types.get(value.ty), Type::Fallible(_)));
    assert!(context.is_some());
    err(&prog("func P() int! { return 1 }\nfunc main() { var x = P()?\n println(x) }"), "? outside of fallible function");
    err(&prog("func P() int { return 1 }\nfunc Q() int! { return P()? }\nfunc main() {}"), "? requires a fallible");
    err(&prog("func P() int! { return 1 }\nfunc main() { P() }"), "result of type int! must be handled");
    err(&prog("func P() int! { return 1 }\nfunc main() { println(P() + 1) }"), "result of type int! must be handled");
    ok(&prog("func P() void! { }\nfunc Q() void! { P()?\n return error(\"x\") }\nfunc main() { _ = Q() }"));
}

#[test]
fn try_else_and_catch_typing() {
    let p = ok(&prog("func P() int! { return 1 }\nfunc main() { var a = try P() else 7\n for i in 0..3 { var b = try P() else continue\n println(i, b) }\n println(a) }"));
    let StmtKind::Let { init: Some(e), .. } = &func(&p, "main").body.stmts[0].kind else { panic!() };
    let ExprKind::TryElse { fallback, .. } = &e.kind else { panic!() };
    assert!(matches!(**fallback, TryFallback::Value(_)));
    assert_eq!(e.ty, p.types.int);
    err(&prog("func P() int! { return 1 }\nfunc main() { var a = try P() else \"s\"\n println(a) }"), "expected int, found string");
    err(&prog("func P() int { return 1 }\nfunc main() { var a = try P() else 0\n println(a) }"), "try requires a fallible");
    // catch must diverge only when the value is used
    ok(&prog("func P() int! { return 1 }\nfunc main() { try P() catch err { println(err) } }"));
    err(&prog("func P() int! { return 1 }\nfunc main() { var a = try P() catch err { println(err) }\n println(a) }"), "catch block must return");
    ok(&prog("func P() int! { return 1 }\nfunc main() { for { var a = try P() catch _ { break }\n println(a) } }"));
}

// ---------------------------------------------------------------------------
// Control flow
// ---------------------------------------------------------------------------

#[test]
fn missing_return_follows_go_terminating_rules() {
    ok(&prog("func F(n int) int { if n > 0 { return 1 } else { return 2 } }\nfunc main() {}"));
    ok(&prog("func F(n int) int { for { } }\nfunc main() {}"));
    ok(&prog("func F(n int) int { switch n { case 1: return 1\n default: return 2 } }\nfunc main() {}"));
    ok(&prog("func F(n int) int { panic(\"no\") }\nfunc main() {}"));
    ok(&prog("func F(n int) void! { if n > 0 { return error(\"x\") } }\nfunc main() {}"));
    err(&prog("func F(n int) int { switch n { case 1: return 1 } }\nfunc main() {}"), "missing return");
    err(&prog("func F(n int) int { for { break } }\nfunc main() {}"), "missing return");
    err(&prog("func F(n int) int { }\nfunc main() {}"), "missing return");
}

#[test]
fn unused_variables_and_undeclared_assignment() {
    err(&main_body("var y = 2\ny = 3"), "unused variable y");
    err(&main_body("for i in 0..3 { }"), "unused variable i");
    ok(&main_body("for _ in 0..3 { }\nvar f = func() int { return 1 }\n_ = f"));
    err(&main_body("z = 3"), "cannot assign to undeclared variable z");
}

#[test]
fn await_needs_async() {
    let p = ok(&prog("async func F() int { await task.Delay(1)\n return 1 }\nasync func main() { var x = await F()\n println(x) }"));
    let f = func(&p, "F");
    assert!(f.is_async);
    assert_eq!(f.ret, p.types.int);
    err(&prog("async func F() int { return 1 }\nfunc main() { var x = await F()\n println(x) }"), "await outside async");
    err(&prog("func F() int { return 1 }\nasync func main() { var x = await F()\n println(x) }"), "await requires a Task");
}

#[test]
fn for_loops_lower_to_specific_nodes() {
    let p = ok(&main_body("for i in 0..3 { println(i) }\nfor i, x in [1, 2] { println(i, x) }\nfor c in \"ab\" { println(c) }\nfor k, v in map[string]int{} { println(k, v) }"));
    let kinds: Vec<&str> = func(&p, "main").body.stmts.iter().map(|s| match &s.kind {
        StmtKind::ForRange { .. } => "range",
        StmtKind::ForSlice { .. } => "slice",
        StmtKind::ForString { .. } => "string",
        StmtKind::ForMap { .. } => "map",
        _ => "other",
    }).collect();
    assert_eq!(kinds, vec!["range", "slice", "string", "map"]);
    let StmtKind::ForString { ch: Some(ch), .. } = &func(&p, "main").body.stmts[2].kind else { panic!() };
    assert_eq!(func(&p, "main").locals[ch.0 as usize].ty, p.types.char_);
}

#[test]
fn switch_typing() {
    let p = ok(&prog("func F(n int) string { switch n { case 1, 2: return \"a\"\n case 3..10: return \"b\"\n case n > 100: return \"c\"\n default: return \"d\" } }\nfunc main() {}"));
    let StmtKind::Switch(sw) = &func(&p, "F").body.stmts[0].kind else { panic!() };
    assert_eq!(sw.cases[0].patterns.len(), 2);
    assert!(sw.cases.iter().all(|c| c.patterns.iter().all(|p| matches!(p, Pattern::Cond(_)))));
    err(&prog("func F(n int) { switch n { case \"x\": return } }\nfunc main() {}"), "switch pattern type mismatch");
    err(&prog("func F(n string) { switch n { case 1..3: return } }\nfunc main() {}"), "range pattern needs a numeric");
    // type switch
    let src = prog("interface I { func M() int }\nstruct A { func M() int { return 1 } }\nstruct B { }\n");
    ok(&format!("{src}\nfunc F(i I) int {{ switch i {{ case A a: return a.M()\n default: return 0 }} }}\nfunc main() {{}}"));
    err(&format!("{src}\nfunc F(i I) int {{ switch i {{ case B b: return 1 }}\n return 0 }}\nfunc main() {{}}"), "impossible type switch case");
}

#[test]
fn enum_payloads_and_matching() {
    let src = prog_io("enum R { Ok(v int)\n Fail(code int, why string)\n Empty }\nenum C { Red, Green }");
    let p = ok(&format!("{src}\nfunc F(r R) int {{ switch r {{ case R.Ok(v): return v\n case R.Fail(c, _): return c\n case R.Empty: return 0 }}\n return -1 }}\nfunc main() {{ println(F(R.Ok(1)), F(R.Empty), int(C.Green), C.Red == C.Green, C.Red) }}"));
    let StmtKind::Switch(sw) = &func(&p, "F").body.stmts[0].kind else { panic!() };
    let Pattern::Variant { variant: 1, bindings } = &sw.cases[1].patterns[0] else { panic!() };
    assert!(bindings[0].is_some() && bindings[1].is_none());
    assert!(p.enums.iter().any(|e| e.name == "C" && e.is_plain));
    err(&format!("{src}\nfunc main() {{ var r = R.Ok(\"s\")\n _ = r }}"), "cannot use string as int");
    err(&format!("{src}\nfunc main() {{ var r = R.Ok\n _ = r }}"), "has fields");
    err(&format!("{src}\nfunc F(r R) {{ switch r {{ case R.Fail(x): return }} }}\nfunc main() {{}}"), "has 2 field(s)");
    err(&format!("{src}\nfunc F(r R) {{ switch r {{ case C.Red: return }} }}\nfunc main() {{}}"), "switch pattern type mismatch");
}

#[test]
fn closures_capture_by_reference() {
    let p = ok(&prog("func Counter() func() int { var n = 0\n return func() int { n += 1\n return n } }\nfunc main() { println(Counter()()) }"));
    let c = func(&p, "Counter");
    assert!(c.locals[0].captured);
    let lambda = p.funcs.iter().find(|f| matches!(f.kind, FuncKind::Lambda { .. })).unwrap();
    assert_eq!(lambda.captures.len(), 1);
    assert_eq!(lambda.captures[0].source, CaptureSource::Local(LocalId(0)));
}

#[test]
fn defer_spills_arguments() {
    let p = ok(&main_body("var x = 1\ndefer println(\"x\", x)\nx = 2\nprintln(x)"));
    let stmts = &func(&p, "main").body.stmts;
    assert!(matches!(stmts[1].kind, StmtKind::Let { .. }), "argument spilled");
    let StmtKind::Defer(e) = &stmts[2].kind else { panic!() };
    // Open-coded defer (the default): the operand is the call itself over the
    // spilled DeferArg local; with ZELL_NO_OPEN_DEFER=1 it is a closure that
    // captures that local.
    if let ExprKind::Closure { captures, .. } = &e.kind {
        assert_eq!(captures.len(), 1);
    } else {
        assert!(matches!(e.kind, ExprKind::Print { .. } | ExprKind::Call { .. }), "deferred call: {:?}", e.kind);
    }
}

#[test]
fn generic_misuse_is_rejected() {
    // Generic interfaces are not supported; see tests/generics.rs for the rest.
    err(&prog("interface Box<T> { func Get() T }\nfunc main() {}"), "generic interfaces are not supported");
    err(&prog("func F(l List<int>) {}\nfunc main() {}"), "unknown type List");
    err(&main_body("var x = Id<int>(1)\n_ = x"), "unknown identifier Id");
}

#[test]
fn attributes_are_resolved_and_evaluated() {
    let src = prog(
        "@attribute(targets: [Target.Field])\nstruct range { min int\n max int\n message string = \"out of range\" }\n\
         @json(name: \"user\")\nstruct User { @range(min: 0, max: 150) Age int\n @range(1, 64) Name string\n @deprecated(\"use Y\")\n func X() {} }\nfunc main() {}",
    );
    let p = ok(&src);
    let user = p.structs.iter().find(|s| s.name == "User").unwrap();
    assert_eq!(user.attrs[0].kind, AttrKind::Builtin("json".into()));
    let age = &user.fields[0];
    assert!(matches!(age.attrs[0].kind, AttrKind::User(_)));
    assert_eq!(age.attrs[0].args, vec![("min".into(), ConstValue::Int(0)), ("max".into(), ConstValue::Int(150)), ("message".into(), ConstValue::Str("out of range".into()))]);
    err(&prog("@nope\nstruct S {}\nfunc main() {}"), "unknown attribute @nope");
    err(&prog("@attribute(targets: [Target.Field])\nstruct r { min int }\n@r(1)\nstruct S {}\nfunc main() {}"), "cannot be applied to a struct");
    err(&prog("@attribute\nstruct r { min int }\nstruct S { @r(\"x\") A int }\nfunc main() {}"), "attribute argument has type string, want int");
    err(&prog("@attribute\nstruct r {\n    @required min int\n}\nstruct S { @r() A int }\nfunc main() {}"), "missing argument min");
    err(&prog("@attribute\nstruct r { min int }\nstruct S { @r(1) @r(2) A int }\nfunc main() {}"), "not repeatable");
}

#[test]
fn diagnostics_carry_file_index() {
    let files = vec![SourceFile::new("a.zl", "namespace a\nfunc F() {}"), SourceFile::new("b.zl", "namespace b\nfunc G() { nope() }")];
    let mut diags = Diagnostics::new();
    assert!(driver::check(&files, &mut diags).is_none());
    assert_eq!(diags.items[0].file, 1);
    assert!(diags.render_all_files(&files).contains("b.zl:2"));
}

#[test]
fn hir_dump_is_readable() {
    let p = ok(&main_body("println(\"hi\")"));
    let d = p.dump();
    assert!(d.contains("func #0 t.main"), "{d}");
    assert!(d.contains("print[Str \"hi\":string; Newline]"), "{d}");
}

// ---------------------------------------------------------------------------
// Memory: interior pointers, destructors, weak references
// ---------------------------------------------------------------------------

const MEM_DECLS: &str = "
struct P { x int\n y int }
struct L { a P\n b P\n ps []P\n arr [3]int }
var g P
func look(p *P) int { return p.x }
func keep(p *P) *P { return p }
func setp(l *L, p *P) { l.a = *p }
";

#[test]
fn interior_pointers_are_borrowed_only() {
    // Read through, compared and passed to non-escaping parameters: fine.
    ok(&prog(&format!("{MEM_DECLS}\nfunc main() {{\n var l = new L{{}}\n println(look(&l.a), look(&g), (&l.b).y)\n setp(l, &l.b)\n l.ps = append(l.ps, P{{}})\n println(look(&l.ps[0]))\n var s = l.arr\n println(s)\n}}")));
    // Kept in a local variable: allowed (it pins its owner, DESIGN.md
    // "Память"), but such a variable cannot be copied, returned or captured.
    ok(&prog(&format!("{MEM_DECLS}\nfunc main() {{\n var l = new L{{}}\n var p = &l.a\n println(p.x)\n}}")));
    err(&prog(&format!("{MEM_DECLS}\nfunc f(l *L) *P {{\n var p = &l.a\n return p\n}}\nfunc main() {{ println(f(new L{{}}).x) }}")), "cannot keep interior pointer variable `p`");
    err(&prog(&format!("{MEM_DECLS}\nfunc main() {{\n var l = new L{{}}\n var p = &l.a\n var f = func() int {{ return p.x }}\n println(f())\n}}")), "cannot be captured by a closure");
    // Returned through an escaping parameter, stored.
    err(&prog(&format!("{MEM_DECLS}\nfunc main() {{\n var l = new L{{}}\n println(keep(&l.a).x)\n}}")), "to parameter `p` of t.keep: the parameter escapes");
    err(&prog(&format!("{MEM_DECLS}\nfunc f() *P {{ return &g }}\nfunc main() {{ println(f().x) }}")), "cannot keep an interior pointer");
    err(&prog(&format!("{MEM_DECLS}\nstruct H {{ p *P }}\nfunc main() {{\n var l = new L{{}}\n var h = H{{p: &l.ps[0]}}\n println(h.p.x)\n}}")), "cannot keep an interior pointer");
    // A slice of an array reached through a pointer has no owner.
    err(&prog(&format!("{MEM_DECLS}\nfunc f(l *L) []int {{ return l.arr[0..2] }}\nfunc main() {{ println(f(new L{{}})) }}")), "a slice of an array that is not a variable");
}

#[test]
fn methods_storing_this_need_a_whole_receiver() {
    let decls = "
struct N { v int\n next *N\n func Link(o *N) { o.next = this } }
struct W { n N }
interface Linker { func Link(o *N) }
struct E { N }
";
    ok(&prog(&format!("{decls}\nfunc main() {{\n var a = new N{{}}\n var b = N{{}}\n b.Link(a)\n a.Link(a)\n println(a.next.v)\n}}")));
    err(&prog(&format!("{decls}\nfunc main() {{\n var w = new W{{}}\n var a = new N{{}}\n w.n.Link(a)\n}}")), "to parameter `this` of t.N.Link: the parameter escapes");
    err(&prog(&format!("{decls}\nfunc main() {{\n var e = E{{}}\n var l Linker = e\n l.Link(new N{{}})\n}}")), "promoted method t.N.Link lets `this` escape");
}

#[test]
fn destructors_are_checked() {
    let res = "struct R { name string\n func Drop() { println(this.name) } }\n";
    ok(&prog(&format!("{res}\nstruct H {{ r R }}\nfunc mk() R {{ return R{{name: \"x\"}} }}\nfunc take(r R) {{ println(r.name) }}\nfunc main() {{\n var a = R{{name: \"a\"}}\n take(mk())\n var h = new H{{r: mk()}}\n println(h.r.name)\n a = mk()\n var ps []*R\n ps = append(ps, new R{{}})\n println(len(ps))\n}}")));
    err(&prog(&format!("{res}\nfunc main() {{\n var a = R{{}}\n var b = a\n println(b.name)\n}}")), "cannot copy a value of type R");
    err(&prog(&format!("{res}\nfunc take(r R) {{}}\nfunc main() {{\n var a = R{{}}\n take(a)\n}}")), "cannot copy a value of type R");
    err(&prog(&format!("{res}\nfunc main() {{\n var rs = [R{{}}]\n println(len(rs))\n}}")), "cannot be stored as a slice of R");
    err(&prog(&format!("{res}\nfunc main() {{\n var a = R{{}}\n a.Drop()\n}}")), "Drop cannot be called directly");
    err(&prog("struct R { func Drop(x int) {} }\nfunc main() {}"), "must be declared as `func Drop()`");
    err(&prog("var keep []*R\nstruct R { func Drop() { keep = append(keep, this) } }\nfunc main() {}"), "must not let `this` escape");
    // A variable with a destructor counts as used.
    ok(&prog(&format!("{res}\nfunc main() {{\n var guard = R{{name: \"g\"}}\n}}")));
}

#[test]
fn weak_pointers_convert_both_ways() {
    let p = ok(&prog("struct N { name string\n parent weak *N }\nfunc main() {\n var a = new N{name: \"a\"}\n var b = new N{parent: a}\n var w weak *N = a\n w = nil\n b.parent = a\n var q = b.parent\n println(q == nil, b.parent.name, w == nil)\n}"));
    let m = func(&p, "main");
    let q = m.locals.iter().find(|l| l.name == "q").unwrap();
    assert!(matches!(p.types.get(q.ty), Type::Pointer(_)), "reading a weak pointer yields *T");
    let w = m.locals.iter().find(|l| l.name == "w").unwrap();
    assert!(matches!(p.types.get(w.ty), Type::Weak(_)));
    assert_eq!(p.type_name(w.ty), "weak *t.N");
    err(&prog("struct N { x int }\nfunc main() {\n var w weak *N = N{}\n}"), "cannot assign");
}

#[test]
fn parameter_modes() {
    use zell::hir::ownership::param_facts;
    let p = ok(&prog("struct N { next *N\n s string }\nfunc read(n *N, s string) int { return len(s) + len(n.s) }\nfunc store(a *N, b *N) { a.next = b }\nfunc walk(n *N) int {\n var c = 0\n for n != nil { c += 1\n n = n.next }\n return c\n}\nasync func later(n *N) int { return len(n.s) }\nfunc main() {\n var a = new N{}\n store(a, new N{})\n println(read(a, \"x\"), walk(a))\n var t = later(a)\n _ = t\n}"));
    let facts = param_facts(&p);
    let id = |name: &str| FuncId(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    assert!(!facts.escapes(id("read"), 0) && !facts.owned(id("read"), 0) && !facts.owned(id("read"), 1));
    assert!(!facts.escapes(id("store"), 0) && facts.escapes(id("store"), 1));
    assert!(facts.owned(id("walk"), 0) && facts.escapes(id("walk"), 0), "a reassigned parameter is owned");
    assert!(facts.owned(id("later"), 0) && facts.escapes(id("later"), 0), "async parameters are owned");
}

// ---------------------------------------------------------------------------
// Reference-cycle analysis (W0001, sema::cycles)
// ---------------------------------------------------------------------------

/// The W0001 warnings of a program (which must compile).
fn cycle_warnings(src: &str) -> Vec<String> {
    let files = vec![SourceFile::new("f0.zl".to_string(), src)];
    let mut diags = Diagnostics::new();
    let p = driver::check(&files, &mut diags);
    assert!(p.is_some(), "unexpected errors: {:#?}", diags.items);
    diags.items.iter().filter(|d| d.code == Some("W0001")).map(|d| d.message.clone()).collect()
}

fn cyclic(p: &Program, name: &str) -> bool {
    let sid = p.structs.iter().position(|s| s.name == name).expect("struct");
    let t = p.types.lookup(&Type::Struct(zell::types::DeclId(sid as u32))).unwrap();
    zell::sema::cycles::analyze(p).is_cyclic(t)
}

#[test]
fn w0001_pointer_self_and_mutual() {
    let w = cycle_warnings("namespace app\nstruct List {\n    V int\n    Next *List\n}\nstruct A {\n    B *B\n}\nstruct B {\n    A *A\n}\nstruct Leaf {\n    V int\n}\nstruct Tree {\n    L *Leaf\n}\nfunc main() {\n    var l = new List{}\n    var t = new Tree{}\n    var a = new A{}\n    l.V = 1\n    t.L = nil\n    a.B = nil\n}\n");
    assert_eq!(w.len(), 3, "{w:#?}");
    assert!(w.iter().any(|m| m == "W0001 type List can form a reference cycle through field Next (*List); consider weak *List or a Drop that breaks the cycle"), "{w:#?}");
    assert!(w.iter().any(|m| m.starts_with("W0001 type A can form a reference cycle through field B (*B)")), "{w:#?}");
    assert!(w.iter().any(|m| m.starts_with("W0001 type B can form a reference cycle through field A (*A)")), "{w:#?}");
}

#[test]
fn w0001_through_closures_interfaces_and_containers() {
    let src = "namespace app\nimport io\ninterface Handler {\n    func Run()\n}\nstruct Server {\n    H Handler\n}\nstruct App {\n    Srv *Server\n    func Run() { println(\"run\") }\n}\nstruct Button {\n    OnClick func()\n}\nstruct Graph {\n    Nodes map[string][]*Graph\n}\nstruct Plain {\n    F func()\n}\nfunc main() {\n    var app = new App{}\n    app.Srv = new Server{H: app}\n    var b = new Button{}\n    b.OnClick = func() { println(b == nil) }\n    var p = Plain{F: func() { println(1) }}\n    p.F()\n    var g = new Graph{}\n    println(g == nil)\n    defer func() { println(app == nil) }()\n}\n";
    let w = cycle_warnings(src);
    assert!(w.iter().any(|m| m.contains("type Server can form a reference cycle through field H (Handler)")), "{w:#?}");
    assert!(w.iter().any(|m| m.contains("type App can form a reference cycle through field Srv (*Server)")), "{w:#?}");
    assert!(w.iter().any(|m| m.contains("type Button can form a reference cycle through field OnClick (func())")), "{w:#?}");
    assert!(w.iter().any(|m| m.contains("type Graph can form a reference cycle through field Nodes (map[string][]*Graph)")), "{w:#?}");
    assert!(!w.iter().any(|m| m.contains("type Plain")), "{w:#?}");
    let p = ok(src);
    assert!(cyclic(&p, "Button") && cyclic(&p, "App") && !cyclic(&p, "Plain"));
}

#[test]
fn w0001_silenced_by_weak_allow_cycle_and_std() {
    let w = cycle_warnings("namespace app\nstruct Node {\n    Parent weak *Node\n    Children []*Node\n}\n@allowCycle\nstruct Ring {\n    Next *Ring\n}\nstruct E {\n    Inner error\n}\nfunc main() {\n    var n = new Node{}\n    var r = new Ring{}\n    var e = E{Inner: error(\"x\")}\n    n.Parent = nil\n    r.Next = r\n    println(e.Inner)\n}\n".replace("println(e.Inner)", "var m = e.Inner.Message()\n    m = m + \"\"").as_str());
    assert!(w.is_empty(), "{w:#?}");
    err("namespace app\n@allowCycle\nfunc main() {\n}\n", "@allowCycle can only be applied to a struct or an enum");
}

#[test]
fn deny_warnings_turns_w0001_into_an_error() {
    zell::sema::cycles::set_deny_warnings(true);
    let (p, errors) = analyze(&["namespace app\nstruct L {\n    N *L\n}\nfunc main() {\n    var l = new L{}\n    l.N = nil\n}\n"]);
    zell::sema::cycles::set_deny_warnings(false);
    assert!(p.is_none());
    assert!(errors.iter().any(|e| e.starts_with("W0001 type L")), "{errors:#?}");
}

#[test]
fn acyclic_types_are_not_cyclic_and_not_warned() {
    let src = "namespace app\n@acyclic\nstruct Tree {\n    L *Tree\n    R *Tree\n}\nstruct Ring {\n    Next *Ring\n}\nfunc main() {\n    var t = new Tree{}\n    var r = new Ring{}\n    t.L = nil\n    r.Next = nil\n}\n";
    let w = cycle_warnings(src);
    assert_eq!(w.len(), 1, "{w:#?}");
    assert!(w[0].contains("type Ring"), "{w:#?}");
    let p = ok(src);
    assert!(!cyclic(&p, "Tree") && cyclic(&p, "Ring"));
    err("namespace app\n@acyclic\nfunc main() {\n}\n", "@acyclic can only be applied to a struct or an enum");
}
