//! An inherent method or associated fn of std that is `#[unstable(feature = ..)]` is not a
//! candidate of a crate that does not turn the feature on, while another applies: rustc_hir_typeck
//! method/probe.rs `pick_core` ("Pick stable methods only first, and consider unstable candidates
//! if not found") and `consider_candidates` (a candidate whose `tcx.eval_stability(..)` is `Deny`
//! is dropped from `applicable_candidates`); rustc_middle stability.rs
//! `eval_stability_allow_unstable` allows it when `self.features().enabled(feature)`, or
//! `enabled(implied_by)`.
//!
//! turso core (no `#![feature]`) six rows of check.out (reports/turso/c1851p): vdbe/
//! hash_table.rs:1335 `spill_state.distinct_key_places[i].try_insert(hash, place)?` (a
//! `HashMap`, its in-scope `TursoHashMapExt::try_insert` meant; std's `#[unstable(feature =
//! "map_try_insert")] HashMap::try_insert` was picked, so `?` met `OccupiedError`: E0277), and
//! index_method/fts/tests.rs:728 `DynVec::try_with_capacity_in(bytes, DynAllocator::default())`
//! (`DynVec<T> = Vec<T>`, `TursoVecInExt::try_with_capacity_in` meant; std's `#[unstable(feature =
//! "allocator_ext", implied_by = "allocator_api")]` inherent fn picked: "found `DynAllocator`,
//! expected `Global`", E0308). Reduced over the installed toolchain's std:
//!
//! Oracle: with no feature, `by_method` and `by_path` are clean (the traits' are picked).
//! Twins: `#![feature(map_try_insert)]` makes `by_method` pick std's inherent `try_insert`, whose
//! `Result<&mut u32, OccupiedError<..>>` is not the declared `Result<(), u8>` (E0308), while
//! `by_path` stays clean; `#![feature(allocator_api)]` (the `implied_by`, not the feature) makes
//! `by_path` pick std's, whose allocator argument is `Global`, not `u8` (E0308), while `by_method`
//! stays clean. Control (rule 23: the cause removed): a STABLE inherent item beside a trait's of
//! the same name still shadows it (`HashMap::insert`, `Vec::with_capacity`): E0308 in both.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

macro_rules! candidates {
    () => {
        r#"
use std::collections::HashMap;
pub trait MapExt<K, V> {
    fn try_insert(&mut self, k: K, v: V) -> Result<(), u8>;
}
impl<K: std::hash::Hash + Eq, V> MapExt<K, V> for HashMap<K, V> {
    fn try_insert(&mut self, k: K, v: V) -> Result<(), u8> {
        self.insert(k, v);
        Ok(())
    }
}
pub trait VecIn: Sized {
    fn try_with_capacity_in(n: usize, a: u8) -> Result<Self, u8>;
}
impl<T> VecIn for Vec<T> {
    fn try_with_capacity_in(n: usize, _a: u8) -> Result<Self, u8> {
        Ok(Vec::with_capacity(n))
    }
}
pub type DynVec<T> = Vec<T>;
pub fn by_method(m: &mut HashMap<u32, u32>) -> Result<(), u8> {
    m.try_insert(1, 2)
}
pub fn by_path() -> Result<DynVec<u8>, u8> {
    DynVec::try_with_capacity_in(4, 0u8)
}
fn main() {}
"#
    };
}

const PLAIN: &str = candidates!();
const MAP_FEATURE: &str = concat!("#![feature(map_try_insert)]\n", candidates!());
const IMPLIED_FEATURE: &str = concat!("#![feature(allocator_api)]\n", candidates!());

const CONTROL: &str = r#"
use std::collections::HashMap;
pub trait Ins<K, V> {
    fn insert(&mut self, k: K, v: V) -> u8;
}
impl<K, V> Ins<K, V> for HashMap<K, V> {
    fn insert(&mut self, _k: K, _v: V) -> u8 {
        0
    }
}
pub trait Cap: Sized {
    fn with_capacity(n: usize) -> u8;
}
impl<T> Cap for Vec<T> {
    fn with_capacity(_n: usize) -> u8 {
        0
    }
}
pub fn stable_method(m: &mut HashMap<u32, u32>) -> u8 {
    m.insert(1, 2)
}
pub fn stable_path() -> u8 {
    Vec::<u8>::with_capacity(4)
}
fn main() {}
"#;

/// Checks each of `bodies` of the one-crate program `source`, handing each verdict to `see`.
fn check_each(source: &'static str, bodies: &[&str], see: &mut dyn FnMut(&str, &BodyVerdict<'_>)) {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/unstable/src/main.rs";
    let files = [(root.to_owned(), source.to_owned())];
    let user = UserCrate {
        name: "unstable", dir: "/unstable/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: source }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    for &name in bodies {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
        let v = world.check(BodyRef::Free(key), alloc);
        see(name, &v);
    }
}

fn clean(what: &str, name: &str, v: &BodyVerdict<'_>) {
    assert!(v.is_clean() && v.reads.is_some(), "{what}: `{name}` must be clean (the trait's item picked): {v:?}");
    println!("{what} {name}: clean");
}

fn mismatch(what: &str, name: &str, v: &BodyVerdict<'_>) {
    assert!(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none(), "{what}: `{name}`: a gap or a refusal: {v:?}");
    assert!(!v.diagnostics.is_empty() && v.diagnostics.iter().all(|d| d.code == "E0308"), "{what}: `{name}` must be E0308 (std's inherent item picked): {v:?}");
    println!("{what} {name}: E0308 (std's inherent item picked)");
}

#[test]
fn an_unstable_inherent_item_yields_to_a_trait_without_its_feature() {
    check_each(PLAIN, &["by_method", "by_path"], &mut |name, v| clean("ORACLE", name, v));
}

#[test]
fn the_feature_or_its_implied_by_makes_the_inherent_item_the_pick() {
    check_each(MAP_FEATURE, &["by_method", "by_path"], &mut |name, v| match name {
        "by_method" => mismatch("TWIN map_try_insert", name, v),
        _ => clean("TWIN map_try_insert", name, v),
    });
    check_each(IMPLIED_FEATURE, &["by_method", "by_path"], &mut |name, v| match name {
        "by_path" => mismatch("TWIN allocator_api (implied_by)", name, v),
        _ => clean("TWIN allocator_api (implied_by)", name, v),
    });
}

#[test]
fn a_stable_inherent_item_still_shadows_the_trait() {
    check_each(CONTROL, &["stable_method", "stable_path"], &mut |name, v| mismatch("CONTROL", name, v));
}
