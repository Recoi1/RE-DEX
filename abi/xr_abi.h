#ifndef XR_ABI_H
#define XR_ABI_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

#define XR_ABI_VERSION 6 /* v4: XrCpu.ret_lr/ret_sp; v5: CRO modules (xr_place_module & co.); v6: XrGameInfo.hooks_hash */

#ifdef XR_BUILDING_GAME
#define XR_EXPORT __declspec(dllexport)
#else
#define XR_EXPORT
#endif

typedef struct XrCpu {
    uint32_t r[16];
    uint32_t cpsr;
    union { float s[32]; double d[16]; uint32_t u[32]; } v;
    uint32_t fpscr;
    uint32_t excl_addr;
    uint32_t excl_valid;
    void* thread;
    uint32_t tls;       /* guest TLS pointer, read by `mrc p15,0,rX,c13,c0,3` (ABI 2) */
    int32_t budget;     /* decremented at every loop back-edge; at <= 0 generated code calls preempt (ABI 3) */
    /* The guest return address and sp of the frame being entered (ABI 3). Native code reads them at entry as the
       target and sp its returns must match; a call sets them to its own return address and sp, a tail call passes
       its own on. Whoever else enters native code (runtime thread start, interp calling a native function) must
       set them first, normally to r[14] and r[13] at that moment. */
    uint32_t ret_lr;
    uint32_t ret_sp;
} XrCpu;

typedef void (*XrFn)(XrCpu*);

typedef struct XrRuntime {
    uint32_t abi_version;
    uint8_t* mem;
    /* r[0..14] and VFP are in c; r[15] holds the guest PC of the svc instruction (no Thumb bit). c->cpsr Q/GE are
       current, but NZCV and T are not: NZCV live in locals of the generated code (no SVC reads them) and T is
       never tracked. Generated code reloads r[0..14] after the call. */
    void (*svc)(XrCpu*, uint32_t imm);
    /* Run guest code from `start` (odd = Thumb) in the interpreter until the guest reaches pc == until_pc with
       sp == until_sp, then return. All guest state (r[0..14], NZCV/Q/GE in cpsr, VFP) goes in and out through c.
       Call-shaped misses pass (target, return address, sp) and continue natively after it returns; tail-shaped
       misses pass the frame's ret_lr/ret_sp and return. A checked-return mismatch (the guest returns somewhere
       other than ret_lr: longjmp, EHABI unwinding) is tail-shaped too, but there only the flags callers read
       after this function (the FlagTable return mask) are valid in c->cpsr; the others are stale. ret_lr/ret_sp
       hold the frame's values on entry. (ABI 3; replaces `miss`) */
    void (*interp)(XrCpu* c, uint32_t start, uint32_t until_pc, uint32_t until_sp);
    /* Called at a loop back-edge when c->budget drops to <= 0; must refill c->budget. May switch guest threads:
       r[0..14] and the live NZCV are in c across the call and reloaded after it. (ABI 3) */
    void (*preempt)(XrCpu* c);
    XrFn* hooks;  /* may be NULL; indexed by position in the hook list (GenOptions::hooks, hooks.txt order) */
} XrRuntime;

/* What xgame.dll was compiled from; the runtime refuses to run it against a dump whose code.bin differs. */
typedef struct XrGameInfo {
    uint8_t code_sha256[32];  /* SHA-256 of the code.bin xr-recomp compiled */
    uint64_t title_id;        /* the exheader's program id */
    const char* build;        /* build id, e.g. "x-usa-v0.2.0" */
    uint64_t hooks_hash;      /* FNV-1a 64 of the hook list's addresses (u32 LE, hooks.txt order); ABI 6 */
} XrGameInfo;

/* exported by xgame.dll */
XR_EXPORT uint32_t xr_game_init(const XrRuntime* rt);  /* returns XR_ABI_VERSION */
/* The native function starting at guest_addr (odd = Thumb), in code.bin or in a placed module; NULL if none. */
XR_EXPORT XrFn     xr_lookup(uint32_t guest_addr);
XR_EXPORT extern const XrGameInfo xr_game_info;
/* CRO modules compiled into the DLL (ABI 5): module k is named xr_module_names[k] (the CRO header's name, which
   ldr:ro reports), and xr_module_hash[k] is xr-hle's `cro::code_hash` (FNV-1a 64) of its code segment as in the
   file, which the runtime compares with the module the game loads before placing it. */
