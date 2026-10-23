/* std_strings_ext.c - more strings functions (zell_strings_*), Go
 * semantics unless ABI.md section 14 says otherwise. Offsets and counts are
 * in bytes except where "runes" is stated; invalid UTF-8 bytes count as one
 * rune each (decoded as U+FFFD). */
#include "rt_internal.h"

#include <string.h>

static inline int64_t slen(const zell_string *s) { return s ? s->len : 0; }
static inline const char *sdata(const zell_string *s) { return s ? s->data : ""; }

static zell_string *same(const zell_string *s) {
    if (!s) return zell__string_alloc(0);
    zell_retain((void *)s);
    return (zell_string *)s;
}

static zell_string *sub(const zell_string *s, int64_t lo, int64_t hi) {
    if (lo == 0 && hi == slen(s)) return same(s);
    return zell__string_sub(s, lo, hi);
}

static bool is_space(int32_t r) {
    switch (r) {
    case '\t': case '\n': case '\v': case '\f': case '\r': case ' ':
    case 0x85: case 0xA0: case 0x1680: case 0x2028: case 0x2029:
    case 0x202F: case 0x205F: case 0x3000:
        return true;
    default:
        return r >= 0x2000 && r <= 0x200A;
    }
}

/* Start offset of the rune that ends at hi (hi > 0). */
static int64_t rune_start_before(const zell_string *s, int64_t hi, int32_t *r) {
    const unsigned char *p = (const unsigned char *)sdata(s);
    int64_t st = hi - 1;
    while (st > 0 && hi - st < 4 && (p[st] & 0xC0) == 0x80) st--;
    int64_t w;
    int32_t c = zell_string_decode_rune(s, st, &w);
    if (st + w != hi) { st = hi - 1; c = 0xFFFD; }
    *r = c;
    return st;
}

/* Appends s[off:off+n] (a view or a copy) to out. */
static void push(zell_slice *out, const zell_string *s, int64_t off, int64_t n) {
    zell_string *x = zell__string_sub(s, off, off + n);
    zell_slice_append(out, 8, &zell_ops_ref, &x);
    zell_release(x);
}

/* ---- Fields ---- */

static inline bool field_space(const zell_string *s, int64_t i, int64_t *w) {
    unsigned char c = (unsigned char)s->data[i];
    if (c < 0x80) {
        *w = 1;
        return c == ' ' || (c >= '\t' && c <= '\r');
    }
    int32_t r = zell_string_decode_rune(s, i, w);
    return is_space(r) && !(r == 0xFFFD && *w == 1);
}

/* Fields' pieces are views in one view block, like Split's. */
void zell_strings_fields(const zell_string *s, zell_slice *out) {
    int64_t n = slen(s), count = 0;
    for (int64_t i = 0, in = 0; i < n;) {
        int64_t w;
        bool sp = field_space(s, i, &w);
        if (!sp && !in) count++;
        in = !sp;
        i += w;
    }
    zell_slice_new_ops(out, 8, count, count, &zell_ops_ref);
    if (count == 0) return;
    zell_string **slots = out->data;
    zell__vblock *b = zell__vblock_new(s, count);
    int64_t start = -1, k = 0;
    for (int64_t i = 0; i < n;) {
        int64_t w;
        bool sp = field_space(s, i, &w);
        if (sp && start >= 0) { slots[k++] = zell__vblock_add(b, s->data + start, i - start); start = -1; }
        if (!sp && start < 0) start = i;
        i += w;
    }
    if (start >= 0) slots[k++] = zell__vblock_add(b, s->data + start, n - start);
    if (b) zell__vblock_done(b);
}

/* ---- Count / LastIndex ---- */

int64_t zell_strings_count(const zell_string *s, const zell_string *substr) {
    int64_t n = slen(s), m = slen(substr), c = 0;
    if (m == 0) return zell_string_rune_count(s) + 1;
    const char *p = sdata(s), *q = sdata(substr);
    for (int64_t i = 0; i + m <= n;) {
        if (memcmp(p + i, q, (size_t)m) == 0) { c++; i += m; } else i++;
    }
    return c;
}

int64_t zell_strings_last_index(const zell_string *s, const zell_string *substr) {
    int64_t n = slen(s), m = slen(substr);
    if (m > n) return -1;
    const char *p = sdata(s), *q = sdata(substr);
    for (int64_t i = n - m; i >= 0; i--)
        if (memcmp(p + i, q, (size_t)m) == 0) return i;
    return -1;
}

/* ---- trimming ---- */

