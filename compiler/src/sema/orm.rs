//! Compile-time expansion for `data` (docs/FRAMEWORK.md section 3),
//! with source templates: the compiler generates Zell
//! source of stdlib templates in namespace `data` whose type
//! parameters are bound to the concrete types, and instantiates them.
//!
//! * `data.modelOf<T>()` (called by the runtime library) becomes the
//!   constant model of entity `T` ([`super::orm_model`]) with typed
//!   accessors: row scanning, column values and setters, written out per
//!   field (no runtime reflection).
//! * `data.Open<C>(provider, dsn)` / `OpenDB<C>(provider, db)` create the
//!   context `C`: every `Set<T>` field is bound to the shared state and
//!   the model of `T`; the tables of the context are registered (for
//!   `EnsureCreated`).
//! * `migrationsOf()` (behind `ctx.Migrate()`) lists every struct of the
//!   program with `@migration("id")`, in id order.
//! * Query operators with lambdas on `Set<T>`/`Query<T>`/`Grouped<T, K>`
//!   (`Where`, `OrderBy`, `Select`, `Include`, `GroupBy`, `Join`, `Sum`,
//!   `ExecuteUpdate`, ...) are translated to SQL fragments for both
//!   dialects ([`super::orm_sql`]); the call becomes a call of the
//!   runtime's fragment method (`where_`, `select_<R>`, ...) with the
//!   SQL as constant strings and the captured values as arguments.
//!   `Select` also generates the row mapper of its result type (tuple,
//!   struct or scalar); `Include` and `ctx.Entry(e).Collection("X")`
//!   generate the loaders of the navigation.
//! * `ModelBuilder` calls with lambdas (`HasIndex((u) => u.Email)`) are
//!   checked here; their effect on the model is read from the source of
//!   `Configure` ([`super::orm_model`]).

use super::coerce::Ctx;
use super::generics::{Inst, TmplDecl};
use super::orm_model::{EntModel, NavKind, NavModel};
use super::orm_sql::{lambda_body, quote, seq, Bind, Cx, Lit, Tr, S, V, VK};
use super::*;
use crate::hir::{ConstValue, ExprKind};
use std::rc::Rc;

/// Type parameters of a generated template: the concrete types in order.
#[derive(Default)]
struct Tys(Vec<TypeId>);

impl Tys {
    fn p(&mut self, t: TypeId) -> String {
        let i = match self.0.iter().position(|x| *x == t) {
            Some(i) => i,
            None => {
                self.0.push(t);
                self.0.len() - 1
            }
        };
        format!("T{i}")
    }

    fn generics(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        format!("<{}>", (0..self.0.len()).map(|i| format!("T{i}")).collect::<Vec<_>>().join(", "))
    }
}

/// A Zell string literal.
fn lit(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn lits(xs: &[String]) -> String {
    format!("[{}]", xs.iter().map(|x| lit(x)).collect::<Vec<_>>().join(", "))
}

/// Parsed generated source, every token at `span` (cached per process).
fn parsed_template(src: &str, span: Span) -> &'static ast::File {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(String, u32, u32), &'static ast::File>>> = OnceLock::new();
    let key = (src.to_string(), span.start, span.end);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(f) = cache.lock().unwrap().get(&key) {
        return f;
    }
    let mut d = crate::diagnostics::Diagnostics::new();
    let mut toks = crate::lexer::lex(src, &mut d);
    for t in &mut toks {
        t.span = span;
    }
    let file = match crate::parser::parse(toks, &mut d) {
        Some(f) if !d.has_errors() => f,
        _ => {
            let errs: Vec<String> = d.items.iter().map(|x| x.message.clone()).collect();
            panic!("internal error: generated data code does not parse: {errs:?}\n{src}")
        }
    };
    let file: &'static ast::File = Box::leak(Box::new(file));
    cache.lock().unwrap().insert(key, file);
    file
}

/// Methods of `Query<T>` reachable from a `Set<T>`.
const QUERY_OPS: &[&str] = &[
    "Where", "OrderBy", "OrderByDescending", "ThenBy", "ThenByDescending", "Select", "Include", "First", "FirstOrDefault", "Single", "SingleOrDefault",
    "Count", "Any", "All", "Sum", "Min", "Max", "Average", "GroupBy", "Join", "ExecuteUpdate", "ExecuteDelete", "AsNoTracking", "AsTracking", "AsSplitQuery",
    "AsSingleQuery", "Skip", "Take", "Distinct", "ToList", "ToSql", "Args", "AsEnumerable",
];

/// Shape of a projection result.
enum Shape {
    Leaf(usize),
    Tuple(Vec<Shape>),
    /// (field, leaf)
    Struct(Vec<(String, usize)>),
}

impl<'a> Sema<'a> {
    // -- generated templates ---------------------------------------------------------

    fn orm_template(&mut self, src: String, span: Span) -> u32 {
        if let Some(&t) = self.orm.tmpls.get(&src) {
            return t;
        }
        if std::env::var_os("ZELL_DATA_DUMP").is_some() {
            eprintln!("{src}");
        }
        let file = parsed_template(&src, span);
        let ast::Decl::Func(f) = &file.decls[0] else { panic!("internal error: generated data code is not a function") };
        let saved = self.cur_ns;
        self.cur_ns = self.ensure_ns("data", Span::DUMMY);
        let t = self.new_template(&f.name.name, TmplDecl::Func(f), &f.generics, Vec::new(), false);
        self.gen.templates[t as usize].generated = true;
        self.cur_ns = saved;
        self.orm.tmpls.insert(src, t);
        t
    }

    /// A call of the generated function `src` instantiated with `tys`.
    fn orm_call(&mut self, src: String, tys: Vec<TypeId>, args: Vec<hir::Expr>, span: Span) -> hir::Expr {
        let t = self.orm_template(src, span);
        let Some(Inst::Func(fid)) = self.instantiate(t, tys, span) else { return self.invalid(span) };
        let sig = self.sigs[fid.0 as usize].clone();
        let fname = self.funcs[fid.0 as usize].name.clone();
        let args: Vec<hir::Expr> = args.into_iter().zip(sig.params.iter()).enumerate().map(|(i, (a, p))| self.coerce(a, *p, Ctx::Arg(i, fname.clone()))).collect();
        let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
        mk(ExprKind::Call { callee: hir::Callee::Func(fid), args }, ty, span)
    }

    fn orm_name(&mut self, base: &str) -> String {
        self.orm.counter += 1;
        format!("{base}{}", self.orm.counter)
    }

    /// Calls method `m` of the data struct value `recv` (generic
    /// methods instantiated with `tys`).
    fn orm_mcall(&mut self, recv: hir::Expr, m: &str, tys: Vec<TypeId>, args: Vec<hir::Expr>, span: Span) -> hir::Expr {
        let Some(sid) = self.struct_of(recv.ty) else { return self.invalid(span) };
        let fid = if let Some(&mt) = self.gen.methods.get(&(sid, m.to_string())) {
            match self.instantiate(mt, tys, span) {
                Some(Inst::Func(f)) => f,
                _ => return self.invalid(span),
            }
        } else {
            match self.find_method(sid, m) {
                Some((f, _)) => f,
                None => panic!("internal error: data method {m} not found on {}", self.structs[sid as usize].name),
            }
        };
        let recv = if matches!(self.tt.get(recv.ty), Type::Pointer(_)) { recv } else { self.addr_of(recv) };
        let sig = self.sigs[fid.0 as usize].clone();
        let fname = self.funcs[fid.0 as usize].name.clone();
        let mut hargs = vec![recv];
        for (i, (a, p)) in args.into_iter().zip(sig.params.iter()).enumerate() {
            hargs.push(self.coerce(a, *p, Ctx::Arg(i, fname.clone())));
        }
        let ty = if sig.is_async { self.tt.task(sig.ret) } else { sig.ret };
        mk(ExprKind::Call { callee: hir::Callee::Func(fid), args: hargs }, ty, span)
    }

