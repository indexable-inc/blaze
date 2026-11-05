# 09 — Concurrency: the shared `BBTreeMap`, closures instead of handles

Status: built (2026-10-15, 2026-10-16, 2026-10-17): `bbtree::BBTreeMap`,
the map threads share, with `read`, `update`, `get_copied`, `get_cloned`,
`contains_key`, `insert`, `update_or_insert`, `get_or_insert`, `remove`,
`remove_entry`, `remove_if`, `scan`, `scan_rev`, `range_cloned`,
`is_empty`; batched `read_many`, `get_many_copied`, `get_many_cloned`,
`insert_many` and `update_or_insert_many` (2026-10-19); and,
for its owner, `get_mut`, `iter_mut`, `into_iter`, `clear`, `extend` and
`collect`: the unsync tree, shared through optimistic lock coupling, its
tree operations the unsync tree's own. No `entry` yet.
`unsync::BBTreeMap` stays single-threaded (D4). Until 2026-10-17 the shared
map was `BCTreeMap` and the unsync one `bbtree::BBTreeMap`.

## Scope

A concurrent B-tree differs enough in design and cost that the std
replacement, `unsync::BBTreeMap`, stays single-threaded, and the map
threads share is a type of its own, `BBTreeMap` (`bbtree/src/map/`), the crate's
headline (D4). The two share the node code and the tree's operations.

The shared map is about single-entry concurrency: latches that keep the
tree consistent while threads read and write it, one entry per operation.
No transactions, no atomicity across entries.

## Requirements

1. Users never take a latch by hand.
2. Users never hold an epoch, guard or handle. The API stays close to the
   unsync map's.
3. A single thread never deadlocks with itself. Two threads may deadlock
   only where the semantics ask for it.

## API: closures, not handles

An operation that exposes a key or value runs the caller's closure while
the entry's leaf is latched, unlatches, and returns what the closure
returned; or hands out a copy. No reference, guard or handle outlives the
call, so no latch does either. The names follow std's where they mean the
same: `get_*` returns a value, `read*` runs a closure on the entry in
place, as scc's maps do.

```rust
impl<K: Ord, V> BBTreeMap<K, V> {
    /// Runs `f` on the entry while its leaf is latched shared.
    fn read<Q, R>(&self, key: &Q, f: impl FnOnce(&K, &V) -> R) -> Option<R>;
    /// A copy of the value, read without a latch where the leaf settles the
    /// key (Lookups, below).
    fn get_copied<Q>(&self, key: &Q) -> Option<V> where V: Copy;
    fn get_cloned<Q>(&self, key: &Q) -> Option<V> where V: Clone {
        self.read(key, |_, v| v.clone())
    }
    /// Runs `f` on the entry while its leaf is latched exclusive.
    fn update<Q, R>(&self, key: &Q, f: impl FnOnce(&K, &mut V) -> R) -> Option<R>;
    /// Runs `f` on the entry and `value`, which it takes, while the leaf is
    /// latched exclusive; or inserts `key` and `value` if there is none
    /// (`None`). `insert` is the `f` that replaces the value.
    fn update_or_insert<R>(&self, key: K, value: V,
                           f: impl FnOnce(&K, &mut V, V) -> R) -> Option<R>;
    /// Runs `f` on the entry, inserting `key` and `value` first if there
    /// is none, while the leaf is latched exclusive.
    fn get_or_insert<R>(&self, key: K, value: V, f: impl FnOnce(&K, &V) -> R) -> R;
    /// Removes the entry if `condition` holds of it, latched exclusive.
    fn remove_if<Q>(&self, key: &Q, condition: impl FnMut(&K, &V) -> bool)
        -> Option<(K, V)>;
    /// Visits the range one leaf at a time until `f` breaks: in key order,
    /// and in reverse (`scan_rev`).
    fn scan<Q, R>(&self, range: impl RangeBounds<Q>,
                  f: impl FnMut(&K, &V) -> ControlFlow<R>) -> Option<R>;
    /// Clones of the range's entries, from both ends, holding nothing
    /// between calls.
    fn range_cloned<Q, R>(&self, range: R) -> RangeCloned<'_, K, V, ..>
    where K: Clone, V: Clone;

    /// Batches: each key in a lane, the lookups' and inserts' waits for
    /// memory overlapped (Batches, below).
    fn read_many<Q, R, const N: usize>(&self, keys: [&Q; N],
                                       f: impl FnMut(&K, &V) -> R) -> [Option<R>; N];
    fn get_many_copied<Q, const N: usize>(&self, keys: [&Q; N]) -> [Option<V>; N]
    where V: Copy;
    fn get_many_cloned<Q, const N: usize>(&self, keys: [&Q; N]) -> [Option<V>; N]
    where V: Clone;
    fn insert_many<const N: usize>(&self, entries: [(K, V); N]) -> [Option<V>; N];
    fn update_or_insert_many<R, const N: usize>(&self, entries: [(K, V); N],
                                                f: impl FnMut(&K, &mut V, V) -> R)
        -> [Option<R>; N];

    /// The owner's, through `&mut` or by value: no latch (Owners, below).
    fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>;
    fn iter_mut(&mut self) -> IterMut<'_, K, V, ..>;
    fn clear(&mut self);
}
// and IntoIterator by value and for `&mut`, Extend, FromIterator, From<[(K, V); N]>
```

