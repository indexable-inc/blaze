//! A program in a simulated world, checked and run in this process (the owner's ask of
//! 2026-11-20: "a World, add files to it, compile, then run where Out is some fake thing,
//! and assert, with fuel as Wasmtime has it"). A child of main.rs's tests: its helpers are
//! theirs.
//!
//! ```ignore
//! let ran = World::new(1).file("Cargo.toml", MANIFEST).file("src/main.rs", MAIN).run(Run { fuel: 50_000, args: &[], mode: Mode::Eval });
//! ran.out_is("Hello, world!\n").exit_is(0).fuel_at_most(50_000);
//! ```
//!
//! WHAT IS REAL TODAY:
//! - [`World`]: the simulated machine (`Io::simulated`), the fixture toolchain on its home's
//!   shelf, the files a test writes; nothing reaches the disk, the real stdout or a process.
//! - [`World::check`]: the engine's check in-process, as engine_check_tests.rs runs it.
//! - [`World::gate`]: THE GATE (D151, run_ids.rs `Checked::runs`) of one fn, read from the
//!   engine's stored answer as the run path reads it (engine_run_answer.rs `EngineAnswer`).
//! - [`run_lir`]: a program's LIR run on the reference evaluator (blaze-lir `eval`) with FUEL:
//!   each body is asked of a [`LirSource`] at the run's first call of it (the evaluator's
//!   `Bodies` seam, [`Lazy`]), made once and kept; running out of fuel is [`Exit::OutOfFuel`],
//!   a deterministic, named end, never a timeout; [`Ran::fuel_used`] is exact (see
//!   [`least_fuel`]). A body the source refuses (the gate's refusal) ends the run BY NAME at
//!   the call ([`Exit::Refused`]), never as the evaluator's "no callee".
//!
//! WHAT IS NOT BUILT, each a named [`Gap`]: [`World::run`] checks for real and then ends
//! [`Exit::NotRun`] naming the gap of its mode, and every test that needs a World's program
//! RUN is `#[ignore]`d with its gaps. The decisive one is [`Gap::MakeKeepsNoLir`]: the make
//! path `blaze run` takes on a first call (blaze-ladder run/make_one.rs `build_one`) emits each
//! instance's LIR to a code record at once and keeps no LIR, so there is no LIR of the real
//! path to hand the evaluator; the [`LirSource`] it would fill is this file's seam, tested
//! here with bodies built by hand.
//!
//! NOT COVERED, by design: the launcher (blaze-launch-rt run mode, its stubs and its socket),
//! the sandbox and the process's own `argv`, environment and exit. Those stay with the
//! process tests: crates/blaze-cli/tests/engine_start.rs (`Command::new(blaze)`).

use core::alloc::Allocator;
use core::cell::{Cell, OnceCell, RefCell};
use core::fmt;

use blaze_drive::{Name, Tool};
use blaze_ids::ContentId;
use blaze_lir::body::Target;
use blaze_lir::builder::{Param, Variadic};
use blaze_lir::eval::{Bodies, BodyId, EvalError, Outcome, Trap, Val, run_calls_in};
use blaze_lir::func::{BlockKind, Conv, RefId, RefKind};
use blaze_lir::{Builder, Flags, Full, Func, Ty, VerifyError, View};
use blaze_own::Alloc;
use blaze_typeck::ItemKey;

use super::*;

use crate::engine_run_answer::EngineAnswer;
use crate::run_ids::{Answer, BodyScratch, Checked as Verdict, NotRunnable};

/// The `BodyCheck` kind's tool: the engine's answer takes none (its key is the row's), as
/// engine_run_answer_tests.rs asks it.
const TOOL: Tool = Tool(ContentId([0; 32]));

/// The most bodies one run makes: a table's cap (rule 18). A program past it ends
/// [`Refused::TooManyBodies`], by name.
const MOST_BODIES: usize = 4096;

/// Which toolchain the world's home holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Shelf {
    /// blaze-world's fixture (main.rs tests `shelf`): a stub core, std and bstd.
    Fixture,
    /// The fixture whose core declares the borrow check's lang items (`lang_shelf`).
    Langs,
}

/// A simulated machine with a checkout `ws` and a home `/h`: what a test writes, the engine
/// checks and the program runs in. Its standard output and error are the simulated host's.
pub(super) struct World {
    io: Io,
    shelf: Shelf,
    /// Each file written, `(checkout path, text)`: what [`World::gate`] finds a fn in.
    files: std::vec::Vec<(std::string::String, std::string::String)>,
}

/// What the engine's check of the world said.
#[derive(Debug)]
pub(super) struct CheckSaid {
    /// The command's status (engine_check.rs `Checked::status`).
    pub(super) status: u8,
    /// Each finding's (code, id), in the order listed.
    pub(super) findings: std::vec::Vec<(std::string::String, std::string::String)>,
    /// What the check printed on the world's standard output.
    pub(super) words: std::string::String,
    /// Every noted finding of the answer, as records ([`noted`], D164): what a test asserts
    /// on, never the words.
    pub(super) noted: std::vec::Vec<Noted>,
}

/// One noted finding of a check's answer (engine_lines.rs `NOTED`, blaze_report::finding), its
/// texts owned: a test's record of a body's note.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Noted {
    pub(super) rule: u16,
    pub(super) cause: blaze_report::finding::CauseKind,
    /// Its rule's kind: what it counts as and how the default view draws it.
    pub(super) kind: blaze_report::frame::words::Kind,
    /// Its subject's file (a file's path, a crate's name) and item ("" for a file or a crate).
    pub(super) file: std::string::String,
    pub(super) item: std::string::String,
    /// Its cause key's words (`CauseKey::of`): what the default view groups it by.
    pub(super) of: std::string::String,
    /// As `--each` says it (blaze_report::query `each`).
    pub(super) words: std::string::String,
}

