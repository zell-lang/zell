/* aes.c - AES-128/192/256 encryption (FIPS 197) and AES-GCM (NIST SP 800-38D).
 *
 * Two implementations behind one API:
 *
 *  - portable, constant-time: a 64-bit bitsliced AES that encrypts four
 *    blocks at once (word i holds bit i of all 64 state bytes; lane
 *    16*block + byte). SubBytes is the Boyar-Peralta circuit (113 gates, no
 *    tables), ShiftRows/MixColumns are mask-and-shift lane permutations.
 *    GHASH uses carry-less multiplication built from integer multiplies with
 *    "holes" (no secret-indexed tables), after BearSSL's ghash_ctmul64.
 *  - x86-64 AES-NI + PCLMULQDQ through intrinsics, chosen at run time by
 *    cpuid (zell__cpu_has_aesni; ZELL_NO_AESNI=1 forces the portable path).
 *    GCM runs 8 CTR blocks per batch with an aggregated 8-block GHASH; with
 *    VAES + VPCLMULQDQ (zell__cpu_has_vaes; ZELL_NO_VAES=1 disables) the
 *    batch uses 256-bit registers, two blocks per instruction. These
 *    functions carry __attribute__((target)); the file itself is compiled
 *    for baseline x86-64.
 *
 * The key schedule is always computed by the portable code (constant time)
 * and its byte round keys are what AESENC consumes. Only encryption is
 * implemented: GCM (and CTR) never needs the inverse cipher. */
#include "crypto.h"

#include <string.h>

/* ---------------------------------------------------------------------- */
/* Bitsliced core                                                          */

/* Boyar-Peralta S-box circuit ("A new combinational logic minimization
 * technique with applications to cryptology", 2009), as in BearSSL. q[0] is
 * the least significant bit plane. */
static void bs_sbox(uint64_t *q) {
    uint64_t x0, x1, x2, x3, x4, x5, x6, x7;
    uint64_t y1, y2, y3, y4, y5, y6, y7, y8, y9, y10, y11, y12, y13, y14, y15, y16, y17, y18, y19, y20, y21;
    uint64_t z0, z1, z2, z3, z4, z5, z6, z7, z8, z9, z10, z11, z12, z13, z14, z15, z16, z17;
    uint64_t t0, t1, t2, t3, t4, t5, t6, t7, t8, t9, t10, t11, t12, t13, t14, t15, t16, t17, t18, t19;
    uint64_t t20, t21, t22, t23, t24, t25, t26, t27, t28, t29, t30, t31, t32, t33, t34, t35, t36, t37;
    uint64_t t38, t39, t40, t41, t42, t43, t44, t45, t46, t47, t48, t49, t50, t51, t52, t53, t54, t55;
    uint64_t t56, t57, t58, t59, t60, t61, t62, t63, t64, t65, t66, t67;
    uint64_t s0, s1, s2, s3, s4, s5, s6, s7;

    x0 = q[7]; x1 = q[6]; x2 = q[5]; x3 = q[4];
    x4 = q[3]; x5 = q[2]; x6 = q[1]; x7 = q[0];

    /* top linear transformation */
    y14 = x3 ^ x5; y13 = x0 ^ x6; y9 = x0 ^ x3; y8 = x0 ^ x5;
    t0 = x1 ^ x2; y1 = t0 ^ x7; y4 = y1 ^ x3; y12 = y13 ^ y14;
    y2 = y1 ^ x0; y5 = y1 ^ x6; y3 = y5 ^ y8; t1 = x4 ^ y12;
    y15 = t1 ^ x5; y20 = t1 ^ x1; y6 = y15 ^ x7; y10 = y15 ^ t0;
    y11 = y20 ^ y9; y7 = x7 ^ y11; y17 = y10 ^ y11; y19 = y10 ^ y8;
    y16 = t0 ^ y11; y21 = y13 ^ y16; y18 = x0 ^ y16;

    /* non-linear section */
    t2 = y12 & y15; t3 = y3 & y6; t4 = t3 ^ t2; t5 = y4 & x7;
    t6 = t5 ^ t2; t7 = y13 & y16; t8 = y5 & y1; t9 = t8 ^ t7;
    t10 = y2 & y7; t11 = t10 ^ t7; t12 = y9 & y11; t13 = y14 & y17;
    t14 = t13 ^ t12; t15 = y8 & y10; t16 = t15 ^ t12; t17 = t4 ^ t14;
    t18 = t6 ^ t16; t19 = t9 ^ t14; t20 = t11 ^ t16; t21 = t17 ^ y20;
    t22 = t18 ^ y19; t23 = t19 ^ y21; t24 = t20 ^ y18;

    t25 = t21 ^ t22; t26 = t21 & t23; t27 = t24 ^ t26; t28 = t25 & t27;
    t29 = t28 ^ t22; t30 = t23 ^ t24; t31 = t22 ^ t26; t32 = t31 & t30;
    t33 = t32 ^ t24; t34 = t23 ^ t33; t35 = t27 ^ t33; t36 = t24 & t35;
    t37 = t36 ^ t34; t38 = t27 ^ t36; t39 = t29 & t38; t40 = t25 ^ t39;

    t41 = t40 ^ t37; t42 = t29 ^ t33; t43 = t29 ^ t40; t44 = t33 ^ t37;
    t45 = t42 ^ t41; z0 = t44 & y15; z1 = t37 & y6; z2 = t33 & x7;
    z3 = t43 & y16; z4 = t40 & y1; z5 = t29 & y7; z6 = t42 & y11;
    z7 = t45 & y17; z8 = t41 & y10; z9 = t44 & y12; z10 = t37 & y3;
    z11 = t33 & y4; z12 = t43 & y13; z13 = t40 & y5; z14 = t29 & y2;
    z15 = t42 & y9; z16 = t45 & y14; z17 = t41 & y8;

    /* bottom linear transformation */
    t46 = z15 ^ z16; t47 = z10 ^ z11; t48 = z5 ^ z13; t49 = z9 ^ z10;
    t50 = z2 ^ z12; t51 = z2 ^ z5; t52 = z7 ^ z8; t53 = z0 ^ z3;
    t54 = z6 ^ z7; t55 = z16 ^ z17; t56 = z12 ^ t48; t57 = t50 ^ t53;
    t58 = z4 ^ t46; t59 = z3 ^ t54; t60 = t46 ^ t57; t61 = z14 ^ t57;
    t62 = t52 ^ t58; t63 = t49 ^ t58; t64 = z4 ^ t59; t65 = t61 ^ t62;
    t66 = z1 ^ t63; s0 = t59 ^ t63; s6 = t56 ^ ~t62; s7 = t48 ^ ~t60;
    t67 = t64 ^ t65; s3 = t53 ^ t66; s4 = t51 ^ t66; s5 = t47 ^ t65;
    s1 = t64 ^ ~s3; s2 = t55 ^ ~t67;

    q[7] = s0; q[6] = s1; q[5] = s2; q[4] = s3;
    q[3] = s4; q[2] = s5; q[1] = s6; q[0] = s7;
}

