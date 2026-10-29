//! Change signature (`refactor.rewrite`): reorder, remove, rename and add
//! parameters of a function or method, updating its declaration and every
//! call across the workspace.
//!
//! The parameter-free code actions, on the name of a function (its
//! declaration or a call): "Change signature: add parameter" (`param int`,
//! calls pass `0`; before a variadic parameter), "remove parameter x",
//! "move parameter x left/right". The command `zell.changeSignature`
//! takes the full new list (see [`Spec`]) for clients that build a UI;
//! renaming goes through it too.
//!
//! A method is changed together with every declaration it must keep
//! matching: the methods of the interfaces its type satisfies that have
//! its name, and every implementation of those (an interface method: the
//! interface and every implementation). Calls are rewritten argument by
//! argument (comments inside an argument list are not kept); a new
//! parameter's argument is its default expression, written as given.
//!
//! Refused: a function used as a value (`f`, `x.M` not called: the
//! function type would change under it), a declaration or call in a
//! read-only file, removing a parameter its body uses, removing an
//! argument with side effects (it would no longer run; bind it to a
//! variable first), moving arguments with side effects past each other
//! (their order would change), a variadic parameter not kept last, names
//! that clash with another parameter or a name in the body.

use crate::calls::{self, WsEdits};
use crate::refactor::{is_word, slice};
use std::collections::HashMap;
use zell::ast::{self, ExprKind, TypeKind};
use zell::sema::index::{strip_type_args, SymKey, SymbolId, SymbolKind as K};
use zell::sema::shapes::ShapeKind;
use zell::session::Analysis;
use zell::span::Span;

/// One parameter of the new signature: an existing one (`from`, its old
/// index; `name` renames it) or a new one (`name`, `ty` and the argument
/// `default` passed at every call).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Param {
    pub from: Option<usize>,
    pub name: Option<String>,
    pub ty: Option<String>,
    pub default: Option<String>,
}

/// The new parameter list.
pub(crate) type Spec = Vec<Param>;

/// Parses `[{"from": 0, "name": "n"}, {"name": "x", "type": "int",
/// "default": "0"}]`.
pub(crate) fn parse_spec(v: &serde_json::Value) -> Result<Spec, String> {
    let items = v.as_array().ok_or("`params` must be an array")?;
    items
        .iter()
        .map(|p| {
            let s = |k: &str| p.get(k).and_then(|x| x.as_str()).map(str::to_string);
            Ok(Param { from: p.get("from").and_then(|x| x.as_u64()).map(|x| x as usize), name: s("name"), ty: s("type"), default: s("default") })
        })
        .collect()
}

/// The function at `off` whose signature can change: a user function,
/// method or interface method, at its declaration or a use.
fn target(a: &Analysis, file: usize, off: u32) -> Option<SymbolId> {
    let id = a.index.declaration_at(file, off).or_else(|| a.index.reference_at(file, off).map(|r| r.symbol))?;
    let s = a.index.symbol(id);
    (matches!(s.key, SymKey::Func(_) | SymKey::IfaceMethod(..)) && s.loc.is_some() && !s.is_std).then_some(id)
}

/// The code action ids at `sel`: `sig|add`, `sig|remove|i`, `sig|left|i`,
/// `sig|right|i`.
pub(crate) fn candidates(a: &Analysis, file: usize, sel: Span) -> Vec<String> {
    let Some(id) = target(a, file, sel.start) else { return Vec::new() };
    let Some(l) = a.index.symbol(id).loc else { return Vec::new() };
    let Some(decl) = calls::find_func(&a.asts[l.file], l.name_span) else { return Vec::new() };
    let n = decl.params.len();
    let mut out = vec!["sig|add".to_string()];
    for i in 0..n {
        out.push(format!("sig|remove|{i}"));
        if i > 0 {
            out.push(format!("sig|left|{i}"));
        }
        if i + 1 < n {
            out.push(format!("sig|right|{i}"));
        }
    }
    out
}

