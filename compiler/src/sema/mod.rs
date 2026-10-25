//! Semantic analysis: name resolution, type checking and lowering to HIR.
//!
//! Entry point: [`analyze`]. The phases are
//!
//! 1. [`resolve`]: build the namespace tree (user files merged by namespace
//!    path, plus the stdlib from [`stdlib`]), register declarations,
//!    resolve imports, then resolve declaration signatures (struct fields,
//!    enum payloads, interface methods, function signatures).
//! 2. [`attrs`]: evaluate and check attributes.
//! 3. Globals and constants are typed lazily (on first use or in order).
//! 4. Function bodies are checked and lowered to HIR in one pass
//!    ([`expr`], [`call`], [`stmt`]); lambdas are lifted to functions.
//!
//! Generic declarations ([`generics`]) are checked once against their
//! constraints and monomorphised: every distinct instantiation becomes an
//! ordinary HIR function, struct or enum. `collections` is a set of
//! generic structs written in Zell ([`collections`]).
//!
//! Reflection ([`reflect`]): `reflect`, `encoding.json` and
//! `strings.Builder` are Zell sources loaded on first use; `typeof`,
//! compile-time `reflect.Fields<T>()` loops and static `if`s on type kinds
//! are handled there, and [`hir::Program::reflect`] tells code generation
//! which type tables and intrinsics to emit.
//!
//! Untyped numeric constants are folded by [`consts`] and materialised with
//! the type the context expects, as in Go.

mod arena;
mod attrs;
mod call;
mod coerce;
mod collections;
mod consts;
pub mod cycles;
// >>> debugger
pub mod debug_agent;
// <<< debugger
mod expr;
mod generics;
mod ifaces;
mod opover;
pub mod index;
pub mod lint;
mod infer;
mod iterate;
mod litcheck;
mod logtmpl;
mod ownership;
mod reflect;
mod reflect_ct;
mod reflect_unroll;
mod reflect_with;
mod resolve;
mod stmt;
pub mod stdfs;
mod stdsrc;
mod suggest;
pub mod stdlib;
pub mod units;
// >>> data (docs/FRAMEWORK.md section 3)
mod orm;
mod orm_model;
mod orm_sql;
// <<< data
pub mod stdlib_docs;

use crate::ast;
use crate::diagnostics::{codes, Diagnostic, Diagnostics, Fix};
use crate::hir::{self, FuncId, GlobalId, Label, LocalId, LocalKind};
use crate::span::Span;
use crate::types::{Type, TypeId, TypeTable};
use std::collections::HashMap;

pub use consts::Folded;
pub use resolve::{ImportPolicy, PackageScope};

/// Analyses a whole program made of `files` (each with its namespace).
/// Diagnostics carry the index of their file in `files`
/// ([`Diagnostic::file`]). Returns `None` if any error was reported.
pub fn analyze(files: &[ast::File], diags: &mut Diagnostics) -> Option<hir::Program> {
    analyze_with_policy(files, diags, None)
}

/// Analyses `files` like [`analyze`] and returns the `data` model of
/// every context as JSON (the migrations snapshot, see `crate::datacli`),
/// or `None` if any error was reported.
pub fn analyze_data_model(files: &[ast::File], diags: &mut Diagnostics) -> Option<String> {
    let files: Vec<&ast::File> = files.iter().collect();
    let mut s = Sema::new(&files, diags);
    s.run();
    if s.diags.has_errors() {
        return None;
    }
    Some(s.orm_model_json())
}

/// [`analyze`] with an optional [`ImportPolicy`] restricting which root
/// namespaces each file may reach (strict package isolation, used by
/// package builds; see `crate::package`).
pub fn analyze_with_policy(files: &[ast::File], diags: &mut Diagnostics, policy: Option<&ImportPolicy>) -> Option<hir::Program> {
    let files: Vec<&ast::File> = files.iter().collect();
    let mut s = Sema::new(&files, diags);
    s.policy = policy;
    s.run();
    let mut had_errors = s.diags.has_errors();
    let program = s.finish();
    if !had_errors {
        ownership::check(&program, diags);
        had_errors = diags.has_errors();
    }
    if !had_errors {
        cycles::check(&program, diags);
    }
    if cycles::deny_warnings() {
        for d in diags.items.iter_mut().filter(|d| d.severity == crate::diagnostics::Severity::Warning) {
            d.severity = crate::diagnostics::Severity::Error;
            d.notes.push(("help: warnings are errors (--deny-warnings)".into(), None));
        }
        had_errors = diags.has_errors();
    }
    if had_errors {
        None
    } else {
        Some(program)
    }
}

/// Index into `Sema::nss` (user and stdlib namespaces).
pub(crate) type NsIdx = usize;

/// A name declared in a namespace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Member {
    Struct(u32),
    Interface(u32),
    Enum(u32),
    Func(FuncId),
    Global(GlobalId),
    /// Index into [`stdlib::FUNCS`].
    StdFunc(usize),
    Special(stdlib::Special),
    /// Index into [`stdlib::CONSTS`].
    StdConst(usize),
    /// A child namespace.
    Ns(NsIdx),
    /// A generic function, struct or enum (index into `GenState::templates`).
    Generic(u32),
    /// `type Name = T` (index into [`Sema::aliases`]); lookups give the
    /// aliased struct, enum or interface itself once aliases are resolved.
    Alias(u32),
}

/// A type alias `type Name = T` (DESIGN.md "Псевдонимы типов").
pub(crate) struct Alias<'a> {
    pub decl: &'a ast::TypeAliasDecl,
    pub file: usize,
    pub ns: NsIdx,
    /// The aliased type once resolved.
    pub ty: Option<TypeId>,
    /// Being resolved (a cycle reaches it again).
    pub resolving: bool,
}

