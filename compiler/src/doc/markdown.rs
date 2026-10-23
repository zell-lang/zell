//! The small Markdown dialect of doc comments, shared by `zell doc`
//! (HTML and terminal output) and the LSP hover.
//!
//! Blocks: paragraphs, `#` headings, `-`/`*`/`1.` lists (one paragraph
//! per item, continuation lines indented), fenced code (```` ``` ````,
//! optional language) and indented code (lines indented by two or more
//! spaces after a blank line or at the start), and a paragraph starting
//! with `Deprecated:` (rendered as a tag). Inline: `` `code` ``,
//! `**strong**`, `*em*` / `_em_`, `[text](url)` and `<https://...>`.
//! A code span that names a symbol (`` `strings.Split` ``) becomes a link
//! when the caller's resolver knows it.

/// One block of a doc comment.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, String),
    Para(String),
    /// `ordered`, items (inline text each).
    List(bool, Vec<String>),
    /// Language (`zell` for indented code), text without the indentation.
    Code(String, String),
    /// The text after `Deprecated:`.
    Deprecated(String),
    /// A pipe table (documents): header cells and rows.
    Table(Vec<String>, Vec<Vec<String>>),
    /// `> text` (documents).
    Quote(String),
    /// `---` (documents).
    Rule,
}

/// Resolves the text of a code span to a link target (`None`: no link).
pub type Resolver<'a> = &'a dyn Fn(&str) -> Option<String>;

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

fn list_marker(l: &str) -> Option<(bool, &str)> {
    let t = l.trim_start();
    if let Some(r) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ ")) {
        return Some((false, r));
    }
    let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && digits < 4 {
        if let Some(r) = t[digits..].strip_prefix(". ").or_else(|| t[digits..].strip_prefix(") ")) {
            return Some((true, r));
        }
    }
    None
}

fn dedent(lines: &[&str]) -> String {
    let n = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| indent_of(l)).min().unwrap_or(0);
    let mut out: Vec<&str> = lines.iter().map(|l| if l.len() >= n { &l[n..] } else { l.trim_start() }).collect();
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Parses a doc comment (comment markers already stripped).
pub fn parse(doc: &str) -> Vec<Block> {
    parse_with(doc, false)
}

