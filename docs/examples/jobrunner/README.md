# jobrunner: async workers, timeouts and graceful shutdown

A job runner of the kind every backend grows sooner or later: a fixed
pool of workers takes jobs from a bounded queue, each job has its own
deadline, the results come back through one channel, and the whole run
can be stopped from outside. The same pool then runs as a small service
that submits a job on every tick until it receives SIGTERM, and shuts
down without losing work.

All the waiting is real (`task.Delay`, channels, a mutex), but the jobs
have fixed costs and deadlines with wide margins, so `zell run` prints
the same report on every run.

What it shows:

- a bounded worker pool over `task.Channel<T>`: a job queue that pushes
  back on the producer, and a results channel drained by one collector
  (fan-in);
- `task.All` over the slice of worker tasks to close the results
  channel when the last worker exits;
- per-job deadlines with `task.WithTimeout`, and work that stops early
  through `task.CheckCancelled()`;
- cancelling a whole run from the parent with `task.NewCancel()` and a
  context passed down, and why the bookkeeping must not run in the
  cancelled context;
- a mutex-protected stats struct (`task.Mutex`) shared by all workers;
- a service loop that waits for "a tick or a signal, whichever comes
  first", and a graceful shutdown that drains the queue;
- async tests (`@test async func`).

## Run it

```sh
cd docs/examples/jobrunner
zell run
zell test
```

## Layout

| File | What is in it |
|---|---|
| `zell.toml` | the manifest (root namespace `jobrunner`) |
| `src/jobs.zl` | `jobrunner.jobs`: `Job`, `Result`, the outcomes, the simulated work |
| `src/pool.zl` | `jobrunner.pool`: the pool, the workers, the stats, `RunBatch` |
| `src/service.zl` | `jobrunner.service`: the tick loop, the signal, graceful shutdown |
| `src/main.zl` | the three runs |
| `src/pool_test.zl` | tests (`zell test`) |

<!-- file: zell.toml -->
```toml title="zell.toml"
[package]
name = "jobrunner"
version = "0.1.0"
namespace = "jobrunner"
entry = "src/main.zl"
```

## src/jobs.zl: the work

A job does not know it runs in a pool. `Work` is an ordinary `async`
function that waits in 5 ms steps and calls `task.CheckCancelled()?`
between them. That is all a piece of work needs to be stoppable: when
the task context it runs in is cancelled (by a deadline or by the run),
`task.Delay` returns at once and `CheckCancelled` fails with
`task.ErrCancelled`. A long computation without waits would call
`CheckCancelled` (and `await task.Yield()`) between its own steps.

A failing job wraps the sentinel `ErrBadInput` with the job's name.
`errors.Is` finds the sentinel through the wrapper, so the pool can tell
a bad input from a timeout without parsing messages.

<!-- file: src/jobs.zl -->
```zell title="src/jobs.zl"
// Jobs: what a worker runs. A job is simulated work: it sleeps for its
// cost in small steps and checks for cancellation between them, the way
// real work (a download, a query, an image resize) would stop early.
namespace jobrunner.jobs

// ErrBadInput is a sentinel error: callers test for it with errors.Is,
// even after it has been wrapped with the job's name.
const ErrBadInput = error("bad input")

// The outcome of one job. Plain string constants print as they are.
const Done = "done"
const Failed = "failed"
const TimedOut = "timed out"
const Cancelled = "cancelled"
const Skipped = "skipped"

struct Job {
    Id      int
    Name    string
    Ms      int  // how long the work takes
    Timeout int  // the job's own deadline in milliseconds; 0 for none
    Fails   bool // its input is bad: the job returns ErrBadInput
}

struct Result {
    Job    Job
    Status string
    Detail string
}

// Work runs the job. It is an ordinary async function: it knows nothing
// about workers, timeouts or the run it belongs to. Cancellation reaches
// it through the task context it was started in: once that context is
// cancelled, task.Delay returns at once and CheckCancelled fails.
async func Work(job Job) string! {
    var left = job.Ms
    for left > 0 {
        var step = 5
        if left < step {
            step = left
        }
        await task.Delay(step)
        task.CheckCancelled()?
        left -= step
    }
    if job.Fails {
        return errors.Wrap(ErrBadInput, "job " + job.Name)
    }
    return fmt.Format("{0}ms of work", job.Ms)
}
```

