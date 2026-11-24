//! The READ LIST of a code entry: what the BUILD of an instance read beyond its key, each
//! question with the digest of the answer it got, so an entry found by its key is used only
//! after every row is asked again and answered the same (AGENTS.md, "The design": a record is
//! valid by its key, or else by its read list checked against today's facts).
//!
//! ONE RECORDER. [`Recorded`] wraps the world the builder is handed (`blaze_build::Adts` and
//! its `LayoutFacts`): every method is forwarded to that world unchanged, and the questions
//! below are logged in a [`ReadLog`] as they are answered. blaze-build's traits are not
//! changed, and a method the wrapper did not forward would answer with the trait's default and
//! change the build, so every one is forwarded, recorded or not.
//!
//! RECORDED (kind: the question, then what the answer's digest holds):
//! - `Adt`: `LayoutFacts::adt` and `Adts::adt`, by item: the definition's kind, generics,
//!   variants (key, name, ctor, field count) and every field's name and type, the type digested
//!   by its structure (`term`), never by a table handle. A struct gaining a field moves it.
//! - `Repr`, `Discriminant` (item, variant), `ValidRange`: the rest of what a layout reads.
//! - `Normalize` (trait, name, arguments) and `OpaqueHidden` (the opaque's owner row: the fn
//!   whose head made it and its index; arguments, normalized): the type a projection or an
//!   `impl Trait` is at the instance, by structure.
//! - `Const` (item, arguments), `ConstBlock` and `Promoted` (owner, expression, arguments):
//!   the value's bytes, alignment, mutability and relocations, every allocation.
//! - `Static` (item): thread-local and mutable.
//! - `ObjectTrait` (trait): the vtable shape's own digest (`ObjectTrait::shape`).
//! - `DropImpl` (type): the `Drop` impl the type's glue calls.
//! - `Abi`, `Nounwind` (item) and `TrackCaller` (instance).
//! - `Resolve` (a call's instance): the instance that runs, the impl a trait's fn is selected
//!   to. Not an `Adts` question: the walk asks the front end's `resolve` as it names a
//!   reference, and logs it through [`ReadLog::ask_resolve`].
//! - `ForeignStatic`, `ForeignFn` (item): a static's or a callee's C symbol, the walk's too
//!   ([`ReadLog::ask_foreign_static`], [`ReadLog::ask_foreign_fn`]).
//! - `CVariadic`, `LegacyConstGenerics` (item): the signature attributes a call reads.
//! - `Laid` (owner, ordinal, arguments): the full lay of the coroutine one of whose functions
//!   the entry is (run/lay_states.rs `lay_one`: its state, region, niche and saved locals'
//!   table), digested ([`laid_answer`]); asked again by laying it again
//!   ([`Beyond::laid`]). What the lay read (the owner's recipe, its held types' layouts) is in
//!   that answer, so the lay keeps no rows of its own.
//! - `CoroutineState`, `CoroutineNiche` (owner, ordinal, arguments): a coroutine state's shape
//!   and its tag's niche as the build read them through `LayoutFacts`; asked again once the
//!   verifier has laid that state as the build's lay did (run/make_one.rs `verify_instance`).
//!
//! FORWARDED, NOT RECORDED: literals, patterns, string and format pieces, and caller sites of
//! a body (`int_lit`, `int_pat`, `float_pat`, `str_pat`, `float_lit`, `byte_str`, `str_lit`,
//! `format_template`, `caller_site`: the owner's text, in the key's body digest when the owner
//! is the instance's own fn); lang items and the run's settings (`lang_fn`, `fn_trait_method`,
//! `future_poll`, `async_items`, `deref_fn`, `index_fn`, `lang_adt`, `fmt_argument_new`,
//! `fmt_argument_adt`, `location_adt`, `panic`, `overflow_checks`, `type_names`); the
//! attribute `intrinsic`; instance keys (`const_fn_instance`); and the closure and coroutine
//! facts (`closure`, `coroutine`). A lang item or
//! attribute edited in a crate of the workspace (a `no_core` program's) is not caught by the
//! list; neither is a closure built in an owner's body other than the instance's own fn.
//!
//! VERIFYING: [`verify_today`] asks each row again, a question already answered this run
//! answered by [`Answers`] (a binary search), any other one front-end lookup and one digest.
//!
//! A QUESTION'S BYTES outlive the run: the kind's byte, an 8-byte scalar (a variant, a name,
//! an expression), then the item and its arguments as an instance name (symbols.rs
//! `encode_instance`), made after the build when the ground table is free
//! ([`ReadLog::seal`]). An answer is digested when it is given, holding no table handle.

use core::cell::{Cell, RefCell};
use core::fmt;
use core::hash::Hash;
use std::alloc::Global;

use blaze_build::data::DataRecord;
use blaze_build::layout::{ClosureFacts, IntRepr, LayoutFacts, Missing, ReprAttrs, ValidRange};
use blaze_build::coroutine_state::{LaidState, StateName};
use blaze_build::{Abi, Adts, BuildError, Instance, InstanceKey, Intrinsic, StateNiche, StateShape, StaticItem};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_typeck::{Adt, ExprId, ItemKey, LangItem, List, Mutbl, Name, PNode, PTy, PTys, PatId, PortableTys};

use crate::code_records::EntryView;
use crate::symbols::{SymbolError, decode_instance, encode_instance};

const ANSWER_DOMAIN: &str = "blaze-ladder code entry read answer 1";

/// The most nodes one type in an answer may have.
pub const TERM_NODES: usize = 1 << 16;

/// The most questions one build may log.
pub const READS_CAP: usize = 1 << 20;

/// What a row asked (the module doc's list). The byte is the question's first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum ReadKind {
    Adt = 1,
    Repr = 2,
    Discriminant = 3,
    ValidRange = 4,
    Normalize = 5,
    OpaqueHidden = 6,
    Const = 7,
    ConstBlock = 8,
    Promoted = 9,
    Static = 10,
    ObjectTrait = 11,
    DropImpl = 12,
    Abi = 13,
    Nounwind = 14,
    TrackCaller = 15,
    Resolve = 16,
    /// A static's C symbol, when an `extern` block declares it (the seam's `foreign_static`).
    ForeignStatic = 17,
    /// A callee's C symbol, when it is an `extern` block's fn (its row's `FnKind::Foreign`).
    ForeignFn = 18,
    /// A fn's C-variadic parameter (`Adts::c_variadic`).
    CVariadic = 19,
    /// A fn's `#[rustc_legacy_const_generics]` (`Adts::legacy_const_generics`).
    LegacyConstGenerics = 20,
    /// The full lay of the coroutine a part's entry was built into ([`laid_answer`]), asked
    /// again by laying it again ([`Beyond::laid`]).
    Laid = 21,
    /// A coroutine state's shape (`LayoutFacts::coroutine_state`).
    CoroutineState = 22,
    /// A coroutine state's tag niche (`LayoutFacts::coroutine_niche`).
    CoroutineNiche = 23,
}

