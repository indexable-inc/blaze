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
//! - A macro call's expansion text (piece 16; runtime.md plan row B3) is served from the
//!   record's expansions DIRECTORY and the expansion records it names, each read from the store
//!   by its key once at the read ([`crate::names::expansion_texts`], the [`Eager`] strategy;
//!   [`read_crate_with`] takes the other). A call the directory names no text for, a record the
//!   store lacks ([`ExpansionRecordsError::Missing`], naming the call and the key: never an
//!   expansion made again) or a damaged row is answered none and filed in
//!   [`StoreLoad::unserved`] with its error, which the caller turns into a refusal of the body
//!   that read it ("not checked: types"), never a pass; every other body still answers.

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

use blaze_scope::expansion_records::ExpansionRecordsError;

use crate::CliError;
use crate::names::expansion_texts::{Eager, ExpansionTexts, ExpansionTextsError, Fetch, RecordSource, ServeError};
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
    /// The items section's edition, read by its one open at the load ([`checked_records`]):
    /// what a reader that needs only the edition takes, never opening the section again (a
    /// proc-macro caller's edition, run_names.rs; before 2026-11-18 every names-records build
    /// opened every crate's items section for it, re-checking its strings).
    pub edition: blaze_surface::Edition,
    /// The texts its macro calls made: its directory and the records it names, served once at
    /// the read.
    pub expansions: ExpansionTexts<'n>,
}

/// The command's store as a strategy's [`RecordSource`]: a record read in place and copied
/// once, 16-aligned, into the reader's allocator (as the record's sections are).
pub(crate) struct StoreRecords<'c, 's, 'm> {
    pub(crate) cx: &'c Cx,
    pub(crate) store: &'s mut dyn Store<'m>,
}

impl<'a> RecordSource<'a> for StoreRecords<'_, '_, '_> {
    fn record(&mut self, key: blaze_ids::ContentId, alloc: Alloc<'a>) -> Result<Option<&'a [u8]>, ServeError> {
        Ok(self.store.get(self.cx, Key(key)).map_err(ServeError::Store)?.map(|held| blaze_scope::resolve::aligned(held.bytes, alloc)))
    }
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
    /// An expansion record `name`'s directory names is held but not where the in-place reader
    /// can borrow it (engine_records.rs): refused, never copied.
    ExpansionNotInPlace { name: Name, key: Key, why: blaze_cas::plain::NotInPlace },
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
            LoadError::ExpansionNotInPlace { name, key, why } => {
                write!(f, "the expansion record {key:?} the directory of {name:?} names is not read in place: {why} (rule: engine_records.rs)")
            }
            LoadError::OtherCrate { name, asked, found, which } => {
                write!(f, "the record of {name:?} holds the {which} section of crate {found:?}, not of {asked:?} (rule: front_load.rs)")
            }
            LoadError::Store(error) => write!(f, "the store: {error}"),
        }
    }
}

/// The two sections of a `CrateScope` record's `payload`, read under `name` for the crate
/// `key`: each copied 16-aligned into `alloc` and checked to be `key`'s; its expansions
/// directory's records served by `F` through `source`.
pub(crate) fn crate_records<'n, F: Fetch>(name: Name, payload: &[u8], key: CrateItemKey, source: &mut dyn RecordSource<'n>, alloc: Alloc<'n>) -> Result<CrateRecords<'n>, LoadError> {
    let parts = sections::sections(payload).map_err(|error| LoadError::Sections { name, error })?;
    let names = blaze_scope::resolve::aligned(parts.names, alloc);
    let items = blaze_scope::resolve::aligned(parts.items, alloc);
    let expansions = blaze_scope::resolve::aligned(parts.expansions, alloc);
    checked_records::<F>(name, key, names, items, expansions, source, alloc)
}

