//! A crate's expansions kept between solves ([`crate::expansions`]): a crate solved again
//! after an edit expands only the calls the edit moved, and what it takes from the earlier
//! solve is what a fresh expansion gives, byte for byte.
//!
//! The rule kept: a macro's expansion is a function of its definition and its input tokens
//! (the Reference, "Macros By Example", "Transcribing": "the transcriber ... is the macro
//! expansion", and "Derive macros": a derive is given "the token stream of the item"), plus
//! the configuration the item is read under ("Conditional compilation": `cfg` attributes on
//! an item's fields are evaluated before a derive sees it). So a call whose definition, input
//! text and asked cfg atoms are unchanged has the expansion it had.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec;
use std::vec::Vec;

use blaze_own::{AVec, Alloc};

use super::entry::{Texts, input};
use crate::crate_scope::{Kept, Ready, Solved, resolve_by_records, resolve_kept};
use crate::expansions::Planted;
use crate::graph::Sysroot;
use crate::resolve::{CrateInput, aligned};
use crate::task::{ExpandPart, Par};

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The library: two builtin derives and a `macro_rules!` macro.
const DEP: &str = "#![no_core]
#[rustc_builtin_macro]
pub macro Clone($item:item) {}
#[rustc_builtin_macro]
pub macro Copy($item:item) {}
#[macro_export]
macro_rules! twice {
    ($a:ident, $b:ident) => { pub fn $a() {} pub fn $b() {} };
}
";

/// Five derive calls and one rules call. `Both`'s two derives have ONE input text (the item):
/// only the derive's name tells their keys apart.
const USER: &str = "#![no_core]
use dep::{Clone, Copy};
#[derive(Clone, Copy)]
pub struct Both;
#[derive(Clone)]
pub struct Other { a: u8 }
#[derive(Clone)]
pub struct Gated { #[cfg(on)] a: u8, b: u8 }
#[derive(Clone)]
pub struct Before;
dep::twice!(one, two);
";

/// [`USER`] after a name edit: `Before` is `After`. Every other call's text is unchanged.
const USER_RENAMED: &str = "#![no_core]
use dep::{Clone, Copy};
#[derive(Clone, Copy)]
pub struct Both;
#[derive(Clone)]
pub struct Other { a: u8 }
#[derive(Clone)]
pub struct Gated { #[cfg(on)] a: u8, b: u8 }
#[derive(Clone)]
pub struct After;
dep::twice!(one, two);
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);
static TEXTS_RENAMED: Texts = Texts(&[&[("/user/lib.rs", USER_RENAMED)], &[("/dep/lib.rs", DEP)]]);

fn crates(cfg: &'static [(&'static str, Option<&'static str>)]) -> [CrateInput<'static>; 2] {
    let mut user = input("user", "/user/lib.rs", &[("dep", 1)]);
    user.cfg = cfg;
    [user, input("dep", "/dep/lib.rs", &[])]
}

/// A store that keeps each crate's expansions set and nothing else: every crate is solved
/// again by every call (`find` finds nothing), so what a call expands is what the kept
/// expansions did not answer.
struct Sets {
    keeps: bool,
    planted: Planted,
    sets: Mutex<Vec<Option<&'static [u8]>>>,
}

impl Sets {
    fn new(keeps: bool, planted: Planted) -> Sets {
        Sets { keeps, planted, sets: Mutex::new(vec![None; 2]) }
    }

    fn set(&self, krate: usize) -> &'static [u8] {
        self.sets.lock().unwrap_or_else(|p| panic!("sets: {p}"))[krate].unwrap_or_else(|| panic!("crate {krate} told no expansions set"))
    }
}

impl Kept for Sets {
    fn find(&self, _krate: usize) -> Option<(&[u8], &[u8], &[u8])> {
        None
    }

    fn made(&self, _krate: usize, _record: &[u8], _inner: &[u8], _read: &[u8]) {}

    fn keeps_expansions(&self) -> bool {
        self.keeps
    }

    fn expansions(&self, krate: usize) -> Option<&[u8]> {
        self.sets.lock().unwrap_or_else(|p| panic!("sets: {p}"))[krate]
    }

    fn expansions_made(&self, krate: usize, set: &[u8]) {
        self.sets.lock().unwrap_or_else(|p| panic!("sets: {p}"))[krate] = Some(aligned(set, Alloc::global()));
    }

    fn expansions_planted(&self) -> Planted {
        self.planted
    }
}

/// One thread, counting the calls that were RUN (a derive, a rules macro) and the calls whose
/// key was looked up.
#[derive(Default)]
struct Counting {
    derives: AtomicUsize,
    rules: AtomicUsize,
    keyed: AtomicUsize,
}

impl Counting {
    fn run(&self) -> (usize, usize, usize) {
        (self.derives.load(Ordering::Relaxed), self.rules.load(Ordering::Relaxed), self.keyed.load(Ordering::Relaxed))
    }
}

