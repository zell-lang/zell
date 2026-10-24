/* rc.c - biased reference counting (ABI.md 3.1/3.2, DESIGN.md "Память").
 *
 * Most objects are only ever retained and released by the thread that uses
 * them, but once the process has a second thread every count update had to
 * be an atomic RMW. Biased counting (Choi et al., PACT 2018; the scheme of
 * Swift's and free-threaded CPython's refcounts) splits the count of an
 * object into a part B owned by one thread, updated with plain 16-bit
 * loads and stores, and a shared part S that other threads update with
 * atomic RMW; the count is B + S. The layout of the word is in zell_rt.h.
 *
 * Owners. A thread that may own objects has a *slot* (1..4095) and the
 * owner tag zell_rc_owner = (BIASED >> 47) | slot: scheduler workers and
 * the thread that called zell_runtime_init (zell__rc_thread_enable).
 * Other threads (timers, pollers, thread, foreign threads) never own
 * objects; their operations on biased objects are the atomic ones.
 *
 * Biasing. Objects are created unowned (count 1). The first retain of an
 * unowned, non-cyclic object with count exactly 1 and no QUEUED flag by a
 * thread with a slot, while the process is threaded, turns it into
 * BIASED | owner | S = 0 | B = 2 with one CAS (a count of 1 means one
 * holder, so this is usually the creating thread). Single-threaded
 * processes never bias: their unowned counts are already updated with
 * plain stores (zell_threaded). CYCLIC objects are never biased (the cycle
 * collector needs every count change to be an atomic RMW it can check).
 *
 * The protocol (o biased, owned by slot T):
 *  - retain by T: B += 1 (plain store); at B == 0xFFFF, one CAS moves
 *    0x8000 from B to S first.
 *  - release by T with B >= 2: B -= 1 (plain store).
 *  - release by T with B == 1: if S == 0 and neither QUEUED nor WEAK is set,
 *    T held the only reference: o is freed without an atomic. Otherwise a
 *    CAS sets B = 0; if then B + S == 0, o is dead: freed now, unless QUEUED
 *    (the queue frees it).
 *  - release by T with B == 0 (its references moved to S at an overflow)
 *    and every release by another thread: S -= 1 with one atomic add whose
 *    result is a snapshot of the whole word. If B + S became 0, the caller
 *    released the last reference: o is freed (unless QUEUED). Otherwise, if
 *    S became negative and QUEUED was not set, the caller sets QUEUED and
 *    pushes o onto T's queue: B + S may now be 0 without anyone noticing
 *    (T may drop its last reference with a plain store), so T must look.
 *  - retain by another thread: S += 1 with one atomic add.
 * A plain store by T writes only the B half-word; an atomic add of
 * +-1 << 16 never changes B, so neither loses the other's update.
 *
 * Merging. The *queue processor* of slot T is T itself (at the slow path of
 * its allocations, before its worker sleeps, at exit) or, while T is parked
 * (an idle worker, a blocking await) or has exited, any thread that takes
 * T's slot lock (`held`, which T keeps while it runs; the thread pushing to
 * a queue whose slot is free drains it). For each queued object it reads
 * B + S: zero means dead (freed); otherwise one CAS folds a negative S into
 * B (B += S, S = 0) and clears QUEUED, so the object stays biased. Nothing
 * else ever changes B while T runs, so these CASes race only with atomic
 * adds to S and plain stores by T itself (they retry).
 *
 * Deallocation condition: a biased object is freed by exactly one thread,
 * the one that observes B + S == 0 in an atomic snapshot it produced (the
 * owner's CAS of B 1 -> 0, another thread's add to S, or the queue
 * processor's read of an object that is QUEUED: queued objects are freed
 * only by the processor), or by a holder that read B + S == 1 with neither
 * QUEUED nor WEAK set (it held the only reference; with a weak box the
 * transition must be atomic, because zell_weak_get may resurrect).
 *
 * Owner exit. A thread's slot is given up when it exits (TLS destructor):
 * it drains its queue and releases the slot lock. Its objects stay biased
 * with the dead slot: every thread now updates them with the atomic S
 * operations (the global "unowned" state), and their queue entries are
 * merged by whichever thread pushes them (it takes the free slot lock).
 * A new thread that takes the slot later inherits them as their owner;
 * taking the slot lock first waits for any merging in progress, so plain
 * stores by the new owner never race with a merge.
 *
 * Limits: S must stay within +-2^30 (minus a margin for adds in flight);
 * beyond that the runtime panics ("reference count overflow"). 4095 slots;
 * threads beyond that work without biasing. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <stdio.h>
#include <stdlib.h>

#if defined(__BYTE_ORDER__) && __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error "biased refcounts assume the owner half-word B is at the object's address (little-endian)"
#endif

#define B_OF(w) ((w) & ZELL_RC_B_MASK)
#define SF_OF(w) (((w) >> ZELL_RC_S_SHIFT) & ZELL_RC_S_FIELD)
#define S_OF(w) (SF_OF(w) - ZELL_RC_S_EXCESS)
#define SLOT_OF(w) ((int)(((uint64_t)(w) >> ZELL_RC_OWNER_SHIFT) & 0xFFF))
#define S_ONE ((int64_t)1 << ZELL_RC_S_SHIFT)
#define S_MARGIN ((int64_t)1 << 20)
#define OWNER_TAG(slot) ((ZELL_RC_BIASED >> ZELL_RC_OWNER_SHIFT) | (int64_t)(slot))

_Thread_local int64_t zell_rc_owner = ZELL_RC_NO_OWNER;
static _Thread_local int t_slot;            /* 0: none */
static _Thread_local int t_eligible;        /* may own objects (gets a slot at its first biasing) */
static _Thread_local int t_held;            /* holds its slot lock (not parked) */