    fn orm_str(&self, s: &str, span: Span) -> hir::Expr {
        mk(ExprKind::Const(ConstValue::Str(s.to_string())), self.tt.string, span)
    }

    fn orm_int(&self, n: i64, span: Span) -> hir::Expr {
        mk(ExprKind::Const(ConstValue::Int(n as i128)), self.tt.int, span)
    }

    fn orm_bool(&self, b: bool, span: Span) -> hir::Expr {
        mk(ExprKind::Const(ConstValue::Bool(b)), self.tt.bool_, span)
    }

    /// The `[]any` argument list of a translation.
    fn orm_args(&mut self, args: Vec<hir::Expr>, span: Span) -> hir::Expr {
        let sl = self.tt.slice(self.tt.any);
        let any = self.tt.any;
        let vs: Vec<hir::Expr> = args.into_iter().map(|a| self.coerce(a, any, Ctx::Elem)).collect();
        mk(ExprKind::SliceLit(vs), sl, span)
    }

    /// (pg, lite, args) HIR arguments of a fragment.
    fn orm_frag(&mut self, s: &S, args: Vec<hir::Expr>, span: Span) -> Vec<hir::Expr> {
        let pg = self.orm_str(&s.render(true), span);
        let lite = self.orm_str(&s.render(false), span);
        let a = self.orm_args(args, span);
        vec![pg, lite, a]
    }

    fn orm_fail_expr(&mut self, e: Option<(String, Span)>, span: Span) -> hir::Expr {
        self.orm_report(e);
        self.invalid(span)
    }

    // -- entry points ------------------------------------------------------------------

    /// Generic data functions expanded at compile time: `modelOf<T>()`,
    /// `migrationsOf()`, `Open<C>(p, dsn)`, `OpenDB<C>(p, db)`.
    pub(crate) fn orm_generic_call(&mut self, t: u32, targs: Option<&[ast::TypeExpr]>, args: &[ast::Expr], span: Span) -> Option<hir::Expr> {
        let tm = &self.gen.templates[t as usize];
        if !tm.is_std || self.nss[tm.ns].path != "data" {
            return None;
        }
        let name = tm.name.clone();
        if !matches!(name.as_str(), "modelOf" | "migrationsOf" | "Open" | "OpenDB") {
            return None;
        }
        let tys: Vec<TypeId> = match targs {
            Some(ta) => {
                let v: Vec<TypeId> = ta.iter().map(|x| self.resolve_type(x)).collect();
                if v.iter().any(|x| self.tt.is_invalid(*x)) {
                    self.check_args_loose(args);
                    return Some(self.invalid(span));
                }
                v
            }
            None => Vec::new(),
        };
        if tys.iter().any(|x| self.has_param(*x)) {
            return None;
        }
        match name.as_str() {
            "modelOf" => {
                let t = *tys.first()?;
                Some(self.orm_model_expr(t, span))
            }
            "migrationsOf" => Some(self.orm_migrations_expr(span)),
            _ => {
                let Some(&c) = tys.first() else {
                    self.err(format!("data.{name} needs the context type: data.{name}<ShopContext>(data.Provider.Sqlite, \":memory:\")"), span);
                    self.check_args_loose(args);
                    return Some(self.invalid(span));
                };
                Some(self.orm_open(c, name == "OpenDB", args, span))
            }
        }
    }

    /// Method calls expanded at compile time; gives `base` back otherwise.
    pub(crate) fn orm_method_call(&mut self, base: hir::Expr, name: &ast::Ident, targs: Option<&[ast::TypeExpr]>, args: &[ast::Expr], span: Span) -> Result<hir::Expr, hir::Expr> {
        let Some(sid) = self.struct_of(base.ty) else { return Err(base) };
        let Some((tname, tas)) = self.orm_inst(sid) else { return Err(base) };
        if tas.iter().any(|t| self.has_param(*t)) {
            return Err(base);
        }
        let n = name.name.as_str();
        match tname.as_str() {
            "Set" if n == "UpsertBy" => Ok(self.orm_upsert_by(base, tas[0], args, span)),
            "Set" => {
                if self.find_method(sid, n).is_some() || self.gen.methods.contains_key(&(sid, n.to_string())) || !QUERY_OPS.contains(&n) {
                    return Err(base);
                }
                let q = self.orm_mcall(base, "AsQuery", vec![], vec![], span);
                Ok(self.method_call_targs(q, name, targs, args, span))
            }
            "Query" => self.orm_query_op(base, tas[0], n, args, span),
            "Grouped" => self.orm_group_call(base, tas[0], tas[1], n, args, span),
            "Projection" => match n {
                "Where" | "OrderBy" | "OrderByDescending" | "ThenBy" | "ThenByDescending" | "Select" | "Include" | "GroupBy" | "Sum" | "Min" | "Max" | "Average" | "Join" | "All" => {
                    self.check_args_loose(&[]);
                    let e = Some((format!("{n} after Select: the projection's columns are not entities; apply {n} before Select"), name.span));
                    Ok(self.orm_fail_expr(e, span))
                }
                _ => Err(base),
            },
            "Entry" if matches!(n, "Collection" | "Reference") => Ok(self.orm_entry_nav(base, tas[0], n, args, span)),
            "EntityBuilder" if matches!(n, "HasKey" | "Ignore" | "Property" | "HasIndex" | "HasMany" | "HasOne" | "BelongsTo" | "HasManyToMany") => Ok(self.orm_fluent_call(base, tas[0], n, args, span)),
            _ => Err(base),
        }
    }

    // -- model ------------------------------------------------------------------------

    fn meta_src(&self, m: &EntModel) -> String {
        let cols: Vec<String> = m
            .cols
            .iter()
            .map(|c| {
                format!(
                    "ColumnMeta{{Name: {}, Field: {}, Type: {}, SqlType: {}, Nullable: {}, Key: {}, Auto: {}, MaxLength: {}, Default: {}, Token: {}}}",
                    lit(&c.name),
                    lit(&c.field),
                    lit(c.kind),
                    lit(&c.sql_type),
                    c.nullable,
                    c.key,
                    c.auto,
                    c.max_len,
                    lit(&c.default),
                    c.token
                )
            })
            .collect();
        let idx: Vec<String> = m.indexes.iter().map(|i| format!("IndexMeta{{Name: {}, Columns: {}, Unique: {}}}", lit(&i.name), lits(&i.cols), i.unique)).collect();
        let fks: Vec<String> = m
            .fks
            .iter()
            .map(|f| format!("ForeignKeyMeta{{Name: {}, Columns: {}, RefTable: {}, RefColumns: {}, OnDelete: {}}}", lit(&f.name), lits(&f.cols), lit(&f.ref_table), lits(&f.ref_cols), lit(&f.on_delete)))
            .collect();
        format!(
            "new EntityMeta{{Entity: {}, Table: {}, Columns: [{}], Key: {}, Token: {}, Indexes: [{}], ForeignKeys: [{}]}}",
            lit(&m.name),
            lit(&m.table),
            cols.join(", "),
            m.key.map(|k| k as i64).unwrap_or(-1),
            m.token.map(|k| k as i64).unwrap_or(-1),
            idx.join(", "),
            fks.join(", ")
        )
    }

