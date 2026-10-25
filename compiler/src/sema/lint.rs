//! Inspections ("lints"): analyses over the typed HIR that find likely
//! mistakes in programs that compile (docs/TOOLING.md, "Инспекции").
//!
//! Every lint has a stable code `L00NN`, a default [`Severity`] and a short
//! doc ([`LINTS`]). A finding is an ordinary [`Diagnostic`] with that code
//! and, when the change is mechanical, a [`Fix`] that the language server
//! offers as a quick fix and `zell check --fix` applies.
//!
//! The pass runs after a clean analysis ([`check`]), over the functions of
//! the files the caller selects (the user's files, not std or other
//! units). Levels come from [`LintConfig`]: `zell check --lint=off|warn|error`
//! and `[lints] L0003 = "error" | "warn" | "info" | "hint" | "off"` in
//! `zell.toml`. A `// zell:allow L0003` comment (codes separated by commas
//! or spaces, or `all`) on the line of a finding or on the line above it
//! suppresses it.
//!
//! Two mistakes that are lints in other languages are compile errors in
//! Zell and are therefore not here: an unhandled `T!` result (`E0002`) and
//! an unused variable, including `for` keys and values (`E0001`).

use crate::diagnostics::{Diagnostic, Diagnostics, Fix, Severity, TextEdit};
use crate::hir::{self, Block, Callee, ConstValue, Expr, ExprKind, FuncId, FuncKind, Function, LocalId, LocalKind, Program, Stmt, StmtKind};
use crate::span::Span;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// A registered lint.
#[derive(Clone, Copy, Debug)]
pub struct Lint {
    pub code: &'static str,
    pub name: &'static str,
    pub severity: Severity,
    /// The finding marks code that can be removed (editors fade it out).
    pub unnecessary: bool,
    pub doc: &'static str,
}

pub const UNREACHABLE: &str = "L0001";
pub const SHADOWED_ERR: &str = "L0002";
pub const EMPTY_CATCH: &str = "L0003";
pub const SELF_ASSIGN: &str = "L0004";
pub const CONST_CONDITION: &str = "L0005";
pub const UNUSED_PARAM: &str = "L0006";
pub const UNUSED_FUNC: &str = "L0007";
pub const DEFER_IN_LOOP: &str = "L0008";
pub const DROP_CAPTURE: &str = "L0009";
pub const IMPORT_SHADOWED: &str = "L0010";

/// Every lint, by code.
pub const LINTS: &[Lint] = &[
    Lint { code: UNREACHABLE, name: "unreachable", severity: Severity::Warning, unnecessary: true, doc: "statements after return, break, continue or panic never run" },
    Lint { code: SHADOWED_ERR, name: "shadowed-err", severity: Severity::Warning, unnecessary: false, doc: "an inner `err` hides an outer one: the outer error can no longer be reported" },
    Lint { code: EMPTY_CATCH, name: "empty-catch", severity: Severity::Warning, unnecessary: false, doc: "`catch { }` with an empty body swallows the error" },
    Lint { code: SELF_ASSIGN, name: "self-assign", severity: Severity::Warning, unnecessary: true, doc: "`x = x` assigns a variable or field to itself" },
    Lint { code: CONST_CONDITION, name: "const-condition", severity: Severity::Warning, unnecessary: false, doc: "a comparison whose result is known at compile time" },
    Lint { code: UNUSED_PARAM, name: "unused-param", severity: Severity::Hint, unnecessary: true, doc: "a parameter of a private function that its body never reads" },
    Lint { code: UNUSED_FUNC, name: "unused-func", severity: Severity::Warning, unnecessary: true, doc: "a private function nothing calls or refers to" },
    Lint { code: DEFER_IN_LOOP, name: "defer-in-loop", severity: Severity::Info, unnecessary: false, doc: "`defer` in a loop runs at function exit, not per iteration, and makes every defer of the function use the heap" },
    Lint { code: IMPORT_SHADOWED, name: "import-shadowed", severity: Severity::Warning, unnecessary: false, doc: "a declaration of the own namespace hides a name an import also brings in: the short name means the local one, the import's needs its qualified name" },
    Lint { code: DROP_CAPTURE, name: "drop-capture", severity: Severity::Warning, unnecessary: false, doc: "a type with `Drop` stores a closure capturing `this` in its own field: the object keeps itself alive and `Drop` waits for the cycle collector" },
];

