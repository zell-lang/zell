# Async performance plan (2026-10-24)

What the task runtime (`runtime/src/task.c`, `task_sync.c`, `task_io.c`) and
the coroutine lowering (`compiler/src/codegen/async_.rs`) must change to
close the gap to Go. Every claim below comes from a measurement on master
`b933053` (16-core Ryzen 7 8845HS, Fedora 42, clang/LLVM 18, go 1.24.7,
`--release`, one run per side, machine load ~1.5): `perf stat`, `perf record
-g` on `-g` builds, a scratch runtime built with `-DZELL_SCHED_STATS` plus
object/frame counters, an `LD_PRELOAD` malloc counter, and micro programs
(1000-deep chains, 1M ping-pongs, a mutex with a yield inside the critical
section) run with `ZELL_WORKERS=1` so that every operation is counted exactly.

## Where the time goes

Zell / Go with Go = 1.00 (lower is better, except round trips/s). "CPU" is
user+sys of the whole process. "Profile" is the share of all samples.

| bench | phase: Zell / Go (ratio) | CPU Zell / Go | profile (top, % of samples) | structural cause |
|---|---|---|---|---|
| await_chain | sync 130 / 10 ms (13x); yield 96 / 7 ms (14x) | 0.38 / 0.02 s | body 15.5, `worker_main` (idle polling) 13.0, `zell_release` 9.7, `zell_coro_frame_alloc` 8.4, `_free` 6.9, memset (frame zeroing) 3.1, `zell_task_get_ctx` + `zell_coro_leave` 4.4, kernel 18.6 (`sched_yield` 9) | sync: one zeroed heap frame per level; yield: **one task object + one run-next job per resumed frame** (lazy 145, complete 92, drop 68, add_cont 57, find_work 31, job tail 35 cycles per frame: 430 cycles) |
| channel_pingpong | ping-pong 619 / 447 (1.38); pipeline 149 / 74 (2.0) | 2.48 / 0.64 s | kernel 54 (`sched_yield` 41.5 incl.), `worker_main` 7.8, `cancel_hook_on` 7.5 incl., `clock_gettime` 6.4, `zell_release` 6.5, malloc+free 4.3, `chan_recv`/`chan_send` 5.5 | idle workers burning CPU next to a one-worker hand-off chain; 2 libc mallocs (cancel hooks) + 1 task object per wait |
| mutex_contention | contended 624 / 210 (3.0); uncontended 71 / 34 (2.1) | 1.36 / 0.71 s | `worker_main` 15.2, `zell_release` 15.0, kernel 36 (`sched_yield` 26), `task_complete_ex`/`add_cont`/`zell_object_new`/`task_drop` 10.5 | FIFO ownership hand-off (DESIGN.md) makes **every** contended Lock a suspension + cross-thread resume (9.99M run-next jobs for 10M locks); uncontended: two runtime calls + `zell_task_is_done` call per Lock/Unlock |
| fanout_fanin | fan-in 606 / 306 (2.0); pool 251 / 76 (3.3) | 6.89 / 1.58 s | kernel 71 (`sched_yield` 30, futex wait 15), `cancel_hook_on` 6.3 incl., malloc+free 4.8 | idle spinning + parking churn (352k park rechecks, 856k steals, 1.05M run-next kicks); hooks per wait |
| spawn_tasks | run 937 / 146 (6.4); park 285 / 1084 (**0.26**); wake 469 / 122 (3.8) | 4.28 / 5.36 s | kernel 73 (futex wait 30, futex hash spin lock 6), `worker_main` 5.3 | **`async func main` runs on the main OS thread**: its 1M `Yield` posts go to the global queue (1.56M malloc'ed jobs, `q_mu`, a wake per post, 274k parks) |
| select_many | `task.Any` 1115 / 848 (1.31); merge 1006 / 402 (2.5) | **23.2 / 2.2 s** | kernel 73 (`sched_yield` 47), `cancel_hook_on` 4.1, `clock_gettime` 4.0 | idle spin/linger (734k rechecks, 2.2M steals, 6.2M run-next kicks, 17 Gcycles in `find_work`); hooks per wait |
| async_io | 643k / 805k round trips/s (0.80); p99 6.2 / 2.4 ms | 24.1 / 18.7 s | `send` path 60 (loopback TCP, same in Go), `recv` 12, `io_main`/`epoll_wait` 10.7, `schedule` 6.4, `try_to_wake_up` 5.3 | **650k context switches vs 2.7k**: dedicated poller threads sleep in `epoll_wait` and every readiness wakes one, then posts through the global queue |

Worker-count sweep (Zell `ZELL_WORKERS`, Go `GOMAXPROCS`; ms):

| bench phase | Zell 1 / 4 / 16 | Go 1 / 4 / 16 |
|---|---|---|
| ping-pong | 477 / 566 / 631 | 449 / 431 / 423 |
| mutex contended | 72 / 574 / 642 | 36 / 201 / 208 |
| fan-in | 90 / 757 / 812 | 35 / 311 / 318 |
| spawn run 1M | 462 / 946 / 937 | 585 / 147 / 146 |

The per-operation paths are close to Go on one worker (ping-pong 1.06x):
the channel already hands the value to the specific waiter and runs it next
(`recv_finish`, `task_sync.c:217`, run-next slot). The losses are (1) idle
workers interfering with the busy one, (2) allocations per wait, (3) a task
per resumed frame, (4) main off the pool, (5) the poller threads.

Surprises:
- `sched_yield` is the single largest symbol in five of the seven benchmarks
  (26-47% of samples). More than 90% of those samples come from
  `worker_main` (the inlined `spin_for_work`/`lingering_runnext` loops,
  `task.c:328`, `:414`), not from the channel or task spin locks.
- Moving `main` onto a worker (a scratch copy of `spawn_tasks` with `await
  task.Yield()` as its first line) cuts run 1M 937 -> 374 ms and wake 1M
  518 -> 180 ms; nothing else in this plan is as cheap for its gain.
- The panic-unwind local pinning (`funcs.rs:133`, `coro_escape_state`) costs
  nothing measurable: `await_chain` compiles to an identical binary with it
  disabled (its frames own no values), and ping-pong / mutex differ by
  less than the run-to-run noise (474-486 vs 498-506 ms, 70-73 vs 71-76 ms;
  the pinned build was not slower). It is not a work item.
- Shortening the idle spin (50 -> 5 us) and the run-next grace (20 -> 3 us),
  or letting parking workers skip run-next slots, did **not** help: ping-pong
  got slower (616 -> 810 ms) and `select_many` starved (timeout). Lingering is
  the only cross-worker pickup of a run-next job, so the idle protocol needs
  a redesign (item R3), not new constants.

## The model gap: one `await` that suspends

Zell today, for `x = await g()` where `g` suspends once (file:line on master):

1. Caller ramp: `zell_task_get_ctx` (call, retain of `t_ctx`), frame from
   `zell_coro_frame_alloc` (`mem.c:143`, zeroed size-class block), push on
   `zell_coro_chain` (`async_.rs:79-157`).
2. `g`'s inner await: `zell_task_is_done` (call); `zell_task_lazy`
   (`task.c:1890`): new task object (header + slot, `ZELL_RC_NOBIAS` so every
   later retain/release is an atomic RMW), `task_attach` takes the parent's
   spin lock and retains the parent; `t_cur` swap.
3. `zell_task_add_continuation` (`task.c:1114`): task spin lock, retain of
   `t_ctx`, move of `t_cur` into `cont0`, restore of the caller context.
4. The caller sees `res.task != NULL`, runs its own `zell_task_lazy` (its
   own task, if it has none yet) and `add_continuation` on `g`'s task: steps
   2-3 again, one level up. A chain of N directly awaited calls makes N tasks.
5. Completion of the leaf: `task_complete_ex` (`task.c:1029`): retain, lock,
   result copy + `ops->retain`, release-store of the state, awaiter list,
   `restore_parent`, release; `sched_post_ctx(..., next=true)` ->
   `runnext_put` (seq-cst store) + `wake_one` (two seq-cst loads, maybe
   `q_mu` + futex).
6. The worker: `find_work` -> `runnext_take` (CAS + store), `run_job`
   (`task.c:435`: TLS swaps, `setjmp` unwind point, release of the job's ctx
   and cur), `zell_coro_resume`, `zell_task_result` (call), `zell_release`
   of `g`'s task -> `task_drop` (`task.c:894`: parent lock to unlink, release
   of parent, cont ctx/cur, hooks).