impl ReadKind {
    const ALL: [ReadKind; 23] = [
        ReadKind::Adt,
        ReadKind::Repr,
        ReadKind::Discriminant,
        ReadKind::ValidRange,
        ReadKind::Normalize,
        ReadKind::OpaqueHidden,
        ReadKind::Const,
        ReadKind::ConstBlock,
        ReadKind::Promoted,
        ReadKind::Static,
        ReadKind::ObjectTrait,
        ReadKind::DropImpl,
        ReadKind::Abi,
        ReadKind::Nounwind,
        ReadKind::TrackCaller,
        ReadKind::Resolve,
        ReadKind::ForeignStatic,
        ReadKind::ForeignFn,
        ReadKind::CVariadic,
        ReadKind::LegacyConstGenerics,
        ReadKind::Laid,
        ReadKind::CoroutineState,
        ReadKind::CoroutineNiche,
    ];

    /// The kind whose byte is `byte`.
    pub fn of(byte: u8) -> Option<ReadKind> {
        ReadKind::ALL.iter().copied().find(|kind| *kind as u8 == byte)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            ReadKind::Adt => "an ADT's definition",
            ReadKind::Repr => "an ADT's repr",
            ReadKind::Discriminant => "a variant's discriminant",
            ReadKind::ValidRange => "a type's valid range",
            ReadKind::Normalize => "a projection's type",
            ReadKind::OpaqueHidden => "an opaque type's hidden type",
            ReadKind::Const => "a constant's value",
            ReadKind::ConstBlock => "a const block's value",
            ReadKind::Promoted => "a promoted operand's value",
            ReadKind::Static => "a static's kind",
            ReadKind::ObjectTrait => "a vtable's shape",
            ReadKind::DropImpl => "a type's Drop impl",
            ReadKind::Abi => "a fn's ABI",
            ReadKind::Nounwind => "a fn's unwinding",
            ReadKind::TrackCaller => "an instance's track_caller",
            ReadKind::Resolve => "the instance a call runs",
            ReadKind::ForeignStatic => "a static's C symbol",
            ReadKind::ForeignFn => "a callee's C symbol",
            ReadKind::CVariadic => "a fn's C-variadic parameter",
            ReadKind::LegacyConstGenerics => "a fn's legacy const generics",
            ReadKind::Laid => "a coroutine's full lay",
            ReadKind::CoroutineState => "a coroutine state's shape",
            ReadKind::CoroutineNiche => "a coroutine state's tag niche",
        }
    }
}

/// Why a read list was not made or not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadsError {
    /// A type in the answer to `kind` of `item` has more than [`TERM_NODES`] nodes.
    TermTooLarge { kind: ReadKind, item: ItemKey },
    /// A type handle of the answer to `kind` of `item` is past its table.
    Malformed { kind: ReadKind, item: ItemKey },
    /// One question asked twice in one build and answered two ways.
    TwoAnswers { kind: ReadKind, item: ItemKey },
    /// A question's instance name did not encode or decode.
    Symbol { kind: ReadKind, item: ItemKey, why: SymbolError },
    /// A stored question of a kind this blaze does not know (an entry of the same form names
    /// only kinds it knows: a corrupt entry).
    Kind { byte: u8 },
    /// A stored question shorter than its kind and scalar.
    Question,
    /// The ground table could not take a question's arguments.
    Full { kind: ReadKind, item: ItemKey },
    /// More than [`READS_CAP`] questions in one build.
    TooMany,
    /// A stored row of `kind`, which the verifier handed in cannot ask today (the whole-program
    /// walk's [`verify`] asks no C symbol; a callee with no row): [`verify_today`] takes the row
    /// as not holding, never as holding.
    Unanswered { kind: ReadKind },
    /// `kind` of `item` was asked before the record it reads (`unmade`: an ADT's head, an
    /// explicit discriminant's value) was made: no answer, so no read list holds it (D194). The
    /// build refused it as a need and makes it; a list sealed past it would be keyed by what
    /// was asked first.
    Unmade { kind: ReadKind, item: ItemKey, unmade: ItemKey },
}

impl fmt::Display for ReadsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "a code entry's read list (blaze-ladder code_reads.rs)";
        match *self {
            ReadsError::TermTooLarge { kind, item } => write!(f, "{rule}: {} of item {:#x} is a type of more than {TERM_NODES} nodes", kind.as_str(), item.0),
            ReadsError::Malformed { kind, item } => write!(f, "{rule}: {} of item {:#x} names a type past its table", kind.as_str(), item.0),
            ReadsError::TwoAnswers { kind, item } => write!(f, "{rule}: {} of item {:#x} was answered two ways in one build", kind.as_str(), item.0),
            ReadsError::Symbol { kind, item, why } => write!(f, "{rule}: {} of item {:#x} has no instance name: {why}", kind.as_str(), item.0),
            ReadsError::Kind { byte } => write!(f, "{rule} holds a question of kind {byte}, which this blaze does not know"),
            ReadsError::Question => write!(f, "{rule} holds a question shorter than its kind and scalar"),
            ReadsError::Full { kind, item } => write!(f, "{rule}: the ground table could not take the arguments of {} of item {:#x}", kind.as_str(), item.0),
            ReadsError::TooMany => write!(f, "{rule}: more than {READS_CAP} questions in one build"),
            ReadsError::Unanswered { kind } => write!(f, "{rule} holds a row of {}, which this walk's verifier does not ask", kind.as_str()),
            ReadsError::Unmade { kind, item, unmade } => {
                write!(f, "{rule}: {} of item {:#x} was asked before the record of item {:#x} it reads was made (D194)", kind.as_str(), item.0, unmade.0)
            }
        }
    }
}

impl core::error::Error for ReadsError {}

/// A bug the read-list tests plant, to show the oracle catches it (rule 23).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadPlant {
    None,
    /// The recorder logs no layout question (`Adt`, `Repr`, `Discriminant`, `ValidRange`): an
    /// entry built over a struct that later gains a field would be found and used.
    SkipsLayouts,
}

/// One question as the build asked it: its arguments still handles of the walk's ground
/// table, its answer already digested.
struct Asked {
    kind: ReadKind,
    item: ItemKey,
    extra: u64,
    args: Vec<PTy, Global>,
    answer: ContentId,
}

/// The questions one build asked ([`Recorded`]), in the order asked.
pub struct ReadLog {
    asked: RefCell<Vec<Asked, Global>>,
    /// The first question whose answer could not be digested: the build's read list is then
    /// refused ([`ReadLog::seal`]), never made short.
    stopped: Cell<Option<ReadsError>>,
    plant: ReadPlant,
}

impl ReadLog {
    pub fn new(plant: ReadPlant) -> ReadLog {
        ReadLog { asked: RefCell::new(Vec::new_in(Global)), stopped: Cell::new(None), plant }
    }

    /// Forgets every question: the log is the next build's.
    pub fn clear(&self) {
        self.asked.borrow_mut().clear();
        self.stopped.set(None);
    }

