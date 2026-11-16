//! What a forked test child tells its zygote on descriptor 3 (docs/v2/plan/design/test.md,
//! "End state", piece 10): one [`TestEnded`] when the test's function returned, panicked or was
//! refused, and, only when the zygote asked for it past the deadline, one [`Stacks`]. Never via
//! stdout or stderr: those are the test's own output, captured whole.
//!
//! A child is untrusted input: the parent opens a message with every length, tag, order and cap
//! checked once and refuses bad bytes with a typed [`WireError`], never a panic; what it then
//! reads it reads in place (rule 74).
//!
//! The form (integers little-endian, fixed width, lengths before every variable part):
//!
//! - a 56-byte header: `BZTE`, the version (u32), the kind (u8: 0 `TestEnded`, 1 `Stacks`),
//!   three zero bytes, the test's row in its unit's tests record (u32), the body's length
//!   (u32), four zero bytes, and at 24 the BLAKE3 digest (32 bytes) of the header's first 24
//!   bytes followed by the body. A report cut anywhere, or with any byte changed, does not open:
//!   a cut is short of its length, a change misses the digest;
//! - a `TestEnded` body: 64 fixed bytes, then the entered functions (32 bytes each), the
//!   outside reads (32 bytes each) and the text. At 0 the outcome (0 returned, 1 panicked, 2
//!   refused by the sandbox); 1 whether a place is given (only a panic gives one); 2 what the
//!   sandbox refused (result.rs's numbering; zero unless refused); 3 whether the run may be kept
//!   (0 kept, else 1 + the [`NotKept`] reason, as result.rs numbers it); 4 the text's length
//!   (u32: the panic message, or what was refused; at most [`TEXT_CAP`]); 8 the entered count
//!   (u32, at most [`ENTERED_CAP`]); 12 the read count (u32, at most [`READS_CAP`]); 16 the
//!   place's item (32 bytes) and 48 its item-relative offset (u32); 52 twelve zero bytes. The
//!   entered names and the read names are each sorted by their bytes and each once, so one run
//!   has one form;
//! - a `Stacks` body: the frame count (u32, at most [`FRAMES_CAP`]), four zero bytes, then
//!   each return address (u64), innermost first. Addresses are this process's own: they are
//!   printed, never kept in a key or a record.
//!
//! [`encode_stacks_into`] writes into a buffer its caller owns and allocates nothing: the child
//! writes `Stacks` from a signal handler.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::head::{le32, word};
use crate::plan::NotKept;
use crate::postings::{FnName, ReadName};
use crate::result::{Place, Refusal, TEXT_CAP, not_kept_number, not_kept_of, refusal_number, refusal_of};

pub const MAGIC: [u8; 4] = *b"BZTE";
pub const VERSION: u32 = 1;
/// The header's length; the digest is its last 32 bytes.
pub const HEADER: usize = 56;
const DIGESTED: usize = 24;
const ENDED_FIXED: usize = 64;
const STACKS_FIXED: usize = 8;
const NAME: usize = 32;
/// The most functions one report names as entered (32 MiB of names).
pub const ENTERED_CAP: usize = 1 << 20;
/// The most outside reads one report names.
pub const READS_CAP: usize = 1 << 16;
/// The most return addresses one `Stacks` holds.
pub const FRAMES_CAP: usize = 256;
/// The largest `TestEnded` body.
pub const ENDED_BODY_CAP: usize = ENDED_FIXED + ENTERED_CAP * NAME + READS_CAP * NAME + TEXT_CAP;
/// The largest `Stacks` message, header included: the buffer a signal handler writes it in.
pub const STACKS_CAP: usize = HEADER + STACKS_FIXED + FRAMES_CAP * 8;
/// The most the parent keeps of one child's descriptor 3: one `TestEnded` and one `Stacks`.
/// More is refused ([`WireError::TooLong`]) and the run fails as having no whole report.
pub const STREAM_CAP: usize = HEADER + ENDED_BODY_CAP + STACKS_CAP;

const KIND_ENDED: u8 = 0;
const KIND_STACKS: u8 = 1;
const RETURNED: u8 = 0;
const PANICKED: u8 = 1;
const REFUSED: u8 = 2;

