//! What `engine-check` printed, kept as one record (engine.md E6, the incremental half): each
//! task's findings and notes as the printer takes them, in a block per task, sorted by name.
//!
//! A request prints the WHOLE answer through one printer ([`print_all`]): the blocks of the tasks
//! it entered, made from their results now, and the blocks of the rows that stand, copied from
//! the last request's record. So an incremental request prints what a fresh request on the same
//! text prints, and a request with nothing changed (the certificate) prints the last record's
//! blocks with no page of the answer read: one get. The record's key is in the pointer
//! (engine_answer.rs), its bytes pinned with the answer's pages.
//!
//! ```text
//! record  "BEF1" | blocks u32 | per block, by name: kind u8 | subject [32] | bytes u32 | entries
//! entry   0 | rule u16 | start u32 | end u32 | path bytes u32 (NO_PATH: none) | path | words u32 | words
//!         1 | class u8 | words u32 | words
//! ```
//!
//! A finding's words are its record's message as stored (a names record's place pieces in
//! them); a finding with a path is an outline's, placed at its file's start.

use core::fmt;

use blaze_drive::{Condition, Kind, Key, Name, Print};
use blaze_engine::{SMap, SVec};
use blaze_ids::ContentId;
use blaze_own::{SharedAlloc, Vec};
use bstd::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;
use bstd::out::Out;

use crate::{CliError, print, work};

/// The record's form.
pub(crate) const LINES_MAGIC: &[u8; 4] = b"BEF1";
const LINES_DOMAIN: &str = "blaze-cli engine-check lines";
/// A finding with no path of its own.
const NO_PATH: u32 = u32::MAX;
const FINDING: u8 = 0;
const NOTE: u8 = 1;

/// What a note counts as in the command's status (main.rs `close`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    /// A task that failed: printed, and the command fails.
    Failure = 0,
    /// An item the check could not check: printed, counted apart (main.rs `UNCHECKED_ITEMS`).
    Unchecked = 1,
    /// A task that waited on another's failure: a failure, counted under one line.
    Waited = 2,
}

impl Class {
    fn of_byte(byte: u8) -> Option<Class> {
        match byte {
            0 => Some(Class::Failure),
            1 => Some(Class::Unchecked),
            2 => Some(Class::Waited),
            _ => None,
        }
    }
}

/// Why a lines record was refused, at which byte (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct LinesError {
    pub(crate) at: usize,
    pub(crate) what: &'static str,
}

impl fmt::Display for LinesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the printed lines' record (engine_lines.rs) is refused at byte {}: {}", self.at, self.what)
    }
}

/// The blocks of one request, being made: each task's entries by name.
pub(crate) struct Lines<'a> {
    blocks: SMap<'a, Name, SVec<'a, u8>>,
    alloc: SharedAlloc<'a>,
}

fn word(out: &mut SVec<'_, u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn len_of(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).unwrap_or(u32::MAX)
}

impl<'a> Lines<'a> {
    pub(crate) fn new_in(alloc: SharedAlloc<'a>) -> Lines<'a> {
        Lines { blocks: SMap::new_in(alloc), alloc }
    }

    fn block(&mut self, name: Name) -> &mut SVec<'a, u8> {
        let alloc = self.alloc;
        self.blocks.entry(name).or_insert_with(|| SVec::new_in(alloc))
    }

    /// A finding of `name`'s record: `path` for an outline's (placed at the file's start).
    pub(crate) fn finding(&mut self, name: Name, rule: u16, start: u32, end: u32, path: Option<&[u8]>, words: &[u8]) {
        let block = self.block(name);
        block.push(FINDING);
        block.extend_from_slice(&rule.to_le_bytes());
        word(block, start);
        word(block, end);
        match path {
            Some(path) => {
                word(block, len_of(path));
                block.extend_from_slice(path);
            }
            None => word(block, NO_PATH),
        }
        word(block, len_of(words));
        block.extend_from_slice(words);
    }

    /// A note of `name`, counted as `class`.
    pub(crate) fn note(&mut self, name: Name, class: Class, words: &[u8]) {
        let block = self.block(name);
        block.push(NOTE);
        block.push(class as u8);
        word(block, len_of(words));
        block.extend_from_slice(words);
    }

    /// A standing row's block, as the last record holds it.
    pub(crate) fn keep(&mut self, name: Name, entries: &[u8]) {
        self.block(name).extend_from_slice(entries);
    }

    /// The blocks in name order.
    pub(crate) fn blocks(&self) -> impl Iterator<Item = (Name, &[u8])> {
        self.blocks.iter().map(|(name, block)| (*name, block.as_slice()))
    }

    /// The record's bytes and its key (of its bytes: two requests that print alike write one).
    pub(crate) fn record(&self) -> (Key, SVec<'a, u8>) {
        let mut out = SVec::new_in(self.alloc);
        out.extend_from_slice(LINES_MAGIC);
        word(&mut out, u32::try_from(self.blocks.len()).unwrap_or(u32::MAX));
        for (name, block) in self.blocks.iter() {
            out.push(name.kind as u8);
            out.extend_from_slice(&name.subject.0);
            word(&mut out, len_of(block));
            out.extend_from_slice(block);
        }
        (Key(ContentId::of(LINES_DOMAIN, &out)), out)
    }
}

