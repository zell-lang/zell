# Stdlib benchmarks: Zell vs Go

Five benchmarks of the standard library rather than the language core:
`json_codec`, `encodings`, `crypto`, `file_io`, `sort_collect`. As in the
other benchmarks, each directory holds an equivalent `main.zl` and
`main.go`. Both generate the same deterministic data, so both print the
same checksums to **stdout**, and each prints the time of every phase (and
MB/s or ops/s) to **stderr**.

**Status: measured on a quiet machine** (2026-10-22, commit 450e35f, 4 vCPU
Intel Xeon @ 2.80 GHz with AVX-512, Firecracker VM, Linux 6.18, clang/LLVM
18, go 1.24.7, nothing else running). Every pair printed identical stdout
at full size. `json_codec` ran with an 8 MB document instead of 100 MB
(see the results).

## The benchmarks

| Benchmark | Zell API | Go API | Full size (default) | `SCALE=small` |
|---|---|---|---|---|
| `json_codec` | `encoding.json.Parse<Doc>` / `Format` (typed, `@json(name:)`), `ParseValue` + `Get`/`Index` path lookups + `Value.Format`, and 1M small messages, each parsed with `Parse<Msg>` and formatted again with `Format` | `encoding/json` `Unmarshal` / `Marshal` into structs and into `any` (`map[string]any`) | 100 MB document (~1M items); 1M messages of ~172 bytes (1024 different texts used in turn) | 1 MB, 10k messages |
| `encodings` | `encoding.xml`, `encoding.yaml`, `encoding.toml` `Parse<Catalog>` / `Format`, one struct with `@serde(name:)` + `@xml(attr:, name:)` | `encoding/xml`, `gopkg.in/yaml.v3` v3.0.1, `github.com/BurntSushi/toml` v1.4.0 (vendored) | 20 MB document per format (~150k-185k items), parsed, formatted, re-parsed (round trip checked) | 200 KB each |
| `crypto` | `crypto.hash.Sum`, `aes.GcmSeal`, `chacha.Seal`, `x25519.SharedSecret`, `ed25519.Sign/Verify`, `rsa.SignPKCS1v15/VerifyPKCS1v15`, `ecdsa.Sign/Verify`, `kdf.Hkdf`, `kdf.Argon2id`, `net.tls` Listen/Dial | `crypto/sha256`, `sha512`, `aes`+`cipher.NewGCM`, `x/crypto/chacha20poly1305`, `crypto/ecdh`, `ed25519`, `rsa`, `ecdsa`, `crypto/hkdf` (Go 1.24), `x/crypto/argon2`, `crypto/tls` (x/crypto v0.31.0 vendored) | SHA-256/512 over 1 GB (1 MB chunks); AES-256-GCM and ChaCha20-Poly1305 sealing 1 GB (1 MB messages); 20k X25519; 20k Ed25519 sign + 20k verify; RSA-2048 500 sign + 5000 verify; P-256 10k sign + 10k verify; 200k HKDF-SHA256; 8 Argon2id (t=1, 64 MiB, 4 lanes); 1000 TLS 1.3 full handshakes on loopback | 16 MB, 1/20 of the operations, 1 Argon2id |
| `file_io` | `os.Create/Open`, `File.Write/Read` (1 MB chunks into one reused buffer), `io.NewBufferedWriter/Reader` (1 KB calls), `io.Copy`, `io.NewScanner` + `Bytes()`, `fs.MkdirAll`, `io.WriteFile`, recursive `os.ReadDir` + `DirEntry.Info`, `fs.RemoveAll` | `os`, `bufio.Writer/Reader/Scanner`, `io.Copy`, `os.WriteFile`, `filepath.WalkDir` + `d.Info()`, `os.RemoveAll` | 1 GB file written and read both ways, copied; 1 GB text (~20M lines) scanned; 100k files of 0-199 bytes in 100 x 10 directories created, walked with stat, removed. Peak disk use 2 GB in `$TMPDIR`/`/tmp` | 16 MB, 2 000 files |
| `sort_collect` | `slices.Sort` (ints, strings), `slices.BinarySearch`, `collections.Deque`, `Heap<int>`, `PriorityQueue<Job>`, `LinkedList`, `OrderedMap`, `strings.Builder`, `text.template.Parse<Page>` + `Execute` | `slices.Sort`, `slices.BinarySearch`, a ring-buffer deque (Go has none), `container/heap`, `container/list`, a sorted-array ordered map (Go has none; the same structure as Zell's), `strings.Builder`, `text/template` | 10M ints, 2M strings, 10M lookups, 10M deque pushes, 1M heap and 1M PQ push+pop, 2M list pushes, 50k ordered-map inserts + 1M gets, 100 MB from ~10-byte pieces, 1M template rows | every size / 100 |

Checksums: sums of decoded fields (ids, quantities, prices in cents, counts,
string bytes), a re-parse of the formatted output compared with the first
parse, SHA-256 of concatenated digests, tags and signatures, XOR of shared
secrets and derived keys, sampled bytes of files, and a hash over every
997th sorted element or every 1009th output byte. Randomized results (ECDSA
signatures, TLS sessions) are checked by counting successful verifies and
echoes.

## How to run

```sh
make -C runtime && cargo build --release -p zell
# smoke run of the reduced sizes
SCALE=small ./scripts/bench-perf.sh json_codec encodings crypto file_io sort_collect
# full size, best of 5 alternating (wall, CPU, RSS of the whole program)
RUNS=5 ./scripts/bench-perf.sh json_codec encodings crypto file_io sort_collect
```

`bench-perf.sh` times the whole process and prints only the first 30
characters of the output. For the per-phase numbers, build the two binaries
and run them alternately, keeping stderr:

```sh
b=crypto; out=$(mktemp -d)
./target/release/zell build benchmarks/$b/main.zl --release -o $out/zl
(cd benchmarks/$b && go build -o $out/go main.go)
for i in 1 2 3 4 5; do for l in zl go; do echo "== $l $i"; $out/$l 2>&1 >/dev/null; done; done
```

`encodings` can run a single format with `FORMAT=xml|yaml|toml`. `crypto`
and `encodings` have a `go.mod` with their dependencies in `vendor/`, so
`go build` works offline. The other three use only Go's standard library. `file_io` writes
to `fs.TempDir()` / `os.TempDir()` (`$TMPDIR`, else `/tmp`) and removes its
directory at the end. It needs 2 GB free.

What is comparable, and what is not:

- TLS: Go's client is limited to X25519 (`CurvePreferences`). Go 1.24 would
  otherwise offer the hybrid X25519MLKEM768, which Zell does not implement.
  Both servers present the P-256 certificate of `examples/tls_echo.zl`. Both
  sides do full handshakes: Go's server has session tickets disabled, and
  Zell's client has no session cache.
- AEAD: Go builds the `cipher.AEAD` once and seals into a reused `dst`.
  Zell's API takes the key on every call and returns a new slice (see open
  items).
