/* debug.c - the C side of the in-process debug agent (docs/DEBUGGER.md
 * stage 2, runtime/ABI.md section 22).
 *
 * Compiled code built with `--debug-agent` calls, guarded by one load of
 * `zell_dbg_on`:
 *
 *   zell_dbg_enter(&F, &func, locals, entry_stmt)   at function entry
 *   zell_dbg_stmt(&F, stmt)                         at every statement
 *   zell_dbg_leave(&F)                              at function exit
 *   zell_dbg_suspend(&F) / zell_dbg_resume(&F)     around an `await`
 *
 * `F` (zell_dbg_frame) lives in the function's frame (the coroutine frame
 * for async functions) and links into a per-thread chain: the shadow stack
 * the agent walks for stack traces and variables. Everything else is
 * decided in Zell (debug): this file only filters the hot path,
 * keeps per-task pause/step state and blocks a paused task's thread on a
 * condition variable until the agent resumes it. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <stdlib.h>
#include <string.h>

/* ---- tables (ABI.md 22.1) ----------------------------------------------- */

typedef struct zell_dbg_stmt_rec {
    int32_t file, line, col, func, scope, kind;
} zell_dbg_stmt_rec;

typedef struct zell_dbg_func {
    zell_string *name;
    zell_string *short_name;
    int32_t file, line;
    int32_t first_stmt, last_stmt;
    int32_t nlocals, flags;
    const void *locals;
    const void *addr;
} zell_dbg_func;

typedef struct zell_dbg_tables {
    int64_t version;
    int64_t nstmts;
    const zell_dbg_stmt_rec *stmts;
    int64_t nfuncs;
    const zell_dbg_func *funcs;
    int64_t nfiles;
    zell_string *const *files;
    uint8_t *bp;
} zell_dbg_tables;

typedef struct zell_dbg_frame {
    struct zell_dbg_frame *prev;
    const zell_dbg_func *func;
    void *const *locals;
    int32_t stmt;
    int32_t active;   /* 0 unlinked, 1 linked, 2 suspended at an await */
} zell_dbg_frame;

_Static_assert(sizeof(zell_dbg_stmt_rec) == 24, "zell_dbg_stmt layout (ABI.md 22.1)");
_Static_assert(sizeof(zell_dbg_func) == 56, "zell_dbg_func layout (ABI.md 22.1)");
_Static_assert(sizeof(zell_dbg_tables) == 64, "zell_dbg_tables layout (ABI.md 22.1)");
_Static_assert(sizeof(zell_dbg_frame) == 32, "zell_dbg_frame layout (ABI.md 22.2)");

enum { KIND_STMT = 0, KIND_ENTRY = 1, KIND_EXIT = 2 };
enum { FUNC_ASYNC = 1 };

/* Reasons passed to the agent's hook (ABI.md 22.3). */
enum {
    R_BP = 1, R_STEP = 2, R_PAUSE = 4, R_PANIC = 8, R_ERROR = 16, R_ENTRY = 32
};
enum { STEP_NONE = 0, STEP_NEXT = 1, STEP_IN = 2, STEP_OUT = 3 };

/* ---- global state ------------------------------------------------------- */

/* Read by compiled code before every hook (hidden symbol: one load). */
ZELL_HIDDEN uint8_t zell_dbg_on;
/* panic.c / error.c call the agent through these (set by zell_dbg_boot),
 * so programs without the agent do not link it. */
void (*zell__dbg_panic_fn)(const char *msg, int64_t len);
void (*zell__dbg_error_fn)(const zell_error *err);

static const zell_dbg_tables *g_tables;
static _Thread_local zell_dbg_frame *t_top;
static _Thread_local int t_no_hooks;          /* agent threads, nested callbacks */
static _Thread_local const char *t_panic_msg;
static _Thread_local int64_t t_panic_len;
static _Thread_local const zell_error *t_error;

typedef int64_t (*hook_fn)(void *env, int64_t stmt, int64_t frame, int64_t reason, int64_t task);
typedef void (*exit_fn)(void *env, int64_t code);
static hook_fn g_hook;
static void *g_hook_env;
static exit_fn g_exit;
static void *g_exit_env;

static int g_freeze;      /* atomic: every task pauses at its next hook */
static int g_nsteps;      /* atomic: tasks with an active step */
static int g_stop_next;   /* atomic: the next hook anywhere stops (stopOnEntry) */
static int g_on_error;    /* atomic: report error creation (exception breakpoint) */

/* Per-task record. Records are never freed (tasks that ended are marked
 * dead), so pointers to them stay valid. Guarded by g_mu. */
typedef struct rec {
    int64_t id;
    int alive;
    int finished;
    int32_t func;                 /* function of the task's first frame */
    zell_dbg_frame *paused;      /* frame while blocked in a hook */
    zell_dbg_frame *suspended;   /* frame while waiting at an await */
    int32_t stmt, reason;
    int resume;                   /* the agent let it go */
    int exempt;                   /* runs while the others stay frozen */
    int deciding;                 /* inside the agent's hook (not yet blocked) */
    int step;
    int stopped;                  /* reported a stop to the client at least once */
    int carried;                  /* taken over by a coroutine at its first suspension */
    zell_dbg_frame *step_frame, *step_parent;
    int32_t step_depth, step_line, step_stmt;
    zell__plat_cond cv;
    struct rec *next_step;        /* list of records with an active step */
} rec;