## src/pool.zl: workers, queue, fan-in

`Pool.Start` makes two channels and starts the workers:

- `Queue` has room for `depth` jobs. When it is full, `Submit` suspends
  the producer until a worker takes a job: the queue is bounded, so a
  fast producer cannot pile up unbounded work in memory.
- `Results` is the fan-in: every worker sends to it, one collector
  receives. `Collect` sorts by job id at the end, so the report does
  not depend on which worker happened to be faster.
- `running` keeps the worker tasks. `closeWhenDone` waits for all of
  them with `task.All(p.running)` (a slice: the number of workers is
  known only at run time) and closes `Results`, which ends the
  collector's `for r in p.Results`.

`for job in p.Queue` receives until the queue is closed and drained.
That is how shutdown works without a special message: the producer
closes the queue, the workers finish what is left and exit.

`runOne` runs one job and classifies the outcome. A deadline and a
cancelled run both arrive as `task.ErrCancelled`; the difference is
whether the run's own context is cancelled as well. The job's deadline
scope (`task.WithTimeout`) is created inside the run's context
(`task.RunWith(run, ...)`), so cancelling the run reaches it too.

**Cancel the work, not the bookkeeping.** In a cancelled context every
wait that has no value yet fails at once: a `Recv` on an empty channel,
a `task.Delay`. If the collector or the workers' own loops ran in the
cancelled context, the collector would stop receiving the moment the
run is cancelled and the report would be empty. So the pool takes the
jobs' context as a parameter (`jobs task.Context`): the workers, the
collector and the stats live in the caller's context, and only the jobs
run in the cancellable one.

`Stats` is updated by every worker, on several threads. Each access
takes `task.Mutex`; a task waiting for it is suspended and its thread
runs other tasks. The counts are a map from outcome to number, and
`Summary` reads them under the same lock.