/// A namespace during analysis.
pub(crate) struct Ns {
    pub path: String,
    pub members: HashMap<String, (Member, Span, usize)>,
    pub is_std: bool,
    pub hir_id: Option<hir::NamespaceId>,
    pub attrs: Vec<hir::Attr>,
}

/// Builtin functions in scope without imports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuiltinFn {
    Error,
    Len,
    Append,
    Delete,
    Panic,
    Typeof,
}

/// An entry of the file-level import scope.
#[derive(Clone, Debug)]
pub(crate) enum ImportEntry {
    /// `import a.b`: members of the namespace are visible by short name.
    Glob(NsIdx),
    /// `import a.b as x` or `import a.b.Type [as x]`.
    Named(String, Member, NsIdx),
}

/// Per-file context.
pub(crate) struct FileScope {
    pub ns: NsIdx,
    pub imports: Vec<ImportEntry>,
    /// Per entry of `imports`: the `import` line's path span and whether a
    /// lookup went through it (unused imports are errors).
    pub import_uses: Vec<(Span, bool)>,
}

/// Signature of a user function (for calls), receiver excluded.
#[derive(Clone, Debug)]
pub(crate) struct Sig {
    pub params: Vec<TypeId>,
    pub ret: TypeId,
    pub is_async: bool,
}

/// Where a declaration's source lives, for later body checking.
#[derive(Clone, Copy)]
pub(crate) enum FuncSrc<'a> {
    Free(&'a ast::FuncDecl),
    Method(&'a ast::FuncDecl, u32),
    /// Lambdas and synthesized thunks (checked when encountered).
    Synthetic,
}

/// Lazy typing state of a global.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GState {
    Pending,
    InProgress,
    Done,
}

/// Source of a global var/const.
#[derive(Clone, Copy)]
pub(crate) enum GlobalSrc<'a> {
    Var(&'a ast::VarDecl),
    Const(&'a ast::ConstDecl),
}

/// Entries of a local scope.
#[derive(Clone, Debug)]
pub(crate) enum ScopeEntry {
    Local(LocalId),
    Const(Folded),
}

/// Kind of a break target.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LabelKind {
    Loop,
    Switch,
}

/// State of one function body being checked (a stack of these handles
/// nested lambdas).
pub(crate) struct FnCtx {
    pub func: Option<FuncId>,
    pub locals: Vec<hir::Local>,
    pub used: Vec<bool>,
    pub scopes: Vec<Vec<(String, ScopeEntry)>>,
    /// Declared result type (`T`, `T!`, `void`); `None` while inferring an
    /// arrow lambda's result.
    pub ret: TypeId,
    pub is_async: bool,
    /// Name for diagnostics.
    pub name: String,
    pub captures: Vec<hir::Capture>,
    /// (depth of the owning FnCtx, local there) for each capture.
    pub capture_keys: Vec<(usize, LocalId)>,
    pub labels: Vec<(Label, LabelKind)>,
    pub next_label: u32,
    /// Open user labels (`outer: for ...`): the label, its name and span,
    /// and whether a `break`/`continue` used it.
    pub named_labels: Vec<(Label, String, Span, bool)>,
    /// The label written before the statement being lowered; taken by the
    /// next [`Sema::push_label`] (the `for`/`switch` it names).
    pub pending_label: Option<(String, Span)>,
    /// Every label declared in the body so far.
    pub declared_labels: Vec<String>,
    /// `break L` / `continue L` naming no enclosing label: reported at the
    /// end of the body ("not defined" or "invalid").
    pub bad_jumps: Vec<(String, Span, &'static str)>,
    /// Number of enclosing block `try` bodies (a `?` inside one jumps to
    /// its `catch`, so it is allowed in any function).
    pub try_depth: u32,
    /// The body has syntax errors (recovered by the parser): whole-body
    /// checks (unused variables, missing return) are skipped.
    pub recovering: bool,
    /// The function's `defer` statements are open-coded
    /// ([`hir::StmtKind::Defer`]): none is inside a loop and there are at
    /// most [`hir::MAX_OPEN_DEFERS`]. Set by [`Sema::fn_body`].
    pub open_defers: bool,
    /// Diagnostics before the body (index into `diags.items`) and the
    /// declaration's span: an error after the start or inside the span (a
    /// bad parameter type) makes follow-up `<invalid>` messages noise.
    pub diag_start: usize,
    pub decl_span: Option<Span>,
}

impl FnCtx {
    fn new(func: Option<FuncId>, ret: TypeId, is_async: bool, name: String) -> FnCtx {
        FnCtx {
            func,
            locals: Vec::new(),
            used: Vec::new(),
            scopes: vec![Vec::new()],
            ret,
            is_async,
            name,
            captures: Vec::new(),
            capture_keys: Vec::new(),
            labels: Vec::new(),
            next_label: 0,
            named_labels: Vec::new(),
            pending_label: None,
            declared_labels: Vec::new(),
            bad_jumps: Vec::new(),
            try_depth: 0,
            recovering: false,
            open_defers: false,
            diag_start: 0,
            decl_span: None,
        }
    }

