use blaze_cas::Key;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use std::alloc::Global;

use super::{Filing, Found, Reads, SLOTS, file, find, slot};

fn base() -> Key {
    Key(ContentId::of("blaze v2 test 1", b"base"))
}

pub(in crate::serve::unit) fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
}

/// A `now` for reads that hold no expansion: these tests run no macro.
pub(in crate::serve::unit) fn no_expansion(
    _: &base_db::Env,
    _: &[u8],
) -> Result<ContentId, String> {
    Err("a test with no proc-macro expansion asked for one's reads".to_owned())
}

/// A `file_now` for reads that hold no file outside every source root.
pub(in crate::serve::unit) fn no_file(path: &str) -> ContentId {
    panic!("a test that read no file outside every source root asked for {path}")
}

/// A `primary` for a request that makes no package primary.
pub(in crate::serve::unit) fn no_primary(_: &str) -> bool {
    false
}

/// A store of slots and records, as a map.
#[derive(Default)]
pub(in crate::serve::unit) struct Store(pub(in crate::serve::unit) HashMap<Key, Vec<u8>, Global>);

impl Store {
    /// The record of base key `base` that `reads` made under `env`, filed as [`file`] says,
    /// with its slot kept.
    pub(in crate::serve::unit) fn keep(
        &mut self,
        base: Key,
        reads: &Reads,
        env: &[(String, String)],
    ) -> Key {
        self.keep_for(base, reads, env, &no_primary)
    }

    /// [`Store::keep`] for a request whose primary packages `primary` says.
    pub(in crate::serve::unit) fn keep_for(
        &mut self,
        base: Key,
        reads: &Reads,
        env: &[(String, String)],
        primary: &dyn Fn(&str) -> bool,
    ) -> Key {
        let Filing { key, slot } =
            file(base, reads, env, primary, |at| self.0.get(at).map(Vec::as_slice))
                .unwrap()
                .unwrap();
        if let Some((at, bytes)) = slot {
            self.0.insert(at, bytes);
        }
        self.0.insert(key, b"record".to_vec());
        key
    }

    /// Where [`find`] finds the record of `base` for `env`, expansions' reads worked out by
    /// `now`.
    pub(in crate::serve::unit) fn find_now(
        &self,
        base: Key,
        env: &[(String, String)],
        now: impl FnMut(&base_db::Env, &[u8]) -> Result<ContentId, String>,
    ) -> Option<Found> {
        find(
            base,
            env,
            &no_primary,
            now,
            no_file,
            |at| self.0.get(at).map(Vec::as_slice),
            |key| self.0.contains_key(key),
        )
        .unwrap()
    }

    /// Where [`find`] finds the record of `base` for no variables, files read by `file_now`.
    pub(in crate::serve::unit) fn find_files(
        &self,
        base: Key,
        file_now: impl FnMut(&str) -> ContentId,
    ) -> Option<Found> {
        let found = |key: &Key| self.0.contains_key(key);
        find(
            base,
            &[],
            &no_primary,
            no_expansion,
            file_now,
            |at| self.0.get(at).map(Vec::as_slice),
            found,
        )
        .unwrap()
    }

    pub(in crate::serve::unit) fn find(
        &self,
        base: Key,
        env: &[(String, String)],
    ) -> Option<Found> {
        self.find_now(base, env, no_expansion)
    }

    /// [`Store::find`] for a request whose primary packages `primary` says.
    pub(in crate::serve::unit) fn find_for(
        &self,
        base: Key,
        env: &[(String, String)],
        primary: &dyn Fn(&str) -> bool,
    ) -> Option<Found> {
        find(
            base,
            env,
            primary,
            no_expansion,
            no_file,
            |at| self.0.get(at).map(Vec::as_slice),
            |key| self.0.contains_key(key),
        )
        .unwrap()
    }
}

