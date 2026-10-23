/* Tests for the sharded I/O pollers (task_io.c), the platform poller
 * (one-shot level-triggered arming, wake-ups, EINTR), the socket handles
 * of std_net.c (close races, double close, ErrClosed, error sentinels),
 * addresses, Unix sockets, UDP, sendfile and the resolver pool. Built
 * with -fsanitize=thread by `make tsan`. Portable: sockets go through the
 * platform layer (zell__plat_sock_*), so the same file runs under Wine
 * (`make CROSS=x86_64-w64-mingw32 test-wine`, poll_iocp.c); the EINTR
 * check is POSIX-only. */
#include "test.h"
#include "../src/platform/platform.h"
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <time.h>
#if !defined(_WIN32)
#include <sys/socket.h>
#endif

int64_t zell__io_shard_of(int64_t fd);
int64_t zell__io_shards(void);

#if defined(_WIN32)
#define NULL_DEVICE "NUL"
#else
#define NULL_DEVICE "/dev/null"
#endif

static int64_t now_ms(void) { return zell__plat_mono_ns() / 1000000; }

static int64_t wait_code(zell_task *t) {
    int64_t r = *(int64_t *)zell_task_await(t);
    zell_release(t);
    return r;
}

#if defined(__linux__)
static void pair(int sv[2]) { assert(socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, sv) == 0); }
#elif !defined(_WIN32)
static void pair(int sv[2]) {  /* macOS/BSD: no SOCK_NONBLOCK on macOS */
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    for (int i = 0; i < 2; i++) assert(fcntl(sv[i], F_SETFL, fcntl(sv[i], F_GETFL) | O_NONBLOCK) == 0);
}
#else
/* A connected pair of non-blocking loopback TCP sockets. */
static void pair(int sv[2]) {
    int64_t l = zell__plat_sock_open(ZELL_PLAT_AF_INET, ZELL_PLAT_STREAM);
    assert(l >= 0);
    unsigned char lo[4] = { 127, 0, 0, 1 };
    zell__plat_addr a;
    assert(zell__plat_addr_ip(&a, lo, 4, 0, 0));
    assert(zell__plat_sock_bind(l, &a) == 0 && zell__plat_sock_listen(l, 4) == 0);
    assert(zell__plat_sock_name(l, false, &a));
    int64_t c = zell__plat_sock_open(ZELL_PLAT_AF_INET, ZELL_PLAT_STREAM);
    int64_t rc = zell__plat_sock_connect(c, &a);
    assert(rc == 0 || rc == -ZELL_PLAT_EAGAIN);
    int64_t s;
    while ((s = zell__plat_sock_accept(l, NULL)) == -ZELL_PLAT_EAGAIN) Sleep(1);
    assert(s >= 0);
    zell__plat_sock_close(l);
    for (int i = 0; i < 2000 && zell__plat_sock_send(c, "", 0) < 0; i++) Sleep(1);  /* connected */
    sv[0] = (int)s;
    sv[1] = (int)c;
    zell__plat_sock_setopt(s, ZELL_PLAT_OPT_NODELAY, 1);
    zell__plat_sock_setopt(c, ZELL_PLAT_OPT_NODELAY, 1);
}
#endif
/* Socket I/O through the platform layer (sockets are not CRT descriptors on Windows). */
static int64_t sock_write(int fd, const char *b, int64_t n) { return zell__plat_sock_send(fd, b, n); }
static int64_t sock_read(int fd, char *b, int64_t n) { return zell__plat_sock_recv(fd, b, n); }
static void sock_close(int fd) { zell__plat_sock_close(fd); }

static const char *tmp_dir(void) {
#if defined(_WIN32)
    const char *t = getenv("TEMP");
    return t ? t : ".";
#else
    return "/tmp";
#endif
}

static bool msg_has(zell_error *e, const char *needle) {
    zell_string *m = zell_error_message(e);
    bool ok = strstr(zell_string_data(m), needle) != NULL;
    zell_release(m);
    return ok;
}

/* ---- the platform poller ---- */

