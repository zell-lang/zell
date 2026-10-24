//! `zell doc` and the documentation toolchain end to end: terminal and
//! JSON output, the coverage check of the standard library, the static
//! sites (`--html`, `--site`) with a link checker, and `zell test std`
//! (the stdlib's runnable examples) including a failing example.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn zell(args: &[&str]) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_zell")).args(args).current_dir(repo()).env("ZELL_NO_BACKTRACE", "1").output().expect("cannot run zell");
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string())
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zell-doc-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ensure_runtime() {
    if !repo().join("runtime/libzell_rt.a").is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
}

#[test]
fn terminal_output_of_a_symbol_and_a_namespace() {
    let (code, out, err) = zell(&["doc", "strings.Split"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("func Split(s string, sep string) []string\n"), "{out}");
    assert!(out.contains("Split slices s into all substrings") && out.contains("std/strings/strings_extern.zl:"), "{out}");
    assert!(out.contains("Example:\n") && out.contains("    // Output:\n    // 3 [a b c]"), "{out}");
    let (code, out, _) = zell(&["doc", "time"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("namespace time  // std/time") && out.contains("func Date(year int"), "{out}");
    // a method, through the last segment of the namespace
    let (code, out, _) = zell(&["doc", "http.Client.Do"]);
    assert_eq!(code, 0);
    assert!(out.contains("func Do(method string, url string"), "{out}");
    let (code, _, err) = zell(&["doc", "strings.NoSuchThing"]);
    assert_eq!(code, 1);
    assert!(err.contains("no such namespace or symbol"), "{err}");
}

#[test]
fn json_output() {
    let (code, out, _) = zell(&["doc", "strings", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["name"], "strings");
    let split = v["items"].as_array().unwrap().iter().find(|i| i["name"] == "Split").unwrap();
    assert_eq!(split["kind"], "func");
    assert!(split["summary"].as_str().unwrap().starts_with("Split slices s"));
    assert!(!split["examples"].as_array().unwrap().is_empty());
}

#[test]
fn std_documentation_coverage_is_at_least_90_percent() {
    let (code, out, _) = zell(&["doc", "--check", "std", "--min", "90"]);
    let last = out.lines().last().unwrap_or("");
    assert_eq!(code, 0, "{last}");
    assert!(last.starts_with("documented ") && last.contains("examples"), "{last}");
    assert!(!out.contains("no symbol"), "an example names a missing symbol:\n{out}");
}

#[test]
fn project_docs_private_and_check() {
    let d = tmp("project");
    std::fs::write(d.join("zell.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nnamespace = \"demo\"\n").unwrap();
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/lib.zl"), "// Package demo does things.\nnamespace demo\n\n// Add adds.\nfunc Add(a int, b int) int {\n    return a + b\n}\n\nfunc Sub(a int, b int) int {\n    return a - b\n}\n\nfunc helper() {}\n").unwrap();
    let p = d.to_str().unwrap();
    let (code, out, _) = zell(&["doc", p]);
    assert_eq!(code, 0);
    assert!(out.contains("namespace demo") && out.contains("Package demo does things.") && out.contains("func Add(a int, b int) int") && !out.contains("helper"), "{out}");
    let (_, out, _) = zell(&["doc", p, "--private"]);
    assert!(out.contains("func helper()"), "{out}");
    let (code, out, _) = zell(&["doc", "--check", p]);
    assert_eq!(code, 1);
    assert!(out.contains("demo.Sub: missing doc comment") && out.contains("documented 1/2 exported symbols (50.0%)"), "{out}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn std_examples_run_under_zell_test() {
    ensure_runtime();
    let (code, out, err) = zell(&["test", "std"]);
    assert_eq!(code, 0, "{out}\n{err}");
    let last = out.lines().last().unwrap_or("");
    assert!(last.starts_with("ok") && last.contains("examples passed, 0 failed"), "{out}");
    let n: usize = last.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(n >= 60, "only {n} std examples");
}

#[test]
fn a_wrong_example_output_fails() {
    ensure_runtime();
    let d = tmp("example");
    std::fs::write(
        d.join("calc_test.zl"),
        "namespace calc_test\n\nimport io\n\nfunc ExampleGood() {\n    println(1 + 1)\n    // Output: 2\n}\n\nfunc ExampleBad() {\n    println(\"two\")\n    // Output:\n    // three\n}\n\nfunc ExampleCompiledOnly() {\n    println(\"not run\")\n}\n",
    )
    .unwrap();
    let (code, out, err) = zell(&["test", d.to_str().unwrap(), "-v"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("--- PASS: ExampleGood") && out.contains("got:\n    two\nwant:\n    three\n--- FAIL: ExampleBad"), "{out}");
    assert!(!out.contains("ExampleCompiledOnly") && out.contains("1 examples passed, 1 failed"), "{out}");
    let _ = std::fs::remove_dir_all(&d);
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn html_reference_of_the_stdlib() {
    let d = tmp("html");
    let (code, _, err) = zell(&["doc", "std", "--html", d.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let strings = read(&d.join("strings.html"));
    assert!(strings.contains("id=\"Split\"") && strings.contains("<pre class=\"sig\">"), "no Split section");
    assert!(strings.contains("<details id=\"ExampleSplit\">"), "no example");
    assert!(strings.contains("href=\"src/std/strings/strings_extern.zl.html#L"), "no source link");
    let idx = read(&d.join("search-index.json"));
    assert!(idx.contains("\"strings.Split\""));
    let bad = zell::doc::site::check_links(&d);
    assert!(bad.is_empty(), "broken links: {:?}", &bad[..bad.len().min(20)]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn documentation_site_links_resolve() {
    let d = tmp("site");
    let (code, _, err) = zell(&["doc", "--site", d.to_str().unwrap(), "--base", "/zell/"]);
    assert_eq!(code, 0, "{err}");
    for f in ["index.html", "guide/tour.html", "guide/faq.html", "design/DESIGN.html", "design/index.html", "std/index.html", "std/strings.html", "404.html", "style.css", "search-index.js", ".nojekyll"] {
        assert!(d.join(f).is_file(), "missing {f}");
    }
    let bad = zell::doc::site::check_links(&d);
    assert!(bad.is_empty(), "{} broken links, e.g. {:?}", bad.len(), &bad[..bad.len().min(20)]);
    // the landing page is the README; guides have prev/next
    let index = read(&d.join("index.html"));
    assert!(index.contains("href=\"guide/tour.html\"") && index.contains("<span class=\"k\">namespace</span>"), "landing page");
    let errors = read(&d.join("guide/errors.html"));
    assert!(errors.contains("class=\"pager\"") && errors.contains("href=\"tour.html\">&larr;") && errors.contains("href=\"async.html\""), "pager");
    // 404 uses absolute links under the base
    assert!(read(&d.join("404.html")).contains("href=\"/zell/style.css\""));
    // the std index lists every std namespace with documentation
    let std_index = read(&d.join("std/index.html"));
    let docs = zell::doc::Docs::build(&zell::doc::load_std(), zell::doc::Options::default());
    for n in docs.namespaces.iter().filter(|n| n.std && !(n.items.is_empty() && n.examples.is_empty() && n.doc.is_empty())) {
        assert!(std_index.contains(&format!("href=\"{}.html\"", n.name)), "std/index.html does not list {}", n.name);
    }
    // one search index over guides, design docs and the API
    let idx = read(&d.join("search-index.json"));
    assert!(idx.contains("\"guide/tour.html\"") && idx.contains("\"design/DESIGN.html\"") && idx.contains("\"std/strings.html#Split\""));
    // one sidebar with the three groups on every kind of page, the current page marked
    for (page, cur) in [("index.html", ""), ("guide/errors.html", "errors.html"), ("design/DESIGN.html", "DESIGN.html"), ("std/net.http.html", "net.http.html"), ("std/index.html", "index.html"), ("design/index.html", "index.html")] {
        let h = read(&d.join(page));
        assert!(h.contains("<nav class=\"side\" id=\"side\" aria-label=\"Documentation\">"), "{page}: no sidebar");
        for g in ["guide", "design", "std"] {
            assert!(h.contains(&format!("<details class=\"grp\" data-key=\"{g}\"")), "{page}: no {g} group");
        }
        assert!(h.contains("class=\"skip\"") && h.contains("id=\"menu-toggle\"") && h.contains("src=\"") && h.contains("nav.js"), "{page}: chrome");
        if !cur.is_empty() {
            assert!(h.contains(&format!("<a href=\"{cur}\" aria-current=\"page\">")), "{page}: current page not marked");
        }
    }
    // the group of the current page is open, the others are closed
    let tour = read(&d.join("guide/tour.html"));
    assert!(tour.contains("data-key=\"guide\" open data-cur>") && tour.contains("data-key=\"design\"><summary>") && tour.contains("data-key=\"std\"><summary>"));
    // the std tree: a root namespace holds its children, a leaf is a plain link
    let http = read(&d.join("std/net.http.html"));
    assert!(http.contains("data-key=\"std-net\" open data-cur><summary>net</summary>") && http.contains("href=\"net.tls.html\">net.tls</a>") && http.contains("href=\"strings.html\">strings</a>"));
    // "On this page" under the current link, from the h2/h3 of the page
    let errors = read(&d.join("guide/errors.html"));
    assert!(errors.contains("aria-current=\"page\">Errors</a><ol class=\"toc\"") && errors.contains("<a href=\"#panics\">Panics</a>"), "on this page");
    assert!(errors.contains("id=\"rtoc\""), "right column");
    let api_toc = http.split("<ol class=\"toc\"").nth(1).unwrap_or("");
    assert!(api_toc.contains("href=\"#sec-functions\"") && api_toc.contains("href=\"#sec-types\""), "api toc");
    assert!(d.join("nav.js").is_file());
    let _ = std::fs::remove_dir_all(&d);
}
