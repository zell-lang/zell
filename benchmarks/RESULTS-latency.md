# Latency, scaling and language mechanisms

Three Zell vs Go benchmarks for questions the throughput suite in
`README.md` does not answer: tail latency under allocation pressure (the
ARC-vs-GC pause argument), scaling over cores, and the cost of individual
language mechanisms.

**Status: measured on a quiet machine** (2026-10-22, commit 450e35f,
4 vCPU Intel Xeon @ 2.80 GHz with AVX-512 in a Firecracker VM, Linux 6.18,
clang/LLVM 18, go 1.24.7; nothing else running, load average < 0.5 before
each suite; `RUNS=5`, full size). Zell `--release` targets the host CPU
(the default; see RESULTS-framework.md open item 9), Go the default
`GOAMD64=v1`. The drivers report the programs' own timings, not process
wall/CPU/RSS.

## How to run

```sh
cargo build --release -p zell && make -C runtime
RUNS=5 ./scripts/bench-perf.sh latency_p99 cores_scaling defer_errors_iface
SCALE=small ./scripts/bench-perf.sh defer_errors_iface     # smoke test (~seconds)
benchmarks/latency_p99/bench.sh                             # one benchmark directly
```

`scripts/bench-perf.sh` now hands a benchmark that has an executable
`benchmarks/<b>/bench.sh` to that driver, passing `ZELL` and `RUNS`.
`SCALE` comes through the environment. Every driver builds `main.zl`
(`--release`) and `main.go`, runs them alternately `RUNS` times, fails if
the deterministic stdout of the two sides differs, and prints the best run.
Programs print checksums on stdout and timings on stderr. The timing lines
have the form `ms|us <row...> <value>`, and `scripts/bench-rows.py` turns
them into one row per measurement, with Zell/Go ratios. `SCALE=small`
shrinks every workload by about 100x and does not change the shape of a
benchmark.

For the quiet re-run: `RUNS=5`, nothing else running (`uptime` load < 0.5),
and the same machine for both languages. Go 1.24 runs with default `GOGC`
and `GOMAXPROCS`. Zell runs with default `ZELL_WORKERS` (online CPUs).

## latency_p99

An HTTP/1.1 server answers `GET /` with a 34-byte JSON body
(`{"message":"hello, world","id":1}`, `Content-Type: application/json`).
Each server is measured in two modes:

- **idle**: the server alone. This is the baseline.
- **churn**: the server plus one background task (Zell) or goroutine (Go)
  that keeps a large live heap and replaces it piece by piece as fast as
  it can. The heap has 1000 buckets. Each bucket holds a list of 3200
  objects (`{Id int, Name string, Prev *Obj}`, where the name is a small
  formatted string) and a map from id to every other object. One rebuild
  replaces the bucket an LCG picks, so 3200 objects, their strings and a
  1600-entry map become garbage (Go) or are freed at once (Zell). At full
  size the live heap is about 200 MB in Go (`HeapAlloc` after a GC: 203 MB)
  and 436 MB in Zell (`diag.rtHeapInUse`); see open item 4. The Zell task builds
  the new bucket in chunks of 400 objects and calls `await task.Yield()`
  after each chunk, because scheduling is cooperative (open item 6). The
  old bucket is freed when the new one replaces it in the slot. Go's
  goroutine builds the bucket in one go and is preempted by the runtime. Every second, the server prints its rebuild
  rate (`churn/s`) to stderr. The driver reports the average rate, so it
  is visible whether both sides really churned at a comparable rate.

The load generator is Go's, taken from `http_hello` (`main.go -load`): 64
keep-alive connections, 200k requests in all, one request in flight per
connection, after 100 warm-up requests. It records the latency of every
request (write to end of body) and reports req/s and p50/p90/p99/p999/max
in microseconds. Every body is checked byte for byte. The output
`requests N bytes B` must be equal for all four runs (zell/go x idle/churn).
The churn also has a deterministic self-check: `main` with no arguments
builds the heap, does 2000 rebuilds and prints object counts and
checksums, which must be equal on both sides (`bench.sh` checks this
first). `SCALE=small` uses 100 buckets of 150 objects and 20k requests.

Results (best of 5 by p99; µs; ratio rows Zell / Go, Go = 1.00: for
req/s and churn/s **> 1 is better**, for latencies < 1 is better):

