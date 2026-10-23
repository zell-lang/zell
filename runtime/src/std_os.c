/* std_os.c - os (zell_os_*). Args is zell_os_args (main.c). Failing
 * calls report "<op> <arg>: <strerror>" errors through the `T!` out-param. */
#include "rt_internal.h"
#include "platform/platform.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void set_err(zell_error **err, const char *op, const char *arg, int e) {
    if (!err) return;
    char buf[512];
    if (arg) snprintf(buf, sizeof buf, "%s %.400s: %s", op, arg, strerror(e));
    else snprintf(buf, sizeof buf, "%s: %s", op, strerror(e));
    *err = zell_error_new_cstr(buf);
}

/* Zell strings may contain NUL bytes; C APIs cannot take them. */
static bool has_nul(const zell_string *s) {
    return (int64_t)strlen(zell_string_cstr(s)) != zell_string_len(s);
}

zell_string *zell_os_getenv(const zell_string *name) {
    char *v = has_nul(name) ? NULL : zell__plat_getenv(zell_string_cstr(name));
    zell_string *r = zell_string_from_cstr(v ? v : "");
    free(v);
    return r;
}

void zell_os_setenv(const zell_string *name, const zell_string *value, zell_error **err) {
    const char *n = zell_string_cstr(name);
    if (zell_string_len(name) == 0 || strchr(n, '=') || has_nul(name) || has_nul(value)) {
        set_err(err, "setenv", n, EINVAL);
        return;
    }
    int e = zell__plat_setenv(n, zell_string_cstr(value));
    if (e) set_err(err, "setenv", n, e);
}

void zell_os_environ(zell_slice *out) {
    zell_slice_new_ops(out, 8, 0, 8, &zell_ops_ref);
    char **env = zell__plat_environ();
    if (!env) return;
    for (char **e = env; *e; e++) {
        zell_string *s = zell_string_from_cstr(*e);
        zell_slice_append(out, 8, &zell_ops_ref, &s);
        zell_release(s);
    }
    zell__plat_free_strv(env);
}

void zell_os_exit(int64_t code) { zell_exit((int32_t)code); }

zell_string *zell_os_hostname(zell_error **err) {
    char buf[300];
    int e = zell__plat_hostname(buf, sizeof buf);
    if (e) {
        set_err(err, "hostname", NULL, e);
        return NULL;
    }
    return zell_string_from_cstr(buf);
}

int64_t zell_os_pid(void) { return zell__plat_pid(); }

zell_string *zell_os_cwd(zell_error **err) {
    int e = 0;
    char *p = zell__plat_getcwd(&e);
    if (!p) {
        set_err(err, "getwd", NULL, e);
        return NULL;
    }
    zell_string *r = zell_string_from_cstr(p);
    free(p);
    return r;
}

void zell_os_chdir(const zell_string *path, zell_error **err) {
    if (has_nul(path)) { set_err(err, "chdir", zell_string_cstr(path), EINVAL); return; }
    int e = zell__plat_chdir(zell_string_cstr(path));
    if (e) set_err(err, "chdir", zell_string_cstr(path), e);
}
