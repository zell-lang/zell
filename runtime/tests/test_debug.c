/* Tests for the debug agent's C helpers (runtime/src/debug.c): how a
 * breakpoint path from the client is matched against the agent's file
 * table, with POSIX and Windows rules, and path canonicalisation. */
#include "test.h"
#include "../src/platform/platform.h"
#if !defined(_WIN32)
#include <sys/stat.h>
#endif

int zell__dbg_path_match(const char *file, const char *path, int fold);
zell_string *zell_debug_rt_path_canon(zell_string *path);

TEST(posix_rules) {
    assert(zell__dbg_path_match("/src/app/main.zl", "/src/app/main.zl", 0) == 3);
    assert(zell__dbg_path_match("/src/app/main.zl", "app/main.zl", 0) == 2);
    assert(zell__dbg_path_match("/src/app/main.zl", "main.zl", 0) == 2);
    /* A tail must start at a separator. */
    assert(zell__dbg_path_match("/src/app/main.zl", "p/main.zl", 0) == 1);
    assert(zell__dbg_path_match("/src/app/main.zl", "/other/main.zl", 0) == 1);
    assert(zell__dbg_path_match("/src/app/main.zl", "/src/app/Main.zl", 0) == 0);
    assert(zell__dbg_path_match("/src/app/main.zl", "/src/app/other.zl", 0) == 0);
    assert(zell__dbg_path_match("/src/app/main.zl", "", 0) == 0);
}

TEST(windows_rules) {
    const char *file = "C:\\Users\\dev\\app\\main.zl";
    assert(zell__dbg_path_match(file, "c:\\users\\DEV\\app\\main.zl", 1) == 3);
    assert(zell__dbg_path_match(file, "c:/Users/dev/app/main.zl", 1) == 3);
    assert(zell__dbg_path_match(file, "\\\\?\\C:\\Users\\dev\\app\\main.zl", 1) == 3);
    assert(zell__dbg_path_match("\\\\?\\C:\\Users\\dev\\app\\main.zl", "C:/USERS/dev/app/MAIN.ZL", 1) == 3);
    assert(zell__dbg_path_match(file, "App\\Main.zl", 1) == 2);
    assert(zell__dbg_path_match(file, "d:\\app\\main.zl", 1) == 1);
    /* Without the Windows rules the spellings differ. */
    assert(zell__dbg_path_match(file, "c:\\users\\DEV\\app\\main.zl", 0) == 0);
}

TEST(canon_resolves_symlinks) {
#if !defined(_WIN32)
    char dir[] = "/tmp/zell_dbg_XXXXXX";
    assert(mkdtemp(dir));
    char real[256], link[256], viaLink[300];
    snprintf(real, sizeof real, "%s/real", dir);
    snprintf(link, sizeof link, "%s/link", dir);
    assert(mkdir(real, 0700) == 0);
    char file[300];
    snprintf(file, sizeof file, "%s/main.zl", real);
    FILE *f = fopen(file, "w");
    assert(f);
    fclose(f);
    assert(symlink(real, link) == 0);
    snprintf(viaLink, sizeof viaLink, "%s/main.zl", link);
    char *want = zell__plat_path_canon(file);
    assert(want);
    zell_string *in = S(viaLink);
    zell_string *got = zell_debug_rt_path_canon(in);
    assert(STR_IS(got, want));
    assert(zell__dbg_path_match(want, zell_string_cstr(got), zell__plat_path_fold()) == 3);
    zell_release(got);
    zell_release(in);
    /* A path that does not exist comes back as it is. */
    zell_string *none = S("/no/such/dir/main.zl");
    zell_string *same = zell_debug_rt_path_canon(none);
    assert(STR_IS(same, "/no/such/dir/main.zl"));
    zell_release(same);
    zell_release(none);
    free(want);
    unlink(file);
    unlink(link);
    rmdir(real);
    rmdir(dir);
#else
    /* A `c:/...` lower-case, forward-slash spelling of a file names it. */
    char tmp[MAX_PATH], file[MAX_PATH + 32], other[MAX_PATH + 32];
    DWORD n = GetTempPathA(sizeof tmp, tmp);
    assert(n > 0 && n < sizeof tmp);
    snprintf(file, sizeof file, "%sZellDbgPath.zl", tmp);
    FILE *f = fopen(file, "w");
    assert(f);
    fclose(f);
    size_t i = 0;
    for (; file[i]; i++) other[i] = file[i] == '\\' ? '/' : (char)(file[i] >= 'A' && file[i] <= 'Z' ? file[i] - 'A' + 'a' : file[i]);
    other[i] = 0;
    char *a = zell__plat_path_canon(file), *b = zell__plat_path_canon(other);
    assert(a && b);
    assert(strncmp(a, "\\\\?\\", 4) != 0);
    assert(zell__dbg_path_match(a, b, zell__plat_path_fold()) == 3);
    assert(zell__dbg_path_match(a, other, zell__plat_path_fold()) == 3);
    free(a);
    free(b);
    remove(file);
#endif
}

int main(void) {
    printf("test_debug\n");
    RUN(posix_rules);
    RUN(windows_rules);
    RUN(canon_resolves_symlinks);
    return 0;
}