static zell__plat_mutex g_mu = ZELL_PLAT_MUTEX_INIT;
#define CHUNK 4096
#define NCHUNKS 4096
static rec **g_chunks[NCHUNKS];
static int64_t g_nrecs;           /* ids 1..g_nrecs are taken; 1 = main */
/* The record of code running with no current task (the main thread):
 * 1 until a coroutine stepped in from there takes it over at its first
 * suspension (see zell_dbg_suspend); 0 then, until such code is observed
 * again. Written under g_mu. */
static int64_t g_main_id = 1;
static rec *g_steps;              /* records with step != STEP_NONE */

/* The agent's own locks (debug.rtLock(n)): 0 = agent state, 1 = output. */
static zell__plat_mutex g_user_mu[2] = { ZELL_PLAT_MUTEX_INIT, ZELL_PLAT_MUTEX_INIT };
static zell__plat_mutex g_ready_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_cond g_ready_cv = ZELL_PLAT_COND_INIT;
static int g_ready;

/* ---- task records --------------------------------------------------------- */

static rec *rec_at(int64_t id) {
    if (id <= 0 || id > __atomic_load_n(&g_nrecs, __ATOMIC_ACQUIRE)) return NULL;
    rec **c = __atomic_load_n(&g_chunks[(id - 1) / CHUNK], __ATOMIC_ACQUIRE);
    return c ? c[(id - 1) % CHUNK] : NULL;
}

/* g_mu held. */
static rec *rec_new(void) {
    int64_t id = g_nrecs + 1;
    if ((id - 1) / CHUNK >= NCHUNKS) return NULL;
    rec **c = g_chunks[(id - 1) / CHUNK];
    if (!c) {
        c = calloc(CHUNK, sizeof *c);
        if (!c) return NULL;
        __atomic_store_n(&g_chunks[(id - 1) / CHUNK], c, __ATOMIC_RELEASE);
    }
    rec *r = calloc(1, sizeof *r);
    if (!r) return NULL;
    r->id = id;
    r->alive = 1;
    r->func = -1;
    zell__plat_cond_init(&r->cv);
    c[(id - 1) % CHUNK] = r;
    __atomic_store_n(&g_nrecs, id, __ATOMIC_RELEASE);
    return r;
}

/* The record of the code running on this thread, if it has one. */
static rec *peek_rec(void) {
    zell_task *t = zell__task_cur();
    return rec_at(t ? zell__task_dbg_id(t) : __atomic_load_n(&g_main_id, __ATOMIC_ACQUIRE));
}

/* The record of the code running on this thread (registering its task). */
static rec *cur_rec(int32_t func) {
    zell_task *t = zell__task_cur();
    if (!t) {
        rec *m = rec_at(__atomic_load_n(&g_main_id, __ATOMIC_ACQUIRE));
        if (m) return m;
        zell__plat_mutex_lock(&g_mu);
        if (g_nrecs == 0) {
            m = rec_new();  /* id 1 */
            if (m) m->func = func;
        } else if (!g_main_id) {
            m = rec_new();
            if (m) {
                m->func = func;
                __atomic_store_n(&g_main_id, m->id, __ATOMIC_RELEASE);
            }
        } else {
            m = rec_at(g_main_id);
        }
        zell__plat_mutex_unlock(&g_mu);
        return m;
    }
    int32_t id = zell__task_dbg_id(t);
    if (id) {
        rec *r = rec_at(id);
        if (r && r->func < 0 && func >= 0) r->func = func;
        return r;
    }
    zell__plat_mutex_lock(&g_mu);
    if (g_nrecs == 0) rec_new();  /* id 1 stays the main thread */
    rec *r = rec_new();
    if (r) {
        r->func = func;
        zell__task_set_dbg_id(t, (int32_t)r->id);
    }
    zell__plat_mutex_unlock(&g_mu);
    return r;
}

/* task.c: a task with a debug id is freed. */
void zell__dbg_task_drop(int32_t id) {
    rec *r = rec_at(id);
    if (!r) return;
    zell__plat_mutex_lock(&g_mu);
    r->alive = 0;
    r->suspended = NULL;
    zell__plat_mutex_unlock(&g_mu);
}

/* ---- the frame chain -------------------------------------------------------- */

static int32_t depth_of(const zell_dbg_frame *f) {
    int32_t d = 0;
    for (; f; f = f->prev) d++;
    return d;
}

static int chain_has(const zell_dbg_frame *f, const zell_dbg_frame *x) {
    if (!x) return 0;
    for (; f; f = f->prev)
        if (f == x) return 1;
    return 0;
}

static const zell_dbg_stmt_rec *stmt_at(int32_t id) {
    if (!g_tables || id < 0 || id >= g_tables->nstmts) return NULL;
    return &g_tables->stmts[id];
}

/* ---- pausing ---------------------------------------------------------------- */

/* The task is marked paused before the agent's hook runs (the hook sends
 * the `stopped` event, and the client may ask for the stack or resume the
 * task at once); after the hook it blocks until resumed, or unmarks itself
 * if the hook let it go on. */
static void step_clear(rec *r);

/* The record of the task inside the agent's hook on this thread. */
static _Thread_local rec *t_deciding;

static void pause_begin(rec *r, zell_dbg_frame *f, int32_t stmt, int32_t reason) {
    zell__plat_mutex_lock(&g_mu);
    r->paused = f;
    r->stmt = stmt;
    r->reason = reason;
    r->resume = 0;
    r->deciding = 1;
    t_deciding = r;
    zell__plat_mutex_unlock(&g_mu);
}

