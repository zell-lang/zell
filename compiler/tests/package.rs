//! Package manager, stage 1: manifests, workspaces, path dependencies,
//! strict isolation, namespace roots, cycles, `zell.lock`, the object
//! cache and `zell init`. Fixtures live in `tests/fixtures/pkg/`; every
//! test works on its own copy (builds write `target/` and `zell.lock`).
//!
//! Needs `clang` and `runtime/libzell_rt.a` for the tests that link.

use zell::package::manifest::{self, Format};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn ensure_runtime() {
    let lib = repo().join("runtime/libzell_rt.a");
    if !lib.is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        let name = e.file_name();
        if name == "target" || name == "zell.lock" {
            continue;
        }
        if p.is_dir() {
            copy_dir(&p, &to.join(&name));
        } else {
            std::fs::copy(&p, to.join(&name)).unwrap();
        }
    }
}

/// A fresh copy of all fixtures; returns its root.
fn fixtures(test: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("zell-pkg-{}-{}-{test}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&d);
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pkg"), &d);
    d
}

fn zell(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zell")).args(args).current_dir(dir).output().expect("cannot run zell")
}

fn ok(dir: &Path, args: &[&str]) -> (String, String) {
    let o = zell(dir, args);
    let (out, err) = (String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string());
    assert!(o.status.success(), "zell {args:?} in {} failed:\nstdout:\n{out}\nstderr:\n{err}", dir.display());
    (out, err)
}

fn fails(dir: &Path, args: &[&str]) -> String {
    let o = zell(dir, args);
    let err = String::from_utf8_lossy(&o.stderr).to_string();
    assert!(!o.status.success(), "zell {args:?} in {} unexpectedly succeeded:\n{err}", dir.display());
    err
}

const APP_OUTPUT: &str = "sum is 42\ntwice: 42\nsquare 3 area 9\nrect 2x5 area 10\ncalls: 2\n";

#[test]
fn workspace_of_three_packages_builds_and_runs() {
    ensure_runtime();
    let root = fixtures("ws");
    let ws = root.join("workspace");
    // From a member directory: discovery walks up to the workspace root.
    let (out, _) = ok(&ws.join("app/src"), &["run"]);
    assert_eq!(out, APP_OUTPUT);
    assert!(ws.join("zell.lock").is_file(), "lock file at the workspace root");
    assert!(ws.join("target/debug/app").is_file());
    // `-p` and `--workspace` from the (virtual) root.
    let (out, _) = ok(&ws, &["run", "-p", "app"]);
    assert_eq!(out, APP_OUTPUT);
    ok(&ws, &["build", "--workspace"]);
    ok(&ws, &["check", "-p", "lib-a"]);
    let err = fails(&ws, &["build", "-p", "nope"]);
    assert!(err.contains("no workspace member named `nope`"), "{err}");
    // A release build of the same graph, linked statically.
    ok(&ws, &["build", "-p", "app", "--release", "--static"]);
    let exe = ws.join("target/release/app");
    let o = Command::new(&exe).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), APP_OUTPUT);
}

#[test]
fn importing_an_undeclared_transitive_dependency_is_an_error() {
    let root = fixtures("phantom");
    let err = fails(&root.join("phantom/app"), &["check"]);
    assert!(err.contains("package `app` cannot use namespace `libb.math`"), "{err}");
    assert!(err.contains("not a declared dependency of `app`"), "{err}");
    assert!(err.contains("through `lib-a`"), "{err}");
    assert!(err.contains("add `lib-b` to [dependencies]"), "{err}");

    // A fully qualified path without `import` is checked too.
    let main = root.join("phantom/app/src/main.zl");
    std::fs::write(&main, "namespace app\n\nimport io\n\nfunc main() {\n    println(libb.math.Add(1, 2))\n}\n").unwrap();
    let err = fails(&root.join("phantom/app"), &["check"]);
    assert!(err.contains("cannot use namespace `libb`"), "{err}");

    // Declaring the dependency fixes it.
    let m = root.join("phantom/app/zell.toml");
    let text = std::fs::read_to_string(&m).unwrap() + "lib-b = { path = \"../../workspace/lib-b\" }\n";
    std::fs::write(&m, text).unwrap();
    ok(&root.join("phantom/app"), &["check"]);
}

#[test]
fn namespace_outside_the_package_root_is_an_error() {
    let root = fixtures("ns");
    let err = fails(&root.join("ns-mismatch"), &["check"]);
    assert!(err.contains("namespace `store.models` is outside package `shop`"), "{err}");
    assert!(err.contains("namespace shop.<sub>"), "{err}");
    assert!(err.contains("models.zl:1:11"), "{err}");
}

#[test]
fn duplicate_namespace_roots_are_an_error() {
    let root = fixtures("dup");
    let err = fails(&root.join("dup-root"), &["check", "--workspace"]);
    assert!(err.contains("both own the namespace root `common`"), "{err}");
}

#[test]
fn dependency_cycle_is_an_error() {
    let root = fixtures("cycle");
    let err = fails(&root.join("cycle/a"), &["check"]);
    assert!(err.contains("dependency cycle: a -> b -> a"), "{err}");
}

#[test]
fn several_manifests_are_an_error() {
    let root = fixtures("two");
    let err = fails(&root.join("two-manifests"), &["check"]);
    assert!(err.contains("several manifests (zell.toml, zell.json)"), "{err}");
}

