//! Expression trees (DESIGN.md "Деревья выражений"): the cost of building
//! one. The node structure of a lambda site is a constant (`expr.Site` in
//! read-only data); only captured values live in a per-evaluation block.
//! A capture-free tree allocates nothing, a tree with captures one box
//! (shared by the compiled closure and the value/type accessors), and the
//! captured variables of the creating function stay plain stack locals.
//! Behaviour is covered by `tests/spec/expr`.

use std::process::Command;
use zell::diagnostics::{Diagnostics, Severity};
use zell::driver;
use zell::hir::*;
use zell::span::SourceFile;

const SRC: &str = r#"namespace app

import io

struct User {
    Name string
    Age int
}

func build0() expr<func(*User) bool> {
    return u => u.Age > 18 && strings.HasPrefix(u.Name, "A")
}

func build1(n int, s string) expr<func(*User) bool> {
    return u => u.Age > n && u.Name != s && len(u.Name) > n
}

func main() {
    var a = build0()
    var b = build1(3, "x")
    var u = User{Name: "Ann", Age: 20}
    println(a.Compile()(&u), b.Compile()(&u))
}
"#;

fn func<'p>(p: &'p Program, name: &str) -> &'p Function {
    p.funcs.iter().find(|f| f.full_name == name).unwrap_or_else(|| panic!("no function {name}"))
}

/// The `EnvClosures` of the tree returned by `f`.
fn env_closures(f: &Function) -> (Vec<FuncId>, Option<Expr>) {
    let Some(StmtKind::Return(ReturnValue::Value(e))) = f.body.stmts.last().map(|s| &s.kind) else { panic!("{}: no return", f.name) };
    let ExprKind::StructLit { fields, .. } = &e.kind else { panic!("{}: not a struct literal: {:?}", f.name, e.kind) };
    for (_, x) in fields {
        if let ExprKind::EnvClosures { funcs, env } = &x.kind {
            return (funcs.clone(), env.as_deref().cloned());
        }
    }
    panic!("{}: no EnvClosures", f.name)
}

#[test]
fn tree_sites_capture_values_in_one_block() {
    let files = vec![SourceFile::new("t.zl".to_string(), SRC.to_string())];
    let mut diags = Diagnostics::new();
    let p = driver::check(&files, &mut diags);
    let errors: Vec<_> = diags.items.iter().filter(|d| d.severity == Severity::Error).map(|d| d.message.clone()).collect();
    assert!(errors.is_empty(), "{errors:#?}");
    let p = p.unwrap();
    // No captures: no environment, closures over NULL.
    let (fs0, env0) = env_closures(func(&p, "app.build0"));
    assert_eq!(fs0.len(), 3);
    assert!(env0.is_none());
    assert!(fs0.iter().all(|f| p.funcs[f.0 as usize].value_env.is_none() && p.funcs[f.0 as usize].captures.is_empty()));
    // Two captured variables (n read twice): one tuple of two values.
    let b1 = func(&p, "app.build1");
    let (fs1, env1) = env_closures(b1);
    let env1 = env1.expect("an environment");
    let ExprKind::Tuple(vs) = &env1.kind else { panic!("env is not a tuple") };
    assert_eq!(vs.len(), 2, "one slot per captured variable");
    assert!(fs1.iter().all(|f| p.funcs[f.0 as usize].value_env == Some(env1.ty)));
    // The creating function's parameters are read, not captured into cells.
    assert!(b1.locals.iter().all(|l| !l.captured), "a local of build1 lives in a cell");
}

/// Calls of allocation functions in the body of `@<symbol>(`.
fn allocations(ir: &str, symbol: &str) -> usize {
    let head = format!("@{symbol}(");
    let mut inside = false;
    let mut n = 0;
    for l in ir.lines() {
        if l.starts_with("define") && l.contains(&head) {
            inside = true;
        }
        if inside {
            n += ["@zell_object_new(", "@zell_cycles_new_entry(", "@zell_alloc("].iter().filter(|f| l.contains(*f)).count();
            if l == "}" {
                break;
            }
        }
    }
    n
}

#[test]
fn building_a_tree_allocates_at_most_the_capture_block() {
    let dir = std::env::temp_dir().join(format!("zell-exprtree-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("t.zl");
    std::fs::write(&src, SRC).unwrap();
    let ll = dir.join("t.ll");
    let r = Command::new(env!("CARGO_BIN_EXE_zell")).arg("build").arg(&src).arg("--emit=llvm-ir").arg("-o").arg(&ll).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let ir = std::fs::read_to_string(&ll).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(allocations(&ir, "zell_app__build0"), 0, "a capture-free tree allocates");
    assert_eq!(allocations(&ir, "zell_app__build1"), 1, "a tree with captures allocates more than its block");
}
