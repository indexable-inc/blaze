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

#[test]
fn type_outlives_subject_change_moves_clauses_read_digest() {
    const WITH_PARAM: &str = "fn main() {}

fn check_outlives<'a, 'k, T>(x: &'a ())
where
    (&'a (), T): 'k,
{
    let _ = x;
}
";
    const WITHOUT_PARAM: &str = "fn main() {}

fn check_outlives<'a, 'k, T>(x: &'a ())
where
    &'a (): 'k,
{
    let _ = x;
}
";
    let run = |text: &'static str| -> Seen {
        let heap = blaze_own::testing::heap();
        let home = std::env::var("HOME").expect("HOME is set");
        let shelf = format!("{home}/.blaze/v2/toolchain");
        let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
        let files = [("/outlives-read/src/main.rs".to_string(), text.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
        let user = UserCrate { name: "outlives_read", dir: "/outlives-read/", root: "/outlives-read/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let sources = [SourceFile { path: "src/main.rs", text }];
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
        let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
        let bodies = world.bodies(heap.alloc());
        let target = bodies
            .iter()
            .find(|(b, _)| match b {
                BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name) == Some("check_outlives"),
                BodyRef::Member { name, .. } => *name == "check_outlives",
            })
            .map(|(b, _)| *b)
            .expect("`check_outlives` listed");
        let (types, borrows) = world.check_borrows(target, heap.alloc());
        assert!(borrows.is_clean(), "`check_outlives` is borrow checked clean: {borrows:?}");
        seen("check_outlives", &types)
    };
    const CLAUSES_DETAIL: u64 = 11;
    let a = run(WITH_PARAM);
    let b = run(WITHOUT_PARAM);
    let a11: Vec<Read> = of_kind(&a.rows, FactKind::BorrowHead).into_iter().filter(|r| r.fact.detail == CLAUSES_DETAIL).collect();
    let b11: Vec<Read> = of_kind(&b.rows, FactKind::BorrowHead).into_iter().filter(|r| r.fact.detail == CLAUSES_DETAIL).collect();
    assert!(!a11.is_empty(), "clauses_read rows recorded for `check_outlives`: {a11:?}");
    assert_eq!(keys_of(&a11), keys_of(&b11), "same BorrowHead detail-11 fact keys: {a11:?} then {b11:?}");
    let moved: Vec<(Read, Read)> = a11.iter().zip(b11.iter()).filter(|(x, y)| x != y).map(|(x, y)| (*x, *y)).collect();
    assert_eq!(
        moved.len(),
        1,
        "changing `(&'a (), T): 'k` to `&'a (): 'k` (same slots and RHS, different subject type) must move `check_outlives`'s clauses_read digest: {a11:?} then {b11:?}"
    );
    assert_ne!(a.digest, b.digest, "body read digest must change when TypeOutlives subject type changes");
}

#[test]
fn fn_ptr_abi_and_wide_tuple_clause_subjects_move_clauses_read_digest() {
    let run = |text: String| -> Seen {
        let heap = blaze_own::testing::heap();
        let home = std::env::var("HOME").expect("HOME is set");
        let shelf = format!("{home}/.blaze/v2/toolchain");
        let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
        let files = [("/clause-hash/src/main.rs".to_string(), text.clone())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
        let user = UserCrate { name: "clause_hash", dir: "/clause-hash/", root: "/clause-hash/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let sources = [SourceFile { path: "src/main.rs", text: text.as_str() }];
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
        let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
        let bodies = world.bodies(heap.alloc());
        let target = bodies
            .iter()
            .find(|(b, _)| match b {
                BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name) == Some("target"),
                BodyRef::Member { name, .. } => *name == "target",
            })
            .map(|(b, _)| *b)
            .expect("`target` listed");
        let (types, borrows) = world.check_borrows(target, heap.alloc());
        assert!(borrows.is_clean(), "`target` is borrow checked clean: {borrows:?}");
        seen("target", &types)
    };
    const CLAUSES_DETAIL: u64 = 11;

    // 1. Function-pointer ABI change (`fn(&'a ())` vs `extern "C" fn(&'a ())`) must move clauses_read.
    let rust_abi = run("fn main() {}\nfn target<'a, 'k>(x: &'a ()) where fn(&'a ()): 'k { let _ = x; }\n".to_string());
    let c_abi = run("fn main() {}\nfn target<'a, 'k>(x: &'a ()) where extern \"C\" fn(&'a ()): 'k { let _ = x; }\n".to_string());
    let r11: Vec<Read> = of_kind(&rust_abi.rows, FactKind::BorrowHead).into_iter().filter(|r| r.fact.detail == CLAUSES_DETAIL).collect();
    let c11: Vec<Read> = of_kind(&c_abi.rows, FactKind::BorrowHead).into_iter().filter(|r| r.fact.detail == CLAUSES_DETAIL).collect();
    assert_eq!(keys_of(&r11), keys_of(&c11), "same detail-11 keys for fn ptr ABI test");
    let moved_abi: Vec<_> = r11.iter().zip(c11.iter()).filter(|(x, y)| x != y).collect();
    assert_eq!(moved_abi.len(), 1, "changing fn ptr ABI must move `target`'s clauses_read digest: {r11:?} then {c11:?}");

    // 2. Wide 130-element tuple differing only in element #129 (`u8` vs `u16`) must move clauses_read.
    let mut prefix = String::from("&'a ()");
    for _ in 1..129 {
        prefix.push_str(", u8");
    }
    let wide_a = run(format!("fn main() {{}}\nfn target<'a, 'k>(x: &'a ()) where ({prefix}, u8): 'k {{ let _ = x; }}\n"));
    let wide_b = run(format!("fn main() {{}}\nfn target<'a, 'k>(x: &'a ()) where ({prefix}, u16): 'k {{ let _ = x; }}\n"));
    let wa11: Vec<Read> = of_kind(&wide_a.rows, FactKind::BorrowHead).into_iter().filter(|r| r.fact.detail == CLAUSES_DETAIL).collect();
    let wb11: Vec<Read> = of_kind(&wide_b.rows, FactKind::BorrowHead).into_iter().filter(|r| r.fact.detail == CLAUSES_DETAIL).collect();
    assert_eq!(keys_of(&wa11), keys_of(&wb11), "same detail-11 keys for wide tuple test");
    let moved_wide: Vec<_> = wa11.iter().zip(wb11.iter()).filter(|(x, y)| x != y).collect();
    assert_eq!(moved_wide.len(), 1, "changing element #129 of a 130-element tuple must move `target`'s clauses_read digest: {wa11:?} then {wb11:?}");
}

#[test]
fn unread_owner_regions_still_files_clauses_read_row() {
    // Two explicit trait-object lifetime bounds cause `heads_regions` to refuse the fn's
    // region record while typeck still checks clean, exercising the `Err(_)` arm for
    // `owner_regions.outlives(owner)` and verifying that detail-11 `clauses_read` is still
    // filed and moves once the signature becomes readable.
    const UNREAD: &str = "fn main() {}

fn unread_owner(_: &'static (dyn Send + 'static + 'static)) {}
";
    const READABLE: &str = "fn main() {}

fn unread_owner(_: &'static (dyn Send + 'static))
where
    &'static (): 'static,
{}
";
    let run = |text: &'static str| -> Seen {
        let heap = blaze_own::testing::heap();
        let home = std::env::var("HOME").expect("HOME is set");
        let shelf = format!("{home}/.blaze/v2/toolchain");
        let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
        let files = [("/unread-owner/src/main.rs".to_string(), text.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
        let user = UserCrate { name: "unread_owner", dir: "/unread-owner/", root: "/unread-owner/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let sources = [SourceFile { path: "src/main.rs", text }];
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
        let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
        let bodies = world.bodies(heap.alloc());
        let target = bodies
            .iter()
            .find(|(b, _)| match b {
                BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name) == Some("unread_owner"),
                BodyRef::Member { name, .. } => *name == "unread_owner",
            })
            .map(|(b, _)| *b)
            .expect("`unread_owner` listed");
        let (types, _borrows) = world.check_borrows(target, heap.alloc());
        seen("unread_owner", &types)
    };
    const CLAUSES_DETAIL: u64 = 11;
    let unread = run(UNREAD);
    let readable = run(READABLE);
    let u11: Vec<Read> = of_kind(&unread.rows, FactKind::BorrowHead).into_iter().filter(|r| r.fact.detail == CLAUSES_DETAIL).collect();
    let r11: Vec<Read> = of_kind(&readable.rows, FactKind::BorrowHead).into_iter().filter(|r| r.fact.detail == CLAUSES_DETAIL).collect();
    assert!(!u11.is_empty(), "unread owner region facts must still file a detail-11 clauses_read row: {u11:?}");
    assert_eq!(keys_of(&u11), keys_of(&r11), "same BorrowHead detail-11 keys across unread -> readable recovery: {u11:?} then {r11:?}");
    let moved: Vec<_> = u11.iter().zip(r11.iter()).filter(|(x, y)| x != y).collect();
    assert_eq!(moved.len(), 1, "recovering unread owner region facts must move its detail-11 clauses_read row: {u11:?} then {r11:?}");
    assert_ne!(unread.digest, readable.digest, "body read digest must move when unread owner clauses become readable");
}
