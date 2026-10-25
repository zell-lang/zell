# validate: your own attributes and reflection

`@json`, `@cli.flag` and `@sql.column` in the other examples are not
compiler magic: an attribute is an ordinary struct marked `@attribute`,
and a library reads it with `reflect`. This example writes a small
validation library that way, then a command registry that finds its
commands by attribute, with no registration code.

What it shows:

- declaring attributes with parameters and defaults
  (`@attribute(targets: [...]) struct Range { Min int; Max int = ... }`)
  and using several on one field;
- a generic `Validate<T>(v T)` built on `reflect.Fields<T>()`, which the
  compiler unrolls into per-field code, with `f.Attribute<A>()` and
  `f.Get(v)`;
- static `if` on compile-time conditions (`f.Attribute<A>() != nil`,
  `f.Type.Kind`, `reflect.TypeOf<F>().Kind`), so one generic function
  handles numbers, strings, slices and nested structs with per-field code;
- `reflect.Error(msg)` as a library-level `static_assert`, also rejecting
  an attribute on a field of the wrong type;
- reading attribute values at run time from `reflect.TypeOf<T>().Fields`;
- `reflect.TypesWith<A>()`: every type of the program with an attribute,
  and `MethodInfo.Params` + `Invoke` to call a method with arguments
  converted from strings.

## Run it

```sh
cd docs/examples/validate
zell run
zell test
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `validate`) |
| `src/attrs.zl` | `validate.attrs`: the attribute declarations |
| `src/rules.zl` | `validate.rules`: `Validate<T>`, `Check<T>`, the error types |
| `src/registry.zl` | `validate.registry`: the `@Command` attribute, usage, dispatch |
| `src/main.zl` | a form with attributes, its schema, three commands, the demo |
| `src/rules_test.zl` | tests (`zell test`) |

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "validate"
version = "0.1.0"
namespace = "validate"
entry = "src/main.zl"
```

## src/attrs.zl: declaring attributes

An attribute is a struct with `@attribute`. Its fields are its parameters,
a field default makes a parameter optional, and `targets` restricts where
it may appear (fields here; structs, methods, parameters, enums and
variants are the other targets). The compiler checks each use like a
struct literal: `@Range(Mni: 1)` or `@Range(Min: "one")` does not
compile, and neither does `@Range` on a struct.

The attributes have capitalized names because they are used from other
namespaces; lower-case names are private to their namespace, attributes
included.

<!-- file: src/attrs.zl -->
```zell title="src/attrs.zl"
// The attributes of the validation library. An attribute is an ordinary
// struct marked @attribute: its fields are the attribute's parameters
// (with defaults), and `targets` says where it may be written. The
// compiler checks every use: an unknown attribute, a misspelled
// parameter or a value of the wrong type is a compile error.
//
// The names start with a capital letter because they are used from other
// namespaces (validate.rules, the application): attributes follow the
// usual visibility rule.
namespace validate.attrs

// The field must not be empty: "" for strings, 0 for numbers, no
// elements for slices.
@attribute(targets: [reflect.Target.Field])
struct Required {}

// The value must lie in [Min, Max]: the number itself for numeric
// fields, the length for strings and slices.
@attribute(targets: [reflect.Target.Field])
struct Range {
    Min int
    Max int = 2147483647
}

// A human-readable name for messages; the field name is used without it.
@attribute(targets: [reflect.Target.Field])
struct Label {
    Text string
}

// The value must look like an e-mail address.
@attribute(targets: [reflect.Target.Field])
struct Email {}
```

## src/rules.zl: a generic validator

`reflect.Fields<T>()` inside a generic function is an instruction to the
compiler, not a call: the loop body is compiled once for each field of
`T`. In each copy, `f.Get(v)` is a direct access to that field with its
own static type, and `f.Attribute<Range>()` is the attribute written on
that field, or `nil`. The result is what you would write by hand for each
struct, with no type inspection at run time.

Inside the loop every question about `f` is answered at compile time,
for each field: whether it has an attribute (`f.Attribute<Range>() !=
nil`, or `r != nil` for a variable holding it), its kind
(`f.Type.Kind == reflect.Kind.Struct`), its name. An `if` on such a
condition is a **static `if`**: only the branch taken is compiled for
that field. So the copy for `Age int` holds just the range check, typed
for an `int`; the copy for `Newsletter bool` holds nothing; and the copy
for `Home Address` recurses into the nested struct and `continue`s. The
same holds for `&&`: in `f.Attribute<Email>() != nil &&
!looksLikeEmail(f.Get(v))` the right side is compiled only for fields
with `@Email`, so it can pass the field to a function taking a
`string`.

