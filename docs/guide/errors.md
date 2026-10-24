# Errors

Zell has no exceptions. Errors are values, as in Go, but the language
gives you three short forms instead of `if err != nil { return err }`:

| You write | Meaning |
|---|---|
| `func F() T!` | F returns a `T` or an error (`void!`: nothing or an error) |
| `f()?` | the value, or return the error from the current function |
| `f()?("context")` | the same, wrapping the error with a message |
| `try f() catch err { ... }` | handle the error here |
| `try f() else v` | use `v` if `f` fails |
| `try { ... } catch err { ... }` | handle the errors of a whole block |

## Returning errors

`error("message")` creates an error. In a `T!` function, `return` accepts
either a `T` or an error; the compiler tells them apart by type.

```zell
namespace guide.errors.basics

import io

func divide(a int, b int) int! {
    if b == 0 {
        return error("division by zero")
    }
    return a / b
}

func average(xs []int) int! {
    var sum = 0
    for x in xs {
        sum += x
    }
    return divide(sum, len(xs))?("average")
}

func main() {
    println(try average([2, 4, 9]) else -1)
    try average([]) catch err {
        println("failed:", err)
    }
    // A `T!` value must be handled: `var x = average(xs)` alone does not compile.
    _ = average([1])
}
// Output:
// 5
// failed: average: division by zero
```

`?` only works inside a function whose result is `T!` (or inside a block
`try`, below). `main` may return `void!`: if it returns an error, the
program prints `error: <message>` and the error trace to stderr and exits
with status 1.

## Handling errors on the spot

`try ... catch` gives the error a name. When the `try` produces a value,
the `catch` block must leave the function (`return`, `continue`, `break`,
`panic`), so the variable after it is always initialized. `try ... else`
substitutes a default and drops the error.

```zell
namespace guide.errors.handling

import io

func parseAll(lines []string) []int {
    var out []int = []
    for line in lines {
        var n = try strings.ToInt(line) catch err {
            println("skipping", strconv.Quote(line))
            continue
        }
        out = append(out, n)
    }
    return out
}

func main() {
    println(parseAll(["1", "x", "3"]))
    var port = try strings.ToInt("") else 8080
    println(port)
}
// Output:
// skipping "x"
// [1 3]
// 8080
```

The block form handles several fallible steps at once: inside the block,
`?` jumps to its `catch` instead of returning from the function, and
execution continues after the `catch`.

```zell
namespace guide.errors.block

import io

func main() {
    var total = 0
    try {
        total += strings.ToInt("40")?
        total += strings.ToInt("2")?
        total += strings.ToInt("oops")?
        total += 1000
    } catch err {
        println("stopped:", err)
    }
    println(total)
}
// Output:
// stopped: invalid integer: "oops"
// 42
```

## Typed errors

`error` is a built-in interface with one method, `Message() string`. Any
struct with that method is an error, so errors can carry data. A type
`switch` picks them apart; sentinel errors are constants compared with
`errors.Is`.

```zell
namespace guide.errors.typed

import io

const ErrNotFound = error("not found")

struct ValidationError {
    Field  string
    Reason string

    func Message() string {
        return this.Field + ": " + this.Reason
    }
}

func lookup(id int) string! {
    if id < 0 {
        return ValidationError{Field: "id", Reason: "must not be negative"}
    }
    if id > 10 {
        return ErrNotFound
    }
    return "user " + strconv.Itoa(id)
}

func show(id int) {
    var name = try lookup(id) catch err {
        switch err {
            case ValidationError v: println("bad input in", v.Field, "-", v.Reason)
            default: println("error:", err, "| not found:", errors.Is(err, ErrNotFound))
        }
        return
    }
    println(name)
}

func main() {
    show(3)
    show(-1)
    show(99)
}
// Output:
// user 3
// bad input in id - must not be negative
// error: not found | not found: true
```

## Wrapping and unwrapping

`?("context")` and `errors.Wrap(err, "context")` add a message in front
of the original error and keep it inside. `errors.Is` and `errors.As`
search the whole chain, so wrapping never hides what happened.

```zell
namespace guide.errors.wrap

import io

struct Timeout {
    Ms int

    func Message() string {
        return "timeout after " + strconv.Itoa(this.Ms) + "ms"
    }
}

func connect() void! {
    return Timeout{Ms: 250}
}

func startService() void! {
    connect()?("connect to db")
}

func main() {
    try startService() catch err {
        println(err)
        var t Timeout
        if errors.As(err, &t) {
            println("retry with more than", t.Ms, "ms")
        }
        println(errors.Unwrap(err))
    }
}
// Output:
// connect to db: timeout after 250ms
// retry with more than 250 ms
// timeout after 250ms
```