TEST(poller_oneshot_level) {
    zell__plat_poller *p = zell__plat_poller_new();
    int sv[2];
    pair(sv);
    zell__plat_event ev[8];
    assert(sock_write(sv[1], "ab", 2) == 2);
    if (zell__plat_poller_edge(p)) {
        /* edge-triggered (epoll): added once, the current readiness is
         * reported once, then only new readiness */
        assert(zell__plat_poller_arm(p, sv[0], ZELL_PLAT_READ, 5, false) == 0);
        assert(zell__plat_poller_wait(p, ev, 8, 1000) == 1 && ev[0].token == 5 && (ev[0].events & ZELL_PLAT_READ));
        assert(zell__plat_poller_wait(p, ev, 8, 20) == 0);  /* data remains, no new edge */
        assert(sock_write(sv[1], "c", 1) == 1);
        assert(zell__plat_poller_wait(p, ev, 8, 1000) == 1 && ev[0].token == 5 && (ev[0].events & ZELL_PLAT_READ));
        sock_close(sv[1]);  /* peer hang-up reports readability (end of stream) */
        assert(zell__plat_poller_wait(p, ev, 8, 1000) == 1 && (ev[0].events & ZELL_PLAT_READ));
        zell__plat_poller_remove(p, sv[0]);
        sock_close(sv[0]);
        goto files;
    }
    /* data already there: reported at once (level-triggered) */
    assert(zell__plat_poller_arm(p, sv[0], ZELL_PLAT_READ, 5, false) == 0);
    assert(zell__plat_poller_wait(p, ev, 8, 1000) == 1 && ev[0].token == 5);
    /* one-shot: no second report until armed again, though data remains */
    assert(zell__plat_poller_wait(p, ev, 8, 20) == 0);
    assert(zell__plat_poller_arm(p, sv[0], ZELL_PLAT_READ | ZELL_PLAT_WRITE, 6, true) == 0);
    int n = zell__plat_poller_wait(p, ev, 8, 1000);
    assert(n == 1 && ev[0].token == 6 && (ev[0].events & ZELL_PLAT_READ) && (ev[0].events & ZELL_PLAT_WRITE));
    /* disarm keeps the registration */
    assert(zell__plat_poller_arm(p, sv[0], 0, 7, true) == 0);
    assert(zell__plat_poller_wait(p, ev, 8, 20) == 0);
    /* peer hang-up reports readability (end of stream) */
    sock_close(sv[1]);
    assert(zell__plat_poller_arm(p, sv[0], ZELL_PLAT_READ, 8, true) == 0);
    assert(zell__plat_poller_wait(p, ev, 8, 1000) == 1 && (ev[0].events & ZELL_PLAT_READ));
    zell__plat_poller_remove(p, sv[0]);
    sock_close(sv[0]);
files:;
    /* a regular file cannot be polled; a closed fd is EBADF */
    int f = open(NULL_DEVICE, O_RDONLY);
    int rc = zell__plat_poller_arm(p, f, ZELL_PLAT_READ, 9, false);
    assert(rc == 0 || rc == ZELL_PLAT_ENOTPOLLABLE);
    close(f);
    rc = zell__plat_poller_arm(p, 1000000, ZELL_PLAT_READ, 9, false);
#if defined(_WIN32)
    assert(rc == EBADF || rc == ZELL_PLAT_ENOTPOLLABLE);  /* a number that is no handle at all */
#else
    assert(rc == EBADF);
#endif
    zell__plat_poller_free(p);
}

typedef struct waker_arg {
    zell__plat_poller *p;
    pthread_t target;
    int mode;  /* 0 = wake, 1 = signal (EINTR) */
} waker_arg;

#if !defined(_WIN32)
static void on_usr1(int sig) { (void)sig; }
#endif

static void *waker(void *a) {
    waker_arg *w = a;
    struct timespec ts = { 0, 30 * 1000000 };
    nanosleep(&ts, NULL);
    if (w->mode == 0) zell__plat_poller_wake(w->p);
#if !defined(_WIN32)
    else pthread_kill(w->target, SIGUSR1);
#endif
    return NULL;
}

TEST(poller_wake_and_eintr) {
    zell__plat_poller *p = zell__plat_poller_new();
    zell__plat_event ev[4];
    /* a wake-up from another thread ends a blocked wait */
    waker_arg w = { p, pthread_self(), 0 };
    pthread_t th;
    pthread_create(&th, NULL, waker, &w);
    int64_t t0 = now_ms();
    int n = zell__plat_poller_wait(p, ev, 4, 5000);
    assert(n == 1 && ev[0].token == ZELL_PLAT_WAKE_TOKEN && now_ms() - t0 < 4000);
    pthread_join(th, NULL);
    /* wake-ups coalesce and are drained */
    for (int i = 0; i < 100; i++) zell__plat_poller_wake(p);
    assert(zell__plat_poller_wait(p, ev, 4, 1000) == 1);
    assert(zell__plat_poller_wait(p, ev, 4, 10) == 0);
#if !defined(_WIN32)
    /* a signal (EINTR) returns 0 events instead of failing */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;  /* no SA_RESTART */
    sigaction(SIGUSR1, &sa, NULL);
    w.mode = 1;
    pthread_create(&th, NULL, waker, &w);
    t0 = now_ms();
    n = zell__plat_poller_wait(p, ev, 4, 5000);
    assert(n == 0 && now_ms() - t0 < 4000);
    pthread_join(th, NULL);
    signal(SIGUSR1, SIG_DFL);
#endif
    zell__plat_poller_free(p);
}

