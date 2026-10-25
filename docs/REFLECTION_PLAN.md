# Reflection: what is compile-time today, what is planned

Status as of 2026-10-25. Spec: DESIGN.md "Рефлексия";
the framework motivation is docs/FRAMEWORK.md "План: тонкое ядро,
фреймворк в пакетах".

## 1. Done: static reflection folded by the compiler

Reflection in Zell is static by design: `reflect.Fields<T>()` is unrolled
per field at compile time and `reflect.TypeOf<T>().Kind == ...` folds to a
constant, so a loop over fields compiles to straight-line code per field
(`compiler/src/sema/reflect.rs`). Commit `bda8098` (2026-10-23, landed with
the `encoding.json` rewrite) finished what was still evaluated at run time
inside such loops:

- `f.Attribute<A>()` folds to a pointer to the immortal attribute value or
  to `nil` (`ExprKind::AttrConst`); `f.Name`, `f.Index`, `f.Exported`,
  `f.Embedded` and the fields of an attribute (`tag.name`) are constants;
  comparisons with constants, `nil` tests of constants and `&&`/`||` with a
  constant side fold; an unused `f` binds to `nil`.
- Codegen does not retain/release constant pointers; a pointer-typed local
  that only ever held constants is not released at block end or function
  exit (`FnState::const_only`); `len(string)` is inlined; the `?`
  error-trace calls are guarded by a null test.

Effect: `json.Parse<T>`/`Format<T>` are plain "read node -> store field"
code with no metadata lookups (typed parse of 100 MB: 360 ms vs Go
1 070 ms, benchmarks/README.md "Performance wave 2"). Every codec or
binder written over `reflect.Fields<T>()` gets the same (XML/YAML/TOML
codecs, `data` materialisation, `web` binding) — not measured separately;
`encodings` (XML 3.0x, TOML 2.2x Go) should be checked for whether they
use `Fields<T>` at all.

## 2. Done: `reflect.TypesWith<A>()` and `reflect.Error(msg)`

Landed 2026-10-24; spec: DESIGN.md "Рефлексия", subsection "Типы с
атрибутом и ошибки из библиотек" (decisions of the maintainer, same day).

- `reflect.TypesWith<A>() []*TypeInfo`: every struct, enum and interface
  of the whole program (the binary's package, all dependencies, the
  stdlib) declared with attribute `A` (`A` must be an attribute type, a
  compile error otherwise). Order: package name, file path, declaration
  order; generic types once per instantiation used. It is an intrinsic
  template: `Sema::types_with_finish` (`compiler/src/sema/reflect_with.rs`)
  collects the lists at the end of the analysis, when the whole program is
  known (a build analyses all packages in one run, so a library call sees
  the application's types: `compiler/tests/package.rs`
  `library_types_with_sees_the_application_types`); codegen emits each
  list once as a constant NULL-backed slice. Only the marked types become
  *full* (method tables, `ptrTo`): cost O(types with `A`); programs that
  never call it emit nothing extra (`compiler/tests/reflection.rs`
  `types_with_only_emits_tables_for_the_marked_types`, and
  `no_reflection_without_use` stays green).
- Units/LSP: `TypesWith` has no diagnostics that depend on the whole
  program (its type is always `[]*TypeInfo`), so per-unit IDE analysis
  needs no special handling; it may see a partial list, which only matters
  for code generation, which always runs over the whole program.
- `reflect.Error(msg)`: a compile error `error: <msg>` with a note
  `raised by <function>` at a reachable call (not in a branch dropped by a
  static `if`, not in the abstract check of a generic body). `msg` must
  fold to a constant (`Sema::const_string`: literals and concatenation,
  `f.Name` of `Fields<T>()`, attribute string fields,
  `reflect.TypeOf<T>().Name/FullName/Package`).
- For frameworks: `TypeInfo.Package` (namespace path), `MethodInfo.Params
  []*ParamInfo` (`Name`, `Type`, `Index`, `Attribute<A>()`,
  `Attributes<A>()`, parameter attributes kept in HIR as
  `Function::param_attrs`), interface tables carry their attributes. ABI
  change is additive (runtime/ABI.md 17.1: `package` at +232 of
  `TypeInfo`, `params` at +72 of `MethodInfo`, new 56-byte `ParamInfo`).
- Spec tests: `tests/spec/reflection/types_with_*`,
  `reflect_error_static_if`, `reflect_error_raised`; example
  `examples/reflect_registry.zl`; guide `docs/guide/tour.md`
  "Reflection and attributes".

