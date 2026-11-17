//! The front end's [`Load`] over the store (drive.md, "End state: the whole pipeline on the
//! command", piece 6, STORE LOAD).
//!
//! front2 checks a body over `Records` (front2 source.rs), which asks a [`Load`] for a crate's
//! names section, its items section and the texts their rows point into, each the first time
//! a question reaches it. Here those come from the command's store: a crate's two sections are
//! read from its `CrateScope` record ([`crate::names::sections`]: the record names.rs `run`
//! files), found by the record's key as the request machine gave it (an [`Input`]); the texts
//! come from the world's trees (`/<tree>/<path>`: the checkout, the toolchain's packs, the
//! fetched packages' packs) through blaze-scope's [`SourceTexts`].
//!
//! - A record the store does not hold is [`LoadError::NotInStore`], naming its key: the caller
//!   asks the machine to make it (the pull). Nothing here makes, guesses or retries.
//! - A record read under one crate's name must be that crate's: both sections carry their
//!   crate's key, checked once at the read ([`crate_records`]); a mismatch is
//!   [`LoadError::OtherCrate`].
//! - Each section is copied once, 16-aligned, into the caller's allocator (the store's
//!   `get_into` reads a record into a frame; the records' readers need 16-aligned bytes). It is
//!   then read in place for the command: nothing decodes it whole and no second cache holds it.
//! - A macro call's expansion text (piece 16) is served from the same record's expansion texts
//!   section ([`crate::names::expansion_texts`]), copied with the other two. A call the section
//!   holds no text for (or a damaged row) is answered none and filed in
//!   [`StoreLoad::unserved`], which the caller turns into a refusal of the body ("not checked:
//!   types"), never a pass.

use core::cell::RefCell;

use blaze_drive::{Input, Key, Name};
use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::front2::source::Load;
use blaze_own::{AVec, Alloc};
use blaze_scope::items_record::{ItemsRecord, ItemsRecordError};
use blaze_scope::names_record::{NamesRecordError, Verified};
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::SourceTexts;
use bstd::cx::Cx;

use crate::CliError;
use crate::names::expansion_texts::{ExpansionTexts, ExpansionTextsError};
use crate::names::sections::{self, SectionsError};
use crate::store::Store;

/// One crate's names and items sections, read from its `CrateScope` record, each 16-aligned.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CrateRecords<'n> {
    pub key: CrateItemKey,
    pub names: &'n [u8],
    /// The names section's one form check this process (rule 74): every later read of `names`
    /// opens it from here, in O(1), with no check again.
    pub names_proof: Verified<'n>,
    pub items: &'n [u8],
    /// The texts its macro calls made, opened once at the read.
    pub expansions: ExpansionTexts<'n>,
}

/// Why a crate's sections could not be served (rule: blaze-cli front_load.rs).
#[derive(Debug)]
pub(crate) enum LoadError {
    /// The store holds no record at the key the machine gave for `name`: the caller has it made.
    NotInStore { name: Name, at: Key },
    /// The bytes at the key are not a whole record of this tool's form.
    NotWhole { name: Name, why: &'static str },
    /// The record's payload is not a `CrateScope` payload of this form.
    Sections { name: Name, error: SectionsError },
    /// A section does not open as its record.
    Names { name: Name, error: NamesRecordError },
    Items { name: Name, error: ItemsRecordError },
    /// The expansion texts section does not open.
    Expansions { name: Name, error: ExpansionTextsError },
    /// The record read under crate `asked`'s name holds crate `found`'s section `which`.
    OtherCrate { name: Name, asked: CrateItemKey, found: CrateItemKey, which: &'static str },
    /// The store could not be read.
    Store(CliError),
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::NotInStore { name, at } => write!(f, "not in the store: {name:?} at key {at:?} (rule: front_load.rs)"),
            LoadError::NotWhole { name, why } => write!(f, "the record of {name:?} is not a whole record: {why} (rule: front_load.rs)"),
            LoadError::Sections { name, error } => write!(f, "the record of {name:?}: {error}"),
            LoadError::Names { name, error } => write!(f, "the names section of {name:?} does not open: {error:?} (rule: front_load.rs)"),
            LoadError::Items { name, error } => write!(f, "the items section of {name:?} does not open: {error:?} (rule: front_load.rs)"),
            LoadError::Expansions { name, error } => write!(f, "the expansion texts of {name:?}: {error}"),
            LoadError::OtherCrate { name, asked, found, which } => {
                write!(f, "the record of {name:?} holds the {which} section of crate {found:?}, not of {asked:?} (rule: front_load.rs)")
            }
            LoadError::Store(error) => write!(f, "the store: {error}"),
        }
    }
}