/// How the test's function ended, as the child saw it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome<'r> {
    Returned,
    /// Its panic, with the message (cut to [`TEXT_CAP`] by `result::capped`) and where.
    Panicked { message: &'r str, at: Option<Place> },
    /// A launcher wrapper refused an operation the sandbox does not allow, and stopped the test.
    Refused { refusal: Refusal, what: &'r str },
}

/// A child's report, to encode: the outcome and the asks (the functions it entered, the
/// outside reads it made), each list sorted and each name once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Report<'a> {
    pub row: u32,
    pub outcome: Outcome<'a>,
    pub keep: Result<(), NotKept>,
    pub entered: &'a [FnName],
    pub reads: &'a [ReadName],
}

/// An opened `TestEnded`, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TestEnded<'r> {
    pub row: u32,
    pub outcome: Outcome<'r>,
    pub keep: Result<(), NotKept>,
    entered: &'r [u8],
    reads: &'r [u8],
}

impl<'r> TestEnded<'r> {
    pub const fn entered_len(&self) -> usize {
        self.entered.len() / NAME
    }

    pub const fn reads_len(&self) -> usize {
        self.reads.len() / NAME
    }

    /// The entered functions, in their sorted order.
    pub fn entered(&self) -> impl Iterator<Item = FnName> + 'r {
        self.entered.chunks_exact(NAME).map(|name| FnName(word(name, 0)))
    }

    /// The outside reads, in their sorted order.
    pub fn reads(&self) -> impl Iterator<Item = ReadName> + 'r {
        self.reads.chunks_exact(NAME).map(|name| ReadName(word(name, 0)))
    }
}

/// An opened `Stacks`: the hung test's return addresses, innermost first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stacks<'r> {
    pub row: u32,
    frames: &'r [u8],
}

impl<'r> Stacks<'r> {
    pub const fn len(&self) -> usize {
        self.frames.len() / 8
    }

    pub const fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn frames(&self) -> impl Iterator<Item = u64> + 'r {
        self.frames.chunks_exact(8).map(|frame| {
            let mut out = [0; 8];
            out.copy_from_slice(frame);
            u64::from_le_bytes(out)
        })
    }
}

/// One opened message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message<'r> {
    Ended(TestEnded<'r>),
    Stacks(Stacks<'r>),
}

/// How much of a stream's front is one message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Frame {
    /// Not yet a header, or a header whose body has not all come: at least this many bytes.
    Need { least: usize },
    /// The first `len` bytes are one message (not yet opened: [`open`] checks it).
    Whole { len: usize },
}

/// Bytes refused, each naming where.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    /// Fewer bytes than the form needs.
    Short { len: usize, least: usize },
    /// Not a message of this form: the magic differs.
    NotThis,
    /// A version this parent does not read.
    Version { found: u32 },
    /// An unknown tag at this offset.
    Tag { at: usize, byte: u8 },
    /// A byte that must be zero is not.
    NotZero { at: usize },
    /// A length over its cap.
    TooLong { what: &'static str, len: usize, cap: usize },
    /// The bytes' length is not the one the lengths inside say.
    Length { want: usize, len: usize },
    /// The digest does not match: a torn or changed message.
    Digest,
    /// A list out of order or with a name twice, at this index.
    Order { what: &'static str, at: usize },
    /// The text is not UTF-8.
    NotUtf8 { at: usize },
    /// The buffer a `Stacks` is written in is too small.
    Buffer { len: usize, least: usize },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            WireError::Short { len, least } => write!(f, "a test report of {len} bytes; its form needs at least {least}"),
            WireError::NotThis => f.write_str("not a test report: the magic is not `BZTE`"),
            WireError::Version { found } => write!(f, "a test report of version {found}; this parent reads {VERSION}"),
            WireError::Tag { at, byte } => write!(f, "a test report's tag {byte} at offset {at} is not one of the form's"),
            WireError::NotZero { at } => write!(f, "a test report's byte at offset {at} must be zero"),
            WireError::TooLong { what, len, cap } => write!(f, "a test report's {what} is {len}, over its cap {cap}"),
            WireError::Length { want, len } => write!(f, "a test report of {len} bytes; its lengths say {want}"),
            WireError::Digest => f.write_str("a test report's digest does not match its bytes: it was torn or changed"),
            WireError::Order { what, at } => {
                write!(f, "a test report's {what} at index {at} is not after the one before it (sorted, each once)")
            }
            WireError::NotUtf8 { at } => write!(f, "a test report's text at offset {at} is not UTF-8"),
            WireError::Buffer { len, least } => write!(f, "a buffer of {len} bytes for a stacks message of {least}"),
        }
    }
}

