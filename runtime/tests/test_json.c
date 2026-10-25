/* Tests for std_json.c (the arena document parser, node queries, the
 * writer, quoting) and std_reflect.c (table lookups on a hand-made table
 * laid out as in ABI.md section 17). */
#include "test.h"

#include <math.h>

static int64_t parse(const char *text, zell_error **err) {
    zell_string *t = S(text);
    *err = (zell_error *)1;
    int64_t d = zell_encoding_json_rt_parse(t, err);
    zell_release(t);
    return d;
}

static int64_t parse_ok(const char *text) {
    zell_error *err;
    int64_t d = parse(text, &err);
    if (err) {
        zell_string *m = zell_error_message(err);
        fprintf(stderr, "parse [%s]: %s\n", text, zell_string_cstr(m));
        assert(0);
    }
    assert(d != 0);
    return d;
}

static int64_t get(int64_t obj, const char *key) {
    zell_string *k = S(key);
    int64_t r = zell_encoding_json_rt_get(obj, k);
    zell_release(k);
    return r;
}

static int str_is(int64_t node, const char *want) {
    zell_string *s = zell_encoding_json_rt_str(node);
    int ok = STR_IS(s, want);
    zell_release(s);
    return ok;
}

TEST(parse_document) {
    int64_t d = parse_ok(" {\"a\\u00e9\\n\": [1, -2.5e3, true, false, null, \"x\"], \"\\ud83d\\ude00\":\"\", \"n\": {\"k\": 0.25}}\n");
    int64_t root = zell_encoding_json_rt_root(d);
    assert(zell_encoding_json_rt_kind(root) == 5);
    assert(zell_encoding_json_rt_len(root) == 3);
    /* keys in document order: key, value, key, value ... */
    int64_t k = zell_encoding_json_rt_first(root);
    assert(zell_encoding_json_rt_kind(k) == 3 && str_is(k, "a\xc3\xa9\n"));
    int64_t arr = zell_encoding_json_rt_next(k);
    assert(zell_encoding_json_rt_kind(arr) == 4 && zell_encoding_json_rt_len(arr) == 6);
    assert(arr == get(root, "a\xc3\xa9\n"));
    int64_t e = zell_encoding_json_rt_first(arr);
    assert(zell_encoding_json_rt_kind(e) == 2 && zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_int(e) == 1);
    assert(zell_encoding_json_rt_float(e) == 1.0);
    e = zell_encoding_json_rt_next(e);
    assert(!zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_float(e) == -2500.0);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_kind(e) == 1 && zell_encoding_json_rt_bool(e));
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_kind(e) == 1 && !zell_encoding_json_rt_bool(e));
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_kind(e) == 0);
    e = zell_encoding_json_rt_next(e);
    assert(str_is(e, "x"));
    assert(e == zell_encoding_json_rt_index(d, arr, 5));
    assert(zell_encoding_json_rt_index(d, arr, 6) == 0 && zell_encoding_json_rt_index(d, arr, -1) == 0);
    /* the second key: a surrogate pair, an empty string value */
    k = zell_encoding_json_rt_next(arr);
    assert(str_is(k, "\xf0\x9f\x98\x80"));
    int64_t empty = zell_encoding_json_rt_next(k);
    assert(zell_encoding_json_rt_kind(empty) == 3 && zell_encoding_json_rt_str(empty) == NULL);
    int64_t n = get(root, "n");
    assert(zell_encoding_json_rt_kind(n) == 5);
    assert(zell_encoding_json_rt_float(get(n, "k")) == 0.25);
    assert(get(root, "missing") == 0);
    assert(get(arr, "x") == 0);                            /* not an object */
    assert(zell_encoding_json_rt_kind(0) == 0);            /* no node is null */
    assert(zell_encoding_json_rt_len(get(root, "n")) == 1);
    zell_encoding_json_rt_free(d);
}

