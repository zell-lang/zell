# A tour of Zell

Twenty minutes, top to bottom. Every program on this page is complete:
copy it into a file, run `zell run file.zl`, and you get the output shown
in its `// Output:` comment (the guides are tested by
`compiler/tests/guide.rs`, so they stay correct).

Zell is a compiled language with Go's simplicity, C#-style namespaces and
`var` inference, methods inside structs, `T!` errors instead of
`if err != nil`, `async`/`await` on a task scheduler, and no garbage
collector (reference counting with compile-time ownership). The language
spec is [DESIGN.md](../../DESIGN.md) (in Russian); this guide is the
English introduction.

## Hello, world

```zell
namespace tour.hello

import io

func main() {
    println("Hello, Zell!")
}
// Output:
// Hello, Zell!
```

Every file starts with `namespace`. `import io` makes the members of
`io` (here `println`) available by their short names; without the
import you would write `io.println`. The standard library namespaces are
top-level, so `strings.Split` or `fmt.Format` work with no import at all.
Unused imports are compile errors, as in Go.

## Variables and types

```zell
namespace tour.vars

import io

const Pi = 3.14159

func main() {
    var n = 42              // int (64-bit)
    var name = "zell"       // string (immutable UTF-8)
    var ratio = 0.5         // float (64-bit)
    var ok = true
    var c = 'é'             // char: a Unicode code point
    var count int           // no initializer: the zero value
    var xs []int = []       // the type is needed when the value does not say it
    xs = append(xs, n, 7)
    var ages = map[string]int{"ann": 31}
    ages["bob"] = 27

    println(n, name, ratio, ok, c, count, xs, len(ages))
    println(fmt.Format("{0} has {1} letters, pi is {2}", name, len(name), Pi))
    var (q, r) = divmod(17, 5)
    println(q, r, xs[1..], name[0..2])
}

func divmod(a int, b int) (int, int) {
    return (a / b, a % b)
}
// Output:
// 42 zell 0.5 true é 0 [42 7] 2
// zell has 4 letters, pi is 3.14159
// 3 2 [7] ze
```

Only `var` declares variables; the type comes after the name when you
write it. Functions return several values as a tuple. Slices (`[]T`) and
maps work like Go's; `s[a..b]` is a sub-slice or substring. `fmt.Format`
fills `{N}` with argument N; `{N,8}` right-aligns it to 8 characters and
`{N,-8}` left-aligns it, as in C#.

`m[k]` gives the value, or the zero value when the key is absent. To tell
an absent key from a stored zero, ask for the second result (Go's
comma-ok): `var v, ok = m[k]`, also written `var (v, ok) = m[k]`, or
`v, ok = m[k]` for existing variables. `ok` is a `bool`; when it is false,
`v` is the zero value. There is no `in` operator for maps.

```zell
namespace tour.commaok

import io

func main() {
    var stock = map[string]int{"apple": 3, "pear": 0}

    var n, ok = stock["pear"]
    println(n, ok)                  // stored zero: ok is true

    var (m, found) = stock["plum"]
    println(m, found)               // absent: zero value, ok is false

    n, ok = stock["apple"]          // assign to existing variables
    if ok {
        println("apples:", n)
    }
}
// Output:
// 0 true
// 0 false
// apples: 3
```

Control flow is Go's, without parentheses:

```zell
namespace tour.control

import io

func main() {
    for i in 0..3 {
        if i == 1 {
            continue
        }
        println("i =", i)
    }
    var words = ["zero", "one", "two"]
    for i, w in words {
        switch {
            case i == 0: println(w, "is first")
            case len(w) == 3: println(w, "has three letters")
        }
    }
    var n = 10
    for n > 1 {
        n /= 3
    }
    println("n =", n)
}
// Output:
// i = 0
// i = 2
// zero is first
// one has three letters
// two has three letters
// n = 1
```

`in` is a keyword only between the loop variables and the collection in
`for … in …`; anywhere else it is an ordinary name, so `var in = 1` and a
parameter called `in` work.

## Structs and methods

Methods live inside the struct; the receiver is `this` (a pointer, so
methods can change the value). Exported names start with an upper-case
letter.

