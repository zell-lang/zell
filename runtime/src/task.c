/* task.c - thread-pool scheduler, tasks, timers, the per-task context slot
 * and small primitives for task (signals, spin locks, yield). Threads, the
 * CPU count and the clock come from the platform layer
 * (platform/platform.h); the I/O poller is in task_io.c, channels and
 * mutexes in task_sync.c. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* ======================================================================
 * Scheduler: N workers, each with a local run queue (a ring of LOCALQ
 * jobs pushed by its owner, popped by the owner and stolen in halves by
 * idle workers), a "run next" slot for the continuation a completing task
 * posts (it runs next on the same worker, without any queue), and one
 * global FIFO for posts from other threads (I/O, timers, main) and ring
 * overflow. ABI.md 10.1 describes the policy; the rules are Go's.
 * ====================================================================== */

/* One unit of work: fn(arg) run with the task context `ctx` as task `cur`
 * (both owned references that move with the record; ABI.md 10.8, 10.11). */
typedef struct jobrec {
    zell_resume_fn fn;
    void *arg;
    zell_object *ctx;
    zell_task *cur;
} jobrec;

/* A node of the global queue. */
typedef struct job {
    jobrec r;
    struct job *next;
} job;

#define LOCALQ 256                 /* ring slots per worker (a power of two) */
#define MAX_WORKERS 1024           /* core workers + spares, ever */
#define GLOBAL_EVERY 61            /* ticks between forced looks at the global queue */
#define RUNNEXT_STREAK 32          /* run-next hand-offs in a row before the ring gets a turn */
#define STEAL_PASSES 4             /* rounds over the victims; run-next only in the last */
#define RUNNEXT_GRACE_NS 20000     /* a victim keeps its run-next this long before a thief takes it */

enum { RN_EMPTY = 0, RN_FULL = 1, RN_BUSY = 2 };

typedef struct worker {
    /* The ring: the owner stores records at `tail`; the owner and thieves
     * take them at `head` with a compare-and-swap (a record is read before
     * the swap and only used when the swap succeeds; the owner never
     * overwrites a slot before `head` has passed it). Record fields are
     * accessed with relaxed atomics: a thief whose swap fails may have
     * read a slot the owner was refilling. */
    uint32_t head;                 /* atomic */
    uint32_t tail;                 /* atomic, written by the owner */
    jobrec ring[LOCALQ];
    /* The run-next slot: RN_EMPTY, RN_FULL, or RN_BUSY while someone copies
     * it out. Only the owner fills it. */
    int32_t rn_state;              /* atomic */
    jobrec runnext;
    /* Owner-only bookkeeping. */
    uint64_t tick;                 /* jobs taken */
    uint64_t rn_streak;            /* consecutive jobs taken from run-next */
    uint64_t rng;                  /* victim order */
    int spinning;                  /* holds the pool's spinning token */
    int64_t spin_until;            /* monotonic ns: when to give the token up and park */
    int spare;
    int alive;                     /* under q_mu: the slot is in use */
} worker;

static worker *workers[MAX_WORKERS];   /* slots, allocated once and reused by spares */
static int64_t q_nslots;               /* atomic: slots ever allocated */

static zell__plat_mutex q_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_cond q_cv = ZELL_PLAT_COND_INIT;     /* work available / stop */
static zell__plat_cond idle_cv = ZELL_PLAT_COND_INIT;  /* drained / thread exited */
static job *q_head, *q_tail;  /* the global queue, under q_mu */
static int64_t q_len;         /* atomic: its length (read without q_mu) */
static int q_stopping;
static int q_started;
static int64_t q_nworkers;    /* target parallelism N */
static int64_t q_alive;       /* worker threads alive (N core + spares), under q_mu */
static int64_t q_idle;        /* atomic: workers parked on q_cv */
static int64_t q_blocked;     /* workers blocked inside zell_task_await, under q_mu */
static int64_t q_spinning;    /* atomic: 1 while a worker polls for work before parking */
static int64_t q_wake_pending; /* under q_mu: wake-ups sent whose spinning token is not yet taken */

/* How long an out-of-work worker polls the queues before it parks on q_cv
 * (and at most one does at a time). A job posted meanwhile is taken
 * without a futex wake-up of a parked thread: hand-offs between tasks
 * (channels, mutexes, awaits) would otherwise pay a sleep/wake per job. */
#define SPIN_NS 50000

#ifdef ZELL_SCHED_STATS
static int64_t st_wake, st_spin, st_spin_found, st_steal, st_linger, st_park, st_recheck, st_jobs_rn, st_jobs_ring, st_jobs_glob, st_rn_kick;
#define STAT(x) __atomic_add_fetch(&st_##x, 1, __ATOMIC_RELAXED)
#else
#define STAT(x) ((void)0)
#endif

static _Thread_local int t_is_worker;
/* Posts from a non-worker thread that batches them (timer_main): the jobs
 * are collected here and pushed to the global queue in one lock round
 * trip by post_batch_flush. NULL on every other thread. */
typedef struct post_batch {
    job *head, *tail;
    int64_t n;
} post_batch;
static _Thread_local post_batch *t_post_batch;
static void post_batch_flush(post_batch *b);
static _Thread_local worker *t_w;   /* this thread's worker, NULL on other threads */

/* The context of the code running on this thread (owned reference, NULL =
 * empty). Jobs carry the context they were posted with; see ABI.md 10.8. */
static _Thread_local zell_object *t_ctx;

/* The task whose code runs on this thread (owned reference, NULL = none):
 * the parent of tasks created now and the target of cancellation hooks.
 * Travels with jobs and continuations like t_ctx; see ABI.md 10.11. */
static _Thread_local zell_task *t_cur;

/* ---- job records ---- */

static inline void rec_store(jobrec *dst, const jobrec *src) {
    __atomic_store_n(&dst->fn, src->fn, __ATOMIC_RELAXED);
    __atomic_store_n(&dst->arg, src->arg, __ATOMIC_RELAXED);
    __atomic_store_n(&dst->ctx, src->ctx, __ATOMIC_RELAXED);
    __atomic_store_n(&dst->cur, src->cur, __ATOMIC_RELAXED);
}

static inline void rec_load(jobrec *dst, const jobrec *src) {
    dst->fn = __atomic_load_n(&src->fn, __ATOMIC_RELAXED);
    dst->arg = __atomic_load_n(&src->arg, __ATOMIC_RELAXED);
    dst->ctx = __atomic_load_n(&src->ctx, __ATOMIC_RELAXED);
    dst->cur = __atomic_load_n(&src->cur, __ATOMIC_RELAXED);
}

/* ---- the global queue (q_mu held unless said otherwise) ---- */

static void global_push_locked(job *first, job *last, int64_t n) {
    last->next = NULL;
    if (q_tail) q_tail->next = first; else q_head = first;
    q_tail = last;
    __atomic_add_fetch(&q_len, n, __ATOMIC_SEQ_CST);
}

static job *global_pop_locked(void) {
    job *j = q_head;
    if (j) {
        q_head = j->next;
        if (!q_head) q_tail = NULL;
        __atomic_sub_fetch(&q_len, 1, __ATOMIC_SEQ_CST);
    }
    return j;
}

static inline bool global_nonempty(void) { return __atomic_load_n(&q_len, __ATOMIC_SEQ_CST) > 0; }

static job *job_node(const jobrec *r) {
    job *j = malloc(sizeof *j);
    if (!j) zell_panic("out of memory (job)");
    j->r = *r;
    j->next = NULL;
    return j;
}

/* ---- the local ring ---- */

static inline uint32_t ring_len(const worker *w) {
    uint32_t h = __atomic_load_n(&w->head, __ATOMIC_SEQ_CST);
    uint32_t t = __atomic_load_n(&w->tail, __ATOMIC_SEQ_CST);
    return t - h;
}

/* Owner: appends r; false when the ring is full. */
static bool ring_push(worker *w, const jobrec *r) {
    uint32_t h = __atomic_load_n(&w->head, __ATOMIC_ACQUIRE);
    uint32_t t = __atomic_load_n(&w->tail, __ATOMIC_RELAXED);
    if (t - h >= LOCALQ) return false;
    rec_store(&w->ring[t % LOCALQ], r);
    /* Sequentially consistent, like a parking worker's idle count: one of
     * the two sees the other (the wake-up protocol below). */
    __atomic_store_n(&w->tail, t + 1, __ATOMIC_SEQ_CST);
    return true;
}

/* Owner: appends r to a full ring by moving half of the ring and r to the
 * global queue (Go's runqputslow). */