/// The two sections of a `CrateScope` record's `payload`, read under `name` for the crate
/// `key`: each copied 16-aligned into `alloc` and checked to be `key`'s.
pub(crate) fn crate_records<'n>(name: Name, payload: &[u8], key: CrateItemKey, alloc: Alloc<'n>) -> Result<CrateRecords<'n>, LoadError> {
    let parts = sections::sections(payload).map_err(|error| LoadError::Sections { name, error })?;
    let names = blaze_scope::resolve::aligned(parts.names, alloc);
    let items = blaze_scope::resolve::aligned(parts.items, alloc);
    let expansions = ExpansionTexts::open(blaze_scope::resolve::aligned(parts.expansions, alloc)).map_err(|error| LoadError::Expansions { name, error })?;
    let names_proof = Verified::check(names).map_err(|error| LoadError::Names { name, error })?;
    let found = names_proof.key();
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "names" });
    }
    let found = ItemsRecord::open(items, alloc).map_err(|error| LoadError::Items { name, error })?.key();
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "items" });
    }
    Ok(CrateRecords { key, names, names_proof, items, expansions })
}

/// The crate `key`'s sections from the record `input` names, read from `store` by its key
/// (`input.at`) in place and copied into `alloc` ([`crate_records`]). Absent:
/// [`LoadError::NotInStore`].
pub(crate) fn read_crate<'n>(cx: &Cx, store: &mut dyn Store<'_>, input: &Input, key: CrateItemKey, alloc: Alloc<'n>) -> Result<CrateRecords<'n>, LoadError> {
    let cx = cx.span("front load: read a crate's records");
    let Some(held) = store.get(&cx, input.at).map_err(LoadError::Store)? else {
        return Err(LoadError::NotInStore { name: input.name, at: input.at });
    };
    match blaze_drive::record::open(held.bytes) {
        Ok(blaze_drive::record::Stored::Whole(view)) => crate_records(input.name, view.payload, key, alloc),
        Ok(blaze_drive::record::Stored::Shape(_)) => Err(LoadError::NotWhole { name: input.name, why: "a shape record, which holds no payload" }),
        Err(_) => Err(LoadError::NotWhole { name: input.name, why: "its bytes are not a record of this tool's form" }),
    }
}

/// front2's [`Load`] over crates read from the store: `crates[i]` is the reader's crate `i`
/// (the program's own at 0, as `Records::new`'s keys), `texts` the world's trees.
pub(crate) struct StoreLoad<'n> {
    crates: &'n [CrateRecords<'n>],
    /// `packages[i]`: the directory the world placed reader crate `i`'s package at (its
    /// `CrateInput::shown_from`): a record's package path is joined onto it to read (D96).
    packages: &'n [&'n str],
    texts: &'n dyn SourceTexts<'n>,
    /// Each (crate, call) whose expansion was asked and that crate's record holds no text for.
    unserved: RefCell<AVec<'n, (usize, ItemKey)>>,
    alloc: Alloc<'n>,
}

