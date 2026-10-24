# FAQ: Zell for Go and C# programmers

## Why another language?

Zell aims at the programs people write in Go and C# (services, tools,
CLIs) with Go's small, readable language, C#'s ergonomics where Go is
verbose (namespaces, `var`, methods inside types, `async`/`await`,
generics, attributes), errors that are values but take one character to
propagate, and no garbage collector: memory is reference counted with
compile-time ownership, so latency has no GC pauses and resources close
deterministically. It compiles through LLVM to small native binaries.

## Coming from Go

| Go | Zell |
|---|---|
| `package users` + directory layout | `namespace app.services.users` in the file; directories do not matter |
| `x := 1`, `var x int` | `var x = 1`, `var x int` (no `:=`) |
| `func (u *User) Name() string` | `func Name() string` inside `struct User`, receiver `this` |
| `v, err := f(); if err != nil { return err }` | `var v = f()?` |
| `fmt.Errorf("ctx: %w", err)` | `f()?("ctx")` or `errors.Wrap(err, "ctx")` |
| `errors.Is` / `errors.As` | the same, through the whole wrap chain |
| goroutines + `sync.WaitGroup` | `async func` + `await`, `task.WaitGroup`, `task.All` |
| `chan T`, `select` | `task.Channel<T>(n)`, `await ch.Recv()`, `task.Any` |
| `context.Context` parameters | the task context is implicit: cancellation and values follow the task |
| `iota` constants | `enum Color { Red, Green }`, enums with data, exhaustive `switch` |
| `interface{}` / `any` | `any`, and real generics with constraints |
| `for i := 0; i < n; i++` | `for i in 0..n` |
| GC | ARC + `Drop` + `weak` (see [memory.md](memory.md)) |
| `go test`, `ExampleX` | `zell test`, `ExampleX` with `// Output:` (the same idea) |
| `go doc strings.Split` | `zell doc strings.Split` |

Things that stay as in Go: structural interfaces, value structs, slices
and maps with reference semantics, `defer`, `switch` without fall-through,
upper-case export, unused imports and variables are errors, a formatter
with no options (`zell fmt`).

```zell
namespace guide.faq.go

import io

struct User {
    Name  string
    Email string

    func Domain() string {
        var at = strings.Index(this.Email, "@")
        return this.Email[at + 1..]
    }
}

func parse(line string) User! {
    var parts = strings.Split(line, ",")
    if len(parts) != 2 {
        return error("want name,email: " + strconv.Quote(line))
    }
    return User{Name: strings.Trim(parts[0]), Email: strings.Trim(parts[1])}
}

func main() {
    for line in ["ann, ann@example.com", "broken"] {
        var u = try parse(line) catch err {
            println("skip:", err)
            continue
        }
        println(u.Name, u.Domain())
    }
}
// Output:
// ann example.com
// skip: want name,email: "broken"
```

## Coming from C#

| C# | Zell |
|---|---|
| `namespace App.Users;` + `using` | `namespace app.users` + `import` (an import shortens names; full paths always work) |
| classes (reference types) | structs are values; `new T{}` gives a `*T` on the heap |
| exceptions, `try`/`catch` | `T!` results, `?`, `try ... catch`; `panic` only for bugs |
| `async Task<T>` / `await` | `async func F() T` returns `Task<T>`; same `await` |
| `CancellationToken` parameters | implicit task context: `task.WithTimeout`, `task.NewCancel` |
| `AsyncLocal<T>` | `task.Key<T>()`, `task.With`, `task.Get` |
| LINQ | `slices` (`Map`, `Filter`, `Reduce`, `SortBy`, `GroupBy`) and lambdas |
| attributes + reflection | attributes + compile-time reflection (`reflect.Fields<T>()`): JSON, SQL mapping and DI without run-time cost |
| ASP.NET Core minimal APIs / controllers | `net.http` routing and middleware for now; an attribute-routed framework returns as the package `zell-lang/web` |
| Serilog | `log` |
| `IDisposable` / `using` | `func Drop()`: runs when the value goes away, no `using` needed |
| inheritance | composition: embedded structs and interfaces with default methods |
| `null` everywhere | `nil` only for pointers, interfaces, slices, maps and functions |

```zell
namespace guide.faq.csharp

import io

interface Notifier {
    func Send(to string, text string)
    // A default method, written in terms of the others.
    func SendAll(to []string, text string) {
        for t in to {
            this.Send(t, text)
        }
    }
}

struct ConsoleNotifier {
    func Send(to string, text string) {
        println(to + ":", text)
    }
}

async func notifyLater(n Notifier, ms int) {
    await task.Delay(ms)
    n.SendAll(["ann", "bob"], "hello")
}

async func main() {
    var n = ConsoleNotifier{}
    await notifyLater(n, 1)
    var adults = slices.Filter([12, 30, 45], (age int) => age >= 18)
    println(slices.Map(adults, (age int) => fmt.Format("{0}y", age)))
}
// Output:
// ann: hello
// bob: hello
// [30y 45y]
```

## Is there a garbage collector? What about cycles?

No collector, ever: memory is freed when the last reference goes. Cycles
need a `weak *T` back reference; `zell run --leak-check` finds the ones
you missed and names the field. See [memory.md](memory.md).

## Are there exceptions?

No. Expected failures are `T!` values; `panic` is for bugs and ends the
program (tests run each test in a child process, so a panic fails only
that test).

## Can I call C?

`unsafe.CallFunc` and `plugin` exist for the standard library's
debugger and low-level packages; they need `[capabilities] unsafe = true`
in the manifest. A proper FFI is on the roadmap.

## What platforms are supported?

Linux (x86-64 and arm64) is the primary target; macOS and Windows
(cross-compiled with mingw-w64) are in progress, see
[docs/PLATFORMS.md](../PLATFORMS.md).

## How fast is it?

On the CPU benchmarks of `benchmarks/` (recursion, maps, slices,
strings, floats, formatting) a `--release` build takes 0.56 to 0.97 times
Go's time; TCP echo and the HTTP server are on par with Go. The numbers
and how they were measured are in [benchmarks/README.md](../../benchmarks/README.md).

## Where is the API reference?

`zell doc` prints it in the terminal (`zell doc http.Client`), and
`zell doc std --html out/` generates a static site with search. The
documentation comes from the comments above declarations in `std/`.
