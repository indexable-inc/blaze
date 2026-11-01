//! How HIR values are written into a [`blaze_format`] buffer (docs/v2/plan/lanes/format.md section 5),
//! for the results the database keeps on disk once they are cold: item trees, expression
//! stores, bodies and their source maps.
//!
//! A buffer is columns, each a [`Slice`] of plain records: one per arena ([`Node`]: what kind
//! of value each element is and where its fields begin) and one of words, [`WordsRec`], that
//! every value's fields are written in one after another, as its [`Code`] says. Each name or
//! other text is in the buffer once, as a [`Str`] in a table the words name it by; a
//! [`Symbol`] is written as its text and interned again when read.
//!
//! What the query database interns or tracks (a crate, a file, a macro call, a block, a syntax context) is
//! written as its dense index, which means something only to the database that made it: these
//! buffers are one run's state, the daemon's image of what it let go of ("IDs, two kinds":
//! a dense index may be kept in the image), never a key, a hash or the bytes of an output.
//!
//! Inside [`relocating`], each such id is written instead as its place in a [`Relocation`]
//! table, one word, and read back through the table: the table's ids are what the writer
//! encodes portably (`crate::deps::signatures` writes them as `nameres::frozen` does), so the
//! words mean the same in another process. Outside, the bytes are exactly the ones above.

use std::cell::RefCell;
use std::hash::BuildHasher;
use std::mem::{offset_of, size_of};
use std::ops::{Deref, DerefMut};

use blaze_format::{FormatError, Slice, Str, Writer};
use blaze_hash::{FixedState, HashMap};
use cfg::{CfgAtom, CfgExpr};
use hir_expand::{
    HirFileId, InFile, MacroCallId,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::{Internable, Interned, Symbol};
use smallvec::SmallVec;
use span::{Edition, ErasedFileAstId, FileAstId, Span, SpanAnchor, SyntaxContext};
use syntax::{TextRange, TextSize};
use thin_vec::ThinVec;

use crate::hir::ExprOrPatIdPacked;

/// `Code` for the query database's ids, as their bits: the database that wrote them is the one reading.
/// Inside [`relocating`], as their place in the table, by the [`Relocate::KIND`] of their type.
macro_rules! query_ids {
    ($($ty:ty),* $(,)?) => {$(
        impl $crate::hir::persist::Code for $ty {
            fn put(&self, w: &mut $crate::hir::persist::Words) {
                w.query_id(
                    <$ty as $crate::hir::persist::Relocate>::KIND,
                    blaze_query::db::AsId::as_id(self),
                );
            }

            fn get(
                r: &mut $crate::hir::persist::Read<'_>,
            ) -> Result<Self, ::blaze_format::FormatError> {
                let kind = <$ty as $crate::hir::persist::Relocate>::KIND;
                Ok(<$ty as blaze_query::db::FromId>::from_id(r.query_id_of(kind)?))
            }
        }
    )*};
}
pub(crate) use query_ids;

/// The id whose bits are the word `bits`: `None` for 0 or a word past `u32`, which no id is.
pub(crate) fn id_of_bits(bits: u64) -> Option<blaze_query::db::Id> {
    u32::try_from(bits).ok().filter(|&it| it != 0).map(blaze_query::db::Id::from_u32)
}

/// The id of a [`Relocation`] entry's bits, which this database made.
pub(crate) fn entry_id(bits: u64) -> blaze_query::db::Id {
    id_of_bits(bits).expect("a relocation entry of an id that is no id of this database")
}

/// What an id in a [`Relocation`] table is: which query database type, or a syntax context, or a span's
/// file. An entry is read back only as the kind it was written as.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Kind {
    Crate,
    File,
    MacroCall,
    Block,
    Function,
    Struct,
    Union,
    Enum,
    Const,
    Static,
    Trait,
    TypeAlias,
    Impl,
    EnumVariant,
    Macro2,
    MacroRules,
    ProcMacro,
    /// A [`SyntaxContext`], as `into_u32` gives it.
    Context,
    /// A [`span::EditionedFileId`], as `as_u32` gives it.
    SpanFile,
}

impl Kind {
    /// Every kind, each at its number.
    pub(crate) const ALL: [Kind; 19] = [
        Kind::Crate,
        Kind::File,
        Kind::MacroCall,
        Kind::Block,
        Kind::Function,
        Kind::Struct,
        Kind::Union,
        Kind::Enum,
        Kind::Const,
        Kind::Static,
        Kind::Trait,
        Kind::TypeAlias,
        Kind::Impl,
        Kind::EnumVariant,
        Kind::Macro2,
        Kind::MacroRules,
        Kind::ProcMacro,
        Kind::Context,
        Kind::SpanFile,
    ];
}

const _: () = {
    let mut at = 0;
    while at < Kind::ALL.len() {
        assert!(Kind::ALL[at] as usize == at, "Kind::ALL is not in the order of the kinds");
        at += 1;
    }
};

/// An id type [`query_ids`] writes, by the kind its table entries are.
pub(crate) trait Relocate {
    const KIND: Kind;
}

macro_rules! relocate {
    ($($ty:ty => $kind:ident),* $(,)?) => {$(
        impl Relocate for $ty {
            const KIND: Kind = Kind::$kind;
        }
    )*};
}

relocate! {
    base_db::Crate => Crate,
    base_db::EditionedFileId => File,
    MacroCallId => MacroCall,
    crate::BlockId => Block,
    crate::FunctionId => Function,
    crate::StructId => Struct,
    crate::UnionId => Union,
    crate::EnumId => Enum,
    crate::ConstId => Const,
    crate::StaticId => Static,
    crate::TraitId => Trait,
    crate::TypeAliasId => TypeAlias,
    crate::ImplId => Impl,
    crate::EnumVariantId => EnumVariant,
    crate::Macro2Id => Macro2,
    crate::MacroRulesId => MacroRules,
    crate::ProcMacroId => ProcMacro,
}

/// The ids a buffer names, each once, in the order first written: an id is written as its
/// index here. Written, it is filled by [`Relocation::index`]; read, it is made whole by
/// [`Relocation::of`] from the ids the writer's table names, made again in this database.
#[derive(Default, Debug)]
pub(crate) struct Relocation {
    entries: Vec<(Kind, u64)>,
    indexes: HashMap<(Kind, u64), u32>,
}

