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
//! hex digits, or `-` when the whole task failed. A constant or engine root prefixes that
//! field with `const/` or `engine/`, so a body can classify a standing head's refusal.
//! When the body's words add a head or constant path, a fourth field keeps the original
//! reason for grouping across heads.

use core::fmt;

use blaze_drive::Name;
use blaze_drive::cells::Cell;
use blaze_ladder::front2::consts::ConstFail;
use blaze_ladder::front2::heads::{HeadError, Want};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{CellFailure, ConstRefused, FreezeError, HeadDone, MOST_MAKING, NotTheirs, SeededHeads, want_bytes};
use blaze_ladder::front2::types::Why;
use blaze_own::Alloc;
use bstd::Cx;
use bstd::mem::Frame;
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
    pub(crate) cause: Cause,
}

impl Refused {
    pub(crate) fn whole(name: Name, head: &'static str, why: &'static str) -> Self {
        Self { name, cell: None, head, cause: Cause::new(RootKind::Head, why) }
    }
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
                Some((cell, kind, head, words, group)) => {
                    let words = leak(g, format_args!("{words}"));
                    let group = group.map_or(words, |why| leak(g, format_args!("{why}")));
                    stood.push(Refused { name, cell, head: leak(g, format_args!("{head}")), cause: Cause { kind, words, group } });
                }
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
    pub(crate) fn body_words<A: core::alloc::Allocator>(&self, task: Name, cell: Cell, into: &mut blaze_own::String<A>) {
        match self.find(task, cell) {
            Some(one) => {
                into.push_str("the head of `");
                into.push_str(one.head);
                into.push_str("` it reads could not be frozen: ");
                into.push_str(one.cause.words);
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
        let lost = Refused { name, cell: None, head: "?", cause: Cause::new(RootKind::Engine, "its head task has findings and the Heads stage kept no reason (engine_head_notes.rs `Failed::note`)") };
        lines.note(name, Class::Head, words(lost, frame).as_bytes());
    }
}

/// Whether a task of kind `name.kind` writes head notes: a head task, or a crate's impl tables.
pub(crate) fn is_head_task(name: Name) -> bool {
    matches!(name.kind, blaze_drive::Kind::Signature | blaze_drive::Kind::ImplBucket)
}

/// The words of the [`Class::Head`] note a body's block holds for a head its seed could not
/// freeze and no head task keeps (engine_bodies.rs `Ran::head_failed`): no cell (`-`), the head
/// and its own reason, so [`print_grouped`] groups it with the head tasks'.
pub(crate) fn seed_words<'f>(head: &'static str, why: &'static str, frame: &'f Frame) -> blaze_own::text::String<&'f Frame> {
    let mut out = blaze_own::text::String::new_in(frame);
    out.push('-');
    push_parts(&mut out, head, why);
    out
}

/// A note's head and reason after its cell, each after the separator, kept out of them.
fn push_parts(out: &mut blaze_own::text::String<&Frame>, head: &str, why: &str) {
    for part in [head, why] {
        push_part(out, part);
    }
}

fn push_part(out: &mut blaze_own::text::String<&Frame>, part: &str) {
    out.push(SEP);
    for ch in part.chars() {
        out.push(if ch == SEP { ' ' } else { ch });
    }
}

/// Whether `name`'s block may hold [`Class::Head`] notes the closing lines group: a head
/// task's, or a body's for a head its seed could not freeze ([`seed_words`]).
fn holds_head_notes(name: Name) -> bool {
    is_head_task(name) || name.kind == blaze_drive::Kind::BodyCheck
}

