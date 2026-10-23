/* ed25519.c - Ed25519 signatures (RFC 8032 section 5.1), ref10-style:
 * twisted Edwards curve -x^2 + y^2 = 1 + d x^2 y^2 over fe25519.h in
 * extended coordinates (X:Y:Z:T), unified addition (add-2008-hwcd-3),
 * doubling dbl-2008-hwcd, mixed addition with precomputed affine points in
 * Niels form (y+x, y-x, 2dxy).
 *
 * Fixed-base multiplication (key derivation and R = [r]B) uses the
 * precomputed table ed25519_table.h: ed_base[j][i] = (i+1) * 256^j * B, the
 * scalar is recoded into 64 signed radix-16 digits and every table lookup
 * scans all eight entries with conditional moves, so no secret selects an
 * address or a branch. Verification (all inputs public) is the
 * variable-time Straus/Shamir double multiplication [S]B + [k](-A) with
 * width-5 sliding windows over the odd multiples of A and the precomputed
 * odd multiples of B, checked as encode(...) == R with S < L and canonical
 * point encodings required.
 *
 * Scalars modulo L = 2^252 + c are reduced in constant time by three rounds
 * of x = lo + hi 2^252 == lo - hi c (mod L), then a bounded number of
 * conditional subtractions.
 *
 * The constants and tables were generated from this file's own arithmetic
 * (d = -121665/121666, sqrt(-1) = 2^((p-1)/4), B = (x even, y = 4/5)) by
 * the generic double-and-add path before it was replaced; the RFC 8032 and
 * Wycheproof vectors in tests/test_crypto.c cover every table position. */
#include "fe25519.h"

#include <string.h>

typedef struct {
    fe X, Y, Z, T;
} ge;

#include "ed25519_table.h"

/* L = 2^252 + 27742317777372353535851937790883648493, little-endian 64-bit limbs. */
static const uint64_t L64[4] = { 0x5812631a5cf5d3edULL, 0x14def9dea2f79cd6ULL, 0, 0x1000000000000000ULL };
static const uint64_t L64x2[4] = { 0xb024c634b9eba7daULL, 0x29bdf3bd45ef39acULL, 0, 0x2000000000000000ULL };

static void ge_identity(ge *p) {
    fe_0(p->X);
    fe_1(p->Y);
    fe_1(p->Z);
    fe_0(p->T);
}

static void ge_add(ge *r, const ge *p, const ge *q) {
    fe a, b, c, d, e, f, g, h, t;
    fe_sub(a, p->Y, p->X);
    fe_sub(t, q->Y, q->X);
    fe_mul(a, a, t);
    fe_add(b, p->Y, p->X);
    fe_add(t, q->Y, q->X);
    fe_mul(b, b, t);
    fe_mul(c, p->T, q->T);
    fe_mul(c, c, ed_d2);
    fe_mul(d, p->Z, q->Z);
    fe_add(d, d, d);
    fe_sub(e, b, a);
    fe_sub(f, d, c);
    fe_add(g, d, c);
    fe_add(h, b, a);
    fe_mul(r->X, e, f);
    fe_mul(r->Y, g, h);
    fe_mul(r->T, e, h);
    fe_mul(r->Z, f, g);
}

/* r = p + q (sub: p - q) for an affine q in Niels form. */
static void ge_madd(ge *r, const ge *p, const fe q[3], int sub) {
    fe a, b, c, d, e, f, g, h;
    fe_sub(a, p->Y, p->X);
    fe_mul(a, a, q[sub ? 0 : 1]);
    fe_add(b, p->Y, p->X);
    fe_mul(b, b, q[sub ? 1 : 0]);
    fe_mul(c, p->T, q[2]);
    fe_add(d, p->Z, p->Z);
    fe_sub(e, b, a);
    if (sub) {
        fe_add(f, d, c);
        fe_sub(g, d, c);
    } else {
        fe_sub(f, d, c);
        fe_add(g, d, c);
    }
    fe_add(h, b, a);
    fe_mul(r->X, e, f);
    fe_mul(r->Y, g, h);
    fe_mul(r->T, e, h);
    fe_mul(r->Z, f, g);
}

