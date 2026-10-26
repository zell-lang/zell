# Reflection: what is compile-time today, what is planned

Status as of 2026-10-25 (run 2). Spec: DESIGN.md "Рефлексия";
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
generic helpers (`reflect.Error`). Since run 2 (below) binding is typed
without boxing (unrolled `TypesWith`/`Methods`/`Params`, `p.Type` as a
type argument); still open: DI by constructor parameters of structs
(fields can be used: `Fields`).

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

### Done 2026-10-25: descriptor loops and descriptor types (run 2)

Spec: DESIGN.md "Развёртывание типов, методов и параметров"; code in
`compiler/src/sema/reflect_unroll.rs`.

- `for t in reflect.TypesWith<A>()` is unrolled per type (in `TypesWith`
  order, in any function) when the body needs a compile-time descriptor:
  `t` (or a descriptor reached from it) in a type position, a
  `CallOn`/`Apply`/`Get`/`Set` call on one, or a `reflect.Error` call.
  Otherwise it stays the run-time loop over the slice. Generic types
  marked `A` are an error in the unrolled form (their instances are known
  only at the end of the analysis).
- Inside a copy, `for m in t.Methods` (every struct method, async ones
  included) and `for p in m.Params` unroll too. Constants: `t.Name/
  FullName/Package/Kind`, `t.Attribute<B>()`, `m.Name/Static/IsAsync`,
  `len(m.Params)`, `m.Attribute<B>()`, `m.Result.Kind`, `p.Name/Index`,
  `p.Attribute<B>()`, `p.Type.Kind/Name`; the static `if`/`switch`,
  `reflect.Error` and reachability rules of run 1 apply to them.
- Descriptor types: `t`, `f.Type`, `p.Type`, `m.Result`, `m.Args` (a
  struct with a field per parameter) where a type is expected (`var x T`,
  `F<T>()`, `T{}`, `new T{}`); the grammar is unchanged. A run-time
  descriptor in a type position is an error.
- Typed calls: `m.CallOn(recv, args...)` (argument types checked like an
  ordinary call) and `m.Apply(recv, args)` with `p.Set(&args, v)` /
  `p.Get(args)` are direct calls of the method.
- Diagnostics of code that does not depend on the descriptor are reported
  once; a local is unused only if unused in every copy.
- Proof: `examples/reflect_binding.zl` (a typed `@controller`/`@get`/
  `@query`/`@body` binder, an async action and `T!` error paths);
  `compiler/tests/reflection.rs` `unrolled_binder_calls_actions_directly`
  (the HIR and IR of `Dispatch` call each action, `strconv.Atoi` and
  `json.Parse<User>` directly; no `Invoke`, `[]any` or table reads; the
  controllers get no tables). Spec tests `tests/spec/reflection/
  types_with_unrolled`, `methods_unrolled`, `params_unrolled`,
  `descriptor_type_args`, `types_with_runtime_loop` and five `// error:`
  programs.

### Remaining steps

1. The `web_app` example of tag `web-in-tree` rewritten in
   `zell-lang/web` on the unrolled loops, with identical output; its
   profile decides what else needs folding.
2. Descriptor types inside lambdas of an unrolled copy (a lambda sees the
   descriptor as an upvalue today, so `(x t) => ...` does not resolve).
3. Constructor injection: `m.Args` of a static `New` gives the parameter
   list; a DI container still has to map parameter types to providers
   (needs a compile-time map from type to factory, not specified).
4. Generic types in the unrolled `TypesWith` form (needs the loop to be
   expanded after the instance set is closed, e.g. a second pass).

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

