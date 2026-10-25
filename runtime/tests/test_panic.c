#include "test.h"
#include "../src/rt_internal.h"   /* zell__plat_setjmp */

static void p_msg(void) { zell_panic("something broke"); }
static void p_nil(void) { zell_panic_nil(); }
static void p_bounds(void) { zell_panic_bounds(5, 3); }
static void p_err(void) {
    zell_string *c = S("ctx");
    zell_error *e = zell_error_wrap(zell_error_new_cstr("inner"), c);
    zell_panic_error(e);
}
static void p_str(void) { zell_panic_str(S("from string")); }
static void p_stdout_flushed(void) {
    zell_print_cstr("before panic");
    zell_panic("x");
}
static void p_exit(void) { zell_print_cstr("bye"); zell_exit(7); }

/* A line table (ABI.md 8.2) whose one code range is p_lined: every
 * address in it is t.zl:7, inside function `lined` (t.zl:1-100). */
static __attribute__((noinline)) void p_lined(void) { zell_panic("lined"); }
static _Alignas(4) unsigned char g_tab[24 + 12 + 4 + 8 + 12 + 16];
static void put32(unsigned char *p, uint32_t v) { memcpy(p, &v, 4); }
static void register_table(void) {
    unsigned char *t = g_tab;
    memcpy(t, "ZLN2", 4);
    put32(t + 4, 1);          /* nfiles */
    put32(t + 8, 1);          /* nlines */
    put32(t + 12, 1);         /* nfuncs */
    put32(t + 16, 11);        /* strsize: "t.zl\0lined\0" */
    put32(t + 20, 1);         /* ncode */
    put32(t + 24, (uint32_t)(int32_t)((intptr_t)(void *)p_lined - (intptr_t)(t + 24)));
    put32(t + 28, 4096);      /* size */
    put32(t + 32, 0);         /* first line entry */
    put32(t + 36, 0);         /* files[0] = "t.zl" */
    put32(t + 40, 0);         /* lines[0]: off 0 */
    put32(t + 44, 7);         /* file 0, line 7 */
    put32(t + 48, 1);         /* funcs[0]: file 0, first line 1 */
    put32(t + 52, 100);       /* last line */
    put32(t + 56, 5);         /* name "lined" */
    memcpy(t + 60, "t.zl\0lined\0", 11);
    zell_lines_register(t, 71);
}
/* registers in the child: on Windows run_child starts a new process */
static void p_lined_child(void) { register_table(); p_lined(); }
/* The same table in the "ZLN3" layout (x86_64 Mach-O): code entries
 * {u64 address; u32 size; u32 first}. */
static _Alignas(8) unsigned char g_tab3[24 + 16 + 4 + 8 + 12 + 16];
static void register_table3(void) {
    unsigned char *t = g_tab3;
    uint64_t a = (uint64_t)(uintptr_t)(void *)p_lined;
    memcpy(t, "ZLN3", 4);
    put32(t + 4, 1);
    put32(t + 8, 1);
    put32(t + 12, 1);
    put32(t + 16, 11);
    put32(t + 20, 1);
    memcpy(t + 24, &a, 8);
    put32(t + 32, 4096);
    put32(t + 36, 0);
    put32(t + 40, 0);
    put32(t + 44, 0);
    put32(t + 48, 7);
    put32(t + 52, 1);
    put32(t + 56, 100);
    put32(t + 60, 5);
    memcpy(t + 64, "t.zl\0lined\0", 11);
    zell_lines_register(t, 75);
}
static void p_lined3_child(void) { register_table3(); p_lined(); }

