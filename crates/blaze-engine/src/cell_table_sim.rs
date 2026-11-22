use std::format;

use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Kind, Name};
use blaze_id::ContentId;
use blaze_sim::{Broke, Config, Tape, check, control};

use super::CellTable;
use crate::engine::{Answer, CellId, Gen, Plant, SMap, View};
use crate::overlay::Overlay;
use crate::sim::{alloc, oracle};

type Value = Option<(ContentId, Gen)>;

// The first six have the last slot as their home, even after growth. The first four
// differ only in the kind, the high selector word, or the last subject byte. The next
// two change a pair of tag inputs whose differences cancel out.
fn key(seed: u64, i: u64) -> CellId {
    let mut subject = [0; 32];
    let mut selector = [0; 16];
    let mut kind = Kind::CrateScope;
    if i < 6 {
        subject[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        subject[24..32].copy_from_slice(&seed.to_le_bytes());
        match i {
            1 => kind = Kind::Signature,
            2 => selector[15] = 1,
            3 => subject[31] ^= 1,
            4 => {
                subject[0] ^= 1;
                selector[0] ^= 1;
            }
            5 => {
                subject[8] ^= 1;
                subject[2] ^= 0x80;
            }
            _ => {}
        }
    } else {
        let input = (u128::from(seed) << 64) | u128::from(i);
        let digest = ContentId::of("blaze engine cell table simulation", &input.to_le_bytes());
        // Many selectors under one definer, and many distinct definers with a fixed selector.
        if i % 2 == 0 {
            subject[..8].copy_from_slice(&seed.to_le_bytes());
            selector[..8].copy_from_slice(&digest.0[..8]);
            selector[8..].copy_from_slice(&i.to_le_bytes());
        } else {
            subject = digest.0;
            subject[16..24].copy_from_slice(&i.to_le_bytes());
        }
    }
    CellId { of: Name::new(kind, ContentId(subject)), cell: Cell(selector) }
}

#[derive(Clone, Copy, Debug)]
enum Compare {
    FullKey,
    TagOnly,
}

const DIFFERS: &str = "cell lookup differs from the tree";

fn compare(table: &CellTable<'_, Value>, tree: &SMap<'_, CellId, Value>, id: CellId, how: Compare) -> Result<(), Broke> {
    let got = match how {
        Compare::FullKey => table.get(&id),
        Compare::TagOnly => table.lookup::<true>(&id),
    };
    let want = tree.get(&id);
    oracle(got == want, || format!("{DIFFERS} for {id:?}: got {got:?}, expected {want:?}"))
}

fn exercise(tape: &Tape, sizes: &[usize], how: Compare) -> Result<(), Broke> {
    let seed = tape.next_u64("cell set");
    let reverse = tape.coin("insert in reverse");
    for &size in sizes {
        let serial = Gen(1);
        let mut overlay = Overlay::new_in(Answer::new_in(alloc()), serial, Plant::None, alloc()).expect("the empty answer opens");
        let mut tree = SMap::new_in(alloc());
        for step in 0..size {
            let i = (if reverse { size - 1 - step } else { step }) as u64;
            let id = key(seed, i);
            let mut digest = id.of.subject;
            digest.0[24..32].copy_from_slice(&i.to_le_bytes());
            let digest = (i % 5 != 0).then_some(digest);
            overlay.set_cell(id, digest);
            tree.insert(id, digest.map(|digest| (digest, serial)));
        }
        let view = View::from_overlay(&overlay);
        let missing = key(seed, size as u64 + 6);
        assert_eq!(view.cell(&missing), ABSENT);
        for (id, value) in tree.iter() {
            assert_eq!(view.cell(id), value.map_or(ABSENT, |(digest, _)| digest));
            let mut absent = *id;
            absent.of.kind = Kind::Lint;
            assert_eq!(view.cell(&absent), ABSENT);
        }
        let (_, work) = overlay.into_parts();
        let mut table = work.cells;
        assert_eq!(table.len(), size);
        assert!(size == 0 || (table.slots.len().is_power_of_two() && size <= table.slots.len() / 2));
        // An ordinary miss even for the empty table. For every key also ask for an absent
        // kind with exactly the same tag and a miss with different bucket bits.
        compare(&table, &tree, missing, how)?;
        for id in tree.keys() {
            compare(&table, &tree, *id, how)?;
            let mut absent = *id;
            absent.of.kind = Kind::Lint;
            compare(&table, &tree, absent, how)?;
            absent.cell.0[0] ^= 1;
            compare(&table, &tree, absent, how)?;
        }
        assert!(table.iter().eq(tree.iter()));
        // Move a cell again or retire it. Removal repairs wrapped probe chains and can
        // relocate the last dense row. A subsequent insertion may reuse either location.
        for i in (0..size as u64).step_by(3) {
            let id = key(seed, i);
            assert_eq!(table.remove(&id), tree.remove(&id));
            if i % 2 != 0 {
                let changed = Some((id.of.subject, Gen(i + 1)));
                assert_eq!(table.insert(id, changed), tree.insert(id, changed));
            }
            compare(&table, &tree, id, how)?;
        }
        for (id, value) in tree.iter_mut() {
            assert_eq!(table.insert(*id, None), Some(*value));
            *value = None;
        }
        for i in 0..size as u64 {
            compare(&table, &tree, key(seed, i), how)?;
        }
        assert!(table.iter().eq(tree.iter()));
    }
    Ok(())
}

#[test]
fn flat_cells_answer_as_the_tree() {
    check(Config::cases(3), |tape| exercise(tape, &[0, 1, 128, 129, 100_000], Compare::FullKey));
}

#[test]
fn control_skipping_the_full_key_is_caught() {
    control(Config::cases(4), Compare::FullKey, Compare::TagOnly, DIFFERS, |tape, how| exercise(tape, &[6], how));
}

#[test]
fn a_retired_cell_shadows_the_last_answer_across_growth() {
    check(Config::cases(12), |tape| {
        let seed = tape.next_u64("previous answer");
        let [removed, standing, replaced, missing] = core::array::from_fn(|i| key(seed, i as u64));
        let old = ContentId([41; 32]);
        let new = ContentId([73; 32]);
        let zero = CellId { of: Name::new(Kind::Units, ABSENT), cell: Cell([0; 16]) };
        let mut last = Answer::new_in(alloc());
        last.serial = Gen(1);
        for id in [removed, standing, replaced] {
            last.cells.insert(id, (old, Gen(1)));
        }
        let mut overlay = Overlay::new_in(last, Gen(2), Plant::None, alloc()).expect("the previous answer opens");
        overlay.set_cell(removed, None);
        overlay.set_cell(replaced, Some(new));
        overlay.set_cell(zero, Some(ABSENT));
        for i in 4..40 {
            overlay.set_cell(key(seed, i), Some(new));
        }
        for (id, want) in [(removed, ABSENT), (standing, old), (replaced, new), (missing, ABSENT)] {
            assert_eq!(View::from_overlay(&overlay).cell(&id), want);
            assert_eq!(overlay.has_cell(&id), want != ABSENT);
        }
        assert_eq!(View::from_overlay(&overlay).cell(&zero), ABSENT);
        assert!(overlay.has_cell(&zero));
        let (mut last, work) = overlay.into_parts();
        work.apply_to(&mut last);
        assert_eq!(last.cells.get(&removed), None);
        assert_eq!(last.cells.get(&standing), Some(&(old, Gen(1))));
        assert_eq!(last.cells.get(&replaced), Some(&(new, Gen(2))));
        assert_eq!(last.cells.get(&zero), Some(&(ABSENT, Gen(2))));
        assert_eq!(last.cell(&missing), ABSENT);
    });
}
