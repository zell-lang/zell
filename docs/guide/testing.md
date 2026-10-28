# Testing

`zell test` finds the tests of a package (or a directory, or one file),
builds them into one binary and runs every test in its own process, so a
failed assertion, a `Fatal` or even a panic ends only that test.

```sh
zell test                     # the package of the current directory
zell test path/to/dir -v      # every test, with names and logs
zell test --filter '^TestParse|Sum$'
zell test --json              # one JSON event per line (CI, editors)
zell test --bench             # also run the benchmarks
zell test --parallel 8        # parallel tests at a time (default 4)
zell test std                 # the standard library's own tests and examples
```

## Tests

A test is a function marked `@test` that takes a `*testing.T` (or no
parameters at all). Assertions from `testing` report a failure and
keep going; their `...Fatal` variants stop the test.

```zell
// zell test
namespace guide.testing.basics

func Add(a int, b int) int {
    return a + b
}

func Parse(s string) int! {
    return strings.ToInt(s)?("parse")
}

@test func TestAdd(t *testing.T) {
    testing.Equal(t, Add(2, 3), 5)
    testing.NotEqual(t, Add(2, 2), 5)
}

@test func TestParse(t *testing.T) {
    testing.Equal(t, t.Must(Parse("42")), 42)
    testing.Contains(t, t.MustFail(Parse("x")).Message(), "parse")
}

@test func TestParseSum(t *testing.T) void! {
    var sum = Parse("2")? + Parse("3")?
    testing.Equal(t, sum, 5)
}

@test func plainTestsNeedNoHandle() {
    if Add(1, 1) != 2 {
        panic("arithmetic is broken")
    }
}
```

Code that can fail is tested without `try ... catch` boilerplate:

- `t.Must(f())` returns the value of a `T!` call (`t.Must(save())` with a
  `void!` result is a statement). If the call fails, the test stops, and
  its log has the location of the `Must`, the message and the error
  trace:

  ```text
      parse_test.zl:12: parse: strconv.Atoi: parsing "x": invalid syntax
        created at   parse.zl:4        Parse
        ...
  ```

- `t.MustFail(f())` returns the error of a call that must fail; if it
  succeeds, the test stops with `parse_test.zl:13: expected an error`.
- A test declared `void!` (`func TestX(t *testing.T) void!`, also without
  parameters or `async`) can use `?`: an error it returns fails the test
  and is logged as `error: <message>` with its trace, like an uncaught
  error in `main`.

`t.Must` and `t.MustFail` are the only calls that accept an unhandled
`T!` argument; anything else still has to handle it with `?` or `try`.
Their argument may be an `await` in an async test:
`t.Must(await Fetch())`.

Messages: `Equal(t, got, want)` reports `got X, want Y`;
`EqualMsg(t, got, want, msg)` is the same with context and reports
`msg: got X, want Y`. `True(t, cond, msg)` and `False(t, cond, msg)`
always take the message, since a bare `expected true` says nothing; they
report `expected true: msg`.

`t.Log(msg)` prints under the test (shown for failures and with `-v`),
`t.Error(msg)` marks it failed and continues, `t.Fatal(msg)` stops it,
`t.Skip(msg)` skips the rest. Other assertions: `True`, `False`, `Nil`,
`NotNil`, `EqualSlices`, `Error`, `NoError`, `ErrorIs` and
`Panics(t, fn)`, which checks that `fn` panics: `fn` runs on a thread of
its own whose panic is caught, so the test's code runs once; `defer`s and
destructors inside the panicking `fn` do not run (on Windows the test is
re-run in a child process up to that call instead). `PanicsWith(t, fn,
want)` also checks the panic message, and `PanicMessage()` returns it.

## Table tests and subtests

Tables are slices of structs; `t.Run(name, fn)` runs a named subtest
(`TestAbs/negative` in the output). A test whose body calls
`t.Parallel()` runs concurrently with the other parallel tests.

