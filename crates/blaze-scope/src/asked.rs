//! A solve's read list at question grain (per-crate-entry.md, "Runner nineteen", THE RULE): each
//! question one crate's solve asked of another crate's records, with its answer's digest. A kept
//! solve stays valid when every crate it was handed has the same exports digest (nothing asked
//! then), or else when each question of a crate whose digest moved gets the same answer from
//! today's record: a new public name nobody looked up moves no answer.
//!
//! The record (integers little-endian): `BZRL`, version u32, the asking crate's key; handed
//! crates (u32 count; each: key, exports digest), sorted by key; questions (u32 count; each:
//! kind u8, namespace u8, two zero bytes, name length u32, module key, answer digest, name
//! bytes), sorted by (kind, module, namespace, name, answer), no repeats. Its bytes are a
//! function of the questions asked, never of the order a worker asked them in.

use core::fmt::{self, Debug};
use std::sync::Mutex;

use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Binding, Namespace};

use crate::crate_scope::{Dep, DepScope};
use crate::read::Read;
use crate::resolve::Built;
use crate::task::{Held, MacroSrc, Own, Records, answer};

const MAGIC: [u8; 4] = *b"BZRL";
const VERSION: u32 = 1;
const ANSWER_DOMAIN: &str = "blaze-scope question answer 1";

/// What a question asks of another crate's records.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Kind {
    /// `Read::Name`: a module's (or enum's) own binding of a name, or none.
    Name = 0,
    /// `Read::Globs`: a module's glob edges and chain.
    Globs = 1,
    /// `Read::TextualEntry`: the macros visible at a module's `mod` line.
    TextualEntry = 2,
    /// `Read::TextualOut`: what a `#[macro_use]` on a module carries out.
    TextualOut = 3,
    /// A macro's source, read to expand a call of it or for a derive's helpers.
    Macro = 4,
    /// A name looked up through globs ([`Built::lookup`]): the std prelude's path.
    Lookup = 5,
    /// Whether a glob source may bind a name (the record writer's `may_shadow`).
    Shadow = 6,
    /// A handed crate's facts: key, root, name, edition, proc key, what it reaches.
    Crate = 7,
}

impl Kind {
    const ALL: [Kind; 8] = [Kind::Name, Kind::Globs, Kind::TextualEntry, Kind::TextualOut, Kind::Macro, Kind::Lookup, Kind::Shadow, Kind::Crate];
}

/// One question and its answer's digest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Question<'a> {
    kind: Kind,
    module: ItemKey,
    ns: u8,
    name: &'a str,
    answer: ContentId,
}

impl<'a> Question<'a> {
    /// A `Read` the fill asked and what it was answered; `None` for a read that is no question
    /// of another crate's record (a derive's helpers are asked as [`Kind::Macro`]).
    pub(crate) fn of_read(read: Read<&'a str>, own: &Own<'_, &'a str>, alloc: Alloc<'_>) -> Option<Question<'a>> {
        #[cfg(test)]
        if plant::forgets(read, own) {
            return None;
        }
        let answer = own_digest(own, alloc)?;
        let (kind, module, ns, name) = match read {
            Read::Name { module, ns, name } => (Kind::Name, module, ns.byte(), name),
            Read::Globs { module } => (Kind::Globs, module, 0, ""),
            Read::TextualEntry { module } => (Kind::TextualEntry, module, 0, ""),
            Read::TextualOut { module } => (Kind::TextualOut, module, 0, ""),
            Read::DeriveHelpers { .. } => return None,
        };
        Some(Question { kind, module, ns, name, answer })
    }

    pub(crate) fn macro_src(key: ItemKey, src: Option<MacroSrc<'_>>, alloc: Alloc<'_>) -> Question<'a> {
        Question { kind: Kind::Macro, module: key, ns: 0, name: "", answer: debug_digest(&src, alloc) }
    }

    pub(crate) fn lookup(module: ItemKey, ns: Namespace, name: &'a str, found: Option<Binding>, alloc: Alloc<'_>) -> Question<'a> {
        Question { kind: Kind::Lookup, module, ns: ns.byte(), name, answer: debug_digest(&found, alloc) }
    }

    pub(crate) fn shadow(module: ItemKey, ns: Namespace, name: &'a str, may: bool) -> Question<'a> {
        Question { kind: Kind::Shadow, module, ns: ns.byte(), name, answer: ContentId::of(ANSWER_DOMAIN, &[u8::from(may)]) }
    }

    pub(crate) fn krate(dep: Dep<'_>, alloc: Alloc<'_>) -> Question<'a> {
        Question { kind: Kind::Crate, module: dep.root(), ns: 0, name: "", answer: crate_digest(dep, alloc) }
    }
}

