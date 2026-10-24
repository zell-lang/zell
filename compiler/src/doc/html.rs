//! `zell doc --html <dir>`: a static site, one page per namespace, an
//! index page with client-side search over `search-index.json` (also
//! loaded as `search-index.js`, so the site works from `file://`), and
//! highlighted source pages with line anchors (`src/std/strings/strings.zl.html#L12`)
//! unless `--source-url` points the source links elsewhere.

use super::markdown::{self, escape};
use super::nav::{self, Heading, NavTree};
use super::{Docs, Example, Item, Namespace};
use std::collections::BTreeSet;
use std::path::Path;

/// The stylesheet of `zell doc --html` and `--site` (light and dark via
/// `prefers-color-scheme`).
pub const CSS: &str = r#":root{--bg:#fff;--fg:#1d1f23;--muted:#5f6670;--line:#e3e5e8;--code:#f5f6f8;--link:#0b62c4;--kw:#8a2be2;--str:#0a7a3a;--num:#b35900;--ty:#0b6e99;--com:#7a7f87;--dep:#b3261e;--side:#fafbfc;--hl:#e8f1fc;--hh:52px;color-scheme:light}
@media (prefers-color-scheme: dark){:root:not([data-theme="light"]){--bg:#16181c;--fg:#e4e6ea;--muted:#9aa1ab;--line:#2b2f36;--code:#1f2228;--link:#6cb2ff;--kw:#c792ea;--str:#8fd18f;--num:#f0a868;--ty:#7fd0f0;--com:#868c96;--dep:#ff8a80;--side:#1a1c21;--hl:#22324a;color-scheme:dark}}
:root[data-theme="dark"]{--bg:#16181c;--fg:#e4e6ea;--muted:#9aa1ab;--line:#2b2f36;--code:#1f2228;--link:#6cb2ff;--kw:#c792ea;--str:#8fd18f;--num:#f0a868;--ty:#7fd0f0;--com:#868c96;--dep:#ff8a80;--side:#1a1c21;--hl:#22324a;color-scheme:dark}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.6 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
@media (prefers-reduced-motion: no-preference){html{scroll-behavior:smooth}}
:focus-visible{outline:2px solid var(--link);outline-offset:2px;border-radius:3px}
.skip{position:absolute;left:8px;top:-60px;background:var(--bg);color:var(--link);padding:6px 12px;border:1px solid var(--line);border-radius:6px;z-index:50}.skip:focus{top:8px}
header.top{position:sticky;top:0;z-index:20;height:var(--hh);background:var(--bg);border-bottom:1px solid var(--line);padding:0 16px;display:flex;gap:16px;align-items:center}
header.top a{color:var(--fg)}header.top a.home{font-weight:700;font-size:1.1em}a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
header.top .hnav{display:flex;gap:16px}
#menu-toggle{position:absolute;opacity:0;width:1px;height:1px}.menu-btn,.backdrop{display:none}
.menu-btn{cursor:pointer;border:1px solid var(--line);border-radius:6px;padding:3px 10px;color:var(--fg)}#menu-toggle:focus-visible ~ header .menu-btn{outline:2px solid var(--link);outline-offset:2px}
#theme{display:none;margin-left:0;background:none;border:1px solid var(--line);border-radius:6px;color:var(--fg);cursor:pointer;padding:3px 9px;font:inherit}html.js #theme{display:block}
#theme::before{content:"Theme"}
.wrap{display:grid;grid-template-columns:270px minmax(0,1fr);max-width:1560px;margin:0 auto}.wrap.nosb{display:block;max-width:60rem}
nav.side{position:sticky;top:var(--hh);align-self:start;max-height:calc(100vh - var(--hh));overflow-y:auto;min-height:calc(100vh - var(--hh));padding:14px 12px 40px 16px;border-right:1px solid var(--line);background:var(--side);font-size:.92em}
nav.side ul,nav.side ol{list-style:none;padding:0;margin:0}nav.side li{margin:.1em 0}
nav.side details{border:0;padding:0;margin:0 0 .4em;border-radius:0}nav.side summary{cursor:pointer;color:var(--fg);font-weight:600;padding:3px 4px;border-radius:4px;list-style-position:inside}nav.side summary:hover{background:var(--hl)}
nav.side details.grp>summary{font-size:.8em;text-transform:uppercase;letter-spacing:.04em;color:var(--muted)}
nav.side details.grp details>summary{font-weight:400;color:var(--link)}
nav.side details.grp>ul{margin:.2em 0 .6em;padding-left:6px}nav.side details details>ul{margin-left:.6em;padding-left:.6em;border-left:1px solid var(--line)}
nav.side li>a{display:block;padding:2px 6px;border-radius:4px;color:var(--fg);overflow-wrap:anywhere}nav.side li>a:hover{background:var(--hl);text-decoration:none}
nav.side a[aria-current="page"]{font-weight:700;color:var(--link);background:var(--hl)}
nav.side ol.toc{margin:.2em 0 .5em 10px;padding-left:8px;border-left:2px solid var(--line);font-size:.93em}nav.side ol.toc ol{padding-left:12px}
nav.side ol.toc a{color:var(--muted);padding:1px 6px}nav.side ol.toc a.active,.rtoc a.active{color:var(--link);font-weight:600}
.rtoc{display:none}
main{min-width:0;max-width:46rem;width:100%;margin:0 auto;padding:16px 28px 64px}main.wide{max-width:64rem}
.wrap.nosb main{max-width:none}
h1,h2,h3,h4,section[id],.member[id],details[id]{scroll-margin-top:calc(var(--hh) + 10px)}
h1{font-size:1.7em;margin:.6em 0 .3em}h2{font-size:1.3em;margin:1.6em 0 .4em;border-bottom:1px solid var(--line);padding-bottom:.2em}h3{font-size:1.08em;margin:1.4em 0 .3em}
section>h3,.member>h3{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}a.anchor{margin-left:.4em;color:var(--muted);font-size:.8em;visibility:hidden}h1:hover a.anchor,h2:hover a.anchor,h3:hover a.anchor,h4:hover a.anchor,a.anchor:focus{visibility:visible}
code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:.9em}code{background:var(--code);padding:.1em .3em;border-radius:4px}
pre{background:var(--code);padding:10px 12px;border-radius:6px;overflow-x:auto;line-height:1.45}pre code{background:none;padding:0}
pre.sig{border-left:3px solid var(--link)}.k{color:var(--kw)}.s{color:var(--str)}.n{color:var(--num)}.t{color:var(--ty)}.a{color:var(--num)}.c{color:var(--com);font-style:italic}
table{border-collapse:collapse;width:100%}.table{overflow-x:auto}td,th{padding:4px 8px;border-bottom:1px solid var(--line);vertical-align:top;text-align:left}td:first-child{white-space:nowrap}
blockquote{margin:1em 0;padding:.2em 1em;border-left:3px solid var(--line);color:var(--muted)}hr{border:0;border-top:1px solid var(--line)}
.muted,.src{color:var(--muted);font-size:.85em}.src{float:right;font-family:ui-monospace,monospace}
.deprecated .tag,.tag.dep{color:var(--dep);font-weight:600;font-size:.8em;text-transform:uppercase;margin-right:.4em}.strike{text-decoration:line-through}
main details{margin:.6em 0;border:1px solid var(--line);border-radius:6px;padding:4px 10px}summary{cursor:pointer;color:var(--muted)}
.member{margin-left:18px}#q{flex:1;max-width:360px;margin-left:auto;padding:5px 10px;border:1px solid var(--line);border-radius:6px;background:var(--bg);color:var(--fg);font-size:14px}
#results{list-style:none;padding:6px 12px;margin:0;position:absolute;right:16px;top:calc(var(--hh) - 6px);width:min(560px,92vw);max-height:70vh;overflow:auto;background:var(--bg);border:1px solid var(--line);border-radius:6px;z-index:40}#results:empty{display:none}#results li{padding:3px 0}#results .kind{color:var(--muted);font-size:.85em;margin-left:.4em}
.pager{display:flex;justify-content:space-between;gap:1em;margin-top:3em;border-top:1px solid var(--line);padding-top:1em}
.lines a.ln{display:inline-block;width:4.5em;color:var(--muted);text-align:right;padding-right:1em;user-select:none}.lines span:target{background:rgba(255,200,0,.25)}
@media (min-width:1280px){.wrap.has-rtoc{grid-template-columns:270px minmax(0,1fr) 230px}.wrap.has-rtoc .rtoc{display:block;position:sticky;top:var(--hh);align-self:start;max-height:calc(100vh - var(--hh));overflow-y:auto;padding:18px 16px;font-size:.88em}
.rtoc h2{font-size:.8em;text-transform:uppercase;letter-spacing:.04em;color:var(--muted);border:0;margin:0 0 .5em;padding:0}.rtoc ol{list-style:none;margin:0;padding:0}.rtoc ol ol{padding-left:12px}.rtoc a{display:block;color:var(--muted);padding:2px 0;overflow-wrap:anywhere}}
@media (max-width:899px){.wrap{display:block}.menu-btn{display:block}main{padding:12px 16px 48px}
nav.side{display:none;position:fixed;top:var(--hh);left:0;bottom:0;width:min(330px,86vw);max-height:none;z-index:30;border-right:1px solid var(--line);box-shadow:4px 0 16px rgba(0,0,0,.25)}
#menu-toggle:checked ~ .wrap nav.side{display:block}#menu-toggle:checked ~ .backdrop{display:block;position:fixed;inset:var(--hh) 0 0 0;background:rgba(0,0,0,.4);z-index:25}}
@media (max-width:640px){header.top .hnav{display:none}header.top{gap:10px}}
@media print{header.top,nav.side,.rtoc,.pager{display:none}.wrap{display:block}}
"#;

/// The client-side search: `window.ZELL_INDEX` entries `{n, k, s, u}`
/// (name, kind, summary, URL relative to the site root), `ZELL_ROOT` the
/// path from the current page to the root.
pub const SEARCH_JS: &str = r#"(function(){
var q=document.getElementById('q'),out=document.getElementById('results');if(!q||!out)return;
var idx=window.ZELL_INDEX||[],root=window.ZELL_ROOT||'';
function score(e,t){var n=e.n.toLowerCase(),last=n.slice(n.lastIndexOf('.')+1);if(last===t||n===t)return 0;if(last.indexOf(t)===0)return 1;if(n.indexOf(t)>=0)return 2;if((e.s||'').toLowerCase().indexOf(t)>=0)return 3;return -1}
function run(){var t=q.value.trim().toLowerCase();out.innerHTML='';if(!t)return;var hits=[];
for(var i=0;i<idx.length;i++){var s=score(idx[i],t);if(s>=0)hits.push([s,idx[i]])}
hits.sort(function(a,b){return a[0]-b[0]||a[1].n.length-b[1].n.length});
hits.slice(0,60).forEach(function(h){var e=h[1],li=document.createElement('li'),a=document.createElement('a');a.href=root+e.u;a.textContent=e.n;li.appendChild(a);
var k=document.createElement('span');k.className='kind';k.textContent=e.k+(e.s?' — '+e.s:'');li.appendChild(k);out.appendChild(li)})}
q.addEventListener('input',run);document.addEventListener('keydown',function(e){if(e.key==='/'&&document.activeElement!==q){e.preventDefault();q.focus()}if(e.key==='Escape'){q.value='';run()}});
var m=location.hash.match(/^#q=(.*)$/);if(m){q.value=decodeURIComponent(m[1]);run()}
})();
"#;

/// Where a page sits: `root` is the path from it to the site root (`""`,
/// `"../"`), `nav` the links of the header (relative to the root).
pub struct Chrome<'a> {
    pub root: String,
    pub nav: &'a [(&'a str, &'a str)],
    /// HTML of a sidebar (already relative to the page), if any.
    pub side: String,
}

/// The header links of the standalone API reference.
pub const API_NAV: &[(&str, &str)] = &[("Packages", "index.html")];

/// One HTML page with the shared header, search box and stylesheet. With
/// a sidebar the page gets the "Menu" toggle (a checkbox, no script
/// needed) and the "On this page" column.
pub fn page(title: &str, body: &str, ch: &Chrome) -> String {
    page_with(title, body, ch, false)
}

/// Like [`page`]; `wide` widens the content column (API pages).
pub fn page_with(title: &str, body: &str, ch: &Chrome, wide: bool) -> String {
    let r = &ch.root;
    let mut nav = String::from("<span class=\"hnav\">");
    for (label, url) in ch.nav {
        nav.push_str(&format!("<a href=\"{r}{url}\">{}</a>", escape(label)));
    }
    nav.push_str("</span>");
    let has_side = !ch.side.is_empty();
    let (toggle, menu, side, backdrop) = if has_side {
        (
            "<input type=\"checkbox\" id=\"menu-toggle\" aria-label=\"Show the menu\">",
            "<label class=\"menu-btn\" for=\"menu-toggle\">Menu</label>",
            format!("<nav class=\"side\" id=\"side\" aria-label=\"Documentation\">{}</nav>", ch.side),
            "<label class=\"backdrop\" for=\"menu-toggle\"></label>",
        )
    } else {
        ("", "", String::new(), "")
    };
    let rtoc = if has_side { "<aside class=\"rtoc\" id=\"rtoc\" aria-label=\"On this page\"></aside>" } else { "" };
    let wrap = if has_side { "wrap" } else { "wrap nosb" };
    let main = if wide { "<main id=\"content\" class=\"wide\" tabindex=\"-1\">" } else { "<main id=\"content\" tabindex=\"-1\">" };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<title>{}</title><link rel=\"stylesheet\" href=\"{r}style.css\"><script>(function(){{var e=document.documentElement;e.className='js';try{{var t=localStorage.getItem('zell-theme');if(t)e.setAttribute('data-theme',t)}}catch(x){{}}}})()</script></head>\n<body><a class=\"skip\" href=\"#content\">Skip to content</a>{toggle}<header class=\"top\">{menu}<a class=\"home\" href=\"{r}index.html\">Zell</a>{nav}<input id=\"q\" placeholder=\"Search (press /)\" autocomplete=\"off\" aria-label=\"Search\"><ul id=\"results\"></ul><button id=\"theme\" type=\"button\" aria-label=\"Toggle dark mode\" title=\"Toggle dark mode\"></button></header>\n<div class=\"{wrap}\">{side}{main}\n{body}</main>{rtoc}</div>{backdrop}\n<script>window.ZELL_ROOT=\"{r}\";</script><script src=\"{r}search-index.js\"></script><script src=\"{r}search.js\"></script><script src=\"{r}nav.js\"></script></body></html>\n",
        escape(title)
    )
}

