# Reflection: what is compile-time today, what is planned

Status as of 2026-10-23 (master `024a992`). Spec: DESIGN.md "Рефлексия";
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

## 2. Planned: `reflect.TypesWith<A>()`

All types of the program carrying attribute `A`, computed by the compiler
at build time like `Fields<T>`, returned as a constant slice of type
descriptors (each with `Fields`, attributes, and typed constructors /
method pointers as DESIGN.md "Рефлексия" already specifies: `Call<F>`
through a typed function pointer, no boxing).

Why: today `web` builds its route table and DI container inside the
compiler (`compiler/src/sema/web.rs`) by scanning every type for
`@controller`/`@route` attributes; with `TypesWith<A>()` the same table is
built by ordinary package code, the hooks in `sema/web.rs` go away, and
`web` can move to `zell-lang/web` (FRAMEWORK.md plan, steps 1-3). It also
makes the table a constant: no start-up scan.

Design points to settle in DESIGN.md before implementing:
- whole-program semantics with packages: types from dependencies count
  (the attribute's package decides; `@controller` from `web` matches types
  in the application) — the result is per program, so it is computed in
  the final link unit, which means the function is only callable from the
  binary's package or must be re-evaluated per package graph;
- determinism: ordering by package path then declaration order, so route
  tables and generated OpenAPI are stable;
- generics: instantiated types with the attribute appear once per
  instantiation actually used;
- cost: must stay O(types with the attribute), not O(all types), in the
  generated code.

Acceptance: spec paragraph + EBNF untouched (library function), spec tests
in `tests/spec/reflection`, `examples/web_app` rewritten on it with
identical output, `sema/web.rs` deleted.

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

1. `TypesWith<A>()` — spec 1 h, compiler 1 agent run (45-60 min),
   `web` rewrite 1 run, remove `sema/web.rs`.
2. Expression trees — spec first (needs a decision on the node set), then
   2 runs (compiler, `data` rewrite).
3. Attributes as values / run-time `reflect` — after the two above; sized
   when specified.

Working rules for the agent runs: agents/prompts/perf-wave-3.md
("Working rules").