/// A crate's three sections, each 16-aligned where it lies (copied by [`crate_records`], or in
/// a segment's mapping, engine_records.rs), checked once as `key`'s (rule 74): the one check of
/// both readers (rule 30). The expansions directory's records are served by `F` through
/// `source`: copied by the copying reader ([`StoreRecords`]), borrowed where they lie by the
/// in-place one (engine_records.rs `InPlaceRecords`).
pub(crate) fn checked_records<'n, F: Fetch>(
    name: Name,
    key: CrateItemKey,
    names: &'n [u8],
    items: &'n [u8],
    expansions: &'n [u8],
    source: &mut dyn RecordSource<'n>,
    alloc: Alloc<'n>,
) -> Result<CrateRecords<'n>, LoadError> {
    let expansions = match ExpansionTexts::open::<F>(expansions, source, alloc) {
        Ok(expansions) => expansions,
        Err(ServeError::Store(error)) => return Err(LoadError::Store(error)),
        Err(ServeError::NotInPlace { key, why }) => return Err(LoadError::ExpansionNotInPlace { name, key: Key(key), why }),
        Err(ServeError::Texts(error)) => return Err(LoadError::Expansions { name, error }),
    };
    let names_proof = Verified::check(names).map_err(|error| LoadError::Names { name, error })?;
    let found = names_proof.key();
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "names" });
    }
    // The section's one open this command: its key checked, its edition kept beside it. Every
    // later reader of its rows opens it once per job, in the job's names records
    // (front2 source.rs `Records::items_record`).
    let opened = ItemsRecord::open(items, alloc).map_err(|error| LoadError::Items { name, error })?;
    let (found, edition) = (opened.key(), opened.edition());
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "items" });
    }
    Ok(CrateRecords { key, names, names_proof, items, edition, expansions })
}

/// The crate `key`'s sections from the record `input` names, read from `store` by its key
/// (`input.at`) in place and copied into `alloc` ([`crate_records`]), its expansion records
/// served [`Eager`]ly. Absent: [`LoadError::NotInStore`].
pub(crate) fn read_crate<'n>(cx: &Cx, store: &mut dyn Store<'_>, input: &Input, key: CrateItemKey, alloc: Alloc<'n>) -> Result<CrateRecords<'n>, LoadError> {
    read_crate_with::<Eager>(cx, store, input, key, alloc)
}

/// [`read_crate`] with the expansion records served by `F` (rule 78: the one seam;
/// names/expansion_texts.rs `OnDemand` is refused by name until its piece is built).
pub(crate) fn read_crate_with<'n, F: Fetch>(cx: &Cx, store: &mut dyn Store<'_>, input: &Input, key: CrateItemKey, alloc: Alloc<'n>) -> Result<CrateRecords<'n>, LoadError> {
    let cx = cx.span("front load: read a crate's records");
    let Some(held) = store.get(&cx, input.at).map_err(LoadError::Store)? else {
        return Err(LoadError::NotInStore { name: input.name, at: input.at });
    };
    match blaze_drive::record::open(held.bytes) {
        Ok(blaze_drive::record::Stored::Whole(view)) => crate_records::<F>(input.name, view.payload, key, &mut StoreRecords { cx: &cx, store }, alloc),
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
    /// Each (crate, call) whose expansion was asked and none was served, with why when its
    /// directory names one ([`ExpansionRecordsError::Missing`]: the store lacks its record; a
    /// damaged row); `None`: the crate made no text for the call.
    unserved: RefCell<AVec<'n, Unserved>>,
    alloc: Alloc<'n>,
}

/// One expansion a question asked that was not served ([`StoreLoad::unserved`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Unserved {
    pub(crate) krate: usize,
    pub(crate) call: ItemKey,
    pub(crate) why: Option<ExpansionRecordsError>,
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
    pub(crate) fn unserved(&self) -> core::cell::Ref<'_, AVec<'n, Unserved>> {
        self.unserved.borrow()
    }

    /// `answer` for the call `call` of crate `krate`: `Some` served, else filed unserved.
    fn served<T>(&self, krate: usize, call: ItemKey, answer: Option<Result<Option<T>, ExpansionRecordsError>>) -> Option<T> {
        let (served, why) = match answer {
            Some(Ok(Some(value))) => (Some(value), None),
            Some(Ok(None)) | None => (None, None),
            Some(Err(error)) => (None, Some(error)),
        };
        if served.is_none() {
            self.unserved.borrow_mut().push(Unserved { krate, call, why });
        }
        served
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
        self.served(krate, call, self.crates.get(krate).map(|c| c.expansions.text(call)))
    }

    fn expansion_origins(&self, krate: usize, call: ItemKey) -> Option<(&'n [u8], &'n [u8])> {
        // Asked only of a text `expansion` served: one it cannot serve is filed as that is. The
        // two parts are handed where they lie (the directory's call part, the record's column:
        // `ExpansionTexts::origins_parts`), copied nowhere; the ladder opens them once a call
        // and keeps the view (blaze-ladder front2 source.rs `text_marks`). A text that keeps no
        // origins is an empty call part (the trait's docs).
        let parts = self.crates.get(krate).map(|c| {
            c.expansions.origins_parts(call).map(|row| {
                row.map(|kept| match kept {
                    Some(parts) => parts,
                    None => (&[][..], &[][..]),
                })
            })
        });
        self.served(krate, call, parts)
    }
}

