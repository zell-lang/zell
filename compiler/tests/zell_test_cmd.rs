//! `zell test` end to end on `tests/fixtures/testing` (testing).

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn zell_test(args: &[&str]) -> (i32, String) {
    zell_test_env(args, &[])
}

fn zell_test_env(args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let lib = repo().join("runtime/libzell_rt.a");
    if !lib.is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
    let out = Command::new(env!("CARGO_BIN_EXE_zell"))
        .arg("test")
        .arg("compiler/tests/fixtures/testing")
        .args(args)
        .current_dir(repo())
        .env("ZELL_NO_BACKTRACE", "1")
        .envs(env.iter().copied())
        .output()
        .expect("cannot run zell");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).to_string())
}

#[test]
fn reports_failures_and_summary() {
    let (code, out) = zell_test(&[]);
    assert_eq!(code, 1, "{out}");
    for want in [
        "    about to fail\n    got 4, want 5\n    got \"a\", want \"b\"\n--- FAIL: TestFailing (",
        "    stop here\n--- FAIL: TestFatalStops (",
        // A panic fails only its test, and the report has its message.
        "panic: runtime error: index out of range [3] with length 0\n",
        "    (exit status 2)\n--- FAIL: TestPanics (",
        "    expected a panic, but the function returned\n--- FAIL: TestPanicsMissing (",
        "FAIL\nFAIL\t6 passed, 4 failed, 1 skipped (",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(!out.contains("never printed"), "{out}");
    assert!(!out.contains("TestAdd"), "passing tests are quiet without -v:\n{out}");
}

#[test]
fn filter_verbose_and_subtests() {
    let (code, out) = zell_test(&["-v", "--filter", "^TestAdd$|plain"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("=== RUN   TestAdd\n    === RUN   TestAdd/1+2\n    --- PASS: TestAdd/1+2 ("), "{out}");
    assert!(out.contains("--- PASS: plainTest ("), "{out}");
    assert!(out.contains("ok  \t2 passed, 0 failed, 0 skipped ("), "{out}");
}

#[test]
fn json_events() {
    let (code, out) = zell_test(&["--json", "--filter", "TestDiv|TestSkipped"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("{\"Action\":\"run\",\"Test\":\"TestDiv\"}\n{\"Action\":\"pass\",\"Test\":\"TestDiv\",\"Elapsed\":"), "{out}");
    assert!(out.contains("{\"Action\":\"skip\",\"Test\":\"TestSkipped\""), "{out}");
}

#[test]
fn benchmarks() {
    let (code, out) = zell_test(&["--bench", "--filter", "Bench", "--benchtime", "20"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("BenchAdd ") && out.contains(" ns/op ") && out.contains(" allocs/op"), "{out}");
    // `@bench func` is discovered like `@test func X(b *testing.B)`.
    assert!(out.contains("BenchMul "), "{out}");
}

#[test]
fn panics_are_caught_in_process() {
    let (code, out) = zell_test(&["-v", "--filter", "TestPanicsOK"]);
    assert_eq!(code, 0, "{out}");
    // the function panics on a thread of its own; the test runs once
    assert_eq!(out.matches("after the probes").count(), 1, "{out}");
    assert!(out.contains("--- PASS: TestPanicsOK ("), "{out}");
}

#[test]
fn panics_probe_runs_in_a_child() {
    // The fallback where panics cannot be caught (Windows), forced here.
    let (code, out) = zell_test_env(&["-v", "--filter", "TestPanicsOK"], &[("ZELL_TEST_PANICS", "process")]);
    assert_eq!(code, 0, "{out}");
    // the test's own output appears once: the probe children are silent
    assert_eq!(out.matches("after the probes").count(), 1, "{out}");
    assert!(out.contains("--- PASS: TestPanicsOK ("), "{out}");
}

#[test]
fn parallel_tests_overlap_and_run_after_sequential_ones() {
    let (code, out) = zell_test(&["-v", "--parallel", "2", "--filter", "TestParallel|TestDiv"]);
    assert_eq!(code, 0, "{out}");
    // the runner's own elapsed time (not the build): two 2s tests in
    // parallel; sequentially they would take 4s or more
    let summary = out.lines().find(|l| l.starts_with("ok  \t")).expect("summary line");
    let secs: f64 = summary.rsplit('(').next().unwrap().trim_end_matches(')').trim_end_matches('s').parse().expect("elapsed seconds");
    assert!(secs < 3.8, "took {secs}s:\n{out}");
    // the sequential TestDiv (declared earlier) runs first anyway
    let div = out.find("=== RUN   TestDiv").expect("TestDiv ran");
    let a = out.find("=== RUN   TestParallelA").expect("TestParallelA ran");
    let b = out.find("=== RUN   TestParallelB").expect("TestParallelB ran");
    assert!(div < a && a < b, "{out}");
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dst = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dst);
        } else {
            std::fs::copy(&p, &dst).unwrap();
        }
    }
}

/// A package with a path dependency (`tests/fixtures/testpkg`, copied so
/// the lock file is written outside the repository): the dependency is
/// built, its `*_test.zl` (a second `Database`) is not, `async @test`
/// functions are awaited, and the renamed `main` is not linted as unused.
#[test]
fn package_with_dependency_and_async_tests() {
    let lib = repo().join("runtime/libzell_rt.a");
    if !lib.is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
    let tmp = std::env::temp_dir().join(format!("zell-testpkg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy_dir(&repo().join("compiler/tests/fixtures/testpkg"), &tmp);
    let out = Command::new(env!("CARGO_BIN_EXE_zell")).arg("test").arg(tmp.join("app")).arg("-v").env("ZELL_NO_BACKTRACE", "1").output().expect("cannot run zell");
    let _ = std::fs::remove_dir_all(&tmp);
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    for want in ["--- PASS: TestNine (", "--- PASS: TestAsyncNine (", "    got 9, want 10\n--- FAIL: TestAsyncFails (", "--- PASS: asyncPlain (", "FAIL\t3 passed, 1 failed, 0 skipped ("] {
        assert!(stdout.contains(want), "missing {want:?} in:\n{stdout}{stderr}");
    }
    assert!(!stderr.contains("zellUserMain__"), "{stderr}");
}