static void ring_push_slow(worker *w, const jobrec *r) {
    for (;;) {
        if (ring_push(w, r)) return;
        uint32_t h = __atomic_load_n(&w->head, __ATOMIC_ACQUIRE);
        uint32_t t = __atomic_load_n(&w->tail, __ATOMIC_RELAXED);
        uint32_t n = (t - h) / 2;
        if (n == 0) continue;   /* thieves emptied it meanwhile: the push will succeed */
        jobrec batch[LOCALQ / 2];
        for (uint32_t i = 0; i < n; i++) rec_load(&batch[i], &w->ring[(h + i) % LOCALQ]);
        if (!__atomic_compare_exchange_n(&w->head, &h, h + n, false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) continue;
        job *first = job_node(&batch[0]), *last = first;
        for (uint32_t i = 1; i < n; i++) { last->next = job_node(&batch[i]); last = last->next; }
        last->next = job_node(r);
        last = last->next;
        zell__plat_mutex_lock(&q_mu);
        global_push_locked(first, last, (int64_t)n + 1);
        zell__plat_mutex_unlock(&q_mu);
        return;
    }
}

/* Owner or thief: takes the oldest record. */
static bool ring_pop(worker *w, jobrec *out) {
    for (;;) {
        uint32_t h = __atomic_load_n(&w->head, __ATOMIC_ACQUIRE);
        uint32_t t = __atomic_load_n(&w->tail, __ATOMIC_ACQUIRE);
        if (h == t) return false;
        rec_load(out, &w->ring[h % LOCALQ]);
        if (__atomic_compare_exchange_n(&w->head, &h, h + 1, false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) return true;
    }
}

/* Thief w (its own ring empty): moves half of v's ring into its ring and
 * returns the last stolen record in *out (Go's runqgrab). */
static bool steal_ring(worker *w, worker *v, jobrec *out) {
    for (;;) {
        uint32_t h = __atomic_load_n(&v->head, __ATOMIC_ACQUIRE);
        uint32_t t = __atomic_load_n(&v->tail, __ATOMIC_ACQUIRE);
        uint32_t n = t - h;
        n = n - n / 2;
        if (n == 0) return false;
        if (n > LOCALQ / 2) continue;   /* h and t read around a push and pops: inconsistent */
        uint32_t wt = __atomic_load_n(&w->tail, __ATOMIC_RELAXED);
        for (uint32_t i = 0; i < n; i++) {
            jobrec r;
            rec_load(&r, &v->ring[(h + i) % LOCALQ]);
            rec_store(&w->ring[(wt + i) % LOCALQ], &r);   /* beyond w's tail: invisible yet */
        }
        if (!__atomic_compare_exchange_n(&v->head, &h, h + n, false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) continue;
        rec_load(out, &w->ring[(wt + n - 1) % LOCALQ]);
        if (n > 1) __atomic_store_n(&w->tail, wt + n - 1, __ATOMIC_SEQ_CST);
        return true;
    }
}

/* ---- the run-next slot ---- */

static bool runnext_take(worker *w, jobrec *out) {
    int32_t s = RN_FULL;
    if (!__atomic_compare_exchange_n(&w->rn_state, &s, RN_BUSY, false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) return false;
    rec_load(out, &w->runnext);
    __atomic_store_n(&w->rn_state, RN_EMPTY, __ATOMIC_RELEASE);
    return true;
}

/* Owner: r runs next; a record already there goes behind the ring. */
static void runnext_put(worker *w, const jobrec *r) {
    for (;;) {
        int32_t s = __atomic_load_n(&w->rn_state, __ATOMIC_ACQUIRE);
        if (s == RN_EMPTY) break;
        jobrec old;
        if (s == RN_FULL && runnext_take(w, &old)) { STAT(rn_kick); ring_push_slow(w, &old); }
        /* RN_BUSY: a thief is copying it out */
    }
    rec_store(&w->runnext, r);
    __atomic_store_n(&w->rn_state, RN_FULL, __ATOMIC_SEQ_CST);
}

static inline bool runnext_full(const worker *w) { return __atomic_load_n(&w->rn_state, __ATOMIC_SEQ_CST) == RN_FULL; }

/* ---- wake-ups ---- */

/* Wakes one parked worker if nobody is polling for work. The woken worker
 * inherits the spinning token (q_spinning is raised here, on its behalf),
 * so posts made before it runs do not wake more workers (Go's wakep). */
static void wake_one(void) {
    if (__atomic_load_n(&q_idle, __ATOMIC_SEQ_CST) == 0 || __atomic_load_n(&q_spinning, __ATOMIC_SEQ_CST) != 0) return;
    zell__plat_mutex_lock(&q_mu);
    if (__atomic_load_n(&q_idle, __ATOMIC_RELAXED) - q_wake_pending > 0 && __atomic_load_n(&q_spinning, __ATOMIC_RELAXED) == 0) {
        __atomic_store_n(&q_spinning, 1, __ATOMIC_SEQ_CST);
        q_wake_pending++;
        STAT(wake);
        zell__plat_cond_signal(&q_cv);
    }
    zell__plat_mutex_unlock(&q_mu);
}

/* Whether the global queue or any ring holds work (run-next slots are
 * their owners' business: see lingering_runnext). */
static bool queued_work(void) {
    if (global_nonempty()) return true;
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < n; i++)
        if (ring_len(workers[i]) != 0) return true;
    return false;
}

/* Whether another worker's run-next slot is full (sequentially consistent
 * loads: the parking protocol). */
static bool any_runnext_full(worker *w) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < n; i++)
        if (workers[i] != w && runnext_full(workers[i])) return true;
    return false;
}

/* A run-next slot of another worker that stays full for the grace period
 * (its owner is busy with a long job rather than about to take it) is
 * taken by w: returns true with the record in *out. Owners normally take
 * their slot within nanoseconds, so this mostly waits a little and
 * returns false. */
static bool lingering_runnext(worker *w, jobrec *out) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < n; i++) {
        worker *v = workers[i];
        if (v == w || !runnext_full(v)) continue;
        int64_t until = zell__plat_mono_ns() + RUNNEXT_GRACE_NS;
        while (runnext_full(v) && zell__plat_mono_ns() < until) zell__plat_yield();
        if (runnext_take(v, out)) { STAT(linger); return true; }
    }
    return false;
}

/* ---- finding work (the owner of w) ---- */

static inline uint64_t rng_next(worker *w) {
    uint64_t x = w->rng;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
    w->rng = x;
    return x;
}

/* Takes a batch from the global queue: the first record in *out, the rest
 * (up to the ring's free space) into w's ring. */
static bool global_get(worker *w, jobrec *out) {
    if (!global_nonempty()) return false;
    zell__plat_mutex_lock(&q_mu);
    job *j = global_pop_locked();
    if (!j) { zell__plat_mutex_unlock(&q_mu); return false; }
    int64_t want = __atomic_load_n(&q_len, __ATOMIC_RELAXED) / (q_nworkers > 0 ? q_nworkers : 1);
    int64_t room = LOCALQ - (int64_t)ring_len(w);
    if (want > room) want = room;
    if (want > LOCALQ / 2) want = LOCALQ / 2;
    job *extra = NULL;
    for (int64_t i = 0; i < want; i++) {
        job *e = global_pop_locked();
        if (!e) break;
        e->next = extra;
        extra = e;
    }
    zell__plat_mutex_unlock(&q_mu);
    /* `extra` is reversed: push in FIFO order */
    job *fwd = NULL;
    while (extra) { job *n = extra->next; extra->next = fwd; fwd = extra; extra = n; }
    while (fwd) { job *n = fwd->next; ring_push_slow(w, &fwd->r); free(fwd); fwd = n; }
    *out = j->r;
    free(j);
    return true;
}

/* Steals from the other workers: halves of rings on every pass, a run-next
 * only on the last pass and only if its owner has not taken it within a
 * grace period (it usually does, within nanoseconds). */
static bool steal_work(worker *w, jobrec *out) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    if (n <= 1) return false;
    for (int pass = 0; pass < STEAL_PASSES; pass++) {
        bool last = pass == STEAL_PASSES - 1;
        uint64_t start = rng_next(w) % (uint64_t)n;
        for (int64_t i = 0; i < n; i++) {
            worker *v = workers[(start + (uint64_t)i) % (uint64_t)n];
            if (v != w && steal_ring(w, v, out)) { STAT(steal); return true; }
        }
        if (last && lingering_runnext(w, out)) return true;
    }
    return false;
}

static bool find_work(worker *w, jobrec *out) {
    w->tick++;
    if (w->tick % GLOBAL_EVERY == 0 && global_get(w, out)) { w->rn_streak = 0; return true; }
    /* Run-next first, unless it has had the worker to itself for a while
     * and the ring waits: then one ring job runs in between (run-next
     * cannot be preempted here, so this is its time slice). */
    bool ring_first = w->rn_streak >= RUNNEXT_STREAK;
    if (!ring_first && runnext_take(w, out)) { w->rn_streak++; STAT(jobs_rn); return true; }
    if (ring_pop(w, out)) { w->rn_streak = 0; STAT(jobs_ring); return true; }
    if (ring_first && runnext_take(w, out)) { w->rn_streak = 1; STAT(jobs_rn); return true; }
    if (global_get(w, out)) { w->rn_streak = 0; STAT(jobs_glob); return true; }
    if (steal_work(w, out)) { w->rn_streak = 0; return true; }
    return false;
}