fn page_name(ns: &str) -> String {
    format!("{ns}.html")
}

fn anchor(path: &str) -> String {
    path.to_string()
}

struct Ctx<'a> {
    docs: &'a Docs,
    source_url: Option<&'a str>,
    sources: BTreeSet<String>,
    layout: &'a Layout<'a>,
}

/// Where the API reference goes in a site.
pub struct Layout<'a> {
    /// From the API directory to the site root (`""` standalone, `"../"`
    /// under `std/` of a `--site`).
    pub root: &'a str,
    /// The API directory relative to the site root (`""` or `"std/"`),
    /// for search URLs.
    pub prefix: &'a str,
    pub nav: &'a [(&'a str, &'a str)],
    /// The pages of the whole site for the shared sidebar; `None` for a
    /// standalone API site (its sidebar lists the namespaces only).
    pub tree: Option<&'a NavTree>,
}

impl Layout<'_> {
    fn chrome(&self, extra_up: usize, side: String) -> Chrome<'_> {
        Chrome { root: format!("{}{}", "../".repeat(extra_up), self.root), nav: self.nav, side }
    }
}

/// The sidebar of an API page: the site-wide tree, or (standalone API
/// site) the namespace list; `current` is the namespace (`""` for the index).
fn api_side(docs: &Docs, layout: &Layout, current: &str, toc: &[Heading]) -> String {
    if let Some(tree) = layout.tree {
        let cur = if current.is_empty() { format!("{}index.html", layout.prefix) } else { format!("{}{}", layout.prefix, page_name(current)) };
        return nav::sidebar(tree, &cur, toc);
    }
    let toc = nav::toc_html(toc);
    let mut o = String::from("<ul>");
    let link = |href: &str, label: &str, here: bool| {
        if here {
            format!("<li><a href=\"{href}\" aria-current=\"page\">{label}</a>{}", if current.is_empty() { "" } else { &toc })
        } else {
            format!("<li><a href=\"{href}\">{label}</a>")
        }
    };
    o.push_str(&link("index.html", "All namespaces", current.is_empty()));
    for n in docs.namespaces.iter().filter(|n| !(n.items.is_empty() && n.examples.is_empty() && n.doc.is_empty())) {
        o.push_str(&link(&page_name(&n.name), &escape(&n.name), n.name == current));
    }
    format!("<details class=\"grp\" data-key=\"api\" open data-cur><summary>Namespaces</summary>{o}</ul></details>")
}

