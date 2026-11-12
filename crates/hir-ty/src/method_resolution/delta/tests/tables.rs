//! Impl tables kept in the store beside a crate's image (`frozen::store`, `frozen::serve`): a
//! database that finds them reads the crate's impls from them and maintains none, answers as
//! one that made them, and lets them go with the image.

use std::sync::Arc;

use base_db::{Crate, Db, Env, Maintained, Trigger, all_crates};
use blaze_core::lock::Lock;
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_def::deps::{self, ImageBuf, ImageStore, Mode};
use std::alloc::Global;

use super::{Loaded, check};
use crate::method_resolution::frozen;

const FIXTURE: &str = r#"
//- /main.rs crate:main deps:dep new_source_root:local
pub struct M;
impl dep::Tr for M {}
//- /dep.rs crate:dep deps:base new_source_root:library
pub trait Tr { fn f(&self) {} }
pub struct S;
impl Tr for S {}
impl base::Base for S {}
impl S { pub fn g(&self) {} }
pub mod inner { pub struct T; impl super::Tr for T {} }
pub fn holds() { struct InBody; impl Tr for InBody {} }
//- /base.rs crate:base new_source_root:library
pub trait Base {}
impl Base for u8 {}
"#;

const DEP: &str = "pub trait Tr { fn f(&self) {} }\npub struct S;\nimpl Tr for S {}\n";

const BUILD: ContentId = ContentId([7; 32]);

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
struct Memory(Lock<HashMap<ContentId, Arc<Aligned>, Global>>);

impl Memory {
    fn new() -> Memory {
        Memory(Lock::new("test.ty.impl_tables.memory", Default::default()))
    }
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(self.0.lock().get(key).map(|it| it.clone() as ImageBuf))
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        let mut kept = self.0.lock();
        match kept.get(&key) {
            Some(it) if it[..] != value[..] => Err(format!("{key:?} kept with other bytes")),
            Some(_) => Ok(()),
            None => {
                kept.insert(key, Arc::new(Aligned::copy_from(&value)));
                Ok(())
            }
        }
    }

    fn reads_now(&self, _: &Env, _: &[u8]) -> Result<ContentId, String> {
        Err("no expansion of this fixture reads anything".to_owned())
    }
}

fn named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.named(db).to_string() == name)
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// The impl tables of each crate of `db` as [`frozen::encode`] writes them now, by name.
fn tables(db: &Db) -> Vec<(&'static str, Vec<u8>)> {
    blaze_query::db::attach(db, || {
        ["base", "dep", "main"]
            .into_iter()
            .map(|name| (name, frozen::encode(db, named(db, name)).expect("the impls encode")))
            .collect()
    })
}