/* Polls the global queue and the rings until w->spin_until (the spinning
 * token is held). Returns whether work showed up somewhere: in a ring or
 * the global queue, or a run-next slot nobody took for the grace period
 * (then it is already taken, in *out). */
static bool spin_for_work(worker *w, jobrec *out, bool *taken) {
    *taken = false;
    for (int i = 1;; i++) {
        if (queued_work()) return true;
        if ((i & 63) == 0) {
            if (zell__plat_mono_ns() >= w->spin_until) return false;
            if (lingering_runnext(w, out)) { *taken = true; return true; }
            zell__plat_yield();   /* a shared machine: let the poster run */
        }
    }
}

/* Drops the spinning token; with work still queued somewhere, hands it to
 * a parked worker (Go's resetspinning), so parallelism is kept up. */
static void stop_spinning(worker *w) {
    if (!w->spinning) return;
    w->spinning = 0;
    __atomic_store_n(&q_spinning, 0, __ATOMIC_SEQ_CST);
    if (ring_len(w) != 0 || global_nonempty()) wake_one();
}

static void run_job(const jobrec *j) {
    zell_object *saved = t_ctx;
    zell_task *saved_cur = t_cur;
    t_ctx = j->ctx;   /* the job's references move to the thread */
    t_cur = j->cur;
    j->fn(j->arg);
    /* Restore the thread-locals before dropping the job's references:
     * the release may free the task (or context) and run destructors
     * (a task result's Drop) that start tasks, which must not see a
     * dying object as their parent (that resurrected and double-freed
     * it). */
    zell_object *done_ctx = t_ctx;
    zell_task *done_cur = t_cur;
    t_ctx = saved;
    t_cur = saved_cur;
    zell_release(done_ctx);
    zell_release(done_cur);
}

static void *worker_main(void *arg) {
    worker *w = arg;
    t_w = w;
    t_is_worker = 1;
    zell__rc_thread_eligible();     /* biased refcounts (rc.c) */
    zell__plat_thread_stack_init(); /* frame walks of the CPU profiler */
    for (;;) {
        jobrec j;
        if (find_work(w, &j)) {
            stop_spinning(w);
            run_job(&j);
            continue;
        }
        /* Out of work. Take back pages other threads freed into (and
         * retire empty ones), so idle workers do not pin memory; merge
         * this thread's refcount queue (rc.c: destructors may run). */
        if (zell__heap_needs_collect()) { zell__heap_collect(0); continue; }
        if (zell__rc_pending()) { zell__rc_poll(); continue; }
        /* Poll before parking, if nobody else does (the token). */
        if (!w->spinning && q_nworkers > 1) {
            int64_t zero = 0;
            if (__atomic_compare_exchange_n(&q_spinning, &zero, 1, false, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)) {
                w->spinning = 1;
                w->spin_until = zell__plat_mono_ns() + SPIN_NS;
                STAT(spin);
            }
        }
        if (w->spinning) {
            bool taken;
            if (spin_for_work(w, &j, &taken)) {
                STAT(spin_found);
                if (taken) { stop_spinning(w); run_job(&j); }
                continue;   /* else find_work takes it; stop_spinning follows */
            }
        }
        /* Park. */
        zell__plat_mutex_lock(&q_mu);
        if (w->spinning) {
            w->spinning = 0;
            __atomic_store_n(&q_spinning, 0, __ATOMIC_SEQ_CST);
        }
        if (q_stopping) break;
        /* spare threads retire once enough unblocked workers exist again */
        if (w->spare && q_alive - q_blocked > q_nworkers) break;
        __atomic_add_fetch(&q_idle, 1, __ATOMIC_SEQ_CST);
        /* A poster stores its queue's tail (or its run-next state), then
         * reads q_idle; we raised q_idle and now read the queues: one of us
         * sees the other. A run-next slot is its owner's to take, but a job
         * that never yields (a loop that keeps finding completed tasks)
         * would starve it with every other worker parked and nobody
         * posting: so a full slot is given its grace period, outside the
         * lock, and stolen if the owner has not taken it by then. */
        if (queued_work() || any_runnext_full(w)) {
            STAT(recheck);
            __atomic_sub_fetch(&q_idle, 1, __ATOMIC_SEQ_CST);
            zell__plat_mutex_unlock(&q_mu);
            if (lingering_runnext(w, &j)) run_job(&j);
            continue;
        }
        if (__atomic_load_n(&q_idle, __ATOMIC_RELAXED) == q_alive) zell__plat_cond_broadcast(&idle_cv);  /* drained */
        zell__rc_park(0);
        STAT(park);
        zell__plat_cond_wait(&q_cv, &q_mu);
        __atomic_sub_fetch(&q_idle, 1, __ATOMIC_SEQ_CST);
        if (q_wake_pending > 0) {   /* the waker reserved the spinning token for us */
            q_wake_pending--;
            w->spinning = 1;
            w->spin_until = zell__plat_mono_ns() + SPIN_NS;
        }
        zell__plat_mutex_unlock(&q_mu);
        zell__rc_unpark();
    }
    /* q_mu held */
    w->alive = 0;
    q_alive--;
    zell__plat_cond_broadcast(&idle_cv);
    zell__plat_mutex_unlock(&q_mu);
    return NULL;
}

/* q_mu must be held. Returns false when no slot is left (MAX_WORKERS). */
static bool spawn_worker_locked(int spare) {
    int64_t n = __atomic_load_n(&q_nslots, __ATOMIC_RELAXED);
    worker *w = NULL;
    for (int64_t i = 0; i < n; i++) {
        if (!workers[i]->alive) { w = workers[i]; break; }
    }
    if (!w) {
        if (n >= MAX_WORKERS) return false;
        w = calloc(1, sizeof *w);
        if (!w) zell_panic("out of memory (worker)");
        w->rng = 0x9E3779B97F4A7C15ull ^ ((uint64_t)n + 1) * 0xBF58476D1CE4E5B9ull;
        workers[n] = w;
        __atomic_store_n(&q_nslots, n + 1, __ATOMIC_RELEASE);
    }
    w->alive = 1;
    w->spare = spare;
    w->spinning = 0;
    w->tick = w->rn_streak = 0;
    if (!zell__plat_thread_start_detached(worker_main, w)) zell_panic("cannot create worker thread");
    q_alive++;
    return true;
}

static int64_t worker_count(void) {
    const char *env = getenv("ZELL_WORKERS");
    long n = env ? strtol(env, NULL, 10) : 0;
    if (n <= 0) n = zell__plat_cpu_count();
    if (n <= 0) n = 1;
    if (n > 256) n = 256;
    return n;
}

static void timers_start(void);
static void timers_stop(void);

void (*zell__io_stop_fn)(void);
void (*zell__net_stop_fn)(void);

/* Stops the resolver pool and the I/O pollers if they ever started. */
static void stop_io(void) {
    if (zell__net_stop_fn) zell__net_stop_fn();
    if (zell__io_stop_fn) zell__io_stop_fn();
}

/* Process-wide setup (SIGPIPE, open-files limit), at most once. */
void zell__process_init(void) {
    static zell__plat_once proc_once = ZELL_PLAT_ONCE_INIT;
    zell__plat_once_run(&proc_once, zell__plat_process_init);
}

/* Set by zell__sched_start, so a program that never starts the scheduler
 * does not link the drain (sched_stop_started, nor what it pulls in). */
static void sched_stop_started(void);
static void (*g_sched_stop)(void);

/* Starts the workers and the timer thread. Called lazily, by the first
 * post, await or timer: a program that never starts a task stays single-
 * threaded (cheaper startup, non-atomic refcounts, ABI.md 3.2). */
void zell__sched_start(void) {
    g_sched_stop = sched_stop_started;
    zell__process_init();
    zell__plat_mutex_lock(&q_mu);
    if (q_started) { zell__plat_mutex_unlock(&q_mu); return; }
    q_stopping = 0;
    q_nworkers = worker_count();
    for (int64_t i = 0; i < q_nworkers; i++) spawn_worker_locked(0);
    __atomic_store_n(&q_started, 1, __ATOMIC_RELEASE);
    zell__plat_mutex_unlock(&q_mu);
    timers_start();
}

void zell__sched_stop(void) {
    if (g_sched_stop) g_sched_stop();
    else stop_io();
}

