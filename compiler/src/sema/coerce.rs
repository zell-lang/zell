//! Assignability, implicit conversions and structural interface checks.

use super::*;
use crate::hir::{ExprKind, MethodSig};

/// Where a value is being converted, for diagnostics.
#[derive(Clone, Debug)]
pub(crate) enum Ctx {
    Assign,
    Init,
    Arg(usize, String),
    Return,
    Field(String),
    Elem,
    Other,
}

impl<'a> Sema<'a> {
    /// Method set required by an interface-like type.
    pub(crate) fn iface_methods(&self, t: TypeId) -> Vec<MethodSig> {
        match self.tt.get(t) {
            Type::Interface(d) => self.interfaces[d.0 as usize].methods.clone(),
            Type::Error => vec![MethodSig { name: "Message".into(), params: Vec::new(), ret: self.tt.string, default: None }],
            _ => Vec::new(),
        }
    }

    /// Struct id of `S` or `*S`.
    pub(crate) fn struct_of(&self, t: TypeId) -> Option<u32> {
        match self.tt.get(t) {
            Type::Struct(d) => Some(d.0),
            Type::Pointer(p) => match self.tt.get(*p) {
                Type::Struct(d) => Some(d.0),
                _ => None,
            },
            _ => None,
        }
    }

    /// Types whose methods can implement interfaces: `S`, `*S` and enums.
    pub(crate) fn has_methods(&self, t: TypeId) -> bool {
        self.struct_of(t).is_some() || matches!(self.tt.get(t), Type::Enum(_))
    }

    /// Checks that `src` (a struct or pointer to struct) has every method of
    /// interface `iface`; returns the problems found.
    pub(crate) fn missing_methods(&self, src: TypeId, iface: TypeId) -> Vec<String> {
        if let Type::Enum(e) = *self.tt.get(src) {
            return self.enum_missing_methods(e.0, iface);
        }
        let Some(sid) = self.struct_of(src) else {
            return vec!["(not a struct)".into()];
        };
        let mut problems = Vec::new();
        if let Some(p) = self.sealed_problem(self.structs[sid as usize].namespace, &self.structs[sid as usize].full_name, iface) {
            problems.push(p);
        }
        for m in self.iface_methods(iface) {
            match self.find_method(sid, &m.name) {
                None if m.default.is_some() => {}
                None => problems.push(format!("missing method {}", m.name)),
                Some((f, _)) => {
                    let sig = &self.sigs[f.0 as usize];
                    if !self.sig_satisfies(sig, &m) {
                        problems.push(self.method_mismatch(&m, sig));
                    }
                }
            }
        }
        problems
    }

    /// Method signature `sig` implements interface method `m`: the same
    /// parameters and result, or an `async` method whose result `R` is the
    /// `Task<R>` that `m` returns (DESIGN.md "Интерфейсы": the vtable slot
    /// returns the call's task).
    pub(crate) fn sig_satisfies(&self, sig: &Sig, m: &MethodSig) -> bool {
        if sig.params != m.params {
            return false;
        }
        if sig.is_async {
            return matches!(self.tt.get(m.ret), Type::Task(inner) if *inner == sig.ret);
        }
        sig.ret == m.ret
    }

    /// The problem text for a method whose signature does not match the
    /// interface method `m`.
    fn method_mismatch(&self, m: &MethodSig, sig: &Sig) -> String {
        let have = self.sig_string(&sig.params, sig.ret);
        let want = self.sig_string(&m.params, m.ret);
        if sig.is_async {
            let hint = if sig.params == m.params && !matches!(self.tt.get(m.ret), Type::Task(_)) {
                format!(": an async method implements an interface method returning Task<{}>", self.tn(sig.ret))
            } else {
                String::new()
            };
            return format!("method {} has type async {have}, want {want}{hint}", m.name);
        }
        format!("method {} has type {have}, want {want}", m.name)
    }

