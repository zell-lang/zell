# Debugging

What to reach for, from the quickest to the most thorough: read the
error trace, look at the panic backtrace, log, check for leaks, step
through the program in the debugger, profile.

## Error traces

An error remembers where it was created and every `?` it passed through.
When `main` returns an error, the program prints the message and the
trace to stderr and exits with status 1:

```text
error: config: open app.toml: not found
  created at   app/fs.zl:42      open
  returned via app/config.zl:17  readConfig  ?("config")
  returned via app/main.zl:9     main
```

In code, `errors.Trace(err)` returns the same text and
`errors.Frames(err)` the records (see [errors.md](errors.md)):

```zell
namespace guide.debugging.trace

import io

func step(n int) int! {
    if n == 0 {
        return error("reached zero")
    }
    return step(n - 1)?
}

func main() {
    try step(2) catch err {
        println(err, "after", len(errors.Frames(err)), "frames")
        for f in errors.Frames(err) {
            println(" ", f.Func, f.Line > 0)
        }
    }
}
// Output:
// reached zero after 4 frames
//   step true
//   step true
//   step true
//   main true
```

## Panics

A panic (`panic(msg)`, an index out of range, a `nil` dereference,
division by zero) prints `panic: <message>` and a native backtrace, and
exits with status 2. Deferred calls and destructors do not run. Set
`ZELL_NO_BACKTRACE=1` to print only the message (the test suites do).

A panic is not an error: `try` and `?` do not catch it. The backtrace
names the source line and function of every Zell frame, like Go:

```
panic: runtime error: index out of range [5] with length 3

backtrace (most recent call first):
  main.zl:6 inner
  main.zl:10 main
./main(main+0x37)[0x452a47]
/lib64/libc.so.6(__libc_start_main+0x88)[0x7f799edec628]
```

The lines come from a small table in the executable (`.zell_lines`), so
they are there in stripped `--release` builds too; frames outside Zell code
(the runtime, libc) keep the `module(symbol+0xoffset)` form. An optimised
build inlines small functions, which then have no frame of their own: the
frame shows the innermost line. `ZELL_NO_LINES=1` prints symbols instead.
The table exists on Linux and FreeBSD; macOS and Windows still print
symbols with offsets. `llvm-strip --strip-all` removes the table (GNU
`strip` keeps it). A panic in a `web` handler without `UseRecover` ends the
task of the request; with it, the log entry's `PanicTrace` has the same
text.

## Logging

`log` is structured logging in the style of Serilog: message
templates are checked by the compiler, properties are kept as values
(for JSON sinks), and a disabled level costs nothing because the
arguments are not evaluated. `log.Info("...")` uses the default logger
(stderr); configure your own for anything else.

```zell
namespace guide.debugging.log

struct Order {
    Id    int
    Total float
}

func main() {
    var logger = log.New().MinimumLevel(log.Level.Debug).WriteTo.Console(log.ConsoleOptions{
        Template: "[{Level:u3}] {Message:lj}{NewLine}",
        Stdout: true,
    }).CreateLogger()
    var o = Order{Id: 7, Total: 19.5}
    logger.Info("order {@Order} paid: {Amount:0.00}", o, o.Total)
    logger.Debug("cache size {Size}", 128)
    logger.Verbose("not shown: below the minimum level")
}
// Output:
// [INF] order Order { Id: 7, Total: 19.5 } paid: 19.50
// [DBG] cache size 128
```

Sinks write text or JSON (CLEF) to the console or rolling files;
`log.Context.Push("RequestId", id)` adds a property to everything logged
by the current task and the tasks it starts.

A template's `{Exception}` prints an error as its message plus one line
saying where it was created; the whole return trace is written only for
`Debug` and `Verbose` events (the JSON sinks keep it in `ErrorTrace`).

## Leaks

Reference counting cannot free a reference cycle by itself (see
[memory.md](memory.md)): the compiler warns about types that can form one
(W0001), and the cycle collector frees the cycles that remain. Run with
`--leak-check` to see what is still alive at exit:

```sh
zell run --leak-check app.zl
```

At exit the program prints `leak: N objects` and, in a debug build, a
line per type still alive and the field that closes a cycle, and exits
with status 66. With the cycle collector on (the default,
`ZELL_CYCLES=auto`) it first collects once more, so what is reported is
what the collector cannot free either: objects kept by a global, by a
task that never finished, by a cycle through an `error` or a map shared
between threads. To see the cycles themselves, turn the collector off or
to manual. This program leaks two nodes then:

```zell
// check-only
namespace guide.debugging.leak

import io

struct Node {
    Name string
    Next *Node       // make this `weak *Node` and the leak is gone
}

func main() {
    var a = new Node{Name: "a"}
    var b = new Node{Name: "b", Next: a}
    a.Next = b
    println(a.Next.Name, b.Next.Name)
}
```

`ZELL_CYCLES=manual zell run --leak-check` prints:

```text
b a
leak: 2 objects
  2 x guide.debugging.leak.Node
  cycle through Node.Next (make one field of the cycle `weak *T` to break it)
  2 of them in unreachable reference cycles the cycle collector can free (ZELL_CYCLES=auto or runtime.CollectCycles())
```

In production, without `--leak-check`, start the program with
`ZELL_DIAG_HEAP=1` and serve `diag.ServeHeap` at `/debug/heap`: it
lists the types with the most live objects and how much each grew since
the previous request; a type that keeps growing is the leak.

## The debugger

Debug builds carry DWARF debug information. Three ways in:

```sh
zell debug run app.zl          # build with the in-process debug agent and debug it
zell debug app.zl              # build with DWARF and start lldb (or --gdb)
ZELL_DEBUG=:2345 ./app         # start the agent, then: zell debug attach :2345
```

`zell debug run` speaks gdb-style commands: `b file:line [if cond]`,
`b func`, `log file:line msg {expr}` (a logpoint), `c`, `n`, `s`, `o`
(step out), `p expr`, `locals`, `bt`, `up`/`down`, `tasks`, `task N`,
`catch panic`, `catch error [Type]`, `q`. Breakpoint conditions are
evaluated inside the process, so they are fast; `next` steps over an
`await` within the same task.

In VS Code, the extension in `editors/vscode` launches programs with the
agent (`"type": "zell"` in `launch.json`) and supports breakpoints,
conditions, logpoints, stepping, variables, tasks as threads, and
stopping on panics or on errors being created. With lldb or gdb, the
formatters in `tools/lldb` and `tools/gdb` show strings, slices, maps,
`T!`, enums and interfaces readably, and their `zell-rc <expr>` command
decodes the reference count of the object a value points at (unowned,
biased to a thread, cycle-collected, immortal).

## Looking at what the compiler did

```sh
zell check --dump-hir app.zl        # the typed program after semantic analysis
zell build app.zl --emit=llvm-ir    # the LLVM IR
```

## Profiling

`diag` writes CPU profiles in pprof format, exposes Prometheus
metrics and records trace spans (exported to stdout or OTLP):

```zell
namespace guide.debugging.profile

import io

func fib(n int) int {
    if n < 2 {
        return n
    }
    return fib(n - 1) + fib(n - 2)
}

func main() void! {
    var path = fs.Join(fs.TempDir(), "guide-cpu.pb")
    diag.StartCPUProfile(path)?
    var start = time.Now()
    var acc = 0
    for time.Since(start) < 200 {
        acc += fib(20)
    }
    var samples = diag.StopCPUProfile()?
    println("samples recorded:", samples > 0, acc > 0)
    fs.Remove(path)?
}
// Output:
// samples recorded: true true
```

Then `zell tool pprof -top guide-cpu.pb` (it runs `go tool pprof`)
shows where the time went. `diag.Runtime()` reports live objects,
allocations and scheduler state, and `diag.ServeMetrics` serves them with
your own counters at `/metrics`.
