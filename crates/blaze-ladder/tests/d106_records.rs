//! D106: `blaze run` on records, through the real maker (crates/blaze-ladder/src/run/make_one.rs)
//! and the record walk (body_ids.rs `Walk`), on a real program (rule 8, each claim with its
//! control).
//!
//! The program is code_records.rs's `PROGRAM` (one `#![no_core]` crate; a second crate needs a
//! dependency, which `NewFront` has no input for yet: said, not faked). Six instances: `main`,
//! `hello`, `beta`, `alpha`, and `pick` at `i32` and at `u8`; five fns with bodies; `write` is a
//! C name, no instance.
//!
//! THE COMMAND'S STAND-IN ([`TextIds`]): the command's BodyCheck keys come from the drive; here
//! each fn's key is a digest of its item's TEXT, read from a program made beside the walk (the
//! stand-in, never the maker's: its checks are not counted). One read per body, always today's;
//! every crate fact constant. So a body edit moves exactly that fn's key, as the command's does.
//!
//! GENERIC OVER THE MAKER ([`Instantiation`]): piece X's instantiation is today's
//! `Front2Program` made on the first miss ([`FirstMissFront2`]); piece Y adds one impl (the lazy
//! program over the names records) and calls the same two bodies.

use blaze_emit::{Isa, Os};
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Counts, Read};
use blaze_ladder::code_records::{Builder, CodeStore, MemoryStore, Settings};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::make_one::{CheckCount, OnFirstMiss, walk_records, with_front2};
use blaze_ladder::run::{NewFront, OverflowChecks};
use blaze_ladder::seam::FrontEnd;
use blaze_own::Alloc;
use blaze_run::Tier;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const PROGRAM: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

unsafe extern \"C\" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

fn hello() -> isize {
    unsafe { write(1, b\"hello\\n\" as *const u8, 6) }
}

fn alpha() -> i32 {
    1
}

fn beta() -> i32 {
    alpha()
}

fn pick<T>(c: bool, a: T, b: T) -> T {
    if c { a } else { b }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    hello();
    let x: i32 = pick(true, beta(), 2);
    let y: u8 = pick(false, 3, 4);
    x + y as i32
}
";

const ROOT: &str = "src/main.rs";
const OVERFLOW: OverflowChecks = OverflowChecks::Off;
const REACHED: usize = 6;
const BODIES: usize = 5;

fn new_front() -> NewFront<'static> {
    NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OVERFLOW }
}

fn isa() -> Isa {
    Isa { lse: true, os: Os::MacOs }
}

fn builder() -> Builder {
    Builder { isa: isa(), tier: Tier::Fast, settings: Settings::baseline(OVERFLOW), toolchain: ToolchainId([7; 32]) }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"d106 make one").get::<CodeBuilder>()
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// The command's stand-in (the module doc).
struct TextIds {
    rows: Vec<(ItemKey, ContentId)>,
    /// `false`: the control, the seam's answers dropped (no BodyCheck record for any fn).
    recorded: bool,
    entry: ItemKey,
}

impl BodyIds for TextIds {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        let n = self.rows.binary_search_by_key(&item, |r| r.0).ok()?;
        Some(BodyId { key: self.rows[n].1, reads: if self.recorded { &READS } else { &[] } })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// Each fn's key from its item's text, read from a program the stand-in makes for itself.
fn ids_of(text: &str, recorded: bool) -> TextIds {
    let files = [SourceFile { path: ROOT, text }];
    with_front2(&files, ROOT, new_front(), alloc(), |construct| {
        let program = construct().unwrap_or_else(|e| panic!("the stand-in's program: {e}"));
        let mut rows = Vec::new();
        for i in 0..program.fn_count() {
            let f = program.function(i);
            rows.push((f.key, Hasher::new("d106 test body check").part(f.text.as_bytes()).finish()));
        }
        rows.sort();
        let entry = program.function(program.entry().expect("the program has a main")).key;
        TextIds { rows, recorded, entry }
    })
}

/// One instantiation of D106's maker.
trait Instantiation {
    /// The bodies a miss checks by constructing the program, of `reached` bodies.
    fn by_construction(reached: usize) -> usize;
    /// One walk of `text` over `store`: its counts and the maker's checks.
    fn walk(text: &str, ids: &TextIds, store: &mut dyn CodeStore) -> (Counts, CheckCount);
}

/// Piece X's: today's `Front2Program`, constructed on the first miss. Its construction checks
/// its whole reach (`Front2Program::new`'s census).
struct FirstMissFront2;

impl Instantiation for FirstMissFront2 {
    fn by_construction(reached: usize) -> usize {
        reached
    }

