/* std_fmt.c - fmt.Format (zell_fmt_format). The compiler converts every
 * argument to a string (as println would print it) and passes them as a
 * []string; this function only substitutes the placeholders. */
#include "rt_internal.h"

#include <string.h>

/* Widest alignment accepted; a larger one leaves the placeholder as text. */
#define ZELL__FMT_MAX_WIDTH 1000000

/* If tmpl[i] starts a placeholder "{N}" or "{N,W}" / "{N,-W}" (N, W = one or
 * more ASCII digits) whose index is < nargs, store the index in *idx, the
 * alignment in *align (0 = none, > 0 right-aligns to that width, < 0
 * left-aligns, as in C#) and the placeholder length in *plen. Indices and
 * widths too large to be valid are rejected without overflowing. */
static bool placeholder(const char *t, int64_t n, int64_t i, int64_t nargs,
                        int64_t *idx, int64_t *align, int64_t *plen) {
    if (t[i] != '{') return false;
    int64_t j = i + 1, v = 0;
    bool big = false;
    while (j < n && t[j] >= '0' && t[j] <= '9') {
        if (v > (INT64_MAX - 9) / 10) big = true;
        else v = v * 10 + (t[j] - '0');
        j++;
    }
    if (j == i + 1 || j >= n || big || v >= nargs) return false;
    int64_t a = 0;
    if (t[j] == ',') {
        j++;
        bool neg = j < n && t[j] == '-';
        if (neg) j++;
        int64_t k = j;
        while (j < n && t[j] >= '0' && t[j] <= '9') {
            if (a <= ZELL__FMT_MAX_WIDTH) a = a * 10 + (t[j] - '0');
            j++;
        }
        if (j == k || a > ZELL__FMT_MAX_WIDTH) return false;
        if (neg) a = -a;
    }
    if (j >= n || t[j] != '}') return false;
    *idx = v;
    *align = a;
    *plen = j + 1 - i;
    return true;
}

/* Code points of s (UTF-8 lead bytes). */
static int64_t runes(const zell_string *s) {
    const unsigned char *d = (const unsigned char *)zell_string_data(s);
    int64_t n = zell_string_len(s), c = 0;
    for (int64_t i = 0; i < n; i++) c += (d[i] & 0xC0) != 0x80;
    return c;
}

/* Spaces needed to align s to |align| code points. */
static int64_t pad_of(const zell_string *s, int64_t align) {
    int64_t w = align < 0 ? -align : align;
    if (w == 0) return 0;
    int64_t r = runes(s);
    return r < w ? w - r : 0;
}

zell_string *zell_fmt_format(const zell_string *tmpl, const zell_slice *args) {
    const char *t = zell_string_data(tmpl);
    int64_t n = zell_string_len(tmpl);
    int64_t nargs = args ? args->len : 0;
    zell_string *const *av = nargs ? args->data : NULL;
    int64_t total = 0, idx, al, pl;
    for (int64_t i = 0; i < n;) {
        if (placeholder(t, n, i, nargs, &idx, &al, &pl)) {
            total += zell_string_len(av[idx]) + pad_of(av[idx], al);
            i += pl;
        } else {
            total++;
            i++;
        }
    }
    zell_string *r = zell__string_alloc(total);
    char *w = r->data;
    for (int64_t i = 0; i < n;) {
        if (placeholder(t, n, i, nargs, &idx, &al, &pl)) {
            int64_t l = zell_string_len(av[idx]), p = pad_of(av[idx], al);
            if (al > 0 && p) { memset(w, ' ', (size_t)p); w += p; }
            if (l) memcpy(w, zell_string_data(av[idx]), (size_t)l);
            w += l;
            if (al < 0 && p) { memset(w, ' ', (size_t)p); w += p; }
            i += pl;
        } else {
            *w++ = t[i++];
        }
    }
    return r;
}
