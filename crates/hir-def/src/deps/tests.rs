use std::{
    any::TypeId,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering::SeqCst},
    },
};

use base_db::{Crate, CrateOrigin, Db, Env, FileId};
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_expand::fixture;
use hir_expand::proc_macro::{
    Expanded, ExpansionReads, ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError,
    ProcMacroKind, StateUse,
};
use blaze_core::lock::Lock;
use std::alloc::Global;

use super::*;
use crate::{nameres::frozen, test_db::TestDB};

const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
use lib::{Shown, shown, deep::deeper::f};
fn main() { shown!(); f(); }

//- /lib.rs crate:lib deps:core new_source_root:library
#[macro_export]
macro_rules! shown { () => { () } }
pub struct Shown;
pub enum E { A, B(u8) }
pub use E::*;
pub mod deep;
mod made { macro_rules! m { () => { pub fn made() {} } } m!(); }
pub use made::made;
use core::Missing;

//- /deep.rs
pub mod deeper { pub fn f() {} }
pub trait T { fn g(&self) -> u8; }
impl T for super::Shown { fn g(&self) -> u8 { 1 } }

//- /core.rs crate:core new_source_root:library
#[lang = "sized"]
pub trait Sized {}
#[rustc_builtin_macro]
pub macro derive {}
pub mod prelude { pub mod rust_2018 { pub use crate::derive; } }
"#;

/// The differences frozen bytes record are the process's: the tests that read them take turns.
pub(super) static TURN: Lock<()> = Lock::new("test.def.deps.turn", ());

const BUILD: ContentId = ContentId([7; 32]);

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
struct Memory {
    images: Lock<HashMap<ContentId, Arc<Aligned>, Global>>,
    gets: Lock<usize>,
}

impl Default for Memory {
    fn default() -> Memory {
        Memory {
            images: Lock::new("test.def.deps.memory.images", Default::default()),
            gets: Lock::new("test.def.deps.memory.gets", 0),
        }
    }
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        *self.gets.lock() += 1;
        Ok(self.images.lock().get(key).map(|it| it.clone() as ImageBuf))
    }

    fn reads_now(&self, _: &Env, shape: &[u8]) -> Result<ContentId, String> {
        Ok(values_of(shape))
    }

    fn put(&self, key: ContentId, image: Vec<u8>) -> Result<(), String> {
        let mut images = self.images.lock();
        match images.get(&key) {
            Some(kept) if kept[..] != image[..] => Err(format!("{key:?} kept with other bytes")),
            Some(_) => Ok(()),
            None => {
                images.insert(key, Arc::new(Aligned::copy_from(&image)));
                Ok(())
            }
        }
    }
}

fn all(db: &TestDB) -> Vec<Crate> {
    base_db::all_crates(db).iter().copied().collect()
}

/// A store with the images of a fresh database of `fixture`.
fn stored(fixture: &str) -> (Memory, Stored) {
    let mut db = fixture::db(fixture).into_db();
    let memory = Memory::default();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new());
    let stored = store(&db, &served, &memory).unwrap();
    (memory, stored)
}

fn names(db: &TestDB, crates: &[Crate]) -> Vec<String> {
    let mut names: Vec<String> = crates.iter().map(|&it| it.named(db).to_string()).collect();
    names.sort();
    names
}

#[test]
fn a_crate_is_named_in_an_error_by_its_rust_name() {
    let db = fixture::db(FIXTURE).into_db();
    for krate in all(&db) {
        let named = krate.named(&db).to_string();
        match &krate.extra_data(&db).display_name {
            Some(name) => assert_eq!(named, name.crate_name().as_str()),
            None => assert!(named.starts_with("{unnamed crate "), "{named}"),
        }
    }
}

#[test]
fn only_crates_outside_the_workspace_are_eligible() {
    let db = fixture::db(FIXTURE).into_db();
    assert!(eligible(&db, crate_named(&db, "lib")));
    assert!(eligible(&db, crate_named(&db, "core")));
    assert!(!eligible(&db, crate_named(&db, "main")));
    let keys = keys(&db, &all(&db), BUILD);
    assert_eq!(names(&db, &keys.keys().copied().collect::<Vec<_>>()), ["core", "lib"]);
}

#[test]
fn a_library_on_a_member_is_not_eligible() {
    let db = fixture::db(
        "//- /lib.rs crate:lib deps:member new_source_root:library\n\
         pub use member::X;\n\
         //- /member.rs crate:member new_source_root:local\n\
         pub struct X;\n",
    )
    .into_db();
    assert!(!eligible(&db, crate_named(&db, "lib")));
}

fn key_of(fixture: &str, name: &str, build: ContentId) -> ContentId {
    let db = fixture::db(fixture).into_db();
    let krate = crate_named(&db, name);
    keys(&db, &[krate], build)[&krate]
}

#[test]
fn keys_are_the_same_in_every_database() {
    assert_eq!(key_of(FIXTURE, "lib", BUILD), key_of(FIXTURE, "lib", BUILD));
}

#[test]
fn a_key_covers_every_input() {
    let lib = key_of(FIXTURE, "lib", BUILD);
    let core = key_of(FIXTURE, "core", BUILD);
    let changed =
        |fixture: String, name| assert_ne!(key_of(&fixture, name, BUILD), lib, "{fixture}");
    // A file of the crate that is not its root.
    changed(FIXTURE.replace("pub mod deeper", "pub mod other"), "lib");
    // A dependency's text, which the crate's content ID leaves out.
    changed(FIXTURE.replace("pub trait Sized {}", "pub trait Sized {}\npub struct More;"), "lib");
    // The crate's cfg and edition.
    changed(FIXTURE.replace("crate:lib deps:core", "crate:lib deps:core cfg:feature=x"), "lib");
    changed(FIXTURE.replace("crate:lib deps:core", "crate:lib deps:core edition:2015"), "lib");
    // The blaze reading.
    assert_ne!(key_of(FIXTURE, "lib", ContentId([8; 32])), lib);
    // A dependent's text changes nothing of its dependencies.
    assert_eq!(key_of(&FIXTURE.replace("pub mod deeper", "pub mod other"), "core", BUILD), core);
    // Nor does a member's.
    assert_eq!(key_of(&FIXTURE.replace("fn main()", "fn main2()"), "lib", BUILD), lib);
}

#[test]
fn an_image_reads_back_its_parts() {
    let key = ContentId([1; 32]);
    let parts: [&[u8]; 3] = [b"def map", b"", &[9; 33]];
    let bytes = Aligned::copy_from(&write_image(key, parts));
    let image = ImageRef::open(&bytes, key).unwrap();
    for (part, bytes) in Part::ALL.into_iter().zip(parts) {
        assert_eq!(image.part(part), bytes, "{part:?}");
        assert!(bytes.is_empty() || (image.part(part).as_ptr() as usize).is_multiple_of(16));
    }
    assert_eq!(write_image(key, parts), write_image(key, parts));
}

#[test]
fn an_image_under_another_key_does_not_read() {
    let bytes = Aligned::copy_from(&write_image(ContentId([1; 32]), [b"a", b"b", b"c"]));
    assert_eq!(
        ImageRef::open(&bytes, ContentId([2; 32])).err(),
        Some(ImageError::Key { found: ContentId([1; 32]), wanted: ContentId([2; 32]) })
    );
}

/// Every byte of an image flipped in turn: it reads, with each part inside the buffer, or it
/// does not, and never panics. The content hash, which catches every flip, is the store's.
#[test]
fn damaged_images_read_within_bounds_or_not_at_all() {
    let key = ContentId([1; 32]);
    let written = write_image(key, [b"def map", b"lang", b"signatures"]);
    let mut read = 0;
    for at in 0..written.len() {
        let mut bytes = written.clone();
        bytes[at] ^= 0xff;
        let bytes = Aligned::copy_from(&bytes);
        if let Ok(image) = ImageRef::open(&bytes, key) {
            read += 1;
            for part in Part::ALL {
                let part = image.part(part);
                assert!(part.is_empty() || bytes.as_ptr_range().contains(&part.as_ptr()), "{at}");
            }
        }
    }
    assert!(read < written.len(), "no flip was refused");
}

