//! Targets of an installed toolchain (docs/PLATFORMS.md, "Установка и
//! цели", "Дистрибутив"): `zell target list`, `zell target add` and
//! `zell doctor`.
//!
//! Layout of a toolchain (`scripts/package-toolchain.sh`), self-contained:
//!
//! ```text
//! <root>/bin/zell, bin/zell-lsp, bin/zell-mcp
//! <root>/bin/lld (+ ld.lld, ld64.lld, lld-link)   the linker (crosslink.rs)
//! <root>/std/...                      standard library sources (sema/stdfs.rs)
//! <root>/targets/<triple>/            for every tier-1/2 target: BUNDLE, libzell_rt.a,
//!                                     compiler-rt builtins, crt objects, the C library
//!                                     (glibc stubs, musl libc.a, mingw-w64, libSystem.tbd)
//! <root>/tools/...                    debugger formatters, editor files
//! <root>/THIRD_PARTY.md               provenance and licences of bin/lld and targets/
//! ```
//!
//! Extra targets live in `$ZELL_TARGETS` (default `$ZELL_HOME/targets`,
//! i.e. `~/.zell/targets`), one directory per LLVM triple holding
//! `libzell_rt.a` (and the link inputs when it came from a release).
//! `zell target add <triple|alias>` downloads
//! `zell-runtime-<version>-<triple>.tar.gz` (which unpacks to
//! `<triple>/`) from the URL template `$ZELL_TARGETS_URL`
//! (default [`DEFAULT_URL`]; `{version}` and `{triple}` are substituted;
//! `https://` goes through curl, `http://` and `file://` are built in, a
//! plain path is a local archive), checks it against `<url>.sha256` when
//! the server has one, and unpacks it with the system `tar`.

use crate::target::Target;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where `zell target add` downloads from unless `$ZELL_TARGETS_URL` is set.
pub const DEFAULT_URL: &str = "https://github.com/zell-lang/zell/releases/download/v{version}/zell-runtime-{version}-{triple}.tar.gz";

/// Targets with prebuilt runtimes in a release (tier 1-2 of docs/PLATFORMS.md).
pub const KNOWN: &[(&str, &str)] = &[
    ("linux-amd64", "x86_64-unknown-linux-gnu"),
    ("linux-arm64", "aarch64-unknown-linux-gnu"),
    ("linux-amd64-musl", "x86_64-unknown-linux-musl"),
    ("linux-arm64-musl", "aarch64-unknown-linux-musl"),
    ("macos-arm64", "aarch64-apple-darwin"),
    ("macos-amd64", "x86_64-apple-darwin"),
    ("windows-amd64", "x86_64-pc-windows-gnu"),
];

/// The directory of downloaded target runtimes (`$ZELL_TARGETS`, else
/// `$ZELL_HOME/targets`).
pub fn targets_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("ZELL_TARGETS").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    crate::package::store::zell_home().join("targets")
}

/// `<exe>/../targets` of an installed toolchain.
pub fn bundled_targets_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.parent()?.join("targets"))
}

/// Every installed place a runtime for `triple` is looked for, in order:
/// the toolchain's own `targets/`, then `$ZELL_TARGETS`.
pub fn runtime_candidates(triple: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(b) = bundled_targets_dir() {
        v.push(b.join(triple).join("libzell_rt.a"));
    }
    v.push(targets_dir().join(triple).join("libzell_rt.a"));
    v
}

fn url_for(template: &str, triple: &str) -> String {
    template.replace("{version}", env!("CARGO_PKG_VERSION")).replace("{triple}", triple)
}

fn fetch(src: &str) -> Result<Vec<u8>, String> {
    if !src.contains("://") {
        return std::fs::read(src).map_err(|e| format!("cannot read {src}: {e}"));
    }
    match crate::package::http::get(src)? {
        Some(b) => Ok(b),
        None => Err(format!("{src}: not found (no prebuilt runtime for this version and target; set ZELL_TARGETS_URL, or build one with `make -C runtime CROSS=...`)")),
    }
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::package::lock::hex(&Sha256::digest(b))
}

