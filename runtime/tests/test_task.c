#include "test.h"
#include <pthread.h>
#include <sched.h>
#include <time.h>

static int64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

/* ---- spawn / await ---- */

static void square(zell_task *t, void *ctx) {
    int64_t v = (int64_t)(intptr_t)ctx;
    v *= v;
    zell_task_complete(t, &v);
}

TEST(spawn_await) {
    zell_task *t = zell_task_spawn(square, (void *)(intptr_t)12);
    int64_t *r = zell_task_await(t);
    assert(*r == 144);
    assert(zell_task_is_done(t));
    assert(*(int64_t *)zell_task_result(t) == 144);
    zell_release(t);
}

static int64_t counter;
static void bump(zell_task *t, void *ctx) {
    (void)ctx;
    __atomic_add_fetch(&counter, 1, __ATOMIC_RELAXED);
    zell_task_complete(t, NULL);
}

TEST(many_spawns_all) {
    enum { N = 2000 };
    static zell_task *ts[N];
    for (int i = 0; i < N; i++) ts[i] = zell_task_spawn(bump, NULL);
    zell_task *all = zell_task_all(ts, N);
    zell_task_await(all);
    assert(counter == N);
    for (int i = 0; i < N; i++) { assert(zell_task_is_done(ts[i])); zell_release(ts[i]); }
    zell_release(all);
    zell_task *empty = zell_task_all(NULL, 0);
    assert(zell_task_is_done(empty));
    zell_release(empty);
}

/* ---- nested blocking await inside a worker (help mode, no deadlock) ---- */

static void outer(zell_task *t, void *ctx) {
    (void)ctx;
    int64_t sum = 0;
    zell_task *inner[8];
    for (int i = 0; i < 8; i++) inner[i] = zell_task_spawn(square, (void *)(intptr_t)i);
    for (int i = 0; i < 8; i++) { sum += *(int64_t *)zell_task_await(inner[i]); zell_release(inner[i]); }
    zell_task_complete(t, &sum);
}

TEST(nested_await_on_worker) {
    zell_task *ts[64];
    for (int i = 0; i < 64; i++) ts[i] = zell_task_spawn(outer, NULL);
    for (int i = 0; i < 64; i++) {
        assert(*(int64_t *)zell_task_await(ts[i]) == 140);
        zell_release(ts[i]);
    }
}

/* ---- hand-lowered coroutine using the switched-resume frame layout ---- */

typedef struct frame {
    void (*resume)(void *);   /* +0: what llvm.coro.resume calls */
    void (*destroy)(void *);  /* +8 */
    int state;
    zell_task *self;
    zell_task *awaited;
    int64_t arg, acc;
} frame;

static void co_body(void *p) {
    frame *f = p;
    switch (f->state) {
    case 0:
        f->awaited = zell_task_delay(5 + f->arg % 7);
        f->state = 1;
        if (zell_task_add_continuation(f->awaited, zell_coro_resume, f)) return; /* suspend */
        __attribute__((fallthrough)); /* already done: continue without suspending */
    case 1:
        zell_release(f->awaited);
        f->awaited = zell_task_spawn(square, (void *)(intptr_t)f->arg);
        f->state = 2;
        if (zell_task_add_continuation(f->awaited, zell_coro_resume, f)) return;
        __attribute__((fallthrough));
    case 2:
        f->acc = *(int64_t *)zell_task_result(f->awaited) + 1;
        zell_release(f->awaited);
        zell_task_complete(f->self, &f->acc);  /* return value */
        zell_release(f->self);                 /* frame's reference */
        zell_free(f);                          /* final suspend + coro.free */
        return;
    }
}

/* Ramp function of `async func f(arg int) int`. */
static zell_task *async_f(int64_t arg) {
    frame *f = zell_alloc(sizeof *f);
    f->resume = co_body;
    f->self = zell_task_new(8, NULL);
    zell_retain(f->self);        /* +1 for the frame, +1 returned to caller */
    f->arg = arg;
    zell_task *t = f->self;
    co_body(f);                   /* run synchronously until the first suspend */
    return t;
}

TEST(coroutine_protocol) {
    enum { N = 200 };
    zell_task *ts[N];
    for (int i = 0; i < N; i++) ts[i] = async_f(i);
    zell_task *all = zell_task_all(ts, N);
    zell_task_await(all);
    zell_release(all);
    for (int i = 0; i < N; i++) {
        assert(*(int64_t *)zell_task_result(ts[i]) == (int64_t)i * i + 1);
        zell_release(ts[i]);
    }
}

