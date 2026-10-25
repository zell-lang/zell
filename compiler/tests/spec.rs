//! Specification corpus runner: `tests/spec/<area>/**`.
//!
//! Every test is a single `.zl` file (with a sibling `.out` unless it is a
//! compile-fail test) or a directory holding several `.zl` files plus one
//! `.out` file. Directives live in the leading `//` comment lines, before
//! `namespace` (for a directory: in its first `.zl` file by name):
//!
//! | directive | meaning |
//! |-----------|---------|
//! | `// test: <text>` | one-line description (ignored by the runner) |
//! | `// error: <keyword>` | compile-fail test: build must fail and stderr must contain `<keyword>` |
//! | `// status: passing\|pending` | required; `pending` tests must currently FAIL |
//! | `// exit: N` | expected exit code of the program (default 0) |
//! | `// args: a b` | program arguments (split on whitespace) |
//! | `// run-flags: --x` | use `zell run <src> <flags> -- <args>` instead of build + exec |
//! | `// stderr: <text>` | stderr must contain `<text>` (repeatable; stderr is otherwise not checked) |
//! | `// no-stderr: <text>` | stderr must not contain `<text>` (repeatable) |
//! | `// warning: <text>` | the build's stderr must contain `<text>` (repeatable; e.g. a W0001 warning) |
//! | `// no-warning: <text>` | the build's stderr must not contain `<text>` (repeatable) |
//! | `// env: K=V` | environment variable of the program run (repeatable; `K=` removes `K`, e.g. `ZELL_NO_BACKTRACE=` to check a panic backtrace) |
//! | `// build-flags: --x` | extra `zell build` flags (also for compile-fail tests) |
//! | `// os: linux macos` | run only on these host operating systems (`std::env::consts::OS`); skipped elsewhere |
//!
//! `SPEC_FILTER=<substring>` restricts the run to tests whose path (relative
//! to the repository, e.g. `tests/spec/errors/trace_basic.zl`) contains it.
//! `SPEC_VERBOSE=1` prints every test result.
//!
//! Needs `clang` and `runtime/libzell_rt.a` (built with `make -C runtime`
//! when missing), like `run_examples.rs`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Every directory directly under `tests/spec`. `areas_are_complete` fails
/// when a directory is missing from this list.
const AREAS: &[&str] = &[
    "basics", "control", "structs", "interfaces", "enums", "errors", "generics", "memory", "async", "attributes", "reflection", "stdlib",
    "namespaces", "switch", "closures", "pointers", "interfaces2", "generics2", "memory2", "errors2", "async2", "strings2",
    "diagnostics", "programs", "attributes2", "platform", "stdlib2", "stdlib3", "stdlib4", "operators", "iteration",
    "stdlib5", "panics",
    "data",
    "memory3",
    "lints",
];

/// Per-process timeout for a built program (a pending feature may hang).
const RUN_TIMEOUT: Duration = Duration::from_secs(20);

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn zell() -> &'static str {
    env!("CARGO_BIN_EXE_zell")
}

fn ensure_runtime() {
    let lib = repo().join("runtime/libzell_rt.a");
    if !lib.is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
}

#[derive(Debug, Default, Clone)]
struct Directives {
    error: Option<String>,
    pending: bool,
    exit: i32,
    args: Vec<String>,
    run_flags: Vec<String>,
    stderr: Vec<String>,
    no_stderr: Vec<String>,
    warnings: Vec<String>,
    no_warnings: Vec<String>,
    env: Vec<(String, String)>,
    build_flags: Vec<String>,
    /// Host operating systems the test applies to (empty: all).
    os: Vec<String>,
}

#[derive(Debug)]
struct Case {
    /// Path relative to the repository root, with `/` separators.
    rel: String,
    /// `.zl` file or test directory.
    src: PathBuf,
    /// Expected stdout (`None` for compile-fail tests).
    out: Option<PathBuf>,
    dirs: Directives,
}

