//! Namespaces, declarations, imports, name lookup and type resolution.

use super::*;
use crate::hir::{FuncKind, NamespaceId};

/// Result of resolving a dotted path symbolically.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PathRes {
    Member(Member),
    Builtin(BuiltinFn),
    BuiltinType(TypeId),
    /// Enum variant (enum id, variant index).
    Variant(u32, u32),
    /// Static method of a struct.
    StaticMethod(FuncId),
    /// Method expression `T.M` (method `M` of struct `T`): a function
    /// taking the receiver `*T` first (method_value.rs).
    MethodExpr(FuncId, u32),
}

/// Embedding depth after which promotion lookup gives up (guards cycles).
const MAX_EMBED_DEPTH: usize = 16;

/// Builtin attribute names.
pub(crate) const BUILTIN_ATTRS: &[&str] = &["attribute", "reflect", "deprecated", "inline", "test", "bench", "json", "log", "csv", "xml", "yaml", "toml", "serde", "allowCycle", "acyclic", "required"];

impl<'a> Sema<'a> {
    // -- namespace tree ------------------------------------------------------

    /// Returns the namespace for `path`, creating it and its parents.
    pub(crate) fn ensure_ns(&mut self, path: &str, span: Span) -> NsIdx {
        if let Some(&i) = self.ns_index.get(path) {
            return i;
        }
        let is_std = stdlib::is_std_namespace(path);
        let parent = path.rfind('.').map(|p| self.ensure_ns(&path[..p], span));
        let idx = self.nss.len();
        let hir_id = if is_std {
            None
        } else {
            Some(NamespaceId(self.nss.iter().filter(|n| n.hir_id.is_some()).count() as u32))
        };
        self.nss.push(Ns { path: path.to_string(), members: HashMap::new(), is_std, hir_id, attrs: Vec::new() });
        self.ns_index.insert(path.to_string(), idx);
        let last = path.rsplit('.').next().unwrap().to_string();
        match parent {
            Some(p) => {
                self.nss[p].members.insert(last, (Member::Ns(idx), span, self.cur_file));
            }
            None => {
                self.roots.insert(last, idx);
            }
        }
        idx
    }

    pub(crate) fn build_stdlib(&mut self) {
        for ns in stdlib::NAMESPACES {
            self.ensure_ns(ns, Span::DUMMY);
        }
        for (i, f) in stdlib::FUNCS.iter().enumerate() {
            let ns = self.ns_index[f.ns];
            self.nss[ns].members.insert(f.name.to_string(), (Member::StdFunc(i), Span::DUMMY, 0));
        }
        for (ns, name, sp) in stdlib::SPECIALS {
            let ns = self.ns_index[*ns];
            self.nss[ns].members.insert(name.to_string(), (Member::Special(*sp), Span::DUMMY, 0));
        }
        for (i, (ns, name, _)) in stdlib::CONSTS.iter().enumerate() {
            let ns = self.ns_index[*ns];
            self.nss[ns].members.insert(name.to_string(), (Member::StdConst(i), Span::DUMMY, 0));
        }
    }

    /// Adds a member to a namespace, reporting duplicates.
    fn register(&mut self, ns: NsIdx, name: &ast::Ident, m: Member) {
        if let Some((_, prev, pfile)) = self.nss[ns].members.get(&name.name).cloned() {
            let note_span = if pfile == self.cur_file { Some(prev) } else { None };
            let path = self.nss[ns].path.clone();
            self.err_note(format!("duplicate declaration of {} in namespace {path}", name.name), name.span, "previously declared here", note_span);
            return;
        }
        self.nss[ns].members.insert(name.name.clone(), (m, name.span, self.cur_file));
    }

    fn reject_generics(&mut self, g: &[ast::GenericParam], what: &str) {
        if let Some(p) = g.first() {
            self.err(format!("generic {what} are not supported (only functions, methods, structs and enums can be generic)"), p.span);
        }
    }

