# testing-guide: zell test in practice

A small library, duration parsing and a plan that adds durations up,
and the tests you would write for it. The program is short; the tests
are the point. `zell run` shows what the code does, and `zell test`
checks it from every side: tables of inputs, helpers, errors checked by
type and position, a property over ten thousand values, a panic, async
code with a fake dependency, examples that are documentation, and a
benchmark.

What it shows:

- table-driven tests with a subtest per row (`t.Run`);
- `t.Must` / `t.MustFail` for `T!` calls, and a `void!` test that uses `?`;
- testing a `T!` boundary: the error's type and fields (`errors.As`),
  identity through wrapping (`testing.ErrorIs`), its context, and where
  the code stopped;
- a property test (round trip) and `testing.PanicsWith`;
- async tests (`@test async func`) with a fake implementation of an
  interface;
- examples (`ExampleFormat` with `// Output:`), checked by `zell test`
  and shown by `zell doc`;
- a benchmark (`@bench`, `zell test --bench`);
- what `-v`, `--filter`, `--json` and a failure look like.

## Run it

```sh
cd docs/examples/testing-guide
zell run
zell test
zell test -v --filter 'TestParse'
zell test --json
zell test --bench --filter Bench
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `units`) |
| `src/duration.zl` | `units.duration`: `Parse`, `Format`, `ParseError` |
| `src/plan.zl` | `units.plan`: the `Source` interface, `Memory`, `Total` |
| `src/main.zl` | a short demonstration |
| `src/duration_test.zl` | tables, helpers, a property, a panic, examples, a benchmark |
| `src/plan_test.zl` | async tests with a fake `Source` |

Test files end in `_test.zl` and live in `src/` next to the code:
`zell test` compiles the package's own `src/**/*.zl` with them, and
`zell build` and `zell run` leave them out. Each test file here has its
own namespace (`units.duration.tests`), so it tests through exported
names only, like a user of the package; a test file that declares the
package's namespace instead can reach its private names too.

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "testing-guide"
version = "0.1.0"
namespace = "units"
entry = "src/main.zl"
```

## The code under test

`Parse` reads `1h30m`-style durations and fails with a `ParseError` that
carries the position and the reason, so tests can check those fields
instead of matching message text. `Format` writes seconds back and
panics on a negative number: that is a bug in the caller, not an input
error.

<!-- file: src/duration.zl -->
```zell title="src/duration.zl"
// The code under test: durations written as "1h30m", "90s" or "2h5m10s",
// parsed to seconds and formatted back.
namespace units.duration

// ParseError says what is wrong and where, so a caller (or a test) can
// check the position instead of matching message text.
struct ParseError {
    Input  string
    Pos    int
    Reason string

    func Message() string {
        return fmt.Format("duration {0}: {1} at {2}", strconv.Quote(this.Input), this.Reason, this.Pos)
    }
}

func unit(c string) int {
    switch c {
        case "h": return 3600
        case "m": return 60
        case "s": return 1
        default: return 0
    }
}

// Parse returns the number of seconds in s. Units must come in the
// order h, m, s, each at most once.
func Parse(s string) int! {
    if s == "" {
        return ParseError{Input: s, Pos: 0, Reason: "empty"}
    }
    var total = 0
    var num = ""
    var last = 3601 // larger than any unit: the next one must be smaller
    for i, ch in s {
        var c = string(ch)
        if c >= "0" && c <= "9" {
            num += c
            continue
        }
        var u = unit(c)
        if u == 0 {
            return ParseError{Input: s, Pos: i, Reason: "unknown unit " + strconv.Quote(c)}
        }
        if num == "" {
            return ParseError{Input: s, Pos: i, Reason: "missing number"}
        }
        if u >= last {
            return ParseError{Input: s, Pos: i, Reason: "units out of order"}
        }
        total += strconv.Atoi(num)? * u
        num = ""
        last = u
    }
    if num != "" {
        return ParseError{Input: s, Pos: len(s), Reason: "missing unit"}
    }
    return total
}

// Format writes seconds the way Parse reads them. A negative duration
// is a bug in the caller, so it panics instead of returning an error.
func Format(secs int) string {
    if secs < 0 {
        panic("duration.Format: negative duration")
    }
    if secs == 0 {
        return "0s"
    }
    var out = ""
    var names = ["h", "m", "s"]
    for i, u in [3600, 60, 1] {
        if secs >= u {
            out += strconv.Itoa(secs / u) + names[i]
            secs %= u
        }
    }
    return out
}
```

