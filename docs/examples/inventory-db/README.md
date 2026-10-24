# inventory-db: a database with database.sql and SQLite

A small stock-keeping program on SQLite: products, stock movements and
orders that take several products out of stock at once. It creates the
database file, brings the schema up to date with migrations, runs a few
orders (one of which must be rolled back), prints a report computed in
SQL, and then opens the file a second time to show that the data and the
schema version persisted.

What it shows:

- `sqlite.Open`, `Ping` at start-up, pool settings, `pragma`s;
- migrations: an append-only list of statements and a `schema_version`
  table, each migration in its own transaction;
- typed queries: `Query<T>` and `QueryOne<T>` into structs (columns
  matched to fields by name, `@sql.column` for exceptions), into plain
  types (`Query<string>`, `QueryOne<int>`), named and positional
  parameters;
- transactions both ways: `db.Transact(func)` and the explicit
  `Begin` / `defer tx.Rollback()` / `Commit`;
- turning `sql.ErrNoRows` into a domain error, and a typed error
  (`OutOfStock`) that carries data to the caller;
- raw string literals (backquotes) for multi-line SQL;
- a type with a destructor (`Drop`) that cleans up a temporary file.

## Run it

```sh
cd docs/examples/inventory-db
zell run
zell test
```

The SQLite driver uses the system `libsqlite3` (`sqlite-devel` or
`libsqlite3-dev`). The database lives in the temporary directory and is
removed at the end; the tests use in-memory databases.

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `inventory`) |
| `src/main.zl` | the demo: two sessions on one database file |
| `src/db.zl` | `inventory.db`: opening, migrations |
| `src/repo.zl` | `inventory.repo`: products, stock movements, orders |
| `src/report.zl` | `inventory.report`: aggregate queries and their output |
| `src/repo_test.zl` | tests (`zell test`) |

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "inventory-db"
version = "0.1.0"
namespace = "inventory"
entry = "src/main.zl"
```

## src/db.zl: opening and migrating

`sqlite.Open` does not touch the file; connections are opened when first
needed. `Ping` forces one, so a wrong path fails at start-up with a clear
message instead of on the first query. The pool is limited to one
connection: SQLite allows one writer at a time anyway, and per-connection
settings such as `foreign_keys` then apply to everything.

`Migrate` reads the current version, applies the missing migrations in
order, and records each new version in the same transaction as the
migration itself. If a migration fails, its transaction rolls back and
the database stays at the last good version. The list in `migrations` is
append-only: a migration that has been released is never edited, a
change is a new entry. Backquoted strings are raw and may span lines,
which suits SQL.

<!-- file: src/db.zl -->
```zell title="src/db.zl"
// Opening the database and bringing its schema up to date.
namespace inventory.db

import database

// Migrations are applied in order, each at most once. The list only ever
// grows: a released migration is never edited, a change is a new entry.
// Version numbers are the positions in this list, starting at 1.
const migrations = [
    `create table products (
        id          integer primary key,
        sku         text not null unique,
        name        text not null,
        category    text not null,
        price_cents integer not null check (price_cents >= 0),
        qty         integer not null default 0 check (qty >= 0)
    )`,
    `create table movements (
        id         integer primary key,
        product_id integer not null references products(id),
        delta      integer not null,
        reason     text not null
    )`,
    "create index movements_product on movements(product_id)",
]

// Open opens (or creates) the SQLite file at path. sqlite.Open never
// fails by itself, connections are opened lazily, so Ping makes a bad
// path fail here, at start-up, rather than on the first query.
async func Open(path string) *sql.DB! {
    var db = sqlite.Open(path)
    await db.Ping()?("open " + fs.Base(path))
    // SQLite enforces foreign keys only when asked, per connection; one
    // connection makes that setting (and the file lock) simple.
    db.SetMaxOpenConns(1)
    _ = await db.Exec("pragma foreign_keys = on", [])?
    return db
}

