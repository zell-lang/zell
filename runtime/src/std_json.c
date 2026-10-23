/* std_json.c - the C half of encoding.json (runtime/ABI.md 17.4): a
 * one-pass parser into an arena document, node queries, a growing text
 * writer and string quoting. json.Value, Parse<T> and Format<T> are Zell
 * (compiler/src/sema/reflect_src/json.zl) on top of these.
 *
 * Document. zell_encoding_json_rt_parse reads a JSON text (RFC 8259) into
 * one array of 16-byte nodes in depth-first document order: a container
 * node is followed by the nodes of its elements (an object: key node, value
 * node, key node, ...), and records how many nodes its subtree spans, so the
 * next sibling of a node is `node + span`. String nodes point into the
 * source text when the string has no escapes and into the document's own
 * byte arena when it does; the document retains the text. Numbers are
 * parsed on the fly: an integer that fits int64 is kept as one (and knows
 * it), everything else is a double. The whole document is freed in one call.
 * Handles are addresses passed as int64_t: the document for free/index, the
 * node for every query (0 = no node = null).
 *
 * Lexical errors fail with "json: <what> at offset N" (byte offset), the
 * grammar with the messages of the former Zell parser ("json: expected , or
 * ] in an array, found number 2", "json: unexpected end of input").
 * Nesting deeper than ZELL__JSON_MAX_DEPTH containers is "json: nesting too
 * deep".
 *
 * Writer. zell_encoding_json_rt_w_* append to one growing zell_string
 * (resized in place, so a long Format is linear); _w_finish returns it. */
#include "rt_internal.h"

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(__SSE2__)
#include <emmintrin.h>
#endif

enum {
    ZELL__JSON_NULL = 0,
    ZELL__JSON_BOOL = 1,
    ZELL__JSON_NUM = 2,
    ZELL__JSON_STR = 3,
    ZELL__JSON_ARR = 4,
    ZELL__JSON_OBJ = 5,
};

enum {
    ZELL__JSON_F_INT = 1,   /* number node: u.i holds the exact integer */
    ZELL__JSON_F_TABLE = 2, /* array node: u.c.table is an index table */
};

#define ZELL__JSON_MAX_DEPTH 512
#define ZELL__JSON_TABLE_MIN 16 /* arrays shorter than this are indexed by walking */

typedef struct zell__json_node {
    uint8_t type;
    uint8_t flags;
    uint16_t pad;
    uint32_t len; /* STR: bytes; ARR: elements; OBJ: pairs; BOOL: 0/1 */
    union {
        int64_t i;     /* NUM with F_INT */
        double f;      /* NUM without F_INT */
        const char *s; /* STR */
        struct {
            uint32_t span;  /* nodes in the subtree, this one included */
            uint32_t table; /* ARR with F_TABLE: index into doc->tables */
        } c;
    } u;
} zell__json_node;

_Static_assert(sizeof(zell__json_node) == 16, "json node is 16 bytes");

typedef struct zell__json_doc {
    zell__json_node *nodes;
    uint32_t n, cap;
    zell_string *text; /* retained: unescaped strings point into it */
    /* Byte arena for strings with escapes. */
    char **chunks;
    uint32_t nchunks, capchunks;
    char *cur;
    size_t cur_used, cur_cap;
    /* Lazily built index tables of long arrays (rt_index). */
    uint32_t **tables;
    uint32_t ntables, captables;
} zell__json_doc;

/* ---- helpers ---------------------------------------------------------- */

static inline uint32_t node_span(const zell__json_node *n) {
    return n->type >= ZELL__JSON_ARR ? n->u.c.span : 1;
}

static inline zell__json_node *node_next(zell__json_node *n) { return n + node_span(n); }

static inline int is_ws(uint8_t c) { return c == ' ' || c == '\n' || c == '\r' || c == '\t'; }
static inline int is_digit(uint8_t c) { return c >= '0' && c <= '9'; }

/* First index >= i whose byte is '"', '\\' or a control character (< 0x20),
 * or n when there is none. */
static int64_t find_special(const uint8_t *p, int64_t i, int64_t n) {
#if defined(__SSE2__)
    const __m128i quote = _mm_set1_epi8('"');
    const __m128i bslash = _mm_set1_epi8('\\');
    const __m128i ctl = _mm_set1_epi8(0x1f);
    for (; i + 16 <= n; i += 16) {
        __m128i v = _mm_loadu_si128((const __m128i *)(p + i));
        __m128i m = _mm_or_si128(_mm_cmpeq_epi8(v, quote), _mm_cmpeq_epi8(v, bslash));
        m = _mm_or_si128(m, _mm_cmpeq_epi8(_mm_max_epu8(v, ctl), ctl));
        int mask = _mm_movemask_epi8(m);
        if (mask) return i + __builtin_ctz((unsigned)mask);
    }
#endif
#if defined(__BYTE_ORDER__) && __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
    /* Eight bytes at a time (SWAR): a byte is special when it is < 0x20,
     * or equal to '"' or '\\' (the xor with the byte repeated is zero).
     * Borrows can flag bytes above a special one, never below it, so the
     * lowest flagged byte (ctz, little-endian) is the first special byte. */
    for (; i + 8 <= n; i += 8) {
        uint64_t x;
        memcpy(&x, p + i, 8);
        const uint64_t ones = 0x0101010101010101ull, highs = 0x8080808080808080ull;
        uint64_t lt = (x - 0x2020202020202020ull) & ~x;
        uint64_t q = x ^ (ones * '"');
        uint64_t b = x ^ (ones * '\\');
        uint64_t m = (lt | ((q - ones) & ~q) | ((b - ones) & ~b)) & highs;
        if (m) return i + (int64_t)(__builtin_ctzll(m) >> 3);
    }
#endif
    for (; i < n; i++) {
        uint8_t c = p[i];
        if (c == '"' || c == '\\' || c < 0x20) return i;
    }
    return n;
}

