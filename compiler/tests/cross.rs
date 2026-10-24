//! Cross-compilation tests (docs/PLATFORMS.md): `zell build --target
//! windows-amd64` on Linux, the executables run under Wine (with the IOCP
//! poller and with the WSAPoll fallback) and compared byte for byte with the
//! examples' `.out` files; `--target linux-arm64` run under user-mode QEMU;
//! `--target linux-amd64-musl` (fully static) run natively; `--target
//! macos-arm64 --emit=obj` for every example and the runtime compiled
//! against `runtime/stub-sdk/macos` (no macOS SDK here, so no linking); the
//! C runtime's unit tests under Wine and QEMU.
//!
//! Needs clang with the mingw-w64 sysroot (`apt-get install mingw-w64`),
//! `wine`, `gcc-aarch64-linux-gnu` + `qemu-user-static`, `musl-tools`, and
//! the cross-built runtimes (`make -C runtime CROSS=...`, built by the tests
//! when missing). Without a toolchain a test prints why and passes (CI's
//! Linux cross job installs them all).
//!
//! Output: Zell's `println` writes `\n` on every platform and the runtime
//! puts stdout/stderr in binary mode, so a Windows executable's output is
//! compared as is, with no CRLF normalisation. Wine runs with
//! `LC_ALL=C.UTF-8` (it maps UTF-16 file names to the Unix locale's
//! charset, so non-ASCII names need a UTF-8 locale) and `WINEDEBUG=-all`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// Examples built and run by the staged self-contained toolchain.
#[path = "cross/bundle.rs"]
mod bundle;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn zell() -> &'static str {
    env!("CARGO_BIN_EXE_zell")
}

fn tmpdir(name: &str) -> PathBuf {
    // unique per call: tests run in parallel and may use the same name
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("zell-cross-{}-{n}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success())
}

/// Why the Windows cross tests cannot run here, or None if they can (the
/// cross runtime is built on first use).
fn windows_unavailable() -> Option<String> {
    static STATE: OnceLock<Option<String>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            if !Path::new("/usr/x86_64-w64-mingw32/include/windows.h").is_file() && !ok("x86_64-w64-mingw32-gcc", &["--version"]) {
                return Some("mingw-w64 is not installed".into());
            }
            if !ok("clang", &["--version"]) && !ok("clang-18", &["--version"]) {
                return Some("clang is not installed".into());
            }
            if !ok("wine", &["--version"]) {
                return Some("wine is not installed".into());
            }
            let lib = repo().join("runtime/build/x86_64-pc-windows-gnu/libzell_rt.a");
            let st = Command::new("make").arg("-C").arg(repo().join("runtime")).arg("CROSS=x86_64-w64-mingw32").arg("-j8").output().expect("cannot run make");
            if !st.status.success() || !lib.is_file() {
                return Some(format!("make -C runtime CROSS=x86_64-w64-mingw32 failed:\n{}", String::from_utf8_lossy(&st.stderr)));
            }
            // A fresh Wine prefix has no %TEMP% directory yet.
            let _ = wine().args(["cmd", "/c", "if not exist %TEMP% mkdir %TEMP%"]).output();
            None
        })
        .clone()
}

fn wine() -> Command {
    let mut c = Command::new("wine");
    c.env("WINEDEBUG", "-all").env("LC_ALL", "C.UTF-8").env("ZELL_NO_BACKTRACE", "1");
    c
}

fn build(src: &Path, out: &Path, extra: &[&str]) -> Result<(), String> {
    build_for("windows-amd64", src, out, extra)
}

fn build_for(target: &str, src: &Path, out: &Path, extra: &[&str]) -> Result<(), String> {
    let b = Command::new(zell()).arg("build").arg(src).arg("--target").arg(target).arg("-o").arg(out).args(extra).output().unwrap();
    if !b.status.success() {
        return Err(format!("build failed:\n{}", String::from_utf8_lossy(&b.stderr)));
    }
    Ok(())
}

fn run_wine(exe: &Path) -> Output {
    wine().arg(exe).output().unwrap()
}

