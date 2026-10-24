/* error.c - boxed error values with wrapping, typed payloads and return
 * traces (ABI.md section 7). */
#include "rt_internal.h"

#include <errno.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static void frames_release(zell_error *e) {
    int64_t n = e->nframes;
    for (int64_t i = 0; i < n; i++) {
        zell_error_frame *f = i < ZELL_ERROR_INLINE_FRAMES ? &e->frames[i] : &e->overflow[i - ZELL_ERROR_INLINE_FRAMES];
        zell_release(f->ctx);
    }
    zell_free(e->overflow);
}

static void error_drop(void *obj) {
    zell_error *e = obj;
    zell_release(e->message);
    zell_release(e->inner);
    zell_release(e->payload);
    zell_release(e->origin);
    frames_release(e);
}

const zell_type_info zell_type_error = { "error", (int64_t)sizeof(zell_error), error_drop };

static ZELL_STRING_STORAGE(4) eof_msg = ZELL_STRING_INIT(eof_msg, "EOF");

static zell_error eof_error = {
    .hdr = { ZELL_RC_IMMORTAL, &zell_type_error }, .message = (zell_string *)&eof_msg,
};
zell_error *const zell_err_eof = &eof_error;

static zell_error *error_alloc(void) {
    return zell_object_new(&zell_type_error, (int64_t)sizeof(zell_error));
}

zell_error *zell_error_new(const zell_string *msg) {
    zell_error *e = error_alloc();
    zell_retain((void *)msg);
    e->message = (zell_string *)msg;
    return e;
}

zell_error *zell_error_new_cstr(const char *msg) {
    zell_error *e = error_alloc();
    e->message = zell_string_from_cstr(msg);
    return e;
}

zell_error *zell_error_sentinel(const zell_string *msg) {
    /* The sentinel and its message (an immortal copy) are never freed and
     * do not count as live objects. */
    zell_string *m = (msg && msg->hdr.refcount != ZELL_RC_IMMORTAL)
                          ? zell_string_literal(zell_string_data(msg), zell_string_len(msg))
                          : (zell_string *)msg;
    zell_error *e = zell_error_new(m);
    zell__object_immortalize(&e->hdr);
    return e;
}

zell_error *zell_error_new_typed(const zell_type_info *type, void *payload,
                                   zell_message_fn message_fn) {
    zell_error *e = error_alloc();
    e->user_type = type;
    zell_retain(payload);
    e->payload = payload;
    e->message_fn = message_fn;
    return e;
}

zell_error *zell_error_wrap(zell_error *inner, const zell_string *ctx) {
    if (!inner) return NULL;
    zell_error *e = error_alloc();
    zell_retain((void *)ctx);
    e->message = (zell_string *)ctx;
    zell_retain(inner);
    e->inner = inner;
    return e;
}

zell_error *zell_error_unwrap(const zell_error *err) {
    if (!err) return NULL;
    zell_retain(err->inner);
    return err->inner;
}

/* Own text of a single link (not including inner). Owned result. */
static zell_string *own_text(const zell_error *e) {
    if (e->message_fn) return e->message_fn(e->payload);
    zell_retain(e->message);
    return e->message;
}

zell_string *zell_error_message(const zell_error *err) {
    if (!err) return zell_string_new("<nil>", 5);
    zell_string *acc = own_text(err);
    static ZELL_STRING_STORAGE(3) sep = ZELL_STRING_INIT(sep, ": ");
    /* A typed (user) link's Message() is its whole text: its inner (the
     * user Unwrap() cause) is not appended. */
    for (const zell_error *p = err, *e = err->inner; e && !p->message_fn; p = e, e = e->inner) {
        zell_string *t = own_text(e);
        zell_string *a = zell_string_concat(acc, (zell_string *)&sep);
        zell_string *b = zell_string_concat(a, t);
        zell_release(a);
        zell_release(t);
        zell_release(acc);
        acc = b;
    }
    return acc;
}