/* 8x8 bit-matrix transpose of the bytes of x (bit c of byte r <-> bit r of byte c). */
static inline uint64_t transpose8(uint64_t x) {
    uint64_t t;
    t = (x ^ (x >> 7)) & 0x00AA00AA00AA00AAULL; x = x ^ t ^ (t << 7);
    t = (x ^ (x >> 14)) & 0x0000CCCC0000CCCCULL; x = x ^ t ^ (t << 14);
    t = (x ^ (x >> 28)) & 0x00000000F0F0F0F0ULL; x = x ^ t ^ (t << 28);
    return x;
}

/* 64 bytes -> 8 bit planes (lane L = byte L). */
static void bs_load(uint64_t q[8], const uint8_t in[64]) {
    for (int i = 0; i < 8; i++) q[i] = 0;
    for (int k = 0; k < 8; k++) {
        uint64_t x = transpose8(zell__ld64le(in + 8 * k));
        for (int c = 0; c < 8; c++) q[c] |= ((x >> (8 * c)) & 0xFF) << (8 * k);
    }
}

static void bs_store(uint8_t out[64], const uint64_t q[8]) {
    for (int k = 0; k < 8; k++) {
        uint64_t x = 0;
        for (int c = 0; c < 8; c++) x |= ((q[c] >> (8 * k)) & 0xFF) << (8 * c);
        zell__st64le(out + 8 * k, transpose8(x));
    }
}

#define REP16(v) ((uint64_t)(v) * 0x0001000100010001ULL)

/* Rotates each 16-bit group right by k bits (k = 4, 8, 12). */
static inline uint64_t rotr16(uint64_t x, int k) {
    uint64_t lo = REP16(0xFFFFu >> k);
    return ((x >> k) & lo) | ((x << (16 - k)) & ~lo);
}

/* ShiftRows: byte 4c+r of a block takes byte 4((c+r)%4)+r. */
static void bs_shift_rows(uint64_t *q) {
    for (int i = 0; i < 8; i++) {
        uint64_t x = q[i];
        q[i] = (x & REP16(0x1111)) | (rotr16(x, 4) & REP16(0x2222)) | (rotr16(x, 8) & REP16(0x4444)) |
               (rotr16(x, 12) & REP16(0x8888));
    }
}

/* Rotation inside each 4-bit group (a column): lane r takes lane (r+k)%4. */
static inline uint64_t rot4_1(uint64_t x) {
    return ((x >> 1) & 0x7777777777777777ULL) | ((x << 3) & 0x8888888888888888ULL);
}
static inline uint64_t rot4_2(uint64_t x) {
    return ((x >> 2) & 0x3333333333333333ULL) | ((x << 2) & 0xCCCCCCCCCCCCCCCCULL);
}
static inline uint64_t rot4_3(uint64_t x) {
    return ((x >> 3) & 0x1111111111111111ULL) | ((x << 1) & 0xEEEEEEEEEEEEEEEEULL);
}

/* MixColumns: out_r = 2(a_r ^ a_{r+1}) ^ a_{r+1} ^ a_{r+2} ^ a_{r+3}. */
static void bs_mix_columns(uint64_t *q) {
    uint64_t r1[8], t[8];
    for (int i = 0; i < 8; i++) {
        r1[i] = rot4_1(q[i]);
        t[i] = q[i] ^ r1[i];
    }
    /* xtime(t): multiply by x modulo x^8 + x^4 + x^3 + x + 1 */
    uint64_t x[8];
    x[0] = t[7];
    x[1] = t[0] ^ t[7];
    x[2] = t[1];
    x[3] = t[2] ^ t[7];
    x[4] = t[3] ^ t[7];
    x[5] = t[4];
    x[6] = t[5];
    x[7] = t[6];
    for (int i = 0; i < 8; i++) q[i] = x[i] ^ r1[i] ^ rot4_2(q[i]) ^ rot4_3(q[i]);
}

/* Encrypts the 4 blocks held in q with bitsliced round keys sk. */
static void bs_encrypt(const uint64_t *sk, unsigned nr, uint64_t *q) {
    for (int i = 0; i < 8; i++) q[i] ^= sk[i];
    for (unsigned r = 1; r < nr; r++) {
        bs_sbox(q);
        bs_shift_rows(q);
        bs_mix_columns(q);
        for (int i = 0; i < 8; i++) q[i] ^= sk[8 * r + i];
    }
    bs_sbox(q);
    bs_shift_rows(q);
    for (int i = 0; i < 8; i++) q[i] ^= sk[8 * nr + i];
}

