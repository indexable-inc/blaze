//! The image's symbol table and the interners under it: names, 32-byte ids and symbol keys,
//! each interned once to a `u32`.
//!
//! Each table is 64 shards ([`crate::intern`]), each its entries in reserved regions and an
//! open-addressing index of `u32` indexes beside the high half of their hashes, doubled in
//! place and never moved. A key is hashed once; bits 32..38 of that hash pick its shard
//! and the rest place it in the shard. A batch ([`Symbols::intern_all`],
//! [`Symbols::intern_wide`]) is partitioned by shard and each shard interned by one task, so
//! nothing is locked and nothing is hashed twice; within a shard keys go in the batch's order,
//! so the ids are the same whatever the core count. A lookup is one hash, one probe of a
//! group and one compare of a 20-byte key (or the name's bytes).

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::hash::Hasher as _;

use blaze_codegen::InstanceKey;
use blaze_format::SpanRef;
use blaze_format::cursor::Put;
use blaze_graph::par;
use blaze_hash::FixedHasher;
use blaze_ids::{ContentId, radix};
use blaze_items::ItemKey;
use blaze_mem::MemError;
use smallvec::SmallVec;

use crate::intern::{IdShard, KeyShard, NameShard, Shard, shards};
use crate::model::Sym;
use crate::reloc::GotKind;

/// Fixed, not chosen from the core count: symbol ids order resolution's sets, and the same
/// inputs must give the same ids on every machine.
pub(crate) const SHARDS: usize = 64;
const SHIFT: u32 = 26;
const LOCAL: u32 = (1 << SHIFT) - 1;
/// Below this many keys a batch is interned on the calling thread (in the same order, so to
/// the same ids).
const PARALLEL: usize = 4096;

/// No symbol: an empty cell of the jump table.
pub(crate) const NO_SYM: Sym = Sym(u32::MAX);

/// An interned name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct NameId(pub u32);

/// An interned [`ContentId`] (also an [`InstanceKey`]'s or an [`ItemKey`]'s).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct IdNo(pub u32);

/// A native object: its input's id, and its member when the input is an archive (0
/// otherwise). Ordered by interning, not by content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct Obj {
    pub native: IdNo,
    pub member: u32,
}

/// What a symbol is, its payload interned: 20 bytes, compared and hashed as five words.
/// Keys have no `Ord` (their ids are in interning order): [`Symbols::cmp`] orders them by
/// content, as placement does.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum SymKey {
    /// A function's code, by its instance key.
    Body(IdNo),
    /// A function's jump-table stub: what its callers call.
    Slot(IdNo),
    /// Data a function's code refers to, by its contents.
    Data(IdNo),
    /// A static: its item and its instance.
    Static(IdNo, IdNo),
    /// The Mach-O TLV descriptor of a thread-local static.
    Tlv(IdNo, IdNo),
    /// A name the image's pieces and native objects share: an extern, an export, a native
    /// global.
    Name(NameId),
    /// A native object's local symbol, by its index.
    Local(Obj, u32),
    /// A native object's section, by its index.
    Section(Obj, u32),
    /// The `core::panic::Location` of a span: its anchor and index.
    Position(IdNo, u32),
    /// A source file's name, NUL-terminated, for the locations of its spans.
    FileName(NameId),
    /// A function's CIE (by its bytes and personality) and FDE.
    Cie(IdNo),
    Fde(IdNo),
    /// A function's Windows `UNWIND_INFO` and its language-specific data.
    WinInfo(IdNo),
    /// A native object's call frame record: its section, and the byte it starts at in it.
    Frame(Obj, u32, u32),
    /// A GOT cell of a symbol, of a kind.
    Got(GotKind, Sym),
    /// A PLT stub of an import.
    Plt(Sym),
    /// The zero cell after a DLL's cells in an import address table, by the DLL's name.
    IatEnd(NameId),
    /// A tentative definition merged by name.
    Common(NameId),
    /// The cell holding the address of the current jump table.
    Generation,
    /// The jump table's cells.
    Cells,
    /// The jump table's stubs.
    Stubs,
    /// The live descriptor.
    Descriptor,
    /// A function's own SanitizerCoverage array, by the function's instance key and the
    /// array's index ([`blaze_codegen::Target::Coverage`]), and its PC table's entries.
    Coverage(IdNo, u32),
    CoveragePcs(IdNo),
    /// A dev run's pointer to `blaze_inproc_start` ([`crate::DevRun`]): before every other
    /// key in [`Symbols::cmp_keys`], so first among the pieces of its class and rank.
    StartHook,
    /// A dev run's tier-up counter pairs ([`blaze_codegen::Target::Counter`]): the start of the
    /// cells reservation, each pair at an offset of it ([`crate::counter`]).
    Counters,
    /// A lazy image's stub region ([`crate::lazy`]): one piece after the slot stubs.
    LazyStubs,
    /// A lazy function's entry in the stub region, by its instance key: its slot's first target.
    LazyEntry(IdNo),
}