    fn lookup(&self, name: &str) -> Option<&ScopeEntry> {
        self.scopes.iter().rev().find_map(|s| s.iter().rev().find(|(n, _)| n == name).map(|(_, e)| e))
    }
}

/// How a value reached by name is accessed from the current function.
#[derive(Clone, Copy, Debug)]
pub(crate) enum VarRef {
    Local(LocalId),
    Upvalue(u32),
}

/// The analyser state.
pub(crate) struct Sema<'a> {
    pub files: &'a [&'a ast::File],
    /// Type aliases of the user files.
    pub aliases: Vec<Alias<'a>>,
    /// Set by [`Sema::resolve_aliases`]: lookups may resolve aliases.
    pub aliases_ready: bool,
    pub diags: &'a mut Diagnostics,
    pub tt: TypeTable,
    pub nss: Vec<Ns>,
    pub ns_index: HashMap<String, NsIdx>,
    /// Root namespaces (the stdlib roots `io`, `net`, ..., and `app`, ...).
    pub roots: HashMap<String, NsIdx>,
    pub file_scopes: Vec<FileScope>,
    pub structs: Vec<hir::Struct>,
    pub struct_src: Vec<&'a ast::StructDecl>,
    pub interfaces: Vec<hir::Interface>,
    pub iface_src: Vec<&'a ast::InterfaceDecl>,
    pub enums: Vec<hir::Enum>,
    pub enum_src: Vec<&'a ast::EnumDecl>,
    pub funcs: Vec<hir::Function>,
    pub func_src: Vec<FuncSrc<'a>>,
    pub sigs: Vec<Sig>,
    pub globals: Vec<hir::Global>,
    pub global_src: Vec<GlobalSrc<'a>>,
    pub global_state: Vec<GState>,
    /// Folded values of constant globals.
    pub global_folded: HashMap<u32, Folded>,
    /// Globals in the order their typing completed (dependencies first).
    pub global_order: Vec<GlobalId>,
    /// Globals whose initialization cycle was already reported.
    pub cycle_reported: std::collections::HashSet<u32>,
    pub externs: Vec<hir::Extern>,
    pub extern_map: HashMap<String, hir::ExternId>,
    /// Current file and namespace.
    pub cur_file: usize,
    /// Lint findings of name resolution (`hir::Program::lint_notes`).
    pub lint_notes: Vec<crate::diagnostics::Diagnostic>,
    pub cur_ns: NsIdx,
    /// Function contexts, innermost last.
    pub fns: Vec<FnCtx>,
    pub lambda_count: u32,
    /// Methods of enums by (enum id, name). They are ordinary functions
    /// whose first parameter is `this` (the enum value).
    pub enum_methods: HashMap<(u32, String), FuncId>,
    /// The enum owning each enum method (by function id).
    pub enum_method_owner: HashMap<u32, u32>,
    /// Default methods of interfaces by (interface id, name): free
    /// functions whose first parameter `this` is the interface value.
    pub iface_defaults: HashMap<(u32, String), FuncId>,
    /// The interface owning each default method (by function id).
    pub iface_default_owner: HashMap<u32, u32>,
    /// Default methods of stdlib-source interfaces: function id -> the
    /// stdlib namespace whose names their signature and body use.
    pub std_fn_ns: HashMap<u32, NsIdx>,
    /// Stdlib-source interfaces whose method signatures are not resolved
    /// yet (interface id -> its namespace): [`Sema::force_iface`] resolves
    /// them on the first use of the interface type, so a program does not
    /// pay for `net.http.Handler` (whose signatures pull in the whole
    /// stdlib) unless it names it.
    pub std_iface_lazy: HashMap<u32, NsIdx>,
    /// [`Sema::check_attributes`] ran: later struct instances evaluate
    /// their attributes themselves.
    pub attrs_done: bool,
    /// Package isolation hook (see [`ImportPolicy`]); `None` = no limits.
    pub policy: Option<&'a ImportPolicy>,
    /// IDE symbol recording (see [`index`]); `None` = off.
    pub(crate) ide: Option<Box<index::Recorder>>,
    /// Set by a tool (the language server) when the result is no longer
    /// wanted: [`Sema::run`] stops between phases and function bodies.
    pub(crate) cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// Generics state (templates, instantiations, type environment).
    pub(crate) gen: generics::GenState<'a>,
    /// Reflection state (lazily loaded library sources, compile-time fields).
    pub(crate) refl: reflect::ReflState,
    /// data compile-time expansion (models, query translation).
    pub(crate) orm: orm_model::OrmState,
    /// Unit mode for incremental IDE analysis (see [`units`]); `None` =
    /// an ordinary analysis.
    pub(crate) unit: Option<Box<units::UnitState>>,
}

impl<'a> Sema<'a> {
    fn new(files: &'a [&'a ast::File], diags: &'a mut Diagnostics) -> Sema<'a> {
        Sema {
            files,
            diags,
            tt: TypeTable::new(),
            nss: Vec::new(),
            ns_index: HashMap::new(),
            roots: HashMap::new(),
            file_scopes: Vec::new(),
            structs: Vec::new(),
            struct_src: Vec::new(),
            interfaces: Vec::new(),
            iface_src: Vec::new(),
            enums: Vec::new(),
            enum_src: Vec::new(),
            funcs: Vec::new(),
            func_src: Vec::new(),
            sigs: Vec::new(),
            globals: Vec::new(),
            aliases: Vec::new(),
            aliases_ready: false,
            global_src: Vec::new(),
            global_state: Vec::new(),
            global_folded: HashMap::new(),
            cycle_reported: Default::default(),
            global_order: Vec::new(),
            externs: Vec::new(),
            extern_map: HashMap::new(),
            cur_file: 0,
            lint_notes: Vec::new(),
            cur_ns: 0,
            fns: Vec::new(),
            lambda_count: 0,
            enum_methods: HashMap::new(),
            enum_method_owner: HashMap::new(),
            iface_defaults: HashMap::new(),
            iface_default_owner: HashMap::new(),
            std_fn_ns: HashMap::new(),
            std_iface_lazy: HashMap::new(),
            attrs_done: false,
            policy: None,
            ide: None,
            cancel: None,
            gen: generics::GenState::default(),
            refl: reflect::ReflState::default(),
            orm: orm_model::OrmState::default(),
            unit: None,
        }
    }