    /// The count of questions logged.
    pub fn len(&self) -> usize {
        self.asked.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn stop(&self, error: ReadsError) {
        if self.stopped.get().is_none() {
            self.stopped.set(Some(error));
        }
    }

    fn ask(&self, kind: ReadKind, item: ItemKey, extra: u64, args: &[PTy], answer: Result<ContentId, Stop>) {
        if self.plant == ReadPlant::SkipsLayouts && matches!(kind, ReadKind::Adt | ReadKind::Repr | ReadKind::Discriminant | ReadKind::ValidRange) {
            return;
        }
        let answer = match answer {
            Ok(it) => it,
            Err(Stop::TooLarge) => return self.stop(ReadsError::TermTooLarge { kind, item }),
            Err(Stop::Malformed) => return self.stop(ReadsError::Malformed { kind, item }),
            Err(Stop::Unmade(unmade)) => return self.stop(ReadsError::Unmade { kind, item, unmade }),
        };
        let mut asked = self.asked.borrow_mut();
        if asked.len() >= READS_CAP {
            drop(asked);
            return self.stop(ReadsError::TooMany);
        }
        let mut kept = Vec::with_capacity_in(args.len(), Global);
        kept.extend_from_slice(args);
        asked.push(Asked { kind, item, extra, args: kept, answer });
    }

    /// The instance call `called` runs, as the front end's `resolve` answered it (`None`: it
    /// refused), its instance names made from `ground`.
    pub fn ask_resolve(&self, called: Instance, resolved: Option<(ItemKey, List)>, ground: PTys<'_>, scratch: Alloc<'_>) {
        let kind = ReadKind::Resolve;
        let args = match ground.list(called.args) {
            Ok(it) => it,
            Err(_) => return self.stop(ReadsError::Malformed { kind, item: called.item }),
        };
        let mut h = Hasher::new(ANSWER_DOMAIN);
        match resolved {
            None => {
                h.part(&[0]);
            }
            Some((item, list)) => {
                let mut name: AVec<'_, u8> = blaze_own::Vec::new_in(scratch);
                if let Err(why) = encode_instance(item, list, ground, scratch, &mut name) {
                    return self.stop(ReadsError::Symbol { kind, item, why });
                }
                h.part(&[1]).part(&name);
            }
        }
        self.ask(kind, called.item, 0, args, Ok(h.finish()));
    }

    /// Static `item`'s C symbol as the front end answered it (`None`: no `extern` block
    /// declares it, so it has a record of its own).
    pub fn ask_foreign_static(&self, item: ItemKey, c_name: Option<&str>) {
        self.ask(ReadKind::ForeignStatic, item, 0, &[], Ok(symbol_answer(c_name)));
    }

    /// Callee `item`'s C symbol as its row answered it (`None`: it is no `extern` block's fn).
    pub fn ask_foreign_fn(&self, item: ItemKey, symbol: Option<&str>) {
        self.ask(ReadKind::ForeignFn, item, 0, &[], Ok(symbol_answer(symbol)));
    }

    /// Coroutine `ordinal` of `owner` at `args` laid out in full as `laid` digests it
    /// ([`laid_answer`]): the coroutine a part's entry was built into ([`ReadKind::Laid`]).
    pub fn ask_laid(&self, owner: ItemKey, ordinal: u32, args: &[PTy], laid: ContentId) {
        self.ask(ReadKind::Laid, owner, u64::from(ordinal), args, Ok(laid));
    }

    /// Every question `other` logged, logged here as asked (this log's plant applies), and
    /// its stop if it stopped: a coroutine's build is logged apart (run/lay_states.rs
    /// `build_laid`, once for its three functions), and each part's make takes its rows into
    /// its own read list.
    pub fn absorb(&self, other: &ReadLog) {
        if let Some(error) = other.stopped.get() {
            self.stop(error);
        }
        for a in other.asked.borrow().iter() {
            self.ask(a.kind, a.item, a.extra, &a.args, Ok(a.answer));
        }
    }

    /// The answer of the one question logged since the last [`ReadLog::clear`].
    fn only_answer(&self, kind: ReadKind, item: ItemKey) -> Result<ContentId, ReadsError> {
        if let Some(error) = self.stopped.get() {
            return Err(error);
        }
        let asked = self.asked.borrow();
        match asked.as_slice() {
            [one] => Ok(one.answer),
            _ => Err(ReadsError::TwoAnswers { kind, item }),
        }
    }

    /// The build's read list: each question named for good (its arguments interned in
    /// `ground` and named by `encode_instance`), sorted by its bytes, each once. A question
    /// asked twice with two answers is refused, as is a log that stopped.
    pub fn seal(&self, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<ReadList, ReadsError> {
        if let Some(error) = self.stopped.get() {
            return Err(error);
        }
        let asked = self.asked.borrow();
        let mut rows: Vec<(Vec<u8, Global>, ContentId, ReadKind, ItemKey), Global> = Vec::with_capacity_in(asked.len(), Global);
        for a in asked.iter() {
            let list = ground.intern_list(&a.args).map_err(|_| ReadsError::Full { kind: a.kind, item: a.item })?;
            let mut name: AVec<'_, u8> = blaze_own::Vec::new_in(scratch);
            encode_instance(a.item, list, ground.view(), scratch, &mut name).map_err(|why| ReadsError::Symbol { kind: a.kind, item: a.item, why })?;
            let mut question = Vec::with_capacity_in(9 + name.len(), Global);
            question.push(a.kind as u8);
            question.extend_from_slice(&a.extra.to_le_bytes());
            question.extend_from_slice(&name);
            rows.push((question, a.answer, a.kind, a.item));
        }
        rows.sort_unstable_by(|x, y| x.0.cmp(&y.0));
        let mut out: Vec<(Vec<u8, Global>, ContentId), Global> = Vec::with_capacity_in(rows.len(), Global);
        for (question, answer, kind, item) in rows {
            match out.last() {
                Some((last, was)) if *last == question => {
                    if *was != answer {
                        return Err(ReadsError::TwoAnswers { kind, item });
                    }
                }
                _ => out.push((question, answer)),
            }
        }
        Ok(ReadList { rows: out })
    }
}

/// A sealed read list: rows of (question, answer digest), sorted by question, each once.
pub struct ReadList {
    rows: Vec<(Vec<u8, Global>, ContentId), Global>,
}

impl ReadList {
    /// No row: an entry made from no text (the thread-local getter and ender, a builtins
    /// shim) reads nothing beyond its key.
    pub fn empty() -> ReadList {
        ReadList { rows: Vec::new_in(Global) }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Each row, in order.
    pub fn rows(&self) -> impl Iterator<Item = (&[u8], ContentId)> {
        self.rows.iter().map(|(question, answer)| (&question[..], *answer))
    }
}

/// What digesting an answer stopped at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Stop {
    TooLarge,
    Malformed,
    /// The answer reads the record of this item, which is not made yet ([`ReadsError::Unmade`]).
    Unmade(ItemKey),
}

/// A `core::hash::Hasher` that feeds every write to a [`Hasher`] as one part, every integer
/// little-endian at a fixed width (`usize` and `isize` as 8 bytes): a derived `Hash` gives
/// the same digest on every machine.
struct Fed<'h>(&'h mut Hasher);

impl core::hash::Hasher for Fed<'_> {
    /// Never asked by a derived `Hash`; the parts written are the value.
    fn finish(&self) -> u64 {
        0
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.part(bytes);
    }
    fn write_u8(&mut self, v: u8) {
        self.0.part(&[v]);
    }
    fn write_u16(&mut self, v: u16) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_u32(&mut self, v: u32) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_u64(&mut self, v: u64) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_u128(&mut self, v: u128) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_usize(&mut self, v: usize) {
        self.0.part(&(v as u64).to_le_bytes());
    }
    fn write_i8(&mut self, v: i8) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_i16(&mut self, v: i16) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_i32(&mut self, v: i32) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_i64(&mut self, v: i64) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_i128(&mut self, v: i128) {
        self.0.part(&v.to_le_bytes());
    }
    fn write_isize(&mut self, v: isize) {
        self.0.part(&(v as i64).to_le_bytes());
    }
}

