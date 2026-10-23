//! `zell test`: finds `@test` functions, generates a driver and runs it.
//!
//! Discovery parses every `.zl` file of the path (the real parser, no
//! type checking) and takes each top-level `@test` function:
//! `func TestX(t *testing.T)` (a test), `func BenchX(b *testing.B)` (a
//! benchmark) or `func X()` (a test without a handle), and each `@bench
//! func BenchX(b *testing.B)`. A test whose body has the statement
//! `t.Parallel()` (on its `*testing.T` parameter) is generated with
//! `Parallel: true` and runs concurrently with the other parallel tests.
//! Per namespace with
//! tests a file is generated that lists them as `testing.Case`s
//! (in the same namespace, so unexported test functions work too), plus a
//! driver namespace whose `main` awaits `testing.Main` (stdlib_src/
//! testing.zl), which runs each test in a child process and prints the
//! summary. Existing `main` functions are renamed in memory so the driver
//! is the entry point. The sources keep their paths, so diagnostics and
//! `@embed` work as in a normal build.
//!
//! Examples: every `func ExampleX()` of a `*_test.zl` file (see
//! [`zell::doc`]) is compiled into the same binary; the driver runs one
//! when started with `--zell-example <ns.Name>`. After the tests, each
//! example with a trailing `// Output:` comment is run that way and its
//! standard output compared with the comment (surrounding white space
//! trimmed; `// Unordered output:` compares the lines as a multiset).
//!
//! `zell test std` (or a directory inside the standard library, like
//! `zell test std/strings`) runs the tests and examples of the stdlib's
//! `*_test.zl` files: only those files are compiled as user code, the
//! modules themselves come from the compiler as in any build.

use zell::ast;
use zell::diagnostics::Diagnostics;
use zell::span::SourceFile;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What a `@test` function takes.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plain,
    Test,
    Bench,
}

const DRIVER_NS: &str = "zell_test_driver__";

