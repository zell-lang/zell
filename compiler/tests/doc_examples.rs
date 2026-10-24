//! The example projects of `docs/examples/*/` (the pages of the
//! documentation's Examples section) are real Zell projects whose README
//! shows every file in full. For each project this test checks that:
//!
//! 1. every block headed by a `<!-- file: PATH -->` line in `README.md` is
//!    byte-for-byte the file `PATH` of the project, and every `.zl` file of
//!    the project is shown that way (so the page can never drift from the
//!    code);
//! 2. `zell run` in the project prints exactly `expected.out`;
//! 3. `zell test` passes when the project has `*_test.zl` files (in a
//!    workspace, `zell test <member>` for each member package);
//! 4. `zell run --leak-check` finishes without leaking.
//!
//! The project is copied to a temporary directory first, so runs never
//! write `target/` or `zell.lock` into the repository. Proxy variables
//! are removed from the environment: the web example talks to itself on
//! 127.0.0.1. `DOC_EXAMPLES_FILTER=name` runs one project.
//!
//! Needs `clang` and `runtime/libzell_rt.a` (built with `make -C runtime`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn ensure_runtime() {
    if !repo().join("runtime/libzell_rt.a").is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
}

/// Every project directory under `docs/examples`, sorted.
fn projects() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(repo().join("docs/examples")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.join("zell.toml").is_file()).collect();
    out.sort();
    out
}

/// Files of a project relative to its root, skipping build output.
fn files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                if name != "target" && name != ".zell" {
                    walk(root, &p, out);
                }
            } else if name != "zell.lock" {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The `<!-- file: PATH -->` blocks of a README: (path, line, contents).
fn readme_blocks(text: &str) -> Result<Vec<(String, usize, String)>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i].trim();
        if let Some(rest) = l.strip_prefix("<!-- file:") {
            let path = rest.strip_suffix("-->").ok_or(format!("README.md:{}: malformed file marker", i + 1))?.trim().to_string();
            let fence = lines.get(i + 1).copied().unwrap_or("");
            if !fence.starts_with("```") {
                return Err(format!("README.md:{}: the marker for {path} must be followed by a ``` fence", i + 1));
            }
            let ticks = fence.chars().take_while(|&c| c == '`').count();
            let close = "`".repeat(ticks);
            let start = i + 2;
            let end = (start..lines.len()).find(|&j| lines[j] == close).ok_or(format!("README.md:{}: unterminated block for {path}", i + 2))?;
            let mut body = lines[start..end].join("\n");
            body.push('\n');
            out.push((path, i + 1, body));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    Ok(out)
}

/// The first line where two texts differ, for a readable failure.
fn first_diff(want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    for i in 0..w.len().max(g.len()) {
        let (a, b) = (w.get(i), g.get(i));
        if a != b {
            return format!("first difference at line {}:\n  file:   {:?}\n  README: {:?}", i + 1, a.unwrap_or(&"<end>"), b.unwrap_or(&"<end>"));
        }
    }
    "the texts differ only in their line endings or final newline".into()
}

fn check_readme(root: &Path) -> Vec<String> {
    let mut errs = Vec::new();
    let text = match std::fs::read_to_string(root.join("README.md")) {
        Ok(t) => t,
        Err(e) => return vec![format!("README.md: {e}")],
    };
    let blocks = match readme_blocks(&text) {
        Ok(b) => b,
        Err(e) => return vec![e],
    };
    for (path, line, body) in &blocks {
        match std::fs::read_to_string(root.join(path)) {
            Ok(disk) if disk == *body => {}
            Ok(disk) => errs.push(format!("README.md:{line}: the block for {path} differs from the file; {}", first_diff(&disk, body))),
            Err(e) => errs.push(format!("README.md:{line}: {path}: {e}")),
        }
    }
    for f in files(root) {
        if f.ends_with(".zl") && !blocks.iter().any(|(p, _, _)| *p == f) {
            errs.push(format!("{f} is not shown in README.md (add a `<!-- file: {f} -->` block)"));
        }
    }
    errs
}

fn copy_project(root: &Path, to: &Path) {
    for f in files(root) {
        let dst = to.join(&f);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(root.join(&f), dst).unwrap();
    }
}

fn zell(dir: &Path, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_zell"));
    c.args(args).current_dir(dir).env("ZELL_NO_BACKTRACE", "1").env("NO_PROXY", "localhost,127.0.0.1").env("no_proxy", "localhost,127.0.0.1");
    for v in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
        c.env_remove(v);
    }
    c.output().unwrap()
}

fn text(o: &Output) -> String {
    format!("--- stdout:\n{}--- stderr:\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// Runs the checks of one project; each failure is one message.
fn check_project(root: &Path) -> Vec<String> {
    let name = root.file_name().unwrap().to_string_lossy().to_string();
    let mut errs = check_readme(root);
    let dir = std::env::temp_dir().join(format!("zell-doc-example-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_project(root, &dir);

    let o = zell(&dir, &["run"]);
    match std::fs::read_to_string(root.join("expected.out")) {
        Err(e) => errs.push(format!("expected.out: {e}")),
        Ok(want) => {
            let got = String::from_utf8_lossy(&o.stdout);
            if !o.status.success() {
                errs.push(format!("zell run failed ({:?}):\n{}", o.status.code(), text(&o)));
            } else if got != want {
                errs.push(format!("zell run: output differs from expected.out; {}\n{}", first_diff(&want, &got).replace("README", "output"), text(&o)));
            }
        }
    }
    // `zell test` works on one package, so a workspace root (a manifest
    // with `[workspace]`) is tested member by member: `zell test <dir>`
    // for every direct subdirectory that has a manifest and tests.
    let workspace = std::fs::read_to_string(root.join("zell.toml")).is_ok_and(|m| m.lines().any(|l| l.trim() == "[workspace]"));
    let members: Vec<String> = if workspace {
        let mut m: Vec<String> = std::fs::read_dir(root).unwrap().map(|e| e.unwrap().path()).filter(|p| p.join("zell.toml").is_file()).map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        m.sort();
        m
    } else {
        vec![String::new()]
    };
    for m in &members {
        if files(&root.join(m)).iter().any(|f| f.ends_with("_test.zl")) {
            let o = if m.is_empty() { zell(&dir, &["test"]) } else { zell(&dir, &["test", m]) };
            if !o.status.success() {
                errs.push(format!("zell test {m} failed:\n{}", text(&o)));
            }
        }
    }
    let o = zell(&dir, &["run", "--leak-check"]);
    if !o.status.success() || String::from_utf8_lossy(&o.stderr).contains("leak:") {
        errs.push(format!("zell run --leak-check failed ({:?}):\n{}", o.status.code(), String::from_utf8_lossy(&o.stderr)));
    }
    let _ = std::fs::remove_dir_all(&dir);
    errs.into_iter().map(|e| format!("docs/examples/{name}: {e}")).collect()
}

#[test]
fn doc_example_projects_match_readme_and_run() {
    ensure_runtime();
    let filter = std::env::var("DOC_EXAMPLES_FILTER").unwrap_or_default();
    let all: Vec<PathBuf> = projects().into_iter().filter(|p| p.to_string_lossy().contains(&filter)).collect();
    assert!(filter != "" || all.len() >= 5, "only {} example projects found in docs/examples", all.len());
    let failures = std::sync::Mutex::new(Vec::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..3 {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(p) = all.get(i) else { break };
                let errs = check_project(p);
                failures.lock().unwrap().extend(errs);
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{} problem(s) in {} example project(s):\n\n{}", failures.len(), all.len(), failures.join("\n\n"));
}