7. A channel wait adds `recv_task_new` (a task object even when a value is
   buffered, `task_sync.c:118`, `:276`), queue retains of task and channel, and
   `wait_hooks` (`task_sync.c:211`): two `cancel_hook_on` (`task.c:1753`), each
   a libc `malloc`, a task lock and a retain; freed in `task_drop`.

Counted per operation (`ZELL_WORKERS=1` micro programs): a 1000-deep chain
that suspends at the leaf makes **1 task object + 1 frame + 1 run-next job per
level**; a synchronous chain 1 frame per level and nothing else; a
ping-pong round trip **2 task objects + 4 libc mallocs + 2 jobs**; a
contended `Lock` 1 task object + 1 job (no hooks: Lock is not cancellable);
`task.Yield` 1 task object + 2 jobs (the yield job, then the continuation).

Go, for the same operations: a call that blocks parks the whole goroutine
stack (`gopark`), no allocation per call level; a channel receive puts a
`sudog` from the per-P cache on the wait queue, the sender copies the value
straight into the receiver's stack slot and `goready` puts it in `runnext`;
`Gosched` is one `globrunqput`; idle Ms spin briefly (at most GOMAXPROCS/2
spinning, a few microseconds) and then `futexsleep`; the netpoller is polled
non-blockingly by idle Ps (`findrunnable`) and blocked on by at most one M,
so readiness does not wake a thread while Ps are busy (2.7k context switches
for 1M loopback round trips).

