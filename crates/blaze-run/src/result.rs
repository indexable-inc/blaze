//! The value of a test's result record (docs/v2/plan/design/test.md, "End state", piece 4): a
//! [`Passed`] or a [`Failed`] run, filed at the run's [`Address`] (H(test id, asks, answers),
//! computed by the driver). An address has two slots, one for a pass and one for a failure
//! ([`Slot`]); each is written once and never over. A failure is filed BESIDE a pass at its
//! address, never over it, and both at one address is a `Flaky` [`Finding`] naming the two runs:
//! one address means the same code entered and the same reads answered, so the run rests on
//! something its asks do not name (a schedule, a time). Nothing is retried, here or anywhere
//! (rule 3): the finding is the answer.
//!
//! The form (integers little-endian). A 16-byte header: `BZTR`, the version, the kind (0 a pass,
//! 1 a failure), seven zero bytes. Then the run's checkout (32 bytes) and its micros (u64).
//!
//! - A pass, 64 bytes in all: at 56 whether it was kept (0 kept, else 1 + the [`NotKept`]
//!   reason's number), seven zero bytes.
//! - A failure, 160 bytes and then its text and its output's tail: at 56 how it ended (0
//!   panicked, 1 did not panic, 2 panicked without the expected text, 3 a signal, 4 timed out,
//!   5 refused by the sandbox, 6 exited with no whole report: its exit code where a signal goes,
//!   the torn report's byte count where the nanoseconds go); 57 whether a place is given; 58 what the sandbox refused; 59
//!   zero; 60 the signal (i32); 64 nanoseconds after its start it was killed (u64); 72 the
//!   place's item (32 bytes) and 104 its item-relative offset (u32); 108 the text's length (u32:
//!   the panic message, or what the sandbox refused); 112 the output's digest (32 bytes); 144 the
//!   output's whole length (u64); 152 the length of its tail kept here (u32); 156 four zero bytes.
//!   A field its ending does not use is zero.
//!
//! [`open`] checks every length, tag and unused field once and reads in place: the text and the
//! output borrow the record's bytes (rule 74). Equal values encode to equal bytes (rule 25).

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::plan::{Address, Failure, NotKept};
use crate::postings::FnName;
use crate::{CheckoutKey, Hex, Nanos, TestId};

/// The most of a run's captured output a failure keeps: its LAST bytes, the ones nearest the
/// failure. The digest covers all of it.
pub const OUTPUT_CAP: usize = 64 * 1024;

/// The most bytes of a panic message or a refused name a failure keeps ([`capped`] cuts one).
pub const TEXT_CAP: usize = 16 * 1024;

/// The `Flaky` finding's rule number, beside blaze-drive's (`OVER_CAP_RULE` 0xC0F6,
/// `MODULE_FILE_MISSING_RULE` 0xC0F7, `PATH_FILE_MISSING_RULE` 0xC0F8).
pub const FLAKY_RULE: u16 = 0xC0F9;

const MAGIC: [u8; 4] = *b"BZTR";
const VERSION: u32 = 1;
const HEADER: usize = 16;
const PASS: u8 = 0;
const FAIL: u8 = 1;
const PASS_LEN: usize = 64;
const FAIL_FIXED: usize = 160;

/// A run that passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Passed {
    /// The checkout it ran on: how a finding names the run.
    pub run: CheckoutKey,
    pub micros: u64,
    /// Whether the pass may be kept, and when not, why, by name. Only a kept pass is filed
    /// ([`file`] refuses the other); a pass not kept is still reported.
    pub kept: Result<(), NotKept>,
}

/// An item-relative place (rule 71): the function instance and the byte offset in its item's
/// text. `file:line:col` exists only when printed (the report takes it as text).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Place {
    pub item: FnName,
    pub offset: u32,
}

/// What the sandbox refused, by kind; the refused thing itself is [`Ended::Refused`]'s `what`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// A write outside the test's temporary folder; `what` is the path.
    Write,
    /// A connection to a peer outside its loopback; `what` is the address.
    Connect,
    /// Starting a program; `what` is the program.
    Spawn,
    /// A system call the profile does not allow; `what` is its name.
    Call,
}

