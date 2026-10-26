//! `data` end to end: the migrations CLI (`zell data migration add`,
//! `update`, `list`, `script`) on SQLite files, and the ORM and the CLI
//! against a real PostgreSQL server (the temporary cluster of
//! `support/pgcluster.rs`, shared with `database.rs`). The ORM fixture
//! `fixtures/data/orm_real.zl` runs the same scenario on SQLite and
//! PostgreSQL and must print `orm_real.out`. Without PostgreSQL binaries
//! the server test is skipped with a note (the SQLite parts always run;
//! the spec area `tests/spec/data` covers the ORM on SQLite).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "support/pgcluster.rs"]
mod pgcluster;
use pgcluster::repo;

fn zell() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_zell"));
    c.env("ZELL_NO_BACKTRACE", "1");
    c
}

fn ensure_runtime() {
    if !repo().join("runtime/libzell_rt.a").is_file() {
        assert!(Command::new("make").arg("-C").arg(repo().join("runtime")).status().unwrap().success());
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// `zell data <args>` in project `dir` (must succeed); returns stdout.
fn data(dir: &Path, args: &[&str], ts: &str) -> String {
    let o = zell().arg("data").args(args).arg("--project").arg(dir).env("ZELL_DATA_TIMESTAMP", ts).output().unwrap();
    assert!(o.status.success(), "zell data {args:?} failed:\n{}", text(&o));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

const MODEL_V1: &str = r#"namespace shop

import data
import io

struct User {
    Id    int
    @index(unique: true) Email string
    @maxLength(80) Name string
    Orders []*Order
}

struct Order {
    Id     int
    UserId int
    Total  float
}

struct ShopContext {
    Context
    Users  Set<User>
    Orders Set<Order>
}

async func main() void! {
    var ctx = Open<ShopContext>(Provider.Sqlite, ":memory:")?
    await ctx.Migrate()?
    ctx.Users.Add(new User{Email: "a@b.c", Name: "Ann"})
    _ = await ctx.SaveChanges()?
    println(await ctx.Users.Select((u) => u.Name).ToList()?, len(Migrations()))
}
"#;

fn project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zell_data_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.zl"), MODEL_V1).unwrap();
    dir
}

/// Writes two migrations into `dir` (Initial, then AddAge after changing the model).
fn two_migrations(dir: &Path) {
    let out = data(dir, &["migration", "add", "Initial"], "20260101000000");
    assert!(out.contains("20260101000000_Initial.zl"), "{out}");
    let init = std::fs::read_to_string(dir.join("migrations/20260101000000_Initial.zl")).unwrap();
    for want in [
        "namespace shop.migrations",
        "@migration(\"20260101000000_Initial\")",
        "struct Initial {",
        "m.CreateTable(\"users\", [",
        "Column{Name: \"id\", Type: \"int64\", Key: true, Auto: true},",
        "Column{Name: \"name\", Type: \"string\", MaxLength: 80},",
        "ForeignKey{Name: \"fk_orders_users_user_id\", Columns: [\"user_id\"], RefTable: \"users\", RefColumns: [\"id\"], OnDelete: \"CASCADE\"},",
        "m.CreateIndex(\"users\", \"ux_users_email\", [\"email\"], true)",
        "m.DropTable(\"orders\")\n        m.DropTable(\"users\")",
    ] {
        assert!(init.contains(want), "missing {want:?} in\n{init}");
    }
    assert!(dir.join("migrations/snapshot.json").is_file());
    let none = data(dir, &["migration", "add", "Nothing"], "20260101000001");
    assert!(none.contains("no changes"), "{none}");
    // change the model: a new column with an index, a longer name, a dropped column
    let v2 = MODEL_V1
        .replace("@maxLength(80) Name string", "@maxLength(120) Name string\n    @index Age *int")
        .replace("    Total  float\n", "    Total  float\n    Note   string\n");
    std::fs::write(dir.join("main.zl"), v2).unwrap();
    data(dir, &["migration", "add", "AddAge"], "20260102000000");
    let add = std::fs::read_to_string(dir.join("migrations/20260102000000_AddAge.zl")).unwrap();
    for want in [
        "m.AlterColumn(\"users\", Column{Name: \"name\", Type: \"string\", MaxLength: 120})",
        "m.AddColumn(\"users\", Column{Name: \"age\", Type: \"int64\", Nullable: true})",
        "m.AddColumn(\"orders\", Column{Name: \"note\", Type: \"string\"})",
        "m.CreateIndex(\"users\", \"ix_users_age\", [\"age\"], false)",
        "m.DropIndex(\"users\", \"ix_users_age\")",
        "m.DropColumn(\"users\", \"age\")",
        "m.AlterColumn(\"users\", Column{Name: \"name\", Type: \"string\", MaxLength: 80})",
    ] {
        assert!(add.contains(want), "missing {want:?} in\n{add}");
    }
    let dup = zell().arg("data").args(["migration", "add", "AddAge"]).arg("--project").arg(dir).output().unwrap();
    assert!(!dup.status.success() && text(&dup).contains("already exists"), "{}", text(&dup));
}

#[test]
fn migrations_cli_sqlite() {
    ensure_runtime();
    let dir = project("cli");
    two_migrations(&dir);
    let db = dir.join("app.db").to_string_lossy().to_string();
    let a = data(&dir, &["update", "--dsn", &db], "");
    assert_eq!(a, "applied 20260101000000_Initial\napplied 20260102000000_AddAge\n");
    assert_eq!(data(&dir, &["update", "--dsn", &db], ""), "database is up to date\n");
    assert_eq!(data(&dir, &["list", "--dsn", &db], ""), "20260101000000_Initial applied\n20260102000000_AddAge applied\n");
    assert_eq!(data(&dir, &["update", "Initial", "--dsn", &db], ""), "reverted 20260102000000_AddAge\n");
    assert_eq!(data(&dir, &["update", "0", "--dsn", &db], ""), "reverted 20260101000000_Initial\n");
    assert_eq!(data(&dir, &["update", "--dsn", &db], ""), "applied 20260101000000_Initial\napplied 20260102000000_AddAge\n");
    assert_eq!(data(&dir, &["migration", "list"], ""), "20260101000000_Initial\n20260102000000_AddAge\n");
    let script = data(&dir, &["script", "--provider", "postgres"], "");
    assert!(script.contains("-- 20260101000000_Initial\nCREATE TABLE \"users\" (\n    \"id\" bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,"), "{script}");
    assert!(script.contains("ALTER TABLE \"users\" ADD COLUMN \"age\" bigint;"), "{script}");
    // the program applies the same migrations itself (ctx.Migrate)
    let run = zell().arg("run").arg(&dir).arg("--leak-check").output().unwrap();
    assert!(run.status.success(), "{}", text(&run));
    assert_eq!(String::from_utf8_lossy(&run.stdout), "[Ann] 2\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn orm_real_postgres() {
    let Some(_) = pgcluster::available() else { return };
    ensure_runtime();
    let dir = std::env::temp_dir().join(format!("zell_data_pg_{}", std::process::id()));
    let cluster = pgcluster::start(&dir);
    let dsn = cluster.dsn();
    // the ORM scenario on both providers
    let src = repo().join("compiler/tests/fixtures/data/orm_real.zl");
    let exe = dir.join("orm_real");
    let b = zell().arg("build").arg(&src).arg("-o").arg(&exe).arg("--leak-check").output().unwrap();
    assert!(b.status.success(), "build failed:\n{}", text(&b));
    let run = Command::new(&exe).env("PGTEST_DSN", &dsn).env("ZELL_NO_BACKTRACE", "1").output().unwrap();
    assert!(run.status.success(), "program failed ({:?}):\n{}", run.status.code(), text(&run));
    let want = std::fs::read_to_string(repo().join("compiler/tests/fixtures/data/orm_real.out")).unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), want);
    // the migrations CLI on PostgreSQL
    let proj = project("pgcli");
    two_migrations(&proj);
    let up = data(&proj, &["update", "--provider", "postgres", "--dsn", &dsn], "");
    assert_eq!(up, "applied 20260101000000_Initial\napplied 20260102000000_AddAge\n");
    let cols = cluster.psql("select string_agg(column_name || ':' || data_type, ',' order by column_name) from information_schema.columns where table_name = 'users'");
    assert!(cols.status.success(), "{}", text(&cols));
    assert_eq!(data(&proj, &["update", "Initial", "--provider", "postgres", "--dsn", &dsn], ""), "reverted 20260102000000_AddAge\n");
    assert_eq!(data(&proj, &["list", "--provider", "postgres", "--dsn", &dsn], ""), "20260101000000_Initial applied\n20260102000000_AddAge pending\n");
    assert_eq!(data(&proj, &["update", "0", "--provider", "postgres", "--dsn", &dsn], ""), "reverted 20260101000000_Initial\n");
    let gone = cluster.psql("select 1 from users");
    assert!(!gone.status.success(), "the users table should be dropped");
    drop(cluster);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&proj);
}

