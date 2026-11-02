//! The intrinsics of LLVM that `core::arch` calls for AES, CRC-32, carry-less multiplication
//! and SHA (`llvm.x86.*`, `llvm.aarch64.*`), as the instructions compute them: their results are
//! fully defined, so a constant computes what the program does at run time on a target that has
//! them.

use super::*;

/// AES's S-box, by the generator 3 of the field's multiplicative group: `p` walks its powers
/// and `q` their inverses.
const SBOX: [u8; 256] = {
    let mut sbox = [0; 256];
    let (mut p, mut q) = (1u8, 1u8);
    loop {
        p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1b } else { 0 };
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4);
        sbox[p as usize] = x ^ 0x63;
        if p == 1 {
            break;
        }
    }
    sbox[0] = 0x63;
    sbox
};

const INV_SBOX: [u8; 256] = {
    let mut inv = [0; 256];
    let mut i = 0;
    while i < 256 {
        inv[SBOX[i] as usize] = i as u8;
        i += 1;
    }
    inv
};

/// Multiplication in AES's field, GF(2^8) modulo x^8 + x^4 + x^3 + x + 1.
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = (a << 1) ^ if a & 0x80 != 0 { 0x1b } else { 0 };
        b >>= 1;
    }
    p
}

/// An AES state: byte `r + 4 * c` is row `r` of column `c`, the order of the block's bytes and
/// of a vector register's.
type State = [u8; 16];

fn sub_bytes(s: State, sbox: &[u8; 256]) -> State {
    s.map(|it| sbox[it as usize])
}

fn shift_rows(s: State, inverse: bool) -> State {
    std::array::from_fn(|i| {
        let (r, c) = (i % 4, i / 4);
        let from = if inverse { (c + 4 - r) % 4 } else { (c + r) % 4 };
        s[r + 4 * from]
    })
}

fn mix_columns(s: State, inverse: bool) -> State {
    let m: [u8; 4] = if inverse { [14, 11, 13, 9] } else { [2, 3, 1, 1] };
    std::array::from_fn(|i| {
        let (r, c) = (i % 4, i / 4);
        (0..4).fold(0, |acc, k| acc ^ gmul(m[(k + 4 - r) % 4], s[k + 4 * c]))
    })
}

fn xor(a: State, b: State) -> State {
    std::array::from_fn(|i| a[i] ^ b[i])
}

/// CRC-32 of the reflected polynomial `poly` over the `bytes` bytes of `data`, little end
/// first, from `crc` and with no inversion before or after, as the instructions do.
fn crc32(poly: u32, mut crc: u32, data: u64, bytes: usize) -> u32 {
    for byte in data.to_le_bytes().into_iter().take(bytes) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { poly } else { 0 };
        }
    }
    crc
}

/// The carry-less product of `a` and `b`.
fn clmul(a: u64, b: u64) -> u128 {
    (0..64).filter(|i| b >> i & 1 != 0).fold(0, |acc, i| acc ^ (u128::from(a) << i))
}

fn words(bytes: &[u8]) -> [u32; 4] {
    std::array::from_fn(|i| u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap()))
}

/// The bytes of an intrinsic's argument or result, inline up to 512 bits.
type Bytes = smallvec::SmallVec<[u8; 64]>;

fn from_words(words: [u32; 4]) -> Bytes {
    let mut bytes = Bytes::new();
    for it in words {
        bytes.extend_from_slice(&it.to_le_bytes());
    }
    bytes
}

/// An integer argument of up to 8 bytes.
fn int(bytes: &[u8]) -> u64 {
    let mut it = [0; 8];
    let n = bytes.len().min(8);
    it[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(it)
}

fn ch(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (!x & z)
}

fn maj(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (x & z) ^ (y & z)
}

fn parity(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}

fn big_sigma0(x: u32) -> u32 {
    x.rotate_right(2) ^ x.rotate_right(13) ^ x.rotate_right(22)
}

fn big_sigma1(x: u32) -> u32 {
    x.rotate_right(6) ^ x.rotate_right(11) ^ x.rotate_right(25)
}

fn sigma0(x: u32) -> u32 {
    x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
}

fn sigma1(x: u32) -> u32 {
    x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
}

/// Arm's `SHA256hash`: four rounds on `x` = a, b, c, d and `y` = e, f, g, h (element 0 first)
/// with the words and constants `w`; the new a to d, or e to h.
fn arm_sha256_hash(mut x: [u32; 4], mut y: [u32; 4], w: [u32; 4], abcd: bool) -> [u32; 4] {
    for w in w {
        let t =
            y[3].wrapping_add(big_sigma1(y[0])).wrapping_add(ch(y[0], y[1], y[2])).wrapping_add(w);
        let new_e = x[3].wrapping_add(t);
        let new_a = t.wrapping_add(big_sigma0(x[0])).wrapping_add(maj(x[0], x[1], x[2]));
        x = [new_a, x[0], x[1], x[2]];
        y = [new_e, y[0], y[1], y[2]];
    }
    if abcd { x } else { y }
}

/// Arm's `SHA1C`, `SHA1P` and `SHA1M`: four rounds on a, b, c, d (element 0 first) and e with
/// the words and constants `w`.
fn arm_sha1_hash(
    mut x: [u32; 4],
    mut e: u32,
    w: [u32; 4],
    f: fn(u32, u32, u32) -> u32,
) -> [u32; 4] {
    for w in w {
        e = e.wrapping_add(x[0].rotate_left(5)).wrapping_add(f(x[1], x[2], x[3])).wrapping_add(w);
        x[1] = x[1].rotate_left(30);
        (x, e) = ([e, x[0], x[1], x[2]], x[3]);
    }
    x
}

/// The arguments of a call of `llvm.{name}`.
struct Args<'a> {
    name: &'a str,
    args: smallvec::SmallVec<[Bytes; 3]>,
}

