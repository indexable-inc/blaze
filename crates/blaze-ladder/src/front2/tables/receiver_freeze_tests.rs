//! A FROZEN TRAIT'S DYN VERDICT IS DECIDED (lane H9, after lane FL2's finding): the engine's
//! freeze (head.rs `freeze`, `freeze_seeded`) makes its store with the program's receiver pointer
//! types (receiver.rs `Pointers::find`), so a method `fn poll(self: Pin<&mut Self>)` is a
//! dispatchable receiver and the trait's dyn-compatibility verdict is `Compatible`. Before, the
//! store was made with `Pointers::NOT_LOOKED_UP`: every struct receiver a gap, and tempo's 27
//! `Future` and 7 `Stream` verdicts `Gap`.
//!
//! CONTROL (rule 23): the same freeze with `Pointers::NOT_LOOKED_UP` gives `Gap`.

use blaze_typeck::dyn_compat::{MemberVerdict, own_verdict};

use super::head::{FrozenHead, freeze, import};
use super::head_tests::{CORE_AT, a, cells, world_of};
use crate::front2::head_record::Head;
use crate::front2::heads::{Store, Want};
use crate::front2::index::Index;
use crate::front2::receiver::Pointers;
use crate::resolve::Paths;

/// The fixture's program: a `Pin` of its own (the fixture's core has none), and a trait whose
/// one method takes `self: Pin<&mut Self>`.
const APP: &str = "#![no_std]
#![feature(lang_items)]
#[lang = \"pin\"]
pub struct Pin<P> {
    pub pointer: P,
}
pub trait Poll {
    fn poll(self: Pin<&mut Self>) -> u32;
}
";

/// The program's crate in the fixture (head_tests.rs `APP_AT`).
const APP_AT: usize = 0;

/// The dyn verdict of the fixture's trait, frozen with the pointers found as the engine finds
/// them (engine_heads.rs `heads_of`), or with none looked up (the control).
fn verdict(looked_up: bool) -> Result<(), MemberVerdict> {
    let (names, sized, frame) = world_of(APP);
    let index = Index::from_source(names, &[APP_AT, CORE_AT]).unwrap_or_else(|_| panic!("the fixture's lang index"));
    let pointers = match looked_up {
        true => Pointers::find(&index, Paths { names }),
        false => Pointers::NOT_LOOKED_UP,
    };
    let tys = Store::new(sized, frame, a());
    let mut today = Store::new(sized, frame, a());
    let mut keys = crate::front2::keys::Keys::new();
    let mut found = None;
    // Why each head that did not freeze or import did not: named when no trait head is found
    // (first run, 2026-11-18: "froze no trait head" with nothing said).
    let mut skipped = std::vec::Vec::new();
    for w in cells(names).iter().copied() {
        // A trait is listed by its `Members` want (by_origin.rs `item_want`: its predicates and
        // its fns' signatures, which the dyn verdict reads); `Head` is its header alone.
        let (Want::Head(_) | Want::Members(_)) = w else { continue };
        let made = match freeze(names, sized, pointers, w, frame, a(), a()) {
            Ok(made) => made,
            Err(e) => {
                skipped.push(std::format!("freeze {w:?}: {e}"));
                continue;
            }
        };
        let frozen = FrozenHead::read(&made.bytes).unwrap_or_else(|e| panic!("read {w:?}: {e}"));
        // A fn head is checked against its item's text at the import (head.rs `import`): today's
        // store hands it, as head_tests.rs `a_frozen_head_imports_as_today_s_head` does (third
        // run, 2026-11-18: with no text the trait's member was `NoText` and no head imported).
        let _ = today.ensure(names, &mut keys, w);
        let mut text_of = |at: crate::front2::head_record::HeadAt| -> Option<&'static str> {
            match at {
                crate::front2::head_record::HeadAt::Fn(k) => today.fns.get(&k).map(|head| head.text),
                _ => None,
            }
        };
        let heads = match import(&frozen, &mut text_of, &tys.tys, a(), a()) {
            Ok(heads) => heads,
            Err(e) => {
                skipped.push(std::format!("import {w:?}: {e:?}"));
                continue;
            }
        };
        for (_, head) in heads.iter() {
            if let Head::Trait { head, .. } = head {
                found = Some(own_verdict(false, &head.members).map_err(|(_, why)| why));
            }
        }
    }
    found.unwrap_or_else(|| panic!("the fixture's trait froze no trait head; skipped: {skipped:#?}"))
}

/// Expected line: `receiver freeze: Pin<&mut Self> decided Compatible; not looked up: Gap`.
#[test]
fn a_pinned_receiver_is_decided_on_the_freeze_path() {
    let decided = verdict(true);
    assert_eq!(decided, Ok(()), "ORACLE: `fn poll(self: Pin<&mut Self>)` frozen with the program's pointers is not compatible");
    let control = verdict(false);
    assert!(matches!(control, Err(MemberVerdict::Gap(_))), "CONTROL: with `Pointers::NOT_LOOKED_UP` the verdict was {control:?}, not a gap");
    std::println!("receiver freeze: Pin<&mut Self> decided Compatible; not looked up: Gap");
}
