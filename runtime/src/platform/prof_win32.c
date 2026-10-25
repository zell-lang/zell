/* prof_win32.c - Windows diagnostics for the platform layer (platform.h):
 * backtraces with symbol names, the CPU-profile sampler and the symbolizer
 * used by profile.c.
 *
 * Symbols: the executable's own COFF symbol table (lld and ld write it for
 * mingw builds unless stripped: every function, local ones included, like
 * the ELF .symtab that posix.c reads), then DbgHelp's SymFromAddr (loaded
 * on demand; it knows exports and PDBs of system DLLs, and under Wine also
 * DWARF). A `--release` build is stripped, so it shows module+offset, as on
 * Linux.
 *
 * CPU sampler: there is no SIGPROF on Windows, so a sampler thread wakes
 * `hz` times a second (a high-resolution waitable timer), measures how much
 * CPU every thread of the process used since the last tick
 * (GetThreadTimes) and takes one sample per 1/hz CPU seconds consumed:
 * SuspendThread, GetThreadContext, a stack walk with RtlVirtualUnwind over
 * the .pdata unwind tables (the frame-pointer chain of Win64 code is not
 * reliable: clang sets rbp at an offset), ResumeThread. Samples go to the
 * same `fn` as on POSIX (lock-free, so calling it from the sampler is
 * fine). The thread list is refreshed with a Toolhelp snapshot every 10
 * ticks. While a thread is suspended the sampler neither allocates nor
 * takes locks; the unwinder may take the loader's function-table lock,
 * which only matters for code that registers dynamic function tables (JITs;
 * Zell has none). */
#include "platform.h"

#if defined(_WIN32)

#include "win32_util.h"

#include <tlhelp32.h>
#include <winternl.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- stack bounds ------------------------------------------------------------ */

/* `t_stack_lo` is kept for the debugger formatters (not read here). */
static _Thread_local uintptr_t __attribute__((unused)) t_stack_lo;
static _Thread_local uintptr_t t_stack_hi;

void zell__plat_thread_stack_init(void) {
    zell__w32_fatal_thread_init();
    if (t_stack_hi) return;
    ULONG_PTR lo = 0, hi = 0;
    GetCurrentThreadStackLimits(&lo, &hi);
    t_stack_lo = lo;
    t_stack_hi = hi;
}

/* ---- the executable's COFF symbol table ---------------------------------- */

typedef struct sym_ent {
    uintptr_t addr;
    uint32_t name;  /* offset in g_names */
} sym_ent;

static zell__plat_mutex g_sym_mu = ZELL_PLAT_MUTEX_INIT;
static int g_syms_loaded;
static sym_ent *g_syms;
static size_t g_nsyms;
static char *g_names;
static uintptr_t g_text_hi;

static int sym_cmp(const void *a, const void *b) {
    uintptr_t x = ((const sym_ent *)a)->addr, y = ((const sym_ent *)b)->addr;
    return x < y ? -1 : x > y;
}

/* Reads the COFF symbol table of the running image from its file (the
 * loader does not map it). */
