//! The SHA-256 and SHA-512 intrinsics (target/sha2.rs): their names as the one set
//! `target::named` asks, and their build, legalized as the Reference and Fast tiers take it
//! (blaze-lir `legalize`), then run by blaze-lir's reference evaluator, one call at a time,
//! inside the `sha2` crate's aarch64 compression functions (`sha2-0.11.0`
//! `src/sha256/aarch64_sha2.rs` and `src/sha512/aarch64_sha3.rs`, transcribed with each
//! intrinsic replaced by the evaluated build). The digests of "abc" are FIPS 180-4's examples
//! (appendix B.1 of SHA-256, C.1 of SHA-512): every one of the eight builds is reached by them,
//! and a wrong rotation, lane order or operand order in any changes the digest.
//!
//! `sha256_of_abc_is_the_fips_vector` prints "SHA-256(\"abc\") = ba7816bf...";
//! `sha512_of_abc_is_the_fips_vector` prints "SHA-512(\"abc\") = ddaf35a1...".

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::eval::{Outcome, Val, run_in};
use blaze_lir::func::{BlockKind, Conv, Func, RefId, SlotKind};
use blaze_lir::legalize::{QuadRoutines, SoftFloat, legalize};
use blaze_lir::{Builder, Flags, Full, Ty};

use super::sha2::{Sha2Fn, all};
use super::{Target, named};

struct NoRoutines;

impl QuadRoutines for NoRoutines {
    fn routine(&self, _: SoftFloat) -> Option<RefId> {
        None
    }
}

/// `f` of the 16-byte operands `ops` (`Vd` first), built, legalized and evaluated: the
/// result's two 64-bit words.
fn eval(f: Sha2Fn, ops: &[[u8; 16]]) -> [u64; 2] {
    let heap = blaze_own::testing::heap();
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64), Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let mut loaded = [blaze_lir::Value(0); 3];
    for (k, bytes) in ops.iter().enumerate() {
        let slot = b.slot(16, 4, SlotKind::Temp);
        let addr = b.slot_addr(slot);
        for (i, &byte) in bytes.iter().enumerate() {
            let v = b.iconst(Ty::I8, u128::from(byte));
            b.store(Flags::NONE, v, addr, i as i32, MemAttrs::aligned(0));
        }
        loaded[k] = b.load(f.shape().ty(), Flags::NONE, addr, 0, MemAttrs::aligned(4));
    }
    let r = super::sha2::build(&mut b, f, &loaded[..ops.len()]).expect("the operand count of the function");
    let slot = b.slot(16, 4, SlotKind::Temp);
    let addr = b.slot_addr(slot);
    b.store(Flags::NONE, r, addr, 0, MemAttrs::aligned(4));
    let lo = b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3));
    let hi = b.load(Ty::I64, Flags::NONE, addr, 8, MemAttrs::aligned(3));
    b.ret(&[lo, hi]);
    let func: Func<Global> = b.finish_checked().unwrap_or_else(|e| panic!("{}: the built function verifies: {e:?}", f.link_name()));
    let alloc = heap.alloc();
    let mut legal = func.view().to_func_in(alloc);
    legalize(&mut legal, &NoRoutines, alloc).unwrap_or_else(|e| panic!("{}: the vector operations legalize: {e:?}", f.link_name()));
    match run_in(legal.view(), &[], 1_000_000, Global).expect("the evaluator models it") {
        Outcome::Returned(v) => match (v.first(), v.get(1)) {
            (Some(Val::Int(lo)), Some(Val::Int(hi))) => [u64::try_from(*lo).expect("an i64's bits"), u64::try_from(*hi).expect("an i64's bits")],
            other => panic!("not two integers: {other:?}"),
        },
        other => panic!("the run did not return: {other:?}"),
    }
}

fn bytes32(v: [u32; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, lane) in v.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&lane.to_le_bytes());
    }
    out
}

fn bytes64(v: [u64; 2]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&v[0].to_le_bytes());
    out[8..].copy_from_slice(&v[1].to_le_bytes());
    out
}

