/* Tests for the platform layer (src/platform/), the I/O readiness waits
 * (task_io.c), the net wrappers (std_net.c) and the task primitives
 * added for task (context slot, signals, spin locks, yield). */
#include "test.h"
#include "../src/platform/platform.h"
#include <errno.h>
#include <time.h>

#if !defined(_WIN32)
#include <sys/socket.h>
#include <fcntl.h>
/* A connected socket pair (non-blocking ends if asked). */
static void sock_pair(int64_t sv[2], bool nonblock) {
    int v[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, v) == 0);  /* SOCK_NONBLOCK is Linux-only */
    for (int i = 0; i < 2 && nonblock; i++) assert(fcntl(v[i], F_SETFL, fcntl(v[i], F_GETFL) | O_NONBLOCK) == 0);
    sv[0] = v[0];
    sv[1] = v[1];
}
static int64_t s_write(int64_t fd, const char *p, int64_t n) { return write((int)fd, p, (size_t)n); }
static int64_t s_read(int64_t fd, char *p, int64_t n) { return read((int)fd, p, (size_t)n); }
static void s_close(int64_t fd) { close((int)fd); }
#else
/* Windows: loopback TCP through the platform layer (no socketpair). */
static void sock_pair(int64_t sv[2], bool nonblock) {
    (void)nonblock;  /* platform sockets are always non-blocking */
    int64_t l = zell__plat_sock_open(ZELL_PLAT_AF_INET, ZELL_PLAT_STREAM);
    assert(l >= 0);
    zell__plat_addr a;
    unsigned char lo[4] = {127, 0, 0, 1};
    assert(zell__plat_addr_ip(&a, lo, 4, 0, 0));
    assert(zell__plat_sock_bind(l, &a) == 0 && zell__plat_sock_listen(l, 4) == 0);
    assert(zell__plat_sock_name(l, false, &a));
    int64_t c = zell__plat_sock_open(ZELL_PLAT_AF_INET, ZELL_PLAT_STREAM);
    int64_t r = zell__plat_sock_connect(c, &a);
    assert(r == 0 || r == -ZELL_PLAT_EAGAIN);
    int64_t s = -1;
    for (int i = 0; i < 2000 && s < 0; i++) {
        s = zell__plat_sock_accept(l, NULL);
        if (s < 0) zell__plat_sleep_ns(1000000);
    }
    assert(s >= 0);
    zell__plat_sock_close(l);
    sv[0] = s;
    sv[1] = c;
}
static int64_t s_write(int64_t fd, const char *p, int64_t n) {
    for (int i = 0; i < 2000; i++) {  /* the connect may still be completing */
        int64_t r = zell__plat_sock_send(fd, p, n);
        if (r != -ZELL_PLAT_EAGAIN) return r;
        zell__plat_sleep_ns(1000000);
    }
    return -1;
}
static int64_t s_read(int64_t fd, char *p, int64_t n) { return zell__plat_sock_recv(fd, p, n); }
static void s_close(int64_t fd) { zell__plat_sock_close(fd); }
#endif

static int64_t now_ms(void) { return zell__plat_mono_ns() / 1000000; }

static int64_t wait_code(zell_task *t) {
    int64_t r = *(int64_t *)zell_task_await(t);
    zell_release(t);
    return r;
}

/* ---- platform basics ---- */

static void *thread_fn(void *arg) {
    *(int *)arg = 42;
    return NULL;
}

TEST(threads_clock_entropy) {
    int v = 0;
    zell__plat_thread th;
    assert(zell__plat_thread_start(&th, thread_fn, &v));
    zell__plat_thread_join(th);
    assert(v == 42);
    assert(zell__plat_cpu_count() >= 1);
    int64_t a = zell__plat_mono_ns();
    zell__plat_sleep_ns(2000000);
    assert(zell__plat_mono_ns() - a >= 2000000);
    unsigned char b1[32] = { 0 }, b2[32] = { 0 };
    assert(zell__plat_random_bytes(b1, sizeof b1));
    assert(zell__plat_random_bytes(b2, sizeof b2));
    assert(memcmp(b1, b2, sizeof b1) != 0);
    assert(zell__plat_is_ip("127.0.0.1") && zell__plat_is_ip("::1") && !zell__plat_is_ip("localhost"));
}

