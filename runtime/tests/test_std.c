#include "test.h"
#include <math.h>

/* []string from C strings (owned slice, each element owned by the backing). */
static void strs(zell_slice *out, int n, const char *const *v) {
    zell_slice_new_ops(out, 8, n, n, &zell_ops_ref);
    for (int i = 0; i < n; i++) ((zell_string **)out->data)[i] = S(v[i]);
}

static bool slice_is(const zell_slice *s, int n, const char *const *v) {
    if (s->len != n) return false;
    for (int i = 0; i < n; i++)
        if (!STR_IS(((zell_string **)s->data)[i], v[i])) return false;
    return true;
}

#define SPLIT_IS(s, sep, ...) do {                                               \
        static const char *const exp_[] = { __VA_ARGS__ };                       \
        zell_string *a_ = S(s), *b_ = S(sep);                                   \
        zell_slice r_;                                                          \
        zell_strings_split(a_, b_, &r_);                                        \
        assert(slice_is(&r_, (int)(sizeof exp_ / sizeof *exp_), exp_));          \
        zell_slice_release(&r_); zell_release(a_); zell_release(b_);          \
    } while (0)

/* Check a string-returning call and release its result. */
#define EXPECT_STR(call, lit) do { zell_string *r_ = (call); assert(r_ != NULL); \
        assert(STR_IS(r_, lit)); zell_release(r_); } while (0)

TEST(split) {
    SPLIT_IS("red,green,blue", ",", "red", "green", "blue");
    SPLIT_IS("a,b,", ",", "a", "b", "");
    SPLIT_IS(",a", ",", "", "a");
    SPLIT_IS("abc", "x", "abc");                 /* no match */
    SPLIT_IS("", ",", "");                       /* Go: [""] */
    SPLIT_IS("a--b----c", "--", "a", "b", "", "c");
    SPLIT_IS("aaa", "aa", "", "a");              /* non-overlapping */
    SPLIT_IS("abc", "abc", "", "");
    SPLIT_IS("héllo", "", "h", "é", "l", "l", "o"); /* empty sep: per UTF-8 sequence */
    /* empty s with empty sep -> empty slice */
    zell_slice r;
    zell_strings_split(NULL, NULL, &r);
    assert(r.len == 0);
    zell_slice_release(&r);
    /* invalid UTF-8 byte becomes a 1-byte element */
    zell_string *bad = zell_string_new("a\xff", 2);
    zell_strings_split(bad, NULL, &r);
    assert(r.len == 2 && zell_string_len(((zell_string **)r.data)[1]) == 2 - 1);
    assert((unsigned char)zell_string_data(((zell_string **)r.data)[1])[0] == 0xff);
    zell_slice_release(&r);
    zell_release(bad);
    /* result is a proper []string: appendable, elements owned */
    zell_string *s = S("x y"), *sp = S(" ");
    zell_strings_split(s, sp, &r);
    zell_string *z = S("z");
    zell_slice_append(&r, 8, &zell_ops_ref, &z);
    zell_release(z);
    static const char *const e3[] = { "x", "y", "z" };
    assert(slice_is(&r, 3, e3));
    zell_slice_release(&r);
    zell_release(s); zell_release(sp);
}

TEST(join) {
    static const char *const v[] = { "red", "green", "blue" };
    zell_slice parts;
    strs(&parts, 3, v);
    zell_string *sep = S(" | ");
    EXPECT_STR(zell_strings_join(&parts, sep), "red | green | blue");
    EXPECT_STR(zell_strings_join(&parts, NULL), "redgreenblue");
    zell_slice one; static const char *const v1[] = { "solo" };
    strs(&one, 1, v1);
    EXPECT_STR(zell_strings_join(&one, sep), "solo");
    zell_slice empty = { 0 };
    EXPECT_STR(zell_strings_join(&empty, sep), "");
    /* NULL elements (zero-value strings) are "" */
    zell_slice holes;
    zell_slice_new_ops(&holes, 8, 3, 3, &zell_ops_ref);
    ((zell_string **)holes.data)[1] = S("m");
    EXPECT_STR(zell_strings_join(&holes, sep), " | m | ");
    /* sub-slice view */
    zell_slice sub;
    zell_slice_sub(&sub, &parts, 1, 3, 8);
    zell_string *comma = S(",");
    EXPECT_STR(zell_strings_join(&sub, comma), "green,blue");
    zell_slice_release(&sub);
    /* round trip with Split */
    zell_string *j = zell_strings_join(&parts, comma);
    zell_slice back;
    zell_strings_split(j, comma, &back);
    assert(slice_is(&back, 3, v));
    zell_slice_release(&back);
    zell_release(j); zell_release(comma);
    zell_slice_release(&holes); zell_slice_release(&one); zell_slice_release(&parts);
    zell_release(sep);
}