/// The title and result of code action `id` (see [`candidates`]).
pub(crate) fn compute(a: &Analysis, file: usize, id: &str, sel: Span) -> Option<(String, Result<WsEdits, String>)> {
    let sym = target(a, file, sel.start)?;
    let l = a.index.symbol(sym).loc?;
    let decl = calls::find_func(&a.asts[l.file], l.name_span)?;
    let n = decl.params.len();
    let keep = |i: usize| Param { from: Some(i), ..Default::default() };
    let mut spec: Spec = (0..n).map(keep).collect();
    let parts: Vec<&str> = id.split('|').collect();
    let pname = |i: usize| decl.params.get(i).map(|p| p.name.name.clone());
    let title = match parts.as_slice() {
        ["sig", "add"] => {
            let name = fresh_param(a, sym);
            let at = if decl.params.last().is_some_and(|p| matches!(p.ty.kind, TypeKind::Variadic(_))) { n - 1 } else { n };
            spec.insert(at, Param { name: Some(name), ty: Some("int".into()), default: Some("0".into()), from: None });
            "Change signature: add parameter".to_string()
        }
        ["sig", op, i] => {
            let i: usize = i.parse().ok()?;
            let name = pname(i)?;
            match *op {
                "remove" => {
                    spec.remove(i);
                    format!("Change signature: remove parameter {name}")
                }
                "left" if i > 0 => {
                    spec.swap(i - 1, i);
                    format!("Change signature: move parameter {name} left")
                }
                "right" if i + 1 < n => {
                    spec.swap(i, i + 1);
                    format!("Change signature: move parameter {name} right")
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some((title, change(a, sym, &spec)))
}

/// `zell.changeSignature`: the edits for the function at `off` of `file`.
pub(crate) fn command(a: &Analysis, file: usize, off: u32, spec: &Spec) -> Result<WsEdits, String> {
    let sym = target(a, file, off).ok_or("no function or method here")?;
    change(a, sym, spec)
}

/// A parameter name used by no declaration of the family of `sym`.
fn fresh_param(a: &Analysis, sym: SymbolId) -> String {
    let texts: Vec<String> = family(a, sym)
        .unwrap_or_else(|_| vec![sym])
        .into_iter()
        .filter_map(|s| a.index.symbol(s).loc)
        .map(|l| slice(&a.files[l.file].text, l.full_span).to_string())
        .collect();
    let mut n = "param".to_string();
    let mut i = 2;
    while texts.iter().any(|t| is_word(t, &n)) {
        n = format!("param{i}");
        i += 1;
    }
    n
}

/// The symbol declared at `l`.
fn symbol_at_loc(a: &Analysis, file: usize, name: Span) -> Option<SymbolId> {
    a.index.symbols.iter().find(|s| s.loc.is_some_and(|l| l.file == file && l.name_span == name)).map(|s| s.id)
}

/// `sym` and every declaration that must keep its signature: interface
/// methods of that name of the interfaces its type satisfies and all
/// implementations of those.
fn family(a: &Analysis, sym: SymbolId) -> Result<Vec<SymbolId>, String> {
    let g = &a.types;
    let mut out = vec![sym];
    let mut i = 0;
    while i < out.len() {
        let s = a.index.symbol(out[i]);
        i += 1;
        let name = s.name.to_string();
        let Some(c) = s.container.map(|c| a.index.symbol(c)) else { continue };
        let Some(t) = g.find(&strip_type_args(&c.full_name)) else { continue };
        let mut found: Vec<SymbolId> = Vec::new();
        match s.key {
            SymKey::IfaceMethod(..) => {
                for imp in g.implementors(t) {
                    if let Some(l) = g.methods(imp).into_iter().find(|m| m.name == name).and_then(|m| m.loc) {
                        found.extend(symbol_at_loc(a, l.file, l.name_span));
                    }
                }
            }
            SymKey::Func(_) if s.kind == K::Method => {
                for &sup in g.supertypes(t) {
                    let st = &g.types[sup as usize];
                    if st.kind != ShapeKind::Interface || !st.methods.iter().any(|m| m.name == name) {
                        continue;
                    }
                    let iface = a.index.symbols.iter().find(|x| matches!(x.key, SymKey::Interface(_)) && strip_type_args(&x.full_name) == st.name);
                    if let Some(iface) = iface {
                        found.extend(a.index.symbols.iter().filter(|x| x.container == Some(iface.id) && *x.name == *name && matches!(x.key, SymKey::IfaceMethod(..))).map(|x| x.id));
                    }
                }
            }
            _ => {}
        }
        for f in found {
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
    for &s in &out {
        let sy = a.index.symbol(s);
        match sy.loc {
            None => return Err(format!("`{}` must keep the signature of the library declaration `{}`", a.index.symbol(sym).name, sy.full_name)),
            Some(l) if a.is_read_only(l.file) => return Err(format!("`{}` is declared in a read-only file ({})", sy.full_name, calls::place(a, l.file, l.name_span.start))),
            _ => {}
        }
    }
    Ok(out)
}

/// Per file: plain edits and call argument lists (the list's span and
/// its pieces: an argument's source span or new text).
#[derive(Default)]
struct FileWork {
    plain: Vec<(Span, String)>,
    lists: Vec<(Span, Vec<Piece>)>,
}

enum Piece {
    Src(Span),
    Text(String),
}

/// The edits giving `sym` (and its family) the parameters `spec`.
fn change(a: &Analysis, sym: SymbolId, spec: &Spec) -> Result<WsEdits, String> {
    let fam = family(a, sym)?;
    let l0 = a.index.symbol(sym).loc.ok_or("no declaration")?;
    let d0 = calls::find_func(&a.asts[l0.file], l0.name_span).ok_or("no declaration")?;
    let n = d0.params.len();
    let variadic = d0.params.last().is_some_and(|p| matches!(p.ty.kind, TypeKind::Variadic(_)));
    // Validate the spec.
    let mut seen = vec![false; n];
    for (k, p) in spec.iter().enumerate() {
        match p.from {
            Some(i) if i >= n => return Err(format!("there is no parameter {i}")),
            Some(i) if seen[i] => return Err(format!("parameter {i} is listed twice")),
            Some(i) => {
                seen[i] = true;
                if variadic && i == n - 1 && k + 1 != spec.len() {
                    return Err(format!("the variadic parameter `{}` must stay last", d0.params[i].name.name));
                }
            }
            None => {
                if p.name.is_none() || p.ty.is_none() || p.default.is_none() {
                    return Err("a new parameter needs `name`, `type` and `default`".into());
                }
            }
        }
        if let Some(name) = &p.name {
            if !crate::features::is_identifier(name) {
                return Err(format!("`{name}` is not a valid Zell identifier"));
            }
        }
    }
    let unchanged = spec.len() == n && spec.iter().enumerate().all(|(k, p)| p.from == Some(k) && p.name.as_ref().is_none_or(|x| *x == d0.params[k].name.name));
    if unchanged {
        return Err("the signature is unchanged".into());
    }
    let mut work: HashMap<usize, FileWork> = HashMap::new();
    for &s in &fam {
        let sy = a.index.symbol(s);
        let l = sy.loc.ok_or("no declaration")?;
        let decl = calls::find_func(&a.asts[l.file], l.name_span).ok_or("no declaration")?;
        if decl.params.len() != n {
            return Err(format!("`{}` has a different number of parameters", sy.full_name));
        }
        declaration(a, l.file, decl, spec, work.entry(l.file).or_default())?;
        calls_of(a, s, spec, n, variadic, &mut work)?;
    }
    let mut out = WsEdits::new();
    let mut files: Vec<usize> = work.keys().copied().collect();
    files.sort();
    for f in files {
        let w = work.remove(&f).unwrap_or_default();
        let edits = assemble(&a.files[f].text, w);
        out.push((f, crate::refactor::formatted(&a.files[f].text, edits)));
    }
    Ok(out)
}

/// The declaration's new parameter list and the renames in its body.
fn declaration(a: &Analysis, file: usize, decl: &ast::FuncDecl, spec: &Spec, w: &mut FileWork) -> Result<(), String> {
    let text = a.files[file].text.as_str();
    let fname = &decl.name.name;
    let body = decl.body.as_ref().map(|b| b.span);
    let body_text = body.map_or("", |b| slice(text, b));
    let refs_of = |p: &ast::Param| -> Vec<Span> {
        symbol_at_loc(a, file, p.name.span).map(|id| a.references_of(id).into_iter().filter(|r| r.file == file).map(|r| r.span).collect()).unwrap_or_default()
    };
    // Removed parameters must be unused.
    for (i, p) in decl.params.iter().enumerate() {
        if !spec.iter().any(|x| x.from == Some(i)) {
            if let Some(r) = refs_of(p).first() {
                return Err(format!("parameter `{}` is used in `{fname}` ({})", p.name.name, calls::place(a, file, r.start)));
            }
        }
    }
    let mut parts = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for x in spec {
        match x.from {
            Some(i) => {
                let p = &decl.params[i];
                let new = x.name.clone().unwrap_or_else(|| p.name.name.clone());
                if new != p.name.name {
                    if is_word(body_text, &new) {
                        return Err(format!("`{new}` is already used in `{fname}`"));
                    }
                    w.plain.extend(refs_of(p).into_iter().map(|r| (r, new.clone())));
                    parts.push(format!("{}{new}{}", &text[p.span.start as usize..p.name.span.start as usize], &text[p.name.span.end as usize..p.span.end as usize]));
                } else {
                    parts.push(slice(text, p.span).to_string());
                }
                names.push(new);
            }
            None => {
                let name = x.name.clone().unwrap_or_default();
                if is_word(body_text, &name) {
                    return Err(format!("`{name}` is already used in `{fname}`"));
                }
                parts.push(format!("{name} {}", x.ty.clone().unwrap_or_default()));
                names.push(name);
            }
        }
    }
    for (k, n) in names.iter().enumerate() {
        if names[..k].contains(n) {
            return Err(format!("two parameters of `{fname}` would be named `{n}`"));
        }
    }
    let list = calls::inside_parens(text, decl.name.span.end).ok_or("no parameter list")?;
    w.plain.push((list, parts.join(", ")));
    Ok(())
}

/// The new argument lists of every call of `sym`.
fn calls_of(a: &Analysis, sym: SymbolId, spec: &Spec, n: usize, variadic: bool, work: &mut HashMap<usize, FileWork>) -> Result<(), String> {
    let name = a.index.symbol(sym).name.to_string();
    let fixed = if variadic { n - 1 } else { n };
    let mut exprs_of: HashMap<usize, Vec<&ast::Expr>> = HashMap::new();
    for r in a.references_of(sym) {
        let exprs = exprs_of.entry(r.file).or_insert_with(|| calls::exprs(&a.asts[r.file]));
        let at = calls::place(a, r.file, r.span.start);
        let Some(call) = calls::call_named(exprs, r.span) else {
            return Err(format!("`{name}` is used as a function value ({at}): its type would change"));
        };
        if a.is_read_only(r.file) {
            return Err(format!("`{name}` is called in a read-only file ({at})"));
        }
        let ExprKind::Call { callee, args } = &call.kind else { continue };
        if args.len() < fixed || (!variadic && args.len() != n) {
            return Err(format!("the call of `{name}` ({at}) does not match its parameters"));
        }
        let text = a.files[r.file].text.as_str();
        // Argument indices (variadic ones share the last parameter's).
        let param_of = |k: usize| k.min(n.saturating_sub(1));
        let mut pieces = Vec::new();
        let mut order = Vec::new();
        for x in spec {
            match x.from {
                Some(i) if variadic && i == n - 1 => {
                    for (k, arg) in args.iter().enumerate().skip(fixed) {
                        pieces.push(Piece::Src(arg.span));
                        order.push(k);
                    }
                }
                Some(i) => {
                    pieces.push(Piece::Src(args[i].span));
                    order.push(i);
                }
                None => pieces.push(Piece::Text(x.default.clone().unwrap_or_default())),
            }
        }
        for (k, arg) in args.iter().enumerate() {
            if !spec.iter().any(|x| x.from == Some(param_of(k))) && !calls::is_pure(arg) {
                return Err(format!("the argument `{}` of the call at {at} has side effects and would no longer be evaluated", slice(text, arg.span)));
            }
        }
        let effects: Vec<usize> = order.iter().copied().filter(|&k| !calls::is_pure(&args[k])).collect();
        if effects.windows(2).any(|w| w[0] > w[1]) {
            return Err(format!("the call at {at} would evaluate its arguments with side effects in another order"));
        }
        let list = calls::inside_parens(text, callee.span.end).ok_or("no argument list")?;
        work.entry(r.file).or_default().lists.push((list, pieces));
    }
    Ok(())
}

/// The edits of one file: argument lists rendered innermost first, each
/// argument with the edits inside it applied; edits inside a rewritten
/// list are folded into it.
fn assemble(text: &str, w: FileWork) -> Vec<(Span, String)> {
    let mut lists = w.lists;
    lists.sort_by_key(|(s, _)| (s.start, s.end));
    lists.dedup_by_key(|(s, _)| *s);
    lists.sort_by_key(|(s, _)| s.end - s.start);
    let mut done: Vec<(Span, String)> = w.plain;
    for (span, pieces) in lists {
        let args: Vec<String> = pieces
            .iter()
            .map(|p| match p {
                Piece::Src(s) => render(text, *s, &done),
                Piece::Text(t) => t.clone(),
            })
            .collect();
        done.retain(|(s, _)| !(span.start <= s.start && s.end <= span.end));
        done.push((span, args.join(", ")));
    }
    done
}

/// `text[span]` with the edits inside it applied.
fn render(text: &str, span: Span, edits: &[(Span, String)]) -> String {
    let mut inner: Vec<&(Span, String)> = edits.iter().filter(|(s, _)| span.start <= s.start && s.end <= span.end).collect();
    inner.sort_by_key(|(s, _)| s.start);
    let mut out = String::new();
    let mut at = span.start as usize;
    for (s, t) in inner {
        if (s.start as usize) < at {
            continue;
        }
        out.push_str(&text[at..s.start as usize]);
        out.push_str(t);
        at = s.end as usize;
    }
    out.push_str(&text[at..span.end as usize]);
    out
}
