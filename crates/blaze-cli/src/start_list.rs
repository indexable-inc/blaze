//! The start list (docs/v2/plan/design/run.md, stage 7 of "End state (D113)", "Start list" in
//! "The child and the parent", and "The cut to the lazy parent (D124)"): a HINT, one mutable
//! root a (program, request shape) and user. It holds the certificate it was written under,
//! then at most [`MOST_ROWS`] rows `(symbol, the code record's key then, the record's id)` in
//! the order the parent first answered them inside the start window (this run's first
//! [`MOST_ROWS`] first answers: the window is the cap).
//!
//! The head carries the run's START HEAD too (blaze-ladder run/special.rs `StartHead`: the
//! entry's launcher form and symbol, and the personality routine's symbol, which the parent
//! names before its first `Publish`), so a start under today's certificate spawns the child
//! with no front-end read at all: both come from the list ([`TodayRows::head`]), and nothing
//! else of the list is read under another certificate (an entry of another program text is
//! never started). The personality's symbol is its instance name's hash, so it is a function
//! of the routine's item key, and the certificate covers every text that decides which item
//! that is.
//!
//! Next start, under the SAME certificate, the rows are placed as they are, before any need is
//! read: nothing keyed or looked up ([`Held::Today`]). Under another (an edit, an undo, another
//! worktree, a pull) a row is only a symbol ([`Held::Other`] hands out [`Symbols`], which has
//! no key and no record): each is resolved like any first call, so an old list can cost time
//! and never run old code. Last writer wins; it is never shared by content, because its order
//! is a fact of one run.
//!
//! Two halves. The sans-IO core: [`Rows`] (the cap, refused by name), [`encode`] (deterministic
//! bytes: little-endian words, rows in first-answer order, nothing of the machine) and [`read`]
//! (every limit checked before a row is trusted: the length against [`LONGEST`], the magic,
//! the version, the row count against [`MOST_ROWS`], the exact length, the entry's form, then
//! each symbol once).
//! The thin driver over the command's one store ([`held`], [`write`]): the ONE mechanism the
//! command has for a mutable name, blaze-cas's POINTER (`Lent::point` / `Lent::pointed`,
//! `POINTER_TAG`: the newest commit's value wins), as code_store.rs points a code key at its
//! entry. The list's bytes are a record under their content key ([`START_LIST_TAG`],
//! `Namespace::LOCAL`), and the list's name ([`ListName`]) points at it; both ride the
//! command's one commit after the answer (store.rs `after_answer_printed`).
//!
//! Byte form, version [`VERSION`]:
//!
//! | at | bytes | what |
//! |---|---|---|
//! | 0 | 8 | [`MAGIC`] |
//! | 8 | 4 | the version, little-endian |
//! | 12 | 4 | the row count, little-endian, at most [`MOST_ROWS`] |
//! | 16 | 32 | the certificate it was written under |
//! | 48 | 4 | the entry's launcher form, little-endian: 0 `unit`, 1 `c-main`, 2 `hosted` |
//! | 52 | 32 | the entry's symbol |
//! | 84 | 4 | whether a personality routine is named, little-endian: 0 none, 1 one |
//! | 88 | 32 | its symbol; all zero when none is named |
//! | 120 | 96 a row | the symbol, the code record's key, the record's id |

use core::fmt;
use std::alloc::Global;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, Namespace, Tag};
use blaze_core::Driver;
use blaze_hash::ByHashSet;
use blaze_ids::ContentId;
use blaze_ladder::code_records::CodeKey;
use blaze_ladder::run::special::{EntryForm, EntryLine, StartHead};
use blaze_launch::wire::RecordId;
use blaze_launch::wire::run::Symbol;
use blaze_own::{AVec, Alloc, Vec};

/// The list's magic: the form's name; its version follows it ([`VERSION`]).
pub(crate) const MAGIC: [u8; 8] = *b"blzstart";

/// The form's version. A list of another is refused by name ([`ReadError::Version`]): the
/// run proceeds as with none, says so, and writes today's form at exit. Version 3: no row's
/// record names a static (run_parent.rs, `Resolved::Record`'s `named`); a version-2
/// list could hold a record naming a static an earlier answer told, which the child then met
/// before that static's `Data`.
pub(crate) const VERSION: u32 = 3;

/// The most rows a list holds (run.md stage 7: "a cap of 8,192 rows"); one more is refused by
/// name ([`CapError::Full`]).
pub(crate) const MOST_ROWS: usize = 8192;

/// The head's bytes: magic, version, row count, certificate, the entry's form and symbol, the
/// personality's word and symbol.
pub(crate) const HEAD: usize = 8 + 4 + 4 + 32 + 4 + 32 + 4 + 32;

