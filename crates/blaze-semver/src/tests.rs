//! The SemVer 2.0.0 spec's order, Cargo's requirement examples (the Cargo book, "Specifying
//! dependencies"), every refusal by its offset, and precedence against a model with its
//! planted-bug control.

use core::cmp::Ordering;
use std::{alloc::Global, format, string::String as StdString, string::ToString, vec::Vec as StdVec};

use blaze_sim::{Broke, Config, Tape};

use crate::{
    Error, INLINE, Limits, Op, Part, Prerelease, Version, VersionReq, What,
    plant::{self, Plant},
};

fn v(text: &str) -> Version<Global> {
    match Version::parse_in(text, Limits::CARGO, Global) {
        Ok(version) => version,
        Err(error) => panic!("{text:?}: {error}"),
    }
}

fn req(text: &str) -> VersionReq<Global> {
    match VersionReq::parse_in(text, Limits::CARGO, Global) {
        Ok(req) => req,
        Err(error) => panic!("{text:?}: {error}"),
    }
}

fn version_err(text: &str) -> Error {
    match Version::<Global>::parse_in(text, Limits::CARGO, Global) {
        Ok(version) => panic!("{text:?} parsed as {version}"),
        Err(error) => error,
    }
}

fn req_err(text: &str) -> Error {
    match VersionReq::<Global>::parse_in(text, Limits::CARGO, Global) {
        Ok(req) => panic!("{text:?} parsed as {req}"),
        Err(error) => error,
    }
}

#[test]
fn the_specs_order() {
    // SemVer 2.0.0, section 11.
    let chain = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "2.0.0",
        "2.1.0",
        "2.1.1",
    ];
    for pair in chain.windows(2) {
        let (a, b) = (v(pair[0]), v(pair[1]));
        assert_eq!(a.cmp_precedence(&b), Ordering::Less, "{a} < {b}");
        assert_eq!(a.cmp(&b), Ordering::Less, "{a} < {b}");
        assert_eq!(b.cmp(&a), Ordering::Greater, "{b} > {a}");
    }
    // Build metadata: no precedence, but `Ord` still tells distinct versions apart.
    let (a, b) = (v("1.0.0+20130313144700"), v("1.0.0+exp.sha.5114f85"));
    assert_eq!(a.cmp_precedence(&b), Ordering::Equal);
    assert_eq!(a.cmp(&b), Ordering::Less, "numeric before alphanumeric");
    assert_eq!(v("1.0.0").cmp(&v("1.0.0+1")), Ordering::Less, "no metadata first");
    assert_eq!(v("1.0.0+1").cmp(&v("1.0.0+01")), Ordering::Less, "equal values, shorter first");
    assert_ne!(a, b);
    // Numeric identifiers of any length.
    assert_eq!(
        v("1.0.0-99999999999999999999").cmp(&v("1.0.0-100000000000000000000")),
        Ordering::Less
    );
}

#[test]
fn versions_print_as_parsed() {
    for text in ["0.0.0", "1.2.3", "1.2.3-alpha.1", "1.2.3+build.5", "1.0.0-rc.1+x.7.z.92"] {
        assert_eq!(v(text).to_string(), text);
    }
    let long = "1.0.0-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    assert!(v(long).pre.0.is_heap(), "past {INLINE} bytes the text is in the allocator");
    assert_eq!(v(long).to_string(), long);
    let fits = format!("1.0.0-{}", "a".repeat(INLINE));
    assert!(!v(&fits).pre.0.is_heap(), "{INLINE} bytes stay in place");
    assert_eq!(v("1.2.3"), Version::<Global>::new(1, 2, 3));
}

