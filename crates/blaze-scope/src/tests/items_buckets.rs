//! The bucket index and the variant table of the items record
//! ([`crate::items_record::buckets`], [`crate::items_record::variants`]) round trip: a crate of
//! one file with two traits, four types and ten impls is solved once and its record written
//! (twice: equal bytes); then each (trait or inherent, self type's head) is asked of the index
//! by KEYS alone and must answer exactly the impls the Reference files there; the impls whose
//! header names nothing are in the unfiled list with why, never dropped; every impl is in the
//! one or the other; each variant answers its enum and index in one search.
//!
//! The Reference, "Implementations": "An implementation is an item that associates items with
//! an implementing type"; "Trait implementations": "A trait implementation is defined like an
//! inherent implementation except that the optional generic type declarations are followed by
//! a trait, followed by the keyword `for`". So `impl T for S` is of trait `T` at the nominal
//! type `S`; `impl<X> T for X` has a type parameter as its self type (a blanket impl: it can
//! apply to any type, so it is filed under its trait at "any"); `impl !T for &S` is a negative
//! impl ("Negative impls" of auto traits are written `impl !Trait for Type`).
//!
//! A DIGEST MOVES ONLY WITH ITS BUCKET: the same crate with one more type and one more impl of
//! `T` (for that type) is solved and written; the digest of bucket (`T`, `S`) is equal in both
//! records and the digest of (`T`, the new type) differs from the empty bucket's.
//!
//! PLANTED BUGS, on copies of the record's bytes: (1) the bucket table's count one less (its
//! last row withheld: the negative impl's, kind 2 sorts last): the negative impls of `T` must
//! now be none; (2) an unfiled row's why set to a byte that is no reason: the reader must
//! refuse it by name.
//!
//! Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
//! blaze-scope test an_impl_question_is_one_search`:
//! `test tests::items_buckets::an_impl_question_is_one_search ... ok`.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::ItemKind;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::{Texts, input};
use crate::builtin::builtin_type;
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::{CrateId, Sysroot};
use crate::items_record::buckets::{BucketKey, F_BUCKETS_COUNT, F_UNFILED_AT, Head, NotFiled};
use crate::items_record::{ItemsRecord, ItemsRecordError, encode};
use crate::resolve::{CrateInput, aligned};
use crate::task::Serial;

const WAIT: core::time::Duration = core::time::Duration::from_secs(60);
const PATH: &str = "/k0/lib.rs";
const TEXT: &str = "#![no_core]\n\
trait T { fn a(); }\n\
trait U<X> {}\n\
struct S;\n\
enum E { A, B { x: S } }\n\
struct G<X>(X);\n\
impl T for S { fn a() {} }\n\
impl T for E { fn a() {} }\n\
impl<X> T for G<X> { fn a() {} }\n\
impl<X> T for X { fn a() {} }\n\
impl U<S> for E {}\n\
impl self :: T for (S, S) { fn a() {} }\n\
impl S { fn own() {} }\n\
impl !T for &S {}\n\
impl T for dyn T { fn a() {} }\n\
impl Missing for S {}\n";
/// [`TEXT`] and one more type with one more impl of `T`.
const MORE: &str = "#![no_core]\n\
trait T { fn a(); }\n\
trait U<X> {}\n\
struct S;\n\
enum E { A, B { x: S } }\n\
struct G<X>(X);\n\
impl T for S { fn a() {} }\n\
impl T for E { fn a() {} }\n\
impl<X> T for G<X> { fn a() {} }\n\
impl<X> T for X { fn a() {} }\n\
impl U<S> for E {}\n\
impl self :: T for (S, S) { fn a() {} }\n\
impl S { fn own() {} }\n\
impl !T for &S {}\n\
impl T for dyn T { fn a() {} }\n\
impl Missing for S {}\n\
struct H;\n\
impl T for H { fn a() {} }\n";

static FILES: [(&str, &str); 1] = [(PATH, TEXT)];
static CRATES: [&[(&str, &str)]; 1] = [&FILES];
static TEXTS: Texts = Texts(&CRATES);
static MORE_FILES: [(&str, &str); 1] = [(PATH, MORE)];
static MORE_CRATES: [&[(&str, &str)]; 1] = [&MORE_FILES];
static MORE_TEXTS: Texts = Texts(&MORE_CRATES);

/// The record of the one crate whose file is `texts`'s, opened.
fn record_of(texts: &'static Texts) -> (&'static [u8], ItemsRecord<'static>) {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k0", PATH, &[])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let names = solved[0].names;
    let a = encode(names, g).unwrap_or_else(|e| panic!("encode: {e:?}"));
    let b = encode(names, g).unwrap_or_else(|e| panic!("encode again: {e:?}"));
    assert!(a == b, "the record encodes to different bytes twice");
    let bytes: &'static [u8] = aligned(&a, bump);
    (bytes, super::items_record::one_file(bytes, names.files[0].text, bump))
}

