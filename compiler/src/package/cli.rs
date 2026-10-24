//! The package-manager commands of the CLI: `install`, `update`, `tree`,
//! `why`, `vendor`, `store`, `env` and `proxy serve` (docs/PACKAGES.md).

use super::graph::{PkgId, Project, Source};
use super::lock::{self, LockFile};
use super::resolve::{self, Options, Update};
use super::store::{self, Store};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Common flags: `--offline`, `--locked`, an optional project path; the
/// other arguments are returned.
struct Flags {
    offline: bool,
    locked: bool,
    path: Option<PathBuf>,
    rest: Vec<String>,
}

fn flags(args: &[String], takes_names: bool) -> Result<Flags, String> {
    let mut f = Flags { offline: false, locked: false, path: None, rest: Vec::new() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--offline" => f.offline = true,
            "--locked" => f.locked = true,
            "--manifest-path" | "-C" => f.path = Some(PathBuf::from(it.next().ok_or("-C needs a directory")?)),
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
            s if takes_names => f.rest.push(s.to_string()),
            s if f.path.is_none() => f.path = Some(PathBuf::from(s)),
            s => return Err(format!("unexpected argument `{s}`")),
        }
    }
    Ok(f)
}

fn project(path: Option<&Path>) -> Result<Project, String> {
    super::discover(path)?.ok_or_else(|| format!("could not find {} in the current directory or any parent", super::manifest::MANIFEST_NAMES.join(", ")))
}

fn old_lock(root: &Path) -> Option<LockFile> {
    std::fs::read_to_string(root.join("zell.lock")).ok().and_then(|t| LockFile::parse(&t).ok())
}

/// Removes module directories of `.zell/deps/` that are not in the graph.
fn clean_deps(project: &Project) -> usize {
    let keep: HashSet<PathBuf> = project.packages.iter().filter(|p| p.source.module().is_some()).map(|p| p.dir.clone()).collect();
    let mut removed = 0;
    fn walk(dir: &Path, keep: &HashSet<PathBuf>, removed: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            if e.file_name().to_string_lossy().contains('@') {
                let canon = crate::paths::canonical(&p).unwrap_or(p.clone());
                if !keep.contains(&canon) && std::fs::remove_dir_all(&p).is_ok() {
                    *removed += 1;
                }
            } else {
                walk(&p, keep, removed);
                let _ = std::fs::remove_dir(&p); // only if empty
            }
        }
    }
    walk(&project.deps_dir(), &keep, &mut removed);
    removed
}

fn modules(p: &Project) -> Vec<PkgId> {
    let mut m: Vec<PkgId> = (0..p.packages.len()).filter(|&i| p.packages[i].source.module().is_some()).collect();
    m.sort_by(|a, b| p.packages[*a].source.module().cmp(&p.packages[*b].source.module()));
    m
}

/// `zell install [--offline] [--locked] [path]`: resolves the graph
/// (from `zell.lock` when it still fits), fills the store and
/// `.zell/deps/`, writes the lock.
pub fn install(args: &[String]) -> Result<(), String> {
    let f = flags(args, false)?;
    resolve::set_options(Options { offline: f.offline, update: Update::None });
    let p = project(f.path.as_deref())?;
    if lock::sync(&p, f.locked)? == lock::LockStatus::Written {
        eprintln!("wrote {}", p.lock_path().display());
    }
    let removed = clean_deps(&p);
    let mods = modules(&p);
    for &m in &mods {
        let pk = &p.packages[m];
        eprintln!("  {} {} -> {}", pk.source.module().unwrap(), version_of(&pk.source), super::display_path(&pk.dir));
    }
    eprintln!("installed {} module(s){}", mods.len(), if removed > 0 { format!(", removed {removed} stale") } else { String::new() });
    Ok(())
}

fn version_of(s: &Source) -> String {
    match s {
        Source::Module { version, .. } => version.clone(),
        Source::Path(_) => String::new(),
    }
}

