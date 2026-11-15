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
use blaze_memo::Id as _;

use crate::{
    nameres::frozen,
    test_db::{TestDB, TestDBExt},
};

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

/// Each run of a crate's image key ([`key::crate_image_key`]), by the database it ran in:
/// what a test counts to say which keys a walk made, a memo's verification counting for none.
/// Each crate by its key, which outlives the commits its id does not.
pub(super) static KEYED: Lock<Vec<(base_db::Nonce, base_db::CrateKey)>> =
    Lock::new("test.def.deps.keyed", Vec::new());

/// The crates whose key was made in `db` since the last take, by name, sorted.
fn take_keyed(db: &TestDB) -> Vec<String> {
    let nonce = db.nonce();
    let mut own = Vec::new();
    KEYED.lock().retain(|&(of, krate)| {
        if of == nonce {
            own.push(db.krate(krate));
        }
        of != nonce
    });
    names(db, &own)
}

/// A workspace member over `libraries` library crates in a chain (`lib0` on `lib1` on ...),
/// each a file of its own; the member's file is `/main.rs`, library `n`'s `/lib{n}.rs`.
fn chain(libraries: usize) -> String {
    let mut text =
        String::from("//- /main.rs crate:main deps:lib0 new_source_root:local\nfn main() {}\n");
    for at in 0..libraries {
        let deps = if at + 1 < libraries { format!(" deps:lib{}", at + 1) } else { String::new() };
        text.push_str(&format!(
            "//- /lib{at}.rs crate:lib{at}{deps} new_source_root:library\npub fn f{at}() {{}}\n"
        ));
    }
    text
}

/// The keys made by a walk of every crate of [`chain`]`(libraries)`: on a first walk, after
/// an edit of the member's file, and after an edit of the innermost library's. Each walk is
/// held to a walk from scratch of a fresh database of the same text.
fn keyed_across_edits(libraries: usize) -> [Vec<String>; 3] {
    let text = chain(libraries);
    let fixture = fixture::db(&text);
    let main = fixture.file_at("/main.rs").file_id(fixture.db());
    let inner = fixture.file_at(&format!("/lib{}.rs", libraries - 1)).file_id(fixture.db());
    let mut db = fixture.into_db();
    // Each walk is of the crates of its own snapshot, its keys by crate key, sorted: the
    // crates' ids do not outlive the commit that drops their arenas, their keys do.
    let walk = |db: &TestDB| -> Vec<(base_db::CrateKey, ContentId)> {
        let mut keyed: Vec<_> =
            keys(db, &all(db), BUILD).iter().map(|(&krate, &key)| (krate.key(db), key)).collect();
        keyed.sort_unstable();
        keyed
    };
    let first = walk(&db);
    assert_eq!(first.len(), libraries + 1, "every crate is keyed, the member too");
    let cold = take_keyed(&db);

    let member = crate_named(&db, "main").key(&db);
    let libraries_of =
        |keys: &[(base_db::CrateKey, ContentId)]| -> Vec<(base_db::CrateKey, ContentId)> {
            keys.iter().copied().filter(|it| it.0 != member).collect()
        };
    _ = db.set_file_text(main, "fn main() { let _ = 1; }\n");
    let after = walk(&db);
    assert_eq!(libraries_of(&after), libraries_of(&first), "an edit of a member moves no library's key");
    assert_ne!(after, first, "the member's own key covers its file");
    let after_member = take_keyed(&db);

    let edited = format!("pub fn f{}() {{}}\npub struct More;\n", libraries - 1);
    _ = db.set_file_text(inner, &edited);
    let moved = walk(&db);
    let after_library = take_keyed(&db);
    // The refinement: the memo's keys are those of a walk from scratch over the same text,
    // the member's edit too (a member is keyed since key domain 9, `super::key::KEY_DOMAIN`).
    let whole = text
        .replace(&format!("pub fn f{}() {{}}\n", libraries - 1), &edited)
        .replace("fn main() {}\n", "fn main() { let _ = 1; }\n");
    let fresh = fixture::db(&whole).into_db();
    let from_scratch: Vec<ContentId> =
        sorted(&keys(&fresh, &all(&fresh), BUILD)).into_iter().map(|it| it.1).collect();
    let mut kept: Vec<ContentId> = moved.iter().map(|it| it.1).collect();
    let mut scratch = from_scratch;
    kept.sort_unstable();
    scratch.sort_unstable();
    assert_eq!(kept, scratch, "the keys after an edit are a fresh database's");
    assert!(moved.iter().zip(&first).all(|(now, was)| now.1 != was.1), "every key covers the innermost library");
    [cold, after_member, after_library]
}

/// The invariant: a walk of the image keys makes a key only for a crate something its key
/// covers changed for. A first walk makes every key once; after an edit of a workspace
/// member's file only the member's own is made (R3: a member has a key, which covers its
/// files), among 4 libraries and among 40 (the count is 1 at both sizes); after an edit of a
/// library every key that covers it is made again, and they equal a fresh database's.
#[test]
fn an_edit_of_a_member_makes_no_dependency_image_key_again() {
    for libraries in [4, 40] {
        let [cold, after_member, after_library] = keyed_across_edits(libraries);
        assert_eq!(cold.len(), libraries + 1, "each crate keyed once: {cold:?}");
        assert_eq!(
            after_member,
            ["main"],
            "no library's key reads a member's file, so only the member's is made again, among {libraries} libraries"
        );
        // Every key covers the innermost library, the member's too.
        assert_eq!(after_library.len(), libraries + 1, "{after_library:?}");
    }
}

/// The control of [`an_edit_of_a_member_makes_no_dependency_image_key_again`] (rule
/// test-the-cause): the memo is what keeps the keys. A database that is told nothing of the
/// first walk (a fresh one of the edited text) makes every key, so the count tells a walk
/// that hashes every dependency again from one that asks the memo.
#[test]
fn the_oracle_catches_a_walk_that_makes_every_key_again() {
    let text = chain(40);
    let db = fixture::db(&text).into_db();
    _ = keys(&db, &all(&db), BUILD);
    assert_eq!(take_keyed(&db).len(), 41);
    let edited = fixture::db(&text.replace("fn main() {}", "fn main() { let _ = 1; }")).into_db();
    _ = keys(&edited, &all(&edited), BUILD);
    assert_eq!(take_keyed(&edited).len(), 41, "with no memo every key is made again");
}

pub(super) const BUILD: ContentId = ContentId([7; 32]);

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// A store in memory, which refuses a second put of a key with other bytes as the store does
/// (also `facts::tests`' image store).
pub(super) struct Memory {
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

    // dyn: `ImageStore::pages`'s signature, once per crate read from pages
    fn pages(&self, key: &ContentId) -> Result<Option<Arc<dyn crate::deps::PageSource>>, String> {
        // A test's store holds each value whole: no part of one is read alone.
        Ok(crate::deps::whole_pages(self.get(key)?, std::alloc::Global))
    }

    // dyn: `ImageStore::for_each`'s signature, one call per batch of crates
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        // A test's store runs a batch in order on its own thread: the same result.
        (0..count).for_each(task);
    }
}

pub(super) fn all(db: &TestDB) -> Vec<Crate> {
    base_db::all_crates(db).iter().copied().collect()
}

/// The stable id a test's driver gives `krate`'s slots under: of its name, which a fixture
/// gives each crate once.
pub(super) fn stable_id(db: &TestDB, krate: Crate) -> ContentId {
    ContentId::of("hir-def test crate", krate.named(db).to_string().as_bytes())
}

/// The driver's serve, as a test makes it: [`super::serve`] over this snapshot, then the next
/// snapshot built with what the driver gives ([`give_next`]).
pub(crate) fn serve(
    db: &mut TestDB,
    crates: &[Crate],
    build: ContentId,
    // dyn: the test's store
    images: &dyn ImageStore,
    mode: Mode,
) -> Result<Served, String> {
    let (served, found) = super::serve(db, crates, build, images, mode)?;
    // The serve names its crates by key ([`Served`]), so it is kept across the give's commit
    // as it is; the withdrawn are taken in this snapshot, which the give reads.
    let withdrawn: Vec<Crate> = served.withdrawn.iter().map(|&key| db.krate(key)).collect();
    give_next(db, found, &withdrawn);
    Ok(served)
}

/// Crates by their key or their id, as a test names them ([`names`]).
pub(super) trait Named: Copy {
    fn in_db(self, db: &TestDB) -> Crate;
}

impl Named for Crate {
    fn in_db(self, _: &TestDB) -> Crate {
        self
    }
}

impl Named for base_db::CrateKey {
    fn in_db(self, db: &TestDB) -> Crate {
        db.krate(self)
    }
}

/// Commits the next snapshot of `db` with each crate's image as a driver gives it: those of
/// `found`, and every image given now that `found` does not replace and that is not
/// `withdrawn` ([`give_image`], each to its crate). `found` and `withdrawn` are of the
/// snapshot `db` holds now, which every read here is of: the commit drops their arenas.
pub(crate) fn give_next(db: &mut TestDB, found: Vec<(Crate, Image), Global>, withdrawn: &[Crate]) {
    let mut edit = db.edit();
    let given = {
        let db = edit.db();
        // alloc: a test's images, once a snapshot
        let mut given: HashMap<Crate, Image, Global> = HashMap::new_in(Global);
        for krate in all(db) {
            if let Some(image) = image_of(db, krate)
                && !withdrawn.contains(&krate)
            {
                given.insert(krate, image.clone()); // clone: an image's handle, once a crate
            }
        }
        given.extend(found);
        // alloc: as above, each with its stable id
        let mut each: Vec<(Crate, ContentId, Image), Global> = Vec::new_in(Global);
        each.extend(given.into_iter().map(|(krate, image)| (krate, stable_id(db, krate), image)));
        // By key: the order the world is given them in, the same in every snapshot.
        each.sort_unstable_by_key(|it| it.0.key(db));
        each
    };
    for (krate, krate_id, image) in given {
        give_image(&mut edit, krate, krate_id, image);
    }
    // A gift lasts until it is taken: a withdrawn crate's image is taken, as a driver does.
    for &krate in withdrawn {
        let key = krate.key(edit.db());
        super::withdraw_image(&mut edit, key);
    }
    _ = edit.commit();
}

/// What a driver does at a write that moves `files` (set or added): it withdraws the images
/// [`withdrawn`] names, asked of the snapshot before the write (`withdrawn` reads the snapshot
/// the driver makes the write from), in the snapshot the write makes.
pub(super) fn write_files(db: &mut TestDB, files: &[FileId], write: impl FnOnce(&mut TestDB)) {
    let served: Vec<Crate> = all(db).into_iter().filter(|&krate| is_served(db, krate)).collect();
    let gone: Vec<base_db::CrateKey> =
        withdrawn(db, &served, files, &[]).iter().map(|krate| krate.key(db)).collect();
    write(db);
    let gone: Vec<Crate> = gone.into_iter().map(|key| db.krate(key)).collect();
    give_next(db, Vec::new_in(Global), &gone);
}

/// A store with the images of a fresh database of `fixture`.
fn stored(fixture: &str) -> (Memory, Stored) {
    let mut db = fixture::db(fixture).into_db();
    let memory = Memory::default();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new());
    let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
    (memory, stored)
}

fn names(db: &TestDB, crates: &[impl Named]) -> Vec<String> {
    let mut names: Vec<String> = crates.iter().map(|&it| it.in_db(db).named(db).to_string()).collect();
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

/// A workspace member is eligible (R3: its pages are kept, under a key that covers its files)
/// and is not sealed; the crates outside the workspace are both.
#[test]
fn only_crates_outside_the_workspace_are_sealed() {
    let db = fixture::db(FIXTURE).into_db();
    for name in ["lib", "core", "main"] {
        assert!(eligible(&db, crate_named(&db, name)), "{name}");
        // The reason is the rule `eligible` refused by, for every crate: none exactly when
        // eligible.
        assert_eq!(super::why_not_eligible(&db, crate_named(&db, name)), None, "{name}");
    }
    assert!(sealed(&db, crate_named(&db, "lib")));
    assert!(sealed(&db, crate_named(&db, "core")));
    assert!(!sealed(&db, crate_named(&db, "main")));
    let keys = keys(&db, &all(&db), BUILD);
    assert_eq!(names(&db, &keys.keys().copied().collect::<Vec<_>>()), ["core", "lib", "main"]);
}

/// A map of keys as a list in crate order, to compare and to show.
fn sorted(keys: &HashMap<Crate, ContentId, Global>) -> Vec<(Crate, ContentId)> {
    let mut list: Vec<(Crate, ContentId)> =
        keys.iter().map(|(&krate, &key)| (krate, key)).collect();
    list.sort_unstable_by_key(|it| it.0);
    list
}

/// The oracle of `a_kept_walk_keys_each_crate_once_and_as_one_walk_does`: `asks` made of
/// `walk` one after another at `db`, what they keyed added to `union`; why the union is not
/// what one walk of every crate of `db` gives, or `None`.
fn kept_walk_differs(
    db: &TestDB,
    asks: &[Crate],
    walk: &mut KeyWalk,
    union: &mut HashMap<Crate, ContentId, Global>,
) -> Option<String> {
    for &ask in asks {
        let more = walk.more(db, &[ask], BUILD);
        if let Some(krate) = more.keys().find(|it| union.contains_key(*it)) {
            return Some(format!("{} was keyed twice", krate.named(db)));
        }
        union.extend(more);
    }
    let (kept, whole) = (sorted(union), sorted(&keys(db, &all(db), BUILD)));
    (kept != whole).then(|| format!("the kept walk keyed {kept:?}, one walk keys {whole:?}"))
}

/// The invariant: at one view of one database by one `build`, the keys a [`KeyWalk`] gives
/// over any sequence of asks are those one walk of all the crates gives, each crate keyed
/// once: so keeping the walk between a build's serves changes no key (rule 21), and a serve
/// costs the crates it is the first to ask for. Every order of asking the fixture's crates
/// one at a time, each asked again after. Its control is
/// `the_oracle_catches_a_walk_kept_across_a_write`.
#[test]
fn a_kept_walk_keys_each_crate_once_and_as_one_walk_does() {
    let db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    assert_eq!(crates.len(), 3);
    for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        let asks: Vec<Crate> = order.iter().map(|&at| crates[at]).collect();
        let (mut walk, mut union) = (KeyWalk::new(), HashMap::new_in(Global));
        assert_eq!(kept_walk_differs(&db, &asks, &mut walk, &mut union), None, "{order:?}");
        assert_eq!(walk.len(), 3, "every crate walked, the member too");
        // Asked again: nothing is keyed, and nothing changes.
        assert_eq!(kept_walk_differs(&db, &asks, &mut walk, &mut union), None, "{order:?}");
    }
}

/// The invariant: a [`KeyWalk`] kept across a write of an input (a dependency's text) keys
/// the written database as one walk of it does: at the new view it forgets what it walked
/// and walks again, with no call of [`KeyWalk::clear`]. The planted bug: the view check off
/// (`plant_keep_across_views`), a walk kept across the write goes on giving nothing for the
/// crates it keyed before, whose keys moved, and the oracle says so.
#[test]
fn the_oracle_catches_a_walk_kept_across_a_write() {
    let edited = FIXTURE.replace("pub trait Sized {}", "pub trait Sized {}\npub struct More;");
    assert_ne!(edited, FIXTURE, "premise: the write changes a dependency's text");
    for plant in [false, true] {
        let db = fixture::db(FIXTURE).into_db();
        let (mut walk, mut union) = (KeyWalk::new(), HashMap::new_in(Global));
        walk.plant_keep_across_views = plant;
        assert_eq!(kept_walk_differs(&db, &all(&db), &mut walk, &mut union), None);
        let written = fixture::db(&edited).into_db();
        union.clear();
        let why = kept_walk_differs(&written, &all(&written), &mut walk, &mut union);
        if plant {
            let why = why.expect("the oracle missed a walk kept across a write");
            assert!(why.contains("one walk keys"), "{why}");
        } else {
            assert_eq!(why, None, "a walk at a new view walks again");
        }
    }
}

/// Whether a crate is sealed by the old rule: an origin outside the workspace and a root in a
/// library root. What [`sealed`] was until 2026-11-12, kept as the control.
fn sealed_by_origin(db: &TestDB, krate: Crate) -> bool {
    let data = krate.data(db);
    let root = db.file_source_root(data.root_file_id).source_root_id(db);
    !data.origin.is_local() && db.source_root(root).source_root_ref(db).is_library
}

/// The invariant: a crate is sealed exactly when it and every crate below it carry a pin
/// (`base_db::CrateData::sealed`, which the planner writes from the driver's one rule); origin
/// and root kind decide nothing. A path dependency with a library origin in a library root has
/// no pin and is not sealed; nor is a doctest crate of a library (a library origin, its root in
/// a local root); a pinned library on it is not either. The control: the origin rule seals the
/// path dependency, which is what this rule refuses.
#[test]
fn a_crate_is_sealed_by_its_pin_and_nothing_else() {
    let mut db = fixture::db(
        "//- /lib.rs crate:lib new_source_root:library\n\
         pub struct L;\n\
         //- /path.rs crate:path new_source_root:library\n\
         pub struct P;\n\
         //- /above.rs crate:above deps:path new_source_root:library\n\
         pub use path::P;\n\
         //- /doc.rs crate:doc library deps:lib new_source_root:local\n\
         pub use lib::L;\n",
    )
    .into_db();
    // The fixture pins every library crate in a library root, as a registry package's planner
    // does; `path` stands for a path dependency, which no planner pins.
    let path = crate_named(&db, "path");
    let mut data = path.data(&db).clone(); // clone: a test's crate data, once
    data.sealed = None;
    _ = db.set_crate_data(path, &data);
    // Each crate's id in the snapshot the write made: the ids before are of dropped arenas.
    let (lib, path, above, doc) = (
        crate_named(&db, "lib"),
        crate_named(&db, "path"),
        crate_named(&db, "above"),
        crate_named(&db, "doc"),
    );
    assert!(lib.data(&db).sealed.is_some() && sealed(&db, lib), "a pinned crate is sealed");
    assert!(!sealed(&db, path), "a path dependency is not, whatever its origin and root");
    assert!(!sealed(&db, above), "nor is a pinned crate above it");
    assert!(doc.data(&db).sealed.is_none() && !sealed(&db, doc), "a doctest crate of a library is not");
    // The control: the rule by origin would seal the path dependency and the doctest's origin
    // is a library's.
    assert!(sealed_by_origin(&db, path), "the origin rule seals the path dependency");
    assert!(!doc.data(&db).origin.is_local());
}

#[test]
fn a_library_on_a_member_is_not_sealed() {
    let db = fixture::db(
        "//- /lib.rs crate:lib deps:member new_source_root:library\n\
         pub use member::X;\n\
         //- /member.rs crate:member new_source_root:local\n\
         pub struct X;\n",
    )
    .into_db();
    let (lib, member) = (crate_named(&db, "lib"), crate_named(&db, "member"));
    assert!(!sealed(&db, lib), "a crate above a member is resolved again with it");
    assert!(!sealed(&db, member));
    // Both have pages, under keys that cover the member's files.
    assert!(eligible(&db, lib) && eligible(&db, member));
    assert_eq!((super::why_not_eligible(&db, lib), super::why_not_eligible(&db, member)), (None, None));
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
    let owned = hand_parts();
    let parts: [&[u8]; Part::ALL.len()] = owned.each_ref().map(Vec::as_slice);
    let bytes = Aligned::copy_from(&image_with(key, parts));
    let image = ImageRef::open(&bytes, key).unwrap();
    for (part, bytes) in Part::ALL.into_iter().zip(parts) {
        assert_eq!(image.part(part), bytes, "{part:?}");
        assert!(bytes.is_empty() || (image.part(part).as_ptr() as usize).is_multiple_of(16));
    }
    assert_eq!(image_with(key, parts), image_with(key, parts));
}

/// [`write_image`] of parts a test made by hand, taken as they are.
fn image_with(key: ContentId, parts: [&[u8]; Part::ALL.len()]) -> Vec<u8> {
    let parts = parts.map(|it| Portable::written(it.to_vec_in(Global)));
    write_image(key, parts.each_ref())
}

/// One part a test makes by hand for each of [`Part::ALL`], however many there are: the
/// first empty, part `n` after it `9 + n` repeated `33 + n` times, so no two are alike and
/// none is a multiple of 16 long.
fn hand_parts() -> [Vec<u8>; Part::ALL.len()] {
    std::array::from_fn(|at| match at {
        0 => Vec::new(),
        at => vec![9 + at as u8; 33 + at],
    })
}

