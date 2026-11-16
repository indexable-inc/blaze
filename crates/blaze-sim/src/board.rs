//! The case board (docs/v2/plan/design/test.md, "End state: property, differential and
//! end-to-end tests", A, "The case board"; piece 1): the page set a tape-search test's child
//! shares with the test runner. The runner writes the PLAN on a fresh board (the tapes given to
//! run first, the seed range, the per-case budget); the child's `blaze_sim::check` writes its
//! PROGRESS (which check, which case is running, that case's tape, how the run ended, the failing
//! case's record) and later the case's COVERAGE bits. A case's panic ends the child (blaze's
//! builder has no unwinding), and the runner reads the board after the child is gone: it knows
//! the case and its tape exactly, with no `catch_unwind`.
//!
//! A board is plain bytes, read by another process later: fixed offsets, little-endian words,
//! no pointers. [`Board::open`] checks the magic, the version, every reserved byte, every count
//! against its cap and every length against the page BEFORE it reads what the length covers; a
//! board cut short, foreign, of another version, or torn (the child ended between the first and
//! the last write of one step) is refused by name with a [`BoardError`], never read.
//!
//! The form (`u32` and `u64` little-endian; every offset from the board's first byte):
//!
//! - HEADER, 0..64, the runner's: 0 `BZSB`; 4 the version (u32, [`VERSION`]); 8 the board's
//!   length (u32: the bytes of the form; the page may be longer, rounded to pages, and the bytes
//!   past the length are not the board's); 12 the target check (u32, from 1: the check in the
//!   child's process the given tapes and the seed range are for; every other check runs its
//!   declared cases); 16 the given tapes (u32, at most [`GIVEN_CAP`]); 20 their values in all
//!   (u32, at most [`VALUES_CAP`]); 24 the seed range's kind (u32: 0 the check's declared
//!   cases, 1 the range at 28 and 32); 28 the range's count (u32; 0 when declared); 32 its first
//!   seed (u64; 0 when declared); 40 the most choices a case may draw (u32; 0 is the check's
//!   own); 44 the per-case deadline in milliseconds (u32; 0 none; the runner's, which kills the
//!   child at it); 48 the per-case instruction budget (u64; 0 none; the runner's too); 56 the
//!   record capacity (u32, 1 to [`RECORD_CAP`]: the most values a case's tape or record holds
//!   on the board); 60 the coverage bytes (u32, a multiple of 8, at most [`COVERAGE_CAP`]).
//! - PROGRESS, 64..192, the child's: 64 the state word (u64: byte 0 the state, 0 fresh, 1
//!   writing a case's tape, 2 running the case, 3 closing, 4 ended; byte 1 the end, when ended:
//!   1 passed, 2 failed, 3 drew nothing, 4 all discarded, 5 unreached, 6 misfit; byte 2 the
//!   failed case's origin: 1 oracle, 2 harness, 3 error, 4 overran; byte 3 zero; bytes 4..8 the
//!   case, from 1 across the check: the given tapes first, then the seed phase); 72 the check
//!   (u32, from 1 in the process; 0 while no check has begun); 76 the given tapes this check
//!   runs (u32); 80 the check's declared first seed (u64); 88 its declared cases (u32); 92 its
//!   declared shrink runs (u32: the runner's shrinker budget); 96 the most choices in force
//!   (u32); 100 the cases passed before the current one (u32); 104 the cases discarded (u32);
//!   108 the current case's tail (u32: 0 calm, 1 seeded); 112 the choices drawn by the cases
//!   before it (u64); 120 its prefix's length (u32, at most the record capacity); 124 the
//!   failed case's record length (u32); 128 the tail's seed (u64; 0 when calm); 136 the
//!   record's spans (u32); 140 the text's length (u32, at most [`TEXT_CAP`]); 144 the cuts
//!   (u32: bit 0 the record was cut to the capacity, bit 1 the text to its cap); 148 a misfit's
//!   clamped values (u32); 152 its unused values (u32); 156..192 zero.
//! - PLAN, from 192, the runner's: each given tape's length (u32), padded with zeros to 8
//!   bytes; then every tape's values (u64), the tapes in order, each its length long.
//! - THE CASE'S TAPE: the record capacity's u64s: the current case's prefix.
//! - THE RECORD: the record capacity's u64s, the failed case's drawn values in order; then the
//!   capacity's spans, each `start`, `end` (u32), positions in those values.
//! - TEXT: [`TEXT_CAP`] bytes: a failure's text, or an unreached run's missing points joined
//!   by `, `; UTF-8, cut on a character boundary.
//! - COVERAGE: the coverage bytes: one bit per function, as the launcher numbers them.
//!
//! The child writes in an order that makes a cut visible: a step first writes the state word
//! saying it is writing (or closing), then its fields and areas, then the state word saying it
//! is done. Nothing is read while the child runs: the runner opens the board after the child
//! has ended, so the order of the child's writes is the order the runner sees.

#![blaze::sans_io]

use core::fmt;

use crate::search::Origin;
use crate::tape::Record;

pub const MAGIC: [u8; 4] = *b"BZSB";
pub const VERSION: u32 = 1;
/// The setting by which the runner says a board is given; its value is [`DESCRIPTOR`].
pub const SETTING: &str = "BLAZE_SIM_BOARD";
/// The descriptor the board is on, as [`SETTING`] gives it: fixed, as the report's is 3.
pub const DESCRIPTOR: &str = "4";
/// The path that opens the board's descriptor without unsafe code.
pub const DESCRIPTOR_PATH: &str = "/dev/fd/4";
pub const HEADER: usize = 64;
const PROGRESS_AT: usize = 64;
/// The progress block's length.
pub const PROGRESS: usize = 128;
pub const PLAN_AT: usize = 192;
/// The most bytes of a failure's text the board keeps.
pub const TEXT_CAP: usize = at(TEXT_CAP_WORD);
const TEXT_CAP_WORD: u32 = 4096;
/// The most tapes one plan gives (64 regression tapes, 1,024 corpus tapes and room).
pub const GIVEN_CAP: u32 = 4096;
/// The most values all of a plan's tapes hold.
pub const VALUES_CAP: u32 = 1 << 22;
/// The largest record capacity: `Tape::MOST_CHOICES`.
pub const RECORD_CAP: u32 = 1 << 20;
/// The most coverage bytes: a bit for each of 128 Mi functions.
pub const COVERAGE_CAP: u32 = 1 << 24;