`Total` depends on a `Source`, an interface, so tests can replace the
real one. Its method is `async` (it returns a task), and so are the
methods that implement it.

<!-- file: src/plan.zl -->
```zell title="src/plan.zl"
// More code under test, with a dependency to replace in tests: a plan
// adds up durations that come from a Source (a file, a service, a
// database). Async, and fallible at its boundary.
namespace units.plan

import units.duration

const ErrUnknownStep = error("unknown step")

// Source is the dependency. Tests pass a fake; main passes Memory.
interface Source {
    async func Lookup(step string) string!
}

// Memory is a Source backed by a map, with a small delay like real I/O.
struct Memory {
    Steps map[string]string

    async func Lookup(step string) string! {
        await task.Delay(1)
        var (d, ok) = this.Steps[step]
        if !ok {
            return errors.Wrap(ErrUnknownStep, strconv.Quote(step))
        }
        return d
    }
}

// Total looks up every step and adds up their durations. The first
// failure stops it, wrapped with the step it happened in.
async func Total(src Source, steps []string) int! {
    var total = 0
    for s in steps {
        var text = await src.Lookup(s)?("step " + s)
        total += Parse(text)?("step " + s)
    }
    return total
}
```

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// testing-guide: a small program whose tests are the real content (see
// src/*_test.zl and the README). It parses and formats durations and
// adds up a plan.
namespace units

import io
import units.duration
import units.plan

async func main() {
    for s in ["1h30m", "90s", "2h5m10s", "5m1h", "10x"] {
        var secs = try Parse(s) catch err {
            println(fmt.Sprintf("%-8s error: %s", s, err.Message()))
            continue
        }
        println(fmt.Sprintf("%-8s %6d s = %s", s, secs, Format(secs)))
    }
    var src = Memory{Steps: map[string]string{"build": "4m30s", "test": "12m", "deploy": "1m15s"}}
    var total = try await Total(src, ["build", "test", "deploy"]) catch err {
        println("plan:", err)
        return
    }
    println("plan:", Format(total))
    try await Total(src, ["build", "review"]) catch err {
        println("plan:", err)
    }
}
```

## src/duration_test.zl: tables, helpers, properties, examples

`@test` marks a test; it takes a `*testing.T`. Every test runs in its
own process, so a failed `Fatal`, or even a crash, ends only that test.

**Tables.** A slice of structs, one per case, and `t.Run(name, fn)`
for each row: the row's name appears in the output
(`TestParse/1h30m`), a failing row does not stop the others, and
`--filter` selects tests by a regular expression over their names.

**`T!` in tests.** `t.Must(Parse(c.In))` gives the parsed value, or
ends the subtest with the `file:line` of the call, the error and its
trace, so the table loop stays one line. `t.MustFail(Parse(s))` is the
other side: it gives the error, and ends the test if `Parse` succeeded.
`parseError` adds the check that the error is a `ParseError`
(`errors.As` also finds it inside wrappers) and returns it for the test
to check its fields. A test declared `void!` (`TestRoundTrip`) uses `?`
directly: an error it returns fails the test like an uncaught error in
`main`. A test of a `T!` function is mostly this: decide what you check
(success, the error's type, its fields, its identity, its message).

**A property.** `TestRoundTrip` checks that `Parse(Format(n)) == n` for
every `n` below 10 000, which catches what a handful of hand-picked
cases miss.

**A panic.** `testing.PanicsWith(t, fn, message)` runs `fn` on a thread
of its own and checks that it panics with that message. `testing.Panics`
checks only that it panics.

**Examples.** A function `ExampleName` in a test file is an example of
`Name`: `zell doc` shows it with the symbol, and `zell test` runs it and
compares its output with the `// Output:` comment.

**A benchmark.** `@bench` takes a `*testing.B` and loops `b.N` times;
`zell test --bench` raises `N` until the timing is stable.