<!-- file: src/pool.zl -->
```zell title="src/pool.zl"
// The worker pool: a fixed number of workers read jobs from one bounded
// queue (a channel) and send their results to a second channel, which a
// single collector drains (fan-in). Stats are shared by every worker and
// protected by a mutex.
namespace jobrunner.pool

import jobrunner.jobs

// Stats counts outcomes. Workers on different threads update it at the
// same time, so every access holds the mutex. task.Mutex suspends a
// waiting task instead of blocking its thread.
struct Stats {
    mu      task.Mutex
    started int
    counts  map[string]int

    async func Record(status string) {
        await this.mu.Lock()
        if status != Skipped {
            this.started += 1
        }
        this.counts[status] += 1
        this.mu.Unlock()
    }

    async func Summary() string {
        await this.mu.Lock()
        var s = fmt.Format("started {0}", this.started)
        for name in [Done, Failed, TimedOut, Cancelled, Skipped] {
            s += fmt.Format(", {0} {1}", name, this.counts[name])
        }
        this.mu.Unlock()
        return s
    }
}

// Pool owns the two channels and the context its jobs run in. The queue is bounded: once Depth jobs are
// waiting, Submit suspends the producer until a worker takes one, which
// is how a pool pushes back on a producer that is faster than it.
struct Pool {
    Workers int
    Queue   task.Channel<Job>
    Results task.Channel<Result>
    Stats   *Stats
    Jobs    task.Context
    running []Task<void>

    // Start launches the workers. They, the collector and the stats run
    // in the caller's context; only the jobs run in `jobs`. Cancelling
    // `jobs` stops the work but not the bookkeeping: in a cancelled
    // context every wait that has no value yet fails, so a collector
    // running there would stop receiving results at once.
    static func Start(workers int, depth int, jobs task.Context) *Pool {
        var p = new Pool{
            Workers: workers,
            Jobs: jobs,
            Queue: task.Channel<Job>(depth),
            Results: task.Channel<Result>(workers),
            Stats: new Stats{counts: map[string]int{}},
        }
        for _ in 0..workers {
            p.running = append(p.running, worker(p))
        }
        // Results is closed once the last worker is done, so the
        // collector's `for r in p.Results` ends by itself.
        _ = closeWhenDone(p)
        return p
    }

    func Submit(job Job) Task<void> {
        return this.Queue.Send(job)
    }

    // Close says no more jobs will come; workers finish what is queued.
    func Close() {
        this.Queue.Close()
    }
}

// worker takes jobs until the queue is closed and drained, or until its
// context is cancelled and nothing is left in the queue.
async func worker(p *Pool) {
    for job in p.Queue {
        var r = await runOne(job, p.Jobs)
        await p.Stats.Record(r.Status)
        await p.Results.Send(r)
    }
}

// runOne runs a job under its own timeout and turns the outcome into a
// Result. A timeout and a cancelled run both surface as
// task.ErrCancelled; the difference is whether the run's context is
// cancelled too.
async func runOne(job Job, run task.Context) Result {
    if run.IsCancelled() {
        return Result{Job: job, Status: Skipped, Detail: "the run was cancelled before it started"}
    }
    var work = task.RunWith(run, func() Task<string!> {
        return start(job)
    })
    var out = try await work catch err {
        if errors.Is(err, ErrBadInput) {
            return Result{Job: job, Status: Failed, Detail: err.Message()}
        }
        if run.IsCancelled() {
            return Result{Job: job, Status: Cancelled, Detail: "stopped by the run"}
        }
        return Result{Job: job, Status: TimedOut, Detail: fmt.Format("deadline {0}ms", job.Timeout)}
    }
    return Result{Job: job, Status: Done, Detail: out}
}

// start begins the job, inside a scope that is cancelled after its
// timeout when it has one. It is nested in the run's context, so
// cancelling the run cancels it too.
func start(job Job) Task<string!> {
    if job.Timeout > 0 {
        return task.WithTimeout(job.Timeout, func() Task<string!> {
            return Work(job)
        })
    }
    return Work(job)
}

async func closeWhenDone(p *Pool) {
    await task.All(p.running)
    p.Results.Close()
}

// Collect is the fan-in: one task receives every result, whichever
// worker produced it, and sorts them so the report does not depend on
// which worker was faster.
async func Collect(p *Pool) []Result {
    var out []Result = []
    for r in p.Results {
        out = append(out, r)
    }
    slices.SortBy(out, (r Result) => r.Job.Id)
    return out
}

// RunBatch submits a fixed list of jobs, closes the queue and waits for
// every result. `run` is the context of the jobs: pass
// task.CurrentContext(), or a cancel handle's Context to be able to stop
// the whole batch from outside.
async func RunBatch(list []Job, workers int, depth int, run task.Context) ([]Result, string) {
    var p = Pool.Start(workers, depth, run)
    var collected = Collect(p)
    for job in list {
        if run.IsCancelled() {
            break
        }
        await p.Submit(job)
    }
    p.Close()
    var results = await collected
    return (results, await p.Stats.Summary())
}
```

## src/service.zl: ticks, a signal, graceful shutdown

The service submits a job on every tick until a signal arrives. Go
would write this as a `select` over a ticker channel and a signal
channel. Zell has no `select` statement. The obvious replacement,
`task.Any` over a receive and a timer in a loop, works (the losing
receive is cancelled and leaves the channel), but it is not atomic like
`select`: a signal that arrives in the very instant the tick wins can be
taken by the receive that is being cancelled, and then it is gone.

So the loop is built from cancellation instead, which cannot lose a
signal: one task, `watch`, owns the only receive on the signal channel and
cancels a scope when the signal comes; the loop waits for each tick
inside that scope. A cancelled `task.Delay` returns at once, so the
signal ends the wait immediately and the loop sees `stop.IsCancelled()`.

Graceful shutdown is then two lines: `p.Close()` stops accepting jobs,
and awaiting the collector waits until the workers have finished the
queued and running jobs.