TEST(parse_numbers) {
    int64_t d = parse_ok("[0, -0, 9223372036854775807, -9223372036854775808, 9223372036854775808, 1e2, 1.0, 0.1, 123456789012345678901234, 1E-7, -1.5e+300, 5e-324, 2.2250738585072014e-308, 0.30000000000000004]");
    int64_t a = zell_encoding_json_rt_root(d);
    int64_t e = zell_encoding_json_rt_first(a);
    assert(zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_int(e) == 0);
    e = zell_encoding_json_rt_next(e); /* -0 stays a float so that it formats as -0 */
    assert(!zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_float(e) == 0 && signbit(zell_encoding_json_rt_float(e)));
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_int(e) == INT64_MAX);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_int(e) == INT64_MIN);
    e = zell_encoding_json_rt_next(e); /* one past int64: a float */
    assert(!zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_float(e) == 9223372036854775808.0);
    e = zell_encoding_json_rt_next(e);
    assert(!zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_float(e) == 100.0);
    e = zell_encoding_json_rt_next(e);
    assert(!zell_encoding_json_rt_is_int(e) && zell_encoding_json_rt_float(e) == 1.0);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_float(e) == 0.1);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_float(e) == 123456789012345678901234.0);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_float(e) == 1e-7);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_float(e) == -1.5e300);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_float(e) == 5e-324);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_float(e) == 2.2250738585072014e-308);
    e = zell_encoding_json_rt_next(e);
    assert(zell_encoding_json_rt_float(e) == 0.30000000000000004);
    zell_encoding_json_rt_free(d);
}

TEST(index_table) {
    /* An array longer than the table threshold is indexed through a lazily
     * built table; elements of mixed sizes check the offsets. */
    char text[4096];
    size_t n = 0;
    text[n++] = '[';
    for (int i = 0; i < 100; i++) {
        n += (size_t)snprintf(text + n, sizeof text - n, "%s%s", i ? "," : "", i % 3 == 0 ? "[1,[2,3]]" : i % 3 == 1 ? "{\"a\":{\"b\":1}}" : "7");
    }
    text[n++] = ']';
    text[n] = 0;
    int64_t d = parse_ok(text);
    int64_t a = zell_encoding_json_rt_root(d);
    assert(zell_encoding_json_rt_len(a) == 100);
    for (int i = 0; i < 100; i++) {
        int64_t e = zell_encoding_json_rt_index(d, a, i);
        int kind = (int)zell_encoding_json_rt_kind(e);
        assert(kind == (i % 3 == 0 ? 4 : i % 3 == 1 ? 5 : 2));
        if (kind == 2) assert(zell_encoding_json_rt_int(e) == 7);
    }
    /* the same by walking */
    int64_t e = zell_encoding_json_rt_first(a);
    for (int i = 0; i < 100; i++) {
        assert(e == zell_encoding_json_rt_index(d, a, i));
        e = zell_encoding_json_rt_next(e);
    }
    zell_encoding_json_rt_free(d);
}

TEST(duplicate_keys) {
    int64_t d = parse_ok("{\"a\": 1, \"b\": 2, \"a\": 3}");
    int64_t root = zell_encoding_json_rt_root(d);
    assert(zell_encoding_json_rt_len(root) == 3);
    assert(zell_encoding_json_rt_int(get(root, "a")) == 3); /* the last one wins */
    assert(zell_encoding_json_rt_int(get(root, "b")) == 2);
    zell_encoding_json_rt_free(d);
}

static void expect_error(const char *text, const char *msg) {
    zell_error *err;
    int64_t d = parse(text, &err);
    assert(d == 0);
    assert(err != NULL);
    zell_string *m = zell_error_message(err);
    if (!STR_IS(m, msg)) {
        fprintf(stderr, "for [%s]: got [%s] want [%s]\n", text, zell_string_cstr(m), msg);
        assert(0);
    }
    zell_release(m);
    zell_release(err);
}

