//! Copy-on-write storage for an image's state, so that a stage's copy costs one reference
//! count per container and copies only what it writes: [`CowVec`] (a column), [`CowList`]
//! (many short lists in shared chunks, CSR), [`CowBytes`] (a region's bytes, by page); and the
//! dense sets a stage keeps ([`IdSet`], [`SymSet`]).
//!
//! Every chunk, page and table node is a [`Shared`] slot of a static [`Pool`] of its type
//! (`Tag::Link`), made and copied in place through the writing thread's [`Cache`] of it. A
//! container is a three-level table of [`FAN`]-entry nodes over its chunks: a clone shares the
//! root (one reference count), the first write after it copies the nodes on the path to what it
//! writes and the chunk, and a container never written after the clone costs nothing. Slots
//! come back to their pool when the last state holding them goes, so once a relink's pools and
//! caches are warm its copies reuse what the history gave back and it never calls the global
//! allocator.
//!
//! Rust has no generic statics, so each element type names its pools by implementing
//! [`Pools`] for every slot type it uses: [`chunked!`] does that for a column's or list
//! arena's element type.

#![forbid(unsafe_code)]

use std::alloc::{Allocator, Global};
use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use blaze_ids::{bits::Bits, radix};
use blaze_mem::pool::Counted;
use blaze_mem::{Cache, Pool, Shared, Tag};

use crate::model::Sym;
use crate::symbols::Symbols;

const CHUNK_BITS: u32 = 10;
/// Elements in a column's chunk and in a list arena's shared chunk.
pub(crate) const CHUNK: usize = 1 << CHUNK_BITS;
/// The chunk classes of a list longer than [`CHUNK`], each four times the last: such a list
/// gets a chunk of its own, of the first class that holds it.
pub(crate) const C4K: usize = CHUNK << 2;
pub(crate) const C16K: usize = CHUNK << 4;
pub(crate) const C64K: usize = CHUNK << 6;
pub(crate) const C256K: usize = CHUNK << 8;
pub(crate) const C1M: usize = CHUNK << 10;
pub(crate) const C4M: usize = CHUNK << 12;
pub(crate) const C16M: usize = CHUNK << 14;
const CLASSES: [usize; 8] = [CHUNK, C4K, C16K, C64K, C256K, C1M, C4M, C16M];
/// The longest list: 2^24 elements, in one slice.
const LONGEST_LIST: usize = C16M;

const FAN_BITS: u32 = 6;
/// Entries in a table node.
const FAN: usize = 1 << FAN_BITS;
/// Entries a table indexes, three levels of nodes: 2^18 chunks or pages.
const ENTRIES: usize = FAN * FAN * FAN;
/// The longest column: 2^28 elements, past the 2^26 symbols an image holds.
const LONGEST_COLUMN: usize = ENTRIES * CHUNK;

/// Address space each pool reserves, committed only as slots are carved: 32 GiB for a pool of
/// column chunks or pages, 8 GiB for a class of longer lists, 2 GiB (a million nodes) for a
/// pool of table nodes. Past it the pool stops with the `MemError` naming `Tag::Link`.
pub(crate) const CHUNKS: usize = 1 << 35;
pub(crate) const LONG: usize = 1 << 33;
pub(crate) const NODES: usize = 1 << 31;

/// A static pool of `A`s, reserved on first use.
pub(crate) struct LazyPool<A: 'static> {
    pool: OnceLock<Pool<Counted<A>>>,
    bytes: usize,
}

impl<A: Send + 'static> LazyPool<A> {
    pub(crate) const fn new(bytes: usize) -> LazyPool<A> {
        LazyPool { pool: OnceLock::new(), bytes }
    }

    fn pool(&'static self) -> &'static Pool<Counted<A>> {
        self.pool.get_or_init(|| {
            let slots = (self.bytes / size_of::<Counted<A>>()).clamp(1, u32::MAX as usize) as u32;
            match Pool::new(slots, Tag::Link) {
                Ok(pool) => pool,
                Err(error) => panic!("the image state's pool of {} B slots: {error}", size_of::<Counted<A>>()),
            }
        })
    }
}

/// A thread's cache of a [`LazyPool`], made on its first use.
pub(crate) struct Local<A: 'static>(RefCell<Option<Cache<'static, Counted<A>>>>);

impl<A: Send + 'static> Local<A> {
    pub(crate) const fn new() -> Local<A> {
        Local(RefCell::new(None))
    }

    #[inline]
    pub(crate) fn with<R>(&self, pool: &'static LazyPool<A>, f: impl FnOnce(&mut Cache<'static, Counted<A>>) -> R) -> R {
        let mut cache = self.0.borrow_mut();
        f(cache.get_or_insert_with(|| pool.pool().cache()))
    }
}

/// A family of slots (an element type, or [`CowBytes`]' pages) whose `A`s live in one static
/// pool, and each thread's cache of it.
pub(crate) trait Pools<A: 'static> {
    fn with<R>(f: impl FnOnce(&mut Cache<'static, Counted<A>>) -> R) -> R;
}

/// Gives `$family` a static pool of `$slot`s reserving `$bytes`, and each thread a cache of it.
macro_rules! pool {
    ($family:ty, $slot:ty, $bytes:expr) => {
        impl $crate::cow::Pools<$slot> for $family {
            #[inline]
            fn with<R>(f: impl FnOnce(&mut ::blaze_mem::Cache<'static, ::blaze_mem::pool::Counted<$slot>>) -> R) -> R {
                static POOL: $crate::cow::LazyPool<$slot> = $crate::cow::LazyPool::new($bytes);
                ::std::thread_local! {
                    static CACHE: $crate::cow::Local<$slot> = const { $crate::cow::Local::new() };
                }
                CACHE.with(|cache| cache.with(&POOL, f))
            }
        }
    };
}

/// Makes each type an element of [`CowVec`] and [`CowList`]: the pools of its chunk classes and
/// of its tables' nodes. Each pool is reserved on first use, so a column's type never reserves
/// the longer lists' classes.
macro_rules! chunked {
    ($($t:ty),* $(,)?) => {$(
        $crate::cow::pool!($t, [$t; $crate::cow::CHUNK], $crate::cow::CHUNKS);
        $crate::cow::pool!($t, [$t; $crate::cow::C4K], $crate::cow::LONG);
        $crate::cow::pool!($t, [$t; $crate::cow::C16K], $crate::cow::LONG);
        $crate::cow::pool!($t, [$t; $crate::cow::C64K], $crate::cow::LONG);
        $crate::cow::pool!($t, [$t; $crate::cow::C256K], $crate::cow::LONG);
        $crate::cow::pool!($t, [$t; $crate::cow::C1M], $crate::cow::LONG);
        $crate::cow::pool!($t, [$t; $crate::cow::C4M], $crate::cow::LONG);
        $crate::cow::pool!($t, [$t; $crate::cow::C16M], $crate::cow::LONG);
        $crate::cow::pool!($t, $crate::cow::Leaf<$crate::cow::Chunk<$t>>, $crate::cow::NODES);
        $crate::cow::pool!($t, $crate::cow::Mid<$crate::cow::Chunk<$t>>, $crate::cow::NODES);
        $crate::cow::pool!($t, $crate::cow::Top<$crate::cow::Chunk<$t>>, $crate::cow::NODES);
        impl $crate::cow::Chunked for $t {}
    )*};
}

pub(crate) use {chunked, pool};

/// An element of a [`CowVec`] or [`CowList`]: [`chunked!`] implements it. Chunks are filled
/// by cloning, so the type need not be `Copy` or `Default` (`IdCold` holds an `Arc`): a chunk's
/// slots past the elements hold clones of the value it was made with.
pub(crate) trait Chunked:
    Clone
    + Send
    + Sync
    + 'static
    + Pools<[Self; CHUNK]>
    + Pools<[Self; C4K]>
    + Pools<[Self; C16K]>
    + Pools<[Self; C64K]>
    + Pools<[Self; C256K]>
    + Pools<[Self; C1M]>
    + Pools<[Self; C4M]>
    + Pools<[Self; C16M]>
    + Pools<Leaf<Chunk<Self>>>
    + Pools<Mid<Chunk<Self>>>
    + Pools<Top<Chunk<Self>>>
{
}

chunked!(
    Sym,
    u8,
    u32,
    u64,
    [u32; 2],
    ListRef,
    crate::model::Fixup,
    crate::state::Bind,
    crate::state::Def,
    crate::state::Hot,
    crate::state::What,
    crate::state::Sort,
    crate::state::FunctionInfo,
    blaze_codegen::Line,
);

/// A new slot of `P`'s pool of `[E; N]`: `head`, then clones of `fill`, written in place.
#[inline]
fn share<P, E, const N: usize>(head: &[E], fill: &E) -> Shared<'static, [E; N]>
where
    P: Pools<[E; N]>,
    E: Clone + 'static,
{
    <P as Pools<[E; N]>>::with(|cache| cache.share_from(head, fill))
}

/// The slot to write: first copied into a fresh one while other owners share it, which keep
/// the old one (`Arc::make_mut`'s rule, the copy made in place rather than on the stack).
#[inline]
fn unique<'a, P, E, const N: usize>(shared: &'a mut Shared<'static, [E; N]>) -> &'a mut [E; N]
where
    P: Pools<[E; N]>,
    E: Clone + 'static,
{
    if Shared::owners(shared) != 1 {
        *shared = share::<P, E, N>(&shared[..], &shared[0]);
    }
    Shared::<'static, [E; N]>::get_mut(shared).expect("a slot just copied has one owner")
}

/// [`unique`] for a chunk, whose element type is its family.
#[inline]
fn own<'a, T, const N: usize>(chunk: &'a mut Shared<'static, [T; N]>) -> &'a mut [T; N]
where
    T: Pools<[T; N]> + Clone + 'static,
{
    unique::<T, T, N>(chunk)
}

pub(crate) type Leaf<C> = [Option<C>; FAN];
pub(crate) type Mid<C> = [Option<Shared<'static, Leaf<C>>>; FAN];
pub(crate) type Top<C> = [Option<Shared<'static, Mid<C>>>; FAN];

#[inline]
fn split(at: usize) -> (usize, usize, usize) {
    (at >> (2 * FAN_BITS), (at >> FAN_BITS) & (FAN - 1), at & (FAN - 1))
}

/// The node in `slot` to write: made empty when there is none, copied while shared.
#[inline]
fn node_mut<'a, P, X>(
    slot: &'a mut Option<Shared<'static, [Option<X>; FAN]>>,
) -> &'a mut [Option<X>; FAN]
where
    P: Pools<[Option<X>; FAN]>,
    X: Clone + 'static,
{
    let node = slot.get_or_insert_with(|| share::<P, Option<X>, FAN>(&[], &None));
    unique::<P, Option<X>, FAN>(node)
}

/// Up to [`ENTRIES`] `C`s in three levels of [`FAN`]-entry nodes, each a shared slot of
/// `F`'s pools. A read is three dependent loads to the entry; a clone shares the root; a write
/// copies the (at most three) shared nodes on its path, 64 entries each.
struct Tree<F, C: 'static> {
    top: Option<Shared<'static, Top<C>>>,
    _family: PhantomData<fn() -> F>,
}

impl<F, C: 'static> Clone for Tree<F, C> {
    fn clone(&self) -> Self {
        // clone: one reference count, the root's
        Tree { top: self.top.clone(), _family: PhantomData }
    }
}

impl<F, C: 'static> Default for Tree<F, C> {
    fn default() -> Self {
        Tree { top: None, _family: PhantomData }
    }
}