/* act (the hook's result): 1 stopped (reported to the client), 2 frozen
 * while another task is stopped (silent: ends with the freeze). */
static void pause_end(rec *r, int64_t act) {
    zell__plat_mutex_lock(&g_mu);
    r->deciding = 0;
    t_deciding = NULL;
    if (act & 1) {
        while (!r->resume) zell__plat_cond_wait(&r->cv, &g_mu);
    } else if (act & 2) {
        while (!r->resume && __atomic_load_n(&g_freeze, __ATOMIC_ACQUIRE)) zell__plat_cond_wait(&r->cv, &g_mu);
    } else if (r->resume) {
        /* Resumed while the hook decided not to stop: drop its step. */
        step_clear(r);
    }
    r->paused = NULL;
    r->resume = 0;
    zell__plat_mutex_unlock(&g_mu);
}

/* g_mu held: removes r from the step list. */
static void step_clear(rec *r) {
    if (r->step == STEP_NONE) return;
    r->step = STEP_NONE;
    for (rec **p = &g_steps; *p; p = &(*p)->next_step) {
        if (*p == r) {
            *p = r->next_step;
            break;
        }
    }
    r->next_step = NULL;
    __atomic_sub_fetch(&g_nsteps, 1, __ATOMIC_RELEASE);
}

/* Whether some active step ends at this hook (g_mu taken here). */
static int step_done(rec *cur, zell_dbg_frame *f, int32_t id) {
    const zell_dbg_stmt_rec *st = stmt_at(id);
    if (!st || st->kind != KIND_STMT) return 0;
    int done = 0;
    zell__plat_mutex_lock(&g_mu);
    for (rec *s = g_steps; s && !done; s = s->next_step) {
        int same = s == cur;
        int on_chain = chain_has(f, s->step_frame);
        switch (s->step) {
        case STEP_NEXT:
        case STEP_IN:
            if (s->step_frame && f == s->step_frame) {
                done = st->line != s->step_line || id <= s->step_stmt;
            } else if (s->step_parent && f == s->step_parent && !(s->step_frame && s->step_frame->active == 2)) {
                /* Back in the caller - unless the frame only suspended at an
                 * `await` (its ramp returned): then the step goes on when the
                 * coroutine resumes, in its own task. */
                done = 1;
            } else if (s->step == STEP_IN && on_chain) {
                done = 1;  /* a callee, also a coroutine with its own task */
            } else if (same && !on_chain && depth_of(f) < s->step_depth && !(s->step_frame && s->step_frame->active == 2)) {
                done = 1;  /* returned (also through an await: the record passed to the awaiter) */
            }
            break;
        case STEP_OUT:
            if (s->step_parent && f == s->step_parent && !(s->step_frame && s->step_frame->active == 2)) {
                done = 1;
            } else if (same && f != s->step_frame && !on_chain && depth_of(f) < s->step_depth &&
                       !(s->step_frame && s->step_frame->active == 2)) {
                done = 1;
            }
            break;
        default:
            break;
        }
        if (done) step_clear(s);
    }
    zell__plat_mutex_unlock(&g_mu);
    return done;
}

static int32_t func_index(const zell_dbg_frame *f) {
    return f && f->func && g_tables ? (int32_t)(f->func - g_tables->funcs) : -1;
}

static void dispatch(zell_dbg_frame *f, int32_t id, int32_t reason) {
    rec *r = cur_rec(func_index(f));
    if (!r) return;
    if ((reason & R_PAUSE) && __atomic_load_n(&r->exempt, __ATOMIC_ACQUIRE)) reason &= ~R_PAUSE;
    if (!reason) return;
    hook_fn h = __atomic_load_n(&g_hook, __ATOMIC_ACQUIRE);
    if (!h) return;
    t_no_hooks = 1;
    pause_begin(r, f, id, reason);
    int64_t act = h(g_hook_env, id, (int64_t)(intptr_t)f, reason, r->id);
    if (act & 1) __atomic_store_n(&r->stopped, 1, __ATOMIC_RELEASE);
    pause_end(r, act);
    t_no_hooks = 0;
}

/* ---- hooks called by compiled code ----------------------------------------- */

static void link_lazily(zell_dbg_frame *f, int32_t id) {
    const zell_dbg_stmt_rec *st = stmt_at(id);
    f->func = st && st->func >= 0 && st->func < g_tables->nfuncs ? &g_tables->funcs[st->func] : NULL;
    f->locals = NULL;
    f->prev = t_top;
    f->active = 1;
    t_top = f;
}

void zell_dbg_stmt(zell_dbg_frame *f, int32_t id) {
    if (!g_tables || t_no_hooks) return;
    f->stmt = id;
    if (!f->active) link_lazily(f, id);
    int32_t reason = 0;
    if (id >= 0 && id < g_tables->nstmts && g_tables->bp[id]) reason |= R_BP;
    if (__atomic_load_n(&g_freeze, __ATOMIC_ACQUIRE)) reason |= R_PAUSE;
    if (__atomic_load_n(&g_stop_next, __ATOMIC_ACQUIRE) && __atomic_exchange_n(&g_stop_next, 0, __ATOMIC_ACQ_REL)) reason |= R_ENTRY;
    if (__atomic_load_n(&g_nsteps, __ATOMIC_ACQUIRE)) {
        /* No record is made here: code is observed (gets an id) only when
         * it stops, so a caller that merely runs on after a coroutine took
         * its record over does not show up as a new thread. */
        if (step_done(peek_rec(), f, id)) reason |= R_STEP;
    }
    if (reason) dispatch(f, id, reason);
}

