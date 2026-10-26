//! `zell data ...`: migrations of `data` (docs/FRAMEWORK.md section 3).
//!
//! * `zell data migration add <Name>` builds the project with a generated
//!   `main` that prints the model of its contexts (`data.ModelJson<C>()`:
//!   attributes, conventions and `Configure`, built by the library), compares it with
//!   `migrations/snapshot.json` and writes `migrations/<timestamp>_<Name>.zl`:
//!   a struct with `@migration("<timestamp>_<Name>")` whose `Up`/`Down`
//!   methods call the `data.Migrator` API (`CreateTable`, `AddColumn`,
//!   `AlterColumn`, `CreateIndex`, `AddForeignKey`, ... and their inverses),
//!   then updates the snapshot.
//! * `zell data update [target]`, `zell data script` and `zell data list`
//!   build a small program from the migration files and a generated `main`
//!   that calls `data.RunMigrations` (the runtime migrator renders the DDL
//!   for the provider; applied migrations are recorded in `__migrations`),
//!   and run it.
//!
//! Provider and DSN come from `--provider`/`--dsn` or `ZELL_DATA_PROVIDER`
//! and `ZELL_DATA_DSN`.

use crate::diagnostics::Diagnostics;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const USAGE: &str = "\
usage: zell data <command> [options]

commands:
  migration add <Name>   compare the model with migrations/snapshot.json and write
                         migrations/<timestamp>_<Name>.zl (Up/Down) and the new snapshot
  migration list         list the migration files
  update [target]        apply the pending migrations (or migrate up/down to target; 0 = none)
  script                 print the SQL of all migrations for the provider
  list                   show which migrations are applied

options:
  --project <dir>        the project directory (default: .)
  --dir <dir>            the migrations directory (default: <project>/migrations)
  --provider <p>         postgres or sqlite (default: $ZELL_DATA_PROVIDER, else sqlite)
  --dsn <dsn>            data source name (default: $ZELL_DATA_DSN)
  --context <Name>       the context whose namespace the migrations get (default: the first)";

struct Opts {
    project: PathBuf,
    dir: Option<PathBuf>,
    provider: String,
    dsn: String,
    context: Option<String>,
    rest: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        project: PathBuf::from("."),
        dir: None,
        provider: std::env::var("ZELL_DATA_PROVIDER").unwrap_or_else(|_| "sqlite".into()),
        dsn: std::env::var("ZELL_DATA_DSN").unwrap_or_default(),
        context: None,
        rest: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let mut val = |name: &str| -> Result<String, String> {
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--project" => o.project = PathBuf::from(val("--project")?),
            "--dir" => o.dir = Some(PathBuf::from(val("--dir")?)),
            "--provider" => o.provider = val("--provider")?,
            "--dsn" => o.dsn = val("--dsn")?,
            "--context" => o.context = Some(val("--context")?),
            x if x.starts_with("--") => return Err(format!("unknown option {x}\n\n{USAGE}")),
            _ => o.rest.push(a.clone()),
        }
        i += 1;
    }
    Ok(o)
}

impl Opts {
    fn mig_dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(|| self.project.join("migrations"))
    }
}

/// Runs `zell data <args>`; returns the exit code.
pub fn run(args: &[String]) -> Result<i32, String> {
    let o = parse(args)?;
    let cmd: Vec<&str> = o.rest.iter().map(|s| s.as_str()).collect();
    match cmd.as_slice() {
        ["migration", "add", name] => migration_add(&o, name).map(|_| 0),
        ["migration", "add"] => Err("migration add needs a name: zell data migration add AddUsers".into()),
        ["migration", "list"] | ["migrations"] => {
            for (id, _) in migration_files(&o.mig_dir())? {
                println!("{id}");
            }
            Ok(0)
        }
        ["update"] => run_migrations(&o, "update", ""),
        ["update", target] => run_migrations(&o, "update", target),
        ["script"] => run_migrations(&o, "script", ""),
        ["list"] => run_migrations(&o, "list", ""),
        [] | ["help"] => {
            println!("{USAGE}");
            Ok(0)
        }
        _ => Err(format!("unknown data command `{}`\n\n{USAGE}", o.rest.join(" "))),
    }
}

