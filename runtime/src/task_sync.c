/* task_sync.c - task.Channel<T> and task.Mutex (ABI.md 10.6, 10.7): a
 * channel with a ring buffer and FIFO queues of waiting senders and
 * receivers, each woken exactly once, and a mutex that lives inline in the
 * Zell struct (three words) and hands ownership to the oldest waiter. The
 * waits are plain tasks (task.c) whose slots hold the queue links, and the
 * element a sender waits with. The C API of 10.6/10.7 (zell_chan_*,
 * zell_mutex_*) is built on the same code. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <stdlib.h>
#include <string.h>

/* ======================================================================
 * Channels
 * ====================================================================== */

/* The three layouts of a receive's result (10.7). */
enum { RES_FALLIBLE = 0, RES_TUPLE = 1, RES_C = 2 };

/* The header a waiting task keeps in its slot, after the result (at
 * ROUND8(result_size), so filling the result never overwrites it and a late
 * cancel hook still reads a valid header). */
typedef struct chan_wait {
    struct chan_wait *next;
    struct zell_chan *ch;      /* owned while queued; NULL once dequeued (guarded by the task's lock) */
    /* The cancel hook on the task that started the wait (no allocation per
     * wait; it holds a reference to this task until the starter prunes it).
     * The wait's own cancellation (task.Any losers) is the task's cancel_fn. */
    zell__hook hook;           /* hook.w: the task this header lives in (always set) */
    int32_t kind;              /* receivers: RES_* */
    int32_t sender;
    unsigned char elem[];      /* senders: the owned element */
} chan_wait;

/* The channel lock: a few loads and stores per operation, never held across
 * a wait or user code, with up to 16 workers contending in a fan-in, so a
 * spin lock (with a yield once it spins for a while) beats a pthread mutex,
 * which parks the contenders in the kernel. */
typedef struct chan_lock { int32_t held; } chan_lock;

static inline void chan_lock_acquire(chan_lock *l) {
    for (int spins = 0;; spins++) {
        if (__atomic_load_n(&l->held, __ATOMIC_RELAXED) == 0 && __atomic_exchange_n(&l->held, 1, __ATOMIC_ACQUIRE) == 0) return;
        if (spins > 64) zell__plat_yield();
    }
}

static inline void chan_lock_release(chan_lock *l) { __atomic_store_n(&l->held, 0, __ATOMIC_RELEASE); }

struct zell_chan {
    zell_object hdr;
    chan_lock mu;
    int64_t elem_size;
    const zell_value_ops *elem_ops;
    /* Result layouts of the compiled receives: `T!` (Recv) and `(T, bool)`
     * (Next); zero when made by zell_chan_new (C code only uses RES_C). */
    int64_t fal_size, fal_err_off;
    const zell_value_ops *fal_ops;
    int64_t tup_size, tup_ok_off;
    const zell_value_ops *tup_ops;
    int64_t capacity, count, head;   /* ring buffer of `capacity` elements */
    unsigned char *buf;
    int32_t closed;
    chan_wait *sendq, *sendq_tail;
    chan_wait *recvq, *recvq_tail;
};

/* task.Channel.Recv on a closed, drained channel: an immortal sentinel. */
static ZELL_STRING_STORAGE(15) closed_msg = ZELL_STRING_INIT(closed_msg, "channel closed");
static zell_error closed_error = {
    .hdr = { ZELL_RC_IMMORTAL, &zell_type_error }, .message = (zell_string *)&closed_msg,
};

static inline void elem_retain(zell_chan *c, void *v) { if (c->elem_ops && c->elem_ops->retain) c->elem_ops->retain(v); }
static inline void elem_release(zell_chan *c, void *v) { if (c->elem_ops && c->elem_ops->release) c->elem_ops->release(v); }
static inline unsigned char *buf_at(zell_chan *c, int64_t i) { return c->buf + (i % c->capacity) * c->elem_size; }
static inline chan_wait *wait_of(zell_task *t) {
    return (chan_wait *)((unsigned char *)zell__task_slot(t) + ZELL_ROUND8(zell__task_result_size(t)));
}

