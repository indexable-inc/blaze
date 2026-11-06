---
status: plan
summary: Every lock in blaze is a named blaze_core::lock::{Lock, ReadLock, Signal} that records who waited, behind whom and for how long, on the slow path only; raw std and parking_lot locks are banned; the profile ranks locks by wait time (`lock_waits`, `blaze debug locks`) and the gate fails a build where one lock eats a share of worker time.
date: 2026-11-05
---
# Named, instrumented locks

## Why

On 2026-11-05 one hot lock, `blaze_profile::flight::intern`, stalled 60+ threads and the
daemon's turn loop. Nothing said so: origin/main had about 840 lock sites (759 `Mutex`, 61
`RwLock`, 22 `Condvar`), none named, none recording when it was contended. The fix (#1805)
came from a sampler stack, a lead, not a span (`slow-is-p0-traced`).

## What

1. **One lock home**, `crates/blaze-core/src/lock.rs`: `Lock<T>` (mutex), `ReadLock<T>`
   (reader-writer) and `Signal` (condition), each on `parking_lot`, each taking a
   `&'static str` name at construction: `Lock::new("store.packs", v)`. `new` is `const`, so
   statics keep working. There is no `Default`: a lock gets a name where it is made.
2. **Contention is recorded on the slow path only.** The fast path is `parking_lot`'s
   `try_lock` (its one CAS); a release adds one relaxed load of a waiter count on the same
   cache line. When an acquire has to wait:
   - it opens the profiler's `wait` span with `awaits = lock <name>` (`every-wait-says-what`);
   - it reads the injected clock at the start and end (the `Probe`'s: the profiler's
     monotonic clock, or a simulation's virtual one), and the waiter's span, thread and call
     site (`#[track_caller]`);
   - the holder, seeing at its release that someone waits, leaves its span, thread, site and
     hold time in the lock's contention slot (allocated on the lock's first wait), and the
     next waiter reads it;
   - the waiter hands one `LockWait` to `Probe::record`.

   Hold time: a holder that itself waited knows its acquire time, so its hold is whole. A
   holder that took the lock uncontended has no acquire timestamp, because reading a clock on
   every acquire would put about 20 cycles on the fast path. Its hold is counted from when the
   first current waiter began (`hold_whole = false`). Losing alternative: an acquire
   timestamp in every guard (the whole hold, always), rejected because the fast path must
   cost what `parking_lot`'s does.
3. **The seam.** blaze-core cannot depend on blaze-profile, so it defines
   `trait Probe { now_ns, span, thread, wait_begin, wait_end, record }` and
   `lock::install(&'static &'static dyn Probe)`. blaze-profile's `locks::Recorder` is the
   probe. Today `record` writes a `tracing` event (`target: "blaze::lock"`, name `lock_wait`)
   through the flight recorder's event path, with names held as `span::Name::kept`. The
   per-thread buffers (branch `prof-per-thread`) reuse the same ring (`flight::Ring::put`),
   so moving to a dedicated row kind is one line in `Recorder::record`. A lock on the
   recording path itself is `Lock::unrecorded`.
4. **`lock_waits`** is a profile view over those events, one row per wait: `lock`,
   `value_type`, `mode`, `site`, `span`, `thread`, `start_ns`, `wait_ns`, `holder_site`,
   `holder_span`, `holder_thread`, `holder_mode`, `hold_ns`, `hold_whole`.
   `blaze debug locks [--build N] [--top N]` ranks locks by total wait. Each row lists its
   call sites, its top waiter and holder spans, and a recommended replacement read from the
   observed pattern:
   - mostly read waits behind writes: arc-swap or RCU;
   - a map value type: `papaya::HashMap`, or `blaze_hash::HashMap` behind one owner;
   - an integer or bool: an atomic;
   - a `VecDeque` or `Vec` passed between threads: a crossbeam channel;
   - one holder thread: move the value into that owner and send it messages.
5. **Gate check `lock_waits`** in `blaze debug gate`: fails when any one lock's total wait
   exceeds `LOCK_WAIT_MAX_PCT` of the build's worker time. The threshold is 2%, set from the
   one convoy we have measured (flight::intern on 2026-11-05) and the gate's 1% uncovered-time
   budget. Re-pick it from the first ten gate builds' `lock_waits` once this lands.
6. **The ban.** `.clippy.toml` bans `std::sync::{Mutex, RwLock, Condvar}` and their guards, and
   `parking_lot::{Mutex, RwLock, Condvar, ReentrantMutex}` and their guards, each with its
   `blaze_core::lock` replacement. Sites move by `blaze check --replace` (name resolution),
   then each constructor gets a real name. Exempt:
   - vendored code and `library/`;
   - blaze-lints' fixtures, whose subject is the raw types;
   - the crates below blaze-core in the graph (blaze-mem, blaze-hash, blaze-radix,
     blaze-rng, blaze-reclaim, blaze-check), each site with
     `#[expect(clippy::disallowed_types, reason = "below blaze-core: ...")]`.

   blaze-graph's `wait::{lock, read, write}` helpers become redundant and are deleted
   (`one-concept-one-home`).
7. **Hold-time lint** `blaze::lock_held_across_slow`: a `blaze_core::lock` guard (or a raw
   one) live across IO (`std::fs`, `std::net`, `std::process`, `std::io::{Read, Write}`,
   blaze-core effects submitted and awaited) or across a call that allocates on the global
   heap (what `implicit_allocator` flags). Holding one across a fiber park is already
   `held_across_wait`.
8. **DST.** `Sim::stall_holders(one_in, most)` and `Sim::critical_section(work)` stall a lock
   holder mid critical section on the virtual clock. `lock/tests.rs` draws workers, turns,
   schedules and stalls. Its oracle: every turn of the turn loop ends within `TURN_BOUND`,
   and the recorded `LockWait`s name the lock, the turn loop as waiter and the stalled worker
   as holder, with `wait_ns` equal to the virtual wait. The planted bug is a hot lock on the
   turn loop's path, and the oracle must catch it. The control is the fixed design, where
   workers publish a snapshot through an atomic and the turn loop never takes the lock, and
   it must pass every case.

## Cost budget

- Uncontended acquire and release: `parking_lot`'s, plus one relaxed load on release.
- Lock size: `parking_lot`'s plus 32 bytes (name, waiter count, slot pointer). The slot
  (about 64 bytes) is allocated only for a lock that has been contended.
- Guard: 32 bytes, up from 8.

## Proof

- The DST above, with its planted hot-lock control.
- `lock/tests.rs`: a contended `Lock` and `ReadLock` on real threads record exactly one
  `LockWait` per waiting acquire, with the holder's span and site.
- The gate check's fixture `fixtures/gate/lock_waits/`, which it must fail.