TEST(trim) {
    zell_string *a = S("   padded   ");
    EXPECT_STR(zell_strings_trim(a), "padded");
    zell_string *b = S(" \t\n\v\f\r x y \r\n");
    EXPECT_STR(zell_strings_trim(b), "x y");
    zell_string *c = S("none");
    zell_string *r = zell_strings_trim(c);
    assert(STR_IS(r, "none"));
    zell_release(r);
    zell_string *d = S("   ");
    EXPECT_STR(zell_strings_trim(d), "");
    EXPECT_STR(zell_strings_trim(NULL), "");
    zell_string *u = S("\xC2\xA0\xE3\x80\x80héllo\xE2\x80\x83\xC2\x85");  /* NBSP, U+3000 ... U+2003, NEL */
    EXPECT_STR(zell_strings_trim(u), "héllo");
    zell_string *inner = S("a  b");
    EXPECT_STR(zell_strings_trim(inner), "a  b");
    zell_string *bad = zell_string_new(" \x80 ", 3);   /* invalid byte is not space */
    r = zell_strings_trim(bad);
    assert(zell_string_len(r) == 1 && (unsigned char)zell_string_data(r)[0] == 0x80);
    zell_release(r);
    zell_string *tail = S("x\xA0");                     /* lone continuation byte at end */
    EXPECT_STR(zell_strings_trim(tail), "x\xA0");
    zell_release(a); zell_release(b); zell_release(c); zell_release(d);
    zell_release(u); zell_release(inner); zell_release(bad); zell_release(tail);
}

TEST(predicates) {
    zell_string *msg = S("Hello, Zell!"), *sl = S("Zell"), *go = S("Go");
    zell_string *he = S("Hello"), *bang = S("!"), *e = S("");
    assert(zell_strings_contains(msg, sl));
    assert(!zell_strings_contains(msg, go));
    assert(zell_strings_contains(msg, e) && zell_strings_contains(NULL, NULL));
    assert(!zell_strings_contains(NULL, go));
    assert(zell_strings_has_prefix(msg, he) && !zell_strings_has_prefix(msg, sl));
    assert(zell_strings_has_prefix(msg, NULL) && !zell_strings_has_prefix(NULL, he));
    assert(zell_strings_has_suffix(msg, bang) && !zell_strings_has_suffix(msg, he));
    assert(zell_strings_has_suffix(NULL, e) && !zell_strings_has_suffix(bang, msg));
    assert(zell_strings_index(msg, sl) == 7);
    assert(zell_strings_index(msg, go) == -1);
    assert(zell_strings_index(msg, e) == 0 && zell_strings_index(NULL, NULL) == 0);
    assert(zell_strings_index(sl, msg) == -1);
    zell_string *h = S("héllo"), *l = S("l");
    assert(zell_strings_index(h, l) == 3);   /* byte offset */
    zell_release(h); zell_release(l);
    zell_release(msg); zell_release(sl); zell_release(go);
    zell_release(he); zell_release(bang); zell_release(e);
}

TEST(replace) {
    zell_string *s = S("a-b-c"), *dash = S("-"), *plus = S("+"), *e = S("");
    EXPECT_STR(zell_strings_replace(s, dash, plus), "a+b+c");
    EXPECT_STR(zell_strings_replace(s, dash, e), "abc");
    EXPECT_STR(zell_strings_replace(s, plus, dash), "a-b-c");     /* no match */
    EXPECT_STR(zell_strings_replace(s, dash, dash), "a-b-c");
    zell_string *aaa = S("aaaa"), *aa = S("aa"), *b = S("b"), *xyz = S("xyz");
    EXPECT_STR(zell_strings_replace(aaa, aa, b), "bb");
    EXPECT_STR(zell_strings_replace(aaa, aa, xyz), "xyzxyz");   /* grows */
    EXPECT_STR(zell_strings_replace(aaa, aaa, e), "");
    /* empty old: Go inserts at start and after each rune */
    zell_string *abc = S("héc");
    EXPECT_STR(zell_strings_replace(abc, e, dash), "-h-é-c-");
    EXPECT_STR(zell_strings_replace(NULL, NULL, dash), "-");
    EXPECT_STR(zell_strings_replace(NULL, dash, plus), "");
    zell_release(s); zell_release(dash); zell_release(plus); zell_release(e);
    zell_release(aaa); zell_release(aa); zell_release(b); zell_release(xyz); zell_release(abc);
}

