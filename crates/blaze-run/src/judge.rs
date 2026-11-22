//! Judge-changed findings (docs/v2/plan/design/test.md, "End state: property, differential and
//! end-to-end tests", D, piece 19): a change cannot quietly rewrite the check that judges it.
//! [`judge`] compares the base's tests with the tree's and files one [`JudgeFinding`] per test
//! and kind whose judge was weakened in the same change that moved code the test's last run
//! entered. Pure: it takes values and pushes findings into a vector its caller hands it; it
//! reads no file, no clock and no process, and hashes nothing.
//!
//! **Which test is which.** A test is followed across the change by its function's item key
//! ([`TestItem`]), never by its position and never by its [`TestId`]: the test id is the key of
//! `TestInputs` (blaze-keyed kind.rs), which holds the attributes and the run spec, so adding
//! `#[ignore]` or lowering a budget gives the same test a new id. The item key is kept by an
//! edit of the attributes, the signature or the body; a rename or a move gives a new one, which
//! is a deletion and an addition (the deletion is filed).
//!
//! **When a finding is filed** (D: "for each test whose judge changed in the same change that
//! moved code its last run entered"). "Code it entered" is every function of the base head's
//! asks for the test except the test's own body ([`JudgeRow::own`]): editing only a test and
//! its judge together moves no code under test. Per kind:
//! - [`Changed::TestDeleted`]: a base row with no tree row. Always filed: the test's own body,
//!   which its last run entered, went with it ([`Reach::Gone`]). It says how its last run ended
//!   ("worse when its last result failed").
//! - [`Changed::IgnoreAdded`], [`Changed::ShouldPanicChanged`], [`Changed::BudgetLowered`]: the
//!   attribute or run spec moved, and either a function the test entered moved
//!   ([`Reach::Entered`], the least such by name) or the base head has no row for the test
//!   ([`Reach::Unknown`]: what it reaches is not known, so the change is judged as reaching it;
//!   the verifier errs toward speaking).
//! - [`Changed::ExpectationChanged`]: a read of the test's last run is among the change's moved
//!   expectations (files under the root that are no unit's source, classified by the driver) and
//!   a function it entered moved. Needs the head's asks: with no head row it is not filed.
//! - [`Changed::JudgeAndCodeMoved`]: the test's own body moved and a function it entered moved.
//!   Reported, never gated (a refactor moves both every day).
//!
//! Two kinds of D are not filed here: `ApproveChanged` needs the differential tests' `approve`
//! bodies (piece 13) and `TapeDropped` the tapes records (piece 4); their kind codes, 5 and 6,
//! are reserved in the record form, and [`open`] refuses them by name until they are built.
//!
//! **Order** (rule 25): the rows are taken sorted by item and each once (refused otherwise,
//! naming the item), and findings come out in item order, then kind code order, so equal input
//! gives equal findings on any machine. Nothing is iterated from a map.
//!
//! **The record** (rule 81: no level, an item-relative range, the rule and its arguments). A
//! finding's record is [`RECORD`] bytes, integers little-endian: `BZTJ`, the version (u32), the
//! kind code (u8), the reach tag (u8: 0 gone, 1 entered, 2 unknown), whether the tree has the
//! test (u8), three detail bytes (a deletion's last run: 0 none, 1 passed, 2 failed, 3 not kept;
//! a should_panic change's two words: 0 no, 1 yes, 2 yes with a text; a lowered budget's 1 when
//! the tree keeps a budget), two zero bytes, the count of further moved expectations (u32), four
//! zero bytes; then 32-byte words at 24: the base test id, the tree test id, the item, the base
//! checkout, the tree checkout, the entered function that moved, the expectation read; then at
//! 248 the base and tree cases (u32 each), seeds (u64 each) and time in nanoseconds (u64 each).
//! A word a kind does not use is zero. [`open`] decodes and encodes again and refuses bytes that
//! are not that one encoding, so a record has exactly one form (rule 21). Its key is
//! H([`JudgeFinding::key_input`]): the kind, the base test id and both checkouts (D), computed by
//! the driver.

#![blaze::sans_io]

use core::fmt;

use blaze_own::AVec;

use crate::head::{Head, RowRef};
use crate::plan::{Asked, Last, ShouldPanic};
use crate::postings::{Ask, FnName, ReadName};
use crate::{CheckoutKey, Hex, Nanos, TestId};

