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
//!
//! An answer's digest (version 2) is canonical: [`ANSWER`]'s hasher fed the answer's fields in
//! declaration order, a tag byte first for every choice (the question's kind, each enum's
//! variant, `Option`'s none or some), each string length-prefixed, each key its 32 bytes, and a
//! crate's `reaches` as a sorted set. Never the answer's `Debug` text (version 1): how a type
//! prints is no stable form, and the text cost a buffer per question.

use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ids::{Domain, Hasher};
use blaze_own::{AVec, Alloc, Bump};
use blaze_surface::{Binding, BindingState, Def, DefKind, Edition, Namespace, Visibility};

use crate::crate_scope::Dep;
use crate::scope_record::Reaches;
use crate::read::{NameAnswer, Read};
use crate::resolve::Built;
use crate::task::{Held, MacroBody, MacroSrc, Own, Records, answer};

const MAGIC: [u8; 4] = *b"BZRL";
/// 2: answers digested canonically ([`ANSWER`]), no longer from `Debug` text; a version 1
/// record is refused as [`ReadListError::NotThisRecord`] and its solve is redone.
const VERSION: u32 = 2;
/// The domain every answer digest is hashed in; its context key is derived once.
static ANSWER: Domain = Domain::new("blaze-scope question answer 2");

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
    /// `_alloc` is unused since answers are digested without a buffer; it goes with its
    /// callers' next batch.
    pub(crate) fn of_read(read: Read<&'a str>, own: &Own<'_, &'a str>, _alloc: Alloc<'_>) -> Option<Question<'a>> {
        #[cfg(test)]
        if plant::forgets(read, own) {
            return None;
        }
        let answer = own_digest(own)?;
        let (kind, module, ns, name) = match read {
            Read::Name { module, ns, name } => (Kind::Name, module, ns.byte(), name),
            Read::Globs { module } => (Kind::Globs, module, 0, ""),
            Read::TextualEntry { module } => (Kind::TextualEntry, module, 0, ""),
            Read::TextualOut { module } => (Kind::TextualOut, module, 0, ""),
            Read::DeriveHelpers { .. } => return None,
        };
        Some(Question { kind, module, ns, name, answer })
    }

    /// `_alloc` as in [`Question::of_read`].
    pub(crate) fn macro_src(key: ItemKey, src: Option<MacroSrc<'_>>, _alloc: Alloc<'_>) -> Question<'a> {
        Question { kind: Kind::Macro, module: key, ns: 0, name: "", answer: macro_digest(src) }
    }

    /// `_alloc` as in [`Question::of_read`].
    pub(crate) fn lookup(module: ItemKey, ns: Namespace, name: &'a str, found: Option<Binding>, _alloc: Alloc<'_>) -> Question<'a> {
        Question { kind: Kind::Lookup, module, ns: ns.byte(), name, answer: lookup_digest(found) }
    }

    pub(crate) fn shadow(module: ItemKey, ns: Namespace, name: &'a str, may: bool) -> Question<'a> {
        Question { kind: Kind::Shadow, module, ns: ns.byte(), name, answer: shadow_digest(may) }
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

/// One question in [`Asked`]'s list, in the solve's arena.
struct AskedNode<'a> {
    question: Question<'a>,
    next: *mut AskedNode<'a>,
}

/// The questions one solve asks, from every worker of its fill: a push-only list with no lock.
/// A worker writes its node in the arena, then publishes it with one compare-exchange on the
/// head; nothing is ever removed while workers push, so the head can only grow (no ABA). The
/// order of the list is the order of the exchanges, which is a worker's: the record boundary
/// ([`encode`]) sorts and dedups it, so the bytes are a function of the questions alone.
pub(crate) struct Asked<'a> {
    head: AtomicPtr<AskedNode<'a>>,
    alloc: Bump<'a>,
}

// SAFETY: a node is reached only through `head`; it is written once by the pushing worker before
// the `Release` exchange publishes it and only read after, in `take`, which owns the list (every
// pushing borrow has ended; the fill joined its workers). A `Question` is plain keys and arena
// `&str`, safe to read from another thread.
unsafe impl Sync for Asked<'_> {}
// SAFETY: as for `Sync`: the nodes are arena memory of `'a`, owned by no thread.
unsafe impl Send for Asked<'_> {}

