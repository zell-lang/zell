//! Generics: generic declarations ("templates"), constraints, instantiation
//! (monomorphisation) and the one-time check of generic bodies against
//! their constraints. Calls and inference of type arguments live in
//! [`super::infer`].
//!
//! # Model
//!
//! * A generic function, struct, enum or method is registered as a
//!   [`Template`] (`Member::Generic` in its namespace) instead of an
//!   ordinary declaration. Its type parameters get entries in
//!   [`GenState::tparams`] (name and [`Bound`]).
//! * Inside generic code a type parameter name resolves through the type
//!   environment [`GenState::tenv`] (`T` -> a [`TypeId`]).
//! * **Abstract check** ([`Sema::check_templates`]): every template is
//!   instantiated once with its own parameters as `Type::Param` types and
//!   its bodies are checked; operations on a `Type::Param` value are
//!   allowed only as far as its constraint permits (Go style). Everything
//!   this creates is rolled back afterwards; only the diagnostics remain.
//! * **Instantiation**: each distinct `(template, type arguments)` pair is
//!   lowered once into an ordinary HIR function, struct or enum with a
//!   readable name (`Max<int>`) and a mangled symbol (`zell_ns__Max__int`),
//!   cached in [`GenState::cache`]. A generic struct instance instantiates
//!   its methods with it. Bodies of instances are checked (with concrete
//!   types) by the main checking loop, so codegen never sees generics.

use super::resolve::PathRes;
use super::*;
use crate::hir::{FuncKind, NamespaceId};
use std::collections::HashSet;

/// Maximum nesting of instantiations (guards infinite expansion such as
/// `func F<T>(x T) { F([x]) }`).
const MAX_DEPTH: u32 = 48;

/// Constraint of a type parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bound {
    /// No constraint: values can only be assigned, passed and returned.
    Any,
    /// Builtin `Ordered`: integers, floats, strings, chars (`< <= > >= == != +`).
    Ordered,
    /// Builtin `Comparable`: types with builtin `==` / `!=` (also map keys).
    Comparable,
    /// Builtin `Equatable`: `Comparable` types and types with
    /// `func Equals(other T) bool` (`==` / `!=` only, not map keys).
    Equatable,
    /// Builtin operator constraint (`T: Add`): numeric types with the
    /// operator or types with the operator method (`func Add(other T) T`).
    Op(super::opover::OpIface),
    /// An interface: the type must have its methods.
    Iface(TypeId),
}

/// A type parameter.
pub(crate) struct TParam {
    pub name: String,
    pub bound: Bound,
}

/// The declaration a template instantiates.
#[derive(Clone, Copy)]
pub(crate) enum TmplDecl<'a> {
    Func(&'a ast::FuncDecl),
    Struct(&'a ast::StructDecl),
    Enum(&'a ast::EnumDecl),
    /// A generic method of struct `.1` (a plain struct or an instance).
    Method(&'a ast::FuncDecl, u32),
}

/// A generic declaration.
pub(crate) struct Template<'a> {
    pub name: String,
    pub decl: TmplDecl<'a>,
    pub generics: &'a [ast::GenericParam],
    /// Indices into [`GenState::tparams`], one per generic parameter.
    pub params: Vec<u32>,
    /// Bindings of the enclosing generic struct instance (methods only).
    pub outer: Vec<(String, TypeId)>,
    pub ns: NsIdx,
    pub file: usize,
    /// Declared in the stdlib (`collections`): trusted, not checked
    /// abstractly, instantiated in the scope of the user file.
    pub is_std: bool,
    /// Template whose abstract check covers this one.
    pub root: u32,
    /// The abstract check reported errors: instances are not checked again.
    pub failed: bool,
    /// Created by the instantiation of a generic struct (checked as part of
    /// the struct's abstract check).
    pub nested: bool,
    /// A stdlib template whose module is not parsed yet: (module, index of
    /// the declaration); `decl` is a placeholder of the right kind until
    /// [`Sema::force_template`].
    pub lazy: Option<(u32, u32)>,
}

/// What an instantiation produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inst {
    Func(FuncId),
    Struct(u32),
    Enum(u32),
}

/// Context of an instantiated function, for checking its body.
pub(crate) struct InstFunc {
    pub env: Vec<(String, TypeId)>,
    pub file: usize,
    pub ns: NsIdx,
    pub root: u32,
    /// `in instantiation of Max<string>` (none for abstract instances).
    pub note: Option<String>,
    pub depth: u32,
    /// Where it was instantiated: the call site (file, span), then the
    /// sites of the instances that call lies in (outermost last).
    pub sites: Vec<(usize, Span)>,
}

/// A constraint check postponed until method signatures are known.
pub(crate) struct Deferred {
    ty: TypeId,
    bound: Bound,
    what: String,
    file: usize,
    span: Span,
}

/// All generics state of an analysis.
#[derive(Default)]
pub(crate) struct GenState<'a> {
    pub templates: Vec<Template<'a>>,
    pub tparams: Vec<TParam>,
    /// Current type-parameter bindings (innermost last).
    pub tenv: Vec<(String, TypeId)>,
    pub cache: HashMap<(u32, Vec<TypeId>), Inst>,
    /// Instantiated functions by `FuncId`.
    pub insts: HashMap<u32, InstFunc>,
    /// Struct instances: struct id -> (template, type arguments).
    pub inst_structs: HashMap<u32, (u32, Vec<TypeId>)>,
    pub inst_enums: HashMap<u32, (u32, Vec<TypeId>)>,
    /// Generic methods: (struct id, name) -> template.
    pub methods: HashMap<(u32, String), u32>,
    /// Note added to diagnostics (instantiation context).
    pub note: Option<String>,
    /// While > 0, diagnostics are dropped (instances of failed templates).
    pub mute: u32,
    pub depth: u32,
    /// Method signatures are resolved (constraint checks can run).
    pub sigs_ready: bool,
    pub deferred: Vec<Deferred>,
    /// Instance methods whose body is not checked (unsatisfied
    /// requirements of a stdlib method, e.g. `List<T>.Contains` with a
    /// non-comparable `T`; calls are rejected instead).
    pub skip_body: HashSet<u32>,
    /// Expected type of the generic call being checked (`return
    /// f()` with `func f<T>() T` infers `T` from the function's result type); set by
    /// `call_res`, taken by `generic_call`.
    pub ret_hint: Option<TypeId>,
    /// Map lookups `(v, ok) = m[k]` of generic bodies whose declared value
    /// type is a type parameter (file, span of the index), found by the
    /// abstract check: always comma-ok in every instance (DESIGN.md,
    /// comma-ok), even when the parameter is instantiated as a tuple.
    pub comma_ok_param: HashSet<(usize, crate::span::Span)>,
}

