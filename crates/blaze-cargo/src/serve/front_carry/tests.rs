//! The carry driver in process, on a real VFS and an in-memory store (rule 64: no daemon),
//! each pass through [`Carry::run`] as the daemon's task runs it, its puts kept in an
//! [`Overlay`] and put into the store after, as the loop puts them:
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
//! What the carry does with a reached row (blaze-kernel's `nest::carry`): it repairs it in
//! place and cuts there where its value is the same bytes, whether or not a step reads it: a
//! root no step reads, `lines(file)` and `modules(crate)` here, is named at its get by the key
//! a read of it would use (`Kernel::get_in`), as a row a step read is. So no edit's root is
//! `gone` or asked again to run ([`Built::ran`] is 0): only a reached row that cannot be valued
//! at the new pin (a deleted file's) is repaired, then `gone`.
//!
//! The `Carried` counts before the kernel repaired readerless roots were the main agent's run
//! on int-1112 (2026-11-12); these, after it, are lane kernel-roots' prediction from them:
//! each root that was `gone` for want of a reader is now repaired, and `changed` or `cut` as
//! blaze-front-items' cutoffs test finds `lines` for the same edit.

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

impl Memory {
    /// The puts of a pass's overlay, put as the loop puts them.
    fn absorb(&mut self, puts: Vec<crate::serve::work::Put>) {
        for put in puts {
            Put::put(self, put.key, put.kind, put.bytes).unwrap();
        }
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
        .map(|n| {
            CrateRoot::new(
                &format!("c{n} c{n} lib"),
                format!("c{n}/src/lib.rs"),
                Edition::CURRENT,
                CfgOptions::default(),
                syntax::Extensions::NONE,
            )
        })
        .collect()
}

/// The checkout, its session, and the store its kernel keeps values in.
struct Checkout {
    vfs: Vfs,
    root: ContentId,
    store: Memory,
    carry: Option<Box<Carry>>,
    crates: Vec<CrateRoot>,
    /// Where the last pass saved the session.
    saved: Saved,
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
    let empty = Saved { session: StoreKey(ContentId([0; 32])), root };
    let mut it = Checkout { vfs, root, store: Memory::default(), carry: None, crates: crates(), saved: empty };
    let built = it.pass(Pass::Seed { root });
    assert_eq!(
        built,
        Built { first: true, crates: CRATES, files: CRATES * (MODS + 1), ..Built::default() },
        "the seed asks every crate's modules and every file's items and lines"
    );
    it
}

impl Checkout {
    /// One pass as the daemon's task runs it: the held session lent, the store read under an
    /// overlay, its puts put after.
    fn pass(&mut self, pass: Pass) -> Built {
        let state = self.carry.take();
        let reader = self.vfs.reader();
        let mut overlay = Overlay::new(&self.store);
        let (carry, built, saved) = Carry::run(state, pass, &reader, &mut overlay, &self.crates).unwrap();
        let puts = overlay.into_puts();
        self.store.absorb(puts);
        assert_eq!(carry.root(), self.root, "the pass ends at the build's root");
        self.carry = Some(carry);
        self.saved = saved;
        built
    }

    /// Applies `ops` to the checkout's tree and builds the new root.
    fn edit(&mut self, ops: &[Op]) -> Built {
        let from = self.root;
        self.root = self.vfs.apply(self.root, ops).unwrap();
        let changes = self.vfs.diff(from, self.root);
        self.pass(Pass::Move { root: self.root, changes })
    }

