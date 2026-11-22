//! The form of a program's names rows (run_names.rs FORM): encoded from the answer's rows, read
//! in place, refused whole when its bytes are not of the form.

use blaze_drive::{Input, Key, Kind, Name};
use blaze_ids::ContentId;
use blaze_own::AVec;

use super::{Answered, FormError, MAGIC, RunNamesError, Stood, encode};

fn input(n: u8) -> Input {
    Input { name: Name::new(Kind::CrateScope, ContentId([n; 32])), at: Key(ContentId([n.wrapping_add(100); 32])), out: ContentId([n.wrapping_add(200); 32]) }
}

fn graph() -> Input {
    Input { name: Name::unit_graph(), at: Key(ContentId([7; 32])), out: ContentId([8; 32]) }
}

fn written(rows: &[(usize, Input)]) -> AVec<'static, u8> {
    let heap = blaze_own::testing::heap();
    let mut out = AVec::new_in(heap.alloc());
    encode(graph(), rows, &mut out, heap.alloc()).expect("the rows encode");
    out
}

/// A walk's rows, in reader order (not sorted), read back by index on a hit: each the row it
/// was written with, under the name the caller gives; an index no row has is no row.
#[test]
fn the_rows_a_walk_writes_a_hit_reads_by_index() {
    let rows = [(5, input(5)), (0, input(0)), (12, input(12))];
    let bytes = written(&rows);
    assert_eq!(&bytes[..16], &MAGIC);
    let stood = Stood::open(&bytes).expect("the record opens");
    assert_eq!(stood.graph(), Some(graph()));
    for (at, row) in rows.iter() {
        assert_eq!(stood.crate_input(*at, row.name), Some(*row));
    }
    assert_eq!(stood.crate_input(3, input(3).name), None);
    assert_eq!(stood.crate_input(13, input(13).name), None);
}

/// Bytes not of the form are refused whole, by what is wrong: never read as rows.
#[test]
fn a_record_not_of_the_form_is_refused() {
    let bytes = written(&[(1, input(1)), (2, input(2))]);
    assert_eq!(Stood::open(&bytes[..10]).err(), Some(FormError::Short { len: 10 }));
    let mut other = bytes.to_vec();
    other[0] ^= 1;
    assert_eq!(Stood::open(&other).err(), Some(FormError::Magic));
    let cut = &bytes[..bytes.len() - 1];
    assert!(matches!(Stood::open(cut), Err(FormError::Length { .. })));
    // The second row's index set below the first's.
    let mut unsorted = bytes.to_vec();
    let second = super::HEAD + super::ROW;
    unsorted[second..second + 4].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(Stood::open(&unsorted).err(), Some(FormError::Unsorted { row: 1 }));
    // One crate written twice is refused at the write.
    let heap = blaze_own::testing::heap();
    let mut out = AVec::new_in(heap.alloc());
    assert!(matches!(encode(graph(), &[(4, input(4)), (4, input(4))], &mut out, heap.alloc()), Err(RunNamesError::Form(FormError::Unsorted { row: 1 }))));
}

/// Cargo runs a harnessless test target's main exactly once, including when another package
/// has a target with the same name. The two rows have the same spelling and distinct owners.
#[test]
fn harnessless_targets_each_have_one_main() {
    for n in [1u8, 2] {
        let main = blaze_id::ItemKey(blaze_id::ContentId([n; 32]));
        let tests = super::TestRows::Main(main);
        assert_eq!(tests.len(), 1);
        let row = tests.row(0).expect("valid test row").expect("one invocation");
        assert_eq!(
            (row.key, row.path, row.ignore, row.should_panic, row.bench),
            (main, "main", None, blaze_scope::tests_record::ShouldPanic::No, false)
        );
        assert_eq!(tests.row(1), Ok(None));
    }
}