static void ge_dbl(ge *r, const ge *p) {
    fe a, b, c, d, e, f, g, h, t;
    fe_sq(a, p->X);
    fe_sq(b, p->Y);
    fe_sq(c, p->Z);
    fe_add(c, c, c);
    fe_neg(d, a);
    fe_add(t, p->X, p->Y);
    fe_sq(e, t);
    fe_sub(e, e, a);
    fe_sub(e, e, b);
    fe_add(g, d, b);
    fe_sub(f, g, c);
    fe_sub(h, d, b);
    fe_mul(r->X, e, f);
    fe_mul(r->Y, g, h);
    fe_mul(r->T, e, h);
    fe_mul(r->Z, f, g);
}

static void ge_neg(ge *r, const ge *p) {
    fe_neg(r->X, p->X);
    fe_copy(r->Y, p->Y);
    fe_copy(r->Z, p->Z);
    fe_neg(r->T, p->T);
}

/* 1 if a == b, 0 otherwise, without branches. */
static inline uint64_t ct_eq64(uint64_t a, uint64_t b) {
    uint64_t x = a ^ b;
    return ((x - 1) & ~x) >> 63;
}

/* t = digit * 256^pos * B for a signed digit in [-8, 8], constant time. */
static void table_select(fe t[3], int pos, int8_t digit) {
    uint64_t neg = (uint64_t)((uint8_t)digit >> 7);
    uint64_t abs = (uint64_t)(uint8_t)(digit - (((0 - (int8_t)neg) & digit) << 1));
    fe_1(t[0]);
    fe_1(t[1]);
    fe_0(t[2]);
    for (int u = 0; u < 8; u++) {
        uint64_t m = ct_eq64(abs, (uint64_t)u + 1);
        fe_cmov(t[0], ed_base[pos][u][0], m);
        fe_cmov(t[1], ed_base[pos][u][1], m);
        fe_cmov(t[2], ed_base[pos][u][2], m);
    }
    fe minus[3];
    fe_copy(minus[0], t[1]);
    fe_copy(minus[1], t[0]);
    fe_neg(minus[2], t[2]);
    fe_cmov(t[0], minus[0], neg);
    fe_cmov(t[1], minus[1], neg);
    fe_cmov(t[2], minus[2], neg);
}

/* r = [a]B for a 32-byte little-endian scalar a < 2^255, constant time. */
static void ge_scalarmult_base(ge *r, const uint8_t a[32]) {
    int8_t e[64];
    for (int i = 0; i < 32; i++) {
        e[2 * i] = (int8_t)(a[i] & 15);
        e[2 * i + 1] = (int8_t)((a[i] >> 4) & 15);
    }
    int8_t carry = 0;
    for (int i = 0; i < 63; i++) {
        e[i] = (int8_t)(e[i] + carry);
        carry = (int8_t)((e[i] + 8) >> 4);
        e[i] = (int8_t)(e[i] - (int8_t)(carry << 4));
    }
    e[63] = (int8_t)(e[63] + carry);
    fe t[3];
    ge h;
    ge_identity(&h);
    for (int i = 1; i < 64; i += 2) {
        table_select(t, i / 2, e[i]);
        ge_madd(&h, &h, t, 0);
    }
    for (int i = 0; i < 4; i++) ge_dbl(&h, &h);
    for (int i = 0; i < 64; i += 2) {
        table_select(t, i / 2, e[i]);
        ge_madd(&h, &h, t, 0);
    }
    *r = h;
    zell__crypto_wipe(e, sizeof e);
    zell__crypto_wipe(t, sizeof t);
}

/* Width-5 sliding-window recoding of a public 256-bit scalar: r[i] is odd in
 * [-15, 15] or zero and a = sum r[i] 2^i. */
static void slide(int8_t r[256], const uint8_t a[32]) {
    for (int i = 0; i < 256; i++) r[i] = (int8_t)(1 & (a[i >> 3] >> (i & 7)));
    for (int i = 0; i < 256; i++) {
        if (!r[i]) continue;
        for (int b = 1; b <= 6 && i + b < 256; b++) {
            if (!r[i + b]) continue;
            if (r[i] + (r[i + b] << b) <= 15) {
                r[i] = (int8_t)(r[i] + (r[i + b] << b));
                r[i + b] = 0;
            } else if (r[i] - (r[i + b] << b) >= -15) {
                r[i] = (int8_t)(r[i] - (r[i + b] << b));
                for (int k = i + b; k < 256; k++) {
                    if (!r[k]) {
                        r[k] = 1;
                        break;
                    }
                    r[k] = 0;
                }
            } else {
                break;
            }
        }
    }
}

