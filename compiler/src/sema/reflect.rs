//! Reflection (DESIGN.md "Рефлексия"): the Zell sources of `reflect`,
//! `encoding.json` and `strings.Builder`, their lazy loading, the
//! `typeof` builtin, compile-time reflection in generic bodies and the
//! summary handed to code generation ([`hir::Reflect`]).
//!
//! # Library sources
//!
//! The modules ([`MODULES`]) are written in Zell (`reflect_src/*.zl`),
//! parsed once per process and **loaded on first use**: the first lookup
//! of one of their names (`reflect.TypeOf`, `json.Parse`,
//! `strings.Builder`), `typeof`, or a builtin attribute used as a type
//! (`f.Attribute<json>()`). Loading registers their declarations as
//! ordinary stdlib declarations with a file scope of their own (index
//! `>= files.len()`, remapped to 0 in the finished program), resolves
//! them, and appends their functions to the checking queue; generic
//! declarations become stdlib templates (instantiated in the scope of the
//! user code, so they name other namespaces by full path). Programs that
//! never touch reflection load nothing and keep the minimal type infos.
//!
//! # Intrinsics
//!
//! A few `reflect` functions have placeholder bodies; code generation
//! emits their real bodies ([`hir::Intrinsic`], `codegen/reflect.rs`).
//!
//! # Compile-time reflection
//!
//! * `for f in reflect.Fields<T>() { body }` inside a generic function is
//!   unrolled when `T` is known: `body` is checked once per field of the
//!   struct `T` with `f` bound to that field's `*FieldInfo`; `f.Get(v)` is a
//!   direct field access of the field's static type. The copies are
//!   dispatched by a loop counter (`continue` / `break` keep their meaning).
//!   In the abstract check of a generic body (`T` a type parameter) the body
//!   is checked once and `f.Get(v)` has no type (no diagnostics follow).
//!   In an unrolled copy `f.Name`, `f.Index`, `f.Exported`, `f.Embedded`
//!   are constants and `f.Attribute<A>()` is `nil` or a pointer to the
//!   attribute's immortal value ([`ExprKind::AttrConst`]); `tag.name` of
//!   such a pointer is the argument's constant (or the field default),
//!   `tag != nil` a constant, and `==`/`!=` of two constants, `&&`/`||`
//!   with a constant left side fold too (`expr::make_binary`). A copy
//!   whose every use of `f` folded binds `f` to nil (no table lookup).
//! * A *static if*: an `if` whose condition is a compile-time type test
//!   (`reflect.TypeOf<T>().Kind == reflect.Kind.Struct`, optionally through
//!   `.Elem` / `.Key`, or `reflect.TypeOf<A>() == reflect.TypeOf<B>()`,
//!   combined with `!`, `&&`, `||`) keeps only the branch taken; the other
//!   one is not checked. In an abstract check both are checked with
//!   diagnostics muted.
//! * `m.Call(recv, args...)` on a `MethodInfo` boxes the arguments into a
//!   `[]any` for `MethodInfo.Invoke`.

use super::coerce::Ctx;
use super::generics::{Inst, TmplDecl};
use super::resolve::PathRes;
use super::*;
use crate::hir::{ExprKind, Intrinsic, Stmt, StmtKind};
use std::sync::OnceLock;

/// Library modules written in Zell: (namespace, file name, source, names
/// that trigger loading; empty = any member of the namespace).
pub(crate) const MODULES: &[(&str, &str, &str, &[&str])] = &[
    ("reflect", "reflect.zl", include_str!("reflect_src/reflect.zl"), &[]),
    ("encoding.json", "json.zl", include_str!("reflect_src/json.zl"), &[]),
    ("strings", "builder.zl", include_str!("reflect_src/builder.zl"), &["Builder"]),
];

const REFLECT: usize = 0;
const JSON: usize = 1;

/// Builtin attributes whose values are structs of `reflect`.
pub(crate) const ATTR_STRUCTS: &[&str] = &["json", "deprecated", "inline", "test", "log", "csv", "xml", "yaml", "toml", "serde"];

/// Builtin attributes whose value struct has another name in
/// `reflect.zl`: `@log`'s is `logAttr`, so that the root namespace
/// `log` and `import reflect` can be used together.
pub(crate) const ATTR_STRUCT_NAMES: &[(&str, &str)] = &[("log", "logAttr"), ("csv", "csvAttr"), ("xml", "xmlAttr"), ("yaml", "yamlAttr"), ("toml", "tomlAttr")];

/// Builtin type names whose canonical spelling differs (`int64` is `int`).
const ALIAS_SPELLINGS: &[&str] = &["int64", "uint", "uint8", "float64"];

/// Intrinsic functions of `reflect` (non-generic and generic).
const INTRINSIC_FUNCS: &[&str] = &["anyType", "anyAddr", "anyBox", "errType", "loadAddr", "newOf", "invokeMethod"];
const INTRINSIC_TEMPLATES: &[&str] = &["TypeOf", "ifaceType", "load", "store", "asFunc", "storeTag", "TypesWith"];

/// The parsed [`MODULES`] (parsed once per process).
fn module_asts() -> &'static [ast::File] {
    static ASTS: OnceLock<Vec<ast::File>> = OnceLock::new();
    ASTS.get_or_init(|| {
        MODULES
            .iter()
            .map(|(ns, name, embedded, _)| {
                let text = super::stdfs::load(ns, name, embedded);
                let src: &str = &text;
                let mut diags = crate::diagnostics::Diagnostics::new();
                let tokens = crate::lexer::lex(src, &mut diags);
                let file = crate::parser::parse(tokens, &mut diags);
                match file {
                    Some(f) if !diags.has_errors() && f.namespace.to_string() == *ns => f,
                    _ => {
                        let errs: Vec<String> = diags
                            .items
                            .iter()
                            .map(|d| format!("{name}:{}: {}", src[..(d.span.start as usize).min(src.len())].matches('\n').count() + 1, d.message))
                            .collect();
                        panic!("internal error: stdlib source {name} does not parse: {errs:?}")
                    }
                }
            })
            .collect()
    })
}

