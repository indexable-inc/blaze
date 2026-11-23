//! ONE MEMBER'S CONSTANT, ONE MEMBER'S GAP (lane RH1): a trait of three members, one of whose
//! signatures holds a constant that does not evaluate (`fn bad(&self, x: [u8; BAD])` with
//! `BAD = not_defined()`). Its members cell is frozen (front2/tables/head.rs `freeze_seeded`)
//! with that member as a gap and the other two as heads; before, the evaluator's refusal failed
//! the trait's whole members cell, so a body calling `first` was refused for `bad`'s constant.
//!
//! The gap travels in the frozen record (head_record.rs form 13, `FnRefused`): a store seeded
//! from it (`seed`, what a body's view reads, front2/tables/view.rs) holds `first`'s and
//! `third`'s heads, which a body calling them is checked against, and `bad`'s `fn_gaps` row
//! with its reason, which a body calling it is refused for by name (body.rs `refused_head`,
//! world_items.rs `Gap::FnHead`).
//!
//! Rule 23, the cause removed: with `BAD = 4` all three members are heads and no gap is filed.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (real core, alloc, std).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::head_record::HeadAt;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{Keys, MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::tables::head::{HeadCells, SeededHeads, SeededHeadsOpts, StoreConsts, cell_of, seed};
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// What the trait `Tr` of `text` froze as and what a store seeded from it holds, per member
/// (`first`, `bad`, `third`, in that order): `Ok(())` a head, `Err(why)` a gap and its words.
struct Members {
    /// How many of the frozen record's entries are fn heads or refused fns (`HeadAt::Fn`).
    fn_entries: usize,
    seeded: [Result<(), String>; 3],
}

fn members_of(text: &str) -> Members {
    let alloc = Alloc::global();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/gaps/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "gaps", dir: "/gaps/", root: "/gaps/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let found = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Tr"] }, alloc).unwrap_or_else(|e| panic!("`Tr`: {e}"));
    let tr = found.whole(1).unwrap_or_else(|| panic!("`Tr` not named")).key;
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut evaluator = StoreConsts { names: &names, index: &front.index, prepared };
    let sized = front.index.lang("sized");
    let mut source = SeededHeads::new(SeededHeadsOpts { names: &names, sized, pointers: blaze_ladder::front2::receiver::Pointers::NOT_LOOKED_UP, frame: &frame, scratch: alloc, alloc, consts: Some(&mut evaluator), known: None, owners: None });
    let cell = cell_of(&names, Want::Members(tr)).unwrap_or_else(|e| panic!("the cell of `Tr`'s members: {e}"));
    assert_eq!(cell, Want::Members(tr), "`Tr`'s members are a cell of their own");
    // The trait's members frozen: never refused whole for one member's constant.
    let fn_entries = match source.cell(cell) {
        Ok((frozen, _)) => frozen.heads().filter(|e| matches!(e.at, HeadAt::Fn(_))).count(),
        Err(e) => panic!("ORACLE: `Tr`'s members cell was refused whole: {e}"),
    };
    // A store seeded from the frozen record, as a body's view is.
    let store = Store::new(sized, &frame, alloc);
    let keys = Keys::new();
    seed(&names, None, cell, &mut source, &store, &keys, alloc).unwrap_or_else(|e| panic!("seeding `Tr`'s members: {e}"));
    let container = checker_of(tr);
    let member = |name: &str| -> Result<(), String> {
        let key = member_key_of(container, MemberKind::Fn, name);
        match (store.fns.get(&key), store.fn_gaps.get(&key)) {
            (Some(_), None) => Ok(()),
            (None, Some(why)) => Err(format!("{why:?}")),
            (Some(_), Some(why)) => panic!("`{name}` is both a head and a gap: {why:?}"),
            (None, None) => panic!("ORACLE: `{name}` is neither a head nor a gap in the seeded store"),
        }
    };
    Members { fn_entries, seeded: [member("first"), member("bad"), member("third")] }
}

const TRAIT: &str = "pub trait Tr {\n    fn first(&self) -> u8;\n    fn bad(&self, x: [u8; BAD]);\n    fn third(&self) -> u16;\n}\n";

/// Expected line: `member gaps: Tr frozen with `bad` a gap (<why>); `first` and `third` heads; BAD = 4: three heads`.
#[test]
#[ignore = "the fixture, not the rule (2026-11-17, lane RH3, read not run): `Tr`'s members cell was refused whole: `the supertrait ItemKey(..) has no library key` (front2 heads.rs, the supertraits loop): the clause is the trait's implied self-size bound on `Self`, and `Keys::library_in` asks the solved `&Names` source, whose `key_beginning` is `None` by contract (source.rs, `Front2::new` files every key into ITS key map), while `SeededHeads` keys over a map of its own that starts empty. Not lang traits: the fixture must freeze over a source whose key map is filled (the engine's `&Records`, or `SeededHeadsOpts` taking `front.keys`); the next piece"]
fn one_members_constant_is_that_members_gap() {
    let failing = format!("pub const BAD: usize = not_defined();\n{TRAIT}fn main() {{}}\n");
    let got = members_of(&failing);
    assert_eq!(got.fn_entries, 3, "ORACLE: the frozen members cell does not hold all three members (a head or a gap each)");
    let [first, bad, third] = got.seeded;
    assert_eq!(first, Ok(()), "ORACLE: `first` (its signature holds no constant) was refused with `bad`");
    assert_eq!(third, Ok(()), "ORACLE: `third` (its signature holds no constant) was refused with `bad`");
    let why = bad.expect_err("ORACLE: `bad`, whose type holds a constant that does not evaluate, is a head");
    // The reason a body calling `bad` is refused for: the constant by name and why.
    assert!(why.contains("BAD"), "`bad`'s gap does not name the constant: {why}");

    // The cause removed: a constant that evaluates.
    let evaluating = format!("pub const BAD: usize = 4;\n{TRAIT}fn main() {{}}\n");
    let control = members_of(&evaluating);
    assert_eq!(control.fn_entries, 3, "CONTROL: the members cell of three heads");
    assert_eq!(control.seeded, [Ok(()), Ok(()), Ok(())], "CONTROL: with `BAD = 4` a member is a gap");
    println!("member gaps: Tr frozen with `bad` a gap ({why}); `first` and `third` heads; BAD = 4: three heads");
}