/* ---- delay / any ---- */

TEST(delay_any) {
    int64_t t0 = now_ms();
    zell_task *d = zell_task_delay(50);
    zell_task_await(d);
    assert(now_ms() - t0 >= 50);
    zell_release(d);
    zell_task *z = zell_task_delay(0);
    assert(zell_task_is_done(z));
    zell_release(z);
    zell_task *ds[3] = { zell_task_delay(300), zell_task_delay(10), zell_task_delay(200) };
    zell_task *any = zell_task_any(ds, 3);
    assert(*(int64_t *)zell_task_await(any) == 1);
    zell_release(any);
    zell_task *all = zell_task_all(ds, 3);
    zell_task_await(all);
    zell_release(all);
    for (int i = 0; i < 3; i++) zell_release(ds[i]);
}

/* ---- timer heap: many delays fire in deadline order; rt_fire removes a
 * pending delay from anywhere in the heap ---- */

static int64_t fired_seq, fired_done;
typedef struct { int64_t ms, order; } fire_rec;
static fire_rec fired[400];

static void on_fired(void *arg) {
    int64_t ms = (int64_t)(intptr_t)arg;
    int64_t i = __atomic_fetch_add(&fired_seq, 1, __ATOMIC_ACQ_REL);
    fired[i].ms = ms;
    fired[i].order = i;
    /* Publish the record: the test reads `fired` after `fired_done`. */
    __atomic_fetch_add(&fired_done, 1, __ATOMIC_RELEASE);
}

TEST(timer_heap_order_and_fire) {
    enum { N = 400 };
    static zell_task *ds[N];
    fired_seq = 0;
    fired_done = 0;
    for (int i = 0; i < N; i++) {
        /* Durations 20..215 ms in a scrambled order, 2 per value. */
        int64_t ms = 20 + (int64_t)((i * 37) % (N / 2));
        ds[i] = zell_task_delay(ms);
        if (i % 4 == 3) continue;  /* fired early below */
        zell_task_add_continuation(ds[i], on_fired, (void *)(intptr_t)ms);
    }
    /* Fire every 4th delay now: removal from the middle of the heap. */
    for (int i = 3; i < N; i += 4) {
        zell_task_rt_fire(ds[i]);
        assert(zell_task_is_done(ds[i]));
        zell_task_rt_fire(ds[i]);  /* idempotent */
    }
    zell_task *all = zell_task_all(ds, N);
    zell_task_await(all);
    zell_release(all);
    /* Continuations run as jobs; wait until all have recorded. */
    while (__atomic_load_n(&fired_done, __ATOMIC_ACQUIRE) < N - N / 4) sched_yield();
    /* Timers fire in deadline order; their jobs may run a little out of
     * order on 3 workers, but never by more than a few ms of deadline. */
    for (int64_t i = 1; i < N - N / 4; i++) assert(fired[i].ms + 15 >= fired[i - 1].ms);
    for (int i = 0; i < N; i++) zell_release(ds[i]);
}

/* ---- hand-offs between workers (spin-before-park path): a chain of
 * spawns where each job posts the next, while the others are idle ---- */

typedef struct { zell_task *done; int64_t left; } relay;

static void relay_step(zell_task *t, void *ctx) {
    relay *r = ctx;
    if (--r->left == 0) {
        zell_task_complete(r->done, NULL);
    } else {
        zell_task *next = zell_task_spawn(relay_step, r);
        zell_release(next);
    }
    zell_task_complete(t, NULL);
}

TEST(relay_handoffs) {
    relay r = { zell_task_new_plain(0, NULL), 20000 };
    zell_task *first = zell_task_spawn(relay_step, &r);
    zell_task_await(r.done);
    zell_task_await(first);
    zell_release(first);
    zell_release(r.done);
    /* After the burst the pool goes idle again and still wakes for work. */
    zell_task *d = zell_task_delay(5);
    zell_task_await(d);
    zell_release(d);
    zell_task *t = zell_task_spawn(square, (void *)(intptr_t)9);
    assert(*(int64_t *)zell_task_await(t) == 81);
    zell_release(t);
}

