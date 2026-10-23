# Plan: performance wave 3 (after the 2026-10-23 wave)

State on master `b0221fb` (see "Performance wave 2" in benchmarks/README.md):
SHA-256/AES-GCM at Go parity or better, `encoding.json` 3-6x faster than Go
except `Value` lookups, channels/mutex 1.8-2.6x Go (were 27-30x), interface
and closure calls 1.2-1.4x, async call without suspension 3 ns. What is left
is listed here in priority order, each item sized for one agent run.

## Working rules (the user insists on these)

- One agent at a time, in its own worktree, 30-60 minutes per item, a short
  report between items; agree the item before launching.
- Every command under `timeout 120`; a benchmark that does not finish in
  2 minutes is killed and reported as such, never waited for. `RUNS=1`,
  the whole re-measure under 10 minutes.
- Output filtered to verdict lines (`| tail -1`, `grep -E 'ok|FAIL|result'`);
  no full `cargo test` inside an agent — targeted tests only (`SPEC_FILTER`,
  5-7 examples under `--leak-check`); the full suite once before a merge,
  in the background, output filtered.
- Mechanical steps (running benchmarks, filling tables) go to a cheaper model.
- Build the runtime with `CC=clang` on GCC 15 hosts; unset `HTTP_PROXY`/
  `HTTPS_PROXY` (or `NO_PROXY=localhost,127.0.0.1`) for anything touching
  loopback, otherwise http/tls examples fail with 451 or hang.
- Known non-regressions when reading test results: the `--static` package
  test needs a static glibc; `examples/cycle_server` is flaky on many-core
  machines (item 1 below).

## Items

### 1. `examples/cycle_server` flaky — DONE 2026-10-23
The half-vs-half comparison of sampled live-object peaks depended on when
the automatic collections landed; the example now forces one collection at
the end and checks that the live count drops to the server's own objects
(`cycles reclaimed: true`), which holds on 1 and 16 workers.
Finding worth a follow-up (collector pacing, `runtime/src/cycles.c`): over
10k requests leaving 2 candidate roots each, auto mode ran only 4-5
collections with `g_threshold` at its minimum 1024 — about one per 2000
requests instead of one per 512 — on 1, 4 and 16 workers alike; the live
peak was 20-27k objects against 49k with the collector off. The request
to collect (`g_pub_req` in `record_count`) seems to be honoured late;
check where `maybe_collect` is actually reached on a worker thread.

### 2. `timers` lateness regression — DONE 2026-10-23
The timer thread already took the due timers from the heap in one batch,
but posted each continuation to the global queue separately (one malloc,
one `q_mu` round trip and one wake per timer, contended by the workers
pulling from the same queue). Posts from a batching thread are now
collected (`t_post_batch`) and pushed in one lock round trip
(`post_batch_flush`). 100k timers of 1-10 ms: mean lateness 15 699 us ->
38 us (Go 102), max 36.8 ms -> 0.63 ms (Go 1.9), wake-ups over 1 ms late
88 682 -> 0 (Go 1 237); wall 1.53x -> 1.04x Go. Left: the one-wake-then-hand-on
protocol means a big batch ramps the idle workers up one by one; waking
`min(n, idle)` at once would need the single-spinner token reworked.

**TSan hang in the channel tests — FIXED 2026-10-23 (`7093729`).** Not a
lost wake-up: every thread blocking in `zell_task_await` waited on one
process-wide condvar and `task_complete` broadcast to all of them; with
10k producers blocking on a full channel (~1000 spare worker threads)
each receive woke ~1000 threads that found their task still pending — a
thundering herd TSan stretched into an apparent hang. Each waiter now has
its own condvar linked on the task (invariant documented at `awaiter`);
regression test `await_wakes_one_thread`. Left: one spare worker per
blocking await (up to MAX_WORKERS=1024 threads) is what made the herd
possible; cap spares or park blocked senders differently.

### 3. Open-coded `defer` — DONE 2026-10-23 (45-60 min, compiler `hir/` + `codegen/`)
Go's rule (all defers outside loops, <= 8): receiver and arguments in frame
slots, direct calls at every exit; `ZELL_NO_OPEN_DEFER=1`. defer_call 6.65x
-> **0.34x Go**. Spec test memory2/open_defer. Left: functions with a
loop defer keep the closure path entirely; panic still runs no defers.

### 4. Sentinel errors with traces — DONE 2026-10-23 (30-45 min, `runtime/src/error.c`, codegen `?`)
Shared errors are never copied while propagating: a thread-local pending
record (8 deep, 24 frames) collects the sites; `try/catch` attaches it only
when the handler can observe the trace (static check), else drops it with
zero allocations. error_sentinel 5.5x -> **2.5x**, error_new 1.95x -> 1.56x.
Left: the remaining ~16 ns are four out-of-line runtime calls per failure;
an inline fast path needs the record layout in the ABI.

### 5. LTO between program and runtime — DONE 2026-10-23 (45 min, `runtime/Makefile`, `compiler/src/build.rs`)
The lookup cost was `rt_index` building a 1M-entry offset table lazily
(~1M dependent cache misses); the parser now builds it when an array
closes. Lookups 143 -> 65 ms (Go 43, 1.5x); with the new opt-in ThinLTO
(`make LTO=1`, `ZELL_LTO=1 zell build --release`) 46 ms (~1.1x). Left: per-
`Value` call overhead (`math.IsNaN`/`Round` as externs, retain/release of
`d`) — compiler intrinsics / ownership elision.

