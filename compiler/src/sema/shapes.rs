//! Shapes of structs, enums and interfaces for the type hierarchy: method
//! sets with id-free signatures and struct embeddings, recorded per run
//! (`Recorder::shapes`) and related over the whole program by
//! `session::typegraph`.
//!
//! A signature is spelled with full declaration names, so shapes of
//! different runs compare as text. A generic struct or enum is recorded
//! once, by the abstract instance its template check makes: its type
//! parameters are spelled `$0`, `$1`, ... (by position), and satisfaction
//! unifies them with the interface's types.

use super::Sema;
use crate::hir::{self, dump::DeclNames, FuncKind};
use crate::sema::index::Location;
use crate::span::Span;
use crate::types::TypeId;

/// What a shape describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    Struct,
    Enum,
    Interface,
}

/// A method of a shape.
#[derive(Clone, Debug, PartialEq)]
pub struct MethodShape {
    pub name: String,
    /// `(params) ret`, `async ` first for an async method.
    pub sig: String,
    /// An interface method with a default body (need not be implemented).
    pub default: bool,
    /// The method's declaration (not for interface methods).
    pub loc: Option<Location>,
}

/// A struct, enum or interface declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeShape {
    /// Full name without type arguments (`app.Box`).
    pub name: String,
    pub kind: ShapeKind,
    /// Namespace path.
    pub ns: String,
    /// A sealed interface.
    pub sealed: bool,
    pub is_std: bool,
    /// Declaration (file of the run, then of the fragment, then of the
    /// analysis); std shapes get theirs from the std sources.
    pub loc: Option<Location>,
    /// Own non-static methods (an interface: all, embedded ones included).
    pub methods: Vec<MethodShape>,
    /// Embedded structs: declaration name and type arguments.
    pub embeds: Vec<(String, Vec<String>)>,
}

/// Type names for signatures: full names, type parameters of the shape by
/// position.
struct ShapeNames<'s, 'a> {
    s: &'s Sema<'a>,
    params: &'s [u32],
}

impl DeclNames for ShapeNames<'_, '_> {
    fn struct_name(&self, id: u32) -> String {
        self.s.structs.get(id as usize).map(|s| s.full_name.clone()).unwrap_or_default()
    }
    fn interface_name(&self, id: u32) -> String {
        self.s.interfaces.get(id as usize).map(|s| s.full_name.clone()).unwrap_or_default()
    }
    fn enum_name(&self, id: u32) -> String {
        self.s.enums.get(id as usize).map(|s| s.full_name.clone()).unwrap_or_default()
    }
    fn param_name(&self, id: u32) -> String {
        match self.params.iter().position(|p| *p == id) {
            Some(i) => format!("${i}"),
            None => "?".into(),
        }
    }
}

fn ns_of(full: &str) -> String {
    full.rsplit_once('.').map(|(ns, _)| ns.to_string()).unwrap_or_default()
}