    /// `modelOf<T>()`: the model and accessors of entity `t`.
    fn orm_model_expr(&mut self, t: TypeId, span: Span) -> hir::Expr {
        let Some(m) = self.orm_model(t) else { return self.invalid(span) };
        let mut scan = String::new();
        let mut values = String::new();
        let mut set = String::new();
        for (i, c) in m.cols.iter().enumerate() {
            scan.push_str(&format!("            database.sql.convertInto(&__e.{}, __v[__o + {i}], {})?\n", c.field, lit(&c.name)));
            if c.kind == "enum" {
                values.push_str(&format!("            __r = append(__r, fmt.Format(\"{{0}}\", __e.{}))\n", c.field));
            } else {
                values.push_str(&format!("            __r = append(__r, database.sql.argOf(__e.{}))\n", c.field));
            }
            set.push_str(&format!("            if __i == {i} {{\n                database.sql.convertInto(&__e.{}, __x, {})?\n            }}\n", c.field, lit(&c.name)));
        }
        let mut unlink = String::new();
        for n in &m.navs {
            if n.kind == NavKind::ManyMany {
                unlink.push_str(&format!("            __e.{} = []\n", n.field));
            } else if !n.many && n.ptr {
                unlink.push_str(&format!("            __e.{} = nil\n", n.field));
            }
        }
        let fname = format!("ormModel_{}", m.name);
        let src = format!(
            "namespace data\n\nfunc {fname}<T0>() *Entity<T0> {{\n    return new Entity<T0>{{\n        Meta: {},\n        scan: func(__v []any, __o int, __e *T0) void! {{\n{scan}        }},\n        values: func(__e *T0) []any {{\n            var __r []any = []\n{values}            return __r\n        }},\n        set: func(__e *T0, __i int, __x any) void! {{\n{set}        }},\n        unlink: func(__e *T0) {{\n{unlink}        }},\n    }}\n}}\n",
            self.meta_src(&m)
        );
        self.orm_call(src, vec![t], vec![], span)
    }

    /// `data.Open<C>(p, dsn)` / `data.OpenDB<C>(p, db)`.
    fn orm_open(&mut self, c: TypeId, with_db: bool, args: &[ast::Expr], span: Span) -> hir::Expr {
        let who = if with_db { "OpenDB" } else { "Open" };
        let cn = self.tn(c);
        let sid = match *self.tt.get(c) {
            Type::Struct(d) if self.orm_is_context(d.0) => d.0,
            _ => {
                self.err(format!("data.{who}<{cn}>: {cn} is not a context (a struct embedding data.Context with data.Set<T> fields)"), span);
                self.check_args_loose(args);
                return self.invalid(span);
            }
        };
        if args.len() != 2 {
            self.err(format!("data.{who} takes a provider and {}", if with_db { "a *sql.DB" } else { "a data source name" }), span);
            self.check_args_loose(args);
            return self.invalid(span);
        }
        self.orm_universe();
        let mut tys = Tys::default();
        let cp = tys.p(c);
        let sets: Vec<(String, TypeId)> = self.structs[sid as usize].fields.iter().filter_map(|f| self.orm_set_arg(f.ty).map(|t| (f.name.clone(), t))).collect();
        let mut body = String::new();
        let mut ents: Vec<Rc<EntModel>> = Vec::new();
        let mut ok = true;
        for (f, t) in &sets {
            let Some(m) = self.orm_model(*t) else {
                ok = false;
                continue;
            };
            let p = tys.p(*t);
            body.push_str(&format!("    __c.{f} = Set<{p}>{{st: __st, ent: modelOf<{p}>()}}\n"));
            if !ents.iter().any(|e| e.ty == m.ty) {
                ents.push(m);
            }
        }
        if !ok {
            self.check_args_loose(args);
            return self.invalid(span);
        }
        // entities reachable through navigations, and join tables
        let mut i = 0;
        let mut joins: Vec<Rc<EntModel>> = Vec::new();
        while i < ents.len() {
            let navs = ents[i].navs.clone();
            for n in navs {
                if n.kind == NavKind::ManyMany {
                    if let Some(j) = self.orm.joins.iter().find(|j| j.table == n.through).cloned() {
                        if !joins.iter().any(|x| x.table == j.table) {
                            joins.push(j);
                        }
                    }
                }
                if !ents.iter().any(|e| e.ty == n.target) {
                    if let Some(m) = self.orm_model(n.target) {
                        ents.push(m);
                    }
                }
            }
            i += 1;
        }
        let mut all = ents.clone();
        all.extend(joins);
        for m in super::orm_model::order_tables(all) {
            if m.sid == u32::MAX {
                body.push_str(&format!("    __st.addTable({})\n", self.meta_src(&m)));
            } else {
                let p = tys.p(m.ty);
                body.push_str(&format!("    __st.addTable(modelOf<{p}>().Meta)\n"));
            }
        }
        let n = self.orm_name("ormOpen");
        let (params, ret, open) = if with_db {
            ("__p Provider, __db *database.sql.DB".to_string(), format!("*{cp}"), String::new())
        } else {
            ("__p Provider, __dsn string".to_string(), format!("*{cp}!"), "    var __db = openDB(__p, __dsn)?\n".to_string())
        };
        let src = format!(
            "namespace data\n\nfunc {n}{}({params}) {ret} {{\n{open}    var __st = newState(__p, __db)\n    var __c {cp}\n    __c.Context = Context{{st: __st}}\n{body}    return &__c\n}}\n",
            tys.generics()
        );
        let prov = match self.orm_provider_ty(span) {
            Some(t) => t,
            None => return self.invalid(span),
        };
        let a0 = self.expr_to(&args[0], prov, Ctx::Arg(0, format!("data.{who}")));
        let a1 = if with_db {
            let Some(dbs) = self.std_struct_ty("database.sql", "DB", span) else { return self.invalid(span) };
            let p = self.tt.pointer(dbs);
            self.expr_to(&args[1], p, Ctx::Arg(1, format!("data.{who}")))
        } else {
            let s = self.tt.string;
            self.expr_to(&args[1], s, Ctx::Arg(1, format!("data.{who}")))
        };
        self.orm_call(src, tys.0, vec![a0, a1], span)
    }

    fn orm_provider_ty(&mut self, span: Span) -> Option<TypeId> {
        let nsi = *self.ns_index.get("data")?;
        let Some((Member::Generic(t), _, _)) = self.nss[nsi].members.get("Provider").cloned() else { return None };
        self.plain_std_type(t, span)
    }

    fn std_struct_ty(&mut self, ns: &str, name: &str, span: Span) -> Option<TypeId> {
        let nsi = *self.ns_index.get(ns)?;
        let Some((Member::Generic(t), _, _)) = self.nss[nsi].members.get(name).cloned() else { return None };
        self.plain_std_type(t, span)
    }

