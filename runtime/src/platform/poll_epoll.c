/* poll_epoll.c - readiness poller on Linux (platform.h): edge-triggered
 * epoll (EPOLLET), each fd added once for both directions and never
 * re-armed, so a wait costs no epoll_ctl (task_io.c keeps the readiness
 * that arrived while nobody waited), plus an eventfd for cross-thread
 * wake-ups; or io_uring readiness polls (poll_uring.c, one-shot) with
 * ZELL_IO_BACKEND=io_uring when the kernel has them. */
#include "platform.h"

#if defined(__linux__)

#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>

/* io_uring backend (poll_uring.c): used instead of epoll when available. */
typedef struct zell__uring zell__uring;
zell__uring *zell__uring_new(void);
void zell__uring_free(zell__uring *u);
int zell__uring_arm(zell__uring *u, int64_t fd, int32_t interest, uint64_t token, bool registered);
void zell__uring_remove(zell__uring *u, int64_t fd);
int zell__uring_wait(zell__uring *u, zell__plat_event *evs, int max, int64_t timeout_ms);
void zell__uring_wake(zell__uring *u);

struct zell__plat_poller {
    int epfd;
    int wakefd;
    zell__uring *u;  /* non-NULL: every call goes to poll_uring.c */
};

zell__plat_poller *zell__plat_poller_new(void) {
    zell__plat_poller *p = malloc(sizeof *p);
    if (!p) return NULL;
    p->u = zell__uring_new();
    if (p->u) {
        p->epfd = p->wakefd = -1;
        return p;
    }
    p->epfd = epoll_create1(EPOLL_CLOEXEC);
    p->wakefd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    if (p->epfd < 0 || p->wakefd < 0) {
        if (p->epfd >= 0) close(p->epfd);
        if (p->wakefd >= 0) close(p->wakefd);
        free(p);
        return NULL;
    }
    /* The wake-up channel stays armed (level-triggered, not one-shot). */
    struct epoll_event ev = { .events = EPOLLIN, .data.u64 = ZELL_PLAT_WAKE_TOKEN };
    if (epoll_ctl(p->epfd, EPOLL_CTL_ADD, p->wakefd, &ev) != 0) {
        close(p->epfd);
        close(p->wakefd);
        free(p);
        return NULL;
    }
    return p;
}

void zell__plat_poller_free(zell__plat_poller *p) {
    if (!p) return;
    if (p->u) {
        zell__uring_free(p->u);
        free(p);
        return;
    }
    close(p->epfd);
    close(p->wakefd);
    free(p);
}

int zell__plat_poller_arm(zell__plat_poller *p, int64_t fd, int32_t interest, uint64_t token, bool registered) {
    if (p->u) return zell__uring_arm(p->u, fd, interest, token, registered);
    /* Edge-triggered: both directions, whatever `interest` asks for. */
    (void)interest;
    struct epoll_event ev = { .events = EPOLLIN | EPOLLRDHUP | EPOLLOUT | EPOLLET, .data.u64 = token };
    int op = registered ? EPOLL_CTL_MOD : EPOLL_CTL_ADD;
    int rc = epoll_ctl(p->epfd, op, (int)fd, &ev);
    if (rc != 0 && errno == ENOENT && registered) rc = epoll_ctl(p->epfd, EPOLL_CTL_ADD, (int)fd, &ev);
    else if (rc != 0 && errno == EEXIST && !registered) rc = epoll_ctl(p->epfd, EPOLL_CTL_MOD, (int)fd, &ev);
    if (rc == 0) return 0;
    return errno == EPERM ? ZELL_PLAT_ENOTPOLLABLE : errno;
}

void zell__plat_poller_remove(zell__plat_poller *p, int64_t fd) {
    if (p->u) {
        zell__uring_remove(p->u, fd);
        return;
    }
    struct epoll_event ev = { 0 };
    epoll_ctl(p->epfd, EPOLL_CTL_DEL, (int)fd, &ev);
}

int zell__plat_poller_wait(zell__plat_poller *p, zell__plat_event *evs, int max, int64_t timeout_ms) {
    if (p->u) return zell__uring_wait(p->u, evs, max, timeout_ms);
    struct epoll_event buf[256];
    if (max > 256) max = 256;
    if (max < 1) return 0;
    int tmo = timeout_ms < 0 ? -1 : timeout_ms > 0x7fffffff ? 0x7fffffff : (int)timeout_ms;
    int n = epoll_wait(p->epfd, buf, max, tmo);
    if (n < 0) return 0;  /* EINTR (a signal): the caller loops */
    int out = 0;
    for (int i = 0; i < n; i++) {
        if (buf[i].data.u64 == ZELL_PLAT_WAKE_TOKEN) {
            uint64_t v;
            while (read(p->wakefd, &v, sizeof v) > 0) {}
            evs[out].token = ZELL_PLAT_WAKE_TOKEN;
            evs[out].events = 0;
            out++;
            continue;
        }
        int32_t e = 0;
        if (buf[i].events & (EPOLLIN | EPOLLRDHUP)) e |= ZELL_PLAT_READ;
        if (buf[i].events & EPOLLRDHUP) e |= ZELL_PLAT_HUP;
        if (buf[i].events & EPOLLOUT) e |= ZELL_PLAT_WRITE;
        if (buf[i].events & (EPOLLERR | EPOLLHUP)) e |= ZELL_PLAT_ERROR | ZELL_PLAT_READ | ZELL_PLAT_WRITE;
        evs[out].token = buf[i].data.u64;
        evs[out].events = e;
        out++;
    }
    return out;
}

void zell__plat_poller_wake(zell__plat_poller *p) {
    if (p->u) {
        zell__uring_wake(p->u);
        return;
    }
    uint64_t one = 1;
    for (;;) {
        ssize_t r = write(p->wakefd, &one, sizeof one);
        if (r >= 0 || errno != EINTR) break;  /* EAGAIN: counter saturated, already pending */
    }
}

bool zell__plat_poller_edge(zell__plat_poller *p) { return p->u == NULL; }

bool zell__plat_poller_shared(zell__plat_poller *p) { return p->u == NULL; }

int zell__plat_poller_link(zell__plat_poller *hub, zell__plat_poller *p, uint64_t token) {
    if (hub->u || p->u) return EOPNOTSUPP;
    /* An epoll instance is readable while its ready list is not empty:
     * level-triggered on the hub, so the hub keeps reporting p until
     * somebody drains it with a wait of timeout 0. */
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.u64 = token;
    return epoll_ctl(hub->epfd, EPOLL_CTL_ADD, p->epfd, &ev) == 0 ? 0 : errno;
}

const char *zell__plat_poller_backend(zell__plat_poller *p) {
    return p->u ? "io_uring" : "epoll";
}

#else
/* Not Linux: the poller comes from poll_kqueue.c. */
typedef int zell__poll_epoll_unused;
#endif