static void load_syms(void) {
    g_syms_loaded = 1;
    HMODULE m = GetModuleHandleW(NULL);
    wchar_t path[MAX_PATH];
    if (!m || !GetModuleFileNameW(NULL, path, MAX_PATH)) return;
    HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_DELETE, NULL, OPEN_EXISTING, 0, NULL);
    if (f == INVALID_HANDLE_VALUE) return;
    LARGE_INTEGER fsz;
    if (!GetFileSizeEx(f, &fsz)) fsz.QuadPart = 0;
    HANDLE map = CreateFileMappingW(f, NULL, PAGE_READONLY, 0, 0, NULL);
    CloseHandle(f);
    if (!map) return;
    const unsigned char *base = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0);
    CloseHandle(map);
    if (!base) return;
    size_t size = (size_t)fsz.QuadPart;
    const IMAGE_DOS_HEADER *dos = (const IMAGE_DOS_HEADER *)base;
    if (size < sizeof *dos || dos->e_magic != IMAGE_DOS_SIGNATURE || (size_t)dos->e_lfanew + sizeof(IMAGE_NT_HEADERS) > size) goto done;
    const IMAGE_NT_HEADERS *nt = (const IMAGE_NT_HEADERS *)(base + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE) goto done;
    const IMAGE_FILE_HEADER *fh = &nt->FileHeader;
    const IMAGE_SECTION_HEADER *sec = IMAGE_FIRST_SECTION(nt);
    size_t nsec = fh->NumberOfSections;
    for (size_t i = 0; i < nsec; i++)
        if (sec[i].Characteristics & IMAGE_SCN_MEM_EXECUTE) {
            uintptr_t hi = (uintptr_t)m + sec[i].VirtualAddress + sec[i].Misc.VirtualSize;
            if (hi > g_text_hi) g_text_hi = hi;
        }
    size_t symoff = fh->PointerToSymbolTable, nsym = fh->NumberOfSymbols;
    if (!symoff || !nsym || symoff + nsym * 18 + 4 > size) goto done;
    const unsigned char *st = base + symoff;
    const unsigned char *strtab = st + nsym * 18;
    uint32_t strsz;
    memcpy(&strsz, strtab, 4);
    if (symoff + nsym * 18 + strsz > size) strsz = 4;
    /* first pass: count, sizes of names */
    size_t cnt = 0, namebytes = 0;
    for (size_t i = 0; i < nsym; i++) {
        const unsigned char *r = st + i * 18;
        int16_t secno;
        uint16_t type;
        memcpy(&secno, r + 12, 2);
        memcpy(&type, r + 14, 2);
        uint8_t naux = r[17];
        if (secno > 0 && (size_t)secno <= nsec && (type >> 4) == 2 /* function */ &&
            (sec[secno - 1].Characteristics & IMAGE_SCN_MEM_EXECUTE)) {
            cnt++;
            uint32_t z;
            memcpy(&z, r, 4);
            if (z == 0) {
                uint32_t off;
                memcpy(&off, r + 4, 4);
                if (off < strsz) namebytes += strnlen((const char *)strtab + off, strsz - off) + 1;
            } else {
                namebytes += 9;
            }
        }
        i += naux;
    }
    if (!cnt) goto done;
    g_syms = malloc(cnt * sizeof *g_syms);
    g_names = malloc(namebytes + 1);
    if (!g_syms || !g_names) {
        free(g_syms);
        free(g_names);
        g_syms = NULL;
        g_names = NULL;
        goto done;
    }
    size_t k = 0, nb = 0;
    for (size_t i = 0; i < nsym && k < cnt; i++) {
        const unsigned char *r = st + i * 18;
        int16_t secno;
        uint16_t type;
        uint32_t value;
        memcpy(&value, r + 8, 4);
        memcpy(&secno, r + 12, 2);
        memcpy(&type, r + 14, 2);
        uint8_t naux = r[17];
        if (secno > 0 && (size_t)secno <= nsec && (type >> 4) == 2 && (sec[secno - 1].Characteristics & IMAGE_SCN_MEM_EXECUTE)) {
            uint32_t z;
            memcpy(&z, r, 4);
            const char *name;
            size_t len;
            if (z == 0) {
                uint32_t off;
                memcpy(&off, r + 4, 4);
                name = off < strsz ? (const char *)strtab + off : "";
                len = off < strsz ? strnlen(name, strsz - off) : 0;
            } else {
                name = (const char *)r;
                len = strnlen(name, 8);
            }
            g_syms[k].addr = (uintptr_t)m + sec[secno - 1].VirtualAddress + value;
            g_syms[k].name = (uint32_t)nb;
            memcpy(g_names + nb, name, len);
            g_names[nb + len] = 0;
            nb += len + 1;
            k++;
        }
        i += naux;
    }
    g_nsyms = k;
    qsort(g_syms, g_nsyms, sizeof *g_syms, sym_cmp);
