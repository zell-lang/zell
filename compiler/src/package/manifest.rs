//! The package manifest: one schema, three syntaxes (`zell.toml`,
//! `zell.json`, `zell.yaml`), all read through serde into [`Manifest`].

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Manifest file names, in the order they are reported.
pub const MANIFEST_NAMES: [&str; 3] = ["zell.toml", "zell.json", "zell.yaml"];

/// Syntax of a manifest file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
    Yaml,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "toml" => Some(Format::Toml),
            "json" => Some(Format::Json),
            "yaml" | "yml" => Some(Format::Yaml),
            _ => None,
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Format::Toml => "zell.toml",
            Format::Json => "zell.json",
            Format::Yaml => "zell.yaml",
        }
    }

    pub fn of_path(p: &Path) -> Option<Format> {
        Format::parse(p.extension()?.to_str()?)
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::Toml => "toml",
            Format::Json => "json",
            Format::Yaml => "yaml",
        })
    }
}

/// A Zell package manifest (docs/PACKAGES.md).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[schemars(title = "Zell package manifest")]
pub struct Manifest {
    /// The package itself; absent in a virtual workspace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<Package>,
    /// Dependencies visible to the package's code.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub dependencies: IndexMap<String, Dependency>,
    /// Dependencies for tests only.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub dev_dependencies: IndexMap<String, Dependency>,
    /// Feature name -> features or `dependency/feature` it enables.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub features: IndexMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<Workspace>,
    /// `[capabilities]`: low-level standard modules the package may
    /// import (`unsafe = true` for `unsafe`, `plugin = true` for
    /// `plugin`; docs/DEBUGGER.md).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub capabilities: IndexMap<String, bool>,
    /// `[lints]`: lint code or name (`L0003`, `empty-catch`) -> level
    /// (`off`, `hint`, `info`, `warn`, `error`); docs/TOOLING.md "Инспекции".
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub lints: IndexMap<String, String>,
}

/// `[package]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Package {
    /// Package name: `[a-z0-9-]`, starting with a letter.
    pub name: String,
    /// Semantic version `MAJOR.MINOR.PATCH`.
    #[serde(deserialize_with = "stringish")]
    pub version: String,
    /// Root namespace owned by the package (a single identifier). Defaults
    /// to the name with `-` replaced by `_`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Entry file of a binary package, relative to the manifest. A package
    /// without `entry` is a library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A dependency: a version constraint (`"^1.2"`) or a detailed table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Dependency {
    /// Registry version constraint (`^1.2`, `~2.0.4`, `>=1, <2`, `=1.4.0`).
    Version(#[serde(deserialize_with = "stringish")] String),
    Detailed(DependencyDetail),
}

/// `{ version = "...", path = "...", git = "...", features = [...] }`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct DependencyDetail {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "opt_stringish")]
    pub version: Option<String>,
    /// Local directory with the dependency's manifest, relative to this manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

impl Dependency {
    pub fn path(&self) -> Option<&str> {
        match self {
            Dependency::Detailed(d) => d.path.as_deref(),
            Dependency::Version(_) => None,
        }
    }

    pub fn features(&self) -> &[String] {
        match self {
            Dependency::Detailed(d) => &d.features,
            Dependency::Version(_) => &[],
        }
    }
}

/// `[workspace]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Workspace {
    /// Member directories, relative to this manifest; `*` globs allowed.
    #[serde(default)]
    pub members: Vec<String>,
}

/// Accepts a string or a number (YAML users write `version: 1.0`).
fn stringish<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum S {
        S(String),
        I(i64),
        F(f64),
    }
    Ok(match S::deserialize(d)? {
        S::S(s) => s,
        S::I(i) => i.to_string(),
        S::F(f) => f.to_string(),
    })
}

fn opt_stringish<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    stringish(d).map(Some)
}

/// Rewrites `[dependencies.x]` sub-tables as inline tables
/// (`x = { path = "..." }`), the style of docs/PACKAGES.md.
fn inline_dependency_tables(text: &str) -> String {
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else { return text.to_string() };
    for section in ["dependencies", "dev-dependencies"] {
        let Some(table) = doc.get_mut(section).and_then(|t| t.as_table_mut()) else { continue };
        let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
        for k in keys {
            if let Some(t) = table.get(&k).and_then(|i| i.as_table()) {
                let inline = t.clone().into_inline_table();
                table.insert(&k, toml_edit::value(inline));
            }
        }
    }
    doc.to_string()
}

