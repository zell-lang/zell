/* main.c - runtime entry points, process arguments, exit. */
#include "rt_internal.h"
#include "heap.h"
#include "platform/platform.h"

#include <stdlib.h>

static zell_slice g_args;   /* []string, owned by the runtime, built on first use */
static int g_argc;
static char **g_argv;
static int g_initialized;
static zell__plat_mutex g_args_mu = ZELL_PLAT_MUTEX_INIT;
static void (*g_args_free)(void);   /* set once g_args exists (shutdown releases it) */

void zell_runtime_init(int argc, char **argv) {
    if (g_initialized) return;
    g_initialized = 1;
    zell__plat_runtime_init(&argc, &argv);  /* Windows: UTF-8 argv, console */
    g_argc = argc;
    g_argv = argv;
    zell__process_init();   /* the scheduler starts with the first task (task.c) */
    zell__rc_thread_eligible();   /* biased refcounts once threaded (rc.c) */
}

void zell_runtime_drain(void) {
    zell__sched_stop();
}

void zell_runtime_shutdown(void) {
    /* Orderly shutdown: process-wide singletons first (diag's slot
     * map, diag.c), while the scheduler still runs the tasks their
     * destructors wake; then the scheduler, I/O and resolver threads;
     * then what was registered again meanwhile. */
    if (zell__diag_shutdown_fn) zell__diag_shutdown_fn();
    zell__sched_stop();
    zell__io_flush_all();
    if (g_args_free) g_args_free();
    if (zell__diag_shutdown_fn) zell__diag_shutdown_fn();
    g_initialized = 0;
    /* objects waiting in refcount merge queues (rc.c) */
    if (zell__rc_drain_fn) zell__rc_drain_fn();
    zell__frame_cache_flush();   /* this thread's cached coroutine frames (mem.c) */
    zell__leak_report();
}

static void args_free(void) { zell_slice_release(&g_args); }

void zell_os_args(zell_slice *out) {
    zell__plat_mutex_lock(&g_args_mu);
    if (!g_args_free) {
        /* Built lazily: a program that never reads its arguments does not
         * allocate them (nor link the slice code for it). */
        int argc = g_argc;
        zell_slice_new_ops(&g_args, (int64_t)sizeof(zell_string *), 0, argc > 0 ? argc : 0, &zell_ops_ref);
        for (int i = 0; i < argc; i++) {
            zell_string *s = zell_string_from_cstr(g_argv[i]);
            zell_slice_append(&g_args, (int64_t)sizeof(zell_string *), &zell_ops_ref, &s);
            zell_release(s);
        }
        g_args_free = args_free;
    }
    zell_slice_retain(&g_args);
    *out = g_args;
    zell__plat_mutex_unlock(&g_args_mu);
}

void zell_exit(int32_t code) {
    zell_dbg_exit(code);
    zell__io_flush_all();
    _Exit(code);
}