/// The lint with `code`.
pub fn lint(code: &str) -> Option<&'static Lint> {
    LINTS.iter().find(|l| l.code == code)
}

/// True for lint codes (`L....`).
pub fn is_lint(code: Option<&str>) -> bool {
    code.is_some_and(|c| c.starts_with('L'))
}

/// The level a lint reports at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Off,
    Is(Severity),
}

impl Level {
    /// `off`, `hint`, `info`, `warn`/`warning`, `error`.
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s {
            "off" | "allow" => Level::Off,
            "hint" => Level::Is(Severity::Hint),
            "info" => Level::Is(Severity::Info),
            "warn" | "warning" => Level::Is(Severity::Warning),
            "error" | "deny" => Level::Is(Severity::Error),
            _ => return None,
        })
    }
}

/// Lint levels: a global switch (`--lint`) and per-code overrides
/// (`[lints]` of the manifest).
#[derive(Clone, Debug, Default)]
pub struct LintConfig {
    /// `--lint=off` disables all lints; `--lint=error` makes every enabled
    /// lint an error; `--lint=warn` (the default) keeps the levels.
    pub global: Option<Level>,
    pub overrides: HashMap<String, Level>,
    /// `--fix`: apply the safe fixes of the findings.
    pub fix: bool,
}

thread_local! {
    static CONFIG: RefCell<LintConfig> = RefCell::new(LintConfig::default());
}

/// Sets the lint configuration of analyses run on this thread.
pub fn set_config(c: LintConfig) {
    CONFIG.with(|x| *x.borrow_mut() = c);
}

/// Changes the configuration of this thread in place.
pub fn update_config(f: impl FnOnce(&mut LintConfig)) {
    CONFIG.with(|x| f(&mut x.borrow_mut()));
}

/// The configuration of this thread.
pub fn config() -> LintConfig {
    CONFIG.with(|x| x.borrow().clone())
}

/// Parses a `[lints]` table: code or name -> level.
pub fn parse_overrides(table: &indexmap::IndexMap<String, String>) -> Result<HashMap<String, Level>, String> {
    let mut out = HashMap::new();
    for (k, v) in table {
        let l = LINTS.iter().find(|l| l.code == k || l.name == k).ok_or_else(|| format!("[lints]: unknown lint `{k}`"))?;
        let level = Level::parse(v).ok_or_else(|| format!("[lints] {k}: unknown level `{v}` (off, hint, info, warn, error)"))?;
        out.insert(l.code.to_string(), level);
    }
    Ok(out)
}

fn level_of(cfg: &LintConfig, l: &Lint) -> Level {
    if cfg.global == Some(Level::Off) {
        return Level::Off;
    }
    let lvl = cfg.overrides.get(l.code).copied().unwrap_or(Level::Is(l.severity));
    match (lvl, cfg.global) {
        (Level::Is(_), Some(Level::Is(Severity::Error))) => Level::Is(Severity::Error),
        _ => lvl,
    }
}

/// Runs every lint over the functions of `p` whose file `wanted` selects;
/// `texts[file]` is that file's source. Findings go to `diags` after the
/// configured levels and `// zell:allow` comments are applied.
pub fn check(p: &Program, texts: &[&str], wanted: &dyn Fn(usize) -> bool, diags: &mut Diagnostics) {
    let cfg = config();
    if cfg.global == Some(Level::Off) {
        return;
    }
    let mut out = Vec::new();
    let mut cx = Cx { p, texts, out: &mut out, refs: referenced_funcs(p), values: func_values(p) };
    for (i, f) in p.funcs.iter().enumerate() {
        if f.file >= texts.len() || !wanted(f.file) || f.span == Span::DUMMY || f.span.end as usize > texts[f.file].len() || crate::sema::stdlib::is_std_namespace(&f.full_name) {
            continue;
        }
        cx.function(FuncId(i as u32), f);
    }
    cx.drop_captures(wanted);
    out.extend(p.lint_notes.iter().filter(|d| d.file < texts.len() && wanted(d.file)).cloned());
    out.sort_by_key(|d| (d.file, d.span.start));
    let mut seen = HashSet::new();
    for mut d in out {
        let Some(l) = d.code.and_then(lint) else { continue };
        if !seen.insert((d.file, d.span, d.code)) {
            continue; // generic instances repeat their body
        }
        let Level::Is(sev) = level_of(&cfg, l) else { continue };
        if allowed(texts[d.file], d.span, l.code) {
            continue;
        }
        d.severity = if crate::sema::cycles::deny_warnings() && sev == Severity::Warning { Severity::Error } else { sev };
        let text = texts[d.file];
        for fix in &mut d.fixes {
            for e in &mut fix.edits {
                if e.new_text.is_empty() {
                    e.span = crate::session::whole_lines(text, e.span);
                }
            }
        }
        diags.push(d);
    }
}

