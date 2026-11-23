#![feature(allocator_api)]
//! ADT INFERRED OUTLIVES AND A FN'S IMPLIED BOUNDS (D190 point 2, fact B: front2
//! adt_outlives.rs, adt_facts.rs, borrows.rs `owner_implied_input`): each rule on a reduced
//! program, with its NEGATIVE TWIN (the same shape where the bound does not follow). The answer
//! is read as the borrow check's facts show it (`implied ..`, borrows.rs `show_facts`), so this
//! test holds before the lowering takes it (scratch d190p2-B-flow-lower-implied-bounds.txt);
//! the findings it changes are front2_variance_a.rs's kind of test once it does.
//!
//! In the answer, `(p, r)` is `T_p: r` (`p` the type parameter's index among the non-lifetime
//! generics, parents first) and `(long, short)` is `'long: 'short`; `Param(i)` the owner's
//! lifetime `i`.
//!
//! - RFC 2093: `struct S<'a, T> { x: &'a T }` infers `T: 'a`; through a wrapper too; through a
//!   recursive type; a declared `where T: 'a` is part of the requirements. Twin: an ADT with
//!   `PhantomData<&'a ()>` and a bare `T` requires nothing.
//! - RFC 1214: a fn assumes its inputs and output well-formed: `W<'a, T>` as an input gives
//!   `T: 'a`; `S<'a, &'b u8>` gives `'b: 'a` (`T: 'a` at `T = &'b X`); nested references give
//!   both; tuples, slices and arrays componentwise; `&'static T` gives `T: 'static`.
//! - An impl's method assumes the impl header's types well-formed: `impl<'a, T> S<'a, T>`
//!   gives its methods `T: 'a`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_mem::{Root, RootOpts};

// Plain Rust (the fixture writes `Option<Box<L<'a, T>>>`): under a blaze edition `std` is
// bstd, whose `Box<T, A>` takes its allocator with no default.
const EDITION: &str = "2024";

const MAIN: &str = "use std::marker::PhantomData;

fn main() {}

struct S<'a, T> {
    x: &'a T,
}

struct W<'a, T>(S<'a, T>);

struct N<'a, T>(PhantomData<&'a ()>, T);

struct E<'a, T>
where
    T: 'a,
{
    t: T,
    p: PhantomData<&'a ()>,
}

struct L<'a, T> {
    next: Option<Box<L<'a, T>>>,
    v: &'a T,
}

struct M<'a, T> {
    next: Option<Box<M<'a, T>>>,
    v: T,
    p: PhantomData<&'a ()>,
}

fn via_adt<'a, T>(w: W<'a, T>) -> usize {
    let _ = w;
    0
}

fn via_none<'a, T>(n: N<'a, T>) -> usize {
    let _ = n;
    0
}

fn via_decl<'a, T>(e: E<'a, T>) -> usize {
    let _ = e;
    0
}

fn via_rec<'a, T>(l: L<'a, T>) -> usize {
    let _ = l;
    0
}

fn via_rec_none<'a, T>(m: M<'a, T>) -> usize {
    let _ = m;
    0
}

fn nest<'a, 'b>(s: S<'a, &'b u8>) -> usize {
    let _ = s;
    0
}

fn nest_no<'a, 'b>(s: S<'a, u8>, g: &'b u8) -> usize {
    let _ = (s, g);
    0
}

fn refs<'a, 'b, T>(x: &'a &'b T) -> usize {
    let _ = x;
    0
}

fn refs_no<'a, 'b, T>(x: &'a T, y: &'b u8) -> usize {
    let _ = (x, y);
    0
}

fn parts<'a, 'b, 'c, T>(x: &'a (&'b u8, [T]), y: &'c [&'b u8; 2]) -> usize {
    let _ = (x, y);
    0
}

fn parts_no<'a, 'b, T>(x: &'a (u8, [u8]), y: (&'b u8, T)) -> usize {
    let _ = (x, y);
    0
}

fn stat<T>(x: &'static T) -> usize {
    let _ = x;
    0
}

fn stat_no<'a, T>(x: &'a T) -> usize {
    let _ = x;
    0
}

impl<'a, T> S<'a, T> {
    fn assoc() -> usize {
        0
    }
}

impl<'a, T> N<'a, T> {
    fn assoc_n() -> usize {
        0
    }
}
";

/// The test's heap: one root, made here and kept for the process.
fn root() -> &'static Root {
    let root = Root::new(RootOpts::TEST_ARENA).unwrap_or_else(|e| panic!("the test's root: {e}"));
    Root::leak(root).unwrap_or_else(|e| panic!("the test's root, kept for the process: {e}"))
}