impl<F, C: 'static> Tree<F, C> {
    #[inline]
    fn get(&self, at: usize) -> Option<&C> {
        let (top, mid, leaf) = split(at);
        self.top.as_ref()?.get(top)?.as_ref()?[mid].as_ref()?[leaf].as_ref()
    }

    /// The leaf node holding entry `at`.
    #[inline]
    fn leaf(&self, at: usize) -> Option<&Leaf<C>> {
        let (top, mid, _) = split(at);
        Some(&**self.top.as_ref()?.get(top)?.as_ref()?[mid].as_ref()?)
    }

    /// Whether both share their root, so every entry.
    fn ptr_eq(&self, other: &Self) -> bool {
        match (&self.top, &other.top) {
            (Some(a), Some(b)) => Shared::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }

    /// Whether any entry of `at..end` is present, skipping absent nodes whole.
    fn any_in(&self, mut at: usize, end: usize) -> bool {
        let Some(top) = &self.top else { return false };
        let end = end.min(ENTRIES);
        while at < end {
            let (t, m, l) = split(at);
            let next_leaf = ((at >> FAN_BITS) + 1) << FAN_BITS;
            let Some(mid) = &top[t] else {
                at = (t + 1) << (2 * FAN_BITS);
                continue;
            };
            let Some(leaf) = &mid[m] else {
                at = next_leaf;
                continue;
            };
            let stop = next_leaf.min(end);
            if leaf[l..l + (stop - at)].iter().any(Option::is_some) {
                return true;
            }
            at = stop;
        }
        false
    }
}

impl<F, C> Tree<F, C>
where
    F: Pools<Leaf<C>> + Pools<Mid<C>> + Pools<Top<C>>,
    C: Clone + 'static,
{
    /// Entry `at` to write, its path's nodes made or copied first.
    #[inline]
    fn entry_mut(&mut self, at: usize) -> &mut Option<C> {
        assert!(at < ENTRIES, "entry {at} of a copy-on-write table, which indexes 2^18");
        let (t, m, l) = split(at);
        let top = node_mut::<F, Shared<'static, Mid<C>>>(&mut self.top);
        let mid = node_mut::<F, Shared<'static, Leaf<C>>>(&mut top[t]);
        let leaf = node_mut::<F, C>(&mut mid[m]);
        &mut leaf[l]
    }

    /// Empties every entry from `from` on, copying only the nodes that change.
    fn truncate(&mut self, from: usize) {
        if from == 0 {
            self.top = None;
            return;
        }
        if from >= ENTRIES {
            return;
        }
        let (t, m, l) = split(from - 1);
        let Some(top) = &self.top else { return };
        let later_top = top[t + 1..].iter().any(Option::is_some);
        let mid = top[t].as_ref();
        let later_mid = mid.is_some_and(|it| it[m + 1..].iter().any(Option::is_some));
        let later_leaf = mid.and_then(|it| it[m].as_ref()).is_some_and(|it| it[l + 1..].iter().any(Option::is_some));
        if !(later_top || later_mid || later_leaf) {
            return;
        }
        let top = node_mut::<F, Shared<'static, Mid<C>>>(&mut self.top);
        top[t + 1..].iter_mut().for_each(|it| *it = None);
        if later_mid || later_leaf {
            let mid = node_mut::<F, Shared<'static, Leaf<C>>>(&mut top[t]);
            mid[m + 1..].iter_mut().for_each(|it| *it = None);
            if later_leaf {
                node_mut::<F, C>(&mut mid[m])[l + 1..].iter_mut().for_each(|it| *it = None);
            }
        }
    }
}

