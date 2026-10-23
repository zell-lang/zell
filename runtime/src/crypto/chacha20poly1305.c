/* chacha20poly1305.c - ChaCha20, Poly1305 and the ChaCha20-Poly1305 AEAD
 * (RFC 8439). Constant time by construction (only adds, rotates, xors and
 * 64x64->128 multiplies; Poly1305 uses radix 2^44 limbs, final reduction by
 * masks). On x86-64 with AVX2 (zell__cpu_has_avx2; ZELL_NO_AVX2=1 disables
 * it) ChaCha20 runs 8 blocks per pass; the portable code is the reference
 * and handles the tail. */
#include "crypto.h"

#include <string.h>

#define ROTL(x, n) (((x) << (n)) | ((x) >> (32 - (n))))
#define QR(a, b, c, d)                    \
    a += b; d ^= a; d = ROTL(d, 16);      \
    c += d; b ^= c; b = ROTL(b, 12);      \
    a += b; d ^= a; d = ROTL(d, 8);       \
    c += d; b ^= c; b = ROTL(b, 7)

static void chacha_block(const uint32_t in[16], uint8_t out[64]) {
    uint32_t x[16];
    memcpy(x, in, sizeof x);
    for (int i = 0; i < 10; i++) {
        QR(x[0], x[4], x[8], x[12]);
        QR(x[1], x[5], x[9], x[13]);
        QR(x[2], x[6], x[10], x[14]);
        QR(x[3], x[7], x[11], x[15]);
        QR(x[0], x[5], x[10], x[15]);
        QR(x[1], x[6], x[11], x[12]);
        QR(x[2], x[7], x[8], x[13]);
        QR(x[3], x[4], x[9], x[14]);
    }
    for (int i = 0; i < 16; i++) zell__st32le(out + 4 * i, x[i] + in[i]);
    zell__crypto_wipe(x, sizeof x);
}

static void chacha_setup(uint32_t st[16], const uint8_t key[32], const uint8_t nonce[12], uint32_t counter) {
    st[0] = 0x61707865; st[1] = 0x3320646e; st[2] = 0x79622d32; st[3] = 0x6b206574;
    for (int i = 0; i < 8; i++) st[4 + i] = zell__ld32le(key + 4 * i);
    st[12] = counter;
    for (int i = 0; i < 3; i++) st[13 + i] = zell__ld32le(nonce + 4 * i);
}

/* Portable path: one block at a time (also the tail of the AVX2 path). */
static void chacha_blocks(uint32_t st[16], const uint8_t *in, size_t n, uint8_t *out) {
    uint8_t ks[64];
    while (n) {
        chacha_block(st, ks);
        st[12]++;
        size_t m = n < 64 ? n : 64;
        for (size_t i = 0; i < m; i++) out[i] = in[i] ^ ks[i];
        in += m;
        out += m;
        n -= m;
    }
    zell__crypto_wipe(ks, sizeof ks);
}

/* ---- Poly1305 (poly1305-donna-64 style, 3 x 44/44/42-bit limbs) ---- */

typedef unsigned __int128 u128;
#if defined(__clang__)
#define ZELL_UNROLL4 _Pragma("clang loop unroll(full)")
#elif defined(__GNUC__)
#define ZELL_UNROLL4 _Pragma("GCC unroll 4")
#else
#define ZELL_UNROLL4
#endif
#define M44 0xfffffffffffULL
#define M42 0x3ffffffffffULL

typedef struct {
    uint64_t r[3], h[3], pad[2];
    uint64_t rp[3][3];   /* r^2, r^3, r^4 (set by poly_powers once n >= 64) */
    bool has_powers;
} poly_state;

