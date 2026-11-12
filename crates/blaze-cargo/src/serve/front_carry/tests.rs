//! The carry driver in process, on a real VFS and an in-memory store (rule 64: no daemon):
//! a checkout of 3 crates of 50 files each (a root and 49 `mod` files, one `mod` gated off by
//! a cfg, so `modules` reads the crate's cfg), seeded, then four edits of crate `c0`, each
//! with its exact [`Carried`] counts pinned and the reread oracle: every root's value as the
//! carried session gives it equals a fresh session's over the new tree.
//!
//! What each step reads (what the counts follow from): `parse(file)` reads the file's text and
//! its value is the tree's shape (kinds and lengths, no text); `items(file)` and `lines(file)`
//! read `parse` and the text; `modules(crate)` reads of each file's `items` only the declared
//! modules' parts, whether both candidates of each exist, and the cfg of each gate.
//!
//! What the carry does with a reached row (blaze-kernel's `nest::carry`): it repairs in place
//! only a row another step read (`Session::asked` names the key a read of it uses); a row no
//! step reads, `lines(file)` and `modules(crate)` here, is dropped and counted `gone`, and the
//! driver asks it again after the carry ([`Built::ran`] counts an edited file's). A reached
//! row that cannot be valued at the new pin (a deleted file's) is repaired, then `gone`. In
//! blaze-front-items' cutoffs test `lines` has readers (`line_of`), so there it is repaired.
//!
//! The `Carried` counts are the main agent's run on int-1112 (2026-11-12); `ran` is this
//! lane's prediction.

use std::collections::BTreeMap;

use blaze_cas::{Key as StoreKey, Kind as StoreKind, Mismatch, content_of};
use blaze_core::FileName;
use blaze_kernel::{Carried, Get, Put};
use blaze_vfs::{Mode, Op, Vfs};
use cfg::CfgOptions;
use syntax::Edition;

use super::*;

/// A store in memory.
#[derive(Default)]
struct Memory {
    values: BTreeMap<StoreKey, Vec<u8>>,
}

impl Get for Memory {
    fn get(&self, key: &StoreKey) -> Result<Option<&[u8]>, String> {
        Ok(self.values.get(key).map(Vec::as_slice))
    }

    fn has(&self, key: &StoreKey) -> Result<bool, String> {
        Ok(self.values.contains_key(key))
    }
}

impl Put for Memory {
    fn put(&mut self, key: StoreKey, _: StoreKind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        match self.values.get(&key) {
            Some(kept) if *kept != bytes => Err(Mismatch { key, kept: content_of(kept), given: content_of(&bytes) }),
            Some(_) => Ok(()),
            None => {
                self.values.insert(key, bytes);
                Ok(())
            }
        }
    }
}

const CRATES: usize = 3;
/// Each crate's `mod` files: with its root, 50 files a crate.
const MODS: usize = 49;

fn module_text(n: usize) -> String {
    let mut text = String::new();
    for f in 0..4 {
        text.push_str(&format!("pub fn f{f}(x: u32) -> u32 {{\n    x + {f}\n}}\n\n"));
    }
    text.push_str(&format!("pub const M: u32 = {n};\n"));
    text
}

fn root_text(mods: impl Iterator<Item = usize>) -> String {
    let mut text = String::from("//! a crate of the carry's checkout\n\n#[cfg(feature = \"extra\")]\nmod extra;\n");
    for n in mods {
        text.push_str(&format!("mod m{n};\n"));
    }
    text.push_str("\npub fn root() -> u32 {\n    0\n}\n");
    text
}

fn write(path: &str, text: &str) -> Op {
    Op::Write { mode: Mode::Regular, path: path.to_owned(), bytes: text.as_bytes().to_vec() }
}

fn crates() -> Vec<CrateRoot> {
    (0..CRATES)
        .map(|n| CrateRoot::new(&format!("c{n} c{n} lib"), format!("c{n}/src/lib.rs"), Edition::CURRENT, CfgOptions::default()))
        .collect()
}

