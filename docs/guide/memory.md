# Memory without a garbage collector

Zell frees memory deterministically, with no tracing garbage collector.
Most of the time you do not think about it: values are freed when nothing
refers to them any more. This page explains what happens underneath, the
two tools you use for special cases (`Drop` and `weak`), the cycle
collector that frees reference cycles you did not break, arenas for bulk
allocation, and the one rule the compiler enforces about pointers into
the middle of objects.

## Values and references

- **Values** (structs, fixed arrays, numbers, tuples) live in variables
  and are copied on assignment, like Go structs.
- **Heap objects** (`new T{}`, slices, maps, strings, closures, errors,
  values stored in interfaces, async frames) are *reference counted*:
  each object knows how many references point to it and is freed the
  moment the count drops to zero.
- The compiler removes most count updates: parameters are borrowed
  (no count change on a call), and `s += x` or `xs = append(xs, x)`
  grow in place when nothing else shares the buffer.

```zell
namespace guide.memory.values

import io

struct Point {
    X int
    Y int
}

func main() {
    var a = Point{X: 1, Y: 2}
    var b = a              // a copy
    b.X = 10
    println(a.X, b.X)

    var p = new Point{X: 1}
    var q = p              // the same object, two references
    q.X = 10
    println(p.X, q.X)

    var xs = [1, 2, 3]
    var ys = xs            // slices share their buffer, as in Go
    ys[0] = 100
    println(xs[0])
}
// Output:
// 1 10
// 10 10
// 100
```

## Destructors: `Drop`

A struct with a `func Drop()` method has a destructor. It runs exactly
once, as soon as the value is gone: at the end of its block for a value
in a variable, when the last reference disappears for a heap object.
Use it for resources: files, sockets, locks, temporary directories.

```zell
namespace guide.memory.drop

import io

struct TempFile {
    name string

    func Drop() {
        println("removing", this.name)
    }
}

func create(name string) TempFile {
    println("creating", name)
    return TempFile{name: name}
}

func main() {
    var a = create("a")
    {
        var b = create("b")
        println("using", b.name)
    }                          // b is dropped here
    var shared = new TempFile{name: "c"}
    var again = shared
    shared = nil
    println("still alive:", again.name)
    again = nil                // the last reference: dropped now
    println("end of main", a.name)
}
// Output:
// creating a
// creating b
// using b
// removing b
// still alive: c
// removing c
// end of main a
// removing a
```

Values with a destructor are **move-only**: you create them in place
(a literal or a function result) and pass them on by value, which
transfers ownership; to share one, use a pointer (`&x` or `new T{}`).
Copying one is a compile error, because two copies would close the same
file twice:

```zell
// error: cannot copy
namespace guide.memory.dropcopy

struct Handle {
    fd int

    func Drop() {}
}

func main() {
    var a = Handle{fd: 3}
    var b = a
    _ = b
}
```

Destructors also run on early exits (`return`, `?`, `break`), in reverse
order of declaration. They do not run on a `panic`.

### `Drop` or `defer Close()`

Types from the standard library that own a resource (files, sockets,
database connections, rows) have a `Drop` that closes them, so they are
closed at the end of the block without any code from you. Do not write
`defer f.Close()` for that. `Close()` is for the cases where you want to
see the close error (a failed flush on a written file, say): call it
explicitly and handle the result. Write your own `Close` so that `Drop`
skips what it already closed, as below.

```zell
namespace guide.memory.close

import io

struct Conn {
    name   string
    closed bool

    func Close() void! {
        if this.closed {
            return
        }
        this.closed = true
        println("closing", this.name)
        return error("flush failed on " + this.name)
    }

    func Drop() {
        if !this.closed {
            this.closed = true
            println("dropping", this.name)
        }
    }
}

func main() {
    {
        var a = Conn{name: "a"}
        println("using", a.name)
    }                                  // Drop closes it here
    var b = Conn{name: "b"}
    try b.Close() catch err {          // explicit: the error is visible
        println("error:", err)
    }
}
// Output:
// using a
// dropping a
// closing b
// error: flush failed on b
```

