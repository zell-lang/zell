/* string.c - immutable UTF-8 strings. NULL is the empty string. */
#include "rt_internal.h"

#include <ctype.h>
#include <errno.h>
#include <inttypes.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

const zell_type_info zell_type_string = { "string", 0, NULL };

/* ---- views (ABI.md 4.1) ----
 * A view shares the bytes of another string. Standalone views are ordinary
 * objects {hdr, len, data, owner} whose drop releases the owner; views made
 * in bulk (Split, Fields, regex matches) live in a view block, and their
 * refcount word forwards to the block (ZELL_RC_VIEW | block). The owner is
 * always the object that holds the bytes inline (never another view), or
 * NULL for the bytes of an immortal string. */

static void sview_drop(void *o) { zell_release(((zell__sview *)o)->owner); }
const zell_type_info zell_type_string_view = { "string", 0, sview_drop };

static void vblock_drop(void *o) { zell_release(((zell__vblock *)o)->owner); }
static const zell_type_info vblock_type = { "string views", 0, vblock_drop };

/* Views of a string of len < VIEW_MIN bytes are copies: a small copy costs
 * less than the owner's refcount traffic and does not keep a big parent
 * alive. */
#define VIEW_MIN 64

/* The bytes owner of s for a new view: sets *owner (NULL for static bytes)
 * and returns 1, or returns 0 when s cannot be viewed (arena strings, whose
 * bytes die with the arena). */
static int view_owner(const zell_string *s, zell_object **owner) {
    int64_t rc = __atomic_load_n(&s->hdr.refcount, __ATOMIC_RELAXED);
    if (rc == ZELL_RC_IMMORTAL) {
        *owner = NULL;
    } else if (rc == ZELL_RC_ARENA) {
        return 0;
    } else if (zell_rc_is_view(rc)) {
        *owner = ((zell__vblock *)(uintptr_t)(rc & ~ZELL_RC_VIEW))->owner;
    } else if (s->hdr.type == &zell_type_string_view) {
        *owner = ((const zell__sview *)s)->owner;
    } else {
        *owner = (zell_object *)&s->hdr;
    }
    return 1;
}

zell_string *zell__string_sub(const zell_string *s, int64_t lo, int64_t hi) {
    int64_t n = hi - lo;
    if (n <= 0) return zell__string_alloc_uninit(0);
    zell_object *owner;
    if (n < VIEW_MIN || !view_owner(s, &owner)) return zell_string_new(s->data + lo, n);
    zell__sview *v = (zell__sview *)zell__object_new_uninit(&zell_type_string_view, (int64_t)sizeof(zell__sview));
    v->s.len = n;
    v->s.data = s->data + lo;
    v->owner = owner;
    zell_retain(owner);
    return &v->s;
}

zell__vblock *zell__vblock_new(const zell_string *parent, int64_t cap) {
    zell_object *owner;
    if (!parent || cap <= 0 || !view_owner(parent, &owner)) return NULL;
    zell__vblock *b = (zell__vblock *)zell__object_new_uninit(
        &vblock_type, (int64_t)sizeof(zell__vblock) + cap * (int64_t)sizeof(zell_string));
    b->owner = owner;
    zell_retain(owner);
    b->n = 0;
    b->cap = cap;
    return b;
}

zell_string *zell__vblock_add(zell__vblock *b, const char *p, int64_t n) {
    if (n <= 0) return NULL;
    if (!b || b->n == b->cap) return zell_string_new(p, n);
    zell_string *v = &b->v[b->n++];
    v->hdr.refcount = ZELL_RC_VIEW | (int64_t)(uintptr_t)b;
    v->hdr.type = &zell_type_string;
    v->len = n;
    v->data = (char *)p;
    b->hdr.refcount++;   /* still private to the caller: no atomics needed */
    return v;
}

void zell__vblock_done(zell__vblock *b) { zell_release(b); }

/* ---- C strings ---- */

#define CSTR_RING 16
static _Thread_local struct { char *buf[CSTR_RING]; size_t cap[CSTR_RING]; unsigned next; } t_cstr;

const char *zell_string_cstr(const zell_string *s) {
    if (!s) return "";
    if (s->data[s->len] == '\0') return s->data;
    unsigned i = t_cstr.next++ % CSTR_RING;
    size_t need = (size_t)s->len + 1;
    if (t_cstr.cap[i] < need) {
        size_t cap = need < 64 ? 64 : need;
        char *nb = realloc(t_cstr.buf[i], cap);
        if (!nb) zell__panicf("out of memory (requested %zu bytes)", cap);
        t_cstr.buf[i] = nb;
        t_cstr.cap[i] = cap;
    }
    memcpy(t_cstr.buf[i], s->data, (size_t)s->len);
    t_cstr.buf[i][s->len] = '\0';
    return t_cstr.buf[i];
}