TEST(parse_errors) {
    /* lexical: with an offset */
    expect_error("{bad", "json: invalid character 'b' at offset 1");
    expect_error("\"abc", "json: unterminated string at offset 4");
    expect_error("\"abc\\", "json: unterminated string at offset 5");
    expect_error("-", "json: invalid number at offset 0");
    expect_error("1.", "json: invalid number at offset 0");
    expect_error("[1.e5]", "json: invalid number at offset 1");
    expect_error("1e", "json: invalid number at offset 0");
    expect_error("\"\\x\"", "json: invalid escape in string at offset 1");
    expect_error("\"a\tb\"", "json: control character in string at offset 2");
    expect_error("\"\\u12\"", "json: invalid \\u escape at offset 3");
    expect_error("\"\\u12g4\"", "json: invalid \\u escape at offset 3");
    expect_error("nul", "json: invalid character 'n' at offset 0");
    expect_error("tru", "json: invalid character 't' at offset 0");
    expect_error("\xc3\xa9", "json: invalid byte 0xc3 at offset 0");
    expect_error("1e400", "json: number out of range at offset 0");
    expect_error("{\"id\": 1} x", "json: invalid character 'x' at offset 10");
    /* grammar */
    expect_error("", "json: unexpected end of input");
    expect_error("  \n", "json: unexpected end of input");
    expect_error("[1, 2", "json: unexpected end of input");
    expect_error("{\"a\"", "json: unexpected end of input");
    expect_error("{\"a\":", "json: unexpected end of input");
    expect_error("{\"a\": 1,", "json: unexpected end of input");
    expect_error("[1 2]", "json: expected , or ] in an array, found number 2");
    expect_error("[01]", "json: expected , or ] in an array, found number 1");
    expect_error("{\"a\": 1 \"b\"}", "json: expected , or } in an object, found string \"b\"");
    expect_error("{1: 2}", "json: expected a string key in an object, found number 1");
    expect_error("{\"a\":1,}", "json: expected a string key in an object, found '}'");
    expect_error("{\"a\" 1}", "json: expected : after an object key, found number 1");
    expect_error("[1,]", "json: unexpected ']'");
    expect_error("}", "json: unexpected '}'");
    expect_error(",", "json: unexpected ','");
    expect_error("{\"id\": 1} 5", "json: unexpected number 5 after the top-level value");
    expect_error("1 true", "json: unexpected true after the top-level value");
    expect_error("[] []", "json: unexpected '[' after the top-level value");
    /* lexical errors inside the offending token win */
    expect_error("[1 \"abc", "json: unterminated string at offset 7");
    /* nesting */
    char deep[1200];
    memset(deep, '[', 513);
    deep[513] = 0;
    expect_error(deep, "json: nesting too deep");
    memset(deep, '[', 512);
    memset(deep + 512, ']', 512);
    deep[1024] = 0;
    int64_t d = parse_ok(deep);
    assert(zell_encoding_json_rt_kind(zell_encoding_json_rt_root(d)) == 4);
    zell_encoding_json_rt_free(d);
}

static zell_string *format_node(int64_t node) {
    int64_t w = zell_encoding_json_rt_w_new();
    zell_encoding_json_rt_w_node(w, node);
    return zell_encoding_json_rt_w_finish(w);
}

static void expect_roundtrip(const char *text, const char *want) {
    int64_t d = parse_ok(text);
    zell_string *s = format_node(zell_encoding_json_rt_root(d));
    if (!STR_IS(s, want)) {
        fprintf(stderr, "for [%s]: got [%s] want [%s]\n", text, zell_string_cstr(s), want);
        assert(0);
    }
    zell_release(s);
    zell_encoding_json_rt_free(d);
}

TEST(writer_node) {
    expect_roundtrip("{\"a\" : [ 1 , 2.50 , -0 , 1e2 , true , null ] , \"s\" : \"x\\ty\\u00e9\\/\" , \"e\" : { } , \"f\" : [ ] }",
                     "{\"a\":[1,2.5,-0,100,true,null],\"s\":\"x\\ty\xc3\xa9/\",\"e\":{},\"f\":[]}");
    expect_roundtrip("[9223372036854775807,-9223372036854775808,1e21,1e-7,0.1,123456789.125,1E6,100000]",
                     "[9223372036854775807,-9223372036854775808,1e+21,1e-07,0.1,1.23456789125e+08,1e+06,100000]");
    expect_roundtrip("\"\\u0001\\b\\f\\n\\r\\t\\\"\\\\ \\ud800 \\udc00x\"", "\"\\u0001\\b\\f\\n\\r\\t\\\"\\\\ \xef\xbf\xbd \xef\xbf\xbdx\"");
    expect_roundtrip("\"\"", "\"\"");
    zell_string *null = format_node(0);
    assert(STR_IS(null, "null"));
    zell_release(null);
}