// -- model --------------------------------------------------------------------------

/// The model of the project's contexts: the project is built with a
/// generated `main` (its own `main` renamed, as `zell test` does) that
/// prints `data.ModelJson<C>()` of every context (a struct embedding
/// `data.Context`), and run. The model is built by the `data` library from
/// reflection and the contexts' `Configure`, exactly as at run time.
pub fn project_model(project: &Path) -> Result<Value, String> {
    let paths = crate::driver::collect_files(project).map_err(|e| format!("cannot read {}: {e}", project.display()))?;
    if paths.is_empty() {
        return Err(format!("no .zl files in {}", project.display()));
    }
    let files = crate::driver::load(&paths).map_err(|e| format!("cannot read source: {e}"))?;
    let mut diags = Diagnostics::new();
    let Some(asts) = crate::driver::parse_all(&files, &mut diags) else {
        eprint!("{}", diags.render_all_files(&files));
        return Err("compilation failed".into());
    };
    let mut contexts = Vec::new();
    let mut sources = Vec::new();
    for (f, ast) in files.iter().zip(&asts) {
        let ns = ast.namespace.to_string();
        let imports_data = ast.imports.iter().any(|i| i.path.to_string() == "data");
        let mut mains = Vec::new();
        for d in &ast.decls {
            match d {
                crate::ast::Decl::Struct(st) => {
                    let ctx = st.embeds.iter().any(|t| match &t.kind {
                        crate::ast::TypeKind::Named { path, .. } => {
                            let p = path.to_string();
                            p == "data.Context" || (p == "Context" && imports_data)
                        }
                        _ => false,
                    });
                    if ctx && st.generics.is_empty() {
                        contexts.push(if ns.is_empty() { st.name.name.clone() } else { format!("{ns}.{}", st.name.name) });
                    }
                }
                crate::ast::Decl::Func(func) if func.name.name == "main" => mains.push((func.name.span.start as usize, func.name.span.end as usize)),
                _ => {}
            }
        }
        let mut text = f.text.clone();
        for (s, e) in mains.into_iter().rev() {
            text.replace_range(s..e, "zellUserMain__");
        }
        sources.push(crate::span::SourceFile::new(f.path.clone(), text));
    }
    if contexts.is_empty() {
        return Err("the project has no context (a struct embedding data.Context with data.Set<T> fields)".into());
    }
    let mut main = String::from("namespace zelldatamodel__\n\nimport data\nimport io\n\nfunc main() {\n");
    for c in &contexts {
        main.push_str(&format!("    println(data.ModelJson<{c}>())\n"));
    }
    main.push_str("}\n");
    sources.push(crate::span::SourceFile::new(project.join("<zell data model>.zl").display().to_string(), main));
    let tmp = std::env::temp_dir().join(format!("zell_data_model_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let exe = tmp.join("model");
    let opts = crate::build::BuildOptions::new(project);
    let out = crate::build::build_sources(sources, &exe, &opts).and_then(|exe| std::process::Command::new(&exe).output().map_err(|e| format!("cannot run {}: {e}", exe.display())));
    let _ = std::fs::remove_dir_all(&tmp);
    let out = out?;
    if !out.status.success() {
        return Err(format!("building the model failed:\n{}", String::from_utf8_lossy(&out.stderr)));
    }
    // one JSON object per context: {"name", "namespace", "sets", "tables"} or {"error"}
    let mut ctxs = Vec::new();
    let mut all: Vec<Value> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).map_err(|e| format!("internal error: model JSON: {e}"))?;
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            return Err(e.to_string());
        }
        for t in arr(&v, "tables") {
            if !all.iter().any(|x| s(x, "name") == s(&t, "name")) {
                all.push(t);
            }
        }
        ctxs.push(json!({"name": v["name"].clone(), "namespace": v["namespace"].clone(), "sets": v["sets"].clone()}));
    }
    Ok(json!({"version": 1, "contexts": ctxs, "tables": all}))
}