/// How a failed run ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended<'r> {
    /// It panicked (the message from descriptor 3, never from stderr).
    Panicked { message: &'r str, at: Option<Place> },
    /// `should_panic`, and it returned.
    DidNotPanic,
    /// `should_panic(expected = ..)`, and this message does not hold the text.
    PanicTextMissing { message: &'r str, at: Option<Place> },
    /// The process ended by this signal (a crash, a stack overflow, an abort with no panic).
    Signal { number: i32 },
    /// Killed past its deadline, this long after its start.
    TimedOut { after: Nanos },
    /// The sandbox refused something and the run was stopped.
    Refused { refusal: Refusal, what: &'r str },
    /// The process exited by itself with this code and no whole report on descriptor 3;
    /// `torn` bytes of a report came that did not open (0: none came).
    NoReport { code: i32, torn: u64 },
}

impl Ended<'_> {
    /// The plan's verdict for this ending.
    pub const fn why(&self) -> Failure {
        match *self {
            Ended::Panicked { .. } => Failure::Panicked,
            Ended::DidNotPanic => Failure::DidNotPanic,
            Ended::PanicTextMissing { .. } => Failure::PanicTextMissing,
            Ended::Signal { number } => Failure::Signal { number },
            Ended::TimedOut { after } => Failure::TimedOut { after },
            Ended::Refused { .. } => Failure::Refused,
            Ended::NoReport { code, .. } => Failure::NoReport { code },
        }
    }
}

/// A run's captured output: the digest and length of all of it, and its last bytes, at most
/// [`OUTPUT_CAP`] (exactly `min(total, OUTPUT_CAP)`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Captured<'r> {
    /// The digest of all of it, computed by the driver (this crate hashes nothing).
    pub digest: [u8; 32],
    pub total: u64,
    pub tail: &'r [u8],
}

impl<'r> Captured<'r> {
    /// No output.
    pub const NONE: Captured<'static> = Captured { digest: [0; 32], total: 0, tail: &[] };

    /// All of a run's output, kept as its last [`OUTPUT_CAP`] bytes.
    pub fn of(digest: [u8; 32], all: &'r [u8]) -> Captured<'r> {
        let from = all.len().saturating_sub(OUTPUT_CAP);
        Captured { digest, total: all.len() as u64, tail: &all[from..] }
    }

    /// How many of the first bytes are not kept here.
    pub const fn cut(&self) -> u64 {
        self.total.saturating_sub(self.tail.len() as u64)
    }
}

/// A run that failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Failed<'r> {
    /// The checkout it ran on: how a finding names the run.
    pub run: CheckoutKey,
    pub micros: u64,
    pub ended: Ended<'r>,
    pub output: Captured<'r>,
}

/// A result record's value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict<'r> {
    Pass(Passed),
    Fail(Failed<'r>),
}

/// Which capped field was too long.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Capped {
    /// The panic message or the refused name ([`TEXT_CAP`]).
    Text,
    /// The output's kept tail ([`OUTPUT_CAP`]).
    Output,
}

/// Why a result record was not written or not read: each names the byte and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultError {
    /// Fewer bytes than its kind's fixed part.
    Short { len: usize, least: usize },
    /// The bytes do not start with this record's magic and version.
    NotThisRecord,
    /// The byte at `at` names no kind, ending, place flag, refusal or reason this form has.
    Tag { at: usize, byte: u8 },
    /// The bytes from `at` are padding, or a field its ending does not use, and are not zero.
    NotZero { at: usize },
    /// The length is not the fixed part plus the text and the tail its fields count.
    Length { want: usize, len: usize },
    /// A capped field is over its cap.
    TooLong { field: Capped, len: usize, cap: usize },
    /// The kept tail is not the last `min(total, OUTPUT_CAP)` bytes' length.
    Output { tail: usize, total: u64 },
    /// The text at `at` is not UTF-8.
    NotUtf8 { at: usize },
}

impl fmt::Display for ResultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ResultError::Short { len, least } => write!(f, "test result record of {len} bytes; its form needs at least {least}"),
            ResultError::NotThisRecord => write!(f, "not a test result record (magic `BZTR`, version {VERSION})"),
            ResultError::Tag { at, byte } => write!(f, "test result record byte {at} is {byte}, which its form does not name"),
            ResultError::NotZero { at } => {
                write!(f, "test result record bytes from {at} are padding or unused and must be zero (rule 25)")
            }
            ResultError::Length { want, len } => {
                write!(f, "test result record of {len} bytes, but its fields count {want}")
            }
            ResultError::TooLong { field, len, cap } => {
                let what = match field {
                    Capped::Text => "text",
                    Capped::Output => "output tail",
                };
                write!(f, "a test result's {what} of {len} bytes is over its cap of {cap} (rule 18)")
            }
            ResultError::Output { tail, total } => write!(
                f,
                "a test result keeps {tail} bytes of {total} of output; it keeps exactly the last min(total, {OUTPUT_CAP})"
            ),
            ResultError::NotUtf8 { at } => write!(f, "test result record text at byte {at} is not UTF-8"),
        }
    }
}