/// A chunk of a column or a list arena: [`CHUNK`] elements, or a longer list's own chunk of
/// one of the larger classes.
pub(crate) enum Chunk<T: 'static> {
    Of1K(Shared<'static, [T; CHUNK]>),
    Of4K(Shared<'static, [T; C4K]>),
    Of16K(Shared<'static, [T; C16K]>),
    Of64K(Shared<'static, [T; C64K]>),
    Of256K(Shared<'static, [T; C256K]>),
    Of1M(Shared<'static, [T; C1M]>),
    Of4M(Shared<'static, [T; C4M]>),
    Of16M(Shared<'static, [T; C16M]>),
}

macro_rules! each_class {
    ($chunk:expr, $it:ident => $body:expr) => {
        match $chunk {
            Chunk::Of1K($it) => $body,
            Chunk::Of4K($it) => $body,
            Chunk::Of16K($it) => $body,
            Chunk::Of64K($it) => $body,
            Chunk::Of256K($it) => $body,
            Chunk::Of1M($it) => $body,
            Chunk::Of4M($it) => $body,
            Chunk::Of16M($it) => $body,
        }
    };
}

impl<T: 'static> Clone for Chunk<T> {
    fn clone(&self) -> Self {
        match self {
            Chunk::Of1K(it) => Chunk::Of1K(it.clone()), // clone: a reference count
            Chunk::Of4K(it) => Chunk::Of4K(it.clone()), // clone: a reference count
            Chunk::Of16K(it) => Chunk::Of16K(it.clone()), // clone: a reference count
            Chunk::Of64K(it) => Chunk::Of64K(it.clone()), // clone: a reference count
            Chunk::Of256K(it) => Chunk::Of256K(it.clone()), // clone: a reference count
            Chunk::Of1M(it) => Chunk::Of1M(it.clone()), // clone: a reference count
            Chunk::Of4M(it) => Chunk::Of4M(it.clone()), // clone: a reference count
            Chunk::Of16M(it) => Chunk::Of16M(it.clone()), // clone: a reference count
        }
    }
}

impl<T: Chunked> Chunk<T> {
    /// A chunk of the first class holding `len` elements: `head`, then clones of `fill`.
    fn new(len: usize, head: &[T], fill: &T) -> Chunk<T> {
        match CLASSES.iter().position(|it| *it >= len) {
            Some(0) => Chunk::Of1K(share::<T, T, CHUNK>(head, fill)),
            Some(1) => Chunk::Of4K(share::<T, T, C4K>(head, fill)),
            Some(2) => Chunk::Of16K(share::<T, T, C16K>(head, fill)),
            Some(3) => Chunk::Of64K(share::<T, T, C64K>(head, fill)),
            Some(4) => Chunk::Of256K(share::<T, T, C256K>(head, fill)),
            Some(5) => Chunk::Of1M(share::<T, T, C1M>(head, fill)),
            Some(6) => Chunk::Of4M(share::<T, T, C4M>(head, fill)),
            Some(7) => Chunk::Of16M(share::<T, T, C16M>(head, fill)),
            _ => panic!("a list of {len} elements: a list arena holds at most 2^24 in one slice"),
        }
    }

    #[inline]
    fn items(&self) -> &[T] {
        each_class!(self, it => &it[..])
    }

    /// The elements to write, the chunk copied first while shared.
    #[inline]
    fn items_mut(&mut self) -> &mut [T] {
        each_class!(self, it => &mut own(it)[..])
    }

    fn ptr_eq(&self, other: &Chunk<T>) -> bool {
        match (self, other) {
            (Chunk::Of1K(a), Chunk::Of1K(b)) => Shared::ptr_eq(a, b),
            (Chunk::Of4K(a), Chunk::Of4K(b)) => Shared::ptr_eq(a, b),
            (Chunk::Of16K(a), Chunk::Of16K(b)) => Shared::ptr_eq(a, b),
            (Chunk::Of64K(a), Chunk::Of64K(b)) => Shared::ptr_eq(a, b),
            (Chunk::Of256K(a), Chunk::Of256K(b)) => Shared::ptr_eq(a, b),
            (Chunk::Of1M(a), Chunk::Of1M(b)) => Shared::ptr_eq(a, b),
            (Chunk::Of4M(a), Chunk::Of4M(b)) => Shared::ptr_eq(a, b),
            (Chunk::Of16M(a), Chunk::Of16M(b)) => Shared::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// A column cloned in O(1), with no allocation: the table and the chunks are shared until
/// written. At most 2^28 elements.
pub(crate) struct CowVec<T: 'static> {
    chunks: Tree<T, Chunk<T>>,
    len: usize,
}

impl<T: 'static> Clone for CowVec<T> {
    fn clone(&self) -> Self {
        CowVec { chunks: self.chunks.clone(), len: self.len } // clone: one reference count
    }
}

impl<T: 'static> Default for CowVec<T> {
    fn default() -> Self {
        CowVec { chunks: Tree::default(), len: 0 }
    }
}

impl<T: Chunked + fmt::Debug> fmt::Debug for CowVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: Chunked> CowVec<T> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, at: usize) -> Option<&T> {
        if at >= self.len {
            return None;
        }
        Some(&self.chunks.get(at >> CHUNK_BITS)?.items()[at & (CHUNK - 1)])
    }

    #[inline]
    pub fn get_mut(&mut self, at: usize) -> Option<&mut T> {
        if at >= self.len {
            return None;
        }
        Some(&mut self.chunks.entry_mut(at >> CHUNK_BITS).as_mut()?.items_mut()[at & (CHUNK - 1)])
    }

    pub fn push(&mut self, value: T) -> usize {
        let at = self.len;
        assert!(at < LONGEST_COLUMN, "a column past 2^28 elements");
        let entry = self.chunks.entry_mut(at >> CHUNK_BITS);
        if at & (CHUNK - 1) == 0 {
            *entry = Some(Chunk::new(CHUNK, std::slice::from_ref(&value), &value));
        } else {
            entry.as_mut().expect("the column's last chunk").items_mut()[at & (CHUNK - 1)] = value;
        }
        self.len += 1;
        at
    }

    /// Sets `at`, growing the column with `fill` to reach it.
    pub fn set(&mut self, at: usize, value: T, fill: impl Fn() -> T) {
        while self.len <= at {
            self.push(fill());
        }
        *self.get_mut(at).expect("grown to it") = value;
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        let len = self.len;
        (0..len.div_ceil(CHUNK)).flat_map(move |chunk| {
            let items = self.chunks.get(chunk).expect("a chunk below the length").items();
            &items[..(len - chunk * CHUNK).min(CHUNK)]
        })
    }
}

/// A list in a [`CowList`]: which chunk, where in it, how long.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct ListRef {
    pub chunk: u32,
    pub start: u32,
    pub len: u32,
}

const _: () = assert!(size_of::<ListRef>() == 12);

impl ListRef {
    pub const EMPTY: ListRef = ListRef { chunk: 0, start: 0, len: 0 };

    pub fn len(self) -> usize {
        self.len as usize
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Many lists in shared chunks (CSR): every piece's fixups in one, every symbol's referrers in
/// another. A list never straddles chunks, so it reads as one slice. Pushing appends to the
/// last chunk of [`CHUNK`] elements (copying it the first time a stage writes it) or starts a
/// new one. A list longer than [`CHUNK`] gets a chunk of its own, of the first class four,
/// sixteen, ... times larger that holds it (at most 2^24 elements, past which the push stops),
/// and the next list starts a new chunk. Replaced lists stay as garbage until the state is
/// compacted.
pub(crate) struct CowList<T: 'static> {
    chunks: Tree<T, Chunk<T>>,
    /// Chunks made.
    count: usize,
    /// Elements in the last chunk.
    fill: usize,
    /// Whether more lists go in the last chunk: a [`CHUNK`] one, not a longer list's own.
    open: bool,
    live: usize,
    garbage: usize,
}

impl<T: 'static> Clone for CowList<T> {
    fn clone(&self) -> Self {
        CowList {
            chunks: self.chunks.clone(), // clone: one reference count
            count: self.count,
            fill: self.fill,
            open: self.open,
            live: self.live,
            garbage: self.garbage,
        }
    }
}

impl<T: 'static> Default for CowList<T> {
    fn default() -> Self {
        CowList { chunks: Tree::default(), count: 0, fill: 0, open: false, live: 0, garbage: 0 }
    }
}

impl<T: 'static> fmt::Debug for CowList<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CowList")
            .field("chunks", &self.count)
            .field("live", &self.live)
            .field("garbage", &self.garbage)
            .finish()
    }
}

impl<T: Chunked> CowList<T> {
    #[inline]
    pub fn get(&self, list: ListRef) -> &[T] {
        if list.len == 0 {
            return &[];
        }
        let start = list.start as usize;
        &self.chunks.get(list.chunk as usize).expect("a list's chunk").items()[start..start + list.len as usize]
    }

    /// The list's elements to write in place; its chunk is copied first when shared.
    pub fn get_mut(&mut self, list: ListRef) -> &mut [T] {
        if list.len == 0 {
            return &mut [];
        }
        let start = list.start as usize;
        let chunk = self.chunks.entry_mut(list.chunk as usize).as_mut().expect("a list's chunk");
        &mut chunk.items_mut()[start..start + list.len as usize]
    }

    pub fn push(&mut self, items: &[T]) -> ListRef {
        let Some(first) = items.first() else { return ListRef::EMPTY };
        let list = self.place(items.len(), items, first);
        self.live += items.len();
        list
    }

    /// A list of `len` copies of `fill`.
    pub fn push_filled(&mut self, len: usize, fill: T) -> ListRef {
        if len == 0 {
            return ListRef::EMPTY;
        }
        let list = self.place(len, &[], &fill);
        self.live += len;
        list
    }

