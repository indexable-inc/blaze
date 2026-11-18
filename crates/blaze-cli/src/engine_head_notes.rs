//! THE REASON A HEAD WAS NOT FROZEN IS ALWAYS PRINTED (runtime.md A8: an error stops only what
//! reads it, and says why).
//!
//! A head task (engine_heads.rs) that could not freeze a head says so per head: which head
//! (`head`, its path as a finding shows it) and why (`why`: the freeze's reason in words that
//! hold no key, so equal reasons group). One note of class [`Class::Head`] a head in the task's
//! block of the lines record (engine_lines.rs), so a row that stands from the last answer, and
//! the certificate, print it again from the record. The command's closing lines group them:
//!
//! ```text
//! heads: 3 head(s) failed: <why>: first `a::f`, `a::g`, `b::S`
//! ```
//!
//! most frequent first, at most [`MOST_GROUPS`] reasons, then `heads: and K more reason(s)`.
//! A body refused because a head it reads is not frozen names that head and quotes the same
//! reason ([`Failed::body_words`]), whether the head task ran this request or stood.
//!
//! The words of a note: `<cell>\x1F<head>\x1F<why>`, `<cell>` the head's cell in its task as 32
//! hex digits, or `-` when the whole task failed (no head of it was frozen).

use core::fmt;

use blaze_drive::Name;
use blaze_drive::cells::Cell;
use blaze_ladder::front2::consts::ConstFail;
use blaze_ladder::front2::heads::HeadError;
use blaze_ladder::front2::tables::head::{ConstRefused, FreezeError};
use blaze_ladder::front2::types::Why;
use blaze_own::Alloc;
use bstd::Cx;
use bstd::mem::Frame;
use bstd::out::Out;
use std::alloc::Global;

use crate::engine_heads::HeadsStage;
use crate::engine_lines::{Class, Last, Lines, head_notes};
use crate::{CliError, print};

/// The separator of a note's three parts.
const SEP: char = '\u{1F}';
/// The most failed heads one request keeps (rule 18): past it the request is refused by name.
pub(crate) const MOST_FAILED: usize = 1 << 20;
/// The most reasons the closing lines print.
pub(crate) const MOST_GROUPS: usize = 20;
/// The heads a reason's line names.
const NAMED: usize = 3;

/// One head that could not be frozen, or a head task that failed whole (`cell: None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Refused {
    /// The head task.
    pub(crate) name: Name,
    /// The head's cell in that task; `None`: the task failed whole, no head of it frozen.
    pub(crate) cell: Option<Cell>,
    /// The head, as a finding shows its path (`a::m::S::f`), or what the task covers.
    pub(crate) head: &'static str,
    /// Why, in words holding no key (equal reasons are one group).
    pub(crate) why: &'static str,
}

/// Every head of this answer that could not be frozen: this request's (the Heads stage's
/// refusals) and those of the head tasks that stood (the last lines record's notes), sorted by
/// task and cell.
pub(crate) struct Failed {
    rows: std::vec::Vec<Refused, Global>,
    /// Made by [`Failed::of`] this request.
    pub(crate) ready: bool,
}

impl Failed {
    pub(crate) fn new() -> Failed {
        Failed { rows: std::vec::Vec::new_in(Global), ready: false }
    }

    /// This request's refusals and, for each head task this request did not enter, the head
    /// notes of its block in the last lines record (the row stood, so its block holds).
    pub(crate) fn of(heads: &HeadsStage, last: Option<&Last<'_, '_>>, g: Alloc<'static>) -> Result<Failed, CliError> {
        let full = CliError::Text { what: "more heads failed than a request keeps (engine_head_notes.rs `MOST_FAILED`)" };
        let mut rows: std::vec::Vec<Refused, Global> = std::vec::Vec::new_in(Global);
        if heads.refused.len() > MOST_FAILED {
            return Err(full);
        }
        rows.extend_from_slice(&heads.refused);
        rows.sort_unstable();
        let refused_now = &rows[..];
        let entered = |name: Name| heads.result(name).is_some() || refused_now.binary_search_by(|one| one.name.cmp(&name)).is_ok();
        let mut stood: std::vec::Vec<Refused, Global> = std::vec::Vec::new_in(Global);
        for (name, block) in last.into_iter().flat_map(|last| last.blocks()) {
            if !is_head_task(name) || entered(name) {
                continue;
            }
            let mut bad = None;
            head_notes(block, &mut |words| match parse(words) {
                Some((cell, head, why)) => stood.push(Refused { name, cell, head: leak(g, format_args!("{head}")), why: leak(g, format_args!("{why}")) }),
                None => {
                    bad.get_or_insert(());
                }
            })
            .map_err(|why| CliError::Text { what: why.what })?;
            if bad.is_some() {
                return Err(CliError::Text { what: "a head note of the last lines record is not `<cell>\\x1F<head>\\x1F<why>` (engine_head_notes.rs `parse`)" });
            }
            if rows.len() + stood.len() > MOST_FAILED {
                return Err(full);
            }
        }
        rows.extend_from_slice(&stood);
        rows.sort_unstable();
        rows.dedup_by_key(|one| (one.name, one.cell));
        Ok(Failed { rows, ready: true })
    }