TEST(writer_primitives) {
    int64_t w = zell_encoding_json_rt_w_new();
    zell_string *lb = S("{"), *rb = S("}"), *k1 = S("id"), *k2 = S("na\"me"), *k3 = S("f"), *k4 = S("u"), *k5 = S("b"), *k6 = S("n");
    zell_string *v = S("a\x01\xc3\xa9");
    zell_encoding_json_rt_w_raw(w, lb);
    zell_encoding_json_rt_w_key(w, k1, true);
    zell_encoding_json_rt_w_int(w, -42);
    zell_encoding_json_rt_w_key(w, k2, false);
    zell_encoding_json_rt_w_str(w, v);
    zell_encoding_json_rt_w_key(w, k3, false);
    zell_encoding_json_rt_w_float(w, 2.5);
    zell_encoding_json_rt_w_key(w, k4, false);
    zell_encoding_json_rt_w_uint(w, UINT64_MAX);
    zell_encoding_json_rt_w_key(w, k5, false);
    zell_encoding_json_rt_w_bool(w, true);
    zell_encoding_json_rt_w_key(w, k6, false);
    zell_encoding_json_rt_w_float(w, NAN);
    zell_encoding_json_rt_w_raw(w, rb);
    zell_string *s = zell_encoding_json_rt_w_finish(w);
    assert(STR_IS(s, "{\"id\":-42,\"na\\\"me\":\"a\\u0001\xc3\xa9\",\"f\":2.5,\"u\":18446744073709551615,\"b\":true,\"n\":null}"));
    zell_release(s);
    zell_release(lb); zell_release(rb); zell_release(k1); zell_release(k2); zell_release(k3);
    zell_release(k4); zell_release(k5); zell_release(k6); zell_release(v);
    /* key + value in one call, and a []string */
    w = zell_encoding_json_rt_w_new();
    zell_string *ka = S("a"), *kb = S("b\n"), *kc = S("c"), *kd = S("d"), *ke = S("e");
    zell_string *vs = S("x\"y");
    zell_string *elems[2] = { S("p"), S("q\t") };
    zell_slice strs;
    zell_slice_from(&strs, 8, &zell_ops_ref, elems, 2);
    zell_encoding_json_rt_w_raw(w, lb = S("{"));
    zell_encoding_json_rt_w_key_int(w, ka, true, 7);
    zell_encoding_json_rt_w_key_str(w, kb, false, vs);
    zell_encoding_json_rt_w_key_float(w, kc, false, 0.5);
    zell_encoding_json_rt_w_key_bool(w, kd, false, false);
    zell_encoding_json_rt_w_key(w, ke, false);
    zell_encoding_json_rt_w_strs(w, &strs);
    zell_encoding_json_rt_w_raw(w, rb = S("}"));
    s = zell_encoding_json_rt_w_finish(w);
    assert(STR_IS(s, "{\"a\":7,\"b\\n\":\"x\\\"y\",\"c\":0.5,\"d\":false,\"e\":[\"p\",\"q\\t\"]}"));
    zell_release(s);
    zell_slice_release(&strs);
    zell_release(elems[0]); zell_release(elems[1]);
    zell_release(ka); zell_release(kb); zell_release(kc); zell_release(kd); zell_release(ke); zell_release(vs);
    zell_release(lb); zell_release(rb);
    /* growth: a long text written in small pieces */
    w = zell_encoding_json_rt_w_new();
    zell_string *piece = S("0123456789");
    for (int i = 0; i < 10000; i++) zell_encoding_json_rt_w_raw(w, piece);
    zell_encoding_json_rt_w_int(w, INT64_MIN);
    s = zell_encoding_json_rt_w_finish(w);
    assert(zell_string_len(s) == 100000 + 20);
    assert(memcmp(zell_string_data(s) + 99990, "0123456789-9223372036854775808", 30) == 0);
    assert(zell_string_data(s)[zell_string_len(s)] == 0);
    zell_release(s);
    zell_release(piece);
}

