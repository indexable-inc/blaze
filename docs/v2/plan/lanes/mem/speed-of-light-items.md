---
status: plan
summary: lane mem's child mem-backend-items - the speed of light of resolving a key, building a crate's key index, hashing a key's preimage and an item's component, and building an index from its rows once on blaze-mem, the arithmetic, how close the design comes, and what it is held against.
date: 2026-10-29
---
# mem-backend-items: speed of light

The machine and the bar are those of `speed-of-light.md` in this directory: a 4 GHz core, an L1
hit 4 to 5 cycles, a DRAM miss about 80 ns, a system call about 1 µs, BLAKE3 about 1 cycle a byte
for one input and 16 inputs at a time with AVX-512. Design: [backend-items.md](backend-items.md).

## Resolving a key (`key::resolve`)

Least work: find the crate (8 bytes of the key against a sorted list of crates, a few hundred
entries: one or two cache lines warm), then find the key in that crate's index. A key is BLAKE3
output past its first 9 bytes, so no hashing is needed: its bits are the hash. The floor for a
cold lookup in an index of `n` rows is one miss for where the key would be and one for the row.

The design: the directory has a bucket per row (`bits = ceil(log2 n)`), so the lookup reads one
directory word (`starts[b]`, `starts[b + 1]`, adjacent) and scans a run of one row on average:
two dependent misses cold, about 160 ns, at the floor. Before, a `HashMap` probe was the same two
misses plus a SipHash-free fold of the key (`KeyHashing`) and control-byte matching, so the
lookup itself is as fast as it was; the win is in memory (40 to 56 bytes a row and 4 bytes a
bucket, against a map's 1/0.875 load factor and control bytes) and in building. Allocations:
none (tested).

## Building a crate's key index

Least work: write each row once and sort `n` rows by a 64-bit key, `O(n log n)` compares on
contiguous memory, then one pass for the directory.

The design writes each row once into a region committed in steps of a quarter of what is
committed (never a copy: a `Vec` doubling copies every row about once more, and a `HashMap`
rehashes every row at each doubling), sorts in place with `sort_unstable_by_key` on
`(order, key)`, which is almost always decided by the first word, and makes the directory in one
pass: one heap allocation an index. The commits are about `log_{1.25}(n / granule)` system calls,
some 20 for a crate of 100 000 rows, 20 µs against milliseconds of walking the def map. Within a
constant of the floor; a radix sort on `order` (`blaze_ids::radix`) would save the compares if
profiles show the sort.

## A key's preimage

Least work: write the preimage's bytes (a parent key, a name, a few tags: under 100 bytes) and
hash them. The design writes them into the thread's scratch buffer, warm after the first key, and
hashes one key (`hashed`) or a batch of 16 (`Batch`, `ContentId::of_many`), so a warm key
allocates nothing; a batch allocates only the rows the query keeps. Before, each key built a
fresh `Vec<u8>`: one malloc and free a key, 40 to 100 cycles, more than the hash of a short key.

## An item's component hash

Least work for an item that is its own component (nearly all): read its forms (memo hits), fill
each ref (a memo hit a ref) and hash `8 + 32 + 8 + 33 * refs` bytes. The design streams exactly
that into one `Hasher` (`hash::alone`): no list, no buffer. Before, it built four `Vec`s and a
byte buffer: five mallocs an item, about as much as the hashing for an item with a few refs.
Tarjan's search still runs first, its stacks in a frame (no malloc) and its index in an
`FxHashMap` (one malloc a search); a component of more than one item is spelled from heap lists
because their fills recurse, and colored in a frame.

## Building an index from rows

Least work: write the rows once, sort them, write each column once at its length. The design
writes the rows into the query's frame (a bump, no malloc; growth past the frame's top copies
only when two lists interleave), sorts in place, counts runs before allocating, and makes each
column with one exact allocation (`table::column`): one malloc a column, the floor for a value
salsa keeps. Before: a doubling `Vec` for the rows, `collect` into each column (exact for most,
but an `unzip` and a run list grew by doubling).