TEST(messages) {
    char buf[16384];
    assert(run_child(p_msg, buf, sizeof buf) == 2);
    assert(strncmp(buf, "panic: something broke\n", 23) == 0);
    assert(strstr(buf, "backtrace"));
    assert(run_child(p_nil, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: runtime error: invalid memory address or nil pointer dereference\n"));
    assert(run_child(p_bounds, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: runtime error: index out of range [5] with length 3\n"));
    assert(run_child(p_err, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: ctx: inner\n"));
    assert(run_child(p_str, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: from string\n"));
}

TEST(flush_and_exit) {
    char buf[4096];
    assert(run_child_fd(p_stdout_flushed, 1, buf, sizeof buf) == 2);
    assert(strcmp(buf, "before panic") == 0);
    assert(run_child_fd(p_exit, 1, buf, sizeof buf) == 7);
    assert(strcmp(buf, "bye") == 0);
}

TEST(line_table) {
    char buf[16384];
    assert(run_child(p_lined_child, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: lined\n"));
    assert(strstr(buf, "\n  t.zl:7 lined\n"));
    assert(run_child(p_lined3_child, buf, sizeof buf) == 2);
    assert(strstr(buf, "\n  t.zl:7 lined\n"));
}

/* ---- Synchronous frames on the frame chain (ABI.md 8.1 item 4a): a task
 * panic long-jumps into each sync frame's landing, innermost first; each
 * landing ends with zell_unwind_resume. */
typedef struct sync_tnode {
    void *prev;
    void *tag;
    zell__plat_jmpbuf jb;
} sync_tnode;

static int64_t sync_done;

/* An outlined cleanup node: { prev, ZELL_SYNC_CLEANUP, fn, sp }, run by a
 * call on top of the panicking stack. */
typedef struct cleanup_tnode {
    void *prev;
    void *tag;
    void (*fn)(void *);
    void *sp;
} cleanup_tnode;

static void middle_cleanup(void *sp) {
    fprintf(stderr, "middle cleanup %s\n", (const char *)sp);
}

static __attribute__((noinline)) void sync_middle(void (*next)(void)) {
    cleanup_tnode n = { zell_coro_chain, ZELL_SYNC_CLEANUP, middle_cleanup, (void *)"ok" };
    zell_coro_chain = &n;
    next();
    zell_coro_chain = n.prev;
}

static __attribute__((noinline)) void sync_inner(void) {
    sync_tnode n;
    n.prev = zell_coro_chain;
    n.tag = ZELL_SYNC_FRAME;
    zell_coro_chain = &n;
    if (zell__plat_setjmp(n.jb)) {
        fputs("inner landing\n", stderr);
        zell_unwind_resume();
    }
    zell_panic("sync boom");
}

static void sync_job(void *arg) {
    (void)arg;
    sync_tnode n;
    n.prev = zell_coro_chain;
    n.tag = ZELL_SYNC_FRAME;
    zell_coro_chain = &n;
    if (zell__plat_setjmp(n.jb)) {
        fputs("outer landing\n", stderr);
        __atomic_store_n(&sync_done, 1, __ATOMIC_RELEASE);
        zell_unwind_resume();
    }
    sync_middle(sync_inner);
}

static void p_sync_frames(void) {
    zell_task *t = zell_task_new(0, NULL);
    zell_task *prev = zell_task_enter_scope(t);
    zell_sched_post(sync_job, NULL);   /* the job runs as task t */
    zell_task_leave_scope(prev);
    zell_release(prev);
    while (!__atomic_load_n(&sync_done, __ATOMIC_ACQUIRE)) zell__plat_sleep_ns(1000000);
    zell__plat_sleep_ns(200000000);   /* the job prints the unobserved panic */
    (void)t;
    fflush(stderr);
    _Exit(0);
}

TEST(sync_frames_unwind) {
    char buf[16384];
    int rc = run_child(p_sync_frames, buf, sizeof buf);
    if (rc != 0 || strncmp(buf, "inner", 5) != 0) fprintf(stderr, "child exit %d:\n%s\n", rc, buf);
    assert(rc == 0);
    assert(strncmp(buf, "inner landing\nmiddle cleanup ok\nouter landing\npanic: sync boom\n", 63) == 0);
}

int main(void) {
    printf("test_panic\n");
    RUN(messages);
    RUN(flush_and_exit);
    RUN(line_table);
    RUN(sync_frames_unwind);
    return 0;
}
