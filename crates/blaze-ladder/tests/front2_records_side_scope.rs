//! pm-small's shape on the RECORDS path (the one `blaze check` takes): a workspace whose binary
//! `app` names a member proc macro `mac`. `mac` is built for the macro side (wasm32-wasip1, D89,
//! rule 10) on its own core, `wasm_core`, beside the target's `target_core`; both cores declare
//! `#[lang = "sized"]` and an `impl Word` (as the two real cores both declare
//! `AsyncGenFinished`: pm-small, 2026-11-16 19:51, pm-3.out: "the lang item `AsyncGenFinished`
//! is declared twice: .. (crate 4 of the records)").
//!
//! The readers each owner is given are what blaze-world check_scope.rs `check_scope` gives for
//! that shape (its own test, list_tests.rs `an_owner_sees_one_side`, proves the sets over a real
//! world): `app` sees itself, the target core and `mac` for its macros only; `mac` sees itself
//! and its side's core. The two sides never meet in one lang or impl table.
//!
//! The rule: a proc macro runs at build time (in blaze, on wasm32-wasip1 in Wasmtime: D89,
//! AGENTS.md rule 10) and is never linked into the program that names it, so its crates are
//! another program's; a proc macro's OWN bodies are its own program's (front2 linked.rs
//! `links_unit`).
//!
//! Tests (each prints `test <name> ... ok`):
//! - `app_checks_with_one_lang_table`: `app`'s world over its readers starts, its `sized` is the
//!   target core's, and its body is clean.
//! - `mac_checks_against_the_macro_side`: `mac`'s own body `Word::get()` finds `mac`'s own
//!   `impl Word`. WITHOUT the fix (buckets.rs `Buckets::admit` and asked.rs `admit_asked`
//!   skipping every proc-macro crate, crate 0 too) the impl is never filed and the body is not
//!   clean: this test fails.
//! - `control_both_sides_in_one_table_clash`: the readers the old rule gave (`app`'s walk
//!   followed `mac` into the macro side; lang crates chosen by name, both cores): "the lang item
//!   `sized` is declared twice".

use std::collections::HashMap;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::project::ProjectWorld;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const APP: &str = "#![no_core]\npub fn main() {}\n";
const CORE: &str = "#![no_core]\n#![feature(lang_items)]\n#[lang = \"sized\"]\npub trait Sized {}\npub struct Word;\nimpl Word {\n    pub fn get() -> Word {\n        Word\n    }\n}\n";
const MAC: &str = "#![no_core]\npub struct Word;\nimpl Word {\n    pub fn get() -> Word {\n        Word\n    }\n}\npub fn expand() -> Word {\n    Word::get()\n}\n";

/// World indexes: 0 `app`, 1 `target_core`, 2 `mac` (proc macro), 3 `wasm_core`.
const APP_AT: usize = 0;
const TARGET_CORE: usize = 1;
const MAC_AT: usize = 2;
const WASM_CORE: usize = 3;

struct Sides;

impl SourceTexts<'static> for Sides {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/main.rs") => Some(APP),
            (TARGET_CORE, "/target_core/lib.rs") | (WASM_CORE, "/wasm_core/lib.rs") => Some(CORE),
            (MAC_AT, "/mac/lib.rs") => Some(MAC),
            _ => None,
        }
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)], proc_macro: bool) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder records side scope test crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

/// The four crates' names, resolved once (each crate's records are written from them).
fn names() -> &'static Names<'static> {
    let crates = [
        input("app", "/app/main.rs", &[("core", TARGET_CORE), ("mac", MAC_AT)], false),
        input("target_core", "/target_core/lib.rs", &[], false),
        input("mac", "/mac/lib.rs", &[("core", WASM_CORE)], true),
        input("wasm_core", "/wasm_core/lib.rs", &[], false),
    ];
    let names = resolve_crates(&crates, Sysroot::default(), &Sides, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

/// One owner's records, in reader order (the owner at 0): the test's effect layer.
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    /// No texts, so no origins.
    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<LoadedExpansion<'static>> {
        None
    }
}

