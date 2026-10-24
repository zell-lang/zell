/* panic.c - fatal errors: message + best-effort backtrace, exit code 2. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <inttypes.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int g_panicking;

/* The panic this thread is unwinding (task panics, ABI.md 8.1). */
static _Thread_local int t_unwinding;
static _Thread_local zell_string *t_pmsg, *t_ptrace;
static _Thread_local int t_pdelivered;

#define TRACE_CAP 16384

static void print_trace_text(const char *trace, int64_t tlen) {
    if (getenv("ZELL_NO_BACKTRACE")) return;
    fputs("\nbacktrace (most recent call first):\n", stderr);
    fwrite(trace, 1, (size_t)tlen, stderr);
}

/* The process ends with the panic: message, trace (given text, or this
 * thread's backtrace when trace is NULL), exit code 2. */
static ZELL_NORETURN __attribute__((noinline)) void die_final(const char *msg, int64_t len, const char *trace, int64_t tlen, int quiet) {
    /* The debug agent stops here first (exception breakpoint "panic"). */
    if (!quiet && __atomic_load_n(&zell_dbg_on, __ATOMIC_RELAXED) && zell__dbg_panic_fn) zell__dbg_panic_fn(msg, len);
    /* A second panic (e.g. inside a destructor run during the first) aborts quietly. */
    if (__atomic_exchange_n(&g_panicking, 1, __ATOMIC_SEQ_CST)) {
        for (;;) zell__plat_sleep_ns((int64_t)1000000000);  /* another thread is printing the first panic and will _exit */
    }
    zell__io_flush_all();
    fputs("panic: ", stderr);
    fwrite(msg, 1, (size_t)len, stderr);
    fputc('\n', stderr);
    if (trace) {
        print_trace_text(trace, tlen);
    } else if (!getenv("ZELL_NO_BACKTRACE")) {
        fputs("\nbacktrace (most recent call first):\n", stderr);
        fflush(stderr);
        /* skip zell__plat_backtrace, die_final, die_traced, die and the zell_panic* entry point */
        zell__plat_backtrace(2, 5);
    }
    fflush(stderr);
    /* An attached debugger learns that the program ends (DAP `exited` +
     * `terminated`), also after `continue` from a panic stop. */
    if (__atomic_load_n(&zell_dbg_on, __ATOMIC_RELAXED)) zell_dbg_exit(2);
    _Exit(2);
}

void zell__panic_print(const zell_string *msg, const zell_string *trace) {
    zell__io_flush_all();
    zell__plat_flockfile(stderr);
    fputs("panic: ", stderr);
    fwrite(zell_string_data(msg), 1, (size_t)zell_string_len(msg), stderr);
    fputc('\n', stderr);
    if (trace) print_trace_text(zell_string_data(trace), zell_string_len(trace));
    zell__plat_funlockfile(stderr);
    fflush(stderr);
}

void zell__panic_current(zell_string **msg, zell_string **trace) {
    *msg = t_unwinding ? t_pmsg : NULL;
    *trace = t_unwinding ? t_ptrace : NULL;
    if (t_unwinding) t_pdelivered++;
}

