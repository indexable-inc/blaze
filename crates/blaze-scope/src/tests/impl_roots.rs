//! An impl an expansion writes with another crate's `$crate` (token-mark-2 R2): the items record
//! keeps the crate its trait path's root names, and files the impl under THAT crate's trait.
//!
//! The Reference, "Paths", "`$crate`": "`$crate` will expand to a path to access items from the
//! top level of the crate where the macro is defined, regardless of which crate the macro is
//! invoked." Crate `a`'s `m!` writes `impl $crate::Tr for S {}`; crate `c` calls it and has a
//! `Tr` of its own. The expansion's text writes the keyword `crate` (token-mark-2) whose mark is
//! `a`'s root; the row keeps it (`trait_root`), so the impl is `a::Tr`'s, never `c`'s.
//!
//! CONTROL: drop the root in the writer (items_record.rs `encode_with`: `let trait_root = None;`):
//! the first assertion fails with "the row keeps a's root". With only buckets.rs `walked`'s
//! override removed, the second fails with "S's impl is filed under a's `Tr`, not c's".
//!
//! Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
//! blaze-scope test impl_roots`:
//! `test tests::impl_roots::an_impl_of_another_crates_dollar_crate_trait_is_filed_under_it ... ok`.

use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::items_record::buckets::{BucketKey, Head};
use crate::items_record::{ItemsRecord, encode_with};
use crate::names::Written;
use crate::resolve::aligned;

const A: &str = "#![no_core]
pub trait Tr {}
#[macro_export]
macro_rules! m { () => { pub struct S; impl $crate::Tr for S {} } }
";

const C: &str = "#![no_core]
pub trait Tr {}
a::m!();
";

static TEXTS: Texts = Texts(&[&[("/a/lib.rs", A)], &[("/c/lib.rs", C)]]);

#[test]
fn an_impl_of_another_crates_dollar_crate_trait_is_filed_under_it() {
    let crates = [input("a", "/a/lib.rs", &[]), input("c", "/c/lib.rs", &[("a", 0)])];
    let names = resolved(&crates, &TEXTS);
    let alloc = blaze_own::testing::heap().alloc();
    let key_of = |krate: usize, name: &'static str| {
        names
            .item_at(names.root(krate), Namespace::Types, Written { kind: PathKind::Plain, segments: &[name] }, alloc)
            .unwrap_or_else(|e| panic!("crate {krate} binds `{name}`: {e}"))
            .def
            .key
    };
    let (a_tr, c_tr, s) = (key_of(0, "Tr"), key_of(1, "Tr"), key_of(1, "S"));
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bytes = encode_with(names.krate(1), &names, alloc).unwrap_or_else(|e| panic!("c's items record: {e:?}"));
    let record = ItemsRecord::open(aligned(&bytes, erased.bump()), alloc).unwrap_or_else(|e| panic!("open c's record: {e:?}"));
    let row = (0..record.impls())
        .filter_map(|i| record.impl_at(i).unwrap_or_else(|e| panic!("impl row {i}: {e:?}")))
        .find(|row| row.has_trait)
        .unwrap_or_else(|| panic!("c's record has no trait impl row"));
    assert_eq!(row.trait_root, Some(names.krate(0).key), "the row keeps a's root; its trait path is {:?}", row.trait_path);
    let filed = |tr| record.bucket(&BucketKey::of_trait(tr, Head::Named(s))).unwrap_or_else(|e| panic!("bucket: {e:?}")).len();
    assert_eq!((filed(a_tr), filed(c_tr)), (1, 0), "S's impl is filed under a's `Tr`, not c's");
}
