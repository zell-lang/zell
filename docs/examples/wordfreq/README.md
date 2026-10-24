# wordfreq: counting words in files

A command-line tool that reads text files, counts how often each word
occurs, prints the most frequent words as an aligned table and saves the
same ranking as a Markdown report. It is the kind of small program you
write in an afternoon, and it touches most of what everyday programs need:
files read line by line, a map of counts, sorting, formatted output,
writing a file and reading it back, and errors that travel up with `?` to
one place that turns them into a message and an exit code.

What it shows:

- `os.Open` + `io.NewScanner` to stream a file line by line, and
  `io.ReadLines` for small files read whole;
- why there is no `defer f.Close()`: files close themselves (`Drop`);
- `map[string]int`, `slices.SortFunc` with a deterministic tie-break;
- `text.table` for aligned and Markdown tables, `fmt.Sprintf` for numbers;
- `T!`, `?` and `?("context")` inside the program, `try ... catch` only at
  the boundary (`run`), exit codes 0, 1 and 2;
- `io.Writer` parameters instead of hard-wired stdout, so the logic is
  testable;
- unit tests in `src/counter_test.zl`.

## Run it

```sh
cd docs/examples/wordfreq
zell run          # the demo: four fixed command lines, output below
zell test         # the unit tests
```

The demo in `main` runs the tool on four fixed argument lists, so the
output is the same on every machine (it is checked byte for byte by the
repository's tests). A real build reads `os.Args()` instead; that one line
is shown in a comment in `main`. With it, you would run:

```sh
zell build --release
./target/release/wordfreq --top 8 --stop data/stopwords.txt --out report.md data/*.txt
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest: package name, root namespace `wordfreq`, entry file |
| `src/main.zl` | argument parsing, the error boundary, the demo |
| `src/counter.zl` | `wordfreq.counter`: tokenizing, reading files, ranking |
| `src/report.zl` | `wordfreq.report`: the terminal table, the Markdown report |
| `src/counter_test.zl` | tests for the counter (`zell test`) |
| `data/*.txt` | the input texts and the stop-word list |

## The manifest

`entry` names the file with `main`; every other `.zl` file under `src/` is
part of the package automatically. Namespaces are not tied to directories:
each file says which namespace it belongs to in its first line.

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "wordfreq"
version = "0.1.0"
namespace = "wordfreq"
entry = "src/main.zl"
```

## src/main.zl: the boundary

The program is split the way most command-line tools should be:

- `parseArgs` turns the argument list into an `Options` struct, or fails.
  Struct fields can have defaults (`Top int = 10`), so `Options{}` already
  holds the defaults.
- `count` does the work. Every step that can fail ends in `?`: the error
  goes back to the caller unchanged, or with a prefix when written
  `?("--top")`. `count` never prints an error and never decides an exit
  code.
- `run` is the boundary. It is the only function with `try ... catch`,
  the only one that formats an error for a person, and it maps failures
  to exit codes: 2 for a usage mistake, 1 for a failure while working.
  It writes to the `io.Writer`s it is given, which is what makes it easy
  to test and to drive from the demo.

`main` passes `os.Stdout()` for both writers so that the error messages are
part of the checked output; a real tool passes `os.Stderr()` for the
second one.

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// wordfreq: count the words of text files and print the most frequent
// ones as a table, then save the same ranking as a Markdown report.
//
//     wordfreq [--top N] [--stop words.txt] [--out report.md] file...
namespace wordfreq

import io

import wordfreq.counter

// Options are the parsed command line. Hand-written parsing is enough for
// two flags; a tool with subcommands would use the cli package (see the
// todo-cli example).
struct Options {
    Top   int = 10
    Out   string
    Stop  string
    Files []string
}

func parseArgs(args []string) Options! {
    var o = Options{}
    var i = 0
    for i < len(args) {
        var a = args[i]
        switch a {
            case "--top", "--out", "--stop":
                if i + 1 >= len(args) {
                    return error(a + " needs a value")
                }
                i += 1
                switch a {
                    // ?("...") wraps strconv's error with the flag name, so
                    // the user learns which value was wrong.
                    case "--top": o.Top = strconv.Atoi(args[i])?("--top")
                    case "--out": o.Out = args[i]
                    default:
                        o.Stop = args[i]
                }
            default:
                if strings.HasPrefix(a, "--") {
                    return error("unknown flag " + a)
                }
                o.Files = append(o.Files, a)
        }
        i += 1
    }
    if len(o.Files) == 0 {
        return error("no input files")
    }
    return o
}

// count does the work and is free to fail anywhere: every fallible step
// ends in `?`, which hands the error to the caller unchanged or with
// context. No error is printed here.
func count(o Options, out io.Writer) void! {
    var c = Counter.New()
    if o.Stop != "" {
        c.LoadStopWords(o.Stop)?
    }
    for path in o.Files {
        c.AddFile(path)?
    }
    var top = c.Top(o.Top)
    io.WriteString(out, report.Summary(c) + "\n\n")?
    io.WriteString(out, report.Table(c, top) + "\n")?
    if o.Out != "" {
        report.Write(o.Out, c, top)?
        var lines = report.Verify(o.Out, len(top))?
        io.WriteString(out, fmt.Format("\nreport: {0} lines written and verified\n", lines))?
    }
}

// run is the boundary between the program and the outside world: the one
// place that turns errors into a message and an exit code. Taking the
// writers as parameters keeps it testable.
func run(args []string, stdout io.Writer, stderr io.Writer) int {
    var o = try parseArgs(args) catch err {
        _ = io.WriteString(
            stderr,
            "wordfreq: " +
                err.Message() +
                "\nusage: wordfreq [--top N] [--stop words.txt] [--out report.md] file...\n",
        )
        return 2
    }
    try count(o, stdout) catch err {
        _ = io.WriteString(stderr, "wordfreq: " + err.Message() + "\n")
        return 1
    }
    return 0
}

func main() {
    // A real tool ends with:
    //     os.Exit(run(os.Args()[1..], os.Stdout(), os.Stderr()))
    // Here the argument lists are fixed so the output is reproducible, and
    // errors go to stdout so they are part of it.
    var out = os.Stdout()
    var tmp = fs.Join(fs.TempDir(), "wordfreq-report.md")
    var runs = [
        [
            "--top",
            "8",
            "--stop",
            "data/stopwords.txt",
            "--out",
            tmp,
            "data/harbor.txt",
            "data/garden.txt",
        ],
        ["--top", "3", "data/garden.txt"],
        ["data/harbor.txt", "data/missing.txt"],
        ["--top", "many", "data/harbor.txt"],
    ]
    for args in runs {
        println(
            "$ wordfreq",
            strings.Join(slices.Map(args, (a string) => strings.Replace(a, tmp, "report.md")), " "),
        )
        println("[exit", run(args, out, out), "]")
        println()
    }
    _ = fs.Remove(tmp)
}
```

## src/counter.zl: reading and counting

`Counter` holds the counts in a `map[string]int`. Reading a missing key
gives the zero value, so `this.counts[word] += 1` needs no "is it there
yet" check. `New` returns a `*Counter`: the methods change the counter in
place and everyone holding the pointer sees the same counts.

`AddLine` walks the line as runes (`strings.Runes`), so "Café" is one word
of four letters, not five bytes. `AddFile` streams the file through a
`Scanner`, the equivalent of Go's `bufio.Scanner`: memory use does not
depend on the file size. Note the error check after the loop: `Scan`
returns `false` both at the end of the input and on a read error.

There is no `defer f.Close()`. `os.File` has a destructor (`Drop`), so the
file is closed when `f` goes out of scope, on every path out of the
function, including the early return of a failed `?`. Call `Close`
yourself only when you want to see its error, which matters after
writing, not after reading. See [Memory](../../guide/memory.md).

`Top` sorts with `slices.SortFunc`. Map iteration order is not specified,
so the comparison breaks ties by the word itself: without that, two runs
could print equally frequent words in a different order.

<!-- file: src/counter.zl -->
```zell title="src/counter.zl"
// Counting words: tokenizing a line, reading a file line by line, and
// turning the map of counts into a sorted ranking.
namespace wordfreq.counter

// Words shorter than this are skipped: "a", "an", "of" would otherwise
// fill the top of every ranking.
const MinLength = 3

// One row of the ranking.
struct Entry {
    Word  string
    Count int
}

// Counter accumulates word counts over any number of inputs.
struct Counter {
    counts map[string]int
    stop   map[string]bool
    Words  int
    Lines  int
    Files  int

    // New returns an empty counter on the heap: the methods below change
    // it in place, and every caller shares the one instance.
    static func New() *Counter {
        return new Counter{counts: map[string]int{}, stop: map[string]bool{}}
    }

    // LoadStopWords reads words to ignore, one per line. io.ReadLines is
    // the simple way to read a small file whole; AddFile below shows the
    // streaming way for inputs of any size.
    func LoadStopWords(path string) void! {
        for line in io.ReadLines(path)?("stop words") {
            var w = strings.Trim(line)
            if w != "" && !strings.HasPrefix(w, "#") {
                this.stop[strings.ToLower(w)] = true
            }
        }
    }

    // AddLine splits one line into words. A word is a run of letters
    // (apostrophes inside a word are kept, so "don't" stays one word);
    // everything else separates words. Letters are lower-cased so that
    // "The" and "the" count together.
    func AddLine(line string) {
        this.Lines += 1
        var word []char = []
        for r in strings.Runes(line) {
            if unicode.IsLetter(r) || (r == '\'' && len(word) > 0) {
                word = append(word, unicode.ToLower(r))
                continue
            }
            this.flush(word)
            word = []
        }
        this.flush(word)
    }

    func flush(runes []char) {
        var word = strings.TrimRight(strings.FromRunes(runes), "'")
        if len(word) < MinLength || this.stop[word] {
            return
        }
        this.counts[word] += 1
        this.Words += 1
    }

    // AddFile streams a file through a Scanner, so memory stays flat no
    // matter how large the file is. The error from Open is returned to the
    // caller as it is (its message already names the path). There is no
    // `defer f.Close()`: os.File has a destructor (Drop), so the file is
    // closed when f goes out of scope, on every path out of the function.
    // Call Close yourself only when you want its error, as after writing.
    func AddFile(path string) void! {
        var f = os.Open(path)?
        var sc = io.NewScanner(f)
        for sc.Scan() {
            this.AddLine(sc.Text())
        }
        // Scan returns false both at the end and on a read error, so the
        // error is checked once after the loop, exactly like Go's bufio.
        if sc.Err() != nil {
            return errors.Wrap(sc.Err(), "read " + path)
        }
        this.Files += 1
    }

    // Distinct is the number of different words seen.
    func Distinct() int {
        return len(this.counts)
    }

    // Count returns how often a word occurred (0 when it never did).
    func Count(word string) int {
        return this.counts[word]
    }

    // Top returns the n most frequent words. Ties are broken
    // alphabetically so the output is the same on every run: map
    // iteration order is not something to rely on.
    func Top(n int) []Entry {
        var all []Entry = []
        for word, count in this.counts {
            all = append(all, Entry{Word: word, Count: count})
        }
        slices.SortFunc(
            all,
            (a Entry, b Entry) => a.Count > b.Count || (a.Count == b.Count && a.Word < b.Word),
        )
        if n < len(all) {
            return all[..n]
        }
        return all
    }
}
```

## src/report.zl: tables and the report file

`text.table` pads every column to its widest cell (it knows that wide East
Asian characters take two columns) and can render the same rows as a
Markdown table. `Write` builds the whole report in a `strings.Builder` and
writes it with one call, so a failure cannot leave a half-written report
behind a success message. `Verify` reads the file back with
`io.ReadLines` and checks the number of table rows: a cheap end-to-end
check worth having in any tool that writes files other programs read.

Inside the package, `report.Summary` in `main.zl` needs no import: a
namespace can name its child namespaces relative to itself
(`wordfreq` → `report` is `wordfreq.report`).

<!-- file: src/report.zl -->
```zell title="src/report.zl"
// Rendering the ranking: an aligned table for the terminal, a Markdown
// file for sharing, and reading that file back to check what was written.
namespace wordfreq.report

import wordfreq.counter

// Table renders the ranking for the terminal with text.table, which pads
// every column to its widest cell. Numbers are right-aligned, as people
// expect from a column of counts.
func Table(c *Counter, top []Entry) string {
    var t = text.table.New(header)
    for col in [0, 2, 3] {
        t.SetAlign(col, text.table.Right)
    }
    for row in rows(c, top) {
        t.AddRow(row)
    }
    // The plain style pads the last column too; trimming keeps the output
    // friendly to diff and to byte-exact tests.
    var lines = slices.Map(strings.Split(t.String(), "\n"), (l string) => strings.TrimRight(l, " "))
    return strings.TrimRight(strings.Join(lines, "\n"), "\n")
}

// Markdown renders the same rows as a GitHub table for the report file.
func Markdown(c *Counter, top []Entry) string {
    var all = [header]
    for row in rows(c, top) {
        all = append(all, row)
    }
    return text.table.Format(all, text.table.Markdown)
}

const header = ["#", "word", "count", "share"]

func rows(c *Counter, top []Entry) [][]string {
    var out [][]string = []
    for i, e in top {
        out = append(out, [
            strconv.Itoa(i + 1),
            e.Word,
            strconv.Itoa(e.Count),
            percent(e.Count, c.Words),
        ])
    }
    return out
}

func percent(n int, total int) string {
    if total == 0 {
        return "-"
    }
    return fmt.Sprintf("%.1f%%", float(n) * 100.0 / float(total))
}

// Summary is the one-line overview printed above the table.
func Summary(c *Counter) string {
    return fmt.Format(
        "{0}, {1}, {2}, {3} distinct",
        plural(c.Files, "file"),
        plural(c.Lines, "line"),
        plural(c.Words, "word"),
        c.Distinct(),
    )
}

func plural(n int, noun string) string {
    if n == 1 {
        return "1 " + noun
    }
    return strconv.Itoa(n) + " " + noun + "s"
}

// Write saves the report as Markdown. Building the whole text first and
// writing it with one call means a failed write never leaves half a
// report behind with a success message printed.
func Write(path string, c *Counter, top []Entry) void! {
    var b = strings.Builder{}
    b.Write("# Word frequencies\n\n")
    b.Write(Summary(c) + "\n\n")
    b.Write(Markdown(c, top))
    b.Write("\n")
    io.WriteFile(path, b.String())?("write report")
}

// Verify reads the report back and counts its table rows: a cheap
// end-to-end check that what is on disk is what we meant to write.
func Verify(path string, want int) int! {
    var lines = io.ReadLines(path)?("read report back")
    var rows = 0
    for l in lines {
        if strings.HasPrefix(l, "|") {
            rows += 1
        }
    }
    // header + separator + one row per entry
    if rows != want + 2 {
        return error(fmt.Format("report has {0} table rows, want {1}", rows, want + 2))
    }
    return len(lines)
}
```

## src/counter_test.zl: tests

`zell test` compiles every `*_test.zl` file under `src/` together with the
package and runs the `@test` functions; `zell run` and `zell build` leave
these files out. The tests use `Counter` directly, without files, except
the last one, which checks that a missing file is an error that names the
path. See [Testing](../../guide/testing.md).

<!-- file: src/counter_test.zl -->
```zell title="src/counter_test.zl"
// Unit tests for the counter. `zell test` in the project directory
// compiles every *_test.zl file under src/ together with the package and
// runs each @test function; `zell run` and `zell build` leave them out.
namespace wordfreq.tests

import wordfreq.counter

@test func TestWordsAreLowerCasedAndSplitOnPunctuation(t *testing.T) {
    var c = Counter.New()
    c.AddLine("The cat; the CAT, the hat!")
    testing.Equal(t, c.Count("the"), 3)
    testing.Equal(t, c.Count("cat"), 2)
    testing.Equal(t, c.Words, 6)
    testing.Equal(t, c.Distinct(), 3)
}

@test func TestShortWordsAndApostrophes(t *testing.T) {
    var c = Counter.New()
    c.AddLine("I don't go to sea, it's cold'")
    // "i", "go", "to" are shorter than MinLength; the apostrophe stays
    // inside a word but not at its end.
    testing.Equal(t, c.Count("don't"), 1)
    testing.Equal(t, c.Count("it's"), 1)
    testing.Equal(t, c.Count("cold"), 1)
    testing.Equal(t, c.Count("go"), 0)
}

@test func TestTopBreaksTiesAlphabetically(t *testing.T) {
    var c = Counter.New()
    c.AddLine("pear apple pear fig apple kiwi")
    var top = c.Top(3)
    testing.Equal(t, len(top), 3)
    testing.Equal(t, top[0].Word, "apple")
    testing.Equal(t, top[1].Word, "pear")
    testing.Equal(t, top[2].Word, "fig")
    testing.Equal(t, len(c.Top(100)), 4)
}

@test func TestMissingFileIsAnError(t *testing.T) {
    var c = Counter.New()
    try c.AddFile("no/such/file.txt") catch err {
        testing.Contains(t, err.Message(), "no/such/file.txt")
        return
    }
    t.Fatal("AddFile succeeded on a missing file")
}
```

## The data

<!-- file: data/harbor.txt -->
```text title="data/harbor.txt"
The harbor wakes before the town does.
Gulls argue over the nets, the boats knock against the pier,
and the first ferry sounds its horn across the grey water.

By noon the harbor is loud: crates, engines, voices,
the smell of salt and diesel and fresh bread from the corner bakery.
By night the water is black again, and the boats sleep.
```

<!-- file: data/garden.txt -->
```text title="data/garden.txt"
In the garden the rain came early this year.
The beans climbed the fence, the tomatoes split their skins,
and the old pear tree gave more fruit than the town could eat.

We carried baskets of pears to the harbor,
traded them for fish, and walked home in the rain.
```

<!-- file: data/stopwords.txt -->
```text title="data/stopwords.txt"
# Words too common to be interesting. One per line; lines starting
# with # are comments.
the
and
they
them
this
that
with
from
for
its
was
```

## Output

<!-- file: expected.out -->
```text title="expected.out"
$ wordfreq --top 8 --stop data/stopwords.txt --out report.md data/harbor.txt data/garden.txt
2 files, 13 lines, 70 words, 64 distinct

#  word     count  share
-  -------  -----  -----
1  harbor       3   4.3%
2  boats        2   2.9%
3  rain         2   2.9%
4  town         2   2.9%
5  water        2   2.9%
6  across       1   1.4%
7  again        1   1.4%
8  against      1   1.4%

report: 15 lines written and verified
[exit 0 ]

$ wordfreq --top 3 data/garden.txt
1 file, 6 lines, 45 words, 35 distinct

#  word  count  share
-  ----  -----  -----
1  the       9  20.0%
2  and       2   4.4%
3  rain      2   4.4%
[exit 0 ]

$ wordfreq data/harbor.txt data/missing.txt
wordfreq: open data/missing.txt: No such file or directory
[exit 1 ]

$ wordfreq --top many data/harbor.txt
wordfreq: --top: strconv.Atoi: parsing "many": invalid syntax
usage: wordfreq [--top N] [--stop words.txt] [--out report.md] file...
[exit 2 ]

```

The first run filters the stop words, so the table shows the words that
say something about the texts. The second does not, and "the" wins by far,
as it does in almost any English text. The last two runs fail: one with
exit code 1 (a file is missing; the message comes from the operating
system and already names the path), one with exit code 2 (a usage
mistake, so the usage line is printed too).

## Next

- [todo-cli](../todo-cli/README.md): subcommands and flags with the `cli`
  package, a JSON file as storage.
- [Errors](../../guide/errors.md): the full story of `T!`, `?` and `try`.
