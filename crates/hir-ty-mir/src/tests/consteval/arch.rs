//! The intrinsics of LLVM that `core::arch` calls for AES, CRC-32, carry-less multiplication and
//! SHA, checked against the published values the instructions give at run time: the check
//! values of CRC-32 and CRC-32C, FIPS-197's AES-128 example and the digests of "abc".

use super::*;

#[test]
fn crc32() {
    check_number(
        r#"
        //- minicore: index, slice
        extern "C" {
            #[link_name = "llvm.x86.sse42.crc32.32.8"]
            fn crc32_32_8(crc: u32, v: u8) -> u32;
            #[link_name = "llvm.x86.sse42.crc32.32.32"]
            fn crc32_32_32(crc: u32, v: u32) -> u32;
            #[link_name = "llvm.x86.sse42.crc32.64.64"]
            fn crc32_64_64(crc: u64, v: u64) -> u64;
            #[link_name = "llvm.aarch64.crc32b"]
            fn crc32b(crc: u32, data: u32) -> u32;
            #[link_name = "llvm.aarch64.crc32cb"]
            fn crc32cb(crc: u32, data: u32) -> u32;
            #[link_name = "llvm.aarch64.crc32x"]
            fn crc32x(crc: u32, data: u64) -> u32;
            #[link_name = "llvm.aarch64.crc32cw"]
            fn crc32cw(crc: u32, data: u32) -> u32;
        }
        const DATA: &[u8; 9] = b"123456789";
        const GOAL: u32 = unsafe {
            let (mut c, mut b, mut cb) = (!0u32, !0u32, !0u32);
            let mut i = 0;
            while i < 9 {
                c = crc32_32_8(c, DATA[i]);
                b = crc32b(b, DATA[i] as u32);
                cb = crc32cb(cb, DATA[i] as u32);
                i += 1;
            }
            let (mut c4, mut b8) = (!0u32, !0u32);
            i = 0;
            while i < 8 {
                if i < 4 {
                    c4 = crc32_32_8(c4, DATA[i]);
                }
                b8 = crc32b(b8, DATA[i] as u32);
                i += 1;
            }
            let mut c8 = !0u32;
            i = 0;
            while i < 8 {
                c8 = crc32_32_8(c8, DATA[i]);
                i += 1;
            }
            let mut wrong = 0;
            if !c != 0xe306_9283 { wrong |= 1; }
            if !b != 0xcbf4_3926 { wrong |= 2; }
            if !cb != 0xe306_9283 { wrong |= 4; }
            if crc32_32_32(!0, 0x3433_3231) != c4 { wrong |= 8; }
            if crc32cw(!0, 0x3433_3231) != c4 { wrong |= 16; }
            if crc32_64_64(!0, 0x3837_3635_3433_3231) != c8 as u64 { wrong |= 32; }
            if crc32x(!0, 0x3837_3635_3433_3231) != b8 { wrong |= 64; }
            wrong
        };
        "#,
        0,
    );
}

#[test]
fn clmul() {
    check_number(
        r#"
        //- minicore: index, slice
        extern "C" {
            #[link_name = "llvm.x86.pclmulqdq"]
            fn pclmulqdq(a: [u64; 2], b: [u64; 2], imm8: u8) -> [u64; 2];
            #[link_name = "llvm.x86.pclmulqdq.256"]
            fn pclmulqdq_256(a: [u64; 4], b: [u64; 4], imm8: u8) -> [u64; 4];
            #[link_name = "llvm.aarch64.neon.pmull64"]
            fn pmull64(a: u64, b: u64) -> u128;
        }
        const GOAL: u32 = unsafe {
            let a = [3, 1 << 63];
            let b = [3, 2];
            let mut wrong = 0;
            let r = pclmulqdq(a, b, 0x00);
            if r[0] != 5 || r[1] != 0 { wrong |= 1; }
            let r = pclmulqdq(a, b, 0x11);
            if r[0] != 0 || r[1] != 1 { wrong |= 2; }
            let r = pclmulqdq(a, b, 0x01);
            if r[0] != 1 << 63 || r[1] != 1 { wrong |= 4; }
            let r = pclmulqdq(a, b, 0x10);
            if r[0] != 6 || r[1] != 0 { wrong |= 8; }
            let r = pclmulqdq([!0, 0], [!0, 0], 0);
            if r[0] != 0x5555_5555_5555_5555 || r[1] != 0x5555_5555_5555_5555 { wrong |= 16; }
            let r = pclmulqdq_256([3, 0, 0, 1 << 63], [3, 0, 3, 0], 0x01);
            if r[0] != 0 || r[1] != 0 || r[2] != 1 << 63 || r[3] != 1 { wrong |= 32; }
            if pmull64(1 << 63, 3) != (1 << 63) | (1 << 64) { wrong |= 64; }
            wrong
        };
        "#,
        0,
    );
}