/// A manifest read from disk.
#[derive(Clone, Debug)]
pub struct LoadedManifest {
    /// Canonical path of the manifest file.
    pub path: PathBuf,
    pub format: Format,
    pub manifest: Manifest,
}

impl LoadedManifest {
    /// Canonical directory of the manifest.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap()
    }
}

impl Manifest {
    /// Parses `text` in `format`.
    pub fn parse(text: &str, format: Format) -> Result<Manifest, String> {
        match format {
            Format::Toml => toml::from_str(text).map_err(|e| e.to_string()),
            Format::Json => serde_json::from_str(text).map_err(|e| e.to_string()),
            Format::Yaml => {
                if text.trim().is_empty() {
                    return Ok(Manifest::default());
                }
                serde_yaml::from_str(text).map_err(|e| e.to_string())
            }
        }
    }

    /// Serialises the manifest in `format`.
    pub fn to_string(&self, format: Format) -> Result<String, String> {
        match format {
            Format::Toml => {
                let text = toml::to_string(self).map_err(|e| e.to_string())?;
                Ok(inline_dependency_tables(&text))
            }
            Format::Json => serde_json::to_string_pretty(self).map(|s| s + "\n").map_err(|e| e.to_string()),
            Format::Yaml => serde_yaml::to_string(self).map_err(|e| e.to_string()),
        }
    }

    /// The JSON Schema of the manifest (`zell manifest schema`).
    pub fn json_schema() -> String {
        let schema = schemars::schema_for!(Manifest);
        serde_json::to_string_pretty(&schema).unwrap() + "\n"
    }

