# .NET 10 vs Go vs Zell: async suite

.NET ports: `benchmarks/dotnet/` (one project, `Program.cs`, `bench <name>`; `SCALE=small` as in Go).
Measured 2026-10-24, same session and machine load for Go and .NET; medians of 3 runs of the values the programs print.

## Environment

- .NET SDK 10.0.108, `net10.0`, `dotnet build -c Release`, binary run directly (not `dotnet run`); TieredPGO on, workstation non-concurrent GC (`ServerGarbageCollector=false`, `ConcurrentGarbageCollection=false`), InvariantGlobalization, default thread pool (no `ThreadPool.SetMinThreads`, `DOTNET_ThreadPool_MinThreads` unset), no `NoGCRegion`.
- Go 1.24.7 (`go build`), default GOMAXPROCS. 16 hardware threads (AMD Ryzen 7 8845HS w- Radeon 780M Graphics), Linux 6.19, loopback TCP.
- Zell: not rebuilt; numbers are copied from commit bodies / docs (source in the table notes). They are from the other agent's runs, so machine load may differ.
- Ported faithfully: same phases, counts, LCG, output lines, checksums (all identical to Go). Deviations: Go unbuffered channels become `Channel.CreateBounded(1)` (.NET has no rendezvous channel); goroutines are `Task.Run`; `sync.Mutex` is `SemaphoreSlim(1,1).WaitAsync` (plus an uncontended `lock` column); `select` is `Task.WhenAny` over 8 pending `ReadAsync` re-armed per event (what `main.zl` does with `task.Any`); `context` is `CancellationTokenSource`; `Gosched` is `await Task.Yield()`; await_chain is an async recursion (Go's is a plain call chain, as in the README); async_io uses `Socket` async APIs, NoDelay, 1000 conns x 1000 x 64 B. park1M uses async tasks awaiting one gate `TaskCompletionSource` (a .NET task cannot block cheaply, a blocked thread per task would not scale).
- Zell columns until 2026-10-24 (superseded, see the note below): spawn_tasks, mutex, await_chain, pingpong: `7a4dc64`; fanout_fanin, select_many: `4b4ec33`; async_io: `docs/ASYNC_PLAN.md` first table (`53dbf10`, before the poller work); timers, cancel_tree: `benchmarks/README.md` (`ce6122a`, 4 vCPU table, wall/drain only).

> Note 2026-10-25: the Zell column is now re-measured, not copied: master `a1cd2e6`, `--release`, 16 threads, same session as the Go and .NET numbers, 5 interleaved rounds, medians (Go/.NET columns below are still the 2026-10-24 3-run medians; the fresh ones are in `benchmarks/README.md` "Async wave result (2026-10-25)", within a few percent). select_many `select` 1237 is interleaving noise, ~1120 in isolation.

## Results (ms unless noted)

### spawn_tasks
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| run1M | 246 | 148 | 260 | 1.76 |
| park1M | 235 | 1039 | 480 | 0.46 |
| wake1M | 89 | 123 | 169 | 1.37 |

### channel_pingpong
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| pingpong 5M | 430 | 423 | 2374 | 5.6 |
| pipeline 1M | 125 | 76 | 258 | 3.4 |

### fanout_fanin
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| fan-in | 460 | 324 | 569 | 1.76 |
| pool | 136 | 76 | 641 | 8.4 |

### mutex_contention
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| contended | 107 | 206 | 3444 | 16.7 |
| uncontended | 39 | 38 | 263 (`SemaphoreSlim`), 55 (`lock`) | 6.9 / 1.4 |

### timers
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| total (wall) | 66 | 51 | 65 | 1.27 |
| start loop | 56 | 41 | 55 | 1.34 |
| mean / max late (us) | 41 / 267 | 129 / 2028 | 808 / 4315 | 6.3 / 2.1 |

### await_chain
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| sync (10M calls) | 11 | 10 | 302 | 30 |
| yield (1k chains) | 14 | 6 | 125 (AOT: 51) | 21 (8.5) |

### cancel_tree
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| cancel drain avg (us) | 2323 | 4393 | 8776 | 2.0 |
| cancel drain max (us) | 2962 | 5909 | 19216 | 3.3 |
| timeout drain avg (us) | 2777 | 4834 | 8192 | 1.7 |

### async_io
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| total | 1254 (797k rt/s) | 1255 | 1551 | 1.24 |
| round trips/s | 797k | 797k | 645k | 0.81 |
| p50 / p99 / max (us) | 1180 / 1591 / 21014 | 1130 / 2491 / 60088 | 1353 / 3287 / 21853 | 1.2 / 1.3 / 0.36 |

### select_many
| phase | Zell | Go | .NET | .NET/Go |
|---|---|---|---|---|
| select / task.Any | 1237 | 832 | 1009 | 1.21 |
| merge | 827 | 399 | 960 | 2.4 |

## CPU and context switches (whole run, median of 3)

| bench | Go CPU s | Go switches | .NET CPU s | .NET switches |
|---|---|---|---|---|
| spawn_tasks | 5.25 | 167943 | 8.60 | 146978 |
| channel_pingpong | 0.61 | 15950 | 23.9 | 710 |
| fanout_fanin | 1.52 | 198004 | 16.4 | 203236 |
| mutex_contention | 0.69 | 12359 | 37.3 | 2489 |
| timers | 0.22 | 5463 | 0.58 | 7701 |
| await_chain | 0.02 | 1063 | 0.76 | 2816 |
| cancel_tree | 0.98 | 82325 | 2.92 | 30322 |
| async_io | 18.9 | 3910 | 21.0 | 154699 |
| select_many | 2.09 | 71099 | 22.0 | 902385 |

(task-clock = user+sys.) Zell CPU/switch figures exist only in `ASYNC_PLAN.md` for the pre-wave runs (select_many 23 CPU s, async_io 650k switches), so they are not tabulated.

## Findings

- Ahead of Go: park1M (480 vs 1039 ms): a suspended `async` method is a small heap state machine, a parked goroutine owns a stack; same effect as Zell's 263 ms. Also async_io is within 1.2x of Go with 1.3x p99 and lower max latency, and CPU at parity.
- Close to Go: timers (1.27x, but 6x mean lateness: the single timer queue wakes and then needs a pool thread hop), select_many select (1.2x), spawn run/wake (1.4-1.8x, `Task.Run` = queue item + `Task` object, no stack to allocate).
- Far behind Go: everything that is a chain of dependent hand-offs. Ping-pong 5.6x, contended `SemaphoreSlim` 16.7x (37 CPU s: its `WaitAsync` waiters are a lock-protected linked list of `Task`s, each release completes a `Task` and queues the continuation to the pool, so ownership hops threads on every acquire, the same FIFO hand-off cost Zell pays at 2.7x; Go's mutex spins and lets the running goroutine barge), pool 8.4x, await_chain 21-30x (one `Task<int>` + state machine box per level when it does not complete synchronously; 10M awaits at 30 ns; AOT halves the yield phase because there is no tiering/PGO warm-up of the continuation path).
- CPU burn is the real .NET cost: pool threads spin (`PortableThreadPool` worker spinning, `SemaphoreSlim` and `Channel` waiters spinning before parking) so ping-pong takes 24 CPU s for 2.4 s wall; Go parks via direct goroutine hand-off (`runnext`). select_many shows 900k context switches (Zell's pre-R3 symptom: 51k at 4 workers) and 22 CPU s.
- No thread-injection problem was visible: spawn run1M is not dominated by pool ramp-up (queue items run on the 16 workers, `SetMinThreads` was not used). The 2x slower cancel drain is likely cancellation callbacks being queued to the pool (not verified).
- Mapping to Zell items: R3 (idle protocol) corresponds to `PortableThreadPool`'s spin-then-park workers and its "no work, one thread keeps spinning" policy; Zell's cost today (sched_yield storms) is the same class as .NET's spinning, and .NET's 22 CPU s on select_many/16 s on fanout shows the price of getting that wrong at 16 threads. C2 (frame-linked await, no task per await) corresponds to what .NET does with the state machine on the heap, `AsyncTaskMethodBuilder` caching the completed `Task<int>`/`ValueTask` and, in .NET 10, runtime-async (not enabled here): synchronous completion costs ~30 ns here. `Channel<T>` waits using pooled `IValueTaskSource` (`ReadAsync` / `WriteAsync` without a `Task` per wait) are the analogue of R2 (allocation-free channel waits) and of the "task.Yield without allocation" commit; `ThreadPool` per-worker local queues with stealing and `Task.Yield` queued to the local queue are the analogue of the local-queue change in `7a4dc64`.
- Not ported faithfully: unbuffered channels (capacity 1 instead of rendezvous, which makes .NET ping-pong slightly easier than Go's, the 5.6x is not an artifact against it); await_chain (async recursion vs Go call stack, as in Zell's version); Zell's timers and cancel_tree come from a 4 vCPU table in the README, not 16 threads; Zell async_io is pre-R-poller.
