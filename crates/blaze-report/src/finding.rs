//! One finding record (D164, docs/v2/plan/design/end-state.md): a rule of the one table
//! ([`crate::frame::words::WORDINGS`]), a [`Subject`] a person can act on, an item-relative
//! range where there is one, typed arguments, and for what was not checked or refused a typed
//! [`Cause`], from which the [`CauseKey`] the views group by is derived. A note is a finding
//! whose rule's [`Kind`] is a note's; its words are the table's, filled here and nowhere else.
//! Every view (the default one, `--each`, `blaze why`, a machine's) is a query over findings
//! ([`crate::query`]).
//!
//! The stored form ([`encode`], [`decode`]) is a subject and a cause, the rule being the
//! cause's: what a body's note is in blaze-cli's lines record (engine_lines.rs `NOTED`).
//!
//! ```text
//! noted    subject | cause
//! subject  tag u8 (0 file, 1 item, 2 crate) | text u32 | text (an item: then its item, u32 | text)
//! cause    tag u8 (CauseKind) | its fields in order: a text u32 | text, a count u64 (LE)
//! ```

use core::fmt;

use crate::frame::words::{self, Area, Kind, Wording, WordsError};

pub use crate::frame::words::Arg;

/// What a finding is about: always a thing a person can act on (rule 19), never a task's name.
/// A lookup that finds no subject is a typed error where it is made ([`Unnamed`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Subject<'a> {
    /// A file as a whole: its checkout path.
    File { path: &'a str },
    /// An item of a file: the file's checkout path, and the item as written (or its place in
    /// its file's outline when it has no name).
    Item { file: &'a str, item: &'a str },
    /// A crate, by its name.
    Crate { name: &'a str },
}

/// A subject a lookup did not find: the site's typed error, never a task's debug form printed
/// in its place (D164).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unnamed {
    /// What was looked up, and where (rule 19).
    pub what: &'static str,
}

impl fmt::Display for Unnamed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a finding's subject was not found: {} (blaze_report::finding, D164)", self.what)
    }
}

impl core::error::Error for Unnamed {}

impl<'a> Subject<'a> {
    /// The subject as its rule's first arguments: a file `{0}`; an item `{0}` its file and `{1}`
    /// the item; a crate `{0}`. How many are used is the second value.
    fn args(&self) -> ([Arg<'a>; 2], usize) {
        match *self {
            Subject::File { path } => ([Arg::Text(path), Arg::Text("")], 1),
            Subject::Item { file, item } => ([Arg::Text(file), Arg::Text(item)], 2),
            Subject::Crate { name } => ([Arg::Text(name), Arg::Text("")], 1),
        }
    }

    /// The same subject with each text given by `keep` (a copy into another arena).
    pub fn map<'b>(self, keep: &mut impl FnMut(&str) -> &'b str) -> Subject<'b> {
        match self {
            Subject::File { path } => Subject::File { path: keep(path) },
            Subject::Item { file, item } => Subject::Item { file: keep(file), item: keep(item) },
            Subject::Crate { name } => Subject::Crate { name: keep(name) },
        }
    }

    /// The subject as a group's line names it ([`crate::query::Group::line`]): "`item` (file)",
    /// "`path`" or "crate `name`".
    pub fn named(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        match *self {
            Subject::File { path } => write!(out, "`{path}`"),
            Subject::Item { file, item } => write!(out, "`{item}` ({file})"),
            Subject::Crate { name } => write!(out, "crate `{name}`"),
        }
    }

    /// The noun a count of subjects of this sort takes ("1 body", "4 bodies").
    pub fn noun(&self, count: usize) -> &'static str {
        match (self, count) {
            (Subject::File { .. }, 1) => "file",
            (Subject::File { .. }, _) => "files",
            (Subject::Item { .. }, 1) => "body",
            (Subject::Item { .. }, _) => "bodies",
            (Subject::Crate { .. }, 1) => "crate",
            (Subject::Crate { .. }, _) => "crates",
        }
    }
}