impl Relocation {
    pub(crate) fn new() -> Relocation {
        Relocation::default()
    }

    /// The table of a buffer being read: entry `i` is the id index `i` names.
    pub(crate) fn of(entries: Vec<(Kind, u64)>) -> Relocation {
        Relocation { entries, indexes: HashMap::default() }
    }

    /// The index of the id of `kind` whose bits are `bits`, added when it is new.
    pub(crate) fn index(&mut self, kind: Kind, bits: u64) -> u32 {
        debug_assert_eq!(self.entries.len(), self.indexes.len(), "a table made to be read");
        let next = self.entries.len();
        *self.indexes.entry((kind, bits)).or_insert_with(|| {
            self.entries.push((kind, bits));
            u32::try_from(next).expect("more than 2^32 ids in one relocation table")
        })
    }

    /// The bits of the id `index` names, `None` when there is none or it is of another kind.
    pub(crate) fn get(&self, kind: Kind, index: u32) -> Option<u64> {
        let &(found, bits) = self.entries.get(index as usize)?;
        (found == kind).then_some(bits)
    }

    pub(crate) fn entries(&self) -> &[(Kind, u64)] {
        &self.entries
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

thread_local! {
    /// The table of the [`relocating`] scope this thread is in, if any. Taken and put back
    /// around each id written or read, never held across anything else.
    static RELOCATION: RefCell<Option<Relocation>> = const { RefCell::new(None) };
}

/// Runs `f` with every id a [`Words`] writes or a [`Read`] reads going through `table`: what is
/// written in `f` adds to it, what is read in `f` is looked up in it. `table` holds what was
/// added when `f` returns or unwinds. Scopes do not nest; `f` must not call into the database,
/// whose own buffers are never relocated.
pub(crate) fn relocating<T>(table: &mut Relocation, f: impl FnOnce() -> T) -> T {
    carry_scopes();
    stdx::lend(&RELOCATION, table, f)
}

/// Carries [`RELOCATION`] and [`ANONYMOUS`] with each job on a fiber across its waits
/// ([`blaze_query::carry`]), from the first scope on: a job that waits inside one resumes in
/// it, wherever it resumes.
fn carry_scopes() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried(&RELOCATION, Option::is_none),
            put: |it| stdx::lend::put_carried(&RELOCATION, Option::is_none, it),
        });
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried(&ANONYMOUS, Option::is_none),
            put: |it| stdx::lend::put_carried(&ANONYMOUS, Option::is_none, it),
        });
    });
}

// Each access to `RELOCATION` or `ANONYMOUS` is a call of its own, never inlined: a job that
// waits between two may resume on another thread, and an inlined access may reuse the address
// of the first thread's slot (docs/v2/plan/lanes/may-suspend.md).

/// The index of an id written in a [`relocating`] scope, `None` outside one.
#[inline(never)]
fn relocated(kind: Kind, bits: u64) -> Option<u32> {
    RELOCATION.with(|slot| slot.borrow_mut().as_mut().map(|table| table.index(kind, bits)))
}

/// Whether ids are read through a table: `Some` of what `index` names in it, `Some(None)`
/// when it names nothing of `kind`, `None` outside a [`relocating`] scope.
#[inline(never)]
fn unrelocated(kind: Kind, index: u32) -> Option<Option<u64>> {
    RELOCATION.with(|slot| slot.borrow().as_ref().map(|table| table.get(kind, index)))
}

#[inline(never)]
fn in_scope() -> bool {
    RELOCATION.with(|slot| slot.borrow().is_some())
}

thread_local! {
    /// What the [`anonymous`] scope this thread is in leaves out, if it is in one.
    static ANONYMOUS: RefCell<Option<Anonymous>> = const { RefCell::new(None) };
}

/// What an [`anonymous`] scope writes as the empty name: every binding's and label's name, and
/// for each path of the store (by its index in the store's paths) its first `blank[i]`
/// segments, whose meaning the caller hashes as what they resolve to. A path past `blank` is
/// written whole. Each literal expression of `slots` (raw expression ids, sorted) is written as
/// a slot: its kind and type, not its value. A named lifetime that is one of `lifetimes` (the
/// owner's lifetime parameters, its impl's or trait's first) is written as its index there.
#[derive(Default)]
pub(crate) struct Anonymous {
    pub(crate) blank: Vec<u16>,
    pub(crate) slots: Vec<u32>,
    pub(crate) lifetimes: Vec<Name>,
}

/// Runs `f` with what [`Words`] writes anonymous as `it` says. Scopes do not nest. Only a
/// write that is hashed, never read back, may be anonymous: it does not round-trip.
pub(crate) fn anonymous<T>(it: &mut Anonymous, f: impl FnOnce() -> T) -> T {
    carry_scopes();
    stdx::lend(&ANONYMOUS, it, f)
}

/// Whether names of bindings and labels are written empty.
#[inline(never)]
pub(crate) fn names_blank() -> bool {
    ANONYMOUS.with(|slot| slot.borrow().is_some())
}

/// How many leading segments of the store's path `at` are written empty.
#[inline(never)]
pub(crate) fn blank_segments(at: usize) -> usize {
    ANONYMOUS.with(|slot| {
        slot.borrow().as_ref().map_or(0, |it| it.blank.get(at).map_or(0, |&n| usize::from(n)))
    })
}

/// The index of the lifetime `name` among the [`Anonymous::lifetimes`] of the scope this thread
/// is in: `None` outside one, or for a name none of them has.
#[inline(never)]
pub(crate) fn lifetime_index(name: &Name) -> Option<u32> {
    ANONYMOUS.with(|slot| {
        let slot = slot.borrow();
        let at = slot.as_ref()?.lifetimes.iter().rposition(|it| it == name)?;
        Some(u32::try_from(at).expect("more than 2^32 lifetime parameters"))
    })
}

/// `f` of the [`Anonymous::slots`] of the scope this thread is in, empty outside one.
#[inline(never)]
pub(crate) fn with_slots<T>(f: impl FnOnce(&[u32]) -> T) -> T {
    ANONYMOUS.with(|slot| f(slot.borrow().as_ref().map_or(&[][..], |it| &it.slots[..])))
}