zell_string *zell_strings_trim_prefix(const zell_string *s, const zell_string *prefix) {
    if (slen(prefix) && zell_string_has_prefix(s, prefix)) return sub(s, slen(prefix), slen(s));
    return same(s);
}

zell_string *zell_strings_trim_suffix(const zell_string *s, const zell_string *suffix) {
    if (slen(suffix) && zell_string_has_suffix(s, suffix)) return sub(s, 0, slen(s) - slen(suffix));
    return same(s);
}

/* Is rune r (of width w at s) in cutset? Invalid bytes match only the same byte. */
static bool in_cutset(const zell_string *cutset, int32_t r, bool invalid, unsigned char byte) {
    int64_t n = slen(cutset);
    for (int64_t i = 0; i < n;) {
        int64_t w;
        int32_t c = zell_string_decode_rune(cutset, i, &w);
        bool cinv = c == 0xFFFD && w == 1;
        if (invalid ? (cinv && (unsigned char)sdata(cutset)[i] == byte) : (!cinv && c == r)) return true;
        i += w;
    }
    return false;
}

zell_string *zell_strings_trim_left(const zell_string *s, const zell_string *cutset) {
    int64_t n = slen(s), lo = 0;
    while (lo < n) {
        int64_t w;
        int32_t r = zell_string_decode_rune(s, lo, &w);
        if (!in_cutset(cutset, r, r == 0xFFFD && w == 1, (unsigned char)sdata(s)[lo])) break;
        lo += w;
    }
    return sub(s, lo, n);
}

zell_string *zell_strings_trim_right(const zell_string *s, const zell_string *cutset) {
    int64_t hi = slen(s);
    while (hi > 0) {
        int32_t r;
        int64_t st = rune_start_before(s, hi, &r);
        bool inv = r == 0xFFFD && hi - st == 1;
        if (!in_cutset(cutset, r, inv, (unsigned char)sdata(s)[st])) break;
        hi = st;
    }
    return sub(s, 0, hi);
}

/* ---- SplitN ---- */

void zell_strings_split_n(const zell_string *s, const zell_string *sep, int64_t n,
                           zell_slice *out) {
    if (n < 0) { zell_strings_split(s, sep, out); return; }
    zell_slice_new_ops(out, 8, 0, n < 16 ? n : 16, &zell_ops_ref);
    if (n == 0) return;
    int64_t len = slen(s), m = slen(sep), start = 0, i = 0;
    const char *p = sdata(s), *q = sdata(sep);
    if (m == 0) {
        while (i < len && out->len < n - 1) {
            int64_t w;
            zell_string_decode_rune(s, i, &w);
            push(out, s, i, w);
            i += w;
        }
        if (i < len) push(out, s, i, len - i);
        return;
    }
    while (out->len < n - 1 && i + m <= len) {
        if (memcmp(p + i, q, (size_t)m) == 0) {
            push(out, s, start, i - start);
            i += m;
            start = i;
        } else {
            i++;
        }
    }
    push(out, s, start, len - start);
}

/* ---- EqualFold / Compare ---- */

bool zell_strings_equal_fold(const zell_string *a, const zell_string *b) {
    /* simple folding: lower(upper(x)) maps ς, σ, Σ (and ſ, s, S) together */
    zell_string *ua = zell_strings_to_upper(a), *ub = zell_strings_to_upper(b);
    zell_string *la = zell_strings_to_lower(ua), *lb = zell_strings_to_lower(ub);
    bool eq = zell_string_eq(la, lb);
    zell_release(ua); zell_release(ub); zell_release(la); zell_release(lb);
    return eq;
}

int64_t zell_strings_compare(const zell_string *a, const zell_string *b) {
    return zell_string_compare(a, b);
}

/* ---- Reverse / Pad / Title ---- */

zell_string *zell_strings_reverse(const zell_string *s) {
    int64_t n = slen(s);
    zell_string *r = zell__string_alloc(n);
    for (int64_t i = 0; i < n;) {
        int64_t w;
        zell_string_decode_rune(s, i, &w);
        memcpy(r->data + (n - i - w), sdata(s) + i, (size_t)w);
        i += w;
    }
    return r;
}

