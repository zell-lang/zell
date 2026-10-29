//! Incremental analysis (`zell::session`) must be observably identical
//! to a full analysis: after every edit of a random edit sequence over the
//! `examples/` and `tests/spec/` corpora, the diagnostics, symbols (with
//! their types), references, file facts and namespace members of
//! `Session::analyze` equal those of `Session::analyze_full`.
//!
//! `INCR_EDITS=<n>` sets the number of edits per corpus, `INCR_SEED=<n>`
//! the seed of the edit sequence.

use zell::session::Session;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn collect(d: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "zl") {
            out.push(p);
        }
    }
}

/// xorshift64*
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One random edit of `text` (number `i`).
fn edit(rng: &mut Rng, text: &str, original: &str, i: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let body_lines: Vec<usize> = (0..lines.len()).filter(|&l| lines[l].starts_with("    ") && !lines[l].trim().is_empty()).collect();
    match rng.below(9) {
        // A statement inside a body (an unused variable).
        0 | 1 if !body_lines.is_empty() => {
            let l = body_lines[rng.below(body_lines.len())];
            let mut v: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            v.insert(l, format!("    var zz{i} = {i}"));
            v.join("\n") + "\n"
        }
        // A used variable in a body (no new diagnostic).
        2 if !body_lines.is_empty() => {
            let l = body_lines[rng.below(body_lines.len())];
            let mut v: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            v.insert(l, format!("    var zz{i} = {i}\n    _ = zz{i}"));
            v.join("\n") + "\n"
        }
        // A new exported declaration (the signature changes).
        3 => format!("{text}\nfunc Zz{i}(x int) int {{\n    return x + {i}\n}}\n\nstruct Zs{i} {{\n    A int\n}}\n"),
        // A line removed.
        4 if lines.len() > 3 => {
            let l = 1 + rng.below(lines.len() - 1);
            let v: Vec<&str> = lines.iter().enumerate().filter(|(k, _)| *k != l).map(|(_, s)| *s).collect();
            v.join("\n") + "\n"
        }
        // Cut at a random point (syntax errors).
        5 if text.len() > 20 => {
            let mut cut = text.len() / 2 + rng.below(text.len() / 2);
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text[..cut].to_string()
        }
        // A declaration renamed.
        6 => {
            let decl = lines.iter().find_map(|l| l.strip_prefix("struct ").or_else(|| l.strip_prefix("func "))).and_then(|r| r.split(|c: char| !c.is_alphanumeric() && c != '_').next()).filter(|n| !n.is_empty() && *n != "main");
            match decl {
                Some(n) => text.replacen(n, &format!("{n}X"), 1),
                None => format!("{text}\n// edit {i}\n"),
            }
        }
        // Back to the original text.
        7 => original.to_string(),
        _ => format!("{text}\n// edit {i}\n"),
    }
}

fn check_corpus(dirs: &[&str], edits: usize, seed: u64) {
    let mut files = Vec::new();
    for d in dirs {
        collect(&root().join(d), &mut files);
    }
    files.sort();
    let mut s = Session::new();
    let mut texts: Vec<(String, String, String)> = Vec::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        let path = f.display().to_string();
        s.update_file(path.clone(), text.clone());
        texts.push((path, text.clone(), text));
    }
    // Files whose namespace is imported elsewhere: their edits reach other units.
    let imported: Vec<usize> = (0..texts.len())
        .filter(|&i| {
            let ns = texts[i].1.lines().find_map(|l| l.strip_prefix("namespace ")).unwrap_or("").trim().to_string();
            !ns.is_empty() && texts.iter().any(|(_, t, _)| t.lines().any(|l| l.trim() == format!("import {ns}")))
        })
        .collect();
    let mut rng = Rng(seed | 1);
    let first = s.analyze();
    compare(&first.canonical(), &s.analyze_full().canonical(), "initial");
    for i in 0..edits {
        let k = if !imported.is_empty() && rng.below(2) == 0 { imported[rng.below(imported.len())] } else { rng.below(texts.len()) };
        let new = edit(&mut rng, &texts[k].2, &texts[k].1, i);
        texts[k].2 = new.clone();
        s.update_file(texts[k].0.clone(), new);
        let inc = s.analyze();
        let stats = s.last_stats();
        let full = s.analyze_full();
        if inc.canonical() != full.canonical() {
            // Keep the edited text for a look.
            let _ = std::fs::write(root().join("target/incremental-failure.zl"), &texts[k].2);
        }
        compare(&inc.canonical(), &full.canonical(), &format!("edit {i} of {} ({stats:?})", texts[k].0));
    }
}

