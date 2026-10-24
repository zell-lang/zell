//! The standard library on disk (docs/TOOLING.md, "Исходники стандартной
//! библиотеки и зависимостей для IDE").
//!
//! `compiler/build.rs` writes the `std/` tree of the repository on every
//! compiler build:
//!
//! ```text
//! std/<namespace path>/<file>.zl      every sema/stdlib_src/*.zl and sema/reflect_src/*.zl
//!                                     (`net.http` -> std/net/http/http_client.zl),
//!                                     and std/collections/collections.zl (sema/collections.rs)
//! std/<namespace path>/<last>_extern.zl
//!                                     `extern func` declarations with doc comments for
//!                                     the C runtime functions of stdlib.rs `FUNCS`
//!                                     (`strings` -> std/strings/strings_extern.zl)
//! ```
//!
//! The compiler loads the Zell-source modules from the first of
//! `$ZELL_STD`, `<exe dir>/../std` (an installed toolchain) and the
//! repository's `std/` that exists, file by file, and falls back to the
//! copies embedded in the binary when a file is absent (or when
//! `$ZELL_STD` names a directory without it). The extern files are only
//! read by tools (LSP navigation, hover); the compiler's knowledge of the
//! C functions is the `FUNCS` table itself.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The repository's `std/` (compile-time path).
const REPO_STD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../std");

/// The directory the standard library is loaded from, if any.
pub fn std_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| find_std_dir(std::env::var_os("ZELL_STD").map(PathBuf::from), std::env::current_exe().ok())).clone()
}

/// [`std_dir`] for explicit inputs: `env` is `$ZELL_STD` (used even when
/// it does not exist: an explicit setting is never second-guessed),
/// `exe` the running executable.
pub fn find_std_dir(env: Option<PathBuf>, exe: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(e) = env.filter(|e| !e.as_os_str().is_empty()) {
        return e.is_dir().then_some(e);
    }
    if let Some(d) = exe.as_ref().and_then(|e| e.parent()).and_then(|b| b.parent()).map(|p| p.join("std")) {
        if d.join("strings").is_dir() {
            return Some(d);
        }
    }
    let repo = Path::new(REPO_STD);
    repo.is_dir().then(|| crate::paths::canonical(&repo).unwrap_or_else(|_| repo.to_path_buf()))
}

/// `net.http` + `http.zl` -> `net/http/http.zl`.
pub fn rel_path(ns: &str, file: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for seg in ns.split('.') {
        p.push(seg);
    }
    p.push(file);
    p
}

/// Where a stdlib module source came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Disk(PathBuf),
    Embedded,
}

/// The source of module file `file` of namespace `ns`: from `dir` when it
/// has the file, else `embedded`.
pub fn load_from(dir: Option<&Path>, ns: &str, file: &str, embedded: &'static str) -> (Cow<'static, str>, Origin) {
    if let Some(d) = dir {
        let p = d.join(rel_path(ns, file));
        if let Ok(text) = std::fs::read_to_string(&p) {
            return (Cow::Owned(text), Origin::Disk(p));
        }
    }
    (Cow::Borrowed(embedded), Origin::Embedded)
}

/// [`load_from`] with [`std_dir`]; the origin of every loaded file is
/// recorded for [`loaded_origins`].
pub fn load(ns: &str, file: &str, embedded: &'static str) -> Cow<'static, str> {
    let (text, origin) = load_from(std_dir().as_deref(), ns, file, embedded);
    ORIGINS.lock().unwrap().push((format!("{ns}/{file}"), origin));
    text
}

static ORIGINS: std::sync::Mutex<Vec<(String, Origin)>> = std::sync::Mutex::new(Vec::new());

/// Gives memory freed after a large transient allocation (parsing the
/// whole stdlib once) back to the operating system where the allocator
/// keeps it otherwise (glibc).
pub fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: malloc_trim only releases free heap pages.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// The modules loaded so far in this process and where each came from.
pub fn loaded_origins() -> Vec<(String, Origin)> {
    ORIGINS.lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modules_load_from_disk_or_fall_back_to_embedded() {
        let dir = std::env::temp_dir().join(format!("zell-stdfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("net/http")).unwrap();
        std::fs::write(dir.join("net/http/http.zl"), "namespace net.http\n// from disk\n").unwrap();
        let (t, o) = load_from(Some(&dir), "net.http", "http.zl", "embedded");
        assert!(t.contains("from disk"));
        assert_eq!(o, Origin::Disk(dir.join("net/http/http.zl")));
        let (t, o) = load_from(Some(&dir), "net.http", "missing.zl", "embedded");
        assert_eq!((t.as_ref(), o), ("embedded", Origin::Embedded));
        let (_, o) = load_from(None, "net.http", "http.zl", "embedded");
        assert_eq!(o, Origin::Embedded);
        // An explicit ZELL_STD wins, even when it does not exist.
        assert_eq!(find_std_dir(Some(dir.clone()), None), Some(dir.clone()));
        assert_eq!(find_std_dir(Some(dir.join("nope")), None), None);
        // <exe>/../std of an installed toolchain.
        std::fs::create_dir_all(dir.join("tc/bin")).unwrap();
        std::fs::create_dir_all(dir.join("tc/std/strings")).unwrap();
        assert_eq!(find_std_dir(None, Some(dir.join("tc/bin/zell"))), Some(dir.join("tc/std")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repository_std_tree_is_generated() {
        let repo = Path::new(REPO_STD);
        assert!(repo.join("strings/strings_extern.zl").is_file(), "compiler/build.rs writes std/");
        assert!(repo.join("net/http/http_client.zl").is_file());
        let ext = std::fs::read_to_string(repo.join("strings/strings_extern.zl")).unwrap();
        assert!(ext.contains("extern func Split("), "{ext}");
        assert_eq!(rel_path("net.http", "http.zl"), PathBuf::from("net/http/http.zl"));
        // Every file of the tree parses (the extern files included).
        fn walk(d: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "zl") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(repo, &mut files);
        assert!(files.len() > 100, "{}", files.len());
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let mut diags = crate::diagnostics::Diagnostics::new();
            let tokens = crate::lexer::lex(&text, &mut diags);
            let ast = crate::parser::parse(tokens, &mut diags);
            assert!(ast.is_some() && !diags.has_errors(), "{}: {:?}", f.display(), diags.items.first().map(|d| &d.message));
        }
    }
}