/* ---- sharded waits ---- */

TEST(io_shards_balance) {
    enum { N = 64 };
    int sv[N][2];
    for (int i = 0; i < N; i++) {
        pair(sv[i]);
        zell_io_register(sv[i][0]);
    }
    int64_t ns = zell__io_shards();
    assert(ns >= 1);
    int64_t count[64] = { 0 };
    for (int i = 0; i < N; i++) {
        int64_t s = zell__io_shard_of(sv[i][0]);
        assert(s >= 0 && s < ns);
        count[s]++;
    }
    for (int64_t s = 0; s < ns; s++) assert(count[s] >= N / ns - 1 && count[s] <= N / ns + 2);
    for (int i = 0; i < N; i++) {
        zell_io_forget(sv[i][0]);
        assert(zell__io_shard_of(sv[i][0]) == -1);
        sock_close(sv[i][0]);
        sock_close(sv[i][1]);
    }
}

TEST(io_two_readers_and_timeouts) {
    int sv[2];
    pair(sv);
    zell_task *a = zell_io_wait_readable(sv[0], -1);
    zell_task *b = zell_io_wait_readable(sv[0], 5000);
    zell_task *c = zell_io_wait_readable(sv[0], 20);   /* times out first */
    assert(wait_code(c) == 1);
    assert(!zell_task_is_done(a) && !zell_task_is_done(b));
    assert(sock_write(sv[1], "x", 1) == 1);
    assert(wait_code(a) == 0 && wait_code(b) == 0);      /* every waiter is woken */
    /* deadlines in any order complete in deadline order */
    char buf[4];
    assert(sock_read(sv[0], buf, 4) == 1);
    zell_task *t[5];
    int64_t ms[5] = { 80, 20, 60, 40, 10 };
    for (int i = 0; i < 5; i++) t[i] = zell_io_wait_readable(sv[0], ms[i]);
    int64_t t0 = now_ms();
    for (int i = 0; i < 5; i++) {
        assert(wait_code(t[i]) == 1);
        assert(now_ms() - t0 >= ms[i] - 1);
    }
    zell_io_forget(sv[0]);
    sock_close(sv[0]);
    sock_close(sv[1]);
}

typedef struct stress_arg {
    int id;
    int rounds;
    int64_t done;
} stress_arg;

/* Each thread: socketpairs whose waits are completed by writes from the
 * peer thread, by forget/close racing with new waits, and by timeouts. */
static void *stress_thread(void *a) {
    stress_arg *s = a;
    for (int r = 0; r < s->rounds; r++) {
        int sv[2];
        pair(sv);
        zell_task *rd = zell_io_wait_readable(sv[0], 2000);
        zell_task *wr = zell_io_wait_writable(sv[1], 2000);
        assert(wait_code(wr) == 0);
        assert(sock_write(sv[1], "z", 1) == 1);
        assert(wait_code(rd) == 0);
        /* close race: a wait on a descriptor forgotten right away */
        zell_task *late = zell_io_wait_readable(sv[1], 2000);
        zell_io_forget(sv[1]);
        int64_t code = wait_code(late);
        assert(code == 2 || code == 0);
        sock_close(sv[1]);
        zell_io_forget(sv[0]);
        sock_close(sv[0]);
        s->done++;
    }
    return NULL;
}

TEST(io_multithread_stress) {
    enum { T = 8 };
    pthread_t th[T];
    stress_arg args[T];
    for (int i = 0; i < T; i++) {
        args[i] = (stress_arg){ i, 300, 0 };
        pthread_create(&th[i], NULL, stress_thread, &args[i]);
    }
    for (int i = 0; i < T; i++) {
        pthread_join(th[i], NULL);
        assert(args[i].done == 300);
    }
}

/* Tasks on worker threads wake each other through the pollers. */
static void ping_worker(zell_task *t, void *ctx) {
    int *sv = ctx;
    for (int i = 0; i < 200; i++) {
        assert(sock_write(sv[1], "p", 1) == 1);
        char c;
        while (sock_read(sv[1], &c, 1) != 1) assert(wait_code(zell_io_wait_readable(sv[1], 5000)) == 0);
    }
    zell_task_complete(t, NULL);
}

static void pong_worker(zell_task *t, void *ctx) {
    int *sv = ctx;
    for (int i = 0; i < 200; i++) {
        char c;
        while (sock_read(sv[0], &c, 1) != 1) assert(wait_code(zell_io_wait_readable(sv[0], 5000)) == 0);
        assert(sock_write(sv[0], "q", 1) == 1);
    }
    zell_task_complete(t, NULL);
}