impl Par for Counting {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        for at in 0..n {
            out.push(f(at));
        }
    }

    fn expand_part(&self, part: ExpandPart, _macro_name: &str, _size: usize) {
        let count = match part {
            ExpandPart::RunDerive => &self.derives,
            ExpandPart::RunRules => &self.rules,
            ExpandPart::Kept => &self.keyed,
            _ => return,
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
}

/// One call of [`resolve_kept`] on `texts` with `sets`: what it ran, and each crate's two
/// records.
fn solve(crates: &[CrateInput<'static>], texts: &'static Texts, sets: &Sets) -> ((usize, usize, usize), Vec<(Vec<u8>, Vec<u8>)>) {
    let counting = Counting::default();
    let (solved, _) = resolve_kept(crates, Sysroot::default(), texts, &counting, &Ready::serial(WAIT), sets, Alloc::global()).unwrap_or_else(|e| panic!("resolve_kept: {e:?}"));
    let records = solved
        .iter()
        .map(|s| match s {
            Solved::Solved(c) => (c.record.to_vec(), c.inner.to_vec()),
            Solved::Found { record, inner } => (record.to_vec(), inner.to_vec()),
        })
        .collect();
    (counting.run(), records)
}

fn cold(crates: &[CrateInput<'static>], texts: &'static Texts) -> Vec<(Vec<u8>, Vec<u8>)> {
    let (solved, _) = resolve_by_records(crates, Sysroot::default(), texts, &Counting::default(), &Ready::serial(WAIT), Alloc::global()).unwrap_or_else(|e| panic!("cold: {e:?}"));
    solved.iter().map(|c| (c.record.to_vec(), c.inner.to_vec())).collect()
}

/// The architect's test (drive.md, piece 16): re-solving a crate after a name edit expands no
/// unchanged derive again. And the round trip: the re-solve's records equal a cold solve's of
/// the edited text, and its expansions set (four entries taken from the earlier set, one made)
/// equals, byte for byte, the set a first solve of the edited text writes (every entry made).
/// The cause removed: a store that keeps no expansions runs every call again.
///
/// Prints nothing; passes as `tests::expansions::a_re_solve_after_a_name_edit_expands_no_unchanged_derive_again ... ok`.
#[test]
fn a_re_solve_after_a_name_edit_expands_no_unchanged_derive_again() {
    let crates = crates(&[]);
    let sets = Sets::new(true, Planted::None);
    let (first, _) = solve(&crates, &TEXTS, &sets);
    assert_eq!(first, (5, 1, 6), "the first solve runs its five derives and its one rules call, each keyed");
    let (again, records) = solve(&crates, &TEXTS_RENAMED, &sets);
    assert_eq!(again, (1, 0, 6), "after the rename only `After`'s derive runs: (derives, rules, keyed)");
    assert_eq!(records, cold(&crates, &TEXTS_RENAMED), "the re-solve's records are a cold solve's");
    let fresh = Sets::new(true, Planted::None);
    solve(&crates, &TEXTS_RENAMED, &fresh);
    assert!(sets.set(0) == fresh.set(0), "a set of kept and made expansions equals the set a first solve writes");
    // Unchanged text: nothing runs at all.
    let (third, _) = solve(&crates, &TEXTS_RENAMED, &sets);
    assert_eq!(third, (0, 0, 6), "an unchanged crate solved again runs no call");
    // The cause removed: no kept expansions, every call runs again.
    let none = Sets::new(false, Planted::None);
    solve(&crates, &TEXTS, &none);
    let (without, _) = solve(&crates, &TEXTS_RENAMED, &none);
    assert_eq!(without, (5, 1, 0), "without kept expansions every call runs again and none is keyed");
}

/// The read list: `Gated`'s derive asked the cfg atom `on` (its field's `#[cfg(on)]`), which is
/// not in its key. Solved again with `on` set and the same text, that one derive runs again
/// and every other call is taken; the set equals a first solve's under the new cfg.
#[test]
fn a_kept_derive_whose_asked_cfg_atom_moved_is_expanded_again() {
    let off = crates(&[]);
    let on = crates(&[("on", None)]);
    let sets = Sets::new(true, Planted::None);
    solve(&off, &TEXTS, &sets);
    let (again, records) = solve(&on, &TEXTS, &sets);
    assert_eq!(again, (1, 0, 6), "only the derive that asked `on` runs again: (derives, rules, keyed)");
    assert_eq!(records, cold(&on, &TEXTS));
    let fresh = Sets::new(true, Planted::None);
    solve(&on, &TEXTS, &fresh);
    assert!(sets.set(0) == fresh.set(0), "the set under the new cfg equals a first solve's under it");
}

/// The planted bug: a key that leaves out the attribute (the derive's name). `Both`'s `Clone`
/// and `Copy` calls then have one key and two expansions, which the set refuses to write
/// (rule 21: two values under one key), naming the rule.
#[test]
#[should_panic(expected = "two expansions under one text key")]
fn a_key_without_the_attribute_is_caught() {
    let crates = crates(&[]);
    solve(&crates, &TEXTS, &Sets::new(true, Planted::KeyWithoutTheAttribute));
}
