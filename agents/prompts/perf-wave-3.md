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

### 3. Open-coded `defer` (45-60 min, compiler `hir/` + `codegen/`)
`defer_call` 6.8x Go: every `defer c.Inc()` allocates a bound-method value
and a closure env, 3 retain/release pairs, 2 frees. For defers not inside
loops, keep the receiver and evaluated arguments in a frame slot and call
directly at exit (Go's open-coded defers); loop defers keep the heap path.
Spec test in `tests/spec/memory2`, DESIGN.md "Память" note. Target <= 2x.

### 4. Sentinel errors with traces (30-45 min, `runtime/src/error.c`, codegen `?`)
`error_sentinel` 5.8x Go, 1.0x with `--no-error-trace`: `return ErrOdd`
copies the constant on the first `return` (copy-on-write of the shared
trace). Keep the trace beside the value in the `T!` slot, or allocate once
per propagation path with no copy on later `?`. Target <= 1.5x.

### 5. LTO between program and runtime (45 min, `runtime/Makefile`, `compiler/src/build.rs`)
`Value` lookups 3.3x Go are one runtime call per `Get`/`Index`/`Int`; the
same applies to string/slice helpers everywhere. Build `libzell_rt.a` as
LLVM bitcode (`-flto=thin`) behind a Makefile switch, pass the matching
linker flag in `--release`, keep the plain archive as default until the
cross builds (mingw, musl, arm64, darwin stub) are verified. Also: `rtGet`
should compare key length and first byte before `memcmp`. Measure
`json_codec` lookups and `strings`/`map` for regressions.

### 6. `log` allocations (45 min, `std/log/log.zl`)
344 allocations per JSON record (zap 1, zerolog 0): `quote` takes a one-byte
substring per input byte, `clef` re-quotes the constant template and
re-converts the time layout per record, `emit` builds an `Event`, a map and
a keys slice before any sink runs. Target <= 5 allocations per record,
`logging/run.sh` enabled path >= 0.5x zap. `web`'s `UseLogging` benefits.

### 7. `data` insert path (30-45 min, `std/data/data.zl`, `data_save.zl`)
`entryOf` scans every tracked entry (quadratic `AddRange`); `batchSize()` is
a constant 100. A pointer->entry map in `ctxState`; batch size from the
provider's parameter limit (SQLite 32766, PostgreSQL 65535) divided by the
column count. Target: insert <= 1.5x `database/sql` (now 7.8x), GORM slower
than Zell on every phase.

### 8. Crypto, second pass (45-60 min each, `runtime/src/crypto/`)
Ed25519 sign 8.2x (public key re-derived per signature, no fixed-base
table), P-256 6.5x/5.3x (fixed/variable-base tables, 64-bit limbs), ChaCha20
7.8x (AVX2, 8 blocks), SHA-512 1.8x, Argon2id 3.9x (AVX2 BLAKE2b). Same
dispatch scheme as SHA-NI/VAES (`target(...)` functions, cpuid once,
`ZELL_NO_*` overrides, portable fallback unchanged, vectors on every path).
TLS 1.3 handshakes (2.2x) follow from the first two.

### 9. Scheduler leftovers (30-45 min, `runtime/src/task.c`)
`await_chain` resume phase ~75 ns per frame (run-next dispatch +
`task_complete` atomics), `fanout_fanin` pool phase 2.3x (futex wake churn
of 50 us spinners). Profile with `-DZELL_SCHED_STATS`, then decide.

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