typedef struct qnode { zell_object *o; struct qnode *next; } qnode;
typedef struct {
    int held;            /* atomic: the slot lock (its thread keeps it while running) */
    int live;            /* atomic: a thread has the slot */
    qnode *queue;        /* atomic: objects waiting for a merge (a stack) */
    int owner_holds;     /* the owner holds `held` (t_held, for its exit: see thread_exit) */
} rc_slot;
static rc_slot g_slots[ZELL_RC_OWNER_SLOTS];
static int g_slot_hint;
static zell__plat_once g_once = ZELL_PLAT_ONCE_INIT;
static zell__plat_tls_key g_key;
static int g_keyed;
/* ZELL_RC_STATS=1: slow-path counters printed at shutdown (relaxed). */
static int64_t s_biased, s_queued, s_delegated, s_merged;

static inline int is_owner(int64_t w) {
    return (int64_t)(((uint64_t)w >> ZELL_RC_OWNER_SHIFT) & ZELL_RC_OWNER_MASK) == zell_rc_owner;
}

/* Release: a thread that later drops the last reference (acquire CAS on the
 * whole word) must see every write the owner made before this store. */
static inline void store_b(zell_object *o, int64_t b) {
    __atomic_store_n((uint16_t *)&o->refcount, (uint16_t)b, __ATOMIC_RELEASE);
}

static ZELL_NORETURN void overflow(void) { zell__panicf("reference count overflow (more than 2^30 references from other threads)"); }

/* The object is dead (count zero, observed by the caller): drop and free. */
static void dead_fin(zell_object *o, int64_t w) {
    (void)w;
    o->type->drop(o);
    zell__object_free(o);
}

static void dead(zell_object *o, int64_t w) {
    if (w & ZELL_RC_WEAK_FLAG) zell__weak_clear(o);
    if (!(o->type && o->type->drop)) {
        zell__object_free(o);
        return;
    }
    if (!zell__dead_enter()) {
        zell__dead_push(dead_fin, o, w);
        return;
    }
    o->type->drop(o);
    zell__dead_leave();
    zell__object_free(o);
}

/* ---- slots ---- */