impl Ctx<'_> {
    fn url(&self, ni: usize, path: &str) -> String {
        let p = page_name(&self.docs.namespaces[ni].name);
        if path.is_empty() {
            p
        } else {
            format!("{p}#{}", anchor(path))
        }
    }

    fn src_link(&mut self, file: &str, line: u32) -> String {
        if file.is_empty() {
            return "<span class=\"src\">builtin</span>".into();
        }
        let href = match self.source_url {
            Some(u) => format!("{u}/{file}#L{line}"),
            None => {
                self.sources.insert(file.to_string());
                format!("src/{file}.html#L{line}")
            }
        };
        format!("<a class=\"src\" href=\"{}\">{}:{line}</a>", escape(&href), escape(file))
    }

    fn doc_html(&self, ns: &str, doc: &str) -> String {
        let link = |t: &str| self.docs.resolve(Some(ns), t).map(|(i, p)| self.url(i, &p));
        markdown::to_html(&markdown::parse(doc), &link)
    }
}

fn examples_html(ctx: &Ctx, exs: &[Example]) -> String {
    let mut o = String::new();
    for e in exs {
        let title = if e.suffix.is_empty() { "Example".to_string() } else { format!("Example ({})", escape(&e.suffix)) };
        o.push_str(&format!("<details id=\"{}\"><summary>{title}</summary>\n<pre><code class=\"language-zell\">{}</code></pre>\n", escape(&e.name), markdown::highlight(&e.code)));
        if let Some(out) = &e.output {
            o.push_str(&format!("<p class=\"muted\">Output:</p><pre><code>{}</code></pre>\n", escape(out)));
        }
        o.push_str("</details>\n");
    }
    let _ = ctx;
    o
}