static void sched_stop_started(void) {
    zell__plat_mutex_lock(&q_mu);
    if (!q_started) {
        zell__plat_mutex_unlock(&q_mu);
        stop_io();   /* I/O waits may exist without a task ever running */
        return;
    }
    /* drain: every worker parked (their queues are then empty) and nothing global */
    while (__atomic_load_n(&q_idle, __ATOMIC_RELAXED) != q_alive || q_head) zell__plat_cond_wait(&idle_cv, &q_mu);
    q_stopping = 1;
    zell__plat_cond_broadcast(&q_cv);
    while (q_alive > 0) zell__plat_cond_wait(&idle_cv, &q_mu);
    zell__plat_mutex_unlock(&q_mu);
#ifdef ZELL_SCHED_STATS
    fprintf(stderr, "sched: wake %lld spin %lld spin_found %lld steal %lld linger %lld park %lld recheck %lld jobs rn %lld ring %lld glob %lld rn_kick %lld\n",
            (long long)st_wake, (long long)st_spin, (long long)st_spin_found, (long long)st_steal, (long long)st_linger, (long long)st_park, (long long)st_recheck,
            (long long)st_jobs_rn, (long long)st_jobs_ring, (long long)st_jobs_glob, (long long)st_rn_kick);
#endif
    timers_stop();
    stop_io();
    zell__plat_mutex_lock(&q_mu);
    q_nworkers = 0;
    q_wake_pending = 0;
    __atomic_store_n(&q_spinning, 0, __ATOMIC_SEQ_CST);
    __atomic_store_n(&q_started, 0, __ATOMIC_RELEASE);
    zell__plat_mutex_unlock(&q_mu);
}

/* Posts fn(arg) to run with context `ctx` as task `cur` (owned references,
 * moved). From a worker, `next` puts it in the run-next slot (a completed
 * task waking its awaiter: it runs as soon as the current job yields the
 * thread), else at the tail of the worker's ring; from any other thread
 * it goes to the global queue. */
static void sched_post_ctx(zell_resume_fn fn, void *arg, zell_object *ctx, zell_task *cur, bool next) {
    if (!fn) zell_panic_nil();
    if (!__atomic_load_n(&q_started, __ATOMIC_ACQUIRE)) zell__sched_start();
    jobrec r = { fn, arg, ctx, cur };
    worker *w = t_w;
    if (w) {
        if (next) runnext_put(w, &r);
        else ring_push_slow(w, &r);
        wake_one();
        return;
    }
    job *j = job_node(&r);
    if (t_post_batch) {
        if (t_post_batch->tail) t_post_batch->tail->next = j; else t_post_batch->head = j;
        t_post_batch->tail = j;
        t_post_batch->n++;
        return;
    }
    post_batch one = { j, j, 1 };
    post_batch_flush(&one);
}

/* Pushes a batch of jobs from a non-worker thread to the global queue in
 * one lock round trip. A polling worker takes them without a wake-up;
 * otherwise one parked worker is woken (none parked: every worker is busy
 * and will look), and it hands the spinning token on while work remains,
 * so the other idle workers follow. */
static void post_batch_flush(post_batch *b) {
    if (b->n == 0) return;
    zell__plat_mutex_lock(&q_mu);
    global_push_locked(b->head, b->tail, b->n);
    if (__atomic_load_n(&q_spinning, __ATOMIC_RELAXED) == 0 && __atomic_load_n(&q_idle, __ATOMIC_RELAXED) - q_wake_pending > 0) {
        __atomic_store_n(&q_spinning, 1, __ATOMIC_SEQ_CST);
        q_wake_pending++;
        STAT(wake);
        zell__plat_cond_signal(&q_cv);
    }
    zell__plat_mutex_unlock(&q_mu);
    b->head = b->tail = NULL;
    b->n = 0;
}

void zell_sched_post(zell_resume_fn fn, void *arg) {
    zell_retain(t_ctx);
    zell_retain(t_cur);
    sched_post_ctx(fn, arg, t_ctx, t_cur, false);
}

int64_t zell_sched_workers(void) {
    if (!__atomic_load_n(&q_started, __ATOMIC_ACQUIRE)) zell__sched_start();
    return q_nworkers;
}

bool zell_sched_is_worker(void) { return t_is_worker != 0; }

void zell_coro_resume(void *frame) {
    if (!frame) zell_panic_nil();
    /* LLVM switched-resume ABI: the first word of a coroutine frame is the resume fn. */
    void (*resume)(void *) = *(void (**)(void *))frame;
    resume(frame);
}


/* ======================================================================
 * Tasks
 * ====================================================================== */

enum { TASK_PENDING = 0, TASK_DONE = 1 };

typedef struct cont {
    zell_resume_fn fn;
    void *frame;
    zell_object *ctx;          /* owned: context of the registering code */
    zell_task *cur;            /* owned: current task of the registering code */
    struct cont *next;
} cont;

/* A wait to wake when its task is cancelled (10.11). */
typedef struct hook {
    void (*fn)(zell_task *w);
    zell_task *w;              /* owned, unless weak */
    struct hook *next;
    void (*fn_data)(zell_task *w, uintptr_t data);  /* instead of fn when set */
    uintptr_t data;
    int weak;                  /* w is the hook's own task: not retained */
} hook;

static inline void hook_free(hook *h) {
    if (!h->weak) zell_release(h->w);
    free(h);
}

struct zell_task {
    zell_object hdr;
    int32_t mu;                 /* spin lock (task_lock): short critical sections only */
    int32_t waiters;            /* atomic: threads blocked in zell_task_await */
    int32_t state;              /* atomic */
    int32_t _pad;
    int64_t result_size;
    const zell_value_ops *ops; /* applied at slot + ops_offset */
    int64_t ops_offset;
    cont *conts;                /* LIFO; reversed when fired */
    /* The first continuation lives in the task (the common case is one
     * awaiter, so no node is allocated); cont0.fn is NULL while unused. */
    cont cont0;
    /* Task tree for cancellation (10.11), guarded by mu (children lists by
     * the parent's mu). Only tasks made by zell_task_new / scopes have it. */
    zell_task *parent;         /* owned until the task is freed */
    zell_task *child_head;     /* children (not owned) */
    zell_task *sib_prev, *sib_next;
    hook *hooks;
    int32_t cancelled;          /* atomic */
    int32_t nhooks;
    /* The caller's context when a coroutine started (zell_task_new; owned,
     * valid while ctx_saved): put back when the coroutine first suspends or
     * completes, so context changes made inside it (task context values,
     * diag spans) never leak into the caller (10.8). */
    zell_object *caller_ctx;
    int32_t ctx_saved;
    int32_t dbg_id;             /* debug agent record (debug.c), 0 = none */
    /* The pending zell_task_delay timer that completes this task, guarded
     * by tm_mu (NULL once it fired or was removed): lets rt_fire drop the
     * timer in O(log n) instead of scanning all timers. */
    struct timer *timer;
    unsigned char slot[];       /* result_size bytes (8-aligned) */
};

static int64_t g_live_tasks; /* diag */

/* A task's lock: held for a few loads and stores (never across a wait or a
 * call into user code that could take it again), so a spin lock with a
 * yield is cheaper than a pthread mutex, which every async call would
 * otherwise initialise, lock and destroy. Threads blocking on a task
 * (zell_task_await) wait on one process-wide condition instead of a
 * per-task one; completion wakes them only when the task has waiters. */
static inline void task_lock(zell_task *t) {
    while (__atomic_exchange_n(&t->mu, 1, __ATOMIC_ACQUIRE)) {
        while (__atomic_load_n(&t->mu, __ATOMIC_RELAXED)) zell__plat_yield();
    }
}

static inline void task_unlock(zell_task *t) { __atomic_store_n(&t->mu, 0, __ATOMIC_RELEASE); }

static zell__plat_mutex g_await_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_cond g_await_cv = ZELL_PLAT_COND_INIT;

int64_t zell__live_tasks(void) { return __atomic_load_n(&g_live_tasks, __ATOMIC_RELAXED); }

int64_t zell__sched_queue_len(void) {
    int64_t n = __atomic_load_n(&q_len, __ATOMIC_SEQ_CST);
    int64_t slots = __atomic_load_n(&q_nslots, __ATOMIC_ACQUIRE);
    for (int64_t i = 0; i < slots; i++) n += ring_len(workers[i]) + (runnext_full(workers[i]) ? 1 : 0);
    return n;
}