TEST(poller_raw) {
    zell__plat_poller *p = zell__plat_poller_new();
    assert(p);
    int64_t sv[2];
    sock_pair(sv, false);
    zell__plat_event ev[8];
    assert(zell__plat_poller_arm(p, sv[0], ZELL_PLAT_READ, 77, false) == 0);
    if (zell__plat_poller_edge(p))   /* edge-triggered: watches both directions, reports writable once */
        assert(zell__plat_poller_wait(p, ev, 8, 1000) == 1 && !(ev[0].events & ZELL_PLAT_READ));
    assert(zell__plat_poller_wait(p, ev, 8, 10) == 0);       /* nothing to read: timeout */
    assert(s_write(sv[1], "x", 1) == 1);
    int n = zell__plat_poller_wait(p, ev, 8, 1000);
    assert(n == 1 && ev[0].token == 77 && (ev[0].events & ZELL_PLAT_READ));
    zell__plat_poller_wake(p);
    n = zell__plat_poller_wait(p, ev, 8, 1000);
    assert(n >= 1);
    bool woke = false;
    for (int i = 0; i < n; i++) woke |= ev[i].token == ZELL_PLAT_WAKE_TOKEN;
    assert(woke);
    zell__plat_poller_remove(p, sv[0]);
    s_close(sv[0]);
    s_close(sv[1]);
    zell__plat_poller_free(p);
}

/* ---- calendar, time formatting, locks, files (portable layer) ---- */

TEST(time_portable) {
    struct tm tm;
    /* 2006-01-02 15:04:05 UTC Monday; leap day; before the epoch; far future */
    static const struct { int64_t t; int y, mo, d, h, mi, se, wd, yd; } cases[] = {
        {1136214245, 2006, 1, 2, 15, 4, 5, 1, 1}, {951782400, 2000, 2, 29, 0, 0, 0, 2, 59},
        {-1, 1969, 12, 31, 23, 59, 59, 3, 364}, {0, 1970, 1, 1, 0, 0, 0, 4, 0},
        {253402300799LL, 9999, 12, 31, 23, 59, 59, 5, 364}, {-62135596800LL, 1, 1, 1, 0, 0, 0, 1, 0},
    };
    for (size_t i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        zell__plat_utc_tm(cases[i].t, &tm);
        assert(tm.tm_year + 1900 == cases[i].y && tm.tm_mon + 1 == cases[i].mo && tm.tm_mday == cases[i].d);
        assert(tm.tm_hour == cases[i].h && tm.tm_min == cases[i].mi && tm.tm_sec == cases[i].se);
        assert(tm.tm_wday == cases[i].wd && tm.tm_yday == cases[i].yd);
        assert(zell__plat_timegm(&tm) == cases[i].t);
    }
#if defined(ZELL_PLAT_LIBC_TIMEFMT)
    for (int64_t t = -5000000000LL; t < 5000000000LL; t += 7777777) {
        time_t tt = (time_t)t;
        struct tm g;
        gmtime_r(&tt, &g);
        zell__plat_utc_tm(t, &tm);
        assert(g.tm_year == tm.tm_year && g.tm_yday == tm.tm_yday && g.tm_sec == tm.tm_sec && g.tm_wday == tm.tm_wday);
        char a[256], b[256];
        const char *f = "%a %A %b %B %C %d %D %e %F %g %G %H %I %j %k %l %m %M %p %R %S %T %u %U %V %w %W %y %Y %%";
        assert(strftime(a, sizeof a, f, &g) > 0);
        assert(zell__plat_strftime_portable(b, sizeof b, f, &tm) > 0);
        assert(strcmp(a, b) == 0);
    }
#endif
    char buf[128];
    zell__plat_utc_tm(1136214245, &tm);
    assert(zell__plat_strftime_portable(buf, sizeof buf, "%Y-%m-%dT%H:%M:%S %a %b %j %e|", &tm) > 0);
    assert(strcmp(buf, "2006-01-02T15:04:05 Mon Jan 002  2|") == 0);
    assert(zell__plat_strftime_portable(buf, 5, "%Y-%m", &tm) == 0);  /* too small */
    struct tm p;
    memset(&p, 0, sizeof p);
    long off = 1;
    const char *end = zell__plat_strptime_portable("2006-01-02 15:04:05 +0130", "%Y-%m-%d %H:%M:%S %z", &p, &off);
    assert(end && *end == 0 && off == 5400);
    assert(zell__plat_timegm(&p) == 1136214245);
    memset(&p, 0, sizeof p);
    end = zell__plat_strptime_portable("Mon, 02 Jan 2006 03:04 PM", "%a, %d %b %Y %I:%M %p", &p, &off);
    assert(end && *end == 0 && p.tm_hour == 15 && p.tm_mon == 0 && off == 0);
    assert(!zell__plat_strptime_portable("2006-13-01", "%Y-%m-%d", &p, &off));
}

