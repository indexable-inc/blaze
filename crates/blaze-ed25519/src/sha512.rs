//! SHA-512 (FIPS 180-4, section 6.4), streaming and one-shot: the hash Ed25519 is defined over
//! (RFC 8032, section 5.1). Scalar code: verification hashes R, A and a short statement, so the
//! hash is a few percent of a verification (`examples/cost.rs`); the CPU's SHA-512 instructions
//! (FEAT_SHA512 on AArch64) are not used, and would be the next step if it ever mattered.
//!
//! The 80 round constants and the initial value are typed in from the standard; the tests derive
//! both again from the first 80 primes (the fractional parts of their cube and square roots) and
//! hold them equal (`tests/sha512.rs`).

/// Bytes of a digest.
pub const DIGEST_LEN: usize = 64;
/// Bytes of a block.
pub const BLOCK_LEN: usize = 128;

/// The round constants, FIPS 180-4 section 4.2.3: the first 64 bits of the fractional parts of
/// the cube roots of the first 80 primes.
pub const K: [u64; 80] = [
    0x428a_2f98_d728_ae22,
    0x7137_4491_23ef_65cd,
    0xb5c0_fbcf_ec4d_3b2f,
    0xe9b5_dba5_8189_dbbc,
    0x3956_c25b_f348_b538,
    0x59f1_11f1_b605_d019,
    0x923f_82a4_af19_4f9b,
    0xab1c_5ed5_da6d_8118,
    0xd807_aa98_a303_0242,
    0x1283_5b01_4570_6fbe,
    0x2431_85be_4ee4_b28c,
    0x550c_7dc3_d5ff_b4e2,
    0x72be_5d74_f27b_896f,
    0x80de_b1fe_3b16_96b1,
    0x9bdc_06a7_25c7_1235,
    0xc19b_f174_cf69_2694,
    0xe49b_69c1_9ef1_4ad2,
    0xefbe_4786_384f_25e3,
    0x0fc1_9dc6_8b8c_d5b5,
    0x240c_a1cc_77ac_9c65,
    0x2de9_2c6f_592b_0275,
    0x4a74_84aa_6ea6_e483,
    0x5cb0_a9dc_bd41_fbd4,
    0x76f9_88da_8311_53b5,
    0x983e_5152_ee66_dfab,
    0xa831_c66d_2db4_3210,
    0xb003_27c8_98fb_213f,
    0xbf59_7fc7_beef_0ee4,
    0xc6e0_0bf3_3da8_8fc2,
    0xd5a7_9147_930a_a725,
    0x06ca_6351_e003_826f,
    0x1429_2967_0a0e_6e70,
    0x27b7_0a85_46d2_2ffc,
    0x2e1b_2138_5c26_c926,
    0x4d2c_6dfc_5ac4_2aed,
    0x5338_0d13_9d95_b3df,
    0x650a_7354_8baf_63de,
    0x766a_0abb_3c77_b2a8,
    0x81c2_c92e_47ed_aee6,
    0x9272_2c85_1482_353b,
    0xa2bf_e8a1_4cf1_0364,
    0xa81a_664b_bc42_3001,
    0xc24b_8b70_d0f8_9791,
    0xc76c_51a3_0654_be30,
    0xd192_e819_d6ef_5218,
    0xd699_0624_5565_a910,
    0xf40e_3585_5771_202a,
    0x106a_a070_32bb_d1b8,
    0x19a4_c116_b8d2_d0c8,
    0x1e37_6c08_5141_ab53,
    0x2748_774c_df8e_eb99,
    0x34b0_bcb5_e19b_48a8,
    0x391c_0cb3_c5c9_5a63,
    0x4ed8_aa4a_e341_8acb,
    0x5b9c_ca4f_7763_e373,
    0x682e_6ff3_d6b2_b8a3,
    0x748f_82ee_5def_b2fc,
    0x78a5_636f_4317_2f60,
    0x84c8_7814_a1f0_ab72,
    0x8cc7_0208_1a64_39ec,
    0x90be_fffa_2363_1e28,
    0xa450_6ceb_de82_bde9,
    0xbef9_a3f7_b2c6_7915,
    0xc671_78f2_e372_532b,
    0xca27_3ece_ea26_619c,
    0xd186_b8c7_21c0_c207,
    0xeada_7dd6_cde0_eb1e,
    0xf57d_4f7f_ee6e_d178,
    0x06f0_67aa_7217_6fba,
    0x0a63_7dc5_a2c8_98a6,
    0x113f_9804_bef9_0dae,
    0x1b71_0b35_131c_471b,
    0x28db_77f5_2304_7d84,
    0x32ca_ab7b_40c7_2493,
    0x3c9e_be0a_15c9_bebc,
    0x431d_67c4_9c10_0d4c,
    0x4cc5_d4be_cb3e_42b6,
    0x597f_299c_fc65_7e2a,
    0x5fcb_6fab_3ad6_faec,
    0x6c44_198c_4a47_5817,
];