static zell_string *pad(const zell_string *s, int64_t width, const zell_string *padding, bool left) {
    int64_t have = zell_string_rune_count(s);
    if (have >= width || slen(padding) == 0) return same(s);
    int64_t need = width - have;
    /* `need` runes of padding cycled from `padding`: at most 4 bytes each */
    if (need > (INT64_MAX / 8)) zell_panic("strings: Pad width overflow");
    char *buf = zell_alloc(need * 4 + slen(s) + 1);
    int64_t len = 0, pi = 0, pn = slen(padding);
    if (!left) { memcpy(buf, sdata(s), (size_t)slen(s)); len = slen(s); }
    for (int64_t k = 0; k < need; k++) {
        int64_t w;
        zell_string_decode_rune(padding, pi, &w);
        memcpy(buf + len, sdata(padding) + pi, (size_t)w);
        len += w;
        pi += w;
        if (pi >= pn) pi = 0;
    }
    if (left) { memcpy(buf + len, sdata(s), (size_t)slen(s)); len += slen(s); }
    zell_string *r = zell_string_new(buf, len);
    zell_free(buf);
    return r;
}

zell_string *zell_strings_pad_left(const zell_string *s, int64_t width, const zell_string *padding) {
    return pad(s, width, padding, true);
}

zell_string *zell_strings_pad_right(const zell_string *s, int64_t width, const zell_string *padding) {
    return pad(s, width, padding, false);
}

zell_string *zell_strings_title(const zell_string *s) {
    int64_t n = slen(s);
    char *buf = zell_alloc(n + 1);
    int64_t len = 0;
    bool word_start = true;
    for (int64_t i = 0; i < n;) {
        int64_t w;
        int32_t r = zell_string_decode_rune(s, i, &w);
        bool sp = is_space(r) && !(r == 0xFFFD && w == 1);
        if (word_start && !sp) {
            zell_string *one = zell_string_new(sdata(s) + i, w);
            zell_string *up = zell_strings_to_upper(one);   /* never grows (ABI 14.2) */
            memcpy(buf + len, up->data, (size_t)up->len);
            len += up->len;
            zell_release(up);
            zell_release(one);
        } else {
            memcpy(buf + len, sdata(s) + i, (size_t)w);
            len += w;
        }
        word_start = sp;
        i += w;
    }
    zell_string *res = zell_string_new(buf, len);
    zell_free(buf);
    return res;
}

/* ---- runes / bytes ---- */

void zell_strings_runes(const zell_string *s, zell_slice *out) {
    int64_t cnt = zell_string_rune_count(s), n = slen(s);
    zell_slice_new(out, 4, cnt, cnt);
    int32_t *rs = out->data;
    for (int64_t i = 0, k = 0; i < n; k++) {
        int64_t w;
        rs[k] = zell_string_decode_rune(s, i, &w);
        i += w;
    }
}

zell_string *zell_strings_from_runes(const zell_slice *runes) {
    int64_t cnt = runes ? runes->len : 0;
    if (cnt == 0) return zell__string_alloc(0);
    const int32_t *rs = runes->data;
    char *buf = zell_alloc(cnt * 4 + 1);
    int64_t len = 0;
    for (int64_t i = 0; i < cnt; i++) {
        int32_t r = rs[i];
        if (r < 0 || r > 0x10FFFF || (r >= 0xD800 && r <= 0xDFFF)) r = 0xFFFD;
        len += zell__utf8_encode(buf + len, r);
    }
    zell_string *res = zell_string_new(buf, len);
    zell_free(buf);
    return res;
}

void zell_strings_bytes(const zell_string *s, zell_slice *out) { zell_string_to_bytes(out, s); }

zell_string *zell_strings_from_bytes(const zell_slice *bytes) {
    if (!bytes || bytes->len == 0) return zell__string_alloc(0);
    return zell_string_from_bytes(bytes);
}

/* ---- byte-buffer primitives for the Zell-source strings.Builder and io
 * (ABI.md section 14, "rt" helpers): bounds are clamped, never trapped, so
 * the Zell side checks lengths and these only move bytes. ---- */

/* Clamps [off, off+n) to [0, len); returns the count that fits. */
static int64_t zell__clamp_span(int64_t len, int64_t off, int64_t n) {
    if (off < 0 || off >= len || n <= 0) return 0;
    return n > len - off ? len - off : n;
}

void zell_strings_rt_copy(zell_slice *dst, int64_t dst_off, const zell_slice *src, int64_t src_off,
                          int64_t n) {
    n = zell__clamp_span(dst->len, dst_off, n);
    n = zell__clamp_span(src->len, src_off, n);
    if (n > 0) memmove((char *)dst->data + dst_off, (const char *)src->data + src_off, (size_t)n);
}

void zell_strings_rt_copy_string(zell_slice *dst, int64_t dst_off, const zell_string *s,
                                 int64_t src_off, int64_t n) {
    n = zell__clamp_span(dst->len, dst_off, n);
    n = zell__clamp_span(s ? s->len : 0, src_off, n);
    if (n > 0) memcpy((char *)dst->data + dst_off, s->data + src_off, (size_t)n);
}

