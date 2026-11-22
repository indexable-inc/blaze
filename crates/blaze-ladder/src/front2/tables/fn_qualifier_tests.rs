use blaze_consteval::FnKind;
use blaze_id::ContentId;
use blaze_typeck::TypeAttrs;

use super::super::head_tests::{a, world_of};
use super::{FrozenHead, freeze};
use crate::front2::head_record::{self, Head, HeadAt};
use crate::front2::heads::{FnQualifiers, Want};
use crate::front2::index::Index;
use crate::front2::keys::checker_of;
use crate::front2::receiver::Pointers;
use crate::front2::source::NamesSource;

fn facts(app: &'static str) -> (FnQualifiers, FnKind, ContentId) {
    let (names, sized, frame) = world_of(app);
    let mut key = None;
    names.each_item(0, &mut |item, _| {
        if names.name_of(item) == Some("f") {
            key = Some(item);
        }
    });
    let lib = key.expect("the app declares f");
    let text = names.text_of(lib).expect("the body of f").text;
    let made = freeze(names, sized, Pointers::NOT_LOOKED_UP, Want::Fn(lib), frame, a(), a()).expect("freeze f");
    let frozen = FrozenHead::read(&made.bytes).expect("the frozen cell of f");
    let entry = frozen.heads().find(|entry| entry.at == HeadAt::Fn(checker_of(lib))).expect("the function's head");
    let tys = crate::front2::rows::stable_tys(a()).expect("a fresh type table");
    let Head::Fn { head, .. } = head_record::decode(entry.record, Some(text), &tys, a()).expect("decode f") else {
        panic!("f is a function")
    };
    let index = Index::from_source(names, &[]).unwrap_or_else(|_| panic!("no duplicate lang item"));
    let count = names.head_lowerings().expect("each record reader counts its lowerings");
    let before = count.total();
    let result = (head.qualifiers, head.const_call(names, &index), frozen.meaning());
    assert_eq!(count.total(), before, "reading f's frozen qualifiers must not lower a head");
    result
}

#[test]
fn frozen_call_qualifiers_follow_const_and_cfg() {
    let (on, konst, active) = facts("#![no_std]\n#![feature(rustc_attrs)]\n#[cfg_attr(all(), rustc_promotable)] pub const fn f() -> u8 { 1 }");
    let (off, same, inactive) = facts("#![no_std]\n#![feature(rustc_attrs)]\n#[cfg_attr(any(), rustc_promotable)] pub const fn f() -> u8 { 1 }");
    let (plain, ordinary, runtime) = facts("#![no_std]\n#![feature(rustc_attrs)]\npub fn f() -> u8 { 1 }");
    assert_eq!((on.typing(), konst), (TypeAttrs(TypeAttrs::PROMOTABLE), FnKind::Const));
    assert_eq!((off.typing(), same), (TypeAttrs(0), FnKind::Const));
    assert_eq!((plain.typing(), ordinary), (TypeAttrs(0), FnKind::NotConst));
    assert_ne!(active, inactive, "enabling a typing attribute must change the reader digest");
    assert_ne!(inactive, runtime, "removing const must change the reader digest");
    std::println!("frozen call qualifiers: cfg selects promotion, const selects const evaluation");
}

#[test]
fn frozen_internal_declaration_uses_the_active_link_name() {
    let (exact, _, unchanged) = facts(r#"#![no_std]
#![feature(rustc_attrs)]
extern "Rust" { #[rustc_std_internal_symbol] #[link_name = "f"] pub fn f(); }"#);
    let (other, _, renamed) = facts(r#"#![no_std]
#![feature(rustc_attrs)]
extern "Rust" { #[rustc_std_internal_symbol] #[cfg_attr(all(), link_name = "other")] pub fn f(); }"#);
    let (off, _, ordinary) = facts(r#"#![no_std]
#![feature(rustc_attrs)]
extern "Rust" { #[cfg_attr(any(), rustc_std_internal_symbol)] #[link_name = "other"] pub fn f(); }"#);
    assert!(exact.internal_symbol() && !exact.renamed_internal());
    assert!(other.internal_symbol() && other.renamed_internal());
    assert!(!off.internal_symbol() && !off.renamed_internal());
    assert_ne!(unchanged, renamed);
    assert_ne!(renamed, ordinary);
    std::println!("frozen internal declaration: f binds by name, a different link_name excludes it");
}

#[test]
fn frozen_callee_distinguishes_written_and_elided_lifetimes() {
    let (named, _, explicit) = facts("#![no_std]\npub fn f<'a>(x: &'a u8) -> &'a u8 { x }");
    let (elided, _, implicit) = facts("#![no_std]\n/// A caller's reference.\npub fn f(x: &u8) -> &u8 { x }");
    assert!(named.writes_lifetimes());
    assert!(!elided.writes_lifetimes());
    assert_ne!(explicit, implicit, "callee lifetime spelling is an input of the borrow check");
    std::println!("frozen callee lifetimes: written true, elided false");
}
