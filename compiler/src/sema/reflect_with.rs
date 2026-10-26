//! `reflect.TypesWith<A>()` and `reflect.Error(msg)` (DESIGN.md
//! "Рефлексия", "Типы с атрибутом и ошибки из библиотек").
//!
//! * `TypesWith<A>()` is an intrinsic template: its instantiations are
//!   collected in [`Sema::types_with_finish`] at the end of the analysis,
//!   when every declaration and generic instance of the whole program
//!   (all packages of a build) is known, and code generation emits each as
//!   a constant slice of the marked types' tables. The marked types become
//!   *full* types (methods, `ptrTo`); nothing else gets a table for it.
//! * `Error(msg)` is checked at its call site ([`Sema::reflect_error_call`]):
//!   a reachable call (not in a branch dropped by a static `if`, not in the
//!   abstract check of a generic body) with a constant message is an error.

use super::generics::Inst;
use super::*;
use crate::hir::{BinOp, ConstValue, ExprKind};

impl<'a> Sema<'a> {
    /// `reflect.TypesWith<A>()`: `A` must be an attribute type. Called for
    /// every generic call; only acts on the `TypesWith` template.
    pub(crate) fn types_with_check(&mut self, t: u32, targs: Option<&[ast::TypeExpr]>, span: Span) {
        if self.refl_template("TypesWith") != Some(t) {
            return;
        }
        let Some([a]) = targs else { return };
        self.gen.mute += 1;
        let aty = self.resolve_type(a);
        self.gen.mute -= 1;
        if self.tt.is_invalid(aty) || self.has_param(aty) {
            return;
        }
        let ok = match *self.tt.get(aty) {
            Type::Struct(d) => self.structs[d.0 as usize].is_attribute || self.is_attr_value_struct(d.0),
            _ => false,
        };
        if !ok {
            self.err(format!("reflect.TypesWith<A>() needs an attribute type (an @attribute struct), {} is not one", self.tn(aty)), span);
        }
    }

    /// The marked types of every `TypesWith<A>` instantiation, in package
    /// (by name), file (by path), declaration order. Called by
    /// `reflect_finish` before file indices of library sources are remapped.
    pub(crate) fn types_with_finish(&mut self, tmpl: Option<u32>) -> Vec<(TypeId, Vec<TypeId>)> {
        let Some(tmpl) = tmpl else { return Vec::new() };
        let mut wanted: Vec<TypeId> = self.gen.cache.iter().filter(|((t, _), i)| *t == tmpl && matches!(i, Inst::Func(_))).filter_map(|((_, args), _)| args.first().copied()).collect();
        wanted.sort();
        wanted.dedup();
        let mut out = Vec::new();
        for a in wanted {
            let Type::Struct(ad) = *self.tt.get(a) else {
                out.push((a, Vec::new()));
                continue;
            };
            out.push((a, self.marked_types(ad.0, true)));
        }
        out
    }

    /// The types of the whole program declared with attribute struct
    /// `asid`, in `TypesWith` order (package, file, declaration); generic
    /// instances only with `instances` (they are complete only at the end
    /// of the analysis).
    pub(crate) fn marked_types(&mut self, asid: u32, instances: bool) -> Vec<TypeId> {
        let marks = |s: &Self, attrs: &[hir::Attr]| {
            attrs.iter().any(|x| match &x.kind {
                hir::AttrKind::User(u) => *u == asid,
                hir::AttrKind::Builtin(n) => s.refl.struct_id(n) == Some(asid),
            })
        };
        // (package, path, file, offset, type)
        let mut found: Vec<(String, String, usize, u32, TypeId)> = Vec::new();
        for i in 0..self.structs.len() {
            let s = &self.structs[i];
            if !marks(self, &s.attrs) || self.is_template_struct(i as u32) || (!instances && self.gen.inst_structs.contains_key(&(i as u32))) {
                continue;
            }
            let (file, off) = (s.file, s.span.start);
            let t = self.tt.intern(Type::Struct(crate::types::DeclId(i as u32)));
            if self.has_param(t) {
                continue;
            }
            found.push((self.file_package(file), stdlib::source_path(file), file, off, t));
        }
        for i in 0..self.enums.len() {
            let e = &self.enums[i];
            if !marks(self, &e.attrs) || self.is_template_enum(i as u32) || (!instances && self.gen.inst_enums.contains_key(&(i as u32))) {
                continue;
            }
            let (file, off) = (e.file, e.span.start);
            let t = self.tt.intern(Type::Enum(crate::types::DeclId(i as u32)));
            if self.has_param(t) {
                continue;
            }
            found.push((self.file_package(file), stdlib::source_path(file), file, off, t));
        }
        for i in 0..self.interfaces.len() {
            let x = &self.interfaces[i];
            if !marks(self, &x.attrs) {
                continue;
            }
            let (file, off) = (x.file, x.span.start);
            let t = self.tt.intern(Type::Interface(crate::types::DeclId(i as u32)));
            if self.has_param(t) {
                continue;
            }
            found.push((self.file_package(file), stdlib::source_path(file), file, off, t));
        }
        // Instances of one template share the declaration's position:
        // keep them in instantiation (type id) order.
        found.sort_by(|x, y| (&x.0, &x.1, x.2, x.3, x.4).cmp(&(&y.0, &y.1, y.2, y.3, y.4)));
        found.into_iter().map(|f| f.4).collect()
    }