/// Saved analyser context around an instantiation.
struct Saved {
    file: usize,
    ns: NsIdx,
    tenv: Vec<(String, TypeId)>,
    note: Option<String>,
    ide: Option<Box<index::Recorder>>,
    depth: u32,
    muted: bool,
}

/// Lengths of the declaration tables, for rolling back an abstract check.
struct Mark {
    funcs: usize,
    structs: usize,
    enums: usize,
    templates: usize,
    tparams: usize,
}

/// Short type names (`Point`, not `app.Point`) for instance names.
struct Short<'s, 'a>(&'s Sema<'a>);

impl hir::dump::DeclNames for Short<'_, '_> {
    fn struct_name(&self, id: u32) -> String {
        self.0.structs.get(id as usize).map(|s| s.name.clone()).unwrap_or_default()
    }
    fn interface_name(&self, id: u32) -> String {
        self.0.interfaces.get(id as usize).map(|s| s.name.clone()).unwrap_or_default()
    }
    fn enum_name(&self, id: u32) -> String {
        self.0.enums.get(id as usize).map(|s| s.name.clone()).unwrap_or_default()
    }
    fn param_name(&self, id: u32) -> String {
        self.0.gen.tparams.get(id as usize).map(|p| p.name.clone()).unwrap_or_default()
    }
}

/// Link symbol for an instance's full name: `app.Max<int>` ->
/// `zell_app__Max__int`, `Pair<[]int, *T>` -> `Pair__Sint_PT`.
fn mangle(full_name: &str) -> String {
    let mut s = String::from("zell_");
    let mut chars = full_name.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_ascii_alphanumeric() || c == '_' => s.push(c),
            '.' | '<' => s.push_str("__"),
            '>' | ' ' => {}
            ',' => s.push('_'),
            '[' => {
                if chars.peek() == Some(&']') {
                    chars.next();
                }
                s.push('S');
            }
            '*' => s.push('P'),
            '!' => s.push('E'),
            _ => s.push('_'),
        }
    }
    s
}

impl<'a> Sema<'a> {
    // -- templates -------------------------------------------------------------

    /// Registers a template in the current scope; bounds are resolved by
    /// [`resolve_template_bounds`](Self::resolve_template_bounds) (or the
    /// caller, for nested templates).
    pub(crate) fn new_template(&mut self, name: &str, decl: TmplDecl<'a>, generics: &'a [ast::GenericParam], outer: Vec<(String, TypeId)>, nested: bool) -> u32 {
        let t = self.gen.templates.len() as u32;
        let mut params = Vec::new();
        for (i, g) in generics.iter().enumerate() {
            if generics[..i].iter().any(|p| p.name.name == g.name.name) {
                self.err(format!("duplicate type parameter {} of {name}", g.name.name), g.name.span);
            }
            params.push(self.gen.tparams.len() as u32);
            self.gen.tparams.push(TParam { name: g.name.name.clone(), bound: Bound::Any });
        }
        let is_std = self.nss[self.cur_ns].is_std;
        self.gen.templates.push(Template {
            name: name.to_string(),
            decl,
            generics,
            params,
            outer,
            ns: self.cur_ns,
            file: self.cur_file,
            is_std,
            root: t,
            failed: false,
            nested,
            lazy: None,
        });
        t
    }

    /// Resolves the constraints of every template registered so far.
    pub(crate) fn resolve_template_bounds(&mut self) {
        for t in 0..self.gen.templates.len() {
            let (file, ns) = (self.gen.templates[t].file, self.gen.templates[t].ns);
            if file >= self.file_scopes.len() {
                continue;
            }
            self.cur_file = file;
            self.cur_ns = ns;
            self.resolve_bounds_of(t as u32);
        }
    }

    /// Resolves the constraints of template `t` in the current scope.
    fn resolve_bounds_of(&mut self, t: u32) {
        let tm = &self.gen.templates[t as usize];
        let (generics, params, is_std) = (tm.generics, tm.params.clone(), tm.is_std);
        for (g, p) in generics.iter().zip(params) {
            if let Some(b) = &g.bound {
                let bound = self.resolve_bound(b, is_std);
                self.gen.tparams[p as usize].bound = bound;
            }
        }
    }