// The header's fields.
const LEN: usize = 8;
const TARGET: usize = 12;
const GIVEN_TAPES: usize = 16;
const GIVEN_VALUES: usize = 20;
const SEEDS_KIND: usize = 24;
const SEEDS_COUNT: usize = 28;
const SEEDS_FIRST: usize = 32;
const BUDGET_CHOICES: usize = 40;
const BUDGET_DEADLINE: usize = 44;
const BUDGET_INSTRUCTIONS: usize = 48;
const RECORD_CAPACITY: usize = 56;
const COVERAGE_BYTES: usize = 60;

// The progress block's fields.
const STATE: usize = 64;
const CHECK: usize = 72;
const GIVEN_RUN: usize = 76;
const DECLARED_FIRST: usize = 80;
const DECLARED_CASES: usize = 88;
const DECLARED_SHRINK: usize = 92;
const MOST_CHOICES: usize = 96;
const PASSED: usize = 100;
const DISCARDED: usize = 104;
const TAIL: usize = 108;
const CHOICES: usize = 112;
const PREFIX_LEN: usize = 120;
const RECORD_LEN: usize = 124;
const SEED: usize = 128;
const RECORD_SPANS: usize = 136;
const TEXT_LEN: usize = 140;
const CUTS: usize = 144;
const CLAMPED: usize = 148;
const UNUSED: usize = 152;
const PROGRESS_RESERVED: usize = 156;

const STATE_FRESH: u8 = 0;
const STATE_WRITING: u8 = 1;
const STATE_RUNNING: u8 = 2;
const STATE_CLOSING: u8 = 3;
const STATE_ENDED: u8 = 4;

const END_PASSED: u8 = 1;
const END_FAILED: u8 = 2;
const END_DREW_NOTHING: u8 = 3;
const END_ALL_DISCARDED: u8 = 4;
const END_UNREACHED: u8 = 5;
const END_MISFIT: u8 = 6;

const CUT_RECORD: u32 = 1;
const CUT_TEXT: u32 = 2;

/// Values one write of an area carries: a 512-byte buffer on the stack, so writing allocates
/// nothing.
const CHUNK: usize = 64;

/// Which seeds the target check runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seeds {
    /// The check's own: `Config::first` and `Config::cases` (and what its config was given).
    Declared,
    /// Seeds `first..first + count`; a count of 0 runs the given tapes alone.
    Range { first: u64, count: u32 },
}

/// What one case may spend. 0 in a field is "no budget of this kind" (for `most_choices`: the
/// check's own bound).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Budget {
    pub most_choices: u32,
    /// The runner's: it kills the child at it and reads the board.
    pub deadline_ms: u32,
    /// The runner's, where the host counts a process's user-mode instructions.
    pub instructions: u64,
}

impl Budget {
    pub const NONE: Budget = Budget { most_choices: 0, deadline_ms: 0, instructions: 0 };
}

/// A plan, for [`encode`]: what the runner puts on a fresh board.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spec<'s> {
    /// From 1.
    pub target_check: u32,
    /// Run first, in order, each on a calm tail.
    pub given: &'s [&'s [u64]],
    pub seeds: Seeds,
    pub budget: Budget,
    /// The most values a case's tape or record holds on the board; every given tape fits.
    pub record_cap: u32,
    /// A multiple of 8.
    pub coverage_bytes: u32,
}

/// The board's shape, from its header: its counts, and where each area is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shape {
    /// The board's length in bytes: [`Shape::end`].
    pub len: u32,
    pub target_check: u32,
    pub given_tapes: u32,
    pub given_values: u32,
    pub seeds: Seeds,
    pub budget: Budget,
    pub record_cap: u32,
    pub coverage_bytes: u32,
}

/// A `u32` count as a `usize`.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
#[expect(
    blaze::as_cast,
    reason = "a board's counts are `u32`, and a `usize` holds every `u32` on every target blaze builds for"
)]
const fn at(n: u32) -> usize {
    n as usize
}

/// A `usize` as a `u64`.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
#[expect(
    blaze::as_cast,
    reason = "a `usize` is at most 64 bits on every target blaze builds for, so a `u64` holds every one"
)]
const fn u64_of(n: usize) -> u64 {
    n as u64
}

/// `n` rounded up to a multiple of 8.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
const fn pad8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