/// `list`'s children pushed so the first is popped first; its length.
fn kids(stack: &mut Vec<PTy, Global>, types: PTys<'_>, list: List) -> Result<u32, Stop> {
    let all = types.list(list).map_err(|_| Stop::Malformed)?;
    stack.extend(all.iter().rev().copied());
    Ok(all.len() as u32)
}

/// Type `t` of `types` into `h` by its structure, pre-order: each node's tag, its scalars and
/// its lists' lengths, then its children. No handle of the table is written, so one type in
/// two tables (or two runs) digests the same; at most [`TERM_NODES`] nodes.
pub(crate) fn term(h: &mut Hasher, t: PTy, types: PTys<'_>) -> Result<(), Stop> {
    let mut stack: Vec<PTy, Global> = Vec::new_in(Global);
    stack.push(t);
    let mut seen = 0usize;
    while let Some(t) = stack.pop() {
        seen += 1;
        if seen > TERM_NODES || stack.len() > TERM_NODES {
            return Err(Stop::TooLarge);
        }
        let node = types.node(t).map_err(|_| Stop::Malformed)?;
        let f = &mut Fed(h);
        match node {
            PNode::Bool => 0u8.hash(f),
            PNode::Char => 1u8.hash(f),
            PNode::Int(it) => (2u8, it).hash(f),
            PNode::Uint(it) => (3u8, it).hash(f),
            PNode::Float(it) => (4u8, it).hash(f),
            PNode::Str => 5u8.hash(f),
            PNode::Never => 6u8.hash(f),
            PNode::Tuple(l) => {
                let n = kids(&mut stack, types, l)?;
                (7u8, n).hash(&mut Fed(h));
            }
            PNode::Ref(m, p) => {
                (8u8, m).hash(f);
                stack.push(p);
            }
            PNode::Ptr(m, p) => {
                (9u8, m).hash(f);
                stack.push(p);
            }
            PNode::Array(elem, len) => {
                10u8.hash(f);
                stack.push(len);
                stack.push(elem);
            }
            PNode::Slice(p) => {
                11u8.hash(f);
                stack.push(p);
            }
            PNode::Pat(base, ranges) => {
                let n = kids(&mut stack, types, ranges)?;
                stack.push(base);
                (12u8, n).hash(&mut Fed(h));
            }
            PNode::Adt(key, l) => {
                let n = kids(&mut stack, types, l)?;
                (13u8, key, n).hash(&mut Fed(h));
            }
            PNode::FnDef(key, l) => {
                let n = kids(&mut stack, types, l)?;
                (14u8, key, n).hash(&mut Fed(h));
            }
            PNode::FnPtr(l, header) => {
                let n = kids(&mut stack, types, l)?;
                (15u8, header, n).hash(&mut Fed(h));
            }
            PNode::Closure { owner, ordinal, args } => {
                let n = kids(&mut stack, types, args)?;
                (16u8, owner, ordinal, n).hash(&mut Fed(h));
            }
            PNode::Future(expr, output, interior) => {
                (17u8, expr).hash(f);
                stack.push(interior);
                stack.push(output);
            }
            PNode::Coroutine { owner, ordinal, args } => {
                let n = kids(&mut stack, types, args)?;
                (18u8, owner, ordinal, n).hash(&mut Fed(h));
            }
            PNode::Param(i) => (19u8, i).hash(f),
            PNode::Alias { trait_, name, args } => {
                let n = kids(&mut stack, types, args)?;
                (20u8, trait_, name, n).hash(&mut Fed(h));
            }
            PNode::Opaque(key, l) => {
                let n = kids(&mut stack, types, l)?;
                (21u8, key, n).hash(&mut Fed(h));
            }
            PNode::Infer => 22u8.hash(f),
            PNode::Error => 23u8.hash(f),
            PNode::ConstValue { ty, bits } => {
                (24u8, bits).hash(f);
                stack.push(ty);
            }
            PNode::ConstParam(i) => (25u8, i).hash(f),
            PNode::Dynamic(l) => {
                let n = kids(&mut stack, types, l)?;
                (26u8, n).hash(&mut Fed(h));
            }
            PNode::ExTrait(key, l) => {
                let n = kids(&mut stack, types, l)?;
                (27u8, key, n).hash(&mut Fed(h));
            }
            PNode::ExProj { trait_, name, args, term } => {
                stack.push(term);
                let n = kids(&mut stack, types, args)?;
                (28u8, trait_, name, n).hash(&mut Fed(h));
            }
            PNode::ExAuto(key) => (29u8, key).hash(f),
            // An extern type, by its item: a tag no earlier node had.
            PNode::Foreign(key) => (30u8, key).hash(f),
            PNode::Record(s, l) => {
                let n = kids(&mut stack, types, l)?;
                (31u8, s.0, n).hash(&mut Fed(h));
            }
            PNode::Union(l) => {
                let n = kids(&mut stack, types, l)?;
                (32u8, n).hash(&mut Fed(h));
            }
        }
    }
    Ok(())
}

fn digest(fill: impl FnOnce(&mut Hasher) -> Result<(), Stop>) -> Result<ContentId, Stop> {
    let mut h = Hasher::new(ANSWER_DOMAIN);
    fill(&mut h)?;
    Ok(h.finish())
}

fn adt_answer(adt: Option<Adt<'_>>) -> Result<ContentId, Stop> {
    digest(|h| {
        let Some(adt) = adt else {
            h.part(&[0]);
            return Ok(());
        };
        (1u8, adt.kind, adt.generics, adt.variants.len()).hash(&mut Fed(h));
        for v in adt.variants {
            (v.key, v.name, v.ctor, v.fields.len).hash(&mut Fed(h));
        }
        adt.fields.len().hash(&mut Fed(h));
        for field in adt.fields {
            field.name.hash(&mut Fed(h));
            term(h, field.ty, adt.types)?;
        }
        Ok(())
    })
}

