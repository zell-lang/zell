# websocket_echo: where the time goes (Zell `net.ws` vs gorilla/websocket)

Measured on master `0ec8d40`, 16 logical cores, loopback, 1000 connections x
10 000 messages of 128 bytes, one in flight per connection. Zell binary built
with `zell build --release -g` (symbols kept; same codegen as `--release`).
Profiles: `perf record -g -F 999` for 6 s in the middle of a full run, one
pairing at a time, the other side being the Go program.

Throughput in these runs (the shape differs a little from `RESULTS-framework.md`,
whose numbers come from 5 alternating rounds; the ratios are what matters):

| pairing | msg/s |
|---|---|
| Go client -> Go server | 667k |
| Go client -> Zell server | 445k |
| Zell client -> Go server | 453k |

## Headline findings

1. The machine is CPU-bound on both sides (16 cores shared by client and
   server), so throughput is inverse CPU per message. Process CPU per message
   (`perf stat` task-clock / messages in the 5 s window):

   | process | CPU per message | user | kernel (incl. loopback TCP) |
   |---|---|---|---|
   | Go server (gorilla) | 11.1 us | 17% (+~13% netfilter) | 68% |
   | Zell server | 19.9 us | 34% + 3.6% libc (+~8% netfilter) | 53% |
   | Zell client | 19.6 us | 33% + libc (+~7% netfilter) | 54% |

   So Zell burns ~1.8x the CPU per message: about 3.8x the Go user-space time
   (7.3 us vs 1.9 us) and ~1.35x the kernel time (12.4 us vs 9 us).
2. Zell user time is dominated by per-message async machinery and
   refcounting, not by WebSocket framing: there is no masking/parsing symbol
   in the top 20 (`rtMask` and the header parse are below 0.5%).
3. Zell makes 1 context switch per ~2 messages (0.52/msg server, 0.55/msg
   client); Go makes 0.025/msg. Idle-worker futex wakes (poller thread ->
   global queue -> parked worker) account for most of it.
4. Syscalls per message: Zell 1 send + 2 recv + 1 epoll_ctl + 0.35 epoll_wait
   (+ futex); Go ~1 write + ~1-2 read, edge-triggered epoll, no epoll_ctl.

## perf top, Zell server (Go client load), self %