#[test]
fn an_image_under_another_key_does_not_read() {
    let owned = hand_parts();
    let parts = owned.each_ref().map(Vec::as_slice);
    let bytes = Aligned::copy_from(&image_with(ContentId([1; 32]), parts));
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
    let owned = hand_parts();
    let written = image_with(key, owned.each_ref().map(Vec::as_slice));
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
    let again = store(&db, &served, &memory, Which::Sealed).unwrap();
    assert_eq!((again.written.len(), again.refused.len()), (0, 0));
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

/// Every key `store` wrote into `memory` for the crates of `served` and `stored` that is
/// neither one of a written crate's front keys ([`front_keys`] of the key its image is filed
/// under) nor one of a keyed crate's filing keys ([`filing_keys`] of its own key).
fn keys_outside_the_lists(memory: &Memory, served: &Served, stored: &Stored) -> Vec<ContentId> {
    let mut listed: std::collections::BTreeSet<ContentId> = std::collections::BTreeSet::new();
    for filed in stored.filed.values() {
        listed.extend(front_keys(*filed));
    }
    for key in served.keys.values() {
        listed.extend(filing_keys(*key));
    }
    let mut outside: Vec<ContentId> =
        memory.images.lock().keys().copied().filter(|key| !listed.contains(key)).collect();
    outside.sort_unstable();
    outside
}

/// Whether `memory` holds, for every crate `stored` filed, each record kind of
/// [`front_keys`] the front end writes: its image and its def map pages. The
/// kept roots ([`kept_key`]) are codegen's record, written by its own store beside these.
fn holds_every_front_record(memory: &Memory, stored: &Stored) -> bool {
    let images = memory.images.lock();
    stored.filed.values().all(|filed| {
        let mut written = front_keys(*filed).filter(|key| *key != kept_key(*filed));
        written.all(|key| images.contains_key(&key))
    })
}

/// The invariant: every record `store` writes for a crate is under a key the lists give
/// ([`front_keys`], [`filing_keys`]), with reads and without: what pins a crate's front end
/// and what a pack of it carries are taken from those lists, so a record kind written and
/// left out of them would be collected under a pinned crate and missing from its pack. Every
/// kind of [`front_keys`] the front end writes is written (checked: each filed crate has all
/// of them), so no key of the list goes unwritten by the test.
#[test]
fn every_key_store_writes_is_a_front_key_or_a_filing_key() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    let dbs: [fn() -> TestDB; 2] = [|| fixture::db(FIXTURE).into_db(), reading_db];
    for db in dbs {
        let memory = Memory::default();
        let mut plain = db();
        let crates = all(&plain);
        let served = serve(&mut plain, &crates, BUILD, &memory, Mode::Serve).unwrap();
        let stored = store(&plain, &served, &memory, Which::Sealed).unwrap();
        assert_eq!(stored.refused, Vec::new(), "a crate whose def map does not write as pages");
        assert!(!stored.written.is_empty(), "the fixture stores no crate: the test tests nothing");
        assert!(holds_every_front_record(&memory, &stored), "a kind of `front_keys` unwritten");
        assert_eq!(keys_outside_the_lists(&memory, &served, &stored), Vec::new());
    }
}

/// The control: a record kind `store` writes that the list leaves out is found. Here the
/// list is the real one less the def map pages' key, as a lane that added pages and forgot
/// the list would have left it.
#[test]
fn the_oracle_catches_a_key_store_writes_that_the_list_leaves_out() {
    let _turn = TURN.lock();
    let mut db = fixture::db(FIXTURE).into_db();
    let memory = Memory::default();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
    assert!(holds_every_front_record(&memory, &stored), "premise: the pages were written");
    let mut listed: std::collections::BTreeSet<ContentId> = std::collections::BTreeSet::new();
    for filed in stored.filed.values() {
        listed.extend(front_keys(*filed).filter(|key| *key != pages_key(*filed)));
    }
    for key in served.keys.values() {
        listed.extend(filing_keys(*key));
    }
    let outside = memory.images.lock().keys().filter(|key| !listed.contains(*key)).count();
    assert!(outside >= 1, "no key of def map pages was found outside the shortened list");
    assert!(outside <= stored.written.len(), "{outside} keys outside it: more than a crate's pages each");
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
        let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
        assert_eq!(stored.filed.len(), stored.written.len());
        let filed: HashMap<String, ContentId, Global> =
            stored.filed.iter().map(|(&krate, &key)| (db.krate(krate).named(&db).to_string(), key)).collect();
        let mut db = open();
        let crates = all(&db);
        serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
        // The give committed: the crates again, as this snapshot has them.
        let crates = all(&db);
        let raw: &Db = &db;
        for image in crates.iter().filter_map(|&krate| Some((krate, image_of(raw, krate)?))) {
            let (krate, image) = image;
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
            assert_eq!(
                frozen::encode_diagnostic_items(&db, krate),
                frozen::encode_diagnostic_items(&fresh, live),
                "{name} {mode:?}"
            );
            frozen::check_diagnostic_items(&db, krate);
        }
        assert_eq!(frozen::differences(), Vec::<String>::new(), "{mode:?}");
    }
}

/// The invariant: a crate has the same module ids whether it is resolved or read from its
/// image. One database resolves `lib` and `core` and is then served their images, stored by
/// another (what a daemon's first build does to the crates its lowering resolved; R3: a
/// database that asked for a crate before and holds its state is not served it again): each
/// crate's root and every module of it keep their ids, so each item's location (interned by
/// its module) keeps its id too, and a row the database made before the serve is still keyed
/// by something its crate's `DefMap` holds.
///
/// Its control is the tree before `DefMap::empty` took `crate_root`: the decoded map's root
/// was a tracked struct of the decoding query, and this failed on the root's id.
///
/// PORT (round 20): false by design on blaze-memo, left failing. It compares ids taken before
/// the serve's commit with ids taken after it; an id is of its snapshot (port decision 3.1:
/// nothing may rely on an arena surviving a change), and the give's commit makes the crates'
/// arenas again (round 20: before `1.0:15, 1.0:20, ..`, after `1.0:11, 1.0:12, ..`), so no row
/// made before the serve is read after it, and the ids it would key by are not comparable.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn a_crate_served_after_it_was_resolved_keeps_its_module_ids() {
    let _turn = TURN.lock();
    let (memory, _) = stored(FIXTURE);
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    let modules = |db: &TestDB, name: &str| -> Vec<crate::ModuleId> {
        let def_map = crate::nameres::crate_def_map(db, crate_named(db, name));
        let mut ids = vec![def_map.crate_root(db)];
        ids.extend(def_map.modules(db).map(|(module, _)| module));
        ids
    };
    let resolved = ["core", "lib"].map(|name| modules(&db, name));
    assert!(resolved[1].len() >= 5, "lib has its root, deep, deeper and made: {:?}", resolved[1]);
    for name in ["core", "lib"] {
        assert_eq!(mode(&db, crate_named(&db, name)), None, "{name} is resolved first");
    }
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    for name in ["core", "lib"] {
        assert_eq!(mode(&db, crate_named(&db, name)), Some(Mode::Serve), "{name} is served now");
    }
    assert_eq!(["core", "lib"].map(|name| modules(&db, name)), resolved);
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

/// A database that resolved `core` and `lib` (their def maps asked) and was then served with
/// their images in the store, through `lowered` as a driver keeps it (noted at a first serve
/// that found the store empty): whether `core` and `lib` stayed lowered in the arenas they
/// had, or the names of those switched to their image or made again.
fn served_after_lowering(note: bool) -> Result<(), String> {
    let (memory, _) = stored(FIXTURE);
    let mut db = fixture::db(FIXTURE).into_db();
    let mut lowered = Lowered::default();
    let empty = Memory::default();
    let crates = all(&db);
    // The first serve, of a store with no image: every crate lowered.
    let (served, found) = super::serve_keeping(&db, &crates, BUILD, &empty, Mode::Serve, &lowered)?;
    assert!(found.is_empty() && served.hits.is_empty(), "an empty store serves nothing");
    let mut keys: Vec<base_db::CrateKey> = all(&db).iter().map(|krate| krate.key(&db)).collect();
    keys.sort_unstable();
    if note {
        lowered.note(&db, keys.iter().copied());
    }
    for name in ["core", "lib"] {
        _ = crate::nameres::crate_def_map(&db, crate_named(&db, name));
    }
    let before = ["core", "lib"].map(|name| crate_named(&db, name));
    // The store has their images now (the cold build's end), and the driver serves again.
    let crates = all(&db);
    let (_, found) = super::serve_keeping(&db, &crates, BUILD, &memory, Mode::Serve, &lowered)?;
    give_next(&mut db, found, &[]);
    let mut moved = Vec::new();
    for (name, was) in ["core", "lib"].into_iter().zip(before) {
        let now = crate_named(&db, name);
        if now != was || is_served(&db, now) {
            moved.push(name);
        }
    }
    match moved.is_empty() {
        true => Ok(()),
        false => Err(format!("{moved:?} left their lowered arenas for their images")),
    }
}

/// The invariant: a crate lowered in its arena stays lowered while that arena lives, though
/// the store now has its image: a later serve neither gives it the image nor drops its arena
/// (R3; the first edit after a cold build dropped 24 arenas giving std its images). The
/// control (the cause removed): with nothing noted lowered, as before, `core` and `lib` are
/// switched to their images and their arenas made again.
#[test]
fn a_crate_lowered_in_its_arena_stays_lowered_when_its_image_is_stored() {
    let _turn = TURN.lock();
    served_after_lowering(true).unwrap_or_else(|why| panic!("{why}"));
    let planted = served_after_lowering(false).expect_err("a lowered crate switched to its image was not caught");
    assert!(planted.contains("left their lowered arenas"), "{planted}");
}

/// A restart as the driver makes one (blaze-cargo's `unit.rs`, fresh path): the cold build
/// stored `core` and `lib` and their found records (the crate's key its image was stored
/// under, `found::Found::base`); the restart's database is given those images before any
/// query runs (`at_start`, the makers' commit: [`serve_found`]) or, the plant, not (the order
/// before 2026-11-14: the first serve gives them, after queries ran). Then a query of each,
/// and the first serve, as the driver runs them. Each crate's arena is noted after the start
/// and after every step; a crate made again past the start is said by name.
fn a_start_given_found_images(at_start: bool) -> Result<(), String> {
    let which = ["core", "lib"];
    // The cold build: what it stored, and the key each crate's image was stored under.
    let memory = Memory::default();
    let bases = {
        let mut db = fixture::db(FIXTURE).into_db();
        let crates = all(&db);
        let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve)?;
        let stored = store(&db, &served, &memory, Which::Sealed).map_err(|why| format!("{why:?}"))?;
        which.map(|name| {
            let key = crate_named(&db, name).key(&db);
            let base = served.keys[&key];
            // `serve_found`'s case: a crate that read nothing is filed under its key alone.
            assert_eq!(stored.filed.get(&key), Some(&base), "{name} is filed under its key alone");
            base
        })
    };
    let mut db = fixture::db(FIXTURE).into_db();
    if at_start {
        // alloc: a test's two crates
        let found: Vec<(Crate, ContentId)> =
            which.iter().zip(bases).map(|(name, base)| (crate_named(&db, name), base)).collect();
        let given = super::serve_found(&db, &found, &memory, Mode::Serve)?;
        assert_eq!(given.len(), which.len(), "each found record's image is found");
        give_next(&mut db, given, &[]);
    }
    // Each crate's arenas, from the one after the start: a second is the arena made again.
    let arena = |db: &TestDB, name: &str| crate_named(db, name).gid().arena();
    // alloc: a test's few arenas
    let mut made: Vec<(&str, Vec<blaze_memo::ArenaId>)> =
        which.iter().map(|&name| (name, vec![arena(&db, name)])).collect();
    let note = |db: &TestDB, made: &mut Vec<(&str, Vec<blaze_memo::ArenaId>)>| {
        for (name, arenas) in made.iter_mut() {
            let now = arena(db, name);
            if arenas.last() != Some(&now) {
                arenas.push(now);
            }
        }
    };
    // A query of each before the serve, as a start's surfaces and identities are asked.
    for name in which {
        _ = crate::nameres::crate_def_map(&db, crate_named(&db, name));
    }
    note(&db, &mut made);
    let crates = all(&db);
    let (served, found) =
        super::serve_keeping(&db, &crates, BUILD, &memory, Mode::Serve, &Lowered::default())?;
    // What the serve gave of the two (`main`, a member, has no image and is asked as before).
    let gave = names(&db, &found.iter().map(|it| it.0).filter(|krate| which.iter().any(|name| crate_named(&db, name) == *krate)).collect::<Vec<_>>());
    if !found.is_empty() {
        give_next(&mut db, found, &[]);
    }
    note(&db, &mut made);
    for name in which {
        _ = crate::nameres::crate_def_map(&db, crate_named(&db, name));
        assert!(is_served(&db, crate_named(&db, name)), "{name} is read from its image");
    }
    note(&db, &mut made);
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    // alloc: a test's names
    let again: Vec<&str> = made.iter().filter(|(_, arenas)| arenas.len() > 1).map(|(name, _)| *name).collect();
    match again.is_empty() && gave.is_empty() {
        true => Ok(()),
        false => Err(format!(
            "{again:?} made their arenas again past the start (the first serve gave {gave:?} their images)"
        )),
    }
}

/// The invariant: a start that will serve a crate gives its image before any query of it
/// runs, so the crate's arena is made once: the first serve, whose walk keys the crate under
/// the found record's base, keeps it (asks the store nothing, gives nothing) and no commit
/// makes the arena again. The control (the plant: the images given by the first serve, after
/// queries ran, the order before 2026-11-14): `core` and `lib` are made again.
#[test]
fn a_start_that_serves_a_crate_makes_its_arena_once() {
    let _turn = TURN.lock();
    a_start_given_found_images(true).unwrap_or_else(|why| panic!("{why}"));
    let planted = a_start_given_found_images(false).expect_err("a crate made again past the start was not caught");
    assert!(planted.contains("made their arenas again past the start"), "{planted}");
    assert!(planted.contains("\"core\"") && planted.contains("\"lib\""), "{planted}");
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
    let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
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
        .insert(key, Arc::new(Aligned::copy_from(&image_with(other, [b""; Part::ALL.len()]))));
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
    // A registry's library is pinned by its planner (`base_db::CrateData::sealed`): the
    // fixture pins only the crates it makes as libraries, and it makes `proc_macros` a member,
    // so it is pinned here with its origin, or nothing above it is sealed and a build's end
    // (`Which::Sealed`) stores none of them.
    data.sealed = Some(ContentId::of("blaze v2 test pin of proc_macros 1", &[]));
    _ = db.set_crate_data(macros, &data);
    db
}

/// Stores the images of a fresh [`reading_db`] in `memory`: the names of the crates written.
fn store_reading(memory: &Memory) -> Vec<String> {
    let mut db = reading_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, memory, Mode::Serve).unwrap();
    names(&db, &store(&db, &served, memory, Which::Sealed).unwrap().written)
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
        assert!(served.hits.contains(&lib.key(&db)), "{mode:?}");
        let raw: &Db = &db;
        let filed = image_of(raw, lib).unwrap().key();
        assert_ne!(filed, served.keys[&lib.key(&db)], "an image with reads is filed by them, not the key");
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
    let lib = crate_named(&db, "lib").key(&db);
    assert!(!served.hits.contains(&lib));
    assert!(served.stale.contains(&lib));
    let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
    assert!(stored.written.contains(&lib), "{stored:?}");
    let key = served.keys[&lib];
    let images = memory.images.lock();
    assert!(images.contains_key(&reads::slot(key, 0)));
    assert!(!images.contains_key(&reads::slot(key, 1)), "the same names share a slot");
    drop(images);
    let mut db = reading_db();
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert!(served.hits.contains(&crate_named(&db, "lib").key(&db)));
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
        let lib = crate_named(&db, "lib").key(&db);
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
    let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
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
        let (lib, app) = (crate_named(&db, "lib").key(&db), crate_named(&db, "app").key(&db));
        (db, served, lib, app)
    };
    let (db, served, lib, app) = serve_app();
    let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
    assert!(stored.written.contains(&lib) && stored.written.contains(&app), "{stored:?}");

    let (db, served, lib, app) = serve_app();
    assert!(served.hits.contains(&lib) && served.hits.contains(&app));
    let raw: &Db = &db;
    let filed = image_of(raw, db.krate(app)).unwrap().key();
    assert_ne!(filed, served.keys[&app], "a dependency's reads file the dependent's image");
    assert!(memory.images.lock().remove(&filed).is_some());

    // Imaged again with `lib` served from its image, not resolved: the same reads.
    let (db, served, lib, app) = serve_app();
    assert!(served.hits.contains(&lib) && !served.hits.contains(&app));
    let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
    assert!(stored.written.contains(&app), "{stored:?}");
    assert!(memory.images.lock().contains_key(&filed), "the same reads file it under the same key");

    VALUE.store(1, SeqCst);
    let (db, served, lib, app) = serve_app();
    assert!(!served.hits.contains(&lib));
    assert!(!served.hits.contains(&app), "served with what `lib`'s expansions made before");
    assert!(served.stale.contains(&app));
    store(&db, &served, &memory, Which::Sealed).unwrap();
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
        let lib = crate_named(db, "lib").key(db);
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
    assert!(served.hits.contains(&crate_named(&db, "lib").key(&db)));
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
    // The crates by key, which outlives the commits; their ids are taken again in each
    // snapshot, as an edit of `lib.rs` drops `lib`'s arena and `main`'s.
    let lib_key = crate_named(&db, "lib").key(&db);
    let crates = |db: &TestDB| (crate_named(db, "lib"), crate_named(db, "main"));
    let unknown = |db: &TestDB| {
        let log = db.files().expansions_read();
        log.iter()
            .any(|(by, read)| *by == lib_key && matches!(read, base_db::ExpansionRead::Unknown(_)))
    };
    let (lib, main) = crates(&db);
    let _ = crate::nameres::crate_def_map(&db, lib);
    assert!(unknown(&db), "the expansion ran and could not be traced");
    let refused = closure_expansion_reads(&db, main).unwrap_err();
    assert!(
        matches!(&refused, Refused::ProcMacro { why: "its server could not trace it", .. }),
        "{refused:?}"
    );

    TRACES.store(true, SeqCst);
    _ = db.set_file_text(file, "#[proc_macros::flaky]\npub struct Made {}\n");
    let (lib, main) = crates(&db);
    let _ = crate::nameres::crate_def_map(&db, lib);
    assert!(!unknown(&db), "the call's latest expansion traced: its unknown read is replaced");
    let read = closure_expansion_reads(&db, main).unwrap();
    assert_eq!(
        read,
        vec![(lib, vec![(Box::<[u8]>::from(SHAPE), values_of(SHAPE))])],
        "main's reads are kept again"
    );

    TRACES.store(false, SeqCst);
    _ = db.set_file_text(file, "#[proc_macros::flaky]\npub struct Made;\n");
    let (lib, main) = crates(&db);
    let _ = crate::nameres::crate_def_map(&db, lib);
    assert!(closure_expansion_reads(&db, main).is_err(), "unknown again");
    _ = db.set_file_text(file, "pub struct Made;\n");
    let (lib, main) = crates(&db);
    let _ = crate::nameres::crate_def_map(&db, lib);
    assert!(unknown(&db), "the call is gone, and what it last read stays in the log");
    assert!(
        closure_expansion_reads(&db, main).is_ok(),
        "the crate's calls as they are now refuse nothing"
    );
    TRACES.store(false, SeqCst);
}