/* r = [a]A + [b]B, variable time (public inputs only). */
static void ge_double_scalarmult_vartime(ge *r, const uint8_t a[32], const ge *A, const uint8_t b[32]) {
    int8_t aslide[256], bslide[256];
    slide(aslide, a);
    slide(bslide, b);
    ge Ai[8], A2;   /* Ai[i] = (2i+1) A */
    Ai[0] = *A;
    ge_dbl(&A2, A);
    for (int i = 1; i < 8; i++) ge_add(&Ai[i], &Ai[i - 1], &A2);
    ge h, t;
    ge_identity(&h);
    int i = 255;
    while (i >= 0 && !aslide[i] && !bslide[i]) i--;
    for (; i >= 0; i--) {
        ge_dbl(&h, &h);
        if (aslide[i] > 0) {
            ge_add(&h, &h, &Ai[aslide[i] / 2]);
        } else if (aslide[i] < 0) {
            ge_neg(&t, &Ai[(-aslide[i]) / 2]);
            ge_add(&h, &h, &t);
        }
        if (bslide[i] > 0) ge_madd(&h, &h, ed_bi[bslide[i] / 2], 0);
        else if (bslide[i] < 0) ge_madd(&h, &h, ed_bi[(-bslide[i]) / 2], 1);
    }
    *r = h;
}

static void ge_encode(uint8_t s[32], const ge *p) {
    fe zi, x, y;
    fe_invert(zi, p->Z);
    fe_mul(x, p->X, zi);
    fe_mul(y, p->Y, zi);
    fe_tobytes(s, y);
    s[31] ^= (uint8_t)(fe_isnegative(x) << 7);
}

/* RFC 8032 5.1.3; false for an invalid or non-canonical encoding. */
static bool ge_decode(ge *p, const uint8_t s[32]) {
    uint8_t chk[32];
    int sign = s[31] >> 7;
    fe_frombytes(p->Y, s);
    fe_tobytes(chk, p->Y);
    uint8_t ys[32];
    memcpy(ys, s, 32);
    ys[31] &= 127;
    if (!zell__crypto_ct_eq(chk, ys, 32)) return false;   /* y >= p */
    fe u, v, v3, x, t, one;
    fe_1(one);
    fe_sq(u, p->Y);
    fe_mul(v, u, ed_d);
    fe_sub(u, u, one);         /* u = y^2 - 1 */
    fe_add(v, v, one);         /* v = d y^2 + 1 */
    fe_sq(v3, v);
    fe_mul(v3, v3, v);         /* v^3 */
    fe_sq(x, v3);
    fe_mul(x, x, v);
    fe_mul(x, x, u);           /* u v^7 */
    fe_pow22523(x, x);
    fe_mul(x, x, v3);
    fe_mul(x, x, u);           /* u v^3 (u v^7)^((p-5)/8) */
    fe_sq(t, x);
    fe_mul(t, t, v);           /* v x^2 */
    fe chk1, chk2;
    fe_sub(chk1, t, u);
    fe_add(chk2, t, u);
    if (fe_iszero(chk1)) {
        /* x is right */
    } else if (fe_iszero(chk2)) {
        fe_mul(x, x, ed_sqrtm1);
    } else {
        return false;
    }
    if (fe_iszero(x) && sign) return false;
    if (fe_isnegative(x) != sign) fe_neg(x, x);
    fe_copy(p->X, x);
    fe_1(p->Z);
    fe_mul(p->T, p->X, p->Y);
    return true;
}

/* ---- scalars modulo L ---- */

/* dst[0..n_src+1] = src * c where L = 2^252 + c (c = low two limbs of L). */
static void sc_mul_c(uint64_t *dst, const uint64_t *src, int n_src) {
    for (int i = 0; i < n_src + 2; i++) dst[i] = 0;
    for (int i = 0; i < n_src; i++) {
        uint64_t carry = 0;
        for (int j = 0; j < 2; j++) {
            u128 t = (u128)src[i] * L64[j] + dst[i + j] + carry;
            dst[i + j] = (uint64_t)t;
            carry = (uint64_t)(t >> 64);
        }
        dst[i + 2] = carry;
    }
}

/* lo = x mod 2^252 (4 limbs), hi = x >> 252 (n_x - 3 limbs). */
static void sc_split252(uint64_t lo[4], uint64_t *hi, const uint64_t *x, int n_x) {
    for (int i = 0; i < 4; i++) lo[i] = i < n_x ? x[i] : 0;
    lo[3] &= 0x0fffffffffffffffULL;
    for (int i = 0; i < n_x - 3; i++) {
        uint64_t a = x[3 + i] >> 60;
        uint64_t b = 4 + i < n_x ? x[4 + i] << 4 : 0;
        hi[i] = a | b;
    }
}

