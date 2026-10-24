//! Project discovery and the package graph: workspace members, `path`
//! dependencies (resolved recursively, deduplicated by canonical path),
//! module dependencies (selected by [`super::resolve`], materialised in
//! `.zell/deps/` from the [`super::store`]), cycle detection and
//! namespace-root ownership.

use super::manifest::{self, Dependency, LoadedManifest};
use crate::sema::{ImportPolicy, PackageScope};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Index of a package in [`Project::packages`].
pub type PkgId = usize;

/// Where a package comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A local directory, relative to the workspace root (`/`-separated;
    /// `.` for the root itself; absolute if outside the root).
    Path(String),
    /// A module version (`zell.lock` `source`: `module`, or
    /// `git+<url>?<ref>#<commit>` for a git pin).
    Module { path: String, version: String, h1: String, lock_source: String },
}

impl Source {
    /// The `source` field of `zell.lock`.
    pub fn lock_id(&self) -> String {
        match self {
            Source::Path(p) => format!("path:{p}"),
            Source::Module { lock_source, .. } => lock_source.clone(),
        }
    }

    /// The module path of a module package.
    pub fn module(&self) -> Option<&str> {
        match self {
            Source::Module { path, .. } => Some(path),
            Source::Path(_) => None,
        }
    }
}

/// A package of the graph.
#[derive(Clone, Debug)]
pub struct PackageNode {
    pub name: String,
    pub version: String,
    /// Root namespace owned by the package.
    pub namespace: String,
    /// Canonical package directory.
    pub dir: PathBuf,
    pub manifest: LoadedManifest,
    pub source: Source,
    /// Direct `[dependencies]`.
    pub deps: Vec<PkgId>,
    /// The `[dependencies]` key of each of `deps` (module path or alias).
    pub dep_keys: Vec<String>,
    /// Direct `[dev-dependencies]` (resolved for workspace members only).
    pub dev_deps: Vec<PkgId>,
    /// A workspace member (or the single root package).
    pub member: bool,
}

impl PackageNode {
    /// Entry file of a binary package; `None` for a library.
    pub fn entry(&self) -> Option<PathBuf> {
        let p = self.manifest.manifest.package.as_ref()?;
        p.entry.as_ref().map(|e| self.dir.join(e))
    }

    pub fn is_bin(&self) -> bool {
        self.entry().is_some()
    }

    /// Source files: `src/**/*.zl` plus the entry file, sorted.
    pub fn source_files(&self) -> Result<Vec<PathBuf>, String> {
        let src = self.dir.join("src");
        let mut files = if src.is_dir() { crate::driver::collect_files(&src).map_err(|e| format!("cannot read {}: {e}", src.display()))? } else { Vec::new() };
        if let Some(e) = self.entry() {
            if !e.is_file() {
                return Err(format!("entry file {} of package `{}` does not exist", e.display(), self.name));
            }
            let e = crate::paths::canonical(&e).unwrap_or(e);
            if !files.iter().any(|f| crate::paths::canonical(&f).ok().as_ref() == Some(&e)) {
                files.push(e);
                files.sort();
            }
        }
        Ok(files)
    }
}

/// A discovered project: the root manifest and the whole package graph.
#[derive(Clone, Debug)]
pub struct Project {
    /// Canonical directory of the root (workspace) manifest; `zell.lock`
    /// and `target/` live here.
    pub root: PathBuf,
    pub root_manifest: LoadedManifest,
    pub packages: Vec<PackageNode>,
    /// Workspace members (including the root package, if any).
    pub members: Vec<PkgId>,
    /// The root manifest's own package, if it has `[package]`.
    pub root_package: Option<PkgId>,
    /// The member nearest to the directory the project was discovered
    /// from (what a plain `zell build` builds), if any.
    pub current: Option<PkgId>,
}