#[test]
fn every_dependency_is_stored_and_served() {
    let _turn = TURN.lock();
    let (memory, stored) = stored(FIXTURE);
    assert_eq!(stored.refused, Vec::new());
    let mut db = fixture::db(FIXTURE).into_db();
    assert_eq!(names(&db, &stored.written), ["core", "lib"]);
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    for name in ["core", "lib"] {
        let krate = crate_named(&db, name);
        assert_eq!(mode(&db, krate), Some(Mode::Serve));
        assert!(image(&db, krate).is_some());
    }
    assert_eq!(mode(&db, crate_named(&db, "main")), None);
    // Nothing more to store, and a second store of the same keys keeps the same bytes.
    let again = store(&db, &served, &memory).unwrap();
    assert_eq!((again.written.len(), again.refused.len()), (0, 0));
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

/// The key `store` says each image is filed under is the one `serve` then gives it, with reads
/// and without.
#[test]
fn a_stored_image_is_served_under_the_key_store_filed_it_by() {
    let _turn = TURN.lock();
    for fixture in [FIXTURE, READING] {
        let memory = Memory::default();
        let open =
            || if fixture == READING { reading_db() } else { fixture::db(fixture).into_db() };
        let mut db = open();
        let crates = all(&db);
        let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
        let stored = store(&db, &served, &memory).unwrap();
        assert_eq!(stored.filed.len(), stored.written.len());
        let filed: HashMap<String, ContentId, Global> =
            stored.filed.iter().map(|(&krate, &key)| (krate.named(&db).to_string(), key)).collect();
        let mut db = open();
        let crates = all(&db);
        serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
        let raw: &Db = &db;
        for (krate, image) in DepImages::try_get(raw).unwrap().images(raw).iter() {
            assert_eq!(Some(&image.key()), filed.get(&krate.named(raw).to_string()));
        }
        assert!(filed.contains_key("lib"), "{filed:?}");
    }
}

/// The def maps (with their diagnostics) and lang items of a database that read every
/// dependency from its image, as the frozen encoding writes them, against a fresh
/// database's.
#[test]
fn a_served_dependency_resolves_as_a_fresh_one() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    let fresh = fixture::db(FIXTURE).into_db();
    for mode in [Mode::Serve, Mode::Verify, Mode::Check] {
        let mut db = fixture::db(FIXTURE).into_db();
        let crates = all(&db);
        serve(&mut db, &crates, BUILD, &memory, mode).unwrap();
        for name in ["core", "lib", "main"] {
            let (krate, live) = (crate_named(&db, name), crate_named(&fresh, name));
            let def_map = crate::nameres::crate_def_map(&db, krate);
            let fresh_map = crate::nameres::crate_def_map(&fresh, live);
            assert_eq!(
                def_map.diagnostics(&db).len(),
                fresh_map.diagnostics(&fresh).len(),
                "{name} {mode:?}"
            );
            assert_eq!(frozen::encode(&db, krate), frozen::encode(&fresh, live), "{name} {mode:?}");
            assert_eq!(
                frozen::encode_lang_items(&db, krate),
                frozen::encode_lang_items(&fresh, live),
                "{name} {mode:?}"
            );
        }
        assert_eq!(frozen::differences(), Vec::<String>::new(), "{mode:?}");
    }
}

#[test]
fn an_unresolved_import_of_a_dependency_is_still_reported() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let lib = crate::nameres::crate_def_map(&db, crate_named(&db, "lib"));
    assert!(!lib.diagnostics(&db).is_empty(), "`use core::Missing` is unresolved");
}

#[test]
fn an_edit_to_a_dependency_misses_its_image_and_its_dependents() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    let edited = FIXTURE.replace("pub trait Sized {}", "pub trait Sized {}\npub struct More;");
    let mut db = fixture::db(&edited).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new());
    let stored = store(&db, &served, &memory).unwrap();
    assert_eq!(names(&db, &stored.written), ["core", "lib"]);
}

#[test]
fn an_image_written_under_another_key_is_an_error() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    let db = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&db, "lib");
    let key = keys(&db, &[lib], BUILD)[&lib];
    let other = ContentId([3; 32]);
    let image = memory.images.lock()[&key].clone();
    memory.images.lock().insert(other, image.clone());
    memory
        .images
        .lock()
        .insert(key, Arc::new(Aligned::copy_from(&write_image(other, [b"", b"", b""]))));
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    let error = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap_err();
    assert!(error.contains("the image of crate lib"), "{error}");
    assert!(error.contains("written under key"), "{error}");
}

/// An expansion whose server said nothing of what it read leaves an input out: its crate is
/// refused, and a library of macros with no known code is not keyed at all.
#[test]
fn a_crate_with_an_expansion_of_unknown_reads_is_refused() {
    let db = fixture::db(
        "//- proc_macros: identity\n\
         //- /lib.rs crate:lib new_source_root:library\n\
         pub mod inner { #[proc_macros::identity] pub fn f() {} }\n",
    )
    .into_db();
    let lib = crate_named(&db, "lib");
    assert!(!eligible(&db, lib), "the fixture's macros have no library");
    let refused = Reads::of(&db, lib, &[]).unwrap_err();
    assert!(
        matches!(&refused, Refused::ProcMacro { call, .. } if call.contains("proc_macros")),
        "{refused:?}"
    );
}

/// The value of the inputs [`Reading`]'s expansions read: it stands for a file the macro
/// reads, which a test changes. The tests that change it or [`LIBRARY`] take [`TURN`].
static VALUE: AtomicU8 = AtomicU8::new(0);
/// The byte [`Reading`]'s library is made of, which a test changes as a rebuild of it would.
static LIBRARY: AtomicU8 = AtomicU8::new(1);
const SHAPE: &[u8] = b"the names a test expansion reads";

fn values_of(shape: &[u8]) -> ContentId {
    ContentId::of("blaze v2 deps test values", &[shape, &[VALUE.load(SeqCst)]].concat())
}

/// An attribute that gives back its item and reads [`SHAPE`], as a traced server reports it.
#[derive(Debug)]
struct Reading;

impl ProcMacroExpander for Reading {
    fn expand(&self, _: &Db, call: ProcMacroCall<'_>) -> Result<Expanded, ProcMacroExpansionError> {
        let reads = ExpansionReads::Traced { shape: SHAPE.into(), values: values_of(SHAPE) };
        Ok(Expanded { tree: call.subtree.copy(), reads, state: StateUse::Fixed })
    }

    fn code(&self) -> Option<ContentId> {
        Some(ContentId([LIBRARY.load(SeqCst); 32]))
    }

    // dyn: `ProcMacroExpander::eq_dyn`'s signature, compared when a crate's macros are set
    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        other.type_id() == TypeId::of::<Self>()
    }
}

const READING: &str = r#"
//- proc_macros: reading
//- /lib.rs crate:lib new_source_root:library
#[proc_macros::reading]
pub struct Shown;
#[proc_macros::reading]
pub fn f() -> Shown { Shown }
pub mod inner { #[proc_macros::reading] pub trait T {} }
"#;

/// [`READING`] and a library on it that imports what its expansions made.
const READING_APP: &str = r#"
//- proc_macros: reading
//- /lib.rs crate:lib new_source_root:library
#[proc_macros::reading]
pub struct Shown;
#[proc_macros::reading]
pub fn f() -> Shown { Shown }
pub mod inner { #[proc_macros::reading] pub trait T {} }
//- /app.rs crate:app deps:lib new_source_root:library
pub use lib::Shown;
pub fn g() -> Shown { lib::f() }
"#;

/// A database of [`READING`], its library of macros a dependency as a registry's would be.
fn reading_db() -> TestDB {
    reading_db_of(READING)
}

/// [`reading_db`] of `fixture`, one of [`READING`] and [`READING_APP`].
fn reading_db_of(fixture: &str) -> TestDB {
    let source = "#[proc_macro_attribute]\n\
                  pub fn reading(_attr: TokenStream, item: TokenStream) -> TokenStream { item }\n";
    let reading = ProcMacro {
        name: intern::Symbol::intern("reading"),
        kind: ProcMacroKind::Attr,
        expander: Arc::new(Reading),
        disabled: false,
    };
    let mut db = fixture::db_with_proc_macros(fixture, vec![(source.into(), reading)]).into_db();
    let macros = crate_named(&db, "proc_macros");
    let mut data = macros.data(&db).clone();
    data.origin = CrateOrigin::Library { repo: None, name: intern::Symbol::intern("proc_macros") };
    macros.set_data(&mut db, data);
    db
}

/// Stores the images of a fresh [`reading_db`] in `memory`: the names of the crates written.
fn store_reading(memory: &Memory) -> Vec<String> {
    let mut db = reading_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, memory, Mode::Serve).unwrap();
    names(&db, &store(&db, &served, memory).unwrap().written)
}

/// A crate with items from proc-macro expansions that read files is imaged, filed by what
/// they read, and resolves from its image as a fresh one does: its def map, its diagnostics
/// and every signature, byte for byte in their portable encodings.
#[test]
fn a_crate_with_traced_expansions_resolves_from_its_image_as_a_fresh_one() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    let memory = Memory::default();
    let written = store_reading(&memory);
    assert!(written.contains(&"lib".to_owned()), "{written:?}");
    let fresh = reading_db();
    let live = crate_named(&fresh, "lib");
    let reads = Reads::of(&fresh, live, &[]).unwrap();
    assert_eq!(reads, Reads::for_tests(&[], &[(SHAPE, values_of(SHAPE))]));
    for mode in [Mode::Serve, Mode::Verify, Mode::Check] {
        let mut db = reading_db();
        let crates = all(&db);
        let served = serve(&mut db, &crates, BUILD, &memory, mode).unwrap();
        let lib = crate_named(&db, "lib");
        assert!(served.hits.contains(&lib), "{mode:?}");
        let raw: &Db = &db;
        let filed = DepImages::try_get(raw).unwrap().images(raw)[&lib].key();
        assert_ne!(filed, served.keys[&lib], "an image with reads is filed by them, not the key");
        let def_map = crate::nameres::crate_def_map(&db, lib);
        let fresh_map = crate::nameres::crate_def_map(&fresh, live);
        assert_eq!(def_map.diagnostics(&db).len(), fresh_map.diagnostics(&fresh).len(), "{mode:?}");
        assert_eq!(frozen::encode(&db, lib), frozen::encode(&fresh, live), "{mode:?}");
        assert_eq!(
            signatures::encode(&db, lib, &mut Vec::new()),
            signatures::encode(&fresh, live, &mut Vec::new()),
            "{mode:?}"
        );
        assert_eq!(frozen::differences(), Vec::<String>::new(), "{mode:?}");
    }
}

