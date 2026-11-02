use crate::{
    Command, Options,
    corpus::{self, Corpus},
    features::{Seen, bucket},
    mutate::{self, MUTATIONS, Mutation, Mutator, Random, Recent},
    raw::{self, Coverage},
};

/// No comparisons seen, as in a build without trace-cmp.
struct NoRecent;

impl Recent for NoRecent {
    fn compare(&self, _: usize) -> (u64, u64, usize) {
        (0, 0, 0)
    }

    fn bytes(&self, _: usize) -> ([u8; raw::WORD], [u8; raw::WORD], usize) {
        ([0; raw::WORD], [0; raw::WORD], 0)
    }
}

/// One comparison in every slot: `0x11223344` against `0xdeadbeef`, and `MAGIC` against
/// `magic`.
struct OneRecent;

impl Recent for OneRecent {
    fn compare(&self, _: usize) -> (u64, u64, usize) {
        (0x1122_3344, 0xdead_beef, 4)
    }

    fn bytes(&self, _: usize) -> ([u8; raw::WORD], [u8; raw::WORD], usize) {
        let (mut a, mut b) = ([0; raw::WORD], [0; raw::WORD]);
        a[..5].copy_from_slice(b"MAGIC");
        b[..5].copy_from_slice(b"magic");
        (a, b, 5)
    }
}

#[test]
fn buckets_are_afls() {
    let expected = [(1, 0), (2, 1), (3, 2), (4, 3), (7, 3), (8, 4), (15, 4), (16, 5), (31, 5), (32, 6), (127, 6), (128, 7), (255, 7)];
    for (count, want) in expected {
        assert_eq!(bucket(count), want, "count {count}");
    }
}

#[test]
fn seen_counts_new_features_once() {
    let mut seen = Seen::new(1 << 10);
    assert_eq!(seen.merge(&[1, 64, 1000]), 3);
    assert_eq!(seen.merge(&[1, 64, 1000]), 0);
    assert_eq!(seen.merge(&[2, 64]), 1);
    assert_eq!(seen.count(), 4);
}

#[test]
fn drain_buckets_counters_and_masks_the_engines() {
    // Counters past one word, so the tail is read too.
    let mut counters = vec![0u8; 19];
    counters[0] = 1;
    counters[3] = 3;
    counters[8] = 200;
    counters[9] = 5;
    counters[18] = 2;
    let before = counters.clone();
    let coverage = Coverage::of(&mut counters, &[9]);
    let mut features = Vec::with_capacity(coverage.total() + raw::VALUE_BITS);
    coverage.drain(&mut features, false);
    features.sort_unstable();
    assert_eq!(features, vec![0 << 3, (3 << 3) | 2, (8 << 3) | 7, (18 << 3) | 1]);
    drop(coverage);
    assert_eq!(counters, before, "drain only reads; run clears");
}

/// The drain reads a cache line at a time; this is the plain walk it must agree with, over
/// lengths with and without whole lines, words and a tail, and sparse and dense counters.
#[test]
fn drain_agrees_with_a_byte_at_a_time() {
    let mut random = Random::new(11);
    for len in [0usize, 1, 7, 8, 9, 63, 64, 65, 127, 128, 200, 1000, 4096 + 3] {
        for density in [0usize, 1, 8, 64] {
            let mut counters: Vec<u8> = (0..len)
                .map(|_| if density > 0 && random.below(64) < density { random.next() as u8 } else { 0 })
                .collect();
            let ignored: Vec<usize> = (0..len).filter(|_| random.below(16) == 0).collect();
            let mut want: Vec<u32> = counters
                .iter()
                .enumerate()
                .filter(|&(k, &c)| c != 0 && !ignored.contains(&k))
                .map(|(k, &c)| ((k as u32) << 3) | bucket(c) as u32)
                .collect();
            let coverage = Coverage::of(&mut counters, &ignored);
            let mut features = Vec::with_capacity(coverage.total() + raw::VALUE_BITS);
            coverage.drain(&mut features, false);
            features.sort_unstable();
            want.sort_unstable();
            assert_eq!(features, want, "{len} counters, density {density}");
        }
    }
}