The helpers `size` and `describe` are generic over the field type `F` and
branch with a static `if` on `reflect.TypeOf<F>().Kind`: `size` returns
`x` for integers and `len(x)` for strings and slices. Once the `int`
branch returns, the rest of the function is dead for `F = int` and is not
compiled for it.

`reflect.Error` is a compile-time error raised when the branch holding it
is compiled for some `T`. `Validate` starts with one for a `T` that is
not a struct; with `_ = Validate(42)` in `main` the build fails with:

```text
error: Validate needs a struct, got int
  --> src/rules.zl:43:9
   = note: raised by Validate<int>
```

`fieldsOf` uses it to reject a rule written on a field it cannot apply
to: `@Required Newsletter bool` in `main.zl` does not compile.

```text
error: @Required and @Range need a number, a string or a slice; Newsletter is bool
  --> src/rules.zl:81:17
   = note: raised by fieldsOf<SignUp>
```

`Check` adapts the result to `T!` code: it returns an `Invalid` error that
carries every problem, so a caller can `?` it or pick it apart with
`errors.As`.

<!-- file: src/rules.zl -->
```zell title="src/rules.zl"
// Validate<T>: a generic validator driven by the attributes of attrs.
//
// reflect.Fields<T>() in a generic function is unrolled by the compiler:
// the loop body is generated once per field of T, f.Get(v) is a direct
// field access with the field's static type, and f.Attribute<A>() is the
// attribute value written on that field (nil when there is none). The
// result is the code you would write by hand for each struct, with no
// run-time type inspection.
namespace validate.rules

import validate.attrs

// FieldError is one problem with one field. Path is the field's position
// ("address.zip" for nested structs), Label what to show to a person.
struct FieldError {
    Path    string
    Label   string
    Problem string

    func Message() string {
        return this.Label + " " + this.Problem
    }
}

// Invalid is the error returned by Check: all problems at once, so a
// form can show every message, not just the first.
struct Invalid {
    Errors []FieldError

    func Message() string {
        return strings.Join(slices.Map(this.Errors, (e FieldError) => e.Message()), "; ")
    }
}

// Validate returns the problems of v, in field order. Only structs can be
// validated: for any other T the static `if` below raises reflect.Error,
// a compile-time error naming the instantiation that caused it,
// Validate<int> (the library's static_assert). For a struct the branch is
// not taken, so it costs nothing; and since reflect.Error ends the
// function, no `return` follows it.
func Validate<T>(v T) []FieldError {
    if reflect.TypeOf<T>().Kind != reflect.Kind.Struct {
        reflect.Error("Validate needs a struct, got " + reflect.TypeOf<T>().Name)
    }
    return fieldsOf(v, "")
}

// Check is Validate for code that works with T! errors.
func Check<T>(v T) void! {
    var errs = Validate(v)
    if len(errs) > 0 {
        return Invalid{Errors: errs}
    }
}

// Every condition on f below is a compile-time constant for each field:
// whether it has an attribute, its kind, its name. A static `if` keeps only
// the branch taken, so each copy of the loop body holds just the checks
// that apply to that field, typed by its own type, and a rule written on
// a field it cannot apply to is a compile error.
func fieldsOf<T>(v T, prefix string) []FieldError {
    var out []FieldError = []
    for f in reflect.Fields<T>() {
        var path = prefix + strings.ToLower(f.Name)
        var label = path
        var l = f.Attribute<Label>()
        if l != nil {
            label = l.Text
        }
        // A nested struct is validated with its own attributes, its paths
        // prefixed by the field name.
        if f.Type.Kind == reflect.Kind.Struct {
            for e in fieldsOf(f.Get(v), path + ".") {
                out = append(out, e)
            }
            continue
        }
        var r = f.Attribute<Range>()
        if f.Attribute<Required>() != nil || r != nil {
            if f.Type.Kind != reflect.Kind.Int && f.Type.Kind != reflect.Kind.String && f.Type.Kind != reflect.Kind.Slice {
                reflect.Error("@Required and @Range need a number, a string or a slice; " + f.Name + " is " + f.Type.Name)
            }
        }
        if f.Attribute<Email>() != nil && f.Type.Kind != reflect.Kind.String {
            reflect.Error("@Email needs a string field; " + f.Name + " is " + f.Type.Name)
        }
        if f.Attribute<Required>() != nil && size(f.Get(v)) == 0 {
            out = append(out, FieldError{Path: path, Label: label, Problem: "is required"})
            // Further rules would only repeat the same complaint.
            continue
        }
        if r != nil {
            var n = size(f.Get(v))
            if n < r.Min || n > r.Max {
                out = append(out, FieldError{Path: path, Label: label, Problem: describe(f.Get(v), r)})
            }
        }
        if f.Attribute<Email>() != nil && !looksLikeEmail(f.Get(v)) {
            out = append(out, FieldError{
                Path: path,
                Label: label,
                Problem: "is not an e-mail address",
            })
        }
    }
    return out
}

// size is what Required and Range measure: the number itself, or the
// length of a string or slice (fieldsOf admits no other kind).
func size<F>(x F) int {
    if reflect.TypeOf<F>().Kind == reflect.Kind.Int {
        return x
    }
    return len(x)
}

func describe<F>(x F, r *Range) string {
    var what = "must be"
    var of = ""
    if reflect.TypeOf<F>().Kind == reflect.Kind.String {
        what = "must have a length"
        of = " of"
    } else if reflect.TypeOf<F>().Kind == reflect.Kind.Slice {
        what = "must have a number of items"
        of = " of"
    }
    if r.Min == r.Max {
        return fmt.Format("{0}{1} exactly {2}", what, of, r.Min)
    }
    if r.Max == 2147483647 {
        return fmt.Format("{0}{1} at least {2}", what, of, r.Min)
    }
    return fmt.Format("{0} between {1} and {2}", what, r.Min, r.Max)
}

func looksLikeEmail(s string) bool {
    var at = strings.Index(s, "@")
    return at > 0 &&
        strings.LastIndex(s, "@") == at &&
        strings.Contains(s[at + 1..], ".") &&
        !strings.ContainsAny(s, " ,;")
}
```