/// Every noted finding of the answer the last check of `io`'s checkout stored, block by block
/// in name order: a query over the stored lines record (engine_lines.rs `noted`), so a
/// certificate's answer gives the findings of the request that wrote it.
pub(super) fn noted(io: &Io) -> std::vec::Vec<Noted> {
    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("the store reads").expect("engine-check stored an answer").expect("its pointer reads");
        let lines = crate::engine_answer::load_lines(cx, store, pointer.lines, opts.heap.shared()).expect("the store reads").expect("its lines record reads");
        let mut all = std::vec::Vec::new();
        for (_, block) in lines.blocks() {
            crate::engine_lines::noted(block, &mut |finding: blaze_report::finding::Finding<'_>| {
                let (file, item) = match finding.subject {
                    blaze_report::finding::Subject::File { path } => (path, ""),
                    blaze_report::finding::Subject::Item { file, item } => (file, item),
                    blaze_report::finding::Subject::Crate { name } => (name, ""),
                };
                let cause = finding.cause.expect("a noted finding has its cause");
                let mut words = std::string::String::new();
                blaze_report::query::each(&finding, &mut words).expect("a noted finding has its words");
                all.push(Noted {
                    rule: finding.rule,
                    cause: cause.kind(),
                    kind: finding.kind().expect("a noted finding's rule has its row"),
                    file: file.to_owned(),
                    item: item.to_owned(),
                    of: cause.key().of.to_owned(),
                    words,
                });
            })
            .expect("the lines record's blocks read");
        }
        all
    })
}

/// The groups the default view draws of `noted` (blaze_report::query `groups`): each cause
/// key's words with how many distinct subjects it holds, in the view's order.
pub(super) fn groups(noted: &[Noted]) -> std::vec::Vec<(std::string::String, usize)> {
    let mut grouped: std::vec::Vec<&Noted> = noted.iter().filter(|one| one.kind == blaze_report::frame::words::Kind::Grouped).collect();
    grouped.sort_by(|a, b| (&a.of, a.cause, &a.file, &a.item).cmp(&(&b.of, b.cause, &b.file, &b.item)));
    grouped.dedup_by(|a, b| (&a.of, a.cause, &a.file, &a.item) == (&b.of, b.cause, &b.file, &b.item));
    let mut out: std::vec::Vec<(std::string::String, usize)> = std::vec::Vec::new();
    for one in grouped {
        match out.last_mut() {
            Some((of, count)) if *of == one.of => *count += 1,
            _ => out.push((one.of.clone(), 1)),
        }
    }
    out
}

/// One fn's verdict at the gate (D151) and the checker key it is of.
#[derive(Clone, Copy, Debug)]
pub(super) struct Gated {
    pub(super) verdict: Verdict,
    /// The first checker key the check covered; `ItemKey(0)` for a check that filed a failure
    /// and covers none, whose verdict refuses it before the key is read.
    pub(super) key: ItemKey,
}

impl Gated {
    /// Whether the run path lets this fn run: the gate's own call (`Checked::runs`).
    pub(super) fn runs(&self) -> bool {
        self.verdict.runs(self.key).is_ok()
    }
}

/// How the program is run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Mode {
    /// On the reference evaluator, with fuel ([`run_lir`]).
    Eval,
    /// As the machine code blaze emits, in this process ([`Gap::LauncherOwnsTheProcess`]).
    Native,
}

/// One run's terms: its fuel (the evaluator's unit: one LIR instruction executed, terminators
/// and calls included), the command line after the program's name, and the mode.
#[derive(Clone, Copy, Debug)]
pub(super) struct Run<'a> {
    pub(super) fuel: u64,
    pub(super) args: &'a [&'a str],
    pub(super) mode: Mode,
}

/// How a run ended.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum Exit {
    /// The entry returned: nothing is 0, an integer is its low 32 bits (the entry's `i32`).
    Code(i32),
    /// The run executed its whole fuel and was not done: deterministic, never a timeout.
    OutOfFuel,
    /// A LIR `trap`, a division by zero or a division's overflow.
    Trapped(Trap),
    /// Undefined behaviour at instruction `inst` of the running body.
    Undefined { inst: usize, why: &'static str },
    /// The entry returned what is no status: `values` values, or one not an integer.
    NotAStatus { values: usize },
    /// A body the run reached was refused before it ran: the gate's verdict, by name.
    Refused(Refused),
    /// What the evaluator does not model (blaze-lir eval.rs `EvalError`), by name.
    Unsupported(EvalError),
    /// The program was checked and not run: what this harness does not have yet.
    NotRun(Gap),
}

/// Why a body the run reached was not made.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum Refused {
    /// D151: the gate gave no proof for `function` (run_parent.rs `ParentError::NotChecked`).
    NotChecked { function: ItemKey, why: NotRunnable },
    /// The source has no body for reference `reference` of `from`.
    NoBody { from: BodyId, reference: u32 },
    /// A body the source gave does not verify: no run takes an unverified body.
    Unverified { body: BodyId, why: VerifyError },
    /// The run reached more than [`MOST_BODIES`] bodies.
    TooManyBodies { most: usize },
}

/// What one run gives back: the world's captured output, the end, the fuel it took and the
/// check's findings.
#[derive(Debug)]
pub(super) struct Ran {
    pub(super) out: std::string::String,
    pub(super) err: std::string::String,
    pub(super) exit: Exit,
    /// The least fuel with which the run ends as it did; the fuel given when it ran out.
    pub(super) fuel_used: u64,
    pub(super) findings: std::vec::Vec<(std::string::String, std::string::String)>,
}

