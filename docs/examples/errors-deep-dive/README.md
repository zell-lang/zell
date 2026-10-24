# errors-deep-dive: errors through the layers of an application

A small shop with three layers, storage, service and api, and eleven
command lines that fail in every way an application fails: bad input,
a business rule, a missing record, a broken disk, and a bug that panics
in a task. Each layer adds what it knows, and the boundary turns each
failure into an exit code, a message for the user and, for the
unexpected ones, a trace for the developer.

What it shows:

- sentinel errors (`const ErrNotFound = error("not found")`) and
  `errors.Is`;
- typed errors that carry data (`ValidationError`, `OutOfStock`), one
  with a cause linked by an `Unwrap()` method (`DiskError`), and
  `errors.As`;
- wrapping with `?("context")` and `errors.Wrap`, and why the boundary
  searches the chain instead of switching on the type;
- converting an error between layers (`strconv`'s parse error becomes a
  `ValidationError` at the boundary);
- return traces from `errors.Frames`, printed only for unexpected errors;
- mapping errors to exit codes in one function, with a table test;
- a panic in a task observed as an error with `task.ErrPanic`, and the
  rules that make it so.

## Run it

```sh
cd docs/examples/errors-deep-dive
zell run
zell test
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `shop`) |
| `src/storage.zl` | `shop.storage`: sentinels, `DiskError`, the in-memory store |
| `src/service.zl` | `shop.service`: business rules, `ValidationError`, `OutOfStock`, the audit task |
| `src/api.zl` | `shop.api`: exit codes, reporting, the commands |
| `src/main.zl` | eleven command lines |
| `src/api_test.zl` | tests of the mapping (`zell test`) |

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "errors-deep-dive"
version = "0.1.0"
namespace = "shop"
entry = "src/main.zl"
```

## src/storage.zl: the lowest layer

Storage knows two things can go wrong: a record does not exist, or the
disk fails. The first is a sentinel, a constant error compared by
identity: `errors.Wrap(ErrNotFound, "order 42")` adds the key to the
message and keeps `ErrNotFound` in the chain.

The second is a typed error, `DiskError`, because it has data a caller
may want (the operation and the key). Its `Unwrap()` method returns the
sentinel `ErrUnavailable`, so a caller can ask either question:
`errors.As(err, &d)` for the details, `errors.Is(err, ErrUnavailable)`
for "is this an outage?". Its `cause` field is private: only storage
decides what a disk error wraps.

Order 2 sits on a broken sector, and order 9 was written by an old
version of the program, with a sku that has no batch part.

<!-- file: src/storage.zl -->
```zell title="src/storage.zl"
// The storage layer: orders and stock in memory, with one "disk" that
// fails on purpose. It defines the low-level errors: two sentinels and
// a typed error that carries its cause.
namespace shop.storage

// Sentinel errors are constants. Callers test for them with errors.Is,
// which searches the whole chain, so wrapping never hides them.
const ErrNotFound = error("not found")
const ErrUnavailable = error("storage unavailable")

// DiskError is a typed error: it carries data (the operation and key),
// and its Unwrap method links it to its cause, so errors.Is(err,
// ErrUnavailable) is true for it too.
struct DiskError {
    Op    string
    Key   string
    cause error

    func Message() string {
        return this.Op + " " + this.Key + ": disk error"
    }

    func Unwrap() error {
        return this.cause
    }
}

struct Order {
    Id  int
    Sku string
    Qty int
}

struct Store {
    orders map[int]Order
    stock  map[string]int
    bad    map[int]bool // orders on a broken sector
    nextId int

    static func New() *Store {
        var s = new Store{orders: map[int]Order{}, stock: map[string]int{}, bad: map[int]bool{}, nextId: 1}
        s.stock["widget-a1"] = 10
        s.stock["gadget-b2"] = 3
        _ = s.Insert("widget-a1", 1)
        _ = s.Insert("gadget-b2", 1)
        s.bad[2] = true
        // An order written by an old version: its sku has no batch part.
        s.orders[9] = Order{Id: 9, Sku: "legacy", Qty: 1}
        return s
    }

    func Stock(sku string) int! {
        var (n, ok) = this.stock[sku]
        if !ok {
            // Wrap adds context and keeps the sentinel in the chain.
            return errors.Wrap(ErrNotFound, "sku " + strconv.Quote(sku))
        }
        return n
    }

    func Insert(sku string, qty int) Order {
        var o = Order{Id: this.nextId, Sku: sku, Qty: qty}
        this.orders[o.Id] = o
        this.stock[sku] -= qty
        this.nextId += 1
        return o
    }

    func Load(id int) Order! {
        if this.bad[id] {
            return DiskError{Op: "read", Key: fmt.Format("order/{0}", id), cause: ErrUnavailable}
        }
        var (o, ok) = this.orders[id]
        if !ok {
            return errors.Wrap(ErrNotFound, fmt.Format("order {0}", id))
        }
        return o
    }
}
```

