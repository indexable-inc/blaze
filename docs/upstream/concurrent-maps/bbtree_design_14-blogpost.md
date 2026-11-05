The blog post that goes with the visualization.

## Blog content

The post's text as the site shows it (`vis/src/post.rs`,
`vis/src/use_cases.rs`, `vis/src/how_it_works.rs`,
`vis/src/correctness.rs`; its headings and their ids in
`vis/src/outline.rs`). Bracketed lines mark where a figure or the charts
sit; each figure names its SVG file and its caption, as the site shows
them. Keep this text and the site in step: a change to one is a change to
the other. On the site, the citations sit beside
their lines in the right margin and the outline in the left, as on the
author's blog (Quarto's `reference-location: margin`); each part (`##`)
opens with an overview, and its sections (`###`) are tabs under it, one
open at a time, so that an impatient reader gets the part from its
overview and opens only the section they want (`vis/src/part_tabs.rs`):
the first is open until the reader picks another, a pick names its
section in the address, a link to any heading in a section opens it, and
so does the browser's find (the closed sections stay in the page, hidden
until found); an open section ends with a bar across the text that opens
the next ("Next, 2 of 3"), so
that a reader who reads on can't miss that more follows; the three principles are
numbered, since the post ranks them; and the post has a dark theme. The figures are SVG files that
`vis/figures/` writes into `vis/assets/figures/` (`cargo run --release
--manifest-path vis/figures/Cargo.toml`), so that the same files go into
any other page, such as a Quarto copy of the post: each plays its own CSS
animation in a loop and rests on its last frame with reduced motion; the
site draws it into the page as the reader nears it, plays it from the
start once they see it, and its pause holds it where it is.

===============================================

# Title: bbtree: fastest ordered map

