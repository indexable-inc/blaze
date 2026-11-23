//! A KEPT ITEM'S HEAD IN A DEPENDENCY reads on the census path (lanes KI and OC, 2026-11-17).
//! tempo's census g051231 (tree 847c0fc770) left out 5,007 fns (107,822 reached before, 72,465
//! after): `LEFT 3944 check gap: RefusedImpl` and `LEFT 295 check: Head: Item: Program(..)`, each
//! ending "an expansion's origin row 1 (0..39) is out of order after one ending at 39", for the
//! heads of `alloy_rpc_types_trace::geth::call::CallKind` and the like: types written `///
//! doc` `#[derive(..)] #[helper(..)]`, kept by their keep call (blaze-scope resolve.rs
//! `keep_item`). The kept text's identity column had a row per TOKEN of `input_places`, and a
//! `///` comment lexes to `# [ doc = ".." ]`, every token at the comment's bytes: rows repeating
//! one range, a column that does not read.
//!
//! Here the dependency `dep` declares `Kind` so (a doc comment, a derive with a helper, the
//! helper on the item), and `impl PartialEq<Kind> for &str` reads `Kind`'s head in its header;
//! the program's crate `app` reaches both through `Front2Program::survey_from`, as blaze-real's
//! `tempo_build` does. Nothing is left out.
//!
//! The derive is a built-in one `core` declares with a helper, as the pinned library declares
//! `Default` (core/src/default.rs `#[rustc_builtin_macro(Default, attributes(default))]`).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::front2::program::Front2Program;
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::names::Written;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const CORE: &str = "#![no_core]
#![feature(no_core, lang_items, decl_macro, rustc_attrs)]
pub mod marker {
    #[lang = \"pointee_sized\"]
    pub trait PointeeSized {}
    #[lang = \"meta_sized\"]
    pub trait MetaSized: PointeeSized {}
    #[lang = \"sized\"]
    pub trait Sized: MetaSized {}
    #[lang = \"copy\"]
    pub trait Copy: crate::clone::Clone {}
    impl Copy for u8 {}
    /// The derive, declaring the helper `show` (as serde's `Serialize` declares `serde`).
    #[rustc_builtin_macro(Copy, attributes(show))]
    pub macro Copy($item:item) {}
}
pub mod clone {
    #[lang = \"clone\"]
    pub trait Clone: crate::marker::Sized {
        fn clone(&self) -> Self;
    }
    impl Clone for u8 {
        fn clone(&self) -> u8 {
            *self
        }
    }
}
pub mod cmp {
    #[lang = \"eq\"]
    pub trait PartialEq<Rhs = Self> {
        fn eq(&self, other: &Rhs) -> bool;
    }
}
pub mod prelude {
    pub mod v1 {
        pub use crate::clone::Clone;
        pub use crate::cmp::PartialEq;
        pub use crate::marker::{Copy, Sized};
    }
    pub mod rust_2015 {
        pub use super::v1::*;
    }
    pub mod rust_2018 {
        pub use super::v1::*;
    }
    pub mod rust_2021 {
        pub use super::v1::*;
    }
    pub mod rust_2024 {
        pub use super::v1::*;
    }
}
";

/// `CallKind`'s shape: a doc comment, a derive with a helper, the helper on the item; its head
/// read by an impl header of a primitive self.
const DEP: &str = "#![no_std]
/// The kind of a call, as `alloy_rpc_types_trace::geth::call::CallKind` is documented.
#[derive(core::marker::Copy)]
#[show(rename_all = \"UPPERCASE\")]
pub struct Kind {
    pub a: u8,
}
impl Clone for Kind {
    fn clone(&self) -> Kind {
        *self
    }
}
impl PartialEq<Kind> for &str {
    fn eq(&self, other: &Kind) -> bool {
        let _ = other.a;
        true
    }
}
pub fn field(k: &Kind) -> u8 {
    k.a
}
pub fn is_kind(s: &str, k: &Kind) -> bool {
    PartialEq::eq(&s, k)
}
";

const APP: &str = "#![no_std]
pub fn go() {
    let k = dep::Kind { a: 1 };
    let _ = dep::field(&k);
    let _ = dep::is_kind(\"call\", &k);
}
";

/// World indexes: 0 `app` (the program), 1 `core`, 2 `dep`.
const APP_AT: usize = 0;
const CORE_AT: usize = 1;
const DEP_AT: usize = 2;

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
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
        key: CrateItemKey(ContentId::of("blaze-ladder kept item head crate", name.as_bytes())),
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
    let crates = [
        input("app", "/app/lib.rs", &[("core", CORE_AT), ("dep", DEP_AT)]),
        input("core", "/core/lib.rs", &[]),
        input("dep", "/dep/lib.rs", &[("core", CORE_AT)]),
    ];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let names = resolve_crates(&crates, sysroot, &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

/// Rule: a kept item's text keeps an origin column that reads (blaze-expand origin_column.rs:
/// rows sorted, each range once), so `Kind`'s head resolves and every fn the survey reaches from
/// `app::go` checks: none is left out. Expected "1 passed". CONTROL: on green 0c73bddbca (a row
/// per token in blaze-scope expansion_origins.rs `kept`), the impl member `eq` and its callers
/// are left out with "an expansion's origin row 1 (..) is out of order".
#[test]
fn a_kept_item_with_a_doc_comment_in_a_dependency_reads_its_head() {
    let names = names();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame")));
    let segments = ["go"];
    let written = Written { kind: PathKind::Plain, segments: &segments[..] };
    let found = blaze_scope::names::path(names, names.root(APP_AT), Namespace::Values, written, a()).unwrap_or_else(|e| panic!("`app::go`: {e:?}"));
    let roots = [found.whole(1).unwrap_or_else(|| panic!("`app::go` names no fn")).key];
    let mut left_out = Vec::new_in(Global);
    let _program = Front2Program::survey_from(names, frame, &[], &roots, &mut left_out, None, a()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<(String, String)> = left_out.iter().map(|(f, class)| (f.to_string(), class.to_string())).collect();
    for (f, class) in left.iter() {
        println!("LEFT {f}: {class}");
    }
    assert!(left.is_empty(), "fns left out that check clean by Rust's rules: {left:#?}");
}