// Migrate applies the migrations the database has not seen and returns
// how many it applied. Each one runs in its own transaction together with
// the version bump, so a failing migration leaves the schema at the last
// good version instead of half-changed.
async func Migrate(db *sql.DB) int! {
    _ = await db.Exec("create table if not exists schema_version (version integer not null)", [])?
    var current = await db.QueryOne<int>("select coalesce(max(version), 0) from schema_version", [

    ])?
    var applied = 0
    for i, stmt in migrations {
        var version = i + 1
        if version <= current {
            continue
        }
        await db.Transact(async func(tx sql.Tx) void! {
            _ = await tx.Exec(stmt, [])?(fmt.Format("migration {0}", version))
            _ = await tx.Exec("insert into schema_version (version) values (?)", [version])?
        })?
        applied += 1
    }
    return applied
}
```

## src/repo.zl: the repository

`Product` maps a row. `Query<Product>` is generated at compile time: each
column is matched to the field of the same name in snake case
(`price_cents` → `PriceCents`), and `@sql.column("qty")` maps a column
whose name does not follow the rule.

`BySku` converts `sql.ErrNoRows` into `ErrUnknownSku`, so callers never
see the storage layer's error values. `Restock` uses
`db.Transact(func)`, which commits when the function succeeds and rolls
back when it fails.

`Ship` takes several products out of stock, all or nothing, and uses the
explicit form of a transaction: `Begin`, `defer tx.Rollback()`, `Commit`
at the end. Any `return` before `Commit`, including the one inside `?`,
runs the deferred `Rollback` and undoes the lines already taken out;
after a successful `Commit`, the transaction is done and `Rollback`
changes nothing. `move` takes a
`sql.Tx`, not the `DB`, so it can only run as part of a transaction.

<!-- file: src/repo.zl -->
```zell title="src/repo.zl"
// The repository: every SQL statement that reads or changes products
// lives here, behind typed methods.
namespace inventory.repo

import database

// Product maps a row of `products`. Query<Product> matches columns to
// fields by name in snake_case (PriceCents <- price_cents) at compile
// time; @sql.column names a column that does not follow the rule.
struct Product {
    Id         int
    Sku        string
    Name       string
    Category   string
    PriceCents int
    @sql.column("qty") Stock int
}

// One line of an order: which product and how many.
struct Line {
    Sku string
    Qty int
}

const ErrUnknownSku = error("unknown sku")

// OutOfStock carries what the caller needs to tell the customer.
struct OutOfStock {
    Sku       string
    Wanted    int
    Available int

    func Message() string {
        return fmt.Format(
            "{0}: wanted {1}, only {2} in stock",
            this.Sku,
            this.Wanted,
            this.Available,
        )
    }
}

struct Repo {
    db *sql.DB

    static func New(db *sql.DB) Repo {
        return Repo{db: db}
    }

    async func Add(p Product) int! {
        // Named parameters keep long inserts readable; the driver rewrites
        // them to positional ones.
        var r = await this.db.Exec(
            "insert into products (sku, name, category, price_cents, qty) values (:sku, :name, :cat, :price, :qty)",
            [
                sql.Named("sku", p.Sku),
                sql.Named("name", p.Name),
                sql.Named("cat", p.Category),
                sql.Named("price", p.PriceCents),
                sql.Named("qty", p.Stock),
            ],
        )?(p.Sku)
        return r.LastInsertId
    }

    // BySku turns "no row" into the domain error. Callers test
    // errors.Is(err, ErrUnknownSku) and never see sql.ErrNoRows, which
    // would leak the storage layer into their code.
    async func BySku(sku string) Product! {
        return try await this.db.QueryOne<Product>("select * from products where sku = ?", [
            sku,
        ]) catch err {
            if errors.Is(err, sql.ErrNoRows) {
                return errors.Wrap(ErrUnknownSku, sku)
            }
            return err
        }
    }

    async func All() []Product! {
        return await this.db.Query<Product>("select * from products order by sku", [])?
    }

    // Restock adds to the stock and records the movement, atomically.
    async func Restock(sku string, qty int) void! {
        await this.db.Transact(async func(tx sql.Tx) void! {
            await move(tx, sku, qty, "restock")?
        })?
    }

