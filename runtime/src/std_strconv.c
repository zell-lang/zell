/* std_strconv.c - the C half of strconv (the rest is Zell,
 * compiler/src/sema/stdlib_src/strconv.zl): float formatting and parsing,
 * which need correctly rounded conversions (snprintf/strtod), and the
 * sentinel errors ErrSyntax / ErrRange. */
#include "rt_internal.h"

#include <errno.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

zell_error *zell_strconv_err_syntax(void);
zell_error *zell_strconv_err_range(void);
zell_string *zell_strconv_rt_format_float(double f, int64_t fmt, int64_t prec);
double zell_strconv_rt_parse_float(const zell_string *s, zell_error **err);

#define SENTINEL(fn, text)                                                                 \
    static ZELL_STRING_STORAGE(sizeof text) fn##_msg = ZELL_STRING_INIT(fn##_msg, text); \
    static zell_error fn##_err = {                                                        \
        .hdr = {ZELL_RC_IMMORTAL, &zell_type_error}, .message = (zell_string *)&fn##_msg, \
    };                                                                                     \
    zell_error *fn(void) { return &fn##_err; }

SENTINEL(zell_strconv_err_syntax, "invalid syntax")
SENTINEL(zell_strconv_err_range, "value out of range")

/* Number of significant digits of the shortest %e form that round-trips. */
static int shortest_digits(double f) {
    char buf[64];
    for (int p = 1; p < 17; p++) {
        snprintf(buf, sizeof buf, "%.*e", p - 1, f);
        if (strtod(buf, NULL) == f) return p;
    }
    return 17;
}

/* Decimal exponent of f in %e form with p significant digits. */
static int exp10_of(double f, int p) {
    char buf[64];
    snprintf(buf, sizeof buf, "%.*e", p - 1, f);
    const char *e = strchr(buf, 'e');
    return e ? atoi(e + 1) : 0;
}

/* FormatFloat(f, fmt, prec): fmt is 'f', 'e', 'E', 'g' or 'G'; prec < 0
 * means the fewest digits that parse back to f exactly (Go semantics). */
zell_string *zell_strconv_rt_format_float(double f, int64_t fmt, int64_t prec) {
    char buf[512];
    if (isnan(f)) return zell_string_from_cstr("NaN");
    if (isinf(f)) return zell_string_from_cstr(f > 0 ? "+Inf" : "-Inf");
    if (prec > 300) prec = 300;
    char c = (char)fmt;
    if (c != 'f' && c != 'e' && c != 'E' && c != 'g' && c != 'G') c = 'g';
    if (prec >= 0) {
        char spec[8] = {'%', '.', '*', c, 0};
        snprintf(buf, sizeof buf, spec, (int)prec, f);
        return zell_string_from_cstr(buf);
    }
    int p = f == 0 ? 1 : shortest_digits(f);
    int x = f == 0 ? 0 : exp10_of(f, p);
    bool use_e = c == 'e' || c == 'E';
    if (c == 'g' || c == 'G') use_e = x < -4 || x >= 6;
    if (use_e) {
        char spec[8] = {'%', '.', '*', (c == 'E' || c == 'G') ? 'E' : 'e', 0};
        snprintf(buf, sizeof buf, spec, p - 1, f);
    } else {
        int dec = p - 1 - x;
        if (dec < 0) dec = 0;
        snprintf(buf, sizeof buf, "%.*f", dec, f);
    }
    return zell_string_from_cstr(buf);
}

/* ParseFloat(s): the whole string must be a float (strtod syntax: decimal,
 * hex, inf, infinity, nan; optional sign; no spaces). Overflow gives
 * +-Inf with ErrRange, as in Go. */
double zell_strconv_rt_parse_float(const zell_string *s, zell_error **err) {
    if (err) *err = NULL;
    const char *p = zell_string_cstr(s);
    int64_t n = s ? s->len : 0;
    if (n == 0 || (int64_t)strlen(p) != n || p[0] == ' ' || (p[0] >= '\t' && p[0] <= '\r')) {
        if (err) *err = zell_strconv_err_syntax();
        return 0;
    }
    errno = 0;
    char *end;
    double v = strtod(p, &end);
    if (end != p + n) {
        if (err) *err = zell_strconv_err_syntax();
        return 0;
    }
    if (errno == ERANGE && isinf(v)) {
        if (err) *err = zell_strconv_err_range();
        return v;
    }
    return v;
}

/* strconv.Itoa / FormatInt / FormatUint in base 10: the string's digits are
 * written straight into the result (string.c). +1 return. */
zell_string *zell_strconv_rt_itoa(int64_t n) { return zell_string_from_int(n); }
zell_string *zell_strconv_rt_utoa(uint64_t n) { return zell_string_from_uint(n); }