/// Every example (name, source, expected output), like run_examples.rs.
fn all_examples() -> Vec<(String, PathBuf, String)> {
    let dir = repo().join("examples");
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_stem().unwrap().to_string_lossy().to_string();
        if p.extension().is_some_and(|x| x == "zl") && p.with_extension("out").is_file() {
            out.push((name, p.clone(), std::fs::read_to_string(p.with_extension("out")).unwrap()));
        } else if p.is_dir() && name != "errors_compile" {
            if let Some(o) = std::fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()).find(|f| f.extension().is_some_and(|x| x == "out")) {
                out.push((name, p.clone(), std::fs::read_to_string(o).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// Source and expected output of example `name` (a file or a directory).
fn example(name: &str) -> (PathBuf, String) {
    let file = repo().join("examples").join(format!("{name}.zl"));
    if file.is_file() {
        return (file.clone(), std::fs::read_to_string(file.with_extension("out")).unwrap());
    }
    let dir = repo().join("examples").join(name);
    let out = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|x| x == "out")).unwrap();
    (dir, std::fs::read_to_string(out).unwrap())
}

/// Builds every example for Windows and runs it under Wine; returns the
/// failures.
fn check_examples(names: &[&str], extra: &[&str]) -> Vec<String> {
    check_examples_env(names, extra, &[])
}

/// [`check_examples`] with extra environment for the Windows processes.
fn check_examples_env(names: &[&str], extra: &[&str], env: &[(&str, &str)]) -> Vec<String> {
    let dir = tmpdir(&format!("ex{}{}", extra.join(""), env.len()));
    let mut failures = Vec::new();
    for name in names {
        let (src, expected) = example(name);
        let exe = dir.join(format!("{name}.exe"));
        if let Err(e) = build(&src, &exe, extra) {
            failures.push(format!("{name}: {e}"));
            continue;
        }
        let r = wine().envs(env.iter().copied()).arg(&exe).output().unwrap();
        let got = String::from_utf8_lossy(&r.stdout);
        if !r.status.success() {
            failures.push(format!("{name}: exit status {:?}, stderr:\n{}", r.status.code(), String::from_utf8_lossy(&r.stderr)));
        } else if got != expected {
            failures.push(format!("{name}: output differs\n--- expected\n{expected}--- got\n{got}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    failures
}

/// Examples that run under Wine with the Linux `.out` byte for byte.
const WINDOWS_EXAMPLES: &[&str] = &[
    "hello", "arith", "control", "functions", "structs", "switch", "enums", "slices_maps", "strings", "interfaces",
    "errors", "errors_trace", "defer", "bank", "async", "cancel", "channels", "drop", "generics", "weak", "json",
    "reflection", "regex", "sealed_shapes", "try_block", "task_context", "tracing", "log", "metrics", "cli_tool",
    "stdlib_collections_algos", "stdlib_crypto", "stdlib_formats", "stdlib_io_encoding", "stdlib_money",
    "stdlib_strings2", "multi", "cpuprofile",
];

/// Networking examples (Winsock2, loopback only), run with the IOCP poller
/// (poll_iocp.c, the default) and again with the WSAPoll fallback
/// (ZELL_POLLER=wsapoll). http_realtime (HTTP/2, WebSocket, SSE) failed
/// once with a connection reset in the very first run on a fresh Wine
/// prefix and passed in every run since.
const WINDOWS_NET_EXAMPLES: &[&str] = &["tcp_echo", "http_server", "tls_echo", "redis", "http_realtime"];

/// Examples whose expected output is Linux-specific or that need what the
/// Windows port does not have (see the ignored test below).
const WINDOWS_NOT_APPLICABLE: &[(&str, &str)] = &[
    ("cfg", "its windows variant calls a winapi namespace that does not exist (the example only type-checks the host variant)"),
    ("stdlib_time_os_fs", "fs paths use '\\\\' and drive letters on Windows; the .out has Unix paths"),
    ("sql", "database.sqlite binds sqlite3.dll / winsqlite3.dll at run time; Wine ships neither (see windows_sqlite_with_dll)"),
];

#[test]
fn windows_examples_under_wine() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let f = check_examples(WINDOWS_EXAMPLES, &["--leak-check"]);
    assert!(f.is_empty(), "{} example(s) failed for windows-amd64:\n{}", f.len(), f.join("\n"));
}

#[test]
fn windows_examples_release_under_wine() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let f = check_examples(&["hello", "structs", "errors", "async", "strings", "slices_maps", "json"], &["--release"]);
    assert!(f.is_empty(), "{} example(s) failed for windows-amd64 --release:\n{}", f.len(), f.join("\n"));
}

#[test]
fn windows_net_examples_under_wine() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let f = check_examples(WINDOWS_NET_EXAMPLES, &[]);
    assert!(f.is_empty(), "{} network example(s) failed for windows-amd64:\n{}", f.len(), f.join("\n"));
}

#[test]
fn windows_net_examples_wsapoll_under_wine() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let f = check_examples_env(WINDOWS_NET_EXAMPLES, &[], &[("ZELL_POLLER", "wsapoll")]);
    assert!(f.is_empty(), "{} network example(s) failed with ZELL_POLLER=wsapoll:\n{}", f.len(), f.join("\n"));
}

/// database.sqlite on Windows with a sqlite3.dll next to the program
/// ($ZELL_TEST_SQLITE3_DLL: e.g. built from the amalgamation with
/// `x86_64-w64-mingw32-gcc -O1 -shared -DSQLITE_API=__declspec(dllexport)
/// sqlite3.c -o sqlite3.dll`); without one, Open fails cleanly.
#[test]
fn windows_sqlite_with_dll() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let dir = tmpdir("sqlite");
    let (src, expected) = example("sql");
    let exe = dir.join("sql.exe");
    build(&src, &exe, &[]).unwrap();
    let r = run_wine(&exe);
    let dll = std::env::var_os("ZELL_TEST_SQLITE3_DLL").map(PathBuf::from).filter(|p| p.is_file());
    match dll {
        None => {
            assert!(!r.status.success());
            assert!(String::from_utf8_lossy(&r.stderr).contains("no sqlite3.dll or winsqlite3.dll"), "{}", String::from_utf8_lossy(&r.stderr));
            eprintln!("no ZELL_TEST_SQLITE3_DLL: checked the clean failure only");
        }
        Some(d) => {
            std::fs::copy(&d, dir.join("sqlite3.dll")).unwrap();
            let r = run_wine(&exe);
            assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
            assert_eq!(String::from_utf8_lossy(&r.stdout), expected);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Windows-only platform pieces under Wine: child processes through named
/// pipes waited on by the IOCP poller (and through relay threads with
/// WSAPoll), IANA time zones from the registry (same output as Linux's
/// tzdata), the ROOT certificate store, and backtraces with symbol names.
#[test]
fn windows_processes_zones_roots_backtraces() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let dir = tmpdir("winplat");
    let src = dir.join("winplat.zl");
    std::fs::write(
        &src,
        r#"namespace t.winplat

import io
import crypto

func inner(xs []int) int {
    return xs[5]
}

async func main() void! {
    var args = os.Args()
    if len(args) > 1 && args[1] == "child" {
        var data = io.ReadAll()?
        println("child got", len(data), "bytes")
        io.Eprintln("child stderr")
        return
    }
    if len(args) > 1 && args[1] == "panic" {
        println(inner([1, 2]))
        return
    }
    var p = process.Start(args[0], ["child"], process.Options{Stdin: process.Pipe, Stdout: process.Pipe, Stderr: process.Pipe})?
    var chunk = strings.Bytes(strings.Repeat("x", 100000))
    var total = 0
    for _ in 0..20 {
        total += await p.Stdin.Write(chunk)?
    }
    p.Stdin.Close()
    var out = await p.Stdout.ReadAll()?
    var errs = await p.Stderr.ReadAll()?
    println("wrote", total, "exit", await p.Wait()?)
    print(strings.FromBytes(out))
    print(strings.FromBytes(errs))
    for n in ["America/New_York", "Europe/Berlin", "Australia/Sydney", "Asia/Kolkata", "America/St_Johns"] {
        var loc = time.LoadLocation(n)?
        for s in [1704067200, 1720000000, 2000000000] {
            println(n, time.Unix(s, 0).In(loc).Format("2006-01-02 15:04:05 MST -0700"))
        }
    }
    println("roots:", x509.SystemRoots()?.Len() > 0)
}
"#,
    )
    .unwrap();
    let exe = dir.join("winplat.exe");
    build(&src, &exe, &[]).unwrap();
    let linux = Command::new(zell()).arg("run").arg(&src).output().unwrap();
    assert!(linux.status.success(), "{}", String::from_utf8_lossy(&linux.stderr));
    for poller in ["iocp", "wsapoll"] {
        let r = wine().env("ZELL_POLLER", poller).env_remove("SSL_CERT_FILE").arg(&exe).output().unwrap();
        assert!(r.status.success(), "{poller}: {}", String::from_utf8_lossy(&r.stderr));
        assert_eq!(String::from_utf8_lossy(&r.stdout), String::from_utf8_lossy(&linux.stdout), "{poller}");
    }
    assert!(String::from_utf8_lossy(&linux.stdout).contains("wrote 2000000 exit 0\nchild got 2000000 bytes\nchild stderr\n"));
    let r = wine().env_remove("ZELL_NO_BACKTRACE").arg(&exe).arg("panic").output().unwrap();
    let err = String::from_utf8_lossy(&r.stderr);
    // Zell frames: file:line from the line table (ABI.md 8.2), as on Linux
    assert!(err.contains("winplat.zl:") && err.contains(" inner\n"), "backtrace without file:line:\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "documents the examples that do not apply to Windows yet (each has a reason in WINDOWS_NOT_APPLICABLE); run with --ignored to see their state"]
fn windows_not_applicable_examples() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let names: Vec<&str> = WINDOWS_NOT_APPLICABLE.iter().map(|(n, _)| *n).collect();
    let f = check_examples(&names, &[]);
    for (n, why) in WINDOWS_NOT_APPLICABLE {
        let failed = f.iter().any(|m| m.starts_with(&format!("{n}:")));
        eprintln!("{n}: {} ({why})", if failed { "fails" } else { "passes" });
    }
}

/// os.Name/os.Arch/PathSeparator follow the target, the UTF-8 command line
/// arrives intact, non-ASCII output is UTF-8 bytes, and the exit code
/// passes through.
#[test]
fn windows_target_constants_args_and_exit_code() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let dir = tmpdir("consts");
    let src = dir.join("consts.zl");
    std::fs::write(
        &src,
        "namespace t.consts\n\nimport io\n\nfunc main() {\n    println(os.Name, os.Arch, os.PathSeparator, len(os.LineSeparator))\n    var a = os.Args()\n    println(len(a), a[1], a[2])\n    println(\"h\u{e9}llo \u{2713}\")\n    os.Exit(3)\n}\n",
    )
    .unwrap();
    let exe = dir.join("consts.exe");
    build(&src, &exe, &[]).unwrap();
    let r = wine().arg(&exe).arg("x y").arg("\u{e9}").output().unwrap();
    assert_eq!(r.status.code(), Some(3), "stderr: {}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(String::from_utf8_lossy(&r.stdout), "windows amd64 \\ 2\n3 x y \u{e9}\nh\u{e9}llo \u{2713}\n");
    // `zell run --target windows-amd64` goes through Wine.
    let r = Command::new(zell()).arg("run").arg("--target").arg("windows-amd64").arg(&src).arg("--").arg("p").arg("q").env("LC_ALL", "C.UTF-8").output().unwrap();
    assert_eq!(r.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&r.stdout).starts_with("windows amd64"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn windows_exe_is_pe_and_default_name_has_exe_suffix() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let dir = tmpdir("pe");
    let r = Command::new(zell()).arg("build").arg("--target").arg("windows-amd64").arg("--release").arg(repo().join("examples/hello.zl")).current_dir(&dir).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let exe = dir.join("hello.exe");
    let bytes = std::fs::read(&exe).unwrap();
    assert_eq!(&bytes[..2], b"MZ");
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
    assert_eq!(u16::from_le_bytes(bytes[pe + 4..pe + 6].try_into().unwrap()), 0x8664, "machine: AMD64");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Why the linux-arm64 tests cannot run here (QEMU or the aarch64 gcc
/// sysroot missing), or None; builds the runtime when needed.
fn arm64_unavailable() -> Option<String> {
    static STATE: OnceLock<Option<String>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            if !ok("qemu-aarch64-static", &["--version"]) && !ok("qemu-aarch64", &["--version"]) {
                return Some("qemu-user-static is not installed".into());
            }
            if !Path::new("/usr/aarch64-linux-gnu/lib").is_dir() {
                return Some("gcc-aarch64-linux-gnu (the aarch64 sysroot) is not installed".into());
            }
            let st = Command::new("make").arg("-C").arg(repo().join("runtime")).arg("CROSS=aarch64-linux-gnu").arg("-j8").output().expect("cannot run make");
            if !st.status.success() {
                return Some(format!("make -C runtime CROSS=aarch64-linux-gnu failed:\n{}", String::from_utf8_lossy(&st.stderr)));
            }
            None
        })
        .clone()
}

/// Why the musl tests cannot run here, or None.
fn musl_unavailable() -> Option<String> {
    static STATE: OnceLock<Option<String>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            if std::env::consts::ARCH != "x86_64" || !ok("musl-gcc", &["--version"]) {
                return Some("musl-tools (musl-gcc) is not installed, or the host is not x86_64".into());
            }
            let st = Command::new("make").arg("-C").arg(repo().join("runtime")).arg("CROSS=x86_64-linux-musl").arg("-j8").output().expect("cannot run make");
            if !st.status.success() {
                return Some(format!("make -C runtime CROSS=x86_64-linux-musl failed:\n{}", String::from_utf8_lossy(&st.stderr)));
            }
            None
        })
        .clone()
}

/// Examples that need a target library this machine does not have for
/// linux-arm64 / musl: database.sqlite links libsqlite3 (arm64: not in
/// the cross sysroot; musl: no static libsqlite3, the runtime has no sqlite).
const NEEDS_SQLITE: &[&str] = &["sql", "web_app"];

/// Builds every example for `target` and runs it through `zell run
/// --target` (QEMU for arm64, natively for musl); returns the failures.
fn check_target(target: &str, extra: &[&str]) -> Vec<String> {
    let dir = tmpdir(&format!("{target}{}", extra.join("")));
    let mut failures = Vec::new();
    for (name, src, expected) in all_examples() {
        if NEEDS_SQLITE.contains(&name.as_str()) {
            continue;
        }
        let exe = dir.join(&name);
        if let Err(e) = build_for(target, &src, &exe, extra) {
            failures.push(format!("{name}: {e}"));
            continue;
        }
        let mut cmd = if target.starts_with("linux-arm64") {
            let mut c = Command::new(if ok("qemu-aarch64-static", &["--version"]) { "qemu-aarch64-static" } else { "qemu-aarch64" });
            c.arg("-L").arg("/usr/aarch64-linux-gnu").arg(&exe);
            c
        } else {
            Command::new(&exe)
        };
        let r = cmd.env("ZELL_NO_BACKTRACE", "1").output().unwrap();
        let got = String::from_utf8_lossy(&r.stdout);
        if !r.status.success() {
            failures.push(format!("{name}: exit status {:?}, stderr:\n{}", r.status.code(), String::from_utf8_lossy(&r.stderr)));
        } else if got != expected {
            failures.push(format!("{name}: output differs\n--- expected\n{expected}--- got\n{got}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    failures
}

/// Every example built for linux-arm64 and run under user-mode QEMU.
#[test]
fn linux_arm64_examples_under_qemu() {
    if let Some(why) = arm64_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let f = check_target("linux-arm64", &["--leak-check"]);
    assert!(f.is_empty(), "{} example(s) failed for linux-arm64:\n{}", f.len(), f.join("\n"));
    // `zell run --target linux-arm64` goes through QEMU too
    let r = Command::new(zell()).arg("run").arg("--target").arg("linux-arm64").arg(repo().join("examples/hello.zl")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&r.stdout), "Hello, Zell!\n", "{}", String::from_utf8_lossy(&r.stderr));
}

/// Every example as a fully static musl executable.
#[test]
fn linux_musl_static_examples() {
    if let Some(why) = musl_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    let f = check_target("linux-amd64-musl", &["--leak-check"]);
    assert!(f.is_empty(), "{} example(s) failed for linux-amd64-musl:\n{}", f.len(), f.join("\n"));
    let dir = tmpdir("musl-static");
    let exe = dir.join("hello");
    build_for("linux-amd64-musl", &repo().join("examples/hello.zl"), &exe, &["--release"]).unwrap();
    let b = std::fs::read(&exe).unwrap();
    // ELF without a PT_INTERP program header: statically linked
    let phoff = u64::from_le_bytes(b[0x20..0x28].try_into().unwrap()) as usize;
    let (phentsize, phnum) = (u16::from_le_bytes(b[0x36..0x38].try_into().unwrap()) as usize, u16::from_le_bytes(b[0x38..0x3a].try_into().unwrap()) as usize);
    let interp = (0..phnum).any(|i| u32::from_le_bytes(b[phoff + i * phentsize..phoff + i * phentsize + 4].try_into().unwrap()) == 3);
    assert!(!interp, "the musl executable has an ELF interpreter (not static)");
    assert_eq!(Command::new(&exe).output().unwrap().stdout, b"Hello, Zell!\n");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The runtime's C tests built for linux-arm64 (QEMU) and musl.
#[test]
#[ignore = "slow (19 C test binaries per target under QEMU / natively, several minutes); `make -C runtime CROSS=aarch64-linux-gnu test-cross` / CROSS=x86_64-linux-musl"]
fn runtime_c_tests_arm64_and_musl() {
    for (cross, why) in [("aarch64-linux-gnu", arm64_unavailable()), ("x86_64-linux-musl", musl_unavailable())] {
        if let Some(why) = why {
            eprintln!("skipping {cross}: {why}");
            continue;
        }
        let r = Command::new("make").arg("-C").arg(repo().join("runtime")).arg(format!("CROSS={cross}")).arg("test-cross").output().unwrap();
        let out = String::from_utf8_lossy(&r.stdout);
        assert!(r.status.success(), "C tests for {cross}:\n{out}");
    }
}

/// The runtime compiles for macOS (arm64 and x86_64, every file and the C
/// tests, -Werror) against the stub SDK; with no SDK, no archive is made.
#[test]
fn macos_runtime_compiles_against_stub_sdk() {
    if !ok("clang", &["--version"]) {
        eprintln!("skipping: clang is not installed");
        return;
    }
    for cross in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        let r = Command::new("make").arg("-C").arg(repo().join("runtime")).arg(format!("CROSS={cross}")).arg("SDKROOT=").arg("-j8").output().unwrap();
        let out = String::from_utf8_lossy(&r.stdout);
        assert!(r.status.success() && out.contains("stub-SDK compile check passed"), "{cross}:\n{out}\n{}", String::from_utf8_lossy(&r.stderr));
    }
}

/// Every example compiles to a Mach-O object for macos-arm64.
#[test]
fn macos_arm64_objects_for_all_examples() {
    let dir = tmpdir("macos-all");
    let mut failures = Vec::new();
    for (name, src, _) in all_examples() {
        let obj = dir.join(format!("{name}.o"));
        let r = Command::new(zell()).arg("build").arg("--target").arg("macos-arm64").arg("--emit=obj").arg(&src).arg("-o").arg(&obj).output().unwrap();
        let magic = std::fs::read(&obj).ok().filter(|b| b.len() >= 4).map(|b| u32::from_le_bytes(b[0..4].try_into().unwrap()));
        if !r.status.success() || magic != Some(0xfeed_facf) {
            failures.push(format!("{name}: {}", String::from_utf8_lossy(&r.stderr)));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Without a macOS SDK, `--emit=obj` still produces a Mach-O arm64 object,
/// and linking says what is missing.
#[test]
fn macos_arm64_emits_macho_object() {
    let dir = tmpdir("macos");
    let obj = dir.join("async.o");
    let r = Command::new(zell()).arg("build").arg("--target").arg("macos-arm64").arg("--emit=obj").arg(repo().join("examples/async.zl")).arg("-o").arg(&obj).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let b = std::fs::read(&obj).unwrap();
    assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), 0xfeed_facf, "MH_MAGIC_64");
    assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 0x0100_000c, "CPU_TYPE_ARM64");
    assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 1, "MH_OBJECT");
    if std::env::var_os("SDKROOT").is_none() && !ok("xcrun", &["--version"]) {
        // with bundled link inputs (targets/aarch64-apple-darwin/BUNDLE, e.g.
        // from scripts/bundle-targets.sh) the link succeeds without an SDK;
        // without them it fails and names the SDK and `zell target add`
        let exe = dir.join("hello");
        let r = Command::new(zell()).arg("build").arg("--target").arg("macos-arm64").arg(repo().join("examples/hello.zl")).arg("-o").arg(&exe).output().unwrap();
        if r.status.success() {
            let b = std::fs::read(&exe).unwrap();
            assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), 0xfeed_facf, "MH_MAGIC_64");
            assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 2, "MH_EXECUTE");
        } else {
            let e = String::from_utf8_lossy(&r.stderr);
            assert!(e.contains("macOS SDK") && e.contains("zell target add"), "{e}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Other targets' objects: ELF arm64 and COFF for the MSVC ABI.
#[test]
fn other_target_objects() {
    let dir = tmpdir("objs");
    for (target, magic) in [("linux-arm64", &b"\x7fELF"[..]), ("x86_64-pc-windows-msvc", &b"\x64\x86"[..])] {
        let obj = dir.join(format!("{target}.o"));
        let r = Command::new(zell()).arg("build").arg("--target").arg(target).arg("--emit=obj").arg(repo().join("examples/structs.zl")).arg("-o").arg(&obj).output().unwrap();
        assert!(r.status.success(), "{target}: {}", String::from_utf8_lossy(&r.stderr));
        let b = std::fs::read(&obj).unwrap();
        assert_eq!(&b[..magic.len()], magic, "{target}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "slow (builds and runs the 19 C test binaries under Wine, about two minutes); `cargo test --test cross -- --ignored` or `make -C runtime CROSS=x86_64-w64-mingw32 test-wine`"]
fn runtime_c_tests_under_wine() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    // the IOCP poller (default), then the WSAPoll fallback
    for poller in ["", "wsapoll"] {
        let r = Command::new("make").arg("-C").arg(repo().join("runtime")).arg("CROSS=x86_64-w64-mingw32").arg("test-wine").env("ZELL_POLLER", poller).output().unwrap();
        let out = String::from_utf8_lossy(&r.stdout);
        let fail = out.lines().find(|l| l.starts_with("WINE FAIL:")).unwrap_or("");
        assert!(r.status.success() && fail.trim() == "WINE FAIL:", "C tests under wine (ZELL_POLLER={poller:?}):\n{out}");
    }
}

/// On a console (a pseudo-terminal through `script`, which Wine turns into
/// a Windows console) the output goes through WriteConsoleW: the UTF-8
/// text must survive the UTF-16 conversion, including sequences split
/// across print calls. BMP characters only: Wine 9's console writes the
/// two halves of a surrogate pair (an emoji) as two U+FFFD, even for a
/// plain C WriteConsoleW call.
#[test]
fn windows_console_output_is_utf8() {
    if let Some(why) = windows_unavailable() {
        eprintln!("skipping: {why}");
        return;
    }
    if !ok("script", &["--version"]) {
        eprintln!("skipping: `script` (util-linux) is not installed");
        return;
    }
    let dir = tmpdir("console");
    let src = dir.join("console.zl");
    std::fs::write(&src, "namespace t.console\n\nimport io\n\nfunc main() {\n    print(\"h\u{e9}\")\n    println(\"llo w\u{f6}rld \u{2713} \u{4e16}\u{754c}\")\n}\n").unwrap();
    let exe = dir.join("console.exe");
    build(&src, &exe, &[]).unwrap();
    let r = Command::new("script").arg("-qc").arg(format!("wine {}", exe.display())).arg("/dev/null").env("WINEDEBUG", "-all").env("LC_ALL", "C.UTF-8").output().unwrap();
    let text = String::from_utf8_lossy(&r.stdout);
    // drop the console's ANSI control sequences
    let mut plain = String::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        plain.push(c);
    }
    assert!(plain.contains("h\u{e9}llo w\u{f6}rld \u{2713} \u{4e16}\u{754c}"), "console output: {plain:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
