//! The finding record: what one lint found in one item, level-free and text-free.
//!
//! A finding holds the lint, the item it is in (its [`StableId`]), a range relative to the item's
//! start (never an absolute line or column, rule 71: an edit above an item moves no finding), the
//! message as a template index with typed arguments, and the fix as edits on item-relative ranges
//! with its applicability. What it was computed from is the record's, not the finding's: the
//! record's key and (for a traced kind) its read list ([`crate::kinds`]).
//!
//! Not in a finding: its level, its group, whether a ratchet baseline holds it, and its drawn
//! text. Those are the view's ([`crate::view`]), joined when a reply is made, so changing a level
//! changes no key and recomputes nothing.
//!
//! The payload is fixed-width little-endian, read in place: a header, then rows, arguments and
//! edits as arrays, then one byte pool for names and fix text. [`Findings::open`] checks every
//! offset once (verify once, decision 18); after it, no accessor can fail.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_keyed::record::StableId;

/// A lint by its full name (`blaze::could_be_narrower`, `clippy::needless_range_loop`): its
/// name's 64-bit FNV-1a hash, so a lint table is a `const` and the id is stable across releases
/// while the name is. A collision among the few thousand lints is refused where the table is
/// built, naming both.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LintId(pub u64);

/// 64-bit FNV-1a of `bytes` under a one-byte domain: the const hash of a name.
pub const fn name_hash(domain: u8, bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ domain as u64;
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        at += 1;
    }
    hash
}

impl LintId {
    pub const fn of(full_name: &str) -> LintId {
        LintId(name_hash(b'l', full_name.as_bytes()))
    }
}

/// A byte range from the start of its item's text: `start..end`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ItemRange {
    pub start: u32,
    pub end: u32,
}

/// How sure a fix is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Applicability {
    /// Applied by `blaze check --fix` without asking: the program means the same after it.
    MachineApplicable = 0,
    MaybeIncorrect = 1,
    HasPlaceholders = 2,
    Unspecified = 3,
}

impl Applicability {
    fn from_u8(byte: u8) -> Option<Applicability> {
        match byte {
            0 => Some(Applicability::MachineApplicable),
            1 => Some(Applicability::MaybeIncorrect),
            2 => Some(Applicability::HasPlaceholders),
            3 => Some(Applicability::Unspecified),
            _ => None,
        }
    }
}

/// One typed argument of a message. Drawing it (a type's path, an item's name) is the view's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg<'b> {
    Item(StableId),
    /// A type, by the content id of its canonical form.
    Type(ContentId),
    Lint(LintId),
    Int(i128),
    /// A closed range of values, `lo..=hi`.
    Range { lo: i128, hi: i128 },
    Count(u64),
    /// A name as written (an identifier, a feature): bytes, never a sentence.
    Name(&'b [u8]),
}

impl Arg<'_> {
    fn tag(&self) -> u8 {
        match self {
            Arg::Item(_) => 0,
            Arg::Type(_) => 1,
            Arg::Lint(_) => 2,
            Arg::Int(_) => 3,
            Arg::Range { .. } => 4,
            Arg::Count(_) => 5,
            Arg::Name(_) => 6,
        }
    }
}

/// One edit of a fix: replace `at` in `item` with `text`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Edit<'b> {
    pub item: StableId,
    pub at: ItemRange,
    pub text: &'b [u8],
}

/// A fix: its edits, in order, and how sure it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fix<'b> {
    pub applicability: Applicability,
    pub edits: &'b [Edit<'b>],
}

/// A finding as a lint makes it, before it is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Finding<'b> {
    pub lint: LintId,
    pub item: StableId,
    pub at: ItemRange,
    /// The lint's message template, by index in the lint's table.
    pub message: u16,
    pub args: &'b [Arg<'b>],
    pub fix: Option<Fix<'b>>,
}

/// A finding's identity across edits: its lint, item, message, arguments and its ordinal among
/// findings equal in all four, never its position. An edit inside an item that leaves a finding
/// standing keeps its id, so "new since" and "fixed since" ([`crate::view::diff`]) are exact.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FindingId(pub ContentId);

impl FindingId {
    fn of<'b>(lint: LintId, item: StableId, message: u16, args: impl Iterator<Item = Arg<'b>>, ordinal: u32) -> FindingId {
        let mut hasher = Hasher::new("blaze-findings finding id");
        hasher.update(&lint.0.to_le_bytes()).id(&item.0).update(&message.to_le_bytes());
        for arg in args {
            hasher.update(&[arg.tag()]);
            match arg {
                Arg::Item(StableId(id)) | Arg::Type(id) => {
                    hasher.id(&id);
                }
                Arg::Lint(lint) => {
                    hasher.update(&lint.0.to_le_bytes());
                }
                Arg::Int(value) => {
                    hasher.update(&value.to_le_bytes());
                }
                Arg::Range { lo, hi } => {
                    hasher.update(&lo.to_le_bytes()).update(&hi.to_le_bytes());
                }
                Arg::Count(count) => {
                    hasher.update(&count.to_le_bytes());
                }
                Arg::Name(name) => {
                    hasher.part(name);
                }
            }
        }
        hasher.update(&ordinal.to_le_bytes());
        FindingId(hasher.finish())
    }
}

