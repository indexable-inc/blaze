//! The tier equality harness (WAVE-BACKEND.md piece A; backend-next.md D97 point 1, D100 points
//! 1 and 14): every tier in [`TIERS`] compiles the same generated LIR programs through piece 0's
//! seam, each record is sealed, encoded, read and placed by the ONE new placer
//! (`blaze_tier::record::place`), run as machine code in this process, and compared with LIR's
//! evaluator (`blaze_lir::eval::run_in`) on the same arguments. Every later tier and every LIR
//! rewrite stands on this harness, so:
//!
//! - A program it could NOT compare is never silent: the evaluator's refusals and outcomes it does
//!   not compare (a trap, undefined behaviour, out of fuel) and each tier's refusals by name are
//!   counted, per tier and per reason, printed at the end of the run, and the counts are checked
//!   to account for every program; a tier that compared no program fails the test (a vacuous
//!   pass).
//! - The generator (tests.rs `random::program`) is cut to the Reference tier's subset, so a
//!   Reference refusal is a finding, not a count; the Fast tier's refusals are counted.
//! - The control (rule 8): a `Table` wrapper whose `add` templates have their bytes' bit 30 set
//!   (arm64 `add` becomes `sub`) is served to the Fast tier, and the run must catch it with "differs
//!   from the evaluator" (`blaze_sim::control`). A second control keeps the bytes and renumbers
//!   one field (`WrongField`: an `add`'s second input filled from its first), caught the same way.
//!
//! The ladder's rungs are not here: blaze-ladder exposes no rung's LIR, and it depends on this
//! crate (a dev-dependency on it would be a cycle).

use core::cell::RefCell;

use blaze_code::image::ImageError;
use blaze_code::place::{At, PlaceError, Words};
use blaze_code::record::{Space, Symbol, Target};
use blaze_inproc::exec::Executable;
use blaze_lir::eval::{self, Outcome, Val};
use blaze_lir::{Block, Op, Value, View};
use blaze_own::{ASortedMap, AString, AVec, Alloc, SortedMap, Vec, format_in};
use blaze_sim::{Broke, Config, Tape};
use blaze_tier::record::{encode, place, read, seal};
use blaze_tier::{
    ARM64, Arch, Emitted, Field, FieldKind, Form, Machine, Named, Names, Os, Outside, Pattern, Table, TableHead, Template, Tier, TierError,
    TierName,
};

use crate::compose::emit::POLL_PAGE;
use crate::table_gen::EncodedTable;
use crate::tests::{FUEL, mask, random, run, sum_func};
use crate::{Fast, Reference};

/// Register pressure and the 1-bit templates in the Fast tier (fast-evict-1).
mod pressure;

/// The Fast tier's directed cases (D195 item 4): far frames, a loop's back edge, a call with
/// live caller-saved registers, a flag across stores, and the scratch pool's control.
mod directed;

/// The routine table as the seam's input (D197 root 6, piece (a)): Reference's record through
/// the seam equals `emit_record`'s, an `f128` add runs through `__addtf3`, and the control.
mod routines;

/// The function's own data in the record form's columns by both tiers, run: a row aligned past
/// 8 bytes placed where its record asks, a source position left to the placer, and the controls.
mod own_data;

/// `tls_addr` through the seam by both tiers, run on two threads (the table's root form is
/// macOS's: `tpidrro_el0`).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod thread_locals;

/// One tier as the harness calls it: piece 0's `Tier::emit` as a plain function, so the list can
/// hold tiers whose state (a table) the function makes itself.
type TierFn = for<'a> fn(View<'_>, &dyn Names, Machine, Alloc<'a>) -> Result<Emitted<'a>, TierError>;

/// One row of the tier list.
struct Row {
    name: TierName,
    emit: TierFn,
    /// The generator is cut to this tier's subset: a refusal of a generated program is a finding
    /// (`Broke::Oracle`), not a count.
    covers_the_generator: bool,
}

/// Every tier the harness runs, the oracle tier first.
const TIERS: [Row; 2] = [
    Row { name: TierName::Reference, emit: reference, covers_the_generator: true },
    Row { name: TierName::Fast, emit: fast, covers_the_generator: false },
];