/* ---- allocation ---- */

static inline int64_t slen(const zell_string *s) { return s ? s->len : 0; }
static inline const char *sdata(const zell_string *s) { return s ? s->data : ""; }

zell_string *zell__string_alloc(int64_t len) {
    if (len < 0) zell__panicf("negative string length %lld", (long long)len);
    zell_string *s = zell_object_new(&zell_type_string,
                                       (int64_t)sizeof(zell_string) + len + 1);
    s->len = len;
    s->data = (char *)(s + 1);
    s->data[len] = ZELL__STR_END;
    return s;
}

zell_string *zell__string_alloc_uninit(int64_t len) {
    if (len < 0) zell__panicf("negative string length %lld", (long long)len);
    zell_string *s = (zell_string *)zell__object_new_uninit(
        &zell_type_string, (int64_t)sizeof(zell_string) + len + 1);
    s->len = len;
    s->data = (char *)(s + 1);
    s->data[len] = ZELL__STR_END;
    return s;
}

zell_string *zell_string_new(const char *bytes, int64_t len) {
    zell_string *s = zell__string_alloc_uninit(len);
    if (len > 0) memcpy(s->data, bytes, (size_t)len);
    return s;
}

zell_string *zell_string_from_cstr(const char *cstr) {
    if (!cstr) cstr = "";
    return zell_string_new(cstr, (int64_t)strlen(cstr));
}

zell_string *zell_string_literal(const char *bytes, int64_t len) {
    zell_string *s = zell_string_new(bytes, len);
    s->data[len] = '\0';
    zell__object_immortalize(&s->hdr);
    return s;
}

const char *zell_string_data(const zell_string *s) { return sdata(s); }
int64_t zell_string_len(const zell_string *s) { return slen(s); }

/* Decode one rune at p[0..n). Returns rune, sets *w (>=1). Invalid -> U+FFFD, w=1. */
static int32_t utf8_decode(const unsigned char *p, int64_t n, int64_t *w) {
    unsigned c = p[0];
    if (c < 0x80) { *w = 1; return (int32_t)c; }
    int need; uint32_t r, min;
    if ((c & 0xE0) == 0xC0) { need = 1; r = c & 0x1F; min = 0x80; }
    else if ((c & 0xF0) == 0xE0) { need = 2; r = c & 0x0F; min = 0x800; }
    else if ((c & 0xF8) == 0xF0) { need = 3; r = c & 0x07; min = 0x10000; }
    else { *w = 1; return 0xFFFD; }
    if (n < need + 1) { *w = 1; return 0xFFFD; }
    for (int i = 1; i <= need; i++) {
        if ((p[i] & 0xC0) != 0x80) { *w = 1; return 0xFFFD; }
        r = (r << 6) | (p[i] & 0x3F);
    }
    if (r < min || r > 0x10FFFF || (r >= 0xD800 && r <= 0xDFFF)) { *w = 1; return 0xFFFD; }
    *w = need + 1;
    return (int32_t)r;
}

int zell__utf8_encode(char *buf, int32_t r) {
    uint32_t c = (uint32_t)r;
    if (c > 0x10FFFF || (c >= 0xD800 && c <= 0xDFFF)) c = 0xFFFD;
    if (c < 0x80) { buf[0] = (char)c; return 1; }
    if (c < 0x800) {
        buf[0] = (char)(0xC0 | (c >> 6)); buf[1] = (char)(0x80 | (c & 0x3F)); return 2;
    }
    if (c < 0x10000) {
        buf[0] = (char)(0xE0 | (c >> 12)); buf[1] = (char)(0x80 | ((c >> 6) & 0x3F));
        buf[2] = (char)(0x80 | (c & 0x3F)); return 3;
    }
    buf[0] = (char)(0xF0 | (c >> 18)); buf[1] = (char)(0x80 | ((c >> 12) & 0x3F));
    buf[2] = (char)(0x80 | ((c >> 6) & 0x3F)); buf[3] = (char)(0x80 | (c & 0x3F));
    return 4;
}

int64_t zell_string_rune_count(const zell_string *s) {
    int64_t n = slen(s), count = 0;
    const unsigned char *p = (const unsigned char *)sdata(s);
    for (int64_t i = 0; i < n;) {
        int64_t w;
        utf8_decode(p + i, n - i, &w);
        i += w;
        count++;
    }
    return count;
}