    /// `migrationsOf()`: every `@migration` struct of the program.
    fn orm_migrations_expr(&mut self, span: Span) -> hir::Expr {
        let mut migs: Vec<(String, u32)> = Vec::new();
        for sid in 0..self.structs.len() as u32 {
            if self.gen.inst_structs.contains_key(&sid) || crate::sema::stdlib::is_std_namespace(&self.structs[sid as usize].full_name) {
                continue;
            }
            let attrs = self.structs[sid as usize].attrs.clone();
            for a in &attrs {
                if self.orm_attr(a).as_deref() == Some("migration") {
                    let id = a.args.iter().find(|(k, _)| k == "id").and_then(|(_, v)| if let ConstValue::Str(s) = v { Some(s.clone()) } else { None }).unwrap_or_default();
                    migs.push((id, sid));
                }
            }
        }
        migs.sort();
        let mut tys = Tys::default();
        let mut body = String::new();
        let mut prev = String::new();
        for (id, sid) in &migs {
            let sname = self.structs[*sid as usize].name.clone();
            let (file, sspan) = (self.structs[*sid as usize].file, self.structs[*sid as usize].span);
            if *id == prev || id.is_empty() {
                let saved = self.cur_file;
                self.cur_file = file;
                self.err(format!("data: migration {sname}: {}", if id.is_empty() { "empty migration id".to_string() } else { format!("duplicate migration id {id:?}") }), sspan);
                self.cur_file = saved;
                continue;
            }
            prev = id.clone();
            let has = |m: &str| self.find_method(*sid, m).map(|(f, _)| f);
            let (up, down) = (has("Up"), has("Down"));
            let Some(up) = up else {
                let saved = self.cur_file;
                self.cur_file = file;
                self.err(format!("data: migration {sname} needs a method Up(m *data.Migrator)"), sspan);
                self.cur_file = saved;
                continue;
            };
            for f in [Some(up), down].into_iter().flatten() {
                let sig = &self.sigs[f.0 as usize];
                if sig.params.len() != 1 || !matches!(self.tt.get(sig.ret), Type::Void) || sig.is_async {
                    let saved = self.cur_file;
                    self.cur_file = file;
                    self.err(format!("data: migration {sname}: Up and Down must be func(m *data.Migrator) (synchronous, no result)"), sspan);
                    self.cur_file = saved;
                }
            }
            let ty = self.struct_ty(*sid);
            let p = tys.p(ty);
            let d = if down.is_some() { format!("var __x {p}\n            __x.Down(__m)") } else { String::new() };
            body.push_str(&format!(
                "    __l = append(__l, MigrationEntry{{Id: {}, Up: func(__m *Migrator) {{\n            var __x {p}\n            __x.Up(__m)\n        }}, Down: func(__m *Migrator) {{\n            {d}\n        }}}})\n",
                lit(id)
            ));
        }
        let n = self.orm_name("ormMigrations");
        let src = format!("namespace data\n\nfunc {n}{}() []MigrationEntry {{\n    var __l []MigrationEntry = []\n{body}    return __l\n}}\n", tys.generics());
        self.orm_call(src, tys.0, vec![], span)
    }

    // -- queries ----------------------------------------------------------------------

    fn orm_query_op(&mut self, base: hir::Expr, t: TypeId, n: &str, args: &[ast::Expr], span: Span) -> Result<hir::Expr, hir::Expr> {
        let lambda_ops = ["Where", "OrderBy", "OrderByDescending", "ThenBy", "ThenByDescending", "Select", "Include", "All", "Sum", "Min", "Max", "Average", "GroupBy", "ExecuteUpdate"];
        let pred_ops = ["First", "FirstOrDefault", "Single", "SingleOrDefault", "Count", "Any"];
        let is_join = n == "Join" && args.len() == 4;
        if !(lambda_ops.contains(&n) && args.len() == 1 || pred_ops.contains(&n) && args.len() == 1 || is_join) {
            return Err(base);
        }
        let Some(m) = self.orm_model(t) else {
            self.check_args_loose(&[]);
            return Ok(self.invalid(span));
        };
        let mut cx = Cx::new();
        let ent = vec![Bind::Ent(m.clone(), "t0".into())];
        let r: Tr<hir::Expr> = (|| -> Tr<hir::Expr> {
            match n {
                "Where" => {
                    let body = self.orm_lambda(&args[0], ent, "Where", &mut cx)?;
                    let p = self.orm_pred(body, &mut cx)?;
                    let fa = self.orm_frag(&p, std::mem::take(&mut cx.args), span);
                    Ok(self.orm_mcall(base, "where_", vec![], fa, span))
                }
                "OrderBy" | "OrderByDescending" | "ThenBy" | "ThenByDescending" => {
                    let body = self.orm_lambda(&args[0], ent, n, &mut cx)?;
                    let s = self.orm_order_keys(body, n.ends_with("Descending"), &mut cx)?;
                    let mut fa = self.orm_frag(&s, std::mem::take(&mut cx.args), span);
                    fa.push(self.orm_bool(n.starts_with("Then"), span));
                    Ok(self.orm_mcall(base, "order_", vec![], fa, span))
                }
                "First" | "FirstOrDefault" | "Single" | "SingleOrDefault" | "Count" | "Any" => {
                    let body = self.orm_lambda(&args[0], ent, n, &mut cx)?;
                    let p = self.orm_pred(body, &mut cx)?;
                    let fa = self.orm_frag(&p, std::mem::take(&mut cx.args), span);
                    let q = self.orm_mcall(base, "where_", vec![], fa, span);
                    Ok(self.orm_mcall(q, n, vec![], vec![], span))
                }
                "All" => {
                    let body = self.orm_lambda(&args[0], ent, n, &mut cx)?;
                    let p = self.orm_pred(body, &mut cx)?;
                    let fa = self.orm_frag(&p, std::mem::take(&mut cx.args), span);
                    Ok(self.orm_mcall(base, "all_", vec![], fa, span))
                }
                "Sum" | "Min" | "Max" | "Average" => {
                    let body = self.orm_lambda(&args[0], ent, n, &mut cx)?;
                    let x = self.orm_value(body, &mut cx)?;
                    let ty = self.orm_ty(&x);
                    let bt = match self.orm_scalar(ty) {
                        Some((_, b, _)) => b,
                        None => ty,
                    };
                    if (n == "Sum" || n == "Average") && !matches!(self.tt.get(bt), Type::Int(_) | Type::Float32 | Type::Float64) && self.orm_scalar(bt).map(|s| s.0) != Some("decimal") {
                        return Err(Some((format!("{n} needs a number, found {}", self.tn(ty)), body.span)));
                    }
                    let (agg, rty) = self.orm_agg(n, &x.s, bt, false);
                    let rty = match self.orm_scalar(rty) {
                        Some((_, b, _)) => b,
                        None => rty,
                    };
                    let mut fa = self.orm_frag(&agg, std::mem::take(&mut cx.args), span);
                    fa.push(self.orm_int(if n == "Sum" { 0 } else { 1 }, span));
                    let what = format!("{n} {}", m.name);
                    fa.push(self.orm_str(&what, span));
                    Ok(self.orm_mcall(base, "scalar_", vec![rty], fa, span))
                }
                "Select" => {
                    let body = self.orm_lambda(&args[0], ent, "Select", &mut cx)?;
                    self.orm_projection(base, "select_", body, Vec::new(), &mut cx, span)
                }
                "Include" => {
                    let body = self.orm_lambda(&args[0], ent, "Include", &mut cx)?;
                    let v = self.orm_tr(body, &mut cx)?;
                    let nav = match v.kind {
                        VK::Coll(nv, ..) | VK::Ref(nv, ..) => nv,
                        _ => return Err(Some(("Include needs a navigation: Include((u) => u.Orders)".into(), body.span))),
                    };
                    let inc = self.orm_include(&m, &nav, span)?;
                    Ok(self.orm_mcall(base, "include_", vec![], vec![inc], span))
                }
                "GroupBy" => {
                    let body = self.orm_lambda(&args[0], ent, "GroupBy", &mut cx)?;
                    let keys = self.orm_keys(body, &mut cx)?;
                    let kt = if keys.len() == 1 { self.orm_ty(&keys[0]) } else { let ts: Vec<TypeId> = keys.iter().map(|k| self.orm_ty(k)).collect(); self.tt.tuple(ts) };
                    let a = std::mem::take(&mut cx.args);
                    let pgs: Vec<String> = keys.iter().map(|k| k.s.render(true)).collect();
                    let lites: Vec<String> = keys.iter().map(|k| k.s.render(false)).collect();
                    let ss = self.tt.slice(self.tt.string);
                    let pgl: Vec<hir::Expr> = pgs.iter().map(|s| self.orm_str(s, span)).collect();
                    let litel: Vec<hir::Expr> = lites.iter().map(|s| self.orm_str(s, span)).collect();
                    let al = self.orm_args(a, span);
                    let fa = vec![mk(ExprKind::SliceLit(pgl), ss, span), mk(ExprKind::SliceLit(litel), ss, span), al];
                    Ok(self.orm_mcall(base, "group_", vec![kt], fa, span))
                }
                "Join" => self.orm_join(base, &m, args, &mut cx, span),
                "ExecuteUpdate" => {
                    let s = self.orm_setters(&args[0], &m, &mut cx)?;
                    let fa = self.orm_frag(&s, std::mem::take(&mut cx.args), span);
                    Ok(self.orm_mcall(base, "update_", vec![], fa, span))
                }
                _ => unreachable!(),
            }
        })();
        Ok(match r {
            Ok(e) => e,
            Err(e) => self.orm_fail_expr(e, span),
        })
    }