/* S-box of 4 bytes at once (key schedule). */
static uint32_t sub_word(uint32_t w) {
    uint64_t q[8];
    for (int i = 0; i < 8; i++) {
        uint64_t v = 0;
        for (int b = 0; b < 4; b++) v |= (uint64_t)((w >> (8 * b + i)) & 1) << b;
        q[i] = v;
    }
    bs_sbox(q);
    uint32_t r = 0;
    for (int i = 0; i < 8; i++)
        for (int b = 0; b < 4; b++) r |= (uint32_t)((q[i] >> b) & 1) << (8 * b + i);
    return r;
}

uint8_t zell__aes_sbox_ct(uint8_t x) {
    return (uint8_t)sub_word(x);
}

bool zell__aes_init(zell_aes_key *k, const uint8_t *key, size_t klen) {
    if (klen != 16 && klen != 24 && klen != 32) return false;
    memset(k, 0, sizeof *k);
    unsigned nk = (unsigned)(klen / 4), nr = nk + 6, total = 4 * (nr + 1);
    uint32_t w[60];
    /* words are little-endian loads: byte 0 of the word is the first key byte */
    for (unsigned i = 0; i < nk; i++) w[i] = zell__ld32le(key + 4 * i);
    uint32_t rcon = 1;
    for (unsigned i = nk; i < total; i++) {
        uint32_t t = w[i - 1];
        if (i % nk == 0) {
            t = (t >> 8) | (t << 24);   /* RotWord on little-endian words */
            t = sub_word(t) ^ rcon;
            rcon = (rcon << 1) ^ (0x11b & (0u - (rcon >> 7)));
        } else if (nk > 6 && i % nk == 4) {
            t = sub_word(t);
        }
        w[i] = w[i - nk] ^ t;
    }
    for (unsigned i = 0; i < total; i++) zell__st32le(k->rk + 4 * i, w[i]);
    k->nr = nr;
    k->ni = zell__cpu_has_aesni() ? 1 : 0;
    if (!k->ni) {
        uint8_t rep[64];
        for (unsigned r = 0; r <= nr; r++) {
            for (int b = 0; b < 4; b++) memcpy(rep + 16 * b, k->rk + 16 * r, 16);
            bs_load(k->sk + 8 * r, rep);
        }
        zell__crypto_wipe(rep, sizeof rep);
    }
    zell__crypto_wipe(w, sizeof w);
    return true;
}

/* ---------------------------------------------------------------------- */
/* AES-NI + PCLMULQDQ                                                      */

#if defined(__x86_64__) && (defined(__GNUC__) || defined(__clang__))
#define ZELL_HAVE_AESNI 1
#include <emmintrin.h>
#include <smmintrin.h>
#include <tmmintrin.h>
#include <wmmintrin.h>

#define NI_TARGET __attribute__((target("aes,pclmul,sse4.1,ssse3")))

NI_TARGET static inline __m128i ni_encrypt(const __m128i *rk, unsigned nr, __m128i b) {
    b = _mm_xor_si128(b, rk[0]);
    for (unsigned r = 1; r < nr; r++) b = _mm_aesenc_si128(b, rk[r]);
    return _mm_aesenclast_si128(b, rk[nr]);
}

NI_TARGET static void ni_load_keys(const zell_aes_key *k, __m128i *rk) {
    for (unsigned r = 0; r <= k->nr; r++) rk[r] = _mm_loadu_si128((const __m128i *)(k->rk + 16 * r));
}

NI_TARGET static void ni_encrypt_block(const zell_aes_key *k, const uint8_t in[16], uint8_t out[16]) {
    __m128i rk[15];
    ni_load_keys(k, rk);
    _mm_storeu_si128((__m128i *)out, ni_encrypt(rk, k->nr, _mm_loadu_si128((const __m128i *)in)));
}

/* Reduction of the 256-bit carry-less product hi:lo of two byte-reflected
 * operands: shift left by one, then reduce modulo x^128 + x^7 + x^2 + x + 1
 * (Intel's carry-less multiplication white paper). Linear, so a sum of
 * unreduced products can be reduced once (aggregated GHASH). */
NI_TARGET static inline __m128i ni_reduce(__m128i t3, __m128i t6) {
    __m128i t2, t4, t5, t7, t8, t9;
    t7 = _mm_srli_epi32(t3, 31);
    t8 = _mm_srli_epi32(t6, 31);
    t3 = _mm_slli_epi32(t3, 1);
    t6 = _mm_slli_epi32(t6, 1);
    t9 = _mm_srli_si128(t7, 12);
    t8 = _mm_slli_si128(t8, 4);
    t7 = _mm_slli_si128(t7, 4);
    t3 = _mm_or_si128(t3, t7);
    t6 = _mm_or_si128(t6, t8);
    t6 = _mm_or_si128(t6, t9);
    t7 = _mm_slli_epi32(t3, 31);
    t8 = _mm_slli_epi32(t3, 30);
    t9 = _mm_slli_epi32(t3, 25);
    t7 = _mm_xor_si128(t7, t8);
    t7 = _mm_xor_si128(t7, t9);
    t8 = _mm_srli_si128(t7, 4);
    t7 = _mm_slli_si128(t7, 12);
    t3 = _mm_xor_si128(t3, t7);
    t2 = _mm_srli_epi32(t3, 1);
    t4 = _mm_srli_epi32(t3, 2);
    t5 = _mm_srli_epi32(t3, 7);
    t2 = _mm_xor_si128(t2, t4);
    t2 = _mm_xor_si128(t2, t5);
    t2 = _mm_xor_si128(t2, t8);
    t3 = _mm_xor_si128(t3, t2);
    t6 = _mm_xor_si128(t6, t3);
    return t6;
}