int32_t zell_string_decode_rune(const zell_string *s, int64_t i, int64_t *width) {
    int64_t n = slen(s);
    if (i < 0 || i >= n) zell_panic_bounds(i, n);
    int64_t w;
    int32_t r = utf8_decode((const unsigned char *)sdata(s) + i, n - i, &w);
    if (width) *width = w;
    return r;
}

zell_string *zell_string_concat(const zell_string *a, const zell_string *b) {
    int64_t la = slen(a), lb = slen(b);
    zell_string *s = zell__string_alloc_uninit(la + lb);
    if (la) memcpy(s->data, a->data, (size_t)la);
    if (lb) memcpy(s->data + la, b->data, (size_t)lb);
    return s;
}

zell_string *zell_string_append(zell_string *a, const zell_string *b) {
    int64_t la = slen(a), lb = slen(b);
    if (lb == 0) return a;
    if (!a || !zell__rc_unique(&a->hdr) || a->data != (char *)(a + 1)) {
        /* Shared, immortal, a view or empty: copy. */
        zell_string *s = zell_string_concat(a, b);
        zell_release(a);
        return s;
    }
    /* `a` is uniquely referenced by the caller: extend it in place, with
     * geometric growth so that a loop of `s += x` is linear. */
    int64_t need = (int64_t)sizeof(zell_string) + la + lb + 1;
    int self = (b == a);
    if (zell__object_capacity(&a->hdr) < need) {
        int64_t cap = need + need / 2 + 16;
        a = (zell_string *)zell__object_resize(&a->hdr, cap);
        a->data = (char *)(a + 1);
    }
    memcpy(a->data + la, self ? a->data : b->data, (size_t)lb);
    a->len = la + lb;
    a->data[a->len] = ZELL__STR_END;
    return a;
}

int32_t zell_string_compare(const zell_string *a, const zell_string *b) {
    int64_t la = slen(a), lb = slen(b);
    int64_t m = la < lb ? la : lb;
    int c = m ? memcmp(sdata(a), sdata(b), (size_t)m) : 0;
    if (c < 0) return -1;
    if (c > 0) return 1;
    return la < lb ? -1 : (la > lb ? 1 : 0);
}

bool zell_string_eq(const zell_string *a, const zell_string *b) {
    if (a == b) return true;
    int64_t la = slen(a);
    if (la != slen(b)) return false;
    return la == 0 || memcmp(a->data, b->data, (size_t)la) == 0;
}

uint64_t zell_hash_bytes(const void *p, int64_t n) {
    const unsigned char *b = p;
    uint64_t h = 1469598103934665603ULL;
    for (int64_t i = 0; i < n; i++) { h ^= b[i]; h *= 1099511628211ULL; }
    return h;
}

uint64_t zell_string_hash(const zell_string *s) { return zell_hash_bytes(sdata(s), slen(s)); }

zell_string *zell_string_slice(const zell_string *s, int64_t lo, int64_t hi) {
    int64_t n = slen(s);
    if (lo < 0 || hi < lo || hi > n) zell_panic_slice_bounds(lo, hi, n);
    if (lo == 0 && hi == n && s) {
        zell_retain((void *)s);
        return (zell_string *)s;
    }
    return zell__string_sub(s, lo, hi);
}

uint8_t zell_string_byte_at(const zell_string *s, int64_t i) {
    int64_t n = slen(s);
    if (i < 0 || i >= n) zell_panic_bounds(i, n);
    return (uint8_t)s->data[i];
}

const char *zell__memmem(const char *hay, int64_t n, const char *needle, int64_t m) {
    if (m > n) return NULL;
    /* memchr for the first byte (vectorised in libc), then verify. */
    const char *p = hay, *last = hay + (n - m);
    unsigned char c0 = (unsigned char)needle[0];
    while (p <= last) {
        p = memchr(p, c0, (size_t)(last - p + 1));
        if (!p) return NULL;
        if (memcmp(p + 1, needle + 1, (size_t)(m - 1)) == 0) return p;
        p++;
    }
    return NULL;
}

int64_t zell_string_index(const zell_string *s, const zell_string *sub) {
    int64_t n = slen(s), m = slen(sub);
    if (m == 0) return 0;
    const char *hs = sdata(s);
    const char *f = zell__memmem(hs, n, sdata(sub), m);
    return f ? (int64_t)(f - hs) : -1;
}

bool zell_string_contains(const zell_string *s, const zell_string *sub) {
    return zell_string_index(s, sub) >= 0;
}