blaze_format::record! {
    /// One element of an arena: which of its type's variants it is, and the word its fields
    /// begin at.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct Node: 1 {
        pub(crate) kind: u32,
        pub(crate) at: u32,
    }
}

blaze_format::record! {
    /// The words and the names of a buffer.
    #[derive(Clone, Copy)]
    pub(crate) struct WordsRec: 1 {
        pub(crate) words: Slice<u32>,
        pub(crate) names: Slice<Str>,
    }
}

/// A value written as words: at least one, so that a count read from a buffer is never more
/// than the words left.
pub(crate) trait Code: Sized {
    fn put(&self, w: &mut Words);
    fn get(r: &mut Read<'_>) -> Result<Self, FormatError>;
}

/// The words and names of a buffer being written. Its vectors and maps are this thread's
/// spares ([`Words::new`]), given back emptied when it is dropped, so a warm write grows
/// none of them.
pub(crate) struct Words(WordsBuffers);

#[derive(Default)]
pub(crate) struct WordsBuffers {
    words: Vec<u32>,
    text: String,
    /// Each name's start in `text` and its length.
    names: Vec<(u32, u32)>,
    /// The last name of each text's hash; earlier names of the same hash are chained in
    /// `same_hash`.
    by_hash: HashMap<u64, u32>,
    /// For each name, the name before it with the same hash, [`NO_NAME`] for none.
    same_hash: Vec<u32>,
    by_symbol: HashMap<Symbol, u32>,
    /// What [`Words::place`] builds the name column in.
    placed: Vec<Str>,
}

const NO_NAME: u32 = u32::MAX;
/// The spares a thread keeps (writes nest: a buffer's values are written while another's
/// are), and the words above which a spare is freed rather than kept.
const SPARE_WORDS: usize = 4;
const SPARE_WORDS_CAP: usize = 1 << 22;

thread_local! {
    // alloc: const-empty, once per thread.
    static SPARES: RefCell<Vec<WordsBuffers>> = const { RefCell::new(Vec::new()) };
}

impl WordsBuffers {
    fn clear(&mut self) {
        let WordsBuffers { words, text, names, by_hash, same_hash, by_symbol, placed } = self;
        words.clear();
        text.clear();
        names.clear();
        by_hash.clear();
        same_hash.clear();
        by_symbol.clear();
        placed.clear();
    }

    fn held(&self) -> usize {
        self.words.capacity() + self.text.capacity() / 4 + self.names.capacity()
    }
}

impl Deref for Words {
    type Target = WordsBuffers;

    fn deref(&self) -> &WordsBuffers {
        &self.0
    }
}

impl DerefMut for Words {
    fn deref_mut(&mut self) -> &mut WordsBuffers {
        &mut self.0
    }
}

impl Drop for Words {
    fn drop(&mut self) {
        let mut buffers = std::mem::take(&mut self.0);
        if buffers.held() > SPARE_WORDS_CAP {
            return;
        }
        buffers.clear();
        // A thread that is ending frees it.
        let _ = SPARES.try_with(move |spares| {
            let mut spares = spares.borrow_mut();
            if spares.len() < SPARE_WORDS {
                spares.push(buffers);
            }
        });
    }
}

impl Words {
    /// Words on one of this thread's spares, empty.
    pub(crate) fn new() -> Words {
        let spare = SPARES.try_with(|spares| spares.borrow_mut().pop()).ok().flatten();
        Words(spare.unwrap_or_default())
    }

    /// The word the next one written will be.
    pub(crate) fn at(&self) -> u32 {
        u32::try_from(self.words.len()).expect("more than 2^32 words of HIR in one buffer")
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.words.push(value);
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.u32(value as u32);
        self.u32((value >> 32) as u32);
    }

    pub(crate) fn u128(&mut self, value: u128) {
        self.u64(value as u64);
        self.u64((value >> 64) as u64);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.u32(value as u32);
    }

    /// A count of what follows.
    pub(crate) fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).expect("more than 2^32 values in one HIR list"));
    }

    pub(crate) fn str(&mut self, text: &str) {
        let index = self.name(text);
        self.u32(index);
    }

    pub(crate) fn symbol(&mut self, symbol: &Symbol) {
        let index = match self.by_symbol.get(symbol) {
            Some(&index) => index,
            None => {
                let index = self.name(symbol.as_str());
                // clone: a `Symbol` is an interned pointer; the map of a write keeps one per name.
                self.by_symbol.insert(symbol.clone(), index);
                index
            }
        };
        self.u32(index);
    }

    pub(crate) fn put<T: Code>(&mut self, value: &T) {
        value.put(self);
    }

    /// An id the query database made: its bits as a `u64` word, or inside [`relocating`] its
    /// index in the table.
    pub(crate) fn query_id(&mut self, kind: Kind, id: blaze_query::db::Id) {
        let bits = u64::from(id.as_u32());
        match relocated(kind, bits) {
            Some(index) => self.u32(index),
            None => self.u64(bits),
        }
    }

    /// A count, then each of `values`.
    pub(crate) fn seq<'a, T: Code + 'a>(&mut self, values: impl ExactSizeIterator<Item = &'a T>) {
        self.len(values.len());
        values.for_each(|it| it.put(self));
    }

    fn name(&mut self, text: &str) -> u32 {
        let hash = FixedState.hash_one(text);
        let mut at = self.by_hash.get(&hash).copied().unwrap_or(NO_NAME);
        while at != NO_NAME {
            let (start, len) = self.names[at as usize];
            if &self.text[start as usize..(start + len) as usize] == text {
                return at;
            }
            at = self.same_hash[at as usize];
        }
        let index = u32::try_from(self.names.len()).expect("more than 2^32 names in one buffer");
        let start = u32::try_from(self.text.len()).expect("more than 4 GiB of names in one buffer");
        let len = u32::try_from(text.len()).expect("a name of 4 GiB or more");
        self.text.push_str(text);
        self.names.push((start, len));
        let earlier = self.by_hash.insert(hash, index).unwrap_or(NO_NAME);
        self.same_hash.push(earlier);
        index
    }

    /// Writes the text, the names and the words, for a [`WordsRec`] to point at
    /// ([`Placed::rec`]).
    pub(crate) fn place(&mut self, out: &mut Writer) -> Placed {
        let text_at = out.push_slice(self.text.as_bytes());
        let names_at = out.next::<Str>();
        let WordsBuffers { names, placed, .. } = &mut self.0;
        placed.clear();
        placed.extend(names.iter().enumerate().map(|(index, &(start, len))| {
            let field_at = names_at + (index * size_of::<Str>()) as u64;
            Str::between(field_at, text_at + u64::from(start), len as usize)
        }));
        let pushed = out.push_slice(placed);
        debug_assert_eq!(pushed, names_at, "`Writer::next` is not where `push_slice` writes");
        let names = Column { at: names_at, len: placed.len() as u32 };
        let words_at = out.push_slice(&self.words);
        Placed { words: Column { at: words_at, len: self.at() }, names }
    }
}