Not every type has a `Drop`. `database.sql` transactions (`Tx`) and
pools (`DB`) do not close themselves, so `defer tx.Rollback()` (a no-op
after `Commit`) and `defer db.Close()` stay as they are in
`examples/sql.zl`; `sql.Rows` do have a `Drop`. Check the type's
documentation before writing the `defer`.

## Cycles and `weak`

Reference counting cannot free a cycle by itself: a parent pointing to
a child that points back keeps both alive. Make the back reference
`weak *T`: it does not keep the object alive, reads as `nil` once the
object is gone, and converts from `*T` implicitly.

```zell
namespace guide.memory.weak

import io

struct Node {
    Name     string
    Parent   weak *Node
    Children []*Node

    func Drop() {
        println("free", this.Name)
    }
}

func build() {
    var root = new Node{Name: "root"}
    var leaf = new Node{Name: "leaf", Parent: root}
    root.Children = append(root.Children, leaf)
    var p = leaf.Parent        // *Node (or nil)
    println(leaf.Name, "belongs to", p.Name)
}

func main() {
    build()
    println("done")
}
// Output:
// leaf belongs to root
// free root
// free leaf
// done
```

## Cycles the compiler warns about, and the cycle collector

The compiler looks at your types and warns when values of a type *can*
point back to themselves through strong references:

```text
warning: W0001 type Session can form a reference cycle through field Peer (*Session); consider weak *Session or a Drop that breaks the cycle
```

It warns once per type, at the first field on the cycle, and counts
pointers, slices, maps, closures (a `func` field holding a lambda that
captures the object) and interfaces (the types your program stores in
them). What to do:

- if the reference points "back" or "up" (a parent, an owner, an
  observer list), make it `weak *T` — then the compiler stays quiet,
  because a `weak` reference into the cycle shows the ownership
  direction;
- if the data really forms cycles (a graph), put `@allowCycle` on the
  type to silence the warning; the collector still handles it;
- if the type is a tree or a list whose shape you control and it never
  forms a cycle, put `@acyclic` on it: no warning, and its objects skip
  the cycle collector entirely (plain reference counting). It is a
  promise: break it and the cycle leaks — `zell run --leak-check` then
  reports `type Node is @acyclic but was found in a cycle`;
- `--deny-warnings` turns warnings into errors for CI.

Whatever is left is handled at run time by the **cycle collector**. It
is not a tracing garbage collector: it only ever looks at objects of the
types W0001 names, starting from the ones whose reference count recently
went down without reaching zero, and never scans the rest of the heap.
When it finds a group of such objects that only reference each other, it
runs all their `Drop` methods (each one still sees intact fields) and
frees them:

```zell
namespace guide.memory.cycles

import io

struct Session {
    Id   int
    Peer *Session

    func Drop() {
        println("drop session", this.Id)
    }
}

func pair() {
    var a = new Session{Id: 1}
    var b = new Session{Id: 2, Peer: a}
    a.Peer = b              // a -> b -> a: reference counting alone keeps both
}

func main() {
    pair()
    println("after pair")
    println("freed", runtime.CollectCycles())
}
// Output:
// after pair
// drop session 2
// drop session 1
// freed 2
```

When it runs, set with `ZELL_CYCLES` in the environment:

- `auto` (the default): by itself, once enough possible cycle roots were
  recorded (a few thousand; the number adapts to how much garbage it
  finds), at the next allocation of such an object. The thread that runs
  it pauses for as long as the examined objects take; other threads keep
  going.
- `manual`: only when you call `runtime.CollectCycles()`.
- `off`: never — plain reference counting, as if the collector did not
  exist.