/* ---- a run-next slot behind a job that never yields: the job completes a
 * task (its awaiter lands in this worker's run-next slot) and then spins
 * until that awaiter has run, which it only can if another worker steals
 * the slot after the grace period (the select_many deadlock: a loop that
 * keeps finding completed tasks never gives its worker back). ---- */

static int64_t starved_ran;

static void starved_cont(void *arg) { (void)arg; __atomic_store_n(&starved_ran, 1, __ATOMIC_RELEASE); }

static void hog_job(zell_task *t, void *ctx) {
    (void)ctx;
    zell_task *w = zell_task_new_plain(0, NULL);
    zell_task_add_continuation(w, starved_cont, NULL);
    zell_task_complete(w, NULL);   /* the continuation goes to this worker's run-next slot */
    zell_release(w);
    int64_t t0 = now_ms();
    while (!__atomic_load_n(&starved_ran, __ATOMIC_ACQUIRE)) {
        assert(now_ms() - t0 < 5000);
        for (volatile int i = 0; i < 1000; i++) {}   /* busy, never yields */
    }
    zell_task_complete(t, NULL);
}

TEST(runnext_stolen_from_busy_worker) {
    for (int round = 0; round < 20; round++) {
        starved_ran = 0;
        zell_task *t = zell_task_spawn(hog_job, NULL);
        zell_task_await(t);
        zell_release(t);
    }
}

/* ---- results with refcounted payloads ---- */

static void make_str(zell_task *t, void *ctx) {
    (void)ctx;
    zell_string *s = S("result");
    zell_task_complete(t, &s);   /* task retains */
    zell_release(s);
}

TEST(refcounted_result) {
    zell_task *t = zell_task_spawn_ex(make_str, NULL, sizeof(zell_string *), &zell_ops_ref);
    zell_string *s = *(zell_string **)zell_task_await(t);
    assert(STR_IS(s, "result"));
    zell_retain(s);              /* take our own copy before the task dies */
    zell_release(t);
    /* the worker may still be inside make_str or hold its job reference to t, so the
     * exact count is 1..3 here; leaks are checked after shutdown in main() */
    assert(STR_IS(s, "result") && zell_refcount(s) >= 1 && zell_refcount(s) <= 3);
    zell_release(s);
}

/* ---- mutex ---- */

static zell_mutex *mu;
static int64_t guarded;

static void locker(zell_task *t, void *ctx) {
    (void)ctx;
    for (int i = 0; i < 2000; i++) {
        zell_mutex_lock(mu);
        int64_t v = guarded;
        guarded = v + 1;
        zell_mutex_unlock(mu);
    }
    zell_task_complete(t, NULL);
}

TEST(mutex) {
    mu = zell_mutex_new();
    assert(zell_mutex_try_lock(mu));
    assert(!zell_mutex_try_lock(mu));
    zell_task *pending = zell_mutex_lock_async(mu);
    assert(!zell_task_is_done(pending));
    zell_mutex_unlock(mu);                 /* hands ownership to `pending` */
    zell_task_await(pending);
    assert(!zell_mutex_try_lock(mu));
    zell_mutex_unlock(mu);
    zell_release(pending);

    zell_task *ts[8];
    for (int i = 0; i < 8; i++) ts[i] = zell_task_spawn(locker, NULL);
    for (int i = 0; i < 8; i++) { zell_task_await(ts[i]); zell_release(ts[i]); }
    assert(guarded == 8 * 2000);
    zell_release(mu);
}

/* ---- channels ---- */

typedef struct { zell_chan *ch; int64_t n; } prod_ctx;

static void producer(zell_task *t, void *ctx) {
    prod_ctx *pc = ctx;
    for (int64_t i = 1; i <= pc->n; i++) zell_chan_send(pc->ch, &i);
    zell_chan_close(pc->ch);
    zell_task_complete(t, NULL);
}

static void chan_roundtrip(int64_t cap) {
    zell_chan *ch = zell_chan_new(8, NULL, cap);
    prod_ctx pc = { ch, 5000 };
    zell_task *p = zell_task_spawn(producer, &pc);
    int64_t v, sum = 0, n = 0;
    while (zell_chan_recv(ch, &v)) { sum += v; n++; }
    assert(n == 5000 && sum == 5000 * 5001 / 2);
    assert(!zell_chan_recv(ch, &v) && v == 0);   /* closed: zero value, ok=false */
    zell_task_await(p);
    zell_release(p);
    zell_release(ch);
}

