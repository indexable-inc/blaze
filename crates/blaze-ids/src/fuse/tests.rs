use hegel::TestCase;
use hegel::generators as gs;

use super::{Fuse8, HEADER};
use crate::rng::SplitMix64;

fn random(seed: u64, n: usize) -> Vec<u64> {
    let mut rng = SplitMix64::new(seed);
    (0..n).map(|_| rng.next_u64()).collect()
}

/// The model: a filter never hides a key it was built of, at any size, with keys repeated,
/// and the same after an encoding and decoding.
#[hegel::test]
fn a_filter_never_hides_a_key_it_was_built_of(tc: TestCase) {
    let mut keys: Vec<u64> = tc.draw(gs::vecs(gs::integers()).max_size(3000));
    let repeated: usize = tc.draw(gs::integers().max_value(keys.len()));
    keys.extend_from_within(..repeated);
    let filter = Fuse8::of(keys.clone());
    let mut bytes = Vec::new();
    filter.encode(&mut bytes);
    assert_eq!(bytes.len(), filter.encoded_len());
    let decoded = Fuse8::decode(&bytes).unwrap();
    assert_eq!(decoded, filter);
    for key in keys {
        assert!(filter.contains(key), "{key:#x} hidden");
    }
}

#[test]
fn every_size_up_to_a_thousand_holds_its_keys() {
    for n in 0..1000 {
        let keys = random(n as u64, n);
        let filter = Fuse8::of(keys.clone());
        assert!(keys.iter().all(|&key| filter.contains(key)), "a filter of {n} keys");
    }
}

#[test]
fn a_large_filter_is_about_nine_bits_a_key_and_rarely_wrong() {
    const N: usize = 200_000;
    let keys = random(1, N);
    let filter = Fuse8::of(keys.clone());
    assert!(keys.iter().all(|&key| filter.contains(key)));
    let bits = 8.0 * filter.encoded_len() as f64 / N as f64;
    assert!(bits < 9.5, "{bits} bits a key");
    let wrong = random(2, N).into_iter().filter(|&key| filter.contains(key)).count();
    let rate = wrong as f64 / N as f64;
    assert!(rate < 0.006, "{rate} of keys not in it are said to be");
}

#[test]
fn decode_refuses_what_a_build_does_not_make() {
    let mut good = Vec::new();
    Fuse8::of(random(3, 100)).encode(&mut good);
    let refused = |bytes: &[u8], why: &str| {
        let error = Fuse8::decode(bytes).unwrap_err();
        assert!(error.contains(why), "{error:?} does not say {why:?}");
    };
    refused(&good[..HEADER - 1], "shorter");
    refused(&good[..good.len() - 1], "another length");
    let mut long = good.clone();
    long.push(0);
    refused(&long, "another length");
    let mut length = good.clone();
    length[8..12].copy_from_slice(&3u32.to_le_bytes());
    refused(&length, "power of two");
    length[8..12].copy_from_slice(&(1u32 << 19).to_le_bytes());
    refused(&length, "power of two");
    let mut none = good.clone();
    none[12..16].copy_from_slice(&0u32.to_le_bytes());
    refused(&none, "no segments");
    let mut huge = good;
    huge[8..16].copy_from_slice(&[0, 0, 4, 0, 0, 0, 0, 1]);
    refused(&huge, "more than 2^32");
}
