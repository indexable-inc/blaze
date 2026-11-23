//! The blaze edition's prelude read through two crates' globs, from a fresh solve and from the
//! names records (the hello world's `fn main(cx: &Cx, io: Io) -> Result<(), Error>`, 2026-11-16
//! 18:44: `Cx` found, `Result` and `Option` not, at `main`'s head).
//!
//! core: `mod option { pub enum Option { None } } pub mod prelude { pub mod v1 { pub use
//! crate::option::Option; } pub mod rust_2024 { pub use super::v1::*; } }`; bstd: `pub mod
//! prelude { pub mod blaze_2026_11 { pub use core::prelude::rust_2024::*; pub use
//! crate::cx::Cx; } } pub mod cx { pub struct Cx; }`; the program, blaze edition: nothing.
//!
//! The Reference, `names.preludes.std`: the standard prelude's names are in scope in every
//! module of a crate without `#[no_implicit_prelude]`; `items.use.glob`: a glob import imports
//! every public name of its target, the target's own glob imports included.

use std::format;
use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_surface::{Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::Texts;
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::{CrateId, Sysroot};
use crate::names::{Written, path};
use crate::names_record::{LoadNames, RecordNames, encode};
use crate::resolve::{CrateInput, CrateNames, Names, aligned};
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

const CORE: &str = "#![no_core]
pub mod option { pub enum Option { None } }
pub mod prelude {
    pub mod v1 { pub use crate::option::Option; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
";

const BSTD: &str = "#![no_std]
pub mod cx { pub struct Cx; }
pub mod prelude {
    pub mod blaze_2026_11 {
        pub use core::prelude::rust_2024::*;
        pub use crate::cx::Cx;
    }
}
";

const HELLO: &str = "fn main() {}\n";

static TEXTS: Texts = Texts(&[&[("/core/lib.rs", CORE)], &[("/bstd/lib.rs", BSTD)], &[("/hello/main.rs", HELLO)]]);

const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };
const BLAZE: Implicit = Implicit {
    std: StdIs::Bstd(edition::BlazeEdition::Blaze2026_11),
    prelude: Prelude::Blaze(edition::BlazeEdition::Blaze2026_11),
    proc_macro: false,
    entry: true,
};

fn input(name: &'static str, root: &'static str, implicit: Implicit) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-scope test crate", name.as_bytes())),
        edition: blaze_surface::Edition::E2021,
        implicit,
        root,
        shown_from: "/",
        features: &[],
        deps: &[],
        cfg: &[],
        script: None,
    }
}

struct Loaded(Vec<&'static [u8]>);

impl LoadNames<'static> for Loaded {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        self.0.get(krate).copied()
    }
}

/// `Option` and `Cx` at the program's root name core's enum and bstd's struct, from the fresh
/// solve's `Names` and from the records' `RecordNames` alike.
#[test]
fn the_blaze_prelude_names_what_its_globs_reach_fresh_and_from_records() {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("core", "/core/lib.rs", RUST), input("bstd", "/bstd/lib.rs", RUST), input("hello", "/hello/main.rs", BLAZE)];
    let sys = Sysroot { core: Some(CrateId(0)), alloc: None, std: None, bstd: Some(CrateId(1)) };
    let (solved, _) = resolve_by_records(&inputs, sys, &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        assert!(c.notes.is_empty(), "crate {i}: notes {:?}", c.notes);
    }
    let fresh = Names::of_crates(&inputs, sys, &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let root: ItemKey = fresh.root(2);
    let mut wrong = Vec::new();
    for name in ["Cx", "Option"] {
        let segments = [name];
        let written = Written { kind: PathKind::Plain, segments: &segments[..] };
        let a = path(&fresh, root, Namespace::Types, written, g);
        let b = path(&read, root, Namespace::Types, written, g);
        if a.is_err() || a != b {
            wrong.push(format!("`{name}`: fresh {a:?}, record {b:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

const EXT: &str = "#![no_core]
macro_rules! ext { () => { extern crate self as here; } }
ext!();
pub trait Tr {}
";

static EXT_TEXTS: Texts = Texts(&[&[("/ext/lib.rs", EXT)]]);

/// A macro-written root `extern crate self as here;` puts `here` in the extern prelude (the
/// Reference, `names.preludes.extern.decl`), in the crate root RECORD's list too: `::here::Tr`
/// read by `names::path` (the walk front2's header lowering takes) names the trait, fresh and
/// from the records (libc under `rustc-dep-of-std`: `cfg_if! { extern crate
/// rustc_std_workspace_core as core; }`, then `impl ::core::default::Default for stat`).
/// Before the fix: `NotFound` at segment 0.
#[test]
fn a_macro_written_root_extern_crate_is_in_the_records_extern_prelude() {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("ext", "/ext/lib.rs", RUST)];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &EXT_TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let root: ItemKey = fresh.root(0);
    let segments = ["here", "Tr"];
    let written = Written { kind: PathKind::Abs, segments: &segments[..] };
    let a = path(&fresh, root, Namespace::Types, written, g);
    let b = path(&read, root, Namespace::Types, written, g);
    assert!(a.is_ok() && a == b, "`::here::Tr`: fresh {a:?}, record {b:?}");
}
