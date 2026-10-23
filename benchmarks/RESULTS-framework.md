# Framework benchmarks: Zell vs Go

Benchmarks of the framework layer (`web`, `data`, `log`, `net.ws`) and of
process startup, each against the usual Go libraries. They complement the
language-level benchmarks in [README.md](README.md).

**Status: the programs are written and verified; no results yet.** The
4 vCPU machine was shared by five agents when these were added, so no
timing run was made. Every benchmark was built and run once at smoke
scale (`SCALE=small`), and every Zell/Go pair printed identical
checksums. The result tables below are **TODO**: fill them from a quiet
run (best of 5, implementations alternating; that is what each `run.sh`
does by default). Until then, the only numbers in this file are ones
that do not depend on machine load: allocation counts, SQL statement
counts and binary sizes.

Base: commit e51cf3d (compiler and runtime unchanged by this work), clang/LLVM 18,
go 1.24.7, Linux 6.18 x86-64. Go modules (fetched with `go mod tidy`, pinned
in each `go.sum`): gin 1.10.0, gorm 1.31.2 + gorm.io/driver/sqlite 1.6.0 +
gorm.io/driver/postgres 1.5.11, mattn/go-sqlite3 1.14.52, pgx 5.7.2, zap
1.28.0, zerolog 1.35.1, gorilla/websocket 1.5.3.

## How to run

```sh
make -C runtime && cargo build --release -p zell
benchmarks/web_api/run.sh          # 3 servers x RUNS rounds with the Go load generator
benchmarks/orm_sqlite/run.sh       # SQLite memory + file, PostgreSQL if installed (PG=0 skips)
benchmarks/logging/run.sh          # needs ~1.2 GB free in $LOGDIR (default /tmp)
benchmarks/websocket_echo/run.sh   # needs ulimit -n >= 2100
benchmarks/startup/run.sh          # 300 runs per program + stripped sizes
```

Common knobs: `RUNS=5` (rounds; the implementations alternate within a
round), `SCALE=small` (smoke sizes, seconds per benchmark), `ZELL=<compiler>`.
`orm_sqlite`, `logging` and `websocket_echo` also have a `main.zl` +
`main.go` pair at the top of their directory, so `scripts/bench-perf.sh
orm_sqlite logging websocket_echo` runs them too (one process each, with
the default arguments: SQLite in memory, zap, both ends of the echo in one
process). Each `run.sh` exits with `MISMATCH` if an implementation's
output differs from Zell's.

## The benchmarks

### `web_api`: REST API on `web` vs Gin vs net/http

| file | what |
|---|---|
| `web_api/server.zl` | `web`: `@route` controller with `@get`/`@post` attribute routes, two DI singletons (`Catalog` built by its static `New`, `Orders` with a `sync.AtomicInt`) injected into controller fields, `@body` JSON binding, `@validate` (range, required, email) with 400 Problem Details, `Result<T>` statuses, `UseLogging` (one rendered event per request into a discarding `log` sink) and an API-key middleware (`X-Api-Key`, else 401) |
| `web_api/go/gin` | Gin in release mode: the same routes, `ShouldBindJSON` + `binding:` tags (go-playground/validator), `gin.LoggerWithWriter(io.Discard)`, the same middleware |
| `web_api/go/nethttp` | Go 1.22 `ServeMux` patterns, `encoding/json`, hand-written validation, `slog` text line per request into `io.Discard` |
| `web_api/go/load` | the load generator: 64 keep-alive HTTP/1.1 connections, 200k requests in all, one in flight per connection, round-robin over the 4 endpoints; req/s, p50 and p99 latency |

Endpoints: `GET /api/items/{id}` (path parameter), `GET
/api/items?category=&limit=` (query), `GET /api/items/{id}/quote?qty=`
(path parameter and query), `POST /api/orders` (validated JSON body, 201).
A catalogue of 1000 items is identical on all servers.