/// `// zell:allow CODE` on the line of `span` or the line before.
fn allowed(text: &str, span: Span, code: &str) -> bool {
    let start = (span.start as usize).min(text.len());
    let ls = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let le = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let prev_s = if ls == 0 { 0 } else { text[..ls - 1].rfind('\n').map_or(0, |i| i + 1) };
    let lines = [&text[ls..le], if ls == 0 { "" } else { &text[prev_s..ls - 1] }];
    lines.iter().any(|line| {
        line.find("zell:allow").is_some_and(|i| {
            line[i + "zell:allow".len()..].split(|c: char| c == ',' || c.is_whitespace()).any(|w| w == code || w == "all" || lint(code).is_some_and(|l| l.name == w))
        })
    })
}

/// Every function some code calls or refers to (as a value).
fn referenced_funcs(p: &Program) -> HashSet<u32> {
    let mut out = HashSet::new();
    if let Some(e) = p.entry {
        out.insert(e.0);
    }
    for f in &p.funcs {
        hir::ownership::walk_function(p, f, &mut |e, _| match &e.kind {
            ExprKind::FuncRef(id) | ExprKind::Call { callee: Callee::Func(id), .. } | ExprKind::Closure { func: id, .. } => {
                out.insert(id.0);
            }
            ExprKind::EnvClosures { funcs, .. } => out.extend(funcs.iter().map(|f| f.0)),
            _ => {}
        });
    }
    // Enum methods are free functions reached through vtables: a method
    // that implements an interface the enum satisfies counts as used.
    for en in &p.enums {
        let has = |n: &str| en.methods.iter().any(|(m, _)| m == n);
        for it in &p.interfaces {
            if it.methods.iter().all(|m| has(&m.name) || m.default.is_some()) {
                for (_, f) in en.methods.iter().filter(|(m, _)| it.methods.iter().any(|im| im.name == *m)) {
                    out.insert(f.0);
                }
            }
        }
    }
    out
}

/// Functions used as values (their signature is fixed by the use).
fn func_values(p: &Program) -> HashSet<u32> {
    let mut out = HashSet::new();
    for f in &p.funcs {
        hir::ownership::walk_function(p, f, &mut |e, _| {
            if let ExprKind::FuncRef(id) = &e.kind {
                out.insert(id.0);
            }
        });
    }
    out
}

struct Cx<'a> {
    p: &'a Program,
    texts: &'a [&'a str],
    out: &'a mut Vec<Diagnostic>,
    refs: HashSet<u32>,
    values: HashSet<u32>,
}

/// True when `s` never completes normally.
fn terminates(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Return(_) | StmtKind::Break(_) | StmtKind::Continue(_) => true,
        StmtKind::Expr(e) => matches!(e.kind, ExprKind::Builtin { op: hir::Builtin::Panic, .. }),
        _ => false,
    }
}

