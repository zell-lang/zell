# CLAUDE.md — guide for AI agents working on Zell

Zell is a compiled language (Go-like simplicity, C#-style namespaces and
`var` inference, methods inside structs, `T!` errors with `?`/`try`,
`async`/`await`, no GC) implemented as a Rust compiler on LLVM 18 with a C
runtime. Read `DESIGN.md` before touching anything: it is the language spec
and every change must match it or update it.

## Layout

| Path | What | Owner docs |
|---|---|---|
| `DESIGN.md` | language spec (syntax, semantics, memory, errors, async, reflection, stdlib) | — |
| `docs/TOOLING.md`, `PACKAGES.md`, `FRAMEWORK.md`, `PLATFORMS.md` | tooling/LSP, package manager, web/ORM/log/diag framework, platforms | — |
| `compiler/` | Rust crate `zell`: `lexer` → `parser` (`ast.rs`) → `sema/` (resolve, check, lower to `hir.rs`) → `codegen/` (inkwell/LLVM) → `build.rs` (object + link); `package/` (manifest, graph, lock); `session.rs` + `sema/index.rs` (LSP queries); `main.rs` CLI | `codegen/mod.rs` header = compiler-side ABI |
| `runtime/` | C11 runtime: refcounted objects, strings, slices, maps, errors, panic, I/O, task scheduler (`task.c`), stdlib `std_*.c` | `runtime/ABI.md` = C-side ABI, exact |
| `lsp/` | `zell-lsp` binary (lsp-server) | `docs/TOOLING.md` |
| `editors/vscode/` | TextMate grammar + LSP client | — |
| `grammar/zell.bnf` | canonical Grammar-Kit grammar (IDEA plugin); keep in sync with parser and `DESIGN.md` EBNF | — |
| `docs/guide/` | user guides in English (tour, errors, async, memory, packages, testing, debugging, web, faq); every ```zell block is compiled and run by `compiler/tests/guide.rs` | — |
| `examples/` | runnable `.zl` + byte-exact `.out`; `errors_compile/` must fail; `README.md` lists output rules and assumptions | — |
| `tests/spec/` | spec corpus, `// status: passing|pending` per file (TDD: pending tests must fail; flip when implemented) | `tests/spec/README.md` |
| `benchmarks/` | Zell vs Go programs; `scripts/bench-perf.sh`, `scripts/bench-size.sh` | `benchmarks/README.md` |

## Build and test

```sh
./scripts/setup.sh                 # once: llvm-18-dev, clang, lld (Ubuntu)
make -C runtime && make -C runtime test     # C runtime (-Werror), `make asan` for sanitizers
make -C runtime CROSS=x86_64-w64-mingw32 test-wine  # Windows runtime (mingw-w64, -Werror) + C tests under wine
make -C runtime CROSS=aarch64-linux-gnu test-cross  # linux-arm64 runtime + C tests under qemu (also CROSS=x86_64-linux-musl)
make -C runtime CROSS=aarch64-apple-darwin          # macOS: compile check against runtime/stub-sdk/macos (no SDK here)
cd compiler && cargo build && cargo test    # unit, parser, sema, package, run_examples, spec
cargo build --release && ./target/release/zell run examples/hello.zl
./target/release/zell check --dump-hir file.zl   # inspect typed HIR
./target/release/zell build file.zl --emit=llvm-ir # inspect IR
```

`cargo test` builds and runs every example byte-exact against its `.out`,
so the runtime must be built first (`make -C runtime`). Keep `cargo build`
warning-free and the runtime `-Werror` clean.

## Rules

- **Spec first.** New syntax or semantics go into `DESIGN.md` (and
  `grammar/zell.bnf` + the EBNF section) in the same change. Examples and
  spec tests are the acceptance criteria: add a `.zl` + `.out` for every
  feature, and a `// error:` program for every new diagnostic.
- **ABI is a contract.** Anything crossing compiler↔runtime is documented in
  `runtime/ABI.md` (C side) and `compiler/src/codegen/mod.rs` (compiler
  side): layouts, ownership (+1 returns, borrowed args), NULL rules. Change
  both or neither. Stdlib externs are declared in `compiler/src/sema/stdlib.rs`
  and implemented as `zell_<ns>_<name>` in `runtime/src/std_*.c`.
- **Output format** follows `examples/README.md`: `println` separates with
  one space; floats print like Go `%v` (fixed unless exp < -4 or >= 6);
  slices print `[1 2 3]`; errors print `Message()`.