/* A short read drains the socket; the next wait (no recv in between, as
 * net.sock.readInto does) must still see data that arrives before it is
 * parked (edge-triggered epoll keeps that report in the fd's ready bits)
 * or after (the report wakes the parked wait). */
typedef struct late_writer {
    int fd;
    int n;
} late_writer;

static void *late_write_thread(void *arg) {
    late_writer *lw = arg;
    for (int i = 0; i < lw->n; i++) {
        if (i % 3 == 1) zell__plat_sleep_ns(50000);   /* the reader is parked by now, mostly */
        assert(sock_write(lw->fd, "abc", 3) == 3);
        char c;
        while (sock_read(lw->fd, &c, 1) != 1) zell__plat_sleep_ns(10000);   /* wait for the ack */
    }
    return NULL;
}

TEST(io_short_read_then_wait) {
    int sv[2];
    pair(sv);
    char buf[64];
    /* data arrives after the short read and before the wait: completes at once */
    assert(sock_write(sv[1], "ab", 2) == 2);
    assert(wait_code(zell_io_wait_readable(sv[0], 5000)) == 0);
    assert(sock_read(sv[0], buf, sizeof buf) == 2);   /* short: drained */
    assert(sock_write(sv[1], "cd", 2) == 2);
    zell__plat_sleep_ns(20000000);                     /* the poller has seen it, nobody waited */
    assert(wait_code(zell_io_wait_readable(sv[0], 5000)) == 0);
    assert(sock_read(sv[0], buf, sizeof buf) == 2);
    /* data arrives after the wait is parked: wakes it */
    zell_task *t = zell_io_wait_readable(sv[0], 5000);
    zell__plat_sleep_ns(20000000);
    assert(!zell_task_is_done(t));
    assert(sock_write(sv[1], "e", 1) == 1);
    assert(wait_code(t) == 0);
    assert(sock_read(sv[0], buf, sizeof buf) == 1);
    /* racing writer: a short read, then a wait without a recv, many times
     * (a lost wake-up shows as a timeout, code 1) */
    late_writer lw = { sv[1], 300 };
    pthread_t th;
    pthread_create(&th, NULL, late_write_thread, &lw);
    for (int i = 0; i < lw.n; i++) {
        int64_t got = 0;
        while (got < 3) {   /* the 3 bytes may come in pieces; every read before is short */
            assert(wait_code(zell_io_wait_readable(sv[0], 5000)) == 0);
            int64_t k = sock_read(sv[0], buf, sizeof buf);   /* < 0: a stale report, wait again */
            if (k > 0) got += k;
        }
        assert(got == 3);
        assert(sock_write(sv[0], "k", 1) == 1);
    }
    pthread_join(th, NULL);
    /* the last data and the peer's shutdown arrive together: the short read
     * returns the data, not the EOF behind it; the next wait must not park */
    assert(sock_write(sv[1], "zz", 2) == 2);
    assert(zell__plat_sock_shutdown(sv[1], ZELL_PLAT_WRITE) == 0);
    zell__plat_sleep_ns(20000000);
    assert(wait_code(zell_io_wait_readable(sv[0], 5000)) == 0);
    assert(sock_read(sv[0], buf, sizeof buf) == 2);
    assert(wait_code(zell_io_wait_readable(sv[0], 2000)) == 0);
    assert(sock_read(sv[0], buf, sizeof buf) == 0);
    assert(wait_code(zell_io_wait_readable(sv[0], 2000)) == 0);   /* stays readable */
    zell_io_forget(sv[0]);
    sock_close(sv[0]);
    sock_close(sv[1]);
}

TEST(io_worker_ping_pong) {
    int sv[2];
    pair(sv);
    zell_task *a = zell_task_spawn(ping_worker, sv);
    zell_task *b = zell_task_spawn(pong_worker, sv);
    zell_task_await(a);
    zell_task_await(b);
    zell_release(a);
    zell_release(b);
    zell_io_forget(sv[0]);
    zell_io_forget(sv[1]);
    sock_close(sv[0]);
    sock_close(sv[1]);
}

TEST(io_cancel_wakes_wait) {
    int sv[2];
    pair(sv);
    zell_task *scope = zell_task_new_scope();
    zell_task *prev = zell_task_enter_scope(scope);
    zell_task *w = zell_io_wait_readable(sv[0], -1);
    zell_task_leave_scope(prev);
    assert(!zell_task_is_done(w));
    zell_task_cancel(scope);
    assert(wait_code(w) == 3);
    /* a wait started under a cancelled task completes at once */
    prev = zell_task_enter_scope(scope);
    w = zell_io_wait_readable(sv[0], -1);
    zell_task_leave_scope(prev);
    assert(wait_code(w) == 3);
    zell_release(scope);
    zell_io_forget(sv[0]);
    sock_close(sv[0]);
    sock_close(sv[1]);
}