/* o = a * b mod 2^130-5, limbs partially reduced (about 2^44 + epsilon). */
static void poly_mul(uint64_t o[3], const uint64_t a[3], const uint64_t b[3]) {
    uint64_t s1 = b[1] * (5 << 2), s2 = b[2] * (5 << 2);
    u128 d0 = (u128)a[0] * b[0] + (u128)a[1] * s2 + (u128)a[2] * s1;
    u128 d1 = (u128)a[0] * b[1] + (u128)a[1] * b[0] + (u128)a[2] * s2;
    u128 d2 = (u128)a[0] * b[2] + (u128)a[1] * b[1] + (u128)a[2] * b[0];
    uint64_t c = (uint64_t)(d0 >> 44); o[0] = (uint64_t)d0 & M44;
    d1 += c; c = (uint64_t)(d1 >> 44); o[1] = (uint64_t)d1 & M44;
    d2 += c; c = (uint64_t)(d2 >> 42); o[2] = (uint64_t)d2 & M42;
    o[0] += c * 5; c = o[0] >> 44; o[0] &= M44;
    o[1] += c;
}

typedef struct {
    uint64_t R[4][3], S1[4], S2[4];   /* r^4, r^3, r^2, r and their *20 multiples */
} poly_pre;

static void poly_powers(poly_state *p);

static void poly_prepare(poly_state *p, poly_pre *pre) {
    if (!p->has_powers) poly_powers(p);
    const uint64_t *R[4] = { p->rp[2], p->rp[1], p->rp[0], p->r };
    for (int k = 0; k < 4; k++) {
        for (int j = 0; j < 3; j++) pre->R[k][j] = R[k][j];
        pre->S1[k] = R[k][1] * (5 << 2);
        pre->S2[k] = R[k][2] * (5 << 2);
    }
}

/* Four full blocks with one reduction: h = (h+m0) r^4 + m1 r^3 + m2 r^2 + m3 r;
 * the four products are independent, so their latencies overlap. */
static inline __attribute__((always_inline)) void poly_group4(uint64_t h[3], const uint8_t *m, const poly_pre *pre) {
    u128 d0 = 0, d1 = 0, d2 = 0;
    ZELL_UNROLL4
    for (int k = 0; k < 4; k++) {
        uint64_t t0 = zell__ld64le(m + 16 * k), t1 = zell__ld64le(m + 16 * k + 8);
        uint64_t a0 = t0 & M44, a1 = ((t0 >> 44) | (t1 << 20)) & M44, a2 = ((t1 >> 24) & M42) | (1ULL << 40);
        if (k == 0) { a0 += h[0]; a1 += h[1]; a2 += h[2]; }
        d0 += (u128)a0 * pre->R[k][0] + (u128)a1 * pre->S2[k] + (u128)a2 * pre->S1[k];
        d1 += (u128)a0 * pre->R[k][1] + (u128)a1 * pre->R[k][0] + (u128)a2 * pre->S2[k];
        d2 += (u128)a0 * pre->R[k][2] + (u128)a1 * pre->R[k][1] + (u128)a2 * pre->R[k][0];
    }
    uint64_t c = (uint64_t)(d0 >> 44); h[0] = (uint64_t)d0 & M44;
    d1 += c; c = (uint64_t)(d1 >> 44); h[1] = (uint64_t)d1 & M44;
    d2 += c; c = (uint64_t)(d2 >> 42); h[2] = (uint64_t)d2 & M42;
    h[0] += c * 5; c = h[0] >> 44; h[0] &= M44;
    h[1] += c;
}

static void poly_powers(poly_state *p) {
    poly_mul(p->rp[0], p->r, p->r);
    poly_mul(p->rp[1], p->rp[0], p->r);
    poly_mul(p->rp[2], p->rp[1], p->r);
    p->has_powers = true;
}