The types keep references from escaping: `impl FnOnce(&K, &V) -> R` is
`for<'a> FnOnce(&'a K, &'a V) -> R`, and `R` is fixed outside `'a`, so
`map.read(&k, |_, v| v)` doesn't compile. `std::thread::scope` and
`LocalKey::with` rely on the same rule. scc's `HashMap` (`read_sync`,
`update_sync`, `iter_sync`) and DashMap (`view`, `alter`) offer the same
shape.

One entry at a time: an entry can't be held across calls, there is no
`get_mut` handle through `&self`, and an operation on two entries isn't
atomic. `entry`,
std's API within one exclusive latch, is not built: `update_or_insert`
(change the entry, or insert), `get_or_insert` (insert unless present,
then read) and `remove_if` (remove only the entry the caller saw) cover
what the integrations surveyed need of it.

`update_or_insert` is the insert's own policy (`InsertPolicy`,
`tree/latched/insert.rs`) as a closure; `insert` is the closure that
replaces the value, and the batch runs the same one per entry. Without
it, a writer of plain values could not tell an entry it inserted from one
it found: Lance's adapter (bbtree-eval) marked a fresh `AtomicU64` from
`get_or_insert`'s closure, then looked each key up before inserting it,
188 ns a put at 10M against 158 for one descent; and a lookup then an
insert loses a count when two threads add to one key. Its shape:

- **One closure, for a held entry.** `None` says the entry was inserted,
  as `update`'s `None` says there was none. A second closure for a new
  entry could not share a `&mut` capture with the first.
- **The new value goes to the closure,** which merges it (a sum, an
  extended list), so making it before the call wastes nothing. A value
  made only for a new entry would not fit `InsertError`, which hands the
  value back.
- **The name is the map's own:** `update` and `or_insert`. scc's `upsert`
  is a replacing insert, this map's `insert`.

There is no `len`: a shared counter would cost every insert an atomic on
one line all threads write. The systems integrated (14-blogpost.md) count
entries themselves, from what `insert` returns.

Rejected:

- **A handle that holds the leaf latch**, like DashMap's `Ref`. Its docs
  warn on `insert`, `remove`, `get_mut` and others: "May deadlock if called
  when holding any sort of reference into the map". That breaks
  requirement 3, and a held latch also blocks the leaf's splits.
- **A per-entry lock under an intention lock on the leaf.** A split needs
  the leaf exclusive, which conflicts with every intention lock, so a
  thread that holds an entry and then inserts into the same leaf can wait
  on itself; and a split moves entries to another leaf, so their locks
  can't live in it.

## One tree, shared

The shared map's tree is the unsync map's: the same nodes, `Leaf`, `Internal` and `RootPage`,
the same shared prefix (D50) and prefixes fit to their fences, the same
4-byte windows of heap strings in internal nodes (D43), the same root that
grows and shrinks in its page, and an append to the last leaf (D45). Its
nodes have the strategy `Latched<S>` (`tree/node/latch.rs`), a
crate-private wrapper of the map's: the same inline keys, plus a latch word
in every node's header. The layouts put the inline keys 16 bytes in
instead of 8 (`LatchedNodeHeader`); u64→u64 nodes keep 55 entries per leaf
and go from 49 to 48 children per internal node. The keys need inline keys:
an internal node read optimistically must keep no keys (D49).

Its tree operations are the unsync tree's own too. A split, a removal that
rebalances, and a shrinking of the shared prefix first take every node
the unsync tree's operation will change or read a separator of, without waiting,
then run it along a node stack from the topmost node they hold
(`NodeStackMut::along`, "Changes to more than a leaf" below). The shared map's own
code is what sharing needs and nothing else (`tree/latched/`):

- the optimistic descent (`descent.rs`);
- which nodes each change takes (`insert.rs`, `remove.rs`,
  `shared_prefix.rs`), and taking them (`held.rs`);
- type-stable pages (`pages.rs`);
- the pointer to the last leaf that appends go to (`append.rs`).

A first version rewrote the tree layer, and left out the shared prefix,
fitting prefixes after removals, lowering the tree to a leaf, 4-byte
windows and appends. Each cut put a special case into shared code, a flag
in the node plan and in the invariant check, a root shape the unsync tree never
has, or made another path hot: with an empty shared prefix, dense integers
tied at the root and along the borders, and once removal latched ties,
`u64-seq` hits took 205 ns.

The unsync tree changed in three places for it, none on its lookup paths: the
inline-key searches read their slots through a raw `Slots` view, so that a
racing reader forms no reference into a node a writer is changing; a node
stack can start along a path a descent kept; and a removal's fixing decides
whether to go up from the node it is at, reading no node above the last it
changes, and lowers the root only if it emptied it.

## Latching

One word per node, our own (`NodeLatch`): from the low bit, an exclusive
bit, a count of shared holders (20 bits), and a version (43 bits). One word
serves both modes, so the root page keeps it when it turns from a leaf into
an internal node and back.

- **Inner nodes: optimistic.** A reader reads the version (waiting while
  the exclusive bit is set), reads the node, and checks the version after
  an acquire fence, ignoring the shared count. A writer takes the node by
  compare-and-swap from the version its descent read, never waiting:
  failure restarts the operation. Letting go bumps the version. bf-tree
  does the same (`src/utils/inner_lock.rs`: `try_read`, `check_version`,
  `upgrade`).
