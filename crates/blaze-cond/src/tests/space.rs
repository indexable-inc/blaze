//! A space of real shape: three targets, two features with an implication, a switch; what it
//! allows, how a witness reads, the printed form, and that two builders agree.

use std::{format, string::String};

use blaze_own::AVec;

use crate::cfg;
use crate::isop::Cover;
use crate::portable::export;
use crate::{Atom, Bdd, Cond, Limits, ParseError, Space, SpaceBuilder, VarKey};

const LINUX: &[Atom<'static>] = &[
    Atom::flag("unix"),
    Atom::pair("target_family", "unix"),
    Atom::pair("target_os", "linux"),
    Atom::pair("target_arch", "x86_64"),
];
const MAC: &[Atom<'static>] = &[
    Atom::flag("unix"),
    Atom::pair("target_family", "unix"),
    Atom::pair("target_os", "macos"),
    Atom::pair("target_arch", "aarch64"),
];
const WINDOWS: &[Atom<'static>] = &[
    Atom::flag("windows"),
    Atom::pair("target_family", "windows"),
    Atom::pair("target_os", "windows"),
    Atom::pair("target_arch", "x86_64"),
];

fn space(reversed: bool) -> Space<'static> {
    let mut b = SpaceBuilder::new_in(blaze_own::testing::heap().alloc());
    if reversed {
        b.switch(Atom::flag("test")).unwrap();
        b.implies(("tokio", "full"), ("tokio", "rt")).unwrap();
        b.target("x86_64-pc-windows-msvc", WINDOWS).unwrap();
        b.target("aarch64-apple-darwin", MAC).unwrap();
        b.target("x86_64-unknown-linux-gnu", LINUX).unwrap();
    } else {
        b.target("x86_64-unknown-linux-gnu", LINUX).unwrap();
        b.target("aarch64-apple-darwin", MAC).unwrap();
        b.target("x86_64-pc-windows-msvc", WINDOWS).unwrap();
        b.feature("tokio", "rt").unwrap();
        b.implies(("tokio", "full"), ("tokio", "rt")).unwrap();
        b.switch(Atom::flag("test")).unwrap();
    }
    b.build().unwrap()
}

fn lower(text: &str, space: &Space<'_>, bdd: &mut Bdd<'_>) -> Cond {
    let alloc = blaze_own::testing::heap().alloc();
    let expr = cfg::parse(text, alloc).unwrap_or_else(|e| panic!("`{text}`: {e}"));
    let mut absent = AVec::new_in(alloc);
    cfg::lower(&expr, space, "tokio", bdd, &mut absent).unwrap()
}