    /// [`missing_methods`](Self::missing_methods) for enum `eid` (its
    /// methods take the enum value as `this`).
    fn enum_missing_methods(&self, eid: u32, iface: TypeId) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(p) = self.sealed_problem(self.enums[eid as usize].namespace, &self.enums[eid as usize].full_name, iface) {
            problems.push(p);
        }
        for m in self.iface_methods(iface) {
            match self.enum_methods.get(&(eid, m.name.clone())) {
                None if m.default.is_some() => {}
                None => problems.push(format!("missing method {}", m.name)),
                Some(f) => {
                    let sig = &self.sigs[f.0 as usize];
                    if !self.sig_satisfies(sig, &m) {
                        problems.push(self.method_mismatch(&m, sig));
                    }
                }
            }
        }
        problems
    }

    /// For a sealed interface `iface` and a type `full_name` declared in
    /// namespace `ns`: the reason it cannot implement it, if `ns` is another
    /// namespace. (Stdlib types share HIR namespace id 0 with a user
    /// namespace, hence the name check.)
    pub(crate) fn sealed_problem(&self, ns: hir::NamespaceId, full_name: &str, iface: TypeId) -> Option<String> {
        let Type::Interface(d) = self.tt.get(iface) else { return None };
        let it = &self.interfaces[d.0 as usize];
        if !it.sealed || (it.namespace == ns && crate::sema::stdlib::is_std_namespace(&full_name) == crate::sema::stdlib::is_std_namespace(&it.full_name)) {
            return None;
        }
        let path = self.nss[self.ns_of_hir(it.namespace)].path.clone();
        Some(format!("sealed interface {} can only be implemented in namespace {path}", self.tn(iface)))
    }

    /// Interface `iface` is sealed.
    pub(crate) fn is_sealed(&self, iface: TypeId) -> bool {
        matches!(self.tt.get(iface), Type::Interface(d) if self.interfaces[d.0 as usize].sealed)
    }

    /// `func(a, b) r` spelling of a signature.
    pub(crate) fn sig_string(&self, params: &[TypeId], ret: TypeId) -> String {
        let ps: Vec<String> = params.iter().map(|p| self.tn(*p)).collect();
        if matches!(self.tt.get(ret), Type::Void) {
            format!("func({})", ps.join(", "))
        } else {
            format!("func({}) {}", ps.join(", "), self.tn(ret))
        }
    }

    /// True if `src` can be stored in interface-like `target`.
    pub(crate) fn implements(&self, src: TypeId, target: TypeId) -> bool {
        if matches!(self.tt.get(target), Type::Any) {
            return true;
        }
        (self.has_methods(src) && self.missing_methods(src, target).is_empty()) || self.param_implements(src, target) || self.iface_subset(src, target)
    }

    /// Interface `src` has every method of interface `target` (same
    /// signatures): its values convert implicitly (decision 18).
    pub(crate) fn iface_subset(&self, src: TypeId, target: TypeId) -> bool {
        if src == target || !matches!(self.tt.get(src), Type::Interface(_)) || !matches!(self.tt.get(target), Type::Interface(_)) {
            return false;
        }
        // Values of a sealed interface come only from its namespace: only a
        // sealed interface of the same namespace converts to it statically.
        if self.is_sealed(target) {
            let (Type::Interface(a), Type::Interface(b)) = (self.tt.get(src), self.tt.get(target)) else { return false };
            let (a, b) = (&self.interfaces[a.0 as usize], &self.interfaces[b.0 as usize]);
            if !a.sealed || a.namespace != b.namespace {
                return false;
            }
        }
        let have = self.iface_methods(src);
        self.iface_methods(target)
            .iter()
            .all(|m| have.iter().any(|h| h.name == m.name && h.params == m.params && h.ret == m.ret) || (m.default.is_some() && !have.iter().any(|h| h.name == m.name)))
    }

    /// Converts `e` to `target` if assignable (identity, typed `nil`,
    /// interface boxing), else reports a mismatch.
    pub(crate) fn coerce(&mut self, e: hir::Expr, target: TypeId, ctx: Ctx) -> hir::Expr {
        let src = e.ty;
        if src == target || self.tt.is_invalid(src) || self.tt.is_invalid(target) {
            return e;
        }
        if matches!(self.tt.get(src), Type::Nil) {
            if self.tt.is_nilable(target) {
                return mk(ExprKind::Nil, target, e.span);
            }
            let t = self.tn(target);
            self.err(format!("cannot use nil as {t}: {t} is not nilable (only pointers, slices, maps, functions and interfaces can be nil)"), e.span);
            return mk(ExprKind::Nil, self.tt.invalid, e.span);
        }
        if let (Type::Pointer(a), Type::Weak(b)) = (self.tt.get(src).clone(), self.tt.get(target).clone()) {
            if a == b {
                let span = e.span;
                return mk(ExprKind::Convert { kind: crate::hir::ConvKind::PtrToWeak, value: Box::new(e) }, target, span);
            }
        }
        if self.tt.is_interface(target) {
            let any = matches!(self.tt.get(target), Type::Any);
            if any || (self.has_methods(src) && self.missing_methods(src, target).is_empty()) || self.param_implements(src, target) || self.iface_subset(src, target) {
                if self.tt.is_fallible(src) || matches!(self.tt.get(src), Type::Void) {
                    self.mismatch(src, target, &ctx, e.span);
                    return e;
                }
                let span = e.span;
                return mk(ExprKind::ToInterface { value: Box::new(e) }, target, span);
            }
            if self.has_methods(src) {
                let problems = self.missing_methods(src, target);
                let (s, t) = (self.tn(src), self.tn(target));
                self.err(format!("{s} does not implement {t}: {}", problems.join(", ")), e.span);
                return mk(e.kind, self.tt.invalid, e.span);
            }
        }
        self.mismatch(src, target, &ctx, e.span);
        mk(e.kind, self.tt.invalid, e.span)
    }

    fn mismatch(&mut self, src: TypeId, target: TypeId, ctx: &Ctx, span: Span) {
        let (s, t) = (self.tn(src), self.tn(target));
        let msg = match ctx {
            Ctx::Assign => format!("cannot assign {s} to {t}"),
            Ctx::Init => format!("cannot assign {s} to variable of type {t}"),
            Ctx::Arg(i, f) => format!("cannot use {s} as {t} in argument {} of {f}", i + 1),
            Ctx::Return => format!("cannot return {s} from a function returning {t}"),
            Ctx::Field(name) => format!("cannot use {s} as {t} for field {name}"),
            Ctx::Elem => format!("cannot use {s} as element of type {t}"),
            Ctx::Other => format!("type mismatch: expected {t}, found {s}"),
        };
        self.err(msg, span);
    }

    /// Rejects untyped `nil` where no type is known (`var x = nil`).
    pub(crate) fn default_type(&mut self, e: hir::Expr) -> hir::Expr {
        if matches!(self.tt.get(e.ty), Type::Nil) {
            self.err_help("use of untyped nil", e.span, "give the variable an explicit type: `var x *T = nil`");
            return mk(e.kind, self.tt.invalid, e.span);
        }
        e
    }
}
