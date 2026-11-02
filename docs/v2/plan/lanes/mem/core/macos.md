---
status: plan
summary: mem-core-macos - the macOS driver (kqueue.rs), its FSEvents watches (fsevents.rs) and the footprint sampler (footprint.rs) onto blaze-mem slabs and bounded channels, each warm operation's allocations held to its payload by a test, and each operation's cost against its bound.
date: 2026-10-29
---

# mem-core-macos: the macOS driver onto blaze-mem

Child of [mem-core](../core.md). Paths: `crates/blaze-core/src/kqueue.rs`,
`crates/blaze-core/src/fsevents.rs`, `crates/blaze-core/src/footprint.rs`. Nothing was built or
run; the tests are written beside the code for the coordinator's run.

## What moved where

| Was | Is | Held by |
| --- | --- | --- |
| `owners: FxHashMap<u64, Owner>`, a counted `u64` per registration | `Owners`, a `Slab<Owner>` (`Tag::Io`), the handle's bits as the kevent's `udata` | `owners_answer_as_the_map_they_replaced` (model test, 300 seeds), `an_event_of_a_registration_ended_and_its_slot_reused_is_dropped` |
| `watches`, `spawns`, `listens`, `connecting`, `conns`: maps by tag or counted id | slabs; a small tag-to-handle map where an effect names a tag; `Conn` is a handle's bits | the driver's tests, each ending with every slab empty; stale `Conn`s after a hangup and slot reuse fail |
| a `Box<dyn FnOnce>` per file op on an unbounded channel | the `Effect` itself on a bounded channel (`FILES` deep), past that a backlog; completions back on a second bounded channel | `file_effects_past_the_threads_room_wait_and_all_end`, `a_warm_file_cycle_allocates_only_the_reads_bytes` |
| per-op `CString`s | `realpath` into a stack buffer, paths into reused `Vec<u8>` | the same |
| per stream `Box::into_raw(Box::new(Shared))` and `Box<dyn Fn(Completion)>` | `fsevents::Watch<S: Tell>`, made once and kept (a spare list of 64): its `Shared` in the watch's slab entry, which never moves, as the stream's context; a bounded channel as the sink | `a_warm_watch_batch_allocates_only_its_payload`, `a_warm_watch_add_change_and_remove_allocates_only_the_changes` |
| a batch's names as a `String` each | spans into one reused `String`, sorted and deduplicated before any `FileName` is made | the same |
| footprint: `Vec<Entry>` with a counted id, a linear search per remove | a `Slab<Entry>` under the sampler's mutex | `a_watch_taken_out_names_nothing_once_its_slot_is_another_s` |

Timers were already on the wheel: `a_warm_timer_arm_and_fire_allocates_nothing` holds it.

## Speed of light

Each operation's floor is the kernel's own work; the driver's part is held to a few cache lines
and no allocation.

- **A file op** (Sync, Stat, Read): the syscall, plus one handoff each way. Each handoff is a
  slot of a bounded channel: one cache line moves between cores, about 100 ns. Waking the driver
  takes one `kevent` (an `EVFILT_USER` trigger, about 0.3 us). Its wait returns on the same call
  that collects up to `EVENTS` events. Left: the trigger is sent per completion. Sending it only
  when the completion channel was empty would make it one per batch, which is the bound.
- **A timer**: a wheel insert and a pop, O(1). The next deadline is the `kevent` timeout, so
  there is no syscall of its own.
- **A watch event**: FSEvents' latency (kernel, then `fseventsd`, then the dispatch queue) is
  milliseconds and dominates. Per batch the driver does one unstable sort of spans, one
  `FileName` per distinct name (the payload), and one channel slot.
- **A connection send or receive**: one `send(2)` or `recv(2)`. Registrations are made once per
  connection, so there is no `kevent` change per operation. The received `Vec<u8>` is the payload.

## Caveats

- CoreFoundation and dispatch allocate on their own allocator, which Rust's counter does not
  see. Per stream that is the directory's CFString, a CFArray and the FSEventStream. The queue
  and the cookie CFString are made once per `Watch` and reused.
- Before `blaze-mem`'s `Slab` matches only odd generations, a handle with an even generation can
  name a free slot (request in `v2/requests/mem-core.md`). `conn_id` guards the one place a
  caller's bits become a handle.
- `fsevents.rs` keeps a copy of `FileName::new`'s rule (`file_name_ok`) so it can check a name
  without allocating. A test holds the two in agreement (optional request for
  `FileName::is_valid` in `v2/requests/mem-core.md`).