done:
    UnmapViewOfFile(base);
}

/* ---- DbgHelp (loaded on demand) ---------------------------------------------- */

typedef BOOL(WINAPI *sym_init_fn)(HANDLE, PCSTR, BOOL);
typedef DWORD(WINAPI *sym_setopt_fn)(DWORD);
typedef struct dbg_symbol {  /* SYMBOL_INFO */
    ULONG SizeOfStruct, TypeIndex;
    ULONG64 Reserved[2];
    ULONG Index, Size;
    ULONG64 ModBase;
    ULONG Flags;
    ULONG64 Value, Address;
    ULONG Register, Scope, Tag, NameLen, MaxNameLen;
    CHAR Name[1];
} dbg_symbol;
typedef BOOL(WINAPI *sym_from_addr_fn)(HANDLE, DWORD64, PDWORD64, dbg_symbol *);
typedef BOOL(WINAPI *sym_cleanup_fn)(HANDLE);

static int g_dbghelp_state;  /* 0 = not tried, 1 = ready, -1 = unavailable */
static sym_from_addr_fn g_sym_from_addr;
static sym_cleanup_fn g_sym_cleanup;

static bool dbghelp_lookup(uintptr_t pc, char *buf, size_t cap, uintptr_t *start) {
    if (g_dbghelp_state == 0) {
        g_dbghelp_state = -1;
        HMODULE h = LoadLibraryW(L"dbghelp.dll");
        if (h) {
            sym_init_fn init = (sym_init_fn)(void *)GetProcAddress(h, "SymInitialize");
            sym_setopt_fn setopt = (sym_setopt_fn)(void *)GetProcAddress(h, "SymSetOptions");
            g_sym_from_addr = (sym_from_addr_fn)(void *)GetProcAddress(h, "SymFromAddr");
            g_sym_cleanup = (sym_cleanup_fn)(void *)GetProcAddress(h, "SymCleanup");
            if (setopt) setopt(0x00000002 /* SYMOPT_UNDNAME */ | 0x00000004 /* SYMOPT_DEFERRED_LOADS */);
            if (init && g_sym_from_addr && init(GetCurrentProcess(), NULL, TRUE)) g_dbghelp_state = 1;
        }
    }
    if (g_dbghelp_state != 1) return false;
    union {
        dbg_symbol s;
        char space[sizeof(dbg_symbol) + 512];
    } u;
    memset(&u.s, 0, sizeof u.s);
    u.s.SizeOfStruct = (ULONG)sizeof(dbg_symbol);
    u.s.MaxNameLen = 511;
    DWORD64 disp = 0;
    if (!g_sym_from_addr(GetCurrentProcess(), (DWORD64)pc, &disp, &u.s) || !u.s.Name[0]) return false;
    snprintf(buf, cap, "%s", u.s.Name);
    *start = (uintptr_t)u.s.Address;
    return true;
}

static bool symbolize_locked(uintptr_t pc, char *buf, size_t cap, uintptr_t *start) {
    if (!g_syms_loaded) load_syms();
    if (g_nsyms && pc >= g_syms[0].addr && pc < g_text_hi) {
        size_t lo = 0, hi = g_nsyms;
        while (hi - lo > 1) {
            size_t mid = (lo + hi) / 2;
            if (g_syms[mid].addr <= pc) lo = mid;
            else hi = mid;
        }
        snprintf(buf, cap, "%s", zell__plat_sym_clean(g_names + g_syms[lo].name));
        *start = g_syms[lo].addr;
        return true;
    }
    return dbghelp_lookup(pc, buf, cap, start);
}

bool zell__plat_symbolize(uintptr_t pc, char *buf, size_t cap, uintptr_t *start) {
    zell__plat_mutex_lock(&g_sym_mu);
    bool ok = symbolize_locked(pc, buf, cap, start);
    zell__plat_mutex_unlock(&g_sym_mu);
    return ok;
}