/// The entry's form as the head writes it: one number a form, never reused.
const fn form_word(form: EntryForm) -> u32 {
    match form {
        EntryForm::Unit => 0,
        EntryForm::CMain => 1,
        EntryForm::Hosted => 2,
    }
}

/// The form a head's word names, if any.
const fn word_form(word: u32) -> Option<EntryForm> {
    match word {
        0 => Some(EntryForm::Unit),
        1 => Some(EntryForm::CMain),
        2 => Some(EntryForm::Hosted),
        _ => None,
    }
}

/// A row's bytes: symbol, code key, record id.
pub(crate) const ROW: usize = 32 * 3;

/// The longest list: a longer input is refused before any of it is read.
pub(crate) const LONGEST: usize = HEAD + MOST_ROWS * ROW;

/// The tag the list's record is filed under in the plain store's index: its own number, apart
/// from the roots' (`u16::MAX`), the startup image's (`u16::MAX - 1`), the drive's records
/// (`DRIVE_TAG`, `u16::MAX - 2`), code entries (`CODE_TAG`, `u16::MAX - 3`) and pointers
/// (`POINTER_TAG`, `u16::MAX - 4`). A pointer of the list's name naming a record of another tag
/// is refused by the store (`LentError::TargetTag`), never read as a list.
pub(crate) const START_LIST_TAG: Tag = Tag(u16::MAX - 5);

/// The domain of a list name's key.
const NAME_DOMAIN: &str = "blaze-cli start list name 1";

/// The domain of a list record's content key.
const RECORD_DOMAIN: &str = "blaze-cli start list record 1";

/// The certificate a list was written under: the request's (blaze-drive `Request::certificate`,
/// the tool, the command and every input's digest).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Certificate(pub [u8; 32]);

/// The name a (program, request shape) list hangs from: a key derived from the request's shape
/// (blaze-drive `request::shape_of`, which holds the command and the program), in the user's
/// one store. Not the checkout's: two worktrees of one program share one list, and the
/// certificate decides whether its rows are placed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ListName(ContentId);

impl ListName {
    pub(crate) fn of(shape: &ContentId) -> ListName {
        ListName(ContentId::of(NAME_DOMAIN, &shape.0))
    }
}

/// What `blaze run` reads and writes the list under: today's certificate and the list's name,
/// handed from main.rs's `Effect::StartProgram` site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Today {
    pub certificate: Certificate,
    pub name: ListName,
}

/// One row: a function instance the parent answered, the code record's key it had then, and
/// the record's id (its file in the records folder).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Row {
    pub symbol: Symbol,
    pub key: CodeKey,
    pub record: RecordId,
}

/// A list over its cap: the row that did not fit, by its symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CapError {
    Full { symbol: Symbol },
}

impl fmt::Display for CapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapError::Full { symbol } => {
                write!(f, "the start list holds at most {MOST_ROWS} rows, and the row of symbol ")?;
                hex(f, &symbol.0)?;
                f.write_str(" is one more (run.md stage 7)")
            }
        }
    }
}

impl core::error::Error for CapError {}

/// The rows of a list being made, in the order pushed; at most [`MOST_ROWS`].
pub(crate) struct Rows<'a> {
    rows: AVec<'a, Row>,
}

impl<'a> Rows<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Rows<'a> {
        Rows { rows: Vec::new_in(alloc) }
    }

    /// `row` after the others; past [`MOST_ROWS`] refused, naming it.
    pub(crate) fn push(&mut self, row: Row) -> Result<(), CapError> {
        if self.rows.len() >= MOST_ROWS {
            return Err(CapError::Full { symbol: row.symbol });
        }
        self.rows.push(row);
        Ok(())
    }

    pub(crate) fn is_full(&self) -> bool {
        self.rows.len() >= MOST_ROWS
    }

    pub(crate) fn as_slice(&self) -> &[Row] {
        self.rows.as_slice()
    }
}

/// The list of `rows` under `certificate` with the run's `head`, its bytes onto the end of
/// `out` (the module's table). Equal inputs, equal bytes. More than [`MOST_ROWS`] rows is
/// refused, naming the first past it.
pub(crate) fn encode(certificate: Certificate, head: StartHead, rows: &[Row], out: &mut AVec<'_, u8>) -> Result<(), CapError> {
    let entry = head.line;
    if let Some(past) = rows.get(MOST_ROWS) {
        return Err(CapError::Full { symbol: past.symbol });
    }
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    // At most MOST_ROWS (checked above): fits in 32 bits.
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&certificate.0);
    out.extend_from_slice(&form_word(entry.form).to_le_bytes());
    out.extend_from_slice(&entry.symbol.0);
    match head.personality {
        None => {
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&[0u8; 32]);
        }
        Some(symbol) => {
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(&symbol.0);
        }
    }
    for row in rows {
        out.extend_from_slice(&row.symbol.0);
        out.extend_from_slice(&row.key.0.0);
        out.extend_from_slice(&row.record.0);
    }
    Ok(())
}