static zell__plat_mutex g_mu = ZELL_PLAT_MUTEX_INIT;
static zell__plat_cond g_cv = ZELL_PLAT_COND_INIT;
static zell__plat_once g_once = ZELL_PLAT_ONCE_INIT;
static int g_once_n, g_flag;
static void once_fn(void) { g_once_n++; }
static void *signaller(void *arg) {
    (void)arg;
    zell__plat_sleep_ns(20000000);
    zell__plat_mutex_lock(&g_mu);
    g_flag = 1;
    zell__plat_cond_signal(&g_cv);
    zell__plat_mutex_unlock(&g_mu);
    return NULL;
}

TEST(locks_once_tls) {
    zell__plat_once_run(&g_once, once_fn);
    zell__plat_once_run(&g_once, once_fn);
    assert(g_once_n == 1);
    zell__plat_mutex_lock(&g_mu);
    int64_t t0 = zell__plat_mono_ns();
    assert(!zell__plat_cond_wait_ns(&g_cv, &g_mu, 15000000));  /* times out */
    assert(zell__plat_mono_ns() - t0 >= 14000000);
    zell__plat_thread th;
    assert(zell__plat_thread_start(&th, signaller, NULL));
    while (!g_flag) zell__plat_cond_wait(&g_cv, &g_mu);
    zell__plat_mutex_unlock(&g_mu);
    zell__plat_thread_join(th);
    zell__plat_cond init_cv;
    zell__plat_cond_init(&init_cv);
    zell__plat_mutex_lock(&g_mu);
    assert(!zell__plat_cond_wait_ns(&init_cv, &g_mu, 1000000));
    zell__plat_mutex_unlock(&g_mu);
    zell__plat_cond_destroy(&init_cv);
    zell__plat_tls_key k;
    assert(zell__plat_tls_new(&k, NULL));
    zell__plat_tls_set(k, &g_flag);
    int64_t w0 = zell__plat_wall_ns();
    assert(w0 > 1600000000LL * 1000000000LL);  /* after 2020 */
    void *a = zell__plat_aligned_alloc(4096, 8192);
    assert(a && ((uintptr_t)a & 4095) == 0);
    zell__plat_aligned_free(a);
    char *m = malloc(100);
    assert(zell__plat_alloc_size(m) >= 100);
    free(m);
    assert(zell__plat_pid() > 0);
    char host[256];
    assert(zell__plat_hostname(host, sizeof host) == 0 && host[0]);
    char **env = zell__plat_environ();
    assert(env && env[0]);
    zell__plat_free_strv(env);
    assert(zell__plat_setenv("ZELL_PLAT_T", "x\xc3\xa9") == 0);  /* UTF-8 value */
    char *v = zell__plat_getenv("ZELL_PLAT_T");
    assert(v && strcmp(v, "x\xc3\xa9") == 0);
    free(v);
    assert(zell__plat_setenv("ZELL_PLAT_T", NULL) == 0);
    assert(!zell__plat_getenv("ZELL_PLAT_T"));
}