fn title_html(it: &Item, label: &str) -> String {
    let name = escape(label);
    if it.deprecated.is_some() {
        format!("<span class=\"strike\">{name}</span> <span class=\"tag dep\">deprecated</span>")
    } else {
        name
    }
}

fn item_html(ctx: &mut Ctx, n: &Namespace, it: &Item) -> String {
    let mut o = format!("<section id=\"{}\">\n<h3>{} {}{}</h3>\n", escape(&it.name), it.kind, title_html(it, &it.name), ctx.src_link(&it.file, it.line));
    let mut sig = it.signature.clone();
    if it.is_type() {
        let fields: Vec<&Item> = it.members.iter().filter(|m| m.kind != "method").collect();
        if !fields.is_empty() {
            sig.push_str(" {\n");
            for f in fields {
                sig.push_str(&format!("    {}", f.signature));
                if !f.summary.is_empty() {
                    sig.push_str(&format!("  // {}", f.summary));
                }
                sig.push('\n');
            }
            sig.push('}');
        }
    }
    o.push_str(&format!("<pre class=\"sig\"><code>{}</code></pre>\n", markdown::highlight(&sig)));
    o.push_str(&ctx.doc_html(&n.name, &it.doc));
    o.push_str(&examples_html(ctx, &it.examples));
    for m in it.members.iter().filter(|m| m.kind == "method") {
        let path = format!("{}.{}", it.name, m.name);
        o.push_str(&format!("<div class=\"member\" id=\"{}\">\n<h3>{}{}</h3>\n", escape(&path), title_html(m, &path), ctx.src_link(&m.file, m.line)));
        o.push_str(&format!("<pre class=\"sig\"><code>{}</code></pre>\n", markdown::highlight(&m.signature)));
        o.push_str(&ctx.doc_html(&n.name, &m.doc));
        o.push_str(&examples_html(ctx, &m.examples));
        o.push_str("</div>\n");
    }
    o.push_str("</section>\n");
    o
}