static void wq_push(chan_wait **head, chan_wait **tail, chan_wait *w) {
    w->next = NULL;
    if (*tail) (*tail)->next = w; else *head = w;
    *tail = w;
}

static chan_wait *wq_pop(chan_wait **head, chan_wait **tail) {
    chan_wait *w = *head;
    if (w) { *head = w->next; if (!*head) *tail = NULL; }
    return w;
}

static bool wq_remove(chan_wait **head, chan_wait **tail, chan_wait *w) {
    chan_wait *prev = NULL;
    for (chan_wait *x = *head; x; prev = x, x = x->next) {
        if (x != w) continue;
        if (prev) prev->next = x->next; else *head = x->next;
        if (*tail == x) *tail = prev;
        return true;
    }
    return false;
}

static void chan_drop(void *obj) {
    zell_chan *c = obj;
    for (int64_t i = 0; i < c->count; i++) elem_release(c, buf_at(c, c->head + i));
    /* Waiters hold a reference to the channel, so none are queued here. */
    zell_free(c->buf);
}

static const zell_type_info chan_type = { "Channel", (int64_t)sizeof(struct zell_chan), chan_drop };

static zell_chan *chan_new(int64_t elem_size, const zell_value_ops *ops, int64_t capacity) {
    if (elem_size < 0 || capacity < 0) zell_panic("zell_chan_new: invalid size");
    zell_chan *c = zell_object_new(&chan_type, (int64_t)sizeof(struct zell_chan));
    c->elem_size = elem_size;
    c->elem_ops = ops;
    c->capacity = capacity;
    c->buf = capacity ? zell_alloc(capacity * (elem_size ? elem_size : 1)) : NULL;
    return c;
}

static void chan_wait_cancelled(zell_task *t);

/* A receive task of layout `kind`: with `wait`, pending with the wait
 * header set up; else only room for the result (a receive that completes
 * at once never waits, never has a header and is not cancellable). */
static zell_task *recv_task_new(zell_chan *c, int kind, bool wait) {
    int64_t rs, off = 0;
    const zell_value_ops *ops;
    switch (kind) {
    case RES_FALLIBLE: rs = c->fal_size; ops = c->fal_ops; break;
    case RES_TUPLE: rs = c->tup_size; ops = c->tup_ops; break;
    default: rs = 8 + c->elem_size; ops = c->elem_ops; off = 8; break;
    }
    if (!wait) return zell__task_new_slot(rs, ops, off, rs);
    zell_task *t = zell__task_new_slot(rs, ops, off, ZELL_ROUND8(rs) + (int64_t)sizeof(chan_wait));
    chan_wait *w = wait_of(t);
    w->hook.w = t;
    w->kind = kind;
    zell__task_set_cancel_fn(t, chan_wait_cancelled);
    return t;
}

/* Writes a receive's result into t's slot (before the wait header): the
 * value, which is MOVED in (`value` is an owned copy whose ownership passes
 * to the task; NULL for the zero value), and the status: `err` for the
 * fallible layout, ok for the others. The task is not completed here. */
static void recv_fill(zell_chan *c, zell_task *t, int kind, const void *value, zell_error *err) {
    unsigned char *s = zell__task_slot(t);
    switch (kind) {
    case RES_FALLIBLE:
        memset(s, 0, (size_t)c->fal_size);
        if (value) memcpy(s, value, (size_t)c->elem_size);
        *(zell_error **)(s + c->fal_err_off) = err;
        break;
    case RES_TUPLE:
        memset(s, 0, (size_t)c->tup_size);
        if (value) memcpy(s, value, (size_t)c->elem_size);
        s[c->tup_ok_off] = err == NULL;
        break;
    default:
        *(int64_t *)s = err == NULL;
        if (value) memcpy(s + 8, value, (size_t)c->elem_size);
        else memset(s + 8, 0, (size_t)c->elem_size);
        break;
    }
}

/* Takes a wait out of its queue (c->mu held): its task no longer belongs
 * to the channel. Returns the channel reference the wait held. */
static void wait_unlink(zell_task *t) {
    chan_wait *w = wait_of(t);
    zell__task_lock(t);
    w->ch = NULL;
    zell__task_unlock(t);
}