    /// The analysis was cancelled through [`Sema::cancel`].
    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn run(&mut self) {
        let phases: [fn(&mut Self); 12] = [
            Self::build_stdlib,
            Self::collect_decls,
            Self::add_std_structs,
            Self::register_collections,
            Self::register_std_sources,
            Self::resolve_imports,
            Self::resolve_aliases,
            Self::resolve_template_bounds,
            Self::resolve_signatures,
            Self::check_implements,
            Self::run_deferred_bounds,
            Self::check_attributes,
        ];
        for phase in phases {
            if self.cancelled() {
                return;
            }
            phase(self);
        }
        for g in 0..self.globals.len() {
            self.ensure_global(GlobalId(g as u32));
        }
        self.check_templates();
        self.instantiate_debug_agent();
        if let Some(u) = self.unit.as_mut() {
            u.in_bodies = true;
        }
        // Instantiations append functions while bodies are checked; they
        // are checked in turn.
        let mut f = 0;
        while f < self.func_src.len() {
            if self.cancelled() {
                return;
            }
            self.check_function_body(FuncId(f as u32));
            f += 1;
        }
        if let Some(u) = self.unit.as_mut() {
            u.in_bodies = false;
        }
        self.check_unused_imports();
        if self.unit.is_some() {
            // The entry point is chosen over the whole program (see units).
            return;
        }
        if let Some(main) = self.find_entry() {
            let sig = &self.sigs[main.0 as usize];
            let void_ok = matches!(self.tt.get(sig.ret), Type::Void) || matches!(self.tt.fallible_inner(sig.ret).map(|t| self.tt.get(t)), Some(Type::Void));
            if !sig.params.is_empty() || !void_ok {
                let f = &self.funcs[main.0 as usize];
                self.cur_file = f.file;
                let span = f.span;
                self.err("main must take no parameters and return nothing (or void!)", span);
            }
        }
    }

    fn finish(mut self) -> hir::Program {
        let reflect = self.reflect_finish();
        // Library namespaces loaded on demand get their ids late.
        let mut nss: Vec<(hir::NamespaceId, &Ns)> = self.nss.iter().filter_map(|n| n.hir_id.map(|h| (h, n))).collect();
        nss.sort_by_key(|(h, _)| *h);
        let namespaces = nss.into_iter().map(|(_, ns)| hir::Namespace { path: ns.path.clone(), attrs: ns.attrs.clone() }).collect();
        let entry = self.find_entry();
        hir::Program {
            types: self.tt,
            namespaces,
            structs: self.structs,
            interfaces: self.interfaces,
            enums: self.enums,
            funcs: self.funcs,
            globals: self.globals,
            global_init_order: self.global_order,
            externs: self.externs,
            entry,
            sources: Vec::new(),
            reflect,
            lint_notes: self.lint_notes,
        }
    }

    /// `main` of the user namespace with the shortest path declaring one.
    fn find_entry(&self) -> Option<FuncId> {
        let mut best: Option<(usize, FuncId)> = None;
        for ns in self.nss.iter().filter(|n| !n.is_std) {
            if let Some((Member::Func(f), _, _)) = ns.members.get("main") {
                let depth = ns.path.split('.').count();
                if best.is_none_or(|(d, _)| depth < d) {
                    best = Some((depth, *f));
                }
            }
        }
        best.map(|(_, f)| f)
    }

    // -- diagnostics --------------------------------------------------------

    /// Reports an error in the current file (with the generic
    /// instantiation context, if any).
    pub fn err(&mut self, msg: impl Into<String>, span: Span) {
        let d = Diagnostic::error(msg, span).in_file(self.cur_file);
        self.push_diag(d);
    }

    /// Reports an error with a note.
    pub fn err_note(&mut self, msg: impl Into<String>, span: Span, note: impl Into<String>, note_span: Option<Span>) {
        let d = Diagnostic::error(msg, span).with_note(note, note_span).in_file(self.cur_file);
        self.push_diag(d);
    }

    /// Reports an error with an optional `help:` line.
    pub fn err_maybe_help(&mut self, msg: impl Into<String>, span: Span, help: Option<String>) {
        let mut d = Diagnostic::error(msg, span).in_file(self.cur_file);
        if let Some(h) = help {
            d = d.with_help(h);
        }
        self.push_diag(d);
    }

    /// "unknown type X" with a "did you mean" help for a one-segment name.
    pub fn unknown_type(&mut self, path: &ast::Path) {
        let help = match path.segments.as_slice() {
            [one] => {
                let names = self.visible_names();
                self.did_you_mean(&one.name, &names)
            }
            _ => None,
        };
        self.err_maybe_help(format!("unknown type {}", path.to_string()), path.span, help);
    }

    /// Reports an error with a `help:` line suggesting the fix.
    pub fn err_help(&mut self, msg: impl Into<String>, span: Span, help: impl Into<String>) {
        let d = Diagnostic::error(msg, span).with_help(help).in_file(self.cur_file);
        self.push_diag(d);
    }

    /// Reports an error with a stable code and quick fixes
    /// (docs/TOOLING.md, structured diagnostics).
    pub fn err_fix(&mut self, msg: impl Into<String>, span: Span, code: &'static str, fixes: Vec<Fix>) {
        let mut d = Diagnostic::error(msg, span).in_file(self.cur_file).with_code(code);
        d.fixes = fixes;
        self.push_diag(d);
    }