    fn resolve_bound(&mut self, b: &ast::TypeExpr, is_std: bool) -> Bound {
        if let ast::TypeKind::Named { path, generics } = &b.kind {
            if generics.is_empty() && path.segments.len() == 1 {
                let n = &path.segments[0];
                let builtin = match n.name.as_str() {
                    "Ordered" => Some(Bound::Ordered),
                    "Comparable" => Some(Bound::Comparable),
                    "Equatable" => Some(Bound::Equatable),
                    other => super::opover::OpIface::from_name(other).map(Bound::Op),
                };
                if let Some(bb) = builtin {
                    // A user type of the same name wins; a function named
                    // `Add` does not hide the builtin constraint.
                    let user_type = matches!(
                        self.lookup_global(n, false),
                        Some(PathRes::Member(Member::Struct(_) | Member::Interface(_) | Member::Enum(_) | Member::Generic(_)))
                    );
                    if is_std || !user_type {
                        return bb;
                    }
                }
            }
        }
        let ty = self.resolve_type(b);
        match self.tt.get(ty) {
            Type::Interface(_) | Type::Error => Bound::Iface(ty),
            Type::Any | Type::Invalid => Bound::Any,
            _ => {
                self.err_help(
                    format!("constraint {} is not an interface", self.tn(ty)),
                    b.span,
                    "use an interface, or a builtin constraint: Ordered, Comparable, Equatable, Add, Sub, Mul, Div, Rem, Neg, MatMul",
                );
                Bound::Any
            }
        }
    }

    /// The binding of type parameter `name` in the current environment.
    pub(crate) fn type_param(&self, name: &str) -> Option<TypeId> {
        self.gen.tenv.iter().rev().find(|(n, _)| n == name).map(|(_, t)| *t)
    }

    fn param_types(&mut self, t: u32) -> Vec<TypeId> {
        let ps = self.gen.templates[t as usize].params.clone();
        ps.into_iter().map(|p| self.tt.intern(Type::Param(p))).collect()
    }

    /// Constraint of a `Type::Param`.
    pub(crate) fn bound_of(&self, t: TypeId) -> Option<Bound> {
        match self.tt.get(t) {
            Type::Param(p) => Some(self.gen.tparams[*p as usize].bound),
            _ => None,
        }
    }

    /// A type parameter whose constraint allows `==`.
    pub(crate) fn param_comparable(&self, t: TypeId) -> bool {
        matches!(self.bound_of(t), Some(Bound::Ordered | Bound::Comparable))
    }

    /// True if `t` mentions a type parameter (also through instance args).
    pub(crate) fn has_param(&self, t: TypeId) -> bool {
        match self.tt.get(t) {
            Type::Param(_) => true,
            Type::Pointer(x) | Type::Weak(x) | Type::Slice(x) | Type::Array(x, _) | Type::Fallible(x) | Type::Task(x) => self.has_param(*x),
            Type::Map(k, v) => self.has_param(*k) || self.has_param(*v),
            Type::Tuple(ts) => ts.iter().any(|x| self.has_param(*x)),
            Type::Func(sig) => sig.params.iter().any(|x| self.has_param(*x)) || self.has_param(sig.ret),
            Type::Struct(d) => self.gen.inst_structs.get(&d.0).is_some_and(|(_, a)| a.iter().any(|x| self.has_param(*x))),
            Type::Enum(d) => self.gen.inst_enums.get(&d.0).is_some_and(|(_, a)| a.iter().any(|x| self.has_param(*x))),
            _ => false,
        }
    }

    // -- constraints ---------------------------------------------------------

    /// Types usable with `< <= > >=`.
    pub(crate) fn is_ordered(&self, t: TypeId) -> bool {
        self.tt.is_numeric(t) || matches!(self.tt.get(t), Type::String | Type::Char | Type::Invalid) || self.bound_of(t) == Some(Bound::Ordered)
    }

    /// Types usable with `==` / `!=` (and as map keys).
    pub(crate) fn is_comparable(&self, t: TypeId) -> bool {
        match self.tt.get(t) {
            Type::Int(_) | Type::Float32 | Type::Float64 | Type::Bool | Type::Char | Type::String | Type::Pointer(_) | Type::Invalid => true,
            Type::Enum(e) => self.enums.get(e.0 as usize).is_none_or(|e| e.is_plain),
            Type::Param(_) => self.param_comparable(t),
            _ => false,
        }
    }

    /// Methods of an interface missing from type `ty` (a struct, an
    /// interface or an interface-constrained type parameter).
    fn missing_for_bound(&self, ty: TypeId, iface: TypeId) -> Vec<String> {
        let have: Option<Vec<hir::MethodSig>> = match self.bound_of(ty) {
            Some(Bound::Iface(j)) => Some(self.iface_methods(j)),
            Some(_) => Some(Vec::new()),
            None if self.tt.is_interface(ty) => Some(self.iface_methods(ty)),
            None => None,
        };
        match have {
            Some(have) => self
                .iface_methods(iface)
                .into_iter()
                .filter(|m| !have.iter().any(|h| h.name == m.name && h.params == m.params && h.ret == m.ret) && !(m.default.is_some() && !have.iter().any(|h| h.name == m.name)))
                .map(|m| format!("missing method {}", m.name))
                .collect(),
            // `S`, `*S` (like the conversion `var l Logger = &s`) and enums.
            None if self.has_methods(ty) => self.missing_methods(ty, iface),
            None => self.iface_methods(iface).into_iter().map(|m| format!("missing method {}", m.name)).collect(),
        }
    }