/// A SHA-256 intrinsic of `uint32x4_t` operands, evaluated.
fn q32(f: Sha2Fn, ops: &[[u32; 4]]) -> [u32; 4] {
    let mut bytes = [[0u8; 16]; 3];
    for (k, &v) in ops.iter().enumerate() {
        bytes[k] = bytes32(v);
    }
    let [lo, hi] = eval(f, &bytes[..ops.len()]);
    [lo as u32, (lo >> 32) as u32, hi as u32, (hi >> 32) as u32]
}

/// A SHA-512 intrinsic of `uint64x2_t` operands, evaluated.
fn q64(f: Sha2Fn, ops: &[[u64; 2]]) -> [u64; 2] {
    let mut bytes = [[0u8; 16]; 3];
    for (k, &v) in ops.iter().enumerate() {
        bytes[k] = bytes64(v);
    }
    eval(f, &bytes[..ops.len()])
}

fn add32(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    core::array::from_fn(|i| a[i].wrapping_add(b[i]))
}

fn add64(a: [u64; 2], b: [u64; 2]) -> [u64; 2] {
    [a[0].wrapping_add(b[0]), a[1].wrapping_add(b[1])]
}

/// `vextq_u64(a, b, 1)`.
fn ext64(a: [u64; 2], b: [u64; 2]) -> [u64; 2] {
    [a[1], b[0]]
}

const K32: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const K64: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc, 0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b,
    0xab1c5ed5da6d8118, 0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2, 0x72be5d74f27b896f, 0x80deb1fe3b1696b1,
    0x9bdc06a725c71235, 0xc19bf174cf692694, 0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65, 0x2de92c6f592b0275,
    0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5, 0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70, 0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df, 0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b, 0xa2bfe8a14cf10364, 0xa81a664bbc423001,
    0xc24b8b70d0f89791, 0xc76c51a30654be30, 0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8, 0x19a4c116b8d2d0c8,
    0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8, 0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec, 0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915,
    0xc67178f2e372532b, 0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178, 0x06f067aa72176fba, 0x0a637dc5a2c898a6,
    0x113f9804bef90dae, 0x1b710b35131c471b, 0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c, 0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

/// The one padded block of "abc" for a digest of `N`-byte blocks: the bytes, 0x80, zeros, and
/// the bit length (24) big-endian in the last byte.
fn abc_block<const N: usize>() -> [u8; N] {
    let mut block = [0u8; N];
    block[..3].copy_from_slice(b"abc");
    block[3] = 0x80;
    block[N - 1] = 24;
    block
}

/// Every row is found by its own link name and no other; the set's sizes and shapes.
#[test]
fn every_sha2_name_round_trips() {
    let mut seen = 0;
    for f in all() {
        let name = f.link_name();
        assert_eq!(named(name), Ok(Target::Sha2(f)), "{name}");
        assert_eq!(Target::Sha2(f).link_name(), name);
        assert_eq!(f.shape().bytes(), 16, "{name}: a Q register");
        seen += 1;
    }
    assert_eq!(seen, 8);
    assert_eq!(super::sha2::named("llvm.aarch64.crypto.sha256su0").map(|f| f.vectors()), Some(2));
    assert_eq!(super::sha2::named("llvm.aarch64.crypto.sha512h2").map(|f| f.vectors()), Some(3));
    assert!(named("llvm.aarch64.crypto.sha1c").is_err(), "SHA-1 is not in the set");
    println!("{seen} SHA-2 names, each its own");
}