#[test]
fn version_refusals_name_the_offset_and_part() {
    let at = |text, at, part, what| {
        assert_eq!(version_err(text), Error { at, part, what }, "{text:?}");
    };
    at("", 0, Part::Major, What::End);
    at("1.0", 3, Part::Minor, What::End);
    at("01.0.0", 0, Part::Major, What::LeadingZero);
    at("1.00.0", 2, Part::Minor, What::LeadingZero);
    at("1.0.0-01", 6, Part::Pre, What::LeadingZero);
    at("1.0.0-", 6, Part::Pre, What::EmptyIdentifier);
    at("1.0.0-a..b", 8, Part::Pre, What::EmptyIdentifier);
    at("1.0.0+", 6, Part::Build, What::EmptyIdentifier);
    at("v1.0.0", 0, Part::Major, What::Unexpected('v'));
    at(" 1.0.0", 0, Part::Major, What::Unexpected(' '));
    at("1.0.0 ", 5, Part::Patch, What::Unexpected(' '));
    at("1.0.0-alpha_beta", 11, Part::Pre, What::Unexpected('_'));
    at("1.0.0+a+b", 7, Part::Build, What::Unexpected('+'));
    at("18446744073709551616.0.0", 0, Part::Major, What::Overflow);
    assert_eq!(v("18446744073709551615.0.0").major, u64::MAX);
    assert_eq!(v("1.0.0+001").build.as_str(), "001", "leading zeros are allowed in build metadata");
    let long = format!("1.0.0-{}", "a".repeat(2000));
    assert_eq!(
        version_err(&long).what,
        What::TooLong { len: long.len(), most: Limits::CARGO.most_len }
    );
    assert!(Prerelease::<Global>::new_in("alpha.01", Global).is_err());
    assert_eq!(Prerelease::<Global>::new_in("alpha.1", Global).unwrap().as_str(), "alpha.1");
}

/// Each requirement with versions it admits and versions it does not.
const CARGO_BOOK: &[(&str, &[&str], &[&str])] = &[
    ("^1.2.3", &["1.2.3", "1.9.9"], &["1.2.2", "2.0.0"]),
    ("1.2.3", &["1.2.3", "1.9.9"], &["1.2.2", "2.0.0"]),
    ("^1.2", &["1.2.0", "1.99.0"], &["1.1.9", "2.0.0"]),
    ("^1", &["1.0.0", "1.99.99"], &["0.9.0", "2.0.0"]),
    ("^0.2.3", &["0.2.3", "0.2.99"], &["0.2.2", "0.3.0"]),
    ("^0.2", &["0.2.0", "0.2.9"], &["0.1.9", "0.3.0"]),
    ("^0.0.3", &["0.0.3"], &["0.0.2", "0.0.4"]),
    ("^0.0", &["0.0.0", "0.0.9"], &["0.1.0"]),
    ("^0", &["0.0.0", "0.99.0"], &["1.0.0"]),
    ("~1.2.3", &["1.2.3", "1.2.9"], &["1.2.2", "1.3.0"]),
    ("~1.2", &["1.2.0", "1.2.9"], &["1.1.0", "1.3.0"]),
    ("~1", &["1.0.0", "1.9.9"], &["0.9.9", "2.0.0"]),
    ("*", &["0.0.0", "99.0.0"], &["1.0.0-alpha"]),
    ("1.*", &["1.0.0", "1.9.0"], &["0.9.0", "2.0.0"]),
    ("1.2.*", &["1.2.0", "1.2.9"], &["1.1.0", "1.3.0"]),
    (">= 1.2.0", &["1.2.0", "5.0.0"], &["1.1.9"]),
    ("> 1", &["2.0.0"], &["1.9.9"]),
    ("< 2", &["1.9.9", "0.0.1"], &["2.0.0"]),
    ("= 1.2.3", &["1.2.3"], &["1.2.4", "1.2.3-rc.1"]),
    ("=1.2", &["1.2.0", "1.2.7"], &["1.3.0"]),
    ("<= 1.2", &["1.2.9", "1.0.0"], &["1.3.0"]),
    (">= 1.2, < 1.5", &["1.2.0", "1.4.9"], &["1.1.0", "1.5.0"]),
    // The pre-release rule.
    ("^1.2.3-alpha.2", &["1.2.3-alpha.2", "1.2.3-alpha.10", "1.2.3", "1.3.0"], &["1.2.3-alpha.1", "1.2.4-alpha"]),
    (">=1.2.0-alpha, <2", &["1.2.0-beta", "1.5.0"], &["1.3.0-beta", "1.2.0-0"]),
    ("^1.2.0", &["1.3.0"], &["1.3.0-alpha", "1.2.0-alpha"]),
    ("~1.2.3-beta", &["1.2.3-beta", "1.2.3-rc", "1.2.5"], &["1.2.3-alpha", "1.2.4-rc"]),
];

#[test]
fn cargos_requirement_examples() {
    for (text, yes, no) in CARGO_BOOK {
        let r = req(text);
        for version in *yes {
            assert!(r.matches(&v(version)), "{text} admits {version}");
        }
        for version in *no {
            assert!(!r.matches(&v(version)), "{text} refuses {version}");
        }
    }
}