/// The first judge-changed rule number, beside result.rs's `FLAKY_RULE` (0xC0F9): a kind's rule
/// is this plus its code, 0xC0FA to 0xC101 (to verify by a test: no other crate took them).
pub const JUDGE_RULE_FIRST: u16 = 0xC0FA;

/// One finding's record, in bytes.
pub const RECORD: usize = 288;

/// The bytes a finding's key is the hash of: the kind code, the base test id, the base and the
/// tree checkouts.
pub const KEY_INPUT: usize = 1 + 32 + 32 + 32;

const MAGIC: [u8; 4] = *b"BZTJ";
const VERSION: u32 = 1;
const KIND_AT: usize = 8;
const REACH_AT: usize = 9;
const HAS_NOW_AT: usize = 10;
const DETAIL_AT: usize = 11;
const MORE_AT: usize = 16;
const TEST_AT: usize = 24;
const NOW_AT: usize = 56;
const ITEM_AT: usize = 88;
const BASE_AT: usize = 120;
const TREE_AT: usize = 152;
const ENTERED_AT: usize = 184;
const READ_AT: usize = 216;
const CASES_AT: usize = 248;
const SEEDS_AT: usize = 256;
const TIME_AT: usize = 272;

/// A test's identity across a change: its function's item key as the tests record's row holds
/// it. Becomes `blaze_id::ItemKey` when this crate depends on blaze-id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TestItem(pub [u8; 32]);

/// What a tape-search test's run spec asks of its run (`TestInputs::args`; test.md A): the
/// declared cases, the length of its seed range and the time its search may take. A per-case
/// deadline is not a budget: a lower one is stricter, not weaker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Budget {
    pub cases: u32,
    pub seeds: u64,
    pub time: Nanos,
}

impl Budget {
    /// Whether `to` asks less than `self` in any of the three: fewer cases, fewer seeds or less
    /// time. A raise of one does not pay for a cut of another.
    pub const fn lowered_to(self, to: Budget) -> bool {
        to.cases < self.cases || to.seeds < self.seeds || to.time.0 < self.time.0
    }
}

/// One test as one side of the change lists it: its unit's tests record row, joined with what
/// its run spec says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JudgeRow<'r> {
    pub item: TestItem,
    /// The tests record's row on this side: the id (the head's key on the base side), the path,
    /// `#[ignore]` and `#[should_panic]`.
    pub asked: Asked<'r>,
    /// The test's own function as an ask names it: its body is the judge, not the code judged.
    pub own: FnName,
    /// The run spec's budget; `None` for a test with no run spec (an opaque test).
    pub budget: Option<Budget>,
}

/// One change to judge: both sides' tests, the base's head and what moved between them.
#[derive(Clone, Copy, Debug)]
pub struct Change<'r> {
    /// The base's tests, sorted by item and each once.
    pub base: &'r [JudgeRow<'r>],
    /// The tree's tests, sorted by item and each once.
    pub tree: &'r [JudgeRow<'r>],
    /// The base's suite head: each test's last outcome and asks, by the base test id.
    pub head: Head<'r>,
    /// The functions whose code identity moved from base to tree, or which are gone: sorted by
    /// name and each once.
    pub moved: &'r [FnName],
    /// The reads that moved and are expectations (a file under the root that is no unit's
    /// source): sorted by name and each once.
    pub expectations: &'r [ReadName],
    pub base_checkout: CheckoutKey,
    pub tree_checkout: CheckoutKey,
}

/// Which side of a change a row is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Base,
    Tree,
}

/// Why a change was not judged. Each names the row or name and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JudgeError {
    /// A row is not after the one before it on its side: sorted by item, each once (rule 25).
    RowOrder { side: Side, item: TestItem },
    /// A moved function is not after the one before it: sorted by name, each once.
    MovedOrder { name: FnName },
    /// A moved expectation is not after the one before it: sorted by name, each once.
    ExpectationOrder { read: ReadName },
}

impl fmt::Display for JudgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            JudgeError::RowOrder { side, item } => {
                let side = match side {
                    Side::Base => "base",
                    Side::Tree => "tree",
                };
                write!(
                    f,
                    "the {side}'s tests are not sorted by item and each once at item {}: the judge takes them so (rule 25)",
                    Hex(&item.0)
                )
            }
            JudgeError::MovedOrder { name } => write!(
                f,
                "the moved functions are not sorted by name and each once at function {}: the judge takes them so (rule 25)",
                Hex(&name.0)
            ),
            JudgeError::ExpectationOrder { read } => write!(
                f,
                "the moved expectations are not sorted by name and each once at read {}: the judge takes them so (rule 25)",
                Hex(&read.0)
            ),
        }
    }
}