/// The records of the world crates `readers` (the owner first), as `blaze check`'s body job
/// gives them (body_types.rs `checker`).
fn records_of(readers: &[usize]) -> &'static Records<'static> {
    let names = names();
    let g = a();
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new() };
    let mut crate_names: Vec<&'static str> = Vec::new();
    let mut keys: Vec<CrateItemKey> = Vec::new();
    for &at in readers {
        let c = &names.crates()[at];
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), f.text)).collect());
        crate_names.push(c.name);
        keys.push(c.key);
    }
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(crate_names), g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    Box::leak(Box::new(Records::new(Vec::leak(keys), load, log, frame, g)))
}

/// The world over `readers`' records with lang crates `lang` (places in `readers`), or the
/// refusal's text.
fn world_of(readers: &[usize], lang: &[usize]) -> Result<(&'static Records<'static>, ProjectWorld<'static, &'static Records<'static>>), String> {
    let records = records_of(readers);
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the world's frame")));
    match ProjectWorld::from_records(records, frame, lang, a()) {
        Ok(world) => Ok((records, world)),
        Err(e) => Err(format!("{e}")),
    }
}

/// Every body of the owner checks clean over its records, and the records filed nothing.
fn every_body_clean(label: &str, records: &'static Records<'static>, world: &mut ProjectWorld<'static, &'static Records<'static>>) -> usize {
    let g = a();
    let bodies: Vec<_> = world.bodies(g).iter().map(|(b, _)| *b).collect();
    for body in bodies.iter() {
        let v = world.check(*body, g);
        assert!(v.is_clean(), "{label}: body {body:?} is not clean: diagnostics {:?}, gaps {}, refused {:?}", v.diagnostics, v.gaps.len(), v.refused.as_ref().map(|r| r.text()));
        let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{label}: body {body:?}: the records filed {errors:?}");
    }
    bodies.len()
}

#[test]
fn app_checks_with_one_lang_table() {
    // check_scope's readers for `app`: itself, the target core (its sysroot), `mac` for its
    // macros; lang: the owner and the target core.
    let (records, mut world) = world_of(&[APP_AT, TARGET_CORE, MAC_AT], &[0, 1]).unwrap_or_else(|e| panic!("app's world did not start: {e}"));
    let sized = world.front.index.lang("sized").expect("app's table has `sized`");
    assert_eq!(NamesSource::crate_of(records, sized), Some(1), "app's `sized` is the target core's (reader 1)");
    assert_eq!(every_body_clean("app", records, &mut world), 1, "`main`");
}

#[test]
fn mac_checks_against_the_macro_side() {
    // check_scope's readers for `mac` (checked on the macro side): itself and its side's core.
    let (records, mut world) = world_of(&[MAC_AT, WASM_CORE], &[0, 1]).unwrap_or_else(|e| panic!("mac's world did not start: {e}"));
    let sized = world.front.index.lang("sized").expect("mac's table has `sized`");
    assert_eq!(NamesSource::crate_of(records, sized), Some(1), "mac's `sized` is the macro side's core (reader 1)");
    // `Word::get` and `expand`: `expand`'s `Word::get()` is answered by mac's OWN `impl Word`.
    assert_eq!(every_body_clean("mac", records, &mut world), 2, "`Word::get` and `expand`");
}

#[test]
fn control_both_sides_in_one_table_clash() {
    // The old rule: app's walk followed `mac` to `wasm_core`, and both cores were lang crates
    // by name.
    let got = world_of(&[APP_AT, TARGET_CORE, MAC_AT, WASM_CORE], &[0, 1, 3]).err().expect("both cores in one table started");
    assert!(got.contains("the lang item `sized` is declared twice"), "{got}");
}
