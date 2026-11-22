//! Piece S1a's proof. With the feature `edge-first` on (`mod on`): every drawn value lies in its
//! range, with a control that plants a table read unfiltered; every value the table and the range
//! promise inside a range is drawn over a few thousand seeded cases, with a control that plants a
//! table without the neighbour below each power of two; a search finds the one value 65536 of a
//! `u32` that the old draw does not; the calm tape still draws the simplest values; and
//! `Tape::int_in` is the edge choice followed by the old draw. With it off (`mod off`): three
//! recorded tapes and two more replay to the values and choices the draw logic gave before the
//! piece, read from that logic by hand, and `size` draws nothing.

#[cfg(feature = "edge-first")]
mod on {
    #![expect(
        blaze::error_debug,
        reason = "a test's failure message shows the values it compared as they are"
    )]

    use std::collections::BTreeSet;
    use std::fmt::Debug;

    use crate::edge_ints::{Rows, Width, in_range_planted, size};
    use crate::tape::{Int, Tape};
    use crate::{Broke, Config, Verdict, control, search};

    /// `Tape::int_in` over `lo..=hi` with `rows` in place of the table, on the ordered line:
    /// the edge choice, else the old draw. With `Rows::All` it is `int_in` choice for choice
    /// (`int_in_is_the_edge_choice_then_the_old_draw`).
    fn drawn<T: Int>(tape: &Tape, lo: T, hi: T, rows: Rows, label: &'static str) -> u128 {
        let (lo, hi) = (lo.to_ordered(), hi.to_ordered());
        match in_range_planted(tape, lo, hi, Width::of::<T>(), rows, label) {
            Some(edge) => edge,
            None => tape.ordered_in(lo, hi, label),
        }
    }

    /// A generated range of `T` (two draws of `int`, so its ends are often edges themselves),
    /// then eight values drawn in it under `rows`, each checked against its ends on the ordered
    /// line (so a value past the type cannot wrap back inside).
    fn stays_inside<T: Int + PartialOrd + Debug>(tape: &Tape, rows: Rows) -> Result<(), Broke> {
        let (a, b) = (tape.int::<T>("a"), tape.int::<T>("b"));
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        for _ in 0..8 {
            let value = drawn(tape, lo, hi, rows, "value");
            if value < lo.to_ordered() || hi.to_ordered() < value {
                return Err(Broke::Oracle(format!(
                    "drew the ordered value {value:#x}, outside {lo:?}..={hi:?} of {}",
                    core::any::type_name::<T>()
                )));
            }
        }
        Ok(())
    }

    /// [`stays_inside`] for every width.
    fn every_width_stays_inside(tape: &Tape, rows: Rows) -> Result<(), Broke> {
        stays_inside::<u8>(tape, rows)?;
        stays_inside::<u16>(tape, rows)?;
        stays_inside::<u32>(tape, rows)?;
        stays_inside::<u64>(tape, rows)?;
        stays_inside::<u128>(tape, rows)?;
        stays_inside::<usize>(tape, rows)?;
        stays_inside::<i8>(tape, rows)?;
        stays_inside::<i16>(tape, rows)?;
        stays_inside::<i32>(tape, rows)?;
        stays_inside::<i64>(tape, rows)?;
        stays_inside::<i128>(tape, rows)?;
        stays_inside::<isize>(tape, rows)
    }

    /// (a), in range. Prints `every width: 2000 cases of generated ranges, every value inside`;
    /// the control's planted table read unfiltered is caught saying "outside".
    #[test]
    fn every_drawn_value_lies_in_its_range() {
        control(
            Config::cases(2000),
            Rows::All,
            Rows::Unfiltered,
            "outside",
            every_width_stays_inside,
        );
        println!("every width: 2000 cases of generated ranges, every value inside");
    }

    /// The values the spec promises inside `lo..=hi` (test.md, S1a), written here from its words
    /// and not from the table's code, ordered: 0, 1, -1; the type's MIN, MIN+1, MAX-1, MAX; every
    /// power of two with its two neighbours, of both signs (which holds the narrower widths' ends,
    /// 127, 128, 255, 256, -128, -129, ..); the range's ends, each end's neighbour, and its middle
    /// with its neighbours.
    fn promised<T: Int>(lo: T, hi: T) -> BTreeSet<u128> {
        let width = Width::of::<T>();
        let (lo, hi) = (lo.to_ordered(), hi.to_ordered());
        let zero: u128 = if width.signed { 1 << 127 } else { 0 };
        let (min, max) = if width.signed {
            (zero - (1 << (width.bits - 1)), zero + ((1 << (width.bits - 1)) - 1))
        } else {
            (0, u128::MAX >> (128 - width.bits))
        };
        let mut numbers: Vec<Option<u128>> = Vec::new();
        let mut signed = |magnitude: Option<u128>| {
            numbers.push(magnitude.and_then(|m| zero.checked_add(m)));
            numbers.push(magnitude.and_then(|m| zero.checked_sub(m)));
        };
        signed(Some(0));
        signed(Some(1));
        for k in 0..128 {
            let power = 1u128 << k;
            signed(Some(power - 1));
            signed(Some(power));
            signed(power.checked_add(1));
        }
        let middle = lo + (hi - lo) / 2;
        numbers.extend([
            Some(min),
            min.checked_add(1),
            max.checked_sub(1),
            Some(max),
            Some(lo),
            lo.checked_add(1),
            hi.checked_sub(1),
            Some(hi),
            Some(middle),
            middle.checked_sub(1),
            middle.checked_add(1),
        ]);
        numbers.into_iter().flatten().filter(|&value| lo <= value && value <= hi).collect()
    }

    /// The seeded cases of a coverage run, and the values each draws in one range.
    const COVER_CASES: u64 = 3000;
    /// See [`COVER_CASES`].
    const COVER_DRAWS: u32 = 48;

    /// A promised value no case drew.
    #[derive(Debug, PartialEq, Eq)]
    struct Missing {
        range: String,
        value: String,
    }

    /// Draws [`COVER_DRAWS`] values in `lo..=hi` under `rows` in each of [`COVER_CASES`] seeded
    /// cases; the count of distinct promised values hit, or the least promised value missed.
    fn covered<T: Int + Debug>(lo: T, hi: T, rows: Rows) -> Result<usize, Missing> {
        let promised = promised(lo, hi);
        let mut hit = BTreeSet::new();
        let (ordered_lo, ordered_hi) = (lo.to_ordered(), hi.to_ordered());
        for seed in 0..COVER_CASES {
            let tape = Tape::seeded(seed);
            for _ in 0..COVER_DRAWS {
                // Only the edge choice's values count: the old draw gives small values by itself.
                let edge =
                    in_range_planted(&tape, ordered_lo, ordered_hi, Width::of::<T>(), rows, "value");
                if let Some(value) = edge.filter(|value| promised.contains(value)) {
                    hit.insert(value);
                }
            }
        }
        match promised.iter().find(|value| !hit.contains(*value)) {
            Some(&value) => Err(Missing {
                range: format!("{lo:?}..={hi:?}"),
                value: format!("{:?}", T::from_ordered(value)),
            }),
            None => Ok(hit.len()),
        }
    }

    /// `covered` with the whole table, panicking on a miss, printing the count.
    fn covers<T: Int + Debug>(lo: T, hi: T) -> usize {
        match covered(lo, hi, Rows::All) {
            Ok(hit) => {
                println!(
                    "{}: {lo:?}..={hi:?}: {hit} distinct edge values hit, every one promised",
                    core::any::type_name::<T>()
                );
                hit
            }
            Err(missing) => panic!("never drew a promised edge value: {missing:?}"),
        }
    }

    /// (a), coverage. Prints one line per range, e.g. `u8: 0..=255: <n> distinct edge values
    /// hit, every one promised`, then `total: <n> distinct edge values over 23 ranges`.
    #[test]
    fn every_promised_edge_value_inside_a_range_is_drawn() {
        let total = covers(u8::MIN, u8::MAX)
            + covers(u16::MIN, u16::MAX)
            + covers(u32::MIN, u32::MAX)
            + covers(u64::MIN, u64::MAX)
            + covers(u128::MIN, u128::MAX)
            + covers(usize::MIN, usize::MAX)
            + covers(i8::MIN, i8::MAX)
            + covers(i16::MIN, i16::MAX)
            + covers(i32::MIN, i32::MAX)
            + covers(i64::MIN, i64::MAX)
            + covers(i128::MIN, i128::MAX)
            + covers(isize::MIN, isize::MAX)
            + covers(3u8, 200)
            + covers(0u8, 0)
            + covers(-300i16, 300)
            + covers(100u32, 70_000)
            + covers(-(1i64 << 40), 5)
            + covers(0usize, 4097)
            + covers((1u128 << 64) - 3, 1 << 100)
            + covers(i128::MIN, -1)
            + covers(-1i32, 1)
            + covers(isize::MIN + 1, isize::MIN + 300)
            + covers(u64::MAX - 1000, u64::MAX);
        println!("total: {total} distinct edge values over 23 ranges");
    }

    /// (b): the planted table without the neighbour below each power of two loses 7, the least
    /// such value no other row holds, and the coverage oracle of (a) says so. Prints
    /// `caught: Missing { range: "0..=4294967295", value: "7" }`.
    #[test]
    fn the_coverage_catches_a_table_without_the_neighbour_below_a_power() {
        let caught = covered(u32::MIN, u32::MAX, Rows::WithoutBelowPowers);
        let Err(missing) = caught else {
            panic!("the coverage oracle missed the planted table: {caught:?}");
        };
        assert_eq!(missing, Missing { range: "0..=4294967295".into(), value: "7".into() });
        println!("caught: {missing:?}");
    }

    /// (c): a bug that needs exactly `x == 65536` in a `u32` drawn from the full range is found in
    /// the default 256 cases with the edge values (by the table's weights, a case draws it about
    /// one time in 76, so 256 cases miss it about one time in 30: the pinned seeds are to verify
    /// by a test), and is not found by the old draw alone (about 1.4e-7 a case). Prints `found
    /// x == 65536 at case <k> of 256 (seed Some(<s>)); the old draw passed 256 cases`.
    #[test]
    fn a_search_finds_the_one_value_65536_in_a_u32() {
        let needs = |x: u32| {
            if x == 65536 { Err(Broke::Oracle(format!("x is {x}"))) } else { Ok(()) }
        };
        let edge = search(Config::DEFAULT, |tape| needs(tape.int::<u32>("x")));
        let Verdict::Failed(failed) = &edge else {
            panic!("the edge values never drew 65536 in {} cases: {edge:?}", Config::DEFAULT.cases);
        };
        assert!(failed.failure.text.contains("x is 65536"), "{failed:?}");
        let old = search(Config::DEFAULT, |tape| {
            needs(u32::from_ordered(tape.ordered_in(0, u32::MAX.to_ordered(), "x")))
        });
        let Verdict::Passed(passed) = &old else {
            panic!("the old draw found 65536 as well, so this test shows nothing: {old:?}");
        };
        println!(
            "found x == 65536 at case {} of {} (seed {:?}); the old draw passed {} cases",
            failed.case, failed.cases, failed.seed, passed.cases
        );
    }

    /// The calm tape draws the simplest values still: 0 for `int`, a range's start for `int_in`,
    /// `None` from `size`. Prints `calm: 0, 0, 5, -7, None`.
    #[test]
    fn the_calm_tape_still_draws_the_simplest_value() {
        let tape = Tape::calm();
        let (a, b) = (tape.int::<i64>("a"), tape.int::<u128>("b"));
        let (c, d) = (tape.int_in(5u32..=1_000_000, "c"), tape.int_in(-7i128..=7, "d"));
        let e = size(&tape, 3, 10, "e");
        assert_eq!((a, b, c, d, e), (0, 0, 5, -7, None));
        println!("calm: {a}, {b}, {c}, {d}, {e:?}");
    }

    /// `int_in` is the edge choice then the old draw, choice for choice, so the tests above,
    /// which draw through [`drawn`], speak of `int_in`. Prints `int_in equals drawn over 500
    /// seeds and 6 ranges`.
    #[test]
    fn int_in_is_the_edge_choice_then_the_old_draw() {
        fn same<T: Int + Debug>(seed: u64, lo: T, hi: T) {
            let (a, b) = (Tape::seeded(seed), Tape::seeded(seed));
            for _ in 0..16 {
                let ours = drawn(&a, lo, hi, Rows::All, "x");
                let theirs = b.int_in(lo..=hi, "x").to_ordered();
                assert_eq!(ours, theirs, "seed {seed}, {lo:?}..={hi:?}");
            }
        }
        for seed in 0..500 {
            same(seed, u8::MIN, u8::MAX);
            same(seed, 10u32, 1 << 30);
            same(seed, i64::MIN, i64::MAX);
            same(seed, -5i16, 5);
            same(seed, u128::MIN, u128::MAX);
            same(seed, i128::MIN, 0);
        }
        println!("int_in equals drawn over 500 seeds and 6 ranges");
    }

    /// Every edge size lies in `min..=max`, about one list in four takes one, and each promised
    /// size inside is drawn: 0, 1, 2, the least size and one more, the cap and the cap less one
    /// (a cap of at most 4097), the powers of two from 4 to 4096 with their neighbours, and for
    /// a cap past 4097 (up to 2^20) the cap and the cap less one, rarely. Prints, per range,
    /// `sizes <min>..=<max>: <edges> edges in 4000 lists, <n> distinct sizes, every one
    /// promised`.
    #[test]
    fn every_edge_size_lies_in_its_sizes_and_each_is_drawn() {
        fn promised_sizes(min: usize, max: usize) -> BTreeSet<usize> {
            let mut sizes = vec![Some(0), Some(1), Some(2), Some(min), min.checked_add(1)];
            // A cap of at most 4097 commonly, one up to 2^20 rarely: both promised.
            if max <= 1 << 20 {
                sizes.extend([Some(max), max.checked_sub(1)]);
            }
            for k in 2..=12 {
                sizes.extend([Some((1usize << k) - 1), Some(1 << k), Some((1 << k) + 1)]);
            }
            sizes.into_iter().flatten().filter(|&n| min <= n && n <= max).collect()
        }
        for (min, max) in [(0, 0), (0, 10), (3, 100), (0, 5000), (1, 1 << 20), (0, usize::MAX)] {
            let promised = promised_sizes(min, max);
            let mut hit = BTreeSet::new();
            let mut edges = 0u32;
            for seed in 0..4000 {
                let tape = Tape::seeded(seed);
                if let Some(n) = size(&tape, min, max, "size") {
                    assert!(min <= n && n <= max, "size {n} outside {min}..={max}");
                    edges += 1;
                    hit.insert(n);
                }
            }
            assert!((700..=1300).contains(&edges), "{edges} edge sizes in 4000 lists, not about a quarter");
            let missed: Vec<_> = promised.difference(&hit).collect();
            assert!(missed.is_empty(), "sizes {min}..={max}: never drew {missed:?}");
            assert!(hit.is_subset(&promised), "sizes {min}..={max}: drew unpromised {:?}", hit.difference(&promised).collect::<Vec<_>>());
            println!("sizes {min}..={max}: {edges} edges in 4000 lists, {} distinct sizes, every one promised", hit.len());
        }
    }
}

