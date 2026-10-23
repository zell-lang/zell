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
7. **Suspicious trace locations** in release builds (`web.Scope.Get` pointing
   at a test file line).

## Standard library
8. **No `os.Environ()`** — zconf reads `/proc/self/environ` (Linux-only, misses
   `os.Setenv` after start).
9. **`errors.Is`/`As` ignore a struct's `Unwrap()` method** — only `errors.Wrap`
   chains are followed.
10. **The built-in attribute name `json` shadows the `encoding.json` namespace**
    inside a package (`json.Value` → "Value is not a static method of struct
    json"); same for `yaml`, `toml`. Users write `encoding.json.Value`.
11. **`toml.Value`, `yaml.Value`, `json.Value` expose different method names**
    for the same idea (`IsTable`/`IsMap`/`IsObject`, `IsArray`/`IsSeq`).
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
17. **Handler errors become Problem Details at the endpoint**
    (`std/web/web_app.zl:581`, `writeError` at `:1079`): no middleware or hook
    sees the original error, so domain-error → status mapping lives in every
    controller action; and unknown errors return their message in `detail`
    (500) — an information leak. Wanted: an error-mapping hook/middleware
    (`app.MapError<ErrNotFound>(404)`) and a generic 500 body by default.

Not an obstacle but noted: OpenAPI 3.1 generation, `web.TestServer`, DI,
request id / tracing / metrics middleware all worked as documented.

## Added while writing `example-cli` and `example-worker` (same day)
18. **`zell test` leaks an internal symbol**: in a package with `src/main.zl` it
    prints `warning[L0007]: function zellUserMain__ is never used` — the lint
    must skip compiler-generated names.
19. **`cli.App` cannot be redirected or given exit codes**: `Run` returns only
    0/1/2, help and group text go to stdout via `io.print`
    (`std/cli/cli.zl:199,274,315`), a command error is always status 1 plus a
    printed `error: …`. Wanted: `App.Out/Errors` writers and typed exit codes.
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

25. **Panic backtraces show symbols+offsets, not source lines**
    (`zell_app__main+0x172` in debug, bare addresses in `--release`), while Go
    prints `main.go:10`. Wanted: a compact pc→file:line table in the binary
    (works stripped, no DWARF reader at run time), used by the panic printer and
    by error traces.

26. **Known red test: `compiler/tests/reflection.rs::no_reflection_without_use`.**
    Every program, even `println(1)`, loads `reflect` and type-checks ~755
    stdlib instances. Root cause (traced 2026-10-23): stdlib interfaces are
    registered eagerly from the module summaries and `resolve_signatures`
    resolves every stdlib interface method signature up front;
    `net.http.Handler.Serve(req *Request, …)` forces the `Request` template,
    whose fields force `RequestBody`, `net.sock`, `time`, `log` … down to
    `strings.Builder` and `reflect`. Fix: resolve stdlib interface signatures
    (or struct fields of forced templates) lazily, on first use of the
    interface. `zell check hello.zl` still takes 0.05 s, so this is about
    pruning/intent, not speed.

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