fn compare(inc: &[String], full: &[String], what: &str) {
    if inc == full {
        return;
    }
    let a: BTreeSet<&String> = inc.iter().collect();
    let b: BTreeSet<&String> = full.iter().collect();
    let mut msg = format!("incremental analysis differs from a full one after {what}:\n");
    for x in a.difference(&b).take(20) {
        msg.push_str(&format!("  incremental only: {x}\n"));
    }
    for x in b.difference(&a).take(20) {
        msg.push_str(&format!("  full only:        {x}\n"));
    }
    if inc.len() != full.len() {
        msg.push_str(&format!("  {} vs {} lines\n", inc.len(), full.len()));
    }
    panic!("{msg}");
}

fn edits(default: usize) -> usize {
    std::env::var("INCR_EDITS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn seed() -> u64 {
    std::env::var("INCR_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x5eed_1234)
}

#[test]
fn incremental_equals_full_on_examples() {
    check_corpus(&["examples"], edits(24), seed());
}

#[test]
fn incremental_equals_full_on_spec() {
    check_corpus(&["tests/spec"], edits(8), seed() ^ 0x9e37_79b9);
}

// ---------------------------------------------------------------------------
// Scripted edits over a small layered project: lib <- core <- web, other.
// ---------------------------------------------------------------------------

const LIB: &str = "namespace app.lib

@attribute(targets: [reflect.Target.Struct])
struct Tag {
    name string
}

func Twice(x int) int {
    return x * 2
}

func Pick<T>(a, b T) T {
    return a
}
";

const CORE: &str = "namespace app.core

import app.lib

struct Item {
    N int
}

func Make(n int) Item {
    return Item{N: Twice(n)}
}
";

const WEB: &str = "namespace app.web

import app.core
import app.lib

func Run() int {
    var a = Make(1)
    var b = Pick(a, Make(2))
    return b.N + len(reflect.TypesWith<Tag>())
}
";

const OTHER: &str = "namespace app.other

func Answer() int {
    return 42
}
";

/// Applies `f` to the session, then checks that the incremental analysis
/// equals the full one; returns the diagnostics as `path: message`.
fn step(s: &mut Session, what: &str, f: impl FnOnce(&mut Session)) -> (Vec<String>, zell::session::Stats) {
    f(s);
    let inc = s.analyze();
    let stats = s.last_stats();
    compare(&inc.canonical(), &s.analyze_full().canonical(), &format!("{what} ({stats:?})"));
    let diags = inc.diagnostics.iter().map(|d| format!("{}: {}", inc.files[d.file].path, d.message)).collect();
    (diags, stats)
}

fn has(diags: &[String], path: &str, needle: &str) -> bool {
    diags.iter().any(|d| d.starts_with(&format!("{path}: ")) && d.contains(needle))
}

#[test]
fn scripted_edits_equal_full_analysis() {
    let mut s = Session::new();
    let (d, st) = step(&mut s, "initial", |s| {
        for (p, t) in [("lib.zl", LIB), ("core.zl", CORE), ("web.zl", WEB), ("other.zl", OTHER)] {
            s.update_file(p, t);
        }
    });
    assert!(d.is_empty(), "{d:?}");
    assert!(st.full_run);

    // A body edit re-checks only its namespace.
    let (d, st) = step(&mut s, "body edit", |s| s.update_file("core.zl", CORE.replace("Twice(n)", "Twice(n) + 1")));
    assert!(d.is_empty(), "{d:?}");
    assert_eq!((st.dirty, st.unit_runs, st.full_run), (1, 1, false));
    // ...and a body error stays in its file.
    let (d, st) = step(&mut s, "body error", |s| s.update_file("core.zl", CORE.replace("Twice(n)", "Twice(\"n\")")));
    assert!(has(&d, "core.zl", "") && d.len() == 1, "{d:?}");
    assert_eq!(st.dirty, 1);

    // A signature change reaches the dependent namespaces, not `other`.
    let (d, st) = step(&mut s, "signature change", |s| {
        s.update_file("core.zl", CORE);
        s.update_file("lib.zl", LIB.replace("func Twice(x int) int {\n    return x * 2", "func Twice(x int) string {\n    return \"\""));
    });
    assert!(has(&d, "core.zl", ""), "{d:?}");
    assert_eq!((st.dirty, st.run_files, st.full_run), (3, 3, false), "lib, core, web");
    let (d, _) = step(&mut s, "signature restored", |s| s.update_file("lib.zl", LIB));
    assert!(d.is_empty(), "{d:?}");

    // Renaming a type breaks its users in another namespace.
    let (d, _) = step(&mut s, "rename a type", |s| s.update_file("core.zl", CORE.replace("Item", "Thing")));
    assert!(d.is_empty(), "{d:?}"); // `web` never names the type.
    let (d, _) = step(&mut s, "rename a function", |s| s.update_file("core.zl", CORE.replace("Make", "Build")));
    assert!(has(&d, "web.zl", "Make"), "{d:?}");
    step(&mut s, "rename restored", |s| s.update_file("core.zl", CORE));

    // Adding and removing files.
    let (d, _) = step(&mut s, "add a file", |s| s.update_file("core2.zl", "namespace app.core\n\nfunc Make2() Item {\n    return Make(3)\n}\n"));
    assert!(d.is_empty(), "{d:?}");
    let (d, _) = step(&mut s, "add a duplicate", |s| s.update_file("core3.zl", "namespace app.core\n\nfunc Make(n int) Item {\n    return Item{}\n}\n"));
    assert!(!d.is_empty());
    let (d, _) = step(&mut s, "remove files", |s| {
        s.remove_file("core2.zl");
        s.remove_file("core3.zl");
    });
    assert!(d.is_empty(), "{d:?}");
    let (d, _) = step(&mut s, "add a namespace", |s| s.update_file("extra.zl", "namespace app.extra\n\nimport app.other\n\nfunc E() int {\n    return Answer()\n}\n"));
    assert!(d.is_empty(), "{d:?}");
    let (d, _) = step(&mut s, "remove its dependency", |s| {
        s.remove_file("other.zl");
    });
    assert!(has(&d, "extra.zl", ""), "{d:?}");
    step(&mut s, "restore it", |s| s.update_file("other.zl", OTHER));

    // Adding and removing imports.
    let (d, _) = step(&mut s, "remove an import", |s| s.update_file("web.zl", WEB.replace("import app.lib\n", "")));
    assert!(has(&d, "web.zl", ""), "{d:?}");
    let (d, _) = step(&mut s, "add an unused import", |s| s.update_file("web.zl", WEB.replace("import app.lib\n", "import app.lib\nimport app.other\n")));
    assert!(has(&d, "web.zl", "unused import"), "{d:?}");
    step(&mut s, "imports restored", |s| s.update_file("web.zl", WEB));

    // A generic of `lib` instantiated in `web` with a type of `core`: an
    // edit of the template reports in the instance's requester.
    let (d, _) = step(&mut s, "generic template edit", |s| s.update_file("lib.zl", LIB.replace("    return a\n}", "    return a.N\n}")));
    assert!(!d.is_empty(), "{d:?}");
    let (d, _) = step(&mut s, "generic instance type edit", |s| {
        s.update_file("lib.zl", LIB);
        s.update_file("web.zl", WEB.replace("Pick(a, Make(2))", "Pick(a, 2)"));
    });
    assert!(has(&d, "web.zl", ""), "{d:?}");
    step(&mut s, "generic restored", |s| s.update_file("web.zl", WEB));

    // `TypesWith<Tag>()`: a type marked in another namespace.
    let (d, _) = step(&mut s, "attribute added elsewhere", |s| s.update_file("other.zl", OTHER.replace("namespace app.other\n", "namespace app.other\n\nimport app.lib\n\n@Tag(name: \"o\")\nstruct Marked {\n}\n")));
    assert!(d.is_empty(), "{d:?}");
    let (d, _) = step(&mut s, "attribute removed", |s| s.update_file("other.zl", OTHER));
    assert!(d.is_empty(), "{d:?}");

    // Whole-program checks: `main` of the entry namespace.
    let (d, _) = step(&mut s, "bad main", |s| s.update_file("main.zl", "namespace app\n\nfunc main(x int) {\n}\n"));
    assert!(has(&d, "main.zl", "main"), "{d:?}");
    let (d, _) = step(&mut s, "main fixed", |s| s.update_file("main.zl", "namespace app\n\nimport io\n\nfunc main() {\n    println(app.web.Run())\n}\n"));
    assert!(d.is_empty(), "{d:?}");
}

/// Random edits over the generated layered workspace of the benchmark
/// (generics of `app.m0` instantiated everywhere, `TypesWith` over all).
#[test]
fn incremental_equals_full_on_synthetic_workspace() {
    let mut s = Session::new();
    let mut texts: Vec<(String, String, String)> = Vec::new();
    for k in 0..6 {
        for j in 0..3 {
            let (p, t) = zell::session::synthetic_file(k, j);
            s.update_file(p.clone(), t.clone());
            texts.push((p, t.clone(), t));
        }
    }
    compare(&s.analyze().canonical(), &s.analyze_full().canonical(), "initial");
    let mut rng = Rng(seed() | 1);
    for i in 0..edits(16) {
        let k = rng.below(texts.len());
        let new = edit(&mut rng, &texts[k].2, &texts[k].1, i);
        texts[k].2 = new.clone();
        s.update_file(texts[k].0.clone(), new);
        let inc = s.analyze();
        let stats = s.last_stats();
        compare(&inc.canonical(), &s.analyze_full().canonical(), &format!("edit {i} of {} ({stats:?})", texts[k].0));
    }
}
