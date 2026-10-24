//! Attribute resolution and checking.
//!
//! An attribute is either a builtin (`@attribute`, `@reflect`,
//! `@deprecated`, `@inline`, `@test`, `@json`, `@log`, `@allowCycle`, `@acyclic`) or a struct declared with
//! `@attribute`, whose fields are its parameters. Arguments are positional
//! (in field order) or named, must be compile-time constants, and are stored
//! evaluated in [`hir::Attr`].

use super::resolve::{PathRes, BUILTIN_ATTRS};
use super::*;
use crate::hir::{Attr, AttrKind, ConstValue};

/// Declaration kinds attributes can target (`@attribute(targets: [...])`).
pub(crate) const TARGETS: &[&str] = &["Namespace", "Struct", "Interface", "Enum", "Variant", "Field", "Func", "Param"];

/// Parameter kinds of builtin attributes.
#[derive(Clone, Copy, PartialEq)]
enum PKind {
    Str,
    Bool,
    Int,
    Targets,
}

fn builtin_params(name: &str) -> &'static [(&'static str, PKind)] {
    match name {
        "attribute" => &[("targets", PKind::Targets), ("repeatable", PKind::Bool)],
        "deprecated" => &[("message", PKind::Str)],
        "json" => &[("name", PKind::Str), ("omitEmpty", PKind::Bool), ("skip", PKind::Bool)],
        // log destructuring (`{@X}`): `@log(skip: true)` hides a field.
        "log" => &[("skip", PKind::Bool)],
        // `@embed("path")` on a global: the file's contents (see embed_global).
        "embed" => &[("path", PKind::Str)],
        // encoding.* field mappings (tier 2): csv, xml, yaml, toml and
        // the format-independent serde.
        "csv" => &[("name", PKind::Str), ("index", PKind::Int), ("skip", PKind::Bool), ("omitEmpty", PKind::Bool)],
        "xml" => &[("name", PKind::Str), ("attr", PKind::Bool), ("text", PKind::Bool), ("omitEmpty", PKind::Bool), ("ns", PKind::Str), ("skip", PKind::Bool)],
        "yaml" => &[("name", PKind::Str), ("omitEmpty", PKind::Bool), ("skip", PKind::Bool), ("flow", PKind::Bool)],
        "toml" => &[("name", PKind::Str), ("omitEmpty", PKind::Bool), ("skip", PKind::Bool), ("inline", PKind::Bool)],
        "serde" => &[("name", PKind::Str), ("omitEmpty", PKind::Bool), ("skip", PKind::Bool)],
        _ => &[],
    }
}

impl<'a> Sema<'a> {
    /// Evaluates the attributes (and field defaults) of struct `sid` and
    /// its fields in the current scope. Also used for instances of generic
    /// structs created after [`check_attributes`](Self::check_attributes):
    /// the attributes of a generic struct belong to every instance.
    pub(crate) fn struct_attrs(&mut self, sid: usize) {
        let src = self.struct_src[sid];
        let attrs = self.eval_attrs(&src.attrs, "Struct");
        self.structs[sid].attrs = attrs;
        for f in &src.fields {
            let attrs = self.eval_attrs(&f.attrs, "Field");
            let ty = self.structs[sid].fields.iter().find(|x| x.name == f.name.name).map(|x| x.ty);
            let default = match (&f.default, ty) {
                (Some(d), Some(ty)) => self.attr_value(d, ty),
                _ => None,
            };
            if let Some(field) = self.structs[sid].fields.iter_mut().find(|x| x.name == f.name.name) {
                field.attrs = attrs;
                field.default = default;
            }
        }
    }