    /// `key ASC, key2 ASC` of an ordering lambda body (a value or a tuple).
    fn orm_order_keys(&mut self, body: &ast::Expr, desc: bool, cx: &mut Cx) -> Tr<S> {
        let keys = self.orm_keys(body, cx)?;
        let dir = if desc { " DESC" } else { " ASC" };
        let mut parts = Vec::new();
        for (i, k) in keys.into_iter().enumerate() {
            if i > 0 {
                parts.push(S::t(", "));
            }
            parts.push(k.s);
            parts.push(S::t(dir));
        }
        Ok(S::Seq(parts))
    }

    /// The values of a key lambda body: one value or a tuple of values.
    fn orm_keys(&mut self, body: &ast::Expr, cx: &mut Cx) -> Tr<Vec<V>> {
        let xs: Vec<&ast::Expr> = match &expr::strip_parens(body).kind {
            ast::ExprKind::Tuple(xs) => xs.iter().collect(),
            _ => vec![body],
        };
        let mut out = Vec::new();
        for x in xs {
            let v = self.orm_value(x, cx)?;
            if matches!(v.kind, VK::Tuple(_)) {
                return Err(Some(("a composite key cannot be used here".into(), x.span)));
            }
            out.push(v);
        }
        Ok(out)
    }

    /// Translates a projection body into leaves and a shape.
    fn orm_shape(&mut self, e: &ast::Expr, leaves: &mut Vec<(S, TypeId)>, cx: &mut Cx, top: bool) -> Tr<(Shape, TypeId)> {
        let e = expr::strip_parens(e);
        match &e.kind {
            ast::ExprKind::Tuple(xs) => {
                let mut shapes = Vec::new();
                let mut tys = Vec::new();
                for x in xs {
                    let (s, t) = self.orm_shape(x, leaves, cx, false)?;
                    shapes.push(s);
                    tys.push(t);
                }
                Ok((Shape::Tuple(shapes), self.tt.tuple(tys)))
            }
            ast::ExprKind::StructLit { path, generics, fields } if top => {
                let Some(sid) = self.struct_lit_struct(path, generics, None, e.span) else { return Err(None) };
                let sty = self.struct_ty(sid);
                let mut fs = Vec::new();
                for fi in fields {
                    let Some(f) = self.structs[sid as usize].fields.iter().find(|f| f.name == fi.name.name).cloned() else {
                        let sn = self.structs[sid as usize].name.clone();
                        return Err(Some((format!("struct {sn} has no field {}", fi.name.name), fi.name.span)));
                    };
                    let v = self.orm_value(&fi.value, cx)?;
                    let vt = self.orm_ty(&v);
                    let ok = self.orm_base(vt) == self.orm_base(f.ty) || (v.ty.is_none() && self.orm_scalar(f.ty).is_some()) || matches!(self.tt.get(f.ty), Type::Float64) && matches!(self.tt.get(self.orm_base(vt)), Type::Int(_));
                    if !ok {
                        let (a, b) = (self.tn(vt), self.tn(f.ty));
                        return Err(Some((format!("field {} has type {b}, the value has type {a}", f.name), fi.value.span)));
                    }
                    leaves.push((v.s, f.ty));
                    fs.push((f.name.clone(), leaves.len() - 1));
                }
                Ok((Shape::Struct(fs), sty))
            }
            _ => {
                let v = self.orm_tr(e, cx)?;
                if let VK::Tuple(keys) = &v.kind {
                    let mut shapes = Vec::new();
                    let mut tys = Vec::new();
                    for k in keys {
                        let t = self.orm_ty(k);
                        leaves.push((k.s.clone(), t));
                        shapes.push(Shape::Leaf(leaves.len() - 1));
                        tys.push(t);
                    }
                    return Ok((Shape::Tuple(shapes), self.tt.tuple(tys)));
                }
                let v = self.orm_scalar_v(v, e.span)?;
                let t = self.orm_ty(&v);
                leaves.push((v.s, t));
                Ok((Shape::Leaf(leaves.len() - 1), t))
            }
        }
    }

    /// Generates the row mapper of a projection (`func([]any, *R) void!`).
    fn orm_mapper(&mut self, shape: &Shape, rty: TypeId, leaves: &[(S, TypeId)], span: Span) -> hir::Expr {
        let mut tys = Tys::default();
        let r = tys.p(rty);
        let mut body = String::new();
        fn tuple_expr(s: &Shape) -> String {
            match s {
                Shape::Leaf(i) => format!("__a{i}"),
                Shape::Tuple(xs) => format!("({})", xs.iter().map(tuple_expr).collect::<Vec<_>>().join(", ")),
                Shape::Struct(_) => String::new(),
            }
        }
        match shape {
            Shape::Leaf(_) => body.push_str("        database.sql.convertInto(__o, __v[0], \"0\")?\n"),
            Shape::Struct(fs) => {
                for (f, i) in fs {
                    body.push_str(&format!("        database.sql.convertInto(&__o.{f}, __v[{i}], {})?\n", lit(f)));
                }
            }
            Shape::Tuple(_) => {
                for (i, (_, t)) in leaves.iter().enumerate() {
                    let p = tys.p(*t);
                    body.push_str(&format!("        var __a{i} {p}\n        database.sql.convertInto(&__a{i}, __v[{i}], \"{i}\")?\n"));
                }
                body.push_str(&format!("        *__o = {}\n", tuple_expr(shape)));
            }
        }
        let n = self.orm_name("ormMap");
        let src = format!("namespace data\n\nfunc {n}{}() func([]any, *{r}) void! {{\n    return func(__v []any, __o *{r}) void! {{\n{body}    }}\n}}\n", tys.generics());
        self.orm_call(src, tys.0, vec![], span)
    }

