//! The shapes of lists and operation sequences (test.md S2), with `edge-first` on; and, with it
//! off, `Tape::vec` choice for choice as it was.

#[cfg(feature = "edge-first")]
mod edge_first {
    use std::alloc::Global;

    use blaze_rng::SplitMix64;

    use crate::search::{Broke, Config, Origin, Verdict};
    use crate::shape::{Elements, Repetition, Swarm, vec_shaped};
    use crate::tape::{Record, Tail, Tape};

    /// Seeded cases the share tests run.
    const CASES: u32 = 512;

    /// A recorded tape on `seed`'s stream.
    fn seeded(seed: u64) -> Tape {
        Tape::new(&[], Tail::Seeded(SplitMix64::new(seed)), Tape::MOST_CHOICES)
    }

    fn has_equal_pair(keys: &[u64]) -> bool {
        keys.iter().enumerate().any(|(at, key)| keys[at + 1..].contains(key))
    }

    /// Lists of `u64` keys (2^64 values, so two equal by chance about never) over `CASES`
    /// seeded cases: how many hold an equal pair.
    fn equal_pair_share(repetition: Repetition) -> u32 {
        let mut with_pair = 0;
        for seed in 0..u64::from(CASES) {
            let tape = seeded(seed);
            let keys = vec_shaped(
                &tape,
                0..=64,
                "keys",
                |tape| tape.next_u64("key"),
                Elements::Free,
                repetition,
            );
            if has_equal_pair(&keys) {
                with_pair += 1;
            }
        }
        with_pair
    }

    /// The oracle: a quarter of the cases at least. Expected about 0.6 with replay (one element
    /// in eight an equal replay, lists about sixteen long): the threshold is to verify by this
    /// test's printed line.
    fn enough_pairs(with_pair: u32) -> bool {
        with_pair * 4 >= CASES
    }

    /// Prints `lists with an equal pair: <n> of 512 cases` (n at least 128; about 300 expected,
    /// to verify) and `with replay switched off (planted): 0 of 512 cases`.
    #[test]
    fn lists_of_wide_keys_repeat_a_key_in_a_large_share_of_cases() {
        let with_pair = equal_pair_share(Repetition::On);
        println!("lists with an equal pair: {with_pair} of {CASES} cases");
        assert!(
            enough_pairs(with_pair),
            "only {with_pair} of {CASES} lists of `u64` keys held an equal pair: repetition is too rare"
        );
        let planted = equal_pair_share(Repetition::Off);
        println!("with replay switched off (planted): {planted} of {CASES} cases");
        assert!(
            !enough_pairs(planted),
            "the oracle missed replay switched off: {planted} of {CASES} lists still held an equal pair"
        );
    }

    const KINDS: [&str; 3] = ["insert", "remove", "get"];

    /// What `subject` draws: nested lists, a swarm and its operations.
    #[derive(Debug, PartialEq, Eq)]
    struct Drawn {
        lists: Vec<(u64, Vec<u64>)>,
        on: [bool; 3],
        ops: Vec<usize>,
    }

    fn draw_all(subject: &Tape) -> Drawn {
        let lists = subject.vec(0..=32, "keys", |tape| {
            (tape.next_u64("key"), tape.vec(0..=3, "inner", |tape| tape.below(4, "small")))
        });
        let swarm = subject.swarm(&KINDS, "kinds");
        let ops = subject.vec(0..=16, "ops", |tape| tape.op(&swarm, "op"));
        Drawn { lists, on: [swarm.is_on(0), swarm.is_on(1), swarm.is_on(2)], ops }
    }

    const REPEATED: &str = "a list with an equal pair of outer keys";

    /// A seeded tape's record, replayed as a prefix on a calm tail, gives the same nested lists,
    /// mask and operations. Must reach a case whose list repeated a key, so replay was exercised.
    /// Prints the check's pass line (`... 200 cases passed ...`).
    #[test]
    fn a_recorded_tape_replays_to_the_same_lists_mask_and_operations() {
        crate::check(Config::cases(200).must_reach(&[REPEATED]), |tape| {
            let seed = tape.next_u64("seed");
            let subject = seeded(seed);
            let first = draw_all(&subject);
            let keys: Vec<u64> = first.lists.iter().map(|(key, _)| *key).collect();
            if has_equal_pair(&keys) {
                tape.reached(REPEATED);
            }
            let mut record = Record::new();
            subject.finish(&mut record);
            let mut values = Vec::new_in(Global);
            record.values_into(&mut values);
            let again = draw_all(&Tape::replay(&values));
            if again == first {
                Ok(())
            } else {
                Err(Broke::Oracle(format!(
                    "seed {seed}: the recorded tape replayed to {again:?}, the seeded tape gave {first:?}"
                )))
            }
        });
    }

