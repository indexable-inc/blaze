//! The leading blocks are the same chaining values however many lanes compress them, and on
//! AVX-512 when the CPU has it; the last blocks are the same IDs at every width and on every
//! x86 path; the register transposes are the scalar one.

use super::*;

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = crate::rng::SplitMix64::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

#[cfg(all(
    target_endian = "little",
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
    ),
))]
#[test]
fn leading_blocks_are_alike_at_every_width() {
    let key = words_of(&b3::context_key("blaze v2 many lanes 1"));
    for blocks in 1..CHUNK_LEN / BLOCK_LEN {
        let inputs: Vec<Vec<u8>> = (0..37).map(|n| noise(blocks * BLOCK_LEN + 1 + n % 60, n as u64)).collect();
        let inputs: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
        for count in [0, 1, 7, 8, 9, 15, 16, 17, 37] {
            let members: Vec<u32> = (0..count as u32).rev().collect();
            let (mut eight, mut sixteen, mut dispatched) = (Vec::new(), Vec::new(), Vec::new());
            leading_lanes_of::<8>(&key, &inputs, &members, blocks, &mut eight);
            leading_lanes_of::<16>(&key, &inputs, &members, blocks, &mut sixteen);
            leading_lanes(&key, &inputs, &members, blocks, &mut dispatched);
            assert_eq!(eight, sixteen, "{blocks} blocks, {count} members");
            assert_eq!(eight, dispatched, "{blocks} blocks, {count} members");
            #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
            {
                if std::arch::is_x86_feature_detected!("avx512f") {
                    let mut wide = Vec::new();
                    x86::leading_lanes_avx512(&key, &inputs, &members, blocks, &mut wide);
                    assert_eq!(eight, wide, "{blocks} blocks, {count} members, AVX-512");
                }
                if std::arch::is_x86_feature_detected!("avx2") {
                    let mut narrow = Vec::new();
                    x86::leading_lanes_avx2(&key, &inputs, &members, blocks, &mut narrow);
                    assert_eq!(eight, narrow, "{blocks} blocks, {count} members, AVX2");
                }
            }
        }
    }
}

fn scalar_columns<const S: usize>(rows: &[[u32; 16]; S]) -> [[u32; S]; 16] {
    std::array::from_fn(|word| std::array::from_fn(|lane| rows[lane][word]))
}

#[test]
fn register_transposes_are_the_scalar_one() {
    let mut rng = crate::rng::SplitMix64::new(7);
    for _ in 0..64 {
        let rows: [[u32; 16]; 16] = std::array::from_fn(|_| std::array::from_fn(|_| rng.next_u64() as u32));
        assert_eq!(columns(&rows), scalar_columns(&rows));
        let eight: [[u32; 16]; 8] = std::array::from_fn(|lane| rows[lane]);
        assert_eq!(columns(&eight), scalar_columns(&eight));
        let four: [[u32; 16]; 4] = std::array::from_fn(|lane| rows[lane]);
        assert_eq!(columns(&four), scalar_columns(&four));
        let one: [[u32; 16]; 1] = [rows[0]];
        assert_eq!(columns(&one), scalar_columns(&one));
    }
}

#[test]
fn back_transposes_are_the_scalar_one() {
    fn scalar<const S: usize>(columns: &[[u32; S]; 8]) -> [[u32; 8]; S] {
        std::array::from_fn(|lane| std::array::from_fn(|word| columns[word][lane]))
    }
    let mut rng = crate::rng::SplitMix64::new(11);
    for _ in 0..64 {
        let sixteen: [[u32; 16]; 8] = std::array::from_fn(|_| std::array::from_fn(|_| rng.next_u64() as u32));
        assert_eq!(rows_of(&sixteen), scalar(&sixteen));
        let eight: [[u32; 8]; 8] = std::array::from_fn(|word| std::array::from_fn(|lane| sixteen[word][lane]));
        assert_eq!(rows_of(&eight), scalar(&eight));
        let four: [[u32; 4]; 8] = std::array::from_fn(|word| std::array::from_fn(|lane| sixteen[word][lane]));
        assert_eq!(rows_of(&four), scalar(&four));
        let one: [[u32; 1]; 8] = std::array::from_fn(|word| [sixteen[word][0]]);
        assert_eq!(rows_of(&one), scalar(&one));
    }
}

