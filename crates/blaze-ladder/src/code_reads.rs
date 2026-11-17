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
//! - `Normalize` (trait, name, arguments) and `OpaqueHidden` (opaque, arguments): the type
//!   a projection or an `impl Trait` is at the instance, by structure.
//! - `Const` (item, arguments), `ConstBlock` and `Promoted` (owner, expression, arguments):
//!   the value's bytes, alignment, mutability and relocations, every allocation.
//! - `Static` (item): thread-local and mutable.
//! - `ObjectTrait` (trait): the vtable shape's own digest (`ObjectTrait::shape`).
//! - `DropImpl` (type): the `Drop` impl the type's glue calls.
//! - `Abi`, `Nounwind` (item) and `TrackCaller` (instance).
//! - `Resolve` (a call's instance): the instance that runs, the impl a trait's fn is selected
//!   to. Not an `Adts` question: the walk (run.rs `build_from`) asks the front end's `resolve`
//!   after the build, and logs it through [`ReadLog::ask_resolve`].
//!
//! FORWARDED, NOT RECORDED: literals, patterns, string and format pieces, and caller sites of
//! a body (`int_lit`, `int_pat`, `float_pat`, `str_pat`, `float_lit`, `byte_str`, `str_lit`,
//! `format_template`, `caller_site`: the owner's text, in the key's body digest when the owner
//! is the instance's own fn); lang items and the run's settings (`lang_fn`, `fn_trait_method`,
//! `future_poll`, `async_items`, `deref_fn`, `index_fn`, `lang_adt`, `fmt_argument_new`,
//! `fmt_argument_adt`, `location_adt`, `panic`, `overflow_checks`, `type_names`); the
//! signature attributes `c_variadic`, `legacy_const_generics` and `intrinsic`; instance keys
//! (`promoted_fn_key`, `const_fn_instance`); and the closure and coroutine facts (`closure`,
//! `coroutine`, `coroutine_state`, `coroutine_niche`). A lang item or attribute edited in a
//! crate of the workspace (a `no_core` program's) is not caught by the list; neither is a
//! closure built in an owner's body other than the instance's own fn.
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
use blaze_build::{Abi, Adts, BuildError, Instance, InstanceKey, Intrinsic, StaticItem};
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
}

impl ReadKind {
    const ALL: [ReadKind; 16] = [
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
enum Stop {
    TooLarge,
    Malformed,
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
fn term(h: &mut Hasher, t: PTy, types: PTys<'_>) -> Result<(), Stop> {
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

fn small(value: impl Hash) -> ContentId {
    let mut h = Hasher::new(ANSWER_DOMAIN);
    value.hash(&mut Fed(&mut h));
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

    fn repr(&self, key: ItemKey) -> ReprAttrs {
        let answer = self.world.facts().repr(key);
        self.log.ask(ReadKind::Repr, key, 0, &[], Ok(repr_answer(answer)));
        answer
    }

    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        let answer = self.world.facts().discriminant(key, variant);
        let digest = match answer {
            Ok(None) => small(0u8),
            Ok(Some(v)) => small((1u8, v)),
            Err(missing) => small((2u8, missing.item)),
        };
        self.log.ask(ReadKind::Discriminant, key, u64::from(variant), &[], Ok(digest));
        answer
    }

    fn valid_range(&self, key: ItemKey) -> ValidRange {
        let answer = self.world.facts().valid_range(key);
        self.log.ask(ReadKind::ValidRange, key, 0, &[], Ok(small((answer.start, answer.end))));
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
        self.world.facts().coroutine_state(owner, ordinal, args, ground)
    }

    fn coroutine_niche(&self, owner: ItemKey, ordinal: u32, args: &[PTy], ground: PTys<'_>) -> Result<Option<blaze_build::StateNiche>, Missing> {
        self.world.facts().coroutine_niche(owner, ordinal, args, ground)
    }

    fn opaque_hidden(&self, opaque: ItemKey, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        let answer = self.world.facts().opaque_hidden(opaque, args, ground);
        self.log.ask(ReadKind::OpaqueHidden, opaque, 0, args, ty_answer(&answer, ground.view()));
        answer
    }
}

impl Adts for Recorded<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let answer = self.world.adt(key);
        self.log.ask(ReadKind::Adt, key, 0, &[], adt_answer(answer));
        answer
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

    fn overflow_checks(&self) -> blaze_build::OverflowChecks {
        self.world.overflow_checks()
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
        self.world.c_variadic(item)
    }

    fn legacy_const_generics(&self, item: ItemKey) -> &[u32] {
        self.world.legacy_const_generics(item)
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

    fn promoted_fn_key(&self, instance: Instance, ground: &mut PortableTys<'_>) -> Option<InstanceKey> {
        self.world.promoted_fn_key(instance, ground)
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

    fn caller_site(&self, owner: ItemKey, expr: ExprId) -> Option<blaze_build::CallerSite<'_>> {
        self.world.caller_site(owner, expr)
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
}

/// Asks each row of `entry`'s read list again through `world` (the same world the build is
/// handed), its arguments decoded into `ground`; `resolve` answers a `Resolve` row as the walk
/// resolves a call. Stops at the first row answered otherwise.
pub fn verify(
    entry: &EntryView<'_>,
    world: &dyn Adts,
    resolve: &mut dyn FnMut(ItemKey, List, &mut PortableTys<'_>) -> Option<(ItemKey, List)>,
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<Verdict, ReadsError> {
    let log = ReadLog::new(ReadPlant::None);
    let recorded = Recorded::new(world, &log);
    let mut args: Vec<PTy, Global> = Vec::new_in(Global);
    for row in entry.reads() {
        let (question, answer) = row.map_err(|_| ReadsError::Question)?;
        let (&byte, rest) = question.split_first().ok_or(ReadsError::Question)?;
        let kind = ReadKind::of(byte).ok_or(ReadsError::Kind { byte })?;
        let (extra, name) = rest.split_first_chunk::<8>().ok_or(ReadsError::Question)?;
        let extra = u64::from_le_bytes(*extra);
        let (item, list) = decode_instance(name, ground, scratch).map_err(|why| ReadsError::Symbol { kind, item: ItemKey(0), why })?;
        args.clear();
        args.extend_from_slice(ground.view().list(list).map_err(|_| ReadsError::Malformed { kind, item })?);
        log.clear();
        match kind {
            ReadKind::Adt => {
                LayoutFacts::adt(&recorded, item);
            }
            ReadKind::Repr => {
                recorded.repr(item);
            }
            ReadKind::Discriminant => {
                let variant = u32::try_from(extra).map_err(|_| ReadsError::Question)?;
                let _ = recorded.discriminant(item, variant);
            }
            ReadKind::ValidRange => {
                recorded.valid_range(item);
            }
            ReadKind::Normalize => {
                let _ = recorded.normalize(item, Name(extra), &args, ground);
            }
            ReadKind::OpaqueHidden => {
                let _ = recorded.opaque_hidden(item, &args, ground);
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
                let resolved = resolve(item, list, ground);
                log.ask_resolve(Instance { item, args: list }, resolved, ground.view(), scratch);
            }
        }
        if log.only_answer(kind, item)? != answer {
            return Ok(Verdict::Stale { kind, item });
        }
    }
    Ok(Verdict::Holds)
}