    /// "unknown identifier X", with "import ns" fixes when `X` is a
    /// stdlib member (or a std namespace) that is not imported.
    pub fn unknown_ident(&mut self, id: &ast::Ident) {
        let mut paths: Vec<String> = Vec::new();
        for ns in self.nss.iter().filter(|n| n.is_std) {
            if ns.members.get(&id.name).is_some_and(|m| !matches!(m.0, Member::Ns(_))) {
                paths.push(ns.path.clone());
            }
        }
        paths.sort();
        let mut fixes = Vec::new();
        if let Some(file) = self.files.get(self.cur_file) {
            let at = file.imports.last().map(|i| i.span.end).unwrap_or(file.namespace.span.end);
            let sep = if file.imports.is_empty() { "\n\n" } else { "\n" };
            for p in paths.iter().filter(|p| !file.imports.iter().any(|i| i.path.to_string() == **p && i.alias.is_none())).take(3) {
                fixes.push(Fix::edit(format!("Import {p}"), Span::new(at, at), format!("{sep}import {p}")));
            }
        }
        let help = match paths.first() {
            Some(p) => Some(format!("{} is in {p}: add `import {p}`", id.name)),
            None => {
                let names = self.visible_names();
                self.did_you_mean(&id.name, &names)
            }
        };
        let mut d = Diagnostic::error(format!("unknown identifier {}", id.name), id.span).in_file(self.cur_file).with_code(codes::UNKNOWN_IDENTIFIER);
        if let Some(h) = help {
            d = d.with_help(h);
        }
        d.fixes = fixes;
        self.push_diag(d);
    }

    /// "result of type T! must be handled" with quick fixes: append `?`
    /// (only where `?` is allowed) or wrap in `try ... catch`.
    pub fn unhandled_result(&mut self, ty: TypeId, span: Span) {
        let can_propagate = self.fns.last().is_some_and(|f| f.try_depth > 0 || self.tt.is_fallible(f.ret));
        let mut fixes = Vec::new();
        if can_propagate {
            fixes.push(Fix::edit("Propagate the error with `?`", Span::new(span.end, span.end), "?"));
        }
        fixes.push(Fix::new(
            "Handle the error with `try ... catch`",
            vec![
                crate::diagnostics::TextEdit { span: Span::new(span.start, span.start), new_text: "try ".into() },
                crate::diagnostics::TextEdit { span: Span::new(span.end, span.end), new_text: " catch err { panic(err.Message()) }".into() },
            ],
        ));
        let msg = format!("result of type {} must be handled (use ?, try ... catch, try ... else, or `_ =`)", self.tn(ty));
        let help = if can_propagate {
            "add `?` to pass the error to the caller, or handle it here with `try ... catch err { ... }` / `try ... else <fallback>`"
        } else {
            "handle it with `try ... catch err { ... }` or `try ... else <fallback>` (`?` needs a function returning T!)"
        };
        let mut d = Diagnostic::error(msg, span).in_file(self.cur_file).with_code(codes::UNHANDLED_RESULT).with_help(help);
        d.fixes = fixes;
        self.push_diag(d);
    }

    fn push_diag(&mut self, mut d: Diagnostic) {
        if self.gen.mute > 0 {
            return;
        }
        // A type that is `<invalid>` comes from an error already reported
        // in this function: its follow-ups only repeat it.
        if d.severity == crate::diagnostics::Severity::Error && d.message.contains("<invalid>") && self.function_has_error() {
            return;
        }
        // The copies of an unrolled descriptor loop repeat a diagnostic of
        // code that does not depend on the descriptor: report it once.
        if !self.refl.ct_descs.is_empty() && self.diags.items.iter().rev().take(64).any(|x| x.message == d.message && x.span == d.span && x.file == d.file) {
            return;
        }
        self.unit_note_diag();
        if let Some(n) = &self.gen.note {
            d = d.with_note(n.clone(), None);
        }
        self.diags.push(d);
    }

    /// An error was already reported in the function being checked (its
    /// body so far, or its declaration: parameter and result types).
    fn function_has_error(&self) -> bool {
        let Some(f) = self.fns.first() else { return false };
        let file = self.cur_file;
        self.diags.items.iter().enumerate().any(|(i, x)| {
            x.severity == crate::diagnostics::Severity::Error && x.file == file && (i >= f.diag_start || f.decl_span.is_some_and(|s| s.start <= x.span.start && x.span.end <= s.end))
        })
    }

    /// Spelling of a type for messages: declared types of the current
    /// namespace by their short name (`Point`), others by their full path
    /// (`app.models.User`).
    pub fn tn(&self, t: TypeId) -> String {
        hir::dump::type_name(&self.tt, t, self)
    }

    /// Spelling of a type with full paths for every declared type (names
    /// of generic instances, independent of the current namespace).
    pub fn tn_full(&self, t: TypeId) -> String {
        hir::dump::type_name(&self.tt, t, &FullNames(self))
    }

    /// Short or full name of a declared type of HIR namespace `ns`.
    fn decl_display(&self, ns: hir::NamespaceId, name: &str, full: &str) -> String {
        let here = self.nss.get(self.cur_ns).and_then(|n| n.hir_id);
        if here == Some(ns) && !crate::sema::stdlib::is_std_namespace(&full) {
            name.to_string()
        } else {
            full.to_string()
        }
    }

    // -- function contexts ---------------------------------------------------

    /// The innermost function context. Panics outside of a body (callers
    /// only use it while checking expressions, which always have one).
    pub fn fcx(&mut self) -> &mut FnCtx {
        self.fns.last_mut().expect("no function context")
    }

    /// Declares a new local in the innermost function and scope.
    pub fn declare_local(&mut self, name: &str, ty: TypeId, kind: LocalKind, span: Span) -> LocalId {
        let dup = self.fns.last().and_then(|f| f.scopes.last()).is_some_and(|s| s.iter().any(|(n, _)| n == name));
        let temp = matches!(kind, LocalKind::Temp | LocalKind::DeferArg);
        if dup && name != "_" && !temp {
            let prev = self.fns.last().and_then(|f| {
                f.scopes.last()?.iter().rev().find(|(n, _)| n == name).and_then(|(_, e)| match e {
                    ScopeEntry::Local(l) => Some(f.locals[l.0 as usize].span),
                    _ => None,
                })
            });
            match prev {
                Some(p) => self.err_note(format!("{name} redeclared in this block"), span, "previously declared here", Some(p)),
                None => self.err(format!("{name} redeclared in this block"), span),
            }
        }
        let f = self.fcx();
        let id = LocalId(f.locals.len() as u32);
        f.locals.push(hir::Local { name: name.to_string(), ty, kind, captured: false, address_taken: false, span });
        // Parameters, pattern bindings, catch variables and temps never count
        // as unused.
        f.used.push(!matches!(kind, LocalKind::Var | LocalKind::LoopVar));
        if !temp && name != "_" {
            f.scopes.last_mut().unwrap().push((name.to_string(), ScopeEntry::Local(id)));
            self.ide_def_local(name, id, span);
        }
        id
    }