    /// Puts a list of `len`, `head` and then clones of `fill`, at the end of the last chunk when
    /// it fits, else in a new chunk.
    fn place(&mut self, len: usize, head: &[T], fill: &T) -> ListRef {
        assert!(len <= LONGEST_LIST, "a list of {len} elements: a list arena holds at most 2^24 in one slice");
        if self.open && self.fill + len <= CHUNK {
            let (chunk, start) = (self.count - 1, self.fill);
            let last = self.chunks.entry_mut(chunk).as_mut().expect("the arena's last chunk");
            let items = &mut last.items_mut()[start..start + len];
            items[..head.len()].clone_from_slice(head);
            items[head.len()..].fill(fill.clone()); // clone: one element, `Copy` for all but `IdCold`
            self.fill += len;
            return ListRef { chunk: chunk as u32, start: start as u32, len: len as u32 };
        }
        assert!(self.count < ENTRIES, "a list arena past 2^18 chunks");
        let chunk = Chunk::new(len, head, fill);
        self.open = chunk.items().len() == CHUNK;
        *self.chunks.entry_mut(self.count) = Some(chunk);
        self.count += 1;
        self.fill = len;
        ListRef { chunk: (self.count - 1) as u32, start: 0, len: len as u32 }
    }

    /// Counts a list that nothing refers to any more as garbage.
    pub fn free(&mut self, list: ListRef) {
        self.live -= list.len();
        self.garbage += list.len();
    }

    pub fn garbage(&self) -> usize {
        self.garbage
    }

    pub fn live(&self) -> usize {
        self.live
    }
}

/// A set of dense `u32` ids (pieces, slots): a bitset answers membership, and the members are
/// kept in the order they were first added, so iterating costs the members, not the range. A
/// stage's sets live in its arena ([`IdSet::new_in`]), so a warm stage grows them without the
/// global allocator.
#[derive(Debug)]
pub(crate) struct IdSet<A: Allocator + Copy = Global> {
    bits: Bits<Vec<u64, A>, u32>,
    listed: Bits<Vec<u64, A>, u32>,
    list: Vec<u32, A>,
    len: usize,
}

impl Default for IdSet {
    fn default() -> Self {
        IdSet::new_in(Global)
    }
}

impl<A: Allocator + Copy> IdSet<A> {
    pub fn new_in(alloc: A) -> Self {
        IdSet {
            bits: Bits::from_words(Vec::new_in(alloc)),
            listed: Bits::from_words(Vec::new_in(alloc)),
            list: Vec::new_in(alloc),
            len: 0,
        }
    }

    #[inline]
    pub fn contains(&self, id: u32) -> bool {
        self.bits.contains(id)
    }

    pub fn insert(&mut self, id: u32) -> bool {
        if !self.bits.insert_growing(id) {
            return false;
        }
        self.len += 1;
        if self.listed.insert_growing(id) {
            self.list.push(id);
        }
        true
    }

    pub fn remove(&mut self, id: u32) -> bool {
        let removed = self.bits.remove(id);
        self.len -= usize::from(removed);
        removed
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The members, each once, in the order they were first added.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.list.iter().copied().filter(|it| self.contains(*it))
    }

    /// The members in increasing order, in one buffer of exactly their count.
    pub fn sorted(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.len);
        self.sorted_into(&mut out);
        out
    }

    /// The members in increasing order, into `out` (cleared first).
    pub fn sorted_into(&self, out: &mut Vec<u32>) {
        out.clear();
        out.extend(self.iter());
        radix::sort(out);
    }

    pub fn extend(&mut self, ids: impl IntoIterator<Item = u32>) {
        for id in ids {
            self.insert(id);
        }
    }
}

/// A set of symbols: an [`IdSet`] per symbol shard.
#[derive(Debug)]
pub(crate) struct SymSet<A: Allocator + Copy = Global> {
    shards: Vec<IdSet<A>, A>,
    len: usize,
}

impl Default for SymSet {
    fn default() -> Self {
        SymSet::new_in(Global)
    }
}

impl<A: Allocator + Copy> SymSet<A> {
    pub fn new_in(alloc: A) -> Self {
        let mut shards = Vec::with_capacity_in(Symbols::SHARDS, alloc);
        shards.resize_with(Symbols::SHARDS, || IdSet::new_in(alloc));
        SymSet { shards, len: 0 }
    }

    pub fn insert(&mut self, sym: Sym) -> bool {
        let (shard, at) = Symbols::split(sym);
        let new = self.shards[shard].insert(at as u32);
        self.len += usize::from(new);
        new
    }

    pub fn remove(&mut self, sym: Sym) -> bool {
        let (shard, at) = Symbols::split(sym);
        let gone = self.shards[shard].remove(at as u32);
        self.len -= usize::from(gone);
        gone
    }

    pub fn contains(&self, sym: Sym) -> bool {
        let (shard, at) = Symbols::split(sym);
        self.shards[shard].contains(at as u32)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The members, shard by shard, each in the order it was first added.
    pub fn iter(&self) -> impl Iterator<Item = Sym> + '_ {
        self.shards
            .iter()
            .enumerate()
            .flat_map(|(shard, it)| it.iter().map(move |at| Symbols::join(shard, at as usize)))
    }

    pub fn extend(&mut self, syms: impl IntoIterator<Item = Sym>) {
        for sym in syms {
            self.insert(sym);
        }
    }
}

const PAGE: usize = 16 * 1024;

type Page = [u8; PAGE];
type PageRef = Shared<'static, Page>;

/// The longest region: 2^18 pages, 4 GiB. Past rustc's reach too: its default code model
/// keeps code and data within 2 GiB.
const LONGEST_BYTES: usize = ENTRIES * PAGE;

/// The family of [`CowBytes`]' pages and their tables' nodes.
struct Bytes;

pool!(Bytes, Page, CHUNKS);
pool!(Bytes, Leaf<PageRef>, NODES);
pool!(Bytes, Mid<PageRef>, NODES);
pool!(Bytes, Top<PageRef>, NODES);

/// A region's bytes: the buffer the last full link wrote, and the 16 KiB pages written since,
/// each shared with the states it came from until written again. A stage that writes one
/// function copies the page it is in, not the region; reading a range that lies in the
/// unwritten buffer or in one page borrows it. A clone is two reference counts: the page table
/// is shared too, and its nodes on a written page's path are copied by the first write after
/// the clone. At most 4 GiB.
#[derive(Clone, Default)] // clone: two reference counts, as above
pub(crate) struct CowBytes {
    base: Arc<Vec<u8>>,
    /// How much of `base` is still the bytes there: a truncation shortens it, so growing
    /// again reads zeros.
    base_len: usize,
    /// By page of `0..len`: the page as written, or none for `base` (zeros past `base_len`).
    /// Nothing past `len`'s last page.
    pages: Tree<Bytes, PageRef>,
    len: usize,
}

impl fmt::Debug for CowBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let written = (0..self.len.div_ceil(PAGE)).filter(|it| self.pages.get(*it).is_some()).count();
        f.debug_struct("CowBytes")
            .field("len", &self.len)
            .field("base_len", &self.base_len)
            .field("written_pages", &written)
            .finish()
    }
}

/// `have` bytes of the buffer from `at`; `at` may lie past the buffer when `have` is 0, as it
/// does for a page past what the full link wrote.
#[inline]
fn base_at(base: &[u8], base_len: usize, at: usize, have: usize) -> &[u8] {
    &base[at.min(base_len)..][..have]
}

/// What [`CowBytes::same`] compares at a time when neither side can be borrowed.
const COMPARE: usize = 4096;

impl CowBytes {
    pub const PAGE: usize = PAGE;