const _: () = assert!(size_of::<SymKey>() == 20);
const _: () = assert!(size_of::<Obj>() == 8);

impl SymKey {
    /// The variant, then up to four payload words: what the key is hashed as.
    #[inline]
    fn words(self) -> [u32; 5] {
        use SymKey::*;
        match self {
            Body(a) => [0, a.0, 0, 0, 0],
            Slot(a) => [1, a.0, 0, 0, 0],
            Data(a) => [2, a.0, 0, 0, 0],
            Static(a, b) => [3, a.0, b.0, 0, 0],
            Tlv(a, b) => [4, a.0, b.0, 0, 0],
            Name(a) => [5, a.0, 0, 0, 0],
            Local(o, a) => [6, o.native.0, o.member, a, 0],
            Section(o, a) => [7, o.native.0, o.member, a, 0],
            Position(a, b) => [8, a.0, b, 0, 0],
            FileName(a) => [9, a.0, 0, 0, 0],
            Cie(a) => [10, a.0, 0, 0, 0],
            Fde(a) => [11, a.0, 0, 0, 0],
            WinInfo(a) => [12, a.0, 0, 0, 0],
            Frame(o, a, b) => [13, o.native.0, o.member, a, b],
            Got(kind, a) => [14, kind as u32, a.0, 0, 0],
            Plt(a) => [15, a.0, 0, 0, 0],
            IatEnd(a) => [16, a.0, 0, 0, 0],
            Common(a) => [17, a.0, 0, 0, 0],
            Generation => [18, 0, 0, 0, 0],
            Cells => [19, 0, 0, 0, 0],
            Stubs => [20, 0, 0, 0, 0],
            Descriptor => [21, 0, 0, 0, 0],
            Coverage(a, b) => [23, a.0, b, 0, 0],
            CoveragePcs(a) => [24, a.0, 0, 0, 0],
            StartHook => [25, 0, 0, 0, 0],
            Counters => [26, 0, 0, 0, 0],
            LazyStubs => [27, 0, 0, 0, 0],
            LazyEntry(a) => [28, a.0, 0, 0, 0],
        }
    }
}