TEST(case_mapping) {
    zell_string *msg = S("Hello, Zell! 123");
    EXPECT_STR(zell_strings_to_upper(msg), "HELLO, ZELL! 123");
    EXPECT_STR(zell_strings_to_lower(msg), "hello, zell! 123");
    EXPECT_STR(zell_strings_to_upper(NULL), "");
    EXPECT_STR(zell_strings_to_lower(NULL), "");
    zell_string *u = S("héllo ÿ straße ı ǆ мир αβγ ς ά ő 😀");
    EXPECT_STR(zell_strings_to_upper(u), "HÉLLO Ÿ STRAßE I ǆ МИР ΑΒΓ Σ Ά Ő 😀");
    zell_string *l = S("HÉLLO Ÿ İ ЁЖ ΑΒΓ Ά Ő × ÷");
    EXPECT_STR(zell_strings_to_lower(l), "héllo ÿ i ёж αβγ ά ő × ÷");
    zell_string *bad = zell_string_new("a\xff" "b", 3);
    zell_string *r = zell_strings_to_upper(bad);
    assert(zell_string_len(r) == 3 && memcmp(zell_string_data(r), "A\xff" "B", 3) == 0);
    zell_release(r);
    zell_release(msg); zell_release(u); zell_release(l); zell_release(bad);
}

static void repeat_negative(void) { zell_strings_repeat(NULL, -1); }
static void repeat_overflow(void) {
    zell_string *s = S("ab");
    zell_strings_repeat(s, INT64_MAX / 2);
}

TEST(repeat) {
    zell_string *ab = S("ab");
    EXPECT_STR(zell_strings_repeat(ab, 3), "ababab");
    EXPECT_STR(zell_strings_repeat(ab, 1), "ab");
    EXPECT_STR(zell_strings_repeat(ab, 0), "");
    EXPECT_STR(zell_strings_repeat(NULL, 1000000), "");
    zell_release(ab);
    char buf[4096];
    setenv("ZELL_NO_BACKTRACE", "1", 1);
    assert(run_child(repeat_negative, buf, sizeof buf) == 2);
    assert(strstr(buf, "panic: strings: negative Repeat count"));
    assert(run_child(repeat_overflow, buf, sizeof buf) == 2);
    assert(strstr(buf, "overflow"));
}

static int64_t parse_int(const char *lit, bool *ok) {
    zell_string *s = S(lit);
    zell_error *err = (zell_error *)1;
    int64_t v = zell_strings_to_int(s, &err);
    *ok = err == NULL;
    if (err) { assert(v == 0); zell_release(err); }
    zell_release(s);
    return v;
}

TEST(to_int) {
    bool ok;
    assert(parse_int("42", &ok) == 42 && ok);
    assert(parse_int("-17", &ok) == -17 && ok);
    assert(parse_int("0", &ok) == 0 && ok);
    assert(parse_int("+5", &ok) == 5 && ok);   /* as Go strconv.Atoi */
    assert(parse_int("9223372036854775807", &ok) == INT64_MAX && ok);
    assert(parse_int("-9223372036854775808", &ok) == INT64_MIN && ok);
    const char *bad[] = { "4x2", "", " 1", "1 ", "\r1", "\v1", "-", "+", "0x10", "1.5",
                          "9223372036854775808", "-9223372036854775809",
                          "99999999999999999999999", "--1", "abc" };
    for (size_t i = 0; i < sizeof bad / sizeof *bad; i++) {
        parse_int(bad[i], &ok);
        assert(!ok);
    }
    zell_error *err = NULL;
    assert(zell_strings_to_int(NULL, &err) == 0 && err != NULL);
    zell_string *m = zell_error_message(err);
    assert(strstr(zell_string_data(m), "invalid integer"));
    zell_release(m); zell_release(err);
    zell_string *big = S("99999999999999999999");
    assert(zell_strings_to_int(big, &err) == 0 && err != NULL);
    m = zell_error_message(err);
    assert(strstr(zell_string_data(m), "out of range"));
    zell_release(m); zell_release(err); zell_release(big);
    /* embedded NUL */
    zell_string *nul = zell_string_new("1\0" "2", 3);
    assert(zell_strings_to_int(nul, &err) == 0 && err != NULL);
    zell_release(err); zell_release(nul);
    assert(zell_strings_to_int(NULL, NULL) == 0);   /* err may be NULL */
}