/// What a World's program cannot be run through yet, each one place (FEASIBILITY in the
/// lane's report).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Gap {
    /// G1. blaze-ladder run/make_one.rs `build_one` hands `&built.func` (blaze-build `Built`)
    /// to code_records.rs `make_entry_with_vtables`, which emits it (blaze-emit
    /// record_check.rs `emit_record_measured`) and keeps no LIR; a found record is machine
    /// code only. The missing piece: a sink the maker carries (a `LirSink` beside its
    /// `entry` out-parameter) that receives each made instance's verified `Func` and its
    /// references by symbol, filled only in a test build; then this file's [`LirSource`] is
    /// the run parent's `Lazy` (its gate, its walk, its make) with that sink.
    MakeKeepsNoLir,
    /// G2. The simulated world's toolchain is blaze-world fixture.rs's: its bstd is
    /// `#![no_std] pub mod prelude {}`, so a blaze-edition program's `Cx`, `Out` and
    /// `bstd::main` resolve to nothing. The real shelf would have to be written into the
    /// world's home (read once from the pinned toolchain).
    NoBstdInTheWorld,
    /// G3. A hosted program's every effect is an indirect call through bstd's `HostTable`
    /// (wire.rs; guest.rs `Guest::write`), and the evaluator refuses every function the run's
    /// bodies do not give (eval/data.rs `OUTSIDE`): there is no seam for a host function
    /// answered by the test (a `Bodies::host` that runs bstd hosting.rs `Hosting`'s entries
    /// over the evaluator's memory), and no way to place the table itself in the run's memory
    /// before the entry is called (`run_calls_in` takes only `Val`s).
    NoHostCalls,
    /// G4. The evaluator refuses `invoke` (a call with a landing pad: any call past a value
    /// with drop glue under `panic = unwind`), atomics, and any outside function (`malloc`,
    /// `write`, `pthread_*`): eval.rs module doc, eval/calls.rs. A Rust-edition `println!`
    /// reaches all three.
    NoUnwindNoAtomicsNoLibc,
    /// G5. Native code of the real path runs only in a launcher process. blaze-launch-rt
    /// run_mode.rs `run` RETURNS the entry's status under `End::Return` (D175), but a refused
    /// first call still `_exit`s (`stop_with`), and its signal and fault handlers, its slot
    /// table and its thread-local root are the process's: the run needs a process of its own,
    /// so a World's program cannot run native in this test's process.
    LauncherOwnsTheProcess,
    /// G6. No plant turns the gate off (run_ids.rs `Checked::runs`, run_parent.rs `clear`):
    /// the control that runs an erroring fn has nothing to flip.
    NoGatePlant,
}

impl fmt::Display for Gap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Gap::MakeKeepsNoLir => "G1: the make path keeps no LIR (blaze-ladder run/make_one.rs `build_one` emits it at once)",
            Gap::NoBstdInTheWorld => "G2: the simulated world's bstd is the fixture's stub (blaze-world fixture.rs)",
            Gap::NoHostCalls => "G3: the evaluator has no host-function seam and cannot place a HostTable (blaze-lir eval/data.rs `OUTSIDE`)",
            Gap::NoUnwindNoAtomicsNoLibc => "G4: the evaluator refuses `invoke`, atomics and outside functions (blaze-lir eval.rs)",
            Gap::LauncherOwnsTheProcess => "G5: native code runs only in a launcher process: `run` returns under End::Return, but its stops, handlers, slot table and thread-local root are the process's (blaze-launch-rt run_mode.rs `run`)",
            Gap::NoGatePlant => "G6: no plant turns the gate off (run_ids.rs `Checked::runs`)",
        })
    }
}

impl World {
    /// A fresh simulated machine of seed `seed`, its home's shelf the fixture's.
    pub(super) fn new(seed: u64) -> World {
        World { io: Io::simulated(SimWorld::new(seed, Limits::USUAL)), shelf: Shelf::Fixture, files: std::vec::Vec::new() }
    }

    /// This world with the fixture whose core declares the borrow check's lang items.
    pub(super) fn langs(self) -> World {
        World { shelf: Shelf::Langs, ..self }
    }

    /// This world with `text` written at `path` of the checkout.
    pub(super) fn file(mut self, path: &str, text: &str) -> World {
        write(&self.io, path, text);
        self.files.push((std::string::String::from(path), std::string::String::from(text)));
        self
    }

    fn text(&self, path: &str) -> &str {
        match self.files.iter().rev().find(|(at, _)| at == path) {
            Some((_, text)) => text,
            None => panic!("the world has no file `{path}` (World::file writes one)"),
        }
    }

    /// The engine's check of the checkout, in this process, from its stored answer (the
    /// command's own start): its status, findings and words.
    pub(super) fn check(&self) -> CheckSaid {
        let cx = self.io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let mut opts = super::engine_check_tests::opts(&self.io, 1, false);
        if self.shelf == Shelf::Langs {
            opts.toolchain = lang_shelf(&self.io);
        }
        let before = stdout(&self.io).len();
        let (findings, checked) = super::engine_check_tests::engine_view(&cx, &self.io, opts, blaze_report::frame::plain::View::Person);
        let words = stdout(&self.io).split_off(before);
        CheckSaid { status: checked.status.0, findings, words, noted: noted(&self.io) }
    }