fn split_row(l: &str) -> Vec<String> {
    let t = l.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut code = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '`' => {
                code = !code;
                cur.push(c);
            }
            '|' if !code => cells.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

fn is_table_sep(l: &str) -> bool {
    let t = l.trim();
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Parses Markdown; `page` selects document rules (whole Markdown files:
/// tables, quotes, rules, indented code only at four spaces) instead of
/// doc-comment rules.
pub fn parse_with(doc: &str, page: bool) -> Vec<Block> {
    let code_indent = if page { 4 } else { 2 };
    let lines: Vec<&str> = doc.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut after_blank = true;
    while i < lines.len() {
        let l = lines[i];
        let t = l.trim();
        if t.is_empty() {
            after_blank = true;
            i += 1;
            continue;
        }
        // fenced code
        if let Some(lang) = t.strip_prefix("```") {
            let fence_indent = indent_of(l);
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                let x = lines[i];
                body.push(if indent_of(x) >= fence_indent { &x[fence_indent.min(x.len())..] } else { x.trim_start() });
                i += 1;
            }
            i += 1;
            let lang = if lang.trim().is_empty() { "zell" } else { lang.trim() };
            out.push(Block::Code(lang.to_string(), dedent(&body)));
            after_blank = false;
            continue;
        }
        // indented code
        if after_blank && indent_of(l) >= code_indent && list_marker(l).is_none() && !matches!(out.last(), Some(Block::List(..)) if page) {
            let mut body = Vec::new();
            while i < lines.len() && (lines[i].trim().is_empty() || indent_of(lines[i]) >= code_indent) {
                body.push(lines[i]);
                i += 1;
            }
            out.push(Block::Code("zell".into(), dedent(&body)));
            after_blank = false;
            continue;
        }
        if page {
            // table
            if t.starts_with('|') && i + 1 < lines.len() && is_table_sep(lines[i + 1]) {
                let head = split_row(t);
                let mut rows = Vec::new();
                i += 2;
                while i < lines.len() && lines[i].trim().starts_with('|') {
                    rows.push(split_row(lines[i]));
                    i += 1;
                }
                out.push(Block::Table(head, rows));
                after_blank = false;
                continue;
            }
            if t.starts_with('>') {
                let mut q = Vec::new();
                while i < lines.len() && lines[i].trim().starts_with('>') {
                    q.push(lines[i].trim().trim_start_matches('>').trim());
                    i += 1;
                }
                out.push(Block::Quote(q.join(" ")));
                after_blank = false;
                continue;
            }
            if t == "---" || t == "***" || t == "___" {
                out.push(Block::Rule);
                i += 1;
                continue;
            }
            // raw HTML lines (comments, <details>) are dropped
            if t.starts_with("<!--") || t.starts_with("<details") || t.starts_with("</details") || t.starts_with("<summary") {
                i += 1;
                continue;
            }
        }
        // heading
        if t.starts_with('#') {
            let level = t.bytes().take_while(|&b| b == b'#').count();
            if level <= 6 && t[level..].starts_with(' ') {
                out.push(Block::Heading(level as u8, t[level..].trim().to_string()));
                i += 1;
                after_blank = false;
                continue;
            }
        }
        // list
        if let Some((ordered, _)) = list_marker(l) {
            let mut items: Vec<String> = Vec::new();
            while i < lines.len() {
                let x = lines[i];
                if x.trim().is_empty() {
                    // a blank line ends the list unless another item follows
                    if i + 1 < lines.len() && list_marker(lines[i + 1]).is_some_and(|(o, _)| o == ordered) {
                        i += 1;
                        continue;
                    }
                    break;
                }
                if let Some((o, rest)) = list_marker(x) {
                    if o != ordered {
                        break;
                    }
                    items.push(rest.trim().to_string());
                } else if indent_of(x) >= 1 || !items.is_empty() && !x.trim().is_empty() && indent_of(x) > 0 {
                    if let Some(last) = items.last_mut() {
                        last.push(' ');
                        last.push_str(x.trim());
                    }
                } else {
                    break;
                }
                i += 1;
            }
            out.push(Block::List(ordered, items));
            after_blank = false;
            continue;
        }
        // paragraph
        let mut para = Vec::new();
        while i < lines.len() {
            let x = lines[i];
            let xt = x.trim();
            if xt.is_empty() || xt.starts_with("```") || (!para.is_empty() && (list_marker(x).is_some() || xt.starts_with('#') || page && (xt.starts_with('|') || xt.starts_with('>')))) {
                break;
            }
            para.push(xt);
            i += 1;
        }
        let text = para.join(" ");
        match text.strip_prefix("Deprecated:") {
            Some(rest) => out.push(Block::Deprecated(rest.trim().to_string())),
            None => out.push(Block::Para(text)),
        }
        after_blank = false;
    }
    out
}

/// The first sentence of the first paragraph (Go's rule: up to the first
/// period followed by a space, or the end of the paragraph).
pub fn summary(doc: &str) -> String {
    let first = parse(doc).into_iter().find_map(|b| match b {
        Block::Para(p) => Some(p),
        _ => None,
    });
    let Some(p) = first else { return String::new() };
    let bytes = p.as_bytes();
    let mut depth = 0;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'`' => depth ^= 1,
            b'.' if depth == 0 && (i + 1 == bytes.len() || bytes[i + 1] == b' ') => {
                // not after a lone capital (e.g. "e.g." handled loosely)
                let prev = p[..i].rsplit(' ').next().unwrap_or("");
                if prev == "e.g" || prev == "i.e" || prev == "vs" || prev == "etc" && i + 1 != bytes.len() {
                    continue;
                }
                return p[..=i].to_string();
            }
            _ => {}
        }
    }
    p
}

/// True if the doc has a `Deprecated:` paragraph.
pub fn is_deprecated(doc: &str) -> Option<String> {
    parse(doc).into_iter().find_map(|b| match b {
        Block::Deprecated(t) => Some(t),
        _ => None,
    })
}

/// HTML escaping of text.
pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

/// Inline tokens of a paragraph.
#[derive(Debug, PartialEq)]
enum Inline {
    Text(String),
    Code(String),
    Strong(String),
    Em(String),
    Link(String, String),
}