/// The `(params, regions)` lists of the `implied Ok(Some(([..], [..])))` line, as text.
fn implied_lists(shown: &str) -> Option<(String, String)> {
    let line = shown.lines().find(|l| l.starts_with("implied "))?;
    let rest = line.strip_prefix("implied Ok(Some(([")?;
    let (params, rest) = rest.split_once("], [")?;
    let regions = rest.strip_suffix("])))")?;
    Some((params.to_string(), regions.to_string()))
}

#[test]
fn inferred_outlives_and_implied_bounds() {
    let root = root();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/ib/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "ib", dir: "/ib/", root: "/ib/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, root.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(root.alloc());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
    // One body's implied bounds: (params, regions), as text.
    let mut implied = |n: &str| -> (String, String) {
        let b = find(n);
        let mut shown = String::new();
        let (types, _) = world.check_borrows_shown(b, &mut shown, root.alloc());
        assert!(types.is_clean(), "{n}'s types: {types:?}");
        let line = shown.lines().find(|l| l.starts_with("implied ")).unwrap_or("<no implied line>").to_string();
        println!("{n}: {line}");
        implied_lists(&shown).unwrap_or_else(|| panic!("{n}: the implied bounds were not computed: {line}"))
    };
    let has = |(p, r): &(String, String), param: Option<&str>, region: Option<&str>, n: &str| {
        if let Some(x) = param {
            assert!(p.contains(x), "{n}: `{x}` not among the implied type bounds [{p}]");
        }
        if let Some(x) = region {
            assert!(r.contains(x), "{n}: `{x}` not among the implied region bounds [{r}]");
        }
    };
    let lacks = |(p, r): &(String, String), param: Option<&str>, region: Option<&str>, n: &str| {
        if let Some(x) = param {
            assert!(!p.contains(x), "{n} (twin): `{x}` among the implied type bounds [{p}]");
        }
        if let Some(x) = region {
            assert!(!r.contains(x), "{n} (twin): `{x}` among the implied region bounds [{r}]");
        }
    };
    const T_A: &str = "(0, Param(0))";

    // RFC 2093 through a wrapper; the twin requires nothing of `T`.
    has(&implied("via_adt"), Some(T_A), None, "via_adt");
    lacks(&implied("via_none"), Some(T_A), None, "via_none");
    // A declared `where T: 'a` is a requirement of every use.
    has(&implied("via_decl"), Some(T_A), None, "via_decl");
    // Through a recursive type (the group's fixed point); its twin holds `T` bare.
    has(&implied("via_rec"), Some(T_A), None, "via_rec");
    lacks(&implied("via_rec_none"), Some(T_A), None, "via_rec_none");
    // `T: 'a` at `T = &'b u8` is `'b: 'a`.
    has(&implied("nest"), None, Some("(Param(1), Param(0))"), "nest");
    lacks(&implied("nest_no"), None, Some("(Param(1), Param(0))"), "nest_no");
    // Nested references: `&'a &'b T` gives `'b: 'a`, `T: 'b` and `T: 'a`.
    let r = implied("refs");
    has(&r, Some("(0, Param(1))"), Some("(Param(1), Param(0))"), "refs");
    has(&r, Some(T_A), None, "refs");
    let r = implied("refs_no");
    lacks(&r, Some("(0, Param(1))"), Some("(Param(1), Param(0))"), "refs_no");
    // Componentwise: a tuple's and a slice's parts under `&'a`, an array's under `&'c`.
    let p = implied("parts");
    has(&p, Some(T_A), Some("(Param(1), Param(0))"), "parts");
    has(&p, None, Some("(Param(1), Param(2))"), "parts");
    let p = implied("parts_no");
    lacks(&p, Some(T_A), Some("(Param(1), Param(0))"), "parts_no");
    // `&'static T` gives `T: 'static`.
    has(&implied("stat"), Some("(0, Static)"), None, "stat");
    lacks(&implied("stat_no"), Some("(0, Static)"), None, "stat_no");
    // A member assumes its impl's header, with no `self` in its signature: `S<'a, T>` gives
    // `T: 'a` (the impl's `'a` is the member's lifetime 0); the twin impl's header `N<'a, T>`
    // gives nothing of `T`.
    has(&implied("assoc"), Some(T_A), None, "assoc");
    lacks(&implied("assoc_n"), Some(T_A), None, "assoc_n");
}