fn tables(v: &Value) -> Vec<Value> {
    v.get("tables").and_then(|t| t.as_array()).cloned().unwrap_or_default()
}

fn find<'v>(ts: &'v [Value], name: &str) -> Option<&'v Value> {
    ts.iter().find(|t| t["name"].as_str() == Some(name))
}

fn arr(v: &Value, k: &str) -> Vec<Value> {
    v.get(k).and_then(|x| x.as_array()).cloned().unwrap_or_default()
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

// -- code generation ----------------------------------------------------------------

/// A Zell string literal.
fn lit(x: &str) -> String {
    let mut out = String::from("\"");
    for c in x.chars() {
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

fn lits(v: &Value) -> String {
    let xs: Vec<String> = v.as_array().map(|a| a.iter().map(|x| lit(x.as_str().unwrap_or(""))).collect()).unwrap_or_default();
    format!("[{}]", xs.join(", "))
}

/// `Column{...}` with only the fields that differ from the zero value.
fn column_src(c: &Value) -> String {
    let mut f = vec![format!("Name: {}", lit(&s(c, "name"))), format!("Type: {}", lit(&s(c, "type")))];
    if !s(c, "sqlType").is_empty() {
        f.push(format!("SqlType: {}", lit(&s(c, "sqlType"))));
    }
    for k in ["nullable", "key", "auto"] {
        if c[k].as_bool() == Some(true) {
            let name = format!("{}{}", k[..1].to_uppercase(), &k[1..]);
            f.push(format!("{name}: true"));
        }
    }
    if let Some(n) = c["maxLength"].as_i64().filter(|n| *n > 0) {
        f.push(format!("MaxLength: {n}"));
    }
    if !s(c, "default").is_empty() {
        f.push(format!("Default: {}", lit(&s(c, "default"))));
    }
    format!("Column{{{}}}", f.join(", "))
}

fn fk_src(fk: &Value) -> String {
    format!(
        "ForeignKey{{Name: {}, Columns: {}, RefTable: {}, RefColumns: {}, OnDelete: {}}}",
        lit(&s(fk, "name")),
        lits(&fk["columns"]),
        lit(&s(fk, "refTable")),
        lits(&fk["refColumns"]),
        lit(&s(fk, "onDelete"))
    )
}

fn create_table(t: &Value) -> String {
    let cols: Vec<String> = arr(t, "columns").iter().map(|c| format!("            {},\n", column_src(c))).collect();
    let fks: Vec<String> = arr(t, "foreignKeys").iter().map(|f| format!("            {},\n", fk_src(f))).collect();
    let fk = if fks.is_empty() { "[]".to_string() } else { format!("[\n{}        ]", fks.concat()) };
    format!("        m.CreateTable({}, [\n{}        ], {fk})\n", lit(&s(t, "name")), cols.concat())
}

fn create_index(table: &str, ix: &Value) -> String {
    format!("        m.CreateIndex({}, {}, {}, {})\n", lit(table), lit(&s(ix, "name")), lits(&ix["columns"]), ix["unique"].as_bool().unwrap_or(false))
}

fn same_col(a: &Value, b: &Value) -> bool {
    ["type", "sqlType", "nullable", "key", "auto", "maxLength", "default"].iter().all(|k| a.get(*k) == b.get(*k))
}

/// The Up and Down statements from `old` to `new`.
pub fn diff(old: &Value, new: &Value) -> (String, String) {
    let (ot, nt) = (tables(old), tables(new));
    let mut up = String::new();
    let mut down: Vec<String> = Vec::new();
    let mut up_idx = String::new();
    // dropped tables (dependents first)
    for t in ot.iter().rev() {
        let name = s(t, "name");
        if find(&nt, &name).is_none() {
            up.push_str(&format!("        m.DropTable({})\n", lit(&name)));
            let mut d = create_table(t);
            for ix in arr(t, "indexes") {
                d.push_str(&create_index(&name, &ix));
            }
            down.push(d);
        }
    }
    for t in &nt {
        let name = s(t, "name");
        match find(&ot, &name) {
            None => {
                up.push_str(&create_table(t));
                for ix in arr(t, "indexes") {
                    up_idx.push_str(&create_index(&name, &ix));
                }
                down.push(format!("        m.DropTable({})\n", lit(&name)));
            }
            Some(o) => {
                let (oc, nc) = (arr(o, "columns"), arr(t, "columns"));
                // foreign keys and indexes removed
                for fk in arr(o, "foreignKeys") {
                    if !arr(t, "foreignKeys").iter().any(|x| x == &fk) {
                        up.push_str(&format!("        m.DropForeignKey({}, {})\n", lit(&name), lit(&s(&fk, "name"))));
                        down.push(format!("        m.AddForeignKey({}, {})\n", lit(&name), fk_src(&fk)));
                    }
                }
                for ix in arr(o, "indexes") {
                    if !arr(t, "indexes").iter().any(|x| x == &ix) {
                        up.push_str(&format!("        m.DropIndex({}, {})\n", lit(&name), lit(&s(&ix, "name"))));
                        down.push(create_index(&name, &ix));
                    }
                }
                for c in &oc {
                    let cn = s(c, "name");
                    if !nc.iter().any(|x| s(x, "name") == cn) {
                        up.push_str(&format!("        m.DropColumn({}, {})\n", lit(&name), lit(&cn)));
                        down.push(format!("        m.AddColumn({}, {})\n", lit(&name), column_src(c)));
                    }
                }
                for c in &nc {
                    let cn = s(c, "name");
                    match oc.iter().find(|x| s(x, "name") == cn) {
                        None => {
                            up.push_str(&format!("        m.AddColumn({}, {})\n", lit(&name), column_src(c)));
                            down.push(format!("        m.DropColumn({}, {})\n", lit(&name), lit(&cn)));
                        }
                        Some(x) if !same_col(x, c) => {
                            up.push_str(&format!("        m.AlterColumn({}, {})\n", lit(&name), column_src(c)));
                            down.push(format!("        m.AlterColumn({}, {})\n", lit(&name), column_src(x)));
                        }
                        _ => {}
                    }
                }
                for ix in arr(t, "indexes") {
                    if !arr(o, "indexes").iter().any(|x| x == &ix) {
                        up_idx.push_str(&create_index(&name, &ix));
                        down.push(format!("        m.DropIndex({}, {})\n", lit(&name), lit(&s(&ix, "name"))));
                    }
                }
                for fk in arr(t, "foreignKeys") {
                    if !arr(o, "foreignKeys").iter().any(|x| x == &fk) {
                        up_idx.push_str(&format!("        m.AddForeignKey({}, {})\n", lit(&name), fk_src(&fk)));
                        down.push(format!("        m.DropForeignKey({}, {})\n", lit(&name), lit(&s(&fk, "name"))));
                    }
                }
            }
        }
    }
    up.push_str(&up_idx);
    down.reverse();
    (up, down.concat())
}

/// UTC `YYYYMMDDHHMMSS` (or `$ZELL_DATA_TIMESTAMP`).
fn timestamp() -> String {
    if let Ok(t) = std::env::var("ZELL_DATA_TIMESTAMP") {
        return t;
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The migration files of `dir`: (id, path), by id.
fn migration_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(out) };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "zl") {
            let stem = p.file_stem().unwrap().to_string_lossy().to_string();
            if stem.len() > 15 && stem.as_bytes()[14] == b'_' && stem[..14].chars().all(|c| c.is_ascii_digit()) {
                out.push((stem, p));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn migration_add(o: &Opts, name: &str) -> Result<(), String> {
    if name.is_empty() || !name.chars().next().unwrap().is_ascii_uppercase() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("migration name `{name}` must be a capitalised identifier (AddUsers)"));
    }
    let dir = o.mig_dir();
    let existing = migration_files(&dir)?;
    if existing.iter().any(|(id, _)| id[15..] == *name) {
        return Err(format!("a migration named {name} already exists"));
    }
    let model = project_model(&o.project)?;
    let ctxs = arr(&model, "contexts");
    let ctx = match &o.context {
        Some(c) => ctxs.iter().find(|x| s(x, "name") == *c).ok_or_else(|| format!("no context named {c}"))?,
        None => ctxs.first().ok_or("the project has no context (a struct embedding data.Context with data.Set<T> fields)")?,
    };
    let ns = s(ctx, "namespace");
    let ns = if ns.is_empty() { "migrations".to_string() } else { format!("{ns}.migrations") };
    let snap_path = dir.join("snapshot.json");
    let old: Value = match std::fs::read_to_string(&snap_path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {e}", snap_path.display()))?,
        Err(_) => json!({"version": 1, "tables": []}),
    };
    let snapshot = json!({"version": 1, "tables": model["tables"].clone()});
    let (up, down) = diff(&old, &snapshot);
    if up.is_empty() {
        println!("no changes: the model matches {}", snap_path.display());
        return Ok(());
    }
    let id = format!("{}_{name}", timestamp());
    let src = format!(
        "// Generated by `zell data migration add {name}` (data). Migrations\n// are ordinary code: edit them as needed; the model they lead to is\n// in snapshot.json next to this file.\nnamespace {ns}\n\nimport data\n\n@migration({})\nstruct {name} {{\n    func Up(m *Migrator) {{\n{up}    }}\n\n    func Down(m *Migrator) {{\n{down}    }}\n}}\n",
        lit(&id)
    );
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let file = dir.join(format!("{id}.zl"));
    std::fs::write(&file, src).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    std::fs::write(&snap_path, serde_json::to_string_pretty(&snapshot).unwrap() + "\n").map_err(|e| format!("cannot write {}: {e}", snap_path.display()))?;
    println!("created {}", file.display());
    Ok(())
}

// -- applying -----------------------------------------------------------------------

fn run_migrations(o: &Opts, command: &str, target: &str) -> Result<i32, String> {
    let dir = o.mig_dir();
    let files = migration_files(&dir)?;
    let mut sources = Vec::new();
    for (_, p) in &files {
        let text = std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        sources.push(crate::span::SourceFile::new(p.display().to_string(), text));
    }
    if command != "script" && o.dsn.is_empty() {
        return Err("no data source: pass --dsn (or set ZELL_DATA_DSN)".into());
    }
    let main = format!(
        "namespace zelldatacli\n\nimport data\n\nasync func main() void! {{\n    var p = ParseProvider({})?\n    await RunMigrations(p, {}, Migrations(), {}, {})?\n}}\n",
        lit(&o.provider),
        lit(&o.dsn),
        lit(command),
        lit(target)
    );
    sources.push(crate::span::SourceFile::new(dir.join("<zell data>.zl").display().to_string(), main));
    let tmp = std::env::temp_dir().join(format!("zell_data_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let exe = tmp.join("migrate");
    let opts = crate::build::BuildOptions::new(&dir);
    let r = crate::build::build_sources(sources, &exe, &opts);
    let r = r.and_then(|exe| std::process::Command::new(&exe).status().map_err(|e| format!("cannot run {}: {e}", exe.display())));
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(r?.code().unwrap_or(1))
}
