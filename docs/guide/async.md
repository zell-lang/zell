# Async programming

Zell's concurrency model is tasks on a thread pool with language-level
`async`/`await`, like C# or Rust's Tokio, but with Go's habit of making
the simple thing simple: the standard library's I/O (sockets, HTTP,
files through the async APIs, timers) never blocks a worker thread.

## Tasks

An `async func` returns `Task<T>`. **Calling it starts it**; `await`
waits for the result. `await` is only allowed inside `async` functions,
and `main` may be `async`.

```zell
namespace guide.async.tasks

import io

async func work(name string, ms int) string {
    await task.Delay(ms)
    return name
}

async func main() {
    var t = work("first", 30)       // running now
    var u = work("second", 10)
    println("started both")
    println(await u, await t)

    // task.All waits for several tasks; task.Any for the first one,
    // cancelling the others.
    var (a, b) = await task.All(work("a", 5), work("b", 1))
    println(a, b)
    println(await task.Any(work("slow", 50), work("fast", 1)))
}
// Output:
// started both
// second first
// a b
// fast
```

Errors and `await` compose: `await f()?` waits and propagates an error
of a `Task<T!>`. `task.All` of fallible tasks is itself fallible; the
first error cancels the remaining tasks.

```zell
namespace guide.async.errors

import io

async func fetch(id int) string! {
    await task.Delay(id)
    if id == 2 {
        return error("item 2 is missing")
    }
    return "item " + strconv.Itoa(id)
}

async func main() void! {
    println(await fetch(1)?)
    try await task.All(fetch(1), fetch(2), fetch(3)) catch err {
        println("all:", err)
    }
}
// Output:
// item 1
// all: item 2 is missing
```

The scheduler runs tasks on one worker per core and switches them at
`await` points. A long CPU-bound loop should `await task.Yield()` now
and then; `task.Spawn(f)` starts `f` on another worker, and `task.Go`
does the same for functions without a result.

## Channels

`task.Channel<T>(n)` makes a FIFO channel with a buffer of `n` values;
`n = 0` makes it unbuffered (every `Send` waits for its `Recv`). Copies of
a channel share it. `Recv` fails once the channel is closed and drained,
and `for v in ch` receives until then. Sending on a closed channel
panics.

```zell
namespace guide.async.channels

import io

async func squares(src task.Channel<int>, out task.Channel<int>) {
    for v in src {
        await out.Send(v * v)
    }
    out.Close()
}

async func main() {
    var nums = task.Channel<int>(0)
    var results = task.Channel<int>(8)
    var worker = squares(nums, results)
    for i in 1..5 {
        await nums.Send(i)
    }
    nums.Close()
    var sum = 0
    for r in results {
        sum += r
    }
    await worker
    println("sum of squares:", sum)
}
// Output:
// sum of squares: 30
```

## Shared state: Mutex and WaitGroup

Prefer channels for passing data between tasks; when tasks share a
value, protect it with `task.Mutex` (its zero value is unlocked). It
follows Go's `sync.Mutex`: `Unlock` wakes the oldest waiter, but a running
task may take the mutex first, so the order of acquisition is not
guaranteed; a waiter that has waited more than 1 ms switches the mutex to
FIFO hand-off until the queue drains, so nobody starves. `task.WaitGroup` waits for a group of tasks.
`sync` adds `RwMutex`, `Semaphore`, `Once`, `Barrier` and atomics.

```zell
namespace guide.async.mutex

import io

struct Counter {
    mu task.Mutex
    n  int

    async func Inc() {
        await this.mu.Lock()
        this.n += 1
        this.mu.Unlock()
    }
}

async func worker(c *Counter, wg *task.WaitGroup, times int) {
    for _ in 0..times {
        await c.Inc()
        await task.Yield()
    }
    wg.Done()
}

async func main() {
    var c = new Counter{}
    var wg = new task.WaitGroup{}
    for _ in 0..8 {
        wg.Add(1)
        _ = worker(c, wg, 100)
    }
    await wg.Wait()
    println(c.n)
}
// Output:
// 800
```

## Cancellation and timeouts

Cancellation is cooperative and structured: it belongs to the **task
context**, which every task inherits from the code that started it.

- `task.WithTimeout(ms, f)` runs `f` in a scope that is cancelled after
  `ms` milliseconds.
- `task.NewCancel()` returns a handle: run code in `h.Context` (with
  `task.RunWith` or `task.SpawnWith`) and call `h.Cancel()` from outside
  (a stop button, server shutdown).
- In a cancelled scope, waits that have a result (`Recv`, network reads)
  fail with `task.ErrCancelled`, and `task.Delay` returns at once.
  Long computations call `task.CheckCancelled()?` between steps.