TEST(files_dirs) {
    char *tmp = zell__plat_temp_dir();
    assert(tmp);
    char dir[512], file[600], file2[600];
    snprintf(dir, sizeof dir, "%s/zell_plat_%lld_\xc3\xa9", tmp, (long long)zell__plat_pid());  /* non-ASCII name */
    free(tmp);
    assert(zell__plat_mkdir(dir) == 0);
    assert(zell__plat_mkdir(dir) == EEXIST);
    snprintf(file, sizeof file, "%s/a.txt", dir);
    snprintf(file2, sizeof file2, "%s/b.txt", dir);
    int64_t fd = zell__plat_open(file, ZELL_PLAT_O_WRONLY | ZELL_PLAT_O_CREATE | ZELL_PLAT_O_TRUNC, 0644);
    assert(fd >= 0);
    assert(zell__plat_write_all(fd, "hello\nworld", 11) == 0);
    zell__plat_stat st;
    assert(zell__plat_fstat(fd, &st) == 0 && st.size == 11 && st.kind == ZELL_PLAT_KIND_FILE);
    assert(zell__plat_close(fd) == 0);
    fd = zell__plat_open(file, ZELL_PLAT_O_RDONLY, 0);
    char b[32];
    assert(zell__plat_pread(fd, b, 5, 6) == 5 && memcmp(b, "world", 5) == 0);
    assert(zell__plat_read(fd, b, sizeof b) == 11);
    assert(zell__plat_read(fd, b, sizeof b) == 0);
    assert(zell__plat_seek(fd, 2, 0) == 2);
    zell__plat_close(fd);
    assert(zell__plat_open(file, ZELL_PLAT_O_WRONLY | ZELL_PLAT_O_CREATE | ZELL_PLAT_O_EXCL, 0644) == -EEXIST);
    fd = zell__plat_open(file, ZELL_PLAT_O_WRONLY | ZELL_PLAT_O_APPEND, 0);
    assert(fd >= 0 && zell__plat_write_all(fd, "!", 1) == 0);
    assert(zell__plat_ftruncate(fd, 3) == 0);
    zell__plat_close(fd);
    assert(zell__plat_stat_path(file, true, &st) == 0 && st.size == 3 && st.mtime_sec > 1600000000);
    assert(zell__plat_stat_path(dir, false, &st) == 0 && st.kind == ZELL_PLAT_KIND_DIR);
    assert(zell__plat_rename(file, file2) == 0);
    assert(zell__plat_stat_path(file, true, &st) == ENOENT);
    int e = 0, kind = -2, count = 0;
    zell__plat_dir *d = zell__plat_dir_open(dir, &e);
    assert(d && e == 0);
    const char *name;
    while ((name = zell__plat_dir_next(d, &kind, &e)) != NULL) {
        assert(strcmp(name, "b.txt") == 0 && (kind == ZELL_PLAT_KIND_FILE || kind == -1));
        count++;
    }
    assert(e == 0 && count == 1);
    zell__plat_dir_close(d);
    assert(zell__plat_dir_open(file2, &e) == NULL && e == ENOTDIR);
    assert(zell__plat_rmdir(dir) != 0);  /* not empty */
    assert(zell__plat_chmod(file2, 0444) == 0);
    assert(zell__plat_stat_path(file2, true, &st) == 0 && !(st.perm & 0200));
    assert(zell__plat_unlink_file(file2) == 0);
    assert(zell__plat_remove(dir) == 0);
    int ce = 0;
    char *cwd = zell__plat_getcwd(&ce);
    assert(cwd && ce == 0);
    free(cwd);
}

/* ---- readiness waits through the scheduler ---- */

TEST(io_waits) {
    int64_t sv[2];
    sock_pair(sv, true);
    /* readable after a write from another side */
    zell_task *t = zell_io_wait_readable(sv[0], -1);
    assert(!zell_task_is_done(t));
    assert(s_write(sv[1], "hi", 2) == 2);
    assert(wait_code(t) == 0);
    char buf[4];
    assert(s_read(sv[0], buf, sizeof buf) == 2);
    /* timeout */
    int64_t t0 = now_ms();
    assert(wait_code(zell_io_wait_readable(sv[0], 30)) == 1);
    assert(now_ms() - t0 >= 29);
    /* read and write waiters on one fd at once; the writer completes at
     * once (writable; on an edge-triggered poller the writability reported
     * when the fd was added is still unconsumed: no write has failed) */
    zell_task *r = zell_io_wait_readable(sv[0], -1);
    zell_task *w = zell_io_wait_writable(sv[0], -1);
    assert(wait_code(w) == 0);
    assert(!zell_task_is_done(r));
    /* forget wakes the reader with 2 */
    zell_io_forget(sv[0]);
    assert(wait_code(r) == 2);
    s_close(sv[0]);
    s_close(sv[1]);
}