impl Sema<'_> {
    fn shape_ty(&self, t: TypeId, params: &[u32]) -> String {
        hir::dump::type_name(&self.tt, t, &ShapeNames { s: self, params })
    }

    fn shape_sig(&self, params: &[TypeId], ret: TypeId, is_async: bool, tps: &[u32]) -> String {
        let ps: Vec<String> = params.iter().map(|p| self.shape_ty(*p, tps)).collect();
        format!("{}({}) {}", if is_async { "async " } else { "" }, ps.join(", "), self.shape_ty(ret, tps))
    }

    /// The non-static methods `ms` (of a struct or enum) as shapes.
    fn method_shapes(&self, ms: &[(String, hir::FuncId)], tps: &[u32], file: usize) -> Vec<MethodShape> {
        let mut out = Vec::new();
        for (name, f) in ms {
            let Some(func) = self.funcs.get(f.0 as usize) else { continue };
            if matches!(func.kind, FuncKind::Method { is_static: true, .. }) || self.enum_method_static(f.0) {
                continue;
            }
            let sig = &self.sigs[f.0 as usize];
            let loc = match self.func_src.get(f.0 as usize) {
                Some(super::FuncSrc::Method(d, _)) | Some(super::FuncSrc::Free(d)) if d.name.span != Span::DUMMY => Some(Location { file, name_span: d.name.span, full_span: d.span }),
                _ => None,
            };
            out.push(MethodShape { name: name.clone(), sig: self.shape_sig(&sig.params, sig.ret, sig.is_async, tps), default: false, loc });
        }
        out
    }

    /// Enum methods are free functions; a static one has no `this`.
    fn enum_method_static(&self, f: u32) -> bool {
        self.enum_method_owner.contains_key(&f) && self.funcs[f as usize].params.first().and_then(|l| self.funcs[f as usize].locals.get(l.0 as usize)).is_none_or(|l| l.kind != hir::LocalKind::This)
    }

    fn struct_shape(&self, sid: u32, tps: &[u32], file: usize) -> TypeShape {
        let st = &self.structs[sid as usize];
        let src = self.struct_src[sid as usize];
        let name = super::index::strip_type_args(&st.full_name);
        let embeds = st
            .fields
            .iter()
            .filter(|f| f.embedded)
            .filter_map(|f| match self.tt.get(f.ty) {
                crate::types::Type::Struct(d) => {
                    let args = self.gen.inst_structs.get(&d.0).map(|(_, a)| a.iter().map(|t| self.shape_ty(*t, tps)).collect()).unwrap_or_default();
                    Some((super::index::strip_type_args(&self.structs[d.0 as usize].full_name), args))
                }
                _ => None,
            })
            .collect();
        TypeShape {
            ns: ns_of(&name),
            is_std: super::stdlib::is_std_namespace(&name),
            name,
            kind: ShapeKind::Struct,
            sealed: false,
            loc: (src.name.span != Span::DUMMY).then_some(Location { file, name_span: src.name.span, full_span: src.span }),
            methods: self.method_shapes(&st.methods, tps, file),
            embeds,
        }
    }

    fn enum_shape(&self, eid: u32, tps: &[u32], file: usize) -> TypeShape {
        let e = &self.enums[eid as usize];
        let src = self.enum_src[eid as usize];
        let name = super::index::strip_type_args(&e.full_name);
        TypeShape {
            ns: ns_of(&name),
            is_std: super::stdlib::is_std_namespace(&name),
            name,
            kind: ShapeKind::Enum,
            sealed: false,
            loc: (src.name.span != Span::DUMMY).then_some(Location { file, name_span: src.name.span, full_span: src.span }),
            methods: self.method_shapes(&e.methods, tps, file),
            embeds: Vec::new(),
        }
    }

    fn iface_shape(&self, iid: u32) -> TypeShape {
        let it = &self.interfaces[iid as usize];
        let src = self.iface_src[iid as usize];
        let methods = it.methods.iter().map(|m| MethodShape { name: m.name.clone(), sig: self.shape_sig(&m.params, m.ret, false, &[]), default: m.default.is_some(), loc: None }).collect();
        TypeShape {
            ns: ns_of(&it.full_name),
            is_std: super::stdlib::is_std_namespace(&it.full_name),
            name: it.full_name.clone(),
            kind: ShapeKind::Interface,
            sealed: it.sealed,
            loc: (src.name.span != Span::DUMMY).then_some(Location { file: it.file, name_span: src.name.span, full_span: src.span }),
            methods,
            embeds: Vec::new(),
        }
    }

    /// Records the shape of the abstract instance of generic struct or
    /// enum template `t` (during its template check).
    pub(crate) fn ide_template_shape(&mut self, t: u32, s: Option<u32>, e: Option<u32>) {
        if self.ide.is_none() {
            return;
        }
        let tm = &self.gen.templates[t as usize];
        let (params, file) = (tm.params.clone(), tm.file);
        let shape = match (s, e) {
            (Some(sid), _) => self.struct_shape(sid, &params, file),
            (_, Some(eid)) => self.enum_shape(eid, &params, file),
            _ => return,
        };
        if !shape.is_std {
            self.ide.as_mut().unwrap().shapes.push(shape);
        }
    }

    /// Records the shapes of the non-generic structs, enums and interfaces:
    /// the user ones, or (`std`) every std one, with lazily resolved std
    /// interfaces resolved first.
    pub(crate) fn ide_collect_shapes(&mut self, std: bool) {
        if self.ide.is_none() {
            return;
        }
        if std {
            let mut lazy: Vec<u32> = self.std_iface_lazy.keys().copied().collect();
            lazy.sort();
            for i in lazy {
                self.force_iface(i);
            }
        }
        let mut out = Vec::new();
        for sid in 0..self.structs.len() as u32 {
            let st = &self.structs[sid as usize];
            if st.is_attribute || self.gen.inst_structs.contains_key(&sid) || st.span == Span::DUMMY || super::stdlib::is_std_namespace(&st.full_name) != std {
                continue;
            }
            out.push(self.struct_shape(sid, &[], st.file));
        }
        for eid in 0..self.enums.len() as u32 {
            let e = &self.enums[eid as usize];
            if self.gen.inst_enums.contains_key(&eid) || e.span == Span::DUMMY || super::stdlib::is_std_namespace(&e.full_name) != std {
                continue;
            }
            out.push(self.enum_shape(eid, &[], e.file));
        }
        for iid in 0..self.interfaces.len() as u32 {
            let it = &self.interfaces[iid as usize];
            if super::stdlib::is_std_namespace(&it.full_name) != std || (!std && it.span == Span::DUMMY) || self.std_iface_lazy.contains_key(&iid) {
                continue;
            }
            out.push(self.iface_shape(iid));
        }
        if std {
            for s in &mut out {
                s.loc = None;
                s.methods.iter_mut().for_each(|m| m.loc = None);
            }
        }
        self.ide.as_mut().unwrap().shapes.extend(out);
    }
}