/// [`TIERS`] with the Fast tier over the planted table.
const PLANTED: [Row; 2] = [
    Row { name: TierName::Reference, emit: reference, covers_the_generator: true },
    Row { name: TierName::Fast, emit: fast_planted, covers_the_generator: false },
];

/// [`TIERS`] with the Fast tier over the table with one field renumbered.
const PLANTED_FIELD: [Row; 2] = [
    Row { name: TierName::Reference, emit: reference, covers_the_generator: true },
    Row { name: TierName::Fast, emit: fast_planted_field, covers_the_generator: false },
];

/// The control's plants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The Fast tier's `add` templates with bit 30 of their bytes set ([`WrongTemplate`]).
    WrongTemplate,
    /// The Fast tier's register-register `add` templates with the field of their second input
    /// naming the first: one patched field off by one operand ([`WrongField`]).
    WrongField,
}

fn tiers(plant: Plant) -> &'static [Row; 2] {
    match plant {
        Plant::None => &TIERS,
        Plant::WrongTemplate => &PLANTED,
        Plant::WrongField => &PLANTED_FIELD,
    }
}

/// The table T1 commits (crates/blaze-emit/table/arm64.bztab).
const TABLE: &[u8] = include_bytes!("../table/arm64.bztab");

/// The committed table, read; the tests read it once up front and stop if it does not read, so a
/// tier function's own read cannot fail unseen.
fn table() -> EncodedTable<'static> {
    match EncodedTable::read(TABLE, Alloc::global()) {
        Ok(table) => table,
        Err(error) => panic!("the committed table does not read: {error}"),
    }
}

fn reference<'a>(func: View<'_>, names: &dyn Names, machine: Machine, alloc: Alloc<'a>) -> Result<Emitted<'a>, TierError> {
    Reference.emit(blaze_lir::verify(func).expect("the harness's function is LIR"), names, machine, alloc)
}

fn fast<'a>(func: View<'_>, names: &dyn Names, machine: Machine, alloc: Alloc<'a>) -> Result<Emitted<'a>, TierError> {
    let table = EncodedTable::read(TABLE, Alloc::global()).map_err(|error| TierError::Table { tier: TierName::Fast, error })?;
    Fast { table: &table, regs: &ARM64 }.emit(blaze_lir::verify(func).expect("the harness's function is LIR"), names, machine, alloc)
}

fn fast_planted<'a>(func: View<'_>, names: &dyn Names, machine: Machine, alloc: Alloc<'a>) -> Result<Emitted<'a>, TierError> {
    let table = EncodedTable::read(TABLE, Alloc::global()).map_err(|error| TierError::Table { tier: TierName::Fast, error })?;
    let planted = WrongTemplate::over(&table);
    Fast { table: &planted, regs: &ARM64 }.emit(blaze_lir::verify(func).expect("the harness's function is LIR"), names, machine, alloc)
}

fn fast_planted_field<'a>(func: View<'_>, names: &dyn Names, machine: Machine, alloc: Alloc<'a>) -> Result<Emitted<'a>, TierError> {
    let table = EncodedTable::read(TABLE, Alloc::global()).map_err(|error| TierError::Table { tier: TierName::Fast, error })?;
    let planted = WrongField::over(&table);
    Fast { table: &planted, regs: &ARM64 }.emit(blaze_lir::verify(func).expect("the harness's function is LIR"), names, machine, alloc)
}

/// The planted wrong field: every `add` template of `inner` whose second operand is a register,
/// with that operand's register field renumbered to operand 0 (the bytes unchanged), so the
/// composer fills `add d, a, a` for `add d, a, b`. A field holds an operand's bits, so "one
/// register off" in the field's own data is its operand number off by one; a register number
/// plus one is no field edit (piece 0's `Field` copies bits, it adds nothing). Every other
/// pattern is served by `inner` unchanged.
struct WrongField<'t> {
    inner: &'t EncodedTable<'static>,
    /// `(pattern, first field in fields, count)`, sorted by pattern (the table's order).
    adds: AVec<'static, (Pattern, u32, u32)>,
    fields: AVec<'static, Field>,
}