/// The payload layout's version: a change bumps every finding kind's `VERSION`.
pub const FORMAT: u32 = 1;

const HEADER: usize = 24;
const ROW: usize = 72;
const ARG: usize = 72;
const EDIT: usize = 48;

/// Why bytes are not a findings payload: the byte offset and what was wrong there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FindingsError {
    pub at: usize,
    pub what: &'static str,
}

impl fmt::Display for FindingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a findings payload, at byte {}: {}", self.at, self.what)
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn i128_at(bytes: &[u8], at: usize) -> i128 {
    let mut word = [0u8; 16];
    word.copy_from_slice(&bytes[at..at + 16]);
    i128::from_le_bytes(word)
}

fn id_at(bytes: &[u8], at: usize) -> ContentId {
    let mut id = [0u8; 32];
    id.copy_from_slice(&bytes[at..at + 32]);
    ContentId(id)
}

/// Writes findings into a payload. Rows are sorted by (item, range, lint, message), stably, so
/// the bytes depend only on what the lint found and the order it found it in (rule 25).
pub struct Writer {
    rows: Vec<(StableId, ItemRange, LintId, u16, [u8; ROW]), Global>,
    args: Vec<u8, Global>,
    args_count: u32,
    edits: Vec<u8, Global>,
    edits_count: u32,
    text: Vec<u8, Global>,
}

impl Default for Writer {
    fn default() -> Writer {
        Writer::new()
    }
}

impl Writer {
    pub fn new() -> Writer {
        Writer {
            rows: Vec::new_in(Global),
            args: Vec::new_in(Global),
            args_count: 0,
            edits: Vec::new_in(Global),
            edits_count: 0,
            text: Vec::new_in(Global),
        }
    }

    fn text_of(&mut self, bytes: &[u8]) -> (u32, u32) {
        let at = self.text.len() as u32;
        self.text.extend_from_slice(bytes);
        (at, bytes.len() as u32)
    }

    pub fn push(&mut self, finding: &Finding<'_>) {
        let args_at = self.args_count;
        for arg in finding.args {
            let mut slot = [0u8; ARG];
            slot[0] = arg.tag();
            let (a, b, id) = match *arg {
                Arg::Item(StableId(id)) | Arg::Type(id) => (0, 0, id),
                Arg::Lint(lint) => (i128::from(lint.0), 0, ContentId::default()),
                Arg::Int(value) => (value, 0, ContentId::default()),
                Arg::Range { lo, hi } => (lo, hi, ContentId::default()),
                Arg::Count(count) => (i128::from(count), 0, ContentId::default()),
                Arg::Name(name) => {
                    let (at, len) = self.text_of(name);
                    (i128::from(at), i128::from(len), ContentId::default())
                }
            };
            slot[8..24].copy_from_slice(&a.to_le_bytes());
            slot[24..40].copy_from_slice(&b.to_le_bytes());
            slot[40..72].copy_from_slice(&id.0);
            self.args.extend_from_slice(&slot);
            self.args_count += 1;
        }
        let edits_at = self.edits_count;
        let (applicability, has_fix) = match finding.fix {
            Some(fix) => {
                for edit in fix.edits {
                    let (text_at, text_len) = self.text_of(edit.text);
                    let mut slot = [0u8; EDIT];
                    slot[0..32].copy_from_slice(&edit.item.0.0);
                    slot[32..36].copy_from_slice(&edit.at.start.to_le_bytes());
                    slot[36..40].copy_from_slice(&edit.at.end.to_le_bytes());
                    slot[40..44].copy_from_slice(&text_at.to_le_bytes());
                    slot[44..48].copy_from_slice(&text_len.to_le_bytes());
                    self.edits.extend_from_slice(&slot);
                    self.edits_count += 1;
                }
                (fix.applicability as u8, 1u8)
            }
            None => (Applicability::Unspecified as u8, 0u8),
        };
        let mut row = [0u8; ROW];
        row[0..8].copy_from_slice(&finding.lint.0.to_le_bytes());
        row[8..40].copy_from_slice(&finding.item.0.0);
        row[40..44].copy_from_slice(&finding.at.start.to_le_bytes());
        row[44..48].copy_from_slice(&finding.at.end.to_le_bytes());
        row[48..50].copy_from_slice(&finding.message.to_le_bytes());
        row[50] = applicability;
        row[51] = has_fix;
        row[52..56].copy_from_slice(&args_at.to_le_bytes());
        row[56..60].copy_from_slice(&(finding.args.len() as u32).to_le_bytes());
        row[60..64].copy_from_slice(&edits_at.to_le_bytes());
        row[64..68].copy_from_slice(&(self.edits_count - edits_at).to_le_bytes());
        self.rows.push((finding.item, finding.at, finding.lint, finding.message, row));
    }