    /// Checks `ty` against `bound`; the error text on failure.
    pub(crate) fn satisfies(&self, ty: TypeId, bound: Bound) -> Result<(), String> {
        if self.tt.is_invalid(ty) {
            return Ok(());
        }
        let t = self.tn(ty);
        match bound {
            Bound::Any => Ok(()),
            Bound::Ordered if self.is_ordered(ty) || self.has_cmp_method(ty, "Compare") => Ok(()),
            Bound::Ordered => Err(format!("type {t} does not satisfy constraint Ordered (integers, floats, strings, chars and types with func Compare(other {t}) int are ordered)")),
            Bound::Comparable if self.is_comparable(ty) => Ok(()),
            Bound::Comparable => Err(format!("type {t} does not satisfy constraint Comparable: values of {t} cannot be compared with ==")),
            Bound::Equatable if self.is_comparable(ty) || self.has_cmp_method(ty, "Equals") || self.bound_of(ty) == Some(Bound::Equatable) => Ok(()),
            Bound::Equatable => Err(format!("type {t} does not satisfy constraint Equatable: values of {t} cannot be compared with == (declare func Equals(other {t}) bool)")),
            Bound::Op(k) => self.op_bound_ok(ty, k),
            Bound::Iface(i) => {
                let missing = self.missing_for_bound(ty, i);
                if missing.is_empty() {
                    Ok(())
                } else {
                    Err(format!("type {t} does not satisfy constraint {}: {}", self.tn(i), missing.join(", ")))
                }
            }
        }
    }

    /// Checks the type arguments of an instantiation of `t` against its
    /// constraints (postponed while method signatures are unresolved).
    fn check_bounds(&mut self, t: u32, args: &[TypeId], span: Span) -> bool {
        let tm = &self.gen.templates[t as usize];
        let params = tm.params.clone();
        let tname = tm.name.clone();
        let mut ok = true;
        for (p, a) in params.into_iter().zip(args) {
            let bound = self.gen.tparams[p as usize].bound;
            if bound == Bound::Any {
                continue;
            }
            let what = format!("type parameter {} of {tname}", self.gen.tparams[p as usize].name);
            if !self.gen.sigs_ready {
                self.gen.deferred.push(Deferred { ty: *a, bound, what, file: self.cur_file, span });
                continue;
            }
            if let Err(msg) = self.satisfies(*a, bound) {
                self.err(format!("{msg} ({what})"), span);
                ok = false;
            }
        }
        ok
    }

    /// Runs the constraint checks postponed during signature resolution.
    pub(crate) fn run_deferred_bounds(&mut self) {
        self.gen.sigs_ready = true;
        for d in std::mem::take(&mut self.gen.deferred) {
            if let Err(msg) = self.satisfies(d.ty, d.bound) {
                let saved = self.cur_file;
                self.cur_file = d.file;
                self.err(format!("{msg} ({})", d.what), d.span);
                self.cur_file = saved;
            }
        }
    }

    // -- instantiation -------------------------------------------------------

    /// `Name<A, B>` with short argument names.
    pub(crate) fn inst_name(&self, base: &str, args: &[TypeId]) -> String {
        let names: Vec<String> = args.iter().map(|a| hir::dump::type_name(&self.tt, *a, &Short(self))).collect();
        format!("{base}<{}>", names.join(", "))
    }

    /// A link symbol for `full_name`, unique in the program.
    fn inst_symbol(&mut self, full_name: &str) -> String {
        let base = mangle(full_name);
        let mut sym = base.clone();
        let mut n = 1;
        while self.funcs.iter().any(|f| f.symbol == sym) {
            n += 1;
            sym = format!("{base}_{n}");
        }
        sym
    }

    /// Switches to the scope of template `t` with bindings `env`.
    fn enter(&mut self, t: u32, env: Vec<(String, TypeId)>, note: Option<String>) -> Saved {
        let tm = &self.gen.templates[t as usize];
        let (file, ns, is_std, root) = (tm.file, tm.ns, tm.is_std, tm.root);
        let muted = self.gen.templates[root as usize].failed;
        let saved = Saved {
            file: self.cur_file,
            ns: self.cur_ns,
            tenv: std::mem::replace(&mut self.gen.tenv, env),
            note: std::mem::replace(&mut self.gen.note, note),
            ide: self.ide.take(),
            depth: self.gen.depth,
            muted,
        };
        if !is_std {
            self.cur_file = file;
        }
        self.cur_ns = ns;
        self.gen.depth += 1;
        if muted {
            self.gen.mute += 1;
        }
        saved
    }

    fn leave(&mut self, s: Saved) {
        self.cur_file = s.file;
        self.cur_ns = s.ns;
        self.gen.tenv = s.tenv;
        self.gen.note = s.note;
        self.ide = s.ide;
        self.gen.depth = s.depth;
        if s.muted {
            self.gen.mute -= 1;
        }
    }

    /// HIR namespace for declarations of template `t`: its own, or for
    /// stdlib templates the namespace of the instantiating code.
    fn inst_hir_ns(&self, t: u32, caller_ns: NsIdx) -> NamespaceId {
        let tm = &self.gen.templates[t as usize];
        let ns = if tm.is_std { caller_ns } else { tm.ns };
        self.nss[ns].hir_id.unwrap_or(NamespaceId(0))
    }

    fn env_of(&self, t: u32, args: &[TypeId]) -> Vec<(String, TypeId)> {
        let tm = &self.gen.templates[t as usize];
        let mut env = tm.outer.clone();
        env.extend(tm.generics.iter().map(|g| g.name.name.clone()).zip(args.iter().copied()));
        env
    }

    fn depth_ok(&mut self, name: &str, span: Span) -> bool {
        if self.gen.depth >= MAX_DEPTH {
            self.err(format!("generic instantiation nested too deeply at {name} (limit {MAX_DEPTH}; infinitely recursive instantiation?)"), span);
            return false;
        }
        true
    }

    /// Instantiation sites for an instance created by the code at `span`
    /// of the current file (see [`InstFunc::sites`]).
    fn inst_sites(&self, span: Span) -> Vec<(usize, Span)> {
        let mut sites = Vec::new();
        if span != Span::DUMMY {
            sites.push((self.cur_file, span));
        }
        let owner = self.fns.iter().rev().find_map(|c| c.func.filter(|fid| !matches!(self.funcs[fid.0 as usize].kind, FuncKind::Lambda { .. })));
        if let Some(i) = owner.and_then(|fid| self.gen.insts.get(&fid.0)) {
            sites.extend(i.sites.iter().copied());
        }
        sites
    }