    /// Registers every declaration of every file.
    pub(crate) fn collect_decls(&mut self) {
        let files = self.files;
        for (fi, file) in files.iter().enumerate() {
            self.cur_file = fi;
            let ns = self.ensure_ns(&file.namespace.to_string(), file.namespace.span);
            let path = &self.nss[ns].path;
            if self.nss[ns].is_std {
                let root = path.split('.').next().unwrap_or(path);
                let msg = if root == path { format!("namespace `{path}` belongs to the standard library") } else { format!("namespace `{path}` belongs to the standard library (`{root}` is a standard library root)") };
                self.err_help(msg, file.namespace.span, "choose another root namespace, e.g. the package name");
            } else if let Some(msg) = stdlib::removed_std_root(path) {
                self.err(format!("namespace `{path}` is reserved: {msg}"), file.namespace.span);
            }
            self.cur_ns = ns;
            self.file_scopes.push(FileScope { ns, imports: Vec::new(), import_uses: Vec::new() });
            self.ide_namespace_decl(file);
            let hns = self.cur_hir_ns();
            for d in &file.decls {
                match d {
                    ast::Decl::Struct(s) if !s.generics.is_empty() => {
                        let t = self.new_template(&s.name.name, generics::TmplDecl::Struct(s), &s.generics, Vec::new(), false);
                        self.register(ns, &s.name, Member::Generic(t));
                    }
                    ast::Decl::Struct(s) => {
                        let id = self.structs.len() as u32;
                        let full_name = format!("{}.{}", self.nss[ns].path, s.name.name);
                        let is_attribute = s.attrs.iter().any(|a| a.path.to_string() == "attribute");
                        self.structs.push(hir::Struct {
                            name: s.name.name.clone(),
                            full_name,
                            namespace: hns,
                            fields: Vec::new(),
                            methods: Vec::new(),
                            attrs: Vec::new(),
                            is_attribute,
                            exported: is_exported(&s.name.name),
                            file: fi,
                            span: s.span,
                        });
                        self.struct_src.push(s);
                        self.register(ns, &s.name, Member::Struct(id));
                        let c = self.ide_cur_ns();
                        self.ide_def(index::SymKey::Struct(id), &s.name, s.span, c);
                        for m in &s.methods {
                            if self.structs[id as usize].methods.iter().any(|(n, _)| *n == m.name.name) || self.gen.methods.contains_key(&(id, m.name.name.clone())) {
                                self.err(format!("duplicate method {} in struct {}", m.name.name, s.name.name), m.name.span);
                                continue;
                            }
                            if !m.generics.is_empty() {
                                let t = self.new_template(&m.name.name, generics::TmplDecl::Method(m, id), &m.generics, Vec::new(), false);
                                self.gen.methods.insert((id, m.name.name.clone()), t);
                                continue;
                            }
                            let kind = FuncKind::Method { struct_id: id, is_static: m.is_static };
                            let fid = self.new_func(m, kind, FuncSrc::Method(m, id), &format!("{}.{}", s.name.name, m.name.name));
                            self.structs[id as usize].methods.push((m.name.name.clone(), fid));
                            self.ide_def(index::SymKey::Func(fid.0), &m.name, m.span, Some(index::SymKey::Struct(id)));
                        }
                    }
                    ast::Decl::Interface(it) => {
                        self.reject_generics(&it.generics, "interfaces");
                        let id = self.interfaces.len() as u32;
                        self.interfaces.push(hir::Interface {
                            name: it.name.name.clone(),
                            full_name: format!("{}.{}", self.nss[ns].path, it.name.name),
                            namespace: hns,
                            methods: Vec::new(),
                            sealed: it.is_sealed,
                            attrs: Vec::new(),
                            exported: is_exported(&it.name.name),
                            file: fi,
                            span: it.span,
                        });
                        self.iface_src.push(it);
                        self.register(ns, &it.name, Member::Interface(id));
                        let c = self.ide_cur_ns();
                        self.ide_def(index::SymKey::Interface(id), &it.name, it.span, c);
                        for m in it.methods.iter().filter(|m| m.body.is_some()) {
                            if m.is_static || m.is_async || !m.generics.is_empty() {
                                self.err("default methods of interfaces cannot be static, async or generic", m.name.span);
                                continue;
                            }
                            if self.iface_defaults.contains_key(&(id, m.name.name.clone())) {
                                continue; // reported as a duplicate method by `resolve_signatures`
                            }
                            let fid = self.new_func(m, FuncKind::Free, FuncSrc::Free(m), &format!("{}.{}", it.name.name, m.name.name));
                            self.iface_defaults.insert((id, m.name.name.clone()), fid);
                            self.iface_default_owner.insert(fid.0, id);
                        }
                    }
                    ast::Decl::Enum(e) if !e.generics.is_empty() => {
                        let t = self.new_template(&e.name.name, generics::TmplDecl::Enum(e), &e.generics, Vec::new(), false);
                        self.register(ns, &e.name, Member::Generic(t));
                    }
                    ast::Decl::Enum(e) => {
                        let id = self.enums.len() as u32;
                        self.enums.push(hir::Enum {
                            name: e.name.name.clone(),
                            full_name: format!("{}.{}", self.nss[ns].path, e.name.name),
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
                        self.register(ns, &e.name, Member::Enum(id));
                        let c = self.ide_cur_ns();
                        self.ide_def(index::SymKey::Enum(id), &e.name, e.span, c);
                        for m in &e.methods {
                            if m.is_static || !m.generics.is_empty() {
                                self.err("enum methods cannot be static or generic", m.name.span);
                                continue;
                            }
                            if self.enum_methods.contains_key(&(id, m.name.name.clone())) || e.variants.iter().any(|v| v.name.name == m.name.name) {
                                self.err(format!("duplicate member {} in enum {}", m.name.name, e.name.name), m.name.span);
                                continue;
                            }
                            let fid = self.new_func(m, FuncKind::Free, FuncSrc::Free(m), &format!("{}.{}", e.name.name, m.name.name));
                            self.enum_methods.insert((id, m.name.name.clone()), fid);
                            self.enums[id as usize].methods.push((m.name.name.clone(), fid));
                            self.enum_method_owner.insert(fid.0, id);
                            self.ide_def(index::SymKey::Func(fid.0), &m.name, m.span, Some(index::SymKey::Enum(id)));
                        }
                    }
                    ast::Decl::Func(f) if !f.generics.is_empty() => {
                        if f.body.is_none() {
                            self.err(format!("function {} has no body", f.name.name), f.name.span);
                        }
                        let t = self.new_template(&f.name.name, generics::TmplDecl::Func(f), &f.generics, Vec::new(), false);
                        self.register(ns, &f.name, Member::Generic(t));
                    }
                    ast::Decl::Func(f) if f.is_extern => {
                        self.err(
                            format!("`extern func {}` declares a runtime function; extern declarations exist only in the standard library's generated std/**/*_extern.zl files", f.name.name),
                            f.name.span,
                        );
                    }
                    ast::Decl::Func(f) => {
                        if f.body.is_none() {
                            self.err(format!("function {} has no body", f.name.name), f.name.span);
                        }
                        let fid = self.new_func(f, FuncKind::Free, FuncSrc::Free(f), &f.name.name);
                        self.register(ns, &f.name, Member::Func(fid));
                        let c = self.ide_cur_ns();
                        self.ide_def(index::SymKey::Func(fid.0), &f.name, f.span, c);
                    }
                    ast::Decl::Var(v) => {
                        if v.names.len() != 1 {
                            self.err("tuple destructuring is not allowed at the top level", v.span);
                            continue;
                        }
                        let gid = self.new_global(&v.names[0], false, GlobalSrc::Var(v), v.span);
                        self.register(ns, &v.names[0], Member::Global(gid));
                        let c = self.ide_cur_ns();
                        self.ide_def(index::SymKey::Global(gid.0), &v.names[0], v.span, c);
                    }
                    ast::Decl::Const(c) => {
                        let gid = self.new_global(&c.name, true, GlobalSrc::Const(c), c.span);
                        self.register(ns, &c.name, Member::Global(gid));
                        let cn = self.ide_cur_ns();
                        self.ide_def(index::SymKey::Global(gid.0), &c.name, c.span, cn);
                    }
                    ast::Decl::TypeAlias(a) => {
                        if let Some(at) = a.attrs.first() {
                            self.err("attributes are not allowed on type aliases", at.span);
                        }
                        let id = self.aliases.len() as u32;
                        self.aliases.push(super::Alias { decl: a, file: fi, ns, ty: None, resolving: false });
                        self.register(ns, &a.name, Member::Alias(id));
                    }
                    // Syntax error, already reported by the parser.
                    ast::Decl::Error(_) => {}
                }
            }
        }
    }

    /// Resolves every type alias (after the imports, so alias bodies see
    /// their file's imports); from here on lookups see through aliases.
    pub(crate) fn resolve_aliases(&mut self) {
        self.aliases_ready = true;
        for a in 0..self.aliases.len() {
            self.alias_type(a as u32);
        }
    }

    /// The type an alias stands for, resolved in the alias's own file and
    /// namespace (no type parameters in scope); a cycle is an error.
    pub(crate) fn alias_type(&mut self, a: u32) -> TypeId {
        let al = &self.aliases[a as usize];
        if let Some(t) = al.ty {
            return t;
        }
        let decl = al.decl;
        if al.resolving {
            let (file, saved) = (al.file, self.cur_file);
            self.cur_file = file;
            self.err(format!("invalid recursive type alias {}", decl.name.name), decl.name.span);
            self.cur_file = saved;
            self.aliases[a as usize].ty = Some(self.tt.invalid);
            return self.tt.invalid;
        }
        self.aliases[a as usize].resolving = true;
        let saved = (self.cur_file, self.cur_ns, std::mem::take(&mut self.gen.tenv));
        (self.cur_file, self.cur_ns) = (self.aliases[a as usize].file, self.aliases[a as usize].ns);
        let t = self.resolve_type(&decl.ty);
        (self.cur_file, self.cur_ns, self.gen.tenv) = saved;
        let al = &mut self.aliases[a as usize];
        al.resolving = false;
        *al.ty.get_or_insert(t)
    }

    /// An alias of a struct, enum or interface is that declaration itself
    /// (literals, static methods, variants); other aliases stay aliases.
    fn unalias(&mut self, m: Member) -> Member {
        let Member::Alias(a) = m else { return m };
        if !self.aliases_ready {
            return m;
        }
        let t = self.alias_type(a);
        match *self.tt.get(t) {
            Type::Struct(d) => Member::Struct(d.0),
            Type::Enum(d) => Member::Enum(d.0),
            Type::Interface(d) => Member::Interface(d.0),
            _ => m,
        }
    }

    pub(crate) fn new_func(&mut self, d: &'a ast::FuncDecl, kind: FuncKind, src: FuncSrc<'a>, local_name: &str) -> FuncId {
        let fid = FuncId(self.funcs.len() as u32);
        let mut f = self.dummy_function(d.name.name.clone(), kind, self.cur_hir_ns());
        f.full_name = format!("{}.{local_name}", self.nss[self.cur_ns].path);
        f.symbol = format!("zell_{}", f.full_name.replace('.', "__"));
        f.is_async = d.is_async;
        f.exported = is_exported(&d.name.name);
        f.span = d.span;
        self.funcs.push(f);
        self.func_src.push(src);
        self.sigs.push(Sig { params: Vec::new(), ret: self.tt.void, is_async: d.is_async });
        fid
    }

    fn new_global(&mut self, name: &ast::Ident, is_const: bool, src: GlobalSrc<'a>, span: Span) -> GlobalId {
        let gid = GlobalId(self.globals.len() as u32);
        let full_name = format!("{}.{}", self.nss[self.cur_ns].path, name.name);
        self.globals.push(hir::Global {
            name: name.name.clone(),
            symbol: format!("zell_{}", full_name.replace('.', "__")),
            full_name,
            namespace: self.cur_hir_ns(),
            ty: self.tt.invalid,
            is_const,
            value: None,
            init: None,
            attrs: Vec::new(),
            exported: is_exported(&name.name),
            file: self.cur_file,
            span,
        });
        self.global_src.push(src);
        self.global_state.push(GState::Pending);
        gid
    }

    /// Resolves the `import` lines of every file.
    pub(crate) fn resolve_imports(&mut self) {
        let files = self.files;
        for (fi, file) in files.iter().enumerate() {
            self.cur_file = fi;
            self.cur_ns = self.file_scopes[fi].ns;
            for imp in &file.imports {
                let path = imp.path.to_string();
                if let Some(msg) = stdlib::removed_std_root(&path) {
                    let fixes = match path.strip_prefix("std.") {
                        Some(rest) => vec![Fix::edit(format!("Use `import {rest}`"), imp.path.span, rest.to_string())],
                        None => vec![Fix::edit("Remove `import std`", imp.span, String::new())],
                    };
                    self.err_fix(format!("unknown namespace {path} in import: {msg}"), imp.path.span, codes::REMOVED_STD_ROOT, fixes);
                    continue;
                }
                if let Err(msg) = self.check_capability(fi, &path) {
                    self.err(msg, imp.path.span);
                    continue;
                }
                if let Err(msg) = self.policy.map_or(Ok(()), |p| p.check(fi, &path)) {
                    self.err(msg, imp.path.span);
                    continue;
                }
                let entry = if let Some(&ns) = self.ns_index.get(&path) {
                    self.ide_import(&imp.path, None);
                    match &imp.alias {
                        Some(a) => ImportEntry::Named(a.name.clone(), Member::Ns(ns), ns),
                        None => ImportEntry::Glob(ns),
                    }
                } else {
                    let segs = &imp.path.segments;
                    let parent = segs[..segs.len() - 1].iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(".");
                    let last = segs.last().unwrap();
                    let found = self.ns_index.get(&parent).copied().and_then(|ns| self.member_of(ns, last, true).map(|m| (m, ns)));
                    match found {
                        Some((m, ns)) => {
                            self.ide_import(&imp.path, Some(m));
                            let name = imp.alias.as_ref().unwrap_or(last).name.clone();
                            ImportEntry::Named(name, m, ns)
                        }
                        None => {
                            self.err(format!("unknown namespace or declaration {path} in import"), imp.path.span);
                            continue;
                        }
                    }
                };
                self.file_scopes[fi].imports.push(entry);
                self.file_scopes[fi].import_uses.push((imp.path.span, false));
            }
        }
    }

    // -- lookup -------------------------------------------------------------

    /// Member `name` of namespace `ns`, checking visibility from the current
    /// namespace.
    pub(crate) fn member_of(&mut self, ns: NsIdx, name: &ast::Ident, report: bool) -> Option<Member> {
        self.std_lookup_hook(ns, &name.name);
        let (m, _, _) = *self.nss[ns].members.get(&name.name)?;
        if report && ns != self.cur_ns && !self.nss[ns].is_std && !is_exported(&name.name) && !matches!(m, Member::Ns(_)) {
            let path = self.nss[ns].path.clone();
            self.err(format!("{} is private to namespace {path} (only capitalised names are exported)", name.name), name.span);
        }
        let m = self.unalias(m);
        // A gated module reached by its full path (`unsafe.ReadInt`)
        // needs the capability just like `import unsafe`.
        if report && !self.nss[self.cur_ns].is_std && super::debug_agent::gate_of(&self.nss[ns].path).is_some() {
            let path = self.nss[ns].path.clone();
            if let Err(msg) = self.check_capability(self.cur_file, &path) {
                self.err(msg, name.span);
            }
        }
        Some(m)
    }

    /// Looks up a non-local name: own namespace, imports, builtins, roots.
    pub(crate) fn lookup_global(&mut self, name: &ast::Ident, report: bool) -> Option<PathRes> {
        let n = name.name.as_str();
        if let Some(&(m, _, _)) = self.nss[self.cur_ns].members.get(n) {
            let m = self.unalias(m);
            if let Member::Ns(ns) = m {
                self.mark_ns_import_used(ns);
            }
            if report {
                self.note_import_shadowed(name);
            }
            return Some(PathRes::Member(m));
        }
        // Stdlib code is instantiated in the scope of the user file that
        // uses it, but a stdlib root it names (`hash.fnv`) is never hidden
        // by that file's imports (`import crypto` gives `crypto.hash`).
        if self.nss[self.cur_ns].is_std && stdlib::is_std_namespace(n) {
            if let Some(&r) = self.roots.get(n) {
                return Some(PathRes::Member(Member::Ns(r)));
            }
        }
        self.std_glob_hook(n);
        let mut found: Vec<(Member, NsIdx)> = Vec::new();
        let mut private: Option<NsIdx> = None;
        let mut used = Vec::new();
        for (i, e) in self.file_scopes[self.cur_file].imports.iter().enumerate() {
            match e {
                ImportEntry::Glob(ns) => {
                    // A stdlib attribute named like a builtin (`@delete`) does not hide the builtin outside attributes.
                    if self.nss[*ns].is_std && !self.refl.in_attr && matches!(n, "delete" | "len" | "append" | "panic" | "typeof" | "error") {
                        continue;
                    }
                    if let Some((m, _, _)) = self.nss[*ns].members.get(n) {
                        if self.nss[*ns].is_std || is_exported(n) || matches!(m, Member::Ns(_)) {
                            used.push(i);
                            if !found.iter().any(|(f, _)| f == m) {
                                found.push((*m, *ns));
                            }
                        } else {
                            private = Some(*ns);
                        }
                    }
                }
                ImportEntry::Named(alias, m, ns) if alias == n => {
                    used.push(i);
                    if !found.iter().any(|(f, _)| f == m) {
                        found.push((*m, *ns));
                    }
                }
                ImportEntry::Named(..) => {}
            }
        }
        for i in used {
            if let Some(u) = self.file_scopes[self.cur_file].import_uses.get_mut(i) {
                u.1 = true;
            }
        }
        if found.len() > 1 && report {
            let a = self.nss[found[0].1].path.clone();
            let b = self.nss[found[1].1].path.clone();
            self.err(format!("ambiguous name {n}: imported from both {a} and {b}; use an alias (`as`) or the full path"), name.span);
        }
        if let Some(&(m, _)) = found.first() {
            return Some(PathRes::Member(self.unalias(m)));
        }
        let builtin = match n {
            "error" => Some(BuiltinFn::Error),
            "len" => Some(BuiltinFn::Len),
            "append" => Some(BuiltinFn::Append),
            "delete" => Some(BuiltinFn::Delete),
            "panic" => Some(BuiltinFn::Panic),
            "typeof" => Some(BuiltinFn::Typeof),
            _ => None,
        };
        if let Some(b) = builtin {
            return Some(PathRes::Builtin(b));
        }
        if let Some(t) = self.tt.builtin(n) {
            return Some(PathRes::BuiltinType(t));
        }
        if let Some(&r) = self.roots.get(n) {
            if let Err(msg) = self.policy.map_or(Ok(()), |p| p.check(self.cur_file, n)) {
                if report {
                    self.err(msg, name.span);
                }
                return None;
            }
            self.mark_ns_import_used(r);
            return Some(PathRes::Member(Member::Ns(r)));
        }
        if let Some(ns) = self.enclosing_child_ns(n) {
            return Some(PathRes::Member(Member::Ns(ns)));
        }
        if let Some(s) = self.builtin_attr_type(n) {
            return Some(PathRes::Member(Member::Struct(s)));
        }
        if let (Some(ns), true) = (private, report) {
            let path = self.nss[ns].path.clone();
            self.err(format!("{n} is private to namespace {path} (only capitalised names are exported)"), name.span);
        }
        None
    }

    /// A declaration of the own namespace hides a name an import also
    /// exports (DESIGN.md «Правила импорта»): no error, lint L0010 at the
    /// use (the import's member stays reachable by its qualified name).
    fn note_import_shadowed(&mut self, name: &ast::Ident) {
        if self.nss[self.cur_ns].is_std {
            return;
        }
        let n = name.name.as_str();
        let hidden = self.file_scopes[self.cur_file].imports.iter().find_map(|e| match e {
            ImportEntry::Glob(ns) => {
                let m = self.nss[*ns].members.get(n)?;
                let visible = (self.nss[*ns].is_std || is_exported(n)) && !matches!(m.0, Member::Ns(_));
                visible.then(|| (self.nss[*ns].path.clone(), format!("import {}", self.nss[*ns].path)))
            }
            ImportEntry::Named(alias, _, ns) if alias == n => Some((self.nss[*ns].path.clone(), format!("import {}.{n}", self.nss[*ns].path))),
            ImportEntry::Named(..) => None,
        });
        let Some((path, import)) = hidden else { return };
        let msg = format!("{n} is the declaration of this namespace and hides {path}.{n} brought in by `{import}`; write {path}.{n} for the imported one");
        let d = crate::diagnostics::Diagnostic::warning(msg, name.span).in_file(self.cur_file).with_code(super::lint::IMPORT_SHADOWED);
        self.lint_notes.push(d);
    }

    /// A relative namespace reference (DESIGN.md «Правила импорта»): a
    /// head that is neither local nor imported names a child namespace of
    /// an enclosing namespace, innermost first (`model.X` in `app.api` is
    /// `app.model.X`). The own namespace, imports and roots were searched
    /// by the caller (a root wins: `errors.New` in `spec.errors.x` stays
    /// the std `errors`); std code keeps its own rules.
    fn enclosing_child_ns(&self, n: &str) -> Option<NsIdx> {
        if self.nss[self.cur_ns].is_std {
            return None;
        }
        let mut path = self.nss[self.cur_ns].path.as_str();
        while let Some(p) = path.rfind('.') {
            path = &path[..p];
            let ns = *self.ns_index.get(path)?;
            if let Some(&(Member::Ns(c), _, _)) = self.nss[ns].members.get(n) {
                return Some(c);
            }
        }
        None
    }

    /// A namespace reached by name (`geo.Hello()` after `import geo`)
    /// uses the file's glob import of that namespace, if any.
    fn mark_ns_import_used(&mut self, ns: NsIdx) {
        let fs = &mut self.file_scopes[self.cur_file];
        for (i, e) in fs.imports.iter().enumerate() {
            if matches!(e, ImportEntry::Glob(g) if *g == ns) {
                if let Some(u) = fs.import_uses.get_mut(i) {
                    u.1 = true;
                }
            }
        }
    }

    /// `json.Value` in a file with `import encoding.json`: a builtin
    /// attribute name (`json`, `yaml`, `toml`, ...) as the head of a
    /// qualified path is the imported namespace of that last segment, not
    /// the attribute's value struct (which has no members to qualify).
    fn attr_name_namespace(&mut self, segs: &[&ast::Ident]) -> Option<NsIdx> {
        let n = segs[0].name.as_str();
        if segs.len() < 2 || !super::reflect::ATTR_STRUCTS.contains(&n) || self.nss[self.cur_ns].members.contains_key(n) {
            return None;
        }
        let suffix = format!(".{n}");
        let (i, ns) = self.file_scopes[self.cur_file].imports.iter().enumerate().find_map(|(i, e)| match e {
            ImportEntry::Glob(ns) if self.nss[*ns].path.ends_with(&suffix) => Some((i, *ns)),
            _ => None,
        })?;
        if let Some(u) = self.file_scopes[self.cur_file].import_uses.get_mut(i) {
            u.1 = true;
        }
        Some(ns)
    }

    /// `json.Parse` without `import encoding.json`: the head is the builtin
    /// attribute (its value struct, possibly named `tomlAttr`), so the
    /// member is not found there. When a std namespace `....<head>`
    /// exports the member, reports the missing import (with a fix) instead
    /// of naming the internal struct; true if reported.
    fn attr_name_needs_import(&mut self, head: &ast::Ident, member: &ast::Ident) -> bool {
        let n = head.name.as_str();
        if !super::reflect::ATTR_STRUCTS.contains(&n) {
            return false;
        }
        let suffix = format!(".{n}");
        let mut paths: Vec<String> = self
            .nss
            .iter()
            .filter(|ns| ns.is_std && (ns.path == n || ns.path.ends_with(&suffix)) && ns.members.contains_key(&member.name))
            .map(|ns| ns.path.clone())
            .collect();
        // Namespaces written in Zell are loaded on first use (reflect.rs).
        let decl = [format!("func {}(", member.name), format!("func {}<", member.name)];
        for (path, _, src, _) in super::reflect::MODULES {
            if (*path == n || path.ends_with(&suffix)) && decl.iter().any(|d| src.contains(d.as_str())) {
                paths.push(path.to_string());
            }
        }
        paths.sort();
        paths.dedup();
        let Some(p) = paths.into_iter().next() else { return false };
        let mut fixes = Vec::new();
        if let Some(file) = self.files.get(self.cur_file) {
            let at = file.imports.last().map(|i| i.span.end).unwrap_or(file.namespace.span.end);
            let sep = if file.imports.is_empty() { "\n\n" } else { "\n" };
            fixes.push(crate::diagnostics::Fix::edit(format!("Import {p}"), Span::new(at, at), format!("{sep}import {p}")));
        }
        let msg = format!("{n} here is the @{n} attribute; add \"import {p}\" to call {n}.{}", member.name);
        let d = crate::diagnostics::Diagnostic::error(msg, member.span).in_file(self.cur_file).with_help(format!("{n}.{} is in {p}", member.name));
        let mut d = d;
        d.fixes = fixes;
        self.push_diag(d);
        true
    }

    /// Resolves as many leading segments of `segs` as name declarations
    /// (namespaces, types, static methods, variants); returns the result and
    /// the number of segments consumed. `None` if the head is unknown.
    pub(crate) fn resolve_segments(&mut self, segs: &[&ast::Ident], report: bool) -> Option<(PathRes, usize)> {
        let mut cur = match self.attr_name_namespace(segs) {
            Some(ns) => PathRes::Member(Member::Ns(ns)),
            None => self.lookup_global(segs[0], report)?,
        };
        self.ide_res(segs[0], cur);
        let mut i = 1;
        while i < segs.len() {
            let seg = segs[i];
            // A plain stdlib struct/enum template (`log.Level.Debug`,
            // `log.Context.Push`) resolves like the type it names.
            if let PathRes::Member(Member::Generic(t)) = cur {
                if let Some(m) = self.plain_std_member(t, seg.span) {
                    cur = PathRes::Member(m);
                }
            }
            let next = match cur {
                PathRes::Member(Member::Ns(ns)) => match self.member_of(ns, seg, report) {
                    Some(m) => PathRes::Member(m),
                    None => {
                        if report {
                            let path = self.nss[ns].path.clone();
                            let names: Vec<String> = self.nss[ns].members.keys().filter(|n| self.nss[ns].is_std || is_exported(n)).cloned().collect();
                            let help = self.did_you_mean(&seg.name, &names);
                            self.err_maybe_help(format!("unknown identifier {} in namespace {path}", seg.name), seg.span, help);
                        }
                        return None;
                    }
                },
                PathRes::Member(Member::Struct(s)) => {
                    let found = self.structs[s as usize].methods.iter().find(|(n, _)| *n == seg.name).map(|(_, f)| *f);
                    match found {
                        Some(f) if self.is_static(f) => {
                            self.check_member_visibility(self.structs[s as usize].namespace, &seg.name, seg.span, report);
                            PathRes::StaticMethod(f)
                        }
                        Some(f) => {
                            self.check_member_visibility(self.structs[s as usize].namespace, &seg.name, seg.span, report);
                            PathRes::MethodExpr(f, s)
                        }
                        None if self.gen.methods.contains_key(&(s, seg.name.clone())) => {
                            self.check_member_visibility(self.structs[s as usize].namespace, &seg.name, seg.span, report);
                            PathRes::Member(Member::Generic(self.gen.methods[&(s, seg.name.clone())]))
                        }
                        _ => {
                            if report {
                                if i == 1 && self.attr_name_needs_import(segs[0], seg) {
                                    return None;
                                }
                                let sn = self.structs[s as usize].name.clone();
                                // never the internal value struct names (`tomlAttr`)
                                let builtin = self.refl.structs.values().any(|&x| x == s);
                                let attr = super::reflect::ATTR_STRUCT_NAMES.iter().find(|(_, x)| *x == sn).map(|(a, _)| *a).or_else(|| super::reflect::ATTR_STRUCTS.iter().find(|a| **a == sn).copied());
                                let shown = match attr {
                                    Some(a) if builtin => format!("the @{a} attribute"),
                                    _ if found.is_some() => sn,
                                    _ => format!("struct {sn}"),
                                };
                                self.err(format!("{} is not a static method of {shown}", seg.name), seg.span);
                            }
                            return None;
                        }
                    }
                }
                PathRes::Member(Member::Enum(e)) => match self.enums[e as usize].variants.iter().position(|v| v.name == seg.name) {
                    Some(v) => PathRes::Variant(e, v as u32),
                    None => {
                        if report {
                            let en = self.enums[e as usize].name.clone();
                            self.err(format!("enum {en} has no variant {}", seg.name), seg.span);
                        }
                        return None;
                    }
                },
                _ => break,
            };
            self.ide_res(seg, next);
            cur = next;
            i += 1;
        }
        Some((cur, i))
    }

    fn is_static(&self, f: FuncId) -> bool {
        matches!(self.funcs[f.0 as usize].kind, FuncKind::Method { is_static: true, .. })
    }

    /// Reports access to a lowercase member (field/method) of a type from
    /// another namespace.
    pub(crate) fn check_member_visibility(&mut self, owner: NamespaceId, name: &str, span: Span, report: bool) {
        if report && !is_exported(name) && !self.nss[self.cur_ns].is_std && self.nss[self.cur_ns].hir_id != Some(owner) {
            let path = self.nss[self.ns_of_hir(owner)].path.clone();
            self.err(format!("{name} is private to namespace {path} (only capitalised names are exported)"), span);
        }
    }

    // -- types ----------------------------------------------------------------

    /// Resolves a written type.
    pub(crate) fn resolve_type(&mut self, t: &ast::TypeExpr) -> TypeId {
        use ast::TypeKind as K;
        match &t.kind {
            // Syntax error, already reported by the parser.
            K::Error => self.tt.invalid,
            // Untyped lambda parameter outside a typed context.
            K::Infer => {
                self.err("missing parameter type (write `(x T) => ...`; lambda parameters are typed from the expected function type only)", t.span);
                self.tt.invalid
            }
            // Accepted only by `resolve_func_sig` (the last parameter).
            K::Variadic(x) => {
                self.err("a variadic parameter ...T is only allowed as the last parameter of a non-generic function or method", t.span);
                let _ = self.resolve_type(x);
                self.tt.invalid
            }
            K::Named { path, generics } => {
                if path.segments.len() == 1 && generics.is_empty() {
                    if let Some(ty) = self.type_param(&path.segments[0].name) {
                        return ty;
                    }
                }
                // `t`, `p.Type`, `m.Args` ... of an unrolled descriptor loop.
                if generics.is_empty() && (path.segments.len() <= 2 || path.segments[1].name == "Type") {
                    if let Some(ty) = self.ct_desc_type(path) {
                        return ty;
                    }
                }
                if !generics.is_empty() {
                    if path.segments.len() == 1 && path.segments[0].name == "Task" && generics.len() == 1 {
                        let inner = self.resolve_type(&generics[0]);
                        return self.tt.task(inner);
                    }
                    // `expr<F>`: an expression tree (sema/exprtree.rs).
                    if path.segments.len() == 1 && path.segments[0].name == "expr" && generics.len() == 1 {
                        let inner = self.resolve_type(&generics[0]);
                        return self.expr_tree_type(inner, t.span);
                    }
                    return self.resolve_generic_type(path, generics, t.span);
                }
                if path.segments.len() == 1 {
                    if let Some(b) = self.tt.builtin(&path.segments[0].name) {
                        self.ide_ref(path.segments[0].span, index::SymKey::Builtin(path.segments[0].name.clone()));
                        return b;
                    }
                }
                let segs: Vec<&ast::Ident> = path.segments.iter().collect();
                match self.resolve_segments(&segs, true) {
                    Some((PathRes::Member(Member::Generic(g)), n)) if n == segs.len() => {
                        if let Some(ty) = self.plain_std_type(g, path.span) {
                            return ty;
                        }
                        let name = self.gen.templates[g as usize].name.clone();
                        self.err(format!("generic type {name} needs type arguments ({name}<...>)"), path.span);
                        self.tt.invalid
                    }
                    Some((res, n)) if n == segs.len() => match self.path_res_type(res) {
                        Some(ty) => ty,
                        None => {
                            self.err(format!("{} is not a type", path.to_string()), path.span);
                            self.tt.invalid
                        }
                    },
                    Some(_) => {
                        self.unknown_type(path);
                        self.tt.invalid
                    }
                    None => {
                        if !self.diags.has_errors_at(path.span, self.cur_file) {
                            self.unknown_type(path);
                        }
                        self.tt.invalid
                    }
                }
            }
            K::Fallible(inner) => {
                let i = self.resolve_type(inner);
                if self.tt.is_fallible(i) {
                    self.err("a fallible type cannot be fallible again", t.span);
                }
                self.tt.fallible(i)
            }
            K::Pointer(inner) => {
                let i = self.resolve_type(inner);
                self.tt.pointer(i)
            }
            K::Weak(inner) => {
                let i = self.resolve_type(inner);
                self.tt.weak(i)
            }
            K::Slice(inner) => {
                let i = self.resolve_type(inner);
                self.tt.slice(i)
            }
            K::Array(inner, n) => {
                let i = self.resolve_type(inner);
                self.tt.array(i, *n)
            }
            K::ArrayExpr(inner, len) => {
                let i = self.resolve_type(inner);
                match self.fold(len).map(|f| f.value) {
                    Some(hir::ConstValue::Int(n)) if n >= 0 && n <= u32::MAX as i128 => self.tt.array(i, n as u64),
                    Some(hir::ConstValue::Int(_)) => {
                        self.err("array length must be a non-negative constant", len.span);
                        self.tt.invalid
                    }
                    _ => {
                        self.err("array length must be an integer constant expression", len.span);
                        self.tt.invalid
                    }
                }
            }
            K::Map(k, v) => {
                let k = self.resolve_type(k);
                let v = self.resolve_type(v);
                if !self.is_map_key(k) {
                    self.err(format!("invalid map key type {}", self.tn(k)), t.span);
                }
                self.tt.map(k, v)
            }
            K::Func { params, ret } => {
                let ps = params.iter().map(|p| self.resolve_type(p)).collect();
                let r = match ret {
                    Some(r) => self.resolve_type(r),
                    None => self.tt.void,
                };
                self.tt.func(ps, r)
            }
            K::Tuple(ts) => {
                let ts = ts.iter().map(|p| self.resolve_type(p)).collect();
                self.tt.tuple(ts)
            }
        }
    }

    /// Map keys: numbers, bool, char, string, plain enums, pointers.
    pub(crate) fn is_map_key(&self, t: TypeId) -> bool {
        match self.tt.get(t) {
            Type::Int(_) | Type::Float32 | Type::Float64 | Type::Bool | Type::Char | Type::String | Type::Pointer(_) | Type::Invalid => true,
            Type::Enum(e) => self.enums.get(e.0 as usize).is_none_or(|e| e.is_plain),
            Type::Param(_) => self.param_comparable(t),
            _ => false,
        }
    }

    /// The type named by a path resolution, if it names a type.
    pub(crate) fn path_res_type(&mut self, r: PathRes) -> Option<TypeId> {
        use crate::types::DeclId;
        Some(match r {
            PathRes::BuiltinType(t) => t,
            PathRes::Builtin(BuiltinFn::Error) => self.tt.error,
            PathRes::Member(Member::Struct(s)) => self.tt.intern(Type::Struct(DeclId(s))),
            PathRes::Member(Member::Interface(i)) => {
                self.force_iface(i);
                self.tt.intern(Type::Interface(DeclId(i)))
            }
            PathRes::Member(Member::Enum(e)) => self.tt.intern(Type::Enum(DeclId(e))),
            PathRes::Member(Member::Alias(a)) => self.alias_type(a),
            _ => return None,
        })
    }

    pub(crate) fn struct_ty(&mut self, s: u32) -> TypeId {
        self.tt.intern(Type::Struct(crate::types::DeclId(s)))
    }

    pub(crate) fn enum_ty(&mut self, e: u32) -> TypeId {
        self.tt.intern(Type::Enum(crate::types::DeclId(e)))
    }

    // -- signatures -------------------------------------------------------

    /// Resolves struct fields, enum payloads, interface methods and
    /// function signatures.
    pub(crate) fn resolve_signatures(&mut self) {
        for sid in 0..self.structs.len() {
            if self.gen.inst_structs.contains_key(&(sid as u32)) {
                continue;
            }
            self.cur_file = self.structs[sid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            self.resolve_struct_fields(sid);
        }
        self.check_recursive_structs();
        for eid in 0..self.enums.len() {
            if self.gen.inst_enums.contains_key(&(eid as u32)) {
                continue;
            }
            self.cur_file = self.enums[eid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            self.resolve_enum_variants(eid);
        }
        for iid in 0..self.interfaces.len() {
            if self.std_iface_lazy.contains_key(&(iid as u32)) {
                continue;
            }
            self.cur_file = self.interfaces[iid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            self.resolve_iface_methods(iid);
        }
        self.resolve_interface_embeds();

        for fid in 0..self.funcs.len() {
            if self.gen.insts.contains_key(&(fid as u32)) || matches!(self.func_src[fid], FuncSrc::Synthetic) {
                continue;
            }
            self.cur_file = self.funcs[fid].file;
            self.cur_ns = self.std_fn_ns.get(&(fid as u32)).copied().unwrap_or(self.file_scopes[self.cur_file].ns);
            self.resolve_func_sig(fid);
        }
    }

    /// Resolves the method signatures of interface `iid` in the scope of
    /// its file and namespace (set by the caller).
    fn resolve_iface_methods(&mut self, iid: usize) {
        let src = self.iface_src[iid];
        let mut ms: Vec<hir::MethodSig> = Vec::new();
        for m in &src.methods {
            self.reject_generics(&m.generics, "interface methods");
            if ms.iter().any(|x| x.name == m.name.name) {
                self.err(format!("duplicate method {} in interface {}", m.name.name, src.name.name), m.name.span);
                continue;
            }
            // (Stdlib interfaces have no declaration span: not recorded.)
            if self.interfaces[iid].span != Span::DUMMY {
                self.ide_def(index::SymKey::IfaceMethod(iid as u32, ms.len() as u32), &m.name, m.span, Some(index::SymKey::Interface(iid as u32)));
            }
            let params: Vec<TypeId> = m.params.iter().map(|p| self.resolve_type(&p.ty)).collect();
            let ret = m.ret.as_ref().map(|r| self.resolve_type(r)).unwrap_or(self.tt.void);
            // `async func M() R` in an interface is sugar for
            // `func M() Task<R>` (DESIGN.md "Интерфейсы").
            let ret = if m.is_async && m.body.is_none() { self.tt.task(ret) } else { ret };
            let default = if m.body.is_some() { self.iface_defaults.get(&(iid as u32, m.name.name.clone())).copied() } else { None };
            ms.push(hir::MethodSig { name: m.name.name.clone(), params, ret, default });
        }
        self.interfaces[iid].methods = ms;
    }

    /// Resolves the signatures of stdlib interface `iid` on its first use
    /// (see `std_iface_lazy`); a no-op for every other interface.
    pub(crate) fn force_iface(&mut self, iid: u32) {
        let Some(ns) = self.std_iface_lazy.remove(&iid) else { return };
        let saved = (self.cur_file, self.cur_ns);
        self.cur_file = 0;
        self.cur_ns = ns;
        self.resolve_iface_methods(iid as usize);
        // Embedded interfaces (none in the stdlib today): their methods
        // follow the interface's own, as resolve_interface_embeds does.
        let src = self.iface_src[iid as usize];
        for e in &src.embeds {
            let t = self.resolve_type(e);
            if let Type::Interface(d) = *self.tt.get(t) {
                for m in self.interfaces[d.0 as usize].methods.clone() {
                    if !self.interfaces[iid as usize].methods.iter().any(|x| x.name == m.name) {
                        self.interfaces[iid as usize].methods.push(m);
                    }
                }
            }
        }
        (self.cur_file, self.cur_ns) = saved;
    }

    /// Adds the methods of embedded interfaces (`interface RW { Reader;
    /// Writer }`), transitively, after the interface's own methods in the
    /// order written. A method embedded twice must have the same signature.
    fn resolve_interface_embeds(&mut self) {
        let mut embeds: Vec<Vec<(u32, Span)>> = Vec::new();
        for iid in 0..self.interfaces.len() {
            let src = self.iface_src[iid];
            let mut v = Vec::new();
            if self.std_iface_lazy.contains_key(&(iid as u32)) {
                embeds.push(v);
                continue;
            }
            self.cur_file = self.interfaces[iid].file;
            self.cur_ns = self.file_scopes[self.cur_file].ns;
            for e in &src.embeds {
                let t = self.resolve_type(e);
                match *self.tt.get(t) {
                    Type::Interface(d) if d.0 as usize == iid => self.err(format!("interface {} embeds itself", src.name.name), e.span),
                    Type::Interface(d) => v.push((d.0, e.span)),
                    Type::Invalid => {}
                    _ => self.err(format!("only interfaces can be embedded in an interface, found {}", self.tn(t)), e.span),
                }
            }
            embeds.push(v);
        }
        for (i, other) in self.embedding_cycles(&embeds) {
            embeds[i].retain(|(o, _)| *o != other);
        }
        // Fixpoint: nested embeddings settle in at most `n` rounds.
        for _ in 0..=self.interfaces.len() {
            let mut changed = false;
            for iid in 0..self.interfaces.len() {
                for &(other, span) in &embeds[iid] {
                    for m in self.interfaces[other as usize].methods.clone() {
                        match self.interfaces[iid].methods.iter().find(|x| x.name == m.name) {
                            Some(x) if x.params == m.params && x.ret == m.ret => {}
                            Some(_) => {
                                self.cur_file = self.interfaces[iid].file;
                                let name = self.interfaces[iid].name.clone();
                                self.err(format!("interface {name} has conflicting signatures for method {}", m.name), span);
                                self.interfaces[iid].methods.retain(|x| x.name != m.name || x.params == m.params);
                            }
                            None => {
                                self.interfaces[iid].methods.push(m);
                                changed = true;
                            }
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Resolves the fields of struct `sid` in the current scope.
    pub(crate) fn resolve_struct_fields(&mut self, sid: usize) {
        let src = self.struct_src[sid];
        let mut fields: Vec<hir::StructField> = Vec::new();
        for e in &src.embeds {
            let ty = self.resolve_type(e);
            let name = match (&e.kind, self.tt.get(ty)) {
                (ast::TypeKind::Named { path, .. }, Type::Struct(_)) => path.segments.last().unwrap().name.clone(),
                _ => {
                    if !self.tt.is_invalid(ty) {
                        self.err("only structs can be embedded", e.span);
                    }
                    continue;
                }
            };
            fields.push(hir::StructField { exported: is_exported(&name), name, ty, embedded: true, default: None, attrs: Vec::new(), span: e.span });
        }
        for f in &src.fields {
            let ty = self.resolve_type(&f.ty);
            if fields.iter().any(|x| x.name == f.name.name) {
                self.err(format!("duplicate field {} in struct {}", f.name.name, src.name.name), f.name.span);
                continue;
            }
            if self.structs[sid].span != Span::DUMMY {
                self.ide_def(index::SymKey::Field(sid as u32, fields.len() as u32), &f.name, f.span, Some(index::SymKey::Struct(sid as u32)));
            }
            fields.push(hir::StructField {
                name: f.name.name.clone(),
                ty,
                embedded: false,
                exported: is_exported(&f.name.name),
                default: None,
                attrs: Vec::new(),
                span: f.span,
            });
        }
        let methods: Vec<String> = self.structs[sid].methods.iter().map(|m| m.0.clone()).collect();
        for name in &methods {
            if let Some(f) = fields.iter().find(|f| f.name == *name) {
                let span = f.span;
                self.err(format!("struct {} has both a field and a method named {name}", src.name.name), span);
            }
        }
        self.structs[sid].fields = fields;
    }

    /// Resolves the variants of enum `eid` in the current scope.
    pub(crate) fn resolve_enum_variants(&mut self, eid: usize) {
        let src = self.enum_src[eid];
        let mut vs: Vec<hir::Variant> = Vec::new();
        for v in &src.variants {
            if vs.iter().any(|x| x.name == v.name.name) {
                self.err(format!("duplicate variant {} in enum {}", v.name.name, src.name.name), v.name.span);
                continue;
            }
            self.ide_def(index::SymKey::Variant(eid as u32, vs.len() as u32), &v.name, v.span, Some(index::SymKey::Enum(eid as u32)));
            let fields = v.fields.iter().map(|f| (f.name.name.clone(), self.resolve_type(&f.ty))).collect();
            vs.push(hir::Variant { name: v.name.name.clone(), fields, attrs: Vec::new(), span: v.span });
        }
        self.enums[eid].variants = vs;
    }

    /// Resolves the signature (parameters as locals, result) of user
    /// function `fid` in the current scope.
    pub(crate) fn resolve_func_sig(&mut self, fid: usize) {
        let (decl, struct_id) = match self.func_src[fid] {
            FuncSrc::Free(d) => (d, None),
            FuncSrc::Method(d, s) => (d, Some(s)),
            FuncSrc::Synthetic => return,
        };
        let mut locals = Vec::new();
        let mut params = Vec::new();
        if let (Some(s), false) = (struct_id, decl.is_static) {
            let st = self.struct_ty(s);
            let ty = self.tt.pointer(st);
            locals.push(hir::Local { name: "this".into(), ty, kind: LocalKind::This, captured: false, address_taken: false, span: decl.name.span });
        }
        if let Some(&e) = self.enum_method_owner.get(&(fid as u32)) {
            // Enum methods receive the value itself.
            let ty = self.enum_ty(e);
            locals.push(hir::Local { name: "this".into(), ty, kind: LocalKind::This, captured: false, address_taken: false, span: decl.name.span });
        }
        if let Some(&i) = self.iface_default_owner.get(&(fid as u32)) {
            // Default methods receive the interface value.
            self.force_iface(i);
            let ty = self.tt.intern(Type::Interface(crate::types::DeclId(i)));
            locals.push(hir::Local { name: "this".into(), ty, kind: LocalKind::This, captured: false, address_taken: false, span: decl.name.span });
        }
        for (i, p) in decl.params.iter().enumerate() {
            let ty = match &p.ty.kind {
                ast::TypeKind::Variadic(x) if i + 1 == decl.params.len() && decl.generics.is_empty() => {
                    let el = self.resolve_type(x);
                    self.variadic.insert(fid as u32);
                    self.tt.slice(el)
                }
                _ => self.resolve_type(&p.ty),
            };
            if p.name.name != "_" && locals.iter().any(|l: &hir::Local| l.name == p.name.name) {
                self.err(format!("duplicate parameter {}", p.name.name), p.name.span);
            }
            params.push(ty);
            if p.name.name != "_" {
                self.ide_def(index::SymKey::Local(fid as u32, locals.len() as u32), &p.name, p.span, Some(index::SymKey::Func(fid as u32)));
            }
            locals.push(hir::Local { name: p.name.name.clone(), ty, kind: LocalKind::Param, captured: false, address_taken: false, span: p.span });
        }
        let ret = decl.ret.as_ref().map(|r| self.resolve_type(r)).unwrap_or(self.tt.void);
        let f = &mut self.funcs[fid];
        f.params = (0..locals.len()).map(|i| LocalId(i as u32)).collect();
        f.locals = locals;
        f.ret = ret;
        self.sigs[fid] = Sig { params, ret, is_async: decl.is_async };
    }

    /// Reports structs that contain themselves by value.
    fn check_recursive_structs(&mut self) {
        for sid in 0..self.structs.len() {
            self.check_recursive_struct(sid);
        }
    }

    /// Reports struct `sid` if it contains itself by value.
    pub(crate) fn check_recursive_struct(&mut self, sid: usize) {
        fn contains(s: &Sema, target: u32, ty: TypeId, seen: &mut Vec<u32>) -> bool {
            match s.tt.get(ty) {
                Type::Struct(d) => {
                    if d.0 == target {
                        return true;
                    }
                    if seen.contains(&d.0) {
                        return false;
                    }
                    seen.push(d.0);
                    s.structs[d.0 as usize].fields.iter().any(|f| contains(s, target, f.ty, seen))
                }
                Type::Array(t, _) => contains(s, target, *t, seen),
                Type::Tuple(ts) => ts.iter().any(|t| contains(s, target, *t, seen)),
                Type::Enum(e) => s.enums.get(e.0 as usize).is_some_and(|e| e.variants.iter().any(|v| v.fields.iter().any(|f| contains(s, target, f.1, seen)))),
                _ => false,
            }
        }
        let mut seen = Vec::new();
        if self.structs[sid].fields.iter().any(|f| contains(self, sid as u32, f.ty, &mut seen)) {
            let saved = self.cur_file;
            if !self.gen.inst_structs.contains_key(&(sid as u32)) {
                self.cur_file = self.structs[sid].file;
            }
            let name = self.structs[sid].name.clone();
            let span = self.struct_src[sid].name.span;
            self.err(format!("invalid recursive type {name}: it contains itself by value (use a pointer)"), span);
            self.cur_file = saved;
        }
    }

    // -- struct members ------------------------------------------------------

    /// Path of field indices to field `name` of struct `sid`, following
    /// embedded structs (shallowest match wins).
    pub(crate) fn find_field(&self, sid: u32, name: &str) -> Option<Vec<u32>> {
        self.find_field_at(sid, name, 0)
    }

    fn find_field_at(&self, sid: u32, name: &str, depth: usize) -> Option<Vec<u32>> {
        let st = &self.structs[sid as usize];
        if let Some(i) = st.fields.iter().position(|f| f.name == name) {
            return Some(vec![i as u32]);
        }
        // Embedding cycles are reported elsewhere; stop following them.
        if depth > MAX_EMBED_DEPTH {
            return None;
        }
        let mut best: Option<Vec<u32>> = None;
        for (i, f) in st.fields.iter().enumerate().filter(|(_, f)| f.embedded) {
            if let Type::Struct(d) = self.tt.get(f.ty) {
                if let Some(mut p) = self.find_field_at(d.0, name, depth + 1) {
                    p.insert(0, i as u32);
                    if best.as_ref().is_none_or(|b| p.len() < b.len()) {
                        best = Some(p);
                    }
                }
            }
        }
        best
    }

    /// Non-static method `name` of struct `sid`, possibly promoted through
    /// embedded fields: (function, embedded field path).
    pub(crate) fn find_method(&self, sid: u32, name: &str) -> Option<(FuncId, Vec<u32>)> {
        self.find_method_at(sid, name, 0)
    }

    fn find_method_at(&self, sid: u32, name: &str, depth: usize) -> Option<(FuncId, Vec<u32>)> {
        let st = &self.structs[sid as usize];
        if let Some((_, f)) = st.methods.iter().find(|(n, _)| n == name) {
            if !self.is_static(*f) {
                return Some((*f, Vec::new()));
            }
        }
        if depth > MAX_EMBED_DEPTH {
            return None;
        }
        let mut best: Option<(FuncId, Vec<u32>)> = None;
        for (i, f) in st.fields.iter().enumerate().filter(|(_, f)| f.embedded) {
            if let Type::Struct(d) = self.tt.get(f.ty) {
                if let Some((m, mut p)) = self.find_method_at(d.0, name, depth + 1) {
                    p.insert(0, i as u32);
                    if best.as_ref().is_none_or(|b| p.len() < b.1.len()) {
                        best = Some((m, p));
                    }
                }
            }
        }
        best
    }
}

// -- package isolation hook ---------------------------------------------------

/// Strict dependency isolation for package builds (docs/PACKAGES.md,
/// "строгая изоляция"): a file may reach, by `import` or by a fully
/// qualified path, only the standard library roots, the namespace root of its own package and
/// the roots of the packages its manifest declares as dependencies.
///
/// Built by `crate::package` and passed to [`super::analyze_with_policy`].
/// Without a policy (single-file and plain-directory builds) every
/// namespace is visible. Package namespace roots are single identifiers
/// owned by exactly one package, so checking the first path segment is
/// enough.
#[derive(Clone, Debug, Default)]
pub struct ImportPolicy {
    /// For each file (same index as the files given to sema): its package,
    /// an index into `packages`.
    pub file_package: Vec<usize>,
    pub packages: Vec<PackageScope>,
}

/// One package of an [`ImportPolicy`].
#[derive(Clone, Debug)]
pub struct PackageScope {
    /// Package name (for messages).
    pub name: String,
    /// Root namespace owned by the package.
    pub namespace: String,
    /// Declared (direct) dependencies, indices into `ImportPolicy::packages`.
    pub deps: Vec<usize>,
    /// Capabilities granted by the manifest (`[capabilities]`: `unsafe`,
    /// `plugin`), see [`super::debug_agent::GATED`].
    pub capabilities: Vec<String>,
}

impl ImportPolicy {
    /// Whether the package of file `file` was granted capability `cap`.
    pub fn has_capability(&self, file: usize, cap: &str) -> bool {
        self.file_package.get(file).and_then(|&i| self.packages.get(i)).is_some_and(|p| p.capabilities.iter().any(|c| c == cap))
    }

    /// `Err(message)` if code in file `file` may not reference the
    /// namespace `path` (or anything under its root).
    pub fn check(&self, file: usize, path: &str) -> Result<(), String> {
        let root = path.split('.').next().unwrap_or(path);
        let Some(&me) = self.file_package.get(file) else { return Ok(()) };
        if super::stdlib::is_std_namespace(root) {
            return Ok(());
        }
        let Some(owner) = self.packages.iter().position(|p| p.namespace == root) else {
            // Not owned by any package: sema reports unknown names itself.
            return Ok(());
        };
        let pkg = &self.packages[me];
        if owner == me || pkg.deps.contains(&owner) {
            return Ok(());
        }
        let on = &self.packages[owner].name;
        let via = self.transitive_via(me, owner).map(|v| format!(" (it is only a transitive dependency, through `{}`)", self.packages[v].name)).unwrap_or_default();
        Err(format!(
            "package `{}` cannot use namespace `{path}`: it belongs to package `{on}`, which is not a declared dependency of `{}`{via}; add `{on}` to [dependencies] in the manifest of `{}`",
            pkg.name, pkg.name, pkg.name
        ))
    }

    /// The direct dependency of `from` through which `to` is reachable.
    fn transitive_via(&self, from: usize, to: usize) -> Option<usize> {
        for &d in &self.packages[from].deps {
            let mut seen = vec![false; self.packages.len()];
            let mut stack = vec![d];
            while let Some(p) = stack.pop() {
                if p == to {
                    return Some(d);
                }
                if !std::mem::replace(&mut seen[p], true) {
                    stack.extend(self.packages[p].deps.iter().copied());
                }
            }
        }
        None
    }
}