    /// A failure that needs a repeated key: found, shrunk to a smaller tape whose list still
    /// repeats one (the later element's span keeps its own copy of the choices). Prints
    /// `shrunk from <first> to <least> choices: keys [0, 0]` (the least list to verify; [0, 0]
    /// expected: two elements, the second fresh and calm).
    #[test]
    fn a_failure_that_needs_a_repeat_shrinks_to_a_smaller_tape_that_still_repeats() {
        let keys_of = |tape: &Tape| tape.vec(0..=64, "keys", |tape| tape.next_u64("key"));
        let verdict = crate::search(Config::DEFAULT, |tape| {
            let keys = keys_of(tape);
            if has_equal_pair(&keys) {
                Err(Broke::Oracle(format!("an equal pair in {keys:?}")))
            } else {
                Ok(())
            }
        });
        let Verdict::Failed(failed) = &verdict else {
            panic!("no case of {} repeated a key: {verdict:?}", Config::DEFAULT.cases)
        };
        let mut values = Vec::new_in(Global);
        failed.least.values_into(&mut values);
        let keys = keys_of(&Tape::replay(&values));
        let first = usize::try_from(failed.first_len).expect("a `u32` fits a 64-bit `usize`");
        println!(
            "shrunk from {first} to {} choices: keys {keys:?}",
            failed.least.choices.len()
        );
        assert!(has_equal_pair(&keys), "the least tape {values:?} lost the repeat: {keys:?}");
        assert!(
            failed.least.choices.len() < first,
            "the least tape ({} choices) is not smaller than the first failure ({first})",
            failed.least.choices.len()
        );
    }

    /// The map under test: it counts inserts of a key and a remove takes one away, so a key
    /// inserted twice and removed once is still there, where a map says it is gone.
    #[derive(Debug, Default)]
    struct CountingMap {
        entries: Vec<(u64, u32)>,
    }

    impl CountingMap {
        fn insert(&mut self, key: u64) {
            match self.entries.iter_mut().find(|(at, _)| *at == key) {
                Some((_, count)) => *count += 1,
                None => self.entries.push((key, 1)),
            }
        }

        fn remove(&mut self, key: u64) {
            if let Some(found) = self.entries.iter().position(|(at, _)| *at == key) {
                self.entries[found].1 -= 1;
                if self.entries[found].1 == 0 {
                    self.entries.swap_remove(found);
                }
            }
        }

        fn contains(&self, key: u64) -> bool {
            self.entries.iter().any(|(at, _)| *at == key)
        }
    }

    /// Operations drawn key first, kind last, so a neighbour replay of an insert is a remove of
    /// the same key. The oracle: after each operation the map holds the key iff the model does.
    fn map_case(tape: &Tape, repetition: Repetition) -> Result<(), Broke> {
        let swarm = tape.swarm(&KINDS, "map ops");
        let ops = vec_shaped(
            tape,
            0..=64,
            "ops",
            |tape| {
                let key = tape.next_u64("key");
                (tape.op(&swarm, "op"), key)
            },
            Elements::Free,
            repetition,
        );
        let mut map = CountingMap::default();
        let mut model: Vec<u64> = Vec::new_in(Global);
        for (at, &(kind, key)) in ops.iter().enumerate() {
            match kind {
                0 => {
                    map.insert(key);
                    if !model.contains(&key) {
                        model.push(key);
                    }
                }
                1 => {
                    map.remove(key);
                    model.retain(|held| *held != key);
                }
                _ => {}
            }
            if map.contains(key) != model.contains(&key) {
                let names: Vec<String> = ops[..=at]
                    .iter()
                    .map(|&(kind, key)| format!("{} {key}", KINDS[kind]))
                    .collect();
                return Err(Broke::Oracle(format!(
                    "key {key} is in the map: {}, in the model: {}, after [{}]",
                    map.contains(key),
                    model.contains(&key),
                    names.join(", ")
                )));
            }
        }
        Ok(())
    }

    /// Found within the default 256 cases (to verify by this test); prints
    /// `found at case <k> of 256: key 0 is in the map: true, in the model: false, after [insert
    /// 0, insert 0, remove 0]` (the shrunk case; to verify), and with replay switched off
    /// (planted) the bug is never found: `replay off: passed`.
    #[test]
    fn a_map_bug_that_needs_one_key_inserted_twice_then_removed_is_found() {
        let verdict = crate::search(Config::DEFAULT, |tape| map_case(tape, Repetition::On));
        let Verdict::Failed(failed) = &verdict else {
            panic!("the bug that needs a key inserted twice was not found: {verdict:?}")
        };
        println!("found at case {} of {}: {}", failed.case, failed.cases, failed.failure.text);
        assert_eq!(failed.failure.origin, Origin::Oracle, "{:?}", failed.failure);
        assert!(failed.failure.text.contains("is in the map"), "{}", failed.failure.text);
        let planted = crate::search(Config::DEFAULT, |tape| map_case(tape, Repetition::Off));
        assert!(
            matches!(planted, Verdict::Passed(_)),
            "with replay switched off the bug was still found, so the catch above is not repetition's: {planted:?}"
        );
        println!("replay off: passed");
    }