#[cfg(test)]
#[path = "names/origins_parts_tests.rs"]
mod origins_parts_tests;

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
    pub(super) struct Files;

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

    pub(super) fn solved() -> Names<'static> {
        let inputs = [input("app", "/0/app/lib.rs", &[("lib", 1)]), input("lib", "/1/lib/lib.rs", &[])];
        let sysroot = Sysroot { core: None, alloc: None, std: None, bstd: None };
        let names = blaze_scope::resolve::resolve_crates(&inputs, sysroot, &Files, &blaze_scope::task::Serial, Alloc::global());
        names.unwrap_or_else(|e| panic!("the two crates' names: {e:?}"))
    }

    /// Each crate's `CrateScope` payload as names.rs `run` writes it.
    pub(super) fn payloads(names: &Names<'_>) -> std::vec::Vec<blaze_own::Vec<u8, std::alloc::Global>> {
        let g = Alloc::global();
        let mut out = std::vec::Vec::new();
        for c in names.crates().iter() {
            let n = blaze_scope::names_record::encode(c, g).unwrap_or_else(|e| panic!("names record of `{}`: {e:?}", c.name));
            let i = blaze_scope::items_record::encode(c, g).unwrap_or_else(|e| panic!("items record of `{}`: {e:?}", c.name));
            let split = blaze_scope::expansion_records::write(c, g).unwrap_or_else(|e| panic!("expansions of `{}`: {e}", c.name));
            out.push(sections::write(0, [&[], &[], &n[..], &i[..], &[], &split.directory[..], &[], &[]]));
        }
        out
    }

    /// The expansion records the crates' directories name, as a store holds them after their
    /// puts: every one (`all`), or (the planted bug: their puts skipped) none.
    pub(super) struct Kept(std::vec::Vec<(ContentId, std::vec::Vec<u8>)>);

    impl<'s> RecordSource<'s> for Kept {
        fn record(&mut self, key: ContentId, alloc: Alloc<'s>) -> Result<Option<&'s [u8]>, ServeError> {
            Ok(self.0.iter().find(|(k, _)| *k == key).map(|(_, bytes)| blaze_scope::resolve::aligned(bytes, alloc)))
        }
    }

    pub(super) fn kept(names: &Names<'_>, all: bool) -> Kept {
        let mut kept = std::vec::Vec::new();
        for c in names.crates().iter().filter(|_| all) {
            let split = blaze_scope::expansion_records::write(c, Alloc::global()).unwrap_or_else(|e| panic!("expansions of `{}`: {e}", c.name));
            kept.extend(split.records.iter().map(|r| (r.key, r.bytes.to_vec())));
        }
        Kept(kept)
    }

    pub(super) fn scope_name(key: CrateItemKey) -> Name {
        blaze_drive::key::crate_scope_name(&key)
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
    /// the calls whose texts the load could not serve.
    fn both(texts: bool) -> (std::vec::Vec<std::string::String>, std::vec::Vec<std::string::String>, bool, std::vec::Vec<Unserved>) {
        let names = solved();
        let solved_answers = answers(&names);
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads = payloads(&names);
        let mut kept = kept(&names, texts);
        let g = Alloc::global();
        let read: std::vec::Vec<CrateRecords<'static>> = keys
            .iter()
            .zip(payloads.iter())
            .map(|(key, payload)| crate_records::<Eager>(scope_name(*key), payload, *key, &mut kept, g).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        let read: &'static [CrateRecords<'static>] = std::vec::Vec::leak(read);
        let packages: &'static [&'static str] = std::vec::Vec::leak(keys.iter().map(|_| "/").collect());
        let load: &'static StoreLoad<'static> = std::boxed::Box::leak(std::boxed::Box::new(StoreLoad::new(read, packages, &Files, g)));
        let crate_names: &'static [&'static str] = std::vec::Vec::leak(names.crates().iter().map(|c| &*std::string::String::from(c.name).leak()).collect());
        let log: &'static Log<'static> = std::boxed::Box::leak(std::boxed::Box::new(Log::new(crate_names, g)));
        let frame: &'static Frame = std::boxed::Box::leak(std::boxed::Box::new(Frame::new(bstd::mem::Tag::Other).expect("a frame")));
        let records: &'static Records<'static> = std::boxed::Box::leak(std::boxed::Box::new(Records::new(load.keys(g), load, log, frame, g)));
        let over_store = answers(records);
        let unserved = load.unserved().to_vec();
        (solved_answers, over_store, log.is_empty(), unserved)
    }

    /// Piece 16, B3: an item a macro call wrote (`lib`'s `made`, `Made`) is read through the
    /// command's loader from its crate's directory and the expansion record it names, its text
    /// equal to the solve's; the loader leaves no call unserved. PLANTED BUG: the same records
    /// with the expansion records' puts skipped: the crates still open (nothing re-expands), the
    /// macro's items' answers differ (no text) and each unserved call is `Missing`, naming the
    /// record's key, while every item no macro wrote answers as before.
    /// Prints `front load: N items equal over the store, M of them written by a macro call`.
    #[test]
    fn an_item_a_macro_call_wrote_is_read_through_the_store_load() {
        let (solved_answers, over_store, quiet, unserved) = both(true);
        let by_macro = solved_answers.iter().filter(|a| a.contains("pub fn made") || a.contains("pub struct Made")).count();
        assert!(by_macro >= 2, "the macro's two items are among the answers: {solved_answers:?}");
        assert!(quiet, "the reader logged over the records with texts");
        assert!(unserved.is_empty(), "a call's text was not served: {unserved:?}");
        assert_eq!(solved_answers, over_store, "an answer differs over the store");
        let (_, without, _, unserved) = both(false);
        let differ = solved_answers.iter().zip(without.iter()).filter(|(a, b)| a != b).count();
        // The two items the call wrote (the crate's root text also holds the macro's rules, so
        // `by_macro` counts more answers than the call's own).
        assert!(differ >= 2 && !unserved.is_empty(), "CONTROL: without the expansion records the macro's items still answered ({differ} differ, {unserved:?})");
        for one in unserved.iter() {
            assert!(
                matches!(one.why, Some(ExpansionRecordsError::Missing { call, .. }) if call == one.call),
                "an unserved call is the record the store lacks, naming the call: {one:?}"
            );
        }
        let same = solved_answers.iter().zip(without.iter()).filter(|(a, b)| a == b).count();
        assert_eq!(same + differ, solved_answers.len(), "every answer is compared");
        assert!(same >= solved_answers.len() - by_macro, "CONTROL: an item no macro wrote answered otherwise without the expansion records ({same} of {})", solved_answers.len());
        println!("front load: {} items equal over the store, {by_macro} of them written by a macro call", over_store.len());
    }

    /// Rule 78's other strategy: `OnDemand` is not built, and its open is the typed refusal
    /// naming the piece it waits for, never a quiet eager read.
    #[test]
    fn the_on_demand_strategy_is_refused_naming_its_piece() {
        let names = solved();
        let lib = names.crates().iter().position(|c| c.name == "lib").expect("lib is solved");
        let key = names.crates()[lib].key;
        let payloads = payloads(&names);
        let mut kept = kept(&names, true);
        match crate_records::<crate::names::expansion_texts::OnDemand>(scope_name(key), &payloads[lib], key, &mut kept, Alloc::global()) {
            Err(LoadError::Expansions { error: ExpansionTextsError::NotBuilt { piece, .. }, .. }) => assert_eq!(piece, crate::names::expansion_texts::ON_DEMAND_PIECE),
            other => panic!("the on-demand strategy gave {other:?}"),
        }
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
            .map(|(key, payload)| crate_records::<Eager>(scope_name(*key), payload, *key, &mut kept(&names, true), g).unwrap_or_else(|e| panic!("{e}")))
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
        match crate_records::<Eager>(scope_name(keys[0]), &payloads[1], keys[0], &mut kept(&names, true), Alloc::global()) {
            Err(LoadError::OtherCrate { asked, found, which: "names", .. }) => assert_eq!((asked, found), (keys[0], keys[1])),
            other => panic!("lib's record under app's key was {other:?}"),
        }
    }

    /// THE TWO ROUTES OPEN ONE RECORD (engine_records.rs, rule 78), AND A COPY IS COUNTED: each
    /// crate's whole `CrateScope` record laid at a multiple of sixteen, as a segment lays it, and
    /// opened in place and by the copying route: the same sections' bytes; in place, each section
    /// borrowed from inside the record and 0 bytes counted copied; copying, from outside it, every
    /// section byte counted. The control for `engine_check_reads_records_in_place`'s 0. Prints
    /// `engine records: N crates opened in place (B bytes, 0 copied) and by copying (C copied)`.
    #[test]
    fn the_copying_route_counts_every_byte_it_copies() {
        use crate::engine_records::{Copying, InPlace, ReadCounts, open_record};
        use blaze_cas::plain::Aligned16;
        let names = solved();
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads = payloads(&names);
        let g = Alloc::global();
        let (mut in_place, mut copying) = (ReadCounts::default(), ReadCounts::default());
        for (key, payload) in keys.iter().zip(payloads.iter()) {
            let name = scope_name(*key);
            let head = blaze_drive::record::write_plain_head(name, ContentId::of("blaze-cli engine records test", b"source"), ContentId::of("blaze-cli engine records test", b"out"), &[], payload.len(), g);
            let mut whole = std::vec::Vec::new();
            whole.extend_from_slice(&head);
            whole.extend_from_slice(payload);
            let laid: &'static [u8] = blaze_scope::resolve::aligned(&whole, g);
            let record = Aligned16::new(laid).unwrap_or_else(|e| panic!("the laid record: {e}"));
            // The expansion records from a test's source on both routes (the command's in-place
            // source is engine_records.rs `InPlaceRecords`): this counts the sections alone.
            let read = open_record::<InPlace>(name, record, *key, &mut kept(&names, true), &mut in_place, g).unwrap_or_else(|e| panic!("in place: {e}"));
            let copied = open_record::<Copying>(name, record, *key, &mut kept(&names, true), &mut copying, g).unwrap_or_else(|e| panic!("copying: {e}"));
            assert_eq!((read.names, read.items), (copied.names, copied.items), "the two routes read other bytes");
            assert_eq!(read.expansions.len(), copied.expansions.len(), "the two routes' directories hold other rows");
            let inside = laid.as_ptr_range();
            assert!(inside.contains(&read.names.as_ptr()) && inside.contains(&read.items.as_ptr()), "in place: a section not borrowed from its record");
            assert!(!inside.contains(&copied.names.as_ptr()) && !inside.contains(&copied.items.as_ptr()), "copying: a section borrowed, not copied");
        }
        let sections: usize = payloads.iter().map(|payload| {
            let parts = sections::sections(payload).unwrap_or_else(|e| panic!("{e}"));
            parts.names.len() + parts.items.len() + parts.expansions.len()
        }).sum();
        assert_eq!(in_place.records, keys.len() as u64);
        assert_eq!(in_place.copied, 0, "in place: a byte counted copied");
        assert_eq!(copying.copied, sections as u64, "CONTROL: the copying route's copy is counted, every section byte");
        println!("engine records: {} crates opened in place ({} bytes, 0 copied) and by copying ({} copied)", keys.len(), in_place.bytes, copying.copied);
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
            fn toolchain_crates(&mut self, _subjects: &[ContentId]) {}
            fn index_beside(&mut self, _cx: &Cx, _owner: Key, _keys: &[Key]) -> Result<(), CliError> {
                Ok(())
            }
            fn put_parts(&mut self, _cx: &Cx, _key: Key, _parts: &[&[u8]]) -> Result<(), CliError> {
                Ok(())
            }
            fn put_record(&mut self, _cx: &Cx, _key: Key, _tag: blaze_keyed::kind::Tag, _owner: &[&[u8]], _bytes: &[u8]) -> Result<blaze_keyed::store::Put, CliError> {
                Ok(blaze_keyed::store::Put::Written)
            }
            fn kept(&self) -> crate::store::StoreKept {
                crate::store::StoreKept::default()
            }
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
            .map(|at| crate_records::<Eager>(scope_name(keys[*at]), &payloads[*at], keys[*at], &mut kept(names, true), g).unwrap_or_else(|e| panic!("{e}")))
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