/// `zell update [module|name ...] [--offline]`: newest versions within
/// the constraints, for the named modules or all.
pub fn update(args: &[String]) -> Result<(), String> {
    let f = flags(args, true)?;
    let update = if f.rest.is_empty() { Update::All } else { Update::Only(f.rest.clone()) };
    let start = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = super::manifest::find_upwards(&start)?.ok_or("no manifest in the current directory or any parent")?;
    let before = old_lock(root.parent().unwrap());
    resolve::set_options(Options { offline: f.offline, update });
    let p = project(None)?;
    lock::sync(&p, false)?;
    clean_deps(&p);
    let after = LockFile::of(&p)?;
    let mut changed = 0;
    for n in &after.packages {
        let Some(m) = &n.module else { continue };
        let old = before.as_ref().and_then(|b| b.packages.iter().find(|o| o.module.as_ref() == Some(m)));
        match old {
            Some(o) if o.version == n.version => {}
            Some(o) => {
                eprintln!("  updated {m} {} -> {}", o.version, n.version);
                changed += 1;
            }
            None => {
                eprintln!("  added {m} {}", n.version);
                changed += 1;
            }
        }
    }
    if let Some(b) = &before {
        for o in &b.packages {
            if let Some(m) = &o.module {
                if !after.packages.iter().any(|n| n.module.as_ref() == Some(m)) {
                    eprintln!("  removed {m} {}", o.version);
                    changed += 1;
                }
            }
        }
    }
    eprintln!("{}", if changed == 0 { "everything is up to date".to_string() } else { format!("{changed} change(s) written to zell.lock") });
    Ok(())
}

fn label(p: &Project, id: PkgId) -> String {
    let n = &p.packages[id];
    match &n.source {
        Source::Module { path, version, .. } => format!("{} {version} ({path})", n.name),
        Source::Path(d) => format!("{} v{} (path:{d})", n.name, n.version),
    }
}

/// `zell tree [path]`: the dependency graph of every member.
pub fn tree(args: &[String]) -> Result<String, String> {
    let f = flags(args, false)?;
    resolve::set_options(Options { offline: f.offline, update: Update::None });
    let p = project(f.path.as_deref())?;
    let mut out = String::new();
    fn go(p: &Project, id: PkgId, prefix: &str, seen: &mut HashSet<PkgId>, out: &mut String) {
        let deps = &p.packages[id].deps;
        for (i, &d) in deps.iter().enumerate() {
            let last = i + 1 == deps.len();
            let again = !seen.insert(d) && !p.packages[d].deps.is_empty();
            out.push_str(&format!("{prefix}{}{}{}\n", if last { "└── " } else { "├── " }, label(p, d), if again { " (*)" } else { "" }));
            if !again {
                go(p, d, &format!("{prefix}{}", if last { "    " } else { "│   " }), seen, out);
            }
        }
    }
    for &m in &p.members {
        out.push_str(&label(&p, m));
        out.push('\n');
        let mut seen = HashSet::new();
        go(&p, m, "", &mut seen, &mut out);
        let dev = &p.packages[m].dev_deps;
        if !dev.is_empty() {
            out.push_str("[dev-dependencies]\n");
            for &d in dev {
                out.push_str(&format!("└── {}\n", label(&p, d)));
            }
        }
    }
    Ok(out)
}

/// `zell why <module|name> [path]`: every chain from a workspace member
/// to the package, with the requirement at each step.
pub fn why(args: &[String]) -> Result<String, String> {
    let f = flags(args, true)?;
    let [target] = f.rest.as_slice() else { return Err("usage: zell why <module path | package name>".into()) };
    resolve::set_options(Options { offline: f.offline, update: Update::None });
    let p = project(f.path.as_deref())?;
    let id = p
        .packages
        .iter()
        .position(|n| n.source.module() == Some(target.as_str()) || &n.name == target)
        .ok_or_else(|| format!("`{target}` is not in the dependency graph (see `zell tree`)"))?;
    let mut chains = Vec::new();
    fn go(p: &Project, cur: PkgId, target: PkgId, path: &mut Vec<String>, chains: &mut Vec<String>) {
        if cur == target {
            chains.push(path.join(" -> "));
            return;
        }
        let n = &p.packages[cur];
        for (k, &d) in n.dep_keys.iter().zip(&n.deps) {
            if path.len() > 32 {
                return;
            }
            let req = match n.manifest.manifest.dependencies.get(k) {
                Some(super::manifest::Dependency::Version(v)) => format!(" ({k} {v})"),
                Some(super::manifest::Dependency::Detailed(dd)) => {
                    let mut parts = Vec::new();
                    for (name, v) in [("version", &dd.version), ("git", &dd.git), ("tag", &dd.tag), ("branch", &dd.branch), ("rev", &dd.rev), ("path", &dd.path)] {
                        if let Some(v) = v {
                            parts.push(format!("{name} = {v}"));
                        }
                    }
                    format!(" ({k}: {})", parts.join(", "))
                }
                None => String::new(),
            };
            path.push(format!("{}{req}", label(p, d)));
            go(p, d, target, path, chains);
            path.pop();
        }
    }
    for &m in &p.members {
        let mut path = vec![label(&p, m)];
        go(&p, m, id, &mut path, &mut chains);
    }
    chains.sort();
    chains.dedup();
    if chains.is_empty() {
        return Ok(format!("{} is a workspace member; nothing requires it\n", label(&p, id)));
    }
    Ok(chains.into_iter().map(|c| c + "\n").collect())
}