| server | mode | req/s | p50 | p90 | p99 | p999 | max | churn/s |
|---|---|---|---|---|---|---|---|---|
| zell | idle | 77 602 | 764 | 1 303 | 2 210 | 4 154 | 12 123 | - |
| go | idle | 95 924 | 491 | 1 265 | 3 411 | 6 556 | 12 679 | - |
| zell | churn | 65 210 | 860 | 1 608 | 3 145 | 6 459 | 18 654 | 281 |
| go | churn | 52 539 | 643 | 3 323 | 8 142 | 12 217 | 16 760 | 1 208 |
| zell/go | idle | 0.81 | 1.56 | 1.03 | 0.65 | 0.63 | 0.96 | - |
| zell/go | churn | 1.24 | 1.34 | 0.48 | 0.39 | 0.53 | 1.11 | 0.23 |

Reading: idle, Go serves 24% more requests and has the lower median, Zell
the lower tail (p99 0.65x). Under churn Go's throughput drops by 45%
(95.9k -> 52.5k req/s) and its p99 grows 2.4x, Zell's throughput drops by
16% and its p99 grows 1.4x, so Zell is ahead on req/s (1.24x), p90, p99
and p999 under churn. But the churn rates are not comparable: Go's
background goroutine rebuilt 4.3x as many buckets per second (1208 vs
281), i.e. it produced 4.3x the garbage. Per unit of churn work Go's
interference is lower; per wall second of a "background job that runs as
fast as it can", Zell's request tail is better. Max latency is similar
(Zell 18.7 ms, Go 16.8 ms).

What to look at: how much churn moves p99, p999 and max away from each
server's idle baseline. The absolute numbers matter less. Go's concurrent
GC has no long stop-the-world pause, but mark assists and the GC's 25% CPU
share land on request goroutines. Zell frees a whole bucket synchronously
on the churn thread. That is about 3200 releases, a cost paid by that
thread and not by the request handlers, unless they share its worker
(open item 6).

## cores_scaling

The total work is fixed and split among 1, 2 or 4 workers: Zell tasks
(`async`, first `await task.Yield()` so they run on the pool) and Go
goroutines.

- **wordfreq**: word frequencies over 400 MB of generated text. The text
  is one shared read-only `[]byte` of words drawn with a skewed LCG choice
  from a 1000-word vocabulary. It is split at spaces into one range per
  worker. Each worker counts FNV-1a hashes of words into its own
  `map[int]int`, and the maps are merged at the end. This phase is CPU
  bound (byte scanning plus map updates) and shares no writable memory.
- **trees**: 2048 binary trees of depth 14 (32767 nodes each, 67M
  allocations in all) built and checked, divided among the workers. Each
  worker also keeps one long-lived tree. This phase is allocation heavy
  and exposes allocator contention (Zell: per-thread heap and refcount
  fast paths; Go: per-P mcache plus GC).

The output gives counts and checksums per worker count, and they must not
depend on the worker count. The driver prints the best ms per row and the
1->4 speedup for each language. `SCALE=small` uses 4 MB of text and 32
trees.

Results (ms, best of 5; Zell / Go, Go = 1.00, < 1 is better; for the
speedup rows > 1 is better):

| phase | workers | zell | go | zell/go |
|---|---|---|---|---|
| wordfreq | 1 | 1 948 | 2 151 | 0.91 |
| wordfreq | 2 | 1 014 | 1 101 | 0.92 |
| wordfreq | 4 | 532 | 535 | 0.99 |
| trees | 1 | 4 770 | 2 384 | 2.00 |
| trees | 2 | 2 526 | 2 075 | 1.22 |
| trees | 4 | 1 319 | 1 504 | 0.88 |
| speedup 1->4 | wordfreq | 3.66x | 4.02x | 0.91 |
| speedup 1->4 | trees | 3.62x | 1.59x | 2.28 |

wordfreq scales nearly linearly in both. For trees, Go's single worker is
helped by the GC running on the other 3 cores (one goroutine, but
`GOMAXPROCS=4`), so Go's 1-worker number is not a 1-core number and its
speedup flattens; Zell frees synchronously on the worker, is 2x slower
with one task, and scales 3.6x to beat Go at 4 workers.

