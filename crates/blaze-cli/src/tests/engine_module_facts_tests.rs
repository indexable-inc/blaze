//! Bodies that name things THROUGH modules and write constants as expressions, through
//! `engine-check` (front2 by_origin.rs `FactHolder`, tables/task.rs `fact_read`).
//!
//! THE FIND (g190500, the 1,100-crate workspace): 467 bodies "not checked: no head cell holds
//! the body's fact of ..: no want makes it: NoHead(.., Module)": an anonymous constant (an array
//! length written as an expression) is keyed under its module, and its fact was sent to the
//! module's head cell, which no head task makes. Before the fix `head_len` and `body_len` below
//! were such notes. A child of main.rs's tests: its helpers are theirs, and engine_check_tests.rs
//! lends `engine_from` and `entries`.
//!
//! NOT RUN by the writer (a write-only lane, uncompiled).

use super::engine_check_tests::{engine_from, entries};
use super::*;

const DEP: &str = "pub mod m {\n    pub fn f() -> u8 {\n        7\n    }\n\n    pub struct S;\n\n    pub fn arr() -> [u8; { 4 }] {\n        [1, 2, 3, 4]\n    }\n}\n";

/// [`DEP`] with an item no body names added at the end of `m`.
const DEP_UNRELATED: &str = "pub mod m {\n    pub fn f() -> u8 {\n        7\n    }\n\n    pub struct S;\n\n    pub fn arr() -> [u8; { 4 }] {\n        [1, 2, 3, 4]\n    }\n\n    pub fn unrelated() -> u8 {\n        9\n    }\n}\n";

const APP: &str = "use dep::m;
use dep::m as n;

pub fn a() -> u8 {
    m::f()
}

pub fn b() -> u8 {
    n::f()
}

pub fn through_reexport() -> u8 {
    dep2::shown::f()
}

pub fn c() -> dep::m::S {
    dep::m::S
}

mod local {
    pub fn g() {}

    pub fn seven() -> u8 {
        7
    }

    pub mod deeper {
        pub fn up() -> u8 {
            super::seven()
        }
    }
}

pub fn d() {
    local::g()
}

pub fn via_self() -> u8 {
    self::local::seven()
}

pub fn via_crate() -> u8 {
    crate::local::deeper::up()
}

macro_rules! mk {
    () => {
        pub fn mac() -> u8 {
            m::f()
        }
    };
}

mk!();

pub fn head_len() -> u8 {
    let _x = m::arr();
    0
}

pub fn body_len() -> u8 {
    let _x: [u8; { 2 }] = [1, 2];
    0
}
";

/// The bodies of [`APP`]: 13 (`g`, `seven` and `up` in `local`, `mac` from `mk!`).
const APP_BODIES: u32 = 13;

/// Three members: `dep` with `pub mod m`, `dep2` re-exporting it as `shown`, `app` naming it.
pub(super) fn three_members(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"dep\", \"dep2\", \"app\"]\nresolver = \"2\"\n");
    write(io, "dep/Cargo.toml", "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "dep/src/lib.rs", DEP);
    write(io, "dep2/Cargo.toml", "[package]\nname = \"dep2\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n");
    write(io, "dep2/src/lib.rs", "pub use dep::m as shown;\n");
    write(io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\ndep2 = { path = \"../dep2\" }\n");
    write(io, "app/src/lib.rs", APP);
}

/// EVERY BODY THROUGH A MODULE IS CHECKED ON THE FIRST REQUEST, AND AN UNRELATED ITEM ADDED TO
/// THE DEPENDENCY'S MODULE ENTERS NO BODY BUT ITS OWN: the first request has no body note (no
/// "no head cell holds", no "NoHead") and checks every body clean (`app`'s 13 and `dep`'s `f`
/// and `arr`). Then `pub fn unrelated` is added at the end of `dep::m`: the Bodies stage enters
/// exactly one body, the new one (no body of `app` reads a cell that edit moves: `m`'s `Item`
/// cell holds its head only, each binding cell its own name). CONTROL: the request after the
/// edit is not the certificate (the edit was seen: its outline and names were entered).
#[test]
fn bodies_through_modules_are_checked_and_an_unrelated_item_enters_none() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    three_members(&io);
    let (_, first) = engine_from(&io, 2, false);
    let (notes, clean) = &first.bodies;
    println!("the first request: {}, status {}, bodies {:?}", entries(&first), first.status.0, first.bodies);
    let finding: std::vec::Vec<_> = notes.iter().filter(|note| note.contains("no head cell holds") || note.contains("NoHead")).collect();
    assert!(finding.is_empty(), "bodies refused for a module's head cell (the find): {finding:?}");
    // This fixture's std is nearly empty (no `Copy` lang item), so a body that moves an ADT by value
    // gets a "borrows not checked" note: its TYPES were checked, which is what this test is about.
    let unborrowed = notes.iter().filter(|note| note.contains("borrows not checked")).count() as u32;
    let other: std::vec::Vec<_> = notes.iter().filter(|note| !note.contains("borrows not checked")).collect();
    assert!(other.is_empty(), "every body through a module is checked: notes {other:?}");
    assert!(*clean <= APP_BODIES + 2 && *clean + unborrowed >= APP_BODIES + 2, "`app`'s bodies and `dep`'s `f` and `arr` checked ({unborrowed} without borrows): {:?}", first.bodies);

    write(&io, "dep/src/lib.rs", DEP_UNRELATED);
    let (_, after) = engine_from(&io, 2, false);
    println!("an unrelated item added to `dep::m`: {}", entries(&after));
    assert!(!after.certified && after.stages[blaze_engine::Stage::Outlines.index()].entered >= 1, "CONTROL: the edit was seen: {}", entries(&after));
    let mut bodies: std::vec::Vec<(&'static str, Option<blaze_drive::Kind>, u32)> =
        after.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Bodies).map(|(group, count)| (group.cause, group.definer.map(|name| name.kind), *count)).collect();
    bodies.sort();
    // D191: `dep`'s library and its test build both hold the new fn, a row each.
    assert_eq!(bodies, [("new", None, 2)], "an unrelated item added to `dep::m` enters its own body alone, in its 2 units' rows (D191): {}", entries(&after));
    let after_other: std::vec::Vec<_> = after.bodies.0.iter().filter(|note| !note.contains("borrows not checked")).collect();
    assert!(after_other.is_empty(), "no body note after the edit: {:?}", after.bodies.0);
}