/* `trace` NULL: capture this thread's backtrace (a fresh panic). */
static ZELL_NORETURN __attribute__((noinline)) void die_traced(const char *msg, int64_t len, const char *trace, int64_t tlen, int quiet) {
    /* testing.Panics: a capture scope on this thread takes the panic. */
    zell__panic_capture *cap = zell__panic_capture_tls;
    if (cap) {
        zell__panic_capture_tls = NULL;
        cap->msg = malloc((size_t)len + 1);
        if (cap->msg) {
            memcpy(cap->msg, msg, (size_t)len);
            cap->msg[len] = 0;
            cap->len = len;
        }
        longjmp(cap->jb, 1);
    }
    if (t_unwinding) {
        /* A panic in a deferred call run while unwinding another panic
         * ends the process: both messages, exit code 2. */
        zell__io_flush_all();
        fputs("panic: ", stderr);
        fwrite(zell_string_data(t_pmsg), 1, (size_t)zell_string_len(t_pmsg), stderr);
        fputc('\n', stderr);
        print_trace_text(zell_string_data(t_ptrace), zell_string_len(t_ptrace));
        fputs("\npanic while unwinding the panic above:\n", stderr);
        die_final(msg, len, trace, tlen, quiet);
    }
    zell__job_unwind *uw = zell__job_unwind_tls;
    if (uw && (uw->resumed || zell__task_running())) {
        /* A task panics: only the task ends (ABI.md 8.1). The debugger
         * stops before anything is unwound. */
        if (!quiet && __atomic_load_n(&zell_dbg_on, __ATOMIC_RELAXED) && zell__dbg_panic_fn) zell__dbg_panic_fn(msg, len);
        t_pmsg = zell_string_new(msg, len);
        if (trace) {
            t_ptrace = zell_string_new(trace, tlen);
        } else {
            char *buf = malloc(TRACE_CAP);
            if (buf) {
                /* skip zell__plat_backtrace_buf, die_traced, die and the zell_panic* entry point */
                zell__plat_backtrace_buf(buf, TRACE_CAP, 4);
                t_ptrace = zell_string_new(buf, (int64_t)strlen(buf));
                free(buf);
            } else {
                t_ptrace = zell_string_new("", 0);
            }
        }
        t_unwinding = 1;
        t_pdelivered = 0;
        zell__coro_unwind(uw->chain0);
        t_unwinding = 0;
        /* No coroutine of a task took it (the job's task is not a compiled
         * coroutine on this stack): nobody will observe it. */
        if (!t_pdelivered) zell__panic_print(t_pmsg, t_ptrace);
        zell_release(t_pmsg);
        zell_release(t_ptrace);
        t_pmsg = t_ptrace = NULL;
        zell__plat_longjmp(uw->jb);
    }
    die_final(msg, len, trace, tlen, quiet);
}

static ZELL_NORETURN __attribute__((noinline)) void die(const char *msg, int64_t len) {
    die_traced(msg, len, NULL, 0, 0);
}

void zell__panic_traced(const char *prefix, const zell_string *msg, const zell_string *trace, int quiet) {
    size_t pl = strlen(prefix);
    int64_t ml = zell_string_len(msg);
    /* A panic awaited through several tasks keeps one prefix. */
    if ((int64_t)pl <= ml && memcmp(zell_string_data(msg), prefix, pl) == 0) pl = 0;
    char *buf = malloc(pl + (size_t)ml + 1);
    if (!buf) zell_panic("out of memory (panic)");
    memcpy(buf, prefix, pl);
    memcpy(buf + pl, zell_string_data(msg), (size_t)ml);
    die_traced(buf, (int64_t)pl + ml, zell_string_data(trace), zell_string_len(trace), quiet);
}

void zell__panic_traced_owned(const char *prefix, zell_string *msg, zell_string *trace) {
    size_t pl = strlen(prefix);
    int64_t ml = zell_string_len(msg), tl = zell_string_len(trace);
    if ((int64_t)pl <= ml && memcmp(zell_string_data(msg), prefix, pl) == 0) pl = 0;
    char *buf = malloc(pl + (size_t)ml + (size_t)tl + 2);
    if (!buf) zell_panic("out of memory (panic)");
    memcpy(buf, prefix, pl);
    memcpy(buf + pl, zell_string_data(msg), (size_t)ml);
    char *tb = buf + pl + (size_t)ml + 1;
    memcpy(tb, zell_string_data(trace), (size_t)tl);
    zell_release(msg);
    zell_release(trace);
    die_traced(buf, (int64_t)pl + ml, tb, tl, 0);
}

void zell_panic(const char *msg) {
    if (!msg) msg = "";
    die(msg, (int64_t)strlen(msg));
}

void zell_panic_str(const zell_string *msg) {
    die(zell_string_data(msg), zell_string_len(msg));
}

void zell_panic_error(const zell_error *err) {
    zell_string *m = zell_error_message(err);
    die(zell_string_data(m), zell_string_len(m));
}

void zell__panicf(const char *fmt, ...) {
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (n < 0) n = 0;
    if (n >= (int)sizeof buf) n = (int)sizeof buf - 1;
    die(buf, n);
}

void zell_panic_nil(void) {
    zell__panicf("runtime error: invalid memory address or nil pointer dereference");
}

void zell_panic_bounds(int64_t idx, int64_t len) {
    zell__panicf("runtime error: index out of range [%" PRId64 "] with length %" PRId64,
                  idx, len);
}

void zell_panic_slice_bounds(int64_t lo, int64_t hi, int64_t cap) {
    zell__panicf("runtime error: slice bounds out of range [%" PRId64 ":%" PRId64
                  "] with capacity %" PRId64, lo, hi, cap);
}