/// The PostgreSQL-only features of `data` (`fixtures/data/pg_features.zl`)
/// on a real server: `PGTEST_DSN` if set (e.g. a throwaway container),
/// else the temporary cluster; skipped without either.
#[test]
fn pg_features() {
    ensure_runtime();
    let dir = std::env::temp_dir().join(format!("zell_data_pgf_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut cluster = None;
    let dsn = match std::env::var("PGTEST_DSN") {
        Ok(d) if !d.is_empty() => d,
        _ => {
            let Some(_) = pgcluster::available() else { return };
            let c = pgcluster::start(&dir);
            let d = c.dsn();
            cluster = Some(c);
            d
        }
    };
    let src = repo().join("compiler/tests/fixtures/data/pg_features.zl");
    let exe = dir.join("pg_features");
    let b = zell().arg("build").arg(&src).arg("-o").arg(&exe).arg("--leak-check").output().unwrap();
    assert!(b.status.success(), "build failed:\n{}", text(&b));
    let run = Command::new(&exe).env("PGTEST_DSN", &dsn).env("ZELL_NO_BACKTRACE", "1").env_remove("HTTP_PROXY").env_remove("HTTPS_PROXY").output().unwrap();
    assert!(run.status.success(), "program failed ({:?}):\n{}", run.status.code(), text(&run));
    let want = std::fs::read_to_string(repo().join("compiler/tests/fixtures/data/pg_features.out")).unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), want);
    drop(cluster);
    let _ = std::fs::remove_dir_all(&dir);
}