/// Why a body was not checked, or not checked whole: the closed set, each with its arguments.
/// Its rule ([`CauseKind::rule`]) words it. A text argument that is still words is the words of
/// the code that refused (the checker's verdict lines, the plan's records), carried as they are
/// until that code files a cause of its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause<'a> {
    /// The plan or the view refused the body's task (front2 task.rs `TaskError`): its words.
    Task { error: &'a str },
    /// The body's check panicked: this body alone failed (rule 20).
    Panicked,
    /// The shared stretch of `reserve` bytes filled while checking the body: owed to the next
    /// request.
    StretchFilled { reserve: u64 },
    /// The body used up its frame of `reserve` bytes, for `what`.
    FrameUsedUp { what: &'a str, reserve: u64 },
    /// This stage could not give the body a check: why.
    Refused { why: &'a str },
    /// The plan refused the body by its file's, member's or unit's records (blaze-cli
    /// engine_body_plan.rs `Decision::Refused`): why, the same words for every body it refuses.
    PlanRefused { why: &'a str },
    /// The checker's verdict failed and said no cause.
    Silent,
    /// The checker's verdict left the body not checked whole: its own causes' words.
    NotWhole { why: &'a str },
    /// The body reads a name a refused macro call may have made: the call's failure.
    RefusedCall { call: &'a str },
    /// Types checked whole; the move and borrow check refused: why.
    BorrowsRefused { why: &'a str },
}

/// A [`Cause`]'s variant, as stored and as its rule.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum CauseKind {
    Task = 0,
    Panicked = 1,
    StretchFilled = 2,
    FrameUsedUp = 3,
    Refused = 4,
    PlanRefused = 5,
    Silent = 6,
    NotWhole = 7,
    RefusedCall = 8,
    BorrowsRefused = 9,
}

impl CauseKind {
    /// Every kind, in tag order.
    pub const ALL: [CauseKind; 10] = [
        CauseKind::Task,
        CauseKind::Panicked,
        CauseKind::StretchFilled,
        CauseKind::FrameUsedUp,
        CauseKind::Refused,
        CauseKind::PlanRefused,
        CauseKind::Silent,
        CauseKind::NotWhole,
        CauseKind::RefusedCall,
        CauseKind::BorrowsRefused,
    ];

    /// The rule (of [`Area::Cli`]) whose words say a finding of this cause.
    pub const fn rule(self) -> u16 {
        match self {
            CauseKind::Task => 35,
            CauseKind::Panicked => 36,
            CauseKind::StretchFilled => 37,
            CauseKind::FrameUsedUp => 38,
            CauseKind::Refused => 39,
            CauseKind::PlanRefused => 40,
            CauseKind::Silent => 41,
            CauseKind::NotWhole => 42,
            CauseKind::RefusedCall => 43,
            CauseKind::BorrowsRefused => 44,
        }
    }

    fn of_byte(byte: u8) -> Option<CauseKind> {
        CauseKind::ALL.into_iter().find(|kind| *kind as u8 == byte)
    }
}

/// What findings are grouped by: the cause's words first (so groups come in the order of their
/// words, rule 25), then its kind. Two findings of one key are one line of the default view.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CauseKey<'a> {
    /// The cause's first text argument ("" for a cause of none): what its group's line says.
    pub of: &'a str,
    pub kind: CauseKind,
}

impl<'a> Cause<'a> {
    pub fn kind(&self) -> CauseKind {
        match self {
            Cause::Task { .. } => CauseKind::Task,
            Cause::Panicked => CauseKind::Panicked,
            Cause::StretchFilled { .. } => CauseKind::StretchFilled,
            Cause::FrameUsedUp { .. } => CauseKind::FrameUsedUp,
            Cause::Refused { .. } => CauseKind::Refused,
            Cause::PlanRefused { .. } => CauseKind::PlanRefused,
            Cause::Silent => CauseKind::Silent,
            Cause::NotWhole { .. } => CauseKind::NotWhole,
            Cause::RefusedCall { .. } => CauseKind::RefusedCall,
            Cause::BorrowsRefused { .. } => CauseKind::BorrowsRefused,
        }
    }

    /// Its key: derived, never stored.
    pub fn key(&self) -> CauseKey<'a> {
        let of = match *self {
            Cause::Task { error } => error,
            Cause::FrameUsedUp { what, .. } => what,
            Cause::Refused { why } | Cause::PlanRefused { why } | Cause::NotWhole { why } | Cause::BorrowsRefused { why } => why,
            Cause::RefusedCall { call } => call,
            Cause::Panicked | Cause::StretchFilled { .. } | Cause::Silent => "",
        };
        CauseKey { of, kind: self.kind() }
    }

    /// The arguments its rule's words take after the subject's, in the table's order.
    fn args(&self) -> ([Arg<'a>; 2], usize) {
        match *self {
            Cause::Task { error } => ([Arg::Text(error), Arg::Count(0)], 1),
            Cause::Panicked | Cause::Silent => ([Arg::Count(0), Arg::Count(0)], 0),
            Cause::StretchFilled { reserve } => ([Arg::Count(reserve), Arg::Count(0)], 1),
            Cause::FrameUsedUp { what, reserve } => ([Arg::Text(what), Arg::Count(reserve)], 2),
            Cause::Refused { why } | Cause::PlanRefused { why } | Cause::NotWhole { why } | Cause::BorrowsRefused { why } => ([Arg::Text(why), Arg::Count(0)], 1),
            Cause::RefusedCall { call } => ([Arg::Text(call), Arg::Count(0)], 1),
        }
    }

    /// The same cause with each text given by `keep` (a copy into another arena).
    pub fn map<'b>(self, keep: &mut impl FnMut(&str) -> &'b str) -> Cause<'b> {
        match self {
            Cause::Task { error } => Cause::Task { error: keep(error) },
            Cause::Panicked => Cause::Panicked,
            Cause::StretchFilled { reserve } => Cause::StretchFilled { reserve },
            Cause::FrameUsedUp { what, reserve } => Cause::FrameUsedUp { what: keep(what), reserve },
            Cause::Refused { why } => Cause::Refused { why: keep(why) },
            Cause::PlanRefused { why } => Cause::PlanRefused { why: keep(why) },
            Cause::Silent => Cause::Silent,
            Cause::NotWhole { why } => Cause::NotWhole { why: keep(why) },
            Cause::RefusedCall { call } => Cause::RefusedCall { call: keep(call) },
            Cause::BorrowsRefused { why } => Cause::BorrowsRefused { why: keep(why) },
        }
    }
}

/// Where in its item a finding is: `start..end` from the item's start (rule 71), the item the
/// one whose chain is `chain` in its file's outline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct At {
    pub chain: u64,
    pub start: u32,
    pub end: u32,
}

/// One finding (module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Finding<'a> {
    pub area: Area,
    pub rule: u16,
    pub subject: Subject<'a>,
    /// `None`: about its subject as a whole.
    pub at: Option<At>,
    /// Its own arguments, after its subject's and its cause's.
    pub args: &'a [Arg<'a>],
    pub cause: Option<Cause<'a>>,
}