impl core::error::Error for ResultError {}

/// `text`'s first [`TEXT_CAP`] bytes, cut at a character's start: what a driver files for a
/// longer panic message or name.
pub fn capped(text: &str) -> &str {
    let mut end = text.len().min(TEXT_CAP);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(crate) const fn not_kept_number(why: NotKept) -> u8 {
    match why {
        NotKept::Clock => 1,
        NotKept::Network => 2,
        NotKept::WroteOutside => 3,
        NotKept::Foreign => 4,
        NotKept::Unsandboxed => 5,
        NotKept::Descriptor => 6,
        NotKept::Threads => 7,
        NotKept::Untraced => 8,
    }
}

pub(crate) const fn not_kept_of(byte: u8) -> Option<Result<(), NotKept>> {
    match byte {
        0 => Some(Ok(())),
        1 => Some(Err(NotKept::Clock)),
        2 => Some(Err(NotKept::Network)),
        3 => Some(Err(NotKept::WroteOutside)),
        4 => Some(Err(NotKept::Foreign)),
        5 => Some(Err(NotKept::Unsandboxed)),
        6 => Some(Err(NotKept::Descriptor)),
        7 => Some(Err(NotKept::Threads)),
        8 => Some(Err(NotKept::Untraced)),
        _ => None,
    }
}

pub(crate) const fn refusal_number(refusal: Refusal) -> u8 {
    match refusal {
        Refusal::Write => 0,
        Refusal::Connect => 1,
        Refusal::Spawn => 2,
        Refusal::Call => 3,
    }
}

pub(crate) const fn refusal_of(byte: u8) -> Option<Refusal> {
    match byte {
        0 => Some(Refusal::Write),
        1 => Some(Refusal::Connect),
        2 => Some(Refusal::Spawn),
        3 => Some(Refusal::Call),
        _ => None,
    }
}

/// A failure's ending as the form's fixed fields.
struct Wire<'r> {
    tag: u8,
    place: Option<Place>,
    refusal: u8,
    signal: i32,
    after: u64,
    text: &'r str,
}

fn wire<'r>(ended: &Ended<'r>) -> Wire<'r> {
    let none = Wire { tag: 0, place: None, refusal: 0, signal: 0, after: 0, text: "" };
    match *ended {
        Ended::Panicked { message, at } => Wire { tag: 0, place: at, text: message, ..none },
        Ended::DidNotPanic => Wire { tag: 1, ..none },
        Ended::PanicTextMissing { message, at } => Wire { tag: 2, place: at, text: message, ..none },
        Ended::Signal { number } => Wire { tag: 3, signal: number, ..none },
        Ended::TimedOut { after } => Wire { tag: 4, after: after.0, ..none },
        Ended::Refused { refusal, what } => Wire { tag: 5, refusal: refusal_number(refusal), text: what, ..none },
        Ended::NoReport { code, torn } => Wire { tag: 6, signal: code, after: torn, ..none },
    }
}

fn header(out: &mut AVec<'_, u8>, kind: u8, run: &CheckoutKey, micros: u64) {
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&[0; 7]);
    out.extend_from_slice(&run.0);
    out.extend_from_slice(&micros.to_le_bytes());
}

/// The checks a kept tail must pass, at write and at read alike.
fn check_output(tail: usize, total: u64) -> Result<(), ResultError> {
    if tail > OUTPUT_CAP {
        return Err(ResultError::TooLong { field: Capped::Output, len: tail, cap: OUTPUT_CAP });
    }
    if tail as u64 != total.min(OUTPUT_CAP as u64) {
        return Err(ResultError::Output { tail, total });
    }
    Ok(())
}