    pub fn finish(mut self) -> Vec<u8, Global> {
        self.rows.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));
        let mut out = Vec::with_capacity_in(
            HEADER + self.rows.len() * ROW + self.args.len() + self.edits.len() + self.text.len(),
            Global,
        );
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&(self.rows.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.args_count.to_le_bytes());
        out.extend_from_slice(&self.edits_count.to_le_bytes());
        out.extend_from_slice(&(self.text.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        for (_, _, _, _, row) in &self.rows {
            out.extend_from_slice(row);
        }
        out.extend_from_slice(&self.args);
        out.extend_from_slice(&self.edits);
        out.extend_from_slice(&self.text);
        out
    }
}

/// A checked findings payload, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Findings<'b> {
    bytes: &'b [u8],
    count: usize,
    args_from: usize,
    edits_from: usize,
    text_from: usize,
}

impl<'b> Findings<'b> {
    /// Checks every count, offset and enum byte once.
    pub fn open(bytes: &'b [u8]) -> Result<Findings<'b>, FindingsError> {
        let fail = |at: usize, what: &'static str| Err(FindingsError { at, what });
        if bytes.len() < HEADER {
            return fail(0, "shorter than the header");
        }
        if u32_at(bytes, 0) != FORMAT {
            return fail(0, "another payload format");
        }
        let count = u32_at(bytes, 4) as usize;
        let args = u32_at(bytes, 8) as usize;
        let edits = u32_at(bytes, 12) as usize;
        let text = u32_at(bytes, 16) as usize;
        let args_from = HEADER + count * ROW;
        let edits_from = args_from + args * ARG;
        let text_from = edits_from + edits * EDIT;
        if bytes.len() != text_from + text {
            return fail(4, "the counts do not add up to the length");
        }
        let in_text = |at: u64, len: u64| at.checked_add(len).is_some_and(|end| end <= text as u64);
        for row in 0..count {
            let at = HEADER + row * ROW;
            if u32_at(bytes, at + 40) > u32_at(bytes, at + 44) {
                return fail(at + 40, "a range that ends before it starts");
            }
            if Applicability::from_u8(bytes[at + 50]).is_none() {
                return fail(at + 50, "an unknown applicability");
            }
            let has_fix = bytes[at + 51];
            if has_fix > 1 {
                return fail(at + 51, "a fix flag that is neither 0 nor 1");
            }
            let (args_at, args_len) = (u32_at(bytes, at + 52) as usize, u32_at(bytes, at + 56) as usize);
            if args_at + args_len > args {
                return fail(at + 52, "arguments past the argument table");
            }
            let (edits_at, edits_len) = (u32_at(bytes, at + 60) as usize, u32_at(bytes, at + 64) as usize);
            if edits_at + edits_len > edits || (has_fix == 0 && edits_len != 0) {
                return fail(at + 60, "edits past the edit table, or edits without a fix");
            }
            if row > 0 && Self::order_at(bytes, at - ROW) > Self::order_at(bytes, at) {
                return fail(at, "rows out of order");
            }
        }
        for arg in 0..args {
            let at = args_from + arg * ARG;
            match bytes[at] {
                0..=5 => {}
                6 => {
                    let (name_at, len) = (i128_at(bytes, at + 8), i128_at(bytes, at + 24));
                    let (Ok(name_at), Ok(len)) = (u64::try_from(name_at), u64::try_from(len)) else {
                        return fail(at + 8, "a name at a negative offset");
                    };
                    if !in_text(name_at, len) {
                        return fail(at + 8, "a name past the text pool");
                    }
                }
                _ => return fail(at, "an unknown argument kind"),
            }
        }
        for edit in 0..edits {
            let at = edits_from + edit * EDIT;
            if u32_at(bytes, at + 32) > u32_at(bytes, at + 36) {
                return fail(at + 32, "an edit range that ends before it starts");
            }
            if !in_text(u64::from(u32_at(bytes, at + 40)), u64::from(u32_at(bytes, at + 44))) {
                return fail(at + 40, "an edit's text past the text pool");
            }
        }
        Ok(Findings { bytes, count, args_from, edits_from, text_from })
    }

    fn order_at(bytes: &[u8], at: usize) -> (ContentId, u32, u32, u64, u16) {
        (id_at(bytes, at + 8), u32_at(bytes, at + 40), u32_at(bytes, at + 44), u64_at(bytes, at), u16_at(bytes, at + 48))
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn get(&self, index: usize) -> Option<Row<'b>> {
        (index < self.count).then(|| self.row(index))
    }

    fn row(&self, index: usize) -> Row<'b> {
        Row { of: *self, at: HEADER + index * ROW }
    }