/// `zell target add <triple|alias>... [--url template]`.
pub fn add(args: &[String]) -> Result<String, String> {
    add_into(args, &targets_dir())
}

/// [`add`] into directory `dest`.
fn add_into(args: &[String], dest: &Path) -> Result<String, String> {
    let mut template = std::env::var("ZELL_TARGETS_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_URL.to_string());
    let mut names = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" => template = it.next().ok_or("--url needs a template")?.clone(),
            s if s.starts_with('-') => return Err(format!("unknown option {s} (usage: zell target add <target>... [--url template])")),
            s => names.push(s.to_string()),
        }
    }
    if names.is_empty() {
        return Err("usage: zell target add <target>... (see `zell target list`)".into());
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut out = String::new();
    for n in names {
        let t = Target::parse(&n)?;
        let src = url_for(&template, &t.triple);
        let bytes = fetch(&src)?;
        if src.contains("://") {
            if let Ok(Some(sum)) = crate::package::http::get(&format!("{src}.sha256")) {
                let want = String::from_utf8_lossy(&sum).split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                let got = sha256_hex(&bytes);
                if !want.is_empty() && want != got {
                    return Err(format!("{src}: checksum mismatch (sha256 {got}, expected {want})"));
                }
            }
        }
        let tmp = dest.join(format!(".download-{}-{}.tar.gz", t.triple, std::process::id()));
        std::fs::write(&tmp, &bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        let st = Command::new("tar").arg("-xzf").arg(&tmp).arg("-C").arg(dest).output();
        let _ = std::fs::remove_file(&tmp);
        let st = st.map_err(|e| format!("cannot run tar: {e}"))?;
        if !st.status.success() {
            return Err(format!("unpacking {src} failed: {}", String::from_utf8_lossy(&st.stderr).trim()));
        }
        let lib = dest.join(&t.triple).join("libzell_rt.a");
        if !lib.is_file() {
            return Err(format!("{src} does not contain {}/libzell_rt.a", t.triple));
        }
        out.push_str(&format!("installed {} ({}) in {}\n", t.triple, n, lib.parent().unwrap().display()));
    }
    Ok(out)
}

/// Where the runtime of `t` is (any of the places the compiler looks), if anywhere.
fn installed(t: &Target) -> Option<PathBuf> {
    if t.is_host() {
        return crate::build::find_runtime().ok();
    }
    crate::crosslink::find_runtime(t).ok()
}

/// `zell target list`: the known targets and where their runtime is
/// (`bundled` for the toolchain's own link inputs).
pub fn list(_args: &[String]) -> Result<String, String> {
    let mut out = format!("{:<20} {:<28} {:<9} {}\n", "target", "triple", "linker", "runtime");
    let host = Target::host();
    let mut seen = Vec::new();
    let mut rows: Vec<(String, String)> = KNOWN.iter().map(|(a, t)| (a.to_string(), t.to_string())).collect();
    // runtimes downloaded or built for other triples too
    for dir in [Some(targets_dir()), bundled_targets_dir()].into_iter().flatten() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().join("libzell_rt.a").is_file() && !rows.iter().any(|(_, t)| *t == name) {
                    rows.push((String::new(), name));
                }
            }
        }
    }
    for (alias, triple) in rows {
        if seen.contains(&triple) {
            continue;
        }
        seen.push(triple.clone());
        let t = Target::parse(&triple)?;
        let (how, what) = match installed(&t) {
            Some(p) => {
                let bundled = bundled_targets_dir().is_some_and(|b| p.starts_with(crate::paths::canonical(&b).unwrap_or(b)));
                let how = match crate::crosslink::bundle_of(&p) {
                    Some(_) if bundled => "bundled",
                    Some(_) => "lld",
                    None => "clang",
                };
                (how, p.display().to_string())
            }
            None => ("-", "not installed (zell target add ".to_string() + if alias.is_empty() { &triple } else { &alias } + ")"),
        };
        let mark = if t.triple == host.triple { " (host)" } else { "" };
        out.push_str(&format!("{:<20} {:<28} {how:<9} {what}{mark}\n", alias, triple));
    }
    out.push_str(&format!("downloads go to {} (ZELL_TARGETS); source: {}\n", targets_dir().display(), std::env::var("ZELL_TARGETS_URL").unwrap_or_else(|_| DEFAULT_URL.into())));
    Ok(out)
}