/// The engine's work per run, for an input that reaches nothing new (nearly every run): pick
/// two corpus inputs, mutate into the reused buffer, clear, run, drain and merge. It allocates
/// nothing. `blaze_fuzz`'s counting allocator is this binary's global one (it can't also have
/// `blaze-alloc-count`'s), and its largest allocation is 0 exactly when nothing was allocated.
#[test]
fn a_run_that_keeps_nothing_allocates_nothing() {
    let mut counters = vec![0u8; 300];
    let coverage = Coverage::of(&mut counters, &[]);
    let mut seen = Seen::new((coverage.total() << 3) + raw::VALUE_BITS);
    let mut corpus = Corpus::new().unwrap();
    corpus.add(b"header 1234 MAGIC tail", 1).unwrap();
    corpus.add(b"another input to splice", 3).unwrap();
    let dictionary: &[&[u8]] = &[b"WORD", b"\xff\xff\xff\xff"];
    let mut mutator = Mutator::new(2, dictionary, OneRecent);
    let mut features = Vec::with_capacity(coverage.total() + raw::VALUE_BITS);
    let mut data = Vec::with_capacity(64);
    for run in 0..10_000 {
        let (largest, ()) = blaze_fuzz::largest_allocation(|| {
            data.clear();
            data.extend_from_slice(corpus.pick(&mut mutator.random));
            let other = corpus.pick(&mut mutator.random);
            mutator.mutate(&mut data, other, 64);
            features.clear();
            assert!(coverage.run(|_| {}, &data, 0));
            coverage.drain(&mut features, true);
            seen.merge(&features);
        });
        assert_eq!(largest, 0, "run {run} allocated (largest {largest} bytes)");
    }
}

#[test]
fn run_clears_whole_lines_and_the_tail() {
    for len in [0usize, 5, 64, 70, 130] {
        let mut random = Random::new(len as u64);
        let mut counters: Vec<u8> = (0..len).map(|_| if random.below(4) == 0 { 1 } else { 0 }).collect();
        let coverage = Coverage::of(&mut counters, &[]);
        assert!(coverage.run(|_| {}, b"", 0));
        drop(coverage);
        assert!(counters.iter().all(|&c| c == 0), "{len}: {counters:?}");
    }
}

#[test]
fn run_clears_first_and_reports_a_panic() {
    let mut counters = vec![7u8; 8];
    let coverage = Coverage::of(&mut counters, &[]);
    assert!(coverage.run(|_| {}, b"fine", 0));
    let mut features = Vec::with_capacity(coverage.total() + raw::VALUE_BITS);
    coverage.drain(&mut features, false);
    assert!(features.is_empty(), "run clears what came before it: {features:?}");
    assert!(!coverage.run(|data| assert!(data.is_empty(), "not empty"), b"x", 0));
    assert!(raw::current().is_none(), "no input is published between runs");
}

#[test]
fn a_build_without_coverage_says_which_flags() {
    match Coverage::new(&[]) {
        Err(e) => assert!(e.contains("-Zsanitizer-coverage=inline-8bit-counters,pc-table,trace-cmp"), "{e}"),
        Ok(coverage) => assert!(coverage.total() > 0),
    }
}

#[test]
fn mutations_stay_in_the_limit_and_repeat_with_the_seed() {
    let dictionary: &[&[u8]] = &[b"WORD", b"\xff\xff\xff\xff"];
    let start = b"header 1234 \x00\x01\x02\x03\x04\x05\x06\x07 tail".to_vec();
    let other = b"another input to splice".to_vec();
    let mut outputs = [Vec::new(), Vec::new()];
    for output in &mut outputs {
        let mut mutator = Mutator::new(7, dictionary, OneRecent);
        let mut data = start.clone();
        for _ in 0..5000 {
            mutator.mutate(&mut data, &other, 64);
            assert!(data.len() <= 64, "{} bytes", data.len());
            output.extend_from_slice(&data);
            if data.is_empty() {
                data = start.clone();
            }
        }
    }
    assert_eq!(outputs[0], outputs[1], "the same seed makes the same inputs");
}

#[test]
fn every_mutation_applies_somewhere() {
    let dictionary: &[&[u8]] = &[b"WORD"];
    let mut mutator = Mutator::new(1, dictionary, OneRecent);
    for mutation in MUTATIONS {
        let mut applied = false;
        for _ in 0..100 {
            let mut data = b"some 42 bytes with MAGIC and \x44\x33\x22\x11".to_vec();
            let before = data.clone();
            if mutator.apply(mutation, &mut data, b"other", 128) {
                assert!(data.len() <= 128, "{mutation:?}");
                applied |= data != before || mutation == Mutation::ShuffleBytes;
            }
        }
        assert!(applied, "{mutation:?} never applied");
    }
}