    // Ship takes every line of an order out of stock, or none of them.
    // The explicit form of a transaction: Begin, a deferred Rollback, and
    // Commit at the end. When any step returns an error, the function
    // exits before Commit and the deferred Rollback undoes the lines
    // already taken; after a successful Commit the Rollback is a no-op.
    async func Ship(order string, lines []Line) void! {
        var tx = await this.db.Begin()?
        defer tx.Rollback()
        for l in lines {
            var p = try await tx.QueryOne<Product>("select * from products where sku = ?", [
                l.Sku,
            ]) catch err {
                if errors.Is(err, sql.ErrNoRows) {
                    return errors.Wrap(ErrUnknownSku, l.Sku)
                }
                return err
            }
            if p.Stock < l.Qty {
                return OutOfStock{Sku: l.Sku, Wanted: l.Qty, Available: p.Stock}
            }
            await move(tx, l.Sku, -l.Qty, "order " + order)?
        }
        await tx.Commit()?
    }
}

// move changes the stock of one product inside a transaction and records
// why. It takes the transaction, not the DB, so it can only run as part
// of a larger unit of work.
async func move(tx sql.Tx, sku string, delta int, reason string) void! {
    var r = await tx.Exec("update products set qty = qty + ? where sku = ?", [delta, sku])?
    if r.RowsAffected == 0 {
        return errors.Wrap(ErrUnknownSku, sku)
    }
    _ = await tx.Exec(
        "insert into movements (product_id, delta, reason) select id, ?, ? from products where sku = ?",
        [delta, reason, sku],
    )?
}
```

## src/report.zl: queries for a report

Report queries return their own structs: a row of a `group by` is not a
`Product`. Column aliases (`as units`, `as value_cents`) connect the
select list to the fields. Money is integer cents everywhere, in the
database and in the program; `Money` formats it only for display.

<!-- file: src/report.zl -->
```zell title="src/report.zl"
// Read-only queries for reporting. They return their own small structs:
// a report row is not a Product, and Query<T> maps any select list whose
// column names match T's fields.
namespace inventory.report

import database
import io

struct CategoryValue {
    Category   string
    Items      int
    Units      int
    ValueCents int
}

struct Movement {
    Sku    string
    Delta  int
    Reason string
}

// ByCategory aggregates in SQL: the database does the grouping and the
// program only formats. Column aliases (as items, as units, ...) are what
// connects the select list to the struct fields.
async func ByCategory(db *sql.DB) []CategoryValue! {
    return await db.Query<CategoryValue>(
        `select category, count(*) as items, sum(qty) as units, sum(qty * price_cents) as value_cents
         from products group by category order by value_cents desc`,
        [],
    )?
}

async func LowStock(db *sql.DB, below int) []string! {
    // A single column maps to a slice of a plain type.
    return await db.Query<string>("select sku from products where qty < ? order by qty, sku", [
        below,
    ])?
}

async func History(db *sql.DB) []Movement! {
    return await db.Query<Movement>(
        `select p.sku as sku, m.delta as delta, m.reason as reason
         from movements m join products p on p.id = m.product_id order by m.id`,
        [],
    )?
}

// Money formats cents as 12.34. Money is kept in integer cents in the
// database and in the program: floats cannot represent 0.10 exactly.
func Money(cents int) string {
    return fmt.Format("{0}.{1}", cents / 100, strings.PadLeft(strconv.Itoa(cents % 100), 2, "0"))
}