/// A lines record read: each block by name, sorted, every entry checked once here so the
/// printer never stops half way.
pub(crate) struct Last<'m, 'a> {
    blocks: SVec<'a, (Name, &'m [u8])>,
}

impl<'m> Last<'m, '_> {
    /// `name`'s block, when the last request printed anything of it.
    pub(crate) fn block(&self, name: Name) -> Option<&'m [u8]> {
        self.blocks.binary_search_by_key(&name, |(at, _)| *at).ok().map(|at| self.blocks[at].1)
    }

    /// The blocks in name order.
    pub(crate) fn blocks(&self) -> impl Iterator<Item = (Name, &'m [u8])> + '_ {
        self.blocks.iter().copied()
    }
}

/// A cursor over a record's bytes.
struct Reader<'m> {
    bytes: &'m [u8],
    at: usize,
}

impl<'m> Reader<'m> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'m [u8], LinesError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(LinesError { at: self.at, what })?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self, what: &'static str) -> Result<u8, LinesError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, LinesError> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, LinesError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn sized(&mut self, what: &'static str) -> Result<&'m [u8], LinesError> {
        let len = self.u32(what)?;
        self.take(len as usize, what)
    }
}

/// One entry of a block.
enum Entry<'m> {
    Finding { rule: u16, start: u32, end: u32, path: Option<&'m [u8]>, words: &'m [u8] },
    Note { class: Class, words: &'m [u8] },
}

fn entry<'m>(reader: &mut Reader<'m>) -> Result<Entry<'m>, LinesError> {
    match reader.byte("an entry's tag")? {
        FINDING => {
            let rule = reader.u16("a finding's rule")?;
            let start = reader.u32("a finding's start")?;
            let end = reader.u32("a finding's end")?;
            let path = match reader.u32("a finding's path length")? {
                NO_PATH => None,
                len => Some(reader.take(len as usize, "a finding's path")?),
            };
            if path.is_some_and(|path| core::str::from_utf8(path).is_err()) {
                return Err(LinesError { at: reader.at, what: "a finding's path is not UTF-8" });
            }
            let words = reader.sized("a finding's words")?;
            Ok(Entry::Finding { rule, start, end, path, words })
        }
        NOTE => {
            let at = reader.at;
            let class = Class::of_byte(reader.byte("a note's class")?).ok_or(LinesError { at, what: "a note's class is none of the three" })?;
            let words = reader.sized("a note's words")?;
            if core::str::from_utf8(words).is_err() {
                return Err(LinesError { at: reader.at, what: "a note's words are not UTF-8" });
            }
            Ok(Entry::Note { class, words })
        }
        _ => Err(LinesError { at: reader.at - 1, what: "an entry's tag is neither a finding nor a note" }),
    }
}

/// The record `bytes`, every block and entry checked.
pub(crate) fn decode<'m, 'a>(bytes: &'m [u8], alloc: SharedAlloc<'a>) -> Result<Last<'m, 'a>, LinesError> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(4, "the form")? != LINES_MAGIC {
        return Err(LinesError { at: 0, what: "the form is not BEF1" });
    }
    let count = reader.u32("the block count")?;
    let mut blocks: SVec<'a, (Name, &'m [u8])> = SVec::new_in(alloc);
    for _ in 0..count {
        let at = reader.at;
        let kind = Kind::from_u8(reader.byte("a block's kind")?).ok_or(LinesError { at, what: "a block's kind is no task kind" })?;
        let mut subject = [0; 32];
        subject.copy_from_slice(reader.take(32, "a block's subject")?);
        let name = Name::new(kind, ContentId(subject));
        if blocks.last().is_some_and(|(last, _)| *last >= name) {
            return Err(LinesError { at, what: "the blocks are not sorted by name, one each" });
        }
        let block = reader.sized("a block's entries")?;
        let mut inner = Reader { bytes: block, at: 0 };
        while inner.at < block.len() {
            entry(&mut inner).map_err(|why| LinesError { at: reader.at - block.len() + why.at, what: why.what })?;
        }
        blocks.push((name, block));
    }
    if reader.at != bytes.len() {
        return Err(LinesError { at: reader.at, what: "bytes follow the last block" });
    }
    Ok(Last { blocks })
}

/// What [`print_all`] counted, for the command's status.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tally {
    pub(crate) failures: u32,
    pub(crate) unchecked: u32,
    pub(crate) waited: u32,
    /// Findings listed (after one finding of two tasks is folded into one).
    pub(crate) listed: u32,
}

/// What makes two findings one when printed: their file, their item (its chain, or its start),
/// rule, range and words.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Seen<'p> {
    path: Option<&'p str>,
    at_item: (u8, u64),
    rule: u16,
    start: u32,
    end: u32,
    words: &'p [u8],
}