bool zell__plat_exe_range(uintptr_t *lo, uintptr_t *hi, char *path, size_t cap) {
    HMODULE m = GetModuleHandleW(NULL);
    if (!m) return false;
    const IMAGE_DOS_HEADER *dos = (const IMAGE_DOS_HEADER *)m;
    const IMAGE_NT_HEADERS *nt = (const IMAGE_NT_HEADERS *)((const char *)m + dos->e_lfanew);
    *lo = (uintptr_t)m;
    *hi = (uintptr_t)m + nt->OptionalHeader.SizeOfImage;
    wchar_t w[MAX_PATH];
    char *u = GetModuleFileNameW(NULL, w, MAX_PATH) ? zell__w32_utf8(w, -1) : NULL;
    snprintf(path, cap, "%s", u ? u : "");
    free(u);
    return true;
}

void zell__plat_symbolize_done(void) {
    zell__plat_mutex_lock(&g_sym_mu);
    free(g_syms);
    free(g_names);
    g_syms = NULL;
    g_names = NULL;
    g_nsyms = 0;
    g_syms_loaded = 0;
    g_text_hi = 0;
    if (g_dbghelp_state == 1 && g_sym_cleanup) g_sym_cleanup(GetCurrentProcess());
    g_dbghelp_state = 0;
    zell__plat_mutex_unlock(&g_sym_mu);
}

/* ---- backtraces ------------------------------------------------------------------ */

/* One frame: "  file:line function" from the line table (ABI.md 8.2) for
 * Zell code, else glibc's backtrace_symbols_fd format module(function+0xoff)
 * [0xaddr], or module(+0xoff) [0xaddr] without a symbol. */
static int bt_format(uintptr_t pc, char *line, size_t cap) {
    char sym[400];
    uintptr_t start = 0;
    const char *file, *func;
    uint32_t ln;
    /* return addresses point after the call: look up pc - 1 */
    if (zell__lines_find(pc - 1, &file, &ln, &func)) {
        if (!func) func = zell__plat_symbolize(pc - 1, sym, sizeof sym, &start) ? sym : "?";
        return snprintf(line, cap, "  %s:%u %s\n", zell__lines_shown(file), (unsigned)ln, func);
    }
    HMODULE mod = NULL;
    wchar_t wpath[MAX_PATH];
    const char *name = "?";
    char *u = NULL;
    if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, (LPCWSTR)pc, &mod) &&
        GetModuleFileNameW(mod, wpath, MAX_PATH)) {
        u = zell__w32_utf8(wpath, -1);
        if (u) {
            char *b = strrchr(u, '\\');
            name = b ? b + 1 : u;
        }
    }
    int k;
    bool named = !getenv("ZELL_NO_SYMBOLS") && zell__plat_symbolize(pc - 1, sym, sizeof sym, &start) && start <= pc;
    /* The runtime's panic entry points are not the program's frames. */
    if (named && (strncmp(sym, "zell_panic", 10) == 0 || strncmp(sym, "zell__panic", 11) == 0)) {
        free(u);
        return 0;
    }
    if (named)
        k = snprintf(line, cap, "%s(%s+0x%llx) [0x%llx]\n", name, sym, (unsigned long long)(pc - start), (unsigned long long)pc);
    else
        k = snprintf(line, cap, "%s(+0x%llx) [0x%llx]\n", name, (unsigned long long)(pc - (uintptr_t)mod), (unsigned long long)pc);
    free(u);
    return k;
}

void zell__plat_backtrace(int fd, int skip) {
    void *frames[62];
    USHORT n = CaptureStackBackTrace((DWORD)(skip > 0 ? skip : 0), 62, frames, NULL);
    for (USHORT i = 0; i < n && frames[i]; i++) {
        char line[800];
        int k = bt_format((uintptr_t)frames[i], line, sizeof line);
        if (k > 0) zell__plat_write_all(fd, line, k < (int)sizeof line ? k : (int)sizeof line - 1);
    }
}