#[test]
fn requirements_print_as_cargo_writes_them() {
    let shown = [
        ("  >= 1.2 , <1.5.0-rc.1 ", ">=1.2, <1.5.0-rc.1"),
        ("1.2.3", "^1.2.3"),
        ("1.*", "1.*"),
        ("1.2.x", "1.2.*"),
        ("1.*.*", "1.*"),
        ("*", "*"),
        ("X", "*"),
        ("=1.2.3+build", "=1.2.3"),
        ("~0", "~0"),
    ];
    for (text, printed) in shown {
        let r = req(text);
        assert_eq!(r.to_string(), printed, "{text:?}");
        assert_eq!(req(printed), r, "{printed:?} parses back");
    }
    let r = req("1.*");
    assert_eq!((r.comparators[0].op, r.comparators[0].minor), (Op::Wildcard, None));
    assert_eq!(req(">=1.*").comparators[0].op, Op::GreaterEq, "an explicit operator stays");
    assert!(req("*").is_star());
}

#[test]
fn requirement_refusals_name_the_offset() {
    assert_eq!(req_err("").what, What::End);
    assert_eq!(req_err("*, <2"), Error { at: 1, part: Part::Requirement, what: What::WildcardNotAlone });
    assert_eq!(req_err("* 1").what, What::AfterWildcard);
    assert_eq!(req_err("1.*.3"), Error { at: 4, part: Part::Patch, what: What::AfterWildcard });
    assert_eq!(
        req_err(">=1 <2"),
        Error { at: 4, part: Part::Requirement, what: What::ExpectedComma('<') }
    );
    assert_eq!(req_err("1.0,"), Error { at: 4, part: Part::Major, what: What::End });
    assert_eq!(req_err("^01").what, What::LeadingZero);
    assert_eq!(req_err("1.2.3 - 2.0").what, What::ExpectedComma('-'));
    let many = ["1"; 33].join(",");
    assert_eq!(req_err(&many).what, What::TooManyComparators { most: 32 });
    assert_eq!(req(&["1"; 32].join(",")).comparators.len(), 32);
    let text = format!("{}", req_err(">=1 <2"));
    assert!(text.contains("byte 4"), "{text}");
}

/// A pre-release identifier of the model: numeric before alphanumeric, each by its own order.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Id {
    Num(u64),
    Alnum(StdString),
}

fn draw_ids(tape: &Tape, label: &'static str) -> StdVec<Id> {
    tape.vec(0..=3, label, |t| match t.coin("numeric") {
        true => Id::Num(t.int_in(0u64..=20, "number")),
        false => Id::Alnum(t.text_from("ab-", 1..=3, "word")),
    })
}

fn text_of(ids: &[Id]) -> StdString {
    let words: StdVec<StdString> = ids
        .iter()
        .map(|id| match id {
            Id::Num(n) => n.to_string(),
            Id::Alnum(w) => w.clone(),
        })
        .collect();
    words.join(".")
}

/// Precedence against a model built from tuples and derived orders, and display parsing back.
fn precedence(tape: &Tape) -> Result<(), Broke> {
    let draw = |tape: &Tape| {
        let nums = [0u64; 3].map(|_| tape.int_in(0u64..=2, "number"));
        let pre = draw_ids(tape, "pre");
        let mut text = format!("{}.{}.{}", nums[0], nums[1], nums[2]);
        if !pre.is_empty() {
            text = format!("{text}-{}", text_of(&pre));
        }
        (nums, pre, text)
    };
    let (an, ap, at) = draw(tape);
    let (bn, bp, bt) = draw(tape);
    let (a, b) = (v(&at), v(&bt));
    let model = (an, ap.is_empty(), &ap).cmp(&(bn, bp.is_empty(), &bp));
    if a.cmp_precedence(&b) != model {
        return Err(Broke::Oracle(format!(
            "precedence of {at} and {bt} differs from the model: {:?}, not {model:?}",
            a.cmp_precedence(&b)
        )));
    }
    if v(&a.to_string()) != a {
        return Err(Broke::Harness(format!("{at} does not print back as itself")));
    }
    Ok(())
}

#[test]
fn precedence_matches_the_model() {
    blaze_sim::check(Config::cases(600), precedence);
}

#[test]
fn the_oracle_catches_numbers_compared_as_text() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || precedence(tape));
    blaze_sim::control(Config::cases(600), Plant::Clean, Plant::NumbersAsText, "differs from the model", run);
}
