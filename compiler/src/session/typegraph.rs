//! The type hierarchy of an [`super::Analysis`]: which structs, enums and
//! interfaces satisfy which interfaces, related over the whole program
//! (user and std shapes) when the fragments are assembled.
//!
//! Interfaces are structural (DESIGN.md "Интерфейсы"): a struct or enum
//! satisfies an interface when it has every method the interface has no
//! default body for, and every method of that name it has carries the
//! interface's signature (methods promoted from embedded structs count,
//! the shallowest wins). An interface is a subtype of another when it has
//! all of that one's methods (decision 18, implicit conversion). A sealed
//! interface is satisfied only by types of its namespace (and, of
//! interfaces, only by sealed ones). An embedded struct is a supertype of
//! the struct embedding it. Interfaces with no method to implement (empty,
//! or defaults only) relate to everything and are left out.
//!
//! The relation is recomputed on every assembly from the shapes, so an
//! interface edited in one namespace updates types anywhere, imported or
//! not, without re-analysing them.

use crate::sema::shapes::{MethodShape, ShapeKind, TypeShape};
use std::collections::HashMap;

/// Types and their direct super- and subtypes (indices into `types`,
/// sorted by name).
#[derive(Debug, Default)]
pub struct TypeGraph {
    pub types: Vec<TypeShape>,
    by_name: HashMap<String, u32>,
    supers: Vec<Vec<u32>>,
    subs: Vec<Vec<u32>>,
}

/// Maximum embedding depth followed for promoted methods.
const MAX_EMBED: usize = 8;

impl TypeGraph {
    /// Relates `types` (the first shape of a name wins).
    pub fn build(shapes: Vec<TypeShape>) -> TypeGraph {
        let mut g = TypeGraph::default();
        for s in shapes {
            if g.by_name.contains_key(&s.name) {
                continue;
            }
            g.by_name.insert(s.name.clone(), g.types.len() as u32);
            g.types.push(s);
        }
        let n = g.types.len();
        g.supers = vec![Vec::new(); n];
        g.subs = vec![Vec::new(); n];
        let flat: Vec<Vec<MethodShape>> = (0..n as u32).map(|t| g.methods(t)).collect();
        // Candidates of an interface: types with a method of the name of
        // its first required method.
        let mut by_method: HashMap<&str, Vec<u32>> = HashMap::new();
        for (t, ms) in flat.iter().enumerate() {
            for m in ms {
                by_method.entry(m.name.as_str()).or_default().push(t as u32);
            }
        }
        let mut edges: Vec<(u32, u32)> = Vec::new();
        for (i, it) in g.types.iter().enumerate() {
            if it.kind != ShapeKind::Interface {
                continue;
            }
            let Some(first) = it.methods.iter().find(|m| !m.default) else { continue };
            for &c in by_method.get(first.name.as_str()).map_or(&[][..], |v| &v[..]) {
                if c as usize != i && satisfies(&g.types[c as usize], &flat[c as usize], it) {
                    edges.push((c, i as u32));
                }
            }
        }
        for (s, t) in g.types.iter().enumerate() {
            for (e, _) in &t.embeds {
                if let Some(&d) = g.by_name.get(e) {
                    if d as usize != s {
                        edges.push((s as u32, d));
                    }
                }
            }
        }
        edges.sort();
        edges.dedup();
        for (sub, sup) in edges {
            g.supers[sub as usize].push(sup);
            g.subs[sup as usize].push(sub);
        }
        let types = &g.types;
        for v in g.supers.iter_mut().chain(g.subs.iter_mut()) {
            v.sort_by(|a, b| types[*a as usize].name.cmp(&types[*b as usize].name));
        }
        g
    }

    /// The type with full declaration name `name` (no type arguments).
    pub fn find(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }

    /// Direct supertypes: the interfaces a type satisfies, the structs it
    /// embeds.
    pub fn supertypes(&self, t: u32) -> &[u32] {
        self.supers.get(t as usize).map_or(&[], |v| &v[..])
    }

    /// Direct subtypes: the types satisfying an interface, the structs
    /// embedding a struct.
    pub fn subtypes(&self, t: u32) -> &[u32] {
        self.subs.get(t as usize).map_or(&[], |v| &v[..])
    }

    /// The structs and enums satisfying interface `t`.
    pub fn implementors(&self, t: u32) -> Vec<u32> {
        if self.types.get(t as usize).is_none_or(|x| x.kind != ShapeKind::Interface) {
            return Vec::new();
        }
        self.subtypes(t).iter().copied().filter(|&s| self.types[s as usize].kind != ShapeKind::Interface).collect()
    }