fn parse_directives(file: &Path) -> Result<Directives, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let mut d = Directives::default();
    let mut status = None;
    for line in text.lines() {
        let Some(c) = line.trim().strip_prefix("//") else { break };
        let c = c.trim();
        let Some((key, val)) = c.split_once(':') else { continue };
        let val = val.trim();
        match key.trim() {
            "error" => d.error = Some(val.to_string()),
            "status" => status = Some(val.to_string()),
            "exit" => d.exit = val.parse().map_err(|_| format!("{}: bad exit directive `{val}`", file.display()))?,
            "args" => d.args = val.split_whitespace().map(String::from).collect(),
            "run-flags" => d.run_flags = val.split_whitespace().map(String::from).collect(),
            "stderr" => d.stderr.push(val.to_string()),
            "no-stderr" => d.no_stderr.push(val.to_string()),
            "warning" => d.warnings.push(val.to_string()),
            "no-warning" => d.no_warnings.push(val.to_string()),
            "build-flags" => d.build_flags = val.split_whitespace().map(String::from).collect(),
            "os" => d.os = val.split_whitespace().map(String::from).collect(),
            "env" => match val.split_once('=') {
                Some((k, v)) => d.env.push((k.trim().to_string(), v.trim().to_string())),
                None => return Err(format!("{}: bad env directive `{val}` (expected K=V)", file.display())),
            },
            _ => {}
        }
    }
    match status.as_deref() {
        Some("passing") => d.pending = false,
        Some("pending") => d.pending = true,
        Some(s) => return Err(format!("{}: bad status `{s}` (expected passing or pending)", file.display())),
        None => return Err(format!("{}: missing `// status:` directive", file.display())),
    }
    Ok(d)
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    v.sort();
    v
}