/// A record is found while every variable its closure read has the value it was made with,
/// whatever else the environment holds; a read variable changed or unset misses.
#[test]
fn a_record_is_found_while_what_it_read_is_as_it_was() {
    let mut store = Store::default();
    let reads = Reads::named(&["LLVM_SYS_231_PREFIX", "PROFILE_TAG"]);
    let made = env(&[("LLVM_SYS_231_PREFIX", "/l"), ("PROFILE_TAG", "x"), ("OLDPWD", "/a")]);
    let key = store.keep(base(), &reads, &made);
    assert_ne!(key, base(), "a record that read a variable is filed by it");

    let other_shell = env(&[
        ("LLVM_SYS_231_PREFIX", "/l"),
        ("PROFILE_TAG", "x"),
        ("OLDPWD", "/b"),
        ("SHLVL", "3"),
    ]);
    assert_eq!(
        store.find(base(), &other_shell).map(|it| it.key),
        Some(key),
        "a variable nothing read changes nothing"
    );
    let changed = env(&[("LLVM_SYS_231_PREFIX", "/m"), ("PROFILE_TAG", "x")]);
    assert_eq!(store.find(base(), &changed), None, "a read value changed");
    assert_eq!(
        store.find(base(), &env(&[("LLVM_SYS_231_PREFIX", "/l")])),
        None,
        "a read variable unset"
    );
}

/// A record whose closure read no variable is under its base key alone, with no slot.
#[test]
fn a_record_that_read_nothing_is_under_its_base_key() {
    let mut store = Store::default();
    let key = store.keep(base(), &Reads::default(), &env(&[("OLDPWD", "/a")]));
    assert_eq!(key, base());
    assert_eq!(store.0.len(), 1, "no slot is kept");
    assert_eq!(store.find(base(), &env(&[])), Some(Found { key: base(), slot: None }));
}

/// Another set of names takes the next slot; the same names reuse theirs; with every slot
/// holding other names the record is not kept, never filed over another's slot.
#[test]
fn each_set_of_names_has_a_slot_until_they_run_out() {
    let mut store = Store::default();
    let a = env(&[("A", "1"), ("B", "2")]);
    store.keep(base(), &Reads::named(&["A"]), &a);
    store.keep(base(), &Reads::named(&["B"]), &a);
    assert!(store.0.contains_key(&slot(base(), 0)) && store.0.contains_key(&slot(base(), 1)));
    let again = file(base(), &Reads::named(&["A"]), &env(&[("A", "9")]), &no_primary, |at| {
        store.0.get(at).map(Vec::as_slice)
    });
    assert!(again.unwrap().is_some_and(|it| it.slot.is_none()), "the same names reuse their slot");
    for n in 2..SLOTS {
        store.0.insert(slot(base(), n), Reads::named(&[format!("N{n}").as_str()]).encode());
    }
    assert_eq!(
        file(base(), &Reads::named(&["C"]), &a, &no_primary, |at| store
            .0
            .get(at)
            .map(Vec::as_slice)),
        Ok(None)
    );
    let nothing =
        file(base(), &Reads::default(), &a, &no_primary, |at| store.0.get(at).map(Vec::as_slice));
    assert_eq!(
        nothing.map(|it| it.map(|it| it.key)),
        Ok(Some(base())),
        "a record that read nothing needs no slot"
    );
}

/// A slot's bytes come from a store, which checked only their hash: names out of order,
/// repeated, cut short or of another version are refused, and the lookup names the slot.
#[test]
fn a_slot_that_does_not_decode_is_an_error_naming_it() {
    let names = Reads::named(&["A", "B"]).encode();
    assert_eq!(Reads::decode(&names), Ok(Reads::named(&["A", "B"])));
    let mut swapped = vec![names[0]];
    swapped.extend_from_slice(&2u32.to_le_bytes());
    for name in ["B", "A"] {
        swapped.extend_from_slice(&1u32.to_le_bytes());
        swapped.extend_from_slice(name.as_bytes());
    }
    swapped.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(Reads::decode(&swapped), Err("a record's read variables out of order or repeated"));
    assert_eq!(
        Reads::decode(&names[..names.len() - 1]),
        Err("a record's reads shorter than their lengths")
    );
    let mut other = names.clone();
    other[0] += 1;
    assert_eq!(Reads::decode(&other), Err("a record's reads of another version"));

    let mut store = Store::default();
    store.0.insert(slot(base(), 0), swapped);
    let error = find(
        base(),
        &[],
        &no_primary,
        no_expansion,
        no_file,
        |at| store.0.get(at).map(Vec::as_slice),
        |key| store.0.contains_key(key),
    )
    .unwrap_err();
    assert!(error.starts_with("slot 0 of record key"), "{error}");
}

