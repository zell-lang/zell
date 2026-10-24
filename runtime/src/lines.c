/* lines.c - lookups in the `.zell_lines` table of panic backtraces
 * (ABI.md 8.2): code address -> source file, line and Zell function.
 * The platform layer finds the table (posix.c: the executable's ELF
 * section) and calls zell__lines_lookup. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <stdlib.h>
#include <string.h>

#define ZELL__LINES_HDR 32
#define ZELL__LINES_NOFILE 0xfffu

static uint32_t rd32(const unsigned char *p) {
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

/* The last entry of `n` 8-byte {off, value} entries at `e` with off <= x,
 * or -1. */
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

bool zell__lines_lookup(const unsigned char *t, size_t len, uint64_t vaddr, const char **file, uint32_t *line, const char **func) {
    if (!t || len < ZELL__LINES_HDR || memcmp(t, "ZLN1", 4) != 0) return false;
    uint32_t nfiles = rd32(t + 4), nlines = rd32(t + 8), nfuncs = rd32(t + 12), strsize = rd32(t + 16);
    uint64_t base = (uint64_t)rd32(t + 24) | (uint64_t)rd32(t + 28) << 32;
    size_t files = ZELL__LINES_HDR, lines = files + (size_t)nfiles * 4, funcs = lines + (size_t)nlines * 8, str = funcs + (size_t)nfuncs * 12;
    if (str + strsize > len || strsize == 0 || t[str + strsize - 1] != 0) return false;
    if (vaddr < base || vaddr - base > UINT32_MAX) return false;
    uint32_t x = (uint32_t)(vaddr - base);
    long i = find(t + lines, nlines, x);
    if (i < 0) return false;
    uint32_t fl = rd32(t + lines + (size_t)i * 8 + 4), f = fl >> 20;
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
