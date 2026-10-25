/* lines.c - the line table of panic backtraces (ABI.md 8.2): code address
 * -> source file, line and Zell function.
 *
 * The compiler links one extra object into every program; its constructor
 * hands the table to zell_lines_register. Code addresses in a "ZLN2" table
 * are 32-bit offsets relative to the table itself, resolved by the static
 * linker (ELF, COFF, arm64 Mach-O); a "ZLN3" table (x86_64 Mach-O, whose
 * relocations cannot express that offset) holds absolute 64-bit pointers,
 * rebased by dyld. Either way they are already run-time addresses: no load
 * bias, no file reading, nothing platform-specific. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <stdlib.h>
#include <string.h>

#define ZELL__LINES_HDR 24
#define ZELL__LINES_NOFILE 0xfffu

static uint32_t rd32(const unsigned char *p) {
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

/* The registered table (one per program). */
static const unsigned char *g_tab;
static size_t g_len;
static size_t g_stride;  /* code entry size: 12 (ZLN2) or 16 (ZLN3) */

void zell_lines_register(const void *table, int64_t len) {
    if (!table || len < ZELL__LINES_HDR) return;
    if (memcmp(table, "ZLN2", 4) == 0) g_stride = 12;
    else if (memcmp(table, "ZLN3", 4) == 0) g_stride = 16;
    else return;
    g_tab = table;
    g_len = (size_t)len;
}

/* The run-time address of code entry `e`. */
static uintptr_t code_addr(const unsigned char *e) {
    if (g_stride == 16) {
        uint64_t a;
        memcpy(&a, e, 8);  /* a pointer of this process: native byte order */
        return (uintptr_t)a;
    }
    return (uintptr_t)e + (uintptr_t)(intptr_t)(int32_t)rd32(e);
}

/* The code ranges sorted by run-time address, built on first use. */
typedef struct code_range {
    uintptr_t lo;
    uint32_t size, k;
} code_range;
static code_range *g_code;
static uint32_t g_ncode;
static int g_state;  /* 0 not built, 1 building, 2 built */

static int code_cmp(const void *a, const void *b) {
    uintptr_t x = ((const code_range *)a)->lo, y = ((const code_range *)b)->lo;
    return x < y ? -1 : x > y;
}

static void build_index(void) {
    const unsigned char *t = g_tab;
    uint32_t ncode = rd32(t + 20);
    if ((size_t)ZELL__LINES_HDR + (size_t)ncode * g_stride > g_len) return;
    code_range *c = malloc(sizeof *c * (ncode ? ncode : 1));
    if (!c) return;
    for (uint32_t k = 0; k < ncode; k++) {
        const unsigned char *e = t + ZELL__LINES_HDR + (size_t)k * g_stride;
        c[k].lo = code_addr(e);
        c[k].size = rd32(e + g_stride - 8);
        c[k].k = k;
    }
    qsort(c, ncode, sizeof *c, code_cmp);
    g_code = c;
    g_ncode = ncode;
}

/* The last of `n` 8-byte {off, value} entries at `e` with off <= x, or -1. */
static long find(const unsigned char *e, uint32_t n, uint32_t x) {
    if (n == 0 || rd32(e) > x) return -1;
    uint32_t lo = 0, hi = n;
    while (hi - lo > 1) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (rd32(e + (size_t)mid * 8) <= x) lo = mid;
        else hi = mid;
    }
    return (long)lo;
}

bool zell__lines_find(uintptr_t pc, const char **file, uint32_t *line, const char **func) {
    if (!g_tab || getenv("ZELL_NO_LINES")) return false;
    int st = __atomic_load_n(&g_state, __ATOMIC_ACQUIRE);
    if (st == 0) {
        int expect = 0;
        if (__atomic_compare_exchange_n(&g_state, &expect, 1, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            build_index();
            __atomic_store_n(&g_state, 2, __ATOMIC_RELEASE);
            st = 2;
        }
    }
    if (st != 2 || !g_code) return false;  /* another thread is building it: symbol+offset */
    const unsigned char *t = g_tab;
    uint32_t nfiles = rd32(t + 4), nlines = rd32(t + 8), nfuncs = rd32(t + 12), strsize = rd32(t + 16), ncode = rd32(t + 20);
    size_t files = ZELL__LINES_HDR + (size_t)ncode * g_stride, lines = files + (size_t)nfiles * 4, funcs = lines + (size_t)nlines * 8,
           str = funcs + (size_t)nfuncs * 12;
    if (str + strsize > g_len || strsize == 0 || t[str + strsize - 1] != 0) return false;
    /* the code range holding pc */
    uint32_t lo = 0, hi = g_ncode;
    if (hi == 0 || pc < g_code[0].lo) return false;
    while (hi - lo > 1) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (g_code[mid].lo <= pc) lo = mid;
        else hi = mid;
    }
    const code_range *c = &g_code[lo];
    if (pc - c->lo >= c->size) return false;
    const unsigned char *ce = t + ZELL__LINES_HDR + (size_t)c->k * g_stride;
    uint32_t first = rd32(ce + g_stride - 4), end = c->k + 1 < ncode ? rd32(ce + 2 * g_stride - 4) : nlines;
    if (first > end || end > nlines) return false;
    long i = find(t + lines + (size_t)first * 8, end - first, (uint32_t)(pc - c->lo));
    if (i < 0) return false;
    uint32_t fl = rd32(t + lines + ((size_t)first + (size_t)i) * 8 + 4), f = fl >> 20;
    if (f == ZELL__LINES_NOFILE || f >= nfiles) return false;
    uint32_t fo = rd32(t + files + (size_t)f * 4);
    if (fo >= strsize) return false;
    *file = (const char *)t + str + fo;
    *line = fl & 0xfffffu;
    *func = NULL;
    /* the innermost function whose source range holds the line */
    uint32_t best = UINT32_MAX;
    for (uint32_t k = 0; k < nfuncs; k++) {
        const unsigned char *e = t + funcs + (size_t)k * 12;
        uint32_t ffl = rd32(e), last = rd32(e + 4), no = rd32(e + 8);
        if (ffl >> 20 != f || (ffl & 0xfffffu) > *line || last < *line || no >= strsize) continue;
        if (last - (ffl & 0xfffffu) < best) {
            best = last - (ffl & 0xfffffu);
            *func = (const char *)t + str + no;
        }
    }
    return true;
}

/* The working directory, read once (paths print relative to it, like
 * error traces: error.c site_file). */
static char *g_cwd;
static int g_cwd_state;

const char *zell__lines_shown(const char *file) {
    int st = __atomic_load_n(&g_cwd_state, __ATOMIC_ACQUIRE);
    if (st == 0) {
        int expect = 0;
        if (__atomic_compare_exchange_n(&g_cwd_state, &expect, 1, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            int err = 0;
            g_cwd = zell__plat_getcwd(&err);
            __atomic_store_n(&g_cwd_state, 2, __ATOMIC_RELEASE);
            st = 2;
        }
    }
    if (st != 2 || !g_cwd) return file;
    size_t n = strlen(g_cwd);
    if (n > 1 && strncmp(file, g_cwd, n) == 0 && (file[n] == '/' || file[n] == '\\') && file[n + 1]) return file + n + 1;
    return file;
}