fn inlines(s: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut text = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    let flush = |text: &mut String, out: &mut Vec<Inline>| {
        if !text.is_empty() {
            out.push(Inline::Text(std::mem::take(text)));
        }
    };
    while i < b.len() {
        let rest = &s[i..];
        if b[i] == b'`' {
            let run = rest.bytes().take_while(|&c| c == b'`').count();
            let fence = &rest[..run];
            if let Some(end) = rest[run..].find(fence) {
                flush(&mut text, &mut out);
                out.push(Inline::Code(rest[run..run + end].trim().to_string()));
                i += run + end + run;
                continue;
            }
            text.push_str(fence);
            i += run;
            continue;
        }
        if rest.starts_with("**") {
            if let Some(end) = rest[2..].find("**") {
                if end > 0 {
                    flush(&mut text, &mut out);
                    out.push(Inline::Strong(rest[2..2 + end].to_string()));
                    i += end + 4;
                    continue;
                }
            }
        }
        if (b[i] == b'*' || b[i] == b'_') && i + 1 < b.len() && b[i + 1] != b' ' && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let m = b[i] as char;
            if let Some(end) = rest[1..].find(m) {
                let after = rest.as_bytes().get(end + 2).copied();
                if end > 0 && rest.as_bytes()[end] != b' ' && after.is_none_or(|a| !a.is_ascii_alphanumeric()) {
                    flush(&mut text, &mut out);
                    out.push(Inline::Em(rest[1..1 + end].to_string()));
                    i += end + 2;
                    continue;
                }
            }
        }
        if b[i] == b'[' {
            if let Some(close) = rest.find("](") {
                if let Some(end) = rest[close + 2..].find(')') {
                    let label = &rest[1..close];
                    if !label.contains('[') {
                        flush(&mut text, &mut out);
                        out.push(Inline::Link(label.to_string(), rest[close + 2..close + 2 + end].to_string()));
                        i += close + 3 + end;
                        continue;
                    }
                }
            }
        }
        if b[i] == b'<' && (rest.starts_with("<http://") || rest.starts_with("<https://")) {
            if let Some(end) = rest.find('>') {
                flush(&mut text, &mut out);
                let u = rest[1..end].to_string();
                out.push(Inline::Link(u.clone(), u));
                i += end + 1;
                continue;
            }
        }
        let c = rest.chars().next().unwrap();
        text.push(c);
        i += c.len_utf8();
    }
    flush(&mut text, &mut out);
    out
}

/// Inline Markdown to HTML.
pub fn inline_html(s: &str, link: Resolver) -> String {
    inline_html_with(s, link, &|u: &str| u.to_string())
}

/// Rewrites the target of a Markdown link (`guide.md` -> `guide.html`).
pub type Href<'a> = &'a dyn Fn(&str) -> String;

fn inline_html_with(s: &str, link: Resolver, href: Href) -> String {
    let mut o = String::new();
    for t in inlines(s) {
        match t {
            Inline::Text(t) => o.push_str(&escape(&t)),
            Inline::Code(c) => match link(&c) {
                Some(url) => o.push_str(&format!("<a href=\"{}\"><code>{}</code></a>", escape(&url), escape(&c))),
                None => o.push_str(&format!("<code>{}</code>", escape(&c))),
            },
            Inline::Strong(t) => o.push_str(&format!("<strong>{}</strong>", inline_html_with(&t, link, href))),
            Inline::Em(t) => o.push_str(&format!("<em>{}</em>", inline_html_with(&t, link, href))),
            Inline::Link(t, u) => o.push_str(&format!("<a href=\"{}\">{}</a>", escape(&href(&u)), inline_html_with(&t, link, href))),
        }
    }
    o
}

/// GitHub's heading anchor: lower case, letters, digits, `-` and `_`
/// kept, spaces to `-`.
pub fn slug(text: &str) -> String {
    let plain = inline_text(text);
    let mut o = String::new();
    for c in plain.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            o.extend(c.to_lowercase());
        } else if c == ' ' {
            o.push('-');
        }
    }
    o
}