    fn mask(swarm: &Swarm) -> usize {
        (0..KINDS.len()).filter(|&kind| swarm.is_on(kind)).map(|kind| 1 << kind).sum()
    }

    /// Over 512 seeded cases: never the empty set; every single-kind set and the full set seen;
    /// each off kind, and only those, in the notes; `op` gives only kinds that are on. Calm: all
    /// on, and the first operation is the first kind. Prints `swarm masks over 512 seeded cases:
    /// [0, n1, ..., n7]` (index = mask; index 0 must be 0).
    #[test]
    fn a_swarm_is_never_empty_reaches_every_single_kind_and_the_full_set_and_notes_what_is_off() {
        let mut seen = [0u32; 8];
        for seed in 0..u64::from(CASES) {
            let tape = seeded(seed);
            let swarm = tape.swarm(&KINDS, "kinds");
            let mask = mask(&swarm);
            assert_ne!(mask, 0, "seed {seed}: the swarm has no kind on");
            assert_eq!(swarm.on_count(), usize::try_from(mask.count_ones()).expect("fits"));
            seen[mask] += 1;
            for _ in 0..8 {
                let kind = tape.op(&swarm, "op");
                assert!(swarm.is_on(kind), "seed {seed}: `op` gave {kind}, which is off");
            }
            let mut record = Record::new();
            tape.finish(&mut record);
            for (kind, name) in KINDS.iter().enumerate() {
                let line = format!("`{name}` is off this case");
                let noted = record.notes.iter().any(|note| note.text.contains(&line));
                assert_eq!(
                    noted,
                    !swarm.is_on(kind),
                    "seed {seed}: kind `{name}` on: {}, noted off: {noted}; notes {:?}",
                    swarm.is_on(kind),
                    record.notes
                );
            }
        }
        println!("swarm masks over {CASES} seeded cases: {seen:?}");
        for single in [1, 2, 4] {
            assert!(seen[single] > 0, "the single-kind set {single:#b} never came: {seen:?}");
        }
        assert!(seen[7] > 0, "the full set never came: {seen:?}");
        let calm = Tape::calm();
        let swarm = calm.swarm(&KINDS, "kinds");
        assert_eq!(mask(&swarm), 7, "the calm swarm is not all on");
        assert_eq!(calm.op(&swarm, "op"), 0, "the calm operation is not the first kind");
    }
}

#[cfg(not(feature = "edge-first"))]
mod today {
    use crate::tape::{Record, Tape};

    fn finished(tape: &Tape) -> Record {
        let mut record = Record::new();
        tape.finish(&mut record);
        record
    }

    /// Three pinned tapes give the lists `vec` gave before S2, choice for choice (feature off).
    /// - `[1, 5, 1, 7]` over `0..=10` of `below(100)`: more, 5, more, 7, a calm stop: [5, 7] in 5
    ///   choices and 2 spans.
    /// - `[3, 4]` over `2..=3` of `below(10)`: two forced elements, then a calm stop: [3, 4] in 3
    ///   choices.
    /// - `[1, 50, 1, 2, 1, 1, 1]` over `0..=3` of `below(10)`: 50 lowered to 9, then at `max` no
    ///   stop is drawn: [9, 2, 1] in 6 choices, 1 clamped, 1 unused.
    #[test]
    fn three_pinned_tapes_give_the_lists_vec_gave_before() {
        let tape = Tape::replay(&[1, 5, 1, 7]);
        let list = tape.vec(0..=10, "list", |tape| tape.below(100, "x"));
        assert_eq!(list, [5, 7]);
        let record = finished(&tape);
        assert_eq!((record.choices.len(), record.spans.len()), (5, 2));

        let tape = Tape::replay(&[3, 4]);
        let list = tape.vec(2..=3, "list", |tape| tape.below(10, "x"));
        assert_eq!(list, [3, 4]);
        assert_eq!(finished(&tape).choices.len(), 3);

        let tape = Tape::replay(&[1, 50, 1, 2, 1, 1, 1]);
        let list = tape.vec(0..=3, "list", |tape| tape.below(10, "x"));
        assert_eq!(list, [9, 2, 1]);
        let record = finished(&tape);
        assert_eq!((record.choices.len(), record.clamped, record.unused), (6, 1, 1));
    }
}