impl Cx<'_> {
    fn text(&self, file: usize, s: Span) -> &str {
        self.texts[file].get(s.start as usize..s.end as usize).unwrap_or("")
    }

    fn push(&mut self, file: usize, d: Diagnostic) {
        self.out.push(d.in_file(file));
    }

    fn function(&mut self, id: FuncId, f: &Function) {
        let file = f.file;
        // Per-block lints: unreachable code, defer in loops, shadowed err.
        let mut scopes: Vec<Vec<LocalId>> = vec![f.params.clone()];
        self.block(f, &f.body, 0, &mut scopes);
        // Statement lints.
        let mut stmts: Vec<Stmt> = Vec::new();
        hir::ownership::walk_function_stmts(self.p, f, &mut |s| {
            if matches!(s.kind, StmtKind::Assign { .. } | StmtKind::If { .. } | StmtKind::Loop { .. }) {
                stmts.push(s.clone());
            }
        });
        for s in &stmts {
            match &s.kind {
                StmtKind::Assign { target, value } if same_place(target, value) => {
                    let t = self.text(file, target.span).to_string();
                    let d = Diagnostic::warning(format!("self-assignment of {t}"), s.span).with_code(SELF_ASSIGN).with_fix(Fix::edit("Remove the self-assignment", s.span, ""));
                    self.push(file, d);
                }
                StmtKind::If { cond, .. } | StmtKind::Loop { cond: Some(cond), .. } => self.const_cond(file, cond),
                _ => {}
            }
        }
        // Expression lints: empty catch, comparisons of a value with itself.
        let mut exprs: Vec<Expr> = Vec::new();
        hir::ownership::walk_function(self.p, f, &mut |e, _| {
            if matches!(e.kind, ExprKind::TryCatch { .. } | ExprKind::Binary { .. }) {
                exprs.push(e.clone());
            }
        });
        for e in &exprs {
            match &e.kind {
                ExprKind::TryCatch { value, handler, result_used: false, .. } if handler.stmts.is_empty() => self.empty_catch(f, e.span, value.span, handler.span),
                ExprKind::Binary { op, lhs, rhs } if is_cmp(*op) && same_place(lhs, rhs) && !self.p.types.is_float(lhs.ty) => {
                    let always = matches!(op, hir::BinOp::Eq | hir::BinOp::Le | hir::BinOp::Ge);
                    let d = Diagnostic::warning(format!("comparison of {} with itself is always {always}", self.text(file, lhs.span)), e.span)
                        .with_code(CONST_CONDITION)
                        .with_fix(Fix::edit(format!("Replace with {always}"), e.span, always.to_string()));
                    self.push(file, d);
                }
                _ => {}
            }
        }
        self.unused(id, f);
    }

    fn const_cond(&mut self, file: usize, cond: &Expr) {
        let ExprKind::Const(ConstValue::Bool(v)) = cond.kind else { return };
        let t = self.text(file, cond.span);
        // Only comparisons of literals: `if true`, `if debug` and
        // `os.Name == "linux"` (constant per target) are intended.
        let unquoted: String = t.split('"').step_by(2).collect();
        if !["==", "!=", "<", ">"].iter().any(|op| t.contains(op)) || unquoted.chars().any(|c| c.is_alphabetic() || c == '_') {
            return; // `if true`, `if debug`: intended
        }
        let d = Diagnostic::warning(format!("condition {t} is always {v}"), cond.span).with_code(CONST_CONDITION).with_fix(Fix::edit(format!("Replace with {v}"), cond.span, v.to_string()));
        self.push(file, d);
    }

    fn empty_catch(&mut self, f: &Function, whole: Span, value: Span, handler: Span) {
        let mut d = Diagnostic::warning("empty catch block swallows the error", handler).with_code(EMPTY_CATCH).with_help("handle the error, propagate it with `?`, or write `_ = ...` to discard it on purpose");
        let fallible = self.p.types.is_fallible(f.ret);
        let src = self.text(f.file, whole);
        if fallible && src.starts_with("try ") {
            d = d.with_fix(Fix::new(
                "Propagate the error with `?`",
                vec![TextEdit { span: Span::new(whole.start, value.start), new_text: String::new() }, TextEdit { span: Span::new(value.end, whole.end), new_text: "?".into() }],
            ));
        }
        if src.starts_with("try ") {
            d = d.with_fix(Fix::edit("Panic with the error", handler, "{ panic(err.Message()) }".to_string()).only_if(src.contains("catch err")));
        }
        d.fixes.retain(|x| !x.edits.is_empty());
        self.push(f.file, d);
    }

    fn block(&mut self, f: &Function, b: &Block, loops: usize, scopes: &mut Vec<Vec<LocalId>>) {
        scopes.push(Vec::new());
        let mut dead_from: Option<usize> = None;
        for (i, s) in b.stmts.iter().enumerate() {
            if dead_from.is_none() && i > 0 && terminates(&b.stmts[i - 1]) && s.span != Span::DUMMY && s.span.start >= b.stmts[i - 1].span.end {
                dead_from = Some(i);
            }
            self.stmt(f, s, loops, scopes);
        }
        if let Some(i) = dead_from {
            let last = b.stmts.iter().rev().find(|s| s.span != Span::DUMMY).map_or(b.stmts[i].span, |s| s.span);
            let span = Span::new(b.stmts[i].span.start, last.end.max(b.stmts[i].span.end));
            let d = Diagnostic::warning("unreachable code", span).with_code(UNREACHABLE).with_fix(Fix::edit("Remove unreachable code", span, ""));
            self.push(f.file, d);
        }
        scopes.pop();
    }

    fn declare(&mut self, f: &Function, l: LocalId, scopes: &mut [Vec<LocalId>]) {
        let local = &f.locals[l.0 as usize];
        if local.name == "err" && local.span != Span::DUMMY {
            let outer = scopes.iter().flatten().find(|o| f.locals[o.0 as usize].name == "err").copied();
            if let Some(o) = outer {
                let fix = self.rename_fix(f, l, "err2");
                let d = Diagnostic::warning("err shadows an outer err: the outer error can no longer be reported here", local.span).with_code(SHADOWED_ERR).with_note("outer err declared here", Some(f.locals[o.0 as usize].span));
                self.push(f.file, d.with_fix(fix));
            }
        }
        if let Some(s) = scopes.last_mut() {
            s.push(l);
        }
    }

    /// Renames local `l` (declaration and every use) to `to`.
    fn rename_fix(&self, f: &Function, l: LocalId, to: &str) -> Fix {
        let name = &f.locals[l.0 as usize].name;
        let mut spans = vec![f.locals[l.0 as usize].span];
        hir::ownership::walk_function(self.p, f, &mut |e, _| {
            if matches!(e.kind, ExprKind::Local(x) if x == l) && self.text(f.file, e.span) == name {
                spans.push(e.span);
            }
        });
        spans.sort_by_key(|s| s.start);
        spans.dedup();
        Fix::new(format!("Rename to {to}"), spans.into_iter().map(|span| TextEdit { span, new_text: to.into() }).collect())
    }

    fn expr_blocks(&mut self, f: &Function, e: &Expr, loops: usize, scopes: &mut Vec<Vec<LocalId>>) {
        let mut found: Vec<(Option<LocalId>, Block)> = Vec::new();
        hir::ownership::walk_expr(self.p, e, hir::ownership::Pos::Read, &mut |x, _| {
            if let ExprKind::TryCatch { err, handler, .. } = &x.kind {
                found.push((*err, handler.clone()));
            }
        });
        for (err, h) in found {
            scopes.push(Vec::new());
            if let Some(l) = err {
                self.declare(f, l, scopes);
            }
            self.block(f, &h, loops, scopes);
            scopes.pop();
        }
    }

    fn stmt(&mut self, f: &Function, s: &Stmt, loops: usize, scopes: &mut Vec<Vec<LocalId>>) {
        match &s.kind {
            StmtKind::Let { local, init } => {
                if let Some(e) = init {
                    self.expr_blocks(f, e, loops, scopes);
                }
                self.declare(f, *local, scopes);
            }
            StmtKind::Expr(e) | StmtKind::Discard(e) => self.expr_blocks(f, e, loops, scopes),
            StmtKind::Assign { value, .. } => self.expr_blocks(f, value, loops, scopes),
            StmtKind::If { then, else_, .. } => {
                self.block(f, then, loops, scopes);
                if let Some(e) = else_ {
                    self.block(f, e, loops, scopes);
                }
            }
            StmtKind::Loop { body, .. } => self.block(f, body, loops + 1, scopes),
            StmtKind::ForRange { var, body, .. } => self.loop_body(f, &[*var], body, loops, scopes),
            StmtKind::ForSlice { index, elem, body, .. } => self.loop_body(f, &[*index, *elem], body, loops, scopes),
            StmtKind::ForString { index, ch, body, .. } => self.loop_body(f, &[*index, *ch], body, loops, scopes),
            StmtKind::ForMap { key, value, body, .. } => self.loop_body(f, &[*key, *value], body, loops, scopes),
            StmtKind::Switch(sw) => {
                for c in &sw.cases {
                    self.block(f, &c.body, loops, scopes);
                }
                if let Some(d) = &sw.default {
                    self.block(f, d, loops, scopes);
                }
            }
            StmtKind::Block(b) => self.block(f, b, loops, scopes),
            StmtKind::TryBlock { body, err, handler } => {
                self.block(f, body, loops, scopes);
                scopes.push(Vec::new());
                if let Some(l) = err {
                    self.declare(f, *l, scopes);
                }
                if handler.stmts.is_empty() && handler.span != Span::DUMMY {
                    let d = Diagnostic::warning("empty catch block swallows the error", handler.span).with_code(EMPTY_CATCH).with_help("handle the error, or propagate it from the `try` block's statements with `?` outside a `try` block");
                    self.push(f.file, d);
                }
                self.block(f, handler, loops, scopes);
                scopes.pop();
            }
            StmtKind::Defer(_) if loops > 0 && s.span != Span::DUMMY => {
                let d = Diagnostic::warning("defer in a loop runs at function exit, not at the end of each iteration", s.span)
                    .with_code(DEFER_IN_LOOP)
                    .with_help("move the loop body into a function (its defers run per call), or call the cleanup directly; a defer in a loop also moves every defer of the function to the heap");
                self.push(f.file, d);
            }
            _ => {}
        }
    }

    fn loop_body(&mut self, f: &Function, vars: &[Option<LocalId>], body: &Block, loops: usize, scopes: &mut Vec<Vec<LocalId>>) {
        scopes.push(Vec::new());
        for l in vars.iter().flatten() {
            self.declare(f, *l, scopes);
        }
        self.block(f, body, loops + 1, scopes);
        scopes.pop();
    }

    /// Unused private functions and their unused parameters.
    fn unused(&mut self, id: FuncId, f: &Function) {
        let FuncKind::Free = f.kind else { return };
        // `zellUserMain__`: the `main` that `zell test` renames (testrun.rs).
        if f.exported || !f.attrs.is_empty() || f.name == "main" || f.name == "init" || f.name == "zellUserMain__" || f.name.contains(['<', '[', '$', '#']) {
            return;
        }
        if !self.refs.contains(&id.0) {
            let d = Diagnostic::warning(format!("function {} is never used", f.name), self.name_span(f)).with_code(UNUSED_FUNC).with_fix(Fix::edit(format!("Remove function {}", f.name), f.span, ""));
            self.push(f.file, d);
            return;
        }
        if self.values.contains(&id.0) {
            return;
        }
        let mut used = vec![false; f.locals.len()];
        hir::ownership::walk_function(self.p, f, &mut |e, _| match &e.kind {
            ExprKind::Local(l) => used[l.0 as usize] = true,
            ExprKind::Closure { captures, .. } => {
                for c in captures {
                    if let hir::CaptureSource::Local(l) = c {
                        used[l.0 as usize] = true;
                    }
                }
            }
            _ => {}
        });
        for &pid in &f.params {
            let local = &f.locals[pid.0 as usize];
            if local.kind != LocalKind::Param || used[pid.0 as usize] || local.name.starts_with('_') || local.span == Span::DUMMY {
                continue;
            }
            let name = Span::new(local.span.start, local.span.start + local.name.len() as u32);
            let d = Diagnostic::warning(format!("parameter {} is never used", local.name), name).with_code(UNUSED_PARAM).with_fix(Fix::edit(format!("Rename {} to _", local.name), name, "_"));
            self.push(f.file, d);
        }
    }

    /// The span of `f`'s name in its declaration.
    fn name_span(&self, f: &Function) -> Span {
        let t = self.text(f.file, f.span);
        match t.find(&format!("func {}", f.name)) {
            Some(i) => {
                let s = f.span.start + i as u32 + 5;
                Span::new(s, s + f.name.len() as u32)
            }
            None => f.span,
        }
    }

    /// Types with `Drop` whose methods store a closure capturing `this` in
    /// a field of `this`.
    fn drop_captures(&mut self, wanted: &dyn Fn(usize) -> bool) {
        for s in &self.p.structs {
            if s.file >= self.texts.len() || !wanted(s.file) || crate::sema::stdlib::is_std_namespace(&s.full_name) || !s.methods.iter().any(|(n, _)| n == "Drop") {
                continue;
            }
            for (_, mid) in &s.methods {
                let m = &self.p.funcs[mid.0 as usize];
                let Some(&this) = m.params.first().filter(|l| m.locals[l.0 as usize].kind == LocalKind::This) else { continue };
                let mut hits = Vec::new();
                hir::ownership::walk_function_stmts(self.p, m, &mut |st| {
                    if let StmtKind::Assign { target, value } = &st.kind {
                        let is_this = |e: &Expr| match &e.kind {
                            ExprKind::Local(l) => *l == this,
                            ExprKind::Deref(b) => matches!(b.kind, ExprKind::Local(l) if l == this),
                            _ => false,
                        };
                        let on_this = matches!(&target.kind, ExprKind::Field { base, .. } if is_this(base));
                        let captures_this = matches!(&value.kind, ExprKind::Closure { captures, .. } if captures.iter().any(|c| matches!(c, hir::CaptureSource::Local(l) if *l == this)));
                        if on_this && captures_this {
                            hits.push(st.span);
                        }
                    }
                });
                for span in hits {
                    let d = Diagnostic::warning(format!("{} has Drop but stores a closure capturing this in its own field: the object keeps itself alive", s.name), span)
                        .with_code(DROP_CAPTURE)
                        .with_help("capture a weak reference to this, or clear the field before the last reference goes away");
                    self.push(m.file, d);
                }
            }
        }
    }
}