/// Runs `zell test <args>`; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let mut path: Option<String> = None;
    let mut pass: Vec<String> = Vec::new();
    let mut build_args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--json" | "--bench" | "-v" | "--verbose" => pass.push(a.clone()),
            "--filter" | "--run" | "--benchtime" | "--parallel" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("error: {a} needs a value");
                    return 2;
                };
                pass.push(if a == "--run" { "--filter".into() } else { a.clone() });
                pass.push(v.clone());
                i += 1;
            }
            _ if a.starts_with('-') => build_args.push(a.clone()),
            _ if path.is_none() => path = Some(a.clone()),
            _ => build_args.push(a.clone()),
        }
        i += 1;
    }
    let mut root = PathBuf::from(path.unwrap_or_else(|| ".".into()));
    // `std` (all of it), `strings`, `net.http`: the standard library's test files
    if let Some(p) = root.to_str().filter(|p| *p == "std" || zell::sema::stdlib::is_std_namespace(p)).map(str::to_string) {
        if !root.exists() || p == "std" {
            match zell::sema::stdfs::std_dir() {
                Some(d) => root = p.split('.').filter(|_| p != "std").fold(d, |acc, seg| acc.join(seg)),
                None => {
                    eprintln!("error: the standard library sources were not found (set ZELL_STD)");
                    return 1;
                }
            }
        }
    }
    let std_mode = zell::sema::stdfs::std_dir().is_some_and(|d| {
        let d = d.canonicalize().unwrap_or(d);
        root.canonicalize().is_ok_and(|r| r.starts_with(&d))
    });
    let mut opts = match zell::build::parse_args(&build_args) {
        Ok((o, _)) => o,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    opts.input = Some(root.clone());
    let src_root = if root.join("src").is_dir() && (root.join("zell.toml").is_file() || root.join("zell.json").is_file() || root.join("zell.yaml").is_file()) {
        root.join("src")
    } else {
        root.clone()
    };
    let collected = zell::driver::collect_files(&src_root).map(|mut p| {
        if std_mode {
            p.retain(|f| zell::doc::is_test_file(&f.to_string_lossy()));
        }
        p
    });
    let paths = match collected {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            eprintln!("error: no .zl files in {}", src_root.display());
            return 1;
        }
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", src_root.display());
            return 1;
        }
    };
    let mut files = match zell::driver::load(&paths) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: cannot read source: {e}");
            return 1;
        }
    };
    let own_files = files.len();
    let mut tests: BTreeMap<String, Vec<(String, Kind, bool)>> = BTreeMap::new();
    let mut examples: Vec<(String, zell::doc::Example)> = Vec::new();
    for f in &mut files {
        let mut diags = Diagnostics::new();
        let tokens = zell::lexer::lex(&f.text, &mut diags);
        let Some(file) = zell::parser::parse(tokens, &mut diags) else { continue };
        if diags.has_errors() {
            continue;
        }
        let ns = file.namespace.to_string();
        if zell::doc::is_test_file(&f.path) {
            for e in zell::doc::examples_of(&file, &f.text, &f.path) {
                examples.push((format!("{ns}.{}", e.name), e));
            }
        }
        let mut mains: Vec<(usize, usize)> = Vec::new();
        for d in &file.decls {
            let ast::Decl::Func(func) = d else { continue };
            if func.name.name == "main" {
                mains.push((func.name.span.start as usize, func.name.span.end as usize));
                continue;
            }
            let is_test = func.attrs.iter().any(|a| a.path.to_string() == "test");
            let is_bench = func.attrs.iter().any(|a| a.path.to_string() == "bench");
            if !(is_test || is_bench) || !func.generics.is_empty() {
                continue;
            }
            let kind = match func.params.as_slice() {
                [] if !is_bench => Kind::Plain,
                [p] => match handle_type(&p.ty) {
                    Some("T") if !is_bench => Kind::Test,
                    Some("B") => Kind::Bench,
                    _ => continue,
                },
                _ => continue,
            };
            let parallel = kind == Kind::Test && calls_parallel(func);
            tests.entry(ns.clone()).or_default().push((func.name.name.clone(), kind, parallel));
        }
        // the driver's main is the entry point
        if !mains.is_empty() {
            let mut text = f.text.clone();
            for (s, e) in mains.into_iter().rev() {
                text.replace_range(s..e, "zellUserMain__");
            }
            *f = SourceFile::new(f.path.clone(), text);
        }
    }
    let total: usize = tests.values().map(|v| v.len()).sum();
    if total == 0 && examples.is_empty() {
        println!("no tests found in {}", src_root.display());
        return 0;
    }
    let tmp = std::env::temp_dir().join(format!("zell-test-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        eprintln!("error: cannot create {}: {e}", tmp.display());
        return 1;
    }
    let mut driver = format!("namespace {DRIVER_NS}\n\nasync func main() {{\n    var args = os.Args()\n    if len(args) == 3 && args[1] == \"--zell-example\" {{\n");
    for (full, e) in &examples {
        let call = if e.is_async { format!("await {full}()") } else { format!("{full}()") };
        let call = if e.fallible { format!("try {call} catch err {{\n                io.Eprintln(err.Message())\n                os.Exit(1)\n            }}") } else { call };
        driver.push_str(&format!("        if args[2] == \"{full}\" {{\n            {call}\n        }}\n"));
    }
    driver.push_str("        return\n    }\n    var cases []testing.Case = []\n");
    for (k, (ns, list)) in tests.iter().enumerate() {
        let mut gen = format!("namespace {ns}\n\n// generated by `zell test`\nfunc ZellTestCases{k}__() []testing.Case {{\n    var out []testing.Case = []\n");
        for (name, kind, parallel) in list {
            let case = match kind {
                Kind::Test if *parallel => format!("testing.Case{{Name: \"{name}\", Run: {name}, Parallel: true}}"),
                Kind::Test => format!("testing.Case{{Name: \"{name}\", Run: {name}}}"),
                Kind::Plain => format!("testing.Case{{Name: \"{name}\", Run: func(t *testing.T) {{\n        {name}()\n    }}}}"),
                Kind::Bench => format!("testing.Case{{Name: \"{name}\", Bench: {name}}}"),
            };
            gen.push_str(&format!("    out = append(out, {case})\n"));
        }
        gen.push_str("    return out\n}\n");
        files.push(SourceFile::new(tmp.join(format!("tests_{k}.zl")).display().to_string(), gen));
        driver.push_str(&format!("    for c in {ns}.ZellTestCases{k}__() {{\n        cases = append(cases, c)\n    }}\n"));
    }
    driver.push_str("    os.Exit(await testing.Main(cases))\n}\n");
    files.push(SourceFile::new(tmp.join("driver.zl").display().to_string(), driver));
    let exe = tmp.join("tests");
    let code = match zell::build::build_test_sources(files, own_files, &exe, &opts) {
        Ok(exe) => {
            let tests_code = if total == 0 {
                0
            } else {
                match std::process::Command::new(&exe).args(&pass).status() {
                    Ok(st) => st.code().unwrap_or(1),
                    Err(e) => {
                        eprintln!("error: cannot run {}: {e}", exe.display());
                        1
                    }
                }
            };
            let ex_code = run_examples(&exe, &examples, &pass);
            if tests_code != 0 {
                tests_code
            } else {
                ex_code
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    };
    let _ = std::fs::remove_dir_all(&tmp);
    code
}

/// The `--filter` rule of testing: alternatives separated by `|`,
/// each a substring with optional `^` / `$` anchors.
fn filter_matches(filter: &str, name: &str) -> bool {
    filter.is_empty()
        || filter.split('|').any(|alt| {
            let (pre, alt) = alt.strip_prefix('^').map_or((false, alt), |a| (true, a));
            let (suf, alt) = alt.strip_suffix('$').map_or((false, alt), |a| (true, a));
            match (pre, suf) {
                (true, true) => name == alt,
                (true, false) => name.starts_with(alt),
                (false, true) => name.ends_with(alt),
                _ => name.contains(alt),
            }
        })
}

/// Normalizes example output for comparison: trimmed, with trailing
/// spaces of each line removed; sorted lines when `unordered`.
fn normalize_output(s: &str, unordered: bool) -> String {
    let mut lines: Vec<&str> = s.trim().lines().map(str::trim_end).collect();
    if unordered {
        lines.sort_unstable();
    }
    lines.join("\n")
}

/// Runs every example that has an expected output; returns the exit code.
fn run_examples(exe: &std::path::Path, examples: &[(String, zell::doc::Example)], pass: &[String]) -> i32 {
    let verbose = pass.iter().any(|a| a == "-v" || a == "--verbose");
    let json = pass.iter().any(|a| a == "--json");
    let filter = pass.iter().position(|a| a == "--filter").and_then(|i| pass.get(i + 1)).cloned().unwrap_or_default();
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let (mut passed, mut failed) = (0, 0);
    let start = std::time::Instant::now();
    for (full, e) in examples {
        let Some(want) = &e.output else { continue };
        if !filter_matches(&filter, &e.name) {
            continue;
        }
        let t0 = std::time::Instant::now();
        let out = std::process::Command::new(exe).arg("--zell-example").arg(full).env("ZELL_NO_BACKTRACE", "1").output();
        let secs = t0.elapsed().as_secs_f64();
        let (ok, report) = match out {
            Err(e) => (false, format!("    cannot run: {e}\n")),
            Ok(o) => {
                let got = String::from_utf8_lossy(&o.stdout).to_string();
                if !o.status.success() {
                    (false, format!("    exit status {:?}\n{}", o.status.code(), String::from_utf8_lossy(&o.stderr)))
                } else if normalize_output(&got, e.unordered) == normalize_output(want, e.unordered) {
                    (true, String::new())
                } else {
                    let ind = |s: &str| s.trim().lines().map(|l| format!("    {l}\n")).collect::<String>();
                    (false, format!("got:\n{}want:\n{}", ind(&got), ind(want)))
                }
            }
        };
        if ok {
            passed += 1;
        } else {
            failed += 1;
        }
        let status = if ok { "PASS" } else { "FAIL" };
        if json {
            println!("{{\"Action\":\"run\",\"Test\":{}}}", quote(&e.name));
            if !report.is_empty() {
                println!("{{\"Action\":\"output\",\"Test\":{},\"Output\":{}}}", quote(&e.name), quote(&report));
            }
            println!("{{\"Action\":\"{}\",\"Test\":{},\"Elapsed\":{secs:.3}}}", status.to_lowercase(), quote(&e.name));
        } else if verbose || !ok {
            if verbose {
                println!("=== RUN   {}", e.name);
            }
            print!("{report}");
            println!("--- {status}: {} ({secs:.2}s)  {}:{}", e.name, e.file, e.line);
        }
    }
    if passed + failed == 0 {
        return 0;
    }
    if !json {
        let word = if failed == 0 { "ok  " } else { "FAIL" };
        println!("{word}\t{passed} examples passed, {failed} failed ({:.2}s)", start.elapsed().as_secs_f64());
    }
    i32::from(failed > 0)
}

/// True if the body of test `func` has the statement `t.Parallel()` on
/// its handle parameter (also nested in blocks, not in lambdas).
fn calls_parallel(func: &ast::FuncDecl) -> bool {
    fn in_block(b: &ast::Block, t: &str) -> bool {
        b.stmts.iter().any(|s| match &s.kind {
            ast::StmtKind::Expr(e) => match &e.kind {
                ast::ExprKind::Call { callee, args } if args.is_empty() => {
                    matches!(&callee.kind, ast::ExprKind::Field { base, name } if name.name == "Parallel" && matches!(&base.kind, ast::ExprKind::Ident(i) if i.name == t))
                }
                _ => false,
            },
            ast::StmtKind::Block(b) => in_block(b, t),
            ast::StmtKind::If(i) => in_block(&i.then, t),
            _ => false,
        })
    }
    let (Some(p), Some(body)) = (func.params.first(), &func.body) else { return false };
    in_block(body, &p.name.name)
}

/// `T` / `B` for a parameter typed `*testing.T` / `*testing.B` (any
/// path ending in T or B under a `testing` segment, or bare `*T`).
fn handle_type(t: &ast::TypeExpr) -> Option<&'static str> {
    let ast::TypeKind::Pointer(inner) = &t.kind else { return None };
    let ast::TypeKind::Named { path, generics } = &inner.kind else { return None };
    if !generics.is_empty() {
        return None;
    }
    let segs: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
    let last = *segs.last()?;
    let ok = segs.len() == 1 || segs[segs.len() - 2] == "testing";
    match (last, ok) {
        ("T", true) => Some("T"),
        ("B", true) => Some("B"),
        _ => None,
    }
}
