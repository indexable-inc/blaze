---
status: plan
summary: the pump's table of runs (crates/blaze-graph/src/pump/table.rs), brief-locks step 4: why the first lock-free table lost to the sharded one, the bucket table that replaced it, how it is proven, and the microbenches.
date: 2026-11-01
---
# The pump's table of runs

Written by lane one-pool on 2026-11-01, from the code. `Flights` (crates/blaze-graph/src/pump.rs)
runs each key once at a time: the first task to ask runs it, every other one waits for its value.
Its table was 64 shards of a lock and a hash map with waiter lists. It is now
[`pump/table.rs`](../../../../../crates/blaze-graph/src/pump/table.rs).

## The first try, and why it lost

The first table followed the brief: open-addressed, a key CAS-claimed into its slot and kept
there (the tombstone) until a rebuild, every operation entering behind a rebuild gate of
per-thread counters. On [tests-diag's bench](flights.md) (load about 40, 18 cores) it lost at
every point to the sharded table under parking_lot: 29.9 against 13.9 ns at one thread, 1403.9
against 370.9 at 18 threads on keys of their own, 1129.4 against 367.1 on one key. Why:

- Real keys, and the bench's, are asked once each. A table that keeps an ended run's key fills
  with keys nobody runs, and was rebuilt every 2,048 new keys: a scan and a clear of a 512 KiB
  table, stopping every thread at the gate.
- The fast path did seven atomic read-modify-writes against the shards' four: the gate's
  counter in and out on both ask and end, the tag's CAS, a count of used slots that every
  insert of every thread bumped (one line all threads write), and the run's CAS.
- Each new key landed in a slot of the table nobody had touched lately; the shards' hash maps
  reuse the same few buckets, warm in L1.

## Design

- A bucket is one 128-byte line pair: a byte latch, two entries' run words, the entries' keys
  and waiter lists, and a link to a bucket added behind it once both entries were held. A
  key's home bucket's latch guards the whole chain.
- The run word is `scope:32 | waited:1 | task:31`; 0 is a free entry. A new run takes the
  latch, finds no entry of its key, writes the key into a free entry and its run word, and lets
  go. A run nobody waits for ends with one CAS of its run word to 0, taking no latch, and the
  entry is free. A table of N runs under way holds N entries whatever keys came and went:
  nothing is rebuilt, nothing is counted on the fast path. An ask and its end are two
  read-modify-writes of the latch and one CAS, all on the key's one line pair.
- A waiter takes the latch, marks the run waited, pushes itself (a node on its own stack) and
  claims the run's flight in the scheduler if no waiter did yet. An end that finds the waited
  bit takes the latch, takes the list and the flight, frees the entry, then writes each waiter
  its state (DONE with a clone of the value, or GAVE_WAY on a cancel). A waiter that gives way
  unlinks itself under the latch.
- Once the buckets added behind home buckets pass an eighth of the table, it grows to twice the
  buckets: each chain, under its latch, moves to the next table (each run word swapped for
  `MOVED`, which fails its runner's CAS and sends its end to the latch) and is marked moved;
  then the next table is published. An operation that finds its chain moved waits for that.
  The replaced table is kept until the `Flights` is dropped, so no reader or claim is left
  dangling, at no more memory than the table now: no gate, no epoch.

## Where it departs from the brief

- A new run takes its bucket's latch, for a few stores: two askers of one key must not both
  insert it, and without a latch that takes either a duplicate check after the insert, which
  two inserters can both pass, or keys kept until a rebuild, which is what lost above.
- Waiters are under the same latch, not a Treiber stack: a waiter that gives way must unlink
  itself from the middle of the list while the run goes on, which a Treiber stack can only do
  with an ABA-tagged 128-bit CAS, and a waiter takes the scheduler's lock to suspend anyway.
- Values are an `Arc` only where the caller makes them one: each waiter gets a clone of `V`,
  and the main users (blaze-query's `Database` and blaze-codegen's flights) have `V = ()`.

## How it is proven

- Model tests under `--cfg blaze_check` (`pump/model.rs`, two and three threads, tables of one
  bucket so a third run grows them): a waiter gets the run's value or runs the key itself; a
  cancelled run sends its waiter to ask again; a waiter that gives way leaves while the run
  ends; a grow moves a run and its waiter; claims and ends across a grow. Each asserts there is
  never more than one run of a key.
- A Hegel property (`pump/oracle.rs`): random asks, runs if idle, holds, ends and cancels by
  three tasks over three keys, given both to the table and to the sharded design it replaced
  (kept as `pump/sharded.rs`, test-only), give the same answers, the same woken waiters'
  answers and the same count of runs under way at every step.
- The pump tests (`pump.rs`), with two more: 100,000 keys that come and go leave the table as
  it was, and a grow for a thousand runs under way keeps them and their waiters.

## The microbenches

- `crates/blaze-graph/benches/flights.rs` (lane tests-diag): N threads over three layouts; its
  sharded baselines under std's mutex and parking_lot's are in [flights.md](flights.md). On this
  table its "128 shards" and "one shard" layouts are the same, since a key's home bucket mixes
  every key word.
- `bench_flights_against_sharded` in `pump/oracle.rs` (ignored; run it with `--ignored`): the
  table and the sharded design in one process, in nanoseconds a run.

### Numbers

Measured by the coordinator on 2026-11-01 on the 18-core Mac, the tree synced at 04:35,
release builds, the machine's load about 31 to 38 throughout. These are of the bucket table
before the two changes below. No digest of the tree or the binary was kept, so they are a
direction, not evidence: the rerun names its tree, and tests each change's cause by measuring
with it taken out again.

`benches/flights.rs`, back to back, ns a call per thread, best of 5, as 128 shards / one
shard / one key:

| threads | sharded, parking_lot | bucket table |
|---|---|---|
| 1 | 15.8 / 15.4 / 15.2 | 14.8 / 14.4 / 14.0 |
| 2 | 30.5 / 32.5 / 32.3 | 39.2 / 80.0 / 78.6 |
| 4 | 70.9 / 66.1 / 64.6 | 73.9 / 91.2 / 89.6 |
| 8 | 152.5 / 151.3 / 181.1 | 111.2 / 113.5 / 110.5 |
| 16 | 344.1 / 320.4 / 385.1 | 134.0 / 138.0 / 175.3 |
| 18 | 414.5 / 355.6 / 494.7 | 156.3 / 143.5 / 119.4 |

`bench_flights_against_sharded`, ns a run:

| ns a run | bucket table | sharded |
|---|---|---|
| uncontended claim and end | 18.6 | 16.9 |
| 18 threads, one hot key | 39.9 | 43.7 |
| 18 threads, keys of their own | 19.0 | 22.4 |

Uncontended, the two are level. At 8 threads and more the table is 1.4 to 4 times faster. At
two and four threads, in the one-shard and one-key layouts, it was up to 2.5 times slower, so
two changes followed, not measured yet:

- The bucket hash was linear. The bench's keys are numbered, so consecutive keys stepped
  through the buckets in a fixed stride, and two threads a few keys apart met in one bucket
  again and again, the latch's line passing between them on each call. murmur3's finalizer
  now mixes the key's words.
- An asker that finds the key running spins a few rounds (at most 63 pauses, never yielding)
  on the run word, outside the latch, before it marks the run waited. A run that ends within
  that spares both sides the scheduler: the flight's claim, the suspension and the wake.

### Numbers after the two changes

The coordinator's rerun on 2026-11-01, the live tree synced at 04:45 (again no digest kept),
the same Mac, load about 34. The pump tests passed 10 of 10, and the `blaze_check` models of
`pump/model.rs` passed.

`benches/flights.rs`, back to back, as above:

| threads | sharded, parking_lot | bucket table |
|---|---|---|
| 1 | 13.9 / 13.9 / 13.9 | 13.7 / 14.0 / 14.2 |
| 2 | 66.3 / 27.8 / 27.9 | 56.7 / 57.3 / 54.9 |
| 4 | 111.8 / 75.7 / 72.6 | 88.1 / 91.4 / 89.3 |
| 8 | 243.7 / 192.2 / 196.9 | 91.8 / 88.1 / 86.7 |
| 16 | 406.8 / 433.9 / 479.3 | 113.7 / 102.9 / 106.6 |
| 18 | 364.7 / 603.5 / 703.4 | 114.5 / 118.4 / 113.2 |

`bench_flights_against_sharded`, ns a run:

| ns a run | bucket table | sharded |
|---|---|---|
| uncontended claim and end | 15.5 | 15.1 |
| 18 threads, one hot key | 29.6 | 117.2 |
| 18 threads, keys of their own | 15.1 | 30.6 |

Uncontended, the two are level; at 8 to 18 threads the table is 2.6 to 6 times faster, on one
hot key as on keys of their own. At two threads it is still about twice as slow as the
sharded table's one-shard and one-key layouts (55 to 57 against 28). That gap is not
explained yet, and the two-thread numbers don't hold together well enough to explain it.
The table takes the same time in all three layouts, whether its two threads share a bucket
on every call (one key) or never do, so the gap is not latch contention. The sharded table
is faster with both threads on one lock than with them spread over 128, which no contention
explains either. On a machine loaded with about 34 runnable threads, a two-thread best of five
depends on where the two threads land. The next step tests that: the two-thread rows ten
times each on the Mac once it is quiet, the tree's digest kept, for the table as it is,
without the murmur3 mix and without the spin before a wait. macOS has no call that pins a
thread to a core, so the runs are not pinned.

### Two-thread rounds on the Mac

The coordinator's ten interleaved rounds on 2026-11-01 at 05:41 to 05:44 (logs
`/tmp/blaze-lanes/bench-flights-2t-{B,T0,nofmix,nospin}-{1..10}.log`, each naming its tree and
`table.rs`'s hash), the Mac's load falling from 54 to 30. Each round ran four processes: B (the
sharded table under `parking_lot`), T0 (the table as it is), nofmix (without the murmur3 mix),
nospin (without the spin before a wait). The 128-shard column, ns a call.

Every process ran in one of two modes, the same mode at every thread count:

| mode | 1 thread | 2 threads | 8 threads | 16 threads |
|---|---|---|---|---|
| S | 13.6 | about 55 | about 100 | about 110 |
| F | about 16 | about 33 | about 165 | 300 to 370 |

The machine is an M5 Max: 6 cores at its top performance level and 12 at the second, in
clusters of 6, each sharing an L2. Mode F's slower single thread, cheap two-thread handoff and
poor scaling fit a process placed first on the second level's cores, its threads sharing one
L2. That is not proven. Which mode a process gets depends on the time, not on the variant:
rounds 7 to 10 ran mostly in F. B was in S 6 times of 10, T0 5 (round 5 mixed: S at two
threads, F-like at 16), nofmix 5, nospin 8.

The medians over all ten rounds therefore compare how many of each variant's processes
landed in S. Within one mode the variants are level:

| median, ns | T0 | nofmix | nospin | B |
|---|---|---|---|---|
| S, 2 threads | 55.1 | 54.9 | 55.3 | 52.3 |
| S, 16 threads | 114.1 | 111.9 | 116.7 | 260 |
| S, 18 threads | 119.6 | 114.7 | 122.0 | 282 |
| F, 2 threads | 34.3 | 35.9 | (2 runs) | 35.4 |
| F, 16 threads | 351.8 | 317.2 | (2 runs) | 370 |

So neither the spin nor the mix shows an effect at any thread count; both stay as they are,
the spin capped at 63 pauses and the mix for keys that step through the buckets. At two
threads the table and the sharded one are level within a mode (55 against 52, 34 against
35): the earlier factor of two came from the processes' modes, not the table. At 16 and 18
threads in S the table is 2.3 times faster.

