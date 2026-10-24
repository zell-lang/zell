#include "test.h"

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
}

int main(void) {
    printf("test_panic\n");
    RUN(messages);
    RUN(flush_and_exit);
    RUN(line_table);
    return 0;
}
