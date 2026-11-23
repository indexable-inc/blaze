//! A type written `/// doc` + `#[derive(..)] #[helper(..)]` in a DEPENDENCY has its head read,
//! so an impl whose header names it (`impl PartialEq<CallKind> for &str`) is a candidate that
//! answers (lane OC, 2026-11-17).
//!
//! tempo's build census g051231 (tree 847c0fc770): REACH fell from 107,822 to 72,465; every
//! large SPLIT row ended in the head of one such type refused, "the expansion writes another
//! crate's `$crate` and keeps no origin column: .. origin row 1 (0..39) is out of order after one
//! ending at 39": alloy-rpc-types-trace's `CallKind` (2,442 fns behind it), alloy-eip7928's
//! `AccountChanges`, commonware's `U64`, tempo's `SignatureType`, protobuf's `Error`. The derive's
//! helper attribute keeps the item through a keep call (blaze-scope resolve.rs `keep_item`),
//! whose identity column gave each token of the doc comment (`# [ doc = ".." ]`, all at the
//! comment's bytes) its own row: the column did not read, so no head the text declares did.
//!
//! The census's path (`Front2Program::survey_from`, as tests/front2_prim_self_ops.rs): `app`
//! calls `dep`'s fns, whose bodies ask `&str: PartialEq<CallKind>` (and the same of a type with a
//! lifetime, a lifetime's `'` and name also sharing one place). Expected: nothing left out.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::front2::program::Front2Program;
use blaze_own::{AVec, Alloc};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::names::Written;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const CORE: &str = "#![no_core]
#![feature(no_core, lang_items)]
pub mod marker {
    #[lang = \"pointee_sized\"]
    pub trait PointeeSized {}
    #[lang = \"meta_sized\"]
    pub trait MetaSized: PointeeSized {}
    #[lang = \"sized\"]
    pub trait Sized: MetaSized {}
    #[lang = \"copy\"]
    pub trait Copy {}
}
pub mod cmp {
    #[lang = \"eq\"]
    pub trait PartialEq<Rhs = Self> {
        fn eq(&self, other: &Rhs) -> bool;
    }
}
pub mod prelude {
    pub mod v1 {
        pub use crate::marker::{Copy, Sized};
        pub use crate::cmp::PartialEq;
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

/// A derive declaring the helper `show` (serde's `Serialize` and `#[serde(..)]`).
const MACROS: &str = "#![no_core]
#[proc_macro_derive(Show, attributes(show))]
pub fn show(item: TokenStream) -> TokenStream { item }
";

/// alloy's shape: a doc comment, a derive with a helper attribute, and `impl PartialEq<CallKind>
/// for &str`; and the same with a lifetime in the head.
const DEP: &str = "#![no_std]
pub trait Shown {}
/// The kind of a call, as a tracer names it.
#[derive(macros::Show)]
#[show(rename_all = \"UPPERCASE\")]
pub enum CallKind {
    Call,
    StaticCall,
}
impl core::cmp::PartialEq<CallKind> for &str {
    fn eq(&self, other: &CallKind) -> bool {
        true
    }
}
/// A borrowed name.
#[derive(macros::Show)]
#[show(skip)]
pub struct Named<'a> {
    name: &'a u8,
}
impl<'a> core::cmp::PartialEq<Named<'a>> for &str {
    fn eq(&self, other: &Named<'a>) -> bool {
        true
    }
}
pub fn is_call(s: &str) -> bool {
    core::cmp::PartialEq::eq(&s, &CallKind::Call)
}
pub fn is_named<'a>(s: &str, n: &Named<'a>) -> bool {
    core::cmp::PartialEq::eq(&s, n)
}
";

const APP: &str = "#![no_std]
pub fn go(n: &dep::Named<'_>) {
    let _ = dep::is_call(\"CALL\");
    let _ = dep::is_named(\"n\", n);
}
";

/// World indexes: 0 `app` (the program), 1 `core`, 2 `dep`, 3 `macros` (the proc-macro crate).
const APP_AT: usize = 0;
const CORE_AT: usize = 1;
const DEP_AT: usize = 2;
const MACROS_AT: usize = 3;

const RUNNER: CrateItemKey = CrateItemKey(ContentId([15; 32]));

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
            (DEP_AT, "/dep/lib.rs") => Some(DEP),
            (MACROS_AT, "/macros/lib.rs") => Some(MACROS),
            _ => None,
        }
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == MACROS_AT).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        if !matches!((call.kind, def.proc_name()), (ProcKind::Derive, Some("Show"))) {
            return Err(refused);
        }
        // What the derive writes is not this test's subject: one impl of the item's crate's trait.
        let input = call.input;
        let text = if input.tokens().iter().any(|t| input.text_of(t) == "CallKind") { "impl crate::Shown for CallKind {}" } else { "impl<'a> crate::Shown for Named<'a> {}" };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)], proc_macro: bool) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder kept item heads crate", name.as_bytes())),
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

fn names() -> &'static Names<'static> {
    let crates = [
        input("app", "/app/lib.rs", &[("core", CORE_AT), ("dep", DEP_AT)], false),
        input("core", "/core/lib.rs", &[], false),
        input("dep", "/dep/lib.rs", &[("core", CORE_AT), ("macros", MACROS_AT)], false),
        input("macros", "/macros/lib.rs", &[], true),
    ];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let names = resolve_crates(&crates, sysroot, &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

#[test]
fn a_kept_item_s_head_reads_in_an_impl_header_of_a_dependency() {
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
    assert!(left.is_empty(), "fns left out that check clean by Rust's rules (before OC: the head of `CallKind` refused, origin row 1 out of order): {left:#?}");
    println!("kept heads: nothing left out");
}