impl<'n> StoreLoad<'n> {
    pub(crate) fn new(crates: &'n [CrateRecords<'n>], packages: &'n [&'n str], texts: &'n dyn SourceTexts<'n>, alloc: Alloc<'n>) -> StoreLoad<'n> {
        StoreLoad { crates, packages, texts, unserved: RefCell::new(AVec::new_in(alloc)), alloc }
    }

    /// The crates' keys in reader order: `Records::new`'s `keys`.
    pub(crate) fn keys(&self, alloc: Alloc<'n>) -> &'n [CrateItemKey] {
        let mut keys = AVec::with_capacity_in(self.crates.len(), alloc);
        keys.extend(self.crates.iter().map(|c| c.key));
        keys.leak()
    }

    /// The macro calls whose expansion a question asked and none was served, in asked order.
    pub(crate) fn unserved(&self) -> core::cell::Ref<'_, AVec<'n, (usize, ItemKey)>> {
        self.unserved.borrow()
    }
}

impl<'n> Load<'n> for StoreLoad<'n> {
    fn names(&self, krate: usize) -> Option<&'n [u8]> {
        self.crates.get(krate).map(|c| c.names)
    }

    fn items(&self, krate: usize) -> Option<&'n [u8]> {
        self.crates.get(krate).map(|c| c.items)
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'n str> {
        let at = path.in_package(self.packages.get(krate)?, self.alloc)?;
        self.texts.read(krate, &at)
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'n str> {
        let text = self.crates.get(krate).and_then(|c| c.expansions.text(call).ok().flatten());
        if text.is_none() {
            self.unserved.borrow_mut().push((krate, call));
        }
        text
    }

    fn expansion_origins(&self, krate: usize, call: ItemKey) -> Option<&'n [u8]> {
        // Asked only of a text `expansion` served: one it cannot serve is filed as that is.
        let origins = self.crates.get(krate).and_then(|c| c.expansions.origins(call).ok().flatten());
        if origins.is_none() {
            self.unserved.borrow_mut().push((krate, call));
        }
        origins
    }
}

#[cfg(test)]
mod tests {
    //! Piece 6's proof: crates solved in memory, their sections written as names.rs `run`
    //! writes them (`sections::write`), read back through [`StoreLoad`] under `Records`: every
    //! answer about every item equals the in-memory names' (the comparison of blaze-ladder's
    //! tests/front2_records.rs `answers`, cut to the questions a body's check asks of an item);
    //! a record the store lacks is "not in the store"; a section read under another crate's
    //! key is caught.
    //!
    //! Prints `front load: N items equal over the store`.

    use super::*;
    use blaze_drive::Kind;
    use blaze_ids::ContentId;
    use blaze_ladder::front2::source::{Log, NamesSource, Records};
    use bstd::mem::Frame;
    use blaze_scope::graph::Sysroot;
    use blaze_scope::resolve::{CrateInput, Names};
    use blaze_surface::{Edition, Namespace};
    use blaze_unit_graph::{Implicit, Prelude, StdIs};
    use bstd::io::{Io, Limits, SimWorld};

    /// `made` and `Made` are written by a macro call (piece 16): their text is the call's
    /// expansion, read through [`StoreLoad::expansion`] from the record's expansion texts.
    const LIB: &str = "#![no_core]\npub mod m {\n    pub struct Thing;\n    pub fn make() -> Thing { Thing }\n}\npub use m::Thing;\nfn hidden() {}\nmacro_rules! write_two {\n    () => {\n        pub struct Made;\n        pub fn made() -> Made { Made }\n    };\n}\nwrite_two!();\n";
    const APP: &str = "#![no_core]\nuse lib::m::make;\npub struct Own;\nfn main() { let _ = make(); }\nmod inner { pub fn g() {} }\n";

    /// The files of the two crates, at their world paths.
    struct Files;