static void task_drop(void *obj) {
    zell_task *t = obj;
    __atomic_sub_fetch(&g_live_tasks, 1, __ATOMIC_RELAXED);
    if (t->state == TASK_DONE && t->ops && t->ops->release)
        t->ops->release(t->slot + t->ops_offset);
    cont *c = t->conts;
    while (c) { cont *n = c->next; zell_release(c->ctx); zell_release(c->cur); free(c); c = n; }
    if (t->cont0.fn) { zell_release(t->cont0.ctx); zell_release(t->cont0.cur); }
    hook *h = t->hooks;
    while (h) { hook *n = h->next; hook_free(h); h = n; }
    if (t->ctx_saved) zell_release(t->caller_ctx);
    if (t->dbg_id) zell__dbg_task_drop(t->dbg_id);
    if (t->parent) {
        zell_task *p = t->parent;
        task_lock(p);
        if (t->sib_prev) t->sib_prev->sib_next = t->sib_next; else p->child_head = t->sib_next;
        if (t->sib_next) t->sib_next->sib_prev = t->sib_prev;
        task_unlock(p);
        zell_release(p);
    }
}

static const zell_type_info task_type = { "Task", 0, task_drop };

static zell_task *task_new_ex(int64_t result_size, const zell_value_ops *ops, int64_t ops_off) {
    if (result_size < 0) zell__panicf("zell_task_new: negative result size");
    zell_task *t = zell_object_new(&task_type,
                                     (int64_t)sizeof(zell_task) + ZELL_ROUND8(result_size));
    /* Tasks move between threads all the time (awaiters, continuations,
     * jobs): biasing them to one thread would only queue merges (rc.c). */
    t->hdr.refcount |= ZELL_RC_NOBIAS;
    __atomic_add_fetch(&g_live_tasks, 1, __ATOMIC_RELAXED);
    t->result_size = result_size;
    t->ops = ops;
    t->ops_offset = ops_off;
    return t;
}

/* Links t under the current task (or `parent`), inheriting cancellation. */
static void task_attach(zell_task *t, zell_task *parent) {
    if (!parent) return;
    zell_retain(parent);
    t->parent = parent;
    task_lock(parent);
    t->sib_next = parent->child_head;
    if (parent->child_head) parent->child_head->sib_prev = t;
    parent->child_head = t;
    if (__atomic_load_n(&parent->cancelled, __ATOMIC_ACQUIRE)) __atomic_store_n(&t->cancelled, 1, __ATOMIC_RELEASE);
    task_unlock(parent);
}

/* The task of the code that runs now: a child of the current task, and the
 * current task from here on (until it suspends or completes). */
zell_task *zell_task_new(int64_t result_size, const zell_value_ops *result_ops) {
    zell_task *t = task_new_ex(result_size, result_ops, 0);
    task_attach(t, t_cur);
    zell_retain(t_ctx);
    t->caller_ctx = t_ctx;
    t->ctx_saved = 1;
    zell_retain(t);
    zell_task *old = t_cur;
    t_cur = t;
    zell_release(old);  /* t->parent holds its own reference */
    return t;
}

zell_task *zell_task_new_plain(int64_t result_size, const zell_value_ops *result_ops) {
    return task_new_ex(result_size, result_ops, 0);
}

/* task_sync.c (ABI.md 10.6, 10.7): a plain task whose slot has room for
 * slot_bytes (at least result_size) so a pending wait can keep its queue
 * links and a sender its value there; ops apply at slot + ops_off. */
zell_task *zell__task_new_slot(int64_t result_size, const zell_value_ops *ops, int64_t ops_off, int64_t slot_bytes) {
    if (result_size < 0 || slot_bytes < result_size) zell__panicf("zell__task_new_slot: bad sizes");
    zell_task *t = zell_object_new(&task_type, (int64_t)sizeof(zell_task) + ZELL_ROUND8(slot_bytes));
    t->hdr.refcount |= ZELL_RC_NOBIAS;
    __atomic_add_fetch(&g_live_tasks, 1, __ATOMIC_RELAXED);
    t->result_size = result_size;
    t->ops = ops;
    t->ops_offset = ops_off;
    return t;
}

void *zell__task_slot(zell_task *t) { return t->slot; }
void zell__task_lock(zell_task *t) { task_lock(t); }
void zell__task_unlock(zell_task *t) { task_unlock(t); }

/* Completes a task nobody else has seen yet, whose result is already in
 * its slot (no waiters, no continuations, not the current task). */
void zell__task_mark_done(zell_task *t) { __atomic_store_n(&t->state, TASK_DONE, __ATOMIC_RELEASE); }

/* The immortal completed Task<void>: what an operation that did not have
 * to wait returns (Mutex.Lock of a free mutex, Delay(0), a Send that found
 * room); awaiting it takes the is_done fast path, retain and release are
 * no-ops. */
static struct zell_task done_task = {
    .hdr = { ZELL_RC_IMMORTAL, &task_type }, .state = TASK_DONE,
};

zell_task *zell_task_done(void) { return &done_task; }

/* The current task stops running on this thread: its parent is current. */
static void restore_parent(zell_task *t) {
    if (t_cur != t) return;
    if (t->ctx_saved) {
        /* Back on the caller's stack (first suspension or completion): the
         * caller's context too. The task's own context lives on in its
         * continuations. */
        zell_object *old = t_ctx;
        t_ctx = t->caller_ctx;
        t->caller_ctx = NULL;
        t->ctx_saved = 0;
        zell_release(old);
    }
    zell_retain(t->parent);
    t_cur = t->parent;
    zell_release(t);
}

/* Completes t; returns false (and does nothing) if it already was done. */
static bool task_complete(zell_task *t, const void *result) {
    if (!t) zell_panic_nil();
    /* A waiter that sees DONE may drop the last reference at once, while
     * this thread still writes t (conts, the lock word, t_cur): hold one
     * until the task is left (the drop then runs on whichever thread lets
     * go last, ordered by the count's acquire-release decrement). */
    zell_retain(t);
    task_lock(t);
    if (t->state == TASK_DONE) {
        task_unlock(t);
        zell_release(t);
        return false;
    }
    if (result && t->result_size > 0) {
        memcpy(t->slot, result, (size_t)t->result_size);
        if (t->ops && t->ops->retain) t->ops->retain(t->slot + t->ops_offset);
    }
    __atomic_store_n(&t->state, TASK_DONE, __ATOMIC_SEQ_CST);
    cont *c = t->conts, *rev = NULL;
    t->conts = NULL;
    cont first = t->cont0;   /* copied out: t may be freed once unlocked */
    t->cont0.fn = NULL;
    /* A waiter increments `waiters` before it checks the state (both
     * sequentially consistent), and checks it under g_await_mu, which the
     * broadcast takes: it either sees DONE or is woken. */
    if (__atomic_load_n(&t->waiters, __ATOMIC_SEQ_CST)) {
        zell__plat_mutex_lock(&g_await_mu);
        zell__plat_cond_broadcast(&g_await_cv);
        zell__plat_mutex_unlock(&g_await_mu);
    }
    task_unlock(t);
    restore_parent(t);  /* a coroutine returning: the code after it is its parent's */
    zell_release(t);
    /* After this point `t` may be freed; use only `first` and `rev`. */
    while (c) { cont *n = c->next; c->next = rev; rev = c; c = n; }
    /* The first continuation runs next on this worker (the common case is
     * one awaiter); the others queue behind the ring in registration order. */
    bool next = true;
    if (first.fn) {
        sched_post_ctx(first.fn, first.frame, first.ctx, first.cur, true);
        next = false;
    }
    while (rev) {
        cont *n = rev->next;
        sched_post_ctx(rev->fn, rev->frame, rev->ctx, rev->cur, next);
        next = false;
        free(rev);
        rev = n;
    }
    return true;
}

void zell_task_complete(zell_task *t, const void *result) {
    if (!task_complete(t, result)) zell_panic("task completed twice");
}

bool zell_task_try_complete(zell_task *t, const void *result) {
    return task_complete(t, result);
}

bool zell_task_is_done(const zell_task *t) {
    if (!t) zell_panic_nil();
    return __atomic_load_n(&t->state, __ATOMIC_ACQUIRE) == TASK_DONE;
}

/* `suspend`: the registering coroutine suspends when this succeeds. Its
 * task's saved caller context is then restored on this thread *before* the
 * continuation is published: once it is, the continuation may resume on
 * another thread, whose restore_parent must not race with ours on the
 * task's ctx_saved/caller_ctx. */