/// Once a value an expansion read is another, the image is not found and the crate is stale;
/// imaged again, it is filed in the same slot under the new value and found.
#[test]
fn a_changed_read_misses_the_image_and_the_next_one_is_found() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    let memory = Memory::default();
    store_reading(&memory);
    VALUE.store(1, SeqCst);
    let mut db = reading_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let lib = crate_named(&db, "lib");
    assert!(!served.hits.contains(&lib));
    assert!(served.stale.contains(&lib));
    let stored = store(&db, &served, &memory).unwrap();
    assert!(stored.written.contains(&lib), "{stored:?}");
    let key = served.keys[&lib];
    let images = memory.images.lock();
    assert!(images.contains_key(&reads::slot(key, 0)));
    assert!(!images.contains_key(&reads::slot(key, 1)), "the same names share a slot");
    drop(images);
    let mut db = reading_db();
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert!(served.hits.contains(&crate_named(&db, "lib")));
    VALUE.store(0, SeqCst);
}

/// [`READING`]'s attribute at assoc-item position alone: on a method of an impl, where no
/// signature's store names the call.
const READING_IN_IMPL: &str = r#"
//- proc_macros: reading
//- /lib.rs crate:lib new_source_root:library
pub struct S;
impl S { #[proc_macros::reading] pub fn m() {} }
"#;

/// A proc macro among an impl's items makes an item the image holds, so what it read files
/// the image: once that value is another, the image is not served though the crate's key is
/// the same (rule 21).
#[test]
fn a_read_of_a_proc_macro_among_an_impls_items_files_the_image() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    let memory = Memory::default();
    let serve_lib = || {
        let mut db = reading_db_of(READING_IN_IMPL);
        let crates = all(&db);
        let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
        let lib = crate_named(&db, "lib");
        (db, served, lib)
    };
    let fresh = reading_db_of(READING_IN_IMPL);
    let reads = Reads::of(&fresh, crate_named(&fresh, "lib"), &[]).unwrap();
    assert_eq!(
        reads,
        Reads::for_tests(&[], &[(SHAPE, values_of(SHAPE))]),
        "the impl's call is read"
    );

    let (db, served, lib) = serve_lib();
    let stored = store(&db, &served, &memory).unwrap();
    assert!(stored.written.contains(&lib), "{stored:?}");
    let (_db, served, lib) = serve_lib();
    assert!(served.hits.contains(&lib));

    VALUE.store(1, SeqCst);
    let (_db, served, lib) = serve_lib();
    assert!(!served.hits.contains(&lib), "served with what the impl's expansion made before");
    assert!(served.stale.contains(&lib));
    VALUE.store(0, SeqCst);
}

/// A crate's image holds what its dependencies' expansions made (`app` imports `lib`'s
/// `Shown`, which an expansion gave back), and neither crate's key covers what those
/// expansions read: once a value one of them read is another, the dependent's image is not
/// served either, though its key is the same (rule 21). Its reads are the same whether the
/// dependency was resolved or served as it was imaged, so either files it under one key.
#[test]
fn a_dependents_image_is_not_served_once_a_dependencys_expansion_reads_another_value() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    let memory = Memory::default();
    let serve_app = || {
        let mut db = reading_db_of(READING_APP);
        let crates = all(&db);
        let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
        let (lib, app) = (crate_named(&db, "lib"), crate_named(&db, "app"));
        (db, served, lib, app)
    };
    let (db, served, lib, app) = serve_app();
    let stored = store(&db, &served, &memory).unwrap();
    assert!(stored.written.contains(&lib) && stored.written.contains(&app), "{stored:?}");

    let (db, served, lib, app) = serve_app();
    assert!(served.hits.contains(&lib) && served.hits.contains(&app));
    let raw: &Db = &db;
    let filed = DepImages::try_get(raw).unwrap().images(raw)[&app].key();
    assert_ne!(filed, served.keys[&app], "a dependency's reads file the dependent's image");
    assert!(memory.images.lock().remove(&filed).is_some());

    // Imaged again with `lib` served from its image, not resolved: the same reads.
    let (db, served, lib, app) = serve_app();
    assert!(served.hits.contains(&lib) && !served.hits.contains(&app));
    let stored = store(&db, &served, &memory).unwrap();
    assert!(stored.written.contains(&app), "{stored:?}");
    assert!(memory.images.lock().contains_key(&filed), "the same reads file it under the same key");

    VALUE.store(1, SeqCst);
    let (db, served, lib, app) = serve_app();
    assert!(!served.hits.contains(&lib));
    assert!(!served.hits.contains(&app), "served with what `lib`'s expansions made before");
    assert!(served.stale.contains(&app));
    store(&db, &served, &memory).unwrap();
    let (_db, served, lib, app) = serve_app();
    assert!(served.hits.contains(&lib) && served.hits.contains(&app));
    VALUE.store(0, SeqCst);
}

/// The expansion log has what a crate's expansions read, with their values, whether they ran
/// or the crate was served from an image filed by them, which runs none: what keys a value by
/// the log (`blaze-cargo`'s unit records) sees the same reads either way.
#[test]
fn the_expansion_log_has_a_crates_reads_whether_they_ran_or_were_served() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    let logged = |db: &TestDB| -> Vec<base_db::ExpansionRead> {
        let lib = crate_named(db, "lib");
        db.files()
            .expansions_read()
            .into_iter()
            .filter(|(by, _)| *by == lib)
            .map(|(_, it)| it)
            .collect()
    };
    let read =
        vec![base_db::ExpansionRead::Traced { shape: SHAPE.into(), values: values_of(SHAPE) }];
    let memory = Memory::default();
    store_reading(&memory);
    let ran = reading_db();
    let _ = crate::nameres::crate_def_map(&ran, crate_named(&ran, "lib"));
    assert_eq!(logged(&ran), read, "resolved: each expansion ran");
    let mut db = reading_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert!(served.hits.contains(&crate_named(&db, "lib")));
    assert_eq!(logged(&db), read, "served from its image, filed by those reads");
}

/// Whether [`Flaky`]'s expansions report their reads: off, they are unknown, as a server that
/// could not trace them reports.
static TRACES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// An attribute that gives back its item and reads [`SHAPE`] when [`TRACES`] is on, and what
/// is not known when it is off.
#[derive(Debug)]
struct Flaky;

impl ProcMacroExpander for Flaky {
    fn expand(&self, _: &Db, call: ProcMacroCall<'_>) -> Result<Expanded, ProcMacroExpansionError> {
        let reads = match TRACES.load(SeqCst) {
            true => ExpansionReads::Traced { shape: SHAPE.into(), values: values_of(SHAPE) },
            false => ExpansionReads::Unknown("its server could not trace it"),
        };
        Ok(Expanded { tree: call.subtree.copy(), reads, state: StateUse::Fixed })
    }

    fn code(&self) -> Option<ContentId> {
        Some(ContentId([LIBRARY.load(SeqCst); 32]))
    }

    // dyn: `ProcMacroExpander::eq_dyn`'s signature, compared when a crate's macros are set
    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        other.type_id() == TypeId::of::<Self>()
    }
}

/// `lib`'s first file is [`Flaky`]'s call; `main` depends on `lib`.
const FLAKY: &str = r#"
//- proc_macros: flaky
//- /lib.rs crate:lib new_source_root:library
#[proc_macros::flaky]
pub struct Made;
//- /main.rs crate:main deps:lib new_source_root:local
pub fn f() -> lib::Made { lib::Made }
"#;

/// A database of [`FLAKY`], and `lib.rs`.
fn flaky_db() -> (TestDB, base_db::FileId) {
    let source = "#[proc_macro_attribute]\n\
                  pub fn flaky(_attr: TokenStream, item: TokenStream) -> TokenStream { item }\n";
    let flaky = ProcMacro {
        name: intern::Symbol::intern("flaky"),
        kind: ProcMacroKind::Attr,
        expander: Arc::new(Flaky),
        disabled: false,
    };
    let fixture = fixture::db_with_proc_macros(FLAKY, vec![(source.into(), flaky)]);
    let file = fixture.files()[0];
    let db = fixture.into_db();
    let file = file.file_id(&db);
    (db, file)
}

