# todo-cli: a command-line tool with subcommands

A to-do list kept in a JSON file, driven by subcommands: `todo add`,
`todo list`, `todo done`, `todo rm`. It is built on the standard `cli`
package: flags are declared as struct fields with attributes, and `cli`
generates the parser, the help text, "did you mean" suggestions and the
exit codes from them.

What it shows:

- `cli.App`, `cli.Command`, flags on structs with `@cli.flag(...)`,
  global flags, repeatable flags (`-t home -t errands`), aliases (`ls`);
- `cli.Bind<T>(c)` filling a flags struct, with defaults from the field
  values and an environment fallback (`TODO_FILE`);
- persistence with `encoding.json`: `json.Parse<T>` and `json.Format`
  generated at compile time from `@json` attributes, and error messages
  that carry the JSON path of a bad value;
- a safe save: write a temporary file, then rename it over the old one;
- custom error types: a sentinel (`ErrNotFound`) found with `errors.Is`
  through a typed error that links to it with `Unwrap`, and `ExitCode()`
  methods that choose the process exit code;
- dependency injection without a framework: an `Env` struct with the
  output writer and the clock;
- tests that drive the whole `App` in-process.

## Run it

```sh
cd docs/examples/todo-cli
zell run          # the demo session below
zell test
```

The demo plays a fixed shell session against a file in the temporary
directory, with a fixed date, so its output is reproducible. The real
program is the two lines in the comment in `main`:

```sh
zell build --release
./target/release/todo-cli add -p high fix the bike -t home
./target/release/todo-cli ls
TODO_FILE=~/work.json ./target/release/todo-cli list --all
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `todo`) |
| `src/main.zl` | the demo session |
| `src/commands.zl` | `todo.commands`: the flag structs, the four commands, `App` |
| `src/store.zl` | `todo.store`: loading and saving the JSON file |
| `src/model.zl` | `todo.model`: `Item`, `Priority`, the error types |
| `src/store_test.zl` | tests (`zell test`) |

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "todo-cli"
version = "0.1.0"
namespace = "todo"
entry = "src/main.zl"
```

## src/model.zl: data and errors

`Item` and `File` are plain structs; the `@json` attributes choose the
key names and leave empty tag lists out of the file. They are typed
values, checked by the compiler: `@json(nmae: "id")` does not compile,
unlike a misspelled struct tag in Go. Enums such as `Priority` are
written as their variant names (`"High"`).

The errors are where this file earns its keep:

- `ErrNotFound` is a sentinel: a constant error value that callers compare
  with `errors.Is`.
- `NotFoundError` carries the missing id. Its `Unwrap` returns
  `ErrNotFound`, so `errors.Is(err, ErrNotFound)` is true for it, and for
  any wrapper around it (`?("context")` keeps the chain).