static inline void slot_lock(rc_slot *s) {
    int z = 0;
    while (!__atomic_compare_exchange_n(&s->held, &z, 1, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
        z = 0;
        zell__plat_yield();
    }
}

static inline int slot_trylock(rc_slot *s) {
    int z = 0;
    return !__atomic_load_n(&s->held, __ATOMIC_RELAXED) &&
           __atomic_compare_exchange_n(&s->held, &z, 1, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED);
}

static inline void slot_unlock(rc_slot *s) { __atomic_store_n(&s->held, 0, __ATOMIC_RELEASE); }

/* Merges every object queued on s (its lock held). */
static void process(rc_slot *s) {
    qnode *q = __atomic_exchange_n(&s->queue, (qnode *)NULL, __ATOMIC_ACQUIRE);
    while (q) {
        qnode *next = q->next;
        zell_object *o = q->o;
        free(q);
        __atomic_add_fetch(&s_merged, 1, __ATOMIC_RELAXED);
        int64_t w = __atomic_load_n(&o->refcount, __ATOMIC_ACQUIRE);
        for (;;) {
            int64_t total = B_OF(w) + S_OF(w);
            if (total == 0) {
                dead(o, w);
                break;
            }
            int64_t nw = w & ~ZELL_RC_QUEUED;
            if (S_OF(w) < 0) {
                /* B += S, S = 0 (total < B, so it fits B) */
                nw = (nw & ~(ZELL_RC_B_MASK | (ZELL_RC_S_FIELD << ZELL_RC_S_SHIFT))) |
                     (ZELL_RC_S_EXCESS << ZELL_RC_S_SHIFT) | total;
            }
            if (__atomic_compare_exchange_n(&o->refcount, &w, nw, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) break;
        }
        q = next;
    }
}

/* Drains slot s's queue as a delegate while its lock is free. */
static void delegate(rc_slot *s) {
    while (__atomic_load_n(&s->queue, __ATOMIC_ACQUIRE) && slot_trylock(s)) {
        __atomic_add_fetch(&s_delegated, 1, __ATOMIC_RELAXED);
        process(s);
        slot_unlock(s);
    }
}

static void push(int slot, zell_object *o) {
    qnode *n = malloc(sizeof *n);
    if (!n) zell__panicf("out of memory (refcount queue)");
    n->o = o;
    __atomic_add_fetch(&s_queued, 1, __ATOMIC_RELAXED);
    rc_slot *s = &g_slots[slot];
    qnode *head = __atomic_load_n(&s->queue, __ATOMIC_RELAXED);
    do {
        n->next = head;
    } while (!__atomic_compare_exchange_n(&s->queue, &head, n, 1, __ATOMIC_RELEASE, __ATOMIC_RELAXED));
    if (slot == t_slot) return;   /* our own (an owner that moved B to S): polled */
    delegate(s);                  /* the owner is parked or gone: merge now */
}

/* The key's value is the slot: thread-locals may already be gone when key
 * destructors run (macOS frees them first), so nothing here reads one. */
static void thread_exit(void *arg) {
    int slot = (int)(intptr_t)arg;
    if (slot <= 0 || slot >= ZELL_RC_OWNER_SLOTS) return;
    rc_slot *s = &g_slots[slot];
    if (!s->owner_holds) slot_lock(s);
    s->owner_holds = 0;
    t_held = 0;
    process(s);
    t_slot = 0;
    zell_rc_owner = ZELL_RC_NO_OWNER;
    __atomic_store_n(&s->live, 0, __ATOMIC_RELEASE);
    slot_unlock(s);
    delegate(s);   /* pushes that raced with the exit */
}

static void key_init(void) { g_keyed = zell__plat_tls_new(&g_key, thread_exit); }

void (*zell__rc_drain_fn)(void);

void zell__rc_thread_enable(void) {
    if (t_slot) return;
    t_eligible = 0;   /* one attempt: with no free slot the thread stays unowned */
    zell__plat_once_run(&g_once, key_init);
    if (!g_keyed) return;
    int start = __atomic_load_n(&g_slot_hint, __ATOMIC_RELAXED);
    for (int k = 0; k < ZELL_RC_OWNER_SLOTS - 1; k++) {
        int i = 1 + (start + k) % (ZELL_RC_OWNER_SLOTS - 1);
        int z = 0;
        if (__atomic_load_n(&g_slots[i].live, __ATOMIC_RELAXED) ||
            !__atomic_compare_exchange_n(&g_slots[i].live, &z, 1, 0, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED))
            continue;
        slot_lock(&g_slots[i]);   /* waits for a delegate merging the slot's old objects */
        t_held = 1;
        g_slots[i].owner_holds = 1;
        __atomic_store_n(&g_slot_hint, i, __ATOMIC_RELAXED);
        t_slot = i;
        zell_rc_owner = OWNER_TAG(i);
        zell__rc_drain_fn = zell__rc_drain_all;
        zell__plat_tls_set(g_key, (void *)(intptr_t)i);
        return;
    }
}

/* ZELL_NO_BIAS=1: no thread owns objects (every count stays unowned). */
static int g_no_bias = -1;

void zell__rc_thread_eligible(void) {
    int nb = __atomic_load_n(&g_no_bias, __ATOMIC_RELAXED);
    if (nb < 0) {
        const char *e = getenv("ZELL_NO_BIAS");
        nb = e && *e && *e != '0';
        __atomic_store_n(&g_no_bias, nb, __ATOMIC_RELAXED);
    }
    t_eligible = !nb;
}

void zell__rc_poll(void) {
    int slot = t_slot;
    if (slot && t_held && __atomic_load_n(&g_slots[slot].queue, __ATOMIC_RELAXED)) process(&g_slots[slot]);
}

int zell__rc_pending(void) {
    int slot = t_slot;
    return slot && t_held && __atomic_load_n(&g_slots[slot].queue, __ATOMIC_RELAXED) != NULL;
}

void zell__rc_park(int merge) {
    int slot = t_slot;
    if (!slot || !t_held) return;
    if (merge) process(&g_slots[slot]);
    t_held = 0;
    g_slots[slot].owner_holds = 0;
    slot_unlock(&g_slots[slot]);
    if (merge) delegate(&g_slots[slot]);
}

void zell__rc_unpark(void) {
    int slot = t_slot;
    if (!slot || t_held) return;
    slot_lock(&g_slots[slot]);
    t_held = 1;
    g_slots[slot].owner_holds = 1;
}

void zell__rc_drain_all(void) {
    /* Merging frees objects whose destructors may queue more: repeat until
     * every queue is empty (our own included). */
    /* (bounded: a slot held by a thread that still runs is merged by it) */
    for (int again = 1, round = 0; again && round < 10000; round++) {
        again = 0;
        for (int i = 1; i < ZELL_RC_OWNER_SLOTS; i++) {
            if (!__atomic_load_n(&g_slots[i].queue, __ATOMIC_ACQUIRE)) continue;
            if (i == t_slot && t_held) process(&g_slots[i]);
            else delegate(&g_slots[i]);
            if (__atomic_load_n(&g_slots[i].queue, __ATOMIC_ACQUIRE)) again = 1;
        }
        if (again) zell__plat_yield();
    }
    const char *e = getenv("ZELL_RC_STATS");
    if (e && *e && *e != '0')
        fprintf(stderr, "rc: %lld objects biased, %lld queued for a merge (%lld merged), %lld delegated merges\n",
                (long long)__atomic_load_n(&s_biased, __ATOMIC_RELAXED), (long long)__atomic_load_n(&s_queued, __ATOMIC_RELAXED),
                (long long)__atomic_load_n(&s_merged, __ATOMIC_RELAXED), (long long)__atomic_load_n(&s_delegated, __ATOMIC_RELAXED));
}

/* ---- the operations (from mem.c's zell_retain / zell_release) ---- */

void zell__rc_retain_biased(zell_object *o, int64_t w) {
    if (is_owner(w)) {
        int64_t b = B_OF(w);
        if (b != ZELL_RC_B_MASK) {
            store_b(o, b + 1);
            return;
        }
        /* B is full: move half of it to S (and count this retain). */
        for (;;) {
            if (SF_OF(w) + 0x8000 > ZELL_RC_S_FIELD - S_MARGIN) overflow();
            int64_t nw = w - 0x8000 + 1 + 0x8000 * S_ONE;
            if (__atomic_compare_exchange_n(&o->refcount, &w, nw, 1, __ATOMIC_RELAXED, __ATOMIC_RELAXED)) return;
        }
    }
    int64_t old = __atomic_fetch_add(&o->refcount, S_ONE, __ATOMIC_RELAXED);
    if (SF_OF(old) > ZELL_RC_S_FIELD - S_MARGIN) overflow();
}

void zell__rc_release_biased(zell_object *o, int64_t w) {
    if (is_owner(w)) {
        int64_t b = B_OF(w);
        if (b >= 2) {
            store_b(o, b - 1);
            return;
        }
        if (b == 1) {
            for (;;) {
                if (S_OF(w) == 0 && !(w & (ZELL_RC_QUEUED | ZELL_RC_WEAK_FLAG))) {
                    dead(o, w);   /* the only reference */
                    return;
                }
                if (__atomic_compare_exchange_n(&o->refcount, &w, w - 1, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) break;
                if (B_OF(w) != 1) {   /* cannot happen: only we change B */
                    zell__panicf("biased refcount: owner count changed concurrently");
                }
            }
            if (S_OF(w) == 0 && !(w & ZELL_RC_QUEUED)) dead(o, w - 1);
            zell__rc_poll();
            return;
        }
        /* b == 0: our references were moved to S; fall through. */
    }
    /* S -= 1 with a CAS: whoever makes S negative must set QUEUED in the
     * same atomic step, or another thread could see the count reach zero
     * (and free) in between. */
    for (;;) {
        int64_t s = S_OF(w) - 1;
        if (SF_OF(w) < S_MARGIN) overflow();
        int64_t nw = w - S_ONE;
        int q = 0;
        if (s < 0 && !(w & ZELL_RC_QUEUED) && B_OF(w) + s != 0) {
            nw |= ZELL_RC_QUEUED;
            q = 1;
        }
        if (__atomic_compare_exchange_n(&o->refcount, &w, nw, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            if (B_OF(w) + s == 0) {
                if (!(w & ZELL_RC_QUEUED)) dead(o, nw);   /* the last reference */
            } else if (q) {
                push(SLOT_OF(w), o);
            }
            return;
        }
    }
}

void zell__rc_retain_unowned(zell_object *o, int64_t w) {
    if (__builtin_expect(t_eligible && !t_slot, 0)) zell__rc_thread_enable();
    for (;;) {
        if (w & ZELL_RC_BIASED) {
            zell__rc_retain_biased(o, w);
            return;
        }
        int64_t nw = w + 1;
        /* One holder and we may own objects: bias it to us (B = 2). */
        if ((w & ~ZELL_RC_WEAK_FLAG) == 1 && zell_rc_owner != ZELL_RC_NO_OWNER)
            nw = (w & ZELL_RC_WEAK_FLAG) | ZELL_RC_BIASED | ((zell_rc_owner & 0xFFF) << ZELL_RC_OWNER_SHIFT) |
                 (ZELL_RC_S_EXCESS << ZELL_RC_S_SHIFT) | 2;
        if (__atomic_compare_exchange_n(&o->refcount, &w, nw, 1, __ATOMIC_RELAXED, __ATOMIC_RELAXED)) {
            if (nw & ZELL_RC_BIASED) __atomic_add_fetch(&s_biased, 1, __ATOMIC_RELAXED);
            return;
        }
    }
}

void zell__rc_release_unowned(zell_object *o, int64_t w) {
    for (;;) {
        if (w & ZELL_RC_BIASED) {
            zell__rc_release_biased(o, w);
            return;
        }
        if (__atomic_compare_exchange_n(&o->refcount, &w, w - 1, 1, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) break;
    }
    if (((w - 1) & ZELL_RC_COUNT_MASK) == 0) dead(o, w - 1);
}

int64_t zell__rc_count(int64_t w) {
    if (w < 0) return w;
    if ((w & ZELL_RC_BIASED) && !(w & ZELL_RC_CYCLIC)) return B_OF(w) + S_OF(w);
    return w & ZELL_RC_COUNT_MASK;
}

int zell__rc_unique(const zell_object *o) {
    int64_t w = __atomic_load_n(&o->refcount, __ATOMIC_ACQUIRE);
    if (w == 1) return 1;
    if ((w & (ZELL_RC_BIASED | ZELL_RC_CYCLIC | ZELL_RC_QUEUED | ZELL_RC_WEAK_FLAG)) != ZELL_RC_BIASED || w < 0) return 0;
    return B_OF(w) + S_OF(w) == 1;
}

int zell__rc_try_retain_biased(zell_object *o) {
    int64_t w = __atomic_load_n(&o->refcount, __ATOMIC_RELAXED);
    for (;;) {
        if (B_OF(w) + S_OF(w) <= 0) return 0;
        if (SF_OF(w) > ZELL_RC_S_FIELD - S_MARGIN) overflow();
        if (__atomic_compare_exchange_n(&o->refcount, &w, w + S_ONE, 1, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) return 1;
    }
}