    impl<'a> SourceTexts<'a> for Files {
        fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
            match path {
                "/0/app/lib.rs" => Some(APP),
                "/1/lib/lib.rs" => Some(LIB),
                _ => None,
            }
        }
    }

    const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

    fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
        CrateInput {
            name,
            key: CrateItemKey(ContentId::of("blaze-cli front load test crate", name.as_bytes())),
            edition: Edition::E2021,
            implicit: RUST,
            root,
            shown_from: "/",
            features: &[],
            deps,
            cfg: &[],
            script: None,
        }
    }

    fn solved() -> Names<'static> {
        let inputs = [input("app", "/0/app/lib.rs", &[("lib", 1)]), input("lib", "/1/lib/lib.rs", &[])];
        let sysroot = Sysroot { core: None, alloc: None, std: None, bstd: None };
        let names = blaze_scope::resolve::resolve_crates(&inputs, sysroot, &Files, &blaze_scope::task::Serial, Alloc::global());
        names.unwrap_or_else(|e| panic!("the two crates' names: {e:?}"))
    }

    /// Each crate's `CrateScope` payload as names.rs `run` writes it.
    fn payloads(names: &Names<'_>) -> std::vec::Vec<blaze_own::Vec<u8, std::alloc::Global>> {
        payloads_with(names, true)
    }

    /// The payloads, with each crate's expansion texts section, or (`texts` false: the planted
    /// bug, the loader as before piece 16) with that section left empty.
    fn payloads_with(names: &Names<'_>, texts: bool) -> std::vec::Vec<blaze_own::Vec<u8, std::alloc::Global>> {
        let g = Alloc::global();
        let mut out = std::vec::Vec::new();
        for c in names.crates().iter() {
            let n = blaze_scope::names_record::encode(c, g).unwrap_or_else(|e| panic!("names record of `{}`: {e:?}", c.name));
            let i = blaze_scope::items_record::encode(c, g).unwrap_or_else(|e| panic!("items record of `{}`: {e:?}", c.name));
            let e = crate::names::expansion_texts::write(c).unwrap_or_else(|e| panic!("expansion texts of `{}`: {e}", c.name));
            let e: &[u8] = if texts { &e[..] } else { &[] };
            out.push(sections::write(0, [&[], &[], &n[..], &i[..], &[], e, &[]]));
        }
        out
    }

    fn scope_name(key: CrateItemKey) -> Name {
        crate::names::name_of(key)
    }

    /// Every answer about every item of every crate, as text: one function over both sources.
    fn answers<'n, S: NamesSource<'n>>(names: S) -> std::vec::Vec<std::string::String> {
        let mut out = std::vec::Vec::new();
        for krate in 0..names.crate_count() {
            let mut items = std::vec::Vec::new();
            names.each_item(krate, &mut |key, module| items.push((key, module)));
            items.sort();
            for (key, module) in items {
                let name = names.name_of(key);
                let text = names.text_of(key).map(|t| t.text);
                let binds = name.map(|n| (names.binds(module, Namespace::Types, n), names.binds(module, Namespace::Values, n)));
                out.push(format!(
                    "crate {krate} item {key:?} in {module:?}: name {name:?} kind {:?} home {:?} place {:?} text {text:?} binds {binds:?}",
                    names.kind_of_item(key),
                    names.home_of(key),
                    names.place_of(key),
                ));
            }
        }
        out
    }

    /// The answers in memory, the answers over the store load, whether the reader logged, and
    /// how many calls' texts the load could not serve.
    fn both(texts: bool) -> (std::vec::Vec<std::string::String>, std::vec::Vec<std::string::String>, bool, usize) {
        let names = solved();
        let solved_answers = answers(&names);
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads = payloads_with(&names, texts);
        let g = Alloc::global();
        let read: std::vec::Vec<CrateRecords<'static>> = keys
            .iter()
            .zip(payloads.iter())
            .map(|(key, payload)| crate_records(scope_name(*key), payload, *key, g).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        let read: &'static [CrateRecords<'static>] = std::vec::Vec::leak(read);
        let packages: &'static [&'static str] = std::vec::Vec::leak(keys.iter().map(|_| "/").collect());
        let load: &'static StoreLoad<'static> = std::boxed::Box::leak(std::boxed::Box::new(StoreLoad::new(read, packages, &Files, g)));
        let crate_names: &'static [&'static str] = std::vec::Vec::leak(names.crates().iter().map(|c| &*std::string::String::from(c.name).leak()).collect());
        let log: &'static Log<'static> = std::boxed::Box::leak(std::boxed::Box::new(Log::new(crate_names, g)));
        let frame: &'static Frame = std::boxed::Box::leak(std::boxed::Box::new(Frame::new(bstd::mem::Tag::Other).expect("a frame")));
        let records: &'static Records<'static> = std::boxed::Box::leak(std::boxed::Box::new(Records::new(load.keys(g), load, log, frame, g)));
        let over_store = answers(records);
        let unserved = load.unserved().len();
        (solved_answers, over_store, log.is_empty(), unserved)
    }

    /// Piece 16: an item a macro call wrote (`lib`'s `made`, `Made`) is read through the
    /// command's loader from its crate's record, its text equal to the solve's; the loader
    /// leaves no call unserved. CONTROL: the same records without the expansion texts section
    /// (the loader before piece 16, which served none): the macro's items' answers differ (no
    /// text) and their calls are unserved.
    /// Prints `front load: N items equal over the store, M of them written by a macro call`.
    #[test]
    fn an_item_a_macro_call_wrote_is_read_through_the_store_load() {
        let (solved_answers, over_store, quiet, unserved) = both(true);
        let by_macro = solved_answers.iter().filter(|a| a.contains("pub fn made") || a.contains("pub struct Made")).count();
        assert!(by_macro >= 2, "the macro's two items are among the answers: {solved_answers:?}");
        assert!(quiet, "the reader logged over the records with texts");
        assert_eq!(unserved, 0, "a call's text was not served");
        assert_eq!(solved_answers, over_store, "an answer differs over the store");
        let (_, without, _, unserved) = both(false);
        let differ = solved_answers.iter().zip(without.iter()).filter(|(a, b)| a != b).count();
        // The two items the call wrote (the crate's root text also holds the macro's rules, so
        // `by_macro` counts more answers than the call's own).
        assert!(differ >= 2 && unserved > 0, "CONTROL: without the texts section the macro's items still answered ({differ} differ, {unserved} unserved)");
        println!("front load: {} items equal over the store, {by_macro} of them written by a macro call", over_store.len());
    }

    #[test]
    fn sections_read_through_the_store_load_answer_as_the_solved_names() {
        let names = solved();
        let solved_answers = answers(&names);
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads = payloads(&names);
        let g = Alloc::global();
        let read: std::vec::Vec<CrateRecords<'static>> = keys
            .iter()
            .zip(payloads.iter())
            .map(|(key, payload)| crate_records(scope_name(*key), payload, *key, g).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        let read: &'static [CrateRecords<'static>] = std::vec::Vec::leak(read);
        // Every crate's package at `/` (`input`'s `shown_from`).
        let packages: &'static [&'static str] = std::vec::Vec::leak(keys.iter().map(|_| "/").collect());
        let load: &'static StoreLoad<'static> = std::boxed::Box::leak(std::boxed::Box::new(StoreLoad::new(read, packages, &Files, g)));
        let crate_names: &'static [&'static str] = std::vec::Vec::leak(names.crates().iter().map(|c| &*std::string::String::from(c.name).leak()).collect());
        let log: &'static Log<'static> = std::boxed::Box::leak(std::boxed::Box::new(Log::new(crate_names, g)));
        let frame: &'static Frame = std::boxed::Box::leak(std::boxed::Box::new(Frame::new(bstd::mem::Tag::Other).expect("a frame")));
        let records: &'static Records<'static> = std::boxed::Box::leak(std::boxed::Box::new(Records::new(load.keys(g), load, log, frame, g)));
        let over_store = answers(records);
        assert!(log.is_empty(), "the reader logged: {:?}", &*log.errors());
        assert!(solved_answers.len() >= 8, "the two crates hold their items: {solved_answers:?}");
        for (at, (solved, stored)) in solved_answers.iter().zip(over_store.iter()).enumerate() {
            assert_eq!(solved, stored, "answer {at} differs over the store");
        }
        assert_eq!(solved_answers.len(), over_store.len());
        println!("front load: {} items equal over the store", solved_answers.len());
    }

    /// PLANTED BUG: the `lib` crate's payload read under `app`'s name and key is refused at the
    /// read, naming both crates; no reader ever answers from it.
    #[test]
    fn a_section_read_under_another_crates_key_is_caught() {
        let names = solved();
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads = payloads(&names);
        match crate_records(scope_name(keys[0]), &payloads[1], keys[0], Alloc::global()) {
            Err(LoadError::OtherCrate { asked, found, which: "names", .. }) => assert_eq!((asked, found), (keys[0], keys[1])),
            other => panic!("lib's record under app's key was {other:?}"),
        }
    }

    /// A store without the record: "not in the store", naming the key; nothing is made here.
    #[test]
    fn a_record_the_store_lacks_is_not_in_the_store() {
        struct Empty;
        impl<'m> Store<'m> for Empty {
            fn get(&mut self, _cx: &Cx, _key: Key) -> Result<Option<crate::store::Held<'m>>, CliError> {
                Ok(None)
            }
            fn put(&mut self, _cx: &Cx, _key: Key, _bytes: &[u8]) -> Result<(), CliError> {
                Ok(())
            }
            fn map_all(&mut self, _cx: &Cx) -> Result<(), CliError> {
                Ok(())
            }
            fn after_answer(&mut self, _cx: &Cx, _profile: &mut blaze_drive::profile::Profile) -> Result<(), CliError> {
                Ok(())
            }
            fn public_crates(&mut self, _subjects: &[ContentId]) {}
        }
        let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
        let key = CrateItemKey(ContentId::of("blaze-cli front load test crate", b"absent"));
        let name = scope_name(key);
        assert_eq!(name.kind, Kind::CrateScope);
        let at = Key(ContentId::of("blaze-cli front load test key", b"absent"));
        let input = Input { name, at, out: ContentId::of("blaze-cli front load test out", b"absent") };
        match read_crate(&cx, &mut Empty, &input, key, Alloc::global()) {
            Err(error @ LoadError::NotInStore { .. }) => assert!(error.to_string().starts_with("not in the store: "), "{error}"),
            other => panic!("an empty store gave {other:?}"),
        }
    }

    /// The prelude reached through TWO crates' globs, as hello world reaches `Result`: the
    /// program's implicit prelude is `std::prelude::rust_2021` (the Rust Reference, "Preludes":
    /// the standard library prelude is `std::prelude::rust_20xx`, glob-imported into every
    /// module), which globs `core::prelude::rust_2021`, itself a glob of `core::prelude::v1`
    /// (the pinned library's own library/std/src/prelude/mod.rs and library/core/src/prelude/
    /// mod.rs). `Result` and `Option` resolve at the program's root through the command's
    /// loader as they do in the solved names.
    const PRE_CORE: &str = "#![no_core]\npub mod result { pub enum Result<T, E> { Ok(T), Err(E) } }\npub mod option { pub enum Option<T> { None, Some(T) } }\npub mod prelude {\n    pub mod v1 {\n        pub use crate::option::Option::{self, None, Some};\n        pub use crate::result::Result::{self, Err, Ok};\n    }\n    pub mod rust_2021 { pub use super::v1::*; }\n}\n";
    const PRE_STD: &str = "#![no_core]\npub mod prelude {\n    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }\n}\n";
    const PRE_APP: &str = "pub fn head() -> Result<(), ()> { loop {} }\n";

    struct PreludeFiles;

    impl<'a> SourceTexts<'a> for PreludeFiles {
        fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
            match path {
                "/0/app/lib.rs" => Some(PRE_APP),
                "/1/std/lib.rs" => Some(PRE_STD),
                "/2/core/lib.rs" => Some(PRE_CORE),
                _ => None,
            }
        }
    }

    /// Prints `front load: Result and Option through two preludes' globs, solved and over the store`.
    #[test]
    fn a_prelude_through_two_crates_globs_resolves_over_the_store() {
        let inputs = [input("app", "/0/app/lib.rs", &[]), input("std", "/1/std/lib.rs", &[("core", 2)]), input("core", "/2/core/lib.rs", &[])];
        let sysroot = Sysroot { core: Some(blaze_scope::graph::CrateId(2)), alloc: None, std: Some(blaze_scope::graph::CrateId(1)), bstd: None };
        let names = blaze_scope::resolve::resolve_crates(&inputs, sysroot, &PreludeFiles, &blaze_scope::task::Serial, Alloc::global())
            .unwrap_or_else(|e| panic!("the three crates' names: {e:?}"));
        let names: &'static Names<'static> = std::boxed::Box::leak(std::boxed::Box::new(names));
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads: &'static [blaze_own::Vec<u8, std::alloc::Global>] = std::vec::Vec::leak(payloads(names));
        let g = Alloc::global();
        // The readers in the command's order (body_types.rs `prepare`): the owner, then its
        // dependencies, then the sysroot's crates; here app, core, std, not the world's order.
        let order = [0usize, 2, 1];
        let read: std::vec::Vec<CrateRecords<'static>> = order
            .iter()
            .map(|at| crate_records(scope_name(keys[*at]), &payloads[*at], keys[*at], g).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        let read: &'static [CrateRecords<'static>] = std::vec::Vec::leak(read);
        let packages: &'static [&'static str] = std::vec::Vec::leak(order.iter().map(|_| "/").collect());
        let load: &'static StoreLoad<'static> = std::boxed::Box::leak(std::boxed::Box::new(StoreLoad::new(read, packages, &PreludeFiles, g)));
        let crate_names: &'static [&'static str] = std::vec::Vec::leak(order.iter().map(|at| &*std::string::String::from(names.crates()[*at].name).leak()).collect());
        let log: &'static Log<'static> = std::boxed::Box::leak(std::boxed::Box::new(Log::new(crate_names, g)));
        let frame: &'static Frame = std::boxed::Box::leak(std::boxed::Box::new(Frame::new(bstd::mem::Tag::Other).expect("a frame")));
        let records: &'static Records<'static> = std::boxed::Box::leak(std::boxed::Box::new(Records::new(load.keys(g), load, log, frame, g)));
        let app = names.crates().iter().position(|c| c.name == "app").expect("the app crate");
        let stored_app = order.iter().position(|at| *at == app).expect("app among the readers");
        for word in ["Result", "Option"] {
            let segments = [word];
            let written = blaze_scope::names::Written { kind: blaze_surface::PathKind::Plain, segments: &segments[..] };
            let solved_root = NamesSource::root_module(names, app).expect("the solved root");
            let solved = NamesSource::walk(names, solved_root, Namespace::Types, written);
            let stored_root = NamesSource::root_module(records, stored_app).expect("the stored root");
            let stored = NamesSource::walk(records, stored_root, Namespace::Types, written);
            assert!(solved.is_ok(), "`{word}` in the solved names: {solved:?}");
            assert_eq!(format!("{solved:?}"), format!("{stored:?}"), "`{word}` over the store differs from the solved names; the reader logged {:?}", &*log.errors());
        }
        println!("front load: Result and Option through two preludes' globs, solved and over the store");
    }
}