trait OnlyIf {
    fn only_if(self, ok: bool) -> Self;
}

impl OnlyIf for Fix {
    fn only_if(mut self, ok: bool) -> Fix {
        if !ok {
            self.edits.clear();
        }
        self
    }
}

fn is_cmp(op: hir::BinOp) -> bool {
    use hir::BinOp::*;
    matches!(op, Eq | Ne | Lt | Le | Gt | Ge)
}

/// The same variable, or the same field path on the same variable.
fn same_place(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Local(x), ExprKind::Local(y)) => x == y,
        (ExprKind::Global(x), ExprKind::Global(y)) => x == y,
        (ExprKind::Field { base: b1, index: i1 }, ExprKind::Field { base: b2, index: i2 }) => i1 == i2 && same_place(b1, b2),
        _ => false,
    }
}

/// `--fix`: applies the first fix of every lint finding in `diags` to its
/// file on disk (overlapping fixes after the first are skipped) and
/// reports how many it applied.
pub fn apply_fixes_if_asked(diags: &Diagnostics, files: &[crate::span::SourceFile]) {
    if !config().fix {
        return;
    }
    let mut by_file: HashMap<usize, Vec<TextEdit>> = HashMap::new();
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for d in diags.items.iter().filter(|d| is_lint(d.code)) {
        let Some(fix) = d.fixes.first() else { continue };
        let edits = by_file.entry(d.file).or_default();
        if fix.edits.iter().any(|e| edits.iter().any(|x| e.span.start < x.span.end.max(x.span.start + 1) && x.span.start < e.span.end.max(e.span.start + 1))) {
            continue;
        }
        edits.extend(fix.edits.iter().cloned());
        *counts.entry(d.file).or_default() += 1;
    }
    for (file, mut edits) in by_file {
        let Some(f) = files.get(file) else { continue };
        edits.sort_by_key(|e| e.span.start);
        let text = crate::session::apply_edits(&f.text, &edits);
        match std::fs::write(&f.path, text) {
            Ok(()) => eprintln!("fixed {} lint finding(s) in {}", counts[&file], f.path),
            Err(e) => eprintln!("cannot write {}: {e}", f.path),
        }
    }
}