/* GF(2^128) multiplication of byte-reflected operands. */
NI_TARGET static inline __m128i ni_gfmul(__m128i a, __m128i b) {
    __m128i t3, t4, t5, t6;
    t3 = _mm_clmulepi64_si128(a, b, 0x00);
    t4 = _mm_clmulepi64_si128(a, b, 0x10);
    t5 = _mm_clmulepi64_si128(a, b, 0x01);
    t6 = _mm_clmulepi64_si128(a, b, 0x11);
    t4 = _mm_xor_si128(t4, t5);
    t5 = _mm_slli_si128(t4, 8);
    t4 = _mm_srli_si128(t4, 8);
    t3 = _mm_xor_si128(t3, t5);
    t6 = _mm_xor_si128(t6, t4);
    return ni_reduce(t3, t6);
}

NI_TARGET static inline __m128i ni_bswap(__m128i x) {
    return _mm_shuffle_epi8(x, _mm_set_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15));
}

/* Powers of H for aggregated GHASH: hp[i] = H^(i+1) byte-reflected, and
 * hk[i] = hp[i] with both 64-bit halves replaced by their XOR (the middle
 * Karatsuba operand). */
typedef struct {
    __m128i hp[8], hk[8];
} ni_hkeys;

NI_TARGET static void ni_load_hkeys(const zell_gcm_ctx *c, ni_hkeys *k) {
    for (int i = 0; i < 8; i++) {
        k->hp[i] = _mm_loadu_si128((const __m128i *)c->hpow[i]);
        k->hk[i] = _mm_xor_si128(k->hp[i], _mm_shuffle_epi32(k->hp[i], 0x4E));
    }
}

/* H^1..H^8 into c->hpow (from c->h). */
NI_TARGET static void ni_gcm_powers(zell_gcm_ctx *c) {
    __m128i h = ni_bswap(_mm_loadu_si128((const __m128i *)c->h)), p = h;
    for (int i = 0; i < 8; i++) {
        _mm_storeu_si128((__m128i *)c->hpow[i], p);
        p = ni_gfmul(p, h);
    }
}

/* Accumulates the unreduced Karatsuba product of block x and key i. */
#define NI_KMUL(x, i)                                                                        \
    do {                                                                                     \
        __m128i x_ = (x);                                                                    \
        lo = _mm_xor_si128(lo, _mm_clmulepi64_si128(x_, hk->hp[i], 0x00));                 \
        hi = _mm_xor_si128(hi, _mm_clmulepi64_si128(x_, hk->hp[i], 0x11));                 \
        mid = _mm_xor_si128(mid, _mm_clmulepi64_si128(_mm_xor_si128(x_, _mm_shuffle_epi32(x_, 0x4E)), \
                                                      hk->hk[i], 0x00));                     \
    } while (0)

/* y = (...((y ^ b[0]) H ^ b[1]) H ... ^ b[m-1]) H for m (1..8) whole blocks,
 * as sum (y ^ b[0]) H^m ^ b[1] H^(m-1) ^ ... ^ b[m-1] H with a single
 * reduction. */
NI_TARGET static inline __m128i ni_ghash_blocks(__m128i y, const ni_hkeys *hk, const uint8_t *p, unsigned m) {
    __m128i lo = _mm_setzero_si128(), hi = lo, mid = lo;
    __m128i x = _mm_xor_si128(y, ni_bswap(_mm_loadu_si128((const __m128i *)p)));
    NI_KMUL(x, m - 1);
    for (unsigned j = 1; j < m; j++) NI_KMUL(ni_bswap(_mm_loadu_si128((const __m128i *)(p + 16 * j))), m - 1 - j);
    mid = _mm_xor_si128(mid, _mm_xor_si128(lo, hi));
    lo = _mm_xor_si128(lo, _mm_slli_si128(mid, 8));
    hi = _mm_xor_si128(hi, _mm_srli_si128(mid, 8));
    return ni_reduce(lo, hi);
}

/* The 8-block case, fully unrolled. */
NI_TARGET static inline __m128i ni_ghash8(__m128i y, const ni_hkeys *hk, const uint8_t *p) {
    __m128i lo = _mm_setzero_si128(), hi = lo, mid = lo;
    NI_KMUL(_mm_xor_si128(y, ni_bswap(_mm_loadu_si128((const __m128i *)p))), 7);
#pragma GCC unroll 8
    for (int j = 1; j < 8; j++) NI_KMUL(ni_bswap(_mm_loadu_si128((const __m128i *)(p + 16 * j))), 7 - j);
    mid = _mm_xor_si128(mid, _mm_xor_si128(lo, hi));
    lo = _mm_xor_si128(lo, _mm_slli_si128(mid, 8));
    hi = _mm_xor_si128(hi, _mm_srli_si128(mid, 8));
    return ni_reduce(lo, hi);
}

/* 8 CTR blocks, out = in ^ E(K, base with big-endian counter c..c+7), with
 * the aggregated GHASH of the 8 blocks at src (when src is not NULL) spread
 * over the first 8 AES rounds so the AESENC and PCLMULQDQ units overlap.
 * Returns the new GHASH state. All src blocks are loaded before out is
 * written, so src may equal out (in-place decryption). */