fn repr_answer(r: ReprAttrs) -> ContentId {
    let mut h = Hasher::new(ANSWER_DOMAIN);
    let int = match r.int {
        None => (0u8, 0u8),
        Some(IntRepr::Signed(t)) => (1, t as u8),
        Some(IntRepr::Unsigned(t)) => (2, t as u8),
    };
    (r.c, int, r.transparent, r.packed, r.align, r.simd, r.hide_niche).hash(&mut Fed(&mut h));
    h.finish()
}

fn ty_answer(answer: &Result<PTy, BuildError>, ground: PTys<'_>) -> Result<ContentId, Stop> {
    digest(|h| match *answer {
        Ok(t) => {
            h.part(&[1]);
            term(h, t, ground)
        }
        Err(_) => {
            h.part(&[0]);
            Ok(())
        }
    })
}

fn data_answer(value: Option<&DataRecord<Global>>) -> ContentId {
    let mut h = Hasher::new(ANSWER_DOMAIN);
    match value {
        None => {
            h.part(&[0]);
        }
        Some(record) => {
            (1u8, record.allocs.len()).hash(&mut Fed(&mut h));
            for alloc in record.allocs.iter() {
                h.part(&alloc.bytes);
                (alloc.align, alloc.mutable, alloc.relocs.len()).hash(&mut Fed(&mut h));
                for reloc in alloc.relocs.iter() {
                    (reloc.offset, reloc.target).hash(&mut Fed(&mut h));
                }
            }
        }
    }
    h.finish()
}

/// A coroutine's full lay as one answer ([`ReadKind::Laid`]): its state's shape, the region
/// its flow's saved locals take, its tag's niche, and each saved local's identity, offset,
/// size and drop flag (sorted by identity: run/lay_states.rs `lay_one`). What a part's code
/// was built into, so two lays equal here build equal parts.
pub fn laid_answer(laid: &LaidState, names: &[StateName]) -> ContentId {
    let mut h = Hasher::new(ANSWER_DOMAIN);
    let f = &mut Fed(&mut h);
    shape_parts(laid.shape).hash(f);
    (laid.region.start, laid.region.end, laid.region.align).hash(f);
    niche_parts(laid.niche).hash(f);
    names.len().hash(f);
    for n in names {
        (n.identity, n.offset, n.size, n.flag).hash(f);
    }
    h.finish()
}

fn shape_parts(s: StateShape) -> (u32, u8, u32, u16) {
    (s.size, s.align, s.tag_offset, s.tag_ty.0)
}

fn niche_parts(n: StateNiche) -> (u32, u16, u32) {
    (n.tag_offset, n.tag_ty.0, n.valid_end)
}

/// `LayoutFacts::coroutine_state`'s answer ([`ReadKind::CoroutineState`]).
/// A state not laid yet is no answer: a need ([`Stop::Unmade`]), never a digest.
fn state_answer(answer: Result<Option<StateShape>, Missing>) -> Result<ContentId, Stop> {
    match answer {
        Ok(None) => Ok(small(0u8)),
        Ok(Some(s)) => Ok(small((1u8, shape_parts(s)))),
        Err(missing) => Err(Stop::Unmade(missing.item)),
    }
}

/// `LayoutFacts::coroutine_niche`'s answer ([`ReadKind::CoroutineNiche`]), as [`state_answer`].
fn niche_answer(answer: Result<Option<StateNiche>, Missing>) -> Result<ContentId, Stop> {
    match answer {
        Ok(None) => Ok(small(0u8)),
        Ok(Some(n)) => Ok(small((1u8, niche_parts(n)))),
        Err(missing) => Err(Stop::Unmade(missing.item)),
    }
}

fn small(value: impl Hash) -> ContentId {
    let mut h = Hasher::new(ANSWER_DOMAIN);
    value.hash(&mut Fed(&mut h));
    h.finish()
}

fn symbol_answer(symbol: Option<&str>) -> ContentId {
    let mut h = Hasher::new(ANSWER_DOMAIN);
    match symbol {
        None => h.part(&[0]),
        Some(s) => h.part(&[1]).part(s.as_bytes()),
    };
    h.finish()
}

/// The world handed to the builder, recorded ([`ReadLog`]; the module doc says which methods
/// are).
pub struct Recorded<'w> {
    world: &'w dyn Adts,
    log: &'w ReadLog,
}

impl<'w> Recorded<'w> {
    pub fn new(world: &'w dyn Adts, log: &'w ReadLog) -> Recorded<'w> {
        Recorded { world, log }
    }
}

impl LayoutFacts for Recorded<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let answer = self.world.facts().adt(key);
        self.log.ask(ReadKind::Adt, key, 0, &[], adt_answer(answer));
        answer
    }

    /// A `Missing` is no answer: the log stops ([`ReadsError::Unmade`]), so the list is never
    /// sealed with it, and the build's refusal makes the record (D194).
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
        let answer = self.world.facts().repr(key);
        self.log.ask(ReadKind::Repr, key, 0, &[], answer.map(repr_answer).map_err(|m| Stop::Unmade(m.item)));
        answer
    }

    /// A `Missing` as [`Self::repr`]'s: never digested as an answer.
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        let answer = self.world.facts().discriminant(key, variant);
        let digest = match answer {
            Ok(None) => Ok(small(0u8)),
            Ok(Some(v)) => Ok(small((1u8, v))),
            Err(missing) => Err(Stop::Unmade(missing.item)),
        };
        self.log.ask(ReadKind::Discriminant, key, u64::from(variant), &[], digest);
        answer
    }

    /// A `Missing` as [`Self::repr`]'s.
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> {
        let answer = self.world.facts().valid_range(key);
        self.log.ask(ReadKind::ValidRange, key, 0, &[], answer.map(|r| small((r.start, r.end))).map_err(|m| Stop::Unmade(m.item)));
        answer
    }

    fn normalize(&self, trait_: ItemKey, name: Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        let answer = self.world.facts().normalize(trait_, name, args, ground);
        self.log.ask(ReadKind::Normalize, trait_, name.0, args, ty_answer(&answer, ground.view()));
        answer
    }

    fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<ClosureFacts<'_>> {
        self.world.facts().closure(owner, ordinal)
    }

    fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::CoroutineFacts<'_>> {
        self.world.facts().coroutine(owner, ordinal)
    }

    fn coroutine_state(&self, owner: ItemKey, ordinal: u32, args: &[PTy], ground: PTys<'_>) -> Result<Option<blaze_build::StateShape>, Missing> {
        let answer = self.world.facts().coroutine_state(owner, ordinal, args, ground);
        self.log.ask(ReadKind::CoroutineState, owner, u64::from(ordinal), args, state_answer(answer));
        answer
    }

    fn coroutine_niche(&self, owner: ItemKey, ordinal: u32, args: &[PTy], ground: PTys<'_>) -> Result<Option<blaze_build::StateNiche>, Missing> {
        let answer = self.world.facts().coroutine_niche(owner, ordinal, args, ground);
        self.log.ask(ReadKind::CoroutineNiche, owner, u64::from(ordinal), args, niche_answer(answer));
        answer
    }

    /// Named by the head that made the opaque, `(owner, index)` (`Adts::opaque_of`), never by
    /// the opaque's own key: a fresh program knows an opaque only once that head is made (a fn
    /// declared in a body, core's `for_each`'s inner `call`, is made only when asked), and from
    /// the row the verifier makes it, by name, before it asks (make_one.rs `verify_instance`).
    /// Asked with no such head, the read is of a record not made: `Unmade` (D194).
    fn opaque_hidden(&self, opaque: ItemKey, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        let answer = self.world.facts().opaque_hidden(opaque, args, ground);
        match self.world.opaque_of(opaque) {
            Some((owner, index)) => self.log.ask(ReadKind::OpaqueHidden, owner, u64::from(index), args, ty_answer(&answer, ground.view())),
            None => self.log.ask(ReadKind::OpaqueHidden, opaque, 0, args, Err(Stop::Unmade(opaque))),
        }
        answer
    }
}