    /// The failure of head task `task` that a reader of its cell `cell` meets: that head's own,
    /// or the task's whole failure.
    pub(crate) fn find(&self, task: Name, cell: Cell) -> Option<&Refused> {
        let at = |want: Option<Cell>| self.rows.binary_search_by(|one| (one.name, one.cell).cmp(&(task, want))).ok();
        at(Some(cell)).or_else(|| at(None)).map(|at| &self.rows[at])
    }

    /// Whether head `cell` of task `task` was not frozen (its own failure, or the task's).
    pub(crate) fn has(&self, task: Name, cell: Cell) -> bool {
        self.find(task, cell).is_some()
    }

    /// The words of a body refused because it reads head `cell` of task `task`, which was not
    /// frozen: the head named, its reason quoted.
    pub(crate) fn body_words(&self, task: Name, cell: Cell, into: &mut std::string::String) {
        match self.find(task, cell) {
            Some(one) => {
                into.push_str("the head of `");
                into.push_str(one.head);
                into.push_str("` it reads could not be frozen: ");
                into.push_str(one.why);
            }
            // A failed row with no reason kept is a bug of the Heads stage: said, never quiet.
            None => into.push_str("a head it reads could not be frozen, and its head task kept no reason (engine_head_notes.rs `Failed`)"),
        }
    }

    /// Head task `name`'s notes into `lines` (it was entered this request and has findings),
    /// one a head it could not freeze. A task the Heads stage never ran failed by the engine's
    /// rule, waiting on a task that failed: a [`Class::Waited`] note, as every stage's. One it
    /// ran that kept no reason is said as such (rule 3).
    pub(crate) fn note(&self, heads: &HeadsStage, name: Name, lines: &mut Lines<'_>, frame: &Frame) {
        let from = self.rows.partition_point(|one| one.name < name);
        let mine = self.rows[from..].iter().take_while(|one| one.name == name);
        let mut any = false;
        for one in mine {
            any = true;
            lines.note(name, Class::Head, words(*one, frame).as_bytes());
        }
        if any {
            return;
        }
        if heads.result(name).is_none() {
            lines.note(name, Class::Waited, b"");
            return;
        }
        let lost = Refused { name, cell: None, head: "?", why: "its head task has findings and the Heads stage kept no reason (engine_head_notes.rs `Failed::note`)" };
        lines.note(name, Class::Head, words(lost, frame).as_bytes());
    }
}

/// Whether a task of kind `name.kind` writes head notes: a head task, or a crate's impl tables.
pub(crate) fn is_head_task(name: Name) -> bool {
    matches!(name.kind, blaze_drive::Kind::Signature | blaze_drive::Kind::ImplBucket)
}

/// A note's words (module docs), the separator kept out of its parts.
fn words<'f>(one: Refused, frame: &'f Frame) -> blaze_own::text::String<&'f Frame> {
    let mut out = blaze_own::text::String::new_in(frame);
    match one.cell {
        Some(Cell(bytes)) => {
            for byte in bytes {
                out.push(hex(byte >> 4));
                out.push(hex(byte & 0xf));
            }
        }
        None => out.push('-'),
    }
    for part in [one.head, one.why] {
        out.push(SEP);
        // The separator kept out of a part: a space in its place.
        for ch in part.chars() {
            out.push(if ch == SEP { ' ' } else { ch });
        }
    }
    out
}

fn hex(nibble: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(nibble & 0xf)])
}

fn unhex(ch: u8) -> Option<u8> {
    match ch {
        b'0'..=b'9' => Some(ch - b'0'),
        b'a'..=b'f' => Some(ch - b'a' + 10),
        _ => None,
    }
}