/* The error a copy-on-write copy was made from (identity for errors.Is). */
static const zell_error *root_of(const zell_error *e) {
    return e->origin ? e->origin : e;
}

bool zell_error_is(const zell_error *err, const zell_error *target) {
    if (!target) return err == NULL;
    const zell_error *t = root_of(target);
    for (const zell_error *e = err; e; e = e->inner)
        if (root_of(e) == t) return true;
    return false;
}

void *zell_error_as(const zell_error *err, const zell_type_info *type) {
    for (const zell_error *e = err; e; e = e->inner)
        if (e->user_type && e->user_type == type) return e->payload;
    return NULL;
}

/* ---------------------------------------------------------------------------
 * Error traces (ABI.md 7.4)
 * ------------------------------------------------------------------------- */

static zell_error_frame *frame_at(const zell_error *e, int64_t i) {
    return i < ZELL_ERROR_INLINE_FRAMES ? (zell_error_frame *)&e->frames[i]
                                         : &e->overflow[i - ZELL_ERROR_INLINE_FRAMES];
}

static void frame_push(zell_error *e, const zell_error_site *site, const zell_string *ctx) {
    int64_t i = e->nframes;
    if (i >= ZELL_ERROR_INLINE_FRAMES) {
        int64_t k = i - ZELL_ERROR_INLINE_FRAMES;
        if (k >= e->overflow_cap) {
            int64_t cap = e->overflow_cap ? e->overflow_cap * 2 : 8;
            e->overflow = zell_realloc(e->overflow, cap * (int64_t)sizeof(zell_error_frame));
            e->overflow_cap = cap;
        }
    }
    zell_error_frame *f = frame_at(e, i);
    f->site = site;
    if (ctx) zell_retain((void *)ctx);
    f->ctx = (zell_string *)ctx;
    e->nframes = i + 1;
}

/* Copies the trace of src into dst (which has none). */
static void trace_copy(zell_error *dst, const zell_error *src) {
    dst->created = src->created;
    for (int64_t i = 0; i < src->nframes; i++) {
        const zell_error_frame *f = frame_at(src, i);
        frame_push(dst, f->site, f->ctx);
    }
}

/* Copy-on-write: an error that is referenced elsewhere (or immortal) is
 * copied before its trace changes. Consumes err, returns an owned error
 * that is uniquely referenced. */
static zell_error *err_unshare(zell_error *err) {
    if (zell_refcount(err) == 1) return err;
    zell_error *c = zell_object_new(&zell_type_error, (int64_t)sizeof(zell_error));
    zell_retain(err->message);
    c->message = err->message;
    zell_retain(err->inner);
    c->inner = err->inner;
    c->user_type = err->user_type;
    zell_retain(err->payload);
    c->payload = err->payload;
    c->message_fn = err->message_fn;
    zell_error *root = (zell_error *)root_of(err);
    zell_retain(root);
    c->origin = root;
    trace_copy(c, err);
    zell_release(err);
    return c;
}

/* ---- pending traces of shared errors ----
 * A shared error (a sentinel, or any error with other holders) is not
 * copied while it propagates: its creation site and `returned via` frames
 * go to a thread-local record keyed by the error pointer. The record is
 * consumed where the propagation ends (zell_error_caught: `try ... catch`,
 * the result of an async function, the error of `main`), either attached to
 * a copy of the error or dropped when the handler never looks at the trace.
 * Records nest (a defer or a handler may propagate another error while one
 * is in flight); a full stack or a long trace falls back to the copy. */
#define PEND_DEPTH 8
#define PEND_FRAMES 24

typedef struct {
    const zell_error *err;
    const zell_error_site *created;
    int64_t n;
    const zell_error_site *sites[PEND_FRAMES];   /* `?` frames, never with ctx */
} pend_rec;

/* One thread-local block (initial-exec: the runtime is linked into the
 * executable, so the address is a constant offset, no __tls_get_addr). */