impl Shape {
    /// Where the given tapes' values start.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn values_at(&self) -> usize {
        PLAN_AT + pad8(4 * at(self.given_tapes))
    }

    /// Where the current case's tape starts: the end of the plan.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn tape_at(&self) -> usize {
        self.values_at() + 8 * at(self.given_values)
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn record_at(&self) -> usize {
        self.tape_at() + 8 * at(self.record_cap)
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn spans_at(&self) -> usize {
        self.record_at() + 8 * at(self.record_cap)
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn text_at(&self) -> usize {
        self.spans_at() + 8 * at(self.record_cap)
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn coverage_at(&self) -> usize {
        self.text_at() + TEXT_CAP
    }

    /// The board's length: where its coverage ends.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn end(&self) -> usize {
        self.coverage_at() + at(self.coverage_bytes)
    }

    /// The shape of a plan, every count checked against its cap.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn of(spec: &Spec<'_>) -> Result<Shape, BoardError> {
        let given_tapes = capped("given tapes", spec.given.len(), GIVEN_CAP)?;
        let mut values: u64 = 0;
        for (index, tape) in spec.given.iter().enumerate() {
            let len = u64_of(tape.len());
            if len > u64::from(spec.record_cap) {
                return Err(BoardError::GivenTooLong {
                    tape: capped("given tapes", index, GIVEN_CAP)?,
                    len,
                    cap: spec.record_cap,
                });
            }
            values += len;
        }
        let given_values = capped("given values", values, VALUES_CAP)?;
        let shape = Shape {
            len: 0,
            target_check: spec.target_check,
            given_tapes,
            given_values,
            seeds: spec.seeds,
            budget: spec.budget,
            record_cap: spec.record_cap,
            coverage_bytes: spec.coverage_bytes,
        };
        shape.check_counts()?;
        let len = capped("board bytes", shape.end(), u32::MAX)?;
        Ok(Shape { len, ..shape })
    }

    /// What `of` and `open` both ask of the counts.
    fn check_counts(&self) -> Result<(), BoardError> {
        if self.target_check == 0 {
            return Err(BoardError::Bound { what: "the target check (from 1)", value: 0, bound: 1 });
        }
        if self.record_cap == 0 {
            return Err(BoardError::Bound { what: "the record capacity", value: 0, bound: 1 });
        }
        capped("given tapes", self.given_tapes, GIVEN_CAP)?;
        capped("given values", self.given_values, VALUES_CAP)?;
        capped("the record capacity", self.record_cap, RECORD_CAP)?;
        capped("coverage bytes", self.coverage_bytes, COVERAGE_CAP)?;
        if self.coverage_bytes % 8 != 0 {
            return Err(BoardError::Bound {
                what: "coverage bytes (a multiple of 8)",
                value: u64::from(self.coverage_bytes),
                bound: 8,
            });
        }
        Ok(())
    }

    /// The shape a page's header says, checked: `head` holds at least the header's bytes (the
    /// page's first [`HEADER`]), and the page is `page_len` bytes long.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn open(head: &[u8], page_len: usize) -> Result<Shape, BoardError> {
        if head.len() < HEADER {
            return Err(BoardError::Short { len: head.len(), need: HEADER });
        }
        let magic = [head[0], head[1], head[2], head[3]];
        if magic != MAGIC {
            return Err(BoardError::Foreign { magic });
        }
        let version = le32(head, 4);
        if version != VERSION {
            return Err(BoardError::Version { found: version });
        }
        let seeds = match le32(head, SEEDS_KIND) {
            0 => {
                if le32(head, SEEDS_COUNT) != 0 {
                    return Err(BoardError::Reserved { at: SEEDS_COUNT });
                }
                if le64(head, SEEDS_FIRST) != 0 {
                    return Err(BoardError::Reserved { at: SEEDS_FIRST });
                }
                Seeds::Declared
            }
            1 => Seeds::Range { first: le64(head, SEEDS_FIRST), count: le32(head, SEEDS_COUNT) },
            value => return Err(BoardError::Tag { what: "the seed range's kind", value }),
        };
        let said = le32(head, LEN);
        let shape = Shape {
            len: said,
            target_check: le32(head, TARGET),
            given_tapes: le32(head, GIVEN_TAPES),
            given_values: le32(head, GIVEN_VALUES),
            seeds,
            budget: Budget {
                most_choices: le32(head, BUDGET_CHOICES),
                deadline_ms: le32(head, BUDGET_DEADLINE),
                instructions: le64(head, BUDGET_INSTRUCTIONS),
            },
            record_cap: le32(head, RECORD_CAPACITY),
            coverage_bytes: le32(head, COVERAGE_BYTES),
        };
        shape.check_counts()?;
        if at(said) != shape.end() {
            return Err(BoardError::Misshapen { said, shape: shape.end() });
        }
        if page_len < shape.end() {
            return Err(BoardError::Short { len: page_len, need: shape.end() });
        }
        Ok(shape)
    }
}

/// `value` as a `u32` no larger than `cap`, else refused naming `what`.
fn capped<N: TryInto<u64> + Copy>(what: &'static str, value: N, cap: u32) -> Result<u32, BoardError> {
    let Ok(wide) = value.try_into() else {
        return Err(BoardError::OverCap { what, value: u64::MAX, cap: u64::from(cap) });
    };
    if wide > u64::from(cap) {
        return Err(BoardError::OverCap { what, value: wide, cap: u64::from(cap) });
    }
    // `wide <= cap`, a `u32`.
    u32::try_from(wide).map_err(|_| BoardError::OverCap { what, value: wide, cap: u64::from(cap) })
}

/// Writes the plan of `spec` on a fresh board in `out` (its first `Shape::end` bytes, the rest
/// untouched) and gives its shape. The progress, the areas and the coverage are zero: a fresh
/// board. Allocates nothing.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn encode(spec: &Spec<'_>, out: &mut [u8]) -> Result<Shape, BoardError> {
    let shape = Shape::of(spec)?;
    let end = shape.end();
    if out.len() < end {
        return Err(BoardError::Short { len: out.len(), need: end });
    }
    let board = &mut out[..end];
    board.fill(0);
    board[..4].copy_from_slice(&MAGIC);
    put32(board, 4, VERSION);
    put32(board, LEN, shape.len);
    put32(board, TARGET, shape.target_check);
    put32(board, GIVEN_TAPES, shape.given_tapes);
    put32(board, GIVEN_VALUES, shape.given_values);
    match shape.seeds {
        Seeds::Declared => {}
        Seeds::Range { first, count } => {
            put32(board, SEEDS_KIND, 1);
            put32(board, SEEDS_COUNT, count);
            put64(board, SEEDS_FIRST, first);
        }
    }
    put32(board, BUDGET_CHOICES, shape.budget.most_choices);
    put32(board, BUDGET_DEADLINE, shape.budget.deadline_ms);
    put64(board, BUDGET_INSTRUCTIONS, shape.budget.instructions);
    put32(board, RECORD_CAPACITY, shape.record_cap);
    put32(board, COVERAGE_BYTES, shape.coverage_bytes);
    let mut length_at = PLAN_AT;
    let mut value_at = shape.values_at();
    for tape in spec.given {
        // `Shape::of` held every tape to the record capacity, a `u32`.
        put32(board, length_at, capped("a given tape's length", tape.len(), shape.record_cap)?);
        length_at += 4;
        for value in *tape {
            put64(board, value_at, *value);
            value_at += 8;
        }
    }
    Ok(shape)
}

/// The plan as a board holds it, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Plan<'b> {
    lengths: &'b [u8],
    values: &'b [u8],
    tapes: u32,
    longest: u32,
}