    /// This world with the bytes `bytes` written at `path` of the checkout (a file that is not
    /// text, which [`World::file`] cannot write).
    pub(super) fn bytes(self, path: &str, bytes: &[u8]) -> World {
        {
            let cx = self.io.cx(BUDGET, TASK_FRAME).expect("a root context");
            let root = self.io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(bytes), Durable::No).expect("write");
        }
        self
    }

    /// The world's machine: for a test that drives the command itself (the views a person or
    /// an agent reads) and asks the records with [`noted`].
    pub(super) fn io(&self) -> &Io {
        &self.io
    }

    /// THE GATE of fn `function` of `path` (a top-level fn: blaze-file-items' outline), after
    /// [`World::check`]: its verdict in the engine's stored answer, read as the run path reads
    /// it (`EngineAnswer::body`), and the key the check covered.
    pub(super) fn gate(&self, path: &str, function: &str) -> Gated {
        let check = body_of(path, self.text(path), function);
        super::engine_answer_rows_tests::with_store(&self.io, |cx, store, opts, _| {
            let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
            let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("the store reads").expect("engine-check stored an answer").expect("its pointer reads");
            let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("the store reads").expect("its pages read");
            let mut answer = EngineAnswer::new(&kept.answer, &kept, store);
            let mut scratch = BodyScratch::new(blaze_own::testing::heap().alloc());
            let facts = answer
                .body(cx, check, TOOL, None, &mut scratch)
                .unwrap_or_else(|e| panic!("the answer's row of `{function}` in {path}: {e}"))
                .unwrap_or_else(|| panic!("the engine's answer holds no body row of `{function}` in {path}"));
            Gated { verdict: facts.checked, key: scratch.covers.first().copied().unwrap_or(ItemKey(0)) }
        })
    }

    /// The engine's check, then the program run as `run` says. The check is real; the run of
    /// a World's program ends [`Exit::NotRun`] naming its mode's gap until that gap is built.
    pub(super) fn run(&self, run: Run<'_>) -> Ran {
        let said = self.check();
        let gap = match run.mode {
            Mode::Eval => Gap::MakeKeepsNoLir,
            Mode::Native => Gap::LauncherOwnsTheProcess,
        };
        // No program ran, so its output is empty; the check's words stay in `CheckSaid`.
        Ran { out: std::string::String::new(), err: stderr(&self.io), exit: Exit::NotRun(gap), fuel_used: 0, findings: said.findings }
    }
}

/// What the simulated host's standard error holds.
fn stderr(io: &Io) -> std::string::String {
    let err = io.script(|host| host.err.clone()).expect("a simulated host");
    std::string::String::from_utf8(err).expect("UTF-8")
}

/// The `BodyCheck` name of top-level fn `name` of `source` at checkout path `path`, as
/// work.rs `body_name` makes it.
fn body_of(path: &str, source: &str, name: &str) -> Name {
    let outline = blaze_file_items::outline(source, blaze_surface::Edition::E2024, blaze_own::testing::heap().alloc());
    let item = outline
        .items()
        .iter()
        .find(|item| item.name.is_some_and(|range| item.text(source, range) == name))
        .unwrap_or_else(|| panic!("no top-level fn `{name}` in {path}"));
    crate::work::body_name(Name::file(path.as_bytes()), &outline, item, source)
}

impl Ran {
    fn said(&self) -> std::string::String {
        std::format!("exit {:?}, fuel used {}, out {:?}, err {:?}, findings {:?}", self.exit, self.fuel_used, self.out, self.err, self.findings)
    }

    fn ran(&self) {
        if let Exit::NotRun(gap) = self.exit {
            panic!("the program was checked and not run: {gap}\n{}", self.said());
        }
    }

    /// The program's standard output is `want`.
    pub(super) fn out_is(&self, want: &str) -> &Ran {
        self.ran();
        assert_eq!(self.out, want, "standard output\n{}", self.said());
        self
    }

    /// The entry returned status `code`.
    pub(super) fn exit_is(&self, code: i32) -> &Ran {
        self.ran();
        assert_eq!(self.exit, Exit::Code(code), "the status\n{}", self.said());
        self
    }

    /// The run ended by its fuel, having executed all of it.
    pub(super) fn out_of_fuel(&self) -> &Ran {
        self.ran();
        assert_eq!(self.exit, Exit::OutOfFuel, "a run that should have run out of fuel\n{}", self.said());
        self
    }

    /// The run took at most `most` fuel.
    pub(super) fn fuel_at_most(&self, most: u64) -> &Ran {
        self.ran();
        assert!(self.fuel_used <= most, "the run took {} fuel, past its ceiling {most}\n{}", self.fuel_used, self.said());
        self
    }

    /// The gate's refusal the run stopped at.
    pub(super) fn refused(&self) -> &Refused {
        match &self.exit {
            Exit::Refused(why) => why,
            _ => panic!("a run that should have been refused at a call\n{}", self.said()),
        }
    }
}

/// The program run on the evaluator AND as native code with the same fuel: equal output and
/// equal end, or the panic says which differ (the code generator against the reference
/// evaluator, rule 23's oracle form). A mode that did not run fails by its gap.
pub(super) fn both(world: &World, fuel: u64) -> (Ran, Ran) {
    let eval = world.run(Run { fuel, args: &[], mode: Mode::Eval });
    let native = world.run(Run { fuel, args: &[], mode: Mode::Native });
    eval.ran();
    native.ran();
    assert_eq!(eval.out, native.out, "the evaluator's output and the machine code's\neval: {}\nnative: {}", eval.said(), native.said());
    assert_eq!(eval.exit, native.exit, "the evaluator's end and the machine code's\neval: {}\nnative: {}", eval.said(), native.said());
    (eval, native)
}

/// One body a [`LirSource`] made: its identity and its LIR.
pub(super) struct Made<A: Allocator> {
    pub(super) id: BodyId,
    pub(super) func: Func<A>,
}

/// Where a run's bodies come from, each asked at the run's first call of it: the entry, then
/// the body reference `reference` of body `from` names. The run path's form is G1's sink
/// behind the run parent's gate and make; a test's is built by hand.
pub(super) trait LirSource<A: Allocator> {
    fn entry(&mut self) -> Result<Made<A>, Refused>;
    fn callee(&mut self, from: BodyId, reference: RefId) -> Result<Made<A>, Refused>;
}

/// The evaluator's `Bodies` over a [`LirSource`], lazily: a body is asked of the source at
/// its first call and kept for every later call and run; a refusal is kept for the run's end.
/// The bodies sit in a table of [`MOST_BODIES`] cells made once, never moved, so a view of
/// one stays valid while others are added.
struct Lazy<'s, A: Allocator + Clone, S: LirSource<A>> {
    source: RefCell<&'s mut S>,
    bodies: std::vec::Vec<OnceCell<Func<A>>, A>,
    /// `ids[i]` is `bodies[i]`'s identity.
    ids: RefCell<std::vec::Vec<BodyId, A>>,
    /// `(from, reference, body)` of each call answered.
    calls: RefCell<std::vec::Vec<(BodyId, u32, usize), A>>,
    /// The first refusal of the run going on.
    refused: RefCell<Option<Refused>>,
    /// How many times the source was asked for a callee.
    asked: Cell<u64>,
}