impl Adts for Recorded<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let answer = self.world.adt(key);
        self.log.ask(ReadKind::Adt, key, 0, &[], adt_answer(answer));
        answer
    }

    // No row: it names the `OpaqueHidden` question itself (`Recorded::opaque_hidden`).
    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
        self.world.opaque_of(opaque)
    }

    fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        self.world.int_lit(owner, expr)
    }

    fn int_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        self.world.int_pat(owner, pat)
    }

    fn float_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        self.world.float_pat(owner, pat)
    }

    fn str_pat(&self, owner: ItemKey, pat: PatId) -> Option<&[u8]> {
        self.world.str_pat(owner, pat)
    }

    fn float_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        self.world.float_lit(owner, expr)
    }

    fn byte_str(&self, owner: ItemKey, expr: ExprId) -> Option<&[u8]> {
        self.world.byte_str(owner, expr)
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        self.world.lang_fn(name)
    }

    fn type_names(&self) -> Option<&dyn blaze_build::type_name::TypeNames> {
        self.world.type_names()
    }

    fn panic(&self) -> blaze_build::Panic {
        self.world.panic()
    }

    fn nounwind(&self, owner: ItemKey) -> Option<bool> {
        let answer = self.world.nounwind(owner);
        self.log.ask(ReadKind::Nounwind, owner, 0, &[], Ok(small(answer)));
        answer
    }

    fn fn_attrs(&self, owner: ItemKey) -> blaze_lir::func::FnAttrs {
        self.world.fn_attrs(owner)
    }

    fn overflow_checks(&self) -> blaze_build::OverflowChecks {
        self.world.overflow_checks()
    }

    fn ub_checks(&self) -> blaze_build::UbChecks {
        self.world.ub_checks()
    }

    fn fn_trait_method(&self, which: blaze_build::FnTrait) -> Option<ItemKey> {
        self.world.fn_trait_method(which)
    }

    fn future_poll(&self) -> Option<ItemKey> {
        self.world.future_poll()
    }

    fn async_items(&self) -> Option<blaze_build::AsyncItems> {
        self.world.async_items()
    }

    fn deref_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        self.world.deref_fn(mutbl)
    }

    fn index_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        self.world.index_fn(mutbl)
    }

    fn lang_adt(&self, item: LangItem) -> Option<ItemKey> {
        self.world.lang_adt(item)
    }

    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        let answer = self.world.object_trait(t);
        let digest = match &answer {
            None => small(0u8),
            Some(it) => small((1u8, it.shape.0)),
        };
        self.log.ask(ReadKind::ObjectTrait, t, 0, &[], Ok(digest));
        answer
    }

    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        let answer = self.world.drop_impl(ty, ground);
        let digest = match &answer {
            Ok(None) => small(0u8),
            Ok(Some(item)) => small((1u8, *item)),
            Err(_) => small(2u8),
        };
        self.log.ask(ReadKind::DropImpl, ItemKey(0), 0, &[ty], Ok(digest));
        answer
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        let answer = self.world.abi(item);
        let byte: u8 = match answer {
            None => 0,
            Some(Abi::Rust) => 1,
            Some(Abi::C) => 2,
            Some(Abi::RustCall) => 3,
        };
        self.log.ask(ReadKind::Abi, item, 0, &[], Ok(small(byte)));
        answer
    }

    fn c_variadic(&self, item: ItemKey) -> Option<u32> {
        let answer = self.world.c_variadic(item);
        self.log.ask(ReadKind::CVariadic, item, 0, &[], Ok(small(answer)));
        answer
    }

    fn legacy_const_generics(&self, item: ItemKey) -> &[u32] {
        let answer = self.world.legacy_const_generics(item);
        self.log.ask(ReadKind::LegacyConstGenerics, item, 0, &[], Ok(small(answer)));
        answer
    }

    fn str_lit(&self, owner: ItemKey, expr: ExprId) -> Option<&str> {
        self.world.str_lit(owner, expr)
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        self.world.intrinsic(item)
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn static_item(&self, item: ItemKey) -> Option<StaticItem> {
        let answer = self.world.static_item(item);
        let digest = match answer {
            None => small(0u8),
            Some(s) => small((1u8, s.thread_local, s.mutable)),
        };
        self.log.ask(ReadKind::Static, item, 0, &[], Ok(digest));
        answer
    }

    fn const_value(&self, item: ItemKey, args: &[PTy]) -> Option<&DataRecord<Global>> {
        let answer = self.world.const_value(item, args);
        self.log.ask(ReadKind::Const, item, 0, args, Ok(data_answer(answer)));
        answer
    }

    fn const_block_value(&self, owner: ItemKey, block: ExprId, args: &[PTy]) -> Option<&DataRecord<Global>> {
        let answer = self.world.const_block_value(owner, block, args);
        self.log.ask(ReadKind::ConstBlock, owner, u64::from(block.0), args, Ok(data_answer(answer)));
        answer
    }

    fn promoted_value(&self, owner: ItemKey, borrow: ExprId, args: &[PTy]) -> Option<&DataRecord<Global>> {
        let answer = self.world.promoted_value(owner, borrow, args);
        self.log.ask(ReadKind::Promoted, owner, u64::from(borrow.0), args, Ok(data_answer(answer)));
        answer
    }

    fn const_fn_instance(&self, key: InstanceKey, ground: &mut PortableTys<'_>) -> Option<Instance> {
        self.world.const_fn_instance(key, ground)
    }

    fn format_template(&self, owner: ItemKey, expr: ExprId) -> Option<&[blaze_build::FmtPiece<'_>]> {
        self.world.format_template(owner, expr)
    }

    fn fmt_argument_new(&self, trait_: blaze_typeck::FmtTrait) -> Option<ItemKey> {
        self.world.fmt_argument_new(trait_)
    }

    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        self.world.fmt_argument_adt()
    }

    fn track_caller(&self, instance: Instance, ground: &mut PortableTys<'_>) -> Option<bool> {
        let answer = self.world.track_caller(instance, ground);
        match ground.view().list(instance.args) {
            Ok(args) => self.log.ask(ReadKind::TrackCaller, instance.item, 0, args, Ok(small(answer))),
            Err(_) => self.log.stop(ReadsError::Malformed { kind: ReadKind::TrackCaller, item: instance.item }),
        }
        answer
    }

    fn caller_site(&self, owner: ItemKey, expr: ExprId) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
        self.world.caller_site(owner, expr)
    }

    /// Not logged, as `caller_site`: the builder asks it only of the body's own owner (a
    /// `Fn*` shim's `Fn*` method, blaze-build walk/track_caller.rs `shim_caller_arg`), so the
    /// site is of the item the record is named by and item-relative (rule 71).
    fn def_site(&self, item: ItemKey) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
        self.world.def_site(item)
    }

    fn expr_site(&self, owner: ItemKey, expr: ExprId) -> Option<u32> {
        self.world.expr_site(owner, expr)
    }

    fn location_adt(&self) -> Option<ItemKey> {
        self.world.location_adt()
    }
}