/// A whole Markdown document to HTML: headings get GitHub-style ids (made
/// unique), links are rewritten by `href`, `zell` code is highlighted.
/// Returns the HTML and the (level, text, id) of every heading.
pub fn page_html(md: &str, href: Href) -> (String, Vec<(u8, String, String)>) {
    let blocks = parse_with(md, true);
    let mut used: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut heads = Vec::new();
    let none = |_: &str| None;
    let mut o = String::new();
    for b in &blocks {
        match b {
            Block::Heading(n, t) => {
                let base = slug(t);
                let k = used.entry(base.clone()).or_insert(0);
                let id = if *k == 0 { base.clone() } else { format!("{base}-{k}") };
                *k += 1;
                o.push_str(&format!("<h{n} id=\"{}\">{}<a class=\"anchor\" href=\"#{}\">#</a></h{n}>\n", escape(&id), inline_html_with(t, &none, href), escape(&id)));
                heads.push((*n, inline_text(t), id));
            }
            other => o.push_str(&render_block(other, &none, href, 0)),
        }
    }
    (o, heads)
}

fn render_block(b: &Block, link: Resolver, href: Href, shift: u8) -> String {
    let il = |t: &str| inline_html_with(t, link, href);
    match b {
        Block::Heading(n, t) => {
            let n = (*n + shift).min(6);
            format!("<h{n}>{}</h{n}>\n", il(t))
        }
        Block::Para(t) => format!("<p>{}</p>\n", il(t)),
        Block::Deprecated(t) => format!("<p class=\"deprecated\"><span class=\"tag\">Deprecated</span> {}</p>\n", il(t)),
        Block::List(ordered, items) => {
            let tag = if *ordered { "ol" } else { "ul" };
            let mut o = format!("<{tag}>\n");
            for it in items {
                o.push_str(&format!("<li>{}</li>\n", il(it)));
            }
            o + &format!("</{tag}>\n")
        }
        Block::Code(lang, code) => {
            let body = if lang == "zell" { highlight(code) } else { escape(code) };
            format!("<pre><code class=\"language-{}\">{body}</code></pre>\n", escape(lang))
        }
        Block::Table(head, rows) => {
            let mut o = String::from("<div class=\"table\"><table>\n<thead><tr>");
            for h in head {
                o.push_str(&format!("<th>{}</th>", il(h)));
            }
            o.push_str("</tr></thead>\n<tbody>\n");
            for r in rows {
                o.push_str("<tr>");
                for c in r {
                    o.push_str(&format!("<td>{}</td>", il(c)));
                }
                o.push_str("</tr>\n");
            }
            o + "</tbody></table></div>\n"
        }
        Block::Quote(t) => format!("<blockquote><p>{}</p></blockquote>\n", il(t)),
        Block::Rule => "<hr>\n".to_string(),
    }
}

/// Highlights Zell code for HTML with the compiler's lexer: keywords
/// `k`, strings and chars `s`, numbers `n`, type names (upper case) `t`,
/// attributes `a`, comments `c` (the text between tokens).
pub fn highlight(code: &str) -> String {
    use crate::token::TokenKind as K;
    let mut diags = crate::diagnostics::Diagnostics::new();
    let toks = crate::lexer::lex(code, &mut diags);
    let mut o = String::new();
    let mut pos = 0usize;
    let gap = |o: &mut String, g: &str| {
        // comments live between tokens
        let mut rest = g;
        while !rest.is_empty() {
            let at = [rest.find("//"), rest.find("/*")].into_iter().flatten().min();
            match at {
                None => {
                    o.push_str(&escape(rest));
                    break;
                }
                Some(i) => {
                    o.push_str(&escape(&rest[..i]));
                    let end = if rest[i..].starts_with("//") { rest[i..].find('\n').map_or(rest.len(), |e| i + e) } else { rest[i..].find("*/").map_or(rest.len(), |e| i + e + 2) };
                    o.push_str(&format!("<span class=\"c\">{}</span>", escape(&rest[i..end])));
                    rest = &rest[end..];
                }
            }
        }
    };
    let mut prev_at = false;
    for t in &toks {
        let (s, e) = (t.span.start as usize, t.span.end as usize);
        if e <= s || s < pos || e > code.len() || !code.is_char_boundary(s) || !code.is_char_boundary(e) {
            continue;
        }
        gap(&mut o, &code[pos..s]);
        let text = escape(&code[s..e]);
        let class = match &t.kind {
            K::Str(_) | K::Char(_) => Some("s"),
            K::Int(_) | K::Float(_) => Some("n"),
            K::Ident(name) if prev_at => {
                let _ = name;
                Some("a")
            }
            // the contextual keyword of `for x in xs` (rarely a name in docs)
            K::Ident(name) if name == "in" => Some("k"),
            K::Ident(name) if name.starts_with(|c: char| c.is_ascii_uppercase()) => Some("t"),
            K::Ident(_) => None,
            K::Namespace | K::Import | K::As | K::Struct | K::Interface | K::Enum | K::Func | K::Static | K::Async | K::Await | K::Var | K::Const | K::If | K::Else | K::For | K::Switch | K::Case
            | K::Default | K::Return | K::Break | K::Continue | K::Defer | K::Try | K::Catch | K::New | K::Nil | K::True | K::False | K::Map | K::This => Some("k"),
            K::At => Some("a"),
            _ => None,
        };
        prev_at = matches!(t.kind, K::At);
        match class {
            Some(c) => o.push_str(&format!("<span class=\"{c}\">{text}</span>")),
            None => o.push_str(&text),
        }
        pos = e;
    }
    gap(&mut o, &code[pos.min(code.len())..]);
    o
}