```zell
namespace tour.structs

import io

struct Point {
    X float
    Y float

    func Length() float {
        return math.Sqrt(this.X * this.X + this.Y * this.Y)
    }

    func Move(dx float, dy float) {
        this.X += dx
        this.Y += dy
    }

    static func Origin() Point {
        return Point{}
    }
}

func main() {
    var p = Point{X: 3, Y: 4}
    println(p.Length())
    p.Move(1, 1)
    println(p.X, p.Y)

    var q = p           // structs are values: q is a copy
    q.X = 100
    println(p.X, q.X)

    var ptr = new Point{X: 1}   // *Point on the heap
    ptr.Move(1, 0)
    println(ptr.X, Point.Origin().Length())
}
// Output:
// 5
// 4 5
// 4 100
// 2 0
```

A line that starts with `.` continues the previous one, so a method chain
can be broken before each dot. Methods that return `this` make a builder:

```zell
namespace tour.chain

import io

struct Query {
    table string
    conds []string
    max   int

    func Where(c string) *Query {
        this.conds = append(this.conds, c)
        return this
    }

    func Limit(n int) *Query {
        this.max = n
        return this
    }

    func Text() string {
        return "select * from " + this.table + " where " + strings.Join(this.conds, " and ") + " limit " + strconv.Itoa(this.max)
    }
}

func main() {
    var q = new Query{table: "users"}
    println(q
        .Where("age > 18")
        .Where("active")
        .Limit(10)
        .Text())
}
// Output:
// select * from users where age > 18 and active limit 10
```

`zell fmt` keeps one `.name` per line in such a chain. Inside `( )` and
`[ ]` no semicolons are inserted either, so argument lists can wrap freely.

## Type aliases

`type Name = Type` gives another name to an existing type. The alias is
transparent: `Name` and `Type` are the same type and can be used for one
another everywhere (assignments, signatures, literals, methods). It works
for structs, slices, maps and function types; generic aliases and aliases
inside functions are not supported yet.

```zell
namespace tour.alias

import io

struct Point {
    X int
    Y int

    func Sum() int {
        return this.X + this.Y
    }
}

type P = Point
type Op = func(int, int) int
type Ints = []int

func apply(f Op, a int, b int) int {
    return f(a, b)
}

func main() {
    var p = P{X: 1, Y: 2}      // a literal through the alias
    var q Point = p            // the same type: no conversion
    println(q.Sum(), p.Sum())

    var add Op = (a int, b int) => a + b
    println(apply(add, 3, 4))

    var xs Ints = [1, 2, 3]
    var ys []int = xs
    println(len(ys))
}
// Output:
// 3 3
// 7
// 3
```

Attributes (`@json(name: "id")`, `@deprecated("use Save")`) attach
metadata to declarations. An attribute is a struct marked `@attribute`,
so the usual visibility rule applies: to use one from another namespace,
give its type a capital letter. Keywords are allowed as attribute argument
and field names, for example `@Conf(enum: "a,b")`.

## Interfaces

Interfaces are structural, as in Go: a type implements an interface by
having its methods. A `switch` on an interface value can test the
dynamic type.

```zell
namespace tour.interfaces

import io

interface Shape {
    func Area() float
    func Name() string
}

struct Rect {
    W float
    H float

    func Area() float {
        return this.W * this.H
    }

    func Name() string {
        return "rect"
    }
}

struct Circle {
    R float

    func Area() float {
        return 3.0 * this.R * this.R
    }

    func Name() string {
        return "circle"
    }
}

func describe(s Shape) {
    switch s {
        case Circle c: println(s.Name(), "with radius", c.R, "has area", s.Area())
        default: println(s.Name(), "has area", s.Area())
    }
}

func main() {
    var shapes []Shape = [Rect{W: 2, H: 3}, Circle{R: 1}]
    for s in shapes {
        describe(s)
    }
}
// Output:
// rect has area 6
// circle with radius 1 has area 3
```

## Enums

Plain enums and enums whose variants carry data; `switch` destructures
them, and a `switch` that covers every variant needs no `default`.