XR_EXPORT extern const uint32_t xr_module_count;
XR_EXPORT extern const char* const xr_module_names[];
XR_EXPORT extern const uint64_t xr_module_hash[];
/* Module k's code segment is now at guest address code_base: xr_lookup finds its functions there, and its code
   reads code_base as xr_mod_base[k]. Returns 1, or 0 (refused) when k is out of range or code_base is not
   4-aligned. Placing over another placed module's range unplaces that one. Called by the one thread holding the
   runtime's baton, like all generated code. */
XR_EXPORT uint32_t xr_place_module(uint32_t k, uint32_t code_base);
/* Module k is unloaded: xr_lookup no longer finds its functions (frames still running it keep their base).
   Unsupported, both because there is one xr_mod_base[k] per module: moving a module (unplace, place elsewhere) while
   frames still run its code, which then read the new base; and loading the same CRO twice at once (two live
   copies): the second place unplaces the first, whose native frames then use the second copy's base and whose
   later calls run interpreted. Unplace and place again at the same base is fine. */
XR_EXPORT void     xr_unplace_module(uint32_t k);

/* ---- Support for generated code (xr-codegen). Build with -O2 -fno-strict-aliasing -fwrapv. ---- */

/* Set by xr_game_init. */
extern uint8_t* xr_mem;
extern const XrRuntime* xr_rt;
/* Module-mode code (CRO code segments, xr-codegen `GenOptions::module`): xr_mod_base[k] is the guest address of
   module k's code segment (offset 0) while it is placed. Defined (one per module) and set by xr_place_module inside
   xgame.dll; the runtime never reads it. */
extern uint32_t xr_mod_base[];

/* Tail calls between generated functions; clang turns them into jumps (no host stack growth). */
#if defined(__has_attribute)
#if __has_attribute(musttail)
#define XR_TAIL __attribute__((musttail))
#endif
#endif
#ifndef XR_TAIL
#define XR_TAIL
#endif

/* Guest memory is host m + a (guest and host are both little-endian). __builtin_memcpy makes each helper
   one host load or store at any alignment (ARM11 with unaligned access enabled). */
static inline uint32_t xr_ld8(const uint8_t* m, uint32_t a) { return m[a]; }
static inline uint32_t xr_lds8(const uint8_t* m, uint32_t a) { return (uint32_t)(int32_t)(int8_t)m[a]; }
static inline uint32_t xr_ld16(const uint8_t* m, uint32_t a) { uint16_t v; __builtin_memcpy(&v, m + a, 2); return v; }
static inline uint32_t xr_lds16(const uint8_t* m, uint32_t a) { int16_t v; __builtin_memcpy(&v, m + a, 2); return (uint32_t)(int32_t)v; }
static inline uint32_t xr_ld32(const uint8_t* m, uint32_t a) { uint32_t v; __builtin_memcpy(&v, m + a, 4); return v; }
static inline uint64_t xr_ld64(const uint8_t* m, uint32_t a) { uint64_t v; __builtin_memcpy(&v, m + a, 8); return v; }
static inline void xr_st8(uint8_t* m, uint32_t a, uint32_t v) { m[a] = (uint8_t)v; }
static inline void xr_st16(uint8_t* m, uint32_t a, uint32_t v) { uint16_t t = (uint16_t)v; __builtin_memcpy(m + a, &t, 2); }
static inline void xr_st32(uint8_t* m, uint32_t a, uint32_t v) { __builtin_memcpy(m + a, &v, 4); }
static inline void xr_st64(uint8_t* m, uint32_t a, uint64_t v) { __builtin_memcpy(m + a, &v, 8); }