NI_TARGET static inline __m128i ni_ctr8_ghash8(const __m128i *rk, unsigned nr, __m128i base, uint32_t c,
                                               const uint8_t *in, uint8_t *out, __m128i y, const ni_hkeys *hk,
                                               const uint8_t *src) {
    __m128i b[8];
#pragma GCC unroll 8
    for (int j = 0; j < 8; j++)
        b[j] = _mm_xor_si128(_mm_insert_epi32(base, (int)__builtin_bswap32(c + (uint32_t)j), 3), rk[0]);
    if (src) {
        __m128i lo = _mm_setzero_si128(), hi = lo, mid = lo;
#pragma GCC unroll 8
        for (int r = 1; r <= 8; r++) {
            __m128i k = rk[r];
#pragma GCC unroll 8
            for (int j = 0; j < 8; j++) b[j] = _mm_aesenc_si128(b[j], k);
            /* blocks 1..7 first, block 0 (which needs the previous state y)
             * last: the serial chain through y stays short */
            int j = r & 7;
            __m128i x = ni_bswap(_mm_loadu_si128((const __m128i *)(src + 16 * j)));
            if (j == 0) x = _mm_xor_si128(x, y);
            NI_KMUL(x, 7 - j);
        }
        mid = _mm_xor_si128(mid, _mm_xor_si128(lo, hi));
        lo = _mm_xor_si128(lo, _mm_slli_si128(mid, 8));
        hi = _mm_xor_si128(hi, _mm_srli_si128(mid, 8));
        y = ni_reduce(lo, hi);
    } else {
#pragma GCC unroll 8
        for (int r = 1; r <= 8; r++) {
            __m128i k = rk[r];
#pragma GCC unroll 8
            for (int j = 0; j < 8; j++) b[j] = _mm_aesenc_si128(b[j], k);
        }
    }
    for (unsigned r = 9; r < nr; r++) {
        __m128i k = rk[r];
#pragma GCC unroll 8
        for (int j = 0; j < 8; j++) b[j] = _mm_aesenc_si128(b[j], k);
    }
#pragma GCC unroll 8
    for (int j = 0; j < 8; j++) {
        b[j] = _mm_aesenclast_si128(b[j], rk[nr]);
        _mm_storeu_si128((__m128i *)(out + 16 * j),
                         _mm_xor_si128(b[j], _mm_loadu_si128((const __m128i *)(in + 16 * j))));
    }
    return y;
}
#undef NI_KMUL

/* GHASH over data (zero-padded to whole blocks); y is the reflected state. */
NI_TARGET static __m128i ni_ghash(__m128i y, const ni_hkeys *hk, const uint8_t *p, size_t n) {
    for (; n >= 128; p += 128, n -= 128) y = ni_ghash8(y, hk, p);
    if (n >= 16) {
        unsigned m = (unsigned)(n / 16);
        y = ni_ghash_blocks(y, hk, p, m);
        p += 16 * m;
        n -= 16 * m;
    }
    if (n) {
        uint8_t blk[16] = { 0 };
        memcpy(blk, p, n);
        y = ni_ghash_blocks(y, hk, blk, 1);
    }
    return y;
}

/* CTR keystream XOR, 32-bit big-endian counter in the last 4 bytes of ctr. */
NI_TARGET static void ni_ctr(const __m128i *rk, unsigned nr, uint8_t ctr[16], const uint8_t *in, size_t n,
                             uint8_t *out) {
    uint32_t c = zell__ld32be(ctr + 12);
    uint8_t cb[64];
    while (n >= 64) {
        for (int j = 0; j < 4; j++) {
            memcpy(cb + 16 * j, ctr, 12);
            zell__st32be(cb + 16 * j + 12, c + (uint32_t)j);
        }
        c += 4;
        __m128i b0 = _mm_xor_si128(_mm_loadu_si128((const __m128i *)cb), rk[0]);
        __m128i b1 = _mm_xor_si128(_mm_loadu_si128((const __m128i *)(cb + 16)), rk[0]);
        __m128i b2 = _mm_xor_si128(_mm_loadu_si128((const __m128i *)(cb + 32)), rk[0]);
        __m128i b3 = _mm_xor_si128(_mm_loadu_si128((const __m128i *)(cb + 48)), rk[0]);
        for (unsigned r = 1; r < nr; r++) {
            b0 = _mm_aesenc_si128(b0, rk[r]);
            b1 = _mm_aesenc_si128(b1, rk[r]);
            b2 = _mm_aesenc_si128(b2, rk[r]);
            b3 = _mm_aesenc_si128(b3, rk[r]);
        }
        b0 = _mm_aesenclast_si128(b0, rk[nr]);
        b1 = _mm_aesenclast_si128(b1, rk[nr]);
        b2 = _mm_aesenclast_si128(b2, rk[nr]);
        b3 = _mm_aesenclast_si128(b3, rk[nr]);
        _mm_storeu_si128((__m128i *)out, _mm_xor_si128(b0, _mm_loadu_si128((const __m128i *)in)));
        _mm_storeu_si128((__m128i *)(out + 16), _mm_xor_si128(b1, _mm_loadu_si128((const __m128i *)(in + 16))));
        _mm_storeu_si128((__m128i *)(out + 32), _mm_xor_si128(b2, _mm_loadu_si128((const __m128i *)(in + 32))));
        _mm_storeu_si128((__m128i *)(out + 48), _mm_xor_si128(b3, _mm_loadu_si128((const __m128i *)(in + 48))));
        in += 64;
        out += 64;
        n -= 64;
    }
    while (n) {
        memcpy(cb, ctr, 12);
        zell__st32be(cb + 12, c++);
        uint8_t ks[16];
        _mm_storeu_si128((__m128i *)ks, ni_encrypt(rk, nr, _mm_loadu_si128((const __m128i *)cb)));
        size_t k = n < 16 ? n : 16;
        for (size_t i = 0; i < k; i++) out[i] = in[i] ^ ks[i];
        in += k;
        out += k;
        n -= k;
    }
    zell__st32be(ctr + 12, c);
}

#if !(defined(_WIN32) && !defined(__clang__))
/* VAES + VPCLMULQDQ: the same 8-block batches with two blocks per 256-bit
 * register, so AESENC and PCLMULQDQ (which share execution ports on some
 * CPUs) take half the instructions. Chosen when zell__cpu_has_vaes(). Not
 * built by GCC for Windows: it does not align the stack for 32-byte AVX
 * spills there (GCC bug 54412). */