/// A record whose closure read `CARGO_PRIMARY_PACKAGE` is filed by whether the request made its
/// package primary, and by nothing else of the request: found while that is as it was, missed
/// once it moves (rule 21). When not primary, the process's variable is what `env!` read, so it
/// is filed by the value read, not by how the value came.
#[test]
fn a_record_that_read_primary_package_is_filed_by_it() {
    const MANIFEST: &str = "/w/s/Cargo.toml";
    let mut store = Store::default();
    let reads = Reads::primary(MANIFEST);
    let primary = |manifest: &str| manifest == MANIFEST;
    let others = |manifest: &str| manifest == "/w/b/Cargo.toml";
    let key = store.keep_for(base(), &reads, &env(&[]), &primary);
    assert_ne!(key, base(), "a record that read it is filed by it");
    assert_eq!(
        store.find_for(base(), &env(&[("OLDPWD", "/b")]), &primary).map(|it| it.key),
        Some(key)
    );
    assert_eq!(store.find_for(base(), &env(&[]), &others), None, "its package no longer primary");
    assert_eq!(store.find_for(base(), &env(&[]), &no_primary), None, "no package primary");
    let not = store.keep_for(base(), &reads, &env(&[]), &others);
    assert_ne!(not, key);
    assert_eq!(
        store.find_for(base(), &env(&[]), &no_primary).map(|it| it.key),
        Some(not),
        "another package's primary-ness is nothing it read"
    );
    let process = env(&[("CARGO_PRIMARY_PACKAGE", "1")]);
    assert_eq!(
        store.find_for(base(), &process, &no_primary).map(|it| it.key),
        Some(key),
        "the process's variable, which `env!` reads when not primary, read as primary does"
    );
    assert_eq!(
        store.find_for(base(), &env(&[("CARGO_PRIMARY_PACKAGE", "0")]), &no_primary),
        None,
        "another value of it"
    );
    assert_eq!(
        store.find_for(base(), &env(&[]), &primary).map(|it| it.key),
        Some(key),
        "primary again"
    );
    let decoded = Reads::decode(&reads.encode()).unwrap();
    assert_eq!(decoded, reads, "its slot keeps the manifest");
}

/// A record whose closure read a file outside every source root is filed by the content ID
/// read of it: found while the file on disk reads the same, missed once it changed or can't
/// be read, found again once it is back as it was. Its path survives the slot.
#[test]
fn a_record_that_read_a_file_outside_every_root_is_found_while_the_file_is_as_it_was() {
    let mut store = Store::default();
    let read = |bytes: &[u8]| base_db::IncludedFile::content_of(Ok(bytes));
    let reads = Reads::of_files(&[("/far/code.rs", read(b"pub fn far() {}"))]);
    assert_eq!(
        Reads::decode(&reads.encode()).map(|it| it
            .files
            .into_iter()
            .map(|(path, _)| path)
            .collect()),
        Ok(vec!["/far/code.rs".to_owned()])
    );
    let key = store.keep(base(), &reads, &env(&[]));
    assert_ne!(key, base(), "a record that read a file is filed by it");
    let now = |bytes: &'static [u8]| {
        move |path: &str| {
            assert_eq!(path, "/far/code.rs");
            read(bytes)
        }
    };
    assert_eq!(
        store.find_files(base(), now(b"pub fn far() {}")).map(|it| it.key),
        Some(key),
        "the file as it was"
    );
    assert_eq!(store.find_files(base(), now(b"pub fn far() { 1; }")), None, "the file changed");
    let missing = base_db::ReadError::of(&std::io::Error::from_raw_os_error(2));
    let gone = |_: &str| base_db::IncludedFile::content_of(Err(&missing));
    assert_eq!(store.find_files(base(), gone), None, "the file is gone");
    assert_eq!(
        store.find_files(base(), now(b"pub fn far() {}")).map(|it| it.key),
        Some(key),
        "the file as it was again"
    );
}