impl<'b> Plan<'b> {
    /// The plan of a board of `shape`, from `front`: the board's bytes through the plan's end
    /// (`Shape::tape_at`) at least. Each length is checked against the record capacity and their
    /// sum against the header's before a value is read.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn open(shape: &Shape, front: &'b [u8]) -> Result<Plan<'b>, BoardError> {
        let end = shape.tape_at();
        if front.len() < end {
            return Err(BoardError::Short { len: front.len(), need: end });
        }
        let lengths_end = PLAN_AT + 4 * at(shape.given_tapes);
        if let Some(stray) = front[lengths_end..shape.values_at()].iter().position(|byte| *byte != 0)
        {
            return Err(BoardError::Reserved { at: lengths_end + stray });
        }
        let lengths = &front[PLAN_AT..lengths_end];
        let mut sum: u64 = 0;
        let mut longest = 0;
        for (index, length) in lengths.chunks_exact(4).enumerate() {
            let len = le32(length, 0);
            if len > shape.record_cap {
                return Err(BoardError::GivenTooLong {
                    tape: capped("given tapes", index, GIVEN_CAP)?,
                    len: u64::from(len),
                    cap: shape.record_cap,
                });
            }
            longest = longest.max(len);
            sum += u64::from(len);
        }
        if sum != u64::from(shape.given_values) {
            return Err(BoardError::GivenSum { said: shape.given_values, sum });
        }
        Ok(Plan {
            lengths,
            values: &front[shape.values_at()..end],
            tapes: shape.given_tapes,
            longest,
        })
    }

    /// How many tapes the plan gives.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn tapes_len(&self) -> u32 {
        self.tapes
    }

    /// The longest given tape's length: the buffer `GivenTape::copy_into` needs.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn longest(&self) -> usize {
        at(self.longest)
    }

    /// The given tapes, in order.
    pub fn tapes(&self) -> impl Iterator<Item = GivenTape<'b>> + 'b {
        let (lengths, values) = (self.lengths, self.values);
        let mut from = 0;
        lengths.chunks_exact(4).map(move |length| {
            let len = 8 * at(le32(length, 0));
            let tape = GivenTape { values: &values[from..from + len] };
            from += len;
            tape
        })
    }
}

/// One given tape, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GivenTape<'b> {
    values: &'b [u8],
}

impl<'b> GivenTape<'b> {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn len(&self) -> usize {
        self.values.len() / 8
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn values(&self) -> impl Iterator<Item = u64> + 'b {
        let values = self.values;
        values.chunks_exact(8).map(|value| le64(value, 0))
    }

    /// The values into the front of `out`, which is given: refused when `out` is shorter.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn copy_into<'o>(&self, out: &'o mut [u64]) -> Result<&'o [u64], BoardError> {
        let len = self.len();
        if out.len() < len {
            return Err(BoardError::Bound {
                what: "a given tape against the buffer it is copied into",
                value: u64_of(len),
                bound: u64_of(out.len()),
            });
        }
        for (slot, value) in out.iter_mut().zip(self.values()) {
            *slot = value;
        }
        Ok(&out[..len])
    }
}

/// What a case's tape hands out past its prefix, as the board says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TailWord {
    Calm,
    /// The stream of this seed (`SplitMix64::new(seed)`).
    Seeded(u64),
}

/// A case's tape on the board: its prefix and its tail. With a calm tail the prefix is the
/// tape; with a seeded one the prefix and then the seed's stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CaseTape<'b> {
    pub tail: TailWord,
    prefix: &'b [u8],
}

impl<'b> CaseTape<'b> {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn prefix_len(&self) -> usize {
        self.prefix.len() / 8
    }

    pub fn prefix(&self) -> impl Iterator<Item = u64> + 'b {
        let prefix = self.prefix;
        prefix.chunks_exact(8).map(|value| le64(value, 0))
    }
}

/// A failed case's record on the board: the values it drew, in order, and its spans.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CaseRecord<'b> {
    values: &'b [u8],
    spans: &'b [u8],
    /// The case drew more than the record capacity, or had spans past it: the record holds the
    /// first values and the spans inside them.
    pub cut: bool,
}

impl<'b> CaseRecord<'b> {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn len(&self) -> usize {
        self.values.len() / 8
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn values(&self) -> impl Iterator<Item = u64> + 'b {
        let values = self.values;
        values.chunks_exact(8).map(|value| le64(value, 0))
    }

    /// Each span's `(start, end)`, positions in `values`.
    pub fn spans(&self) -> impl Iterator<Item = (u32, u32)> + 'b {
        let spans = self.spans;
        spans.chunks_exact(8).map(|span| (le32(span, 0), le32(span, 4)))
    }
}

/// Where a failed case's failure came from, as the board says it. A panic is not one: a panic
/// ends the child, and the board says the case was running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaseOrigin {
    Oracle,
    Harness,
    Error,
    Overran,
}

impl CaseOrigin {
    /// The origin of a failure the body returned; a panic's is refused by name.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn of(origin: &Origin) -> Result<CaseOrigin, BoardError> {
        match origin {
            Origin::Oracle => Ok(CaseOrigin::Oracle),
            Origin::Harness => Ok(CaseOrigin::Harness),
            Origin::Error => Ok(CaseOrigin::Error),
            Origin::Overran => Ok(CaseOrigin::Overran),
            Origin::Panic { .. } => Err(BoardError::PanicReturned),
        }
    }

    /// The search's origin of this one.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn origin(self) -> Origin {
        match self {
            CaseOrigin::Oracle => Origin::Oracle,
            CaseOrigin::Harness => Origin::Harness,
            CaseOrigin::Error => Origin::Error,
            CaseOrigin::Overran => Origin::Overran,
        }
    }

    const fn number(self) -> u8 {
        match self {
            CaseOrigin::Oracle => 1,
            CaseOrigin::Harness => 2,
            CaseOrigin::Error => 3,
            CaseOrigin::Overran => 4,
        }
    }

    fn of_number(value: u8) -> Result<CaseOrigin, BoardError> {
        match value {
            1 => Ok(CaseOrigin::Oracle),
            2 => Ok(CaseOrigin::Harness),
            3 => Ok(CaseOrigin::Error),
            4 => Ok(CaseOrigin::Overran),
            other => Err(BoardError::Tag { what: "a failed case's origin", value: u32::from(other) }),
        }
    }
}

/// A check's declared run, as its config says it: what the runner shrinks and replays with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Declared {
    pub first: u64,
    pub cases: u32,
    pub shrink_runs: u32,
    /// The most choices in force for this check's cases (the plan's budget when it gave one).
    pub most_choices: u32,
}