void zell_dbg_enter(zell_dbg_frame *f, const zell_dbg_func *func, void *const *locals, int32_t entry) {
    /* The frame record is not zeroed by compiled code: this is its init. */
    f->active = 0;
    f->stmt = entry;
    if (!g_tables || t_no_hooks) return;
    f->func = func;
    f->locals = locals;
    f->prev = t_top;
    f->active = 1;
    t_top = f;
    zell_dbg_stmt(f, entry);
}

void zell_dbg_leave(zell_dbg_frame *f) {
    if (!f->active) return;
    if (t_top == f) {
        t_top = f->prev;
    } else {
        /* Unbalanced (a frame linked while hooks were off): unlink it wherever it is. */
        for (zell_dbg_frame **p = &t_top; *p; p = &(*p)->prev) {
            if (*p == f) {
                *p = f->prev;
                break;
            }
        }
    }
    f->active = 0;
    if (__atomic_load_n(&g_nsteps, __ATOMIC_ACQUIRE)) {
        /* The frame's memory is about to be reused: steps must not match it. */
        zell__plat_mutex_lock(&g_mu);
        for (rec *s = g_steps; s; s = s->next_step) {
            if (s->step_frame == f) s->step_frame = NULL;
            if (s->step_parent == f) s->step_parent = f->prev;
        }
        zell__plat_mutex_unlock(&g_mu);
    }
    if (f->func && (f->func->flags & FUNC_ASYNC) && !f->prev && g_tables) {
        zell_task *t = zell__task_cur();
        int32_t id = t ? zell__task_dbg_id(t) : 0;
        rec *r = id ? rec_at(id) : NULL;
        if (r) {
            /* A record a coroutine took over from the code it was stepped
             * in goes on with the awaiter (the task whose continuation is
             * registered on this one): the logical thread continues
             * there. The awaiter's own record, made when it suspended,
             * is retired unless it was ever reported stopped. */
            zell_task *w = r->carried ? zell__task_awaiter(t) : NULL;
            zell__plat_mutex_lock(&g_mu);
            int moved = 0;
            if (w) {
                int32_t wid = zell__task_dbg_id(w);
                rec *wr = wid ? rec_at(wid) : NULL;
                if (!wr || (!wr->stopped && !wr->paused && wr->step == STEP_NONE)) {
                    r->suspended = wr ? wr->suspended : NULL;
                    r->func = wr ? wr->func : -1;
                    if (wr) {
                        wr->alive = 0;
                        wr->suspended = NULL;
                    }
                    zell__task_set_dbg_id(t, 0);
                    zell__task_set_dbg_id(w, (int32_t)r->id);
                    moved = 1;
                }
            }
            if (!moved) r->finished = 1;
            zell__plat_mutex_unlock(&g_mu);
        }
    }
}

void zell_dbg_suspend(zell_dbg_frame *f) {
    if (f->active != 1) return;
    if (t_top == f) t_top = f->prev;
    f->active = 2;
    if (!g_tables) return;
    zell_task *t = zell__task_cur();
    if (t && !zell__task_dbg_id(t)) {
        /* The coroutine's first suspension (its task was made just now,
         * ABI.md 10.3). If a step is under way in this very frame, the
         * code was observed under its host's record (the caller's task or
         * the main thread): the record moves with the coroutine, so the
         * logical thread keeps its id across the await, and the host,
         * which merely runs on after the ramp returned, gets a new record
         * if it is observed again. */
        zell__plat_mutex_lock(&g_mu);
        rec *s = g_steps;
        while (s && s->step_frame != f) s = s->next_step;
        int took = 0;
        if (s && s->alive) {
            if (s->id == g_main_id) {
                __atomic_store_n(&g_main_id, 0, __ATOMIC_RELEASE);
                took = 1;
            } else {
                for (zell_task *p = zell__task_parent(t); p && !took; p = zell__task_parent(p)) {
                    if (zell__task_dbg_id(p) == s->id) {
                        zell__task_set_dbg_id(p, 0);
                        took = 1;
                    }
                }
            }
        }
        if (took) {
            zell__task_set_dbg_id(t, (int32_t)s->id);
            s->carried = 1;
            s->func = func_index(f);
            s->suspended = f;
        }
        zell__plat_mutex_unlock(&g_mu);
        if (took) return;
    }
    rec *r = cur_rec(func_index(f));
    if (r) {
        zell__plat_mutex_lock(&g_mu);
        r->suspended = f;
        /* Until its first suspension a coroutine runs as part of its
         * caller's task (ABI.md 10.3), so a step begun in this frame may be
         * recorded on the caller's record: it moves to the coroutine's own
         * record here and goes on when the frame resumes there, not in the
         * caller, which merely continues after the ramp returned. */
        for (rec *s = g_steps; s; s = s->next_step) {
            if (s == r || s->step_frame != f) continue;
            int step = s->step;
            zell_dbg_frame *parent = s->step_parent;
            int32_t depth = s->step_depth, line = s->step_line, stmt = s->step_stmt;
            step_clear(s);
            step_clear(r);
            r->step = step;
            r->step_frame = f;
            r->step_parent = parent;
            r->step_depth = depth;
            r->step_line = line;
            r->step_stmt = stmt;
            r->next_step = g_steps;
            g_steps = r;
            __atomic_add_fetch(&g_nsteps, 1, __ATOMIC_RELEASE);
            break;
        }
        zell__plat_mutex_unlock(&g_mu);
    }
}