    /// Takes the buffer a full link wrote: one allocation (its `Arc`), once per region per
    /// full link.
    pub fn from_vec(bytes: Vec<u8>) -> CowBytes {
        let len = bytes.len();
        assert!(len <= LONGEST_BYTES, "a region of {len} bytes: its page table indexes 4 GiB");
        // alloc: once per region per full link (the buffer's `Arc`)
        CowBytes { base: Arc::new(bytes), base_len: len, pages: Tree::default(), len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `at..at + len`, borrowed when it is in the unwritten buffer or in one written page.
    /// Hot paths that can't take a copy use [`CowBytes::slice`], [`CowBytes::copy_to`] or
    /// [`CowBytes::same`] instead.
    pub fn read(&self, at: usize, len: usize) -> Cow<'_, [u8]> {
        if let Some(it) = self.slice(at, len) {
            return Cow::Borrowed(it);
        }
        // alloc: only a range across pages written since the full link; the copy is the caller's
        let mut out = vec![0; len];
        self.copy_to(at, &mut out);
        Cow::Owned(out)
    }

    /// Whether `self[at..at + len]` and `other[other_at..other_at + len]` hold the same bytes,
    /// compared in place: borrowed when both can be, else through two stack buffers.
    pub fn same(&self, at: usize, other: &CowBytes, other_at: usize, len: usize) -> bool {
        if let (Some(a), Some(b)) = (self.slice(at, len), other.slice(other_at, len)) {
            return std::ptr::eq(a, b) || a == b;
        }
        let (mut x, mut y) = ([0u8; COMPARE], [0u8; COMPARE]);
        let (mut a, mut b, mut left) = (at, other_at, len);
        while left > 0 {
            let n = left.min(COMPARE);
            self.copy_to(a, &mut x[..n]);
            other.copy_to(b, &mut y[..n]);
            if x[..n] != y[..n] {
                return false;
            }
            (a, b, left) = (a + n, b + n, left - n);
        }
        true
    }

    /// `at..at + len` if it can be borrowed (see [`CowBytes::read`]).
    pub fn slice(&self, at: usize, len: usize) -> Option<&[u8]> {
        let end = at.checked_add(len).filter(|it| *it <= self.len).expect("a read past the bytes' end");
        if len == 0 {
            return Some(&[]);
        }
        let (first, last) = (at / PAGE, (end - 1) / PAGE);
        if end <= self.base_len && !self.pages.any_in(first, last + 1) {
            return Some(&self.base[at..end]);
        }
        match self.pages.get(first) {
            Some(page) if first == last => Some(&page[at % PAGE..at % PAGE + len]),
            _ => None,
        }
    }

    pub fn copy_to(&self, mut at: usize, mut out: &mut [u8]) {
        assert!(at + out.len() <= self.len, "a read past the bytes' end");
        while !out.is_empty() {
            let (page, from) = (at / PAGE, at % PAGE);
            let n = (PAGE - from).min(out.len());
            let (head, rest) = out.split_at_mut(n);
            match self.pages.get(page) {
                Some(it) => head.copy_from_slice(&it[from..from + n]),
                None => {
                    let have = self.base_len.saturating_sub(at).min(n);
                    head[..have].copy_from_slice(base_at(&self.base, self.base_len, at, have));
                    head[have..].fill(0);
                }
            }
            out = rest;
            at += n;
        }
    }

    /// Page `index` to write: copied from the buffer (or made) the first time, and copied
    /// again while another state shares it.
    fn page_mut(&mut self, index: usize) -> &mut Page {
        let CowBytes { base, base_len, pages, .. } = self;
        let page = pages.entry_mut(index).get_or_insert_with(|| {
            let start = index * PAGE;
            let have = base_len.saturating_sub(start).min(PAGE);
            share::<Bytes, u8, PAGE>(base_at(base, *base_len, start, have), &0)
        });
        unique::<Bytes, u8, PAGE>(page)
    }

    pub fn write(&mut self, at: usize, bytes: &[u8]) {
        if at + bytes.len() > self.len {
            self.resize(at + bytes.len());
        }
        let (mut at, mut bytes) = (at, bytes);
        while !bytes.is_empty() {
            let (page, from) = (at / PAGE, at % PAGE);
            let n = (PAGE - from).min(bytes.len());
            self.page_mut(page)[from..from + n].copy_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            at += n;
        }
    }

    pub fn fill(&mut self, at: usize, len: usize, byte: u8) {
        if at + len > self.len {
            self.resize(at + len);
        }
        let (mut at, end) = (at, at + len);
        while at < end {
            let (page, from) = (at / PAGE, at % PAGE);
            let n = (PAGE - from).min(end - at);
            self.page_mut(page)[from..from + n].fill(byte);
            at += n;
        }
    }

    /// Grows with zeros or truncates.
    pub fn resize(&mut self, len: usize) {
        assert!(len <= LONGEST_BYTES, "a region of {len} bytes: its page table indexes 4 GiB");
        if len < self.len {
            self.base_len = self.base_len.min(len);
            self.pages.truncate(len.div_ceil(PAGE));
            if len % PAGE != 0 {
                let last = len / PAGE;
                if self.pages.get(last).is_some() {
                    self.page_mut(last)[len % PAGE..].fill(0);
                }
            }
        }
        self.len = len;
    }

    /// All the bytes: borrowed when no page was written since the buffer.
    pub fn contiguous(&self) -> Cow<'_, [u8]> {
        if self.len <= self.base_len && !self.pages.any_in(0, self.len.div_ceil(PAGE)) {
            return Cow::Borrowed(&self.base[..self.len]);
        }
        // alloc: the edge: a whole region for the file writer, once per region per write
        let mut out = vec![0; self.len];
        self.copy_to(0, &mut out);
        Cow::Owned(out)
    }

    /// The page-aligned ranges of `self` that may differ from `old`, into `out` (cleared
    /// first): pages both share are skipped without reading them, and each side's leaf node is
    /// found once per 64 pages.
    pub fn changed_since(&self, old: &CowBytes, out: &mut Vec<Range<usize>>) {
        fn entry(leaf: Option<&Leaf<PageRef>>, index: usize) -> Option<&PageRef> {
            leaf.and_then(|it| it[index & (FAN - 1)].as_ref())
        }
        out.clear();
        let same_base = Arc::ptr_eq(&self.base, &old.base);
        let (pages, old_pages) = (self.len.div_ceil(PAGE), old.len.div_ceil(PAGE));
        let mut index = 0;
        while index < pages {
            let group = (((index >> FAN_BITS) + 1) << FAN_BITS).min(pages);
            let (mine, theirs) = (self.pages.leaf(index), old.pages.leaf(index));
            for index in index..group {
                let (start, end) = (index * PAGE, ((index + 1) * PAGE).min(self.len));
                let same = index < old_pages
                    && match (entry(mine, index), entry(theirs, index)) {
                        (Some(a), Some(b)) => Shared::ptr_eq(a, b) && end <= old.len,
                        (None, None) => {
                            let base = |it: &CowBytes| it.base_len.clamp(start, end);
                            same_base && base(self) == base(old) && end <= old.len
                        }
                        _ => false,
                    };
                if same {
                    continue;
                }
                match out.last_mut() {
                    Some(last) if last.end == start => last.end = end,
                    _ => out.push(start..end),
                }
            }
            index = group;
        }
    }
}

/// What a writer's kept table and the file's parts need of their bytes: the runs they are held
/// in, a write that copies only the pages it changes, the blocks that differ from an older
/// copy, and equality by bytes.
impl CowBytes {
    /// Each run of the bytes as (offset, bytes), in order, none empty: the unwritten buffer
    /// between written pages as one run, each written page as its own. The zeros past the
    /// buffer that no page holds are in no run.
    pub fn runs<'s>(&'s self, mut f: impl FnMut(usize, &'s [u8])) {
        let pages = self.len.div_ceil(PAGE);
        if !self.pages.any_in(0, pages) {
            self.base_run(0, self.len, &mut f);
            return;
        }
        let mut unwritten = None;
        for index in 0..pages {
            let start = index * PAGE;
            match self.pages.get(index) {
                Some(page) => {
                    if let Some(from) = unwritten.take() {
                        self.base_run(from, start, &mut f);
                    }
                    f(start, &page[..((index + 1) * PAGE).min(self.len) - start]);
                }
                None => {
                    unwritten.get_or_insert(start);
                }
            }
        }
        if let Some(from) = unwritten {
            self.base_run(from, self.len, &mut f);
        }
    }

