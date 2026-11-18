//! The start list (docs/v2/plan/design/run.md, stage 7 of "End state (D113)", "Start list" in
//! "The child and the parent", and "The cut to the lazy parent (D124)"): a HINT, one mutable
//! root a (program, request shape) and user. It holds the certificate it was written under,
//! then at most [`MOST_ROWS`] rows ([`Line`]) in the order the parent first answered inside
//! the start window: each first answer is a GROUP, its `Data` rows (the statics its answer sent
//! the child, each `(symbol, datum record)`, in the batch's order: every datum once in the list,
//! in the group of the first answer that sent it), then its `Named` rows (every static its
//! record and those data name, ascending, each once: what [`crate::run_parent::Needs`]'s one
//! `Publish` writer checks is on the child before the record), then its code row `(symbol, the
//! code record's key then, the record's id)`. A replay sends each group as the run did: the
//! data not on the child in one `Data`, then the record's `Publish` (run.md, "Statics in the
//! run"). The window closes at the first answer whose group does not fit under the cap, so a
//! later answer never stands on statics the list does not hold.
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
//! the version (an older one is [`ReadError::Old`]: nothing of it is read, and the run writes
//! today's form at exit, rule 26), the row count against [`MOST_ROWS`], the exact length, the
//! entry's form, then each row's kind and zero padding, each group's order (data, then names
//! ascending, then its code row; no group without one), each code symbol once and each datum's
//! symbol once).
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
//! | 120 | 97 a row | the rows, in first-answer order (below) |
//!
//! A row, [`ROW`] bytes:
//!
//! | at | bytes | code row (kind 0) | datum row (kind 1) | named row (kind 2) |
//! |---|---|---|---|---|
//! | 0 | 1 | 0 | 1 | 2 |
//! | 1 | 32 | the fn's symbol | the static's symbol | the static's symbol |
//! | 33 | 32 | the code record's key | the datum record's id | zero |
//! | 65 | 32 | the code record's id | zero | zero |

use core::fmt;
use std::alloc::Allocator;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, Namespace, Tag};
use blaze_core::Driver;
use blaze_hash::ByHashSet;
use blaze_ids::ContentId;
use blaze_ladder::code_records::CodeKey;
use blaze_ladder::run::special::{EntryForm, EntryLine, StartHead};
use blaze_launch::wire::RecordId;
use blaze_launch::wire::data::Datum;
use blaze_launch::wire::run::Symbol;
use blaze_own::{AVec, Alloc, Vec};

/// The list's magic: the form's name; its version follows it ([`VERSION`]).
pub(crate) const MAGIC: [u8; 8] = *b"blzstart";

/// The form's version. A list of another is refused by name ([`ReadError::Old`] for an older
/// one, [`ReadError::Version`] for a newer): the run proceeds as with none, says so, and
/// writes today's form at exit. Version 4: a first answer is a group, its data and the statics
/// it names before its code row, so a record naming a static is a row too. Version 3 held no
/// record naming a static (so `println!`'s path was resolved again on every run, building the
/// front end); version 2 could hold a record naming a static an earlier answer told, which the
/// child then met before that static's `Data`.
pub(crate) const VERSION: u32 = 4;

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

/// A row's bytes: its kind, then three words (the module's second table).
pub(crate) const ROW: usize = 1 + 32 * 3;

/// A code row's kind byte.
const KIND_CODE: u8 = 0;
/// A datum row's kind byte.
const KIND_DATUM: u8 = 1;
/// A named row's kind byte.
const KIND_NAMED: u8 = 2;

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

    /// The name's key: what the program root's name is made from (store/roots.rs).
    pub(crate) fn id(&self) -> ContentId {
        self.0
    }
}

/// What `blaze run` reads and writes the list under: today's certificate and the list's name,
/// handed from main.rs's `Effect::StartProgram` site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Today {
    pub certificate: Certificate,
    pub name: ListName,
    /// The certificate's run tables (run_ids.rs `table_key`, run_names.rs `inputs_key`): read
    /// by the next run under this certificate, so named by the program root beside the list
    /// (store/roots.rs `keep_program`). An older certificate's tables are named by no root once
    /// the program root is renewed under a new one: dead, and swept.
    pub tables: [CasKey; 2],
}