void zell_dbg_resume(zell_dbg_frame *f) {
    if (f->active != 2) return;
    f->prev = t_top;
    f->active = 1;
    t_top = f;
    if (!g_tables) return;
    rec *r = peek_rec();
    if (r) {
        zell__plat_mutex_lock(&g_mu);
        if (r->suspended == f) r->suspended = NULL;
        zell__plat_mutex_unlock(&g_mu);
    }
}

/* panic.c: before the message is printed. */
void zell__dbg_panic(const char *msg, int64_t len) {
    if (!g_tables || t_no_hooks || !__atomic_load_n(&g_hook, __ATOMIC_ACQUIRE)) return;
    t_panic_msg = msg;
    t_panic_len = len;
    zell_dbg_frame *f = t_top;
    dispatch(f, f ? f->stmt : -1, R_PANIC);
    t_panic_msg = NULL;
}

/* error.c: an error got its creation site. */
void zell__dbg_error(const zell_error *err) {
    if (!g_tables || t_no_hooks || !__atomic_load_n(&g_on_error, __ATOMIC_ACQUIRE)) return;
    t_error = err;
    zell_dbg_frame *f = t_top;
    dispatch(f, f ? f->stmt : -1, R_ERROR);
    t_error = NULL;
}

int zell__dbg_error_wanted(void) { return __atomic_load_n(&g_on_error, __ATOMIC_RELAXED); }

/* ---- program entry and exit (called by the compiled C main) ------------------ */

/* `entry` is debug.AgentMain wrapped as a C function (NULL if the
 * program was built without the agent): it listens, starts the agent
 * thread and, with `--wait`, returns once the client is ready. */
void zell_dbg_boot(const void *tables, void (*entry)(void)) {
    const char *env = getenv("ZELL_DEBUG");
    if (!env || !*env || !entry || !tables) return;
    g_tables = tables;
    zell__dbg_panic_fn = zell__dbg_panic;
    zell__dbg_error_fn = zell__dbg_error;
    __atomic_store_n(&zell_dbg_on, 1, __ATOMIC_RELEASE);
    t_no_hooks = 1;
    entry();
    t_no_hooks = 0;
}

void zell_dbg_exit(int32_t code) {
    exit_fn e = __atomic_exchange_n(&g_exit, NULL, __ATOMIC_ACQ_REL);
    if (!e) return;
    t_no_hooks = 1;
    e(g_exit_env, code);
}

/* ======================================================================
 * debug internal externs (rt*): the agent's view of the state above.
 * ====================================================================== */

int64_t zell_debug_rt_tables(void) { return (int64_t)(intptr_t)g_tables; }

void zell_debug_rt_set_hook(void *fn, void *env) {
    zell_retain(env);
    g_hook_env = env;
    __atomic_store_n(&g_hook, (hook_fn)fn, __ATOMIC_RELEASE);
}

void zell_debug_rt_set_exit_hook(void *fn, void *env) {
    zell_retain(env);
    g_exit_env = env;
    __atomic_store_n(&g_exit, (exit_fn)fn, __ATOMIC_RELEASE);
}

void zell_debug_rt_set_bp(int64_t stmt, bool on) {
    if (!g_tables || stmt < 0 || stmt >= g_tables->nstmts) return;
    __atomic_store_n(&g_tables->bp[stmt], on ? 1 : 0, __ATOMIC_RELEASE);
}

void zell_debug_rt_on_error(bool on) { __atomic_store_n(&g_on_error, on ? 1 : 0, __ATOMIC_RELEASE); }

void zell_debug_rt_stop_next(void) { __atomic_store_n(&g_stop_next, 1, __ATOMIC_RELEASE); }

/* Freezing makes every task (except exempt ones) pause at its next hook;
 * a new freeze clears the exemptions. */
void zell_debug_rt_freeze(bool on) {
    zell__plat_mutex_lock(&g_mu);
    /* Called by the hook that is about to report a stop: from now on a
     * `continue` from the client releases this task too. */
    if (on && t_deciding) t_deciding->deciding = 0;
    if (on) {
        for (int64_t i = 1; i <= g_nrecs; i++) {
            rec *r = rec_at(i);
            if (r) r->exempt = 0;
        }
    }
    __atomic_store_n(&g_freeze, on ? 1 : 0, __ATOMIC_RELEASE);
    zell__plat_mutex_unlock(&g_mu);
}

/* Lets task `id` go: mode 0 continue, 1 next, 2 step in, 3 step out. Unless
 * `single`, the freeze ends and every paused task goes too. */
bool zell_debug_rt_resume(int64_t id, int64_t mode, bool single) {
    zell__plat_mutex_lock(&g_mu);
    rec *r = rec_at(id);
    bool ok = r && r->paused;
    if (ok) {
        step_clear(r);
        if (mode >= STEP_NEXT && mode <= STEP_OUT) {
            r->step = (int)mode;
            r->step_frame = r->paused;
            r->step_parent = r->paused->prev;
            r->step_depth = depth_of(r->paused);
            const zell_dbg_stmt_rec *st = stmt_at(r->stmt);
            r->step_line = st ? st->line : -1;
            r->step_stmt = r->stmt;
            r->next_step = g_steps;
            g_steps = r;
            __atomic_add_fetch(&g_nsteps, 1, __ATOMIC_RELEASE);
        }
        if (single) {
            __atomic_store_n(&r->exempt, 1, __ATOMIC_RELEASE);
        }
        r->resume = 1;
        zell__plat_cond_signal(&r->cv);
    }
    if (!single) {
        __atomic_store_n(&g_freeze, 0, __ATOMIC_RELEASE);
        for (int64_t i = 1; i <= g_nrecs; i++) {
            rec *o = rec_at(i);
            /* A task still deciding in its hook may be about to report a
             * stop of its own: it is not released by someone else's resume. */
            if (o && o->paused && !o->resume && !o->deciding) {
                o->resume = 1;
                zell__plat_cond_signal(&o->cv);
            }
        }
    }
    zell__plat_mutex_unlock(&g_mu);
    return ok;
}

