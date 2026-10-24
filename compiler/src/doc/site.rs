//! `zell doc --site <dir>`: the project's documentation site (GitHub
//! Pages): the landing page from `README.md`, the guides of `docs/guide/`
//! with a sidebar and prev/next links, the design documents (`DESIGN.md`,
//! `docs/*.md` and the Markdown files they link to) under `design/`, and
//! the standard library reference under `std/`, with one client-side
//! search index over all of it. Every link is relative, so the site works
//! under any base path (`https://zell-lang.github.io/zell/`); `--base` is
//! only needed for `404.html`, which GitHub Pages serves at any depth.

use super::html::{self, Chrome, Layout};
use super::markdown;
use super::nav::{self, relative, NavTree};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// Options of `zell doc --site`.
pub struct SiteOptions {
    /// The repository (README.md, docs/, std/).
    pub root: PathBuf,
    pub out: PathBuf,
    /// The URL path the site is served under (`/zell/`), for 404.html.
    pub base: String,
    /// Where links to non-Markdown repository files point
    /// (`https://github.com/org/zell/blob/master/`).
    pub repo_url: String,
}

/// The guides in reading order; other files of docs/guide follow sorted.
const GUIDE_ORDER: &[&str] = &["tour", "errors", "async", "memory", "packages", "testing", "debugging", "web", "faq"];

const SITE_NAV: &[(&str, &str)] = &[("Guide", "guide/tour.html"), ("Design", "design/index.html"), ("Std", "std/index.html")];

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Home,
    Guide,
    Design,
}

struct Page {
    /// Repository-relative source (`docs/guide/tour.md`).
    src: String,
    /// Site-relative output (`guide/tour.html`).
    out: String,
    section: Section,
    title: String,
}

/// `a/b/../c` -> `a/c` (no file system access).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The Markdown link targets of a document (`[x](target)`), without code.
fn md_links(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for l in md.lines() {
        if l.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let mut rest = l;
        while let Some(i) = rest.find("](") {
            let after = &rest[i + 2..];
            if let Some(e) = after.find(')') {
                out.push(after[..e].trim().to_string());
                rest = &after[e..];
            } else {
                break;
            }
        }
    }
    out
}

fn title_of(md: &str, fallback: &str) -> String {
    md.lines().find_map(|l| l.strip_prefix("# ")).map(|t| t.trim().replace('`', "")).unwrap_or_else(|| fallback.to_string())
}

fn guide_rank(stem: &str) -> usize {
    GUIDE_ORDER.iter().position(|g| *g == stem).unwrap_or(GUIDE_ORDER.len())
}

/// Finds the pages: README, guides, design docs and the Markdown files
/// they link to (transitively).
fn collect_pages(root: &Path) -> Result<Vec<Page>, String> {
    let mut pages: Vec<Page> = Vec::new();
    let read = |rel: &str| std::fs::read_to_string(root.join(rel)).map_err(|e| format!("{rel}: {e}"));
    let readme = read("README.md")?;
    pages.push(Page { src: "README.md".into(), out: "index.html".into(), section: Section::Home, title: "Zell".into() });
    let mut guides: Vec<String> = std::fs::read_dir(root.join("docs/guide"))
        .map_err(|e| format!("docs/guide: {e}"))?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".md"))
        .collect();
    guides.sort_by_key(|g| (guide_rank(g.trim_end_matches(".md")), g.clone()));
    for g in guides {
        let src = format!("docs/guide/{g}");
        let title = title_of(&read(&src)?, &g);
        pages.push(Page { out: format!("guide/{}", g.replace(".md", ".html")), src, section: Section::Guide, title });
    }
    let design = read("DESIGN.md")?;
    pages.push(Page { src: "DESIGN.md".into(), out: "design/DESIGN.html".into(), section: Section::Design, title: title_of(&design, "DESIGN") });
    let mut docs: Vec<String> = std::fs::read_dir(root.join("docs")).map_err(|e| format!("docs: {e}"))?.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".md")).collect();
    docs.sort();
    for d in docs {
        let src = format!("docs/{d}");
        let title = title_of(&read(&src)?, &d);
        pages.push(Page { out: format!("design/{}", d.replace(".md", ".html")), src, section: Section::Design, title });
    }
    let _ = readme;
    // the Markdown files linked from the pages, transitively
    let mut i = 0;
    while i < pages.len() {
        let src = pages[i].src.clone();
        let text = read(&src)?;
        let dir = Path::new(&src).parent().unwrap_or(Path::new("")).to_path_buf();
        for l in md_links(&text) {
            if l.contains("://") || l.starts_with('#') || l.starts_with("mailto:") {
                continue;
            }
            let path = l.split('#').next().unwrap_or("");
            if !path.ends_with(".md") {
                continue;
            }
            let target = normalize(&dir.join(path)).to_string_lossy().replace('\\', "/");
            if pages.iter().any(|p| p.src == target) || !root.join(&target).is_file() {
                continue;
            }
            let title = title_of(&read(&target)?, &target);
            let out = format!("design/{}", target.trim_end_matches(".md").replace('/', "-") + ".html");
            pages.push(Page { src: target, out, section: Section::Design, title });
        }
        i += 1;
    }
    Ok(pages)
}

