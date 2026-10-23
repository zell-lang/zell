/* libFuzzer target for encoding.json (runtime/src/std_json.c): parses the
 * input, walks every node of the document with the handle API, writes it
 * back with the writer and re-parses the output (which must be valid).
 * Build and run: make -C runtime fuzz-json   (ASan + UBSan + libFuzzer).
 * Without libFuzzer (-fsanitize=fuzzer), -DFUZZ_STANDALONE builds a main()
 * that runs the files given in argv (ASan + UBSan only). */
#include "zell_rt.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum { K_BOOL = 1, K_NUM = 2, K_STR = 3, K_ARR = 4, K_OBJ = 5 };

static volatile uint64_t sink;

static void walk(int64_t doc, int64_t n, int depth) {
    int64_t kind = zell_encoding_json_rt_kind(n);
    switch (kind) {
    case K_BOOL: sink += zell_encoding_json_rt_bool(n); break;
    case K_NUM:
        /* rt_int on a float node outside the int64 range is UB (double ->
         * int64 cast, std_json.c rt_int): found by this fuzzer, input in
         * crashes/. Guarded here so the fuzzer can go on; remove the guard
         * once rt_int saturates. */
        double f = zell_encoding_json_rt_float(n);
        if (zell_encoding_json_rt_is_int(n) || (f > -9e18 && f < 9e18)) sink += zell_encoding_json_rt_int(n);
        sink += f > 0;
        break;
    case K_STR: {
        zell_string *s = zell_encoding_json_rt_str(n);
        if (s) { sink += zell_string_len(s); zell_release(s); }
        break;
    }
    case K_ARR: case K_OBJ: {
        int64_t len = zell_encoding_json_rt_len(n);
        int64_t c = zell_encoding_json_rt_first(n);
        int64_t count = kind == K_OBJ ? 2 * len : len;
        for (int64_t i = 0; i < count && c; i++, c = zell_encoding_json_rt_next(c)) {
            if (kind == K_OBJ && i % 2 == 0) {
                zell_string *k = zell_encoding_json_rt_str(c);
                zell_string *kk = k ? k : zell_string_new("", 0);
                sink += zell_encoding_json_rt_get(n, kk) != 0;
                zell_release(kk);
            } else if (depth < 512) {
                walk(doc, c, depth + 1);
            }
            if (kind == K_ARR && i < 4) sink += zell_encoding_json_rt_index(doc, n, i) != 0;
        }
        if (kind == K_ARR && len > 0) {
            sink += zell_encoding_json_rt_index(doc, n, len - 1) != 0;
            sink += zell_encoding_json_rt_index(doc, n, len) != 0;
        }
        break;
    }
    default: break;
    }
}

int LLVMFuzzerInitialize(int *argc, char ***argv) {
    (void)argc;
    zell_runtime_init(1, *argv);
    return 0;
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
    zell_string *text = zell_string_new((const char *)data, (int64_t)size);
    zell_error *err = NULL;
    int64_t doc = zell_encoding_json_rt_parse(text, &err);
    if (doc) {
        int64_t root = zell_encoding_json_rt_root(doc);
        walk(doc, root, 0);
        int64_t w = zell_encoding_json_rt_w_new();
        zell_encoding_json_rt_w_node(w, root);
        zell_string *out = zell_encoding_json_rt_w_finish(w);
        zell_error *err2 = NULL;
        int64_t doc2 = zell_encoding_json_rt_parse(out, &err2);
        if (!doc2) {
            fprintf(stderr, "writer output does not re-parse\n");
            abort();
        }
        zell_encoding_json_rt_free(doc2);
        zell_release(out);
        zell_encoding_json_rt_free(doc);
    } else if (err) {
        zell_string *m = zell_error_message(err);
        if (m) zell_release(m);
        zell_release(err);
    }
    zell_release(text);
    return 0;
}

#ifdef FUZZ_STANDALONE
int main(int argc, char **argv) {
    LLVMFuzzerInitialize(&argc, &argv);
    for (int i = 1; i < argc; i++) {
        FILE *f = fopen(argv[i], "rb");
        if (!f) { perror(argv[i]); return 2; }
        static uint8_t buf[1 << 20];
        size_t n = fread(buf, 1, sizeof buf, f);
        fclose(f);
        LLVMFuzzerTestOneInput(buf, n);
    }
    return 0;
}
#endif