    /// `recv.<method><R>(pre..., pg, lite, args, mapper)` for a Select body.
    fn orm_projection(&mut self, recv: hir::Expr, method: &str, body: &ast::Expr, pre: Vec<hir::Expr>, cx: &mut Cx, span: Span) -> Tr<hir::Expr> {
        let mut leaves = Vec::new();
        let (shape, rty) = self.orm_shape(body, &mut leaves, cx, true)?;
        let mut parts = Vec::new();
        for (i, (s, _)) in leaves.iter().enumerate() {
            if i > 0 {
                parts.push(S::t(", "));
            }
            parts.push(s.clone());
        }
        let sel = S::Seq(parts);
        let mapper = self.orm_mapper(&shape, rty, &leaves, span);
        let mut fa = pre;
        fa.extend(self.orm_frag(&sel, std::mem::take(&mut cx.args), span));
        fa.push(mapper);
        Ok(self.orm_mcall(recv, method, vec![rty], fa, span))
    }

    /// `outer.Join(inner, (u) => u.Id, (o) => o.UserId, (u, o) => (u.Name, o.Total))`.
    fn orm_join(&mut self, base: hir::Expr, m: &Rc<EntModel>, args: &[ast::Expr], cx: &mut Cx, span: Span) -> Tr<hir::Expr> {
        let inner = self.value(&args[0], None);
        if self.tt.is_invalid(inner.ty) {
            return Err(None);
        }
        let Some(u) = self.orm_set_arg(inner.ty).or_else(|| if let Type::Pointer(x) = self.tt.get(inner.ty) { self.orm_set_arg(*x) } else { None }) else {
            let tn = self.tn(inner.ty);
            return Err(Some((format!("the first argument of Join must be a set of the context (ctx.Orders), found {tn}"), args[0].span)));
        };
        let Some(um) = self.orm_model(u) else { return Err(None) };
        let ok = self.orm_lambda(&args[1], vec![Bind::Ent(m.clone(), "t0".into())], "Join (outer key)", cx)?;
        let ok = self.orm_keys(ok, cx)?;
        cx.scopes.clear();
        let ik = self.orm_lambda(&args[2], vec![Bind::Ent(um.clone(), "t1".into())], "Join (inner key)", cx)?;
        let ik = self.orm_keys(ik, cx)?;
        cx.scopes.clear();
        if ok.len() != ik.len() {
            return Err(Some(("the outer and inner keys of Join must have the same number of values".into(), args[2].span)));
        }
        let mut on = Vec::new();
        for (i, (a, b)) in ok.iter().zip(&ik).enumerate() {
            self.orm_unify(a, b, args[2].span)?;
            if i > 0 {
                on.push(S::t(" AND "));
            }
            on.push(seq![&a.s, " = ", &b.s]);
        }
        let join = seq![" JOIN ", quote(&um.table), " AS t1 ON ", S::Seq(on)];
        let body = self.orm_lambda(&args[3], vec![Bind::Ent(m.clone(), "t0".into()), Bind::Ent(um.clone(), "t1".into())], "Join (result)", cx)?;
        // the join fragment shares the argument list with the selection
        let pre = vec![self.orm_str(&join.render(true), span), self.orm_str(&join.render(false), span)];
        // arguments of the keys go with the join fragment
        let jargs = std::mem::take(&mut cx.args);
        let ja = self.orm_args(jargs, span);
        let mut pre = pre;
        pre.push(ja);
        self.orm_projection(base, "join_", body, pre, cx, span)
    }

    /// `ExecuteUpdate((s) => s.Set((u) => u.Name, "x").Set((u) => u.N, (u) => u.N + 1))`.
    fn orm_setters(&mut self, arg: &ast::Expr, m: &Rc<EntModel>, cx: &mut Cx) -> Tr<S> {
        let ast::ExprKind::Lambda { params, body, .. } = &expr::strip_parens(arg).kind else {
            return Err(Some(("ExecuteUpdate needs a lambda: ExecuteUpdate((s) => s.Set((u) => u.Name, value))".into(), arg.span)));
        };
        let (Some(p), Some(body)) = (params.first(), lambda_body(body)) else {
            return Err(Some(("ExecuteUpdate needs a lambda with one parameter: (s) => s.Set(...)".into(), arg.span)));
        };
        let mut sets = Vec::new();
        let mut cur = expr::strip_parens(body);
        loop {
            match &cur.kind {
                ast::ExprKind::Call { callee, args } => {
                    let ast::ExprKind::Field { base, name } = &expr::strip_parens(callee).kind else {
                        return Err(Some(("expected s.Set(column, value)".into(), cur.span)));
                    };
                    if name.name != "Set" || args.len() != 2 {
                        return Err(Some((format!("expected s.Set((u) => u.Column, value), found {}", name.name), name.span)));
                    }
                    sets.push((&args[0], &args[1]));
                    cur = expr::strip_parens(base);
                }
                ast::ExprKind::Ident(id) if id.name == p.name.name => break,
                _ => return Err(Some(("expected s.Set(column, value) calls on the lambda parameter".into(), cur.span))),
            }
        }
        sets.reverse();
        let mut parts = Vec::new();
        for (i, (col, val)) in sets.into_iter().enumerate() {
            let fs = super::orm_model::lambda_fields(col);
            let [f] = fs.as_slice() else { return Err(Some(("the first argument of Set selects one column: (u) => u.Name".into(), col.span))) };
            let Some(c) = m.col(f).cloned() else { return Err(Some((format!("{} has no column {f}", m.name), col.span))) };
            if c.key {
                return Err(Some((format!("the key {} cannot be updated", c.field), col.span)));
            }
            let v = if let ast::ExprKind::Lambda { .. } = &expr::strip_parens(val).kind {
                let depth = cx.scopes.len();
                let b = self.orm_lambda(val, vec![Bind::Ent(m.clone(), "t0".into())], "Set", cx)?;
                let v = self.orm_value(b, cx)?;
                cx.scopes.truncate(depth);
                v
            } else {
                self.orm_value(val, cx)?
            };
            let cv = V { s: S::t(""), ty: Some(c.ty), lit: Lit::No, kind: VK::Scalar };
            if v.lit != Lit::Nil {
                self.orm_unify(&cv, &v, val.span)?;
            } else if !c.nullable {
                return Err(Some((format!("{} is not nullable", c.field), val.span)));
            }
            if i > 0 {
                parts.push(S::t(", "));
            }
            parts.push(seq![quote(&c.name), " = ", v.s]);
        }
        if parts.is_empty() {
            return Err(Some(("ExecuteUpdate needs at least one s.Set(...)".into(), arg.span)));
        }
        Ok(S::Seq(parts))
    }