/// Reflection state of an analysis.
#[derive(Default)]
pub(crate) struct ReflState {
    /// Per module: loaded.
    loaded: Vec<bool>,
    /// Struct / enum / function / template ids of loaded modules by name.
    pub(crate) structs: HashMap<String, u32>,
    enums: HashMap<String, u32>,
    funcs: HashMap<String, FuncId>,
    templates: HashMap<String, u32>,
    /// Compile-time field bindings of unrolled `reflect.Fields<T>()` loops:
    /// (function-context depth, local, struct type, field index or `None`
    /// in an abstract check).
    ct_fields: Vec<(usize, LocalId, TypeId, Option<u32>)>,
    /// Per compile-time field variable (depth, local): references that were
    /// not folded away (`f.Type`, passing `f` on). A variable without any
    /// is bound to nil instead of the `FieldInfo`.
    ct_uses: HashMap<(usize, LocalId), u32>,
    /// Static types named by `typeof`.
    full: Vec<TypeId>,
    /// Set while resolving an attribute name (builtin attribute names must
    /// not resolve to their value structs there).
    pub in_attr: bool,
    /// Loaded modules with the number of functions before the load (to
    /// undo loads made inside a rolled-back abstract check).
    loads: Vec<(usize, usize)>,
    /// Namespace index of each module (filled on first lookup).
    module_ns: Vec<NsIdx>,
}

impl ReflState {
    /// `reflect` is loaded.
    pub(crate) fn loaded_reflect(&self) -> bool {
        self.loaded.first() == Some(&true)
    }

    /// Template id of a generic `reflect` function.
    pub(crate) fn template(&self, name: &str) -> Option<u32> {
        self.templates.get(name).copied()
    }

    /// Struct id of a `reflect` struct (or builtin attribute value struct).
    pub(crate) fn struct_id(&self, name: &str) -> Option<u32> {
        self.structs.get(name).copied()
    }

    /// Function id of a non-generic `reflect` function.
    pub(crate) fn func(&self, name: &str) -> Option<FuncId> {
        self.funcs.get(name).copied()
    }
}

/// Result of a compile-time condition.
#[derive(Clone, Copy, PartialEq)]
enum Ct {
    Known(bool),
    /// Depends on a type parameter (abstract check).
    Abstract,
}

impl<'a> Sema<'a> {
    // -- lazy loading ----------------------------------------------------------

    /// Loads the module that declares `name` in namespace `ns`, if any and
    /// not loaded yet. Called by every namespace member lookup.
    pub(crate) fn std_lookup_hook(&mut self, ns: NsIdx, name: &str) {
        if !self.nss[ns].is_std || self.files.is_empty() {
            return;
        }
        if self.refl.module_ns.is_empty() {
            self.refl.module_ns = MODULES.iter().map(|(path, ..)| self.ns_index.get(*path).copied().unwrap_or(usize::MAX)).collect();
        }
        for (m, module) in MODULES.iter().enumerate() {
            if self.refl.module_ns[m] != ns || self.refl.loaded.get(m).copied().unwrap_or(false) {
                continue;
            }
            let names = module.3;
            if names.is_empty() || names.contains(&name) {
                self.load_module(m);
            }
        }
    }

    /// [`std_lookup_hook`](Self::std_lookup_hook) for the glob imports of
    /// the current file (names found through `import reflect`).
    pub(crate) fn std_glob_hook(&mut self, name: &str) {
        if self.refl.loaded.len() == MODULES.len() && self.refl.loaded.iter().all(|l| *l) {
            return;
        }
        let n = self.file_scopes.get(self.cur_file).map_or(0, |s| s.imports.len());
        for i in 0..n {
            if let ImportEntry::Glob(ns) = self.file_scopes[self.cur_file].imports[i] {
                self.std_lookup_hook(ns, name);
            }
        }
    }

    /// Loads `reflect` (needed by `typeof` and attribute types).
    pub(crate) fn load_reflect(&mut self) {
        if !self.files.is_empty() {
            self.load_module(REFLECT);
        }
    }

    /// Loads every library module (the IDE index of the stdlib lists
    /// their members).
    pub(crate) fn load_all_modules(&mut self) {
        for m in 0..MODULES.len() {
            self.load_module(m);
        }
    }

    fn load_module(&mut self, m: usize) {
        if self.refl.loaded.len() < MODULES.len() {
            self.refl.loaded.resize(MODULES.len(), false);
        }
        if self.refl.loaded[m] {
            return;
        }
        self.refl.loaded[m] = true;
        if m == JSON {
            self.load_module(REFLECT);
        }
        let file: &'static ast::File = &module_asts()[m];
        let ns = self.ensure_ns(MODULES[m].0, Span::DUMMY);
        if self.nss[ns].hir_id.is_none() {
            let n = self.nss.iter().filter(|n| n.hir_id.is_some()).count() as u32;
            self.nss[ns].hir_id = Some(hir::NamespaceId(n));
        }
        self.refl.loads.push((m, self.funcs.len()));
        let fi = self.file_scopes.len();
        self.file_scopes.push(FileScope { ns, imports: Vec::new(), import_uses: Vec::new() });
        let ide = self.ide.take();
        let mute = std::mem::take(&mut self.gen.mute);
        self.in_file(fi, |s| s.register_module(file, ns, fi));
        self.gen.mute = mute;
        self.ide = ide;
    }

    /// Undoes the loads made after a mark of `funcs` functions (the
    /// abstract check of a generic body is rolled back; the module is
    /// loaded again on its next use).
    pub(crate) fn reflect_rollback(&mut self, funcs: usize) {
        while let Some(&(m, f)) = self.refl.loads.last() {
            if f < funcs {
                break;
            }
            self.refl.loads.pop();
            self.refl.loaded[m] = false;
            let ns = self.ns_index[MODULES[m].0];
            for d in &module_asts()[m].decls {
                let name = match d {
                    ast::Decl::Struct(s) => &s.name.name,
                    ast::Decl::Enum(e) => &e.name.name,
                    ast::Decl::Func(f) => &f.name.name,
                    _ => continue,
                };
                self.nss[ns].members.remove(name);
                if m == REFLECT {
                    self.refl.structs.remove(name);
                    if let Some((attr, _)) = ATTR_STRUCT_NAMES.iter().find(|(_, n)| n == name) {
                        self.refl.structs.remove(*attr);
                    }
                    self.refl.enums.remove(name);
                    self.refl.funcs.remove(name);
                    self.refl.templates.remove(name);
                }
            }
        }
    }