> B-trees are by far the most important access path structure in database and file systems. — [Jim Gray](https://en.wikipedia.org/wiki/Jim_Gray_(computer_scientist)) and Andreas Reuter[^gray]

**bbtree** is by far the fastest B-tree (that I'm aware of):
1. 5× faster than std's `BTreeMap` on a single thread, 12× faster than `crossbeam-skiplist` at 96 threads.
2. 30% lower memory usage.
3. Zero dependencies, `no_std` compatible.
4. `bbtree::unsync::BBTreeMap` is a drop-in replacement for std's `BTreeMap`/`BTreeSet`.
5. Production-ready by formal verification, model checking, deterministic simulation, fuzzing, and lots of testing.
6. Brought to you by decades of database research and careful curation by [Xiangpeng Hao](https://xiangpeng.systems).

[Charts: Single thread, Concurrent, End to End, Memory, Architectures]

## What can it do?

B-trees are the most important data structure in big data systems, a fast b-tree like `bbtree` benefits everything on top of it: agents memory, LLM training, RAG, KV store, state caching, vector search, and more. 

Here we discuss three representative use cases:
1. **Replace skiplist in LSM-trees.** In place of the skiplist of four LSM-trees, `bbtree` is 1.37–6.83× faster.
2. **In-memory database index.** In place of Turso's skiplists, inserts are 2.22× and indexed `SELECT`s 1.90× faster.
3. **Mmap a bbtree from disk.** No rebuild after a restart: the first answer comes 160–2,500× sooner than std's `BTreeMap`.

### Case 1: faster memtable 

All LSM-trees use an ordered map to hold recent writes in memory, the memtable. In Rust, this ordered map is usually a skiplist, like `crossbeam-skiplist`.

`bbtree` can easily replace it and is much faster. A skiplist takes a cache miss for almost every node it visits on its way down; a `bbtree` lookup touches a handful of cache lines (see How it works, below).

`bbtree` has a similar interface as `crossbeam-skiplist` but simpler: there is no epoch, no need to worry about when to pin or re-enter an epoch.

`bbtree` allows more advanced memory management:
1. Custom allocator is supported on day one.
2. It never allocates on irregular sizes (i.e., not a power of two): memory management stays easy, with no external fragmentation[^free-space].
3. Allocation failure can be gracefully handled.

We integrated `bbtree` into four LSM-trees, on each system's own benchmarks:
1. [fjall](https://github.com/fjall-rs/fjall): gets 2.47×, inserts 1.61×, write batches 3.00×.
2. [SlateDB](https://github.com/slatedb/slatedb): gets 1.71×, puts 1.65×, write batches of 10 rows 2.36×.
3. [Tonbo](https://github.com/tonbo-io/tonbo): 10M-row writes 6.83×, conflict-checked transactions 4.10×.
4. [Lance](https://github.com/lance-format/lance)'s MemWAL index (its own skiplist): point lookups 2.19×, batched lookups 3.94×, batched puts 4.21×.

[the End to End card's LSM-trees again, as a figure: `end-to-end.svg`. Caption: The LSM-trees of the End to End card: each system's own benchmark, one thread, Ryzen 9 9900X. The dark bar is the time per operation as shipped, the bright one inside it with `bbtree`; only the map changed.]

### Case 2: database index

A database's tables change while they are read: rows are inserted, updated and deleted between queries, and every index of a table changes with it. An in-memory database keeps each in an ordered map, which has to stay sorted under these writes.

[Turso](https://github.com/tursodatabase/turso), SQLite rewritten in Rust, keeps its MVCC store in memory: its tables' rows and its indexes' entries, each in a `crossbeam-skiplist`. With `bbtree` in their place, on Turso's own benchmarks:
1. `INSERT` of random rowids: 2.22×.
2. Indexed point `SELECT`: 1.90×.
3. `UPDATE` in `BEGIN CONCURRENT`: 1.22×.
4. Point `SELECT` by rowid: 1.16×.

[the End to End card's Turso again, as a figure: `database-index.svg`. Caption: Turso on the End to End card: its own benchmark, one thread, Ryzen 9 9900X. The dark bar is the time per operation as shipped, the bright one inside it with `bbtree`; only the map changed.]

### Case 3: bbtree-mmap

[`bbtree-mmap`](https://crates.io/crates/bbtree-mmap) allows you to build a bbtree and write it to disk once; when you need it, you can simply mmap it and use it instantly, without rebuilding anything.

```bash
cargo add bbtree-mmap
```

**Build a bbtree and save to disk**
```rust
use bbtree::unsync::BBTreeMap;
use bbtree_mmap::BBTreeMapFile;

let names = BBTreeMap::from([(7_u64, String::from("ferris")), (42, String::from("corro"))]);
BBTreeMapFile::<u64, String>::create("names.bbtree", &names)?;
```

**Mmap bbtree and query it immediately**
```rust
use bbtree_mmap::BBTreeMapFile;

let names = unsafe { BBTreeMapFile::<u64, String>::mmap("names.bbtree").open()? };
assert_eq!(names.get(&42), Some("corro"));
```

[the time to the first answer after a restart, std's `BTreeMap` rebuilt against `bbtree-mmap` mapped, with the file in the page cache and from disk: `first-query.svg`. Caption: Time to the first answer after a restart, 10M `u64` entries, Ryzen 9 9900X. std's `BTreeMap` reads its file back and rebuilds; `bbtree-mmap` maps its file and reads only the pages the lookup touches.]

## How it works?

Every performance optimization should be guided by a cost model.

B-tree optimizations are simple, three easy pieces[^ostp]: we just need to remember three principles, in order:
1. The number of **chained cache lines** touched. A cache miss costs about 100 ns. 
2. The number of **instructions** executed. An instruction costs about 0.5 ns. 
3. The amount of **cache coherence traffic**, especially important for concurrent access. It forces a cache miss, and costs a little more than 100 ns. 

### Minimize cache misses

With this cost model, it's clear that the most important optimization is to minimize the number of cache misses.
To put it another way, for each cache miss, we want to narrow down the search space as much as we can.

#### Thought experiment: binary search vs linear search

Say we have a sorted array of `u64`s, and we want to find a target `u64`.

With binary search, each cache miss narrows down the search space by half.

With linear search, each cache miss narrows down the search space by `cache_line_size / item_size` items, which is 64 / 8 = 8.

So the break-even point is about 16 items, where binary search costs the same as linear search: two cache misses to find the target.

But there's a catch: 1. the CPU prefetcher may fetch the next cache lines on its own, which gives linear search an even bigger advantage; 2. linear search is simpler, allowing much more parallelism in the CPU pipeline. So the actual break-even point is usually higher.

[a figure to compare binary search and linear search side by side, with animation: `search-space.svg`. Caption: 64 sorted `u64`s, 8 to a cache line. Each step is one cache miss: binary search halves what is left, linear search rules out one line. Here binary search needs 3 misses and linear search 6; at 16 items, both need at most 2.]

#### How does that apply to B-trees?

In a B-tree, this decides the node size[^cache-conscious]. If we size nodes so that each holds about 16–32 items, we can always do linear search, which lets us do nice things like SIMD (in later sections).

**What if the keys are Strings?**
`String`s are different, because their bytes are stored on the heap, not inline in the node.
This changes our equation completely. We no longer get 8 keys from one cache miss; instead, each key we load is its own cache miss. So we almost always want to do binary search over `String`s.

[a small figure to show the layout of an array of `String`s: what's on the heap, what's in the node: `string-keys.svg`. Caption: A node of `String`s holds each key's pointer, length and capacity; its bytes are on the heap. Every comparison of a binary search follows a pointer: 3 heap reads to find one key of 8.]

**How does `bbtree` do it?**
Heap keys are very common, so `bbtree` must handle them well.
`bbtree` introduces `inline_keys`, an idea databases have used for decades[^graefe-larson][^graefe], similar to German strings[^umbra] and `StringView` in Arrow[^stringview].

With fixed-size inline keys, we get back the benefit of one cache miss loading 8 (or more) keys.
And we only load the full key when the comparison ties.

[a figure to show the layout of `inline_keys`: `inline-keys.svg`. Caption: The same node with inline keys: the first 8 bytes of every key, 64 bytes, one cache line. One compare of all of them leaves a single candidate, and only it is read from the heap.]

**What about keys with long prefixes?**
Many keys, like URLs and paths, have long prefixes. If we just take the first 8 bytes as the inline key, we may compare nothing at all.
`bbtree` addresses this with `prefix_keys`: the common prefix of all keys in a node[^prefix-btree].
The beauty of `prefix_keys` is that no node has to store them. Every key in a node sorts between the two separators on either side of the node in its parent. So if those two separators share their first 20 bytes, every key in the node starts with the same 20 bytes.

[a figure with parent and child nodes: two separators in the parent, and the child's prefix is what the two separators share: `prefix-keys.svg`. Caption: Neighboring Wikipedia titles. The parent's two separators share their first 20 bytes, so every key of the child does too. Over the first 8 bytes, the keys all tie; past the shared 20, they all differ.]

So each node only stores the length of its prefix, and keeps the window of each key right after it, which makes inline keys far more effective.

**How does this apply to other key types, and how does it work with Rust's type system?**

It's clear that `String` and `Vec<u8>` benefit from this optimization. But integers like `u64` and `u128` can too; we just need to mind the endianness: stored big-endian, their bytes sort in the same order as the numbers.

[a small figure to show a `u64` array, decomposed into bytes: the common prefix, the inline keys (2 bytes), and the rest of the keys: `integer-keys.svg`. Caption: Four keys of one leaf of a tree of 10M random `u64`s, as bytes: the prefix its separators share, which the leaf skips; the 2 bytes it keeps as each inline key; and the rest, read only on a tie.]

For other key types, like compound keys in structs, users can implement the `InlineKey` trait, and when they do, we use the inline keys wherever we can.

```rust
pub trait InlineKey: Ord {
    /// How many bytes of each inline key the nodes keep: 2, 4 or 8.
    const WIDTH: InlineWidth = InlineWidth::Eight;

    /// Bytes `offset..offset + 8` of the key's encoding, big-endian,
    /// zero past its end, which orders as the keys do.
    fn inline_key_at(&self, offset: usize) -> InlineWord;
}

// A compound key, ordered by customer, then order: its encoding is
// both fields' bytes, one after the other.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct OrderKey {
    customer: u32,
    order: u32,
}

impl InlineKey for OrderKey {
    fn inline_key_at(&self, offset: usize) -> InlineWord {
        let mut bytes = [0; 16];
        bytes[..4].copy_from_slice(&self.customer.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.order.to_be_bytes());
        let start = offset.min(8);
        u64::from_be_bytes(bytes[start..start + 8].try_into().unwrap())
    }
}

let orders: BBTreeMap<OrderKey, u64, _> =
    BBTreeMap::with_strategy(WithInlineKey);
```

The trickiest part is Rust's type system. If we required every key type to implement `InlineKey`, `bbtree` couldn't be a drop-in replacement for std's `BTreeMap`/`BTreeSet`.
Nightly Rust has `specialization`[^specialization] for exactly this.
For stable Rust, we specialize the common types by hand: `u64`, `i64`, `u128`, `i128`, `String`, `str`, `&str`, `Vec<u8>`, `&[u8]`, `Box<str>`, `Box<[u8]>`, `[u8; 16]`, `[u8; 20]` and `[u8; 32]`.
So without any extra work, the built-in types are optimized, and users can still implement the trait for their own types to get the full benefit.

#### Results

After these optimizations, we can show how many cache lines a lookup touches.

[a bar chart comparing std's `BTreeMap` and bbtree on different key types: cache lines from memory per lookup: `cache-lines.svg`. Caption: Cache lines brought in from memory per lookup that finds its key, 10M entries, one thread, Ryzen 9 9900X; lower is better.]

#### Bonus: batch mode using Rust's async state machines

So far we have focused on maximizing the value of a cache miss: packing as much information as possible into one cache line.

There's a more advanced optimization: hiding the cost of cache misses.

The simplest way to hide latency is software prefetching: while working on one key, we prefetch the next one.
This usually works well, but it isn't enough for B-trees.

This is because a **B-tree's memory accesses are fundamentally chained**: you don't know which node to fetch next until you have searched the current one.

But B-trees are often read or built in batches, which lets us play a trick with Rust's async state machines. A batch is one call with many keys, such as `BBTreeMap`'s [`get_many_copied`](https://docs.rs/bbtree/latest/bbtree/struct.BBTreeMap.html#method.get_many_copied) and [`insert_many`](https://docs.rs/bbtree/latest/bbtree/struct.BBTreeMap.html#method.insert_many): an array of keys in, an array of answers out.

The high-level idea is simple: in Rust we often treat an IO operation as a `Future`, and `await` it. In `bbtree`, we treat a cache miss as a `Future`, and `await` it too. Database research has interleaved lookups like this for years, by hand and with coroutines[^interleaving].

The code looks roughly like this (they run in a single thread):

```rust
fn get_many<const N: usize>(&self, keys: [&K; N]) -> [Option<V>; N] {
    let lanes = keys.map(|key| self.search(key));
    block_on(join_all(lanes))
}
```

And `search` looks like this:

```rust
async fn search(&self, key: &K) -> Option<V> {
    let mut node = self.root();
    while node.is_internal() {
        let edge = node.search(key);
        node = node.child(edge);
        prefetch(node.header()).await;
    }
    node.get(key)
}
```

[four `u64` keys looked up together, from bbtree's trace of the batch, the core as the axis and a row a task under it: `batch-lanes.svg`. Caption: Four `u64` keys looked up together on one core, each key's search a task, from bbtree's own run of the batch; time is counted in turns. In its turn, a task reads the line it prefetched, steps down the tree, prefetches the next line and yields; while that line is on its way, the core runs the other tasks.]

But we need to be very careful:
1. A cache miss costs 100 ns, so our `Future` must be as lightweight as possible. This means we can't use `tokio` or any general-purpose async runtime, because their overhead is already higher than a cache miss. `bbtree` polls its lanes with a tiny executor of its own: no allocation, no runtime.
2. Each async function in Rust is a state machine generated at compile time. We want this state machine to be as small as possible, so that switching between states is cheap.
3. There's no way to know whether a cache line is already in cache, so we have to guess. In `bbtree` that's not so hard: once the leaves outgrow the L2 cache, a leaf is almost always a cache miss; smaller trees look their keys up one at a time.

### Minimize instructions

In general, minimizing instructions means reducing CPU time,
which means two things: 1. reducing the instruction count; 2. reducing branches.

A mispredicted branch costs about 20 cycles (the time of 20 instructions), about 10% of a cache miss.

#### Binary search vs linear search, again

With inline keys, searching a node means searching a small array of small integers: 2 bytes a key for integers and UUIDs, 4 to 8 for strings.
Now let's compare their instructions and branches over a node of 32 keys:

1. Binary search: about 5 steps, each a compare and a branch, and the CPU typically guesses wrong half of the time. A branchless binary search[^layouts] avoids the guess, but then each step waits for the one before.
2. Linear search, without its early exit, becomes a count: compare the target with every key, and count the keys smaller than it; that number is the child to follow. It does more compares, but none branches or waits for another, a perfect fit for the CPU pipeline.

[binary search against the count over eight inline keys, each row a compare's result per slot, the child both reach marked across the rows: `key-count.svg`. Caption: Looking up 45 among seven inline keys, all in one cache line: neither search takes another miss. The count compares all of them at once: four are smaller, so it follows child 4. Binary search reaches child 4 after three compares, each waiting for the one before; over 32 keys, it takes about five.]

#### How does `bbtree` search?

With SIMD, searching a node takes a handful of instructions[^kary].
One AVX-512 instruction compares the target with 32 two-byte keys at once; its result is a 32-bit mask, a bit a key, and the number of ones in the mask, its popcount, is the answer:

```rust
fn child_index(inline_keys: &[u16; 32], target: u16) -> usize {
    let keys = _mm512_loadu_epi16(inline_keys.as_ptr().cast());
    let smaller =
        _mm512_cmplt_epu16_mask(keys, _mm512_set1_epi16(target as i16));
    smaller.count_ones() as usize
}
```

The function has only three steps, all branchless and super fast:
1. `_mm512_loadu_epi16` loads the inline keys into a SIMD register.
2. `_mm512_cmplt_epu16_mask` compares them with the target and returns a mask, a bit for each smaller key, in key order (e.g., `1111100000000000`).
3. `smaller.count_ones()` counts the ones in the mask, which is the index of the child to follow. x86 does this in one instruction, `popcnt`.

We also implement the same search for ARM in NEON, so Apple Silicon and AWS Graviton work too.
On x86, AVX-512 needs `-C target-cpu=native` (Rust 1.89 or later).

#### The compiler is not your friend

We often hear "write simple code and let the compiler optimize it". The extreme version is FastLanes[^fastlanes], whose scalar code the compiler automatically vectorizes (generates SIMD code for).

Here the compiler is not your friend. A node holds few keys, a few dozen at most, and their number is usually not a power of two.
The compiler often considers it not worth vectorizing, and sometimes can't prove it safe, so we write the SIMD by hand, and handle the boundary cases very carefully.
On an M1 Pro, the compiler's own count widened each compare's result to 64 bits and added them up one at a time. We wrote the count in NEON by hand and changed nothing else: the same tree, the same nodes, the same cache lines.
A lookup of a `u64` key in a map of 10M entries went from 1,678 to 407 instructions, and from 390 to 174 ns.[^neon-count]

Vectorization isn't the only problem: sometimes the compiler generates bad code because it doesn't know the workload.
For example, to compare two `&[u8]`, Rust calls `memcmp`, a function call that isn't inlined.
But in `bbtree`, we know the keys are usually short, so instead of calling `memcmp`, we load the bytes as integers and compare the integers. Arrow does the same for `StringView`[^stringview][^arrow-compare].

#### Results

[a bar chart comparing std's `BTreeMap` and bbtree on different key types: instructions per lookup: `instructions.svg`. Caption: Instructions per lookup that finds its key, 10M entries, one thread, Ryzen 9 9900X; lower is better.]

### Minimize cache coherence traffic

Cache coherence traffic happens when one core writes to a cache line that other cores hold. To make sure every core sees a consistent view of memory, CPU caches implement a cache coherence protocol[^primer]. At a very high level, a write to a cache line invalidates all copies of that line in other cores.

This causes two problems: 1. more cache misses; 2. more importantly, it puts a ceiling on the B-tree's concurrency.

On modern hardware, a contended cache line supports at most about 10 M op/s. So if we simply put a `Mutex` over the B-tree, we reach at most 10 M op/s, while `bbtree` reaches 180 M op/s at 96 threads.

Note that a `RwLock` won't help: even a read lock must __increment__ the lock's reader count, which is a write to its cache line.

So how does `bbtree` solve this?

The key idea is a version lock[^olfit][^olc]. Every node carries a version: a reader reads the version, reads the node, and reads the version again. If the version hasn't changed, what it read is consistent; otherwise, it retries.

[a figure to show the version lock: `version-lock.svg`. Caption: Time runs down. Readers only read. Reader 1 finished before the writer changed anything, so its read stands. Reader 2 overlapped the change, finds version 8, and starts over.]

This allows very high concurrency, because a read causes no write traffic at all.

But there are two things we need to be careful about.

##### Problem 1: we can no longer hand out a `&V` to the user

The value may change, or even be freed, while the user holds the reference, so it's unsafe to use.

We can't return a clone of `V` either, because `V` may not be `Clone`.

Here we use a trick similar to Bf-Tree's[^bftree]: only inner nodes are optimistic; leaves are not.
This works because (1) contention usually happens at inner nodes, (2) values are stored only in leaves, and (3) leaves are only 20% of the nodes on a lookup's path (assuming a tree of 5 levels).
For `Copy` values, `get_copied` goes one step further: it reads the leaf optimistically too, copies the value out, and checks the version after, like a seqlock.

##### Problem 2: memory reclamation

The second problem is more subtle: if one thread is reading a node while another thread frees it, the reader may segfault.

The usual solution is epoch-based memory reclamation[^ebr], e.g., `crossbeam-epoch`.

But then we either leak the epoch to the user, which is ugly and error-prone, or pin an epoch on every access (many libraries do this), which is expensive: every access is a global read and a private write.

In `bbtree`, we do it differently: a freed node never goes back to the allocator while the map lives. Instead, the map keeps it on a free list and reuses it for the next node it needs.
The node keeps its version through both steps: freeing it bumps the version, reusing it bumps the version again, and a version only grows. So a late reader still reads a node, not arbitrary memory, finds a higher version than it saw, and retries, without any segfault.

#### Results

[the concurrent results again, `u64` gets: `concurrent-gets.svg`. Caption: Gets of `u64` keys, half of them misses, 10M entries, all threads together, a thread a core, on AWS's c8a.metal-24xl (96 cores); the Concurrent card above has the other keys and workloads.]

`bbtree` is wildly concurrent: its get throughput grows 83–90× as the core count grows to 96, 86–94% of linear scaling.
At this thread count, `bbtree` is even faster than [DashMap](https://github.com/xacrimon/dashmap), a _hashtable_.


## How do we make it correct?

Performance optimization is easy: you simply ask Claude or Codex to make the code 10× faster on a workload. Speed is measurable, and LLMs can iterate on it relentlessly.

But making sure the code is correct is hard. Correctness is implicit: you can never list every DON'T in the prompt, you can never write enough tests to prove the code correct, and you can never enumerate every property you want to hold.

`bbtree` has three guards to approach correctness.


### Guard 1: Xiangpeng Hao

[Xiangpeng](https://xiangpeng.systems) is a real human behind `bbtree`, and the front line guarding the correctness of its code.
All of the code is written by LLMs, but Xiangpeng steers every design decision, and is accountable for the code.

He spent his PhD writing many, many B-trees, including one that runs in production at Microsoft (Bf-Tree[^bftree]). Choosing `bbtree` means offloading the attention to Xiangpeng.

### Guard 2: LLMs

No human can write better code than LLMs. All of the implementation is written by LLMs, and cross-checked by LLMs from different providers.

### Guard 3: fuzzing, verification, DSTs

Conventional approaches still apply here:
1. **Fuzzing** runs random operations on `bbtree` and std's `BTreeMap` side by side, checks that every answer matches, and checks the whole tree's invariants after every operation: [8 targets](https://github.com/XiangpengHao/bbtree/tree/main/bbtree/fuzz/fuzz_targets), fuzzed weekly and before every release ([CI](https://github.com/XiangpengHao/bbtree/blob/main/.github/workflows/fuzz.yml)).
2. **Verification** with Verus[^verus] proves the critical properties of the code, e.g., that the inline keys order the same way as the keys themselves: [the proofs](https://github.com/XiangpengHao/bbtree/tree/main/bbtree/src/proof), checked on every pull request ([CI](https://github.com/XiangpengHao/bbtree/blob/main/.github/workflows/verus.yml)).
3. **DSTs** (deterministic simulation testing) with Shuttle[^shuttle] and ffs[^ffs] test concurrent behavior in a deterministic and principled way: Shuttle runs [each test](https://github.com/XiangpengHao/bbtree/blob/main/bbtree/src/map/shuttle_tests.rs) under thousands of thread schedules on every pull request ([CI](https://github.com/XiangpengHao/bbtree/blob/main/.github/workflows/shuttle.yml)); ffs, run by its author, found a race Shuttle had missed ([issue #9](https://github.com/XiangpengHao/bbtree/issues/9)), now fixed and tested.


## Reviewers of this post

Thanks to [Andrew Lamb](https://andrew.nerdnetworks.org/) (InfluxData), [Zac Farrell](https://github.com/zfarrell) (Hotdata), [Eddie Tejeda](https://eddietejeda.com/) (Hotdata), [Divya Ranganathan](https://www.linkedin.com/in/divs1101/) (Hotdata), [Jacopo Tagliabue](https://jacopotagliabue.it/) (bauplan), [Wanxiang Zhong](https://shawnzhong.com) and [Junxuan Liao](https://github.com/MikeWalrus) for reviewing this post.

[^ostp]: Not more complicated than an operating system! Remzi H. Arpaci-Dusseau and Andrea C. Arpaci-Dusseau, [Operating Systems: Three Easy Pieces](https://pages.cs.wisc.edu/~remzi/OSTEP/) (2016).

[^free-space]: Remzi H. Arpaci-Dusseau and Andrea C. Arpaci-Dusseau, [Free-Space Management](https://pages.cs.wisc.edu/~remzi/OSTEP/vm-freespace.pdf), chapter 17 of *Operating Systems: Three Easy Pieces*: external fragmentation is free space chopped into pieces of different sizes, so that a request can fail although the total would fit it.

[^gray]: Jim Gray and Andreas Reuter, *Transaction Processing: Concepts and Techniques* (1993), as quoted by Goetz Graefe in [Modern B-Tree Techniques](https://doi.org/10.1561/1900000028) (2011).

[^cache-conscious]: Jun Rao and Kenneth A. Ross, [Making B+-Trees Cache Conscious in Main Memory](https://doi.org/10.1145/342009.335449) (SIGMOD 2000).

[^graefe-larson]: Goetz Graefe and Per-Åke Larson, [B-Tree Indexes and CPU Caches](https://doi.org/10.1109/ICDE.2001.914847) (ICDE 2001).

[^graefe]: Goetz Graefe, [More Modern B-Tree Techniques](https://doi.org/10.1561/1900000070) (2024): keep full keys out of the comparison stream with dense, fixed-width codes, and skip the prefix a page shares.

[^umbra]: Thomas Neumann and Michael Freitag, [Umbra: A Disk-Based System with In-Memory Performance](https://db.in.tum.de/~freitag/papers/p29-neumann-cidr20.pdf) (CIDR 2020); see also my post on [the nature of strings](https://blog.xiangpeng.systems/posts/nature-of-string/).

[^stringview]: Xiangpeng Hao and Andrew Lamb, Using StringView / German Style Strings to Make Queries Faster: [Part 1, Reading Parquet](https://datafusion.apache.org/blog/2024/09/13/string-view-german-style-strings-part-1/) and [Part 2, String Operations](https://datafusion.apache.org/blog/2024/09/13/string-view-german-style-strings-part-2/) (2024).

[^prefix-btree]: Rudolf Bayer and Karl Unterauer, [Prefix B-Trees](https://doi.org/10.1145/320521.320530) (TODS 1977).

[^specialization]: [RFC 1210: impl specialization](https://rust-lang.github.io/rfcs/1210-impl-specialization.html), unstable since 2016 ([tracking issue](https://github.com/rust-lang/rust/issues/31844)).

[^interleaving]: Onur Kocberber, Babak Falsafi and Boris Grot, [Asynchronous Memory Access Chaining](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf) (VLDB 2015); Georgios Psaropoulos et al., [Interleaving with Coroutines: A Practical Approach for Robust Index Joins](https://www.vldb.org/pvldb/vol11/p230-psaropoulos.pdf) (VLDB 2017); Christopher Jonathan et al., [Exploiting Coroutines to Attack the "Killer Nanoseconds"](https://www.vldb.org/pvldb/vol11/p1702-jonathan.pdf) (VLDB 2018).

[^layouts]: Paul-Virak Khuong and Pat Morin, [Array Layouts for Comparison-Based Searching](https://arxiv.org/abs/1509.05053) (2017).

[^kary]: Benjamin Schlegel, Rainer Gemulla and Wolfgang Lehner, [k-ary Search on Modern Processors](https://doi.org/10.1145/1565694.1565705) (DaMoN 2009).

[^fastlanes]: Azim Afroozeh and Peter Boncz, [The FastLanes Compression Layout: Decoding >100 Billion Integers per Second with Scalar Code](https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf) (VLDB 2023).

[^neon-count]: Lookups of `u64` keys that hit, in a map of 10M `u64` entries, one run each on an M1 Pro, with bbtree-eval's [portable probe](https://github.com/XiangpengHao/bbtree-eval/tree/main/diagnostics/portable), before and after [decision D56](https://github.com/XiangpengHao/bbtree/blob/main/design/03-decisions.md), which counts inline keys in NEON.

[^arrow-compare]: arrow-rs [#7748](https://github.com/apache/arrow-rs/pull/7748) compares inlined string views as one `u128`, and their 4-byte prefixes as one `u32`, instead of byte by byte.

[^primer]: Vijay Nagarajan, Daniel J. Sorin, Mark D. Hill and David A. Wood, [A Primer on Memory Consistency and Cache Coherence](https://doi.org/10.1007/978-3-031-01764-3), second edition (2020).

[^olfit]: Sang K. Cha, Sangyong Hwang, Kihong Kim and Keunjoo Kwon, [Cache-Conscious Concurrency Control of Main-Memory Indexes on Shared-Memory Multiprocessor Systems](https://www.vldb.org/conf/2001/P181.pdf) (VLDB 2001).

[^olc]: Viktor Leis et al., [The ART of Practical Synchronization](https://db.in.tum.de/~leis/papers/artsync.pdf) (DaMoN 2016).

[^bftree]: Xiangpeng Hao and Badrish Chandramouli, [Bf-Tree: A Modern Read-Write-Optimized Concurrent Larger-Than-Memory Range Index](https://www.vldb.org/pvldb/vol17/p3442-hao.pdf) (VLDB 2024); code at [microsoft/bf-tree](https://github.com/microsoft/bf-tree).

[^ebr]: Keir Fraser, [Practical Lock-Freedom](https://www.cl.cam.ac.uk/techreports/UCAM-CL-TR-579.pdf) (PhD thesis, 2004); in Rust, [`crossbeam-epoch`](https://docs.rs/crossbeam-epoch).

[^pin]: crossbeam's [`SkipMap::get`](https://docs.rs/crossbeam-skiplist/0.1.3/crossbeam_skiplist/map/struct.SkipMap.html) pins an epoch on every call; scc's [`TreeIndex`](https://docs.rs/scc/latest/scc/tree_index/struct.TreeIndex.html) either takes a guard from the caller (`peek`) or pins one on every call (`peek_with`).

[^tsm]: Michael Greenwald and David Cheriton, [The Synergy Between Non-Blocking Synchronization and Operating System Structure](https://www.usenix.org/legacy/publications/library/proceedings/osdi96/full_papers/greenwald/index.html) (OSDI 1996), which calls this type-stable memory.

[^verus]: Andrea Lattuada et al., [Verus: Verifying Rust Programs using Linear Ghost Types](https://doi.org/10.1145/3586037) (OOPSLA 2023); code at [verus-lang/verus](https://github.com/verus-lang/verus).

[^shuttle]: [Shuttle](https://github.com/awslabs/shuttle), a library for testing concurrent Rust code, from AWS.

[^ffs]: [ffs](https://github.com/aoli-al/ffs), fork-based deterministic concurrency testing.








## Status and decisions (2026-10-17, from the runs)

Added by the session that built the post; the blog content above is the
author's.

- **Where it lives.** The post is `vis/src/post.rs` at `/post/`. Its cards
  (`performance_cards.rs`) and charts (`single_thread_chart.rs`,
  `concurrent_chart.rs`, `end_to_end_chart.rs`, `memory_chart.rs`,
  `architectures_chart.rs`, whose grouped bars are `baseline_bars.rs`)
  compile in the CSVs of `vis/data/<machine>/` (`measurements.rs`). The
  runs live in [bbtree-eval](https://github.com/XiangpengHao/bbtree-eval):
  its `blog/runs.sh` runs every chart's cells on a machine into its
  `blog/results/`, and `blog/collect.sh` merges them into this
  repository's `vis/data/`; a cell rerun (`reruns/`) replaces the first run
  of it. On AWS, `blog/aws-runs.sh` runs `blog/runs.sh` on a spot instance
  it launches and terminates.
- **Figures (2026-10-18).** "How it works" is drawn by SVG files, the
  same on the site and in any copy of the post (the author's Quarto
  blog): `vis/figures/` writes them into `vis/assets/figures/`, the
  mechanisms from fixed keys, `integer-keys.svg` and `batch-lanes.svg`
  from a tree of 10M random `u64`s it builds and reads through bbtree's
  inspect views, and the charts from `vis/data/`; `end-to-end.svg` and
  `database-index.svg`, the End to End card's LSM-trees and Turso again
  in "What can it do?", from the 9900X's `integrations.csv`;
  `first-query.svg`, in "Case 3: bbtree-mmap", from
  `vis/data/9900x-20261021/first-query.csv` (below). Each plays a CSS loop
  and rests on its last frame with reduced motion; the charts play once.
  The two Results charts are `vis/data/9900x-20261018/lookups.csv`:
  bbtree-eval's `bbtree-bench --perf`, gets that hit, 10M entries, `u64`
  values, bbtree and std's `BTreeMap`, on `u64`, `hash32` (32-byte
  hashes), JSON keys and Wikipedia titles, one run. With the NMI watchdog
  on, the counter group holds four events, so that run counted cycles,
  instructions, branch misses and lines from memory only. Per get, std
  against bbtree: lines from memory 10.1 / 13.9 / 45.2 / 45.1 against
  4.0 / 4.7 / 6.9 / 13.8; instructions 409 / 1,219 / 1,465 / 1,468
  against 312 / 322 / 411 / 690, each row's end saying how many fewer
  bbtree takes as a share of std's, from the values as shown ("23.7%
  fewer"; "1.3× fewer" read wrong, the author, 2026-10-22); branch misses 8.7 / 7.5 / 8.5 / 9.5
  against 0.5 / 0.5 / 1.0 / 4.4 (not in the post).
- **Time to the first answer (2026-10-21).** bbtree-mmap's `first_query`
  example (`cargo run --release -p bbtree-mmap --example first_query`,
  `-C target-cpu=native`, about a second): 10M random `u64` keys and
  values on disk, each restart a process of its own, timed from its start
  to its first lookup's answer, a key from the middle, once with the file
  dropped from the page cache (`posix_fadvise`, on the 9900X's NVMe SSD)
  and once with it read in. std's `BTreeMap` at its best: its entries in
  key order, 16 raw bytes each (160 MB), read whole, then its bulk build
  (`collect`). The `BBTreeMapFile` (185 MB) maps the file and reads its
  pages as the lookup reaches them.
  Warm: std 45.1 ms (read 9.6, rebuild 35.5), bbtree-mmap 18 µs (open 8);
  cold: std 101.5 ms (read 65.4, rebuild 36.1), bbtree-mmap 618 µs (open
  244). The figure plays the restarts ten times slower than they ran, at
  one speed, and rounds the speedups to two digits (one run): 2,500× warm,
  160× cold.
- **One bbtree map (2026-10-17).** The post shows `bbtree::BBTreeMap`,
  the map threads share, as "bbtree" in every tab, one thread included;
  the single-thread `bbtree::unsync::BBTreeMap` runs in none (doc 03,
  D4). Every cell was re-run for it on 2026-10-17, on every machine and in
  every integration.
- **Harness** (bbtree-eval's `bbtree-scaling`): `uuid` (`[u8; 16]`, version 4) and
  `wiki-titles` keys; std's `BTreeMap` and `HashMap` owned by one thread
  (`Owned`, at one thread only); `RwLock<BTreeMap>`;
  lookups half of which miss (`get-half`, `get-half-batch`); `update`;
  a 90/10 lookup/update mix (`mixed`) and a 90/10 scan/update mix
  (`scan-mixed`, 100-entry scans, the ordered maps only); batched inserts
  (`insert-batch`, `insert_many` 16 at a time); scc batches under one
  epoch guard for the batch; the heap's peak while a map is built. Each
  shared map is built by every CPU, but the `RwLock` map, built by one
  thread, as its writers take turns (`WRITERS_TAKE_TURNS`).
- **Best use of each map.** bbtree runs with inline keys (`Auto`
  recognizes all four key types), copies its `u64` values out
  (`get_copied`), and uses its batch APIs (`get_many_copied`, 64 keys a
  call; `insert_many`, 16). artmap (ART) was measured, in both its forms,
  and left out of the post as not a fair comparison; its numbers are in
  the session's notes, not in `vis/data/`. DashMap, a hash map, is shown
  only on the concurrent card, for reference.
- **Charts.** The cards are "Single thread", "Concurrent", "End to End",
  "Memory" and "Architectures", in that order (the author's names); they
  don't turn by themselves. Above them,
  one sentence states the result, computed from the data (bbtree's gets
  against std's on one thread, plain and batched; the systems end to
  end). The map is "bbtree", lowercase, everywhere. The
  wide charts are drawn at their card's width, a unit a pixel, and lay
  themselves out for it: on a phone the line chart names its lines in a
  key under the plot, and the grouped charts put a group a row (on a
  desk, the groups split into rows as even as they go). bbtree wears the
  site's highlighter (`--marker`), rimmed and lined in its dark gold
  (`--marker-strong`, 3.8:1 on the panel); every other map is a dark
  slate.
  - Single thread: the mean time per operation, operations back to back, a
    bar a map from zero, one scale for the four panels (a panel a kind of
    key) ending at the longest bar; Get (half hits, half misses), Insert,
    Update, Scan (100 entries). The maps: bbtree, std's `BTreeMap`, scc
    TreeIndex and concurrent-map (concurrent maps, used from one thread),
    crossbeam's SkipMap. bbtree's batch API is a hollow bar under
    bbtree's own, "batched" said once in the legend. std's `HashMap` is a
    reference for an operation that needn't be ordered: last, below a
    dashed rule, a pale dashed bar, "unordered"; under Scan its row says
    it can't. The note says it stands in for DashMap, the other cards'
    hash map: DashMap is std's hash table (hashbrown's) split into
    shards, each behind a lock; readers mixed the two up (the author,
    2026-10-22). The Concurrent card's note says the same from its side.
  - Concurrent: a line a map, a point at each thread count measured,
    thread counts to scale from zero (a map whose throughput grows with
    its threads draws a straight line from the corner), names at the
    lines' ends, a crosshair; Get, Insert,
    Update, 90/10 get/update, 90/10 scan/update. bbtree's batched lookups
    are a solid line of their own, "bbtree (batched)"; other maps'
    batches aren't shown. DashMap, a hash map threads share, is drawn for
    reference (the author's call, 2026-10-17): a faint, dotted line that
    fades in, "DashMap (unordered, hashtable)" in italics, a dotted swatch
    in the phone's key; no scans, so no line under 90/10 scan/update. Its
    cells come from `blog/runs.sh tab2-extra` (DashMap and bbtree's
    batched inserts, the latter not shown) on the same machine type the
    same day, copied into the card's results before `blog/collect.sh`,
    which keeps DashMap's rows. It is the one card not run on the 9900X, whose 12 cores made a
    cramped chart and whose second hardware threads bent its lines: it
    runs on AWS's c8a.metal-24xl (AMD EPYC 9R45, Zen 5, 96 cores, one
    socket, 12 × 32 MiB L3; AWS's AMD instances run a thread a core), at
    1 thread and every 8 to 96 (`THREADS=1,8,…,96 blog/aws-runs.sh
    c8a.metal-24xl,c8a.24xlarge aws-c8a tab2 <keys>`), about $0.90 on
    spot. 10M entries still measure memory there: no map fits the 32 MiB
    L3 of an 8-core group, as on the 9900X.
  - Memory: a bar a map, what its entries cost the machine, its keys'
    allocations included: the growth of the resident set once built, and
    lighter on to its peak while it was built, what a machine must have
    to build it; DashMap, a hash map, last, set apart by a dashed rule and
    an "unordered, hashtable" note, as std's hash map is on the one-thread
    card. The resident set, not the bytes asked for, because the
    allocator's size classes hide unequal costs: mimalloc gives std's
    192 B `u64` leaf a 224 B block (+21% resident), bbtree's 1 KiB node
    exactly 1 KiB. bbtree-eval's `blog/runs.sh memory` runs one process a
    map, holding nothing else: its keys are fed to the map as it is built
    (integers made from their indexes, strings streamed from a dump a
    child process makes), so no key set or dataset load shares the
    resident set. The peak is `VmHWM`'s growth, mimalloc and THP as the
    machine has them; once built is `VmRSS`'s growth after `mi_collect`
    hands the free pages back. An empty build grows it by 0.7 MB. DashMap
    peaks 30–70% above what it keeps (old shard tables freed but still
    resident); every map built on 24 threads peaks about 5 bytes an entry
    above on string keys, about 2 MiB a worker that only THP adds (off,
    the gap is gone; not the stacks), which std's one-thread build
    doesn't pay.
  - End to End: drawn as `end-to-end.svg` (the LSM-trees, Case 1) and
    `database-index.svg` (Turso, Case 2) is (the author's call,
    2026-10-22): a row a workload, grouped by system under its
    name and the map it ships; the time as shipped a dark bar at 100%,
    bbtree's a bright one inside it, as a share of it, so that no
    system's times compare with another's; the speedup at the row's
    end. On a phone the same rows, the bars narrower. The note names the
    cells that went against bbtree.
  - Architectures (no title, no machines table, by the author): grouped bars (`baseline_bars.rs`),
    two bars a machine, std's BTreeMap and bbtree side by side on one
    axis, each labelled with its time: UUID gets (the author's pick of
    workload), each machine captioned with its instruction set. Each
    machine's bench is built for its own CPU (`target-cpu=native` there,
    or its name when built elsewhere: pittsburgh's `rocketlake`). The
    machines: the Ryzen 9 9900X, pittsburgh's Core i5-11400, the M1 Pro,
    and AWS's C8i (Xeon 6) and C9g (Graviton5); the note names the AWS
    instances. Cells come from `blog/runs.sh arch` (UUIDs unless
    other keys are named; Linux or macOS), merged by `blog/collect.sh`;
    on AWS, `blog/aws-runs.sh <types> <name>` runs them on a
    throwaway spot instance (c8i.large and c9g.large, 2 vCPUs, about a
    cent a run in us-west-2) and terminates it, the instance also
    shutting itself down two hours after boot. A machine joins the card in
    `ArchitectureMachine::ALL`. WebAssembly and the ESP32-P4 are left out:
    WebAssembly until Rust builds wasm32 with SIMD (`simd128`) by default
    (its harness is bbtree-eval's `wasm/`, on keys from `bbtree-scaling
    --dump-keys`).
  - Motion: bars grow from their baseline at one speed, each taking the
    time its length takes on its chart's scale (`LinearScale::growth_ms`,
    at most 0.8 s), so that the faster map's bar lands first, the chart
    acting out the time it measures; a value fades in as its bar lands;
    the concurrent lines draw left to right, as threads are added. It
    waits until the cards are first in view, replays when a toggle
    changes what a chart shows (keyed remounts), and with reduced motion
    bars and lines appear whole while values still fade in.
  - Every chart's note ends with links to the code of the maps and
    systems outside std (Dioxus has no SVG link element, so the labels
    themselves aren't links). Notes are kept short, at most 75
    characters a line.
- **Latency was dropped.** Each operation timed alone, fenced (a bare
  `rdtsc` read the end before a lookup's misses returned), takes 20–25 ns
  of fences more and can't overlap the next: bbtree 240 ns alone against
  109 back to back, std 281 and 277. That measured the timer as much as
  the map; the post shows the mean back to back.
- **What the numbers say (Ryzen 9 9900X, 10M entries, 2026-10-17).**
  - One thread, ns per operation, bbtree against std's `BTreeMap`
    (lookups half missing; batched in brackets):

    | keys | get | insert | update | 100-entry scan |
    |---|--:|--:|--:|--:|
    | `u64` | 109 (74) vs 264 | 155 (113) vs 239 | 117 vs 273 | 619 vs 1,647 |
    | UUID | 120 (85) vs 335 | 172 (130) vs 357 | 144 vs 366 | 773 vs 1,764 |
    | Wikipedia titles | 400 (294) vs 693 | 417 (324) vs 613 | 439 vs 702 | 1,248 vs 2,128 |
    | JSON keys | 162 (102) vs 666 | 244 (157) vs 572 | 197 vs 676 | 1,029 vs 2,101 |

    Gets 1.7–4.1× std's, batched 2.4–6.6×. The single-thread map of the
    first round was 2–16% faster than these on gets and 16–22% on
    batches (09-concurrency.md, Performance): the price of sharing,
    which this round cut (09, Lookups, Batches).
  - Threads (AWS c8a.metal-24xl, 96 cores, 2026-10-17): bbtree leads
    all 260 cells, every key, workload and thread count. At 96 threads
    against scc TreeIndex, the next map in each: lookups half missing
    3.9 / 4.5 / 2.9 / 6.9× (`u64`, UUID, titles, JSON; `u64` 627 M/s
    against 161, batched 846), updates 3.5 / 3.9 / 2.8 / 5.4×, the 90/10
    lookup/update mix 3.3 / 3.8 / 2.6 / 5.2×, the 90/10 scan/update mix
    2.0 / 1.4 / 1.3 / 1.4×, inserts 1.2 / 1.4 / 1.4 / 1.7× (a 10M build
    at 96 threads takes about 70 ms, the least precise cell). bbtree's
    lookups at 96 threads are 86–94% of 96 times its one-thread rate. The
    `RwLock` map's updates fall from 3.0 M/s on one thread to 1.2 at 96.
    On the 9900X at 24 threads, scc passed bbtree on the titles'
    scan/update mix (6.5 against 6.1 M ops/s); on the c8a's 12 memory
    channels bbtree leads it, 34 against 26.
  - Architectures, UUID gets, bbtree against std's BTreeMap: Ryzen 9
    9900X 2.8×; pittsburgh's Core i5-11400 2.8× (8.72 against 3.08 M gets
    a second); Apple M1 Pro 2.3× (5.08 against 2.18); AWS C8i (Xeon 6975P-C)
    2.6× (4.72 against 1.83), C9g (Graviton5, Neoverse V3) 2.0× (6.01
    against 3.01); about a cent for both AWS runs, on spot. This C8i
    instance ran std 25% slower than the first round's, so its absolute
    times don't compare across rounds. All four keys on pittsburgh and the
    M1 too (bbtree-eval's `blog/results/*-20261017`): pittsburgh 2.6/2.8/1.8/4.5×,
    M1 1.8/2.3/1.8/4.4× (`u64`, UUID, titles, JSON). WebAssembly at its
    default features, measured in the first round on the single-thread
    map, is the exception: 0.45/0.66/1.18/1.20×, its fixed-size-key gets
    1.5–2.2× slower than std's, as without SIMD bbtree counts a node's
    inline keys one at a time; with `+simd128` 1.04/1.27/1.82/2.44× (raw
    runs in bbtree-eval's `blog/results/wasm*-pittsburgh-20261016`; neither is in
    the post). A wasm SIMD count, like the NEON one (D56), is the fix to
    consider.
  - Hash maps, for reference: std's `HashMap` takes 23 ns a `u64` get
    (bbtree 109, 74 batched), 43 an insert (155).
  - Not in the post: artmap (ART) tied or beat the first round's single
    lookups and inserts of fixed-size keys at 1.2–2× the memory.
  - Memory, resident bytes per entry once built (2026-10-18, the 9900X's
    `reruns/memory-<keys>-<map>`): bbtree 27.1 / 39.9 / 88.9 / 72.2
    (`u64`, UUID, titles, JSON), std 31.5 / 43.8 / 81.0 / 65.2, DashMap
    31.3 / 45.4 / 89.1 / 72.6; peaks bbtree 27.2 / 40.0 / 93.9 / 77.1, std
    31.5 / 43.9 / 81.2 / 65.5, DashMap 52.7 / 73.1 / 115.2 / 101.6.
    bbtree is the least of every map on `u64` and UUID, 10–11% above std on
    strings once built (asked for: 13–16%).
- **Integrations**, single thread only (with more, each system is bound by
  its own locks): bbtree-eval's `integrations/{fjall,slatedb,turso,tonbo,lance}`,
  the shipped skiplist against bbtree, re-run on 2026-10-17 with the
  renamed library (Lance on 2026-10-19, below), each with its RESULTS.md.
  Only the map changed, and where a system writes a batch, the call that
  hands it to the map whole (`insert_many`: fjall's `WriteBatch::commit`,
  SlateDB's `write_entries_to_memtable`; `update_or_insert_many`: Lance's
  put batches). Turso keeps two of
  PR #9267's call-site changes the map needs.
  - fjall: gets 2.47×, mix 2.44×, inserts 1.61×, write batches 3.00×
    (1.5× over single inserts); fjall and Turso at 1M rows.
  - Turso: inserts 2.22×, indexed SELECT 1.90×, point SELECT 1.16×,
    update 1.22×; its values are `Arc`s and its writes single, so none of
    the new APIs apply. Its two allocation-failure tests still fail: no
    fallible insert.
  - SlateDB: gets 1.71×, puts 1.65×, 80/20 mix 1.37×, write batches of 10
    rows 2.36× (2.06× with single inserts). `memtable_seek` 0.91–0.99×,
    from 0.80×: SlateDB's iterators read three rows ahead of a seek, and
    the adapter's first batch of two made every seek scan twice; its
    seek batch is now four rows.
  - Tonbo (crossbeam `SkipMap` memtable, Rust 1.90): 10M-row writes
    6.83×, conflict-checked transactions 4.10×, `write_heavy_no_sst_sweep`
    2.12× at 1M-row memtables (a background compaction lands inside the
    timed window or not; 2.12× is the comparable mode; measured on the
    build before the scan fix below). `insert_many` doesn't help it: each
    row's existence probe has loaded the leaf. Its seal drains with
    `into_iter` now. Not in the chart: point reads that walk the whole
    memtable in scan planning, 10.8–11.8×; this round's library first
    walked it 25% slower, entry loads waiting on each leaf's header, found
    by its integration and fixed in `scan.rs` (09-concurrency.md, Scans);
    `Transaction::get` 0.86× (it copies every row per call).
  - Lance (MemWAL index, its own arena skiplist, PR #7011; Rust 1.98): at
    10M rows, point lookups through its API 2.19×, `lookup_many` 3.94×,
    batched puts 4.22×, updates 3.40×, with its `newest` index a plain
    `u64` word, read by `get_copied`, and each put one `update_or_insert`,
    batches `update_or_insert_many`, which no longer look the values up
    first. Re-run alone on 2026-10-19 (bbtree `f404337`), not the
    library the other systems ran: puts 3.94× and updates 3.29× on
    2026-10-17, lookups 2.28× and 4.03×. Lance's own small benches
    (2026-10-17) and lookups of absent keys (they plan a DataFusion query)
    are unchanged, 0.97–1.06×.
  - **Toolchain.** Before Rust 1.89 the AVX-512 target features are
    unstable and bbtree's search falls back to a scalar count. Systems
    pinned below 1.89 (Turso, Neon: 1.88) run 1.89 with
    `-C target-cpu=native` on both sides, their pin reported too.
    SlateDB's run.sh builds without `target-cpu=native`; with it its rows
    are the same within 2%.
  - **Neon** (pageserver `InMemoryLayer`, std `BTreeMap` → the unsync
    map): stopped during the first round's final runs, not in the chart;
    not re-run.
- **Open.** Not in the post:
  the unsync map's UUID inserts are slower than the shared map's on the
  same tree code (261 against 167 ns), a bug to find.
