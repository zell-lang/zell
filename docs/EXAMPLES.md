# Examples

Complete programs, shown file by file, the way Zell is really written:
each one is a project with a manifest, several source files split into
namespaces, tests where they make sense, and a page that walks through
every file and shows all of its code. They are deterministic and
self-contained (fixed inputs, servers on a loopback port, databases in a
temporary file), so `zell run` prints the same output everywhere, and
`cargo test` (`compiler/tests/doc_examples.rs`) runs each of them, checks
that output byte for byte, runs its tests and its leak check, and checks
that every file on the page is the file in the repository.

| Example | What it is |
|---|---|
| [wordfreq](examples/wordfreq/README.md) | text processing: read files line by line, count words in a map, sort, print a table, write a report and read it back; errors at the boundary |
| [todo-cli](examples/todo-cli/README.md) | a command-line tool with `cli` subcommands and flags on structs, a JSON file as storage, custom error types and exit codes |
| [bookshelf-api](examples/bookshelf-api/README.md) | an HTTP JSON API on `net.http`: router, handlers over a repository interface, middleware for logging and error mapping, graceful shutdown |
| [inventory-db](examples/inventory-db/README.md) | `database.sql` with SQLite: migrations, typed `Query<T>`, transactions that roll back, report queries, `Drop` |
| [validate](examples/validate/README.md) | your own attributes and reflection: a generic validator on `reflect.Fields<T>()`, a command registry on `reflect.TypesWith<A>()`, `reflect.Error` |

Run one with:

```sh
cd docs/examples/wordfreq
zell run
zell test
```

## Showcase repositories

These are separate repositories, **being created at the time of writing**
— all four are published. Each is a Zell project
(`zell.toml`), so the same two commands work everywhere: `zell run` and
`zell test`. They need the toolchain from the
[README](../README.md#install).

### zconf — published (v0.1.0)

[github.com/zell-lang/zconf](https://github.com/zell-lang/zconf): a layered
configuration library. Values come from JSON, YAML, TOML and `.env` files,
environment variables and command-line arguments, merged in layers, and
`Load<T>` fills a struct driven by attributes. Modelled on the maintainer's
Go library `sconf`. Shows: generics, attributes and compile-time
reflection, the `encoding.*` packages, how to build and publish a library.

```sh
git clone https://github.com/zell-lang/zconf && cd zconf
zell test
```

### example-web-service — published

[github.com/zell-lang/example-web-service](https://github.com/zell-lang/example-web-service):
a REST service in a clean architecture. Shows: dependency injection,
middleware that maps errors to HTTP statuses, an OpenAPI document, metrics,
and SQLite through `data`. It is written on the `web` framework, which
has moved out of the toolchain until it returns as the package
`zell-lang/web` (see [FRAMEWORK.md](FRAMEWORK.md); the in-tree sources are
kept by the git tag `web-in-tree`), so build it with toolchain
**`v0.1.0-rc.3`**, the last release with the built-in `web`.

```sh
git clone https://github.com/zell-lang/example-web-service && cd example-web-service
zell run           # then call the printed address
zell test
```

### example-cli — published

[github.com/zell-lang/example-cli](https://github.com/zell-lang/example-cli):
a command-line tool with subcommands, JSON and YAML input and output, and
meaningful exit codes. Shows: the `cli` package, `T!` errors at the program
boundary, `zell build --release` for a single distributable binary.

```sh
git clone https://github.com/zell-lang/example-cli && cd example-cli
zell run -- --help
zell test
```

### example-worker — published

[github.com/zell-lang/example-worker](https://github.com/zell-lang/example-worker):
a background worker. Shows: channels, a worker pool, and graceful shutdown
on a signal with task cancellation. See also [Async](guide/async.md).

```sh
git clone https://github.com/zell-lang/example-worker && cd example-worker
zell run           # Ctrl-C stops it gracefully
zell test
```

## Small programs in `examples/`

Every program in [`examples/`](../examples) is a small program (most are one file) with a
byte-exact `.out` file, built and run by `cargo test`; each shows one
feature in isolation. Run one with `zell run examples/channels.zl`. The
output rules are in [examples/README.md](../examples/README.md). A
selection:

| Example | What it shows |
|---|---|
| [`hello.zl`](../examples/hello.zl) | the smallest program |
| [`errors.zl`](../examples/errors.zl), [`try_block.zl`](../examples/try_block.zl) | `T!`, sentinel errors, `?`, `try ... catch`, block `try` |
| [`errors_trace.zl`](../examples/errors_trace.zl) | error traces: where an error was created and passed through |
| [`generics.zl`](../examples/generics.zl) | generic functions and structs, constraints, inference |
| [`async.zl`](../examples/async.zl), [`channels.zl`](../examples/channels.zl) | tasks, `await`, a producer/consumer pipeline, `WaitGroup`, mutexes |
| [`cancel.zl`](../examples/cancel.zl) | cancellation, timeouts and `task.Any` through the task context |
| [`weak.zl`](../examples/weak.zl), [`drop.zl`](../examples/drop.zl) | `weak` back pointers and destructors: memory without a GC |
| [`reflection.zl`](../examples/reflection.zl) | attributes as values and compile-time reflection |
| [`json.zl`](../examples/json.zl) | typed JSON through reflection, `@json` attributes |
| [`sql.zl`](../examples/sql.zl) | `database.sql` with SQLite: typed `Query<T>`, transactions |
| [`http_server.zl`](../examples/http_server.zl) | an HTTP router queried by a client in the same program |
| [`cli_tool.zl`](../examples/cli_tool.zl) | a CLI with subcommands and flags declared on structs |
| [`log.zl`](../examples/log.zl), [`metrics.zl`](../examples/metrics.zl) | structured logging with message templates; Prometheus metrics |