    /// Checks field syntax that serde does not.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(p) = &self.package {
            if !valid_package_name(&p.name) {
                return Err(format!("invalid package name `{}`: use lowercase letters, digits and `-`, starting with a letter", p.name));
            }
            if !valid_version(&p.version) {
                return Err(format!("invalid version `{}` of package `{}`: expected MAJOR.MINOR.PATCH", p.version, p.name));
            }
            let ns = p.namespace_root();
            if !valid_namespace_root(&ns) {
                return Err(format!(
                    "invalid namespace `{ns}` of package `{}`: the namespace root must be a single identifier (lowercase letters, digits, `_`)",
                    p.name
                ));
            }
            if ns == "std" || crate::sema::stdlib::is_std_namespace(&ns) {
                return Err(format!("package `{}` cannot own the namespace `{ns}`: it belongs to the standard library; set another `namespace` in [package]", p.name));
            }
        } else if self.workspace.is_none() {
            return Err("manifest has neither [package] nor [workspace]".into());
        } else if !self.dependencies.is_empty() || !self.dev_dependencies.is_empty() || !self.features.is_empty() {
            return Err("a virtual workspace (no [package]) cannot have dependencies or features".into());
        }
        for (name, deps) in [("dependencies", &self.dependencies), ("dev-dependencies", &self.dev_dependencies)] {
            for (k, d) in deps {
                let module_key = super::resolve::is_module_key(k);
                if module_key && !super::store::valid_module_path(k) {
                    return Err(format!("invalid module path `{k}` in [{name}]"));
                }
                if !module_key && !valid_package_name(k) {
                    return Err(format!("invalid package name `{k}` in [{name}]"));
                }
                if module_key && d.path().is_some() {
                    return Err(format!("dependency `{k}` in [{name}]: a `path` dependency is keyed by its package name"));
                }
                if let Dependency::Detailed(dd) = d {
                    let sources = [dd.path.is_some(), dd.git.is_some()].iter().filter(|b| **b).count();
                    if sources > 1 {
                        return Err(format!("dependency `{k}`: `path` and `git` are mutually exclusive"));
                    }
                    if sources == 0 && dd.version.is_none() {
                        return Err(format!("dependency `{k}` needs `version`, `path` or `git`"));
                    }
                    if dd.git.is_none() && (dd.tag.is_some() || dd.branch.is_some() || dd.rev.is_some()) {
                        return Err(format!("dependency `{k}`: `tag`/`branch`/`rev` need `git`"));
                    }
                }
            }
        }
        for (f, items) in &self.features {
            for it in items {
                match split_feature_ref(it, &self.dependencies) {
                    Some((dep, _)) => {
                        if !self.dependencies.contains_key(dep) {
                            return Err(format!("feature `{f}` refers to `{it}`, but `{dep}` is not in [dependencies]"));
                        }
                    }
                    None => {
                        if !self.features.contains_key(it.as_str()) {
                            return Err(format!("feature `{f}` enables unknown feature `{it}`"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Splits a `dep/feature` reference of a `[features]` entry; `dep` may be
/// a module path (`github.com/org/pkg/tls`), matched against the keys of
/// `deps` first. `None` for a plain feature name.
pub fn split_feature_ref<'a, V>(item: &'a str, deps: &IndexMap<String, V>) -> Option<(&'a str, &'a str)> {
    for k in deps.keys() {
        if let Some(f) = item.strip_prefix(k.as_str()).and_then(|r| r.strip_prefix('/')) {
            if !f.contains('/') {
                return Some((&item[..k.len()], f));
            }
        }
    }
    item.rsplit_once('/')
}

impl Package {
    /// The root namespace: `namespace`, or the name with `-` -> `_`.
    pub fn namespace_root(&self) -> String {
        self.namespace.clone().unwrap_or_else(|| self.name.replace('-', "_"))
    }
}

pub fn valid_package_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !s.ends_with('-')
}

fn valid_namespace_root(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn valid_version(s: &str) -> bool {
    let core = s.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// The manifest files present in `dir`.
pub fn manifests_in(dir: &Path) -> Vec<PathBuf> {
    MANIFEST_NAMES.iter().map(|n| dir.join(n)).filter(|p| p.is_file()).collect()
}

/// The single manifest of `dir`: `Ok(None)` if there is none, an error if
/// there are several.
pub fn manifest_in(dir: &Path) -> Result<Option<PathBuf>, String> {
    let found = manifests_in(dir);
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.into_iter().next()),
        _ => Err(format!(
            "{} has several manifests ({}); keep exactly one",
            dir.display(),
            found.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Reads and validates the manifest at `path`.
pub fn load(path: &Path) -> Result<LoadedManifest, String> {
    let format = Format::of_path(path).ok_or_else(|| format!("{}: unknown manifest format", path.display()))?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let manifest = Manifest::parse(&text, format).map_err(|e| format!("{}: {}", path.display(), e.trim_end()))?;
    manifest.validate().map_err(|e| format!("{}: {e}", path.display()))?;
    let path = crate::paths::canonical(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(LoadedManifest { path, format, manifest })
}

/// Loads the manifest of directory `dir` (exactly one must exist).
pub fn load_dir(dir: &Path) -> Result<LoadedManifest, String> {
    match manifest_in(dir)? {
        Some(p) => load(&p),
        None => Err(format!("no manifest ({}) in {}", MANIFEST_NAMES.join(", "), dir.display())),
    }
}

/// Walks up from `start` to the nearest directory with a manifest.
pub fn find_upwards(start: &Path) -> Result<Option<PathBuf>, String> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if let Some(p) = manifest_in(d)? {
            return Ok(Some(p));
        }
        dir = d.parent();
    }
    Ok(None)
}

/// `zell manifest convert <fmt>`: rewrites the manifest of `dir` in
/// `format` and removes the old file. Returns (old, new) paths.
pub fn convert(dir: &Path, format: Format) -> Result<(PathBuf, PathBuf), String> {
    let lm = load_dir(dir)?;
    let new = lm.dir().join(format.file_name());
    if lm.format == format {
        return Ok((lm.path.clone(), new));
    }
    let text = lm.manifest.to_string(format)?;
    // Verify the round trip before touching the disk.
    let back = Manifest::parse(&text, format).map_err(|e| format!("internal error: converted manifest does not parse: {e}"))?;
    if back != lm.manifest {
        return Err("internal error: converted manifest differs from the original".into());
    }
    std::fs::write(&new, text).map_err(|e| format!("cannot write {}: {e}", new.display()))?;
    std::fs::remove_file(&lm.path).map_err(|e| format!("cannot remove {}: {e}", lm.path.display()))?;
    Ok((lm.path, new))
}