static _Thread_local __attribute__((tls_model("initial-exec"))) struct {
    int n;
    pend_rec recs[PEND_DEPTH];
} t_pend_tls;
#define t_npend t_pend_tls.n
#define t_pend t_pend_tls.recs

/* Is `err` uniquely referenced? Fast for a fresh error (refcount word 1)
 * and for a sentinel (immortal word); other words ask zell_refcount. */
static inline bool unique(const zell_error *err) {
    int64_t rc = __atomic_load_n(&err->hdr.refcount, __ATOMIC_RELAXED);
    if (rc == 1) return true;
    if (rc == ZELL_RC_IMMORTAL) return false;
    return zell_refcount(err) == 1;
}

static pend_rec *pend_find(const zell_error *err) {
    for (int i = t_npend - 1; i >= 0; i--)
        if (t_pend[i].err == err) return &t_pend[i];
    return NULL;
}

static void pend_remove(pend_rec *r) {
    int i = (int)(r - t_pend);
    if (i < t_npend - 1) memmove(r, r + 1, (size_t)(t_npend - 1 - i) * sizeof *r);
    t_npend--;
}

/* Does the trace of `err` live in a pending record (no trace in the object)? */
static inline bool trace_pending(const zell_error *err) {
    return !err->created && err->nframes == 0 && t_npend > 0;
}

/* Moves the pending record `r` into `err` (uniquely owned, no trace of its
 * own) and drops the record. */
static void pend_adopt(zell_error *err, pend_rec *r) {
    err->created = r->created;
    for (int64_t i = 0; i < r->n; i++) frame_push(err, r->sites[i], NULL);
    pend_remove(r);
}

/* Attaches the pending record `r` of `err` to the error itself: to `err`
 * when it is uniquely owned, else to the copy-on-write copy the trace would
 * have had. Consumes err, returns the owned error. */
static zell_error *materialize(zell_error *err, pend_rec *r) {
    if (!unique(err)) {
        zell_retain(err);               /* makes unshare copy; consumes our ref below */
        zell_error *c = err_unshare(err);
        zell_release(err);
        err = c;
    }
    pend_adopt(err, r);
    return err;
}

zell_error *zell_error_created(zell_error *err, const zell_error_site *site) {
    if (!err || err->created || !site) return err;
    if (unique(err)) {
        pend_rec *r = trace_pending(err) ? pend_find(err) : NULL;
        if (r) pend_adopt(err, r);      /* the record already holds the site */
        else err->created = site;
    } else if (err->nframes == 0 && t_npend < PEND_DEPTH) {
        pend_rec *r = &t_pend[t_npend++];
        r->err = err;
        r->created = site;
        r->n = 0;
    } else {
        err = err_unshare(err);
        err->created = site;
    }
    if (__atomic_load_n(&zell_dbg_on, __ATOMIC_RELAXED) && zell__dbg_error_fn) zell__dbg_error_fn(err);
    return err;
}

static inline zell_error *returned(zell_error *err, const zell_error_site *site, const zell_string *ctx) {
    pend_rec *r = trace_pending(err) ? pend_find(err) : NULL;
    if (ctx) {
        zell_error *w = zell_error_wrap(err, ctx);
        if (r) pend_adopt(w, r);
        else trace_copy(w, err);
        zell_release(err);
        err = w;
    } else if (!unique(err)) {
        if (r && r->n < PEND_FRAMES) {
            r->sites[r->n++] = site;
            return err;
        }
        err = r ? materialize(err, r) : err_unshare(err);
    } else if (r) {
        pend_adopt(err, r);
    }
    frame_push(err, site, ctx);
    return err;
}

zell_error *zell_error_returned(zell_error *err, const zell_error_site *site, const zell_string *ctx) {
    return err ? returned(err, site, ctx) : NULL;
}