## Work items

Expected gains are derived from the profile shares and counts above;
"moves" lists the benchmarks whose phase should change.

### R1. Task-free `task.Yield` (runtime)
- Change: `zell_task_yield` (`task.c:1704`) returns an immortal "yield"
  task; `add_cont` (`task.c:1114`) recognises it, posts `(fn, frame)` with
  the caller's ctx/cur to the **ring tail** (never run-next) and returns true;
  `zell_task_is_done` on it is false. ABI 10.2 row for `zell_task_yield`.
- Keeps: Yield still yields (ring tail, behind queued work); cancellation (a
  cancelled task's Yield resumes as before); the C API (a blocking
  `zell_task_await` of it must still work: treat it as a fresh plain task there).
- Moves: spawn_tasks run (1M yields: -1 object, -1 job, -1 complete each, ~60 ns
  of the ~370 ns per task after C1), await_chain yield (1 per chain), mutex.
- Expected: spawn run 1M -15%. Risk low. Size 2 h.

### R2. Allocation-free channel waits (runtime)
- Change: embed both cancellation hook nodes in `chan_wait` (the task slot,
  `task_sync.c:22`) and link them intrusively (`hook.weak` already exists);
  replace the self hook (`zell__cancel_hook_self`) by a per-task
  `cancel_fn` field called by `zell_task_cancel`; free nothing in `task_drop`
  for embedded nodes. Unlink from the waiting task's hook list on completion
  (today `task_drop` does it).
- Keeps: cancellation of a queued wait from both sides (`chan_wait_cancelled`
  idempotent), `task.Any` losers, `--leak-check` (no hook outlives its task).
- Moves: channel_pingpong (`cancel_hook_on` 7.5% + malloc/free 4.3%),
  fanout_fanin (6.3% + 4.8%), select_many (4.1% + 2.8%).