static int64_t skip_ws(const uint8_t *p, int64_t i, int64_t n) {
    if (i >= n || !is_ws(p[i])) return i;
    i++;
#if defined(__SSE2__)
    const __m128i sp = _mm_set1_epi8(' ');
    const __m128i nl = _mm_set1_epi8('\n');
    const __m128i cr = _mm_set1_epi8('\r');
    const __m128i tb = _mm_set1_epi8('\t');
    for (; i + 16 <= n; i += 16) {
        __m128i v = _mm_loadu_si128((const __m128i *)(p + i));
        __m128i m = _mm_or_si128(_mm_or_si128(_mm_cmpeq_epi8(v, sp), _mm_cmpeq_epi8(v, nl)),
                                 _mm_or_si128(_mm_cmpeq_epi8(v, cr), _mm_cmpeq_epi8(v, tb)));
        int mask = ~_mm_movemask_epi8(m) & 0xFFFF;
        if (mask) return i + __builtin_ctz((unsigned)mask);
    }
#endif
    while (i < n && is_ws(p[i])) i++;
    return i;
}

static int hexval(uint8_t c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

static int read_hex4(const uint8_t *p, int64_t n, int64_t i, uint32_t *out) {
    if (i + 4 > n) return 0;
    uint32_t v = 0;
    for (int k = 0; k < 4; k++) {
        int h = hexval(p[i + k]);
        if (h < 0) return 0;
        v = v << 4 | (uint32_t)h;
    }
    *out = v;
    return 1;
}

/* ---- the document ----------------------------------------------------- */

static zell__json_node *doc_push(zell__json_doc *d) {
    if (d->n == d->cap) {
        uint32_t cap = d->cap ? d->cap * 2 : 64;
        d->nodes = zell_realloc(d->nodes, (int64_t)cap * (int64_t)sizeof(zell__json_node));
        d->cap = cap;
    }
    zell__json_node *nd = &d->nodes[d->n++];
    nd->flags = 0;
    nd->pad = 0;
    return nd;
}

static char *doc_bytes(zell__json_doc *d, size_t n) {
    if (d->cur_used + n > d->cur_cap) {
        size_t cap = n > 65536 ? n : 65536;
        if (d->nchunks == d->capchunks) {
            d->capchunks = d->capchunks ? d->capchunks * 2 : 4;
            d->chunks = zell_realloc(d->chunks, (int64_t)(d->capchunks * sizeof(char *)));
        }
        d->cur = zell_alloc((int64_t)cap);
        d->chunks[d->nchunks++] = d->cur;
        d->cur_used = 0;
        d->cur_cap = cap;
    }
    char *r = d->cur + d->cur_used;
    d->cur_used += n;
    return r;
}

/* Frees everything but the node array and the document itself. */
static void doc_reset(zell__json_doc *d) {
    for (uint32_t i = 0; i < d->nchunks; i++) zell_free(d->chunks[i]);
    zell_free(d->chunks);
    for (uint32_t i = 0; i < d->ntables; i++) zell_free(d->tables[i]);
    zell_free(d->tables);
    zell_release(d->text);
    uint32_t cap = d->cap;
    zell__json_node *nodes = d->nodes;
    memset(d, 0, sizeof *d);
    d->cap = cap;
    d->nodes = nodes;
}

static void doc_free(zell__json_doc *d) {
    if (!d) return;
    doc_reset(d);
    zell_free(d->nodes);
    zell_free(d);
}

/* One idle document per thread (its node array kept while it is small):
 * parsing a small text then makes no allocation at all. */
#define ZELL__JSON_DOC_CACHE_NODES 4096
static _Thread_local zell__json_doc *zell__json_doc_cache;

/* ---- the parser ------------------------------------------------------- */

typedef struct zell__json_parser {
    const uint8_t *p;
    int64_t n, i;
    zell__json_doc *d;
    /* Open containers: node index and the elements seen so far. */
    uint32_t stack[ZELL__JSON_MAX_DEPTH];
    uint32_t count[ZELL__JSON_MAX_DEPTH];
    int depth;
    zell_error *err;
} zell__json_parser;

static void fail_at(zell__json_parser *ps, const char *what, int64_t off) {
    char buf[160];
    snprintf(buf, sizeof buf, "json: %s at offset %lld", what, (long long)off);
    if (!ps->err) ps->err = zell_error_new_cstr(buf);
}

static void fail_msg(zell__json_parser *ps, const char *msg) {
    if (!ps->err) ps->err = zell_error_new_cstr(msg);
}

static void fail_char(zell__json_parser *ps, int64_t i) {
    char what[48];
    uint8_t c = ps->p[i];
    if (c >= 0x20 && c < 0x7F) snprintf(what, sizeof what, "invalid character '%c'", c);
    else snprintf(what, sizeof what, "invalid byte 0x%02x", c);
    fail_at(ps, what, i);
}

/* Scans the number at i; on success sets *end and the node and returns 1. */
static int scan_number(zell__json_parser *ps, int64_t i, int64_t *end, zell__json_node *nd) {
    const uint8_t *p = ps->p;
    int64_t n = ps->n, s = i;
    int neg = 0;
    if (p[i] == '-') { neg = 1; i++; }
    if (i >= n || !is_digit(p[i])) { fail_at(ps, "invalid number", s); return 0; }
    uint64_t mant = 0;
    int digits = 0;     /* significant digits accumulated in mant */
    int dropped = 0;    /* digits beyond 19, not in mant */
    int exp10 = 0;      /* decimal exponent of mant */
    int is_float = 0;
    if (p[i] == '0') {
        i++;
    } else {
        while (i < n && is_digit(p[i])) {
            if (digits < 19) { mant = mant * 10 + (uint64_t)(p[i] - '0'); digits++; }
            else { dropped++; exp10++; }
            i++;
        }
    }
    if (i < n && p[i] == '.') {
        is_float = 1;
        i++;
        if (i >= n || !is_digit(p[i])) { fail_at(ps, "invalid number", s); return 0; }
        while (i < n && is_digit(p[i])) {
            if (digits < 19) {
                mant = mant * 10 + (uint64_t)(p[i] - '0');
                if (mant) digits++;
                exp10--;
            } else {
                dropped++;
            }
            i++;
        }
    }
    if (i < n && (p[i] == 'e' || p[i] == 'E')) {
        is_float = 1;
        i++;
        int eneg = 0;
        if (i < n && (p[i] == '+' || p[i] == '-')) { eneg = p[i] == '-'; i++; }
        if (i >= n || !is_digit(p[i])) { fail_at(ps, "invalid number", s); return 0; }
        int e = 0;
        while (i < n && is_digit(p[i])) {
            if (e < 100000) e = e * 10 + (p[i] - '0');
            i++;
        }
        exp10 += eneg ? -e : e;
    }
    *end = i;
    nd->type = ZELL__JSON_NUM;
    nd->len = 0;
    if (!is_float && dropped == 0 && (mant <= (uint64_t)INT64_MAX || (neg && mant == (uint64_t)INT64_MAX + 1)) &&
        !(neg && mant == 0)) {
        nd->flags = ZELL__JSON_F_INT;
        nd->u.i = neg ? -(int64_t)(mant - 1) - 1 : (int64_t)mant;
        return 1;
    }
    nd->flags = 0;
    /* Clinger's fast path: an exact mantissa below 2^53 scaled by an exact
     * power of ten is one correctly rounded operation. */
    static const double pow10[] = { 1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11,
                                    1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22 };
    if (dropped == 0 && mant < ((uint64_t)1 << 53) && exp10 >= -22 && exp10 <= 22) {
        double v = (double)mant;
        v = exp10 < 0 ? v / pow10[-exp10] : v * pow10[exp10];
        nd->u.f = neg ? -v : v;
        return 1;
    }
    char buf[80];
    char *txt = buf;
    int64_t len = i - s;
    if (len >= (int64_t)sizeof buf) txt = zell_alloc(len + 1);
    memcpy(txt, p + s, (size_t)len);
    txt[len] = 0;
    nd->u.f = strtod(txt, NULL);
    if (txt != buf) zell_free(txt);
    if (isinf(nd->u.f)) { fail_at(ps, "number out of range", s); return 0; }
    return 1;
}

/* Scans the string whose opening quote is at i; sets *end past the closing
 * quote and fills the node (into the text, or unescaped into the arena). */
static int scan_string(zell__json_parser *ps, int64_t i, int64_t *end, zell__json_node *nd) {
    const uint8_t *p = ps->p;
    int64_t n = ps->n, start = i + 1;
    int64_t j = find_special(p, start, n);
    if (j < n && p[j] == '"') {
        nd->type = ZELL__JSON_STR;
        nd->len = (uint32_t)(j - start);
        nd->u.s = (const char *)p + start;
        *end = j + 1;
        return 1;
    }
    /* Find the end first (escapes skip a pair), then unescape into the
     * arena: the unescaped text is never longer than the escaped one. */
    int64_t k = j;
    for (;;) {
        if (k >= n) { fail_at(ps, "unterminated string", n); return 0; }
        uint8_t c = p[k];
        if (c == '"') break;
        if (c < 0x20) { fail_at(ps, "control character in string", k); return 0; }
        if (c == '\\') {
            if (k + 1 >= n) { fail_at(ps, "unterminated string", n); return 0; }
            k += 2;
        } else {
            k++;
        }
        k = find_special(p, k, n);
    }
    char *buf = doc_bytes(ps->d, (size_t)(k - start));
    int64_t w = j - start;
    memcpy(buf, p + start, (size_t)w);
    i = j;
    while (i < k) {
        uint8_t c = p[i];
        if (c != '\\') { buf[w++] = (char)c; i++; continue; }
        uint8_t e = p[i + 1];
        i += 2;
        switch (e) {
        case '"': buf[w++] = '"'; break;
        case '\\': buf[w++] = '\\'; break;
        case '/': buf[w++] = '/'; break;
        case 'b': buf[w++] = '\b'; break;
        case 'f': buf[w++] = '\f'; break;
        case 'n': buf[w++] = '\n'; break;
        case 'r': buf[w++] = '\r'; break;
        case 't': buf[w++] = '\t'; break;
        case 'u': {
            uint32_t cp;
            if (!read_hex4(p, k, i, &cp)) { fail_at(ps, "invalid \\u escape", i); return 0; }
            i += 4;
            if (cp >= 0xD800 && cp < 0xDC00) {
                uint32_t lo;
                if (i + 6 <= k && p[i] == '\\' && p[i + 1] == 'u' && read_hex4(p, k, i + 2, &lo) && lo >= 0xDC00 && lo < 0xE000) {
                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                    i += 6;
                } else {
                    cp = 0xFFFD;
                }
            } else if (cp >= 0xDC00 && cp < 0xE000) {
                cp = 0xFFFD;
            }
            w += zell__utf8_encode(buf + w, (int32_t)cp);
            break;
        }
        default:
            fail_at(ps, "invalid escape in string", i - 2);
            return 0;
        }
    }
    nd->type = ZELL__JSON_STR;
    nd->len = (uint32_t)w;
    nd->u.s = buf;
    *end = k + 1;
    return 1;
}

/* Describes the token at i for a grammar error ("number 2", "string
 * \"x\"", "true", "'}'"); a lexical error there is reported instead and 0
 * returned. The text goes to *out (malloc'd by zell_alloc). */
static int describe_token(zell__json_parser *ps, int64_t i, char **out) {
    const uint8_t *p = ps->p;
    int64_t n = ps->n;
    uint8_t c = p[i];
    const char *prefix = NULL;
    int64_t s = i, e = i;
    if (c == '"') {
        zell__json_node tmp;
        if (!scan_string(ps, i, &e, &tmp)) return 0;
        prefix = "string ";
    } else if (c == '-' || is_digit(c)) {
        zell__json_node tmp;
        if (!scan_number(ps, i, &e, &tmp)) return 0;
        prefix = "number ";
    } else if (c && strchr("{}[]:,", c)) {
        char *r = zell_alloc(4);
        r[0] = '\'';
        r[1] = (char)c;
        r[2] = '\'';
        *out = r;
        return 1;
    } else {
        static const char *const lits[] = { "true", "false", "null" };
        for (size_t k = 0; k < 3; k++) {
            size_t len = strlen(lits[k]);
            if ((int64_t)len <= n - i && memcmp(p + i, lits[k], len) == 0) {
                *out = zell_alloc((int64_t)len + 1);
                memcpy(*out, lits[k], len);
                return 1;
            }
        }
        fail_char(ps, i);
        return 0;
    }
    size_t pl = strlen(prefix);
    char *r = zell_alloc((int64_t)pl + (e - s) + 1);
    memcpy(r, prefix, pl);
    memcpy(r + pl, p + s, (size_t)(e - s));
    *out = r;
    return 1;
}

/* "json: <what><token><tail>" for the token at i (or "json: unexpected end
 * of input" at the end). */
static void fail_grammar(zell__json_parser *ps, int64_t i, const char *what, const char *tail) {
    if (i >= ps->n) { fail_msg(ps, "json: unexpected end of input"); return; }
    char *tok;
    if (!describe_token(ps, i, &tok)) return;
    size_t len = 6 + strlen(what) + strlen(tok) + strlen(tail) + 1;
    char *msg = zell_alloc((int64_t)len);
    snprintf(msg, len, "json: %s%s%s", what, tok, tail);
    fail_msg(ps, msg);
    zell_free(msg);
    zell_free(tok);
}

/* Closes the innermost container: records its element count and span. */
static void close_container(zell__json_parser *ps) {
    uint32_t idx = ps->stack[--ps->depth];
    zell__json_node *nd = &ps->d->nodes[idx];
    nd->len = ps->count[ps->depth];
    nd->u.c.span = ps->d->n - idx;
}

static int open_container(zell__json_parser *ps, uint8_t type) {
    if (ps->depth == ZELL__JSON_MAX_DEPTH) { fail_msg(ps, "json: nesting too deep"); return 0; }
    zell__json_node *nd = doc_push(ps->d);
    nd->type = type;
    nd->len = 0;
    nd->u.c.span = 1;
    nd->u.c.table = 0;
    ps->stack[ps->depth] = ps->d->n - 1;
    ps->count[ps->depth] = 0;
    ps->depth++;
    return 1;
}

/* Scans an object key (a string) and its ':' at i; returns 0 on error. */
static int scan_key(zell__json_parser *ps) {
    int64_t i = skip_ws(ps->p, ps->i, ps->n);
    if (i >= ps->n) { fail_msg(ps, "json: unexpected end of input"); return 0; }
    if (ps->p[i] != '"') { fail_grammar(ps, i, "expected a string key in an object, found ", ""); return 0; }
    zell__json_node *key = doc_push(ps->d);
    if (!scan_string(ps, i, &i, key)) return 0;
    i = skip_ws(ps->p, i, ps->n);
    if (i >= ps->n) { fail_msg(ps, "json: unexpected end of input"); return 0; }
    if (ps->p[i] != ':') { fail_grammar(ps, i, "expected : after an object key, found ", ""); return 0; }
    ps->i = i + 1;
    return 1;
}

static int parse_document(zell__json_parser *ps) {
    const uint8_t *p = ps->p;
    int64_t n = ps->n;
    for (;;) {
        /* A value. */
        int64_t i = skip_ws(p, ps->i, n);
        if (i >= n) { fail_msg(ps, "json: unexpected end of input"); return 0; }
        uint8_t c = p[i];
        int opened = 0;
        switch (c) {
        case '{':
            if (!open_container(ps, ZELL__JSON_OBJ)) return 0;
            ps->i = i + 1;
            i = skip_ws(p, ps->i, n);
            if (i < n && p[i] == '}') { ps->i = i + 1; close_container(ps); break; }
            if (!scan_key(ps)) return 0;
            opened = 1;
            break;
        case '[':
            if (!open_container(ps, ZELL__JSON_ARR)) return 0;
            ps->i = i + 1;
            i = skip_ws(p, ps->i, n);
            if (i < n && p[i] == ']') { ps->i = i + 1; close_container(ps); break; }
            opened = 1;
            break;
        case '"': {
            zell__json_node *nd = doc_push(ps->d);
            if (!scan_string(ps, i, &ps->i, nd)) return 0;
            break;
        }
        case 't':
        case 'f':
        case 'n': {
            const char *lit = c == 't' ? "true" : c == 'f' ? "false" : "null";
            int64_t len = (int64_t)strlen(lit);
            if (len > n - i || memcmp(p + i, lit, (size_t)len) != 0) { fail_char(ps, i); return 0; }
            zell__json_node *nd = doc_push(ps->d);
            nd->type = c == 'n' ? ZELL__JSON_NULL : ZELL__JSON_BOOL;
            nd->len = c == 't';
            nd->u.i = c == 't';
            ps->i = i + len;
            break;
        }
        default:
            if (c == '-' || is_digit(c)) {
                zell__json_node *nd = doc_push(ps->d);
                if (!scan_number(ps, i, &ps->i, nd)) return 0;
                break;
            }
            if (c && strchr("}]:,", c)) { fail_grammar(ps, i, "unexpected ", ""); return 0; }
            fail_char(ps, i);
            return 0;
        }
        if (opened) continue; /* the first element follows */
        /* After a value: the separator or the end of the container. */
        for (;;) {
            if (ps->depth == 0) {
                i = skip_ws(p, ps->i, n);
                if (i < n) { fail_grammar(ps, i, "unexpected ", " after the top-level value"); return 0; }
                return 1;
            }
            ps->count[ps->depth - 1]++;
            int obj = ps->d->nodes[ps->stack[ps->depth - 1]].type == ZELL__JSON_OBJ;
            i = skip_ws(p, ps->i, n);
            if (i >= n) { fail_msg(ps, "json: unexpected end of input"); return 0; }
            c = p[i];
            if (c == ',') {
                ps->i = i + 1;
                if (obj && !scan_key(ps)) return 0;
                break; /* next value */
            }
            if (c == (obj ? '}' : ']')) {
                ps->i = i + 1;
                close_container(ps);
                continue;
            }
            fail_grammar(ps, i, obj ? "expected , or } in an object, found " : "expected , or ] in an array, found ", "");
            return 0;
        }
    }
}

int64_t zell_encoding_json_rt_parse(const zell_string *text, zell_error **err) {
    if (err) *err = NULL;
    int64_t n = zell_string_len(text);
    zell__json_doc *d = zell__json_doc_cache;
    if (d) {
        zell__json_doc_cache = NULL;
    } else {
        d = zell_alloc(sizeof *d);
        d->cap = (uint32_t)(n / 8 + 16);
        d->nodes = zell_realloc(NULL, (int64_t)d->cap * (int64_t)sizeof(zell__json_node));
    }
    d->text = (zell_string *)text;
    zell_retain(d->text);
    zell__json_parser ps;
    memset(&ps, 0, sizeof ps);
    ps.p = (const uint8_t *)zell_string_data(text);
    ps.n = n;
    ps.d = d;
    if (!parse_document(&ps)) {
        if (err) *err = ps.err;
        else zell_release(ps.err);
        doc_free(d);
        return 0;
    }
    return (int64_t)(intptr_t)d;
}

void zell_encoding_json_rt_free(int64_t h) {
    zell__json_doc *d = (zell__json_doc *)(intptr_t)h;
    if (!d) return;
    if (!zell__json_doc_cache && d->cap <= ZELL__JSON_DOC_CACHE_NODES) {
        doc_reset(d);
        zell__json_doc_cache = d;
        return;
    }
    doc_free(d);
}

int64_t zell_encoding_json_rt_root(int64_t d) {
    zell__json_doc *doc = (zell__json_doc *)(intptr_t)d;
    return doc && doc->n ? (int64_t)(intptr_t)doc->nodes : 0;
}

/* ---- node queries ----------------------------------------------------- */

#define NODE(h) ((zell__json_node *)(intptr_t)(h))

int64_t zell_encoding_json_rt_kind(int64_t n) { return n ? NODE(n)->type : ZELL__JSON_NULL; }

bool zell_encoding_json_rt_is_int(int64_t n) {
    return n && NODE(n)->type == ZELL__JSON_NUM && (NODE(n)->flags & ZELL__JSON_F_INT);
}

bool zell_encoding_json_rt_bool(int64_t n) { return n && NODE(n)->type == ZELL__JSON_BOOL && NODE(n)->len; }

int64_t zell_encoding_json_rt_int(int64_t n) {
    if (!n || NODE(n)->type != ZELL__JSON_NUM) return 0;
    return (NODE(n)->flags & ZELL__JSON_F_INT) ? NODE(n)->u.i : (int64_t)NODE(n)->u.f;
}

double zell_encoding_json_rt_float(int64_t n) {
    /* NaN for every other kind: JSON has no NaN, so one call both reads
     * and classifies a number node. */
    if (!n || NODE(n)->type != ZELL__JSON_NUM) return NAN;
    return (NODE(n)->flags & ZELL__JSON_F_INT) ? (double)NODE(n)->u.i : NODE(n)->u.f;
}

zell_string *zell_encoding_json_rt_str(int64_t n) {
    if (!n || NODE(n)->type != ZELL__JSON_STR || NODE(n)->len == 0) return NULL;
    return zell_string_new(NODE(n)->u.s, NODE(n)->len);
}

int64_t zell_encoding_json_rt_len(int64_t n) {
    return n && NODE(n)->type >= ZELL__JSON_ARR ? NODE(n)->len : 0;
}

int64_t zell_encoding_json_rt_first(int64_t n) {
    return n && NODE(n)->type >= ZELL__JSON_ARR && NODE(n)->len ? (int64_t)(intptr_t)(NODE(n) + 1) : 0;
}

int64_t zell_encoding_json_rt_next(int64_t n) { return n ? (int64_t)(intptr_t)node_next(NODE(n)) : 0; }

int64_t zell_encoding_json_rt_get(int64_t n, const zell_string *key) {
    if (!n || NODE(n)->type != ZELL__JSON_OBJ) return 0;
    const char *kd = zell_string_data(key);
    uint32_t kl = (uint32_t)zell_string_len(key);
    zell__json_node *k = NODE(n) + 1;
    zell__json_node *found = NULL;
    for (uint32_t i = 0, cnt = NODE(n)->len; i < cnt; i++) {
        if (k->len == kl && (kl == 0 || memcmp(k->u.s, kd, kl) == 0)) found = k + 1;
        k = node_next(k + 1);
    }
    return (int64_t)(intptr_t)found;
}

int64_t zell_encoding_json_rt_index(int64_t d, int64_t n, int64_t i) {
    if (!n || NODE(n)->type != ZELL__JSON_ARR || i < 0 || i >= (int64_t)NODE(n)->len) return 0;
    zell__json_node *arr = NODE(n);
    zell__json_doc *doc = (zell__json_doc *)(intptr_t)d;
    if (arr->len < ZELL__JSON_TABLE_MIN || !doc) {
        zell__json_node *e = arr + 1;
        for (int64_t k = 0; k < i; k++) e = node_next(e);
        return (int64_t)(intptr_t)e;
    }
    if (!(arr->flags & ZELL__JSON_F_TABLE)) {
        uint32_t *t = zell_alloc((int64_t)arr->len * (int64_t)sizeof(uint32_t));
        zell__json_node *e = arr + 1;
        for (uint32_t k = 0; k < arr->len; k++) {
            t[k] = (uint32_t)(e - arr);
            e = node_next(e);
        }
        if (doc->ntables == doc->captables) {
            doc->captables = doc->captables ? doc->captables * 2 : 4;
            doc->tables = zell_realloc(doc->tables, (int64_t)(doc->captables * sizeof(uint32_t *)));
        }
        doc->tables[doc->ntables] = t;
        arr->u.c.table = doc->ntables++;
        arr->flags |= ZELL__JSON_F_TABLE;
    }
    return (int64_t)(intptr_t)(arr + doc->tables[arr->u.c.table][i]);
}

/* ---- the writer ------------------------------------------------------- */

/* A small text is built in a raw buffer (reused by the next writer of the
 * thread; the result is a copy) and a large one directly in the string
 * object that is handed out (grown in place, no copy). */
#define ZELL__JSON_W_RAW_MAX 65536
typedef struct zell__json_w {
    char *buf;      /* the bytes: raw while s is NULL, else s->data */
    zell_string *s; /* the uniquely owned result once the text is large */
    int64_t len, cap;
} zell__json_w;

#define W(h) ((zell__json_w *)(intptr_t)(h))

static inline char *w_reserve(zell__json_w *w, int64_t extra) {
    if (w->len + extra > w->cap) {
        int64_t cap = w->cap * 2;
        if (cap < w->len + extra) cap = w->len + extra + 64;
        if (w->s) {
            w->s = (zell_string *)zell__object_resize(&w->s->hdr, (int64_t)sizeof(zell_string) + cap + 1);
            w->buf = w->s->data = (char *)(w->s + 1);
        } else if (cap <= ZELL__JSON_W_RAW_MAX) {
            w->buf = zell_realloc(w->buf, cap + 1);
        } else {
            w->s = zell__string_alloc_uninit(cap);
            memcpy(w->s->data, w->buf, (size_t)w->len);
            zell_free(w->buf);
            w->buf = w->s->data;
        }
        w->cap = cap;
    }
    return w->buf + w->len;
}

static inline void w_raw(zell__json_w *w, const char *p, int64_t n) {
    char *out = w_reserve(w, n);
    if (n <= 8) {
        for (int64_t i = 0; i < n; i++) out[i] = p[i];
    } else {
        memcpy(out, p, (size_t)n);
    }
    w->len += n;
}

static inline void w_byte(zell__json_w *w, char c) {
    w_reserve(w, 1)[0] = c;
    w->len++;
}

/* Writes `"` + s escaped + `"` (bytes needing an escape: '"', '\\' and
 * control characters), with `pre` in front when it is not 0 and `post`
 * after when it is not 0. The common case (no escapes) is one reservation
 * and one copy. */
static void w_quoted_ex(zell__json_w *w, const char *s, int64_t n, char pre, char post) {
    static const char hex[] = "0123456789abcdef";
    const uint8_t *p = (const uint8_t *)s;
    char *out = w_reserve(w, n + 4);
    char *o = out;
    if (pre) *o++ = pre;
    *o++ = '"';
    int64_t j = find_special(p, 0, n);
    memcpy(o, s, (size_t)j);
    o += j;
    w->len += o - out;
    int64_t i = j;
    while (i < n) {
        /* An escape: 6 bytes at most, then the plain run after it. */
        uint8_t c = p[i];
        i++;
        j = find_special(p, i, n);
        out = w_reserve(w, 6 + (j - i) + 2);
        o = out;
        *o++ = '\\';
        switch (c) {
        case '"': *o++ = '"'; break;
        case '\\': *o++ = '\\'; break;
        case '\n': *o++ = 'n'; break;
        case '\r': *o++ = 'r'; break;
        case '\t': *o++ = 't'; break;
        case '\b': *o++ = 'b'; break;
        case '\f': *o++ = 'f'; break;
        default:
            *o++ = 'u';
            *o++ = '0';
            *o++ = '0';
            *o++ = hex[c >> 4];
            *o++ = hex[c & 15];
        }
        memcpy(o, s + i, (size_t)(j - i));
        o += j - i;
        w->len += o - out;
        i = j;
    }
    out = w_reserve(w, 2);
    o = out;
    *o++ = '"';
    if (post) *o++ = post;
    w->len += o - out;
}

static void w_quoted(zell__json_w *w, const char *s, int64_t n) { w_quoted_ex(w, s, n, 0, 0); }

static void w_int(zell__json_w *w, int64_t v) {
    char buf[24];
    char *e = buf + sizeof buf, *p = e;
    uint64_t u = v < 0 ? 0 - (uint64_t)v : (uint64_t)v;
    do { *--p = (char)('0' + u % 10); u /= 10; } while (u);
    if (v < 0) *--p = '-';
    w_raw(w, p, e - p);
}

static void w_float(zell__json_w *w, double f) {
    if (isnan(f) || isinf(f)) { w_raw(w, "null", 4); return; }
    char buf[64];
    int n = zell__format_float(buf, sizeof buf, f);
    w_raw(w, buf, n);
}

static void w_node(zell__json_w *w, const zell__json_node *nd) {
    if (!nd) { w_raw(w, "null", 4); return; }
    switch (nd->type) {
    case ZELL__JSON_NULL: w_raw(w, "null", 4); break;
    case ZELL__JSON_BOOL: if (nd->len) w_raw(w, "true", 4); else w_raw(w, "false", 5); break;
    case ZELL__JSON_NUM:
        if (nd->flags & ZELL__JSON_F_INT) w_int(w, nd->u.i);
        else w_float(w, nd->u.f);
        break;
    case ZELL__JSON_STR: w_quoted(w, nd->u.s, nd->len); break;
    case ZELL__JSON_ARR: {
        w_byte(w, '[');
        const zell__json_node *e = nd + 1;
        for (uint32_t i = 0; i < nd->len; i++) {
            if (i) w_byte(w, ',');
            w_node(w, e);
            e += node_span(e);
        }
        w_byte(w, ']');
        break;
    }
    default: {
        w_byte(w, '{');
        const zell__json_node *k = nd + 1;
        for (uint32_t i = 0; i < nd->len; i++) {
            if (i) w_byte(w, ',');
            w_quoted(w, k->u.s, k->len);
            w_byte(w, ':');
            w_node(w, k + 1);
            k += 1 + node_span(k + 1);
        }
        w_byte(w, '}');
    }
    }
}

/* One idle writer (with its raw buffer) per thread: a Format of a small
 * text then makes one allocation, the result. Raw memory only, so the
 * cache never shows up as a live object. */
static _Thread_local zell__json_w *zell__json_w_cache;

int64_t zell_encoding_json_rt_w_new(void) {
    zell__json_w *w = zell__json_w_cache;
    if (w) {
        zell__json_w_cache = NULL;
        w->len = 0;
        return (int64_t)(intptr_t)w;
    }
    w = zell_alloc(sizeof *w);
    w->cap = 256;
    w->buf = zell_realloc(NULL, w->cap + 1);
    return (int64_t)(intptr_t)w;
}

zell_string *zell_encoding_json_rt_w_finish(int64_t h) {
    zell__json_w *w = W(h);
    if (!w->s) {
        zell_string *r = zell_string_new(w->buf, w->len);
        if (!zell__json_w_cache) {
            zell__json_w_cache = w;
        } else {
            zell_free(w->buf);
            zell_free(w);
        }
        return r;
    }
    zell_string *s = w->s;
    s->len = w->len;
    s->data[w->len] = ZELL__STR_END;
    zell_free(w);
    return s;
}

void zell_encoding_json_rt_w_raw(int64_t h, const zell_string *s) { w_raw(W(h), zell_string_data(s), zell_string_len(s)); }

void zell_encoding_json_rt_w_key(int64_t h, const zell_string *name, bool first) {
    w_quoted_ex(W(h), zell_string_data(name), zell_string_len(name), first ? 0 : ',', ':');
}

/* Key and scalar value in one call (the typed encoder's struct fields). */
void zell_encoding_json_rt_w_key_int(int64_t h, const zell_string *name, bool first, int64_t v) {
    w_quoted_ex(W(h), zell_string_data(name), zell_string_len(name), first ? 0 : ',', ':');
    w_int(W(h), v);
}

void zell_encoding_json_rt_w_key_str(int64_t h, const zell_string *name, bool first, const zell_string *s) {
    w_quoted_ex(W(h), zell_string_data(name), zell_string_len(name), first ? 0 : ',', ':');
    w_quoted(W(h), zell_string_data(s), zell_string_len(s));
}

void zell_encoding_json_rt_w_key_float(int64_t h, const zell_string *name, bool first, double f) {
    w_quoted_ex(W(h), zell_string_data(name), zell_string_len(name), first ? 0 : ',', ':');
    w_float(W(h), f);
}

void zell_encoding_json_rt_w_key_bool(int64_t h, const zell_string *name, bool first, bool b) {
    w_quoted_ex(W(h), zell_string_data(name), zell_string_len(name), first ? 0 : ',', ':');
    if (b) w_raw(W(h), "true", 4);
    else w_raw(W(h), "false", 5);
}

/* A []string as a JSON array of strings (the caller writes null for nil). */
void zell_encoding_json_rt_w_strs(int64_t h, const zell_slice *s) {
    zell__json_w *w = W(h);
    w_byte(w, '[');
    const zell_string *const *p = s->data;
    for (int64_t i = 0; i < s->len; i++) {
        if (i) w_byte(w, ',');
        w_quoted(w, zell_string_data(p[i]), zell_string_len(p[i]));
    }
    w_byte(w, ']');
}

void zell_encoding_json_rt_w_str(int64_t h, const zell_string *s) { w_quoted(W(h), zell_string_data(s), zell_string_len(s)); }

void zell_encoding_json_rt_w_int(int64_t h, int64_t v) { w_int(W(h), v); }

void zell_encoding_json_rt_w_uint(int64_t h, uint64_t v) {
    char buf[24];
    char *e = buf + sizeof buf, *p = e;
    do { *--p = (char)('0' + v % 10); v /= 10; } while (v);
    w_raw(W(h), p, e - p);
}

void zell_encoding_json_rt_w_float(int64_t h, double f) { w_float(W(h), f); }

void zell_encoding_json_rt_w_bool(int64_t h, bool b) {
    if (b) w_raw(W(h), "true", 4);
    else w_raw(W(h), "false", 5);
}

void zell_encoding_json_rt_w_node(int64_t h, int64_t n) { w_node(W(h), NODE(n)); }

zell_string *zell_encoding_json_quote(const zell_string *s) {
    int64_t h = zell_encoding_json_rt_w_new();
    w_quoted(W(h), zell_string_data(s), zell_string_len(s));
    return zell_encoding_json_rt_w_finish(h);
}