/// The repository URL for links to non-Markdown files.
pub fn default_repo_url(root: &Path) -> String {
    let url = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let url = url.strip_suffix(".git").unwrap_or(&url).to_string();
    let url = match url.strip_prefix("git@") {
        Some(rest) => format!("https://{}", rest.replacen(':', "/", 1)),
        None => url,
    };
    if url.starts_with("https://") {
        format!("{url}/blob/master/")
    } else {
        "https://github.com/zell-lang/zell/blob/master/".to_string()
    }
}

/// The pages every sidebar links to: guides in reading order, `DESIGN.md`
/// then the other design documents by title, the documented std namespaces.
fn nav_tree(pages: &[Page], docs: &super::Docs) -> NavTree {
    let guide = pages.iter().filter(|p| p.section == Section::Guide).map(|p| (p.out.clone(), p.title.clone())).collect();
    let mut design: Vec<(String, String)> = pages.iter().filter(|p| p.section == Section::Design && p.src != "DESIGN.md").map(|p| (p.out.clone(), p.title.clone())).collect();
    design.sort_by_key(|(_, t)| t.to_lowercase());
    if let Some(d) = pages.iter().find(|p| p.src == "DESIGN.md") {
        design.insert(0, (d.out.clone(), d.title.clone()));
    }
    let mut std: Vec<String> = docs.namespaces.iter().filter(|n| n.std && !(n.items.is_empty() && n.examples.is_empty() && n.doc.is_empty())).map(|n| n.name.clone()).collect();
    std.sort();
    NavTree { guide, design, std }
}

/// Builds the site; returns the number of HTML pages written.
pub fn build_site(o: &SiteOptions) -> Result<usize, String> {
    let pages = collect_pages(&o.root)?;
    let srcs = super::load_std();
    if srcs.is_empty() {
        return Err("the standard library sources were not found (set ZELL_STD)".into());
    }
    let docs = super::Docs::build(&srcs, super::Options::default());
    let tree = nav_tree(&pages, &docs);
    let by_src: BTreeMap<String, String> = pages.iter().map(|p| (p.src.clone(), p.out.clone())).collect();
    let mut search: Vec<serde_json::Value> = Vec::new();
    let mut count = 0;
    for (i, p) in pages.iter().enumerate() {
        let md = std::fs::read_to_string(o.root.join(&p.src)).map_err(|e| format!("{}: {e}", p.src))?;
        let dir = Path::new(&p.src).parent().unwrap_or(Path::new("")).to_path_buf();
        let href = |u: &str| -> String {
            if u.contains("://") || u.starts_with('#') || u.starts_with("mailto:") {
                return u.to_string();
            }
            let (path, frag) = match u.split_once('#') {
                Some((a, b)) => (a, format!("#{b}")),
                None => (u, String::new()),
            };
            let target = normalize(&dir.join(path)).to_string_lossy().replace('\\', "/");
            if let Some(out) = by_src.get(&target) {
                return relative(&p.out, out) + &frag;
            }
            let t = target.trim_end_matches('/');
            if t == "std" || t.starts_with("std/") {
                return relative(&p.out, "std/index.html");
            }
            let kind = if o.root.join(&target).is_dir() { "tree" } else { "blob" };
            format!("{}{}{frag}", o.repo_url.replace("/blob/", &format!("/{kind}/")), t)
        };
        let (mut body, heads) = markdown::page_html(&md, &href);
        if p.section == Section::Guide {
            let guides: Vec<&Page> = pages.iter().filter(|g| g.section == Section::Guide).collect();
            let at = guides.iter().position(|g| g.out == p.out).unwrap_or(0);
            let prev = at.checked_sub(1).and_then(|k| guides.get(k)).map(|g| format!("<a href=\"{}\">&larr; {}</a>", relative(&p.out, &g.out), markdown::escape(&g.title))).unwrap_or_default();
            let next = guides.get(at + 1).map(|g| format!("<a href=\"{}\">{} &rarr;</a>", relative(&p.out, &g.out), markdown::escape(&g.title))).unwrap_or_default();
            body.push_str(&format!("<div class=\"pager\"><span>{prev}</span><span>{next}</span></div>\n"));
        }
        let depth = p.out.matches('/').count();
        let ch = Chrome { root: "../".repeat(depth), nav: SITE_NAV, side: nav::sidebar(&tree, &p.out, &heads) };
        html::write(&o.out.join(&p.out), &html::page(&p.title, &body, &ch))?;
        count += 1;
        let kind = match p.section {
            Section::Home => "page",
            Section::Guide => "guide",
            Section::Design => "design",
        };
        search.push(serde_json::json!({"n": p.title, "k": kind, "s": "", "u": p.out}));
        for (lvl, t, id) in &heads {
            if (2..=3).contains(lvl) {
                search.push(serde_json::json!({"n": format!("{} › {t}", p.title), "k": kind, "s": "", "u": format!("{}#{id}", p.out)}));
            }
        }
        let _ = i;
    }
    // the design index
    let mut d = String::from("<h1>Design documents</h1>\n<p>The language specification and the design of the tools, packages, framework, standard library and platforms (mostly in Russian).</p>\n<ul>\n");
    for p in pages.iter().filter(|p| p.section == Section::Design) {
        d.push_str(&format!("<li><a href=\"{}\">{}</a> <span class=\"muted\">{}</span></li>\n", relative("design/index.html", &p.out), markdown::escape(&p.title), markdown::escape(&p.src)));
    }
    d.push_str("</ul>\n");
    let idx_page = Page { src: String::new(), out: "design/index.html".into(), section: Section::Design, title: "Design documents".into() };
    let ch = Chrome { root: "../".into(), nav: SITE_NAV, side: nav::sidebar(&tree, &idx_page.out, &[]) };
    html::write(&o.out.join("design/index.html"), &html::page("Design documents", &d, &ch))?;
    count += 1;
    // the standard library reference
    let layout = Layout { root: "../", prefix: "std/", nav: SITE_NAV, tree: Some(&tree) };
    count += html::write_api(&docs, &o.out.join("std"), None, &layout)?;
    search.extend(html::search_entries(&docs, "std/"));
    // shared files
    let idx = serde_json::Value::Array(search);
    html::write(&o.out.join("style.css"), html::CSS)?;
    html::write(&o.out.join("search.js"), html::SEARCH_JS)?;
    html::write(&o.out.join("nav.js"), nav::NAV_JS)?;
    html::write(&o.out.join("search-index.json"), &serde_json::to_string(&idx).unwrap())?;
    html::write(&o.out.join("search-index.js"), &format!("window.ZELL_INDEX = {};\n", serde_json::to_string(&idx).unwrap()))?;
    html::write(&o.out.join("std/docs.json"), &serde_json::to_string(&docs).unwrap())?;
    html::write(&o.out.join(".nojekyll"), "")?;
    // 404 is served at any path: absolute links under the base
    let base = format!("/{}/", o.base.trim_matches('/')).replace("//", "/");
    let ch = Chrome { root: base, nav: SITE_NAV, side: String::new() };
    html::write(&o.out.join("404.html"), &html::page("Not found", "<h1>Page not found</h1>\n<p>Try the search box above.</p>\n", &ch))?;
    Ok(count)
}