- Expected: ping-pong and pipeline -10%, fan-in -8%, select -6%. Risk medium
  (hook list races with `cancel_collect`). Size 4-5 h.

### R3. Idle-worker protocol without `sched_yield` storms (runtime)
- Change (`task.c:292-433`, `worker_main` `:468`): (a) only the spinning-token
  holder may watch run-next slots; a worker that finds work only in another
  worker's run-next while parking parks with a timed wait (`zell__plat_cond_wait_ns`,
  20 us) instead of `lingering_runnext`'s yield loop, so at most one thread
  watches; (b) spin with `pause` backoff, `sched_yield` only when more
  threads than cores are alive (spares); read the clock every 256 polls;
  (c) the spinner reads peers' `rn_state` lines once per pass, not per poll
  (each read pulls the busy owner's line, written on every hand-off);
  (d) cap spinners at max(1, busy/2) like Go and give the token up after one
  full steal pass with nothing found.
- Keeps: no lost wake-up (store-then-load protocol of ABI 10.1), the
  never-yielding job cannot starve its run-next (`relay_handoffs`,
  `timer_heap_order_and_fire`, `await_wakes_one_thread` in
  `runtime/tests/test_task.c`, plus a new test with a job that loops 10 ms
  after posting to run-next while 15 workers park).
- Moves: CPU of every hand-off benchmark (`sched_yield` 26-47% of samples);
  wall of ping-pong (16-worker 631 vs 1-worker 477 ms), select_many, fanout.
- Expected: CPU -50% on select_many/fanout_fanin/channel_pingpong, ping-pong
  wall toward the 1-worker figure (-15%). Uncertain: the two constant
  experiments failed; measure each sub-change separately. Risk high
  (starvation, TSan). Size 6-8 h.

### R4. Netpoll inside the scheduler (runtime, epoll first)
- Change (`task_io.c`, `task.c` `find_work`/`worker_main`, `platform/poll_epoll*`):
  a worker out of local work calls `epoll_wait(shard, 0)` on the shards
  before stealing and pushes ready continuations into its own ring; the
  poller threads block in `epoll_wait` only while no worker is spinning
  (Go's "one M in netpoll"), and the last worker to park blocks in
  `epoll_wait` with the next timer deadline instead of on `q_cv`, woken by
  the shard eventfd. kqueue/io_uring/IOCP keep the dedicated threads for now
  (`zell__plat_poller_edge`-style capability flag). This is WebSocket fix 4.
- Keeps: ABI 10.9 contract (edge-triggered, stale-generation drop, forget),
  timeouts, `zell_runtime_shutdown` order, cross-platform rule (polling calls
  only through `platform/`).
- Moves: async_io (650k context switches, `io_main` 10.7% + `schedule`/
  `try_to_wake_up` 11.7%), websocket_echo, http_hello.
- Expected: async_io CPU -20%, round trips/s 0.80 -> >= 0.95x Go, p99 <= 1.3x
  Go, context switches <= 20k. Risk medium-high. Size 10-14 h.

### R5. Barging mutex with a starvation threshold (runtime + DESIGN.md)
- Needs a spec decision: DESIGN.md "Асинхронность" says `Unlock` hands
  ownership to the oldest waiter (FIFO). Go's rule instead: `Unlock` wakes
  the oldest waiter without handing over; a running task may take the freed
  mutex; a waiter that waited > 1 ms switches the mutex to FIFO hand-off
  until the queue drains. Plus spin-then-park in `zell_task_mutex_lock`
  (`task_sync.c:456`): up to ~100 CAS attempts while the owner is running.
- Keeps: no lost waiter (a woken waiter that loses the race re-queues at the
  head), TryLock invariant of the guard bit, no starvation (> 1 ms rule).
- Moves: mutex_contention contended. On one worker (no hand-offs) Zell does
  10M increments in 72 ms; with 16 workers FIFO forces 9.99M suspensions.
- Expected: contended 624 -> ~200 ms (1.0-1.3x Go). Without the spec change
  only the spin helps (est. -15%). Risk medium. Size 4-6 h.

### C1. `async func main` on a worker (codegen main entry + runtime)
- Change: `codegen/mod.rs:925-947` calls a new `zell_task_run_main(ramp, res)`
  that posts the ramp to the scheduler as a root task and blocks the main
  thread in `zell_task_await_main` (ABI 10.2/10.5 new row). Sync `main` unchanged.
- Keeps: exit code / `main() void!` error report, a panic in main ends the
  process with the same output (`panics` spec area), the debug agent's main
  task, `zell_runtime_shutdown` after main's task.
- Moves: spawn_tasks run 937 -> 374 ms and wake 518 -> 180 ms (measured),
  any program whose main does work before its first suspension.
- Risk low. Size 2-3 h.

### C2. Frame-linked await: no task per directly awaited call (codegen + ABI 10.3)
- Change: for `await g(args)` inside a coroutine the caller stores its frame
  handle in an extended slot `{ ptr task; T value; ptr awaiter; i32 state }`.
  At its first suspension `g` makes **no task** when `awaiter != NULL`: it
  keeps `(awaiter, res)` in its frame, sets `state = LINKED`, and suspends
  on its inner await under the caller's current task (`t_cur` unchanged,
  as before the first suspension today). The caller sees LINKED, does
  `coro.save` and CAS `LINKED -> SUSPENDED`; on failure (already DONE) it
  continues. At return `g` moves its result into `res.value`, restores
  ctx0, exchanges `state = DONE`, and if the previous state was SUSPENDED
  hands the awaiter to a thread-local "resume next" slot that `run_job`
  drains in a loop (no recursion, no queue, no `setjmp` per level). Callers
  that keep the `Task<T>` (`awaiter == NULL`) get a real task as today.
- Keeps: result ownership (+1 moved, once); the res slot lives in the
  suspended caller's frame; cancellation (waits hook on the root task, which
  is what `t_cur` is); context isolation (`zell_coro_leave` before resuming);
  panics: a linked frame's destroy path stores PANICKED + message/trace in
  `res` and resumes the awaiter, which re-raises (`Task<T>`) or yields the
  `task.ErrPanic` error (`T!`) exactly as `zell_task_result[_err]` does;
  `zell_coro_chain` pushes/pops unchanged; `--leak-check`; `diag`/debugger
  task lists show the root task only (document it).