```zell
// zell test
namespace guide.testing.table

import io

struct Case {
    Name string
    In   int
    Want int
}

func abs(x int) int {
    if x < 0 {
        return -x
    }
    return x
}

@test func TestAbs(t *testing.T) {
    t.Parallel()
    var cases = [Case{Name: "negative", In: -3, Want: 3}, Case{Name: "zero", In: 0, Want: 0}, Case{Name: "positive", In: 5, Want: 5}]
    for c in cases {
        t.Run(c.Name, func(t *testing.T) {
            testing.Equal(t, abs(c.In), c.Want)
        })
    }
}

@test func TestPanics(t *testing.T) {
    var xs = [1, 2, 3]
    testing.Panics(t, func() {
        println(xs[abs(-7)])
    })
}
```

## Async tests

A test can be `async` (`func(t *testing.T) Task<void>`, or `async` with
no parameters); the runner awaits it, so it can `await` timers, channels
and network calls directly.

```zell
// zell test
namespace guide.testing.async

async func fetchAnswer() int {
    await task.Delay(1)
    return 42
}

async func fetchPort(s string) int! {
    await task.Delay(1)
    return strings.ToInt(s)?("port")
}

@test
async func TestFetch(t *testing.T) {
    var n = await fetchAnswer()
    testing.Equal(t, n, 42)
    testing.Equal(t, t.Must(await fetchPort("8080")), 8080)
    _ = t.MustFail(await fetchPort("http"))
}
```

## Packages and dependencies

In a package `zell test` resolves the dependency graph like `zell build`
(dev-dependencies included) and compiles the package's own `*_test.zl`
files; the test files of its dependencies are never compiled into it.

## Benchmarks

A benchmark takes a `*testing.B` and runs the measured code `b.N` times;
`zell test --bench` reports time and allocations per operation.
`b.ResetTimer()`, `StopTimer()` and `StartTimer()` exclude setup.

```zell
// zell test
namespace guide.testing.bench

@bench func BenchJoin(b *testing.B) {
    var parts = ["a", "b", "c", "d"]
    b.ResetTimer()
    for _ in 0..b.N {
        _ = strings.Join(parts, ",")
    }
}

@test func TestJoin(t *testing.T) {
    testing.Equal(t, strings.Join(["a", "b"], ","), "a,b")
}
```

## Examples

A function `ExampleName()` in a `*_test.zl` file is an *example*: it is
compiled with the tests, `zell doc` shows it under the symbol it names,
and if it ends with an `// Output:` comment `zell test` runs it and
compares its standard output with the comment (surrounding white space
ignored; `// Unordered output:` compares the lines in any order).

| Function | Documents |
|---|---|
| `Example()` | the namespace |
| `ExampleSplit()` | function or type `Split` |
| `ExampleBuilder_Write()` | method `Builder.Write` |
| `ExampleSplit_empty()` | `Split` again; the lower-case suffix names the variant |

An example may also be `async` or return `void!` (an error fails it).

```zell
// zell test
namespace guide.testing.examples

import io

// Greet returns a greeting for name.
func Greet(name string) string {
    return "Hello, " + name + "!"
}

func ExampleGreet() {
    println(Greet("Ann"))
    println(Greet("Bob"))
    // Output:
    // Hello, Ann!
    // Hello, Bob!
}

func ExampleGreet_unordered() {
    for name in ["x", "y"] {
        println(Greet(name))
    }
    // Unordered output:
    // Hello, y!
    // Hello, x!
}
```

The standard library documents itself this way: `std/**/*_test.zl` hold
examples for the most used functions, and `zell test std` runs them.

## Output and exit status

Without `-v`, passing tests are quiet; a failure prints the test's log and
`--- FAIL: TestName (0.00s)`, and the run ends with a summary such as
`ok   12 passed, 0 failed, 1 skipped (0.31s)`. The exit status is 1 when
anything failed, so `zell test` works in CI as is; `--json` gives
machine-readable events (`{"Action":"pass","Test":"TestAdd","Elapsed":0.004}`).