/// The log has each call's latest expansion. Once a macro whose expansion was unknown expands
/// again and traces (its call edited, as the next build after its server is fixed expands it),
/// the unknown read is gone from the log and a dependent's reads are kept again, in the same
/// database: no restart. A call edited out of its crate leaves its unknown read in the log,
/// and the crate's calls as they are now are what decide.
#[test]
fn an_unknown_expansion_that_traces_once_expanded_again_stops_refusing_its_dependents() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    TRACES.store(false, SeqCst);
    let (mut db, file) = flaky_db();
    let (lib, main) = (crate_named(&db, "lib"), crate_named(&db, "main"));
    let unknown = |db: &TestDB| {
        let log = db.files().expansions_read();
        log.iter()
            .any(|(by, read)| *by == lib && matches!(read, base_db::ExpansionRead::Unknown(_)))
    };
    let _ = crate::nameres::crate_def_map(&db, lib);
    assert!(unknown(&db), "the expansion ran and could not be traced");
    let refused = closure_expansion_reads(&db, main).unwrap_err();
    assert!(
        matches!(&refused, Refused::ProcMacro { why: "its server could not trace it", .. }),
        "{refused:?}"
    );

    TRACES.store(true, SeqCst);
    db.set_file_text(file, "#[proc_macros::flaky]\npub struct Made {}\n");
    let _ = crate::nameres::crate_def_map(&db, lib);
    assert!(!unknown(&db), "the call's latest expansion traced: its unknown read is replaced");
    let read = closure_expansion_reads(&db, main).unwrap();
    assert_eq!(
        read,
        vec![(lib, vec![(Box::<[u8]>::from(SHAPE), values_of(SHAPE))])],
        "main's reads are kept again"
    );

    TRACES.store(false, SeqCst);
    db.set_file_text(file, "#[proc_macros::flaky]\npub struct Made;\n");
    let _ = crate::nameres::crate_def_map(&db, lib);
    assert!(closure_expansion_reads(&db, main).is_err(), "unknown again");
    db.set_file_text(file, "pub struct Made;\n");
    let _ = crate::nameres::crate_def_map(&db, lib);
    assert!(unknown(&db), "the call is gone, and what it last read stays in the log");
    assert!(
        closure_expansion_reads(&db, main).is_ok(),
        "the crate's calls as they are now refuse nothing"
    );
    TRACES.store(false, SeqCst);
}

/// A rebuilt library of macros is another library: its dependents' keys change with it.
#[test]
fn a_key_covers_the_library_of_its_macros() {
    let _turn = TURN.lock();
    let key = || {
        let db = reading_db();
        let lib = crate_named(&db, "lib");
        keys(&db, &[lib], BUILD)[&lib]
    };
    let before = key();
    LIBRARY.store(2, SeqCst);
    let after = key();
    LIBRARY.store(1, SeqCst);
    assert_ne!(before, after);
    assert_eq!(before, key());
}

/// The process variables a crate's `env!`s read are its reads, and their values file it.
#[test]
fn process_variables_read_file_the_image_by_their_values() {
    let mut db = fixture::db(FIXTURE).into_db();
    let reads = Reads::for_tests(&["BLAZE_DEPS_TEST"], &[]);
    let of = crate_named(&db, "lib");
    let unset = reads.values_then(&db, of);
    base_db::set_process_env(&mut db, Env::from_iter([("BLAZE_DEPS_TEST".into(), "1".into())]));
    let one = reads.values_then(&db, of);
    base_db::set_process_env(&mut db, Env::from_iter([("BLAZE_DEPS_TEST".into(), "2".into())]));
    let two = reads.values_then(&db, of);
    assert!(unset != one && one != two && unset != two);
    let record = reads.encode();
    let key = ContentId([4; 32]);
    assert_ne!(reads::image_key(key, &record, one), reads::image_key(key, &record, two));
    let lib = crate_named(&db, "lib");
    assert_eq!(reads.values_now(&db, lib, &Memory::default(), |_, _| {}).unwrap(), two);
}

#[test]
fn reads_read_back_and_damaged_ones_do_not_panic() {
    let reads = Reads::for_tests(
        &["A", "B"],
        &[(b"one", ContentId([1; 32])), (b"two", ContentId([2; 32]))],
    );
    let (env, shapes) = reads.names();
    assert_eq!(env, [Box::<str>::from("A"), "B".into()]);
    assert_eq!(shapes.collect::<Vec<_>>(), [b"one" as &[u8], b"two"]);
    let bytes = reads.encode();
    let back = Reads::decode(&bytes).unwrap();
    assert_eq!(back.encode(), bytes);
    assert!(!back.is_empty() && Reads::default().is_empty());
    for at in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0xff;
        if let Ok(read) = Reads::decode(&damaged) {
            assert_eq!(read.encode(), damaged, "what decodes encodes back to its bytes");
        }
        assert!(Reads::decode(&bytes[..at]).is_err(), "{at} bytes of {}", bytes.len());
    }
}

/// What [`Reads::parts_for_tests`] gives: the variables, the expansions' names, each
/// dependency's by its index, and the crates that read `CARGO_PRIMARY_PACKAGE`.
pub(super) type ReadsParts = (Vec<Box<str>>, Vec<Box<[u8]>>, Vec<(u32, Vec<Box<[u8]>>)>, Vec<u32>);

/// The version of the reads' encoding [`old_reads`] reads.
const OLD_READS_VERSION: u8 = 3;

/// The reads' decoder as it was before `blaze_format::cursor`, hand-rolled, written for the
/// format at [`OLD_READS_VERSION`]: its parts, or none where it refuses.
fn old_reads(bytes: &[u8]) -> Option<ReadsParts> {
    fn take<'b>(rest: &mut &'b [u8], len: usize) -> Option<&'b [u8]> {
        if rest.len() < len {
            return None;
        }
        let (taken, after) = rest.split_at(len);
        *rest = after;
        Some(taken)
    }
    fn u32_of(rest: &mut &[u8]) -> Option<u32> {
        Some(u32::from_le_bytes(take(rest, 4)?.try_into().expect("four bytes")))
    }
    fn count(rest: &mut &[u8]) -> Option<usize> {
        u32_of(rest).map(|it| it as usize)
    }
    fn framed(rest: &mut &[u8]) -> Option<Box<[u8]>> {
        let len = count(rest)?;
        take(rest, len).map(Box::from)
    }
    fn increasing<T: PartialOrd>(items: &[T]) -> bool {
        items.windows(2).all(|pair| pair[0] < pair[1])
    }
    let mut rest = bytes;
    if take(&mut rest, 1)? != [OLD_READS_VERSION] {
        return None;
    }
    let (mut env, mut macros, mut deps, mut primary) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for _ in 0..count(&mut rest)? {
        let len = count(&mut rest)?;
        env.push(Box::<str>::from(std::str::from_utf8(take(&mut rest, len)?).ok()?));
    }
    for _ in 0..count(&mut rest)? {
        macros.push(framed(&mut rest)?);
    }
    for _ in 0..count(&mut rest)? {
        let at = u32_of(&mut rest)?;
        let mut shapes = Vec::new();
        for _ in 0..count(&mut rest)? {
            shapes.push(framed(&mut rest)?);
        }
        if shapes.is_empty() || !increasing(&shapes) {
            return None;
        }
        deps.push((at, shapes));
    }
    for _ in 0..count(&mut rest)? {
        primary.push(u32_of(&mut rest)?);
    }
    let sorted = increasing(&env)
        && increasing(&macros)
        && deps.windows(2).all(|pair| pair[0].0 < pair[1].0)
        && increasing(&primary);
    (rest.is_empty() && sorted).then_some((env, macros, deps, primary))
}