- Moves: await_chain yield (430 -> ~80 cycles per frame: lazy, add_cont,
  complete, drop and the run-next trip go), websocket/http (~13 lazy tasks
  and continuations per round trip, `benchmarks/websocket_echo/PROFILE.md`).
- Expected: yield phase 96 -> <= 28 ms (<= 4x Go); websocket CPU per message
  -8%. Risk high (the hardest race of 10.3, every async spec test). Size 16-24 h.

- Done (2026-10-25), with one change to the design above: no state
  machine and no atomics. The awaiter's handle goes into the slot's first
  word (`{ ptr task; T value }`, no new fields), the callee leaves
  `LINKED` there and **defers** its registration (`zell_coro_defer`); the
  frames above it link with plain stores and the last frame of the
  synchronous chain to suspend publishes it (`zell_coro_register`), so a
  callee can never finish before its awaiter suspended and no CAS or
  exchange is needed (ABI.md 10.3). Measured (medians of 3, same session,
  HEAD -> C2, Go): await_chain yield 87 -> 20 ms (Go 7), sync 130 -> 129
  (Go 10); websocket_echo server CPU 11.3 -> 9.2 us/msg at 538k -> 591k
  msg/s (Go 7.6 us, 673k, measured as server ticks / messages with the Go
  client); http_hello HTTP/1.1 server CPU 8.5 -> 8.3 us/request, h2 server
  CPU 27.7 -> 25.9 us/request but 89k -> 86k req/s (one latency-bound
  connection); channel_pingpong, select_many, fanout_fanin, spawn_tasks
  unchanged within noise (fan-in and spawn run 1M +2%).

