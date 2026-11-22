//! SHA-1 (FIPS 180-4), portable. **Only git's object id, never trust.** SHA-1 is broken for
//! collisions (SHAttered, 2017): an id here names an object the way git does and checks a
//! pack was not corrupted in transit, nothing more. What makes a fetched git dependency
//! trusted is the commit id the lock file pins, fetched over TLS from the named host; blaze's
//! own records are keyed by BLAKE3 and SHA-256 (blaze-blake3, blaze-sha256).

#![blaze::sans_io]

/// A running SHA-1.
#[derive(Clone, Copy)]
pub struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    filled: usize,
    len: u64,
}

impl Sha1 {
    #[expect(clippy::new_without_default, reason = "a hash's start is named, not defaulted")]
    pub const fn new() -> Sha1 {
        Sha1 { state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0], block: [0; 64], filled: 0, len: 0 }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        self.len = self.len.wrapping_add(bytes.len() as u64);
        if self.filled > 0 {
            let take = (64 - self.filled).min(bytes.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&bytes[..take]);
            self.filled += take;
            bytes = &bytes[take..];
            if self.filled < 64 {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        let (blocks, rest) = bytes.as_chunks::<64>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        self.block[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.len.wrapping_mul(8);
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        let pad_len = if self.filled < 56 { 56 - self.filled } else { 120 - self.filled };
        pad[pad_len..pad_len + 8].copy_from_slice(&bits.to_be_bytes());
        let len = self.len;
        self.update(&pad[..pad_len + 8]);
        self.len = len;
        let mut out = [0u8; 20];
        for (word, chunk) in self.state.iter().zip(out.chunks_exact_mut(4)) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// The SHA-1 of `bytes`.
pub fn sha1(bytes: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(bytes);
    h.finish()
}

fn compress(state: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for (i, chunk) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (i, &wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..20 => ((b & c) | (!b & d), 0x5a82_7999),
            20..40 => (b ^ c ^ d, 0x6ed9_eba1),
            40..60 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d, e]) {
        *s = s.wrapping_add(v);
    }
}