TEST(channel_unbuffered) { chan_roundtrip(0); }
TEST(channel_buffered) { chan_roundtrip(16); }

TEST(channel_async_and_strings) {
    zell_chan *ch = zell_chan_new(sizeof(zell_string *), &zell_ops_ref, 2);
    zell_task *r1 = zell_chan_recv_async(ch);         /* waits */
    assert(!zell_task_is_done(r1));
    zell_string *a = S("a"), *b = S("b"), *c = S("c");
    zell_task *s1 = zell_chan_send_async(ch, &a);      /* hands off to r1 */
    assert(zell_task_is_done(s1) && zell_task_is_done(r1));
    unsigned char *res = zell_task_result(r1);
    assert(*(int64_t *)res == 1 && *(zell_string **)(res + 8) == a);
    zell_task *s2 = zell_chan_send_async(ch, &b);      /* buffered */
    zell_task *s3 = zell_chan_send_async(ch, &c);      /* buffered */
    zell_task *s4 = zell_chan_send_async(ch, &a);      /* blocks: buffer full */
    assert(zell_task_is_done(s2) && zell_task_is_done(s3) && !zell_task_is_done(s4));
    assert(zell_chan_len(ch) == 2);
    zell_string *got;
    assert(zell_chan_recv(ch, &got) && got == b);       /* refills from s4 */
    zell_release(got);
    assert(zell_task_is_done(s4));
    zell_release(s1); zell_release(s2); zell_release(s3); zell_release(s4);
    zell_release(r1);
    zell_release(ch);                                   /* drops buffered c, a */
    assert(zell_refcount(a) == 1 && zell_refcount(b) == 1 && zell_refcount(c) == 1);
    zell_release(a); zell_release(b); zell_release(c);
}

/* ---- work stealing: one worker floods its local queue (the ring
 * overflows to the global queue), the other workers drain it ---- */

enum { FLOOD = 4000 };
static pthread_t flood_thread[FLOOD];
static int64_t flood_done;

static void flood_job(void *arg) {
    int64_t i = (int64_t)(intptr_t)arg;
    flood_thread[i] = pthread_self();
    __atomic_add_fetch(&flood_done, 1, __ATOMIC_RELEASE);
}

static void flooder(zell_task *t, void *ctx) {
    (void)ctx;
    for (int64_t i = 0; i < FLOOD; i++) zell_sched_post(flood_job, (void *)(intptr_t)i);
    zell_task_complete(t, NULL);
}

TEST(work_stealing) {
    flood_done = 0;
    zell_task *f = zell_task_spawn(flooder, NULL);
    zell_task_await(f);
    zell_release(f);
    int64_t t0 = now_ms();
    while (__atomic_load_n(&flood_done, __ATOMIC_ACQUIRE) < FLOOD) {
        assert(now_ms() - t0 < 10000);
        sched_yield();
    }
    /* Every job ran exactly once (the counter), on more than one worker. */
    int distinct = 1;
    for (int64_t i = 1; i < FLOOD; i++) {
        int seen = 0;
        for (int64_t j = 0; j < i && !seen; j++) seen = pthread_equal(flood_thread[i], flood_thread[j]);
        if (!seen) distinct++;
    }
    assert(distinct >= 2);
}

/* ---- run-next fairness: a pair of tasks completing each other in a tight
 * loop on one worker (the continuation always lands in its run-next slot)
 * must not starve a job queued in that worker's ring. The other two
 * workers are kept busy, so nothing can steal the queued job. ---- */

static int64_t busy_running, busy_release;
static int64_t pp_steps, third_ran_at;

static void busy_job(zell_task *t, void *ctx) {
    (void)ctx;
    __atomic_add_fetch(&busy_running, 1, __ATOMIC_RELEASE);
    while (!__atomic_load_n(&busy_release, __ATOMIC_ACQUIRE)) sched_yield();
    zell_task_complete(t, NULL);
}

static void third_job(void *arg) {
    (void)arg;
    __atomic_store_n(&third_ran_at, pp_steps, __ATOMIC_RELEASE);
}

