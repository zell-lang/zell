//! Canonical file paths that compare equal across tools.
//!
//! `std::fs::canonicalize` returns verbatim paths on Windows
//! (`\\?\C:\Users\...`, `\\?\UNC\server\share\...`), which editors, debug
//! adapters and the IDE plugin never produce: a breakpoint set on
//! `C:\Users\...\main.zl` would not match the file table of the debug agent.
//! [`canonical`] strips the verbatim prefix so that the compiler, the debug
//! tables (`codegen/debug_agent.rs`, DWARF in `codegen/debug.rs`) and the
//! DAP client (`dap.rs`) all spell a path the way the user does. On other
//! platforms it is `canonicalize` unchanged.

use std::path::{Path, PathBuf};

/// `std::fs::canonicalize` without the Windows `\\?\` verbatim prefix.
pub fn canonical(p: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p).map(strip_verbatim)
}

/// [`canonical`], or `p` as given (joined to the current directory when
/// relative) when it cannot be resolved (the file does not exist yet).
pub fn canonical_or_absolute(p: impl AsRef<Path>) -> PathBuf {
    let p = p.as_ref();
    canonical(p).unwrap_or_else(|_| if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf()) })
}

/// Removes the `\\?\` (and `\\?\UNC\` → `\\`) prefix of a Windows verbatim
/// path; other paths are returned unchanged.
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = match p.to_str() {
        Some(s) => s,
        None => return p,
    };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefixes_are_stripped() {
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\C:\Users\me\main.zl")), PathBuf::from(r"C:\Users\me\main.zl"));
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share\a.zl")), PathBuf::from(r"\\srv\share\a.zl"));
        assert_eq!(strip_verbatim(PathBuf::from("/home/me/main.zl")), PathBuf::from("/home/me/main.zl"));
        assert_eq!(strip_verbatim(PathBuf::from(r"C:\plain\path.zl")), PathBuf::from(r"C:\plain\path.zl"));
    }

    #[test]
    fn canonical_resolves_existing_paths_without_prefix() {
        let dir = std::env::temp_dir();
        let c = canonical(&dir).unwrap();
        assert!(!c.to_string_lossy().starts_with(r"\\?\"));
        assert!(c.is_absolute());
        assert!(canonical_or_absolute("definitely/missing/file.zl").is_absolute());
    }
}