/* A queued wait whose task was cancelled (10.11): a receiver completes
 * with task.ErrCancelled, a sender returns without sending. Idempotent (it
 * runs from the hook on the task that started the wait and from the wait's
 * own cancel_fn; only the caller that dequeues the wait completes it). */
static void chan_wait_cancelled(zell_task *t) {
    chan_wait *w = wait_of(t);
    zell__task_lock(t);
    zell_chan *c = w->ch;
    if (c) zell_retain(c);
    zell__task_unlock(t);
    if (!c) return;
    chan_lock_acquire(&c->mu);
    bool queued = w->ch == c;
    if (queued) {
        if (w->sender) wq_remove(&c->sendq, &c->sendq_tail, w);
        else wq_remove(&c->recvq, &c->recvq_tail, w);
        wait_unlink(t);
    }
    chan_lock_release(&c->mu);
    if (queued) {
        if (w->sender) {
            elem_release(c, w->elem);
        } else {
            recv_fill(c, t, w->kind, NULL, zell_task_err_cancelled());
        }
        zell_task_complete(t, NULL);
        zell_release(t);   /* the queue's reference */
        zell_release(c);   /* the wait's reference */
    }
    zell_release(c);
}

/* Queues the wait t on c (c->mu held); wait_hook follows after the lock is
 * dropped (the wait may have completed by then: harmless). */
static void wait_enqueue(zell_chan *c, zell_task *t, bool sender) {
    chan_wait *w = wait_of(t);
    w->sender = sender;
    zell_retain(c);
    w->ch = c;
    zell_retain(t);   /* the queue's reference */
    if (sender) wq_push(&c->sendq, &c->sendq_tail, w);
    else wq_push(&c->recvq, &c->recvq_tail, w);
}

/* The hook on the task that waits (a Recv in a cancelled scope fails). */
static void wait_hook(zell_task *t) { zell__hook_link(&wait_of(t)->hook, zell__task_cur(), chan_wait_cancelled, t); }

/* Completes a dequeued receiver with an owned value (moved) or a status. */
static void recv_finish(zell_chan *c, chan_wait *rw, const void *value, zell_error *err) {
    zell_task *t = rw->hook.w;
    int kind = rw->kind;
    recv_fill(c, t, kind, value, err);
    zell_task_complete(t, NULL);
    zell_release(t);   /* the queue's reference */
    zell_release(c);   /* the wait's reference */
}

/* Completes a dequeued sender whose value was taken. */
static void send_finish(zell_chan *c, chan_wait *sw) {
    zell_task *t = sw->hook.w;
    zell_task_complete(t, NULL);
    zell_release(t);
    zell_release(c);
}

static zell_task *chan_send(zell_chan *c, const void *elem) {
    chan_lock_acquire(&c->mu);
    if (c->closed) {
        chan_lock_release(&c->mu);
        zell_panic("send on closed channel");
    }
    chan_wait *rw = wq_pop(&c->recvq, &c->recvq_tail);
    if (rw) {
        /* hand the value to a waiting receiver (a retained copy moves in) */
        wait_unlink(rw->hook.w);
        chan_lock_release(&c->mu);
        unsigned char tmp[256];
        unsigned char *v = (size_t)c->elem_size <= sizeof tmp ? tmp : zell_alloc(c->elem_size);
        memcpy(v, elem, (size_t)c->elem_size);
        elem_retain(c, v);
        recv_finish(c, rw, v, NULL);
        if (v != tmp) zell_free(v);
        return zell_task_done();
    }
    if (c->count < c->capacity) {
        unsigned char *slot = buf_at(c, c->head + c->count);
        memcpy(slot, elem, (size_t)c->elem_size);
        elem_retain(c, slot);
        c->count++;
        chan_lock_release(&c->mu);
        return zell_task_done();
    }
    if (zell_task_cur_cancelled()) {   /* a cancelled task does not wait: nothing is sent */
        chan_lock_release(&c->mu);
        return zell_task_done();
    }
    zell_task *t = zell__task_new_slot(0, NULL, 0, (int64_t)sizeof(chan_wait) + c->elem_size);
    chan_wait *w = wait_of(t);
    w->hook.w = t;
    zell__task_set_cancel_fn(t, chan_wait_cancelled);
    memcpy(w->elem, elem, (size_t)c->elem_size);
    elem_retain(c, w->elem);
    wait_enqueue(c, t, true);
    chan_lock_release(&c->mu);
    wait_hook(t);
    return t;
}