/* Guest r0..r14 live in locals of each generated function; these move them to and from the XrCpu. */
#define XR_REGS uint32_t r0 = c->r[0], r1 = c->r[1], r2 = c->r[2], r3 = c->r[3], r4 = c->r[4], r5 = c->r[5], \
    r6 = c->r[6], r7 = c->r[7], r8 = c->r[8], r9 = c->r[9], r10 = c->r[10], r11 = c->r[11], r12 = c->r[12], \
    r13 = c->r[13], r14 = c->r[14]
#define XR_SPILL() (c->r[0] = r0, c->r[1] = r1, c->r[2] = r2, c->r[3] = r3, c->r[4] = r4, c->r[5] = r5, \
    c->r[6] = r6, c->r[7] = r7, c->r[8] = r8, c->r[9] = r9, c->r[10] = r10, c->r[11] = r11, c->r[12] = r12, \
    c->r[13] = r13, c->r[14] = r14)
#define XR_RELOAD() (r0 = c->r[0], r1 = c->r[1], r2 = c->r[2], r3 = c->r[3], r4 = c->r[4], r5 = c->r[5], \
    r6 = c->r[6], r7 = c->r[7], r8 = c->r[8], r9 = c->r[9], r10 = c->r[10], r11 = c->r[11], r12 = c->r[12], \
    r13 = c->r[13], r14 = c->r[14])

#define XR_Q 0x08000000u /* CPSR.Q; GE[3:0] are CPSR bits 19:16. Both live in c->cpsr, NZCV in locals. */

/* Register-specified shifts, s = Rs[7:0]; k: 0 LSL, 1 LSR, 2 ASR, 3 ROR. _c gives the shifter carry-out. */
static inline uint32_t xr_shift(uint32_t k, uint32_t v, uint32_t s) {
    switch (k) {
    case 0: return s < 32 ? v << s : 0;
    case 1: return s < 32 ? v >> s : 0;
    case 2: return (uint32_t)((int32_t)v >> (s < 32 ? s : 31));
    default: return (v >> (s & 31)) | (v << ((32 - (s & 31)) & 31));
    }
}
static inline uint32_t xr_shift_c(uint32_t k, uint32_t v, uint32_t s, uint32_t cin) {
    if (s == 0) return cin;
    switch (k) {
    case 0: return s < 32 ? (v >> (32 - s)) & 1 : s == 32 ? v & 1 : 0;
    case 1: return s < 32 ? (v >> (s - 1)) & 1 : s == 32 ? v >> 31 : 0;
    case 2: return s < 32 ? (v >> (s - 1)) & 1 : v >> 31;
    default: return (v >> ((s - 1) & 31)) & 1;
    }
}

/* Saturate to a signed `bits`-bit (1..32) or unsigned `bits`-bit (0..31) range, setting Q when clipped. */
static inline uint32_t xr_ssat(int64_t v, uint32_t bits, uint32_t* cpsr) {
    int64_t hi = ((int64_t)1 << (bits - 1)) - 1, lo = -hi - 1;
    if (v > hi) { *cpsr |= XR_Q; return (uint32_t)hi; }
    if (v < lo) { *cpsr |= XR_Q; return (uint32_t)lo; }
    return (uint32_t)v;
}
static inline uint32_t xr_usat(int64_t v, uint32_t bits, uint32_t* cpsr) {
    int64_t hi = ((int64_t)1 << bits) - 1;
    if (v > hi) { *cpsr |= XR_Q; return (uint32_t)hi; }
    if (v < 0) { *cpsr |= XR_Q; return 0; }
    return (uint32_t)v;
}
static inline uint32_t xr_ssat16(uint32_t v, uint32_t bits, uint32_t* cpsr) {
    return (xr_ssat((int16_t)v, bits, cpsr) & 0xFFFF) | xr_ssat((int16_t)(v >> 16), bits, cpsr) << 16;
}
static inline uint32_t xr_usat16(uint32_t v, uint32_t bits, uint32_t* cpsr) {
    return xr_usat((int16_t)v, bits, cpsr) | xr_usat((int16_t)(v >> 16), bits, cpsr) << 16;
}