    fn note_for(&self, name: &str, args: &[TypeId]) -> Option<String> {
        (!args.iter().any(|a| self.has_param(*a))).then(|| format!("in instantiation of {name}"))
    }

    /// Instantiates template `t` with `args` after checking constraints.
    pub(crate) fn instantiate(&mut self, t: u32, args: Vec<TypeId>, span: Span) -> Option<Inst> {
        if args.iter().any(|a| self.tt.is_invalid(*a)) {
            return None;
        }
        if let Some(&i) = self.gen.cache.get(&(t, args.clone())) {
            self.unit_memo(units::MemoKey::Inst(i.into()));
            return Some(i);
        }
        if !self.check_bounds(t, &args, span) {
            return None;
        }
        match self.gen.templates[t as usize].decl {
            TmplDecl::Func(_) | TmplDecl::Method(..) => self.instantiate_func(t, args, span).map(Inst::Func),
            TmplDecl::Struct(_) => self.instantiate_struct(t, args, span).map(Inst::Struct),
            TmplDecl::Enum(_) => self.instantiate_enum(t, args, span).map(Inst::Enum),
        }
    }

    /// Instance of a generic function or method (constraints not checked).
    pub(crate) fn instantiate_func(&mut self, t: u32, args: Vec<TypeId>, span: Span) -> Option<FuncId> {
        let key = (t, args.clone());
        if let Some(&Inst::Func(f)) = self.gen.cache.get(&key) {
            self.unit_memo(units::MemoKey::Inst(Inst::Func(f).into()));
            return Some(f);
        }
        self.force_template(t);
        let tm = &self.gen.templates[t as usize];
        let (decl, owner) = match tm.decl {
            TmplDecl::Func(d) => (d, None),
            TmplDecl::Method(d, s) => (d, Some(s)),
            _ => return None,
        };
        // `@intrinsic` stubs (`testing.T.Must`) document a call the
        // compiler lowers itself; any other use would run the stub body.
        if tm.is_std && decl.attrs.iter().any(|a| a.path.to_string() == "intrinsic") {
            let what = match owner {
                Some(s) => format!("{}.{}", super::index::strip_type_args(&self.structs[s as usize].full_name), decl.name.name),
                None => format!("{}.{}", self.nss[tm.ns].path, decl.name.name),
            };
            self.err(format!("{what} is a compiler intrinsic: call it directly, without type arguments"), span);
            return None;
        }
        let root = tm.root;
        let name = self.inst_name(&tm.name.clone(), &args);
        if !self.depth_ok(&name, span) {
            return None;
        }
        let env = self.env_of(t, &args);
        let note = self.note_for(&name, &args);
        let hns = self.inst_hir_ns(t, self.cur_ns);
        let node = self.unit_new_node();
        let unit_prev = self.unit_enter(node);
        let sites = self.inst_sites(span);
        let saved = self.enter(t, env.clone(), note.clone());
        let (kind, src, local) = match owner {
            Some(s) => (FuncKind::Method { struct_id: s, is_static: decl.is_static }, FuncSrc::Method(decl, s), format!("{}.{name}", self.structs[s as usize].name)),
            None => (FuncKind::Free, FuncSrc::Free(decl), name.clone()),
        };
        let fid = self.new_func(decl, kind, src, &local);
        let full = self.funcs[fid.0 as usize].full_name.clone();
        let symbol = self.inst_symbol(&full);
        let f = &mut self.funcs[fid.0 as usize];
        f.name = name;
        f.symbol = symbol;
        f.namespace = hns;
        self.gen.cache.insert(key, Inst::Func(fid));
        self.unit_bind(node, units::MemoKey::Inst(Inst::Func(fid).into()));
        self.unit_bind_func(node, fid);
        self.gen.insts.insert(fid.0, InstFunc { env, file: self.cur_file, ns: self.cur_ns, root, note, depth: self.gen.depth, sites });
        self.resolve_func_sig(fid.0 as usize);
        self.leave(saved);
        self.unit_leave(unit_prev);
        Some(fid)
    }