    /// Declares a fresh compiler temporary.
    pub fn temp(&mut self, ty: TypeId, span: Span) -> LocalId {
        self.declare_local("$tmp", ty, LocalKind::Temp, span)
    }

    pub fn push_scope(&mut self) {
        self.fcx().scopes.push(Vec::new());
    }

    /// True while checking a concrete instance of a generic function (or a
    /// lambda inside one).
    pub(crate) fn in_concrete_instance(&self) -> bool {
        self.fns.iter().any(|f| f.func.is_some_and(|fid| self.gen.insts.get(&fid.0).is_some_and(|i| i.note.is_some())))
    }

    /// Pops a scope, reporting unused variables.
    pub fn pop_scope(&mut self) {
        let scope = self.fcx().scopes.pop().unwrap_or_default();
        for (name, e) in scope {
            if let ScopeEntry::Local(l) = e {
                let f = self.fns.last().unwrap();
                // A variable with a destructor is used by its destructor.
                // In a concrete generic instance code dropped by a static
                // `if` may hold the only use; the abstract check of the
                // body sees every branch and reports unused variables.
                if !self.refl.ct_descs.is_empty() {
                    // In a copy of an unrolled descriptor loop: unused only if
                    // unused in every copy (decided when the loop ends).
                    let span = f.locals[l.0 as usize].span;
                    if f.used[l.0 as usize] || f.recovering || self.type_has_dtor(f.locals[l.0 as usize].ty) || self.in_concrete_instance() {
                        self.refl.copy_used.insert((self.cur_file, span));
                    } else if self.gen.mute == 0 {
                        self.refl.copy_unused.push((self.cur_file, span, name));
                    }
                    continue;
                }
                if !f.used[l.0 as usize] && !f.recovering && !self.type_has_dtor(f.locals[l.0 as usize].ty) && !self.in_concrete_instance() {
                    let span = f.locals[l.0 as usize].span;
                    self.report_unused(&name, span);
                }
            }
        }
    }

    /// The unused-variable error for local `name` declared at `span`.
    pub(crate) fn report_unused(&mut self, name: &str, span: Span) {
        let fix = Fix::edit(format!("Rename {name} to _"), span, "_");
        let mut d = Diagnostic::error(format!("unused variable {name}: declared and never used"), span)
            .in_file(self.cur_file)
            .with_code(codes::UNUSED_VARIABLE)
            .with_help("use it, remove it, or rename it to `_`");
        d.fixes = vec![fix];
        self.push_diag(d);
    }

    /// Looks a name up in local scopes, innermost function first, creating
    /// capture chains for outer functions' locals. Marks the local used
    /// when `mark_used`.
    pub fn lookup_local(&mut self, name: &str, mark_used: bool) -> Option<Result<VarRef, Folded>> {
        let depth = self.fns.len();
        for d in (0..depth).rev() {
            let entry = self.fns[d].lookup(name).cloned();
            match entry {
                None => continue,
                Some(ScopeEntry::Const(c)) => return Some(Err(c)),
                Some(ScopeEntry::Local(l)) => {
                    if mark_used {
                        self.fns[d].used[l.0 as usize] = true;
                    }
                    if d == depth - 1 {
                        return Some(Ok(VarRef::Local(l)));
                    }
                    return Some(Ok(VarRef::Upvalue(self.capture_chain(d, l))));
                }
            }
        }
        None
    }

    /// Makes local `l` of function context `d` available in the innermost
    /// lambda; returns the upvalue index there.
    fn capture_chain(&mut self, d: usize, l: LocalId) -> u32 {
        self.fns[d].locals[l.0 as usize].captured = true;
        let name = self.fns[d].locals[l.0 as usize].name.clone();
        let ty = self.fns[d].locals[l.0 as usize].ty;
        let mut source = hir::CaptureSource::Local(l);
        let mut idx = 0;
        for j in d + 1..self.fns.len() {
            let f = &mut self.fns[j];
            idx = match f.capture_keys.iter().position(|k| *k == (d, l)) {
                Some(i) => i as u32,
                None => {
                    f.capture_keys.push((d, l));
                    f.captures.push(hir::Capture { name: name.clone(), ty, source });
                    (f.captures.len() - 1) as u32
                }
            };
            source = hir::CaptureSource::Upvalue(idx);
        }
        idx
    }

    /// Type of a local/upvalue reference.
    pub fn var_ty(&self, v: VarRef) -> TypeId {
        let f = self.fns.last().unwrap();
        match v {
            VarRef::Local(l) => f.locals[l.0 as usize].ty,
            VarRef::Upvalue(i) => f.captures[i as usize].ty,
        }
    }

    /// Marks a local as having its address taken (follows captures).
    pub fn mark_address_taken(&mut self, e: &hir::Expr) {
        match &e.kind {
            hir::ExprKind::Local(l) => {
                if let Some(f) = self.fns.last_mut() {
                    f.locals[l.0 as usize].address_taken = true;
                }
            }
            hir::ExprKind::Field { base, .. } | hir::ExprKind::Index { base, .. } => {
                if matches!(self.tt.get(base.ty), Type::Struct(_) | Type::Array(..)) {
                    self.mark_address_taken(base)
                }
            }
            _ => {}
        }
    }