#[test]
fn manifest_formats_round_trip_through_convert() {
    let root = fixtures("formats");
    let dir = root.join("formats");
    let original = manifest::load_dir(&dir).unwrap().manifest;
    assert!(original.package.as_ref().unwrap().license.is_some());
    assert_eq!(original.features.len(), 3);
    for (fmt, file) in [("json", "zell.json"), ("yaml", "zell.yaml"), ("toml", "zell.toml"), ("yaml", "zell.yaml"), ("json", "zell.json")] {
        ok(&dir, &["manifest", "convert", fmt]);
        assert_eq!(manifest::manifests_in(&dir), vec![dir.join(file)], "exactly one manifest after converting to {fmt}");
        let lm = manifest::load_dir(&dir).unwrap();
        assert_eq!(lm.format, Format::parse(fmt).unwrap());
        assert_eq!(lm.manifest, original, "content preserved after converting to {fmt}");
        ok(&dir, &["check"]);
    }
    // Library-level round trip too, for every pair of formats.
    for a in [Format::Toml, Format::Json, Format::Yaml] {
        let text = original.to_string(a).unwrap();
        assert_eq!(zell::package::Manifest::parse(&text, a).unwrap(), original, "{a}");
    }
}

#[test]
fn manifest_schema_is_json_schema() {
    let root = fixtures("schema");
    let (out, _) = ok(&root, &["manifest", "schema"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["$schema"].as_str().unwrap().contains("json-schema.org"));
    for key in ["package", "dependencies", "dev-dependencies", "features", "workspace"] {
        assert!(v["properties"][key].is_object(), "schema has {key}");
    }
}

#[test]
fn lock_file_is_deterministic() {
    let a = fixtures("lock-a");
    let b = fixtures("lock-b");
    ok(&a.join("workspace"), &["check", "--workspace"]);
    ok(&b.join("workspace/lib-a"), &["check"]);
    let la = std::fs::read_to_string(a.join("workspace/zell.lock")).unwrap();
    let lb = std::fs::read_to_string(b.join("workspace/zell.lock")).unwrap();
    assert_eq!(la, lb, "same graph, same lock, wherever it lives and whatever was built");
    // Sorted packages, relative path sources, checksums.
    let names: Vec<&str> = la.lines().filter_map(|l| l.strip_prefix("  name: ")).collect();
    assert_eq!(names, ["app", "lib-a", "lib-b"]);
    assert!(la.contains("source: path:lib-b"), "{la}");
    assert!(la.contains("checksum: sha256:"), "{la}");
    assert!(!la.contains(a.to_str().unwrap()), "no absolute paths: {la}");
    // Re-running does not touch it.
    let before = std::fs::metadata(a.join("workspace/zell.lock")).unwrap().modified().unwrap();
    let (_, err) = ok(&a.join("workspace"), &["check", "--workspace", "--verbose"]);
    assert!(!err.contains("wrote"), "{err}");
    assert_eq!(std::fs::metadata(a.join("workspace/zell.lock")).unwrap().modified().unwrap(), before);
}

#[test]
fn locked_fails_when_the_lock_would_change() {
    let root = fixtures("locked");
    let ws = root.join("workspace");
    let err = fails(&ws, &["check", "--workspace", "--locked"]);
    assert!(err.contains("zell.lock needs to be updated (it does not exist)"), "{err}");
    ok(&ws, &["check", "--workspace"]);
    ok(&ws, &["check", "--workspace", "--locked"]);
    // A manifest change (new version) ...
    let m = ws.join("lib-b/zell.toml");
    std::fs::write(&m, std::fs::read_to_string(&m).unwrap().replace("1.0.0", "1.1.0")).unwrap();
    let err = fails(&ws, &["check", "--workspace", "--locked"]);
    assert!(err.contains("version of `lib-b` changed") && err.contains("--locked"), "{err}");
    ok(&ws, &["check", "--workspace"]);
    // ... and a source change both need a new lock.
    let f = ws.join("lib-b/src/math/add.zl");
    std::fs::write(&f, std::fs::read_to_string(&f).unwrap() + "\n// edited\n").unwrap();
    let err = fails(&ws, &["check", "--workspace", "--locked"]);
    assert!(err.contains("sources (checksum) of `lib-b` changed"), "{err}");
}

#[test]
fn unchanged_packages_are_not_recompiled() {
    ensure_runtime();
    let root = fixtures("cache");
    let app = root.join("workspace/app");
    let (_, first) = ok(&app, &["build", "--verbose"]);
    for p in ["lib-b", "lib-a", "app"] {
        assert!(first.contains(&format!("compiling {p} -> ")), "{first}");
    }
    let objs: Vec<_> = std::fs::read_dir(root.join("workspace/target/zell")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
    assert_eq!(objs.len(), 3, "{objs:?}");
    assert!(objs.iter().all(|o| o.ends_with(".o")), "{objs:?}");

    let (_, second) = ok(&app, &["build", "--verbose"]);
    assert!(!second.contains("compiling"), "second build recompiled:\n{second}");
    for p in ["lib-b", "lib-a", "app"] {
        assert!(second.contains(&format!("cache hit: {p} (")), "{second}");
    }

    // Editing the binary recompiles only the binary.
    let main = app.join("src/main.zl");
    std::fs::write(&main, std::fs::read_to_string(&main).unwrap().replace("calls:", "calls so far:")).unwrap();
    let (_, third) = ok(&app, &["build", "--verbose"]);
    assert!(third.contains("cache hit: lib-b (") && third.contains("cache hit: lib-a ("), "{third}");
    assert!(third.contains("compiling app -> "), "{third}");
    let o = Command::new(root.join("workspace/target/debug/app")).output().unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("calls so far: 2"));

    // Different flags are a different cache entry.
    let (_, rel) = ok(&app, &["build", "--release", "--verbose"]);
    assert!(rel.contains("compiling lib-b -> "), "{rel}");
}

#[test]
fn init_creates_runnable_packages_in_every_format() {
    ensure_runtime();
    let root = fixtures("init");
    for fmt in ["toml", "json", "yaml"] {
        let name = format!("hello-{fmt}");
        ok(&root, &["init", &name, "--format", fmt]);
        let dir = root.join(&name);
        assert!(dir.join(format!("zell.{fmt}")).is_file());
        let (out, _) = ok(&dir, &["run"]);
        assert_eq!(out, "Hello, world!\n");
    }
    ok(&root, &["init", "util", "--lib"]);
    assert!(root.join("util/src/lib.zl").is_file());
    let lm = manifest::load_dir(&root.join("util")).unwrap();
    assert!(lm.manifest.package.unwrap().entry.is_none(), "a library has no entry");
    ok(&root.join("util"), &["build"]);
    let err = fails(&root.join("util"), &["run"]);
    assert!(err.contains("libraries"), "{err}");
    let err = fails(&root, &["init", "util"]);
    assert!(err.contains("already has a manifest"), "{err}");
    // `--help` after a command prints that command's usage (exit 0) and
    // creates nothing.
    let (help, _) = ok(&root, &["init", "--help"]);
    assert!(help.starts_with("usage: zell init") && help.contains("--format toml|json|yaml"), "{help}");
    assert!(!root.join("--help").exists());
    let (help, _) = ok(&root, &["build", "-h"]);
    assert!(help.contains("build options:") && help.contains("--release"), "{help}");
}

#[test]
fn plain_directory_without_manifest_still_builds() {
    ensure_runtime();
    let out_dir = fixtures("plain");
    let exe = out_dir.join("multi");
    let multi = repo().join("examples/multi");
    ok(&out_dir, &["build", multi.to_str().unwrap(), "-o", exe.to_str().unwrap()]);
    let o = Command::new(&exe).output().unwrap();
    let want = std::fs::read_to_string(multi.join("namespaces.out")).unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), want);
    // No manifest anywhere up from a temp dir without a path: a clear error.
    let empty = out_dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    if manifest::find_upwards(&empty).unwrap().is_none() {
        let err = fails(&empty, &["build"]);
        assert!(err.contains("could not find zell.toml, zell.json, zell.yaml"), "{err}");
    }
}