impl Project {
    /// Finds the project containing `start` (a directory): the nearest
    /// manifest, or an enclosing workspace that lists it as a member.
    /// `Ok(None)` if there is no manifest up to the filesystem root.
    pub fn discover(start: &Path) -> Result<Option<Project>, String> {
        let start = crate::paths::canonical(&start).map_err(|e| format!("cannot read {}: {e}", start.display()))?;
        let Some(nearest) = manifest::find_upwards(&start)? else { return Ok(None) };
        let nearest = manifest::load(&nearest)?;
        let mut root = nearest.clone();
        if nearest.manifest.workspace.is_none() {
            let mut dir = nearest.dir().parent();
            while let Some(d) = dir {
                if let Some(p) = manifest::manifest_in(d)? {
                    let lm = manifest::load(&p)?;
                    if lm.manifest.workspace.is_some() && member_dirs(&lm)?.iter().any(|m| m == nearest.dir()) {
                        root = lm;
                        break;
                    }
                }
                dir = d.parent();
            }
        }
        let mut project = Project::load(root)?;
        project.current = project.packages.iter().position(|p| p.dir == nearest.dir() && p.member);
        Ok(Some(project))
    }

    /// Loads the graph rooted at manifest `root`.
    pub fn load(root: LoadedManifest) -> Result<Project, String> {
        let root_dir = root.dir().to_path_buf();
        let mut b = Builder { root: root_dir.clone(), packages: Vec::new(), by_dir: HashMap::new(), stack: Vec::new(), pending: Vec::new() };
        let mut members = Vec::new();
        let mut root_package = None;
        if root.manifest.package.is_some() {
            let id = b.visit(&root_dir, true)?;
            root_package = Some(id);
            members.push(id);
        }
        if root.manifest.workspace.is_some() {
            for d in member_dirs(&root)? {
                let id = b.visit(&d, true)?;
                if !members.contains(&id) {
                    members.push(id);
                }
            }
        }
        // Dev-dependencies of members (outside the cycle check: a test kit
        // may depend on the package it tests).
        for &m in &members {
            let deps = b.packages[m].manifest.manifest.dev_dependencies.clone();
            let mut ids = Vec::new();
            for (i, (name, dep)) in deps.iter().enumerate() {
                ids.push(b.dep(m, name, dep, "dev-dependencies", (true, i))?);
            }
            b.packages[m].dev_deps = ids;
        }
        if !b.pending.is_empty() {
            b.attach_modules()?;
        }
        let packages = b.packages;
        // Namespace roots and names are unique in the graph.
        for (i, p) in packages.iter().enumerate() {
            for q in &packages[..i] {
                if q.namespace == p.namespace {
                    return Err(format!(
                        "packages `{}` ({}) and `{}` ({}) both own the namespace root `{}`; every package in the graph needs its own root (set `namespace` in [package])",
                        q.name,
                        q.manifest.path.display(),
                        p.name,
                        p.manifest.path.display(),
                        p.namespace
                    ));
                }
                if q.name == p.name {
                    return Err(format!("two different packages are named `{}`: {} and {}", p.name, q.manifest.path.display(), p.manifest.path.display()));
                }
            }
        }
        Ok(Project { root: root_dir, root_manifest: root, packages, members, root_package, current: None })
    }

    pub fn find_member(&self, name: &str) -> Option<PkgId> {
        self.members.iter().copied().find(|&m| self.packages[m].name == name)
    }

    /// `pkg` and everything it depends on (dev-dependencies of `pkg` too
    /// when `dev`), dependencies first; the order is deterministic.
    pub fn closure(&self, pkg: PkgId, dev: bool) -> Vec<PkgId> {
        fn go(p: &Project, id: PkgId, seen: &mut Vec<bool>, out: &mut Vec<PkgId>) {
            if std::mem::replace(&mut seen[id], true) {
                return;
            }
            for &d in &p.packages[id].deps {
                go(p, d, seen, out);
            }
            out.push(id);
        }
        let mut seen = vec![false; self.packages.len()];
        let mut out = Vec::new();
        if dev {
            seen[pkg] = true;
            for &d in &self.packages[pkg].dev_deps {
                go(self, d, &mut seen, &mut out);
            }
            seen[pkg] = false;
        }
        go(self, pkg, &mut seen, &mut out);
        out
    }