/// The cases before the current one: passed, discarded, and the choices they drew.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Tally {
    pub passed: u32,
    pub discarded: u32,
    pub choices: u64,
}

impl Tally {
    pub const ZERO: Tally = Tally { passed: 0, discarded: 0, choices: 0 };
}

/// What a check on the board has told so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Counts {
    /// From 1, in the process.
    pub check: u32,
    /// The given tapes this check runs (the plan's when it is the target, else 0).
    pub given_run: u32,
    pub declared: Declared,
    pub tally: Tally,
}

/// How a check's run ended, as the board says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum End<'b> {
    Passed,
    /// The case in [`Progress::Ended`]'s `case` returned a failure.
    Failed { origin: CaseOrigin, record: CaseRecord<'b>, text: &'b str, text_cut: bool },
    DrewNothing,
    AllDiscarded,
    /// The points no passing case reached, joined by `, `.
    Unreached { missing: &'b str, cut: bool },
    /// A tape the check's own config gave did not fit the case.
    Misfit { clamped: u32, unused: u32 },
}

/// Where the child got to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Progress<'b> {
    /// No check has begun: the board is as the runner wrote it.
    Untouched,
    /// A check began and has started no case.
    Begun(Counts),
    /// The case was running: a child that is gone ended in it (a panic, a kill at the deadline).
    Running { counts: Counts, case: u32, tape: CaseTape<'b> },
    /// The check's run ended; `case` is the last case it ran, `tape` that case's tape.
    Ended { counts: Counts, case: u32, tape: CaseTape<'b>, end: End<'b> },
}

/// A board, opened: its shape, its plan and the child's progress, every length checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Board<'b> {
    shape: Shape,
    plan: Plan<'b>,
    progress: Progress<'b>,
    coverage: &'b [u8],
}

impl<'b> Board<'b> {
    /// Opens the board on `page` (the page may be longer than the board).
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn open(page: &'b [u8]) -> Result<Board<'b>, BoardError> {
        Board::open_with(page, false)
    }

    /// `open` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) fn open_planted(page: &'b [u8], plant: Plant) -> Result<Board<'b>, BoardError> {
        Board::open_with(page, plant == Plant::OpensTorn)
    }

    fn open_with(page: &'b [u8], opens_torn: bool) -> Result<Board<'b>, BoardError> {
        let shape = Shape::open(page, page.len())?;
        let board = &page[..shape.end()];
        let plan = Plan::open(&shape, board)?;
        let progress = progress(&shape, board, opens_torn)?;
        Ok(Board { shape, plan, progress, coverage: &board[shape.coverage_at()..] })
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn shape(&self) -> Shape {
        self.shape
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn plan(&self) -> Plan<'b> {
        self.plan
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn progress(&self) -> Progress<'b> {
        self.progress
    }

    /// The coverage bits, `Shape::coverage_bytes` long.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn coverage(&self) -> &'b [u8] {
        self.coverage
    }
}

/// The progress block of `board` (its whole length, checked by `Shape::open`), read.
fn progress<'b>(shape: &Shape, board: &'b [u8], opens_torn: bool) -> Result<Progress<'b>, BoardError> {
    if let Some(stray) = board[PROGRESS_RESERVED..PLAN_AT].iter().position(|byte| *byte != 0) {
        return Err(BoardError::Reserved { at: PROGRESS_RESERVED + stray });
    }
    if board[STATE + 3] != 0 {
        return Err(BoardError::Reserved { at: STATE + 3 });
    }
    let (state, end_tag, origin, case) =
        (board[STATE], board[STATE + 1], board[STATE + 2], le32(board, STATE + 4));
    let check = le32(board, CHECK);
    let state = match state {
        STATE_WRITING if opens_torn => STATE_RUNNING,
        STATE_WRITING => return Err(BoardError::Torn { case, state: "writing the case's tape" }),
        STATE_CLOSING => return Err(BoardError::Torn { case, state: "closing the run" }),
        STATE_FRESH | STATE_RUNNING | STATE_ENDED => state,
        other => return Err(BoardError::Tag { what: "the board's state", value: u32::from(other) }),
    };
    if state != STATE_ENDED && (end_tag != 0 || origin != 0) {
        return Err(BoardError::Reserved { at: STATE + 1 });
    }
    if state == STATE_ENDED && end_tag != END_FAILED && origin != 0 {
        return Err(BoardError::Reserved { at: STATE + 2 });
    }
    if state == STATE_FRESH {
        if case != 0 {
            return Err(BoardError::Bound { what: "the case of a board with none started", value: u64::from(case), bound: 0 });
        }
        if check == 0 {
            if let Some(stray) = board[PROGRESS_AT..PLAN_AT].iter().position(|byte| *byte != 0) {
                return Err(BoardError::Reserved { at: PROGRESS_AT + stray });
            }
            return Ok(Progress::Untouched);
        }
    }
    if check == 0 {
        return Err(BoardError::Bound { what: "the check of a board in use (from 1)", value: 0, bound: 1 });
    }
    let counts = Counts {
        check,
        given_run: le32(board, GIVEN_RUN),
        declared: Declared {
            first: le64(board, DECLARED_FIRST),
            cases: le32(board, DECLARED_CASES),
            shrink_runs: le32(board, DECLARED_SHRINK),
            most_choices: le32(board, MOST_CHOICES),
        },
        tally: Tally {
            passed: le32(board, PASSED),
            discarded: le32(board, DISCARDED),
            choices: le64(board, CHOICES),
        },
    };
    if counts.given_run > shape.given_tapes {
        return Err(BoardError::Bound {
            what: "the given tapes a check runs, against the plan's",
            value: u64::from(counts.given_run),
            bound: u64::from(shape.given_tapes),
        });
    }
    if state == STATE_FRESH {
        return Ok(Progress::Begun(counts));
    }
    if state == STATE_RUNNING && case == 0 {
        return Err(BoardError::Bound { what: "the running case (from 1)", value: 0, bound: 1 });
    }
    let tape = case_tape(shape, board)?;
    if state == STATE_RUNNING {
        return Ok(Progress::Running { counts, case, tape });
    }
    let end = match end_tag {
        END_PASSED => End::Passed,
        END_FAILED => failed_end(shape, board, origin)?,
        END_DREW_NOTHING => End::DrewNothing,
        END_ALL_DISCARDED => End::AllDiscarded,
        END_UNREACHED => {
            let (missing, cut) = text(shape, board)?;
            End::Unreached { missing, cut }
        }
        END_MISFIT => End::Misfit { clamped: le32(board, CLAMPED), unused: le32(board, UNUSED) },
        other => return Err(BoardError::Tag { what: "how the run ended", value: u32::from(other) }),
    };
    Ok(Progress::Ended { counts, case, tape, end })
}