/// The key of the item of `kind` named `name`.
fn key_of(record: &ItemsRecord<'static>, kind: ItemKind, name: &str) -> ItemKey {
    (0..record.items())
        .filter_map(|i| record.identity_at(i).ok().flatten())
        .find(|row| row.kind == kind && row.name == Some(name))
        .unwrap_or_else(|| panic!("no item row for the {kind:?} `{name}`"))
        .key
}

/// The impls `key` answers: each with (its trait path as its row writes it, its self path,
/// bare, blanket), in the bucket's order.
fn asked(record: &ItemsRecord<'static>, key: &BucketKey) -> AVec<'static, (Option<&'static str>, Option<&'static str>, bool, bool)> {
    let mut out = AVec::new_in(Alloc::global());
    for at in record.bucket(key).unwrap_or_else(|e| panic!("the bucket {key:?}: {e:?}")) {
        let it = record.bucket_impl(at).unwrap_or_else(|e| panic!("bucket row {at}: {e:?}"));
        let row = record.impl_at(it.row).unwrap_or_else(|e| panic!("impl row {}: {e:?}", it.row)).unwrap_or_else(|| panic!("no impl row {}", it.row));
        assert!(row.key == it.key, "bucket row {at} names impl row {} whose key is another's", it.row);
        out.push((row.trait_path, row.self_path, it.bare, it.blanket));
    }
    out
}

/// `bytes` with the u32 at `at` set to `value`, aligned as a record is read.
fn with_u32(bytes: &[u8], at: usize, value: u32) -> &'static [u8] {
    let mut copy = AVec::from_slice_in(bytes, Alloc::global());
    copy[at..at + 4].copy_from_slice(&value.to_le_bytes());
    aligned(&copy, super::SyncFrame::leaked())
}