#[test]
fn pshufb() {
    check_number(
        r#"
        //- minicore: index, slice
        extern "C" {
            #[link_name = "llvm.x86.ssse3.pshuf.b.128"]
            fn pshufb(a: [u8; 16], b: [u8; 16]) -> [u8; 16];
        }
        const GOAL: u32 = unsafe {
            let a = [10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25];
            let r = pshufb(a, [0x80, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0x1f]);
            let want = [0, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 25];
            let mut wrong = 0;
            let mut i = 0;
            while i < 16 {
                if r[i] != want[i] { wrong |= 1 << i; }
                i += 1;
            }
            wrong
        };
        "#,
        0,
    );
}

#[test]
fn aes() {
    check_number(
        r#"
        //- minicore: index, slice
        extern "C" {
            #[link_name = "llvm.x86.aesni.aesenc"]
            fn aesenc(a: [u8; 16], k: [u8; 16]) -> [u8; 16];
            #[link_name = "llvm.x86.aesni.aesenclast"]
            fn aesenclast(a: [u8; 16], k: [u8; 16]) -> [u8; 16];
            #[link_name = "llvm.x86.aesni.aesdec"]
            fn aesdec(a: [u8; 16], k: [u8; 16]) -> [u8; 16];
            #[link_name = "llvm.x86.aesni.aesdeclast"]
            fn aesdeclast(a: [u8; 16], k: [u8; 16]) -> [u8; 16];
            #[link_name = "llvm.x86.aesni.aesimc"]
            fn aesimc(a: [u8; 16]) -> [u8; 16];
            #[link_name = "llvm.x86.aesni.aeskeygenassist"]
            fn aeskeygenassist(a: [u8; 16], imm8: u8) -> [u8; 16];
            #[link_name = "llvm.x86.aesni.aesenc.256"]
            fn aesenc_256(a: [u8; 32], k: [u8; 32]) -> [u8; 32];
            #[link_name = "llvm.aarch64.crypto.aese"]
            fn aese(data: [u8; 16], key: [u8; 16]) -> [u8; 16];
            #[link_name = "llvm.aarch64.crypto.aesd"]
            fn aesd(data: [u8; 16], key: [u8; 16]) -> [u8; 16];
            #[link_name = "llvm.aarch64.crypto.aesmc"]
            fn aesmc(data: [u8; 16]) -> [u8; 16];
            #[link_name = "llvm.aarch64.crypto.aesimc"]
            fn aesimc_arm(data: [u8; 16]) -> [u8; 16];
        }
        const fn xor(a: [u8; 16], b: [u8; 16]) -> [u8; 16] {
            let mut r = [0; 16];
            let mut i = 0;
            while i < 16 {
                r[i] = a[i] ^ b[i];
                i += 1;
            }
            r
        }
        const fn same(a: [u8; 16], b: [u8; 16]) -> bool {
            let mut i = 0;
            while i < 16 {
                if a[i] != b[i] {
                    return false;
                }
                i += 1;
            }
            true
        }
        const GOAL: u32 = unsafe {
            let key = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
            let pt = [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                0xdd, 0xee, 0xff,
            ];
            let ct = [
                0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70,
                0xb4, 0xc5, 0x5a,
            ];
            let rcon = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];
            let mut rk = [[0u8; 16]; 11];
            rk[0] = key;
            let mut r = 1;
            while r < 11 {
                let t = aeskeygenassist(rk[r - 1], rcon[r - 1]);
                let k = rk[r - 1];
                let mut n = [0u8; 16];
                let mut j = 0;
                while j < 4 {
                    n[j] = k[j] ^ t[12 + j];
                    n[4 + j] = k[4 + j] ^ n[j];
                    n[8 + j] = k[8 + j] ^ n[4 + j];
                    n[12 + j] = k[12 + j] ^ n[8 + j];
                    j += 1;
                }
                rk[r] = n;
                r += 1;
            }
            let mut wrong = 0;

            let mut s = xor(pt, rk[0]);
            r = 1;
            while r < 10 {
                s = aesenc(s, rk[r]);
                r += 1;
            }
            if !same(aesenclast(s, rk[10]), ct) { wrong |= 1; }

            let mut s = xor(ct, rk[10]);
            r = 9;
            while r > 0 {
                s = aesdec(s, aesimc(rk[r]));
                r -= 1;
            }
            if !same(aesdeclast(s, rk[0]), pt) { wrong |= 2; }

            let mut s = pt;
            r = 0;
            while r < 9 {
                s = aesmc(aese(s, rk[r]));
                r += 1;
            }
            if !same(xor(aese(s, rk[9]), rk[10]), ct) { wrong |= 4; }

            let mut s = aesd(ct, rk[10]);
            r = 9;
            while r > 0 {
                s = aesd(aesimc_arm(s), aesimc_arm(rk[r]));
                r -= 1;
            }
            if !same(xor(s, rk[0]), pt) { wrong |= 8; }

            let mut two = [0u8; 32];
            let mut keys = [0u8; 32];
            let mut j = 0;
            while j < 16 {
                two[j] = pt[j];
                two[16 + j] = ct[j];
                keys[j] = rk[1][j];
                keys[16 + j] = rk[2][j];
                j += 1;
            }
            let both = aesenc_256(two, keys);
            let (lo, hi) = (aesenc(pt, rk[1]), aesenc(ct, rk[2]));
            j = 0;
            while j < 16 {
                if both[j] != lo[j] || both[16 + j] != hi[j] { wrong |= 16; }
                j += 1;
            }
            wrong
        };
        "#,
        0,
    );
}