#[test]
fn cfg_features_and_file_suffixes_in_packages() {
    ensure_runtime();
    static N: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!("zell-pkg-{}-{}-cfg", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&root);
    let write = |p: &str, s: &str| {
        let f = root.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, s).unwrap();
    };
    write("app/zell.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nnamespace = \"app\"\nentry = \"src/main.zl\"\n\n[dependencies]\nlib = { path = \"../lib\", features = [\"fast\"] }\n\n[features]\ndefault = [\"verbose\"]\nverbose = [\"lib/extra\"]\nunused = []\n");
    write(
        "app/src/main.zl",
        "namespace app\nimport io\n@cfg(feature: \"verbose\")\nfunc Mode() string { return \"verbose\" }\n@cfg(feature: \"unused\")\nfunc Mode() string { return nope }\nfunc main() { println(Mode(), lib.Speed(), lib.Extra(), lib.Base(), lib.Os()) }\n",
    );
    write("lib/zell.toml", "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nnamespace = \"lib\"\n\n[features]\ndefault = [\"base\"]\nbase = []\nfast = []\nextra = []\nslow = []\n");
    write(
        "lib/src/lib.zl",
        "namespace lib\n@cfg(feature: \"fast\")\nfunc Speed() string { return \"fast\" }\n@cfg(not(feature: \"fast\"))\nfunc Speed() string { return \"slow\" }\n@cfg(feature: \"extra\")\nfunc Extra() string { return \"extra\" }\n@cfg(all(feature: \"base\", not(feature: \"slow\")))\nfunc Base() string { return \"base\" }\n",
    );
    let host = zell::target::Target::host();
    let other = if host.os == "windows" { "linux" } else { "windows" };
    write(&format!("lib/src/os_{}.zl", host.os), "namespace lib\nfunc Os() string { return \"host\" }\n");
    write(&format!("lib/src/os_{other}.zl"), "namespace lib\nfunc Os() string { return \"other\" }\n");
    let (out, _) = ok(&root.join("app"), &["run"]);
    assert_eq!(out, "verbose fast extra base host\n");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Stage 2: module dependencies (git tags, MVS, lock, proxy, checksum log,
// store, offline, vendor). Every test builds local bare git repositories
// and maps `https://example.com/<name>` to them with git's `insteadOf`
// (through GIT_CONFIG_* variables), so nothing leaves the machine.
// ---------------------------------------------------------------------------

/// A sandbox: bare repositories, a store, a home, projects.
struct Sandbox {
    root: PathBuf,
}

/// Runs git in `dir` (test identity, no user configuration).
fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

/// One release of a test module: tag and files.
type Release<'a> = (&'a str, &'a [(&'a str, &'a str)]);

impl Sandbox {
    fn new(test: &str) -> Sandbox {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!("zell-mod-{}-{}-{test}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repos")).unwrap();
        Sandbox { root }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.root.join(p)
    }

    fn write(&self, p: &str, s: &str) {
        let f = self.root.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, s).unwrap();
    }

    /// Creates the bare repository `repos/<name>` (module path
    /// `example.com/<name>`) with one commit and tag per release; files of
    /// a release replace the previous tree.
    fn module(&self, name: &str, releases: &[Release]) {
        let work = self.path(&format!("work/{name}"));
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q"]);
        for (tag, files) in releases {
            for e in std::fs::read_dir(&work).unwrap().flatten() {
                if e.file_name() != ".git" {
                    let p = e.path();
                    if p.is_dir() {
                        std::fs::remove_dir_all(p).unwrap();
                    } else {
                        std::fs::remove_file(p).unwrap();
                    }
                }
            }
            for (p, text) in *files {
                let f = work.join(p);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(f, text).unwrap();
            }
            git(&work, &["add", "-A"]);
            git(&work, &["commit", "-q", "--allow-empty", "-m", tag]);
            if !tag.is_empty() && !tag.starts_with("branch:") {
                git(&work, &["tag", "-f", tag]);
            }
        }
        let bare = self.path(&format!("repos/{name}"));
        let _ = std::fs::remove_dir_all(&bare);
        git(&self.root, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    }

    /// Environment of a `zell` run: own store and home, and (`git`) the
    /// rewrite of example.com to the local repositories.
    fn env(&self, git: bool) -> Vec<(String, String)> {
        let mut v = vec![
            ("ZELL_STORE".to_string(), self.path("store").display().to_string()),
            ("ZELL_HOME".to_string(), self.path("home").display().to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ];
        if git {
            v.push(("GIT_CONFIG_COUNT".into(), "1".into()));
            v.push(("GIT_CONFIG_KEY_0".into(), format!("url.file://{}/.insteadOf", self.path("repos").display())));
            v.push(("GIT_CONFIG_VALUE_0".into(), "https://example.com/".into()));
        }
        v
    }

    fn run(&self, dir: &str, args: &[&str], env: &[(String, String)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_zell"));
        c.args(args).current_dir(self.path(dir));
        for v in ["ZELL_PROXY", "ZELL_SUMDB", "ZELL_PRIVATE", "ZELL_NOPROXY", "ZELL_NOSUMDB", "GIT_CONFIG_COUNT", "ZELL_STD"] {
            c.env_remove(v);
        }
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().expect("cannot run zell")
    }

    fn ok(&self, dir: &str, args: &[&str], env: &[(String, String)]) -> String {
        let o = self.run(dir, args, env);
        let (out, err) = (String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string());
        assert!(o.status.success(), "zell {args:?} in {dir} failed:\nstdout:\n{out}\nstderr:\n{err}");
        out + &err
    }

    fn fails(&self, dir: &str, args: &[&str], env: &[(String, String)]) -> String {
        let o = self.run(dir, args, env);
        let err = String::from_utf8_lossy(&o.stderr).to_string();
        assert!(!o.status.success(), "zell {args:?} in {dir} unexpectedly succeeded:\n{err}");
        err
    }

    fn read(&self, p: &str) -> String {
        std::fs::read_to_string(self.path(p)).unwrap_or_else(|e| panic!("{p}: {e}"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            make_writable(&self.root);
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn make_writable(p: &Path) {
    if let Ok(m) = std::fs::symlink_metadata(p) {
        if m.is_dir() {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                make_writable(&e.path());
            }
        }
        let mut perm = m.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        let _ = std::fs::set_permissions(p, perm);
    }
}

/// A library module manifest + source: `func Name() string` returns `text`.
fn lib_files(name: &str, version: &str, deps: &str, body: &str) -> Vec<(String, String)> {
    vec![
        ("zell.toml".into(), format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nnamespace = \"{}\"\n\n[dependencies]\n{deps}", name.replace('-', "_"))),
        ("src/lib.zl".into(), format!("namespace {}\n\n// Name says which version this is.\n{body}\n", name.replace('-', "_"))),
    ]
}

fn module_from(sb: &Sandbox, name: &str, releases: &[(&str, Vec<(String, String)>)]) {
    let owned: Vec<(&str, Vec<(&str, &str)>)> = releases.iter().map(|(t, fs)| (*t, fs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect())).collect();
    let rel: Vec<Release> = owned.iter().map(|(t, fs)| (*t, fs.as_slice())).collect();
    sb.module(name, &rel);
}

/// The app project in `dir` depending on `deps`, printing `body`.
fn app(sb: &Sandbox, dir: &str, deps: &str, imports: &str, body: &str) {
    sb.write(&format!("{dir}/zell.toml"), &format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nnamespace = \"app\"\nentry = \"src/main.zl\"\n\n[dependencies]\n{deps}"));
    sb.write(&format!("{dir}/src/main.zl"), &format!("namespace app\n\nimport io\n{imports}\nfunc main() {{\n    println({body})\n}}\n"));
}

/// Modules a (v1.0.0, v1.1.0, v2.0.0) and c (v1.0.0..v1.3.0), b (v1.0.0)
/// with a -> c ^1.1, b -> c ^1.2.
fn three_modules(sb: &Sandbox) {
    let c = |v: &str| lib_files("modc", &v[1..], "", &format!("func Name() string {{\n    return \"c{v}\"\n}}"));
    module_from(sb, "c", &[("v1.0.0", c("v1.0.0")), ("v1.1.0", c("v1.1.0")), ("v1.2.0", c("v1.2.0")), ("v1.3.0", c("v1.3.0"))]);
    let a = |v: &str| lib_files("moda", &v[1..], "\"example.com/c\" = \"^1.1\"\n", &format!("func Name() string {{\n    return \"a{v}+\" + modc.Name()\n}}"));
    module_from(sb, "a", &[("v1.0.0", a("v1.0.0")), ("v1.1.0", a("v1.1.0")), ("v2.0.0", a("v2.0.0"))]);
    let b = lib_files("modb", "1.0.0", "\"example.com/c\" = \"^1.2\"\n", "func Name() string {\n    return \"b+\" + modc.Name()\n}");
    module_from(sb, "b", &[("v1.0.0", b)]);
}

#[test]
fn git_dependencies_resolve_from_tags_with_mvs() {
    ensure_runtime();
    let sb = Sandbox::new("mvs");
    three_modules(&sb);
    let env = sb.env(true);
    app(&sb, "p", "\"example.com/a\" = \"^1.0\"\n\"example.com/b\" = \"^1\"\n", "", "moda.Name(), modb.Name()");
    let out = sb.ok("p", &["run"], &env);
    // MVS: a at its minimum v1.0.0; c at max(min(^1.1), min(^1.2)) = v1.2.0.
    assert!(out.ends_with("av1.0.0+cv1.2.0 b+cv1.2.0\n"), "{out}");
    let lock = sb.read("p/zell.lock");
    assert!(lock.contains("module: example.com/a") && lock.contains("version: v1.0.0"), "{lock}");
    assert!(lock.contains("module: example.com/c") && lock.contains("version: v1.2.0"), "{lock}");
    assert!(lock.contains("checksum: h1:") && lock.contains("source: module"), "{lock}");
    assert!(sb.path("p/.zell/deps/example.com/c@v1.2.0/src/lib.zl").is_file());
    // tree and why.
    let tree = sb.ok("p", &["tree"], &env);
    assert!(tree.contains("├── moda v1.0.0 (example.com/a)") && tree.contains("modc v1.2.0 (example.com/c)"), "{tree}");
    let why = sb.ok("p", &["why", "example.com/c"], &env);
    assert!(why.contains("moda v1.0.0 (example.com/a) (example.com/a ^1.0) -> modc v1.2.0 (example.com/c) (example.com/c ^1.1)"), "{why}");
    assert!(why.contains("modb v1.0.0 (example.com/b)"), "{why}");
    // Re-tagged releases with other content: the lock's checksum catches it.
    let c13 = lib_files("modc", "1.4.0", "", "func Name() string {\n    return \"cv1.4.0\"\n}");
    module_from(&sb, "c", &[("v1.0.0", c13.clone()), ("v1.1.0", c13.clone()), ("v1.2.0", c13.clone()), ("v1.3.0", c13.clone()), ("v1.4.0", c13)]);
    make_writable(&sb.path("store"));
    std::fs::remove_dir_all(sb.path("store")).unwrap();
    let err = sb.fails("p", &["install"], &env);
    assert!(err.contains("checksum mismatch for example.com/c@v1.2.0"), "{err}");
    // Back to the original content (v1.4.0 stays): a new release changes
    // nothing until `zell update`.
    three_modules(&sb);
    sb.ok("p", &["install"], &env);
    assert!(sb.read("p/zell.lock").contains("version: v1.2.0"));
    let up = sb.ok("p", &["update", "example.com/a"], &env);
    assert!(up.contains("updated example.com/a v1.0.0 -> v1.1.0"), "{up}");
    assert!(!sb.path("p/.zell/deps/example.com/a@v1.0.0").exists(), "stale version removed");
    let out = sb.ok("p", &["run"], &env);
    assert!(out.ends_with("av1.1.0+cv1.2.0 b+cv1.2.0\n"), "major 2 is never picked: {out}");
    let up = sb.ok("p", &["update"], &env);
    assert!(up.contains("updated example.com/c v1.2.0 -> v1.4.0"), "{up}");
}

#[test]
fn git_pins_aliases_and_version_errors() {
    ensure_runtime();
    let sb = Sandbox::new("pins");
    three_modules(&sb);
    let env = sb.env(true);
    // Alias key with a git location and a version; a tag pin; a branch pin.
    let d = lib_files("modd", "0.1.0", "", "func Name() string {\n    return \"d-main\"\n}");
    module_from(&sb, "d", &[("v0.1.0", d.clone()), ("branch:main", lib_files("modd", "0.2.0", "", "func Name() string {\n    return \"d-head\"\n}"))]);
    let deps = "cee = { git = \"https://example.com/c\", version = \"~1.1\" }\n\"example.com/d\" = { git = \"example.com/d\", branch = \"main\" }\n";
    app(&sb, "p", deps, "", "modc.Name(), modd.Name()");
    let out = sb.ok("p", &["run"], &env);
    assert!(out.ends_with("cv1.1.0 d-head\n"), "{out}");
    let lock = sb.read("p/zell.lock");
    assert!(lock.contains("source: git+https://example.com/d?branch=main#"), "{lock}");
    assert!(lock.contains("version: v0.0.0-"), "branch pins get a pseudo-version: {lock}");
    // Tag and rev pins.
    app(&sb, "p", "\"example.com/d\" = { git = \"example.com/d\", tag = \"v0.1.0\" }\n", "", "modd.Name()");
    assert!(sb.ok("p", &["run"], &env).ends_with("d-main\n"));
    // Errors: no matching version; two majors; a version without a module path.
    app(&sb, "p", "\"example.com/c\" = \"^3\"\n", "", "1");
    let err = sb.fails("p", &["check"], &env);
    assert!(err.contains("no version of example.com/c matches `^3`") && err.contains("v1.3.0"), "{err}");
    app(&sb, "p", "\"example.com/a\" = \"^2\"\n\"example.com/b\" = \"^1\"\nalias = { git = \"example.com/a\", version = \"^1\" }\n", "", "1");
    let err = sb.fails("p", &["check"], &env);
    assert!(err.contains("only one major version"), "{err}");
    app(&sb, "p", "router = \"^1\"\n", "", "1");
    let err = sb.fails("p", &["check"], &env);
    assert!(err.contains("a version needs a module path"), "{err}");
}

#[test]
fn lock_file_with_modules_is_deterministic_and_locked() {
    let sb = Sandbox::new("lockdet");
    three_modules(&sb);
    let env = sb.env(true);
    for p in ["p1", "p2"] {
        app(&sb, p, "\"example.com/b\" = \"^1\"\n\"example.com/a\" = \"^1.0\"\n", "", "moda.Name(), modb.Name()");
    }
    sb.ok("p1", &["install"], &env);
    sb.ok("p2", &["check"], &env);
    let (l1, l2) = (sb.read("p1/zell.lock"), sb.read("p2/zell.lock"));
    assert_eq!(l1, l2);
    assert!(!l1.contains(sb.root.to_str().unwrap()), "no machine paths: {l1}");
    let names: Vec<&str> = l1.lines().filter_map(|l| l.strip_prefix("  name: ")).collect();
    assert_eq!(names, ["app", "moda", "modb", "modc"]);
    sb.ok("p1", &["install", "--locked"], &env);
    assert_eq!(sb.read("p1/zell.lock"), l1, "re-install keeps the lock byte for byte");
    // A stricter constraint needs a new lock.
    app(&sb, "p1", "\"example.com/b\" = \"^1\"\n\"example.com/a\" = \"^1.1\"\n", "", "1");
    let err = sb.fails("p1", &["install", "--locked"], &env);
    assert!(err.contains("--locked"), "{err}");
}

/// A running `zell proxy serve`, killed on drop.
struct ProxyServer {
    child: std::process::Child,
    url: String,
}

impl ProxyServer {
    fn start(sb: &Sandbox, dir: &str, extra: &[&str], git: bool) -> ProxyServer {
        use std::io::BufRead;
        let mut c = Command::new(env!("CARGO_BIN_EXE_zell"));
        c.args(["proxy", "serve", "--dir", sb.path(dir).to_str().unwrap()]).args(extra);
        for (k, v) in sb.env(git) {
            c.env(k, v);
        }
        let mut child = c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let url = line.trim().strip_prefix("listening on ").unwrap_or_else(|| panic!("proxy said {line:?}")).to_string();
        ProxyServer { child, url }
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn proxy_server_round_trip_and_checksum_log() {
    let sb = Sandbox::new("proxy");
    three_modules(&sb);
    // The proxy reaches git; the client only the proxy (no git rewrite).
    let proxy = ProxyServer::start(&sb, "cache", &["--sumdb"], true);
    let mut env = sb.env(false);
    env.push(("ZELL_PROXY".into(), proxy.url.clone()));
    env.push(("ZELL_SUMDB".into(), format!("{}/sumdb", proxy.url)));
    app(&sb, "p", "\"example.com/a\" = \"^1.0\"\n\"example.com/b\" = \"^1\"\n", "", "moda.Name(), modb.Name()");
    sb.ok("p", &["install"], &env);
    sb.ok("p", &["check"], &env);
    for f in ["example.com/c/@v/list", "example.com/c/@v/v1.2.0.info", "example.com/c/@v/v1.2.0.manifest", "example.com/c/@v/v1.2.0.zip", "sumdb/latest", "sumdb/lookup/example.com/c@v1.2.0"] {
        assert!(sb.path(&format!("cache/{f}")).is_file(), "proxy cache has {f}");
    }
    assert_eq!(sb.read("cache/example.com/c/@v/list"), "v1.0.0\nv1.1.0\nv1.2.0\nv1.3.0\n");
    let info = sb.read("cache/example.com/c/@v/v1.2.0.info");
    assert!(info.contains("\"Version\": \"v1.2.0\"") && info.contains("\"Manifest\": \"zell.toml\""), "{info}");
    let lookup = sb.read("cache/sumdb/lookup/example.com/c@v1.2.0");
    let lock = sb.read("p/zell.lock");
    let h = lookup.lines().nth(1).unwrap().rsplit(' ').next().unwrap();
    assert!(lock.contains(&format!("checksum: {h}")), "log and lock agree on {h}:\n{lock}");
    assert!(sb.read("cache/sumdb/latest").starts_with("zell-sumdb v1\n"));

    // The filled directory is a static mirror: `--upstream off` serves it alone.
    let mirror = ProxyServer::start(&sb, "cache", &["--upstream", "off", "--sumdb"], false);
    let mut env2 = sb.env(false);
    env2.push(("ZELL_STORE".into(), sb.path("store2").display().to_string()));
    env2.push(("ZELL_PROXY".into(), mirror.url.clone()));
    env2.push(("ZELL_SUMDB".into(), format!("{}/sumdb", mirror.url)));
    app(&sb, "q", "\"example.com/b\" = \"^1\"\n", "", "modb.Name()");
    sb.ok("q", &["install"], &env2);
    // Unknown on the mirror (404): `,direct` falls back to git.
    let d = lib_files("modd", "0.1.0", "", "func Name() string {\n    return \"d\"\n}");
    module_from(&sb, "d", &[("v0.1.0", d)]);
    app(&sb, "q", "\"example.com/b\" = \"^1\"\n\"example.com/d\" = \"^0.1\"\n", "", "modb.Name(), modd.Name()");
    let err = sb.fails("q", &["install"], &env2);
    assert!(err.contains("example.com/d"), "{err}");
    let mut env3 = sb.env(true);
    env3.push(("ZELL_STORE".into(), sb.path("store2").display().to_string()));
    env3.push(("ZELL_PROXY".into(), format!("{},direct", mirror.url)));
    env3.push(("ZELL_NOSUMDB".into(), "example.com/d".into()));
    env3.push(("ZELL_SUMDB".into(), format!("{}/sumdb", mirror.url)));
    sb.ok("q", &["install"], &env3);
    assert!(sb.read("q/zell.lock").contains("module: example.com/d"));
    // ZELL_PRIVATE: direct, without proxy or log.
    let mut env4 = sb.env(true);
    env4.push(("ZELL_STORE".into(), sb.path("store3").display().to_string()));
    env4.push(("ZELL_PROXY".into(), "off".into()));
    env4.push(("ZELL_PRIVATE".into(), "example.com/*".into()));
    env4.push(("ZELL_SUMDB".into(), "http://127.0.0.1:9/none".into()));
    sb.ok("q", &["install"], &env4);
}

#[test]
fn checksum_mismatch_is_rejected() {
    let sb = Sandbox::new("tamper");
    three_modules(&sb);
    let proxy = ProxyServer::start(&sb, "cache", &["--sumdb"], true);
    let mut env = sb.env(false);
    env.push(("ZELL_PROXY".into(), proxy.url.clone()));
    env.push(("ZELL_SUMDB".into(), format!("{}/sumdb", proxy.url)));
    app(&sb, "p", "\"example.com/c\" = \"^1.2\"\n", "", "modc.Name()");
    sb.ok("p", &["install"], &env);
    // Replace the cached archive with different content: the log says otherwise.
    let zip = sb.path("cache/example.com/c/@v/v1.2.0.zip");
    let files = zell::package::sum::zip_read(&std::fs::read(&zip).unwrap(), "example.com/c@v1.2.0").unwrap();
    let evil: Vec<(String, Vec<u8>)> = files.into_iter().map(|(p, d)| if p == "src/lib.zl" { (p, b"namespace modc\nfunc Name() string { return \"evil\" }\n".to_vec()) } else { (p, d) }).collect();
    std::fs::write(&zip, zell::package::sum::zip_write("example.com/c@v1.2.0", &evil)).unwrap();
    let mut env2 = env.clone();
    env2.push(("ZELL_STORE".into(), sb.path("store2").display().to_string()));
    let _ = std::fs::remove_file(sb.path("p/zell.lock"));
    let err = sb.fails("p", &["install"], &env2);
    assert!(err.contains("SECURITY ERROR: checksum mismatch for example.com/c@v1.2.0") && err.contains("checksum log"), "{err}");
    // Without the log, the lock catches it.
    sb.ok("p", &["install"], &env);
    let mut env3: Vec<(String, String)> = env.iter().filter(|(k, _)| k != "ZELL_SUMDB").cloned().collect();
    env3.push(("ZELL_STORE".into(), sb.path("store3").display().to_string()));
    let err = sb.fails("p", &["install"], &env3);
    assert!(err.contains("checksum mismatch for example.com/c@v1.2.0") && err.contains("zell.lock"), "{err}");
    // A corrupted store file is found by `zell store verify`.
    sb.ok("p", &["store", "verify"], &env);
    let idx = sb.read("store/packages/example.com/c/v1.2.0/index.yaml");
    let sha = idx.lines().find(|l| l.contains("sha256:")).unwrap().rsplit(' ').next().unwrap().to_string();
    let f = sb.path(&format!("store/files/{sha}"));
    make_writable(&f);
    std::fs::write(&f, "corrupt").unwrap();
    let err = sb.fails("p", &["store", "verify"], &env);
    assert!(err.contains("contents do not match the hash"), "{err}");
}

#[cfg(unix)]
#[test]
fn store_deduplicates_across_projects_and_installs_offline() {
    use std::os::unix::fs::MetadataExt;
    ensure_runtime();
    let sb = Sandbox::new("store");
    three_modules(&sb);
    let env = sb.env(true);
    for p in ["p1", "p2"] {
        app(&sb, p, "\"example.com/b\" = \"^1\"\n", "", "modb.Name()");
    }
    sb.ok("p1", &["install"], &env);
    let count = || std::fs::read_dir(sb.path("store/files")).unwrap().count();
    let n = count();
    sb.ok("p2", &["install"], &env);
    assert_eq!(count(), n, "the second project adds no files to the store");
    let f1 = std::fs::metadata(sb.path("p1/.zell/deps/example.com/c@v1.2.0/src/lib.zl")).unwrap();
    let f2 = std::fs::metadata(sb.path("p2/.zell/deps/example.com/c@v1.2.0/src/lib.zl")).unwrap();
    assert_eq!(f1.ino(), f2.ino(), "hard links to one store file");
    assert!(f1.nlink() >= 3);
    let store_path = sb.ok("p1", &["store", "path"], &env);
    assert_eq!(store_path.trim(), sb.path("store").display().to_string());
    // Offline: from the lock and the store, without git.
    std::fs::remove_dir_all(sb.path("p2/.zell")).unwrap();
    let offline = sb.env(false);
    sb.ok("p2", &["install", "--offline"], &offline);
    assert!(sb.ok("p2", &["run", "--offline"], &offline).ends_with("b+cv1.2.0\n"));
    let mut fresh = offline.clone();
    fresh.push(("ZELL_STORE".into(), sb.path("empty-store").display().to_string()));
    let err = sb.fails("p2", &["install", "--offline"], &fresh);
    assert!(err.contains("--offline"), "{err}");
    // prune keeps what registered projects lock, drops the rest.
    app(&sb, "p1", "", "", "1");
    sb.ok("p1", &["install"], &env);
    std::fs::remove_dir_all(sb.path("p2")).unwrap();
    let pruned = sb.ok("p1", &["store", "prune"], &env);
    assert!(pruned.contains("removed example.com/c@v1.2.0"), "{pruned}");
    assert_eq!(count(), 0, "{pruned}");
}

#[test]
fn vendor_directory_replaces_store_and_network() {
    ensure_runtime();
    let sb = Sandbox::new("vendor");
    three_modules(&sb);
    let env = sb.env(true);
    app(&sb, "p", "\"example.com/a\" = \"^1.0\"\n", "", "moda.Name()");
    sb.ok("p", &["vendor"], &env);
    assert!(sb.path("p/vendor/modules.yaml").is_file());
    assert!(sb.path("p/vendor/example.com/c@v1.1.0/src/lib.zl").is_file());
    std::fs::remove_dir_all(sb.path("p/.zell")).unwrap();
    make_writable(&sb.path("store"));
    std::fs::remove_dir_all(sb.path("store")).unwrap();
    let offline = sb.env(false);
    let out = sb.ok("p", &["run"], &offline);
    assert!(out.ends_with("av1.0.0+cv1.1.0\n"), "{out}");
    assert!(!sb.path("store").exists(), "vendor builds never touch the store");
    // A tampered vendored file is caught.
    let f = sb.path("p/vendor/example.com/c@v1.1.0/src/lib.zl");
    std::fs::write(&f, std::fs::read_to_string(&f).unwrap() + "// edit\n").unwrap();
    let err = sb.fails("p", &["check"], &offline);
    assert!(err.contains("checksum mismatch"), "{err}");
}

#[test]
fn std_loads_from_disk_and_falls_back_to_embedded() {
    let sb = Sandbox::new("std");
    // A std tree with one extra function in cmp.
    copy_dir(&repo().join("std"), &sb.path("std"));
    let cmp = sb.path("std/cmp/cmp.zl");
    std::fs::write(&cmp, std::fs::read_to_string(&cmp).unwrap() + "\n// FromDisk exists only in this copy.\nfunc FromDisk() int {\n    return 7\n}\n").unwrap();
    sb.write("x/main.zl", "namespace app\n\nimport io\nimport cmp\n\nfunc main() {\n    println(FromDisk())\n}\n");
    let mut env = sb.env(false);
    env.push(("ZELL_STD".into(), sb.path("std").display().to_string()));
    sb.ok("x", &["check", "main.zl"], &env);
    let out = sb.ok("x", &["env", "ZELL_STD"], &env);
    assert_eq!(out.trim(), sb.path("std").display().to_string());
    // An empty ZELL_STD directory: every module comes from the binary.
    std::fs::create_dir_all(sb.path("empty")).unwrap();
    let mut env2 = sb.env(false);
    env2.push(("ZELL_STD".into(), sb.path("empty").display().to_string()));
    let err = sb.fails("x", &["check", "main.zl"], &env2);
    assert!(err.contains("FromDisk"), "{err}");
    sb.write("y/main.zl", "namespace app\n\nimport io\nimport cmp\n\nfunc main() {\n    println(Compare(1, 2))\n}\n");
    sb.ok("y", &["check", "main.zl"], &env2);
    let all = sb.ok("x", &["env"], &env);
    for v in ["ZELL_STD=", "ZELL_HOME=", "ZELL_STORE=", "ZELL_PROXY=\"direct\""] {
        assert!(all.contains(v), "{all}");
    }
}

#[test]
fn package_cannot_own_a_stdlib_root() {
    for (name, ns) in [("io", ""), ("myhttp", "net"), ("oldstd", "std")] {
        let ns_line = if ns.is_empty() { String::new() } else { format!("namespace = \"{ns}\"\n") };
        let text = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{ns_line}");
        let err = zell::package::Manifest::parse(&text, Format::Toml).unwrap().validate().unwrap_err();
        let root = if ns.is_empty() { name } else { ns };
        assert!(err.contains(&format!("cannot own the namespace `{root}`")), "{err}");
    }
    let ok = "[package]\nname = \"iox\"\nversion = \"0.1.0\"\n";
    assert!(zell::package::Manifest::parse(ok, Format::Toml).unwrap().validate().is_ok());
}