/// One row: a function instance the parent answered, the code record's key it had then, and
/// the record's id (its file in the records folder).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Row {
    pub symbol: Symbol,
    pub key: CodeKey,
    pub record: RecordId,
}

/// One row of a list (the module's second table), in first-answer order: a first answer's
/// group is its `Datum` rows, then its `Named` rows, then its `Code` row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Line {
    /// A function instance the parent answered: its symbol, code key then, and record.
    Code(Row),
    /// A static the group's answer sent the child (one datum of its `Data`), by its symbol and
    /// its datum record (blaze-code datum.rs, a file of the records folder).
    Datum(Datum),
    /// A static the group's record or data name: on the child before its `Publish`.
    Named(Symbol),
}

impl Line {
    /// The row's symbol: the fn's or the static's.
    pub(crate) fn symbol(&self) -> Symbol {
        match self {
            Line::Code(row) => row.symbol,
            Line::Datum(datum) => datum.symbol,
            Line::Named(symbol) => *symbol,
        }
    }

    /// The code row, if this is one.
    pub(crate) fn code(&self) -> Option<Row> {
        match self {
            Line::Code(row) => Some(*row),
            Line::Datum(_) | Line::Named(_) => None,
        }
    }
}

impl From<Row> for Line {
    fn from(row: Row) -> Line {
        Line::Code(row)
    }
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
    rows: AVec<'a, Line>,
}

impl<'a> Rows<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Rows<'a> {
        Rows { rows: Vec::new_in(alloc) }
    }

    /// A code row with no data and naming nothing, after the others; past [`MOST_ROWS`]
    /// refused, naming it.
    #[cfg(test)]
    pub(crate) fn push(&mut self, row: Row) -> Result<(), CapError> {
        self.push_answer(&[], &[], row)
    }

    /// One first answer's group after the others (the module's doc): `data` (what its answer
    /// sent), `named` (ascending, each once), then `row`. All or nothing: a group that does not
    /// fit under [`MOST_ROWS`] is refused whole, naming `row`.
    pub(crate) fn push_answer(&mut self, data: &[Datum], named: &[Symbol], row: Row) -> Result<(), CapError> {
        if !self.fits(data.len() + named.len() + 1) {
            return Err(CapError::Full { symbol: row.symbol });
        }
        for datum in data {
            self.rows.push(Line::Datum(*datum));
        }
        for symbol in named {
            self.rows.push(Line::Named(*symbol));
        }
        self.rows.push(Line::Code(row));
        Ok(())
    }

    /// Whether `rows` more fit under [`MOST_ROWS`].
    pub(crate) fn fits(&self, rows: usize) -> bool {
        self.rows.len().saturating_add(rows) <= MOST_ROWS
    }

    #[cfg(test)]
    pub(crate) fn is_full(&self) -> bool {
        self.rows.len() >= MOST_ROWS
    }

    pub(crate) fn as_slice(&self) -> &[Line] {
        self.rows.as_slice()
    }
}