void zell__plat_backtrace_buf(char *buf, size_t cap, int skip) {
    void *frames[62];
    size_t len = 0;
    if (cap) buf[0] = 0;
    USHORT n = CaptureStackBackTrace((DWORD)(skip > 0 ? skip : 0), 62, frames, NULL);
    for (USHORT i = 0; i < n && frames[i]; i++) {
        char line[800];
        int k = bt_format((uintptr_t)frames[i], line, sizeof line);
        if (k <= 0) continue;
        if (k >= (int)sizeof line) k = (int)sizeof line - 1;
        if (len + (size_t)k >= cap) break;
        memcpy(buf + len, line, (size_t)k);
        len += (size_t)k;
        buf[len] = 0;
    }
}

void zell__plat_backtrace_pcs(int fd, const uintptr_t *pcs, int n) {
    for (int i = 0; i < n; i++) {
        char line[800];
        int k = bt_format(pcs[i], line, sizeof line);
        if (k > 0) zell__plat_write_all(fd, line, k < (int)sizeof line ? k : (int)sizeof line - 1);
    }
}

int zell__w32_walk(void *ctx_, uintptr_t lo, uintptr_t hi, uintptr_t *pcs, int max) {
    CONTEXT *ctx = ctx_;
    int n = 0;
    if (max <= 0) return 0;
#if defined(_M_X64) || defined(__x86_64__)
    pcs[n++] = (uintptr_t)ctx->Rip;
    while (n < max) {
        DWORD64 image = 0;
        PRUNTIME_FUNCTION fe = RtlLookupFunctionEntry(ctx->Rip, &image, NULL);
        if (fe) {
            PVOID handler_data = NULL;
            DWORD64 frame = 0;
            RtlVirtualUnwind(UNW_FLAG_NHANDLER, image, ctx->Rip, fe, ctx, &handler_data, &frame, NULL);
        } else {
            /* a leaf function: the return address is at rsp */
            if (ctx->Rsp < lo || ctx->Rsp + 8 > hi) break;
            ctx->Rip = *(const DWORD64 *)(uintptr_t)ctx->Rsp;
            ctx->Rsp += 8;
        }
        if (!ctx->Rip || ctx->Rsp < lo || ctx->Rsp >= hi) break;
        pcs[n++] = (uintptr_t)ctx->Rip;
    }
#elif defined(_M_ARM64) || defined(__aarch64__)
    pcs[n++] = (uintptr_t)ctx->Pc;
    while (n < max) {
        DWORD64 image = 0;
        PRUNTIME_FUNCTION fe = RtlLookupFunctionEntry(ctx->Pc, &image, NULL);
        if (!fe) break;
        PVOID handler_data = NULL;
        DWORD64 frame = 0;
        RtlVirtualUnwind(UNW_FLAG_NHANDLER, image, ctx->Pc, fe, ctx, &handler_data, &frame, NULL);
        if (!ctx->Pc || ctx->Sp < lo || ctx->Sp >= hi) break;
        pcs[n++] = (uintptr_t)ctx->Pc;
    }
#else
    (void)lo; (void)hi;
    n = 0;
#endif
    return n;
}

/* ---- the CPU sampler -------------------------------------------------------------- */

#define PROF_MAX_FRAMES 64
#define PROF_MAX_THREADS 512

typedef struct prof_thread {
    DWORD tid;
    HANDLE h;
    uint64_t cpu;      /* user + kernel time at the last tick, 100 ns */
    uint64_t acc;      /* CPU time not yet turned into samples */
    int seen;          /* still present in the latest snapshot */
} prof_thread;

typedef struct thread_basic_info {
    NTSTATUS ExitStatus;
    PVOID TebBaseAddress;
    struct { HANDLE p, t; } ClientId;
    ULONG_PTR AffinityMask;
    LONG Priority, BasePriority;
} thread_basic_info;
typedef NTSTATUS(NTAPI *nt_qit_fn)(HANDLE, int, PVOID, ULONG, PULONG);

