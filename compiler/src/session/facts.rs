//! Per-file facts for incremental analysis, computed once per text: the
//! hash of the text, the hash of what other files can observe of it (its
//! *export signature*), its namespace, imports and identifiers.

use crate::ast;
use crate::span::Span;
use crate::token::{Token, TokenKind};
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

/// What the unit graph and the cache keys need of one file.
#[derive(Clone, Debug, Default)]
pub(crate) struct FileFacts {
    /// Hash of the whole text.
    pub text_hash: u64,
    /// Hash of everything a file analysed together with this one (as a
    /// declaration-only file) can depend on: the tokens outside the bodies
    /// of non-generic functions, with the positions of generic code (its
    /// diagnostics are reported at those positions).
    pub sig_hash: u64,
    /// `namespace` path.
    pub namespace: String,
    /// Paths of the `import` lines (after `@cfg`).
    pub imports: Vec<String>,
    /// Hashes of the identifiers of the file, sorted (a root namespace
    /// named by one is reachable without an import); see [`FileFacts::has_ident`].
    idents: Vec<u64>,
    /// Number of lines (a cost estimate).
    pub lines: usize,
    /// Hash of the namespace, imports and identifiers (what the units and
    /// their dependencies are computed from).
    pub deps_hash: u64,
}

impl FileFacts {
    /// The file may use identifier `name` (outside its `namespace` and
    /// `import` lines). Hash collisions only make it say yes.
    pub fn has_ident(&self, name: &str) -> bool {
        self.idents.binary_search(&hash_str(name)).is_ok()
    }
}

pub(crate) fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Facts of a file from its tokens and (cfg-applied) AST.
pub(crate) fn facts(text: &str, tokens: &[Token], file: &ast::File) -> FileFacts {
    let mut skip: Vec<Span> = Vec::new();
    let mut generic: Vec<Span> = Vec::new();
    let body = |f: &ast::FuncDecl, in_generic: bool, skip: &mut Vec<Span>, generic: &mut Vec<Span>| {
        // `Configure` bodies are read by data (the fluent model).
        if in_generic || !f.generics.is_empty() {
            generic.push(f.span);
        } else if let Some(b) = &f.body {
            if f.name.name != "Configure" {
                skip.push(b.span);
            }
        }
    };
    for d in &file.decls {
        match d {
            ast::Decl::Func(f) => body(f, false, &mut skip, &mut generic),
            ast::Decl::Struct(s) => {
                let g = !s.generics.is_empty();
                if g {
                    generic.push(s.span);
                }
                for m in &s.methods {
                    body(m, g, &mut skip, &mut generic);
                }
            }
            ast::Decl::Enum(e) => {
                let g = !e.generics.is_empty();
                if g {
                    generic.push(e.span);
                }
                for m in &e.methods {
                    body(m, g, &mut skip, &mut generic);
                }
            }
            ast::Decl::Interface(i) => {
                for m in &i.methods {
                    body(m, false, &mut skip, &mut generic);
                }
            }
            _ => {}
        }
    }
    skip.sort_by_key(|s| s.start);
    let inside = |spans: &[Span], at: u32| spans.iter().any(|s| s.start <= at && at < s.end);
    let mut h = DefaultHasher::new();
    let mut idents: HashSet<&str> = HashSet::new();
    let mut si = 0;
    // Identifiers of the `namespace` and `import` lines name no root.
    let mut header: Vec<Span> = vec![file.namespace.span];
    header.extend(file.imports.iter().map(|i| i.span));
    for t in tokens {
        if let TokenKind::Ident(name) = &t.kind {
            if !idents.contains(name.as_str()) && !inside(&header, t.span.start) {
                idents.insert(name.as_str());
            }
        }
        while si < skip.len() && skip[si].end <= t.span.start {
            si += 1;
        }
        if si < skip.len() && skip[si].start <= t.span.start && t.span.start < skip[si].end {
            continue;
        }
        token_hash(&t.kind, &mut h);
        if inside(&generic, t.span.start) {
            t.span.start.hash(&mut h);
        }
    }
    // How the parser grouped the tokens (error recovery can turn the same
    // tokens outside bodies into different declarations).
    for d in &file.decls {
        decl_shape(d, &mut h);
    }
    let namespace = file.namespace.to_string();
    let imports: Vec<String> = file.imports.iter().map(|i| i.path.to_string()).collect();
    let mut idents: Vec<u64> = idents.into_iter().map(hash_str).collect();
    idents.sort_unstable();
    let mut d = DefaultHasher::new();
    (&namespace, &imports, &idents).hash(&mut d);
    FileFacts { text_hash: hash_str(text), sig_hash: h.finish(), namespace, imports, idents, lines: text.lines().count(), deps_hash: d.finish() }
}

