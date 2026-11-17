//! STD-CENSUS 1 class C4 (2026-11-16): std's `Instant::now` (s7-time-06-monotonic) and the
//! temp-dir file rung (s7-fs-05-temp-dir-files) stop at "UnreadImpl: std impl 4bc897936089 does
//! not read (`Resolve(Path(.. at: 0, why: NotFound }), 30..57/68..95)`)". The impl is the
//! toolchain libc's (libc-0.2.191; ItemKey prefix 4bc8979360898bd0 is libc's, as are the ADTs
//! asked at) macro-made `impl ::core::default::Default for $name` (src/macros.rs:518 and :760;
//! each range is the 27 bytes of `:: core:: default:: Default`). That libc is built with
//! `rustc-dep-of-std` (src/lib.rs:219-230): `#![no_core]`, and its only `core` is the root's
//! `cfg_if! { if #[cfg(feature = "rustc-dep-of-std")] { extern crate rustc_std_workspace_core
//! as core; } }`, a crate whose root is `pub use core::*;`
//! (library/rustc-std-workspace-core/lib.rs). The fresh solve knew the macro-written `core`
//! (entry.rs `libc_shape_macro_written_extern_crate_and_modules`); the crate root RECORD's
//! extern prelude did not, so every walk over the records (`names::path`, front2's header
//! lowering) said NotFound at segment 0.
//!
//! The Reference, `names.preludes.extern.decl`: "If the `extern crate` declaration is in the
//! crate root, then the crate name is also added to the extern prelude" (by its `as` name; a
//! macro's expansion at the root counts as written there); `paths.qualifiers.global-root`: from
//! 2018 a path beginning `::` starts at the extern prelude; `items.use.glob`: a glob imports
//! every public name of its target.
//!
//! Here, by the same shape with names no other rule could reach: `base` (core's role) holds the
//! traits; `ws` (rustc-std-workspace-core's) is `pub use base::*;`; `libc` is `#![no_core]`,
//! depends on `ws` only, and writes `extern crate ws as core;` through a recursive `cfg_if!`
//! (an internal arm, as the real one has), its module `unix` through `cfg_if!` too, and its
//! impls through an `s!` macro defined in `#[macro_use] mod macros`. A wrong fix fails:
//! - none (the record keeps the graph's list): `::core::default::Default` NotFound at 0;
//! - one that binds `core` to some crate by its name, or to the crate the glob reaches
//!   (`base`), and not to what the root's `extern crate .. as core` names: `::core` is not
//!   `ws`'s root;
//! - one that puts every crate of the graph in the list: `::base` resolves (it must not: libc
//!   does not depend on `base`);
//! - one that merges only an `extern crate` written directly at the root, not through a
//!   macro's own recursive call: NotFound at 0 again.
//!
//! Expected: `cargo test -p blaze-scope -- tests::libc_core_records` prints
//! "test tests::libc_core_records::a_no_core_crates_macro_written_core_reads_the_same_from_records ... ok".
//! Control (resolve.rs: the `added_externs` merge into `crate_root_record.extern_prelude` off):
//! the assertion "`::core::default::Default` from `unix`, records" fails with
//! `Err(PathError { .., at: 0, why: NotFound })`.

use std::format;
use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_surface::{Def, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::Texts;
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::names::{Why, Written, path};
use crate::names_record::{LoadNames, RecordNames, encode};
use crate::resolve::{CrateInput, CrateNames, Names, aligned};
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

const BASE: &str = "#![no_core]
pub mod default { pub trait Default {} }
pub mod marker { pub trait Copy {} }
";

const WS: &str = "#![no_core]
pub use base::*;
";

const LIBC: &str = "#![no_core]
#[macro_use]
mod macros;
cfg_if! { if { extern crate ws as core; } }
cfg_if! { if { mod unix; } }
";

const LIBC_MACROS: &str = "macro_rules! cfg_if {
    (if { $($t:tt)* }) => { cfg_if! { @__identity $($t)* } };
    (@__identity $($t:tt)*) => { $($t)* };
}
macro_rules! s {
    ($(pub struct $name:ident {})*) => ($(
        pub struct $name {}
        #[allow(deprecated)]
        impl ::core::default::Default for $name {}
    )*);
}
";