static zell__plat_prof_fn g_prof_fn;
static HANDLE g_prof_thread, g_prof_stop_ev;
static int g_prof_hz;
static volatile LONG g_prof_on;
static prof_thread g_threads[PROF_MAX_THREADS];
static int g_nthreads;
static DWORD g_prof_tid;
static nt_qit_fn g_nt_qit;

static uint64_t ft64(FILETIME f) { return ((uint64_t)f.dwHighDateTime << 32) | f.dwLowDateTime; }

static uint64_t thread_cpu(HANDLE h) {
    FILETIME c, e, k, u;
    if (!GetThreadTimes(h, &c, &e, &k, &u)) return 0;
    return ft64(k) + ft64(u);
}

static void refresh_threads(void) {
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if (snap == INVALID_HANDLE_VALUE) return;
    DWORD pid = GetCurrentProcessId();
    for (int i = 0; i < g_nthreads; i++) g_threads[i].seen = 0;
    THREADENTRY32 te;
    te.dwSize = sizeof te;
    for (BOOL ok = Thread32First(snap, &te); ok; ok = Thread32Next(snap, &te)) {
        if (te.th32OwnerProcessID != pid || te.th32ThreadID == g_prof_tid) continue;
        int j = 0;
        while (j < g_nthreads && g_threads[j].tid != te.th32ThreadID) j++;
        if (j < g_nthreads) {
            g_threads[j].seen = 1;
            continue;
        }
        if (g_nthreads == PROF_MAX_THREADS) continue;
        HANDLE h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, FALSE, te.th32ThreadID);
        if (!h) continue;
        prof_thread *t = &g_threads[g_nthreads++];
        t->tid = te.th32ThreadID;
        t->h = h;
        t->cpu = thread_cpu(h);
        t->acc = 0;
        t->seen = 1;
    }
    CloseHandle(snap);
    for (int i = 0; i < g_nthreads;) {
        if (!g_threads[i].seen) {
            CloseHandle(g_threads[i].h);
            g_threads[i] = g_threads[--g_nthreads];
        } else {
            i++;
        }
    }
}

/* Stack bounds of a (suspended) thread from its TEB's NT_TIB. */
static bool stack_bounds(HANDLE h, uintptr_t *lo, uintptr_t *hi) {
    if (!g_nt_qit) return false;
    thread_basic_info tbi;
    memset(&tbi, 0, sizeof tbi);
    if (g_nt_qit(h, 0 /* ThreadBasicInformation */, &tbi, sizeof tbi, NULL) != 0 || !tbi.TebBaseAddress) return false;
    const NT_TIB *tib = (const NT_TIB *)tbi.TebBaseAddress;
    *hi = (uintptr_t)tib->StackBase;
    *lo = (uintptr_t)tib->StackLimit;
    return *hi > *lo;
}

/* Suspends t, walks its stack, resumes it; number of PCs stored. */
static int sample_thread(prof_thread *t, uintptr_t *pcs) {
    if (SuspendThread(t->h) == (DWORD)-1) return 0;
    int n = 0;
    CONTEXT ctx;
    memset(&ctx, 0, sizeof ctx);
    ctx.ContextFlags = CONTEXT_CONTROL | CONTEXT_INTEGER;
    uintptr_t lo = 0, hi = 0;
    if (GetThreadContext(t->h, &ctx) && stack_bounds(t->h, &lo, &hi)) n = zell__w32_walk(&ctx, lo, hi, pcs, PROF_MAX_FRAMES);
    ResumeThread(t->h);
    return n;
}