/// The planted bugs of the read list's property test (tests/asked.rs): a kind of question left
/// out of the list. Set on the test's thread (its solves run serially on it).
#[cfg(test)]
pub(crate) mod plant {
    use core::cell::Cell;

    use crate::read::{NameAnswer, Read};
    use crate::task::Own;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Plant {
        None,
        /// A named read that found a binding is not noted.
        ForgetFound,
        /// A named read that found nothing is not noted.
        ForgetFailed,
        /// A read of a module's glob edges is not noted.
        ForgetGlobs,
    }

    std::thread_local! {
        pub(crate) static PLANT: Cell<Plant> = const { Cell::new(Plant::None) };
    }

    pub(super) fn forgets<N>(read: Read<N>, own: &Own<'_, N>) -> bool {
        match (PLANT.with(Cell::get), read, own) {
            (Plant::ForgetFound, Read::Name { .. }, Own::Name(NameAnswer::Found { .. })) => true,
            (Plant::ForgetFailed, Read::Name { .. }, Own::Name(NameAnswer::Absent)) => true,
            (Plant::ForgetGlobs, Read::Globs { .. }, _) => true,
            _ => false,
        }
    }
}

/// The questions one solve asks, from every worker of its fill.
pub(crate) struct Asked<'a> {
    list: Mutex<AVec<'a, Question<'a>>>,
}

impl<'a> Asked<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Asked<'a> {
        Asked { list: Mutex::new(AVec::new_in(alloc)) }
    }

    pub(crate) fn push(&self, question: Question<'a>) {
        self.list.lock().unwrap_or_else(|p| panic!("the asked list's lock was poisoned: {p}")).push(question);
    }

    pub(crate) fn take(self) -> AVec<'a, Question<'a>> {
        self.list.into_inner().unwrap_or_else(|p| panic!("the asked list's lock was poisoned: {p}"))
    }
}

/// A byte sink for `Debug` text: the answer of a question is digested from it (every field of
/// a derived `Debug`, in declaration order; keys print as their bytes, no address).
struct Sink<'b>(AVec<'b, u8>);

impl fmt::Write for Sink<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

fn debug_digest(value: &dyn Debug, alloc: Alloc<'_>) -> ContentId {
    let mut sink = Sink(AVec::new_in(alloc));
    fmt::write(&mut sink, format_args!("{value:?}")).unwrap_or_else(|e| panic!("a Debug answer did not format: {e}"));
    ContentId::of(ANSWER_DOMAIN, &sink.0)
}

fn own_digest<N: Copy + Debug>(own: &Own<'_, N>, alloc: Alloc<'_>) -> Option<ContentId> {
    match own {
        Own::Name(a) => Some(debug_digest(a, alloc)),
        Own::Globs { digest, .. } | Own::Textual(_, digest) | Own::Helpers(digest) => Some(*digest),
        Own::InProgress(_) => None,
    }
}

fn crate_digest(dep: Dep<'_>, alloc: Alloc<'_>) -> ContentId {
    let facts = (dep.key(), dep.root(), dep.name(), dep.edition(), dep.proc_key(), dep.reaches());
    debug_digest(&facts, alloc)
}