- **Leaves: latched to change them, and to run a closure.** Shared for
  `read`'s closure and scans, exclusive for `update` and `insert`,
  spinning with exponential backoff. No scheduler hint: the crate is
  `no_std` (D3). Releasing exclusively bumps the version too. Writers can
  starve under a steady stream of readers. A lookup reads its leaf
  optimistically first, as it reads internal nodes, and latches it only
  where that doesn't settle it (Lookups, below).
- **Descent** (`descent.rs`): a key that lacks what every key shares is in
  no node, which the descent says once the root's version vouches for the
  shared prefix it read, or with the root leaf latched. Optimistic through
  the inner nodes: at each, route the key, load the child pointer, read the
  child's version, then check the node's version, so that the child was
  the node's child when its version was read (the root's version is also
  checked before the edge is followed: Edges, below); at the leaf, latch
  it, or for a lookup read its version (`Optimistic`), then check its
  parent's version, so the leaf still covers the key. A root leaf checks the tree's root pointer
  instead, once latched, and an internal root checks it once its version
  is read: a lowering to a leaf that finished between the pointer's read
  and the version's leaves a version that checks out on a leaf, but
  retagged the pointer first. Any failed check restarts from the root.

Deadlock: an operation holds at most one leaf latch while it waits, and
takes everything else without waiting. Pages it allocates it latches
waiting, for a thread that reached a free page through a stale pointer may
hold it a moment, waiting on nothing.

## Optimistic lock coupling

Readers read internal nodes in place, through raw pointers, while writers
holding a node change it in place with the node code as it is; the version
check throws away whatever a reader read meanwhile (`node/optimistic.rs`),
as in C++ OLC B-trees (Leis et al., "The ART of Practical Synchronization").
No copies on either side. A racing read forms no reference into the node,
so the aliasing model is not involved; what it reads before its check must
not crash it or run user code on garbage:

- **Routing** reads `len` and `prefix_len` once each (relaxed atomic loads,
  so the compiler loads them once), clamps the length to the capacity, and
  runs the shared inline-key search on the page's slots: any inline keys
  give an answer in bounds (D14). The shared prefix is copied out of the
  root page, as raw bytes.
- **Edges** of the root page are followed only once the root's version
  vouches for them: it is the one page that changes kind, so an edge read
  from it may be a leaf's bytes. An edge read from any other node, even a
  stale one or one past the length, is null or a pointer to a node page of
  the kind its tag says, live or free: internal pages come zeroed and keep
  their kind while threads share the tree (type-stable nodes, below). So
  the descent reads the child's version before checking the parent's,
  once, which then vouches for both.
- **Ties** compare the separator's key, the head of the leftmost leaf right
  of it (D49), with that leaf latched shared: the walk reads the node's
  edge, checks the node's version, walks down first edges, which only a
  thread holding the node changes, latches the leaf, and checks the node's
  version again before comparing. The latch keeps the head in place: a
  removal takes out a head with its leaf latched exclusively, and its key's
  memory may be freed at once. Keys ordered by their own bytes alone, the
  std integers and byte arrays `Auto` recognizes, skip the latch: the walk
  copies the head's key, compares the copy, and checks the node's version
  after, as the head changes only while the node is held. `u64` keeps
  2-byte windows in internal nodes, so lookups tie: 0.16 walks a lookup at
  10M random keys; with the latch, each tie wrote two atomics to a leaf
  every other thread's ties write too. A walk that meets a null edge, a leaf
  without a head, or a changed version restarts.
- **A separator match** is checked at the leaf: the descent checks only
  the leaf's parent, and a separator matched higher up may have been
  removed since, its successor the leaf's head now. The latched leaf's head
  must be the key, or the descent starts over.

## Lookups

A lookup reads its leaf as it reads internal nodes, without the latch
(`tree/latched/lookup.rs`, D57): the descent ends at the leaf with its
version read, the parent checked after. Then, in place:

- **The inline keys rule the key out:** absent, once the leaf's version,
  unchanged, confirms the read. A miss writes nothing.
- **A copy** (`get_copied`, `get_many_copied`, `contains_key`): where the
  leaf settles its one candidate without following a pointer, the lookup
  copies the candidate's value out and checks the version after, as a
  sequence lock reads (`RacingRead`). A key ordered by its own bytes, a
  std integer or byte array, is copied with the value and compared once
  the version holds; a byte string that ends within the leaf's 8-byte
  inline key is the candidate if their lengths are equal (D36), as the
  leaf's hash tags tell where they are exact, below 255, so no heap
  byte is read. Every slot read is a
  `MaybeUninit` until the version holds. A head the descent matched is
  copied and compared the same way.
- **Otherwise** the lookup latches the leaf shared, checks that it is still
  at the version read, so that it still covers the key and its candidate
  is still where it was, and searches it, or compares the candidate, and
  runs the closure.

A leaf that changed meanwhile sends the lookup the latched way
(`read_latched`), which waits for a writer rather than race it again, so
readers don't starve behind a hot leaf.

Only a `Copy` value is read this way: a closure, or a clone of a value that
owns memory, can't run on what an unlatched read saw, since a writer may
free that memory before the check fails. A `Copy` value has no `Drop` and
no interior mutability, so a torn copy is thrown away unused. Racing plain
reads are formally a data race, as the internal nodes' are, and crossbeam's
`AtomicCell` sequence lock's; the threads tests stay out of Miri for that
(`.github/miri-nextest.toml`).