static DWORD WINAPI prof_main(LPVOID arg) {
    (void)arg;
    uint64_t period = 10000000ull / (uint64_t)g_prof_hz;  /* 100 ns units of CPU per sample */
    if (!period) period = 1;
    HANDLE timer = CreateWaitableTimerExW(NULL, NULL, 0x2 /* CREATE_WAITABLE_TIMER_HIGH_RESOLUTION */, TIMER_ALL_ACCESS);
    if (!timer) timer = CreateWaitableTimerW(NULL, FALSE, NULL);
    LARGE_INTEGER due;
    due.QuadPart = -(LONGLONG)period;
    LONG ms = (LONG)(period / 10000);
    if (timer && !SetWaitableTimer(timer, &due, ms > 0 ? ms : 1, NULL, NULL, FALSE)) {
        CloseHandle(timer);
        timer = NULL;
    }
    uintptr_t pcs[PROF_MAX_FRAMES];
    unsigned tick = 0;
    refresh_threads();
    for (;;) {
        HANDLE hs[2] = { g_prof_stop_ev, timer };
        DWORD r = timer ? WaitForMultipleObjects(2, hs, FALSE, INFINITE) : WaitForSingleObject(g_prof_stop_ev, ms > 0 ? (DWORD)ms : 1);
        if (r == WAIT_OBJECT_0) break;
        if (++tick % 10 == 0) refresh_threads();
        zell__plat_prof_fn fn = __atomic_load_n(&g_prof_fn, __ATOMIC_ACQUIRE);
        if (!fn) continue;
        for (int i = 0; i < g_nthreads; i++) {
            prof_thread *t = &g_threads[i];
            uint64_t now = thread_cpu(t->h);
            if (now > t->cpu) t->acc += now - t->cpu;
            t->cpu = now;
            /* GetThreadTimes advances in clock ticks (~15.6 ms): the
             * accumulator spreads them over the sampling periods */
            int take = 0;
            while (t->acc >= period && take < 4) {
                t->acc -= period;
                take++;
            }
            if (t->acc > 4 * period) t->acc = 4 * period;
            if (!take) continue;
            int n = sample_thread(t, pcs);
            for (int k = 0; k < take && n > 0; k++) fn(pcs, n);
        }
    }
    if (timer) CloseHandle(timer);
    for (int i = 0; i < g_nthreads; i++) CloseHandle(g_threads[i].h);
    g_nthreads = 0;
    return 0;
}

bool zell__plat_prof_start(int hz, zell__plat_prof_fn fn) {
    if (g_prof_on || hz <= 0) return false;
    if (!g_nt_qit) {
        HMODULE nt = GetModuleHandleW(L"ntdll.dll");
        if (nt) g_nt_qit = (nt_qit_fn)(void *)GetProcAddress(nt, "NtQueryInformationThread");
    }
    zell__plat_thread_stack_init();
    g_prof_hz = hz > 1000 ? 1000 : hz;
    __atomic_store_n(&g_prof_fn, fn, __ATOMIC_RELEASE);
    g_prof_stop_ev = CreateEventW(NULL, TRUE, FALSE, NULL);
    if (!g_prof_stop_ev) return false;
    g_prof_thread = CreateThread(NULL, 0, prof_main, NULL, CREATE_SUSPENDED, &g_prof_tid);
    if (!g_prof_thread) {
        CloseHandle(g_prof_stop_ev);
        return false;
    }
    SetThreadPriority(g_prof_thread, THREAD_PRIORITY_HIGHEST);
    ResumeThread(g_prof_thread);
    g_prof_on = 1;
    return true;
}

void zell__plat_prof_stop(void) {
    if (!g_prof_on) return;
    __atomic_store_n(&g_prof_fn, (zell__plat_prof_fn)NULL, __ATOMIC_RELEASE);
    SetEvent(g_prof_stop_ev);
    WaitForSingleObject(g_prof_thread, INFINITE);
    CloseHandle(g_prof_thread);
    CloseHandle(g_prof_stop_ev);
    g_prof_thread = g_prof_stop_ev = NULL;
    g_prof_on = 0;
}

#else
typedef int zell__prof_win32_unused;
#endif
