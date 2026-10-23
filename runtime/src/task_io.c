/* task_io.c - readiness waits for async I/O (ABI.md 10.9).
 *
 * Sharded pollers. The runtime runs S poller shards (S = ZELL_IO_POLLERS,
 * default: half the scheduler workers, at least 1, at most 8). Each shard
 * owns one platform poller (an epoll instance on Linux, a kqueue on
 * macOS/BSD), one thread that waits on it, a mutex, and a binary min-heap
 * of the deadlines of its waits. A descriptor is assigned to the least
 * loaded shard when it is registered (zell_io_register at socket
 * creation, or its first wait) and stays there until zell_io_forget, so
 * the shards share nothing on the hot path: two connections on different
 * shards never touch the same lock or poller.
 *
 * zell_io_wait_readable/writable(fd, timeout) add a pending Task<int> to
 * the fd's reader or writer list. With a one-shot poller (io_uring,
 * kqueue, IOCP, WSAPoll) the wait arms the fd in its shard's poller
 * (level-triggered: readiness that is already there is reported at once).
 * With an edge-triggered poller (epoll) the fd is added once, at its first
 * wait, and a wait costs no system call: a report that finds no waiter of
 * a direction is kept in the fd's `ready` bits, and the next wait of that
 * direction consumes it and completes at once. That is only correct
 * because every caller waits after an operation that failed with EAGAIN
 * (or a short stream read, which drained the socket): readiness that came
 * before is either consumed by that operation or still in `ready`.
 * When the poller reports the fd (or
 * the deadline passes, or zell_io_forget releases the fd, or the waiting
 * task is cancelled) the waits are completed, which posts their
 * continuations to the scheduler like a timer does. Every waiter of a
 * direction is woken; reports may be spurious and callers retry the
 * operation, so a stale report is harmless. Events carry a token of
 * (generation << 32 | fd): the generation changes whenever the fd is
 * forgotten, so a report that raced with close + reuse of the number is
 * dropped. The descriptor itself must be in non-blocking mode. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <stdlib.h>
#include <string.h>

enum { IO_READY = 0, IO_TIMEOUT = 1, IO_CLOSED = 2, IO_CANCELLED = 3 };

typedef struct io_wait {
    zell_task *task;        /* owned */
    int64_t deadline;        /* monotonic ns, 0 = none */
    int64_t fd;
    int32_t heap;            /* index in the shard's deadline heap, -1 = none */
    int32_t write;
    struct io_wait *next;    /* the fd's list for this direction */
} io_wait;

/* Guarded by the owning shard's mutex, except `shard` (atomic). */
typedef struct io_fd {
    io_wait *rd, *wr;
    int32_t shard;           /* owning shard, -1 = unassigned */
    uint32_t gen;            /* bumped when the fd is forgotten */
    int32_t armed;           /* interest armed (one-shot) in the poller */
    int32_t registered;      /* added to the shard's poller */
    int32_t ready;           /* edge-triggered: reported directions nobody waited for */
    int32_t sticky;          /* edge-triggered: directions ready for good (peer shutdown, error) */
} io_fd;

typedef struct io_shard {
    zell__plat_mutex mu;
    zell__plat_poller *poller;
    zell__plat_thread thread;
    io_wait **heap;
    int32_t nheap, capheap;
    int64_t sleep_until;     /* deadline the thread sleeps toward, INT64_MAX = none */
    int64_t nfds;            /* atomic: descriptors assigned (load) */
    int32_t stopping;
    int32_t index;
    bool edge;               /* the poller is edge-triggered (armed once per fd) */
} io_shard;

typedef struct io_done {
    zell_task *task;        /* owned */
    int64_t code;
} io_done;

#define IO_CHUNK_BITS 12
#define IO_CHUNK (1 << IO_CHUNK_BITS)
#define IO_MAX_CHUNKS 4096   /* descriptors up to 16M */