#define ZELL_HAVE_VAES 1
#include <immintrin.h>

#define VAES_TARGET __attribute__((target("aes,pclmul,sse4.1,ssse3,avx,avx2,vaes,vpclmulqdq")))

VAES_TARGET static inline __m256i vaes_pair(__m128i lo, __m128i hi) {
    return _mm256_inserti128_si256(_mm256_castsi128_si256(lo), hi, 1);
}

/* CTR over the whole 128-byte chunks [0, bulk) with counters c, c+1, ...,
 * and their aggregated GHASH folded into y exactly as the 128-bit loop in
 * ni_gcm does (ciphertext: out when enc, in otherwise). Returns y. */
VAES_TARGET static __m128i vaes_gcm_bulk(const __m128i *rk, unsigned nr, __m128i base, uint32_t c,
                                         const uint8_t *in, uint8_t *out, size_t bulk, __m128i y,
                                         const ni_hkeys *hk, bool enc) {
    __m256i kr[15], hp[4], hm[4];
    for (unsigned r = 0; r <= nr; r++) kr[r] = _mm256_broadcastsi128_si256(rk[r]);
    /* pair p holds blocks 2p (low lane, key H^(8-2p)) and 2p+1 (H^(7-2p)) */
    for (int p = 0; p < 4; p++) {
        hp[p] = vaes_pair(hk->hp[7 - 2 * p], hk->hp[6 - 2 * p]);
        hm[p] = vaes_pair(hk->hk[7 - 2 * p], hk->hk[6 - 2 * p]);
    }
    const __m256i bswap =
        _mm256_broadcastsi128_si256(_mm_set_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15));
    /* encryption hashes the previous chunk's ciphertext, kept in registers */
    __m256i prev[4] = { _mm256_setzero_si256(), _mm256_setzero_si256(), _mm256_setzero_si256(),
                        _mm256_setzero_si256() };
    for (size_t i = 0; i < bulk; i += 128, c += 8) {
        __m256i b[4];
#pragma GCC unroll 4
        for (int p = 0; p < 4; p++)
            b[p] = _mm256_xor_si256(
                vaes_pair(_mm_insert_epi32(base, (int)__builtin_bswap32(c + 2 * (uint32_t)p), 3),
                          _mm_insert_epi32(base, (int)__builtin_bswap32(c + 2 * (uint32_t)p + 1), 3)),
                kr[0]);
        unsigned r = 1;
        if (!enc || i) {
            __m256i lo = _mm256_setzero_si256(), hi = lo, mid = lo;
#pragma GCC unroll 4
            for (; r <= 4; r++) {
#pragma GCC unroll 4
                for (int p = 0; p < 4; p++) b[p] = _mm256_aesenc_epi128(b[p], kr[r]);
                int p = (int)(r & 3);   /* pairs 1, 2, 3, then 0, which needs y */
                __m256i x = _mm256_shuffle_epi8(
                    enc ? prev[p] : _mm256_loadu_si256((const __m256i *)(in + i + 32 * p)), bswap);
                if (p == 0) x = _mm256_xor_si256(x, vaes_pair(y, _mm_setzero_si128()));
                lo = _mm256_xor_si256(lo, _mm256_clmulepi64_epi128(x, hp[p], 0x00));
                hi = _mm256_xor_si256(hi, _mm256_clmulepi64_epi128(x, hp[p], 0x11));
                mid = _mm256_xor_si256(
                    mid, _mm256_clmulepi64_epi128(_mm256_xor_si256(x, _mm256_shuffle_epi32(x, 0x4E)), hm[p], 0x00));
            }
            __m128i l = _mm_xor_si128(_mm256_castsi256_si128(lo), _mm256_extracti128_si256(lo, 1));
            __m128i h = _mm_xor_si128(_mm256_castsi256_si128(hi), _mm256_extracti128_si256(hi, 1));
            __m128i m = _mm_xor_si128(_mm256_castsi256_si128(mid), _mm256_extracti128_si256(mid, 1));
            m = _mm_xor_si128(m, _mm_xor_si128(l, h));
            l = _mm_xor_si128(l, _mm_slli_si128(m, 8));
            h = _mm_xor_si128(h, _mm_srli_si128(m, 8));
            y = ni_reduce(l, h);
        }
        for (; r < nr; r++) {
#pragma GCC unroll 4
            for (int p = 0; p < 4; p++) b[p] = _mm256_aesenc_epi128(b[p], kr[r]);
        }
#pragma GCC unroll 4
        for (int p = 0; p < 4; p++) {
            b[p] = _mm256_xor_si256(_mm256_aesenclast_epi128(b[p], kr[nr]),
                                    _mm256_loadu_si256((const __m256i *)(in + i + 32 * p)));
            _mm256_storeu_si256((__m256i *)(out + i + 32 * p), b[p]);
            prev[p] = b[p];
        }
    }
    if (enc && bulk) y = ni_ghash8(y, hk, out + bulk - 128);
    return y;
}
#endif

/* Full GCM in the AES-NI path; returns the tag in tag[16]. Whole 128-byte
 * chunks go through 8 interleaved AES pipelines and an 8-block aggregated
 * GHASH (encryption hashes the previous chunk's ciphertext while the next
 * chunk is encrypted; decryption hashes and decrypts the same chunk). */