    /// Instance of a generic struct, with its methods.
    pub(crate) fn instantiate_struct(&mut self, t: u32, args: Vec<TypeId>, span: Span) -> Option<u32> {
        let key = (t, args.clone());
        if let Some(&Inst::Struct(s)) = self.gen.cache.get(&key) {
            self.unit_memo(units::MemoKey::Inst(Inst::Struct(s).into()));
            return Some(s);
        }
        self.force_template(t);
        let tm = &self.gen.templates[t as usize];
        let TmplDecl::Struct(decl) = tm.decl else { return None };
        let (root, tname, ns) = (tm.root, tm.name.clone(), tm.ns);
        let name = self.inst_name(&tname, &args);
        if !self.depth_ok(&name, span) {
            return None;
        }
        let full_args: Vec<String> = args.iter().map(|a| self.tn_full(*a)).collect();
        let full_name = format!("{}.{tname}<{}>", self.nss[ns].path, full_args.join(", "));
        let env = self.env_of(t, &args);
        let note = self.note_for(&name, &args);
        let hns = self.inst_hir_ns(t, self.cur_ns);
        let node = self.unit_new_node();
        let unit_prev = self.unit_enter(node);
        let sites = self.inst_sites(span);
        let saved = self.enter(t, env.clone(), note.clone());
        let sid = self.structs.len() as u32;
        self.structs.push(hir::Struct {
            name: name.clone(),
            full_name,
            namespace: hns,
            fields: Vec::new(),
            methods: Vec::new(),
            attrs: Vec::new(),
            is_attribute: false,
            exported: is_exported(&tname),
            file: self.cur_file,
            span: decl.span,
        });
        self.struct_src.push(decl);
        self.gen.cache.insert(key, Inst::Struct(sid));
        self.unit_bind(node, units::MemoKey::Inst(Inst::Struct(sid).into()));
        self.gen.inst_structs.insert(sid, (t, args));
        self.resolve_struct_fields(sid as usize);
        self.check_recursive_struct(sid as usize);
        if self.attrs_done {
            self.struct_attrs(sid as usize);
        }
        for m in &decl.methods {
            let dup = self.structs[sid as usize].methods.iter().any(|(n, _)| *n == m.name.name) || self.gen.methods.contains_key(&(sid, m.name.name.clone()));
            if dup {
                self.err(format!("duplicate method {} in struct {tname}", m.name.name), m.name.span);
                continue;
            }
            if let Some(f) = self.structs[sid as usize].fields.iter().find(|f| f.name == m.name.name) {
                let fspan = f.span;
                self.err(format!("struct {tname} has both a field and a method named {}", m.name.name), fspan);
            }
            if !m.generics.is_empty() {
                let mt = self.new_template(&m.name.name, TmplDecl::Method(m, sid), &m.generics, env.clone(), true);
                self.gen.templates[mt as usize].root = root;
                self.resolve_bounds_of(mt);
                self.gen.methods.insert((sid, m.name.name.clone()), mt);
                continue;
            }
            let kind = FuncKind::Method { struct_id: sid, is_static: m.is_static };
            let fid = self.new_func(m, kind, FuncSrc::Method(m, sid), &format!("{name}.{}", m.name.name));
            let full = self.funcs[fid.0 as usize].full_name.clone();
            let symbol = self.inst_symbol(&full);
            self.funcs[fid.0 as usize].symbol = symbol;
            self.funcs[fid.0 as usize].namespace = hns;
            self.structs[sid as usize].methods.push((m.name.name.clone(), fid));
            self.unit_bind_func(node, fid);
            self.gen.insts.insert(fid.0, InstFunc { env: env.clone(), file: self.cur_file, ns: self.cur_ns, root, note: note.clone(), depth: self.gen.depth, sites: sites.clone() });
            self.resolve_func_sig(fid.0 as usize);
        }
        if self.gen.sigs_ready && !decl.implements.is_empty() {
            self.check_struct_implements(sid);
        }
        self.std_instance_hook(sid);
        self.leave(saved);
        self.unit_leave(unit_prev);
        Some(sid)
    }

    /// Instance of a generic enum.
    pub(crate) fn instantiate_enum(&mut self, t: u32, args: Vec<TypeId>, span: Span) -> Option<u32> {
        let key = (t, args.clone());
        if let Some(&Inst::Enum(e)) = self.gen.cache.get(&key) {
            self.unit_memo(units::MemoKey::Inst(Inst::Enum(e).into()));
            return Some(e);
        }
        self.force_template(t);
        let tm = &self.gen.templates[t as usize];
        let TmplDecl::Enum(decl) = tm.decl else { return None };
        let (root, tname, ns) = (tm.root, tm.name.clone(), tm.ns);
        let name = self.inst_name(&tname, &args);
        if !self.depth_ok(&name, span) {
            return None;
        }
        let full_args: Vec<String> = args.iter().map(|a| self.tn_full(*a)).collect();
        let full_name = format!("{}.{tname}<{}>", self.nss[ns].path, full_args.join(", "));
        let env = self.env_of(t, &args);
        let note = self.note_for(&name, &args);
        let hns = self.inst_hir_ns(t, self.cur_ns);
        let node = self.unit_new_node();
        let unit_prev = self.unit_enter(node);
        let sites = self.inst_sites(span);
        let saved = self.enter(t, env.clone(), note.clone());
        let eid = self.enums.len() as u32;
        self.enums.push(hir::Enum {
            name: name.clone(),
            full_name,
            namespace: hns,
            variants: Vec::new(),
            is_plain: decl.variants.iter().all(|v| v.fields.is_empty()),
            methods: Vec::new(),
            attrs: Vec::new(),
            exported: is_exported(&tname),
            file: self.cur_file,
            span: decl.span,
        });
        self.enum_src.push(decl);
        self.gen.cache.insert(key, Inst::Enum(eid));
        self.unit_bind(node, units::MemoKey::Inst(Inst::Enum(eid).into()));
        self.gen.inst_enums.insert(eid, (t, args));
        self.resolve_enum_variants(eid as usize);
        // Methods: instantiated with the enum, like a generic struct's
        // (the receiver is the enum value, as for a plain enum).
        for m in &decl.methods {
            if m.is_static || !m.generics.is_empty() {
                self.err("enum methods cannot be static or generic", m.name.span);
                continue;
            }
            if self.enum_methods.contains_key(&(eid, m.name.name.clone())) || decl.variants.iter().any(|v| v.name.name == m.name.name) {
                self.err(format!("duplicate member {} in enum {tname}", m.name.name), m.name.span);
                continue;
            }
            let fid = self.new_func(m, FuncKind::Free, FuncSrc::Free(m), &format!("{name}.{}", m.name.name));
            let full = self.funcs[fid.0 as usize].full_name.clone();
            let symbol = self.inst_symbol(&full);
            self.funcs[fid.0 as usize].symbol = symbol;
            self.funcs[fid.0 as usize].namespace = hns;
            self.enum_methods.insert((eid, m.name.name.clone()), fid);
            self.enums[eid as usize].methods.push((m.name.name.clone(), fid));
            self.enum_method_owner.insert(fid.0, eid);
            self.unit_bind_func(node, fid);
            self.gen.insts.insert(fid.0, InstFunc { env: env.clone(), file: self.cur_file, ns: self.cur_ns, root, note: note.clone(), depth: self.gen.depth, sites: sites.clone() });
            self.resolve_func_sig(fid.0 as usize);
        }
        self.leave(saved);
        self.unit_leave(unit_prev);
        Some(eid)
    }