/// The list of `rows` (code rows alone, or whole [`Line`]s) under `certificate` with the run's
/// `head`, its bytes onto the end of `out` (the module's tables). Equal inputs, equal bytes.
/// More than [`MOST_ROWS`] rows is refused, naming the first past it.
pub(crate) fn encode<L: Copy + Into<Line>>(certificate: Certificate, head: StartHead, rows: &[L], out: &mut AVec<'_, u8>) -> Result<(), CapError> {
    let entry = head.line;
    if let Some(past) = rows.get(MOST_ROWS) {
        return Err(CapError::Full { symbol: Into::<Line>::into(*past).symbol() });
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
        let line: Line = (*row).into();
        let (kind, first, second) = match line {
            Line::Code(row) => (KIND_CODE, row.key.0.0, row.record.0),
            Line::Datum(datum) => (KIND_DATUM, datum.record.0, [0u8; 32]),
            Line::Named(_) => (KIND_NAMED, [0u8; 32], [0u8; 32]),
        };
        out.push(kind);
        out.extend_from_slice(&line.symbol().0);
        out.extend_from_slice(&first);
        out.extend_from_slice(&second);
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
    /// A start list of an older form (a version before [`VERSION`]): nothing of it is read,
    /// nothing is migrated, and this run's list replaces it at exit (rule 26).
    Old { found: u32 },
    /// A start list of a newer version than this blaze reads.
    Version { found: u32 },
    /// A row count past [`MOST_ROWS`].
    TooMany { rows: u32 },
    /// The length is not the head and the count's rows: truncated, or bytes past its end.
    Length { len: usize, want: usize },
    /// The entry's form word names no launcher form.
    Form { found: u32 },
    /// The personality's word is neither 0 nor 1, or is 0 with a symbol that is not all zero.
    Personality { found: u32 },
    /// One symbol in two code rows (the writer answers each symbol first once).
    Twice { symbol: Symbol },
    /// Row `row`'s kind byte is `found`: neither a code row (0), a datum row (1) nor a named
    /// row (2).
    Kind { row: u32, found: u8 },
    /// Row `row`'s words past its kind's are not all zero.
    Padding { row: u32 },
    /// Row `row` is out of its group's order: a datum after a named row, or a named row not
    /// above the one before it in its group.
    Order { row: u32 },
    /// One static's datum in two datum rows (each datum is sent once a run).
    DatumTwice { symbol: Symbol },
    /// The last `rows` rows are data or names with no code row after them.
    Dangling { rows: u32 },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = "the start list (run.md stage 7)";
        match self {
            ReadError::Oversized { len } => write!(f, "{what} is {len} bytes, past the longest list of {LONGEST}"),
            ReadError::Short { len } => write!(f, "{what} is {len} bytes, shorter than its {HEAD}-byte head"),
            ReadError::Magic { found } => write!(f, "{what} begins {found:?}, not its magic {MAGIC:?}"),
            ReadError::Old { found } => write!(f, "{what} is of version {found}, an older form: none of it is read, and this run writes version {VERSION} at exit (rule 26)"),
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
                f.write_str(" in two code rows")
            }
            ReadError::Kind { row, found } => write!(f, "{what}'s row {row} is of kind {found}: 0 (a code row), 1 (a datum row) or 2 (a named row) is read, nothing else"),
            ReadError::Padding { row } => write!(f, "{what}'s row {row} has bytes past its kind's words that are not zero"),
            ReadError::Order { row } => write!(f, "{what}'s row {row} is out of its group's order: data, then the statics named ascending, then the code row"),
            ReadError::DatumTwice { symbol } => {
                write!(f, "{what} holds the datum of static ")?;
                hex(f, &symbol.0)?;
                f.write_str(" in two rows: each datum is sent once a run")
            }
            ReadError::Dangling { rows } => write!(f, "{what} ends with {rows} data or named rows and no code row after them"),
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
    if version < VERSION {
        return Err(ReadError::Old { found: version });
    }
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
    // Each row's kind and padding, each group's order, before any row is decoded.
    let mut in_names = false;
    let mut last_named: Option<[u8; 32]> = None;
    let mut open = 0u32;
    for (at, row) in view.rows.chunks_exact(ROW).enumerate() {
        // At most MOST_ROWS rows (checked above): fits in 32 bits.
        let at = at as u32;
        let zero = |from: usize| row[from..].iter().all(|byte| *byte == 0);
        match row[0] {
            KIND_CODE => {
                in_names = false;
                last_named = None;
                open = 0;
            }
            KIND_DATUM => {
                if in_names {
                    return Err(ReadError::Order { row: at });
                }
                if !zero(65) {
                    return Err(ReadError::Padding { row: at });
                }
                open += 1;
            }
            KIND_NAMED => {
                if !zero(33) {
                    return Err(ReadError::Padding { row: at });
                }
                let mut symbol = [0u8; 32];
                symbol.copy_from_slice(&row[1..33]);
                if last_named.is_some_and(|last| last >= symbol) {
                    return Err(ReadError::Order { row: at });
                }
                in_names = true;
                last_named = Some(symbol);
                open += 1;
            }
            found => return Err(ReadError::Kind { row: at, found }),
        }
    }
    if open > 0 {
        return Err(ReadError::Dangling { rows: open });
    }
    let mut seen: ByHashSet<[u8; 32], Alloc<'_>> = ByHashSet::with_capacity_in(count as usize, alloc);
    let mut data: ByHashSet<[u8; 32], Alloc<'_>> = ByHashSet::with_capacity_in(count as usize, alloc);
    for line in view.lines() {
        match line {
            Line::Code(row) => {
                if !seen.insert(row.symbol.0) {
                    return Err(ReadError::Twice { symbol: row.symbol });
                }
            }
            Line::Datum(datum) => {
                if !data.insert(datum.symbol.0) {
                    return Err(ReadError::DatumTwice { symbol: datum.symbol });
                }
            }
            Line::Named(_) => {}
        }
    }
    Ok(view)
}

impl<'b> ListView<'b> {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rows.len() / ROW
    }

    /// The code rows in first-answer order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = Row> + Clone + 'b {
        self.lines().filter_map(|line| line.code())
    }

    /// Every row in first-answer order, each group whole ([`read`] checked each kind).
    pub(crate) fn lines(&self) -> impl Iterator<Item = Line> + Clone + 'b {
        self.rows.chunks_exact(ROW).map(|row| {
            let word = |at: usize| {
                let mut out = [0u8; 32];
                out.copy_from_slice(&row[at..at + 32]);
                out
            };
            match row[0] {
                KIND_CODE => Line::Code(Row { symbol: Symbol(word(1)), key: CodeKey(ContentId(word(33))), record: RecordId(word(65)) }),
                KIND_DATUM => Line::Datum(Datum { symbol: Symbol(word(1)), record: RecordId(word(33)) }),
                // KIND_NAMED: `read` refused every other kind.
                _ => Line::Named(Symbol(word(1))),
            }
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
    /// The code rows alone, in order.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = Row> + Clone + 'b {
        self.view.iter()
    }

    /// Every row, each group whole, in first-answer order: what a replay places.
    pub(crate) fn lines(&self) -> impl Iterator<Item = Line> + Clone + 'b {
        self.view.lines()
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
pub(crate) fn held<'m, D: Driver, A: Allocator + Clone>(lent: &'m Lent<D, A>, name: &ListName) -> Result<Option<&'m [u8]>, ListError> {
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
pub(crate) fn write<D: Driver, A: Allocator + Clone>(lent: &Lent<D, A>, name: &ListName, held: Option<&[u8]>, bytes: &[u8]) -> Result<Wrote, ListError> {
    if held == Some(bytes) {
        return Ok(Wrote::Nothing);
    }
    let content = CasKey(ContentId::of(RECORD_DOMAIN, bytes));
    lent.put(&content, Namespace::LOCAL, START_LIST_TAG, bytes).map_err(ListError::Store)?;
    lent.point(START_LIST_TAG, &CasKey(name.0), &content).map_err(ListError::Store)?;
    Ok(Wrote::Written)
}