- `task.Any` cancels the losers; `task.All` cancels the rest on the first
  error.

```zell
namespace guide.async.cancel

import io

async func consume(ch task.Channel<int>) string {
    var total = 0
    for {
        var v = try await ch.Recv() catch err {
            return "stopped at " + strconv.Itoa(total) + ", cancelled: " + fmt.Format("{0}", errors.Is(err, task.ErrCancelled))
        }
        total += v
    }
}

async func main() {
    var ch = task.Channel<int>(4)
    var h = task.NewCancel()
    var c = task.SpawnWith(h.Context, func() Task<string> {
        return consume(ch)
    })
    await ch.Send(1)
    await ch.Send(2)
    await task.Delay(20)
    h.Cancel()
    println(await c)

    var result = try await task.WithTimeout(10, async func() string! {
        for _ in 0..1000 {
            await task.Delay(1)
            task.CheckCancelled()?
        }
        return "finished"
    }) else "gave up"
    println(result)
}
// Output:
// stopped at 3, cancelled: true
// gave up
```

`await task.Timeout(t, ms)` is the non-cancelling variant: it reports
whether `t` finished within `ms` milliseconds, and `t` keeps running.

## Values in the task context

`task.Key<T>()` creates a typed key; `task.With(key, value, f)` runs `f`
with the value set, and every task started inside sees it through
`task.Get(key)` (like C#'s `AsyncLocal`). `log` uses this for request
properties and `diag` for the current trace span.

```zell
namespace guide.async.context

import io

var RequestId = task.Key<string>()

async func handler() string {
    await task.Delay(1)
    var id = task.Get(RequestId)
    if id == nil {
        return "no request"
    }
    return "handling " + *id
}

async func main() {
    println(await task.With(RequestId, "req-42", handler))
    println(await handler())
}
// Output:
// handling req-42
// no request
```

## Timers

`time.NewTicker(d)` fires every period (`for await t.Tick()` style loops)
and `time.NewTimer(d)` once; both are stopped with `Stop()`. In async
code wait with `task.Delay(ms)`; `time.Sleep` and `time.SleepFor` block
the thread and belong in synchronous code.

```zell
namespace guide.async.timers

import io

async func main() {
    var t = time.NewTicker(time.Milliseconds(5))
    var n = 0
    for await t.Tick() {
        n += 1
        if n == 3 {
            t.Stop()
        }
    }
    println("ticks:", n)
}
// Output:
// ticks: 3
```

## When a task panics

A panic ends only the task it happens in. Its pending `defer`s run
(a `defer m.Unlock()` releases the mutex), and the owner of the task
learns about it: awaiting a `Task<T!>` gives an error wrapping
`task.ErrPanic`, awaiting a `Task<T>` panics again in the awaiter, and a
task nobody awaits prints the panic to stderr. An async function runs in
its caller's task until it first suspends, so a panic before that point
belongs to the caller.

```zell
namespace guide.async.panics

import io

async func risky(n int) int! {
    await task.Yield()
    defer println("cleanup runs")
    var xs = [1, 2, 3]
    return xs[n]
}

async func main() {
    var v = try await risky(1) catch err {
        return
    }
    println("got", v)
    var w = try await risky(7) catch err {
        println("task panicked:", errors.Is(err, task.ErrPanic))
        println(task.PanicMessage(err))
        return
    }
    println("unreachable", w)
}
// Output:
// cleanup runs
// got 2
// cleanup runs
// task panicked: true
// runtime error: index out of range [7] with length 3
```

## What things cost

Rough costs on a 16-core x86-64 machine, from the async benchmarks
(`benchmarks/README.md`, "Async benchmarks", the per-worker run queue and
runtime channel/mutex pass):

| Operation | Zell | Go equivalent |
|---|---|---|
| a suspended task (memory) | ~0.8 KB | ~2.6 KB per parked goroutine |
| `async` call that does not suspend | ~55 ns | ~1 ns function call |
| uncontended `task.Mutex` lock + unlock | ~4 ns | ~4 ns `sync.Mutex` |
| contended `task.Mutex` (8 tasks), per increment | ~11 ns | ~20 ns |
| unbuffered channel message (ping-pong) | ~130 ns | ~90 ns |
| resuming a suspended `await` | ~75 ns | ~6 ns goroutine switch |

Tasks are cheap to keep around in large numbers, and a hand-off between
tasks (a channel message, a woken mutex waiter) runs the woken task
next on the same worker without a queue in between; resuming after an
`await` is still the expensive part next to a plain call, so prefer
batching work per message and buffered channels on hot paths.