    /// Generic struct and enum declarations marked with attribute `asid`
    /// (by the attribute's name), which a compile-time `TypesWith` loop
    /// cannot list.
    pub(crate) fn marked_templates(&self, asid: u32) -> Vec<String> {
        let aname = self.structs[asid as usize].name.clone();
        let builtin = self.attr_value_struct_name(asid);
        let marks = |attrs: &[ast::Attribute]| attrs.iter().any(|a| a.path.segments.last().is_some_and(|s| s.name == aname || Some(s.name.as_str()) == builtin));
        self.gen
            .templates
            .iter()
            .filter(|t| match t.decl {
                super::generics::TmplDecl::Struct(d) => marks(&d.attrs),
                super::generics::TmplDecl::Enum(d) => marks(&d.attrs),
                _ => false,
            })
            .map(|t| t.name.clone())
            .collect()
    }

    /// The package name of `file` in a package build ("" otherwise; library
    /// sources of the toolchain sort first as "").
    fn file_package(&self, file: usize) -> String {
        match self.policy {
            Some(p) => p.file_package.get(file).and_then(|k| p.packages.get(*k)).map(|k| k.name.clone()).unwrap_or_default(),
            None => String::new(),
        }
    }

    /// A generic struct declaration itself (not an instance): it has no
    /// table of its own.
    fn is_template_struct(&self, sid: u32) -> bool {
        !self.gen.inst_structs.contains_key(&sid) && self.struct_src.get(sid as usize).is_some_and(|s| !s.generics.is_empty())
    }

    fn is_template_enum(&self, eid: u32) -> bool {
        !self.gen.inst_enums.contains_key(&eid) && self.enum_src.get(eid as usize).is_some_and(|e| !e.generics.is_empty())
    }

    fn refl_template(&self, name: &str) -> Option<u32> {
        if self.refl.loaded_reflect() {
            self.refl.template(name)
        } else {
            None
        }
    }

    /// `reflect.Error(msg)` at a reachable call site: reports `msg`, with
    /// the call sites that instantiated the enclosing generic. Returns
    /// true for a `reflect.Error` call (it is lowered as a terminating
    /// `panic`, so no `return` is needed after it).
    pub(crate) fn reflect_error_call(&mut self, f: FuncId, args: &[hir::Expr], span: Span) -> bool {
        if !self.refl.loaded_reflect() || self.refl.func("Error") != Some(f) {
            return false;
        }
        // The innermost named (non-lambda) function being checked.
        let owner = self.fns.iter().rev().find_map(|c| c.func.filter(|fid| !matches!(self.funcs[fid.0 as usize].kind, hir::FuncKind::Lambda { .. })));
        let abstract_ = owner.is_some_and(|fid| self.gen.insts.get(&fid.0).is_some_and(|i| i.note.is_none()));
        if abstract_ || self.gen.mute > 0 {
            return true;
        }
        let Some(msg) = args.first().and_then(|a| self.const_string(a)) else {
            if args.len() == 1 {
                self.err("reflect.Error needs a constant string message (literals, constants and their concatenation, f.Name, reflect.TypeOf<T>().Name, attribute fields)", span);
            }
            return true;
        };
        let by = match owner {
            Some(fid) => {
                let f = &self.funcs[fid.0 as usize];
                match f.kind {
                    hir::FuncKind::Method { struct_id, .. } => format!("{}.{}", self.structs[struct_id as usize].name, f.name),
                    _ => f.name.clone(),
                }
            }
            None => "a global initializer".to_string(),
        };
        let sites = owner.and_then(|fid| self.gen.insts.get(&fid.0)).map(|i| i.sites.clone()).unwrap_or_default();
        let mut d = Diagnostic::error(msg, span).with_note(format!("raised by {by}"), None).in_file(self.cur_file);
        d.related = sites.into_iter().map(|(fi, s)| ("instantiated here".to_string(), fi, s)).collect();
        // A method of a generic struct instance is checked with the
        // struct; it is only reachable if something uses it.
        if let Some(fid) = owner {
            if let hir::FuncKind::Method { struct_id, .. } = self.funcs[fid.0 as usize].kind {
                if self.gen.inst_structs.contains_key(&struct_id) {
                    self.refl.deferred.push((fid, d));
                    return true;
                }
            }
        }
        self.push_diag(d);
        true
    }