/* r -= L if r >= L, constant time (r has 5 limbs). */
static void sc_csub_L(uint64_t r[5]) {
    uint64_t d[5], borrow = 0;
    for (int i = 0; i < 5; i++) {
        uint64_t li = i < 4 ? L64[i] : 0;
        u128 t = (u128)r[i] - li - borrow;
        d[i] = (uint64_t)t;
        borrow = (uint64_t)(t >> 64) & 1;
    }
    uint64_t keep = 0 - borrow;
    for (int i = 0; i < 5; i++) r[i] = (r[i] & keep) | (d[i] & ~keep);
}

/* out = (in as a little-endian number of n <= 72 bytes) mod L, constant time. */
static void sc_reduce(uint8_t out[32], const uint8_t *in, size_t n) {
    uint8_t buf[72] = { 0 };
    uint64_t x[9];
    memcpy(buf, in, n > 72 ? 72 : n);
    for (int i = 0; i < 9; i++) x[i] = zell__ld64le(buf + 8 * i);
    /* x = lo1 + hi1 2^252 == lo1 - hi1 c; t1 = hi1 c < 2^449 */
    uint64_t lo1[4], hi1[6], t1[8];
    sc_split252(lo1, hi1, x, 9);
    sc_mul_c(t1, hi1, 6);
    /* t1 = lo2 + hi2 2^252 == lo2 - hi2 c; t2 = hi2 c < 2^322 */
    uint64_t lo2[4], hi2[5], t2[7];
    sc_split252(lo2, hi2, t1, 8);
    sc_mul_c(t2, hi2, 5);
    /* t2 = lo3 + hi3 2^252 == lo3 - hi3 c; t3 = hi3 c < 2^195 */
    uint64_t lo3[4], hi3[4], t3[6];
    sc_split252(lo3, hi3, t2, 7);
    sc_mul_c(t3, hi3, 4);
    /* x == lo1 - lo2 + lo3 - t3: r = (lo1 + lo3) - (lo2 + t3) + 2L in (0, 4L) */
    uint64_t r[5], b[5], carry = 0, borrow = 0;
    for (int i = 0; i < 4; i++) {
        u128 s = (u128)lo1[i] + lo3[i] + L64x2[i] + carry;
        r[i] = (uint64_t)s;
        carry = (uint64_t)(s >> 64);
    }
    r[4] = carry;
    carry = 0;
    for (int i = 0; i < 4; i++) {
        u128 s = (u128)lo2[i] + t3[i] + carry;
        b[i] = (uint64_t)s;
        carry = (uint64_t)(s >> 64);
    }
    b[4] = carry;
    for (int i = 0; i < 5; i++) {
        u128 s = (u128)r[i] - b[i] - borrow;
        r[i] = (uint64_t)s;
        borrow = (uint64_t)(s >> 64) & 1;
    }
    sc_csub_L(r);
    sc_csub_L(r);
    sc_csub_L(r);
    for (int i = 0; i < 4; i++) zell__st64le(out + 8 * i, r[i]);
    zell__crypto_wipe(buf, sizeof buf);
    zell__crypto_wipe(x, sizeof x);
    zell__crypto_wipe(lo1, sizeof lo1);
    zell__crypto_wipe(hi1, sizeof hi1);
    zell__crypto_wipe(t1, sizeof t1);
    zell__crypto_wipe(lo2, sizeof lo2);
    zell__crypto_wipe(hi2, sizeof hi2);
    zell__crypto_wipe(t2, sizeof t2);
    zell__crypto_wipe(lo3, sizeof lo3);
    zell__crypto_wipe(hi3, sizeof hi3);
    zell__crypto_wipe(t3, sizeof t3);
    zell__crypto_wipe(r, sizeof r);
}