/// The invariant: the expansion log holds one row per call site, however many edits: each
/// edit of `lib.rs` makes `lib`'s arena again and gives its call another id, and the call's
/// next expansion replaces the row of its site. Keyed by the call's id, a row was left under
/// each dead id, one more per edit.
///
/// The control: an edit that moves the call to another site (its item renamed) adds a row,
/// and the old site keeps its own: what the log is keyed by is the site.
#[test]
fn the_expansion_log_holds_one_row_per_call_site_across_edits() {
    let _turn = TURN.lock();
    VALUE.store(0, SeqCst);
    TRACES.store(false, SeqCst);
    let (mut db, file) = flaky_db();
    let expand = |db: &TestDB| {
        let _ = crate::nameres::crate_def_map(db, crate_named(db, "lib"));
    };
    expand(&db);
    assert_eq!(db.files().expansion_rows(), 1, "premise: the call's unknown read is logged");
    for text in ["pub struct Made {}", "pub struct Made;", "pub struct Made(u8);", "pub struct Made;"] {
        _ = db.set_file_text(file, &format!("#[proc_macros::flaky]\n{text}\n"));
        expand(&db);
        assert_eq!(db.files().expansion_rows(), 1, "after `{text}`: one call site, one row");
    }

    _ = db.set_file_text(file, "#[proc_macros::flaky]\npub struct Moved;\n");
    expand(&db);
    assert_eq!(db.files().expansion_rows(), 2, "the control: a call at another site is another row");
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
    let unset = reads.values_then(&db, crate_named(&db, "lib"));
    // `lib` is taken again after each write: a session write drops every arena it reads.
    let set = |db: &mut TestDB, value: &str| {
        let vars = Env::from_iter([("BLAZE_DEPS_TEST".into(), value.into())]);
        _ = db.commit_with(|edit| _ = edit.set_process_env(vars));
    };
    set(&mut db, "1");
    let one = reads.values_then(&db, crate_named(&db, "lib"));
    set(&mut db, "2");
    let two = reads.values_then(&db, crate_named(&db, "lib"));
    assert!(unset != one && one != two && unset != two);
    let mut list = Vec::new();
    reads::write_list(&mut list, ContentId([4; 32]), &reads.encode());
    assert_ne!(reads::image_key(&list, one), reads::image_key(&list, two));
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
/// dependency's by its index, the crates that read `CARGO_PRIMARY_PACKAGE`, and the paths
/// of the files outside the VFS.
pub(super) type ReadsParts =
    (Vec<Box<str>>, Vec<Box<[u8]>>, Vec<(u32, Vec<Box<[u8]>>)>, Vec<u32>, Vec<Box<[u8]>>);

/// The version of the reads' encoding [`old_reads`] reads: 4, which has the files outside
/// the VFS after the crates that read `CARGO_PRIMARY_PACKAGE`.
const OLD_READS_VERSION: u8 = 4;

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
    let (mut env, mut macros, mut deps, mut primary, mut files) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
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
    for _ in 0..count(&mut rest)? {
        files.push(framed(&mut rest)?);
    }
    let sorted = increasing(&env)
        && increasing(&macros)
        && deps.windows(2).all(|pair| pair[0].0 < pair[1].0)
        && increasing(&primary)
        && increasing(&files);
    (rest.is_empty() && sorted).then_some((env, macros, deps, primary, files))
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
        Reads::default().with_files_for_tests(&[b""]),
        Reads::for_tests(&["C"], &[(b"own", ContentId([4; 32]))])
            .with_deps_for_tests(&[(2, &[b"e"])], &[0])
            .with_files_for_tests(&[b"\0f/registry/lib/a.rs", b"/usr/include/x.rs"]),
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

/// The reads `reads` (which hold no file) as the previous layout, version 3, wrote them:
/// the same parts up to the crates that read `CARGO_PRIMARY_PACKAGE`, and no list of files
/// after them.
fn reads_of_layout_3(reads: &Reads) -> Vec<u8> {
    let now = reads.encode();
    let (parts, files) = now.split_at(now.len() - 4);
    assert_eq!(files, [0; 4], "the sample holds no file, so layout 4 ends in a count of none");
    let mut was = parts.to_vec();
    was[0] = 3;
    was
}

/// Reads of the previous layout (version 3, before the files outside the VFS) are handled:
/// refused by name where they are decoded, and a slot that holds them stops the search for
/// the crate with an error naming the crate, the slot and that refusal; never read as reads
/// that hold no file. (A key of this build does not find one: the keys' producer covers
/// [`READS_DOMAIN`](super::READS_DOMAIN), which moved with the version.)
#[test]
fn reads_of_the_previous_layout_are_refused_by_name() {
    let reads = Reads::for_tests(&["A"], &[(b"one", ContentId([1; 32]))])
        .with_deps_for_tests(&[(1, &[b"a"])], &[1]);
    let was = reads_of_layout_3(&reads);
    assert_eq!(Reads::decode(&was), Err("reads of another version"));
    // The control: the same bytes with this version's number are not reads either (the
    // files' count is missing), so the version alone is not what tells them apart.
    let mut renumbered = was.clone();
    renumbered[0] = reads.encode()[0];
    assert_eq!(Reads::decode(&renumbered), Err("reads shorter than their lengths"));

    let db = fixture::db("//- /lib.rs crate:lib new_source_root:library\npub fn f() {}\n").into_db();
    let lib = crate_named(&db, "lib");
    let memory = Memory::default();
    let key = ContentId([6; 32]);
    let mut list = Vec::new();
    reads::write_list(&mut list, key, &was);
    memory.put(reads::slot(key, 0), list).unwrap();
    let lookups = blaze_record::Tally::new();
    let why =
        find_by_reads(&db, lib, key, &memory, &lookups, |_| Ok(None::<ContentId>)).unwrap_err();
    assert!(
        why.contains("lib") && why.contains("slot 0") && why.contains("reads of another version"),
        "{why}"
    );
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
    let bob = checkout("/srv/build/bob/deps/x", "/scratch/target-77/out", "pub struct A;");
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
    let clean: [(Part, &[u8]); 3] =
        [(Part::LangItems, b"lang"), (Part::Signatures, b"signatures"), (Part::DefMap, b"pages")];
    assert_eq!(base.held_by(&clean), None);
    let mut held = clean;
    held[1].1 = b"..\x01/home/alice/work/src/gen.rs..";
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
    // The second session's crates are its own database's: a `Crate` is an id of one
    // snapshot's arena, not a name that holds across databases.
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &through, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    assert_eq!(*global.gets.lock(), 2, "the second session reads the local store alone");
}

#[test]
fn a_crate_with_no_outside_input_is_not_refused() {
    let db = fixture::db(FIXTURE).into_db();
    for name in ["core", "lib"] {
        assert_eq!(serve::refusal(&db, crate_named(&db, name)), None, "{name}");
        let walked = late::walk(&db, crate_named(&db, name), &[], late::Nested::Walk);
        assert!(walked.as_ref().is_ok_and(|it| it.unasked.is_none()), "{name}");
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

    // dyn: `ImageStore::pages`'s signature, once per crate read from pages
    fn pages(&self, key: &ContentId) -> Result<Option<Arc<dyn crate::deps::PageSource>>, String> {
        self.0.pages(key)
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
    // A build's database: the store's threads ask its queries at once, which a test
    // database's single flight (one thread's) cannot tell from a cycle.
    let mut db = fixture::db_on_threads(FIXTURE).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &threads, Mode::Serve).unwrap();
    let stored = store(&db, &served, &threads, Which::Sealed).unwrap();
    assert_eq!(names(&db, &stored.written), ["core", "lib"]);
    assert_eq!(contents(&threads.0), contents(&memory));
    let mut db = fixture::db_on_threads(FIXTURE).into_db();
    // This database's crates: ids are another database's in the first.
    let crates = all(&db);
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
    searched_in(db, lib, key, memory).0
}

/// [`found_in`], with what the search of the key's slots came to as `blaze_record` counted it.
fn searched_in(
    db: &TestDB,
    lib: Crate,
    key: ContentId,
    memory: &Memory,
) -> (Option<ContentId>, blaze_record::Counts) {
    let lookups = blaze_record::Tally::new();
    let found =
        find_by_reads(db, lib, key, memory, &lookups, |at| Ok(memory.get(&at)?.map(|_| at)))
            .unwrap();
    (found, lookups.take())
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
        _ = db.commit_with(|edit| _ = edit.set_process_env(vars));
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
    // Each search is one lookup that read the one slot filed and valued its one set of reads;
    // it is a hit while the value those reads name is kept.
    let counted = |hits| blaze_record::Counts { lookups: 1, hits, lists: 1, reads: 1 };
    assert_eq!(searched_in(&db, lib, key, &memory), (Some(filed), counted(1)));
    // `lib` again after each write: a session write drops every arena it reads.
    set(&mut db, "2");
    assert_eq!(searched_in(&db, crate_named(&db, "lib"), key, &memory), (None, counted(0)));
    set(&mut db, "1");
    assert_eq!(found_in(&db, crate_named(&db, "lib"), key, &memory), Some(filed));
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
        _ = db.commit_with(|edit| _ = edit.set_process_env(vars));
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
    assert_eq!(fresh.files().env_read(), vec![(lib.key(&fresh), "BLAZE_DEPS_NOTED".to_owned())]);
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
        _ = db.commit_with(|edit| _ = edit.set_process_env(vars));
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
    assert!(served.hits.contains(&lib.key(&db)));
    assert!(checked_reads(&db, lib).unwrap().is_empty());
}

/// Serving a crate at a time adds to what was served: a crate served before under the same key
/// is kept, not fetched again, and one whose key changed is fetched again and, when missing,
/// loses its image.
#[test]
fn a_serve_that_finds_nothing_writes_nothing() {
    let db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    // `serve` reads the snapshot it is given; what it would change is what it returns.
    let (served, found) = super::serve(&db, &crates, BUILD, &Memory::default(), Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new());
    assert!(
        found.is_empty() && served.withdrawn.is_empty(),
        "a cold store's misses give nothing to the next snapshot"
    );
    let raw: &Db = &db;
    assert!(crates.iter().all(|&krate| image_of(raw, krate).is_none()), "nothing served, nothing given");
}

#[test]
fn serving_a_crate_at_a_time_adds_to_what_was_served() {
    let (memory, _) = stored(FIXTURE);
    let mut db = fixture::db(FIXTURE).into_db();
    // A `Crate` is an id of one snapshot's arena: a serve's give commits the next snapshot,
    // which makes a served crate's arena again, so each crate is named again after each
    // serve, as a driver names its crates by key across a commit (`Served`). Kept across
    // the commit, the id of the dropped arena is refused by `Memo::next`'s check.
    let named = |db: &TestDB| (crate_named(db, "lib"), crate_named(db, "core"));
    let (_, core) = named(&db);
    let first = serve(&mut db, &[core], BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &first.hits), ["core"]);
    let gets = *memory.gets.lock();
    let (lib, _) = named(&db);
    let second = serve(&mut db, &[lib], BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &second.hits), ["core", "lib"]);
    assert_eq!(*memory.gets.lock(), gets + 1, "core is kept, not fetched again");
    let (lib, core) = named(&db);
    let raw: &Db = &db;
    assert!(image_of(raw, core).is_some() && image_of(raw, lib).is_some());
    assert!(is_paged(raw, lib) && is_paged(raw, core));
    assert!(
        [lib, core].iter().all(|&krate| image_of(raw, krate).unwrap().def_map.is_none()),
        "a served crate's record held whole"
    );
    let again = serve(&mut db, &[lib], BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(*memory.gets.lock(), gets + 1, "nothing is fetched again");
    assert_eq!(again.hits, second.hits);
    let (lib, _) = named(&db);
    let other = serve(&mut db, &[lib], ContentId([8; 32]), &memory, Mode::Serve).unwrap();
    assert!(other.hits.is_empty(), "another build has other keys, which have no image");
    let (lib, core) = named(&db);
    let raw: &Db = &db;
    assert!(image_of(raw, lib).is_none() && image_of(raw, core).is_none());
    assert!(!is_paged(raw, lib) && !is_paged(raw, core));
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

/// The invariant: a crate whose `#[path]` module is a file of another package's source root
/// (in an arena of that root's, made before every crate's) reads it as its own module, and
/// an edit of that file reaches the crate: the commit makes the crate's arena again, and its
/// def map then has the file's new items. The edit drops the file's root, the crates rooted
/// there and the crates that read it across roots, with their dependents, and no other.
///
/// The control: the same edit with the `#[path]` module taken out of `lib` carries `lib`'s
/// arena: the read across roots is what takes the edit to it, not a drop of every arena.
#[test]
fn an_edit_of_a_path_module_in_another_source_root_reaches_the_crate_that_reads_it() {
    const READS: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library
#[path = "../../../ws/member/src/shared.rs"]
pub mod m;
//- /ws/member/src/lib.rs crate:member new_source_root:local
pub mod shared;
//- /ws/member/src/shared.rs
pub fn f() {}
//- /other/src/lib.rs crate:other new_source_root:local
pub fn o() {}
"#;
    let _turn = TURN.lock();
    // Whether the edit of `shared.rs` made each crate's arena again, by name; and the names
    // `lib`'s module of that file declares after it (none where `lib` has no such module).
    let edit = |fixture: &str| -> (Vec<(&'static str, bool)>, Vec<String>) {
        let fx = fixture::db(fixture);
        let shared = fx.file_at("/ws/member/src/shared.rs").file_id(fx.db());
        let mut db = fx.into_db();
        let keys = ["lib", "member", "other"].map(|name| (name, crate_named(&db, name).key(&db)));
        let _ = crate::nameres::crate_def_map(&db, crate_named(&db, "lib"));
        let _ = crate::nameres::crate_def_map(&db, crate_named(&db, "member"));
        let committed = db.set_file_text(shared, "pub fn g() {}\n");
        let dropped = keys
            .iter()
            .map(|&(name, key)| (name, committed.dropped.contains(&db.krate(key).gid().arena().index())))
            .collect();
        let lib = crate_named(&db, "lib");
        let def_map = crate::nameres::crate_def_map(&db, lib);
        let declared = def_map
            .modules(&db)
            .find(|(_, it)| it.origin.file_id().map(|file| file.file_id(&db)) == Some(shared))
            .map(|(_, it)| it.scope(&db).entries().map(|(name, _)| name.as_str().to_owned()).collect())
            .unwrap_or_default();
        (dropped, declared)
    };
    let (dropped, declared) = edit(READS);
    assert_eq!(dropped, [("lib", true), ("member", true), ("other", false)]);
    assert_eq!(declared, ["g"], "`lib`'s module `m` is the file as edited");

    let (dropped, declared) =
        edit(&READS.replace("#[path = \"../../../ws/member/src/shared.rs\"]\npub mod m;", "pub fn own() {}"));
    assert_eq!(declared, Vec::<String>::new(), "premise: `lib` has no module of the file");
    assert_eq!(dropped, [("lib", false), ("member", true), ("other", false)], "the control");
}

/// A registry crate under `dir` that includes `<dir>/outside.rs`, a file of no source root.
fn outside_fixture(dir: &std::path::Path) -> String {
    let dir = dir.to_str().expect("a UTF-8 temporary directory");
    format!(
        "//- {dir}/lib/src/lib.rs crate:lib new_source_root:library\n\
         #[rustc_builtin_macro]\nmacro_rules! include {{ () => {{}} }}\n\
         include!(\"../../outside.rs\");\n"
    )
}

/// The invariant: a crate that includes a file outside the VFS has an image, filed by that
/// file's content: the image is served while the file is as it was read, missed once it is
/// edited (the key, which cannot cover the file, does not move), and found again when the
/// file is put back. Until 2026-11-12 such a crate was refused an image whole.
///
/// The control: the same reads with the file left out have one value before and after the
/// edit, so the image made before would be served after it.
#[test]
fn an_include_of_a_file_outside_the_vfs_files_the_image_by_the_files_content() {
    let _turn = TURN.lock();
    base_db::LOOKS_EVERY_TIME.set(true);
    let dir = blaze_core::temp::TempDir::for_test("blaze-deps-outside-");
    let file = dir.join("outside.rs");
    let fixture = outside_fixture(dir.path());
    let hits = |memory: &Memory| {
        let mut db = fixture::db(&fixture).into_db();
        let crates = all(&db);
        let served = serve(&mut db, &crates, BUILD, memory, Mode::Serve).unwrap();
        let hits = names(&db, &served.hits);
        let stored = store(&db, &served, memory, Which::Sealed).unwrap();
        assert_eq!(stored.refused, Vec::new());
        (hits, stored.written.len())
    };
    let reads = || {
        let db = fixture::db(&fixture).into_db();
        let lib = crate_named(&db, "lib");
        let reads = Reads::of(&db, lib, &[]).unwrap();
        let values = reads.values_then(&db, lib);
        let without = reads.clone().without_files_for_tests().values_then(&db, lib);
        (reads.encode(), values, without)
    };

    std::fs::write(&file, "pub fn one() {}\n").unwrap();
    let key = key_of(&fixture, "lib", BUILD);
    let one = reads();
    let memory = Memory::default();
    assert_eq!(hits(&memory), (Vec::new(), 1), "imaged, where it was refused");
    assert_eq!(hits(&memory), (vec!["lib".to_owned()], 0), "served while the file is as read");

    std::fs::write(&file, "pub fn two() {}\n").unwrap();
    assert_eq!(key_of(&fixture, "lib", BUILD), key, "premise: the file is not in the key");
    let two = reads();
    assert_eq!(one.0, two.0, "the same file is read");
    assert_ne!(one.1, two.1, "by another content");
    assert_eq!(one.2, two.2, "the control: with the file left out, the stale image's values");
    assert_eq!(hits(&memory), (Vec::new(), 1), "the edit misses the image, and another is made");
    assert_eq!(hits(&memory), (vec!["lib".to_owned()], 0));

    std::fs::write(&file, "pub fn one() {}\n").unwrap();
    assert_eq!(hits(&memory), (vec!["lib".to_owned()], 0), "put back: the first image again");
    base_db::LOOKS_EVERY_TIME.set(false);
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
    // `lib` again after the write: the write dropped the arena its id was of.
    let lib = crate_named(&db, "lib");
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
    let stored = store(&db, &served, &memory, Which::Sealed).unwrap();
    assert_eq!(stored.refused, Vec::new(), "a file of its own source root is no outside read");
    assert_eq!(names(&db, &stored.written), ["lib"]);
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
            files.included_by(&db, lib.key(&db)),
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
    let key = lib.key(&db);
    let mut data = lib.data(&db).keyed(&db);
    // One write: the alias, what it appends, and `lib` rooted at it.
    _ = db.commit_with(|edit| {
        let alias = edit.alias(data.root_file_id).unwrap();
        edit.set_alias_appended(alias, Some(("__glue", glue))).unwrap();
        data.root_file_id = alias;
        edit.set_crate_data(key, data);
    });
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
    assert_eq!(names(&db, &store(&db, &served, &memory, Which::Sealed).unwrap().written), ["core", "lib"]);

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
    // Each crate is taken again in each snapshot: a write drops the arenas of what it moved.
    let crates = |db: &TestDB| (crate_named(db, "core"), crate_named(db, "lib"));
    let (core, lib) = crates(&db);
    assert_eq!((mode(&db, core), mode(&db, lib)), (Some(Mode::Serve), Some(Mode::Serve)));
    let text =
        db.file_text(deep).text(&db).replace("pub fn f() {}", "pub fn f() {} pub fn more() {}");
    write_files(&mut db, &[deep], |db| _ = db.set_file_text(deep, &text));
    let (core, lib) = crates(&db);
    assert_eq!(mode(&db, lib), None, "`lib`'s image is withdrawn by the write of its file");
    assert!(image(&db, lib).is_none());
    assert_eq!(mode(&db, core), Some(Mode::Serve), "`core` keeps its image");
    resolves_fresh(&db, &FIXTURE.replace("pub fn f() {}", "pub fn f() {} pub fn more() {}"), "lib");

    // `core.rs`: `core` loses its image, and so does `lib`, which depends on it.
    let (mut db, core_rs) = served_db(3);
    let more =
        |text: &str| text.replace("pub trait Sized {}", "pub trait Sized {}\npub struct More;");
    let text = more(&db.file_text(core_rs).text(&db));
    write_files(&mut db, &[core_rs], |db| _ = db.set_file_text(core_rs, &text));
    let (core, lib) = crates(&db);
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

/// Def map pages read against a file they were not made from stop the query that asked with
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
    let bytes = frozen::pages::encode(&db, crate_named(&db, "lib")).unwrap();
    let text =
        db.file_text(deep).text(&db).replace("pub fn f() {}", "pub fn f() {} pub fn more() {}");
    _ = db.set_file_text(deep, &text);
    // `lib` of each snapshot: the edit and the serve each commit, which drops its arena.
    let before = crate_named(&db, "lib");
    frozen::serve_pages(&mut db, &[(before, bytes)], Mode::Serve);
    let lib = crate_named(&db, "lib");
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        _ = crate::nameres::crate_def_map(&db, lib);
    }));
    let Err(payload) = caught else {
        panic!("`lib`'s frozen def map was read against a file it was not made from")
    };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("the refusal unwound with another payload than `frozen::Unreadable`")
    };
    assert_eq!((stop.krate, stop.kind, stop.name.as_deref()), (lib, "def map pages", Some("lib")));
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

/// The invariant: a dependency enters the key of a crate above it by its surface where the
/// lowering gave it one ([`super::given::give_surface`]), so an edit of the dependency that
/// leaves its surface as it was moves the dependency's own key and not the key above, and a
/// surface that moved moves the key above. The control, first (the cause removed): with no
/// surface given the same kind of edit moves both, the dependency then entering by its key.
#[test]
fn a_dependency_enters_the_key_above_it_by_its_surface_where_it_was_given_one() {
    let text = chain(2);
    let fixture = fixture::db(&text);
    let inner = fixture.file_at("/lib1.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    // Each crate by name in the snapshot asked: every write here commits, which drops the
    // arenas of the crates it moved.
    let key = |db: &TestDB, name: &str| {
        let krate = crate_named(db, name);
        keys(db, &[krate], BUILD)[&krate]
    };
    let (above, below) = ("lib0", "lib1");

    let (was_above, was_below) = (key(&db, above), key(&db, below));
    _ = db.set_file_text(inner, "pub fn f1() { let _ = 1; }\n");
    assert_ne!(key(&db, below), was_below, "a crate's own key covers its bytes");
    assert_ne!(key(&db, above), was_above, "with no surface given, the key above covers them too");

    let surface = ContentId([3; 32]);
    let given_to = crate_named(&db, below);
    give_surface_next(&mut db, given_to, surface);
    assert_eq!(super::given::surface(&db, crate_named(&db, below)), Some(surface));
    let (with_above, with_below) = (key(&db, above), key(&db, below));
    _ = db.set_file_text(inner, "pub fn f1() { let _ = 2; }\n");
    assert_ne!(key(&db, below), with_below, "its own key still covers its bytes");
    assert_eq!(key(&db, above), with_above, "the key above holds the surface, which stands");

    let given_to = crate_named(&db, below);
    give_surface_next(&mut db, given_to, ContentId([4; 32]));
    assert_ne!(key(&db, above), with_above, "a surface that moved moves the key above");
}

/// What a driver does once the lowering has worked out `krate`'s surface: gives it in the
/// crate in the next snapshot ([`super::given::give_surface`]), `krate` of the snapshot `db`
/// holds now.
fn give_surface_next(db: &mut TestDB, krate: Crate, surface: ContentId) {
    let krate_id = stable_id(db, krate);
    _ = db.commit_with(|edit| {
        super::given::give_surface(edit, krate, krate_id, surface);
    });
}

/// A dependency of a source root of its own, of five modules over three files, and a crate
/// that names it by a path (`lib::m::f`) and by a glob import (`lib::*`, which takes `S` and
/// `h`, and `f` through `lib`'s own glob of `m`). What hello-world's path asks of `std` is in
/// `lib` too: a lang item (`main`'s lang items read every module of `lib`'s scope), a module of
/// `macro_rules!` (two of one name, told apart by their order, and an exported one `main` calls
/// at item level, which writes `Made`), and an `include!` in a module, as `core`'s
/// `include!("primitive_docs.rs")` is.
const NAMED_FROM_ABOVE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
use lib::*;
use lib::m::f as g;
lib::made!();
pub fn main() { g(); h(); }

//- /lib.rs crate:lib new_source_root:library
#[rustc_builtin_macro]
macro_rules! include { () => {} }
pub mod m;
pub mod inner { pub struct Deep; }
pub mod macros {
    macro_rules! twice { () => {} }
    macro_rules! twice { () => { pub struct Twice; } }
    twice!();
    #[macro_export]
    macro_rules! made { () => { pub struct Made; } }
}
pub mod docs { include!("inc.rs"); }
pub use m::*;
pub fn h() {}
#[lang = "sized"]
pub trait Sized {}

//- /m.rs
pub fn f() {}
pub struct S;

//- /inc.rs
pub struct Included;
"#;

/// What `main`'s def map holds in `db`: its whole record, written as the stream encoder
/// writes it (which names everything by what it is, so two databases compare by it), and
/// whether each name it was given by `lib` (`g` by a path, `S` and `h` by a glob) or made by
/// `lib`'s macro (`Made`) resolves, and whether `main`'s lang items found `lib`'s `Sized`.
fn main_of(db: &TestDB) -> (Vec<u8>, [bool; 5]) {
    let main = crate_named(db, "main");
    let def_map = crate::nameres::crate_def_map(db, main);
    let [g, s, h, made] = ["g", "S", "h", "Made"].map(|name| {
        let name = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern(name));
        !def_map.name_in_module(db, def_map.root, &name).is_none()
    });
    let sized = crate::lang_item::lang_items(db, main).Sized.is_some();
    (frozen::encode(db, main).unwrap(), [g, s, h, made, sized])
}