impl<'a> Asked<'a> {
    pub(crate) fn new_in(alloc: Bump<'a>) -> Asked<'a> {
        Asked { head: AtomicPtr::new(ptr::null_mut()), alloc }
    }

    pub(crate) fn push(&self, question: Question<'a>) {
        let node: *mut AskedNode<'a> = self.alloc.alloc_val(AskedNode { question, next: ptr::null_mut() });
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            // SAFETY: `node` is this worker's alone until the exchange below publishes it.
            unsafe { (*node).next = head };
            match self.head.compare_exchange_weak(head, node, Ordering::Release, Ordering::Acquire) {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    /// Every question pushed, in no particular order ([`encode`] sorts them).
    pub(crate) fn take(self) -> AVec<'a, Question<'a>> {
        let mut out = AVec::new_in(self.alloc.alloc());
        let mut at = self.head.into_inner();
        while !at.is_null() {
            // SAFETY: every non-null pointer in the list is a published node of the arena `'a`,
            // written before its `Release` publish; `self` is owned here, so no push races.
            let node = unsafe { &*at };
            out.push(node.question);
            at = node.next;
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Answer digests: each answer's fields fed to [`ANSWER`]'s hasher in declaration order, a tag
// byte before every choice, strings length-prefixed, keys as their 32 bytes. Every digest
// starts with its question's kind byte. No buffer: nothing is allocated per question.

/// A hasher in [`ANSWER`] with the question's kind byte fed.
fn answer_hasher(kind: Kind) -> Hasher {
    let mut h = Hasher::of(&ANSWER);
    h.update(&[kind as u8]);
    h
}

/// A binding's target: its key, then its kind's tag (a macro's shapes after its tag).
fn feed_def(h: &mut Hasher, def: Def) {
    h.id(&def.key.0);
    match def.kind {
        DefKind::Module => h.update(&[0]),
        DefKind::Enum => h.update(&[1]),
        DefKind::Trait => h.update(&[2]),
        DefKind::Macro(shapes) => h.update(&[3, shapes.byte()]),
        DefKind::Other => h.update(&[4]),
    };
}

/// A visibility: its tag, then the crate or module it names.
fn feed_vis(h: &mut Hasher, vis: Visibility) {
    match vis {
        Visibility::Public => h.update(&[0]),
        Visibility::Crate(krate) => h.update(&[1]).id(&krate.0),
        Visibility::Restricted(module) => h.update(&[2]).id(&module.0),
    };
}

/// A binding's state: its tag, then an ambiguity's kind.
fn feed_state(h: &mut Hasher, state: BindingState) {
    match state {
        BindingState::Determined => h.update(&[0]),
        BindingState::Ambiguous(kind) => h.update(&[1, kind.byte()]),
    };
}

/// The digest of a [`Kind::Name`] answer: none, or the binding's def, vis, state and origin.
pub(crate) fn name_digest(answer: &NameAnswer) -> ContentId {
    let mut h = answer_hasher(Kind::Name);
    match *answer {
        NameAnswer::Absent => {
            h.update(&[0]);
        }
        NameAnswer::Found { def, vis, state, origin } => {
            h.update(&[1]);
            feed_def(&mut h, def);
            feed_vis(&mut h, vis);
            feed_state(&mut h, state);
            h.update(&[origin.byte()]);
        }
    }
    h.finish()
}

/// The digest of a [`Kind::Lookup`] answer: none, or every field of the binding found.
pub(crate) fn lookup_digest(found: Option<Binding>) -> ContentId {
    let mut h = answer_hasher(Kind::Lookup);
    match found {
        None => {
            h.update(&[0]);
        }
        Some(Binding { def, vis, origin, expansion, state }) => {
            h.update(&[1]);
            feed_def(&mut h, def);
            feed_vis(&mut h, vis);
            h.update(&[origin.byte()]).update(&expansion.to_le_bytes());
            feed_state(&mut h, state);
        }
    }
    h.finish()
}

/// The digest of a [`Kind::Macro`] answer: none, or the macro's name, body (its variant's tag,
/// then its fields), module, and its `export` and `local_inner` flags.
pub(crate) fn macro_digest(src: Option<MacroSrc<'_>>) -> ContentId {
    let mut h = answer_hasher(Kind::Macro);
    match src {
        None => {
            h.update(&[0]);
        }
        Some(MacroSrc { name, body, module, export, local_inner }) => {
            h.update(&[1]).part(name.as_bytes());
            match body {
                MacroBody::Rules(text) => h.update(&[0]).part(text.as_bytes()),
                MacroBody::Macro2 { params, body } => h.update(&[1]).opt_part(params).part(body.as_bytes()),
                MacroBody::BuiltinFn(name) => h.update(&[2]).part(name.as_bytes()),
                MacroBody::BuiltinOther(shape) => h.update(&[3, shape.byte()]),
                MacroBody::Proc { kind, helpers } => h.update(&[4, kind.byte()]).part(helpers.as_bytes()),
                MacroBody::BuiltinDerive { helpers } => h.update(&[5]).part(helpers.as_bytes()),
            };
            h.id(&module.0).update(&[u8::from(export), u8::from(local_inner)]);
        }
    }
    h.finish()
}

/// The digest of a [`Kind::Shadow`] answer.
pub(crate) fn shadow_digest(may: bool) -> ContentId {
    let mut h = answer_hasher(Kind::Shadow);
    h.update(&[u8::from(may)]);
    h.finish()
}

/// A handed crate's facts, what a [`Kind::Crate`] question is answered.
#[derive(Clone, Copy)]
pub(crate) struct CrateFacts<'a> {
    pub(crate) key: CrateItemKey,
    pub(crate) root: ItemKey,
    pub(crate) name: &'a str,
    pub(crate) edition: Edition,
    pub(crate) proc_key: Option<CrateItemKey>,
    /// A set: digested sorted and once each, whatever order it is held in.
    pub(crate) reaches: Reaches<'a>,
}

/// The digest of a [`Kind::Crate`] answer: key, root, name, edition, proc key, then `reaches`
/// as a sorted set (count, keys). A record holds `reaches` sorted, so `alloc` is used only for
/// a list held in another order.
pub(crate) fn facts_digest(facts: CrateFacts<'_>, alloc: Alloc<'_>) -> ContentId {
    let CrateFacts { key, root, name, edition, proc_key, reaches } = facts;
    let mut h = answer_hasher(Kind::Crate);
    h.id(&key.0).id(&root.0).part(name.as_bytes()).update(&[edition.byte()]);
    match proc_key {
        None => h.update(&[0]),
        Some(k) => h.update(&[1]).id(&k.0),
    };
    let feed = |h: &mut Hasher, keys: Reaches<'_>| {
        h.update(&count(keys.len()).to_le_bytes());
        for k in keys.iter() {
            h.id(&k.0);
        }
    };
    if (1..reaches.len()).all(|i| reaches.get(i - 1) < reaches.get(i)) {
        feed(&mut h, reaches);
    } else {
        let mut set = AVec::from_iter_in(reaches.iter(), alloc);
        set.sort_unstable();
        set.dedup();
        feed(&mut h, Reaches::Keys(set.as_slice()));
    }
    h.finish()
}

fn own_digest<N>(own: &Own<'_, N>) -> Option<ContentId> {
    match own {
        Own::Name(a) => Some(name_digest(a)),
        Own::Globs { digest, .. } | Own::Textual(_, digest) | Own::Helpers(digest) => Some(*digest),
        Own::InProgress(_) => None,
    }
}

fn crate_digest(dep: Dep<'_>, alloc: Alloc<'_>) -> ContentId {
    let facts = CrateFacts { key: dep.key(), root: dep.root(), name: dep.name(), edition: dep.edition(), proc_key: dep.proc_key(), reaches: dep.reaches() };
    facts_digest(facts, alloc)
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

/// A handed crate as this call has it: its index, exports digest and record (read in place).
pub(crate) type Now<'a> = (usize, ContentId, Dep<'a>);

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
pub(crate) fn check<'a>(bytes: &'a [u8], now: &dyn Fn(CrateItemKey) -> Option<Option<Now<'a>>>, alloc: Alloc<'a>) -> Result<Option<Stale<'a>>, ReadListError> {
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
        slots.push(Some(record));
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
            Kind::Name => own_digest(&check.answer(Read::Name { module, ns: ns()?, name }, here)),
            Kind::Globs => own_digest(&check.answer(Read::Globs { module }, here)),
            Kind::TextualEntry => own_digest(&check.answer(Read::TextualEntry { module }, here)),
            Kind::TextualOut => own_digest(&check.answer(Read::TextualOut { module }, here)),
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
            return Held::Variants { variants: AVec::from_iter_in(v.iter(), self.alloc), krate: other.key() };
        }
        Held::Failed { krate: other.key(), chain: None }
    }

    fn parent(&self, _: ItemKey) -> Option<Option<ItemKey>> {
        None
    }
}