#[test]
fn valid_is_one_target_and_closed_features() {
    let space = space(false);
    let mut bdd = Bdd::new_in(space.len(), Limits::SHIPPED, blaze_own::testing::heap().alloc()).unwrap();
    let valid = space.valid_in(&mut bdd).unwrap();
    // Three targets, {}, {rt}, {rt, full}, test on or off.
    assert_eq!(bdd.sat_count(Cond::TRUE, valid).unwrap(), 18);
    let impossible = lower(r#"all(feature = "full", not(feature = "rt"))"#, &space, &mut bdd);
    assert_eq!(bdd.sat_count(impossible, valid).unwrap(), 0);
    let two_oses = lower(r#"all(target_os = "linux", target_os = "macos")"#, &space, &mut bdd);
    assert!(bdd.and(two_oses, valid).unwrap().is_false());
    // An atom no target sets is false.
    let haiku = lower(r#"target_os = "haiku""#, &space, &mut bdd);
    assert!(haiku.is_false());
}

#[test]
fn a_witness_reads_as_a_configuration() {
    let space = space(false);
    let mut bdd = Bdd::new_in(space.len(), Limits::SHIPPED, blaze_own::testing::heap().alloc()).unwrap();
    let valid = space.valid_in(&mut bdd).unwrap();
    let f = lower(r#"all(windows, feature = "full", not(test))"#, &space, &mut bdd);
    let mut witness = AVec::new_in(blaze_own::testing::heap().alloc());
    assert!(bdd.any_sat(f, valid, &mut witness).unwrap());
    let mut text = String::new();
    space.describe(&witness, &mut text).unwrap();
    assert_eq!(text, "`test` off, feature `tokio/full` on, feature `tokio/rt` on, target x86_64-pc-windows-msvc");
}

#[test]
fn the_printed_form_says_the_same_within_valid() {
    let space = space(false);
    let alloc = blaze_own::testing::heap().alloc();
    let mut bdd = Bdd::new_in(space.len(), Limits::SHIPPED, alloc).unwrap();
    let valid = space.valid_in(&mut bdd).unwrap();
    let mut cover = Cover::new_in(alloc);
    for text in [
        r#"any(target_os = "linux", target_os = "macos")"#,
        r#"all(unix, any(feature = "full", all(feature = "rt", test)))"#,
        r#"not(any(windows, target_arch = "aarch64"))"#,
        "true",
        "false",
    ] {
        let f = lower(text, &space, &mut bdd);
        let lo = bdd.and(f, valid).unwrap();
        let hi = bdd.or(f, valid.not()).unwrap();
        cover.compute(&mut bdd, lo, hi, 1 << 16).unwrap();
        let mut printed = String::new();
        cover.write_cfg(&space, "tokio", &mut printed).unwrap();
        let back = lower(&printed, &space, &mut bdd);
        let back_valid = bdd.and(back, valid).unwrap();
        assert_eq!(back_valid, lo, "`{text}` printed as `{printed}`");
        // Exact: the cover of f alone, when f is small.
        cover.compute(&mut bdd, f, f, 1 << 16).unwrap();
        let mut exact = String::new();
        cover.write_cfg(&space, "tokio", &mut exact).unwrap();
        assert_eq!(lower(&exact, &space, &mut bdd), f, "`{text}` printed exactly as `{exact}`");
        std::println!("{text}  =>  within valid: {printed};  exactly: {exact}");
    }
    let unix = lower(r#"any(target_os = "linux", target_os = "macos")"#, &space, &mut bdd);
    let lo = bdd.and(unix, valid).unwrap();
    let hi = bdd.or(unix, valid.not()).unwrap();
    cover.compute(&mut bdd, lo, hi, 1 << 16).unwrap();
    assert_eq!(cover.len(), 1, "within valid, two oses that are all the unix targets are one cube");
}

#[test]
fn two_builders_in_two_orders_write_equal_bytes() {
    let (a, b) = (space(false), space(true));
    assert_eq!(a.len(), b.len());
    let alloc = blaze_own::testing::heap().alloc();
    let mut bdd_a = Bdd::new_in(a.len(), Limits::SHIPPED, alloc).unwrap();
    let mut bdd_b = Bdd::new_in(b.len(), Limits::SHIPPED, alloc).unwrap();
    let text = r#"any(all(unix, feature = "rt"), all(windows, not(test)))"#;
    let fa = lower(text, &a, &mut bdd_a);
    let fb = lower(text, &b, &mut bdd_b);
    let (mut x, mut y) = (AVec::new_in(alloc), AVec::new_in(alloc));
    export(&mut bdd_a, &a, fa, &mut x).unwrap();
    export(&mut bdd_b, &b, fb, &mut y).unwrap();
    assert_eq!(x.as_slice(), y.as_slice());
    let unix = a.atom("tokio", Atom::flag("unix")).unwrap();
    assert_eq!(a.key(unix), VarKey::of(b"cfg:unix"));
    assert_eq!(a.by_key(VarKey::of(b"feature:tokio/rt")), a.feature("tokio", "rt"));
}

#[test]
fn malformed_predicates_are_refused_by_place() {
    let alloc = blaze_own::testing::heap().alloc();
    let err = |text: &str| cfg::parse(text, alloc).err();
    assert_eq!(err("all(unix"), Some(ParseError::End { at: 8 }));
    assert_eq!(err("foo(unix)"), Some(ParseError::UnknownFunction { at: 0 }));
    assert_eq!(err("not(a, b)"), Some(ParseError::NotArity { at: 0 }));
    assert_eq!(err("not()"), Some(ParseError::NotArity { at: 0 }));
    assert_eq!(err("a = b"), Some(ParseError::Unexpected { at: 4 }));
    assert_eq!(err(r#"feature = "x" junk"#), Some(ParseError::Unexpected { at: 14 }));
    assert_eq!(err("all(a, any(), all(),)"), None);
    let (expr, end) = cfg::parse_prefix(r#"unix, derive(Debug)"#, alloc).unwrap();
    assert_eq!((expr.ops.len(), end), (1, 4));
    let _ = format!("{:?}", expr.ops.as_slice());
}