static void pending_at_shutdown(void) {
    int64_t live = zell_live_objects();
    zell_runtime_init(0, NULL);
    int sv[2];
    pair(sv);
    zell_task *w = zell_io_wait_readable(sv[0], -1);
    zell_task *w2 = zell_io_wait_readable(sv[0], 60000);
    zell_release(w);
    zell_release(w2);
    zell_runtime_shutdown();  /* must not hang; the waits are dropped */
    if (zell_live_objects() != live) _exit(3);
    _exit(0);
}

TEST(io_shutdown_with_pending_waits) {
    char buf[256];
    assert(run_child(pending_at_shutdown, buf, sizeof buf) == 0);
}

/* ---- socket handles ---- */

static int64_t listen_lo(void) {
    zell_error *err = NULL;
    zell_string *lo = S("127.0.0.1");
    int64_t l = zell_net_rt_listen(lo, 0, 0, 0, &err);
    assert(!err && l != -1);
    zell_release(lo);
    return l;
}

static int64_t dial_lo(int64_t port) {
    zell_error *err = NULL;
    zell_string *lo = S("127.0.0.1"), *none = S("");
    int64_t c = zell_net_rt_dial(lo, port, none, 0, &err);
    assert(!err && c != -1);
    assert(wait_code(zell_net_rt_wait_writable(c, 2000)) == 0);
    zell_net_rt_connect_result(c, &err);
    assert(!err);
    zell_release(lo);
    zell_release(none);
    return c;
}

static int64_t accept_one(int64_t l) {
    zell_error *err = NULL;
    int64_t a;
    while ((a = zell_net_rt_accept(l, &err)) == -1) {
        assert(!err);
        assert(wait_code(zell_net_rt_wait_readable(l, 2000)) == 0);
    }
    return a;
}

TEST(net_round_trip_and_close) {
    zell_error *err = NULL;
    int64_t l = listen_lo();
    int64_t port = zell_net_rt_port(l);
    assert(port > 0);
    int64_t c = dial_lo(port);
    int64_t a = accept_one(l);
    zell_string *ra = zell_net_rt_remote_addr(a);
    assert(strncmp(zell_string_data(ra), "127.0.0.1:", 10) == 0);
    zell_release(ra);
    zell_string *msg = S("ping");
    assert(zell_net_rt_write_str(c, msg, 0, &err) == 4 && !err);
    zell_slice buf;
    zell_slice_new(&buf, 1, 16, 16);
    int64_t k;
    while ((k = zell_net_rt_read_into(a, &buf, 0, &err)) == -1)
        assert(wait_code(zell_net_rt_wait_readable(a, 2000)) == 0);
    assert(!err && k == 4 && memcmp(buf.data, "ping", 4) == 0);
    /* a reader waiting when the socket is closed is woken with 2 */
    zell_task *w = zell_net_rt_wait_readable(a, -1);
    zell_net_rt_close(a);
    zell_net_rt_close(a);  /* double close: no-op */
    assert(wait_code(w) == 2);
    assert(zell_net_rt_is_closed(a) && zell_net_rt_fd(a) == -1);
    /* every operation on a closed handle fails with ErrClosed */
    assert(zell_net_rt_read_into(a, &buf, 0, &err) == -1 && err == zell_net_err_closed());
    err = NULL;
    assert(zell_net_rt_write_str(a, msg, 0, &err) == -1 && err == zell_net_err_closed());
    err = NULL;
    assert(wait_code(zell_net_rt_wait_writable(a, -1)) == 2);
    /* the peer address survives Close */
    ra = zell_net_rt_remote_addr(a);
    assert(strncmp(zell_string_data(ra), "127.0.0.1:", 10) == 0);
    zell_release(ra);
    /* the other end sees end of stream, then a write fails with ErrReset */
    while ((k = zell_net_rt_read_into(c, &buf, 0, &err)) == -1)
        assert(wait_code(zell_net_rt_wait_readable(c, 2000)) == 0);
    assert(k == 0 && !err);
    for (int i = 0; i < 100 && !err; i++) {
        zell_net_rt_write_str(c, msg, 0, &err);
        if (!err) { struct timespec ts = { 0, 2000000 }; nanosleep(&ts, NULL); }
    }
    assert(err && zell_error_is(err, zell_net_err_reset()));
    zell_release(err);
    err = NULL;
    zell_net_rt_free(a);
    zell_net_rt_free(c);
    /* refused: the sentinel */
    zell_net_rt_close(l);
    zell_string *lo = S("127.0.0.1"), *none = S("");
    int64_t c2 = zell_net_rt_dial(lo, port, none, 0, &err);
    if (!err) {
        assert(wait_code(zell_net_rt_wait_writable(c2, 2000)) == 0);
        zell_net_rt_connect_result(c2, &err);
        zell_net_rt_free(c2);
    }
    assert(err == zell_net_err_refused());
    err = NULL;
    /* accept on a closed listener */
    assert(zell_net_rt_accept(l, &err) == -1 && err == zell_net_err_closed());
    err = NULL;
    zell_net_rt_free(l);
    zell_slice_release(&buf);
    zell_release(msg);
    zell_release(lo);
    zell_release(none);
}