### 6. `log` allocations — DONE 2026-10-23 (45 min, `std/log/log.zl`)
344 -> 10 allocations per record, enabled path 3.9x faster: 344k rec/s
(zap 912k, zerolog 1.41M, slog 749k) = 0.38x zap devnull / 0.44x file;
disabled path still 0 allocations. 6 of the remaining 10 are in the
compiler-lowered call (`sema/logtmpl.rs`: three constant `[]string` slice
literals allocated per call, a box per string argument); 4 in log (Event,
extra props, joined line, float text). Next: constant slices in the
lowering (-3), string-inline `any` (-1/string), a C-side line builder.

### 7. `data` insert path — DONE 2026-10-23 (30-45 min, `std/data/data.zl`, `data_save.zl`)
Pointer-keyed entry map (`unsafe.AddrOf` of the box payload), batch size
= provider parameter limit / columns, rows materialised without the
`[][]any` copy, `First` with LIMIT 1. insert 476 -> 246 ms (sql 102,
2.4x), lookup 4565 -> 3435 (sql 1924, 1.8x); GORM slower than Zell on
every phase but insert. Left: per-shape SQL text + arg-plan cache,
`e.values()` computed twice, closures per tracked entity; side finding: a
plain `database.sql` loop measured ~2 ms per query in a scratch program
(scratchpad floor.zl) — verify, may be a driver/scheduler issue.

### 8. Crypto, second pass (45-60 min each, `runtime/src/crypto/`)
Ed25519 sign 8.2x (public key re-derived per signature, no fixed-base
table), P-256 6.5x/5.3x (fixed/variable-base tables, 64-bit limbs), ChaCha20
7.8x (AVX2, 8 blocks), SHA-512 1.8x, Argon2id 3.9x (AVX2 BLAKE2b). Same
dispatch scheme as SHA-NI/VAES (`target(...)` functions, cpuid once,
`ZELL_NO_*` overrides, portable fallback unchanged, vectors on every path).
TLS 1.3 handshakes (2.2x) follow from the first two.

### 9. Scheduler leftovers — DONE 2026-10-23 (30-45 min, `runtime/src/task.c`)
Spare workers bounded at max(16, 4N) plus one per 10 ms of complete stall
(ABI.md 10.1/10.5): 10k blocked C-API producers -> 19 threads. Compiled
Zell code never blocks a worker (await outside async is a sema error).
Eight atomics cut from suspend/resume with no wall change: await_chain's
resume phase creates and completes 1M tasks where Go makes 1M plain
calls; reaching <= 6x needs no task per level (or a task freelist / smaller
task), measured with ZELL_WORKERS=1 (at 16 workers only ~26% of frames
really suspend).

### 10. I/O and strings (45 min, `std/io`, `std/strings`, `runtime/src/std_strings.c`)
Buffered read 1 KB 3.6x, line scan 2.6x, buffered write 2.0x,
`strings.Builder` 5.4x (keeps every piece), string sort 5x, linked list
6.8x (cycle-collector candidate tracking — measure with `ZELL_CYCLES=off`
first). Copy straight from the buffer into the caller's slice, `memchr` line
scanning, one growing buffer in `Builder`.

### Later, needs a spec decision first
`select` over channels/timers (DESIGN.md "Асинхронность" + grammar);
escape analysis / stack allocation of non-escaping objects; ownership
elision and reuse (Perceus-style last-use moves, DESIGN.md "Память");
single-implementation devirtualisation of interface calls; preemption or a
`SpawnBlocking` API; smaller object header (~2x Go memory per small object).

## Suggested order
1 -> 2 -> 3 -> 4 (cheap, visible in `defer_errors_iface` and CI), then 6 and
7 if the target is HTTP services, else 5 and 8.

## JSON: reaching System.Text.Json speed (plan, not started)

Reference point to establish first: a .NET side in `benchmarks/json_codec`
(System.Text.Json with source generators; the SDK is on the dev machine).
Public figures put STJ at roughly 300-600 MB/s typed deserialisation and
1-2 GB/s serialisation; Zell today is 277 MB/s typed parse, 584 MB/s typed
format, 619 MB/s Value parse (100 MB document, 16-core Zen 4). Steps in
order of expected gain, each one agent run (`runtime/src/std_json.c`,
`compiler/src/sema/reflect_src/json.zl`, the decoder generator):

1. One pass instead of two: `Parse<T>` currently builds the arena document
   and then walks it. Feed the generated decoder from the scanner token by
   token (SAX style); build the document only for `ParseValue`. Expected
   -30-40% on typed parse.
2. Field-name matching as STJ does it: names are compile-time constants, so
   compare as 8-byte words / perfect hash on length and first bytes, and
   try the field expected next in declaration order first (real documents
   almost always match, the search becomes one compare).
3. Numbers: SWAR parse of up to 8 digits for ints, Eisel-Lemire for floats
   (today Clinger fast path + `strtod` fallback, and `strtod` is slow).
4. Strings: UTF-8 validation and quote/escape search in one SIMD pass (SSE2
   baseline, AVX2 behind the cpuid dispatch); allocate string fields from a
   per-parse slab instead of the general allocator (~60 instructions per
   object today).
5. Writer: two-digit itoa tables, check that `zell__format_float` is
   Ryu/Dragonbox class (replace if not), SIMD escape scan, "no escape -> one
   memcpy".
6. simdjson stage-1 structural indexing on AVX2 — only after 1-5; ~1.5x more
   on parse for ~500 lines plus a dispatcher.
Also still open from wave 2: `Value` lookups 1.5x Go (1.07x with
`ZELL_LTO=1`), per-`Value` extern calls (`math.IsNaN`/`Round`).
