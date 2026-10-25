//! A real fault (DESIGN.md "Паника", ABI.md 8.3): a program that prints and
//! then reads a bad address through `unsafe` (a package with the `unsafe`
//! capability; spec tests are single files without one) keeps its earlier
//! output, prints `fatal error: unexpected signal SIGSEGV` with a Zell
//! backtrace and exits with code 2. Windows: `cross.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Writes the package; returns its directory.
fn fault_package(dir: &Path) -> PathBuf {
    let pkg = dir.join("fault");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(pkg.join("zell.toml"), "[package]\nname = \"fault\"\nversion = \"0.1.0\"\nentry = \"src/main.zl\"\n\n[capabilities]\nunsafe = true\n").unwrap();
    std::fs::write(
        pkg.join("src/main.zl"),
        "namespace fault\n\nimport io\nimport unsafe\n\nfunc read(addr int) int {\n    return unsafe.ReadInt(addr, 8)\n}\n\nfunc main() {\n    for i in 0..3 {\n        println(\"line\", i)\n    }\n    println(read(8))\n    println(\"not reached\")\n}\n",
    )
    .unwrap();
    pkg
}

#[test]
fn bad_address_read_flushes_output_and_reports_fatal_error() {
    let lib = repo().join("runtime/libzell_rt.a");
    assert!(lib.is_file(), "build the runtime first: make -C runtime");
    let dir = std::env::temp_dir().join(format!("zell-fatal-{}", std::process::id()));
    let pkg = fault_package(&dir);
    let exe = dir.join("fault-prog");
    let b = Command::new(env!("CARGO_BIN_EXE_zell")).arg("build").arg(&pkg).arg("-o").arg(&exe).output().unwrap();
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    // stdout is a pipe here: fully buffered, so the flush is what keeps it
    let r = Command::new(&exe).env_remove("ZELL_NO_BACKTRACE").output().unwrap();
    let (out, err) = (String::from_utf8_lossy(&r.stdout), String::from_utf8_lossy(&r.stderr));
    assert_eq!(r.status.code(), Some(2), "stderr: {err}");
    assert_eq!(out, "line 0\nline 1\nline 2\n");
    assert!(err.starts_with("fatal error: unexpected signal SIGSEGV\n"), "{err}");
    assert!(err.contains("segmentation violation addr=0x8"), "{err}");
    assert!(err.contains("backtrace (most recent call first):"), "{err}");
    assert!(err.contains("src/main.zl:14 main"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