    /// Allocates a label for a loop or switch and pushes it.
    pub fn push_label(&mut self, kind: LabelKind) -> Label {
        let f = self.fcx();
        let l = Label(f.next_label);
        f.next_label += 1;
        f.labels.push((l, kind));
        if let Some((name, span)) = f.pending_label.take() {
            f.named_labels.push((l, name, span, false));
        }
        l
    }

    pub fn pop_label(&mut self) {
        let f = self.fcx();
        let Some((l, _)) = f.labels.pop() else { return };
        if f.named_labels.last().is_some_and(|n| n.0 == l) {
            let (_, name, span, used) = f.named_labels.pop().unwrap();
            if !used {
                self.err(format!("label {name} defined and not used"), span);
            }
        }
    }

    /// The target of `break name` / `continue name` (Go labels): the
    /// enclosing statement labeled `name`.
    pub fn named_target(&mut self, name: &ast::Ident, is_continue: bool) -> Option<Label> {
        let f = self.fcx();
        let Some(i) = f.named_labels.iter().rposition(|n| n.1 == name.name) else {
            let what = if is_continue { "continue" } else { "break" };
            f.bad_jumps.push((name.name.clone(), name.span, what));
            return None;
        };
        f.named_labels[i].3 = true;
        let l = f.named_labels[i].0;
        if is_continue && f.labels.iter().any(|x| x.0 == l && x.1 == LabelKind::Switch) {
            self.err(format!("invalid continue label {}: it labels a switch, not a loop", name.name), name.span);
            return None;
        }
        Some(l)
    }

    /// Reports the `break`/`continue` labels of the finished body that name
    /// no enclosing statement.
    pub fn report_bad_jumps(&mut self) {
        let f = self.fcx();
        let bad = std::mem::take(&mut f.bad_jumps);
        let declared = f.declared_labels.clone();
        for (name, span, what) in bad {
            if declared.contains(&name) {
                self.err(format!("invalid {what} label {name}: it does not label an enclosing for or switch"), span);
            } else {
                self.err(format!("{what} label not defined: {name}"), span);
            }
        }
    }

    /// Innermost break target / loop.
    pub fn break_target(&mut self) -> Option<Label> {
        self.fns.last().and_then(|f| f.labels.last().map(|l| l.0))
    }

    pub fn continue_target(&mut self) -> Option<Label> {
        self.fns.last().and_then(|f| f.labels.iter().rev().find(|l| l.1 == LabelKind::Loop).map(|l| l.0))
    }

    /// Adds (or reuses) an extern for a stdlib function.
    pub fn extern_id(&mut self, path: String, symbol: &str, params: Vec<TypeId>, ret: TypeId) -> hir::ExternId {
        if let Some(&id) = self.extern_map.get(symbol) {
            return id;
        }
        let id = hir::ExternId(self.externs.len() as u32);
        self.externs.push(hir::Extern { path, symbol: symbol.to_string(), params, ret });
        self.extern_map.insert(symbol.to_string(), id);
        id
    }

    /// Reports imports no name was looked up through (like Go). Not when
    /// other errors were found (a failed lookup may hide the use).
    fn check_unused_imports(&mut self) {
        // Unit mode: candidates of the full files, whatever the errors.
        let unit = self.unit.is_some();
        if !unit && self.diags.has_errors() {
            return;
        }
        let mark = self.diags.items.len();
        for fi in 0..self.file_scopes.len() {
            if self.unit.as_ref().is_some_and(|u| !u.full.get(fi).copied().unwrap_or(false)) {
                continue;
            }
            for (span, used) in self.file_scopes[fi].import_uses.clone() {
                let imp = self.files[fi].imports.iter().find(|i| i.path.span == span);
                if !used {
                    self.cur_file = fi;
                    let path = imp.map(|i| i.path.to_string()).unwrap_or_default();
                    let full = imp.map_or(span, |i| i.span);
                    let fix = Fix::edit(format!("Remove import {path}"), full, "");
                    self.err_fix(format!("unused import {path}: nothing from it is used"), span, codes::UNUSED_IMPORT, vec![fix]);
                }
            }
        }
        if let Some(u) = self.unit.as_mut() {
            u.unused_imports = self.diags.items.split_off(mark);
            u.diag_nodes.retain(|(i, _)| *i < mark);
        }
    }

    /// Checks one function body (no-op for synthetic functions).
    fn check_function_body(&mut self, fid: FuncId) {
        let decl = match self.func_src[fid.0 as usize] {
            FuncSrc::Free(d) => d,
            FuncSrc::Method(d, _) => d,
            FuncSrc::Synthetic => return,
        };
        let Some(body) = &decl.body else { return };
        if self.gen.insts.contains_key(&fid.0) {
            self.check_instance_body(fid, decl, body);
            return;
        }
        if self.unit_skips_body(fid) {
            return;
        }
        let f = &self.funcs[fid.0 as usize];
        self.cur_file = f.file;
        if let Some(u) = self.unit.as_mut() {
            u.body_file = f.file;
        }
        if let Some(&ns) = self.std_fn_ns.get(&fid.0) {
            // A stdlib default method: its namespace's scope, nothing
            // recorded for the IDE (the spans are in the stdlib source).
            self.cur_ns = ns;
            let ide = self.ide.take();
            self.check_body_in_scope(fid, decl, body);
            self.ide = ide;
            return;
        }
        self.cur_ns = self.ns_of_hir(f.namespace);
        if self.unit.is_some() {
            // The namespace of the file (std namespaces have no HIR id).
            if let Some(fs) = self.file_scopes.get(self.cur_file) {
                self.cur_ns = fs.ns;
            }
        }
        self.check_body_in_scope(fid, decl, body);
    }

