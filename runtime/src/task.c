/* task.c - thread-pool scheduler, tasks, timers, mutex, channels, the
 * per-task context slot and small primitives for task (signals, spin
 * locks, yield). Threads, the CPU count and the clock come from the platform
 * layer (platform/platform.h); the I/O poller is in task_io.c. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* ======================================================================
 * Scheduler: N workers over one global FIFO queue.
 * ====================================================================== */

typedef struct job {
    zell_resume_fn fn;
    void *arg;
    zell_object *ctx;   /* owned: the task context the job runs with */
    zell_task *cur;     /* owned: the current task the job runs as (10.11) */
    struct job *next;
} job;

static zell__plat_mutex q_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_cond q_cv = ZELL_PLAT_COND_INIT;     /* work available / stop */
static zell__plat_cond idle_cv = ZELL_PLAT_COND_INIT;  /* drained / thread exited */
static job *q_head, *q_tail;
static int q_stopping;
static int q_started;
static int64_t q_active;      /* jobs currently running */
static int64_t q_nworkers;    /* target parallelism N */
static int64_t q_alive;       /* worker threads alive (N core + spares) */
static int64_t q_idle;        /* workers parked waiting for work */
static int64_t q_blocked;     /* workers blocked inside zell_task_await */
static int64_t q_spinning;    /* workers polling for work before parking */

/* How long an out-of-work worker polls the queue before it parks on q_cv
 * (and at most one does at a time). A job posted meanwhile is taken
 * without a futex wake-up of a parked thread: hand-offs between tasks
 * (channels, mutexes, awaits) would otherwise pay a sleep/wake per job. */
#define SPIN_NS 50000

static _Thread_local int t_is_worker;

/* The context of the code running on this thread (owned reference, NULL =
 * empty). Jobs carry the context they were posted with; see ABI.md 10.8. */
static _Thread_local zell_object *t_ctx;

/* The task whose code runs on this thread (owned reference, NULL = none):
 * the parent of tasks created now and the target of cancellation hooks.
 * Travels with jobs and continuations like t_ctx; see ABI.md 10.11. */
static _Thread_local zell_task *t_cur;

/* q_head is written under q_mu but read without it by spinning workers. */
static job *pop_locked(void) {
    job *j = q_head;
    if (j) {
        __atomic_store_n(&q_head, j->next, __ATOMIC_RELAXED);
        if (!q_head) q_tail = NULL;
    }
    return j;
}

/* Polls for work for up to SPIN_NS without the lock; q_mu is held on entry
 * and on return. Returns whether work showed up. */
static bool spin_for_work_locked(void) {
    q_spinning++;
    zell__plat_mutex_unlock(&q_mu);
    bool found = false;
    int64_t until = zell__plat_mono_ns() + SPIN_NS;
    for (int i = 1;; i++) {
        if (__atomic_load_n(&q_head, __ATOMIC_RELAXED)) { found = true; break; }
        if ((i & 63) == 0) {
            if (zell__plat_mono_ns() >= until) break;
            zell__plat_yield();   /* a shared machine: let the poster run */
        }
    }
    zell__plat_mutex_lock(&q_mu);
    q_spinning--;
    return found;
}

static void *worker_main(void *spare_flag) {
    int spare = spare_flag != NULL;
    t_is_worker = 1;
    zell__rc_thread_eligible();     /* biased refcounts (rc.c) */
    zell__plat_thread_stack_init(); /* frame walks of the CPU profiler */
    zell__plat_mutex_lock(&q_mu);
    bool spun = false;   /* polled since the last job: park next time */
    for (;;) {
        job *j = pop_locked();
        if (!j) {
            if (q_stopping) break;
            /* spare threads retire once enough unblocked workers exist again */
            if (spare && q_alive - q_blocked > q_nworkers) break;
            /* Before sleeping, take back pages other threads freed into
             * (and retire empty ones), so idle workers do not pin memory. */
            if (zell__heap_needs_collect()) {
                zell__plat_mutex_unlock(&q_mu);
                zell__heap_collect(0);
                zell__plat_mutex_lock(&q_mu);
                continue;
            }
            /* Merge this thread's refcount queue first (rc.c): destructors
             * may run, so not under q_mu. While parked, others merge it. */
            if (zell__rc_pending()) {
                zell__plat_mutex_unlock(&q_mu);
                zell__rc_poll();
                zell__plat_mutex_lock(&q_mu);
                continue;
            }
            if (!spun && q_spinning == 0 && q_nworkers > 1) {
                spun = true;
                spin_for_work_locked();
                continue;
            }
            spun = false;
            q_idle++;
            zell__rc_park(0);
            zell__plat_cond_wait(&q_cv, &q_mu);
            q_idle--;
            continue;
        }
        spun = false;
        q_active++;
        /* More work queued and nobody polling: wake a parked worker, so a
         * burst of posts (which skip the wake-up while one worker polls)
         * still spreads over the pool. */
        if (q_head && q_idle > 0 && q_spinning == 0) zell__plat_cond_signal(&q_cv);
        zell__plat_mutex_unlock(&q_mu);
        zell__rc_unpark();   /* after a park above; no-op otherwise */
        zell_resume_fn fn = j->fn;
        void *arg = j->arg;
        zell_object *saved = t_ctx;
        zell_task *saved_cur = t_cur;
        t_ctx = j->ctx;   /* the job's references move to the thread */
        t_cur = j->cur;
        free(j);
        fn(arg);
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
        zell__plat_mutex_lock(&q_mu);
        q_active--;
        if (!q_head && q_active == 0) zell__plat_cond_broadcast(&idle_cv);
    }
    q_alive--;
    zell__plat_cond_broadcast(&idle_cv);
    zell__plat_mutex_unlock(&q_mu);
    return NULL;
}

