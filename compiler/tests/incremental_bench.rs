//! Timing of incremental analysis on a generated workspace (docs/TOOLING.md,
//! "Performance"): 50 namespaces x 20 files x ~200 lines, layered (each
//! namespace imports the one below it and the base namespace with a generic
//! function and a `TypesWith` attribute). Ignored by default:
//!
//! ```sh
//! cargo test --release --test incremental_bench -- --ignored --nocapture
//! ```
//!
//! `BENCH_NS`, `BENCH_FILES` change the size; `ZELL_TIME=1` prints phases.

use std::time::Instant;
use zell::session::Session;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn time<R>(what: &str, f: impl FnOnce() -> R) -> R {
    let t = Instant::now();
    let r = f();
    println!("{what:<40} {:>8.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    r
}

#[test]
#[ignore]
fn incremental_bench() {
    let (ns, nf) = (env("BENCH_NS", 50), env("BENCH_FILES", 20));
    let mut s = Session::new();
    let mut lines = 0;
    time("parse all files", || {
        for k in 0..ns {
            for j in 0..nf {
                let (p, t) = zell::session::synthetic_file(k, j);
                let p = format!("/bench/{p}");
                lines += t.lines().count();
                s.update_file(p, t);
            }
        }
    });
    println!("{ns} namespaces x {nf} files, {lines} lines");
    let a = time("initial analysis", || s.analyze());
    let errs = a.canonical().iter().filter(|l| l.starts_with("diag")).count();
    println!("diagnostics: {errs} ({:?})", s.last_stats());
    for l in a.canonical().iter().filter(|l| l.starts_with("diag")).take(3) {
        println!("  {l}");
    }
    drop(a);
    let mid = ns / 2;
    let (p, t) = zell::session::synthetic_file(mid, nf / 2);
    let p = format!("/bench/{p}");
    for (what, text) in [
        ("body edit (middle namespace)", t.replace("s -= 1", "s -= 2")),
        ("body edit again", t.replace("s -= 1", "s -= 3")),
        ("signature edit (middle namespace)", t.replace("func Bump(n int) int", "func Bump(n int) int64").replace("this.Id += n\n        return this.Id", "this.Id += n\n        return int64(this.Id)")),
        ("revert", t.clone()),
    ] {
        time(&format!("{what}: update_file"), || s.update_file(p.clone(), text));
        let a = time(&format!("{what}: analyze"), || s.analyze());
        println!("  {:?}", s.last_stats());
        drop(a);
    }
    let (_, tn) = zell::session::synthetic_file(mid, nf);
    time("new file: update_file", || s.update_file(format!("/bench/m{mid}/new.zl"), tn));
    let a = time("new file: analyze", || s.analyze());
    println!("  {:?}", s.last_stats());
    drop(a);
    let (p0, t0) = zell::session::synthetic_file(ns - 1, 0);
    let p0 = format!("/bench/{p0}");
    s.update_file(p0, t0.replace("s -= 1", "s -= 4"));
    let a = time("body edit (top namespace): analyze", || s.analyze());
    println!("  {:?}", s.last_stats());
    drop(a);
    // `BENCH_LOOP=n`: n more body edits (for a profiler).
    let n = env("BENCH_LOOP", 0);
    if n > 0 {
        let start = Instant::now();
        for i in 0..n {
            s.update_file(p.clone(), t.replace("s -= 1", &format!("s -= {}", i + 10)));
            let a = s.analyze();
            std::thread::spawn(move || drop(a));
        }
        println!("body edit x{n}: {:.1} ms each", start.elapsed().as_secs_f64() * 1000.0 / n as f64);
    }
}