impl Args<'_> {
    fn arg<'db>(&self, i: usize) -> Result<'db, &[u8]> {
        match self.args.get(i) {
            Some(it) => Ok(it),
            None => Err(MirEvalError::InternalError(
                // alloc: error path
                format!("llvm.{} has no argument {i}", self.name).into(),
            )),
        }
    }

    fn state<'db>(&self, i: usize) -> Result<'db, State> {
        match self.arg(i)?.try_into() {
            Ok(it) => Ok(it),
            Err(_) => Err(MirEvalError::InternalError(
                // alloc: error path
                format!("llvm.{} takes 16 bytes", self.name).into(),
            )),
        }
    }

    /// `f` of the 128-bit lanes of the first two arguments, for the instructions that VAES
    /// and VPCLMULQDQ widen to 256 and 512 bits.
    fn lanes<'db>(&self, f: fn(State, State) -> State) -> Result<'db, Bytes> {
        let (a, b) = (self.arg(0)?, self.arg(1)?);
        let mut out = Bytes::new();
        for (a, b) in a.chunks_exact(16).zip(b.chunks_exact(16)) {
            out.extend_from_slice(&f(a.try_into().unwrap(), b.try_into().unwrap()));
        }
        Ok(out)
    }
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// `name` is the intrinsic's without `llvm.`.
    pub(super) fn exec_llvm_intrinsic(
        &mut self,
        name: &str,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
    ) -> Result<'db, ()> {
        let args = Args {
            name,
            args: args
                .iter()
                .map(|it| Ok(Bytes::from_slice(self.read_scalar(it.interval)?)))
                .collect::<Result<'db, _>>()?,
        };
        let arg = |i| args.arg(i);
        let state = |i| args.state(i);
        let lanes = |f: fn(State, State) -> State| args.lanes(f);
        let (family, width) = match name.rsplit_once('.') {
            Some((family, width @ ("256" | "512"))) => (family, Some(width)),
            _ => (name, None),
        };
        let result: Bytes = match family {
            "x86.aesni.aesenc" => {
                lanes(|s, k| xor(mix_columns(sub_bytes(shift_rows(s, false), &SBOX), false), k))?
            }
            "x86.aesni.aesenclast" => lanes(|s, k| xor(sub_bytes(shift_rows(s, false), &SBOX), k))?,
            "x86.aesni.aesdec" => {
                lanes(|s, k| xor(mix_columns(sub_bytes(shift_rows(s, true), &INV_SBOX), true), k))?
            }
            "x86.aesni.aesdeclast" => {
                lanes(|s, k| xor(sub_bytes(shift_rows(s, true), &INV_SBOX), k))?
            }
            "x86.aesni.aesimc" if width.is_none() => {
                Bytes::from_slice(&mix_columns(state(0)?, true))
            }
            "x86.aesni.aeskeygenassist" if width.is_none() => {
                let w = words(&state(0)?);
                let rcon = int(arg(1)?) as u8 as u32;
                let sub = |w: u32| u32::from_le_bytes(sub_bytes_word(w.to_le_bytes()));
                let (x1, x3) = (sub(w[1]), sub(w[3]));
                from_words([x1, x1.rotate_right(8) ^ rcon, x3, x3.rotate_right(8) ^ rcon])
            }
            "x86.pclmulqdq" => {
                let imm = int(arg(2)?);
                let (a, b) = (arg(0)?, arg(1)?);
                a.chunks(16)
                    .zip(b.chunks(16))
                    .flat_map(|(a, b)| {
                        let a = int(&a[8 * (imm & 1) as usize..]);
                        let b = int(&b[8 * (imm >> 4 & 1) as usize..]);
                        clmul(a, b).to_le_bytes()
                    })
                    .collect() // alloc: inline `Bytes`, on the stack up to 512 bits
            }
            "x86.ssse3.pshuf.b.128" | "x86.avx2.pshuf.b" | "x86.avx512.pshuf.b" => {
                let (a, b) = (arg(0)?, arg(1)?);
                a.chunks(16)
                    .zip(b.chunks(16))
                    .flat_map(|(a, b)| {
                        b.iter().map(|&i| if i & 0x80 != 0 { 0 } else { a[(i & 15) as usize] })
                    })
                    .collect() // alloc: inline `Bytes`, on the stack up to 512 bits
            }
            "x86.sse42.crc32.32.8"
            | "x86.sse42.crc32.32.16"
            | "x86.sse42.crc32.32.32"
            | "x86.sse42.crc32.64.64" => {
                let bytes = name.rsplit('.').next().unwrap().parse::<usize>().unwrap() / 8;
                let crc = crc32(0x82f6_3b78, int(arg(0)?) as u32, int(arg(1)?), bytes);
                Bytes::from_slice(&u64::from(crc).to_le_bytes())
            }
            "x86.sha1rnds4" => {
                let [d, c, b, a] = words(&state(0)?);
                let [w3, w2, w1, w0e] = words(&state(1)?);
                let (f, k): (fn(u32, u32, u32) -> u32, u32) = match int(arg(2)?) & 3 {
                    0 => (ch, 0x5a82_7999),
                    1 => (parity, 0x6ed9_eba1),
                    2 => (maj, 0x8f1b_bcdc),
                    _ => (parity, 0xca62_c1d6),
                };
                let (mut a, mut b, mut c, mut d, mut e) = (a, b, c, d, 0u32);
                for (i, w) in [w0e, w1, w2, w3].into_iter().enumerate() {
                    let e_in = if i == 0 { 0 } else { e };
                    let new_a = f(b, c, d)
                        .wrapping_add(a.rotate_left(5))
                        .wrapping_add(w)
                        .wrapping_add(e_in)
                        .wrapping_add(k);
                    (a, b, c, d, e) = (new_a, a, b.rotate_left(30), c, d);
                }
                from_words([d, c, b, a])
            }
            "x86.sha1nexte" => {
                let a = words(&state(0)?)[3].rotate_left(30);
                let mut w = words(&state(1)?);
                w[3] = w[3].wrapping_add(a);
                from_words(w)
            }
            "x86.sha1msg1" => {
                let [w3, w2, w1, w0] = words(&state(0)?);
                let [_, _, w5, w4] = words(&state(1)?);
                from_words([w5 ^ w3, w4 ^ w2, w3 ^ w1, w2 ^ w0])
            }
            "x86.sha1msg2" => {
                let [s0, s1, s2, s3] = words(&state(0)?);
                let [w15, w14, w13, _] = words(&state(1)?);
                let w16 = (s3 ^ w13).rotate_left(1);
                let w17 = (s2 ^ w14).rotate_left(1);
                let w18 = (s1 ^ w15).rotate_left(1);
                let w19 = (s0 ^ w16).rotate_left(1);
                from_words([w19, w18, w17, w16])
            }
            "x86.sha256rnds2" => {
                let [h, g, d, c] = words(&state(0)?);
                let [f, e, b, a] = words(&state(1)?);
                let [wk0, wk1, _, _] = words(&state(2)?);
                let (mut a, mut b, mut c, mut d) = (a, b, c, d);
                let (mut e, mut f, mut g, mut h) = (e, f, g, h);
                for wk in [wk0, wk1] {
                    let t1 =
                        h.wrapping_add(big_sigma1(e)).wrapping_add(ch(e, f, g)).wrapping_add(wk);
                    let t2 = big_sigma0(a).wrapping_add(maj(a, b, c));
                    (h, g, f, e) = (g, f, e, d.wrapping_add(t1));
                    (d, c, b, a) = (c, b, a, t1.wrapping_add(t2));
                }
                from_words([f, e, b, a])
            }
            "x86.sha256msg1" => {
                let w = words(&state(0)?);
                let w4 = words(&state(1)?)[0];
                let next = [w[1], w[2], w[3], w4];
                from_words(std::array::from_fn(|i| w[i].wrapping_add(sigma0(next[i]))))
            }
            "x86.sha256msg2" => {
                let s = words(&state(0)?);
                let [_, _, w14, w15] = words(&state(1)?);
                let w16 = s[0].wrapping_add(sigma1(w14));
                let w17 = s[1].wrapping_add(sigma1(w15));
                let w18 = s[2].wrapping_add(sigma1(w16));
                let w19 = s[3].wrapping_add(sigma1(w17));
                from_words([w16, w17, w18, w19])
            }
            "aarch64.crypto.aese" => {
                Bytes::from_slice(&sub_bytes(shift_rows(xor(state(0)?, state(1)?), false), &SBOX))
            }
            "aarch64.crypto.aesd" => {
                Bytes::from_slice(&sub_bytes(shift_rows(xor(state(0)?, state(1)?), true), &INV_SBOX))
            }
            "aarch64.crypto.aesmc" => Bytes::from_slice(&mix_columns(state(0)?, false)),
            "aarch64.crypto.aesimc" => Bytes::from_slice(&mix_columns(state(0)?, true)),
            "aarch64.crc32b" | "aarch64.crc32h" | "aarch64.crc32w" | "aarch64.crc32x"
            | "aarch64.crc32cb" | "aarch64.crc32ch" | "aarch64.crc32cw" | "aarch64.crc32cx" => {
                let (poly, size) = match name.strip_prefix("aarch64.crc32").unwrap() {
                    it if it.starts_with('c') => (0x82f6_3b78, &it[1..]),
                    it => (0xedb8_8320, it),
                };
                let bytes = match size {
                    "b" => 1,
                    "h" => 2,
                    "w" => 4,
                    _ => 8,
                };
                Bytes::from_slice(&crc32(poly, int(arg(0)?) as u32, int(arg(1)?), bytes).to_le_bytes())
            }
            "aarch64.neon.pmull64" => Bytes::from_slice(&clmul(int(arg(0)?), int(arg(1)?)).to_le_bytes()),
            "aarch64.crypto.sha1c" | "aarch64.crypto.sha1p" | "aarch64.crypto.sha1m" => {
                let f: fn(u32, u32, u32) -> u32 = match name {
                    "aarch64.crypto.sha1c" => ch,
                    "aarch64.crypto.sha1p" => parity,
                    _ => maj,
                };
                let e = int(arg(1)?) as u32;
                from_words(arm_sha1_hash(words(&state(0)?), e, words(&state(2)?), f))
            }
            "aarch64.crypto.sha1h" => Bytes::from_slice(&(int(arg(0)?) as u32).rotate_left(30).to_le_bytes()),
            "aarch64.crypto.sha1su0" => {
                let (x, y, z) = (words(&state(0)?), words(&state(1)?), words(&state(2)?));
                let t = [x[2], x[3], y[0], y[1]];
                from_words(std::array::from_fn(|i| t[i] ^ x[i] ^ z[i]))
            }
            "aarch64.crypto.sha1su1" => {
                let (x, y) = (words(&state(0)?), words(&state(1)?));
                let t = [x[0] ^ y[1], x[1] ^ y[2], x[2] ^ y[3], x[3]];
                let mut w: [u32; 4] = t.map(|it| it.rotate_left(1));
                w[3] ^= t[0].rotate_left(2);
                from_words(w)
            }
            "aarch64.crypto.sha256h" => {
                let (abcd, efgh) = (words(&state(0)?), words(&state(1)?));
                from_words(arm_sha256_hash(abcd, efgh, words(&state(2)?), true))
            }
            "aarch64.crypto.sha256h2" => {
                let (efgh, abcd) = (words(&state(0)?), words(&state(1)?));
                from_words(arm_sha256_hash(abcd, efgh, words(&state(2)?), false))
            }
            "aarch64.crypto.sha256su0" => {
                let (w, next) = (words(&state(0)?), words(&state(1)?));
                let t = [w[1], w[2], w[3], next[0]];
                from_words(std::array::from_fn(|i| w[i].wrapping_add(sigma0(t[i]))))
            }
            "aarch64.crypto.sha256su1" => {
                let (x, y, z) = (words(&state(0)?), words(&state(1)?), words(&state(2)?));
                let t0 = [y[1], y[2], y[3], z[0]];
                let mut w = [0u32; 4];
                for i in 0..4 {
                    let prev = if i < 2 { z[i + 2] } else { w[i - 2] };
                    w[i] = sigma1(prev).wrapping_add(x[i]).wrapping_add(t0[i]);
                }
                from_words(w)
            }
            _ => not_supported!("the LLVM intrinsic `llvm.{name}` in const evaluation"),
        };
        destination.write_from_bytes(self, &result[..destination.size])
    }
}

/// AES's `SubWord`.
fn sub_bytes_word(w: [u8; 4]) -> [u8; 4] {
    w.map(|it| SBOX[it as usize])
}