TEST(quote) {
    zell_string *s = zell_string_new("a\"b\\c\n\x01\xc3\xa9", 9);
    zell_string *q = zell_encoding_json_quote(s);
    assert(STR_IS(q, "\"a\\\"b\\\\c\\n\\u0001\xc3\xa9\""));
    zell_release(q);
    zell_release(s);
    q = zell_encoding_json_quote(NULL);
    assert(STR_IS(q, "\"\""));
    zell_release(q);
    /* a long run without escapes goes through the vector path */
    char buf[300];
    memset(buf, 'x', sizeof buf);
    buf[150] = '"';
    buf[299] = '\n';
    s = zell_string_new(buf, sizeof buf);
    q = zell_encoding_json_quote(s);
    assert(zell_string_len(q) == 300 + 2 + 2);
    assert(zell_string_data(q)[151] == '\\' && zell_string_data(q)[152] == '"');
    assert(memcmp(zell_string_data(q) + 301, "\\n\"", 3) == 0);
    zell_release(q);
    zell_release(s);
}

static void expect_indent(const char *src, const char *prefix, const char *indent, const char *want) {
    zell_string *s = zell_string_new(src, (int64_t)strlen(src));
    zell_string *p = zell_string_new(prefix, (int64_t)strlen(prefix));
    zell_string *in = zell_string_new(indent, (int64_t)strlen(indent));
    zell_string *r = zell_encoding_json_rt_indent(s, p, in);
    if (!STR_IS(r, want)) {
        fprintf(stderr, "indent %s: got [%.*s]\n", src, (int)zell_string_len(r), zell_string_data(r));
        assert(0);
    }
    zell_release(r);
    zell_release(in);
    zell_release(p);
    zell_release(s);
}

TEST(indent) {
    expect_indent("{\"a\":[1,2],\"b\":{},\"c\":[]}", "", "  ", "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": []\n}");
    expect_indent("[{\"k\":\"x,\\\"y:{\"}]", ">", "\t", "[\n>\t{\n>\t\t\"k\": \"x,\\\"y:{\"\n>\t}\n>]");
    expect_indent(" 42 ", "", "  ", "42");
    expect_indent("{ \"a\" : [ ] }", "", "", "{\n\"a\": []\n}");
    expect_indent("", "", "  ", "");
    expect_indent("\"unterminated", "", "  ", "\"unterminated");
}

/* Random mutations and truncations of a corpus: no crash, no sanitizer
 * report, and a document that parses formats to something that parses. */
static const char *const corpus[] = {
    "{\"items\":[{\"id\":1,\"name\":\"lorem-ipsum-0\",\"price\":12.34,\"qty\":5,\"active\":true,\"tags\":[\"a\",\"b\"]}]}",
    "[1, -2.5e3, true, false, null, \"x\\u00e9\\n\\ud83d\\ude00\", {}, [], [[[]]], {\"a\":{\"b\":{\"c\":0}}}]",
    "\"\\\"\\\\\\/\\b\\f\\n\\r\\t\\u0000\"",
    "-0.000001e-10",
    "{\"a\":1,\"a\":2,\"\":3}",
    "   7   ",
};

static uint64_t rng_state = 0x9E3779B97F4A7C15ull;
static uint32_t rnd(void) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 7;
    rng_state ^= rng_state << 17;
    return (uint32_t)rng_state;
}

static void try_parse(const char *text, int64_t len) {
    zell_string *t = zell_string_new(text, len);
    zell_error *err = NULL;
    int64_t d = zell_encoding_json_rt_parse(t, &err);
    if (err) {
        assert(d == 0);
        zell_release(err);
    } else {
        assert(d != 0);
        zell_string *s = format_node(zell_encoding_json_rt_root(d));
        zell_error *err2 = NULL;
        int64_t d2 = zell_encoding_json_rt_parse(s, &err2);
        assert(err2 == NULL && d2 != 0);
        zell_string *s2 = format_node(zell_encoding_json_rt_root(d2));
        assert(zell_string_eq(s, s2));
        zell_release(s2);
        zell_encoding_json_rt_free(d2);
        zell_release(s);
        zell_encoding_json_rt_free(d);
    }
    zell_release(t);
}