## src/registry.zl: finding commands by attribute

`reflect.TypesWith<Command>()` is the list of every type in the whole
program, dependencies included, declared with `@Command`, in a fixed
order (package, file, declaration). The compiler builds the list, so
there is no registration call to forget and no scan at start-up, and
only the marked types get reflection tables.

`Usage` reads the parameters of each `Run` method from
`MethodInfo.Params` (name, position, type). `Run` converts the strings of
a command line to those types and calls the method with
`MethodInfo.Invoke(receiver, args)`, which takes and returns `any`: one
dispatcher for commands it has never seen, at the cost of boxing the
arguments. When the signature is fixed and known, `Call<F>()` gives a
typed function instead (see the [tour](../../guide/tour.md#reflection-and-attributes)).

<!-- file: src/registry.zl -->
```zell title="src/registry.zl"
// A command registry with no registration code. Every struct marked
// @Command anywhere in the program (this package and all of its
// dependencies) is listed by reflect.TypesWith<Command>(): the compiler
// builds that list, so there is no start-up scan and nothing to forget.
//
// A command is a struct with a method Run whose parameters are the
// command's arguments. The registry reads their names and types from
// MethodInfo.Params to print usage and convert the strings of a command
// line, then calls Run with MethodInfo.Invoke.
namespace validate.registry

@attribute(targets: [reflect.Target.Struct])
struct Command {
    Name string
    Help string
}

// Usage lists the commands with their arguments, e.g. `add <a int> <b int>`.
func Usage() string {
    var lines []string = []
    for t in reflect.TypesWith<Command>() {
        var c = t.Attribute<Command>()
        var args = slices.Map(
            t.Method("Run").Params,
            (p *reflect.ParamInfo) => "<" + p.Name + " " + p.Type.Name + ">",
        )
        var head = strings.Trim(c.Name + " " + strings.Join(args, " "))
        lines = append(lines, "  " + strings.PadRight(head, 46, " ") + c.Help)
    }
    return strings.Join(lines, "\n")
}

// Run finds the command by name, converts args to the types of Run's
// parameters and invokes it. Invoke takes and returns `any`, so this is
// the dynamic path: one generic dispatcher for commands it has never
// seen. The typed path, when the signature is fixed, is
// t.Method("Run").Call<func(*T, int) string>(), as fast as a direct call.
func Run(line []string) string! {
    if len(line) == 0 {
        return error("no command")
    }
    for t in reflect.TypesWith<Command>() {
        if t.Attribute<Command>().Name != line[0] {
            continue
        }
        var m = t.Method("Run")
        if len(line) - 1 != len(m.Params) {
            return error(
                fmt.Format(
                    "{0}: expected {1} argument(s), got {2}",
                    line[0],
                    len(m.Params),
                    len(line) - 1,
                ),
            )
        }
        var args []any = []
        for p in m.Params {
            args = append(args, convert(line[p.Index + 1], p)?)
        }
        // t.New() makes a fresh receiver (*T, zeroed) for every call.
        var result = m.Invoke(t.New(), args)?
        switch result {
            case string s: return s
            default:
                return fmt.Format("{0}", result)
        }
    }
    return error("unknown command " + strconv.Quote(line[0]))
}

// convert parses one argument by the parameter's type kind; the
// parameter's name makes the error message useful.
func convert(s string, p *reflect.ParamInfo) any! {
    switch p.Type.Kind {
        case reflect.Kind.String: return s
        case reflect.Kind.Int: return strconv.Atoi(s)?(p.Name)
        case reflect.Kind.Float: return strconv.ParseFloat(s)?(p.Name)
        case reflect.Kind.Bool: return strconv.ParseBool(s)?(p.Name)
    }
    return error(fmt.Format("{0}: parameters of type {1} are not supported", p.Name, p.Type.Name))
}
```

## src/main.zl: using it

`SignUp` is a form: the attributes say what a valid one is, and
`Validate(form)` checks it. Note `Home Address`: a nested struct is
validated with its own attributes, and its errors get paths like
`home.zip`.

`Schema<T>` reads the same attributes at run time. `reflect.TypeOf<T>()`
is a table the compiler emitted, and `f.Attribute<Range>()` on a
`FieldInfo` returns the attribute as an ordinary `*Range` struct, with
the values written in the source.

The three commands are plain structs with `@Command`; nothing else
connects them to the registry.

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// validate: declaring your own attributes and reading them with
// compile-time reflection (a generic validator) and with run-time
// reflection (a schema printout and a command registry).
namespace validate

import io

import validate.attrs
import validate.registry
import validate.rules

struct Address {
    @Required                                    City string
    @Label("postal code") @Range(Min: 5, Max: 5) Zip  string
}

// SignUp is a form. The attributes say what a valid one is; the code that
// checks them is generated by Validate<SignUp>.
struct SignUp {
    @Label("user name") @Required @Range(Min: 3, Max: 16) Name string
    @Label("e-mail") @Required @Email                     Mail string
    @Range(Min: 13, Max: 130)                             Age  int
    @Range(Min: 1, Max: 3)                                Tags []string
    Home       Address
    Newsletter bool
}

// Schema prints the attributes at run time: TypeOf<T>() is a table the
// compiler emitted, and Attribute<A>() returns the attribute's values as
// an ordinary struct (*Range with Min and Max), or nil.
func Schema<T>() string {
    var t = reflect.TypeOf<T>()
    var b = strings.Builder{}
    b.Write(t.FullName + "\n")
    for f in t.Fields {
        var rules []string = []
        if f.Attribute<Required>() != nil {
            rules = append(rules, "required")
        }
        var r = f.Attribute<Range>()
        if r != nil {
            rules = append(rules, fmt.Format("range {0}..{1}", r.Min, r.Max))
        }
        if f.Attribute<Email>() != nil {
            rules = append(rules, "email")
        }
        var l = f.Attribute<Label>()
        if l != nil {
            rules = append(rules, "label " + strconv.Quote(l.Text))
        }
        var line = fmt.Format(
            "  {0} {1}  {2}",
            strings.PadRight(f.Name, 10, " "),
            strings.PadRight(f.Type.Name, 9, " "),
            strings.Join(rules, ", "),
        )
        b.Write(strings.TrimRight(line, " ") + "\n")
    }
    return b.String()
}

func show(title string, form SignUp) {
    println(title)
    var errs = Validate(form)
    if len(errs) == 0 {
        println("  valid")
    }
    for e in errs {
        println(" ", strings.PadRight(e.Path, 10, " "), e.Message())
    }
}

// The commands. Nothing registers them: @Command is enough for
// registry.Run to find them.

@Command(Name: "check", Help: "validate a sign-up from arguments")
struct CheckCommand {
    func Run(name string, mail string, age int) string {
        var form = SignUp{
            Name: name,
            Mail: mail,
            Age: age,
            Tags: ["cli"],
            Home: Address{City: "Oslo", Zip: "01500"},
        }
        // Check returns an Invalid error that carries every problem.
        try Check(form) catch err {
            var inv Invalid
            if errors.As(err, &inv) {
                return fmt.Format("{0} problem(s): {1}", len(inv.Errors), err)
            }
            return err.Message()
        }
        return "ok"
    }
}

@Command(Name: "add", Help: "add two integers")
struct AddCommand {
    func Run(a int, b int) int {
        return a + b
    }
}

@Command(Name: "schema", Help: "print the rules of SignUp")
struct SchemaCommand {
    func Run() string {
        return strings.TrimRight(Schema<SignUp>(), "\n")
    }
}

func main() {
    show("an empty form:", SignUp{})
    show("a form with mistakes:", SignUp{
        Name: "al",
        Mail: "al.example.com",
        Age: 7,
        Tags: ["a", "b", "c", "d"],
        Home: Address{City: "Oslo", Zip: "150"},
    })
    show("a good form:", SignUp{
        Name: "alice",
        Mail: "alice@example.com",
        Age: 34,
        Tags: ["news"],
        Home: Address{City: "Oslo", Zip: "01500"},
    })

    // Uncommenting this line fails the build, not the run:
    //     error: Validate needs a struct, got int
    //     = note: raised by Validate<int>
    // _ = Validate(42)

    println("commands:")
    println(registry.Usage())
    var lines = [
        ["schema"],
        ["add", "2", "40"],
        ["add", "2"],
        ["add", "two", "40"],
        ["check", "bob", "bob@example.com", "40"],
        ["check", "b", "bob", "200"],
        ["deploy"],
    ]
    for line in lines {
        println("$", strings.Join(line, " "))
        var out = try registry.Run(line) catch err {
            println("error:", err)
            continue
        }
        println(out)
    }
}
```

## src/rules_test.zl: tests

Test fixtures are small structs declared in the test file, with exactly
the attributes each test is about.

<!-- file: src/rules_test.zl -->
```zell title="src/rules_test.zl"
// Tests for the validator, on small structs declared just for them: the
// rules come from attributes, so a test type is the clearest fixture.
namespace validate.tests

import validate.attrs
import validate.rules

struct Point {
    @Range(Min: 0, Max: 10) X int
    @Range(Min: 0)          Y int
}

struct Named {
    @Required @Label("the name") Name string
    Note string
}

@test func TestRangeOnNumbers(t *testing.T) {
    testing.Equal(t, len(Validate(Point{X: 5, Y: 100})), 0)
    var errs = Validate(Point{X: 11, Y: -1})
    testing.Equal(t, len(errs), 2)
    testing.Equal(t, errs[0].Message(), "x must be between 0 and 10")
    testing.Equal(t, errs[1].Message(), "y must be at least 0")
}

@test func TestRequiredUsesTheLabel(t *testing.T) {
    var errs = Validate(Named{Note: "unlabelled fields without rules are ignored"})
    testing.Equal(t, len(errs), 1)
    testing.Equal(t, errs[0].Path, "name")
    testing.Equal(t, errs[0].Message(), "the name is required")
}

@test func TestCheckReturnsAllProblems(t *testing.T) {
    try Check(Point{X: -1, Y: -1}) catch err {
        var inv Invalid
        testing.True(t, errors.As(err, &inv), "Invalid expected")
        testing.Equal(t, len(inv.Errors), 2)
        return
    }
    t.Fatal("Check accepted an invalid point")
}
```

## Output

<!-- file: expected.out -->
```text title="expected.out"
an empty form:
  name       user name is required
  mail       e-mail is required
  age        age must be between 13 and 130
  tags       tags must have a number of items between 1 and 3
  home.city  home.city is required
  home.zip   postal code must have a length of exactly 5
a form with mistakes:
  name       user name must have a length between 3 and 16
  mail       e-mail is not an e-mail address
  age        age must be between 13 and 130
  tags       tags must have a number of items between 1 and 3
  home.zip   postal code must have a length of exactly 5
a good form:
  valid
commands:
  check <name string> <mail string> <age int>   validate a sign-up from arguments
  add <a int> <b int>                           add two integers
  schema                                        print the rules of SignUp
$ schema
validate.SignUp
  Name       string     required, range 3..16, label "user name"
  Mail       string     required, email, label "e-mail"
  Age        int        range 13..130
  Tags       []string   range 1..3
  Home       Address
  Newsletter bool
$ add 2 40
42
$ add 2
error: add: expected 2 argument(s), got 1
$ add two 40
error: a: strconv.Atoi: parsing "two": invalid syntax
$ check bob bob@example.com 40
ok
$ check b bob 200
3 problem(s): user name must have a length between 3 and 16; e-mail is not an e-mail address; age must be between 13 and 130
$ deploy
error: unknown command "deploy"
```

## Next

- [todo-cli](../todo-cli/README.md) and
  [bookshelf-api](../bookshelf-api/README.md): the standard library's
  attributes (`@cli.flag`, `@json`) at work.
- The tour's [reflection section](../../guide/tour.md#reflection-and-attributes).