TEST(io_many_fds) {
    enum { N = 40 };
    int64_t sv[N][2];
    zell_task *ts[N];
    for (int i = 0; i < N; i++) {
        sock_pair(sv[i], true);
        ts[i] = zell_io_wait_readable(sv[i][0], 5000);
    }
    for (int i = N - 1; i >= 0; i--) assert(s_write(sv[i][1], "z", 1) == 1);
    zell_task *all = zell_task_all(ts, N);
    zell_task_await(all);
    zell_release(all);
    for (int i = 0; i < N; i++) {
        assert(*(int64_t *)zell_task_result(ts[i]) == 0);
        zell_release(ts[i]);
        zell_io_forget(sv[i][0]);
        s_close(sv[i][0]);
        s_close(sv[i][1]);
    }
}

/* ---- task context, signals, spin locks, yield ---- */

typedef struct ctx_probe {
    zell_task *done;    /* completed by the continuation */
    zell_object *seen;  /* context seen there (owned) */
} ctx_probe;

static void probe_cont(void *arg) {
    ctx_probe *p = arg;
    p->seen = zell_task_get_ctx();
    zell_task_complete(p->done, NULL);
}

TEST(context_follows_continuations) {
    zell_string *c1 = S("ctx-one");
    zell_task_set_ctx((zell_object *)c1);
    zell_object *got = zell_task_get_ctx();
    assert(got == (zell_object *)c1);
    zell_release(got);
    ctx_probe p = { zell_task_rt_waker(), NULL };
    zell_task *d = zell_task_delay(5);
    assert(zell_task_add_continuation(d, probe_cont, &p));
    zell_task_set_ctx(NULL);           /* the registering code's context is kept */
    zell_task_await(p.done);
    assert(p.seen == (zell_object *)c1);
    zell_release(p.seen);
    zell_release(p.done);
    zell_release(d);
    /* posted jobs inherit the poster's context too */
    zell_task_set_ctx((zell_object *)c1);
    ctx_probe q = { zell_task_rt_waker(), NULL };
    zell_sched_post(probe_cont, &q);
    zell_task_set_ctx(NULL);
    zell_task_await(q.done);
    assert(q.seen == (zell_object *)c1);
    zell_release(q.seen);
    zell_release(q.done);
    got = zell_task_get_ctx();
    assert(got == NULL);
    zell_release(c1);
}

TEST(signals_and_cancel) {
    zell_task *s = zell_task_rt_signal();
    assert(!zell_task_is_done(s));
    zell_task_rt_fire(s);
    zell_task_rt_fire(s);  /* idempotent */
    assert(zell_task_is_done(s));
    zell_release(s);
    int64_t t0 = now_ms();
    zell_task *d = zell_task_delay(60000);
    zell_task_rt_fire(d);  /* cancels the timer and completes now */
    zell_task_await(d);
    assert(now_ms() - t0 < 1000);
    zell_release(d);
    zell_task *s2 = zell_task_rt_signal();
    assert(zell_task_try_complete(s2, NULL));
    assert(!zell_task_try_complete(s2, NULL));
    zell_release(s2);
    zell_task *y = zell_task_yield();
    zell_task_await(y);
    zell_release(y);
}

static int64_t spin_slot, spin_count;

static void spin_worker(zell_task *t, void *unused) {
    (void)unused;
    for (int i = 0; i < 20000; i++) {
        zell_task_spin_lock(&spin_slot);
        spin_count++;
        zell_task_spin_unlock(&spin_slot);
    }
    zell_task_complete(t, NULL);
}

TEST(spin_lock) {
    zell_task *ts[4];
    for (int i = 0; i < 4; i++) ts[i] = zell_task_spawn(spin_worker, NULL);
    for (int i = 0; i < 4; i++) { zell_task_await(ts[i]); zell_release(ts[i]); }
    assert(spin_count == 80000 && spin_slot == 0);
}

int main(int argc, char **argv) {
    printf("test_platform\n");
    setenv("ZELL_WORKERS", "3", 1);
    int64_t live = zell_live_objects();
    zell_runtime_init(argc, argv);
    RUN(threads_clock_entropy);
    RUN(time_portable);
    RUN(locks_once_tls);
    RUN(files_dirs);
    RUN(poller_raw);
    RUN(io_waits);
    RUN(io_many_fds);
    RUN(context_follows_continuations);
    RUN(signals_and_cancel);
    RUN(spin_lock);
    zell_runtime_shutdown();
    assert(zell_live_objects() == live);
    return 0;
}