    /// The isolation policy of a build of `closure` (see [`Project::closure`])
    /// whose files belong to `file_pkg` (positions in `closure`). With
    /// `dev`, the last package (the member) also sees its dev-dependencies.
    pub fn policy(&self, closure: &[PkgId], file_pkg: Vec<usize>, dev: bool) -> ImportPolicy {
        let local = |id: PkgId| closure.iter().position(|&c| c == id);
        let packages = closure
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                let p = &self.packages[id];
                let mut deps: Vec<usize> = p.deps.iter().filter_map(|&d| local(d)).collect();
                if dev && i + 1 == closure.len() {
                    deps.extend(p.dev_deps.iter().filter_map(|&d| local(d)));
                }
                let capabilities = p.manifest.manifest.capabilities.iter().filter(|(_, on)| **on).map(|(c, _)| c.clone()).collect();
                PackageScope { name: p.name.clone(), namespace: p.namespace.clone(), deps, capabilities }
            })
            .collect();
        ImportPolicy { file_package: file_pkg, packages }
    }

    /// Features enabled for each package of `closure` (same order), for
    /// `@cfg(feature: ...)`: every package's `default` feature, the
    /// features its dependents request (`features = [...]` on the
    /// dependency) and, transitively, what enabled features enable (plain
    /// names in the package itself, `dep/feature` in a dependency).
    pub fn enabled_features(&self, closure: &[PkgId]) -> Vec<std::collections::HashSet<String>> {
        // Position in `closure` of dependency `key` of package `id`.
        let local = |id: PkgId, key: &str| {
            let p = &self.packages[id];
            let dep = p.dep_keys.iter().position(|k| k == key).and_then(|i| p.deps.get(i).copied());
            dep.and_then(|d| closure.iter().position(|&c| c == d))
        };
        let mut on: Vec<std::collections::HashSet<String>> = vec![Default::default(); closure.len()];
        let mut work: Vec<(usize, String)> = Vec::new();
        for (i, &id) in closure.iter().enumerate() {
            let m = &self.packages[id].manifest.manifest;
            if m.features.contains_key("default") {
                work.push((i, "default".into()));
            }
            for (name, dep) in &m.dependencies {
                if let Some(d) = local(id, name) {
                    work.extend(dep.features().iter().map(|f| (d, f.clone())));
                }
            }
        }
        while let Some((i, f)) = work.pop() {
            if !on[i].insert(f.clone()) {
                continue;
            }
            let m = &self.packages[closure[i]].manifest.manifest;
            for item in m.features.get(&f).into_iter().flatten() {
                match manifest::split_feature_ref(item, &m.dependencies) {
                    Some((dep, df)) => {
                        if let Some(d) = local(closure[i], dep) {
                            work.push((d, df.to_string()));
                        }
                    }
                    None => work.push((i, item.clone())),
                }
            }
        }
        on
    }

    /// Directory of build outputs and the object cache.
    pub fn target_dir(&self) -> PathBuf {
        self.root.join("target")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("zell.lock")
    }

    /// `.zell/deps/` of the project (materialised module dependencies).
    pub fn deps_dir(&self) -> PathBuf {
        self.root.join(".zell").join("deps")
    }

    /// The package of module `path`, if it is in the graph.
    pub fn find_module(&self, path: &str) -> Option<PkgId> {
        self.packages.iter().position(|p| p.source.module() == Some(path))
    }
}