/* s = (a * b + c) mod L. */
static void sc_muladd(uint8_t s[32], const uint8_t a[32], const uint8_t b[32], const uint8_t c[32]) {
    uint64_t x[4], y[4], z[9] = { 0 };
    for (int i = 0; i < 4; i++) {
        x[i] = zell__ld64le(a + 8 * i);
        y[i] = zell__ld64le(b + 8 * i);
    }
    for (int i = 0; i < 4; i++) {
        uint64_t carry = 0;
        for (int j = 0; j < 4; j++) {
            u128 t = (u128)x[i] * y[j] + z[i + j] + carry;
            z[i + j] = (uint64_t)t;
            carry = (uint64_t)(t >> 64);
        }
        z[i + 4] = carry;
    }
    uint64_t carry = 0;
    for (int i = 0; i < 9; i++) {
        u128 t = (u128)z[i] + (i < 4 ? zell__ld64le(c + 8 * i) : 0) + carry;
        z[i] = (uint64_t)t;
        carry = (uint64_t)(t >> 64);
    }
    uint8_t buf[72];
    for (int i = 0; i < 9; i++) zell__st64le(buf + 8 * i, z[i]);
    sc_reduce(s, buf, 72);
    zell__crypto_wipe(buf, sizeof buf);
    zell__crypto_wipe(z, sizeof z);
    zell__crypto_wipe(x, sizeof x);
}

static bool sc_is_canonical(const uint8_t s[32]) {
    for (int i = 3; i >= 0; i--) {
        uint64_t v = zell__ld64le(s + 8 * i);
        if (v < L64[i]) return true;
        if (v > L64[i]) return false;
    }
    return false;   /* equal to L */
}

/* ---- API ---- */

static void expand_seed(const uint8_t seed[32], uint8_t a[32], uint8_t prefix[32]) {
    uint8_t h[64];
    zell__hash(ZELL_HASH_SHA512, seed, 32, h);
    memcpy(a, h, 32);
    a[0] &= 248;
    a[31] &= 127;
    a[31] |= 64;
    memcpy(prefix, h + 32, 32);
    zell__crypto_wipe(h, sizeof h);
}

void zell__ed25519_pubkey(uint8_t pub[32], const uint8_t seed[32]) {
    uint8_t a[32], prefix[32];
    expand_seed(seed, a, prefix);
    ge A;
    ge_scalarmult_base(&A, a);
    ge_encode(pub, &A);
    zell__crypto_wipe(a, sizeof a);
    zell__crypto_wipe(prefix, sizeof prefix);
}

void zell__ed25519_sign(uint8_t sig[64], const uint8_t seed[32], const uint8_t *msg, size_t n) {
    uint8_t a[32], prefix[32], pub[32], h[64], r[32], kk[32];
    expand_seed(seed, a, prefix);
    ge P;
    ge_scalarmult_base(&P, a);
    ge_encode(pub, &P);
    zell_hash_ctx c;
    zell__hash_init(&c, ZELL_HASH_SHA512);
    zell__hash_update(&c, prefix, 32);
    zell__hash_update(&c, msg, n);
    zell__hash_final(&c, h);
    sc_reduce(r, h, 64);
    ge_scalarmult_base(&P, r);
    ge_encode(sig, &P);
    zell__hash_init(&c, ZELL_HASH_SHA512);
    zell__hash_update(&c, sig, 32);
    zell__hash_update(&c, pub, 32);
    zell__hash_update(&c, msg, n);
    zell__hash_final(&c, h);
    sc_reduce(kk, h, 64);
    sc_muladd(sig + 32, kk, a, r);
    zell__crypto_wipe(a, sizeof a);
    zell__crypto_wipe(prefix, sizeof prefix);
    zell__crypto_wipe(r, sizeof r);
    zell__crypto_wipe(h, sizeof h);
    zell__crypto_wipe(&c, sizeof c);
}

bool zell__ed25519_verify(const uint8_t sig[64], const uint8_t pub[32], const uint8_t *msg, size_t n) {
    ge A, R;
    if (!sc_is_canonical(sig + 32)) return false;
    if (!ge_decode(&A, pub) || !ge_decode(&R, sig)) return false;
    uint8_t h[64], kk[32], chk[32];
    zell_hash_ctx c;
    zell__hash_init(&c, ZELL_HASH_SHA512);
    zell__hash_update(&c, sig, 32);
    zell__hash_update(&c, pub, 32);
    zell__hash_update(&c, msg, n);
    zell__hash_final(&c, h);
    sc_reduce(kk, h, 64);
    /* [S]B + [k](-A) */
    ge nA, sum;
    ge_neg(&nA, &A);
    ge_double_scalarmult_vartime(&sum, kk, &nA, sig + 32);
    ge_encode(chk, &sum);
    return zell__crypto_ct_eq(chk, sig, 32) == 1;
}
