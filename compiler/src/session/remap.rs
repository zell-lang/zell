//! Copying types and type declarations between programs.
//!
//! Every analysis run has its own type table and declaration ids. What the
//! tools need of a program (the types of symbols, the fields of structs,
//! the methods of interfaces, the variants of enums) is copied into a
//! [`DeclStore`] where declarations are identified by a *key* (their full
//! name, plus `#n` for the n-th duplicate of a name in one program), so the
//! results of many runs share one table.

use crate::hir::{self, Program};
use crate::types::{DeclId, FuncSig, Type, TypeId, TypeTable};
use std::collections::HashMap;

/// A program holding only types and type declarations (no functions).
pub(crate) fn empty_program() -> Program {
    Program {
        types: TypeTable::new(),
        namespaces: Vec::new(),
        structs: Vec::new(),
        interfaces: Vec::new(),
        enums: Vec::new(),
        funcs: Vec::new(),
        globals: Vec::new(),
        global_init_order: Vec::new(),
        externs: Vec::new(),
        entry: None,
        sources: Vec::new(),
        reflect: None,
        lint_notes: Vec::new(),
    }
}

/// Keys of the declarations of a program (see the module docs).
#[derive(Clone, Debug, Default)]
pub(crate) struct Keys {
    pub structs: Vec<String>,
    pub ifaces: Vec<String>,
    pub enums: Vec<String>,
}

impl Keys {
    /// Keys of the declarations of `p`: full names, duplicates numbered.
    pub fn of(p: &Program) -> Keys {
        fn keys<'n>(names: impl Iterator<Item = &'n str>) -> Vec<String> {
            let mut seen: HashMap<&str, u32> = HashMap::new();
            names
                .map(|n| {
                    let c = seen.entry(n).or_insert(0);
                    *c += 1;
                    if *c == 1 {
                        n.to_string()
                    } else {
                        format!("{n}#{c}")
                    }
                })
                .collect()
        }
        Keys {
            structs: keys(p.structs.iter().map(|s| s.full_name.as_str())),
            ifaces: keys(p.interfaces.iter().map(|s| s.full_name.as_str())),
            enums: keys(p.enums.iter().map(|s| s.full_name.as_str())),
        }
    }
}

/// Types and declarations identified by key.
pub(crate) struct DeclStore {
    pub prog: Program,
    pub keys: Keys,
    structs: HashMap<String, u32>,
    ifaces: HashMap<String, u32>,
    enums: HashMap<String, u32>,
    /// Type parameters by name (their ids differ between runs).
    params: HashMap<String, u32>,
}

impl Default for DeclStore {
    fn default() -> DeclStore {
        DeclStore { prog: empty_program(), keys: Keys::default(), structs: HashMap::new(), ifaces: HashMap::new(), enums: HashMap::new(), params: HashMap::new() }
    }
}

impl DeclStore {
    pub fn types_len(&self) -> usize {
        self.prog.types.len()
    }

    /// Names of the type parameters by id.
    pub fn param_names(&self) -> Vec<String> {
        let mut v = vec![String::new(); self.params.len()];
        for (n, &i) in &self.params {
            v[i as usize] = n.clone();
        }
        v
    }
}

/// Where the types and declarations of one source program went.
#[derive(Clone, Debug, Default)]
pub(crate) struct Mapping {
    pub types: HashMap<TypeId, TypeId>,
    pub structs: HashMap<u32, u32>,
    pub ifaces: HashMap<u32, u32>,
    pub enums: HashMap<u32, u32>,
}

/// Copies from `src` (with keys `keys`, type parameter names `params`)
/// into `dst`. With `overwrite`, declarations already in `dst` get the
/// contents of `src` (the newer analysis); otherwise they are kept.
pub(crate) struct Copier<'s> {
    pub src: &'s Program,
    pub keys: &'s Keys,
    pub params: &'s [String],
    pub overwrite: bool,
    pub map: Mapping,
}