/// Whether a found entry's read list still holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Every row asked again and answered the same: the entry is today's.
    Holds,
    /// The first row answered otherwise: the entry is stale and is built again.
    Stale { kind: ReadKind, item: ItemKey },
    /// The first row not held could not be answered: `kind` of `item` reads the record of
    /// `unmade`, which is not made yet (a lazy front noted it: [`ReadsError::Unmade`]). A NEED,
    /// not a difference: the verifier makes what was noted and asks again (run/make_one.rs
    /// `verify_instance`); a record that cannot be made today (its item gone) leaves the entry
    /// stale. Nothing from such a round is kept in [`Answers`].
    Needs { kind: ReadKind, item: ItemKey, unmade: ItemKey },
}

/// The questions of a read list that are not `Adts` questions, answered by the walk's front
/// end as the build asked them: a call's instance (`Resolve`) and the C symbols
/// (`ForeignStatic`, `ForeignFn`).
pub trait Beyond {
    /// The instance a call of `item` at `args` runs; `None` where the front end refuses it.
    fn resolve(&mut self, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Option<(ItemKey, List)>;
    /// Static `item`'s C symbol (`None`: it has a record of its own).
    fn foreign_static(&mut self, item: ItemKey) -> Result<Option<&str>, ReadsError>;
    /// Callee `item`'s C symbol (`None`: it is no `extern` block's fn).
    fn foreign_fn(&mut self, item: ItemKey) -> Result<Option<&str>, ReadsError>;
    /// Coroutine `ordinal` of `owner` at `args` (a list of the verifier's ground table) laid
    /// out in full again, digested as the build digested it ([`laid_answer`]);
    /// [`ReadsError::Unanswered`] where it was not laid today.
    fn laid(&mut self, owner: ItemKey, ordinal: u32, args: List) -> Result<ContentId, ReadsError>;
}

/// The answers one run found holding, by question (a row's bytes, which hold no table handle),
/// sorted: a question many entries ask (one ADT every instance over it lays out) is asked of
/// the front end once a run, each later row a binary search (the "batch by fact" of the read
/// list's cost). Only an answer equal to a stored row's is kept: a run's facts do not move
/// under it, so it is today's for every entry.
pub struct Answers<'a> {
    rows: AVec<'a, (AVec<'a, u8>, ContentId)>,
    alloc: Alloc<'a>,
}

impl<'a> Answers<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Answers<'a> {
        Answers { rows: blaze_own::Vec::new_in(alloc), alloc }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn get(&self, question: &[u8]) -> Option<ContentId> {
        self.rows.binary_search_by(|row| row.0[..].cmp(question)).ok().map(|at| self.rows[at].1)
    }

    fn keep(&mut self, question: &[u8], answer: ContentId) {
        if let Err(at) = self.rows.binary_search_by(|row| row.0[..].cmp(question)) {
            let mut q = blaze_own::Vec::with_capacity_in(question.len(), self.alloc);
            q.extend_from_slice(question);
            self.rows.insert_shift(at, (q, answer));
        }
    }
}

/// A stored row's question read back: its kind, its scalar (a variant, a name, an expression),
/// and its item and arguments decoded into `ground`. The one reader of a row's bytes.
pub fn question(bytes: &[u8], ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<(ReadKind, u64, ItemKey, List), ReadsError> {
    let (kind, extra, name) = question_head(bytes)?;
    let (item, list) = decode_instance(name, ground, scratch).map_err(|why| ReadsError::Symbol { kind, item: ItemKey(0), why })?;
    Ok((kind, extra, item, list))
}

/// A row's kind and scalar, and the bytes of its instance name, nothing decoded.
fn question_head(bytes: &[u8]) -> Result<(ReadKind, u64, &[u8]), ReadsError> {
    let (&byte, rest) = bytes.split_first().ok_or(ReadsError::Question)?;
    let kind = ReadKind::of(byte).ok_or(ReadsError::Kind { byte })?;
    let (extra, name) = rest.split_first_chunk::<8>().ok_or(ReadsError::Question)?;
    Ok((kind, u64::from_le_bytes(*extra), name))
}

/// What one verification did ([`verify_today`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verified {
    pub verdict: Verdict,
    /// Rows asked of the front end.
    pub asked: u32,
    /// Rows answered by [`Answers`], asked of no one.
    pub known: u32,
}