fn rel(p: &Path) -> String {
    p.strip_prefix(repo()).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Collects the cases of one area: every `.zl` file is one test, and every
/// subdirectory is one multi-file test (all its `.zl` files are compiled
/// together; it holds exactly one `.out` file unless it is a compile-fail
/// test, and its directives are read from its first `.zl` file by name).
fn collect(dir: &Path, cases: &mut Vec<Case>, problems: &mut Vec<String>) {
    for p in sorted_entries(dir) {
        if p.is_dir() {
            let entries = sorted_entries(&p);
            let outs: Vec<&PathBuf> = entries.iter().filter(|f| f.extension().is_some_and(|x| x == "out")).collect();
            let Some(first) = entries.iter().find(|f| f.extension().is_some_and(|x| x == "zl")) else {
                problems.push(format!("{}: no .zl files", rel(&p)));
                continue;
            };
            let d = match parse_directives(first) {
                Ok(d) => d,
                Err(e) => {
                    problems.push(e);
                    continue;
                }
            };
            let out = match (d.error.is_some(), outs.as_slice()) {
                (true, []) => None,
                (true, _) => {
                    problems.push(format!("{}: compile-fail test must not have a .out file", rel(&p)));
                    continue;
                }
                (false, [o]) => Some((*o).clone()),
                (false, _) => {
                    problems.push(format!("{}: a multi-file test needs exactly one .out file", rel(&p)));
                    continue;
                }
            };
            cases.push(Case { rel: rel(&p), src: p.clone(), out, dirs: d });
        } else if p.extension().is_some_and(|x| x == "zl") {
            let d = match parse_directives(&p) {
                Ok(d) => d,
                Err(e) => {
                    problems.push(e);
                    continue;
                }
            };
            let o = p.with_extension("out");
            let out = if d.error.is_some() {
                if o.exists() {
                    problems.push(format!("{}: compile-fail test must not have a .out file", rel(&p)));
                }
                None
            } else if o.is_file() {
                Some(o)
            } else {
                problems.push(format!("{}: missing {}", rel(&p), rel(&o)));
                continue;
            };
            cases.push(Case { rel: rel(&p), src: p.clone(), out, dirs: d });
        } else if !p.extension().is_some_and(|x| x == "out") {
            problems.push(format!("{}: unexpected file (only .zl, .out and test directories belong here)", rel(&p)));
        }
    }
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs `cmd` with a timeout, capturing stdout and stderr.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("cannot spawn: {e}"))?;
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let to = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let te = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = String::from_utf8_lossy(&to.join().unwrap()).to_string();
    let stderr = String::from_utf8_lossy(&te.join().unwrap()).to_string();
    Ok(Output { code: status.code(), stdout, stderr })
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zell-spec-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Checks one case against its expectation. `Ok(())` means the
/// expectation holds (the feature works), `Err(why)` that it does not.
fn check(case: &Case) -> Result<(), String> {
    let tmp = tmpdir(&case.rel.replace('/', "_"));
    let exe = tmp.join("prog");
    let rel_src = case.src.strip_prefix(repo()).unwrap_or(&case.src).to_path_buf();
    let result = (|| {
        if let Some(kw) = &case.dirs.error {
            let mut c = Command::new(zell());
            c.current_dir(repo()).arg("build").arg(&rel_src).arg("-o").arg(&exe).args(&case.dirs.build_flags);
            let o = run_with_timeout(c, Duration::from_secs(60))?;
            if o.code == Some(0) {
                return Err(format!("expected a compile error containing `{kw}`, but the build succeeded"));
            }
            if !o.stderr.contains(kw.as_str()) {
                return Err(format!("build failed, but stderr does not contain `{kw}`:\n{}", o.stderr.trim_end()));
            }
            return Ok(());
        }
        let check_warnings = |build_stderr: &str| -> Result<(), String> {
            for w in &case.dirs.warnings {
                if !build_stderr.contains(w.as_str()) {
                    return Err(format!("build stderr does not contain the warning `{w}`:\n{}", build_stderr.trim_end()));
                }
            }
            for w in &case.dirs.no_warnings {
                if build_stderr.contains(w.as_str()) {
                    return Err(format!("build stderr contains `{w}`:\n{}", build_stderr.trim_end()));
                }
            }
            Ok(())
        };
        let out = if case.dirs.run_flags.is_empty() {
            let mut c = Command::new(zell());
            c.current_dir(repo()).arg("build").arg(&rel_src).arg("-o").arg(&exe).args(&case.dirs.build_flags);
            let b = run_with_timeout(c, Duration::from_secs(60))?;
            if b.code != Some(0) {
                return Err(format!("build failed:\n{}", b.stderr.trim_end()));
            }
            check_warnings(&b.stderr)?;
            let mut c = Command::new(&exe);
            c.current_dir(repo()).env("ZELL_NO_BACKTRACE", "1").args(&case.dirs.args);
            for (k, v) in &case.dirs.env {
                if v.is_empty() {
                    c.env_remove(k);
                } else {
                    c.env(k, v);
                }
            }
            run_with_timeout(c, RUN_TIMEOUT)?
        } else {
            let mut c = Command::new(zell());
            c.current_dir(repo()).env("ZELL_NO_BACKTRACE", "1").arg("run").arg(&rel_src).args(&case.dirs.run_flags).arg("--").args(&case.dirs.args);
            for (k, v) in &case.dirs.env {
                if v.is_empty() {
                    c.env_remove(k);
                } else {
                    c.env(k, v);
                }
            }
            let o = run_with_timeout(c, RUN_TIMEOUT + Duration::from_secs(60))?;
            check_warnings(&o.stderr)?;
            o
        };
        let expected = std::fs::read_to_string(case.out.as_ref().unwrap()).unwrap();
        let mut why = Vec::new();
        if out.code != Some(case.dirs.exit) {
            why.push(format!("exit code {:?}, expected {}", out.code, case.dirs.exit));
        }
        if out.stdout != expected {
            why.push(format!("stdout differs\n--- expected\n{expected}--- got\n{}", out.stdout));
        }
        for s in &case.dirs.stderr {
            if !out.stderr.contains(s.as_str()) {
                why.push(format!("stderr does not contain `{s}`"));
            }
        }
        for s in &case.dirs.no_stderr {
            if out.stderr.contains(s.as_str()) {
                why.push(format!("stderr contains `{s}`"));
            }
        }
        if why.is_empty() {
            Ok(())
        } else {
            if !out.stderr.is_empty() {
                why.push(format!("stderr:\n{}", out.stderr.trim_end()));
            }
            Err(why.join("\n"))
        }
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn run_area(area: &str) {
    ensure_runtime();
    let dir = repo().join("tests/spec").join(area);
    assert!(dir.is_dir(), "{} not found", dir.display());
    let mut cases = Vec::new();
    let mut problems = Vec::new();
    collect(&dir, &mut cases, &mut problems);
    let filter = std::env::var("SPEC_FILTER").ok().filter(|f| !f.is_empty());
    if let Some(f) = &filter {
        cases.retain(|c| c.rel.contains(f.as_str()));
    }
    let before = cases.len();
    cases.retain(|c| c.dirs.os.is_empty() || c.dirs.os.iter().any(|o| o == std::env::consts::OS));
    let other_os = before - cases.len();
    let verbose = std::env::var("SPEC_VERBOSE").is_ok_and(|v| v != "0" && !v.is_empty());

    // A few workers per area; cargo already runs the areas in parallel.
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<(usize, Result<(), String>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if i >= cases.len() {
                            break;
                        }
                        mine.push((i, check(&cases[i])));
                    }
                    mine
                })
            })
            .collect();
        let mut all: Vec<_> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_by_key(|(i, _)| *i);
        all
    });

    let (mut passing, mut pending, mut compile_fail) = (0, 0, 0);
    let mut unexpected = problems.clone();
    for (i, r) in results {
        let c = &cases[i];
        let kind = if c.dirs.error.is_some() { "compile-fail" } else { "run" };
        if c.dirs.error.is_some() {
            compile_fail += 1;
        }
        match (c.dirs.pending, r) {
            (false, Ok(())) => {
                passing += 1;
                if verbose {
                    eprintln!("ok       {} ({kind})", c.rel);
                }
            }
            (false, Err(why)) => unexpected.push(format!("{} FAILED ({kind}):\n{why}", c.rel)),
            (true, Ok(())) => unexpected.push(format!("{} now passes: change status to passing", c.rel)),
            (true, Err(why)) => {
                pending += 1;
                if verbose {
                    eprintln!("pending  {} ({kind}): {}", c.rel, why.lines().next().unwrap_or(""));
                }
            }
        }
    }
    eprintln!(
        "spec {area}: {} tests ({compile_fail} compile-fail): {passing} passing, {pending} pending, {} unexpected, {other_os} for other systems",
        cases.len(),
        unexpected.len()
    );
    assert!(unexpected.is_empty(), "spec {area}: {} unexpected result(s):\n\n{}", unexpected.len(), unexpected.join("\n\n"));
}