## src/service.zl: rules and context

The service defines the errors of its own level: `ValidationError` (the
request is wrong) and `OutOfStock` (a valid request that cannot be
served). They are returned as they are; an error is just a value whose
type has a `Message()` method.

Errors from storage are passed on with `?("context")`, which wraps them
with what the service was doing: `get order 42: order 42: not found`.
Nothing is lost by wrapping; `errors.Is` and `errors.As` see through it.

`Audit` runs `check` in a task of its own and awaits it. `check` has a
bug: it indexes `parts[1]` for a sku without a dash. That is a panic,
not an error, and DESIGN.md "Паника" decides what happens:

1. A panic ends the task it happens in. Nothing recovers it, and `try`
   does not see it where it happens.
2. The task is unwound: its pending `defer`s run and its async frames
   release what they own (`parts` here), so nothing leaks.
3. Its owner learns about it. Awaiting a `Task<T!>` gives an error with
   `task.ErrPanic` in its chain; awaiting a `Task<T>` (no `!`) panics
   again in the awaiting task; a task nobody awaits prints the panic.
4. `check` suspends (`await task.Yield()`) before the bug. An async
   function runs in its caller's task until its first suspension, and a
   panic before that point belongs to the caller: here, `main`, which
   would end the process with status 2.

Because `check` returns `string!`, the panic arrives in `Audit` as an
ordinary error, and `?("audit")` wraps it like any other.

<!-- file: src/service.zl -->
```zell title="src/service.zl"
// The service layer: business rules. It adds its own typed errors for
// what a caller did wrong, and wraps storage errors with context that
// says what the service was doing when they happened.
namespace shop.service

import shop.storage

// ValidationError: the request itself is wrong. Nothing to retry.
struct ValidationError {
    Field  string
    Reason string

    func Message() string {
        return this.Field + " " + this.Reason
    }
}

// OutOfStock: a valid request that cannot be served right now.
struct OutOfStock {
    Sku  string
    Want int
    Have int

    func Message() string {
        return fmt.Format("{0}: want {1}, have {2}", this.Sku, this.Want, this.Have)
    }
}

struct Service {
    store *Store

    static func New(store *Store) *Service {
        return new Service{store: store}
    }

    func Place(sku string, qty int) Order! {
        if qty <= 0 {
            return ValidationError{Field: "qty", Reason: "must be positive"}
        }
        // ?("...") returns the error wrapped with this context; the
        // sentinel inside (ErrNotFound) stays visible to errors.Is.
        var have = this.store.Stock(sku)?("place order")
        if have < qty {
            return OutOfStock{Sku: sku, Want: qty, Have: have}
        }
        return this.store.Insert(sku, qty)
    }

    func Get(id int) Order! {
        return this.store.Load(id)?(fmt.Format("get order {0}", id))
    }

    // Audit checks an order in a task of its own. A panic there ends
    // only that task, and because Audit returns Task<string!>, awaiting
    // it gives an error with task.ErrPanic in its chain instead of
    // ending the program.
    async func Audit(id int) string! {
        var o = this.Get(id)?
        return await check(o)?("audit")
    }
}

// check is a separate async function, so it is a separate task once it
// suspends. The panic below happens after the first suspension: before
// it, the function still runs in its caller's task, and the panic would
// belong to the caller (DESIGN.md, "Паника").
async func check(o Order) string! {
    await task.Yield()
    var parts = strings.Split(o.Sku, "-")
    // A bug, not an expected failure: old skus have no batch part, and
    // parts[1] is out of range. Bugs panic; they are not errors.
    var batch = parts[1]
    return o.Sku + " batch " + batch + " ok"
}
```

## src/api.zl: the boundary

`ExitCode` is the one place that knows how errors map to exit codes
(the codes follow BSD `sysexits.h`). It asks `errors.As` and `errors.Is`
because they search the whole chain. A `switch err { case OutOfStock s:
... }` would test only the outermost error, which after a `?("...")` is
the wrapper, so it suits errors that are returned unwrapped and is not
used here.

`Report` prints the message for every error, and stops there for the
expected ones (usage, not found, out of stock): those are for the user,
and a trace would be noise. For the rest, an outage or a bug, it prints
the details the developer needs: the panic's own message from
`task.PanicMessage`, the failed disk operation from `errors.As`, and
the return trace from `errors.Frames`: where the error was created and
every `?` it passed through, with the context each one added. Traces
cost one pointer per step and need no stack unwinding.