bool zell_string_has_prefix(const zell_string *s, const zell_string *p) {
    int64_t m = slen(p);
    return m <= slen(s) && (m == 0 || memcmp(sdata(s), sdata(p), (size_t)m) == 0);
}

bool zell_string_has_suffix(const zell_string *s, const zell_string *p) {
    int64_t n = slen(s), m = slen(p);
    return m <= n && (m == 0 || memcmp(sdata(s) + n - m, sdata(p), (size_t)m) == 0);
}

/* ---- conversions ---- */

/* Decimal digits of v at the end of buf[0..32); returns the start. */
static char *utoa_end(char *end, uint64_t v) {
    char *p = end;
    do { *--p = (char)('0' + v % 10); v /= 10; } while (v);
    return p;
}

static const char zell__digits2[201] =
    "00010203040506070809101112131415161718192021222324252627282930313233343536373839"
    "40414243444546474849505152535455565758596061626364656667686970717273747576777879"
    "8081828384858687888990919293949596979899";

static int udigits(uint64_t v) {
    int n = 1;
    while (v >= 10000) { v /= 10000; n += 4; }
    if (v >= 1000) return n + 3;
    if (v >= 100) return n + 2;
    if (v >= 10) return n + 1;
    return n;
}

/* Writes the nd digits of v ending at out+nd (two digits at a time). */
static void write_digits(char *out, int nd, uint64_t v) {
    char *p = out + nd;
    while (v >= 100) {
        unsigned r = (unsigned)(v % 100); v /= 100;
        p -= 2; memcpy(p, zell__digits2 + 2 * r, 2);
    }
    if (v >= 10) { p -= 2; memcpy(p, zell__digits2 + 2 * v, 2); }
    else *--p = (char)('0' + v);
}

zell_string *zell_string_from_int(int64_t v) {
    uint64_t u = v < 0 ? 0 - (uint64_t)v : (uint64_t)v;
    int nd = udigits(u), neg = v < 0;
    zell_string *s = zell__string_alloc_uninit(nd + neg);
    if (neg) s->data[0] = '-';
    write_digits(s->data + neg, nd, u);
    return s;
}

zell_string *zell_string_from_uint(uint64_t v) {
    int nd = udigits(v);
    zell_string *s = zell__string_alloc_uninit(nd);
    write_digits(s->data, nd, v);
    return s;
}

/* Fast exact path of zell__format_float for values printed in fixed
 * notation with few digits (1e-4 <= |v| < 1e6, at most 15 significant
 * digits): finds the smallest k such that m = round(v * 10^k) parses back
 * to v. That m is the shortest round-trip digit string, i.e. exactly what
 * the %.*e search below produces. Returns the length, or -1 to fall back. */
static int format_float_fast(char *buf, size_t cap, double v) {
    static const double pow10[] = { 1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8,
                                    1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
                                    1e17, 1e18, 1e19, 1e20 };
    double a = fabs(v);
    if (!(a >= 1e-4 && a < 1e6) || cap < 32) return -1;
    for (int k = 0; k <= 20; k++) {
        double p = pow10[k];
        double m = rint(a * p);
        if (m >= 1e15) return -1;            /* > 15 digits: use the general path */
        if (m / p != a) continue;            /* exact: m, p < 2^53, one rounding */
        /* m must be the correctly rounded k-decimal value of a, not a
         * neighbour that happens to round-trip: check the exact residual. */
        double r = fma(a, p, -m);
        if (fabs(r) >= 0.4999) return -1;
        if (m >= 1e6 * p) return -1;         /* rounded up to 1e6: exponent form */
        char dig[32];
        char *end = dig + sizeof dig;
        char *d = utoa_end(end, (uint64_t)m);
        int nd = (int)(end - d);
        size_t n = 0;
        if (v < 0) buf[n++] = '-';
        if (nd <= k) {                       /* 0.000ddd */
            buf[n++] = '0';
            buf[n++] = '.';
            for (int i = nd; i < k; i++) buf[n++] = '0';
            memcpy(buf + n, d, (size_t)nd);
            n += (size_t)nd;
        } else {
            memcpy(buf + n, d, (size_t)(nd - k));
            n += (size_t)(nd - k);
            if (k > 0) {
                buf[n++] = '.';
                memcpy(buf + n, d + nd - k, (size_t)k);
                n += (size_t)k;
            }
        }
        buf[n] = 0;
        return (int)n;
    }
    return -1;
}

