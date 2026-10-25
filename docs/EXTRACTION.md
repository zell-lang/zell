# Extracting stdlib packages

Status 2026-10-25: the repositories below exist in the `zell-lang` org as
scaffolds (manifest, CI, README, an empty namespace). No code has moved yet.

## Target layout

| Repo | What | Source today |
|---|---|---|
| `db` | base database interfaces (connections, transactions, rows, driver interface) and the ORM (models, queries, change tracking) | `std/database/sql`, `std/data/*.zl` |
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
   `expr`, DESIGN.md "Деревья выражений"), and `db` queries built on them
   (next).
2. The ORM model on static reflection (`Fields<T>()`, `Attribute<A>()`,
   `TypesWith<A>()`).
3. Remove `sema/orm*.rs`.
4. Move the packages one at a time. Each move removes its root from
   `STD_ROOTS` (`data`, `database`, `cloud`, parts of `text`) once nothing
   left in the root, updates imports in examples/tests/guides, and keeps
   outputs identical.

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