/// The read list record of a solve of `here`: `handed`, each crate whose exports record it was
/// given with that record's digest, and the questions it asked.
pub fn encode<'b>(here: CrateItemKey, handed: &[(CrateItemKey, ContentId)], asked: &[Question<'_>], alloc: Alloc<'b>) -> AVec<'b, u8> {
    let mut handed = AVec::from_slice_in(handed, alloc);
    handed.sort_unstable();
    handed.dedup();
    let mut asked = AVec::from_slice_in(asked, alloc);
    asked.sort_unstable();
    asked.dedup();
    let mut out = AVec::with_capacity_in(48 + handed.len() * 64 + asked.len() * 80, alloc);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&here.0.0);
    out.extend_from_slice(&count(handed.len()).to_le_bytes());
    for (key, digest) in handed.iter() {
        out.extend_from_slice(&key.0.0);
        out.extend_from_slice(&digest.0);
    }
    out.extend_from_slice(&count(asked.len()).to_le_bytes());
    for q in asked.iter() {
        out.extend_from_slice(&[q.kind as u8, q.ns, 0, 0]);
        out.extend_from_slice(&count(q.name.len()).to_le_bytes());
        out.extend_from_slice(&q.module.0.0);
        out.extend_from_slice(&q.answer.0);
        out.extend_from_slice(q.name.as_bytes());
    }
    out
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or_else(|_| panic!("a read list of {n} entries is past u32 (rule 18: the cap)"))
}

/// Why a read list record was not read.
#[derive(Debug)]
pub enum ReadListError {
    /// Not `BZRL` and this version.
    NotThisRecord,
    /// A field ran past the end: what it was.
    Short(&'static str),
    /// A byte that is no question kind or namespace, at this offset.
    BadByte { at: usize, what: &'static str },
    /// A name that is not UTF-8, at this offset.
    NotUtf8 { at: usize },
}

/// Why a kept solve does not hold today.
#[derive(Debug)]
pub enum Stale<'s> {
    /// It was handed a crate not in this call.
    NotInGraph(CrateItemKey),
    /// It was handed a crate whose exports record is not written yet in this call.
    NotWritten(CrateItemKey),
    /// A question of crate `krate` gets another answer: what it asked.
    Answer { krate: usize, kind: Kind, module: ItemKey, name: &'s str },
}

/// A handed crate as this call has it: its index, exports digest and decoded record.
pub type Now<'a> = (usize, ContentId, &'a DepScope<'a>);

struct Bytes<'s> {
    bytes: &'s [u8],
    at: usize,
}

impl<'s> Bytes<'s> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'s [u8], ReadListError> {
        let part = self.bytes.get(self.at..self.at + n).ok_or(ReadListError::Short(what))?;
        self.at += n;
        Ok(part)
    }

    fn u32(&mut self, what: &'static str) -> Result<usize, ReadListError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    fn id(&mut self, what: &'static str) -> Result<ContentId, ReadListError> {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.take(32, what)?);
        Ok(ContentId(out))
    }
}