/* q_mu must be held. */
static void spawn_worker_locked(int spare) {
    if (!zell__plat_thread_start_detached(worker_main, spare ? (void *)1 : NULL))
        zell_panic("cannot create worker thread");
    q_alive++;
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
    while (q_head || q_active > 0) zell__plat_cond_wait(&idle_cv, &q_mu);  /* drain */
    q_stopping = 1;
    zell__plat_cond_broadcast(&q_cv);
    while (q_alive > 0) zell__plat_cond_wait(&idle_cv, &q_mu);
    zell__plat_mutex_unlock(&q_mu);
    timers_stop();
    stop_io();
    zell__plat_mutex_lock(&q_mu);
    q_nworkers = 0;
    __atomic_store_n(&q_started, 0, __ATOMIC_RELEASE);
    zell__plat_mutex_unlock(&q_mu);
}

/* Posts fn(arg) to run with context `ctx` as task `cur` (owned references, moved). */
static void sched_post_ctx(zell_resume_fn fn, void *arg, zell_object *ctx, zell_task *cur) {
    if (!fn) zell_panic_nil();
    if (!__atomic_load_n(&q_started, __ATOMIC_ACQUIRE)) zell__sched_start();
    job *j = malloc(sizeof *j);
    if (!j) zell_panic("out of memory (job)");
    j->fn = fn;
    j->arg = arg;
    j->ctx = ctx;
    j->cur = cur;
    j->next = NULL;
    zell__plat_mutex_lock(&q_mu);
    if (q_tail) q_tail->next = j; else __atomic_store_n(&q_head, j, __ATOMIC_RELAXED);
    q_tail = j;
    /* A polling worker takes the job without a wake-up; otherwise wake a
     * parked one (none parked: every worker is busy and will look). */
    if (q_spinning == 0 && q_idle > 0) zell__plat_cond_signal(&q_cv);
    zell__plat_mutex_unlock(&q_mu);
}

void zell_sched_post(zell_resume_fn fn, void *arg) {
    zell_retain(t_ctx);
    zell_retain(t_cur);
    sched_post_ctx(fn, arg, t_ctx, t_cur);
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
    zell_task *w;              /* owned */
    struct hook *next;
    void (*fn_data)(zell_task *w, uintptr_t data);  /* instead of fn when set */
    uintptr_t data;
} hook;

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
    int64_t n = 0;
    zell__plat_mutex_lock(&q_mu);
    for (job *j = q_head; j; j = j->next) n++;
    zell__plat_mutex_unlock(&q_mu);
    return n;
}