zell_error *zell_error_caught(zell_error *err, const zell_error_site *site, int64_t keep) {
    if (!err) return NULL;
    if (site) err = returned(err, site, NULL);
    pend_rec *r = trace_pending(err) ? pend_find(err) : NULL;
    if (!r) return err;
    if (keep) return materialize(err, r);
    pend_remove(r);
    return err;
}

void zell_error_discarded(zell_error *err) {
    if (!err) return;
    if (trace_pending(err)) {
        pend_rec *r = pend_find(err);
        if (r) pend_remove(r);
    }
    zell_release(err);
}

/* The error whose trace to print: `err`, or a temporary copy holding its
 * pending record (the record stays). Owned when *tmp is set. */
static const zell_error *trace_view(const zell_error *err, zell_error **tmp) {
    *tmp = NULL;
    if (!err || !trace_pending(err)) return err;
    pend_rec *r = pend_find(err);
    if (!r) return err;
    zell_error *c = error_alloc();
    c->created = r->created;
    for (int64_t i = 0; i < r->n; i++) frame_push(c, r->sites[i], NULL);
    *tmp = c;
    return c;
}

/* ---- trace text ---- */
typedef struct { char *p; size_t len, cap; } tbuf;

static void tb_put(tbuf *b, const char *s, size_t n) {
    if (b->len + n + 1 > b->cap) {
        size_t cap = b->cap ? b->cap * 2 : 256;
        while (cap < b->len + n + 1) cap *= 2;
        b->p = zell_realloc(b->p, (int64_t)cap);
        b->cap = cap;
    }
    memcpy(b->p + b->len, s, n);
    b->len += n;
    b->p[b->len] = 0;
}

static void tb_pad(tbuf *b, size_t n) {
    while (n--) tb_put(b, " ", 1);
}

/* platform/platform.h (not included: its names clash with this file's). */
char *zell__plat_getcwd(int *err);

/* The file of a site as printed: relative to the working directory
 * `cwd` when it is under it (absolute paths of dependencies, builds run
 * elsewhere), else as compiled. */
static const char *site_file(const zell_error_site *s, const char *cwd) {
    const char *f = s->file ? s->file : "?";
    size_t n = cwd ? strlen(cwd) : 0;
    if (n > 1 && strncmp(f, cwd, n) == 0 && (f[n] == '/' || f[n] == '\\') && f[n + 1]) return f + n + 1;
    return f;
}

static int loc_len(const zell_error_site *s, const char *cwd) {
    char buf[32];
    int n = snprintf(buf, sizeof buf, ":%lld", (long long)s->line);
    return (int)strlen(site_file(s, cwd)) + n;
}

zell_string *zell_error_trace(const zell_error *err) {
    if (!err) return zell_string_new("", 0);
    zell_error *tmp;
    err = trace_view(err, &tmp);
    int64_t n = err->nframes + (err->created ? 1 : 0);
    int cwd_err = 0;
    char *cwd = n > 0 ? zell__plat_getcwd(&cwd_err) : NULL;
    size_t maxloc = 0, maxfn = 0;
    for (int64_t i = 0; i < n; i++) {
        const zell_error_site *s = err->created ? (i == 0 ? err->created : frame_at(err, i - 1)->site)
                                                 : frame_at(err, i)->site;
        size_t l = (size_t)loc_len(s, cwd), f = strlen(s->func ? s->func : "");
        if (l > maxloc) maxloc = l;
        if (f > maxfn) maxfn = f;
    }
    tbuf b = { 0 };
    for (int64_t i = 0; i < n; i++) {
        bool created = err->created && i == 0;
        const zell_error_frame *fr = created ? NULL : frame_at(err, err->created ? i - 1 : i);
        const zell_error_site *s = created ? err->created : fr->site;
        if (i > 0) tb_put(&b, "\n", 1);
        tb_put(&b, created ? "  created at   " : "  returned via ", 15);
        char loc[4096];
        int ll = snprintf(loc, sizeof loc, "%s:%lld", site_file(s, cwd), (long long)s->line);
        if (ll < 0) ll = 0;
        if ((size_t)ll >= sizeof loc) ll = (int)sizeof loc - 1;
        tb_put(&b, loc, (size_t)ll);
        tb_pad(&b, maxloc - (size_t)ll + 2);
        const char *fn = s->func ? s->func : "";
        tb_put(&b, fn, strlen(fn));
        if (fr && fr->ctx) {
            tb_pad(&b, maxfn - strlen(fn) + 2);
            tb_put(&b, "?(\"", 3);
            tb_put(&b, zell_string_data(fr->ctx), (size_t)zell_string_len(fr->ctx));
            tb_put(&b, "\")", 2);
        }
        while (b.len > 0 && b.p[b.len - 1] == ' ') b.p[--b.len] = 0;
    }
    zell_string *r = zell_string_new(b.p ? b.p : "", (int64_t)b.len);
    zell_free(b.p);
    free(cwd);
    zell_release(tmp);
    return r;
}

