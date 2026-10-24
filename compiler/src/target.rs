//! The compilation target (docs/PLATFORMS.md): operating system and
//! architecture names as Zell sees them (`os.Name`, `os.Arch`, `@cfg`,
//! file-name suffixes).
//!
//! The target is process-wide: the host by default, or the one chosen with
//! `--target <triple|alias>` (set once by the CLI through [`set_current`]
//! before the front end runs).

use std::sync::RwLock;

/// Operating systems recognised in `@cfg(os: ...)` and file suffixes.
pub const KNOWN_OS: &[&str] = &["linux", "macos", "windows", "freebsd", "android", "ios", "wasi"];
/// Architectures recognised in `@cfg(arch: ...)` and file suffixes.
pub const KNOWN_ARCH: &[&str] = &["amd64", "arm64", "wasm32"];

/// A compilation target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// `linux`, `macos`, `windows`, ... (one of [`KNOWN_OS`] or the triple's OS).
    pub os: String,
    /// `amd64`, `arm64`, ... (one of [`KNOWN_ARCH`] or the triple's CPU).
    pub arch: String,
    /// LLVM target triple, e.g. `x86_64-unknown-linux-gnu`.
    pub triple: String,
}

impl Target {
    /// The machine the compiler runs on.
    pub fn host() -> Target {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            o => o,
        };
        let arch = arch_name(std::env::consts::ARCH);
        let triple = match (std::env::consts::ARCH, os) {
            (a, "linux") => format!("{a}-unknown-linux-gnu"),
            // The same spelling as `--target macos-arm64` and the toolchain's
            // `targets/aarch64-apple-darwin/`: the host runtime is looked up by it.
            (a, "macos") => format!("{a}-apple-darwin"),
            (a, "windows") => format!("{a}-pc-windows-gnu"),
            (a, o) => format!("{a}-unknown-{o}"),
        };
        Target { os: os.to_string(), arch, triple }
    }

    /// Parses `--target`: an alias `os-arch` (`linux-amd64`, `macos-arm64`,
    /// `windows-amd64`), `linux-<arch>-musl` (a fully static musl binary:
    /// `x86_64-unknown-linux-musl`), or an LLVM triple (`aarch64-apple-darwin`).
    /// `windows-*` means the MinGW ABI (`x86_64-pc-windows-gnu`): the
    /// runtime is built with mingw-w64 headers and linked against its CRT;
    /// the MSVC ABI (`x86_64-pc-windows-msvc`, CodeView/PDB, the Windows
    /// SDK's libraries) is accepted as a triple for `check`/`--emit=obj`.
    pub fn parse(s: &str) -> Result<Target, String> {
        if let Some(arch) = s.strip_prefix("linux-").and_then(|r| r.strip_suffix("-musl")) {
            if KNOWN_ARCH.contains(&arch) {
                let cpu = if arch == "amd64" { "x86_64" } else if arch == "arm64" { "aarch64" } else { arch };
                return Ok(Target { os: "linux".into(), arch: arch.into(), triple: format!("{cpu}-unknown-linux-musl") });
            }
        }
        if let Some((os, arch)) = s.split_once('-') {
            if KNOWN_OS.contains(&os) && KNOWN_ARCH.contains(&arch) {
                let cpu = match arch {
                    "amd64" => "x86_64",
                    "arm64" => "aarch64",
                    a => a,
                };
                let triple = match os {
                    "linux" => format!("{cpu}-unknown-linux-gnu"),
                    "macos" => format!("{cpu}-apple-darwin"),
                    "windows" => format!("{cpu}-pc-windows-gnu"),
                    o => format!("{cpu}-unknown-{o}"),
                };
                return Ok(Target { os: os.into(), arch: arch.into(), triple });
            }
        }
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() < 2 {
            return Err(format!("unknown target `{s}` (use a triple such as x86_64-unknown-linux-gnu or an alias such as linux-amd64)"));
        }
        let arch = arch_name(parts[0]);
        let rest = &parts[1..];
        let os = if rest.iter().any(|p| p.starts_with("linux")) {
            if rest.iter().any(|p| *p == "android") { "android" } else { "linux" }
        } else if rest.iter().any(|p| *p == "darwin" || p.starts_with("macos")) {
            "macos"
        } else if rest.iter().any(|p| p.starts_with("ios")) {
            "ios"
        } else if rest.iter().any(|p| *p == "windows") {
            "windows"
        } else if rest.iter().any(|p| p.starts_with("freebsd")) {
            "freebsd"
        } else if rest.iter().any(|p| *p == "wasi") {
            "wasi"
        } else {
            return Err(format!("unknown operating system in target `{s}`"));
        };
        Ok(Target { os: os.into(), arch, triple: s.to_string() })
    }

    /// True if this is the machine the compiler runs on (same OS,
    /// architecture and ABI), so the host CPU's features may be used and the
    /// output can run here.
    pub fn is_host(&self) -> bool {
        let h = Target::host();
        self.os == h.os && self.arch == h.arch && self.triple == h.triple
    }

    /// The object file format of the target.
    pub fn object_format(&self) -> ObjectFormat {
        match self.os.as_str() {
            "windows" => ObjectFormat::Coff,
            "macos" | "ios" => ObjectFormat::MachO,
            "wasi" => ObjectFormat::Wasm,
            _ => ObjectFormat::Elf,
        }
    }

    /// A musl libc target (`*-linux-musl`): linked fully static.
    pub fn is_musl(&self) -> bool {
        self.triple.ends_with("-musl")
    }

    /// The MSVC ABI (`*-windows-msvc`) rather than MinGW (`*-windows-gnu`).
    pub fn is_msvc(&self) -> bool {
        self.os == "windows" && self.triple.ends_with("-msvc")
    }

    /// Suffix of executables (`.exe` on Windows).
    pub fn exe_suffix(&self) -> &'static str {
        if self.os == "windows" { ".exe" } else { "" }
    }

    /// `os.PathSeparator`.
    pub fn path_separator(&self) -> &'static str {
        if self.os == "windows" { "\\" } else { "/" }
    }

    /// `os.LineSeparator`.
    pub fn line_separator(&self) -> &'static str {
        if self.os == "windows" { "\r\n" } else { "\n" }
    }

    /// True if a file named `path` is excluded on this target by its name
    /// suffix (Go style): `x_linux.zl`, `x_arm64.zl`, `x_windows_amd64.zl`.
    /// Only known OS and architecture names count as suffixes.
    pub fn excludes_file(&self, path: &str) -> bool {
        let name = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let parts: Vec<&str> = name.split('_').collect();
        if parts.len() < 2 {
            return false;
        }
        let last = parts[parts.len() - 1];
        let prev = if parts.len() >= 3 { Some(parts[parts.len() - 2]) } else { None };
        if KNOWN_ARCH.contains(&last) {
            if let Some(os) = prev.filter(|p| KNOWN_OS.contains(p)) {
                return os != self.os || last != self.arch;
            }
            return last != self.arch;
        }
        if KNOWN_OS.contains(&last) {
            return last != self.os;
        }
        false
    }
}