/// A key as parallel code builds it without the table: its ids and name not yet interned,
/// borrowed from codegen's data. [`Symbols::intern_wide`] interns a batch of them.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Wide<'a> {
    Key(SymKey),
    Name(&'a str),
    Body(&'a InstanceKey),
    Slot(&'a InstanceKey),
    Data(&'a ContentId),
    Static(&'a ItemKey, &'a ContentId),
    Position(&'a SpanRef),
    /// [`SymKey::Coverage`]: the function the array is of, and the array.
    Coverage(&'a InstanceKey, u32),
}

const K0: u64 = 0x9e37_79b9_7f4a_7c15;
const K1: u64 = 0xc2b2_ae3d_27d4_eb4f;
const K2: u64 = 0x1656_67b1_9e37_79f9;

/// The high and low halves of the 128-bit product, folded (wyhash's and foldhash's mixer).
#[inline]
fn fold(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
    (product as u64) ^ ((product >> 64) as u64)
}

#[inline]
pub(crate) fn hash_key(key: SymKey) -> u64 {
    let w = key.words();
    let a = u64::from(w[0]) | (u64::from(w[1]) << 32);
    let b = u64::from(w[2]) | (u64::from(w[3]) << 32);
    fold(fold(a ^ K0, b ^ K1) ^ u64::from(w[4]), K2)
}

#[inline]
pub(crate) fn hash_name(name: &str) -> u64 {
    let mut hasher = FixedHasher::default();
    hasher.write(name.as_bytes());
    fold(hasher.finish() ^ name.len() as u64, K0)
}

/// A content id is a BLAKE3 hash: its first eight bytes are its hash.
#[inline]
pub(crate) fn hash_id(id: &ContentId) -> u64 {
    u64::from_le_bytes(id.0[..8].try_into().expect("eight bytes"))
}

#[inline]
fn shard_of(hash: u64) -> usize {
    (hash >> 32) as usize & (SHARDS - 1)
}

#[inline]
fn join(shard: usize, local: u32) -> u32 {
    assert!(local <= LOCAL, "more than 2^26 entries in one shard of a symbol table");
    ((shard as u32) << SHIFT) | local
}

#[inline]
fn split(it: u32) -> (usize, usize) {
    ((it >> SHIFT) as usize, (it & LOCAL) as usize)
}

/// Interns a batch: each key hashed once, partitioned by shard (a stable radix sort by shard, so
/// each shard's keys keep the batch's order), each shard's keys interned by one task. Appends
/// `wrap` of the joined (shard, index) of each key to `out`. Below [`PARALLEL`] keys nothing is
/// allocated but `out`'s growth; above, two buffers of a pair per key.
fn batch<K, S, T>(shards: &mut [S], keys: &[K], hash: impl Fn(K) -> u64 + Sync, wrap: impl Fn(u32) -> T, out: &mut Vec<T>)
where
    K: Copy + Sync,
    S: Shard<K>,
    T: Copy,
{
    out.reserve(keys.len());
    if keys.len() < PARALLEL {
        out.extend(keys.iter().map(|key| {
            let hash = hash(*key);
            let shard = shard_of(hash);
            wrap(join(shard, shards[shard].get_or_insert(hash, *key)))
        }));
        return;
    }
    let mut hashed: Vec<(u32, u64)> = Vec::with_capacity(keys.len());
    hashed.clear();
    par::extend(&mut hashed, par::enumerate(keys), |(at, key)| (at as u32, hash(*key)));
    // Stable, so each shard's keys keep the batch's order; one 8-bit pass.
    radix::sort_copy_by_key(&mut hashed, |it| shard_of(it.1) as u8);
    let mut parts: SmallVec<[&mut [(u32, u64)]; SHARDS]> = SmallVec::new();
    let mut rest = hashed.as_mut_slice();
    for shard in 0..SHARDS {
        let len = rest.partition_point(|it| shard_of(it.1) == shard);
        let (part, tail) = std::mem::take(&mut rest).split_at_mut(len);
        parts.push(part);
        rest = tail;
    }
    par::for_each(par::enumerate(par::zip(shards, parts.as_mut_slice())), |(shard, (it, items))| {
        for item in items.iter_mut() {
            item.1 = u64::from(join(shard, it.get_or_insert(item.1, keys[item.0 as usize])));
        }
    });
    drop(parts);
    let base = out.len();
    out.resize(base + keys.len(), wrap(0));
    for (at, it) in &hashed {
        out[base + *at as usize] = wrap(*it as u32);
    }
}

/// The image's symbols, names and ids. Append-only for the image's life: a failed stage's
/// symbols stay, unused.
pub(crate) struct Symbols {
    keys: Vec<KeyShard>,
    names: Vec<NameShard>,
    ids: Vec<IdShard>,
}

impl Symbols {
    /// Reserves the tables' regions, committing nothing until a key goes in: once per image,
    /// three allocations (the lists of shards).
    pub fn new() -> Result<Symbols, MemError> {
        Ok(Symbols { keys: shards(KeyShard::new)?, names: shards(NameShard::new)?, ids: shards(IdShard::new)? })
    }
}

impl Default for Symbols {
    /// [`Symbols::new`], stopping with the error when the address space can't be reserved.
    fn default() -> Self {
        Symbols::new().unwrap_or_else(|error| panic!("{error}"))
    }
}

impl Symbols {
    pub const SHARDS: usize = SHARDS;

    pub fn name(&mut self, name: &str) -> NameId {
        let hash = hash_name(name);
        let shard = shard_of(hash);
        NameId(join(shard, self.names[shard].get_or_insert(hash, name)))
    }

    pub fn find_name(&self, name: &str) -> Option<NameId> {
        let hash = hash_name(name);
        let shard = shard_of(hash);
        self.names[shard].find(hash, name).map(|at| NameId(join(shard, at)))
    }

    pub fn str(&self, name: NameId) -> &str {
        let (shard, at) = split(name.0);
        self.names[shard].get(at)
    }

    /// Interns every name, the shards in parallel: one buffer, the result.
    pub fn names_all(&mut self, names: &[&str]) -> Vec<NameId> {
        let mut out = Vec::with_capacity(names.len());
        batch(&mut self.names, names, hash_name, NameId, &mut out);
        out
    }

    pub fn id(&mut self, id: &ContentId) -> IdNo {
        let hash = hash_id(id);
        let shard = shard_of(hash);
        IdNo(join(shard, self.ids[shard].get_or_insert(hash, id)))
    }

    pub fn find_id(&self, id: &ContentId) -> Option<IdNo> {
        let hash = hash_id(id);
        let shard = shard_of(hash);
        self.ids[shard].find(hash, id).map(|at| IdNo(join(shard, at)))
    }

    pub fn ids_all(&mut self, ids: &[&ContentId]) -> Vec<IdNo> {
        let mut out = Vec::with_capacity(ids.len());
        batch(&mut self.ids, ids, hash_id, IdNo, &mut out);
        out
    }

    pub fn content(&self, id: IdNo) -> &ContentId {
        let (shard, at) = split(id.0);
        self.ids[shard].id(at)
    }

    pub fn instance(&self, id: IdNo) -> InstanceKey {
        InstanceKey(*self.content(id))
    }

    pub fn item(&self, id: IdNo) -> ItemKey {
        ItemKey(*self.content(id))
    }

    pub fn span(&self, anchor: IdNo, index: u32) -> SpanRef {
        SpanRef { anchor: *self.content(anchor), index }
    }

    pub fn obj(&mut self, native: &ContentId, member: u32) -> Obj {
        Obj { native: self.id(native), member }
    }

    pub fn intern(&mut self, key: SymKey) -> Sym {
        let hash = hash_key(key);
        let shard = shard_of(hash);
        Sym(join(shard, self.keys[shard].get_or_insert(hash, key)))
    }

    /// Interns every key, the shards in parallel; the symbols in the keys' order.
    pub fn intern_all(&mut self, keys: &[SymKey]) -> Vec<Sym> {
        let mut out = Vec::with_capacity(keys.len());
        batch(&mut self.keys, keys, hash_key, Sym, &mut out);
        out
    }

    /// Interns keys whose ids and names are not interned yet: the ids, then the names, then
    /// the keys, each a batch by shard. Six buffers whatever the batch's size: the ids and
    /// names gathered, each interned, the keys made of them, and the result.
    pub fn intern_wide(&mut self, keys: &[Wide<'_>]) -> Vec<Sym> {
        let mut ids: Vec<&ContentId> = Vec::with_capacity(keys.len());
        let mut names: Vec<&str> = Vec::with_capacity(keys.iter().filter(|it| matches!(it, Wide::Name(_))).count());
        for key in keys {
            match *key {
                Wide::Key(_) => {}
                Wide::Name(name) => names.push(name),
                Wide::Body(it) | Wide::Slot(it) | Wide::Coverage(it, _) => ids.push(&it.0),
                Wide::Data(it) => ids.push(it),
                Wide::Static(item, instance) => {
                    ids.push(&item.0);
                    ids.push(instance);
                }
                Wide::Position(span) => ids.push(&span.anchor),
            }
        }
        let ids = self.ids_all(&ids);
        let names = self.names_all(&names);
        let (mut id, mut name) = (ids.iter().copied(), names.iter().copied());
        let mut next = || id.next().expect("one id per id of the keys");
        let mut compact: Vec<SymKey> = Vec::with_capacity(keys.len());
        compact.extend(keys.iter().map(|key| match *key {
            Wide::Key(key) => key,
            Wide::Name(_) => SymKey::Name(name.next().expect("one name per name of the keys")),
            Wide::Body(_) => SymKey::Body(next()),
            Wide::Slot(_) => SymKey::Slot(next()),
            Wide::Data(_) => SymKey::Data(next()),
            Wide::Static(..) => {
                let item = next();
                SymKey::Static(item, next())
            }
            Wide::Position(span) => SymKey::Position(next(), span.index),
            Wide::Coverage(_, array) => SymKey::Coverage(next(), array),
        }));
        self.intern_all(&compact)
    }

    pub fn find(&self, key: SymKey) -> Option<Sym> {
        let hash = hash_key(key);
        let shard = shard_of(hash);
        self.keys[shard].find(hash, key).map(|at| Sym(join(shard, at)))
    }

    #[inline]
    pub fn key(&self, sym: Sym) -> SymKey {
        let (shard, at) = split(sym.0);
        self.keys[shard].key(at)
    }

    pub fn sym_name(&mut self, name: &str) -> Sym {
        let name = self.name(name);
        self.intern(SymKey::Name(name))
    }

    pub fn find_sym_name(&self, name: &str) -> Option<Sym> {
        self.find(SymKey::Name(self.find_name(name)?))
    }

    /// The name a symbol is, when its key is a name.
    pub fn name_of(&self, sym: Sym) -> Option<&str> {
        match self.key(sym) {
            SymKey::Name(name) => Some(self.str(name)),
            _ => None,
        }
    }

    pub fn body(&mut self, key: &InstanceKey) -> Sym {
        let id = self.id(&key.0);
        self.intern(SymKey::Body(id))
    }

    pub fn find_body(&self, key: &InstanceKey) -> Option<Sym> {
        self.find(SymKey::Body(self.find_id(&key.0)?))
    }

    pub fn find_slot(&self, key: &InstanceKey) -> Option<Sym> {
        self.find(SymKey::Slot(self.find_id(&key.0)?))
    }

    /// Two symbols in the order of their contents: the variant, then names as strings and ids
    /// as bytes (what the keys' derived order was when they held them).
    pub fn cmp(&self, a: Sym, b: Sym) -> Ordering {
        if a == b {
            return Ordering::Equal;
        }
        self.cmp_keys(self.key(a), self.key(b))
    }

    pub fn cmp_keys(&self, a: SymKey, b: SymKey) -> Ordering {
        use SymKey::*;
        let id = |x: IdNo, y: IdNo| if x == y { Ordering::Equal } else { self.content(x).cmp(self.content(y)) };
        let name = |x: NameId, y: NameId| if x == y { Ordering::Equal } else { self.str(x).cmp(self.str(y)) };
        let obj = |x: Obj, y: Obj| id(x.native, y.native).then(x.member.cmp(&y.member));
        match (a, b) {
            (Body(x), Body(y))
            | (Slot(x), Slot(y))
            | (Data(x), Data(y))
            | (Cie(x), Cie(y))
            | (Fde(x), Fde(y))
            | (WinInfo(x), WinInfo(y))
            | (CoveragePcs(x), CoveragePcs(y))
            | (LazyEntry(x), LazyEntry(y)) => id(x, y),
            (Static(x, i), Static(y, j)) | (Tlv(x, i), Tlv(y, j)) => id(x, y).then_with(|| id(i, j)),
            (Name(x), Name(y)) | (FileName(x), FileName(y)) | (IatEnd(x), IatEnd(y)) | (Common(x), Common(y)) => {
                name(x, y)
            }
            (Local(x, i), Local(y, j)) | (Section(x, i), Section(y, j)) => obj(x, y).then(i.cmp(&j)),
            (Position(x, i), Position(y, j)) | (Coverage(x, i), Coverage(y, j)) => id(x, y).then(i.cmp(&j)),
            (Frame(x, s, i), Frame(y, t, j)) => obj(x, y).then(s.cmp(&t)).then(i.cmp(&j)),
            (Got(k, x), Got(l, y)) => k.cmp(&l).then_with(|| self.cmp(x, y)),
            (Plt(x), Plt(y)) => self.cmp(x, y),
            (StartHook, StartHook) => Ordering::Equal,
            (StartHook, _) => Ordering::Less,
            (_, StartHook) => Ordering::Greater,
            _ => a.words()[0].cmp(&b.words()[0]),
        }
    }

    /// A symbol as an error names it.
    pub fn describe(&self, sym: Sym) -> String {
        // alloc: error messages: the one string the message is written into
        let mut out = String::new();
        let _ = match self.key(sym) {
            SymKey::Body(key) | SymKey::Slot(key) => write!(out, "the function {:?}", self.instance(key)),
            SymKey::Static(item, instance) => write!(out, "the static {:?} ({:?})", self.item(item), self.content(instance)),
            SymKey::Name(name) => write!(out, "`{}`", self.str(name)),
            SymKey::Data(id) => write!(out, "the data {:?}", self.content(id)),
            SymKey::Coverage(key, array) => {
                write!(out, "SanitizerCoverage array {array} of the function {:?}", self.instance(key))
            }
            SymKey::CoveragePcs(key) => write!(out, "the SanitizerCoverage PC table of the function {:?}", self.instance(key)),
            SymKey::LazyEntry(key) => write!(out, "the lazy entry of the function {:?}", self.instance(key)),
            key => self.write_debug(key, &mut out),
        };
        out
    }

    /// A key written out with its contents, as `{:?}` of the key once was.
    pub fn debug(&self, key: SymKey) -> String {
        // alloc: error messages: the one string the key is written into
        let mut out = String::new();
        let _ = self.write_debug(key, &mut out);
        out
    }

    fn write_debug(&self, key: SymKey, out: &mut String) -> std::fmt::Result {
        let obj = |out: &mut String, o: Obj| write!(out, "Obj {{ native: {:?}, member: {} }}", self.content(o.native), o.member);
        match key {
            SymKey::Body(it) => write!(out, "Body({:?})", self.instance(it)),
            SymKey::Slot(it) => write!(out, "Slot({:?})", self.instance(it)),
            SymKey::Data(it) => write!(out, "Data({:?})", self.content(it)),
            SymKey::Static(a, b) => write!(out, "Static({:?}, {:?})", self.item(a), self.content(b)),
            SymKey::Tlv(a, b) => write!(out, "Tlv({:?}, {:?})", self.item(a), self.content(b)),
            SymKey::Name(it) => write!(out, "Name({:?})", self.str(it)),
            SymKey::Local(o, at) => {
                out.push_str("Local(");
                obj(out, o)?;
                write!(out, ", {at})")
            }
            SymKey::Section(o, at) => {
                out.push_str("Section(");
                obj(out, o)?;
                write!(out, ", {at})")
            }
            SymKey::Position(anchor, index) => write!(out, "Position({:?})", self.span(anchor, index)),
            SymKey::FileName(it) => write!(out, "FileName({:?})", self.str(it)),
            SymKey::Cie(it) => write!(out, "Cie({:?})", self.content(it)),
            SymKey::Fde(it) => write!(out, "Fde({:?})", self.instance(it)),
            SymKey::WinInfo(it) => write!(out, "WinInfo({:?})", self.instance(it)),
            SymKey::Frame(o, section, at) => {
                out.push_str("Frame(");
                obj(out, o)?;
                write!(out, ", {section}, {at})")
            }
            SymKey::Got(kind, it) => {
                write!(out, "Got({kind:?}, ")?;
                self.write_debug(self.key(it), out)?;
                out.push(')');
                Ok(())
            }
            SymKey::Plt(it) => {
                out.push_str("Plt(");
                self.write_debug(self.key(it), out)?;
                out.push(')');
                Ok(())
            }
            SymKey::IatEnd(it) => write!(out, "IatEnd({:?})", self.str(it)),
            SymKey::Common(it) => write!(out, "Common({:?})", self.str(it)),
            SymKey::Generation => write!(out, "Generation"),
            SymKey::Cells => write!(out, "Cells"),
            SymKey::Stubs => write!(out, "Stubs"),
            SymKey::Descriptor => write!(out, "Descriptor"),
            SymKey::Coverage(it, array) => write!(out, "Coverage({:?}, {array})", self.instance(it)),
            SymKey::CoveragePcs(it) => write!(out, "CoveragePcs({:?})", self.instance(it)),
            SymKey::StartHook => write!(out, "StartHook"),
            SymKey::Counters => write!(out, "Counters"),
            SymKey::LazyStubs => write!(out, "LazyStubs"),
            SymKey::LazyEntry(it) => write!(out, "LazyEntry({:?})", self.instance(it)),
        }
    }

    /// The symbol's key as bytes of its contents (the variant, ids' bytes, names' lengths and
    /// bytes, words little-endian): the same for the same key in any image, for hashes.
    pub fn write_key(&self, sym: Sym, out: &mut Vec<u8>) {
        let key = self.key(sym);
        let words = key.words();
        out.push(words[0] as u8);
        let id = |out: &mut Vec<u8>, it: IdNo| out.extend_from_slice(&self.content(it).0);
        match key {
            SymKey::Body(a)
            | SymKey::Slot(a)
            | SymKey::Data(a)
            | SymKey::Cie(a)
            | SymKey::Fde(a)
            | SymKey::WinInfo(a)
            | SymKey::CoveragePcs(a)
            | SymKey::LazyEntry(a) => id(out, a),
            SymKey::Static(a, b) | SymKey::Tlv(a, b) => {
                id(out, a);
                id(out, b);
            }
            SymKey::Name(a) | SymKey::FileName(a) | SymKey::IatEnd(a) | SymKey::Common(a) => {
                out.put_bytes_u64(self.str(a).as_bytes());
            }
            SymKey::Local(o, a) | SymKey::Section(o, a) => {
                id(out, o.native);
                out.put_u32(o.member);
                out.put_u32(a);
            }
            SymKey::Position(a, index) | SymKey::Coverage(a, index) => {
                id(out, a);
                out.put_u32(index);
            }
            SymKey::Frame(o, section, at) => {
                id(out, o.native);
                out.put_u32(o.member);
                out.put_u32(section);
                out.put_u32(at);
            }
            SymKey::Got(kind, target) => {
                out.push(kind as u8);
                self.write_key(target, out);
            }
            SymKey::Plt(target) => self.write_key(target, out),
            SymKey::Generation
            | SymKey::Cells
            | SymKey::Stubs
            | SymKey::Descriptor
            | SymKey::StartHook
            | SymKey::Counters
            | SymKey::LazyStubs => {}
        }
    }

    /// Where a symbol's per-symbol state is kept: its shard and its index in it.
    #[inline]
    pub fn split(sym: Sym) -> (usize, usize) {
        split(sym.0)
    }

    #[inline]
    pub fn join(shard: usize, at: usize) -> Sym {
        Sym(join(shard, at as u32))
    }

    /// A symbol's index in a dense column of every symbol: its place in its shard times the
    /// shard count, plus its shard. It never changes as symbols are added, and shards fill
    /// evenly, so a column of [`Symbols::dense_len`] is within a few percent of the count.
    #[inline]
    pub fn dense(sym: Sym) -> usize {
        let (shard, at) = split(sym.0);
        at * SHARDS + shard
    }

    /// The length a column indexed by [`Symbols::dense`] needs.
    pub fn dense_len(&self) -> usize {
        self.keys.iter().map(|it| it.len()).max().unwrap_or(0) * SHARDS
    }

    /// Of an interned id, the same.
    #[inline]
    pub fn split_id(id: IdNo) -> (usize, usize) {
        split(id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(n: u32) -> ContentId {
        ContentId::of("blaze v2 link symbols test", &n.to_le_bytes())
    }

    #[test]
    fn interning_is_idempotent_and_parallel_interning_agrees() {
        let mut symbols = Symbols::default();
        let a = symbols.sym_name("a");
        assert_eq!(symbols.sym_name("a"), a);
        let names: Vec<String> = (0..20_000).map(|n| format!("n{n}")).collect();
        let keys: Vec<SymKey> = names.iter().map(|it| SymKey::Name(symbols.name(it))).collect();
        let syms = symbols.intern_all(&keys);
        let again = symbols.intern_all(&keys);
        assert_eq!(syms, again);
        for (key, sym) in keys.iter().zip(&syms) {
            assert_eq!(symbols.key(*sym), *key);
            assert_eq!(symbols.find(*key), Some(*sym));
        }
        assert_eq!(symbols.name_of(a), Some("a"));
        assert_eq!(symbols.find_sym_name("none"), None);
        assert_eq!(symbols.find_sym_name("n77"), Some(syms[77]));
    }

    /// A batch above and below the parallel threshold gives the same ids: within a shard,
    /// keys go in the batch's order either way.
    #[test]
    fn ids_do_not_depend_on_how_a_batch_is_run() {
        let ids: Vec<ContentId> = (0..(2 * PARALLEL as u32)).map(content).collect();
        let wide: Vec<Wide<'_>> = ids.iter().map(Wide::Data).collect();
        let mut parallel = Symbols::default();
        let a = parallel.intern_wide(&wide);
        let mut serial = Symbols::default();
        let b: Vec<Sym> = wide.chunks(PARALLEL / 4).flat_map(|it| serial.intern_wide(it)).collect();
        let mut one = Symbols::default();
        let c: Vec<Sym> = ids.iter().map(|it| {
            let id = one.id(it);
            one.intern(SymKey::Data(id))
        }).collect();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn wide_keys_intern_their_ids_and_names() {
        let mut symbols = Symbols::default();
        let (f, item, data) = (InstanceKey(content(1)), ItemKey(content(2)), content(3));
        let span = SpanRef { anchor: content(4), index: 9 };
        let wide =
            [Wide::Body(&f), Wide::Slot(&f), Wide::Static(&item, &data), Wide::Name("memcpy"), Wide::Position(&span), Wide::Key(SymKey::Cells)];
        let syms = symbols.intern_wide(&wide);
        assert_eq!(syms[0], symbols.body(&f));
        assert_eq!(Some(syms[1]), symbols.find_slot(&f));
        let SymKey::Static(a, b) = symbols.key(syms[2]) else { panic!("a static") };
        assert_eq!((symbols.item(a), *symbols.content(b)), (item, data));
        assert_eq!(symbols.name_of(syms[3]), Some("memcpy"));
        let SymKey::Position(anchor, index) = symbols.key(syms[4]) else { panic!("a position") };
        assert_eq!(symbols.span(anchor, index), span);
        assert_eq!(symbols.key(syms[5]), SymKey::Cells);
        assert_eq!(symbols.intern_wide(&wide), syms);
    }

    /// A lookup, and interning what is already interned, allocate nothing; a batch of hits
    /// allocates its result only.
    #[test]
    fn hits_and_lookups_allocate_nothing() {
        let mut symbols = Symbols::default();
        let names: Vec<String> = (0..10_000).map(|n| format!("name{n}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ids = symbols.names_all(&refs);
        let keys: Vec<SymKey> = ids.iter().map(|it| SymKey::Name(*it)).collect();
        let syms = symbols.intern_all(&keys);
        let id = content(5);
        let idno = symbols.id(&id);
        blaze_alloc_count::assert_at_most(0, "a symbol lookup and an interner hit", || {
            assert_eq!(symbols.find_sym_name("name4321"), Some(syms[4321]));
            assert_eq!(symbols.find_sym_name("absent"), None);
            assert_eq!(symbols.sym_name("name17"), syms[17]);
            assert_eq!(symbols.name("name17"), ids[17]);
            assert_eq!(symbols.intern(keys[99]), syms[99]);
            assert_eq!(symbols.id(&id), idno);
            assert_eq!(symbols.find_id(&id), Some(idno));
            assert_eq!(symbols.str(ids[3]), "name3");
            assert_eq!(symbols.key(syms[3]), keys[3]);
            assert_eq!(symbols.name_of(syms[3]), Some("name3"));
            assert_eq!(symbols.cmp(syms[3], syms[4]), "name3".cmp("name4"));
        });
        let (again, used) = blaze_alloc_count::measure(|| symbols.intern_all(&keys[..100]));
        assert_eq!(used.allocations, 1, "only the result");
        assert_eq!(again, syms[..100]);
    }

    /// Content order is the variant, then contents, whatever order the keys were interned in.
    #[test]
    fn keys_order_by_content() {
        let mut symbols = Symbols::default();
        let b = symbols.sym_name("b");
        let a = symbols.sym_name("a");
        let x = symbols.intern(SymKey::Generation);
        let (late, early) = (content(10), content(11));
        let (late, early) = if late < early { (early, late) } else { (late, early) };
        let first = symbols.id(&late);
        let second = symbols.id(&early);
        let d1 = symbols.intern(SymKey::Data(first));
        let d2 = symbols.intern(SymKey::Data(second));
        assert_eq!(symbols.cmp(a, b), Ordering::Less);
        assert_eq!(symbols.cmp(d2, d1), Ordering::Less);
        assert_eq!(symbols.cmp(d1, a), Ordering::Less);
        assert_eq!(symbols.cmp(a, x), Ordering::Less);
        let ga = symbols.intern(SymKey::Got(GotKind::Address, a));
        let gb = symbols.intern(SymKey::Got(GotKind::Address, b));
        assert_eq!(symbols.cmp(gb, ga), Ordering::Greater);
        let mut bytes = Vec::new();
        symbols.write_key(ga, &mut bytes);
        assert_eq!(bytes, [&[14u8, 0, 5][..], &1u64.to_le_bytes(), b"a"].concat());
        assert_eq!(symbols.describe(a), "`a`");
        assert_eq!(symbols.describe(ga), "Got(Address, Name(\"a\"))");
    }
}