### C3. Cheaper synchronous frames (codegen + runtime)
- Change: (a) frames from a non-zeroing allocator: the lowering already
  stores zero into every slot the unwind path reads (`entry_alloca` init
  stores in `coro_prologue`); audit, then use `zell__heap_alloc(size, 0)`;
  (b) export `t_ctx` as a TLS global like `zell_coro_chain` and inline
  `zell_task_get_ctx` + `zell_coro_leave` (a load, a compare, retain only
  when non-NULL).
- Keeps: the destroy path never reads an uninitialised slot (panics spec
  tests under ASan), Windows TLS (same model as `zell_coro_chain`).
- Moves: await_chain sync (frame alloc/free/memset 18.4%, ctx 4.4%), every
  async call with a suspend point.
- Expected: sync 130 -> ~95 ms. Risk medium. Size 4 h.

### C4. Inline fast paths for done tasks and the free mutex (codegen + ABI)
- Change: emit the `state == TASK_DONE` test inline (offset already published in
  `zell_debug_task_layout`, make it ABI 10.2), `zell_task_result` as pointer
  arithmetic, and `Mutex.Lock`/`Unlock` as an inline CAS on the first word
  with the runtime call only on the slow path.
- Moves: mutex uncontended (71 vs 34 ms: 3 calls per iteration), every `await`
  of a completed task. Expected uncontended -> <= 1.3x Go. Risk low. Size 3 h.

Not pursued: cheaper local pinning (measured 0); fewer biased-RC atomics in
frames (frames are not refcounted; the hot `zell_release` is task objects,
removed by C2/R2); a malloc-free global queue (after C1 the global queue
carries only timer/poller posts, and R4 moves those); generic task pooling
(the object heap already serves tasks from size-class free lists; what costs
is attach/lock/drop, which C2 removes); direct channel hand-off (already
there: 1.06x Go on one worker).

## Order and acceptance

**Run 1, runtime only** (R1, R2, R3, R4, then R5 if the spec change is
approved): `make -C runtime CC=clang test`, `make -C runtime CC=clang tsan`
(target exists) and `make asan`, `make -C runtime CROSS=x86_64-w64-mingw32
test-wine`, `cd compiler && SPEC_FILTER=async cargo test --test spec`, the same
for `panics`, `zell run --leak-check` on `examples/` that use tasks,
channels, net (`grep -l task\\. examples/*.zl`), `scripts/spec-wine.sh`.
Targets (16 cores, wall, Go = 1.00): channel_pingpong ping-pong <= 1.2x,
pipeline <= 1.6x; fanout_fanin <= 1.7x; select_many <= 1.4x and CPU <= 3x Go;
async_io round trips/s >= 0.95x, context switches <= 20k; mutex contended
<= 1.3x with R5 (<= 2.6x without); no benchmark of `./scripts/bench-perf.sh
async` slower than today; spawn_tasks park stays <= 0.3x and its RSS
unchanged.

**Run 2, codegen/ABI** (C1, C3, C4, then C2; `runtime/ABI.md` 10.2/10.3/10.5
and `codegen/mod.rs` header updated in the same commits, `tests/coro_lowering.ll`
extended with a linked await): the run 1 checks plus the full
`cargo test --test spec`, `tests/spec/async*` under `--leak-check`, the
`panics` area with a panic in a linked frame 1, 2 and 1000 levels deep
(new spec tests), `runtime/tests/test_task.c` under TSan. Targets:
spawn_tasks run 1M <= 2.5x, wake <= 1.5x; await_chain sync <= 9.5x, yield
<= 4x (wall <= 6x, from 11.6x); mutex uncontended <= 1.3x; websocket_echo
CPU per message <= 13 us (Go 11.1).
