//! The borrow check's reads are in the body's read list (front2 borrow_reads.rs; rule 21, keys
//! cover every input): a change that moves only what the move and borrow check reads moves the
//! body's read-list digest (`BodyVerdict::reads`), so a body's borrow notes cannot outlive it.
//!
//! Three versions of one program, each body checked by `ProjectWorld::check_borrows`:
//!
//! - BASE: `Token` is not `Copy`; `pick` takes `x: &u8` and returns `&u8` (elided).
//! - COPY: BASE plus `impl Clone for Token` and `impl Copy for Token {}`. The Reference, "Moved
//!   and copied types": "If the type of that value implements Copy, then the value will be
//!   copied ... the value is moved" otherwise; so `moves`'s `let b = a;` is a copy here and a
//!   move in BASE: its borrow verdict reads `Token`'s `Copy` answer. Its TYPE check asks no
//!   question about `Copy` (a move needs no trait), so the type check's rows of `moves` are
//!   EQUAL in BASE and COPY (asserted: the control, that without the borrow rows the list
//!   would not move), and only the `BorrowAdt` row about `Token` differs. Without the fix
//!   (borrows.rs filing no row) the two digests are equal and this test fails at
//!   "the digest of `moves` moved".
//! - LIFE: BASE with `pick`'s head written `fn pick<'a>(x: &'a u8) -> &'a u8`. The Reference,
//!   "Lifetime elision": a single input lifetime is given to the elided output, so both heads
//!   mean one relation, but the borrow check reads the head's region record (D148), which a
//!   written lifetime moves: `calls`'s `BorrowHead` row about `pick` differs.
//!
//! Prints `BORROW READS: moves typeck rows equal, Token's row moved, digest moved; calls pick's
//! head row moved, digest moved`. Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld};
use blaze_ladder::front2::reads::{BodyReads, FactKind, Read, Reach};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

const BASE: &str = "fn main() {}

struct Token {
    n: u8,
}

fn pick(x: &u8) -> &u8 {
    x
}

fn moves() {
    let a = Token { n: 1 };
    let b = a;
    let n = b.n;
}

fn calls() {
    let v = 1u8;
    let r = pick(&v);
    let k = *r;
}
";

const COPY: &str = "fn main() {}

struct Token {
    n: u8,
}

fn pick(x: &u8) -> &u8 {
    x
}

fn moves() {
    let a = Token { n: 1 };
    let b = a;
    let n = b.n;
}

fn calls() {
    let v = 1u8;
    let r = pick(&v);
    let k = *r;
}

impl Clone for Token {
    fn clone(&self) -> Token {
        *self
    }
}

impl Copy for Token {}
";

const LIFE: &str = "fn main() {}

struct Token {
    n: u8,
}

fn pick<'a>(x: &'a u8) -> &'a u8 {
    x
}

fn moves() {
    let a = Token { n: 1 };
    let b = a;
    let n = b.n;
}

fn calls() {
    let v = 1u8;
    let r = pick(&v);
    let k = *r;
}
";

/// One body's list and digest in one version.
struct Seen {
    rows: Vec<Read>,
    digest: blaze_ids::ContentId,
}

/// What one version gave for `moves` and `calls`.
struct Version {
    moves: Seen,
    calls: Seen,
}

fn seen(name: &str, verdict: &BodyVerdict<'_>) -> Seen {
    assert!(verdict.is_clean(), "`{name}`'s types are clean: {verdict:?}");
    let list: &BodyReads<'_> = verdict.facts.as_ref().unwrap_or_else(|e| panic!("`{name}`'s read list: {e}"));
    assert_eq!(list.reach, Reach::Whole, "`{name}`'s list is whole: {verdict:?}");
    assert!(list.rows.windows(2).all(|w| w[0].fact.bytes() < w[1].fact.bytes()), "`{name}`'s list is sorted with no fact twice: {:?}", list.rows);
    let digest = verdict.reads.clone().unwrap_or_else(|| panic!("`{name}`'s finished check has a digest: {verdict:?}"));
    Seen { rows: list.rows.iter().copied().collect(), digest }
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
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
    let (moves, borrows) = world.check_borrows(find("moves"), heap.alloc());
    assert!(borrows.is_clean(), "`moves` is borrow checked clean: {borrows:?}");
    let moves = seen("moves", &moves);
    let (calls, borrows) = world.check_borrows(find("calls"), heap.alloc());
    assert!(borrows.findings.is_empty(), "`calls` has no borrow finding: {borrows:?}");
    let calls = seen("calls", &calls);
    Version { moves, calls }
}

fn of_kind(rows: &[Read], kind: FactKind) -> Vec<Read> {
    rows.iter().filter(|r| r.fact.kind == kind).copied().collect()
}

fn keys_of(rows: &[Read]) -> Vec<u128> {
    rows.iter().map(|r| r.fact.key).collect()
}

fn typeck_rows(rows: &[Read]) -> Vec<Read> {
    rows.iter().filter(|r| !matches!(r.fact.kind, FactKind::BorrowAdt | FactKind::BorrowHead)).copied().collect()
}

