//! End-to-end tests: build every runnable example with `zell build`, run
//! it and compare stdout byte for byte with its `.out` file.
//!
//! Every example is built with `--leak-check`, so it must also release every
//! heap object before exiting.
//!
//! Needs `clang` and `runtime/libzell_rt.a` (built with `make -C runtime`;
//! the tests run `make` themselves if the archive is missing).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Examples that are known not to work yet, with the reason. Each entry is
/// skipped (and reported) instead of failing the test.
///
/// Currently empty: all of hello, arith, control, functions, structs,
/// switch, enums, slices_maps, strings, interfaces, errors, defer, bank,
/// async and multi/ pass byte-exact.
const SKIP: &[(&str, &str)] = &[];

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

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zell-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// (name, source path, expected output path) of every runnable example.
fn examples() -> Vec<(String, PathBuf, PathBuf)> {
    let dir = repo().join("examples");
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "zl") {
            let o = p.with_extension("out");
            if o.is_file() {
                out.push((p.file_stem().unwrap().to_string_lossy().to_string(), p.clone(), o));
            }
        } else if p.is_dir() {
            // A directory with a `.out` file is one multi-file program.
            let outs: Vec<PathBuf> = std::fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()).filter(|f| f.extension().is_some_and(|x| x == "out")).collect();
            if let [o] = outs.as_slice() {
                out.push((p.file_name().unwrap().to_string_lossy().to_string(), p.clone(), o.clone()));
            }
        }
    }
    out.sort();
    out
}

fn build_and_run(name: &str, src: &Path, extra: &[&str]) -> Result<String, String> {
    let dir = tmpdir(&format!("{name}{}", extra.join("")));
    let exe = dir.join(name);
    let b = Command::new(zell()).arg("build").arg(src).arg("-o").arg(&exe).args(extra).output().unwrap();
    if !b.status.success() {
        return Err(format!("build failed:\n{}", String::from_utf8_lossy(&b.stderr)));
    }
    let r = Command::new(&exe).env("ZELL_NO_BACKTRACE", "1").output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    if !r.status.success() {
        return Err(format!("exit status {:?}, stderr:\n{}", r.status.code(), String::from_utf8_lossy(&r.stderr)));
    }
    if !r.stderr.is_empty() {
        return Err(format!("unexpected stderr:\n{}", String::from_utf8_lossy(&r.stderr)));
    }
    Ok(String::from_utf8_lossy(&r.stdout).to_string())
}