/* errors.Frame {File string, Line int, Func string, Context string}. */
typedef struct { zell_string *file; int64_t line; zell_string *func; zell_string *ctx; } frame_rec;

static void frame_rec_retain(void *slot) {
    frame_rec *f = slot;
    zell_retain(f->file);
    zell_retain(f->func);
    zell_retain(f->ctx);
}

static void frame_rec_release(void *slot) {
    frame_rec *f = slot;
    zell_release(f->file);
    zell_release(f->func);
    zell_release(f->ctx);
}

const zell_value_ops zell_ops_error_frame = { frame_rec_retain, frame_rec_release, NULL };

void zell_error_frames(const zell_error *err, zell_slice *out) {
    zell_error *tmp;
    err = trace_view(err, &tmp);
    int64_t n = err ? err->nframes + (err->created ? 1 : 0) : 0;
    zell_slice_new_ops(out, (int64_t)sizeof(frame_rec), n, n, &zell_ops_error_frame);
    for (int64_t i = 0; i < n; i++) {
        bool created = err->created && i == 0;
        const zell_error_frame *fr = created ? NULL : frame_at(err, err->created ? i - 1 : i);
        const zell_error_site *s = created ? err->created : fr->site;
        frame_rec *r = (frame_rec *)out->data + i;
        r->file = zell_string_from_cstr(s->file ? s->file : "");
        r->line = s->line;
        r->func = zell_string_from_cstr(s->func ? s->func : "");
        if (fr && fr->ctx) {
            zell_retain(fr->ctx);
            r->ctx = fr->ctx;
        }
    }
    zell_release(tmp);
}

void zell_error_report(const zell_error *err) {
    zell_string *m = zell_error_message(err);
    zell_string *t = zell_error_trace(err);
    fprintf(stderr, "error: %s\n", zell_string_cstr(m));
    if (zell_string_len(t) > 0) fprintf(stderr, "%s\n", zell_string_cstr(t));
    fflush(stderr);
    zell_release(t);
    zell_release(m);
}

/* fs.ErrNotFound: an immortal sentinel; ENOENT errors wrap it. */
static ZELL_STRING_STORAGE(26) not_found_msg = ZELL_STRING_INIT(not_found_msg, "No such file or directory");

static zell_error not_found_error = {
    .hdr = { ZELL_RC_IMMORTAL, &zell_type_error }, .message = (zell_string *)&not_found_msg,
};

zell_error *zell_fs_err_not_found(void) {
    return &not_found_error;
}

zell_error *zell_error_errno(const char *op, const char *path, int e) {
    char buf[1024];
    if (e == ENOENT) {
        snprintf(buf, sizeof buf, "%s %.900s", op, path ? path : "");
        zell_string *ctx = zell_string_from_cstr(buf);
        zell_error *w = zell_error_wrap(&not_found_error, ctx);
        zell_release(ctx);
        return w;
    }
    snprintf(buf, sizeof buf, "%s %.900s: %s", op, path ? path : "", strerror(e));
    return zell_error_new_cstr(buf);
}
