/* Tests for the cryptographic primitives (src/crypto/): known-answer vectors
 * from the RFCs and NIST documents named next to each test, plus
 * Wycheproof-style negative cases (tampered tags and signatures, invalid
 * points, non-canonical encodings, small-order inputs). Both AES paths are
 * exercised (AES-NI when the CPU has it, and the bitsliced fallback), and
 * both SHA-256 paths (SHA-NI when available, and the portable code). */
#include "test.h"
#include "../src/crypto/crypto.h"
#include "vectors/rsa2048.h"

#include <time.h>

static size_t unhex(uint8_t *out, const char *hx) {
    size_t n = 0;
    for (; hx[0] && hx[1]; hx += 2) {
        unsigned v;
        sscanf(hx, "%2x", &v);
        out[n++] = (uint8_t)v;
    }
    return n;
}

static void check_hex(const uint8_t *got, size_t n, const char *want) {
    uint8_t w[1024];
    size_t wn = unhex(w, want);
    if (wn != n || memcmp(got, w, n) != 0) {
        fprintf(stderr, "mismatch: want %s\n           got  ", want);
        for (size_t i = 0; i < n; i++) fprintf(stderr, "%02x", got[i]);
        fprintf(stderr, "\n");
        assert(0);
    }
}

/* ---- SHA-2, HMAC, HKDF ---- */

