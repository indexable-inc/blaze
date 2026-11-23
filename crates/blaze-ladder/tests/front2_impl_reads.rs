//! A selected impl's REGIONS are in the reading body's read list (front2 borrow_reads.rs
//! `impl_read`, `BorrowHead` detail 6; rule 21, keys cover every input).
//!
//! Two readers of the impl `impl<'a> Tr for Wrap<'a>`:
//!
//! - `reads` calls `take::<Wrap<'_>>`, whose output `T::A` the type check selects at that impl
//!   (a `Checked::selected` row at the call); the borrow check lays the call's output through
//!   the impl's `type A` column (`RegionFacts::impl_assoc`).
//! - `get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8` uses the impl ONLY in its own signature: the
//!   type check files that projection's row at the body's root (blaze-typeck machine.rs
//!   `sig_rows`), in the same table, and the borrow check lays `x` through the impl (design B).
//!
//! The type check's own reads hold the impl's TYPES only: `type A = Inner<'a>` and `type A =
//! Inner<'static>` erase to the same type, so before this row an edit of that lifetime left
//! both bodies' keys unchanged while their borrow answers could change: a stale answer under
//! an unchanged key.
//!
//! - BASE: `type A = Inner<'a>`. STATIC: the same with `type A = Inner<'static>`. For each
//!   body the detail-6 row about the impl DIFFERS, and so does its digest; its type check's
//!   rows are EQUAL (the control: without the row the key would not move).
//! - TWIN (rule 23): BASE and OTHER differ only in that lifetime of an UNRELATED impl
//!   (`impl<'b> Tr for Other<'b>`, selected by neither body): each body's detail-6 rows and its
//!   digest are EQUAL. So the row moves with the impl the body read, not with any edit.
//!
//! Prints `IMPL READS: the selected impl's lifetime moves the key of a call and of a signature;
//! an unrelated impl's does not`. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::reads::{BodyReads, FactKind, Read};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "2024";

const BASE: &str = "fn main() {}
pub trait Tr { type A; }
pub struct Wrap<'a>(pub &'a u8);
pub struct Other<'b>(pub &'b u8);
pub struct Inner<'a>(pub &'a u8);
impl<'a> Tr for Wrap<'a> { type A = Inner<'a>; }
impl<'b> Tr for Other<'b> { type A = Inner<'b>; }
fn take<T: Tr>(t: T) -> T::A { let _ = t; loop {} }
fn reads() {
    let x = 1u8;
    let _i = take(Wrap(&x));
}
fn get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8 { x.0 }
";

const STATIC: &str = "fn main() {}
pub trait Tr { type A; }
pub struct Wrap<'a>(pub &'a u8);
pub struct Other<'b>(pub &'b u8);
pub struct Inner<'a>(pub &'a u8);
impl<'a> Tr for Wrap<'a> { type A = Inner<'static>; }
impl<'b> Tr for Other<'b> { type A = Inner<'b>; }
fn take<T: Tr>(t: T) -> T::A { let _ = t; loop {} }
fn reads() {
    let x = 1u8;
    let _i = take(Wrap(&x));
}
fn get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8 { x.0 }
";

const OTHER: &str = "fn main() {}
pub trait Tr { type A; }
pub struct Wrap<'a>(pub &'a u8);
pub struct Other<'b>(pub &'b u8);
pub struct Inner<'a>(pub &'a u8);
impl<'a> Tr for Wrap<'a> { type A = Inner<'a>; }
impl<'b> Tr for Other<'b> { type A = Inner<'static>; }
fn take<T: Tr>(t: T) -> T::A { let _ = t; loop {} }
fn reads() {
    let x = 1u8;
    let _i = take(Wrap(&x));
}
fn get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8 { x.0 }
";

/// One body's read list and digest in one version.
struct Seen {
    rows: Vec<Read>,
    digest: blaze_ids::ContentId,
}

/// What one version gave for `reads` (the call) and `get` (the signature).
struct Version {
    reads: Seen,
    get: Seen,
}

fn version(text: &'static str) -> Version {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let find = |n: &str| {
        bodies
            .iter()
            .find(|(b, _)| matches!(b, BodyRef::Free(k) if names.item_text(*k).and_then(|t| t.name) == Some(n)))
            .map(|(b, _)| *b)
            .unwrap_or_else(|| panic!("`{n}` listed"))
    };
    let mut seen = |n: &str| -> Seen {
        let (types, borrows) = world.check_borrows(find(n), heap.alloc());
        assert!(types.is_clean(), "`{n}`'s types are clean: {types:?}");
        assert!(borrows.findings.is_empty(), "`{n}` has no borrow finding: {borrows:?}");
        let list: &BodyReads<'_> = types.facts.as_ref().unwrap_or_else(|e| panic!("`{n}`'s read list: {e}"));
        let digest = types.reads.clone().unwrap_or_else(|| panic!("`{n}`'s finished check has a digest: {types:?}"));
        Seen { rows: list.rows.iter().copied().collect(), digest }
    };
    let reads = seen("reads");
    let get = seen("get");
    Version { reads, get }
}

fn impl_rows(rows: &[Read]) -> Vec<Read> {
    rows.iter().filter(|r| r.fact.kind == FactKind::BorrowHead && r.fact.detail == 6).copied().collect()
}

fn typeck_rows(rows: &[Read]) -> Vec<Read> {
    rows.iter().filter(|r| !matches!(r.fact.kind, FactKind::BorrowAdt | FactKind::BorrowHead)).copied().collect()
}

/// `name`'s three versions: the selected impl's lifetime moves its row and digest (the type
/// check's rows equal), an unrelated impl's moves neither.
fn moves_with_the_impl(name: &str, base: &Seen, stat: &Seen, other: &Seen) {
    let (b6, s6, o6) = (impl_rows(&base.rows), impl_rows(&stat.rows), impl_rows(&other.rows));
    assert_eq!(b6.len(), 1, "`{name}` selected one impl (`Wrap`'s), so one detail-6 row: {b6:?}");
    assert_eq!(s6.len(), 1, "STATIC: `{name}` has one detail-6 row: {s6:?}");
    assert_eq!(b6[0].fact, s6[0].fact, "`{name}`: the same impl in both versions");
    assert_ne!(b6[0].answer, s6[0].answer, "`{name}`: `type A = Inner<'a>` vs `Inner<'static>` moves the impl's row");
    assert_eq!(typeck_rows(&base.rows), typeck_rows(&stat.rows), "CONTROL `{name}`: the type check's rows are equal, so without the row the key would not move");
    assert_ne!(base.digest, stat.digest, "`{name}`'s digest moved with the selected impl's lifetime");

    assert_eq!(b6, o6, "TWIN `{name}`: an unrelated impl's lifetime moves no detail-6 row");
    assert_eq!(base.digest, other.digest, "TWIN `{name}`: an unrelated impl's lifetime leaves the digest equal");
}

#[test]
fn a_selected_impls_lifetime_moves_the_readers_key() {
    let base = version(BASE);
    let stat = version(STATIC);
    let other = version(OTHER);
    moves_with_the_impl("reads", &base.reads, &stat.reads, &other.reads);
    moves_with_the_impl("get", &base.get, &stat.get, &other.get);
    println!("IMPL READS: the selected impl's lifetime moves the key of a call and of a signature; an unrelated impl's does not");
}