impl<'s, A: Allocator + Clone, S: LirSource<A>> Lazy<'s, A, S> {
    fn new(source: &'s mut S, alloc: A) -> Lazy<'s, A, S> {
        let mut bodies = std::vec::Vec::with_capacity_in(MOST_BODIES, alloc.clone());
        bodies.resize_with(MOST_BODIES, OnceCell::new);
        Lazy {
            source: RefCell::new(source),
            bodies,
            ids: RefCell::new(std::vec::Vec::new_in(alloc.clone())),
            calls: RefCell::new(std::vec::Vec::new_in(alloc)),
            refused: RefCell::new(None),
            asked: Cell::new(0),
        }
    }

    fn slot_of(&self, id: BodyId) -> Option<usize> {
        self.ids.borrow().iter().position(|it| *it == id)
    }

    /// `made` kept (verified first), or the one of its identity already kept: its cell.
    fn keep(&self, made: Made<A>) -> Result<usize, Refused> {
        if let Some(at) = self.slot_of(made.id) {
            return Ok(at);
        }
        if let Err(why) = blaze_lir::verify(made.func.view()) {
            return Err(Refused::Unverified { body: made.id, why });
        }
        let at = self.ids.borrow().len();
        let Some(cell) = self.bodies.get(at) else { return Err(Refused::TooManyBodies { most: MOST_BODIES }) };
        // A cell past the last identity is empty: each is filled once, in order.
        if cell.set(made.func).is_err() {
            return Err(Refused::TooManyBodies { most: at });
        }
        self.ids.borrow_mut().push(made.id);
        Ok(at)
    }

    fn view(&self, at: usize) -> Option<(BodyId, View<'_>)> {
        let id = *self.ids.borrow().get(at)?;
        Some((id, self.bodies.get(at)?.get()?.view()))
    }

    fn entry(&self) -> Result<usize, Refused> {
        let made = self.source.borrow_mut().entry();
        made.and_then(|made| self.keep(made))
    }

    /// The cell of the body reference `reference` of `from` names: asked of the source once.
    fn resolve(&self, from: BodyId, reference: RefId) -> Option<usize> {
        if let Some(&(_, _, at)) = self.calls.borrow().iter().find(|(f, r, _)| *f == from && *r == reference.0) {
            return Some(at);
        }
        self.asked.set(self.asked.get() + 1);
        let made = self.source.borrow_mut().callee(from, reference);
        match made.and_then(|made| self.keep(made)) {
            Ok(at) => {
                self.calls.borrow_mut().push((from, reference.0, at));
                Some(at)
            }
            Err(why) => {
                self.refused.borrow_mut().get_or_insert(why);
                None
            }
        }
    }

    /// One run from the entry (cell `entry`) with `fuel`.
    fn once(&self, entry: usize, fuel: u64, alloc: A) -> Exit {
        self.refused.replace(None);
        let Some((id, view)) = self.view(entry) else { return Exit::Refused(Refused::TooManyBodies { most: 0 }) };
        match run_calls_in(view, id, &[], fuel, self, alloc) {
            Ok(outcome) => exit_of(outcome),
            // The evaluator's "a call whose callee the run's bodies do not give" is the
            // source's refusal when it made one: said by its name.
            Err(error) => match self.refused.borrow().clone() {
                Some(why) => Exit::Refused(why),
                None => Exit::Unsupported(error),
            },
        }
    }
}

impl<A: Allocator + Clone, S: LirSource<A>> Bodies for Lazy<'_, A, S> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let at = self.resolve(from, reference)?;
        self.view(at)
    }

    /// A function's identity for `func_addr`; a static's, thread-local's or vtable's is not
    /// given (the evaluator refuses it by name: statics come with G1's sink).
    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let at = self.resolve(from, reference)?;
        Some(Target::Func(*self.ids.borrow().get(at)?))
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        let at = self.slot_of(id)?;
        Some(self.view(at)?.1)
    }
}

fn exit_of<A: Allocator>(outcome: Outcome<A>) -> Exit {
    match outcome {
        Outcome::Returned(values) => match values[..] {
            [] => Exit::Code(0),
            [Val::Int(bits)] => Exit::Code(bits as u32 as i32),
            _ => Exit::NotAStatus { values: values.len() },
        },
        Outcome::Trapped(trap) => Exit::Trapped(trap),
        Outcome::Undefined { inst, why } => Exit::Undefined { inst, why },
        Outcome::OutOfFuel => Exit::OutOfFuel,
    }
}

/// The least fuel in `1..=ceiling` with which `once` does not run out, for a run that did not
/// run out with `ceiling`. A run is a function of its fuel and stops the same at any fuel at
/// least what it executes, and runs out below it, so bisection finds exactly the instructions
/// it executed; at most 64 runs. (The evaluator returns no count: eval.rs `go` keeps its fuel
/// in a local. One returned field would replace this.)
fn least_fuel(once: &dyn Fn(u64) -> Exit, ceiling: u64) -> u64 {
    // Invariant: `once(lo)` runs out (fuel 0 runs out before the first instruction) and
    // `once(hi)` does not.
    let (mut lo, mut hi) = (0u64, ceiling);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match once(mid) {
            Exit::OutOfFuel => lo = mid,
            _ => hi = mid,
        }
    }
    hi
}