/* The task that waits is made only once a receive has to wait (under the
 * lock: dropping it and rechecking costs more on a contended channel than
 * the allocation); a receive that completes at once gets a task with room
 * for the result only. */
static zell_task *chan_recv(zell_chan *c, int kind) {
    chan_lock_acquire(&c->mu);
    if (c->count > 0) {
        unsigned char tmp[256];
        unsigned char *v = (size_t)c->elem_size <= sizeof tmp ? tmp : zell_alloc(c->elem_size);
        memcpy(v, buf_at(c, c->head), (size_t)c->elem_size);   /* moves out of the buffer */
        c->head = (c->head + 1) % c->capacity;
        c->count--;
        chan_wait *sw = wq_pop(&c->sendq, &c->sendq_tail);
        if (sw) {   /* refill the buffer from the oldest blocked sender */
            memcpy(buf_at(c, c->head + c->count), sw->elem, (size_t)c->elem_size);
            c->count++;
            wait_unlink(sw->hook.w);
        }
        chan_lock_release(&c->mu);
        zell_task *t = recv_task_new(c, kind, false);
        recv_fill(c, t, kind, v, NULL);
        zell__task_mark_done(t);
        if (v != tmp) zell_free(v);
        if (sw) send_finish(c, sw);
        return t;
    }
    chan_wait *sw = wq_pop(&c->sendq, &c->sendq_tail);
    if (sw) {   /* unbuffered: take the value from the oldest waiting sender */
        wait_unlink(sw->hook.w);
        chan_lock_release(&c->mu);
        zell_task *t = recv_task_new(c, kind, false);
        recv_fill(c, t, kind, sw->elem, NULL);
        zell__task_mark_done(t);
        send_finish(c, sw);
        return t;
    }
    zell_error *err = c->closed ? &closed_error : zell_task_cur_cancelled() ? zell_task_err_cancelled() : NULL;
    if (err) {
        chan_lock_release(&c->mu);
        zell_task *t = recv_task_new(c, kind, false);
        recv_fill(c, t, kind, NULL, err);
        zell__task_mark_done(t);
        return t;
    }
    zell_task *wt = recv_task_new(c, kind, true);
    wait_enqueue(c, wt, false);
    chan_lock_release(&c->mu);
    wait_hook(wt);
    return wt;
}

static void chan_close(zell_chan *c) {
    chan_lock_acquire(&c->mu);
    if (c->closed) { chan_lock_release(&c->mu); zell_panic("close of closed channel"); }
    if (c->sendq) { chan_lock_release(&c->mu); zell_panic("send on closed channel"); }
    c->closed = 1;
    chan_wait *rs = c->recvq;
    c->recvq = c->recvq_tail = NULL;
    for (chan_wait *w = rs; w; w = w->next) wait_unlink(w->hook.w);
    chan_lock_release(&c->mu);
    while (rs) {
        chan_wait *n = rs->next;
        recv_finish(c, rs, NULL, &closed_error);   /* every pending receiver, once */
        rs = n;
    }
}

static inline zell_chan *chan_of(int64_t h) {
    if (!h) zell_panic_nil();
    return (zell_chan *)(intptr_t)h;
}

/* ---- the compiled task.Channel<T> (handles are the channel's address) ---- */

int64_t zell_task_chan_new(int64_t capacity, int64_t elem_size, const zell_value_ops *elem_ops,
                           int64_t fal_size, const zell_value_ops *fal_ops, int64_t fal_err_off,
                           int64_t tup_size, const zell_value_ops *tup_ops, int64_t tup_ok_off) {
    if (capacity < 0) zell_panic("negative channel capacity");
    zell_chan *c = chan_new(elem_size, elem_ops, capacity);
    c->fal_size = fal_size;
    c->fal_ops = fal_ops;
    c->fal_err_off = fal_err_off;
    c->tup_size = tup_size;
    c->tup_ops = tup_ops;
    c->tup_ok_off = tup_ok_off;
    return (int64_t)(intptr_t)c;
}