<!-- file: src/duration_test.zl -->
```zell title="src/duration_test.zl"
// Tests for units.duration: a table of good inputs, a table of bad ones
// checked by error type and position, a round-trip property, a panic,
// examples with their output, and a benchmark.
namespace units.duration.tests

import io
import units.duration

struct Good {
    In   string
    Secs int
}

// Table-driven: one row per case, one subtest per row. A failing row is
// reported by name (TestParse/1h30m) and the other rows still run.
// t.Must gives the value of Parse, or ends the subtest with the error.
@test func TestParse(t *testing.T) {
    var cases = [
        Good{In: "45s", Secs: 45},
        Good{In: "1h30m", Secs: 5400},
        Good{In: "2h5m10s", Secs: 7510},
        Good{In: "0s", Secs: 0},
        Good{In: "100m", Secs: 6000},
    ]
    for c in cases {
        t.Run(c.In, func(t *testing.T) {
            testing.Equal(t, t.Must(Parse(c.In)), c.Secs)
        })
    }
}

// parseError runs Parse on bad input and returns its ParseError.
// t.MustFail ends the test if Parse succeeds (with the file:line of the
// call); a different error type fails it too.
func parseError(t *testing.T, s string) ParseError {
    var pe ParseError
    var err = t.MustFail(Parse(s))
    if !errors.As(err, &pe) {
        t.Fatal("not a ParseError: " + err.Message())
    }
    return pe
}

struct Bad {
    In     string
    Pos    int
    Reason string
}

@test func TestParseErrors(t *testing.T) {
    var cases = [
        Bad{In: "", Pos: 0, Reason: "empty"},
        Bad{In: "10", Pos: 2, Reason: "missing unit"},
        Bad{In: "h", Pos: 0, Reason: "missing number"},
        Bad{In: "5m1h", Pos: 3, Reason: "units out of order"},
        Bad{In: "1m1m", Pos: 3, Reason: "units out of order"},
        Bad{In: "3d", Pos: 1, Reason: "unknown unit \"d\""},
    ]
    for c in cases {
        t.Run(strconv.Quote(c.In), func(t *testing.T) {
            var pe = parseError(t, c.In)
            testing.Equal(t, pe.Pos, c.Pos)
            testing.Equal(t, pe.Reason, c.Reason)
        })
    }
}

// A property instead of examples: formatting and parsing back gives the
// same number, for every duration in a range. The test returns `void!`,
// so `?` works in it: an error from Parse fails the test with its trace.
@test func TestRoundTrip(t *testing.T) void! {
    for secs in 0..10000 {
        if Parse(Format(secs))? != secs {
            t.Fatal("round trip failed for " + strconv.Itoa(secs))
        }
    }
}

// A negative duration is a bug, and bugs panic. testing.PanicsWith runs
// the function on a thread of its own, so the panic fails nothing.
@test func TestFormatPanicsOnNegative(t *testing.T) {
    testing.PanicsWith(t, func() {
        _ = Format(-1)
    }, "duration.Format: negative duration")
}

// Examples are documentation that is tested: `zell doc` shows this
// under Format, and `zell test` checks the output comment.
func ExampleFormat() {
    println(Format(5400))
    println(Format(61))
    // Output:
    // 1h30m
    // 1m1s
}

func ExampleParse() {
    println(try Parse("1h1s") else -1)
    println(try Parse("1s1h") else -1)
    // Output:
    // 3601
    // -1
}

// A benchmark runs its loop b.N times; `zell test --bench` picks N and
// reports the time per iteration.
@bench func BenchParse(b *testing.B) {
    for _ in 0..b.N {
        _ = try Parse("2h5m10s") else 0
    }
}
```

## src/plan_test.zl: async tests and a fake

An `async` test is awaited by the runner, so it calls `Total` and
awaits it directly. `Fake` implements `Source` with fixed answers, a
step that fails like a network call, and a call counter, which lets
`TestTotalStopsAtTheFirstError` check not only the error but also that
`Total` did not ask for the steps after it. The counter is a `*int`
because `Fake` is passed by value into the interface: the copy and the
test's variable share the counter.

`testing.ErrorIs(t, err, ErrUnknownStep)` passes when the sentinel is
anywhere in the chain; the exact message then checks the context that
`Total` added.

