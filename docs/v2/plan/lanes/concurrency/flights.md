---
status: record
summary: Flights under contention (crates/blaze-graph/benches/flights.rs, 2026-11-01, busy Mac). parking_lot's sharded map is 1.5x std's uncontended and 3-5x on one lock; the current lock-free table matches parking_lot on one thread and is 3x it at 18 threads; the first open-addressed table was flat at 13 Mops/s (shared counter, rebuilds).
date: 2026-11-01
---
# Flights under contention

`crates/blaze-graph/benches/flights.rs` (`blaze bench -p blaze-graph --bench flights`) runs N
threads, each doing 200 000 `Flights::get_or_run` calls, in three layouts:

- **128 shards**: keys nobody else asks for, spread over 128 shards (every shard on a machine
  with fewer than 32 cores, which has 64). Each call takes its shard's lock twice, to add the run
  and to remove it, and the sharding keeps the threads apart.
- **one shard**: the same keys, all in one shard. Each call is the same two claims, with every
  thread on one lock.
- **one key**: every thread asks for the same keys in the same order, so each key is run by one
  thread and waited for by the others.

Each number is the best of five rounds: the nanoseconds per call a thread sees, and millions of
calls a second over all the threads. One thread is the uncontended cost, which the lock-free
table (lane one-pool, brief step 4) must not lose; the one-shard column against the 128 column is
what contention on one lock costs.

## Numbers

All on the Mac, 18 cores, 2026-11-01, while a self-build and tests ran (load 30 to 43), so each
group compares with itself, not with the others. ns per call a thread sees, best of five; in
brackets, millions of calls a second over all threads. The sharded runs are HEAD d4d8268391 with
the bench pointed at the sharded map (std's `Mutex` with A.patch, `parking_lot`'s with B.patch);
logs are `/tmp/blaze-lanes/bench-flights-*.log`.

### Locks: std against `parking_lot`, sharded map (04:18 to 04:19)

| threads | std 128 shards | std one shard | std one key | `parking_lot` 128 shards | `parking_lot` one shard | `parking_lot` one key |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 20.9 | 18.6 | 19.3 | 13.9 | 13.3 | 13.6 |
| 2 | 59.4 | 56.5 | 58.0 | 68.3 | 35.9 | 35.1 |
| 4 | 128.0 | 257.1 | 238.4 | 131.7 | 99.1 | 70.5 |
| 8 | 216.9 | 934.3 | 932.6 | 181.4 | 133.4 | 161.2 |
| 16 | 450.5 | 1389.1 | 1468.1 | 410.1 | 305.8 | 403.4 |
| 18 | 532.4 | 1713.9 | 1738.3 | 370.9 | 333.5 | 367.1 |

`parking_lot` is 1.5 times as fast with no contention, and 3 to 5 times as fast with every
thread on one shard or one key: std's lock (on macOS, a boxed pthread mutex) falls off
from four threads on one lock, and `parking_lot`'s spin-then-park holds up.

### The lock-free table against `parking_lot`'s sharded map

Each pair ran back to back. The tables are the forms of `pump/table.rs` in the live tree when
each ran.

| threads | 04:19 first table (open-addressed) | 04:34 table | 04:34 `parking_lot` sharded | 04:44 table | 04:44 `parking_lot` sharded |
|--:|--:|--:|--:|--:|--:|
| 1 | 29.9 (33) | 14.8 (68) | 15.8 (63) | 13.7 (73) | 13.9 (72) |
| 2 | 117.2 (17) | 39.2 (51) | 30.5 (66) | 56.7 (35) | 66.3 (30) |
| 4 | 335.6 (12) | 73.9 (54) | 70.9 (56) | 88.1 (45) | 111.8 (36) |
| 8 | 642.0 (12) | 111.2 (72) | 152.5 (52) | 91.8 (87) | 243.7 (33) |
| 16 | 1216.9 (13) | 134.0 (119) | 344.1 (47) | 113.7 (141) | 406.8 (39) |
| 18 | 1403.9 (13) | 156.3 (115) | 414.5 (43) | 114.5 (157) | 364.7 (49) |

These are the 128-shard column; a table's other two columns are the same keys to it (see
"Reading the layouts"). The 04:44 table loses nothing with no contention (13.7 ns against 13.9) and is 3 times
as fast at 18 threads (157 million calls a second against 49), still climbing where the locks
have flattened. The first table, at 04:19 and in the coordinator's 04:10 run, was the slowest of
all: flat at about 13 million calls a second from two threads up.

### Why the first table was flat

Every call of the bench is a key nobody asked for before, and in the first, open-addressed form
of the table:

- every new key added one to the table's `used` counter, one cache line every thread writes;
- a slot kept its key once its run ended, until a rebuild, and the table stayed at most 4096
  slots, so about every 3000 new keys an asker found it full and rebuilt it: the gate closed,
  every thread waited for the operations under way to leave, and the runs moved to a new table.
  At eighteen threads that is about 1200 stops a round.

The current form (buckets of two entries under a home bucket's latch) frees an entry when its
run ends and counts only the buckets added behind a home bucket, so neither is on the path of a
call nobody waits for.

### Reading the layouts

The layouts name the sharded map's shards: the second word of a key picks its shard. The table
places a key by all four words, so its three columns are the same spread of keys and their
ratio says nothing; for it only the thread counts compare. A quiet-machine rerun of the 04:44
pair is still to come.