static bool add_cont(zell_task *t, zell_resume_fn fn, void *frame, bool suspend) {
    if (!t || !fn) zell_panic_nil();
    if (zell_task_is_done(t)) return false;
    task_lock(t);
    if (t->state == TASK_DONE) {
        task_unlock(t);
        return false;
    }
    cont *c;
    if (!t->cont0.fn) {
        c = &t->cont0;   /* the first continuation: no allocation */
    } else {
        c = malloc(sizeof *c);
        if (!c) zell_panic("out of memory (continuation)");
        c->next = t->conts;
        t->conts = c;
    }
    c->fn = fn;
    c->frame = frame;
    c->ctx = t_ctx;
    c->cur = t_cur;
    zell_retain(c->ctx);
    zell_retain(c->cur);
    zell_object *drop_ctx = NULL;
    zell_task *cur = t_cur;
    if (suspend && cur && cur->ctx_saved) {
        /* restore_parent's context step (the continuation holds the task's
         * context now, so the old t_ctx is released after unlocking). */
        drop_ctx = t_ctx;
        t_ctx = cur->caller_ctx;
        cur->caller_ctx = NULL;
        cur->ctx_saved = 0;
    }
    task_unlock(t);
    zell_release(drop_ctx);
    return true;
}

/* Public: the registering code (a coroutine) suspends when this returns
 * true, so the current task becomes its parent again. */
bool zell_task_add_continuation(zell_task *t, zell_resume_fn fn, void *frame) {
    if (!add_cont(t, fn, frame, true)) return false;
    if (t_cur) restore_parent(t_cur);
    return true;
}

void *zell_task_result(const zell_task *t) {
    if (!zell_task_is_done(t)) zell_panic("task result read before completion");
    return (void *)t->slot;
}

void *zell_task_await(zell_task *t) {
    if (!t) zell_panic_nil();
    if (zell_task_is_done(t)) return t->slot;
    if (t_is_worker) {
        /* Blocking a worker: its queued work must not wait with it. The
         * run-next slot goes to the ring, where thieves find it, and a
         * parked worker is woken to steal (the ring may hold the very job
         * that completes t). Then keep N runnable threads by starting a
         * spare worker if nobody is idle (it retires later). */
        worker *w = t_w;
        jobrec rn;
        if (runnext_take(w, &rn)) ring_push_slow(w, &rn);
        if (ring_len(w) != 0) wake_one();
        zell__plat_mutex_lock(&q_mu);
        q_blocked++;
        if (__atomic_load_n(&q_idle, __ATOMIC_RELAXED) == 0 && !q_stopping) spawn_worker_locked(1);
        zell__plat_mutex_unlock(&q_mu);
    }
    int parked = __atomic_load_n(&t->state, __ATOMIC_ACQUIRE) != TASK_DONE;
    if (parked) zell__rc_park(1);   /* others may merge our refcount queue (rc.c) */
    __atomic_add_fetch(&t->waiters, 1, __ATOMIC_SEQ_CST);
    zell__plat_mutex_lock(&g_await_mu);
    while (__atomic_load_n(&t->state, __ATOMIC_SEQ_CST) != TASK_DONE) zell__plat_cond_wait(&g_await_cv, &g_await_mu);
    zell__plat_mutex_unlock(&g_await_mu);
    __atomic_sub_fetch(&t->waiters, 1, __ATOMIC_SEQ_CST);
    if (parked) zell__rc_unpark();
    if (t_is_worker) {
        zell__plat_mutex_lock(&q_mu);
        q_blocked--;
        zell__plat_mutex_unlock(&q_mu);
    }
    return t->slot;
}

typedef struct spawn_ctx {
    zell_task_fn fn;
    zell_task *task;
    void *ctx;
} spawn_ctx;
/* (The spawned job inherits the spawner's task context through
 * zell_sched_post.) */

static void spawn_tramp(void *arg) {
    spawn_ctx *sc = arg;
    zell_task_fn fn = sc->fn;
    zell_task *t = sc->task;
    void *ctx = sc->ctx;
    free(sc);
    fn(t, ctx);
    zell_release(t);  /* the job's reference */
}

zell_task *zell_task_spawn_ex(zell_task_fn fn, void *ctx, int64_t result_size,
                                const zell_value_ops *result_ops) {
    if (!fn) zell_panic_nil();
    zell_task *t = task_new_ex(result_size, result_ops, 0);
    spawn_ctx *sc = malloc(sizeof *sc);
    if (!sc) zell_panic("out of memory (spawn)");
    sc->fn = fn;
    sc->task = t;
    sc->ctx = ctx;
    zell_retain(t);  /* held by the job until fn returns */
    zell_sched_post(spawn_tramp, sc);
    return t;
}

zell_task *zell_task_spawn(zell_task_fn fn, void *ctx) {
    return zell_task_spawn_ex(fn, ctx, 8, NULL);
}

/* ======================================================================
 * Timers (zell_task_delay): one thread, a binary min-heap on (deadline,
 * seq) so equal deadlines fire in insertion order, monotonic clock.
 * Insert, pop and removal (rt_fire of a pending delay) are O(log n).
 * ====================================================================== */

typedef struct timer {
    int64_t deadline;   /* monotonic ns */
    uint64_t seq;       /* insertion order, breaks deadline ties */
    int64_t idx;        /* position in tm_heap */
    zell_task *task;   /* owned: completed at the deadline, or */
    zell_task *cancel; /* owned: cancelled at the deadline (zell_task_set_deadline) */
    struct timer *next; /* batch of due timers in timer_main */
} timer;

static zell__plat_mutex tm_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_cond tm_cv;
static timer **tm_heap;           /* guarded by tm_mu */
static int64_t tm_n, tm_cap;
static uint64_t tm_seq;
static int tm_running, tm_stop;
static zell__plat_thread tm_thread;

static bool tm_less(const timer *a, const timer *b) {
    return a->deadline < b->deadline || (a->deadline == b->deadline && a->seq < b->seq);
}

static void tm_set(int64_t i, timer *t) { tm_heap[i] = t; t->idx = i; }

static void tm_sift_up(int64_t i) {
    timer *t = tm_heap[i];
    while (i > 0) {
        int64_t p = (i - 1) / 2;
        if (!tm_less(t, tm_heap[p])) break;
        tm_set(i, tm_heap[p]);
        i = p;
    }
    tm_set(i, t);
}

static void tm_sift_down(int64_t i) {
    timer *t = tm_heap[i];
    for (;;) {
        int64_t c = 2 * i + 1;
        if (c >= tm_n) break;
        if (c + 1 < tm_n && tm_less(tm_heap[c + 1], tm_heap[c])) c++;
        if (!tm_less(tm_heap[c], t)) break;
        tm_set(i, tm_heap[c]);
        i = c;
    }
    tm_set(i, t);
}

/* tm_mu held. */
static void tm_push(timer *t) {
    if (tm_n == tm_cap) {
        int64_t cap = tm_cap ? tm_cap * 2 : 64;
        timer **h = realloc(tm_heap, sizeof *h * (size_t)cap);
        if (!h) zell_panic("out of memory (timer)");
        tm_heap = h;
        tm_cap = cap;
    }
    t->seq = tm_seq++;
    tm_set(tm_n++, t);
    tm_sift_up(t->idx);
}

/* Removes t from the heap (tm_mu held); clears its task's back pointer. */
static void tm_remove(timer *t) {
    int64_t i = t->idx;
    timer *last = tm_heap[--tm_n];
    if (last != t) {
        tm_set(i, last);
        if (i > 0 && tm_less(last, tm_heap[(i - 1) / 2])) tm_sift_up(i);
        else tm_sift_down(i);
    }
    if (t->task) __atomic_store_n(&t->task->timer, NULL, __ATOMIC_RELEASE);
}

static void *timer_main(void *unused) {
    (void)unused;
    zell__plat_mutex_lock(&tm_mu);
    while (!tm_stop) {
        if (tm_n == 0) { zell__plat_cond_wait(&tm_cv, &tm_mu); continue; }
        int64_t now = zell__plat_mono_ns();
        int64_t left = tm_heap[0]->deadline - now;
        if (left > 0) {
            zell__plat_cond_wait_ns(&tm_cv, &tm_mu, left);
            continue;
        }
        /* Take every due timer in one go (one lock round trip per batch). */
        timer *due = NULL, **tail = &due;
        while (tm_n > 0 && tm_heap[0]->deadline <= now) {
            timer *t = tm_heap[0];
            tm_remove(t);
            t->next = NULL;
            *tail = t;
            tail = &t->next;
        }
        zell__plat_mutex_unlock(&tm_mu);
        /* The continuations of the due timers are posted as one batch: one
         * global-queue lock round trip per batch instead of one per timer. */
        post_batch batch = { NULL, NULL, 0 };
        t_post_batch = &batch;
        while (due) {
            timer *t = due;
            due = t->next;
            if (t->task) zell_task_try_complete(t->task, NULL);  /* may race with rt_fire */
            if (t->cancel) zell_task_cancel(t->cancel);
            zell_release(t->task);
            zell_release(t->cancel);
            free(t);
        }
        t_post_batch = NULL;
        post_batch_flush(&batch);
        zell__plat_mutex_lock(&tm_mu);
    }
    /* Pending timers at shutdown are dropped without completing. */
    while (tm_n > 0) {
        timer *t = tm_heap[0];
        tm_remove(t);
        zell_release(t->task);
        zell_release(t->cancel);
        free(t);
    }
    free(tm_heap);
    tm_heap = NULL;
    tm_cap = 0;
    zell__plat_mutex_unlock(&tm_mu);
    return NULL;
}