#[test]
fn recent_operands_replace_what_was_compared() {
    let mut mutator = Mutator::new(3, &[], OneRecent);
    let mut replaced = false;
    for _ in 0..200 {
        let mut data = b"xxMAGICxx".to_vec();
        assert!(mutator.apply(Mutation::RecentBytes, &mut data, &[], 64));
        replaced |= data == b"xxmagicxx";
    }
    assert!(replaced, "MAGIC in the input becomes the operand it was compared with");
    let mut found = false;
    for _ in 0..200 {
        let mut data = vec![0, 0x44, 0x33, 0x22, 0x11, 0];
        assert!(mutator.apply(Mutation::RecentCompare, &mut data, &[], 64));
        found |= data == [0, 0xef, 0xbe, 0xad, 0xde, 0];
    }
    assert!(found, "a little-endian operand in the input becomes the other");
}

#[test]
fn finding_an_operand_agrees_with_a_search_at_every_offset() {
    let linear = |data: &[u8], from: &[u8], start: usize| {
        let last = data.len() - from.len();
        (start..=last).chain(0..start).find(|&at| &data[at..at + from.len()] == from)
    };
    let mut random = Random::new(11);
    let mut found = 0;
    for _ in 0..20_000 {
        // Two letters, so words are found often, and found again before where the search starts.
        let data: Vec<u8> = (0..random.below(48)).map(|_| b'a' + random.below(2) as u8).collect();
        let from: Vec<u8> = (0..1 + random.below(6)).map(|_| b'a' + random.below(2) as u8).collect();
        if data.len() < from.len() {
            continue;
        }
        let start = random.below(data.len() - from.len() + 1);
        let want = linear(&data, &from, start);
        assert_eq!(mutate::find_from(&data, &from, start), want, "{from:?} in {data:?} from {start}");
        found += usize::from(want.is_some_and(|at| at < start));
    }
    assert!(found > 1000, "words found before the start only {found} times");
}

#[test]
fn a_byte_comparison_that_differs_is_kept_for_the_dictionary() {
    let a: [u8; raw::WORD] = std::array::from_fn(|i| 0x40 + i as u8);
    let mut b = a;
    b[raw::WORD - 1] ^= 0xff;
    raw::__sanitizer_weak_hook_memcmp(std::ptr::null(), a.as_ptr().cast(), b.as_ptr().cast(), raw::WORD, -1);
    assert!(
        (0..raw::TORC_WORDS).any(|slot| raw::recent_bytes(slot) == (a, b, raw::WORD)),
        "the operands' first {} bytes are in a slot",
        raw::WORD
    );
}

#[test]
fn a_case_repeats_with_the_input_and_one_byte_reaches_every_case() {
    for cases in [1, 2, 3, 5, 7, 14] {
        let mut data = b"input".to_vec();
        let mut reached = vec![false; cases];
        for last in 0..=u8::MAX {
            *data.last_mut().unwrap() = last;
            let case = crate::case(&data, cases);
            assert_eq!(crate::case(&data, cases), case, "the same input takes the same case");
            reached[case] = true;
        }
        assert!(reached.iter().all(|it| *it), "every one of {cases} cases is reached");
    }
    assert_eq!(crate::case(&[], 3), 0);
}

#[test]
fn decimal_digits_match_the_formatter() {
    let mut buffer = [0u8; 20];
    for value in [0u64, 1, 9, 10, 99, 100, 12345, u32::MAX as u64, u64::MAX] {
        assert_eq!(mutate::decimal(value, &mut buffer), value.to_string().as_bytes());
    }
}

#[test]
fn nothing_recent_is_no_mutation() {
    let mut mutator = Mutator::new(5, &[], NoRecent);
    let mut data = b"abc".to_vec();
    assert!(!mutator.apply(Mutation::RecentCompare, &mut data, &[], 64));
    assert!(!mutator.apply(Mutation::RecentBytes, &mut data, &[], 64));
    assert!(!mutator.apply(Mutation::Dictionary, &mut data, &[], 64));
    assert_eq!(data, b"abc");
}

#[test]
fn random_stays_below_its_bound() {
    let mut random = Random::new(0);
    for bound in [1usize, 2, 3, 10, 255, 1 << 40] {
        for _ in 0..1000 {
            assert!(random.below(bound) < bound);
        }
    }
    assert_eq!(random.below(0), 0);
}