/// Why bytes are not a start list, by name. Each is checked before any row is trusted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReadError {
    /// Longer than the longest list ([`LONGEST`]): refused before a byte is read.
    Oversized { len: usize },
    /// Shorter than the head.
    Short { len: usize },
    /// Not a start list.
    Magic { found: [u8; 8] },
    /// A start list of another version.
    Version { found: u32 },
    /// A row count past [`MOST_ROWS`].
    TooMany { rows: u32 },
    /// The length is not the head and the count's rows: truncated, or bytes past its end.
    Length { len: usize, want: usize },
    /// The entry's form word names no launcher form.
    Form { found: u32 },
    /// The personality's word is neither 0 nor 1, or is 0 with a symbol that is not all zero.
    Personality { found: u32 },
    /// One symbol in two rows (the writer answers each symbol first once).
    Twice { symbol: Symbol },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = "the start list (run.md stage 7)";
        match self {
            ReadError::Oversized { len } => write!(f, "{what} is {len} bytes, past the longest list of {LONGEST}"),
            ReadError::Short { len } => write!(f, "{what} is {len} bytes, shorter than its {HEAD}-byte head"),
            ReadError::Magic { found } => write!(f, "{what} begins {found:?}, not its magic {MAGIC:?}"),
            ReadError::Version { found } => write!(f, "{what} is of version {found}; this blaze reads version {VERSION}"),
            ReadError::TooMany { rows } => write!(f, "{what} says {rows} rows, past its cap of {MOST_ROWS}"),
            ReadError::Length { len, want } => write!(f, "{what} is {len} bytes where its head says {want}: truncated or overlong"),
            ReadError::Form { found } => write!(f, "{what}'s entry form is {found}, which names no launcher form (0 `unit`, 1 `c-main`, 2 `hosted`)"),
            ReadError::Personality { found } => {
                write!(f, "{what}'s personality word is {found}: 0 (none, its symbol all zero) or 1 (one) is read, nothing else")
            }
            ReadError::Twice { symbol } => {
                write!(f, "{what} holds the symbol ")?;
                hex(f, &symbol.0)?;
                f.write_str(" in two rows")
            }
        }
    }
}

impl core::error::Error for ReadError {}

/// A list read in place: its certificate, its head and its rows' bytes, every limit checked.
/// The head is handed out only under today's certificate ([`TodayRows::head`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ListView<'b> {
    pub certificate: Certificate,
    head: StartHead,
    rows: &'b [u8],
}

/// `bytes` as a start list (the module's table), every limit checked first; `alloc` holds the
/// set the symbols are checked once against, dropped before the return.
pub(crate) fn read<'b>(bytes: &'b [u8], alloc: Alloc<'_>) -> Result<ListView<'b>, ReadError> {
    let len = bytes.len();
    if len > LONGEST {
        return Err(ReadError::Oversized { len });
    }
    let Some(head) = bytes.first_chunk::<HEAD>() else { return Err(ReadError::Short { len }) };
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&head[..8]);
    if magic != MAGIC {
        return Err(ReadError::Magic { found: magic });
    }
    let word = |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
    let version = word(8);
    if version != VERSION {
        return Err(ReadError::Version { found: version });
    }
    let count = word(12);
    if count as usize > MOST_ROWS {
        return Err(ReadError::TooMany { rows: count });
    }
    let want = HEAD + count as usize * ROW;
    if len != want {
        return Err(ReadError::Length { len, want });
    }
    let form = word(48);
    let form = word_form(form).ok_or(ReadError::Form { found: form })?;
    let mut certificate = [0u8; 32];
    certificate.copy_from_slice(&head[16..48]);
    let mut entry = [0u8; 32];
    entry.copy_from_slice(&head[52..84]);
    let mut routine = [0u8; 32];
    routine.copy_from_slice(&head[88..120]);
    let personality = match word(84) {
        0 if routine == [0u8; 32] => None,
        1 => Some(blaze_code::record::Symbol(routine)),
        found => return Err(ReadError::Personality { found }),
    };
    let line = EntryLine { form, symbol: blaze_code::record::Symbol(entry) };
    let view = ListView { certificate: Certificate(certificate), head: StartHead { line, personality }, rows: &bytes[HEAD..] };
    let mut seen: ByHashSet<[u8; 32], Alloc<'_>> = ByHashSet::with_capacity_in(count as usize, alloc);
    for row in view.iter() {
        if !seen.insert(row.symbol.0) {
            return Err(ReadError::Twice { symbol: row.symbol });
        }
    }
    Ok(view)
}

