//! D114 rule 1, the names half: a names record is its own index. Two crates (k1 uses k0) are
//! solved; each crate's names record is written, checked once ([`Verified::check`]) and read in
//! place through [`RecordNames`] and through one [`NamesRecord`] alone. Every path of a family,
//! from every module, in both namespaces, answers as the fresh solve's `Names` does.
//!
//! The allocation claim is counted, not argued: the readers and the walk are given a counting
//! allocator, and a lookup whose every segment is a binding of the module it is looked up in
//! (no glob walked) allocates NOTHING, opening the record included (`RecordNames::record` is a
//! search and a copy of the proof). Two controls show the counter sees what it must: a lookup
//! through a glob allocates the walk's scratch, and the old reader's decode of a module scope
//! into its working form (`ScopeBuilder::from`, what `NamesRecord::scope` did on first use
//! before D114) allocates.
//!
//! The floor (rule 67) is printed, one line: `NAMES IN PLACE: open+scope+get <t> per lookup`.
//!
//! NEVER COMPILED (lane d114a-inplace, 2026-11-16): inert until `mod names_inplace;` is added
//! to tests/mod.rs by the wiring.

use std::format;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Erased;
use blaze_own::testing::Counting;
use blaze_surface::{Namespace, PathKind, ScopeBuilder};

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::names::{Written, path};
use crate::names_record::{LoadNames, NamesRecord, RecordNames, Verified, encode};
use crate::resolve::{CrateNames, Names, aligned};
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

const K0: &str = "#![no_core]
pub mod a {
    pub struct S;
    pub mod b {
        pub fn f() {}
        pub use super::S as T;
    }
}
pub mod c {
    pub use crate::a::b::*;
    pub enum E { V, W {} }
}
pub use c::E;
";

const K1: &str = "#![no_core]
pub mod m {
    pub use k0::c::E;
    pub fn g() {}
}
pub use m::g;
";

static TEXTS: Texts = Texts(&[&[("/k0/lib.rs", K0)], &[("/k1/lib.rs", K1)]]);

struct Loaded(Vec<&'static [u8]>);

// For any `'a`, not only `'static`: the reader's lifetime is its allocator's, and the counting
// allocator below lives on this test's stack.
impl<'a> LoadNames<'a> for Loaded {
    fn load(&self, krate: usize) -> Option<&'a [u8]> {
        self.0.get(krate).copied()
    }
}

/// Paths from a crate's root module whose every segment is bound directly where it is looked
/// up (the extern prelude's names included): no glob is walked. (crate, kind, segments, ns)
const DIRECT: [(usize, PathKind, &[&str], Namespace); 10] = [
    (0, PathKind::Plain, &["a"], Namespace::Types),
    (0, PathKind::Plain, &["a", "S"], Namespace::Types),
    (0, PathKind::Plain, &["a", "b", "f"], Namespace::Values),
    (0, PathKind::Plain, &["a", "b", "T"], Namespace::Types),
    (0, PathKind::Crate, &["c", "E"], Namespace::Types),
    (0, PathKind::Crate, &["c", "E", "V"], Namespace::Values),
    (0, PathKind::Plain, &["E", "W"], Namespace::Types),
    (1, PathKind::Plain, &["k0", "a", "S"], Namespace::Types),
    (1, PathKind::Abs, &["k0", "c", "E"], Namespace::Types),
    (1, PathKind::Plain, &["m", "g"], Namespace::Values),
];

/// The family every module is asked: each leaf alone, under each module, under `crate`, and
/// through `k0`.
fn family() -> Vec<(PathKind, Vec<&'static str>)> {
    let leaves = ["a", "b", "c", "m", "S", "T", "f", "g", "E", "V", "W", "k0", "missing"];
    let mut all = Vec::new();
    for n in leaves {
        all.push((PathKind::Plain, std::vec![n]));
        all.push((PathKind::Crate, std::vec![n]));
        all.push((PathKind::Abs, std::vec!["k0", n]));
        for a in ["a", "c", "m", "E", "super", "self"] {
            all.push((PathKind::Plain, std::vec![a, n]));
            all.push((PathKind::Crate, std::vec![a, n]));
            all.push((PathKind::Plain, std::vec!["k0", a, n]));
            all.push((PathKind::Plain, std::vec![a, "b", n]));
        }
    }
    all
}