/// Where a column of records was written and how many it holds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Column {
    pub(crate) at: u64,
    pub(crate) len: u32,
}

impl Column {
    /// Writes `values` as a column.
    pub(crate) fn push<T: blaze_format::Record>(out: &mut Writer, values: &[T]) -> Column {
        let len = u32::try_from(values.len()).expect("a column of more than 2^32 records");
        Column { at: out.push_slice(values), len }
    }

    /// The slice of this column for a field that is written at `field_at`.
    pub(crate) fn slice<T>(self, field_at: u64) -> Slice<T> {
        match self.len {
            0 => Slice::EMPTY,
            len => Slice::between(field_at, self.at, len as usize),
        }
    }
}

/// Where [`Words::place`] wrote the words and the names.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Placed {
    words: Column,
    names: Column,
}

impl Placed {
    /// The record of the words and names, for one written at `at`.
    pub(crate) fn rec(self, at: u64) -> WordsRec {
        WordsRec {
            words: self.words.slice(at + offset_of!(WordsRec, words) as u64),
            names: self.names.slice(at + offset_of!(WordsRec, names) as u64),
        }
    }
}

/// Reads the words of a checked buffer, from a word on.
#[derive(Clone, Copy)]
pub(crate) struct Read<'a> {
    words: &'a [u32],
    names: &'a [Str],
    buffer: &'a [u8],
    next: usize,
}

impl<'a> Read<'a> {
    /// The words of `rec`, which is in `buffer`, from the first.
    pub(crate) fn new(rec: &'a WordsRec, buffer: &'a [u8]) -> Read<'a> {
        Read { words: rec.words.get(buffer), names: rec.names.get(buffer), buffer, next: 0 }
    }

    /// The same words, from word `at`.
    pub(crate) fn from(&self, at: u32) -> Read<'a> {
        Read { next: at as usize, ..*self }
    }

    /// Where the next word is in the buffer, for an error.
    fn here(&self) -> u64 {
        // An empty slice may point anywhere.
        let words = (self.words.as_ptr() as usize).saturating_sub(self.buffer.as_ptr() as usize);
        (words + self.next * size_of::<u32>()) as u64
    }

    pub(crate) fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.here(), what }
    }

    pub(crate) fn u32(&mut self) -> Result<u32, FormatError> {
        let word = *self.words.get(self.next).ok_or(FormatError::OutOfBounds { at: self.here() })?;
        self.next += 1;
        Ok(word)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, FormatError> {
        let low = self.u32()?;
        Ok(u64::from(low) | (u64::from(self.u32()?) << 32))
    }

    pub(crate) fn u128(&mut self) -> Result<u128, FormatError> {
        let low = self.u64()?;
        Ok(u128::from(low) | (u128::from(self.u64()?) << 64))
    }

    pub(crate) fn bool(&mut self) -> Result<bool, FormatError> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(self.invalid("a bool that is neither 0 nor 1")),
        }
    }

    /// A count of what follows, which is never more than the words left: each value is at
    /// least one.
    pub(crate) fn len(&mut self) -> Result<usize, FormatError> {
        let len = self.u32()? as usize;
        match len <= self.words.len().saturating_sub(self.next) {
            true => Ok(len),
            false => Err(self.invalid("a count of more values than there are words")),
        }
    }

    /// Which of `count` variants a value is.
    pub(crate) fn tag(&mut self, count: u32, what: &'static str) -> Result<u32, FormatError> {
        let tag = self.u32()?;
        match tag < count {
            true => Ok(tag),
            false => Err(self.invalid(what)),
        }
    }

    pub(crate) fn str(&mut self) -> Result<&'a str, FormatError> {
        let index = self.u32()? as usize;
        let name = self.names.get(index).ok_or_else(|| self.invalid("a name not in the table"))?;
        name.as_str(self.buffer)
    }

    pub(crate) fn symbol(&mut self) -> Result<Symbol, FormatError> {
        Ok(Symbol::intern(self.str()?))
    }

    pub(crate) fn get<T: Code>(&mut self) -> Result<T, FormatError> {
        T::get(self)
    }

    /// A count, then that many values.
    pub(crate) fn seq<T: Code, C: FromIterator<T>>(&mut self) -> Result<C, FormatError> {
        let len = self.len()?;
        // alloc: the collection the caller asks for, of the values read.
        (0..len).map(|_| T::get(self)).collect()
    }

    /// The id of something the query database made, as [`query_ids`] wrote it outside
    /// [`relocating`].
    pub(crate) fn query_id(&mut self) -> Result<blaze_query::db::Id, FormatError> {
        let bits = self.u64()?;
        id_of_bits(bits).ok_or_else(|| self.invalid("an id of 0 or of more than 32 bits"))
    }

    /// The id of something the query database made of `kind`, as [`Words::query_id`] wrote it:
    /// inside [`relocating`], looked up in the table.
    pub(crate) fn query_id_of(&mut self, kind: Kind) -> Result<blaze_query::db::Id, FormatError> {
        if !in_scope() {
            return self.query_id();
        }
        let bits = self.relocated(kind)?;
        id_of_bits(bits).ok_or_else(|| self.invalid("a relocated id of 0 or of more than 32 bits"))
    }

    /// The bits of the id of `kind` the next word names in the [`relocating`] table.
    fn relocated(&mut self, kind: Kind) -> Result<u64, FormatError> {
        let index = self.u32()?;
        match unrelocated(kind, index) {
            Some(Some(bits)) => Ok(bits),
            Some(None) => {
                Err(self.invalid("an id the relocation table has not, or of another kind"))
            }
            None => Err(self.invalid("a relocated id read outside a relocation")),
        }
    }
}