void zell_task_chan_release(int64_t h) { zell_release((zell_chan *)(intptr_t)h); }
zell_task *zell_task_chan_send(int64_t h, const void *elem) { return chan_send(chan_of(h), elem); }
zell_task *zell_task_chan_recv(int64_t h) { return chan_recv(chan_of(h), RES_FALLIBLE); }
zell_task *zell_task_chan_next(int64_t h) { return chan_recv(chan_of(h), RES_TUPLE); }
void zell_task_chan_close(int64_t h) { chan_close(chan_of(h)); }
int64_t zell_task_chan_cap(int64_t h) { return chan_of(h)->capacity; }

int64_t zell_task_chan_len(int64_t h) {
    zell_chan *c = chan_of(h);
    chan_lock_acquire(&c->mu);
    int64_t n = c->count;
    chan_lock_release(&c->mu);
    return n;
}

bool zell_task_chan_is_closed(int64_t h) {
    zell_chan *c = chan_of(h);
    chan_lock_acquire(&c->mu);
    bool closed = c->closed != 0;
    chan_lock_release(&c->mu);
    return closed;
}

/* ---- the C API (10.7): receives have the { int64 ok; T value @8 } layout ---- */

zell_chan *zell_chan_new(int64_t elem_size, const zell_value_ops *ops, int64_t capacity) {
    return chan_new(elem_size, ops, capacity);
}

zell_task *zell_chan_send_async(zell_chan *c, const void *elem) {
    if (!c) zell_panic_nil();
    return chan_send(c, elem);
}

zell_task *zell_chan_recv_async(zell_chan *c) {
    if (!c) zell_panic_nil();
    return chan_recv(c, RES_C);
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
        elem_retain(c, out);
    }
    zell_release(t);
    return ok;
}

void zell_chan_close(zell_chan *c) {
    if (!c) zell_panic_nil();
    chan_close(c);
}

int64_t zell_chan_len(const zell_chan *c) {
    return c ? zell_task_chan_len((int64_t)(intptr_t)c) : 0;
}

/* ======================================================================
 * Mutex: three words inline in the Zell struct (10.6): m[0] is the state,
 * m[1] and m[2] the head and tail of the FIFO of waiting tasks (linked
 * through their slots). State bits: 0 locked, 1 the guard of the queue, 2
 * waiters queued (set with the queue under the guard); higher bits are
 * reserved (R5: barging with a starvation threshold extends the word,
 * keeping 0 = free and 1 = locked with nothing else to do). Lock of a free
 * mutex is one compare-and-swap 0 -> 1 and returns the immortal done task;
 * Unlock of a mutex nobody waits for is one compare-and-swap 1 -> 0. The
 * compiler inlines both (ABI.md 10.6) and calls the functions below only
 * when the CAS fails. Unlock hands the lock to the oldest waiter by
 * completing its task, which runs next on the unlocking worker.
 *
 * Invariant: the locked bit is set only from a state with the guard clear
 * (a CAS from 0, the free unguarded state), or by the guard's holder. A
 * slow Lock that takes the guard and finds the mutex freed meanwhile
 * (state == guard) stores LOCKED with a plain store, so a fast Lock or
 * TryLock that CASed the locked bit in under the guard would be lost: two
 * owners, and the second Unlock panics "unlock of unlocked mutex". The
 * waiters bit is set only while locked, so a free mutex is 0 or guarded.
 * ====================================================================== */

enum { MU_LOCKED = 1, MU_GUARD = 2, MU_WAITERS = 4 };