/// The files a bundled target directory must hold to link (crosslink.rs
/// recipes), by target kind.
pub fn bundle_files(t: &Target) -> &'static [&'static str] {
    match (t.os.as_str(), t.is_musl(), t.triple.split('-').next().unwrap_or("")) {
        ("linux", true, _) => &["BUNDLE", "libzell_rt.a", "libclang_rt.builtins.a", "crt1.o", "crti.o", "crtn.o", "crtbegin.o", "crtend.o", "libc.a"],
        ("linux", false, "aarch64") => &[
            "BUNDLE", "libzell_rt.a", "libclang_rt.builtins.a", "Scrt1.o", "crti.o", "crtn.o", "crtbeginS.o", "crtendS.o", "libc_nonshared.a", "libc.so", "libm.so", "libpthread.so", "libdl.so", "librt.so",
            "ld-linux-aarch64.so.1", "libsqlite3.so",
        ],
        ("linux", false, _) => &[
            "BUNDLE", "libzell_rt.a", "libclang_rt.builtins.a", "Scrt1.o", "crti.o", "crtn.o", "crtbeginS.o", "crtendS.o", "libc_nonshared.a", "libc.so", "libm.so", "libpthread.so", "libdl.so", "librt.so",
            "ld-linux-x86-64.so.2", "libsqlite3.so",
        ],
        ("windows", _, _) => &[
            "BUNDLE", "libzell_rt.a", "libclang_rt.builtins.a", "crt2.o", "libmingw32.a", "libmingwex.a", "libmoldname.a", "libmsvcrt.a", "libkernel32.a", "libuser32.a", "libadvapi32.a", "libshell32.a", "libws2_32.a",
            "libiphlpapi.a", "libbcrypt.a",
        ],
        ("macos", _, _) => &["BUNDLE", "libzell_rt.a", "libclang_rt.builtins.a", "libSystem.tbd"],
        _ => &["BUNDLE", "libzell_rt.a"],
    }
}

/// `zell env ZELL_LINKER`: the linker links use (the bundled lld, or clang).
pub fn linker_description() -> String {
    use crate::crosslink::{find_lld, linker_choice, LinkerChoice};
    match (linker_choice(), find_lld()) {
        (LinkerChoice::Clang, _) => "clang".into(),
        (_, Some(l)) => l.program.display().to_string(),
        (LinkerChoice::Lld, None) => "lld (not found)".into(),
        (LinkerChoice::Auto, None) => "clang (no lld found)".into(),
    }
}

/// `zell env ZELL_TARGETS`: the target directories, bundled first.
pub fn targets_description() -> String {
    let mut v: Vec<String> = bundled_targets_dir().filter(|d| d.is_dir()).map(|d| d.display().to_string()).into_iter().collect();
    v.push(targets_dir().display().to_string());
    v.join(if cfg!(windows) { ";" } else { ":" })
}

