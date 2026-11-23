//! A build script's `cargo::rustc-cfg` atoms are part of the cfg its crate's names are resolved
//! under: [`UnitCrates::join_scripts`], the one place they reach a crate of the list.
//!
//! The shape is curve25519-dalek 4.1.3's: its build.rs prints
//! `cargo:rustc-cfg=curve25519_dalek_bits="64"`, and src/backend/serial/mod.rs picks the
//! module (`#[cfg(curve25519_dalek_bits = "64")] pub mod u64;`) that src/field.rs's
//! `FieldElement` alias names. Without the atom neither module exists and every impl on the
//! alias has a header that does not build (tempo's census, 2026-11-16: 849 + 147 bodies; the
//! same cause with a bare atom: nix's `apple_targets`, 831, and rustix's `libc`, 704 + 185).

use std::boxed::Box;

use blaze_build_script::instr::OutputLimits;
use blaze_build_script::{Outputs, OutputsRef};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs, Text, UnitId};

use super::{Origin, UnitCrate, UnitCrates, UnitsError};
use crate::graph::Sysroot;
use crate::names::{Why, Written};
use crate::resolve::{Names, SourceTexts, resolve_crates};
use crate::task::Serial;

fn a() -> Alloc<'static> {
    Alloc::global()
}

/// Each crate's files, by its index.
struct Texts(&'static [&'static [(&'static str, &'static str)]]);

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }
}

const USER: &str = "#![no_core]
pub fn main() {}
";

/// The Reference, "Conditional compilation": `#[cfg(name = \"value\")]` holds when the pair is
/// set, and an item whose predicate is false is removed with all it holds (so `mod a32;` names
/// no file to read: none is served for it below).
const DEP: &str = "#![no_core]
#[cfg(bits = \"64\")]
mod a64;
#[cfg(bits = \"32\")]
mod a32;
#[cfg(bits = \"64\")]
pub use a64::Elem;
";

const A64: &str = "pub struct Elem;
";

static TEXTS: Texts = Texts(&[&[("/package/0/src/lib.rs", USER)], &[("/package/1/src/lib.rs", DEP), ("/package/1/src/a64.rs", A64)]]);

const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

/// The outputs record of a build script that printed `printed`.
fn record(printed: &str) -> OutputsRef<'static> {
    let out = Outputs::read(printed.as_bytes(), "/target/debug/build/dep-0123456789abcdef/out", OutputLimits::DEFAULT, a()).unwrap_or_else(|e| panic!("{e:?}"));
    let mut bytes = AVec::new_in(a());
    out.encode(&mut bytes);
    let bytes: &'static [u8] = bytes.leak_static();
    OutputsRef::open(bytes).unwrap_or_else(|e| panic!("{e:?}"))
}

fn krate(name: &'static str, package: u32, deps: &[(&'static str, usize)], build_script: bool) -> UnitCrate<'static> {
    let mut dep_list = AVec::new_in(a());
    dep_list.extend(deps.iter().copied());
    let base = std::format!("/package/{package}/");
    UnitCrate {
        name,
        origin: Origin::Unit(UnitId(package)),
        key: CrateItemKey(ContentId::of("blaze-scope test crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: RUST,
        root: Text::from_str_in(&std::format!("{base}src/lib.rs"), a()),
        shown_from: Text::from_str_in(&base, a()),
        seen: blaze_expand::seen::SeenRoot::Package,
        features: AVec::new_in(a()),
        deps: dep_list,
        cfg: AVec::new_in(a()),
        build_script,
        script: None,
    }
}

/// `user` (crate 0) depending on `dep` (crate 1), which has a build script.
fn list() -> UnitCrates<'static> {
    let mut crates = AVec::new_in(a());
    crates.push(krate("user", 0, &[("dep", 1)], false));
    crates.push(krate("dep", 1, &[], true));
    UnitCrates { crates, sysroot: Sysroot::default(), toolchain: 0, build_scripts: 1, refused: AVec::new_in(a()), macro_from: 2, built_for_macro: AVec::new_in(a()), alloc: a() }
}

fn resolved(crates: UnitCrates<'static>) -> Names<'static> {
    let crates: &'static UnitCrates<'static> = Box::leak(Box::new(crates));
    let inputs = Box::leak(Box::new(crates.inputs()));
    resolve_crates(inputs, crates.sysroot, &TEXTS, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

/// What `::dep::Elem` names from `user`: the crate it is in, or why it names nothing.
fn elem(names: &Names<'static>) -> Result<usize, Why> {
    let written = Written { kind: PathKind::Plain, segments: &["{{root}}", "dep", "Elem"] };
    names.item_at(names.root(0), Namespace::Types, written, a()).map(|found| found.krate as usize).map_err(|e| e.why)
}

/// Expected line: `test units::script_tests::a_scripts_key_value_cfg_is_in_its_crates_cfg ... ok`.
///
/// With the script's record joined, `dep` is resolved under `bits="64"`: `mod a64;` and the
/// re-export exist, so `::dep::Elem` is dep's. The planted case withholds the record (what the
/// loader did before): the same path is not found. A record for a crate with no build script
/// is refused by name, and a joined crate has another key (rule 21).
#[test]
fn a_scripts_key_value_cfg_is_in_its_crates_cfg() {
    let script = record("cargo:rustc-cfg=bits=\"64\"\ncargo:rustc-cfg=has_bits\n");

    let mut joined = list();
    let plain_key = joined.crates[1].key;
    joined.join_scripts(&[None, Some(script)]).unwrap_or_else(|e| panic!("join_scripts: {e}"));
    assert_eq!(joined.build_scripts, 0, "the one build script's record is joined");
    assert_eq!(&joined.crates[1].cfg[..], &[("bits", Some("64")), ("has_bits", None)][..], "both forms of atom, sorted");
    assert!(joined.crates[0].cfg.is_empty(), "a script's cfg is its own crate's only");
    assert_ne!(joined.crates[1].key, plain_key, "the record is in the crate's key");
    assert!(joined.inputs()[1].script.is_some(), "the record goes on to the crate's input");
    assert_eq!(elem(&resolved(joined)), Ok(1), "`::dep::Elem` under the script's `bits=\"64\"`");

    // The planted case: the script's line withheld.
    let withheld = list();
    assert_eq!(withheld.build_scripts, 1);
    assert_eq!(elem(&resolved(withheld)), Err(Why::NotFound), "without the script's cfg, `Elem` is not there");

    // Refusals, each naming the crate.
    let mut wrong = list();
    assert_eq!(wrong.join_scripts(&[Some(script), None]).err(), Some(UnitsError::RecordWithoutScript { krate: "user" }));
    let mut short = list();
    assert_eq!(short.join_scripts(&[None]).err(), Some(UnitsError::ScriptsCount { crates: 2, given: 1 }));
    let mut twice = list();
    twice.join_scripts(&[None, Some(script)]).unwrap_or_else(|e| panic!("join_scripts: {e}"));
    assert_eq!(twice.join_scripts(&[None, Some(script)]).err(), Some(UnitsError::ScriptJoinedTwice { krate: "dep" }));
}