/* Ends every step and lets everything run (disconnect). */
void zell_debug_rt_release_all(void) {
    zell__plat_mutex_lock(&g_mu);
    while (g_steps) step_clear(g_steps);
    __atomic_store_n(&g_freeze, 0, __ATOMIC_RELEASE);
    __atomic_store_n(&g_stop_next, 0, __ATOMIC_RELEASE);
    __atomic_store_n(&g_on_error, 0, __ATOMIC_RELEASE);
    for (int64_t i = 1; i <= g_nrecs; i++) {
        rec *o = rec_at(i);
        if (!o) continue;
        o->exempt = 0;
        if (o->paused && !o->resume) {
            o->resume = 1;
            zell__plat_cond_signal(&o->cv);
        }
    }
    zell__plat_mutex_unlock(&g_mu);
    if (g_tables) memset(g_tables->bp, 0, (size_t)g_tables->nstmts);
}

/* Live tasks seen by a hook, by id (the main thread is 1). */
void zell_debug_rt_tasks(zell_slice *out) {
    zell_slice_new(out, 8, 0, 0);
    zell__plat_mutex_lock(&g_mu);
    for (int64_t i = 1; i <= g_nrecs; i++) {
        rec *r = rec_at(i);
        if (r && r->alive && !r->finished) zell_slice_append(out, 8, NULL, &r->id);
    }
    zell__plat_mutex_unlock(&g_mu);
}

/* 0 running, 1 paused in a hook, 2 waiting at an await, -1 gone. */
int64_t zell_debug_rt_task_state(int64_t id) {
    zell__plat_mutex_lock(&g_mu);
    rec *r = rec_at(id);
    int64_t s = !r || !r->alive || r->finished ? -1 : r->paused ? 1 : r->suspended ? 2 : 0;
    zell__plat_mutex_unlock(&g_mu);
    return s;
}

/* The top frame of a paused (or awaiting) task, 0 if none. */
int64_t zell_debug_rt_task_frame(int64_t id) {
    zell__plat_mutex_lock(&g_mu);
    rec *r = rec_at(id);
    zell_dbg_frame *f = r ? (r->paused ? r->paused : r->suspended) : NULL;
    zell__plat_mutex_unlock(&g_mu);
    return (int64_t)(intptr_t)f;
}

int64_t zell_debug_rt_task_func(int64_t id) {
    rec *r = rec_at(id);
    return r ? r->func : -1;
}

int64_t zell_debug_rt_task_reason(int64_t id) {
    rec *r = rec_at(id);
    return r && r->paused ? r->reason : 0;
}

void zell_debug_rt_lock(int64_t n) { zell__plat_mutex_lock(&g_user_mu[n & 1]); }
void zell_debug_rt_unlock(int64_t n) { zell__plat_mutex_unlock(&g_user_mu[n & 1]); }

/* `--wait`: the program starts once the client sent configurationDone. */
void zell_debug_rt_wait_ready(void) {
    zell__plat_mutex_lock(&g_ready_mu);
    while (!g_ready) zell__plat_cond_wait(&g_ready_cv, &g_ready_mu);
    zell__plat_mutex_unlock(&g_ready_mu);
}

void zell_debug_rt_signal_ready(void) {
    zell__plat_mutex_lock(&g_ready_mu);
    g_ready = 1;
    zell__plat_cond_broadcast(&g_ready_cv);
    zell__plat_mutex_unlock(&g_ready_mu);
}

/* Hooks never fire on this thread (the agent's own threads). */
void zell_debug_rt_ignore_hooks(void) { t_no_hooks = 1; }

zell_string *zell_debug_rt_panic_text(void) {
    return t_panic_msg ? zell_string_new(t_panic_msg, t_panic_len) : zell_string_new("", 0);
}

int64_t zell_debug_rt_cur_error(void) { return (int64_t)(intptr_t)t_error; }

/* The full message of the error at `addr` (may run a user Message()). */
zell_string *zell_debug_rt_error_text(int64_t addr) {
    return zell_error_message((const zell_error *)(intptr_t)addr);
}

/* Name of the runtime type info at `addr` ("" for NULL). */
zell_string *zell_debug_rt_type_name(int64_t addr) {
    const zell_type_info *ti = (const zell_type_info *)(intptr_t)addr;
    return zell_string_from_cstr(ti && ti->name ? ti->name : "");
}

/* Blocking await of a Task<int> (the agent thread is no scheduler worker). */
int64_t zell_debug_rt_await_int(zell_task *t) {
    int64_t *slot = zell_task_await(t);
    return slot ? *slot : 0;
}

/* Task<T> values: whether done, and the result slot address. */
bool zell_debug_rt_task_done(int64_t addr) {
    return addr && zell_task_is_done((const zell_task *)(intptr_t)addr);
}

int64_t zell_debug_rt_task_slot(int64_t addr) {
    if (!addr || !zell_task_is_done((const zell_task *)(intptr_t)addr)) return 0;
    return (int64_t)(intptr_t)zell_task_result((const zell_task *)(intptr_t)addr);
}