#[test]
fn an_impl_question_is_one_search() {
    let g = Alloc::global();
    let (bytes, record) = record_of(&TEXTS);
    let (t, u) = (key_of(&record, ItemKind::Trait, "T"), key_of(&record, ItemKind::Trait, "U"));
    let (s, e, generic) = (key_of(&record, ItemKind::Struct, "S"), key_of(&record, ItemKind::Enum, "E"), key_of(&record, ItemKind::Struct, "G"));

    // Each bucket holds exactly its impls.
    let one = |key: BucketKey, what: &str, want: (Option<&str>, Option<&str>, bool, bool)| {
        let got = asked(&record, &key);
        assert!(got.len() == 1 && got[0] == want, "{what}: the index answers {got:?}, the text files {want:?} there alone");
    };
    one(BucketKey::of_trait(t, Head::Named(s)), "`T for S`", (Some("T"), Some("S"), true, false));
    one(BucketKey::of_trait(t, Head::Named(e)), "`T for E`", (Some("T"), Some("E"), true, false));
    one(BucketKey::of_trait(t, Head::Named(generic)), "`T for G<X>`", (Some("T"), Some("G"), true, false));
    one(BucketKey::of_trait(t, Head::Any), "`impl<X> T for X` (a blanket impl: any head)", (Some("T"), Some("X"), true, true));
    one(BucketKey::of_trait(u, Head::Named(e)), "`U<S> for E` (its trait written with an argument: not bare)", (Some("U"), Some("E"), false, false));
    one(BucketKey::of_trait(t, Head::Tuple(2)), "`impl self :: T for (S, S)` (a path read by its tokens)", (Some("self :: T"), None, true, false));
    one(BucketKey::inherent(Head::Named(s)), "`impl S`", (None, Some("S"), false, false));
    one(BucketKey::negatives(t), "`impl !T for &S` (every negative impl of `T`)", (Some("T"), None, true, false));
    // A bucket no impl is in: `U for S`, an inherent impl of `E`, a positive impl at `&_`.
    for (key, what) in [(BucketKey::of_trait(u, Head::Named(s)), "`U for S`"), (BucketKey::inherent(Head::Named(e)), "`impl E`"), (BucketKey::of_trait(t, Head::Ref), "`T for &_`")] {
        assert!(asked(&record, &key).is_empty(), "{what}: no impl is written, the index answers {:?}", asked(&record, &key));
    }

    // The two impls whose header names nothing as written: unfiled with why, never dropped.
    let mut unfiled = AVec::new_in(g);
    let mut i = 0;
    while let Some((it, why)) = record.unfiled_at(i).unwrap_or_else(|e| panic!("unfiled row {i}: {e:?}")) {
        let row = record.impl_at(it.row).unwrap_or_else(|e| panic!("impl row {}: {e:?}", it.row)).unwrap_or_else(|| panic!("no impl row {}", it.row));
        unfiled.push((row.trait_path, why));
        i += 1;
    }
    unfiled.sort_by_key(|u| u.1 as u8);
    assert!(
        unfiled[..] == [(Some("Missing"), NotFiled::TraitNotFound), (Some("T"), NotFiled::SelfOther)],
        "`impl Missing for S` (its trait names nothing) and `impl T for dyn T` (no head read) are the unfiled list: {unfiled:?}"
    );
    assert!(record.unfiled() == 2 && record.bucket_rows() + record.unfiled() == record.impls(), "{} filed + {} unfiled of {} impls", record.bucket_rows(), record.unfiled(), record.impls());
    assert!(record.bucket_rows() == 8, "eight impls are filed under a bucket: {}", record.bucket_rows());

    // Each variant answers its enum and its index; a key that is no variant answers none.
    let mut variants = AVec::new_in(g);
    let mut i = 0;
    while let Some(row) = record.variant_at(i).unwrap_or_else(|e| panic!("variant row {i}: {e:?}")) {
        variants.push(row);
        i += 1;
    }
    assert!(variants.len() == 2 && record.variant_rows() == 2, "`E` has the variants `A` and `B`: {variants:?}");
    for (variant, owner, index) in variants.iter() {
        assert!(*owner == e, "variant {variant:?} is of {owner:?}, the enum is {e:?}");
        assert!(matches!(record.variant_home(*variant), Ok(Some(home)) if home == (e, *index)), "variant {variant:?} by its key: {:?}", record.variant_home(*variant));
    }
    let mut indices: AVec<'_, u32> = AVec::from_iter_in(variants.iter().map(|v| v.2), g);
    indices.sort_unstable();
    assert!(indices[..] == [0, 1], "the variants' indices are their declaration order: {indices:?}");
    assert!(matches!(record.variant_home(s), Ok(None)), "a struct's key is no variant's: {:?}", record.variant_home(s));

    // A digest moves only with its bucket.
    let (_, more) = record_of(&MORE_TEXTS);
    let (t2, s2, h2) = (key_of(&more, ItemKind::Trait, "T"), key_of(&more, ItemKind::Struct, "S"), key_of(&more, ItemKind::Struct, "H"));
    assert!((t2, s2) == (t, s), "`T` and `S` keep their keys when an item is added after them");
    let digest = |r: &ItemsRecord<'static>, key: BucketKey| r.bucket_digest(&key, g).unwrap_or_else(|e| panic!("the digest of {key:?}: {e:?}"));
    assert!(more.impls() == record.impls() + 1, "the second crate writes one more impl");
    assert!(digest(&more, BucketKey::of_trait(t, Head::Named(s))) == digest(&record, BucketKey::of_trait(t, Head::Named(s))), "an impl added for `H` moved the bucket (`T`, `S`)");
    assert!(digest(&more, BucketKey::of_trait(t, Head::Named(h2))) != digest(&record, BucketKey::of_trait(t, Head::Named(h2))), "an impl added for `H` did not move the bucket (`T`, `H`)");
    assert!(more.unfiled_digest(g).ok() == record.unfiled_digest(g).ok(), "an impl filed under a bucket moved the unfiled list's digest");

    // PLANTED BUG 1: the bucket table's last row withheld.
    let count = u32::try_from(record.bucket_rows()).expect("a count");
    let planted = ItemsRecord::open(with_u32(bytes, 8 + 4 * F_BUCKETS_COUNT, count - 1), g).unwrap_or_else(|e| panic!("open the planted record: {e:?}"));
    assert!(asked(&planted, &BucketKey::negatives(t)).is_empty(), "planted bug 1 (the last bucket row withheld) was not caught: the negative impls of `T` are still {:?}", asked(&planted, &BucketKey::negatives(t)));

    // PLANTED BUG 2: an unfiled row's why is no reason.
    let at = u32::from_le_bytes(bytes[8 + 4 * F_UNFILED_AT..][..4].try_into().expect("four bytes")) as usize;
    let planted = ItemsRecord::open(with_u32(bytes, at + 4, 200), g).unwrap_or_else(|e| panic!("open the second planted record: {e:?}"));
    assert!(
        matches!(planted.unfiled_at(0), Err(ItemsRecordError::Byte { what: "why an impl is unfiled", byte: 200 })),
        "planted bug 2 (a why that is no reason) was not refused by name: {:?}",
        planted.unfiled_at(0)
    );
}