impl core::error::Error for JudgeError {}

/// A judge-changed kind (D), by its code in the record. Codes 5 (`ApproveChanged`) and 6
/// (`TapeDropped`) are reserved.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Kind {
    TestDeleted,
    IgnoreAdded,
    ShouldPanicChanged,
    BudgetLowered,
    ExpectationChanged,
    JudgeAndCodeMoved,
}

impl Kind {
    pub const fn code(self) -> u8 {
        match self {
            Kind::TestDeleted => 0,
            Kind::IgnoreAdded => 1,
            Kind::ShouldPanicChanged => 2,
            Kind::BudgetLowered => 3,
            Kind::ExpectationChanged => 4,
            Kind::JudgeAndCodeMoved => 7,
        }
    }

    pub const fn rule(self) -> u16 {
        JUDGE_RULE_FIRST + self.code() as u16
    }

    /// Whether CI refuses a merge with it unless the pull request carries `Judge-changed: <test
    /// path>: <reason>` (D: every kind but `JudgeAndCodeMoved`).
    pub const fn gated(self) -> bool {
        !matches!(self, Kind::JudgeAndCodeMoved)
    }

    /// As the `Judge-changed:` trailer and the JSON view name it.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::TestDeleted => "TestDeleted",
            Kind::IgnoreAdded => "IgnoreAdded",
            Kind::ShouldPanicChanged => "ShouldPanicChanged",
            Kind::BudgetLowered => "BudgetLowered",
            Kind::ExpectationChanged => "ExpectationChanged",
            Kind::JudgeAndCodeMoved => "JudgeAndCodeMoved",
        }
    }
}

/// How a deleted test's last run ended, as the base head says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WasLast {
    /// The head has no row for it.
    NoRun,
    /// A kept pass, settled or not.
    Passed,
    Failed,
    NotKept,
}

impl WasLast {
    const fn of(last: Option<Last>) -> WasLast {
        match last {
            None => WasLast::NoRun,
            Some(Last::Passed { .. } | Last::Unsettled { .. }) => WasLast::Passed,
            Some(Last::Failed) => WasLast::Failed,
            Some(Last::NotKept(_)) => WasLast::NotKept,
        }
    }

    const fn code(self) -> u8 {
        match self {
            WasLast::NoRun => 0,
            WasLast::Passed => 1,
            WasLast::Failed => 2,
            WasLast::NotKept => 3,
        }
    }
}

/// A `should_panic` as the record words it: the expected text itself is the tests record's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panics {
    No,
    Yes,
    WithText,
}

impl Panics {
    const fn of(should: ShouldPanic<'_>) -> Panics {
        match should {
            ShouldPanic::No => Panics::No,
            ShouldPanic::Yes => Panics::Yes,
            ShouldPanic::WithText(_) => Panics::WithText,
        }
    }

    const fn code(self) -> u8 {
        match self {
            Panics::No => 0,
            Panics::Yes => 1,
            Panics::WithText => 2,
        }
    }

    const fn words(self) -> &'static str {
        match self {
            Panics::No => "no",
            Panics::Yes => "yes",
            Panics::WithText => "yes with a text",
        }
    }
}

/// Why the change is judged as reaching the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reach {
    /// The test is gone, and its own body with it.
    Gone,
    /// A function its last run entered, other than its own body, moved: the least by name.
    Entered(FnName),
    /// The base head has no row for it: what it reaches is not known.
    Unknown,
}

/// What changed in the test's judge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Changed {
    TestDeleted { last: WasLast },
    IgnoreAdded,
    ShouldPanicChanged { from: Panics, to: Panics },
    /// `to` is `None` when the tree's test has no run spec left.
    BudgetLowered { from: Budget, to: Option<Budget> },
    /// The least moved expectation the test read, and how many more it read.
    ExpectationChanged { read: ReadName, more: u32 },
    JudgeAndCodeMoved,
}