static void poly_init(poly_state *p, const uint8_t key[32]) {
    uint64_t t0 = zell__ld64le(key), t1 = zell__ld64le(key + 8);
    p->r[0] = t0 & 0xffc0fffffffULL;
    p->r[1] = ((t0 >> 44) | (t1 << 20)) & 0xfffffc0ffffULL;
    p->r[2] = (t1 >> 24) & 0x00ffffffc0fULL;
    p->h[0] = p->h[1] = p->h[2] = 0;
    p->pad[0] = zell__ld64le(key + 16);
    p->pad[1] = zell__ld64le(key + 24);
    p->has_powers = false;
}

/* Absorbs whole 16-byte blocks; hibit is 1<<40 for full blocks. */
static void poly_blocks(poly_state *p, const uint8_t *m, size_t n, uint64_t hibit) {
    uint64_t r0 = p->r[0], r1 = p->r[1], r2 = p->r[2];
    uint64_t s1 = r1 * (5 << 2), s2 = r2 * (5 << 2);
    uint64_t h0 = p->h[0], h1 = p->h[1], h2 = p->h[2];
    if (n >= 64 && hibit) {
        poly_pre pre;
        poly_prepare(p, &pre);
        uint64_t h[3] = { h0, h1, h2 };
        while (n >= 64) {
            poly_group4(h, m, &pre);
            m += 64;
            n -= 64;
        }
        h0 = h[0]; h1 = h[1]; h2 = h[2];
        zell__crypto_wipe(&pre, sizeof pre);
    }
    while (n >= 16) {
        uint64_t t0 = zell__ld64le(m), t1 = zell__ld64le(m + 8);
        h0 += t0 & M44;
        h1 += ((t0 >> 44) | (t1 << 20)) & M44;
        h2 += ((t1 >> 24) & M42) | hibit;
        u128 d0 = (u128)h0 * r0 + (u128)h1 * s2 + (u128)h2 * s1;
        u128 d1 = (u128)h0 * r1 + (u128)h1 * r0 + (u128)h2 * s2;
        u128 d2 = (u128)h0 * r2 + (u128)h1 * r1 + (u128)h2 * r0;
        uint64_t c = (uint64_t)(d0 >> 44); h0 = (uint64_t)d0 & M44;
        d1 += c; c = (uint64_t)(d1 >> 44); h1 = (uint64_t)d1 & M44;
        d2 += c; c = (uint64_t)(d2 >> 42); h2 = (uint64_t)d2 & M42;
        h0 += c * 5; c = h0 >> 44; h0 &= M44;
        h1 += c;
        m += 16;
        n -= 16;
    }
    p->h[0] = h0; p->h[1] = h1; p->h[2] = h2;
}

/* Absorbs n bytes; a trailing partial block is padded with 0x01 then zeros
 * (plain Poly1305), or with zeros only as a full block (the AEAD's
 * pad16 framing) when pad16. */
static void poly_update(poly_state *p, const uint8_t *m, size_t n, bool pad16) {
    size_t full = n & ~(size_t)15;
    poly_blocks(p, m, full, 1ULL << 40);
    if (n > full) {
        uint8_t blk[16] = { 0 };
        memcpy(blk, m + full, n - full);
        if (pad16) {
            poly_blocks(p, blk, 16, 1ULL << 40);
        } else {
            blk[n - full] = 1;
            poly_blocks(p, blk, 16, 0);
        }
    }
}