#[test]
fn reads_decode_as_the_old_decoder_did() {
    assert_eq!(
        Reads::default().encode().first(),
        Some(&OLD_READS_VERSION),
        "the reads' encoding moved past the version `old_reads` is written for: \
         port `old_reads` to the new format (and `OLD_READS_VERSION` with it)"
    );
    let samples = [
        Reads::default(),
        Reads::for_tests(&["A"], &[]),
        Reads::for_tests(&[], &[(b"", ContentId([1; 32]))]),
        Reads::for_tests(
            &["", "PATH", "\u{e9}t\u{e9}"],
            &[(b"one", ContentId([1; 32])), (b"two", ContentId([2; 32]))],
        ),
        Reads::default().with_deps_for_tests(&[(0, &[b""])], &[]),
        Reads::default().with_deps_for_tests(&[], &[7]),
        Reads::for_tests(&["B"], &[(b"own", ContentId([3; 32]))])
            .with_deps_for_tests(&[(1, &[b"a", b"bc"]), (4, &[b"d"])], &[1, 2]),
    ];
    let mut rng = blaze_ids::rng::SplitMix64::new(0x5eed_0f_4ead5);
    for reads in &samples {
        let bytes = reads.encode();
        assert_eq!(old_reads(&bytes), Some(reads.parts_for_tests()), "{reads:?}");
        let mut inputs = vec![bytes.clone()];
        for at in 0..=bytes.len() {
            inputs.push(bytes[..at].to_vec());
            if at < bytes.len() {
                let mut damaged = bytes.clone();
                damaged[at] ^= 1u8 << rng.below(8);
                inputs.push(damaged);
            }
        }
        inputs.push([&bytes[..], &[0]].concat());
        for input in inputs {
            let new = Reads::decode(&input).ok().map(|it| it.parts_for_tests());
            assert_eq!(new, old_reads(&input), "{input:?}");
        }
    }
}

/// Two checkouts of the same crates at different paths, with different `OUT_DIR`s and other
/// paths in their variables, have the same keys; any other input gives other keys.
#[test]
fn keys_hold_no_path_of_the_checkout() {
    let checkout = |dir: &str, out: &str, text: &str| {
        let cargo =
            if dir.contains("alice") { "/home/alice/.cargo/bin/cargo" } else { "/usr/bin/cargo" };
        format!(
            "//- {dir}/src/lib.rs crate:lib deps:core new_source_root:library \
             env:CARGO_MANIFEST_DIR={dir},OUT_DIR={out},CARGO={cargo}\n\
             {text}\n\
             pub mod deep;\n\
             //- {dir}/src/deep.rs\n\
             pub fn f() {{}}\n\
             //- /sysroot/core/lib.rs crate:core new_source_root:library\n\
             pub struct Core;\n"
        )
    };
    let alice = checkout("/home/alice/work", "/home/alice/work/target/out", "pub struct A;");
    let bob = checkout("/srv/build/bob/deps/x", "/tmp/target-77/out", "pub struct A;");
    assert_eq!(key_of(&alice, "lib", BUILD), key_of(&bob, "lib", BUILD));
    assert_eq!(key_of(&alice, "core", BUILD), key_of(&bob, "core", BUILD));
    let other = checkout("/home/alice/work", "/home/alice/work/target/out", "pub struct B;");
    assert_ne!(key_of(&alice, "lib", BUILD), key_of(&other, "lib", BUILD));
    let moved = alice.replace("//- /home/alice/work/src/deep.rs", "//- /home/alice/work/deep.rs");
    assert_ne!(key_of(&alice, "lib", BUILD), key_of(&moved, "lib", BUILD));
    let unset = alice.replace(",CARGO=", ",NOT_CARGO=");
    assert_ne!(key_of(&alice, "lib", BUILD), key_of(&unset, "lib", BUILD));
}

/// An outside source file is named from the crate when its image is restored (#1637).
#[test]
fn moving_a_crate_without_its_outside_source_file_moves_its_image_key() {
    let _turn = TURN.lock();
    let checkout = |dir: &str, outside: &str| {
        // alloc: test fixture
        format!(
            "//- {dir}/src/lib.rs crate:lib new_source_root:library env:CARGO_MANIFEST_DIR={dir}\n\
             #[path = \"{outside}\"] pub mod shared;\n\
             //- {outside}\npub struct Shared;\n"
        )
    };
    let original = checkout("/checkout/pkg", "/shared/code.rs");
    let moved = checkout("/elsewhere/checkout/pkg", "/shared/code.rs");
    assert_ne!(key_of(&original, "lib", BUILD), key_of(&moved, "lib", BUILD));

    // Moving the whole layout keeps both the image's relative name and its key.
    let together = checkout("/elsewhere/checkout/pkg", "/elsewhere/shared/code.rs");
    let together_original = checkout("/checkout/pkg", "/shared/code.rs");
    // The source text contains an absolute include path, which is an input of its own.
    // Use a relative path so only the checkout's placement differs.
    let original = together_original.replace("/shared/code.rs\"]", "../../../shared/code.rs\"]");
    let together = together.replace("/elsewhere/shared/code.rs\"]", "../../../shared/code.rs\"]");
    assert_eq!(key_of(&original, "lib", BUILD), key_of(&together, "lib", BUILD));

    let (memory, _) = stored(&original);
    let mut db = fixture::db(&together).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["lib"]);
    let fresh = fixture::db(&together).into_db();
    let live = crate_named(&fresh, "lib");
    let restored = crate_named(&db, "lib");
    assert_eq!(frozen::encode(&db, restored), frozen::encode(&fresh, live));
    assert_eq!(
        signatures::encode(&db, restored, &mut Vec::new()),
        signatures::encode(&fresh, live, &mut Vec::new())
    );
}

/// A part that holds a path its key writes as a marker refuses the image.
#[test]
fn an_image_holding_a_path_of_the_checkout_is_refused() {
    let base = key::base_for_tests("/home/alice/work", Some("/home/alice/target/out"));
    let clean = [b"def map".to_vec(), b"lang".to_vec(), b"signatures".to_vec()];
    assert_eq!(base.held_by(&clean), None);
    let mut held = clean.clone();
    held[2] = b"..\x01/home/alice/work/src/gen.rs..".to_vec();
    assert_eq!(
        base.held_by(&held),
        Some(Refused::MachinePath { part: Part::Signatures, path: "/home/alice/work".to_owned() })
    );
    assert_eq!(base.portable("/home/alice/work/src/a.rs"), b"\0PKG/src/a.rs");
    assert_eq!(base.portable("/home/alice/target/out/x.rs"), b"\0OUT_DIR/x.rs");
    assert_eq!(base.portable("/elsewhere"), b"/elsewhere");
}

/// The global cache is asked only for what the local store lacks, and what it gives is kept
/// locally, so the next session asks the global cache nothing.
#[test]
fn the_global_cache_is_read_through_the_local_store() {
    let (global, _) = stored(FIXTURE);
    let local = Memory::default();
    *global.gets.lock() = 0;
    let through = ReadThrough { local: &local, global: &global };
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &through, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    assert_eq!(*global.gets.lock(), 2);
    assert_eq!(contents(&local), contents(&global));
    let mut db = fixture::db(FIXTURE).into_db();
    let served = serve(&mut db, &crates, BUILD, &through, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    assert_eq!(*global.gets.lock(), 2, "the second session reads the local store alone");
}

#[test]
fn a_crate_with_no_outside_input_is_not_refused() {
    let db = fixture::db(FIXTURE).into_db();
    for name in ["core", "lib"] {
        assert_eq!(serve::refusal(&db, crate_named(&db, name)), None, "{name}");
        assert_eq!(serve::include_refusal(&db, crate_named(&db, name), &[]), None, "{name}");
    }
}

#[test]
fn serving_asks_the_store_once_a_crate() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    *memory.gets.lock() = 0;
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(*memory.gets.lock(), 2);
}

/// [`Memory`] whose tasks run on four threads, each taking every fourth.
#[derive(Default)]
struct Threads(Memory);

impl ImageStore for Threads {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        self.0.get(key)
    }

    fn reads_now(&self, env: &Env, shape: &[u8]) -> Result<ContentId, String> {
        self.0.reads_now(env, shape)
    }

    fn put(&self, key: ContentId, image: Vec<u8>) -> Result<(), String> {
        self.0.put(key, image)
    }

    // dyn: `ImageStore::for_each`'s signature, one call per batch of crates
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        std::thread::scope(|scope| {
            for first in 0..4 {
                scope.spawn(move || (first..count).step_by(4).for_each(task));
            }
        });
    }
}

#[test]
fn tasks_on_threads_store_and_serve_as_in_order() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    let threads = Threads::default();
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &threads, Mode::Serve).unwrap();
    let stored = store(&db, &served, &threads).unwrap();
    assert_eq!(names(&db, &stored.written), ["core", "lib"]);
    assert_eq!(contents(&threads.0), contents(&memory));
    let mut db = fixture::db(FIXTURE).into_db();
    let served = serve(&mut db, &crates, BUILD, &threads, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
}

/// A served image's parts are slices of the buffer the store gave, not copies.
#[test]
fn served_parts_are_the_store_s_bytes() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let lib = crate_named(&db, "lib");
    let key = keys(&db, &[lib], BUILD)[&lib];
    let kept = memory.images.lock()[&key].clone();
    let part = image(&db, lib).unwrap().part(Part::Signatures);
    assert!(kept.as_ptr_range().contains(&part.as_ptr()) || part.is_empty());
}

/// What `memory` keeps, by key.
fn contents(memory: &Memory) -> Vec<(ContentId, Vec<u8>)> {
    let mut kept: Vec<_> =
        memory.images.lock().iter().map(|(&key, bytes)| (key, bytes.to_vec())).collect();
    kept.sort();
    kept
}