    /// Evaluates the attributes of every declaration.
    pub(crate) fn check_attributes(&mut self) {
        // Attribute structs first: their own `@attribute(...)` and field
        // defaults are needed to check uses.
        let order: Vec<usize> = (0..self.structs.len()).filter(|&s| self.structs[s].is_attribute).chain((0..self.structs.len()).filter(|&s| !self.structs[s].is_attribute)).collect();
        for sid in order {
            self.cur_file = self.structs[sid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            self.struct_attrs(sid);
        }
        self.attrs_done = true;
        let files = self.files;
        for (fi, file) in files.iter().enumerate() {
            self.cur_file = fi;
            self.cur_ns = self.file_scopes[fi].ns;
            let attrs = self.eval_attrs(&file.attrs, "Namespace");
            let ns = self.cur_ns;
            self.nss[ns].attrs.extend(attrs);
        }
        for iid in 0..self.interfaces.len() {
            self.cur_file = self.interfaces[iid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            let src = self.iface_src[iid];
            let attrs = self.eval_attrs(&src.attrs, "Interface");
            self.interfaces[iid].attrs = attrs;
        }
        for eid in 0..self.enums.len() {
            self.cur_file = self.enums[eid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            let src = self.enum_src[eid];
            let attrs = self.eval_attrs(&src.attrs, "Enum");
            self.enums[eid].attrs = attrs;
            for v in &src.variants {
                let attrs = self.eval_attrs(&v.attrs, "Variant");
                if let Some(hv) = self.enums[eid].variants.iter_mut().find(|x| x.name == v.name.name) {
                    hv.attrs = attrs;
                }
                for f in &v.fields {
                    self.eval_attrs(&f.attrs, "Field");
                }
            }
        }
        for fid in 0..self.funcs.len() {
            let decl = match self.func_src[fid] {
                FuncSrc::Free(d) | FuncSrc::Method(d, _) => d,
                FuncSrc::Synthetic => continue,
            };
            self.cur_file = self.funcs[fid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            let attrs = self.eval_attrs(&decl.attrs, "Func");
            self.funcs[fid].attrs = attrs;
            let mut params = Vec::new();
            for p in &decl.params {
                params.push(self.eval_attrs(&p.attrs, "Param"));
            }
            if params.iter().any(|a| !a.is_empty()) {
                self.funcs[fid].param_attrs = params;
            }
        }
        for gid in 0..self.globals.len() {
            self.cur_file = self.globals[gid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            let attrs = match self.global_src[gid] {
                GlobalSrc::Var(v) => &v.attrs,
                GlobalSrc::Const(c) => &c.attrs,
            };
            let attrs = self.eval_attrs(attrs, "Var");
            self.globals[gid].attrs = attrs;
            self.embed_global(gid);
        }
    }

    /// Resolves the attribute's declaration: a user `@attribute` struct in
    /// scope, else a builtin.
    fn attr_kind(&mut self, a: &ast::Attribute) -> Option<AttrKind> {
        let segs: Vec<&ast::Ident> = a.path.segments.iter().collect();
        let name = a.path.to_string();
        // Builtin attribute names must not resolve to their value structs.
        self.refl.in_attr = true;
        let res = self.resolve_segments(&segs, false);
        self.refl.in_attr = false;
        if let Some((PathRes::Member(Member::Struct(s)), n)) = res {
            if n == segs.len() {
                if self.structs[s as usize].is_attribute {
                    self.resolve_segments(&segs, true);
                    return Some(AttrKind::User(s));
                }
                // `@json` with `import reflect` (or `@reflect.json`):
                // the value struct of a builtin attribute names the builtin.
                if let Some(n) = self.attr_value_struct_name(s) {
                    return Some(AttrKind::Builtin(n.to_string()));
                }
                self.err(format!("{name} is not an attribute (declare the struct with @attribute)"), a.path.span);
                return None;
            }
        }
        if let Some((PathRes::Member(Member::Generic(t)), n)) = res {
            if n == segs.len() {
                if let Some(s) = self.std_attribute_struct(t, a.path.span) {
                    self.resolve_segments(&segs, true);
                    return Some(AttrKind::User(s));
                }
            }
        }
        if segs.len() == 1 && (BUILTIN_ATTRS.contains(&name.as_str()) || name == "embed") {
            return Some(AttrKind::Builtin(name));
        }
        let mut names: Vec<String> = BUILTIN_ATTRS.iter().map(|s| s.to_string()).collect();
        names.push("embed".into());
        names.extend(self.structs.iter().filter(|s| s.is_attribute).map(|s| s.name.clone()));
        let help = self.did_you_mean(&name, &names).map(|h| h.replacen('`', "`@", 1));
        self.err_maybe_help(format!("unknown attribute @{name}"), a.path.span, help);
        None
    }

    /// Evaluates a list of attributes on a declaration of kind `target`.
    pub(crate) fn eval_attrs(&mut self, attrs: &[ast::Attribute], target: &str) -> Vec<Attr> {
        let mut out: Vec<Attr> = Vec::new();
        for a in attrs {
            let Some(kind) = self.attr_kind(a) else { continue };
            let args = match &kind {
                AttrKind::Builtin(n) => self.builtin_args(n, a),
                AttrKind::User(s) => self.user_args(*s, a),
            };
            let (targets, repeatable) = self.attr_meta(&kind);
            let name = a.path.to_string();
            if let AttrKind::Builtin(n) = &kind {
                if n == "attribute" && target != "Struct" {
                    self.err("@attribute can only be applied to a struct", a.span);
                }
                if (n == "allowCycle" || n == "acyclic") && target != "Struct" && target != "Enum" {
                    self.err(format!("@{n} can only be applied to a struct or an enum"), a.span);
                }
            }
            if let Some(ts) = targets {
                if !ts.iter().any(|t| t == target) {
                    self.err(format!("attribute @{name} cannot be applied to a {} (allowed: {})", target.to_lowercase(), ts.join(", ")), a.span);
                }
            }
            if !repeatable && out.iter().any(|o| o.kind == kind) {
                self.err(format!("attribute @{name} is not repeatable"), a.span);
            }
            out.push(Attr { kind, args, span: a.span });
        }
        out
    }

    /// (allowed targets, repeatable) of an attribute kind.
    fn attr_meta(&self, kind: &AttrKind) -> (Option<Vec<String>>, bool) {
        let AttrKind::User(s) = kind else { return (None, false) };
        let Some(meta) = self.structs[*s as usize].attrs.iter().find(|a| a.kind == AttrKind::Builtin("attribute".into())) else {
            return (None, false);
        };
        let mut targets = None;
        let mut repeatable = false;
        for (k, v) in &meta.args {
            match (k.as_str(), v) {
                ("targets", ConstValue::List(vs)) => {
                    targets = Some(vs.iter().filter_map(|v| if let ConstValue::Name(n) = v { Some(n.clone()) } else { None }).collect())
                }
                ("repeatable", ConstValue::Bool(b)) => repeatable = *b,
                _ => {}
            }
        }
        (targets, repeatable)
    }

    /// Matches arguments to parameter names: positional by order, or named.
    fn bind_args<'e>(&mut self, a: &'e ast::Attribute, params: &[String]) -> Vec<(usize, &'e ast::Expr)> {
        let mut out: Vec<(usize, &ast::Expr)> = Vec::new();
        let name = a.path.to_string();
        for (i, arg) in a.args.iter().enumerate() {
            let idx = match &arg.name {
                Some(n) => match params.iter().position(|p| *p == n.name) {
                    Some(i) => i,
                    None => {
                        self.err(format!("unknown argument {} for attribute @{name}", n.name), n.span);
                        continue;
                    }
                },
                None => {
                    if i >= params.len() {
                        self.err(format!("too many arguments for attribute @{name}"), arg.span);
                        continue;
                    }
                    i
                }
            };
            if out.iter().any(|(j, _)| *j == idx) {
                self.err(format!("argument {} given twice for attribute @{name}", params[idx]), arg.span);
                continue;
            }
            out.push((idx, &arg.value));
        }
        out
    }

    fn builtin_args(&mut self, n: &str, a: &ast::Attribute) -> Vec<(String, ConstValue)> {
        let params = builtin_params(n);
        let names: Vec<String> = params.iter().map(|p| p.0.to_string()).collect();
        let mut out = Vec::new();
        for (idx, e) in self.bind_args(a, &names) {
            let (pname, kind) = params[idx];
            let v = match kind {
                PKind::Targets => self.targets_value(e),
                PKind::Str => self.attr_value(e, self.tt.string),
                PKind::Bool => self.attr_value(e, self.tt.bool_),
                PKind::Int => self.attr_value(e, self.tt.int),
            };
            if let Some(v) = v {
                out.push((pname.to_string(), v));
            }
        }
        out
    }

    fn user_args(&mut self, s: u32, a: &ast::Attribute) -> Vec<(String, ConstValue)> {
        let fields: Vec<(String, TypeId, Option<ConstValue>)> = self.structs[s as usize].fields.iter().map(|f| (f.name.clone(), f.ty, f.default.clone())).collect();
        let names: Vec<String> = fields.iter().map(|f| f.0.clone()).collect();
        let bound = self.bind_args(a, &names);
        let mut out = Vec::new();
        for (i, (fname, ty, default)) in fields.iter().enumerate() {
            match bound.iter().find(|(j, _)| *j == i) {
                Some((_, e)) => {
                    if let Some(v) = self.attr_value(e, *ty) {
                        out.push((fname.clone(), v));
                    }
                }
                None => match default {
                    Some(d) => out.push((fname.clone(), d.clone())),
                    None => self.err(format!("missing argument {fname} for attribute @{}", a.path.to_string()), a.span),
                },
            }
        }
        out
    }

    /// An attribute-struct argument written as a literal (`unit{name: "ms"}`):
    /// the values of all its fields in declaration order (defaults filled
    /// in), as a [`ConstValue::List`].
    fn attr_struct_value(&mut self, path: &ast::Path, generics: &[ast::TypeExpr], fields: &[ast::FieldInit], sid: u32, span: Span) -> Option<ConstValue> {
        let ty = self.struct_ty(sid);
        let found = self.struct_lit_struct(path, generics, Some(ty), span)?;
        if found != sid {
            let (want, got) = (self.structs[sid as usize].name.clone(), self.structs[found as usize].name.clone());
            self.err(format!("attribute argument has type {got}, want {want}"), span);
            return None;
        }
        if !self.structs[sid as usize].is_attribute {
            let n = self.structs[sid as usize].name.clone();
            self.err(format!("attribute arguments must be compile-time constants ({n} is not an @attribute struct)"), span);
            return None;
        }
        for f in fields {
            if !self.structs[sid as usize].fields.iter().any(|x| x.name == f.name.name) {
                let n = self.structs[sid as usize].name.clone();
                self.err(format!("unknown field {} in struct literal of type {n}", f.name.name), f.name.span);
            }
        }
        let decl: Vec<(String, TypeId, Option<ConstValue>)> = self.structs[sid as usize].fields.iter().map(|f| (f.name.clone(), f.ty, f.default.clone())).collect();
        let mut out = Vec::new();
        for (name, fty, default) in decl {
            let v = match fields.iter().find(|f| f.name.name == name) {
                Some(f) => self.attr_value(&f.value, fty)?,
                None => match default {
                    Some(d) => d,
                    None => {
                        let n = self.structs[sid as usize].name.clone();
                        self.err(format!("missing field {name} in attribute value {n}{{...}}"), span);
                        return None;
                    }
                },
            };
            out.push(v);
        }
        Some(ConstValue::List(out))
    }

    /// `[reflect.Target.Field, ...]`: the last path segment of each element.
    fn targets_value(&mut self, e: &ast::Expr) -> Option<ConstValue> {
        let ast::ExprKind::SliceLit(es) = &e.kind else {
            self.err("targets must be a list like [reflect.Target.Field]", e.span);
            return None;
        };
        let mut out = Vec::new();
        for x in es {
            match Self::flatten_path(x).and_then(|s| s.last().map(|i| i.name.clone())) {
                Some(n) if TARGETS.contains(&n.as_str()) => out.push(ConstValue::Name(n)),
                _ => self.err(format!("unknown attribute target (expected one of {})", TARGETS.join(", ")), x.span),
            }
        }
        Some(ConstValue::List(out))
    }

    /// A compile-time constant of type `ty` (primitives, strings, plain
    /// enums, slices of those).
    pub(crate) fn attr_value(&mut self, e: &ast::Expr, ty: TypeId) -> Option<ConstValue> {
        if let (ast::ExprKind::SliceLit(es), Type::Slice(el)) = (&e.kind, self.tt.get(ty).clone()) {
            let vs: Option<Vec<ConstValue>> = es.iter().map(|x| self.attr_value(x, el)).collect();
            return vs.map(ConstValue::List);
        }
        if let (ast::ExprKind::StructLit { path, generics, fields }, Type::Struct(d)) = (&e.kind, self.tt.get(ty).clone()) {
            return self.attr_struct_value(path, generics, fields, d.0, e.span);
        }
        let Some(f) = self.fold(e) else {
            self.err("attribute arguments must be compile-time constants", e.span);
            return None;
        };
        let h = self.const_expr(f, Some(ty), e.span);
        if h.ty != ty {
            self.err(format!("attribute argument has type {}, want {}", self.tn(h.ty), self.tn(ty)), e.span);
            return None;
        }
        match h.kind {
            hir::ExprKind::Const(v) => Some(v),
            hir::ExprKind::EnumLit { enum_id, variant, .. } => Some(ConstValue::Enum(enum_id, variant)),
            _ => None,
        }
    }

    /// A struct declared with `@attribute` in a Zell-source stdlib module
    /// (`@binary(endian: "big")` of `encoding.binary`, `@flag` of
    /// `cli`): such structs are plain templates, instantiated here on
    /// first use as an attribute, marked as attribute structs and given
    /// their own attribute values (field defaults, `targets`).
    fn std_attribute_struct(&mut self, t: u32, span: Span) -> Option<u32> {
        self.force_template(t);
        let tm = &self.gen.templates[t as usize];
        if !tm.is_std || !tm.params.is_empty() {
            return None;
        }
        let super::generics::TmplDecl::Struct(decl) = tm.decl else { return None };
        if !decl.attrs.iter().any(|a| a.path.to_string() == "attribute") {
            return None;
        }
        let sid = self.plain_std_struct(t, span)?;
        if !self.structs[sid as usize].is_attribute {
            self.structs[sid as usize].is_attribute = true;
            self.struct_attrs(sid as usize);
        }
        Some(sid)
    }

    /// `@embed("path")` on a global (DESIGN.md "Атрибуты"): the contents of
    /// the file, relative to the declaring source file, become the value.
    /// `var X string` / `const X = ""` (UTF-8 text; the const's own value
    /// is replaced), `var X []byte` (any bytes), or `var X embed.FS` for a
    /// directory or a `dir/*.ext` glob (every regular file, recursively for
    /// a directory, sorted by path).
    fn embed_global(&mut self, gid: usize) {
        let Some(attr) = self.globals[gid].attrs.iter().find(|a| a.kind == AttrKind::Builtin("embed".into())) else { return };
        let span = attr.span;
        let rel = match attr.args.iter().find(|(k, _)| k == "path") {
            Some((_, ConstValue::Str(p))) => p.clone(),
            _ => {
                self.err("@embed needs a path: @embed(\"file.txt\")", span);
                return;
            }
        };
        let file = self.globals[gid].file;
        let base = super::stdlib::embed_source_dir(file);
        let (declared, is_const) = match self.global_src[gid] {
            GlobalSrc::Var(v) => {
                if v.init.is_some() {
                    self.err("a variable with @embed takes its value from the file: remove the initializer", span);
                    return;
                }
                (v.ty.as_ref(), false)
            }
            GlobalSrc::Const(c) => (c.ty.as_ref(), true),
        };
        let ty = match declared {
            Some(t) => self.resolve_type(t),
            None if is_const => self.tt.string,
            None => {
                self.err("@embed needs a declared type: string, []byte or embed.FS", span);
                return;
            }
        };
        let bytes_ty = self.tt.slice(self.tt.byte);
        let string = self.tt.string;
        let init = if ty == string || ty == bytes_ty {
            let path = base.join(&rel);
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(e) => {
                    self.err(format!("@embed: cannot read {}: {e}", path.display()), span);
                    return;
                }
            };
            if ty == string {
                match String::from_utf8(data) {
                    Ok(text) => mk(hir::ExprKind::Const(ConstValue::Str(text)), string, span),
                    Err(_) => {
                        self.err(format!("@embed: {rel} is not valid UTF-8 text (embed it as []byte)"), span);
                        return;
                    }
                }
            } else {
                self.byte_slice_lit(&data, span)
            }
        } else if let Some(sid) = self.struct_of(ty).filter(|&s| self.structs[s as usize].full_name == "embed.FS") {
            if is_const {
                self.err("@embed of an embed.FS must be a var", span);
                return;
            }
            let files = match embed_glob(&base, &rel) {
                Ok(f) => f,
                Err(e) => {
                    self.err(format!("@embed: {e}"), span);
                    return;
                }
            };
            let strs = self.tt.slice(string);
            let bytess = self.tt.slice(bytes_ty);
            let names = files.iter().map(|(n, _)| mk(hir::ExprKind::Const(ConstValue::Str(n.clone())), string, span)).collect();
            let datas = files.iter().map(|(_, d)| self.byte_slice_lit(d, span)).collect();
            let field = |s: &Self, n: &str| s.structs[sid as usize].fields.iter().position(|f| f.name == n).unwrap_or(0) as u32;
            let (fnames, fdata) = (field(self, "names"), field(self, "data"));
            mk(
                hir::ExprKind::StructLit {
                    struct_id: sid,
                    fields: vec![(fnames, mk(hir::ExprKind::SliceLit(names), strs, span)), (fdata, mk(hir::ExprKind::SliceLit(datas), bytess, span))],
                },
                ty,
                span,
            )
        } else {
            let tn = self.tn(ty);
            self.err(format!("@embed works on string, []byte and embed.FS, not {tn}"), span);
            return;
        };
        if is_const {
            let hir::ExprKind::Const(v) = &init.kind else {
                self.err("@embed on a const needs type string (use var for []byte)", span);
                return;
            };
            let v = v.clone();
            self.global_folded.insert(gid as u32, super::consts::Folded { value: v.clone(), ty: Some(string) });
            self.globals[gid].ty = string;
            self.globals[gid].value = Some(v);
        } else {
            self.globals[gid].ty = ty;
            self.globals[gid].init = Some(init);
        }
        self.global_state[gid] = super::GState::Done;
        self.global_order.push(hir::GlobalId(gid as u32));
    }

    fn byte_slice_lit(&mut self, data: &[u8], span: Span) -> hir::Expr {
        let byte = self.tt.byte;
        let elems = data.iter().map(|b| mk(hir::ExprKind::Const(ConstValue::Int(*b as i128)), byte, span)).collect();
        let ty = self.tt.slice(byte);
        mk(hir::ExprKind::SliceLit(elems), ty, span)
    }
}

/// The files of an `embed.FS` pattern: a directory (all regular files,
/// recursively) or `dir/*suffix` (files of dir whose name ends with
/// suffix); names are relative to `base`, with `/` separators, sorted.
fn embed_glob(base: &std::path::Path, pattern: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let (dir, suffix) = match pattern.rsplit_once('/') {
        Some((d, last)) if last.starts_with('*') => (d.to_string(), Some(last[1..].to_string())),
        _ if pattern.starts_with('*') => (".".to_string(), Some(pattern[1..].to_string())),
        _ => (pattern.trim_end_matches('/').to_string(), None),
    };
    let root = base.join(&dir);
    let mut out = Vec::new();
    let mut stack = vec![(root.clone(), dir.clone())];
    while let Some((d, rel)) = stack.pop() {
        let entries = std::fs::read_dir(&d).map_err(|e| format!("cannot read directory {}: {e}", d.display()))?;
        for e in entries {
            let e = e.map_err(|e| e.to_string())?;
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let rel_name = if rel == "." || rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if p.is_dir() {
                if suffix.is_none() {
                    stack.push((p, rel_name));
                }
            } else if suffix.as_ref().is_none_or(|s| name.ends_with(s.as_str())) {
                let data = std::fs::read(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                out.push((rel_name, data));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    if out.is_empty() {
        return Err(format!("no files match {pattern:?}"));
    }
    Ok(out)
}