With 4 vCPU and a 4-worker run, any other load on the machine shows up
directly in the 4-worker numbers. That is the main reason this table must
come from a quiet run.

## defer_errors_iface

> **Update 2026-10-23** ("Performance wave 2" in [README.md](README.md),
> 16-core machine): with the inline refcount fast path and borrowed struct
> temporaries, `iface_call` is 1.39x Go, `closure_call` 1.27x, `iface_sum`
> 1.24x (open item 2 largely closed); `defer_call` 6.8x and `error_sentinel`
> 5.8x remain (open items 1 and 3).

Each mechanism gets its own row, and each row is a loop whose result feeds
a checksum (µs, best of `RUNS`). `bench.sh` builds the Zell program twice:
with error return traces (the default) and with `--no-error-trace`.

| row | iterations | what |
|---|---|---|
| `direct_call` | 100M | method calls on concrete structs (two types, alternating) |
| `iface_call` | 100M | the same methods through an interface: `ops[i & 1].Apply(i)`, two implementations, one call site |
| `closure_call` | 100M | closures capturing a local, called through a `[]func(int) int` |
| `defer_call` | 20M | a function with `defer c.Inc()`, called in a loop |
| `no_defer_call` | 20M | the same function without `defer` |
| `error_new` | 20M | 1 in 8 calls fails: `error("odd")` created at the bottom of 3 levels of `?` (Go: `errors.New` + `if err != nil { return }`) |
| `error_sentinel` | 20M | the same with a constant error (`const ErrOdd = error(...)`; Go: a package `var`) |
| `generic_sum` | 100M adds | `SumG<T: Add>(xs []T)` over `[]int` (1M elements, 100 passes; Go: `SumG[T ~int\|~float64]`) |
| `iface_sum` | 100M calls | `SumI(xs []Valuer)` over boxed `*Val`, the interface-based equivalent |

Results (µs, best of 5; Go = 1.00 in the ratio columns, < 1 is better):

| row | zell | zell --no-error-trace | go | zell/go | notrace/go |
|---|---|---|---|---|---|
| direct_call | 14 890 | 14 828 | 85 525 | 0.17 | 0.17 |
| iface_call | 627 507 | 612 248 | 161 928 | 3.88 | 3.78 |
| closure_call | 567 193 | 563 917 | 187 465 | 3.03 | 3.01 |
| defer_call | 852 201 | 944 735 | 66 943 | 12.73 | 14.11 |
| no_defer_call | 998 | 997 | 43 242 | 0.02 | 0.02 |
| error_new | 168 782 | 98 190 | 120 995 | 1.39 | 0.81 |
| error_sentinel | 207 046 | 42 929 | 36 440 | 5.68 | 1.18 |
| generic_sum | 27 559 | 25 802 | 50 423 | 0.55 | 0.51 |
| iface_sum | 744 186 | 770 541 | 296 792 | 2.51 | 2.60 |

The open items below are confirmed: `defer` is 12.7x Go (43 ns per
deferred call vs 3.3 ns; item 1), interface and closure calls 3-4x
(the out-of-line retain/release of the receiver, item 2), and a sentinel
error with traces 5.7x but 1.18x without them (item 3). Error traces
cost about 28 ns per failing call on `error_new` (2.5M failures; 1.72x
the no-trace time).

How to read the rows: LLVM vectorizes the `direct_call`, `no_defer_call`
and `generic_sum` loops in Zell (`llvm.vector.reduce.add` in the IR), and
Go does not. Those rows therefore measure "the call disappears entirely"
and not the cost of a call. Compare `iface_call` and `closure_call` with
Go's equivalents, not with `direct_call`. Go has no error traces, so the
fair comparisons are `zell --no-error-trace` vs Go for the cost of the
mechanism, and `zell` vs `zell --no-error-trace` for the cost of traces.

## Open items (found while writing; for the compiler/runtime owners)

These come from the IR (`zell build --emit=llvm-ir`) and the heap counters.
The quiet run (tables above) confirms items 1-3.

