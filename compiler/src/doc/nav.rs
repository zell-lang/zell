//! The sidebar shared by every page of the site: three collapsible groups
//! (Guide, Design, Standard library as a two-level tree) with the current
//! page marked and its headings listed right under it ("On this page").
//! Plain `<details>` elements, so it works without JavaScript; `nav.js`
//! only remembers the open state and highlights the heading in view.

use super::markdown::escape;

/// `(level, text, id)` of a heading, as returned by `markdown::page_html`.
pub type Heading = (u8, String, String);

/// The pages the sidebar links to (site-relative paths).
#[derive(Default, Clone)]
pub struct NavTree {
    /// `(output path, title)` of the guides, in reading order.
    pub guide: Vec<(String, String)>,
    /// Design documents: `DESIGN.md` first, then the rest by title.
    pub design: Vec<(String, String)>,
    /// Documented standard library namespaces (`net.http`), sorted.
    pub std: Vec<String>,
}

/// The relative URL from the directory of page `from` to `to` (both site-relative).
pub fn relative(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = from.split('/').collect::<Vec<_>>().split_last().map(|x| x.1.to_vec()).unwrap_or_default();
    let to_parts: Vec<&str> = to.split('/').collect();
    let common = from_dir.iter().zip(&to_parts).take_while(|(a, b)| a == b).count();
    let mut s = "../".repeat(from_dir.len() - common);
    s.push_str(&to_parts[common..].join("/"));
    s
}

/// The nested list of `h2`/`h3` headings ("On this page"); empty when
/// there are no `h2`.
pub fn toc_html(heads: &[Heading]) -> String {
    let items: Vec<&Heading> = heads.iter().filter(|h| (2..=3).contains(&h.0)).collect();
    if !items.iter().any(|h| h.0 == 2) {
        return String::new();
    }
    let mut o = String::from("<ol class=\"toc\" aria-label=\"On this page\">");
    let mut sub = false;
    let mut open_li = false;
    for (lvl, t, id) in items {
        let a = format!("<a href=\"#{}\">{}</a>", escape(id), escape(t));
        if *lvl == 2 || !open_li {
            if sub {
                o.push_str("</ol>");
                sub = false;
            }
            if open_li {
                o.push_str("</li>");
            }
            o.push_str(&format!("<li>{a}"));
            open_li = true;
        } else {
            if !sub {
                o.push_str("<ol>");
                sub = true;
            }
            o.push_str(&format!("<li>{a}</li>"));
        }
    }
    if sub {
        o.push_str("</ol>");
    }
    o.push_str("</li></ol>");
    o
}

fn group(key: &str, title: &str, open: bool, body: &str) -> String {
    let (o, c) = if open { (" open", " data-cur") } else { ("", "") };
    format!("<details class=\"grp\" data-key=\"{key}\"{o}{c}><summary>{title}</summary>{body}</details>")
}

/// The sidebar HTML of the page `cur` (site-relative output path); `toc`
/// is listed under its link.
pub fn sidebar(tree: &NavTree, cur: &str, toc: &[Heading]) -> String {
    let toc = toc_html(toc);
    let li = |out: &str, label: &str| -> String {
        if out == cur {
            format!("<li><a href=\"{}\" aria-current=\"page\">{label}</a>{toc}", relative(cur, out))
        } else {
            format!("<li><a href=\"{}\">{label}</a>", relative(cur, out))
        }
    };
    let mut o = String::from("<ul>");
    for (out, title) in &tree.guide {
        o.push_str(&li(out, &escape(title)));
    }
    o.push_str("</ul>");
    let guide = group("guide", "Guide", cur.starts_with("guide/"), &o);
    let mut o = String::from("<ul>");
    o.push_str(&li("design/index.html", "All documents"));
    for (out, title) in &tree.design {
        o.push_str(&li(out, &escape(title)));
    }
    o.push_str("</ul>");
    let design = group("design", "Design", cur.starts_with("design/"), &o);
    // std: roots with children are nested groups
    let mut o = String::from("<ul>");
    o.push_str(&li("std/index.html", "All namespaces"));
    let root_of = |n: &str| n.split('.').next().unwrap_or(n).to_string();
    let mut roots: Vec<String> = tree.std.iter().map(|n| root_of(n)).collect();
    roots.dedup();
    for r in roots {
        let kids: Vec<&String> = tree.std.iter().filter(|n| n.starts_with(&format!("{r}."))).collect();
        let has_self = tree.std.iter().any(|n| *n == r);
        let page = |n: &str| format!("std/{n}.html");
        if kids.is_empty() {
            o.push_str(&li(&page(&r), &escape(&r)));
            continue;
        }
        let here = cur.strip_prefix("std/").and_then(|c| c.strip_suffix(".html")).is_some_and(|c| c == r || c.starts_with(&format!("{r}.")));
        let mut body = String::from("<ul>");
        if has_self {
            body.push_str(&li(&page(&r), &escape(&r)));
        }
        for k in kids {
            body.push_str(&li(&page(k), &escape(k)));
        }
        body.push_str("</ul>");
        o.push_str(&format!("<li>{}</li>", group(&format!("std-{r}"), &escape(&r), here, &body)));
    }
    o.push_str("</ul>");
    let std = group("std", "Standard library", cur.starts_with("std/"), &o);
    format!("{guide}{design}{std}")
}