    pub fn iter(&self) -> impl Iterator<Item = Row<'b>> + use<'b> {
        let of = *self;
        (0..self.count).map(move |index| Row { of, at: HEADER + index * ROW })
    }

    /// Each finding's id, in row order: the ordinal counts earlier rows equal in lint, item,
    /// message and arguments. Rows are sorted by item first, so only the item's own earlier
    /// rows are compared.
    pub fn ids(&self, out: &mut Vec<FindingId, Global>) {
        let mut item_from = 0;
        for index in 0..self.count {
            let row = self.row(index);
            if index > 0 && self.row(index - 1).item() != row.item() {
                item_from = index;
            }
            let mut ordinal = 0u32;
            for earlier in item_from..index {
                ordinal += u32::from(row.same_as(&self.row(earlier)));
            }
            out.push(FindingId::of(row.lint(), row.item(), row.message(), row.args(), ordinal));
        }
    }
}

/// One finding of a checked payload.
#[derive(Clone, Copy, Debug)]
pub struct Row<'b> {
    of: Findings<'b>,
    at: usize,
}

impl<'b> Row<'b> {
    fn bytes(&self) -> &'b [u8] {
        self.of.bytes
    }

    pub fn lint(&self) -> LintId {
        LintId(u64_at(self.bytes(), self.at))
    }

    pub fn item(&self) -> StableId {
        StableId(id_at(self.bytes(), self.at + 8))
    }

    pub fn at(&self) -> ItemRange {
        ItemRange { start: u32_at(self.bytes(), self.at + 40), end: u32_at(self.bytes(), self.at + 44) }
    }

    pub fn message(&self) -> u16 {
        u16_at(self.bytes(), self.at + 48)
    }

    pub fn args(&self) -> impl Iterator<Item = Arg<'b>> + use<'b> {
        let (of, from, len) = (self.of, u32_at(self.bytes(), self.at + 52) as usize, u32_at(self.bytes(), self.at + 56) as usize);
        (from..from + len).map(move |index| {
            let bytes = of.bytes;
            let at = of.args_from + index * ARG;
            let (a, b) = (i128_at(bytes, at + 8), i128_at(bytes, at + 24));
            match bytes[at] {
                0 => Arg::Item(StableId(id_at(bytes, at + 40))),
                1 => Arg::Type(id_at(bytes, at + 40)),
                2 => Arg::Lint(LintId(a as u64)),
                3 => Arg::Int(a),
                4 => Arg::Range { lo: a, hi: b },
                5 => Arg::Count(a as u64),
                // Checked by `open`: 6 is the only other kind, its bytes inside the pool.
                _ => {
                    let from = of.text_from + a as usize;
                    Arg::Name(&bytes[from..from + b as usize])
                }
            }
        })
    }

    /// The fix's applicability and edits, when there is one.
    pub fn fix(&self) -> Option<(Applicability, impl Iterator<Item = Edit<'b>> + use<'b>)> {
        let bytes = self.bytes();
        if bytes[self.at + 51] == 0 {
            return None;
        }
        // Checked by `open`.
        let applicability = Applicability::from_u8(bytes[self.at + 50]).unwrap_or(Applicability::Unspecified);
        let (of, from, len) = (self.of, u32_at(bytes, self.at + 60) as usize, u32_at(bytes, self.at + 64) as usize);
        let edits = (from..from + len).map(move |index| {
            let at = of.edits_from + index * EDIT;
            let bytes = of.bytes;
            let text_at = of.text_from + u32_at(bytes, at + 40) as usize;
            let text_len = u32_at(bytes, at + 44) as usize;
            Edit {
                item: StableId(id_at(bytes, at)),
                at: ItemRange { start: u32_at(bytes, at + 32), end: u32_at(bytes, at + 36) },
                text: &bytes[text_at..text_at + text_len],
            }
        });
        Some((applicability, edits))
    }

    /// Whether `other` is equal in lint, item, message and arguments: the same finding but for
    /// its position.
    fn same_as(&self, other: &Row<'b>) -> bool {
        self.lint() == other.lint()
            && self.item() == other.item()
            && self.message() == other.message()
            && self.args().eq(other.args())
    }
}