static inline void mu_guard(int64_t *m) {
    for (int spins = 0;; spins++) {
        int64_t s = __atomic_load_n(m, __ATOMIC_RELAXED);
        if (!(s & MU_GUARD) && __atomic_compare_exchange_n(m, &s, s | MU_GUARD, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) return;
        if (spins > 64) zell__plat_yield();
    }
}

static inline zell_task **mu_next(zell_task *t) { return zell__task_slot(t); }

zell_task *zell_task_mutex_lock(int64_t *m) {
    if (!m) zell_panic_nil();
    int64_t s = __atomic_load_n(m, __ATOMIC_RELAXED);
    if (s == 0 && __atomic_compare_exchange_n(m, &s, MU_LOCKED, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
        return zell_task_done();   /* only from free and unguarded (the invariant above) */
    mu_guard(m);
    s = __atomic_load_n(m, __ATOMIC_RELAXED);
    if (!(s & MU_LOCKED)) {   /* freed while we waited for the guard */
        __atomic_store_n(m, MU_LOCKED, __ATOMIC_RELEASE);
        return zell_task_done();
    }
    zell_task *t = zell__task_new_slot(0, NULL, 0, (int64_t)sizeof(zell_task *));
    zell_retain(t);   /* the queue's reference */
    *mu_next(t) = NULL;
    zell_task *tail = (zell_task *)(intptr_t)m[2];
    if (tail) *mu_next(tail) = t; else m[1] = (int64_t)(intptr_t)t;
    m[2] = (int64_t)(intptr_t)t;
    __atomic_store_n(m, MU_LOCKED | MU_WAITERS, __ATOMIC_RELEASE);   /* guard off */
    return t;
}

bool zell_task_mutex_try_lock(int64_t *m) {
    if (!m) zell_panic_nil();
    /* A guarded state is locked or about to be: the guard of a free mutex
     * is held only by a slow Lock, which then takes it (the invariant). */
    int64_t s = 0;
    return __atomic_compare_exchange_n(m, &s, MU_LOCKED, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED);
}

void zell_task_mutex_unlock(int64_t *m) {
    if (!m) zell_panic_nil();
    mu_guard(m);
    int64_t s = __atomic_load_n(m, __ATOMIC_RELAXED);
    if (!(s & MU_LOCKED)) {
        __atomic_store_n(m, s & ~(int64_t)MU_GUARD, __ATOMIC_RELEASE);
        zell_panic("unlock of unlocked mutex");
    }
    zell_task *t = (zell_task *)(intptr_t)m[1];
    if (!t) {
        __atomic_store_n(m, 0, __ATOMIC_RELEASE);   /* unlocked, guard off */
        return;
    }
    m[1] = (int64_t)(intptr_t)*mu_next(t);
    if (!m[1]) m[2] = 0;
    /* ownership passes to t; guard off */
    __atomic_store_n(m, m[1] ? MU_LOCKED | MU_WAITERS : MU_LOCKED, __ATOMIC_RELEASE);
    zell_task_complete(t, NULL);
    zell_release(t);
}

/* Releases the waiters of a mutex that is dropped while tasks wait on it
 * (the C API's zell_mutex; the Zell struct has no destructor and leaks). */
static void mutex_words_drop(int64_t *m) {
    zell_task *t = (zell_task *)(intptr_t)m[1];
    while (t) {
        zell_task *n = *mu_next(t);
        zell_release(t);
        t = n;
    }
}

/* ---- the C API (10.6) ---- */

struct zell_mutex {
    zell_object hdr;
    int64_t m[3];
};

static void mutex_drop(void *obj) { mutex_words_drop(((zell_mutex *)obj)->m); }

static const zell_type_info mutex_type = { "Mutex", (int64_t)sizeof(struct zell_mutex), mutex_drop };

zell_mutex *zell_mutex_new(void) { return zell_object_new(&mutex_type, (int64_t)sizeof(struct zell_mutex)); }

zell_task *zell_mutex_lock_async(zell_mutex *m) {
    if (!m) zell_panic_nil();
    return zell_task_mutex_lock(m->m);
}

void zell_mutex_lock(zell_mutex *m) {
    zell_task *t = zell_mutex_lock_async(m);
    zell_task_await(t);
    zell_release(t);
}

bool zell_mutex_try_lock(zell_mutex *m) {
    if (!m) zell_panic_nil();
    return zell_task_mutex_try_lock(m->m);
}

void zell_mutex_unlock(zell_mutex *m) {
    if (!m) zell_panic_nil();
    zell_task_mutex_unlock(m->m);
}