<!-- file: src/service.zl -->
```zell title="src/service.zl"
// A long-running service on the pool: it submits a job on every tick of
// a timer until a signal asks it to stop, then shuts down gracefully:
// no new jobs, the queued and running ones finish, results are reported.
namespace jobrunner.service

import io
import os
import jobrunner.jobs
import jobrunner.pool

// watch waits for the first signal and turns it into a cancellation of
// the service loop's scope; it returns the signal number.
async func watch(sigs task.Channel<int>, stop task.CancelHandle) int {
    var sig = try await sigs.Recv() else 0
    stop.Cancel()
    return sig
}

// Serve runs until a signal arrives on sigs. next makes the n-th job.
//
// The loop waits for "a tick or a signal, whichever comes first". Zell
// has no `select` statement; here the timer wait runs in a scope that
// the signal watcher cancels, and a cancelled task.Delay returns at once.
// So a signal ends the wait immediately, and nothing is lost: there is
// exactly one receive on the signal channel, owned by the watcher.
async func Serve(next func(int) Job, sigs task.Channel<int>, tickMs int) ([]Result, string) {
    var p = Pool.Start(2, 4, task.CurrentContext())
    var collected = Collect(p)
    var stop = task.NewCancel()
    var watcher = watch(sigs, stop)
    var n = 0
    for {
        await task.RunWith(stop.Context, func() Task<void> {
            return task.Delay(tickMs)
        })
        if stop.IsCancelled() {
            break
        }
        n += 1
        await p.Submit(next(n))
    }
    var sig = await watcher
    println("  received", signal.Name(sig) + ": no new jobs, draining", n, "submitted")

    // Graceful shutdown: closing the queue lets the workers finish what
    // is queued and running, then they exit and Results is closed.
    p.Close()
    var results = await collected
    return (results, await p.Stats.Summary())
}
```

## src/main.zl: three runs

The first run is a batch where every kind of outcome happens once:
`build-index` needs 400 ms but has a 50 ms deadline, `parse-feed` has
bad input, and `report` finishes well inside its deadline.

The second run cancels five 500 ms jobs after 60 ms. Three are running
on the three workers and stop early; the other two are still in the
queue, and the workers skip them without starting them. The pool itself
is not cancelled: the collector still receives all five results.

The third run is the service. A real program passes the channel that
`signal.Notify` returns, as the comment shows; here a simulated
operator sends SIGTERM (15) on a channel of the same type while the
fourth job is made. The loop sees it at its next wait, prints the
signal's name with `signal.Name`, and drains the four jobs.

<!-- file: src/main.zl -->
```zell title="src/main.zl"
// jobrunner: async workers in three runs over the same pool. A batch
// with timeouts and a failing job, the same kind of batch cancelled from
// outside, and a long-running service that shuts down gracefully when it
// receives SIGTERM.
namespace jobrunner

import io
import jobrunner.jobs
import jobrunner.pool
import jobrunner.service

func job(id int, name string, ms int) Job {
    return Job{Id: id, Name: name, Ms: ms}
}

func print(results []Result, summary string) {
    for r in results {
        println(fmt.Sprintf("  #%d %-14s %-10s %s", r.Job.Id, r.Job.Name, r.Status, r.Detail))
    }
    println("  stats:", summary)
}

async func main() {
    // 1. A batch: every job ends one way or another.
    var nightly = [
        job(1, "resize-images", 30),
        job(2, "send-emails", 10),
        Job{Id: 3, Name: "build-index", Ms: 400, Timeout: 50},
        Job{Id: 4, Name: "parse-feed", Ms: 10, Fails: true},
        job(5, "backup-db", 40),
        job(6, "thumbnails", 20),
        Job{Id: 7, Name: "report", Ms: 10, Timeout: 200},
        job(8, "cleanup", 10),
    ]
    println("batch: 8 jobs, 3 workers, a queue of 2")
    var (results, summary) = await RunBatch(nightly, 3, 2, task.CurrentContext())
    print(results, summary)

    // 2. The same pool cancelled from outside after 60ms. Three jobs are
    // running and stop early; two are still in the queue and are skipped.
    var slow []Job = []
    for i in 1..6 {
        slow = append(slow, job(i, "import-" + strconv.Itoa(i), 500))
    }
    println("cancelled run: 5 slow jobs, 3 workers, cancelled after 60ms")
    var h = task.NewCancel()
    var run = RunBatch(slow, 3, 5, h.Context)
    await task.Delay(60)
    h.Cancel()
    var (cancelled, summary2) = await run
    print(cancelled, summary2)

    // 3. A service: a job every tick until a signal arrives. A real
    // program passes the channel of OS signals:
    //
    //     var sigs = signal.Notify([signal.SIGINT, signal.SIGTERM])
    //
    // Here the same kind of channel is fed by a simulated operator who
    // presses Ctrl-C (SIGTERM, 15) after the fourth job, so every run
    // prints the same thing.
    println("service: a job every 40ms until SIGTERM")
    var sigs = task.Channel<int>(1)
    var source = func(n int) Job {
        if n == 4 {
            _ = sigs.Send(15)
        }
        return job(n, "tick-" + strconv.Itoa(n), 30)
    }
    var (served, summary3) = await Serve(source, sigs, 40)
    print(served, summary3)
}
```