/// The program `source` gives, run on the evaluator with `run`'s fuel, every buffer in
/// `alloc`: its end, and the fuel it took (exactly; [`least_fuel`]). The run at that fuel is
/// run again and must end the same (rule 25). A hosted program's output and command line go
/// through the host (G3), so a run given arguments is not run.
pub(super) fn run_lir<A: Allocator + Clone, S: LirSource<A>>(source: &mut S, run: &Run<'_>, alloc: A) -> (Ran, u64) {
    let ran = |exit, fuel_used| Ran { out: std::string::String::new(), err: std::string::String::new(), exit, fuel_used, findings: std::vec::Vec::new() };
    if !run.args.is_empty() {
        return (ran(Exit::NotRun(Gap::NoHostCalls), 0), 0);
    }
    let lazy = Lazy::new(source, alloc.clone());
    let entry = match lazy.entry() {
        Ok(entry) => entry,
        Err(why) => return (ran(Exit::Refused(why), 0), 0),
    };
    let once = |fuel| lazy.once(entry, fuel, alloc.clone());
    let first = once(run.fuel);
    let used = match first {
        Exit::OutOfFuel => run.fuel,
        _ => {
            let used = least_fuel(&once, run.fuel);
            let again = once(used);
            assert_eq!(again, first, "the run at its least fuel {used} ended otherwise than at its fuel {}", run.fuel);
            used
        }
    };
    (ran(first, used), lazy.asked.get())
}

// ---- Bodies built by hand, for the evaluator's half (it runs today). ----

const MAIN: BodyId = BodyId([1; 32]);
const F: BodyId = BodyId([2; 32]);
/// The checker key a refusal names in [`TwoCalls`].
const F_KEY: ItemKey = ItemKey(0xf);

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `fn main() { loop {} }` as its LIR: an entry block that jumps to a block that jumps to
/// itself.
struct Forever;

impl LirSource<Alloc<'static>> for Forever {
    fn entry(&mut self) -> Result<Made<Alloc<'static>>, Refused> {
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc());
        let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
        b.set_signature(sig);
        let [entry, again] = [(); 2].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        b.jump((again, &[]));
        b.begin(again);
        b.jump((again, &[]));
        Ok(Made { id: MAIN, func: b.finish_checked().expect("the loop builds") })
    }

    fn callee(&mut self, from: BodyId, reference: RefId) -> Result<Made<Alloc<'static>>, Refused> {
        Err(Refused::NoBody { from, reference: reference.0 })
    }
}

/// `fn main() -> i32 { let a = f(); f(); a }` and `fn f() -> i32 { 3 }`; `f` is refused with
/// `refuse` when it is set (the gate's verdict for it).
struct TwoCalls {
    refuse: Option<NotRunnable>,
    /// How many times `f` was made.
    made: u32,
}

impl LirSource<Alloc<'static>> for TwoCalls {
    fn entry(&mut self) -> Result<Made<Alloc<'static>>, Refused> {
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc());
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
        b.set_signature(sig);
        let f = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let first = b.call(Flags::NONE, f, sig, &[]);
        let _second = b.call(Flags::NONE, f, sig, &[]);
        b.ret(&[first]);
        Ok(Made { id: MAIN, func: b.finish_checked().expect("main builds") })
    }

    fn callee(&mut self, from: BodyId, reference: RefId) -> Result<Made<Alloc<'static>>, Refused> {
        if (from, reference.0) != (MAIN, 0) {
            return Err(Refused::NoBody { from, reference: reference.0 });
        }
        if let Some(why) = self.refuse {
            return Err(Refused::NotChecked { function: F_KEY, why });
        }
        self.made += 1;
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc());
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let three = b.iconst(Ty::I32, 3);
        b.ret(&[three]);
        Ok(Made { id: F, func: b.finish_checked().expect("f builds") })
    }
}

fn eval(fuel: u64) -> Run<'static> {
    Run { fuel, args: &[], mode: Mode::Eval }
}

/// `loop {}` ends as `OutOfFuel` having executed exactly the fuel given (by definition of the
/// end: the run stops when its fuel is spent), at two fuels, and twice at one fuel with equal
/// `fuel_used` (rule 25). CONTROL: the program that returns ([`TwoCalls`]) does not run out
/// with the same fuel, so the end is the loop's, not the harness's.
#[test]
fn a_loop_runs_out_of_fuel_at_exactly_its_fuel() {
    for fuel in [1_000u64, 1_000_000] {
        let (once, _) = run_lir(&mut Forever, &eval(fuel), alloc());
        let (twice, _) = run_lir(&mut Forever, &eval(fuel), alloc());
        once.out_of_fuel();
        assert_eq!(once.fuel_used, fuel, "a run that ran out took its whole fuel");
        assert_eq!((&twice.exit, twice.fuel_used), (&once.exit, once.fuel_used), "two runs of one program at one fuel end equal");
    }
    let (returns, _) = run_lir(&mut TwoCalls { refuse: None, made: 0 }, &eval(1_000), alloc());
    returns.exit_is(3);
    println!("world_run: `loop {{}}` ran out at exactly 1000 and 1000000 fuel, twice each equal; control returned 3");
}

/// The callee is asked of the source at the run's FIRST call and kept: `f`, called twice a run
/// and run again by the bisection, is made once. `fuel_used` is every instruction executed:
/// main's two calls and return (3), `f`'s constant and return twice (4): 7. Fuel 7 returns 3;
/// fuel 6 runs out (the boundary is exact). CONTROL: a run given fewer instructions than it
/// needs never returns.
#[test]
fn a_callee_is_made_at_its_first_call_once_and_every_instruction_is_counted() {
    let mut source = TwoCalls { refuse: None, made: 0 };
    let (ran, asked) = run_lir(&mut source, &eval(10_000), alloc());
    ran.exit_is(3);
    assert_eq!(ran.fuel_used, 7, "main's call, call, return and f's constant and return, twice\n{}", ran.said());
    assert_eq!((source.made, asked), (1, 1), "f is made once, at main's first call of it, across every run of the bisection");
    let (exact, _) = run_lir(&mut TwoCalls { refuse: None, made: 0 }, &eval(7), alloc());
    exact.exit_is(3);
    let (short, _) = run_lir(&mut TwoCalls { refuse: None, made: 0 }, &eval(6), alloc());
    short.out_of_fuel();
    println!("world_run: two calls of f took 7 fuel; f made once; fuel 6 ran out");
}