static inline uint32_t xr_sel(uint32_t a, uint32_t b, uint32_t cpsr) {
    uint32_t r = 0;
    for (int i = 0; i < 4; i++) r |= ((cpsr >> (16 + i)) & 1 ? a : b) & (0xFFu << (8 * i));
    return r;
}
static inline uint32_t xr_usad8(uint32_t a, uint32_t b) {
    uint32_t s = 0;
    for (int i = 0; i < 4; i++) {
        int32_t d = (int32_t)((a >> (8 * i)) & 0xFF) - (int32_t)((b >> (8 * i)) & 0xFF);
        s += (uint32_t)(d < 0 ? -d : d);
    }
    return s;
}

/* One lane of a parallel add/sub. kind: 0 S, 1 Q, 2 SH, 3 U, 4 UQ, 5 UH; r is the exact lane result. */
static inline uint32_t xr_par_lane(uint32_t kind, int sub, int32_t r, int w, uint32_t* ge) {
    int32_t smax = (1 << (w - 1)) - 1, smin = -(1 << (w - 1)), umax = (1 << w) - 1;
    switch (kind) {
    case 0: *ge = r >= 0; return (uint32_t)r;
    case 1: return (uint32_t)(r > smax ? smax : r < smin ? smin : r);
    case 3: *ge = sub ? r >= 0 : r > umax; return (uint32_t)r;
    case 4: return (uint32_t)(r > umax ? umax : r < 0 ? 0 : r);
    default: return (uint32_t)(r >> 1); /* SH, UH: halving */
    }
}
/* Parallel add/sub (ARMv6 SIMD). op: 0 ADD16, 1 ASX, 2 SAX, 3 SUB16, 4 ADD8, 5 SUB8. S and U set GE. */
static inline uint32_t xr_par(uint32_t kind, uint32_t op, uint32_t a, uint32_t b, uint32_t* cpsr) {
    uint32_t res = 0, ge = 0, g = 0;
    int sgn = kind < 3;
    if (op >= 4) {
        for (int i = 0; i < 4; i++) {
            int32_t x = sgn ? (int8_t)(a >> (8 * i)) : (int32_t)((a >> (8 * i)) & 0xFF);
            int32_t y = sgn ? (int8_t)(b >> (8 * i)) : (int32_t)((b >> (8 * i)) & 0xFF);
            int sub = op == 5;
            res |= (xr_par_lane(kind, sub, sub ? x - y : x + y, 8, &g) & 0xFF) << (8 * i);
            ge |= g << i;
        }
    } else {
        for (int i = 0; i < 2; i++) {
            int j = (op == 1 || op == 2) ? 1 - i : i; /* ASX/SAX cross the halves of b */
            int sub = op == 3 || (op == 1 && i == 0) || (op == 2 && i == 1);
            int32_t x = sgn ? (int16_t)(a >> (16 * i)) : (int32_t)((a >> (16 * i)) & 0xFFFF);
            int32_t y = sgn ? (int16_t)(b >> (16 * j)) : (int32_t)((b >> (16 * j)) & 0xFFFF);
            res |= (xr_par_lane(kind, sub, sub ? x - y : x + y, 16, &g) & 0xFFFF) << (16 * i);
            ge |= g ? 3u << (2 * i) : 0;
        }
    }
    if (kind == 0 || kind == 3) *cpsr = (*cpsr & ~0x000F0000u) | ge << 16;
    return res;
}

/* ---- VFPv2 in the game's FPSCR mode 0x03000000: flush-to-zero (FZ), default NaN (DN), round to nearest (see
   xr-codegen src/vfp.rs). Values are bit patterns. Needs the default MXCSR (round to nearest, no FTZ/DAZ) and
   no fast-math. */