    /// Registers and resolves the declarations of a module.
    fn register_module(&mut self, file: &'a ast::File, ns: NsIdx, fi: usize) {
        let hns = self.nss[ns].hir_id.unwrap();
        let path = self.nss[ns].path.clone();
        let (mut structs, mut enums, mut funcs) = (Vec::new(), Vec::new(), Vec::new());
        for d in &file.decls {
            match d {
                ast::Decl::Struct(s) if s.generics.is_empty() => {
                    let id = self.structs.len() as u32;
                    self.structs.push(hir::Struct {
                        name: s.name.name.clone(),
                        full_name: format!("{path}.{}", s.name.name),
                        namespace: hns,
                        fields: Vec::new(),
                        methods: Vec::new(),
                        attrs: Vec::new(),
                        is_attribute: false,
                        exported: is_exported(&s.name.name),
                        file: fi,
                        span: s.span,
                    });
                    self.struct_src.push(s);
                    self.nss[ns].members.insert(s.name.name.clone(), (Member::Struct(id), Span::DUMMY, fi));
                    if path == "reflect" {
                        self.refl.structs.insert(s.name.name.clone(), id);
                        if let Some((attr, _)) = ATTR_STRUCT_NAMES.iter().find(|(_, n)| *n == s.name.name) {
                            self.refl.structs.insert(attr.to_string(), id);
                        }
                    }
                    structs.push(id as usize);
                    for m in &s.methods {
                        if !m.generics.is_empty() {
                            let t = self.new_template(&m.name.name, TmplDecl::Method(m, id), &m.generics, Vec::new(), false);
                            self.gen.methods.insert((id, m.name.name.clone()), t);
                            continue;
                        }
                        let kind = hir::FuncKind::Method { struct_id: id, is_static: m.is_static };
                        let fid = self.new_func(m, kind, FuncSrc::Method(m, id), &format!("{}.{}", s.name.name, m.name.name));
                        self.structs[id as usize].methods.push((m.name.name.clone(), fid));
                        funcs.push(fid.0 as usize);
                    }
                }
                ast::Decl::Enum(e) if e.generics.is_empty() && e.methods.is_empty() => {
                    let id = self.enums.len() as u32;
                    self.enums.push(hir::Enum {
                        name: e.name.name.clone(),
                        full_name: format!("{path}.{}", e.name.name),
                        namespace: hns,
                        variants: Vec::new(),
                        is_plain: e.variants.iter().all(|v| v.fields.is_empty()),
                        methods: Vec::new(),
                        attrs: Vec::new(),
                        exported: is_exported(&e.name.name),
                        file: fi,
                        span: e.span,
                    });
                    self.enum_src.push(e);
                    self.nss[ns].members.insert(e.name.name.clone(), (Member::Enum(id), Span::DUMMY, fi));
                    if path == "reflect" {
                        self.refl.enums.insert(e.name.name.clone(), id);
                    }
                    enums.push(id as usize);
                }
                ast::Decl::Func(f) if !f.generics.is_empty() => {
                    let t = self.new_template(&f.name.name, TmplDecl::Func(f), &f.generics, Vec::new(), false);
                    self.nss[ns].members.insert(f.name.name.clone(), (Member::Generic(t), Span::DUMMY, fi));
                    if path == "reflect" {
                        self.refl.templates.insert(f.name.name.clone(), t);
                    }
                }
                ast::Decl::Func(f) => {
                    let fid = self.new_func(f, hir::FuncKind::Free, FuncSrc::Free(f), &f.name.name);
                    self.nss[ns].members.insert(f.name.name.clone(), (Member::Func(fid), Span::DUMMY, fi));
                    if path == "reflect" {
                        self.refl.funcs.insert(f.name.name.clone(), fid);
                    }
                    funcs.push(fid.0 as usize);
                }
                _ => panic!("internal error: unsupported declaration in stdlib source {path}"),
            }
        }
        for &sid in &structs {
            self.resolve_struct_fields(sid);
            self.check_recursive_struct(sid);
        }
        for &eid in &enums {
            self.resolve_enum_variants(eid);
        }
        for &fid in &funcs {
            self.resolve_func_sig(fid);
        }
        // Field defaults (normally evaluated with the attributes).
        for &sid in &structs {
            let src = self.struct_src[sid];
            for f in &src.fields {
                let Some(d) = &f.default else { continue };
                let Some(idx) = self.structs[sid].fields.iter().position(|x| x.name == f.name.name) else { continue };
                let ty = self.structs[sid].fields[idx].ty;
                let v = self.attr_value(d, ty);
                self.structs[sid].fields[idx].default = v;
            }
        }
    }

    /// True if `sid` is the value struct of a builtin attribute (`json`):
    /// its lowercase fields are readable everywhere.
    pub(crate) fn is_attr_value_struct(&self, sid: u32) -> bool {
        ATTR_STRUCTS.iter().any(|n| self.refl.structs.get(*n) == Some(&sid))
    }

