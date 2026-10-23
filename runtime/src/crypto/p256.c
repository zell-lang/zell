/* p256.c - NIST P-256 (secp256r1): ECDH (SEC 1 3.3.1) and ECDSA
 * (FIPS 186-4 6, deterministic nonces per RFC 6979 3.2, optionally hedged
 * with extra randomness as in RFC 6979 3.6).
 *
 * Arithmetic: 4 x 64-bit limbs, Montgomery multiplication (CIOS) modulo
 * either the field prime p or the group order n, with constant-time final
 * subtraction; additions/subtractions use masks, never branches. Points are
 * projective (X:Y:Z) and combined with the complete addition formula of
 * Renes-Costello-Batina 2016 (algorithm 4, a = -3), which has no special
 * cases (doubling and the point at infinity included), so the 4-bit
 * fixed-window scalar multiplication with a constant-time table scan does
 * not branch on secret data. Verification uses the same code. */
#include "crypto.h"

#include <string.h>

typedef unsigned __int128 u128;
typedef uint64_t limb4[4];

typedef struct {
    limb4 m;        /* modulus */
    uint64_t m0;    /* -m^-1 mod 2^64 */
    limb4 rr;       /* R^2 mod m, R = 2^256 */
    limb4 one;      /* R mod m (1 in Montgomery form) */
} mont;

static const limb4 P_M = { 0xffffffffffffffffULL, 0x00000000ffffffffULL, 0, 0xffffffff00000001ULL };
static const limb4 N_M = { 0xf3b9cac2fc632551ULL, 0xbce6faada7179e84ULL, 0xffffffffffffffffULL, 0xffffffff00000000ULL };
/* b, G and the Montgomery constants live in p256_table.h (generated from the
 * SEC 2 values with this file's own arithmetic). */

static void l4_from_be(limb4 r, const uint8_t b[32]) {
    for (int i = 0; i < 4; i++) r[i] = zell__ld64be(b + 24 - 8 * i);
}
static void l4_to_be(uint8_t b[32], const limb4 a) {
    for (int i = 0; i < 4; i++) zell__st64be(b + 24 - 8 * i, a[i]);
}
static void l4_copy(limb4 r, const limb4 a) { for (int i = 0; i < 4; i++) r[i] = a[i]; }

/* r = a - b; returns the borrow (0 or 1). */
static uint64_t l4_sub(limb4 r, const limb4 a, const limb4 b) {
    uint64_t borrow = 0;
    for (int i = 0; i < 4; i++) {
        u128 t = (u128)a[i] - b[i] - borrow;
        r[i] = (uint64_t)t;
        borrow = (uint64_t)(t >> 64) & 1;
    }
    return borrow;
}
static uint64_t l4_add(limb4 r, const limb4 a, const limb4 b) {
    uint64_t carry = 0;
    for (int i = 0; i < 4; i++) {
        u128 t = (u128)a[i] + b[i] + carry;
        r[i] = (uint64_t)t;
        carry = (uint64_t)(t >> 64);
    }
    return carry;
}
/* r = (mask ? a : r) with mask all-ones or zero. */
static void l4_cmov(limb4 r, const limb4 a, uint64_t mask) {
    for (int i = 0; i < 4; i++) r[i] ^= (r[i] ^ a[i]) & mask;
}
static uint64_t l4_is_zero(const limb4 a) {
    uint64_t x = a[0] | a[1] | a[2] | a[3];
    return 1 & ~((x | (0 - x)) >> 63);   /* 1 if zero */
}
/* 1 if a < m (constant time). */
static uint64_t l4_lt(const limb4 a, const limb4 m) {
    limb4 t;
    return l4_sub(t, a, m);
}