macro_rules! narrow {
    ($($ty:ty),*) => {$(
        impl Code for $ty {
            fn put(&self, w: &mut Words) {
                w.u32(u32::from(*self));
            }

            fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
                let word = r.u32()?;
                <$ty>::try_from(word).map_err(|_| r.invalid(concat!("a ", stringify!($ty), " out of range")))
            }
        }
    )*};
}
narrow!(u8, u16, u32);

impl Code for u64 {
    fn put(&self, w: &mut Words) {
        w.u64(*self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u64()
    }
}

impl Code for usize {
    fn put(&self, w: &mut Words) {
        w.u64(*self as u64);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let value = r.u64()?;
        usize::try_from(value).map_err(|_| r.invalid("a usize out of range"))
    }
}

impl Code for u128 {
    fn put(&self, w: &mut Words) {
        w.u128(*self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u128()
    }
}

impl Code for i128 {
    fn put(&self, w: &mut Words) {
        w.u128(*self as u128);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(r.u128()? as i128)
    }
}

impl Code for bool {
    fn put(&self, w: &mut Words) {
        w.bool(*self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.bool()
    }
}

impl Code for char {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(*self));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let word = r.u32()?;
        char::from_u32(word).ok_or_else(|| r.invalid("a char that is not a scalar value"))
    }
}

impl<T: Code> Code for Option<T> {
    fn put(&self, w: &mut Words) {
        match self {
            None => w.u32(0),
            Some(it) => {
                w.u32(1);
                it.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        match r.tag(2, "an option that is neither 0 nor 1")? {
            0 => Ok(None),
            _ => T::get(r).map(Some),
        }
    }
}

impl<T: Code> Code for Box<T> {
    fn put(&self, w: &mut Words) {
        (**self).put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        T::get(r).map(Box::new)
    }
}

impl<T: Code> Code for triomphe::Arc<T> {
    fn put(&self, w: &mut Words) {
        (**self).put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        T::get(r).map(triomphe::Arc::new)
    }
}

impl<T: Code> Code for Box<[T]> {
    fn put(&self, w: &mut Words) {
        w.seq(self.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.seq::<T, _>()
    }
}

impl<T: Code> Code for Vec<T> {
    fn put(&self, w: &mut Words) {
        w.seq(self.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.seq::<T, _>()
    }
}

impl<T: Code> Code for ThinVec<T> {
    fn put(&self, w: &mut Words) {
        w.seq(self.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.seq::<T, _>()
    }
}

impl<A: smallvec::Array<Item: Code>> Code for SmallVec<A> {
    fn put(&self, w: &mut Words) {
        w.seq(self.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.seq::<A::Item, _>()
    }
}

impl<A: Code, B: Code> Code for (A, B) {
    fn put(&self, w: &mut Words) {
        self.0.put(w);
        self.1.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok((A::get(r)?, B::get(r)?))
    }
}

impl<A: Code, B: Code, C: Code> Code for (A, B, C) {
    fn put(&self, w: &mut Words) {
        self.0.put(w);
        self.1.put(w);
        self.2.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok((A::get(r)?, B::get(r)?, C::get(r)?))
    }
}

impl Code for Box<str> {
    fn put(&self, w: &mut Words) {
        w.str(self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.str().map(Box::from)
    }
}

impl Code for String {
    fn put(&self, w: &mut Words) {
        w.str(self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        // alloc: a `String` field owns its text.
        r.str().map(String::from)
    }
}

impl Code for Symbol {
    fn put(&self, w: &mut Words) {
        w.symbol(self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.symbol()
    }
}

impl Code for Name {
    fn put(&self, w: &mut Words) {
        w.symbol(self.symbol());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let symbol = r.symbol()?;
        match symbol.as_str().starts_with("r#") {
            true => Err(r.invalid("a name written with its `r#`")),
            false => Ok(Name::new_symbol_root(symbol)),
        }
    }
}

impl Code for PathKind {
    fn put(&self, w: &mut Words) {
        match *self {
            PathKind::Plain => w.u32(0),
            PathKind::Super(n) => {
                w.u32(1);
                w.u32(u32::from(n));
            }
            PathKind::Crate => w.u32(2),
            PathKind::Abs => w.u32(3),
            PathKind::DollarCrate(krate) => {
                w.u32(4);
                krate.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(5, "a path kind out of range")? {
            0 => PathKind::Plain,
            1 => PathKind::Super(r.get()?),
            2 => PathKind::Crate,
            3 => PathKind::Abs,
            _ => PathKind::DollarCrate(r.get()?),
        })
    }
}

impl Code for ModPath {
    fn put(&self, w: &mut Words) {
        self.kind.put(w);
        w.seq(self.segments().iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let kind = r.get()?;
        let segments: Vec<Name> = r.seq::<Name, _>()?;
        Ok(ModPath::from_segments(kind, segments))
    }
}

impl<T: Internable + Code> Code for Interned<T> {
    fn put(&self, w: &mut Words) {
        (**self).put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        T::get(r).map(Interned::new)
    }
}

impl Code for Edition {
    fn put(&self, w: &mut Words) {
        w.u32(*self as u32);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let edition = r.tag(Edition::LATEST as u32 + 1, "an edition out of range")?;
        Ok(Edition::from_u32(edition))
    }
}

/// The syntax context `raw` names, as `into_u32` gave it; `None` for `u32::MAX`, which no
/// context is.
///
/// The one place persisted contexts are trusted. `raw` is what `into_u32` gave in the database
/// that wrote the buffer, which is the one reading it (the module's docs), or, inside
/// `relocating`, a context the reader made in this database from the table's portable encoding
/// (`crate::deps::signatures`). Store bytes reach a `read` only after their content hash is
/// checked (`deps::serve::ImageStore::get`). Another value names another context or panics on
/// use, never UB.
pub(crate) fn syntax_context(raw: u32) -> Option<SyntaxContext> {
    if raw == u32::MAX {
        return None;
    }
    Some(SyntaxContext::from_u32(raw))
}

impl Code for SyntaxContext {
    fn put(&self, w: &mut Words) {
        let raw = self.into_u32();
        w.u32(relocated(Kind::Context, u64::from(raw)).unwrap_or(raw));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let raw = match in_scope() {
            true => r.relocated(Kind::Context)? as u32,
            false => r.u32()?,
        };
        syntax_context(raw).ok_or_else(|| r.invalid("a syntax context of u32::MAX"))
    }
}

impl Code for TextSize {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(*self));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u32().map(TextSize::from)
    }
}

impl Code for TextRange {
    fn put(&self, w: &mut Words) {
        self.start().put(w);
        self.end().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let (start, end): (TextSize, TextSize) = (r.get()?, r.get()?);
        match start <= end {
            true => Ok(TextRange::new(start, end)),
            false => Err(r.invalid("a range that ends before it starts")),
        }
    }
}

impl Code for ErasedFileAstId {
    fn put(&self, w: &mut Words) {
        w.u32(self.into_raw());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u32().map(ErasedFileAstId::from_raw)
    }
}

impl<N> Code for FileAstId<N> {
    fn put(&self, w: &mut Words) {
        self.erase().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.get().map(FileAstId::from_erased_unchecked)
    }
}

impl Code for span::EditionedFileId {
    fn put(&self, w: &mut Words) {
        let raw = self.as_u32();
        w.u32(relocated(Kind::SpanFile, u64::from(raw)).unwrap_or(raw));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let raw = match in_scope() {
            true => r.relocated(Kind::SpanFile)? as u32,
            false => r.u32()?,
        };
        let edition = (raw & Self::EDITION_MASK) >> Self::FILE_ID_BITS;
        match raw & Self::RESERVED_MASK == 0 && edition <= Edition::LATEST as u32 {
            true => Ok(Self::from_raw(raw)),
            false => Err(r.invalid("a file id with reserved bits or an edition out of range")),
        }
    }
}

impl Code for SpanAnchor {
    fn put(&self, w: &mut Words) {
        self.file_id.put(w);
        self.ast_id.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(SpanAnchor { file_id: r.get()?, ast_id: r.get()? })
    }
}

impl Code for Span {
    fn put(&self, w: &mut Words) {
        self.range.put(w);
        self.anchor.put(w);
        self.ctx.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(Span { range: r.get()?, anchor: r.get()?, ctx: r.get()? })
    }
}

impl Code for CfgAtom {
    fn put(&self, w: &mut Words) {
        match self {
            CfgAtom::Flag(flag) => {
                w.u32(0);
                w.symbol(flag);
            }
            CfgAtom::KeyValue { key, value } => {
                w.u32(1);
                w.symbol(key);
                w.symbol(value);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a cfg atom out of range")? {
            0 => CfgAtom::Flag(r.symbol()?),
            _ => CfgAtom::KeyValue { key: r.symbol()?, value: r.symbol()? },
        })
    }
}

impl Code for CfgExpr {
    fn put(&self, w: &mut Words) {
        match self {
            CfgExpr::Invalid => w.u32(0),
            CfgExpr::Atom(atom) => {
                w.u32(1);
                atom.put(w);
            }
            CfgExpr::All(all) => {
                w.u32(2);
                all.put(w);
            }
            CfgExpr::Any(any) => {
                w.u32(3);
                any.put(w);
            }
            CfgExpr::Not(not) => {
                w.u32(4);
                not.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(5, "a cfg expression out of range")? {
            0 => CfgExpr::Invalid,
            1 => CfgExpr::Atom(r.get()?),
            2 => CfgExpr::All(r.get()?),
            3 => CfgExpr::Any(r.get()?),
            _ => CfgExpr::Not(r.get()?),
        })
    }
}

query_ids!(base_db::Crate, base_db::EditionedFileId, MacroCallId, crate::BlockId);

impl Code for HirFileId {
    fn put(&self, w: &mut Words) {
        match self {
            HirFileId::FileId(file) => {
                w.u32(0);
                file.put(w);
            }
            HirFileId::MacroFile(call) => {
                w.u32(1);
                call.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a file id that is neither a file nor a macro file")? {
            0 => HirFileId::FileId(r.get()?),
            _ => HirFileId::MacroFile(r.get()?),
        })
    }
}

impl<T: Code> Code for InFile<T> {
    fn put(&self, w: &mut Words) {
        self.file_id.put(w);
        self.value.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let file_id = r.get()?;
        Ok(InFile::new(file_id, r.get()?))
    }
}

impl<T> Code for la_arena::Idx<T> {
    fn put(&self, w: &mut Words) {
        w.u32(self.into_raw().into_u32());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(r.u32()?)))
    }
}

impl Code for ExprOrPatIdPacked {
    fn put(&self, w: &mut Words) {
        w.u32(self.0);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u32().map(ExprOrPatIdPacked)
    }
}

/// Writes `node` for each value of `values` in turn, the value's fields as `put` writes them
/// and its kind as `kind` says, and returns the column of their [`Node`]s.
pub(crate) fn nodes<'a, T: 'a>(
    w: &mut Words,
    values: impl Iterator<Item = &'a T>,
    kind: impl Fn(&T) -> u32,
    mut put: impl FnMut(&T, &mut Words),
) -> Vec<Node> {
    values
        .map(|value| {
            let at = w.at();
            put(value, w);
            debug_assert!(w.at() > at, "a value written as no words");
            Node { kind: kind(value), at }
        })
        // alloc: write path, one node per value written.
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    blaze_format::record! {
        #[derive(Clone, Copy)]
        struct Root: 1 {
            words: WordsRec,
            first: u32,
        }
    }

    const SCHEMA: blaze_format::SchemaHash = <Root as blaze_format::Record>::SCHEMA;

    fn round_trip<T: Code + PartialEq + std::fmt::Debug>(value: T) {
        let mut w = Words::new();
        let first = w.at();
        value.put(&mut w);
        let mut out = Writer::new(SCHEMA);
        let placed = w.place(&mut out);
        let at = out.next::<Root>();
        let root = out.push(&Root { words: placed.rec(at + offset_of!(Root, words) as u64), first });
        let buffer = blaze_format::Aligned::copy_from(&out.finish(root));
        let root = blaze_format::open::<Root>(&buffer).expect("a buffer just written");
        let mut r = Read::new(&root.words, &buffer).from(root.first);
        assert_eq!(r.get::<T>().expect("a value just written"), value);
    }

    #[test]
    fn values_read_back_as_written() {
        round_trip(0u8);
        round_trip(u32::MAX);
        round_trip(u64::MAX - 1);
        round_trip(u128::MAX - 7);
        round_trip(i128::MIN + 3);
        round_trip('é');
        round_trip(Some(Box::<str>::from("a name")));
        round_trip(None::<u32>);
        round_trip(vec![String::from("x"), String::from("y"), String::from("x")]);
        round_trip((true, 7u16, Symbol::intern("sym")));
        round_trip(ModPath::from_segments(
            PathKind::Super(2),
            [Name::new_symbol_root(Symbol::intern("a")), Name::new_symbol_root(Symbol::intern("b"))],
        ));
        round_trip(CfgExpr::All(Box::new([
            CfgExpr::Atom(CfgAtom::Flag(Symbol::intern("test"))),
            CfgExpr::Not(Box::new(CfgExpr::Atom(CfgAtom::KeyValue {
                key: Symbol::intern("target_os"),
                value: Symbol::intern("linux"),
            }))),
        ])));
        round_trip(TextRange::new(3.into(), 9.into()));
        round_trip(SyntaxContext::root(Edition::Edition2021));
        round_trip(Edition::Edition2024);
    }

    #[test]
    fn a_count_past_the_words_is_refused() {
        let mut w = Words::new();
        w.u32(5);
        w.u32(1);
        let mut out = Writer::new(SCHEMA);
        let placed = w.place(&mut out);
        let at = out.next::<Root>();
        let root = out.push(&Root { words: placed.rec(at + offset_of!(Root, words) as u64), first: 0 });
        let buffer = blaze_format::Aligned::copy_from(&out.finish(root));
        let root = blaze_format::open::<Root>(&buffer).unwrap();
        assert!(Read::new(&root.words, &buffer).get::<Vec<u32>>().is_err());
    }

    /// Names: one index per text whether it comes as a `str` or a symbol, in first-use order,
    /// with texts of one hash told apart by their bytes; a warm `Words` allocates nothing.
    #[test]
    fn names_are_one_index_per_text_on_warm_words() {
        let texts = ["a", "b", "a", "", "ab", "b", ""];
        let indices = |w: &mut Words| -> Vec<u32> { texts.iter().map(|text| w.name(text)).collect() };
        let mut cold = Words::new();
        assert_eq!(indices(&mut cold), [0, 1, 0, 2, 3, 1, 2]);
        cold.symbol(&Symbol::intern("ab"));
        assert_eq!(cold.words.last(), Some(&3), "a symbol's text is the same name");
        drop(cold);
        // A collision: two names chained under one hash are still found by their text.
        let mut w = Words::new();
        let (first, second) = (w.name("x"), w.name("y"));
        let hash = FixedState.hash_one("x");
        w.by_hash.insert(hash, second);
        w.same_hash[second as usize] = first;
        assert_eq!(w.name("x"), first);
        drop(w);

        let name_all = || {
            let mut warm = Words::new();
            texts.iter().for_each(|text| {
                warm.name(text);
            });
            warm.symbol(&Symbol::intern("ab"));
        };
        name_all();
        blaze_alloc_count::assert_at_most(0, "names on warm words", name_all);
    }

    fn crate_of(bits: u32) -> base_db::Crate {
        <base_db::Crate as blaze_query::db::FromId>::from_id(blaze_query::db::Id::from_u32(bits))
    }

    fn call_of(bits: u32) -> MacroCallId {
        <MacroCallId as blaze_query::db::FromId>::from_id(blaze_query::db::Id::from_u32(bits))
    }

    /// The words `value` is written as, in a scope of `table` when there is one.
    fn words_of<T: Code>(value: &T, table: Option<&mut Relocation>) -> Vec<u32> {
        let mut w = Words::new();
        match table {
            Some(table) => relocating(table, || value.put(&mut w)),
            None => value.put(&mut w),
        }
        std::mem::take(&mut w.words)
    }

    /// `words` read as a `T` in a scope of `table`.
    fn read_back<T: Code>(words: &[u32], table: &mut Relocation) -> Result<T, FormatError> {
        let mut w = Words::new();
        words.iter().for_each(|&word| w.u32(word));
        let mut out = Writer::new(SCHEMA);
        let placed = w.place(&mut out);
        let at = out.next::<Root>();
        let root = out.push(&Root { words: placed.rec(at + offset_of!(Root, words) as u64), first: 0 });
        let buffer = blaze_format::Aligned::copy_from(&out.finish(root));
        let root = blaze_format::open::<Root>(&buffer).unwrap();
        relocating(table, || Read::new(&root.words, &buffer).get::<T>())
    }

    #[test]
    fn ids_outside_a_relocation_are_their_bits() {
        assert_eq!(words_of(&crate_of(5), None), [5, 0]);
        let high = blaze_query::db::Id::MAX_U32;
        assert_eq!(words_of(&crate_of(high), None), [high, 0]);
        let context = SyntaxContext::root(Edition::Edition2021);
        assert_eq!(words_of(&context, None), [context.into_u32()]);
        let file = span::EditionedFileId::new(span::FileId::from_raw(9), Edition::Edition2018);
        assert_eq!(words_of(&file, None), [file.as_u32()]);
    }

    #[test]
    fn ids_in_a_relocation_are_their_index_once_each() {
        let mut table = Relocation::new();
        let value = (crate_of(7), (call_of(7), (crate_of(9), crate_of(7))));
        let words = words_of(&value, Some(&mut table));
        assert_eq!(words, [0, 1, 2, 0], "one index per distinct id and kind");
        let bits = |it: base_db::Crate| u64::from(blaze_query::db::AsId::as_id(&it).as_u32());
        assert_eq!(
            table.entries(),
            [(Kind::Crate, bits(crate_of(7))), (Kind::MacroCall, 7), (Kind::Crate, 9)]
        );
        let mut reading = Relocation::of(table.entries().to_vec());
        let back: (base_db::Crate, (MacroCallId, (base_db::Crate, base_db::Crate))) =
            read_back(&words, &mut reading).expect("words just written");
        assert_eq!(back, value);
    }

    #[test]
    fn contexts_and_span_files_relocate() {
        let mut table = Relocation::new();
        let context = SyntaxContext::root(Edition::Edition2024);
        let file = span::EditionedFileId::new(span::FileId::from_raw(4), Edition::Edition2015);
        let words = words_of(&(context, (file, context)), Some(&mut table));
        assert_eq!(words, [0, 1, 0]);
        let mut reading = Relocation::of(table.entries().to_vec());
        let back: (SyntaxContext, (span::EditionedFileId, SyntaxContext)) =
            read_back(&words, &mut reading).unwrap();
        assert_eq!(back, (context, (file, context)));
    }

    #[test]
    fn an_index_of_another_kind_or_past_the_table_is_refused() {
        let mut reading = Relocation::of(vec![(Kind::MacroCall, 7)]);
        assert!(read_back::<base_db::Crate>(&[0], &mut reading).is_err(), "another kind");
        assert!(read_back::<MacroCallId>(&[1], &mut reading).is_err(), "past the table");
        assert_eq!(read_back::<MacroCallId>(&[0], &mut reading).unwrap(), call_of(7));
    }

    #[test]
    fn a_relocation_keeps_what_was_added_when_its_scope_unwinds() {
        let mut table = Relocation::new();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            relocating(&mut table, || {
                Words::new().put(&crate_of(11));
                panic!("in the scope");
            })
        }));
        assert!(unwound.is_err());
        assert_eq!(table.len(), 1);
        assert!(!in_scope(), "the scope ended with the unwind");
    }

    #[test]
    #[should_panic(expected = "do not nest")]
    fn relocating_scopes_do_not_nest() {
        let (mut outer, mut inner) = (Relocation::new(), Relocation::new());
        relocating(&mut outer, || relocating(&mut inner, || ()));
    }

    /// On a pool of one worker, two jobs (as two derives expanding for items of their own)
    /// each enter a relocation and an anonymous scope of their own and park in them, the first
    /// on one key and the second on another; the keys end in the other order, so they resume
    /// on the one thread swapped. Each reads its own scopes after its wait, and the jobs after
    /// them none.
    #[test]
    fn jobs_in_scopes_of_their_own_resume_on_one_thread_in_swapped_order_in_their_own() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        use blaze_graph::{Key, pool::Pool, pump::Flights, topology::Topology};
        use parking_lot::Mutex;

        fn scopes_here() -> (Option<Option<u64>>, usize) {
            (unrelocated(Kind::Struct, 0), blank_segments(0))
        }

        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let flights = Arc::new(Flights::<u32>::new());
        let run = |what: &'static str, f: Box<dyn FnOnce() + Send>| {
            let (tx, rx) = crossbeam_channel::unbounded::<Result<(), String>>();
            pool.submit(None, f, move |it| _ = tx.send(it.map_err(|_| format!("{what} panicked"))));
            rx.recv().unwrap().unwrap();
        };
        for round in 0..20u32 {
            let [first, second] = [2 * round, 2 * round + 1].map(|at| {
                let mut bytes = [0u8; 32];
                bytes[..4].copy_from_slice(&at.to_le_bytes());
                bytes[4] = 2;
                Key(bytes)
            });
            let (tx, rx) = crossbeam_channel::unbounded::<Result<u32, String>>();
            let waiting = Arc::new(AtomicUsize::new(0));
            let order = Arc::new(Mutex::new(Vec::new()));
            let parked = |count: usize| {
                while waiting.load(Ordering::SeqCst) < count {
                    std::thread::yield_now();
                }
                run("the probe", Box::new(|| assert_eq!(scopes_here(), (None, 0), "a job came on in another's scopes")));
            };
            let job = |mine: u32, on: Key| {
                let (flights, waiting, order, tx) = (flights.clone(), waiting.clone(), order.clone(), tx.clone());
                pool.submit(
                    None,
                    move || {
                        let own = (Some(Some(u64::from(mine) + 100)), mine as usize + 1);
                        let mut table = Relocation::of(vec![(Kind::Struct, own.0.unwrap().unwrap())]);
                        let mut blank = Anonymous { blank: vec![own.1 as u16], ..Anonymous::default() };
                        relocating(&mut table, || {
                            anonymous(&mut blank, || {
                                assert_eq!(scopes_here(), own, "a job came on in another's scopes");
                                waiting.fetch_add(1, Ordering::SeqCst);
                                assert_eq!(flights.get_or_run(on, || panic!("the test holds the key")), Ok(round));
                                assert_eq!(scopes_here(), own, "a job resumed in another's scopes");
                            })
                        });
                        assert_eq!(scopes_here(), (None, 0), "a job kept scopes it left");
                        order.lock().push(mine);
                        mine
                    },
                    move |it| _ = tx.send(it.map_err(|_| format!("job {mine} of round {round} panicked"))),
                );
            };
            let held = flights.get_or_run(first, || {
                job(0, first);
                parked(1);
                let held = flights.get_or_run(second, || {
                    job(1, second);
                    parked(2);
                    Ok(round)
                });
                assert_eq!(held, Ok(round));
                while order.lock().is_empty() {
                    std::thread::yield_now();
                }
                Ok(round)
            });
            assert_eq!(held, Ok(round));
            let mut ended = [rx.recv().unwrap(), rx.recv().unwrap()];
            ended.sort();
            assert_eq!(ended, [Ok(0), Ok(1)]);
            assert_eq!(*order.lock(), [1, 0], "the jobs did not resume in the swapped order");
            run("a later job", Box::new(|| assert_eq!(scopes_here(), (None, 0), "a job came on in its worker's scopes")));
        }
    }
}