- **Memory**: ARC per `DESIGN.md` "Память"; no tracing GC — the bounded
  cycle collector in `DESIGN.md` "Циклы" (trial deletion over objects of
  cyclic types only, `runtime/src/cycles.c`, ABI.md 3.8) is the only
  collection; never make it scan ordinary objects, roots or the whole
  heap. Double frees are bugs; leaks are tracked by
  `zell run --leak-check` and must not grow.
- **Go semantics are the default** wherever `DESIGN.md` is silent
  (integer division, terminating statements, defer, switch, maps), then
  record the decision in `examples/README.md` or `tests/spec/README.md`.
- **Cross-platform**: platform-specific runtime code belongs in
  `runtime/src/platform/`; no `#ifdef _WIN32` elsewhere (see
  `docs/PLATFORMS.md`).
- **Performance** regressions on `scripts/bench-perf.sh` need a reason in
  the commit message; sizes from `scripts/bench-size.sh` are tracked in
  `benchmarks/README.md`.
- **Git**: commit on `master`, small focused commits, message in English
  imperative ("Add X", "Fix Y"); never commit `target/`, `build/`,
  `*.o`, `libzell_rt.a`, `node_modules/`.
- **Parallel agents**: when several agents work at once, each owns disjoint
  paths; do not edit another agent's files, do not commit for them, and if
  the tree is temporarily broken by in-progress edits, wait and retry
  instead of "fixing" their code.
- Code style: Rust idiomatic, modules under ~1500 lines, doc comments on
  public items; C11 with `-Wall -Wextra -Werror`, every symbol prefixed
  `zell_` (private `zell__`).

## Current status and roadmap

Done: lexer/parser, sema + HIR, LLVM codegen (structs, interfaces, enums,
closures, defer, errors, async via LLVM coroutines), C runtime + core
stdlib, `zell build/run/check/test/fmt`, package manifests/workspaces/lock,
LSP (3.17 conformance in `docs/LSP_CONFORMANCE.md`, lints L0001–L0009,
refactorings) + VS Code client, IDEA plugin (`zell-lang/zell-idea-plugin`,
CI releases `YYYY.M.<build>`), examples, benchmarks, performance wave 1–2
(`agents/prompts/perf-wave-3.md` has every item's numbers), language
decisions of 2026-10-23 (comma-ok map lookup, leading-dot continuation,
type aliases, contextual `in`, keywords as attribute/field names), showcase
repos in `zell-lang` (`zconf`, `example-web-service`, `example-cli`,
`example-worker`; `docs/EXAMPLES.md`).

Next, in this order:

1. **Feedback leftovers** — `docs/LANGUAGE_FEEDBACK.md` items 17 (`web` error-mapping hook
   `MapError<E>(status)`), 19 (`ExitCoder` through `?("ctx")` wrapping), 20
   (generic/local type aliases), 25 (panic backtraces with `file:line` from
   a compact pc table, usable stripped). Simplify the showcase repos to one
   package now that `zell test` builds dependencies.
2. **Security tooling** — `zell audit` (OSV-format advisory DB
   `zell-lang/advisory-db`, check `zell.lock` and the toolchain version,
   `--deny` for CI), capability analysis of dependencies written into the
   lock (`zell tree --caps`), security lints in `sema/lint.rs` (SQL by string
   concatenation, `exec` with request data, insecure TLS, hardcoded
   secrets, path traversal, weak hashes), fuzz targets for every parser
   (HTTP/1.1, HTTP/2, TLS records, YAML/TOML/XML, zstd/brotli, regex) like
   `make -C runtime fuzz-json`, release signing (minisign/sigstore).
3. **LSP pass 3** — `docs/LSP_CONFORMANCE.md` missing items: inline
   function, implement interface, change signature, completion quality
   (textEdit/sortText/snippets), cancellation off the main thread, dynamic
   registration + `workspace/configuration`, incremental analysis per
   namespace; go-to-definition on type aliases.
4. **Memory and performance** — perf-wave-3 open items: task-per-await
   structural cost (await_chain 10.9x Go, WebSocket fix 4 poller→worker
   hand-off), P-256 verify, LTO by default, `--static` = musl/scratch
   images, package proxy public instance, `zell tool counters`
   (dotnet-counters analogue over `diag`).
5. **Language and framework** (`DESIGN.md` stages, `docs/REFLECTION_PLAN.md`,
   `docs/FRAMEWORK.md`): ownership elision, cycle-collector gaps (maps in
   threaded processes, tasks, errors), generics completion, block `try`,
   `reflect.TypesWith<A>()` then `web` → `zell-lang/web`, expression trees
   then `data` → `zell-lang/data`, run-time reflection, stdlib
   (`collections`, `net`, `encoding.json` write API — `Value.Set`),
   platforms (`docs/PLATFORMS.md`, Apple signing).