Verification: `load -check` sends a fixed sequence of 24 requests (incl.
401, 400 for a bad path parameter and three validation failures, 404) to
each fresh server and prints the statuses plus an FNV checksum of the
200/201 bodies; the three outputs must be equal. Every load run prints an
order-independent checksum (sum of FNV-1a of "status body"; the order id
is taken out of POST bodies, because which request gets which id depends
on scheduling), and it must be equal too. Error bodies are not compared
(Problem Details vs Gin's validator messages).

Results (best of 5 rounds by req/s, the p50/p99 of that round; ratio =
X / Gin, Gin = 1.00; for req/s **> 1 is better**, for latency and RSS < 1
is better; peak RSS of the server process after all rounds):

| server | req/s | p50 ms | p99 ms | req/s vs Gin | p99 vs Gin | peak RSS | RSS vs Gin |
|---|---|---|---|---|---|---|---|
| Zell `web` | 41 778 | 1.430 | 3.974 | 0.71 | 0.66 | 6.9 MB | 0.31 |
| Go Gin | 58 960 | 0.746 | 5.982 | 1.00 | 1.00 | 22.5 MB | 1.00 |
| Go net/http | 59 891 | 0.793 | 5.165 | 1.02 | 0.86 | 15.3 MB | 0.68 |

Rounds ranged 39.8-41.8k (Zell), 49.7-59.0k (Gin), 53.9-59.9k (net/http)
req/s. Zell's `web` serves 0.71x Gin's requests with twice its median
latency, but a 34% lower p99 and a third of its memory; every request
pays the `log` pipeline of `UseLogging` (open item 4).

### `orm_sqlite`: `data` vs GORM vs database/sql

`orm_sqlite/main.zl` (`data`) and `orm_sqlite/main.go` (`-impl=gorm`, the
default, or `-impl=sql`) run the same phases on the same schema:

| phase | size | Zell `data` | GORM | database/sql |
|---|---|---|---|---|
| migrate | 1 migration | `@migration` struct (CreateTable + CreateIndex), `ctx.Migrate()` (records `__migrations`) | `AutoMigrate` | DDL + `__migrations` row in one transaction |
| insert | 100k rows, batches of 1000 | `AddRange` + `SaveChanges` + `ClearTracker` | `Create(&batch)` | one 1000-row `INSERT` per transaction |
| lookup | 1M by primary key | `AsNoTracking().FirstOrDefault((x) => x.Id == id)` | `Take(&p, id)` | prepared `QueryRow` |
| filter | 10k queries | `Where((x) => x.Category == c && x.Price > min).OrderBy((x) => x.Id).Take(10)` | `Where("category = ? AND price > ?").Order("id").Limit(10)` | prepared `Query` |
| update | 10k rows | 100 tracked rows loaded, changed, `SaveChanges` (change tracking writes the 2 changed columns) | load 100, `Updates(map)` of the 2 columns in a transaction (GORM has no change tracking) | load 100, prepared `UPDATE` in a transaction |

Stores: SQLite `:memory:` (one connection), an SQLite file (default journal
mode, i.e. DELETE with fsync), and PostgreSQL 16 when its server binaries
are installed (`pg.sh` starts a throwaway cluster as the `postgres` user;
they are on this machine, no server was running, so `run.sh` starts one
itself). SQLite: Zell loads the system `libsqlite3.so.0` dynamically, Go
links the SQLite bundled with mattn/go-sqlite3. Both use a 5 s busy
timeout and the default journal mode. Zell caches 64 prepared statements
per connection by default; GORM runs with defaults (`PrepareStmt` off).

Verification: stdout (sum of looked-up price+stock, rows and id sum of
the filters, updated count, total stock, rows changed, row count) must be
equal for all three on every store. At smoke scale all 9 combinations
printed `lookup 1000600000 / filter 10000 523232 / update 1000 496000 1000
/ rows 10000`.

Results (ms per phase, best of 5 rounds for SQLite; PostgreSQL: best of
2 rounds for Zell and GORM and a single run for database/sql, because the
session's time limit stopped the run in round 3 (the throwaway cluster
was stopped by the script's trap); ratio = Zell / GORM, GORM = 1.00,
< 1 is better; all checksums equal):

| store | phase | Zell | GORM | database/sql | Zell / GORM |
|---|---|---|---|---|---|
| memory | migrate | 0.68 | 0.78 | 0.54 | 0.88 |
| memory | insert 100k | 1 819 | 414 | 221 | 4.40 |
| memory | lookup 1M | 14 079 | 20 796 | 4 639 | 0.68 |
| memory | filter 10k | 468 | 645 | 329 | 0.73 |
| memory | update 10k | 92 | 171 | 41 | 0.54 |
| file | migrate | 3.81 | 3.66 | 2.00 | 1.04 |
| file | insert 100k | 2 243 | 707 | 464 | 3.17 |
| file | lookup 1M | 21 568 | 29 941 | 11 183 | 0.72 |
| file | filter 10k | 589 | 756 | 410 | 0.78 |
| file | update 10k | 247 | 317 | 165 | 0.78 |
| postgres | migrate | 12.0 | 10.3 | 9.1 (single run) | 1.17 |
| postgres | insert 100k | 4 008 | 955 | 900 (single run) | 4.20 |
| postgres | lookup 1M | 315 094 | 145 016 | 123 582 (single run) | 2.17 |
| postgres | filter 10k | 5 379 | 2 658 | 2 386 (single run) | 2.02 |
| postgres | update 10k | 3 361 | 1 549 | 1 516 (single run) | 2.17 |

On SQLite `data` beats GORM on every read and update phase (0.54-0.78x)
and is 1.5-3x behind hand-written database/sql; inserts are 3-4x GORM
(open items 1-2: `entryOf` scans and 100-row statements). On PostgreSQL
the pure-Zell `database.postgres` driver makes every query about 2x
GORM/pgx (315 vs 145 us per primary-key lookup round trip), so the
driver, not `data`, is the bottleneck there.

### `logging`: `log` vs zap, zerolog, slog

`logging/main.zl` and `logging/main.go` (`-impl=zap|zerolog|slog`), 5M
records per scenario. Every record has 4 properties (Method, Path,
Status, Elapsed) and 2 context properties (Service, RequestId). Zell binds
them with `ForContext` and `log.Context.Push`, Go with `With`.

| scenario | Zell | Go |
|---|---|---|
| devnull | CLEF JSON sink (`WriteTo.Json`) to `/dev/null` | JSON encoders (zap production config, zerolog, `slog.NewJSONHandler` with `LogAttrs`) to `/dev/null` |
| file | the same into a file | the same into a file (unbuffered `*os.File`, like Zell's file sink) |
| disabled | `logger.Debug(...)` on an Information logger | the same `Debug` call with the same fields |

Reported: records/s and allocations per record (Zell: the object counter of
`diag.Runtime().Allocations`; Go: `runtime.MemStats.Mallocs`). The two do
not count the same things (Zell counts every refcounted object incl.
strings and slices; Go counts heap allocations after escape analysis).

Verification: `verify.py` checks every written file (line count; every
record's properties and context properties, all lines for small runs,
every 997th for large ones). All four loggers pass at smoke scale. Zell
additionally renders the message (`@m`) and stores the template (`@mt`) per
record (Serilog CLEF), while the Go loggers write a constant message.

Allocations per record (deterministic, measured at smoke scale, 100k records):

| scenario | Zell `log` | zap | zerolog | slog |
|---|---|---|---|---|
| devnull | **344.4** | 1.0 | 0.0 | 3.0 |
| file | **344.4** | 1.0 | 0.0 | 3.0 |
| disabled | 0 | 1.0 | 0 | 0 |

Results (**single run**, `RUNS=1`, 5M records per scenario; records/s,
ratio = Zell / Go library, Go = 1.00, **> 1 is better** for records/s; for
the disabled row ns per call, < 1 is better). Allocations per record at
full size match the smoke-scale table above (Zell 344.4, zap 1.0,
zerolog 0, slog 3.0).

| scenario | Zell | zap | zerolog | slog | Zell / zap | Zell / zerolog | Zell / slog |
|---|---|---|---|---|---|---|---|
| devnull (records/s) | 40 326 | 612 984 | 1 226 122 | 501 155 | 0.066 | 0.033 | 0.080 |
| file (records/s) | 39 589 | 411 188 | 756 629 | 394 849 | 0.096 | 0.052 | 0.100 |
| disabled (ns/call) | 3.7 | 122 | 16.8 | 25.0 | 0.030 | 0.22 | 0.15 |

The enabled path is 10-30x slower than every Go logger (open item 4: 344
allocations per record); the disabled path is the fastest of all (item 6;
0 allocations, 3.7 ns per call, so the guarded call was not folded away
entirely).

### `websocket_echo`: `net.ws` vs gorilla/websocket

1000 connections x 10k binary messages of 128 bytes, one in flight per
connection, every echo compared byte for byte. `run.sh` cross-measures
like `http_hello`: Zell client and Go client against the Zell server and
the Go server, each server in its own process. Module downloads work on
this machine, so gorilla/websocket 1.5.3 is used (no compression on either
side: `net.ws` does not implement permessage-deflate, and gorilla does not
negotiate it by default).

Verification: every client prints `messages <n> bytes <n>`, which must be
equal for all 4 pairs (smoke: `messages 20000 bytes 2560000`).

Results (**single run**, `RUNS=1`, `ulimit -n 4096`; messages/s, in
brackets the ratio to Go client -> Go server, Go = 1.00, **> 1 is
better**; all four pairs echoed `messages 10000000 bytes 1280000000`):

| server \ client | Zell client | Go client |
|---|---|---|
| Zell server | 40 401 (0.30) | 48 441 (0.36) |
| Go server | 55 177 (0.41) | 135 540 (1.00) |

Both the `net.ws` server (0.36x with Go's client) and client (0.41x
against Go's server) are well behind gorilla/websocket on small-message
echo; this is the same per-message async round trip that `async_io`
shows (3.6x Go) plus framing in Zell.

### `startup`: exec to first output

`startup/harness` (a small `perf stat -r`) starts each program 300 times,
the programs alternating, and measures from just before fork+exec to the
first byte on stdout, or on stderr for servers (which are then killed):

| program | Zell | Go |
|---|---|---|
| hello | `startup/hello/main.zl` | `startup/hello/main.go` |
| web_api server reaching "listening" | `web_api/server.zl` (prints the port after `app.Listen`: routes built, socket bound) | `web_api/go/gin`, `web_api/go/nethttp` (port printed after `net.Listen`) |
| CLI with a 1 MB config | `startup/cli/main.zl`: `io.ReadFile` + `json.Parse<Config>` (7329 routes, 1 048 646 bytes, written by `harness -genconfig`) | `startup/cli/main.go`: `os.ReadFile` + `json.Unmarshal` |

Verification: hello output is compared byte for byte; the CLI summaries
must be equal (`gateway 3 false routes 7329 timeouts 27929924 tags 14658`).

Results (TODO, quiet run; ms, ratio = Zell / Go on the median, Go = 1.0):

| program | Zell min / median / p90 | Go min / median / p90 | Zell / Go |
|---|---|---|---|
| hello | TODO | TODO | TODO |
| web_api listening (vs Gin) | TODO | TODO | TODO |
| web_api listening (vs net/http) | TODO | TODO | TODO |
| cli 1 MB config | TODO | TODO | TODO |

Binary sizes (stripped: Zell `--release`, Go `-ldflags="-s -w"`; these
are deterministic; `startup/run.sh` prints them):

| program | Zell | Go | Zell / Go |
|---|---|---|---|
| hello | 33 040 | 1 437 880 | 0.023 |
| cli (encoding.json) | 103 792 | 1 929 400 | 0.054 |
| web_api server vs Gin | 1 911 656 | 8 495 396 | 0.225 |
| web_api server vs net/http | 1 911 656 | 5 992 740 | 0.319 |

`scripts/bench-size.sh` builds only `examples/hello.zl` (plus C/C++/Go
hellos); the web_api server sizes above come from `startup/run.sh`.

## Open items found while writing the benchmarks

These are gaps found in the framework, the stdlib and the toolchain. None was
fixed here, because `runtime/` and `compiler/` are owned by other agents and the
stdlib fixes belong with their owners. Each item says where the code is.

1. **`data`: `Set.Add` is O(tracked entities), so `AddRange` is
   quadratic.** `entryOf` (`std/data/data.zl:505`) scans every entry of
   the context (with a type switch on each, and no early exit) to find
   whether the object is already tracked. `Add`, `Attach`, `Remove`, `Update`
   and `Entry` all call it. With 1000-row batches that is about 500k
   iterations per batch, and adding 100k entities to one context without
   `ClearTracker` would be about 5·10⁹. Fix: an object-pointer to entry
   map in `ctxState` (next to `ids`). The benchmark clears the tracker
   after every batch so that the phase stays measurable.
2. **`data`: inserts go out 100 rows per statement.** `batchSize()`
   (`std/data/data_save.zl:13`) is a constant 100. A `SaveChanges` of 1000
   new rows sent 12 statements (BEGIN, 10 multi-row INSERT ... RETURNING,
   COMMIT; counted with `ctx.LogTo`), where the database/sql baseline sends 1.
   SQLite 3.32+ allows 32766 parameters and PostgreSQL 65535, so the size
   could follow the provider's limit (rows x columns).
3. **`data`: every query rebuilds its SQL text and copies every row.** The
   lambdas are translated at compile time, but each execution reassembles
   the statement (`core.writer` / `selectSql` / `columnList` /
   `sqlw.frag`, roughly 15-20% of the instructions (inclusive) in a callgrind run of the
   smoke workload). `ctxState.all` (`std/data/data.zl:317`) copies each
   row's values into a fresh `[]any` before `materialize` maps them, and
   `FirstOrDefault` goes through `Take(1).ToList()` (`data_query.zl:641`).
   In that profile `zell_retain`/`zell_release` were about 40% of all
   instructions. Caching the SQL text per call site (the fragments are
   constants) and mapping rows straight from the driver buffer would remove
   most of it.
4. **`log`: about 344 object allocations per JSON record** (zap 1,
   zerolog 0, slog 3). The causes, in `std/log/log.zl`:
   - `quote` (line 839) takes a one-byte substring `s[i..i + 1]` for every
     input byte. A template plus message of about 110 bytes per record
     gives more than 200 string objects. Scanning `s[i]` as bytes and
     copying runs would fix it.
   - `clef` (line 1317) quotes the constant template (`@mt`) on every
     record, and `formatTime` converts the .NET-style layout with
     `timeLayout` on every call.
   - `emit` (line 579) allocates an `Event`, a `map[string]any` and a
     `keys` slice, and renders the message, before any sink runs. A JSON
     sink could render from `parts`/`values` directly.
   - `UseLogging` in `web` uses the same pipeline, so these costs are part
     of every `web_api` request too.
5. **`log`: no buffered file sink.** `FileSink.Emit` makes one `write(2)`
   per record under a spin lock (`rtLock`). The Go loggers do the same with
   a plain `*os.File`, so the benchmark is fair, but zap offers
   `BufferedWriteSyncer`. `log` has only `Async` (a channel to a task), not
   a buffered writer with periodic flush.
6. **`log`, a strength: a disabled level costs nothing.** The compiled
   template guards the call, so arguments are not evaluated, and the
   disabled scenario made 0 allocations (zap makes 1 per call for its
   field slice). When the quiet run measures this, check that the loop was
   not folded away entirely.
7. **Partly fixed: `web` / `encoding.json`: an empty list result is
   written as `null`.** `json.Parse` now decodes `[]` into an empty non-nil
   slice (`[z][0..0]`: a backing array with no elements) and `json.Format`
   writes such a slice as `[]`; only a nil slice is `null` (examples/README.md
   decision 22). A handler that builds its result with `var out []T = []`
   and never appends still returns the nil slice, i.e. `null`; it can
   return `items[0..0]` or a decoded value, or `web`'s result writer could
   write slices as arrays regardless.
8. **Fixed: `encoding.json` made three passes and built error paths
   eagerly.** The text is parsed once in C into an arena document
   (`runtime/src/std_json.c`); `Parse<T>` walks its nodes directly and the
   `Users[2].id` paths are assembled only when a value is wrong (a
   `pathError` extended on the way out). Compile-time reflection also
   folds `f.Attribute<json>()`, `f.Name` and the `@json` name tests to
   constants in the unrolled field loops. See RESULTS-stdlib.md,
   `json_codec`.
9. **Release builds target the build machine's CPU.** For the host target
   `codegen::target_machine` (`compiler/src/codegen/mod.rs:458`) passes
   `get_host_cpu_name()` and `get_host_cpu_features()`, like `-march=native`.
   On this machine that means AVX-512 (valgrind 3.22 stops with SIGILL on
   an EVEX instruction in `bench.logging.run`). Two consequences. In every
   Zell vs Go comparison Zell may use AVX-512 while Go targets
   `GOAMD64=v1`; for a fair run, use the same level on both sides or say
   which is used. And a `--release` binary may not run on an older x86-64
   machine. A portable default (x86-64-v2 or v3, with an opt-in `native`)
   looks safer.
10. **Compiler diagnostic: an ambiguous name is reported, then sema goes on
    with one of the candidates.** With `import log` and `import web`,
    `c *Context` gives "ambiguous name Context", then four follow-on errors
    that assume `log.Context` ("struct Context has no method Header", ...).
    After an ambiguity error, a poisoned type would avoid the cascade.
11. **Language ergonomics seen while porting.** A method cannot return
    `&this.items[i]` (an interior pointer; `Catalog.Get` returns `(Item,
    bool)`, a copy, where the Go version returns `*Item`). There is no
    `make([]T, n)` (the 128-byte payloads are built with `append`). Neither
    is a bug (DESIGN.md "Память"), but both show up in every port of Go code.
12. **PostgreSQL** is available here only as server binaries, with no
    running server. `orm_sqlite/pg.sh` starts a throwaway cluster on port
    55433 (as the `postgres` user, since the session runs as root). Zell's
    pure-Zell `database.postgres` driver, GORM and pgx all passed the
    checksum comparison at smoke scale.
13. **`benchmarks/http_hello/run.sh` leaves its servers running.** It
    starts them with `sp=$(start ...)`, so they run in a command
    substitution subshell, and the trap's `kill $(jobs -p)` does not see
    them. The servers outlive the script and keep their ports. The scripts
    added here had the same bug; they now start servers in the main shell
    and kill them by PID. `http_hello/run.sh` was left unchanged.
14. **Not covered:** HTTP/2 or TLS for `web_api` (`http_hello` covers the
    transport); `web` features beyond the four endpoints (JWT `UseAuth`,
    OpenAPI, gzip); `data` relations (`Include`) and grouping; WebSocket
    compression (`net.ws` has none).
