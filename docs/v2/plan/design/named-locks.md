---
status: current
summary: Every lock in blaze is a named blaze_core::lock::{Lock, ReadLock, Signal} on blaze's own word locks, recording who waited, behind whom and for how long, on the slow path only; raw std and parking_lot locks are banned in .clippy.toml; each wait is an event the profile's `lock_waits` view reads, and the gate fails a build where one lock's waits pass 2% of worker time. When a lock may exist at all is lock-policy.md's.
---
# Named, instrumented locks

## Why

One hot lock (`blaze_profile::flight::intern`, 2026-11-05) once stalled 60+ threads, and nothing
said so: no lock was named, and none recorded when it was contended. A lock that costs time must
show up as a row naming it, its waiter and its holder.

## What

1. **One lock home**, `crates/blaze-core/src/lock.rs`: `Lock<T>` (mutex), `ReadLock<T>`
   (reader-writer) and `Signal` (condition), built on blaze's own word locks (`lock::raw`: a futex
   word on Linux, the wait on an address on macOS). Each takes a `&'static str` name where it is
   made (`Lock::new("store.packs", v)`). `new` is `const`, so statics work. There is no `Default`.
2. **Contention is recorded on the slow path only.** An uncontended acquire is the raw lock's one
   compare-and-swap plus the owner stamp. A release adds one relaxed load of the waiter count.
   When an acquire has to wait:
   - it opens the profiler's `wait` span naming the lock (`every-wait-says-what`);
   - it reads the probe's clock at the start and end, with the waiter's span, thread and call
     site (`#[track_caller]`);
   - the holder, seeing at its release that someone waits, leaves its span, thread, site and hold
     time in the lock's contention slot, which is allocated on the lock's first wait;
   - the waiter hands one `LockWait` to `Probe::record`, and the lock's name and wait go to
     `blaze_meter::lock_waited` with or without a probe.

   A holder that took the lock without waiting has no acquire time, because reading a clock on
   every acquire would cost the fast path. Its hold is counted from when the first current waiter
   began (`Held::whole` is false).
3. **The seam.** blaze-core cannot depend on blaze-profile, so it defines `trait Probe` and
   `lock::install`. blaze-profile's `locks::Recorder` is the probe. It writes each wait as a
   `blaze_trace` event of target `blaze::lock`. A simulation hands a lock its own probe
   (`Lock::probed`), and `Lock::enqueue` splits the slow path into two halves that never block. A
   lock on the recording path itself is `Lock::unrecorded`.
4. **`lock_waits`** (`crates/blaze-profile/sql/views.sql`) is a view over those events, one row
   per wait: the lock, value type, mode, site, span, thread, start, wait, and the holder's site,
   span, thread, mode and hold. `locks::rank` orders locks by total wait and names a replacement
   from the observed pattern.
5. **The gate** (`crates/blaze-profile/src/gate.rs`) fails when any one lock's total wait exceeds
   `LOCK_WAIT_MAX_PCT` (2%) of the build's pool-worker time.
6. **The ban.** `.clippy.toml` bans `std::sync::{Mutex, RwLock, Condvar}` and `parking_lot`'s
   locks and their guards, each naming its `blaze_core::lock` replacement. The crates below
   blaze-core in the graph carry their own `clippy.toml`.
7. **DST.** `Sim::stall_holders` stalls a lock holder mid critical section on the virtual clock.
   `crates/blaze-core/src/lock/tests.rs` draws workers, turns, schedules and stalls, and checks
   that each recorded `LockWait` names the lock, the waiter and the stalled holder with the
   virtual wait. A planted hot lock is the control.

## Cost

- Uncontended acquire: one CAS and one relaxed store. Release: one relaxed store and one relaxed
  load.
- A contention slot exists only for a lock that has been contended.
