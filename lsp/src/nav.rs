//! Navigation on top of an [`Analysis`]: `textDocument/implementation`
//! (interfaces to the structs and enums that implement them, interface
//! methods to their implementations), `textDocument/typeDefinition` and
//! `textDocument/selectionRange`.
//!
//! Interfaces are structural (DESIGN.md): implementations come from the
//! satisfaction table of the analysis ([`Analysis::types`], built from the
//! method sets of every struct, enum and interface, std included), which
//! the type hierarchy (`typehier.rs`) shares. Generic types are reported
//! once, at their declaration.

use crate::assist::Walker;
use zell::ast;
use zell::diagnostics::Diagnostics;
use zell::hir::Program;
use zell::sema::index::{SymKey, SymbolKind};
use zell::session::Analysis;
use zell::span::Span;
use zell::types::{Type, TypeId};

/// A declaration's place: file index and the span of its name.
pub(crate) type Target = (usize, Span);

/// `task.Channel<int>` -> `task.Channel`.
fn strip_type_args(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0;
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Where the declaration with HIR `file`/`span` named `name` is: the index
/// symbol for `key` when it has a location, else the AST declaration whose
/// span is `span` (user code, generic instances included), else an index
/// symbol with the same full name (std sources).
fn decl_target(a: &Analysis, key: Option<SymKey>, full_name: &str, file: usize, span: Span) -> Option<Target> {
    if let Some(l) = key.and_then(|k| a.index.lookup(&k)).and_then(|id| a.index.symbol(id).loc) {
        return Some((l.file, l.name_span));
    }
    let plain = strip_type_args(full_name);
    let short = plain.rsplit('.').next().unwrap_or(&plain).to_string();
    if !zell::sema::stdlib::is_std_namespace(&plain) {
        if let Some(name) = a.asts.get(file).and_then(|f| ast_name_at(f, span)) {
            if name.name == short {
                return Some((file, name.span));
            }
        }
    }
    a.index.symbols.iter().find(|s| *s.full_name == *plain && s.loc.is_some() && s.kind != SymbolKind::Local).and_then(|s| s.loc).map(|l| (l.file, l.name_span))
}

/// The name of the declaration (or member) of `f` whose span is `span`.
fn ast_name_at(f: &ast::File, span: Span) -> Option<&ast::Ident> {
    fn funcs(ms: &[ast::FuncDecl], span: Span) -> Option<&ast::Ident> {
        ms.iter().find(|m| m.span == span).map(|m| &m.name)
    }
    for d in &f.decls {
        let hit = match d {
            ast::Decl::Struct(s) => (s.span == span).then_some(&s.name).or_else(|| funcs(&s.methods, span)),
            ast::Decl::Enum(e) => (e.span == span).then_some(&e.name).or_else(|| funcs(&e.methods, span)),
            ast::Decl::Interface(i) => (i.span == span).then_some(&i.name).or_else(|| funcs(&i.methods, span)),
            ast::Decl::Func(func) => (func.span == span).then_some(&func.name),
            _ => None,
        };
        if hit.is_some() {
            return hit;
        }
    }
    None
}

/// The program a lookup works in: [`Analysis::hir`] (types only, the ids
/// of the symbol index) or the full program with functions
/// ([`Analysis::program_partial`], its own ids).
fn in_hir(a: &Analysis, p: &Program) -> bool {
    std::ptr::eq(p, &a.hir)
}

fn struct_target(a: &Analysis, p: &Program, sid: u32) -> Option<Target> {
    let st = p.structs.get(sid as usize)?;
    decl_target(a, in_hir(a, p).then_some(SymKey::Struct(sid)), &st.full_name, st.file, st.span)
}

fn enum_target(a: &Analysis, p: &Program, eid: u32) -> Option<Target> {
    let e = p.enums.get(eid as usize)?;
    decl_target(a, in_hir(a, p).then_some(SymKey::Enum(eid)), &e.full_name, e.file, e.span)
}

fn iface_target(a: &Analysis, p: &Program, iid: u32) -> Option<Target> {
    let i = p.interfaces.get(iid as usize)?;
    decl_target(a, in_hir(a, p).then_some(SymKey::Interface(iid)), &i.full_name, i.file, i.span)
}

/// Where a shape's declaration (or a method's) is.
fn shape_target(l: Option<zell::sema::index::Location>) -> Option<Target> {
    l.map(|l| (l.file, l.name_span))
}

fn push_unique(out: &mut Vec<Target>, t: Option<Target>) {
    if let Some(t) = t {
        if !out.contains(&t) {
            out.push(t);
        }
    }
}

/// `textDocument/implementation` at `off`: for an interface, the structs
/// and enums satisfying it; for an interface method, the implementing
/// methods (types that rely on a default body are skipped; promoted
/// methods count, at their declaration).
pub(crate) fn implementations(a: &Analysis, file: usize, off: u32) -> Vec<Target> {
    let Some(id) = a.symbol_at(file, off) else { return Vec::new() };
    let s = a.index.symbol(id);
    let g = &a.types;
    let mut out = Vec::new();
    let (iface, method) = match s.key {
        SymKey::Interface(_) => (strip_type_args(&s.full_name), None),
        SymKey::IfaceMethod(i, _) => match a.hir.interfaces.get(i as usize) {
            Some(it) => (strip_type_args(&it.full_name), Some(s.name.to_string())),
            None => return out,
        },
        _ => return out,
    };
    let Some(t) = g.find(&iface) else { return out };
    for c in g.implementors(t) {
        match &method {
            None => push_unique(&mut out, shape_target(g.types[c as usize].loc)),
            Some(m) => push_unique(&mut out, g.methods(c).into_iter().find(|x| x.name == *m).and_then(|x| shape_target(x.loc))),
        }
    }
    out.sort_by_key(|t| (t.0, t.1.start, t.1.end));
    out
}

/// The declared type behind `t` of `p`: pointers, `weak`, slices, arrays,
/// fallible results, tasks and map values are looked through.
fn named_type(a: &Analysis, p: &Program, mut t: TypeId) -> Option<Target> {
    for _ in 0..16 {
        t = match p.types.get(t) {
            Type::Pointer(x) | Type::Weak(x) | Type::Slice(x) | Type::Array(x, _) | Type::Fallible(x) | Type::Task(x) | Type::Map(_, x) => *x,
            Type::Struct(d) => return struct_target(a, p, d.0),
            Type::Enum(d) => return enum_target(a, p, d.0),
            Type::Interface(d) => return iface_target(a, p, d.0),
            _ => return None,
        };
    }
    None
}

/// `textDocument/typeDefinition` at `off`: the declaration of the type of
/// the variable, parameter, field or constant there (the result type for
/// a function; the type itself for a type name; for an instance of a
/// generic type, the generic declaration).
pub(crate) fn type_definition(a: &Analysis, file: usize, off: u32) -> Option<Target> {
    let id = a.symbol_at(file, off)?;
    let s = a.index.symbol(id);
    match s.key {
        SymKey::Struct(_) | SymKey::Enum(_) | SymKey::Interface(_) => s.loc.map(|l| (l.file, l.name_span)),
        SymKey::Variant(e, _) => enum_target(a, &a.hir, e),
        SymKey::Func(_) => {
            // Functions are not in `a.hir`: the result type comes from the
            // full program, found by name.
            let p = a.program_partial();
            let full = strip_type_args(&s.full_name);
            let func = p.funcs.iter().find(|f| strip_type_args(&f.full_name) == full)?;
            named_type(a, p, func.ret)
        }
        SymKey::IfaceMethod(i, m) => named_type(a, &a.hir, a.hir.interfaces.get(i as usize)?.methods.get(m as usize)?.ret),
        _ => named_type(a, &a.hir, s.ty?),
    }
}

// ---------------------------------------------------------------------------
// Selection ranges
// ---------------------------------------------------------------------------

/// The spans of the syntax nodes containing `off`, innermost first: the
/// token, expressions, statements, blocks, the function, the declaration
/// and the whole file.
pub(crate) fn selection_chain(f: &ast::File, text: &str, off: u32) -> Vec<Span> {
    let inside = |s: Span| s.start <= off && off <= s.end && s.end > s.start;
    let mut spans: Vec<Span> = vec![Span::new(0, text.len() as u32)];
    if inside(f.span) {
        spans.push(f.span);
    }
    let push = |s: Span, spans: &mut Vec<Span>| {
        if inside(s) {
            spans.push(s);
        }
    };
    let mut funcs: Vec<&ast::FuncDecl> = Vec::new();
    let mut exprs: Vec<&ast::Expr> = Vec::new();
    for d in &f.decls {
        let ds = d.span();
        if !inside(ds) {
            continue;
        }
        push(ds, &mut spans);
        match d {
            ast::Decl::Struct(s) => {
                s.fields.iter().for_each(|x| push(x.span, &mut spans));
                funcs.extend(s.methods.iter());
                exprs.extend(s.fields.iter().filter_map(|x| x.default.as_ref()));
            }
            ast::Decl::Enum(e) => {
                e.variants.iter().for_each(|v| push(v.span, &mut spans));
                funcs.extend(e.methods.iter());
            }
            ast::Decl::Interface(i) => funcs.extend(i.methods.iter()),
            ast::Decl::Func(func) => funcs.push(func),
            ast::Decl::Var(v) => exprs.extend(v.init.iter()),
            ast::Decl::Const(c) => exprs.push(&c.value),
            ast::Decl::TypeAlias(_) | ast::Decl::Error(_) => {}
        }
    }
    for i in &f.imports {
        push(i.span, &mut spans);
    }
    let nodes = std::cell::RefCell::new(Vec::new());
    {
        let add = |s: Span| {
            if inside(s) {
                nodes.borrow_mut().push(s)
            }
        };
        let mut w = Walker { on_stmt: &mut |s| add(s.span), on_expr: &mut |e| add(e.span), on_block: &mut |b| add(b.span) };
        for func in funcs.iter().filter(|x| inside(x.span)) {
            w.func(func);
        }
        for e in exprs.iter().filter(|x| inside(x.span)) {
            w.expr(e);
        }
    }
    spans.extend(nodes.into_inner());
    for func in funcs.iter().filter(|x| inside(x.span)) {
        push(func.span, &mut spans);
        func.params.iter().for_each(|p| push(p.span, &mut spans));
        if let Some(r) = &func.ret {
            push(r.span, &mut spans);
        }
    }
    let mut diags = Diagnostics::new();
    let tokens = zell::lexer::lex(text, &mut diags);
    let token = tokens.iter().find(|t| t.span.start <= off && off < t.span.end).or_else(|| tokens.iter().find(|t| t.span.end == off && t.span.end > t.span.start));
    if let Some(t) = token {
        spans.push(t.span);
    }
    // Outermost first, then keep a properly nested chain.
    spans.sort_by_key(|s| (std::cmp::Reverse(s.end - s.start), s.start));
    let mut chain: Vec<Span> = Vec::new();
    for s in spans {
        match chain.last() {
            Some(l) if *l == s || !(l.start <= s.start && s.end <= l.end) => {}
            _ => chain.push(s),
        }
    }
    chain.reverse();
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_arguments_are_stripped() {
        assert_eq!(strip_type_args("task.Channel<int>.Send"), "task.Channel.Send");
        assert_eq!(strip_type_args("app.Pair<map[string]int, Box<T>>"), "app.Pair");
    }

    #[test]
    fn selection_chain_is_nested() {
        let text = "namespace app\n\nfunc main() {\n    var x = 1 + 2 * 3\n}\n";
        let mut d = Diagnostics::new();
        let f = zell::parser::parse_file(zell::lexer::lex(text, &mut d), &mut d);
        let off = text.find("2 *").unwrap() as u32;
        let chain = selection_chain(&f, text, off);
        let texts: Vec<&str> = chain.iter().map(|s| &text[s.start as usize..s.end as usize]).collect();
        assert_eq!(texts[0], "2");
        assert_eq!(texts[1], "2 * 3");
        assert_eq!(texts[2], "1 + 2 * 3");
        assert!(texts.contains(&"var x = 1 + 2 * 3"), "{texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("func main()")), "{texts:?}");
        assert_eq!(*texts.last().unwrap(), text);
        for w in chain.windows(2) {
            assert!(w[1].start <= w[0].start && w[0].end <= w[1].end && w[0] != w[1], "{chain:?}");
        }
    }
}