NI_TARGET static void ni_gcm(const zell_gcm_ctx *c, const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                             const uint8_t *in, size_t n, uint8_t *out, bool enc, uint8_t tag[16]) {
    __m128i rk[15];
    ni_hkeys hk;
    unsigned nr = c->k.nr;
    ni_load_keys(&c->k, rk);
    ni_load_hkeys(c, &hk);
    __m128i y = ni_ghash(_mm_setzero_si128(), &hk, aad, alen);
    uint8_t ctr[16];
    memcpy(ctr, nonce, 12);
    zell__st32be(ctr + 12, 2);
    __m128i base = _mm_loadu_si128((const __m128i *)ctr);
    uint32_t cnt = 2;
    size_t bulk = n & ~(size_t)127;
#ifdef ZELL_HAVE_VAES
    if (bulk && zell__cpu_has_vaes()) {
        y = vaes_gcm_bulk(rk, nr, base, cnt, in, out, bulk, y, &hk, enc);
        cnt += (uint32_t)(bulk / 16);
    } else
#endif
    if (enc) {
        for (size_t i = 0; i < bulk; i += 128, cnt += 8)
            y = ni_ctr8_ghash8(rk, nr, base, cnt, in + i, out + i, y, &hk, i ? out + i - 128 : NULL);
        if (bulk) y = ni_ghash8(y, &hk, out + bulk - 128);
    } else {
        for (size_t i = 0; i < bulk; i += 128, cnt += 8)
            y = ni_ctr8_ghash8(rk, nr, base, cnt, in + i, out + i, y, &hk, in + i);
    }
    zell__st32be(ctr + 12, cnt);
    if (enc) {
        ni_ctr(rk, nr, ctr, in + bulk, n - bulk, out + bulk);
        y = ni_ghash(y, &hk, out + bulk, n - bulk);
    } else {
        y = ni_ghash(y, &hk, in + bulk, n - bulk);
        ni_ctr(rk, nr, ctr, in + bulk, n - bulk, out + bulk);
    }
    uint8_t lens[16];
    zell__st64be(lens, (uint64_t)alen * 8);
    zell__st64be(lens + 8, (uint64_t)n * 8);
    y = ni_ghash(y, &hk, lens, 16);
    memcpy(ctr, nonce, 12);
    zell__st32be(ctr + 12, 1);
    __m128i ej = ni_encrypt(rk, nr, _mm_loadu_si128((const __m128i *)ctr));
    _mm_storeu_si128((__m128i *)tag, _mm_xor_si128(ni_bswap(y), ej));
}
#endif

/* ---------------------------------------------------------------------- */
/* Portable block/CTR/GHASH                                                */

static void bs_encrypt4(const zell_aes_key *k, const uint8_t in[64], uint8_t out[64]) {
    uint64_t q[8];
    bs_load(q, in);
    bs_encrypt(k->sk, k->nr, q);
    bs_store(out, q);
}

void zell__aes_encrypt_block(const zell_aes_key *k, const uint8_t in[16], uint8_t out[16]) {
#ifdef ZELL_HAVE_AESNI
    if (k->ni) {
        ni_encrypt_block(k, in, out);
        return;
    }
#endif
    uint8_t buf[64] = { 0 };
    memcpy(buf, in, 16);
    bs_encrypt4(k, buf, buf);
    memcpy(out, buf, 16);
    zell__crypto_wipe(buf, sizeof buf);
}

static void pt_ctr(const zell_aes_key *k, uint8_t ctr[16], const uint8_t *in, size_t n, uint8_t *out) {
    uint32_t c = zell__ld32be(ctr + 12);
    uint8_t cb[64], ks[64];
    while (n) {
        for (int j = 0; j < 4; j++) {
            memcpy(cb + 16 * j, ctr, 12);
            zell__st32be(cb + 16 * j + 12, c + (uint32_t)j);
        }
        bs_encrypt4(k, cb, ks);
        size_t m = n < 64 ? n : 64;
        for (size_t i = 0; i < m; i++) out[i] = in[i] ^ ks[i];
        c += (uint32_t)((m + 15) / 16);
        in += m;
        out += m;
        n -= m;
    }
    zell__st32be(ctr + 12, c);
    zell__crypto_wipe(ks, sizeof ks);
}

/* Low 64 bits of the carry-less product x*y (integer multiplies on
 * operands with "holes", so no carries leak between the kept bits). */
static inline uint64_t bmul64(uint64_t x, uint64_t y) {
    uint64_t x0 = x & 0x1111111111111111ULL, x1 = x & 0x2222222222222222ULL;
    uint64_t x2 = x & 0x4444444444444444ULL, x3 = x & 0x8888888888888888ULL;
    uint64_t y0 = y & 0x1111111111111111ULL, y1 = y & 0x2222222222222222ULL;
    uint64_t y2 = y & 0x4444444444444444ULL, y3 = y & 0x8888888888888888ULL;
    uint64_t z0 = (x0 * y0) ^ (x1 * y3) ^ (x2 * y2) ^ (x3 * y1);
    uint64_t z1 = (x0 * y1) ^ (x1 * y0) ^ (x2 * y3) ^ (x3 * y2);
    uint64_t z2 = (x0 * y2) ^ (x1 * y1) ^ (x2 * y0) ^ (x3 * y3);
    uint64_t z3 = (x0 * y3) ^ (x1 * y2) ^ (x2 * y1) ^ (x3 * y0);
    z0 &= 0x1111111111111111ULL;
    z1 &= 0x2222222222222222ULL;
    z2 &= 0x4444444444444444ULL;
    z3 &= 0x8888888888888888ULL;
    return z0 | z1 | z2 | z3;
}

static inline uint64_t rev64(uint64_t x) {
#define RMS(m, s) x = ((x & (uint64_t)(m)) << (s)) | ((x >> (s)) & (uint64_t)(m))
    RMS(0x5555555555555555ULL, 1);
    RMS(0x3333333333333333ULL, 2);
    RMS(0x0F0F0F0F0F0F0F0FULL, 4);
    RMS(0x00FF00FF00FF00FFULL, 8);
    RMS(0x0000FFFF0000FFFFULL, 16);
#undef RMS
    return (x << 32) | (x >> 32);
}