/// The checkout, its session, and the store its kernel keeps values in.
struct Checkout {
    vfs: Vfs,
    root: ContentId,
    store: Memory,
    carry: Carry,
    crates: Vec<CrateRoot>,
}

fn checkout() -> Checkout {
    let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
    let mut ops = Vec::new();
    for c in 0..CRATES {
        ops.push(write(&format!("c{c}/src/lib.rs"), &root_text(0..MODS)));
        ops.push(write(&format!("c{c}/Cargo.toml"), "[package]\nname = \"c\"\n"));
        for n in 0..MODS {
            ops.push(write(&format!("c{c}/src/m{n}.rs"), &module_text(n)));
        }
    }
    let base = vfs.empty();
    let root = vfs.apply(base, &ops).unwrap();
    let mut store = Memory::default();
    let crates = crates();
    let (carry, built) = Carry::seed(&mut vfs, &mut store, root, &crates).unwrap();
    assert_eq!(
        built,
        Built { first: true, crates: CRATES, files: CRATES * (MODS + 1), ..Built::default() },
        "the seed asks every crate's modules and every file's items and lines"
    );
    Checkout { vfs, root, store, carry, crates }
}

impl Checkout {
    /// Applies `ops` to the checkout's tree and builds the new root.
    fn edit(&mut self, ops: &[Op]) -> Built {
        self.root = self.vfs.apply(self.root, ops).unwrap();
        self.carry.build(&mut self.vfs, &mut self.store, self.root, &self.crates).unwrap()
    }

    /// The reread oracle: every root's value in the carried session equals a fresh session's
    /// over the tree as it is now, with a store of its own.
    fn check(&mut self) {
        let carried = self.carry.values(&mut self.store, &self.crates).unwrap();
        let mut store = Memory::default();
        let (mut fresh, _) = Carry::seed(&mut self.vfs, &mut store, self.root, &self.crates).unwrap();
        let expected = fresh.values(&mut store, &self.crates).unwrap();
        fresh.close(&mut self.vfs);
        assert_eq!(carried.keys().collect::<Vec<_>>(), expected.keys().collect::<Vec<_>>(), "the same roots");
        for (label, value) in &expected {
            assert!(carried[label] == *value, "{label}: the carried value is not the fresh one");
        }
    }
}

fn carried(seeded: usize, repaired: usize, changed: usize, cut: usize, gone: usize) -> Carried {
    Carried { seeded, repaired, changed, cut, gone, ..Carried::default() }
}

const EDITED: &str = "c0/src/m3.rs";

/// A comment after the last item of one file. The floor: that file's `parse` (its shape has
/// one token more) changes; `items` reruns and is the same bytes (cut), so nothing that reads
/// an item is reached, `modules` among them; `lines` has no reader, so it is gone and runs
/// again when asked.
#[test]
fn a_comment_edit_carries_three_entries_and_reaches_no_reader() {
    let mut it = checkout();
    let text = format!("{}// a comment after every item\n", module_text(3));
    let built = it.edit(&[write(EDITED, &text)]);
    assert_eq!(built.carried, carried(3, 2, 1, 1, 1), "parse changed, items cut, lines gone");
    assert_eq!((built.paths, built.files, built.ran), (1, CRATES * (MODS + 1), 1), "lines asked again and run");
    it.check();
}

/// One function's body, its length kept. The floor: `parse` (`+` to `*` is another token
/// kind) and `items` (that body's part) change; `modules` reads no body's part, so it is not
/// reached; `lines` has no reader, so it is gone and runs again when asked.
#[test]
fn a_body_edit_carries_three_entries_and_reaches_no_modules() {
    let mut it = checkout();
    let text = module_text(3).replace("    x + 1\n", "    x * 1\n");
    let built = it.edit(&[write(EDITED, &text)]);
    assert_eq!(built.carried, carried(3, 2, 2, 0, 1), "parse and items changed, lines gone");
    assert_eq!(built.ran, 1, "lines asked again and run");
    it.check();
}