/// A callee the source refuses (the gate's verdict: D151) ends the run AT THE CALL, by the
/// refused fn's key and verdict, never as the evaluator's "no callee". CONTROL (rule 23): the
/// same program with the refusal taken away runs `f` and returns 3.
#[test]
fn a_refused_callee_stops_the_run_by_its_name_at_the_call() {
    let why = NotRunnable::Checked(Verdict::Errors { errors: core::num::NonZeroU32::MIN, first: 0 });
    let (ran, _) = run_lir(&mut TwoCalls { refuse: Some(why), made: 0 }, &eval(10_000), alloc());
    assert_eq!(ran.refused(), &Refused::NotChecked { function: F_KEY, why }, "{}", ran.said());
    assert_eq!(ran.fuel_used, 1, "refused at main's first instruction, the call");
    let (control, _) = run_lir(&mut TwoCalls { refuse: None, made: 0 }, &eval(10_000), alloc());
    control.exit_is(3);
    println!("world_run: a refused f stopped main at its first call; control returned 3");
}

// ---- A World's program. ----

const MANIFEST_2024: &str = "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const MANIFEST_BLAZE: &str = "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n";

/// `main` calls `good`; `bad` has a type error and nothing calls it.
const UNCALLED_ERROR: &str = "fn main() {\n    good();\n}\n\nfn good() {}\n\nfn bad() -> u8 {\n    \"x\"\n}\n";
/// The same with `bad` fixed: the control.
const NO_ERROR: &str = "fn main() {\n    good();\n}\n\nfn good() {}\n\nfn bad() -> u8 {\n    1\n}\n";

/// THE GATE over a real check in this process (D151, `Checked::runs`): `main` and `good` are
/// let run, `bad` (its tail a `&str` where `u8` is declared) is refused with an error verdict,
/// and the check found that one error. CONTROL (rule 23): with `bad` fixed, it is let run and
/// the check finds no error.
#[test]
fn the_gate_lets_clean_bodies_run_and_refuses_one_with_an_error() {
    let world = World::new(21).file("Cargo.toml", MANIFEST_2024).file("src/main.rs", UNCALLED_ERROR);
    let said = world.check();
    assert_eq!(said.status, 1, "a type error fails the check\n{}", said.words);
    for function in ["main", "good"] {
        let gated = world.gate("src/main.rs", function);
        assert!(gated.runs(), "`{function}` is clean and runs: {gated:?}\n{}", said.words);
    }
    let bad = world.gate("src/main.rs", "bad");
    assert!(matches!(bad.verdict, Verdict::Errors { .. }) && !bad.runs(), "`bad` has an error and is refused: {bad:?}\n{}", said.words);
    let control = World::new(21).file("Cargo.toml", MANIFEST_2024).file("src/main.rs", NO_ERROR);
    let fixed = control.check();
    let bad = control.gate("src/main.rs", "bad");
    assert!(bad.runs(), "control: `bad` fixed runs: {bad:?}\n{}", fixed.words);
    println!("world_run: the gate let main and good, refused bad ({:?}); fixed, bad runs", world.gate("src/main.rs", "bad").verdict);
}

/// The owner's hello on the blaze edition, through both modes. The fuel ceiling is 50,000
/// instructions: the hosted hello executes 70 functions and 185 calls on the program's side
/// (crates/blaze-cli/tests/ladder/README.md, "What the hosted hello executes", measured
/// 2026-11-16 under rustc), so about 270 instructions a call at dev; the first real run sets
/// the number it then holds to.
#[test]
#[ignore = "G1 make path keeps no LIR; G2 no bstd in the world; G3 no host calls in the evaluator; G5 native runs only in a launcher process (`run` returns under End::Return; its stops and handlers are the process's)"]
fn hello_on_the_blaze_edition() {
    let world = World::new(1).file("Cargo.toml", MANIFEST_BLAZE).file(
        "src/main.rs",
        "fn main(cx: &Cx, out: Out) -> Result<(), Error> {\n    out.line(cx, \"Hello, world!\")?;\n    Ok(())\n}\n",
    );
    let (eval, _) = both(&world, 50_000);
    eval.out_is("Hello, world!\n").exit_is(0).fuel_at_most(50_000);
}

#[test]
#[ignore = "G1 make path keeps no LIR; G4 `println!` reaches invoke, atomics and libc; G5 native runs only in a launcher process (`run` returns under End::Return; its stops and handlers are the process's)"]
fn hello_on_edition_2024() {
    let world = World::new(2).file("Cargo.toml", MANIFEST_2024).file("src/main.rs", "fn main() {\n    println!(\"Hello, world!\");\n}\n");
    let (eval, _) = both(&world, 500_000);
    eval.out_is("Hello, world!\n").exit_is(0);
}

/// bstd's `main!` declares `bstd_hosted_start` inside a body (blaze-std entry.rs): the shape
/// that the process tests met three times on the real hello.
#[test]
#[ignore = "G1 make path keeps no LIR; G5 native runs only in a launcher process (`run` returns under End::Return; its stops and handlers are the process's)"]
fn main_calls_a_fn_declared_inside_it() {
    let world = World::new(3).file("Cargo.toml", MANIFEST_2024).file(
        "src/main.rs",
        "fn main() {\n    fn inner() -> u8 {\n        7\n    }\n    if inner() != 7 {\n        loop {}\n    }\n}\n",
    );
    let (eval, _) = both(&world, 10_000);
    eval.exit_is(0);
}