/// Blocks to HTML (doc comments: headings are shifted below the
/// declaration's own heading).
pub fn to_html(blocks: &[Block], link: Resolver) -> String {
    blocks.iter().map(|b| render_block(b, link, &|u: &str| u.to_string(), 2)).collect()
}

/// Inline text to Markdown with resolved symbol links (`[`Split`](url)`).
fn inline_md(s: &str, link: Resolver) -> String {
    let mut o = String::new();
    for t in inlines(s) {
        match t {
            Inline::Text(t) => o.push_str(&t),
            Inline::Code(c) => match link(&c) {
                Some(url) => o.push_str(&format!("[`{c}`]({url})")),
                None => o.push_str(&format!("`{c}`")),
            },
            Inline::Strong(t) => o.push_str(&format!("**{}**", inline_md(&t, link))),
            Inline::Em(t) => o.push_str(&format!("*{}*", inline_md(&t, link))),
            Inline::Link(t, u) => o.push_str(&format!("[{t}]({u})")),
        }
    }
    o
}

/// Blocks to normalized CommonMark (fenced `zell` code, `Deprecated` as a
/// struck-through tag, symbol links): what the LSP hover shows.
pub fn to_markdown(blocks: &[Block], link: Resolver) -> String {
    let mut parts = Vec::new();
    for b in blocks {
        parts.push(match b {
            Block::Heading(n, t) => format!("{} {}", "#".repeat((*n as usize + 2).min(6)), inline_md(t, link)),
            Block::Para(t) => inline_md(t, link),
            Block::Deprecated(t) => format!("**~~Deprecated~~** {}", inline_md(t, link)),
            Block::List(ordered, items) => items
                .iter()
                .enumerate()
                .map(|(i, it)| if *ordered { format!("{}. {}", i + 1, inline_md(it, link)) } else { format!("- {}", inline_md(it, link)) })
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Code(lang, code) => format!("```{lang}\n{code}\n```"),
            Block::Table(h, rows) => {
                let mut t = format!("| {} |\n|{}|", h.join(" | "), vec!["---"; h.len()].join("|"));
                for r in rows {
                    t.push_str(&format!("\n| {} |", r.join(" | ")));
                }
                t
            }
            Block::Quote(q) => format!("> {}", inline_md(q, link)),
            Block::Rule => "---".to_string(),
        });
    }
    parts.join("\n\n")
}

/// Strips inline markup for terminal output.
fn inline_text(s: &str) -> String {
    let mut o = String::new();
    for t in inlines(s) {
        match t {
            Inline::Text(t) | Inline::Strong(t) | Inline::Em(t) => o.push_str(&t),
            Inline::Code(c) => o.push_str(&c),
            Inline::Link(t, u) if t == u => o.push_str(&u),
            Inline::Link(t, u) => o.push_str(&format!("{t} ({u})")),
        }
    }
    o
}