/// Object file formats ([`Target::object_format`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Elf,
    Coff,
    MachO,
    Wasm,
}

/// Zell's name of an LLVM/Rust CPU name.
fn arch_name(cpu: &str) -> String {
    match cpu {
        "x86_64" | "amd64" => "amd64".into(),
        "aarch64" | "arm64" => "arm64".into(),
        c => c.to_string(),
    }
}

static CURRENT: RwLock<Option<Target>> = RwLock::new(None);

/// Sets the target of this process (`--target`).
pub fn set_current(t: Target) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(t);
}

/// The current target: the one set with [`set_current`], else the host.
pub fn current() -> Target {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(Target::host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_machine_uses_the_compilers_triple() {
        // Not LLVM's default triple: a zell.exe built with the MSVC LLVM would
        // otherwise generate `x86_64-pc-windows-msvc` code for the MinGW runtime.
        let tm = crate::codegen::target_machine(&Target::host(), inkwell::OptimizationLevel::None).unwrap();
        assert_eq!(tm.get_triple().as_str().to_str().unwrap(), Target::host().triple);
    }

    #[test]
    fn parses_aliases_and_triples() {
        let t = Target::parse("windows-amd64").unwrap();
        assert_eq!((t.os.as_str(), t.arch.as_str(), t.triple.as_str()), ("windows", "amd64", "x86_64-pc-windows-gnu"));
        assert_eq!((t.object_format(), t.exe_suffix(), t.is_msvc()), (ObjectFormat::Coff, ".exe", false));
        assert!(Target::parse("x86_64-pc-windows-msvc").unwrap().is_msvc());
        assert_eq!(Target::parse("macos-arm64").unwrap().object_format(), ObjectFormat::MachO);
        assert!(Target::host().is_host() && !Target::parse("windows-amd64").unwrap().is_host() || cfg!(windows));
        let t = Target::parse("aarch64-apple-darwin").unwrap();
        assert_eq!((t.os.as_str(), t.arch.as_str()), ("macos", "arm64"));
        let t = Target::parse("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!((t.os.as_str(), t.arch.as_str()), ("linux", "amd64"));
        assert!(Target::parse("nonsense").is_err());
        let t = Target::parse("linux-amd64-musl").unwrap();
        assert_eq!((t.os.as_str(), t.arch.as_str(), t.triple.as_str(), t.is_musl()), ("linux", "amd64", "x86_64-unknown-linux-musl", true));
        assert_eq!(Target::parse("linux-arm64-musl").unwrap().triple, "aarch64-unknown-linux-musl");
        assert_eq!(Target::parse("linux-arm64").unwrap().triple, "aarch64-unknown-linux-gnu");
    }

    #[test]
    fn file_suffixes() {
        let linux = Target::parse("linux-amd64").unwrap();
        assert!(!linux.excludes_file("src/paths_linux.zl"));
        assert!(linux.excludes_file("src/paths_windows.zl"));
        assert!(linux.excludes_file("paths_arm64.zl"));
        assert!(!linux.excludes_file("paths_amd64.zl"));
        assert!(!linux.excludes_file("paths_linux_amd64.zl"));
        assert!(linux.excludes_file("paths_linux_arm64.zl"));
        assert!(!linux.excludes_file("my_paths.zl"));
        assert!(!linux.excludes_file("linux.zl"));
        assert_eq!(linux.path_separator(), "/");
    }
}