TEST(sha2_fips180) {
    uint8_t d[64];
    zell__hash(ZELL_HASH_SHA256, (const uint8_t *)"abc", 3, d);
    check_hex(d, 32, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    zell__hash(ZELL_HASH_SHA384, (const uint8_t *)"abc", 3, d);
    check_hex(d, 48, "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7");
    zell__hash(ZELL_HASH_SHA512, (const uint8_t *)"abc", 3, d);
    check_hex(d, 64, "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
    const char *two = "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    zell__hash(ZELL_HASH_SHA256, (const uint8_t *)two, strlen(two), d);
    check_hex(d, 32, "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
    zell__hash(ZELL_HASH_SHA1, (const uint8_t *)"abc", 3, d);
    check_hex(d, 20, "a9993e364706816aba3e25717850c26c9cd0d89d");
    /* one million 'a', fed in odd-sized pieces through the streaming API */
    zell_hash_ctx c;
    zell__hash_init(&c, ZELL_HASH_SHA256);
    uint8_t a[997];
    memset(a, 'a', sizeof a);
    size_t left = 1000000;
    while (left) {
        size_t k = left < sizeof a ? left : sizeof a;
        zell__hash_update(&c, a, k);
        left -= k;
    }
    zell__hash_final(&c, d);
    check_hex(d, 32, "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    assert(zell__hash_by_name("SHA-384", 7) == ZELL_HASH_SHA384);
    assert(zell__hash_by_name("md5", 3) == 0);
}

/* SHA-256 around the padding and block boundaries (expected digests from
 * Python's hashlib), one-shot and streamed in uneven pieces, on the SHA-NI
 * path when the CPU has it and on the portable path. */
static void sha256_lengths(void) {
    static const struct { size_t n; const char *want; } V[] = {
        { 0, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" },
        { 1, "e77b9a9ae9e30b0dbdb6f510a264ef9de781501d7b6b92ae89eb059c5ab743db" },
        { 55, "63c06c35402b73061b2b60338121fe8f4895640a178ed01c4b2e40ff8dcd1755" },
        { 56, "86512867c0c0b58974d4a9f76fab04b3ed84c5ae5845bd807f809a7884ec24e5" },
        { 63, "56547b4ccd1b089babcb872418aa6d3be61e3aba1b97e0eda0934d729ebe9e0c" },
        { 64, "44aee5fa258a25ab9eeebaa630ea0ea92b017efb95fbce6f91c9c181e4d8ebe2" },
        { 65, "75a506795a148c07e58b5579d08badc50e39e2e0092c23042a429401dc10d7bf" },
        { 1000, "7994e00959d889b2edd138584884b26ecd04053d86779cb88d89202dea18e599" },
        { 1000000, "2784a21b4c3200a3ba2e60fb6911faadeaba14f0587384bf1344a52fdc377556" },
    };
    static uint8_t m[1000000];
    for (size_t i = 0; i < sizeof m; i++) m[i] = (uint8_t)(i * 13 + 5);
    static const size_t pieces[] = { 1, 7, 63, 64, 65, 200, 4096 };
    for (size_t v = 0; v < sizeof V / sizeof V[0]; v++) {
        uint8_t d[32];
        zell__hash(ZELL_HASH_SHA256, m, V[v].n, d);
        check_hex(d, 32, V[v].want);
        for (size_t pi = 0; pi < sizeof pieces / sizeof pieces[0]; pi++) {
            if (V[v].n > 100000 && pieces[pi] < 64) continue;
            zell_hash_ctx c;
            zell__hash_init(&c, ZELL_HASH_SHA256);
            for (size_t off = 0; off < V[v].n;) {
                size_t k = V[v].n - off < pieces[pi] ? V[v].n - off : pieces[pi];
                zell__hash_update(&c, m + off, k);
                off += k;
            }
            zell__hash_final(&c, d);
            check_hex(d, 32, V[v].want);
        }
    }
}

TEST(sha256_both_paths) {
    int have_ni = zell__cpu_has_shani();
    sha256_lengths();
    zell__sha_force_portable(1);
    assert(!zell__cpu_has_shani());
    sha256_lengths();
    zell__sha_force_portable(0);
    printf("       (SHA-NI %s)\n", have_ni ? "available: both paths tested" : "not available: portable path only");
}

TEST(hmac_rfc4231) {
    uint8_t key[20], d[64];
    memset(key, 0x0b, 20);
    const uint8_t *msg = (const uint8_t *)"Hi There";
    zell__hmac(ZELL_HASH_SHA256, key, 20, msg, 8, d);
    check_hex(d, 32, "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
    zell__hmac(ZELL_HASH_SHA384, key, 20, msg, 8, d);
    check_hex(d, 48, "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6");
    zell__hmac(ZELL_HASH_SHA512, key, 20, msg, 8, d);
    check_hex(d, 64, "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854");
    const char *jm = "what do ya want for nothing?";
    zell__hmac(ZELL_HASH_SHA256, (const uint8_t *)"Jefe", 4, (const uint8_t *)jm, strlen(jm), d);
    check_hex(d, 32, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    /* test case 6: key longer than the block size (131 bytes of 0xaa) */
    uint8_t big[131];
    memset(big, 0xaa, sizeof big);
    const char *m6 = "Test Using Larger Than Block-Size Key - Hash Key First";
    zell__hmac(ZELL_HASH_SHA256, big, sizeof big, (const uint8_t *)m6, strlen(m6), d);
    check_hex(d, 32, "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
}

TEST(hkdf_rfc5869) {
    uint8_t ikm[22], salt[13], info[10], prk[64], okm[82];
    memset(ikm, 0x0b, 22);
    for (int i = 0; i < 13; i++) salt[i] = (uint8_t)i;
    for (int i = 0; i < 10; i++) info[i] = (uint8_t)(0xf0 + i);
    /* test case 1 */
    zell__hkdf_extract(ZELL_HASH_SHA256, salt, 13, ikm, 22, prk);
    check_hex(prk, 32, "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5");
    assert(zell__hkdf_expand(ZELL_HASH_SHA256, prk, 32, info, 10, okm, 42));
    check_hex(okm, 42, "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865");
    /* test case 3: empty salt and info */
    zell__hkdf_extract(ZELL_HASH_SHA256, NULL, 0, ikm, 22, prk);
    check_hex(prk, 32, "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04");
    assert(zell__hkdf_expand(ZELL_HASH_SHA256, prk, 32, NULL, 0, okm, 42));
    check_hex(okm, 42, "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8");
    /* too long */
    uint8_t huge[255 * 32 + 1];
    assert(!zell__hkdf_expand(ZELL_HASH_SHA256, prk, 32, NULL, 0, huge, sizeof huge));
    assert(zell__hkdf_expand(ZELL_HASH_SHA256, prk, 32, NULL, 0, huge, sizeof huge - 1));
}

/* ---- AES / GCM (FIPS 197 appendix C, the GCM spec test cases 1-4, 13-16) ---- */

static uint8_t sbox_ref(uint8_t x) {
    /* multiplicative inverse in GF(2^8) by brute force, then the affine map */
    uint8_t inv = 0;
    for (int y = 1; y < 256 && x; y++) {
        int a = x, b = y, p = 0;
        for (int i = 0; i < 8; i++) {
            if (b & 1) p ^= a;
            b >>= 1;
            a <<= 1;
            if (a & 0x100) a ^= 0x11b;
        }
        if (p == 1) { inv = (uint8_t)y; break; }
    }
    uint8_t s = inv;
    for (int i = 1; i <= 4; i++) s ^= (uint8_t)((inv << i) | (inv >> (8 - i)));
    return s ^ 0x63;
}

static void aes_vectors(void) {
    zell_aes_key k;
    uint8_t key[32], pt[16], ct[16];
    for (int i = 0; i < 32; i++) key[i] = (uint8_t)i;
    unhex(pt, "00112233445566778899aabbccddeeff");
    assert(zell__aes_init(&k, key, 16));
    zell__aes_encrypt_block(&k, pt, ct);
    check_hex(ct, 16, "69c4e0d86a7b0430d8cdb78070b4c55a");
    assert(zell__aes_init(&k, key, 24));
    zell__aes_encrypt_block(&k, pt, ct);
    check_hex(ct, 16, "dda97ca4864cdfe06eaf70a0ec0d7191");
    assert(zell__aes_init(&k, key, 32));
    zell__aes_encrypt_block(&k, pt, ct);
    check_hex(ct, 16, "8ea2b7ca516745bfeafc49904b496089");
    assert(!zell__aes_init(&k, key, 17));
}

typedef struct {
    const char *key, *iv, *pt, *aad, *ct, *tag;
} gcm_vec;

static const gcm_vec GCM[] = {
    { "00000000000000000000000000000000", "000000000000000000000000", "", "", "", "58e2fccefa7e3061367f1d57a4e7455a" },
    { "00000000000000000000000000000000", "000000000000000000000000", "00000000000000000000000000000000", "",
      "0388dace60b6a392f328c2b971b2fe78", "ab6e47d42cec13bdf53a67b21257bddf" },
    { "feffe9928665731c6d6a8f9467308308", "cafebabefacedbaddecaf888",
      "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
      "",
      "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985",
      "4d5c2af327cd64a62cf35abd2ba6fab4" },
    { "feffe9928665731c6d6a8f9467308308", "cafebabefacedbaddecaf888",
      "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
      "feedfacedeadbeeffeedfacedeadbeefabaddad2",
      "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091",
      "5bc94fbc3221a5db94fae95ae7121a47" },
    { "0000000000000000000000000000000000000000000000000000000000000000", "000000000000000000000000", "", "", "",
      "530f8afbc74536b9a963b4f1c4cb738b" },
    { "0000000000000000000000000000000000000000000000000000000000000000", "000000000000000000000000",
      "00000000000000000000000000000000", "", "cea7403d4d606b6e074ec5d3baf39d18", "d0d1c8a799996bf0265b98b5d48ab919" },
    { "feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308", "cafebabefacedbaddecaf888",
      "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
      "feedfacedeadbeeffeedfacedeadbeefabaddad2",
      "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662",
      "76fc6ece0f4e1768cddf8853bb2d551b" },
};

static void gcm_vectors(void) {
    for (size_t i = 0; i < sizeof GCM / sizeof GCM[0]; i++) {
        const gcm_vec *v = &GCM[i];
        uint8_t key[32], iv[12], pt[128], aad[64], out[160], back[128];
        size_t kl = unhex(key, v->key), pl = unhex(pt, v->pt), al = unhex(aad, v->aad);
        unhex(iv, v->iv);
        zell_gcm_ctx c;
        assert(zell__gcm_init(&c, key, kl));
        zell__gcm_seal(&c, iv, aad, al, pt, pl, out);
        check_hex(out, pl, v->ct);
        check_hex(out + pl, 16, v->tag);
        assert(zell__gcm_open(&c, iv, aad, al, out, pl, back));
        assert(memcmp(back, pt, pl) == 0);
        /* tampering with the tag, the ciphertext or the AAD is detected */
        out[pl] ^= 1;
        assert(!zell__gcm_open(&c, iv, aad, al, out, pl, back));
        out[pl] ^= 1;
        if (pl) {
            out[0] ^= 0x80;
            assert(!zell__gcm_open(&c, iv, aad, al, out, pl, back));
            out[0] ^= 0x80;
        }
        aad[0] ^= 1;
        assert(!zell__gcm_open(&c, iv, aad, al ? al : 1, out, pl, back));
    }
    /* every length 0..300: in-place seal equals out-of-place seal, and opens */
    uint8_t key[16] = { 1, 2, 3 }, iv[12] = { 9 }, buf[300 + 16], buf2[300 + 16], back[300];
    zell_gcm_ctx c;
    zell__gcm_init(&c, key, 16);
    for (size_t n = 0; n <= 300; n++) {
        for (size_t i = 0; i < n; i++) buf[i] = (uint8_t)(i * 7 + n);
        memcpy(back, buf, n);
        zell__gcm_seal(&c, iv, back, n / 3, back, n, buf2);
        zell__gcm_seal(&c, iv, back, n / 3, buf, n, buf);
        assert(memcmp(buf, buf2, n + 16) == 0);
        assert(zell__gcm_open(&c, iv, back, n / 3, buf, n, buf));
        assert(memcmp(buf, back, n) == 0);
    }
}

/* Encrypts a fixed long message and returns a digest of the output, so the
 * AES-NI and bitsliced paths can be compared. */
static void gcm_digest(uint8_t d[32]) {
    static uint8_t msg[4099], out[4099 + 16];
    for (size_t i = 0; i < sizeof msg; i++) msg[i] = (uint8_t)(i * 31 + 7);
    uint8_t key[32];
    for (int i = 0; i < 32; i++) key[i] = (uint8_t)(i * 3);
    zell_gcm_ctx c;
    zell_hash_ctx h;
    zell__hash_init(&h, ZELL_HASH_SHA256);
    for (size_t kl = 16; kl <= 32; kl += 16) {
        zell__gcm_init(&c, key, kl);
        for (size_t n = 0; n < sizeof msg; n += 333) {
            zell__gcm_seal(&c, key, msg, n % 41, msg, n, out);
            zell__hash_update(&h, out, n + 16);
        }
    }
    zell__hash_final(&h, d);
}

TEST(aes_gcm) {
    for (int x = 0; x < 256; x++) assert(zell__aes_sbox_ct((uint8_t)x) == sbox_ref((uint8_t)x));
    uint8_t d1[32], d2[32];
    int have_ni = zell__cpu_has_aesni();
    aes_vectors();
    gcm_vectors();
    gcm_digest(d1);
    zell__aes_force_portable(1);
    assert(!zell__cpu_has_aesni());
    aes_vectors();
    gcm_vectors();
    gcm_digest(d2);
    zell__aes_force_portable(0);
    assert(memcmp(d1, d2, 32) == 0);
    printf("       (AES-NI %s)\n", have_ni ? "available: both paths tested" : "not available: portable path only");
}

static double gcm_mbps(int portable) {
    static uint8_t buf[16384 + 16];
    uint8_t key[16] = { 0 }, iv[12] = { 0 };
    zell__aes_force_portable(portable);
    zell_gcm_ctx c;
    zell__gcm_init(&c, key, 16);
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    int iters = portable ? 200 : 2000;
    for (int i = 0; i < iters; i++) zell__gcm_seal(&c, iv, NULL, 0, buf, 16384, buf);
    clock_gettime(CLOCK_MONOTONIC, &b);
    zell__aes_force_portable(0);
    double s = (double)(b.tv_sec - a.tv_sec) + (double)(b.tv_nsec - a.tv_nsec) / 1e9;
    return (double)iters * 16384 / s / 1e6;
}

static double chacha_mbps(void) {
    static uint8_t buf[16384 + 16];
    uint8_t key[32] = { 0 }, iv[12] = { 0 };
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    for (int i = 0; i < 500; i++) zell__chacha20poly1305_seal(key, iv, NULL, 0, buf, 16384, buf);
    clock_gettime(CLOCK_MONOTONIC, &b);
    double s = (double)(b.tv_sec - a.tv_sec) + (double)(b.tv_nsec - a.tv_nsec) / 1e9;
    return 500.0 * 16384 / s / 1e6;
}

TEST(throughput) {
    /* informational only (no assertion on speed) */
    if (getenv("ZELL_CRYPTO_BENCH") == NULL) return;
    printf("       AES-128-GCM: %.0f MB/s with AES-NI%s, %.0f MB/s bitsliced; ChaCha20-Poly1305: %.0f MB/s\n",
           gcm_mbps(0), zell__cpu_has_aesni() ? "" : " (unavailable)", gcm_mbps(1), chacha_mbps());
}

/* ---- ChaCha20-Poly1305 (RFC 8439 2.4.2, 2.5.2, 2.8.2) ---- */

TEST(chacha20poly1305_rfc8439) {
    const char *sun = "Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, "
                      "sunscreen would be it.";
    size_t n = strlen(sun);
    uint8_t key[32], nonce[12], out[256], back[256];
    for (int i = 0; i < 32; i++) key[i] = (uint8_t)i;
    unhex(nonce, "000000000000004a00000000");
    zell__chacha20(key, nonce, 1, (const uint8_t *)sun, n, out);
    check_hex(out, n, "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0bf91b65c5524733ab8f593dabcd62b357"
                      "1639d624e65152ab8f530c359f0861d807ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736"
                      "5af90bbf74a35be6b40b8eedf2785e42874d");
    uint8_t pk[32], tag[16];
    unhex(pk, "85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
    zell__poly1305(pk, (const uint8_t *)"Cryptographic Forum Research Group", 34, tag);
    check_hex(tag, 16, "a8061dc1305136c6c22b8baf0c0127a9");
    for (int i = 0; i < 32; i++) key[i] = (uint8_t)(0x80 + i);
    unhex(nonce, "070000004041424344454647");
    uint8_t aad[12];
    unhex(aad, "50515253c0c1c2c3c4c5c6c7");
    zell__chacha20poly1305_seal(key, nonce, aad, 12, (const uint8_t *)sun, n, out);
    check_hex(out, n + 16,
              "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71"
              "de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4de"
              "f08e4b7a9de576d26586cec64b61161ae10b594f09e26a7e902ecbd0600691");
    assert(zell__chacha20poly1305_open(key, nonce, aad, 12, out, n, back));
    assert(memcmp(back, sun, n) == 0);
    out[n + 15] ^= 0x40;
    assert(!zell__chacha20poly1305_open(key, nonce, aad, 12, out, n, back));
    out[n + 15] ^= 0x40;
    aad[11] ^= 1;
    assert(!zell__chacha20poly1305_open(key, nonce, aad, 12, out, n, back));
}

/* ---- X25519 (RFC 7748 5.2, 6.1) ---- */

TEST(x25519_rfc7748) {
    uint8_t k[32], u[32], out[32];
    unhex(k, "a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
    unhex(u, "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
    assert(zell__x25519(out, k, u));
    check_hex(out, 32, "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552");
    unhex(k, "4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d");
    unhex(u, "e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493");
    assert(zell__x25519(out, k, u));
    check_hex(out, 32, "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957");
    /* iterated: k = u = 9, 1 and 1000 iterations */
    uint8_t kk[32] = { 9 }, uu[32] = { 9 }, r[32];
    for (int i = 1; i <= 1000; i++) {
        zell__x25519(r, kk, uu);
        memcpy(uu, kk, 32);
        memcpy(kk, r, 32);
        if (i == 1) check_hex(kk, 32, "422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079");
    }
    check_hex(kk, 32, "684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51");
    /* Diffie-Hellman */
    uint8_t a[32], b[32], pa[32], pb[32], s1[32], s2[32];
    unhex(a, "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    unhex(b, "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
    zell__x25519_base(pa, a);
    zell__x25519_base(pb, b);
    check_hex(pa, 32, "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
    check_hex(pb, 32, "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
    assert(zell__x25519(s1, a, pb) && zell__x25519(s2, b, pa));
    check_hex(s1, 32, "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
    assert(memcmp(s1, s2, 32) == 0);
    /* Wycheproof: small-order points give the all-zero secret, which is rejected */
    uint8_t zero[32] = { 0 }, one[32] = { 1 };
    assert(!zell__x25519(out, a, zero));
    assert(!zell__x25519(out, a, one));
    /* the high bit of u is ignored */
    unhex(u, "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
    unhex(k, "a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
    u[31] |= 0x80;
    zell__x25519(out, k, u);
    check_hex(out, 32, "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552");
}

/* ---- Ed25519 (RFC 8032 7.1, tests 1-3 and the 1023-byte test 4 is replaced by a round trip) ---- */

typedef struct {
    const char *seed, *pub, *msg, *sig;
} ed_vec;

static const ed_vec ED[] = {
    { "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
      "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a", "",
      "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b" },
    { "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
      "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c", "72",
      "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00" },
    { "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
      "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025", "af82",
      "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a" },
};

TEST(ed25519_rfc8032) {
    for (size_t i = 0; i < sizeof ED / sizeof ED[0]; i++) {
        uint8_t seed[32], pub[32], msg[16], sig[64], want[64];
        unhex(seed, ED[i].seed);
        size_t ml = unhex(msg, ED[i].msg);
        zell__ed25519_pubkey(pub, seed);
        check_hex(pub, 32, ED[i].pub);
        zell__ed25519_sign(sig, seed, msg, ml);
        check_hex(sig, 64, ED[i].sig);
        unhex(want, ED[i].sig);
        assert(zell__ed25519_verify(want, pub, msg, ml));
        /* any flipped bit in signature, key or message fails */
        want[5] ^= 4;
        assert(!zell__ed25519_verify(want, pub, msg, ml));
        want[5] ^= 4;
        want[40] ^= 1;
        assert(!zell__ed25519_verify(want, pub, msg, ml));
        want[40] ^= 1;
        msg[ml] = 0;
        assert(!zell__ed25519_verify(want, pub, msg, ml + 1));
        pub[3] ^= 2;
        assert(!zell__ed25519_verify(want, pub, msg, ml));
    }
    /* Wycheproof: S >= L (S + L) is rejected (malleability) */
    uint8_t seed[32], pub[32], sig[64];
    unhex(seed, ED[0].seed);
    unhex(pub, ED[0].pub);
    unhex(sig, ED[0].sig);
    uint8_t L[32];
    unhex(L, "edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010");
    unsigned carry = 0;
    for (int i = 0; i < 32; i++) {
        unsigned s = sig[32 + i] + L[i] + carry;
        sig[32 + i] = (uint8_t)s;
        carry = s >> 8;
    }
    assert(!zell__ed25519_verify(sig, pub, NULL, 0));
    /* non-canonical public key encoding (y = p) */
    uint8_t bad[32];
    unhex(bad, "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f");
    unhex(sig, ED[0].sig);
    assert(!zell__ed25519_verify(sig, bad, NULL, 0));
    /* longer message round trip */
    uint8_t m[1023];
    for (int i = 0; i < 1023; i++) m[i] = (uint8_t)(i * 13);
    zell__ed25519_sign(sig, seed, m, sizeof m);
    assert(zell__ed25519_verify(sig, pub, m, sizeof m));
}

/* ---- P-256 (RFC 6979 A.2.5, NIST CAVS ECC CDH) ---- */

TEST(p256_ecdsa_ecdh) {
    uint8_t priv[32], pub[65], sig[64], d[32];
    unhex(priv, "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721");
    assert(zell__p256_pubkey(pub, priv));
    check_hex(pub, 65, "0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb67903fe1008b8bc99a41ae9e95628bc64"
                       "f2f1b20c2d7e9f5177a3c294d4462299");
    zell__hash(ZELL_HASH_SHA256, (const uint8_t *)"sample", 6, d);
    assert(zell__p256_sign(sig, priv, d, 32, NULL, 0));
    check_hex(sig, 64, "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716f7cb1c942d657c41d436c7a1b6e29f65"
                       "f3e900dbb9aff4064dc4ab2f843acda8");
    assert(zell__p256_verify(sig, pub, d, 32));
    zell__hash(ZELL_HASH_SHA256, (const uint8_t *)"test", 4, d);
    assert(zell__p256_sign(sig, priv, d, 32, NULL, 0));
    check_hex(sig, 64, "f1abb023518351cd71d881567b1ea663ed3efcf6c5132b354f28d3b0b7d38367019f4113742a2b14bd25926b49c64915"
                       "5f267e60d3814b4c0cc84250e46f0083");
    assert(zell__p256_verify(sig, pub, d, 32));
    /* negative cases */
    sig[10] ^= 1;
    assert(!zell__p256_verify(sig, pub, d, 32));
    sig[10] ^= 1;
    d[0] ^= 1;
    assert(!zell__p256_verify(sig, pub, d, 32));
    d[0] ^= 1;
    uint8_t zsig[64] = { 0 };
    assert(!zell__p256_verify(zsig, pub, d, 32));
    uint8_t offc[65];
    memcpy(offc, pub, 65);
    offc[64] ^= 1;   /* not on the curve */
    assert(!zell__p256_verify(sig, offc, d, 32));
    /* hedged signatures differ but verify; SHA-384 digests use the leftmost 256 bits */
    uint8_t s2[64], d384[48], extra[32] = { 7 };
    assert(zell__p256_sign(s2, priv, d, 32, extra, 32));
    assert(memcmp(s2, sig, 64) != 0 && zell__p256_verify(s2, pub, d, 32));
    zell__hash(ZELL_HASH_SHA384, (const uint8_t *)"sample", 6, d384);
    assert(zell__p256_sign(s2, priv, d384, 48, NULL, 0));
    assert(zell__p256_verify(s2, pub, d384, 48));
    /* ECDH */
    uint8_t di[32], peer[65], z[32], q[65];
    unhex(di, "7d7dc5f71eb29ddaf80d6214632eeae03d9058af1fb6d22ed80badb62bc1a534");
    unhex(peer, "04700c48f77f56584c5cc632ca65640db91b6bacce3a4df6b42ce7cc838833d287db71e509e3fd9b060ddb20ba5c51dcc5"
                "948d46fbf640dfe0441782cab85fa4ac");
    assert(zell__p256_pubkey(q, di));
    check_hex(q, 65, "04ead218590119e8876b29146ff89ca61770c4edbbf97d38ce385ed281d8a6b23028af61281fd35e2fa7002523acc85a"
                     "429cb06ee6648325389f59edfce1405141");
    assert(zell__p256_ecdh(z, di, peer));
    check_hex(z, 32, "46fc62106420ff012e54a434fbdd2d25ccc5852060561e68040dd7778997bd7b");
    peer[40] ^= 1;
    assert(!zell__p256_ecdh(z, di, peer));
    /* invalid scalars */
    uint8_t zero[32] = { 0 }, nn[32];
    unhex(nn, "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551");
    assert(!zell__p256_pubkey(q, zero) && !zell__p256_pubkey(q, nn));
    /* random key round trip */
    uint8_t rk[32], rp[65];
    assert(zell__p256_genkey(rk) && zell__p256_pubkey(rp, rk));
    assert(zell__p256_sign(s2, rk, d, 32, NULL, 0) && zell__p256_verify(s2, rp, d, 32));
}

/* ---- RSA (PKCS#1 v1.5 and PSS, RFC 8017) ---- */

TEST(rsa_pkcs1_pss) {
    static uint8_t der[2048], n[256], sig1[256], sig2[256], out[512];
    size_t dl = unhex(der, RSA_PRIV_DER);
    unhex(n, RSA_N);
    unhex(sig1, RSA_PKCS1_SIG_ABC);
    unhex(sig2, RSA_PSS_SIG_ABC);
    const uint8_t e[3] = { 1, 0, 1 };
    uint8_t d[32];
    zell__hash(ZELL_HASH_SHA256, (const uint8_t *)"abc", 3, d);
    assert(zell__rsa_priv_size(der, dl) == 256);
    assert(zell__rsa_verify_pkcs1(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, sig1, 256));
    assert(zell__rsa_verify_pss(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, sig2, 256, 32));
    assert(zell__rsa_verify_pss(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, sig2, 256, -1));
    assert(!zell__rsa_verify_pss(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, sig2, 256, 20));
    /* PKCS#1 v1.5 signing is deterministic: same bytes as the reference */
    size_t sl = 0;
    assert(zell__rsa_sign_pkcs1(der, dl, ZELL_HASH_SHA256, d, 32, out, &sl) && sl == 256);
    assert(memcmp(out, sig1, 256) == 0);
    assert(zell__rsa_sign_pss(der, dl, ZELL_HASH_SHA256, d, 32, out, &sl) && sl == 256);
    assert(zell__rsa_verify_pss(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, out, 256, 32));
    /* negative cases: wrong hash, flipped bits, truncated signature, s >= n */
    assert(!zell__rsa_verify_pkcs1(n, 256, e, 3, ZELL_HASH_SHA384, d, 32, sig1, 256));
    sig1[100] ^= 1;
    assert(!zell__rsa_verify_pkcs1(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, sig1, 256));
    sig1[100] ^= 1;
    sig2[255] ^= 1;
    assert(!zell__rsa_verify_pss(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, sig2, 256, -1));
    assert(!zell__rsa_verify_pkcs1(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, sig1, 255));
    assert(!zell__rsa_verify_pkcs1(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, n, 256));
    d[0] ^= 1;
    assert(!zell__rsa_verify_pkcs1(n, 256, e, 3, ZELL_HASH_SHA256, d, 32, sig1, 256));
    /* malformed private key */
    der[5] ^= 0xff;
    assert(!zell__rsa_sign_pkcs1(der, dl, ZELL_HASH_SHA256, d, 32, out, &sl));
}

int main(void) {
    printf("test_crypto\n");
    RUN(sha2_fips180);
    RUN(sha256_both_paths);
    RUN(hmac_rfc4231);
    RUN(hkdf_rfc5869);
    RUN(aes_gcm);
    RUN(chacha20poly1305_rfc8439);
    RUN(x25519_rfc7748);
    RUN(ed25519_rfc8032);
    RUN(p256_ecdsa_ecdh);
    RUN(rsa_pkcs1_pss);
    RUN(throughput);
    return 0;
}