fn check_all(extra: &[&str]) {
    ensure_runtime();
    let mut failures = Vec::new();
    let all = examples();
    assert!(all.len() >= 15, "example corpus not found");
    for (name, src, out) in all {
        if let Some((_, why)) = SKIP.iter().find(|(n, _)| *n == name) {
            eprintln!("skipping {name}: {why}");
            continue;
        }
        let expected = std::fs::read_to_string(&out).unwrap();
        match build_and_run(&name, &src, extra) {
            Ok(got) if got == expected => {}
            Ok(got) => failures.push(format!("{name}: output differs\n--- expected\n{expected}--- got\n{got}")),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    assert!(failures.is_empty(), "{} example(s) failed ({extra:?}):\n{}", failures.len(), failures.join("\n"));
}

// Every example must also free every object it allocated: `--leak-check`
// makes the program exit non-zero (with "leak: N objects" on stderr) if any
// object is still alive at exit.

#[test]
fn examples_debug() {
    check_all(&["--leak-check"]);
}

#[test]
fn examples_release() {
    check_all(&["--release", "--leak-check"]);
}

#[test]
fn compile_errors_fail_to_build() {
    let dir = repo().join("examples/errors_compile");
    let tmp = tmpdir("errors");
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let r = Command::new(zell()).arg("build").arg(&p).arg("-o").arg(tmp.join("x")).output().unwrap();
        assert!(!r.status.success(), "{} should not compile", p.display());
        assert!(String::from_utf8_lossy(&r.stderr).contains("error"), "{}: no error diagnostic", p.display());
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn emit_llvm_ir_is_valid() {
    let tmp = tmpdir("ir");
    let ll = tmp.join("hello.ll");
    let r = Command::new(zell()).arg("build").arg(repo().join("examples/hello.zl")).arg("--emit=llvm-ir").arg("-o").arg(&ll).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let text = std::fs::read_to_string(&ll).unwrap();
    assert!(text.contains("define i32 @main(i32"), "no C main in:\n{text}");
    // Re-parse and verify the module, with llvm-as if present, else with
    // inkwell's own parser + verifier.
    let tool = ["llvm-as-18", "llvm-as"].into_iter().find(|t| Command::new(t).arg("--version").output().is_ok_and(|o| o.status.success()));
    match tool {
        Some(t) => {
            let st = Command::new(t).arg(&ll).arg("-o").arg(tmp.join("hello.bc")).status().unwrap();
            assert!(st.success(), "{t} rejected the emitted IR");
        }
        None => {
            let ctx = inkwell::context::Context::create();
            let buf = inkwell::memory_buffer::MemoryBuffer::create_from_file(&ll).unwrap();
            let m = ctx.create_module_from_ir(buf).expect("IR does not parse");
            m.verify().expect("IR does not verify");
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Never-suspending async functions are compiled as plain functions
/// (ABI.md 10.3): `chain` of await_chain has no coroutine parts, its
/// yielding twin keeps them; `--dump-hir` marks the lowered ones.
#[test]
fn never_suspending_async_is_plain() {
    let tmp = tmpdir("plain");
    let ll = tmp.join("ac.ll");
    let src = repo().join("benchmarks/await_chain/main.zl");
    let r = Command::new(zell()).arg("build").arg(&src).arg("--emit=llvm-ir").arg("-o").arg(&ll).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let text = std::fs::read_to_string(&ll).unwrap();
    assert!(!text.contains("@zell_bench__await_chain__chain.resume"), "chain is still a coroutine");
    assert!(text.contains("@zell_bench__await_chain__chainYield.resume"), "chainYield must stay a coroutine");
    assert!(text.contains("\"zell-plain-async\""), "no zell-plain-async attribute");
    let dump = |p: &str| {
        let r = Command::new(zell()).arg("check").arg("--dump-hir").arg(repo().join(p)).output().unwrap();
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        String::from_utf8_lossy(&r.stdout).into_owned()
    };
    let d = dump("benchmarks/await_chain/main.zl");
    assert!(d.contains("async(plain) func #0 bench.await_chain.chain "), "{d}");
    assert!(d.contains("async func #1 bench.await_chain.chainYield "), "{d}");
    let d = dump("tests/spec/async2/plain_lowering_mixed.zl");
    for (name, plain) in [("ping ", false), ("pong ", false), ("even ", true), ("odd ", true), ("step<int> ", true), ("step<string> ", false), ("steps<int> ", true), ("steps<string> ", false)] {
        let line = d.lines().find(|l| l.contains(&format!("plain_lowering_mixed.{name}"))).unwrap_or_else(|| panic!("no {name} in {d}"));
        assert_eq!(line.starts_with("async(plain) "), plain, "{line}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn emit_obj_and_run_with_args() {
    ensure_runtime();
    let tmp = tmpdir("obj");
    let o = tmp.join("hello.o");
    let r = Command::new(zell()).arg("build").arg(repo().join("examples/hello.zl")).arg("--emit=obj").arg("-o").arg(&o).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert!(std::fs::metadata(&o).unwrap().len() > 0);
    let r = Command::new(zell()).arg("run").arg(repo().join("examples/hello.zl")).arg("-O2").arg("--").arg("x").output().unwrap();
    assert!(r.status.success());
    assert_eq!(String::from_utf8_lossy(&r.stdout), "Hello, Zell!\n");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn leak_check_reports_live_objects() {
    ensure_runtime();
    let tmp = tmpdir("leak");
    let src = tmp.join("cycle.zl");
    // A strong cycle: never freed by reference counting (a `weak` back
    // pointer would fix it); the cycle collector frees it at exit unless
    // it is told not to run (ZELL_CYCLES=manual).
    std::fs::write(&src, "namespace app\n\nstruct Node {\n    next *Node\n}\n\nfunc main() {\n    var a = new Node{}\n    var b = new Node{next: a}\n    a.next = b\n}\n").unwrap();
    let cases: [(&[&str], Option<&str>); 3] = [(&["--leak-check"], Some("  2 x app.Node\n")), (&["--leak-check", "--release"], None), (&[], None)];
    for (flags, names) in cases {
        let exe = tmp.join("cycle");
        let b = Command::new(zell()).arg("build").arg(&src).arg("-o").arg(&exe).args(flags).output().unwrap();
        assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
        let r = Command::new(&exe).env("ZELL_CYCLES", "manual").output().unwrap();
        let stderr = String::from_utf8_lossy(&r.stderr);
        if flags.is_empty() {
            assert!(r.status.success() && stderr.is_empty(), "{stderr}");
            continue;
        }
        // With the collector on (the default), the cycle is freed.
        let auto = Command::new(&exe).output().unwrap();
        assert!(auto.status.success() && auto.stderr.is_empty(), "{flags:?}: {}", String::from_utf8_lossy(&auto.stderr));
        assert!(stderr.contains("2 of them in unreachable reference cycles the cycle collector can free"), "{stderr}");
        assert_eq!(r.status.code(), Some(66), "{flags:?}: {stderr}");
        assert!(stderr.starts_with("leak: 2 objects\n"), "{flags:?}: {stderr}");
        match names {
            Some(n) => assert!(stderr.contains(n), "{stderr}"),
            None => assert!(!stderr.contains(" x "), "{stderr}"),
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