impl Changed {
    pub const fn kind(&self) -> Kind {
        match self {
            Changed::TestDeleted { .. } => Kind::TestDeleted,
            Changed::IgnoreAdded => Kind::IgnoreAdded,
            Changed::ShouldPanicChanged { .. } => Kind::ShouldPanicChanged,
            Changed::BudgetLowered { .. } => Kind::BudgetLowered,
            Changed::ExpectationChanged { .. } => Kind::ExpectationChanged,
            Changed::JudgeAndCodeMoved => Kind::JudgeAndCodeMoved,
        }
    }
}

/// A judge-changed finding (rule 81): no level; its range is the test item's start, `0..0`,
/// relative to the item; its rule is its kind's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JudgeFinding {
    /// The test's id at the base: the head's key, and the finding's.
    pub test: TestId,
    /// The test's id in the tree; `None` when it was deleted.
    pub now: Option<TestId>,
    pub item: TestItem,
    pub base: CheckoutKey,
    pub tree: CheckoutKey,
    pub reach: Reach,
    pub changed: Changed,
}

/// Why bytes are not a judge-changed finding's record. Each names the byte or the code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JudgeRecordError {
    /// Not `BZTJ` at this version.
    NotThisRecord,
    /// Not [`RECORD`] bytes long.
    Length { len: usize },
    /// A kind code this version does not know (5 and 6 are reserved: `ApproveChanged`,
    /// `TapeDropped`, not built).
    Kind { code: u8 },
    /// A reach tag that is not 0, 1 or 2.
    Reach { tag: u8 },
    /// A byte at `at` that is not one this kind's form allows.
    Word { at: usize },
    /// The bytes decode, but encode again differently: a word the kind does not use is not zero
    /// (rule 21: one record, one form).
    NotCanonical,
}

impl fmt::Display for JudgeRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            JudgeRecordError::NotThisRecord => {
                write!(f, "not a judge-changed finding record (magic `BZTJ`, version {VERSION})")
            }
            JudgeRecordError::Length { len } => {
                write!(f, "a judge-changed finding record of {len} bytes; the form is {RECORD}")
            }
            JudgeRecordError::Kind { code: code @ (5 | 6) } => write!(
                f,
                "judge-changed kind {code} is reserved (5 `ApproveChanged`, 6 `TapeDropped`) and not built in this version"
            ),
            JudgeRecordError::Kind { code } => write!(f, "judge-changed kind {code} is unknown to this version"),
            JudgeRecordError::Reach { tag } => write!(f, "judge-changed reach tag {tag} is unknown (0 gone, 1 entered, 2 unknown)"),
            JudgeRecordError::Word { at } => write!(f, "judge-changed finding record: byte {at} is not one its kind allows"),
            JudgeRecordError::NotCanonical => {
                write!(f, "judge-changed finding record: a word its kind does not use is not zero (rule 21)")
            }
        }
    }
}

impl core::error::Error for JudgeRecordError {}

impl JudgeFinding {
    pub const fn kind(&self) -> Kind {
        self.changed.kind()
    }

    pub const fn rule(&self) -> u16 {
        self.kind().rule()
    }

    /// The range, relative to the test item's first byte: its start, as `Flaky` marks it.
    pub const fn range(&self) -> (u32, u32) {
        (0, 0)
    }

    /// The finding's words, exactly (ids as their first eight bytes in hex):
    /// - `test 0b00000000000000 was deleted; its last run failed` (`passed`, `was not kept`;
    ///   `; it has no last run` when the head has none);
    /// - `test .. gained #[ignore] in the change that moved function .. it entered`;
    /// - `test ..'s should_panic went from no to yes with a text in the change ..`;
    /// - `test ..'s budget fell from 256 cases, 256 seeds, 30.000 s to 16 cases, 256 seeds,
    ///   30.000 s in the change ..` (`to no run spec`);
    /// - `test .. read expectation .., which changed (and 2 more) in the change ..`;
    /// - `test ..'s own body moved in the change ..`;
    ///
    /// where "in the change .." is `in the change that moved function 0700000000000000 it
    /// entered`, or `in a change the head cannot place: it has no last run of the test`.
    pub fn message(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        let test = Hex(&self.test.0);
        match self.changed {
            Changed::TestDeleted { last } => {
                write!(out, "test {test} was deleted; ")?;
                return out.write_str(match last {
                    WasLast::NoRun => "it has no last run",
                    WasLast::Passed => "its last run passed",
                    WasLast::Failed => "its last run failed",
                    WasLast::NotKept => "its last run was not kept",
                });
            }
            Changed::IgnoreAdded => write!(out, "test {test} gained #[ignore]")?,
            Changed::ShouldPanicChanged { from, to } => {
                write!(out, "test {test}'s should_panic went from {} to {}", from.words(), to.words())?
            }
            Changed::BudgetLowered { from, to } => {
                write!(out, "test {test}'s budget fell from ")?;
                budget(out, from)?;
                out.write_str(" to ")?;
                match to {
                    Some(to) => budget(out, to)?,
                    None => out.write_str("no run spec")?,
                }
            }
            Changed::ExpectationChanged { read, more } => {
                write!(out, "test {test} read expectation {}, which changed", Hex(&read.0))?;
                if more > 0 {
                    write!(out, " (and {more} more)")?;
                }
            }
            Changed::JudgeAndCodeMoved => write!(out, "test {test}'s own body moved")?,
        }
        match self.reach {
            Reach::Entered(function) => write!(out, " in the change that moved function {} it entered", Hex(&function.0)),
            Reach::Unknown => out.write_str(" in a change the head cannot place: it has no last run of the test"),
            // Only a deletion is gone, and it returned above.
            Reach::Gone => Ok(()),
        }
    }