What remains after run 2 (2026-10-25): the compile-time path now covers
typed binding, so run-time reflection is only needed where types are not
known statically: `typeof(x)` of an `any` followed by field get/set by
index (`reflect.Value` exists; `Field(i)` by index and `Set` on a value
of another package's unexported field are open), `Invoke`/`Call<F>` for
**async** methods (they are not in `TypeInfo.Methods`; needs a `Task`
returning thunk and an `IsAsync` field, ABI 17.1), method tables of
interfaces, and `Implements(iface)`.

## 4. Done 2026-10-26: expression trees (run 1)

`expr<F>` parameters receive the lambda as a syntax tree value (C#
`Expression<Func<>>`). Spec: DESIGN.md "Деревья выражений" (grammar
unchanged: a type form). Implemented:

- `sema/exprtree.rs`: a lambda checked where `expr<F>` is expected is
  checked as a lambda of type `F`, validated against the node set (one
  whitelist table, `TREE_CALLS`), encoded into a constant `expr.Site`
  (`ExprKind::AttrConst`, read-only data); parameter-free subexpressions
  become slots (literals: constants; anything else: captured values,
  evaluated when the tree is built, the lifted lambda rewritten to read
  them); forbidden forms are errors at the sub-expression.
- Representation: three closures (the lambda, slot values, node types)
  over one *value environment* (`Function::value_env`, a tuple in one
  box; `ExprKind::EnvClosures`): no allocation without captures, one box
  with them (`compiler/tests/exprtree.rs` checks the IR).
- `stdlib_src/expr.zl` (package `expr`): `Expr<F>` (`Root`, `Params`,
  `ParamNames`, `Compile`, `String`), the `Node` payload enum (`Param`,
  `Field`, `Const`, `Captured`, `Binary`, `Unary`, `Call`, `IsNil`,
  `Tuple`, `Struct`), `Op`, `TypeOf`, `FieldOf`, `Children`, `Walk`,
  `Values`, `Format`, and the combinators `And`/`Or`/`AndAll`/`OrAll`/`Not`
  (generic functions cannot be variadic, so the n-ary forms take a slice).
- Tests: `tests/spec/expr` (a library-style SQL printer over every node
  kind, captures in loops, `Compile`, combinators, projections, field
  descriptors through attributes and `reflect.Fields<T>()`, one error
  program per forbidden form), `examples/expr_trees.zl`.

### Done 2026-10-26: `data` on trees (run 2)

The ORM is library code (`stdlib_src/data_model.zl`, `data_sql.zl`);
`sema/orm*.rs` are gone. What the compiler gained for it, all general:

- trees: any parameter-free subexpression is a slot; `Convert`, `Index`
  and `Lambda` nodes (a lambda argument of a call inside the tree, with
  its own value environment); calls of functions and methods marked
  `@expr.call`; operators on types with `Compare`/`Equals` are `Binary`;
  `expr.Expr.SiteId()`/`Slots()`/`Value(k)` for translation caches;
  result type parameters inferred from the lambda (`Select<R>`), a result
  converted to an interface (`expr<func(*T) any>` keys);
- reflection: `reflect.Fields<T>()` over tuples, `f.Type.Elem`/`.Key` as
  descriptor types, `f.Addr(p)`, `reflect.Error` in a method of a generic
  struct raised only if the method is used, short names in folded
  `TypeOf<T>().Name`/`f.Type.Name`;
- fixes found on the way: `nil` for an already bound type parameter,
  variadic flags and deferred errors rolled back with their functions,
  stdlib interfaces not re-resolved in a user file's scope, lambda hints
  resolved in the template's scope.

Still open: inference of `T` from a lambda alone (`Where(u => ...)` on a
generic free function), compile-time checks that depend on the model
(a lambda reading an `@ignore` field fails when the query runs), and
static relation checks (`data.Open` reports a missing foreign key).

## 5. Order and sizing

1. `TypesWith<A>()` — done 2026-10-24 (section 2); static unrolling of
   the loop and descriptor types — done 2026-10-25; next the
   `zell-lang/web` rewrite (1 run).
2. Expression trees — spec first (needs a decision on the node set), then
   2 runs (compiler, `data` rewrite).
3. Attributes as values / run-time `reflect` — after the two above; sized
   when specified.

Working rules for the agent runs: agents/prompts/perf-wave-3.md
("Working rules").