    /// The method set of type `t`: own methods, then those promoted from
    /// embedded structs (the shallowest declaration of a name wins), type
    /// arguments of embedded generic structs substituted.
    pub fn methods(&self, t: u32) -> Vec<MethodShape> {
        let mut out: Vec<MethodShape> = Vec::new();
        let mut level: Vec<(u32, Vec<String>)> = vec![(t, Vec::new())];
        let mut seen = vec![t];
        for _ in 0..=MAX_EMBED {
            let mut next = Vec::new();
            let mut found: Vec<MethodShape> = Vec::new();
            for (ty, args) in &level {
                let Some(s) = self.types.get(*ty as usize) else { continue };
                for m in &s.methods {
                    if !out.iter().any(|o| o.name == m.name) && !found.iter().any(|o| o.name == m.name) {
                        found.push(MethodShape { sig: subst(&m.sig, args), ..m.clone() });
                    }
                }
                for (e, eargs) in &s.embeds {
                    if let Some(&d) = self.by_name.get(e) {
                        if !seen.contains(&d) {
                            seen.push(d);
                            next.push((d, eargs.iter().map(|a| subst(a, args)).collect()));
                        }
                    }
                }
            }
            out.extend(found);
            if next.is_empty() {
                break;
            }
            level = next;
        }
        out
    }
}

/// `sig` with `$i` replaced by `args[i]` (unchanged without arguments).
fn subst(sig: &str, args: &[String]) -> String {
    if args.is_empty() || !sig.contains('$') {
        return sig.to_string();
    }
    let b = sig.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let j = (i + 1..b.len()).find(|&j| !b[j].is_ascii_digit()).unwrap_or(b.len());
            if let Some(a) = sig[i + 1..j].parse::<usize>().ok().and_then(|n| args.get(n)) {
                out.push_str(a);
                i = j;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// Whether type `c` (method set `ms`) satisfies interface `it`.
fn satisfies(c: &TypeShape, ms: &[MethodShape], it: &TypeShape) -> bool {
    if it.sealed && (c.ns != it.ns || c.is_std != it.is_std || (c.kind == ShapeKind::Interface && !c.sealed)) {
        return false;
    }
    let (mut pat, mut text) = (String::new(), String::new());
    for m in &it.methods {
        match ms.iter().find(|h| h.name == m.name) {
            Some(h) => {
                pat.push_str(&h.sig);
                pat.push('\n');
                text.push_str(&m.sig);
                text.push('\n');
            }
            None if m.default => {}
            None => return false,
        }
    }
    if !pat.contains('$') {
        return pat == text;
    }
    let mut b: Vec<Option<String>> = Vec::new();
    unify(pat.as_bytes(), text.as_bytes(), &mut b)
}

/// Matches `p` (with type parameters `$i`) against the concrete `t`, each
/// parameter standing for one balanced type, the same one everywhere.
fn unify(p: &[u8], t: &[u8], b: &mut Vec<Option<String>>) -> bool {
    let Some(&c) = p.first() else { return t.is_empty() };
    if c != b'$' {
        return t.first() == Some(&c) && unify(&p[1..], &t[1..], b);
    }
    let j = (1..p.len()).find(|&j| !p[j].is_ascii_digit()).unwrap_or(p.len());
    let Ok(n) = std::str::from_utf8(&p[1..j]).unwrap_or("").parse::<usize>() else { return false };
    let rest = &p[j..];
    if b.len() <= n {
        b.resize(n + 1, None);
    }
    if let Some(v) = b[n].clone() {
        return t.starts_with(v.as_bytes()) && unify(rest, &t[v.len()..], b);
    }
    let mut depth = 0i32;
    for end in 1..=t.len() {
        match t[end - 1] {
            b'<' | b'[' | b'(' | b'{' => depth += 1,
            b'>' | b']' | b')' | b'}' => depth -= 1,
            b'\n' | b',' if depth == 0 => break,
            _ => {}
        }
        if depth < 0 {
            break;
        }
        if depth == 0 {
            b[n] = Some(String::from_utf8_lossy(&t[..end]).into_owned());
            if unify(rest, &t[end..], b) {
                return true;
            }
        }
    }
    b[n] = None;
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_parameters_unify_consistently() {
        let mut b = Vec::new();
        assert!(unify(b"() $0\n($0) void\n", b"() []int\n([]int) void\n", &mut b));
        assert_eq!(b[0].as_deref(), Some("[]int"));
        let mut b = Vec::new();
        assert!(!unify(b"() $0\n($0) void\n", b"() int\n(string) void\n", &mut b));
        let mut b = Vec::new();
        assert!(unify(b"(map[$0]$1) $1\n", b"(map[string]app.Box<int>) app.Box<int>\n", &mut b));
        assert_eq!(subst("($0, $1) $0", &["int".into(), "string".into()]), "(int, string) int");
    }
}