    /// The reread oracle: every root's value in the carried session equals a fresh session's
    /// over the tree as it is now, with a store of its own.
    fn check(&mut self) {
        let carried = self.carry.as_mut().unwrap().values(&mut self.store, &self.crates).unwrap();
        let mut store = Memory::default();
        let (mut fresh, _) = Carry::seed(&self.vfs.reader(), &mut store, self.root, &self.crates).unwrap();
        let expected = fresh.values(&mut store, &self.crates).unwrap();
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
/// an item is reached, `modules` among them; `lines` has no reader, and is repaired in the
/// carry and changes (as in blaze-front-items' cutoffs test), so asking it after is a hit.
#[test]
fn a_comment_edit_carries_three_entries_and_reaches_no_reader() {
    let mut it = checkout();
    let text = format!("{}// a comment after every item\n", module_text(3));
    let built = it.edit(&[write(EDITED, &text)]);
    assert_eq!(built.carried, carried(3, 3, 2, 1, 0), "parse and lines changed, items cut");
    assert_eq!((built.paths, built.files, built.ran), (1, CRATES * (MODS + 1), 0), "lines repaired by the carry");
    it.check();
}

/// One function's body, its length kept. The floor: `parse` (`+` to `*` is another token
/// kind) and `items` (that body's part) change; `modules` reads no body's part, so it is not
/// reached; `lines` has no reader, and is repaired in the carry and cut (no row moved).
#[test]
fn a_body_edit_carries_three_entries_and_reaches_no_modules() {
    let mut it = checkout();
    let text = module_text(3).replace("    x + 1\n", "    x * 1\n");
    let built = it.edit(&[write(EDITED, &text)]);
    assert_eq!(built.carried, carried(3, 3, 2, 1, 0), "parse and items changed, lines cut");
    assert_eq!(built.ran, 0, "lines repaired by the carry");
    it.check();
}

/// A new file and its `mod` at the end of the root. The change set: the root's text, and the
/// new file's text and existence, which nothing read before. The floor: the root's `parse`
/// and `items` change (the `mod` is a new item); its `lines` and `modules(c0)` have no reader,
/// and are repaired in the carry and change. The new file's `items` is a new entry, run by
/// `modules(c0)`'s repair; its `lines` is run when asked, not carried.
#[test]
fn a_new_file_reaches_its_crates_modules_and_is_asked() {
    let mut it = checkout();
    let mods = (0..MODS).chain([MODS]);
    let built = it.edit(&[write("c0/src/lib.rs", &root_text(mods)), write(&format!("c0/src/m{MODS}.rs"), &module_text(MODS))]);
    assert_eq!(built.carried, carried(3, 4, 4, 0, 0), "the root's parse, items and lines and modules(c0) changed");
    assert_eq!((built.paths, built.files, built.ran), (2, CRATES * (MODS + 1) + 1, 0));
    it.check();
}

/// A `mod` file deleted with its declaration. The change set: the root's text, and the
/// deleted file's text and existence. Seeded: the root's three, the deleted file's three
/// (they read its text) and `modules(c0)` (it read the file's existence). The root's `parse`
/// and `items` are repaired and change, and so are the root's `lines` and `modules(c0)`,
/// which no step reads; m10's `parse`, `items` and `lines` are repaired, then cannot be
/// valued (no such file) and are gone: three.
#[test]
fn a_deleted_mod_file_drops_its_entries_and_changes_its_crates_modules() {
    let mut it = checkout();
    let mods = (0..MODS).filter(|n| *n != 10);
    let built = it.edit(&[write("c0/src/lib.rs", &root_text(mods)), Op::Delete { path: "c0/src/m10.rs".to_owned() }]);
    assert_eq!(built.carried, carried(7, 7, 4, 0, 3), "the root's three and modules(c0) changed; m10's three gone");
    assert_eq!((built.paths, built.files, built.ran), (2, CRATES * (MODS + 1) - 1, 0), "the root's lines repaired by the carry");
    it.check();
}

/// The same root built again carries nothing and asks nothing that is not a hit.
#[test]
fn a_build_of_the_same_root_carries_nothing() {
    let mut it = checkout();
    let built = it.pass(Pass::Move { root: it.root, changes: Vec::new() });
    assert_eq!(built, Built { crates: CRATES, files: CRATES * (MODS + 1), ..Built::default() });
    it.check();
}

/// The session as store records (rule 26): saved after the seed, opened by a new driver,
/// carried over a body edit, it carries what the kept one does and gives what a fresh one
/// does. Two seeds of the same tree into two stores save to the same root key, every record
/// of it the same bytes (rule 25); a session opened and saved at its own root saves to the key
/// it was opened from.
#[test]
fn a_saved_session_opens_carries_and_equals_a_fresh_one() {
    let mut it = checkout();
    let saved = it.saved;

    let mut store = Memory::default();
    let mut overlay = Overlay::new(&store);
    let (_, _, twin) = Carry::run(None, Pass::Seed { root: it.root }, &it.vfs.reader(), &mut overlay, &it.crates).unwrap();
    let puts = overlay.into_puts();
    store.absorb(puts);
    assert_eq!(twin, saved, "a seed's session saves to one key");
    assert!(store.values == it.store.values, "and to the same records");

    it.carry = None;
    let reopened = it.pass(Pass::Open { saved, root: it.root, changes: Vec::new() });
    assert_eq!(reopened.carried, Carried::default(), "an open at its own root carries nothing");
    assert_eq!(it.saved, saved, "an opened session saved at its own root is the session it was opened from");

    it.carry = None;
    let from = it.root;
    let text = module_text(3).replace("    x + 1\n", "    x * 1\n");
    it.root = it.vfs.apply(it.root, &[write(EDITED, &text)]).unwrap();
    let changes = it.vfs.diff(from, it.root);
    let built = it.pass(Pass::Open { saved, root: it.root, changes });
    assert_eq!(built.carried, carried(3, 3, 2, 1, 0), "the opened session carries as the kept one does");
    assert_eq!((built.files, built.ran), (CRATES * (MODS + 1), 0), "every root asked again after an open, none as an edited file's");
    it.check();
}

/// A build script's `cargo:rustc-cfg` reaches `modules(crate)`: the crates the carry is given
/// are the build's end's ([`super::super::work::Writer::view_crates`]), each crate's cfg with
/// every run applied, so the `mod extra;` the root gates on `feature = "extra"` is found once a
/// run sets it. The crate is another name (its cfg's digest), its `modules` asked anew; the
/// files it shares with the crate before keep their entries, and `extra.rs` is asked.
#[test]
fn a_build_scripts_cfg_that_gates_a_mod_reaches_its_crates_modules() {
    let mut it = checkout();
    let mut cfg = CfgOptions::default();
    cfg.insert_key_value(intern::Symbol::intern("feature"), intern::Symbol::intern("extra"));
    let ran = CrateRoot::new("c0 c0 lib", "c0/src/lib.rs".to_owned(), Edition::CURRENT, cfg, syntax::Extensions::NONE);
    assert_ne!(ran.name, it.crates[0].name, "a cfg a run set is another crate");
    it.crates[0] = ran;
    let built = it.edit(&[write("c0/src/extra.rs", &module_text(99))]);
    assert_eq!((built.crates, built.files), (CRATES, CRATES * (MODS + 1) + 1), "the gated module's file is asked");
    let values = it.carry.as_mut().unwrap().values(&mut it.store, &it.crates).unwrap();
    assert!(values.contains_key("items c0/src/extra.rs"), "the gated module is the crate's");
    it.check();
}