#[test]
fn borrow_reads_move_the_body_digest() {
    let base = version(BASE);
    let copy = version(COPY);
    let life = version(LIFE);

    // COPY: only `Token`'s `Copy` answer moved. Each ADT the borrow check asks is two facts:
    // detail 0 (`borrow_reads::adt_read`) and detail 7 (`adt_facts::ADT_FACTS_DETAIL`).
    assert_eq!(typeck_rows(&base.moves.rows), typeck_rows(&copy.moves.rows), "the control: the type check's rows of `moves` do not see `impl Copy for Token`");
    let detail = |rows: &[Read], d: u64| -> Vec<Read> { rows.iter().filter(|r| r.fact.detail == d).copied().collect() };
    let (adts, copy_adts) = (of_kind(&base.moves.rows, FactKind::BorrowAdt), of_kind(&copy.moves.rows, FactKind::BorrowAdt));
    let (a, b) = (detail(&adts, 0), detail(&copy_adts, 0));
    let (a7, b7) = (detail(&adts, 7), detail(&copy_adts, 7));
    assert_eq!(adts.len(), a.len() + a7.len(), "every ADT row is detail 0 or 7: {adts:?}");
    assert_eq!(copy_adts.len(), b.len() + b7.len(), "every ADT row is detail 0 or 7: {copy_adts:?}");
    assert_eq!(a.len(), 1, "`moves` names one ADT, `Token`: {a:?}");
    assert_eq!(b.len(), 1, "`moves` names one ADT, `Token`: {b:?}");
    assert_eq!(a[0].fact, b[0].fact, "one fact about `Token` in both versions");
    assert_ne!(a[0].answer, b[0].answer, "`Token`'s row moved when it became `Copy`");
    assert_eq!(keys_of(&a7), keys_of(&a), "an adt-facts row for `Token`: {a7:?} beside {a:?}");
    assert_eq!(keys_of(&b7), keys_of(&b), "an adt-facts row for `Token`: {b7:?} beside {b:?}");
    assert_ne!(base.moves.digest, copy.moves.digest, "the digest of `moves` moved");

    // LIFE: only `pick`'s head text moved. Each head the borrow check asks is three facts:
    // detail 0, its lifetime part and callee facts (borrow_reads.rs), detail 1, its declared
    // type-outlives bounds (call_generics.rs), and detail 4, its trait bounds' lifetime ties
    // (borrows_ties.rs `DETAIL`, piece RB8). LIFE writes a lifetime and no bound, so only a
    // detail-0 row moves; the detail-1 and detail-4 rows name the same heads and stay equal.
    const TIES: u64 = 4;
    const IMPLIED: u64 = 8;
    let (heads, life_heads) = (of_kind(&base.calls.rows, FactKind::BorrowHead), of_kind(&life.calls.rows, FactKind::BorrowHead));
    let (a, b) = (detail(&heads, 0), detail(&life_heads, 0));
    let (a1, b1) = (detail(&heads, 1), detail(&life_heads, 1));
    let (a4, b4) = (detail(&heads, TIES), detail(&life_heads, TIES));
    let (a8, b8) = (detail(&heads, IMPLIED), detail(&life_heads, IMPLIED));
    assert_eq!(heads.len(), a.len() + a1.len() + a4.len() + a8.len(), "every head row is detail 0, 1, 4 or 8: {heads:?}");
    assert_eq!(a8, b8, "the owner's implied-bounds row stayed equal: {a8:?} then {b8:?}");
    assert_eq!(keys_of(&a4), keys_of(&a), "a ties row for each head read: {a4:?} beside {a:?}");
    assert_eq!(a4, b4, "no trait bound changed, so no ties row moved: {a4:?} then {b4:?}");
    assert_eq!(a.len(), 2, "`calls` reads two heads, its own and `pick`'s: {a:?}");
    assert_eq!(b.len(), 2, "`calls` reads two heads, its own and `pick`'s: {b:?}");
    let keys = |rows: &[Read]| -> Vec<u128> { rows.iter().map(|r| r.fact.key).collect() };
    assert_eq!(keys(&a1), keys(&a), "a call-bounds row for each head read: {a1:?} beside {a:?}");
    assert_eq!(a1, b1, "no type-outlives bound changed, so no call-bounds row moved: {a1:?} then {b1:?}");
    let moved: Vec<(Read, Read)> = a.iter().zip(b.iter()).filter(|(x, y)| x != y).map(|(x, y)| (*x, *y)).collect();
    assert_eq!(moved.len(), 1, "exactly one head row moved (`pick`'s), `calls`'s own stayed: {a:?} then {b:?}");
    assert_eq!(moved[0].0.fact, moved[0].1.fact, "the moved row is one fact in both versions: {moved:?}");
    assert_ne!(base.calls.digest, life.calls.digest, "the digest of `calls` moved");
    // `moves` reads nothing of `pick`: its list is the same in BASE and LIFE.
    assert_eq!(base.moves.digest, life.moves.digest, "`moves` reads no head `pick`'s text moved");

    println!("BORROW READS: moves typeck rows equal, Token's row moved, digest moved; calls pick's head row moved, digest moved");
}
