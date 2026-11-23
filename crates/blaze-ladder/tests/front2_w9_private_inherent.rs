//! Method lookup skips an inherent method the caller cannot see (the Reference, "Visibility
//! and privacy"; rustc's probe files it as a private candidate and goes on): reth's
//! `Tx<RW>` has a private `put(&self, kind: PutKind, key, value)` beside `DbTxMut::put(&self,
//! key, value)`, and a call `tx.put::<T>(k, v)` from another crate is the trait's. Where no
//! visible method applies, the call is E0624 (a private method), never E0061.

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-clo-04-generic-fn/src/prelude.rs");
const HEADER: &str = r#"#![feature(no_core, lang_items, intrinsics, macro_derive, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
mod prelude;
use prelude::*;
"#;

fn check(source: &str, cases: &[(&str, &[&str])]) {
    let source = [HEADER, source].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &source },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
    ];
    let alloc = blaze_own::testing::heap().alloc();
    let tc = Toolchain::none(alloc);
    let new = NewFront { package: "private_inherent", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).expect("fixture texts");
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).expect("fixture crates");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
    let mut world = ProjectWorld::new(&names, &frame, &files, alloc).expect("front2");
    let bodies = world.bodies(alloc);
    for &(name, expected) in cases {
        let body = bodies.iter().find_map(|&(body, _)| match body {
            BodyRef::Free(key) if names.item_text(key)?.name == Some(name) => Some(body),
            _ => None,
        }).unwrap_or_else(|| panic!("missing fixture function `{name}`"));
        let verdict = world.check(body, alloc);
        assert!(verdict.reads.is_some() && verdict.gaps.is_empty() && verdict.refused.is_none() && verdict.undecided.is_empty(), "{name}: {verdict:?}");
        let codes: Vec<_> = verdict.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes.as_slice(), expected, "{name}: {verdict:?}");
    }
}

#[test]
fn a_private_inherent_method_yields_to_a_visible_trait_method() {
    // reth crates/storage/db/src/implementation/mdbx/tx.rs, cut to its shape.
    const SOURCE: &str = r#"
mod store {
    pub struct Tx;
    enum PutKind { Upsert }
    impl Tx {
        fn put(&self, kind: PutKind, key: u8, value: u8) {}
        fn secret(&self) {}
    }
    pub trait DbTxMut {
        fn put(&self, key: u8, value: u8);
    }
    impl DbTxMut for Tx {
        fn put(&self, key: u8, value: u8) { self.put(PutKind::Upsert, key, value) }
    }
    // In the impl's module the inherent method is visible, so it is still the one taken.
    fn inside(tx: &Tx) { tx.put(PutKind::Upsert, 1, 2) }
}
use store::DbTxMut;
fn outside(tx: &store::Tx) { tx.put(1, 2) }
// Control: a private method with no visible one of its name stays an error, E0624.
fn hidden(tx: &store::Tx) { tx.secret() }
"#;
    check(SOURCE, &[("inside", &[]), ("outside", &[]), ("hidden", &["E0624"])]);
    println!("inside: clean, outside: clean, hidden: E0624");
}

#[test]
fn restricted_methods_are_seen_where_their_restriction_reaches() {
    const SOURCE: &str = r#"
mod a {
    pub mod b {
        pub struct S;
        impl S {
            pub(super) fn up(&self) {}
            pub(crate) fn anywhere(&self) {}
        }
    }
    fn in_parent(s: &b::S) { s.up() }
}
fn in_crate(s: &a::b::S) { s.anywhere() }
// Control: `pub(super)` from `b` reaches `a` and its modules, not the crate's root.
fn past_super(s: &a::b::S) { s.up() }
"#;
    check(SOURCE, &[("in_parent", &[]), ("in_crate", &[]), ("past_super", &["E0624"])]);
    println!("in_parent: clean, in_crate: clean, past_super: E0624");
}