impl core::error::Error for WireError {}

fn digest(header: &[u8], body: &[u8]) -> [u8; 32] {
    let mut hasher = blaze_blake3::Hasher::new();
    hasher.update(&header[..DIGESTED]);
    hasher.update(body);
    hasher.finalize()
}

/// The header's first 24 bytes.
fn head(kind: u8, row: u32, body: usize) -> [u8; DIGESTED] {
    let mut out = [0; DIGESTED];
    out[..4].copy_from_slice(&MAGIC);
    out[4..8].copy_from_slice(&VERSION.to_le_bytes());
    out[8] = kind;
    out[12..16].copy_from_slice(&row.to_le_bytes());
    // The callers cap `body` far below `u32::MAX` (ENDED_BODY_CAP, STACKS_CAP).
    out[16..20].copy_from_slice(&(body as u32).to_le_bytes());
    out
}

fn sorted<T: Ord>(items: &[T], what: &'static str) -> Result<(), WireError> {
    match items.windows(2).position(|pair| pair[0] >= pair[1]) {
        Some(at) => Err(WireError::Order { what, at: at + 1 }),
        None => Ok(()),
    }
}

/// The report's bytes, in `alloc`. Refuses a list or a text over its cap and a list not sorted
/// or with a name twice (the child sorts them first: `child::Asks`).
pub fn encode<'b>(report: &Report<'_>, alloc: Alloc<'b>) -> Result<AVec<'b, u8>, WireError> {
    let (outcome, place, refusal, text) = match report.outcome {
        Outcome::Returned => (RETURNED, None, 0, ""),
        Outcome::Panicked { message, at } => (PANICKED, at, 0, message),
        Outcome::Refused { refusal, what } => (REFUSED, None, refusal_number(refusal), what),
    };
    if text.len() > TEXT_CAP {
        return Err(WireError::TooLong { what: "text", len: text.len(), cap: TEXT_CAP });
    }
    if report.entered.len() > ENTERED_CAP {
        return Err(WireError::TooLong { what: "entered count", len: report.entered.len(), cap: ENTERED_CAP });
    }
    if report.reads.len() > READS_CAP {
        return Err(WireError::TooLong { what: "read count", len: report.reads.len(), cap: READS_CAP });
    }
    sorted(report.entered, "entered function")?;
    sorted(report.reads, "outside read")?;
    let body_len = ENDED_FIXED + NAME * (report.entered.len() + report.reads.len()) + text.len();
    let mut body: AVec<'b, u8> = Vec::with_capacity_in(HEADER + body_len, alloc);
    // The header first, its digest filled in once the body is written.
    let first = head(KIND_ENDED, report.row, body_len);
    body.extend_from_slice(&first);
    body.extend_from_slice(&[0; 32]);
    body.push(outcome);
    body.push(u8::from(place.is_some()));
    body.push(refusal);
    body.push(match report.keep {
        Ok(()) => 0,
        Err(why) => not_kept_number(why),
    });
    body.extend_from_slice(&(text.len() as u32).to_le_bytes());
    body.extend_from_slice(&(report.entered.len() as u32).to_le_bytes());
    body.extend_from_slice(&(report.reads.len() as u32).to_le_bytes());
    match place {
        Some(place) => {
            body.extend_from_slice(&place.item.0);
            body.extend_from_slice(&place.offset.to_le_bytes());
        }
        None => body.extend_from_slice(&[0; 36]),
    }
    body.extend_from_slice(&[0; 12]);
    for name in report.entered {
        body.extend_from_slice(&name.0);
    }
    for name in report.reads {
        body.extend_from_slice(&name.0);
    }
    body.extend_from_slice(text.as_bytes());
    let sum = digest(&first, &body[HEADER..]);
    body[DIGESTED..HEADER].copy_from_slice(&sum);
    Ok(body)
}