/// Client-side behaviour of the sidebar and the theme toggle; everything
/// works (statically) without it.
pub const NAV_JS: &str = r#"(function(){
var d=document;
function get(k){try{return window.localStorage.getItem(k)}catch(e){return null}}
function set(k,v){try{window.localStorage.setItem(k,v)}catch(e){}}
var side=d.getElementById('side');
if(side){
var gs=side.querySelectorAll('details[data-key]');
for(var i=0;i<gs.length;i++){(function(g){var k='zell-nav-'+g.getAttribute('data-key'),s=get(k);
if(s!==null&&!g.hasAttribute('data-cur'))g.open=(s==='1');
g.addEventListener('toggle',function(){set(k,g.open?'1':'0')})})(gs[i])}
var cur=side.querySelector('[aria-current="page"]');
if(cur&&cur.scrollIntoView)cur.scrollIntoView({block:'nearest'});
var toc=side.querySelector('ol.toc'),rt=d.getElementById('rtoc');
if(toc&&rt){var c=toc.cloneNode(true);c.removeAttribute('aria-label');var h=d.createElement('h2');h.textContent='On this page';rt.appendChild(h);rt.appendChild(c);
d.querySelector('.wrap').className+=' has-rtoc'}
var all=d.querySelectorAll('ol.toc a'),byId={};
for(var j=0;j<all.length;j++){var id=decodeURIComponent(all[j].getAttribute('href').slice(1));(byId[id]=byId[id]||[]).push(all[j])}
var ids=Object.keys(byId),els=[];for(var m=0;m<ids.length;m++){var e=d.getElementById(ids[m]);if(e)els.push(e)}
els.sort(function(a,b){return a.compareDocumentPosition(b)&4?-1:1});
function mark(id){for(var n=0;n<all.length;n++)all[n].classList.remove('active');(byId[id]||[]).forEach(function(a){a.classList.add('active')})}
if(els.length&&'IntersectionObserver' in window){
var vis={};var io=new IntersectionObserver(function(es){es.forEach(function(x){vis[x.target.id]=x.isIntersecting});
for(var n=0;n<els.length;n++){if(vis[els[n].id]){mark(els[n].id);return}}},{rootMargin:'-64px 0px -65% 0px'});
els.forEach(function(e){io.observe(e)})}
}
var b=d.getElementById('theme');
if(b){b.addEventListener('click',function(){var r=d.documentElement,t=r.getAttribute('data-theme'),dark=t?t==='dark':window.matchMedia('(prefers-color-scheme: dark)').matches;
t=dark?'light':'dark';r.setAttribute('data-theme',t);set('zell-theme',t);b.setAttribute('aria-pressed',t==='dark'?'true':'false')})}
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_nests_h3_under_h2() {
        let h = vec![(1, "T".into(), "t".into()), (2, "A".into(), "a".into()), (3, "A1".into(), "a1".into()), (2, "B".into(), "b".into())];
        assert_eq!(toc_html(&h), "<ol class=\"toc\" aria-label=\"On this page\"><li><a href=\"#a\">A</a><ol><li><a href=\"#a1\">A1</a></li></ol></li><li><a href=\"#b\">B</a></li></ol>");
        assert_eq!(toc_html(&[(1, "T".into(), "t".into())]), "");
    }

    #[test]
    fn sidebar_marks_current_and_opens_its_group() {
        let t = NavTree { guide: vec![("guide/tour.html".into(), "Tour".into())], design: vec![], std: vec!["net".into(), "net.http".into(), "strings".into()] };
        let s = sidebar(&t, "std/net.http.html", &[]);
        assert!(s.contains("<a href=\"net.http.html\" aria-current=\"page\">net.http</a>"));
        assert!(s.contains("<details class=\"grp\" data-key=\"std-net\" open data-cur>"));
        assert!(s.contains("data-key=\"guide\"><summary>Guide") && s.contains("href=\"../guide/tour.html\""));
    }
}