    fn walk(text: &str, ids: &TextIds, store: &mut dyn CodeStore) -> (Counts, CheckCount) {
        let files = [SourceFile { path: ROOT, text }];
        with_front2(&files, ROOT, new_front(), alloc(), |construct| {
            let mut maker = OnFirstMiss::new(construct, isa(), OVERFLOW, alloc());
            let (_, counts) = walk_records(ids.entry, ids, store, &builder(), &producer(), &mut maker, alloc()).unwrap_or_else(|e| panic!("the record walk: {e}"));
            (counts, maker.checks())
        })
    }
}

/// Run twice: the second run finds every record (found = reached), checks no body and makes no
/// program. Control: the seam's answers dropped over the SAME full store: every instance is
/// made unkeyed, and the bodies checked are the reached bodies.
fn twice<I: Instantiation>(tag: &str) {
    let mut store = MemoryStore::new_in(alloc());
    let ids = ids_of(PROGRAM, true);
    let (first, first_checks) = I::walk(PROGRAM, &ids, &mut store);
    assert_eq!((first.reached, first.found, first.made), (REACHED, 0, REACHED), "{tag}: first run {first}");
    assert_eq!((first_checks.check_one, first_checks.constructed), (BODIES, 1), "{tag}: first run {first_checks:?}");
    let (second, second_checks) = I::walk(PROGRAM, &ids, &mut store);
    assert_eq!((second.reached, second.found, second.made, second.make_calls()), (REACHED, REACHED, 0, 0), "{tag}: second run {second}");
    assert_eq!(second_checks, CheckCount::default(), "{tag}: the second run checked or constructed");
    let control_ids = TextIds { recorded: false, ..ids_of(PROGRAM, true) };
    let (control, control_checks) = I::walk(PROGRAM, &control_ids, &mut store);
    assert_eq!((control.found, control.no_record), (0, REACHED), "{tag}: control {control}");
    assert_eq!((control_checks.check_one, control_checks.constructed), (BODIES, 1), "{tag}: control {control_checks:?}");
    println!(
        "d106 {tag} twice: first {first}; second {second}, checks {}, programs {}; control {control}, checks {}",
        second_checks.all(),
        second_checks.constructed,
        control_checks.check_one
    );
}

/// One body's text edited between runs (`alpha`'s `1` to `5`): the ONE instance of `alpha` is
/// made, every other found; `check_one` is asked of that one body. What constructing the program
/// checks is the instantiation's: the whole reach under today's `Front2Program`, none under a
/// lazy one. Control: the same edit with every record dropped (a fresh store) makes all six.
fn edited_body<I: Instantiation>(tag: &str) {
    let edited = PROGRAM.replace("fn alpha() -> i32 {\n    1\n}", "fn alpha() -> i32 {\n    5\n}");
    assert_ne!(edited, PROGRAM, "the edit applies");
    let mut store = MemoryStore::new_in(alloc());
    I::walk(PROGRAM, &ids_of(PROGRAM, true), &mut store);
    let ids = ids_of(&edited, true);
    let (after, checks) = I::walk(&edited, &ids, &mut store);
    assert_eq!((after.reached, after.found, after.made), (REACHED, REACHED - 1, 1), "{tag}: after the edit {after}");
    assert_eq!(checks.check_one, 1, "{tag}: bodies asked of the seam {checks:?}");
    assert_eq!(checks.constructed, 1, "{tag}: programs {checks:?}");
    assert_eq!(checks.by_construction, I::by_construction(BODIES), "{tag}: checked by construction {checks:?}");
    let (control, _) = I::walk(&edited, &ids, &mut MemoryStore::new_in(alloc()));
    assert_eq!((control.found, control.made), (0, REACHED), "{tag}: control {control}");
    println!("d106 {tag} edit: {after}, check_one {}, by construction {}; control {control}", checks.check_one, checks.by_construction);
}

/// Prints "d106 front2-first-miss twice: first code records: reached 6, found 0, made 6, unkeyed 0
/// (no record 0, stale 0); second code records: reached 6, found 6, made 0, unkeyed 0 (no record
/// 0, stale 0), checks 0, programs 0; control code records: reached 6, found 0, made 0, unkeyed 6
/// (no record 6, stale 0), checks 5".
#[test]
fn a_second_run_finds_every_record_and_checks_no_body() {
    twice::<FirstMissFront2>("front2-first-miss");
}

/// Prints "d106 front2-first-miss edit: code records: reached 6, found 5, made 1, unkeyed 0 (no
/// record 0, stale 0), check_one 1, by construction 5; control code records: reached 6, found 0,
/// made 6, unkeyed 0 (no record 0, stale 0)". Under today's `Front2Program` the miss constructs
/// the whole program, so the bodies CHECKED are the whole reach (5), not the one: asserted.
#[test]
fn an_edited_body_is_made_alone_and_front2program_construction_checks_the_whole_reach() {
    edited_body::<FirstMissFront2>("front2-first-miss");
}