/// Member directories of a workspace manifest (globs expanded, sorted;
/// glob matches without a manifest are skipped, literal members must have one).
pub fn member_dirs(lm: &LoadedManifest) -> Result<Vec<PathBuf>, String> {
    let Some(ws) = &lm.manifest.workspace else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for m in &ws.members {
        let is_glob = m.contains(['*', '?', '[']);
        let pattern = lm.dir().join(m);
        let mut found: Vec<PathBuf> = if is_glob {
            let pat = pattern.to_string_lossy().to_string();
            glob::glob(&pat).map_err(|e| format!("{}: bad workspace member pattern `{m}`: {e}", lm.path.display()))?.filter_map(Result::ok).filter(|p| p.is_dir()).collect()
        } else {
            if !pattern.is_dir() {
                return Err(format!("{}: workspace member `{m}` is not a directory", lm.path.display()));
            }
            vec![pattern]
        };
        found.sort();
        for d in found {
            if manifest::manifest_in(&d)?.is_none() {
                if is_glob {
                    continue;
                }
                return Err(format!("{}: workspace member `{m}` has no manifest", lm.path.display()));
            }
            let d = crate::paths::canonical(&d).map_err(|e| format!("cannot read {}: {e}", d.display()))?;
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    Ok(out)
}

/// `to` relative to `from` (both canonical), `/`-separated, with `..`
/// where needed; `.` when equal. Keeps the lock file machine-independent.
fn relative_path(from: &Path, to: &Path) -> String {
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = vec!["..".to_string(); a.len() - common];
    parts.extend(b[common..].iter().map(|c| c.as_os_str().to_string_lossy().to_string()));
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

struct Builder {
    root: PathBuf,
    packages: Vec<PackageNode>,
    by_dir: HashMap<PathBuf, PkgId>,
    /// Packages whose dependencies are being resolved (cycle detection).
    stack: Vec<PkgId>,
    /// Module dependencies of workspace packages, resolved at the end.
    pending: Vec<Pending>,
}

/// A module dependency waiting for version selection.
struct Pending {
    from: PkgId,
    dev: bool,
    pos: usize,
    req: super::resolve::Requirement,
    features: Vec<String>,
}

impl Builder {
    /// Adds the package in `dir` and, recursively, its dependencies.
    fn visit(&mut self, dir: &Path, member: bool) -> Result<PkgId, String> {
        let dir = crate::paths::canonical(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        if let Some(&id) = self.by_dir.get(&dir) {
            if let Some(pos) = self.stack.iter().position(|&s| s == id) {
                let mut chain: Vec<&str> = self.stack[pos..].iter().map(|&s| self.packages[s].name.as_str()).collect();
                chain.push(&self.packages[id].name);
                return Err(format!("dependency cycle: {}", chain.join(" -> ")));
            }
            self.packages[id].member |= member;
            return Ok(id);
        }
        let lm = manifest::load_dir(&dir)?;
        let Some(pkg) = lm.manifest.package.clone() else {
            return Err(format!("{} has no [package] section (a virtual workspace cannot be a dependency)", lm.path.display()));
        };
        let id = self.packages.len();
        let rel = relative_path(&self.root, &dir);
        self.packages.push(PackageNode {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            namespace: pkg.namespace_root(),
            dir: dir.clone(),
            manifest: lm.clone(),
            source: Source::Path(rel),
            deps: Vec::new(),
            dep_keys: lm.manifest.dependencies.keys().cloned().collect(),
            dev_deps: Vec::new(),
            member,
        });
        self.by_dir.insert(dir, id);
        self.stack.push(id);
        let mut deps = Vec::new();
        for (i, (name, dep)) in lm.manifest.dependencies.iter().enumerate() {
            deps.push(self.dep(id, name, dep, "dependencies", (false, i))?);
        }
        self.stack.pop();
        self.packages[id].deps = deps;
        Ok(id)
    }

    /// Resolves dependency `name` of package `from`. Module dependencies
    /// get a placeholder id, filled in by [`Builder::attach_modules`];
    /// `slot` is (dev, position) of the dependency in its section.
    fn dep(&mut self, from: PkgId, name: &str, dep: &Dependency, section: &str, slot: (bool, usize)) -> Result<PkgId, String> {
        let owner = self.packages[from].name.clone();
        let mpath = self.packages[from].manifest.path.clone();
        let Some(path) = dep.path() else {
            let req = super::resolve::requirement(&owner, name, dep).map_err(|e| format!("{}: [{section}] {e}", mpath.display()))?.expect("not a path dependency");
            self.pending.push(Pending { from, dev: slot.0, pos: slot.1, req, features: dep.features().to_vec() });
            return Ok(usize::MAX);
        };
        let dir = self.packages[from].dir.join(path);
        if !dir.is_dir() {
            return Err(format!("{}: [{section}] {name}: path `{path}` is not a directory", mpath.display()));
        }
        let id = self.visit(&dir, false)?;
        let real = &self.packages[id].name;
        if real != name {
            return Err(format!("{}: [{section}] {name}: the package at `{path}` is named `{real}`; use `{real} = {{ path = \"{path}\" }}`", mpath.display()));
        }
        if id == from {
            return Err(format!("package `{owner}` depends on itself"));
        }
        let have = &self.packages[id].manifest.manifest.features;
        for f in dep.features() {
            if !have.contains_key(f.as_str()) {
                return Err(format!("{}: [{section}] {name}: package `{name}` has no feature `{f}`", mpath.display()));
            }
        }
        // `dep/feature` entries of our own [features] table.
        let m = &self.packages[from].manifest.manifest;
        for (feat, items) in &m.features {
            for it in items {
                if let Some((d, f)) = manifest::split_feature_ref(it, &m.dependencies) {
                    if d == name && !have.contains_key(f) {
                        return Err(format!("{}: feature `{feat}` enables `{it}`, but package `{name}` has no feature `{f}`", mpath.display()));
                    }
                }
            }
        }
        Ok(id)
    }
}

/// Directory of a materialised module under `.zell/deps/`.
pub fn module_dir(root: &Path, module: &str, version: &str) -> PathBuf {
    root.join(".zell").join("deps").join(format!("{}@{version}", super::store::escape_path(module)))
}

impl Builder {
    /// Selects versions for the pending module dependencies (MVS, or the
    /// `vendor/` copy), materialises them in `.zell/deps/` and adds them
    /// to the graph.
    fn attach_modules(&mut self) -> Result<(), String> {
        use super::resolve;
        let opts = resolve::options();
        let lock = std::fs::read_to_string(self.root.join("zell.lock")).ok().and_then(|t| super::lock::LockFile::parse(&t).ok());
        let roots: Vec<resolve::Requirement> = self.pending.iter().map(|p| p.req.clone()).collect();
        let vendor = self.root.join("vendor");
        let selected = if vendor.join(super::vendor::MODULES_FILE).is_file() && opts.update == resolve::Update::None {
            let lock = lock.as_ref().ok_or("vendor/ exists but zell.lock does not; run `zell vendor` again")?;
            resolve::from_vendor(&vendor, lock, &roots)?
        } else {
            let fetcher = super::fetch::Fetcher::new(opts.offline);
            let mut sel = resolve::resolve(roots, lock.as_ref(), &fetcher, &opts.update)?;
            for s in &mut sel {
                let dir = module_dir(&self.root, &s.module, &s.version);
                fetcher.store.materialize(s.index.as_ref().unwrap(), &dir)?;
                s.dir = Some(dir);
            }
            fetcher.store.register_project(&self.root);
            sel
        };
        let mut by_module: HashMap<String, PkgId> = HashMap::new();
        for s in &selected {
            let dir = s.dir.clone().unwrap();
            let dir = crate::paths::canonical(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
            let lm = manifest::load_dir(&dir).map_err(|e| format!("{}@{}: {e}", s.module, s.version))?;
            let pkg = lm.manifest.package.clone().ok_or_else(|| format!("{}@{}: the manifest has no [package] section", s.module, s.version))?;
            let id = self.packages.len();
            self.packages.push(PackageNode {
                name: pkg.name.clone(),
                version: s.version.trim_start_matches('v').to_string(),
                namespace: pkg.namespace_root(),
                dir: dir.clone(),
                manifest: lm.clone(),
                source: Source::Module { path: s.module.clone(), version: s.version.clone(), h1: s.h1.clone(), lock_source: s.source.clone() },
                deps: Vec::new(),
                dep_keys: s.deps.iter().map(|r| r.key.clone()).collect(),
                dev_deps: Vec::new(),
                member: false,
            });
            self.by_dir.insert(dir, id);
            by_module.insert(s.module.clone(), id);
        }
        for s in &selected {
            let id = by_module[&s.module];
            let deps: Vec<PkgId> = s.deps.iter().map(|r| by_module[&r.module]).collect();
            // Features requested by the module of its dependencies.
            let m = self.packages[id].manifest.manifest.clone();
            for (r, &d) in s.deps.iter().zip(&deps) {
                let have = &self.packages[d].manifest.manifest.features;
                if let Some(dep) = m.dependencies.get(&r.key) {
                    for f in dep.features() {
                        if !have.contains_key(f.as_str()) {
                            return Err(format!("{}@{}: dependency `{}` has no feature `{f}`", s.module, s.version, r.key));
                        }
                    }
                }
            }
            self.packages[id].deps = deps;
        }
        for p in std::mem::take(&mut self.pending) {
            let id = by_module[&p.req.module];
            let have = &self.packages[id].manifest.manifest.features;
            let mpath = self.packages[p.from].manifest.path.clone();
            for f in &p.features {
                if !have.contains_key(f.as_str()) {
                    return Err(format!("{}: dependency `{}` ({}) has no feature `{f}`", mpath.display(), p.req.key, p.req.module));
                }
            }
            let list = if p.dev { &mut self.packages[p.from].dev_deps } else { &mut self.packages[p.from].deps };
            if p.pos < list.len() {
                list[p.pos] = id;
            }
        }
        Ok(())
    }
}