1. **`defer` allocates on every execution.** In `withDefer`, `defer
   c.Inc()` becomes two `zell_object_new` per call (a bound-method value
   capturing `c`, type `ti.24`, and a closure env `ti.env`), plus 3
   retain/release pairs and 2 frees. Go open-codes such defers to a few
   instructions. A defer of a direct call with already-evaluated arguments
   could store the receiver and arguments in the frame, with no closure
   (Go-style open-coded defers). The smoke run hinted at >10x Go.
   **Done (2026-10-23):** defers outside loops (at most 8 per function)
   are open-coded (DESIGN.md "Память"; `ZELL_NO_OPEN_DEFER=1` restores
   the closure path). `defer_call` went from 365 883 µs (6.65x Go) to
   18 438 µs (0.34x Go) on the same machine and run; the other rows did
   not move.
2. **Retain/release of a borrowed receiver around interface and closure
   calls.** `ops[i & 1].Apply(i)` emits `zell_ptr_retain(recv)`,
   `call vtable[..]`, `zell_ptr_release(recv)` per call, both out of line.
   The same happens for `fs[i & 1](i)` (the closure env) and for `for x in
   xs { x.Value() }` over `[]Valuer`. The slice keeps the element alive for
   the whole call. The callee could also be handed a borrowed receiver (it
   is `readonly` already) when the element cannot be overwritten during the
   call (no stores to the slice, or a callee without side effects on it).
   The smoke run hinted at about 4x Go for `iface_call` and `iface_sum`.
3. **Sentinel errors with traces** (largely closed). `return ErrOdd` used
   to copy the shared constant on the first `return` (copy-on-write), so
   every failing call allocated. The trace of a shared error now lives in
   a thread-local pending record while it propagates and is attached only
   where the propagation ends and the handler can observe it (DESIGN.md
   "Трасса ошибки", ABI.md 7.4 `zell_error_caught`): `error_sentinel`
   went from 5.7x to 2.6x Go (`error_new` 1.9x to 1.55x) with no
   allocation on the sentinel path. What remains is the four runtime
   calls per failure (`created`, two `?`, the catch), about 4 ns each
   against Go's 9 ns per failure; closing the rest means fewer or inlined
   calls (for example folding the `return Sentinel` site into the first
   `?`).
4. **Memory per small object is about 2x Go's.** The same 3.2M-object
   heap (latency_p99 self-check, full size) has Zell `diag.rtHeapInUse` at
   436 MB and Go `HeapAlloc` after a GC at 203 MB. Peak RSS is 814 MB vs
   434 MB (Go's includes the GOGC=100 headroom, Zell's has no such
   reason). That is about 136 B vs 63 B per object, counting its string,
   list slot and half a map entry. Zell's RSS is also 1.9x its own heap in
   use after the build. Worth a `diag.HeapByType()` breakdown: object
   header size, small-string layout (`zell_string` is 32 bytes + bytes),
   map entry size, and pages retained from `append` growth.
5. **Self-referencing types are cycle-collector candidates.** `Obj.Prev
   *Obj` (latency_p99) and `Node.Left/Right` (cores_scaling) trigger
   W0001. The objects form lists and trees, never cycles, but the types
   could form them, so the bounded cycle collector tracks them. The
   benchmarks keep them this way on purpose, since realistic code looks
   like this. A quiet run with `ZELL_CYCLES=off` (as in `alloc_heavy`)
   gives the cost of that tracking under churn and across threads.
6. **Cooperative scheduling and CPU-bound tasks.** The churn task has to
   `await task.Yield()` every 400 objects. A bucket rebuild without yields
   (3200 objects) keeps its worker thread for about 2 ms (scaled from the
   smoke run's rebuild rate). A request task queued on that worker waits
   that long, because Zell has no preemption (Go has async preemption
   since 1.14). With the chunks, the wait is a chunk, plus the synchronous
   release of the old bucket (3200 objects + map). On a quiet run, check
   whether work stealing moves queued tasks off a busy worker fast enough
   to keep this out of p999. A "CPU-bound/blocking task" API
   (`task.SpawnBlocking` or worker affinity) would let programs keep
   background CPU work off the I/O workers, and freeing a large structure
   could be deferred in pieces (a release queue drained between tasks).
7. **No per-program worker count.** cores_scaling varies the number of
   tasks while the pool stays at `ZELL_WORKERS` (default 4), as with Go
   goroutines under `GOMAXPROCS`. A run with `ZELL_WORKERS=1` /
   `GOMAXPROCS=1` and 4 workers would separate "parallel speedup" from
   "task overhead". The drivers do not do that yet.