/// A note's words (module docs), the separator kept out of its parts.
fn words<'f>(one: Refused, frame: &'f Frame) -> blaze_own::text::String<&'f Frame> {
    let mut out = blaze_own::text::String::new_in(frame);
    out.push_str(one.cause.kind.tag());
    match one.cell {
        Some(Cell(bytes)) => {
            for byte in bytes {
                out.push(hex(byte >> 4));
                out.push(hex(byte & 0xf));
            }
        }
        None => out.push('-'),
    }
    push_parts(&mut out, one.head, one.cause.words);
    if one.cause.group != one.cause.words {
        push_part(&mut out, one.cause.group);
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
fn parse(words: &str) -> Option<(Option<Cell>, RootKind, &str, &str, Option<&str>)> {
    let mut parts = words.split(SEP);
    let (cell, head, why) = (parts.next()?, parts.next()?, parts.next()?);
    let (kind, cell) = [RootKind::Constant, RootKind::Engine, RootKind::Head]
        .into_iter()
        .find_map(|kind| cell.strip_prefix(kind.tag()).map(|cell| (kind, cell)))?;
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
    let group = parts.next();
    parts.next().is_none().then_some((cell, kind, head, why, group))
}

/// The closing lines (module docs): every head note of `blocks`, grouped by reason.
pub(crate) fn print_grouped<'b>(cx: &Cx, blocks: impl Iterator<Item = (Name, &'b [u8])>, shown: &print::Shown<'_>, frame: &Frame, out: &impl print::RenderOutput) -> Result<(), CliError> {
    // (why, head) of every note.
    let mut all: std::vec::Vec<(&'b str, &'b str), &Frame> = std::vec::Vec::new_in(frame);
    for (name, block) in blocks {
        if !holds_head_notes(name) {
            continue;
        }
        let mut bad = false;
        head_notes(block, &mut |words| match parse(words) {
            Some((_, _, head, why, group)) => all.push((group.unwrap_or(why), head)),
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
    // A head of two units (its lib and its test harness, engine_heads.rs `head_cell`) has a
    // note a unit's cell, its path and reason the same: one failed head, counted once.
    all.dedup();
    // (count, why, its first heads by name).
    let mut groups: std::vec::Vec<(usize, &str, &[(&str, &str)]), &Frame> = std::vec::Vec::new_in(frame);
    for run in all.chunk_by(|a, b| a.0 == b.0) {
        groups.push((run.len(), run[0].0, run));
    }
    groups.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    for (count, why, run) in groups.iter().take(MOST_GROUPS) {
        let first = &run[..NAMED.min(run.len())];
        print::note_from(cx, shown, frame, out, || {
            let mut h = blaze_ids::Hasher::new("blaze-cli rendered head group 1");
            h.update(&(*count as u64).to_le_bytes()).part(why.as_bytes());
            for (_, head) in first {
                h.part(head.as_bytes());
            }
            h.finish()
        }, || {
            let mut line = bstd::mem::format_in!(frame, "heads: {count} head(s) failed: {why}: first ");
            for (i, (_, head)) in first.iter().enumerate() {
                if i != 0 {
                    line.push_str(", ");
                }
                line.push('`');
                line.push_str(head);
                line.push('`');
            }
            Ok(line)
        })?;
    }
    if groups.len() > MOST_GROUPS {
        print::note(cx, bstd::mem::format_in!(frame, "heads: and {} more reason(s)", groups.len() - MOST_GROUPS).as_str(), shown, frame, out)?;
    }
    Ok(())
}

/// `args` as text kept for the request (in the request's heap `g`).
pub(crate) fn leak(g: Alloc<'static>, args: fmt::Arguments<'_>) -> &'static str {
    blaze_own::Box::leak_static(blaze_own::text::format(g, args).into_boxed_str())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RootKind {
    Head,
    Constant,
    Engine,
}

impl RootKind {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Head => "",
            Self::Constant => "const/",
            Self::Engine => "engine/",
        }
    }

    pub(crate) fn of(error: &FreezeError) -> Self {
        match error {
            FreezeError::Const { .. } => Self::Constant,
            FreezeError::NotACell { .. } | FreezeError::Read { .. } | FreezeError::Unfrozen { .. }
            | FreezeError::NotTheirs { why: NotTheirs::NoCell { .. } | NotTheirs::OtherUnits { .. }, .. } => Self::Engine,
            _ => Self::Head,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Cause {
    pub(crate) words: &'static str,
    pub(crate) kind: RootKind,
    /// The terminal reason alone, as the closing lines group it.
    pub(crate) group: &'static str,
}

impl Cause {
    pub(crate) fn new(kind: RootKind, words: &'static str) -> Self {
        Self { words, kind, group: words }
    }

    pub(crate) fn own<'n>(error: &FreezeError, records: &'n Records<'n>, g: Alloc<'static>) -> Self {
        Self::from_reason(error, leak(g, format_args!("{}", Reason(error))), records, g)
    }

    pub(crate) fn from_reason<'n>(error: &FreezeError, why: &'static str, records: &'n Records<'n>, g: Alloc<'static>) -> Self {
        let mut cause = Self::new(RootKind::of(error), why);
        if let FreezeError::Const { c, .. } = error {
            let constant = cause_label(records, Want::Head(*c), g);
            cause.words = leak(g, format_args!("{} (constant `{constant}`)", cause.group));
        }
        cause
    }

    /// Each UTF-8 field has a u64 little-endian byte length. The read observes the
    /// classification, the explanation and the terminal reason used for grouping.
    pub(crate) fn digest(self, domain: &'static str) -> blaze_ids::ContentId {
        blaze_ids::Hasher::new(domain).part(self.kind.tag().as_bytes()).part(self.words.as_bytes()).part(self.group.as_bytes()).finish()
    }
}

/// A label kept in a cause. An unnamed item's identity survives edits above it in
/// its file, so its cell digest must not include a display location.
pub(crate) fn cause_label<'n>(records: &'n Records<'n>, want: Want, g: Alloc<'static>) -> &'static str {
    use blaze_ladder::front2::item_shown::ItemShown;
    use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};

    let mut key = want.key();
    if let Want::MemberFn(owner, member) = want {
        let mut found = None;
        records.each_member(owner, &mut |row| {
            if let (blaze_surface::ItemKind::Function, Some(name)) = (row.kind, row.name)
                && member_key_of(checker_of(owner), MemberKind::Fn, name) == member
            {
                found = Some(row.key);
            }
        });
        let Some(item) = found else {
            return leak(g, format_args!("unlisted member fn {:032x} of {owner:x}", member.0));
        };
        key = item;
    }
    match ItemShown::of_lib(records, key).path() {
        Some(path) => leak(g, format_args!("{path}")),
        None => leak(g, format_args!("unnamed item {key:x}")),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Step {
    Needs(Want),
    End(Want, Cause),
}

impl Step {
    pub(crate) fn retained<'n>(cell: Want, failure: CellFailure<'_>, records: &'n Records<'n>, g: Alloc<'static>) -> Self {
        match failure {
            CellFailure::Needs(next) => Self::Needs(next),
            CellFailure::Own(error) => Self::End(cell, Cause::own(error, records, g)),
            CellFailure::Refused { want, why } => {
                let kind = RootKind::of(&FreezeError::NotTheirs { want, why });
                Self::End(want, Cause::new(kind, why.said()))
            }
        }
    }
}