/// `zell doctor`: checks that the toolchain is complete: the compiler,
/// `std/`, the linker, and the link inputs of every tier-1/2 target
/// (missing files are listed). Fails if anything the host needs is missing.
pub fn doctor(_args: &[String]) -> Result<String, String> {
    let mut out = String::new();
    let mut bad = 0;
    let mut line = |ok: bool, what: &str, detail: String| {
        out.push_str(&format!("{} {what:<28} {detail}\n", if ok { "ok  " } else { "FAIL" }));
        if !ok {
            bad += 1;
        }
    };
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    line(true, "zell", format!("{} ({exe})", env!("CARGO_PKG_VERSION")));
    match crate::sema::stdfs::std_dir() {
        Some(d) => line(true, "std", d.display().to_string()),
        None => line(true, "std", "embedded copies (no std/ on disk: IDE navigation into std needs ZELL_STD)".into()),
    }
    let lld = crate::crosslink::find_lld();
    match &lld {
        Some(l) => line(true, "linker", format!("{}{}", l.program.display(), if l.multi { " (multi-flavour)" } else { "" })),
        None => line(false, "linker", "no lld: bin/lld is missing from the toolchain and ld.lld is not on PATH".into()),
    }
    line(true, "ZELL_LINKER", match crate::crosslink::linker_choice() {
        crate::crosslink::LinkerChoice::Auto => "auto (bundled lld where link inputs are bundled, else clang)".into(),
        crate::crosslink::LinkerChoice::Lld => "lld".into(),
        crate::crosslink::LinkerChoice::Clang => "clang".into(),
    });
    let host = Target::host();
    let dirs: Vec<PathBuf> = [bundled_targets_dir(), Some(targets_dir())].into_iter().flatten().collect();
    line(true, "targets", dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", "));
    for (alias, triple) in KNOWN {
        let t = Target::parse(triple)?;
        let found = dirs.iter().map(|d| d.join(triple)).find(|d| d.join("libzell_rt.a").is_file());
        let is_host = t.triple == host.triple;
        match found {
            Some(d) => {
                let Ok(mark) = std::fs::read_to_string(d.join(crate::crosslink::BUNDLE_MARK)) else {
                    line(true, &format!("target {alias}"), format!("{} (runtime only: links with clang)", d.display()));
                    continue;
                };
                let missing: Vec<&str> = bundle_files(&t).iter().copied().filter(|f| !d.join(f).is_file()).collect();
                let libc = mark.lines().find_map(|l| l.strip_prefix("libc=")).unwrap_or("?");
                if missing.is_empty() {
                    line(true, &format!("target {alias}"), format!("{} ({libc})", d.display()));
                } else {
                    line(false, &format!("target {alias}"), format!("{}: missing {}", d.display(), missing.join(", ")));
                }
            }
            None if is_host => match crate::build::find_runtime() {
                Ok(rt) => line(true, &format!("target {alias} (host)"), format!("{} (runtime only: links with clang)", rt.display())),
                Err(e) => line(false, &format!("target {alias} (host)"), e),
            },
            None => line(true, &format!("target {alias}"), format!("not installed (zell target add {alias})")),
        }
    }
    if bad > 0 {
        return Err(format!("{out}{bad} problem(s): reinstall the toolchain (scripts/install.sh) or see docs/PLATFORMS.md, \"Дистрибутив\""));
    }
    out.push_str("toolchain complete\n");
    Ok(out)
}

/// `zell target <list|add> ...`.
pub fn run(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("list") | None => list(&args[args.len().min(1)..]),
        Some("add") => add(&args[1..]),
        Some(c) => Err(format!("unknown subcommand `target {c}` (expected list or add)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_template_and_known_targets() {
        let u = url_for(DEFAULT_URL, "x86_64-pc-windows-gnu");
        assert!(u.ends_with(&format!("zell-runtime-{}-x86_64-pc-windows-gnu.tar.gz", env!("CARGO_PKG_VERSION"))), "{u}");
        for (alias, triple) in KNOWN {
            assert_eq!(Target::parse(alias).unwrap().triple, *triple, "{alias}");
        }
    }

    #[test]
    fn add_unpacks_a_local_archive() {
        let dir = std::env::temp_dir().join(format!("zell-target-add-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/x86_64-unknown-linux-musl")).unwrap();
        std::fs::write(dir.join("src/x86_64-unknown-linux-musl/libzell_rt.a"), b"!<arch>\n").unwrap();
        let ar = dir.join("rt.tar.gz");
        let st = Command::new("tar").arg("-czf").arg(&ar).arg("-C").arg(dir.join("src")).arg("x86_64-unknown-linux-musl").status();
        if !st.is_ok_and(|s| s.success()) {
            return; // no tar here
        }
        let targets = dir.join("targets");
        let msg = add_into(&["linux-amd64-musl".into(), "--url".into(), ar.display().to_string()], &targets).unwrap();
        assert!(msg.contains("installed x86_64-unknown-linux-musl"), "{msg}");
        assert!(targets.join("x86_64-unknown-linux-musl/libzell_rt.a").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
