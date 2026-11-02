//! Deterministic operands: SplitMix64 for the random part, and per type the edges every
//! operation is tried at first (0, ±1, MIN, MAX, NaNs with payloads, ±0, infinities,
//! subnormals). A failure names its seed, so a rerun gives the same inputs.

use blaze_ids::rng::{GAMMA, SplitMix64};

use crate::shape::Ty;

/// [`SplitMix64`] with the operand draws.
#[derive(Clone, Debug)]
pub struct Rng(SplitMix64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(SplitMix64::new(seed ^ GAMMA))
    }

    pub fn next(&mut self) -> u64 {
        self.0.next_u64()
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.0.below(n)
    }

    pub fn fill(&mut self, out: &mut [u8]) {
        self.0.fill(out);
    }

    /// 16 bytes, with the low `ty.bytes()` a random value of `ty` biased to its edges a third
    /// of the time and the rest random (what a slot holds beyond its value is unspecified, so
    /// a stencil that reads it shows).
    pub fn value(&mut self, ty: Ty) -> [u8; 16] {
        let mut out = [0u8; 16];
        self.fill(&mut out);
        if self.below(3) == 0 {
            let edges = edges(ty);
            let edge = &edges[self.below(edges.len() as u64) as usize];
            out[..edge.len()].copy_from_slice(edge);
        } else if ty.is_vector() {
            let lane = ty.lane();
            let width = usize::from(lane.bytes());
            for at in (0..16).step_by(width) {
                if self.below(4) == 0 {
                    let edges = edges(lane);
                    let edge = &edges[self.below(edges.len() as u64) as usize];
                    out[at..at + width].copy_from_slice(edge);
                }
            }
        }
        out
    }
}

/// The values of `ty` every operation is tried at, each `ty.bytes()` little-endian bytes (a
/// vector's are its lane's edges in every lane, and lanes of different edges).
pub fn edges(ty: Ty) -> Vec<Vec<u8>> {
    fn ints(bits: u32) -> Vec<u128> {
        let mask = if bits == 128 { u128::MAX } else { (1u128 << bits) - 1 };
        let min = 1u128 << (bits - 1);
        let mut out = vec![0, 1, mask, min, min - 1, min + 1, 2, mask - 1, 0x55, 0xaa];
        out.extend([bits as u128 - 1, bits as u128, bits as u128 + 1, 0x7f, 0x80, 0xff, 0x100]);
        out.extend([0x7fff, 0x8000, 0xffff, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff]);
        out.extend([0x1_0000_0000, 0x7fff_ffff_ffff_ffff, 0x8000_0000_0000_0000]);
        out.iter().map(|it| it & mask).collect()
    }
    match ty {
        Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::I128 => {
            let bytes = usize::from(ty.bytes());
            let mut out: Vec<Vec<u8>> =
                ints(u32::from(ty.bytes()) * 8).iter().map(|it| it.to_le_bytes()[..bytes].to_vec()).collect();
            out.dedup();
            out
        }
        Ty::F16 => [0x0000u16, 0x8000, 0x3c00, 0xbc00, 0x7c00, 0xfc00, 0x7e00, 0x7d01, 0xfe55, 0x0001, 0x03ff, 0x7bff, 0x3800]
            .iter()
            .map(|it| it.to_le_bytes().to_vec())
            .collect(),
        Ty::F32 => {
            let mut out: Vec<u32> = vec![
                0x0000_0000, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x7f80_0000, 0xff80_0000,
                0x7fc0_0000, 0xffc0_0000, 0x7fa0_0001, 0x7fc1_2345, 0xff81_0000, 0x0000_0001,
                0x8000_0001, 0x007f_ffff, 0x0080_0000, 0x7f7f_ffff, 0xff7f_ffff, 0x3f00_0000,
                0x3fc0_0000, 0x4020_0000, 0xbfc0_0000, 0x4f00_0000, 0x4f80_0000, 0xcf00_0000,
                0x5f00_0000, 0x5f80_0000, 0xdf00_0000, 0x4effffff, 0x4b00_0000, 0x4b40_0001,
            ];
            out.extend([0.5f32, 1.5, 2.5, -0.5, -2.5, 3.0, 1e10, -1e10, 255.5, 65535.0, -1.0e-40].map(f32::to_bits));
            out.iter().map(|it| it.to_le_bytes().to_vec()).collect()
        }
        Ty::F64 => {
            let mut out: Vec<u64> = vec![
                0, 0x8000_0000_0000_0000, 0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
                0x7ff8_0000_0000_0000, 0xfff8_0000_0000_0000, 0x7ff4_0000_0000_0001,
                0x7ff8_dead_beef_0001, 1, 0x8000_0000_0000_0001, 0x000f_ffff_ffff_ffff,
                0x0010_0000_0000_0000, 0x7fef_ffff_ffff_ffff, 0xffef_ffff_ffff_ffff,
            ];
            out.extend(
                [
                    1.0f64, -1.0, 0.5, 1.5, 2.5, -2.5, 3.0, 2147483647.0, 2147483648.0, -2147483648.0,
                    -2147483649.0, 4294967295.0, 4294967296.0, 9.223372036854775807e18,
                    9.223372036854775808e18, -9.223372036854775808e18, 1.8446744073709552e19,
                    4503599627370496.5, 1e300, -1e-310,
                ]
                .map(f64::to_bits),
            );
            out.iter().map(|it| it.to_le_bytes().to_vec()).collect()
        }
        Ty::F128 => {
            let one = 0x3fff_u128 << 112;
            let inf = 0x7fff_u128 << 112;
            [0u128, 1 << 127, one, one | 1 << 127, inf, inf | 1 << 127, inf | 1 << 111, inf | 1, 1]
                .iter()
                .map(|it| it.to_le_bytes().to_vec())
                .collect()
        }
        _ => {
            let lane = ty.lane();
            let lanes = edges(lane);
            let width = usize::from(lane.bytes());
            let count = usize::from(ty.lanes());
            let mut out = Vec::new();
            for edge in &lanes {
                out.push(edge.repeat(count));
            }
            for start in 0..lanes.len() {
                let mut mixed = Vec::with_capacity(16);
                for k in 0..count {
                    mixed.extend_from_slice(&lanes[(start + k * 3) % lanes.len()]);
                }
                debug_assert_eq!(mixed.len(), width * count);
                out.push(mixed);
            }
            out
        }
    }
}

#[test]
fn edges_are_values_of_their_type() {
    for ty in Ty::ALL {
        let edges = edges(ty);
        assert!(edges.len() >= 4, "{}", ty.name());
        for edge in &edges {
            assert_eq!(edge.len(), usize::from(ty.bytes()), "{}", ty.name());
        }
    }
}

#[test]
fn the_same_seed_gives_the_same_values() {
    let (mut a, mut b) = (Rng::new(7), Rng::new(7));
    for ty in Ty::ALL {
        assert_eq!(a.value(ty), b.value(ty));
    }
    assert_ne!(Rng::new(1).next(), Rng::new(2).next());
}