// Print renders the report with text.table, which aligns the columns.
async func Print(db *sql.DB) void! {
    var t = text.table.New(["category", "items", "units", "value"])
    for col in [1, 2, 3] {
        t.SetAlign(col, text.table.Right)
    }
    for c in await ByCategory(db)? {
        t.AddRow([c.Category, strconv.Itoa(c.Items), strconv.Itoa(c.Units), Money(c.ValueCents)])
    }
    print(t.String())
    println("low stock (< 5):", await LowStock(db, 5)?)
    println("movements:")
    for m in await History(db)? {
        println(" ", m.Sku, fmt.Format("{0}", m.Delta), m.Reason)
    }
}
```

## src/main.zl: two sessions

`TempPath` has a `Drop` method: a destructor that runs exactly once when
the value goes out of scope, here at the end of `main`, and removes the
database file. Destructors run on every way out of a block, so they suit
cleanup that must not be forgotten. See
[Memory](../../guide/memory.md#destructors-drop).

`defer conn.Close()` closes the pool when each session ends. `DB.Close`
returns no error, so a plain `defer` is enough; a deferred call that
returns `T!` would have to handle the error in a closure.

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// inventory: a small stock database on SQLite. It creates the database
// in a temporary file, migrates it, runs a few orders (one of which must
// roll back), prints a report, and opens the file again to show that the
// data and the schema version persisted.
namespace inventory

import io

import inventory.repo

// TempPath removes its file when it goes out of scope. Drop runs exactly
// once, at the end of the block that owns the value, also when the block
// is left early by `?` or a panic, so the demo never leaves files behind.
struct TempPath {
    Path string

    func Drop() {
        _ = fs.Remove(this.Path)
    }
}

async func seed(r Repo) void! {
    var products = [
        Product{
            Sku: "BOLT-M6",
            Name: "Bolt M6x20",
            Category: "hardware",
            PriceCents: 12,
            Stock: 500,
        },
        Product{Sku: "NUT-M6", Name: "Nut M6", Category: "hardware", PriceCents: 5, Stock: 800},
        Product{
            Sku: "DRILL-8",
            Name: "Drill bit 8 mm",
            Category: "tools",
            PriceCents: 450,
            Stock: 12,
        },
        Product{Sku: "SAW-HAND", Name: "Hand saw", Category: "tools", PriceCents: 1999, Stock: 3},
        Product{
            Sku: "GLUE-W",
            Name: "Wood glue 250 ml",
            Category: "supplies",
            PriceCents: 675,
            Stock: 4,
        },
    ]
    for p in products {
        _ = await r.Add(p)?
    }
}

async func stock(r Repo, skus []string) string! {
    var parts []string = []
    for s in skus {
        var p = await r.BySku(s)?
        parts = append(parts, fmt.Format("{0}={1}", p.Sku, p.Stock))
    }
    return strings.Join(parts, " ")
}

async func session(path string) void! {
    var conn = await db.Open(path)?
    // DB.Close returns no error, so plain defer works: the pool closes
    // its connections when the function returns, whatever the path.
    defer conn.Close()
    println("migrations applied:", await db.Migrate(conn)?)

    var r = Repo.New(conn)
    await seed(r)?
    println("stock:", await stock(r, ["BOLT-M6", "NUT-M6", "SAW-HAND"])?)

    await r.Ship("A-1001", [Line{Sku: "BOLT-M6", Qty: 100}, Line{Sku: "NUT-M6", Qty: 100}])?
    println("shipped A-1001")

    // The second line cannot be served: the whole order rolls back, the
    // 50 bolts of the first line included.
    try await r.Ship("A-1002", [
        Line{Sku: "BOLT-M6", Qty: 50},
        Line{Sku: "SAW-HAND", Qty: 5},
    ]) catch err {
        var oos OutOfStock
        if errors.As(err, &oos) {
            println("A-1002 rejected:", err, "| short by", oos.Wanted - oos.Available)
        }
    }
    try await r.Ship("A-1003", [
        Line{Sku: "NUT-M6", Qty: 1},
        Line{Sku: "WIDGET", Qty: 1},
    ]) catch err {
        println("A-1003 rejected:", err, "| unknown sku:", errors.Is(err, ErrUnknownSku))
    }
    println("stock:", await stock(r, ["BOLT-M6", "NUT-M6", "SAW-HAND"])?)

    await r.Restock("SAW-HAND", 10)?
    // A constraint violation comes back as an ordinary error.
    try await r.Add(Product{
        Sku: "NUT-M6",
        Name: "dup",
        Category: "hardware",
        PriceCents: 1,
    }) catch err {
        println("duplicate:", strings.Contains(err.Message(), "UNIQUE"))
    }
    await report.Print(conn)?
}

async func reopen(path string) void! {
    var conn = await db.Open(path)?
    defer conn.Close()
    // Already at the latest version: nothing to apply.
    println("migrations applied:", await db.Migrate(conn)?)
    var all = await Repo.New(conn).All()?
    println(len(all), "products, first:", all[0].Sku, all[0].Name, report.Money(all[0].PriceCents))
}

async func main() void! {
    var tmp = TempPath{Path: fs.Join(fs.TempDir(), "inventory-demo.db")}
    _ = fs.Remove(tmp.Path)
    println("== first run")
    await session(tmp.Path)?
    println("== second run")
    await reopen(tmp.Path)?
    // tmp is dropped here and removes the database file.
}
```

