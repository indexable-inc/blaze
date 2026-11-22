use blaze_flow_lower::FieldRegion::{Arg, Bound, Lifetime, Static};
use blaze_id::ContentId;

use super::super::head_tests::{a, world_of};
use super::{FreezeError, FrozenError, FrozenHead, file_frozen, freeze};
use crate::front2::heads::{CoherenceMarks, Store, Want};
use crate::front2::keys::{Keys, checker_of};
use crate::front2::receiver::Pointers;
use crate::front2::source::NamesSource;

const WRAPPER: &str = "#![no_std]
#![feature(fundamental, rustc_attrs)]
#[fundamental]
#[rustc_has_incoherent_inherent_impls]
pub struct Wrapper<'a, 'b, T> {
    first: &'b T,
    second: &'a u8,
    forever: &'static u8,
    plain: u8,
    callback: fn(&u8) -> &u8,
}
";

fn wrapper(text: &'static str) -> (ContentId, CoherenceMarks) {
    let (names, sized, frame) = world_of(text);
    let mut key = None;
    names.each_item(0, &mut |k, _| {
        if names.name_of(k) == Some("Wrapper") {
            key = Some(k);
        }
    });
    let lib = key.expect("fixture Wrapper");
    let built = freeze(names, sized, Pointers::NOT_LOOKED_UP, Want::Adt(lib), frame, a(), a()).expect("freeze Wrapper");
    let frozen = FrozenHead::read(&built.bytes).expect("read frozen Wrapper");
    let store = Store::new(sized, frame, a());
    let keys = Keys::new();
    let wrong = Want::Fn(lib);
    let error = file_frozen(names, wrong, frozen, &store, &keys, a()).expect_err("a body can only import the requested cell");
    assert!(matches!(error, FreezeError::Read { want, error: FrozenError::WrongCell { found } } if want == wrong && found == Want::Adt(lib)));
    file_frozen(names, Want::Adt(lib), frozen, &store, &keys, a()).expect("import the completed Wrapper record");
    let row = store.adts.get(&checker_of(lib)).expect("imported Wrapper");
    assert_eq!(row.lifetimes, 2);
    assert_eq!(row.field_regions.at[..], [(0, 2), (2, 1), (3, 1), (4, 0), (4, 2)]);
    assert_eq!(row.field_regions.list[..], [Lifetime(1), Arg(0), Lifetime(0), Static, Bound(0), Bound(0)]);
    (frozen.meaning(), row.marks)
}

#[test]
fn coherence_and_field_regions_survive_freezing() {
    let (both, marks) = wrapper(WRAPPER);
    assert!(marks.fundamental && marks.incoherent);

    for (attr, fundamental, incoherent) in [("#[fundamental]", false, true), ("#[rustc_has_incoherent_inherent_impls]", true, false)] {
        let edit = WRAPPER.replace(attr, "");
        let (one, marks) = wrapper(std::boxed::Box::leak(edit.into_boxed_str()));
        assert_eq!((marks.fundamental, marks.incoherent), (fundamental, incoherent));
        assert_ne!(one, both, "a change to {attr} must invalidate a coherence reader");
    }
    std::println!("Wrapper: both coherence marks, 2 lifetimes, 5 field rows survive freezing");
}