const LIBC_UNIX: &str = "s! { pub struct timespec {} pub struct stat {} }
";

static TEXTS: Texts = Texts(&[
    &[("/base/lib.rs", BASE)],
    &[("/ws/lib.rs", WS)],
    &[("/libc/lib.rs", LIBC), ("/libc/macros.rs", LIBC_MACROS), ("/libc/unix.rs", LIBC_UNIX)],
]);

const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

const WS_DEPS: &[(&str, usize)] = &[("base", 0)];
const LIBC_DEPS: &[(&str, usize)] = &[("ws", 1)];

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-scope test crate", name.as_bytes())),
        edition: blaze_surface::Edition::E2021,
        implicit: RUST,
        root,
        shown_from: "/",
        features: &[],
        deps,
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

#[test]
fn a_no_core_crates_macro_written_core_reads_the_same_from_records() {
    let g = blaze_own::testing::heap().alloc();
    let inputs = [input("base", "/base/lib.rs", &[]), input("ws", "/ws/lib.rs", WS_DEPS), input("libc", "/libc/lib.rs", LIBC_DEPS)];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), g).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), g)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);

    let libc_root: ItemKey = fresh.root(2);
    let ws_root: ItemKey = fresh.root(1);
    let plain = |segments: &'static [&'static str]| Written { kind: PathKind::Plain, segments };
    let abs = |segments: &'static [&'static str]| Written { kind: PathKind::Abs, segments };
    // The trait itself, named in its own crate without any extern prelude.
    let want: Def = fresh
        .item_at(fresh.root(0), Namespace::Types, plain(&["default", "Default"]), g)
        .unwrap_or_else(|e| panic!("`default::Default` in base: {e}"))
        .def;
    // The macro-written module the impls are in (libc's `unix::bsd::apple` in the real crate).
    let unix: ItemKey = fresh
        .item_at(libc_root, Namespace::Types, plain(&["unix"]), g)
        .unwrap_or_else(|e| panic!("`unix`, written by `cfg_if!`: {e}; notes {:?}", fresh.notes(2)))
        .def
        .key;

    let mut wrong = Vec::new();
    for (place, module) in [("the root", libc_root), ("`unix`", unix)] {
        let segments: &'static [&'static str] = &["core", "default", "Default"];
        let a = path(&fresh, module, Namespace::Types, abs(segments), g);
        let b = path(&read, module, Namespace::Types, abs(segments), g);
        if a.map(|r| r.whole(3)) != Ok(Some(want)) {
            wrong.push(format!("`::core::default::Default` from {place}, fresh: {a:?} (want {want:?})"));
        }
        if b.map(|r| r.whole(3)) != Ok(Some(want)) {
            wrong.push(format!("`::core::default::Default` from {place}, records: {b:?} (want {want:?})"));
        }
        // `core` is what the root's `extern crate ws as core` names: ws's root, not base's.
        let core = path(&read, module, Namespace::Types, abs(&["core"]), g);
        if core.map(|r| r.whole(1)) != Ok(Some(Def::module(ws_root))) {
            wrong.push(format!("`::core` from {place}, records: {core:?} (want ws's root {ws_root:?})"));
        }
        // `base` is in no list of libc's: not a dependency, not written by an `extern crate`.
        let base = path(&read, module, Namespace::Types, abs(&["base", "default", "Default"]), g);
        if base.map_err(|e| (e.at, e.why)) != Err((0, Why::NotFound)) {
            wrong.push(format!("`::base::default::Default` from {place}, records: {base:?} (want NotFound at 0)"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
