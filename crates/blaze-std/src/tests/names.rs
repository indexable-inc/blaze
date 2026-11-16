//! Typed names: the `const fn` checks that `path!`, `var!` and `host!` run as the crate
//! compiles are the checks `new` and `parse` run at run time.
//!
//! - [`Path::new`]'s byte-wise check refuses exactly what the rule by text (std's `split`,
//!   as it was written before the check became a `const fn`) refuses, for the same reason.
//!
//! `HostPort`'s parse rules moved with it to blaze-url (its `tests.rs`, 2026-11-16).

use blaze_sim::{Config, Tape};

use crate::{
    env::{VarName, VarNameWhy},
    net::HostPort,
    path::{Path, PathWhy},
};

/// A string of at most `most` bytes, each one of `alphabet`.
fn word(tape: &Tape, alphabet: &[u8], most: u64) -> String {
    let bytes: Vec<u8> = (0..tape.int_in(0..=most, "word len")).map(|_| *tape.one_of(alphabet, "byte")).collect();
    String::from_utf8(bytes).expect("the alphabets are ASCII")
}

/// The path rule by text, as `path.rs` wrote it before its check was a `const fn`.
fn path_by_text(raw: &str) -> Option<PathWhy> {
    if raw.is_empty() {
        return Some(PathWhy::Empty);
    }
    if raw.starts_with('/') || raw.as_bytes().get(1) == Some(&b':') {
        return Some(PathWhy::Rooted);
    }
    if raw.contains(['\\', '\0']) {
        return Some(PathWhy::Forbidden);
    }
    for part in raw.split('/') {
        match part {
            ".." => return Some(PathWhy::Parent),
            "" | "." => return Some(PathWhy::NotCanonical),
            _ => {}
        }
    }
    None
}

#[test]
fn a_path_is_refused_as_the_rule_by_text_refuses_it() {
    blaze_sim::check(Config::cases(500), |tape| {
        let raw = word(tape, b"a./\\:\0", 8);
        let got = Path::new(&raw).err().map(|error| error.why);
        assert_eq!(got, path_by_text(&raw), "Path::new({raw:?})");
    });
}

#[test]
fn literals_are_checked_as_the_crate_compiles() {
    assert_eq!(crate::path!("data/log"), Path::new("data/log").expect("a path"));
    assert_eq!(crate::var!("HOME"), VarName::new("HOME").expect("a variable name"));
    assert_eq!(crate::host!("api.stripe.com:443"), HostPort::new("api.stripe.com", 443).expect("a host"));
    let why = |raw: &str| VarName::new(raw).err().map(|error| error.why);
    assert_eq!((why(""), why("A=B"), why("A\0")), (Some(VarNameWhy::Empty), Some(VarNameWhy::Equals), Some(VarNameWhy::Nul)));
}