| % | symbol | note |
|---|---|---|
| 53.2 (total kernel) | kernel | `__x64_sys_sendto` 34.3% inclusive (loopback TCP delivery to the peer runs in the sender's context), recvfrom 6.8%, epoll_wait 5.5%, futex 3.1%, epoll_ctl 2.1%, `nf_hook_slow` 10.4% (nftables/conntrack on this host, same for Go) |
| 5.47 | `zell_release` (10.5% inclusive) | refcount drop + object free |
| 5.07 | `entry_SYSRETQ_unsafe_stack` | syscall return |
| 3.54 | `zell__hp_alloc_slow` | allocator slow path (per-thread pool miss) |
| 1.63 | `zell_bytes__Clone__` | `bytes.Clone` of each ReadExact |
| 1.62 | `zell__rc_retain_biased` | |
| 1.50 | `zell__rc_release_biased` | |
| 1.22 | `net.ws.recv.resume` | the ws recv coroutine |
| 1.09 | `task_drop` | |
| 0.97 | `zell_retain` | |
| 0.95 | `sched_post_ctx` | enqueue a continuation |
| 0.92 | `task_complete` | |
| 0.92 | `net.http.rawReadExact` | |
| 0.92 | `zell_task_lazy` (4.0% inclusive) | |
| 0.68 | `zell_object_new` | |
| 0.59 | `memset` | zeroing new objects/frames |
| 0.56 | `add_cont` | |
| 0.53 | `net.ws.recv` | |
| 0.51 | `net.http.fillTo` | |
| 0.50 | `zell_task_get_ctx` | |
| 0.46 | `net.ws.writeFrame` (40.7% inclusive: the send path including the syscall) | |
| 0.45 / 0.35 | `zell_coro_frame_alloc` / `_free` | |

Zell client (Zell -> Go server) is the same list in the same order
(`zell_release` 4.8%, `hp_alloc_slow` 2.9%, `Clone` 1.6%, `client.resume`
1.5%, retain/release 1.5% each, `ws.recv.resume` 1.2%); not repeated.

## Allocations per message (measured)

gdb breakpoint counters on `zell_object_new`, `zell_coro_frame_alloc`,
`zell_task_lazy`, `zell_task_new_plain`, differenced between a 1100-message and
a 100-message single-connection in-process run (client + server in one
process; a round trip is client Send, server Recv, server Send, client Recv):

| per round trip (both sides) | count | per side per message |
|---|---|---|
| `zell_object_new` | 32.3 | ~16 |
| `zell_coro_frame_alloc` (every `async func` call) | 29.0 | ~14.5 |
| `zell_task_lazy` | 10.2 | ~5 |
| `zell_task_new_plain` (I/O readiness waits) | 2.0 | 1 |
| `zell_slice_new` | 2.0 | 1 |

gorilla allocates about one buffer per received message (the returned `[]byte`)
and none on the write path. Zell does ~30 heap allocations plus ~15 coroutine
frame setups per message per side. (Not measured: bytes allocated.
`ZELL_RC_STATS` reported 185 782 objects "biased" for 20 000 messages in the
in-process small run, consistent with this.)

## Syscalls per message

Zell side counted with an `LD_PRELOAD` shim over libc (`send`, `recv`,
`epoll_ctl`, `epoll_wait`, `read`, `write`), 1000 connections x 2000 messages
(2M messages), same result for the server and the client:

| syscall | per message |
|---|---|
| `send` | 1.00 |
| `recv` | 2.00 |
| `epoll_ctl` | 1.00 |
| `epoll_wait` | 0.34-0.38 |
| `futex` | not counted by the shim; 3.1% of server time in perf, ~0.5 context switches/msg |

So ~4.4 syscalls per message plus futex. Why 2 `recv` and 1 `epoll_ctl`:
`net.sock.readInto` (`std/net/net.zl:539`) first tries `recv` speculatively; on
a drained socket that returns EAGAIN, then `waitRead` -> `io_wait_for`
(`runtime/src/task_io.c:380`) re-arms the fd with `EPOLL_CTL_MOD` + EPOLLONESHOT
(`runtime/src/platform/poll_epoll.c:74-77`, one `epoll_ctl` per wait), then the
poller thread's `epoll_wait` returns, and the retry `recv` gets the data.

Go side: not counted with the same tool (no strace, no tracefs permission, Go
bypasses libc so the shim cannot see it). From the design and the perf
profile (kernel entries are `read`/`write`/`epoll_pwait`, no `epoll_ctl`, no
`futex` beyond 0.7%): about 1 write + 1-2 read per message with
edge-triggered epoll registered once; `epoll_pwait` is 3% of the Go server.
This is an inference, not a measurement.

## Hot source locations

| where | what it costs |
|---|---|
| `std/net/ws/ws.zl:295-345` `recv` | three separate `await st.rc.ReadExact(...)` per masked frame (2-byte header, 4-byte mask key, payload): 3 coroutine frames + 3 `bytes.Clone` + 3 slices, all served from the same 4 KiB buffer that one `recv` already filled |
| `std/net/http/http_server.zl:1111` `rawReadExact` (+ `http.zl:446` `fillTo`, `:434` `fill`) | async wrapper + `bytes.Clone` per call, even when the bytes are already buffered |
| `std/net/ws/ws.zl:186-225` `encodeFrame` | builds the header by repeated `append` onto an empty slice (1, 2, 4... growth: 2-3 allocations), then `rtJoin` allocates and copies header + payload again; client adds `random.Int()` + a second mask pass |
| `std/net/ws/ws.zl:226-248` `sendRaw`/`writeFrame` | `await st.wmu.Lock()` (an async mutex task) + `rc.Write` -> `conn.write` -> `sock.write`: 4-5 nested async calls (each a coroutine frame and a Task) for one `send` |
| `std/net/ws/ws.zl:134`, `benchmarks/websocket_echo/main.zl:15` | `for msg in conn` -> `Next()` -> `recv()`: two more coroutine layers per message |
| `std/net/net.zl:539-560` `readInto` | speculative `recv` that is EAGAIN in the steady state |
| `runtime/src/task_io.c:380-428` `io_wait_for` | per wait: `zell_task_new_plain`, `malloc(io_wait)`, shard mutex, `epoll_ctl(MOD)`; completion is delivered by a separate poller thread |
| `runtime/src/platform/poll_epoll.c:74-77` | EPOLLONESHOT re-armed on every wait |
| `runtime/src/task.c:704-720` `post_batch_flush` | poller thread posts the continuation to the global queue under `q_mu` and `cond_signal`s a parked worker: the thread hand-off behind 0.5 context switches/message and the futex time |
| `runtime/src/mem.c:188`, `heap.c` `zell__hp_alloc_slow` | allocator slow path is 3.5% self; objects freed on a different thread than allocated them (poller/worker hand-off) miss the thread-local pool |

## Ranked fixes (estimates, not measured)

1. **Stop awaiting for bytes that are already buffered; parse the frame
   header in place** (`std/net/ws/ws.zl` `recv`, `std/net/http/http_server.zl`
   `RawConn`, `std/net/http/http.zl` `fillTo`). Add a synchronous
   `RawConn.Peek(n)`/`Buffered()` (or make `ReadExact` return a plain slice
   when `buffered() >= n`) and read the 2/4/8 header bytes and mask key
   straight from `b.buf`; only `await` when the buffer is short; unmask in
   place and clone the payload once. Removes ~6 of ~15 coroutine frames and
   ~3 of ~16 object allocations per message per side. Estimated 10-15% of
   per-message CPU. Small, local change, no ABI change.
2. **Make the write path one call and one allocation**: build the frame into a
   single pre-sized buffer (`bytes.Make(2+ext+mask+n)`, fill header by index,
   copy payload once; unmasked server frames can use `WriteV` with header +
   payload and skip the copy), and skip `wmu.Lock()` when the lock is
   uncontended (a try-lock fast path, or a synchronous `tryLock`). Removes the
   append-growth allocations, the join copy and 2-3 coroutine layers. Files:
   `std/net/ws/ws.zl` (`encodeFrame`, `sendRaw`), the mutex in `std/sync`.
   Estimated 8-12%.
3. **Cut the per-message epoll cost**: (a) after a `recv` that returned fewer
   bytes than asked, the socket is drained, so go straight to `waitRead`
   instead of doing the EAGAIN `recv` first (`std/net/net.zl` `readInto`,
   also valid with the current oneshot poller): -1 syscall/message (~2.0 -> 1.0
   recv); (b) edge-triggered, registered-once epoll (no ONESHOT, no
   `EPOLL_CTL_MOD` per wait) in `runtime/src/platform/poll_epoll.c` and
   `runtime/src/task_io.c`: -1 syscall/message plus the shard-mutex round
   trip. Together ~2 of ~4.4 syscalls per message; at ~1 us per syscall on
   this kernel (srso/retbleed mitigations show in the profile: `srso_alias_*`,
   `entry_SYSRETQ`) this is ~2 us of ~20 us, estimate 10-12%. (b) needs care
   for the "every waiter of a direction is woken" and generation-token
   semantics documented at the top of `task_io.c`.
4. **Reduce cross-thread hand-off** (`runtime/src/task_io.c`,
   `runtime/src/task.c`): the poller thread -> global queue -> parked worker
   path costs a futex wake and ~0.5 context switches per message, and
   frees/allocs cross threads (`hp_alloc_slow`, `zell_release` inclusive 10%).
   Quick experiment (server vs Go client, 3M messages): default (16
   workers, 8 pollers) 452k msg/s at 15.6 us CPU/msg; `ZELL_WORKERS=4
   ZELL_IO_POLLERS=2` 266k msg/s at 13.7 us/msg; `ZELL_WORKERS=2
   ZELL_IO_POLLERS=1` 155k msg/s at 11.8 us/msg. Fewer threads cut CPU per
   message by up to 25% (so spinning/wake/cross-thread overhead is worth
   ~4 us per message), but the throughput falls because the server then has
   too few cores; the win is to keep the parallelism and lose the wake-ups:
   let the poller thread run the continuation inline for short ready tasks, or
   have the worker that parks do its own `epoll_wait` (Go's netpoller model).
   Largest potential (up to ~20%) but also the largest and riskiest change.
5. **Allocator/refcount**: `zell_release` + `rc_*` + `hp_alloc_slow` +
   `task_drop` + `memset` are ~17% of server samples; they shrink with fixes
   1-2 (fewer objects), and a cross-thread free path for the pool would
   address `hp_alloc_slow` (3.5%) directly. Not separately estimated.

Applying 1-3 should bring Zell CPU/message from ~20 us to roughly 13-14 us
(estimate: ratio to Go ~0.6-0.65 instead of 0.4); matching Go needs fix 4
and fewer coroutine layers in general.

## What I could not measure

- Go's syscalls per message (no strace, tracefs not readable, `perf trace`
  fails: no `raw_syscalls` events); the Go numbers above are inferred.
- Zell `futex` count (inside libc/pthread, invisible to the shim).
- Bytes allocated per message; only allocation counts (via gdb breakpoints on
  a single connection, so pool/cache effects of 1000 connections are not
  included).
- The Go client's own CPU; only the process under test was sampled each time.
- netfilter (nftables/conntrack) takes ~8-13% of samples on both Go and Zell
  here; that is host configuration, not the program, and is identical for
  both so it does not affect the ratios but it does compress them.
- Absolute msg/s differ from `RESULTS-framework.md` (single run each, noisy
  machine, perf sampling overhead); use the CPU-per-message figures.
- Scratch scripts and raw `perf.data` are in the session scratchpad, not
  committed.