/* r = a * b * R^-1 mod m for a, b < m. */
static void mont_mul(const mont *c, limb4 r, const limb4 a, const limb4 b) {
    uint64_t t[6] = { 0 };
    for (int i = 0; i < 4; i++) {
        uint64_t carry = 0;
        for (int j = 0; j < 4; j++) {
            u128 x = (u128)a[j] * b[i] + t[j] + carry;
            t[j] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
        u128 x = (u128)t[4] + carry;
        t[4] = (uint64_t)x;
        t[5] = (uint64_t)(x >> 64);
        uint64_t mm = t[0] * c->m0;
        x = (u128)mm * c->m[0] + t[0];
        carry = (uint64_t)(x >> 64);
        for (int j = 1; j < 4; j++) {
            x = (u128)mm * c->m[j] + t[j] + carry;
            t[j - 1] = (uint64_t)x;
            carry = (uint64_t)(x >> 64);
        }
        x = (u128)t[4] + carry;
        t[3] = (uint64_t)x;
        t[4] = t[5] + (uint64_t)(x >> 64);
    }
    limb4 res = { t[0], t[1], t[2], t[3] }, d;
    uint64_t borrow = l4_sub(d, res, c->m);
    /* subtract if t >= m, i.e. carry word set or no borrow */
    l4_cmov(res, d, 0 - (t[4] | (borrow ^ 1)));
    l4_copy(r, res);
}

static void mont_add(const mont *c, limb4 r, const limb4 a, const limb4 b) {
    limb4 s, d;
    uint64_t carry = l4_add(s, a, b);
    uint64_t borrow = l4_sub(d, s, c->m);
    l4_cmov(s, d, 0 - (carry | (borrow ^ 1)));
    l4_copy(r, s);
}

static void mont_sub(const mont *c, limb4 r, const limb4 a, const limb4 b) {
    limb4 d, e;
    uint64_t borrow = l4_sub(d, a, b);
    l4_add(e, d, c->m);
    l4_cmov(d, e, 0 - borrow);
    l4_copy(r, d);
}

static void mont_to(const mont *c, limb4 r, const limb4 a) { mont_mul(c, r, a, c->rr); }
static void mont_from(const mont *c, limb4 r, const limb4 a) {
    limb4 one = { 1, 0, 0, 0 };
    mont_mul(c, r, a, one);
}

/* r = a^(m-2) (inverse for prime m); the exponent is public. */
static void mont_inv(const mont *c, limb4 r, const limb4 a) {
    limb4 e, two = { 2, 0, 0, 0 }, acc;
    l4_sub(e, c->m, two);
    l4_copy(acc, c->one);
    for (int i = 255; i >= 0; i--) {
        mont_mul(c, acc, acc, acc);
        if ((e[i / 64] >> (i % 64)) & 1) mont_mul(c, acc, acc, a);
    }
    l4_copy(r, acc);
}

/* ---- curve ---- */

typedef struct {
    limb4 X, Y, Z;
} pt;

typedef struct {
    mont fp, fn;
    limb4 b;      /* Montgomery form */
    pt g;
} curve;

#include "p256_table.h"

/* Complete addition, RCB 2016 algorithm 4 (a = -3). */
static void pt_add(const curve *k, pt *r, const pt *p, const pt *q) {
    const mont *f = &k->fp;
    limb4 t0, t1, t2, t3, t4, X3, Y3, Z3;
    mont_mul(f, t0, p->X, q->X);
    mont_mul(f, t1, p->Y, q->Y);
    mont_mul(f, t2, p->Z, q->Z);
    mont_add(f, t3, p->X, p->Y);
    mont_add(f, t4, q->X, q->Y);
    mont_mul(f, t3, t3, t4);
    mont_add(f, t4, t0, t1);
    mont_sub(f, t3, t3, t4);
    mont_add(f, t4, p->Y, p->Z);
    mont_add(f, X3, q->Y, q->Z);
    mont_mul(f, t4, t4, X3);
    mont_add(f, X3, t1, t2);
    mont_sub(f, t4, t4, X3);
    mont_add(f, X3, p->X, p->Z);
    mont_add(f, Y3, q->X, q->Z);
    mont_mul(f, X3, X3, Y3);
    mont_add(f, Y3, t0, t2);
    mont_sub(f, Y3, X3, Y3);
    mont_mul(f, Z3, k->b, t2);
    mont_sub(f, X3, Y3, Z3);
    mont_add(f, Z3, X3, X3);
    mont_add(f, X3, X3, Z3);
    mont_sub(f, Z3, t1, X3);
    mont_add(f, X3, t1, X3);
    mont_mul(f, Y3, k->b, Y3);
    mont_add(f, t1, t2, t2);
    mont_add(f, t2, t1, t2);
    mont_sub(f, Y3, Y3, t2);
    mont_sub(f, Y3, Y3, t0);
    mont_add(f, t1, Y3, Y3);
    mont_add(f, Y3, t1, Y3);
    mont_add(f, t1, t0, t0);
    mont_add(f, t0, t1, t0);
    mont_sub(f, t0, t0, t2);
    mont_mul(f, t1, t4, Y3);
    mont_mul(f, t2, t0, Y3);
    mont_mul(f, Y3, X3, Z3);
    mont_add(f, Y3, Y3, t2);
    mont_mul(f, X3, t3, X3);
    mont_sub(f, X3, X3, t1);
    mont_mul(f, Z3, t4, Z3);
    mont_mul(f, t1, t3, t0);
    mont_add(f, Z3, Z3, t1);
    l4_copy(r->X, X3);
    l4_copy(r->Y, Y3);
    l4_copy(r->Z, Z3);
}

/* Exception-free doubling, RCB 2016 algorithm 6 (a = -3). */
static void pt_dbl(const curve *k, pt *r, const pt *p) {
    const mont *f = &k->fp;
    limb4 t0, t1, t2, t3, X3, Y3, Z3;
    mont_mul(f, t0, p->X, p->X);
    mont_mul(f, t1, p->Y, p->Y);
    mont_mul(f, t2, p->Z, p->Z);
    mont_mul(f, t3, p->X, p->Y);
    mont_add(f, t3, t3, t3);
    mont_mul(f, Z3, p->X, p->Z);
    mont_add(f, Z3, Z3, Z3);
    mont_mul(f, Y3, k->b, t2);
    mont_sub(f, Y3, Y3, Z3);
    mont_add(f, X3, Y3, Y3);
    mont_add(f, Y3, X3, Y3);
    mont_sub(f, X3, t1, Y3);
    mont_add(f, Y3, t1, Y3);
    mont_mul(f, Y3, X3, Y3);
    mont_mul(f, X3, X3, t3);
    mont_add(f, t3, t2, t2);
    mont_add(f, t2, t2, t3);
    mont_mul(f, Z3, k->b, Z3);
    mont_sub(f, Z3, Z3, t2);
    mont_sub(f, Z3, Z3, t0);
    mont_add(f, t3, Z3, Z3);
    mont_add(f, Z3, Z3, t3);
    mont_add(f, t3, t0, t0);
    mont_add(f, t0, t3, t0);
    mont_sub(f, t0, t0, t2);
    mont_mul(f, t0, t0, Z3);
    mont_add(f, Y3, Y3, t0);
    mont_mul(f, t0, p->Y, p->Z);
    mont_add(f, t0, t0, t0);
    mont_mul(f, Z3, t0, Z3);
    mont_sub(f, X3, X3, Z3);
    mont_mul(f, Z3, t0, t1);
    mont_add(f, Z3, Z3, Z3);
    mont_add(f, Z3, Z3, Z3);
    l4_copy(r->X, X3);
    l4_copy(r->Y, Y3);
    l4_copy(r->Z, Z3);
}

static void pt_infinity(const curve *k, pt *r) {
    memset(r, 0, sizeof *r);
    l4_copy(r->Y, k->fp.one);
}

static void pt_cmov(pt *r, const pt *a, uint64_t mask) {
    l4_cmov(r->X, a->X, mask);
    l4_cmov(r->Y, a->Y, mask);
    l4_cmov(r->Z, a->Z, mask);
}

/* r = [s]p, s a 32-byte big-endian scalar; constant time. */
static void pt_mul(const curve *k, pt *r, const uint8_t s[32], const pt *p) {
    pt tab[16], acc, t;
    pt_infinity(k, &tab[0]);
    tab[1] = *p;
    for (int i = 2; i < 16; i++) pt_add(k, &tab[i], &tab[i - 1], p);
    pt_infinity(k, &acc);
    for (int w = 0; w < 64; w++) {
        for (int d = 0; d < 4; d++) pt_dbl(k, &acc, &acc);
        unsigned nib = (s[w / 2] >> (w % 2 ? 0 : 4)) & 15;
        pt sel;
        memset(&sel, 0, sizeof sel);
        for (unsigned i = 0; i < 16; i++) {
            uint64_t eq = ((uint64_t)(i ^ nib) - 1) >> 63;   /* 1 iff i == nib */
            pt_cmov(&sel, &tab[i], 0 - eq);
        }
        pt_add(k, &t, &acc, &sel);
        acc = t;
    }
    *r = acc;
    zell__crypto_wipe(tab, sizeof tab);
}

/* r = [s]G, s a 32-byte big-endian scalar; constant time. The scalar is
 * recoded into 65 signed radix-16 digits in [-8, 8]; every digit scans all
 * eight affine entries of its P256_BASE row with conditional moves, the
 * selected point is conditionally negated and added (complete formulas,
 * so an accumulator at infinity is fine), and for a zero digit the
 * accumulator is moved back. */
static void pt_mul_base(const curve *k, pt *r, const uint8_t s[32]) {
    int8_t e[65];
    for (int i = 0; i < 32; i++) {
        e[2 * i] = (int8_t)(s[31 - i] & 15);
        e[2 * i + 1] = (int8_t)((s[31 - i] >> 4) & 15);
    }
    int8_t carry = 0;
    for (int i = 0; i < 64; i++) {
        e[i] = (int8_t)(e[i] + carry);
        carry = (int8_t)((e[i] + 8) >> 4);
        e[i] = (int8_t)(e[i] - (int8_t)(carry << 4));
    }
    e[64] = carry;
    pt acc, sel, t;
    pt_infinity(k, &acc);
    l4_copy(sel.Z, k->fp.one);
    for (int j = 0; j < 65; j++) {
        uint64_t neg = (uint64_t)((uint8_t)e[j] >> 7);
        uint64_t abs = (uint64_t)(uint8_t)(e[j] - (((0 - (int8_t)neg) & e[j]) << 1));
        memset(sel.X, 0, sizeof sel.X);
        memset(sel.Y, 0, sizeof sel.Y);
        for (unsigned i = 0; i < 8; i++) {
            uint64_t eq = (((abs ^ (i + 1)) - 1) & ~(abs ^ (i + 1))) >> 63;   /* 1 iff abs == i + 1 */
            l4_cmov(sel.X, P256_BASE[j][i][0], 0 - eq);
            l4_cmov(sel.Y, P256_BASE[j][i][1], 0 - eq);
        }
        limb4 ny, zero = { 0, 0, 0, 0 };
        mont_sub(&k->fp, ny, zero, sel.Y);
        l4_cmov(sel.Y, ny, 0 - neg);
        pt_add(k, &t, &acc, &sel);
        uint64_t nonzero = ((abs - 1) >> 63) ^ 1;   /* 1 iff abs != 0 */
        pt_cmov(&acc, &t, 0 - nonzero);
    }
    *r = acc;
    zell__crypto_wipe(e, sizeof e);
    zell__crypto_wipe(&sel, sizeof sel);
    zell__crypto_wipe(&t, sizeof t);
}

/* Affine coordinates as big-endian bytes; false for the point at infinity. */
static bool pt_affine(const curve *k, const pt *p, uint8_t x[32], uint8_t y[32]) {
    limb4 zi, t;
    if (l4_is_zero(p->Z)) return false;
    mont_inv(&k->fp, zi, p->Z);
    mont_mul(&k->fp, t, p->X, zi);
    mont_from(&k->fp, t, t);
    l4_to_be(x, t);
    if (y) {
        mont_mul(&k->fp, t, p->Y, zi);
        mont_from(&k->fp, t, t);
        l4_to_be(y, t);
    }
    return true;
}

/* Decodes and validates an uncompressed point (on the curve, coordinates < p). */
static bool pt_decode(const curve *k, pt *r, const uint8_t in[65]) {
    if (in[0] != 4) return false;
    limb4 x, y;
    l4_from_be(x, in + 1);
    l4_from_be(y, in + 33);
    if (!l4_lt(x, P_M) || !l4_lt(y, P_M)) return false;
    const mont *f = &k->fp;
    mont_to(f, r->X, x);
    mont_to(f, r->Y, y);
    l4_copy(r->Z, f->one);
    /* y^2 == x^3 - 3x + b */
    limb4 lhs, rhs, t;
    mont_mul(f, lhs, r->Y, r->Y);
    mont_mul(f, rhs, r->X, r->X);
    mont_mul(f, rhs, rhs, r->X);
    mont_add(f, t, r->X, r->X);
    mont_add(f, t, t, r->X);
    mont_sub(f, rhs, rhs, t);
    mont_add(f, rhs, rhs, k->b);
    limb4 d;
    l4_sub(d, lhs, rhs);
    return l4_is_zero(d) == 1;
}

/* 1 <= s < n */
static bool scalar_ok(const uint8_t s[32]) {
    limb4 v;
    l4_from_be(v, s);
    return !l4_is_zero(v) && l4_lt(v, N_M);
}

bool zell__p256_pubkey(uint8_t pub[65], const uint8_t priv[32]) {
    if (!scalar_ok(priv)) return false;
    const curve k = P256;
    pt q;
    pt_mul_base(&k, &q, priv);
    pub[0] = 4;
    return pt_affine(&k, &q, pub + 1, pub + 33);
}

bool zell__p256_ecdh(uint8_t out[32], const uint8_t priv[32], const uint8_t peer[65]) {
    if (!scalar_ok(priv)) return false;
    const curve k = P256;
    pt p, q;
    if (!pt_decode(&k, &p, peer)) return false;
    pt_mul(&k, &q, priv, &p);
    return pt_affine(&k, &q, out, NULL);
}

bool zell__p256_genkey(uint8_t priv[32]) {
    for (int tries = 0; tries < 64; tries++) {
        if (!zell__crypto_random(priv, 32)) return false;
        if (scalar_ok(priv)) return true;
    }
    return false;
}

/* The leftmost 256 bits of a digest as an integer reduced modulo n. */
static void digest_to_scalar(limb4 e, const uint8_t *digest, size_t dlen) {
    uint8_t buf[32] = { 0 };
    if (dlen >= 32) memcpy(buf, digest, 32);
    else memcpy(buf + 32 - dlen, digest, dlen);
    l4_from_be(e, buf);
    limb4 d;
    uint64_t borrow = l4_sub(d, e, N_M);
    l4_cmov(e, d, 0 - (borrow ^ 1));
}

static void hmac2(uint8_t out[32], const uint8_t key[32], const uint8_t *a, size_t an, const uint8_t *b,
                  size_t bn, const uint8_t *c, size_t cn, const uint8_t *d, size_t dn, const uint8_t *e, size_t en) {
    zell_hmac_ctx h;
    zell__hmac_init(&h, ZELL_HASH_SHA256, key, 32);
    zell__hmac_update(&h, a, an);
    zell__hmac_update(&h, b, bn);
    zell__hmac_update(&h, c, cn);
    zell__hmac_update(&h, d, dn);
    zell__hmac_update(&h, e, en);
    zell__hmac_final(&h, out);
    zell__crypto_wipe(&h, sizeof h);
}

bool zell__p256_sign(uint8_t sig[64], const uint8_t priv[32], const uint8_t *digest, size_t dlen,
                      const uint8_t *extra, size_t elen) {
    if (!scalar_ok(priv)) return false;
    const curve k = P256;
    const mont *fn = &k.fn;
    limb4 e, d;
    digest_to_scalar(e, digest, dlen);
    l4_from_be(d, priv);
    uint8_t h1[32];
    l4_to_be(h1, e);
    /* RFC 6979 3.2 (HMAC-SHA256), extra data per 3.6 */
    uint8_t V[32], K[32], z = 0, one = 1;
    memset(V, 1, 32);
    memset(K, 0, 32);
    if (!extra) elen = 0;
    hmac2(K, K, V, 32, &z, 1, priv, 32, h1, 32, extra, elen);
    hmac2(V, K, V, 32, NULL, 0, NULL, 0, NULL, 0, NULL, 0);
    hmac2(K, K, V, 32, &one, 1, priv, 32, h1, 32, extra, elen);
    hmac2(V, K, V, 32, NULL, 0, NULL, 0, NULL, 0, NULL, 0);
    bool ok = false;
    for (int tries = 0; tries < 64 && !ok; tries++) {
        hmac2(V, K, V, 32, NULL, 0, NULL, 0, NULL, 0, NULL, 0);
        if (scalar_ok(V)) {
            pt R;
            pt_mul_base(&k, &R, V);
            uint8_t rx[32];
            if (pt_affine(&k, &R, rx, NULL)) {
                limb4 r, rd, kk, ki, s, t;
                l4_from_be(r, rx);
                uint64_t borrow = l4_sub(rd, r, N_M);
                l4_cmov(r, rd, 0 - (borrow ^ 1));
                if (!l4_is_zero(r)) {
                    /* s = k^-1 (e + r d) mod n, all in Montgomery form */
                    l4_from_be(kk, V);
                    mont_to(fn, kk, kk);
                    mont_inv(fn, ki, kk);
                    limb4 rm, dm, em;
                    mont_to(fn, rm, r);
                    mont_to(fn, dm, d);
                    mont_to(fn, em, e);
                    mont_mul(fn, t, rm, dm);
                    mont_add(fn, t, t, em);
                    mont_mul(fn, s, ki, t);
                    mont_from(fn, s, s);
                    if (!l4_is_zero(s)) {
                        l4_to_be(sig, r);
                        l4_to_be(sig + 32, s);
                        ok = true;
                    }
                    zell__crypto_wipe(kk, sizeof kk);
                    zell__crypto_wipe(ki, sizeof ki);
                    zell__crypto_wipe(dm, sizeof dm);
                }
            }
        }
        if (!ok) {
            hmac2(K, K, V, 32, &z, 1, NULL, 0, NULL, 0, NULL, 0);
            hmac2(V, K, V, 32, NULL, 0, NULL, 0, NULL, 0, NULL, 0);
        }
    }
    zell__crypto_wipe(V, sizeof V);
    zell__crypto_wipe(K, sizeof K);
    zell__crypto_wipe(d, sizeof d);
    return ok;
}

bool zell__p256_verify(const uint8_t sig[64], const uint8_t pub[65], const uint8_t *digest, size_t dlen) {
    if (!scalar_ok(sig) || !scalar_ok(sig + 32)) return false;
    const curve k = P256;
    const mont *fn = &k.fn;
    pt Q;
    if (!pt_decode(&k, &Q, pub)) return false;
    limb4 e, r, s, w, u1, u2;
    digest_to_scalar(e, digest, dlen);
    l4_from_be(r, sig);
    l4_from_be(s, sig + 32);
    mont_to(fn, s, s);
    mont_inv(fn, w, s);
    limb4 em, rm;
    mont_to(fn, em, e);
    mont_to(fn, rm, r);
    mont_mul(fn, u1, em, w);
    mont_mul(fn, u2, rm, w);
    mont_from(fn, u1, u1);
    mont_from(fn, u2, u2);
    uint8_t b1[32], b2[32];
    l4_to_be(b1, u1);
    l4_to_be(b2, u2);
    pt A, B, X;
    pt_mul_base(&k, &A, b1);
    pt_mul(&k, &B, b2, &Q);
    pt_add(&k, &X, &A, &B);
    uint8_t xb[32];
    if (!pt_affine(&k, &X, xb, NULL)) return false;
    limb4 x, xd;
    l4_from_be(x, xb);
    uint64_t borrow = l4_sub(xd, x, N_M);
    l4_cmov(x, xd, 0 - (borrow ^ 1));
    limb4 diff;
    l4_sub(diff, x, r);
    return l4_is_zero(diff) == 1;
}