#pragma STDC FP_CONTRACT OFF
static inline float xr_f(uint32_t u) { float f; __builtin_memcpy(&f, &u, 4); return f; }
static inline uint32_t xr_fu(float f) { uint32_t u; __builtin_memcpy(&u, &f, 4); return u; }
static inline double xr_d(uint64_t u) { double d; __builtin_memcpy(&d, &u, 8); return d; }
static inline uint64_t xr_du(double d) { uint64_t u; __builtin_memcpy(&u, &d, 8); return u; }
/* D[i] = S[2i+1]:S[2i]. */
static inline uint64_t xr_vd(const XrCpu* c, uint32_t i) { return (uint64_t)c->v.u[2 * i + 1] << 32 | c->v.u[2 * i]; }
static inline void xr_vd_set(XrCpu* c, uint32_t i, uint64_t v) { c->v.u[2 * i] = (uint32_t)v; c->v.u[2 * i + 1] = (uint32_t)(v >> 32); }
/* Operand (FZ): a denormal reads as a zero of the same sign. f32 widens to double exactly. */
static inline double xr_in32(uint32_t u) { if (!(u & 0x7F800000u)) u &= 0x80000000u; return (double)xr_f(u); }
static inline double xr_in64(uint64_t u) { if (!(u & 0x7FF0000000000000ull)) u &= 0x8000000000000000ull; return xr_d(u); }
/* Result (DN, FZ): NaN -> default NaN; |x| below the smallest normal -> zero of the same sign. For f32, x is the
   exact result whenever it is that small (or a correctly rounded double otherwise), so the test sees the value
   before rounding, and the final (float) rounding is correct (53 >= 2*24+2 bits: innocuous double rounding). */
static inline uint32_t xr_out32(double x) {
    if (x != x) return 0x7FC00000u;
    if (__builtin_fabs(x) < 0x1p-126) return (uint32_t)(xr_du(x) >> 32) & 0x80000000u;
    return xr_fu((float)x);
}
static inline uint64_t xr_out64(double x) {
    if (x != x) return 0x7FF8000000000000ull;
    if (__builtin_fabs(x) < 0x1p-1022) return xr_du(x) & 0x8000000000000000ull;
    return xr_du(x);
}
/* op: 0 add, 1 sub, 2 mul, 3 div, 4 sqrt(a). */
static inline uint32_t xr_fop32(int op, uint32_t a, uint32_t b) {
    double x = xr_in32(a), y = xr_in32(b);
    return xr_out32(op == 0 ? x + y : op == 1 ? x - y : op == 2 ? x * y : op == 3 ? x / y : __builtin_sqrt(x));
}
static inline uint64_t xr_fop64(int op, uint64_t a, uint64_t b) {
    double x = xr_in64(a), y = xr_in64(b);
    double r = op == 0 ? x + y : op == 1 ? x - y : op == 2 ? x * y : op == 3 ? x / y : __builtin_sqrt(x);
    /* Only a product can round up to the smallest normal from below (sums that small are exact; quotients and
       roots cannot get that close): it was tiny before rounding, so FZ flushes it. fma gives the exact sign. */
    if (op == 2 && __builtin_fabs(r) == 0x1p-1022
        && xr_du(__builtin_fma(__builtin_fabs(x), __builtin_fabs(y), -0x1p-1022)) >> 63)
        return xr_du(r) & 0x8000000000000000ull;
    return xr_out64(r);
}
/* VCMP: FPSCR NZCV nibble (unordered 0011, equal 0110, less 1000, greater 0010). */
static inline uint32_t xr_fcmp(double x, double y) { return x != x || y != y ? 3u : x == y ? 6u : x < y ? 8u : 2u; }
/* Float to 32-bit integer, saturating like ARM (NaN -> 0). rm: 0 nearest-even, 1 +inf, 2 -inf, 3 zero. */
static inline uint32_t xr_ftoi(double x, int sgn, uint32_t rm) {
    if (x != x) return 0;
    if (x > 0x1p40) x = 0x1p40;
    if (x < -0x1p40) x = -0x1p40;
    int64_t t = (int64_t)x;
    double f = x - (double)t; /* exact */
    switch (rm & 3) {
    case 0: if (f > 0.5 || (f == 0.5 && (t & 1))) t++; else if (f < -0.5 || (f == -0.5 && (t & 1))) t--; break;
    case 1: if (f > 0) t++; break;
    case 2: if (f < 0) t--; break;
    default: break;
    }
    if (sgn) return t > INT32_MAX ? 0x7FFFFFFFu : t < INT32_MIN ? 0x80000000u : (uint32_t)t;
    return t > (int64_t)0xFFFFFFFF ? 0xFFFFFFFFu : t < 0 ? 0u : (uint32_t)t;
}

#ifdef __cplusplus
}
#endif
#endif