/// Why a finding's words could not be said.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SayError {
    /// Its rule has no row, or its arguments are not the row's count.
    Words(WordsError),
    /// The writer refused.
    Write,
}

impl fmt::Display for SayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SayError::Words(why) => fmt::Display::fmt(why, f),
            SayError::Write => f.write_str("blaze_report::finding: the writer refused a finding's words"),
        }
    }
}

impl core::error::Error for SayError {}

/// The most arguments one finding's words take: its subject's, its cause's and its own.
const MOST_ARGS: usize = 8;

impl<'a> Finding<'a> {
    /// A note of `subject` for `cause`: its rule is the cause's, about the subject whole.
    pub fn of_cause(subject: Subject<'a>, cause: Cause<'a>) -> Finding<'a> {
        Finding { area: Area::Cli, rule: cause.kind().rule(), subject, at: None, args: &[], cause: Some(cause) }
    }

    /// Its rule's row.
    pub fn wording(&self) -> Result<&'static Wording, WordsError> {
        words::wording(self.area, self.rule).ok_or(WordsError::NoWording { area: self.area, number: self.rule })
    }

    /// What it is to the command: its rule's [`Kind`] column.
    pub fn kind(&self) -> Result<Kind, WordsError> {
        self.wording().map(|row| row.kind)
    }

    /// Its cause's key, when it has a cause.
    pub fn key(&self) -> Option<CauseKey<'a>> {
        self.cause.map(|cause| cause.key())
    }

    /// Its words, as `--each` draws it: its rule's headline filled from its subject's, its
    /// cause's and its own arguments, in that order.
    pub fn words(&self, out: &mut dyn fmt::Write) -> Result<(), SayError> {
        let row = self.wording().map_err(SayError::Words)?;
        let mut args = [Arg::Count(0); MOST_ARGS];
        let mut n = 0;
        let (subject, given) = self.subject.args();
        let (cause, caused) = self.cause.map_or(([Arg::Count(0), Arg::Count(0)], 0), |cause| cause.args());
        for arg in subject[..given].iter().chain(cause[..caused].iter()).chain(self.args.iter()) {
            if n == MOST_ARGS {
                return Err(SayError::Words(WordsError::Arguments { name: row.name, given: MOST_ARGS + 1, takes: row.args }));
            }
            args[n] = *arg;
            n += 1;
        }
        if n != usize::from(row.args) {
            return Err(SayError::Words(WordsError::Arguments { name: row.name, given: n, takes: row.args }));
        }
        words::fill_args(out, row.headline, &args[..n]).map_err(|_| SayError::Write)
    }

    /// The same finding with each text given by `keep`; a finding of its own arguments keeps
    /// none of them (only a note of a cause is copied so: [`Finding::of_cause`]).
    pub fn map<'b>(self, keep: &mut impl FnMut(&str) -> &'b str) -> Result<Finding<'b>, Unnamed> {
        if !self.args.is_empty() {
            return Err(Unnamed { what: "a finding with arguments of its own is copied only with them (blaze_report::finding `map`)" });
        }
        Ok(Finding { area: self.area, rule: self.rule, subject: self.subject.map(keep), at: self.at, args: &[], cause: self.cause.map(|cause| cause.map(keep)) })
    }
}