impl<'t> WrongField<'t> {
    fn over(inner: &'t EncodedTable<'static>) -> WrongField<'t> {
        let mut adds: AVec<'static, (Pattern, u32, u32)> = Vec::new_in(Alloc::global());
        let mut fields: AVec<'static, Field> = Vec::new_in(Alloc::global());
        for pattern in inner.patterns() {
            if pattern.op != Op::Add || !matches!(pattern.operands[1], Form::Reg(_)) || pattern.inner.is_some() {
                continue;
            }
            let Some(template) = inner.find(pattern) else {
                panic!("the table lists `add` at width {} and has no template for it", pattern.width);
            };
            let at = fields.len();
            let mut renumbered = 0;
            for field in template.fields {
                let mut field = *field;
                if field.operand == 1 && matches!(field.kind, FieldKind::Reg) {
                    field.operand = 0;
                    renumbered += 1;
                }
                fields.push(field);
            }
            assert!(renumbered > 0, "an `add` at width {} has no register field for its second input", pattern.width);
            adds.push((*pattern, at as u32, template.fields.len() as u32));
        }
        assert!(!adds.is_empty(), "the committed table has no register `add` template: the plant would plant nothing");
        WrongField { inner, adds, fields }
    }
}

impl Table for WrongField<'_> {
    fn head(&self) -> TableHead {
        self.inner.head()
    }

    fn find(&self, pattern: &Pattern) -> Option<Template<'_>> {
        let template = self.inner.find(pattern)?;
        let Ok(i) = self.adds.binary_search_by(|row| row.0.cmp(pattern)) else { return Some(template) };
        let (_, at, len) = self.adds[i];
        Some(Template { fields: &self.fields[at as usize..(at + len) as usize], ..template })
    }
}

/// The planted wrong template: every `add` template of `inner` with its bytes' bit 30 set (in the
/// table's bit numbering, byte 3's bit 6: an arm64 `add`'s `op` bit, so `add` becomes `sub`).
/// Every other pattern is served by `inner` unchanged.
struct WrongTemplate<'t> {
    inner: &'t EncodedTable<'static>,
    /// `(pattern, first byte in bytes, length)`, sorted by pattern (the table's order).
    adds: AVec<'static, (Pattern, u32, u32)>,
    bytes: AVec<'static, u8>,
}

impl<'t> WrongTemplate<'t> {
    fn over(inner: &'t EncodedTable<'static>) -> WrongTemplate<'t> {
        let mut adds: AVec<'static, (Pattern, u32, u32)> = Vec::new_in(Alloc::global());
        let mut bytes: AVec<'static, u8> = Vec::new_in(Alloc::global());
        for pattern in inner.patterns() {
            if pattern.op != Op::Add {
                continue;
            }
            let Some(template) = inner.find(pattern) else {
                panic!("the table lists `add` at width {} and has no template for it", pattern.width);
            };
            assert!(template.bytes.len() >= 4, "an arm64 template is whole words");
            let at = bytes.len();
            bytes.extend_from_slice(template.bytes);
            bytes[at + 3] |= 0x40;
            adds.push((*pattern, at as u32, template.bytes.len() as u32));
        }
        assert!(!adds.is_empty(), "the committed table has no `add` template: the plant would plant nothing");
        WrongTemplate { inner, adds, bytes }
    }
}

impl Table for WrongTemplate<'_> {
    fn head(&self) -> TableHead {
        self.inner.head()
    }

    fn find(&self, pattern: &Pattern) -> Option<Template<'_>> {
        let template = self.inner.find(pattern)?;
        if pattern.op != Op::Add {
            return Some(template);
        }
        let i = self.adds.binary_search_by(|row| row.0.cmp(pattern)).ok()?;
        let (_, at, len) = self.adds[i];
        Some(Template { bytes: &self.bytes[at as usize..(at + len) as usize], ..template })
    }
}

/// Generated programs name nothing outside themselves: a tier that asks for a name has made a
/// reference the program does not have, and is refused by name (`TierError::NoName`).
struct Nameless;

impl Names for Nameless {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// The run's answers for a placed record: the poll page by its C name, nothing else (generated
/// programs name nothing: any other target is unbound, by name).
struct PollOnly {
    page: u64,
}

impl Words for PollOnly {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        match target.space == Space::Outside && target.symbol == Symbol::of_name(POLL_PAGE) {
            true => Ok(self.page),
            false => Err(ImageError::Unbound { target }),
        }
    }
}

