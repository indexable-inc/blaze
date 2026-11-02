use std::{
    any::TypeId,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering::SeqCst},
    },
};

use base_db::{Crate, CrateOrigin, Db, Env};
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_expand::fixture;
use hir_expand::proc_macro::{
    ExpansionReads, ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError,
    ProcMacroKind, report_reads,
};
use parking_lot::Mutex;

use super::*;
use crate::{nameres::frozen, test_db::TestDB, tt::TopSubtree};

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
pub(super) static TURN: Mutex<()> = Mutex::new(());

const BUILD: ContentId = ContentId([7; 32]);

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
#[derive(Default)]
struct Memory {
    images: Mutex<HashMap<ContentId, Arc<Aligned>>>,
    gets: Mutex<usize>,
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
    let mut names: Vec<String> = crates.iter().map(|&it| serve::crate_name(db, it)).collect();
    names.sort();
    names
}

/// hir-ty's two `crate_name`s before they called this one (blaze-diagnostics' was this one's
/// copy). They named a crate with no display name by its ID, not its root file.
fn crate_name_reference(db: &Db, krate: Crate) -> String {
    match &krate.extra_data(db).display_name {
        Some(name) => name.to_string(),
        None => format!("{krate:?}"),
    }
}

#[test]
fn a_crate_is_named_in_an_error_as_every_old_copy_named_it() {
    let db = fixture::db(FIXTURE).into_db();
    for krate in all(&db) {
        assert_eq!(crate_name(&db, krate), crate_name_reference(&db, krate));
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
        let open = || if fixture == READING { reading_db() } else { fixture::db(fixture).into_db() };
        let mut db = open();
        let crates = all(&db);
        let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
        let stored = store(&db, &served, &memory).unwrap();
        assert_eq!(stored.filed.len(), stored.written.len());
        let filed: HashMap<String, ContentId> = stored
            .filed
            .iter()
            .map(|(&krate, &key)| (serve::crate_name(&db, krate), key))
            .collect();
        let mut db = open();
        let crates = all(&db);
        serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
        let raw: &Db = &db;
        for (krate, image) in DepImages::try_get(raw).unwrap().images(raw).iter() {
            assert_eq!(Some(&image.key()), filed.get(&serve::crate_name(raw, *krate)));
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
    fn expand(
        &self,
        _: &Db,
        call: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        report_reads(ExpansionReads::Traced { shape: SHAPE.into(), values: values_of(SHAPE) });
        Ok(call.subtree.copy())
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

/// A database of [`READING`], its library of macros a dependency as a registry's would be.
fn reading_db() -> TestDB {
    let source = "#[proc_macro_attribute]\n\
                  pub fn reading(_attr: TokenStream, item: TokenStream) -> TokenStream { item }\n";
    let reading = ProcMacro {
        name: intern::Symbol::intern("reading"),
        kind: ProcMacroKind::Attr,
        expander: Arc::new(Reading),
        disabled: false,
    };
    let mut db = fixture::db_with_proc_macros(READING, vec![(source.into(), reading)]).into_db();
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
        assert_eq!(
            def_map.diagnostics(&db).len(),
            fresh_map.diagnostics(&fresh).len(),
            "{mode:?}"
        );
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
    let unset = reads.values_then(&db);
    base_db::set_process_env(&mut db, Env::from_iter([("BLAZE_DEPS_TEST".into(), "1".into())]));
    let one = reads.values_then(&db);
    base_db::set_process_env(&mut db, Env::from_iter([("BLAZE_DEPS_TEST".into(), "2".into())]));
    let two = reads.values_then(&db);
    assert!(unset != one && one != two && unset != two);
    let record = reads.encode();
    let key = ContentId([4; 32]);
    assert_ne!(reads::image_key(key, &record, one), reads::image_key(key, &record, two));
    let lib = crate_named(&db, "lib");
    assert_eq!(reads.values_now(&db, lib, &Memory::default()).unwrap(), two);
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

/// The reads' own decoder before `blaze_format::cursor`: its names, or none where it refused.
fn old_reads_names(bytes: &[u8]) -> Option<(Vec<Box<str>>, Vec<Box<[u8]>>)> {
    fn take<'b>(rest: &mut &'b [u8], len: usize) -> Option<&'b [u8]> {
        if rest.len() < len {
            return None;
        }
        let (taken, after) = rest.split_at(len);
        *rest = after;
        Some(taken)
    }
    fn count(rest: &mut &[u8]) -> Option<usize> {
        Some(u32::from_le_bytes(take(rest, 4)?.try_into().expect("four bytes")) as usize)
    }
    let mut rest = bytes;
    if take(&mut rest, 1)? != [1] {
        return None;
    }
    let (mut env, mut shapes) = (Vec::new(), Vec::new());
    for _ in 0..count(&mut rest)? {
        let len = count(&mut rest)?;
        env.push(Box::<str>::from(std::str::from_utf8(take(&mut rest, len)?).ok()?));
    }
    for _ in 0..count(&mut rest)? {
        let len = count(&mut rest)?;
        shapes.push(Box::<[u8]>::from(take(&mut rest, len)?));
    }
    let sorted = env.windows(2).all(|pair| pair[0] < pair[1])
        && shapes.windows(2).all(|pair| pair[0] < pair[1]);
    (rest.is_empty() && sorted).then_some((env, shapes))
}

#[test]
fn reads_decode_as_the_old_decoder_did() {
    let samples = [
        Reads::default(),
        Reads::for_tests(&["A"], &[]),
        Reads::for_tests(&[], &[(b"", ContentId([1; 32]))]),
        Reads::for_tests(
            &["", "PATH", "\u{e9}t\u{e9}"],
            &[(b"one", ContentId([1; 32])), (b"two", ContentId([2; 32]))],
        ),
    ];
    let mut rng = blaze_ids::rng::SplitMix64::new(0x5eed_0f_4ead5);
    for reads in &samples {
        let bytes = reads.encode();
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
            let new = Reads::decode(&input).ok().map(|it| {
                let (env, shapes) = it.names();
                (env.to_vec(), shapes.map(Box::<[u8]>::from).collect::<Vec<_>>())
            });
            assert_eq!(new, old_reads_names(&input), "{input:?}");
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