    /// Reports the deferred `reflect.Error`s of methods that are used: a
    /// call or a reference anywhere, or any method of a struct converted
    /// to an interface (reachable through its method table).
    pub(crate) fn reflect_flush_deferred(&mut self) {
        if self.refl.deferred.is_empty() {
            return;
        }
        let mut used: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut boxed: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for f in &self.funcs {
            crate::hir::ownership::walk_function_in(&self.tt, f, &mut |e, _| match &e.kind {
                ExprKind::FuncRef(id) | ExprKind::Call { callee: hir::Callee::Func(id), .. } | ExprKind::Closure { func: id, .. } => {
                    used.insert(id.0);
                }
                ExprKind::EnvClosures { funcs, .. } => used.extend(funcs.iter().map(|f| f.0)),
                ExprKind::ToInterface { value } => {
                    let t = match self.tt.get(value.ty) {
                        Type::Pointer(x) => *x,
                        _ => value.ty,
                    };
                    if let Type::Struct(d) = self.tt.get(t) {
                        boxed.insert(d.0);
                    }
                }
                _ => {}
            });
        }
        // A boxed struct's method is reachable through its method table only
        // if some interface declares a method of that name.
        let iface_names: std::collections::HashSet<&str> = self.interfaces.iter().flat_map(|i| i.methods.iter().map(|m| m.name.as_str())).collect();
        let mut deferred = std::mem::take(&mut self.refl.deferred);
        let hits: Vec<bool> = deferred
            .iter()
            .map(|(fid, _)| {
                let f = &self.funcs[fid.0 as usize];
                let in_boxed = matches!(f.kind, hir::FuncKind::Method { struct_id, .. } if boxed.contains(&struct_id)) && iface_names.contains(f.name.as_str());
                used.contains(&fid.0) || in_boxed
            })
            .collect();
        for (hit, (_, d)) in hits.into_iter().zip(deferred.drain(..)) {
            if hit {
                self.push_diag(d);
            }
        }
    }

    /// The value of a compile-time string expression, if `e` is one.
    pub(crate) fn const_string(&self, e: &hir::Expr) -> Option<String> {
        match &e.kind {
            ExprKind::Const(ConstValue::Str(s)) => Some(s.clone()),
            ExprKind::Binary { op: BinOp::StrConcat, lhs, rhs } => Some(self.const_string(lhs)? + &self.const_string(rhs)?),
            // reflect.TypeOf<T>().Name / FullName / Package
            ExprKind::Field { base, index } => {
                let call = match &base.kind {
                    ExprKind::Deref(b) => b,
                    _ => base,
                };
                let ExprKind::Call { callee: hir::Callee::Func(f), args } = &call.kind else { return None };
                if !args.is_empty() {
                    return None;
                }
                let tmpl = self.refl.template("TypeOf")?;
                let t = self.gen.cache.iter().find(|((x, _), i)| *x == tmpl && **i == Inst::Func(*f)).and_then(|((_, a), _)| a.first().copied())?;
                let ti = self.refl.struct_id("TypeInfo")?;
                let field = self.structs[ti as usize].fields.get(*index as usize)?;
                match field.name.as_str() {
                    "Name" => Some(self.tn_short(t)),
                    "FullName" => Some(self.tn_full(t)),
                    "Package" => Some(self.decl_package(t)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `TypeInfo.Name` of `t` as at run time: declared names without their
    /// namespace (`User`, `[]*Order`).
    pub(crate) fn tn_short(&self, t: TypeId) -> String {
        struct Short<'s, 'a>(&'s Sema<'a>);
        impl crate::hir::dump::DeclNames for Short<'_, '_> {
            fn struct_name(&self, id: u32) -> String {
                self.0.structs[id as usize].name.clone()
            }
            fn interface_name(&self, id: u32) -> String {
                self.0.interfaces[id as usize].name.clone()
            }
            fn enum_name(&self, id: u32) -> String {
                self.0.enums[id as usize].name.clone()
            }
        }
        crate::hir::dump::type_name(&self.tt, t, &Short(self))
    }

    /// Namespace path of a named type's declaration ("" for others).
    pub(crate) fn decl_package(&self, t: TypeId) -> String {
        let ns = match *self.tt.get(t) {
            Type::Struct(d) => self.structs[d.0 as usize].namespace,
            Type::Enum(d) => self.enums[d.0 as usize].namespace,
            Type::Interface(d) => self.interfaces[d.0 as usize].namespace,
            _ => return String::new(),
        };
        self.nss.iter().find(|n| n.hir_id == Some(ns)).map(|n| n.path.clone()).unwrap_or_default()
    }
}
