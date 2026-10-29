//! `textDocument/documentLink`: each `import a.b` line links to the
//! namespace it names. A namespace can span many files and directories,
//! so the target is the file that declares it first (the location of the
//! namespace symbol: the first project file in load order; for std
//! namespaces, the first std source declaring one of its members). `import a.b.T` (a member import) links to `a.b`'s
//! file too. Imports that do not resolve get no link.

use crate::range;
use lsp_types::{DocumentLink, Url};
use zell::sema::index::SymbolKind;
use zell::session::Analysis;

/// The links of the imports of `file`.
pub(crate) fn document_links(a: &Analysis, file: usize, uri_of: impl Fn(usize) -> Option<Url>) -> Vec<DocumentLink> {
    let Some(ast) = a.asts.get(file) else { return Vec::new() };
    let f = &a.files[file];
    let mut out = Vec::new();
    for imp in &ast.imports {
        let segs: Vec<&str> = imp.path.segments.iter().map(|s| s.name.as_str()).collect();
        // The namespace itself, else the namespace of an imported member.
        let target = (1..=segs.len()).rev().take(2).find_map(|n| {
            let name = segs[..n].join(".");
            let ns = a.index.symbols.iter().find(|s| s.kind == SymbolKind::Namespace && &*s.full_name == name);
            // Std namespaces have no declaration of their own: the first
            // file declaring one of their members.
            let file = ns.and_then(|s| s.loc).map(|l| l.file).or_else(|| {
                let prefix = format!("{name}.");
                a.index.symbols.iter().filter(|s| s.full_name.strip_prefix(&prefix).is_some_and(|r| !r.contains('.'))).filter_map(|s| s.loc).map(|l| l.file).min()
            })?;
            Some((name, file))
        });
        let Some((name, file)) = target else { continue };
        let Some(uri) = uri_of(file) else { continue };
        out.push(DocumentLink { range: range(f, imp.path.span), target: Some(uri), tooltip: Some(format!("namespace {name}")), data: None });
    }
    out
}