## src/repo_test.zl: tests on in-memory databases

Each `Open(":memory:")` is a new empty database, so the tests are isolated
and leave nothing behind. The pool's single connection matters here:
every connection to `:memory:` would see a database of its own.

<!-- file: src/repo_test.zl -->
```zell title="src/repo_test.zl"
// Repository tests against an in-memory SQLite database: fast, isolated
// (every Open of ":memory:" is a new empty database) and nothing to clean
// up. One connection matters here: each connection to ":memory:" would
// see its own database, which is why db.Open limits the pool to one.
namespace inventory.tests

import inventory.repo

async func fresh() Repo! {
    var conn = await inventory.db.Open(":memory:")?
    _ = await inventory.db.Migrate(conn)?
    var r = Repo.New(conn)
    _ = await r.Add(Product{Sku: "A", Name: "a", Category: "x", PriceCents: 100, Stock: 5})?
    _ = await r.Add(Product{Sku: "B", Name: "b", Category: "x", PriceCents: 250, Stock: 1})?
    return r
}

@test
async func TestShipIsAllOrNothing(t *testing.T) {
    var r = try await fresh() catch err {
        t.Fatal(err.Message())
        return
    }
    try await r.Ship("o1", [Line{Sku: "A", Qty: 2}, Line{Sku: "B", Qty: 2}]) catch err {
        var oos OutOfStock
        testing.True(t, errors.As(err, &oos), "OutOfStock expected")
        testing.Equal(t, oos.Available, 1)
    }
    // A was not taken out: the transaction rolled back.
    var a = try await r.BySku("A") catch err {
        t.Fatal(err.Message())
        return
    }
    testing.Equal(t, a.Stock, 5)
}

@test
async func TestUnknownSku(t *testing.T) {
    var r = try await fresh() catch err {
        t.Fatal(err.Message())
        return
    }
    try await r.BySku("nope") catch err {
        testing.True(t, errors.Is(err, ErrUnknownSku), "ErrUnknownSku expected")
        return
    }
    t.Fatal("BySku found a product that does not exist")
}

@test
async func TestMigrateIsIdempotent(t *testing.T) {
    var conn = try await inventory.db.Open(":memory:") catch err {
        t.Fatal(err.Message())
        return
    }
    defer conn.Close()
    var first = try await inventory.db.Migrate(conn) catch err {
        t.Fatal(err.Message())
        return
    }
    var second = try await inventory.db.Migrate(conn) catch err {
        t.Fatal(err.Message())
        return
    }
    testing.Equal(t, first, 3)
    testing.Equal(t, second, 0)
}
```

## Output

<!-- file: expected.out -->
```text title="expected.out"
== first run
migrations applied: 3
stock: BOLT-M6=500 NUT-M6=800 SAW-HAND=3
shipped A-1001
A-1002 rejected: SAW-HAND: wanted 5, only 3 in stock | short by 2
A-1003 rejected: WIDGET: unknown sku | unknown sku: true
stock: BOLT-M6=400 NUT-M6=700 SAW-HAND=3
duplicate: true
category  items  units  value
--------  -----  -----  ------
tools         2     25  313.87
hardware      2   1100   83.00
supplies      1      4   27.00
low stock (< 5): [GLUE-W]
movements:
  BOLT-M6 -100 order A-1001
  NUT-M6 -100 order A-1001
  SAW-HAND 10 restock
== second run
migrations applied: 0
5 products, first: BOLT-M6 Bolt M6x20 0.12
```

Order A-1002 fails on its second line, and the stock line after it shows
that its first line (50 bolts) was rolled back too. A-1003 fails on an
unknown product. In the second session there is nothing to migrate, and
the data from the first session is still there.

## Next

- [bookshelf-api](../bookshelf-api/README.md): serve data like this over
  HTTP, behind a repository interface.
- [Errors](../../guide/errors.md): sentinels, typed errors, wrapping.