/// What `find_by_reads` finds in `memory`: the filed key, when a value is kept under it.
fn found_in(db: &TestDB, lib: Crate, key: ContentId, memory: &Memory) -> Option<ContentId> {
    find_by_reads(db, lib, key, memory, |at| Ok(memory.get(&at)?.map(|_| at))).unwrap()
}

/// A process variable a body's `env!` reads, not a signature's, files a value made from the
/// crate's bodies in a slot: found while the variable is as it was, not once it changed.
#[test]
fn a_body_s_process_variable_files_a_value_in_a_slot() {
    let mut db = fixture::db(
        "//- /lib.rs crate:lib new_source_root:library\n\
         #[rustc_builtin_macro]\n\
         macro_rules! env {() => {}}\n\
         pub fn f() -> &'static str { env!(\"BLAZE_DEPS_BODY\") }\n",
    )
    .into_db();
    let set = |db: &mut TestDB, value: &str| {
        let vars = Env::from_iter([("BLAZE_DEPS_BODY".into(), value.into())]);
        base_db::set_process_env(db, vars);
    };
    set(&mut db, "1");
    let lib = crate_named(&db, "lib");
    let reads = checked_reads(&db, lib).unwrap();
    assert_eq!(reads, Reads::for_tests(&["BLAZE_DEPS_BODY"], &[]));
    let memory = Memory::default();
    let key = ContentId([5; 32]);
    let filed = file_by_reads(&db, lib, key, &reads, &memory).unwrap().unwrap();
    assert_ne!(filed, key);
    assert!(memory.images.lock().contains_key(&reads::slot(key, 0)));
    memory.put(filed, b"a check".to_vec()).unwrap();
    assert_eq!(found_in(&db, lib, key, &memory), Some(filed));
    set(&mut db, "2");
    assert_eq!(found_in(&db, lib, key, &memory), None);
    set(&mut db, "1");
    assert_eq!(found_in(&db, lib, key, &memory), Some(filed));
}

/// A value found by its reads stands in for the `env!`s that read them, which never run: the
/// `env!` log has their variables as the crate's, as when it is resolved, so what is keyed by
/// the log (a unit's record) does not miss them.
#[test]
fn a_value_found_by_its_reads_notes_their_variables_as_read() {
    let source = "//- /lib.rs crate:lib new_source_root:library\n\
                  #[rustc_builtin_macro]\n\
                  macro_rules! env {() => {}}\n\
                  pub fn f() -> &'static str { env!(\"BLAZE_DEPS_NOTED\") }\n";
    let made = |value: &str| {
        let mut db = fixture::db(source).into_db();
        let vars = Env::from_iter([("BLAZE_DEPS_NOTED".into(), value.into())]);
        base_db::set_process_env(&mut db, vars);
        db
    };
    let db = made("1");
    let lib = crate_named(&db, "lib");
    let reads = checked_reads(&db, lib).unwrap();
    let memory = Memory::default();
    let key = ContentId([7; 32]);
    let filed = file_by_reads(&db, lib, key, &reads, &memory).unwrap().unwrap();
    memory.put(filed, b"a check".to_vec()).unwrap();
    let fresh = made("1");
    let lib = crate_named(&fresh, "lib");
    assert_eq!(fresh.files().env_read(), Vec::new());
    assert_eq!(found_in(&fresh, lib, key, &memory), Some(filed));
    assert_eq!(fresh.files().env_read(), vec![(lib, "BLAZE_DEPS_NOTED".to_owned())]);
    let changed = made("2");
    let lib = crate_named(&changed, "lib");
    assert_eq!(found_in(&changed, lib, key, &memory), None);
    assert_eq!(changed.files().env_read(), Vec::new(), "a value not found notes nothing");
}

/// A crate's outside reads name the values its `env!`s read: the same id while they are as they
/// were, another once one changed, so a record filed by it misses exactly then.
#[test]
fn outside_reads_follow_the_values_read() {
    let source = "//- /lib.rs crate:lib new_source_root:library\n\
                  #[rustc_builtin_macro]\n\
                  macro_rules! env {() => {}}\n\
                  pub fn f() -> &'static str { env!(\"BLAZE_DEPS_OUTSIDE\") }\n";
    let outside = |value: &str| {
        let mut db = fixture::db(source).into_db();
        let vars = Env::from_iter([("BLAZE_DEPS_OUTSIDE".into(), value.into())]);
        base_db::set_process_env(&mut db, vars);
        let lib = crate_named(&db, "lib");
        outside_reads(&db, lib).unwrap()
    };
    assert_eq!(outside("1"), outside("1"));
    assert_ne!(outside("1"), outside("2"));
}

/// A proc macro in a body whose reads are not known refuses the crate's checked reads, though
/// its modules and signatures read nothing.
#[test]
fn a_body_s_expansion_of_unknown_reads_is_refused() {
    let db = fixture::db(
        "//- proc_macros: mirror\n\
         //- /lib.rs crate:lib new_source_root:library\n\
         pub fn f() -> u8 { proc_macros::mirror!(1) }\n",
    )
    .into_db();
    let lib = crate_named(&db, "lib");
    assert!(Reads::of(&db, lib, &[]).unwrap().is_empty());
    let refused = checked_reads(&db, lib).unwrap_err();
    assert!(
        matches!(&refused, Refused::ProcMacro { call, .. } if call.contains("proc_macros")),
        "{refused:?}"
    );
}

/// A crate whose bodies read nothing is filed and found under the key alone, with no slot.
#[test]
fn a_crate_with_no_body_reads_is_found_under_its_key() {
    let db = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&db, "lib");
    let reads = checked_reads(&db, lib).unwrap();
    assert!(reads.is_empty(), "{reads:?}");
    let memory = Memory::default();
    let key = ContentId([6; 32]);
    assert_eq!(file_by_reads(&db, lib, key, &reads, &memory).unwrap(), Ok(key));
    assert!(memory.images.lock().is_empty(), "no slot is written");
    memory.put(key, b"a check".to_vec()).unwrap();
    assert_eq!(found_in(&db, lib, key, &memory), Some(key));
}

/// A crate read from its image adds only its bodies' reads: what its modules and signatures
/// read is in its image's key. Resolved, it has them all.
#[test]
fn an_imaged_crate_s_checked_reads_leave_out_what_its_image_covers() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    let memory = Memory::default();
    store_reading(&memory);
    let fresh = reading_db();
    let live = crate_named(&fresh, "lib");
    assert_eq!(checked_reads(&fresh, live).unwrap(), Reads::of(&fresh, live, &[]).unwrap());
    let mut db = reading_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let lib = crate_named(&db, "lib");
    assert!(served.hits.contains(&lib));
    assert!(checked_reads(&db, lib).unwrap().is_empty());
}

/// Serving a crate at a time adds to what was served: a crate served before under the same key
/// is kept, not fetched again, and one whose key changed is fetched again and, when missing,
/// loses its image.
#[test]
fn a_serve_that_finds_nothing_writes_nothing() {
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    let passes = db.maintain_passes();
    let served = serve(&mut db, &crates, BUILD, &Memory::default(), Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new());
    assert_eq!(
        db.maintain_passes(),
        passes,
        "a cold store's misses change nothing a maintainer reads"
    );
    let raw: &Db = &db;
    assert!(DepImages::try_get(raw).is_none(), "nothing served, nothing given");
}