static void pp_step(void *arg) {
    zell_task *done = arg;
    if (++pp_steps >= 20000) {
        zell_task_complete(done, NULL);
        return;
    }
    /* complete a fresh task whose continuation is this function: it goes
     * to this worker's run-next slot */
    zell_task *t = zell_task_new_plain(0, NULL);
    zell_task_add_continuation(t, pp_step, done);
    zell_task_complete(t, NULL);
    zell_release(t);
}

static void pp_start(void *arg) {
    zell_sched_post(third_job, NULL);   /* into this worker's ring */
    pp_step(arg);
}

TEST(runnext_fairness) {
    busy_running = busy_release = 0;
    pp_steps = 0;
    third_ran_at = -1;
    zell_task *b1 = zell_task_spawn(busy_job, NULL), *b2 = zell_task_spawn(busy_job, NULL);
    int64_t t0 = now_ms();
    while (__atomic_load_n(&busy_running, __ATOMIC_ACQUIRE) < 2) { assert(now_ms() - t0 < 5000); sched_yield(); }
    zell_task *done = zell_task_new_plain(0, NULL);
    zell_sched_post(pp_start, done);
    zell_task_await(done);
    int64_t at = __atomic_load_n(&third_ran_at, __ATOMIC_ACQUIRE);
    assert(at >= 0 && at < 1000);   /* ran within the first streak, not after the 20000 steps */
    __atomic_store_n(&busy_release, 1, __ATOMIC_RELEASE);
    zell_task_await(b1);
    zell_task_await(b2);
    zell_release(b1);
    zell_release(b2);
    zell_release(done);
}

/* ---- channels: cap 0, 1 and N with waiting senders and receivers; close
 * with waiters; 10k producers into one consumer, every producer completes;
 * cancellation of a pending receive and of a pending send ---- */

static void chan_cap(int64_t cap) {
    zell_chan *ch = zell_chan_new(8, NULL, cap);
    /* fill the buffer, then one more sender waits */
    zell_task *sends[4];
    for (int64_t i = 0; i < 4; i++) sends[i] = zell_chan_send_async(ch, &i);
    for (int64_t i = 0; i < 4; i++) assert(zell_task_is_done(sends[i]) == (i < cap));
    assert(zell_chan_len(ch) == (cap < 4 ? cap : 4));
    /* receives take the buffer in order, then the blocked senders in order */
    for (int64_t i = 0; i < 4; i++) {
        int64_t v;
        assert(zell_chan_recv(ch, &v) && v == i);
    }
    for (int64_t i = 0; i < 4; i++) { zell_task_await(sends[i]); zell_release(sends[i]); }
    /* receivers wait, each is woken exactly once, in order */
    zell_task *recvs[3];
    for (int i = 0; i < 3; i++) { recvs[i] = zell_chan_recv_async(ch); assert(!zell_task_is_done(recvs[i])); }
    for (int64_t i = 10; i < 13; i++) { zell_task *s = zell_chan_send_async(ch, &i); assert(zell_task_is_done(s)); zell_release(s); }
    for (int i = 0; i < 3; i++) {
        unsigned char *r = zell_task_await(recvs[i]);
        assert(*(int64_t *)r == 1 && *(int64_t *)(r + 8) == 10 + i);
        zell_release(recvs[i]);
    }
    /* close with waiting receivers: all fail at once; later receives too */
    zell_task *r1 = zell_chan_recv_async(ch), *r2 = zell_chan_recv_async(ch);
    zell_chan_close(ch);
    assert(*(int64_t *)zell_task_await(r1) == 0 && *(int64_t *)zell_task_await(r2) == 0);
    zell_release(r1);
    zell_release(r2);
    int64_t v = 7;
    assert(!zell_chan_recv(ch, &v) && v == 0);
    zell_release(ch);
}

TEST(channel_capacities) {
    chan_cap(0);
    chan_cap(1);
    chan_cap(3);
}

typedef struct { zell_chan *ch; int64_t id; int64_t each; } fan_ctx;

static void fan_producer(zell_task *t, void *ctx) {
    fan_ctx *fc = ctx;
    for (int64_t i = 0; i < fc->each; i++) {
        int64_t v = fc->id * fc->each + i;
        zell_chan_send(fc->ch, &v);
    }
    zell_task_complete(t, NULL);
}