    /// The buffer's bytes of `from..to`, where no page is written, as one run.
    fn base_run<'s, F: FnMut(usize, &'s [u8])>(&'s self, from: usize, to: usize, f: &mut F) {
        let to = to.min(self.base_len);
        if from < to {
            f(from, &self.base[from..to]);
        }
    }

    /// [`CowBytes::write`], but a page already holding the bytes is left as it is: only the
    /// pages whose bytes change are copied, and the others stay shared.
    pub fn update(&mut self, at: usize, bytes: &[u8]) {
        if at + bytes.len() > self.len {
            self.resize(at + bytes.len());
        }
        let (mut at, mut bytes) = (at, bytes);
        while !bytes.is_empty() {
            let (page, from) = (at / PAGE, at % PAGE);
            let n = (PAGE - from).min(bytes.len());
            if !self.holds(at, &bytes[..n]) {
                self.page_mut(page)[from..from + n].copy_from_slice(&bytes[..n]);
            }
            bytes = &bytes[n..];
            at += n;
        }
    }

    /// Whether `self[at..at + bytes.len()]`, within one page, is `bytes`.
    fn holds(&self, at: usize, bytes: &[u8]) -> bool {
        if let Some(it) = self.slice(at, bytes.len()) {
            return it == bytes;
        }
        // An unwritten page reaching past the buffer: its bytes, then zeros.
        let have = self.base_len.saturating_sub(at).min(bytes.len());
        bytes[..have] == *base_at(&self.base, self.base_len, at, have) && bytes[have..].iter().all(|it| *it == 0)
    }

    /// Calls `out(start, len)` for each `block` of `self` (the last one shorter) whose bytes may
    /// differ from `old`'s at the same offset, a block past `old`'s end among them. Pages both
    /// share are skipped unread ([`CowBytes::changed_since`], into `scratch`); `block` divides
    /// a page.
    pub fn changed_blocks(
        &self,
        old: &CowBytes,
        block: usize,
        scratch: &mut Vec<Range<usize>>,
        mut out: impl FnMut(usize, usize),
    ) {
        assert!(block > 0 && PAGE.is_multiple_of(block), "a block of {block} bytes, which doesn't divide a page");
        self.changed_since(old, scratch);
        for range in scratch.iter() {
            for start in (range.start..range.end).step_by(block) {
                let end = (start + block).min(range.end);
                if end > old.len || !self.same(start, old, start, end - start) {
                    out(start, end - start);
                }
            }
        }
    }
}

impl PartialEq for CowBytes {
    fn eq(&self, other: &CowBytes) -> bool {
        self.len == other.len && self.same(0, other, 0, self.len)
    }
}

impl Eq for CowBytes {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_shares_until_written() {
        let mut a = CowVec::default();
        for n in 0..3000u32 {
            a.push(n);
        }
        let mut b = a.clone();
        *b.get_mut(5).unwrap() = 99;
        b.set(4000, 7, || 0);
        assert_eq!(a.get(5), Some(&5));
        assert_eq!(b.get(5), Some(&99));
        assert_eq!(a.len(), 3000);
        assert_eq!(b.len(), 4001);
        assert_eq!(b.get(3500), Some(&0));
        assert!(a.chunks.get(1).unwrap().ptr_eq(b.chunks.get(1).unwrap()));
        assert!(!a.chunks.get(0).unwrap().ptr_eq(b.chunks.get(0).unwrap()));
        assert_eq!(a.iter().copied().sum::<u32>(), (0..3000).sum::<u32>());
    }

    #[test]
    fn lists_are_slices_and_a_clone_copies_only_the_chunk_it_writes() {
        let mut a: CowList<u64> = CowList::default();
        let lists: Vec<(ListRef, Vec<u64>)> = (0..5000u64)
            .map(|n| {
                let items: Vec<u64> = (0..n % 7).map(|it| n * 10 + it).collect();
                (a.push(&items), items)
            })
            .collect();
        for (list, items) in &lists {
            assert_eq!(a.get(*list), &items[..]);
        }
        let big: Vec<u64> = (0..20_000).collect();
        let big_list = a.push(&big);
        assert_eq!(a.get(big_list), &big[..]);
        let mut b = a.clone();
        let (first, _) = lists.iter().find(|it| !it.0.is_empty()).expect("a non-empty list");
        b.get_mut(*first)[0] = u64::MAX;
        let added = b.push(&[1, 2, 3]);
        assert_eq!(b.get(added), &[1, 2, 3]);
        assert_eq!(b.get(*first)[0], u64::MAX);
        assert_ne!(a.get(*first)[0], u64::MAX);
        assert_eq!(b.count, a.count + 1, "a list after a longer list's own chunk starts a new one");
        for at in 0..a.count {
            let (x, y) = (a.chunks.get(at).unwrap(), b.chunks.get(at).unwrap());
            assert_eq!(x.ptr_eq(y), at != first.chunk as usize);
        }
        b.free(*first);
        assert_eq!(b.garbage(), first.len());
        assert_eq!(a.garbage(), 0);
    }