`number` converts an argument. A failed `strconv.Atoi` is an
implementation detail at this level: what the user needs to hear is
that the argument is wrong, so `try ... catch` turns it into a
`ValidationError` and the parse error itself is dropped.

`Run` is the boundary for one command: `exec` returns its errors with
`?`, and `Run` handles them in one `try ... catch`.

<!-- file: src/api.zl -->
```zell title="src/api.zl"
// The api layer: the boundary. Errors from below stop here: each one
// becomes an exit code and a message for the user, and the unexpected
// ones are printed with their trace for the developer.
namespace shop.api

import io
import shop.service
import shop.storage

// Exit codes, after the BSD sysexits.h conventions.
const ExitOk = 0
const ExitFailure = 1
const ExitUsage = 64
const ExitNotFound = 65
const ExitOutOfStock = 69
const ExitSoftware = 70
const ExitTempFail = 75

// ExitCode classifies an error. errors.As and errors.Is search the whole
// chain (wrappers from ?("...") and errors.Wrap, and Unwrap methods), so
// the layers in between can add context freely. A type switch would
// only see the outermost error, which after ?("...") is the wrapper.
func ExitCode(err error) int {
    var v ValidationError
    if errors.As(err, &v) {
        return ExitUsage
    }
    var s OutOfStock
    if errors.As(err, &s) {
        return ExitOutOfStock
    }
    if errors.Is(err, ErrNotFound) {
        return ExitNotFound
    }
    if errors.Is(err, ErrUnavailable) {
        return ExitTempFail
    }
    if errors.Is(err, task.ErrPanic) {
        return ExitSoftware
    }
    return ExitFailure
}

// Expected errors are the user's business: a message is enough. The
// rest are bugs or outages: the developer needs to know where they
// came from.
func expected(code int) bool {
    return code == ExitUsage || code == ExitNotFound || code == ExitOutOfStock
}

// Report prints what the user sees and returns the exit code.
func Report(err error) int {
    var code = ExitCode(err)
    println("  error:", err)
    if expected(code) {
        return code
    }
    if errors.Is(err, task.ErrPanic) {
        // The panic's own message, without the chain around it.
        println("  panic:", task.PanicMessage(err))
    }
    var d DiskError
    if errors.As(err, &d) {
        println("  failed operation:", d.Op, d.Key)
    }
    // The return trace: where the error was created and every ? it went
    // through, with the context each one added.
    for f in errors.Frames(err) {
        var ctx = ""
        if f.Context != "" {
            ctx = " ?(\"" + f.Context + "\")"
        }
        println(fmt.Format("    at {0}:{1} {2}{3}", fs.Base(f.File), f.Line, f.Func, ctx))
    }
    return code
}

// number converts an argument. strconv's own error ("strconv.Atoi:
// parsing ...") is an implementation detail; at the boundary a bad
// argument is the user's mistake, so it becomes a ValidationError.
func number(field string, s string) int! {
    var n = try strconv.Atoi(s) catch err {
        return ValidationError{Field: field, Reason: "must be a number, not " + strconv.Quote(s)}
    }
    return n
}

// Run executes one command line, reports its error if it fails, and
// returns its exit code.
async func Run(svc *Service, args []string) int {
    println("$ shop", strings.Join(args, " "))
    var code = ExitOk
    try await exec(svc, args) catch err {
        code = Report(err)
    }
    println("  exit", code)
    return code
}

// exec is the command itself. Every ? in it returns the error to Run.
async func exec(svc *Service, args []string) void! {
    switch args[0] {
        case "place":
            var o = svc.Place(args[1], number("qty", args[2])?)?
            println("  placed order", o.Id)
        case "get":
            var o = svc.Get(number("id", args[1])?)?
            println("  order", o.Id, o.Sku, "x", o.Qty)
        case "audit":
            println("  " + await svc.Audit(number("id", args[1])?)?)
        default:
            return ValidationError{Field: "command", Reason: strconv.Quote(args[0]) + " is unknown"}
    }
}
```

## src/main.zl: eleven command lines

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// errors-deep-dive: one small shop, three layers, and every way a
// request can fail on its way up from storage to the user.
namespace shop

import io
import shop.api
import shop.service
import shop.storage

