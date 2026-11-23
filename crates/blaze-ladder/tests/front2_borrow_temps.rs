//! Temporary scopes and drop order on exit, by cause, each with its twin (rule 23). Every
//! ORACLE case is accepted by rustc; every TWIN refused by it.
//!
//! 1. A constant initializer's extending borrows make `'static` temporaries (rustc region.rs
//!    `resolve_body`: "temporaries ... in constant initializers may be extended to have
//!    'static lifetimes, using the same syntactical rules used for `let` initializers").
//!    blaze-flow-lower dropped them at the body's end under the returned borrow: a false
//!    "a reference to a local or temporary value is returned" (tempo evm handler/config.rs:60
//!    and eight more).
//!    - `C_EXT`: `const C_EXT: &S = &mk();`: clean.
//!    - TWIN `fn_ext`: the same `&mk()` returned from a fn: E0515.
//! 2. "the argument(s) to an extending tuple struct or tuple variant constructor expression"
//!    are extending (rustc region.rs: "Recurse into tuple constructors"): tempo fuzz-harness
//!    execute.rs:236 (`Contracts(&[&cache.contracts])`), zone_verifier/mod.rs:463.
//!    - `ctor_ext`: `let w = W(&String::new());` then `w` used: clean.
//!    - TWIN `call_noext`: `let w = id(&String::new());` then `w` used: E0716.
//! 3. `&ET` extends down `*ET | ET[..] | ET.f` (rustc `record_subexpr_extended_temp_scopes`):
//!    `&mut *c.borrow_mut()` extends the `RefMut` the overloaded deref borrows (tempo
//!    fuzz-harness invariants/views.rs:155).
//!    - `deref_ext`: `let v = &mut *c.borrow_mut();` then `v` used: clean.
//!    - TWIN `deref_noext`: `let v = first(&mut *c.borrow_mut());` then `v` used: E0716.
//! 4. On an exit (`?`, `return`, `break`) scopes unwind innermost first: a `for` statement's
//!    temporary (`read()?`'s `String`) after the iterator variable borrowing it. The lowering
//!    dropped every temporary before every variable (tempo corpus fuzz.rs:390, :391;
//!    precompiles storage_tests/vecs.rs:28, tip403_registry/mod.rs:1147).
//!    - `exit_order`: `for line in read()?.lines() { n += parse(line)?; }`: clean.
//!    - TWIN `exit_keep`: a variable from before the `for` keeps a borrow of its temporary
//!      past the statement: E0716 (the temporary still dies at the statement's end).
//! 5. An array expression relates each element one way into the element type (the Reference,
//!    "Array and array index expressions"; was one hub, `Unified::AggregateHub`).
//!    - `arr`: `[x, y]` of `&'a u8`, `&'b u8`, `x` returned as `&'a`: clean, not refused.
//!    - TWIN `arr_escape`: `arr[0]` returned as `&'b`: not clean.
//! NOT YET RUN (written 2026-11-21 by a write-only lane). UNSURE: that a `const` item's body is
//! found by name and checked as `BodyRef::Free` as a fn's is.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::cell::RefCell;

pub struct S {
    pub v: u8,
}

pub const fn mk() -> S {
    S { v: 1 }
}

pub const C_EXT: &S = &mk();

fn fn_ext() -> &'static S {
    &mk()
}

pub struct W<'a>(pub &'a String);

fn id(s: &String) -> &String {
    s
}

fn ctor_ext() -> usize {
    let w = W(&String::new());
    w.0.len()
}

fn call_noext() -> usize {
    let w = id(&String::new());
    w.len()
}

fn first(v: &mut Vec<u8>) -> &mut u8 {
    &mut v[0]
}

fn deref_ext(c: &RefCell<Vec<u8>>) -> usize {
    let v = &mut *c.borrow_mut();
    v.push(1);
    v.len()
}

fn deref_noext(c: &RefCell<Vec<u8>>) -> u8 {
    let v = first(&mut *c.borrow_mut());
    *v
}

fn read() -> Result<String, ()> {
    Ok(String::new())
}

fn parse(s: &str) -> Result<u32, ()> {
    s.parse::<u32>().map_err(|_| ())
}

fn exit_order() -> Result<u32, ()> {
    let mut n = 0;
    for line in read()?.lines() {
        n += parse(line)?;
    }
    Ok(n)
}

fn exit_keep() -> Result<usize, ()> {
    let mut keep: Vec<&str> = Vec::new();
    for line in read()?.lines() {
        keep.push(line);
        parse(line)?;
    }
    Ok(keep.len())
}

fn arr<'a, 'b>(x: &'a u8, y: &'b u8) -> &'a u8 {
    let _arr = [x, y];
    x
}

fn arr_escape<'a, 'b>(x: &'a u8, y: &'b u8) -> &'b u8 {
    let arr = [x, y];
    arr[0]
}

fn main() {}
"#;

type Verdict = (Vec<&'static str>, Option<BorrowRefused>);

fn verdicts(names_wanted: &[&str]) -> Vec<(String, Verdict)> {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/temps/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "temps",
        dir: "/temps/",
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
fn temporaries_live_and_die_where_rust_says() {
    let oracles = ["C_EXT", "ctor_ext", "deref_ext", "exit_order", "arr"];
    let twins = ["fn_ext", "call_noext", "deref_noext", "exit_keep", "arr_escape"];
    let all: Vec<&str> = oracles.iter().chain(twins.iter()).copied().collect();
    let rows = verdicts(&all);
    for n in oracles {
        let (codes, refused) = of(&rows, n);
        assert!(codes.is_empty() && refused.is_none(), "ORACLE `{n}`: rustc accepts it; checked clean: {codes:?} {refused:?}");
    }
    for n in twins {
        let (codes, refused) = of(&rows, n);
        assert!(!codes.is_empty(), "TWIN `{n}`: rustc refuses it; a finding must be reported: {codes:?} {refused:?}");
    }
}