/* Name of the code at `addr` (a closure's function), "" if unknown. */
zell_string *zell_debug_rt_sym_name(int64_t addr) {
    char buf[256];
    uintptr_t start = 0;
    if (!addr || !zell__plat_symbolize((uintptr_t)addr, buf, sizeof buf, &start)) buf[0] = 0;
    return zell_string_from_cstr(buf);
}

ZELL_NORETURN void zell_debug_rt_exit(int64_t code) {
    zell__io_flush_all();
    _Exit((int)code);
}

/* ---- breakpoint paths ------------------------------------------------------ */

/* malloc'd comparison form of a path: a `\\?\` prefix dropped and, with
 * `fold` (Windows rules), '\\' turned into '/' and ASCII letters lowered. */
static char *path_norm(const char *p, int fold) {
    if (!p) p = "";
    if (fold && strncmp(p, "\\\\?\\", 4) == 0) p += 4;
    size_t n = strlen(p);
    char *s = malloc(n + 1);
    if (!s) return NULL;
    for (size_t i = 0; i <= n; i++) {
        char c = p[i];
        if (fold && c == '\\') c = '/';
        if (fold && c >= 'A' && c <= 'Z') c = (char)(c - 'A' + 'a');
        s[i] = c;
    }
    return s;
}

static const char *path_base(const char *p) {
    const char *b = strrchr(p, '/');
    return b ? b + 1 : p;
}

/* How well breakpoint path `path` names the table file `file`: 3 the same
 * file, 2 `path` is a relative tail of `file` (on a '/' boundary), 1 the
 * same base name, 0 no match. `fold`: Windows rules (case and separators). */
int zell__dbg_path_match(const char *file, const char *path, int fold) {
    char *f = path_norm(file, fold), *p = path_norm(path, fold);
    int m = 0;
    if (f && p && *p) {
        size_t fl = strlen(f), pl = strlen(p);
        if (strcmp(f, p) == 0) {
            m = 3;
        } else if (pl < fl && strcmp(f + fl - pl, p) == 0 && f[fl - pl - 1] == '/') {
            m = 2;
        } else if (strcmp(path_base(f), path_base(p)) == 0) {
            m = 1;
        }
    }
    free(f);
    free(p);
    return m;
}

/* The canonical spelling of a breakpoint path (symlinks resolved), or the
 * path itself when it does not name an existing file. */
zell_string *zell_debug_rt_path_canon(zell_string *path) {
    char *c = zell__plat_path_canon(zell_string_cstr(path));
    if (!c) {
        zell_retain(path);
        return path;
    }
    zell_string *s = zell_string_from_cstr(c);
    free(c);
    return s;
}

int64_t zell_debug_rt_path_match(zell_string *file, zell_string *path) {
    return zell__dbg_path_match(zell_string_cstr(file), zell_string_cstr(path), zell__plat_path_fold());
}

/* Milliseconds since an arbitrary point (monotonic). */
int64_t zell_debug_rt_now_ms(void) { return zell__plat_mono_ns() / 1000000; }

/* ======================================================================
 * Memory views used by debug's variable formatter (ABI.md 22.4).
 * ====================================================================== */

/* The `string` stored at addr (a zell_string *), retained ("" for NULL). */
zell_string *zell_debug_rt_string(int64_t addr) {
    zell_string *s = addr ? *(zell_string *const *)(intptr_t)addr : NULL;
    if (!s) return zell_string_new("", 0);
    zell_retain(s);
    return s;
}

int64_t zell_debug_rt_map_len(int64_t m) { return zell_map_len((const zell_map *)(intptr_t)m); }

/* Addresses of the key and value slots of entries start.. of map m (at
 * most n entries), as [key0, val0, key1, val1, ...]. */
void zell_debug_rt_map_entries(int64_t m, int64_t start, int64_t n, zell_slice *out) {
    zell_slice_new(out, 8, 0, 0);
    if (!m || n <= 0) return;
    zell_map_iter it;
    zell_map_iter_init(&it, (zell_map *)(intptr_t)m);
    void *k, *v;
    int64_t i = 0;
    while (zell_map_iter_next(&it, &k, &v)) {
        if (i++ < start) continue;
        int64_t ka = (int64_t)(intptr_t)k, va = (int64_t)(intptr_t)v;
        zell_slice_append(out, 8, NULL, &ka);
        zell_slice_append(out, 8, NULL, &va);
        if (i - start >= n) break;
    }
}

/* setVariable (ABI.md 22.4): a float of `size` bytes (4 or 8) at addr. */
void zell_debug_rt_write_float(int64_t addr, int64_t size, double v) {
    if (!addr) return;
    if (size == 4) {
        float f = (float)v;
        memcpy((void *)(intptr_t)addr, &f, sizeof f);
    } else {
        memcpy((void *)(intptr_t)addr, &v, sizeof v);
    }
}

/* setVariable: the `string` slot at addr takes `s` (borrowed: retained
 * for the slot) and releases its old value. */
void zell_debug_rt_set_string(int64_t addr, zell_string *s) {
    if (!addr) return;
    zell_string **slot = (zell_string **)(intptr_t)addr;
    zell_retain(s);
    zell_string *old = *slot;
    *slot = s;
    zell_release(old);
}

/* Type name of a typed error ("" for a plain error or NULL). */
zell_string *zell_debug_rt_error_type(int64_t addr) {
    const zell_error *e = (const zell_error *)(intptr_t)addr;
    while (e && !e->user_type && e->inner) e = e->inner;
    return zell_string_from_cstr(e && e->user_type && e->user_type->name ? e->user_type->name : "");
}

