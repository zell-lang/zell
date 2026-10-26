# Extracting stdlib packages

Status 2026-10-26: the repositories below exist in the `zell-lang` org as
scaffolds (manifest, CI, README, an empty namespace). No code has moved
yet; `data` is plain library code (no compiler support), ready to move.

## Target layout

| Repo | What | Source today |
|---|---|---|
| `db` | base database interfaces (connections, transactions, rows, driver interface) and the ORM (models, queries on expression trees, change tracking) | `std/database/sql`, `std/data/*.zl` |
| `sqlite` | SQLite driver for `db` | `runtime/src/std_sqlite.c`, `std/database/sqlite` |
| `postgres` | PostgreSQL driver | `std/database/postgres` |
| `mysql`, `mssql`, `oracle` | drivers for MySQL/MariaDB, SQL Server, Oracle | new |
| `migrate` | schema migration tool (executable) | `std/data/data_migrate.zl` |
| `redis` | Redis client | `std/data/redis` |
| `kafka` | Kafka client | `std/data/kafka` |
| `s3` | S3 client | `std/cloud/s3` |
| `blob` | blob storage abstraction | `std/cloud/blob` |
| `template` | text templates | `std/text/template` |
| `web` | routing, DI, middleware, binding, OpenAPI (see CLAUDE.md, `web-in-tree`) | tag `web-in-tree` |

Stays in stdlib: `log`, `diag`, and every other root in `STD_ROOTS`
(`compiler/src/sema/stdlib.rs`) not listed above.

## Order

1. Expression trees in the compiler (done 2026-10-26: `expr<F>`, package
   `expr`, DESIGN.md "Деревья выражений").
2. The ORM model on static reflection (done 2026-10-26: `data_model.zl`
   builds it from `reflect.TypeOf<T>()` and `Configure`, the accessors are
   unrolled over `reflect.Fields<T>()`, `data.Open<C>()` binds the sets
   through a static type switch, migrations come from
   `reflect.TypesWith<migration>()`), and `data` queries on expression
   trees (done 2026-10-26: `data_sql.zl`, plans cached per lambda site).
3. Remove `sema/orm*.rs` (done 2026-10-26: the compiler has no `data`
   code left; `zell data migration add` builds the project with a
   generated `main` printing `data.ModelJson<C>()`).
4. Move the packages one at a time. Each move removes its root from
   `STD_ROOTS` (`data`, `database`, `cloud`, parts of `text`) once nothing
   left in the root, updates imports in examples/tests/guides, and keeps
   outputs identical.

### What the move of `data` to `zell-lang/db` still needs

- **Process-wide state.** The context models are cached in the runtime's
  slot map (`diag.rtSlots`/`rtLock`, internal to the stdlib), because
  stdlib sources cannot declare global variables. A package can: the
  cache becomes a package-level `var` with a lock.
- **`unsafe`.** The identity map keys entities by address
  (`unsafe.ReadInt(unsafe.AddrOf(a), 8)` in `ptrKey`): the package needs
  `unsafe = true` in `[capabilities]`, or the identity map keyed otherwise.
- **Its own namespace in the translator.** `data_sql.zl` recognises its
  tree functions by full name (`data.Any`, `data.Group.Count`); the
  prefix must become the package's namespace.
- **Abstract checks.** Stdlib generic code is not checked abstractly; a
  package's is. `migrationEntry<M>` calls `Up` on an unconstrained `M`,
  and the static-switch bodies of `bindContext`/`buildContextModel` rely
  on instance-only checking; they need bounds (`M: upper`) or the shape
  checks moved behind static `if`s.
- **The `zell data` CLI** (`compiler/src/datacli.rs`) stays in the
  compiler until `migrate` exists as a tool (below); it only uses the
  library API (`data.ModelJson`, `data.RunMigrations`, `Migrations()`).
- **SQLite's native layer** (below), and the imports of every spec,
  example and benchmark that uses `data` (`tests/spec/data`,
  `benchmarks/orm_sqlite`, `compiler/tests/fixtures/data`).

### Dialects: the driver plug-in of `db`

The ORM core never branches on the provider; it asks `interface Dialect`
(`std/data/data_dialect.zl`, see docs/FRAMEWORK.md "Диалекты") for
placeholders, paging, casts, dialect calls (`TranslateCall`/
`TranslateMember`), write and locking clauses and the DDL. The split is
by file:

| File | Goes to | Contents |
|---|---|---|
| `data.zl`, `data_model.zl`, `data_query.zl`, `data_sql.zl`, `data_sub.zl`, `data_set.zl`, `data_window.zl`, `data_save.zl`, `data_migrate.zl`, `data_dialect.zl` | `db` | context, model (incl. `@jsonb` documents and array columns), query builder, tree translator (common SQL: subqueries, set operations, CTEs, window functions, raw fragments, `LIKE`), unit of work, migrator (`DdlDialect`), the `Dialect`/`Opener` interfaces, registry, `SqlBuilder` and the `ansi*` defaults |
| `data_sqlite.zl` | `sqlite` | `SqliteDialect` (with `database.sqlite`) |
| `data_postgres.zl` | `postgres` | `PostgresDialect`, the PostgreSQL-only query functions (`@expr.call`: jsonb, arrays, `ILike`, regex, full-text search; they become namespace `postgres`), `Subscription` and `ctx.Notify/Listen` (the `Context` methods become functions of the package), with `database.postgres.Listener` |

At the move, `dialectOf` loses its built-in fallback: each driver package
calls `db.RegisterDialect(SqliteDialect{})` (or its own) from its `Open`,
and `Provider` gives way to the dialect name (`db.Open<C>("postgres",
dsn)`), keeping `Provider.Postgres/.Sqlite` as aliases.

A new driver (`mysql`, `mssql`, `oracle`) implements `Dialect` (every method; `ansiPaging(d, ...)` and the other `ansi*` functions are the ANSI versions to delegate to: interface default methods would be checked in every program) and `Opener`: `Name`,
`Open`; `Placeholder` (`?`, `@pN`, `:N`); `Paging` (`LIMIT`, `OFFSET …
ROWS FETCH NEXT … ROWS ONLY`); `CastType`; `TranslateCall` for the string
functions whose SQL differs (`len`, `strings.Contains/HasPrefix/HasSuffix`:
the core's fallback is SQL:2016 `position`/`substring`) and for its own
functions; `Returning` (MSSQL `OUTPUT`, MySQL none: the core then reads
generated keys another way, still to do), `Lock`, the DDL methods
(`ColumnType`, `ColumnDef` identity syntax, `AlterColumn`, foreign keys,
`CreateIndex`). Identifiers are ANSI-quoted by the core: MySQL turns on
`sql_mode=ANSI_QUOTES` for its connections.

## Open blocker: native code in packages

Packages cannot carry native C code today: `docs/PACKAGES.md` has no native
sources and no FFI. `sqlite` (and any driver needing a C client library)
needs one of:

| Option | Pros | Cons |
|---|---|---|
| A. Package-level C sources (manifest `[native]`: sources, include dirs, link libs; compiled and linked by `zell build`) | packages are self-contained; the runtime does not grow with each driver; third parties can ship native code | new build surface (C toolchain on user machines, cross-compilation per target, caching), supply-chain risk (audit/caps must cover C), ABI stability for `extern` |
| B. Thin C layer stays in the runtime, called via `extern` from the package | no new build machinery; works with the current toolchain; runtime already tested on all platforms | runtime keeps driver-specific code and link deps (libsqlite3), so the move is only partial; every new driver touches the runtime and its release cycle |

Recommendation: B for `sqlite` as the first step (unblocks the move), A as a
design for later. PostgreSQL, MySQL, MSSQL need a decision on whether they
speak the wire protocol in Zell (no C needed) or bind a client library (needs
A or B); Oracle most likely needs the latter.

## `migrate` as a tool

`migrate` is an executable package (`entry = "src/main.zl"`), built from the
`db` migration API. Later, in the style of `go tool` / `go install`:

- `zell install <module>[@version]` builds the package's entry and puts the
  binary in `~/.zell/bin`;
- `zell tool <name>` runs a tool pinned in the project's `zell.toml`
  (`[tools]`), recorded in `zell.lock`, built and cached on first use.

Until then `migrate` is run with `zell run` from a checkout.

## Showcase repos and docs

- Showcase repos (`zconf`, `example-*`) keep working through the move: each
  gets the new dependency in `[dependencies]` in the same step as the root
  removal; outputs must stay identical.
- `docs/examples/inventory-db` uses `database.sql`; it switches to `db` plus
  a driver as a package with dependencies, and the docs-site sync keeps it
  compiled. Guides in `docs/guide/` that use `database.*`, `data.*`, `cloud.*`
  must be rewritten to package imports in the same commit as each move
  (they are compiled by `compiler/tests/guide.rs`; those that need packages
  move to the examples with a manifest).