TEST(to_float) {
    zell_error *err = (zell_error *)1;
    zell_string *s = S("3.14");
    assert(zell_strings_to_float(s, &err) == 3.14 && err == NULL);
    zell_release(s);
    s = S("-2e3");
    assert(zell_strings_to_float(s, &err) == -2000.0 && err == NULL);
    zell_release(s);
    s = S("7");
    assert(zell_strings_to_float(s, &err) == 7.0 && err == NULL);
    zell_release(s);
    const char *bad[] = { "", "x", "1.5x", " 1", "1 ", "\f1", "1e400", "-1e400", "." };
    for (size_t i = 0; i < sizeof bad / sizeof *bad; i++) {
        s = S(bad[i]);
        err = NULL;
        assert(zell_strings_to_float(s, &err) == 0 && err != NULL);
        zell_release(err); zell_release(s);
    }
}

TEST(math) {
    assert(zell_math_sqrt(4.0) == 2.0);
    assert(fabs(zell_math_sqrt(2.0) - 1.4142135623730951) < 1e-15);
    assert(isnan(zell_math_sqrt(-1.0)));
    assert(zell_math_abs(-2.5) == 2.5 && zell_math_abs(3.0) == 3.0);
    assert(!signbit(zell_math_abs(-0.0)));
    assert(zell_math_pow(2.0, 10.0) == 1024.0 && zell_math_pow(5.0, 0.0) == 1.0);
    assert(zell_math_pow(2.0, -1.0) == 0.5);
    assert(zell_math_floor(3.7) == 3.0 && zell_math_floor(-3.2) == -4.0);
    assert(zell_math_ceil(3.2) == 4.0 && zell_math_ceil(-3.7) == -3.0);
    assert(zell_math_min(1.0, 2.0) == 1.0 && zell_math_min(2.0, -1.0) == -1.0);
    assert(zell_math_max(1.0, 2.0) == 2.0 && zell_math_max(2.0, -1.0) == 2.0);
    assert(isnan(zell_math_min(NAN, 1.0)) && isnan(zell_math_max(1.0, NAN)));
    assert(zell_math_min(NAN, -INFINITY) == -INFINITY);
    assert(zell_math_max(NAN, INFINITY) == INFINITY);
    assert(signbit(zell_math_min(0.0, -0.0)) && signbit(zell_math_min(-0.0, 0.0)));
    assert(!signbit(zell_math_max(0.0, -0.0)) && !signbit(zell_math_max(-0.0, 0.0)));
    assert(zell_math_sin(0.0) == 0.0 && fabs(zell_math_sin(M_PI / 2) - 1.0) < 1e-15);
    assert(zell_math_cos(0.0) == 1.0 && fabs(zell_math_cos(M_PI) + 1.0) < 1e-15);
    assert(zell_math_tan(0.0) == 0.0 && fabs(zell_math_tan(M_PI / 4) - 1.0) < 1e-15);
    assert(zell_math_log(1.0) == 0.0 && fabs(zell_math_log(M_E) - 1.0) < 1e-15);
    assert(zell_math_log(0.0) == -INFINITY && isnan(zell_math_log(-1.0)));
    assert(zell_math_exp(0.0) == 1.0 && fabs(zell_math_exp(1.0) - M_E) < 1e-15);
}

static zell_string *fmt(const char *tmpl, int n, const char *const *v) {
    zell_slice args;
    strs(&args, n, v);
    zell_string *t = S(tmpl);
    zell_string *r = zell_fmt_format(t, &args);
    zell_release(t);
    zell_slice_release(&args);
    return r;
}