Your own error types can join the chain too: a struct error with a
method `Unwrap() error` is a link, and `errors.Is`, `errors.As` and
`errors.Unwrap` continue with its cause. `Unwrap()` is called once, when
the value becomes an `error`, so the cause is a snapshot taken at that
moment. The message is only the error's own `Message()`; the cause is not
appended to it.

```zell
namespace guide.errors.unwrap

import io

const ErrDisk = error("disk full")

struct SaveError {
    Path  string
    cause error

    func Message() string {
        return "cannot save " + this.Path
    }

    func Unwrap() error {
        return this.cause
    }
}

func save(path string) void! {
    return SaveError{Path: path, cause: ErrDisk}
}

func main() {
    try save("a.txt") catch err {
        println(err)
        println(errors.Is(err, ErrDisk))
        var se SaveError
        println(errors.As(err, &se), se.Path)
        println(errors.Unwrap(err))
    }
}
// Output:
// cannot save a.txt
// true
// true a.txt
// disk full
```

To branch on the kind of an error, `switch` on it with type cases. A case
tests the dynamic type of the error itself (the struct that was boxed),
not the wrapped chain; use `errors.As` for the chain. A case may name an
interface, which matches any error struct that implements it. The first
matching case wins.

```zell
namespace guide.errors.kinds

import io

interface Coded {
    func Code() int
}

struct NotFound {
    What string

    func Message() string {
        return this.What + " not found"
    }

    func Code() int {
        return 404
    }
}

struct Broken {
    func Message() string {
        return "broken"
    }
}

func classify(err error) string {
    switch err {
        case Coded c: return "coded " + strconv.Itoa(c.Code())
        case Broken b: return "broken"
        default: return "other"
    }
}

func main() {
    println(classify(NotFound{What: "user"}))
    println(classify(Broken{}))
    println(classify(error("x")))
}
// Output:
// coded 404
// broken
// other
```

## Error traces

Every error remembers where it was created and every `?` it went
through: a *return trace* (like Zig's), recorded with one pointer per
step, no stack unwinding. `println(err)` prints only the message;
`errors.Trace(err)` gives the trace as text and `errors.Frames(err)` as
records.

```zell
namespace guide.errors.trace

import io

func open(path string) string! {
    return error("open " + path + ": not found")
}

func readConfig() string! {
    return open("app.toml")?("config")
}

func main() {
    try readConfig() catch err {
        println(err)
        for f in errors.Frames(err) {
            println(f.Func, f.Context)
        }
    }
}
// Output:
// config: open app.toml: not found
// open
// readConfig config
// main
```

A trace printed by `errors.Trace` looks like this:

```text
  created at   app/fs.zl:42      open
  returned via app/config.zl:17  readConfig  ?("config")
  returned via app/main.zl:9     main
```

Build with `--no-error-trace` if you use errors for control flow in a
hot loop; `errors.Trace` then returns `""`.

File names in a trace are printed relative to the current directory when
the file is under it, so a trace from your project reads `app/fs.zl:42`
rather than an absolute path; files outside it (dependencies, a build run
from another directory) keep the path the compiler saw. In logs the
`{Exception}` part of a template is a single line, the message and where
the error was created; the full trace is written at the `Debug` and
`Verbose` levels (see [debugging.md](debugging.md)).

## Errors and `defer`

A deferred call whose result is `T!` must handle its error, because there
is nobody to return it to. Defer a closure that does:

```zell
namespace guide.errors.defer

import io

func cleanup(name string) void! {
    return error("cannot remove " + name)
}

func work() {
    defer func() {
        try cleanup("tmp") catch err {
            println("cleanup:", err)
        }
    }()
    println("working")
}

func main() {
    work()
}
// Output:
// working
// cleanup: cannot remove tmp
```

## Panics

`panic("message")` is for bugs and broken invariants, not for expected
failures: it prints the message and a stack trace and exits with status
2. Index out of range, `nil` dereference and integer division by zero
panic too. A test can check that something panics with
`testing.Panics` (see [testing.md](testing.md)).

A panic is not an error: `try` and `?` do not see it, and it cannot be
caught. A nil dereference and an index out of range are runtime panics
with a message and a backtrace, and the process exits with status 2.
Nothing recovers a panic, not even a `web` handler: the server process
ends too, so return an error for anything a request can cause. See
[debugging.md](debugging.md) for how backtraces look.