#[cfg(not(feature = "edge-first"))]
mod off {
    use crate::edge_ints::size;
    use crate::tape::{Record, Tape};

    /// The choices a tape drew, as (value, most), and how many prefix values were lowered.
    fn drew(tape: &Tape) -> (Vec<(u64, u64)>, u32) {
        let mut record = Record::new();
        tape.finish(&mut record);
        (record.choices.iter().map(|choice| (choice.value, choice.most)).collect(), record.clamped)
    }

    /// (d): with the feature off, recorded tapes replay to the values and choices the draw logic
    /// gave before the piece (read by hand from `int_in` and the zigzag of the signed
    /// `draw_int` as they stood at the 15:45 snapshot):
    /// - `[5]`, `int::<u8>`: 256 values, uniform: `below(256)` gives 5.
    /// - `[1, 3, 7]`, `int_in(0u32..=1000)`: kind 1 (a small offset from the start), a bit
    ///   length of 1000 of 10 so `below(11)` gives 3 bits, `below(8)` gives 7: the value 7.
    /// - `[5, 2, 4]`, `int::<i32>`: the zigzag `u32` draw: kind 5 (back from the end), 32 bits so
    ///   `below(33)` gives 2, `below(4)` lowers 4 to 3 (one clamped): 2^32-1-3 = 4294967292,
    ///   zigzag-decoded to 2147483646.
    /// - `[6, 12345]`, `int::<u64>`: kind 6 (uniform) over all 2^64 values, one raw draw: 12345.
    /// - `[3]`, `int::<i8>`: the zigzag `u8` draw 3, decoded to -2.
    ///
    /// Prints `replayed as before: 5, 7, 2147483646, 12345, -2`.
    #[test]
    fn recorded_tapes_replay_as_before_with_the_feature_off() {
        let tape = Tape::replay(&[5]);
        let a = tape.int::<u8>("a");
        assert_eq!((a, drew(&tape)), (5, (vec![(5, 255)], 0)));

        let tape = Tape::replay(&[1, 3, 7]);
        let b = tape.int_in(0u32..=1000, "b");
        assert_eq!((b, drew(&tape)), (7, (vec![(1, 7), (3, 10), (7, 7)], 0)));

        let tape = Tape::replay(&[5, 2, 4]);
        let c = tape.int::<i32>("c");
        assert_eq!((c, drew(&tape)), (2_147_483_646, (vec![(5, 7), (2, 32), (3, 3)], 1)));

        let tape = Tape::replay(&[6, 12345]);
        let d = tape.int::<u64>("d");
        assert_eq!((d, drew(&tape)), (12345, (vec![(6, 7), (12345, u64::MAX)], 0)));

        let tape = Tape::replay(&[3]);
        let e = tape.int::<i8>("e");
        assert_eq!((e, drew(&tape)), (-2, (vec![(3, 255)], 0)));

        println!("replayed as before: {a}, {b}, {c}, {d}, {e}");
    }

    /// With the feature off `size` answers `None` and draws nothing. Prints `size off: None, 0
    /// choices`.
    #[test]
    fn size_draws_nothing_with_the_feature_off() {
        let tape = Tape::seeded(1);
        let recorded = Tape::calm();
        assert_eq!(size(&tape, 0, 100, "size"), None);
        assert_eq!(size(&recorded, 0, 100, "size"), None);
        let (choices, _) = drew(&recorded);
        assert!(choices.is_empty(), "drew {choices:?}");
        println!("size off: None, {} choices", choices.len());
    }
}
