//! `zell init [name] [--format toml|json|yaml] [--lib]`.

use super::manifest::{self, Format, Manifest, Package};
use std::path::{Path, PathBuf};

/// Creates a package in `dir` (created if missing): a manifest in `format`
/// and `src/main.zl` (binary) or `src/lib.zl` (library). `name` defaults
/// to the directory name. Returns the created files.
pub fn init(dir: &Path, name: Option<&str>, format: Format, lib: bool) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let existing = manifest::manifests_in(dir);
    if !existing.is_empty() {
        return Err(format!("{} already has a manifest ({})", dir.display(), existing[0].display()));
    }
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            let d = crate::paths::canonical(&dir).map_err(|e| e.to_string())?;
            d.file_name().map(|s| s.to_string_lossy().to_lowercase().replace(['_', ' ', '.'], "-")).unwrap_or_else(|| "app".into())
        }
    };
    if !manifest::valid_package_name(&name) {
        return Err(format!("invalid package name `{name}`: use lowercase letters, digits and `-`, starting with a letter"));
    }
    let ns = name.replace('-', "_");
    let src_file = if lib { "src/lib.zl" } else { "src/main.zl" };
    let m = Manifest {
        package: Some(Package {
            name: name.clone(),
            version: "0.1.0".into(),
            namespace: Some(ns.clone()),
            entry: (!lib).then(|| src_file.to_string()),
            license: None,
            description: None,
        }),
        ..Default::default()
    };
    let src = dir.join(src_file);
    if src.exists() {
        return Err(format!("{} already exists", src.display()));
    }
    let mpath = dir.join(format.file_name());
    std::fs::write(&mpath, m.to_string(format)?).map_err(|e| format!("cannot write {}: {e}", mpath.display()))?;
    std::fs::create_dir_all(dir.join("src")).map_err(|e| format!("cannot create src: {e}"))?;
    let body = if lib {
        format!("namespace {ns}\n\n// Exported (capitalised) names are visible to packages that depend on `{name}`.\nfunc Greeting(who string) string {{\n    return \"Hello, \" + who + \"!\"\n}}\n")
    } else {
        format!("namespace {ns}\n\nimport io\n\nfunc main() {{\n    println(\"Hello, world!\")\n}}\n")
    };
    std::fs::write(&src, body).map_err(|e| format!("cannot write {}: {e}", src.display()))?;
    Ok(vec![mpath, src])
}