/// A store a first database of [`FIXTURE`] filled: the images of `dep` and `base` and their
/// impl tables; and the tables of every crate as that database made them.
fn filled() -> (Memory, Vec<(&'static str, Vec<u8>)>) {
    let memory = Memory::new();
    let mut one = Loaded::new(FIXTURE);
    let all: Vec<Crate> = all_crates(&one.db).iter().copied().collect();
    let served = deps::serve(&mut one.db, &all, BUILD, &memory, Mode::Serve).unwrap();
    assert!(served.hits.is_empty(), "a cold store");
    let stored = deps::store(&one.db, &served, &memory).unwrap();
    assert!(stored.refused.is_empty(), "{:?}", stored.refused);
    let mut imaged: Vec<(Crate, ContentId)> = stored.filed.iter().map(|(&k, &f)| (k, f)).collect();
    imaged.sort_unstable();
    assert_eq!(imaged.len(), 2, "`dep` and `base` are imaged, the member is not");
    let untabled = frozen::store(&one.db, &imaged, &memory).unwrap();
    assert!(untabled.is_empty(), "{untabled:?}");
    let made = tables(&one.db);
    (memory, made)
}

/// A second database of [`FIXTURE`] served the images and tables of `memory` in one write,
/// its maintainers' records kept from before it; and the crates whose tables it was given.
fn served(memory: &Memory) -> (Loaded, Vec<String>) {
    let mut two = Loaded::new(FIXTURE);
    two.db.keep_maintained(true);
    let mut before = Vec::new();
    two.db.take_maintained(&mut before);
    let all: Vec<Crate> = all_crates(&two.db).iter().copied().collect();
    let tabled = two.db.batch(Trigger::named("a test's serve of images and impl tables"), |db| {
        let served = deps::serve(db, &all, BUILD, memory, Mode::Serve).unwrap();
        frozen::serve(db, &served, memory, Mode::Serve).unwrap()
    });
    let mut names: Vec<String> = tabled.iter().map(|it| it.named(&two.db).to_string()).collect();
    names.sort();
    (two, names)
}

/// The crates the impls maintainer went over since the records were last taken, by name.
fn maintained(db: &mut Db) -> Vec<String> {
    let mut all: Vec<Maintained> = Vec::new();
    db.take_maintained(&mut all);
    let mut names: Vec<String> = all
        .iter()
        .filter(|it| it.maintainer == super::NAME)
        .map(|it| it.krate.named(&*db).to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The refinement: a database that reads a crate's impls from the tables the store keeps
/// answers every table as the database that made them (the bytes [`frozen::encode`] writes of
/// each crate are the same: of the served crates read back, and of the member maintained over
/// them), and the impls maintainer goes over no served crate in the write that serves them.
/// The tables are found by the image's key and go with the image: an edit of `dep` takes
/// both away in its own write, `dep` is maintained again and answers as a fresh database of
/// the edited text, and `base`, which the edit does not reach, stays read from its tables.
///
/// Its control is [`the_oracle_catches_tables_kept_past_their_image`].
#[test]
fn a_crate_given_its_impl_tables_is_not_maintained_and_answers_as_the_one_that_made_them() {
    let (memory, made) = filled();
    let (mut two, tabled) = served(&memory);
    assert_eq!(tabled, ["base", "dep"], "both imaged crates are given their tables");
    let (dep, base, main) =
        (named(&two.db, "dep"), named(&two.db, "base"), named(&two.db, "main"));
    assert!(frozen::served(&two.db, dep) && frozen::served(&two.db, base));
    assert!(!frozen::served(&two.db, main), "a member has no tables");
    assert_eq!(maintained(&mut two.db), ["main"], "no served crate's impls were maintained");
    assert_eq!(tables(&two.db), made, "every crate's tables, read or maintained, are the same");
    let mut failures = String::new();
    check(&two.db, main, "`main` over crates read from tables", &mut failures);
    assert!(failures.is_empty(), "{failures}");

    // A second serve of the same crates finds the same tables and writes nothing.
    let view = two.db.nonce_and_revision().1;
    let all: Vec<Crate> = all_crates(&two.db).iter().copied().collect();
    let again = two.db.batch(Trigger::named("a test's second serve"), |db| {
        let served = deps::serve(db, &all, BUILD, &memory, Mode::Serve).unwrap();
        frozen::serve(db, &served, &memory, Mode::Serve).unwrap()
    });
    assert_eq!(again, Vec::new(), "the same tables");
    assert_eq!(two.db.nonce_and_revision().1, view, "nothing written");

    // An edit of `dep`: its image and its tables go in that write.
    two.db.set_file_text(two.files[1], DEP);
    assert!(!frozen::served(&two.db, dep), "`dep` lost its tables with its image");
    assert!(frozen::served(&two.db, base), "`base` keeps its own");
    let fresh = Loaded::new(&FIXTURE.replace(
        "pub trait Tr { fn f(&self) {} }\npub struct S;\nimpl Tr for S {}\nimpl base::Base for S {}\nimpl S { pub fn g(&self) {} }\npub mod inner { pub struct T; impl super::Tr for T {} }\npub fn holds() { struct InBody; impl Tr for InBody {} }\n",
        DEP,
    ));
    assert_eq!(tables(&two.db), tables(&fresh.db), "as a fresh database of the edited text");
    for (krate, what) in [(dep, "`dep` edited"), (main, "`main` over `dep` edited")] {
        check(&two.db, krate, what, &mut failures);
    }
    assert!(failures.is_empty(), "{failures}");
}

/// The control of [`a_crate_given_its_impl_tables_is_not_maintained_and_answers_as_the_one_that_made_them`]:
/// with the tables kept past their image (the planted bug: the implied write does nothing),
/// `dep` is still read from the tables of the text before the edit, which that test's
/// assertion after the edit refuses.
#[test]
fn the_oracle_catches_tables_kept_past_their_image() {
    let (memory, _) = filled();
    let (mut two, _) = served(&memory);
    let dep = named(&two.db, "dep");
    frozen::KEEPS_TABLES_PAST_IMAGE.set(true);
    // The pass after the edit may itself stop on the stale tables (they name files that are
    // no longer the crate's): either way they are still what `dep` is read from.
    let file = two.files[1];
    _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| two.db.set_file_text(file, DEP)));
    frozen::KEEPS_TABLES_PAST_IMAGE.set(false);
    assert!(frozen::served(&two.db, dep), "the planted bug keeps the tables of the old text");
}
