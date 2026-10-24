# Language and framework gaps found while writing the showcase projects

Collected on 2026-10-23 while writing `zell-lang/zconf` and
`zell-lang/example-web-service` against master `6ff3eae`. Each item is a
real obstacle met in ordinary application code, with the workaround used;
they are the next inputs for DESIGN.md / stdlib / `zell test` work.

## Language
1. **No comma-ok map lookup**: `var (v, ok) = m[k]` and `k in m` do not exist;
   libraries keep a parallel `map[string]bool`. Spec decision needed (Go
   semantics are the default per CLAUDE.md).
2. **A method chain cannot start a line with `.`**; the dot must end the
   previous line (the lexer's automatic `;`). Go has the same rule, but it is
   the single most common surprise for C#/Kotlin users — consider allowing a
   leading `.` when the previous line ends an expression.
3. **`enum` is a keyword and cannot be an attribute field name** (only `default`
   is special-cased), so `@Conf(enum: "a,b")` had to become `oneOf:`.
4. **Lower-case attribute structs are private to their package**: user
   libraries must capitalise (`@zconf.Conf`), while std uses `@json`, `@cli.flag`.
5. **`async func` in an interface is accepted but never satisfied**, with a
   contradictory message ("method Get has type func(int) int!, want func(int)
   int!"); workaround: `func X() Task<T!>` delegating to a private async method.
6. **`defer this.mu.Unlock()` on a value `task.Mutex` field is rejected**
   ("interior pointer"); the field has to be `*task.Mutex`.
7. **Suspicious trace locations** in release builds (a framework DI lookup pointing
   at a test file line; the framework has since moved out, see item 17).

## Standard library
8. **No `os.Environ()`** — zconf reads `/proc/self/environ` (Linux-only, misses
   `os.Setenv` after start).
9. **`errors.Is`/`As` ignore a struct's `Unwrap()` method** — only `errors.Wrap`
   chains are followed.
10. **The built-in attribute name `json` shadows the `encoding.json` namespace**
    inside a package (`json.Value` → "Value is not a static method of struct
    json"); same for `yaml`, `toml`. Users write `encoding.json.Value`.
11. **Fixed (2026-10-24):** `json.Value`, `yaml.Value` and `toml.Value` all
    have `IsObject()`/`IsArray()`; the format-native names (`IsMap`/`IsSeq`,
    `IsTable`) stay as synonyms.
12. `math.Max` is float-only; `slices.Map` over ORM `ToList()` yields `*Row`.

## Tooling
13. **`zell test` does not support `async @test`** (`func(*testing.T)
    Task<void>` is not accepted); async examples with `// Output:` were used.
14. **`zell test` builds only the package's own `src/`, not its dependencies**
    (`compiler/src/testrun.rs:97-102`): a package importing a git dependency
    cannot be tested, which forced the `server/` split in the web service.
15. **A dependency's `*_test.zl` files are compiled into dependents** (zconf's
    test-only `Database` struct → "ambiguous name Database" downstream).
16. `zell init --help` is an "unknown option".

## Framework (`web`)

**web moved out (2026-10-24):** the `web` framework is no longer in the
tree (sources: git tag `web-in-tree`); it returns as the package
`zell-lang/web` after `reflect.TypesWith<A>()`. Item 17 and the note below
describe the in-tree version.

17. **Fixed (2026-10-24):** `app.MapError<E>(status)` maps a domain error
    type (also through wrappers) to a status once per application, and
    `app.OnError(func(*Context, error) *ProblemDetails)` sees every error
    first; unknown errors already answered a generic `"internal error"`
    500 outside development.

Not an obstacle but noted: OpenAPI 3.1 generation, `web.TestServer`, DI,
request id / tracing / metrics middleware all worked as documented.

## Added while writing `example-cli` and `example-worker` (same day)
18. **`zell test` leaks an internal symbol**: in a package with `src/main.zl` it
    prints `warning[L0007]: function zellUserMain__ is never used` — the lint
    must skip compiler-generated names.
19. **Fixed:** `cli.App.Out/Errors` writers and `cli.ExitCoder` (2026-10-23);
    since 2026-10-24 `Run` finds the `ExitCoder` through wrappers too
    (`lookup(x)?("ctx")`, `errors.Wrap`, `Unwrap()` methods).
20. **No type aliases** (`type Op = func(*Node) string!`): long function types
    are repeated at every use. Spec decision.
21. **`in` is a reserved word** — `var in = …` fails; common flag name. Spec
    decision (contextual keyword?).
22. **`strings` lacks `ContainsAny`, `IndexAny`, `ReplaceAll`**; `Replace`'s doc
    should say whether it replaces every occurrence.
23. **`encoding.json.Value` / `yaml.Value` are read-only**: no `Set`/`Put` or
    mutable builder beyond `NewObject(keys, values)`; only `toml.Document.Set`
    exists. Editing a document means converting to one's own tree.
24. **The default logger prints error traces with absolute paths over several
    lines** — noisy in service logs; relative paths (or one line) by default.

## Decisions (2026-10-23, maintainer)

The spec-level items are decided as follows; they go into DESIGN.md, the
EBNF and `grammar/zell.bnf` first, then the lexer/parser/sema with spec tests
(the IDEA plugin's lexer mirrors the automatic `;` rule and must follow #2):

| # | decision |
|---|---|
| 1 | **Comma-ok map lookup**, Go semantics: `var v, ok = m[k]` / `v, ok := m[k]`; `ok` is `bool`, `v` the zero value when absent. No `in` operator. |
| 2 | **A line starting with `.` continues the previous line**: the lexer inserts no automatic `;` before a leading `.` (method chains may break before the dot). |
| 3 | **Keywords are allowed as attribute argument names** (`@Conf(enum: "a,b")`). |
| 4 | **Attribute visibility stays as is**: a public attribute type is capitalised (`@zconf.Conf`); documented, not changed. |
| 20 | **Type aliases**: `type Name = Type` is a transparent alias (no new nominal type), usable in the stdlib too. |
| 21 | **`in` becomes a contextual keyword**: a keyword only in `for … in …`; an ordinary identifier elsewhere. |

25. **Done (2026-10-24):** panic backtraces print `file:line function` from the
    `.zell_lines` table (runtime/ABI.md 8.2, DESIGN.md "Паника"), stripped
    `--release` included; ELF only, macOS/Windows still symbols+offsets.
    **Panic backtraces show symbols+offsets, not source lines**
    (`zell_app__main+0x172` in debug, bare addresses in `--release`), while Go
    prints `main.go:10`. Wanted: a compact pc→file:line table in the binary
    (works stripped, no DWARF reader at run time), used by the panic printer and
    by error traces.

26. **Fixed (2026-10-24): `compiler/tests/reflection.rs::no_reflection_without_use`.**
    Stdlib interface method signatures were resolved eagerly from the module
    summaries: `net.http.Handler.Serve(req *Request, …)` forced the `Request`
    template, whose fields pulled in `net.sock`, `time`, `log` … down to
    `reflect`, for every program. Now `Sema::force_iface` resolves a stdlib
    interface's signatures on the first use of its type (`std_iface_lazy`);
    interfaces with default methods are forced when the default's `this`
    is typed. `println(1)` no longer loads `reflect`.

27. **Asynchronous destructors (`await using` analogue).** Synchronous
    `using` is already covered by `Drop` (DESIGN.md "Деструкторы"): no
    `defer f.Close()` is needed, and the examples should stop writing it.
    Proposed design when a `Close() Task<…>` first appears in the stdlib:
    `async func Drop()` allowed on a type; the compiler inserts `await
    this.Drop()` at the release point when the value dies inside an async
    function (block end, `?`, `return`, `break`); dying in a synchronous
    context (sync function, field of a non-async-owned object, slice) is a
    compile error, like copying a `Drop` value today; `Drop` still returns
    no error — callers who want the close error call `await x.Close()?`
    first and `Drop` is then a no-op. Not needed yet: every stdlib `Close()`
    is synchronous.

28. **Done (2026-10-24): a panic ends its task, not the process.**
    DESIGN.md "Паника", ABI.md 8.1: the runtime destroys the task's
    coroutine frames (defers run, locals and `Drop`s released) and
    completes the task as panicked; `Task<T!>` awaiters get an error
    wrapping `task.ErrPanic`, `Task<T>` awaiters panic again, unobserved
    tasks print the panic; `main` exits 2 as before; the framework's
    recover middleware answered 500 "internal error" and kept serving
    (`tests/spec/panics/*`; the web part moved out with the framework,
    tag `web-in-tree`). Follow-ups:
    (a) synchronous frames are not unwound (their defers do not run, what
    they own leaks: e.g. a handler that panics in a plain function leaks
    its request objects), which needs unwind tables or per-call landing
    pads; (b) Windows still ends the process (no setjmp unwinding there);
    (c) `UseRecover` costs one `task.Yield` per request; (d) without
    `UseRecover` the request's connection is dropped without a response.
    Still open from the original item: `database.sql.DB`/`Tx` have no
    `Drop` (only `Rows`), so `examples/sql.zl` still needs `defer
    tx.Rollback()` and `defer db.Close()`; give them a `Drop`.

29. **Red test `compiler/tests/debug.rs::debug_info_flags_and_identical_code`**
    since the ZLN2 line tables (`c2d65eb`): codegen now always emits DWARF
    line tables and artificial subprograms for the helpers (`rc.retain.*`,
    `any.inline`), so a `--release --emit=obj` object carries `.debug_info`
    the test expects to be absent. Decide: strip `.debug_*` from release
    objects after the line table is read (keeps the test), or relax the test
    to "no DWARF in the linked `--release` executable" (the linker gets `-s`).

## Writing the documentation example projects (2026-10-25)

Found while writing the eleven projects of `docs/examples/` (the
documentation's Examples section, tested by
`compiler/tests/doc_examples.rs`). Batch 1 (`wordfreq`, `todo-cli`,
`bookshelf-api`, `inventory-db`, `validate`):

30. **`zell test` scans only `src/`**: there is no `tests/` directory for
    integration tests. Workaround: test files live in `src/` as
    `*_test.zl`, in a namespace of their own (`validate.tests`) to test
    through exported names only.
31. **A syntax error in a `_test.zl` file prints only "no tests found in
    ./src"**: the runner skips files that do not parse, without the
    diagnostic. Workaround: `zell check` the file to see the error.
32. **An `async` method does not satisfy an interface method returning
    `Task<T>`** (see item 5): every implementation is a plain method that
    returns the task of a private `async` function (`MemoryRepo` in
    `bookshelf-api`, `Memory` in `testing-guide`). Worth fixing before the
    `zell-lang/web` package, whose handlers and repositories are all like
    this.
33. **`f.Attribute<A>() != nil` is not a static condition**, so a static
    `if` cannot pick it and `reflect.Error` cannot reject a misplaced
    attribute at compile time (`@Email` on an `int` field, say).
    Workaround: `validate` applies a rule only where it makes sense, at run
    time, and documents the rest.
34. **`reflect.Error` is not a terminating statement**, and its diagnostic
    points at the `reflect.Error` call inside the library, not at the call
    site that instantiated it. Workaround: a dummy `return` after it; the
    message names the type (`Validate needs a struct, got int`) to make up
    for the location.
35. **`fmt.Format` has no alignment or width** (`{0,-10}` in C#).
    Workaround: `fmt.Sprintf("%-10s", ...)` or `text.table`.
36. **`strings.Builder` lacks `WriteRune`**. Workaround:
    `b.WriteString(string(r))`.
37. **No indented JSON output** (`json.MarshalIndent`). Workaround: the
    examples print compact JSON.
38. **`defer f.Close()` on a `void!` method is rejected** (a deferred call
    must handle its error). Workaround: rely on `Drop`, or defer a closure
    with `try ... catch`; consider allowing it with the error dropped, or a
    `defer try` form.
39. **`testing.Equal` and `testing.True` take messages inconsistently**:
    `Equal(t, got, want)` has no message argument, `True(t, cond, msg)`
    requires one. Workaround: `t.Log` the context before `Equal`.
40. **`import net` brings `net.ErrNotFound` into scope and it clashes with
    a local `ErrNotFound`**. Workaround: rename the local sentinel
    (`ErrBookNotFound`) or use `net.` names without the import.
41. **Relative namespace references fail from a sibling namespace**
    (`model.X` written in `app.api` for `app.model.X`). Workaround: import
    the full path (`import app.model`).

Batch 2 (`jobrunner`, `typed-cache`, `scene-graph`, `errors-deep-dive`,
`workspace`, `testing-guide`):

42. **No `select`, and `task.Any` over a `Recv` loses values.** The
    stand-in for `select { case v := <-ch: ... case <-time.After(d): ... }`
    is `await task.Any(ch.Recv(), task.Delay(d))` in a loop, but the losing
    `Recv` is cancelled while staying registered as a receiver: the next
    `Send` on that channel hands its value to the dead receiver and it is
    lost (after one lost race, a value sent before the next `Any` is never
    seen). A runtime bug (a cancelled `Recv` must leave the wait queue) and
    a spec gap (`select` over channel operations and timers). Workaround in
    `jobrunner/src/service.zl`: one task owns the only `Recv` and cancels a
    scope; the timer wait runs in that scope.
43. **In a cancelled context, `Send` returns at once without an error**
    (it is `Task<void>`), so a producer cannot tell whether the value was
    delivered, and every receive with no value ready fails: a result
    collector running in the cancelled context stops at once and the
    report is empty. Workaround: cancel the work, not the plumbing; the
    pool takes the jobs' `task.Context` as a parameter and runs only the
    jobs in it (`task.RunWith(run, ...)`). Consider `Send` returning
    `Task<void!>`.
44. **Arenas crash with many or large allocations.** Repro: `a.New<P>()`
    20 000 times into a `[]*P`, then `for p in ps { sum += p.X }` ends in
    SIGSEGV in `zell_retain` (`runtime/src/mem.c:305`); 8 000 objects
    work. `a.Alloc<Particle>(50000)` crashes the same way, and two frames
    of 1000 `a.New` objects crash under `--leak-check` only. Likely
    allocations beyond the first 64 KB chunk are not recognised as arena
    records. Workaround in `scene-graph`: one `a.Alloc<T>(1000)` per frame.
45. **Error frames created in standard library functions carry the
    caller's file name**: `strconv.Atoi` failing in `g.zl` gives frames
    `g.zl:17 strconv.fail`, `g.zl:17 strconv.Atoi` (line 17 of the std
    file, in a 16-line `g.zl`). Workaround in `errors-deep-dive`: convert
    std errors at the boundary (`number` turns the parse error into a
    `ValidationError`), so their frames are never printed.
46. **`return f()` in a `T!` function, where `f()` is also `T!`, is
    rejected** ("result of type V! must be handled"); it has to be `return
    f()?`. Go and Rust return the result as is. Workaround: `?`.
47. **`zell test` does not understand a workspace root**: without a path it
    compiles the whole tree as one package and fails with "namespace
    `mapper` is outside package `geo`"; `-p name` is not accepted
    (`--workspace` neither). Workaround: `zell test <member dir>` for each
    member; `doc_examples.rs` does that for a root with `[workspace]`.
48. **`import geo` followed only by qualified uses (`geo.Hello()`) is an
    "unused import" error.** Workaround: drop the import (a direct
    dependency is reachable by its full name) or use the names unqualified.
49. **`import os.signal` does not make `signal.X` available**; the
    namespace is reached as `signal.Name` after `import os`, unlike every
    other sub-namespace. Workaround: `import os`.
50. **Assertion failures have no `file:line`** (`got 6000, want 600`), and
    `testing.T` has no `Helper()`, `Cleanup()` or `TempDir()`. Workaround:
    a subtest per table row, so the failing row is named.
51. **No assertion for a `T!` result**: every test of a fallible function
    writes `try ... catch err { t.Fatal(...) }`. Workaround: per-package
    helpers (`mustParse`, `parseError` in `testing-guide`); consider
    `testing.Ok(t, f())` returning the value and `testing.Fails(t, f())`
    returning the error.