/// Whether the read list `bytes` holds against this call's records (`now`, by crate key):
/// `None` when it holds. A crate whose exports digest is the one it was handed costs nothing
/// more; each question of a crate whose digest moved is answered again from its record.
pub fn check<'a>(bytes: &'a [u8], now: &dyn Fn(CrateItemKey) -> Option<Option<Now<'a>>>, alloc: Alloc<'a>) -> Result<Option<Stale<'a>>, ReadListError> {
    let mut r = Bytes { bytes, at: 0 };
    if r.take(4, "the magic")? != MAGIC || r.u32("the version")? != VERSION as usize {
        return Err(ReadListError::NotThisRecord);
    }
    let here = CrateItemKey(r.id("the crate's key")?);
    let n = r.u32("the handed count")?;
    let mut slots: AVec<'a, Option<Dep<'a>>> = AVec::with_capacity_in(n, alloc);
    let mut index = AVec::with_capacity_in(n, alloc);
    let mut by_prefix = HashMap::new_in(alloc);
    let mut moved = AVec::new_in(alloc);
    for _ in 0..n {
        let key = CrateItemKey(r.id("a handed crate's key")?);
        let was = r.id("a handed crate's digest")?;
        let Some(found) = now(key) else { return Ok(Some(Stale::NotInGraph(key))) };
        let Some((g, digest, record)) = found else { return Ok(Some(Stale::NotWritten(key))) };
        by_prefix.insert(key.prefix(), slots.len());
        slots.push(Some(Dep::Record(record)));
        index.push(g);
        if digest != was {
            moved.push(key.prefix());
        }
    }
    if moved.is_empty() {
        return Ok(None);
    }
    let built = Built { crates: &slots, by_prefix: &by_prefix, asked: None };
    let check = Check { built: &built, alloc };
    let n = r.u32("the question count")?;
    for _ in 0..n {
        let at = r.at;
        let head = r.take(4, "a question's kind")?;
        let kind = *Kind::ALL.get(head[0] as usize).ok_or(ReadListError::BadByte { at, what: "a byte that is no question kind" })?;
        let ns_byte = head[1];
        let len = r.u32("a question's name length")?;
        let module = ItemKey(r.id("a question's module")?);
        let was = r.id("a question's answer")?;
        let at = r.at;
        let name = core::str::from_utf8(r.take(len, "a question's name")?).map_err(|_| ReadListError::NotUtf8 { at })?;
        let prefix = module.0.0[..8].try_into().unwrap_or([0; 8]);
        // Only a crate whose exports moved can answer otherwise; a lookup walks globs into any.
        if kind != Kind::Lookup && !moved.contains(&prefix) {
            continue;
        }
        let ns = || Namespace::from_byte(ns_byte).ok_or(ReadListError::BadByte { at, what: "a byte that is no namespace" });
        let answer = match kind {
            Kind::Name => own_digest(&check.answer(Read::Name { module, ns: ns()?, name }, here), alloc),
            Kind::Globs => own_digest(&check.answer(Read::Globs { module }, here), alloc),
            Kind::TextualEntry => own_digest(&check.answer(Read::TextualEntry { module }, here), alloc),
            Kind::TextualOut => own_digest(&check.answer(Read::TextualOut { module }, here), alloc),
            Kind::Macro => Some(Question::macro_src(module, built.of(module).and_then(|d| d.macro_of(module)), alloc).answer),
            Kind::Lookup => Some(Question::lookup(module, ns()?, name, built.lookup(module, ns()?, name, alloc), alloc).answer),
            Kind::Shadow => Some(Question::shadow(module, ns()?, name, crate::crate_scope::dep_may_shadow(&built, module, ns()?, name)).answer),
            Kind::Crate => built.of(module).map(|d| crate_digest(d, alloc)),
        };
        if answer != Some(was) {
            let g = by_prefix.get(&prefix).map_or(usize::MAX, |&p| index[p]);
            return Ok(Some(Stale::Answer { krate: g, kind, module, name }));
        }
    }
    Ok(None)
}

/// The records a question is answered from again: the handed crates only, as the solve had them.
struct Check<'q, 'r, 'a> {
    built: &'q Built<'r, 'a>,
    alloc: Alloc<'a>,
}

impl<'a> Check<'_, '_, 'a> {
    fn answer(&self, read: Read<&'a str>, here: CrateItemKey) -> Own<'a, &'a str> {
        match answer(read, self, here, self.alloc) {
            Ok((own, _)) => own,
            // Another crate's module never waits: it is finished or failed.
            Err(_) => Own::InProgress(read.module()),
        }
    }
}

impl<'a> Records<&'a str> for Check<'_, '_, 'a> {
    fn held(&self, key: ItemKey) -> Held<'_, &'a str> {
        let Some(other) = self.built.of(key) else { return Held::Unknown };
        if let Some(scope) = other.scope(key) {
            return Held::Scope { scope, krate: other.key(), chain: other.chain(key) };
        }
        if let Some(v) = other.enum_of(key) {
            return Held::Variants { variants: AVec::from_slice_in(v, self.alloc), krate: other.key() };
        }
        Held::Failed { krate: other.key(), chain: None }
    }

    fn parent(&self, _: ItemKey) -> Option<Option<ItemKey>> {
        None
    }
}