/// The initial value, FIPS 180-4 section 5.3.5: the first 64 bits of the fractional parts of the
/// square roots of the first eight primes.
pub const IV: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

/// The compression function on one block: FIPS 180-4 section 6.4.2, steps 1 to 4, with the
/// message schedule kept as a ring of 16 words.
fn compress(state: &mut [u64; 8], block: &[u8; BLOCK_LEN]) {
    let mut w = [0u64; 16];
    for (word, bytes) in w.iter_mut().zip(block.chunks_exact(8)) {
        let mut be = [0u8; 8];
        be.copy_from_slice(bytes);
        *word = u64::from_be_bytes(be);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for (t, k) in K.iter().enumerate() {
        let wt = if t < 16 {
            w[t]
        } else {
            let w15 = w[(t + 1) % 16];
            let w2 = w[(t + 14) % 16];
            let s0 = w15.rotate_right(1) ^ w15.rotate_right(8) ^ (w15 >> 7);
            let s1 = w2.rotate_right(19) ^ w2.rotate_right(61) ^ (w2 >> 6);
            let next = w[t % 16].wrapping_add(s0).wrapping_add(w[(t + 9) % 16]).wrapping_add(s1);
            w[t % 16] = next;
            next
        };
        let big_s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ (!e & g);
        let t1 = h.wrapping_add(big_s1).wrapping_add(ch).wrapping_add(*k).wrapping_add(wt);
        let big_s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(add);
    }
}

/// A SHA-512 computation fed in pieces: equal to [`hash`] of the pieces joined, however they
/// are cut.
#[derive(Clone)]
pub struct Sha512 {
    state: [u64; 8],
    /// Bytes of a block not yet compressed: `buffer[..buffered]`.
    buffer: [u8; BLOCK_LEN],
    buffered: usize,
    /// Bytes fed so far; the standard bounds a message below 2^128 bits.
    fed: u128,
}

impl Default for Sha512 {
    fn default() -> Sha512 {
        Sha512::new()
    }
}

impl Sha512 {
    pub const fn new() -> Sha512 {
        Sha512 { state: IV, buffer: [0; BLOCK_LEN], buffered: 0, fed: 0 }
    }

    /// Feeds `input`: whole blocks are compressed where they lie, with no copy.
    pub fn update(&mut self, mut input: &[u8]) {
        self.fed = self.fed.wrapping_add(blaze_int::Cast::<usize, u128>::widen(input.len()));
        if self.buffered > 0 {
            let take = (BLOCK_LEN - self.buffered).min(input.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&input[..take]);
            self.buffered += take;
            input = &input[take..];
            if self.buffered < BLOCK_LEN {
                return;
            }
            compress(&mut self.state, &self.buffer);
            self.buffered = 0;
        }
        let mut blocks = input.chunks_exact(BLOCK_LEN);
        for block in &mut blocks {
            let block: &[u8; BLOCK_LEN] = block.try_into().expect("chunks_exact gives whole blocks");
            compress(&mut self.state, block);
        }
        let rest = blocks.remainder();
        self.buffer[..rest.len()].copy_from_slice(rest);
        self.buffered = rest.len();
    }

    /// The digest: padding (section 5.1.2: a one bit, zeros, the length in bits as 128 bits
    /// big-endian), then the state big-endian.
    pub fn finalize(mut self) -> [u8; DIGEST_LEN] {
        let bits = self.fed.wrapping_mul(8);
        self.buffer[self.buffered] = 0x80;
        self.buffer[self.buffered + 1..].fill(0);
        if self.buffered + 1 > BLOCK_LEN - 16 {
            compress(&mut self.state, &self.buffer);
            self.buffer.fill(0);
        }
        self.buffer[BLOCK_LEN - 16..].copy_from_slice(&bits.to_be_bytes());
        compress(&mut self.state, &self.buffer);
        let mut out = [0u8; DIGEST_LEN];
        for (bytes, word) in out.chunks_exact_mut(8).zip(self.state) {
            bytes.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// SHA-512 of `input`, in one call.
pub fn hash(input: &[u8]) -> [u8; DIGEST_LEN] {
    let mut hasher = Sha512::new();
    hasher.update(input);
    hasher.finalize()
}