static void timers_start(void) {
    zell__plat_mutex_lock(&tm_mu);
    if (!tm_running) {
        zell__plat_cond_init(&tm_cv);  /* monotonic timed waits where available */
        tm_stop = 0;
        if (!zell__plat_thread_start(&tm_thread, timer_main, NULL))
            zell_panic("cannot create timer thread");
        tm_running = 1;
    }
    zell__plat_mutex_unlock(&tm_mu);
}

static void timers_stop(void) {
    zell__plat_mutex_lock(&tm_mu);
    if (!tm_running) { zell__plat_mutex_unlock(&tm_mu); return; }
    tm_stop = 1;
    zell__plat_cond_signal(&tm_cv);
    zell__plat_mutex_unlock(&tm_mu);
    zell__plat_thread_join(tm_thread);
    zell__plat_mutex_lock(&tm_mu);
    tm_running = 0;
    zell__plat_cond_destroy(&tm_cv);
    zell__plat_mutex_unlock(&tm_mu);
}

static void cancel_hook(void (*fn)(zell_task *), zell_task *w);
static void timer_insert(int64_t ms, zell_task *t, zell_task *cancel);

static void delay_cancelled(zell_task *w) { zell_task_rt_fire(w); }

/* A delay completes early when the current task is cancelled (10.11). */
zell_task *zell_task_delay(int64_t ms) {
    if (ms <= 0) return zell_task_done();
    zell_task *t = task_new_ex(0, NULL, 0);
    timer_insert(ms, t, NULL);
    cancel_hook(delay_cancelled, t);
    return t;
}

void zell_task_set_deadline(zell_task *t, int64_t ms) {
    if (!t) zell_panic_nil();
    if (ms <= 0) { zell_task_cancel(t); return; }
    timer_insert(ms, NULL, t);
}

/* Adds a timer for t (completed) or cancel (cancelled); both retained. */
static void timer_insert(int64_t ms, zell_task *t, zell_task *cancel) {
    if (!__atomic_load_n(&q_started, __ATOMIC_ACQUIRE)) zell__sched_start();
    timer *tm = malloc(sizeof *tm);
    if (!tm) zell_panic("out of memory (timer)");
    tm->deadline = zell__plat_mono_ns() + ms * 1000000;
    zell_retain(t);
    zell_retain(cancel);
    tm->task = t;
    tm->cancel = cancel;
    zell__plat_mutex_lock(&tm_mu);
    tm_push(tm);
    if (t) __atomic_store_n(&t->timer, tm, __ATOMIC_RELEASE);
    /* Only a new earliest deadline changes how long the timer thread sleeps. */
    if (tm->idx == 0) zell__plat_cond_signal(&tm_cv);
    zell__plat_mutex_unlock(&tm_mu);
}

/* ======================================================================
 * Combinators
 * ====================================================================== */

typedef struct all_state {
    zell_task *result;    /* owned */
    int64_t remaining;     /* atomic */
} all_state;

static void all_one_done(void *arg) {
    all_state *st = arg;
    if (__atomic_sub_fetch(&st->remaining, 1, __ATOMIC_ACQ_REL) == 0) {
        zell_task_complete(st->result, NULL);
        zell_release(st->result);
        free(st);
    }
}

zell_task *zell_task_all(zell_task *const *tasks, int64_t n) {
    zell_task *r = task_new_ex(0, NULL, 0);
    all_state *st = malloc(sizeof *st);
    if (!st) zell_panic("out of memory (task_all)");
    zell_retain(r);
    st->result = r;
    st->remaining = n + 1;  /* +1 guard released below */
    for (int64_t i = 0; i < n; i++) {
        if (!tasks[i]) zell_panic_nil();
        if (!add_cont(tasks[i], all_one_done, st, false)) all_one_done(st);
    }
    all_one_done(st);
    return r;
}

typedef struct any_state {
    zell_task *result;    /* owned */
    int64_t refs;          /* atomic: outstanding callbacks + guard */
    int32_t fired;         /* atomic */
    int64_t n;
    zell_task **inputs;   /* owned: the losers are cancelled (10.11) */
} any_state;

typedef struct any_node {
    any_state *st;
    int64_t index;
} any_node;

static void any_unref(any_state *st) {
    if (__atomic_sub_fetch(&st->refs, 1, __ATOMIC_ACQ_REL) == 0) {
        zell_release(st->result);
        for (int64_t i = 0; i < st->n; i++) zell_release(st->inputs[i]);
        free(st->inputs);
        free(st);
    }
}

static void any_one_done(void *arg) {
    any_node *nd = arg;
    any_state *st = nd->st;
    int64_t idx = nd->index;
    free(nd);
    if (__atomic_exchange_n(&st->fired, 1, __ATOMIC_ACQ_REL) == 0) {
        zell_task_complete(st->result, &idx);
        for (int64_t i = 0; i < st->n; i++)
            if (i != idx && !zell_task_is_done(st->inputs[i])) zell_task_cancel(st->inputs[i]);
    }
    any_unref(st);
}

zell_task *zell_task_any(zell_task *const *tasks, int64_t n) {
    if (n <= 0) zell_panic("task.Any called with no tasks");
    zell_task *r = task_new_ex(8, NULL, 0);
    any_state *st = malloc(sizeof *st);
    if (!st) zell_panic("out of memory (task_any)");
    zell_retain(r);
    st->result = r;
    st->refs = n + 1;
    st->fired = 0;
    st->n = n;
    st->inputs = malloc(sizeof *st->inputs * (size_t)n);
    if (!st->inputs) zell_panic("out of memory (task_any)");
    for (int64_t i = 0; i < n; i++) {
        if (!tasks[i]) zell_panic_nil();
        zell_retain(tasks[i]);
        st->inputs[i] = tasks[i];
    }
    for (int64_t i = 0; i < n; i++) {
        any_node *nd = malloc(sizeof *nd);
        if (!nd) zell_panic("out of memory (task_any)");
        nd->st = st;
        nd->index = i;
        if (!add_cont(tasks[i], any_one_done, nd, false)) any_one_done(nd);
    }
    any_unref(st);
    return r;
}

/* ======================================================================
 * Task context (ABI.md 10.8): one owned zell_object pointer per thread,
 * carried by every job and continuation, so a coroutine sees the context
 * of the code that started it after every await.
 * ====================================================================== */

zell_object *zell_task_get_ctx(void) {
    zell_retain(t_ctx);
    return t_ctx;
}

void zell_task_set_ctx(zell_object *ctx) {
    zell_retain(ctx);
    zell_object *old = t_ctx;
    t_ctx = ctx;
    zell_release(old);
}

/* ======================================================================
 * Primitives for task written in Zell (Channel, Mutex, WaitGroup,
 * Timeout, Spawn): signals, cancellable timers, spin locks, yield.
 * ====================================================================== */

zell_task *zell_task_rt_waker(void) { return task_new_ex(0, NULL, 0); }

/* Completes a signal (no-op if already done). If it is a pending
 * zell_task_delay timer, the timer is cancelled first. */
void zell_task_rt_fire(zell_task *t) {
    if (!t) zell_panic_nil();
    timer *found = NULL;
    /* t->timer is set before a delay task is handed out and only cleared
     * afterwards, so NULL here means no pending timer: signals, wakers and
     * fired delays skip the global timer lock. */
    if (__atomic_load_n(&t->timer, __ATOMIC_ACQUIRE)) {
        zell__plat_mutex_lock(&tm_mu);
        found = __atomic_load_n(&t->timer, __ATOMIC_ACQUIRE);
        if (found) tm_remove(found);
        zell__plat_mutex_unlock(&tm_mu);
    }
    zell_task_try_complete(t, NULL);
    if (found) {
        zell_release(found->task);
        free(found);
    }
}

void zell_task_spin_lock(int64_t *slot) {
    if (!slot) zell_panic_nil();
    for (int spins = 0;; spins++) {
        if (__atomic_load_n(slot, __ATOMIC_RELAXED) == 0 &&
            __atomic_exchange_n(slot, 1, __ATOMIC_ACQUIRE) == 0)
            return;
        if (spins > 64) zell__plat_yield();
    }
}

void zell_task_spin_unlock(int64_t *slot) {
    if (!slot) zell_panic_nil();
    __atomic_store_n(slot, 0, __ATOMIC_RELEASE);
}

static void yield_job(void *arg) {
    zell_task *t = arg;
    zell_task_complete(t, NULL);
    zell_release(t);
}