/// Publish only the cell's own error (and the constant's name in its records) or its
/// first failed direct need. Both are fixed by the cell's demand order. The caller's
/// chain and the order workers reach the cell do not enter the shared row.
pub(crate) fn published<'n>(want: Want, failure: CellFailure<'_>, records: &'n Records<'n>, g: Alloc<'static>) -> Option<(Cause, Option<Want>)> {
    match failure {
        CellFailure::Own(error) => Some((Cause::own(error, records, g), None)),
        CellFailure::Needs(next) => {
            let error = FreezeError::Again { want, failed: next };
            Some((Cause::new(RootKind::Head, leak(g, format_args!("{}", Reason(&error)))), Some(next)))
        }
        CellFailure::Refused { .. } => None,
    }
}

/// Follow at most MOST_MAKING direct needs. Only a repeated cell is a cycle. A long
/// acyclic chain and a missing entry each name the cell at which the explanation stopped.
pub(crate) fn root_cause(
    asked: Want,
    mut step: impl FnMut(Want) -> Option<Step>,
    label: impl Fn(Want) -> &'static str,
    scratch: impl core::alloc::Allocator + Copy,
    g: Alloc<'static>,
) -> Cause {
    let mut seen = blaze_hash::HashSet::new_in(scratch);
    let mut cell = asked;
    for _ in 0..=MOST_MAKING {
        match step(cell) {
            Some(Step::End(root, mut cause)) => {
                if root != asked {
                    cause.words = leak(g, format_args!("it needs `{}`: {}", label(root), cause.words));
                }
                return cause;
            }
            Some(Step::Needs(next)) => {
                seen.insert(want_bytes(cell));
                if seen.contains(&want_bytes(next)) {
                    return Cause::new(RootKind::Engine, leak(g, format_args!("cycle in failed head needs at `{}`", label(next))));
                }
                cell = next;
            }
            None => return Cause::new(RootKind::Engine, leak(g, format_args!("no retained failure for the head of `{}`", label(cell)))),
        }
    }
    Cause::new(RootKind::Engine, leak(g, format_args!("failed head chain reaches `{}` beyond the limit of {MOST_MAKING} needs", label(cell))))
}

/// Resolve a head task's failure while its names and source are still available. A need
/// another job froze has its entry in the shared table. The resulting note can stand on a
/// later request where neither source exists.
pub(crate) fn head_result<'n, N: NamesSource<'n>>(
    job: &crate::engine_heads_once::JobCx<'_, '_, 'n, '_, '_>,
    source: &SeededHeads<'n, '_, N>,
    asked: Want,
    made: HeadDone<'static>,
) -> Result<HeadDone<'static>, Cause> {
    let Some(error) = &made.failed else { return Ok(made) };
    if source.failure(asked).is_none() && !matches!(error, FreezeError::Again { .. } | FreezeError::NotTheirs { why: NotTheirs::Failed { .. }, .. }) {
        return Err(Cause::own(error, job.records, job.g));
    }
    Err(root_cause(asked, |cell| {
        if let Some(entry) = job.once.look_failed(cell) {
            return Some(match entry.needs {
                Some(need) => Step::Needs(need),
                None => Step::End(cell, entry.cause),
            });
        }
        let (failure, _) = source.failure(cell)?;
        Some(Step::retained(cell, failure, job.records, job.g))
    }, |cell| cause_label(job.records, cell, job.g), job.frame, job.g))
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
            FreezeError::NotTheirs { why, .. } => write!(f, "{}", why.said()),
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
        // The same refusal, raised for an item declared in a body: its words, then the item.
        Why::UnsupportedIn(what, item, _) => write!(f, "{what}: {item}"),
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
        Why::Regions(why) => write!(f, "its lifetimes were refused: {why}"),
    }
}

fn const_words(fail: &ConstFail, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match fail {
        ConstFail::Body { .. } => write!(f, "a body its value needs could not be lowered"),
        ConstFail::RequiredBody { .. } => write!(f, "{fail}"),
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
