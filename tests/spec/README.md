# Zell specification corpus

Small, focused programs that define the expected behaviour of the language,
written TDD style: features that are **not implemented yet** have tests too.
Those are marked `pending`; they must fail today and start passing as the
compiler grows. The corpus follows `DESIGN.md`; where it is silent it
follows `examples/README.md`, then Go. Every detail the corpus had to
invent is listed under [Decisions encoded by the spec](#decisions-encoded-by-the-spec)
so that `DESIGN.md` can be updated.

## Running

```
cargo test -p zell --test spec                          # whole corpus, one test per area
cargo test -p zell --test spec spec_errors              # one area
SPEC_FILTER=errors/trace cargo test -p zell --test spec # tests whose path contains the text
SPEC_VERBOSE=1 cargo test -p zell --test spec -- --nocapture   # every result + per-area counts
scripts/spec-status.sh                                    # markdown status table (below)
```

The runner is `compiler/tests/spec.rs`. It builds each test with the
`zell` binary (`zell build <path> -o <exe>`, run from the repository root
with a path relative to it), runs the program with `ZELL_NO_BACKTRACE=1`,
and compares stdout byte for byte with the `.out` file and the exit code with
the expected one. Each area prints a line like
`spec errors: 27 tests (7 compile-fail): 19 passing, 8 pending, 0 unexpected`
(visible with `--nocapture`). The whole corpus runs in about 5 seconds.

## Layout

```
tests/spec/<area>/<name>.zl     one program (namespace spec.<area>.<name>)
tests/spec/<area>/<name>.out    its exact expected stdout (not for compile-fail tests)
tests/spec/<area>/<name>/       a multi-file program: all .zl files are compiled together,
    *.zl                        main is taken from the root namespace; exactly one .out
    <anything>.out              file, none for a compile-fail test
```

Areas: `basics control structs interfaces enums errors generics memory async
attributes reflection stdlib namespaces switch closures pointers`, and the
second wave `interfaces2 generics2 memory2 errors2 async2 strings2 diagnostics
programs attributes2 platform stdlib2 stdlib3`, then `operators` (operator
overloading and its compile errors) and `iteration` (the `for ... in`
protocol) (`stdlib2`: the tier 0/1 stdlib of docs/STDLIB.md, one or more files per namespace named `<ns>_*.zl`; `stdlib3`: the tier 2 stdlib (regex, compress, archive, encodings, templates, mime, url, text), `*_corpus` tests generated from Go / PyYAML / tomllib oracles; `stdlib4`: the tier 3 stdlib (`database.sql` with the SQLite driver and a fake in-test driver, `database.postgres` against an in-process mock server, `net.mail` against an in-process SMTP server, `crypto.kdf/password`, P-384, AES-CBC, RSA-OAEP, the zstd encoder, TLS with P-384; the real-PostgreSQL test is `compiler/tests/database.rs`); `stdlib5`: the tier 3b infrastructure clients (`data.redis` codec and client against in-process RESP mock servers incl. Cluster MOVED/ASK and Sentinel switch-over, `data.kafka` record batches byte-exact against the Java client and a mock broker with groups/SASL/TLS, `cloud.s3` SigV4 vectors and an in-process S3 server that verifies signatures, `cloud.blob`; the real-server tests are `compiler/tests/{redis,kafka,s3}.rs`); `diagnostics`: one compile-fail test per sema
diagnostic, see [Diagnostics coverage](#diagnostics-coverage); `programs`:
larger programs mixing features; `web`: the `web` framework of
docs/FRAMEWORK.md section 2 — routing, binding, validation, DI scopes,
middleware order, JWT auth, Problem Details, results, the OpenAPI document,
configuration, and the compile errors of route tables and DI, all through
the in-memory `web.TestServer`; `data`: the `data` ORM of docs/FRAMEWORK.md
section 3 on SQLite in memory — model conventions and DDL for both providers,
change tracking with minimal UPDATEs (through `LogTo`), queries and their
`ToSql()` for SQLite and PostgreSQL, projections, aggregates, includes,
concurrency tokens, transactions, set-based statements and upserts,
migrations as code, and one compile-fail test per untranslatable expression
or model error; the real-PostgreSQL and migrations-CLI tests are
`compiler/tests/data.rs`; `memory3`: reference cycles — the W0001
warning, `@allowCycle`, `--deny-warnings`, and the cycle collector of
DESIGN.md "Циклы": parent/child, closure-captures-self and map/slice
cycles freed with their `Drop` order, cycles kept by outside references,
weak graphs and arenas left alone, resurrection from `Drop`, the auto,
manual and off modes and the leak report, `@acyclic` and its leak-check
verification). A new area
directory must also be added to `AREAS` and the `areas!` list in
`compiler/tests/spec.rs` (`areas_are_complete` fails otherwise).

## Directives

Directives are the leading `//` comment lines, before `namespace` (or
before an attribute on the namespace). In a multi-file test they are read from
the first `.zl` file by name; the other files repeat them for readability.

| Line | Directive | Meaning |
|------|-----------|---------|
| 1 | `// test: <description>` | what the program checks (runnable tests) |
| 1 | `// error: <keyword>` | compile-fail test: the build must fail **and** stderr must contain `<keyword>` (case-sensitive substring) |
| 2 | `// status: passing` / `// status: pending` | required, see below |
| 3+ | `// exit: N` | expected exit code (default 0); e.g. 2 for a panic |
| 3+ | `// args: a b` | program arguments, split on whitespace |
| 3+ | `// run-flags: --leak-check` | run with `zell run <path> <flags> -- <args>` instead of build + exec |
| 3+ | `// stderr: <text>` | stderr must contain `<text>`; otherwise stderr is not checked |
| 3+ | `// warning: <text>` | the build's stderr must contain `<text>` (e.g. a W0001 warning); repeatable |
| 3+ | `// no-warning: <text>` | the build's stderr must not contain `<text>`; repeatable |
| 3+ | `// env: K=V` | environment variable of the program run (e.g. `ZELL_CYCLES=manual`); repeatable |
| 3+ | `// build-flags: --x` | extra `zell build` flags, also for compile-fail tests (e.g. `--deny-warnings`) |
| 3+ | `// os: linux macos` | run only on these host systems (Rust `std::env::consts::OS`: `linux`, `macos`, `windows`); skipped and counted as "for other systems" elsewhere. For tests whose expected output is the host's (`os.Name`, `@cfg(os: ...)`, the root user's home) |

Output conventions are those of `examples/README.md` ("Output format"):
`println` separates arguments with one space, floats print like Go's `%v`
(exponent form when the exponent is < -4 or >= 6), slices print as `[1 2 3]`.

## Status: passing and pending

- `passing`: the test must pass. A failure is reported with the diff.
- `pending`: the test must currently **fail** (wrong output, wrong exit code,
  build error, or for a compile-fail test: the build succeeds or fails with a
  different message). When it starts passing the runner fails with
  `tests/spec/x.zl now passes: change status to passing`.

Flipping a test: implement the feature, run
`SPEC_FILTER=<area>/<name> cargo test -p zell --test spec`, change line 2 to
`// status: passing` (in every file of a multi-file test), and regenerate the
table below with `scripts/spec-status.sh`. Never edit a `.out` file to match a
buggy implementation; if the expectation itself is wrong, fix it together with
`DESIGN.md` and the decision list below.

## Current status

Generated by `scripts/spec-status.sh`:

| Area | Tests | Passing | Pending | Compile-fail |
|------|------:|--------:|--------:|-------------:|
| async | 24 | 24 | 0 | 3 |
| async2 | 17 | 17 | 0 | 0 |
| attributes | 15 | 15 | 0 | 8 |
| attributes2 | 17 | 17 | 0 | 9 |
| basics | 34 | 34 | 0 | 6 |
| closures | 8 | 8 | 0 | 0 |
| control | 18 | 18 | 0 | 1 |
| data | 23 | 23 | 0 | 15 |
| diagnostics | 177 | 177 | 0 | 177 |
| enums | 11 | 11 | 0 | 2 |
| errors | 27 | 27 | 0 | 7 |
| errors2 | 16 | 16 | 0 | 2 |
| generics | 20 | 20 | 0 | 2 |
| generics2 | 31 | 31 | 0 | 11 |
| interfaces | 12 | 12 | 0 | 2 |
| interfaces2 | 29 | 29 | 0 | 9 |
| iteration | 8 | 8 | 0 | 4 |
| memory | 13 | 13 | 0 | 2 |
| memory2 | 34 | 34 | 0 | 12 |
| memory3 | 16 | 16 | 0 | 1 |
| namespaces | 15 | 15 | 0 | 5 |
| operators | 29 | 29 | 0 | 16 |
| platform | 12 | 12 | 0 | 1 |
| pointers | 12 | 12 | 0 | 1 |
| programs | 6 | 6 | 0 | 0 |
| reflection | 17 | 17 | 0 | 1 |
| stdlib | 77 | 77 | 0 | 0 |
| stdlib2 | 37 | 37 | 0 | 3 |
| stdlib3 | 38 | 38 | 0 | 5 |
| stdlib4 | 9 | 9 | 0 | 0 |
| stdlib5 | 7 | 7 | 0 | 0 |
| strings2 | 12 | 12 | 0 | 0 |
| structs | 19 | 19 | 0 | 4 |
| switch | 10 | 10 | 0 | 2 |
| web | 22 | 22 | 0 | 12 |
| **total** | **872** | **872** | **0** | **323** |

## Decisions encoded by the spec

Places where `DESIGN.md` (and `examples/README.md`) is silent or ambiguous
and the corpus picked an answer. Each is followed by the tests that encode it.

### Basics and control flow

1. **Unused imports are compile errors** (like Go; `DESIGN.md` only says so for
   variables). Keyword `unused import`. — `basics/unused_import`
2. **Constant expressions may size arrays**: `var a [N]int` with `const N`
   (the grammar only allows an integer literal). — `basics/const_array_length`
3. **`float32` prints the shortest decimal that round-trips as float32**, like
   Go (`float32(0.1)` prints `0.1`, `float32(1)/3` prints `0.33333334`).
   — `basics/float32_values`
4. **Integer division by zero panics** (exit code 2, message contains
   `divide by zero`); `MinInt / -1` wraps to `MinInt` and `MinInt % -1` is 0
   (Go). Integer overflow wraps in every width. — `basics/division_*`,
   `basics/int_overflow_wraps`
5. **Strings**: `s[i]` is the byte at offset `i` (type `byte`, prints as a
   number); out of range panics with exit code 2; `s[a..b]`, `s[..b]`, `s[a..]`
   are byte-range substrings. `for i, ch in s` gives the byte offset of each
   code point. `int('A')` is the code point. — `basics/string_*`,
   `control/for_string_runes`
6. **Parallel assignment** `a, b = b, a` (right side evaluated first) and
   **tuple assignment** `(a, b) = f()` to existing variables (the grammar has
   neither; `DESIGN.md` only shows `var (x, y) = f()`). — `basics/multi_assign_swap`,
   `basics/tuple_assign`
7. **Redeclaring a name in the same block is an error** (keyword `redeclared`);
   shadowing in an inner block is fine. Reading `_` is an error.
   — `basics/redeclare_same_scope`, `basics/shadowing`, `basics/underscore_not_readable`
8. **Range bounds are evaluated once**, before the first iteration
   (`for i in 0..n` does not see later changes of `n`). — `control/for_range_bounds_once`
9. **`break` inside a `switch` case leaves the switch** (Go semantics), while
   `continue` continues the enclosing loop. Since Zell has no labels, a loop
   cannot be left from inside a switch except by `return`; see
   ambiguities. — `control/break_inside_switch`
10. **`print(a, b, ...)` follows Go's `fmt.Print`**: a space is added between
    two operands only when neither is a string. — `stdlib/print_spacing`
11. **Defers inside an `async func`** run when its body finishes (after all its
    awaits), before the awaiting caller resumes (open question in `DESIGN.md`).
    — `control/defer_in_async`
12. **Duplicate constant cases in a switch are a compile error** (Go; keyword
    `duplicate case`). — `switch/duplicate_case_error`

### Structs, interfaces, enums, pointers

13. **Struct values cannot be compared with `==`** unless the struct has
    `func Equals(other T) bool` (operator overloading, decision 69)
    (keyword `operator == is not defined on`; the error now has `help:
    declare func Equals(...)`); Go would allow it for comparable
    structs. — `structs/struct_equality_rejected`, `operators/equals_method`
14. **Field defaults** (`Name string = "x"`) are allowed in every struct, not only
    in `@attribute` structs; they apply to literals that omit the field **and**
    to `var x T` without initializer. — `structs/field_defaults`
15. **Private members across namespaces**: the diagnostic contains `private`
    for fields, methods and types. — `structs/private_*`, `namespaces/private_type_error`
16. **Nil dereference** (field read, method reading `this`, `*p`) panics with
    exit code 2 and a message containing `nil`. — `structs/nil_*`, `pointers/nil_deref_panic`
17. **`println` of an `any`** prints its dynamic value. — `interfaces/any_println`
18. **Interface to interface**: an interface value converts implicitly to an
    interface whose methods are a subset; a type switch case may name an
    interface type (`case Mover m:`). — `interfaces/interface_to_interface`
19. **Interface embedding** is written as the bare interface name inside the
    body (`interface ReadWriter { Reader; Writer }`, one per line). — `interfaces/interface_embedding`
20. **Enums**: the zero value of a plain enum is its first variant; enums may
    declare methods after their variants (`this` is the value); plain enums can
    be map keys; `_` ignores a field in a variant pattern. — `enums/*`
21. **Interior pointers kept in variables** (`var p = &xs[1]`) are allowed, as in
    Go: the local pins the owner (the slice's backing array, the pointer of
    `&p.f`, the cell of `&s.f`), so reassigning the slice variable or
    dropping the pointer does not invalidate it. Such a variable can be
    read through, compared, reassigned and passed to non-escaping
    parameters, but not copied elsewhere, returned, captured or have its
    address taken (DESIGN.md "Память"). — `pointers/pointer_to_slice_element`,
    `pointers/interior_pointer_local`, `pointers/interior_pointer_param`,
    `memory2/interior_pointer_closure_error`, `memory2/interior_pointer_local_return_error`

### Errors

22. **`defer f()` where `f` is fallible is a compile error** (`must be
    handled`), consistent with "every `T!` must be handled"; handle it in a
    deferred closure instead. — `errors/defer_unhandled_error`, `errors/error_in_defer`
23. **`errors.Trace(err)` text** (from the example in `DESIGN.md`, made exact):
    - It returns only the frame lines (the message is printed separately with
      `println(err)`), joined by `\n`, no trailing newline.
    - First line `  created at   <loc>  <func>`, then one
      `  returned via <loc>  <func>[  <context>]` per `?` the error passed,
      oldest first, and finally one for the `try` that caught it (the
      `main` line in `DESIGN.md`'s example is such a catch site).
    - `<loc>` is `<file>:<line>` with the file path **as given to the
      compiler** (the runner passes `tests/spec/...` relative to the repository
      root), padded to the longest location; `<func>` is padded to the longest
      function name when a context follows; columns are separated by two
      spaces; trailing spaces are trimmed. `<context>` is `?("text")` for
      `?("text")`. The line is that of the `error(...)` / `return T{...}`
      expression, of the `?`, or of the `try`.
    - Creation sites (resolved in `DESIGN.md`): `error(...)`, `errors.New`,
      `errors.Wrap` (starts a new trace), a struct converted to `error`, a
      failing stdlib call, and `return err` for an error without a site (so
      a sentinel is "created" at its `return`). A `?` inside a block `try`
      adds the frame of the catch.
    — `errors/trace_basic`, `errors/trace_typed_error`
24. **`errors.Frames(err)`** returns one record per trace line with fields
    `File string`, `Line int`, `Func string`, `Context string` (the text inside
    `?("...")`, empty otherwise). — `errors/frames_length`
25. **`func main() void!` and `async func main() void!`**: a returned error is
    printed to stderr (its message) and the exit code is 1.
    — `async/main_error_exit`, `async/async_main_error_exit`
26. **Block `try`**: variables declared in the block are not visible after it
    (`unknown identifier`). — `errors/block_try_*`

### Generics

27. **`Ordered`** is a builtin constraint (integers, floats, strings; supports
    `< > <= >= == +`). Generic bodies are checked against their constraints
    (Go style), so operations must be valid for every allowed type.
    A violation reports `does not satisfy`; a wrong number of type arguments
    reports `type argument`. `Comparable` (alias `Equatable`) is the builtin
    constraint for `==`/`!=` (and map keys); interfaces constrain by method
    set. A generic body is checked once with abstract type parameters; each
    instantiation is then checked with concrete types (errors carry the note
    `in instantiation of F<T>`). — `generics/*`
28. **Instantiation syntax**: `F<int>(x)`, `F<int>` as a function value,
    `Stack<int>{}`, `Pair<B, A>{...}`, generic methods `p.Show<string>(x)`,
    generic enums built as `Option<int>.Some(1)` / `Option<int>.None` and
    matched without type arguments (`case Option.Some(v):`). A line ending in
    `>` (e.g. `var nf NotFound<string>`) ends the statement.
29. **`collections` API**: `List<T>{}` with `Add`, `Len`, `Get(i)`,
    `Set(i, v)`, `Contains`, `Remove(i)`, `Items() []T`; `Map<K, V>{}` with
    `Set`, `Get` (zero value when missing), `Has`, `Remove`, `Len`,
    `Keys() []K` in insertion order; `Set<T>{}` with `Add` (idempotent), `Has`,
    `Remove`, `Len`, `Items()` in insertion order; `Queue<T>{}` / `Stack<T>{}`
    with `Push`, `Pop() T!` (fails when empty), `Len`. Also (not used by the
    corpus): `New()` on every type, `List.Insert/IndexOf/Clear`,
    `Map.Values/Clear`, `Set.Clear`, `Queue/Stack.Peek() T!`; `Map<K, V>` and
    `Set<T>` require `Comparable` keys, `List.Contains/IndexOf` require it at
    the call; `for k, v in m` / `for x in l` iterate in insertion order.
    — `generics/collections_*`, `stdlib/collections_queue_stack`

### Memory

30. **Drop order**: stack values are dropped at the end of their block in
    reverse declaration order; a struct's own `Drop` runs before its fields'
    `Drop`s, which run in declaration order (Rust order). Heap objects are
    dropped when the last reference goes away, including on reassignment.
    — `memory/drop_*`
31. **Returning a local `Drop` value moves it** to the caller (last use is a
    move), so it is dropped exactly once by the caller. — `memory/drop_move_on_return`
32. **Weak references** are created by assigning a `*T` to a `weak *T` and read
    by assigning back to a `*T` (nil once the target is freed).
    — `memory/weak_becomes_nil`, `memory/parent_child_weak`
33. **`zell run --leak-check`** prints `leak: N objects` on stderr and exits
    with code 66 when objects are still alive; the report names the field that
    closes a cycle as `Type.Field`. The cycle tests run with
    `ZELL_CYCLES=manual`, since by default the cycle collector frees those
    cycles before the report. — `memory/leak_check_*`
34a. **Cycle collector** (DESIGN.md "Циклы"): `runtime.CollectCycles()`
    returns the number of objects freed, counting slice backings, maps,
    closure environments and captured cells; `Drop` methods of a garbage
    cycle run in the order its possible roots were recorded (locals are
    released last declared first, so the last declared node's `Drop` runs
    first), all before any field is released; a member stored elsewhere by
    a `Drop` survives (and leaks) and no `Drop` runs twice. — `memory3/*`
34. **Arenas** live in `arena`: `arena.New()`, `a.New<T>() *T`,
    `a.Free()` releases everything at once. — `memory/arena_basic`

### Async

35. **`task.Any(t1, ..., tn)`** yields the result of the first task to finish.
    **`task.Channel<T>(capacity)`** creates a channel; `await ch.Send(v)`,
    `await ch.Recv()` returning `T!` (fails once closed and drained),
    `ch.Close()`. **`task.Mutex`**'s zero value is unlocked; `await mu.Lock()`,
    `mu.Unlock()`. **Async lambdas** are written `async func(...) T { ... }`.
    — `async/task_any`, `async/channel_send_recv`, `async/mutex_counter`, `async/async_lambda`
36. **`defer await f()` is rejected** (keyword `defer requires a function call`).
    — `async/defer_await_rejected`
36a. **More of `task`**: `Recv()` fails with `channel closed`; closing wakes
    every waiting receiver, values buffered before `Close` are still delivered;
    a capacity-0 channel hands values over one at a time. `task.WaitGroup`
    (zero value usable) has `Add/Done/Count/Wait`. `task.Spawn(f)` takes a
    `func() Task<T>` (async lambdas and async functions as values) and runs it
    on a worker (`task.Go` for `func() Task<void>`); `task.Yield()` reschedules.
    `await task.Timeout(t, ms)` is a `bool` (true: `t` is done), it does not
    cancel `t`. Context values: `var K = task.Key<T>()`, `task.With(K, v, f)`,
    `task.Get(K) *T` (nil outside), `task.SpawnWith(task.CurrentContext(), f)`;
    spawned and awaited code inherits them, the caller is unaffected.
    — `async/channel_unbuffered`, `async/channel_close_wakes`, `async/waitgroup`,
    `async/task_spawn_yield`, `async/task_timeout`, `async/task_context`

### Attributes and reflection

37. **An attribute on the namespace** is written on the line before
    `namespace`. An attribute-struct argument is written as a struct literal
    `unit{name: "ms"}`. — `attributes/on_namespace`, `attributes/nested_attribute_arg`
38. **`@json`** has the fields `name string`, `omitEmpty bool`, `skip bool`.
39. **`TypeInfo`**: `Name` (`User`, `*User`, `int`, `[]int`, `map[string]int` — Go
    spelling), `FullName` (namespace path + name), `Kind` (a `reflect.Kind`
    enum printing `Struct`, `Enum`, `Int`, `Pointer`, `Slice`, ...), `Fields`
    (declaration order; `Offset` uses C layout with natural alignment, no
    reordering), `Methods`, `Variants` (each with `Name` and `Fields`),
    `Attribute<A>() *A`, `Attributes<A>() []*A` (source order), `Method(name)`
    (nil if absent), `New() any` holding `*T`. `TypeInfo` values are unique per
    type, so `==` compares types; `typeof(x)` of an interface value is its
    dynamic type (of a static type: that type, without evaluating `x`). A
    field written with a builtin alias shows that spelling (`int64`, the
    canonical name of the type is `int`). Methods (and `New`) are kept for
    types named in `reflect.TypeOf<T>()`, a static `typeof`,
    `reflect.Fields<T>()`, or marked `@reflect` (`reflection/reflect_retention`
    sees the methods of a `@reflect` type through `typeof` on an `any`).
    `reflect.Fields<T>()` is only valid as the sequence of a `for` inside a
    generic function, with a struct `T`. — `reflection/*`
40. **`MethodInfo.Call(recv, args...)`** takes the receiver pointer first and
    returns `any!` (the method's error, if it is fallible); `Call<F>()` returns
    a function whose first parameter is the receiver. The receiver may also be a
    struct value (the method then works on a copy); wrong receiver or argument
    types are errors of the call (`reflect: S.M: ...`). **`Value.Set`** with a
    value of the wrong type panics (exit 2) with a message naming the field;
    so does `Get<T>` with the wrong `T`, and `Field` of a missing field.

### Standard library

41. **`fmt.Sprintf`** implements Go's verbs (`%d %5d %-4d %03d %s %v %.2f %x %q %%`).
42. **`strings.Builder{}`** with `Write(s)`, `String()`, `Len()` (bytes).
43. **`fs.ErrNotFound`** is the sentinel matched (via `errors.Is`) by errors
    for missing files; the error message names the path.
44. **`json.Value`** (from `json.ParseValue`): `Get(key)`, `Has(key)`,
    `Index(i)`, `Len()`, `String() string!`, `Float() float!`, `Bool() bool!`,
    `IsNull()`. `yaml`/`toml`/`xml` offer `Parse<T>(text) T!` like `json`.
    `json.Parse<T>` errors read `<path>: expected <what>, found <kind>`
    (`Users[2].id: expected int, found string`; `json: ...` at the top level);
    `json.Format` writes struct fields in declaration order (all of them unless
    `@json(skip: true)`), sorts map keys, and writes nil slices, maps and
    pointers as `null`; a decoded `[]` is an empty non-nil slice and formats
    back as `[]` (examples/README.md decision 22). Syntax errors are
    `json: <what> at offset N` for lexical ones (unterminated string, control
    character, invalid escape / `\u` escape / number / character, number out
    of range) and `json: unexpected end of input`, `json: expected , or ] in
    an array, found <token>` etc. for grammar ones; more than 512 nested
    containers is `json: nesting too deep`. — `stdlib/json_*`
45. **`log`**: `log.Info(msg)`, `log.Warn(msg)` write `INFO msg` /
    `WARN msg` lines to stderr (the default logger: Information and up,
    output template `{Level:u4} {Message:lj}{NewLine}{Exception}`, Serilog's
    4-letter levels `VERB DBUG INFO WARN EROR FATL`). Message templates are
    string literals checked at compile time (placeholder count = argument
    count); level values are `log.Level.X` because `log.Debug(...)` etc. are
    the default logger's functions; `{Message:lj}` writes strings as they
    are, `{Message}` and `{Properties}` quote them (Serilog). — `stdlib/log_*`,
    `diagnostics/log_template_*`
46. **`os.Exit(n)`** flushes stdout and exits immediately; deferred calls do
    not run (Go).
46a. **`net`**: `Read(n)` yields an empty slice and `ReadInto(buf)` 0 at
    end of stream (never an error); `ReadExact`/`ReadLine`/`ReadByte` fail
    with an error wrapping `net.ErrEOF` there. Every operation error is a
    `net.OpError` with Go's text, `<op> <net> <addr>: <cause>`
    (`dial tcp 127.0.0.1:1: connection refused`, `read tcp 127.0.0.1:5:
    i/o timeout`, `accept tcp 127.0.0.1:5: use of closed network
    connection`), and `errors.Is` sees the cause sentinel (`ErrClosed`,
    `ErrTimeout`, `ErrRefused`, `ErrReset` — also for EPIPE —,
    `ErrUnreachable`, `ErrAddrInUse`, `ErrNotFound`); the address is the
    peer for connections and the local one for listeners and unconnected
    UDP sockets. Address errors are `address A: missing port` / `address A:
    invalid port P` (service names such as `http` are ports);
    `ResolveIP("localhost")` is `[127.0.0.1]`; `Listen` resolves host names;
    an empty `Listen` host is dual-stack. Deadlines are `time.Now()`
    milliseconds. Tests whose subject is IPv6 print the same lines when the
    host has no IPv6 (`net_ipv6`); multicast uses the loopback interface.
    `import net` makes `http.` reachable (decision 50).
    — `stdlib/net_*`
46b. **`net.http`**: header names are canonicalised (`content-type` ->
    `Content-Type`) and repeated headers are joined with `, `; the router answers
    unknown paths with `404 Not Found\n` and other methods with 405 plus `Allow`;
    a handler error becomes `500 Internal Server Error: <message>` (`http.Recover`
    logs it and answers without the message); the default `Content-Type` of a
    non-empty body is `text/plain; charset=utf-8`; every server response has a
    `Date` header (Go does the same; `Server{NoDate}` drops it); repeated
    `Set-Cookie` values are joined with `"\n"` in the header map (they contain
    commas). The client pools keep-alive connections (`Transport`; no more
    `Connection: close`), retries an idempotent request once on a fresh
    connection when a pooled one was closed by the server, asks for gzip and
    decodes it (the `Content-Encoding` header is then removed), follows
    redirects (dropping `Authorization` and `Cookie` for another host);
    `https` URLs go through `net.tls` with ALPN `h2, http/1.1`
    (connection errors are wrapped as `GET <url>: ...`). HTTP/2 responses
    report `Proto` `HTTP/2.0`. A failed `ws.Upgrade` has already written its
    400/426 response, so handlers return without an error.
    — `stdlib/http_*`, `stdlib/http2_*`, `stdlib/ws_echo`
47. Existing stdlib functions follow Go's `strings`/`path` semantics for edge
    cases (`Split(s, "")` splits into code points, `Count(s, "")` is
    runes + 1, `SplitN` with 0 gives no parts and -1 all parts, `EqualFold` uses
    simple case folding so `straße` != `STRASSE`, `fs.Join` cleans the path).

### Tier 0/1 standard library (stdlib2)

1. **Go names that clash with the older int-millisecond `time` functions
   are renamed**: `time.Time.Now()`/`time.Current()`, `time.Elapsed(t)`,
   `time.SleepFor(d)`, `time.ParseTime(layout, s)`; Duration constants are
   constructors (`time.Seconds(5)`), because stdlib constants are untyped
   numbers. `t.Month()` is an `int` (`MonthName()` for the text). Parsing
   defaults the year to 0 (Go). — `stdlib2/time_*`
2. **`io.ReadAll(r)` is `io.ReadAllFrom(r)`** (`io.ReadAll()` reads stdin).
   Reader constructors return pointers (`*StringReader`, like Go's
   `strings.NewReader`) so reads through an interface advance the original.
   `io.Pipe()` is an unbounded in-memory pipe: `Read` on an empty open pipe
   fails with `io: pipe empty`, the async `Recv(n)` waits. — `stdlib2/io_helpers`
3. **Every error of `strconv` wraps `ErrSyntax`/`ErrRange`** with Go's
   text (`strconv.ParseInt: parsing "12a": invalid syntax`). — `stdlib2/strconv_*`
4. **Signals are Linux numbers** (`SIGUSR1` = 10); a signal delivered while
   nobody waits is kept (counted) once it was caught (`Catch`, `Wait`,
   `Notify`). — `stdlib2/os_signal`
5. **`testing` runs every test in its own process** (exit 0 pass, 1 fail,
   3 skip; anything else, e.g. a panic, fails); `t.Fatal` in a subtest ends
   the top-level test; `--no-time` removes durations for golden outputs.
   — `stdlib2/testing_main`
6. **Two imports giving the same short name make it ambiguous**
   (`import text` + `import html`: `template`): alias one or use the full
   path; a name from an import hides the stdlib root of the same name
   (`import crypto`: `hash` is `crypto.hash`, the root `hash` needs its own
   import), but never inside the standard library's own code.
   — `stdlib2/template_import_ambiguous`, `stdlib2/hash_import_shadows_root`
7. **`@embed` paths are relative to the declaring file**; a missing file or
   a type other than `string`/`[]byte`/`embed.FS` is a compile error.
   — `stdlib2/embed_*`

### Tier 2 standard library (stdlib3)

1. **Outputs of the `*_corpus` tests come from reference implementations**
   (Go 1.24 for regex, csv, template, html, mime, url, scanner; PyYAML for
   yaml; Python's tomllib for toml), not from Zell. Known intentional
   divergence: `template_corpus` e9 (a typed template rejects a field that
   `T` lacks even in a branch the data never takes; Go fails only if it runs). — `stdlib3/*_corpus`
2. **`regex.Pattern("...")` and `text/html.template.Parse<T>("...")` with a
   literal are checked by the compiler** (syntax; template fields, functions
   and block structure against `T`); a bad literal is a compile error with
   the runtime's message. Backreferences and lookaround are rejected (RE2).
   — `stdlib3/regex_pattern_*`, `stdlib3/template_unknown_*`
3. **Matching is linear time**: `(a+)+$`-style patterns on long inputs
   finish quickly (lazy DFA, Pike VM fallback). — `stdlib3/regex_redos`
4. **Streaming coders are push-style over `[]byte` chunks** (`Write`/
   `Bytes`, `Feed`/`Read`); gzip also has `io` adapters
   (`NewStreamReader`/`NewStreamWriter`). — `stdlib3/compress_*`
5. **Archive extraction refuses paths outside the target directory**
   (zip-slip, absolute paths, `..`). — `stdlib3/archive_*`

### Namespaces

48. Importing two namespaces with the same short name is an error **at the
    use** of the ambiguous name (`ambiguous name`), not at the imports.
49. Namespaces of one program may refer to each other (no import-cycle rule).
50. Importing a parent namespace makes its children reachable by their last
    segment (`import app.lib` → `text.Shout()` for `app.lib.text`).

### Second wave (interfaces2, generics2, memory2, errors2, async2, strings2, programs, attributes2, platform)

51. **Interface default methods**: `this` in a default body is the interface
    value (it can be passed on as the interface, calls dispatch dynamically);
    a struct method with the same name overrides the default, also through the
    interface; defaults of an embedded interface are inherited; a default may
    call other defaults; generic bodies may call defaults of their interface
    constraint; `implements` does not require them. A default is also
    callable on a value of a type implementing the interface (the value is
    converted; two interfaces with different defaults of that name make the
    call `ambiguous`), and `case I x:` works on `any`. Enums with methods
    implement interfaces like structs. — `interfaces2/default_method_*`,
    `interfaces2/embedding_default_inherited`, `generics2/constraint_default_method`,
    `interfaces2/enum_implements`, `diagnostics/default_method_*`
52. **`sealed` switches**: a type switch over a sealed interface that names every
    implementation needs no `default` and terminates the function (no missing
    return); a non-exhaustive one without `default` is a compile error with the
    keyword `not exhaustive` (suggested wording: `switch on sealed interface
    Shape is not exhaustive: missing Triangle`). The same "terminates" rule is
    taken for an enum switch that names every variant (the `Eval` example in
    `docs/PATTERNS.md` has no `default`). Both `case T x` and `case *T x`
    cover `T`; when no case matches at run time (a nil interface) the
    program panics (`no case of an exhaustive switch matched`). The
    implementations are all structs and enums of the interface's namespace
    that have its methods: `implements` is optional (`docs/PATTERNS.md` said
    it was required for `sealed`; the tests need no annotation, so the doc
    was changed). A non-sealed interface value does not convert statically
    to a sealed interface. — `interfaces2/sealed_*`, `generics2/enum_exhaustive_switch_returns`,
    `diagnostics/sealed_from_open_interface`
53. **Sealed across namespaces**: implementations may live in any file of the
    interface's namespace; a struct of another namespace cannot implement it,
    neither with `implements` nor by an implicit conversion; keyword
    `can only be implemented in namespace` (suggested: `sealed interface Shape
    can only be implemented in namespace app.shapes`). — `interfaces2/sealed_other_namespace_*`,
    `interfaces2/sealed_same_namespace_two_files`
54. **`implements` errors** reuse the conversion wording: `Square does not
    implement Shape: missing method Perimeter`, `method Area has type func()
    int, want func() float`; naming a non-interface reports `is not an
    interface`. Mutual embedding (`A { B }`, `B { A }`) is reported like direct
    self-embedding (`interface A embeds itself through B`). `implements` is
    also allowed on enums. — `interfaces2/implements_*`,
    `interfaces2/embedding_cycle_error`
55. **Collections are values**: a `List`/`Map`/`Set` stored in another
    collection or variable is a copy of the struct (its `Len` does not follow
    later `Add`s of the original); `Map.Get` of a struct value returns a copy
    (write back with `Set`, or store `*T`). Overwriting a `Map` key keeps its
    position; `Remove` then `Set` moves it to the end (same for `Set`).
    — `generics2/map_struct_values`, `generics2/nested_collections_print`,
    `generics2/collections_iteration_order`
56. **Type names in diagnostics**: types declared in the namespace of the
    code being checked print by their short name, all others (including the
    stdlib) by their full path (`type Point does not satisfy constraint
    Ordered`, `Square does not implement app.shapes.Shape: ...`), in every
    sema and ownership diagnostic. — `generics2/constraint_*_wording`
57. **Generic frames in traces** are named after the instance (`First<int>`).
    — `errors2/trace_generic`
58. **`Drop` cannot be fallible**: `func Drop() void!` is rejected by the
    destructor signature rule (keyword ``must be declared as `func Drop()` ``).
    `defer f()?` is rejected with `defer requires a function call`.
    — `errors2/drop_fallible_error`, `errors2/question_in_defer_error`
59. **No "use of moved value"**: passing a variable holding a `Drop` value by
    value is a copy even at its last use, so the error is reported at the call:
    `cannot copy a value of type R: it has a destructor (Drop), so it is
    move-only`; only values created in place move into a parameter.
    — `memory2/move_variable_copy_error`, `memory2/move_last_use_error`, `memory2/move_into_function`
60. **Errors from `main`** (also `async main`) print `error: <message>` and then
    the trace lines (format of decision 23) to stderr; exit code 1.
    `zell run --no-error-trace` makes `errors.Trace` `""` and `Frames` empty.
    — `errors2/main_error_stderr`, `errors2/async_main_error_trace`, `errors2/no_error_trace_flag`
61. **`println` evaluates every argument before writing** (Go); also
    `print`. — `strings2/println_evaluates_args_first`
62. **Case mapping is per code point** (Go `unicode.ToUpper`): `ß` has no
    one-rune upper case and stays (`STRAßE`). — `strings2/case_cyrillic`
63. **Attributes and visibility**: attribute structs follow the export rule,
    so one used from another namespace must be capitalised, with capitalised
    fields (`@meta.Table(Name: "users")`); a lowercase one reports `is private
    to namespace`. Inside namespace `a.b` a child `a.b.c` is reachable as `c`
    without an import (an alias import of it is then an unused import).
    Attributes of a generic struct and its fields belong to every instantiation
    (`reflect.TypeOf<Box<int>>().Attribute<note>()`).
    — `attributes2/other_namespace*`, `attributes2/on_generic_struct`
64. **Platforms**: `os.Name`, `os.Arch`, `os.PathSeparator` and
    `os.LineSeparator` are compile-time `string` constants (usable in `const`);
    a declaration excluded by `@cfg` is not type-checked, using it reports
    `unknown identifier`; two `@cfg` variants of one function or struct may
    coexist; `@cfg(...)` before `namespace` excludes the whole file, like the
    suffixes `_linux.zl`, `_windows.zl`, `_arm64.zl` (only for files found in
    a directory; a file named on the command line is always compiled).
    `@cfg` also works on struct fields and methods, takes `any`/`all`/`arch`/
    `feature`, and rejects unknown keys, OS and architecture names.
    — `platform/*`, `diagnostics/cfg_*`
65. **Arenas**: `defer a.Free()` is the idiom per request; after `Free`,
    `--leak-check` is clean. An `Arena` that is not freed releases
    everything when its last copy goes away; `a.Alloc<T>(n)` and
    `a.String(s)` also allocate in the arena; objects referenced from arena
    objects but living on the heap are released by `Free`. — `memory2/arena_*`
66. **Raw strings** `` `...` `` keep backslashes and newlines; a carriage
    return inside one is dropped (Go). — `basics/raw_string`
67. **Returning a local `Drop` value moves it** (decision 31): only a plain
    local variable in a `return` (not a parameter, a captured or an
    address-taken one) is moved; other copies stay errors (decision 59).
    — `memory/drop_move_on_return`
68. **Leak report names cycle fields**: with `--leak-check` (debug builds)
    the report adds `cycle through Type.Field (...)` for every `*T` field of
    a leaked object that lies on a reference cycle. — `memory/leak_check_names_field`

### Operators, iteration and fixes (operators, iteration)

69. **Operator overloading** (`DESIGN.md` "Перегрузка операторов"): the
    *left* operand's type selects the method and the right operand is
    checked against its parameter, so `func Mul(k int) V` allows `v * 3`
    but not `3 * v` (keyword `mismatched types int and V for operator *`);
    the result type is the method's (`Point - Point` may be a `Delta`, a
    `T!` method makes the operator fallible). `==`/`<` use `Equals`/`Compare`
    only when present, otherwise the builtin rules (plain enums by tag,
    pointers by address); `a op= b` needs a simple place on the left and
    `a[i] op= v` evaluates `i` once; `&`, `|`, `^`, shifts and slicing are not
    overloadable; `@` is MatMul with the precedence of `*`.
    — `operators/*`
70. **Builtin operator constraints** `Add Sub Mul Div Rem Neg MatMul`
    (`T: Add` gives `+` and `+=` on `T`) are satisfied by numbers with the
    operator (`Add` also by `string`, `Rem` only by integers) and by types
    with exactly `func Add(other T) T`; a user type named `Add` hides the
    builtin, a function does not. `Ordered` also admits `Compare`,
    `Equatable` (now distinct from `Comparable`, which stays the map-key
    set) admits `Equals`; `List.Contains`/`IndexOf` need `Equatable`.
    `*S` satisfies an interface constraint when `S`'s methods implement it.
    — `operators/generic_*`, `operators/equatable_constraint`, `generics2/default_method_constraint`
71. **`big.Int` division**: `/` (`Div`) truncates like `int` and pairs
    with `%` (`Rem`); Go's Euclidean `Div` is `DivEuclid` (with `Mod`).
    `big.Decimal` `/` is `Div(y) Decimal!` (= `Quo`, 16 digits HalfEven),
    the old rounding division is `DivRound(y, scale, mode)`.
    — `stdlib2/big_int`, `stdlib2/big_decimal`, `operators/stdlib_types`
72. **Iteration protocol** (`DESIGN.md` "Управление потоком"): `Iter()`,
    else the value's own `Next()`, else `Items() []T`; `Next` returns
    `(T, bool)`, `(K, V, bool)` (exactly two loop variables) or `*T` (nil
    ends); an async `Next` is awaited (only inside async functions); with
    one value `for i, x` counts from 0. `LinkedList`, `OrderedMap`, `LRU` have
    `Iter()`, the other new collections iterate `Items()`, `Channel` has an
    async `Next`, `regex.Iter.Next` returns `*Match`. — `iteration/*`
73. **`append(xs, a, b, c)`** appends every value; all of them are
    evaluated before the first is appended (Go). — `basics/append_many`
74. **`main` returning an error under `--leak-check`**: the error is printed
    and released before the leak report, so the exit code is 1 and what
    the error holds is not reported as leaked. — `errors2/main_error_leak_check`
75. **Stdlib attribute names**: `@flag` (with `import cli`) and
    `@binary` (with `import encoding.binary`) resolve through the
    import like other members; `@json` and the other builtins keep working
    when `import reflect` brings their value structs into scope; the
    lowercase fields of stdlib attribute structs are readable through
    `Attribute<A>()`. — `attributes2/stdlib_attribute_names`
76. **Generic calls after an index**: `t.Fields[1].Attribute<json>()` is a
    call with type arguments (any postfix chain may precede `.Name<...>(`).
    — `reflection/attribute_after_index`
77. **Default methods in stdlib interfaces** (`hash.Hasher.WriteString`)
    and on struct *variables*: the variable is converted by address, so a
    default method that calls mutating methods changes the variable, not a
    copy. — `interfaces2/stdlib_default_method`
78. **An async call that completes without suspending makes no task**
    (runtime/ABI.md 10.3): `await f()` takes the value straight from the
    call, `var t = f()` holds an already completed task (awaitable any
    number of times), `?` propagates its error with the usual trace and its
    defers run at its return. Until its first suspension the body runs as
    part of the caller's current task: it inherits that task's
    cancellation, `task.CurrentContext()` inside it is the caller's, and
    context values it sets (`task.With`, `log.Context.Push`) never reach
    the caller. — `async2/sync_fast_path`, `async2/sync_error_trace`,
    `async2/sync_defer_order`

### Language feedback decisions (docs/LANGUAGE_FEEDBACK.md)

79. **A line starting with `.` continues the previous line**
    (`DESIGN.md` "Точки с запятой"): the automatic `;` before it is not
    inserted, also across comments and blank lines; `zell fmt` keeps such
    a chain broken (one `.name` per line, indented). An explicit `;`
    still ends the statement. — `basics/leading_dot_chain`
80. **`in` is a contextual keyword** (`DESIGN.md` "Управление потоком"):
    the lexer emits it as an identifier; only `for a[, b] in` treats it as
    the keyword, so variables, parameters and fields may be named `in`
    (`for x in in` loops over a variable `in`). — `control/in_contextual`
81. **Keywords as field and attribute argument names** (`DESIGN.md`
    "Атрибуты"): any keyword except `func`, `static`, `async` (they start
    a method) and `_` may name a struct field; the same names are accepted
    as attribute arguments (`@Conf(enum: "a,b")` sets the field `enum`),
    in struct literals (`Conf{enum: "x"}`) and after `.` (statement
    keywords such as `c.for` only directly after the dot, so that `c.` at
    a line end before `for ...` still recovers in the editor). A
    declaration keyword followed by `Name {`, `Name<` or `Name implements`
    inside a struct body is a nested declaration (missing `}`), not a field.
    — `attributes2/keyword_arg_names`
82. **Comma-ok map lookup** (`DESIGN.md` "Объявления переменных", Go
    semantics): a map index destructured into exactly two values
    (`var v, ok = m[k]`, `var (v, ok) = m[k]`, `v, ok = m[k]`) is
    `(V, bool)`; an absent key gives the zero value and `false`, the map is
    not changed. `var a, b = f()` is accepted as `var (a, b) = f()`. No `in`
    operator. Lowered as `hir::Builtin::MapLookup` over the existing
    `zell_map_get*` (NULL = absent), no new runtime entry. — `basics/map_comma_ok`
83. **Type aliases** (`DESIGN.md` "Псевдонимы типов"): `type Name = T` at
    the top level is transparent — resolved once, after the imports, in
    the alias's own file; an alias of a struct/enum/interface *is* that
    declaration for literals, static methods and variants, an alias of a
    builtin converts (`Celsius(36)`), `reflect.TypeOf<Alias>()` names the
    aliased type. Cycles are `invalid recursive type alias`; attributes on
    an alias are an error; generic and local aliases are not supported,
    nor are aliases in the Zell-source stdlib (lazily instantiated
    templates). `type` stays an ordinary name elsewhere.
    — `structs/type_alias`, `structs/type_alias_cycle_error`, `structs/type_alias_attr_error`
84. **Interface cases on an `error` subject** (`DESIGN.md` "switch"):
    `switch err { case I x: }` tests the error's own dynamic type (the
    boxed struct; not the wrap chain) for implementing `I`, exactly like a
    switch over `any`; message-only errors never match. Previously such a
    case compiled but compared type ids and never matched.
    — `errors2/switch_interface_case`

## Ambiguities found in DESIGN.md

Resolved in `DESIGN.md` since: `break` in `switch` (Go semantics, written
down as an open question), the `errors.Trace` format and creation sites,
struct equality (rejected), unused imports (errors, every import), field defaults (all structs; literals and `var x T`),
array lengths (constant expressions), multi/tuple assignment, enum methods,
interface embedding and async lambdas in the grammar and the EBNF, `main`
returning `void!`, `task.Any`. Spec fixes made on the way:
`basics/multi_assign_swap` used an unused loop variable (`for i in 0..5`,
an error like in Go; now `for _`), and `basics/string_index_byte.out`
expected the byte sum of `héllo` to be 701 (it is 795).

- **`break` inside `switch`**: Go semantics (leave the switch) plus "no labels"
  means a loop cannot be left from a case; either `break` should target the
  loop (C#-like switch has no fallthrough, so `break` in a case is otherwise
  useless) or labels are needed. The corpus follows Go for now.
- **`errors.Trace` format**: the example shows three spaces between
  `ReadConfig` and `?("config")` but two elsewhere; it is unclear whether the
  message is part of `Trace`, why `main` appears as "returned via" although it
  catches the error with `try` (not `?`), and where a sentinel `const` error
  is "created". The spec's choices are in decision 23.
- **`ToJson` example** in "Статическая рефлексия" recurses into `ToJson` for
  scalar fields, which would produce `{}` for an `int`; the spec's version uses a
  scalar helper.
- **Field defaults** appear only in the attribute section; whether ordinary
  structs may use them, and whether `var x T` applies them, is unspecified.
- **Struct equality** and **unused imports** are not mentioned.
- **Array length** in the grammar is an `int` literal only.
- **Interior pointers**: `DESIGN.md` only describes escape analysis for `&local`;
  the compiler forbids keeping `&slice[i]` / `&s.f` in a variable, which Go allows.
- **Grammar gaps**: no multi-assignment, tuple assignment, enum methods,
  interface embedding, generic syntax (`<T: C>`), or attribute before
  `namespace`; the prose implies some of them.
- **`main` returning an error**, **`Channel`/`Mutex`/arena APIs**, the
  **`Ordered`** constraint, generic enum construction, `MethodInfo.Call`'s
  receiver/argument passing and the observable effect of **`@reflect`** are not
  specified.
- The final example program in `DESIGN.md` uses `import strings` and then
  `strings.Split`, contradicting the import table (already noted in
  `examples/README.md`).

Found by the second wave:

- **Lowercase attribute structs**: `DESIGN.md` declares `range` and `tag` in
  `app.validation` and uses them on `User` in a separate snippet; under the
  export rule they are private to `app.validation` (only the stdlib is exempt,
  e.g. `@json`). The corpus applies the export rule (decision 63).
- **Type names in diagnostics** were mixed (short in some messages, full
  paths in others); resolved by decision 56 and written down in `DESIGN.md`
  ("Компилятор и инструменты").
- **Exhaustive enum switches**: `docs/PATTERNS.md` (`Eval`) relies on a switch
  naming all variants being terminating; now in `DESIGN.md` ("switch",
  decision 52).
- **`sealed` and `implements`**: `docs/PATTERNS.md` said `implements` was
  mandatory for implementations of a sealed interface, while the corpus
  (and the "structural" rule) need none; `DESIGN.md` and `PATTERNS.md` now
  define the implementations as every type of the namespace with the
  methods (decision 52). The `PATTERNS.md` example used `string(float)`,
  which is not a conversion; it uses `fmt.Format` now (it is
  `examples/sealed_shapes.zl`).
- **Arrow lambdas with a block body** (`(x T) => { ...; return v }`) get a
  `void` result: `return v` reports `too many return values`. `DESIGN.md` only
  shows the block form for `void` (`(s *Server) => { s.timeout = sec }`); the
  corpus writes `func(x T) T { ... }` when a result is needed.
- **Library functions as values**: "functions are first-class values", but
  `strings.ToInt` passed as a `func(string) int!` is rejected (`builtin or
  library function ToInt must be called`); the corpus wraps it in a lambda.
- **`Map` printing** (rejected: `cannot be printed`) is not specified;
  `println` evaluation order is decision 61.
- **Data races between tasks**: tasks run on several threads, so unsynchronised
  writes to shared memory (`p.Data = append(p.Data, i)` from 20 tasks) lose
  updates or crash; `DESIGN.md` does not say what is guaranteed. The `async2`
  tests synchronise with `task.Mutex` or channels, or separate writes in time.

## Diagnostics coverage

The `diagnostics` area has one compile-fail test per sema diagnostic family
(message template in `compiler/src/sema/*.rs`, found by grepping `.err(`,
`.err_note(`, `.err_fix(` and the ownership checker's messages) that had no
spec test yet; each test is named after the diagnostic and its `// error:`
keyword is a distinctive part of the message. `codegen/*.rs` reports only
`not supported in codegen yet` (excluded) and internal errors.

Of about 250 message templates, all but the following are triggered by a
passing compile-fail test of the corpus (`diagnostics/*`, plus the existing
tests in the other areas and the new ones in `interfaces2`, `generics2`,
`memory2`, `errors2`, `attributes2`):

- not reachable from source because the parser rejects the program first:
  `variable needs a type or an initializer` (local and global `var x`),
  `tuple destructuring needs an initializer`, `attributes are not allowed on
  local variables`, `a range a..b is only allowed in for, ...`, `for ... in
  takes one or two variables` (also the `Map` form), `function F has no body`,
  `named argument x: is only allowed inside attribute arguments` (the parser
  accepts `name:` arguments only inside attributes, where constant
  evaluation reports first);
- the arity errors of the internal `arena` intrinsics (`rtArenaNew<T>`,
  `rtArenaAlloc<T>`), which only `Arena.New`/`Alloc` call (calling one
  without a type argument is `diagnostics/arena_intrinsic_no_type`);
- reported by an earlier check: `wrong number of arguments in deferred call`
  (the call check reports it), `cannot use _ as a value` (`unknown identifier
  _`), `generic method F must be called` (`needs type arguments to be used as
  a value`), `cannot be stored as an async result of type ...` (`async function
  F cannot return a value with a destructor`);
- only for packages (`zell.toml` dependencies): `package ... cannot use
  namespace ...` and its `transitive dependency` note.

**Help lines.** Diagnostics with an obvious fix carry a `= help:` line
(rendered after the source excerpt; in the LSP it is a note): did-you-mean
suggestions for unknown identifiers, types, members, namespace members and
attributes (closest name by edit distance), `add import x` for a
stdlib name that is not imported, `?`/`try` for unhandled `T!`, the
parameter list for a wrong argument count, explicit conversions for mixed
numeric operands, the iteration protocol for `cannot iterate`, `async
func` for `await` outside async, and the operator method to declare for
an operator that is not defined on a struct. `redeclared in this block`
has a note pointing at the previous declaration. Only one keyword changed:
`diagnostics/constraint_not_interface` now expects `is not an interface`
(the list of allowed constraints moved into the help line), and parenthesised
hints of `? outside of fallible function`, `mismatched types`, `cannot assign
to undeclared variable`, `use of untyped nil` and `cannot call method`
became help lines (no test keyword included them).

`interfaces2` has the tests of `not exhaustive`, `does not implement` for
`implements`, `is not an interface`, `embeds itself through` and `can only
be implemented in namespace`; `diagnostics/default_method_*`,
`diagnostics/cfg_*` and `diagnostics/sealed_from_open_interface` cover the
other diagnostics of default methods, `@cfg` and sealed interfaces.

### web (area `web`)

- Problem Details members are written in the order `type`, `title`,
  `status`, `detail`, `instance`, `errors`, `trace`; `errors` groups the
  messages by field in order of first appearance; validation problems use
  type `https://tools.ietf.org/html/rfc9110#section-15.5.1` and title
  `One or more validation errors occurred.`, other problems `about:blank`
  and the status text.
- Messages follow ASP.NET Core: `The X field is required.`, `The field X
  must be at least N.`, `... between A and B.`, `... at least N characters
  long.`, `The value 'v' is not valid for x: expected an integer.`; numbers
  print like `println`.
- A `void` handler answers 204 unless it wrote a status, header or body; a
  string value is `text/plain`, every other value JSON unless `Accept`
  asks for XML or YAML.
- Query parameters without `@validate(required: true)` are optional (zero
  value, `nil` for pointers); a repeated key and comma-separated values
  both fill a slice.
- A POST action returning `web.Result<T>` is documented as 201 in OpenAPI,
  every other result as 200 (204 for `void`); operation ids are
  `Controller_Method` or `<method>_<path segments>` for minimal handlers.