    /// Resolves explicit type arguments for template `t` (count checked).
    pub(crate) fn type_args(&mut self, t: u32, targs: &[ast::TypeExpr], span: Span) -> Option<Vec<TypeId>> {
        let tm = &self.gen.templates[t as usize];
        let (n, name) = (tm.params.len(), tm.name.clone());
        let args: Vec<TypeId> = targs.iter().map(|a| self.resolve_type(a)).collect();
        if args.len() != n {
            self.err(format!("wrong number of type arguments for {name}: expected {n}, got {}", args.len()), span);
            return None;
        }
        if args.iter().any(|a| self.tt.is_invalid(*a)) {
            return None;
        }
        Some(args)
    }

    /// `Name<A, B>` written as a type.
    pub(crate) fn resolve_generic_type(&mut self, path: &ast::Path, targs: &[ast::TypeExpr], span: Span) -> TypeId {
        let segs: Vec<&ast::Ident> = path.segments.iter().collect();
        match self.resolve_segments(&segs, true) {
            Some((PathRes::Member(Member::Generic(t)), n)) if n == segs.len() => {
                if matches!(self.gen.templates[t as usize].decl, TmplDecl::Func(_) | TmplDecl::Method(..)) {
                    self.err(format!("{} is a generic function, not a type", path.to_string()), path.span);
                    return self.tt.invalid;
                }
                let Some(args) = self.type_args(t, targs, span) else { return self.tt.invalid };
                match self.instantiate(t, args, span) {
                    Some(Inst::Struct(s)) => self.struct_ty(s),
                    Some(Inst::Enum(e)) => self.enum_ty(e),
                    _ => self.tt.invalid,
                }
            }
            Some((res, n)) if n == segs.len() && self.path_res_type(res).is_some() => {
                self.err(format!("type {} is not generic (it takes no type arguments)", path.to_string()), span);
                self.tt.invalid
            }
            _ => {
                if !self.diags.has_errors_at(path.span, self.cur_file) {
                    self.unknown_type(path);
                }
                self.tt.invalid
            }
        }
    }

    // -- checking ------------------------------------------------------------

    /// Checks the body of an instantiated function in its template's scope.
    pub(crate) fn check_instance_body(&mut self, fid: FuncId, decl: &'a ast::FuncDecl, body: &'a ast::Block) {
        let info = &self.gen.insts[&fid.0];
        if self.gen.templates[info.root as usize].failed || self.gen.skip_body.contains(&fid.0) {
            return;
        }
        if self.unit.is_some() && self.unit_skips_instance(fid) {
            return;
        }
        let saved = Saved {
            file: std::mem::replace(&mut self.cur_file, info.file),
            ns: std::mem::replace(&mut self.cur_ns, info.ns),
            tenv: std::mem::replace(&mut self.gen.tenv, info.env.clone()),
            note: std::mem::replace(&mut self.gen.note, info.note.clone()),
            ide: self.ide.take(),
            depth: std::mem::replace(&mut self.gen.depth, info.depth),
            muted: false,
        };
        let unit_prev = self.unit_enter(self.unit_func_node(fid));
        self.check_body_in_scope(fid, decl, body);
        self.unit_leave(unit_prev);
        self.leave(saved);
    }

    fn error_count(&self) -> usize {
        self.diags.items.iter().filter(|d| d.severity == crate::diagnostics::Severity::Error).count()
    }

    fn mark(&self) -> Mark {
        Mark { funcs: self.funcs.len(), structs: self.structs.len(), enums: self.enums.len(), templates: self.gen.templates.len(), tparams: self.gen.tparams.len() }
    }

    /// Drops every declaration created after `m`.
    fn rollback(&mut self, m: Mark) {
        if let Some(u) = self.unit.as_mut() {
            u.rollback(m.funcs, m.structs, m.enums);
        }
        self.reflect_rollback(m.funcs);
        self.funcs.truncate(m.funcs);
        self.func_src.truncate(m.funcs);
        self.sigs.truncate(m.funcs);
        self.variadic.retain(|f| (*f as usize) < m.funcs);
        self.structs.truncate(m.structs);
        self.struct_src.truncate(m.structs);
        self.enums.truncate(m.enums);
        self.enum_src.truncate(m.enums);
        self.enum_methods.retain(|(e, _), f| (*e as usize) < m.enums && (f.0 as usize) < m.funcs);
        self.enum_method_owner.retain(|f, e| (*f as usize) < m.funcs && (*e as usize) < m.enums);
        let g = &mut self.gen;
        g.templates.truncate(m.templates);
        g.tparams.truncate(m.tparams);
        g.cache.retain(|_, v| match v {
            Inst::Func(f) => (f.0 as usize) < m.funcs,
            Inst::Struct(s) => (*s as usize) < m.structs,
            Inst::Enum(e) => (*e as usize) < m.enums,
        });
        g.insts.retain(|k, _| (*k as usize) < m.funcs);
        g.inst_structs.retain(|k, _| (*k as usize) < m.structs);
        g.inst_enums.retain(|k, _| (*k as usize) < m.enums);
        g.methods.retain(|(s, _), t| (*s as usize) < m.structs && (*t as usize) < m.templates);
        g.skip_body.retain(|k| (*k as usize) < m.funcs);
    }

