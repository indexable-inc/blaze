use blaze_ids::rng::SplitMix64;
use hegel::TestCase;
use hegel::generators as gs;

use super::*;

#[cfg(not(blaze_check))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The reference: the copy `blaze-cargo`'s and `blaze-engine`'s build scripts each carried, the
/// simplest of the four, one portable block at a time through a growable buffer.
struct Reference {
    state: [u32; 8],
    pending: Vec<u8>,
    len: u64,
}

impl Reference {
    fn new() -> Reference {
        Reference { state: INITIAL, pending: Vec::with_capacity(64), len: 0 }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.len += bytes.len() as u64;
        if !self.pending.is_empty() {
            let take = (64 - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() < 64 {
                return;
            }
            let block: [u8; 64] = self.pending[..].try_into().unwrap();
            self.compress(&block);
            self.pending.clear();
        }
        let (blocks, rest) = bytes.as_chunks::<64>();
        for block in blocks {
            self.compress(block);
        }
        self.pending.extend_from_slice(rest);
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.len * 8;
        let mut tail = vec![0x80];
        tail.resize((119 - self.pending.len()) % 64 + 1, 0);
        tail.extend_from_slice(&bits.to_be_bytes());
        self.update(&tail);
        let mut out = [0; 32];
        for (word, at) in self.state.iter().zip(out.as_chunks_mut::<4>().0) {
            *at = word.to_be_bytes();
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (word, at) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*at);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            (h, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (word, add) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(add);
        }
    }
}

fn reference(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Reference::new();
    hasher.update(bytes);
    hasher.finish()
}

/// Every compression this processor can run, by name, for the differential tests.
#[allow(unsafe_code)]
fn paths() -> Vec<(&'static str, fn(&mut [u32; 8], &[u8]))> {
    let mut out: Vec<(&'static str, fn(&mut [u32; 8], &[u8]))> = vec![("portable", portable)];
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("sha2") {
        // SAFETY: the processor has the SHA-256 instructions, checked just above.
        out.push(("aarch64 sha2", |state, blocks| unsafe { aarch64::compress(state, blocks) }));
    }
    #[cfg(target_arch = "x86_64")]
    if x86::detected() {
        // SAFETY: `detected` checked SHA-NI, SSSE3 and SSE4.1.
        out.push(("x86-64 sha-ni", |state, blocks| unsafe { x86::compress(state, blocks) }));
    }
    out
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0; len];
    SplitMix64::new(seed).fill(&mut out);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

#[test]
fn known_digests() {
    let cases: [(&[u8], &str); 4] = [
        (b"", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        (b"abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        (
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        ),
        (
            b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
        ),
    ];
    for (input, want) in cases {
        assert_eq!(hex(&digest(input)), want, "{}", String::from_utf8_lossy(input));
        assert_eq!(hex(&reference(input)), want, "reference: {}", String::from_utf8_lossy(input));
    }
    let million = vec![b'a'; 1_000_000];
    assert_eq!(hex(&digest(&million)), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
}

/// The lengths around one and two padding blocks, and the page sizes a code signature hashes.
#[test]
fn every_length_around_the_padding() {
    let bytes = noise(70_000, 1);
    for len in [0, 1, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128, 129, 4096, 16384, 69_952, 70_000] {
        assert_eq!(digest(&bytes[..len]), reference(&bytes[..len]), "{len} bytes");
    }
}

#[test]
fn instructions_as_portable() {
    let bytes = noise(70_000, 2);
    for len in [0, 64, 128, 4096, 16384, 69_952] {
        let (mut fast, mut slow) = (INITIAL, INITIAL);
        compress(&mut fast, &bytes[..len]);
        portable(&mut slow, &bytes[..len]);
        assert_eq!(fast, slow, "{len} bytes");
        for (name, path) in paths() {
            let mut state = INITIAL;
            path(&mut state, &bytes[..len]);
            assert_eq!(state, slow, "{name}, {len} bytes");
        }
    }
}

#[test]
fn every_path_on_random_states() {
    let mut rng = SplitMix64::new(3);
    for round in 0..64 {
        let blocks = 1 + rng.index(8);
        let bytes = noise(blocks * 64, rng.next_u64());
        let start: [u32; 8] = std::array::from_fn(|_| rng.next_u32());
        let mut want = start;
        portable(&mut want, &bytes);
        for (name, path) in paths() {
            let mut state = start;
            path(&mut state, &bytes);
            assert_eq!(state, want, "{name}, round {round}");
        }
    }
}

#[test]
fn parts_hash_as_the_whole() {
    let bytes = noise(10_000, 4);
    let whole = digest(&bytes);
    let mut rng = SplitMix64::new(5);
    for _ in 0..100 {
        let mut hasher = Sha256::new();
        let mut at = 0;
        while at < bytes.len() {
            let len = rng.index(200).min(bytes.len() - at);
            hasher.update(&bytes[at..at + len]);
            at += len;
        }
        assert_eq!(hasher.finish(), whole);
    }
    let mut hasher = Sha256::default();
    hasher.update(b"ab").update(b"c");
    assert_eq!(hex(&hasher.finish()), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}

#[test]
fn hashing_allocates_nothing() {
    let bytes = noise(70_000, 6);
    let want = reference(&bytes);
    let got = blaze_alloc_count::assert_at_most(0, "SHA-256", || {
        let mut hasher = Sha256::new();
        hasher.update(&bytes[..13]).update(&bytes[13..]);
        assert_eq!(hasher.finish(), digest(&bytes));
        digest(&bytes)
    });
    assert_eq!(got, want);
}

#[hegel::test]
fn matches_the_reference(tc: TestCase) {
    let bytes: Vec<u8> = tc.draw(gs::binary().max_size(1999));
    let want = reference(&bytes);
    assert_eq!(digest(&bytes), want);
    let mut cuts: Vec<usize> = tc.draw(gs::vecs(gs::integers().max_value(bytes.len())).max_size(5));
    cuts.sort_unstable();
    let mut hasher = Sha256::new();
    let mut at = 0;
    for cut in cuts.into_iter().chain([bytes.len()]) {
        hasher.update(&bytes[at..cut]);
        at = cut;
    }
    assert_eq!(hasher.finish(), want);
}