macro_rules! areas {
    ($($name:ident = $area:literal),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                run_area($area);
            }
        )*
    };
}

areas!(
    spec_basics = "basics",
    spec_control = "control",
    spec_structs = "structs",
    spec_interfaces = "interfaces",
    spec_enums = "enums",
    spec_errors = "errors",
    spec_generics = "generics",
    spec_memory = "memory",
    spec_async = "async",
    spec_attributes = "attributes",
    spec_reflection = "reflection",
    spec_stdlib = "stdlib",
    spec_namespaces = "namespaces",
    spec_switch = "switch",
    spec_closures = "closures",
    spec_pointers = "pointers",
    spec_interfaces2 = "interfaces2",
    spec_generics2 = "generics2",
    spec_memory2 = "memory2",
    spec_errors2 = "errors2",
    spec_async2 = "async2",
    spec_panics = "panics",
    spec_strings2 = "strings2",
    spec_diagnostics = "diagnostics",
    spec_programs = "programs",
    spec_attributes2 = "attributes2",
    spec_platform = "platform",
    spec_stdlib2 = "stdlib2",
    spec_stdlib3 = "stdlib3",
    spec_stdlib4 = "stdlib4",
    spec_operators = "operators",
    spec_iteration = "iteration",
    spec_stdlib5 = "stdlib5",
    spec_data = "data",
    spec_memory3 = "memory3",
    spec_lints = "lints",
);

#[test]
fn areas_are_complete() {
    let root = repo().join("tests/spec");
    for p in sorted_entries(&root) {
        if p.is_dir() {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(AREAS.contains(&name.as_str()), "tests/spec/{name} is not listed in AREAS in compiler/tests/spec.rs");
        }
    }
    for a in AREAS {
        assert!(root.join(a).is_dir(), "tests/spec/{a} missing");
    }
}