/* One GHASH step y = (y ^ blk) * H; y1:y0 are the big-endian halves of the
 * (reflected) state. */
static inline void ghash_step(uint64_t *py1, uint64_t *py0, uint64_t h1, uint64_t h0, const uint8_t *src) {
    uint64_t y1 = *py1 ^ zell__ld64be(src), y0 = *py0 ^ zell__ld64be(src + 8);
    uint64_t h0r = rev64(h0), h1r = rev64(h1), h2 = h0 ^ h1, h2r = h0r ^ h1r;
    uint64_t y0r = rev64(y0), y1r = rev64(y1), y2 = y0 ^ y1, y2r = y0r ^ y1r;
    uint64_t z0 = bmul64(y0, h0), z1 = bmul64(y1, h1), z2 = bmul64(y2, h2);
    uint64_t z0h = bmul64(y0r, h0r), z1h = bmul64(y1r, h1r), z2h = bmul64(y2r, h2r);
    z2 ^= z0 ^ z1;
    z2h ^= z0h ^ z1h;
    z0h = rev64(z0h) >> 1;
    z1h = rev64(z1h) >> 1;
    z2h = rev64(z2h) >> 1;
    uint64_t v0 = z0, v1 = z0h ^ z2, v2 = z1 ^ z2h, v3 = z1h;
    v3 = (v3 << 1) | (v2 >> 63);
    v2 = (v2 << 1) | (v1 >> 63);
    v1 = (v1 << 1) | (v0 >> 63);
    v0 = (v0 << 1);
    v2 ^= v0 ^ (v0 >> 1) ^ (v0 >> 2) ^ (v0 >> 7);
    v1 ^= (v0 << 63) ^ (v0 << 62) ^ (v0 << 57);
    v3 ^= v1 ^ (v1 >> 1) ^ (v1 >> 2) ^ (v1 >> 7);
    v2 ^= (v1 << 63) ^ (v1 << 62) ^ (v1 << 57);
    *py0 = v2;
    *py1 = v3;
}

/* GHASH over data zero-padded to whole blocks. */
static void pt_ghash(uint64_t *py1, uint64_t *py0, uint64_t h1, uint64_t h0, const uint8_t *p, size_t n) {
    for (; n >= 16; p += 16, n -= 16) ghash_step(py1, py0, h1, h0, p);
    if (n) {
        uint8_t blk[16] = { 0 };
        memcpy(blk, p, n);
        ghash_step(py1, py0, h1, h0, blk);
    }
}

static void pt_gcm(const zell_gcm_ctx *c, const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                   const uint8_t *in, size_t n, uint8_t *out, bool enc, uint8_t tag[16]) {
    uint64_t y1 = 0, y0 = 0;
    pt_ghash(&y1, &y0, c->h1, c->h0, aad, alen);
    uint8_t ctr[16];
    memcpy(ctr, nonce, 12);
    zell__st32be(ctr + 12, 2);
    if (enc) {
        pt_ctr(&c->k, ctr, in, n, out);
        pt_ghash(&y1, &y0, c->h1, c->h0, out, n);
    } else {
        pt_ghash(&y1, &y0, c->h1, c->h0, in, n);
        pt_ctr(&c->k, ctr, in, n, out);
    }
    uint8_t lens[16];
    zell__st64be(lens, (uint64_t)alen * 8);
    zell__st64be(lens + 8, (uint64_t)n * 8);
    pt_ghash(&y1, &y0, c->h1, c->h0, lens, 16);
    memcpy(ctr, nonce, 12);
    zell__st32be(ctr + 12, 1);
    uint8_t ej[16];
    zell__aes_encrypt_block(&c->k, ctr, ej);
    zell__st64be(tag, y1);
    zell__st64be(tag + 8, y0);
    for (int i = 0; i < 16; i++) tag[i] ^= ej[i];
}

/* ---------------------------------------------------------------------- */
/* GCM API                                                                 */

bool zell__gcm_init(zell_gcm_ctx *c, const uint8_t *key, size_t klen) {
    memset(c, 0, sizeof *c);
    if (!zell__aes_init(&c->k, key, klen)) return false;
    uint8_t zero[16] = { 0 };
    zell__aes_encrypt_block(&c->k, zero, c->h);
    c->h1 = zell__ld64be(c->h);
    c->h0 = zell__ld64be(c->h + 8);
#ifdef ZELL_HAVE_AESNI
    if (c->k.ni) ni_gcm_powers(c);
#endif
    return true;
}

static void gcm_run(const zell_gcm_ctx *c, const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                    const uint8_t *in, size_t n, uint8_t *out, bool enc, uint8_t tag[16]) {
#ifdef ZELL_HAVE_AESNI
    if (c->k.ni) {
        ni_gcm(c, nonce, aad, alen, in, n, out, enc, tag);
        return;
    }
#endif
    pt_gcm(c, nonce, aad, alen, in, n, out, enc, tag);
}

void zell__gcm_seal(const zell_gcm_ctx *c, const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                     const uint8_t *in, size_t n, uint8_t *out) {
    gcm_run(c, nonce, aad, alen, in, n, out, true, out + n);
}

bool zell__gcm_open(const zell_gcm_ctx *c, const uint8_t nonce[12], const uint8_t *aad, size_t alen,
                     const uint8_t *in, size_t n, uint8_t *out) {
    uint8_t tag[16];
    gcm_run(c, nonce, aad, alen, in, n, out, false, tag);
    if (!zell__crypto_ct_eq(tag, in + n, 16)) {
        zell__crypto_wipe(out, n);
        return false;
    }
    return true;
}