int zell__format_float(char *buf, size_t cap, double v) {
    if (isnan(v)) return snprintf(buf, cap, "NaN");
    if (isinf(v)) return snprintf(buf, cap, v > 0 ? "+Inf" : "-Inf");
    if (v == 0) return snprintf(buf, cap, signbit(v) ? "-0" : "0");
    int fast = format_float_fast(buf, cap, v);
    if (fast >= 0) return fast;

    /* Shortest precision that round-trips, in %e form to learn the exponent. */
    char tmp[64];
    int prec = 1;
    for (; prec <= 17; prec++) {
        snprintf(tmp, sizeof tmp, "%.*e", prec - 1, v);
        if (strtod(tmp, NULL) == v) break;
    }
    /* tmp looks like "-d.ddde+XX"; extract exponent and digits. */
    char *epos = strchr(tmp, 'e');
    int exp = atoi(epos + 1);
    char digits[32];
    int nd = 0;
    for (char *c = tmp; c < epos; c++) {
        if (*c >= '0' && *c <= '9') digits[nd++] = *c;
    }
    digits[nd] = 0;
    bool neg = v < 0;

    /* Go %v (strconv 'g', shortest precision): exponent form when
       exp < -4 || exp >= 6, else fixed form without trailing zeros. */
    if (exp < -4 || exp >= 6) {
        if (nd > 1)
            return snprintf(buf, cap, "%s%c.%se%c%02d", neg ? "-" : "", digits[0], digits + 1,
                            exp < 0 ? '-' : '+', exp < 0 ? -exp : exp);
        return snprintf(buf, cap, "%s%ce%c%02d", neg ? "-" : "", digits[0],
                        exp < 0 ? '-' : '+', exp < 0 ? -exp : exp);
    }
    size_t n = 0;
    if (neg && n < cap) buf[n++] = '-';
    if (exp < 0) {
        /* 0.000ddd */
        n += (size_t)snprintf(buf + n, cap - n, "0.");
        for (int i = 0; i < -exp - 1 && n < cap; i++) buf[n++] = '0';
        n += (size_t)snprintf(buf + n, cap - n, "%s", digits);
    } else {
        int int_len = exp + 1;
        if (nd <= int_len) {
            n += (size_t)snprintf(buf + n, cap - n, "%s", digits);
            for (int i = nd; i < int_len && n < cap; i++) buf[n++] = '0';
            if (n < cap) buf[n] = 0;
        } else {
            n += (size_t)snprintf(buf + n, cap - n, "%.*s.%s", int_len, digits, digits + int_len);
        }
    }
    return (int)n;
}

zell_string *zell_string_from_float(double v) {
    char buf[64];
    int n = zell__format_float(buf, sizeof buf, v);
    return zell_string_new(buf, n);
}

zell_string *zell_string_from_bool(bool v) {
    return v ? zell_string_new("true", 4) : zell_string_new("false", 5);
}

zell_string *zell_string_from_rune(int32_t r) {
    char buf[4];
    int n = zell__utf8_encode(buf, r);
    return zell_string_new(buf, n);
}

static void set_err(zell_error **err, const char *fmt, const zell_string *s) {
    if (!err) return;
    char buf[256];
    snprintf(buf, sizeof buf, fmt, zell_string_cstr(s));
    *err = zell_error_new_cstr(buf);
}

int64_t zell_string_to_int(const zell_string *s, zell_error **err) {
    if (err) *err = NULL;
    const char *p = zell_string_cstr(s);
    int64_t n = slen(s);
    if (n == 0) { set_err(err, "invalid integer: \"%s\"", s); return 0; }
    /* reject leading whitespace / embedded NULs that strtoll would accept */
    if ((int64_t)strlen(p) != n || isspace((unsigned char)p[0])) {
        set_err(err, "invalid integer: \"%s\"", s); return 0;
    }
    errno = 0;
    char *end;
    long long v = strtoll(p, &end, 10);
    if (end != p + n) { set_err(err, "invalid integer: \"%s\"", s); return 0; }
    if (errno == ERANGE) { set_err(err, "integer out of range: \"%s\"", s); return 0; }
    return (int64_t)v;
}

double zell_string_to_float(const zell_string *s, zell_error **err) {
    if (err) *err = NULL;
    const char *p = zell_string_cstr(s);
    int64_t n = slen(s);
    if (n == 0 || (int64_t)strlen(p) != n || isspace((unsigned char)p[0])) {
        set_err(err, "invalid float: \"%s\"", s); return 0;
    }
    errno = 0;
    char *end;
    double v = strtod(p, &end);
    if (end != p + n) { set_err(err, "invalid float: \"%s\"", s); return 0; }
    if (errno == ERANGE && (v == HUGE_VAL || v == -HUGE_VAL)) {
        set_err(err, "float out of range: \"%s\"", s); return 0;
    }
    return v;
}