/// The current case's tape, its prefix length checked against the capacity.
fn case_tape<'b>(shape: &Shape, board: &'b [u8]) -> Result<CaseTape<'b>, BoardError> {
    let seed = le64(board, SEED);
    let tail = match le32(board, TAIL) {
        0 if seed == 0 => TailWord::Calm,
        0 => return Err(BoardError::Reserved { at: SEED }),
        1 => TailWord::Seeded(seed),
        value => return Err(BoardError::Tag { what: "a case's tail", value }),
    };
    let prefix_len = le32(board, PREFIX_LEN);
    if prefix_len > shape.record_cap {
        return Err(BoardError::Bound {
            what: "a case's prefix, against the record capacity",
            value: u64::from(prefix_len),
            bound: u64::from(shape.record_cap),
        });
    }
    let from = shape.tape_at();
    Ok(CaseTape { tail, prefix: &board[from..from + 8 * at(prefix_len)] })
}

/// A failed end: its origin, its record (lengths and every span checked) and its text.
fn failed_end<'b>(shape: &Shape, board: &'b [u8], origin: u8) -> Result<End<'b>, BoardError> {
    let origin = CaseOrigin::of_number(origin)?;
    let cuts = le32(board, CUTS);
    if cuts & !(CUT_RECORD | CUT_TEXT) != 0 {
        return Err(BoardError::Reserved { at: CUTS });
    }
    let record_len = le32(board, RECORD_LEN);
    let spans = le32(board, RECORD_SPANS);
    for (what, value) in [("a record's values", record_len), ("a record's spans", spans)] {
        if value > shape.record_cap {
            return Err(BoardError::Bound {
                what,
                value: u64::from(value),
                bound: u64::from(shape.record_cap),
            });
        }
    }
    let values = &board[shape.record_at()..shape.record_at() + 8 * at(record_len)];
    let spans = &board[shape.spans_at()..shape.spans_at() + 8 * at(spans)];
    for span in spans.chunks_exact(8) {
        let (start, end) = (le32(span, 0), le32(span, 4));
        if start > end || end > record_len {
            return Err(BoardError::Bound {
                what: "a span's end, against its start and the record's values",
                value: u64::from(end),
                bound: u64::from(record_len),
            });
        }
    }
    let (text, text_cut) = text(shape, board)?;
    Ok(End::Failed {
        origin,
        record: CaseRecord { values, spans, cut: cuts & CUT_RECORD != 0 },
        text,
        text_cut,
    })
}

/// The text area's text and whether it was cut, its length checked and its bytes UTF-8.
fn text<'b>(shape: &Shape, board: &'b [u8]) -> Result<(&'b str, bool), BoardError> {
    let len = le32(board, TEXT_LEN);
    if at(len) > TEXT_CAP {
        return Err(BoardError::Bound {
            what: "a text",
            value: u64::from(len),
            bound: u64::from(TEXT_CAP_WORD),
        });
    }
    let bytes = &board[shape.text_at()..shape.text_at() + at(len)];
    let Ok(text) = core::str::from_utf8(bytes) else { return Err(BoardError::NotText) };
    Ok((text, le32(board, CUTS) & CUT_TEXT != 0))
}

/// Where the board's bytes are written: the shared page itself, a file under it, or a test's
/// buffer. Writes land in call order.
pub trait Page {
    /// Writes `bytes` at the board's offset `at`.
    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<(), PageError>;
}

/// A board in a buffer of the caller's: a test's, or one the caller maps.
pub struct Bytes<'p>(pub &'p mut [u8]);

impl Page for Bytes<'_> {
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<(), PageError> {
        let refused = PageError { at, len: bytes.len(), os: None };
        let Some(end) = at.checked_add(bytes.len()) else { return Err(refused) };
        let Some(into) = self.0.get_mut(at..end) else { return Err(refused) };
        into.copy_from_slice(bytes);
        Ok(())
    }
}

/// A write the page did not take: where, how long, and the system's error number when there is
/// one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageError {
    pub at: usize,
    pub len: usize,
    pub os: Option<i32>,
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.os {
            Some(code) => write!(f, "{} bytes at {} were not written (system error {code})", self.len, self.at),
            None => write!(f, "{} bytes at {} are past the page's end", self.len, self.at),
        }
    }
}

/// How a run ends, for [`Writer::end`].
#[derive(Clone, Copy, Debug)]
pub enum Ending<'e> {
    Passed,
    Failed { origin: CaseOrigin, record: &'e Record, text: &'e str },
    DrewNothing,
    AllDiscarded,
    Unreached { missing: &'e [&'static str] },
    Misfit { clamped: u32, unused: u32 },
}

/// A bug a control plants in the board's reader or writer.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The reader takes a board whose state says "writing the case's tape" as running.
    OpensTorn,
    /// The writer writes a seeded case's seed one too high.
    SeedOffByOne,
}

/// The child's side of a board: each step writes the state word first, its fields and areas,
/// then the state word again, so a cut between is seen as torn. Allocates nothing.
pub struct Writer<P: Page> {
    page: P,
    shape: Shape,
    #[cfg(test)]
    plant: Plant,
}