TEST(channel_fan_in_10k) {
    enum { P = 10000, EACH = 10 };
    zell_chan *ch = zell_chan_new(8, NULL, 64);
    static fan_ctx fcs[P];
    static zell_task *ps[P];
    for (int64_t i = 0; i < P; i++) {
        fcs[i] = (fan_ctx){ ch, i, EACH };
        ps[i] = zell_task_spawn(fan_producer, &fcs[i]);
    }
    int64_t sum = 0, v;
    for (int64_t n = 0; n < P * EACH; n++) { assert(zell_chan_recv(ch, &v)); sum += v; }
    assert(sum == (int64_t)P * EACH * ((int64_t)P * EACH - 1) / 2);
    for (int64_t i = 0; i < P; i++) { zell_task_await(ps[i]); assert(zell_task_is_done(ps[i])); zell_release(ps[i]); }
    assert(zell_chan_len(ch) == 0);
    zell_release(ch);
}

/* A pending receive and a pending send are cancelled through their own
 * task (task.Any cancels losers this way) and leave the queues. */
TEST(channel_cancel_waits) {
    zell_chan *ch = zell_chan_new(8, NULL, 0);
    zell_task *r = zell_chan_recv_async(ch);
    assert(!zell_task_is_done(r));
    zell_task_cancel(r);
    unsigned char *res = zell_task_await(r);
    assert(*(int64_t *)res == 0);
    zell_release(r);
    int64_t v = 5;
    zell_task *s = zell_chan_send_async(ch, &v);
    assert(!zell_task_is_done(s));
    zell_task_cancel(s);
    zell_task_await(s);
    zell_release(s);
    /* nothing is left queued: a receiver now waits, a sender hands off */
    zell_task *r2 = zell_chan_recv_async(ch);
    assert(!zell_task_is_done(r2));
    zell_task *s2 = zell_chan_send_async(ch, &v);
    assert(zell_task_is_done(s2) && zell_task_is_done(r2) && *(int64_t *)((unsigned char *)zell_task_result(r2) + 8) == 5);
    zell_release(s2);
    zell_release(r2);
    zell_chan_close(ch);
    zell_release(ch);
}

/* ---- mutex: 1k tasks contending, TryLock, lock of a free mutex is the
 * immortal done task, unlock without lock panics ---- */

static zell_mutex *cmu;
static int64_t cguarded, cdone;
static zell_task *call_done;

typedef struct { int64_t left; } contender;

static void contender_step(void *arg);

/* Owns the lock: the critical section, then the next iteration. */
static void contender_locked(void *arg) {
    contender *c = arg;
    int64_t v = cguarded;
    if (c->left % 7 == 0) sched_yield();
    cguarded = v + 1;
    zell_mutex_unlock(cmu);
    if (--c->left == 0) {
        if (__atomic_add_fetch(&cdone, 1, __ATOMIC_ACQ_REL) == 1000) zell_task_complete(call_done, NULL);
        return;
    }
    contender_step(arg);
}

/* Takes the lock without blocking the worker: a free mutex is owned at
 * once (the immortal done task), otherwise the continuation runs when the
 * unlocker hands the lock over (the 10.3 protocol, as compiled code does). */
static void contender_step(void *arg) {
    zell_task *t = zell_mutex_lock_async(cmu);
    bool wait = zell_task_add_continuation(t, contender_locked, arg);
    zell_release(t);   /* a pending task is kept alive by the mutex queue */
    if (!wait) contender_locked(arg);
}

TEST(mutex_contention_1k) {
    enum { N = 1000 };
    cmu = zell_mutex_new();
    cguarded = cdone = 0;
    call_done = zell_task_new_plain(0, NULL);
    static contender cs[N];
    for (int i = 0; i < N; i++) { cs[i].left = 50; zell_sched_post(contender_step, &cs[i]); }
    zell_task_await(call_done);
    zell_release(call_done);
    assert(cguarded == N * 50);
    /* free again: TryLock succeeds once, a Lock of a free mutex is done at
     * once (the shared immortal task), waiters queue in FIFO order */
    zell_task *free_lock = zell_mutex_lock_async(cmu);
    assert(zell_task_is_done(free_lock));
    assert(!zell_mutex_try_lock(cmu));
    zell_task *w1 = zell_mutex_lock_async(cmu), *w2 = zell_mutex_lock_async(cmu);
    assert(!zell_task_is_done(w1) && !zell_task_is_done(w2));
    zell_mutex_unlock(cmu);
    assert(zell_task_is_done(w1) && !zell_task_is_done(w2));
    zell_mutex_unlock(cmu);
    assert(zell_task_is_done(w2));
    zell_mutex_unlock(cmu);
    assert(zell_mutex_try_lock(cmu));
    zell_mutex_unlock(cmu);
    zell_release(w1);
    zell_release(w2);
    zell_release(free_lock);
    zell_release(cmu);
}