/// Every block printed as `check` prints (main.rs `draw`): each note as it comes, then the
/// findings listed and drawn a file at a time; a names record's unresolved path, include outside
/// its package and failed call placed by their message's pieces; one finding two tasks hold
/// printed once, the second task named with it.
pub(crate) fn print_all<'f, 'b>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    blocks: impl Iterator<Item = (Name, &'b [u8])>,
    shown: &mut print::Shown<'f>,
    heap: &'static blaze_own::Heap,
    frame: &'f Frame,
    out: &Out,
) -> Result<Tally, CliError> {
    let erased = blaze_own::Erased::new(frame);
    let listed = erased.handle();
    let alloc = heap.shared();
    // One a finding or a file: the project's size, so in the request's heap, never the frame.
    let mut prints: Vec<Print<'_>, blaze_own::Alloc<'static>> = Vec::new_in(heap.alloc());
    let mut wanted: Vec<print::Wanted<'f>, blaze_own::Alloc<'static>> = Vec::new_in(heap.alloc());
    // (path, first print, past the last): each file's findings are neighbours.
    let mut runs: Vec<(Option<&'f str>, usize, usize), blaze_own::Alloc<'static>> = Vec::new_in(heap.alloc());
    let mut seen: SMap<'_, Seen<'f>, usize> = SMap::new_in(alloc);
    let mut tally = Tally::default();
    let refused = |why: LinesError| CliError::Text { what: why.what };
    for (name, block) in blocks {
        let mut reader = Reader { bytes: block, at: 0 };
        while reader.at < block.len() {
            match entry(&mut reader).map_err(refused)? {
                Entry::Note { class, words } => {
                    // Checked UTF-8 at the decode, and made from a `str` at the write.
                    let words = core::str::from_utf8(words).map_err(|_| CliError::Text { what: "a note's words are not UTF-8" })?;
                    match class {
                        Class::Failure => {
                            tally.failures += 1;
                            print::note(cx, words, shown, frame, out)?;
                        }
                        Class::Unchecked => {
                            tally.unchecked += 1;
                            print::note(cx, words, shown, frame, out)?;
                        }
                        Class::Waited => {
                            tally.failures += 1;
                            tally.waited += 1;
                        }
                    }
                }
                Entry::Finding { rule, start, end, path, words: message } => {
                    let (path, place, words): (Option<&'f str>, print::Place, &[u8]) = match path {
                        Some(path) => (Some(work::held_str(core::str::from_utf8(path).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?, frame)), print::Place::At(0), message),
                        None => {
                            let placed = crate::names::placed(rule) && message.contains(&0x1F);
                            match placed {
                                true => match crate::names::place_pieces(message)? {
                                    (Some((path, chain, edition)), words) => (Some(work::held_str(path, frame)), print::Place::Chain(chain, edition), words),
                                    (None, words) => (None, print::Place::At(0), words),
                                },
                                false => (None, print::Place::At(0), message),
                            }
                        }
                    };
                    let kept: &'f [u8] = Vec::from_slice_in(words, frame).leak();
                    let at = prints.len();
                    let at_item = match place {
                        print::Place::Chain(chain, _) => (1, chain),
                        print::Place::At(start) => (0, u64::from(start)),
                        print::Place::Counted => (2, 0),
                    };
                    let what = Seen { path, at_item, rule, start, end, words: kept };
                    if let Some(&first) = seen.get(&what) {
                        if prints[first].name != name && !prints[first].also.contains(&name) {
                            prints[first].also.push(name);
                        }
                        continue;
                    }
                    seen.insert(what, at);
                    let mut whole = Vec::new_in(listed);
                    whole.extend_from_slice(message);
                    prints.push(Print { name, also: Vec::new_in(listed), condition: Condition::ANY, file: None, place: 0, rule, start, end, message: whole });
                    wanted.push(print::Wanted { path, place, words: kept });
                    match runs.last_mut() {
                        Some(run) if run.0 == path && run.2 == at => run.2 = at + 1,
                        _ => runs.push((path, at, at + 1)),
                    }
                }
            }
        }
    }
    let mut scratch = print::Scratch::new(work::Sizes::USUAL.scratch)?;
    let first = shown.listed.len();
    print::list(cx, root, &prints, &wanted, &mut scratch, frame, &mut shown.listed)?;
    crate::fix::lengths(&mut shown.listed[first..], frame);
    for &(path, from, past) in runs.iter() {
        print::file_findings(cx, root, path, first + from..first + past, shown, &mut scratch, out)?;
    }
    if tally.waited > 0 {
        print::note(cx, bstd::mem::format_in!(frame, "{} task(s) not run: each waits on a task that failed", tally.waited).as_str(), shown, frame, out)?;
    }
    tally.listed = u32::try_from(prints.len()).unwrap_or(u32::MAX);
    Ok(tally)
}