/// D151: an error in a fn `main` never calls does not stop the run.
#[test]
#[ignore = "G1 make path keeps no LIR; G5 native runs only in a launcher process (`run` returns under End::Return; its stops and handlers are the process's)"]
fn an_error_in_a_fn_main_does_not_call_runs() {
    let world = World::new(4).file("Cargo.toml", MANIFEST_2024).file("src/main.rs", UNCALLED_ERROR);
    let (eval, _) = both(&world, 10_000);
    eval.exit_is(0);
    assert!(!eval.findings.is_empty(), "the check found `bad`'s error: {:?}", eval.findings);
}

/// D151: an error in a fn `main` calls refuses the run at that call, by the fn's name.
#[test]
#[ignore = "G1 make path keeps no LIR"]
fn an_error_in_a_fn_main_calls_is_refused_by_name() {
    let world = World::new(5).file("Cargo.toml", MANIFEST_2024).file("src/main.rs", "fn main() {\n    let _ = bad();\n}\n\nfn bad() -> u8 {\n    \"x\"\n}\n");
    let ran = world.run(eval(10_000));
    assert!(matches!(ran.refused(), Refused::NotChecked { why: NotRunnable::Checked(Verdict::Errors { .. }), .. }), "{}", ran.said());
}

/// CONTROL of the one above (rule 23): the gate planted off runs `bad`.
#[test]
#[ignore = "G1 make path keeps no LIR; G6 no plant turns the gate off"]
fn with_the_gate_planted_off_the_erroring_fn_runs() {
    let world = World::new(5).file("Cargo.toml", MANIFEST_2024).file("src/main.rs", "fn main() {\n    let _ = bad();\n}\n\nfn bad() -> u8 {\n    \"x\"\n}\n");
    let ran = world.run(eval(10_000));
    assert!(!matches!(ran.exit, Exit::Refused(_) | Exit::NotRun(_)), "{}", ran.said());
}

/// `loop {}` as a World's program: out of fuel at exactly the fuel given, twice equal.
#[test]
#[ignore = "G1 make path keeps no LIR"]
fn a_worlds_loop_runs_out_of_fuel_deterministically() {
    let world = World::new(6).file("Cargo.toml", MANIFEST_2024).file("src/main.rs", "fn main() {\n    loop {}\n}\n");
    let once = world.run(eval(100_000));
    let twice = world.run(eval(100_000));
    once.out_of_fuel();
    assert_eq!((once.fuel_used, twice.fuel_used, &twice.exit), (100_000, 100_000, &once.exit));
}

/// A package's `src/main.rs` whose `handler!()`, at module level, writes `handle_tip` with
/// tracing's callsite static (`concat!(file!(), ":", line!())` through
/// `$crate::__macro_support`: engine_check_tests.rs `CALLSITE_SUPPORT`) and
/// `const LINE: u32 = line!()`, asserted through a const to be the line of `handler!();` plus
/// `shift` (`[0; LINE as usize]` typed `[u8; K]`); `main` calls it.
fn callsite_world(seed: u64, shift: usize) -> World {
    let support = super::engine_check_tests::CALLSITE_SUPPORT;
    let text = std::format!(
        "{support}macro_rules! handler {{\n    () => {{\n        fn handle_tip() -> u8 {{\n            static META: (&str, u8) = ($crate::__macro_support::concat!(file!(), \":\", line!()), 1);\n            const LINE: u32 = line!();\n            let _at: [u8; K] = [0; LINE as usize];\n            META.1\n        }}\n    }};\n}}\n\nhandler!();\n\nfn main() {{\n    let _ = handle_tip();\n}}\n"
    );
    let at = text.lines().position(|l| l.starts_with("handler!();")).expect("the call's line") + 1 + shift;
    let text = text.replace("[u8; K]", &std::format!("[u8; {at}]"));
    World::new(seed).file("Cargo.toml", MANIFEST_2024).file("src/main.rs", &text)
}

/// `line!()` IN A FN A MACRO WROTE AT MODULE LEVEL, IN A PACKAGE (so `file!()` is answered:
/// the given-files fixture of blaze-ladder's tests has no package and refuses it by name).
/// The check is the run path's gate (D151): the check passes with nothing refused, and `main`
/// is let run. TWIN, planted: the asserted line one past the call: the check fails (one error),
/// so the value was read, not refused. Core `line!`: the outermost call's line (front2
/// body_host.rs `Placed::of`, program/lazy seam_rules.rs `body_place`). Before (tempo a2015):
/// "HostError { ask: Place, why: Refused }", the body not checked.
#[test]
fn line_in_a_fn_a_macro_wrote_is_its_calls_line_at_the_gate() {
    let world = callsite_world(31, 0);
    let said = world.check();
    // Not failed (1, as the gate test's type error); a stage not built yet may leave it
    // incomplete, which is no finding of this program.
    assert_ne!(said.status, 1, "clean, `line!()` the call's line\n{}", said.words);    assert!(!said.words.contains("HostError"), "`line!()` and `file!()` answered, never refused\n{}", said.words);
    assert!(world.gate("src/main.rs", "main").runs(), "`main` is let run\n{}", said.words);
    let planted = callsite_world(31, 1);
    let wrong = planted.check();
    assert_eq!(wrong.status, 1, "planted: one line past the call is a type error\n{}", wrong.words);
    assert!(!wrong.words.contains("HostError"), "planted: refused by its value, not by a place\n{}", wrong.words);
    println!("world_run: line!() in a macro-written fn checked as its call's line; one past it failed");
}

/// The same program RUN: `handle_tip` made on the run path (blaze-ladder run/make_one.rs over
/// the names records, with the package's body sources) and the run exits 0.
#[test]
#[ignore = "G1 make path keeps no LIR"]
fn a_fn_a_macro_wrote_with_line_in_eager_input_runs() {
    let ran = callsite_world(32, 0).run(eval(100_000));
    ran.exit_is(0);
}