Measured (2026-10-17, 9900X, 10M `u64`, bbtree-eval's `bbtree-scaling`,
against the latched lookups of 2026-10-16, millions a second): lookups half of which
miss, 9.16 → 8.84 on one thread, 83.4 → 86.9 at 12, 98.0 → 113.6 at 24;
batched, 13.5 → 13.8, 115.5 → 130.6, 121.5 → 145.9. A hit no longer
writes its leaf's latch word, a line each reader of the leaf took in turn.

## Changes to more than a leaf## Changes to more than a leaf

A change that a leaf can't take alone descends again, keeping each internal
node it passes with the version it read, the edge it took and the length
(D37's second search, `TreePath`). Holding its leaf exclusively, it takes,
without waiting (`held.rs`), internal nodes on the path by compare-and-swap
at the versions it read, so that each is still the parent of the next, and
nodes beside the path at the versions they are at, reached through nodes
it holds. Any node held by another thread, or changed, lets go of all and
starts over. Then it runs the unsync tree's operation along a node stack from the
topmost node it holds, as if that were the root, on the tree as it is
(`LatchedTree::change_held`), and lets go once all is done: a reader that
read a node meanwhile fails its check. Pages it allocates it takes before
it changes anything, and they come latched and stay so until then
(`pages.rs`); a change of the root's kind retags the tree's root pointer
before the root is let go.

What each takes, so that the unsync tree's operation changes nothing and reads no
separator it doesn't hold:

- **A split**: the nodes from the leaf's parent up to the lowest with room,
  which takes the separator, or the root if none has room.
- **A removal that rebalances**: for each level it leaves underfull, from
  the leaf up, the parent and the sibling the child merges with or takes an
  entry from, as `Internal::fix_underfull_child` decides. For a separator,
  also the node holding it, the nodes on the path from there down, whose
  lower fence its successor becomes, and the nodes along the right border
  of the subtree left of it, whose upper fence it becomes. A stress run
  found that the path below the separator is needed: a descent that went
  right of the separator for a key below its successor checks only the
  leaf's parent.
- **Fences** of the topmost node either changes, which the changed nodes'
  prefixes are fit to (`tree/prefix.rs`): the nodes up to the nearest
  above it with a separator on each side of the path
  (`TreePath::fences_top`). A side without one is the tree's border, which
  a node stays on whatever others change, and whose fence is what every
  key shares. Fence values move but never change under a split, merge or
  move between siblings elsewhere: separators go up and come down whole.
  So prefixes stay exact, as in the unsync map.
- **Shrinking the shared prefix**, for a key that lacks it: the root and
  every node along both borders (`shared_prefix.rs`). An operation holding
  a border node holds the shared prefix still.

Holding a node holds its separators' keys, though they are heads of leaves
below (D49): a head changes only when its separator is removed, replaced
by its successor, or rotated through the node by a merge or a move between
siblings, each holding the node. So the unsync tree's operations read every key
they need where it is: a separator's, a fence's, or a window's other bytes
when a node hands out a 4-byte window of a heap string (D43).

## Appends

The tree points at its last leaf, which only a thread holding that leaf
exclusively changes (`append.rs`): a split of it points it at the leaf last
after the split, found down the last edges of the node that took the
separator; a merge that frees it, or a lowering that moves it into the
root page, at the leaf that took its entries. An insert latches the leaf
the pointer names, and if the pointer still names it, the leaf is last and
stays so while held: a key greater than its last key, that shares what
every key shares, goes in if the leaf has room, as the unsync map's append (D45).
A new tree's root leaf is latched until the pointer names it.

## Performance

Ryzen 9 9900X, mimalloc, 10M entries, one run each (2026-10-17).

One thread, ns per operation, against the unsync map, from this round's
probe (one `#[inline(never)]` cell per map, key and operation):

| cell | unsync | shared |
|---|--:|--:|
| `u64` get hit / miss / half | 135 / 93.2 / 122 | 138 / 96.3 / 126 |
| `u64` random / sequential insert | 137–146 / 5.0 | 150 / 11.1 |
| `u64` random remove | 306 | 199 |
| UUID get hit / miss / half | 137 / 96.2 / 127 | 144 / 99.2 / 132 |
| UUID random insert | 261 | 167 |
| JSON get hit / miss / half | 172 / 108 / 152 | 189 / 110 / 161 |
| JSON random insert | 317 | 318 |

A lookup pays for the versions read and checked on the way down, and
where its leaf can't settle a hit unlatched (JSON keys past the inline
key), the leaf's latch. A random insert pays the leaf's compare-and-swap;
an append, that on every key, so sequential inserts take twice as long.
Removal runs the unsync tree's own code, yet is faster than the unsync
map's: most likely because the unsync map keeps a node stack for every
removal, where the shared one removes an entry whose leaf keeps its
minimum with the leaf alone. Not measured yet; if so, the unsync map could
do the same. UUID inserts of the unsync map are slower than the shared
one's on the same tree code: open (14-blogpost.md).