/// A `Stacks` message of `frames` (at most [`FRAMES_CAP`]; more are cut, innermost kept) written
/// into `out`, which needs [`STACKS_CAP`] bytes at most; the length written. Allocates nothing
/// and takes no lock: a signal handler calls it.
pub fn encode_stacks_into(row: u32, frames: &[u64], out: &mut [u8]) -> Result<usize, WireError> {
    let frames = &frames[..frames.len().min(FRAMES_CAP)];
    let body_len = STACKS_FIXED + frames.len() * 8;
    let len = HEADER + body_len;
    if out.len() < len {
        return Err(WireError::Buffer { len: out.len(), least: len });
    }
    let first = head(KIND_STACKS, row, body_len);
    out[..DIGESTED].copy_from_slice(&first);
    out[HEADER..HEADER + 4].copy_from_slice(&(frames.len() as u32).to_le_bytes());
    out[HEADER + 4..HEADER + 8].copy_from_slice(&[0; 4]);
    for (at, frame) in frames.iter().enumerate() {
        let from = HEADER + STACKS_FIXED + at * 8;
        out[from..from + 8].copy_from_slice(&frame.to_le_bytes());
    }
    let sum = digest(&first, &out[HEADER..len]);
    out[DIGESTED..HEADER].copy_from_slice(&sum);
    Ok(len)
}

/// How much of `stream`'s front is one message: checks the header's fixed fields and the body's
/// cap, not the digest or the body ([`open`] does, once the message is whole).
pub fn frame(stream: &[u8]) -> Result<Frame, WireError> {
    if stream.len() < HEADER {
        return Ok(Frame::Need { least: HEADER });
    }
    let (kind, body) = header(stream)?;
    let cap = match kind {
        KIND_ENDED => ENDED_BODY_CAP,
        _ => STACKS_CAP - HEADER,
    };
    if body > cap {
        return Err(WireError::TooLong { what: "body", len: body, cap });
    }
    match stream.len() >= HEADER + body {
        true => Ok(Frame::Whole { len: HEADER + body }),
        false => Ok(Frame::Need { least: HEADER + body }),
    }
}

/// The header's checks: magic, version, kind, zero bytes; the kind and the body's length.
fn header(bytes: &[u8]) -> Result<(u8, usize), WireError> {
    if bytes[..4] != MAGIC {
        return Err(WireError::NotThis);
    }
    let version = le32(bytes, 4);
    if version != VERSION {
        return Err(WireError::Version { found: version });
    }
    let kind = bytes[8];
    if kind != KIND_ENDED && kind != KIND_STACKS {
        return Err(WireError::Tag { at: 8, byte: kind });
    }
    zero(bytes, 9, 12)?;
    zero(bytes, 20, 24)?;
    Ok((kind, le32(bytes, 16) as usize))
}

fn zero(bytes: &[u8], from: usize, to: usize) -> Result<(), WireError> {
    match bytes[from..to].iter().position(|byte| *byte != 0) {
        Some(at) => Err(WireError::NotZero { at: from + at }),
        None => Ok(()),
    }
}

/// Exactly one message, checked once: the header, the length, the digest, every tag, cap and
/// order. Any prefix of a message and any message with a byte changed is refused.
pub fn open(bytes: &[u8]) -> Result<Message<'_>, WireError> {
    open_with(bytes, true)
}

/// [`open`], and with `check_digest` false the digest is not checked: only the planted bug
/// [`crate::Plant::TrustTornReport`] passes false.
pub(crate) fn open_with(bytes: &[u8], check_digest: bool) -> Result<Message<'_>, WireError> {
    let len = bytes.len();
    if len < HEADER {
        return Err(WireError::Short { len, least: HEADER });
    }
    let (kind, body_len) = header(bytes)?;
    let want = HEADER + body_len;
    if len != want {
        return Err(WireError::Length { want, len });
    }
    let body = &bytes[HEADER..];
    if check_digest && digest(bytes, body) != word(bytes, DIGESTED) {
        return Err(WireError::Digest);
    }
    let row = le32(bytes, 12);
    match kind {
        KIND_ENDED => open_ended(row, body).map(Message::Ended),
        _ => open_stacks(row, body).map(Message::Stacks),
    }
}