/// Why a stored finding was refused, at which byte of its bytes (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodeError {
    pub at: usize,
    pub what: &'static str,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a stored finding at byte {}: {} (blaze_report::finding `decode`)", self.at, self.what)
    }
}

impl core::error::Error for DecodeError {}

const FILE: u8 = 0;
const ITEM: u8 = 1;
const CRATE: u8 = 2;

fn text(put: &mut dyn FnMut(&[u8]), text: &str) {
    put(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_le_bytes());
    put(text.as_bytes());
}

/// `subject` noted for `cause` as stored (module docs), each piece handed to `put` in order.
pub fn encode(subject: Subject<'_>, cause: Cause<'_>, put: &mut dyn FnMut(&[u8])) {
    match subject {
        Subject::File { path } => {
            put(&[FILE]);
            text(put, path);
        }
        Subject::Item { file, item } => {
            put(&[ITEM]);
            text(put, file);
            text(put, item);
        }
        Subject::Crate { name } => {
            put(&[CRATE]);
            text(put, name);
        }
    }
    put(&[cause.kind() as u8]);
    match cause {
        Cause::Task { error: words } | Cause::Refused { why: words } | Cause::PlanRefused { why: words } | Cause::NotWhole { why: words } | Cause::BorrowsRefused { why: words } | Cause::RefusedCall { call: words } => text(put, words),
        Cause::Panicked | Cause::Silent => {}
        Cause::StretchFilled { reserve } => put(&reserve.to_le_bytes()),
        Cause::FrameUsedUp { what, reserve } => {
            text(put, what);
            put(&reserve.to_le_bytes());
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], DecodeError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(DecodeError { at: self.at, what })?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self, what: &'static str) -> Result<u8, DecodeError> {
        Ok(self.take(1, what)?[0])
    }

    fn count(&mut self, what: &'static str) -> Result<u64, DecodeError> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8, what)?);
        Ok(u64::from_le_bytes(b))
    }

    fn text(&mut self, what: &'static str) -> Result<&'a str, DecodeError> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4, what)?);
        let at = self.at;
        let bytes = self.take(u32::from_le_bytes(b) as usize, what)?;
        core::str::from_utf8(bytes).map_err(|_| DecodeError { at, what: "a text of a stored finding is not UTF-8" })
    }
}

/// A stored finding's bytes read whole (module docs): every text checked UTF-8, and no byte
/// after its cause.
pub fn decode(bytes: &[u8]) -> Result<Finding<'_>, DecodeError> {
    let mut reader = Reader { bytes, at: 0 };
    let subject = match reader.byte("a stored finding's subject tag")? {
        FILE => Subject::File { path: reader.text("a file subject's path")? },
        ITEM => {
            let file = reader.text("an item subject's file")?;
            Subject::Item { file, item: reader.text("an item subject's item")? }
        }
        CRATE => Subject::Crate { name: reader.text("a crate subject's name")? },
        _ => return Err(DecodeError { at: 0, what: "a stored finding's subject tag is none of file, item and crate" }),
    };
    let at = reader.at;
    let kind = CauseKind::of_byte(reader.byte("a stored finding's cause tag")?).ok_or(DecodeError { at, what: "a stored finding's cause tag is no CauseKind" })?;
    let cause = match kind {
        CauseKind::Task => Cause::Task { error: reader.text("a task fault's words")? },
        CauseKind::Panicked => Cause::Panicked,
        CauseKind::StretchFilled => Cause::StretchFilled { reserve: reader.count("a filled stretch's bytes")? },
        CauseKind::FrameUsedUp => {
            let what = reader.text("a used-up frame's use")?;
            Cause::FrameUsedUp { what, reserve: reader.count("a used-up frame's bytes")? }
        }
        CauseKind::Refused => Cause::Refused { why: reader.text("a refusal's words")? },
        CauseKind::PlanRefused => Cause::PlanRefused { why: reader.text("a plan refusal's words")? },
        CauseKind::Silent => Cause::Silent,
        CauseKind::NotWhole => Cause::NotWhole { why: reader.text("a verdict's causes")? },
        CauseKind::RefusedCall => Cause::RefusedCall { call: reader.text("a refused call's failure")? },
        CauseKind::BorrowsRefused => Cause::BorrowsRefused { why: reader.text("a borrow check's refusal")? },
    };
    if reader.at != bytes.len() {
        return Err(DecodeError { at: reader.at, what: "bytes follow a stored finding's cause" });
    }
    Ok(Finding::of_cause(subject, cause))
}

#[cfg(test)]
#[path = "finding_tests.rs"]
mod tests;