What a `zell-lang/web` package can do with it today: collect controllers
(`TypesWith<controller>()`), read type, method and parameter attributes
(`@route`, `@fromQuery`), create a controller (`t.New()`) and call an
action with bound arguments (`m.Invoke(recv, args)`, arguments as `[]any`
built from the request using `p.Type`), call fixed-signature hooks
typed (`m.Call<F>()`), and reject bad declarations at compile time from
generic helpers (`reflect.Error`). Not yet: unrolling `for t in
TypesWith<A>()` at compile time (so binding would be typed, no boxing) and
using `p.Type` as a type argument (`json.Parse<p.Type>`), DI by
constructor parameters of structs (fields can be used: `Fields`).

### Done 2026-10-25: compile-time conditions (finishing run 1)

LANGUAGE_FEEDBACK 33, 53, 66, 67; spec in DESIGN.md "Статическая
рефлексия" ("Константы развёрнутого цикла", "Статический `switch`",
"Запись поля") and "Свои атрибуты"; code in `compiler/src/sema/reflect_ct.rs`.

- Reachability: after a static `if`/`switch` whose taken branch
  terminates, the rest of the block is dead for that instantiation and is
  not checked (in the abstract check it is checked muted); unused
  variables of generic bodies come from the abstract check only.
- Constants per field: `f.Name/Index/Exported/Embedded`,
  `f.Attribute<A>()` (`!= nil`, `== nil`, its fields), `f.Type` in type
  tests (`f.Type.Kind == ...`, `.Elem`/`.Key`, `f.Type == TypeOf<U>()`),
  `f.Type.Name/FullName/Kind` as values, locals bound to them and never
  assigned; `!`, `==`/`!=`/`<`... of constants and a deciding constant left
  side of `&&`/`||` (the right side is then not checked). An `if` whose
  condition folds through any of these (`ReflState::folds`) is static.
- Static `switch`: a type switch over a value of known type in a generic
  body keeps the matching case; a switch over a reflection constant keeps
  the equal case.
- `f.Set(p *T, v)`: a direct store with the field's static type.
- Attributes: an omitted argument is the default or the zero value;
  builtin `@required` makes a field mandatory.
- Proof: `compiler/tests/reflection.rs`
  `field_conditions_fold_without_run_time_lookups` (the instance reads no
  `FieldInfo`/`TypeInfo` and makes no attribute lookup);
  `docs/examples/validate` rejects misplaced rules at compile time.

### Remaining steps

1. Static unrolling of `for t in reflect.TypesWith<A>()` like
   `Fields<T>()` (`t` a compile-time type, `t.Methods` unrolled too).
2. A compile-time type as a type argument (`Parse<p.Type>(text)`) inside
   such loops.
3. The `web_app` example of tag `web-in-tree` rewritten in
   `zell-lang/web` with identical output.

## 3. Run-time reflection: spec is written, implementation partial

DESIGN.md "Рефлексия" already fixes the model: a static read-only
`TypeInfo` table per type (name, size, kind, fields with offsets and
attributes, methods with function pointers, enum variants, `Elem`/`Key`),
attributes stored as ready values (`field.Attribute<json>()` is `*json` or
`nil` in O(1)), `any`/interfaces/errors carrying the `TypeInfo` pointer
(`typeof(x)` is one load, type switch one pointer compare), tables pruned
when the program uses no `reflect`. `tests/spec/reflection` has 17 passing
cases and no pending ones, so what exists is specified and tested; the
CLAUDE.md roadmap item "reflection + attributes as values" is about the
parts of that model not yet reachable from user code (run-time descriptor
of an `any`, field get/set by index, typed `Call<F>` on a method pointer).
Before implementing, write the missing cases as `// status: pending` spec
tests so the gap is explicit, and fix the descriptor layout in
runtime/ABI.md so generated code and the runtime share it.

## 4. Planned: expression trees

`expr<func(User) bool>` parameters receive the lambda as a syntax tree
value (C# `Expression<Func<>>`), so the ORM translates `Where(u => u.Age >
18)` to SQL in library code and `sema/orm*.rs` can be deleted (FRAMEWORK.md
step 1-2). Spec: DESIGN.md new section, grammar unchanged (a type form),
node set and evaluation rules (`Compile()` back to a closure).

## 5. Order and sizing

1. `TypesWith<A>()` — done 2026-10-24 (section 2); next the
   `zell-lang/web` rewrite (1 run), then static unrolling of the loop.
2. Expression trees — spec first (needs a decision on the node set), then
   2 runs (compiler, `data` rewrite).
3. Attributes as values / run-time `reflect` — after the two above; sized
   when specified.

Working rules for the agent runs: agents/prompts/perf-wave-3.md
("Working rules").