fn open_ended(row: u32, body: &[u8]) -> Result<TestEnded<'_>, WireError> {
    let len = body.len();
    if len < ENDED_FIXED {
        return Err(WireError::Short { len, least: ENDED_FIXED });
    }
    let at = |offset: usize| HEADER + offset;
    let text_len = le32(body, 4) as usize;
    let entered = le32(body, 8) as usize;
    let reads = le32(body, 12) as usize;
    if text_len > TEXT_CAP {
        return Err(WireError::TooLong { what: "text", len: text_len, cap: TEXT_CAP });
    }
    if entered > ENTERED_CAP {
        return Err(WireError::TooLong { what: "entered count", len: entered, cap: ENTERED_CAP });
    }
    if reads > READS_CAP {
        return Err(WireError::TooLong { what: "read count", len: reads, cap: READS_CAP });
    }
    let want = ENDED_FIXED + NAME * (entered + reads) + text_len;
    if len != want {
        return Err(WireError::Length { want: HEADER + want, len: HEADER + len });
    }
    zero(body, 52, ENDED_FIXED).map_err(|_| WireError::NotZero { at: at(52) })?;
    let Some(keep) = not_kept_of(body[3]) else { return Err(WireError::Tag { at: at(3), byte: body[3] }) };
    let outcome_tag = body[0];
    let place = match (body[1], outcome_tag) {
        (0, _) => {
            zero(body, 16, 52).map_err(|_| WireError::NotZero { at: at(16) })?;
            None
        }
        (1, PANICKED) => Some(Place { item: FnName(word(body, 16)), offset: le32(body, 48) }),
        (1, _) => return Err(WireError::NotZero { at: at(1) }),
        (byte, _) => return Err(WireError::Tag { at: at(1), byte }),
    };
    let names_end = ENDED_FIXED + NAME * (entered + reads);
    let entered_bytes = &body[ENDED_FIXED..ENDED_FIXED + NAME * entered];
    let read_bytes = &body[ENDED_FIXED + NAME * entered..names_end];
    in_order(entered_bytes, "entered function")?;
    in_order(read_bytes, "outside read")?;
    let text = core::str::from_utf8(&body[names_end..]).map_err(|_| WireError::NotUtf8 { at: at(names_end) })?;
    let outcome = match outcome_tag {
        RETURNED if text_len == 0 && body[2] == 0 => Outcome::Returned,
        RETURNED if body[2] != 0 => return Err(WireError::NotZero { at: at(2) }),
        RETURNED => return Err(WireError::NotZero { at: at(4) }),
        PANICKED if body[2] != 0 => return Err(WireError::NotZero { at: at(2) }),
        PANICKED => Outcome::Panicked { message: text, at: place },
        REFUSED => match refusal_of(body[2]) {
            Some(refusal) => Outcome::Refused { refusal, what: text },
            None => return Err(WireError::Tag { at: at(2), byte: body[2] }),
        },
        byte => return Err(WireError::Tag { at: at(0), byte }),
    };
    Ok(TestEnded { row, outcome, keep, entered: entered_bytes, reads: read_bytes })
}

fn in_order(names: &[u8], what: &'static str) -> Result<(), WireError> {
    let count = names.len() / NAME;
    let mut at = 1;
    while at < count {
        if names[(at - 1) * NAME..at * NAME] >= names[at * NAME..(at + 1) * NAME] {
            return Err(WireError::Order { what, at });
        }
        at += 1;
    }
    Ok(())
}

fn open_stacks(row: u32, body: &[u8]) -> Result<Stacks<'_>, WireError> {
    let len = body.len();
    if len < STACKS_FIXED {
        return Err(WireError::Short { len, least: STACKS_FIXED });
    }
    let count = le32(body, 0) as usize;
    if count > FRAMES_CAP {
        return Err(WireError::TooLong { what: "frame count", len: count, cap: FRAMES_CAP });
    }
    let want = STACKS_FIXED + count * 8;
    if len != want {
        return Err(WireError::Length { want: HEADER + want, len: HEADER + len });
    }
    zero(body, 4, 8).map_err(|_| WireError::NotZero { at: HEADER + 4 })?;
    Ok(Stacks { row, frames: &body[STACKS_FIXED..] })
}