    // -- groups -----------------------------------------------------------------------

    fn orm_group_call(&mut self, base: hir::Expr, t: TypeId, k: TypeId, n: &str, args: &[ast::Expr], span: Span) -> Result<hir::Expr, hir::Expr> {
        if !matches!(n, "Select" | "Where" | "OrderBy" | "OrderByDescending" | "ThenBy" | "ThenByDescending") {
            if matches!(n, "ToList" | "First" | "Count" | "ToSql") {
                let e = Some((format!("{n} on a GroupBy: select the group values first, e.g. .Select((g) => (g.Key, g.Count()))"), span));
                self.check_args_loose(args);
                return Ok(self.orm_fail_expr(e, span));
            }
            return Err(base);
        }
        if args.len() != 1 {
            return Err(base);
        }
        let Some(m) = self.orm_model(t) else { return Ok(self.invalid(span)) };
        let ktys: Vec<TypeId> = match self.tt.get(k) {
            Type::Tuple(ts) => ts.clone(),
            _ => vec![k],
        };
        let keys: Vec<V> = ktys.iter().enumerate().map(|(i, t)| V { s: S::t(format!("{{#{i}}}")), ty: Some(*t), lit: Lit::No, kind: VK::Scalar }).collect();
        let mut cx = Cx::new();
        let bind = vec![Bind::Group(keys, k, m.clone(), "t0".into())];
        let r: Tr<hir::Expr> = (|| -> Tr<hir::Expr> {
            match n {
                "Select" => {
                    let body = self.orm_lambda(&args[0], bind, "Select", &mut cx)?;
                    self.orm_projection(base, "select_", body, Vec::new(), &mut cx, span)
                }
                "Where" => {
                    let body = self.orm_lambda(&args[0], bind, "Where", &mut cx)?;
                    let p = self.orm_pred(body, &mut cx)?;
                    let fa = self.orm_frag(&p, std::mem::take(&mut cx.args), span);
                    Ok(self.orm_mcall(base, "having_", vec![], fa, span))
                }
                _ => {
                    let body = self.orm_lambda(&args[0], bind, n, &mut cx)?;
                    let s = self.orm_order_keys(body, n.ends_with("Descending"), &mut cx)?;
                    let mut fa = self.orm_frag(&s, std::mem::take(&mut cx.args), span);
                    fa.push(self.orm_bool(n.starts_with("Then"), span));
                    Ok(self.orm_mcall(base, "order_", vec![], fa, span))
                }
            }
        })();
        Ok(match r {
            Ok(e) => e,
            Err(e) => self.orm_fail_expr(e, span),
        })
    }

    // -- navigations --------------------------------------------------------------------

    /// The split loader of navigation `n` of `owner` (an async func literal
    /// `(st, parents, tracking)`), with `__e1` the target's model in scope.
    fn orm_loader_src(&mut self, owner: &EntModel, n: &NavModel, target: &EntModel) -> Tr<String> {
        let need = |m: &EntModel| -> Tr<String> {
            m.key_col().map(|c| c.field.clone()).ok_or_else(|| Some((format!("{} needs a key to load navigations", m.name), Span::DUMMY)))
        };
        let assign = |k: &str| -> String {
            let v = if n.ptr { k.to_string() } else { format!("*{k}") };
            if n.many {
                format!("__p.{f} = append(__p.{f}, {v})", f = n.field)
            } else {
                format!("__p.{} = {v}", n.field)
            }
        };
        let reset = if n.many {
            format!("__p.{} = []", n.field)
        } else if n.ptr {
            format!("__p.{} = nil", n.field)
        } else {
            String::new()
        };
        let head = "async func(__st *ctxState, __ps []*T0, __tr bool) void! {\n";
        let (pkey, col, kidkey) = match n.kind {
            NavKind::Many | NavKind::One => {
                let pk = need(owner)?;
                let fk = target.col(&n.fk).ok_or_else(|| Some((format!("{} has no column {}", target.name, n.fk), Span::DUMMY)))?;
                (pk, fk.name.clone(), n.fk.clone())
            }
            NavKind::Belongs => {
                let tk = target.key_col().ok_or_else(|| Some((format!("{} needs a key", target.name), Span::DUMMY)))?;
                (n.fk.clone(), tk.name.clone(), tk.field.clone())
            }
            NavKind::ManyMany => {
                let pk = need(owner)?;
                let src = format!(
                    "{head}        var __keys []any = []\n        for __p in __ps {{\n            __keys = append(__keys, database.sql.argOf(__p.{pk}))\n        }}\n        var __r = await loadThrough(__st, __e1, {}, {}, {}, __keys, __tr)?\n        for __p in __ps {{\n            var __pk = keyText(database.sql.argOf(__p.{pk}))\n            {reset}\n            for __i, __k in __r.items {{\n                if __r.owners[__i] == __pk {{\n                    {}\n                }}\n            }}\n        }}\n    }}",
                    lit(&n.through),
                    lit(&n.owner_col),
                    lit(&n.target_col),
                    assign("__k")
                );
                return Ok(src);
            }
        };
        Ok(format!(
            "{head}        var __keys []any = []\n        for __p in __ps {{\n            __keys = append(__keys, database.sql.argOf(__p.{pkey}))\n        }}\n        var __kids = await loadIn(__st, __e1, {}, __keys, __tr)?\n        for __p in __ps {{\n            var __pk = keyText(database.sql.argOf(__p.{pkey}))\n            {reset}\n            for __k in __kids {{\n                if keyText(database.sql.argOf(__k.{kidkey})) == __pk {{\n                    {}\n                }}\n            }}\n        }}\n    }}",
            lit(&col),
            assign("__k")
        ))
    }