impl<'r> Verdict<'r> {
    /// The record's bytes, in `alloc`. Refuses a text over [`TEXT_CAP`] (see [`capped`]) and a
    /// tail that is not the last `min(total, OUTPUT_CAP)` bytes (see [`Captured::of`]).
    pub fn encode<'b>(&self, alloc: Alloc<'b>) -> Result<AVec<'b, u8>, ResultError> {
        match self {
            Verdict::Pass(pass) => {
                let mut out = Vec::with_capacity_in(PASS_LEN, alloc);
                header(&mut out, PASS, &pass.run, pass.micros);
                out.push(match pass.kept {
                    Ok(()) => 0,
                    Err(why) => not_kept_number(why),
                });
                out.extend_from_slice(&[0; 7]);
                Ok(out)
            }
            Verdict::Fail(fail) => {
                let w = wire(&fail.ended);
                if w.text.len() > TEXT_CAP {
                    return Err(ResultError::TooLong { field: Capped::Text, len: w.text.len(), cap: TEXT_CAP });
                }
                check_output(fail.output.tail.len(), fail.output.total)?;
                let mut out = Vec::with_capacity_in(FAIL_FIXED + w.text.len() + fail.output.tail.len(), alloc);
                header(&mut out, FAIL, &fail.run, fail.micros);
                out.push(w.tag);
                out.push(u8::from(w.place.is_some()));
                out.push(w.refusal);
                out.push(0);
                out.extend_from_slice(&w.signal.to_le_bytes());
                out.extend_from_slice(&w.after.to_le_bytes());
                let (item, offset) = match w.place {
                    Some(place) => (place.item.0, place.offset),
                    None => ([0; 32], 0),
                };
                out.extend_from_slice(&item);
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&(w.text.len() as u32).to_le_bytes());
                out.extend_from_slice(&fail.output.digest);
                out.extend_from_slice(&fail.output.total.to_le_bytes());
                out.extend_from_slice(&(fail.output.tail.len() as u32).to_le_bytes());
                out.extend_from_slice(&[0; 4]);
                out.extend_from_slice(w.text.as_bytes());
                out.extend_from_slice(fail.output.tail);
                Ok(out)
            }
        }
    }
}

/// A result record, checked once and read in place. Any prefix of a record is refused as
/// [`ResultError::Short`] or [`ResultError::Length`].
pub fn open(bytes: &[u8]) -> Result<Verdict<'_>, ResultError> {
    let len = bytes.len();
    if len < HEADER {
        return Err(ResultError::Short { len, least: HEADER });
    }
    if bytes[..4] != MAGIC || le32(bytes, 4) != VERSION {
        return Err(ResultError::NotThisRecord);
    }
    zero(bytes, 9, HEADER)?;
    match bytes[8] {
        PASS => open_pass(bytes),
        FAIL => open_fail(bytes),
        byte => Err(ResultError::Tag { at: 8, byte }),
    }
}

fn open_pass(bytes: &[u8]) -> Result<Verdict<'_>, ResultError> {
    let len = bytes.len();
    if len < PASS_LEN {
        return Err(ResultError::Short { len, least: PASS_LEN });
    }
    if len != PASS_LEN {
        return Err(ResultError::Length { want: PASS_LEN, len });
    }
    zero(bytes, 57, PASS_LEN)?;
    let Some(kept) = not_kept_of(bytes[56]) else { return Err(ResultError::Tag { at: 56, byte: bytes[56] }) };
    Ok(Verdict::Pass(Passed { run: CheckoutKey(key(bytes, 16)), micros: le64(bytes, 48), kept }))
}