int64_t zell_strings_rt_index_byte(const zell_slice *b, int64_t from, int64_t to, int64_t c) {
    if (from < 0) from = 0;
    if (to > b->len) to = b->len;
    if (from >= to) return -1;
    const char *p = memchr((const char *)b->data + from, (int)(c & 0xff), (size_t)(to - from));
    return p ? (int64_t)(p - (const char *)b->data) : -1;
}

void zell_strings_rt_make_bytes(int64_t n, zell_slice *out) {
    if (n < 0) n = 0;
    zell_slice_new(out, 1, n, n);
}

/* Byte-wise order of two strings (NULL is ""). */
static int zell__str_less(const zell_string *a, const zell_string *b) {
    int64_t la = a ? a->len : 0, lb = b ? b->len : 0;
    int64_t n = la < lb ? la : lb;
    int c = n > 0 ? memcmp(a->data, b->data, (size_t)n) : 0;
    return c != 0 ? c < 0 : la < lb;
}

static void zell__str_insertion(zell_string **a, int64_t lo, int64_t hi) {
    for (int64_t i = lo + 1; i < hi; i++) {
        zell_string *x = a[i];
        int64_t j = i;
        while (j > lo && zell__str_less(x, a[j - 1])) { a[j] = a[j - 1]; j--; }
        a[j] = x;
    }
}

static void zell__str_sift(zell_string **a, int64_t lo, int64_t root, int64_t n) {
    for (;;) {
        int64_t child = 2 * root + 1;
        if (child >= n) return;
        if (child + 1 < n && zell__str_less(a[lo + child], a[lo + child + 1])) child++;
        if (!zell__str_less(a[lo + root], a[lo + child])) return;
        zell_string *t = a[lo + root]; a[lo + root] = a[lo + child]; a[lo + child] = t;
        root = child;
    }
}

static void zell__str_heapsort(zell_string **a, int64_t lo, int64_t hi) {
    int64_t n = hi - lo;
    for (int64_t i = n / 2 - 1; i >= 0; i--) zell__str_sift(a, lo, i, n);
    for (int64_t i = n - 1; i > 0; i--) {
        zell_string *t = a[lo]; a[lo] = a[lo + i]; a[lo + i] = t;
        zell__str_sift(a, lo, 0, i);
    }
}

/* Introsort: median-of-three quicksort, insertion sort below 16 elements,
 * heapsort when the recursion depth is exhausted. A partition compares
 * every element with one cached pivot, so the cache misses on the
 * scattered string objects overlap; a merge sort serialises them on the
 * branch of every step (5x slower on 2M strings). */
static void zell__str_introsort(zell_string **a, int64_t lo, int64_t hi, int depth) {
    while (hi - lo > 16) {
        if (depth-- == 0) { zell__str_heapsort(a, lo, hi); return; }
        int64_t m = lo + (hi - lo) / 2;
        zell_string *t;
        if (zell__str_less(a[m], a[lo])) { t = a[m]; a[m] = a[lo]; a[lo] = t; }
        if (zell__str_less(a[hi - 1], a[m])) {
            t = a[hi - 1]; a[hi - 1] = a[m]; a[m] = t;
            if (zell__str_less(a[m], a[lo])) { t = a[m]; a[m] = a[lo]; a[lo] = t; }
        }
        zell_string *pivot = a[m];
        int64_t i = lo, j = hi - 1;
        for (;;) {
            while (zell__str_less(a[i], pivot)) i++;
            while (zell__str_less(pivot, a[j])) j--;
            if (i >= j) break;
            t = a[i]; a[i] = a[j]; a[j] = t;
            i++; j--;
        }
        /* recurse into the smaller half, loop on the larger */
        if (j + 1 - lo < hi - (j + 1)) { zell__str_introsort(a, lo, j + 1, depth); lo = j + 1; }
        else { zell__str_introsort(a, j + 1, hi, depth); hi = j + 1; }
    }
    zell__str_insertion(a, lo, hi);
}

/* slices.Sort<string>: sorts the string pointers in place, byte-wise, with
 * an introsort; the backing array keeps its references, so nothing is
 * retained or released. Equal strings are indistinguishable, so the
 * missing stability is unobservable. */
void zell_strings_rt_sort(zell_slice *xs) {
    int64_t n = xs->len;
    if (n < 2) return;
    int depth = 0;
    for (int64_t k = n; k > 0; k >>= 1) depth += 2;
    zell__str_introsort((zell_string **)xs->data, 0, n, depth);
}