const PRIM_CORE: &str = "#![no_core]
pub mod prelude {
    pub mod v1 {}
    pub mod rust_2021 { pub use super::v1::*; }
}
";
const PRIM_DEP: &str = "#![no_core]
pub struct Thing;
";
/// The impls at a builtin self type written outside core (num-bigint's `impl Rem<BigUint> for
/// u64`): at the root, nothing nearer binds `u64`; in `globbed`, `use dep::*;` might; in `own`,
/// a struct of the name does.
const PRIM_USER: &str = "#![no_std]
pub trait Tr<X> {}
pub struct Big;
impl Tr<Big> for u64 {}
mod globbed {
    use dep::*;
    impl crate::Tr<crate::Big> for u64 {}
}
mod own {
    pub struct u64;
    impl crate::Tr<crate::Big> for u64 {}
}
";
static PRIM_TEXTS: Texts = Texts(&[&[("/core/lib.rs", PRIM_CORE)], &[("/dep/lib.rs", PRIM_DEP)], &[("/user/lib.rs", PRIM_USER)]]);
const PRIM_DEPS: &[(&str, usize)] = &[("dep", 1)];

fn prim_input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-scope test crate", name.as_bytes())),
        edition: blaze_surface::Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

/// Rule (the Reference, "Names" / preludes: the standard prelude's names are shadowed by any
/// nearer binding, and the builtin types are the last layer of the type namespace; no prelude
/// binds a type named like a builtin): the items record of `user`, written with its OWN names
/// only (core's prelude and `dep` not held), files `impl Tr<Big> for u64` at the root under
/// (`Tr`, the builtin `u64`), the bucket core's own `impl Rem for u64` is in; keeps the same
/// impl in a module with `use dep::*;` unfiled as `SelfNotRead` (the unread glob may bind
/// `u64`); and files the one in a module that declares `struct u64;` under that struct.
/// Expected: `test tests::items_buckets::a_builtin_self_outside_core_is_filed_at_the_builtin
/// ... ok`. Before C05b: the root's impl is unfiled (`SelfNotRead`), "the root's impl at the
/// builtin `u64`" fails.
#[test]
fn a_builtin_self_outside_core_is_filed_at_the_builtin() {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [prim_input("core", "/core/lib.rs", &[]), prim_input("dep", "/dep/lib.rs", &[]), prim_input("user", "/user/lib.rs", PRIM_DEPS)];
    let sys = Sysroot { core: Some(CrateId(0)), alloc: None, std: None, bstd: None };
    let (solved, _) = resolve_by_records(&inputs, sys, &PRIM_TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let a = encode(solved[2].names, g).unwrap_or_else(|e| panic!("encode: {e:?}"));
    let bytes: &'static [u8] = aligned(&a, bump);
    let record = ItemsRecord::open(bytes, g).unwrap_or_else(|e| panic!("open: {e:?}"));
    let tr = key_of(&record, ItemKind::Trait, "Tr");
    let local = key_of(&record, ItemKind::Struct, "u64");
    let builtin = builtin_type("u64").unwrap_or_else(|| panic!("`u64` is a builtin type")).key;
    assert!(local != builtin, "the struct `u64` has a key of its own");

    let at_builtin = asked(&record, &BucketKey::of_trait(tr, Head::Named(builtin)));
    assert!(at_builtin.len() == 1 && at_builtin[0].1 == Some("u64"), "the root's impl at the builtin `u64`: the index answers {at_builtin:?}");
    let at_local = asked(&record, &BucketKey::of_trait(tr, Head::Named(local)));
    assert!(at_local.len() == 1 && at_local[0].1 == Some("u64"), "`own`'s impl at its struct `u64`: the index answers {at_local:?}");

    let mut unfiled = AVec::new_in(g);
    let mut i = 0;
    while let Some((it, why)) = record.unfiled_at(i).unwrap_or_else(|e| panic!("unfiled row {i}: {e:?}")) {
        let row = record.impl_at(it.row).unwrap_or_else(|e| panic!("impl row {}: {e:?}", it.row)).unwrap_or_else(|| panic!("no impl row {}", it.row));
        unfiled.push((row.self_path, why));
        i += 1;
    }
    assert!(
        unfiled[..] == [(Some("u64"), NotFiled::SelfNotRead)],
        "`globbed`'s impl (behind `use dep::*;`, `dep` not held) is the one unfiled, as not read: {unfiled:?}"
    );
    assert!(record.impls() == 3 && record.bucket_rows() == 2, "{} filed + {} unfiled of {} impls", record.bucket_rows(), record.unfiled(), record.impls());
}
