# Zell example corpus

These programs are both documentation and the end-to-end test suite for the
Zell compiler, runtime and standard library. They follow `DESIGN.md`
exactly; where the design is silent, the choice made is listed in
[Design decisions assumed by the examples](#design-decisions-assumed-by-the-examples).

## Layout

```
examples/
  NAME.zl            single-file program, entry point `main` in the file's namespace
  NAME.out           exact expected stdout of NAME.zl
  multi/             one program made of several files (several namespaces)
    namespaces.zl    namespace app         (contains main)
    models.zl        namespace app.models
    report.zl        namespace app.report
    namespaces.out   expected stdout of the whole directory
  errors_compile/    programs that MUST FAIL to compile (no .out files)
```

## Conventions

- **Runnable programs.** For every `NAME.zl` with a sibling `NAME.out`, the
  test runner does `zell run NAME.zl` and compares stdout byte for byte
  with `NAME.out` (every line, including the last, ends with `\n`). Exit
  code must be 0 and nothing is expected on stderr. All output is
  deterministic: no timing, map-order or address dependence.
- **Multi-file programs.** A directory with a `.out` file is one program:
  the runner compiles all `.zl` files in it together
  (`zell run examples/multi`) and uses `main` from the root namespace
  (`app`). The expected output is `namespaces.out`.
- **Compile-failure programs.** Every file in `errors_compile/` starts with a
  line `// error: <reason>` followed by the `namespace` line. The runner
  must check that `zell build FILE` exits non-zero and reports at least one
  error diagnostic. The text after `// error:` names the expected
  diagnostic; a runner may check that the main diagnostic contains its key
  words, but the exact wording is not normative.
- Every `.zl` file starts with `namespace` (only the leading `// error:`
  comment in `errors_compile/` precedes it).

## Output format (normative for `println`, `fmt.Format`)

The `.out` files depend on these rules; the runtime must follow them.

- `println(a, b, ...)` writes its arguments separated by a single space and
  ends with `\n`, like Go's `fmt.Println`. `println()` writes just `\n`.
- `int` (all integer types): decimal, `-` for negatives.
- `float`: the shortest decimal that round-trips, like Go's `%v`
  (`strconv.FormatFloat(f, 'g', -1, 64)` with Go's `%v` exponent
  thresholds): `0.5` -> `0.5`, `2.0` -> `2`, `1.5e3` -> `1500`,
  `0.1 + 0.2` -> `0.30000000000000004`, `-0.5` -> `-0.5`. Exponent form
  (`1e+21`, `1e-05`) only for exponent < -4 or >= 6, as in Go; the
  examples never hit it.
- `bool`: `true` / `false`. `nil` (pointer or error): `nil`.
- `string`: raw contents, no quotes. `char`: the character itself
  (`'a'` prints `a`, unlike Go's rune which prints a number).
- Slices and arrays: Go style, `[1 2 3]`, nested `[[1 2] [3 4]]`.
- Tuples: `(3 x)`, elements separated by one space, nested `((1 2) z)`.
- Plain enum values: the variant name (`Color.Green` prints `Green`).
- Any value whose static type is `error` (or a struct used through the
  `error` interface): the result of its `Message()`.
- `fmt.Format(tmpl, args...)`: each `{N}` is replaced by argument `N`
  formatted exactly as `println` would format it; other text is copied
  verbatim (`$` has no special meaning).
- Maps and structs are never printed directly by the examples.

## Examples

| File | Features exercised |
|------|--------------------|
| `hello.zl` | `namespace`, `import io`, bare `println` |
| `arith.zl` | int/float literals, `+ - * / %`, truncating division and sign of `%`, Go precedence (incl. `<<`, `&`, `\|`, `^`), compound assignment, top-level and local `const`, `var` with inferred type, explicit type and zero value, float printing, `math.Sqrt/Pow/Floor/Abs`, `float()`/`int()` conversions, comparisons, `&&`/`\|\|`/`!` precedence |
| `control.zl` | `if`/`else if`/`else` (all branches return), `for i in a..b` with literal and variable bounds, `for i, x in slice`, `for x in [literal]`, while-style `for cond`, infinite `for` with `break`, `continue`, nested loops |
| `functions.zl` | grouped params `(a, b int)`, multiple results and `var (a, b) = f()`, `_` in destructuring, recursion (factorial, fib), block lambda `func(x int) int {}`, arrow lambda `(x int) => x * x`, lambdas as arguments, returning closures (`Compose`, counter with captured state), function types `func(int) int` / `func(string)`, named functions as values, slice of functions |
| `structs.zl` | fields and methods with `this`, `static func` constructors, struct literals incl. empty `Point{}`, methods that mutate the receiver, value copy on assignment and by-value parameters, `&`, `*p`, `(*p).Y = ...`, auto-deref `p.X`, methods through pointers, `new`, pointer aliasing, `nil` and `== nil`, linked list of `*Node`, embedding with promoted fields/methods and explicit `d.Animal.Name` |
| `interfaces.zl` | interface declaration, structural implementation by two (three) structs, function taking an interface, `[]Shape` of mixed values, type switch `case Circle c:` with `default`, a struct satisfying two interfaces, interface holding a `*Circle` |
| `enums.zl` | plain enum, `Color.Red`, `==` on enums, `int(Color.Blue)`, printing an enum, enum with fields (`Circle(radius float)`), plain variant inside a data enum, constructing variants, `case Result.Ok(v):` destructuring with one and two fields, `default`, nested `if` inside a case |
| `slices_maps.zl` | slice literal, `append`, `len`, indexing and index assignment, sub-slice `s[1..4]` sharing the buffer, empty `[]` vs zero-value slice, nested slices, fixed array `[3]int`, map literal, empty map literal, set/get/overwrite, missing key -> zero value, `delete`, deterministic printing via a key list, order-independent `for _, v in m` and `for k in m`, `m[k] += 1`, reference semantics of maps, map of slices |
| `strings.zl` | `+` concatenation, `len` (bytes), `==`/`<`/`>`, `strings.Split/Join/Trim/Contains/ToUpper/Replace/ToInt`, `try ... else` with `ToInt`, `for ch in s` over code points (non-ASCII `héllo`), `switch` on chars, `string(ch)`, `+=` on strings, `fmt.Format` with reordered/repeated placeholders, escapes `\t \" \\` |
| `errors.zl` | `T!` and `void!` functions, `error("msg")`, `fmt.Format` inside errors, `?`, `?("context")`, `return f()?(...)`, `try ... catch err { return }`, statement-level `try ... catch`, `try ... else value`, `try ... else continue`, `continue` from a `catch` block, custom error structs with `Message()`, `switch err { case NotFound e: ... }`, sentinel `const ErrNotFound = error(...)`, `errors.Is` through wrapping, `errors.As(err, &target)`, `errors.Wrap/Unwrap/New`, `Unwrap` of an unwrapped error -> `nil`, `_ = f()`, top-level `var` |
| `try_block.zl` | block `try { ... } catch err { ... }` in a non-fallible function (`?` jumps to the catch), `?("ctx")` inside the block, catch continuing after the block, `catch _`, block try inside a loop, nested in a `T!` function with `return err` from the catch |
| `errors_trace.zl` | error return traces: `errors.Frames(err)` (`File`, `Line`, `Func`, `Context`), creation by `error(..)` / a stdlib call / `return Sentinel` in a method, `?("ctx")` frames, the catching `try` frame, `errors.Is` on a traced sentinel, `errors.Trace` line count (file names shown with `fs.Base` so the output does not depend on the build directory) |
| `switch.zl` | value cases, value lists, half-open ranges incl. negative bounds, guards (`case n > 100:`), first-match ordering, string cases, no-subject `switch { case cond: }`, multi-statement bodies, no fallthrough, switch with no match and no default, FizzBuzz |
| `defer.zl` | LIFO order, defer in a loop, arguments evaluated at `defer`, deferred closure `defer func() { ... }()` seeing later values, deferring a call of a lambda variable, defers on early `return error(...)`, defer in `main` |
| `async.zl` | `async func`, `async func main`, `await`, `task.Delay`, concurrent tasks with `task.All` and tuple destructuring, `await f()?`, `try await ... catch`, `try await ... else`, `task.All` over fallible tasks with `try`, sequential awaits in a loop |
| `drop.zl` | destructors `func Drop()`: deterministic order at block end (reverse declaration order), per loop iteration and on `break`, by-value parameter takes ownership, pointer parameter borrows, early return through `?`, last reference to a `new` object, struct holding a value with a destructor (outer `Drop` first, then fields), interface box and closure capture, deferred calls run before locals are dropped, globals dropped after `main` |
| `generics.zl` | generic functions with inference (`Max(3, 7)`, untyped constants following `T`) and explicit type arguments (`Max<float>(1, 2)`), the builtin `Ordered`/`Comparable` constraints, an interface constraint calling `x.Area()`, several type parameters and `func(T) U` parameters (`Apply`, `Reduce`), generic struct `Pair<A, B>` with a method returning `Pair<B, A>`, generic enum `Option<int>.Some(i)` matched as `case Option.Some(i):`, `Max<int>` as a function value, a generic error struct with `errors.As`, a generic method on a plain struct, `collections` `List`/`Map` (insertion order, `for k, v in m`)/`Set`/`Queue`/`Stack` |
| `sealed_shapes.zl` | the `docs/PATTERNS.md` example: `sealed interface` with a default method (`Describe` built from `Name`/`Area`), `struct ... implements Shape`, a struct overriding the default, an exhaustive type switch with no `default` and no final `return` |
| `cfg.zl` | `@cfg(os: ...)`/`not(...)`/`any(...)`/`all(...)`/`arch:` on functions, constants, struct fields and methods (the excluded variant is not type-checked), `os.Name`/`os.Arch`/`os.PathSeparator`/`os.LineSeparator` as compile-time constants (the output is that of a Unix amd64/arm64 target) |
| `weak.zl` | `weak *T` back pointers (parent/child): the cycle is freed (shown by `Drop` prints), reading a weak pointer yields `*T` or `nil` once the target is gone, `!= nil` checks, a slice of `weak *T` observers |
| `multi/` | three files, three namespaces: `import app.models` short names, full path `app.models.User.New(...)` even though imported, `import fmt as f` alias, calling non-imported `app.report.Summary` by full path, a file with no imports using `io.println` and `app.models.User` as a parameter type, private field `id` set only inside its namespace, exported `const` |
| `bank.zl` | larger program: `Account` with private fields, `void!` methods, `static func` returning `*Account`, enum `Tx` with fields and plain enum `EntryKind`, `Bank` with `map[string]*Account` and `[]string`, typed error `InsufficientFunds`, sentinel + `errors.Wrap`, `?` and `?("context")` on method calls through map elements, `errors.As` / `errors.Is` in a `catch`, switch over transactions and history, integer-cents money formatting, final report |
| `stdlib_time_os_fs.zl` | `time` (`Format` with `%L`, `Parse` incl. `%z`, calendar accessors on a fixed instant, `Weekday`, pre-epoch time, `Now`/`Sleep`/`Since` only compared, never printed), `os` (`Args`, `Getenv`/`Setenv`, `Pid`, `Hostname`, `Cwd`/`Chdir`), `fs` path helpers (`Join/Base/Dir/Ext/Abs`) and file operations (`MkdirAll`, `AppendFile`, `Size`, `Copy`, `Rename`, sorted `ReadDir`, `Exists`/`IsDir`, `ModTime`, failing `Remove`/`Mkdir`, `RemoveAll` cleanup) in a temp dir under `fs.TempDir()`, `io.ReadLines/WriteLines`, `void!` helpers with `?` and `try ... catch` |
| `stdlib_crypto.zl` | nested stdlib namespaces (`import crypto` -> `hash.X`, no import -> `crypto.hash.X`, `import encoding` -> `base64.X`), SHA-256/512/1, MD5, CRC-32 test vectors, `[]byte` results, HMAC-SHA256 and `hmac.Equal`, base64 std/URL and hex round trips and decode errors, `crypto.rand`/`uuid` (only lengths/validity printed), seeded `random` (`IntN`, `Shuffle`, `Float`, `Bytes`) |
| `stdlib_strings2.zl` | `strings.Fields/Count/LastIndex/TrimPrefix/TrimSuffix/TrimLeft/TrimRight/SplitN/EqualFold/Reverse/PadLeft/PadRight/Compare/Title`, `Runes`/`FromRunes` (`[]char`), `Bytes`/`FromBytes` (`[]byte`), `math.Round/Trunc/Mod/Hypot/Log2/Log10/Atan2/IsNaN/IsInf/Inf/NaN` and the int helpers `IMin/IMax/IAbs` |
| `reflection.zl` | `reflect`: `reflect.TypeOf<T>()` (`Name`, `FullName`, `Kind`, `Size`, `Fields` with `Type.Name`/`Offset`, `Methods` with signature names), a custom `@attribute` struct read with `f.Attribute<column>()` (positional and named arguments, field default), `@reflect`, `t.Method(name).Call(&p, args...)` with a boxed result and a failing `void!` method, typed `Call<func(*Product, int) void!>()`, `reflect.ValueOf(&p).Field(..).Set/Get<T>`, `t.New()` as `any` holding `*T`, enum `Variants`, `typeof` of `any` values and of an interface, compile-time `for f in reflect.Fields<T>()` with `f.Get(v)` in a generic, a static `if reflect.TypeOf<T>().Kind == ...` |
| `json.zl` | `encoding.json`: `json.Parse<T>` into nested structs, slices of structs, `*T`, `map[string]int`, plain enums by name, floats and `\u` escapes; `json.Format` with `@json(name:)`, `@json(omitEmpty:)`, `@json(skip:)`, sorted map keys, `null` for nil slices/maps/pointers and `[]` for an empty slice (decision 22); errors with the path of the bad value (`Users[1].id: expected int, found string`), syntax errors with offsets; the dynamic `json.ParseValue` tree (`Type`, `Len`, `Keys`, `Get`, `Index`, `Has`, `Float`, `Int`, `IsNull`, `Format`) |
| `channels.zl` | `task.Channel<int>(0)` (unbuffered) and `(4)` pipelines with `Close` and `try await ch.Recv() else break`, `task.Mutex` as a struct field, `task.WaitGroup`, fire-and-forget `_ = Work(...)`, `task.Yield`, `task.Spawn` of an async lambda, `task.Timeout` (false, then true), a context value with `task.Key`/`task.With`/`task.Get` |
| `task_context.zl` | `var K = task.Key<Span>()`, `task.With(K, v, f)` seen in the started task, a nested call, a `task.Go` child and a nested `With`, and gone after `With` returns (`task.Get` is nil) |
| `cancel.zl` | `task.NewCancel()` + `task.SpawnWith` stopping a consumer blocked in `Recv` (`errors.Is(err, task.ErrCancelled)`), `task.WithTimeout` waking a long `Delay` (`task.CheckCancelled()?`), `task.Any` cancelling the loser, cancellation reaching a child task, `task.IsCancelled()` outside the scope (only "fast" timing facts are printed) |
| `http_server.zl` | `net.http` server on `127.0.0.1:0` (`Listen`, `Port`, `Serve`, `Shutdown`) with `NewRouter`, `{name}` parameters, an async lambda handler, a redirect, 404/405/500; the client (`http.Get`/`Post`) talking to it; an `https` URL to a closed port (dialed through `net.tls`). Needs loopback networking |
| `cycle_server.zl` | a `net.http` server serving 10k requests, each leaving a reference cycle behind (`Session` <-> `RequestCtx`, plus a closure in the session capturing it; W0001 warns about both types): the cycle collector (`ZELL_CYCLES=auto`) keeps the live object count bounded (`diag.Runtime().LiveObjects` sampled in both halves of the run), `runtime.CycleStats()`, and the program passes `--leak-check`. Needs loopback networking |
| `tcp_echo.zl` | `net`: a line-based TCP echo server (task per connection) with `NewBufferedReader`/`NewBufferedWriter`, clients that half-close (`CloseWrite`) and `ReadAll`, `SetTimeout` failing with `net.IsTimeout`, shutdown by closing the listener (`errors.Is(err, net.ErrClosed)`), `net.ErrRefused` afterwards. Needs loopback networking |
| `tls_echo.zl` | `net.tls` server and clients in one program on `127.0.0.1:0`: TLS 1.3 (ALPN, a 100 KB message, `KeyUpdate`) with `InsecureSkipVerify`, TLS 1.2 with a pinned self-signed root, an Ed25519 client certificate and ticket resumption, TLS 1.3 PSK resumption, verification failures (wrong name, unknown root), HTTPS with `http.Server{TLS}` and `http.Client{TLS}`. The embedded certificates are valid 2000-2100; output does not depend on the cipher suite (which depends on AES-NI). Needs loopback networking |
| `log.zl` | `log`: `log.New()` configuration (`MinimumLevelControlledBy` a `LevelSwitch`, `MinimumLevelOverride`, `Enrich.WithProperty`/`FromContext`, `Clock` for fixed timestamps, `WriteTo.Console`/`Json`/`Async(NewFileSink(...))` rolling by day, `Filter.ByExcluding`), compiled templates with `{@Order}` destructuring (`@log(skip: true)`), `{Amount:0.00}`, `{Rate:P1}`, `{{` escapes, arguments not evaluated for disabled levels, `log.Context.Push` seen in a child task, `ForCategory`, `log.For<T>()`, `Error(err, ...)`, `log.SetGlobal` + `log.Info` |
| `metrics.zl` | `diag` counters/gauges/histograms with labels, `PrometheusText` served by `diag.ServeMetrics` on a router (loopback HTTP), `diag.Runtime()` facts, `EnableRuntimeMetrics` |
| `tracing.zl` | `diag` spans with seeded ids and a fake clock: nested spans across an `await`, a server span parented through the `traceparent` header of `http.Get`, `RecordError`, `SetStatus`, `ConsoleExporter`, `Enrich.FromContext` adding `TraceId`/`SpanId` to a log line, `OtlpSpanJson` |
| `cpuprofile.zl` | `diag.StartCPUProfile`/`StopCPUProfile` around a busy loop (prints only that samples were taken; the profile is pprof `profile.proto`) |
| `stdlib_collections_algos.zl` | word statistics with `slices` (`Map`, `SortFunc` stable, `Unique`, `MaxFunc`, `Chunk`, `Sum`, `BinarySearch`), `maps.SortedKeys`, `cmp` (`Clamp`, `Compare`), and the newer `collections` types: `Heap<int>` as a bounded top-k, `OrderedMap<string, []string>`, `LRU<string, int>(2)` with `Get` tuples and `Keys` order, `Deque.PushFront` |
| `stdlib_money.zl` | an invoice with `math.big.Decimal` (exact `Mul`/`Add`, `Round(2, HalfUp)`, `Div(.., 2, HalfEven)`, `Rescale`), `strconv.Atoi`/`Quote` with `?("context")` errors, `big.ParseDecimal`; `time` types on fixed UTC dates (`Date`, `AddDate`, `Format` with `RFC3339` and a Go layout, `Sub` → `Duration.String()`, `ParseTime`, `Hours()`) |
| `cli_tool.zl` | `cli`: `App` with two subcommands, flags from structs with `@cli.flag(short:, help:)` and field defaults (`cli.FlagsOf<T>()`, `cli.Bind<T>(c)`), positional args, `--times=2`/`-n x`/bool flags, a failing command (exit 1), usage errors with "did you mean" (exit 2) collected through `App.Errors`, generated `--help` for the app and a command |
| `stdlib_io_encoding.zl` | `io` writers (`BufferedWriter` over `MultiWriter` of a `BytesWriter` and an `fnv` hash) and `Scanner` over a `BytesReader`, `strconv.Quote`/`Unquote`, `unicode.utf8.RuneCountInString`; `encoding.binary.Encode/Decode/Size` of a struct with `@binary.binary(endian: "big")`, `AppendUvarint`/`Uvarint` tuples; `pem.Block` with a header (`Encode`/`Decode`), `crc32.ChecksumIEEE`, `base32.Encode`/`EncodeHex`, `unicode.IsLetter`/`ToUpper`/`ToTitle` |
| `regex.zl` | `regex`: `regex.Pattern` literals checked at compile time, named groups (`Groups` + `Get`), `GroupsAll` and `At`, `Replace` with `$1`, `Split`, `Escape` + `Compile`, a `Compile` error message with offset, linear-time matching of `(a+)+$` on 20k chars |
| `stdlib_formats.zl` | tier-2 formats: `encoding.toml.Parse<Config>` with `@toml(name:)`, `toml.ParseDocument` + `Set` keeping comments, `encoding.csv.Read<Item>`/`Write<Item>` with `@csv(name:)` and quoted fields, `html.template.Parse<Page>` (fields checked when compiling, contextual escaping of text and URL query), `text.table` with the Unicode style and right alignment, `compress.gzip` round trip |
| `sql.zl` | `database.sql` over `database.sqlite` (in-memory, linked with `-lsqlite3` only because the program uses the driver): `Exec` with positional `?` and `sql.Named` parameters, `LastInsertId`, typed `db.Query<Book>` with `@sql.column` and a `*float` for NULL, `QueryOne<int>`, `Query<string>`, a transaction with `defer tx.Rollback()`, `errors.Is(err, sql.ErrNoRows)` |
| `redis.zl` | `data.redis` against a tiny in-process RESP server (bottom of the file; the server parses commands with the client's own `redis.ParseReplies`): `redis.Connect`, `Set/Get`, `Incr/IncrBy`, a missing key as `redis.Nil`, `HSetMap/HGetAll`, typed `SetJSON<T>/GetJSON<T>`, a pipeline whose command tasks complete after `Exec`, `TxPipeline` (MULTI/EXEC), `Subscribe` + `for msg in sub` + `Publish`, a server error code. Needs loopback networking |
| `kafka.zl` | `data.kafka` against a tiny in-process broker (ApiVersions, Metadata, Produce, ListOffsets, Fetch only; bottom of the file): `NewProducer` with gzip batches and linger, `Send(topic, key, value)` partitioned by the Java client's murmur2, a group-less `NewConsumerWith` from the earliest offsets read with `for rec in c`. Needs loopback networking |
| `s3.zl` | `cloud.s3` against a tiny in-memory S3 server (`net.http.test`, no signature check; bottom of the file): `CreateBucket`, `PutObject` with a content type, `HeadObject`, `GetObjectBytes`, `List` iteration, `errors.Is(err, s3.ErrNoSuchKey)`, `Presign`; `cloud.blob` with the same calls over `OpenS3` and `OpenDir`. Needs loopback networking |
| `errors_compile/unused_var.zl` | declared and never used local variable |
| `errors_compile/unhandled_result.zl` | `int!` value used without `?`/`try` |
| `errors_compile/unhandled_void_result.zl` | `void!` call used as a statement without `?`/`try`/`_ =` |
| `errors_compile/question_outside_fallible.zl` | `?` inside a function that does not return `T!` |
| `errors_compile/await_outside_async.zl` | `await` in a non-`async` function |
| `errors_compile/log_template_args.zl` | `log` message template with 2 placeholders and 3 arguments |
| `errors_compile/unknown_ident.zl` | reference to an undeclared name |
| `errors_compile/type_mismatch.zl` | assigning a `string` to an `int` variable |
| `errors_compile/missing_return.zl` | `if`/`else if` without final `else` in a function returning `int` |
| `errors_compile/catch_falls_through.zl` | `var n = try f() catch err { ... }` whose block neither returns nor jumps |
| `errors_compile/interior_pointer_escape.zl` | returning an interior pointer variable (`var p = &l.From` pins `l` but cannot leave the function) |
| `errors_compile/copy_drop_value.zl` | copying a value whose type has a destructor (`var b = a`) |
| `errors_compile/weak_to_interior.zl` | making a `weak *T` from an interior pointer |

## Standard library functions used by the examples

The examples use only these functions. "As println" means formatted
per [Output format](#output-format-normative-for-println-fmtformat).

**Builtins** (always in scope, no import)

| Function | Signature / behaviour |
|----------|-----------------------|
| `error` | `error(msg string) error` — new error whose `Message()` is `msg` |
| `len` | `len(x) int` for `string` (bytes), `[]T`, `[N]T`, `map[K]V` |
| `append` | `append(s []T, x T) []T` (Go semantics; nil/zero slice is fine) |
| `delete` | `delete(m map[K]V, k K)`; no-op if absent |
| conversions | `float(int) float`, `int(float) int` (truncates toward zero), `int(enum) int` (variant ordinal from 0), `string(char) string` |

**io** — `println(args ...any)` (also reachable as `io.println`).

**fmt** — `Format(template string, args ...any) string`, placeholders `{0}`, `{1}`, ...

**strings**

| Function | Signature / behaviour |
|----------|-----------------------|
| `Split` | `Split(s, sep string) []string` (as Go `strings.Split`) |
| `Join` | `Join(parts []string, sep string) string` |
| `Trim` | `Trim(s string) string` — removes leading and trailing whitespace (like Go `TrimSpace`; no cutset argument) |
| `Contains` | `Contains(s, substr string) bool` |
| `ToUpper` | `ToUpper(s string) string` |
| `Replace` | `Replace(s, old, new string) string` — replaces all occurrences |
| `ToInt` | `ToInt(s string) int!` — base-10, optional leading `-`; fails on anything else (the error text is not relied on) |

**math** — `Sqrt(x float) float`, `Pow(x, y float) float`,
`Floor(x float) float`, `Abs(x float) float`.

**strings** (used by `stdlib_strings2.zl`, Go semantics; offsets are bytes, widths are runes)

| Function | Signature / behaviour |
|----------|-----------------------|
| `Fields` | `Fields(s string) []string` — split around runs of white space |
| `Count` / `LastIndex` | `Count(s, sub string) int` (non-overlapping; `""` gives runes+1), `LastIndex(s, sub string) int` (-1 if absent) |
| `TrimPrefix` / `TrimSuffix` | `(s, x string) string` — remove one leading/trailing `x` |
| `TrimLeft` / `TrimRight` | `(s, cutset string) string` — strip runes contained in `cutset` |
| `SplitN` | `SplitN(s, sep string, n int) []string` — at most `n` pieces; `n == 0` gives `[]`, `n < 0` all |
| `EqualFold` | `EqualFold(a, b string) bool` — case-insensitive (simple folding) |
| `Reverse` / `Title` | `(s string) string` — reverse code points / upper-case the first letter of each word |
| `PadLeft` / `PadRight` | `(s string, width int, pad string) string` — pad to `width` runes by repeating `pad` |
| `Compare` | `Compare(a, b string) int` — -1, 0, 1 |
| `Runes` / `FromRunes` | `Runes(s string) []char`, `FromRunes(r []char) string` |
| `Bytes` / `FromBytes` | `Bytes(s string) []byte`, `FromBytes(b []byte) string` |

**math** (more, `stdlib_strings2.zl`) — `Round` (half away from zero), `Trunc`,
`Log2`, `Log10` `(float) float`; `Mod`, `Hypot`, `Atan2(y, x)` `(float, float) float`;
`IsNaN(x float) bool`, `IsInf(x float, sign int) bool`, `Inf(sign int) float`, `NaN() float`;
int helpers `IMin(a, b int) int`, `IMax(a, b int) int`, `IAbs(a int) int`.

**io** (more) — `ReadLines(path string) []string!` (no trailing empty line, `\r` dropped),
`WriteLines(path string, lines []string) void!` (each line + `\n`), `WriteFile(path, data string) void!`.

**time** (`stdlib_time_os_fs.zl`; instants are `int` Unix milliseconds, UTC)

| Function | Signature / behaviour |
|----------|-----------------------|
| `Now` / `NowNanos` | `() int` — current time in ms / ns |
| `Sleep` / `Since` | `Sleep(ms int)`, `Since(startMs int) int` (= `Now() - startMs`) |
| `Format` | `Format(ms int, layout string) string` — `strftime` layout in UTC plus `%L` = milliseconds |
| `Parse` | `Parse(s, layout string) int!` — `strptime` layout, UTC unless `%z`; the whole input must match |
| `Year` ... `Second` | `Year/Month/Day/Hour/Minute/Second(ms int) int` — UTC calendar fields (month 1..12) |
| `Weekday` | `Weekday(ms int) int` — 0 = Sunday |

**os** — `Args() []string`, `Getenv(name string) string` (`""` if unset),
`Setenv(name, value string) void!` (empty name fails), `Pid() int`, `Hostname() string!`,
`Cwd() string!`, `Chdir(path string) void!`, `Exit(code int)`.

**fs** (paths are `/`-separated, Go `path/filepath` semantics)

| Function | Signature / behaviour |
|----------|-----------------------|
| `Join` / `Base` / `Dir` / `Ext` | `Join(a, b string) string` (cleaned), `Base/Dir/Ext(path string) string` (`Base("")` = `.`, `Dir("x")` = `.`, `Ext` includes the dot or is `""`) |
| `Abs` / `TempDir` | `Abs(path string) string!`, `TempDir() string` (`$TMPDIR` or `/tmp`) |
| `Exists` / `IsDir` | `(path string) bool` |
| `ReadDir` | `ReadDir(path string) []string!` — entry names sorted byte-wise |
| `Mkdir` / `MkdirAll` | `(path string) void!` — `Mkdir` fails if it exists (`mkdir data: File exists`); `MkdirAll` creates parents and accepts an existing dir |
| `Remove` / `RemoveAll` | `(path string) void!` — `Remove` needs a file or empty dir; `RemoveAll` is recursive and ignores a missing path |
| `Rename` / `Copy` | `(from, to string) void!` |
| `Size` / `ModTime` | `(path string) int!` — bytes / Unix ms |
| `AppendFile` | `AppendFile(path, data string) void!` — creates the file if needed |

**crypto.hash / hmac / rand** (`stdlib_crypto.zl`; hex is lowercase)

| Function | Signature / behaviour |
|----------|-----------------------|
| `hash.Sha256` / `Sha512` / `Sha1` / `Md5` | `(data string) []byte` — raw digest |
| `hash.Sha256Hex` / `Sha512Hex` / `Sha1Hex` / `Md5Hex` | `(data string) string` — hex digest |
| `hash.Crc32` | `Crc32(data string) int` — IEEE CRC-32 |
| `hmac.Sha256` / `Sha256Hex` | `(key, data string) []byte` / `string` — HMAC-SHA256 |
| `hmac.Equal` | `Equal(a, b []byte) bool` — constant-time comparison |
| `rand.Bytes` / `Int` / `Hex` | `Bytes(n int) []byte!`, `Int() int!`, `Hex(n int) string!` (`2n` chars) — OS randomness |

**encoding.base64 / hex**

| Function | Signature / behaviour |
|----------|-----------------------|
| `base64.Encode` / `EncodeString` | `([]byte)` / `(string)` `-> string`, standard alphabet with `=` padding |
| `base64.Decode` / `DecodeString` | `(s string) []byte!` / `string!` — padding required |
| `base64.EncodeURL` / `DecodeURL` | `EncodeURL(b []byte) string` (URL alphabet, no padding), `DecodeURL(s string) []byte!` (padding optional) |
| `hex.Encode` / `EncodeString` | `([]byte)` / `(string)` `-> string` |
| `hex.Decode` / `DecodeString` | `(s string) []byte!` / `string!` — fails on odd length or a non-hex byte |

**random** (non-cryptographic; fixed sequence after `Seed`) — `Seed(n int)`, `Int() int`,
`IntN(n int) int` (`[0, n)`, panics if `n <= 0`), `Float() float` (`[0, 1)`), `Bytes(n int) []byte`,
`Shuffle(s []int)` (in place).

**uuid** — `New() string` (random v4, lowercase), `IsValid(s string) bool`.

**errors**

| Function | Signature / behaviour |
|----------|-----------------------|
| `New` | `New(msg string) error` — same as builtin `error(msg)` |
| `Wrap` | `Wrap(err error, msg string) error` — `Message()` is `msg + ": " + err.Message()`; keeps `err` as the cause |
| `Unwrap` | `Unwrap(err error) error` — the cause, or `nil` if `err` is not a wrapper |
| `Is` | `Is(err, target error) bool` — true if `target` equals `err` or any cause in its chain |
| `As` | `As(err error, target *T) bool` — `T` is a struct type implementing `error`; if some error in the chain has dynamic type `T`, copies it into `*target` and returns true |

**task**

| Function | Signature / behaviour |
|----------|-----------------------|
| `Delay` | `Delay(ms int) Task<void>` — completes after `ms` milliseconds (0 allowed) |
| `All` | `All(t1 Task<T1>, ..., tn Task<Tn>) Task<(T1, ..., Tn)>` — waits for all; if any `Ti` is fallible (`Ui!`) the result is `Task<(U1, ..., Un)!>` and fails with the error of the first failing task in argument order |

## Design decisions assumed by the examples

These are choices made where `DESIGN.md` is silent or ambiguous; each is
consistent with Go unless stated. Please fold them into the design.

1. **Import rule is applied literally.** `import io` gives bare
   `println`; the stdlib namespaces are top-level, so `strings.Split`,
   `fmt.Format`, `math.Sqrt`, `errors.Is`, `task.All` work without any
   import. So most examples only `import io`.
2. **Entry point.** For a single file, `main` is taken from the file's
   namespace. `main` may be `async func main()`; the runtime awaits it.
3. **Builtins** `error`, `len`, `append`, `delete`, `panic` and the
   conversions `float(x)`, `int(x)`, `string(c)` are always in scope.
   There is no implicit int/float conversion between typed values; untyped
   numeric constants convert implicitly as in Go (`Point.New(3, 4)` with
   `float` params, `Point{X: 1, Y: -2}`).
4. **Integer arithmetic** is Go's: `/` truncates toward zero, `%` has the
   sign of the dividend; precedence table as in `ast.rs` (Go's).
5. **Ranges `a..b` are half-open everywhere**: in `for`, in `case` patterns
   (`case 3..10:` matches 3 to 9) and in sub-slicing.
6. **Sub-slicing is `s[a..b]`** (the AST's `Range` expression used as an
   index), meaning Go's `s[a:b]`, sharing the underlying buffer. Open-ended
   forms are not used. Note that `grammar/zell.bnf` currently has no range
   expression at all (not in `for ... in` either); the AST's `Range` is
   what the examples rely on.
7. **Strings**: `len(s)` is the byte length; `for ch in s` yields `char`
   code points; `for _ in s` is allowed. String indexing `s[i]` is not used.
   Comparison is byte-wise lexicographic.
8. **Maps**: reading a missing key yields the zero value without inserting;
   `m[k] += 1` works on missing keys; iteration order is unspecified
   (examples only aggregate when iterating); `for k in m` yields keys,
   `for k, v in m` yields key and value. No comma-ok lookup is used.
9. **`for` variables**: `for i, x in slice` gives index and element; `_` may
   be used for either; each iteration gets a fresh variable (Go 1.22).
10. **Struct literals are not allowed directly in `if`/`for`/`switch`
    headers** (the Go rule, also noted in `grammar/zell.bnf`), so
    `for x in list {` and `switch n {` parse as expected.
11. **Methods and interfaces**: methods have pointer receivers; calling a
    method on an addressable value (`p.Move()`) takes its address
    implicitly; both a `T` value and a `*T` satisfy an interface whose
    methods `T` declares. Storing a value in an interface copies it;
    storing a pointer shares it. A type switch `case Circle c:` matches a
    stored `Circle` value and binds a copy.
12. **Embedding**: an embedded struct is initialised in a literal by its
    type name (`Dog{Animal: Animal{Name: "Rex"}}`) and is accessible as the
    field `d.Animal`.
13. **Enums**: plain variants compare with `==`, print as their name and
    convert to their 0-based ordinal with `int(...)`. A data enum may also
    have plain variants (`Empty`), written without parentheses. A switch
    over an enum need not be exhaustive (the examples add a trailing
    `return` after such switches).
14. **Terminating statements** (for "missing return"): as in Go, a
    `return`, an `if` with `else` whose branches all terminate, and a
    `switch` with `default` whose cases all terminate.
15. **Switch**: cases are tried top to bottom, first match wins. A case
    expression of type `bool` in a switch whose subject is not `bool` is a
    guard. A switch with no matching case and no `default` does nothing.
    A case body is every statement up to the next `case`/`default`.
16. **Errors**:
    - `void!` functions may fall off the end (success), like `void`.
    - A call returning `T!` or `void!` must be handled with `?`, `try` or
      `_ =`; a bare expression statement is a compile error too.
    - `try f() catch err { ... }` as a statement discards the success value.
    - When the value of `try ... catch` is used (`var x = try ...`), the
      catch block must not fall through: it must end in `return`,
      `panic`, `break` or `continue` (the design lists only
      return/panic; `continue` is used by the examples, mirroring
      `try ... else continue`).
    - `f()?("ctx")` and `errors.Wrap(err, "ctx")` produce the message
      `"ctx: " + inner message` and keep the inner error as the cause.
    - `return X` in a `T!` function returns an error when `X`'s type
      implements `error`, otherwise a value.
    - `errors.Is` is used only with the very same `const` sentinel, so
      either identity or by-value comparison works.
    - `errors.Unwrap` of a non-wrapper returns `nil`, printed as `nil`.
17. **Async**: calling an `async func` starts the task immediately; `await
    f()?` means `(await f())?` (the parser yields `Await(Propagate(...))`;
    semantics apply `?` to the awaited result); `try await f() catch` /
    `else` work like on any `T!`. Examples print only from the main task
    or from a task that is awaited immediately, so output is ordered.
18. **Defer**: Go semantics — the deferred call's function value and
    arguments are evaluated at the `defer` statement, calls run LIFO when
    the function returns (including early error returns), defers in loops
    accumulate, closures see the variables' final values, and `main`'s
    defers run before the program exits.
19. **Closures** capture variables by reference. Arrow lambdas infer their
    return type from the body expression.
20. **Multi-line slice literals** use a trailing comma, so Go-style
    semicolon insertion after `)` at line end does not break them.
21. **Unused names**: unused local variables are errors; unused function
    parameters, functions and results of non-fallible calls (`counter()`)
    are not. Every import in the examples is used, so either policy on
    unused imports works.
22. **`encoding.json` and empty slices.** A nil slice, map or pointer
    formats as `null` (Go). A slice value whose `data` pointer is set is
    not nil even when it is empty (`[z][0..0]`, a decoded `[]`), so
    `json.Parse` decodes `[]` into an empty non-nil slice and `json.Format`
    writes it back as `[]`, and `{}` into an empty non-nil map written
    back as `{}`; `null` decodes to nil. The parser rejects nesting deeper
    than 512 containers (`json: nesting too deep`), numbers whose value is
    infinite (`1e400`: `json: number out of range at offset N`) and keeps
    `-0` a float; integers that fit `int` stay exact (no float rounding);
    lone surrogates in `\u` escapes decode to U+FFFD.