    /// The finding's record: one form per finding (see the module's documentation).
    pub fn encode(&self) -> [u8; RECORD] {
        let mut out = [0u8; RECORD];
        out[..4].copy_from_slice(&MAGIC);
        out[4..8].copy_from_slice(&VERSION.to_le_bytes());
        out[KIND_AT] = self.kind().code();
        let (reach, entered) = match self.reach {
            Reach::Gone => (0, [0; 32]),
            Reach::Entered(function) => (1, function.0),
            Reach::Unknown => (2, [0; 32]),
        };
        out[REACH_AT] = reach;
        out[ENTERED_AT..ENTERED_AT + 32].copy_from_slice(&entered);
        if let Some(now) = self.now {
            out[HAS_NOW_AT] = 1;
            out[NOW_AT..NOW_AT + 32].copy_from_slice(&now.0);
        }
        out[TEST_AT..TEST_AT + 32].copy_from_slice(&self.test.0);
        out[ITEM_AT..ITEM_AT + 32].copy_from_slice(&self.item.0);
        out[BASE_AT..BASE_AT + 32].copy_from_slice(&self.base.0);
        out[TREE_AT..TREE_AT + 32].copy_from_slice(&self.tree.0);
        match self.changed {
            Changed::TestDeleted { last } => out[DETAIL_AT] = last.code(),
            Changed::IgnoreAdded | Changed::JudgeAndCodeMoved => {}
            Changed::ShouldPanicChanged { from, to } => {
                out[DETAIL_AT] = from.code();
                out[DETAIL_AT + 1] = to.code();
            }
            Changed::BudgetLowered { from, to } => {
                put_budget(&mut out, 0, from);
                if let Some(to) = to {
                    out[DETAIL_AT] = 1;
                    put_budget(&mut out, 1, to);
                }
            }
            Changed::ExpectationChanged { read, more } => {
                out[READ_AT..READ_AT + 32].copy_from_slice(&read.0);
                out[MORE_AT..MORE_AT + 4].copy_from_slice(&more.to_le_bytes());
            }
        }
        out
    }

    /// The bytes the finding's key hashes: the kind code, the base test id, the base and the
    /// tree checkouts (D).
    pub fn key_input(&self) -> [u8; KEY_INPUT] {
        let mut out = [0u8; KEY_INPUT];
        out[0] = self.kind().code();
        out[1..33].copy_from_slice(&self.test.0);
        out[33..65].copy_from_slice(&self.base.0);
        out[65..97].copy_from_slice(&self.tree.0);
        out
    }
}

/// `256 cases, 256 seeds, 30.000 s`.
fn budget(out: &mut dyn fmt::Write, budget: Budget) -> fmt::Result {
    let millis = budget.time.0 / 1_000_000;
    write!(out, "{} cases, {} seeds, {}.{:03} s", budget.cases, budget.seeds, millis / 1_000, millis % 1_000)
}

/// Writes a budget into side 0 (base) or 1 (tree) of the record's budget words.
fn put_budget(out: &mut [u8; RECORD], side: usize, budget: Budget) {
    let cases = CASES_AT + side * 4;
    let seeds = SEEDS_AT + side * 8;
    let time = TIME_AT + side * 8;
    out[cases..cases + 4].copy_from_slice(&budget.cases.to_le_bytes());
    out[seeds..seeds + 8].copy_from_slice(&budget.seeds.to_le_bytes());
    out[time..time + 8].copy_from_slice(&budget.time.0.to_le_bytes());
}