```zell
namespace tour.enums

import io

enum Color { Red, Green, Blue }

enum Shape {
    Circle(r float)
    Rect(w float, h float)
    Empty

    func Area() float {
        switch this {
            case Shape.Circle(r): return 3.0 * r * r
            case Shape.Rect(w, h): return w * h
            case Shape.Empty: return 0
        }
    }
}

func main() {
    var c = Color.Green
    println(c, c == Color.Green, int(Color.Blue))
    var shapes = [Shape.Circle(2), Shape.Rect(2, 5), Shape.Empty]
    for s in shapes {
        println(s.Area())
    }
}
// Output:
// Green true 2
// 12
// 10
// 0
```

## Generics

Functions, structs, enums and methods can take type parameters,
optionally constrained (`Ordered`, `Comparable`, an interface). They are
compiled per type (monomorphized), so they cost nothing at run time.

```zell
namespace tour.generics

import io

func Max<T: Ordered>(a T, b T) T {
    if a > b {
        return a
    }
    return b
}

struct Stack<T> {
    items []T

    func Push(x T) {
        this.items = append(this.items, x)
    }

    func Pop() T! {
        if len(this.items) == 0 {
            return error("empty stack")
        }
        var x = this.items[len(this.items) - 1]
        this.items = this.items[..len(this.items) - 1]
        return x
    }
}

enum Option<T> {
    Some(value T)
    None
}

func main() {
    println(Max(3, 7), Max("pear", "apple"), Max<float>(1, 2.5))
    var s = Stack<string>{}
    s.Push("a")
    s.Push("b")
    println(try s.Pop() else "?", try s.Pop() else "?", try s.Pop() else "none")
    var o = Option<int>.Some(5)
    switch o {
        case Option.Some(v): println("some", v)
        case Option.None: println("none")
    }
}
// Output:
// 7 pear 2.5
// b a none
// some 5
```

## Errors: `T!`, `?` and `try`

A function that can fail returns `T!` (`void!` when it has no value).
`error("...")` makes an error. `?` returns the error to the caller (and
`?("context")` adds a message); `try ... catch` and `try ... else` handle
it on the spot. A `T!` result cannot be ignored silently. Errors carry a
trace of where they were created and returned. See [errors.md](errors.md).

```zell
namespace tour.errors

import io

func parsePort(s string) int! {
    var n = strings.ToInt(s)?("port " + strconv.Quote(s))
    if n < 1 || n > 65535 {
        return error(fmt.Format("port {0} out of range", n))
    }
    return n
}

func main() {
    println(try parsePort("8080") else 0)
    var p = try parsePort("http") catch err {
        println("error:", err)
        return
    }
    println("unreachable", p)
}
// Output:
// 8080
// error: port "http": invalid integer: "http"
```

## Async and await

`async func` returns a `Task<T>`; calling it starts the task, `await`
waits for its result. Tasks run on a pool of worker threads; waiting
never blocks a thread. `task.All` waits for several tasks at once.

```zell
namespace tour.async

import io

async func fetch(name string, ms int) string {
    await task.Delay(ms)
    return name + " done"
}

async func main() {
    var a = fetch("a", 20)      // both start now
    var b = fetch("b", 10)
    println(await b)
    println(await a)
    var (x, y) = await task.All(fetch("x", 5), fetch("y", 1))
    println(x, y)
}
// Output:
// b done
// a done
// x done y done
```

## Tasks, channels and cancellation

`task.Channel<T>(n)` passes values between tasks (`n` is the buffer).
Cancellation travels in the task context: `task.WithTimeout` cancels the
code it runs (and every task started inside it) when the time is up, and
cancelled waits fail with `task.ErrCancelled`. See [async.md](async.md).

```zell
namespace tour.tasks

import io

async func produce(ch task.Channel<int>) {
    for i in 1..4 {
        await ch.Send(i * 10)
    }
    ch.Close()
}

async func main() {
    var ch = task.Channel<int>(0)
    var p = produce(ch)
    for v in ch {
        println("received", v)
    }
    await p

    try await task.WithTimeout(10, async func() string! {
        await task.Delay(5000)
        task.CheckCancelled()?
        return "too slow"
    }) catch err {
        println("cancelled:", errors.Is(err, task.ErrCancelled))
    }
}
// Output:
// received 10
// received 20
// received 30
// cancelled: true
```

## Collections

Slices and maps are built in; `slices` and `maps` have the usual
algorithms, and `collections` adds ordered maps, sets, queues, heaps,
deques and LRU caches.