TEST(fuzz_mutations) {
    static const char alphabet[] = "{}[]:,\"\\/-+.eE0123456789tfnrulsabu \n\t\x01\x7f\xc3\xff";
    for (size_t c = 0; c < sizeof corpus / sizeof corpus[0]; c++) {
        const char *src = corpus[c];
        int64_t n = (int64_t)strlen(src);
        for (int64_t cut = 0; cut <= n; cut++) try_parse(src, cut);
        char buf[512];
        for (int round = 0; round < 400; round++) {
            memcpy(buf, src, (size_t)n);
            int64_t len = n;
            int edits = 1 + (int)(rnd() % 4);
            for (int e = 0; e < edits && len > 0; e++) {
                int64_t pos = (int64_t)(rnd() % (uint32_t)len);
                switch (rnd() % 3) {
                case 0: buf[pos] = alphabet[rnd() % (sizeof alphabet - 1)]; break;
                case 1: memmove(buf + pos, buf + pos + 1, (size_t)(len - pos - 1)); len--; break;
                default:
                    if (len + 1 < (int64_t)sizeof buf) {
                        memmove(buf + pos + 1, buf + pos, (size_t)(len - pos));
                        buf[pos] = alphabet[rnd() % (sizeof alphabet - 1)];
                        len++;
                    }
                }
            }
            try_parse(buf, len);
        }
    }
}

/* A table for `struct P { X int }` the way the compiler lays it out. */
ZELL_STATIC_STRING(name_int, "int");
ZELL_STATIC_STRING(name_p, "P");
ZELL_STATIC_STRING(name_x, "X");
static zell_reflect_type ti_int = { { "int", 24, NULL }, ZELL_KIND_INT, NULL, NULL, 8, 8, { 0 }, { 0 }, { 0 }, NULL, NULL, 0, { 0 }, NULL, NULL, NULL };
static zell_reflect_field field_x = { NULL, &ti_int, 0, 0, false, true, { 0 } };
static zell_reflect_field *fields_p[] = { &field_x };
static zell_reflect_type ti_p = { { "t.P", 24, NULL }, ZELL_KIND_STRUCT, NULL, NULL, 8, 8, { fields_p, 1, 1, NULL }, { 0 }, { 0 }, NULL, NULL, 0, { 0 }, NULL, NULL, NULL };
static zell_reflect_type ti_int64 = { { "int", 24, NULL }, ZELL_KIND_INT, NULL, NULL, 8, 8, { 0 }, { 0 }, { 0 }, NULL, NULL, 0, { 0 }, NULL, &ti_int, NULL };

TEST(reflect_lookups) {
    ti_int.name = name_int;
    ti_p.name = name_p;
    field_x.name = name_x;
    assert(strcmp(zell_reflect_kind_name(ti_p.kind), "Struct") == 0);
    assert(strcmp(zell_reflect_kind_name(ZELL_KIND_TASK), "Task") == 0);
    assert(strcmp(zell_reflect_kind_name(99), "Invalid") == 0);
    assert(zell_reflect_field_by_name(&ti_p, "X") == &field_x);
    assert(zell_reflect_field_by_name(&ti_p, "Y") == NULL);
    assert(zell_reflect_method_by_name(&ti_p, "X") == NULL);
    assert(zell_reflect_canon(&ti_int64) == &ti_int);
    assert(zell_reflect_canon(&ti_int) == &ti_int);
    /* A box made with a table as its type info. */
    void *obj = zell_object_new(&ti_p.rt, 24);
    assert(zell_reflect_type_of(obj) == &ti_p);
    zell_release(obj);
    assert(zell_reflect_type_of(NULL) == NULL);
}

int main(int argc, char **argv) {
    (void)argc;
    zell_runtime_init(1, argv);
    printf("test_json\n");
    int64_t live = zell_live_objects();
    RUN(parse_document);
    RUN(parse_numbers);
    RUN(index_table);
    RUN(duplicate_keys);
    RUN(parse_errors);
    RUN(writer_node);
    RUN(writer_primitives);
    RUN(quote);
    RUN(indent);
    RUN(fuzz_mutations);
    RUN(reflect_lookups);
    assert(zell_live_objects() == live);
    return 0;
}