<!-- file: src/plan_test.zl -->
```zell title="src/plan_test.zl"
// Tests for units.plan: async tests, a fake dependency, and checks at
// the T! boundary (which error, and how it was wrapped).
namespace units.plan.tests

import units.plan

// Fake is a Source for tests: fixed answers, a failing step, and a
// count of calls, so a test can check what Total asked for.
struct Fake {
    answers map[string]string
    calls   *int

    async func Lookup(step string) string! {
        *this.calls += 1
        if step == "flaky" {
            return error("connection reset")
        }
        var (d, ok) = this.answers[step]
        if !ok {
            return ErrUnknownStep
        }
        return d
    }
}

func fake() Fake {
    return Fake{answers: map[string]string{"a": "1m", "b": "30s", "bad": "1x"}, calls: new int}
}

// An async test: the runner awaits it.
@test async func TestTotal(t *testing.T) {
    var f = fake()
    var total = t.Must(await Total(f, ["a", "b", "a"]))
    testing.Equal(t, total, 150)
    testing.Equal(t, *f.calls, 3)
}

// The boundary: Total fails, and the test checks the error by identity
// (errors.Is through the wrapping), by its context, and that Total
// stopped at the first failure.
@test async func TestTotalStopsAtTheFirstError(t *testing.T) {
    var f = fake()
    var err = t.MustFail(await Total(f, ["a", "missing", "b"]))
    testing.ErrorIs(t, err, ErrUnknownStep)
    testing.Equal(t, err.Message(), "step missing: unknown step")
    testing.Equal(t, *f.calls, 2)
}

@test async func TestTotalReportsTheFailingLayer(t *testing.T) {
    var f = fake()
    testing.Contains(t, t.MustFail(await Total(f, ["flaky"])).Message(), "connection reset")
    // The duration's own error, wrapped with the step's name.
    testing.Contains(t, t.MustFail(await Total(f, ["bad"])).Message(), "step bad: duration \"1x\": unknown unit")
}

// The real Memory source, through the interface like main uses it.
@test async func TestMemory(t *testing.T) {
    var m = Memory{Steps: map[string]string{"x": "2m"}}
    testing.Equal(t, try await Total(m, ["x", "x"]) else -1, 240)
}
```

## What the runner prints

Without flags, passing tests are quiet:

```text
PASS
ok  	8 passed, 0 failed, 0 skipped (0.02s)
ok  	2 examples passed, 0 failed (0.00s)
```

`-v` lists every test and subtest; `--filter` takes a regular
expression:

```text
$ zell test -v --filter 'TestParse$'
=== RUN   TestParse
    === RUN   TestParse/45s
    --- PASS: TestParse/45s (0.00s)
    ...
--- PASS: TestParse (0.00s)
```

A failure prints the test's log and the assertion, here after changing
one row of the table to a wrong value, and the exit status is 1:

```text
    === RUN   TestParse/100m
        got 6000, want 600
    --- FAIL: TestParse/100m (0.00s)
--- FAIL: TestParse (0.00s)
FAIL
FAIL	0 passed, 1 failed, 0 skipped (0.00s)
```

`--json` prints one event per line for CI and editors:

```text
$ zell test --json --filter 'TestTotal$'
{"Action":"run","Test":"TestTotal"}
{"Action":"pass","Test":"TestTotal","Elapsed":0.001}
{"Action":"pass","Elapsed":0.004}
```

`--bench` adds the benchmarks (the numbers depend on the machine):

```text
$ zell test --bench --filter Bench
BenchParse                  3149605          376 ns/op       10 allocs/op
```

## Output of zell run

<!-- file: expected.out -->
```text title="expected.out"
1h30m      5400 s = 1h30m
90s          90 s = 1m30s
2h5m10s    7510 s = 2h5m10s
5m1h     error: duration "5m1h": units out of order at 3
10x      error: duration "10x": unknown unit "x" at 2
plan: 17m45s
plan: step review: "review": unknown step
```

## Next

- [Testing](../../guide/testing.md): every assertion and flag.
- [jobrunner](../jobrunner/README.md) and
  [errors-deep-dive](../errors-deep-dive/README.md): more async tests
  and error-mapping tables.