impl<'b> ListView<'b> {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rows.len() / ROW
    }

    /// The rows in first-answer order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = Row> + Clone + 'b {
        self.rows.chunks_exact(ROW).map(|row| {
            let word = |at: usize| {
                let mut out = [0u8; 32];
                out.copy_from_slice(&row[at..at + 32]);
                out
            };
            Row { symbol: Symbol(word(0)), key: CodeKey(ContentId(word(32))), record: RecordId(word(64)) }
        })
    }

    /// What the list is to a run under `today`: its rows as they are when it was written under
    /// today's certificate, its symbols alone under any other.
    pub(crate) fn under(self, today: Certificate) -> Held<'b> {
        match self.certificate == today {
            true => Held::Today(TodayRows { view: self }),
            false => Held::Other(Symbols { view: self }),
        }
    }
}

/// A list as a run takes it ([`ListView::under`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Held<'b> {
    /// Written under today's certificate: its rows are placed as they are.
    Today(TodayRows<'b>),
    /// Written under another: only its symbols, each resolved like any first call.
    #[allow(dead_code, reason = "compiled-behind (run.md piece 10) reads the symbols; today a run resolves each at its first call")]
    Other(Symbols<'b>),
}

/// A list's rows under today's certificate: the one form that hands out keys and records.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TodayRows<'b> {
    view: ListView<'b>,
}

impl<'b> TodayRows<'b> {
    pub(crate) fn iter(&self) -> impl Iterator<Item = Row> + Clone + 'b {
        self.view.iter()
    }

    /// The run's start head as the list was written with it: today's, since the certificate is
    /// (every input of the program's start and of its personality routine is under it).
    pub(crate) fn head(&self) -> StartHead {
        self.view.head
    }
}

/// A list's symbols under another certificate: no key, no record, so no old code can be placed
/// from it (rule 76).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Symbols<'b> {
    view: ListView<'b>,
}

impl<'b> Symbols<'b> {
    /// The symbols in the list's order. Read by compiled-behind (run.md piece 10) to resolve
    /// them ahead; today a run resolves each at its first call.
    #[allow(dead_code, reason = "compiled-behind (run.md piece 10) resolves these ahead; the tests read them now")]
    pub(crate) fn iter(&self) -> impl Iterator<Item = Symbol> + Clone + 'b {
        self.view.iter().map(|row| row.symbol)
    }
}

/// Why the list could not be read from or written to the store, by name.
#[derive(Debug)]
pub(crate) enum ListError {
    /// The list's bytes are over the cap (never: the parent records at most [`MOST_ROWS`]).
    Cap(CapError),
    /// The store refused the read or the write.
    Store(LentError),
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListError::Cap(why) => write!(f, "{why}"),
            ListError::Store(why) => write!(f, "the start list in the store: {why}"),
        }
    }
}

impl core::error::Error for ListError {}

/// The list `name` points at, in place for the life of the store's borrow; `None` when no list
/// was ever written, or the store let its record go (compaction over the cap keeps a pointer's
/// newest value, not its target): a hint lost costs a start, and the list is written again at
/// exit.
pub(crate) fn held<'m, D: Driver>(lent: &'m Lent<D, Global>, name: &ListName) -> Result<Option<&'m [u8]>, ListError> {
    let target = match lent.pointed(START_LIST_TAG, &CasKey(name.0)) {
        Ok(Some(target)) => target,
        Ok(None) | Err(LentError::TargetGone { .. }) => return Ok(None),
        Err(why) => return Err(ListError::Store(why)),
    };
    // `pointed` checked the target is held under START_LIST_TAG.
    Ok(lent.get(&target).map_err(ListError::Store)?.map(|lend| lend.bytes))
}

/// What [`write`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wrote {
    /// The list held is these bytes: nothing put, nothing pointed.
    Nothing,
    /// The bytes put under their content key and `name` pointed at them, for the next commit.
    Written,
}

/// `bytes` (an [`encode`]d list) as the list `name` points at, unless `held` (what [`held`]
/// read at the start) is these bytes already: then nothing is written.
pub(crate) fn write<D: Driver>(lent: &Lent<D, Global>, name: &ListName, held: Option<&[u8]>, bytes: &[u8]) -> Result<Wrote, ListError> {
    if held == Some(bytes) {
        return Ok(Wrote::Nothing);
    }
    let content = CasKey(ContentId::of(RECORD_DOMAIN, bytes));
    lent.put(&content, Namespace::LOCAL, START_LIST_TAG, bytes).map_err(ListError::Store)?;
    lent.point(START_LIST_TAG, &CasKey(name.0), &content).map_err(ListError::Store)?;
    Ok(Wrote::Written)
}

/// 32 bytes as 64 lowercase hex digits.
fn hex(f: &mut fmt::Formatter<'_>, bytes: &[u8; 32]) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

/// Round trips, the cap, the certificate's two readings, typed refusals of bad bytes, and the
/// store's write-nothing-when-equal.
#[cfg(test)]
#[path = "start_list_tests.rs"]
mod tests;