typedef struct race_arg {
    int64_t h;
    int64_t ops;  /* atomic */
} race_arg;

static void *hammer(void *p) {
    race_arg *r = p;
    zell_slice buf;
    zell_slice_new(&buf, 1, 8, 8);
    for (;;) {
        zell_error *err = NULL;
        zell_net_rt_read_into(r->h, &buf, 0, &err);
        zell_release(err);
        zell_task *w = zell_net_rt_wait_readable(r->h, 1);
        int64_t code = wait_code(w);
        assert(code <= 2);
        __atomic_add_fetch(&r->ops, 1, __ATOMIC_RELAXED);
        if (zell_net_rt_is_closed(r->h)) break;
    }
    zell_slice_release(&buf);
    return NULL;
}

/* Close while other threads read and wait on the same handle: no wait is
 * lost, the descriptor is closed exactly once, after the last operation. */
TEST(net_close_race) {
    for (int round = 0; round < 50; round++) {
        int64_t l = listen_lo();
        int64_t c = dial_lo(zell_net_rt_port(l));
        int64_t a = accept_one(l);
        race_arg r = { a, 0 };
        pthread_t th[4];
        for (int i = 0; i < 4; i++) pthread_create(&th[i], NULL, hammer, &r);
        struct timespec ts = { 0, 200000 };
        nanosleep(&ts, NULL);
        zell_net_rt_close(a);
        for (int i = 0; i < 4; i++) pthread_join(th[i], NULL);
        zell_net_rt_free(a);
        zell_net_rt_free(c);
        zell_net_rt_free(l);
    }
}

TEST(net_unix_udp_sendfile) {
    zell_error *err = NULL;
    char path[512];
    snprintf(path, sizeof path, "%s/zell-test-%d.sock", tmp_dir(), (int)getpid());
    zell_string *ps = S(path);
    int64_t l = zell_net_rt_listen_unix(ps, 0, &err);
    bool unix_ok = true;
#if defined(_WIN32)
    if (err && msg_has(err, "address family not supported")) {
        /* Wine (9.0) has no AF_UNIX; Windows 10 1803+ does: sendfile over TCP */
        printf("  (AF_UNIX unsupported here: sendfile over loopback TCP)\n");
        zell_release(err);
        err = NULL;
        unix_ok = false;
        l = listen_lo();
    }
#endif
    assert(!err && l != -1);
    if (unix_ok) assert(access(path, F_OK) == 0);
    int64_t c = unix_ok ? zell_net_rt_dial_unix(ps, &err) : dial_lo(zell_net_rt_port(l));
    assert(!err && c != -1);
    int64_t a = accept_one(l);
    /* sendfile from a temp file */
    char fpath[512];
    snprintf(fpath, sizeof fpath, "%s/zell-test-%d.dat", tmp_dir(), (int)getpid());
    FILE *f = fopen(fpath, "wb");
    for (int i = 0; i < 10000; i++) fputc('a' + i % 26, f);
    fclose(f);
    zell_string *fp = S(fpath);
    int64_t ffd = zell_net_rt_open_file(fp, &err);
    assert(!err && zell_net_rt_file_size(ffd) == 10000);
    /* Send and receive together: a Unix socket's buffer can be smaller
     * than the file (8 KB on macOS), so the sender waits for the reader. */
    zell_slice buf;
    zell_slice_new(&buf, 1, 20000, 20000);
    int64_t off = 0, got = 0;
    while (off < 10000 || got < 10000) {
        int progress = 0;
        if (off < 10000) {
            int64_t k = zell_net_rt_send_file(c, ffd, off, 10000 - off, &err);
            assert(!err);
            if (k > 0) off += k, progress = 1;
        }
        if (got < 10000) {
            int64_t k = zell_net_rt_read_into(a, &buf, got, &err);
            assert(!err && k != 0);
            if (k > 0) got += k, progress = 1;
        }
        if (!progress) {
            if (got < off) assert(wait_code(zell_net_rt_wait_readable(a, 2000)) == 0);
            else assert(wait_code(zell_net_rt_wait_writable(c, 2000)) == 0);
        }
    }
    zell_net_rt_close_file(ffd);
    unlink(fpath);
    assert(((char *)buf.data)[9999] == 'a' + 9999 % 26);
    zell_net_rt_free(a);
    zell_net_rt_free(c);
    zell_net_rt_close(l);
    if (unix_ok) assert(access(path, F_OK) != 0);  /* the listener removed its file */
    zell_net_rt_free(l);
    zell_release(ps);
    zell_release(fp);
    /* UDP: truncation is reported */
    zell_string *lo = S("127.0.0.1");
    int64_t u1 = zell_net_rt_udp(lo, 0, 0, &err), u2 = zell_net_rt_udp(lo, 0, 0, &err);
    assert(!err);
    zell_slice data;
    zell_slice_new(&data, 1, 100, 100);
    memset(data.data, 'u', 100);
    assert(zell_net_rt_send_to(u1, &data, lo, zell_net_rt_port(u2), &err) == 100);
    zell_slice small;
    zell_slice_new(&small, 1, 10, 10);
    int64_t k;
    while ((k = zell_net_rt_read_from(u2, &small, &err)) == -1)
        assert(wait_code(zell_net_rt_wait_readable(u2, 2000)) == 0);
    assert(k == 10 && zell_net_rt_last_trunc());
    assert(zell_net_rt_last_from_port() == zell_net_rt_port(u1));
    zell_net_rt_free(u1);
    zell_net_rt_free(u2);
    zell_slice_release(&data);
    zell_slice_release(&small);
    zell_slice_release(&buf);
    zell_release(lo);
}