fn open_fail(bytes: &[u8]) -> Result<Verdict<'_>, ResultError> {
    let len = bytes.len();
    if len < FAIL_FIXED {
        return Err(ResultError::Short { len, least: FAIL_FIXED });
    }
    let text_len = le32(bytes, 108) as usize;
    let tail_len = le32(bytes, 152) as usize;
    let total = le64(bytes, 144);
    if text_len > TEXT_CAP {
        return Err(ResultError::TooLong { field: Capped::Text, len: text_len, cap: TEXT_CAP });
    }
    if tail_len > OUTPUT_CAP {
        return Err(ResultError::TooLong { field: Capped::Output, len: tail_len, cap: OUTPUT_CAP });
    }
    let want = FAIL_FIXED + text_len + tail_len;
    if len != want {
        return Err(ResultError::Length { want, len });
    }
    check_output(tail_len, total)?;
    zero(bytes, 59, 60)?;
    zero(bytes, 156, FAIL_FIXED)?;

    let tag = bytes[56];
    if tag > 6 {
        return Err(ResultError::Tag { at: 56, byte: tag });
    }
    let (uses_place, uses_text) = (matches!(tag, 0 | 2), matches!(tag, 0 | 2 | 5));
    let place = match (bytes[57], uses_place) {
        (0, _) => {
            zero(bytes, 72, 108)?;
            None
        }
        (1, true) => Some(Place { item: FnName(key(bytes, 72)), offset: le32(bytes, 104) }),
        (1, false) => return Err(ResultError::NotZero { at: 57 }),
        (byte, _) => return Err(ResultError::Tag { at: 57, byte }),
    };
    let refusal = match tag {
        5 => match refusal_of(bytes[58]) {
            Some(refusal) => refusal,
            None => return Err(ResultError::Tag { at: 58, byte: bytes[58] }),
        },
        _ => {
            zero(bytes, 58, 59)?;
            Refusal::Write
        }
    };
    // Tag 6 (no report) keeps its exit code where a signal's number goes and its torn byte
    // count where a deadline's nanoseconds go.
    if tag != 3 && tag != 6 {
        zero(bytes, 60, 64)?;
    }
    if tag != 4 && tag != 6 {
        zero(bytes, 64, 72)?;
    }
    if !uses_text && text_len != 0 {
        return Err(ResultError::NotZero { at: 108 });
    }
    let text = core::str::from_utf8(&bytes[FAIL_FIXED..FAIL_FIXED + text_len])
        .map_err(|_| ResultError::NotUtf8 { at: FAIL_FIXED })?;
    let ended = match tag {
        0 => Ended::Panicked { message: text, at: place },
        1 => Ended::DidNotPanic,
        2 => Ended::PanicTextMissing { message: text, at: place },
        3 => Ended::Signal { number: le32(bytes, 60) as i32 },
        4 => Ended::TimedOut { after: Nanos(le64(bytes, 64)) },
        5 => Ended::Refused { refusal, what: text },
        _ => Ended::NoReport { code: le32(bytes, 60) as i32, torn: le64(bytes, 64) },
    };
    let output = Captured { digest: key(bytes, 112), total, tail: &bytes[FAIL_FIXED + text_len..] };
    Ok(Verdict::Fail(Failed { run: CheckoutKey(key(bytes, 16)), micros: le64(bytes, 48), ended, output }))
}

fn zero(bytes: &[u8], from: usize, to: usize) -> Result<(), ResultError> {
    match bytes[from..to].iter().position(|byte| *byte != 0) {
        None => Ok(()),
        Some(at) => Err(ResultError::NotZero { at: from + at }),
    }
}

fn key(bytes: &[u8], at: usize) -> [u8; 32] {
    let mut out = [0; 32];
    out.copy_from_slice(&bytes[at..at + 32]);
    out
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut out = [0; 8];
    out.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(out)
}

/// An address's two slots. Each is written once; neither is written over the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Pass,
    Failure,
}

impl Verdict<'_> {
    /// The slot this value is filed in.
    pub const fn slot(&self) -> Slot {
        match self {
            Verdict::Pass(_) => Slot::Pass,
            Verdict::Fail(_) => Slot::Failure,
        }
    }
}

/// What one address holds now: each slot's record, opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AtAddress<'r> {
    pub pass: Option<Passed>,
    pub failure: Option<Failed<'r>>,
}

impl AtAddress<'_> {
    pub const EMPTY: AtAddress<'static> = AtAddress { pass: None, failure: None };
}

/// What the driver does with a new value at an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filing {
    /// Write the value's record into this empty slot.
    Write(Slot),
    /// The slot holds a record already: it is written once, so nothing is written (a second
    /// failure at one address; a pass the plan found there).
    Held(Slot),
}

/// [`file`]'s answer: what to write, and the `Flaky` finding when this value made the address
/// hold both a pass and a failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Filed {
    pub filing: Filing,
    pub flaky: Option<Finding>,
}

/// A value [`file`] refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileError {
    /// A pass that may not be kept is never filed: its asks do not cover what it rests on.
    NotKept { test: TestId, why: NotKept },
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FileError::NotKept { test, why } => {
                write!(f, "test {}'s pass is not kept ({}), so it is not filed", Hex(&test.0), NotKeptWords(why))
            }
        }
    }
}

impl core::error::Error for FileError {}