## src/pool_test.zl: async tests

The tests are `async`: the test runner awaits them, so they call
`RunBatch` directly. The first one uses a queue of one slot and more
jobs than workers, so `Submit` has to wait for the workers again and
again.

<!-- file: src/pool_test.zl -->
```zell title="src/pool_test.zl"
// Tests for the pool. They are async: the runner awaits them, so they
// can run batches directly. Each checks one way a job can end.
namespace jobrunner.tests

import jobrunner.jobs
import jobrunner.pool

func quick(id int) Job {
    return Job{Id: id, Name: "q" + strconv.Itoa(id), Ms: 5}
}

@test
async func TestEveryJobHasOneResultInOrder(t *testing.T) {
    var list []Job = []
    for i in 1..11 {
        list = append(list, quick(i))
    }
    // More jobs than queue slots and workers: Submit has to wait.
    var (results, summary) = await RunBatch(list, 3, 1, task.CurrentContext())
    testing.Equal(t, len(results), 10)
    for i, r in results {
        testing.Equal(t, r.Job.Id, i + 1)
        testing.Equal(t, r.Status, Done)
    }
    testing.Equal(t, summary, "started 10, done 10, failed 0, timed out 0, cancelled 0, skipped 0")
}

@test
async func TestTimeoutAndFailure(t *testing.T) {
    var list = [
        Job{Id: 1, Name: "slow", Ms: 1000, Timeout: 20},
        Job{Id: 2, Name: "bad", Ms: 1, Fails: true},
    ]
    var (results, _) = await RunBatch(list, 2, 2, task.CurrentContext())
    testing.Equal(t, results[0].Status, TimedOut)
    testing.Equal(t, results[1].Status, Failed)
    testing.Contains(t, results[1].Detail, "bad input")
}

@test
async func TestWorkKeepsTheSentinel(t *testing.T) {
    try await Work(Job{Name: "x", Ms: 1, Fails: true}) catch err {
        testing.True(t, errors.Is(err, ErrBadInput), "ErrBadInput expected in the chain")
        return
    }
    t.Fatal("a failing job succeeded")
}

@test
async func TestCancelledBeforeStartIsSkipped(t *testing.T) {
    var h = task.NewCancel()
    h.Cancel()
    var (results, summary) = await RunBatch([quick(1), quick(2)], 1, 2, h.Context)
    // The feeder stops at once: nothing is submitted, nothing runs.
    testing.Equal(t, len(results), 0)
    testing.Equal(t, summary, "started 0, done 0, failed 0, timed out 0, cancelled 0, skipped 0")
}
```

## Output

<!-- file: expected.out -->
```text title="expected.out"
batch: 8 jobs, 3 workers, a queue of 2
  #1 resize-images  done       30ms of work
  #2 send-emails    done       10ms of work
  #3 build-index    timed out  deadline 50ms
  #4 parse-feed     failed     job parse-feed: bad input
  #5 backup-db      done       40ms of work
  #6 thumbnails     done       20ms of work
  #7 report         done       10ms of work
  #8 cleanup        done       10ms of work
  stats: started 8, done 6, failed 1, timed out 1, cancelled 0, skipped 0
cancelled run: 5 slow jobs, 3 workers, cancelled after 60ms
  #1 import-1       cancelled  stopped by the run
  #2 import-2       cancelled  stopped by the run
  #3 import-3       cancelled  stopped by the run
  #4 import-4       skipped    the run was cancelled before it started
  #5 import-5       skipped    the run was cancelled before it started
  stats: started 3, done 0, failed 0, timed out 0, cancelled 3, skipped 2
service: a job every 40ms until SIGTERM
  received SIGTERM: no new jobs, draining 4 submitted
  #1 tick-1         done       30ms of work
  #2 tick-2         done       30ms of work
  #3 tick-3         done       30ms of work
  #4 tick-4         done       30ms of work
  stats: started 4, done 4, failed 0, timed out 0, cancelled 0, skipped 0
```

## Next

- [Async programming](../../guide/async.md): tasks, channels, mutexes,
  cancellation and timers in detail.
- [bookshelf-api](../bookshelf-api/README.md): the same ideas in an HTTP
  server, with a graceful shutdown of real connections.
