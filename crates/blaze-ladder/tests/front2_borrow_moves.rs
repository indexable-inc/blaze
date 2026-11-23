//! False "a use of a value after it was moved" (E0382) findings on tempo, by cause, each with
//! its twin (rule 23). Every ORACLE case is accepted by rustc; every TWIN refused by it.
//!
//! 1. A closure is `Copy` when it captures nothing by mutable reference and every value it
//!    captures by value is `Copy` (the Reference, "Closure types", "Other traits").
//!    blaze-flow-lower `is_copy` answered `false` for every closure, so a closure passed by
//!    value twice was a use after move (tempo zones/prover connection.rs:123..151, twelve
//!    places; alloy rpc/reth_compat.rs:230, :231, :243; zones/checker runtime.rs:416, :435,
//!    :491, :525).
//!    - `closures`: a closure capturing nothing and one capturing a `u8` by reference, each
//!      passed by value twice: clean.
//!    - TWIN `owned`: a `move` closure holding a `String`, passed twice: E0382.
//!    - TWIN `counter`: a closure capturing `n` by mutable reference, passed twice: E0382.
//! 2. Operands are evaluated left to right, and a place operand is read where it is evaluated
//!    (the Reference, "Evaluation order of operands"). The lowering read every place operand at
//!    the call, after the later operands ran (tempo zones/sequencer proofs.rs:486, :540:
//!    `proofs.insert(proof.n, Arc::new(proof))`).
//!    - `order`: `m.insert(p.n, Box::new(p))`: clean.
//!    - TWIN `order_late`: `(Box::new(p), p.n)`: E0382 (the read comes after the move).
//!    - TWIN `order_missed`: `(p, p.s.len())`: E0382 (the move comes first; before, the borrow
//!      of `p.s` was made before a move at the aggregate's end, and nothing was found).
//! 4. A binding by value of a `&mut T` under a guard: the Reference, "Match guards" and
//!    "Binding modes". `Some(count)` over `Option<&mut usize>` binds `count` by value; the
//!    lowering took any binding of a reference type as one by reference (lib.rs `guard_moves`),
//!    so it moved `count` before the guard's fake borrow of the scrutinee (tempo
//!    zones-antithesis checks.rs:434, zones/node role.rs:767).
//!    - `take`: `match counts.get_mut(&key) { Some(count) if *count > 0 => .. }`: clean.
//!    - TWIN `take_then_use`: the scrutinee used after the arm moved its part: E0382.
//! 3. A type parameter bounded `Copy` in a `where` clause or its own list is `Copy` (the
//!    Reference, "Copy types"). The ladder's `Facts::param_copy` answered `false` for every
//!    parameter (tempo payload/types samples.rs:55, :57; estimator/build_time.rs:128, :160,
//!    :161). Its fix is the ladder's (scratch edit `ladder-param-copy-edit.txt`), so this case
//!    is its own test, red until that edit lands.
//!    - `twice_where`, `twice_inline`: `(x, x)` with `T: Copy`: clean.
//!    - TWIN `twice_unbounded`: `(x, x)` with no bound: E0382.
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::collections::BTreeMap;

fn closures(a: Option<u8>, b: Result<u8, u8>) -> Result<u8, u8> {
    let max = 3u8;
    let none = || 0u8;
    let lim = |e: u8| e + max;
    let x = a.ok_or_else(none)?;
    let y = b.map_err(lim)?;
    let _ = a.ok_or_else(none)?;
    let _ = b.map_err(lim)?;
    Ok(x + y)
}

fn owned(a: Option<u8>) -> u8 {
    let s = String::from("x");
    let f = move || s.len() as u8;
    let x = a.unwrap_or_else(f);
    let y = a.unwrap_or_else(f);
    x + y
}

fn counter(a: Option<u8>) -> u8 {
    let mut n = 0u8;
    let f = || {
        n += 1;
        n
    };
    let x = a.unwrap_or_else(f);
    let y = a.unwrap_or_else(f);
    x + y
}

pub struct P {
    pub n: u64,
    pub s: String,
}

fn order(m: &mut BTreeMap<u64, Box<P>>, p: P) {
    m.insert(p.n, Box::new(p));
}

fn order_late(p: P) -> (Box<P>, u64) {
    (Box::new(p), p.n)
}

fn order_missed(p: P) -> (P, usize) {
    (p, p.s.len())
}

fn take(counts: &mut BTreeMap<u8, usize>, key: u8) -> bool {
    match counts.get_mut(&key) {
        Some(count) if *count > 0 => {
            *count -= 1;
            true
        }
        _ => false,
    }
}

fn take_then_use(o: Option<&mut u8>) -> u8 {
    match o {
        Some(c) if *c > 0 => {
            *c -= 1;
            let _d = o;
            1
        }
        _ => 0,
    }
}

fn twice_where<T>(x: T) -> (T, T)
where
    T: Copy,
{
    (x, x)
}

fn twice_inline<T: Copy>(x: T) -> (T, T) {
    (x, x)
}

fn twice_unbounded<T>(x: T) -> (T, T) {
    (x, x)
}

fn main() {}
"#;

type Verdict = (Vec<&'static str>, Option<BorrowRefused>);

fn verdicts(names_wanted: &[&str]) -> Vec<(String, Verdict)> {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/moves/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "moves",
        dir: "/moves/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut out = Vec::new();
    for &name in names_wanted {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect("fixture body");
        let (types, borrows) = world.check_borrows(BodyRef::Free(key), alloc);
        assert!(types.is_clean(), "{name}'s types: {types:?}");
        let codes: Vec<&'static str> = borrows.findings.iter().map(|d| d.code).collect();
        std::println!("{name}: codes {codes:?}, refused {:?}", borrows.refused.as_ref().map(|r| std::format!("{r}")));
        out.push((name.to_owned(), (codes, borrows.refused)));
    }
    out
}

fn of<'r>(rows: &'r [(String, Verdict)], n: &str) -> &'r Verdict {
    &rows.iter().find(|r| r.0 == n).expect("checked above").1
}

#[test]
fn copy_closures_and_operand_order_make_no_false_move() {
    let rows = verdicts(&["closures", "owned", "counter", "order", "order_late", "order_missed", "take", "take_then_use"]);

    for n in ["closures", "order", "take"] {
        let (codes, refused) = of(&rows, n);
        assert!(!codes.contains(&"E0382"), "ORACLE `{n}`: rustc accepts it; no E0382: {codes:?} {refused:?}");
    }
    for n in ["owned", "counter", "order_late", "order_missed", "take_then_use"] {
        let (codes, refused) = of(&rows, n);
        assert!(codes.contains(&"E0382"), "TWIN `{n}`: rustc reports E0382; so must the check: {codes:?} {refused:?}");
    }
}

#[test]
fn where_copy_params_are_copied_needs_ladder_edit() {
    let rows = verdicts(&["twice_where", "twice_inline", "twice_unbounded"]);
    for n in ["twice_where", "twice_inline"] {
        let (codes, refused) = of(&rows, n);
        assert!(!codes.contains(&"E0382"), "ORACLE `{n}`: `T: Copy`; no E0382: {codes:?} {refused:?}");
    }
    let (codes, refused) = of(&rows, "twice_unbounded");
    assert!(codes.contains(&"E0382"), "TWIN `twice_unbounded`: E0382: {codes:?} {refused:?}");
}