static void poly_finish(poly_state *p, uint8_t tag[16]) {
    uint64_t h0 = p->h[0], h1 = p->h[1], h2 = p->h[2], c;
    c = h1 >> 44; h1 &= M44; h2 += c;
    c = h2 >> 42; h2 &= M42; h0 += c * 5;
    c = h0 >> 44; h0 &= M44; h1 += c;
    c = h1 >> 44; h1 &= M44; h2 += c;
    c = h2 >> 42; h2 &= M42; h0 += c * 5;
    c = h0 >> 44; h0 &= M44; h1 += c;
    /* g = h + 5 - 2^130 */
    uint64_t g0 = h0 + 5; c = g0 >> 44; g0 &= M44;
    uint64_t g1 = h1 + c; c = g1 >> 44; g1 &= M44;
    uint64_t g2 = h2 + c - (1ULL << 42);
    uint64_t mask = (g2 >> 63) - 1;   /* all ones if h >= p */
    g0 &= mask; g1 &= mask; g2 &= mask;
    mask = ~mask;
    h0 = (h0 & mask) | g0; h1 = (h1 & mask) | g1; h2 = (h2 & mask) | g2;
    uint64_t lo = h0 | (h1 << 44), hi = (h1 >> 20) | (h2 << 24);
    u128 f = (u128)lo + p->pad[0];
    lo = (uint64_t)f;
    hi = hi + p->pad[1] + (uint64_t)(f >> 64);
    zell__st64le(tag, lo);
    zell__st64le(tag + 8, hi);
    zell__crypto_wipe(p, sizeof *p);
}

void zell__poly1305(const uint8_t key[32], const uint8_t *msg, size_t n, uint8_t tag[16]) {
    poly_state p;
    poly_init(&p, key);
    poly_update(&p, msg, n, false);
    poly_finish(&p, tag);
}

#if defined(__x86_64__) && (defined(__GNUC__) || defined(__clang__))
#include <immintrin.h>
#define CHACHA_AVX2 1

#define VROT(x, n) _mm256_or_si256(_mm256_slli_epi32((x), (n)), _mm256_srli_epi32((x), 32 - (n)))
#define VQR(a, b, c, d, r16, r8)                                   \
    a = _mm256_add_epi32(a, b); d = _mm256_xor_si256(d, a); d = _mm256_shuffle_epi8(d, r16); \
    c = _mm256_add_epi32(c, d); b = _mm256_xor_si256(b, c); b = VROT(b, 12);                \
    a = _mm256_add_epi32(a, b); d = _mm256_xor_si256(d, a); d = _mm256_shuffle_epi8(d, r8);  \
    c = _mm256_add_epi32(c, d); b = _mm256_xor_si256(b, c); b = VROT(b, 7)

/* 8x8 transpose of 32-bit words: a[i] holds word i of 8 blocks; afterwards
 * o[j] holds words 0..7 of block j. */
__attribute__((target("avx2"))) static inline void transpose8(const __m256i a[8], __m256i o[8]) {
    __m256i t0 = _mm256_unpacklo_epi32(a[0], a[1]), t1 = _mm256_unpackhi_epi32(a[0], a[1]);
    __m256i t2 = _mm256_unpacklo_epi32(a[2], a[3]), t3 = _mm256_unpackhi_epi32(a[2], a[3]);
    __m256i t4 = _mm256_unpacklo_epi32(a[4], a[5]), t5 = _mm256_unpackhi_epi32(a[4], a[5]);
    __m256i t6 = _mm256_unpacklo_epi32(a[6], a[7]), t7 = _mm256_unpackhi_epi32(a[6], a[7]);
    __m256i u0 = _mm256_unpacklo_epi64(t0, t2), u1 = _mm256_unpackhi_epi64(t0, t2);
    __m256i u2 = _mm256_unpacklo_epi64(t1, t3), u3 = _mm256_unpackhi_epi64(t1, t3);
    __m256i v0 = _mm256_unpacklo_epi64(t4, t6), v1 = _mm256_unpackhi_epi64(t4, t6);
    __m256i v2 = _mm256_unpacklo_epi64(t5, t7), v3 = _mm256_unpackhi_epi64(t5, t7);
    o[0] = _mm256_permute2x128_si256(u0, v0, 0x20); o[4] = _mm256_permute2x128_si256(u0, v0, 0x31);
    o[1] = _mm256_permute2x128_si256(u1, v1, 0x20); o[5] = _mm256_permute2x128_si256(u1, v1, 0x31);
    o[2] = _mm256_permute2x128_si256(u2, v2, 0x20); o[6] = _mm256_permute2x128_si256(u2, v2, 0x31);
    o[3] = _mm256_permute2x128_si256(u3, v3, 0x20); o[7] = _mm256_permute2x128_si256(u3, v3, 0x31);
}