impl<'s> Copier<'s> {
    pub fn new(src: &'s Program, keys: &'s Keys, params: &'s [String], overwrite: bool) -> Copier<'s> {
        Copier { src, keys, params, overwrite, map: Mapping::default() }
    }

    pub fn ty(&mut self, dst: &mut DeclStore, t: TypeId) -> TypeId {
        if let Some(&d) = self.map.types.get(&t) {
            return d;
        }
        let ty = match self.src.types.get(t).clone() {
            Type::Pointer(x) => Type::Pointer(self.ty(dst, x)),
            Type::Weak(x) => Type::Weak(self.ty(dst, x)),
            Type::Slice(x) => Type::Slice(self.ty(dst, x)),
            Type::Array(x, n) => Type::Array(self.ty(dst, x), n),
            Type::Map(k, v) => {
                let k = self.ty(dst, k);
                Type::Map(k, self.ty(dst, v))
            }
            Type::Tuple(ts) => Type::Tuple(ts.into_iter().map(|x| self.ty(dst, x)).collect()),
            Type::Fallible(x) => Type::Fallible(self.ty(dst, x)),
            Type::Task(x) => Type::Task(self.ty(dst, x)),
            Type::Func(sig) => {
                let params = sig.params.into_iter().map(|x| self.ty(dst, x)).collect();
                Type::Func(FuncSig { params, ret: self.ty(dst, sig.ret) })
            }
            Type::Struct(d) => Type::Struct(DeclId(self.strukt(dst, d.0))),
            Type::Interface(d) => Type::Interface(DeclId(self.iface(dst, d.0))),
            Type::Enum(d) => Type::Enum(DeclId(self.enm(dst, d.0))),
            Type::Param(p) => {
                let name = self.params.get(p as usize).cloned().unwrap_or_else(|| format!("T#{p}"));
                let n = dst.params.len() as u32;
                Type::Param(*dst.params.entry(name).or_insert(n))
            }
            other => other,
        };
        let d = dst.prog.types.intern(ty);
        self.map.types.insert(t, d);
        d
    }

    pub fn strukt(&mut self, dst: &mut DeclStore, sid: u32) -> u32 {
        if let Some(&d) = self.map.structs.get(&sid) {
            return d;
        }
        let Some(s) = self.src.structs.get(sid as usize) else { return 0 };
        let key = self.keys.structs.get(sid as usize).cloned().unwrap_or_else(|| s.full_name.clone());
        let (d, fill) = match dst.structs.get(&key) {
            Some(&d) => (d, self.overwrite),
            None => {
                let d = dst.prog.structs.len() as u32;
                dst.prog.structs.push(hir::Struct { fields: Vec::new(), methods: Vec::new(), attrs: Vec::new(), ..s.clone_shallow() });
                dst.keys.structs.push(key.clone());
                dst.structs.insert(key, d);
                (d, true)
            }
        };
        self.map.structs.insert(sid, d);
        if fill {
            let fields = s
                .fields
                .iter()
                .map(|f| hir::StructField { name: f.name.clone(), ty: self.ty(dst, f.ty), embedded: f.embedded, exported: f.exported, default: None, attrs: Vec::new(), span: f.span })
                .collect();
            let t = &mut dst.prog.structs[d as usize];
            *t = hir::Struct { fields, methods: Vec::new(), attrs: Vec::new(), ..s.clone_shallow() };
        }
        d
    }

    pub fn iface(&mut self, dst: &mut DeclStore, iid: u32) -> u32 {
        if let Some(&d) = self.map.ifaces.get(&iid) {
            return d;
        }
        let Some(s) = self.src.interfaces.get(iid as usize) else { return 0 };
        let key = self.keys.ifaces.get(iid as usize).cloned().unwrap_or_else(|| s.full_name.clone());
        let shallow = || hir::Interface {
            name: s.name.clone(),
            full_name: s.full_name.clone(),
            namespace: hir::NamespaceId(0),
            methods: Vec::new(),
            sealed: s.sealed,
            attrs: Vec::new(),
            exported: s.exported,
            file: 0,
            span: s.span,
        };
        let (d, fill) = match dst.ifaces.get(&key) {
            Some(&d) => (d, self.overwrite),
            None => {
                let d = dst.prog.interfaces.len() as u32;
                dst.prog.interfaces.push(shallow());
                dst.keys.ifaces.push(key.clone());
                dst.ifaces.insert(key, d);
                (d, true)
            }
        };
        self.map.ifaces.insert(iid, d);
        if fill {
            let methods = s
                .methods
                .iter()
                .map(|m| hir::MethodSig { name: m.name.clone(), params: m.params.iter().map(|t| self.ty(dst, *t)).collect(), ret: self.ty(dst, m.ret), default: None })
                .collect();
            dst.prog.interfaces[d as usize] = hir::Interface { methods, ..shallow() };
        }
        d
    }

    pub fn enm(&mut self, dst: &mut DeclStore, eid: u32) -> u32 {
        if let Some(&d) = self.map.enums.get(&eid) {
            return d;
        }
        let Some(s) = self.src.enums.get(eid as usize) else { return 0 };
        let key = self.keys.enums.get(eid as usize).cloned().unwrap_or_else(|| s.full_name.clone());
        let shallow = || hir::Enum {
            name: s.name.clone(),
            full_name: s.full_name.clone(),
            namespace: hir::NamespaceId(0),
            variants: Vec::new(),
            is_plain: s.is_plain,
            methods: Vec::new(),
            attrs: Vec::new(),
            exported: s.exported,
            file: 0,
            span: s.span,
        };
        let (d, fill) = match dst.enums.get(&key) {
            Some(&d) => (d, self.overwrite),
            None => {
                let d = dst.prog.enums.len() as u32;
                dst.prog.enums.push(shallow());
                dst.keys.enums.push(key.clone());
                dst.enums.insert(key, d);
                (d, true)
            }
        };
        self.map.enums.insert(eid, d);
        if fill {
            let variants = s
                .variants
                .iter()
                .map(|v| hir::Variant { name: v.name.clone(), fields: v.fields.iter().map(|(n, t)| (n.clone(), self.ty(dst, *t))).collect(), attrs: Vec::new(), span: v.span })
                .collect();
            dst.prog.enums[d as usize] = hir::Enum { variants, ..shallow() };
        }
        d
    }
}

trait Shallow {
    fn clone_shallow(&self) -> Self;
}

impl Shallow for hir::Struct {
    /// The struct without fields, methods and attributes.
    fn clone_shallow(&self) -> hir::Struct {
        hir::Struct {
            name: self.name.clone(),
            full_name: self.full_name.clone(),
            namespace: hir::NamespaceId(0),
            fields: Vec::new(),
            methods: Vec::new(),
            attrs: Vec::new(),
            is_attribute: self.is_attribute,
            exported: self.exported,
            file: 0,
            span: self.span,
        }
    }
}