/// A note's words read back: its cell, head and reason.
fn parse(words: &str) -> Option<(Option<Cell>, &str, &str)> {
    let mut parts = words.splitn(3, SEP);
    let (cell, head, why) = (parts.next()?, parts.next()?, parts.next()?);
    let cell = match cell.as_bytes() {
        b"-" => None,
        digits if digits.len() == 32 => {
            let mut bytes = [0u8; 16];
            for (at, pair) in digits.chunks_exact(2).enumerate() {
                bytes[at] = unhex(pair[0])? << 4 | unhex(pair[1])?;
            }
            Some(Cell(bytes))
        }
        _ => return None,
    };
    Some((cell, head, why))
}

/// The closing lines (module docs): every head note of `blocks`, grouped by reason.
pub(crate) fn print_grouped<'b>(cx: &Cx, blocks: impl Iterator<Item = (Name, &'b [u8])>, shown: &print::Shown<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    // (why, head) of every note.
    let mut all: std::vec::Vec<(&'b str, &'b str), &Frame> = std::vec::Vec::new_in(frame);
    for (name, block) in blocks {
        if !is_head_task(name) {
            continue;
        }
        let mut bad = false;
        head_notes(block, &mut |words| match parse(words) {
            Some((_, head, why)) => all.push((why, head)),
            None => bad = true,
        })
        .map_err(|why| CliError::Text { what: why.what })?;
        if bad {
            return Err(CliError::Text { what: "a head note is not `<cell>\\x1F<head>\\x1F<why>` (engine_head_notes.rs `parse`)" });
        }
        if all.len() > MOST_FAILED {
            return Err(CliError::Text { what: "more head notes than a request prints (engine_head_notes.rs `MOST_FAILED`)" });
        }
    }
    if all.is_empty() {
        return Ok(());
    }
    all.sort_unstable();
    // (count, why, its first heads by name).
    let mut groups: std::vec::Vec<(usize, &str, &[(&str, &str)]), &Frame> = std::vec::Vec::new_in(frame);
    for run in all.chunk_by(|a, b| a.0 == b.0) {
        groups.push((run.len(), run[0].0, run));
    }
    groups.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    for (count, why, run) in groups.iter().take(MOST_GROUPS) {
        let mut line = bstd::mem::format_in!(frame, "heads: {count} head(s) failed: {why}: first ");
        // The run is sorted by head: a head of two cells (one path) is named once.
        let mut last: Option<&str> = None;
        let mut named = 0;
        for (_, head) in run.iter() {
            if last == Some(*head) {
                continue;
            }
            if named == NAMED {
                break;
            }
            if named > 0 {
                line.push_str(", ");
            }
            line.push('`');
            line.push_str(head);
            line.push('`');
            last = Some(*head);
            named += 1;
        }
        print::note(cx, line.as_str(), shown, frame, out)?;
    }
    if groups.len() > MOST_GROUPS {
        print::note(cx, bstd::mem::format_in!(frame, "heads: and {} more reason(s)", groups.len() - MOST_GROUPS).as_str(), shown, frame, out)?;
    }
    Ok(())
}

/// `args` as text kept for the request (in the request's heap `g`).
pub(crate) fn leak(g: Alloc<'static>, args: fmt::Arguments<'_>) -> &'static str {
    blaze_own::Box::leak(blaze_own::text::format(g, args).into_boxed_str())
}

/// Why a freeze failed, in words that hold no key: two heads refused for one reason print one
/// reason (the closing lines group by it). The freeze's own `Display` names the want's key.
pub(crate) struct Reason<'e>(pub(crate) &'e FreezeError);