#[test]
fn hex_round_trips_and_refuses_what_is_not() {
    let bytes: Vec<u8> = (0..=255).collect();
    assert_eq!(corpus::from_hex(&corpus::to_hex(&bytes)).unwrap(), bytes);
    assert_eq!(corpus::from_hex("de ad\nbe\tef").unwrap(), [0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(corpus::from_hex("").unwrap(), Vec::<u8>::new());
    assert!(corpus::from_hex("abc").is_err());
    assert!(corpus::from_hex("zz").is_err());
}

#[test]
fn the_corpus_picks_every_input_and_favors_the_new() {
    let mut corpus = Corpus::new().unwrap();
    corpus.add(b"a", 1).unwrap();
    corpus.add(b"b", 1).unwrap();
    corpus.add(b"c", 10).unwrap();
    let mut random = Random::new(9);
    let mut counts = [0usize; 3];
    for _ in 0..10_000 {
        counts[(corpus.pick(&mut random)[0] - b'a') as usize] += 1;
    }
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
    assert!(counts[2] > counts[0] + counts[1], "{counts:?}");
    assert_eq!((corpus.len(), corpus.bytes(), corpus.last()), (3, 3, &b"c"[..]));
}

#[test]
fn options_parse() {
    let parse = |args: &[&str]| Options::parse(args.iter().map(|it| it.to_string()));
    let fuzz = parse(&["fuzz", "--seconds", "5", "--seed", "3", "--no-value-profile"]).unwrap();
    assert!(matches!(fuzz.command, Command::Fuzz));
    assert_eq!((fuzz.seconds, fuzz.seed, fuzz.value_profile), (Some(5), Some(3), false));
    assert!(matches!(parse(&["run", "a.hex", "b.hex"]).unwrap().command, Command::Run(files) if files.len() == 2));
    assert!(matches!(parse(&["minimize", "a.hex"]).unwrap().command, Command::Minimize(_)));
    assert!(parse(&[]).is_err());
    assert!(parse(&["run"]).is_err());
    assert!(parse(&["fuzz", "a.hex"]).is_err());
    assert!(parse(&["fuzz", "--seconds"]).is_err());
    assert!(parse(&["fuzz", "--frobnicate"]).is_err());
    let jobs = parse(&["fuzz", "--jobs", "8"]).unwrap();
    assert_eq!((jobs.jobs, jobs.worker), (8, None));
    assert_eq!(parse(&["fuzz", "--worker", "3"]).unwrap().worker, Some(3));
    assert!(parse(&["fuzz", "--jobs", "0"]).is_err());
    assert!(matches!(parse(&["serve", "--timeout-ms", "50"]).unwrap().command, Command::Serve));
    assert!(parse(&["serve", "a.hex"]).is_err());
}

#[test]
fn kept_files_appear_whole_and_listings_skip_the_partial() {
    let dir = std::env::temp_dir().join(format!("blaze-fuzzer-corpus-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = corpus::write(&dir, "", b"input").unwrap();
    std::fs::write(dir.join(".1234-being-written.hex"), "00").unwrap();
    let names = corpus::names(&dir).unwrap();
    assert_eq!(names, vec![std::ffi::OsString::from(format!("{:016x}.hex", corpus::hash(b"input")))]);
    assert_eq!(path, dir.join(&names[0]));
    assert_eq!(corpus::key(&names[0]), corpus::hash(b"input"));
    assert_eq!(corpus::key("other.hex".as_ref()), corpus::hash(b"other.hex"));
    assert_eq!(corpus::read_dir(&dir).unwrap(), vec![b"input".to_vec()]);
    assert_eq!(corpus::names(&dir.join("missing")).unwrap(), Vec::<std::ffi::OsString>::new());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Keeping an input (the rare run that reaches something new) copies it into the corpus's
/// reserved regions and writes its file from reused scratch: no general-purpose allocation
/// once the scratch has grown to the longest input.
#[test]
fn keeping_an_input_allocates_nothing() {
    let dir = std::env::temp_dir().join(format!("blaze-fuzzer-keep-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut corpus = Corpus::new().unwrap();
    let mut scratch = corpus::Scratch::default();
    let mut input = vec![0u8; 200];
    scratch.write(&dir, "", &input).unwrap();
    for keep in 0..100u8 {
        input[0] = keep;
        let (largest, ()) = blaze_fuzz::largest_allocation(|| {
            corpus.add(&input, 1).unwrap();
            scratch.write(&dir, "", &input).unwrap();
        });
        assert_eq!(largest, 0, "keep {keep} allocated (largest {largest} bytes)");
    }
    assert_eq!((corpus.len(), corpus.bytes(), corpus.last()), (100, 20_000, &input[..]));
    assert_eq!(corpus::read(&dir.join(format!("{:016x}.hex", corpus::hash(&input)))).unwrap(), input);
    std::fs::remove_dir_all(&dir).unwrap();
}