fn get_budget(bytes: &[u8], side: usize) -> Budget {
    let cases = CASES_AT + side * 4;
    let seeds = SEEDS_AT + side * 8;
    let time = TIME_AT + side * 8;
    Budget { cases: le32(bytes, cases), seeds: le64(bytes, seeds), time: Nanos(le64(bytes, time)) }
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn word32(bytes: &[u8], at: usize) -> [u8; 32] {
    let mut word = [0u8; 32];
    word.copy_from_slice(&bytes[at..at + 32]);
    word
}

fn panics(byte: u8, at: usize) -> Result<Panics, JudgeRecordError> {
    match byte {
        0 => Ok(Panics::No),
        1 => Ok(Panics::Yes),
        2 => Ok(Panics::WithText),
        _ => Err(JudgeRecordError::Word { at }),
    }
}

/// Opens one finding's record: the length, magic, version, kind code, reach tag and detail
/// bytes checked, then the value encoded again and compared, so stray bytes are refused.
pub fn open(bytes: &[u8]) -> Result<JudgeFinding, JudgeRecordError> {
    if bytes.len() < 8 || bytes[..4] != MAGIC || le32(bytes, 4) != VERSION {
        return Err(JudgeRecordError::NotThisRecord);
    }
    if bytes.len() != RECORD {
        return Err(JudgeRecordError::Length { len: bytes.len() });
    }
    let detail = bytes[DETAIL_AT];
    let changed = match bytes[KIND_AT] {
        0 => Changed::TestDeleted {
            last: match detail {
                0 => WasLast::NoRun,
                1 => WasLast::Passed,
                2 => WasLast::Failed,
                3 => WasLast::NotKept,
                _ => return Err(JudgeRecordError::Word { at: DETAIL_AT }),
            },
        },
        1 => Changed::IgnoreAdded,
        2 => Changed::ShouldPanicChanged {
            from: panics(detail, DETAIL_AT)?,
            to: panics(bytes[DETAIL_AT + 1], DETAIL_AT + 1)?,
        },
        3 => Changed::BudgetLowered {
            from: get_budget(bytes, 0),
            to: match detail {
                0 => None,
                1 => Some(get_budget(bytes, 1)),
                _ => return Err(JudgeRecordError::Word { at: DETAIL_AT }),
            },
        },
        4 => Changed::ExpectationChanged { read: ReadName(word32(bytes, READ_AT)), more: le32(bytes, MORE_AT) },
        7 => Changed::JudgeAndCodeMoved,
        code => return Err(JudgeRecordError::Kind { code }),
    };
    let reach = match bytes[REACH_AT] {
        0 => Reach::Gone,
        1 => Reach::Entered(FnName(word32(bytes, ENTERED_AT))),
        2 => Reach::Unknown,
        tag => return Err(JudgeRecordError::Reach { tag }),
    };
    let now = match bytes[HAS_NOW_AT] {
        0 => None,
        1 => Some(TestId(word32(bytes, NOW_AT))),
        _ => return Err(JudgeRecordError::Word { at: HAS_NOW_AT }),
    };
    // A deletion, and only a deletion, has no tree test and is gone.
    let deleted = matches!(changed, Changed::TestDeleted { .. });
    if deleted != now.is_none() {
        return Err(JudgeRecordError::Word { at: HAS_NOW_AT });
    }
    if deleted != matches!(reach, Reach::Gone) {
        return Err(JudgeRecordError::Word { at: REACH_AT });
    }
    let finding = JudgeFinding {
        test: TestId(word32(bytes, TEST_AT)),
        now,
        item: TestItem(word32(bytes, ITEM_AT)),
        base: CheckoutKey(word32(bytes, BASE_AT)),
        tree: CheckoutKey(word32(bytes, TREE_AT)),
        reach,
        changed,
    };
    if finding.encode()[..] != *bytes {
        return Err(JudgeRecordError::NotCanonical);
    }
    Ok(finding)
}

/// Judges one change: pushes to `out`, in item order then kind code order, one finding per test
/// and kind whose judge the change weakened (see the module's documentation for when).
/// Allocates nothing itself; `out` grows by at most five findings per base test.
pub fn judge(change: &Change<'_>, out: &mut AVec<'_, JudgeFinding>) -> Result<(), JudgeError> {
    rows_in_order(change.base, Side::Base)?;
    rows_in_order(change.tree, Side::Tree)?;
    for pair in change.moved.windows(2) {
        if pair[0] >= pair[1] {
            return Err(JudgeError::MovedOrder { name: pair[1] });
        }
    }
    for pair in change.expectations.windows(2) {
        if pair[0] >= pair[1] {
            return Err(JudgeError::ExpectationOrder { read: pair[1] });
        }
    }
    // One merge walk over both sides by item: a tree row before the base row is an added test,
    // which has no judge to weaken.
    let mut at = 0;
    for base in change.base {
        while at < change.tree.len() && change.tree[at].item < base.item {
            at += 1;
        }
        let last = change.head.row(base.asked.test);
        match change.tree.get(at).filter(|tree| tree.item == base.item) {
            None => out.push(JudgeFinding {
                test: base.asked.test,
                now: None,
                item: base.item,
                base: change.base_checkout,
                tree: change.tree_checkout,
                reach: Reach::Gone,
                changed: Changed::TestDeleted { last: WasLast::of(last.map(|row| row.kept.last)) },
            }),
            Some(tree) => {
                at += 1;
                both_sides(change, base, tree, last, out);
            }
        }
    }
    Ok(())
}

fn rows_in_order(rows: &[JudgeRow<'_>], side: Side) -> Result<(), JudgeError> {
    for pair in rows.windows(2) {
        if pair[0].item >= pair[1].item {
            return Err(JudgeError::RowOrder { side, item: pair[1].item });
        }
    }
    Ok(())
}

/// A test on both sides: its findings in kind code order.
fn both_sides(
    change: &Change<'_>,
    base: &JudgeRow<'_>,
    tree: &JudgeRow<'_>,
    last: Option<RowRef>,
    out: &mut AVec<'_, JudgeFinding>,
) {
    let reach = match last {
        None => Reach::Unknown,
        Some(row) => match entered_moved(change, row, base.own) {
            Some(function) => Reach::Entered(function),
            // Nothing it entered moved: the change does not reach it, so nothing is judged.
            None => return,
        },
    };
    let finding = |changed: Changed| JudgeFinding {
        test: base.asked.test,
        now: Some(tree.asked.test),
        item: base.item,
        base: change.base_checkout,
        tree: change.tree_checkout,
        reach,
        changed,
    };
    if base.asked.ignore.is_none() && tree.asked.ignore.is_some() {
        out.push(finding(Changed::IgnoreAdded));
    }
    if base.asked.should_panic != tree.asked.should_panic {
        out.push(finding(Changed::ShouldPanicChanged {
            from: Panics::of(base.asked.should_panic),
            to: Panics::of(tree.asked.should_panic),
        }));
    }
    if let Some(from) = base.budget {
        let lowered = match tree.budget {
            Some(to) => from.lowered_to(to),
            None => true,
        };
        if lowered {
            out.push(finding(Changed::BudgetLowered { from, to: tree.budget }));
        }
    }
    // The last two read the head's asks, and so need its row.
    let Some(row) = last else { return };
    let mut read = None;
    let mut more = 0u32;
    for ask in change.head.asks(row) {
        if let Ask::Read(name) = ask
            && change.expectations.binary_search(&name).is_ok()
        {
            match read {
                None => read = Some(name),
                Some(_) => more = more.saturating_add(1),
            }
        }
    }
    if let Some(read) = read {
        out.push(finding(Changed::ExpectationChanged { read, more }));
    }
    if change.moved.binary_search(&base.own).is_ok() {
        out.push(finding(Changed::JudgeAndCodeMoved));
    }
}

/// The least function by name that `row`'s last run entered, other than the test's own body,
/// whose code moved. The asks are in [`Ask`] order: functions first, by name.
fn entered_moved(change: &Change<'_>, row: RowRef, own: FnName) -> Option<FnName> {
    for ask in change.head.asks(row) {
        match ask {
            Ask::Fn(function) if function != own && change.moved.binary_search(&function).is_ok() => {
                return Some(function);
            }
            Ask::Fn(_) => {}
            // Reads come after every function.
            Ask::Read(_) => return None,
        }
    }
    None
}