```zell
namespace tour.collections

import io

struct User {
    Name string
    Age  int
}

func main() {
    var users = [User{Name: "cid", Age: 45}, User{Name: "ann", Age: 31}, User{Name: "bob", Age: 27}]
    slices.SortBy(users, u => u.Age)
    var names = slices.Map(users, u => u.Name)
    println(names, slices.Contains(names, "ann"))

    var byFirst = map[string]int{}
    for u in users {
        byFirst[u.Name[0..1]] += u.Age
    }
    println(maps.SortedKeys(byFirst))

    var seen = collections.Set<string>.New()
    for w in strings.Fields("to be or not to be") {
        seen.Add(w)
    }
    println(seen.Items(), seen.Len())
}
// Output:
// [bob ann cid] true
// [a b c]
// [to be or not] 4
```

## Reflection and attributes

Attributes are typed values, and `reflect` reads them from static tables
the compiler emits only when a program uses them.
`reflect.TypesWith<A>()` lists every type of the whole program (your
package and all its dependencies) declared with attribute `A` — so a
library can find the application's handlers or jobs without any
registration code. `m.Call<F>()` turns a method into an ordinary typed
function, and `reflect.Error(msg)` is a compile-time error a library can
raise from a generic function (a `static_assert`).

```zell
namespace tour.reflection

import io

@attribute(targets: [reflect.Target.Struct])
struct job {
    name string
}

@job(name: "backup")
struct Backup {
    static func Run() string {
        return "done"
    }
}

@job(name: "mail")
struct Mail {
    static func Run() string {
        return "sent"
    }
}

func Describe<T>() string {
    if reflect.TypeOf<T>().Kind != reflect.Kind.Struct {
        reflect.Error("Describe needs a struct, not " + reflect.TypeOf<T>().Name)
    }
    return reflect.TypeOf<T>().Name + " has " + fmt.Format("{0}", len(reflect.TypeOf<T>().Fields)) + " fields"
}

func main() {
    for t in reflect.TypesWith<job>() {
        var run = t.Method("Run").Call<func() string>()
        println(t.Attribute<job>().name, run())
    }
    println(Describe<Backup>())
}
// Output:
// backup done
// mail sent
// Backup has 0 fields
```

## Packages

A project is a directory with a manifest (`zell.toml`) and sources in
`src/`; namespaces are just paths, and any file can import any namespace
of the project. `zell init` creates one:

```sh
zell init hello && cd hello
zell run                  # builds src/main.zl and runs it
zell install              # after adding a dependency to zell.toml (packages.md)
```

The pieces you have seen compose: here is a small program that reads a
file with fallible steps, in the style of a real tool:

```zell
namespace tour.packages

import io

struct Config {
    Name string
    Port int
}

func load(path string) Config! {
    var text = io.ReadFile(path)?("reading config")
    var cfg = Config{Port: 8080}
    for line in strings.Split(text, "\n") {
        var kv = strings.SplitN(line, "=", 2)
        if len(kv) != 2 {
            continue
        }
        switch strings.Trim(kv[0]) {
            case "name": cfg.Name = strings.Trim(kv[1])
            case "port": cfg.Port = strings.ToInt(strings.Trim(kv[1]))?("port")
        }
    }
    return cfg
}

func main() void! {
    io.WriteFile("app.conf", "name = demo\nport = 9000\n")?
    var cfg = load("app.conf")?
    println(cfg.Name, cfg.Port)
    try load("missing.conf") catch err {
        println(err)
    }
    fs.Remove("app.conf")?
}
// Output:
// demo 9000
// reading config: open missing.conf: No such file or directory
```

## Where next

- [errors.md](errors.md): error values, wrapping, typed errors, traces.
- [async.md](async.md): tasks, channels, mutexes, cancellation, timeouts.
- [memory.md](memory.md): how memory works without a garbage collector.
- [packages.md](packages.md): projects, dependencies, workspaces.
- [testing.md](testing.md): `zell test`, benchmarks, examples.
- [debugging.md](debugging.md): traces, the debugger, leak checks, profiles.
- [faq.md](faq.md): how Zell differs from Go and C#.
- `zell doc strings.Split`, `zell doc std --html out/`: the API reference.