Threads (`bbtree-scaling`, the blog's runs, `u64 → u64`), on the whole
machine: workers pinned one per core, 1–6 on the first die and 7–12 on the
second, then the SMT siblings; millions of operations a second at 1 / 12 /
24 threads, each map as its crate ships it: scc's `TreeIndex` and
crossbeam's `SkipMap` hand out values shared only, so theirs are atomics an
update adds to; concurrent-map gives each worker a clone and returns values
by copy; std's `BTreeMap` sits behind a `RwLock`. A scan visits 100 entries
from a key the map holds.

| map | lookups, half missing | batched | update | 90/10 get/update | 90/10 scan/update | insert |
|---|--:|--:|--:|--:|--:|--:|
| bbtree | 9.5 / 94.3 / 111.7 | 13.6 / 129.5 / 146.5 | 8.6 / 71.2 / 80.9 | 8.4 / 79.4 / 92.5 | 1.8 / 12.2 / 13.6 | 6.8 / 46.2 / 48.4 |
| scc `TreeIndex` | 2.4 / 27.3 / 47.3 | | 2.3 / 26.1 / 43.3 | 2.4 / 26.8 / 46.5 | 0.6 / 6.3 / 9.2 | 2.5 / 25.7 / 35.1 |
| concurrent-map | 2.4 / 25.7 / 46.2 | | 1.5 / 10.7 / 13.5 | 2.1 / 22.2 / 37.4 | 1.3 / 10.9 / 12.3 | 2.4 / 11.9 / 12.5 |
| crossbeam `SkipMap` | 1.0 / 12.4 / 21.8 | | 1.1 / 12.2 / 20.7 | 1.0 / 12.0 / 21.7 | 0.1 / 1.1 / 1.9 | 1.3 / 10.2 / 14.6 |
| `RwLock<BTreeMap>` | 4.0 / 22.8 / 33.3 | | 3.6 / 2.1 / 2.1 | 3.8 / 4.8 / 5.8 | 0.7 / 2.1 / 2.3 | 4.3 / 2.1 / 1.9 |

Lookups scale near linearly across both dies' cores; the SMT siblings add
about a fifth more. Inserts scale less: each dirties several leaf lines.
On string keys the same holds but for one cell: the 90/10 scan/update mix
on Wikipedia titles at 24 threads, where scc passes bbtree, 6.5 against
6.0 M/s, both near memory's bandwidth, while each of bbtree's scans
writes two atomics to every leaf it visits (Scans, below). In the
2026-10-16 round, against the latched lookups of then, Congee (an ART,
8-byte keys) read 87.9 M/s at 24 threads (85.5 then, 111.7 now) and
inserted 55.3 (49.2 then, 48.4 now), and DashMap, a hash map, read 209;
Congee copies a range out instead of scanning in place, DashMap keeps no
order, and both are out of the blog's comparison.

Memory, 10M entries, each map built in a process of its own
(`bbtree-scaling-memory`): heap bytes per entry, keys' own allocations
included:

| map | `u64` | UUID | JSON | Wikipedia titles |
|---|--:|--:|--:|--:|
| bbtree | 26.9 | 39.7 | 68.5 | 80.9 |
| std `BTreeMap` | 27.1 | 38.9 | 59.1 | 71.6 |
| concurrent-map | 25.9 | 38.0 | 59.4 | 72.8 |
| crossbeam `SkipMap` | 40.0 | 48.0 | 64.5 | 77.0 |
| scc `TreeIndex` | 39.5 | 55.0 | 80.1 | 94.1 |

## Type-stable nodes

An optimistic reader follows pointers it read without a latch and checks
them only after, so a node a merge frees may still be read. Its page can't
go back to the allocator, which could hand it to anything. Instead
(`tree/latched/pages.rs`):

- **Free lists per kind.** A freed leaf or internal page goes on the
  tree's free list for its kind, linked through its first inline-key slots,
  and only a new node of that kind takes it. A reader that reaches it reads
  a node of the kind its pointer says, with edges that are old pointers to
  node pages of their kind, or null.
- **One latch word for life.** The page keeps its latch word, whose version
  only grows: freeing bumps it, and a node moved into a page (a raise, a
  lowered root) keeps the destination's word, so a reader's check can't
  find an old version again. Pages are put on a list only after the
  operation that freed them lets go of its nodes.
- **Back to the allocator** when the map is dropped, with the tree: a map
  keeps the pages of its largest size. No epochs, no guards, nothing per
  operation.

The root page alone changes kind, in place, when the tree grows from a
leaf or lowers to one; readers check a node's version before following an
edge they read from it.

Fresh pages come zeroed from the map's allocator (D8): a latch word
unheld at the first version, and every edge a reader may read before its
check initialized.

A split takes every page it allocates before it changes anything, once it
holds the nodes it changes, which tell how far it goes: free pages of
their kind first, then fresh ones. An allocator with no page to give fails
the insert whole, and the entry goes back to the caller; the pages taken
by then go on the free lists.

## Owners

A thread that holds the map by `&mut` or by value has it alone, so the
unsync tree's own lookups and walks run on it as on an unsync map's tree,
without a latch, and hand out references for as long as the owner lends
the map (`tree/latched/owner.rs`, `map/owner.rs`): `get_mut`, `iter_mut`
(a range walk over the whole tree, from both ends), `into_iter` (the walk
that takes the tree apart, front only: the tree keeps no count of its
entries, which a double-ended walk needs), `clear`. `Extend`,
`FromIterator` and `From<[(K, V); N]>` insert one entry at a time.

## What is left out

- **No `entry`.**
- **No `len`** (above).
- **No iteration by reference through `&self`:** a reference would
  outlive the latch. `scan` runs a closure; `range_cloned` clones; owners
  iterate with `iter_mut` and `into_iter`.

## Scans

`scan` and `scan_rev` (`tree/latched/scan.rs`) run the caller's closure on
each entry of a range, in key order or in reverse, with its leaf latched
shared, until the closure breaks. One code serves both orders, a type
parameter deciding each mirrored step. A scan starts at the leaf where the
bound it starts from belongs, and moves on to the leaf beside it, right in
key order, left in reverse, through the internal nodes its descent kept:
the nearest with an edge on that side of the path, then the edges nearest
the held leaf down, each read optimistically. It latches the next leaf
before it lets go of the one it has, which can't split meanwhile, and the
next leaf is that one's neighbor if every node kept from there down is at
the version read. Otherwise it searches again for the last key it visited,
still holding that key's leaf, and goes on past it: no key is cloned.

A bound that lacks what every key shares (D50), before every key in the
scan's order, starts the scan at the first leaf, or in reverse the last.
The descent read the shared prefix at one time and the scan latches that
leaf at another, and an insert between them can shrink the prefix and put
a key before the bound. So the scan reads the prefix again once it holds
the leaf, which, on the tree's border, holds it still, and descends again
unless the bound still lacks it on the same side. A lookup needs no such
check: its answer, absent, was true when the descent read the prefix.

Scans in both directions latch a leaf while holding another, left to right
and right to left, and can't deadlock: a thread waits for a leaf while it
holds one only in a scan, for a shared latch, which waits only while a
thread holds the leaf exclusively (the latch has no writer preference); and
a thread holding leaves exclusively takes every other node without waiting,
but for a page off a free list, which a thread that reached it through a
stale pointer holds a moment, waiting for nothing.

A scan is not a snapshot: an entry inserted or removed while it runs may or
may not be visited. An entry present for the whole scan is visited exactly
once.

A scan's fixed cost is its descent: the path it keeps (`TreePath`) holds
its levels uninitialized past its depth, so making one writes nothing, and
a hop to the next leaf copies only the levels it walked down, in place.
Each used to write and copy the whole 23-level array, 920 bytes, two
copies a hop; a path returned by value still cost a whole copy where the
compiler kept the move (SlateDB's key type, 2026-10-17).

Every leaf after the first is visited whole (`ScanOrder::whole_entries`):
its keys start at index 0, so no entry's address waits on the header's
head flag. A first index computed from that flag, branch-free, made every
entry load of the leaf wait for the header's line from memory, and the
hardware prefetcher with it: Tonbo's memtable walk, 17 entries a leaf,
took 59 cycles an entry against 47 (its integration's diagnosis,
2026-10-17). A probe of that shape, 1M `u64` keys with 64-byte values,
now walks at 8.8 ns an entry, against 10.0–10.6 before the lookups'
round.
Measured (2026-10-17, 9900X, one thread): a scan of 2 entries from a key,
at 100k `u64` entries, in cache, 43 ns against 31 for a lookup; 100-entry
scans at 10M, 625 ns `u64` and 972 JSON, against 662 and 1,146 for the
unsync map's `range`. The 90/10 scan/update mix at 24 threads, millions a
second: `u64` 13.8 (scc's `TreeIndex` 9.7), JSON 7.8 (6.7), Wikipedia titles
6.3 (6.5); the scans read near memory's bandwidth there.

`range_cloned` (`map/range.rs`) is the iterator over a range that
the integrations surveyed need: a cursor held across calls, by a query
plan or an `async` task. It holds no latch between calls. Each end copies a
few entries during a scan, each with its leaf's stamp: the leaf's latch
word version while the scan held it. It hands a copy out while the stamps
of its leaf and of the previous entry's leaf are current, so that nothing
was inserted or removed between the two, and otherwise scans again past the
last key it handed out. So it hands out entries other threads insert ahead
of it, as a skiplist cursor does. Pages stay the map's until it drops
(type-stable nodes, above), so a kept page is always there to latch or
read. An end copies 1 entry, then 4, then 16 at a time, so that a caller
taking a few entries clones a few.

A copy goes on where the last one's scan stopped (`ScanResume`): it latches
that leaf, checks the stamp, and goes on from the place kept, without a
descent. Past that leaf it descends once, from the leaf's last key, unless
the resume keeps the path down to it too: one box, which the cursor asks
for at its third copy, a caller that scans on, and then the copies cross
leaves along it. A cursor that only seeks stays 264 bytes, with no
allocation; keeping a path inline made it 2,088.

Measured (2026-10-16, 100-entry scans, 10M keys, one thread): copies that
each descended from the root, about six a scan, ran 0.66 million scans a
second for `u64` keys; with the resume, 0.88, as many as the OLC B+ tree of
Turso's PR #9267, whose cursor keeps its leaf, at 1, 12 and 24 threads. In
Turso's own map benchmark, 100-entry scans of table rows take 3.0 µs, of
index records 7.2 µs, from 3.6 and 8.1. The closure scan stays the fastest
way through a range: 1.5 million a second.

The visit counts the entries it visits with a plain counter, for the place
a stopped scan keeps: `enumerate` over the chained head-and-keys iterator
cost about 1,450 instructions more a 100-entry scan, 12% of the closure
scan's throughput.

## Batches

`read_many`, `get_many_copied` and `get_many_cloned` look up a fixed array
of keys at once, and `insert_many` and `update_or_insert_many` insert one,
as the unsync map's `get_many` and `insert_many` do; the inserts run their
closure, `insert_many`'s the one that replaces, per entry in input order
(doc 12; `tree/latched/batch.rs`,
`map/batch.rs`). Each key descends in its own lane, an async block that
prefetches the line it reads next and yields, on the unsync tree's executor
and its `Prefetcher`, the one place that prefetches (D46). Results come in
input order; a closure runs once per entry found, in the order the lanes
reach them.

A suspended lane holds no latch. So the lanes of a batch never wait for
one another, and no yield falls between latching and letting go: a lane
latches its leaf, searches it, runs the closure and lets go within one
poll. On the way down it reads the internal nodes as the descent does,
optimistically, each stage after fetching the line it needs: the chosen
edge, then the child's header, whose version it reads before checking the
parent's. At the leaf it reads the inline keys, unlatched, at the version
it read:

- **No inline key equals the key's:** absent, if the leaf's version is
  still the same. A miss writes nothing.
- **One candidate:** the lane fetches its entry. A lane that copies
  (`get_many_copied`) then settles it as a single lookup does (Lookups,
  above), unlatched. Otherwise, for a heap key, it latches the leaf a
  moment to read where the key's bytes are, lets go, and fetches them: a
  pointer into a key is read only under the latch, and a prefetch of a
  stale one is harmless. Then, latched shared, the leaf still at the
  version read, it compares the candidate and runs the closure.
- **Ties**, and keys equal to a separator, go to the leaf's own search, or
  to its head, latched, as `read`, or copied where the key orders by its
  own bytes. A tie inside an internal node walks to its head
  synchronously: a lane that prefetched and yielded along that walk ran
  `u64` batches slower, 75–80 ns a lookup against 71 (D55).

A failed check starts the lane over from the root, alone.

**Inserts.** An insert's lane descends as a lookup's, and stops at its
leaf with the version it read, and whether its key matched a separator;
it also fetches the entry where the insertion will shift entries, as the
inline keys place it. Then the entries go in, in input order, each into
its lane's leaf, taken by a compare-and-swap at the version the lane read:
unchanged since, the leaf still covers the key, and its search under the
latch finds the key or its place. An insert into a leaf moves the version
the later keys of the batch expect of it on, but only for lanes that read
it at the version the insert took it at: a lane that read it before
another thread changed it doesn't trust it. A leaf another thread holds or
changed, or one that lost the head a key matched, sends that key through a
single insert; a full one, through the insert that splits; either way the
later keys' lanes that named that leaf are forgotten.

Below about an L2 of leaf pages, the lanes find their lines in cache and
cost more than they save: at 10k `u64` keys, 28.3 ns a batched hit against
17.7 alone. There batches go one key at a time, as the unsync map's do. The
shared map has no length to estimate that from, so its height tells: three
internal levels or more, which a tree grown by splits reaches only past
2·m² leaves, m half an internal node's capacity, about 1,150 pages for
`u64` keys.

Measured (2026-10-17, 9900X, 10M entries, `u64` values, 64 keys a lookup
batch, 16 an insert batch, one run each), one thread pinned to a core, ns
a key (the probe of this round, as `bbtree-bench`'s cells), against the
unsync map's batches:

| keys | lookups half missing, alone | batched | unsync batched | inserts, alone | batched | unsync batched |
|---|--:|--:|--:|--:|--:|--:|
| `u64` | 128 | 72 | 61 | 150 | 111 | 96 |
| UUID | 134 | 82 | 68 | 167 | 131 | 110 |
| JSON | 161 | 91 | 77 | 318 | 178 | 168 |

The lookup lanes run about 155 instructions a key more than the unsync
map's (560 against 403 a `u64` miss): the versions read and checked at
each level, the lengths read racily and clamped, and the state kept across
yields. At 24 threads the batches pay off more: `u64` lookups half
missing, 113.6 M/s one key a call, 145.9 batched.

Batches don't fetch fewer lines (3.8 a 10M `u64` hit on one thread,
batched or not): they keep more in flight, until the reads near what
memory delivers.

## Panics

Whatever is simplest. The latch guard unlatches on unwind; there is no
poisoning; a value that a panicking `update` closure left half-changed
stays as it is.

## Open

- **Re-entry:** a closure that calls into the same map. Deferred. Known
  deadlocks: a shared latch taken again on the same leaf while a writer
  waits between the two; any call on the leaf from inside `update`; two
  threads in closures on leaves A and B that each call into the other's
  leaf.
- **Returning free pages** before the map drops, under `&mut self`.
- **Failed takes bump versions.** A shrink of the shared prefix that finds
  a node held lets go of the root and the border nodes it took, each with
  its version bumped, though it changed nothing; so optimistic readers of
  the root start over. Two shrinks racing while a scan holds a border leaf
  starve the scan's descent under Shuttle, and the scan keeps both shrinks
  from finishing (2026-10-19). Letting go of an unchanged node at its old
  version would end it.

## Testing

Differential tests against std one thread at a time, inserts and
removals, copies (`get_copied`, `get_many_copied`, `contains_key`),
batched inserts (repeated keys, splits, ascending runs, keys that shrink
the shared prefix), and the owner's API (`get_mut`, `iter_mut` from both
ends, `into_iter`, `clear`, `extend`, `collect`, `Debug`), with the unsync tree's invariant check, exact prefixes included, and a
check that appends go to the last leaf; a test of the shared prefix
growing, shrinking for keys that lack it, and starting anew in an emptied
tree; a test that removed nodes' pages serve later splits; and threaded
tests: writers whose keys readers check as they appear, threads inserting
and updating the same keys, threads removing while others insert, threads
racing to remove the same keys, threads checking their own interleaved
keys against models of their own, with many keys and with about a leaf of
them, threads appending while another shrinks the shared prefix, and
scans of the steady keys while other threads insert and remove others,
which visit each once, in order, in both directions, and the same for
`range_cloned` iterators (`map/tests.rs`). Scans of random ranges,
in both directions, and `range_cloned` from both ends in random
interleavings, match std's `range`; a `range_cloned` iterator hands out a
key inserted ahead of it in a leaf it copied, and not one removed;
`get_or_insert` matches std's `entry().or_insert`, and threads racing to
`get_or_insert` the same keys all see the entry the map keeps;
`update_or_insert`, single and batched in turns with `insert_many`,
matches std's `entry`, and threads counting the same keys with it, half
in batches, miss no count; `remove_if` removes only what its condition
holds of. Batched lookups,
8 and 65 keys at a time, on every key the differential tests insert and
look up, keys outside the shared prefix included, match separate `read`s
and run the closure once per entry found; and batches of steady and
changing keys, while threads insert and remove, find every steady key,
also while the root grows from a leaf and lowers back (tests run lanes on
every tree with an internal root). Copies, single and batched, while
threads insert, remove and update in place, are always of their own key's
value, for integer keys and for short and long strings; threads that
batch their inserts while others look up find each key from its batch on.

Races that need a narrow schedule escape them, and a 12-thread release
stress run outside the crate found two: a removal that left the path below
a replaced separator unchanged, and a descent that read the root's version
after a lowering to a leaf, without checking the root pointer again. The
model tests passed with both; the stress run's models crashed on the
second only with about a leaf of keys, the root splitting and lowering
over and over (`threads_raise_and_lower_the_root_around_one_leaf` keeps
that shape). Those races need controlled schedules.

### Shuttle

Shuttle (`map/shuttle_tests.rs`, `.github/workflows/shuttle.yml`) runs a test's
threads one at a time and switches between them where the tree touches an
atomic that threads share or waits for another thread
(`tree/interleaving.rs`): a latch word, the root and last-leaf pointers, a
free list, the header fields a racing read loads. Built with
`--cfg shuttle`, like loom's `cfg(loom)`; other builds compile the points
to nothing. Shuttle's own atomics would do the same, but the latch words
live in node pages and racing reads load page fields in place. One thread
runs at a time, so Shuttle finds bad interleavings, not reorderings of a
weak memory model, nor torn reads of a node a writer is changing.

Each schedule reruns the whole test, so the tests stay small. Values are
64 bytes, so a leaf holds 12, and each test sets up a tree one operation
from a change, its shape found by building it (`Shapes`): a full root leaf,
two leaves a removal merges back into it, a full root of two levels, three
levels a removal lowers to two, or one of these with a sibling filled so
that a removal takes from it instead of merging. Small tests race two or
three threads of one to four operations over one change: an empty tree
started, emptied, raised from a leaf and lowered back, raised from two
levels to three and lowered back, leaves and internal nodes taking from a
full sibling, separators removed (the root's too), the shared prefix
shrunk under appends, lookups and scans from outside it, the last leaf
freed under appends, a freed page reused under a
cloned range, and removals and `get_or_insert`s racing for one key. Three
larger ones run random operations, every kind the map has, against
per-thread models around one leaf, around the last leaves of three levels,
and with string keys whose 4-byte windows tie. Every closure checks its
value is the requested key's and not dropped, after a switch point: a
closure that runs on an entry its thread has not latched sees another
thread drop it. A test whose race may or may not reach its change counts
the schedules that did, and fails if none did. Schedules cost about 60 ns a
switch, a test thousands to hundreds of thousands of schedules.

Checked by breaking the tree on purpose, a check at a time: the tests
catch a scan or a descent that skips the root pointer's second read, a
lookup that skips its leaf's or its parent's version, an append that skips
its check of the last leaf, a removal that takes neither sibling leaf nor
sibling internal node, and a new tree's root leaf let go before appends
point at it. They miss two kinds of break. A descent that skips its check
that the leaf's head is the separator it matched is not one: a removal of
the separator holds every node from it down to the leaf, so a parent's
version check fails first. And a change to a node its thread doesn't hold,
as a removal of a separator that doesn't take the right border left of it,
or a shrink of the shared prefix that doesn't take the right border, whose
prefixes each refits: Shuttle runs a change whole between two switch points,
so no reader sees it half done. A debug check that a latched node's writer
holds it would catch those.

It found two bugs the threaded tests and the stress run had not:

- **A scan from either end** read the root's version without checking the
  root pointer again (`scan.rs`, `descend_to_first_leaf`), the descent's
  old bug: after a lowering to a leaf, it took the leaf's bytes for an edge
  and latched whatever they pointed at.
- **A lookup of a separator key**, batched or not, latched the leaf and
  checked only that its head was the key (`batch.rs`, `read_head`;
  `lookup.rs`, `find_optimistic`), not that the leaf was still at the
  version read under its parent. A merge leaves the freed page's bytes as
  they were, so the lookup ran the closure on a stale copy of an entry that
  lives on in the sibling, which another thread could then remove and drop.

ffs, a deterministic-scheduling tester run by an outside reviewer (issue
#9, 2026-10-19), found one Shuttle had no test for: a scan from a bound
outside the shared prefix started at the first entry after a shrink had
put a key before the bound (Scans, above). No test scanned from outside
the prefix while it shrank. `scans_from_outside_the_shared_prefix_race_a_shrink`
does, and Shuttle finds the bug in its first schedules.