async func main() {
    var svc = Service.New(Store.New())
    var commands = [
        ["place", "widget-a1", "2"],
        ["place", "widget-a1", "0"],
        ["place", "gadget-b2", "50"],
        ["place", "gizmo-c3", "1"],
        ["place", "widget-a1", "two"],
        ["get", "1"],
        ["get", "42"],
        ["get", "2"],
        ["audit", "1"],
        ["audit", "9"],
        ["refund", "1"],
    ]
    var worst = 0
    for c in commands {
        var code = await Run(svc, c)
        if code > worst {
            worst = code
        }
    }
    // A real CLI would end with os.Exit(code) for its one command; this
    // demo runs eleven and exits 0, so `zell run` succeeds.
    println("highest exit code:", worst)
}
```

## src/api_test.zl: tests of the mapping

The mapping is what scripts and operators rely on, so it has a table
test with one row per kind of error, built the way the layers build
them: bare, wrapped once or twice, linked by `Unwrap`, or carrying a
panic. The async test drives the real service.

<!-- file: src/api_test.zl -->
```zell title="src/api_test.zl"
// Tests for the error mapping, the part every caller depends on. The
// table builds each error the way the layers would: wrapped, typed, or
// both, and checks the exit code the boundary picks for it.
namespace shop.tests

import shop.api
import shop.service
import shop.storage

struct Case {
    Name string
    Err  error
    Code int
}

// diskError is a real DiskError, made by the store (its cause field is
// private to shop.storage, so a test cannot build one by hand).
func diskError() error {
    try Store.New().Load(2) catch err {
        return err
    }
    return error("order 2 should be on the broken sector")
}

@test func TestExitCodes(t *testing.T) {
    var cases = [
        Case{Name: "validation", Err: ValidationError{Field: "qty", Reason: "x"}, Code: ExitUsage},
        Case{Name: "out of stock", Err: OutOfStock{Sku: "a", Want: 2, Have: 1}, Code: ExitOutOfStock},
        Case{Name: "sentinel", Err: ErrNotFound, Code: ExitNotFound},
        Case{Name: "wrapped sentinel", Err: errors.Wrap(ErrNotFound, "order 1"), Code: ExitNotFound},
        Case{Name: "typed, two layers deep", Err: errors.Wrap(errors.Wrap(OutOfStock{Sku: "a"}, "one"), "two"), Code: ExitOutOfStock},
        Case{Name: "cause via Unwrap", Err: diskError(), Code: ExitTempFail},
        Case{Name: "panic", Err: errors.Wrap(task.ErrPanic, "audit"), Code: ExitSoftware},
        Case{Name: "anything else", Err: error("boom"), Code: ExitFailure},
    ]
    for c in cases {
        t.Run(c.Name, func(t *testing.T) {
            testing.Equal(t, ExitCode(c.Err), c.Code)
        })
    }
}

@test async func TestServiceErrorsReachTheBoundary(t *testing.T) {
    var svc = Service.New(Store.New())
    try svc.Place("widget-a1", 100) catch err {
        var s OutOfStock
        testing.True(t, errors.As(err, &s), "OutOfStock expected")
        testing.Equal(t, s.Have, 9)
    }
    try svc.Get(2) catch err {
        testing.Equal(t, ExitCode(err), ExitTempFail)
        testing.Contains(t, err.Message(), "get order 2")
    }
    try await svc.Audit(9) catch err {
        testing.True(t, errors.Is(err, task.ErrPanic), "the audit task panicked")
        testing.Contains(t, task.PanicMessage(err), "index out of range")
    }
}
```

## Output

Expected errors print one line; the disk error and the panic print the
trace. The trace of the panic starts at `Service.Audit`: the error was
created when the panicking task was awaited there.

<!-- file: expected.out -->
```text title="expected.out"
$ shop place widget-a1 2
  placed order 3
  exit 0
$ shop place widget-a1 0
  error: qty must be positive
  exit 64
$ shop place gadget-b2 50
  error: gadget-b2: want 50, have 2
  exit 69
$ shop place gizmo-c3 1
  error: place order: sku "gizmo-c3": not found
  exit 65
$ shop place widget-a1 two
  error: qty must be a number, not "two"
  exit 64
$ shop get 1
  order 1 widget-a1 x 1
  exit 0
$ shop get 42
  error: get order 42: order 42: not found
  exit 65
$ shop get 2
  error: get order 2: read order/2: disk error
  failed operation: read order/2
    at storage.zl:71 Store.Load
    at service.zl:50 Service.Get ?("get order 2")
    at api.zl:107 exec
    at api.zl:93 Run
  exit 75
$ shop audit 1
  widget-a1 batch a1 ok
  exit 0
$ shop audit 9
  error: audit: panic: runtime error: index out of range [1] with length 1
  panic: runtime error: index out of range [1] with length 1
    at service.zl:59 Service.Audit ?("audit")
    at api.zl:110 exec
    at api.zl:93 Run
  exit 70
$ shop refund 1
  error: command "refund" is unknown
  exit 64
highest exit code: 75
```

## Next

- [Errors](../../guide/errors.md): `T!`, `?`, `try`, wrapping and traces
  in detail.
- [Async](../../guide/async.md#when-a-task-panics): panics in tasks.
- [todo-cli](../todo-cli/README.md): exit codes in a real command-line
  tool.