#[test]
fn a_last_block_reads_as_its_zero_padded_copy() {
    let bytes = noise(3 * BLOCK_LEN, 5);
    for from in 0..BLOCK_LEN {
        for len in 0..=BLOCK_LEN {
            let last = &bytes[from..from + len];
            let mut block = [0; BLOCK_LEN];
            block[..len].copy_from_slice(last);
            assert_eq!(padded_words(last), block_words(&block), "{len} bytes at {from}");
        }
    }
}

/// One member's ID by a one-lane compression from `cv`: the reference every width is held to.
fn last_block_alone(input: &[u8], cv: &[u32; 8], group: usize) -> ContentId {
    let mut out = [ContentId::default()];
    let start = Start::After(std::slice::from_ref(cv));
    last_blocks_of::<1>(&[input], &[0], start, group, &mut out);
    out[0]
}

#[test]
fn last_blocks_are_alike_at_every_width_and_on_every_path() {
    let key = words_of(&b3::context_key("blaze v2 many lanes 1"));
    for group in 0..CHUNK_LEN / BLOCK_LEN {
        let inputs: Vec<Vec<u8>> =
            (0..37).map(|n| noise(group * BLOCK_LEN + 1 + (n * 7) % BLOCK_LEN, n as u64 + 100)).collect();
        let inputs: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
        let mut rng = crate::rng::SplitMix64::new(group as u64);
        let after: Vec<[u32; 8]> = (0..37).map(|_| std::array::from_fn(|_| rng.next_u64() as u32)).collect();
        for count in [0, 1, 7, 8, 9, 15, 16, 17, 37] {
            let members: Vec<u32> = (0..count as u32).rev().collect();
            let starts: [(Start<'_>, &str); 2] =
                [(Start::Key(&key), "from the key"), (Start::After(&after[..count]), "after leading blocks")];
            for (start, from) in starts {
                let cv_of = |n: usize| match start {
                    Start::Key(key) => *key,
                    Start::After(cvs) => cvs[n],
                };
                let mut expected = vec![ContentId::default(); inputs.len()];
                for (n, &at) in members.iter().enumerate() {
                    expected[at as usize] = last_block_alone(inputs[at as usize], &cv_of(n), group);
                }
                let run = |f: &dyn Fn(&mut [ContentId])| {
                    let mut out = vec![ContentId::default(); inputs.len()];
                    f(&mut out);
                    out
                };
                let what = format!("group {group}, {count} members, {from}");
                assert_eq!(run(&|out| last_blocks_of::<8>(&inputs, &members, start, group, out)), expected, "{what}, 8");
                assert_eq!(run(&|out| last_blocks_of::<16>(&inputs, &members, start, group, out)), expected, "{what}, 16");
                assert_eq!(run(&|out| last_blocks(&inputs, &members, start, group, out)), expected, "{what}, dispatched");
                #[cfg(target_arch = "x86_64")]
                {
                    if std::arch::is_x86_feature_detected!("avx512f") {
                        let wide = run(&|out| x86::last_blocks_avx512(&inputs, &members, start, group, out));
                        assert_eq!(wide, expected, "{what}, AVX-512");
                    }
                    if std::arch::is_x86_feature_detected!("avx2") {
                        let narrow = run(&|out| x86::last_blocks_avx2(&inputs, &members, start, group, out));
                        assert_eq!(narrow, expected, "{what}, AVX2");
                    }
                }
            }
        }
    }
}

/// A one-lane compression from the key is BLAKE3 itself, so the reference above is.
#[test]
fn one_lane_from_the_key_is_blake3() {
    let key = words_of(&b3::context_key("blaze v2 many lanes 1"));
    for len in 0..=BLOCK_LEN {
        let input = noise(len, len as u64);
        let mut expected = blake3::Hasher::new_derive_key("blaze v2 many lanes 1");
        expected.update(&input);
        assert_eq!(last_block_alone(&input, &key, 0), ContentId(*expected.finalize().as_bytes()), "{len} bytes");
    }
}
