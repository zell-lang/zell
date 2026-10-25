# Build memory and time: findings and plan (2026-10-26)

Measured on `examples/kitchen_sink.zl` (1509 lines) at master 2a92452, 16-thread
Ryzen 7 8845HS, Fedora 42, `target/release/zell`, peak RSS from `/usr/bin/time -f %M`
(it includes child processes such as the linker).

## Where the memory goes

| stage | peak RSS | time |
|---|---|---|
| `zell check` (parse, sema, HIR) | 57 MB | 0.09 s |
| `--emit=llvm-ir` (18.7 MB of IR, 1001 functions) | 127 MB | 0.56 s |
| `--emit=obj`, debug | 171 MB | 1.33 s |
| full debug build (default: 8 codegen threads) | 610 MB | 1.72 s |
| full `--release` build | 305 MB | 5.6 s |

It is **not the linker**. GNU `ld` and `ld.lld` (`ZELL_LINKER=lld`) give the same
peak (610 / 610 MB debug, 305 / 300 MB release). The runtime archive is small:
`libzell_rt.a` is 5 MB, of which 2 MB is DWARF.

It is **parallel code generation** (`compiler/src/parcodegen.rs`). The module is
compiled to machine code on `min(cores, 8)` threads, and each thread holds its own
copy of the module:

| `ZELL_CODEGEN_THREADS` | peak RSS | debug build time |
|---|---|---|
| 1 | 227 MB | 2.48 s |
| 2 | 321 MB | 2.06 s |
| 4 | 410 MB | 1.73 s |
| 8 (default here) | 610 MB | 1.72 s |

Release with one thread: 160 MB, 9.4 s (5.6 s with the default).

Each thread adds about 50 MB. Beyond 4 threads there is no time gain at all.

## Plan, by gain per cost

1. **Cap the codegen threads.** At most 4, and fewer for small modules, scaling
   with the number of functions or IR size. This is a one-line change in
   `parcodegen.rs::threads()`. Expected: 610 → ~410 MB, same build time.
2. **Split the module instead of cloning it.** Each part gets only its own
   function bodies plus declarations of what it references, as `llvm-split`
   does, instead of a full clone with the other bodies deleted. Expected: peak
   memory almost independent of the thread count (~230 MB at 8 threads) with the
   current speed. Size: about a day. Keep: object-cache keys, line tables
   (`lines.rs` maps rows to function symbols across objects), and the
   function + cleanup pairs of the sync-frame unwinding pass, which must stay in
   one part (a bug of that kind was fixed in edace29).
3. **Precompiled stdlib objects ("store it ahead").** The object cache covers
   user code (`zell build -v` logs it). The stdlib is compiled from source into
   every program, because its generic functions are instantiated with the
   program's types.
   - Ship the non-generic stdlib (`strings`, `strconv`, `net`, `encoding.json`
     and the rest) as prebuilt objects in the toolchain, built once at
     packaging or `zell target add` time, so a build only links them.
   - Cache generic instances keyed by function and type arguments (plus the
     toolchain version and codegen options), so `slices.Map<int>` and the like
     are reused across builds and programs.
   - Effect: LLVM sees only the program's own code. Both time and memory drop.
   - Size: several days. It needs a split of the stdlib into a shared part and
     an instantiated part, stable symbol names and cache keys, and care with
     reflection tables (emitted only for used types) and `@cfg` per target.
4. **Line table** (`lines.rs`, 343 KB for this program) is built by reading
   `.debug_line` after codegen. Its cost is a few percent, not hundreds of
   megabytes. Leave it unless profiles say otherwise.

## Related

- `docs/LANGUAGE_FEEDBACK.md` item 80 (build time and memory note from the
  kitchen-sink program).
- Release builds with LTO (`make -C runtime LTO=1`, `-flto=thin`) are not
  covered here.