/// A new file and its `mod` at the end of the root. The change set: the root's text, and the
/// new file's text and existence, which nothing read before. The floor: the root's `parse`
/// and `items` change (the `mod` is a new item); its `lines` and `modules(c0)` have no reader
/// and are gone, asked again after the carry. The new file's `items` and `lines` are new
/// entries, run when asked, not carried.
#[test]
fn a_new_file_reaches_its_crates_modules_and_is_asked() {
    let mut it = checkout();
    let mods = (0..MODS).chain([MODS]);
    let built = it.edit(&[write("c0/src/lib.rs", &root_text(mods)), write(&format!("c0/src/m{MODS}.rs"), &module_text(MODS))]);
    assert_eq!(built.carried, carried(3, 2, 2, 0, 2), "the root's parse and items changed; its lines and modules gone");
    assert_eq!((built.paths, built.files, built.ran), (2, CRATES * (MODS + 1) + 1, 1));
    it.check();
}

/// A `mod` file deleted with its declaration. The change set: the root's text, and the
/// deleted file's text and existence. Seeded: the root's three, the deleted file's three
/// (they read its text) and `modules(c0)` (it read the file's existence). The root's `parse`
/// and `items` are repaired and change; m10's `parse` and `items` are repaired, then cannot
/// be valued (no such file) and are gone; the root's and m10's `lines` and `modules(c0)` have
/// no reader and are gone: five.
#[test]
fn a_deleted_mod_file_drops_its_entries_and_changes_its_crates_modules() {
    let mut it = checkout();
    let mods = (0..MODS).filter(|n| *n != 10);
    let built = it.edit(&[write("c0/src/lib.rs", &root_text(mods)), Op::Delete { path: "c0/src/m10.rs".to_owned() }]);
    assert_eq!(built.carried, carried(7, 4, 2, 0, 5), "the root's parse and items changed; five gone");
    assert_eq!((built.paths, built.files, built.ran), (2, CRATES * (MODS + 1) - 1, 1), "the root's lines run again");
    it.check();
}

/// The same root built again carries nothing and asks nothing that is not a hit.
#[test]
fn a_build_of_the_same_root_carries_nothing() {
    let mut it = checkout();
    let built = it.carry.build(&mut it.vfs, &mut it.store, it.root, &it.crates).unwrap();
    assert_eq!(built, Built { crates: CRATES, files: CRATES * (MODS + 1), ..Built::default() });
    it.check();
}

/// The session as store records (rule 26): saved after the seed, opened by a new driver,
/// carried over a body edit, it carries what the kept one does and gives what a fresh one
/// does. Two seeds of the same tree into two stores save to the same root key, every record
/// of it the same bytes (rule 25).
#[test]
fn a_saved_session_opens_carries_and_equals_a_fresh_one() {
    let mut it = checkout();
    let saved = it.carry.save(&mut it.store).unwrap();

    let mut store = Memory::default();
    let (mut twin, _) = Carry::seed(&mut it.vfs, &mut store, it.root, &it.crates).unwrap();
    assert_eq!(twin.save(&mut store).unwrap(), saved, "a seed's session saves to one key");
    assert!(store.values == it.store.values, "and to the same records");
    twin.close(&mut it.vfs);

    let opened = Carry::open(&mut it.vfs, &it.store, saved, &it.crates).unwrap();
    std::mem::replace(&mut it.carry, opened).close(&mut it.vfs);
    let text = module_text(3).replace("    x + 1\n", "    x * 1\n");
    let built = it.edit(&[write(EDITED, &text)]);
    assert_eq!(built.carried, carried(3, 2, 2, 0, 1), "the opened session carries as the kept one does");
    assert_eq!((built.files, built.ran), (CRATES * (MODS + 1), 0), "every root asked again after an open, none as an edited file's");
    it.check();
}
