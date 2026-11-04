use blaze_ids::ContentId;
use blaze_ids::rng::SplitMix64;
use hegel::TestCase;
use hegel::generators as gs;

use super::*;

/// The offset `compare` names is the first at which the two differ, or the shorter's length
/// when one is the other's prefix, and equal bytes compare equal.
#[hegel::test(test_cases = 200)]
fn the_named_byte_is_the_first_that_differs(tc: TestCase) {
    let stored: Vec<u8> = tc.draw(gs::vecs(gs::integers::<u8>().max_value(3)).max_size(40));
    let mut recomputed = stored.clone();
    match tc.draw(gs::integers::<u8>().max_value(3)) {
        0 => {}
        1 if !recomputed.is_empty() => {
            let at: usize = tc.draw(gs::integers().max_value(recomputed.len() - 1));
            recomputed[at] ^= 1 + tc.draw(gs::integers::<u8>().max_value(254));
        }
        2 => recomputed.truncate(tc.draw(gs::integers().max_value(stored.len()))),
        _ => recomputed.push(tc.draw(gs::integers::<u8>())),
    }
    let naive =
        (0..stored.len().max(recomputed.len())).find(|&n| stored.get(n) != recomputed.get(n));
    let hit = Hit { function: "f", identity: ContentId([1; 32]), reads: &[], stored: &stored };
    match (compare(&hit, &recomputed), naive) {
        (Ok(()), None) => {}
        (Err(it), Some(at)) => {
            assert_eq!(it.at, at);
            assert_eq!(
                (it.stored, it.recomputed),
                (stored.get(at).copied(), recomputed.get(at).copied())
            );
        }
        (got, naive) => panic!("compare gave {got:?}, the first difference is {naive:?}"),
    }
}

/// A mismatch's message names the function, the identity, the reads and the byte.
#[test]
fn a_mismatch_names_the_function_identity_reads_and_byte() {
    let reads = [RecordedRead { what: "layout of `Foo`", hash: ContentId([7; 32]) }];
    let hit = Hit {
        function: "fixture::f",
        identity: ContentId([9; 32]),
        reads: &reads,
        stored: &[1, 2, 3],
    };
    let message = compare(&hit, &[1, 2, 4]).unwrap_err().to_string();
    for part in [
        "`fixture::f`",
        &format!("{:x}", ContentId([9; 32])),
        "layout of `Foo`",
        "byte 2",
        "0x03",
        "0x04",
    ] {
        assert!(message.contains(part), "{part} is not in: {message}");
    }
}

/// Sampling is a function of the seed and the identity: the same every time, about the rate
/// over many identities, and another seed samples others.
#[test]
fn sampling_is_deterministic_and_near_the_rate() {
    let ids: Vec<ContentId> =
        (0..4000u32).map(|n| ContentId::of("blaze v2 verify test 1", &n.to_le_bytes())).collect();
    let tenth = Setting::parse("0.1").unwrap();
    let taken: Vec<bool> = ids.iter().map(|it| tenth.sampled(it)).collect();
    assert_eq!(taken, ids.iter().map(|it| tenth.sampled(it)).collect::<Vec<_>>());
    let count = taken.iter().filter(|it| **it).count();
    assert!((300..500).contains(&count), "{count} of 4000 at a rate of 0.1");
    let other = Setting::parse(r#"{"rate": 0.1, "seed": 3}"#).unwrap();
    assert_ne!(taken, ids.iter().map(|it| other.sampled(it)).collect::<Vec<_>>());
    assert!(ids.iter().all(|it| Setting::ALL.sampled(it) && !Setting::OFF.sampled(it)));
}

/// What is no setting is refused naming the variable, never read as off.
#[test]
fn a_bad_setting_is_refused() {
    for bad in ["", "yes", "1.5", "-0.1", r#"{"rate": 0.1, "sed": 3}"#, r#"{"seed": 3}"#] {
        let why = Setting::parse(bad).unwrap_err().to_string();
        assert!(why.starts_with(VAR), "{bad}: {why}");
    }
    let env = |value: &str| vec![(VAR.to_owned(), value.to_owned())];
    assert_eq!(Setting::from_env(&[]), Ok(Setting::OFF));
    assert_eq!(Setting::from_env(&env("1")), Ok(Setting::ALL));
    assert!(Setting::from_env(&env("x")).is_err());
}

struct Again(Vec<u8>);

impl Recompute for Again {
    fn recompute(&self, _: &Hit<'_>) -> Result<Vec<u8>, String> {
        Ok(self.0.clone())
    }
}

/// `check` recomputes what it samples and passes it when equal.
#[test]
fn check_recomputes_what_it_samples() {
    let hit = Hit { function: "f", identity: ContentId([2; 32]), reads: &[], stored: &[5, 6] };
    assert_eq!(check(&Setting::OFF, &hit, &Again(vec![0])), Ok(false));
    assert_eq!(check(&Setting::ALL, &hit, &Again(vec![5, 6])), Ok(true));
    assert!(
        matches!(check(&Setting::ALL, &hit, &Again(vec![5])), Err(VerifyError::Mismatch(it)) if it.at == 1)
    );
}

#[test]
fn every_simd_path_finds_the_first_difference_the_scalar_one_does() {
    let mut rng = SplitMix64::new(1606);
    let paths = bytes::available();
    assert_eq!(paths.last(), Some(&bytes::Simd::Scalar));
    for len in (0..300).chain([1 << 12, (1 << 16) + 7]) {
        let a: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        let mut cases = vec![(a.clone(), None)];
        for _ in 0..4 {
            if len == 0 {
                break;
            }
            let at = rng.below(len as u64) as usize;
            let mut b = a.clone();
            b[at] ^= 1 << rng.below(8);
            // A second difference after the first must not hide it.
            if at + 1 < len {
                b[len - 1] ^= 0x80;
            }
            cases.push((b, Some(at)));
        }
        let mut shorter = a.clone();
        shorter.truncate(len / 2);
        cases.push((shorter, if len / 2 < len { Some(len / 2) } else { None }));
        for (b, want) in &cases {
            for &path in &paths {
                assert_eq!(
                    bytes::first_difference_with(path, &a, b),
                    *want,
                    "{path:?}, length {len}"
                );
                assert_eq!(
                    bytes::first_difference_with(path, b, &a),
                    *want,
                    "{path:?}, length {len}, swapped"
                );
            }
        }
    }
}

#[test]
fn every_simd_path_finds_each_difference_in_a_block_and_the_overlapping_tail() {
    let paths = bytes::available();
    let mut rng = SplitMix64::new(77);
    // Every length across the four-vector blocks (up to 256 bytes) and the tails after them;
    // a single difference at every offset, so each lane of each vector and of the last,
    // overlapping one is where the first difference is.
    for len in (0..=600).chain([1023, 1024, 1025, 4096 + 63]) {
        let a: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        for &path in &paths {
            assert_eq!(bytes::first_difference_with(path, &a, &a), None, "{path:?}, length {len}");
        }
        let offsets: Vec<usize> = if len <= 300 {
            (0..len).collect()
        } else {
            (0..64).map(|_| rng.below(len as u64) as usize).collect()
        };
        for at in offsets {
            let mut b = a.clone();
            b[at] = !b[at];
            for &path in &paths {
                assert_eq!(
                    bytes::first_difference_with(path, &a, &b),
                    Some(at),
                    "{path:?}, length {len}, at {at}"
                );
            }
        }
    }
}