static void unlock_unlocked(void) {
    zell_mutex *m = zell_mutex_new();
    zell_mutex_unlock(m);
}

/* ---- misuse ---- */

static void double_complete(void) {
    zell_task *t = zell_task_new(0, NULL);
    zell_task_complete(t, NULL);
    zell_task_complete(t, NULL);
}

static void early_result(void) {
    zell_task *t = zell_task_new(8, NULL);
    zell_task_result(t);
}

/* ---- a destructor run by a worker's release of the job's task starts a
 * task (regression: the worker's current-task slot still named the dying
 * task, which the new task took as its parent: resurrected, double free) ---- */

static int64_t drop_job_go, drop_ran;

static void guard_drop(void *obj) {
    (void)obj;
    zell_task *n = zell_task_new(0, NULL);   /* parent = the current task */
    zell_task_complete(n, NULL);
    zell_release(n);
    __atomic_store_n(&drop_ran, 1, __ATOMIC_RELEASE);
}

static const zell_type_info guard_type = { "Guard", 0, guard_drop };

static void wait_flag(void *arg) {
    (void)arg;
    while (!__atomic_load_n(&drop_job_go, __ATOMIC_ACQUIRE)) sched_yield();
}

TEST(drop_in_job_teardown_starts_task) {
    for (int round = 0; round < 50; round++) {
        drop_job_go = drop_ran = 0;
        zell_task *t = zell_task_new_plain(8, &zell_ops_ref);
        void *g = zell_object_new(&guard_type, 16);
        zell_task_complete(t, &g);              /* the result holds the guard */
        zell_release(g);
        zell_task *prev = zell_task_enter_scope(t);
        zell_sched_post(wait_flag, NULL);       /* the job runs as task t */
        zell_task_leave_scope(prev);
        zell_release(prev);
        zell_release(t);                        /* the job's is the last reference */
        __atomic_store_n(&drop_job_go, 1, __ATOMIC_RELEASE);
        int64_t t0 = now_ms();
        while (!__atomic_load_n(&drop_ran, __ATOMIC_ACQUIRE)) {
            assert(now_ms() - t0 < 5000);
            sched_yield();
        }
    }
}

TEST(misuse_panics) {
    char buf[8192];
    assert(run_child(double_complete, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: task completed twice"));
    assert(run_child(early_result, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: task result read before completion"));
    assert(run_child(unlock_unlocked, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: unlock of unlocked mutex"));
}

int main(int argc, char **argv) {
    printf("test_task\n");
    setenv("ZELL_WORKERS", "3", 1);
    int64_t live = zell_live_objects();
    zell_runtime_init(argc, argv);
    assert(zell_sched_workers() == 3);
    assert(!zell_sched_is_worker());
    zell_slice args;
    zell_os_args(&args);
    assert(args.len == argc);
    assert(strstr(zell_string_data(*(zell_string **)zell_slice_index(&args, 0, 8)), "test_task"));
    zell_slice_release(&args);

    RUN(misuse_panics);   /* forks: run before worker threads are busy */
    RUN(spawn_await);
    RUN(many_spawns_all);
    RUN(nested_await_on_worker);
    RUN(coroutine_protocol);
    RUN(delay_any);
    RUN(timer_heap_order_and_fire);
    RUN(relay_handoffs);
    RUN(work_stealing);
    RUN(runnext_fairness);
    RUN(runnext_stolen_from_busy_worker);
    RUN(refcounted_result);
    RUN(mutex);
    RUN(channel_unbuffered);
    RUN(channel_buffered);
    RUN(channel_async_and_strings);
    RUN(channel_capacities);
    RUN(channel_fan_in_10k);
    RUN(channel_cancel_waits);
    RUN(mutex_contention_1k);
    RUN(drop_in_job_teardown_starts_task);

    zell_runtime_shutdown();
    assert(zell_live_objects() == live);
    return 0;
}