static void task_drop(void *obj) {
    zell_task *t = obj;
    __atomic_sub_fetch(&g_live_tasks, 1, __ATOMIC_RELAXED);
    if (t->state == TASK_DONE && t->ops && t->ops->release)
        t->ops->release(t->slot + t->ops_offset);
    cont *c = t->conts;
    while (c) { cont *n = c->next; zell_release(c->ctx); zell_release(c->cur); free(c); c = n; }
    hook *h = t->hooks;
    while (h) { hook *n = h->next; zell_release(h->w); free(h); h = n; }
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
    /* After this point `t` may be freed; use only `rev`. */
    while (c) { cont *n = c->next; c->next = rev; rev = c; c = n; }
    while (rev) {
        cont *n = rev->next;
        sched_post_ctx(rev->fn, rev->frame, rev->ctx, rev->cur);
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
    cont *c = malloc(sizeof *c);
    if (!c) zell_panic("out of memory (continuation)");
    c->fn = fn;
    c->frame = frame;
    c->ctx = t_ctx;
    c->cur = t_cur;
    task_lock(t);
    if (t->state == TASK_DONE) {
        task_unlock(t);
        free(c);
        return false;
    }
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
    c->next = t->conts;
    t->conts = c;
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
        /* Blocking a worker: make sure the pool keeps N runnable threads by
         * starting a spare worker if nobody is idle (it retires later). */
        zell__plat_mutex_lock(&q_mu);
        q_blocked++;
        if (q_idle == 0 && !q_stopping) spawn_worker_locked(1);
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
        while (due) {
            timer *t = due;
            due = t->next;
            if (t->task) zell_task_try_complete(t->task, NULL);  /* may race with rt_fire */
            if (t->cancel) zell_task_cancel(t->cancel);
            zell_release(t->task);
            zell_release(t->cancel);
            free(t);
        }
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
    zell_task *t = task_new_ex(0, NULL, 0);
    if (ms <= 0) { zell_task_complete(t, NULL); return t; }
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
 * Mutex (async-aware: waiters are tasks, unlock hands ownership over)
 * ====================================================================== */

typedef struct waiter {
    zell_task *task;      /* owned */
    struct waiter *next;
    unsigned char elem[];  /* channel senders: owned element copy */
} waiter;

typedef struct wqueue { waiter *head, *tail; } wqueue;

static void wq_push(wqueue *q, waiter *w) {
    w->next = NULL;
    if (q->tail) q->tail->next = w; else q->head = w;
    q->tail = w;
}

static waiter *wq_pop(wqueue *q) {
    waiter *w = q->head;
    if (w) { q->head = w->next; if (!q->head) q->tail = NULL; }
    return w;
}

struct zell_mutex {
    zell_object hdr;
    zell__plat_mutex mu;
    int32_t locked;
    wqueue waiters;
};

static void mutex_drop(void *obj) {
    zell_mutex *m = obj;
    waiter *w;
    while ((w = wq_pop(&m->waiters))) { zell_release(w->task); free(w); }
    zell__plat_mutex_destroy(&m->mu);
}

static const zell_type_info mutex_type = { "Mutex", (int64_t)sizeof(struct zell_mutex), mutex_drop };

zell_mutex *zell_mutex_new(void) {
    zell_mutex *m = zell_object_new(&mutex_type, (int64_t)sizeof(struct zell_mutex));
    zell__plat_mutex_init(&m->mu);
    return m;
}

zell_task *zell_mutex_lock_async(zell_mutex *m) {
    if (!m) zell_panic_nil();
    zell_task *t = task_new_ex(0, NULL, 0);
    zell__plat_mutex_lock(&m->mu);
    if (!m->locked) {
        m->locked = 1;
        zell__plat_mutex_unlock(&m->mu);
        zell_task_complete(t, NULL);
        return t;
    }
    waiter *w = malloc(sizeof *w);
    if (!w) zell_panic("out of memory (mutex)");
    zell_retain(t);
    w->task = t;
    wq_push(&m->waiters, w);
    zell__plat_mutex_unlock(&m->mu);
    return t;
}

void zell_mutex_lock(zell_mutex *m) {
    zell_task *t = zell_mutex_lock_async(m);
    zell_task_await(t);
    zell_release(t);
}

bool zell_mutex_try_lock(zell_mutex *m) {
    if (!m) zell_panic_nil();
    zell__plat_mutex_lock(&m->mu);
    bool ok = !m->locked;
    if (ok) m->locked = 1;
    zell__plat_mutex_unlock(&m->mu);
    return ok;
}

void zell_mutex_unlock(zell_mutex *m) {
    if (!m) zell_panic_nil();
    zell__plat_mutex_lock(&m->mu);
    if (!m->locked) { zell__plat_mutex_unlock(&m->mu); zell_panic("unlock of unlocked mutex"); }
    waiter *w = wq_pop(&m->waiters);
    if (!w) m->locked = 0;           /* else: ownership passes directly to w */
    zell__plat_mutex_unlock(&m->mu);
    if (w) {
        zell_task_complete(w->task, NULL);
        zell_release(w->task);
        free(w);
    }
}

/* ======================================================================
 * Channels
 * ====================================================================== */

struct zell_chan {
    zell_object hdr;
    zell__plat_mutex mu;
    int64_t elem_size;
    const zell_value_ops *ops;
    int64_t capacity;
    int64_t count, head;     /* ring buffer of `capacity` elements */
    unsigned char *buf;
    int32_t closed;
    wqueue senders;          /* waiter.elem holds the (owned) value */
    wqueue receivers;
};

static void val_retain(zell_chan *c, void *v) { if (c->ops && c->ops->retain) c->ops->retain(v); }
static void val_release(zell_chan *c, void *v) { if (c->ops && c->ops->release) c->ops->release(v); }

static void chan_drop(void *obj) {
    zell_chan *c = obj;
    for (int64_t i = 0; i < c->count; i++)
        val_release(c, c->buf + ((c->head + i) % c->capacity) * c->elem_size);
    waiter *w;
    while ((w = wq_pop(&c->senders))) { val_release(c, w->elem); zell_release(w->task); free(w); }
    while ((w = wq_pop(&c->receivers))) { zell_release(w->task); free(w); }
    zell_free(c->buf);
    zell__plat_mutex_destroy(&c->mu);
}

static const zell_type_info chan_type = { "Channel", (int64_t)sizeof(struct zell_chan), chan_drop };

zell_chan *zell_chan_new(int64_t elem_size, const zell_value_ops *ops, int64_t capacity) {
    if (elem_size < 0 || capacity < 0) zell_panic("zell_chan_new: invalid size");
    zell_chan *c = zell_object_new(&chan_type, (int64_t)sizeof(struct zell_chan));
    zell__plat_mutex_init(&c->mu);
    c->elem_size = elem_size;
    c->ops = ops;
    c->capacity = capacity;
    c->buf = capacity ? zell_alloc(capacity * (elem_size ? elem_size : 1)) : NULL;
    return c;
}

int64_t zell_chan_len(const zell_chan *c) {
    if (!c) return 0;
    zell__plat_mutex_lock((zell__plat_mutex *)&c->mu);
    int64_t n = c->count;
    zell__plat_mutex_unlock((zell__plat_mutex *)&c->mu);
    return n;
}

/* Complete a receive task with {ok, value}. value is borrowed (task retains). */
static void finish_recv(zell_chan *c, zell_task *t, int64_t ok, const void *value) {
    unsigned char tmp[8 + 256];
    int64_t sz = 8 + c->elem_size;
    unsigned char *r = (size_t)sz <= sizeof tmp ? tmp : zell_alloc(sz);
    memset(r, 0, (size_t)sz);
    *(int64_t *)r = ok;
    if (value) memcpy(r + 8, value, (size_t)c->elem_size);
    zell_task_complete(t, r);
    if (r != tmp) zell_free(r);
}

static zell_task *recv_task_new(zell_chan *c) {
    return task_new_ex(8 + c->elem_size, c->ops, 8);
}

zell_task *zell_chan_send_async(zell_chan *c, const void *elem) {
    if (!c) zell_panic_nil();
    zell_task *done = task_new_ex(0, NULL, 0);
    waiter *sw = malloc(sizeof(waiter) + (size_t)c->elem_size);  /* holds owned copy */
    if (!sw) zell_panic("out of memory (channel)");
    memcpy(sw->elem, elem, (size_t)c->elem_size);
    val_retain(c, sw->elem);

    zell__plat_mutex_lock(&c->mu);
    if (c->closed) {
        zell__plat_mutex_unlock(&c->mu);
        zell_panic("send on closed channel");
    }
    waiter *rw = wq_pop(&c->receivers);
    if (rw) {
        zell__plat_mutex_unlock(&c->mu);
        finish_recv(c, rw->task, 1, sw->elem);
        zell_release(rw->task);
        free(rw);
        val_release(c, sw->elem);
        free(sw);
        zell_task_complete(done, NULL);
        return done;
    }
    if (c->count < c->capacity) {
        memcpy(c->buf + ((c->head + c->count) % c->capacity) * c->elem_size, sw->elem,
               (size_t)c->elem_size);  /* ownership moves into the buffer */
        c->count++;
        zell__plat_mutex_unlock(&c->mu);
        free(sw);
        zell_task_complete(done, NULL);
        return done;
    }
    zell_retain(done);
    sw->task = done;
    wq_push(&c->senders, sw);
    zell__plat_mutex_unlock(&c->mu);
    return done;
}

zell_task *zell_chan_recv_async(zell_chan *c) {
    if (!c) zell_panic_nil();
    zell_task *t = recv_task_new(c);
    unsigned char tmp[256];
    unsigned char *v = (size_t)c->elem_size <= sizeof tmp ? tmp : zell_alloc(c->elem_size);
    waiter *sender = NULL;

    zell__plat_mutex_lock(&c->mu);
    if (c->count > 0) {
        unsigned char *slot = c->buf + c->head * c->elem_size;
        memcpy(v, slot, (size_t)c->elem_size);   /* take ownership */
        c->head = (c->head + 1) % c->capacity;
        c->count--;
        sender = wq_pop(&c->senders);
        if (sender) {   /* refill the buffer from a blocked sender */
            memcpy(c->buf + ((c->head + c->count) % c->capacity) * c->elem_size, sender->elem,
                   (size_t)c->elem_size);
            c->count++;
        }
    } else if ((sender = wq_pop(&c->senders))) {
        memcpy(v, sender->elem, (size_t)c->elem_size);  /* direct hand-off (unbuffered) */
    } else if (c->closed) {
        zell__plat_mutex_unlock(&c->mu);
        finish_recv(c, t, 0, NULL);
        if (v != tmp) zell_free(v);
        return t;
    } else {
        waiter *w = malloc(sizeof *w);
        if (!w) zell_panic("out of memory (channel)");
        zell_retain(t);
        w->task = t;
        wq_push(&c->receivers, w);
        zell__plat_mutex_unlock(&c->mu);
        if (v != tmp) zell_free(v);
        return t;
    }
    zell__plat_mutex_unlock(&c->mu);
    finish_recv(c, t, 1, v);
    val_release(c, v);
    if (v != tmp) zell_free(v);
    if (sender) {
        zell_task_complete(sender->task, NULL);
        zell_release(sender->task);
        free(sender);
    }
    return t;
}

void zell_chan_send(zell_chan *c, const void *elem) {
    zell_task *t = zell_chan_send_async(c, elem);
    zell_task_await(t);
    zell_release(t);
}

bool zell_chan_recv(zell_chan *c, void *out) {
    zell_task *t = zell_chan_recv_async(c);
    unsigned char *r = zell_task_await(t);
    bool ok = *(int64_t *)r != 0;
    if (out) {
        memcpy(out, r + 8, (size_t)c->elem_size);
        val_retain(c, out);
    }
    zell_release(t);
    return ok;
}

void zell_chan_close(zell_chan *c) {
    if (!c) zell_panic_nil();
    zell__plat_mutex_lock(&c->mu);
    if (c->closed) { zell__plat_mutex_unlock(&c->mu); zell_panic("close of closed channel"); }
    if (c->senders.head) { zell__plat_mutex_unlock(&c->mu); zell_panic("send on closed channel"); }
    c->closed = 1;
    wqueue rs = c->receivers;
    c->receivers.head = c->receivers.tail = NULL;
    zell__plat_mutex_unlock(&c->mu);
    waiter *w;
    while ((w = wq_pop(&rs))) {
        finish_recv(c, w->task, 0, NULL);
        zell_release(w->task);
        free(w);
    }
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
        zell_release(hs->w);
        free(hs);
        hs = n;
    }
}

bool zell_task_is_cancelled(const zell_task *t) {
    return t && __atomic_load_n(&t->cancelled, __ATOMIC_ACQUIRE) != 0;
}

bool zell_task_cur_cancelled(void) { return zell_task_is_cancelled(t_cur); }

/* Registers fn(w) to run when the current task is cancelled (at once if it
 * already is). Hooks of waits that completed meanwhile are pruned. */
static void cancel_hook_ex(void (*fn)(zell_task *), void (*fn_data)(zell_task *, uintptr_t), uintptr_t data, zell_task *w) {
    zell_task *t = t_cur;
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
                zell_release(h->w);
                free(h);
                t->nhooks--;
            } else {
                pp = &h->next;
            }
        }
    }
    hook *h = malloc(sizeof *h);
    if (!h) zell_panic("out of memory (cancel hook)");
    zell_retain(w);
    h->fn = fn;
    h->fn_data = fn_data;
    h->data = data;
    h->w = w;
    h->next = t->hooks;
    t->hooks = h;
    t->nhooks++;
    task_unlock(t);
}

static void cancel_hook(void (*fn)(zell_task *), zell_task *w) { cancel_hook_ex(fn, NULL, 0, w); }

void zell__cancel_hook(void (*fn)(zell_task *), zell_task *w) { cancel_hook(fn, w); }

void zell__cancel_hook_data(void (*fn)(zell_task *, uintptr_t), zell_task *w, uintptr_t data) {
    cancel_hook_ex(NULL, fn, data, w);
}

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