fn namespace_page(ctx: &mut Ctx, n: &Namespace) -> String {
    let mut toc: Vec<Heading> = Vec::new();
    let mut o = format!("<h1>namespace {}</h1>\n", escape(&n.name));
    if let Some(f) = n.files.first() {
        let dir = f.rsplit_once('/').map_or(f.as_str(), |x| x.0);
        o.push_str(&format!("<p class=\"muted\">{}</p>\n", escape(dir)));
    }
    o.push_str(&ctx.doc_html(&n.name, &n.doc));
    o.push_str(&examples_html(ctx, &n.examples));
    // sub-namespaces
    let subs: Vec<&Namespace> = ctx.docs.namespaces.iter().filter(|s| s.name.starts_with(&format!("{}.", n.name)) && !s.name[n.name.len() + 1..].contains('.')).collect();
    if !subs.is_empty() {
        toc.push((2, "Namespaces".into(), "sec-namespaces".into()));
        o.push_str("<h2 id=\"sec-namespaces\">Namespaces</h2>\n<table>\n");
        for s in subs {
            o.push_str(&format!("<tr><td><a href=\"{}\">{}</a></td><td>{}</td></tr>\n", page_name(&s.name), escape(&s.name), escape(&s.summary)));
        }
        o.push_str("</table>\n");
    }
    let groups: [(&str, &[&str]); 4] = [("Constants", &["const"]), ("Variables", &["var"]), ("Types", &["struct", "interface", "enum"]), ("Functions", &["func"])];
    if !n.items.is_empty() {
        toc.push((2, "Index".into(), "sec-index".into()));
        o.push_str("<h2 id=\"sec-index\">Index</h2>\n<table>\n");
        for (title, kinds) in groups {
            let items: Vec<&Item> = n.items.iter().filter(|i| kinds.contains(&i.kind)).collect();
            if items.is_empty() {
                continue;
            }
            o.push_str(&format!("<tr><td colspan=\"2\"><strong>{title}</strong></td></tr>\n"));
            for it in items {
                o.push_str(&format!(
                    "<tr><td><a href=\"#{}\">{}</a></td><td>{}</td></tr>\n",
                    escape(&it.name),
                    title_html(it, &it.name),
                    markdown::inline_html(&it.summary, &|_| None)
                ));
            }
        }
        o.push_str("</table>\n");
    }
    for (title, kinds) in groups {
        let items: Vec<&Item> = n.items.iter().filter(|i| kinds.contains(&i.kind)).collect();
        if items.is_empty() {
            continue;
        }
        o.push_str(&format!("<h2 id=\"sec-{}\">{title}</h2>\n", title.to_lowercase()));
        toc.push((2, title.to_string(), format!("sec-{}", title.to_lowercase())));
        for it in items {
            toc.push((3, it.name.clone(), it.name.clone()));
            o.push_str(&item_html(ctx, n, it));
        }
    }
    page_with(&n.name, &o, &ctx.layout.chrome(0, api_side(ctx.docs, ctx.layout, &n.name, &toc)), true)
}