TEST(format) {
    static const char *const a2[] = { "Alice", "31" };
    EXPECT_STR(fmt("{0} is {1} years old", 2, a2), "Alice is 31 years old");
    static const char *const xy[] = { "x", "y" };
    EXPECT_STR(fmt("{1} before {0}, {0} again", 2, xy), "y before x, x again");
    EXPECT_STR(fmt("no placeholders", 2, xy), "no placeholders");          /* extra ignored */
    EXPECT_STR(fmt("{0} {2} {1}", 2, xy), "x {2} y");                    /* missing kept */
    EXPECT_STR(fmt("{0}", 0, NULL), "{0}");
    EXPECT_STR(fmt("", 2, xy), "");
    EXPECT_STR(fmt("{} {x} {-1} { 0} {0 } {0", 2, xy), "{} {x} {-1} { 0} {0 } {0");
    EXPECT_STR(fmt("{{0}} {01} {99999999999999999999999}", 2, xy), "{x} y {99999999999999999999999}");
    EXPECT_STR(fmt("${0}.0{1}", 2, xy), "$x.0y");                         /* $ not special */
    EXPECT_STR(fmt("{0}{0}{0}", 1, xy), "xxx");
    static const char *const empt[] = { "" };
    EXPECT_STR(fmt("[{0}]", 1, empt), "[]");
    /* alignment: {N,W} right, {N,-W} left, width in code points */
    EXPECT_STR(fmt("[{0,3}|{1,-3}]", 2, xy), "[  x|y  ]");
    static const char *const uni[] = { "\xd1\x8f\xd1\x8f", "toolong" };
    EXPECT_STR(fmt("[{0,4}][{1,3}][{1,-3}][{0,0}]", 2, uni), "[  \xd1\x8f\xd1\x8f][toolong][toolong][\xd1\x8f\xd1\x8f]");
    EXPECT_STR(fmt("{0,} {0,-} {0,x} {0, 3} {0,3 } {0,99999999999}", 2, xy), "{0,} {0,-} {0,x} {0, 3} {0,3 } {0,99999999999}");
    EXPECT_STR(fmt("[{0,3}]", 1, empt), "[   ]");
    /* NULL template, NULL args pointer, NULL elements */
    EXPECT_STR(zell_fmt_format(NULL, NULL), "");
    zell_string *t = S("a{0}b");
    EXPECT_STR(zell_fmt_format(t, NULL), "a{0}b");
    zell_slice holes;
    zell_slice_new_ops(&holes, 8, 1, 1, &zell_ops_ref);
    EXPECT_STR(zell_fmt_format(t, &holes), "ab");
    zell_slice_release(&holes);
    zell_release(t);
}

TEST(io) {
    char dir[] = "/tmp/zell_std_test_XXXXXX";
    assert(mkdtemp(dir));
    char path[256];
    snprintf(path, sizeof path, "%s/f.txt", dir);
    zell_string *p = S(path), *data = S("hello\nworld\n");
    zell_error *err = (zell_error *)1;
    zell_io_write_file(p, data, &err);
    assert(err == NULL);
    err = (zell_error *)1;
    zell_string *back = zell_io_read_file(p, &err);
    assert(err == NULL && zell_string_eq(back, data));
    zell_release(back);
    zell_io_write_file(p, NULL, &err);            /* empty contents */
    assert(err == NULL);
    back = zell_io_read_file(p, &err);
    assert(err == NULL && zell_string_len(back) == 0);
    zell_release(back);
    zell_io_write_file(p, data, NULL);            /* err may be NULL */
    /* failures */
    char missing[300];
    snprintf(missing, sizeof missing, "%s/nope/x", dir);
    zell_string *mp = S(missing);
    assert(zell_io_read_file(mp, &err) == NULL && err != NULL);
    zell_string *m = zell_error_message(err);
    assert(strstr(zell_string_data(m), "open ") && strstr(zell_string_data(m), "No such file"));
    zell_release(m); zell_release(err);
    err = NULL;
    zell_io_write_file(mp, data, &err);
    assert(err != NULL);
    zell_release(err);
    zell_io_write_file(mp, data, NULL);           /* error dropped without leaking */
    unlink(path);
    rmdir(dir);
    zell_release(p); zell_release(data); zell_release(mp);
}