/* XORs the largest multiple of 512 bytes of n with the keystream of blocks
 * st[12], st[12]+1, ... (st[12] advances) and returns it. With p != NULL the
 * Poly1305 hash of the ciphertext is interleaved into the round loop (the
 * scalar multiplies and the vector rounds use different execution ports, so
 * they overlap): p absorbs exactly the returned number of ciphertext bytes.
 * seal: the ciphertext of chunk i exists after chunk i, so poly lags one
 * chunk; open: it is the input of the current chunk, read before it is
 * overwritten (in == out is fine). Branches depend on public lengths only. */
__attribute__((target("avx2"))) static inline __attribute__((always_inline)) size_t chacha_avx2_impl(
    uint32_t st[16], const uint8_t *in, size_t n, uint8_t *out, poly_state *p, int seal) {
    const __m256i r16 = _mm256_setr_epi8(2, 3, 0, 1, 6, 7, 4, 5, 10, 11, 8, 9, 14, 15, 12, 13,
                                         2, 3, 0, 1, 6, 7, 4, 5, 10, 11, 8, 9, 14, 15, 12, 13);
    const __m256i r8 = _mm256_setr_epi8(3, 0, 1, 2, 7, 4, 5, 6, 11, 8, 9, 10, 15, 12, 13, 14,
                                        3, 0, 1, 2, 7, 4, 5, 6, 11, 8, 9, 10, 15, 12, 13, 14);
    const __m256i lanes = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
    size_t done = 0;
    __m256i x[16], o[16];
    poly_pre pre;
    uint64_t h[3] = { 0, 0, 0 };
    if (p) {
        poly_prepare(p, &pre);
        h[0] = p->h[0]; h[1] = p->h[1]; h[2] = p->h[2];
    }
    while (n - done >= 512) {
        for (int i = 0; i < 16; i++) x[i] = _mm256_set1_epi32((int)st[i]);
        x[12] = _mm256_add_epi32(x[12], lanes);
        for (int i = 0; i < 16; i++) o[i] = x[i];
#define DOUBLE_ROUND()                                  \
        VQR(x[0], x[4], x[8], x[12], r16, r8);          \
        VQR(x[1], x[5], x[9], x[13], r16, r8);          \
        VQR(x[2], x[6], x[10], x[14], r16, r8);         \
        VQR(x[3], x[7], x[11], x[15], r16, r8);         \
        VQR(x[0], x[5], x[10], x[15], r16, r8);         \
        VQR(x[1], x[6], x[11], x[12], r16, r8);         \
        VQR(x[2], x[7], x[8], x[13], r16, r8);          \
        VQR(x[3], x[4], x[9], x[14], r16, r8)
        const uint8_t *pm = seal ? out + done - 512 : in + done;
        int dop = p && (!seal || done);
        for (int i = 0; i < 8; i++) {
            DOUBLE_ROUND();
            if (dop) poly_group4(h, pm + 64 * i, &pre);
        }
        for (int i = 8; i < 10; i++) {
            DOUBLE_ROUND();
        }
#undef DOUBLE_ROUND
        for (int i = 0; i < 16; i++) x[i] = _mm256_add_epi32(x[i], o[i]);
        __m256i lo[8], hi[8];
        transpose8(x, lo);
        transpose8(x + 8, hi);
        for (int j = 0; j < 8; j++) {
            const uint8_t *ip = in + done + 64 * j;
            uint8_t *op = out + done + 64 * j;
            _mm256_storeu_si256((__m256i *)op, _mm256_xor_si256(lo[j], _mm256_loadu_si256((const __m256i *)ip)));
            _mm256_storeu_si256((__m256i *)(op + 32),
                                _mm256_xor_si256(hi[j], _mm256_loadu_si256((const __m256i *)(ip + 32))));
        }
        st[12] += 8;
        done += 512;
    }
    if (p) {
        if (seal && done)
            for (int i = 0; i < 8; i++) poly_group4(h, out + done - 512 + 64 * i, &pre);
        p->h[0] = h[0]; p->h[1] = h[1]; p->h[2] = h[2];
        zell__crypto_wipe(&pre, sizeof pre);
    }
    zell__crypto_wipe(x, sizeof x);
    zell__crypto_wipe(o, sizeof o);
    return done;
}