#[test]
fn serving_a_crate_at_a_time_adds_to_what_was_served() {
    let (memory, _) = stored(FIXTURE);
    let mut db = fixture::db(FIXTURE).into_db();
    let (lib, core) = (crate_named(&db, "lib"), crate_named(&db, "core"));
    let first = serve(&mut db, &[core], BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(first.hits, [core]);
    let gets = *memory.gets.lock();
    let second = serve(&mut db, &[lib], BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &second.hits), ["core", "lib"]);
    assert_eq!(*memory.gets.lock(), gets + 1, "core is kept, not fetched again");
    let raw: &Db = &db;
    let images = DepImages::try_get(raw).unwrap().images(raw);
    assert!(images.contains_key(&lib) && images.contains_key(&core));
    let maps = frozen::FrozenDefMaps::try_get(raw).unwrap().maps(raw);
    assert!(maps.contains_key(&lib) && maps.contains_key(&core));
    assert!(image_of(raw, core).is_some() && image_of(raw, lib).is_some());
    let again = serve(&mut db, &[lib], BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(*memory.gets.lock(), gets + 1, "nothing is fetched again");
    assert_eq!(again.hits, second.hits);
    let other = serve(&mut db, &[lib], ContentId([8; 32]), &memory, Mode::Serve).unwrap();
    assert!(other.hits.is_empty(), "another build has other keys, which have no image");
    let raw: &Db = &db;
    assert!(DepImages::try_get(raw).unwrap().images(raw).is_empty());
    assert!(frozen::FrozenDefMaps::try_get(raw).unwrap().maps(raw).is_empty());
    assert!(image_of(raw, core).is_none());
}

/// A key's inputs are those of the walk that made it: the crate, every crate its key covers
/// once each, and their `OUT_DIR`s as their variables give them.
#[test]
fn key_inputs_are_what_the_key_was_made_from() {
    let db = fixture::db(
        "//- /lib.rs crate:lib deps:mid,core new_source_root:library env:OUT_DIR=/out/lib\n\
         pub use mid::M;\n\
         //- /mid.rs crate:mid deps:core new_source_root:library\n\
         pub use core::C as M;\n\
         //- /core.rs crate:core new_source_root:library env:OUT_DIR=/out/core\n\
         pub struct C;\n",
    )
    .into_db();
    let (lib, mid) = (crate_named(&db, "lib"), crate_named(&db, "mid"));
    let inputs = key_inputs(&db, &[lib], BUILD);
    let keys = keys(&db, &[lib], BUILD);
    assert_eq!(inputs.len(), keys.len());
    for (krate, it) in &inputs {
        assert_eq!((it.key, it.build, it.closure[0]), (keys[krate], BUILD, *krate));
    }
    assert_eq!(names(&db, &inputs[&lib].closure), ["core", "lib", "mid"]);
    assert_eq!(inputs[&lib].closure.len(), 3, "core, a dependency of two, is in it once");
    assert_eq!(inputs[&lib].out_dirs, ["/out/lib", "/out/core"]);
    assert_eq!(inputs[&mid].out_dirs, ["/out/core"]);
}

/// A dependency whose `#[path]` module is in another directory of its package.
const PATH_ELSEWHERE: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library
#[path = "../shared/m.rs"]
pub mod m;
pub use m::f;
//- /registry/lib/shared/m.rs
pub fn f() {}
"#;

/// The key covers a `#[path]` module's file wherever in its package it is: an edit of that file
/// alone gives the crate another key, so the image written for the old text is never served
/// for the new one, and the image is written (the file is not refused as outside).
#[test]
fn a_path_module_in_another_directory_gives_a_new_key_when_its_file_changes() {
    let _turn = TURN.lock();
    let key = key_of(PATH_ELSEWHERE, "lib", BUILD);
    let edited = PATH_ELSEWHERE.replace("pub fn f() {}", "pub fn f() {}\npub fn g() {}");
    assert_ne!(key_of(&edited, "lib", BUILD), key, "an edit of the `#[path]` file moves the key");
    let (_, stored) = stored(PATH_ELSEWHERE);
    assert_eq!(stored.refused, Vec::new());
    assert_eq!(stored.written.len(), 1);
}

/// A `#[path]` module in another package's source root (a registry crate's into a member's
/// files) is known only once the crate is resolved, so no key made before can cover it: the
/// crate is refused an image, never given one that stays served after that file is edited.
#[test]
fn a_path_module_in_another_source_root_is_refused_an_image() {
    let _turn = TURN.lock();
    const INTO_MEMBER: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library
#[path = "../../../ws/member/src/shared.rs"]
pub mod m;
//- /ws/member/src/lib.rs crate:member new_source_root:local
pub mod shared;
//- /ws/member/src/shared.rs
pub fn f() {}
"#;
    let key = key_of(INTO_MEMBER, "lib", BUILD);
    let edited = INTO_MEMBER.replace("pub fn f() {}", "pub fn g() {}");
    assert_eq!(key_of(&edited, "lib", BUILD), key, "premise: the member's file is not in the key");
    let (_, stored) = stored(INTO_MEMBER);
    assert_eq!(stored.written, Vec::new());
    assert!(
        matches!(&stored.refused[..], [(_, Refused::OutsideSourceRoot { what })]
            if what.contains("module `crate::m`") && what.contains("/ws/member/src/shared.rs")),
        "{:?}",
        stored.refused
    );
}

/// A crate that includes what its build script wrote, through `concat!` and `env!`.
const OUT_DIR_INCLUDE: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library env:OUT_DIR=/registry/lib/out
#[rustc_builtin_macro]
macro_rules! include { () => {} }
#[rustc_builtin_macro]
macro_rules! concat { () => {} }
#[rustc_builtin_macro]
macro_rules! env { () => {} }
include!(concat!(env!("OUT_DIR"), "/made.rs"));
//- /registry/lib/out/made.rs
pub fn made() {}
"#;

/// The calls in an `include!`'s argument (`concat!`, `env!`) are expanded as calls of the
/// argument's own: the walk climbs through it without taking it for an include whose file is
/// not known, so the crate is imaged.
#[test]
fn an_include_of_out_dir_through_concat_and_env_is_imaged() {
    let _turn = TURN.lock();
    let (_, stored) = stored(OUT_DIR_INCLUDE);
    assert_eq!(stored.refused, Vec::new());
    assert_eq!(stored.written.len(), 1);
}

/// A build script's file is in its package's source root, so the key covers it: a run that
/// changes only that file gives the crate another key.
#[test]
fn an_edit_of_an_out_dir_file_alone_gives_a_new_key() {
    let key = key_of(OUT_DIR_INCLUDE, "lib", BUILD);
    let edited = OUT_DIR_INCLUDE.replace("pub fn made() {}", "pub fn made() {}\npub fn more() {}");
    assert_ne!(key_of(&edited, "lib", BUILD), key);
}

/// An `include_bytes!` in a signature (an array's length) of a file of another package's
/// source root, which the crate's key does not cover: the crate is refused, the refusal naming
/// the macro, where it is and the file. Its call is one the signatures name, not a module's.
#[test]
fn an_include_bytes_in_a_signature_of_a_file_outside_the_root_is_refused() {
    let _turn = TURN.lock();
    const IN_SIGNATURE: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library
#[rustc_builtin_macro]
macro_rules! include_bytes { () => {} }
pub struct Blob(pub [u8; include_bytes!("../../other/blob.txt").len()]);
//- /registry/other/lib.rs crate:other new_source_root:library
//- /registry/other/blob.txt
blob
"#;
    let (_, stored) = stored(IN_SIGNATURE);
    assert_eq!(stored.written.len(), 1, "`other` alone is imaged");
    assert!(
        matches!(&stored.refused[..], [(_, Refused::OutsideSourceRoot { what })]
            if what.contains("`include_bytes!`")
                && what.contains("/registry/lib/src/lib.rs:3")
                && what.contains("/registry/other/blob.txt")),
        "{:?}",
        stored.refused
    );
}

/// A dependency whose signature holds what `include_bytes!` reads of a file of its own source
/// root that is not UTF-8 (`blob.bin`, set by [`binary_db`]).
const BINARY: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library
#[rustc_builtin_macro]
macro_rules! include_bytes { () => {} }
pub struct Blob(pub [u8; include_bytes!("blob.bin").len()]);
"#;

/// [`BINARY`] with `blob.bin`'s bytes `blob`, set as a checkout's loader sets them.
fn binary_db(blob: &[u8]) -> TestDB {
    assert!(std::str::from_utf8(blob).is_err(), "premise: the file has no text");
    let mut db = fixture::db(BINARY).into_db();
    fixture::set_bytes(&mut db, "/registry/lib/src/blob.bin", blob);
    db
}

/// An edit of a file that is not UTF-8 gives its crate another key, as an edit of a source
/// file does: the bytes `include_bytes!` reads are the ones the key hashes, those the loader
/// gave the file, never another read of the disk (the fixture's path is on no disk).
#[test]
fn an_edit_of_a_file_that_is_not_utf8_gives_a_new_key() {
    let _turn = TURN.lock();
    let key_in = |db: &TestDB| {
        let lib = crate_named(db, "lib");
        keys(db, &[lib], BUILD)[&lib]
    };
    let (before, after): (&[u8], &[u8]) = (&[0xff, 0xfe], &[0xff, 0xfe, 0xfd]);
    let mut db = binary_db(before);
    let lib = crate_named(&db, "lib");
    let key = key_in(&db);
    let signature = signatures::encode(&db, lib, &mut Vec::new()).unwrap();
    assert_eq!(key_in(&binary_db(before)), key, "the same bytes, the same key");

    fixture::set_bytes(&mut db, "/registry/lib/src/blob.bin", after);
    assert_ne!(key_in(&db), key, "an edit of the bytes moves the key");
    assert_ne!(
        signatures::encode(&db, lib, &mut Vec::new()).unwrap(),
        signature,
        "`include_bytes!` reads the new bytes"
    );
    assert_eq!(key_in(&db), key_in(&binary_db(after)));
    let memory = Memory::default();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let stored = store(&db, &served, &memory).unwrap();
    assert_eq!(stored.refused, Vec::new(), "a file of its own source root is no outside read");
    assert_eq!(stored.written, vec![lib]);
}

/// Making a crate's key reads its files and writes nothing (rule 25): what the caches that hash
/// the files read outside the VFS hold (`blaze-items`' crate facts and a kept proc-macro
/// expansion's tracked reads, both made from `Files::included_by` and `included_files`) is
/// the same whether the key was made before or after the crate's expansions, and the bytes
/// of a file of the VFS are no such file.
#[test]
fn a_key_made_before_or_after_the_expansions_leaves_the_same_reads() {
    let _turn = TURN.lock();
    let blob: &[u8] = &[0xff, 0xfe];
    let in_order = |key_first: bool| {
        let db = binary_db(blob);
        let lib = crate_named(&db, "lib");
        let key = || keys(&db, &[lib], BUILD)[&lib];
        let signature = || signatures::encode(&db, lib, &mut Vec::new()).unwrap();
        let (key, signature) = match key_first {
            true => (key(), signature()),
            false => {
                let signature = signature();
                (key(), signature)
            }
        };
        let files = db.files();
        (
            key,
            signature,
            files.included_by(&db, lib),
            files.included_files(),
            files.sources_included_by(),
        )
    };
    let key_first = in_order(true);
    assert_eq!(key_first, in_order(false));
    assert_eq!(
        (&key_first.2, &key_first.3),
        (&Vec::new(), &Vec::new()),
        "nothing was read from outside the VFS"
    );
}

/// A `mod` whose file is not there (rustc's E0583) is refused an image, the refusal naming the
/// module, where it is declared and the files looked for: the key covers the files that are,
/// so it would not move once that file appears.
#[test]
fn a_module_whose_file_is_not_there_is_refused_an_image() {
    let _turn = TURN.lock();
    const MISSING: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library
pub struct S;
pub mod inner { pub mod gone; }
"#;
    let (_, stored) = stored(MISSING);
    assert_eq!(stored.written, Vec::new());
    assert!(
        matches!(&stored.refused[..], [(_, Refused::ModuleFileMissing { what })]
            if what.contains("module `crate::inner::gone` of crate lib")
                && what.contains("/registry/lib/src/lib.rs:2")
                && what.contains("inner/gone.rs, inner/gone/mod.rs")),
        "{:?}",
        stored.refused
    );
}

const ALIASED: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
use lib::Shown;
fn main() {}

//- /lib.rs crate:lib deps:core new_source_root:library
pub struct Shown;

//- /core.rs crate:core new_source_root:library
#[lang = "sized"]
pub trait Sized {}
"#;

/// A database of [`ALIASED`] whose `lib` has as its root an alias of `lib.rs` with `glue`
/// appended as the module `__glue`, as `Lowered::append_roots` makes for a test harness or a
/// proc macro's wasm glue.
fn aliased(glue: &str) -> TestDB {
    let mut db = fixture::db(ALIASED).into_db();
    let lib = crate_named(&db, "lib");
    let mut data = lib.data(&db).clone();
    let alias = db.alias(data.root_file_id).unwrap();
    db.set_alias_appended(alias, Some(("__glue", glue))).unwrap();
    data.root_file_id = alias;
    lib.set_data(&mut db, data);
    db.note_crates_changed();
    db
}

/// The alias's path is its file's, so named by path it reads back as `lib.rs`, whose syntax
/// lacks the appended module (seedengine's "Can't find ast id" in a wasm proc-macro crate
/// after a re-serve): an image names it as the crate's root, and its key covers the appended
/// text, so a crate with other glue misses the image.
#[test]
fn an_image_of_a_crate_whose_root_has_a_module_appended_reads_back_that_root() {
    let _turn = TURN.lock();
    const GLUE: &str = "\npub mod __glue { pub struct Glue; }";
    let memory = Memory::default();
    let mut db = aliased(GLUE);
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &store(&db, &served, &memory).unwrap().written), ["core", "lib"]);

    let mut db = aliased(GLUE);
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    let lib = crate_named(&db, "lib");
    let root = lib.data(&db).root_file_id;
    let def_map = crate::nameres::crate_def_map(&db, lib);
    let files: Vec<FileId> = (def_map.modules(&db))
        .filter_map(|(_, module)| Some(module.definition_source_file_id().file_id()?.file_id(&db)))
        .collect();
    assert_eq!(files, [root, root], "the root module and `__glue` are of the alias");

    let mut other = aliased("\npub mod __glue { pub struct Other; }");
    let crates = all(&other);
    let served = serve(&mut other, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&other, &served.hits), ["core"], "other glue misses `lib`'s image");
}