- `io.Copy`: Go's copy between two `*os.File` uses `copy_file_range`, so
  the kernel copies the data. Zell's goes through a 32 KB user-space buffer.
- `OrderedMap` and the deque: Go has neither in its standard library, so
  `main.go` has the same data structures written out (a sorted array, a ring
  buffer). This compares the implementations, not two libraries.

## Results (2026-10-22, quiet machine, best of 5 alternating, Go = 1.00)

> **Superseded for `json_codec` and `crypto`** by "Performance wave 2" in
> [README.md](README.md) (2026-10-23, 16-core machine): `encoding.json` was
> rewritten in the C runtime (typed parse of 100 MB 360 ms vs Go 1 070 ms;
> the quadratic tokenizer and open items 1-2 below are gone) and SHA-256 /
> AES-GCM got SHA-NI and VAES paths (parity with Go / 0.93x). The table
> below is kept as the 4 vCPU baseline.

`RUNS=5 DETAIL=1 ./scripts/bench-perf.sh <bench>`: wall, CPU (user+sys)
and peak RSS of the whole process are from the best of 5 runs (by wall
time, Zell and Go alternating); the phase times are that best run's own
stderr lines. "time Zell / Go" below 1 means Zell is faster; for
throughput rows "rate Zell / Go" is the inverse (Zell's MB/s or ops/s over
Go's) and above 1 means Zell is faster. Zell `--release` targets the host
CPU (AVX-512 here), Go `GOAMD64=v1`.

`json_codec` ran with `JSON_DOC_BYTES=8000000` (an 8 MB document, 78k
items; the message phase is full size, 1M messages). The full 100 MB is
infeasible with the quadratic tokenizer (open item 1): one 8 MB parse
takes 97 s (1 MB 0.78 s, 4 MB 23 s), so one 100 MB parse would take hours;
8 MB is the largest size whose run (two parses) stays under 10 minutes
(305 s). `JSON_DOC_BYTES` was added to both programs for this.

| Benchmark | Phase | Zell | Go | time Zell / Go (< 1 better) | rate Zell / Go (> 1 better) |
|---|---|---|---|---|---|
| json_codec | whole program, 8 MB doc: wall s (CPU s), peak RSS | 305.3 s (301.7), 283 MB | 5.83 s (6.41), 102 MB | 52.40 (RSS 2.78) | - |
| json_codec | typed parse 8 MB (`Parse<Doc>`) | 96 879 ms | 195 ms | 496.82 | - |
| json_codec | typed format | 307 ms | 64 ms | 4.80 | - |
| json_codec | Value parse 8 MB | 98 231 ms | 212 ms | 463.35 | - |
| json_codec | Value lookups | 40 ms | 7 ms | 5.71 | - |
| json_codec | Value format | 468 ms | 204 ms | 2.29 | - |
| json_codec | 1M small messages (msg/s) | 12 973 ms (77 077) | 4 815 ms (207 641) | 2.69 | 0.37 |
| encodings | whole program: wall s (CPU s), peak RSS | 27.7 s (28.6), 961 MB | 30.1 s (43.7), 2 945 MB | 0.92 (RSS 0.33) | - |
| encodings | XML parse 20 MB | 3 308 ms | 1 563 ms | 2.12 | - |
| encodings | XML format | 1 269 ms | 565 ms | 2.25 | - |
| encodings | YAML parse 20 MB | 2 328 ms | 3 436 ms | 0.68 | - |
| encodings | YAML format | 1 977 ms | 11 905 ms | 0.17 | - |
| encodings | TOML parse 20 MB | 4 598 ms | 2 618 ms | 1.76 | - |
| encodings | TOML format | 1 529 ms | 1 964 ms | 0.78 | - |
| crypto | whole program: wall s (CPU s), peak RSS | 44.8 s (44.9), 77 MB | 15.0 s (16.1), 142 MB | 2.98 (RSS 0.54) | - |
| crypto | SHA-256 1 GB (MB/s) | 5 215 ms (205) | 3 089 ms (347) | 1.69 | 0.59 |
| crypto | SHA-512 1 GB (MB/s) | 3 116 ms (344) | 2 024 ms (530) | 1.54 | 0.65 |
| crypto | AES-256-GCM seal 1 GB (MB/s) | 1 107 ms (969) | 220 ms (4 867) | 5.03 | 0.20 |
| crypto | ChaCha20-Poly1305 seal 1 GB (MB/s) | 4 447 ms (241) | 575 ms (1 865) | 7.73 | 0.13 |
| crypto | X25519 20k (ops/s) | 1 492 ms (13 403) | 2 472 ms (8 090) | 0.60 | 1.66 |
| crypto | Ed25519 sign 20k (ops/s) | 5 687 ms (3 516) | 580 ms (34 476) | 9.81 | 0.10 |
| crypto | Ed25519 verify 20k (ops/s) | 5 201 ms (3 845) | 1 449 ms (13 793) | 3.59 | 0.28 |
| crypto | RSA-2048 sign 500 (ops/s) | 956 ms (522) | 756 ms (660) | 1.26 | 0.79 |
| crypto | RSA-2048 verify 5000 (ops/s) | 528 ms (9 454) | 218 ms (22 872) | 2.42 | 0.41 |
| crypto | P-256 sign 10k (ops/s) | 4 404 ms (2 270) | 498 ms (20 065) | 8.84 | 0.11 |
| crypto | P-256 verify 10k (ops/s) | 8 065 ms (1 239) | 920 ms (10 861) | 8.77 | 0.11 |
| crypto | HKDF-SHA256 200k (ops/s) | 534 ms (374 466) | 634 ms (315 102) | 0.84 | 1.19 |
| crypto | Argon2id x8 (ops/s) | 817 ms (9) | 448 ms (17) | 1.82 | 0.55 |
| crypto | TLS 1.3 handshakes 1000 (ops/s) | 3 155 ms (316) | 1 081 ms (924) | 2.92 | 0.34 |
| file_io | whole program: wall s (CPU s), peak RSS | 30.6 s (29.2), 10.9 MB | 29.3 s (29.9), 11.0 MB | 1.04 (RSS 0.99) | - |
| file_io | write 1 GB, 1 MB chunks (MB/s) | 429 ms (2 498) | 306 ms (3 502) | 1.40 | 0.71 |
| file_io | read 1 GB, 1 MB chunks (MB/s) | 178 ms (6 028) | 176 ms (6 092) | 1.01 | 0.99 |
| file_io | buffered write, 1 KB calls (MB/s) | 2 378 ms (451) | 981 ms (1 093) | 2.42 | 0.41 |
| file_io | buffered read, 1 KB calls (MB/s) | 2 673 ms (401) | 289 ms (3 708) | 9.25 | 0.11 |
| file_io | io.Copy 1 GB (MB/s) | 534 ms (2 008) | 443 ms (2 423) | 1.21 | 0.83 |
| file_io | line scan 1 GB (MB/s) | 2 669 ms (402) | 937 ms (1 145) | 2.85 | 0.35 |
| file_io | create 100k files | 1 850 ms | 15 966 ms | 0.12 | - |
| file_io | walk + stat 100k files | 197 ms | 269 ms | 0.73 | - |
| file_io | remove tree | 1 274 ms | 1 170 ms | 1.09 | - |
| sort_collect | whole program: wall s (CPU s), peak RSS | 23.8 s (23.6), 1 598 MB | 15.0 s (18.3), 294 MB | 1.58 (RSS 5.44) | - |
| sort_collect | sort 10M ints | 1 373 ms | 1 164 ms | 1.18 | - |
| sort_collect | binary search 10M | 5 829 ms | 5 611 ms | 1.04 | - |
| sort_collect | sort 2M strings | 3 243 ms | 995 ms | 3.26 | - |
| sort_collect | deque 10M | 223 ms | 198 ms | 1.13 | - |
| sort_collect | heap 1M push+pop | 214 ms | 450 ms | 0.48 | - |
| sort_collect | priority queue 1M | 803 ms | 610 ms | 1.32 | - |
| sort_collect | linked list 2M | 1 693 ms | 299 ms | 5.66 | - |
| sort_collect | ordered map 50k inserts + 1M gets | 727 ms | 282 ms | 2.58 | - |
| sort_collect | strings.Builder 100 MB | 3 592 ms | 1 682 ms | 2.14 | - |
| sort_collect | text.template 1M rows | 5 124 ms | 1 649 ms | 3.11 | - |

Verdicts: `json_codec` is unusable for large documents until open item 1
is fixed (500x on a parse; even the 200-byte messages are 2.7x Go);
`encodings` is at parity overall (YAML far ahead of yaml.v3, XML 2x
behind, TOML parse 1.8x behind) with a third of Go's memory; `crypto` is
2-10x behind except X25519 and HKDF (ahead) and RSA sign (1.26x): the
AEADs lack AES-NI/SIMD-class speed (AES-GCM 5x, ChaCha 7.7x), Ed25519
sign and P-256 are 9-10x (open item 6 and no fixed-base tables);
`file_io` is at parity for large I/O, far ahead creating files (Go
15.9 s vs 1.9 s for the same `MkdirAll` + `WriteFile` calls, 160 vs
18 us per file; the cause on the Go side was not investigated),
but 2.4-9x behind for buffered 1 KB calls and line scanning (the Zell
`io` buffered reader/scanner path); `sort_collect` is 1.6x behind overall
and uses 5.4x the memory (`strings.Builder` keeps every piece, open item
11; `LinkedList` 5.7x, string sort 3.3x, `text.template` 3.1x).

## Open items found while writing the benchmarks

These come from reading the code and from the small smoke runs, not from
timings.

1. **Fixed: the JSON tokenizer was quadratic** (`scan_string` allocated
   and zeroed a buffer the size of the rest of the document for every
   string token, so 4x the input took 16x the time). `encoding.json` now
   parses in C in one pass into an arena document of 16-byte nodes
   (`runtime/src/std_json.c`, ABI.md 17.4); `json.Value` is a node of that
   document, `Parse<T>` walks the nodes directly (no token slice, no
   `Value` tree, error paths built only on an error) and `Format<T>`
   appends to a C writer. The full-size run is in the table above.
2. **Fixed: `json.Format` wrote `null` for an empty slice that came from
   `[]`.** The decoder now produces an empty non-nil slice for `[]` (a
   slice with a backing array but no elements; `[z][0..0]` in Zell), so a
   round trip keeps `[]`; nil slices still format as `null`
   (examples/README.md decision 22). The typed output of `json_codec` is
   byte-identical to Go's.
3. **`xml.Format` ignores `@serde(name:)` on attribute fields.**
   `encodeElem` (`std/encoding/xml/xml.zl`, around line 806) names an
   `@xml(attr: true)` attribute from `tag.name`, else the field name.
   `decodeElem` also uses `@serde(name:)`. So
   `@serde(name: "id") @xml(attr: true) Id int` parses `id="8"` but formats
   `Id="8"`, and that output parses back with `Id = 0`. The encoder's
   attribute loop also does not skip `@serde(skip: true)` fields. The
   benchmark works around it with `@xml(attr: true, name: "id")`.
4. **No public streaming hash.** `crypto.hash` exports only one-shot
   functions: `Sha256(string)`, `Sha512(string)` and `Sum(alg, []byte)`.
   The runtime has an incremental hasher (`rtNew`/`rtWrite`/`rtSum`,
   `zell_crypto_hash_rt_*`), but no Zell type wraps it, and no
   `hash.Hash`-style `New("sha256")` with `Write`/`Sum` exists. A 1 GB
   stream cannot get one digest without holding all of it in memory. The
   benchmark hashes each 1 MB chunk separately, and Go does the same
   (`Reset` per chunk) to match.
5. **The AEAD API cannot reuse a key schedule or an output buffer.**
   `aes.GcmSeal(key, nonce, pt, aad)` and `chacha.Seal` take the raw key
   on every call: the AES key schedule and GHASH setup run per message, and
   every call allocates a new output slice. Go builds the AEAD once and
   seals into `dst[:0]`. For 1 MB messages this barely matters; for small
   records (TLS-like sizes) it does. Suggestion: an `aes.NewGcm(key)` /
   `chacha.New(key)` object with `Seal(dst, nonce, pt, aad)`.
6. **Ed25519 signing does two base-point multiplications per signature.**
   `zell__ed25519_sign` (`runtime/src/crypto/ed25519.c:282`) takes only the
   seed. On every call it re-derives the public key (`ge_scalarmult` of the
   base point), then computes R with a second generic `ge_scalarmult` of
   the base point, and runs `consts_init` again. Go keeps the public key in
   its 64-byte `PrivateKey` and uses a precomputed fixed-base table. The
   smoke run already shows signing about 10x slower than Go (to be
   confirmed on a quiet machine). Suggestions: an expanded-key type, or a
   `Sign(priv64, msg)` that takes the public key with it, and a fixed-base
   comb table.
7. **No `fs.WalkDir`/`filepath.WalkDir`.** The tree walk is a hand-written
   recursion over `os.ReadDir` with `DirEntry.Info()` (one `lstat` per
   entry, as in Go's `d.Info()`). It works. Walking a directory tree is
   common enough to deserve an API in `fs` (with `SkipDir`).
8. **`io.Copy` has no fast path.** It always copies through a 32 KB
   user-space buffer (`std/io/io.zl:179`). There is no
   `ReaderFrom`/`WriterTo` protocol, and no `copy_file_range` or `sendfile`
   for file-to-file and file-to-socket copies, which Go uses on Linux.
9. **TLS config has no curve or group preferences.** `tls.Config` has no
   `CurvePreferences`, and the client always offers X25519, P-256 and
   P-384. There is no post-quantum hybrid (X25519MLKEM768, Go 1.24's
   default), so a default Go client and a Zell server negotiate classical
   X25519 only.
10. **No `make([]T, n)`.** Buffers are built by appending in a loop
    (`slices.Of(byte(0), n)`, `io.makeBuf`), which costs one append per
    element. That is an awkward idiom for I/O buffers. A zeroed-allocation builtin or `slices.Make<T>(n)` would
    help. (2026-10-23: the stdlib has an internal `strings.rtMakeBytes(n)`
    for its own buffers; the public builtin is still open.)
11. **Fixed: `strings.Builder` kept every piece** (2026-10-23). It stored a
    `[]string` of parts and joined them in `String()`; it now appends into
    one `[]byte` whose capacity doubles and builds the string once.
    Builder 100 MB: 1471 -> 645 ms (Go 286); the rest is the benchmark's
    `itoa` through `fmt.Format`. In the same pass the `io` buffers moved
    bytes with `memmove`/`memchr` instead of Zell loops (buffered write
    1 KB 856 -> 424 ms, Go 433; buffered read 1 KB 753 -> 218 ms, Go 215;
    scan lines 935 -> 610 ms, Go 521), and `slices.Sort<string>` became an
    introsort in C over the string pointers with no refcount traffic
    (sort 2M strings 1363 -> 480 ms, Go 376; a merge sort in C was 2.6 s:
    its per-step branch waits for a cache miss on a scattered string
    object, a partition overlaps the misses against one cached pivot).
12. **`collections.OrderedMap` is a sorted array.** `Set` inserts in O(n)
    by shifting element by element (`std/collections/collections2.zl:650`).
    It is not a tree or B-tree, so 1M random inserts would be quadratic.
    The benchmark uses 50k inserts, and Go gets the same structure.
13. **`text.template.Execute` returns a string and converts all the data
    first.** `Execute` turns the whole data value into a dynamic `Value`
    tree (1M rows become 1M `Value` structs with field slices) before it
    renders. It has no `io.Writer` variant, so the output of 1M rows is
    built as one string. Go streams to a writer and reflects lazily.

Smaller notes: `crypto.hash.Sum` picks the algorithm by name on every
call. `ecdsa.Sign` takes a bare 32-byte scalar and returns DER, so a Go
key has to be rebuilt from the scalar for the comparison. `os.ReadDir`
returns entries sorted, as Go's does.