/// The keys that keep the list `name` and the code it names (store/roots.rs `keep_program`),
/// pushed onto `out`: the list's pointer and record, and each code row's key and, when the key
/// is a pointer, its target (reach does not follow a pointer: blaze-cas `POINTER_TAG`). `bytes`
/// is the list just written ([`encode`]d by this run, so its form is known). Answers the rows
/// whose key has no target the store holds: named by key alone and counted, never dropped
/// quietly (rule 3); the next run builds them again. A row's record id is a file in the records
/// folder, not a store record: it is not the store's to keep.
pub(crate) fn rooted_keys<D: Driver, A: Allocator + Clone, B: Allocator>(lent: &Lent<D, A>, name: &ListName, bytes: &[u8], out: &mut std::vec::Vec<CasKey, B>) -> Result<u32, ListError> {
    out.push(CasKey(name.0));
    out.push(CasKey(ContentId::of(RECORD_DOMAIN, bytes)));
    let mut stale = 0u32;
    for row in bytes.get(HEAD..).unwrap_or(&[]).chunks_exact(ROW) {
        if row[0] != KIND_CODE {
            continue;
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&row[33..65]);
        let key = CasKey(ContentId(key));
        out.push(key);
        match lent.pointed(crate::code_store::CODE_TAG, &key) {
            Ok(Some(target)) => out.push(target),
            // An entry under its code key (the getter's and ender's): the key is the record.
            Err(LentError::PointerTag { .. }) => {}
            Ok(None) | Err(LentError::TargetGone { .. }) => stale += 1,
            Err(why) => return Err(ListError::Store(why)),
        }
    }
    Ok(stale)
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

/// Version 4's groups: data and names before their code row, each datum once, the refusals of
/// a malformed group, and an older form refused as such (start_list_statics_tests.rs).
#[cfg(test)]
#[path = "start_list_statics_tests.rs"]
mod statics_tests;