/// A write that moves a file of a crate read from its image takes the image away in that
/// write, with the images of the served crates that depend on it, and leaves the others. No
/// reader sees the new file with the old image, which no longer reads. The crate then resolves
/// from its files as they are now, as a fresh database of them does.
#[test]
fn a_write_to_a_served_crates_file_withdraws_its_image_and_its_dependents_in_that_write() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    // `FIXTURE`'s files in order: main.rs, lib.rs, deep.rs, core.rs.
    let served_db = |at: usize| {
        let fixture = fixture::db(FIXTURE);
        let file = fixture.file(at);
        let mut db = fixture.into_db();
        let file = file.file_id(&db);
        let crates = all(&db);
        let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
        assert_eq!(names(&db, &served.hits), ["core", "lib"]);
        (db, file)
    };
    let resolves_fresh = |db: &TestDB, edited: &str, name: &str| {
        let fresh = fixture::db(edited).into_db();
        let (krate, live) = (crate_named(db, name), crate_named(&fresh, name));
        assert_eq!(
            frozen::encode(db, krate),
            frozen::encode(&fresh, live),
            "`{name}` resolves as a fresh one"
        );
    };

    // `deep.rs`, a module of `lib` that is not its root: `lib` loses its image, and `core`,
    // which depends on nothing that moved, keeps its own.
    let (mut db, deep) = served_db(2);
    let (core, lib) = (crate_named(&db, "core"), crate_named(&db, "lib"));
    assert_eq!((mode(&db, core), mode(&db, lib)), (Some(Mode::Serve), Some(Mode::Serve)));
    let text =
        db.file_text(deep).text(&db).replace("pub fn f() {}", "pub fn f() {} pub fn more() {}");
    db.set_file_text(deep, &text);
    assert_eq!(mode(&db, lib), None, "`lib`'s image is withdrawn by the write of its file");
    assert!(image(&db, lib).is_none());
    assert_eq!(mode(&db, core), Some(Mode::Serve), "`core` keeps its image");
    resolves_fresh(&db, &FIXTURE.replace("pub fn f() {}", "pub fn f() {} pub fn more() {}"), "lib");

    // `core.rs`: `core` loses its image, and so does `lib`, which depends on it.
    let (mut db, core_rs) = served_db(3);
    let (core, lib) = (crate_named(&db, "core"), crate_named(&db, "lib"));
    let more =
        |text: &str| text.replace("pub trait Sized {}", "pub trait Sized {}\npub struct More;");
    let text = more(&db.file_text(core_rs).text(&db));
    db.set_file_text(core_rs, &text);
    assert_eq!(
        (mode(&db, core), mode(&db, lib)),
        (None, None),
        "`core` and its dependent `lib` are withdrawn"
    );
    assert!(image(&db, core).is_none() && image(&db, lib).is_none());
    for name in ["core", "lib"] {
        resolves_fresh(&db, &more(FIXTURE), name);
    }
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

/// Frozen bytes read against a file they were not made from stop the query that asked with
/// [`frozen::Unreadable`], naming the crate, the file and both content IDs. Here no write
/// withdraws them: the bytes are set by hand after the edit. A driver fails that one job with
/// it ([`crate::nameres::stopped`]). It is never a panic with only text, and never a def map
/// resolved instead.
#[test]
fn a_frozen_def_map_read_against_a_moved_file_stops_the_query_with_an_error_naming_it() {
    let _turn = TURN.lock();
    let fixture = fixture::db(FIXTURE);
    let deep = fixture.file(2);
    let mut db = fixture.into_db();
    let deep = deep.file_id(&db);
    let lib = crate_named(&db, "lib");
    let bytes = frozen::encode(&db, lib).unwrap();
    let text =
        db.file_text(deep).text(&db).replace("pub fn f() {}", "pub fn f() {} pub fn more() {}");
    db.set_file_text(deep, &text);
    frozen::set(
        &mut db,
        [(lib, frozen::Frozen { bytes: bytes.into(), mode: Mode::Serve })].into_iter().collect(),
    );
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        _ = crate::nameres::crate_def_map(&db, lib);
    }));
    let Err(payload) = caught else {
        panic!("`lib`'s frozen def map was read against a file it was not made from")
    };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("the refusal unwound with another payload than `frozen::Unreadable`")
    };
    assert_eq!((stop.krate, stop.kind, stop.name.as_deref()), (lib, "def map", Some("lib")));
    assert!(
        stop.why.contains("deep.rs") && stop.why.contains("the image was made from"),
        "{}",
        stop.why
    );
    let text = crate::nameres::stopped(&*payload).expect("a typed stop of name resolution");
    assert!(
        text.contains("`lib`") && text.contains("deep.rs") && text.contains("refused"),
        "{text}"
    );
}