TEST(net_addresses_and_options) {
    zell_string *s = S("2001:db8::1");
    zell_slice b;
    zell_net_rt_parse_ip(s, &b);
    assert(b.len == 16);
    zell_string *t = zell_net_rt_format_ip(&b);
    assert(STR_IS(t, "2001:db8::1"));
    zell_release(t);
    zell_slice_release(&b);
    zell_release(s);
    s = S("300.1.1.1");
    zell_net_rt_parse_ip(s, &b);
    assert(b.len == 0);
    zell_release(s);
    unsigned char ip4[4] = { 10, 0, 0, 1 };
    zell__plat_addr a;
    assert(zell__plat_addr_ip(&a, ip4, 4, 8080, 0));
    char buf[128];
    zell__plat_addr_format(&a, buf, sizeof buf);
    assert(strcmp(buf, "10.0.0.1:8080") == 0);
    unsigned char ip6[16] = { 0xfe, 0x80, [15] = 1 };
    assert(zell__plat_addr_ip(&a, ip6, 16, 80, 1));
    zell__plat_addr_format(&a, buf, sizeof buf);
    assert(strncmp(buf, "[fe80::1%", 9) == 0);
    assert(zell__plat_addr_unix(&a, "/tmp/x.sock"));
    zell__plat_addr_format(&a, buf, sizeof buf);
    assert(strcmp(buf, "/tmp/x.sock") == 0);
#if defined(__linux__)
    assert(zell__plat_addr_unix(&a, "@abstract"));
    zell__plat_addr_format(&a, buf, sizeof buf);
    assert(strcmp(buf, "@abstract") == 0);
#endif
#if defined(_WIN32)
    enum { E_REFUSED = 10061 /* WSAECONNREFUSED */, E_RESET = 10054 /* WSAECONNRESET */ };
#else
    enum { E_REFUSED = ECONNREFUSED, E_RESET = EPIPE };
#endif
    assert(zell__plat_err_class(E_REFUSED) == ZELL_PLAT_E_REFUSED);
    assert(zell__plat_err_class(E_RESET) == ZELL_PLAT_E_RESET);
    char eb[64];
    assert(strcmp(zell__plat_strerror(E_REFUSED, eb, sizeof eb), "connection refused") == 0);
    /* options round-trip */
    int64_t l = listen_lo();
    int64_t c = dial_lo(zell_net_rt_port(l));
    zell_error *err = NULL;
    zell_net_rt_set_opt(c, ZELL_PLAT_OPT_NODELAY, 0, &err);
    assert(!err && zell_net_rt_get_opt(c, ZELL_PLAT_OPT_NODELAY, &err) == 0);
    zell_net_rt_set_opt(c, ZELL_PLAT_OPT_KEEPALIVE, 1, &err);
    assert(!err && zell_net_rt_get_opt(c, ZELL_PLAT_OPT_KEEPALIVE, &err) != 0);
    zell_net_rt_set_opt(c, ZELL_PLAT_OPT_RCVBUF, 65536, &err);
    assert(!err && zell_net_rt_get_opt(c, ZELL_PLAT_OPT_RCVBUF, &err) >= 65536);
    zell_net_rt_set_opt(c, ZELL_PLAT_OPT_LINGER, 0, &err);
    assert(!err && zell_net_rt_get_opt(c, ZELL_PLAT_OPT_LINGER, &err) == 0);
    zell_net_rt_free(c);
    zell_net_rt_free(l);
    /* address in use */
    zell_string *lo = S("127.0.0.1");
    int64_t l1 = zell_net_rt_listen(lo, 0, 0, 1 /* no SO_REUSEADDR */, &err);
    assert(!err);
    int64_t l2 = zell_net_rt_listen(lo, zell_net_rt_port(l1), 0, 1, &err);
    assert(l2 == -1 && err == zell_net_err_addr_in_use());
    err = NULL;
#if !defined(_WIN32)
    /* SO_REUSEPORT: two listeners share the port */
    int64_t r1 = zell_net_rt_listen(lo, 0, 0, 2, &err);
    assert(!err);
    int64_t r2 = zell_net_rt_listen(lo, zell_net_rt_port(r1), 0, 2, &err);
    assert(!err && r2 != -1);
    zell_net_rt_free(r1);
    zell_net_rt_free(r2);
#else
    /* Windows has no SO_REUSEPORT: the listen fails instead of sharing */
    int64_t r1 = zell_net_rt_listen(lo, 0, 0, 2, &err);
    assert(r1 == -1 && err);
    zell_release(err);
    err = NULL;
#endif
    zell_net_rt_free(l1);
    zell_release(lo);
}