/// Hashes the structure of a declaration (names, flags and types, not
/// positions or bodies).
fn decl_shape(d: &ast::Decl, h: &mut DefaultHasher) {
    fn ty(t: &ast::TypeExpr, h: &mut DefaultHasher) {
        use ast::TypeKind as K;
        std::mem::discriminant(&t.kind).hash(h);
        match &t.kind {
            K::Named { path, generics } => {
                path.to_string().hash(h);
                generics.iter().for_each(|g| ty(g, h));
            }
            K::Fallible(x) | K::Pointer(x) | K::Weak(x) | K::Slice(x) | K::Variadic(x) | K::ArrayExpr(x, _) => ty(x, h),
            K::Array(x, n) => {
                ty(x, h);
                n.hash(h);
            }
            K::Map(k, v) => {
                ty(k, h);
                ty(v, h);
            }
            K::Func { params, ret } => {
                params.iter().for_each(|p| ty(p, h));
                ret.as_ref().map(|r| ty(r, h)).hash(h);
            }
            K::Tuple(ts) => ts.iter().for_each(|x| ty(x, h)),
            K::Error | K::Infer => {}
        }
    }
    fn attrs(a: &[ast::Attribute], h: &mut DefaultHasher) {
        a.len().hash(h);
        for x in a {
            x.path.to_string().hash(h);
            x.args.len().hash(h);
        }
    }
    fn generics(g: &[ast::GenericParam], h: &mut DefaultHasher) {
        g.len().hash(h);
        for p in g {
            p.name.name.hash(h);
            p.bound.as_ref().map(|b| ty(b, h)).hash(h);
        }
    }
    fn func(f: &ast::FuncDecl, h: &mut DefaultHasher) {
        f.name.name.hash(h);
        (f.is_async, f.is_static, f.is_extern, f.body.is_some(), f.ret.is_some()).hash(h);
        attrs(&f.attrs, h);
        generics(&f.generics, h);
        f.params.len().hash(h);
        for p in &f.params {
            p.name.name.hash(h);
            ty(&p.ty, h);
            attrs(&p.attrs, h);
        }
        if let Some(r) = &f.ret {
            ty(r, h);
        }
    }
    fn fields(fs: &[ast::Field], h: &mut DefaultHasher) {
        fs.len().hash(h);
        for f in fs {
            f.name.name.hash(h);
            ty(&f.ty, h);
            f.default.is_some().hash(h);
            attrs(&f.attrs, h);
        }
    }
    std::mem::discriminant(d).hash(h);
    match d {
        ast::Decl::Struct(s) => {
            s.name.name.hash(h);
            attrs(&s.attrs, h);
            generics(&s.generics, h);
            fields(&s.fields, h);
            s.embeds.iter().for_each(|e| ty(e, h));
            s.implements.iter().for_each(|e| ty(e, h));
            s.methods.len().hash(h);
            s.methods.iter().for_each(|m| func(m, h));
        }
        ast::Decl::Interface(i) => {
            i.name.name.hash(h);
            (i.is_sealed, i.methods.len(), i.embeds.len()).hash(h);
            attrs(&i.attrs, h);
            generics(&i.generics, h);
            i.methods.iter().for_each(|m| func(m, h));
            i.embeds.iter().for_each(|e| ty(e, h));
        }
        ast::Decl::Enum(e) => {
            e.name.name.hash(h);
            attrs(&e.attrs, h);
            generics(&e.generics, h);
            e.variants.len().hash(h);
            for v in &e.variants {
                v.name.name.hash(h);
                attrs(&v.attrs, h);
                fields(&v.fields, h);
            }
            e.implements.iter().for_each(|x| ty(x, h));
            e.methods.len().hash(h);
            e.methods.iter().for_each(|m| func(m, h));
        }
        ast::Decl::Func(f) => func(f, h),
        ast::Decl::Var(v) => {
            v.names.iter().for_each(|n| n.name.hash(h));
            v.ty.as_ref().map(|t| ty(t, h)).hash(h);
            v.init.is_some().hash(h);
            attrs(&v.attrs, h);
        }
        ast::Decl::Const(c) => {
            c.name.name.hash(h);
            c.ty.as_ref().map(|t| ty(t, h)).hash(h);
            attrs(&c.attrs, h);
        }
        ast::Decl::TypeAlias(a) => {
            a.name.name.hash(h);
            ty(&a.ty, h);
            attrs(&a.attrs, h);
        }
        ast::Decl::Error(_) => {}
    }
}

fn token_hash(k: &TokenKind, h: &mut DefaultHasher) {
    std::mem::discriminant(k).hash(h);
    match k {
        TokenKind::Ident(s) | TokenKind::Int(s) | TokenKind::Float(s) | TokenKind::Str(s) => s.hash(h),
        TokenKind::Char(c) => c.hash(h),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_of(src: &str) -> FileFacts {
        let mut d = crate::diagnostics::Diagnostics::new();
        let toks = crate::lexer::lex(src, &mut d);
        let file = crate::parser::parse_file(toks.clone(), &mut d);
        facts(src, &toks, &file)
    }

    #[test]
    fn signature_ignores_bodies_of_plain_functions() {
        let a = facts_of("namespace a\n\nimport b.c\n\nfunc G<T>(x T) T {\n    return x\n}\n\nfunc F() int {\n    return 1\n}\n");
        let body = facts_of("namespace a\n\nimport b.c\n\nfunc G<T>(x T) T {\n    return x\n}\n\nfunc F() int {\n    var y = 2\n    return y\n}\n");
        assert_eq!(a.sig_hash, body.sig_hash);
        assert_ne!(a.text_hash, body.text_hash);
        let sig = facts_of("namespace a\n\nimport b.c\n\nfunc G<T>(x T) T {\n    return x\n}\n\nfunc F() string {\n    return 1\n}\n");
        assert_ne!(a.sig_hash, sig.sig_hash);
        // Generic code is compared with its positions.
        let moved = facts_of("namespace a\n\nimport b.c\n\n\nfunc G<T>(x T) T {\n    return x\n}\n\nfunc F() int {\n    return 1\n}\n");
        assert_ne!(a.sig_hash, moved.sig_hash);
        assert_eq!(a.namespace, "a");
        assert_eq!(a.imports, ["b.c"]);
        assert!(a.has_ident("x") && !a.has_ident("namespace_of_nobody"));
    }

}