    #[test]
    fn id_sets_iterate_members_once_in_order() {
        let mut set = IdSet::default();
        for id in [5, 900, 3, 5, 64, 0] {
            set.insert(id);
        }
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![5, 900, 3, 64, 0]);
        assert!(set.remove(900));
        assert!(!set.remove(900));
        set.insert(900);
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![5, 900, 3, 64, 0]);
        assert_eq!(set.sorted(), vec![0, 3, 5, 64, 900]);
        assert_eq!(set.len(), 5);
        assert!(!set.contains(1_000_000));
        let mut syms = SymSet::default();
        let a = Symbols::join(3, 7);
        assert!(syms.insert(a));
        assert!(!syms.insert(a));
        assert!(syms.contains(a));
        assert_eq!(syms.iter().collect::<Vec<_>>(), vec![a]);
        assert!(syms.remove(a));
        assert!(syms.is_empty());
    }

    /// A stage's sets in its arena: the same members as sets on the heap, and once the arena is
    /// warm, filling them makes no global allocation.
    #[test]
    fn sets_in_a_frame_allocate_nothing() {
        const IDS: u32 = 200_000;
        fn fill<A: Allocator + Copy>(ids: &mut IdSet<A>, syms: &mut SymSet<A>) {
            for n in 0..IDS {
                ids.insert(n.wrapping_mul(7919) % (IDS * 4));
                syms.insert(Symbols::join(n as usize % Symbols::SHARDS, (n / 3) as usize));
            }
            for n in (0..IDS).step_by(5) {
                ids.remove(n);
                syms.remove(Symbols::join(n as usize % Symbols::SHARDS, (n / 3) as usize));
            }
        }
        fn sets_in(frame: &blaze_mem::Frame) -> (IdSet<&blaze_mem::Frame>, SymSet<&blaze_mem::Frame>) {
            let (mut ids, mut syms) = (IdSet::new_in(frame), SymSet::new_in(frame));
            fill(&mut ids, &mut syms);
            (ids, syms)
        }
        fn same<A: Allocator + Copy>(got: (IdSet<A>, SymSet<A>), ids: &IdSet, syms: &SymSet) {
            assert_eq!(got.0.iter().collect::<Vec<_>>(), ids.iter().collect::<Vec<_>>());
            assert_eq!(got.0.sorted(), ids.sorted());
            assert_eq!(got.0.len(), ids.len());
            assert_eq!(got.1.iter().collect::<Vec<_>>(), syms.iter().collect::<Vec<_>>());
            assert_eq!(got.1.len(), syms.len());
        }
        let (mut ids, mut syms) = (IdSet::default(), SymSet::default());
        fill(&mut ids, &mut syms);
        let mut frame = crate::arena::stage_frame().unwrap();
        same(sets_in(&frame), &ids, &syms);
        frame.reset();
        same(blaze_alloc_count::assert_at_most(0, "a warm stage's sets", || sets_in(&frame)), &ids, &syms);
    }

    /// The pages against a plain vector given the same writes.
    #[test]
    fn paged_bytes_read_as_a_vector_would() {
        let start: Vec<u8> = (0..(3 * PAGE + 100)).map(|it| it as u8).collect();
        let a = CowBytes::from_vec(start.clone());
        let mut b = a.clone();
        let mut reference = start.clone();
        let mut write = |b: &mut CowBytes, at: usize, bytes: &[u8]| {
            b.write(at, bytes);
            if reference.len() < at + bytes.len() {
                reference.resize(at + bytes.len(), 0);
            }
            reference[at..at + bytes.len()].copy_from_slice(bytes);
        };
        write(&mut b, PAGE - 3, &[1, 2, 3, 4, 5, 6]);
        write(&mut b, 3 * PAGE + 90, &[9; 40]);
        assert!(matches!(b.read(10, 20), Cow::Borrowed(_)));
        assert!(matches!(b.read(PAGE + 1, 20), Cow::Borrowed(_)));
        assert_eq!(&*b.read(PAGE - 5, 10), &reference[PAGE - 5..PAGE + 5]);
        assert_eq!(&*b.contiguous(), &reference[..]);
        assert_eq!(&*a.contiguous(), &start[..]);
        assert!(matches!(a.contiguous(), Cow::Borrowed(_)));
        let mut changed = Vec::new();
        b.changed_since(&a, &mut changed);
        assert_eq!(changed, vec![0..2 * PAGE, 3 * PAGE..b.len()]);
        assert!(b.same(PAGE + 3, &a, PAGE + 3, 20), "an unwritten range is the same");
        assert!(!b.same(PAGE - 5, &a, PAGE - 5, 10), "a written range differs");
        assert!(b.same(PAGE - 5, &b.clone(), PAGE - 5, PAGE + 10), "a range across pages, copied in parts");
        assert!(b.same(0, &a, 0, PAGE - 3));
        b.resize(PAGE + 10);
        reference.truncate(PAGE + 10);
        b.resize(2 * PAGE);
        reference.resize(2 * PAGE, 0);
        assert_eq!(&*b.contiguous(), &reference[..]);
        b.fill(5, 3, 0xee);
        reference[5..8].fill(0xee);
        assert_eq!(&*b.read(0, 16), &reference[..16]);
    }

    /// A clone of each container allocates nothing and shares its table until written; a read
    /// and a comparison in place allocate nothing.
    #[test]
    fn clones_and_reads_allocate_nothing() {
        let mut column = CowVec::default();
        let mut lists: CowList<u32> = CowList::default();
        for n in 0..5000u32 {
            column.push(n);
            lists.push(&[n, n + 1]);
        }
        let mut bytes = CowBytes::from_vec(vec![1; 5 * PAGE]);
        bytes.write(PAGE + 7, &[2; 3]);
        let list = ListRef { chunk: 0, start: 2, len: 2 };
        let (copies, used) = blaze_alloc_count::measure(|| (column.clone(), lists.clone(), bytes.clone()));
        assert_eq!(used.allocations, 0, "a clone is reference counts");
        let (mut column2, mut lists2, mut bytes2) = copies;
        assert!(column.chunks.ptr_eq(&column2.chunks), "an unwritten clone shares the table");
        assert!(lists.chunks.ptr_eq(&lists2.chunks));
        assert!(bytes.pages.ptr_eq(&bytes2.pages));
        blaze_alloc_count::assert_at_most(0, "column, list and byte reads", || {
            assert_eq!(column2.get(4321), Some(&4321));
            assert_eq!(lists2.get(list), &[1, 2]);
            assert!(bytes2.slice(PAGE + 7, 3).is_some());
            assert!(bytes2.same(0, &bytes, 0, 5 * PAGE));
        });
        // The first writes after a clone warm this thread's caches of the pools (a carve of 64
        // slots each, and the thread-local's registration).
        *column2.get_mut(3).unwrap() = 0;
        lists2.push(&[7, 8]);
        bytes2.write(3 * PAGE, &[3; 5]);
        // Once warm, the first write after a clone copies the table nodes on its path and the
        // chunk or page into slots from those caches: no allocation, whatever the length.
        let (mut column3, mut lists3, mut bytes3) = (column2.clone(), lists2.clone(), bytes2.clone());
        blaze_alloc_count::assert_at_most(0, "a warm first write after a clone", || *column3.get_mut(3000).unwrap() = 0);
        blaze_alloc_count::assert_at_most(0, "a second write to the same chunk", || *column3.get_mut(3001).unwrap() = 0);
        blaze_alloc_count::assert_at_most(0, "a warm list push after a clone", || lists3.push(&[9, 10]));
        blaze_alloc_count::assert_at_most(0, "a warm write of a shared page", || bytes3.write(PAGE + 8, &[4; 9]));
        blaze_alloc_count::assert_at_most(0, "a warm write of an unwritten page", || bytes3.write(4 * PAGE + 1, &[5; 9]));
        assert_eq!(column.get(3), Some(&3));
        assert_eq!(column2.get(3000), Some(&3000));
        assert_eq!(column3.get(3000), Some(&0));
        assert_eq!(lists2.get(list), &[1, 2]);
        assert_eq!(&*bytes2.read(PAGE + 8, 9), &[2, 2, 1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(&*bytes3.read(PAGE + 7, 10), &[2, 4, 4, 4, 4, 4, 4, 4, 4, 4]);
        assert!(!column.chunks.ptr_eq(&column2.chunks));
        assert!(column.chunks.get(1).unwrap().ptr_eq(column2.chunks.get(1).unwrap()));
        assert!(column2.chunks.get(0).unwrap().ptr_eq(column3.chunks.get(0).unwrap()));
    }

    /// Of `Chunked` for the tests alone, so no other test shares its pools.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Probe(u64);

    /// Not `Copy`: what `IdCold` is (it holds an `Arc`).
    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Held(Option<Arc<u32>>);

    chunked!(Probe, Held);

    /// Each column against a plain vector given the same pushes and writes, across clones that
    /// stay in a history of 16 and are written on in turn: the old ones never change.
    #[test]
    fn columns_answer_as_vectors_do_across_clones() {
        let mut rng = blaze_ids::rng::SplitMix64::new(7);
        let mut states: Vec<(CowVec<u64>, Vec<u64>)> = vec![(CowVec::default(), Vec::new())];
        for _ in 0..300 {
            let from = rng.index(states.len());
            let (mut column, mut model) = (states[from].0.clone(), states[from].1.clone());
            for _ in 0..rng.index(8) + 1 {
                match rng.below(4) {
                    0 => {
                        for _ in 0..rng.index(CHUNK + 50) {
                            let value = rng.next_u64();
                            assert_eq!(column.push(value), model.len());
                            model.push(value);
                        }
                    }
                    1 if !model.is_empty() => {
                        let (at, value) = (rng.index(model.len()), rng.next_u64());
                        *column.get_mut(at).unwrap() = value;
                        model[at] = value;
                    }
                    2 => {
                        let (at, value) = (rng.index(model.len() + 2 * CHUNK), rng.next_u64());
                        column.set(at, value, || 5);
                        if model.len() <= at {
                            model.resize(at + 1, 5);
                        }
                        model[at] = value;
                    }
                    _ => {}
                }
            }
            assert_eq!(column.len(), model.len());
            assert!(column.iter().eq(model.iter()));
            assert_eq!(column.get(model.len()), None);
            states.push((column, model));
            if states.len() > 16 {
                states.remove(rng.index(states.len() - 1));
            }
            for (column, model) in &states {
                for _ in 0..32.min(model.len()) {
                    let at = rng.index(model.len());
                    assert_eq!(column.get(at), Some(&model[at]));
                }
            }
        }
        for (column, model) in &states {
            assert!(column.iter().eq(model.iter()));
        }
    }

    /// Each list arena against plain vectors given the same pushes (short ones, and longer ones
    /// in chunks of their own of several classes), writes and frees, across clones.
    #[test]
    fn list_arenas_answer_as_vectors_do_across_clones() {
        type Model = (Vec<(ListRef, Vec<u32>)>, usize, usize);
        let mut rng = blaze_ids::rng::SplitMix64::new(11);
        let mut states: Vec<(CowList<u32>, Model)> = vec![(CowList::default(), (Vec::new(), 0, 0))];
        for _ in 0..300 {
            let from = rng.index(states.len());
            let (mut arena, (mut lists, mut live, mut garbage)) = (states[from].0.clone(), states[from].1.clone());
            for _ in 0..rng.index(16) + 1 {
                match rng.below(6) {
                    0 | 1 => {
                        let len = match rng.below(20) {
                            0 => CHUNK + rng.index(4 * C4K),
                            1 => C4K + 1,
                            _ => rng.index(40),
                        };
                        let items: Vec<u32> = (0..len).map(|_| rng.next_u32()).collect();
                        let list = arena.push(&items);
                        assert_eq!(list.len(), len);
                        live += len;
                        lists.push((list, items));
                    }
                    2 => {
                        let (len, fill) = (rng.index(2 * CHUNK), rng.next_u32());
                        lists.push((arena.push_filled(len, fill), vec![fill; len]));
                        live += len;
                    }
                    3 | 4 if !lists.is_empty() => {
                        let pick = rng.index(lists.len());
                        let (list, items) = &mut lists[pick];
                        if !items.is_empty() {
                            let (at, value) = (rng.index(items.len()), rng.next_u32());
                            arena.get_mut(*list)[at] = value;
                            items[at] = value;
                        }
                    }
                    5 if !lists.is_empty() => {
                        let (list, items) = lists.swap_remove(rng.index(lists.len()));
                        arena.free(list);
                        (live, garbage) = (live - items.len(), garbage + items.len());
                    }
                    _ => {}
                }
            }
            for (list, items) in &lists {
                assert_eq!(arena.get(*list), &items[..]);
            }
            assert_eq!((arena.live(), arena.garbage()), (live, garbage));
            states.push((arena, (lists, live, garbage)));
            if states.len() > 16 {
                states.remove(rng.index(states.len() - 1));
            }
        }
        for (arena, (lists, ..)) in &states {
            for (list, items) in lists {
                assert_eq!(arena.get(*list), &items[..]);
            }
        }
    }

    /// Each region's bytes against a plain vector given the same writes, fills, truncations
    /// and growth, across clones; `changed_since` covers every byte that differs from the state
    /// it was cloned from.
    #[test]
    fn region_bytes_answer_as_vectors_do_across_clones() {
        let mut rng = blaze_ids::rng::SplitMix64::new(13);
        let start: Vec<u8> = (0..(3 * PAGE + PAGE / 2)).map(|_| rng.next_u32() as u8).collect();
        let mut states: Vec<(CowBytes, Vec<u8>)> = vec![(CowBytes::from_vec(start.clone()), start)];
        let mut changed = Vec::new();
        for _ in 0..200 {
            let from = rng.index(states.len());
            let (mut bytes, mut model) = (states[from].0.clone(), states[from].1.clone());
            for _ in 0..rng.index(6) + 1 {
                let at = rng.index(model.len() + PAGE);
                match rng.below(4) {
                    0 | 1 => {
                        let mut data = vec![0; rng.index(2 * PAGE)];
                        rng.fill(&mut data);
                        bytes.write(at, &data);
                        if model.len() < at + data.len() {
                            model.resize(at + data.len(), 0);
                        }
                        model[at..at + data.len()].copy_from_slice(&data);
                    }
                    2 => {
                        let (len, byte) = (rng.index(PAGE), rng.next_u32() as u8);
                        bytes.fill(at, len, byte);
                        if model.len() < at + len {
                            model.resize(at + len, 0);
                        }
                        model[at..at + len].fill(byte);
                    }
                    _ => {
                        let len = rng.index(8 * PAGE);
                        bytes.resize(len);
                        model.resize(len, 0);
                    }
                }
            }
            assert_eq!(bytes.len(), model.len());
            assert_eq!(&*bytes.contiguous(), &model[..]);
            for _ in 0..16 {
                let at = rng.index(model.len() + 1);
                let len = rng.index(model.len() - at + 1);
                assert_eq!(&*bytes.read(at, len), &model[at..at + len]);
                if let Some(it) = bytes.slice(at, len) {
                    assert_eq!(it, &model[at..at + len]);
                }
            }
            let (old, old_model) = &states[from];
            bytes.changed_since(old, &mut changed);
            for (at, byte) in model.iter().enumerate() {
                if old_model.get(at) != Some(byte) {
                    assert!(changed.iter().any(|it| it.contains(&at)), "byte {at} differs and is not in {changed:?}");
                }
            }
            assert!(changed.iter().all(|it| it.start % PAGE == 0 && it.end <= model.len()));
            states.push((bytes, model));
            if states.len() > 16 {
                states.remove(rng.index(states.len() - 1));
            }
        }
        for (bytes, model) in &states {
            assert_eq!(&*bytes.contiguous(), &model[..]);
        }
    }

    /// A history of 16 states, each a clone of the last with writes to four chunks of a column
    /// and to a list: once warm, each step's copies take slots the dropped states gave back, so
    /// the pools carve no more.
    #[test]
    fn a_history_of_writes_reuses_its_slots() {
        fn carved() -> [u32; 4] {
            [
                <Probe as Pools<[Probe; CHUNK]>>::with(|cache| cache.pool().carved()),
                <Probe as Pools<Leaf<Chunk<Probe>>>>::with(|cache| cache.pool().carved()),
                <Probe as Pools<Mid<Chunk<Probe>>>>::with(|cache| cache.pool().carved()),
                <Probe as Pools<Top<Chunk<Probe>>>>::with(|cache| cache.pool().carved()),
            ]
        }
        const CHUNKS: u64 = 20;
        let mut column: CowVec<Probe> = CowVec::default();
        for n in 0..CHUNKS * CHUNK as u64 {
            column.push(Probe(n));
        }
        let mut lists: CowList<Probe> = CowList::default();
        let list = lists.push(&[Probe(0); 3]);
        let mut history = std::collections::VecDeque::new();
        let mut step = |n: u64, column: &mut CowVec<Probe>, lists: &mut CowList<Probe>| {
            let (mut next, mut next_lists) = (column.clone(), lists.clone());
            for k in 0..4 {
                let chunk = (n * 4 + k) % CHUNKS;
                *next.get_mut((chunk * CHUNK as u64 + n % CHUNK as u64) as usize).unwrap() = Probe(n);
            }
            next_lists.get_mut(list)[(n % 3) as usize] = Probe(n);
            history.push_back((std::mem::replace(column, next), std::mem::replace(lists, next_lists)));
            if history.len() > 16 {
                history.pop_front();
            }
        };
        for n in 0..1000 {
            step(n, &mut column, &mut lists);
        }
        let warm = carved();
        for n in 1000..3000 {
            step(n, &mut column, &mut lists);
        }
        assert_eq!(carved(), warm, "a warm history carves no new slots");
    }

    /// A type that isn't `Copy`: every clone a chunk holds is dropped with the chunk's last
    /// owner, and a written clone leaves the first untouched.
    #[test]
    fn chunks_of_a_type_that_is_not_copy_drop_what_they_hold() {
        let held = Arc::new(7);
        let mut column: CowVec<Held> = CowVec::default();
        for _ in 0..CHUNK + 3 {
            column.push(Held(None));
        }
        *column.get_mut(5).unwrap() = Held(Some(Arc::clone(&held)));
        let mut lists: CowList<Held> = CowList::default();
        let list = lists.push(&[Held(Some(Arc::clone(&held))), Held(None)]);
        let (mut column2, lists2) = (column.clone(), lists.clone());
        *column2.get_mut(5).unwrap() = Held(None);
        assert_eq!(column.get(5), Some(&Held(Some(Arc::clone(&held)))));
        assert_eq!(column2.get(5), Some(&Held(None)));
        assert_eq!(lists2.get(list)[0], Held(Some(Arc::clone(&held))));
        drop((column, column2, lists, lists2));
        assert_eq!(Arc::strong_count(&held), 1, "every clone was dropped with its chunk");
    }

    #[test]
    #[should_panic(expected = "2^24")]
    fn a_list_past_the_longest_class_stops() {
        CowList::<u32>::default().push_filled(LONGEST_LIST + 1, 0);
    }

    /// The runs, an update and the changed blocks of paged bytes against a plain vector given
    /// the same writes: an update that writes what a page holds copies no page.
    #[test]
    fn runs_updates_and_changed_blocks_read_as_a_vector_would() {
        let start: Vec<u8> = (0..(4 * PAGE + 300)).map(|it| (it * 7) as u8).collect();
        let old = CowBytes::from_vec(start.clone());
        let mut new = old.clone();
        new.update(10, &start[10..PAGE + 20]);
        let mut changed = Vec::new();
        new.changed_since(&old, &mut changed);
        assert!(changed.is_empty(), "an update of the bytes there copies no page");
        new.update(2 * PAGE + 5, &[0xab; 3]);
        new.write(4 * PAGE + 200, &[1; 200]);
        new.update(5 * PAGE, &[0; 64]);
        let mut reference = start.clone();
        reference[2 * PAGE + 5..2 * PAGE + 8].fill(0xab);
        reference.resize(5 * PAGE + 64, 0);
        reference[4 * PAGE + 200..4 * PAGE + 400].fill(1);
        assert_eq!(&*new.contiguous(), &reference[..]);
        let mut rebuilt = vec![0u8; new.len()];
        let mut runs = Vec::new();
        new.runs(|at, bytes| {
            rebuilt[at..at + bytes.len()].copy_from_slice(bytes);
            runs.push((at, bytes.len()));
        });
        assert_eq!(rebuilt, reference);
        assert_eq!(runs, [(0, 2 * PAGE), (2 * PAGE, PAGE), (3 * PAGE, PAGE), (4 * PAGE, PAGE)]);
        let mut blocks = Vec::new();
        new.changed_blocks(&old, 4096, &mut changed, |at, len| blocks.push((at, len)));
        let past_old: Vec<(usize, usize)> = (4 * PAGE..5 * PAGE).step_by(4096).map(|it| (it, 4096)).collect();
        assert_eq!(blocks[0], (2 * PAGE, 4096), "the one block of page 2 written");
        assert_eq!(blocks[1..5], past_old[..], "page 4 reaches past the old bytes' end");
        assert_eq!(blocks[5..], [(5 * PAGE, 64)]);
        assert_ne!(new, old);
        assert_eq!(new, CowBytes::from_vec(reference));
        assert_eq!(old, CowBytes::from_vec(start));
    }
}