/// Files `new` at `address`, which holds `at` now. A failure goes into the failure slot BESIDE
/// a pass, never over it; a slot that holds a record is not written again. When this value is
/// the one that makes the address hold both, the answer carries the one `Flaky` finding; a
/// second failure (or a second pass) adds none.
pub fn file(test: TestId, address: Address, at: &AtAddress<'_>, new: &Verdict<'_>) -> Result<Filed, FileError> {
    match new {
        Verdict::Pass(pass) => {
            if let Err(why) = pass.kept {
                return Err(FileError::NotKept { test, why });
            }
            if at.pass.is_some() {
                return Ok(Filed { filing: Filing::Held(Slot::Pass), flaky: None });
            }
            let flaky = at.failure.as_ref().map(|failure| flaky(test, address, pass, failure));
            Ok(Filed { filing: Filing::Write(Slot::Pass), flaky })
        }
        Verdict::Fail(failure) => {
            if at.failure.is_some() {
                return Ok(Filed { filing: Filing::Held(Slot::Failure), flaky: None });
            }
            let flaky = at.pass.as_ref().map(|pass| flaky(test, address, pass, failure));
            Ok(Filed { filing: Filing::Write(Slot::Failure), flaky })
        }
    }
}

/// What a `Flaky` finding says: the test, the address, and the two runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Flaky {
    pub test: TestId,
    pub address: Address,
    /// The checkout the pass ran on.
    pub passed: CheckoutKey,
    /// The checkout the failure ran on.
    pub failed: CheckoutKey,
    pub why: Failure,
}

/// A finding (rule 81): no level, an item-relative range, its rule and its arguments. The same
/// fields as `blaze_drive::record::Finding` (`rule`, `start`, `end`, `message`), which blaze-run
/// cannot name (blaze-drive depends on it): the driver files it as one, with `message` written
/// by [`Finding::message`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Finding {
    pub rule: u16,
    /// Relative to the test item's first byte. A flaky test marks its item's start: `0..0`.
    pub start: u32,
    pub end: u32,
    pub flaky: Flaky,
}

impl Finding {
    /// The finding's words, exactly: `test 0b00000000000000 passed and failed at address
    /// 2a00000000000000: it passed on checkout 0100000000000000 and panicked on checkout
    /// 0200000000000000, so its result rests on something its asks do not name`.
    pub fn message(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        let flaky = &self.flaky;
        write!(
            out,
            "test {} passed and failed at address {}: it passed on checkout {} and {} on checkout {}, so its result rests on something its asks do not name",
            Hex(&flaky.test.0),
            Hex(&flaky.address.0),
            Hex(&flaky.passed.0),
            Why(flaky.why),
            Hex(&flaky.failed.0),
        )
    }
}

/// The `Flaky` finding for a pass and a failure filed at one address.
pub fn flaky(test: TestId, address: Address, pass: &Passed, failure: &Failed<'_>) -> Finding {
    Finding {
        rule: FLAKY_RULE,
        start: 0,
        end: 0,
        flaky: Flaky { test, address, passed: pass.run, failed: failure.run, why: failure.ended.why() },
    }
}

/// A failure in words: `panicked`, `did not panic`, `panicked without the expected text`,
/// `ended by signal 11`, `was refused by the sandbox`, `was killed past its deadline`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Why(pub Failure);

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Failure::Panicked => f.write_str("panicked"),
            Failure::DidNotPanic => f.write_str("did not panic"),
            Failure::PanicTextMissing => f.write_str("panicked without the expected text"),
            Failure::Signal { number } => write!(f, "ended by signal {number}"),
            Failure::Refused => f.write_str("was refused by the sandbox"),
            Failure::TimedOut { .. } => f.write_str("was killed past its deadline"),
            Failure::NoReport { code } => write!(f, "exited with code {code} and no whole report"),
        }
    }
}

/// A reason a pass is not kept, in words: `read the real clock`, and so on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotKeptWords(pub NotKept);

impl fmt::Display for NotKeptWords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            NotKept::Clock => "read the real clock",
            NotKept::Network => "reached the network",
            NotKept::WroteOutside => "wrote outside its temporary folder",
            NotKept::Foreign => "entered foreign code",
            NotKept::Unsandboxed => "ran outside the sandbox",
            NotKept::Descriptor => "read through a descriptor it did not open",
            NotKept::Threads => "started a thread with no reach set",
            NotKept::Untraced => "was not traced",
        })
    }
}
