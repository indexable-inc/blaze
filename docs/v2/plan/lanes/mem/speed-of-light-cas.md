---
status: plan
summary: lane mem's child mem-backend-cas - the speed of light of the store's lookups, inserts, swaps, flag sets and segment chunks once on blaze-mem regions, the arithmetic, how close the design comes, and what it is held against.
date: 2026-10-29
---
# mem-backend-cas: speed of light

The machine and the bar are those of `speed-of-light.md` in this directory: a 4 GHz core, an L1
hit 4 to 5 cycles, a DRAM miss about 80 ns, an uncontended atomic read-modify-write about 18
cycles (x86-64 `lock`) or 10 to 20 (arm64 LSE), a line moving between cores 50 to 100 ns, a
system call about 1 µs. Design: [backend-cas.md](backend-cas.md).

## A lookup that hits a delta

Least work: hash the key (the first 8 bytes of a 32-byte hash, one load), load the slot, load
the value and compare 32 bytes. Warm, that is three L1 hits and a compare, about 15 cycles; cold,
two dependent DRAM misses (slot, then value), about 160 ns. The design does exactly this per
level probed, oldest level first: a key in level `n` costs `n` extra slot probes, and a level is
made only when the one before is half full, so with the delta sized for what precedes an index
(`DELTA_FIRST`) almost every hit is in level 0 or 1. Allocations: none (tested). Against the
bound: within 10% for level 0; one extra miss a level beyond it. The same as before the move:
the slot and value layouts are what they were, only where the memory comes from changed.

## An insert (a writer's put publishing its spot)

Least work: the probe above to the empty slot, write the 128-byte spot, one CAS on the slot. The
design adds one `fetch_add` on the table's `carved` counter for the value slot, where before it
called `malloc` (a thread cache hit, 20 to 50 cycles, sometimes a central-list refill). One
writer: the `fetch_add` on an owned line is about 18 cycles, under the malloc it replaces. Many
writers at once: `carved`, like `used` beside it (already there before), is a shared line, so
each insert can pay a line transfer (50 to 100 ns). That is the gap to the bound under
contention; the fix is to carve blocks of 64 slots a writer, one shared RMW a 64 inserts
(follow-up in backend-cas.md). Making a level: one `mprotect`/commit of its pages (a system
call, about 1 µs, once per doubling) and the kernel's zeroing at memory bandwidth, which the old
`Box<[AtomicPtr]>` of zeros paid too, plus its `calloc`.

## A swap (a commit pointing a key at its pack) and a collect

Least work: probe, write the new value, one swap on the slot, note the grace. The design takes
a recycled slot (pop a `u32`), writes, swaps, starts a grace (one SeqCst fence and RMW, as the
limbo did) and pushes `(grace, slot)`: no allocation where the limbo boxed each old value and
pushed onto a `Vec`. Collect is a `partition_point` over graces in start order and one
`drop_in_place` and push a value: proportional to what is freed, as the limbo was.

## Flag sets

Unchanged: one relaxed load, and a `fetch_or` only when the bit is clear. The words now sit on
a region committed whole when the index is mapped.

## A segment chunk

Making one: a reservation and a commit (two system calls, about 2 µs) where `vec![0; 1 MB]`
was a `calloc` that, at this size, is an `mmap` too: equal. The bytes are the kernel's zeroed
pages either way. Writes and reads are unchanged copies. Freeing: one `munmap`, as `free` of a
1 MB block was. Recycling regions through a free list would remove both system calls a roll
(follow-up).

## A chunked put

Least work: cut, hash each chunk, LZ4 each new chunk, copy each into the segment, write the list.
Before, it also made the cut list, a ref list, an LZ4 output `Vec` a chunk, a copy of each raw
chunk and the payload `Vec`. Now: none of those (scratch buffers, warm after the first put of a
thread); the LZ4 output is written into a buffer sized by `get_maximum_output_size`, the same
block encoder `compress` uses, so the bytes are identical (tested).

## Held against

rust-analyzer's salsa tables (a `DashMap` of boxed memos, a lock a shard) and RocksDB's memtable
(a skiplist with an arena per memtable, one CAS an insert, nodes never freed until the memtable
is): the delta is the memtable's design without the skiplist's `O(log n)` probes and with values
swapped in place, and like it, nothing is freed until a grace or the table goes.