const SHA256: &str = r#"
        #[rustc_intrinsic]
        pub const fn wrapping_add<T>(a: T, b: T) -> T;
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        const H: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        // SHA-256 of "abc".
        const DIGEST: [u32; 8] = [
            0xba7816bf, 0x8f01cfea, 0x414140de, 0x5dae2223, 0xb00361a3, 0x96177a9c, 0xb410ff61,
            0xf20015ad,
        ];
        // The one block of "abc".
        const fn block() -> [u32; 64] {
            let mut w = [0u32; 64];
            w[0] = 0x61626380;
            w[15] = 0x18;
            w
        }
        const fn quad(w: &[u32; 64], i: usize) -> [u32; 4] {
            [w[i], w[i + 1], w[i + 2], w[i + 3]]
        }
        const fn add(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
            [
                wrapping_add(a[0], b[0]),
                wrapping_add(a[1], b[1]),
                wrapping_add(a[2], b[2]),
                wrapping_add(a[3], b[3]),
            ]
        }
        const fn wrong(out: [u32; 8]) -> u32 {
            let mut wrong = 0;
            let mut i = 0;
            while i < 8 {
                if wrapping_add(out[i], H[i]) != DIGEST[i] {
                    wrong |= 1 << i;
                }
                i += 1;
            }
            wrong
        }
"#;