/// SHA-256 of "abc" by the `sha2` crate's aarch64 compression, every intrinsic the evaluated
/// build: FIPS 180-4 B.1's digest.
#[test]
fn sha256_of_abc_is_the_fips_vector() {
    let block = abc_block::<64>();
    let mut s: [[u32; 4]; 4] = core::array::from_fn(|k| core::array::from_fn(|i| u32::from_be_bytes(block[16 * k + 4 * i..16 * k + 4 * i + 4].try_into().expect("4 bytes"))));
    let state: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let (abcd_orig, efgh_orig) = ([state[0], state[1], state[2], state[3]], [state[4], state[5], state[6], state[7]]);
    let (mut abcd, mut efgh) = (abcd_orig, efgh_orig);
    for t in 0..16 {
        let k = t % 4;
        if t >= 4 {
            let su0 = q32(Sha2Fn::Sha256su0, &[s[k], s[(k + 1) % 4]]);
            s[k] = q32(Sha2Fn::Sha256su1, &[su0, s[(k + 2) % 4], s[(k + 3) % 4]]);
        }
        let tmp = add32(s[k], [K32[4 * t], K32[4 * t + 1], K32[4 * t + 2], K32[4 * t + 3]]);
        let prev = abcd;
        abcd = q32(Sha2Fn::Sha256h, &[prev, efgh, tmp]);
        efgh = q32(Sha2Fn::Sha256h2, &[efgh, prev, tmp]);
    }
    let (abcd, efgh) = (add32(abcd, abcd_orig), add32(efgh, efgh_orig));
    let digest: [u32; 8] = [abcd[0], abcd[1], abcd[2], abcd[3], efgh[0], efgh[1], efgh[2], efgh[3]];
    assert_eq!(digest, [0xba7816bf, 0x8f01cfea, 0x414140de, 0x5dae2223, 0xb00361a3, 0x96177a9c, 0xb410ff61, 0xf20015ad]);
    println!("SHA-256(\"abc\") = {}", digest.iter().map(|w| format!("{w:08x}")).collect::<String>());
}

/// SHA-512 of "abc" by the `sha2` crate's aarch64 compression, every intrinsic the evaluated
/// build: FIPS 180-4 C.1's digest.
#[test]
fn sha512_of_abc_is_the_fips_vector() {
    let block = abc_block::<128>();
    let mut s: [[u64; 2]; 8] = core::array::from_fn(|k| core::array::from_fn(|i| u64::from_be_bytes(block[16 * k + 8 * i..16 * k + 8 * i + 8].try_into().expect("8 bytes"))));
    let h: [u64; 8] = [
        0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1, 0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b,
        0x5be0cd19137e2179,
    ];
    let orig: [[u64; 2]; 4] = [[h[0], h[1]], [h[2], h[3]], [h[4], h[5]], [h[6], h[7]]];
    // `[ab, cd, ef, gh]`; pair `t` of rounds names them, from `ab`, rotated back by `t % 4`.
    let mut p = orig;
    for t in 0..40 {
        let k = t % 8;
        if t >= 8 {
            let su0 = q64(Sha2Fn::Sha512su0, &[s[k], s[(k + 1) % 8]]);
            s[k] = q64(Sha2Fn::Sha512su1, &[su0, s[(k + 7) % 8], ext64(s[(k + 4) % 8], s[(k + 5) % 8])]);
        }
        let r = t % 4;
        let (ia, ic, ie, ig) = ((4 - r) % 4, (5 - r) % 4, (6 - r) % 4, (7 - r) % 4);
        let initial = add64(s[k], [K64[2 * t], K64[2 * t + 1]]);
        let sum = add64(ext64(initial, initial), p[ig]);
        let intermed = q64(Sha2Fn::Sha512h, &[sum, ext64(p[ie], p[ig]), ext64(p[ic], p[ie])]);
        p[ig] = q64(Sha2Fn::Sha512h2, &[intermed, p[ic], p[ia]]);
        p[ic] = add64(p[ic], intermed);
    }
    let d: [[u64; 2]; 4] = core::array::from_fn(|i| add64(p[i], orig[i]));
    let digest = [d[0][0], d[0][1], d[1][0], d[1][1], d[2][0], d[2][1], d[3][0], d[3][1]];
    assert_eq!(
        digest,
        [
            0xddaf35a193617aba, 0xcc417349ae204131, 0x12e6fa4e89a97ea2, 0x0a9eeee64b55d39a, 0x2192992a274fc1a8, 0x36ba3c23a3feebbd, 0x454d4423643ce80e,
            0x2a9ac94fa54ca49f,
        ]
    );
    println!("SHA-512(\"abc\") = {}", digest.iter().map(|w| format!("{w:016x}")).collect::<String>());
}