fn index_page(docs: &Docs, layout: &Layout) -> String {
    let mut o = String::from("<h1>API reference</h1>\n");
    for (title, std) in [("Packages", false), ("Standard library", true)] {
        let list: Vec<&Namespace> = docs.namespaces.iter().filter(|n| n.std == std && !(n.items.is_empty() && n.examples.is_empty() && n.doc.is_empty())).collect();
        if list.is_empty() {
            continue;
        }
        o.push_str(&format!("<h2>{title}</h2>\n<table>\n"));
        for n in list {
            o.push_str(&format!("<tr><td><a href=\"{}\">{}</a></td><td>{}</td></tr>\n", page_name(&n.name), escape(&n.name), markdown::inline_html(&n.summary, &|_| None)));
        }
        o.push_str("</table>\n");
    }
    page_with("API reference", &o, &layout.chrome(0, api_side(docs, layout, "", &[])), true)
}

/// The search entries of the API: one per namespace, declaration and
/// method, with URLs under `prefix`.
pub fn search_entries(docs: &Docs, prefix: &str) -> Vec<serde_json::Value> {
    let mut v = Vec::new();
    for n in &docs.namespaces {
        let p = format!("{prefix}{}", page_name(&n.name));
        v.push(serde_json::json!({"n": n.name, "k": "namespace", "s": n.summary, "u": p}));
        for it in &n.items {
            v.push(serde_json::json!({"n": format!("{}.{}", n.name, it.name), "k": it.kind, "s": it.summary, "u": format!("{p}#{}", it.name)}));
            for m in it.members.iter().filter(|m| m.kind == "method") {
                v.push(serde_json::json!({"n": format!("{}.{}.{}", n.name, it.name, m.name), "k": "method", "s": m.summary, "u": format!("{p}#{}.{}", it.name, m.name)}));
            }
        }
    }
    v
}