/// How many runs of `query` `ran` holds, by the name the run log gives it.
fn runs(ran: &[String], query: &str) -> usize {
    ran.iter().filter(|it| it.contains(query)).count()
}

/// The runs in `ran` that read the syntax of a file of `krate` in `db`: its parse, its AST id
/// map or its span map (`parse_`, `ast_id_map`, `real_span_map`), each keyed by the file's
/// [`base_db::EditionedFileId`], which the run log prints with its id first. A file of the
/// crate is read in the crate's edition under its extensions, as [`Crate::root_file_id`] and
/// `include!` make it.
fn syntax_reads_of(db: &TestDB, krate: Crate, ran: &[String]) -> Vec<String> {
    let root = krate.data(db).root_file_id;
    let source_root = db.file_source_root(root).source_root_id(db);
    let (edition, extensions) = (krate.data(db).edition, krate.extensions(db));
    // The file's id as the run log prints it: unattached, `EditionedFileId(<id>)`; attached,
    // `EditionedFileId { [id]: <id>, <fields> }` (`blaze_memo::interned`'s `Debug`).
    let marks: Vec<[String; 2]> = db
        .source_root(source_root)
        .source_root_ref(db)
        .iter()
        .map(|file| {
            let file = base_db::EditionedFileId::new_with_extensions(db, file, edition, extensions);
            let shown = format!("{file:?}");
            let id = match shown.strip_prefix("EditionedFileId(") {
                Some(rest) => rest.strip_suffix(')'),
                None => shown.strip_prefix("EditionedFileId { [id]: ").and_then(|rest| rest.split(", ").next()),
            };
            let id = id.unwrap_or_else(|| panic!("a file id the run log's form does not print: {shown}"));
            [format!("EditionedFileId({id})"), format!("EditionedFileId {{ [id]: {id},")]
        })
        .collect();
    assert!(!marks.is_empty(), "premise: `{}` has files", krate.named(db));
    ran.iter()
        .filter(|run| ["parse_", "ast_id_map", "real_span_map"].iter().any(|query| run.contains(query)))
        .filter(|run| marks.iter().flatten().any(|mark| run.contains(mark.as_str())))
        .cloned()
        .collect()
}

/// The invariant: a crate served from its records is read in place: resolving a crate that
/// names it, by a path and by a glob import, calls its `macro_rules!` and finds its lang item
/// reads no syntax of any file of it (no `parse_`, `ast_id_map` or `real_span_map` of one: its
/// `include!` is resolved from the call's tokens, its scopes from its pages alone), collects
/// none of its modules (`module_decls` runs for the dependent's one module alone), and holds
/// no decoded def map of it (its image holds no def map whole, [`Image`]); and what the
/// dependent resolves to is what it resolves to over the crate resolved from source
/// ([`Mode::Check`]). The cause removed, under `Mode::Check`, the same resolution collects
/// every module of the dependency and reads the syntax of its files, which is what the counts
/// are of (and what shows [`syntax_reads_of`] finds a read when there is one). The control:
/// pages whose name tables leave `S` out, served in their place, are caught by the comparison.
#[test]
fn a_served_crate_parses_no_file_and_decodes_no_def_map() {
    use crate::test_db::TestDBExt;
    let _turn = TURN.lock();
    let (memory, stored) = stored(NAMED_FROM_ABOVE);
    assert_eq!(stored.refused, Vec::new());
    let mut checked = fixture::db(NAMED_FROM_ABOVE).into_db();
    let crates = all(&checked);
    let served = serve(&mut checked, &crates, BUILD, &memory, Mode::Check).unwrap();
    assert_eq!(names(&checked, &served.hits), ["lib"]);
    let mut from_source = None;
    let ran_from_source = checked.executed_in_read_order(|| from_source = Some(main_of(&checked)));
    let from_source = from_source.unwrap();
    assert_eq!(from_source.1, [true; 5], "premise: `main` names what `lib` has and makes");
    assert!(
        runs(&ran_from_source, "module_decls") >= 2,
        "resolved from source, `lib`'s modules and `main`'s are collected: {ran_from_source:?}"
    );
    let lib_from_source = crate_named(&checked, "lib");
    let read_from_source = syntax_reads_of(&checked, lib_from_source, &ran_from_source);
    for query in ["parse_", "ast_id_map"] {
        assert!(
            read_from_source.iter().any(|run| run.contains(query)),
            "premise: resolved from source, `lib`'s files are read by `{query}`: {ran_from_source:?}"
        );
    }

    let mut db = fixture::db(NAMED_FROM_ABOVE).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["lib"]);
    let lib = crate_named(&db, "lib");
    let raw: &Db = &db;
    assert!(is_paged(raw, lib), "`lib` is read from its pages");
    assert!(
        image_of(raw, lib).unwrap().def_map.is_none(),
        "a served crate's def map record is held whole"
    );
    let mut read = None;
    let ran = db.executed_in_read_order(|| read = Some(main_of(&db)));
    assert_eq!(read.unwrap(), from_source, "`main` over a served `lib` is `main` over its source");
    assert_eq!(runs(&ran, "module_decls"), 1, "only `main`'s module is collected: {ran:?}");
    let lib = crate_named(&db, "lib");
    assert_eq!(
        syntax_reads_of(&db, lib, &ran),
        Vec::<String>::new(),
        "a file of the served `lib` had its syntax read: {ran:?}"
    );
    assert!(
        runs(&ran, "item_tree") < runs(&ran_from_source, "item_tree"),
        "as many item trees made as over `lib`'s source: {ran:?}"
    );
    assert_eq!(frozen::differences(), Vec::<String>::new());

    // The control: `lib`'s pages with `S` left out of every name table, served in their place.
    let planted =
        frozen::pages::encode_without(&checked, crate_named(&checked, "lib"), "S").unwrap();
    let mut wrong = fixture::db(NAMED_FROM_ABOVE).into_db();
    let lib = crate_named(&wrong, "lib");
    frozen::serve_pages(&mut wrong, &[(lib, planted)], Mode::Serve);
    assert_ne!(main_of(&wrong), from_source, "pages without `S` answered as `lib`'s source");
}

/// The invariant: an image of the layout before the def map left it (layout 7: three parts,
/// the def map's record one of them) is never read as this layout's and never an error: the
/// serve names it a miss of its layout ([`Served::old_layout`]), resolves the crate from
/// source, and serves every other crate as before.
#[test]
fn a_layout_7_image_is_a_named_miss() {
    let _turn = TURN.lock();
    let (memory, stored) = stored(FIXTURE);
    let mut db = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&db, "lib");
    let filed = stored.filed[&lib.key(&db)];
    let old = super::image::write_layout_7_image(filed, [b"def map", b"lang", b"signatures"]);
    assert!(ImageRef::is_layout_7(&Aligned::copy_from(&old)), "premise: the image is of layout 7");
    assert!(ImageRef::open(&Aligned::copy_from(&old), filed).is_err(), "premise: not this layout");
    memory.images.lock().insert(filed, Arc::new(Aligned::copy_from(&old)));
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.old_layout), ["lib"]);
    assert_eq!(names(&db, &served.hits), ["core"]);
    // `lib` again after the serve: its commit may have dropped the arena its id was of.
    let lib = crate_named(&db, "lib");
    assert_eq!(mode(&db, lib), None, "`lib` is resolved from source");
    assert_eq!(mode(&db, crate_named(&db, "core")), Some(Mode::Serve));
    let fresh = fixture::db(FIXTURE).into_db();
    let live = crate_named(&fresh, "lib");
    assert_eq!(frozen::encode(&db, lib), frozen::encode(&fresh, live));
}

/// [`FIXTURE`] with an unresolved import in the workspace member too: its pages carry a
/// diagnostic of their own.
fn member_fixture() -> String {
    FIXTURE.replace("fn main() { shown!(); f(); }", "use lib::Absent;\nfn main() { shown!(); f(); }")
}

/// A bug planted in what is compared of a crate served from its pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    Nothing,
    /// The served member's resolution is short one diagnostic, as pages that dropped one are.
    DropsADiagnostic,
}

/// Where a database that read every crate from its pages, the workspace member's too (stored
/// with [`Which::All`] by another), differs from a fresh resolution of the same text, by crate
/// and part: its diagnostics (counted, less one for `main` under the plant), its frozen
/// encoding and its lang items.
fn served_against_fresh(plant: Plant) -> Vec<String> {
    let text = member_fixture();
    let memory = Memory::default();
    let mut first = fixture::db(&text).into_db();
    let crates = all(&first);
    let served = serve(&mut first, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let stored = store(&first, &served, &memory, Which::All).unwrap();
    assert_eq!(names(&first, &stored.written), ["core", "lib", "main"], "the member's pages are written too");
    let mut db = fixture::db(&text).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib", "main"]);
    assert_eq!(mode(&db, crate_named(&db, "main")), Some(Mode::Serve), "the member is read from its pages");
    let fresh = fixture::db(&text).into_db();
    let mut differs = Vec::new();
    for name in ["core", "lib", "main"] {
        let (krate, live) = (crate_named(&db, name), crate_named(&fresh, name));
        let mut diagnostics = crate::nameres::crate_def_map(&db, krate).diagnostics(&db).len();
        if plant == Plant::DropsADiagnostic && name == "main" {
            diagnostics = diagnostics.saturating_sub(1);
        }
        if diagnostics != crate::nameres::crate_def_map(&fresh, live).diagnostics(&fresh).len() {
            differs.push(format!("{name}: diagnostics"));
        }
        if frozen::encode(&db, krate) != frozen::encode(&fresh, live) {
            differs.push(format!("{name}: def map"));
        }
        if frozen::encode_lang_items(&db, krate) != frozen::encode_lang_items(&fresh, live) {
            differs.push(format!("{name}: lang items"));
        }
    }
    differs.extend(frozen::differences());
    differs
}

/// Rule 26. The invariant: a workspace member read from its pages (R3) resolves byte for
/// byte as a fresh resolution of its text, diagnostics and all, as do the crates below it.
#[test]
fn a_member_served_from_its_pages_resolves_as_a_fresh_resolution() {
    let _turn = TURN.lock();
    assert_eq!(served_against_fresh(Plant::Nothing), Vec::<String>::new());
}

/// The control: pages that drop one of the member's diagnostics are caught.
#[test]
fn the_oracle_catches_member_pages_that_drop_a_diagnostic() {
    let _turn = TURN.lock();
    assert_eq!(served_against_fresh(Plant::DropsADiagnostic), ["main: diagnostics"]);
}

/// Two members over one library, `app` on `mid` on `lib`, and `other` on `lib`.
const MEMBERS: &str = r#"
//- /app.rs crate:app deps:mid new_source_root:local
pub fn app() { mid::mid(); }
//- /mid.rs crate:mid deps:lib new_source_root:local
pub fn mid() { lib::lib(); }
//- /other.rs crate:other deps:lib new_source_root:local
pub fn other() { lib::lib(); }
//- /lib.rs crate:lib new_source_root:library
pub fn lib() {}
"#;

/// What a fresh database of [`MEMBERS`] is served from pages every crate of which was written
/// (with [`Which::All`]) before `mid.rs` was edited, by name: with `edits` none, all of them.
fn served_after_an_edit_of_mid(edits: bool) -> Vec<String> {
    let memory = Memory::default();
    let mut first = fixture::db(MEMBERS).into_db();
    let crates = all(&first);
    let served = serve(&mut first, &crates, BUILD, &memory, Mode::Serve).unwrap();
    store(&first, &served, &memory, Which::All).unwrap();
    let fixture = fixture::db(MEMBERS);
    let mid = fixture.file_at("/mid.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    if edits {
        db.set_file_text(mid, "pub fn mid() { lib::lib(); let _ = 1; }\n");
    }
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    names(&db, &served.hits)
}

/// The invariant: an edit of a member withdraws its pages and those of the members above it,
/// and no other's: after an edit of `mid`, `mid` and `app` are resolved and `lib` and `other`
/// are read from their pages. Its control is the same serve with no edit, which reads all four.
#[test]
fn an_edit_of_a_member_withdraws_it_and_those_above_and_no_other() {
    let _turn = TURN.lock();
    assert_eq!(served_after_an_edit_of_mid(false), ["app", "lib", "mid", "other"], "the control: no edit");
    assert_eq!(served_after_an_edit_of_mid(true), ["lib", "other"]);
}

/// The members a store of `which` writes of a fresh database of [`FIXTURE`], and then what a
/// second store, with [`Which::All`], writes after it.
fn written_by(which: Which) -> [Vec<String>; 2] {
    let memory = Memory::default();
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let first = names(&db, &store(&db, &served, &memory, which).unwrap().written);
    let second = names(&db, &store(&db, &served, &memory, Which::All).unwrap().written);
    [first, second]
}

/// The invariant (R3, rule (b)): a build's end store ([`Which::Sealed`]) writes the sealed
/// crates alone, never a member, whose pages an edit would throw away; the second store, before
/// a front end is let go or at a stop, writes the member and nothing written already.
#[test]
fn a_build_end_store_writes_no_member() {
    let _turn = TURN.lock();
    assert_eq!(written_by(Which::Sealed), [vec!["core", "lib"], vec!["main"]]);
}

/// The control: a build's end that stored everything writes the member, which the oracle
/// above refuses.
#[test]
fn the_oracle_catches_a_build_end_store_that_writes_a_member() {
    let _turn = TURN.lock();
    assert_ne!(written_by(Which::All)[0], ["core", "lib"]);
}

/// A section of a record held where a test keeps it, read as a page source with its reads
/// counted: what a `Surface` record's pages section is to [`Image::in_place`].
struct Section {
    holder: ImageBuf,
    range: std::ops::Range<usize>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl PageSource for Section {
    fn size(&self) -> u64 {
        self.range.len() as u64
    }

    // dyn: as `PageSource::read`
    fn read(&self, at: u64, len: u64, with: &mut dyn FnMut(&[u8])) -> Result<(), String> {
        self.reads.fetch_add(1, SeqCst);
        let bytes = &self.holder.bytes()[self.range.clone()];
        let range = usize::try_from(at).ok().zip(usize::try_from(len).ok());
        let part = range.and_then(|(at, len)| bytes.get(at..at.checked_add(len)?));
        with(part.ok_or_else(|| format!("bytes {at}+{len} of a section of {}", bytes.len()))?);
        Ok(())
    }
}

/// The byte a test record is filled with around its sections.
const FILL: u8 = 0xa5;

/// A record laid out as a `Surface` record's payload is: `lead` bytes of [`FILL`], the image of
/// `parts` written under `key` at the next multiple of 16, then `pages` at the next, then 16
/// bytes of fill. The record, held as a store holds it, and the image's and the pages' ranges.
fn record_of(
    lead: usize,
    key: ContentId,
    parts: &[Portable; Part::ALL.len()],
    pages: &[u8],
) -> (ImageBuf, std::ops::Range<usize>, std::ops::Range<usize>) {
    let image = write_image(key, parts.each_ref());
    let mut bytes = vec![FILL; lead];
    let mut place = |section: &[u8]| {
        let start = bytes.len().next_multiple_of(blaze_format::ALIGN);
        bytes.resize(start, FILL);
        bytes.extend_from_slice(section);
        start..start + section.len()
    };
    let image_at = place(&image);
    let pages_at = place(pages);
    bytes.resize(bytes.len() + 16, FILL);
    let holder: ImageBuf = Arc::new(Aligned::copy_from(&bytes));
    (holder, image_at, pages_at)
}

/// The invariant: a crate given an image opened in place from a section of a larger record
/// ([`Image::in_place`] over [`image_parts`], as a `Surface` record holds it, at a non-zero
/// offset) is served from it and resolves as the crate built from source: its def map with
/// its diagnostics and its lang items, as the frozen encoding writes them, and its dependent's.
/// And opening and giving it read no page and run no query: only a query that asks reads.
///
/// The control of the second: the counter counts, since the first query that asks reads the
/// pages. The control of the first is that each crate is served (`Mode::Serve`) and read from
/// the record's pages, not resolved.
#[test]
fn a_crate_opened_in_place_from_a_record_resolves_as_a_fresh_one() {
    let _turn = TURN.lock();
    let source = fixture::db(FIXTURE).into_db();
    let mut db = fixture::db(FIXTURE).into_db();
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut opened = Vec::new();
    for name in ["core", "lib"] {
        let krate = crate_named(&source, name);
        let parts = image_parts(&source, krate).unwrap().parts;
        let pages = frozen::pages::encode(&source, krate).unwrap();
        let key = ContentId::of("hir-def test surface", name.as_bytes());
        let (holder, image_at, pages_at) = record_of(48, key, &parts, &pages);
        assert_ne!(image_at.start, 0, "the image is inside the record, not at its start");
        let section = Section { holder: holder.clone(), range: pages_at, reads: reads.clone() };
        let paged = Paged {
            key: ContentId::of("hir-def test surface pages", name.as_bytes()),
            source: Arc::new(section),
        };
        let image = Image::in_place(key, key, holder, image_at, Mode::Serve, Some(paged)).unwrap();
        let krate = crate_named(&db, name);
        opened.push((krate, stable_id(&db, krate), image));
    }
    assert_eq!(reads.load(SeqCst), 0, "opening reads no page");
    _ = db.runs().expect("a test database logs its runs").take();
    let mut edit = db.edit();
    for (krate, krate_id, image) in opened {
        give_image(&mut edit, krate, krate_id, image);
    }
    _ = edit.commit();
    assert_eq!(reads.load(SeqCst), 0, "giving reads no page");
    let ran = db.runs().expect("a test database logs its runs").take();
    assert!(ran.is_empty(), "giving runs no query: {ran:?}");

    let fresh = fixture::db(FIXTURE).into_db();
    for name in ["core", "lib"] {
        assert_eq!(mode(&db, crate_named(&db, name)), Some(Mode::Serve), "{name} is served");
    }
    for name in ["core", "lib", "main"] {
        let (krate, live) = (crate_named(&db, name), crate_named(&fresh, name));
        let def_map = crate::nameres::crate_def_map(&db, krate);
        let fresh_map = crate::nameres::crate_def_map(&fresh, live);
        assert_eq!(def_map.diagnostics(&db).len(), fresh_map.diagnostics(&fresh).len(), "{name}");
        assert_eq!(frozen::encode(&db, krate), frozen::encode(&fresh, live), "{name}");
        assert_eq!(
            frozen::encode_lang_items(&db, krate),
            frozen::encode_lang_items(&fresh, live),
            "{name}"
        );
    }
    assert!(reads.load(SeqCst) > 0, "a query that asks reads the record's pages");
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

/// A dependency of two files with a signature, a dependency of it with a lang item, and a
/// crate naming both of the first's files: what is given by its address ([`give_keyed`]).
/// `deep.rs` also holds an inherent impl and a trait with its impl, whose items, attribute
/// flags and written visibilities its record holds (items B, C and D of the port).
const KEYED_SOURCE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
use lib::{Shown, deep::deeper::f};
pub fn main() { let _ = f(1); }

//- /lib.rs crate:lib deps:core new_source_root:library
pub struct Shown;
pub mod deep;

//- /deep.rs
#[doc(hidden)]
pub mod deeper { #[doc(alias = "g")] pub fn f(x: u8) -> u8 { x } }
#[deprecated = "use f"]
pub fn old() {}
#[must_use]
pub struct Held { pub count: u8 }
impl Held {
    #[inline]
    pub fn get(&self) -> u8 { self.count }
    pub(crate) fn shared() {}
    fn own() {}
    #[doc(hidden)]
    pub const ONE: u8 = 1;
}
#[must_use]
pub trait Count { fn count(&self) -> u8; type Out; }
impl Count for Held { fn count(&self) -> u8 { self.count } type Out = u8; }
pub struct Boxed<'a, #[may_dangle] T>(&'a T);

//- /core.rs crate:core new_source_root:library
#![feature(lang_items)]
#[lang = "sized"]
pub trait Sized {}
"#;

/// [`KEYED_SOURCE`]'s crate graph, each dependency in a directory of its own so that its root can be
/// left with no text ([`hollow_db`]).
const HOLLOW: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
use lib::{Shown, deep::deeper::f};
pub fn main() { let _ = f(1); }

//- /lib/lib.rs crate:lib deps:core new_source_root:library
// hollow

//- /core/lib.rs crate:core new_source_root:library
// hollow
"#;

/// [`HOLLOW`] with none of the dependencies' files having text: each root's bytes set to bytes
/// that are not UTF-8, as a checkout's loader sets such a file (a file with no text, whose
/// text a read of panics in base-db), and `deep.rs` not in the database. A fixture's text
/// cannot leave a file without text, and base-db has no edit that takes a file's text away
/// (`Edit::set_file_text` sets one; a view's file has text when its bytes are UTF-8): bytes
/// that are not UTF-8 are how a file id with no text is made.
fn hollow_db() -> TestDB {
    let mut db = fixture::db(HOLLOW).into_db();
    for root in ["/lib/lib.rs", "/core/lib.rs"] {
        let file = fixture::set_bytes(&mut db, root, &[0xff, 0xfe]);
        assert!(!db.has_file_text(file), "premise: {root} has no text");
    }
    db
}

/// `core`'s and `lib`'s surfaces as records hold them, made from `source` and opened in place
/// for `db`'s crates of the same names: each crate, its stable id, its image (`Mode::Serve`,
/// read from its record's pages) and the record's address.
fn keyed_images<A: core::alloc::Allocator>(
    source: &TestDB,
    db: &TestDB,
    alloc: A,
) -> Vec<(Crate, ContentId, Image, ContentId), A> {
    let mut out = Vec::new_in(alloc);
    for name in ["core", "lib"] {
        let krate = crate_named(source, name);
        let parts = image_parts(source, krate).unwrap().parts;
        let pages = frozen::pages::encode(source, krate).unwrap();
        let key = ContentId::of("hir-def test keyed surface", name.as_bytes());
        let (holder, image_at, pages_at) = record_of(32, key, &parts, &pages);
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let section = Section { holder: holder.clone(), range: pages_at, reads };
        let paged = Paged {
            key: ContentId::of("hir-def test keyed surface pages", name.as_bytes()),
            source: Arc::new(section),
        };
        let image = Image::in_place(key, key, holder, image_at, Mode::Serve, Some(paged)).unwrap();
        let krate = crate_named(db, name);
        out.push((krate, stable_id(db, krate), image, key));
    }
    out
}

/// The proof a test's record was read at `at`: what the driver's `Table` stands for, made here
/// as `blaze_front::surface::Opened::give` makes it (the one other maker the ban allows).
#[expect(clippy::disallowed_methods, reason = "hir-def's tests stand in for the driver's checked `Table`")]
fn read_at(at: ContentId) -> ReadAtAddress {
    ReadAtAddress::vouched(at)
}

fn named(text: &str) -> hir_expand::name::Name {
    hir_expand::name::Name::new_symbol_root(intern::Symbol::intern(text))
}

/// What `path` names from the root of `krate`'s def map, each segment but the last a module
/// of the crate: its type if it has one, else its value.
fn item_at(db: &TestDB, krate: Crate, path: &[&str]) -> Option<crate::ModuleDefId> {
    let def_map = crate::nameres::crate_def_map(db, krate);
    let (last, modules) = path.split_last()?;
    let mut module = def_map.root;
    for segment in modules {
        match def_map.name_in_module(db, module, &named(segment)).take_types()? {
            crate::ModuleDefId::ModuleId(it) => module = it,
            _ => return None,
        }
    }
    let found = def_map.name_in_module(db, module, &named(last));
    found.take_types().or_else(|| found.take_values())
}

/// `lib::deep::deeper::f` of `db`.
fn keyed_f(db: &TestDB) -> crate::FunctionId {
    match item_at(db, crate_named(db, "lib"), &["deep", "deeper", "f"]) {
        Some(crate::ModuleDefId::FunctionId(f)) => f,
        other => panic!("`lib::deep::deeper::f` is not a function: {other:?}"),
    }
}

/// The invariant: a crate given by its address ([`give_keyed`], the proof
/// [`ReadAtAddress`]) with none of its files in the database answers from its record: its
/// def map (its own modules, over a file the database does not hold, and what a crate above
/// it imports), a signature (`f`'s, as the crate built from source has it) and an attribute
/// (`core`'s `#[lang = "sized"]`).
///
/// The planted control, the cause removed: the same records given by [`give_image`], whose
/// pages look up and compare every file, are refused naming `lib` (its root has no text,
/// `deep.rs` is not loaded): it is the keyed give that lets the hollow database answer, not
/// the records. Every root of the hollow database is a file with no text ([`hollow_db`]), so a
/// read of one that no guard stops is a base-db panic, which fails this test, never a read of
/// other text.
#[test]
fn a_crate_given_by_its_address_answers_with_none_of_its_files() {
    let _turn = TURN.lock();
    let source = fixture::db(KEYED_SOURCE).into_db();
    let fresh = fixture::db(KEYED_SOURCE).into_db();

    let mut db = hollow_db();
    let given = keyed_images(&source, &db, Global);
    let mut edit = db.edit();
    for (krate, krate_id, image, at) in given {
        give_keyed(&mut edit, krate, krate_id, image, read_at(at)).unwrap();
    }
    _ = edit.commit();

    let raw: &Db = &db;
    for name in ["core", "lib"] {
        let krate = crate_named(&db, name);
        assert_eq!(mode(raw, krate), Some(Mode::Serve), "{name} is served");
        let files = keyed_files(raw, krate).unwrap_or_else(|| panic!("{name} is keyed"));
        // The rule: one root, the crate's own root file id, for an ordinary root (no alias).
        let crate_root = krate.data(raw).root_file_id;
        assert!(raw.files().alias_of(crate_root).is_none(), "premise: {name}'s root is no alias");
        let roots: Vec<FileId, Global> = {
            let mut roots = Vec::new_in(Global);
            roots.extend(files.iter().filter(|it| it.is_root()).map(|it| it.file));
            roots
        };
        assert_eq!(roots, [crate_root], "{name}'s one root is its crate's root file");
        assert!(
            files.iter().filter(|it| !it.is_root()).all(|it| it.file != crate_root),
            "{name}: no other file of it is its root"
        );
        // And the def map read off its pages has that file for its root module.
        let def_map = crate::nameres::crate_def_map(&db, krate);
        let crate::nameres::ModuleOrigin::CrateRoot { definition } =
            def_map.module(&db, def_map.root).origin
        else {
            panic!("{name}'s root module is no crate root")
        };
        assert_eq!(definition.file_id(raw), crate_root, "{name}'s def map root is its root file");
    }
    let lib = crate_named(&db, "lib");
    let deep = keyed_files(raw, lib).unwrap().iter().find(|it| &*it.path == "deep.rs");
    let deep = deep.expect("`deep.rs` is named by `lib`'s pages").file;
    assert!(!raw.has_file_text(deep), "premise: `deep.rs` is not in the database");

    // The def map: `lib`'s own modules, one written in the absent file.
    let f = keyed_f(&db);
    assert!(
        matches!(item_at(&db, lib, &["Shown"]), Some(crate::ModuleDefId::AdtId(_))),
        "`lib::Shown`"
    );
    // What `main`, resolved from its source, imports from it.
    let main = crate_named(&db, "main");
    assert_eq!(item_at(&db, main, &["f"]), Some(crate::ModuleDefId::FunctionId(f)), "`main`'s `f`");
    assert!(
        matches!(item_at(&db, main, &["Shown"]), Some(crate::ModuleDefId::AdtId(_))),
        "`main`'s `Shown`"
    );

    // A signature, served, as the crate built from source has it.
    let signature = crate::signatures::FunctionSignature::served(&db, f).expect("`f` is served");
    let fresh_f = keyed_f(&fresh);
    let fresh_signature = crate::signatures::FunctionSignature::of(&fresh, fresh_f);
    assert_eq!(signature.name, fresh_signature.name);
    assert_eq!(signature.params.len(), fresh_signature.params.len());
    assert_eq!(signature.ret_type.is_some(), fresh_signature.ret_type.is_some());

    // An attribute: `core`'s lang item, read off its image.
    let core = crate_named(&db, "core");
    let sized = match item_at(&db, core, &["Sized"]) {
        Some(crate::ModuleDefId::TraitId(it)) => it,
        other => panic!("`core::Sized` is not a trait: {other:?}"),
    };
    // Premise: built from source, `core` collects lang items (under `#![feature(lang_items)]`,
    // without which a crate collects none and its record holds none).
    let fresh_core = crate_named(&fresh, "core");
    assert!(crate::lang_item::crate_lang_items(&fresh, fresh_core).is_some(), "premise: fresh `core`");
    let lang_items = crate::lang_item::crate_lang_items(&db, core).expect("`core` has lang items");
    assert_eq!(lang_items.Sized, Some(sized), "`#[lang = \"sized\"]` of a keyed `core`");
    assert_eq!(frozen::differences(), Vec::<String>::new());

    // The planted control: the same records given to be checked against the hollow files.
    let mut checked = hollow_db();
    let given = keyed_images(&source, &checked, Global);
    let mut edit = checked.edit();
    for (krate, krate_id, image, _) in given {
        give_image(&mut edit, krate, krate_id, image);
    }
    _ = edit.commit();
    let lib = crate_named(&checked, "lib");
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        _ = crate::nameres::crate_def_map(&checked, lib);
    }));
    let Err(payload) = caught else {
        panic!("`lib`'s pages were read as given against files they were not made from")
    };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("the refusal unwound with another payload than `frozen::Unreadable`")
    };
    assert_eq!((stop.krate, stop.kind, stop.name.as_deref()), (lib, "def map pages", Some("lib")));
}