impl fmt::Display for Reason<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            FreezeError::NotACell { .. } => write!(f, "its want is no item's whole want (front2 tables/head.rs `NotACell`)"),
            FreezeError::Fill { error, .. } => write!(f, "no want makes a head it needs ({})", Variant(error)),
            FreezeError::NoWant { error, .. } => write!(f, "no want makes a head cell it reads ({})", Variant(error)),
            FreezeError::Head { error, .. } => head_words(error, f),
            FreezeError::Const { error, .. } => {
                write!(f, "the value of a constant its type holds: ")?;
                match error {
                    ConstRefused::NoEvaluator => write!(f, "the freeze has no constant evaluator"),
                    ConstRefused::Eval(fail) => const_words(fail, f),
                    ConstRefused::NotFiled => write!(f, "evaluated, and asked for again: its value was not filed"),
                    ConstRefused::TooMany { most } => write!(f, "the head asks for more than {most} constants"),
                    ConstRefused::TypeNeedsConst { .. } => write!(f, "its own type needs another constant's value first"),
                }
            }
            FreezeError::Record { error, .. } => write!(f, "its head record could not be written ({})", Variant(error)),
            FreezeError::TooBig { what, .. } => write!(f, "{what} past what the form holds"),
            FreezeError::Read { error, .. } => write!(f, "a frozen head did not read back ({})", Variant(error)),
            FreezeError::NoText { .. } => write!(f, "the names library has no text of its item"),
            FreezeError::Unfrozen { .. } => write!(f, "a cell was asked inside another cell's freeze before it was frozen"),
            FreezeError::TooDeep { .. } => write!(f, "a chain of cells, each needing the next, deeper than a freeze follows"),
            FreezeError::Steps { most, .. } => write!(f, "more than {most} freeze attempts"),
            FreezeError::Again { .. } => write!(f, "a head it reads failed when first frozen"),
        }
    }
}

fn head_words(error: &HeadError, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match error {
        HeadError::NoText(_) => write!(f, "the names library holds no text of an item it needs"),
        HeadError::Cfg(_) => write!(f, "an item asks for cfg atoms its crate's set does not answer"),
        HeadError::NotAHead(_, what) => write!(f, "an item it needs is {what}, not a struct, enum, union, trait, impl or fn"),
        HeadError::Cycle(keys) => write!(f, "{} items need each other's heads in a cycle", keys.len()),
        HeadError::NeedMade { .. } => write!(f, "a head was asked again after it was made (its lookup reads what that want does not make)"),
        HeadError::Key(_) => write!(f, "two origins under one checker key"),
        HeadError::Item(_, why) => why_words(why, f),
        HeadError::Seed { .. } => write!(f, "a head it needs could not be seeded from its frozen record"),
        HeadError::Filed { what, .. } => write!(f, "a head of its record could not be filed: {what}"),
    }
}

fn why_words(why: &Why, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match why {
        Why::Need(_) => write!(f, "a head it needs was not made"),
        Why::Unsupported(what, _) => write!(f, "{what}"),
        Why::Program(what, _) => write!(f, "{what}"),
        Why::Resolve(error, _) => write!(f, "a path did not resolve ({})", Variant(error)),
        Why::NoCheckerType(what, _) => write!(f, "the checker has no type for `{what}`"),
        Why::AssocNotFound(what, _) => write!(f, "no bound declares `{what}`"),
        Why::AssocAmbiguous(what, _, _) => write!(f, "more than one bound declares `{what}`"),
        Why::TwoPrincipals(..) => write!(f, "a `dyn` type with two non-auto traits"),
        Why::TypeMacro(what, _) => write!(f, "a macro call in type position: `{what}`"),
        Why::PatternType(what, _) => write!(f, "a pattern type refused: {what}"),
        Why::ConstPath(what, _) => write!(f, "a constant expression in a type: `{what}`"),
        Why::NeedConst(..) => write!(f, "a constant its type names was not evaluated"),
    }
}

fn const_words(fail: &ConstFail, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match fail {
        ConstFail::Body { .. } => write!(f, "a body its value needs could not be lowered"),
        ConstFail::Spelling { why, .. } => why_words(why, f),
        ConstFail::Check { .. } => write!(f, "a body its value needs did not check"),
        ConstFail::Diag { code, .. } => write!(f, "a body its value needs has error {code}"),
        ConstFail::Eval(what) => write!(f, "the evaluator refused: {what}"),
        ConstFail::NotConstFn { shown, .. } => write!(f, "it calls `{shown}`, which is no const fn"),
        other => write!(f, "the evaluator refused ({})", Variant(other)),
    }
}

/// A value's variant name alone (its `Debug` up to its fields): no key, no offset.
struct Variant<'v, T>(&'v T);

impl<T: fmt::Debug> fmt::Display for Variant<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut name = VariantName { out: f, done: false };
        fmt::write(&mut name, format_args!("{:?}", self.0))
    }
}

/// A writer that passes on the first identifier it is given and drops the rest.
struct VariantName<'w, 'f> {
    out: &'w mut fmt::Formatter<'f>,
    done: bool,
}

impl fmt::Write for VariantName<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.done {
            return Ok(());
        }
        let end = text.find(|ch: char| !(ch.is_alphanumeric() || ch == '_')).unwrap_or(text.len());
        if end < text.len() {
            self.done = true;
        }
        self.out.write_str(&text[..end])
    }
}