/// Carves a zeroed, 4096-byte-aligned poll page out of the front of `Executable::written`'s
/// adjacent data mapping (guaranteed within `adrp`'s ±4 GiB `Page21` reach on every host ASLR
/// layout, including Linux ARM64 PIE) and returns `(poll_addr, rest)`.
const POLL_PAGE_ROOM: usize = 8192;

fn carve_poll_page(data: &mut [u8]) -> (u64, &mut [u8]) {
    let skip = (4096 - data.as_ptr() as usize % 4096) % 4096;
    let (poll_and_pad, rest) = data.split_at_mut(skip + 4096);
    let poll = &mut poll_and_pad[skip..];
    poll.fill(0);
    (poll.as_ptr() as u64, rest)
}

/// `e` sealed, encoded, read back and placed by the one new placer into executable memory: its
/// code where it runs (a mapping's start, page-aligned), its relro part at its record's
/// alignment in the data after it.
fn placed(e: Emitted<'static>, tier: TierName) -> Result<Executable, Broke> {
    let name = tier.as_str();
    let sealed = seal(e).map_err(|error| Broke::Oracle(format!("the {name} tier's record does not seal: {error}")))?;
    let mut bytes: AVec<'static, u8> = Vec::new_in(Alloc::global());
    encode(&sealed, &mut bytes);
    let view = read(&bytes).map_err(|error| Broke::Oracle(format!("the {name} tier's record does not read back: {error}")))?;
    let relro_len = view.relro().len();
    let align = 1usize << view.relro_align();
    let symbol = Symbol::of_name("blaze-emit tier_tests: the function under test");
    let mut done: Result<(), PlaceError> = Ok(());
    let exec = Executable::written(view.code().len(), POLL_PAGE_ROOM + relro_len + align, |code, data| {
        let (page, rest) = carve_poll_page(data);
        let skip = (align - rest.as_ptr() as usize % align) % align;
        let relro = &mut rest[skip..skip + relro_len];
        let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
        let mut words = PollOnly { page };
        done = place(&view, symbol, at, &mut words, code, relro);
    })
    .expect("the code maps");
    done.map_err(|error| Broke::Oracle(format!("the {name} tier's record does not place: {error}")))?;
    Ok(exec)
}

/// What a run could not compare, and what it did: per tier, and per (who, reason).
struct Tally {
    /// Programs generated and verified.
    cases: u32,
    compared: ASortedMap<'static, TierName, u32>,
    /// `(who, reason)`: "the evaluator" for a program no tier was compared on, else the tier.
    not: ASortedMap<'static, (&'static str, AString<'static>), u32>,
}

/// Who a program no tier was compared on is counted under.
const EVALUATOR: &str = "the evaluator";

impl Tally {
    fn new() -> Tally {
        Tally { cases: 0, compared: SortedMap::new_in(Alloc::global()), not: SortedMap::new_in(Alloc::global()) }
    }

    fn not_compared(&mut self, who: &'static str, why: AString<'static>) {
        *self.not.entry((who, why)).or_insert(0) += 1;
    }

    fn compared(&mut self, tier: TierName) {
        *self.compared.entry(tier).or_insert(0) += 1;
    }

    fn compared_of(&self, tier: TierName) -> u32 {
        self.compared.get(&tier).copied().unwrap_or(0)
    }

    fn not_of(&self, who: &str) -> u32 {
        self.not.as_slice().iter().filter(|((by, _), _)| *by == who).map(|(_, n)| *n).sum()
    }

    /// The counts, one line each: what was compared per tier, then every reason a program was
    /// not, with its count.
    fn print(&self, title: &str) {
        println!("{title}: {} programs", self.cases);
        for row in TIERS.iter() {
            println!("  {}: compared {} of {}", row.name.as_str(), self.compared_of(row.name), self.cases);
        }
        for ((who, why), n) in self.not.as_slice() {
            println!("  not compared, {who}: {n} x {why}");
        }
    }
}