    /// The builtin attribute whose value struct is `sid` (`json`).
    pub(crate) fn attr_value_struct_name(&self, sid: u32) -> Option<&'static str> {
        ATTR_STRUCTS.iter().copied().find(|n| self.refl.structs.get(*n) == Some(&sid))
    }

    /// A builtin attribute used as a type (`f.Attribute<json>()`): its
    /// value struct in `reflect`.
    pub(crate) fn builtin_attr_type(&mut self, name: &str) -> Option<u32> {
        if self.refl.in_attr || !ATTR_STRUCTS.contains(&name) {
            return None;
        }
        self.load_reflect();
        self.refl.structs.get(name).copied()
    }

    fn refl_struct_ty(&mut self, name: &str) -> TypeId {
        match self.refl.structs.get(name).copied() {
            Some(s) => self.struct_ty(s),
            None => self.tt.invalid,
        }
    }

    /// `*reflect.TypeInfo`.
    fn type_info_ptr(&mut self) -> TypeId {
        let t = self.refl_struct_ty("TypeInfo");
        self.tt.pointer(t)
    }

    /// A call of `reflect.TypeOf<T>()`.
    fn type_of_call(&mut self, t: TypeId, span: Span) -> hir::Expr {
        let ptr = self.type_info_ptr();
        let Some(&tmpl) = self.refl.templates.get("TypeOf") else { return self.invalid(span) };
        match self.instantiate(tmpl, vec![t], span) {
            Some(Inst::Func(f)) => mk(ExprKind::Call { callee: hir::Callee::Func(f), args: Vec::new() }, ptr, span),
            _ => self.invalid(span),
        }
    }

    // -- typeof ----------------------------------------------------------------

    /// `typeof(x)`: the static type of `x` (not evaluated), or the dynamic
    /// type of an interface, `any` or `error` value (nil for nil).
    pub(crate) fn typeof_builtin(&mut self, args: &[ast::Expr], span: Span) -> hir::Expr {
        if args.len() != 1 {
            self.err(format!("wrong number of arguments in call to typeof: expected 1, got {}", args.len()), span);
            self.check_args_loose(args);
            return self.invalid(span);
        }
        self.load_reflect();
        let v = self.value(&args[0], None);
        let v = self.default_type(v);
        let ptr = self.type_info_ptr();
        let call = |f: FuncId, v: hir::Expr| mk(ExprKind::Call { callee: hir::Callee::Func(f), args: vec![v] }, ptr, span);
        match self.tt.get(v.ty).clone() {
            Type::Invalid => self.invalid(span),
            Type::Any => match self.refl.funcs.get("anyType").copied() {
                Some(f) => call(f, v),
                None => self.invalid(span),
            },
            Type::Error => match self.refl.funcs.get("errType").copied() {
                Some(f) => call(f, v),
                None => self.invalid(span),
            },
            Type::Interface(_) => {
                let Some(&t) = self.refl.templates.get("ifaceType") else { return self.invalid(span) };
                match self.instantiate(t, vec![v.ty], span) {
                    Some(Inst::Func(f)) => call(f, v),
                    _ => self.invalid(span),
                }
            }
            Type::Param(_) => mk(ExprKind::Nil, ptr, span),
            _ => {
                if !self.has_param(v.ty) {
                    self.refl.full.push(v.ty);
                }
                self.type_of_call(v.ty, span)
            }
        }
    }

    // -- generic calls and methods ----------------------------------------------

    /// `reflect.Fields<T>()` outside a `for` loop is an error.
    pub(crate) fn reflect_generic_call(&mut self, t: u32, span: Span) -> Option<hir::Expr> {
        if self.refl.templates.get("Fields") != Some(&t) {
            return None;
        }
        self.err("reflect.Fields<T>() can only be the sequence of a for loop in a generic function (it is unrolled at compile time); use reflect.TypeOf<T>().Fields at run time", span);
        Some(self.invalid(span))
    }

    /// Method calls with special meaning: `f.Get(v)` on a compile-time
    /// field and `m.Call(recv, args...)` on a `MethodInfo`. Gives `base`
    /// back otherwise.
    pub(crate) fn reflect_method_call(&mut self, base: hir::Expr, name: &ast::Ident, targs: Option<&[ast::TypeExpr]>, args: &[ast::Expr], span: Span) -> Result<hir::Expr, hir::Expr> {
        if self.refl.loaded.first() != Some(&true) {
            return Err(base);
        }
        if let ExprKind::Local(l) = base.kind {
            let depth = self.fns.len();
            let ct = self.refl.ct_fields.iter().rev().find(|c| c.0 == depth && c.1 == l).copied();
            if let (Some((_, _, sty, idx)), "Get", None) = (ct, name.name.as_str(), targs) {
                self.fcx().used[l.0 as usize] = true;
                self.ct_field_use(l, -1);
                if args.len() != 1 {
                    self.err(format!("f.Get takes one argument (the struct value), got {}", args.len()), span);
                    self.check_args_loose(args);
                    return Ok(self.invalid(span));
                }
                let v = self.value(&args[0], None);
                let Some(k) = idx else { return Ok(self.invalid(span)) };
                let v = self.auto_deref(v);
                if self.tt.is_invalid(v.ty) {
                    return Ok(self.invalid(span));
                }
                if v.ty != sty {
                    self.err(format!("f.Get needs a value of type {} (the type of reflect.Fields<{}>()), found {}", self.tn(sty), self.tn(sty), self.tn(v.ty)), v.span);
                    return Ok(self.invalid(span));
                }
                let Type::Struct(d) = *self.tt.get(sty) else { return Ok(self.invalid(span)) };
                let fty = self.structs[d.0 as usize].fields[k as usize].ty;
                return Ok(mk(ExprKind::Field { base: Box::new(v), index: k }, fty, span));
            }
            // `f.Attribute<A>()` of a compile-time field: the attribute is
            // known, so the result is a constant (`nil` or a pointer to the
            // immortal attribute value), and `tag != nil` / `tag.name`
            // fold at compile time.
            if let (Some((_, _, sty, Some(k))), "Attribute", Some([a])) = (ct, name.name.as_str(), targs) {
                if args.is_empty() {
                    let aty = self.resolve_type(a);
                    if self.tt.is_invalid(aty) {
                        return Ok(self.invalid(span));
                    }
                    if let Type::Struct(ad) = *self.tt.get(aty) {
                        self.fcx().used[l.0 as usize] = true;
                        self.ct_field_use(l, -1);
                        return Ok(self.ct_attribute(sty, k, ad.0, aty, span));
                    }
                }
            }
        }
        let Some(&mi) = self.refl.structs.get("MethodInfo") else { return Err(base) };
        if name.name != "Call" || targs.is_some() || self.struct_of(base.ty) != Some(mi) {
            return Err(base);
        }
        let Some(invoke) = self.structs[mi as usize].methods.iter().find(|m| m.0 == "Invoke").map(|m| m.1) else { return Err(base) };
        if args.is_empty() {
            self.err("MethodInfo.Call needs the receiver as its first argument (nil for a static method)", span);
            return Ok(self.invalid(span));
        }
        let recv = if matches!(self.tt.get(base.ty), Type::Pointer(_)) { base } else { self.addr_of(base) };
        let any = self.tt.any;
        let first = self.expr_to(&args[0], any, Ctx::Arg(0, "MethodInfo.Call".into()));
        let rest: Vec<hir::Expr> = args[1..].iter().enumerate().map(|(i, a)| self.expr_to(a, any, Ctx::Arg(i + 1, "MethodInfo.Call".into()))).collect();
        let sl = self.tt.slice(any);
        let list = mk(ExprKind::SliceLit(rest), sl, span);
        let ret = self.tt.fallible(any);
        Ok(mk(ExprKind::Call { callee: hir::Callee::Func(invoke), args: vec![recv, first, list] }, ret, span))
    }

    /// Adds `delta` to the unfolded-use count of `l` if it is a compile-time
    /// field variable of the current function (see `ReflState::ct_uses`).
    pub(crate) fn ct_field_use(&mut self, l: LocalId, delta: i32) {
        if self.refl.ct_fields.is_empty() {
            return;
        }
        let depth = self.fns.len();
        if self.refl.ct_fields.iter().any(|c| c.0 == depth && c.1 == l) {
            let n = self.refl.ct_uses.entry((depth, l)).or_insert(0);
            *n = (*n as i64 + delta as i64).max(0) as u32;
        }
    }

    /// The constant value of `f.Attribute<A>()` for field `k` of struct
    /// `sty`, `A` being `structs[asid]` (type `aty`): a pointer to the first
    /// attribute of that type on the field, or `nil`.
    fn ct_attribute(&mut self, sty: TypeId, k: u32, asid: u32, aty: TypeId, span: Span) -> hir::Expr {
        let ptr = self.tt.pointer(aty);
        let Type::Struct(sd) = *self.tt.get(sty) else { return mk(ExprKind::Nil, ptr, span) };
        let attrs = self.structs[sd.0 as usize].fields.get(k as usize).map(|f| f.attrs.clone()).unwrap_or_default();
        for a in attrs {
            let sid = match &a.kind {
                hir::AttrKind::User(s) => Some(*s),
                // Builtin value structs are registered under the attribute
                // name (`log` for `logAttr`).
                hir::AttrKind::Builtin(n) => self.refl.structs.get(n.as_str()).copied(),
            };
            if sid == Some(asid) {
                return mk(ExprKind::AttrConst { attr: Box::new(a), sid: asid }, ptr, span);
            }
        }
        mk(ExprKind::Nil, ptr, span)
    }

    /// `tag.<field>` of a compile-time attribute value (`ExprKind::AttrConst`
    /// of struct `sid`): the argument given in the attribute, else the
    /// field's default, else the zero value; `None` when the field is not a
    /// plain scalar or string (then the value object is read at run time).
    pub(crate) fn attr_const_field(&mut self, attr: &hir::Attr, sid: u32, field: &str, span: Span) -> Option<hir::Expr> {
        let f = self.structs.get(sid as usize)?.fields.iter().find(|f| f.name == field)?;
        let (fty, default) = (f.ty, f.default.clone());
        let v = attr.args.iter().find(|(n, _)| n == field).map(|(_, v)| v.clone()).or(default);
        let v = match (v, self.tt.get(fty)) {
            (Some(v @ (hir::ConstValue::Int(_) | hir::ConstValue::Float(_) | hir::ConstValue::Bool(_) | hir::ConstValue::Str(_) | hir::ConstValue::Char(_))), _) => v,
            (None, Type::String) => hir::ConstValue::Str(String::new()),
            (None, Type::Bool) => hir::ConstValue::Bool(false),
            (None, Type::Int(_)) => hir::ConstValue::Int(0),
            (None, Type::Float64 | Type::Float32) => hir::ConstValue::Float(0.0),
            _ => return None,
        };
        Some(mk(ExprKind::Const(v), fty, span))
    }

    /// `f.Name`, `f.Index`, `f.Exported`, `f.Embedded` of a compile-time
    /// field `l` (an unrolled `reflect.Fields<T>()` variable): constants.
    /// `None` for other fields or variables.
    pub(crate) fn ct_field_const(&mut self, l: LocalId, name: &ast::Ident) -> Option<hir::Expr> {
        if self.refl.loaded.first() != Some(&true) {
            return None;
        }
        let depth = self.fns.len();
        let (_, _, sty, Some(k)) = self.refl.ct_fields.iter().rev().find(|c| c.0 == depth && c.1 == l).copied()? else { return None };
        let Type::Struct(sd) = *self.tt.get(sty) else { return None };
        let field = self.structs[sd.0 as usize].fields.get(k as usize)?;
        let (fname, exported, embedded) = (field.name.clone(), field.exported, field.embedded);
        let span = name.span;
        let e = match name.name.as_str() {
            "Name" => mk(ExprKind::Const(hir::ConstValue::Str(fname)), self.tt.string, span),
            "Index" => mk(ExprKind::Const(hir::ConstValue::Int(k as i128)), self.tt.int, span),
            "Exported" => mk(ExprKind::Const(hir::ConstValue::Bool(exported)), self.tt.bool_, span),
            "Embedded" => mk(ExprKind::Const(hir::ConstValue::Bool(embedded)), self.tt.bool_, span),
            _ => return None,
        };
        self.fcx().used[l.0 as usize] = true;
        self.ct_field_use(l, -1);
        Some(e)
    }

    // -- compile-time reflection ------------------------------------------------

    /// `T` of a call `reflect.TypeOf<T>()` (or `Fields<T>()` when
    /// `template` says so), if `e` is one.
    fn reflect_targ(&mut self, e: &ast::Expr, template: &str) -> Option<TypeId> {
        let ast::ExprKind::Call { callee, args } = &expr::strip_parens(e).kind else { return None };
        if !args.is_empty() {
            return None;
        }
        let ast::ExprKind::TypeArgs { base, args: targs } = &expr::strip_parens(callee).kind else { return None };
        if targs.len() != 1 {
            return None;
        }
        let segs = Self::flatten_path(base)?;
        if segs.last().map(|s| s.name.as_str()) != Some(template) || self.lookup_local(&segs[0].name, false).is_some() {
            return None;
        }
        let (res, n) = self.resolve_segments(&segs, false)?;
        let PathRes::Member(Member::Generic(t)) = res else { return None };
        if n != segs.len() || self.refl.templates.get(template) != Some(&t) {
            return None;
        }
        Some(self.resolve_type(&targs[0]))
    }

    /// The type a `.Elem` / `.Key` step leads to.
    fn type_step(&self, t: TypeId, step: &str) -> Option<TypeId> {
        match (self.tt.get(t), step) {
            (Type::Pointer(x) | Type::Weak(x) | Type::Slice(x) | Type::Array(x, _) | Type::Fallible(x) | Type::Task(x), "Elem") => Some(*x),
            (Type::Map(_, v), "Elem") => Some(*v),
            (Type::Map(k, _), "Key") => Some(*k),
            _ => None,
        }
    }

    /// `TypeOf<T>()[.Elem|.Key]*.Kind`: `Some(Some(kind name))`, or
    /// `Some(None)` when it depends on a type parameter.
    fn ct_kind(&mut self, e: &ast::Expr) -> Option<Option<&'static str>> {
        let ast::ExprKind::Field { base, name } = &expr::strip_parens(e).kind else { return None };
        if name.name != "Kind" {
            return None;
        }
        let mut steps = Vec::new();
        let mut cur: &ast::Expr = base;
        while let ast::ExprKind::Field { base: b, name: n } = &expr::strip_parens(cur).kind {
            if n.name != "Elem" && n.name != "Key" {
                return None;
            }
            steps.push(n.name.clone());
            cur = b;
        }
        let mut t = self.reflect_targ(cur, "TypeOf")?;
        if self.has_param(t) {
            return Some(None);
        }
        for s in steps.iter().rev() {
            match self.type_step(t, s) {
                Some(x) => t = x,
                None => return Some(Some("Invalid")),
            }
        }
        Some(Some(self.kind_name(t)))
    }

    /// The `reflect.Kind` variant named by `e` (`reflect.Kind.Struct`).
    fn ct_kind_const(&mut self, e: &ast::Expr) -> Option<String> {
        let segs = Self::flatten_path(expr::strip_parens(e))?;
        if self.lookup_local(&segs[0].name, false).is_some() {
            return None;
        }
        let (res, n) = self.resolve_segments(&segs, false)?;
        match res {
            PathRes::Variant(en, v) if n == segs.len() && self.refl.enums.get("Kind") == Some(&en) => Some(self.enums[en as usize].variants[v as usize].name.clone()),
            _ => None,
        }
    }

    /// Evaluates a compile-time type test, `None` if `e` is not one.
    fn ct_cond(&mut self, e: &ast::Expr) -> Option<Ct> {
        use ast::BinaryOp as B;
        match &e.kind {
            ast::ExprKind::Paren(x) => self.ct_cond(x),
            ast::ExprKind::Unary { op: ast::UnaryOp::Not, expr } => Some(match self.ct_cond(expr)? {
                Ct::Known(b) => Ct::Known(!b),
                Ct::Abstract => Ct::Abstract,
            }),
            ast::ExprKind::Binary { op: op @ (B::And | B::Or), left, right } => {
                let l = self.ct_cond(left)?;
                let r = self.ct_cond(right)?;
                Some(match (l, r) {
                    (Ct::Known(a), Ct::Known(b)) => Ct::Known(if *op == B::And { a && b } else { a || b }),
                    _ => Ct::Abstract,
                })
            }
            ast::ExprKind::Binary { op: op @ (B::Eq | B::Ne), left, right } => {
                let neg = *op == B::Ne;
                let fin = |b: bool| Ct::Known(b != neg);
                // TypeOf<A>() == TypeOf<B>()
                if let (Some(a), Some(b)) = (self.reflect_targ_quiet(left), self.reflect_targ_quiet(right)) {
                    if self.has_param(a) || self.has_param(b) {
                        return Some(Ct::Abstract);
                    }
                    return Some(fin(a == b));
                }
                let (k, c) = match self.ct_kind(left) {
                    Some(k) => (k, self.ct_kind_const(right)?),
                    None => (self.ct_kind(right)?, self.ct_kind_const(left)?),
                };
                Some(match k {
                    Some(k) => fin(k == c),
                    None => Ct::Abstract,
                })
            }
            _ => None,
        }
    }

    /// [`reflect_targ`](Self::reflect_targ) for `TypeOf` that only resolves
    /// when the shape matches.
    fn reflect_targ_quiet(&mut self, e: &ast::Expr) -> Option<TypeId> {
        self.reflect_targ(e, "TypeOf")
    }

    /// A static `if` (see the module documentation); `None` for an
    /// ordinary `if`.
    pub(crate) fn static_if(&mut self, i: &ast::IfStmt) -> Option<StmtKind> {
        let c = self.ct_cond(&i.cond)?;
        let else_block = |s: &mut Self| -> Option<hir::Block> {
            i.else_.as_ref().map(|e| match &**e {
                ast::ElseBranch::Block(b) => s.block(b),
                ast::ElseBranch::If(ei) => {
                    let k = s.if_stmt(ei);
                    hir::Block { stmts: vec![Stmt { kind: k, span: ei.span }], span: ei.span }
                }
            })
        };
        Some(match c {
            Ct::Known(true) => StmtKind::Block(self.block(&i.then)),
            Ct::Known(false) => match else_block(self) {
                Some(b) => StmtKind::Block(b),
                None => StmtKind::Block(hir::Block::default()),
            },
            Ct::Abstract => {
                self.gen.mute += 1;
                let then = self.block(&i.then);
                let else_ = else_block(self);
                self.gen.mute -= 1;
                let cond = mk(ExprKind::Const(hir::ConstValue::Bool(false)), self.tt.bool_, i.cond.span);
                StmtKind::If { cond, then, else_ }
            }
        })
    }

    /// True inside the body of a generic function instance (or its lambdas).
    fn in_generic_body(&self) -> bool {
        self.fns.iter().any(|f| f.func.is_some_and(|fid| self.gen.insts.contains_key(&fid.0)))
    }

    /// `for f in reflect.Fields<T>() { body }`; `None` if `iter` is no such
    /// call.
    pub(crate) fn fields_for(&mut self, vars: &[ast::Ident], iter: &ast::Expr, body: &ast::Block, span: Span) -> Option<StmtKind> {
        if self.refl.loaded.first() != Some(&true) {
            // `reflect.Fields` resolves (and loads reflect) below.
            let ast::ExprKind::Call { callee, .. } = &expr::strip_parens(iter).kind else { return None };
            if !matches!(expr::strip_parens(callee).kind, ast::ExprKind::TypeArgs { .. }) {
                return None;
            }
        }
        let t = self.reflect_targ(iter, "Fields")?;
        let empty = || StmtKind::Block(hir::Block::default());
        if vars.len() != 1 {
            self.err("for ... in reflect.Fields<T>() takes one variable", span);
            return Some(empty());
        }
        if !self.in_generic_body() {
            self.err("reflect.Fields<T>() can only be used in a generic function (it is unrolled at instantiation); use reflect.TypeOf<T>().Fields at run time", iter.span);
            return Some(empty());
        }
        if self.tt.is_invalid(t) {
            return Some(empty());
        }
        let abstract_ = self.has_param(t);
        let nfields = match *self.tt.get(t) {
            Type::Struct(d) if !abstract_ => self.structs[d.0 as usize].fields.len(),
            _ if abstract_ => 0,
            _ => {
                self.err(format!("reflect.Fields<T>() needs a struct type, {} is not a struct", self.tn(t)), iter.span);
                return Some(empty());
            }
        };
        let fi_ptr = {
            let s = self.refl_struct_ty("FieldInfo");
            self.tt.pointer(s)
        };
        let int = self.tt.int;
        let var = &vars[0];
        if abstract_ || nfields == 0 {
            // Checked once without a field (diagnostics muted for an empty
            // concrete struct: the body is not emitted).
            if !abstract_ {
                self.gen.mute += 1;
            }
            self.push_scope();
            let label = self.push_label(LabelKind::Loop);
            let f = self.declare_local(&var.name, fi_ptr, LocalKind::LoopVar, var.span);
            let depth = self.fns.len();
            self.refl.ct_fields.push((depth, f, t, None));
            let mut b = self.block(body);
            self.refl.ct_fields.pop();
            self.pop_label();
            self.pop_scope();
            if !abstract_ {
                self.gen.mute -= 1;
                return Some(empty());
            }
            b.stmts.insert(0, Stmt { kind: StmtKind::Let { local: f, init: Some(mk(ExprKind::Nil, fi_ptr, var.span)) }, span: var.span });
            let zero = mk(ExprKind::Const(hir::ConstValue::Int(0)), int, span);
            return Some(StmtKind::ForRange { label, var: None, start: zero.clone(), end: zero, body: b });
        }
        let idx = self.temp(int, span);
        let label = self.push_label(LabelKind::Loop);
        let mut copies = Vec::new();
        for k in 0..nfields {
            self.push_scope();
            let f = self.declare_local(&var.name, fi_ptr, LocalKind::LoopVar, var.span);
            let depth = self.fns.len();
            self.refl.ct_fields.push((depth, f, t, Some(k as u32)));
            self.refl.ct_uses.remove(&(depth, f));
            let b = self.block(body);
            self.refl.ct_fields.pop();
            let dynamic_uses = self.refl.ct_uses.remove(&(depth, f)).unwrap_or(0);
            self.pop_scope();
            let init = if dynamic_uses == 0 {
                // Every use of f was folded (f.Get, f.Name, f.Attribute<A>()):
                // no table lookup, nothing to retain.
                mk(ExprKind::Nil, fi_ptr, var.span)
            } else {
                // f = TypeOf<T>().Fields[k]
                let ti = self.type_of_call(t, var.span);
                let ti_struct = self.refl_struct_ty("TypeInfo");
                let fields_idx = self.refl.structs.get("TypeInfo").map(|s| self.structs[*s as usize].fields.iter().position(|x| x.name == "Fields").unwrap_or(0)).unwrap_or(0) as u32;
                let sl = self.tt.slice(fi_ptr);
                let deref = mk(ExprKind::Deref(Box::new(ti)), ti_struct, var.span);
                let fields = mk(ExprKind::Field { base: Box::new(deref), index: fields_idx }, sl, var.span);
                let kexpr = mk(ExprKind::Const(hir::ConstValue::Int(k as i128)), int, var.span);
                mk(ExprKind::Index { base: Box::new(fields), index: Box::new(kexpr) }, fi_ptr, var.span)
            };
            let stmts = vec![Stmt { kind: StmtKind::Let { local: f, init: Some(init) }, span: var.span }, Stmt { kind: StmtKind::Block(b), span: body.span }];
            copies.push(hir::Block { stmts, span: body.span });
        }
        self.pop_label();
        // if idx == 0 { copy0 } else if idx == 1 { copy1 } ...
        let mut chain: Option<hir::Block> = None;
        for (k, copy) in copies.into_iter().enumerate().rev() {
            let l = mk(ExprKind::Local(idx), int, span);
            let r = mk(ExprKind::Const(hir::ConstValue::Int(k as i128)), int, span);
            let cond = mk(ExprKind::Binary { op: hir::BinOp::Eq, lhs: Box::new(l), rhs: Box::new(r) }, self.tt.bool_, span);
            let st = Stmt { kind: StmtKind::If { cond, then: copy, else_: chain.take() }, span };
            chain = Some(hir::Block { stmts: vec![st], span });
        }
        let start = mk(ExprKind::Const(hir::ConstValue::Int(0)), int, span);
        let end = mk(ExprKind::Const(hir::ConstValue::Int(nfields as i128)), int, span);
        Some(StmtKind::ForRange { label, var: Some(idx), start, end, body: chain.unwrap_or_default() })
    }

    /// Name of the `reflect.Kind` variant of `t`.
    pub(crate) fn kind_name(&self, t: TypeId) -> &'static str {
        hir::reflect_kind_name(&self.tt, t)
    }

    // -- result ------------------------------------------------------------------

    /// The reflection summary for code generation (`None` if `reflect`
    /// was never loaded). Also maps the file indices of library sources to
    /// file 0 (they are not user files).
    pub(crate) fn reflect_finish(&mut self) -> Option<hir::Reflect> {
        let tw = if self.refl.loaded_reflect() { self.refl.template("TypesWith") } else { None };
        let types_with = self.types_with_finish(tw);
        let nfiles = self.files.len();
        for f in &mut self.funcs {
            if f.file >= nfiles {
                f.file = 0;
            }
        }
        for s in &mut self.structs {
            if s.file >= nfiles {
                s.file = 0;
            }
        }
        for e in &mut self.enums {
            if e.file >= nfiles {
                e.file = 0;
            }
        }
        if self.refl.loaded.first() != Some(&true) {
            return None;
        }
        let st = |n: &str| self.refl.structs.get(n).copied().unwrap_or(0);
        let mut r = hir::Reflect {
            type_info: st("TypeInfo"),
            field_info: st("FieldInfo"),
            method_info: st("MethodInfo"),
            variant_info: st("VariantInfo"),
            kind: self.refl.enums.get("Kind").copied().unwrap_or(0),
            attr_structs: ATTR_STRUCTS.iter().filter_map(|n| self.refl.structs.get(*n).map(|s| (n.to_string(), *s))).collect(),
            full: self.refl.full.clone(),
            intrinsics: Vec::new(),
            field_spellings: Vec::new(),
            pointers: Vec::new(),
            method_types: Vec::new(),
            param_info: st("ParamInfo"),
            types_with,
        };
        // Types listed by TypesWith are full (methods, ptrTo).
        for (_, ts) in &r.types_with {
            r.full.extend(ts.iter().copied());
        }
        for n in INTRINSIC_FUNCS {
            if let Some(&f) = self.refl.funcs.get(*n) {
                let k = match *n {
                    "anyType" => Intrinsic::AnyType,
                    "anyAddr" => Intrinsic::AnyAddr,
                    "anyBox" => Intrinsic::AnyBox,
                    "errType" => Intrinsic::ErrType,
                    "loadAddr" => Intrinsic::LoadAddr,
                    "newOf" => Intrinsic::New,
                    _ => Intrinsic::Invoke,
                };
                r.intrinsics.push((f, k));
            }
        }
        let tmpls: Vec<(u32, &str)> = INTRINSIC_TEMPLATES.iter().filter_map(|n| self.refl.templates.get(*n).map(|t| (*t, *n))).collect();
        let mut insts: Vec<(FuncId, Intrinsic)> = Vec::new();
        for ((t, args), inst) in &self.gen.cache {
            let (Some((_, name)), Inst::Func(f)) = (tmpls.iter().find(|(x, _)| x == t), inst) else { continue };
            let a = args.first().copied().unwrap_or(self.tt.void);
            let k = match *name {
                "TypeOf" => {
                    r.full.push(a);
                    Intrinsic::TypeOf(a)
                }
                "ifaceType" => Intrinsic::IfaceType,
                "load" => Intrinsic::Load(a),
                "store" => Intrinsic::Store(a),
                "asFunc" => Intrinsic::AsFunc(a),
                "TypesWith" => Intrinsic::TypesWith(a),
                _ => Intrinsic::StoreTag(a),
            };
            insts.push((*f, k));
        }
        insts.sort_by_key(|(f, _)| *f);
        r.intrinsics.extend(insts);
        // @reflect types keep their methods.
        for (i, s) in self.structs.iter().enumerate() {
            if s.attrs.iter().any(|a| a.kind == hir::AttrKind::Builtin("reflect".into())) {
                let t = self.tt.intern(Type::Struct(crate::types::DeclId(i as u32)));
                r.full.push(t);
            }
        }
        for (i, e) in self.enums.iter().enumerate() {
            if e.attrs.iter().any(|a| a.kind == hir::AttrKind::Builtin("reflect".into())) {
                let t = self.tt.intern(Type::Enum(crate::types::DeclId(i as u32)));
                r.full.push(t);
            }
        }
        r.full.sort();
        r.full.dedup();
        // `*T` of full types and attribute structs; method signatures.
        let mut ptr_of: Vec<TypeId> = r.full.clone();
        let mut attr_sids: Vec<u32> = Vec::new();
        let mut collect = |attrs: &[hir::Attr]| {
            for a in attrs {
                match &a.kind {
                    hir::AttrKind::User(s) => attr_sids.push(*s),
                    hir::AttrKind::Builtin(n) => {
                        if let Some(s) = self.refl.structs.get(n.as_str()) {
                            if ATTR_STRUCTS.contains(&n.as_str()) {
                                attr_sids.push(*s);
                            }
                        }
                    }
                }
            }
        };
        for s in &self.structs {
            collect(&s.attrs);
            for f in &s.fields {
                collect(&f.attrs);
            }
        }
        for e in &self.enums {
            collect(&e.attrs);
            for v in &e.variants {
                collect(&v.attrs);
            }
        }
        for f in &self.funcs {
            collect(&f.attrs);
            for p in &f.param_attrs {
                collect(p);
            }
        }
        for s in attr_sids {
            ptr_of.push(self.tt.intern(Type::Struct(crate::types::DeclId(s))));
        }
        ptr_of.sort();
        ptr_of.dedup();
        for t in ptr_of {
            let p = self.tt.pointer(t);
            r.pointers.push((t, p));
        }
        for &t in &r.full.clone() {
            let Type::Struct(d) = *self.tt.get(t) else { continue };
            for (_, fid) in self.structs[d.0 as usize].methods.clone() {
                let f = &self.funcs[fid.0 as usize];
                if f.is_async {
                    continue;
                }
                let params: Vec<TypeId> = f.params.iter().map(|l| f.locals[l.0 as usize].ty).collect();
                let ret = f.ret;
                let ft = self.tt.func(params, ret);
                r.method_types.push((fid, ft));
            }
        }
        // Written spellings of builtin field types (`int64`).
        for (sid, src) in self.struct_src.iter().enumerate() {
            if self.gen.inst_structs.contains_key(&(sid as u32)) {
                continue;
            }
            for f in &src.fields {
                let ast::TypeKind::Named { path, generics } = &f.ty.kind else { continue };
                if !generics.is_empty() || path.segments.len() != 1 || !ALIAS_SPELLINGS.contains(&path.segments[0].name.as_str()) {
                    continue;
                }
                if let Some(idx) = self.structs[sid].fields.iter().position(|x| x.name == f.name.name) {
                    r.field_spellings.push((sid as u32, idx as u32, path.segments[0].name.clone()));
                }
            }
        }
        Some(r)
    }

}

#[cfg(test)]
mod tests {
    #[test]
    fn library_sources_parse() {
        assert_eq!(super::module_asts().len(), super::MODULES.len());
    }
}