/// `zell vendor [path]`.
pub fn vendor(args: &[String]) -> Result<(), String> {
    let f = flags(args, false)?;
    let start = match &f.path {
        Some(p) => p.clone(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    if let Some(m) = super::manifest::find_upwards(&crate::paths::canonical(&start).map_err(|e| e.to_string())?)? {
        let _ = std::fs::remove_dir_all(m.parent().unwrap().join("vendor"));
    }
    resolve::set_options(Options { offline: f.offline, update: Update::None });
    let p = project(f.path.as_deref())?;
    lock::sync(&p, f.locked)?;
    let list = super::vendor::vendor(&p)?;
    eprintln!("vendored {} module(s) into {}", list.len(), super::display_path(&p.root.join("vendor")));
    Ok(())
}

/// `zell store verify|prune|path`.
pub fn store_cmd(args: &[String]) -> Result<String, String> {
    let s = Store::open();
    match args.first().map(String::as_str) {
        Some("path") => Ok(format!("{}\n", s.root.display())),
        Some("verify") => {
            let problems = s.verify();
            if problems.is_empty() {
                Ok(format!("store {} is sound ({} module version(s))\n", s.root.display(), s.packages().len()))
            } else {
                Err(format!("store {} has {} problem(s):\n  {}", s.root.display(), problems.len(), problems.join("\n  ")))
            }
        }
        Some("prune") => {
            let (versions, files) = s.prune();
            let mut out: String = versions.iter().map(|v| format!("removed {v}\n")).collect();
            out.push_str(&format!("pruned {} module version(s), {files} file(s)\n", versions.len()));
            Ok(out)
        }
        _ => Err("usage: zell store verify | prune | path".into()),
    }
}

/// `zell env [NAME...]`: the toolchain and package-manager settings.
pub fn env(args: &[String]) -> Result<String, String> {
    let std_dir = crate::sema::stdfs::std_dir().map(|d| d.display().to_string()).unwrap_or_else(|| "(embedded)".into());
    let get = |n: &str| std::env::var(n).unwrap_or_default();
    let vars: Vec<(&str, String)> = vec![
        ("ZELL_STD", std_dir),
        ("ZELL_HOME", store::zell_home().display().to_string()),
        ("ZELL_STORE", store::store_root().display().to_string()),
        ("ZELL_PROXY", super::fetch::Config::proxy_spec()),
        ("ZELL_NOPROXY", get("ZELL_NOPROXY")),
        ("ZELL_PRIVATE", get("ZELL_PRIVATE")),
        ("ZELL_SUMDB", std::env::var("ZELL_SUMDB").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "off".into())),
        ("ZELL_NOSUMDB", get("ZELL_NOSUMDB")),
        ("ZELL_LINKER", crate::toolchain::linker_description()),
        ("ZELL_LTO", std::env::var("ZELL_LTO").ok().filter(|s| s == "1").map(|_| "1 (ThinLTO with the runtime in --release builds)".to_string()).unwrap_or_else(|| "off".into())),
        ("ZELL_TARGETS", crate::toolchain::targets_description()),
    ];
    if args.is_empty() {
        return Ok(vars.iter().map(|(k, v)| format!("{k}=\"{v}\"\n")).collect());
    }
    let mut out = String::new();
    for a in args {
        let v = vars.iter().find(|(k, _)| k == a).ok_or_else(|| format!("unknown variable `{a}` (known: {})", vars.iter().map(|v| v.0).collect::<Vec<_>>().join(", ")))?;
        out.push_str(&v.1);
        out.push('\n');
    }
    Ok(out)
}

/// `zell proxy serve ...`.
pub fn proxy(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("serve") => {
            let (cfg, addr) = super::proxy::parse_args(&args[1..])?;
            super::proxy::serve(cfg, &addr)
        }
        _ => Err("usage: zell proxy serve --dir <cache> [--addr host:port] [--upstream direct|off|<url>] [--sumdb]".into()),
    }
}