#[test]
fn sha256_x86() {
    check_number(
        &format!(
            r#"
        //- minicore: index, slice
        {SHA256}
        extern "C" {{
            #[link_name = "llvm.x86.sha256rnds2"]
            fn rnds2(a: [u32; 4], b: [u32; 4], k: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.x86.sha256msg1"]
            fn msg1(a: [u32; 4], b: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.x86.sha256msg2"]
            fn msg2(a: [u32; 4], b: [u32; 4]) -> [u32; 4];
        }}
        const GOAL: u32 = unsafe {{
            let mut w = block();
            let mut i = 16;
            while i < 64 {{
                let x = add(msg1(quad(&w, i - 16), quad(&w, i - 12)), quad(&w, i - 7));
                let n = msg2(x, quad(&w, i - 4));
                w[i] = n[0];
                w[i + 1] = n[1];
                w[i + 2] = n[2];
                w[i + 3] = n[3];
                i += 4;
            }}
            let mut abef = [H[5], H[4], H[1], H[0]];
            let mut cdgh = [H[7], H[6], H[3], H[2]];
            i = 0;
            while i < 64 {{
                let wk = add(quad(&w, i), quad(&K, i));
                let next = rnds2(cdgh, abef, wk);
                cdgh = abef;
                abef = next;
                let next = rnds2(cdgh, abef, [wk[2], wk[3], 0, 0]);
                cdgh = abef;
                abef = next;
                i += 4;
            }}
            wrong([abef[3], abef[2], cdgh[3], cdgh[2], abef[1], abef[0], cdgh[1], cdgh[0]])
        }};
        "#
        ),
        0,
    );
}

#[test]
fn sha256_aarch64() {
    check_number(
        &format!(
            r#"
        //- minicore: index, slice
        {SHA256}
        extern "C" {{
            #[link_name = "llvm.aarch64.crypto.sha256h"]
            fn sha256h(abcd: [u32; 4], efgh: [u32; 4], wk: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.aarch64.crypto.sha256h2"]
            fn sha256h2(efgh: [u32; 4], abcd: [u32; 4], wk: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.aarch64.crypto.sha256su0"]
            fn sha256su0(w0_3: [u32; 4], w4_7: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.aarch64.crypto.sha256su1"]
            fn sha256su1(tw0_3: [u32; 4], w8_11: [u32; 4], w12_15: [u32; 4]) -> [u32; 4];
        }}
        const GOAL: u32 = unsafe {{
            let mut w = block();
            let mut i = 16;
            while i < 64 {{
                let x = sha256su0(quad(&w, i - 16), quad(&w, i - 12));
                let n = sha256su1(x, quad(&w, i - 8), quad(&w, i - 4));
                w[i] = n[0];
                w[i + 1] = n[1];
                w[i + 2] = n[2];
                w[i + 3] = n[3];
                i += 4;
            }}
            let mut abcd = [H[0], H[1], H[2], H[3]];
            let mut efgh = [H[4], H[5], H[6], H[7]];
            i = 0;
            while i < 64 {{
                let wk = add(quad(&w, i), quad(&K, i));
                let next = sha256h(abcd, efgh, wk);
                efgh = sha256h2(efgh, abcd, wk);
                abcd = next;
                i += 4;
            }}
            wrong([abcd[0], abcd[1], abcd[2], abcd[3], efgh[0], efgh[1], efgh[2], efgh[3]])
        }};
        "#
        ),
        0,
    );
}

const SHA1: &str = r#"
        #[rustc_intrinsic]
        pub const fn wrapping_add<T>(a: T, b: T) -> T;
        const H: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
        // SHA-1 of "abc".
        const DIGEST: [u32; 5] = [0xa9993e36, 0x4706816a, 0xba3e2571, 0x7850c26c, 0x9cd0d89d];
        const fn block() -> [u32; 80] {
            let mut w = [0u32; 80];
            w[0] = 0x61626380;
            w[15] = 0x18;
            w
        }
        const fn wrong(out: [u32; 5]) -> u32 {
            let mut wrong = 0;
            let mut i = 0;
            while i < 5 {
                if wrapping_add(out[i], H[i]) != DIGEST[i] {
                    wrong |= 1 << i;
                }
                i += 1;
            }
            wrong
        }
"#;

#[test]
fn sha1_x86() {
    check_number(
        &format!(
            r#"
        //- minicore: index, slice
        {SHA1}
        extern "C" {{
            #[link_name = "llvm.x86.sha1rnds4"]
            fn sha1rnds4(a: [u32; 4], b: [u32; 4], func: i8) -> [u32; 4];
            #[link_name = "llvm.x86.sha1nexte"]
            fn sha1nexte(a: [u32; 4], b: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.x86.sha1msg1"]
            fn sha1msg1(a: [u32; 4], b: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.x86.sha1msg2"]
            fn sha1msg2(a: [u32; 4], b: [u32; 4]) -> [u32; 4];
        }}
        // Words `t` to `t + 3`, `t` in the highest lane, as `_mm_shuffle_epi8` loads them.
        const fn rev(w: &[u32; 80], t: usize) -> [u32; 4] {{
            [w[t + 3], w[t + 2], w[t + 1], w[t]]
        }}
        const fn xor(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {{
            [a[0] ^ b[0], a[1] ^ b[1], a[2] ^ b[2], a[3] ^ b[3]]
        }}
        const GOAL: u32 = unsafe {{
            let mut w = block();
            let mut t = 16;
            while t < 80 {{
                let x = xor(sha1msg1(rev(&w, t - 16), rev(&w, t - 12)), rev(&w, t - 8));
                let n = sha1msg2(x, rev(&w, t - 4));
                w[t] = n[3];
                w[t + 1] = n[2];
                w[t + 2] = n[1];
                w[t + 3] = n[0];
                t += 4;
            }}
            let mut abcd = [H[3], H[2], H[1], H[0]];
            let mut prev = abcd;
            let mut g = 0;
            while g < 20 {{
                let msg = rev(&w, 4 * g);
                let src = if g == 0 {{
                    [msg[0], msg[1], msg[2], wrapping_add(msg[3], H[4])]
                }} else {{
                    sha1nexte(prev, msg)
                }};
                prev = abcd;
                abcd = sha1rnds4(abcd, src, (g / 5) as i8);
                g += 1;
            }}
            let e = sha1nexte(prev, [0; 4])[3];
            wrong([abcd[3], abcd[2], abcd[1], abcd[0], e])
        }};
        "#
        ),
        0,
    );
}

#[test]
fn sha1_aarch64() {
    check_number(
        &format!(
            r#"
        //- minicore: index, slice
        {SHA1}
        extern "C" {{
            #[link_name = "llvm.aarch64.crypto.sha1c"]
            fn sha1c(abcd: [u32; 4], e: u32, wk: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.aarch64.crypto.sha1p"]
            fn sha1p(abcd: [u32; 4], e: u32, wk: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.aarch64.crypto.sha1m"]
            fn sha1m(abcd: [u32; 4], e: u32, wk: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.aarch64.crypto.sha1h"]
            fn sha1h(e: u32) -> u32;
            #[link_name = "llvm.aarch64.crypto.sha1su0"]
            fn sha1su0(w0_3: [u32; 4], w4_7: [u32; 4], w8_11: [u32; 4]) -> [u32; 4];
            #[link_name = "llvm.aarch64.crypto.sha1su1"]
            fn sha1su1(tw0_3: [u32; 4], w12_15: [u32; 4]) -> [u32; 4];
        }}
        const fn quad(w: &[u32; 80], i: usize) -> [u32; 4] {{
            [w[i], w[i + 1], w[i + 2], w[i + 3]]
        }}
        const K: [u32; 4] = [0x5a827999, 0x6ed9eba1, 0x8f1bbcdc, 0xca62c1d6];
        const GOAL: u32 = unsafe {{
            let mut w = block();
            let mut t = 16;
            while t < 80 {{
                let x = sha1su0(quad(&w, t - 16), quad(&w, t - 12), quad(&w, t - 8));
                let n = sha1su1(x, quad(&w, t - 4));
                w[t] = n[0];
                w[t + 1] = n[1];
                w[t + 2] = n[2];
                w[t + 3] = n[3];
                t += 4;
            }}
            let mut abcd = [H[0], H[1], H[2], H[3]];
            let mut e = H[4];
            let mut g = 0;
            while g < 20 {{
                let k = K[g / 5];
                let q = quad(&w, 4 * g);
                let wk = [
                    wrapping_add(q[0], k),
                    wrapping_add(q[1], k),
                    wrapping_add(q[2], k),
                    wrapping_add(q[3], k),
                ];
                let next_e = sha1h(abcd[0]);
                abcd = match g / 5 {{
                    0 => sha1c(abcd, e, wk),
                    2 => sha1m(abcd, e, wk),
                    _ => sha1p(abcd, e, wk),
                }};
                e = next_e;
                g += 1;
            }}
            wrong([abcd[0], abcd[1], abcd[2], abcd[3], e])
        }};
        "#
        ),
        0,
    );
}

#[test]
fn unknown_llvm_intrinsic() {
    check_fail(
        r#"
        //- minicore: index, slice
        extern "C" {
            #[link_name = "llvm.x86.rdrand.32"]
            fn rdrand32() -> (u32, i32);
        }
        const GOAL: u32 = unsafe { rdrand32().0 };
        "#,
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::NotSupported(_))),
    );
}
