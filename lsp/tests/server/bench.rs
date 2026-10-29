//! End-to-end timing of an edit in a large generated workspace
//! (`zell::session::synthetic_file`; docs/TOOLING.md, "Performance"): the
//! time from `didChange` to the answer of a request that waits for the new
//! analysis, then `semanticTokens/full` of the edited file. Ignored by
//! default:
//!
//! ```sh
//! cargo test --release --test server bench -- --ignored --nocapture
//! ```
//!
//! `BENCH_NS`, `BENCH_FILES` change the size.

use super::*;
use std::time::Instant;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
#[ignore]
fn lsp_edit_latency_in_a_large_workspace() {
    let (ns, nf) = (env("BENCH_NS", 50), env("BENCH_FILES", 20));
    let dir = std::env::temp_dir().join(format!("zell-lsp-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for k in 0..ns {
        for j in 0..nf {
            let (p, t) = zell::session::synthetic_file(k, j);
            let p = dir.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        }
    }
    let (rel, text) = zell::session::synthetic_file(ns / 2, nf / 2);
    let uri = Url::from_file_path(dir.join(rel)).unwrap();
    let t = Instant::now();
    let mut c = Client::start(&dir);
    c.open(&uri, &text);
    assert_eq!(c.diagnostics(&uri), vec![]);
    println!("initialize + first analysis: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    let tokens = |c: &mut Client| {
        c.request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
    };
    let edits: Vec<(&str, String)> = vec![
        ("body edit", text.replace("s -= 1", "s -= 2")),
        ("body edit", text.replace("s -= 1", "s -= 3")),
        ("body edit", text.replace("s -= 1", "s -= 4")),
        ("signature edit", text.replace("func Bump(n int) int", "func Bump(n int) int64").replace("return this.Id\n", "return int64(this.Id)\n")),
        ("revert", text.clone()),
    ];
    for (i, (what, new)) in edits.into_iter().enumerate() {
        let t = Instant::now();
        c.change(&uri, i as i32 + 2, &new);
        let d = c.diagnostics(&uri);
        let analysed = t.elapsed();
        tokens(&mut c);
        println!("{what:<16} diagnostics {:>6.1} ms, + semantic tokens {:>6.1} ms ({} diagnostics)", analysed.as_secs_f64() * 1000.0, t.elapsed().as_secs_f64() * 1000.0, d.len());
    }
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}