/* ======================================================================
 * Blocking sockets of the agent thread (the agent is no scheduler task:
 * it must keep answering while every worker is blocked in a paused task).
 * ====================================================================== */

static zell__plat_mutex g_poll_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_poller *g_poller;

/* Waits until fd is ready for `interest` (or timeout_ms passes, -1 = forever). */
static bool wait_fd(int64_t fd, int32_t interest, int64_t timeout_ms) {
    zell__plat_mutex_lock(&g_poll_mu);
    if (!g_poller) g_poller = zell__plat_poller_new();
    zell__plat_poller *p = g_poller;
    zell__plat_mutex_unlock(&g_poll_mu);
    if (!p) return false;
    if (zell__plat_poller_arm(p, fd, interest, (uint64_t)fd, false) != 0) return false;
    zell__plat_event ev[4];
    int64_t deadline = timeout_ms < 0 ? -1 : zell__plat_mono_ns() / 1000000 + timeout_ms;
    for (;;) {
        int64_t left = deadline < 0 ? -1 : deadline - zell__plat_mono_ns() / 1000000;
        if (deadline >= 0 && left < 0) left = 0;
        int n = zell__plat_poller_wait(p, ev, 4, left);
        for (int i = 0; i < n; i++)
            if (ev[i].token == (uint64_t)fd) return true;
        if (deadline >= 0 && zell__plat_mono_ns() / 1000000 >= deadline) {
            zell__plat_poller_arm(p, fd, 0, (uint64_t)fd, true);
            return false;
        }
    }
}

/* A listening TCP socket on host:port (port 0: any free port). */
int64_t zell_debug_rt_listen(const zell_string *host, int64_t port, zell_error **err) {
    unsigned char ip[16];
    int len = zell__plat_parse_ip(zell_string_len(host) ? zell_string_cstr(host) : "127.0.0.1", ip);
    zell__plat_addr a;
    if (!len || !zell__plat_addr_ip(&a, ip, len, port, 0)) {
        *err = zell_error_new_cstr("zell-debug: bad listen address (want a numeric IP)");
        return -1;
    }
    int64_t fd = zell__plat_sock_open(len == 4 ? ZELL_PLAT_AF_INET : ZELL_PLAT_AF_INET6, ZELL_PLAT_STREAM);
    if (fd < 0) {
        *err = zell_error_new_cstr("zell-debug: cannot create a socket");
        return -1;
    }
    zell__plat_sock_setopt(fd, ZELL_PLAT_OPT_REUSEADDR, 1);
    if (zell__plat_sock_bind(fd, &a) < 0 || zell__plat_sock_listen(fd, 4) < 0) {
        zell__plat_sock_close(fd);
        *err = zell_error_new_cstr("zell-debug: cannot listen (address in use?)");
        return -1;
    }
    return fd;
}

int64_t zell_debug_rt_sock_port(int64_t fd) {
    zell__plat_addr a;
    if (!zell__plat_sock_name(fd, false, &a)) return 0;
    unsigned char ip[16];
    int len;
    int64_t port = 0;
    uint32_t scope;
    zell__plat_addr_parts(&a, ip, &len, &port, &scope);
    return port;
}

/* Blocks until a client connects; its descriptor, or -1. */
int64_t zell_debug_rt_accept(int64_t fd) {
    for (;;) {
        int64_t c = zell__plat_sock_accept(fd, NULL);
        if (c >= 0) {
            zell__plat_sock_setopt(c, ZELL_PLAT_OPT_NODELAY, 1);
            return c;
        }
        if (c != -ZELL_PLAT_EAGAIN) return -1;
        if (!wait_fd(fd, ZELL_PLAT_READ, -1)) return -1;
    }
}

/* Blocks until data arrives: up to max bytes, empty at EOF or on error. */
void zell_debug_rt_recv(int64_t fd, int64_t max, zell_slice *out) {
    if (max <= 0) max = 65536;
    zell_slice_new(out, 1, max, max);
    for (;;) {
        int64_t n = zell__plat_sock_recv(fd, out->data, max);
        if (n >= 0) {
            out->len = n;
            return;
        }
        if (n != -ZELL_PLAT_EAGAIN || !wait_fd(fd, ZELL_PLAT_READ, -1)) {
            out->len = 0;
            return;
        }
    }
}

/* Sends all of s; false if the peer went away. */
bool zell_debug_rt_send(int64_t fd, const zell_string *s) {
    const char *p = zell_string_data(s);
    int64_t left = zell_string_len(s);
    while (left > 0) {
        int64_t n = zell__plat_sock_send(fd, p, left);
        if (n > 0) {
            p += n;
            left -= n;
        } else if (n != -ZELL_PLAT_EAGAIN || !wait_fd(fd, ZELL_PLAT_WRITE, 10000)) {
            return false;
        }
    }
    return true;
}

void zell_debug_rt_close(int64_t fd) {
    zell__plat_mutex_lock(&g_poll_mu);
    if (g_poller) zell__plat_poller_remove(g_poller, fd);
    zell__plat_mutex_unlock(&g_poll_mu);
    zell__plat_sock_close(fd);
}

/* Unbuffered write to stderr (the port announcement). */
void zell_debug_rt_eprint(const zell_string *s) {
    int64_t n = zell_string_len(s);
    const char *p = zell_string_data(s);
    zell__plat_write_all(2, p, n);
}