    /// `Include<T0>{...}` for navigation `n` of `m`.
    fn orm_include(&mut self, m: &Rc<EntModel>, n: &NavModel, span: Span) -> Tr<hir::Expr> {
        let Some(target) = self.orm_model(n.target) else { return Err(None) };
        let Some(tk) = target.key else { return Err(Some((format!("Include({}) needs a key on {}", n.field, target.name), span))) };
        let a = format!("i_{}", super::orm_model::snake(&n.field));
        let tq = quote(&target.table);
        let join = match n.kind {
            NavKind::Many | NavKind::One => {
                let (Some(fk), Some(pk)) = (target.col(&n.fk), m.key_col()) else { return Err(Some(("the navigation has no foreign key".into(), span))) };
                format!(" LEFT JOIN {tq} AS {a} ON {a}.{} = t0.{}", quote(&fk.name), quote(&pk.name))
            }
            NavKind::Belongs => {
                let Some(fk) = m.col(&n.fk) else { return Err(Some(("the navigation has no foreign key".into(), span))) };
                format!(" LEFT JOIN {tq} AS {a} ON {a}.{} = t0.{}", quote(&target.cols[tk].name), quote(&fk.name))
            }
            NavKind::ManyMany => {
                let Some(pk) = m.key_col() else { return Err(Some(("the navigation needs keys".into(), span))) };
                let j = format!("j_{}", super::orm_model::snake(&n.field));
                format!(
                    " LEFT JOIN {} AS {j} ON {j}.{} = t0.{} LEFT JOIN {tq} AS {a} ON {a}.{} = {j}.{}",
                    quote(&n.through),
                    quote(&n.owner_col),
                    quote(&pk.name),
                    quote(&target.cols[tk].name),
                    quote(&n.target_col)
                )
            }
        };
        let cols: Vec<String> = target.cols.iter().map(|c| format!("{a}.{}", quote(&c.name))).collect();
        let loader = self.orm_loader_src(m, n, &target).map_err(|e| e.map(|(w, _)| (w, span)))?;
        let v = if n.ptr { "__k".to_string() } else { "*__k".to_string() };
        let attach = if n.many { format!("__p.{f} = append(__p.{f}, {v})", f = n.field) } else { format!("__p.{} = {v}", n.field) };
        let reset = if n.many {
            format!("__p.{} = []", n.field)
        } else if n.ptr {
            format!("__p.{} = nil", n.field)
        } else {
            String::new()
        };
        let name = self.orm_name("ormInclude");
        let src = format!(
            "namespace data\n\nfunc {name}<T0, T1>() Include<T0> {{\n    var __e1 = modelOf<T1>()\n    return Include<T0>{{\n        name: {},\n        load: {loader},\n        join: Frag{{pg: {j}, lite: {j}, args: []}},\n        cols: {},\n        ncols: {},\n        childKey: {tk},\n        attach: func(__st *ctxState, __p *T0, __v []any, __o int, __tr bool) void! {{\n            var __k = materialize(__st, __e1, __v, __o, __tr)?\n            {attach}\n        }},\n        reset: func(__p *T0) {{\n            {reset}\n        }},\n    }}\n}}\n",
            lit(&n.field),
            lit(&cols.join(", ")),
            target.cols.len(),
            j = lit(&join),
        );
        Ok(self.orm_call(src, vec![m.ty, n.target], vec![], span))
    }

    /// `ctx.Entry(u).Collection("Orders")` / `.Reference("User")`: a NavLoader.
    fn orm_entry_nav(&mut self, base: hir::Expr, t: TypeId, n: &str, args: &[ast::Expr], span: Span) -> hir::Expr {
        let name = match args {
            [a] => match &expr::strip_parens(a).kind {
                ast::ExprKind::Str(s) => s.clone(),
                _ => {
                    self.err(format!("{n} needs the navigation name as a string constant: {n}(\"Orders\")"), a.span);
                    self.check_args_loose(args);
                    return self.invalid(span);
                }
            },
            _ => {
                self.err(format!("{n} takes the navigation name: {n}(\"Orders\")"), span);
                self.check_args_loose(args);
                return self.invalid(span);
            }
        };
        let Some(m) = self.orm_model(t) else { return self.invalid(span) };
        let Some(nav) = m.nav(&name).cloned() else {
            let navs: Vec<String> = m.navs.iter().map(|x| x.field.clone()).collect();
            self.err(format!("{} has no navigation {name} (navigations: {})", m.name, if navs.is_empty() { "none".to_string() } else { navs.join(", ") }), args[0].span);
            return self.invalid(span);
        };
        if (n == "Collection") != nav.many {
            let other = if nav.many { "Collection" } else { "Reference" };
            self.err(format!("{}.{name} is {}: use {other}(\"{name}\")", m.name, if nav.many { "a collection" } else { "a reference" }), args[0].span);
            return self.invalid(span);
        }
        let Some(target) = self.orm_model(nav.target) else { return self.invalid(span) };
        let loader = match self.orm_loader_src(&m, &nav, &target) {
            Ok(l) => l,
            Err(e) => {
                let e = e.map(|(w, _)| (w, span));
                return self.orm_fail_expr(e, span);
            }
        };
        let fname = self.orm_name("ormNav");
        let src = format!(
            "namespace data\n\nfunc {fname}<T0, T1>(__en Entry<T0>) NavLoader {{\n    var __e1 = modelOf<T1>()\n    var __st = __en.state()\n    var __p = __en.entity()\n    var __load = {loader}\n    return NavLoader{{load: async func() void! {{\n        await __load(__st, [__p], true)?\n    }}}}\n}}\n"
        );
        self.orm_call(src, vec![t, nav.target], vec![base], span)
    }

    /// `ctx.Users.UpsertBy(u, (x) => x.Email)`: the conflict columns.
    fn orm_upsert_by(&mut self, base: hir::Expr, t: TypeId, args: &[ast::Expr], span: Span) -> hir::Expr {
        let Some(m) = self.orm_model(t) else { return self.invalid(span) };
        let fields = match args {
            [_, l] => super::orm_model::lambda_fields(l),
            _ => Vec::new(),
        };
        if fields.is_empty() {
            self.err("UpsertBy takes the entity and a lambda selecting the conflict columns: UpsertBy(u, (x) => x.Email) or (x) => (x.A, x.B)", span);
            self.check_args_loose(args);
            return self.invalid(span);
        }
        let mut cols = Vec::new();
        for f in &fields {
            match m.col(f) {
                Some(c) => cols.push(c.name.clone()),
                None => {
                    self.err(format!("UpsertBy: {} has no column {f}", m.name), args[1].span);
                    return self.invalid(span);
                }
            }
        }
        let p = self.tt.pointer(t);
        let e = self.expr_to(&args[0], p, Ctx::Arg(0, "UpsertBy".into()));
        let ss = self.tt.slice(self.tt.string);
        let vs: Vec<hir::Expr> = cols.iter().map(|c| self.orm_str(c, span)).collect();
        let list = mk(ExprKind::SliceLit(vs), ss, span);
        self.orm_mcall(base, "upsertBy_", vec![], vec![e, list], span)
    }

    // -- model builder ------------------------------------------------------------------

    /// `m.Entity<User>().HasIndex((u) => u.Email)`: checks the lambda and
    /// calls the builder method with the field names (the model itself is
    /// read from the source of Configure).
    fn orm_fluent_call(&mut self, base: hir::Expr, t: TypeId, n: &str, args: &[ast::Expr], span: Span) -> hir::Expr {
        let method = match n {
            "HasKey" => "key_",
            "Ignore" => "ignore_",
            "Property" => "property_",
            "HasIndex" => "index_",
            _ => "nav_",
        };
        let fields = match args {
            [a] => super::orm_model::lambda_fields(a),
            _ => Vec::new(),
        };
        let multi = n == "HasIndex";
        if fields.is_empty() || (!multi && fields.len() != 1) {
            let want = if multi { "(u) => u.Email or (u) => (u.A, u.B)" } else { "(u) => u.Email" };
            self.err(format!("{n} needs a lambda selecting {}: {n}({want})", if multi { "fields" } else { "one field" }), span);
            self.check_args_loose(args);
            return self.invalid(span);
        }
        if let Some(sid) = self.struct_of(t) {
            for f in &fields {
                if !self.structs[sid as usize].fields.iter().any(|x| x.name == *f) {
                    let sn = self.structs[sid as usize].name.clone();
                    self.err(format!("{n}: {sn} has no field {f}"), args[0].span);
                    return self.invalid(span);
                }
            }
        }
        let ss = self.tt.slice(self.tt.string);
        let vs: Vec<hir::Expr> = fields.iter().map(|f| self.orm_str(f, span)).collect();
        let list = mk(ExprKind::SliceLit(vs), ss, span);
        self.orm_mcall(base, method, vec![], vec![list], span)
    }
}