/// A library with an `include!` at its root, as `core`'s `include!("primitive_docs.rs")` is.
const INCLUDE_AT_ROOT: &str = r#"
//- /lib/lib.rs crate:lib new_source_root:library
#[rustc_builtin_macro]
macro_rules! include { () => {} }
include!("inc.rs");
pub struct Own;
//- /lib/inc.rs
pub struct Included;
"#;

/// [`INCLUDE_AT_ROOT`]'s crate as a body snapshot has it before the crate is given: its root
/// alone, with the text its pages were written from (as `blaze_command::snapshot::open` puts a
/// given crate's root with its gathered text), and `inc.rs` neither a file of the database nor
/// on disk (the fixture's paths are not real), so nothing but the crate's own pages can name it.
const INCLUDE_AT_ROOT_HOLLOW: &str = r#"
//- /lib/lib.rs crate:lib new_source_root:library
#[rustc_builtin_macro]
macro_rules! include { () => {} }
include!("inc.rs");
pub struct Own;
"#;

/// [`INCLUDE_AT_ROOT`]'s `lib` given by its address ([`give_keyed`]) in a database that has its
/// root alone, as `blaze_command::snapshot::open` gives a body task its own crate; then its root
/// module's scope read back from its pages, and what that read unwound with, if anything.
fn include_read_back(unplaced: bool) -> (TestDB, Result<(), Box<dyn std::any::Any + Send>>) {
    let source = fixture::db(INCLUDE_AT_ROOT).into_db();
    let krate = crate_named(&source, "lib");
    let parts = image_parts(&source, krate).unwrap().parts;
    let pages = frozen::pages::encode(&source, krate).unwrap();
    let key = ContentId::of("hir-def test include at root", b"lib");
    let (holder, image_at, pages_at) = record_of(32, key, &parts, &pages);
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let paged = Paged {
        key: ContentId::of("hir-def test include at root pages", b"lib"),
        source: Arc::new(Section { holder: holder.clone(), range: pages_at, reads }),
    };
    let image = Image::in_place(key, key, holder, image_at, Mode::Serve, Some(paged)).unwrap();

    let mut db = fixture::db(INCLUDE_AT_ROOT_HOLLOW).into_db();
    let lib = crate_named(&db, "lib");
    let krate_id = stable_id(&db, lib);
    frozen::pages::UNPLACED.with(|it| it.set(unplaced));
    let mut edit = db.edit();
    let given = give_keyed(&mut edit, lib, krate_id, image, read_at(key));
    frozen::pages::UNPLACED.with(|it| it.set(false));
    given.unwrap();
    _ = edit.commit();
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let lib = crate_named(&db, "lib");
        let def_map = crate::nameres::crate_def_map(&db, lib);
        _ = def_map.module(&db, def_map.root).scope(&db);
    }));
    (db, read)
}

/// The invariant: a crate given by its address whose root `include!`s a file reads its root
/// module's scope back from its own pages: the `include!` its pages name resolves, by the name
/// the pages give the file, to the file id the give made for that name (the file put in the
/// crate's source root at its path from the root's directory), never to a file read from disk,
/// and the item the included file declares is in the scope.
///
/// The control, the cause removed ([`frozen::pages::UNPLACED`]: the given files put in no
/// source root): the same read is refused, naming both files, by the words a body snapshot of
/// `core` was refused with (`std_census`'s read-back).
#[test]
fn a_crate_given_by_its_address_with_an_include_at_its_root_reads_its_root_scope_back() {
    let _turn = TURN.lock();
    let (db, read) = include_read_back(false);
    if let Err(payload) = read {
        panic!("the root scope did not read back: {}", crate::nameres::stopped(&*payload).unwrap_or_default());
    }
    let lib = crate_named(&db, "lib");
    assert!(
        matches!(item_at(&db, lib, &["Included"]), Some(crate::ModuleDefId::AdtId(_))),
        "`lib::Included`, declared by the included file"
    );
    assert!(matches!(item_at(&db, lib, &["Own"]), Some(crate::ModuleDefId::AdtId(_))), "`lib::Own`");

    let (_, read) = include_read_back(true);
    let Err(payload) = read else { panic!("the control read the include! back with its file put nowhere") };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("the control unwound with another payload than `frozen::Unreadable`")
    };
    assert!(
        stop.why.contains("an include! of another file than was written") && stop.why.contains("resolves to no file"),
        "{stop}"
    );
}

/// std's facade as the pinned library has it: `core` (here a `#![no_core]` stand-in with the
/// modules a signature names), `rustc_std_workspace_core` (library/rustc-std-workspace-core's
/// `lib.rs`: its attributes and `pub use core::*;`; its `use compiler_builtins as _;` is left
/// out, there being no such crate here), and a `#![no_std]` crate that names the facade `core`
/// and names `core::` paths and prelude names in type position, as the eight units refused a
/// signature on the real std do. `user` names the facade `core` by a dependency rename, as
/// panic_abort's manifest does (`core = { package = "rustc-std-workspace-core" }`), and
/// `user_extern` by `#[macro_use] extern crate .. as core`, as libc does.
///
/// The facade is a language crate (`lang`), as the command makes every toolchain package, so
/// its edge to `core` is its manifest's and not a sysroot one, and `#![no_core]` keeps `core`
/// in its extern prelude. `core`'s prelude is reached through its own glob
/// (`rust_2024` globs `v1`, as the real one does). `reexports` re-exports from `core` in each
/// of the other forms a name crosses a crate in: a named item, a module, `pub extern crate`,
/// and a `#[macro_export]` macro by name.
const FACADE: &str = r#"
//- /core/lib.rs crate:core new_source_root:library edition:2024
#![feature(no_core)]
#![no_core]
pub mod any { pub trait Any {} }
pub mod marker { pub trait Send {} pub struct PhantomData<T>(T); }
pub mod option { pub enum Option<T> { None, Some(T) } }
pub mod prelude {
    pub mod v1 {
        pub use crate::option::Option;
        pub use crate::option::Option::{None, Some};
        pub use crate::marker::Send;
    }
    pub mod rust_2024 { pub use super::v1::*; }
}
#[macro_export]
macro_rules! core_macro { () => {}; }

//- /facade/lib.rs crate:rustc_std_workspace_core lang deps:core new_source_root:library edition:2024
#![feature(no_core)]
#![no_core]

pub use core::*;

//- /reexports/lib.rs crate:reexports deps:core new_source_root:library edition:2024
#![no_std]
pub use core::marker::PhantomData;
pub use core::any;
pub extern crate core as reexported_core;
pub use core::core_macro;

//- /user/lib.rs crate:user deps:core=rustc_std_workspace_core,reexports new_source_root:local edition:2024
#![no_std]
use core::any::Any;
pub struct S { pub a: core::marker::PhantomData<u8>, pub b: Option<u8> }
pub fn f(_: &dyn Any) -> Option<u8> { None }

//- /user_extern/lib.rs crate:user_extern deps:rustc_std_workspace_core,reexports new_source_root:local edition:2024
#![no_std]
#[macro_use]
extern crate rustc_std_workspace_core as core;
use core::any::Any;
pub struct S { pub a: core::marker::PhantomData<u8>, pub b: Option<u8> }
pub fn f(_: &dyn Any) -> Option<u8> { None }
"#;

/// [`FACADE`]'s database, with `core`, the facade and `reexports` given by their addresses
/// ([`give_keyed`], pages written from a first database and read in this one, as the command
/// gives every dependency of a unit) when `given`, else all from source.
fn facade_db(given: bool) -> TestDB {
    let mut db = fixture::db(FACADE).into_db();
    if !given {
        return db;
    }
    let source = fixture::db(FACADE).into_db();
    let mut opened = Vec::new();
    for name in ["core", "rustc_std_workspace_core", "reexports"] {
        let krate = crate_named(&source, name);
        let parts = image_parts(&source, krate).unwrap().parts;
        let pages = frozen::pages::encode(&source, krate).unwrap();
        let key = ContentId::of("hir-def test facade", name.as_bytes());
        let (holder, image_at, pages_at) = record_of(32, key, &parts, &pages);
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let paged = Paged {
            key: ContentId::of("hir-def test facade pages", name.as_bytes()),
            source: Arc::new(Section { holder: holder.clone(), range: pages_at, reads }),
        };
        let image = Image::in_place(key, key, holder, image_at, Mode::Serve, Some(paged)).unwrap();
        let krate = crate_named(&db, name);
        opened.push((name, krate, stable_id(&db, krate), image, key));
    }
    let mut edit = db.edit();
    for (name, krate, krate_id, image, key) in opened {
        if let Err(why) = give_keyed(&mut edit, krate, krate_id, image, read_at(key)) {
            panic!("`{name}` is not given by its address: {why:?}");
        }
    }
    _ = edit.commit();
    db
}

/// What [`FACADE`]'s names resolve to in `db`: evidence lines, which say what the facade holds
/// and fail nothing, and the checks that failed, each with what it found. All are run; none
/// stops the rest.
///
/// Evidence: (1) the facade's root scope as the page writer reads it (`ModuleData::scope`);
/// (2) the facade's extern prelude, whose `core` its `pub use core::*;` needs; (3) from inside
/// the facade, `crate::any` (`rustc_std_workspace_core::any`) and `core::any` by path
/// resolution.
///
/// Checks, all from `user`: (4) `core::marker::PhantomData` is a struct (through the facade's
/// glob); (5) `user`'s prelude is `core::prelude::rust_2024` and `Option` is an enum; (6) `Any`
/// (`use core::any::Any`) is a trait. These are the names `S`'s fields and `f`'s signature are
/// written with: a type with an error there is one of them not resolving (lowering is hir-ty's,
/// not asserted here). The other forms a name crosses a crate in: (7) a named re-export,
/// `reexports::PhantomData`; (8) a module re-export, `reexports::any::Any`; (9) `pub extern
/// crate`, `reexports::reexported_core::option::Option`; (10) a macro re-export by name,
/// `reexports::core_macro`; (11) a `#[macro_export]` macro through the facade, by path
/// (`core::core_macro`) and by `#[macro_use]` (in `user`'s macro-use prelude); (12) the prelude
/// module's own glob, `Some` in the value namespace (`rust_2024`'s `pub use super::v1::*;`).
fn facade_probe(db: &TestDB) -> (Vec<String>, Vec<String>) {
    use crate::{
        ModuleDefId,
        item_scope::BuiltinShadowMode,
        nameres::{crate_def_map, crate_local_def_map},
    };
    use hir_expand::mod_path::{ModPath, PathKind};
    let dep = crate_named(db, "core");
    let facade = crate_named(db, "rustc_std_workspace_core");
    let user = crate_named(db, "user");

    let mut evidence = Vec::new();
    let facade_map = crate_def_map(db, facade);
    let facade_local = crate_local_def_map(db, facade).local(db);
    let facade_root = facade_map.root_module_id();
    match facade_map.modules(db).find(|&(id, _)| id == facade_root) {
        Some((_, data)) => {
            let mut names: Vec<&str> = data.scope(db).entries().map(|(name, _)| name.as_str()).collect();
            names.sort_unstable();
            evidence.push(format!("(1) the facade's root scope, as the page writer reads it, holds {names:?}"));
        }
        None => evidence.push("(1) the facade's def map has no root module in its table".to_owned()),
    }
    let mut extern_prelude: Vec<String> = facade_local
        .extern_prelude()
        .map(|(name, (module, _))| format!("`{}` = crate `{}`", name.as_str(), module.krate(db).named(db)))
        .collect();
    extern_prelude.sort_unstable();
    evidence.push(format!("(2) the facade's extern prelude holds {extern_prelude:?}"));
    let in_facade = |kind: PathKind, path: &[&str]| {
        let path = ModPath::from_segments(kind, path.iter().map(|it| named(it)));
        let found = facade_map.resolve_path(facade_local, db, facade_root, &path, BuiltinShadowMode::Other, None);
        match found.0.take_types() {
            Some(ModuleDefId::ModuleId(module)) => {
                format!("{module:?} of crate `{}`", module.krate(db).named(db))
            }
            other => format!("{other:?}"),
        }
    };
    evidence.push(format!(
        "(3) from inside the facade, `crate::any` (`rustc_std_workspace_core::any`) resolves to {} and \
         `core::any` to {}",
        in_facade(PathKind::Crate, &["any"]),
        in_facade(PathKind::Plain, &["core", "any"]),
    ));

    let mut failures = Vec::new();
    let mut check = |step: &str, holds: bool, found: String| {
        if !holds {
            failures.push(format!("{step}: found {found}"));
        }
    };
    let user_map = crate_def_map(db, user);
    let user_local = crate_local_def_map(db, user).local(db);
    let user_root = user_map.root_module_id();
    let resolve = |path: &[&str]| {
        let path = ModPath::from_segments(PathKind::Plain, path.iter().map(|it| named(it)));
        user_map.resolve_path(user_local, db, user_root, &path, BuiltinShadowMode::Other, None).0
    };

    let it = resolve(&["core", "marker", "PhantomData"]).take_types();
    check(
        "(4) `core::marker::PhantomData` is a struct",
        matches!(it, Some(ModuleDefId::AdtId(_))),
        format!("{it:?}"),
    );
    let rust_2024 = item_at(db, dep, &["prelude", "rust_2024"]);
    let prelude = user_map.prelude().map(|(module, _)| module);
    check(
        "(5) `user`'s prelude is `core::prelude::rust_2024`",
        prelude.is_some() && prelude.map(ModuleDefId::ModuleId) == rust_2024,
        format!(
            "{prelude:?} of crate {:?}, `core::prelude::rust_2024` being {rust_2024:?}",
            prelude.map(|module| module.krate(db).named(db).to_string())
        ),
    );
    let it = resolve(&["Option"]).take_types();
    check("(5) the prelude's `Option` is an enum", matches!(it, Some(ModuleDefId::AdtId(_))), format!("{it:?}"));
    let it = resolve(&["Any"]).take_types();
    check(
        "(6) `Any` (`use core::any::Any`) is a trait",
        matches!(it, Some(ModuleDefId::TraitId(_))),
        format!("{it:?}"),
    );
    let it = resolve(&["reexports", "PhantomData"]).take_types();
    check(
        "(7) the named re-export `reexports::PhantomData` is a struct",
        matches!(it, Some(ModuleDefId::AdtId(_))),
        format!("{it:?}"),
    );
    let it = resolve(&["reexports", "any", "Any"]).take_types();
    check(
        "(8) through the module re-export, `reexports::any::Any` is a trait",
        matches!(it, Some(ModuleDefId::TraitId(_))),
        format!("{it:?}"),
    );
    let it = resolve(&["reexports", "reexported_core", "option", "Option"]).take_types();
    check(
        "(9) through `pub extern crate`, `reexports::reexported_core::option::Option` is an enum",
        matches!(it, Some(ModuleDefId::AdtId(_))),
        format!("{it:?}"),
    );
    let it = resolve(&["reexports", "core_macro"]).take_macros();
    check("(10) the macro re-export `reexports::core_macro` is a macro", it.is_some(), format!("{it:?}"));
    let it = resolve(&["core", "core_macro"]).take_macros();
    check(
        "(11) `core::core_macro`, `#[macro_export]` in `core`, is a macro through the facade's glob",
        it.is_some(),
        format!("{it:?}"),
    );
    let it = resolve(&["Some"]).take_values();
    check(
        "(12) through the prelude's own glob, `Some` is a variant",
        matches!(it, Some(ModuleDefId::EnumVariantId(_))),
        format!("{it:?}"),
    );

    // `user_extern`, libc's form: the facade named `core` by `#[macro_use] extern crate`.
    let extern_user = crate_named(db, "user_extern");
    let extern_map = crate_def_map(db, extern_user);
    let extern_local = crate_local_def_map(db, extern_user).local(db);
    let extern_root = extern_map.root_module_id();
    let resolve = |path: &[&str]| {
        let path = ModPath::from_segments(PathKind::Plain, path.iter().map(|it| named(it)));
        extern_map.resolve_path(extern_local, db, extern_root, &path, BuiltinShadowMode::Other, None).0
    };
    let it = resolve(&["core", "marker", "PhantomData"]).take_types();
    check(
        "(4) in `user_extern`, `core::marker::PhantomData` is a struct",
        matches!(it, Some(ModuleDefId::AdtId(_))),
        format!("{it:?}"),
    );
    let it = resolve(&["Any"]).take_types();
    check("(6) in `user_extern`, `Any` is a trait", matches!(it, Some(ModuleDefId::TraitId(_))), format!("{it:?}"));
    let mut macro_use: Vec<&str> = extern_map.macro_use_prelude().keys().map(|name| name.as_str()).collect();
    macro_use.sort_unstable();
    check(
        "(11) `#[macro_use] extern crate rustc_std_workspace_core` puts `core_macro` in `user_extern`'s macro-use prelude",
        macro_use.contains(&"core_macro"),
        format!("{macro_use:?}"),
    );
    // Its prelude: rustc finds `core` for the injected `extern crate core;` by the library
    // search path (a transitive dependency); name resolution here looks for a dependency named
    // `core` and has none. Evidence, not a check, until that is settled.
    evidence.push(format!(
        "(5) `user_extern`'s prelude is {:?}",
        extern_map.prelude().map(|(module, _)| (module, module.krate(db).named(db).to_string()))
    ));
    (evidence, failures)
}

