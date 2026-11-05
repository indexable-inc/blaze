---
status: plan
summary: mem-core-windows - the Windows driver (iocp.rs and iocp/) onto blaze-mem slabs, inline OVERLAPPEDs, kernel buffers in one reserved region and bounded channels, each warm operation's allocations held to its payload by a test, and each operation's cost against its bound.
date: 2026-10-29
---

# mem-core-windows: the Windows driver onto blaze-mem

Child of [mem-core](../core.md). Paths: `crates/blaze-core/src/iocp.rs`,
`crates/blaze-core/src/iocp/`. Nothing was built or run; the tests are written beside the code
for the coordinator's run, and need Windows (the module is `#[cfg(windows)]`).

## What moved where

| Was | Is | Held by |
| --- | --- | --- |
| `requests: FxHashMap<u64, Request>`, a counted `u64` id, a `Box<Slot>` (`OVERLAPPED` + id) per request | `Slab<Request>` (`Tag::Io`), the `OVERLAPPED` inline in the entry (`sys::Head`: `UnsafeCell`, `PhantomPinned`), its id the handle's bits; every step asked for in place, the entry let go only once the last step's packet is taken | `requests_agree_with_the_map_they_replace` (model test, 64 seeds), `a_packet_of_a_request_ended_and_its_slot_reused_is_stale` |
| a packet's id looked up with `expect` | `Iocp::owner`: the handle read through the packet's `OVERLAPPED`, looked up and its head's address compared; a miss is stale, dropped and counted (`Iocp::stale`) | the same |
| `readers`/`appenders: FxHashMap<FileName, Arc<OwnedHandle>>`, an `Arc` clone per request, a fresh `Arc` per write at an offset | `Slab<Cached>` (handle, count of requests on it, whether named), name maps of handles reserved to `CACHED`; a handle closes once unnamed and unused | `a_warm_sync_stat_and_read_allocate_only_the_read`, `files_end_as_on_threads` |
| `vec![0; size]` per read, zeroed, then filled | `Vec::with_capacity(size)`: the kernel writes its spare room, bounded by the length wanted, no zeroing pass | `a_warm_sync_stat_and_read_allocate_only_the_read`, `files_end_as_on_threads` |
| a `PathBuf` joined per cached lookup, a wide `Vec<u16>` per open | the name looked up first; on a miss the wide path made in `sys::Paths`' kept buffers | the same |
| `vec![file.clone()]` per write, rename, clone, link or remove, to forget handles | the effect's names forgotten in place | the same |
| `spawns: FxHashMap<Tag, Spawned>` + `serials: FxHashMap<u64, Tag>` | `Slab<Spawned>` + `spawning: FxHashMap<Tag, Handle>`; the exit packet and the job's key carry the handle's bits (a live generation is odd, so a key is at least 2^32, above every fixed key) | `processes_end_with_their_output_code_and_kill` |
| `Box::into_raw(Box::new(Waiter))` per process wait | `process::Waiter` inline in the spawn's entry; the wait is unregistered (`UnregisterWaitEx`, waiting out a call) before the spawn leaves its entry (`Iocp::end_wait`) | the same |
| a 64 KiB `Box<[u8]>` per process output, per watch | `sys::Buffers`: 64 KiB buffers in one reserved `Region`, committed as made, a free list sized at setup | `a_warm_watch_allocates_only_its_changes` |
| the input bytes cloned from the command | moved out of it | the same process test |
| `conns: FxHashMap<Conn, Connection>`, `Arc<OwnedSocket>` per request | `Slab<Connection>`, `Conn` the handle's bits; a hung-up connection names nothing and closes once its one receive and one send in flight end | `round_trip`, `a_warm_connection_sends_and_receives_only_its_bytes` |
| `Op::Recv { flags: Box::new(0) }`, `vec![0; most]` | flags inline (`UnsafeCell`); the kernel writes into the payload `Vec`'s spare room, no zeroing, no copy | the same |
| `Box::new([0; 2 * ADDRESS])` per accept, `Box::new(sockaddr)` per connect | inline in the request (`UnsafeCell` for what the kernel writes) | `a_tcp_round_trip` |
| `mpsc` channels (a node per message) behind a `Mutex` | crossbeam `bounded` both ways, `WORK` deep, a backlog past that; one wake packet per batch (`woken`) | every test that runs a file effect |
| `entries: vec![..; 64]` | an array in the driver | |
| a watch's names made, then sorted and deduplicated | spans into one kept `String`, sorted and deduplicated before any `FileName` is made | `a_warm_watch_allocates_only_its_changes` |
| a `PathBuf` joined per blocking file effect on the threads | two kept per thread | `files_end_as_on_threads` |

Timers were already on the wheel: `a_warm_timer_allocates_nothing` holds the arm and fire to zero.

What a completion carries (`Done::Read`'s `Vec<u8>`, `Done::Changed`'s list and names,
`Done::Exited`'s box and output) is the API's payload; each budget test counts it and holds the
cycle to it. Calls of `crate::threads::run` (a sync, a stat, a listing) allocate on the threads
as that module does; that module is not this lane's.

## Speed of light

Each operation's floor is the kernel's work; the driver's part is a few cache lines and no
allocation.

- **A read of a cached file**: `ReadFile` (one syscall, about 1 to 2 us from the cache) and its
  share of one `GetQueuedCompletionStatusEx`, which takes up to 64 packets. The driver: one
  hash probe of the name (two lines), a slab pop (the entry's ~400 bytes, seven lines written),
  the `OVERLAPPED` reset, and on the packet one read of the id, one slab lookup and the remove.
  About 100 ns against 1 us: inside 10%. The one allocation is the bytes, which the caller owns.
- **A sync or stat**: the syscall on a thread, a slot of a bounded channel each way (one line
  moved between cores, about 100 ns each), and a wake packet per batch, not per completion: the
  bound is the syscall plus one post per batch, which this is.
- **A timer**: a wheel insert and pop, O(1); the next deadline is the port's wait, no syscall of
  its own.
- **A watch**: one `ReadDirectoryChangesW` per batch; the records read in place, names joined
  into one kept string, one unstable sort of spans and one dedup, then the payload. The system's
  delivery latency dominates by orders of magnitude.
- **A send and receive**: `WSASend` and `WSARecv`, one syscall each, the packet shared; the
  received bytes land in the payload's own room, so the copy is the kernel's one.
- **A process**: `CreateProcessW` and its job (milliseconds); what the driver makes for it (the
  command line, the environment block, the pipe names) is made once as it starts, microseconds.