static io_fd *io_chunks[IO_MAX_CHUNKS];  /* atomic pointers, never moved */
static zell__plat_mutex io_start_mu = ZELL_PLAT_MUTEX_INIT;
static io_shard *io_shards;
static int32_t io_nshards;
static int32_t io_running;               /* atomic */
static uint32_t io_rr;                   /* atomic: round-robin tie breaker */

/* ---- descriptor table ---- */

static io_fd *entry(int64_t fd, bool create) {
    if (fd < 0 || fd >= (int64_t)IO_MAX_CHUNKS * IO_CHUNK) {
        if (!create) return NULL;
        zell__panicf("zell_io_wait: file descriptor %lld out of range", (long long)fd);
    }
    int64_t ci = fd >> IO_CHUNK_BITS;
    io_fd *c = __atomic_load_n(&io_chunks[ci], __ATOMIC_ACQUIRE);
    if (!c) {
        if (!create) return NULL;
        io_fd *n = calloc(IO_CHUNK, sizeof *n);
        if (!n) zell_panic("out of memory (io)");
        for (int i = 0; i < IO_CHUNK; i++) n[i].shard = -1;
        io_fd *expected = NULL;
        if (__atomic_compare_exchange_n(&io_chunks[ci], &expected, n, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) c = n;
        else { free(n); c = expected; }
    }
    return &c[fd & (IO_CHUNK - 1)];
}

static uint64_t token_of(const io_fd *e, int64_t fd) { return ((uint64_t)e->gen << 32) | (uint64_t)(uint32_t)fd; }

/* The least loaded shard (ties: round robin). */
static int32_t pick_shard(void) {
    int32_t n = io_nshards;
    int32_t start = (int32_t)(__atomic_fetch_add(&io_rr, 1, __ATOMIC_RELAXED) % (uint32_t)n);
    int32_t best = start;
    int64_t load = __atomic_load_n(&io_shards[start].nfds, __ATOMIC_RELAXED);
    for (int32_t k = 1; k < n; k++) {
        int32_t i = (start + k) % n;
        int64_t l = __atomic_load_n(&io_shards[i].nfds, __ATOMIC_RELAXED);
        if (l < load) { load = l; best = i; }
    }
    return best;
}

/* Locks and returns the shard that owns e, assigning one if `assign`;
 * NULL if e is unassigned and !assign. */
static io_shard *shard_lock(io_fd *e, bool assign) {
    for (;;) {
        int32_t s = __atomic_load_n(&e->shard, __ATOMIC_ACQUIRE);
        if (s < 0) {
            if (!assign) return NULL;
            int32_t pick = pick_shard();
            if (!__atomic_compare_exchange_n(&e->shard, &s, pick, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) continue;
            __atomic_add_fetch(&io_shards[pick].nfds, 1, __ATOMIC_RELAXED);
            s = pick;
        }
        io_shard *sh = &io_shards[s];
        zell__plat_mutex_lock(&sh->mu);
        if (__atomic_load_n(&e->shard, __ATOMIC_ACQUIRE) == s) return sh;
        zell__plat_mutex_unlock(&sh->mu);
    }
}

/* ---- deadline heap (shard mutex held) ---- */

static void heap_swap(io_shard *sh, int32_t a, int32_t b) {
    io_wait *t = sh->heap[a];
    sh->heap[a] = sh->heap[b];
    sh->heap[b] = t;
    sh->heap[a]->heap = a;
    sh->heap[b]->heap = b;
}

static void heap_up(io_shard *sh, int32_t i) {
    while (i > 0) {
        int32_t p = (i - 1) / 2;
        if (sh->heap[p]->deadline <= sh->heap[i]->deadline) break;
        heap_swap(sh, i, p);
        i = p;
    }
}

static void heap_down(io_shard *sh, int32_t i) {
    for (;;) {
        int32_t l = 2 * i + 1, r = l + 1, m = i;
        if (l < sh->nheap && sh->heap[l]->deadline < sh->heap[m]->deadline) m = l;
        if (r < sh->nheap && sh->heap[r]->deadline < sh->heap[m]->deadline) m = r;
        if (m == i) break;
        heap_swap(sh, i, m);
        i = m;
    }
}

static void heap_push(io_shard *sh, io_wait *w) {
    if (sh->nheap == sh->capheap) {
        int32_t cap = sh->capheap ? sh->capheap * 2 : 64;
        io_wait **nh = realloc(sh->heap, sizeof *nh * (size_t)cap);
        if (!nh) zell_panic("out of memory (io)");
        sh->heap = nh;
        sh->capheap = cap;
    }
    w->heap = sh->nheap++;
    sh->heap[w->heap] = w;
    heap_up(sh, w->heap);
}

static void heap_remove(io_shard *sh, io_wait *w) {
    int32_t i = w->heap;
    if (i < 0) return;
    w->heap = -1;
    int32_t last = --sh->nheap;
    if (i != last) {
        sh->heap[i] = sh->heap[last];
        sh->heap[i]->heap = i;
        heap_down(sh, i);
        heap_up(sh, i);
    }
}

/* ---- completions ---- */

typedef struct done_buf {
    io_done *v;
    int n, cap;
    io_done local[8];
} done_buf;

static void done_init(done_buf *d) {
    d->v = d->local;
    d->n = 0;
    d->cap = 8;
}

static void done_push(done_buf *d, zell_task *t, int64_t code) {
    if (d->n == d->cap) {
        int cap = d->cap * 2;
        io_done *nv = malloc(sizeof *nv * (size_t)cap);
        if (!nv) zell_panic("out of memory (io)");
        memcpy(nv, d->v, sizeof *nv * (size_t)d->n);
        if (d->v != d->local) free(d->v);
        d->v = nv;
        d->cap = cap;
    }
    d->v[d->n].task = t;
    d->v[d->n].code = code;
    d->n++;
}

/* Completes the collected waits (no lock held) and empties the buffer. */
static void done_finish(done_buf *d) {
    for (int i = 0; i < d->n; i++) {
        zell_task_complete(d->v[i].task, &d->v[i].code);
        zell_release(d->v[i].task);
    }
    d->n = 0;
}

static void done_free(done_buf *d) {
    if (d->v != d->local) free(d->v);
    done_init(d);
}

/* Shard mutex held: moves every waiter of the list to d. */
static void take_all(io_shard *sh, io_wait **list, int64_t code, done_buf *d) {
    io_wait *w = *list;
    *list = NULL;
    while (w) {
        io_wait *n = w->next;
        heap_remove(sh, w);
        done_push(d, w->task, code);
        free(w);
        w = n;
    }
}

/* Shard mutex held: removes w from its fd's list. */
static void unlink_wait(io_fd *e, io_wait *w) {
    io_wait **pp = w->write ? &e->wr : &e->rd;
    while (*pp && *pp != w) pp = &(*pp)->next;
    if (*pp) *pp = w->next;
}

/* Shard mutex held: arms the fd for whatever its waiters still need. */
static void rearm(io_shard *sh, io_fd *e, int64_t fd) {
    int32_t want = (e->rd ? ZELL_PLAT_READ : 0) | (e->wr ? ZELL_PLAT_WRITE : 0);
    if (want == 0 || (want & ~e->armed) == 0) return;
    if (zell__plat_poller_arm(sh->poller, fd, want, token_of(e, fd), e->registered != 0) == 0) {
        e->registered = 1;
        e->armed = want;
    }
}

/* ---- the poller threads ---- */

static void *io_main(void *arg) {
    io_shard *sh = arg;
    zell__plat_event evs[256];
    done_buf d;
    done_init(&d);
    zell__plat_mutex_lock(&sh->mu);
    for (;;) {
        if (sh->stopping) break;
        int64_t timeout = -1;
        sh->sleep_until = INT64_MAX;
        if (sh->nheap > 0) {
            int64_t next = sh->heap[0]->deadline;
            sh->sleep_until = next;
            int64_t left = next - zell__plat_mono_ns();
            timeout = left <= 0 ? 0 : (left + 999999) / 1000000;
        }
        zell__plat_mutex_unlock(&sh->mu);
        int n = zell__plat_poller_wait(sh->poller, evs, 256, timeout);
        zell__plat_mutex_lock(&sh->mu);
        for (int i = 0; i < n; i++) {
            if (evs[i].token == ZELL_PLAT_WAKE_TOKEN) continue;
            int64_t fd = (int64_t)(uint32_t)evs[i].token;
            uint32_t gen = (uint32_t)(evs[i].token >> 32);
            io_fd *e = entry(fd, false);
            /* stale: the fd was forgotten (and maybe reused) since it was armed */
            if (!e || __atomic_load_n(&e->shard, __ATOMIC_RELAXED) != sh->index || e->gen != gen) continue;
            if (sh->edge) {
                /* edge-triggered: wake the waiters, or keep the readiness for
                 * the next wait. End of stream and errors stay: a short read
                 * may return the last data without the EOF behind it, and no
                 * new edge would come for the EOF. */
                if (evs[i].events & ZELL_PLAT_HUP) e->sticky |= ZELL_PLAT_READ;
                if (evs[i].events & ZELL_PLAT_ERROR) e->sticky |= ZELL_PLAT_READ | ZELL_PLAT_WRITE;
                if (evs[i].events & ZELL_PLAT_READ) {
                    if (e->rd) take_all(sh, &e->rd, IO_READY, &d);
                    else e->ready |= ZELL_PLAT_READ;
                }
                if (evs[i].events & ZELL_PLAT_WRITE) {
                    if (e->wr) take_all(sh, &e->wr, IO_READY, &d);
                    else e->ready |= ZELL_PLAT_WRITE;
                }
                continue;
            }
            e->armed = 0;  /* one-shot: the report disarmed it */
            if (evs[i].events & ZELL_PLAT_READ) take_all(sh, &e->rd, IO_READY, &d);
            if (evs[i].events & ZELL_PLAT_WRITE) take_all(sh, &e->wr, IO_READY, &d);
            rearm(sh, e, fd);
        }
        int64_t now = zell__plat_mono_ns();
        while (sh->nheap > 0 && sh->heap[0]->deadline <= now) {
            io_wait *w = sh->heap[0];
            heap_remove(sh, w);
            io_fd *e = entry(w->fd, false);
            if (e) unlink_wait(e, w);
            done_push(&d, w->task, IO_TIMEOUT);
            free(w);
        }
        if (d.n) {
            zell__plat_mutex_unlock(&sh->mu);
            done_finish(&d);
            zell__plat_mutex_lock(&sh->mu);
        }
    }
    zell__plat_mutex_unlock(&sh->mu);
    done_free(&d);
    return NULL;
}

static int32_t shard_count(void) {
    const char *env = getenv("ZELL_IO_POLLERS");
    long n = env ? strtol(env, NULL, 10) : 0;
    if (n <= 0) n = (long)((zell_sched_workers() + 1) / 2);
    if (n < 1) n = 1;
    if (n > 8 && !env) n = 8;
    if (n > 64) n = 64;
    return (int32_t)n;
}

static void io_start(void) {
    if (__atomic_load_n(&io_running, __ATOMIC_ACQUIRE)) return;
    int32_t n = shard_count();  /* may start the scheduler: outside io_start_mu */
    zell__plat_mutex_lock(&io_start_mu);
    if (!io_running) {
        io_shard *s = calloc((size_t)n, sizeof *s);
        if (!s) zell_panic("out of memory (io)");
        for (int32_t i = 0; i < n; i++) {
            zell__plat_mutex_init(&s[i].mu);
            s[i].index = i;
            s[i].sleep_until = INT64_MAX;
            s[i].poller = zell__plat_poller_new();
            if (!s[i].poller) zell_panic("cannot create the I/O poller");
            s[i].edge = zell__plat_poller_edge(s[i].poller);
        }
        io_shards = s;
        io_nshards = n;
        zell__io_stop_fn = zell__io_stop;  /* shutdown stops the shards */
        for (int32_t i = 0; i < n; i++)
            if (!zell__plat_thread_start(&s[i].thread, io_main, &s[i])) zell_panic("cannot create an I/O poller thread");
        __atomic_store_n(&io_running, 1, __ATOMIC_RELEASE);
    }
    zell__plat_mutex_unlock(&io_start_mu);
}

/* ---- waits ---- */

/* Cancellation hook: completes the wait of task `t` (data = fd << 1 | write) with IO_CANCELLED. */
static void io_cancelled(zell_task *t, uintptr_t data) {
    int64_t fd = (int64_t)(data >> 1);
    int write = (int)(data & 1);
    io_fd *e = entry(fd, false);
    if (!e) return;
    io_shard *sh = shard_lock(e, false);
    if (!sh) return;
    io_wait **pp = write ? &e->wr : &e->rd;
    io_wait *found = NULL;
    for (; *pp; pp = &(*pp)->next) {
        if ((*pp)->task == t) {
            found = *pp;
            *pp = found->next;
            break;
        }
    }
    if (found) heap_remove(sh, found);
    zell__plat_mutex_unlock(&sh->mu);
    if (found) {
        int64_t code = IO_CANCELLED;
        zell_task_complete(found->task, &code);
        zell_release(found->task);
        free(found);
    }
}

static zell_task *io_wait_for(int64_t fd, bool write, int64_t timeout_ms) {
    if (fd < 0) zell_panic("zell_io_wait: invalid file descriptor");
    zell_task *t = zell_task_new_plain(8, NULL);
    io_start();
    io_fd *e = entry(fd, true);
    io_wait *w = malloc(sizeof *w);
    if (!w) zell_panic("out of memory (io)");
    zell_retain(t);
    w->task = t;
    w->fd = fd;
    w->write = write;
    w->heap = -1;
    w->deadline = timeout_ms >= 0 ? zell__plat_mono_ns() + timeout_ms * 1000000 + 1 : 0;
    io_shard *sh = shard_lock(e, true);
    io_wait **list = write ? &e->wr : &e->rd;
    int32_t dir = write ? ZELL_PLAT_WRITE : ZELL_PLAT_READ;
    int64_t immediate = -1;
    if (sh->edge && ((e->ready | e->sticky) & dir)) {
        /* reported while nobody waited (consumed), or EOF/error: no poller call */
        e->ready &= ~dir;
        immediate = IO_READY;
    } else {
        w->next = *list;
        *list = w;
    }
    int32_t want = (e->rd ? ZELL_PLAT_READ : 0) | (e->wr ? ZELL_PLAT_WRITE : 0);
    if (immediate >= 0 || (sh->edge && e->registered)) {
        /* nothing to arm: an edge-triggered fd is armed once */
    } else if ((want & ~e->armed) != 0) {
        int rc = zell__plat_poller_arm(sh->poller, fd, want, token_of(e, fd), e->registered != 0);
        if (rc == 0) {
            e->registered = 1;
            e->armed = want;
        } else {
            /* EPERM: cannot be polled (a regular file), so always ready;
             * anything else (EBADF: closed) fails the wait. */
            immediate = rc == ZELL_PLAT_ENOTPOLLABLE ? IO_READY : IO_CLOSED;
            *list = w->next;
        }
    }
    bool wake = false;
    if (immediate < 0 && w->deadline) {
        heap_push(sh, w);
        wake = w->deadline < sh->sleep_until;
        if (wake) sh->sleep_until = w->deadline;
    }
    zell__plat_poller *p = sh->poller;
    zell__plat_mutex_unlock(&sh->mu);
    if (wake) zell__plat_poller_wake(p);
    if (immediate >= 0) {
        zell_task_complete(t, &immediate);
        zell_release(t);
        free(w);
        return t;
    }
    zell__cancel_hook_data(io_cancelled, t, ((uintptr_t)fd << 1) | (write ? 1u : 0u));
    return t;
}

zell_task *zell_io_wait_readable(int64_t fd, int64_t timeout_ms) { return io_wait_for(fd, false, timeout_ms); }
zell_task *zell_io_wait_writable(int64_t fd, int64_t timeout_ms) { return io_wait_for(fd, true, timeout_ms); }

void zell_io_register(int64_t fd) {
    if (fd < 0) return;
    io_start();
    io_shard *sh = shard_lock(entry(fd, true), true);
    zell__plat_mutex_unlock(&sh->mu);
}

void zell_io_forget(int64_t fd) {
    io_fd *e = entry(fd, false);
    if (!e || !__atomic_load_n(&io_running, __ATOMIC_ACQUIRE)) return;
    io_shard *sh = shard_lock(e, false);
    if (!sh) return;
    done_buf d;
    done_init(&d);
    take_all(sh, &e->rd, IO_CLOSED, &d);
    take_all(sh, &e->wr, IO_CLOSED, &d);
    if (e->registered) zell__plat_poller_remove(sh->poller, fd);
    e->registered = 0;
    e->armed = 0;
    e->ready = 0;
    e->sticky = 0;
    e->gen++;
    __atomic_store_n(&e->shard, -1, __ATOMIC_RELEASE);
    __atomic_sub_fetch(&sh->nfds, 1, __ATOMIC_RELAXED);
    zell__plat_mutex_unlock(&sh->mu);
    done_finish(&d);
    done_free(&d);
}

int64_t zell__io_shard_of(int64_t fd) {
    io_fd *e = entry(fd, false);
    return e ? __atomic_load_n(&e->shard, __ATOMIC_ACQUIRE) : -1;
}

int64_t zell__io_shards(void) { return __atomic_load_n(&io_running, __ATOMIC_ACQUIRE) ? io_nshards : 0; }

void zell__io_stop(void) {
    zell__net_stop();
    zell__plat_mutex_lock(&io_start_mu);
    if (!io_running) { zell__plat_mutex_unlock(&io_start_mu); return; }
    for (int32_t i = 0; i < io_nshards; i++) {
        zell__plat_mutex_lock(&io_shards[i].mu);
        io_shards[i].stopping = 1;
        zell__plat_mutex_unlock(&io_shards[i].mu);
        zell__plat_poller_wake(io_shards[i].poller);
    }
    for (int32_t i = 0; i < io_nshards; i++) zell__plat_thread_join(io_shards[i].thread);
    /* Waits still pending at shutdown are dropped without completing (the
     * scheduler has stopped: nobody would run their continuations). */
    for (int64_t c = 0; c < IO_MAX_CHUNKS; c++) {
        io_fd *chunk = io_chunks[c];
        if (!chunk) continue;
        for (int i = 0; i < IO_CHUNK; i++) {
            for (int dir = 0; dir < 2; dir++) {
                io_wait *w = dir ? chunk[i].wr : chunk[i].rd;
                while (w) {
                    io_wait *n = w->next;
                    zell_release(w->task);
                    free(w);
                    w = n;
                }
            }
        }
        free(chunk);
        io_chunks[c] = NULL;
    }
    for (int32_t i = 0; i < io_nshards; i++) {
        zell__plat_poller_free(io_shards[i].poller);
        free(io_shards[i].heap);
        zell__plat_mutex_destroy(&io_shards[i].mu);
    }
    free(io_shards);
    io_shards = NULL;
    io_nshards = 0;
    __atomic_store_n(&io_running, 0, __ATOMIC_RELEASE);
    zell__plat_mutex_unlock(&io_start_mu);
}