/// Asks each row of `entry`'s read list again through `world` (the same world the build is
/// handed) and `beyond`, its arguments decoded into `ground`; a row `known` holds is answered
/// there. With `each`, every row is asked though one differed (so a lazy front end notes every
/// head the list reads unmade, made in one round: run/make_one.rs `verify_instance`); without,
/// it stops at the first. A [`ReadKind::Laid`] row is answered by `beyond` laying the
/// coroutine again; its `CoroutineState` and `CoroutineNiche` rows by `world`, which holds the
/// states the caller laid again first. A row `beyond` cannot answer today
/// ([`ReadsError::Unanswered`]) does not hold. The
/// verdict names the first row answered otherwise, in the list's order.
///
/// `known` takes this entry's answers only when EVERY row held: a lazy front end answers some
/// questions with a default while a head is unmade (its `repr`, `valid_range` and
/// `discriminant`), and such an answer met in a round that did not hold is never kept for a
/// later entry or round.
#[allow(clippy::too_many_arguments)]
pub fn verify_today(
    entry: &EntryView<'_>,
    world: &dyn Adts,
    beyond: &mut dyn Beyond,
    known: &mut Answers<'_>,
    each: bool,
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<Verified, ReadsError> {
    let log = ReadLog::new(ReadPlant::None);
    let recorded = Recorded::new(world, &log);
    let mut args: AVec<'_, PTy> = blaze_own::Vec::new_in(scratch);
    let mut out = Verified { verdict: Verdict::Holds, asked: 0, known: 0 };
    // This entry's rows answered the same today, kept in `known` once every row held.
    let mut held: AVec<'_, (&[u8], ContentId)> = blaze_own::Vec::new_in(scratch);
    for row in entry.reads() {
        let (question, answer) = row.map_err(|_| ReadsError::Question)?;
        let (kind, extra, name) = question_head(question)?;
        // A row answered from `known` or a `Laid` row is named by no item: its name is not
        // decoded (that is the saving).
        let stale = |item: ItemKey, out: &mut Verified| {
            if out.verdict == Verdict::Holds {
                out.verdict = Verdict::Stale { kind, item };
            }
        };
        if let Some(today) = known.get(question) {
            out.known += 1;
            if today != answer {
                stale(ItemKey(0), &mut out);
                if !each {
                    return Ok(out);
                }
            }
            continue;
        }
        let (item, list) = decode_instance(name, ground, scratch).map_err(|why| ReadsError::Symbol { kind, item: ItemKey(0), why })?;
        args.clear();
        args.extend_from_slice(ground.view().list(list).map_err(|_| ReadsError::Malformed { kind, item })?);
        log.clear();
        out.asked += 1;
        match kind {
            ReadKind::Laid => {
                let ordinal = u32::try_from(extra).map_err(|_| ReadsError::Question)?;
                match beyond.laid(item, ordinal, list) {
                    Ok(laid) => log.ask_laid(item, ordinal, &args, laid),
                    // Not laid again today (its owner refused, a lay refused): the row does not
                    // hold, and the make that follows lays it and meets the refusal by name.
                    Err(ReadsError::Unanswered { .. }) => {
                        stale(item, &mut out);
                        if !each {
                            return Ok(out);
                        }
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            ReadKind::CoroutineState | ReadKind::CoroutineNiche => {
                let ordinal = u32::try_from(extra).map_err(|_| ReadsError::Question)?;
                match kind {
                    ReadKind::CoroutineState => {
                        let _ = recorded.coroutine_state(item, ordinal, &args, ground.view());
                    }
                    _ => {
                        let _ = recorded.coroutine_niche(item, ordinal, &args, ground.view());
                    }
                }
            }
            ReadKind::ForeignStatic | ReadKind::ForeignFn => {
                let today = match kind {
                    ReadKind::ForeignStatic => beyond.foreign_static(item),
                    _ => beyond.foreign_fn(item),
                };
                match today {
                    Ok(symbol) if kind == ReadKind::ForeignStatic => log.ask_foreign_static(item, symbol),
                    Ok(symbol) => log.ask_foreign_fn(item, symbol),
                    // Not answerable today: the row does not hold, and the make that follows
                    // meets the question itself.
                    Err(ReadsError::Unanswered { .. }) => {
                        stale(item, &mut out);
                        if !each {
                            return Ok(out);
                        }
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            ReadKind::CVariadic => {
                recorded.c_variadic(item);
            }
            ReadKind::LegacyConstGenerics => {
                recorded.legacy_const_generics(item);
            }
            ReadKind::Adt => {
                LayoutFacts::adt(&recorded, item);
            }
            ReadKind::Repr => {
                let _ = recorded.repr(item);
            }
            ReadKind::Discriminant => {
                let variant = u32::try_from(extra).map_err(|_| ReadsError::Question)?;
                let _ = recorded.discriminant(item, variant);
            }
            ReadKind::ValidRange => {
                let _ = recorded.valid_range(item);
            }
            ReadKind::Normalize => {
                let _ = recorded.normalize(item, Name(extra), &args, ground);
            }
            // Named by its owner row (`Recorded::opaque_hidden`): the opaque is that row's.
            ReadKind::OpaqueHidden => {
                let index = u32::try_from(extra).map_err(|_| ReadsError::Question)?;
                let _ = recorded.opaque_hidden(crate::front2::keys::opaque_of_row(item, index), &args, ground);
            }
            ReadKind::Const => {
                recorded.const_value(item, &args);
            }
            ReadKind::ConstBlock | ReadKind::Promoted => {
                let expr = ExprId(u32::try_from(extra).map_err(|_| ReadsError::Question)?);
                match kind {
                    ReadKind::ConstBlock => recorded.const_block_value(item, expr, &args),
                    _ => recorded.promoted_value(item, expr, &args),
                };
            }
            ReadKind::Static => {
                recorded.static_item(item);
            }
            ReadKind::ObjectTrait => {
                recorded.object_trait(item);
            }
            ReadKind::DropImpl => {
                let &[ty] = args.as_slice() else { return Err(ReadsError::Question) };
                let _ = recorded.drop_impl(ty, ground);
            }
            ReadKind::Abi => {
                recorded.abi(item);
            }
            ReadKind::Nounwind => {
                recorded.nounwind(item);
            }
            ReadKind::TrackCaller => {
                recorded.track_caller(Instance { item, args: list }, ground);
            }
            ReadKind::Resolve => {
                let resolved = beyond.resolve(item, list, ground);
                log.ask_resolve(Instance { item, args: list }, resolved, ground.view(), scratch);
            }
        }
        let today = match log.only_answer(kind, item) {
            Ok(today) => today,
            // Asked before the record it reads was made: no answer to compare, a need (a lazy
            // front noted it for `make_asked`), never a digest of a default (D194).
            Err(ReadsError::Unmade { kind, item, unmade }) => {
                if out.verdict == Verdict::Holds {
                    out.verdict = Verdict::Needs { kind, item, unmade };
                }
                if !each {
                    return Ok(out);
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        if today == answer {
            held.push((question, today));
        } else {
            stale(item, &mut out);
            if !each {
                return Ok(out);
            }
        }
    }
    if out.verdict == Verdict::Holds {
        for &(question, answer) in held.iter() {
            known.keep(question, answer);
        }
    }
    Ok(out)
}

/// [`verify_today`] for the whole-program walk (run.rs `build_from`): `resolve` answers a
/// `Resolve` row as that walk resolves a call; a C symbol's row does not hold
/// ([`ReadsError::Unanswered`]: that walk records none). Stops at the first row answered
/// otherwise; nothing is kept between entries.
pub fn verify(
    entry: &EntryView<'_>,
    world: &dyn Adts,
    resolve: &mut dyn FnMut(ItemKey, List, &mut PortableTys<'_>) -> Option<(ItemKey, List)>,
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<Verdict, ReadsError> {
    struct Calls<'r>(&'r mut dyn FnMut(ItemKey, List, &mut PortableTys<'_>) -> Option<(ItemKey, List)>);
    impl Beyond for Calls<'_> {
        fn resolve(&mut self, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Option<(ItemKey, List)> {
            (self.0)(item, args, ground)
        }
        fn foreign_static(&mut self, _: ItemKey) -> Result<Option<&str>, ReadsError> {
            Err(ReadsError::Unanswered { kind: ReadKind::ForeignStatic })
        }
        fn foreign_fn(&mut self, _: ItemKey) -> Result<Option<&str>, ReadsError> {
            Err(ReadsError::Unanswered { kind: ReadKind::ForeignFn })
        }
        // That walk lays no state again before it verifies: a part's entry is made again.
        fn laid(&mut self, _: ItemKey, _: u32, _: List) -> Result<ContentId, ReadsError> {
            Err(ReadsError::Unanswered { kind: ReadKind::Laid })
        }
    }
    let mut known = Answers::new_in(scratch);
    verify_today(entry, world, &mut Calls(resolve), &mut known, false, ground, scratch).map(|v| v.verdict)
}
