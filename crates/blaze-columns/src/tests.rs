use std::alloc::Global;

use blaze_own::{String, Vec};

use crate::{Batch, Column, ColumnError, Field, Scalar, Texts, Type, Values};

fn texts(values: &[&str]) -> Texts<Global> {
    let mut out = Texts::new_in(Global);
    for value in values {
        out.push(value);
    }
    out
}

#[test]
fn texts_are_offsets_into_one_string() {
    let t = texts(&["ab", "", "c"]);
    assert_eq!(t.offsets(), [0, 2, 2, 3]);
    assert_eq!(t.text(), "abc");
    assert_eq!(t.iter().collect::<std::vec::Vec<_>>(), ["ab", "", "c"]);
}

#[test]
fn offsets_inside_a_character_are_refused() {
    let offsets = Vec::from_slice_in(&[0, 1, 2], Global);
    let err = Texts::from_parts(offsets, String::from_str_in("é", Global)).unwrap_err();
    assert!(matches!(err, ColumnError::Offsets { .. }), "{err:?}");
}

#[test]
fn a_column_with_no_null_keeps_no_validity() {
    let column = Column::options([Some(1u32), Some(2)], Global);
    assert_eq!(column.validity(), None);
    let column = Column::options([Some(1u32), None], Global);
    assert_eq!(column.validity(), Some(&[true, false][..]));
    assert_eq!((column.int(0), column.int(1), column.null_count()), (Some(1), None, 1));
}

#[test]
fn a_dictionary_key_past_its_values_is_refused() {
    let err = Column::dict([Some(0), Some(2)], texts(&["a", "b"]), Global).unwrap_err();
    assert_eq!(err, ColumnError::DictKey { row: 1, key: 2, values: 2 });
    // A null row's key is not read.
    let column = Column::dict([Some(1), None], texts(&["a", "b"]), Global).unwrap();
    assert_eq!((column.text(0), column.text(1)), (Some("b"), None));
}

#[test]
fn lists_are_offsets_into_their_elements() {
    let column = Column::list(
        Vec::from_slice_in(&[0, 2, 2, 3], Global),
        Some(Vec::from_slice_in(&[true, false, true], Global)),
        Values::U64(Vec::from_slice_in(&[1, 2, 3], Global)),
    )
    .unwrap();
    assert_eq!(column.ty(), Type::List(Scalar::U64));
    let lists = column.lists().unwrap();
    assert_eq!((lists.slice::<u64>(0), lists.slice::<u64>(1), lists.slice::<u64>(2)), (Some(&[1, 2][..]), None, Some(&[3][..])));
    let bad = Column::list(Vec::from_slice_in(&[0, 2, 1], Global), None, Values::U64(Vec::from_slice_in(&[1, 2], Global)));
    assert!(matches!(bad, Err(ColumnError::Offsets { .. })));
}

#[test]
fn a_batch_checks_its_columns_against_its_fields() {
    let fields = || {
        Vec::from_iter_in([Field::new("a", Type::One(Scalar::U32), false), Field::new("b", Type::One(Scalar::Text), true)], Global)
    };
    let ok = Batch::try_new(
        fields(),
        Vec::from_iter_in([Column::values([1u32, 2], Global), Column::opt_texts(&[Some("x"), None], Global)], Global),
    )
    .unwrap();
    assert_eq!((ok.num_rows(), ok.index_of("b")), (2, Some(1)));
    let rows = Batch::try_new(
        fields(),
        Vec::from_iter_in([Column::values([1u32], Global), Column::opt_texts(&[Some("x"), None], Global)], Global),
    );
    assert!(matches!(rows, Err(ColumnError::Rows { .. })), "{rows:?}");
    let null = Batch::try_new(
        fields(),
        Vec::from_iter_in([Column::options([Some(1u32), None], Global), Column::texts(&["x", "y"], Global)], Global),
    );
    assert!(matches!(null, Err(ColumnError::Null { at: 0, row: 1 })), "{null:?}");
    let ty = Batch::try_new(
        fields(),
        Vec::from_iter_in([Column::values([1u64, 2], Global), Column::texts(&["x", "y"], Global)], Global),
    );
    assert!(matches!(ty, Err(ColumnError::Type { at: 0, .. })), "{ty:?}");
}