static void read_lines(void) {
    zell_error *err = (zell_error *)1;
    zell_string *a = zell_io_read_line(&err);
    assert(err == NULL && STR_IS(a, "first"));
    zell_string *b = zell_io_read_line(&err);
    assert(err == NULL && STR_IS(b, ""));
    zell_string *c = zell_io_read_line(&err);
    assert(err == NULL && STR_IS(c, "last"));
    zell_string *d = zell_io_read_line(&err);
    assert(d == NULL && err == zell_err_eof);
    zell_release(err);
    zell_release(a); zell_release(b); zell_release(c);
    printf("lines ok");
    fflush(stdout);
}

TEST(read_line) {
    int fds[2];
    assert(pipe(fds) == 0);
    const char *in = "first\r\n\nlast";
    assert(write(fds[1], in, strlen(in)) == (ssize_t)strlen(in));
    close(fds[1]);
    int saved = dup(0);
    dup2(fds[0], 0);
    close(fds[0]);
    char buf[256];
    int st = run_child_fd(read_lines, 1, buf, sizeof buf);
    dup2(saved, 0);
    close(saved);
    assert(st == 0 && strcmp(buf, "lines ok") == 0);
}

TEST(errors) {
    ZELL_STATIC_STRING(nf_msg, "not found");
    zell_error *sentinel = zell_error_sentinel(nf_msg);
    zell_error *other = zell_error_new_cstr("empty");
    zell_string *ctx = S("user 3");
    zell_error *w = zell_errors_wrap(sentinel, ctx);
    zell_string *m = zell_error_message(w);
    assert(STR_IS(m, "user 3: not found"));
    zell_release(m);
    assert(zell_errors_is(w, sentinel) && !zell_errors_is(w, other));
    assert(zell_errors_is(sentinel, sentinel) && !zell_errors_is(sentinel, w));
    assert(zell_errors_is(NULL, NULL) && !zell_errors_is(w, NULL) && !zell_errors_is(NULL, sentinel));
    zell_string *req = S("request");
    zell_error *w2 = zell_errors_wrap(w, req);
    m = zell_error_message(w2);
    assert(STR_IS(m, "request: user 3: not found"));
    zell_release(m);
    assert(zell_errors_is(w2, sentinel) && zell_errors_is(w2, w));
    zell_error *u1 = zell_errors_unwrap(w2);
    assert(u1 == w);
    zell_error *u2 = zell_errors_unwrap(u1);
    assert(u2 == sentinel);
    assert(zell_errors_unwrap(u2) == NULL);        /* not a wrapper */
    assert(zell_errors_unwrap(other) == NULL);
    assert(zell_errors_unwrap(NULL) == NULL);
    assert(zell_errors_wrap(NULL, ctx) == NULL);   /* wrapping nil is nil */
    zell_error *we = zell_errors_wrap(other, NULL);  /* empty context */
    m = zell_error_message(we);
    assert(STR_IS(m, ": empty"));
    zell_release(m);
    /* the wrapper keeps its cause alive */
    zell_release(other);
    zell_error *c = zell_errors_unwrap(we);
    m = zell_error_message(c);
    assert(STR_IS(m, "empty"));
    zell_release(m); zell_release(c); zell_release(we);
    zell_release(u1); zell_release(u2);
    zell_release(w2); zell_release(w);
    zell_release(ctx); zell_release(req);
}

TEST(delay) {
    zell_task *t = zell_task_delay(1);
    zell_task_await(t);
    assert(zell_task_is_done(t));
    zell_release(t);
    t = zell_task_delay(0);
    assert(zell_task_is_done(t));
    zell_release(t);
}

int main(void) {
    printf("test_std\n");
    int64_t live = zell_live_objects();
    RUN(split);
    RUN(join);
    RUN(trim);
    RUN(predicates);
    RUN(replace);
    RUN(case_mapping);
    RUN(repeat);
    RUN(to_int);
    RUN(to_float);
    RUN(math);
    RUN(format);
    RUN(io);
    RUN(read_line);
    assert(zell_live_objects() == live);
    RUN(errors);
    assert(zell_live_objects() == live);  /* immortal sentinels do not count */
    RUN(delay);
    zell_runtime_shutdown();
    assert(zell_live_objects() == live);
    return 0;
}