- Both `NotFoundError` and `UsageError` have `ExitCode() int`. That is the
  `cli.ExitCoder` interface: when a command fails, `App.Run` returns the
  exit code of the first error in the chain that has one, so a script can
  tell "no such item" (3) from a usage mistake (2) and any other failure
  (1). See [Typed errors](../../guide/errors.md#typed-errors).

<!-- file: src/model.zl -->
```zell title="src/model.zl"
// The data model: what one to-do item is, how it looks in JSON, and the
// errors the program can report.
namespace todo.model

enum Priority { Low, Normal, High }

// ParsePriority turns a flag value into a Priority. Enums print and parse
// by their variant names in JSON already; the command line is lower-case,
// so it gets its own small parser.
func ParsePriority(s string) Priority! {
    switch strings.ToLower(s) {
        case "low": return Priority.Low
        case "normal", "": return Priority.Normal
        case "high": return Priority.High
    }
    return UsageError{Text: "unknown priority " + strconv.Quote(s) + " (low, normal, high)"}
}

// Item is one to-do. The @json attributes decide the file format: short
// lower-case keys, empty tag lists left out. encoding.json reads them at
// compile time, so a typo in an attribute is a compile error, not a
// silently ignored struct tag.
struct Item {
    @json(name: "id")                    Id       int
    @json(name: "title")                 Title    string
    @json(name: "done")                  Done     bool
    @json(name: "priority")              Priority Priority
    @json(name: "tags", omitEmpty: true) Tags     []string
    @json(name: "created")               Created  string
}

// The file is an object rather than a bare array so that fields can be
// added later (NextId today) without breaking old files.
struct File {
    @json(name: "next_id") NextId int
    @json(name: "items")   Items  []Item
}

// ErrNotFound is the sentinel callers test with errors.Is, whatever
// concrete error carries it.
const ErrNotFound = error("no such item")

// NotFoundError says which id was missing. It links to ErrNotFound through
// Unwrap, so errors.Is(err, ErrNotFound) is true for it and for anything
// that wraps it with ?("context").
struct NotFoundError {
    Id int

    func Message() string {
        return fmt.Format("item {0} not found", this.Id)
    }

    func Unwrap() error {
        return ErrNotFound
    }

    // cli.App.Run uses ExitCode() of the first error in the chain that
    // has one (the cli.ExitCoder interface): scripts can tell "not found"
    // (3) from other failures (1) and usage errors (2).
    func ExitCode() int {
        return 3
    }
}

// UsageError is a mistake in the arguments: exit code 2, like the
// errors cli reports itself (unknown flag, missing value).
struct UsageError {
    Text string

    func Message() string {
        return this.Text
    }

    func ExitCode() int {
        return 2
    }
}
```

## src/store.zl: the JSON file

`Store.Open` treats a missing file as an empty list, so the first
`todo add` just works, and wraps a parse error with the file name.
`json.Parse<File>` is generated for `File` at compile time, so it is as
fast as hand-written code, and its errors say where the problem is:
`items[0].done: expected bool, found string`.

`Save` writes only when a command changed something, and never overwrites
the file in place: it writes `todo.json.tmp` and renames it over
`todo.json`. A rename within a directory is atomic, so a crash or a full
disk leaves the old list intact.

The clock (`today`) is a field holding a function: the program passes one
that formats the current date, the demo and the tests pass a fixed one.

<!-- file: src/store.zl -->
```zell title="src/store.zl"
// Persistence: the list of items lives in one JSON file that is read at
// the start of every command and written back when the command changed it.
namespace todo.store

import encoding

import todo.model

struct Store {
    path  string
    data  File
    dirty bool
    // The clock is a field so tests and the demo get fixed dates; the
    // real program passes a function that formats time.Time.Now().
    today func() string

    // Open reads the file, or starts empty when it does not exist yet:
    // the first `todo add` must not fail because there is no file.
    static func Open(path string, today func() string) *Store! {
        var s = new Store{path: path, data: File{NextId: 1}, today: today}
        if !fs.Exists(path) {
            return s
        }
        var text = io.ReadFile(path)?
        // Parse<File> is generated at compile time from the attributes on
        // File and Item; a malformed file reports the JSON path of the bad
        // value, e.g. `items[2].done: expected a boolean`.
        s.data = json.Parse<File>(text)?("parse " + fs.Base(path))
        return s
    }

    // Save writes only if something changed. It writes a temporary file
    // and renames it over the old one: a crash halfway through leaves the
    // previous list intact instead of half a JSON document.
    func Save() void! {
        if !this.dirty {
            return
        }
        var tmp = this.path + ".tmp"
        io.WriteFile(tmp, json.Format(this.data) + "\n")?("save")
        fs.Rename(tmp, this.path)?("save")
        this.dirty = false
    }

    func Add(title string, p Priority, tags []string) Item! {
        var t = strings.Trim(title)
        if t == "" {
            return UsageError{Text: "the title is empty"}
        }
        var it = Item{
            Id: this.data.NextId,
            Title: t,
            Priority: p,
            Tags: tags,
            Created: this.today(),
        }
        this.data.NextId += 1
        this.data.Items = append(this.data.Items, it)
        this.dirty = true
        return it
    }

    // Items returns a sorted copy, so callers can filter or reorder it
    // without touching what will be saved.
    func Items() []Item {
        var xs = slices.Clone(this.data.Items)
        slices.SortFunc(xs, displayOrder)
        return xs
    }

    func index(id int) int! {
        for i, it in this.data.Items {
            if it.Id == id {
                return i
            }
        }
        return NotFoundError{Id: id}
    }

    func Complete(id int) Item! {
        var i = this.index(id)?
        this.data.Items[i].Done = true
        this.dirty = true
        return this.data.Items[i]
    }

    func Remove(id int) Item! {
        var i = this.index(id)?
        var it = this.data.Items[i]
        this.data.Items = slices.Delete(this.data.Items, i, i + 1)
        this.dirty = true
        return it
    }
}

// displayOrder: open items before done ones, then high priority first,
// then oldest (smallest id) first.
func displayOrder(a Item, b Item) bool {
    if a.Done != b.Done {
        return !a.Done
    }
    if a.Priority != b.Priority {
        return int(a.Priority) > int(b.Priority)
    }
    return a.Id < b.Id
}
```

## src/commands.zl: the commands

Each command declares its flags as a struct. `cli.FlagsOf<AddFlags>()`
reads the struct at compile time: the field name gives the flag name
(`Priority` → `--priority`), the attribute gives the short name and the
help text, the field's default value is the flag's default, and a
`[]string` field makes a repeatable flag. Inside `Run`,
`cli.Bind<AddFlags>(c)` returns the filled struct. The global flags
(`--file`) are bound the same way, from any command.

`withStore` is the shape every command shares: open the store, do the
work, save. Because the save happens only after `f` succeeded, a command
that fails halfway never writes a half-changed list.

`rm` shows the reason sentinels exist: removing an id that is already
gone is fine for `rm`, so it catches the error, asks
`errors.Is(err, ErrNotFound)`, and returns any other error unchanged.

<!-- file: src/commands.zl -->
```zell title="src/commands.zl"
// The subcommands. Each one is a cli.Command whose flags are declared as
// a struct: cli.FlagsOf<T>() reads the @cli.flag attributes at compile time
// to build the help text and the parser, and cli.Bind<T>(c) fills a T.
namespace todo.commands

import todo.model
import todo.store

// Global flags, accepted before or after the subcommand name.
struct GlobalFlags {
    @cli.flag(short: "f", env: "TODO_FILE", help: "the to-do file") File string = "todo.json"
}

struct AddFlags {
    @cli.flag(short: "p", help: "low, normal or high") Priority string = "normal"
    // A []string flag may be repeated: -t home -t errands.
    @cli.flag(short: "t", help: "a tag (repeatable)") Tag []string
}

struct ListFlags {
    @cli.flag(short: "a", help: "include finished items")   All bool
    @cli.flag(short: "t", help: "only items with this tag") Tag string
}

// Env bundles what every command needs. The commands receive it instead
// of reaching for globals, which is what lets main run them against a
// temporary file and a fixed clock.
struct Env {
    Out   io.Writer
    Today func() string
}

// withStore opens the store named by --file, runs f, and saves when f
// succeeded. Every command goes through it, so none of them can forget to
// save, and a failed command never writes a half-changed list.
func withStore(env *Env, c *cli.Context, f func(*Store) void!) void! {
    var g = cli.Bind<GlobalFlags>(c)?
    var s = Store.Open(g.File, env.Today)?
    f(s)?
    s.Save()?
}

// ids parses the positional arguments as item ids. A bad id is a
// UsageError, so it exits with 2 like any other mistake on the command line.
func ids(c *cli.Context) []int! {
    if len(c.Args) == 0 {
        return UsageError{Text: "expected at least one item id"}
    }
    var out []int = []
    for a in c.Args {
        var n = try strconv.Atoi(a) catch err {
            return UsageError{Text: "not an item id: " + strconv.Quote(a)}
        }
        out = append(out, n)
    }
    return out
}

func line(it Item) string {
    var mark = "[ ]"
    if it.Done {
        mark = "[x]"
    }
    var prio = strings.ToLower(fmt.Format("{0}", it.Priority))
    var s = mark +
        " " +
        strings.PadLeft(strconv.Itoa(it.Id), 3, " ") +
        "  " +
        strings.PadRight(prio, 6, " ") +
        "  " +
        it.Title
    if len(it.Tags) > 0 {
        s += "  #" + strings.Join(it.Tags, " #")
    }
    return s
}

func Commands(env *Env) []cli.Command {
    var add = cli.Command{
        Name: "add",
        Help: "add an item",
        Args: "<title...>",
        Flags: cli.FlagsOf<AddFlags>(),
        Run: func(c *cli.Context) void! {
            var f = cli.Bind<AddFlags>(c)?
            var p = ParsePriority(f.Priority)?
            withStore(env, c, func(s *Store) void! {
                var it = s.Add(strings.Join(c.Args, " "), p, f.Tag)?
                io.WriteString(env.Out, fmt.Format("added {0}: {1}\n", it.Id, it.Title))?
            })?
        },
    }
    var list = cli.Command{
        Name: "list",
        Aliases: ["ls"],
        Help: "show the items",
        Flags: cli.FlagsOf<ListFlags>(),
        Run: func(c *cli.Context) void! {
            var f = cli.Bind<ListFlags>(c)?
            withStore(env, c, func(s *Store) void! {
                var shown = 0
                for it in s.Items() {
                    if (it.Done && !f.All) || (f.Tag != "" && !slices.Contains(it.Tags, f.Tag)) {
                        continue
                    }
                    io.WriteString(env.Out, line(it) + "\n")?
                    shown += 1
                }
                if shown == 0 {
                    io.WriteString(env.Out, "nothing to do\n")?
                }
            })?
        },
    }
    var done = cli.Command{
        Name: "done",
        Help: "mark items as finished",
        Args: "<id...>",
        Run: func(c *cli.Context) void! {
            var xs = ids(c)?
            withStore(env, c, func(s *Store) void! {
                for id in xs {
                    var it = s.Complete(id)?
                    io.WriteString(env.Out, fmt.Format("done {0}: {1}\n", it.Id, it.Title))?
                }
            })?
        },
    }
    var rm = cli.Command{
        Name: "rm",
        Help: "delete items",
        Args: "<id...>",
        Run: func(c *cli.Context) void! {
            var xs = ids(c)?
            withStore(
                env,
                c,
                func(s *Store) void! {
                    for id in xs {
                        // Removing an id that is already gone is not an error
                        // for `rm`: the goal (the item is not there) holds.
                        // errors.Is sees ErrNotFound through NotFoundError.
                        var it = try s.Remove(id) catch err {
                            if errors.Is(err, ErrNotFound) {
                                io.WriteString(env.Out, fmt.Format("{0} was already gone\n", id))?
                                continue
                            }
                            return err
                        }
                        io.WriteString(env.Out, fmt.Format("removed {0}: {1}\n", it.Id, it.Title))?
                    }
                },
            )?
        },
    }
    return [add, list, done, rm]
}

func App(env *Env) cli.App {
    return cli.App{
        Name: "todo",
        Version: "1.0.0",
        Help: "a to-do list in a JSON file",
        Flags: cli.FlagsOf<GlobalFlags>(),
        Commands: Commands(env),
    }
}
```

## src/main.zl: the session

`App.Run` never exits the process; it returns the exit code. That is what
lets `main` run a dozen command lines in one process, and lets the tests
check exit codes. `App.Errors` is where `cli` writes error messages
(normally stderr); the demo collects them so they appear in order with
the rest of the output.

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// todo: a to-do list kept in a JSON file.
//
//     todo [--file todo.json] add|list|done|rm ...
namespace todo

import io

import todo.commands

// session runs one command line the way a shell would and prints its exit
// code. cli.App.Run never exits the process itself: it returns the code,
// so the same App can be driven from tests and from this demo.
func session(file string, args []string) {
    println("$ todo", strings.Join(args, " "))
    var errs = io.NewBytesWriter()
    var env = new Env{Out: os.Stdout(), Today: () => "2026-10-24"}
    var app = commands.App(env)
    app.Errors = errs
    var all = ["todo", "--file", file]
    for a in args {
        all = append(all, a)
    }
    var code = app.Run(all)
    print(errs.String())
    if code != 0 {
        println("[exit", code, "]")
    }
}

func main() void! {
    // A real tool is two lines:
    //     var env = new Env{Out: os.Stdout(), Today: () => time.Time.Now().Format("2006-01-02")}
    //     os.Exit(commands.App(env).Run(os.Args()))
    // The demo runs a fixed script against a file in a temporary
    // directory, with a fixed date, so its output is reproducible.
    var dir = fs.Join(fs.TempDir(), "todo-cli-demo")
    fs.MkdirAll(dir)?
    var file = fs.Join(dir, "todo.json")
    _ = fs.Remove(file)

    session(file, ["list"])
    session(file, ["add", "buy", "milk", "-t", "errands"])
    session(file, ["add", "-p", "high", "fix", "the", "bike", "-t", "home", "-t", "errands"])
    session(file, ["add", "--priority=low", "read", "a", "book"])
    session(file, ["ls"])
    session(file, ["done", "2"])
    session(file, ["list", "--all"])
    session(file, ["list", "-t", "errands"])
    session(file, ["rm", "3", "3"])
    session(file, ["done", "7"])
    session(file, ["done", "x"])
    session(file, ["add", "-p", "urgent", "panic"])
    session(file, ["add", "--prio", "high", "typo"])
    session(file, ["add", "--help"])

    // The file the commands wrote, as another program would see it.
    println("--- todo.json")
    print(io.ReadFile(file)?)

    // A damaged file is reported with the JSON path of the bad value, and
    // nothing is overwritten: Save only runs after a command succeeded.
    io.WriteFile(file, "{\"next_id\": 4, \"items\": [{\"id\": 1, \"done\": \"yes\"}]}")?
    session(file, ["list"])
    fs.RemoveAll(dir)?
}
```

## src/store_test.zl: tests

The tests use files in the temporary directory and a fixed clock. The last
one drives the whole `App` with argument lists and checks the exit codes,
which is the contract scripts rely on.

<!-- file: src/store_test.zl -->
```zell title="src/store_test.zl"
// Tests for the store and the commands. They use a file in the temporary
// directory, so they never touch a real to-do list.
namespace todo.tests

import todo.commands
import todo.model
import todo.store

func fixedDay() string {
    return "2026-01-02"
}

func tempFile(name string) string {
    var p = fs.Join(fs.TempDir(), "todo-test-" + name + ".json")
    _ = fs.Remove(p)
    return p
}

@test func TestSaveAndReopen(t *testing.T) {
    var path = tempFile("reopen")
    var s = try Store.Open(path, fixedDay) catch err {
        t.Fatal(err.Message())
        return
    }
    _ = s.Add("one", Priority.Low, [])
    _ = s.Add("two", Priority.High, ["x"])
    try s.Save() catch err {
        t.Fatal(err.Message())
    }
    var again = try Store.Open(path, fixedDay) catch err {
        t.Fatal(err.Message())
        return
    }
    var items = again.Items()
    testing.Equal(t, len(items), 2)
    // high priority sorts first
    testing.Equal(t, items[0].Title, "two")
    testing.Equal(t, items[0].Created, "2026-01-02")
    _ = fs.Remove(path)
}

@test func TestNotFoundIsDetectable(t *testing.T) {
    var s = try Store.Open(tempFile("missing"), fixedDay) catch err {
        t.Fatal(err.Message())
        return
    }
    try s.Complete(42) catch err {
        // the sentinel is found through the typed error and a wrapper
        testing.True(t, errors.Is(err, ErrNotFound), "Is on the typed error")
        testing.True(t, errors.Is(errors.Wrap(err, "done"), ErrNotFound), "Is through a wrapper")
        var nf NotFoundError
        testing.True(t, errors.As(err, &nf), "As finds NotFoundError")
        testing.Equal(t, nf.Id, 42)
        return
    }
    t.Fatal("Complete(42) succeeded on an empty list")
}

@test func TestExitCodes(t *testing.T) {
    var path = tempFile("codes")
    var out = io.NewBytesWriter()
    var app = App(new Env{Out: out, Today: fixedDay})
    app.Errors = io.NewBytesWriter()
    testing.Equal(t, app.Run(["todo", "-f", path, "add", "x"]), 0)
    testing.Equal(t, app.Run(["todo", "-f", path, "done", "9"]), 3)
    testing.Equal(t, app.Run(["todo", "-f", path, "done", "nine"]), 2)
    testing.Equal(t, app.Run(["todo", "-f", path, "add"]), 2)
    testing.Equal(t, out.String(), "added 1: x\n")
    _ = fs.Remove(path)
}
```

## Output

<!-- file: expected.out -->
```text title="expected.out"
$ todo list
nothing to do
$ todo add buy milk -t errands
added 1: buy milk
$ todo add -p high fix the bike -t home -t errands
added 2: fix the bike
$ todo add --priority=low read a book
added 3: read a book
$ todo ls
[ ]   2  high    fix the bike  #home #errands
[ ]   1  normal  buy milk  #errands
[ ]   3  low     read a book
$ todo done 2
done 2: fix the bike
$ todo list --all
[ ]   1  normal  buy milk  #errands
[ ]   3  low     read a book
[x]   2  high    fix the bike  #home #errands
$ todo list -t errands
[ ]   1  normal  buy milk  #errands
$ todo rm 3 3
removed 3: read a book
3 was already gone
$ todo done 7
error: item 7 not found
[exit 3 ]
$ todo done x
error: not an item id: "x"
[exit 2 ]
$ todo add -p urgent panic
error: unknown priority "urgent" (low, normal, high)
[exit 2 ]
$ todo add --prio high typo
error: unknown flag --prio (did you mean "--priority"?)
Run 'todo add --help' for usage.
[exit 2 ]
$ todo add --help
add an item

Usage:
  todo add [flags] <title...>

Flags:
  -f, --file string      the to-do file (default todo.json, env $TODO_FILE)
  -p, --priority string  low, normal or high (default normal)
  -t, --tag strings      a tag (repeatable)
  -h, --help             Show help
--- todo.json
{"next_id":4,"items":[{"id":1,"title":"buy milk","done":false,"priority":"Normal","tags":["errands"],"created":"2026-10-24"},{"id":2,"title":"fix the bike","done":true,"priority":"High","tags":["home","errands"],"created":"2026-10-24"}]}
$ todo list
error: parse todo.json: items[0].done: expected bool, found string
[exit 1 ]
```

Things to notice: `ls` is an alias of `list`; items are sorted open
first, then by priority; `rm 3 3` removes once and reports the second as
already gone; each failure has its own exit code; `--prio` gets a "did
you mean"; `add --help` is generated from `AddFlags` and `GlobalFlags`.
The damaged file at the end is reported with the path of the bad value
and is not overwritten.

## Next

- [bookshelf-api](../bookshelf-api/README.md): the same ideas (typed JSON,
  errors mapped to codes) in an HTTP service.
- [validate](../validate/README.md): how attributes like `@cli.flag` and
  `@json` work, by writing your own.