/// The invariant: a crate whose `core` is std's facade resolves the names its signatures are
/// written with, `core::` paths and prelude names alike, when `core` and the facade are given
/// by their addresses as when they are built from source. Eight units of the real std were
/// refused a signature for a type with an error, each naming a type reached through the
/// facade. The same holds of each other form a name crosses a crate in. Every check is run and
/// every failure said, with the evidence of what the facade holds ([`facade_probe`]).
///
/// The control: the same checks with nothing given, which must pass, so a failure given is the
/// give's and not the fixture's.
#[test]
fn a_crate_naming_core_through_a_facade_given_by_its_address_resolves_its_signatures_names() {
    let _turn = TURN.lock();
    // Both halves run and are said whatever the other found; the test fails on either.
    let mut said = Vec::new();
    for (given, half) in [(false, "the control, nothing given"), (true, "core, the facade and `reexports` given by their addresses")] {
        let (evidence, failures) = facade_probe(&facade_db(given));
        let evidence = evidence.join("\n    ");
        eprintln!("{half}:\n    {evidence}");
        if !failures.is_empty() {
            said.push(format!("{half} fails:\n    {}\n  evidence:\n    {evidence}", failures.join("\n    ")));
        }
    }
    assert!(said.is_empty(), "{}", said.join("\n"));
}

/// The invariant: a body of a crate given by its address, demanded while its file is not in
/// the database, is a typed refusal ([`frozen::Unreadable`], kind
/// [`frozen::pages::KEYED_FILE`]) naming the crate and the file, before anything parses. (A
/// file whose text differs from the one the pages were made from by what the key leaves out
/// lowers from today's text: [`a_crate_given_by_its_pages_reads_todays_text_under_every_edit_its_key_leaves_out`];
/// one whose text lacks an item the pages name is refused there.)
///
/// The control, the cause removed: with the crate's files set to the texts its pages were made
/// from, on the file ids it was given ([`keyed_files`]), the same body lowers.
#[test]
fn a_demanded_body_of_a_crate_given_by_its_address_whose_file_is_absent_is_refused_naming_it() {
    let _turn = TURN.lock();
    let fixture = fixture::db(KEYED_SOURCE);
    let (lib_root, deep_file) = (fixture.file(1), fixture.file(2));
    let source = fixture.into_db();
    let text_of = |file: FileId| source.file_text(file).text(&source).to_owned();
    let (lib_text, deep_text) = (text_of(lib_root.file_id(&source)), text_of(deep_file.file_id(&source)));

    let mut db = hollow_db();
    let given = keyed_images(&source, &db, Global);
    let mut edit = db.edit();
    for (krate, krate_id, image, at) in given {
        give_keyed(&mut edit, krate, krate_id, image, read_at(at)).unwrap();
    }
    _ = edit.commit();

    let lowered = |db: &TestDB| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let f = keyed_f(db);
            _ = crate::expr_store::Body::of(db, f.into());
        }))
    };
    let refused = |db: &TestDB, what: &str| {
        let Err(payload) = lowered(db) else {
            panic!("a body of `lib` lowered with `deep.rs` {what}")
        };
        let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
            panic!("the refusal unwound with another payload than `frozen::Unreadable`")
        };
        let lib = crate_named(db, "lib");
        assert_eq!(
            (stop.krate, stop.kind, stop.name.as_deref()),
            (lib, frozen::pages::KEYED_FILE, Some("lib"))
        );
        assert!(stop.why.contains("deep.rs"), "{}", stop.why);
        stop.why.clone()
    };

    let why = refused(&db, "absent");
    assert!(why.contains("is not in this snapshot"), "{why}");

    let files = |db: &TestDB| {
        let raw: &Db = db;
        let lib = crate_named(db, "lib");
        keyed_files(raw, lib).unwrap().iter().map(|it| (it.is_root(), it.path.clone(), it.file)).collect::<Vec<_, Global>>()
    };

    // The control: every file of `lib` as its pages were made from.
    for (is_root, path, file) in files(&db) {
        let text = match (is_root, &*path) {
            (true, _) => &lib_text,
            (false, "deep.rs") => &deep_text,
            (false, other) => panic!("`lib`'s pages name a file the fixture has not: {other}"),
        };
        _ = db.set_file_text(file, text);
    }
    if let Err(payload) = lowered(&db) {
        let why = frozen::Unreadable::of_payload(&*payload).map(|it| it.why.clone());
        panic!("with its files as made, `f`'s body did not lower: {why:?}");
    }
}

/// The surface key of [`KEYED_SOURCE`]'s `lib` whose root is `lib` and whose `deep.rs` is
/// `deep`, as the command keys a unit (`blaze_unitkey::unit::key`): what its pages are found
/// by. Tool, settings and unit id are the test's own; only the texts move between calls.
fn lib_surface(lib: &str, deep: &str) -> blaze_keyed::kind::Key<blaze_keyed::kind::Surface> {
    use blaze_keyed::kind::{Namespace, Settings, ToolId};
    use blaze_unitkey::{
        image::{self, Reading},
        unit::{self, Unit, UnitFile},
    };
    let tool = ToolId { tool: ContentId([1; 32]), target: ContentId([4; 32]), namespace: Namespace::PUBLIC };
    let reading = Reading { edition: syntax::Edition::Edition2024, extensions: syntax::Extensions::NONE };
    let images = [lib, deep].map(|text| image::image(&tool, reading, text.as_bytes(), Global).expect("the file images"));
    let aligned = [Aligned::copy_from(&images[0].record), Aligned::copy_from(&images[1].record)];
    let files = [
        UnitFile { key: images[0].key, image: &aligned[0], module: b"" },
        UnitFile { key: images[1].key, image: &aligned[1], module: b"deep" },
    ];
    let options = cfg::CfgOptions::default();
    let unit = Unit {
        tool,
        settings: Settings(ContentId::of("hir-def test settings", &[])),
        unit: ContentId::of("hir-def test unit", b"lib"),
        cfg: &options,
        files: &files,
        answers: &[],
        names: &[ContentId([0; 32]); 2],
    };
    unit::key(&unit, Global).expect("`lib`'s keys").surface
}

/// The invariant: a crate given by its pages, which were found by its surface key, reads
/// today's text of its files under every edit that key leaves out: (a) a body edited, (b) a
/// comment appended, (c) a comment inserted above every item (each later item's offset
/// shifts), (d) a body made longer above another item. In each the pages read, names resolve
/// (`lib::deep::deeper::f`, `lib::deep::old`), `f`'s body is lowered from today's text (a
/// binary operator in it exactly where the edit wrote one) and `old`'s, below the edit, lowers.
/// Before, the reader compared each file's whole content with the one the pages were made
/// from, and every edit to a file refused every body of its crate.
///
/// Controls: (e) a signature changed and (f) an item added move the surface key, so the old
/// pages are never given for them (asserted on the key: nothing is given). (g), planted: the
/// pages given with a `deep.rs` whose non-body text differs (`old` renamed) under the same
/// key, a key violated, is the typed refusal ([`frozen::Unreadable`], kind
/// [`frozen::pages::KEYED_FILE`]) naming the crate and the item, never a lowering of another
/// item or an engine panic.
#[test]
fn a_crate_given_by_its_pages_reads_todays_text_under_every_edit_its_key_leaves_out() {
    use crate::hir::Expr;
    let _turn = TURN.lock();
    let fixture = fixture::db(KEYED_SOURCE);
    let (lib_root, deep_file) = (fixture.file(1), fixture.file(2));
    let source = fixture.into_db();
    let text_of = |file: FileId| source.file_text(file).text(&source).to_owned();
    let (lib_text, deep_text) = (text_of(lib_root.file_id(&source)), text_of(deep_file.file_id(&source)));
    assert!(deep_text.contains("pub fn f(x: u8) -> u8 { x }"), "premise: `f`'s body is `{{ x }}`");
    assert!(deep_text.contains("pub fn old() {}"), "premise: `old` is below `f`");
    let base = lib_surface(&lib_text, &deep_text);

    // `lib` given by its pages with its files' texts set: the root as made, `deep.rs` as `deep`.
    let given = |deep: &str| {
        let mut db = given_hollow_db(&source);
        let files: Vec<(bool, Box<str, Global>, FileId), Global> = {
            let raw: &Db = &db;
            let lib = crate_named(&db, "lib");
            keyed_files(raw, lib).unwrap().iter().map(|it| (it.is_root(), it.path.clone(), it.file)).collect()
        };
        for (is_root, path, file) in files {
            let text = match (is_root, &*path) {
                (true, _) => &*lib_text,
                (false, "deep.rs") => deep,
                (false, other) => panic!("`lib`'s pages name a file the fixture has not: {other}"),
            };
            _ = db.set_file_text(file, text);
        }
        db
    };
    // Whether `f`'s body has a binary operator, after `old`'s body lowered too.
    let lowered = |db: &TestDB| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let lib = crate_named(db, "lib");
            let f = keyed_f(db);
            let old = match item_at(db, lib, &["deep", "old"]) {
                Some(crate::ModuleDefId::FunctionId(it)) => it,
                other => panic!("`lib::deep::old` is not a function: {other:?}"),
            };
            let operator =
                crate::expr_store::Body::of(db, f.into()).exprs().any(|(_, it)| matches!(it, Expr::BinaryOp { .. }));
            _ = crate::expr_store::Body::of(db, old.into());
            operator
        }))
    };
    let shown = |payload: &(dyn std::any::Any + Send)| {
        frozen::Unreadable::of_payload(payload).map_or_else(|| "a panic of no `Unreadable`".to_owned(), |it| it.to_string())
    };

    let edits = [
        ("(a) a body edited", deep_text.replace("{ x }", "{ x + 1 }"), true),
        ("(b) a comment appended", format!("{deep_text}\n// appended\n"), false),
        ("(c) a comment inserted above every item", format!("// above\n// and again\n{deep_text}"), false),
        ("(d) a body made longer above another item", deep_text.replace("{ x }", "{ let y = x; let z = y; z + 0 }"), true),
    ];
    for (what, deep, operator) in &edits {
        assert_ne!(deep.as_str(), &*deep_text, "{what}: premise: the text moved");
        assert_eq!(lib_surface(&lib_text, deep), base, "{what}: premise: the surface key leaves it out");
        match lowered(&given(deep)) {
            Ok(found) => assert_eq!(found, *operator, "{what}: `f`'s body is not today's text"),
            Err(payload) => panic!("{what}: a body of `lib` did not lower: {}", shown(&*payload)),
        }
    }

    // (e), (f): the key moves, so the pages made from the old text are never given.
    let signature = deep_text.replace("pub fn f(x: u8) -> u8", "pub fn f(x: u16) -> u16");
    assert_ne!(signature.as_str(), &*deep_text, "premise: (e) moved the text");
    assert_ne!(lib_surface(&lib_text, &signature), base, "(e) a signature changed keeps the surface key");
    let added = format!("{deep_text}\npub fn added() {{}}\n");
    assert_ne!(lib_surface(&lib_text, &added), base, "(f) an item added keeps the surface key");

    // (g), planted: the old pages given with `old` renamed, as if under the same key.
    let renamed = deep_text.replace("pub fn old() {}", "pub fn older() {}");
    assert_ne!(lib_surface(&lib_text, &renamed), base, "premise: (g)'s text is another key's");
    let db = given(&renamed);
    let Err(payload) = lowered(&db) else {
        panic!("(g) `old`'s body lowered from a text without `old`")
    };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("(g) the key violated unwound with another payload than `frozen::Unreadable`")
    };
    let lib = crate_named(&db, "lib");
    assert_eq!((stop.krate, stop.kind, stop.name.as_deref()), (lib, frozen::pages::KEYED_FILE, Some("lib")));
    assert!(stop.why.contains("deep.rs") && stop.why.contains("has no item"), "{}", stop.why);
}

/// The invariant: an attribute value of an item of a crate given by its address that its
/// records do not hold (the aliases `f`'s `#[doc(alias)]` names: its flags are in its image,
/// and this database keeps no entries, where the values are kept) is read off its file, and with that file's text absent the query is a typed
/// refusal ([`frozen::Unreadable`], kind [`frozen::pages::KEYED_FILE`]) naming the crate and
/// the file, before any tree of the file is read (`attrs`' and `item_tree`'s guard). Its flags
/// (`AttrFlags::query`) are served from the record with no text, and equal the fresh build's.
///
/// The planted controls: without the guard the same read is base-db's own panic on a file
/// with no text, which names no crate and unwinds with no `Unreadable` (`db.file_text` of the
/// file, asked directly); and with the file's text set as the pages were made from, the
/// aliases are read and are the ones the crate built from source has.
#[test]
fn an_attribute_of_a_crate_given_by_its_address_whose_file_is_absent_is_refused_naming_it() {
    let _turn = TURN.lock();
    let fixture = fixture::db(KEYED_SOURCE);
    let (lib_root, deep_file) = (fixture.file(1), fixture.file(2));
    let source = fixture.into_db();
    let text_of = |file: FileId| source.file_text(file).text(&source).to_owned();
    let (lib_text, deep_text) = (text_of(lib_root.file_id(&source)), text_of(deep_file.file_id(&source)));
    let fresh = fixture::db(KEYED_SOURCE).into_db();
    let flags_of = |db: &TestDB, f: crate::FunctionId| {
        crate::attrs::AttrFlags::query(db, crate::AttrDefId::FunctionId(f))
    };
    let fresh_flags = flags_of(&fresh, keyed_f(&fresh));
    assert!(
        fresh_flags.contains(crate::attrs::AttrFlags::HAS_DOC_ALIASES),
        "premise: `f` has a `#[doc(alias)]` its flags hold"
    );

    let mut db = hollow_db();
    let given = keyed_images(&source, &db, Global);
    let mut edit = db.edit();
    for (krate, krate_id, image, at) in given {
        give_keyed(&mut edit, krate, krate_id, image, read_at(at)).unwrap();
    }
    _ = edit.commit();
    let file_of = |db: &TestDB, path: &str| {
        let raw: &Db = db;
        let files = keyed_files(raw, crate_named(db, "lib")).unwrap();
        files.iter().find(|it| &*it.path == path || (path == "root" && it.is_root())).unwrap().file
    };

    let aliases_of = |db: &TestDB| {
        let f = keyed_f(db);
        let owner = either::Either::Left(crate::AttrDefId::FunctionId(f));
        flags_of(db, f).doc_aliases(db, owner).to_vec()
    };
    let fresh_aliases = aliases_of(&fresh);
    assert!(!fresh_aliases.is_empty(), "premise: `f` names an alias");
    // Its flags: served from the record, no text read.
    assert_eq!(flags_of(&db, keyed_f(&db)), fresh_flags, "`f`'s flags, served with `deep.rs` absent");
    let asked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        _ = aliases_of(&db);
    }));
    let Err(payload) = asked else {
        panic!("`f`'s aliases were read with `deep.rs` absent")
    };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("the refusal unwound with another payload than `frozen::Unreadable`")
    };
    let lib = crate_named(&db, "lib");
    assert_eq!((stop.krate, stop.kind, stop.name.as_deref()), (lib, frozen::pages::KEYED_FILE, Some("lib")));
    assert!(stop.why.contains("deep.rs") && stop.why.contains("is not in this snapshot"), "{}", stop.why);

    // The planted control: the read the guard stands before, unguarded, is base-db's panic.
    let deep = file_of(&db, "deep.rs");
    let unguarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        _ = db.file_text(deep);
    }));
    let Err(payload) = unguarded else {
        panic!("premise: base-db reads a text the database does not hold")
    };
    assert!(frozen::Unreadable::of_payload(&*payload).is_none(), "the unguarded read is no typed refusal");

    // The control, the cause removed: `lib`'s files as its pages were made from.
    _ = db.set_file_text(file_of(&db, "root"), &lib_text);
    _ = db.set_file_text(file_of(&db, "deep.rs"), &deep_text);
    assert_eq!(aliases_of(&db), fresh_aliases, "`f`'s aliases, with its file as made");
}

/// [`hollow_db`] with `core` and `lib` given by their addresses ([`give_keyed`]) from
/// `source`'s records: no file of either has text here.
fn given_hollow_db(source: &TestDB) -> TestDB {
    let mut db = hollow_db();
    let given = keyed_images(source, &db, Global);
    let mut edit = db.edit();
    for (krate, krate_id, image, at) in given {
        give_keyed(&mut edit, krate, krate_id, image, read_at(at)).unwrap();
    }
    _ = edit.commit();
    db
}

