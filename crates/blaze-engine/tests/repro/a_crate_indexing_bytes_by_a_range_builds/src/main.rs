use std::hash::{BuildHasher, Hasher};
use std::hint::black_box;

use foldhash::fast::FixedState;

/// A hasher over foldhash's, as blaze-hash's `FixedHasher` is.
struct Fixed(foldhash::fast::FoldHasher<'static>);

impl Hasher for Fixed {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.0.write(&bytes[..bytes.len()]);
    }

    fn finish(&self) -> u64 {
        self.0.finish()
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = Fixed(FixedState::with_seed(7).build_hasher());
    hasher.write(bytes);
    hasher.finish()
}

fn main() {
    let bytes: Vec<u8> = (0..=255u8).cycle().take(300).collect();
    let bytes = black_box(&bytes[..]);
    println!("{:?}", &bytes[1..3]);
    let lens = [0, 3, 7, 16, 200, 300];
    let hashes: Vec<u64> = lens.iter().map(|&len| hash(&bytes[..len])).collect();
    let again: Vec<u64> = lens.iter().map(|&len| hash(&bytes[..len])).collect();
    let mut distinct = hashes.clone();
    distinct.sort_unstable();
    distinct.dedup();
    println!("equal {} distinct {}", hashes == again, distinct.len() == lens.len());
}
