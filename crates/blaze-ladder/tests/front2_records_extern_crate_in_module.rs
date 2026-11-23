//! An `extern crate` written in a module other than the root binds its name in THAT module
//! only (the Reference, "Extern crate declarations": "the external crate is then bound into the
//! declaring scope as the given identifier"; only the root's join the extern prelude). bstd
//! (`#![no_std]`) gets `std` so: `mod rt { extern crate std; pub(crate) type ThreadMark =
//! std::thread::ThreadId; impl std::process::Termination for Exit {..} }` (blaze-std rt.rs),
//! and over records `std::..` there was refused `NotFound` at segment 0 (hello world's `blaze
//! run`, 2026-11-17: "the type alias `crate::rt::ThreadMark` is refused").
//!
//! The shape here: `app` names `dep` (so `dep` is in app's extern prelude), and `mod m` writes
//! `extern crate dep as d2;`: `d2` is bound in `m` alone, as `std` is in bstd's `rt`.
//!
//! Tests (each prints its lines):
//! - `a_module_s_extern_crate_resolves_over_records_as_over_names`: `d2::T` in `m` is `dep::T`
//!   over the solved names and over the records (`NamesSource::walk`), and `d2::T` at the crate
//!   root is NotFound on both (the control: the binding is `m`'s alone).
//! - `an_alias_through_a_module_s_extern_crate_checks_over_records`: `pub fn f(a: m::A) ->
//!   m::A` with `type A = d2::T;` in `m`: every body clean over the records, nothing filed.

use std::collections::HashMap;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::project::ProjectWorld;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::graph::Sysroot;
use blaze_scope::names::Written;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const APP: &str = "#![no_core]
mod m {
    extern crate dep as d2;
    pub type A = d2::T;
    impl d2::Tr for super::S {}
}
pub struct S;
pub fn f(a: m::A) -> m::A {
    a
}
";
const DEP: &str = "#![no_core]\n#![feature(lang_items)]\n#[lang = \"sized\"]\npub trait Sized {}\npub struct T;\npub trait Tr {}\n";

const APP_AT: usize = 0;
const DEP_AT: usize = 1;

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (DEP_AT, "/dep/lib.rs") => Some(DEP),
            _ => None,
        }
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder extern crate in a module test crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

fn names() -> &'static Names<'static> {
    let crates = [input("app", "/app/lib.rs", &[("dep", DEP_AT)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolve_crates(&crates, Sysroot::default(), &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

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

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<LoadedExpansion<'static>> {
        None
    }
}

/// Both crates' records, `app` first, as `blaze check`'s body job gives them.
fn records(names: &'static Names<'static>) -> &'static Records<'static> {
    let g = a();
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new() };
    let mut crate_names: Vec<&'static str> = Vec::new();
    let mut keys: Vec<CrateItemKey> = Vec::new();
    for at in [APP_AT, DEP_AT] {
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

/// `app`'s module `m` and its root module.
fn modules(names: &Names<'_>) -> (ItemKey, ItemKey) {
    let all: Vec<_> = names.modules(APP_AT).map(|m| (m.key, m.parent, m.path.to_string())).collect();
    let root = all.iter().find(|m| m.1.is_none()).map(|m| m.0).unwrap_or_else(|| panic!("app's root module: {all:?}"));
    let m = all.iter().find(|m| m.1.is_some() && m.2.ends_with('m')).map(|m| m.0).unwrap_or_else(|| panic!("app's `mod m`: {all:?}"));
    (m, root)
}

#[test]
fn a_module_s_extern_crate_resolves_over_records_as_over_names() {
    let names = names();
    let records = records(names);
    let (m, root) = modules(names);
    let segments = ["d2", "T"];
    let written = Written { kind: PathKind::Plain, segments: &segments[..] };
    let solved = names.path(m, Namespace::Types, written, a()).map(|r| (r.def.key, r.consumed));
    let over_records = NamesSource::walk(records, m, Namespace::Types, written).map(|r| (r.def.key, r.consumed));
    println!("d2::T in m: solved {solved:?}; over records {over_records:?}");
    assert!(solved.is_ok(), "the solved names resolve `d2::T` in `m`: {solved:?}");
    assert_eq!(over_records, solved, "over records as over the solved names");
    // The control: the binding is `m`'s alone, so at the root `d2` is no name.
    let at_root = names.path(root, Namespace::Types, written, a()).map(|r| r.def.key);
    let at_root_records = NamesSource::walk(records, root, Namespace::Types, written).map(|r| r.def.key);
    println!("d2::T at the root: solved {at_root:?}; over records {at_root_records:?}");
    assert!(at_root.is_err() && at_root_records.is_err(), "`d2` is bound in `m` only");
}

#[test]
fn an_alias_through_a_module_s_extern_crate_checks_over_records() {
    let names = names();
    let records = records(names);
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the world's frame")));
    let mut world = ProjectWorld::from_records(records, frame, &[0, 1], a()).unwrap_or_else(|e| panic!("app's world did not start: {e}"));
    let g = a();
    let bodies: Vec<_> = world.bodies(g).iter().map(|(b, _)| *b).collect();
    assert!(!bodies.is_empty(), "`f` is a body");
    for body in bodies.iter() {
        let v = world.check(*body, g);
        println!("body {body:?}: diagnostics {:?}, refused {:?}", v.diagnostics, v.refused.as_ref().map(|r| r.text()));
        assert!(v.is_clean(), "body {body:?} is not clean: diagnostics {:?}, gaps {}, refused {:?}", v.diagnostics, v.gaps.len(), v.refused.as_ref().map(|r| r.text()));
        let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "body {body:?}: the records filed {errors:?}");
    }
}