/// The invariant: what a dependent's check reads off a crate's attributes beside its
/// signatures is read, of a crate given by its address, from the entries its store kept
/// (`facts::keep_all`), with no text of its files, and equals the fresh build's: a module's
/// flags (`deeper`, an inline module of `deep.rs`, whose flags stability's inheritance reads),
/// a definition's own deprecation (`old`'s, `attrs::stability`'s fact with its parse context
/// in its name) and the flags of a definition's generic parameters (`Boxed`'s
/// `#[may_dangle] T`, `attrs::generic_params_attr_flags`'s header fact). `deep.rs` is not in the
/// database; its index and the entries are found by the content `lib`'s pages name.
///
/// The planted control, the cause removed: the same asks with every fact read live
/// (`facts::LIVE`: no entry looked up, each made off its syntax) are a typed refusal naming
/// `lib` and `deep.rs`, before any tree of it is read.
///
/// What this cannot show, and `facts::tests` does: a fact of a crate's root file (the root
/// module's flags, the crate's blaze edition). [`hollow_db`]'s roots hold bytes of another
/// content than their pages name, so a root's entries are not found there (a miss, refused,
/// never another root's entry served).
#[test]
fn the_attribute_facts_of_a_crate_given_by_its_address_are_read_from_its_entries() {
    use crate::{attrs::AttrFlags, nameres::frozen::entries};

    let _turn = TURN.lock();
    let store = entries::Memory::new();
    let mut source = fixture::db(KEYED_SOURCE).into_db();
    // clone: a test's handle of its store
    entries::set_for_test(&mut source, store.clone());
    for name in ["core", "lib"] {
        super::facts::keep_all(&source, crate_named(&source, name));
    }
    let fresh = fixture::db(KEYED_SOURCE).into_db();

    let asked = |db: &TestDB| -> Vec<String, Global> {
        let lib = crate_named(db, "lib");
        let deeper = match item_at(db, lib, &["deep", "deeper"]) {
            Some(crate::ModuleDefId::ModuleId(it)) => it,
            other => panic!("`lib::deep::deeper` is not a module: {other:?}"),
        };
        let old = match item_at(db, lib, &["deep", "old"]) {
            Some(crate::ModuleDefId::FunctionId(it)) => it,
            other => panic!("`lib::deep::old` is not a function: {other:?}"),
        };
        let boxed = match item_at(db, lib, &["deep", "Boxed"]) {
            Some(crate::ModuleDefId::AdtId(it)) => it,
            other => panic!("`lib::deep::Boxed` is not a struct: {other:?}"),
        };
        let deeper = AttrFlags::query(db, crate::AttrDefId::ModuleId(deeper));
        let old = crate::attrs::stability::own_deprecation(db, crate::AttrDefId::FunctionId(old).into());
        let (lifetimes, types) = AttrFlags::query_generic_params(db, boxed.into());
        let mut said = Vec::new_in(Global);
        said.push(format!("deeper {deeper:?}"));
        said.push(format!("old {old:?}"));
        said.push(format!("lifetimes {:?}", lifetimes.values().collect::<Vec<_, Global>>()));
        said.push(format!("types {:?}", types.values().collect::<Vec<_, Global>>()));
        said
    };
    let want = asked(&fresh);
    // Premises: each fact holds something, so an empty answer cannot pass for it.
    assert!(want[0].contains("IS_DOC_HIDDEN"), "premise: `deeper` is `#[doc(hidden)]`: {want:?}");
    assert!(want[1].contains("use f"), "premise: `old` is deprecated: {want:?}");
    assert!(want[3].contains("MAY_DANGLE"), "premise: `Boxed`'s `T` is `#[may_dangle]`: {want:?}");

    let mut db = given_hollow_db(&source);
    // clone: a test's handle of its store
    entries::set_for_test(&mut db, store.clone());
    let raw: &Db = &db;
    let lib = keyed_files(raw, crate_named(&db, "lib")).unwrap();
    let deep = lib.iter().find(|it| &*it.path == "deep.rs").expect("`deep.rs` is named").file;
    assert!(!raw.has_file_text(deep), "premise: `deep.rs` is not in the database");
    assert_eq!(asked(&db), want, "the facts of a keyed `lib`, read from its entries");

    // The planted control: the same asks, every fact made off its syntax.
    let mut live = given_hollow_db(&source);
    // clone: a test's handle of its store
    entries::set_for_test(&mut live, store.clone());
    super::facts::LIVE.set(true);
    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        _ = asked(&live);
    }));
    super::facts::LIVE.set(false);
    let Err(payload) = made else {
        panic!("the facts of a keyed `lib` were made with `deep.rs` absent")
    };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("the refusal unwound with another payload than `frozen::Unreadable`")
    };
    let lib = crate_named(&live, "lib");
    assert_eq!((stop.krate, stop.kind, stop.name.as_deref()), (lib, frozen::pages::KEYED_FILE, Some("lib")));
    assert!(stop.why.contains("deep.rs"), "{}", stop.why);
}

/// `lib::deep` of `db`.
fn deep_module(db: &TestDB) -> crate::ModuleId {
    match item_at(db, crate_named(db, "lib"), &["deep"]) {
        Some(crate::ModuleDefId::ModuleId(it)) => it,
        other => panic!("`lib::deep` is not a module: {other:?}"),
    }
}

/// The impls `lib::deep` declares, in its scope's order.
fn deep_impls<A: core::alloc::Allocator>(db: &TestDB, alloc: A) -> Vec<crate::ImplId, A> {
    let def_map = crate::nameres::crate_def_map(db, crate_named(db, "lib"));
    let mut out = Vec::new_in(alloc);
    out.extend(def_map.module(db, deep_module(db)).scope(db).impls());
    out
}

/// `lib::deep::Count` of `db`.
fn deep_count(db: &TestDB) -> crate::TraitId {
    match item_at(db, crate_named(db, "lib"), &["deep", "Count"]) {
        Some(crate::ModuleDefId::TraitId(it)) => it,
        other => panic!("`lib::deep::Count` is not a trait: {other:?}"),
    }
}

/// `lib::deep::Held` of `db`.
fn deep_held(db: &TestDB) -> crate::StructId {
    match item_at(db, crate_named(db, "lib"), &["deep", "Held"]) {
        Some(crate::ModuleDefId::AdtId(crate::AdtId::StructId(it))) => it,
        other => panic!("`lib::deep::Held` is not a struct: {other:?}"),
    }
}

/// The kind of an assoc item, as a name a test compares across databases.
fn assoc_kind(item: crate::AssocItemId) -> &'static str {
    match item {
        crate::AssocItemId::FunctionId(_) => "fn",
        crate::AssocItemId::ConstId(_) => "const",
        crate::AssocItemId::TypeAliasId(_) => "type",
    }
}

/// Assoc items as `(name, kind)`, in their order: what two databases' ids are compared by.
fn assoc_named<'a, A: core::alloc::Allocator>(
    items: &'a [(hir_expand::name::Name, crate::AssocItemId)],
    alloc: A,
) -> Vec<(&'a str, &'static str), A> {
    let mut out = Vec::new_in(alloc);
    out.extend(items.iter().map(|(name, item)| (name.as_str(), assoc_kind(*item))));
    out
}

/// Asks `ask` of `db` (built by [`given_hollow_db`], a section of `lib`'s record left out) and
/// checks it is the typed refusal of a read of `deep.rs`'s text: [`frozen::Unreadable`] of kind
/// [`frozen::pages::KEYED_FILE`] naming `lib` and the file. A parse of a file with no text is
/// base-db's own panic, with no `Unreadable`, and fails here; so does an answer.
fn refused_naming_deep(db: &TestDB, what: &str, ask: impl FnOnce()) {
    let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(ask)) else {
        panic!("{what} answered with `deep.rs` absent and its section left out")
    };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("{what}: the refusal unwound with another payload than `frozen::Unreadable`")
    };
    let lib = crate_named(db, "lib");
    assert_eq!(
        (stop.krate, stop.kind, stop.name.as_deref()),
        (lib, frozen::pages::KEYED_FILE, Some("lib")),
        "{what}"
    );
    assert!(
        stop.why.contains("deep.rs") && stop.why.contains("is not in this snapshot"),
        "{what}: {}",
        stop.why
    );
}

/// The invariant (item B): of a crate given by its address with no text of any of its files
/// here ([`HOLLOW`]), the items of each impl (`ImplItems::of`) and trait
/// (`TraitItems::query_with_diagnostics`) are read from its record: the same names and kinds in
/// the same order as a fresh build from source has, with no macro call.
///
/// The planted control, the cause removed: with the record's assoc items left out
/// ([`signatures::left_out::ITEMS`], as an image written without them reads), the same
/// queries take the item's syntax and are the typed refusal naming `lib` and `deep.rs`
/// ([`refused_naming_deep`]), never a parse.
#[test]
fn the_items_of_an_impl_or_trait_of_a_crate_given_by_its_address_are_read_from_its_record() {
    use crate::nameres::assoc::{ImplItems, TraitItems};
    let _turn = TURN.lock();
    let source = fixture::db(KEYED_SOURCE).into_db();
    let fresh = fixture::db(KEYED_SOURCE).into_db();
    let db = given_hollow_db(&source);

    let (served, made) = (deep_impls(&db, Global), deep_impls(&fresh, Global));
    assert_eq!(made.len(), 2, "premise: `deep` declares an inherent impl and a trait impl");
    assert_eq!(served.len(), made.len(), "the served def map has `deep`'s impls");
    let mut lists = Vec::new_in(Global);
    for (&served, &made) in served.iter().zip(&made) {
        let (ours, theirs) = (&ImplItems::of(&db, served).0, &ImplItems::of(&fresh, made).0);
        assert_eq!(assoc_named(&ours.items, Global), assoc_named(&theirs.items, Global));
        assert!(ours.macro_calls.is_empty() && theirs.macro_calls.is_empty());
        lists.push(assoc_named(&theirs.items, Global));
    }
    let inherent = [("get", "fn"), ("shared", "fn"), ("own", "fn"), ("ONE", "const")];
    let of_trait = [("count", "fn"), ("Out", "type")];
    assert!(lists.iter().any(|it| *it == inherent), "premise: the inherent impl's items: {lists:?}");
    assert!(lists.iter().any(|it| *it == of_trait), "premise: the trait impl's items: {lists:?}");

    let ours = &TraitItems::query_with_diagnostics(&db, deep_count(&db)).0;
    let theirs = &TraitItems::query_with_diagnostics(&fresh, deep_count(&fresh)).0;
    assert_eq!(assoc_named(&ours.items, Global), assoc_named(&theirs.items, Global));
    assert_eq!(assoc_named(&theirs.items, Global), [("count", "fn"), ("Out", "type")], "premise");
    assert_eq!(frozen::differences(), Vec::<String>::new());

    // The planted control: the same records, their assoc items left out.
    let control = given_hollow_db(&source);
    let _left = super::signatures::left_out::leave_out(super::signatures::left_out::ITEMS);
    let first = deep_impls(&control, Global)[0];
    refused_naming_deep(&control, "`ImplItems::of`", || {
        _ = ImplItems::of(&control, first);
    });
    let count = deep_count(&control);
    refused_naming_deep(&control, "`TraitItems::query_with_diagnostics`", || {
        _ = TraitItems::query_with_diagnostics(&control, count);
    });
}

/// The invariant (item C): of a crate given by its address with no text of any of its files
/// here ([`HOLLOW`]), `AttrFlags::query` of each item its record holds flags of (a function, a
/// struct, a trait, an impl, an assoc fn, const and type) is read from its record, and is what
/// a fresh build from source answers, the flags of `#[inline]`, `#[must_use]`,
/// `#[doc(hidden)]` and `#[doc(alias)]` among them.
///
/// The planted control, the cause removed: with the record's flags left out
/// ([`signatures::left_out::FLAGS`]), the same query takes the item's attributes and is the
/// typed refusal naming `lib` and `deep.rs` ([`refused_naming_deep`]), never a parse.
#[test]
fn the_attribute_flags_of_a_crate_given_by_its_address_are_read_from_its_record() {
    use crate::{AttrDefId, attrs::AttrFlags, nameres::assoc::{ImplItems, TraitItems}};
    let _turn = TURN.lock();
    let source = fixture::db(KEYED_SOURCE).into_db();
    let fresh = fixture::db(KEYED_SOURCE).into_db();
    let db = given_hollow_db(&source);

    // Every owner of `deep` the record holds flags of, by a name both databases agree on.
    let owners = |db: &TestDB| {
        let mut out: Vec<(String, AttrDefId), Global> = Vec::new_in(Global);
        out.push(("f".to_owned(), AttrDefId::FunctionId(keyed_f(db))));
        out.push(("Held".to_owned(), AttrDefId::AdtId(crate::AdtId::StructId(deep_held(db)))));
        let count = deep_count(db);
        out.push(("Count".to_owned(), AttrDefId::TraitId(count)));
        for (name, item) in &TraitItems::query_with_diagnostics(db, count).0.items {
            out.push((format!("Count::{}", name.as_str()), AttrDefId::from(*item)));
        }
        for (at, &impl_) in deep_impls(db, Global).iter().enumerate() {
            out.push((format!("impl {at}"), AttrDefId::ImplId(impl_)));
            for (name, item) in &ImplItems::of(db, impl_).0.items {
                out.push((format!("impl {at}::{}", name.as_str()), AttrDefId::from(*item)));
            }
        }
        out
    };
    let flags = |db: &TestDB| {
        let mut out: Vec<(String, AttrFlags), Global> = Vec::new_in(Global);
        out.extend(owners(db).into_iter().map(|(name, owner)| (name, AttrFlags::query(db, owner))));
        out
    };
    let (ours, theirs) = (flags(&db), flags(&fresh));
    assert_eq!(ours, theirs);
    for (name, flag) in [
        ("f", AttrFlags::HAS_DOC_ALIASES),
        ("Held", AttrFlags::IS_MUST_USE),
        ("Count", AttrFlags::IS_MUST_USE),
        ("impl 0::get", AttrFlags::IS_INLINE),
        ("impl 0::ONE", AttrFlags::IS_DOC_HIDDEN),
    ] {
        let found = theirs.iter().find(|it| it.0 == name).map(|it| it.1);
        let found = found.unwrap_or_else(|| panic!("premise: `{name}` is an owner: {theirs:?}"));
        assert!(found.contains(flag), "premise: `{name}` has {flag:?}: {found:?}");
    }
    assert_eq!(frozen::differences(), Vec::<String>::new());

    // The planted control: the same records, their flags left out.
    let control = given_hollow_db(&source);
    let held = AttrDefId::AdtId(crate::AdtId::StructId(deep_held(&control)));
    let _left = super::signatures::left_out::leave_out(super::signatures::left_out::FLAGS);
    refused_naming_deep(&control, "`AttrFlags::query` of `Held`", || {
        _ = AttrFlags::query(&control, held);
    });
}

/// The invariant (item D): of a crate given by its address with no text of any of its files
/// here ([`HOLLOW`]), the visibility of each assoc item (`AssocItemId::assoc_visibility`: an
/// impl's item its own written one, a trait's item its trait's) is read from its record's assoc
/// items, and resolves to what a fresh build from source has: `pub`, `pub(crate)` and none
/// written among them.
///
/// The planted control, the cause removed: with the record's visibilities left out, its items
/// still read from it ([`signatures::left_out::VISIBILITIES`]), the same query takes the
/// item's syntax and is the typed refusal naming `lib` and `deep.rs` ([`refused_naming_deep`]),
/// never a parse.
#[test]
fn the_visibility_of_an_assoc_item_of_a_crate_given_by_its_address_is_read_from_its_record() {
    use crate::{AssocItemId, nameres::assoc::{ImplItems, TraitItems}, visibility::Visibility};
    let _turn = TURN.lock();
    let source = fixture::db(KEYED_SOURCE).into_db();
    let fresh = fixture::db(KEYED_SOURCE).into_db();
    let db = given_hollow_db(&source);

    let items = |db: &TestDB| {
        let mut out: Vec<(String, AssocItemId), Global> = Vec::new_in(Global);
        for (name, item) in &TraitItems::query_with_diagnostics(db, deep_count(db)).0.items {
            out.push((format!("Count::{}", name.as_str()), *item));
        }
        for (at, &impl_) in deep_impls(db, Global).iter().enumerate() {
            for (name, item) in &ImplItems::of(db, impl_).0.items {
                out.push((format!("impl {at}::{}", name.as_str()), *item));
            }
        }
        out
    };
    let seen = |db: &TestDB| {
        let here = deep_module(db);
        let root = crate::nameres::crate_def_map(db, crate_named(db, "lib")).root;
        let mut out: Vec<(String, &'static str), Global> = Vec::new_in(Global);
        out.extend(items(db).into_iter().map(|(name, item)| {
            let shown = match item.assoc_visibility(db) {
                Visibility::Public => "pub",
                Visibility::PubCrate(_) => "pub(crate)",
                Visibility::Module(module, _) if module == here => "deep",
                Visibility::Module(module, _) if module == root => "the crate root",
                Visibility::Module(..) => "another module",
            };
            (name, shown)
        }));
        out
    };
    let (ours, theirs) = (seen(&db), seen(&fresh));
    assert_eq!(ours, theirs);
    let of = |name: &str| theirs.iter().find(|it| it.0 == name).map(|it| it.1);
    assert_eq!(of("impl 0::get"), Some("pub"), "premise: {theirs:?}");
    assert_eq!(of("impl 0::own"), Some("deep"), "premise: {theirs:?}");
    assert!(matches!(of("impl 0::shared"), Some("pub(crate)" | "the crate root")), "premise: {theirs:?}");
    assert_eq!(of("Count::count"), Some("pub"), "premise: a trait's item has its trait's: {theirs:?}");
    assert_eq!(frozen::differences(), Vec::<String>::new());

    // The planted control: the same records, their visibilities left out and their items not.
    let control = given_hollow_db(&source);
    let _left = super::signatures::left_out::leave_out(super::signatures::left_out::VISIBILITIES);
    let first = deep_impls(&control, Global)[0];
    let get = ImplItems::of(&control, first).0.items[0].1;
    refused_naming_deep(&control, "`assoc_visibility` of an impl's item", || {
        _ = get.assoc_visibility(&control);
    });
    let count = TraitItems::query_with_diagnostics(&control, deep_count(&control)).0.items[0].1;
    refused_naming_deep(&control, "`assoc_visibility` of a trait's item", || {
        _ = count.assoc_visibility(&control);
    });
}

/// Parts a test makes by hand ([`hand_parts`]), as an image holds them.
fn portable_hand_parts() -> [Portable; Part::ALL.len()] {
    hand_parts().map(Portable::written)
}

/// What an image answers: each part's bytes, then the lang items as name resolution reads
/// them (the bytes of the slot [`give_image`] fills).
fn answers(image: &Image) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Part::ALL.iter().map(|&part| image.view().part(part).to_vec()).collect();
    out.push(image.lang_items.bytes.to_vec());
    out
}

/// The refinement: an image opened in place at a non-zero offset of its record answers as the
/// same image opened at offset zero, and both as the parts written. The control: the same
/// image with each part's range left at its offset in the image (the shift by `range.start`
/// planted away) answers otherwise, and the oracle catches it.
#[test]
fn an_image_opened_in_place_answers_the_same_at_any_offset() {
    let key = ContentId([3; 32]);
    let parts = portable_hand_parts();
    let open = |lead: usize| {
        let (holder, image_at, _) = record_of(lead, key, &parts, b"pages");
        let paged = Paged { key, source: Arc::new(frozen::pages::Whole(holder.clone())) };
        let image = Image::in_place(key, key, holder, image_at.clone(), Mode::Serve, Some(paged));
        (image.unwrap(), image_at)
    };
    let (at_zero, zero) = open(0);
    let (inside, range) = open(48);
    assert_eq!((zero.start, range.start), (0, 48));
    let mut written: Vec<Vec<u8>> = parts.iter().map(|it| it.as_bytes().to_vec()).collect();
    written.push(parts[Part::LangItems as usize].as_bytes().to_vec());
    assert_eq!(answers(&at_zero), written);
    assert_eq!(answers(&inside), answers(&at_zero));

    let mut planted = inside.clone();
    planted.parts = planted.parts.map(|it| it.start - range.start..it.end - range.start);
    planted.lang_items.bytes = PartBytes::of(&planted.bytes, &planted.parts, Part::LangItems);
    assert_ne!(answers(&planted), answers(&at_zero), "the oracle catches parts read at their offsets in the image");
}

/// An image opened in place at a range not aligned to 16 or outside its record, of bytes
/// written under another key, in a mode that compares with a whole def map, or with no pages,
/// is an error naming which. The control: the same bytes at their range, under their key, in
/// `Mode::Serve` with pages, open.
#[test]
fn an_image_in_place_at_a_wrong_range_or_key_is_an_error_naming_which() {
    let key = ContentId([4; 32]);
    let (holder, at, _) = record_of(48, key, &portable_hand_parts(), b"pages");
    let len = holder.bytes().len();
    let paged = || Some(Paged { key, source: Arc::new(frozen::pages::Whole(holder.clone())) });
    let open = |range: std::ops::Range<usize>, wanted: ContentId, mode: Mode| {
        Image::in_place(wanted, wanted, holder.clone(), range, mode, paged()).err()
    };
    assert_eq!(open(at.clone(), key, Mode::Serve), None, "the control opens");
    assert_eq!(open(at.start + 8..at.end, key, Mode::Serve), Some(ImageError::Unaligned { start: at.start + 8 }));
    assert_eq!(
        open(at.start..len + 16, key, Mode::Serve),
        Some(ImageError::OutOfRecord { range: at.start..len + 16, len })
    );
    let other = ContentId([5; 32]);
    assert_eq!(open(at.clone(), other, Mode::Serve), Some(ImageError::Key { found: key, wanted: other }));
    for mode in [Mode::Check, Mode::Verify] {
        assert!(matches!(open(at.clone(), key, mode), Some(ImageError::Mode { mode: it, .. }) if it == mode), "{mode:?}");
    }
    let no_pages = Image::in_place(key, key, holder.clone(), at, Mode::Serve, None).err();
    assert!(matches!(no_pages, Some(ImageError::Mode { mode: Mode::Serve, .. })), "{no_pages:?}");
}

/// A crate whose expansion of a procedural macro traced reading something has no surface
/// image: a surface's shape has no fact that names what it read, so [`image_parts`] refuses it
/// by the call. The control: a crate with no such expansion ([`FIXTURE`]'s `lib`) is not
/// refused.
#[test]
fn image_parts_refuses_a_traced_expansion_by_its_call() {
    let _turn = TURN.lock();
    let db = reading_db();
    let refused = image_parts(&db, crate_named(&db, "lib")).err();
    assert!(
        matches!(&refused, Some(Refused::Unasked { what }) if what.contains("proc_macros")),
        "{refused:?}"
    );
    let fixture = fixture::db(FIXTURE).into_db();
    assert!(image_parts(&fixture, crate_named(&fixture, "lib")).is_ok());
}

/// A library with a late read of each kind at item level: `env!("X")` in a signature,
/// `option_env!` of a variable no one sets, and `include!(concat!(env!("Y"), ".rs"))` in its
/// module, whose `env!("Y")` is two eager arguments deep.
const LATE_READS: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library env:X=ex,Y=made
#[rustc_builtin_macro]
macro_rules! include { () => {} }
#[rustc_builtin_macro]
macro_rules! concat { () => {} }
#[rustc_builtin_macro]
macro_rules! env { () => {} }
#[rustc_builtin_macro]
macro_rules! option_env { () => {} }
pub struct A(pub [u8; env!("X").len()]);
pub struct B(pub [u8; option_env!("BLAZE_DEPS_LATE_UNSET").is_some() as usize]);
include!(concat!(env!("Y"), ".rs"));
//- /registry/lib/src/made.rs
pub fn made() {}
"#;

