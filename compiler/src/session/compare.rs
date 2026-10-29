//! A canonical, id-free listing of an [`Analysis`], for comparing an
//! incremental analysis with a full one (tests and tools).

use super::Analysis;
use crate::sema::index::{ImportInfo, SymKey, Symbol, SymbolId};
use crate::types::{Type, TypeId};

impl Analysis {
    /// Every diagnostic, user symbol (with its type), reference, file fact
    /// and user namespace member list as sorted text lines without ids.
    #[doc(hidden)]
    pub fn canonical(&self) -> Vec<String> {
        let mut out = Vec::new();
        let path = |f: usize| self.files.get(f).map_or("?", |f| f.path.as_str());
        let n_user = self.asts.len();
        for d in &self.diagnostics {
            let fixes: Vec<String> = d.fixes.iter().map(|f| format!("{}{:?}", f.title, f.edits.iter().map(|e| (e.span.start, e.span.end, e.new_text.clone())).collect::<Vec<_>>())).collect();
            out.push(format!("diag {} {}..{} {:?} {:?} {} notes={:?} fixes={:?}", path(d.file), d.span.start, d.span.end, d.severity, d.code, d.message, d.notes, fixes));
        }
        for s in &self.index.symbols {
            let Some(l) = s.loc else { continue };
            if l.file >= n_user {
                continue;
            }
            let container = s.container.map(|c| self.sym_id(c)).unwrap_or_default();
            let ty = s.ty.map(|t| self.type_text(t, 0)).unwrap_or_default();
            out.push(format!(
                "sym {} kind={:?} name={} full={} sig={:?} doc={:?} in={} static={} async={} ro={} std={} ty={}",
                self.sym_id(s.id),
                s.kind,
                s.name,
                s.full_name,
                s.signature,
                s.doc,
                container,
                s.is_static,
                s.is_async,
                s.is_readonly,
                s.is_std,
                ty
            ));
        }
        for r in &self.index.references {
            if r.file < n_user {
                out.push(format!("ref {} {}..{} -> {}", path(r.file), r.span.start, r.span.end, self.sym_id(r.symbol)));
            }
        }
        for (i, f) in self.index.files.iter().enumerate().take(n_user) {
            let Some(f) = f else {
                out.push(format!("file {} none", path(i)));
                continue;
            };
            let imports: Vec<String> = f
                .imports
                .iter()
                .map(|imp| match imp {
                    ImportInfo::Glob(p) => format!("glob {p}"),
                    ImportInfo::Named(n, id) => format!("named {n} {}", self.sym_id(*id)),
                })
                .collect();
            out.push(format!("file {} ns={} {:?} {:?} imports={imports:?}", path(i), f.namespace, f.namespace_span, f.span));
        }
        for (ns, members) in &self.index.ns_members {
            if crate::sema::stdlib::is_std_namespace(ns) {
                continue;
            }
            let mut ms: Vec<String> = members.iter().map(|(n, id)| format!("{n}={}", self.sym_id(*id))).collect();
            ms.sort();
            out.push(format!("ns {ns} {ms:?}"));
        }
        let g = &self.types;
        for (t, sh) in g.types.iter().enumerate() {
            if sh.loc.is_none_or(|l| l.file >= n_user) {
                continue;
            }
            let names = |ids: &[u32]| ids.iter().map(|&i| g.types[i as usize].name.clone()).collect::<Vec<_>>();
            let ms: Vec<String> = sh.methods.iter().map(|m| format!("{}{}{}", m.name, m.sig, if m.default { " default" } else { "" })).collect();
            out.push(format!("type {} {:?} at={:?} methods={ms:?} embeds={:?} supers={:?} subs={:?}", sh.name, sh.kind, sh.loc.map(|l| (path(l.file), l.name_span.start)), sh.embeds, names(g.supertypes(t as u32)), names(g.subtypes(t as u32))));
        }
        out.push(format!("ok {}", self.ok));
        out.sort();
        out
    }

    /// Identity of a symbol: where it is declared, or its key.
    fn sym_id(&self, id: SymbolId) -> String {
        let s: &Symbol = self.index.symbol(id);
        match s.loc {
            Some(l) => format!("{:?}:{}@{}:{}-{}", s.kind, s.full_name, self.files.get(l.file).map_or("?", |f| f.path.as_str()), l.name_span.start, l.name_span.end),
            None => match &s.key {
                SymKey::Ns(p) => format!("ns:{p}"),
                SymKey::Std(p) => format!("std:{p}"),
                SymKey::Builtin(b) => format!("builtin:{b}"),
                _ => format!("{:?}:{}", s.kind, s.full_name),
            },
        }
    }

    /// A type spelled with full declaration names (type parameters
    /// without their ids).
    fn type_text(&self, t: TypeId, depth: usize) -> String {
        let p = &self.hir;
        if depth > 12 {
            return "...".into();
        }
        let n = |t: TypeId| self.type_text(t, depth + 1);
        match p.types.get(t) {
            Type::Pointer(x) => format!("*{}", n(*x)),
            Type::Weak(x) => format!("weak *{}", n(*x)),
            Type::Slice(x) => format!("[]{}", n(*x)),
            Type::Array(x, k) => format!("[{k}]{}", n(*x)),
            Type::Map(k, v) => format!("map[{}]{}", n(*k), n(*v)),
            Type::Tuple(ts) => format!("({})", ts.iter().map(|t| n(*t)).collect::<Vec<_>>().join(", ")),
            Type::Fallible(x) => format!("{}!", n(*x)),
            Type::Task(x) => format!("Task<{}>", n(*x)),
            Type::Func(sig) => format!("func({}) {}", sig.params.iter().map(|t| n(*t)).collect::<Vec<_>>().join(", "), n(sig.ret)),
            Type::Struct(d) => {
                let s = &p.structs[d.0 as usize];
                let fields: Vec<String> = s.fields.iter().map(|f| format!("{}:{}", f.name, if depth < 2 { n(f.ty) } else { String::new() })).collect();
                format!("{}{{{}}}", s.full_name, fields.join(","))
            }
            Type::Interface(d) => {
                let s = &p.interfaces[d.0 as usize];
                let ms: Vec<&str> = s.methods.iter().map(|m| m.name.as_str()).collect();
                format!("{}{{{}}}", s.full_name, ms.join(","))
            }
            Type::Enum(d) => p.enums[d.0 as usize].full_name.clone(),
            Type::Param(_) => "T?".into(),
            other => format!("{other:?}"),
        }
    }
}