__attribute__((target("avx2"))) static size_t chacha_avx2(uint32_t st[16], const uint8_t *in, size_t n, uint8_t *out) {
    return chacha_avx2_impl(st, in, n, out, NULL, 0);
}

__attribute__((target("avx2"))) static size_t chacha_avx2_poly(uint32_t st[16], const uint8_t *in, size_t n,
                                                               uint8_t *out, poly_state *p, int seal) {
    return seal ? chacha_avx2_impl(st, in, n, out, p, 1) : chacha_avx2_impl(st, in, n, out, p, 0);
}
#endif

void zell__chacha20(const uint8_t key[32], const uint8_t nonce[12], uint32_t counter, const uint8_t *in,
                     size_t n, uint8_t *out) {
    uint32_t st[16];
    chacha_setup(st, key, nonce, counter);
#ifdef CHACHA_AVX2
    if (n >= 512 && zell__cpu_has_avx2()) {
        size_t d = chacha_avx2(st, in, n, out);
        in += d;
        out += d;
        n -= d;
    }
#endif
    chacha_blocks(st, in, n, out);
    zell__crypto_wipe(st, sizeof st);
}

/* ---- AEAD (RFC 8439 2.8) ---- */

static void aead_finish(poly_state *p, size_t alen, size_t n, uint8_t tag[16]) {
    uint8_t lens[16];
    zell__st64le(lens, (uint64_t)alen);
    zell__st64le(lens + 8, (uint64_t)n);
    poly_blocks(p, lens, 16, 1ULL << 40);
    poly_finish(p, tag);
}

static void aead_start(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                       poly_state *p) {
    uint8_t otk[64] = { 0 };
    zell__chacha20(key, nonce, 0, otk, 64, otk);
    poly_init(p, otk);
    zell__crypto_wipe(otk, sizeof otk);
    poly_update(p, aad, alen, true);
}

/* Encrypts/decrypts in -> out (counter 1) and computes the tag over the
 * ciphertext (out when sealing, in when opening), fused where AVX2 allows. */
static void aead_run(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                     const uint8_t *in, size_t n, uint8_t *out, int seal, uint8_t tag[16]) {
    poly_state p;
    aead_start(key, nonce, aad, alen, &p);
    uint32_t st[16];
    chacha_setup(st, key, nonce, 1);
    size_t d = 0;
#ifdef CHACHA_AVX2
    if (n >= 512 && zell__cpu_has_avx2()) d = chacha_avx2_poly(st, in, n, out, &p, seal);
#endif
    const uint8_t *ct = seal ? out : in;
    chacha_blocks(st, in + d, n - d, out + d);
    poly_update(&p, ct + d, n - d, true);
    aead_finish(&p, alen, n, tag);
    zell__crypto_wipe(st, sizeof st);
}

void zell__chacha20poly1305_seal(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                                  const uint8_t *in, size_t n, uint8_t *out) {
    aead_run(key, nonce, aad, alen, in, n, out, 1, out + n);
}

bool zell__chacha20poly1305_open(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                                  const uint8_t *in, size_t n, uint8_t *out) {
    uint8_t tag[16];
    aead_run(key, nonce, aad, alen, in, n, out, 0, tag);
    if (!zell__crypto_ct_eq(tag, in + n, 16)) {
        memset(out, 0, n);
        return false;
    }
    return true;
}