/// `fixture`'s `lib`: its late reads as [`image_parts`] gives them with `nested`, and the
/// blaze-vfs content ID of its `made.rs`'s bytes: what the host answers a file ask with
/// (`blaze_host::outside::file_answer`), never base-db's `file_content`.
fn late_of(fixture: &str, nested: late::Nested) -> (Vec<LateSeen, Global>, ContentId) {
    let fixture = fixture::db(fixture);
    let made = fixture.file_at("/registry/lib/src/made.rs").file_id(fixture.db());
    let db = fixture.into_db();
    let parts = serve::image_parts_walking(&db, crate_named(&db, "lib"), nested);
    let late = match parts {
        Ok(parts) => parts.late,
        Err(refused) => panic!("`lib` refused: {refused}"),
    };
    let bytes = ContentId::of(blaze_vfs::FILE_DOMAIN, db.file_text(made).text(&db).as_bytes());
    assert_ne!(bytes, db.file_content(made), "premise: base-db's content ID is not the bytes' alone");
    (late, bytes)
}

fn seen(which: u8, name: &str, value: LateValue) -> LateSeen {
    LateSeen { which, name: name.as_bytes().into(), value }
}

/// The invariant: an image's late reads are every `env!`, `option_env!` and `include!` its
/// parts depend on, each once with the value its expansion saw, sorted by `(which, name)`;
/// a call nested in another's eager argument among them, and an include named by its path
/// relative to the crate root's directory. The control: with the nested calls dropped (the
/// planted bug), the nested `env!("Y")` is missing, and the list is otherwise the same.
#[test]
fn image_parts_lists_every_late_read_nested_ones_included() {
    let _turn = TURN.lock();
    let (late, made) = late_of(LATE_READS, late::Nested::Walk);
    let want = vec![
        seen(LATE_ENV, "X", LateValue::Var(Some("ex".into()))),
        seen(LATE_ENV, "Y", LateValue::Var(Some("made".into()))),
        seen(LATE_OPTION_ENV, "BLAZE_DEPS_LATE_UNSET", LateValue::Var(None)),
        seen(LATE_INCLUDE, "made.rs", LateValue::File(made)),
    ];
    assert_eq!(late.iter().collect::<Vec<_>>(), want.iter().collect::<Vec<_>>());

    let (planted, _) = late_of(LATE_READS, late::Nested::Skip);
    assert!(
        !planted.iter().any(|it| it.which == LATE_ENV && &*it.name == b"Y"),
        "the planted walk reached the nested `env!(\"Y\")`, so it is no control: {planted:?}"
    );
    let rest: Vec<_> = want.iter().filter(|it| &*it.name != b"Y").collect();
    assert_eq!(planted.iter().collect::<Vec<_>>(), rest, "the plant drops the nested call alone");
}

/// The invariant: the late reads are a function of the sources and the environment: two
/// databases of the same give equal lists, and another value of `X` changes that read's
/// value and nothing else, its name the same. The control is the second half: a list that
/// ignored the values would be equal there too.
#[test]
fn late_reads_are_the_same_in_every_database_and_follow_the_values() {
    let _turn = TURN.lock();
    let (one, _) = late_of(LATE_READS, late::Nested::Walk);
    let (two, _) = late_of(LATE_READS, late::Nested::Walk);
    assert_eq!(one.iter().collect::<Vec<_>>(), two.iter().collect::<Vec<_>>());

    let other = LATE_READS.replace("env:X=ex,", "env:X=other,");
    assert_ne!(other, LATE_READS, "premise: the fixture sets `X`");
    let (moved, _) = late_of(&other, late::Nested::Walk);
    assert_eq!(moved.len(), one.len());
    for (was, now) in one.iter().zip(&moved) {
        assert_eq!((was.which, &was.name), (now.which, &now.name));
        if was.which == LATE_ENV && &*was.name == b"X" {
            assert_eq!(now.value, LateValue::Var(Some("other".into())));
        } else {
            assert_eq!(was.value, now.value, "{was:?}");
        }
    }
}

/// `filed` as a list in key order, to compare.
fn filed_list(filed: &HashMap<base_db::CrateKey, ContentId, Global>) -> Vec<(base_db::CrateKey, ContentId)> {
    let mut list: Vec<_> = filed.iter().map(|(&krate, &key)| (krate, key)).collect();
    list.sort_unstable();
    list
}

/// The invariant: a [`Served`] names its crates by key, so a driver keeps it across a commit
/// that does not touch its crates (an edit of the member `main`'s file, after the commit
/// that gave the images) and stores from it as from a serve of that snapshot: each key names
/// the same crate, and the same crates are written, filed under the same keys as a serve and
/// store with no commit between.
#[test]
fn a_served_kept_across_an_unrelated_commit_is_used_again() {
    let _turn = TURN.lock();
    let fixture = fixture::db(FIXTURE);
    let main_rs = fixture.file_at("/main.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let crates = all(&db);
    let memory = Memory::default();
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let keyed = |db: &TestDB| names(db, &served.keys.keys().copied().collect::<Vec<_>>());
    let before = keyed(&db);
    assert_eq!(before, ["core", "lib", "main"], "premise: the serve keyed every crate");
    _ = db.set_file_text(main_rs, "use lib::{Shown, shown, deep::deeper::f};\nfn main() { shown!(); f(); f(); }\n");
    assert_eq!(keyed(&db), before, "each key names its crate after the commit");

    let stored_after = store(&db, &served, &memory, Which::Sealed).unwrap();
    assert_eq!(stored_after.refused, Vec::new());
    assert_eq!(names(&db, &stored_after.written), ["core", "lib"]);
    let (_, without_commit) = stored(FIXTURE);
    assert_eq!(filed_list(&stored_after.filed), filed_list(&without_commit.filed));
}

/// The control of [`a_served_kept_across_an_unrelated_commit_is_used_again`], on the type.
/// The old form (the three keyed by [`Crate`] ids) is not kept behind a test switch: it is a
/// second type of every field and of each user, and what it does wrong is not a wrong
/// answer a test can compare but a dead id, whose use after the commit that drops its arena
/// panics or, once the arena is made again, may name another crate; no cheap plant shows
/// that. So the control is that no field of [`Served`], [`Stored`] or [`KeyWalk`] can hold a
/// [`Crate`]: each one that names a crate is a [`base_db::CrateKey`], by its type, and the
/// patterns are exhaustive, so a field added fails to compile here until it is held to this.
#[test]
fn no_field_of_served_stored_or_key_walk_is_a_crate_id() {
    type Key = base_db::CrateKey;
    fn served(it: &Served) -> usize {
        let Served { nonce, keys, hits, stale, build: _, old_layout, lookups: _, stored, withdrawn } = it;
        let _: &base_db::Nonce = nonce;
        let keys: &HashMap<Key, ContentId, Global> = keys;
        let lists: [&Vec<Key>; 3] = [hits, stale, old_layout];
        let withdrawn: &Vec<Key, Global> = withdrawn;
        let StoredOf(stored) = stored;
        let stored: &Lock<blaze_hash::HashSet<Key, Global>> = stored;
        keys.len() + lists.iter().map(|it| it.len()).sum::<usize>() + withdrawn.len() + stored.lock().len()
    }
    fn stored_of(it: &Stored) -> usize {
        let Stored { written, refused, filed } = it;
        let written: &Vec<Key> = written;
        let refused: &Vec<(Key, Refused)> = refused;
        let filed: &HashMap<Key, ContentId, Global> = filed;
        written.len() + refused.len() + filed.len()
    }
    fn walk(it: &KeyWalk) -> usize {
        let KeyWalk { at, memo, plant_keep_across_views: _ } = it;
        let _: &Option<(base_db::Nonce, blaze_memo::Epoch, ContentId)> = at;
        let memo: &HashMap<Key, Option<ContentId>, Global> = memo;
        memo.len()
    }
    let _turn = TURN.lock();
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    let memory = Memory::default();
    let mut key_walk = KeyWalk::new();
    assert_eq!(key_walk.more(&db, &crates, BUILD).len(), 3);
    let served_now = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let stored_now = store(&db, &served_now, &memory, Which::Sealed).unwrap();
    assert!(served(&served_now) > 0 && stored_of(&stored_now) > 0 && walk(&key_walk) == 3);
}

/// The invariant: a [`Served`] whose crates a later write removed is stale, and [`store`]
/// says so by the typed error [`StoreError::Gone`] naming the least such key and the rule,
/// before it writes anything: never a panic, never the crate skipped. The control: the same
/// serve stored before the write writes images.
///
/// A crate, once made, keeps its key and its slot for the database's life: a graph that lacks
/// it takes it off the list (`Crates::set_graph`, "leave the list") and `Db::crate_at` still
/// finds it. So "removed" is "no longer listed", and that is what [`store`] reads.
#[test]
fn a_key_that_names_no_crate_is_the_typed_error() {
    let _turn = TURN.lock();
    let mut db = fixture::db(FIXTURE).into_db();
    let crates = all(&db);
    let memory = Memory::default();
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let control = store(&db, &served, &memory, Which::Sealed).unwrap();
    assert!(!control.written.is_empty(), "the control writes nothing: the test tests nothing");

    // Every crate removed: an empty graph.
    _ = db.commit_with(|edit| _ = edit.set_crate_graph(base_db::CrateGraphBuilder::default()));
    let least = served.keys.keys().copied().min().expect("premise: the serve keyed crates");
    let removed = db.crate_at(least).is_none_or(|krate| !base_db::all_crates(&db).contains(&krate));
    assert!(removed, "premise: the write took the crate off the list");
    let images = memory.images.lock().len();
    let error = store(&db, &served, &memory, Which::All).err();
    assert_eq!(error, Some(StoreError::Gone { key: least }));
    assert_eq!(memory.images.lock().len(), images, "nothing written");
    let shown = error.map(|it| it.to_string()).unwrap_or_default();
    assert!(
        shown.contains(&format!("crate key {}", least.index())) && shown.contains("keys-cover-every-input"),
        "{shown}"
    );
}

/// A library with a diagnostic item on a function, a struct, a trait and its method, an
/// inherent impl's method, an exported `macro_rules!` and a module, and one name given twice.
const DIAGNOSTIC_ITEMS: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
fn main() {}

//- /lib.rs crate:lib deps:core new_source_root:library
#![feature(rustc_attrs)]
#[rustc_diagnostic_item = "lib_fn"]
pub fn f() {}
#[rustc_diagnostic_item = "LibStruct"]
pub struct S;
#[rustc_diagnostic_item = "LibTrait"]
pub trait T {
    #[rustc_diagnostic_item = "lib_trait_method"]
    fn m(&self);
}
impl S {
    #[rustc_diagnostic_item = "lib_impl_method"]
    pub fn new() -> S { S }
}
#[macro_export]
#[rustc_diagnostic_item = "lib_macro"]
macro_rules! made { () => {} }
#[rustc_diagnostic_item = "lib_mod"]
pub mod inner {
    #[rustc_diagnostic_item = "Twice"]
    pub fn first() {}
    #[rustc_diagnostic_item = "Twice"]
    pub fn second() {}
}

//- /core.rs crate:core new_source_root:library
#[lang = "sized"]
pub trait Sized {}
#[rustc_builtin_macro]
pub macro derive {}
pub mod prelude { pub mod rust_2018 { pub use crate::derive; } }
"#;

/// The queries that read a page of a served crate's def map past its head (a module's scope,
/// a name, its traits, a textual macro), and the one that reads an item's
/// `#[rustc_diagnostic_item]`: what a walk of the crate runs.
fn diagnostic_item_page_reads(ran: &[String]) -> usize {
    let read = ["scope_memo", "name_memo", "traits_memo", "legacy_macro_memo", "diagnostic_item"];
    ran.iter().filter(|it| read.contains(&query_of(it))).count()
}

/// The query a run of the log is of: a row is `path::to::query(key)`
/// ([`TestDBExt::executed_in_read_order`]), this is `query`.
fn query_of(run: &str) -> &str {
    let path = run.split('(').next().unwrap_or(run);
    path.rsplit("::").next().unwrap_or(path)
}

/// The invariant: a crate served from its image answers `crate_diagnostic_items` from the
/// image's diagnostic items part, reading no page of its def map past its head and no item's
/// attributes, and the answer is the walk's: equal, item for item, to the walk of the same
/// crate in the same database (the same ids), and written as the same bytes as the crate's
/// diagnostic items resolved from source in another database (each item named by its path,
/// [`frozen::Enc::attr_owner`]). The control: with the part taken away (its slot taken in an
/// edit, as [`super::withdraw_image`] takes it), the same question walks the crate's pages,
/// which the count sees, and writes the same bytes. Compared as bytes, by path: the edit that
/// takes the slot makes `lib`'s arena again, so its items' ids are not the first answer's.
#[test]
fn diagnostic_items_of_a_served_crate_are_read_from_its_image_and_equal_the_walk() {
    use crate::{AdtId, AttrDefId, MacroId, diagnostic_item::crate_diagnostic_items, test_db::TestDBExt};
    let _turn = TURN.lock();
    let (memory, stored) = stored(DIAGNOSTIC_ITEMS);
    assert_eq!(stored.refused, Vec::new());
    let fresh = fixture::db(DIAGNOSTIC_ITEMS).into_db();
    let from_source = frozen::encode_diagnostic_items(&fresh, crate_named(&fresh, "lib")).unwrap();

    let mut db = fixture::db(DIAGNOSTIC_ITEMS).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    // `lib` of this snapshot: the serve committed, which drops its arena.
    let lib = crate_named(&db, "lib");
    // The head of `lib`'s pages, which a dependent reads first: read before the count.
    _ = crate::nameres::crate_def_map(&db, lib).crate_root(&db);
    assert_eq!(
        image(&db, lib).unwrap().part(Part::DiagnosticItems),
        from_source.as_slice(),
        "the part is the crate's diagnostic items as resolved from source"
    );
    let mut read = None;
    let ran = db.executed_in_read_order(|| read = Some(crate_diagnostic_items(&db, lib).clone()));
    let read = read.unwrap();
    assert!(
        ran.iter().any(|it| query_of(it) == "crate_diagnostic_items"),
        "premise: the query ran: {ran:?}"
    );
    assert_eq!(diagnostic_item_page_reads(&ran), 0, "a served crate's pages were walked: {ran:?}");
    assert_eq!(frozen::encode_diagnostic_items(&db, lib).unwrap(), from_source);

    let mut by_name: Vec<&str> = read.by_name.keys().map(|it| it.as_str()).collect();
    by_name.sort_unstable();
    assert_eq!(
        by_name,
        [
            "LibStruct",
            "LibTrait",
            "Twice",
            "lib_fn",
            "lib_impl_method",
            "lib_macro",
            "lib_mod",
            "lib_trait_method"
        ]
    );
    let get = |name: &str| read.get(&intern::Symbol::intern(name)).unwrap();
    assert!(matches!(get("lib_fn"), AttrDefId::FunctionId(_)));
    assert!(matches!(get("LibStruct"), AttrDefId::AdtId(AdtId::StructId(_))));
    assert!(matches!(get("LibTrait"), AttrDefId::TraitId(_)));
    assert!(matches!(get("lib_trait_method"), AttrDefId::FunctionId(_)));
    assert!(matches!(get("lib_impl_method"), AttrDefId::FunctionId(_)));
    assert!(matches!(get("lib_macro"), AttrDefId::MacroId(MacroId::MacroRulesId(_))));
    assert!(matches!(get("lib_mod"), AttrDefId::ModuleId(_)));
    assert_eq!(read.duplicates.len(), 1, "{:?}", read.duplicates);
    assert_eq!(read.duplicates[0].0.as_str(), "Twice");

    // The control: the part taken away, the same question walks `lib`'s pages, in the same
    // database, and the walk writes the bytes the image's answer wrote.
    let mut edit = db.edit();
    let key = lib.key(edit.db());
    assert!(edit.take_given::<frozen::DiagnosticItemsSlot>(key), "premise: `lib` held the part");
    _ = edit.commit();
    let lib = crate_named(&db, "lib");
    let mut walked = None;
    let ran = db.executed_in_read_order(|| walked = Some(crate_diagnostic_items(&db, lib).clone()));
    assert!(diagnostic_item_page_reads(&ran) > 0, "the count does not see a walk of the pages: {ran:?}");
    let walked = walked.unwrap();
    assert_eq!(walked.by_name.len(), read.by_name.len(), "the image's answer is not the walk's");
    assert_eq!(walked.duplicates.len(), read.duplicates.len(), "the image's answer is not the walk's");
    assert_eq!(
        frozen::encode_diagnostic_items(&db, lib).unwrap(),
        from_source,
        "the image's answer is not the walk's"
    );
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

/// The invariant: two databases write a crate's diagnostic items as the same bytes (the rows
/// in the order of their names, never a hash map's); and an image without the part (version
/// 3's schema, or this schema with two parts) is refused when opened, and a serve that finds
/// one under a crate's key is an error naming the crate: never an image whose crate has no
/// diagnostic items. Bytes of the part that do not decode stop the query with
/// [`frozen::Unreadable`] naming the crate and the part, never a walk in their place.
#[test]
fn diagnostic_items_are_the_same_bytes_and_an_image_without_them_is_refused() {
    let _turn = TURN.lock();
    let [a, b] = [(); 2].map(|()| {
        let db = fixture::db(DIAGNOSTIC_ITEMS).into_db();
        frozen::encode_diagnostic_items(&db, crate_named(&db, "lib")).unwrap()
    });
    assert_eq!(a, b);
    assert!(a.len() > 8, "the part holds the table: {} bytes", a.len());

    let key = ContentId([1; 32]);
    let old = Aligned::copy_from(&super::image::write_two_part_image(key, [b"lang", b"sig"], true));
    assert!(matches!(ImageRef::open(&old, key), Err(ImageError::Format(_))), "version 3 read");
    let short = Aligned::copy_from(&super::image::write_two_part_image(key, [b"lang", b"sig"], false));
    assert!(matches!(ImageRef::open(&short, key), Err(ImageError::Parts(_))), "two parts read");

    let (memory, stored) = stored(DIAGNOSTIC_ITEMS);
    let db = fixture::db(DIAGNOSTIC_ITEMS).into_db();
    let lib = crate_named(&db, "lib");
    let filed = stored.filed[&lib.key(&db)];
    let two = super::image::write_two_part_image(filed, [b"", b""], false);
    memory.images.lock().insert(filed, Arc::new(Aligned::copy_from(&two)));
    let mut db = fixture::db(DIAGNOSTIC_ITEMS).into_db();
    let crates = all(&db);
    let error = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap_err();
    assert!(error.contains("the image of crate lib"), "{error}");

    let mut db = fixture::db(DIAGNOSTIC_ITEMS).into_db();
    let mut damaged = a.clone();
    damaged.truncate(a.len() - 1);
    let frozen = frozen::Frozen { bytes: PartBytes::from(damaged), mode: Mode::Serve };
    let mut edit = db.edit();
    let given = {
        let db = edit.db();
        let lib = crate_named(db, "lib");
        let fact = frozen::frozen_fact(stable_id(db, lib), "diagnostic items", &frozen);
        (lib, fact, frozen)
    };
    assert_eq!(frozen::give_diagnostic_items(&mut edit, [given]), 1);
    _ = edit.commit();
    // `lib` of this snapshot: the gift's commit dropped its arena.
    let lib = crate_named(&db, "lib");
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        _ = crate::diagnostic_item::crate_diagnostic_items(&db, lib);
    }));
    let Err(payload) = caught else { panic!("a damaged part was answered") };
    let Some(stop) = frozen::Unreadable::of_payload(&*payload) else {
        panic!("the refusal unwound with another payload than `frozen::Unreadable`")
    };
    assert_eq!((stop.krate, stop.kind), (lib, "diagnostic items"));
}

/// The census (rule 82, [`serve::census`]): with it taken, [`image_parts`] records each
/// refusal and goes on, so one run lists both `#[path]` modules of `lib` in another package's
/// source root, in the order the def map has them, each naming its file and its declaration.
///
/// The control: the same crate with no census ends at the first, and only the first.
#[test]
fn a_census_lists_every_refusal_where_a_run_ends_at_the_first() {
    let _turn = TURN.lock();
    const TWO_INTO_MEMBER: &str = r#"
//- /registry/lib/src/lib.rs crate:lib new_source_root:library
#[path = "../../../ws/member/src/a.rs"]
pub mod a;
#[path = "../../../ws/member/src/b.rs"]
pub mod b;
//- /ws/member/src/lib.rs crate:member new_source_root:local
pub mod a;
pub mod b;
//- /ws/member/src/a.rs
pub fn f() {}
//- /ws/member/src/b.rs
pub fn g() {}
"#;
    let db = fixture::db(TWO_INTO_MEMBER).into_db();
    let lib = crate_named(&db, "lib");
    let names = |why: &Refused, module: &str, file: &str| {
        matches!(why, Refused::OutsideSourceRoot { what }
            if what.contains(&format!("module `crate::{module}`"))
                && what.contains(file)
                && what.contains("declared at")
                && what.contains("a file of another source root"))
    };
    let alone = image_parts(&db, lib).err();
    assert!(
        alone.as_ref().is_some_and(|why| names(why, "a", "/ws/member/src/a.rs")),
        "control: a run with no census ends at the first refusal: {alone:?}"
    );
    let (_, met) = serve::census::take(|| image_parts(&db, lib).map(|_| ()));
    assert!(
        matches!(&met[..], [a, b] if names(a, "a", "/ws/member/src/a.rs") && names(b, "b", "/ws/member/src/b.rs")),
        "the census lists both modules, each once: {met:?}"
    );
    assert!(!serve::census_taken(), "the census ends with its run");
}