/* ---- the resolver pool ---- */

TEST(resolver_pool) {
    enum { N = 32 };
    zell_string *h = S("localhost");
    zell_task *ts[N];
    for (int i = 0; i < N; i++) ts[i] = zell_net_rt_resolve(h, 1);
    for (int i = 0; i < N; i++) {
        int64_t hnd = *(int64_t *)zell_task_await(ts[i]);
        zell_release(ts[i]);
        zell_slice out;
        zell_error *err = NULL;
        zell_net_rt_resolve_take(hnd, &out, &err);
        assert(!err && out.len >= 1);
        assert(STR_IS(*(zell_string **)zell_slice_index(&out, 0, 8), "127.0.0.1"));
        zell_slice_release(&out);
    }
    zell_release(h);
    /* a name that does not exist wraps ErrNotFound */
    h = S("nosuchhost.invalid");
    zell_task *t = zell_net_rt_resolve(h, 0);
    int64_t hnd = *(int64_t *)zell_task_await(t);
    zell_release(t);
    zell_slice out;
    zell_error *err = NULL;
    zell_net_rt_resolve_take(hnd, &out, &err);
    assert(err && msg_has(err, "lookup nosuchhost.invalid"));
    assert(zell_error_is(err, zell_net_err_not_found()));
    zell_release(err);
    zell_release(h);
    /* literals complete at once, zones included */
    h = S("fe80::1%1");
    t = zell_net_rt_resolve(h, 0);
    assert(zell_task_is_done(t));
    hnd = *(int64_t *)zell_task_result(t);
    zell_release(t);
    err = NULL;
    zell_net_rt_resolve_take(hnd, &out, &err);
    assert(!err && out.len == 1);
    zell_slice_release(&out);
    zell_release(h);
    h = S("http");
    zell_string *tcp = S("tcp");
    int64_t port = zell_net_rt_lookup_port(tcp, h);
    assert(port == 80 || port == -1);  /* -1 without /etc/services */
    zell_release(h);
    zell_release(tcp);
}

int main(int argc, char **argv) {
    printf("test_net\n");
    {
        zell__plat_poller *p = zell__plat_poller_new();
        printf("  poller backend: %s\n", zell__plat_poller_backend(p));
        zell__plat_poller_free(p);
    }
    setenv("ZELL_WORKERS", "4", 1);
    setenv("ZELL_IO_POLLERS", "3", 1);
    int64_t live = zell_live_objects();
    RUN(poller_oneshot_level);
    RUN(poller_wake_and_eintr);
    RUN(io_shutdown_with_pending_waits);
    zell_runtime_init(argc, argv);
    RUN(io_shards_balance);
    RUN(io_two_readers_and_timeouts);
    RUN(io_multithread_stress);
    RUN(io_worker_ping_pong);
    RUN(io_short_read_then_wait);
    RUN(io_cancel_wakes_wait);
    RUN(net_round_trip_and_close);
    RUN(net_close_race);
    RUN(net_unix_udp_sendfile);
    RUN(net_addresses_and_options);
    RUN(resolver_pool);
    zell_runtime_shutdown();
    assert(zell_live_objects() == live);
    return 0;
}
