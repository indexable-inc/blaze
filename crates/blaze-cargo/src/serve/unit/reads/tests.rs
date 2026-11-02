use blaze_cas::Key;
use blaze_hash::HashMap;
use std::alloc::Global;
use blaze_ids::ContentId;

use super::{Filing, Found, Reads, SLOTS, file, find, slot};

fn base() -> Key {
    Key(ContentId::of("blaze v2 test 1", b"base"))
}

pub(in crate::serve::unit) fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
}

/// A `now` for reads that hold no expansion: these tests run no macro.
pub(in crate::serve::unit) fn no_expansion(_: &base_db::Env, _: &[u8]) -> Result<ContentId, String> {
    Err("a test with no proc-macro expansion asked for one's reads".to_owned())
}

/// A store of slots and records, as a map.
#[derive(Default)]
pub(in crate::serve::unit) struct Store(pub(in crate::serve::unit) HashMap<Key, Vec<u8>, Global>);

impl Store {
    /// The record of base key `base` that `reads` made under `env`, filed as [`file`] says,
    /// with its slot kept.
    pub(in crate::serve::unit) fn keep(&mut self, base: Key, reads: &Reads, env: &[(String, String)]) -> Key {
        let Filing { key, slot } = file(base, reads, env, |at| self.0.get(at).map(Vec::as_slice)).unwrap().unwrap();
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
        find(base, env, now, |at| self.0.get(at).map(Vec::as_slice), |key| self.0.contains_key(key)).unwrap()
    }

    pub(in crate::serve::unit) fn find(&self, base: Key, env: &[(String, String)]) -> Option<Found> {
        self.find_now(base, env, no_expansion)
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

    let other_shell = env(&[("LLVM_SYS_231_PREFIX", "/l"), ("PROFILE_TAG", "x"), ("OLDPWD", "/b"), ("SHLVL", "3")]);
    assert_eq!(store.find(base(), &other_shell).map(|it| it.key), Some(key), "a variable nothing read changes nothing");
    let changed = env(&[("LLVM_SYS_231_PREFIX", "/m"), ("PROFILE_TAG", "x")]);
    assert_eq!(store.find(base(), &changed), None, "a read value changed");
    assert_eq!(store.find(base(), &env(&[("LLVM_SYS_231_PREFIX", "/l")])), None, "a read variable unset");
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
    let again = file(base(), &Reads::named(&["A"]), &env(&[("A", "9")]), |at| store.0.get(at).map(Vec::as_slice));
    assert!(again.unwrap().is_some_and(|it| it.slot.is_none()), "the same names reuse their slot");
    for n in 2..SLOTS {
        store.0.insert(slot(base(), n), Reads::named(&[format!("N{n}").as_str()]).encode());
    }
    assert_eq!(file(base(), &Reads::named(&["C"]), &a, |at| store.0.get(at).map(Vec::as_slice)), Ok(None));
    let nothing = file(base(), &Reads::default(), &a, |at| store.0.get(at).map(Vec::as_slice));
    assert_eq!(nothing.map(|it| it.map(|it| it.key)), Ok(Some(base())), "a record that read nothing needs no slot");
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
    assert_eq!(Reads::decode(&names[..names.len() - 1]), Err("a record's reads shorter than their lengths"));
    let mut other = names.clone();
    other[0] += 1;
    assert_eq!(Reads::decode(&other), Err("a record's reads of another version"));

    let mut store = Store::default();
    store.0.insert(slot(base(), 0), swapped);
    let error = find(base(), &[], no_expansion, |at| store.0.get(at).map(Vec::as_slice), |key| store.0.contains_key(key)).unwrap_err();
    assert!(error.starts_with("slot 0 of record key"), "{error}");
}