Freeing objects as soon as their count drops to zero stays the rule:
only objects in unreachable cycles wait for the collector, so a `Drop`
that must run at a precise moment (closing a file) belongs to a type
whose cycles you break with `weak`. Cycles through `error` values, tasks
and (in programs with several threads) maps are not collected.
`runtime.CycleStats()` reports what the collector did.

`zell run --leak-check` reports the objects still alive at exit (and
exits with status 66); with the collector in `auto` mode it collects one
last time first, and in `manual` mode it tells you how many of the leaked
objects are cycles the collector could have freed. The test suites of the
compiler run every example this way.

## Arenas

For work with a clear lifetime (one request, one parse) `arena`
allocates many objects in one region and frees them all at once. Objects
in an arena have no reference count, so allocation is a pointer bump.

```zell
namespace guide.memory.arena

import io

struct Token {
    Text string
    Pos  int
}

func tokenize(a arena.Arena, s string) []*Token {
    var out []*Token = []
    for i, w in strings.Fields(s) {
        var t = a.New<Token>()
        t.Text = a.String(w)
        t.Pos = i
        out = append(out, t)
    }
    return out
}

func main() {
    var a = arena.New()
    defer a.Free()
    var toks = tokenize(a, "let x = 42")
    println(len(toks), toks[3].Text, a.Len())
}
// Output:
// 4 42 8
```

- `a.New<T>()` returns a zeroed `*T`, `a.Alloc<T>(n)` a `[]T`,
  `a.String(s)` a copy of `s`, all inside the arena.
- `a.Free()` runs the destructors of the arena's objects and releases
  everything; the arena can be reused. When the last copy of the `Arena`
  value disappears, the same happens without `Free`.
- Pointers into an arena must not be used after `Free` (this is not
  checked yet), and an arena is not thread-safe.

## Pointers into the middle of objects

`&x.field`, `&slice[i]` and `&array[i]` are *interior pointers*: they
point inside another object rather than to an object of their own. You
can use them freely while the owner is certainly alive: dereference
them, read and write through them, pass them to a function that does
not keep them, or store them in a local variable (which keeps the owner
alive until the end of its block).

```zell
namespace guide.memory.interior

import io

struct Account {
    Balance int
}

func deposit(b *int, amount int) {
    *b += amount               // borrows: does not keep the pointer
}

func main() {
    var accounts = [Account{Balance: 10}, Account{Balance: 20}]
    deposit(&accounts[1].Balance, 5)
    var first = &accounts[0]
    first.Balance += 1
    accounts = append(accounts, Account{})
    println(accounts[0].Balance, accounts[1].Balance, first.Balance)
}
// Output:
// 11 25 11
```

What you cannot do is keep an interior pointer beyond that: store it in
a struct field, a slice, a map or a closure, return it, or pass it to a
function that stores it. The compiler rejects those with "cannot keep an
interior pointer"; allocate the value with `new` instead.

```zell
// error: cannot keep an interior pointer
namespace guide.memory.interiorerror

struct Inner {
    V int
}

struct Outer {
    In Inner
}

struct Ref {
    P *Inner
}

func main() {
    var o = new Outer{}
    var r = Ref{P: &o.In}
    _ = r
}
```

## What is guaranteed

- Memory and resources are released deterministically: when the last
  reference goes, or at the end of the block.
- No use-after-free and no dangling pointers in safe code; `nil`
  dereference and out-of-range indexing panic with a message instead of
  corrupting memory; there is no pointer arithmetic (`unsafe` is the
  explicit, capability-gated exception).
- Counts are thread-safe, so values can be shared between tasks; a count
  used by one worker thread only is updated without atomic instructions
  (biased counting), and mutable shared state still needs a `task.Mutex`
  or a channel.
- Freed memory goes back to the operating system: a reserve up to the
  recent peak is kept for reuse, the rest is returned within about half a
  second (`runtime.FreeOSMemory()` returns it at once;
  `diag.Runtime()` shows `HeapInUse`, `HeapRetained`, `HeapReleased`).
