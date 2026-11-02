use syntax::Edition;

use crate::{file::Working, path};

#[test]
fn a_warm_find_allocates_nothing() {
    let file = Working::parsed(
        "lib.rs",
        Edition::CURRENT,
        "mod m {\n    pub struct S(u8, u16);\n    impl Clone for S {\n        fn clone(&self) -> S { S(self.0, self.1) }\n    }\n    impl S {}\n    impl S {}\n}\nfn f() {}\n",
    );
    let root = file.parse.root();
    for wanted in
        ["f", "m::S::1", "m::<S as Clone>::clone", "m::struct S", "m::< S as  Clone >::clone", "m::<S>@1"]
    {
        let first = path::find(root, wanted).unwrap_or_else(|error| panic!("{wanted}: {error}"));
        let (again, used) = blaze_alloc_count::measure(|| path::find(root, wanted).unwrap());
        assert_eq!(again, first);
        assert_eq!(used, blaze_alloc_count::Used::default(), "a warm find of `{wanted}`");
    }
}

#[test]
fn a_tuple_field_is_found_by_its_index_as_written_and_by_no_other_spelling() {
    let file = Working::parsed("lib.rs", Edition::CURRENT, "struct T(u8, u16, u32);\n");
    let root = file.parse.root();
    assert_eq!(path::of(path::find(root, "T::0").unwrap()), "T::0");
    assert_eq!(path::of(path::find(root, "T::2").unwrap()), "T::2");
    for wrong in ["T::00", "T::01", "T::+1", "T::3", "T::"] {
        assert!(path::find(root, wrong).is_err(), "{wrong}");
    }
}