#[test]
fn a_names_record_answers_in_place_with_no_allocation() {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k0", "/k0/lib.rs", &[]), input("k1", "/k1/lib.rs", &[("k0", 0)])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    for (i, c) in all.iter().enumerate() {
        assert!(c.notes.is_empty(), "crate {i}: notes {:?}", c.notes);
    }
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let records: Vec<&'static [u8]> = all.iter().map(|n| aligned(&encode(n, g).unwrap_or_else(|e| panic!("encode: {e:?}")), bump)).collect();
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let roots: Vec<ItemKey> = (0..all.len()).map(|i| fresh.root(i)).collect();

    // The readers and the walk in a counting allocator; each record checked once, up front (the
    // check a store fetch or front_load.rs makes, never a lookup's).
    let counting = Erased::new(Counting::new(g));
    let c = counting.handle();
    let read = RecordNames::new(&keys, Loaded(records.clone()), c);
    for k in 0..keys.len() {
        assert!(read.record(k).is_some(), "crate {k}'s names record does not open");
    }
    let k0 = NamesRecord::open(Verified::check(records[0]).unwrap_or_else(|e| panic!("k0's record does not check: {e:?}")));

    // 1. Direct lookups: the fresh solve's answers, and not one allocation.
    let before = counting.inner().allocations();
    let mut wrong = Vec::new();
    for (krate, kind, segments, ns) in DIRECT {
        let written = Written { kind, segments };
        let b = path(&read, roots[krate], ns, written, c);
        let a = path(&fresh, roots[krate], ns, written, g);
        if a.is_err() || a != b || b.is_ok_and(|r| r.whole(segments.len()).is_none()) {
            wrong.push(format!("`{kind:?} {}` in crate {krate}: fresh {a:?}, record {b:?}", segments.join("::")));
        }
        if krate == 0 {
            let one = path(&k0, roots[0], ns, written, c);
            if one != a {
                wrong.push(format!("`{kind:?} {}` through k0's record alone: fresh {a:?}, record {one:?}", segments.join("::")));
            }
        }
    }
    let direct_allocations = counting.inner().allocations() - before;
    assert!(wrong.is_empty(), "{wrong:?}");
    assert_eq!(direct_allocations, 0, "a lookup with no glob walked allocated: open and search must be in place");

    // 2. Every module, every path of the family, both namespaces: the fresh solve's answers.
    let mut modules: Vec<ItemKey> = all.iter().flat_map(|n| n.chains.keys().copied()).collect();
    modules.sort_unstable();
    for &m in modules.iter() {
        for ns in [Namespace::Types, Namespace::Values, Namespace::Macros] {
            for (kind, segments) in family().iter() {
                let written = Written { kind: *kind, segments: segments.as_slice() };
                let a = path(&fresh, m, ns, written, g);
                let b = path(&read, m, ns, written, g);
                if a != b {
                    wrong.push(format!("`{kind:?} {}` ({ns:?}) in {m:?}: fresh {a:?}, record {b:?}", segments.join("::")));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "the in-place record answers differently: {wrong:?}");

    // Control 1: a lookup through a glob (`c::f`: `f` reaches `c` by `pub use crate::a::b::*`)
    // allocates the walk's scratch in the counted allocator, so the counter is the walk's.
    let before = counting.inner().allocations();
    let written = Written { kind: PathKind::Plain, segments: &["c", "f"][..] };
    let through = path(&read, roots[0], Namespace::Values, written, c);
    assert_eq!(through, path(&fresh, roots[0], Namespace::Values, written, g), "`c::f` through the glob");
    assert!(through.is_ok(), "`c::f` does not resolve: {through:?}");
    assert!(counting.inner().allocations() > before, "control: a glob walk allocated nothing, so the counter is not the walk's");

    // Control 2: the reader before D114 turned a module's scope into its working form on first
    // use; that decode allocates in the counted allocator. In place, the same scope is searched
    // with none.
    let scope = k0.scope(roots[0]).unwrap_or_else(|| panic!("k0's root scope is not in its record"));
    let before = counting.inner().allocations();
    let decoded = ScopeBuilder::from(&scope, c);
    assert!(counting.inner().allocations() > before, "control: the old decode allocated nothing");
    assert_eq!(decoded.get(Namespace::Types, "a").copied(), scope.get(Namespace::Types, "a"), "the decode and the view disagree on `a`");

    // The floor (rule 67): open the record and look one name up, many times.
    const ROUNDS: u32 = 100_000;
    let proof = Verified::check(records[0]).unwrap_or_else(|e| panic!("k0: {e:?}"));
    let start = std::time::Instant::now();
    let mut hits = 0u32;
    for _ in 0..ROUNDS {
        let record = NamesRecord::open(core::hint::black_box(proof));
        if record.scope(core::hint::black_box(roots[0])).and_then(|s| s.get(Namespace::Types, core::hint::black_box("a"))).is_some() {
            hits += 1;
        }
    }
    let per = start.elapsed() / ROUNDS;
    assert_eq!(hits, ROUNDS);
    std::println!(
        "NAMES IN PLACE: open+scope+get {per:?} per lookup over {ROUNDS} rounds; {} direct lookups allocated {direct_allocations}; record {} bytes",
        DIRECT.len(),
        records[0].len()
    );
}