    /// Checks every user template once against its constraints (with its
    /// type parameters as `Type::Param`); the instances it creates are
    /// discarded, only diagnostics remain. Failing templates are marked so
    /// that their instances are not checked again.
    pub(crate) fn check_templates(&mut self) {
        let n = self.gen.templates.len();
        for t in 0..n {
            let tm = &self.gen.templates[t];
            if tm.is_std || tm.nested {
                continue;
            }
            let t = t as u32;
            let span = match tm.decl {
                TmplDecl::Func(d) | TmplDecl::Method(d, _) => d.name.span,
                TmplDecl::Struct(d) => d.name.span,
                TmplDecl::Enum(d) => d.name.span,
            };
            let before = self.error_count();
            let mark = self.mark();
            let node = self.unit_file_node(self.gen.templates[t as usize].file);
            let unit_prev = self.unit_enter(node);
            let saved_fns = std::mem::take(&mut self.fns);
            let args = self.param_types(t);
            match self.gen.templates[t as usize].decl {
                TmplDecl::Func(_) | TmplDecl::Method(..) => {
                    if let Some(fid) = self.instantiate_func(t, args, span) {
                        self.check_function_body(fid);
                    }
                }
                TmplDecl::Struct(_) => {
                    if let Some(sid) = self.instantiate_struct(t, args, span) {
                        let ms: Vec<FuncId> = self.structs[sid as usize].methods.iter().map(|m| m.1).collect();
                        for f in ms {
                            self.check_function_body(f);
                        }
                        let mut nested: Vec<u32> = self.gen.methods.iter().filter(|((s, _), _)| *s == sid).map(|(_, mt)| *mt).collect();
                        nested.sort();
                        for mt in nested {
                            let margs = self.param_types(mt);
                            if let Some(f) = self.instantiate_func(mt, margs, span) {
                                self.check_function_body(f);
                            }
                        }
                    }
                }
                TmplDecl::Enum(_) => {
                    if let Some(eid) = self.instantiate_enum(t, args, span) {
                        let ms: Vec<FuncId> = self.enums[eid as usize].methods.iter().map(|m| m.1).collect();
                        for f in ms {
                            self.check_function_body(f);
                        }
                    }
                }
            }
            self.fns = saved_fns;
            self.unit_leave(unit_prev);
            self.rollback(mark);
            if self.error_count() > before {
                self.gen.templates[t as usize].failed = true;
            }
        }
    }

    // -- operations on type parameters ----------------------------------------

    /// `l op l` for a type parameter `l`, as its constraint allows.
    pub(crate) fn param_binop(&mut self, op: ast::BinaryOp, l: TypeId, span: Span) -> Option<(hir::BinOp, TypeId)> {
        use ast::BinaryOp as B;
        use hir::BinOp as H;
        let bound = self.bound_of(l).unwrap_or(Bound::Any);
        let ordered = bound == Bound::Ordered;
        let comparable = ordered || bound == Bound::Comparable || bound == Bound::Equatable;
        let b = self.tt.bool_;
        let opk = match bound {
            Bound::Op(k) => Some(k),
            _ => None,
        };
        use super::opover::OpIface as K;
        let r = match op {
            B::Add if ordered || opk == Some(K::Add) => Some((H::Add, l)),
            B::Sub if opk == Some(K::Sub) => Some((H::Sub, l)),
            B::Mul if opk == Some(K::Mul) => Some((H::Mul, l)),
            B::Div if opk == Some(K::Div) => Some((H::Div, l)),
            B::Rem if opk == Some(K::Rem) => Some((H::Rem, l)),
            // Abstract check only: instances call the MatMul method.
            B::MatMul if opk == Some(K::MatMul) => Some((H::Mul, l)),
            B::Lt if ordered => Some((H::Lt, b)),
            B::Le if ordered => Some((H::Le, b)),
            B::Gt if ordered => Some((H::Gt, b)),
            B::Ge if ordered => Some((H::Ge, b)),
            B::Eq if comparable => Some((H::Eq, b)),
            B::Ne if comparable => Some((H::Ne, b)),
            _ => None,
        };
        if r.is_none() {
            let sym = expr::op_symbol(op);
            let t = self.tn(l);
            let hint = match op {
                B::Eq | B::Ne => format!(" (constrain it: {t}: Comparable)"),
                B::Lt | B::Le | B::Gt | B::Ge => format!(" (constrain it: {t}: Ordered)"),
                B::Add => format!(" (constrain it: {t}: Add or {t}: Ordered)"),
                B::Sub | B::Mul | B::Div | B::Rem | B::MatMul => {
                    let k = super::opover::binary_method(op).unwrap_or("");
                    format!(" (constrain it: {t}: {k})")
                }
                _ => String::new(),
            };
            self.err(format!("operator {sym} is not defined on type parameter {t}: its constraint does not allow it{hint}"), span);
        }
        r
    }

    /// True if a value of type parameter `src` converts to interface
    /// `target` (its constraint includes the interface's methods).
    pub(crate) fn param_implements(&self, src: TypeId, target: TypeId) -> bool {
        match self.bound_of(src) {
            Some(_) if matches!(self.tt.get(target), Type::Any) => true,
            Some(Bound::Iface(_)) => self.missing_for_bound(src, target).is_empty(),
            _ => false,
        }
    }

    /// Interface methods available on a value of type parameter `t`.
    pub(crate) fn param_methods(&self, t: TypeId) -> Option<Vec<hir::MethodSig>> {
        match self.bound_of(t)? {
            Bound::Iface(i) => Some(self.iface_methods(i)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mangle;

    #[test]
    fn mangling() {
        assert_eq!(mangle("app.Max<int>"), "zell_app__Max__int");
        assert_eq!(mangle("a.Pair<[]int, *a.T>.Swap"), "zell_a__Pair__Sint_Pa__T__Swap");
    }
}