zell_task *zell_task_yield(void) {
    zell_task *t = task_new_ex(0, NULL, 0);
    zell_retain(t);  /* held by the job */
    zell_sched_post(yield_job, t);
    return t;
}

/* ======================================================================
 * Cancellation (ABI.md 10.11): a tree of tasks (parent = the current task
 * when zell_task_new ran), a cancelled flag per task inherited by new
 * children, and per-task hooks that wake the waits of a cancelled task.
 * ====================================================================== */

/* Collects (moves) the hooks of t and its descendants and flags them; locks
 * are taken parent before child. */
static void cancel_collect(zell_task *t, hook **out) {
    task_lock(t);
    if (__atomic_exchange_n(&t->cancelled, 1, __ATOMIC_ACQ_REL) == 0 || t->hooks) {
        hook *h = t->hooks;
        while (h) { hook *n = h->next; h->next = *out; *out = h; h = n; }
        t->hooks = NULL;
        t->nhooks = 0;
    }
    for (zell_task *c = t->child_head; c; c = c->sib_next) cancel_collect(c, out);
    task_unlock(t);
}

void zell_task_cancel(zell_task *t) {
    if (!t) zell_panic_nil();
    hook *hs = NULL;
    cancel_collect(t, &hs);
    while (hs) {
        hook *n = hs->next;
        if (hs->fn_data) hs->fn_data(hs->w, hs->data);
        else hs->fn(hs->w);
        hook_free(hs);
        hs = n;
    }
}

bool zell_task_is_cancelled(const zell_task *t) {
    return t && __atomic_load_n(&t->cancelled, __ATOMIC_ACQUIRE) != 0;
}

bool zell_task_cur_cancelled(void) { return zell_task_is_cancelled(t_cur); }

/* Registers fn(w) to run when task t is cancelled (at once if it already
 * is). Hooks of waits that completed meanwhile are pruned. A weak hook
 * (w == t, a wait that watches its own cancellation) holds no reference. */
static void cancel_hook_on(zell_task *t, void (*fn)(zell_task *), void (*fn_data)(zell_task *, uintptr_t), uintptr_t data, zell_task *w, int weak) {
    if (!t) return;
    task_lock(t);
    if (__atomic_load_n(&t->cancelled, __ATOMIC_ACQUIRE)) {
        task_unlock(t);
        if (fn_data) fn_data(w, data);
        else fn(w);
        return;
    }
    if (t->nhooks >= 8) {
        hook **pp = &t->hooks;
        while (*pp) {
            hook *h = *pp;
            if (zell_task_is_done(h->w)) {
                *pp = h->next;
                hook_free(h);
                t->nhooks--;
            } else {
                pp = &h->next;
            }
        }
    }
    hook *h = malloc(sizeof *h);
    if (!h) zell_panic("out of memory (cancel hook)");
    if (!weak) zell_retain(w);
    h->fn = fn;
    h->fn_data = fn_data;
    h->data = data;
    h->w = w;
    h->weak = weak;
    h->next = t->hooks;
    t->hooks = h;
    t->nhooks++;
    task_unlock(t);
}

/* The same on the current task. */
static void cancel_hook_ex(void (*fn)(zell_task *), void (*fn_data)(zell_task *, uintptr_t), uintptr_t data, zell_task *w) {
    cancel_hook_on(t_cur, fn, fn_data, data, w, 0);
}

static void cancel_hook(void (*fn)(zell_task *), zell_task *w) { cancel_hook_ex(fn, NULL, 0, w); }

void zell__cancel_hook(void (*fn)(zell_task *), zell_task *w) { cancel_hook(fn, w); }

void zell__cancel_hook_data(void (*fn)(zell_task *, uintptr_t), zell_task *w, uintptr_t data) {
    cancel_hook_ex(NULL, fn, data, w);
}

void zell__cancel_hook_self(zell_task *w, void (*fn)(zell_task *)) { cancel_hook_on(w, fn, NULL, 0, w, 1); }

static void signal_cancelled(zell_task *w) { zell_task_rt_fire(w); }

zell_task *zell_task_rt_signal(void) {
    zell_task *t = task_new_ex(0, NULL, 0);
    cancel_hook(signal_cancelled, t);
    return t;
}

/* debug.c: the current task (borrowed) and its debug record id. */
zell_task *zell__task_cur(void) { return t_cur; }
int32_t zell__task_dbg_id(const zell_task *t) { return t ? __atomic_load_n(&t->dbg_id, __ATOMIC_ACQUIRE) : 0; }
void zell__task_set_dbg_id(zell_task *t, int32_t id) { __atomic_store_n(&t->dbg_id, id, __ATOMIC_RELEASE); }

zell_task *zell_task_current(void) {
    zell_retain(t_cur);
    return t_cur;
}

zell_task *zell_task_new_scope(void) {
    zell_task *t = task_new_ex(0, NULL, 0);
    task_attach(t, t_cur);
    return t;
}

zell_task *zell_task_enter_scope(zell_task *scope) {
    zell_task *prev = t_cur;  /* moved to the caller */
    if (scope) {
        zell_retain(scope);
        t_cur = scope;
    } else {
        zell_retain(prev);
        t_cur = prev;
    }
    return prev;
}

void zell_task_leave_scope(zell_task *prev) {
    zell_retain(prev);
    zell_task *old = t_cur;
    t_cur = prev;
    zell_release(old);
}

/* task.ErrCancelled: an immortal sentinel. */
static ZELL_STRING_STORAGE(15) cancelled_msg = ZELL_STRING_INIT(cancelled_msg, "task cancelled");

static zell_error cancelled_error = {
    .hdr = { ZELL_RC_IMMORTAL, &zell_type_error }, .message = (zell_string *)&cancelled_msg,
};

zell_error *zell_task_err_cancelled(void) { return &cancelled_error; }

/* task.Key<T>() ids: unique per process. */
int64_t zell_task_next_key(void) {
    static int64_t next;
    return __atomic_add_fetch(&next, 1, __ATOMIC_RELAXED);
}

/* Debugger support (ABI.md §20): where the lldb/gdb formatters in tools/
 * find a task's state and result slot. They read memory only (no calls into
 * the debuggee), so this works on core dumps too. */
const int64_t zell_debug_task_layout[4] = {
    1, /* version of this table */
    (int64_t)offsetof(struct zell_task, state),
    (int64_t)offsetof(struct zell_task, slot),
    TASK_DONE,
};

/* ======================================================================
 * Synchronous fast path of async functions (ABI.md 10.3)
 *
 * A coroutine ramp makes no task until its first real suspension: the body
 * runs as part of the caller's current task, and when it returns without
 * suspending the result goes to the caller through an out pointer (no
 * task, and no frame either when LLVM sees no suspend point). These three
 * functions are the runtime half of that protocol.
 * ====================================================================== */

/* The coroutine's own task, made at its first suspension: `t` when it
 * already exists, else a new pending task that is attached under the
 * current task and made current (what zell_task_new does at the start of
 * the ramp in the eager protocol), with `ctx0` (+1, the context saved when
 * the ramp started) as the context to put back when the coroutine suspends
 * or completes. Returns the task with two references: the frame's and the
 * one the ramp returns to its caller (the current-task slot holds a third,
 * dropped by restore_parent). */
zell_task *zell_task_lazy(zell_task *t, int64_t result_size, const zell_value_ops *result_ops, zell_object *ctx0) {
    if (t) return t;
    t = task_new_ex(result_size, result_ops, 0);
    task_attach(t, t_cur);
    t->caller_ctx = ctx0;   /* moved */
    t->ctx_saved = 1;
    zell_retain(t);
    zell_retain(t);
    zell_task *old = t_cur;
    t_cur = t;
    zell_release(old);  /* t->parent holds its own reference */
    return t;
}

/* A coroutine that made no task returns: the context saved when its ramp
 * started (`ctx0`, +1, consumed) is current again, so whatever the body set
 * (task.With, a diag span, log.Context.Push) does not leak into the caller. */
void zell_coro_leave(zell_object *ctx0) {
    zell_object *old = t_ctx;
    if (old == ctx0) {
        zell_release(ctx0);
        return;
    }
    t_ctx = ctx0;
    zell_release(old);
}

/* An already completed plain task holding `result` (result_size bytes
 * copied; the slot takes over its reference, nothing is retained), +1.
 * Materialises the result of an async function that completed without
 * suspending when the caller keeps the Task<T> instead of awaiting it. */
zell_task *zell_task_completed(int64_t result_size, const zell_value_ops *result_ops, const void *result) {
    zell_task *t = task_new_ex(result_size, result_ops, 0);
    if (result && result_size > 0) memcpy(t->slot, result, (size_t)result_size);
    __atomic_store_n(&t->state, TASK_DONE, __ATOMIC_RELEASE);
    return t;
}