/// Every broken internal link of the site in `dir`: (page, href). An
/// `href` resolves when its file exists and, with a `#fragment` into an
/// HTML page, that page has the id. External and `javascript:` links are
/// skipped; `404.html` (absolute links) is not checked.
pub fn check_links(dir: &Path) -> Vec<(String, String)> {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "html") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    let mut ids: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let attr_values = |text: &str, attr: &str| -> Vec<String> {
        let pat = format!("{attr}=\"");
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find(&pat) {
            let after = &rest[i + pat.len()..];
            let Some(e) = after.find('"') else { break };
            out.push(after[..e].replace("&amp;", "&").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">"));
            rest = &after[e..];
        }
        out
    };
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        ids.insert(f.clone(), attr_values(&text, " id").into_iter().collect());
    }
    let mut bad = Vec::new();
    for f in &files {
        if f.file_name().is_some_and(|n| n == "404.html") {
            continue;
        }
        let text = std::fs::read_to_string(f).unwrap_or_default();
        let page = f.strip_prefix(dir).unwrap_or(f).display().to_string();
        let mut targets = attr_values(&text, "href");
        targets.extend(attr_values(&text, "src"));
        for h in targets {
            if h.contains("://") || h.starts_with("mailto:") || h.starts_with("javascript:") || h.is_empty() {
                continue;
            }
            let (path, frag) = match h.split_once('#') {
                Some((a, b)) => (a.to_string(), Some(b.to_string())),
                None => (h.clone(), None),
            };
            let target = if path.is_empty() { f.clone() } else { normalize(&f.parent().unwrap().join(&path)) };
            let ok = target.is_file()
                && match (&frag, ids.get(&target)) {
                    (Some(fr), Some(set)) if !fr.is_empty() && !fr.starts_with("q=") => set.contains(fr),
                    _ => true,
                };
            if !ok {
                bad.push((page.clone(), h));
            }
        }
    }
    bad
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_urls() {
        assert_eq!(relative("guide/tour.html", "guide/errors.html"), "errors.html");
        assert_eq!(relative("guide/tour.html", "design/DESIGN.html"), "../design/DESIGN.html");
        assert_eq!(relative("index.html", "guide/tour.html"), "guide/tour.html");
        assert_eq!(relative("design/x.html", "index.html"), "../index.html");
        assert_eq!(normalize(Path::new("docs/guide/../../DESIGN.md")), PathBuf::from("DESIGN.md"));
        assert_eq!(md_links("see [a](b.md#x) and `[c](d)`\n```\n[e](f)\n```"), vec!["b.md#x", "d"]);
    }
}