    /// Checks a function body in the current file/namespace scope.
    pub(crate) fn check_body_in_scope(&mut self, fid: FuncId, decl: &ast::FuncDecl, body: &ast::Block) {
        let f = &self.funcs[fid.0 as usize];
        let sig = self.sigs[fid.0 as usize].clone();
        let mut ctx = FnCtx::new(Some(fid), sig.ret, sig.is_async, f.name.clone());
        ctx.recovering = body.has_errors();
        ctx.diag_start = self.diags.items.len();
        ctx.decl_span = Some(decl.span);
        // Parameters were registered as locals by `resolve_signatures`.
        ctx.locals = f.locals.clone();
        ctx.used = vec![true; ctx.locals.len()];
        for (i, l) in f.locals.iter().enumerate() {
            if l.name != "_" {
                ctx.scopes[0].push((l.name.clone(), ScopeEntry::Local(LocalId(i as u32))));
            }
        }
        self.fns.push(ctx);
        let block = self.fn_body(body, sig.ret, decl.name.span);
        let ctx = self.fns.pop().unwrap();
        let f = &mut self.funcs[fid.0 as usize];
        f.locals = ctx.locals;
        f.body = block;
    }

    /// Checks a function body block and the missing-return rule.
    pub fn fn_body(&mut self, body: &ast::Block, ret: TypeId, name_span: Span) -> hir::Block {
        self.fcx().open_defers = stmt::open_defers_ok(&body.stmts);
        let block = self.block(body);
        self.report_bad_jumps();
        let needs_return = !matches!(self.tt.get(ret), Type::Void | Type::Invalid)
            && !matches!(self.tt.fallible_inner(ret).map(|t| self.tt.get(t)), Some(Type::Void));
        if needs_return && !body.has_errors() && !stmt::terminates(&block.stmts, false) {
            let name = self.fns.last().map(|f| f.name.clone()).unwrap_or_default();
            let end = Span::new(body.span.end.saturating_sub(1), body.span.end);
            self.err_note(
                format!("missing return at end of function {name} returning {}", self.tn(ret)),
                end,
                "every path must end in return or panic (an if needs an else; a switch needs a default)",
                Some(name_span),
            );
        }
        block
    }

    /// Maps a HIR namespace id back to the analyser's namespace index.
    pub fn ns_of_hir(&self, id: hir::NamespaceId) -> NsIdx {
        self.nss.iter().position(|n| n.hir_id == Some(id)).unwrap_or(0)
    }

    /// Placeholder function (filled in later).
    pub fn dummy_function(&self, name: String, kind: hir::FuncKind, ns: hir::NamespaceId) -> hir::Function {
        hir::Function {
            name: name.clone(),
            full_name: name.clone(),
            symbol: format!("zell_{}", name.replace('.', "__")),
            kind,
            namespace: ns,
            params: Vec::new(),
            ret: self.tt.void,
            is_async: false,
            locals: Vec::new(),
            captures: Vec::new(),
            body: hir::Block::default(),
            attrs: Vec::new(),
            param_attrs: Vec::new(),
            exported: false,
            file: self.cur_file,
            span: Span::DUMMY,
        }
    }

    /// The HIR namespace id of the current namespace.
    pub fn cur_hir_ns(&self) -> hir::NamespaceId {
        self.nss[self.cur_ns].hir_id.unwrap_or(hir::NamespaceId(0))
    }

    /// Runs `f` with the file/namespace context of `file`, saving and
    /// restoring the current function stack.
    pub fn in_file<R>(&mut self, file: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = (self.cur_file, self.cur_ns, std::mem::take(&mut self.fns), std::mem::take(&mut self.gen.tenv), self.gen.note.take());
        self.cur_file = file;
        self.cur_ns = self.file_scopes[file].ns;
        let r = f(self);
        self.cur_file = saved.0;
        self.cur_ns = saved.1;
        self.fns = saved.2;
        self.gen.tenv = saved.3;
        self.gen.note = saved.4;
        r
    }
}

impl hir::dump::DeclNames for Sema<'_> {
    fn struct_name(&self, id: u32) -> String {
        self.structs.get(id as usize).map(|s| self.decl_display(s.namespace, &s.name, &s.full_name)).unwrap_or_default()
    }
    fn interface_name(&self, id: u32) -> String {
        self.interfaces.get(id as usize).map(|s| self.decl_display(s.namespace, &s.name, &s.full_name)).unwrap_or_default()
    }
    fn enum_name(&self, id: u32) -> String {
        self.enums.get(id as usize).map(|s| self.decl_display(s.namespace, &s.name, &s.full_name)).unwrap_or_default()
    }
    fn param_name(&self, id: u32) -> String {
        self.gen.tparams.get(id as usize).map(|p| p.name.clone()).unwrap_or_default()
    }
}

/// Full paths for every declared type (see [`Sema::tn_full`]).
struct FullNames<'s, 'a>(&'s Sema<'a>);

impl hir::dump::DeclNames for FullNames<'_, '_> {
    fn struct_name(&self, id: u32) -> String {
        self.0.structs.get(id as usize).map(|s| s.full_name.clone()).unwrap_or_default()
    }
    fn interface_name(&self, id: u32) -> String {
        self.0.interfaces.get(id as usize).map(|s| s.full_name.clone()).unwrap_or_default()
    }
    fn enum_name(&self, id: u32) -> String {
        self.0.enums.get(id as usize).map(|s| s.full_name.clone()).unwrap_or_default()
    }
    fn param_name(&self, id: u32) -> String {
        self.0.gen.tparams.get(id as usize).map(|p| p.name.clone()).unwrap_or_default()
    }
}

/// True if `name` is exported (starts with an uppercase letter).
pub(crate) fn is_exported(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Shorthand for a HIR expression.
pub(crate) fn mk(kind: hir::ExprKind, ty: TypeId, span: Span) -> hir::Expr {
    hir::Expr { kind, ty, span }
}
