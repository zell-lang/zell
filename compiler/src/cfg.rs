//! Conditional compilation (docs/PLATFORMS.md): `@cfg(...)` on
//! declarations and before `namespace`, and file-name suffixes.
//!
//! Runs between parsing and semantic analysis: declarations whose `@cfg`
//! is false are removed from the AST (so they are not type-checked and
//! their names do not exist), and a file excluded by its `@cfg` or by its
//! name suffix (`_windows.zl`, `_arm64.zl`, see
//! [`Target::excludes_file`]) keeps only its namespace line. The `@cfg`
//! attributes of kept declarations are dropped as well: they have no
//! meaning after this pass.
//!
//! Predicates: `os: "linux"`, `arch: "amd64"`, `feature: "tls"` (features
//! of the package, from its manifest), `not(p)`, `any(p, ...)`,
//! `all(p, ...)`; several arguments of one `@cfg` must all hold.

use crate::ast::{self, Attribute, Decl, Expr, ExprKind};
use crate::diagnostics::{Diagnostic, Diagnostics};
use crate::span::Span;
use crate::target::{Target, KNOWN_ARCH, KNOWN_OS};
use std::collections::HashSet;

/// What `@cfg` predicates are evaluated against.
pub struct CfgEnv<'a> {
    pub target: Target,
    /// Enabled features per file (by file index); `None` = no features.
    pub features: Option<&'a dyn Fn(usize) -> HashSet<String>>,
}

impl CfgEnv<'_> {
    /// The current target (see [`crate::target::current`]), no features.
    pub fn current() -> CfgEnv<'static> {
        CfgEnv { target: crate::target::current(), features: None }
    }
}

/// Applies conditional compilation to parsed `files` (whose source paths
/// are `paths`, same order). Errors in predicates are reported with the
/// file index.
pub fn apply(files: &mut [ast::File], paths: &[&str], env: &CfgEnv, diags: &mut Diagnostics) {
    for (i, f) in files.iter_mut().enumerate() {
        let features = env.features.map(|g| g(i)).unwrap_or_default();
        let mut ev = Eval { target: &env.target, features: &features, diags, file: i };
        let by_suffix = paths.get(i).is_some_and(|p| env.target.excludes_file(p));
        let keep = ev.keep(&mut f.attrs);
        if by_suffix || !keep {
            f.imports.clear();
            f.decls.clear();
            continue;
        }
        ev.filter_decls(&mut f.decls);
    }
}

struct Eval<'e> {
    target: &'e Target,
    features: &'e HashSet<String>,
    diags: &'e mut Diagnostics,
    file: usize,
}

impl Eval<'_> {
    fn err(&mut self, msg: String, span: Span) {
        self.diags.push(Diagnostic::error(msg, span).in_file(self.file));
    }

    /// Evaluates and removes the `@cfg` attributes of `attrs`; true if the
    /// item is kept.
    fn keep(&mut self, attrs: &mut Vec<Attribute>) -> bool {
        let mut keep = true;
        attrs.retain(|a| {
            if a.path.to_string() != "cfg" {
                return true;
            }
            keep &= self.attr(a);
            false
        });
        keep
    }

    fn attr(&mut self, a: &Attribute) -> bool {
        if a.args.is_empty() {
            self.err("@cfg needs a condition, e.g. @cfg(os: \"linux\")".into(), a.span);
            return true;
        }
        let mut all = true;
        for arg in &a.args {
            let v = match &arg.name {
                Some(n) => self.key(&n.name, &arg.value, n.span),
                None => self.pred(&arg.value),
            };
            all &= v;
        }
        all
    }

    /// `not(...)`, `any(...)`, `all(...)` or `key: "value"`.
    fn pred(&mut self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::NamedArg { name, value } => self.key(&name.name, value, name.span),
            ExprKind::Paren(x) => self.pred(x),
            ExprKind::Call { callee, args } => {
                let op = match &callee.kind {
                    ExprKind::Ident(id) => id.name.as_str(),
                    _ => "",
                };
                match op {
                    "not" => {
                        if args.len() != 1 {
                            self.err(format!("not(...) in @cfg takes one condition, found {}", args.len()), e.span);
                            return false;
                        }
                        !self.pred(&args[0])
                    }
                    "any" => {
                        let vs: Vec<bool> = args.iter().map(|a| self.pred(a)).collect();
                        vs.into_iter().any(|v| v)
                    }
                    "all" => {
                        let vs: Vec<bool> = args.iter().map(|a| self.pred(a)).collect();
                        vs.into_iter().all(|v| v)
                    }
                    _ => {
                        self.err("unknown @cfg operator: expected not(...), any(...) or all(...)".into(), callee.span);
                        false
                    }
                }
            }
            _ => {
                self.err("invalid @cfg condition: expected os: \"...\", arch: \"...\", feature: \"...\", not(...), any(...) or all(...)".into(), e.span);
                false
            }
        }
    }

    fn key(&mut self, key: &str, value: &Expr, span: Span) -> bool {
        let ExprKind::Str(v) = &value.kind else {
            self.err(format!("@cfg {key}: needs a string literal"), value.span);
            return false;
        };
        match key {
            "os" => {
                if !KNOWN_OS.contains(&v.as_str()) {
                    self.err(format!("unknown operating system \"{v}\" in @cfg (known: {})", KNOWN_OS.join(", ")), value.span);
                }
                *v == self.target.os
            }
            "arch" => {
                if !KNOWN_ARCH.contains(&v.as_str()) {
                    self.err(format!("unknown architecture \"{v}\" in @cfg (known: {})", KNOWN_ARCH.join(", ")), value.span);
                }
                *v == self.target.arch
            }
            "feature" => self.features.contains(v.as_str()),
            _ => {
                self.err(format!("unknown @cfg key {key} (expected os, arch or feature)"), span);
                false
            }
        }
    }

    fn filter_decls(&mut self, decls: &mut Vec<Decl>) {
        decls.retain_mut(|d| {
            let keep = match d {
                Decl::Struct(s) => self.keep(&mut s.attrs),
                Decl::Interface(it) => self.keep(&mut it.attrs),
                Decl::Enum(e) => self.keep(&mut e.attrs),
                Decl::Func(f) => self.keep(&mut f.attrs),
                Decl::Var(v) => self.keep(&mut v.attrs),
                Decl::Const(c) => self.keep(&mut c.attrs),
                Decl::TypeAlias(a) => self.keep(&mut a.attrs),
                Decl::Error(_) => true,
            };
            if keep {
                match d {
                    Decl::Struct(s) => {
                        s.fields.retain_mut(|f| self.keep(&mut f.attrs));
                        s.methods.retain_mut(|m| self.keep(&mut m.attrs));
                    }
                    Decl::Interface(it) => it.methods.retain_mut(|m| self.keep(&mut m.attrs)),
                    Decl::Enum(e) => {
                        e.variants.retain_mut(|v| self.keep(&mut v.attrs));
                        e.methods.retain_mut(|m| self.keep(&mut m.attrs));
                    }
                    _ => {}
                }
            }
            keep
        });
    }
}