impl<P: Page> Writer<P> {
    /// The writer of a board of `shape` (as `Shape::open` read it) on `page`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(page: P, shape: Shape) -> Writer<P> {
        Writer {
            page,
            shape,
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    /// `new` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) const fn planted(page: P, shape: Shape, plant: Plant) -> Writer<P> {
        Writer { page, shape, plant }
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn shape(&self) -> Shape {
        self.shape
    }

    /// The page back.
    pub fn into_page(self) -> P {
        self.page
    }

    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<(), BoardError> {
        self.page.put(at, bytes).map_err(BoardError::Page)
    }

    fn put_state(&mut self, state: u8, end: u8, origin: u8, case: u32) -> Result<(), BoardError> {
        let mut word = [0u8; 8];
        word[0] = state;
        word[1] = end;
        word[2] = origin;
        word[4..].copy_from_slice(&case.to_le_bytes());
        self.put(STATE, &word)
    }

    /// A check begins: the whole progress block, fresh, in one write.
    pub fn begin(&mut self, check: u32, given_run: u32, declared: Declared) -> Result<(), BoardError> {
        if check == 0 {
            return Err(BoardError::Bound { what: "a check (from 1)", value: 0, bound: 1 });
        }
        let mut block = [0u8; PROGRESS];
        let field = |at: usize| at - PROGRESS_AT;
        put32(&mut block, field(CHECK), check);
        put32(&mut block, field(GIVEN_RUN), given_run);
        put64(&mut block, field(DECLARED_FIRST), declared.first);
        put32(&mut block, field(DECLARED_CASES), declared.cases);
        put32(&mut block, field(DECLARED_SHRINK), declared.shrink_runs);
        put32(&mut block, field(MOST_CHOICES), declared.most_choices);
        self.put(PROGRESS_AT, &block)
    }

    /// Case `case` starts on `tail` after `prefix`, with `tally` the cases before it.
    pub fn start_case(&mut self, case: u32, tally: Tally, tail: TailWord, prefix: &[u64]) -> Result<(), BoardError> {
        let Ok(prefix_len) = capped("a case's prefix", prefix.len(), self.shape.record_cap) else {
            return Err(BoardError::GivenTooLong {
                tape: case,
                len: u64_of(prefix.len()),
                cap: self.shape.record_cap,
            });
        };
        #[cfg(test)]
        let tail = match (self.plant, tail) {
            (Plant::SeedOffByOne, TailWord::Seeded(seed)) => TailWord::Seeded(seed.wrapping_add(1)),
            (Plant::None | Plant::OpensTorn | Plant::SeedOffByOne, tail) => tail,
        };
        self.put_state(STATE_WRITING, 0, 0, case)?;
        // 100..136: the tally, the tail, the prefix's length, no record yet; then the seed.
        let mut fields = [0u8; SEED + 8 - PASSED];
        let field = |at: usize| at - PASSED;
        put32(&mut fields, field(PASSED), tally.passed);
        put32(&mut fields, field(DISCARDED), tally.discarded);
        put64(&mut fields, field(CHOICES), tally.choices);
        put32(&mut fields, field(PREFIX_LEN), prefix_len);
        match tail {
            TailWord::Calm => {}
            TailWord::Seeded(seed) => {
                put32(&mut fields, field(TAIL), 1);
                put64(&mut fields, field(SEED), seed);
            }
        }
        self.put(PASSED, &fields)?;
        self.put_values(self.shape.tape_at(), prefix.iter().copied())?;
        self.put_state(STATE_RUNNING, 0, 0, case)
    }

    /// The run ended after case `case` (0 when it ran none), with `tally` every case it ran.
    pub fn end(&mut self, case: u32, tally: Tally, ending: Ending<'_>) -> Result<(), BoardError> {
        self.put_state(STATE_CLOSING, 0, 0, case)?;
        let mut cuts = 0;
        let mut text_len = 0;
        let mut record_len = 0;
        let mut spans = 0;
        let (mut clamped, mut unused) = (0, 0);
        let (end, origin) = match ending {
            Ending::Passed => (END_PASSED, 0),
            Ending::Failed { origin, record, text } => {
                let cap = at(self.shape.record_cap);
                let kept = record.choices.len().min(cap);
                self.put_values(self.shape.record_at(), record.choices[..kept].iter().map(|choice| choice.value))?;
                let inside = record.spans.iter().filter(|span| at(span.end) <= kept).take(cap);
                spans = self.put_spans(inside)?;
                if kept < record.choices.len() || at(spans) < record.spans.len() {
                    cuts |= CUT_RECORD;
                }
                // `kept` is at most the capacity, a `u32`.
                record_len = capped("a record's values", kept, self.shape.record_cap)?;
                let (len, cut) = self.put_text([text].into_iter())?;
                text_len = len;
                if cut {
                    cuts |= CUT_TEXT;
                }
                (END_FAILED, origin.number())
            }
            Ending::DrewNothing => (END_DREW_NOTHING, 0),
            Ending::AllDiscarded => (END_ALL_DISCARDED, 0),
            Ending::Unreached { missing } => {
                let (len, cut) = self.put_text(missing.iter().copied())?;
                text_len = len;
                if cut {
                    cuts |= CUT_TEXT;
                }
                (END_UNREACHED, 0)
            }
            Ending::Misfit { clamped: lowered, unused: left } => {
                clamped = lowered;
                unused = left;
                (END_MISFIT, 0)
            }
        };
        let mut tallied = [0u8; CHOICES + 8 - PASSED];
        put32(&mut tallied, 0, tally.passed);
        put32(&mut tallied, DISCARDED - PASSED, tally.discarded);
        put64(&mut tallied, CHOICES - PASSED, tally.choices);
        // TAIL, inside the span, is the last case's: written back as it is.
        self.put(PASSED, &tallied[..TAIL - PASSED])?;
        self.put(CHOICES, &tallied[CHOICES - PASSED..])?;
        let mut record = [0u8; 4];
        record.copy_from_slice(&record_len.to_le_bytes());
        self.put(RECORD_LEN, &record)?;
        let mut rest = [0u8; PROGRESS_RESERVED - RECORD_SPANS];
        put32(&mut rest, 0, spans);
        put32(&mut rest, TEXT_LEN - RECORD_SPANS, text_len);
        put32(&mut rest, CUTS - RECORD_SPANS, cuts);
        put32(&mut rest, CLAMPED - RECORD_SPANS, clamped);
        put32(&mut rest, UNUSED - RECORD_SPANS, unused);
        self.put(RECORD_SPANS, &rest)?;
        self.put_state(STATE_ENDED, end, origin, case)
    }

    /// The current case's coverage bits, `Shape::coverage_bytes` long.
    pub fn coverage(&mut self, bits: &[u8]) -> Result<(), BoardError> {
        if bits.len() != at(self.shape.coverage_bytes) {
            return Err(BoardError::Coverage { given: bits.len(), shape: self.shape.coverage_bytes });
        }
        self.put(self.shape.coverage_at(), bits)
    }

    /// `values` as u64s from `from`, a chunk to a write.
    fn put_values(&mut self, from: usize, values: impl Iterator<Item = u64>) -> Result<(), BoardError> {
        let mut chunk = [0u8; CHUNK * 8];
        let mut filled = 0;
        let mut at = from;
        for value in values {
            chunk[filled..filled + 8].copy_from_slice(&value.to_le_bytes());
            filled += 8;
            if filled == chunk.len() {
                self.put(at, &chunk)?;
                at += filled;
                filled = 0;
            }
        }
        if filled > 0 {
            self.put(at, &chunk[..filled])?;
        }
        Ok(())
    }

    /// The spans as `start`, `end` pairs into the spans area; gives how many.
    fn put_spans<'s>(&mut self, spans: impl Iterator<Item = &'s crate::tape::Span>) -> Result<u32, BoardError> {
        let mut chunk = [0u8; CHUNK * 8];
        let mut filled = 0;
        let mut at = self.shape.spans_at();
        let mut count: u32 = 0;
        for span in spans {
            put32(&mut chunk, filled, span.start);
            put32(&mut chunk, filled + 4, span.end);
            filled += 8;
            count += 1;
            if filled == chunk.len() {
                self.put(at, &chunk)?;
                at += filled;
                filled = 0;
            }
        }
        if filled > 0 {
            self.put(at, &chunk[..filled])?;
        }
        Ok(count)
    }