/// A tier's refusal as its count's reason: the reason without the instruction's number, so one
/// form is one line.
fn reason(error: TierError) -> AString<'static> {
    match error {
        TierError::Refused { what, .. } => format_in!(Alloc::global(), "refused: {what}"),
        other => format_in!(Alloc::global(), "{other}"),
    }
}

/// The evaluator's answer on `args`, or why the program is not compared.
fn evaluated(view: View<'_>, args: &[Val]) -> Result<u64, AString<'static>> {
    let g = Alloc::global();
    match eval::run_in(view, args, FUEL, Alloc::global()) {
        Err(why) => Err(format_in!(g, "refused: {why}")),
        Ok(Outcome::Returned(values)) => match values.first() {
            Some(Val::Int(it)) => Ok(*it as u64),
            _ => Err(format_in!(g, "a return other than one integer")),
        },
        // A trap stops this process; traps are run in a child (tests.rs `traps_stop_with_sigtrap`).
        Ok(Outcome::Trapped(_)) => Err(format_in!(g, "the run traps")),
        Ok(Outcome::Undefined { why, .. }) => Err(format_in!(g, "undefined behaviour: {why}")),
        Ok(Outcome::OutOfFuel) => Err(format_in!(g, "out of fuel")),
    }
}

/// One generated program run by every tier of `plant`'s list as machine code and by the
/// evaluator: equal returns, or the case is broken. What it could not compare goes to `tally`.
#[cfg(target_arch = "aarch64")]
fn tiers_agree(tape: &Tape, plant: Plant, machine: Machine, tally: &RefCell<Tally>) -> Result<(), Broke> {
    let (func, args) = random::program(tape).map_err(|why| Broke::Harness(format!("a generated program does not build: {why:?}")))?;
    let view = func.view();
    blaze_lir::verify(view).map_err(|why| Broke::Harness(format!("a generated program does not verify: {why}")))?;
    tally.borrow_mut().cases += 1;
    let want = match evaluated(view, &args) {
        Ok(want) => want,
        Err(why) => {
            tally.borrow_mut().not_compared(EVALUATOR, why);
            return Ok(());
        }
    };
    let garbage = tape.next_u64("garbage above the arguments' widths");
    let mut regs = [0u64; 4];
    for (at, (inst, arg)) in view.params(Block(0)).zip(&args).enumerate() {
        let Val::Int(bits) = *arg else { return Err(Broke::Harness("a pointer argument".to_owned())) };
        let width = view.ty(Value(inst as u32)).bits(64);
        regs[at] = bits as u64 | (garbage & !mask(width));
    }
    for row in tiers(plant) {
        let name = row.name.as_str();
        let emitted = match (row.emit)(view, &Nameless, machine, Alloc::global()) {
            Ok(emitted) => emitted,
            Err(error) if row.covers_the_generator => {
                return Err(Broke::Oracle(format!(
                    "the {name} tier refused a program of its subset: {error}\n{}",
                    blaze_lir::print(view)
                )));
            }
            Err(error) => {
                tally.borrow_mut().not_compared(name, reason(error));
                continue;
            }
        };
        let exec = placed(emitted, row.name)?;
        let got = run::call(&exec, regs);
        if got != want {
            return Err(Broke::Oracle(format!(
                "the {name} tier differs from the evaluator: it returned {got:#x}, the evaluator {want:#x}, on {args:x?}\n{}",
                blaze_lir::print(view)
            )));
        }
        tally.borrow_mut().compared(row.name);
    }
    Ok(())
}

/// Every tier agrees with the evaluator on 512 generated programs, and the run accounts for every
/// program: for each tier, compared + its refusals + the programs the evaluator did not answer =
/// programs; each tier compared at least one (else the pass is vacuous).
/// Expected: passes, printing "tiers against the evaluator: 512 programs", one "compared N of 512"
/// line per tier and one "not compared, <who>: <n> x <reason>" line per reason.
#[cfg(target_arch = "aarch64")]
#[test]
fn every_tier_agrees_with_the_evaluator() {
    let machine = table().head().machine;
    let tally = RefCell::new(Tally::new());
    blaze_sim::check(Config::cases(512), |tape: &Tape| tiers_agree(tape, Plant::None, machine, &tally));
    let tally = tally.into_inner();
    tally.print("tiers against the evaluator");
    let unanswered = tally.not_of(EVALUATOR);
    for row in TIERS.iter() {
        let name = row.name.as_str();
        let compared = tally.compared_of(row.name);
        let refused = tally.not_of(name);
        assert_eq!(
            compared + refused + unanswered,
            tally.cases,
            "the {name} tier: compared {compared}, refused {refused}, not answered by the evaluator {unanswered}, of {} programs",
            tally.cases
        );
        assert!(compared > 0, "the {name} tier compared no program: its pass would be vacuous");
    }
}

/// The control (rule 8): the Fast tier over a table whose `add` templates are `sub` is caught by
/// the run, and the same harness with the true table passes.
/// Expected: passes; blaze-sim prints "plant WrongTemplate: caught at case k of 512" on standard
/// error, and the tally prints "the control, clean and planted runs together: N programs".
#[cfg(target_arch = "aarch64")]
#[test]
fn plant_wrong_template_is_caught() {
    let machine = table().head().machine;
    let tally = RefCell::new(Tally::new());
    blaze_sim::control(Config::cases(512), Plant::None, Plant::WrongTemplate, "differs from the evaluator", |tape: &Tape, plant: Plant| {
        tiers_agree(tape, plant, machine, &tally)
    });
    tally.into_inner().print("the control, clean and planted runs together");
}

/// The control for one wrong FIELD (D195 item 4): the Fast tier over a table whose register
/// `add` templates fill their second input's field from the first operand is caught by the run
/// (any generated `add` of two different values differs), and the clean run passes.
/// Expected: passes; blaze-sim prints "plant WrongField: caught at case k of 512" on standard
/// error.
#[cfg(target_arch = "aarch64")]
#[test]
fn plant_wrong_field_is_caught() {
    let machine = table().head().machine;
    let tally = RefCell::new(Tally::new());
    blaze_sim::control(Config::cases(512), Plant::None, Plant::WrongField, "differs from the evaluator", |tape: &Tape, plant: Plant| {
        tiers_agree(tape, plant, machine, &tally)
    });
    tally.into_inner().print("the field control, clean and planted runs together");
}

/// `sum(n)` (tests.rs `sum_func`: a loop, block parameters) through every tier, against the
/// evaluator: the one fixed program both tiers must compare, so the harness's own path (seal,
/// encode, read, place, call) is seen to run for each tier.
/// Expected: passes, printing "sum: every tier agrees with the evaluator on 6 arguments".
#[cfg(target_arch = "aarch64")]
#[test]
fn sum_agrees_in_every_tier() {
    let machine = table().head().machine;
    let func = sum_func();
    let view = func.view();
    let ns = [0u64, 1, 2, 10, 1000, 65_537];
    for row in TIERS.iter() {
        let name = row.name.as_str();
        let emitted = match (row.emit)(view, &Nameless, machine, Alloc::global()) {
            Ok(emitted) => emitted,
            Err(error) => panic!("the {name} tier refused sum: {error}"),
        };
        let exec = match placed(emitted, row.name) {
            Ok(exec) => exec,
            Err(broke) => panic!("{broke}"),
        };
        for n in ns {
            let want = match evaluated(view, &[Val::Int(u128::from(n))]) {
                Ok(want) => want,
                Err(why) => panic!("the evaluator did not answer sum({n}): {why}"),
            };
            assert_eq!(run::call(&exec, [n, 0, 0, 0]), want, "the {name} tier's sum({n})");
        }
    }
    println!("sum: every tier agrees with the evaluator on {} arguments", ns.len());
}

/// The Reference tier refuses a machine it does not compile for by name.
/// Expected: passes. Fails if an x86-64 machine reaches the arm64 emitter.
#[test]
fn reference_refuses_x86_64() {
    let machine = Machine { arch: Arch::X86_64, os: Os::Linux, features: 0 };
    let func = sum_func();
    match Reference.emit(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &Nameless, machine, Alloc::global()) {
        Err(TierError::Machine { tier: TierName::Reference, machine: refused }) => assert_eq!(refused, machine),
        Err(other) => panic!("the wrong refusal: {other}"),
        Ok(_) => panic!("the Reference tier emitted for x86-64"),
    }
}
