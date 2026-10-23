/* util.c - shared helpers of the crypto primitives: wiping, constant-time
 * comparison, the CSPRNG entry point and CPU feature detection. */
#include "crypto.h"
#include "../platform/platform.h"

#include <stdlib.h>
#include <string.h>

/* memset, then a compiler barrier that makes the zeroed memory observable,
 * so the stores cannot be dropped as dead (explicit_bzero without needing
 * it from libc). A byte loop through a volatile pointer was ~1 instruction
 * per byte; TLS wipes a 6 KB AES-GCM context copy per record. */
void zell__crypto_wipe(void *p, size_t n) {
    if (!n) return;
    memset(p, 0, n);
#if defined(__GNUC__) || defined(__clang__)
    __asm__ __volatile__("" : : "r"(p) : "memory");
#else
    volatile uint8_t *v = p;
    while (n--) *v++ = 0;
#endif
}

int zell__crypto_ct_eq(const uint8_t *a, const uint8_t *b, size_t n) {
    uint32_t acc = 0;
    for (size_t i = 0; i < n; i++) acc |= (uint32_t)(a[i] ^ b[i]);
    return (int)(1 & ((acc - 1) >> 8));
}

bool zell__crypto_random(uint8_t *buf, size_t n) {
    return n == 0 || zell__plat_random_bytes(buf, n);
}

#if defined(__x86_64__) || defined(__i386__)
#include <cpuid.h>
static int aesni_state = -1;   /* -1 unknown, 0 no, 1 yes */
static int aes_forced_portable;

bool zell__cpu_has_aesni(void) {
    if (aes_forced_portable) return false;
    int s = __atomic_load_n(&aesni_state, __ATOMIC_RELAXED);
    if (s < 0) {
        unsigned a, b, c, d;
        s = 0;
        if (__get_cpuid(1, &a, &b, &c, &d)) {
            /* ECX bit 25 AES, bit 1 PCLMULQDQ, bit 19 SSE4.1, bit 9 SSSE3 */
            s = (c & (1u << 25)) && (c & (1u << 1)) && (c & (1u << 19)) && (c & (1u << 9));
        }
        const char *env = getenv("ZELL_NO_AESNI");
        if (env && env[0] == '1') s = 0;
        __atomic_store_n(&aesni_state, s, __ATOMIC_RELAXED);
    }
    return s == 1;
}

void zell__aes_force_portable(int on) { aes_forced_portable = on; }

static int shani_state = -1;   /* -1 unknown, 0 no, 1 yes */
static int sha_forced_portable;

bool zell__cpu_has_shani(void) {
    if (sha_forced_portable) return false;
    int s = __atomic_load_n(&shani_state, __ATOMIC_RELAXED);
    if (s < 0) {
        unsigned a, b, c, d;
        s = 0;
        /* leaf 1 ECX: bit 19 SSE4.1, bit 9 SSSE3; leaf 7.0 EBX bit 29: SHA */
        if (__get_cpuid(1, &a, &b, &c, &d) && (c & (1u << 19)) && (c & (1u << 9)) &&
            __get_cpuid_count(7, 0, &a, &b, &c, &d))
            s = (b & (1u << 29)) != 0;
        const char *env = getenv("ZELL_NO_SHANI");
        if (env && env[0] == '1') s = 0;
        __atomic_store_n(&shani_state, s, __ATOMIC_RELAXED);
    }
    return s == 1;
}

void zell__sha_force_portable(int on) { sha_forced_portable = on; }

static int vaes_state = -1;   /* -1 unknown, 0 no, 1 yes */
static int vaes_forced_off;

bool zell__cpu_has_vaes(void) {
    if (vaes_forced_off || !zell__cpu_has_aesni()) return false;
    int s = __atomic_load_n(&vaes_state, __ATOMIC_RELAXED);
    if (s < 0) {
        unsigned a, b, c, d;
        s = 0;
        /* leaf 1 ECX: bit 27 OSXSAVE, bit 28 AVX; XCR0 bits 1-2: the OS
         * saves XMM and YMM state; leaf 7.0: EBX bit 5 AVX2, ECX bit 9 VAES,
         * bit 10 VPCLMULQDQ */
        if (__get_cpuid(1, &a, &b, &c, &d) && (c & (1u << 27)) && (c & (1u << 28))) {
            unsigned xlo, xhi;
            __asm__ __volatile__("xgetbv" : "=a"(xlo), "=d"(xhi) : "c"(0));
            (void)xhi;
            if ((xlo & 6) == 6 && __get_cpuid_count(7, 0, &a, &b, &c, &d))
                s = (b & (1u << 5)) && (c & (1u << 9)) && (c & (1u << 10));
        }
        const char *env = getenv("ZELL_NO_VAES");
        if (env && env[0] == '1') s = 0;
        __atomic_store_n(&vaes_state, s, __ATOMIC_RELAXED);
    }
    return s == 1;
}

void zell__vaes_force_off(int on) { vaes_forced_off = on; }

static int avx2_state = -1;   /* -1 unknown, 0 no, 1 yes */
static int avx2_forced_off;

bool zell__cpu_has_avx2(void) {
    if (avx2_forced_off) return false;
    int s = __atomic_load_n(&avx2_state, __ATOMIC_RELAXED);
    if (s < 0) {
        unsigned a, b, c, d;
        s = 0;
        /* leaf 1 ECX: bit 27 OSXSAVE, bit 28 AVX; XCR0 bits 1-2: the OS
         * saves XMM and YMM state; leaf 7.0 EBX bit 5: AVX2 */
        if (__get_cpuid(1, &a, &b, &c, &d) && (c & (1u << 27)) && (c & (1u << 28))) {
            unsigned xlo, xhi;
            __asm__ __volatile__("xgetbv" : "=a"(xlo), "=d"(xhi) : "c"(0));
            (void)xhi;
            if ((xlo & 6) == 6 && __get_cpuid_count(7, 0, &a, &b, &c, &d)) s = (b & (1u << 5)) != 0;
        }
        const char *env = getenv("ZELL_NO_AVX2");
        if (env && env[0] == '1') s = 0;
        __atomic_store_n(&avx2_state, s, __ATOMIC_RELAXED);
    }
    return s == 1;
}

void zell__avx2_force_off(int on) { avx2_forced_off = on; }
#else
bool zell__cpu_has_aesni(void) { return false; }
void zell__aes_force_portable(int on) { (void)on; }
bool zell__cpu_has_shani(void) { return false; }
void zell__sha_force_portable(int on) { (void)on; }
bool zell__cpu_has_vaes(void) { return false; }
void zell__vaes_force_off(int on) { (void)on; }
bool zell__cpu_has_avx2(void) { return false; }
void zell__avx2_force_off(int on) { (void)on; }
#endif