/// The search index of a standalone API site.
pub fn search_index(docs: &Docs) -> serde_json::Value {
    serde_json::Value::Array(search_entries(docs, ""))
}

fn source_page(display: &str, text: &str, ch: &Chrome) -> String {
    let mut o = format!("<h1>{}</h1>\n<pre class=\"lines\"><code>", escape(display));
    let hl = markdown::highlight(text);
    for (i, l) in hl.split('\n').enumerate() {
        let n = i + 1;
        o.push_str(&format!("<span id=\"L{n}\"><a class=\"ln\" href=\"#L{n}\">{n}</a>{l}</span>\n"));
    }
    o.push_str("</code></pre>\n");
    page(display, &o, ch)
}

/// Writes a file, creating its directory.
pub fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the standalone API site into `dir`; returns the number of pages.
pub fn write_site(docs: &Docs, dir: &Path, source_url: Option<&str>) -> Result<usize, String> {
    let layout = Layout { root: "", prefix: "", nav: API_NAV, tree: None };
    write(&dir.join("style.css"), CSS)?;
    write(&dir.join("search.js"), SEARCH_JS)?;
    write(&dir.join("nav.js"), nav::NAV_JS)?;
    let idx = search_index(docs);
    write(&dir.join("search-index.json"), &serde_json::to_string(&idx).unwrap())?;
    write(&dir.join("search-index.js"), &format!("window.ZELL_INDEX = {};\n", serde_json::to_string(&idx).unwrap()))?;
    write(&dir.join("docs.json"), &serde_json::to_string(docs).unwrap())?;
    write_api(docs, dir, source_url, &layout)
}

/// Writes the API pages (index, namespaces, sources) into `dir`.
pub fn write_api(docs: &Docs, dir: &Path, source_url: Option<&str>, layout: &Layout) -> Result<usize, String> {
    let mut ctx = Ctx { docs, source_url, sources: BTreeSet::new(), layout };
    let mut pages = 0;
    write(&dir.join("index.html"), &index_page(docs, layout))?;
    pages += 1;
    for n in &docs.namespaces {
        if n.items.is_empty() && n.examples.is_empty() && n.doc.is_empty() {
            continue;
        }
        let html = namespace_page(&mut ctx, n);
        write(&dir.join(page_name(&n.name)), &html)?;
        pages += 1;
    }
    // source pages, for the files that were linked
    let mut paths = std::collections::HashMap::new();
    for n in &docs.namespaces {
        for it in &n.items {
            if let Some(p) = &it.path {
                paths.insert(it.file.clone(), p.clone());
            }
        }
    }
    for file in &ctx.sources {
        let Some(p) = paths.get(file) else { continue };
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        let depth = file.matches('/').count() + 1;
        let html = source_page(file, &text, &layout.chrome(depth, String::new()));
        write(&dir.join("src").join(format!("{file}.html")), &html)?;
        pages += 1;
    }
    Ok(pages)
}