fn wrap(text: &str, width: usize, first: &str, rest: &str) -> String {
    let mut out = String::new();
    let mut line = first.to_string();
    let mut empty = true;
    for w in text.split_whitespace() {
        if !empty && line.chars().count() + 1 + w.chars().count() > width {
            out.push_str(line.trim_end());
            out.push('\n');
            line = rest.to_string();
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(w);
        empty = false;
    }
    out.push_str(line.trim_end());
    out.push('\n');
    out
}

/// Blocks as plain text, every line prefixed with `indent`, wrapped at 80
/// columns (code blocks are kept as written, indented four more).
pub fn to_text(blocks: &[Block], indent: &str) -> String {
    let mut parts = Vec::new();
    for b in blocks {
        parts.push(match b {
            Block::Heading(_, t) => format!("{indent}{}\n", inline_text(t).to_uppercase()),
            Block::Para(t) => wrap(&inline_text(t), 80, indent, indent),
            Block::Deprecated(t) => wrap(&format!("DEPRECATED: {}", inline_text(t)), 80, indent, indent),
            Block::List(ordered, items) => {
                let mut s = String::new();
                for (i, it) in items.iter().enumerate() {
                    let marker = if *ordered { format!("{}. ", i + 1) } else { "- ".to_string() };
                    let cont = " ".repeat(marker.len());
                    s.push_str(&wrap(&inline_text(it), 80, &format!("{indent}{marker}"), &format!("{indent}{cont}")));
                }
                s
            }
            Block::Code(_, code) => code.lines().map(|l| if l.is_empty() { "\n".to_string() } else { format!("{indent}    {l}\n") }).collect(),
            Block::Table(h, rows) => {
                let mut t = format!("{indent}{}\n", h.join(" | "));
                for r in rows {
                    t.push_str(&format!("{indent}{}\n", r.join(" | ")));
                }
                t
            }
            Block::Quote(q) => wrap(&inline_text(q), 80, &format!("{indent}> "), &format!("{indent}> ")),
            Block::Rule => format!("{indent}---\n"),
        });
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_and_summary() {
        let doc = "Split slices s. More text\nhere.\n\n  var x = 1\n  println(x)\n\n- one\n- two\n  continued\n\n# Notes\n\nDeprecated: use `Fields`.";
        let b = parse(doc);
        assert_eq!(b[0], Block::Para("Split slices s. More text here.".into()));
        assert_eq!(b[1], Block::Code("zell".into(), "var x = 1\nprintln(x)".into()));
        assert_eq!(b[2], Block::List(false, vec!["one".into(), "two continued".into()]));
        assert_eq!(b[3], Block::Heading(1, "Notes".into()));
        assert_eq!(b[4], Block::Deprecated("use `Fields`.".into()));
        assert_eq!(summary(doc), "Split slices s.");
        assert_eq!(summary("Uses e.g. this. Then"), "Uses e.g. this.");
        assert!(is_deprecated(doc).is_some());
    }

    #[test]
    fn html_and_markdown() {
        let link = |s: &str| (s == "Fields").then(|| "#Fields".to_string());
        let b = parse("Use `Fields` or `x`, **bold** and *em* <b>.\n\n```\nvar s = \"a\" // c\n```\n\nDeprecated: gone.");
        let h = to_html(&b, &link);
        assert!(h.contains("<a href=\"#Fields\"><code>Fields</code></a>"), "{h}");
        assert!(h.contains("<code>x</code>") && h.contains("<strong>bold</strong>") && h.contains("<em>em</em>") && h.contains("&lt;b&gt;"), "{h}");
        assert!(h.contains("<span class=\"k\">var</span>") && h.contains("<span class=\"c\">// c</span>"), "{h}");
        assert!(h.contains("class=\"deprecated\""), "{h}");
        let m = to_markdown(&b, &link);
        assert!(m.contains("[`Fields`](#Fields)") && m.contains("```zell\nvar s") && m.contains("**~~Deprecated~~** gone."), "{m}");
        let t = to_text(&b, "    ");
        assert!(t.contains("    Use Fields or x, bold and em <b>.") && t.contains("        var s"), "{t}");
    }
}