    /// `pieces` joined by `, ` into the text area, cut on a character boundary at
    /// [`TEXT_CAP`]; gives the length and whether it was cut.
    fn put_text<'t>(&mut self, pieces: impl Iterator<Item = &'t str>) -> Result<(u32, bool), BoardError> {
        let mut len = 0;
        let mut cut = false;
        for (index, piece) in pieces.enumerate() {
            for part in [if index == 0 { "" } else { ", " }, piece] {
                let room = TEXT_CAP - len;
                let mut take = part.len().min(room);
                // At most three steps back: a UTF-8 character is at most four bytes.
                while !part.is_char_boundary(take) {
                    take -= 1;
                }
                if take > 0 {
                    self.put(self.shape.text_at() + len, &part.as_bytes()[..take])?;
                }
                len += take;
                if take < part.len() {
                    cut = true;
                    break;
                }
            }
            if cut {
                break;
            }
        }
        // At most `TEXT_CAP`.
        Ok((capped("a text", len, TEXT_CAP_WORD)?, cut))
    }
}

/// What a board is refused for, naming the field and the rule (test.md, "The case board": a
/// board is read only whole, of its own version, and not torn).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoardError {
    /// The page (or the bytes given) is shorter than the header, the board or the plan needs.
    Short { len: usize, need: usize },
    /// The first four bytes are not `BZSB`: not a case board.
    Foreign { magic: [u8; 4] },
    /// A board of another version: never read as this one.
    Version { found: u32 },
    /// A byte the form keeps zero is not.
    Reserved { at: usize },
    /// The header's length is not the length its counts give.
    Misshapen { said: u32, shape: usize },
    OverCap { what: &'static str, value: u64, cap: u64 },
    /// A given tape (or a case's prefix) longer than the record capacity.
    GivenTooLong { tape: u32, len: u64, cap: u32 },
    /// The tapes' lengths do not add up to the header's count of values.
    GivenSum { said: u32, sum: u64 },
    /// A tag with no meaning in this version.
    Tag { what: &'static str, value: u32 },
    /// The child ended between the first and the last write of a step.
    Torn { case: u32, state: &'static str },
    /// A field out of its bounds.
    Bound { what: &'static str, value: u64, bound: u64 },
    /// The text area is not UTF-8.
    NotText,
    /// A failure with a panic's origin was handed to the writer: on a board a panic ends the
    /// child, and is never returned.
    PanicReturned,
    /// A search with no shrink runs ended "not a function", which only a final run gives.
    NotAFunction,
    /// Coverage bits of another length than the board's.
    Coverage { given: usize, shape: u32 },
    Page(PageError),
}

impl fmt::Display for BoardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the case board (test.md, \"The case board\") is refused: ")?;
        match self {
            BoardError::Short { len, need } => write!(f, "{len} bytes, where it needs {need}: a cut board is never read"),
            BoardError::Foreign { magic: [a, b, c, d] } => {
                write!(f, "it starts with bytes {a} {b} {c} {d}, not `BZSB`: not a case board")
            }
            BoardError::Version { found } => write!(f, "version {found}, where this blaze-sim reads {VERSION} alone"),
            BoardError::Reserved { at } => write!(f, "byte {at}, which the form keeps zero, is not"),
            BoardError::Misshapen { said, shape } => write!(f, "its header says {said} bytes and its counts give {shape}"),
            BoardError::OverCap { what, value, cap } => write!(f, "{what}: {value}, over the cap of {cap}"),
            BoardError::GivenTooLong { tape, len, cap } => write!(f, "tape {tape} has {len} values, over the record capacity of {cap}"),
            BoardError::GivenSum { said, sum } => write!(f, "the header says {said} given values and the tapes' lengths add up to {sum}"),
            BoardError::Tag { what, value } => write!(f, "{what}: tag {value} has no meaning in version {VERSION}"),
            BoardError::Torn { case, state } => write!(f, "torn: the child ended {state} for case {case}"),
            BoardError::Bound { what, value, bound } => write!(f, "{what}: {value}, out of its bound {bound}"),
            BoardError::NotText => f.write_str("its text area is not UTF-8"),
            BoardError::PanicReturned => f.write_str("a failure with a panic's origin was returned: on a board a panic ends the child"),
            BoardError::NotAFunction => f.write_str(
                "a search with no shrink runs said \"not a function\", which only a final run says",
            ),
            BoardError::Coverage { given, shape } => write!(f, "{given} coverage bytes for a board of {shape}"),
            BoardError::Page(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for BoardError {}

/// The `u32` at `at`; the caller checked `at + 4 <= bytes.len()`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

/// The `u64` at `at`; the caller checked `at + 8 <= bytes.len()`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
