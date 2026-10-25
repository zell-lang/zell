/* win32_util.h - helpers shared by the Windows files of the platform layer
 * (win32.c, fs_win32.c, proc_win32.c, net_win32.c, poll_iocp.c,
 * poll_wsapoll.c, prof_win32.c, tz_win32.c, certroots.c, sqlite_win32.c). Only
 * included on _WIN32. */
#ifndef ZELL_WIN32_UTIL_H
#define ZELL_WIN32_UTIL_H

#if !defined(_WIN32)
#error "win32_util.h is Windows-only"
#endif

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>

#include "platform.h"

/* malloc'd UTF-16 copy of a UTF-8 string (NULL on invalid input or OOM). */
ZELL_PLAT_HIDDEN wchar_t *zell__w32_wide(const char *s);
/* The same for a path: '/' becomes '\\' (Win32 accepts both, but the
 * \\?\ form and some shell APIs do not). */
ZELL_PLAT_HIDDEN wchar_t *zell__w32_wpath(const char *s);
/* malloc'd UTF-8 copy of a UTF-16 string (n = -1: NUL-terminated). */
ZELL_PLAT_HIDDEN char *zell__w32_utf8(const wchar_t *w, int n);
/* POSIX errno of a GetLastError() code. */
ZELL_PLAT_HIDDEN int zell__w32_errno(DWORD err);
/* Writes UTF-8 text to a console handle with WriteConsoleW; carry holds an
 * incomplete trailing sequence between calls (4 bytes + length byte). */
ZELL_PLAT_HIDDEN void zell__w32_console_write(HANDLE h, unsigned char carry[5], const char *p, size_t n);
/* True if CRT descriptor fd (1 or 2) is a console (set at runtime init). */
ZELL_PLAT_HIDDEN bool zell__w32_is_console_fd(int64_t fd);
/* Console writer for fd 1/2 (shares the carry buffer with stdio output). */
ZELL_PLAT_HIDDEN void zell__w32_console_fd_write(int64_t fd, const char *p, size_t n);
/* WSAStartup once. */
ZELL_PLAT_HIDDEN void zell__w32_wsa_init(void);
/* A connected pair of loopback TCP sockets (not inherited); 0 or a WSA error. */
ZELL_PLAT_HIDDEN int zell__w32_socketpair(SOCKET sv[2]);

/* Child-process pipes of proc_win32.c for poll_iocp.c: the parent's handle
 * of pipe descriptor fd and its direction (false: not such a pipe); whether
 * its write is still in flight; whether ov is the OVERLAPPED of a pipe's
 * write (and which descriptor). */
ZELL_PLAT_HIDDEN bool zell__w32_pipe_of(int64_t fd, HANDLE *h, bool *write);
ZELL_PLAT_HIDDEN bool zell__w32_pipe_write_busy(int64_t fd);
ZELL_PLAT_HIDDEN bool zell__w32_pipe_write_ov(const void *ov, int64_t *fd);
/* True if the pollers are IOCP ones, which can wait on named pipes
 * (proc_win32.c then uses them instead of relay threads). */
ZELL_PLAT_HIDDEN bool zell__w32_poller_waits_pipes(void);

/* The WSAPoll poller (poll_wsapoll.c), the fallback of poll_iocp.c; same
 * contract as zell__plat_poller_*. */
typedef struct zell__w32_wsapoll zell__w32_wsapoll;
ZELL_PLAT_HIDDEN zell__w32_wsapoll *zell__w32_wsapoll_new(void);
ZELL_PLAT_HIDDEN void zell__w32_wsapoll_free(zell__w32_wsapoll *p);
ZELL_PLAT_HIDDEN int zell__w32_wsapoll_arm(zell__w32_wsapoll *p, int64_t fd, int32_t interest, uint64_t token, bool registered);
ZELL_PLAT_HIDDEN void zell__w32_wsapoll_remove(zell__w32_wsapoll *p, int64_t fd);
ZELL_PLAT_HIDDEN int zell__w32_wsapoll_wait(zell__w32_wsapoll *p, zell__plat_event *evs, int max, int64_t timeout_ms);
ZELL_PLAT_HIDDEN void zell__w32_wsapoll_wake(zell__w32_wsapoll *p);

/* Walks the stack from `ctx` (a CONTEXT, unwound in place) while it stays
 * in [lo, hi): the context's PC first, then return addresses; returns the
 * number stored (prof_win32.c; the CPU sampler and the fault handler). */
ZELL_PLAT_HIDDEN int zell__w32_walk(void *ctx, uintptr_t lo, uintptr_t hi, uintptr_t *pcs, int max);
/* Reserves stack for the fault handler of a stack overflow on this thread
 * (win32.c; called from zell__plat_thread_stack_init). */
ZELL_PLAT_HIDDEN void zell__w32_fatal_thread_init(void);

#endif
