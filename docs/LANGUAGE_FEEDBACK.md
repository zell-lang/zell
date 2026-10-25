# Language and framework gaps found while writing the showcase projects

Status (2026-10-25): ✅ done · ❌ open · 📘 kept as specified (decision, no change) · ⏳ ready for the spec · ❓ not re-verified

Collected on 2026-10-23 while writing `zell-lang/zconf` and
`zell-lang/example-web-service` against master `6ff3eae`. Each item is a
real obstacle met in ordinary application code, with the workaround used;
they are the next inputs for DESIGN.md / stdlib / `zell test` work.

## Language
1. ✅ **No comma-ok map lookup**: `var (v, ok) = m[k]` and `k in m` do not exist;
   libraries keep a parallel `map[string]bool`. Spec decision needed (Go
   semantics are the default per CLAUDE.md).
2. ✅ **A method chain cannot start a line with `.`**; the dot must end the
   previous line (the lexer's automatic `;`). Go has the same rule, but it is
   the single most common surprise for C#/Kotlin users — consider allowing a
   leading `.` when the previous line ends an expression.
3. ✅ **`enum` is a keyword and cannot be an attribute field name** (only `default`
   is special-cased), so `@Conf(enum: "a,b")` had to become `oneOf:`.
4. 📘 **Lower-case attribute structs are private to their package**: user
   libraries must capitalise (`@zconf.Conf`), while std uses `@json`, `@cli.flag`.
5. ❌ **Still open (verified 2026-10-25; the diagnostic now explains the wrapper):** **`async func` in an interface is accepted but never satisfied**, with a
   contradictory message ("method Get has type func(int) int!, want func(int)
   int!"); workaround: `func X() Task<T!>` delegating to a private async method.
6. ✅ **Fixed (verified 2026-10-25):** **`defer this.mu.Unlock()` on a value `task.Mutex` field is rejected**
   ("interior pointer"); the field has to be `*task.Mutex`.
7. ✅ **Fixed (verified 2026-10-25):** **Suspicious trace locations** in release builds (a framework DI lookup pointing
   at a test file line; the framework has since moved out, see item 17).

## Standard library
8. ✅ **Fixed (verified 2026-10-25):** **No `os.Environ()`** — zconf reads `/proc/self/environ` (Linux-only, misses
   `os.Setenv` after start).
9. ✅ **Fixed (verified 2026-10-25):** **`errors.Is`/`As` ignore a struct's `Unwrap()` method** — only `errors.Wrap`
   chains are followed.
10. ✅ **Fixed (verified 2026-10-25):** **The built-in attribute name `json` shadows the `encoding.json` namespace**
    inside a package (`json.Value` → "Value is not a static method of struct
    json"); same for `yaml`, `toml`. Users write `encoding.json.Value`.
11. ✅ **Fixed (2026-10-24):** `json.Value`, `yaml.Value` and `toml.Value` all
    have `IsObject()`/`IsArray()`; the format-native names (`IsMap`/`IsSeq`,
    `IsTable`) stay as synonyms.
12. ❓ `math.Max` is float-only; `slices.Map` over ORM `ToList()` yields `*Row`.

## Tooling
13. ✅ **Fixed (verified 2026-10-25):** **`zell test` does not support `async @test`** (`func(*testing.T)
    Task<void>` is not accepted); async examples with `// Output:` were used.
14. ✅ **Fixed (verified 2026-10-25):** **`zell test` builds only the package's own `src/`, not its dependencies**
    (`compiler/src/testrun.rs:97-102`): a package importing a git dependency
    cannot be tested, which forced the `server/` split in the web service.
15. ✅ **Fixed (verified 2026-10-25):** **A dependency's `*_test.zl` files are compiled into dependents** (zconf's
    test-only `Database` struct → "ambiguous name Database" downstream).
16. ❓ `zell init --help` is an "unknown option".

## Framework (`web`)

**web moved out (2026-10-24):** the `web` framework is no longer in the
tree (sources: git tag `web-in-tree`); it returns as the package
`zell-lang/web` after `reflect.TypesWith<A>()`. Item 17 and the note below
describe the in-tree version.

17. ✅ **Fixed (2026-10-24):** `app.MapError<E>(status)` maps a domain error
    type (also through wrappers) to a status once per application, and
    `app.OnError(func(*Context, error) *ProblemDetails)` sees every error
    first; unknown errors already answered a generic `"internal error"`
    500 outside development.

Not an obstacle but noted: OpenAPI 3.1 generation, `web.TestServer`, DI,
request id / tracing / metrics middleware all worked as documented.

## Added while writing `example-cli` and `example-worker` (same day)
18. ✅ **Fixed (verified 2026-10-25):** **`zell test` leaks an internal symbol**: in a package with `src/main.zl` it
    prints `warning[L0007]: function zellUserMain__ is never used` — the lint
    must skip compiler-generated names.
19. ✅ **Fixed:** `cli.App.Out/Errors` writers and `cli.ExitCoder` (2026-10-23);
    since 2026-10-24 `Run` finds the `ExitCoder` through wrappers too
    (`lookup(x)?("ctx")`, `errors.Wrap`, `Unwrap()` methods).
20. ✅ **No type aliases** (`type Op = func(*Node) string!`): long function types
    are repeated at every use. Spec decision.
21. ✅ **`in` is a reserved word** — `var in = …` fails; common flag name. Spec
    decision (contextual keyword?).
22. ✅ **Fixed (verified 2026-10-25):** **`strings` lacks `ContainsAny`, `IndexAny`, `ReplaceAll`**; `Replace`'s doc
    should say whether it replaces every occurrence.
23. ❌ **`encoding.json.Value` / `yaml.Value` are read-only**: no `Set`/`Put` or
    mutable builder beyond `NewObject(keys, values)`; only `toml.Document.Set`
    exists. Editing a document means converting to one's own tree.
24. ✅ **Fixed (verified 2026-10-25):** **The default logger prints error traces with absolute paths over several
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

25. ✅ **Done (2026-10-24):** panic backtraces print `file:line function` from the
    `.zell_lines` table (runtime/ABI.md 8.2, DESIGN.md "Паника"), stripped
    `--release` included; ELF only, macOS/Windows still symbols+offsets.
    **Panic backtraces show symbols+offsets, not source lines**
    (`zell_app__main+0x172` in debug, bare addresses in `--release`), while Go
    prints `main.go:10`. Wanted: a compact pc→file:line table in the binary
    (works stripped, no DWARF reader at run time), used by the panic printer and
    by error traces.

26. ✅ **Fixed (2026-10-24): `compiler/tests/reflection.rs::no_reflection_without_use`.**
    Stdlib interface method signatures were resolved eagerly from the module
    summaries: `net.http.Handler.Serve(req *Request, …)` forced the `Request`
    template, whose fields pulled in `net.sock`, `time`, `log` … down to
    `reflect`, for every program. Now `Sema::force_iface` resolves a stdlib
    interface's signatures on the first use of its type (`std_iface_lazy`);
    interfaces with default methods are forced when the default's `this`
    is typed. `println(1)` no longer loads `reflect`.

27. ⏳ **Ready for the spec (2026-10-25: `sql.Rows.Close()` already returns `Task<void!>`):** **Asynchronous destructors (`await using` analogue).** Synchronous
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

28. ✅ **Done (2026-10-24): a panic ends its task, not the process.**
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

29. ❌ **Red test `compiler/tests/debug.rs::debug_info_flags_and_identical_code`**
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

30. ❌ **`zell test` scans only `src/`**: there is no `tests/` directory for
    integration tests. Workaround: test files live in `src/` as
    `*_test.zl`, in a namespace of their own (`validate.tests`) to test
    through exported names only.
31. ✅ **Fixed (2026-10-25):** cause: the test collector skipped files that did not parse; `zell test` now prints their diagnostics and exits 1 (`compiler/src/testrun.rs`). **A syntax error in a `_test.zl` file prints only "no tests found in
    ./src"**: the runner skips files that do not parse, without the
    diagnostic. Workaround: `zell check` the file to see the error.
32. ❌ **An `async` method does not satisfy an interface method returning
    `Task<T>`** (see item 5): every implementation is a plain method that
    returns the task of a private `async` function (`MemoryRepo` in
    `bookshelf-api`, `Memory` in `testing-guide`). Worth fixing before the
    `zell-lang/web` package, whose handlers and repositories are all like
    this.
33. ❌ **`f.Attribute<A>() != nil` is not a static condition**, so a static
    `if` cannot pick it and `reflect.Error` cannot reject a misplaced
    attribute at compile time (`@Email` on an `int` field, say).
    Workaround: `validate` applies a rule only where it makes sense, at run
    time, and documents the rest.
34. ✅ **Fixed (2026-10-25):** `reflect.Error(msg)` is lowered as a terminating `panic` (never runs: reachable means a compile error), and its diagnostic adds `instantiated here (file:line:col)` for the call that created the instance and the outer ones (`InstFunc::sites`, `Diagnostic::related`). **`reflect.Error` is not a terminating statement**, and its diagnostic
    points at the `reflect.Error` call inside the library, not at the call
    site that instantiated it. Workaround: a dummy `return` after it; the
    message names the type (`Validate needs a struct, got int`) to make up
    for the location.
35. ✅ **Fixed (2026-10-25):** `{N,W}` right-aligns and `{N,-W}` left-aligns to W code points (C#; `fmt.Format` has no `:format` spec since its arguments arrive as strings), in `runtime/src/std_fmt.c`. **`fmt.Format` has no alignment or width** (`{0,-10}` in C#).
    Workaround: `fmt.Sprintf("%-10s", ...)` or `text.table`.
36. ✅ **Fixed (2026-10-25):** `strings.Builder` gained `WriteRune(r char)` (UTF-8, invalid code points as U+FFFD) plus the Go names `WriteString` and `WriteByte`. **`strings.Builder` lacks `WriteRune`**. Workaround:
    `b.WriteString(string(r))`.
37. ✅ **Fixed (2026-10-25):** `json.FormatIndent<T>(v, prefix, indent)` with Go `MarshalIndent` layout (C re-layout `zell_encoding_json_rt_indent` over `Format`). **No indented JSON output** (`json.MarshalIndent`). Workaround: the
    examples print compact JSON.
38. 📘 **As specified, not changed (2026-10-25):** a `T!` result must be handled (DESIGN.md «Игнорирование и обязательность»), a deferred call included; the diagnostic proposes `defer func() { try f() catch err { ... } }()`. **`defer f.Close()` on a `void!` method is rejected** (a deferred call
    must handle its error). Workaround: rely on `Drop`, or defer a closure
    with `try ... catch`; consider allowing it with the error dropped, or a
    `defer try` form.
39. ✅ **Fixed (2026-10-25):** added `testing.EqualMsg(t, got, want, msg)` (no variadics; named like `EqualFatal`), reporting `msg: got X, want Y`; `True`/`False` keep their required message; rules in `docs/guide/testing.md`. **`testing.Equal` and `testing.True` take messages inconsistently**:
    `Equal(t, got, want)` has no message argument, `True(t, cond, msg)`
    requires one. Workaround: `t.Log` the context before `Equal`.
40. ❌ **`import net` brings `net.ErrNotFound` into scope and it clashes with
    a local `ErrNotFound`**. Workaround: rename the local sentinel
    (`ErrBookNotFound`) or use `net.` names without the import.
41. ❌ **Relative namespace references fail from a sibling namespace**
    (`model.X` written in `app.api` for `app.model.X`). Workaround: import
    the full path (`import app.model`).

Batch 2 (`jobrunner`, `typed-cache`, `scene-graph`, `errors-deep-dive`,
`workspace`, `testing-guide`):

42. ✅ **Fixed (2026-10-25)** (the runtime bug; `select` stays a spec gap): cause: a coroutine runs as part of its caller's task until its first suspension, so the `Recv` it started hooked its cancellation on the caller (or nowhere, before `main` first suspended), and cancelling the losing coroutine's task never reached the queued wait; `zell_task_lazy` now moves the hooks linked since the ramp's `zell_coro_enter` mark onto the new task, and `task.Any` cancels its losers before it completes. **No `select`, and `task.Any` over a `Recv` loses values.** The
    stand-in for `select { case v := <-ch: ... case <-time.After(d): ... }`
    is `await task.Any(ch.Recv(), task.Delay(d))` in a loop, but the losing
    `Recv` is cancelled while staying registered as a receiver: the next
    `Send` on that channel hands its value to the dead receiver and it is
    lost (after one lost race, a value sent before the next `Any` is never
    seen). A runtime bug (a cancelled `Recv` must leave the wait queue) and
    a spec gap (`select` over channel operations and timers). Workaround in
    `jobrunner/src/service.zl`: one task owns the only `Recv` and cancels a
    scope; the timer wait runs in that scope.
43. 📘 **As specified, not changed (2026-10-25):** DESIGN.md «Асинхронность» says `Delay` and `Send` wake at once in a cancelled context but cannot return an error (check `task.CheckCancelled()?`), while `Recv` returns `task.ErrCancelled`; making `Send` `Task<void!>` would be a language change. **In a cancelled context, `Send` returns at once without an error**
    (it is `Task<void>`), so a producer cannot tell whether the value was
    delivered, and every receive with no value ready fails: a result
    collector running in the cancelled context stops at once and the
    report is empty. Workaround: cancel the work, not the plumbing; the
    pool takes the jobs' `task.Context` as a parameter and runs only the
    jobs in it (`task.RunWith(run, ...)`). Consider `Send` returning
    `Task<void!>`.
44. ✅ **Fixed (2026-10-25):** cause: a `for` loop's copy of its sequence lived until function exit and was released after the locals (the `Arena` included), and `Free` returned chunks to the system, so releasing a slice of arena pointers read freed memory; now the copy is released at the loop's end, scoped temporaries go before locals, and `Free` keeps its chunks as spares (ABI.md 3.6). **Arenas crash with many or large allocations.** Repro: `a.New<P>()`
    20 000 times into a `[]*P`, then `for p in ps { sum += p.X }` ends in
    SIGSEGV in `zell_retain` (`runtime/src/mem.c:305`); 8 000 objects
    work. `a.Alloc<Particle>(50000)` crashes the same way, and two frames
    of 1000 `a.New` objects crash under `--leak-check` only. Likely
    allocations beyond the first 64 KB chunk are not recognised as arena
    records. Workaround in `scene-graph`: one `a.Alloc<T>(1000)` per frame.
45. ❌ **Error frames created in standard library functions carry the
    caller's file name**: `strconv.Atoi` failing in `g.zl` gives frames
    `g.zl:17 strconv.fail`, `g.zl:17 strconv.Atoi` (line 17 of the std
    file, in a 16-line `g.zl`). Workaround in `errors-deep-dive`: convert
    std errors at the boundary (`number` turns the parse error into a
    `ValidationError`), so their frames are never printed.
46. 📘 **As specified, not changed (2026-10-25):** DESIGN.md «Ошибки»: "`return` в такой функции принимает либо значение `T`, либо ошибку" and "Значение типа `T!` нельзя использовать, не обработав"; a `T!` is neither, and propagation is always marked with `?` ("неявного проброса нет"). **`return f()` in a `T!` function, where `f()` is also `T!`, is
    rejected** ("result of type V! must be handled"); it has to be `return
    f()?`. Go and Rust return the result as is. Workaround: `?`.
47. ❌ **`zell test` does not understand a workspace root**: without a path it
    compiles the whole tree as one package and fails with "namespace
    `mapper` is outside package `geo`"; `-p name` is not accepted
    (`--workspace` neither). Workaround: `zell test <member dir>` for each
    member; `doc_examples.rs` does that for a root with `[workspace]`.
48. ✅ **Fixed (2026-10-25):** cause: a qualified head (`geo`) resolved through the own namespace or the roots and never marked the glob import of that namespace as used; `lookup_global` now does. **`import geo` followed only by qualified uses (`geo.Hello()`) is an
    "unused import" error.** Workaround: drop the import (a direct
    dependency is reachable by its full name) or use the names unqualified.
49. 📘 **As specified, not changed (2026-10-25):** DESIGN.md «Правила импорта»: `import net.http` gives `Get(url)`, and "Импорт родительского пространства делает дочерние доступными по последнему сегменту: `import net` → `http.Get(url)`"; `os.signal` behaves exactly so (`net.http` too; `json.X` after `import encoding.json` is the attribute-name exception). Spec test `namespaces/nested_import_forms`. **`import os.signal` does not make `signal.X` available**; the
    namespace is reached as `signal.Name` after `import os`, unlike every
    other sub-namespace. Workaround: `import os`.
50. ❌ **Assertion failures have no `file:line`** (`got 6000, want 600`), and
    `testing.T` has no `Helper()`, `Cleanup()` or `TempDir()`. Workaround:
    a subtest per table row, so the failing row is named.
51. ❌ **Needs a spec decision (2026-10-25):** `testing.Ok(t, f())` cannot be written: DESIGN.md «Игнорирование и обязательность» forbids passing an unhandled `T!` as an argument, and the closure form fails the same rule and inference. Options: a `T!`-typed parameter form, or a `t.Must(f())` intrinsic. **No assertion for a `T!` result**: every test of a fallible function
    writes `try ... catch err { t.Fatal(...) }`. Workaround: per-package
    helpers (`mustParse`, `parseError` in `testing-guide`); consider
    `testing.Ok(t, f())` returning the value and `testing.Fails(t, f())`
    returning the error.
52. ✅ **Fixed (2026-10-25):** same cause as 48 (stdlib namespaces are roots reachable without import, so `task.Delay` never touched `import task`); an import used by qualified names now counts, a truly unused one stays an error (DESIGN.md «Правила импорта»). **Implicit namespaces report "unused import"**: `import task`, `import os`,
    `import errors`, `import strings` are errors ("unused import") because those
    namespaces are implicit, while `println` is unknown without `import io`.
    Either accept (and ignore) an explicit import of an implicit namespace, or
    make `io` implicit too. Found 2026-10-25 while re-verifying this file.

## Verification (2026-10-25)

Every open item above was re-checked on HEAD 6f25c88 with a small program
(release build). Items 6, 7, 8, 9, 10, 13, 14, 15, 18, 22 and 24 are fixed
and marked so; 5, 23, 28 (sync frames below a task; `sql.DB/Tx` without
`Drop`) and 29 still hold; 27 has met its own precondition. Of item 22,
`strings.Builder.WriteRune` is still missing (see 36). `defer f.Close()` on a
`void!` close (38) is rejected by design, with a message that proposes a
closure. 42 and 44 are runtime bugs (a losing `Recv` in `task.Any` swallows
the next `Send`; arenas crash in `zell_retain` past ~12 000 objects).

## Found while fixing the items above (2026-10-25)

53. ❌ **A statement after a static `if` whose then-branch returns is still
    "reachable"**: `if Kind == Struct { return "ok" }; reflect.Error(...)`
    fires the error for valid types; only an explicit `else` works.
    Reachability after a folded static `if` should treat the fall-through as
    dead when the taken branch terminates.
54. ❌ **`Diagnostic::related` is not forwarded to LSP `relatedInformation`**
    (added for `reflect.Error` "instantiated here" notes, item 34).
55. ❌ **Unused-import tracking has one flag per import**: `import crypto`
    used only through a child namespace (`hash.X`) and similar parent-import
    cases may still be reported as unused; not verified, worth a spec test.
56. ❌ **`task.WithTimeout(ms, f)` is easy to misread as a delayed start**:
    `WithTimeout(60, () => ch.Send(x))` on a buffered channel runs at once.
    Documentation gap: the async guide should show "delay an action" as
    `await task.Delay(ms)` in a spawned function and name the difference.

## Kitchen-sink program (2026-10-25)

Found while writing `examples/kitchen_sink.zl` (about 1500 lines, one file:
an order service with config parsing in JSON, TOML and YAML, validation
through attributes, generic pipelines, async workers, an LRU cache,
reports). Each item has a minimal repro. The workaround is what the program
uses now. Open items this program confirmed again: **5/32** (`async func`
in an interface; `Notifier` returns `Task<int>` from a private async
function), **12** (`math.Min/Max` take only floats; `minInt`/`maxInt`
written by hand), **33** (see 67), **46** (see 78).

57. ✅ **Fixed (2026-10-25):** **bug:** **writing into the slice from `reflect.TypesWith<A>()` crashes
    the process (SIGSEGV)**. The slice points at the read-only static table,
    so `slices.SortBy` crashes, and so does a plain `ts[0] = ts[1]`.
    ```zell
    var ts = reflect.TypesWith<job>()
    slices.SortBy(ts, (t *reflect.TypeInfo) => t.Attribute<job>().order)  // exit 139
    ```
    Workaround: `slices.Clone(reflect.TypesWith<handler>())`. **Decision:**
    `TypesWith` returns a fresh heap slice. The table can stay static, and
    the copy costs one allocation per call. The other choice is a slice
    flagged as read-only, so writes panic with `cannot modify a reflection
    table`. Add a spec test that sorts the result.
    **Cause and fix:** the intrinsic returned `const_slice`, a slice over an
    internal constant global placed in read-only memory, so any store
    faulted. `TypesWith<A>()` now allocates a fresh slice per call
    (`zell_slice_new_ops`, the tables stay static and immortal); nothing is
    emitted when it is unused. Spec test `reflection/types_with_sort`; the
    `slices.Clone` workaround is gone from `examples/kitchen_sink.zl`.
58. ✅ **Fixed (2026-10-25):** **bug:** **a real SIGSEGV (as in 57) prints nothing and loses all
    buffered stdout**: the ~90 lines the program had already printed were
    gone, with no message and exit code 139. A nil dereference is caught
    and reported properly, but its debug backtrace starts with a raw
    `…/prog(zell_panic_nil+0x12)[0x4684c2]` line before `r_nil.zl:9 main`.
    **Decision:** add a SIGSEGV/SIGBUS handler in `runtime/src/platform/`
    that flushes stdout and prints `fatal error: unexpected signal SIGSEGV`
    plus the backtrace (Go prints the same), then exits with status 2.
    Hide `zell_panic_*` frames from backtraces.
    **Cause and fix:** the runtime had no handler for real faults (only
    compiled nil/bounds checks panic), so the default action killed the
    process before stdio flushed; the panic backtrace skipped a fixed number
    of frames, which left `zell_panic_nil` (it calls `zell__panicf`)
    visible. Now `zell__plat_runtime_init` installs a
    SIGSEGV/SIGBUS/SIGILL/SIGFPE handler on a per-thread alternate signal
    stack (`platform/posix.c`; Windows: a vectored exception handler for
    faults in the executable plus an unhandled-exception filter in
    `platform/win32.c`); it flushes stdout, prints `fatal error: unexpected
    signal SIGSEGV`, the fault line and the backtrace from the faulting
    context, exit code 2 (`zell__fatal_signal` in `panic.c`, ABI.md 8.3,
    DESIGN.md «Паника»). Backtraces drop `zell_panic*`/`zell__panic*` frames
    by name. Tests: spec `panics/fatal_stack_overflow`,
    `panics/nil_backtrace_frames` (new `// no-stderr:` directive),
    `compiler/tests/fatal_signal.rs` (unsafe read of address 8 after
    buffered output) and `cross.rs::windows_fatal_access_violation` under
    Wine. Wine itself kills a process on stack overflow before any handler
    runs, so that case is checked on Linux only.
59. ❌ **bug:** **methods of a generic enum cannot be called**: "cannot call
    method Or on value of type Opt<int> … methods exist only on structs,
    enums and interfaces".
    ```zell
    enum Opt<T> {
        Some(v T)
        None
        func Or(d T) T { switch this { case Opt.Some(v): return v
                                       case Opt.None: return d } }
    }
    println(Opt<int>.Some(3).Or(0))
    ```
    Workaround: a free generic function `orElse<T>(l Lookup<T>, fallback T)`.
    **Decision:** methods of a generic enum get the same instantiation path
    as methods of a generic struct. The tour says enums take type
    parameters and methods.
60. ❌ **bug:** **`var (a, b) = m[k]` with a 2-tuple value type means comma-ok,
    not destructuring**: with `m map[string](int, int)`, `a` is the tuple and
    `b` is the `bool`, and the error only shows up later ("(int, int) cannot
    be printed").
    ```zell
    var m = map[string](int, int){"a": (1, 2)}
    var (lo, hi) = m["a"]          // lo: (int, int), hi: bool
    ```
    Workaround: `var pair = m[k]; var (lo, hi) = pair`. **Decision:** a
    destructuring with N names over `m[k]` destructures the value when V is
    an N-tuple. It means comma-ok only when N == 2 and V is not a 2-tuple.
    An N-tuple V with N+1 names gives the elements plus `ok`. Write this
    rule in DESIGN.md next to comma-ok.
61. ❌ **bug (diagnostic):** **`json.Parse` without `import encoding.json`
    names an internal struct**: "Parse is not a static method of struct
    json", and for TOML "…of struct tomlAttr". `strings.`/`fmt.` work
    without an import, so a user expects `json.` to work too. **Decision:**
    when `X.Name` resolves to an attribute struct and a std namespace
    `….X` exports `Name`, report `json here is the @json attribute; add
    "import encoding.json" to call json.Parse`. Never print the `tomlAttr`
    / `yamlAttr` names. (Item 10 fixed the case with the import.)
62. ❌ **bug (diagnostic) / inconsistency:** **a map literal through a type
    alias does not parse** (`type Inventory = map[Sku]int;
    Inventory{"apple": 10}` gives "expected identifier, found string
    literal"), and the parser then reports every following statement of the
    function as "expected declaration". Struct literals work through an
    alias (`P{X: 1}`), and the tour says aliases work in literals.
    Workaround: `map[Sku]int{…}`. **Decision:** `Alias{k: v}` over a map
    alias is a map literal. The parser can accept `name { expr : expr }`
    and let sema decide whether it is a struct or a map. Recover at the
    closing `}` so one bad literal does not report the rest of the
    function.
63. ❌ **bug (diagnostic):** **follow-up errors mention `<invalid>`**: "value
    of type (<invalid>) cannot be printed", "cannot use func(int) <invalid>
    as func(int) void!", "cannot return (<invalid>, bool) …". **Decision:**
    an expression whose type contains `<invalid>` produces no further
    diagnostics.
64. ❌ **inconsistency:** **`println` and `fmt.Format` ignore a `String()
    string` method** ("value of type Money cannot be printed") even though
    `fmt.Stringer` exists (DESIGN.md mentions `implements fmt.Stringer`).
    Workaround: `.String()` written at every call site (about 15 times in the
    program). **Decision:** like Go, `println`, `print`, `fmt.Format` `{N}`
    and `fmt.Sprintf` `%v`/`%s` call `String()` on structs and enums that
    have it (static dispatch, monomorphized). Write it in DESIGN.md and
    `examples/README.md` output rules.
65. ❌ **inconsistency:** **`strings.Trim(s)` is Go's `TrimSpace`**, while
    `TrimLeft(s, cutset)` / `TrimRight(s, cutset)` take a cutset, and there
    is no `TrimSpace`. A Go user's `strings.Trim(s, ",")` fails with
    "expected 1, got 2". **Decision:** add `TrimSpace(s)` and make
    `Trim(s, cutset)` take a cutset as in Go. If that breaks too much,
    keep `Trim(s)` and add `TrimSpace` plus `TrimChars(s, cutset)`.
66. ❌ **spec gap:** **every attribute field without a default is a
    required argument**: `@rule(min: 1)` on a 5-field attribute gives four
    "missing argument" errors. DESIGN.md «Атрибуты» only says that applied
    attributes use "the arguments and the field defaults". Workaround: `= 0`
    / `= ""` / `= false` on every field. **Decision:** like C# named
    arguments, an omitted argument takes the field's default, or else its
    zero value. A field that must be given is marked
    (`@required name string`) if that is ever needed.
67. ❌ **ergonomics (extends 33):** **the body of a compile-time
    `reflect.Fields<T>()` loop is unrolled per field, but nothing in it
    branches on the field's type**: (a) `switch f.Get(v) { case string s:
    … }` is rejected with "type pattern string needs an interface or error
    subject, found string", once per field and case; (b) `if f.Type.Kind
    == reflect.Kind.String { f.Get(v) == "" }` is still checked for `int`
    fields; (c) there is no `f.Set(&v, x)`, so `@rule(default:)` cannot be
    applied.
    ```zell
    for f in reflect.Fields<T>() {
        switch f.Get(v) { case string s: … case int n: … }   // rejected
    }
    ```
    Workaround: `var value any = f.Get(v)` and a run-time type switch (it
    boxes). Defaults are only listed, never applied. **Decision:** inside an
    unrolled `Fields<T>` body, (a) a type switch on a value of known
    static type folds to the one matching case (a mismatched case is dead,
    not an error); (b) conditions on `f.Type.Kind`, `f.Type ==
    reflect.TypeOf<X>()` and `f.Attribute<A>() != nil` are static `if`
    conditions; (c) add `f.Set(target *T, value)` with the field's static
    type.
68. ❌ **spec gap:** **there is no way to pass a slice to a variadic
    parameter**, so `task.All` cannot wait on a number of tasks known only
    at run time ("task.All arguments must be tasks, found []Task<int>"),
    and `append(xs, ys...)` does not parse.
    Workaround: `for t in pending { out = append(out, await t) }`, which
    loses the fail-fast cancellation of `All`, and `slices.Concat`.
    **Decision:** add Go's spread `f(xs...)` for variadic parameters, and
    have `task.All` of a `[]Task<T>` give `Task<[]T>` (`[]T!` when T is
    fallible, with the first error cancelling the rest).
69. ❌ **ergonomics:** **fixed arrays have no literal**: `var a [3]string =
    ["a", "b", "c"]` is "cannot assign []string to variable of type
    [3]string", and the Go form `[3]string{…}` does not parse. Workaround:
    one assignment per element (the `labels` array). **Decision:** a slice
    literal of exactly N constant-length elements converts implicitly to
    `[N]T` where that type is expected. A length mismatch is an error
    `array literal has 2 elements, want 3`.
70. ❌ **ergonomics:** **tuples cannot be printed and have no element
    access**: `println(t)` fails, and so do `t.0` and `t._0` (reflection
    names them `_0`, `_1`). Only destructuring works. **Decision:** print
    tuples as `(1 x)` (Go-like, space-separated), and allow `t.0`, `t.1`
    as in Rust/Swift.
71. ❌ **ergonomics:** **`char` has no arithmetic**: `c - 'a'` ("operator -
    is not defined on char") and `'a' + 2` both fail, so a Caesar shift is
    `char(int('a') + (int(r) - int('a') + k) % 26)`. **Decision:** like Go
    runes, `char - char` gives `int`, and `char ± int` gives `char`
    (checked for a valid code point in debug builds). Comparisons already
    work.
72. ❌ **ergonomics:** **`nil` in a tuple `return` is "use of untyped nil"**
    even though the result type `(*Order, bool)` is known. Workaround: `var
    none *Order = nil; return (none, false)` (in the iterator `Next`).
    **Decision:** a tuple literal in a `return`, an assignment or an
    argument position takes its expected type element by element, so `nil`
    gets typed.
73. ❌ **ergonomics:** **there is no value-producing error or conditional
    expression**. A `catch` block cannot end with a value (`catch err {
    println(err); 0 }` gives "expression is evaluated but not used"), and
    `a ? b : c` gives "expected `,` or `)`" because `?` parses as
    propagation. Workaround: `var x = default; try { x = f()? } catch err
    { log }` (twice in `demoAsync`), and `if` statements instead of the
    ternary. **Decision:** a `catch` block may end with an expression of the
    `try`'s type, which becomes its value (Kotlin). No ternary; instead,
    report "Zell has no ?: operator; use if/else" when `?` is followed by
    an expression and `:`.
74. ❌ **spec gap:** **no labeled `break`/`continue`**. DESIGN.md «switch»
    lists it as an open question, and `break` inside `case` cannot leave
    the loop. A nested search returns from a helper (`firstSharedSku`)
    instead. **Decision:** Go labels, `name:` on the line before `for`, plus
    `break name` / `continue name`. This also answers the `case` question.
75. ❌ **ergonomics:** **`i++` gives "expected expression, found `+`"**, then
    "expected expression, found `var`" on the next line. **Decision:**
    either add Go's `i++` / `i--` statements (not expressions), or report
    `Zell has no ++; write i += 1` and recover.
76. ❌ **spec gap:** **method values are not supported** ("method C.Inc must
    be called"), while plain functions are values (`.Then("limits",
    ValidateOrder)`). Workaround: `() => c.Inc()`. **Decision:** `x.M`
    without a call is a closure that captures `x` (a pointer receiver
    keeps the object alive, as Go does), and `T.M` is a function whose
    first parameter is the receiver.
77. ❌ **ergonomics (diagnostic):** **an attribute after the field (Go tag
    position)** gives "expected end of line, found `@`". **Decision:** keep
    the C# position, and report `attributes go before the field: @json(…)
    OrderId int`.
78. ❌ **ergonomics (variant of 46):** **an expression-bodied lambda cannot
    pass a `T!` through**: `(x *Order) => ValidateOrder(x)` where
    `func(*Order) void!` is expected gives "result of type void! must be
    handled" and "cannot use func(*Order) <invalid>". Workaround: pass the
    function itself, or use a block body with `?`. **Decision:** the
    expression body of a lambda whose expected result is `T!` may be a `T!`
    (it is the lambda's result, not an ignored value). This does not
    change the `return f()` rule of 46.
79. ❌ **ergonomics:** **a custom iterator cannot yield pointers to its
    elements** (`Next() (*Order, bool)` returning `&this.book.orders[i]`
    is "cannot keep interior pointer"), so a collection cannot offer
    `for o in book { o.Status = … }`. Workaround: store `[]*Order`.
    **Decision:** keep the rule, and document "iterate by index or store
    pointers" in memory.md. Or let `for` over a slice field bind `&x` as
    a loop-local interior pointer (`for &o in this.orders`).
80. ❌ **compile time/size note:** `zell build` of the 1500-line file takes
    1.3 s with a 595 MB peak RSS (debug; `hello.zl`: 0.36 s, 86 MB), and
    `--release` takes 4.2 s / 310 MB. The binary is 4.4 MB debug and 978 KB
    release (`hello`: 46 KB release). The debug build uses about twice the
    memory of the release build, which is worth profiling before files get
    larger. No decision needed: record it in `benchmarks/README.md` if it
    is tracked.
