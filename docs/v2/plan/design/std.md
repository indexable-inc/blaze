---
status: plan
summary: blaze's own standard library, written from scratch on core and alloc. No ambient authority (main names each capability it uses and every function touches only what it is handed), sans-IO underneath (one Io over io_uring, kqueue, IOCP and the simulation), and every blaze crate moves onto it.
date: 2026-11-10
---
# bstd: blaze's standard library

Rust's `std` lets any function open a file, read the environment, dial a host, read the clock or
start a thread, and nothing in its signature says so. It also bakes in choices blaze has already
replaced: its own locks, its own hasher, a global allocator, blocking calls with no deadline, and
no way to run a program under a simulated world.

`bstd` is blaze's own standard library. It is written from scratch on `core` and `alloc` and does
not use Rust's `std`. Three properties define it:

1. **Capabilities.** `main` names each capability it uses as a parameter (`out: Out`,
   `data: Dir<ReadOnly>`, `api: Net`, ...); a whole `Io` cannot be asked for (D104). A function
   can touch only what it is handed, so its signature is the complete list of what it can do.
2. **Sans-IO.** Every operation is an effect handed to one driver. The same `Io` runs on
   io_uring, kqueue, IOCP and the deterministic simulation, and code above it cannot tell which.
3. **Explicit context and memory.** Every task has a `Cx` that says how long it may run, whether
   it was cancelled, what span it is and where its scratch memory comes from. Nothing allocates
   from a hidden global heap: memory comes from an allocator the code was handed. The same
   goes for every other cost and guarantee: none has a default ("The second rule").
4. **One of each.** One lock, one map, one channel, one clock, one wait, one error shape: the
   ones blaze's rules already name.

Every blaze crate moves onto it, and later it is the `std` of the blaze edition. Built so far: the
driver layer it starts from (`crates/blaze-core`) and S1's interface over it
(`crates/blaze-std`).

```rust
// Each grant is named in the signature (D104): the folder `data`, the variables `env`, the
// network `api` and the clock; `blaze run` refuses to start it without the grants it lacks.
fn main(cx: &Cx, data: Dir<ReadWrite>, env: Env, api: Net, clock: Clock) -> Result<(), Error> {
    let cfg: Dir<ReadOnly> = data.read_only();               // narrow
    let key = StripeKey::from_env(&env.only(&[var!("STRIPE_KEY")]))?;
    let api = api.connect_only(host!("api.stripe.com:443"));   // one host
    let cx = cx.until(clock.after(Duration::from_secs(5)));   // a deadline only shrinks
    charge(&cx, &key, &api, &cfg)
}

// charge cannot open files outside cfg, write, read other variables, dial other hosts,
// run past its deadline, or allocate anywhere but its task's memory.
fn charge(cx: &Cx, key: &StripeKey, api: &Net, cfg: &Dir<ReadOnly>) -> Result<(), Error> {
    let mut body = Vec::new_in(cx.frame());                  // freed when the task ends
    ..
}
```

## End state: efficiency (architect, 2026-11-16)

What each operation should cost, and its floor. What it costs today, with evidence, is in
"Gap" below. "(verify)" marks a platform fact no test here has shown yet (rule 45).

**(a) Files.** The seam stays: ops out, events in ("The boundary" stands). What costs is the
queue, the world's lock (taken four to six times a call; held across the driver's blocking
poll, so a world has about one effect in flight) and macOS's two thread hops, not the `match`
on the backend (about 1 ns).
- macOS: a blocking call (stat, open, pread, close, getattrlistbulk) runs inline on the asking
  thread, so the pool's workers are the blocking thread set. Linux: one io_uring per worker
  ("`Threads`" item 7 stands), with a linked `OPENAT` into a fixed slot, `READ` and `CLOSE`
  (uring/files.rs stands). The simulation keeps its queue: one `Files` seam, two
  instantiations (rule 78), held equal by the drivers' equality tests. The clock is read
  with no lock. A path is text in the thread's path buffer or a `Names` column, never a
  `format!` and an `Arc<str>`. A read lands in memory the caller handed in.
- N small files. Floor: `openat(dirfd, leaf)`, one `pread` of the stamp's size plus one byte,
  `close`: 3 calls a file and no copy (today 6 calls and 2 trips). On Linux a batch is one
  submission. On macOS a batch spreads over the workers, with `F_RDADVISE` before a cold
  batch (verify). `F_NOCACHE` is never set: the page cache is the only cache (rule 51).
- Listing is one level, as columns (`Dir::entries` stands: 1.13x of one `getattrlistbulk`
  pass). `Dir::list`, at any depth, goes.
- A repeat's read list. Floor without a journal: ONE `Effect::Stamp` (bstd `Dir::looks`;
  0.77 us a name, blaze-core `examples/stat_cost.rs`). Floor with a journal: macOS FSEvents
  since the stored event id, then stat only the named paths. The journal is trusted only when
  the volume UUID is the stored one, no event carries `MustScanSubDirs`, `UserDropped`,
  `KernelDropped` or `RootChanged`, the history reached the stored id, and the ids did not go
  back. Anything else is a full sweep (verify: ids survive a reboot; the UUID changes when the
  history is lost). Linux keeps no journal for a process that exits, so there the repeat is
  one `STATX` batch, unless a helper with an expiry keeps an inotify journal. Its absence, or
  `IN_Q_OVERFLOW`, means a full sweep.
- Map or read: a pack is mapped read-only once per process and never unmapped in the run (no
  TLB shootdown across 128 cores), and its records are read in place once it is verified. A
  file read once is `pread` into the task's frame. The line: map what a process reads many
  times, or plain arrays over about 64 KiB. This replaces S1f's "no `mmap`" for packs (a
  decision).

**(b) Memory.** Frames stand (memory.md "Allocators by lifetime"; a bump is 2 ns, the top
block grows in place). A table that grows while other things are allocated after it gets a
region of its own in the frame (`Frame::table` over `RegionVec`): it never copies, leaves no
hole and keeps stable u32 indexes. A refusal is typed (parallel.md items 6 and 7 stand;
blaze-own has the `try_` forms). Data is columns with u32 indexes (blaze-own `Soa`, `Segs`;
blaze-mem `Column`; core's `Listing`, `Names` stand). A path is a u32 id into the request's
path table, with joined text only at the system call. No task copies a path, and no effect
holds an `Arc<str>` or a copy of the bytes it writes: a write borrows them until it completes.

**(c) Tasks.** parallel.md "End state" stands. bstd's pool adds four things. A joiner that is
not a worker helps: it takes injected work, and steals once it has a worker identity. Results
come in completion order and are merged by index. One wake serves a batch, with no mutex per
task. A worker's frame is reset to a marker after each item. Floor: under 200 ns from a spawn
to its result being seen.

**(d) Hashing.** BLAKE3 for every key, content id, name another machine or user sees, and
digest checked after a fetch or a boot. In-process maps need none (foldhash over blaze-swiss
stands, kept scalar on purpose by measurement). Everything persisted uses one digest (rule 30):
no XXH3 beside it. Many small inputs go side by side (`hash_many`: 8 lanes at the baseline, 16
on AVX-512; the outline uses it). A parent's digest takes its children's digests, and an
unchanged stamp means the file is not read, so no byte is hashed twice.

**(e) The record store.** A put is O(1): the row is appended, sorted once at seal, and found
meanwhile through a hash map. The maker writes its record into a per-worker segment that never
moves, and the pack is written by gathered writes. Items that are many make one record a
crate, not a file an item. Packs are mapped (a), and the index is a column of keys searched in
place. A pack is verified once per boot ("Verify once" stands, store.rs `read_pack`). Nothing
is synced on the command path, and a torn pack is never trusted (footer digest plus boot
record stand). One store serves a user's worktrees (store.rs `OWN` stands). Outputs are placed
by `clonefile` or `FICLONE`; ext4 has no clone, so there it is `copy_file_range` (store.md
"Store" stands). Large records stay readable in place: a text table with u32 ids per record,
u8 kind columns, item-relative u32 spans. zstd is used only for cold records fetched over the
network, since it forces a copy.

| System | What blaze matches, or does better by design |
|---|---|
| io_uring (TigerBeetle, glommio, Seastar) | a ring per core, fixed files, linked ops; blaze also stats a column in one op |
| LMDB, git packs; Cap'n Proto, rkyv | immutable sorted packs, mapped and read in place; verified once, with a proof |
| bumpalo, Zig `MultiArrayList`, mimalloc; rayon, Chase-Lev, Tokio | frames, `Soa`, a heap per tag, a growing table that never copies; the joiner helps |
| BLAKE3 `hash_many`, Swiss, F14; Bazel, Buck2, Nix; APFS clones | digests of digests; keys at item grain; outputs cloned |

Floor (estimate, rule 67): a repeat with nothing changed is two `pread`s, a journal query and
mapped packs: under 5 ms on tempo (27.9 to 37 ms today) and under 10 ms on blaze (120 ms).

### Gap (tree of 2026-11-16, by search)

| End state needs | Today, with evidence | Cost | Missing |
|---|---|---|---|
| A put in O(1) | sorted `index.insert` (blaze-cli store.rs `PackStore::put`) | 2.44 s, 114,429 puts | append, then sort at seal |
| The joiner helps, in completion order | `Pool::wait` helps only `Here::Worker` (task/threads.rs) | 48.2 s on the main thread, pool parked | piece 1 |
| A growing table that never copies | frames grow only the top block (blaze-mem frame.rs `grow`, `grow_top`) | 45.8 MB and 4.17 MB abandoned | piece 2 (written, uncompiled) |
| No whole-depth listing | `Dir::list` (bstd fs.rs); callers blaze-cli program.rs `run`, bstd kv.rs, blobs.rs | 64 MiB overflow in 4 to 21 s | callers moved to `entries`, then a ban |
| A read list stamped in one effect | `Follow::follow` makes one `Dir::stat` a path (blaze-cli sweep.rs) | tempo 8.8 ms (11.9 on 18 workers) | `Dir::looks` (written, uncompiled); the sweep's switch |
| IO in parallel | `Io::poll` holds the world's lock across the driver's blocking poll (bstd io.rs) | is the 8.8 against 11.9 ms above | inline blocking calls (macOS), a ring per worker (Linux) |
| A clock read without the lock | `Io::now` locks the world (bstd io.rs) | not measured: 10^6 `Clock::now` on 1 and 18 threads | the native clock held outside the lock |
| A source read in one trip | `Dir::read_all` = `found` (a stat) + `ReadAll` (std's read by path; core threads.rs `run_kept`) | not measured: 2,000 `read_all`s against `openat`+`pread` | `Dir::read_sized`: one `Read` of len+1 |
| A record read without an effect | `get_into` = `read_at` = `Effect::Read` (open by path, `pread`, Vec, copy; core fs.rs `read`) | not measured: the profile's count of gets times their ns | packs mapped once; the index read in place |
| The pack index read in place | `read_pack` parses every row into a Vec at each open (store.rs) | not measured: open time against row count | binary search over the mapped key column |
| Writes that borrow | `append_bytes` calls `bytes.to_vec()` (bstd fs.rs) | not measured: bytes copied a command | an op body borrowed until its completion (the `Pending` form) |
| A repeat that stats nothing | no journal on the command path (`Effect::Journal` exists, used only by blaze-vfs) | part of the 27.9 to 120 ms repeat | the sweep asks FSEvents since its stored id |
| Outline digests at the floor | digest.rs batches already | 74 ms against 25 ms | the profile's split |
| Compact expansions | 22 columns, about 400 B each, a call | 321 MB, 128,546 calls | per-record text tables, u8 kinds |

### Pieces, in build order (one writer each; A = can be written at the same time as the others marked A)

1. **A. The pool's join** (writer ab1a42b2898694bd9, written, uncompiled: task/help.rs,
   `Pool::lend`, `Tally::joins`). A non-worker joiner runs injected tasks of its class. A batch
   join reads a count of ended tasks and merges by index. Saves up to the 48.2 s of in-order
   join. Test: `a_batch_join_merged_by_key_gives_equal_bytes_in_any_schedule`, with its plant.
2. **A. The frame's growing table** (writer a18ba224587a1dab7, written, uncompiled:
   blaze-mem frame/table.rs, frame/grow.rs). Saves the 45.8 MB and the 4.17 MB. Test: 10^6
   interleaved pushes, 0 copies, within 1.25x. Left: the profile's row (DRIVER), and on macOS
   reserving in 4 GiB segments.
3. **A. `PackStore::put` in O(1)** (writer a5aadb06ee11bb5be). Saves the 2.44 s. Test: the
   bytes moved by 200,000 puts are linear; the pack's bytes are unchanged.
4. **A. `Dir::looks`** (this architect, written: bstd fs/looks.rs, tests/looks.rs). Then the
   sweep uses it (DRIVER: sweep.rs `Follow::follow`). Saves tempo's 8.8 ms, down to about 1 ms
   (0.77 us a name; tempo's path count is to be printed). Test: one effect, each look the same
   as its stat.
5. **A. `Dir::read_sized(cx, path, len, into)`.** One `Read` of len+1, no stat before it; a file
   that grew is refused by name. Saves one trip and three calls a file read. Test: the effect
   count, and a file that grew between its stamp and its read.
6. **A. `Io::now` without the lock.** The native clock is held beside the lock; the simulation
   keeps its virtual clock under it. Test: the simulated clock still advances only by
   `idle_until`, and the native reads need no lock (counted).
7. **Inline blocking file calls** (blaze-core kqueue.rs, bstd io.rs `call`), after 6: a native
   `perform_now(effect) -> Option<Done>` for blocking calls, asked before `submit`. Test: the
   drivers' equality tests, and the sweep on 1 and 18 workers (18 must not be slower).
8. **Packs mapped once, the index in place** (store.rs), after 3: `get_into` reads from the
   map. Test: the `StorePlant` simulations, and 0 effects a get.
9. **A. `Dir::list` gone**: kv.rs and blobs.rs move to `entries` with a work list, program.rs
   follows the loader; then a `.clippy.toml` ban. Test: kv's simulation tests unchanged.
10. **The journal on a repeat** (sweep.rs, macOS), after 4: `Effect::Journal` since the stored
    cursor, and every distrust condition above falls back to the full `looks`. Test: the
    simulation drops a planted event, and the repeat must still see the change.
11. **A. Writes that borrow** (bstd fs.rs `append_bytes`, `write_at`): the `Pending` form. Test:
    the copy count is 0.
12. **Paths as ids** (cross-crate, one batch): `FileName` out of the hot effects.
13. **Compact expansions** (EXPANSION lane): text tables and u8 kinds.
14. **The outline's digest split** (OUTLINE lane): profile first.
15. **The pool as one batch** (task/threads.rs, after 1). A joiner adopts a worker identity so
    it can steal. A ring per scope replaces the count's sweep, behind `Joining`. `current`
    becomes an `AtomicPtr`, and the pinned queue is checked through an atomic count. Test: the
    pool's interleaving model, plus a planted lost wake.

Decisions wanted:
- core's `Stamp` gains `dev` (40 to 48 bytes; one blaze-core batch), so that a look equals a
  stat in full. Recommended: yes.
- packs are mapped (replacing S1f's "no `mmap`" for packs). Recommended: yes.
- on Linux, a helper with an expiry keeps the journal. Recommended: later, after piece 4
  on io_uring.

## The one rule

**With no capability, code is pure computation.** A function that is handed nothing can take its
arguments, compute, and return. It cannot read or write a file, a socket, the clock, the
environment, a device or another process; it cannot wait; it cannot allocate from a heap it was
not given; it cannot keep state between calls. Everything else in this document is a way of
handing something over, and every such thing is a value in a parameter list.

## The second rule

**A cost or a guarantee the caller should decide is an argument or a type, never a default.**
The first rule says what code may touch; this one says what touching it costs and promises.
Whatever waits on a device, allocates, copies in proportion to its data, can lose data or picks
a policy says so where the call is written: in a parameter that has no default, or in the type
of what goes in or comes out. Decided 2026-11-10 (the Files section below is where it was
learned). How it is applied:

- **A type that makes the wrong call unwritable** comes first (`clone_to` exists on `Flushed`
  alone; a `Buffered` writer cannot be dropped with bytes it holds); then **a required
  argument** (`Durable`, a frame's size, a `Wire`); a doc comment is never enough.
- **A policy that changes an effect is a different function, not an argument.** An effect
  cannot depend on a run-time value, so where a choice decides whether a call can block,
  allocate or panic, each side is its own function with its effect static: `publish_unsynced`
  never waits for the device and `publish_synced` does. The form that takes the policy as a
  value (`publish(.., durable)`) is only a match over the two, for a caller that holds the
  policy as data, and has the larger effect.
- **Two costs are two functions, or one whose result says which was paid**: `clone_to` and
  `copy_to`; `Published::how`; `Pushed::{Held, Wrote}`.
- **A copy is named one** (`copy_to`, `copy_of`, `to_owned_in(alloc)`), and what does not copy
  is `as_*` or a view. What allocates takes the allocator or returns a type that names it
  (`Vec<T, &Frame>`, `Bytes<'a>`, `Pinned`).
- **All or an error is named `_all`** (`send_all`, `write_all`); what may do less returns how
  much. Nothing truncates, saturates or answers a question that was not asked: a call that
  cannot answer returns why (`Result`), or `None` only where "nothing there" is the whole
  story.
- **A bound bstd must pick** (how long a dropped value waits for its op, how many polls a
  stalled driver gets) is a named `pub const`, quoted in the typed outcome that it ends in.
  A bound the caller could pick is an argument.
- **Until records land** ([edition](edition.md), change 4) options are plain required
  parameters and small structs with no `Default`. With records, the fields that are a cost or
  a guarantee stay required (a record field with no default); only true conveniences get one.
  "What each API makes explicit" lists which signatures change shape then.

What bstd found it was deciding for its callers, and what each became, is the table of "What
each API makes explicit".

## Layers

Each layer uses only the ones below it.

| layer | module | what it is |
|---|---|---|
| 4 | `bstd::{fs, net, env, clock, rng, task, process, out}` | the capability types people use |
| 3 | `bstd::io` | `Io`: one handle to the process's driver; turns a call into an effect and parks the caller until its completion |
| 2 | `bstd::driver` | `Effect`, `Completion`, `Driver`, and the drivers: `Uring`, `Kqueue`, `Iocp`, `Sim` (today's `blaze-core`) |
| 1 | `bstd::sys` | raw system calls per target, the only home of `unsafe`; Linux by syscall number with no libc, macOS through libSystem, Windows through ntdll and kernel32 ([sys](sys.md); today the crate `crates/blaze-sys`) |
| 0 | `core`, `alloc` | the language's own libraries, unchanged |

## Two builders

While blaze cannot build itself (its item layer is being rebuilt: [bottom](bottom.md)), cargo
with the pinned nightly builds bstd, and blaze must still read the same source. So bstd is plain
Rust that both compile, plus what needs blaze's extensions under `cfg(blaze)`, the cfg blaze sets
for every crate it compiles (Andrew, 2026-11-15: a simplified bstd rustc can use, and blaze's
bstd that plus the custom parts). Each use of an extension takes the first of:

1. **A form both read**, kept in place: a tool attribute (`#[blaze::pure]`, `#[blaze::linear]`,
   ...), an attribute under `cfg_attr(blaze, ..)`, or nightly's own spelling where blaze lowers
   it as nightly does (`#[core::contracts::ensures]`, `core::mem::type_info`, `#[rustc_comptime]`).
2. **Syntax or an API rustc lacks**: the item goes under `#[cfg(blaze)]` (an out-of-line module
   when rustc cannot parse it, since rustc never opens a cfg'd-out module's file), with a plain
   item of the same name beside it under `#[cfg(not(blaze))]` whose doc says what blaze's adds.
3. **Never weaker in silence**: a property blaze proves at compile time keeps a check in the plain
   form (a `const` assertion of the instance, a runtime check), and the doc says who enforces what.

Until blaze sets `cfg(blaze)` it compiles the plain forms, which it accepts as any Rust.

| part | base (rustc and blaze) | blaze only, and its plain form |
|---|---|---|
| tool attributes (`pure`, `noalloc`, `linear`, `consumes`, `memo`, `policy`, `sans_io`, ...) | read by blaze, ignored by rustc | — |
| contracts (`budget`, `clock`, `kv`, `fs`) | `#[core::contracts::ensures]`, run under `contract_checks` | native `requires`/`ensures`: after the switch |
| `own`'s walk (`reach`, `raw`) | nightly's `type_info` and `#[rustc_comptime]` | `raw::name` (`core::reflect::name_of`); plain: a fixed word, so a refusal names fields, not types |
| `Acyclic`, `NoCapability` | the traits, the questions, `none` | the `where const` impls (`own/extended.rs`); plain: a blanket impl, with the walk asserted in a `const` where `Shared::holding` and `Shares::new` make a handle |
| `Flat` | the trait, `Address`, the question `AnyAddress`, `is_flat` | the `where const` impl (`flat/extended.rs`); plain: a blanket impl, with `is_flat::<T>()` asserted in a `const` where `chan::bounded` makes a channel |
| `Shares` | the type, `Deref`, `Send`/`Sync`, `Owns` | `where const` and `{ N + M }` (`own/shares/extended.rs`); plain (`plain.rs`): the same conditions as `const` assertions, `join` with the sum as a parameter `S` |
| `eval` | `Eval`, `Point`, keys, providers | `MemoPoint` and `code_id_of` (`core::reflect`); plain: every type is a point, and its identity is its `TypeId`'s, for one process |
| linear types (`Scope`, `Pending`, `File`, `Conn`, ...) | the types and their abandonment path in `Drop`, `#[must_use]` | leaving one unfinished is a compile error under blaze only |
| features `blaze_comptime`, `blaze_const_exprs`, `blaze_verify` | — | `cfg_attr(blaze, feature(..))` |

**What a new crate may depend on today**: bstd by its plain form, `core`, `alloc` (until bstd's own
collections replace it), and blaze's piece crates bstd re-exports. Not Rust's `std` (only
`bstd::rt` names it, the driver boundary), and no extension outside a form both builders read.

## What is in it

bstd is batteries included, and most of the batteries exist. It is a **facade**: the pieces stay
separate crates (they build in parallel, and a build script can take one alone), bstd re-exports
them under one tree of names, and code outside bstd imports `bstd::..` only (a lint bans the
piece crates' own names, so there is one name per thing).

Something belongs in bstd when most programs need it, or a rule names it as the one structure for
a need, and it can obey bstd's rules (no ambient authority, explicit memory, no global state).
What is about building Rust (the cargo graph, the store, codegen) stays out.

| module | what | from |
|---|---|---|
| `mem` | allocators by lifetime: frames, arenas, pools, caches | `blaze-mem` |
| `bytes`, `text` | held bytes and UTF-8 text, lent and never copied | `blaze-bytes` |
| `map`, `set`, `hash` | the one hash map and set (fixed seed), the identity hasher | `blaze-hash` |
| `id` | content ids (BLAKE3), dense `u32` indices, and the one interner | `blaze-ids`, `intern`, `blaze-hashcons` merged |
| `sync`, `cell` | named locks, `Shared`, `Atomic`, token cells, the bounded spin, reclamation at task ends | `blaze_core::lock`, `blaze-spin`, `blaze-reclaim` |
| `chan` | the one queue and the one channel | the crossbeam types rule `standard-structures` names |
| `sort`, `phf` | the stable radix sort; perfect hashes of fixed name sets | `blaze-radix`, `blaze-phf` |
| `digest` | BLAKE3 and SHA-256 | `blaze-ids`' `b3`, `blaze-sha256` |
| `rng` | the seeded generator behind the `Rng` capability | `blaze-rng` |
| `path`, `time` | relative paths; durations and instants | `paths` (replaced), new |
| `data` | serialization by compile-time reflection (below) | new, replacing `blaze-serde` in blaze's crates |
| `record` | zero-copy records: bytes in memory are the bytes on disk | `blaze-format` |
| `report` | the `Report`: what a program says to a person or an agent, as data | `blaze-report` |
| `quic` | QUIC as a sans-IO core: datagrams and time in, datagrams and stream events out; the driver is a `Net` datagram socket and the `Clock` | `blaze-http`'s QUIC core ([http3-sans-io](http3-sans-io.md)) |
| `http` | sans-IO HTTP/1.1 over a `Conn`, and HTTP/3 and WebTransport over `quic` | `blaze-http` |
| `trace` | spans and events through `Cx` | `blaze-trace` (no-tracing) |
| `gpu` | the device behind the `Gpu` capability: buffers, launches, render targets | new, with [gpu](../../../future/gpu/plan.md) |
| `video`, `image` | codecs as sans-IO cores; image decode and encode as functions | `harness-video`'s encoder, moved; `png` |
| `test` | the simulation, property tests, the model checker for lock-free code, allocation counting | `blaze_core::sim`, `blaze-sim`, `blaze-check`, `blaze-alloc-count` |
| `io`, `fs`, `net`, ... | the capabilities of the sections below | new, on `blaze-core` |

`stdx` (leftover helpers) is dissolved into the modules above or deleted. Each merge follows rule
`one-concept-one-home`: one type per concept, and the removed name banned.

### What a program pays for at start

A bstd program pays at start only for what it calls. Every library a binary links, dyld loads and
initialises before `main` on every run: CoreFoundation and CoreServices alone cost a tiny Rust
program about 1.0 ms and 11 M instructions (measured 2026-11-16). So:

- **Heavy areas are features.** The default is the core a command-line tool needs; `image` and
  `video` (VideoToolbox, CoreMedia, CoreVideo, libobjc, Foundation) are bstd features, and a crate
  that uses one names it in its manifest. A feature is for what a program never calls.
- **A system library wanted on some paths is loaded at first use**, never linked: one `dlopen` of
  the framework's binary, each symbol `dlsym`ed once into a typed table of function pointers
  behind a `OnceLock`, the table's `&'static` held by the value that needs it (so no call is made
  before the load), and an error naming the framework and symbol when either is absent, with no
  fallback. A test finds every symbol on the OS it runs on. blaze-core's FSEvents driver is the
  first (`fsevents.rs`, `Frameworks`): a check that never watches loads neither framework.
- **The command line and environment are read where the kernel left them**, never copied.
  `Args` and `Env` are views over the process's start block (`bstd::rt::block`): on macOS through
  `_NSGetArgv`/`_NSGetArgc`/`_NSGetEnviron` (libSystem's `crt_externs.h`), on Linux through the
  `(argc, argv, envp)` glibc hands each `.init_array` function. At start the arguments are
  checked to be UTF-8 (a refusal names the first that is not) and the environment's pointer
  array is taken, nothing more: a variable is found by a scan when asked, a non-UTF-8 one is
  unset, and nothing is sorted. A world from a run spec, or a simulated one, holds owned text
  instead: one type, two sources. An ordinary start's `RunSpec` holds no command line,
  environment or directory: the program's world reads the process's own. The current directory
  is not read at start: the native driver's root is `.`, made absolute only where a call needs
  an absolute path (a watch, a sandbox's scratch folder).
- **The binary is linked with `-dead_strip_dylibs`** on macOS (blaze-cli's `build.rs`), and
  `tests/links.rs` fails when `blaze` links anything but libSystem.

### `bstd::data`: serialization by reflection

`Encode` and `Decode` are implemented for a type from its fields ([comptime](comptime.md)), in
ordinary typed code: no derive macro, no generated tokens, errors on real lines, and nothing to
compile to wasm and sandbox. A type opts in by naming a policy for its fields, not by deriving.
The formats are bstd's: deterministic CBOR (the agent API's), JSON, and `record` for zero-copy.
Decoding takes an allocator, as everything does, and a size limit, since input is untrusted.
A union decodes by a tag naming its member. A type that holds a capability or a `Secret` has no
`Decode`, and saying so is a compile error naming the field: bytes can describe what is wanted
(a `Path`, a `VarName`), and only code holding the authority can turn that description into a
capability by narrowing its own.
The same field walk gives tracing its encoders and `Secret` its redaction. `blaze-serde` remains
for crates from crates.io that speak serde; blaze's own crates move to `data` once reflection
lands.

The runtime (`bstd::rt`) is what `std` hides: the entry that builds the `Io` and calls
`main(io)`, the panic handler and unwinding (on `library/unwind`), the allocator, thread-local
storage, and arguments and environment read once from the process's start block.

## Io

`Io` is one concrete type, not a trait and not a generic parameter: code is not monomorphized per
driver and no signature carries `<I: Io>`. Inside is the driver the target's `cfg` chose, or the
simulation, picked when the `Io` is made. `Io::sim(seed)` gives a whole simulated world (an
in-memory file tree, a virtual clock, a seeded generator, scripted peers, injected failures and
crashes), so any program on bstd is a deterministic simulation test with no mocking.

Calls read as blocking code and never block a thread. A call pushes an effect, the calling fiber
parks on the scheduler, and the completion resumes it. There is no `async`, no function colour
and no second copy of each API.

A sans-IO core takes no `Io` at all. It takes bytes and returns bytes and effects, as rule
`sans-io-cores` already asks; with capabilities that is a fact of its signature, not a lint.

## The boundary: ops out, events in

Under every capability is one boundary between a program and whatever hosts it. The program
writes **ops** (open, read, send, connect, set a timer); the host answers with **events**
(opened, data, sent, accepted, a datagram, the timer fired). Both are fixed-layout records in two
rings, exchanged in batches, each op carrying a token its event returns. Nothing else crosses.

The io_uring driver, the kqueue and IOCP drivers, the simulation and a wasm host are all hosts of
this one boundary, which is why a program cannot tell them apart. It is `blaze-core`'s
`Effect`/`Completion` made flat, and the shape wasm-host's guest ABI already proved at speed
(`ix-abi`, `ix-worker`). Taken from there:

- **Records are plain words** with one layout on every target, so a ring can be shared memory
  between a guest and its host, and a batch costs one crossing however many ops it holds.
- **Ids are dense with a generation**: a stream id indexes a slab directly, no hash map, and a
  stale id is caught by its generation.
- **Events lend their bytes.** Data arrives as a view into a host buffer, valid for the event;
  `retain` turns it into an owned buffer from the pool, and dropping that queues its release.
  An op's body is static, owned or pool memory. Receives are zero-copy end to end.
- **The host terminates protocols it is better placed to** (TLS, HTTP/1.1, 2 and 3, WebSocket
  and WebTransport framing in a wasm host) and hands over protocol-neutral events: a request is a
  request whichever version carried it.

### Three kinds of code on it

| kind | takes | is |
|---|---|---|
| a **core** | nothing: no `Cx`, no capability | a state machine. Inputs (bytes, events, the time) go in, outputs (bytes, ops, the next deadline) come out. `quic`, `http`, TLS, a journal, the build graph. Testable with no host at all |
| a **driver** | a `Cx` and the capabilities it needs | a thin loop: feed the core what the capability delivers, do what the core asks. A few dozen lines per core per capability |
| **straight-line code** | a `Cx` and capabilities | ordinary functions that wait. The runtime turns each wait into an op and a park, and its event into a wake-up |

Straight-line code and a hand-written machine are the same thing at the boundary. Under the
`Frames` layout the compiler makes the state machine from the straight-line function, so "write a
function, get a machine" is the default, and a machine is written by hand only for a core: logic
worth testing with no host, or reused under several drivers.

A program can also be a machine at the top: `impl Worker { fn event(&mut self, cx, ev) }`, called
once per batch with no runtime underneath. That is the right shape for a wasm guest and for the
hottest paths.

### Every reach goes through the driver

Everything a program does outside its own memory goes through one seam: the driver behind its
`Io` (`io.rs`, `Backend`: the native driver, the simulation, or a host's table in a hosted run,
run.md "The hosted world"). Record, replay, a fake, a dry run and asking all hold a run by holding
that seam. A text check in the crate's tests (`tests/reach.rs`) keeps it true: no file but the
backends (`rt.rs`, `rt/`, `native.rs`, and the one line of `io.rs` that makes the native driver)
names a system call or a std function that reaches the system. It sees names, not calls: an
aliased import, a macro-built path, another crate that reaches the system itself, or inline
assembly are outside it.

Inventory, as built on 2026-11-16. "Varies" says whether two runs can answer differently, so a
replay must supply the answer.

| capability | how it reaches the system | varies |
|---|---|---|
| arguments (`need_args`) | given to the world by the launcher when the driver is opened (native: a view of the kernel's start block; spec: the spec's; hosted: the table's `arg`), held, read in place | yes |
| environment (`need_env`) | as arguments (hosted: none yet) | yes |
| standard input | no capability yet | (yes) |
| output and error (`need_out`, `need_err`) | `Backend::write_out`, one call per flush of an `Out`'s buffer, made on the calling thread: native through `rt::write_out`, simulated into `SimHost`, hosted the table's `write`. Not an `Effect`: the native driver performs one on a thread per stream, which `hello` cannot pay for | no |
| files (`need_fs`) | `Effect`s through `Io::submit` and `Io::poll` | yes |
| monotonic clock (`need_clock`, `Clock::now`) | `Backend::now`, a reading through the driver per call (hosted: the table's `clock`). Not an `Effect`: a reading per call as an effect costs a wait per `now` | yes |
| wall clock (`Clock::wall`) | `Backend::wall_ns`, as above | yes |
| since process start (`Clock::since_process_start`) | the host facts' `born`, read once at open (`native.rs`) | yes |
| sleeping (`Clock::sleep`) | `Effect::Timer` | yes |
| randomness (`need_rng`) | one `Effect::Random` of 8 bytes at the first ask, kept as the seed; every number after is SplitMix64's from it, so the seed replays them all | yes |
| host facts (`need_host`: pid, cores, name, build id) | read once at open by the native backend (`native.rs`), kept in the world | yes |
| memory (`need_mem`) | inside the process (frames, arenas); nothing reaches out | no |
| network (`need_net`) | `Effect`s | yes |
| child processes (`Procs`) | simulation only (`SimHost`); no native provider | (yes) |
| terminal, keys, roots, locale, user, code, windows, automation, opener, power, sound, devices, http and streams by the host | simulation only (`SimHost`); a native world withholds them | (yes) |
| exit (`Host::exit`, end of `main`) | direct by design: flush, then `rt::exit` (hosted: the table's `exit`) | no |
| the entry's last words (`rt::error_line`) and the manifest a launcher prints | direct, in `rt`, before or after any world | no |
| the scheduler's clock and the machine's topology (`rt::pool`, `rt::machine`) | direct, in `rt`: they decide where work runs, never what a program sees | no |

Bypasses counted before this change: three. The host facts were read in `Io::native` (cores,
process age, pid, build id): now in `native.rs`, the native backend. Output was written by
`Io::write_out` calling `rt::write_out` itself: now `Backend::write_out`. The generator's seed was
std's `RandomState`, then `std::random` in `Io::rng`: now one `Effect::Random`.

### When a capability is missing

A host need not offer everything. A wasm worker has no sockets; a sandboxed child was handed two
directories; a test gives a fake.

- **What a host offers is known at start**, from a table it hands over before `main` (the
  manifest). Nothing is discovered by an op failing halfway through.
- **`main` says what it needs, and that is the program's manifest.** A required capability the
  host does not offer stops the program before `main` runs, naming it. An optional one is an
  `Option` in `main`'s hands: `io.net()` returns `Option<Net>`.
- **Code below `main` never asks whether a capability exists.** It takes `&Http` or it does not;
  absence is settled once, at the top, where the program chooses a provider:

  ```rust
  let http = match (io.http(), io.net()) {
      (Some(host), _) => host,                              // the host makes requests for us
      (None, Some(net)) => Http::over(&net, &clock, &roots), // we do, over its sockets
      (None, None) => return Err(Missing("http")),
  };
  ```

  `Http::best(&io)` is that match, for the common case.

- **Both arms give the same type.** `Http` is one struct whose provider is a field, so the match
  type-checks, and code below it is compiled once.
- **What the target can never offer is settled at compile time.** A target's possible
  capabilities are a constant (`bstd::host::OFFERS`). A provider the target cannot have is not a
  variant of the field at all, and `io.net()` on a target with no sockets is a constant `None`,
  so the match above folds to one arm and the call to one direct call: no dispatch is paid for a
  choice the build already made. A capability `main` requires that the target cannot offer is a
  compile error, not a failure at start.
- **What is left for run time** is only what truly varies there: a parent that granted a child
  less than its target allows, and a handler passed in a test.

## Cx: the task's context

A capability says what code may touch. `Cx` says on whose behalf it runs. Every task has one, and
every call that can wait or allocate scratch takes `cx: &Cx` as its first parameter. It holds:

- **the deadline**, an instant on the task's `Clock`. `cx.until(deadline)` makes a child context
  whose deadline is the earlier of the two: like a capability, it only narrows.
- **cancellation.** A scope that fails or is dropped cancels its tasks; a wait under a cancelled
  `Cx` ends with `Stopped::Cancelled`. `cx.check()?` is the one line a long computation calls.
- **the span.** The task's name and its place in the trace. `cx.span("what")` opens a child, so
  every wait is named by construction (rule `every-wait-says-what`).
- **the frame.** `cx.frame()` is the task's bump allocator, freed whole when the task ends.

A sans-IO core takes no `Cx` and no `Io`: it cannot wait, so it needs neither. A function that
takes `&Cx` and no capability can wait and allocate scratch but cannot touch the world. `Cx` is
not ambient and is never fetched from a thread-local; `main` receives the root one.

## Capabilities

| type | made from | narrowing | gives |
|---|---|---|---|
| `Fs` | `io.fs()` | `open_dir(path)` | directories |
| `Dir<P>` | `Fs`, a `Dir` | `subdir`, `read_only` | files beneath it, by relative path only |
| `File<P>` | `Dir` | `read_only` | positional reads and writes, each saying its durability; `flush`; pinning |
| `Net` | `io.net()` | `connect_only(host)`, `listen_only(addr)` | `Conn`, `Listener`, `Datagrams` (UDP), name lookup |
| `Env` | `io.env()` | `only(names)` | environment variables |
| `Args` | `io.args()` | | the command line |
| `Host` | `io.host()` | | the process id, core count, host name, signals, and exiting with a status |
| `Clock` | `io.clock()` | | `Instant`, wall time, `Deadline` |
| `Rng` | `io.rng()` | `fork()` | random bytes; seeded in the simulation |
| `Sandboxes` | `io.sandboxes()` | a smaller budget | `OsSandbox`, `Vm`, `WasmSandbox`, and the `Runner` each makes (below) |
| `Out` | `io.out()` | | standard output (built); standard input (S5e). Standard error is the entry's, not a capability |
| `Mem` | `io.mem()` | | named arenas and caches against the memory budget |
| `AudioOut`, `AudioIn` | `io.audio()` | a device | playing and recording sound (below) |
| `Gpu` | `io.gpu()` | `only(kernels)`, a memory budget | a device: buffers, kernel launches, offscreen render targets (below) |

Rules that hold for all of them:

- **Unforgeable.** Private fields; only the launcher (a grant `main` names) or narrowing makes
  one.
- **Narrowing never widens**, and it happens in the OS too: `read_only()` reopens the handle with
  fewer rights, so trusted `unsafe` code cannot write through it.
- **Permissions are types.** `P` is `ReadOnly`, `ReadWrite` or `ReadWriteExec`; methods are gated
  by `P: CanWrite`.
- **Paths are relative.** `Path` cannot hold a root or `..`; every open is beneath its `Dir`
  (`openat2` with `RESOLVE_BENEATH`, and its equal elsewhere). There is no current directory.
- **Never in a static.** A capability cannot be stored in a `static` or a thread-local, or the
  signature would stop being the whole truth. The `ambient` check refuses it.
- **Clone is allowed.** Copying authority you hold adds none.

**Nothing is global.** Everything a program can reach outside its own values comes down from
`main`'s grants and `Cx`:

- There is no `std::env::var`, no arguments function, no current directory, no process-wide
  clock, random source, exit, signal handler or standard stream: each is a capability above.
- **No global mutable state.** A `static` may hold only immutable data. A `static` with interior
  mutability (an atomic, a lock, a cell), a `static mut` and a thread-local are refused in a
  capabilities crate (the effect `global_state`): state shared across calls is a value someone
  owns and passes, or a context.
- **Spans and events go through `Cx`** (`cx.span(..)`, `cx.event(..)`); the recorder is reached
  from the root `Cx`, not from a global.
- Allocation is not ambient (below).

The one thing left ambient is a panic, which is the language's, and it fails its task.

### What replaces a thread-local

A thread-local is the wrong tool on fibers anyway: a task can resume on another thread after any
wait (rule `no-wait-across-thread-locals`). Each thing people use one for has an owner here:

| used for | instead |
|---|---|
| the "current" something (span, deadline, database, runtime) | `Cx`, or a `with` context |
| scratch buffers reused across calls | `cx.frame()` |
| state that belongs to one task | a local the task owns, passed down |
| per-thread caches and counters that avoid contention (allocator caches, statistics, shards of an interner) | `PerWorker<T>` |

`PerWorker<T, A>` is an ordinary value: one cache-line-padded slot per pool worker, made with
`PerWorker::new_in(workers, alloc, |worker| ..)` and owned and passed like anything else.
`slots.with(cx, |slot| ..)` gives `&mut T` for the worker in the `Cx` (`Here::Worker(i)`); code
off the pool (the entry's thread, an `Inline` root) gets a typed `NotAWorker`. The closure gets no
`Cx`, so it cannot wait and its task cannot leave the worker while it holds the slot. A closure
can still capture an outer `Cx` and wait through it, letting another task of the worker in, so
each slot carries an entered flag, checked in every build (not only debug: it is what makes the
`&mut` sound under rustc); a second entry panics, failing that task.

- `iter(&mut self)`, `into_iter(self)` and `take_all(&mut self) -> Vec<T, A>` read every slot
  once no task can reach them: the exact total of counters after their scope joined.
- `fold(&self, cx, init, visit, merge)` reads them while work runs: a visit per worker goes on
  that worker's pinned queue, which it runs between tasks (or while it waits, helping), so no
  visit ever sees a slot mid-`with`; it is awaited with the scope's deadline, and merged in worker
  order.
- `snapshot_racy(&self, f)` reads every slot from the calling thread, for a `PerWorker` built
  with `new_racy_in`, whose `T: Racy` (a marker trait, `unsafe`, for atomics and arrays and tuples
  of them): the statistics case, with no visit and no wait.

This is the per-CPU variable of a kernel and the shard of a shared-nothing runtime, without a
global: the worker index lives in the `Cx`, so the runtime keeps no thread-local at all.

**Secrets.** `#[derive(bstd::Secret)]` gives a type no `Debug`, `Display` or `Clone`, zeroes it on
drop, and hands its bytes only to functions of its own module.

### Capabilities are layered, and each has providers

A capability names what a program needs, not how the host supplies it. `Http` is one: "may make
requests to these origins". It has more than one provider:

| provider | how it answers a request |
|---|---|
| `Http::over(&net, &clock, &roots)` | bstd's sans-IO HTTP core over connections from that `Net`; never reaches further than the `Net` does |
| `io.http()` | the host does it: `fetch` in a wasm worker, where there are no sockets at all |
| `Http::handler(f)` | a function you pass: a fake for a test, a cache, a proxy |

All three are the same type, `Http`, so `fn sync(cx: &Cx, http: &Http)` runs natively, in a
worker and in a test unchanged. The provider is a field, chosen where the capability is made; a
request is already far more work than one indirect call. `http.only(origin)` narrows whichever
it is.

Three rules come out of this:

- **Ask for the highest capability that does the job.** A function that takes `&Http` runs
  anywhere a request can be made; one that takes `&Net` runs only where there are sockets. The
  same goes for `Streams` (WebTransport: `quic` natively, the browser's in a worker) over
  `Datagrams`.
- **`Io` has the shape of its host**, known at start (above): a native process offers `net`,
  `fs` and `process`; a wasm worker offers `http` and `streams` and no `net`.
- **A lower capability can always make the higher one**, never the reverse: `Net` gives `Http`,
  `Datagrams` gives `Streams`, a `Dir` gives a key-value store. That is narrowing: the higher one
  can do strictly less.

The protocol logic is the sans-IO core in every case; a provider is only its driver.

## Tasks and waiting

There is no `async` and no `.await` in a capabilities crate. A function that waits is an ordinary
function. ([may-suspend](may-suspend.md) is the analysis underneath.)

- **One wait.** `Cx` has the single wait primitive: wait for this, until the deadline, with a
  typed outcome (`Ready`, `Stopped::Deadline`, `Stopped::Cancelled`). Locks, channels, sleeps and
  every IO call are built on it, which is why each takes a `Cx`. Nothing can wait for ever (rules
  `bounded-progress`, `deadline-waits`).
- **Structured only.** `cx.scope(|s| s.may_overlap("name", scratch, |cx| ..))`. The child's
  `cx` is borrowed from the scope, so a task cannot outlive it; children may borrow from the
  parent, as `std::thread::scope` allows. There is no detached task and no `io.tasks()`:
  starting work is something a `Cx` does.
- **May overlap, or must** (from Zig's `std.Io`, `async` against `concurrent`). The spawn form
  says what the code's correctness rests on, at the one seam, the scope:
  - `scope.may_overlap(..)`: the task may run later or at the same time as what follows, and
    the code is correct either way. With no worker free it runs inline, to completion, at its
    spawn; the one-thread executor (the simulation's, a test runner narrowed to one worker)
    runs every one inline, in spawn order. `par`'s chunks are these.
  - `scope.must_overlap(..)`: the code is correct only if the task and its spawner make
    progress at once (a producer and a consumer of one bounded channel). Where the scope has
    fewer workers than that (`Scope::workers`: the layout's, narrowed by the scope's budget,
    `narrow_workers`), the task never runs and the call returns a typed `NoParallelism {
    needed, have, .. }` at once, before anything waits: never a deadlock that ends only at
    its deadline. The scope is not failed by it; the caller says what follows.
  - Built by blaze, `must_overlap` where the build's layout has one worker becomes a compile
    error; the plain form is the run-time refusal, and `Inline` refuses every one.
- **Spawn is eager, and cancellation is a value.** A cancelled task learns it only as
  `Stopped::Cancelled` from its next wait. Nothing is cancelled by being dropped, and user code
  never holds a task that has not started or is half run.
- **Finishing is explicit.** What must be closed, flushed or joined is a linear value with a
  call that takes `cx` ("Values that must be finished"); `Drop` waits only on the panic path.
- **A panic fails its task** with a typed error to its scope (rule `panics-fail-one-job`).
- **A program runs only in a sandbox** (below). There is no bare `spawn`.
- **Code that uses `async` still builds** as the pinned nightly builds it. The bridge is one
  function: wait on a `Future` (poll it, park the task while it is pending, its waker unparks).

### Layouts

How a waiting task's frames are stored is the **layout**. Programs behave the same in every
layout they compile under: same outputs, same order of cancellation, drops and panics.

| layout | a task is | a wait |
|---|---|---|
| `Inline` | run to completion at `spawn` | nothing else runs; no runtime at all |
| `Threads` | a job on a worker of a thread-per-core pool, stolen only before it starts | blocks the worker's thread |
| `Fibers` | its own stack (the scheduler of section 6) | switches stacks |
| `Frames` | a bump slab holding the frames of its functions that can wait, each made on call and freed on return; code that cannot wait runs on the worker's stack | returns to the scheduler |

`Frames` is the long-term one: no stack per task, and recursion and `dyn` calls work because the
callee makes its own frame. Frames never move, so there is no `Pin`. Fusing a known,
non-recursive callee's frame into its caller's is a later optimization, not another layout. A
panic unwinds across slab frames by a second return path each may-suspend call checks.

The simulation is not a layout: `Io::sim(seed)` picks a seeded scheduler and world at run time,
under any layout.

**A started task never changes its thread** ([bottom](bottom.md), "Fibers and `Send`"). A new
task may be placed on any worker (`Scope::may_overlap` and every `par` chunk); once it has started it
resumes from each wait on that worker and OS thread, so a value that is not `Send` (an `Rc`, a
`LocalLock`, a guard) held across a wait is still on the thread that made it. That is the
contract of the plain form, the one rustc builds: a fiber or frames scheduler must put a parked
task back where it ran, and a debug build checks it at every resume (`Cx`'s `resumed`, a panic
naming the task). `tests/placement.rs` proves it on the simulation, with a planted resume that
moves the task. Built by blaze the contract is wider: a value that is not `Send` live across a
call that can wait is a compile error, so a task may move between waits and the check goes;
this is stated, not yet code. bstd has no thread-local storage, so nothing else could tell, and
each one-thread type (`LocalShared`, `LocalLock`, `LocalOnce`, `LocalLazy`) checks in a debug
build that it is dropped on the thread that made it.

**The layout is chosen per build, not per signature.** `Cx` and the capabilities are concrete
types; the build configuration says which layout they are, as it says which IO driver a target
gets. Functions are compiled once, and no signature carries a type parameter for it. (The
alternative, `cx: &impl Tasks` with functions that wait monomorphized over the layout type, lets
two layouts live in one binary at the price of a generic on every waiting signature and two
copies of the code. The differential test below builds twice instead.) In the plain form the
choice is a value of the root context instead: `Io::cx` gives an `Inline` root (the
simulation's, every test's), and the entry (`bstd::entry::run`) starts a `Threads` pool over
the machine's topology and gives `main` a root on it. One binary can hold both, so the tests
compare them in one run; the type that says which is `Layout`, and the bound every spawn takes
is the strictest layout's (`Send`, below), so code that compiles for one compiles for all.

### `Threads`: the runtime of every bstd program

`Threads` is not a test executor: it is what every bstd program runs on, the compiler's driver
included, and it is built for a 128-core EPYC or Graviton first and a Mac with performance and
efficiency cores second, with one code path sized by the machine (crates/blaze-std/src/task/
threads.rs, its deque in task/deque.rs, its idle protocol in task/idle.rs, the machine's side in
rt.rs). Each point below says whether it is built (**built**) or designed only (**designed**).

1. **Thread-per-core; only unstarted tasks move** (**built**). A started task never changes
   thread (above), so stealing takes only tasks that have not begun, and everything a running
   task touches is its worker's alone: its frame (reserved by the spawner, first touched by the
   worker, so a pinned worker's pages land on its node), its lock-order token and its placement
   (`Placed`, an `Rc` made on the worker). A running task takes no atomic of the pool's: the
   pool's atomics are on the paths between tasks (take, spawn, finish). Its timers, IO ring and
   trace producer are worker-local in the design (point 7) but not yet: they are the world's,
   behind its one lock (**designed**).
2. **Queues** (**built**). Each worker has a fixed ring of 256 tasks, a Chase-Lev deque: the
   owner pushes and pops at the bottom with no lock and no read-modify-write except on the last
   task, thieves take from the top with one compare-and-swap each. Beside it a one-task LIFO
   slot holds the task just spawned (the cache-hot one, run next); a spawn into a full slot moves
   the slot's task to the deque, so a ping-pong pair stays in the slot and cannot starve the
   deque, because the slot is taken at most `LIFO_RUN` (3) times in a row before the deque is
   looked at. Tasks spawned from outside the pool (the entry's thread) go to a bounded injection
   queue per worker group (1,024 tasks), foreground and background apart: never one global
   queue. The orderings: the owner's push writes the slot then stores `bottom` with `Release`;
   a thief loads `top` with `Acquire`, then (after a `SeqCst` fence) `bottom` with `Acquire`, reads the
   slot and claims it with a `SeqCst` compare-and-swap of `top`, so the slot it read was written
   before the `bottom` it saw and no other thief or the owner took it; the owner's pop stores
   `bottom - 1`, then a `SeqCst` fence orders that store before its load of `top` (the store and
   the thief's load of `bottom` are the two sides of a Dekker pair), and only when one task is
   left does it race the thieves with the same compare-and-swap. The deque is checked by
   crates/blaze-check under `cfg(blaze_check)` (`task/deque.rs`), with a planted steal that does
   not re-check the top that the model must catch.
3. **Topology is a value** (**built**). `Topology` is a list of groups, each the workers that
   share a last-level cache, with their level (performance, efficiency, or uniform) and, where
   the system pins, their CPUs (rule nothing-ambient: the pool reads no machine fact). The entry
   reads it in `rt.rs`: on Linux from `/sys/devices/system/cpu/cpu*/cache/index3/shared_cpu_list`
   (an EPYC's eight cores a CCX; a machine that lists no level-3 cache, as one Graviton node, is
   one group); on macOS from `hw.nperflevels` and `hw.perflevel{N}.logicalcpu` and
   `.cpusperl2` (a Mac's clusters). A thief tries its own group, from its neighbour on, then the
   other groups; a steal takes half the victim's tasks in one go (one claim each, so each is the
   proven single steal), the first run at once and the rest put in the thief's own deque.
4. **Idle and wake** (**built**). An idle worker spins `SPIN_ROUNDS` (64) looks at the queues,
   then parks through bstd's one kernel wait (`blaze_core::wait::Parker`: the futex on Linux,
   `os_sync_wait_on_address_with_deadline` on macOS) with a deadline (one second, then it looks
   again: rule deadline-waits). A spawn wakes at most one worker, and only when no worker is
   already searching: the count of searching workers is Go's and Tokio's guard against a
   thundering herd, and a searcher that finds work and was the last searcher wakes one more, so
   a burst fans out one worker at a time. At most half the workers search at once. The
   orderings (`task/idle.rs`): a worker going to sleep stores its state `Parked` and leaves
   the search (`SeqCst`), then a `SeqCst` fence, then looks at every queue once more; a spawner
   publishes its task (the deque's `Release`, or the injection queue's lock), then a `SeqCst`
   fence, then reads the searching count and the states. Of the two fences one is first in the
   single order, so either the sleeper sees the task or the spawner sees the sleeper: no wake is
   lost. The wake itself is a compare-and-swap of the sleeper's state `Parked -> Searching` and an
   unpark (a token: an unpark before the park makes the park return). A blaze-check model runs
   this protocol on the shims with a planted wake that skips the fence. The lazy start (a
   pool's threads begun by its first spawn, `Pool::begin`'s compare-and-swap) is modelled too:
   two first spawns at once start the worker once and lose neither task, and a begin without
   its compare-and-swap is caught with its own message. Run 2026-11-16 on snapshot 6f08c0ba7e
   (`--cfg blaze_check`, release): `model_lazy_start` 2 passed, `model_idle` 2 passed,
   `model_deque` 3 passed, every planted control caught. A `must_overlap` task is reserved
   for the worker whose mailbox holds it, and its joiner can never claim it (task/claim.rs:
   the state says which kind it is, and the joiner's claim takes only open tasks); before the
   lazy start that held only by timing. Modelled as a joiner racing the worker's arrival, with
   a planted joiner claim of a reserved task: run 2026-11-16 on snapshot b4bf3a4528,
   `model_claim` 2 passed, the plant caught.
5. **Placement** (**built**). On Linux each worker is pinned to its CPU (`sched_setaffinity`),
   so first touch keeps its frame on its node. macOS has no affinity: each worker sets a QoS
   class instead (`pthread_set_qos_class_self_np`, rule macos-private-apis: a public but
   Darwin-only call, tested to exist): workers of the performance level at user-initiated,
   those of the efficiency level at utility, so the system keeps them on their cores. A scope
   narrowed to `Class::Background` (`Scope::narrow_workers(most, class)`) sends its tasks only to
   the groups' background queues, which a foreground worker takes from only when no foreground
   task is queued anywhere it looks, and an efficiency worker takes from first.
6. **Fine-grain loops** (**designed**). Today a `par` call makes one task per chunk at the
   caller's grain. The design is lazy splitting: a parallel range runs serially on its worker
   and offers its remaining half as a task only when the worker's deque is empty or a heartbeat
   passes (100 microseconds on the worker's clock, read every few thousand items). With a
   heartbeat `H` and a task's creation cost `c` (a frame reserve and one deque push, about a
   microsecond), the overhead against serial is at most `c / H`, 1% at those numbers, however
   fine the items are, and a range splits only as fast as idle workers can take it.
7. **IO per worker** (**designed**). Each worker owns its driver: an io_uring on Linux, a kqueue
   on macOS. A task's effects are submitted in one batch at the end of its turn (its next wait),
   and a completion wakes only tasks of that worker, so IO a task started never crosses a core.
   Today one `Io` holds one driver behind the world's lock (crates/blaze-std/src/io.rs, `World`),
   and a wait polls it under that lock: IO waits of different workers are serialized. What
   blaze-core must change: `Uring` and `Kqueue` must be made one per thread from shared roots
   (the file table and the root directory handle shared read-only, the request slots and wheel
   per ring), a ring's poll must be callable without a lock other workers take, a completion
   must name its ring, and `Uring`'s off-ring errands (listings, the kqueue's file threads) must
   hand back to the ring that asked, by its eventfd or `EVFILT_USER`. bstd then keeps one
   driver per worker in the worker's state and `Cx::wait` polls its own.
8. **Backpressure by construction** (the rule; **built** for the pool's queues). Every queue
   between workers is bounded and holds flat values or task handles of fixed size: the deque
   (256), the LIFO slot (1), the injection queues (1,024 each), `chan` (its capacity). A full
   queue never grows: a spawn into a full deque runs the task inline, at its spawn (correct for
   `may_overlap` by its contract); a full channel makes its sender wait under its deadline. The
   reason: a producer that outruns its consumer, or a link when the consumer is another machine,
   must be slowed where it produces, since memory that grows to absorb the difference is only a
   later and less legible failure (rules bounded-progress and one-memory-measure), and a flow
   larger than any link carries can only be paced, never buffered.
9. **No false sharing** (**built**). Each worker's state, and each counter the workers write
   (searching, parked, the injection queues), is padded to a cache line: `LINE` is 128 bytes on
   Apple silicon and 64 elsewhere, one `const` a target, asserted against the padding type's
   alignment.
10. **The simulation stays the reference** (**built** in part). Every decision the pool makes
    (which queue, which victim, whom to wake) is a function of the pool's state and the
    worker's index, never of a clock or a random draw, so a schedule is replayable from the
    order of events; the simulation executor is `Inline`, against which the tests compare
    `Threads` at 1, 2 and 8 workers. The deque and the idle protocol are model-checked; the
    task cell's claim (a task run by its worker or by its joiner, never both) is a compare-and-
    swap stated in its `SAFETY` and exercised by the stress test, not yet model-checked.

**What a spawn does under `Threads`.** `may_overlap` reserves the task's frame on the spawner,
records the task's opening in the trace, makes the `Seed` of its context and one task cell (one
allocation, `blaze_own::Arc` in `Global`), and pushes it onto the spawner's LIFO slot (on a
worker) or its group's injection queue (outside). When the scope is narrowed to one worker, or
the queue is full, the task runs inline instead: never a block, never a drop. `Task::join` that finds its task unstarted claims it and runs it on the joiner's thread
(correct for `may_overlap`: it runs later than its spawn, with its spawner paused); one that
finds it running parks on its scope's `Parker` until the worker unparks it. `must_overlap`
reserves a worker that is searching or parked (a compare-and-swap of its state to `Reserved`) and
hands it the task through its mailbox, which that worker runs before anything else; when the
scope's budget has one worker, or no worker is idle, it is the typed `NoParallelism`, at once.

**A `Seed`: what of a context crosses threads.** `Cx` is not `Send`: its frame is a bump
allocator with no lock, its placement and lock-order token are one task's on one thread. A
`Seed` (cx.rs) carries what may cross: the deadline, the scope's cancellation (an `Arc` of an
atomic flag and its outer chain), the task's name and its parent's, the frame (reserved, never
touched by the spawner afterwards; `Frame` is `Send`), the world (`Io` is `Send` and `Sync`),
the plant, the pool and the scope's class. The worker grows it into the task's `Cx` on its own
thread, with a placement made there. A task's closure and its result are `Send` (rustc's bound
on both spawns, in every layout, so code that compiles under `Inline` compiles under `Threads`);
the closure's error is turned into the task's message on the thread that ran it.

**What stays single-threaded, and why.** A scope and its task handles (they hold the scope's
context and are joined by the thread that made them, which parks on the scope's one `Parker`);
the world's driver (point 7); the order of a scope's failures (its first failure is the one of
the lowest spawn index, so it is the one `Inline` reports, whatever finished first).

**Ending.** A scope returns only once each of its tasks has ended, since they may borrow from
its frame: its join waits until its context's deadline, then cancels and waits
`Limits::abandon_wait` more, and a task still running then is a hang, which stops the process
with a message naming the scope and the task (rule every-command-has-a-timeout: a hang is a bug
to report, and returning would free what the task borrows). A panic in a task fails that task
with `TaskWhy::Panicked` and the worker goes on. The entry's end stops the pool: every worker is
told to stop and is joined by a deadline, and a worker that has not stopped by it is reported by
name with the task it runs (`Hung`), and the program exits failed.

### What the compiler checks

- A `!Send` or `#[must_not_suspend]` value live across a call that may wait is an error naming
  the value, the call and the path to the wait, in every layout.
- May-suspend is inferred exactly, from the wait primitive; "takes a `Cx`" is only the usual
  sign, since a value can hold its capability out of sight. The `#[blaze::may_suspend]` mark on
  `dyn` methods and function-pointer fields is also the calling-convention fact `Frames` needs.
- What `Frames` cannot do is an error with the call path, never a quiet switch to a fiber (rule
  `no-fallback`): for one, a foreign (C) frame live across a wait.
- `must_overlap` under a layout with one worker (`Inline`) is an error; until blaze checks it,
  it is the run-time `NoParallelism`, returned before the task runs.
- A layout with more than one worker runs a `must_overlap` task beside its spawner: `Threads`
  does, on a reserved worker, from the `Seed` of the task's context (above).
- A task cannot outlive its scope: the lifetime on the child's `cx` enforces it.
- In ordinary crates, where thread-locals still exist, their address is reloaded after any call
  that may wait.

### Proof

Deterministic simulation tests of the scheduler, scopes and cancellation with an oracle and a
planted bug; a differential test that runs the same programs under `Fibers` and `Frames` and
compares outputs and the order of cancellations, drops and panics; and a UI test for each error
above.

### Files: whether bytes reached the disk is in the type

Decided 2026-11-10, after a build's answer was held four seconds on a 162 MB executable: the
store wrote the file without a sync and then cloned it, and a clone of a file whose data is
still in the page cache makes the kernel write it out first (1.3 s there, against 29 ms for the
write and 1 ms for a clone of a flushed file). The API let "write, then clone" be written and
hid the two things that decide its cost. Built in `bstd::fs` (S1).

- **Durability is an argument with no default.** Every change to a file or a name (publish,
  append, write at an offset, cut, rename, remove, close; `Kv` and `Blobs` put and delete)
  takes `Durable::No` (derived data: lost or torn by a power failure and made again),
  `Durable::Started` (writeback begun, nothing waits) or `Durable::Yes` (synced: the one that
  waits for the device). Nothing syncs unasked, and nothing claims a durability it was not
  asked for: there is no `write` that is quietly atomic and durable, and no `close` that
  quietly syncs.
- **Each has three forms**, by the second rule's "a policy that changes an effect is a
  different function":

  | form | takes | waits for the device |
  |---|---|---|
  | `x_unsynced(.., Unsynced)` | `Unsynced::{No, Started}` | never |
  | `x_synced(..)` | nothing more | yes: the file's bytes, then its directory's names |
  | `x(.., Durable)` | the policy as a value | when it is `Yes` |

  So a caller checked as non-blocking calls `_unsynced` and cannot reach a sync. The only
  other wait for the device is `flush`.
- **A file whose data may not be on disk is another type.** An `_unsynced` call gives
  `Written`; `written.flush(cx)` gives `Flushed`, and is the wait; a `_synced` call gives
  `Flushed`. `clone_to` exists on `Flushed` alone; `copy_to` (a plain read and write of the
  bytes, a chunk at a time) is on both. A clone that forces writeback cannot be written. A
  `Flushed` does not go stale silently: it carries the file's generation (the number of the
  last effect of its world that changed the file, kept by the `Io`), and a clone of a file
  changed since is refused, naming it (`FsKind::Stale`); flushing again gives one that clones.
  A change by another process is not seen until an effect reports the inode and change time.
  Neither replaces a file that is there: that is `publish`.
- **Say what a path should hold, not how.** `dir.publish(path, content, durable)` makes `path`
  hold `content`, replaced all at once (a temporary file beside it, renamed over it), where
  `content` is bytes (`Content::bytes`), a flushed file (`Content::file`, a clone), any file
  (`Content::copy_of(&file, chunk)`, a copy), or a flushed file and the ranges that differ
  from it (`Content::patch_of(&base, ranges)`, a clone and writes). What it did is in its
  result (`Published::how`: `Written`, `Cloned`, `Copied`, `Derived`) and recorded in the
  caller's span. The constructor's argument type decides the cost today: a `Flushed` is
  cloned, anything else copied. Choosing between them itself (a link where nothing writes in
  place, a copy across file systems, a derive only when little differs) needs the volume and
  the size of the difference, which blaze-core's effects do not report yet; until then a file
  system that cannot share blocks fails a clone, naming both volumes, and nothing copies in
  its place. The store's placement of outputs is one caller.
- **What a crash keeps is stated per call** and tested: a `_synced` publish or remove that
  returned is kept exactly; an `_unsynced` one leaves what was last synced, or itself cut
  short; an append is kept once a later `flush` returned. A reader, crash or no crash, sees
  the old contents of a published path or the new, never a part.
- **A writer that buffers is its own type.** `file.buffered(frame, capacity)` gives
  `Buffered`: pushes are kept until the buffer is full, and each says which it did
  (`Pushed::Held` or `Pushed::Wrote(n)`). It is linear, so bytes it holds cannot be dropped:
  it ends with `finish(cx)`. `append` keeps nothing back.
- **The simulation charges for it.** `io.disk()` is the simulated disk's ledger: bytes written
  and read, flushes, waits for the device, clones, and the bytes a clone of an unflushed file
  forces out (`forced_writeback`). A test of an edit path asserts none were, and that no
  `_unsynced` call waited for the device, with no real disk. The ledger also holds the name
  changes no directory sync covered, and a simulated crash undoes a suffix of them, so "synced
  the bytes, forgot the directory" loses the file in a test as it would on a disk.

What blaze-core's effects make of it today, each a refusal or a stated gap and never a quiet
substitute:

| wanted | today | needs |
|---|---|---|
| `Durable::Started` | built: the change is made, then `Effect::WriteBack` (all-1110's effect: `sync_file_range` on Linux, `fsync` without `F_FULLFSYNC` on Apple's) is handed over and not waited for; a name change also begins its directory's `Effect::SyncDir` | nothing; `F_BARRIERFSYNC` on macOS comes with blaze-sys's macOS layer |
| a name made durable after a remove, a create or `sync_names` on a real disk | built: `Effect::SyncDir` (`fsync` of the directory), waited for, the driver's root included (`DirName::Root`); refused on Windows, naming it | the Windows call |
| an unsynced rename or publish that costs no sync on a real disk | built: an `_unsynced` call renames by `Effect::RenameUnsynced`; only a `_synced` one by the syncing `Effect::Rename`. The unsynced rename takes the caller's `Unsynced`, the synced one gives the proof `sync_names` takes; `tests/syncs.rs` asserts each call's effects, with a plant for each mistake | |
| a read that does not copy | `Done::Read` hands back a buffer the driver made, copied into the caller's `Bytes` | the page pool (`file.pin`, S1f) |
| a write that does not copy | each write's bytes are copied once into the effect, which owns them | registered buffers (`WriteFrom`), with S4 |

**Folders above the start folder** (decided 2026-11-16; a command finds its workspace's manifest
above the folder it was started in). `fs.ancestors(cx)` yields a `Dir<ReadOnly>` per folder
above the start, nearest first, ending at the granted top: the filesystem's root for a command,
the run spec's directory itself (so none) for a launched program unless its grant names more
levels. An ancestor is named relative to the start, by leading `..` steps alone (`..`,
`../..`), and its files beneath it the same way (`../Cargo.toml`): the driver's root stays the
start folder, so no absolute path enters a name or a key, and the kernel walks no more of a path
than before. The depth is read through the driver once, at the first call (the start's own
names in each ancestor, one `getcwd`), never at start. A name that walks back down into the start
(`../proj/x` for `x`) is refused, so one file has one name; the ancestors are read-only.
Built: blaze-core's `Above` (made only by `Ancestors::above` on the driver's answer to
`Effect::Ancestors`, taken only by the reading `Effect::Above`) and bstd's `fs.ancestors(cx)`,
an `Ancestor` with `stat`, `read_all` and `list` and nothing that writes.

**One level of a folder is columns** (built 2026-11-16). `Effect::Entries` completes with
`Done::Entries(Box<Listing>)` (blaze-core `listing.rs`): the leaf names in one byte buffer with
`u32` ends (blaze-own's `SortedTexts`), and beside them a `Stamp` column and a `Kind` column
(file, dir, link, other, gone, unknown). It is sorted by the names' bytes, whatever order the
kernel gave (rule 25), so `find(leaf)` and `find_file(dir/leaf)` are binary searches and two
drivers give equal listings. Every driver builds it through one `ListingBuilder`: getattrlistbulk
on macOS (`fs/bulk.rs`), `read_dir` plus `statx` elsewhere, WASI, IOCP, and the simulation's
journal. The rule for what is not a name: `.` and `..` are skipped; an empty name, or one holding
`/` or NUL, is `ListingError::NotAName`; bytes that are not UTF-8 (Linux allows them) fail the
whole listing with `NotUtf8`, naming the folder and the name escaped, since blaze's names are
text; a name twice is `Twice`. The caps are 2^20 entries and 64 MiB of names, each refused by
a typed error naming the folder (`TooManyEntries`, `TooManyNameBytes`); memory comes by
`try_reserve`, refused as `OutOfMemory`. No adapter rebuilds the old pairs: blaze-vfs (`watch.rs`,
which now finds a relisted file by binary search instead of a hash map), blaze-solo, blaze-cas
`peers.rs` and blaze-storemodel read the columns. bstd's `Dir::entries(cx, path) ->
Result<Option<Entries>, FsError>` is the one-level listing for programs: `None` for a folder
not there, `NotADirectory` for a file, then by index `name(i)`, `kind(i)`, `len_of(i)`,
`path(i)`, `find(name)` and `iter()`. The recursive `Effect::List` (`Done::Listed`, pairs of
name and `Meta`) is unchanged; its callers are blaze-cas `lib.rs` and `plain/core.rs`,
blaze-certify `kept.rs`, blaze-profile `history/files.rs`, blaze-storemodel and bstd `fs.rs`.
Cost, measured by `fs/bulk.rs`'s ignored `listing_cost`: see the table under "For a successor".

A known gap, not closed by this: **a link inside the root is followed out of it.** The native
drivers open a `FileName` as the root's path joined with the name, through calls that follow
symbolic links at every component but the last (the last too, except where an effect says
`O_NOFOLLOW`), and Linux's `openat(AT_FDCWD, ..)` has no `RESOLVE_BENEATH`. So `FileName`
keeps `..` out of the text, but a link in the tree (`src -> /etc`) still reaches outside the
root. Closing it: on Linux, `openat2` with `RESOLVE_BENEATH` (and `RESOLVE_NO_MAGICLINKS`)
from the root's descriptor for every open, `statx` and directory read; on macOS, which has no
`openat2`, `openat` from the root's descriptor with `O_RESOLVE_BENEATH` (0x1000, in this SDK's
`sys/fcntl.h`, with `AT_RESOLVE_BENEATH` for the `*at` calls), or `O_NOFOLLOW_ANY` (0x20000000)
where no link at all is wanted; both are in the macOS 27 SDK.

**The disk model's gap (closed 2026-11-16 by the strict name rule; see the second hand-off's
item 3 below; kept as its history).** The simulation (blaze-core's `Sim`, and the model it refines,
`blaze_core::world::disk`) ties a rename's durability to the renamed file, not to its
directory. Two ways:
- **Stricter than a disk:** `Effect::SyncDir` makes nothing durable in it, so an unsynced
  rename whose directory was synced afterwards can still be undone by a crash. A real disk keeps
  it. `sim.rs`'s `a_rename_kept_by_its_directorys_sync_survives_a_crash` fails on purpose
  (`should_panic`) until this is fixed. This side hides no loss, but it fails correct code.
- **Looser than a disk:** a file sync that leaves no change unsynced clears the file's undo
  (`File::sync`: "nothing is left to undo"), so a crash keeps the rename. On a real disk that
  needs the directory's sync. This side can hide a bug: rename unsynced, sync the file, never
  sync the directory. The simulation passes it; a crash on APFS or XFS can put the old file
  back. The first side even pushes code toward this, since the file sync is what makes the
  simulation keep the rename.

The proved statement that rests on the loose rule is
`blaze_core.world.disk.a_crash_keeps_a_synced_file` (`crates/blaze-core/lean/BlazeCore/World/Disk.lean`).
Closing the gap means moving the undo from the file to its directory: the directory's sync
clears it, and the file's sync does not. That restates this theorem and
`a_synced_append_survives_a_crash`. The Lean sources are unchanged until then.

**For a successor** (2026-11-16, the bstd-items lane, handing off three items the coordinator
set in this order):

1. **The listing as columns.** Done (2026-11-16, see "One level of a folder is columns"; no
   `pairs` adapter was kept, by the coordinator's brief). Measured on APFS, macOS arm64, best
   of 100 interleaved runs, `listing_cost`:

   | entries | floor: one getattrlistbulk pass | columns | old pairs |
   |---|---|---|---|
   | 1,000 | 380 µs, 1 allocation | 442 µs (1.16x), 35 allocations, 58 KB kept | 470 µs, 1,010 allocations |
   | 10,000 | 7.53 ms, 1 allocation | 8.48 ms (1.13x), 47 allocations, 580 KB kept | 8.39 ms, 10,014 allocations |

   What stays above the floor: the growth of the two columns (no size is known before the
   pass; `ATTR_DIR_ENTRYCOUNT` of the folder would give one) and the sort's copy, since APFS
   does not return names in byte order. Item as first written:
   - Replace `Done::Entries(Vec<(FileName, Stamp)>)`, the one-level listing, with a sorted `Listing`. It holds:
     - `names: Vec<u8>`, all the names in one buffer;
     - `ends: Vec<u32>`, the offset where each name ends;
     - parallel `stamps` and `kinds` columns.
   - Keep the old pairs only as an adapter (`Listing::pairs`). Leave the recursive `Effect::List` as it is, and list its callers: blaze-cas `lib.rs` and `plain/core.rs`, blaze-certify `kept.rs`, blaze-profile `history/files.rs`, blaze-storemodel, and bstd `fs.rs`.
   - Port every `Entries` consumer in one batch:
     - blaze-vfs `watch.rs` and `watch/reference.rs`;
     - blaze-solo `run.rs`;
     - blaze-cas `peers.rs`;
     - blaze-storemodel `fs.rs`;
     - blaze-core: sim, `journal.rs`, and the drivers `fs/bulk.rs` (getattrlistbulk), `fs/wasi.rs` and the statx path.
   - Count the allocations of a listing of 1,000 entries before and after.
   - Then build bstd's `Dir::entries(&self, cx, path, frame) -> Result<Option<Vec<(Path, Meta), &Frame>>, FsError>`: one level, `None` when the folder is absent, sorted by name. When it lands, tell the driver lane (agent a11fa75d6a6d4471e).
   - `FileName` stays unchanged: 1313 mentions in 141 files across 17 crates, 256 fields typed `FileName`, 168 collections of it.
2. **Make the disk model strict** (see "The disk model's gap" above).
   - A create, rename or unlink is durable only once its directory is synced. A file's sync makes its bytes durable, never its name. `SyncDir` makes the names in that directory durable.
   - In code: the undo moves from `File` to its directory, in `blaze_core::world::disk` and in `Sim`'s `SimFile::undo` and `flushed`, where a file sync clears the undo today. The `SyncDir` arm in `Sim::complete` and in `world/asks.rs` then clears the undos of names in that directory.
   - Say which call the macOS driver makes for a file sync and a directory sync (F_FULLFSYNC), and that the model matches it.
   - `a_rename_kept_by_its_directorys_sync_survives_a_crash` then loses its `should_panic`.
   - Run every test that crashes the simulated disk: blaze-core, blaze-std, blaze-cas, blaze-drive's seal and `last`, the store, blaze-remote. Classify each new failure:
     - a real missing directory sync in the code under test: send the tape to that crate's lane;
     - a test that leaned on the loose rule: fix the test.
     Never weaken the model to make a test pass.
   - Report how `blaze_core.world.disk.a_crash_keeps_a_synced_file` must change (its file must also have had its name synced) and which proofs use it. Find them with `rg a_crash_keeps_a_synced_file crates/blaze-core/lean lean`. Edit the Lean only where the proofs build here.
3. **Close the symlink gap above.**
   - Every open, stat, list and rename target under a `Dir`, and under the driver's root, resolves beneath its root:
     - macOS: `O_RESOLVE_BENEATH` and `AT_RESOLVE_BENEATH`, from this SDK's `sys/fcntl.h`;
     - Linux: `openat2` with `RESOLVE_BENEATH`.
   - Probe once at start, with a real escaping link in a scratch folder, that the kernel honours the flag.
   - Where a host has neither, stop with a typed error naming what is missing. Never fall back to following links.
   - A link that stays beneath the root keeps working. One that leaves is a typed refusal naming the path and the link.
   - The simulation gets links under the same rule in `SimWorld`. Add a property test (no sequence of names, links and renames reaches outside the root) and a planted bug (the flag left off one call) that the test catches.
   - Measure 1,000 opens with the flag and 1,000 without.

State at hand-off: items 5, 7 and 8 are accepted. Their code is in the tree and passes: blaze-std "241 passed, 0 failed". None of the three items above is started.

**For a successor (2026-11-16, second hand-off, the bstd and effect-layer lane)**

1. **Listing a folder as columns: done and accepted.** On top of that, `Effect::Entries { dir }` now takes a `DirName`, and `DirName::Root` lists the driver's root. This is built in every driver, the simulation and blaze-storemodel. `Dir::own_entries(cx)` lists a `Dir` itself, whether it is the root or a named folder. `Entries::folder()` returns `Option<&Path>`. The every-driver test lists the root. Results: blaze-core 365 passed, blaze-std 252, blaze-vfs 138. The driver lane (agent af4ff0d27ddf27bfd) uses `own_entries` for its sweep's root.

2. **The two blaze-cas controls bite again.** Both had been failing since at least 2026-11-14 (the notes' port27 list). In both cases the test had gone blind; the simulation was not at fault.
   - `layers::the_oracle_catches_a_record_visible_before_its_outputs`: the outputs were put while the record's commit was still in flight. The store's commit takes puts made during it, so the record and its outputs became durable together. Fix: a `LayerOp::Settle` after the record's flush.
   - `place_crash::the_oracle_catches_a_derived_file_accepted_as_a_crash_left_it`: the test's reopen set `store.plant` only after `drive` had already run the open's sweep of `placed/`. That sweep removed the torn `.derived` file before the plant could keep it. The crash did tear the file: in 94 of 192 runs it was left torn. Fix: set the plant before the open is driven.
   - The cause was tested both ways for each fix: without it, every seed missed; with it, the control is caught.
   - blaze-cas now: 397 passed, 3 failed. The three failures are the ones that have failed on main since 11-14: `a_clone_placed_where_a_program_starts_is_prewarmed_before_its_rename`, `a_damaged_value_in_another_writers_store_is_never_served` and `a_flipped_byte_of_a_value_is_never_served`.

3. **The strict name rule: in the tree (2026-11-16) and accepted.** blaze-core: 380 passed, and
   the one failure was a test pinning the old sweep chunk, now fixed. Three sim tests leaned on
   the loose rule (their files' names were never synced). They were fixed by making each file and
   syncing its directory first; the model was not weakened. The Lean proofs over the disk world
   are owed (decision D55; storemodel.md, "The disk world's proofs under the strict rule"). The
   native calls the model matches:
   - macOS: a file's sync is `F_FULLFSYNC` (`File::sync_all`), and a directory's sync opens the
     directory and does `F_FULLFSYNC` on it;
   - Linux: `fsync` of the file, and `fsync` of the directory's descriptor.
   Miri: the strict rule is safe Rust in the simulation and the world model (`ready for Miri:
   blaze-core sim::`). No Miri run covers the native drivers' unsafe calls (kqueue, io_uring,
   `boot::system`'s sysctl). What covers them is the every-driver equality tests in `tests.rs`,
   which run each driver against the simulation on a real disk. The history of the design
   follows:
   - **The rule:** a name (create, rename, unlink) is durable only once its directory is synced. A file's sync covers its bytes, never its name.
     A directory made or removed is a create or unlink of a name of its PARENT's, durable only once the parent is synced; a directory's own sync covers the names in it, never its own (2026-11-16: `Sim` journals `MadeDir`/`RemovedDir`, `sim/host/dir_names.rs`; before, a made directory survived every crash, looser than a disk).
   - **What is in the tree:** `world/disk.rs` keeps a log of names (`Named { Put, Gone, Moved }`)
     that `sync_names()` clears, and a crash is `Cuts { files, undone }`, undone entries closed
     forward (undoing one undoes every later entry that shares a name) and undone newest first.
     `world/asks.rs` and `sim.rs` follow it: SyncDir clears its directory's entries, and a synced
     Write or Rename syncs its directory. `Plant::FileSyncKeepsName` brings the loose rule back as
     the control.
   - **Other crash suites under the strict rule:** blaze-std's `tests::syncs` oracle expected a
     `SyncDir` after a synced rename. Under the rule, the rename syncs its directories, so the
     oracle was fixed to the contract (a test leaning on the loose rule). The other suites'
     results are in the lane's reports.
   - **Lean:**
     - The statement change: `blaze_core.world.disk.a_crash_keeps_a_synced_file` gains a premise, "the file's name is not in the unsynced name log": `disk.named(file) = false`. Its `cut` becomes a `Crash`.
     - Proofs that use it: `lean/BlazeCore/World/DiskCrash.lean` (~1089), which is named in `Disk.lean:170`, and row V14 of `lean/theorems.json` (line 112).
     - A new control theorem: `a_synced_file_whose_name_was_not_synced_may_be_gone`.
     - Regenerate the model with hir-ty-lean's `blaze_cores_world_models_are_its_sources` under `UPDATE_EXPECT`. Do not hand-edit the proofs; they are built where Lean exists.

4. **Links beneath the root: the spec (2026-11-16), being built by the effect-layer lane.**
   - **One seam:** every driver holds its root as a directory descriptor, opened once. Every
     name under it is resolved relative to that descriptor with the kernel's beneath rule:
     - macOS: `openat(.., O_RESOLVE_BENEATH)`, `fstatat`/`unlinkat`/`linkat`/`mkdirat` with
       `AT_RESOLVE_BENEATH`, and `renameatx_np(.., RENAME_RESOLVE_BENEATH)`;
     - Linux: `openat2` with `RESOLVE_BENEATH`, and `IORING_OP_OPENAT2` on the ring, with an
       `O_PATH` descriptor of the parent for the calls that take no `how`;
     - Windows: refused by name until there is a design (reparse points).
     The threads driver's `at(root, name, ..)` (39 call sites) becomes `beneath(root_fd, name,
     ..)`, and a path joined to the root is used nowhere else.
   - **Probe:** once per driver, at its making. Put a link to `/` in a scratch directory under
     the root, open through it with the flag, and require `EXDEV` on Linux or the documented
     refusal on macOS. A kernel that follows the link, or does not know the flag (`EINVAL`), is a
     typed `NoBeneath { call, why }` the driver's maker returns. There is no fallback.
   - **Refusal:** a name whose resolution leaves the root completes `Done::Failed` naming the
     name and that it escaped through a link. A link that stays beneath the root resolves.
   - **The simulation:** `Sim` gets links whose targets are names, resolved under the same rule.
     The property test: no sequence of makes, links and renames lets a later effect read or write
     outside the root. The planted bug: one effect's resolution follows links freely, and the
     property test must catch it.
   - **Cost:** 1,000 opens with and without the flag, by an example under examplerun.sh.
   - **Order:** after the `Effect::Roots` wiring (item 6 below).

6. **For a successor (2026-11-16, third hand-off, the effect-layer lane at about 70% of its context).**
   - **Done, checked and accepted:**
     - the strict name rule (item 3);
     - the Boot fact: `blaze_core::Boot` and `NoBoot`, `Driver::boot()`, and bstd's `Io::boot()`;
     - `Effect::Stamp` as the batched stat: `STAMP_MOST` with a typed refusal, and `SWEEP_CHUNK`
       256. Measured at 544 µs for 870 rows, 1.09x the 0.5 ms floor (examples/stat_cost.rs).
     - The Lean debt is written down: decision D55, and storemodel.md's "The disk world's proofs
       under the strict rule".
     - blaze-std's `tests::syncs` oracle is fixed to the strict contract: a synced rename syncs
       its directories, so no `SyncDir` follows it.
   - **`Effect::Roots`: written, not in the tree.** It was reverted after other lanes' checks met
     the tree between two of its saves. Every file is kept whole at
     /Volumes/Projects/blaze-int-1112-notes/port27/roots-wip/: the files under `crates/`, and
     blaze-std's io, disk, net and layers.
     - What it does:
       - `Effect::Roots { cert_file, cert_dir }` takes `Option<String>`, so the `#![no_std]` bstd
         can name it. It completes `Done::Roots(Box<Roots<Global>>)` or `Done::Untrusted`.
       - A Unix driver runs it on a host thread named `blaze-host-roots`. Windows and WASI refuse
         it by name.
       - In the simulation, `Sim::roots` sets the roots and `Sim::roots_read` counts the reads.
       - `blaze_core::trust` loses its cfg gate, and blaze-base64 moves to the ordinary
         dependencies. Only `trust::system` stays per OS.
       - bstd gains `Net::roots(cx, env)` and `RootsError`. It does not cache: the client keeps
         the answer.
       - The test is `roots_are_the_worlds_and_each_ask_is_counted` in blaze-core's tests.rs.
     - To put it back:
       1. Check that the live files still equal `refs/green/blaze-core` and `refs/green/blaze-std`.
       2. If they do, copy every kept file into the tree in one step, then run
          `lanecheck.sh blaze-core` at once, and blaze-std after it.
       3. If another lane changed a file since, port the hunks into it by hand, then save every
          file back to back.
   - **Crash suites under the strict rule (2026-11-16):**
     - blaze-std: 249 passed, 1 failed. The failure was
       `explicit::no_unsynced_call_waits_for_the_device_and_each_synced_one_does`: the syncing
       rename's name sync was not counted as a wait. `sync_names` now counts it
       (`io.names_synced()`), and `tests::` gives 244 passed, 0 failed.
     - blaze-http: 30 passed, 0 failed.
     - blaze-cas: 381 passed, 19 failed. Three of these failed before the rule. The 16 new ones
       are all `plain::tests` crash and oracle tests, likely the plain store's missing directory
       syncs. Not classified yet.
     - blaze-vfs: 135 passed, 3 failed: `image::tests::a_stop_at_any_step_of_moving_onto_an_image_loads_what_was_durable`,
       `tests::stamps::stamps_survive_a_crash_with_the_bytes_they_vouch_for` and
       `tests::the_journal_survives_crashes_and_failures`. Not classified yet.
     - blaze-profile: 267 passed, 3 failed. These are flight-span and account tests, not crash
       tests.
     - blaze-certify is no package in this tree.
     - Not run: blaze-drive, blaze-storemodel, blaze-remote, blaze-fetch, blaze-pack,
       blaze-image, blaze-solo and blaze-cargo.
   - **Lean models: not regenerated.** `hir-ty-lean` is in the workspace's `exclude` list, so
     `mixcheck.sh hir-ty-lean` "did not match any packages". Regenerating needs it built some
     other way, which is main's call. The DiskCrash proof is owed (D55). The `lean` job turns
     red by name and blocks merges, and it never skips.
   - **Not started:** links beneath the root (item 4); the store model's blind control, and why
     the store model's suite takes more than 900 s, with a bounded run in its place.

8. **Built (2026-11-16, fifth lane): the crash tears by sectors.** The world's `Torn` is now
   `Nothing | Sectors { upto, len, landed }` (world/disk.rs; `File::after`, `mixed`); the
   simulation draws that same value in `crash_cut` (per file in name order: `kept`, then `upto`
   only where changes are unsynced; after the barriers: the length (longer, shorter or any
   between, one in three each), then all sectors landed (one in three) or each on its own) and
   applies it with the world's `mixed`. Grain: `SECTOR` (512), not 4096: finer, every 4096-grain
   outcome is one at 512, and one constant. Results: blaze-core "382 passed, 0 failed" (the
   refinement, the reserve's `a_crash_at_any_point_leaves_no_record_or_the_whole_one` and its
   plant among them); bstd `tests::` "244 passed, 0 failed"; blaze-cli `store::tests` "10 passed,
   0 failed" with both plants un-ignored (were "8 passed, 2 failed"). Oracles that assumed a
   prefix were fixed (not code): sim.rs `a_write_at_an_offset_is_durable_after_its_sync_on_every_seed`,
   bstd tests/crash.rs and tests/durable.rs (through `blaze_core::sim::appended_and_torn`).
   The prefix theorem is replaced (D55, storemodel.md "The crash tears by sectors"); Lean owed.
   The roots test's count is now the value's (`Roots::reads`). blaze-storemodel does not use
   blaze-core's disk (its own `CrashFs`, still prefix-only: its own debt); its full suite times
   out at 900 s, so run it with a filter. Still open from item 7: plain/core.rs's `SyncDir` fix.

7. **For a successor (2026-11-16, fourth hand-off, at the budget).**
   - **`Effect::Roots` is in the tree.** The kept files were copied in one step. Two holes
     were fixed at once: `trust::Roots` now has a hand-written `PartialEq`/`Eq` (a derive asked
     for `A: PartialEq`, which `Global` lacks), and bstd's `pending.rs` has the arms
     `Done::Roots`/`Done::Untrusted`. blaze-storemodel's fs.rs refuses `Effect::Roots` by name.
     The checks: blaze-core, blaze-std, blaze-http, blaze-fetch and blaze-storemodel each "check
     ok (all targets)". blaze-cli is unverified: blaze-consteval (`Lit::CStr`, not this lane's)
     does not compile under it. `lanecheck.sh blaze-core test roots` failed one test:
     `trust::tests::this_macs_store_gives_roots_and_is_read_only_when_asked` asserts "left: 2,
     right: 1" on the process-wide `macos::reads()` counter. The likely cause, not yet shown by
     a run, is that the new roots test read the system store in parallel with it. The fix to
     make: count reads per call (return the count from the read) or serialize the two tests.
     Never loosen the `+ 1` check. blaze-std was red for about 20 minutes between the copy and the
     pending.rs fix. The WIP had missed that file, and the checks after blaze-core had not been
     run back to back with it. blaze-cli's check, the roots test and the message to the driver's
     lane were in flight at hand-off: run `lanecheck.sh blaze-cli` and `lanecheck.sh blaze-core
     test roots`, then tell the driver's lane (OWNERS.md), in one message, that the client can
     take its roots through `Net::roots`.
   - **FIRST, owed (the coordinator's order): the crash is too kind to unsynced data.** The
     pack store's plants were un-ignored. Neither was caught: "the planted VerifySkipped was not
     caught in 5560 starts", and the same for SelfVouching. A cut tears only the NEXT change, and
     an append only to a prefix (`Op::torn`; the world's `Torn::Prefix`), so a cut pack always
     loses its footer and the open refuses it before verifying. POSIX leaves unsynced data after
     a crash unspecified (`fsync`, `write`: no ordering or atomicity promise without a sync).
     ALICE (Pillai et al., OSDI 2014) observed real file systems reorder and drop unsynced
     block writes, and ext4 `data=writeback` and XFS leave zero-filled blocks under a grown
     size. Both plants are re-ignored with that reason. The design to build:
     - Grain: the 4096-byte block (`SECTOR`), the device and page-cache unit that writeback
       flushes as a whole.
     - A `Cut` keeps `kept` (the changes whole, in order, as now) and draws `upto` in
       `kept..=ops.len()`.
     - In `crash_with`: let `old` be the file after `sync(kept)`, and `new` be `old` with ops
       `kept..upto` applied.
     - The length is drawn between `old.len()` and `new.len()`.
     - Each block where they differ is, on its own, `new` or `old`, or zeros where the block
       starts at or past `old.len()` (newly allocated). Synced bytes never change.
     - Barriers: a file's reach is `upto`, not `kept + 1`. When a barrier raises `kept`, set
       `upto = upto.max(kept)`; do not clear it.
     - What the change must reach beyond sim.rs, in one save: the disk world's `Kept`/`Torn`
       (world/disk.rs: `fits`, `reach`, `keeps`, `crash`), the refinement's `kept()`
       (sim/world.rs:540), asks.rs:835/861, and the Lean statement
       `a_crash_cuts_an_appended_file_to_a_prefix`, which becomes false as stated. Its
       replacement is "synced bytes and synced length survive". Record it with D55.
     - Then un-ignore the two plants in crates/blaze-cli/src/store.rs and quote the lines.
       Rerun blaze-core's and bstd's crash suites and classify each new failure (code or
       oracle), with no oracle weakened.
     - It was not started: it is a change to the spec the proofs state, too large to write
       whole and check at once at the end of this budget.
   - **The new path's crash suites under the strict rule (each line quoted from a run):**
     blaze-drive "36 passed, 0 failed", blaze-fetch "12 passed", blaze-git-fetch "12 passed",
     blaze-build-script "36 passed", blaze-toolchain "5 passed", blaze-remote (bare) "6 passed",
     and blaze-proc-macro "48 passed". The proc-macro run was through mixcheck, because
     blaze-scope's resolve.rs did not compile. None of these crash suites runs on blaze-core's
     simulated disk, so the rule changes none of them. The pack store's suite (blaze-cli
     store::tests) gives "8 passed, 2 failed" with the plants un-ignored: the failures are the
     two plants above.
   - **Where each crate's on-disk state is written, and on which simulation (for main):**

     | Crate: state | Writes through the effect layer? | Crash test runs on | Never read torn? |
     |---|---|---|---|
     | **blaze-proc-macro: expansion records (`record::DirStore`, used on the binary's path by blaze-world's `ProcMacros`)** | **no: ambient `std::fs::write` + `rename`, no sync** | **none** | **no: no whole-record digest; a torn note can decode as other tokens** |
     | blaze-proc-macro: compiled modules + seals (host.rs `write_renamed`) | no: ambient std::fs | none | yes: the seal's digest is checked on every read; a mismatch is removed and rebuilt |
     | blaze-cli: the pack store, boot record, `last` | yes (bstd `publish`, `Durable::No` by decision) | blaze-core's disk | yes in design, unproven: the model cannot tear a pack's middle |
     | blaze-fetch / blaze-git-fetch: source records | yes (`blaze_core::reserve`: synced rename) | its own machine model (a BTreeMap store, crash at an effect: no tearing); the reserve's crash proof is on blaze-core's disk | yes |
     | blaze-build-script: script records | through proc-macro's script.rs: ambient `std::fs::rename` (script.rs:376) | none on a disk | not checked |
     | blaze-toolchain: the shelf | read-only on the binary's path; written by the `blaze-toolchain` bin (ambient std::fs) | none | yes: BLAKE3 checked against the name |
     | blaze-remote: local write-through | in-memory model in read/tests.rs | its own model | per its oracle |
     | blaze-world: names records | not found written by the driver in this tree | none | n/a |
     | blaze-storemodel | models the OLD `blaze_cas::Store` on its own `CrashFs` (ALICE: prefix plus last torn; POSIX entries as an option): no middle tear | its own | finding 2 (POSIX entries) stays ignored |
   - **Step 3 (old layer), from searches:** blaze-vfs and blaze-profile are used by neither
     blaze-remote nor blaze-storemodel: old layer; deleted at the switch; they fail 3 tests
     each under the strict rule (vfs: three crash tests; profile: not crash tests): not fixed.
     blaze-remote's binary shape uses only blaze-cas `plain`'s pure format code (`Namespace`,
     `seal_segment`, `check_segment`, `find_many`). Only `cmd` (feature `sign`: the pusher and
     `blaze cache`) opens the disk store `plain::Driven`.
     - The 16 `plain::tests` failures are, by reading, a CODE fault: plain/core.rs makes its
       head, segments and manifest logs by `WriteAt`/`Truncate`, syncs each file, and never
       emits `SyncDir`, so under the strict rule a head can name a segment whose name a cut
       lost.
     - The fix: in durable mode, one `SyncDir` of `cfg.dir` after `LOG_SYNC`, before
       `write_slot`. It covers every new segment and the log, since all are in one directory.
       Add one more after the head's first sync (`HEAD_MAKE` index 1).
     - The cost: one directory fsync per flip, plus one at the store's birth.
     - Not yet applied or run.
   - **Lean models, for main (do not build here):** `blaze_cores_world_models_are_its_sources`
     (crates/hir-ty-lean/src/tests.rs:1797) writes `lean/BlazeModel/blaze_core/world/{clock,
     disk}/…` when `UPDATE_EXPECT=1`.
     - hir-ty-lean and its deps hir-ty, hir-ty-borrowck and hir-ty-mir (dev) are all in the
       root Cargo.toml's `exclude`, and their manifests inherit `workspace = true`. So
       `--manifest-path crates/hir-ty-lean/Cargo.toml` cannot load them: an excluded package
       finds no workspace to inherit from.
     - The way: on a Lean host, in a scratch worktree, delete those four lines from `exclude`
       (`members` is `crates/*`, so they become members). Then run
       `UPDATE_EXPECT=1 cargo test -p hir-ty-lean blaze_cores_world_models_are_its_sources`.
       Add the asks world's `a_synced_append_survives_a_crash` to that test's list first
       (storemodel.md). Then build the proofs with `lake build` under
       crates/blaze-core/lean.
     - Unverified: whether hir-ty still compiles after the old layers' deletion (8e87a017ff).
       If it does not, the generator has to move off hir-ty first.

5. **What other lanes need from the effect layer:**
   - The fetch area (now `main`'s; lane aee48fa9980e2272a is retired) is adding `Effect::Reserve { key, room }` to blaze-core, in every driver and the simulation: a large record written into a mapped file, visible only on finish. Its crash proof depends on the strict name rule, because finish ends in a directory sync.
   - Agreed with that lane:
     - the mapping's bytes fold into `SimFile` as ordinary ops (`push_op`), so the crash cut tears them, with no second durability path;
     - finish uses a synced `Rename` alone, or `RenameUnsynced` plus `SyncDir`, never both.

With records, `publish(path, content, { durable: Durable::No, .. })`; `durable` stays a field
with no default.

## Values that must be finished

bstd is written with linear types ([linear](linear.md)): a `#[blaze::linear]` value cannot be
dropped or forgotten, only handed to a function that takes it by value. Rust's std cannot use
them (its generics would have to change), and much of its shape is a workaround for that. bstd
does not need the workarounds.

- **A scope is a value, not a closure.** `let s = cx.scope(); s.spawn(..); s.join(cx)?;`. Rust
  needs the closure form because a guard can be leaked, and a leaked guard would let a task
  outlive what it borrowed. A linear `Scope` cannot be leaked, so the code is straight-line and
  an early return has to go through `join`. The same holds for a GPU launch scope. The closure
  forms remain as conveniences.
- **An operation in flight borrows its buffer.** With completion-based IO the kernel writes into
  a buffer after the call returns, so today's libraries make every read take an owned buffer,
  since a forgotten future would free memory the kernel still writes. A linear
  `Pending<'buf>` cannot be forgotten: it must be `wait`ed or `cancel`led (which also waits), so
  `file.read_into(cx, &mut buf)` on a borrowed buffer is sound and zero-copy.
- **Closing is a call that can fail and can wait**: `file.close_synced(cx)?`,
  `conn.finish(cx)?`, `vm.stop(cx)?`. A handle with unsynced writes, a buffer with unwritten
  bytes, a connection with unsent data, a sandbox and a transaction are linear, so their last
  error cannot be lost in a destructor and nothing has to wait inside `Drop`.
- **A task's result must be taken**: `Task<T>` is linear and `join` returns its outcome, so a
  failure cannot be ignored by dropping the handle.
- **On a panic** the value's abandonment path runs instead (roll back, cancel and wait, close
  without syncing), as [linear](linear.md) says; that is the one place a destructor of these types
  runs.
- **Under rustc**, which has no linear types, each must-finish type holds a guard
  (`crate::unfinished::Unfinished`) that its finishing calls disarm. In a debug build dropping
  one unfinished stops the program, naming the type, the call that made it (`#[track_caller]`
  back to the program's line) and how it is finished, unless the thread is already unwinding. A
  release build of rustc's keeps no guard: an unfinished value there still runs its abandonment
  path silently, losing what it held, and only blaze's build refuses it. The guarded types:
  `Pending`, `local::End`, `term::RawMode`, `net::Listener`, `net::Conn`, `streams::Stream`,
  `task::Scope`, `task::Task`, `power::Awake`, `requests::Exchange`, `fs::Buffered` and
  `fs::File<ReadWrite>` (through its `ReadWrite`). `own::Shares` is linear but unguarded: a
  dropped share leaks its value's memory and loses no data.
- **bstd's containers say whether they admit linear elements.** `Vec`, `Option`, `Result` and
  the maps do (and are then linear themselves). `Shared` and `LocalShared` do when the value's
  type is `Acyclic`, which is derived from its shape, so no cycle can leak one
  ([ownership](ownership.md)); `Shares` always does. There is no `forget`.

## Eval: who decides how a pure function is computed

A call to a pure function equals its value, so *how* the value is obtained is free to choose:
run it, look it up, fetch it from another machine, record where it came from. bstd makes that
choice a value the program passes, like every other.

```rust
#[blaze::memo]                      // a pure function chosen as a point the evaluator sees
fn layout(points: &mut Nested, ty: &Type) -> Layout { .. }

#[blaze::memo]
fn size_of(points: &mut Nested, s: &Struct) -> u64 {
    s.fields.iter().map(|f| points.call(layout, &f.ty).size).sum()   // a nested point
}

fn main(cx: &Cx, mem: Mem) -> Result<(), Error> {
    let eval = Eval::memo(&mem, limit)?;    // or direct(), stored(..), handler(..)
    let size = eval.call(cx, size_of, &my_struct);
    ..
}
```

A memo point's first parameter is `&mut Nested`, through which it calls other points: a point is
pure, so it cannot hold the evaluator itself, and `Nested` is what carries "who called whom".
`Eval` is an ordinary parameter, passed down like `cx`. Calling `layout(..)` directly still
works and simply runs it.

- **A memo point is a pure function marked `#[blaze::memo]`.** `eval.call(f, args)` hands the
  evaluator the point's code identity, the arguments, and a way to
  compute. Purity is what makes this sound: the arguments are everything the result depends on.
  Only marked functions pay for it; an ordinary pure call is an ordinary call.
- **The key is derived, not written**: the function's code identity plus its arguments, by
  reflection ([reflect](reflect.md), items 1 and 5). A changed body is a different key.
- **`Eval` is one type with providers**, as `Http` is:

  | provider | does |
  |---|---|
  | `Eval::direct()` | calls the body; with this one the marks compile to plain calls |
  | `Eval::memo(mem)` | remembers results in a cache of the memory budget |
  | `Eval::stored(mem, store)` | the above, kept in the store and the remote cache: the engine blaze builds with ([kernel](kernel.md)) |
  | `Eval::handler(h)` | yours |

- **A handler sees every point and can wrap another evaluator**, so behaviours stack: record
  provenance over a stored evaluator, count and time over a direct one, send some points to
  another machine. `h.run(point, args, next)` returns the value; what it does around `next` is
  its business, and because the functions are pure it cannot change any answer, only where it
  comes from.
- **Provenance is what an evaluator already knows.** While a point runs, the points it calls are
  its inputs, so the evaluator holds the whole graph of what was computed from what. `blaze why`
  is that graph for the compiler; a program gets the same for its own values by choosing an
  evaluator that keeps it.
- A library that marks points takes `&Eval` where it calls them; a caller with no opinion passes
  `Eval::direct()`.

This is the third place bstd lets a program supply its own machinery, after the host behind `Io`
(the boundary) and a capability's provider (`Http::handler`). None of them is a compiler plug-in:
each is a value passed down, which is why they compose and why a test can swap any of them.

## Sandboxes: the one way to run a program

There is no "start a process" call. A program runs in a sandbox, and a sandbox is a value with a
definition, so what a command could see and touch is written down before it runs.

The three ways to isolate take different things and can do different things, so they are three
types, each with exactly the spec that is valid for it and the operations it really has:

| type | made with | the program is | besides running, it can |
|---|---|---|---|
| `OsSandbox` | `Dir`s at chosen paths, an `Env`, a net policy, limits | a host process behind the OS's sandbox (Landlock, seccomp and namespaces; a sandbox profile on macOS; AppContainer on Windows) | signal a process, see its resource use |
| `Vm` | an image or a view of the file tree, cores and memory from the host's menu, a lifetime, a net policy | a guest in a micro VM | stop, resume, snapshot, fork |
| `WasmSandbox` | a module, `Dir`s as preopens, an `Env`, fuel and memory limits | a WASI module in the runtime | call an export, meter fuel |

```rust
let sb: OsSandbox = io.sandboxes().open(cx, &OsSpec {
    dirs: &[(path!("src"), &src.read_only()), (path!("out"), &out)],
    env: &env.only(&[var!("CC")]),
    net: NetPolicy::None,
    limits: Limits { cores: 4, memory: 2 << 30, wall: Duration::from_secs(60) },
})?;
compile(cx, &sb.runner())?;

// Runs anywhere a command can be run; cannot tell which kind it was given.
fn compile(cx: &Cx, run: &Runner) -> Result<(), Error> {
    run.run(cx, &Command::new(prog!("cc")).args(&["-c", "main.c"]))?; ..
}
```

- **One `open`; the spec's type picks the sandbox's type.** Each spec names its sandbox
  (`trait Spec { type Sandbox; }`), so `open(cx, &VmSpec { .. })` returns a `Vm` and
  `open(cx, &OsSpec { .. })` an `OsSandbox`. A spec for one kind cannot hold a field of another,
  and an operation a kind lacks does not exist on its type.
- **`Runner` is what they share**: run a command, get its status and its output streams. It is
  the higher capability each of the three can make (as `Net` makes `Http`), one concrete type
  with the provider inside. Code that only runs commands takes `&Runner`; code that snapshots
  takes `&Vm`. Nothing asks a sandbox at run time what kind it is.
- **A spec holds only capabilities and limits**, and never more than its maker held. The file
  system is the `Dir`s handed in; the environment is an `Env`; the network is nothing, a narrowed
  `Net`, or a proxy the parent runs for host-level rules.
- **A host that cannot build the isolation refuses**, naming what it could not enforce. Which of
  the three a host offers is known at start, like any capability.
- **A sandbox is reused.** Making one is the expensive part; a run is cheap, so a build makes one
  per kind of work and runs many commands in it. It ends with its scope, like a task.
- **Every run is recorded.** The sandbox traces what the program read and wrote, so a run is a
  function of its command, its spec and the content it read. That is a cache key: the same
  command over the same inputs is a store hit ([caching](caching.md)), which is how blaze already
  treats build scripts. A `Vm` over a view of the Merkle tree ([vfs](vfs.md)) gives its output as
  the difference between two roots.
- **A bstd program inside one** gets its grants made from the spec: its `main` is handed the
  ones it names and no others. Any other program sees an ordinary system that happens to contain
  only what was granted.

This is one design for what blaze has three of today (the build-script sandbox, the proc-macro
host, `ix` VMs for lanes), and the shape of wasm-host's sandbox ops (a spec, a handle, `exec`,
events) with capabilities as the spec.

## Gpu, video and images

**`Gpu` is a capability.** A program can use a graphics device only if it was handed one, like a
socket. It is the `Device` of the GPU plan ([gpu](../../../future/gpu/plan.md)) moved under `Io`:

- `io.gpu()` is `None` on a host with no device; `Gpu::cpu()` is always available and runs the
  same kernels as host code, which is what tests and the simulation use.
- **Running Rust on it is the GPU plan's job, not bstd's**: a `#[gpu::kernel]` function in the
  same crate is compiled by blaze for Metal, Vulkan, CUDA and the CPU, and
  `gpu.scope(cx, |s| s.launch(&kernel, grid, args))` is type-checked against its signature. bstd
  supplies what that plan called the runtime crate: the device, `Buffer<T>` made in an allocator
  the caller hands over and counted against the memory budget, the launch that goes through the
  one wait (so it has a deadline, and host buffers stay borrowed until the device finishes), and
  offscreen render targets.
- Narrowing: `gpu.only(&[kernels])`, and a smaller memory budget.
- The providers are Metal, Vulkan, CUDA and the CPU, one `Gpu` type, chosen as any provider is.

**Getting data to the device, and unified memory.** A kernel's arguments are `Buffer<T>`s, never
plain slices, so a copy is never hidden. What a buffer costs depends on the machine, and the same
code is right on both kinds:

| | unified memory (Apple silicon, integrated GPUs) | separate memory (a discrete card) |
|---|---|---|
| `Vec::new_in(gpu.alloc())`, `gpu.buffer(n)` | page-aligned memory both processors address | pinned host memory, with device memory behind it |
| the host writes it (`buf.host_mut()`) | writes the bytes the device will read | writes host memory and marks the range dirty |
| `launch` | no copy | uploads the dirty ranges first |
| the host reads a result (`buf.host(cx)`) | ready at once, no copy | a wait: downloads what the device changed |
| a file read into it | disk to device-visible pages, no copy | then an upload |

- **Allocate where the data will be used.** Because every collection takes its allocator, data
  built in `gpu.alloc()` is a device buffer from its first byte. That is the whole optimization
  on unified memory: nothing is ever copied in either direction.
- **The borrow checker carries the rule that makes it safe on both.** A launch borrows its inputs
  shared and its outputs uniquely until the device finishes (the scope), so the host cannot read
  a buffer the device is writing. On unified memory that prevents a race; on separate memory it
  is the same rule, and it is why the dirty ranges are always known.
- **Reading back is a wait** (`host(cx)`), so it has a deadline everywhere and costs nothing where
  nothing needs copying.
- **`Buffer<T, DeviceOnly>` has no host view at all**, for intermediates between kernels. On a
  discrete card it never crosses the bus; on unified memory the driver may keep it private.
- **Which kind a device is**: a constant where the target decides it (Metal on Apple silicon, so
  the dirty tracking compiles out), otherwise `gpu.memory()` at run time for a program that wants
  to pick a strategy, such as keeping data on the device across many launches.
- `gpu.upload(cx, &slice)` is the explicit copy from ordinary memory, for data that already
  exists somewhere else.

**A codec is a sans-IO core.** `bstd::video::Encoder` takes frames and returns packets;
`Decoder` the reverse; a `Muxer` takes packets and returns the bytes of a container. None of
them takes a capability or reads a clock: frame times are data. Where the work is done is a
provider:

| provider | made from | |
|---|---|---|
| the platform's hardware encoder (VideoToolbox; VA-API and NVENC later) | a `Gpu` | fast, HDR; what the agent app's video tool uses today |
| a software encoder | nothing | anywhere, including the simulation |

Writing a file is the caller's: packets go to a `File` it holds. So rendering a video is
`Gpu` renders a frame into a buffer, `Encoder` turns frames into packets, `Muxer` into bytes,
`File` takes the bytes, and each step can be tested without the others.

Both are behind bstd features, `video` and `image` (above, "What a program pays for at start").
**Known failing (2026-11-16):** `video::videotoolbox::tests::the_media_engine_encodes_frames_into_packets_an_mp4_takes`
fails on macOS with `the muxer takes the packet: Order { sample: 3, why: "it is presented before
it is decoded" }`: the hardware session hands a packet whose presentation time is before its
decode time, and the muxer refuses it. Run it with `LANECHECK_FEATURES="image,video"`.

**Images are functions.** `bstd::image::{decode, encode}` take bytes and an allocator and return
pixels, or the reverse, with a size limit on decode since input is untrusted. PNG first; no
capability, no IO.

## Sound, and the device capabilities

The rule for hardware: **the capability is in bstd whenever it is authority a program should not
have by default; the implementation is in bstd only when most programs need it and it is the same
on every target.**

**Sound is in.** `io.audio()` gives `AudioOut` and, separately, `AudioIn`: a microphone is as
sensitive as the file system, so listening is its own capability, narrowed to a device.

- A device is a provider (CoreAudio, PipeWire or ALSA, WASAPI, the browser's in a wasm host;
  silence, a buffer or a file in the simulation), one type each for input and output.
- The stream is pulled by the device. The callback that fills or reads a buffer must be
  `nonblocking` and `noalloc`, checked, since the audio thread may never wait; it talks to the
  rest of the program through a bounded queue.
- Mixing, resampling and codecs are sans-IO cores, samples in and samples out, like `video`'s.
  Sample times are data on the injected clock, so a simulated run produces exact samples a test
  can compare.
- Kept small: devices, PCM buffers, a mixer, resampling, one lossless and one lossy codec, and an
  audio track in the MP4 muxer. Synthesis, effect graphs and spatial audio are libraries.

**Device capabilities** have a type and ops on the boundary in bstd, so that code cannot reach
them unasked and a host can say it has none, and nothing more. Their protocol logic lives outside
bstd as sans-IO cores over the capability.

| capability | narrowed to | why it is authority |
|---|---|---|
| `AudioIn` | a device | listens |
| `Camera`, `Screen` | a device, a window | watches |
| `Location` | a precision | says where the user is |
| `Bluetooth` | a device, a service | scanning reveals location; pairing reaches other devices |
| `Usb`, `Serial` | a device | reaches hardware directly |
| `Clipboard` | read or write | holds whatever the user last copied |
| `Notify` | | interrupts the user |

## The rest of the set

The test for a capability: could code use it to learn or change something outside its own values
that its caller did not hand it? Everything an OS or a browser lets a process do passes through
one of these, so "what can this function reach" always has an answer.

**Roots, beyond those above.**

| capability | from | narrowed to | gives |
|---|---|---|---|
| `Term` | `io.term()` | | the terminal as a device: size, raw mode, cursor and colour, key events. `Out` is only bytes |
| `Keys` | `io.keys()` | a named key | the OS keychain and hardware keys: sign, decrypt, fetch a `Secret`. A key in hardware never leaves it |
| `Roots` | `io.roots()` | a set of authorities | whom TLS trusts. Chosen explicitly, so no connection trusts the machine's store unasked |
| `Locale` | `io.locale()` | | time zone, language, formats. Ambient and different per machine, so a deterministic program takes it or does without |
| `User` | `io.user()` | | who is running: name, home, ids. Not in `Env`, so reading variables does not reveal it |
| `Procs` | `io.host().procs()` | a process | other processes: list, signal, inspect. Separate from one's own exit and pid |
| `Code` | `io.code()` | a module | make memory executable, load native code. What live patching and a JIT hold; nothing else can turn data into instructions |
| `Window` | `io.windows()` | a window | a surface to draw on and the input sent to it. Input to *other* windows is `Automation` |
| `Automation` | `io.automation()` | an application | drive other applications: synthesize input, read their UI |
| `Opener` | `io.opener()` | a scheme | hand a URL or a file to the user's default application |
| `Power` | `io.power()` | | keep the machine awake, read the battery |

**Layered**, each made from a lower one or offered by the host directly, as `Http` is:

| capability | made from | or from a host that offers |
|---|---|---|
| `Http` | `Net`, `Clock`, `Roots` | requests |
| `Streams` | `Datagrams`, `Clock`, `Roots` (`quic`) | WebTransport |
| `Resolver` | `Net` | name lookup alone, for code that should resolve and not connect |
| `Local` | a `Dir` (a socket's path), or a pair made in memory | pipes and local sockets between processes |
| `Kv`, `Blobs` | a `Dir` | a key-value or object store, which is all a wasm host may offer in place of files |
| `Shared` memory | `Mem` | a region two processes map, handed to a child through its sandbox |
| `Runner` | a sandbox | running a command |
| `Eval` | `Mem`, a store | computing or recalling a pure function |

**Budgets ride the same way.** Cores and time are in `Cx` (a scope is given a share of workers and
a deadline), memory in `Mem`, disk in a `Dir`'s quota, network in a `Net`'s byte and rate limits,
device memory in `Gpu`. A capability with a limit is narrower than one without, and exceeding it
is a typed outcome, never a stall.

**Deliberately not capabilities:** panicking and purity (effects of the code, with nothing to
hand over); `unsafe` and foreign calls (the trusted list); arithmetic, formatting and the
collections (they reach nothing).

## Shared mutation

One naming rule: **the plain name is safe across threads; `Local` marks the cheaper version for
one thread.** Docs and diagnostics say "shared" and "unique" for `&` and `&mut`, which is what
they mean.

| bstd | Rust's std | |
|---|---|---|
| `Shared<T>` / `LocalShared<T>` | `Arc` / `Rc` | both are `Handle`s ([edition](edition.md), change 1); over an `Acyclic` type they hold a linear value and are released by hand ([ownership](ownership.md)) |
| `Shares<T, K, N, D>` | none | `N` of `D` parts of one `T`, the fraction in the type, no count at run time |
| `Lock<T>` / `LocalLock<T>` | `Mutex` / `RefCell` | the first waits, the second panics; the same operation |
| `RankedLock<T, R>` | none | a lock in a rank order the compiler checks (below) |
| `RwLock<T>` | `RwLock` | demoted: prefer `Swap<T>` (below) |
| `PublishOnce<T>` / `LocalOnce<T>`, `LocalLazy<T>` | `OnceLock` / `OnceCell`, `LazyCell` | `Once` and `Lazy`, which wait on first use, come when bstd's own wait is under them |
| `Counter`, `Flag`, `Seq`; `sync::raw::Atomic<T>` | `AtomicU64`, `AtomicBool`, ... | one type per use, its ordering chosen inside; the generic one under a longer name |
| `Cell<T>` | `Cell` | unchanged |
| `RawCell<T>` | `UnsafeCell` | trusted code only |

- **Or none.** Tasks that only hand each other `Flat` values (`bstd::flat`: no pointer,
  reference, cell, capability, allocator or machine-local id inside) share no memory, so there
  is nothing to lock between them: a flat channel (`chan::bounded`) is the default way two
  tasks talk, and every row below is for state that must be shared.
- **Which sharing.** Borrow from a scope when the owners' lifetimes nest: the default, and it
  costs nothing. `Shares` when the number of owners is fixed and known as the crate compiles:
  each holds a fraction, any part reads, the whole writes or takes the value out. `Shared` over
  an acyclic type when the number is decided at run time: a count, and the last `release` hands
  the value back.
- **These are the named locks of rule `standard-structures`**, in their one home: a `Lock` is
  made with its name, records who waited behind whom, and its `lock(cx)` is the one wait, so it
  has a deadline and can be cancelled. The removed names are banned by lint.
- **No poisoning.** `lock` returns the guard, not a `Result`. A panic while holding a lock fails
  that task and the lock unlocks.
- **`Freeze` is a public bound**: no shared mutation anywhere inside the type. `&T where T:
  Freeze` is truly immutable, which `pure` and every data-flow analysis want to say. A `static`
  must be `Freeze` (the `global_state` check above); the only way around it is the trusted list,
  not a mark on the item.
- **Token cells.** `TokenCell<T, K>` holds data; a `Token<K>` is the permission to touch every
  cell of its kind. `cell.get(&token)` and `cell.get_mut(&mut token)` are checked by the borrow
  checker, with no run-time flag and no panic. Put the token behind one `Lock` and that lock
  guards a whole graph of cells. It is the idea of the rest of bstd: authority is a value you
  pass. `LocalLock` stays for what a token cannot express.
- **Projection through cells** (`&Cell<Struct>` to `&Cell<Field>`) is a library method built on
  reflection ([comptime](comptime.md)) before any syntax is considered.
- Kept from Rust: a lock owns its data, the `Send`/`Sync` split, and shared mutation as an
  opt-in per type.
- **Moving a `Lock` to a `RankedLock`.** A lock that is ever held with another gets a rank:
  declare `enum StoreRank {}` with `impl Rank { const LEVEL: u32 = 10; const NAME = "store"; }`,
  change the type to `RankedLock<T, StoreRank>` and each `lock(cx)` to `lock(&mut order)`, with
  `order` from `cx.lock_order()` or a lower guard's `order()`. A call site that took two locks
  in an order the ranks forbid stops compiling, which is the point; fix it by the ranks, never
  by a third lock. A lock never held with another stays a `Lock`. The move is by name, through
  a `.clippy.toml` ban on `Lock` in a crate once its locks are ranked (rule 13).

## Concurrency: what to reach for

bstd offers a primitive named for each pattern of parallel or shared work, so code reaches for
the pattern, not for a lock and an atomic. What a pattern does not need is absent or awkward on
purpose. All live in `bstd::sync`, except `par`.

| you want to | use | instead of |
|---|---|---|
| work on parts of one slice, or columns of one table, at once | `par::for_each`, `par::map_into`, `par::reduce` | threads over `Arc<Mutex<Vec<_>>>`, rayon |
| build a table, then share it read-only | `Building<T>` then `.freeze()` to `Frozen<T>` | `Arc<RwLock<T>>` read for ever |
| readers that never block, a writer replacing the whole value | `Swap<T>` | `RwLock<T>`, `ArcSwap` |
| compute a key once while other askers wait | `Flight<K, V, E>` | a map of `OnceLock`s, a lock held over the computation |
| hand values from task to task | `chan::bounded::<T: Flat>`: a `Chan` of flat messages | an unbounded channel; `Arc<Mutex<_>>` messages |
| hand over what a value owns (a buffer, a handle, a capability), in this process | `chan::owned`: an `OwnedChan` | the same, unbounded |
| run a producer and its consumer at once | `scope.must_overlap` for one of them | `may_overlap`, which runs it inline into its deadline on one worker |
| count, flag, number versions, publish one value | `Counter`, `Flag`, `Seq`, `PublishOnce<T>` | `AtomicU64` with an ordering chosen at each call |
| hold two locks at once | `RankedLock<T, R>` | two `Lock`s and a comment about their order |
| state for one thread | `LocalLock`, `LocalOnce`, `LocalLazy`, `LocalShared` | `thread_local!` |

**Guarantees and costs.**

- `par`: chunks are disjoint `&mut` ranges cut by `split_at_mut`, so the borrow checker proves
  no two overlap; where the cuts fall depends on the length and the grain alone, and `reduce`
  joins neighbours left to right by position, so the result is the same in every schedule and
  layout, for a join that is neither associative nor commutative too. The grain (`Grain`: the
  fewest items a chunk is worth, and each chunk's frame) is the caller's. Cost: one task and
  frame a chunk; `n / items` chunks, and the calling task runs one itself.
- `Frozen<T>`: `T: Freeze`, so nothing inside can change; a clone is a count, a read is a
  pointer, no lock. One allocation at the freeze, and no way back to a `Building`.
- `Swap<T>`: a read is an acquire load inside a reclamation task (`reclaim::enter`), never a
  wait; a publish takes the writers' lock, swaps the pointer and holds the old value until every
  task that was in when it was replaced has left (blaze-reclaim's epochs). Cost: a writer
  allocates the whole new value; readers pay nothing but the task's entry.
- `Flight<K, V, E>`: at most one computation of a key at a time; waiters wait under their own
  deadlines; a failure or a panic fails its waiters with a typed `FlightError` and leaves no
  entry, so the next asker retries; the table holds at most its capacity, a full one replacing
  its oldest finished key or refusing with `FlightError::Full`. Cost: a scan of the table under
  its lock per ask (a hashed core is the same interface for large tables), a clone of the value
  per asker.
- `Chan<T, A, M>`: its capacity is an argument and its ring of that many slots is allocated
  when made; `send` and `recv` take `cx` and end in `Stopped` at its deadline, a send giving
  its value back; closed is a typed outcome after what is left is drained. One type, one ring,
  told apart by `M` (`Copies` from `chan::bounded`, `Moves` from `chan::owned`): only the
  constructors differ. Cost: a lock, a copy into or out of a slot and a notify per value; no
  allocation and no reference count per message.

**Why messages are flat by default** (the idea is Goose's: each thread its own program, and
only flat values through typed queues, so data races, locks and atomics do not exist between
them). `Flat` is derived by the same walk as `Acyclic` (`AnyAddress` finds a raw pointer,
reference or function pointer, an owning or counted pointer, a cell, a capability, an
allocator, a `usize`, or a type that says it is a machine-local id by `impl Address`; and drop
glue is refused), so the bound costs nothing at run time. Three things follow:

- **A flat message crosses to a remote worker unchanged.** Remote execution is the same driver
  with another executor; a message whose bytes are all of it is already what the wire carries,
  so the code that sends to a local task sends to a remote one. An `OwnedChan`'s message, a
  `Box` or a handle, cannot leave the process at all.
- **No atomic reference counts on 128 cores.** Sending an `Arc` per job makes every receive
  and every drop an atomic on a shared line; a flat message is copied into a slot and out
  again, and the lines it touches are the ring's.
- **No shared mutable state by accident.** A flat value holds no cell and no pointer, so the
  receiver cannot write anything the sender reads; the sharing that remains is the rows above,
  each named.

`chan::owned` is the right one when the receiver must own memory or authority the sender made
and moving it beats copying it: a large buffer filled in the sender's allocator, a narrowed
capability handed to a worker task. Prefer a flat key or index into data borrowed from the
scope.
- `Counter` (relaxed: a total publishes nothing), `Flag` (release set, acquire read), `Seq`
  (acquire/release steps), `PublishOnce<T>` (set once, never waits; a second set gets its value
  back). Cost: one atomic instruction each.
- `RankedLock<T, R>`: taken with a token of a lower rank, a `const` check of the two ranks
  that fails to compile out of order, in both builders; one token a task (`cx.lock_order()`);
  a guard borrows the token, so code written against the token has no `Cx` to wait with while
  it holds a guard. Cost: what a `Lock` costs; the ranks are types.

**What the types enforce, and what is convention.** Enforced: disjoint chunks; a frozen value
never changes; a channel's bound (no constructor without one); a flat channel's messages hold
no address (at the bound built by blaze, by a `const` assertion in `chan::bounded` under
rustc); a `must_overlap` task is never run inline; every wait takes `cx`; rank order; one
token a task. By convention until blaze's checks: that ranked code reaches its `Cx`
only through the token (blaze's "a guard or a value that is not `Send` live across a call that
can wait" makes it a compile error everywhere); the atomics' orderings, argued in their docs and
not yet run under blaze-check's interleavings.

**Absent on purpose.**

- **No unbounded channel.** It turns a slow consumer into unbounded memory (rule
  bounded-progress); a producer that must not wait says so with `try_send` and handles `Full`.
- **No bare thread spawn.** Work starts as a task of a `Scope`, which joins it: nothing
  outlives the scope that started it, and a task's failure is its scope's.
- **No thread-local storage.** State for a thread is a value passed to it; bstd has no
  `thread_local!` and no ambient state to read through one (rule nothing-ambient).
- **No sleep without `cx`.** `Clock::sleep(cx, ..)` is the only sleep: it has the task's
  deadline and its cancellation.
- **`RwLock` demoted.** Its readers block a writer and its waits take no `cx`; `Swap` covers its
  use, and the rest say why in a comment.
- **No generic atomic under the short name.** `sync::raw::Atomic<T>` makes each caller choose
  an ordering at each call; the named ones choose once, with a reason.

## Names are typed, and authority is stored

- **A capability's methods take specific types, never strings**: `dir.open(cx, &Path)`,
  `net.connect(cx, &HostPort)`, `env.get(&VarName)`. A field's type then says which capability it
  is for, and the set of places a name meets authority is closed, so a later analysis can follow
  a value from a field to the capability call it reaches.
- **Store the narrowed capability, not the name.** A struct holds `file: File<ReadOnly>`, not a
  `path` to be combined with an `Fs` later. A name joined to authority far from where it was
  checked is where confused-deputy bugs come from.

## Tracing

The recorder is the one [no-tracing](no-tracing.md) specifies (`blaze-trace`: per-thread rings,
fixed-layout rows, fields as words, no formatting on the hot path) feeding the profile history
([profiling](../../../profiling.md), [ix-prof-format](../../../ix-prof-format.md)). bstd does not
add a second one. It changes where the span lives, what is recorded without being asked, and how
a site is described. Where this section and no-tracing differ, this section is the newer.

- **The span is in `Cx`.** `let cx = cx.span(FETCH, &url);` returns a child `Cx`; the span ends
  when it drops; everything called with it is inside. A spawned task's `cx` comes from its scope,
  so the trace tree is the task tree and nothing needs attaching by hand. This replaces the
  thread-local enter stack: a task can change threads at any wait, and nothing here is global.
  The ring is the current worker's, reached through `Cx`.
- **The runtime records by itself**, with no instrumentation in user code:
  - per task, time split three ways: running, parked (on what, from the one wait), and ready but
    not running (queue delay);
  - wake edges, which task woke which. A lock is built on the wait and records its holder, so a
    slow wait reads "lock L, held by task N in span S";
  - the task's logical stack for the sampler: the slab's frame chain under `Frames`, the fiber's
    stack under `Fibers`.
- **A site is described at compile time.** blaze is the compiler and the linker, so each span and
  event site gets its id and a static schema (name, field names and types, file and line) in a
  table beside the binary. Nothing about a site is formatted, hashed or registered at run time.
  (no-tracing rejected link-time ids because a distributed slice was the only way; it is not.)
- **An event is fixed bytes**: site id, timestamp, task id, span id, raw field bytes, copied into
  the worker's ring. No formatting, allocation, lock, system call or write to another thread's
  memory. A drainer moves full chunks out; decoding happens offline against the schema table,
  straight into the history's typed columns.
- **Field encoders come from reflection** ([comptime](comptime.md)), not a macro and not `Debug`.
  A field whose type has no encoder is an error at the site, naming the type; there is no
  format-as-text path.
- **Secrets cannot be recorded.** A `Secret` field of a struct encodes as nothing; recording one
  directly is a compile error naming the field.
- **Time comes from the injected clock**: the CPU's counter read directly in production, virtual
  time under `Io::sim(seed)`. A seeded run gives a byte-identical trace, so a trace can be a
  test's oracle.
- **The tracer never blocks a worker.** A full ring drops and counts, per worker, and the count
  is recorded.
- **Keep or drop at the end.** A task's detailed events are buffered and kept only when the task
  was slow or failed; its cheap summary is always kept.
- **A disabled site**: patched out by the live-patch engine so it costs nothing, or one branch on
  a per-site flag. Chosen by measurement, and the number is written here.

Checked: the emit path is `nopanic`, `noalloc` and `nonblocking`, enforced, so tracing stays on
in release. Proved: a bound for the emit path in [bounds.md](../bounds.md) (a counter read and a
few stores into a hot buffer) with a budget on ns and bytes per event and zero allocations; a
simulation test of ring and drainer whose oracle is "every event is delivered once, in order per
worker, or counted as dropped", with a planted bug; and an encode-decode round trip.

## What each API makes explicit

The audit of `crates/blaze-std` against the second rule (2026-11-10). One row a place bstd was
deciding for its caller, or already was not. "Record" marks a signature that becomes a record
parameter when records land; its cost and guarantee fields stay required.

| module | API | what it makes explicit | was |
|---|---|---|---|
| `fs` | `Dir::publish{,_unsynced,_synced}`, `Content`, `Published::how` | durability; which of write, clone, copy, derive was paid; atomic replace (record: `durable`, a copy's `chunk`) | `Dir::write(path, bytes)`: always synced and atomic, unsaid |
| `fs` | `File::{append,write_at,set_len}{,_unsynced,_synced}`, `File::flush` | whether the call waits for the device | "durable once a later sync has returned", a doc comment; `File::sync` |
| `fs` | `File::close{,_unsynced,_synced}` giving `Written` or `Flushed` | that closing syncs only when told, and what the file then is | `close` always synced |
| `fs` | `Dir::{remove,rename_replacing}{,_unsynced,_synced}`, `Dir::sync_names` | that a name is durable only once its directory is synced; that a rename replaces | `remove`, `rename`: durability unsaid |
| `fs` | `Written`, `Flushed`, `clone_to`, `copy_to(.., chunk)` | whether bytes reached the disk; clone against copy; neither replaces (`FsKind::Exists`) | no clone or copy in bstd; the store's own |
| `fs` | `Flushed` carries its file's generation; `clone_to` and a cloning publish refuse `FsKind::Stale`; `Flushed::is_stale` | that the file was not changed, through this world, since its flush | a write behind a `Flushed` was unseen, and its clone forced writeback |
| `fs` | `Dir::create` | in place, not atomic, nothing durable until its close says | "made empty, replacing what was there" through a synced whole-file write |
| `fs` | `Buffered`, `Pushed` | the one buffer, sized by its maker; held against written; linear | no buffered writer |
| `fs` | `File::read_into(buf)`, `read_at`, `read_all(into)`, `Dir::list(cx, frame)` | the caller's buffer or allocator | already explicit |
| `disk` | `Io::disk() -> DiskCounts` | what a path paid, in a test | nothing |
| `kv`, `blobs` | `put{,_unsynced,_synced}`, `delete{,_unsynced,_synced}` | durability of a put and a delete | "durably and all at once", fixed |
| `io` | `Io::cx(budget, scratch)` | the root frame's size (record: `scratch`) | 64 MiB picked inside |
| `io` | `Io::fs()` .. `Option`, `need_x() -> Result<_, Missing>` | absence at the root, named | already explicit |
| `task` | `Scope::spawn(name, scratch, f)`, `spawn_concurrent` | the task's frame size (record: `scratch`); in any order against must overlap | 64 MiB picked inside |
| `task` | `Scope`, `Task` linear, `join(cx)` | that tasks are joined and failures seen | already explicit |
| `cx` | `Cx::{deadline, name, cancelled, check}` | `noalloc`, `nonblocking`, checked | unmarked |
| `cx` | every waiting call takes `&Cx`; a wait ends in `Stopped` | deadline, cancellation, span | already explicit |
| `pending` | `Pending::cancel(cx, give_up)` | how long a cancel waits past its context | the later of the deadline and 1 s, picked inside |
| `pending` | `Pending` linear; `read_into`/`write_from` against `read_at`/`write_at` | an op in flight is finished; start against start-and-wait | already explicit |
| `net` | `Conn::send_all`; `recv(cx, most, into)`; `Net::limit` | all or an error; the caller's bound and buffer; the byte budget | `send` |
| `net` | `Listener::close` hangs up at most the world's `Limits::close_most` and says `Unending { accepted, most }` | the bound, said where the `Io` is made | `CLOSE_MOST`, a constant |
| `io` | `Limits { abandon_wait, poll_fuel, close_most, trace_most }` on `SimWorld` and `NativeWorld`, no `Default`; `Io::limits()` | the bounds of an abandoned value's wait, a stalled driver's polls, a close and the trace | four `pub const`s picked inside |
| `sync` | `Lock::lock(cx)`, `Lock::try_lock()` | a lock wait is a wait: a deadline, a span named for the lock, `Stopped` | `lock()` with no `Cx` |
| `local` | `pair(name, NonZero)`, `End::write_all` (`LocalError::moved`) | the buffer's size; all or how far | capacity 0 raised to 1; `write` |
| `streams` | `Stream::send_all`; `Streams::open(cx, origin, inbox_most)` and `Refused::InboxFull` | all or an error; the most bytes a stream holds unreceived | `send`; an unbounded inbox |
| `requests` | `Http::over(net, clock, roots, Wire)` (record: `Wire`'s fields) | user agent, most body bytes, most receives; one connection a request, no retry, redirect or pool | three constants picked inside |
| `requests` | `Exchange` linear, `request(.., into)` | the answer's buffer; finishing | already explicit |
| `env` | `Env::get -> Result<Option<&str>, NotAllowed>` | not allowed against not set | both `None` |
| `clock` | `Instant::since -> Option<Duration>` | that the earlier instant was later | zero |
| `clock` | `sleep(cx, ..)`, `sleep_until(cx, at)`, `Deadline` absolute | the wait and its bound | already explicit |
| `rng` | `Rng::below(NonZero<u64>)`; no `Clone`, `fork()` | an empty range cannot be asked; a seed is never shared by accident | `below(0)` gave 0 |
| `mem` | `Mem::arena(name, limit)`, `Bytes::try_*`, `Arena::alloc()` | name, limit, fallible growth | already explicit |
| `eval` | `Eval::memo(mem, MemoLimit)` (record), `Graph::new_in(arena)` | entries and bytes kept; where provenance is kept | already explicit |
| `budget` | `Budget::{take, give_back, left}` | `noalloc`, `nonblocking`, checked | unmarked |
| `device` | `Usb`/`Serial`/`Bluetooth::write_all`; `scan(cx, frame)`; `read(.., into)` | all or an error; the caller's allocator | `write` |
| `audio` | `play(cx, device, pcm, periods, fill)` | how long it plays; the device's pull | already explicit |
| `keys`, `procs`, `code`, `window`, `term`, `power`, `opener`, `automation` | every call takes `&Cx`; narrowing by typed name; `RawMode`, `Awake` linear | wait, authority, finishing | already explicit |
| `path`, `url`, `name`, `env::VarName`, `net::HostPort` | checked constructors with a typed `Why`; `path!`/`var!`/`host!` at compile time | no lossy parse | already explicit |
| `sort` | `sort::on(workers)`, `Serial` | which threads sort; no ambient pool | already explicit |
| `map`, `set`, `hash` | one type per need ([Collections](#collections)); fixed seed; `KeyedMap::with_hasher_in(key, a)` for keys from outside | no per-process hasher seed; the allocator; which removal | `bstd::map::Map` named `HashMap` two ways |

Left implicit, with what each waits for (none is hidden by a default a caller could have
chosen otherwise today, and each is a row to close):

| where | what is still implicit | waits for |
|---|---|---|
| every write and send | the bytes are copied once into the effect, in the global heap | registered buffers, S4 |
| `Pending` | one boxed closure an op | the `Frames` layout (the op's state is in its frame) |
| `requests::Http::over` | the h1 core's head and body buffers are `Vec`s in the global heap | an h1 core that takes an allocator |
| `streams` (handler provider) | replies are held in the global heap (bounded by `inbox_most`) | an allocator on `Streams::open` |
| `Path::join`, `Dir::subdir`, narrowing (`only`) | a name or an allow-list is allocated in the global heap, once | S4's `String<A>`, or names interned in the task's frame |
| `keys::Secret` | its bytes are in the global heap | a locked, zeroed arena from `Mem` |
| `fs::Flushed` | a change to its file by another process is not seen (the generation counts this world's effects) | a stat effect that reports the inode and change time (`statx` in blaze-sys has both) |
| `sync::RwLock`, `Signal` | `read()`, `write()` and `wait()` take no `Cx` (blaze-core's, re-exported); `Lock::lock(cx)` waits on the wall clock, not the driver's | S1b: bstd's own locks over the one wait |
| `Io`'s own state | one process lock around the driver (uncontended under `Inline`) | the scheduler (S3b) |
| `kv::KEY_MOST` | a bound of the format (a key's longest file name), a `pub const` named in the outcome it ends in | nothing: not a policy |
| `Io::sim(seed)` | `Limits::USUAL`, a named value its doc says | nothing: a test's world; `Io::simulated(SimWorld::new(seed, limits))` says its own |
| `SimWorld::new(seed)` | the simulation's latency and faults default (`Faults::default`) | nothing: a test's world, and the seed is said |
| `audio` callbacks | `Fill`/`Take` should be `noalloc` and `nonblocking`; a closure type cannot carry the mark | effect bounds on closure parameters |
| the facade (`record`, `report`, `bytes`, `http`, `quic`) | re-exports are not audited here: `record::to_bytes` and `report::render` allocate in the global heap | S1d, as each piece is merged |

## Errors

One error type per area (`FsError`, `NetError`, ...), each naming the operation, the thing (the
path, the host) and the kind. No catch-all `io::Error`, no error without its subject (rule
`errors-name-the-thing`).

## Memory is explicit

There is no global heap in bstd. Every collection is made in an allocator its code was handed
(`Vec::new_in(a)`, `HashMap::new_in(a)`, `String::new_in(a)`); `Vec::new()` does not exist. This is
Zig's rule, and it buys what the plan needs: every byte belongs to a task or a named cache
(section 6), `noalloc` is visible in a signature, and tests can count and fail allocations.

| allocator | from | lives |
|---|---|---|
| `Frame` | `cx.frame()` | until the task ends; a bump pointer, freed whole |
| `Arena` | `mem.arena("name", limit)` | until dropped; for a result that outlives its task |
| `Cache` | `mem.cache("name", limit)` | until evicted; the one evictor may take entries back |

`Mem` is a capability from `io.mem()`: it hands out named arenas and caches against the process's
one memory budget ([memory](memory.md)). Every arena has
a name and a limit, so "what holds the memory" always has an answer.

**Running out is an outcome, not a crash.** An allocation over its limit or the budget fails the
task with `Stopped::OutOfMemory { arena, asked }` through the same path as a panic (rule
`panics-fail-one-job`); the process and the other tasks go on. Code that wants to handle it itself
uses the `try_` forms (`try_push`, `try_reserve`), which return the error.

Until S4's runtime ([sys](sys.md), "Runtime"), `alloc`'s global allocator still exists underneath; the `ambient` check treats a use of
it in a capabilities crate as an effect (`global_alloc`), which replaces the `// alloc:` comment.

### One handle, not a type parameter

The allocator is a value of one type, `Alloc<'a>` (blaze-own's `alloc_ref`), as Zig's is: one
word, `Copy`, the address of an allocator whose first field is a header pointing at a table of
its functions, borrowing that allocator for `'a`. `Erased::new(a).handle()` makes one of any
allocator. A generic function is type-checked with its trait obligations at every call, compiled
once per instantiation, and cannot be a plain function pointer or a stored task; a function over
the handle is checked and compiled once.

- A function that allocates takes `alloc: Alloc<'_>` (`Alloc<'a>` when its result holds it).
- A type that owns collections holds them as `AVec<'a, T>`, `AString<'a>`, `ADeque`, `ABox`,
  `ASortedMap`, `ABTreeMap`, `ASoa` (a `soa!` table is `Rows<Alloc<'a>>`) and takes the handle
  once, in `new_in(alloc)`; a view that only borrows record bytes names no allocator at all.
- A function or type is generic over `A: Allocator` only if it is collection library code (a
  container over any allocator) or a measured hot loop, the measurement named in a comment
  beside it (`examples/alloc_cost.rs` in blaze-own is the template); such a loop names `&Frame`.
- Tests and a program's top may use `Alloc::global()`; nothing below the top names it.
- `Alloc` is neither `Send` nor `Sync` (it may be a task's frame); a value that crosses threads
  names a `Sync` allocator concretely, or takes a fresh handle on the other side.

The measurement behind the rule (blaze-own `examples/alloc_cost.rs`, release, Apple silicon,
2026-11-15 21:45 PST, one run, this machine; handles hidden from the optimizer by `black_box`):

| ns per | bump | bump through `Alloc` | global | global through `Alloc` |
|---|---|---|---|---|
| push, vector of 16 | 0.94 | 1.03 | 3.20 | 3.48 |
| push, vector of 1024 | 0.46 | 0.46 | 1.02 | 1.05 |
| push, vector of 65536 | 0.71 | 0.70 | 0.74 | 0.75 |
| box of 16 bytes made and dropped | 0.99 | 1.52 | 6.43 | 7.32 |
| box of 128 bytes | 4.46 | 4.57 | 8.56 | 9.58 |
| box of 1024 bytes | 17.97 | 18.55 | 33.43 | 33.14 |

The handle costs at most about half a nanosecond per allocation and nothing measurable per push:
an allocator choice (bump over global) is worth ten times what the handle costs.

### Collections

One type per need. Each is made with `new_in(alloc)` or `with_capacity_in(n, alloc)`; none has a
`new()`, and none picks a hasher seed at run time. The names are the same in `bstd`, in
`blaze-hash` and in the blaze edition's prelude (`HashMap`, not `Map`: `blaze_hash::Map` is the
type generic over its hasher, which only the aliases below name).

| need | type | what it is | what it costs |
|---|---|---|---|
| ordinary keys (ids, names, tuples, paths) | `map::HashMap<K, V, A>`, `set::HashSet<K, A>` | hashbrown on foldhash with one fixed seed | one hash, one probe; one allocation with room |
| keys that are hashes already (a content id, a store key, a fingerprint) | `map::ByHash<K, V, A>`, `set::ByHashSet<K, A>` | the same table on the identity hasher: the key's first word places it | no hash at all |
| values stored once, found by their content | `map::ContentMap<V, A>` (one owner); `blaze_hashcons::HashCons<T>` (shared between threads, a dense `Idx` and a handle that compares by address) | `insert(value) -> ContentId`, `get(&id) -> Option<&V>`; a `ByHash` on the BLAKE3 of the value under its type's domain | BLAKE3 once per insert, over the value; a lookup hashes nothing |
| iteration that reaches an output, or entries named by position | `map::IndexMap<K, V, A>`, `set::IndexSet<K, A>` | entries dense in a vector in insertion order, and a table of `u32` indices; `swap_remove` (O(1), one entry moves) and `shift_remove` (O(n), order kept), no `remove` | a lookup is a hash map's plus one load; iteration is a vector's; two allocations with room |
| sorted keys, range queries | `map::SortedMap<K, V, A>`, `set::SortedSet<K, A>` | `alloc`'s B-tree (`BTreeMap::new_in`: it takes an allocator on the pinned nightly, `allocator_ext`) | O(log n), a node per 11 entries |
| dense ids whose slots are reused | `mem::Slab<T>` and `mem::Handle<T>` (blaze-mem) | `(index, generation)`: a stale handle is detected, not dereferenced; a `Slab` grows in place | a bounds check, one compare, one load; no hash |
| keys an attacker may pick (names, paths and headers from outside) | `map::KeyedMap<K, V, A>`, `set::KeyedSet<K, A>` | the hash table on SipHash-1-3 under a `hash::SipKey` | SipHash per key, about 3 times foldhash on a short key |
| shared between threads | `map::SharedMap<K, V>`, `map::SharedByHash<K, V>`, `set::SharedSet<K>` | papaya's lock-free tables; values are lent, never moved out | a read takes no lock and writes nothing shared |

Decided, with the reason for each:

- **A content map is two types, not one.** `ContentMap` has one owner, its allocator in the
  type, removes, and no index. `HashCons` is shared (shards, lock-free reads), hands out a dense
  `Idx`, never frees, and lives on the global heap. One type could not be both without paying
  for the sharing in the single-owner case, which is the same split as `HashMap` and
  `SharedMap`. They agree on what a value's id is (`content_of`: the type's `Domain`, then what
  its `Content::hash_content` feeds), so a value keeps its id between them.
- **The trust rule.** An id is trusted only where the map computed it. A value that arrives
  with an id (from a file, a peer, a cache) goes in through `insert_claimed(id, value)`, which
  hashes the value and returns `Mismatch { claimed, actual }` without storing when they differ.
  Two unequal values with one id stop the process naming the type: the type's `hash_content`
  leaves out something its `Eq` compares.
- **Is a map from `Hash(v)` to `v` efficient?** The lookup is: no hashing, one probe. The
  insert pays BLAKE3 over the value, about a nanosecond a byte for small values, so it is the
  right table when the id is wanted anyway (it is persisted, sent or compared across runs), and
  the wrong one for deduplicating in memory only, where a `HashSet` of the values costs a
  foldhash. `ByHash` against `HashMap<[u8; 32]>` is the measured row below.
- **An insertion-ordered map of bstd's own**, not the `indexmap` crate: that one has no
  allocator parameter, holds `usize` indices (twice the table), and offers `remove`, which
  hides which of the two removals a call meant. Equality here is of the order too, since the
  order is what a reader of the map sees.
- **No sorted map of bstd's own.** `alloc`'s B-tree already takes its allocator; a second tree
  would be a second home for one concept.
- **A fixed seed gives up resistance to chosen keys**, on purpose, for reproducible runs and a
  hash the compiler folds. Where the keys come from outside and the table grows with them, the
  maker hands a key in: `KeyedMap::with_hasher_in(SipKey::random(), alloc)` for a table nothing
  persists, or a key derived from the content (`id::index_key`) for one an image lays out, or a
  key from `io.rng()` in code that takes capabilities. `SipKey` has no `Default`, so a keyed
  table has no `new_in`: it cannot be made without its key. Iteration order of a keyed
  `HashMap` differs by key; a keyed table whose order is read is `OrderMap<K, V, SipKey, A>`.

Proven by (`crates/blaze-hash/src/order_tests.rs`, `crates/blaze-hashcons/src/content_map.rs`):
an ordered map equals a `Vec` of pairs under random inserts, both removals by key and by index,
`pop` and `retain` (blaze-sim, 300 cases); its order is the same under the fixed seed, under random
keys, at three capacities and in two other processes, with the control that a hash map's order
under two keys differs; a table with room allocates when made (hash map 1, ordered map 2, content
map 1) and never within the room; a content map equals the distinct values by id under random
inserts and removes; a claimed id that is not the value's is refused and stores nothing; keys
picked to collide under a known hasher spread under a key.

Cost rows are printed by `cargo test --release -p blaze-hash --lib collections_cost -- --ignored
--nocapture` (insert, hash and lookup, and iteration of a million `u32` ids for `IndexMap`, the
`indexmap` crate and `HashMap`; lookup of a million 32-byte hashes for `ByHash` and
`HashMap<[u8; 32]>`). Not measured yet: no run of it has printed (2026-11-12); the numbers go
here with the commit and the machine's load when one has.

Moving blaze's own code off the `indexmap` crate: nothing has moved yet, and `.clippy.toml` does
not ban it yet. Its uses are 53 files of 14 crates, nearly all through `base_db::FxIndexMap` and
`FxIndexSet` (hir-def 18 files, hir-ty 9, blaze-diagnostics 6, blaze-lints 5, base-db 4, ix-prof
and blaze-engine 2, and one each in span, hir-ty-charon, hir-lint-levels, hir-expand, blaze-query,
blaze-profile, blaze-explore-core). What a move has to settle first, since these are the front
end's own tables: equality (`indexmap`'s ignores order, `OrderMap`'s does not, and `FileSet` and
query values derive `PartialEq` over one); the entry API and serde, which `OrderMap` does not
have yet; and `blaze_query`'s `container_values!` for it. The ban goes in with the port of
`base_db`'s two aliases, as one batch (rule whole-ports), with its baseline. Decision D25 is not
what holds it: `blaze-hash` builds under cargo.

### Contracts

bstd's public functions carry `requires` and `ensures` (ledger V3). While rustc also builds bstd
("Two builders") they are nightly's `#[core::contracts::ensures(..)]`, which both builders read
and run under `cfg(contract_checks)`; the native clause syntax ([contracts](contracts.md);
`#![feature(blaze_verify)]`), checked by default in the dev and test profiles and the form the
generator and the proofs read, comes back through `blaze::native_contracts` when blaze is the
only builder.

| function | contract | generated test |
|---|---|---|
| `budget::Budget::take` | a refusal names what was asked, and its `used` never passes its `limit` | no: a method |
| `clock::Instant::since` | it answers exactly when `earlier` is not the later | no: a method |
| `kv::Key::new` | a key is 1 to `KEY_MOST` bytes | no: an associated function |
| `fs::Dir::quota` | the directory given has at most `bytes` left | no: a method |

Written and not yet stated, because each needs `self` or a borrowed argument in its `ensures`,
which no test of the lowering shows it takes: `Kv::get` after `put_synced` reads the value put;
`Blobs::put` returns the BLAKE3 of its bytes and `get` hands back only bytes whose BLAKE3 is the
id; `Budget::left` is at most the limit; `Clock::after(wait)` is `now + wait`; a `_synced` call
leaves `device_waits` higher and an `_unsynced` one does not.

What the generator (`#[blaze::contract_test]`) needs before these are run as property tests: it
refuses methods and associated functions (bstd has almost nothing else), so it needs a receiver
(drawn, or built by a constructor the type names); a `&Cx` and capabilities, made from
`Io::sim(seed)` with the seed drawn; `&mut` arguments (`Bytes`); and a `Debug` bound it can do
without for capabilities. The crate-wide `#![blaze::contract_tests]` is also not built.

### The page pool: one home for bytes that come from outside

The store's buffer pool (`docs/v2/plan/design/store-pool.md`) is not a store feature. It is how bstd
holds any bytes that came from a file, a socket or a device, and it moves into `bstd::mem::pool`
with the store as its first user. There is no `mmap` in bstd.

- **One reserved range, pages loaded by explicit reads** on the driver, in parallel, with a
  state word per page. A miss is an op with a deadline, never a page fault that stalls a worker.
- **`file.pin(cx, range)` gives `Pinned`**, which derefs to `&[u8]`: the bytes of the file in
  place, held for as long as the pin lives. This is what `mmap` was used for, with the pool
  choosing what stays in memory and blaze choosing what to prefetch.
- **Records are read in place from pinned pages** (`bstd::record`: bytes in memory are the bytes
  on disk), so loading a result decodes nothing and copies nothing.
- **Receives land in pool pages.** The buffers a driver lends with an event (the boundary's
  "events lend their bytes") are pool pages, so network data, file data and what the program
  keeps of them are one kind of memory with one owner and one count.
- **On unified memory a pool page can be a GPU buffer** (page-aligned, shared), so disk to pool to
  device is no copy at all.
- **It is the `Cache` of `Mem`.** Pool pages are the cheapest thing to drop (one read brings them
  back), so the one evictor takes them first, and the pool grows only into room the budget has.
- **The pool's logic is a sans-IO core**: its inputs are requests and completions, its outputs
  are reads, advice to the OS and wake-ups. The simulation runs it with failed reads, short
  reads and crashes like everything else.

`Mem` itself is a capability whose system calls (reserve, commit, give back) are ops on the
boundary, so a simulated `Io` can refuse memory, count it and fail it at a chosen point.

Strings are UTF-8; a path is bytes on Unix and WTF-8 on Windows behind one `Path`.

## The checks

Two effects join the existing MIR walk (`crates/hir-ty-pure`, with pure, nopanic, noalloc and
nonblocking; [effects](effects.md)):

- **`ambient`**: reaches a system call, or Rust `std`'s file, environment, network, clock, random,
  process or thread API, outside the trusted list.
- **`unsafe`**: reaches an `unsafe` block outside the trusted list.
- **`global_alloc`**: allocates from the global heap instead of a handed allocator.
- **`global_state`**: reads or writes a `static` with interior mutability, a `static mut` or a
  thread-local.

Effect attributes on bstd itself. This branch has `#[blaze::pure]`, `nopanic`, `noalloc` and
`nonblocking` ([effects](effects.md)); bstd marks with those where the mark holds and is
checked, and the positive form being built (`#[blaze::expect(block, alloc, ..)]`) replaces
each with the exact list of what a function does, so "only `_synced` and `flush` may block"
becomes `expect(block)` on those and its absence everywhere else. Marked today: `Cx::{deadline, name,
cancelled, check}`, `Budget`'s counting, `Instant::{since_start, since}`, `Durable::unsynced`,
`How::says`, `Buffered::held`. Not marked, and why: every call that reaches the driver takes
`Io`'s process lock and polls inline in S1, which the walk rightly reads as blocking, so
`_unsynced` cannot be marked `nonblocking` until a wait parks on the scheduler; its promise is
the narrower one the ledger tests ("no wait for the device").

Lints the second rule implies:

| lint | refuses | exists |
|---|---|---|
| `blaze::nopanic`, `noalloc`, `nonblocking` | a marked function that reaches the effect | yes |
| `blaze::linear` (the borrow check's `LINEAR`) | a `File<ReadWrite>`, `Buffered`, `Pending`, `Scope`, `Conn` left unfinished | yes |
| `blaze::implicit_policy` | a public bstd function that reaches a cost its signature does not carry (below) | no: specified here. Not built with `default_policy` because the lints read no effect summary yet (`instance_effects` is the MIR walk's, with no query the lint pass can ask per function), and the `Sync` and `Write` kinds are not rows of `effect_attrs::ROWS` |
| `blaze::default_policy` | `impl Default`, `#[derive(Default)]`, or a `new()` with no arguments, for a type with a field marked `#[blaze::policy]` (a cost or a guarantee) | yes: `crates/blaze-lints/src/blaze/default_policy.rs`, on in bstd; `Limits`, `SimWorld::limits`, `Wire` and `MemoLimit` are marked |
| `.clippy.toml` bans of the removed names (`Dir::write`, `File::sync`, `File::replace`, `Conn::send`, `End::write`) | a use of one, by resolution | no: the names are gone from bstd, so the ban is for crates that move onto it (S3) |
| `Flat` (`where const { is_flat::<T>() }`; plain: the `const` assertion in `chan::bounded`) | a flat channel of a type with an address or drop glue in it, naming the field path built by blaze | yes: the walk's answers in `src/tests/flat.rs`; the refused channel, by an address and by drop glue, is compiled by `tests/refusals.rs` |
| compile-fail fixtures for bstd | `written.clone_to(..)`; a `Buffered` dropped; a write through a handle narrowed to read-only; a call of a removed name (`Dir::write`) | yes: `bstd_the_wrong_call_does_not_compile` in `crates/blaze-engine/tests/repro.rs` runs `blaze check` on each file of `crates/blaze-engine/tests/repro/bstd_the_wrong_call_does_not_compile/wrong`, beside a control that checks |

**`blaze::implicit_policy`**, for crates on the trusted list that are libraries (bstd first).
For each `pub` function and method, read the effect summary the walk already makes
(`instance_effects`: one `EffectSet` per instance, with the shortest witness of each kind) and
compare it with the signature:

| the summary has | the signature must have one of | else |
|---|---|---|
| `Block` through the one wait, a lock or a foreign wait | a parameter of type `&Cx`, `Deadline`, or a type holding one (`Scope`, `Pending`) | "waits (witness path) and takes no deadline" |
| `Alloc` (the global heap) | nothing satisfies it | "allocates from the global heap (witness)": a `// alloc:` line with a reason, until S4, is the one opt-out |
| `AllocIn` | a parameter or `self` field whose type is an `Allocator`, `&Frame`, `Bytes`, `Arena`, `Mem`, or a return type parameterised by one | "allocates in an allocator its caller did not hand it" |
| a new kind `Sync` (reaches `Effect::Sync`, `fsync`, `F_FULLFSYNC`, a directory sync) | a name ending `_synced`, or `flush`, or a parameter of type `Durable` | "waits for the device and does not say so" |
| a new kind `Write` (reaches a file-changing effect) with no `Sync` | a name ending `_unsynced`, or a parameter of type `Durable` or `Unsynced`, or `self: Buffered` | "changes a file and does not say how durably" |
| a copy of a slice parameter into owned memory (`to_vec`, `extend_from_slice` of an argument) | a name starting `copy_` or ending `_all`/`_owned_in`, or an `into:` buffer parameter | "copies its input and is not named a copy" |

What it needs that does not exist: two effect kinds (`Sync`, `Write`) as rows of
`effect_attrs::ROWS` and `FOREIGN` (the driver's sync and write entry points, `fsync` and its
family); the table of signature types above as data beside the rows; and, for the copy row, the
walk recording when an argument's bytes reach an allocation (the taint summary
`constant_time` would add; without it the copy row is left out and the rest still stands).
It reads summaries and signatures only, so it adds no walk.

The trusted list is in the workspace manifest (`[blaze.trusted]`, each entry with an owner and a
reason); `bstd` is on it. `blaze explain trusted <binary>` prints what a binary rests on, and
`blaze explain caps <item>` follows a capability or a secret through the call graph. A crate
opts in with `#![blaze::capabilities]`, which makes both effects errors in it and its
dependencies' reached code.

## The blaze edition: opting in

Decided 2026-11-12 (S5, slice 1 built). A package opts in; nothing else changes anywhere.

**The opt-in** is the manifest's edition, as [edition](edition.md) designed it:

```toml
[package]
edition = "blaze-2026.11"
```

One spelling. A key cargo could read (`[package.metadata.blaze]`) lost: a package of the
edition is not a cargo package (its `std` is bstd and its `main` takes parameters), so cargo
stopping at the edition key, naming it, is the true error, and one that parsed would only fail
later and worse. blaze's own crates stay on `edition = "2024"` while cargo builds blaze
(decision D25) and use bstd as a dependency by name. `edition.workspace = true` inherits it.

To the front end the manifest value is two things (`edition::BlazeEdition`, the one home of
every spelling): the Rust edition it fixes (2024, for the parser, name resolution and every
manifest rule) and the crate attribute `#![blaze::edition = "2026.11"]` on each crate of the
package but its build script and a proc-macro target. The attribute is the tracked input: a
crate's key hashes its crate attributes, so no query result, record or object is shared
between a crate with it and one without. A root file may write the attribute itself (a
single-file program); two that disagree, or a version that is no edition's, are errors.

**What changes under it, and nothing else:**

| | under the edition | built |
|---|---|---|
| `std` | names the dependency called `bstd`; Rust's `std` has no name in the crate (`core` is unchanged, `alloc` is by `extern crate alloc`). So `std::fs`, `std::env`, `println!` and the rest of Rust std's ambient authority are not reachable by any path. Decided 2026-11-16 (toolchain.md): the toolchain's bstd is bound to one extern name, `std`; `bstd` is not a second name (rule 30), and a path written `bstd::..` is a finding whose fix rewrites it to `std::..`. Canonical item paths in records and in what tools print stay the crate's own (`bstd::cx::Cx`). A Rust-edition crate that depends on bstd names it by its manifest line, as before. There is no opt-in to Rust's `std` under the edition | yes |
| the prelude | `std::prelude::blaze_2026_11`: `core`'s Rust 2024 prelude, `Cx`, the grant types a `main` may take (`Out`, `Args`, `Clock`, `Mem`, `Spec`, `Dir` with `ReadOnly`/`ReadWrite`, `Net`, `Env`, and the whole capabilities `Fs`, `Rng`, `Host`, `Term`, ..), the entry's `Error`, `core`'s macros (`assert_eq!`, `panic!`, `write!`, ..), and for memory `Vec`, `Box`, bstd's `String` and `format_in!` ("Memory under the edition"). Not `Io` (D104) | yes |
| the entry | D104: `main` names each capability it uses. After an optional `cx: &Cx`, each parameter is a grant: its type the kind (every `impl Grant for` in bstd's grant.rs), its name the grant's name (`out: Out` is standard output, `err: Out` standard error, `data: Dir<ReadOnly>` the folder granted as `data`). A whole `Io` cannot be asked for: `fn main(cx: &Cx, io: Io)` is refused at the parameter, with the kinds to name instead. `fn main()` takes nothing. It returns anything that is a `bstd::entry::Outcome` (a trait: `()`, a `Status`, a `Result` of one and an error that can be shown; a program's own type may implement it) | yes |
| the root context | `main(cx, ..)` gets one with the named bounds `entry::MAIN_BUDGET` (no deadline) and `MAIN_SCRATCH`; a program that wants narrower ones makes a child context (`cx.until(..)`) | yes |
| a test | takes the same roots, handed the simulation's (`Io::sim(seed)`, the seed printed on failure); `#[test(native)]` asks for the real one | S5b |

The entry is library code. In the blaze edition the root's `fn main` is expanded by bstd's
`main!` attribute macro with no attribute written (D104), the same path a written
`#[bstd::main]` takes in a Rust-edition crate: it keeps the function, puts its manifest (each
parameter's name and kind) in the binary, and makes the process's `main`, which makes each
grant from the run spec (or, started from a shell with no spec, the kinds that fill
themselves: `args`, `out`, `clock`, `mem`, `spec`), refuses to start with every missing grant
listed, calls `main`, prints a failure as `error: <what>` on standard error and exits 0 or 1.
`entry::run_in(io, ..)` runs a body on an `Io` a test made (`Io` stays bstd's own world handle
and a test's simulated world). Standard output is the grant `out: Out`, in the simulation and
natively.

**Saying something** is one call, whatever is said: `out.line(cx, "text")?` and
`out.line(cx, format_args!("x = {x}"))?` (`write` for no line feed), on anything `Display`,
formatted on the stack with no allocation, returning the `OutError` itself. `out.with(cx)` is
a `core::fmt::Write` for `write!`/`writeln!` and keeps the first error, which `fmt::Error`
cannot carry. bstd holds nothing between calls: a call of up to `out::HELD` (1024) bytes is
one write, with the operating system when it returns, so a crash leaves on screen every line
whose call returned. The price is one system call a line, which is what Rust's line-buffered
`println!` pays too (measured: see the lane's report); a program that wants fewer builds its
text in an allocator and writes once.

**Memory under the edition.** The prelude has `Vec` and `Box` (alloc's types, for
`Vec::new_in(cx.frame())` and `Box::new_in(v, a)`; the allocator API needs no `#![feature]`
in the crate), bstd's `String` (made in an allocator) and `format_in!`. It has no `vec!` and
no `format!`, and `alloc` is not nameable without `extern crate alloc`. That is not
enforcement: `Vec::new()` is a method of the same type, and it allocates from the global
heap. Two things refuse it. `blaze check` denies `blaze::std_use` in a crate of the edition
(its table has the global-heap rows: `Vec::new`, `Box::new`, `String::from`, `to_string`,
`format!`, `vec!`, the collections), naming the explicit form; built. The build's own
refusal is the `global_alloc` effect of S2 over every function the program reaches; S5g,
not built, so `blaze build` and `blaze run` of a program that writes `Vec::new()` succeed
today. A dependency of a Rust edition allocates from the global heap as before and still
works; its bytes belong to no task and no arena, so the program's accounting ("every byte
belongs to a task or a named cache") covers the edition's own code and bstd's handed-back
memory only, and says so, until S5g's walk either proves the reached code allocates nothing
or the crate is on the trusted list with its heap counted as one named arena. bstd itself
still has about 128 global allocations, each with its `// alloc:` line (effects' buffers,
errors' messages, once-per-world tables), until S4.

**Errors name the rule.** A `main` of the edition with a parameter that is no root, a root
twice or out of order is refused at `main`, naming the parameter and `entry-takes-roots`. A
`main` with parameters in a crate of a Rust edition (which rustc refuses as E0580, and blaze
used to compile to a call with garbage arguments) is refused saying it needs the blaze
edition and the manifest line to add. An edition crate with no `bstd` dependency says so.

**Old code is untouched.** A package without the opt-in is given no attribute, and every
branch above is behind `Crate::blaze_edition`, which is `None` for it: same keys, same
resolution, same entry, same bytes. Checked by a build of the same program on both editions
side by side, and by the fixtures of every crate that existed before.

**Across the boundary.** Both directions link, since an edition is a property of a crate:

- A new-edition package depends on old-edition crates as on any crate. What crosses is
  ordinary Rust: types, traits, functions. The old crate keeps Rust std's ambient authority,
  so the new program's "what can this reach" claim stops at that call: `ambient` (S2) proves
  from source that the reached code has none, or the crate is on the trusted list. Until S2
  and S5c the claim is about the edition's own code only, and says so.
- An old-edition crate depends on a new-edition library by naming bstd too (`bstd::Cx`,
  `bstd::Io`) and handing over capabilities it made (`bstd::entry::run`, `Io::sim`).

**`blaze new`** writes a package of the latest blaze edition (the opt-in, and a `main(cx:
&Cx, out: Out) -> Result<(), Error>` that writes its line through `out`) and
`blaze new --rust` the plain Rust package, byte for byte cargo's. Until blaze ships bstd
(S5d) a package of the edition names bstd as a dependency by path, so the edition is the
default only where `--bstd <PATH>` says where bstd is; without it `blaze new` still writes
the Rust package, and `--edition blaze-2026.11` alone is refused naming the flag. One
constant, `blaze_new::BSTD_SHIPS`, makes it the default everywhere.

**Not on the latest edition** is one lint, `blaze::not_latest_edition`, for a Rust edition and an older blaze edition
alike: one finding a package, only for the workspace's own packages, saying what opting in
gives and the manifest line; a diagnostic like any other (stored in the crate's record, a
`compiler-message` in JSON, allowed or denied in the workspace's lint table or by flag). It
warns by default from the same day `blaze new` defaults to the edition
(`ready::EDITION_SHIPS`): until a package of the edition builds wherever blaze runs, telling
people to move would be a nag. blaze's own workspace allows it while cargo builds blaze (D25).

Deferred, a row each below: tests (S5b), the sandbox derived from `main` (S5c), bstd shipped
with blaze (S5d), `Out` on the driver (S5e), build scripts and proc macros (S5f), the global
heap refused by the build (S5g).

## Moving blaze onto it

Interface first, bottom last, so nothing waits on the runtime and every step lands on its own.

| step | what | done when |
|---|---|---|
| S1 | The crate `crates/blaze-std` (`bstd`): `Cx`, `Io`, every capability above, the allocators, the scopes and deadlines, the errors, over `blaze-core`'s drivers and its simulation; the second rule applied to all of it ("What each API makes explicit"), with the Files design | its simulation tests pass; one planted bug per invariant is caught (for files: a publish that reports done before its rename, a `Durable::Yes` that skipped the directory sync, a `Flushed` handed out before its flush) |
| S2 | The `ambient` and `unsafe` effects, `#![blaze::capabilities]`, `blaze explain trusted` | a crate with the attribute that calls `std::fs` is refused, naming the call path |
| S3 | blaze's crates move, leaves first: a sans-IO core takes nothing, a driver takes the piece it needs, `blaze`'s `main` names the grants it uses (D104). Each move is a `.clippy.toml` ban and `blaze check --fix` (rule `fix-through-blaze-lints`) | every crate in `crates/` has `#![blaze::capabilities]` |
| S1b | `bstd::{sync, cell}`: the table of "Shared mutation", token cells, `Freeze` as a bound, typed names (`HostPort`, `VarName`) in place of strings, and bans on the removed names | misuse tests: two live `get_mut` on one token do not compile; a static that is not `Freeze` is refused |
| S1c | Tracing on bstd: the span in `Cx` and the scheduler's own events first; then the compile-time site table and the ring; then reflection encoders and redaction; disabled-site patching and keep-or-drop last | the emit budget holds; the ring's simulation test and the round trip pass |
| S1d | The facade: bstd re-exports the crates of "What is in it", the interner and helper crates are merged, and the piece crates' names are banned outside bstd | `blaze check` with the bans on; no blaze crate outside bstd names a piece crate |
| S1e | `bstd::image` (PNG), `bstd::video` (the encoder core, the muxer, the VideoToolbox provider moved out of `harness-video`, which then uses bstd's), and `Gpu` as a capability with the CPU provider and offscreen targets | a round trip of an image; a clip encoded from fixed frames is byte-identical twice; `harness-video` builds on it |
| S1f | The page pool moves from `blaze-cas` to `bstd::mem::pool`; `file.pin`, pooled receive buffers, records read in place; no `mmap` anywhere in bstd | the pool's simulation test still passes from its new home; the store builds on it |
| S1g | `bstd::audio`: `AudioOut`, `AudioIn`, PCM buffers, the mixer and resampler as cores, the simulation's provider and one native one; the audio track in the muxer; the types of the device capabilities | a simulated run's samples equal a model's; the callback's effects are refused when it waits or allocates |
| S3b | The `Frames` layout: the calling convention, the slab, unwinding, and the profiler and backtraces walking the frame chain | the differential test against `Fibers` passes |
| S4 | The bottom ([sys](sys.md)): `bstd::sys` and `bstd::rt`, `blaze-core`'s drivers move in as `bstd::driver`, and bstd is `#![no_std]`. Slice 1 is written: `crates/blaze-sys` for Linux, the first file effects on it, bstd `#![no_std]` with one std module (`rt.rs`) | the ban list (`harness/v2/nostd.json`) is empty and no blaze crate links Rust's `std` (`harness/v2/nostd.py`); C1 still passes |
| S5 | The edition, slice 1 ("The blaze edition: opting in"): `edition = "blaze-2026.11"` makes `std` name bstd and its prelude the crate's, `main` takes the roots the runtime makes (`entry-takes-roots`), `bstd::entry::run` makes them, `Out` is standard output, `blaze new` writes it where bstd is named, and `blaze::not_latest_edition` | built: a new-edition hello world builds, prints its line through `Out` and exits 0, and one returning `Err` prints the error and exits 1; the same program on Rust 2024 is unchanged; each direction of the boundary links; a bad `main` gets the named error (`crates/blaze-engine/tests/repro.rs`, `the_blaze_edition_*`) |
| S5b | Tests of the edition: a `#[test]` takes the roots `main` may, handed `Io::sim(seed)`; `#[test(native)]` the real one; a harness that names no Rust `std` | `blaze test` of an edition package runs its tests on the simulation and prints the seed of a failure; today it stops naming this row |
| S5c | The OS sandbox of a program derived from what `main` hands out | a program of the edition that never opens the network cannot, checked by the sandbox |
| S5d | bstd ships with blaze and is given to every crate of the edition as `std` is given (`attach_std`), so the manifest's `bstd` line goes; `blaze_new::BSTD_SHIPS` and `ready::EDITION_SHIPS` turn on | `blaze new` then `blaze run` prints its line on a machine with no checkout of blaze; the release's install check uses it |
| S5e | `Out` (and standard input) as effects of the drivers, with the context's deadline | `rt::write_out` is gone from S4's ban list |
| S5g | No global heap in the edition, at build time: the `global_alloc` effect (S2) is an error in a crate of the edition over what it reaches, old-edition dependencies proved or trusted with their heap as one named arena | a program of the edition with `Vec::new()`, `Box::new`, `format!` or `extern crate alloc` + `vec!` fails `blaze build` naming the call path and `Vec::new_in(cx.frame())`; today only `blaze check` refuses it (`blaze::std_use`) |
| S5f | A build script and a proc-macro target of an edition package take capabilities (`ready::CAPABILITIES_FOR_BUILD_SCRIPTS`) | they are of the edition, not Rust 2024 |
| S6 | C dependencies as wasm, the registry, the migration (`blaze fix --edition`, which rewrites `main`, `std::` paths the table of `blaze::std_use` covers, and the manifest) | each has its own row when S5c is met |

S1 and S2 do not depend on each other. S3 is the long one: about 270 files name `std::fs`, 280
`std::env`, 200 `std::process`, 200 `std::thread`, 160 the clock, and 410 `std::sync`.

**S3's worklist is a lint.** `blaze::std_use` finds
each use of an item of Rust's `std` that bstd replaces, by what the use resolves to, once per
item per function, and names what is written instead from one table of rows (ambient
authority, hidden cost, global-heap allocation, renamed) that is revised as bstd's API
settles. `harness/v2/std_uses.py` runs it over the workspace and writes, per crate, the
findings by kind and item, how many have a fix, and the number of functions whose signature
changes, with the crates in layers: a layer depends on no findings of its own, so it is one
batch, leaves first. S3 is met when that script reports no finding and every crate has the
attribute. Only the renames are mechanical (`blaze check --fix` writes `bstd::sync::Shared`
for `Arc`, and the like); taking a capability is a change of signatures, made by hand a layer
at a time. The lint is `allow` until a crate has `#![blaze::capabilities]`; blaze's own
workspace has it at `warn` from the day `BSTD_IN_TREE` (`crates/blaze-lint-defs/src/ready.rs`)
is turned on, which is when S1 compiles on a stage-1 blaze and S3 starts.

Ordinary-edition dependencies stay usable: the walk proves, from source, that what the program
reaches in them has neither effect. Most pure libraries pass unchanged; one that fails is refused
with its path, or put on the trusted list with a reason.

## Decided

- **A new library, not Rust's `std` with parts hidden.** The parts blaze wants gone (ambient
  calls, blocking without deadlines, its locks and maps) are most of it, and the bottom has to be
  the driver either way.
- **`Io` is a concrete value**, as Zig's `std.Io` is passed explicitly, without Zig's vtable in
  every signature and without a generic.
- **No `async`.** One kind of function; fibers first, task frames after.
- **The layout is a build setting**, not a type parameter.
- **Context is a parameter**, as Go's `context` and Zig's allocator are, never a thread-local: a
  thread-local context is ambient authority over time and memory.
- **No global heap.** Out of memory fails a task, never the process.
- **`main` names each capability it uses** (D104): `fn main(cx: &Cx, out: Out, data:
  Dir<ReadOnly>)`, each grant by its type and name; a whole `Io` cannot be asked for ("The
  blaze edition: opting in").
- **The opt-in is the manifest's `edition`**, carried to the front end as a crate attribute.
- **Structured tasks with deadlines**, as Trio's scopes.
- **Standard output and standard error are each a capability** (`out: Out`, `err: Out`: one
  type, the parameter's name says which). Reversed 2026-11-12 from "standard error is not a capability": a program that
  reports on standard error is saying something to whoever started it, as on standard output.
- **No revocation** until something needs it.
- **No default for a cost or a guarantee** (the second rule); a policy that changes an effect
  is a function of its own.

## What Rust's std got wrong, and what bstd does

Andrew, 2026-11-15 20:54: "anything rust or rust stdlib got wrong? maybe try to fix these in our
own std library (also consider things LLMs do that are explicit that could make non
traditional code that can be optimized a lot)". std cannot change these: it is one library
for every edition. bstd can. Status is as of that evening.

| std's choice | what it costs | bstd | status |
|---|---|---|---|
| Ambient authority: any code opens files, sockets, the clock, the environment | nothing can be sandboxed, simulated or audited | capabilities handed to `main` | built |
| A hidden global heap; `String` and most collections take no allocator | every byte unowned; no task memory | the allocator is named at every allocation | built for the new crates |
| Allocation failure aborts the process | one big build kills the tool | `try_` forms; a failure fails one task | written; `every_failure` tests |
| `Drop` cannot fail, take an argument, or be required | an unflushed writer loses data silently | values that must be finished | linear under blaze; a debug stop under rustc |
| `Clone` is one word for a bit copy, a count bump and a deep copy | hidden allocation; 3,347 `// clone:` comments in this tree | `share()`, `clone_in(alloc)` | being added |
| `as` converts silently; overflow wraps in release and stops in debug | 16,104 bare casts here | named conversions (`blaze-int`); checks by crate | written; ratchet |
| Thread-locals are statics; `Mutex` poisons | hidden per-thread state, 233 `thread_local!` here | `PerWorker<T>`, a value; ranked locks | being built with the threaded layout |
| `async`: `Pin`, wakers, cancellation by drop | cancel-safety bugs; no structure | tasks in scopes with deadlines | one thread built; many being built |
| `fmt`: every `{}` goes through `fmt::Arguments` and `dyn Write`; `format!` allocates | slow, large, opaque to the optimizer | typed writers straight into a string (`push_u64`, `push_hex`, `Show`) | to build |
| `Hash` feeds a `Hasher` field by field; SipHash with a random seed | slow hashing; map order differs per run | fixed seed (built); one hash of a flat value's bytes | to build |
| `FromStr` cannot borrow from its input; `?` converts errors through `From` unseen | forced copies; hidden code at every `?` | `Parse<'a>`; conversions written out | to build |
| `usize` for every index and length; any `usize` indexes any slice | twice the index memory; wrong-table bugs | typed 32-bit ids (built); an index type per table | partly |
| Closures have no name | a task cannot be stored, keyed, sent to another machine or cached | a job is a function and a flat value of arguments (`Eval`) | designed |
| Iterator adaptors are free only after heavy inlining | slow in a fast tier that does not inline | whole-slice and batch methods first; plain loops in hot code | to do |
| Everything generic by default | each use compiles again | a concrete core under a thin generic shell; the allocator as a value, not a type parameter ("One handle, not a type parameter") | the allocator decided 2026-11-15 and being built; the rest to decide |
| `println!` locks and flushes per line | slow tools | `Out` with a buffer the caller owns | built |
| `Path`, `OsString`, `CStr`: one idea, six types | conversions and lossy fallbacks | typed names | built |

### What an author that does not tire can write

Much of Rust's library design buys brevity for a person typing. When the author is a model,
length is free and the explicit form is the better one, because the compiler can see more:

- **Facts stated once, used many times.** A contract on a function (`requires i < len`), a
  proof-carrying index, a `Verified` view: the check moves to one place and every later use is
  unchecked without `unsafe` at the use (rule 74).
- **Effects on every function** (`pure`, `nonblocking`, `nopanic`, `noalloc`; bstd has 127
  `pure` already): a pure function is cacheable by its code identity, reorderable and safe to
  run in parallel, and a `nopanic` one needs no unwinding tables.
- **Data laid out by hand**: columns instead of rows, 32-bit ids instead of pointers, batches
  instead of one item per call, sizes counted before filling so nothing reallocates.
- **Machines instead of sugar**: a state machine the simulation can replay, where a person
  would have written `async`; an error set per function, where a person would have boxed.
- **Specialized code where it pays**: a table for one key type instead of a generic map, a
  hot path written out, because writing it twice costs nothing.
- **Advice from the running program.** The run already counts calls and can see sizes and
  types at hot call sites; the compiler knows which generic was made how many times and which
  bounds checks it could not remove. Each is a finding record (rule 81) an agent can act on:
  blaze tells the author where the explicit form would pay. Not built.

### SIMD

Andrew, 2026-11-15: "we love simd"; "there is some simd stuff in rust's, you should be using simd
properly". The vector API is the language's own, `core::simd` (`#![feature(portable_simd)]`,
in the pinned library): `Simd<u8, N>`, masks and `to_bitmask`/`first_set`, `swizzle_dyn` (the
table lookup that classifies bytes), `simd_eq` and friends. One source serves NEON, SSE2, AVX2
and AVX-512, the compiler picks the instructions, and a target with no vectors still builds.
Rules: (1) byte and word scanning, classification, comparison of many keys at once,
bit-unpacking and digit conversion are written in `core::simd` first; (2) `core::arch`
intrinsics only where the portable form cannot say it (carry-less multiply, CRC and AES
instructions, a shuffle the portable API lacks), behind `cfg(target_arch)`, with the portable
version kept as the reference the tests compare against; (3) a wider vector chosen at run
time (AVX2 over SSE2) is a function compiled with `#[target_feature]` and picked once from a
`Cpu` value the caller passes (rule 75), never detected inside; (4) every vector routine has
a scalar twin and a test that the two agree on random input and at every alignment;
(5) each claims a speedup only from its cost example, run on the machine and quoted. Where it
is used: crates/blaze-memchr (search), blaze-swiss (group probe), blaze-lex (lexing, body
skipping, UTF-8), blaze-solve (candidate fingerprints), blaze-text (digits), blaze-columns
(bit-unpacking), blaze-blake3 (its own intrinsics, by rule 2).

### What std makes easy to get slow, and bstd's form

Each row is a mistake that ordinary Rust code makes without noticing, because the short name
hides the cost. bstd's form says the cost or removes it; being longer or unusual is accepted.
State as of 2026-11-15.

| what std makes easy | what it costs | bstd's form | state |
|---|---|---|---|
| `Vec<Vec<T>>`, `Vec<String>`, `HashMap<K, Vec<V>>` | an allocation and a pointer a row; rows scattered over the heap | `Lists<T>`: one array of items and one of row ends; `Texts`: one byte buffer and its offsets; a multimap as sorted pairs | built in blaze-own (`Lists::from_pairs_in` is the multimap). At 1e6 rows: `Lists` bytes 1.00x the floor sized, 1.78x grown, against `Vec<Vec>` 2.32x; build 6.74 ns a row against a 5.52 floor; `Texts` 1.04x the floor against `Vec<String>` 4.31x |
| `map.entry(key.to_string())` | the key is allocated on every hit | `get_or_insert_with(&key, make)`: the owned key is made only on a miss | built on blaze-own's `SortedMap` and `BTreeMap`; the hash maps are the foundation lane's |
| `v.remove(i)`, `v.insert(0, x)`, `v.contains(&x)`, `s.chars().nth(i)` | linear work behind a short name; quadratic in a loop | the name says the cost: `remove_shift`, `remove_swap`, `insert_shift`, `scan_for`; text has no `nth` | built on `Vec`, `VecDeque`, `Soa`, `FixedVec`; old names deprecated, callers to move by tool |
| one `Vec` grown to a gigabyte | each doubling copies everything and holds old and new at once | `Segs<T>`: append-only segments, nothing moves, an index is two shifts; `SharedSegs` for one appender and many readers | built. 1e8 `u64`: append 1.07 ns against `Vec` 1.20; random reads 4.71 against a slice 4.10. Peak memory equal (802 against 804 MB): macOS remaps a large realloc, so `Vec` holds no old-plus-new physically there; the gain is no copy and stable references |
| `Arc::clone` in a hot path on many cores | every clone writes one shared cache line | a value many workers share is borrowed from a scope, or cloned once per worker at spawn; a clone of a shared handle inside a per-item loop is a defect; `share()` makes each one visible | measured 2026-11-15 on 18 threads (Mac, `alloc_cost arc`), ns a clone and drop: one shared `Arc` 215.6, a value per thread 4.16, a scope borrow 0.52. No biased count: the owner's plain count leaves the other 17 threads on the shared atomic, the case measured |
| `read_dir`, then `metadata` on each entry | a system call a file | the names and stamps of a directory in one call | being built |
| `File::open(path)` from the root each time, then `read_to_end` | a path walk a component, an `fstat`, a buffer filled with zeros first | open relative to a `Dir`; the size from the stamp; read into unfilled capacity; a small file in one open, read, close | partly there |
| `sync_all` by habit; a rename that syncs | about 5 ms a file on macOS | durability is in the type; derived data is unsynced by default | a bstd bug of exactly this kind was found on 2026-11-15 (unsynced publishes renamed through the syncing rename); fixed, and asserted per call by `tests/syncs.rs` |
| `println!`, `eprintln!` | a lock and a write a call; stderr is unbuffered | one buffer a command, flushed when full, at the end, or a line at a time only to a terminal | to build |
| dropping everything before exit | freeing memory the kernel frees faster | a command prints its answer and exits; frames are not unwound at exit | to check on the exit path |
| `available_parallelism()`, `Instant::now()`, `RandomState::new()` in hot code | files read, a clock call or a seed drawn, each call | the topology, the clock and the seed are values made once and passed (rule 75) | there |
| a 300-byte struct moved by value through layers | a copy a hop | built in place (`make`); the nightly `large_assignments` lint with a limit | lint to turn on |
| a `String` for every short text | 24 bytes and a heap block each; equality follows a pointer | interned ids for names; for columns of text a 16-byte string (length, a 4-byte prefix, inline up to 12 bytes) | to build with the query engine |
| fresh memory for each task | a page fault every 16 KiB | a worker's frame is reset, not returned; large pages for big tables on Linux | to measure on Linux |
| per-thread counters side by side | false sharing | `PerWorker<T>` padded to the target's cache line (128 bytes on Apple silicon) | to check |

## The fast path is chosen by the type

Andrew, 2026-11-15: "add specialization EVERYWHERE for optimization, i.e. sort should =
sort_unstable if plain old data, and stuff like that". The library picks the fastest algorithm
from the element type, without the caller asking, wherever the faster path gives the same
result for every input. That last part is the rule: a fast path must be unobservable.

"Plain old data" is not the condition for a sort. A `(key, payload)` struct with a
hand-written `Ord` on the key is plain data, and a stable sort of it differs from an unstable
one. Stability is unobservable exactly when values that compare equal are one value: integers,
`bool`, `char`, floats under `total_cmp`, arrays and tuples of those. That is
`blaze_sort::RadixKey`. A struct is one only when its type says so (`radix_key!`).

Three tools, chosen per case:

- A specialization trait (`#[rustc_specialization_trait]`, under `min_specialization`) when a
  generic method must choose for its caller: `RadixKey` (order is the image's, equal is the same
  value), blaze-hash's `PlainKey` (`==` is byte equality), core's `TrivialClone` (a clone is a
  copy), core's `TrustedLen` (an exact length).
- A `const { .. }` branch on `size_of`, `align_of`, `needs_drop` or an associated const, when the
  bound is already there and only the kernel differs. The dead arm is removed.
- Reflection (`core::mem::type_info`) only for a property a type's shape decides. Whether `Ord`
  is derived is not in the shape, so sorting does not use it; and reflection on a tuple panics
  the pinned compiler (crates/blaze-std/src/own/raw.rs).

Each path is tested equal to the general form on random input, and timed against it in its
crate's cost example. State as of 2026-11-15, from reading the five crates; "who" is the lane
that owns the crate.

| operation | where | there now | missing | unobservable because | who |
|---|---|---|---|---|---|
| sort, sort by key, search, `==`, `cmp`, `is_sorted`, `dedup`, `min` | crates/blaze-sort | radix sorts for `RadixKey`, a stable radix sort by key for any `T`, branch-free search, vector `position`/`==`/`cmp` for `PlainKey`; measured in its crate notes | `RadixKey` for blaze-kit's `NonMax` and `id!` types | equal keys are one value; `==` is byte equality | blaze-sort (done) |
| radix sort, twice | crates/blaze-radix and crates/blaze-sort | two LSD sorts: blaze-radix on std, parallel past 100,000 keys, used by blaze-core and blaze-graph; blaze-sort `no_std`, serial, with the in-place sort and the specialized front doors | one home (rule 30): blaze-sort's kernels with blaze-radix's parallel passes and CSR; then ban the other name | same algorithm, same output | blaze-sort and blaze-radix lanes |
| `RawTable::clone` (every hash map and set) | blaze-hash/src/raw.rs:506 | each live slot cloned and its control word set one by one | control words and slots as two block copies when `T: TrivialClone` | `TrivialClone`; same slots, so same iteration order | blaze-hash |
| fill: `resize`, `vec_in![x; n]` | blaze-own/src/vec.rs:463, :85 | a clone and a write each | a fill with no per-value count for `TrivialClone`; `allocate_zeroed` when the value's bytes are all zero and the vector is new | the bytes written are the same | blaze-own |
| zeroed tables | blaze-kit/src/sparse.rs:94; blaze-swiss/src/lib.rs:286; blaze-own/src/lists.rs:84; blaze-kit/src/table.rs:509 | `resize(n, 0)`, `resize_with(n, empty)` | the fill row's `allocate_zeroed`: fresh pages are zero already | 0 (or `None` of a niche id, or an empty control word) is all-zero bytes | blaze-kit, blaze-swiss, blaze-own |
| `Dict` hashing | blaze-kit/src/dict.rs:58 | `FixedState.hash_one`, through `Hash` | `hash_plain` for `K: PlainKey` | `==` is byte equality; codes follow push order, so slots are never seen | blaze-kit |
| ids as keys | blaze-kit/src/id.rs:44, :113 | `NonMax` and `id!` types hash through `write_u32`; no `PlainKey`, no `RadixKey`, no `SearchKey` | the three impls in `id!` (one line each) | the stored complement is one-to-one, and the order is the index's | blaze-kit |
| `SortedMap`/`SortedSet` `Extend`; `BTreeMap` `Extend`, `append` | blaze-own/src/sorted.rs:416, :709; btree.rs:1504, :1226 | one `insert` an item: `O(n)` moves each in the sorted vector | append, sort (stable, by key), keep the last of equal keys; a tree built bottom-up | the same later-wins map | blaze-own |
| `SortedSet::from_vec`, `Soa::sort_by_key`, `OrderMap::sort_keys` | blaze-own/src/sorted.rs:597; soa.rs:808; blaze-hash/src/order.rs:370 | comparison sorts (`sort_unstable`, pairs with a row tie-break) | `blaze_sort::sort` for `RadixKey` keys; `sort_by_key_in`, stable, so no tie-break | a set's equal keys are one value; the by-key sort is stable by construction | blaze-own, blaze-hash |
| search in sorted vectors and B-tree nodes | blaze-own/src/sorted.rs:84, :618; btree.rs:231, :247 | `binary_search_by`; a linear scan with an early exit per node | `blaze_sort::lower_bound` for `RadixKey` keys; a branch-free count of keys below in a node (as `StaticSearch` does) | the order is the image's | blaze-own |
| `SortedMap::try_clone_in` | blaze-own/src/sorted.rs:436 | one `try_clone_in` a key and a value; spec.rs's table says it takes the `Copy` block copy, and it does not | delegate to the entries' `try_clone_in`, which does | `(K, V): Copy` | blaze-own |
| `BTreeMap` clone | blaze-own/src/btree.rs:818, :1447 | a closure call an entry | a node's key and value columns as block copies | `TrivialClone` | blaze-own |
| `VecDeque` `==`, `cmp` | blaze-own/src/deque.rs:726-740 | `iter().eq`, `iter().cmp` | compare the two `as_slices` halves, so core's `memcmp` (or `blaze_sort::cmp`) applies | byte equality (`PlainKey`) | blaze-own |
| `BTreeMap` `==`, `cmp` | blaze-own/src/btree.rs:1462, :1476 | entry iterators | leaf by leaf over slices when the shapes match | byte equality | blaze-own |
| `contains`, `position` | blaze-own/src/spec.rs:120 (`Find`) | blaze-memchr for `u8`, `i8`, `bool` | 2-, 4- and 8-byte plain keys by vector compare: `blaze_sort::position` has it | `==` is byte equality | blaze-own |
| decimal text of `u8`, `u16`, `u32` | blaze-text/src/int.rs:46, :168 | every width through the `u64` writer into a 40-byte buffer zeroed each call | a buffer and digit count by width (3, 5, 10 digits), not zeroed | the same digits | blaze-text |
| hex and binary text | blaze-text/src/int.rs:101; write.rs:75-111 | `u128` arithmetic for every width | a `u32`/`u64` loop by width | the same digits | blaze-text |
| `SortedTexts` | blaze-own/src/texts.rs:210 | indices sorted by comparing their strings, then deduplicated | an MSD radix sort on the bytes | equal strings are merged anyway | blaze-own |
| `RawTable::allocate` | blaze-hash/src/raw.rs:600 | `write_bytes` over the control words | `allocate_zeroed` for the block | 0 is empty | blaze-hash |
| drop of values with no drop glue | blaze-kit/src/inline_vec.rs:542; blaze-hash/src/raw.rs:554 | skipped by `needs_drop` | nothing | no code runs either way | there |
| slice `==` and hashing in the collections | `Vec`, `FixedVec`, `InlineVec`, `Lists`, `Texts`, `SortedMap` | through slices: core's `memcmp` for same-width integers; `hash_slice` | nothing beyond `blaze_sort::eq` for arrays and newtypes core does not mark | byte equality | there |

The five worth most, in order: the hash table clone (every map copy); zeroed allocation for
fills (sparse sets, swiss tables, lists); bulk `Extend` of the sorted and B-tree maps
(quadratic today); `PlainKey` and `RadixKey` for blaze-kit's ids, with `Dict` hashing by bytes;
and the sorts and searches of blaze-own's sorted collections moved onto blaze-sort, with
blaze-radix merged into it.

## Prior art

cap-std and WASI preopens (directories with no ambient authority), Zig's `std.Io` (IO passed
explicitly, swappable for tests), Trio and Kotlin (structured concurrency, cancel scopes),
TigerBeetle and FoundationDB (one IO interface, deterministic simulation), quinn-proto and wasm-host's guest ABI (protocols and programs as explicit state machines over ops and events), Austral and Pony
(capabilities in the language), E and Fuchsia (narrowing, handles with rights), Capsicum and
Landlock (the OS side).

## Nightly features blaze's own code uses

The owner, 2026-11-15: use any nightly feature that makes blaze better. Each row below was
probed at the pin, `nightly-2026-10-27` (rustc 1.101.0-nightly 16d1ffac1), on an M-series
Mac, with a small program of the kind blaze writes, one file each in
/Volumes/Projects/blaze-int-1112-notes/nightly-probes/ (`run.sh <file>` builds with
`-O -C target-cpu=native`, runs it and keeps the log in `out/`). Times are the best of 5 to 9
runs, one machine, one day. The noise band matters: two functions that compiled to the same
instructions measured 0.84 to 1.04 of each other (hints.rs), so a ratio inside 0.90 to 1.10
is no effect until a profile says otherwise.

Already used in the new crates, not probed again: allocator_api, portable_simd,
min_specialization, trivial_clone, type_info, thread_local, rustc_attrs, dropck_eyepatch,
coerce_unsized and unsize, fn_traits, const_trait_impl, register_tool, custom_inner_attributes.
Also already on in live crates: decl_macro (hir-def), macro_attr (blaze-cli, blaze-std),
contracts (blaze-std; see its row), fn_delegation (blaze-fmt).

### Speed

| feature | at the pin | what it buys | where | verdict | the probe's number |
|---|---|---|---|---|---|
| `loop_match` | incomplete | a state machine whose transitions are direct jumps: `loop { match }` goes back through a compare tree on the state every byte | blaze-lex's lexer, blaze-deflate's inflate loop, blaze-json, blaze-cbor, blaze-h1's parser | try behind a measured twin (incomplete: `#![allow(incomplete_features)]` with the reason) | a byte-class tokenizer over 64 MB: 38.8 ms against 112.1 ms (0.346, 1730 against 598 MB/s); no jump table in either, the plain one re-tests the state at its loop head (loop_match.rs) |
| `explicit_tail_calls` with `extern "rust-preserve-none"` | incomplete; plain unstable | a threaded interpreter: each handler ends in `become` to the next, compiled to `br x0` with no frame on the hot path, the interpreter's state in callee-saved registers | any bytecode or MIR interpreter (const eval, the tier-0 runner) | try behind a measured twin | 0.280 against 0.475 ns an instruction (0.590), 350M instructions (tail_calls.rs) |
| `core_intrinsics` `prefetch_read_data` | internal | the next lookups' lines requested before they are needed | a chain of dependent lookups whose addresses are known a batch ahead (a store index walk, a probe sequence) | try behind a measured twin; not for independent lookups | dependent chain into 256 MB: 117.36 to 4.15 ns a lookup (0.035); independent lookups: 3.66 against 3.68 (out-of-order execution already overlaps them) (prefetch.rs) |
| `StaticSearch` with prefetch (blaze-kit) | as above | tried in the crate, reverted | blaze-kit | not yet | prefetching all 17 children each level: 1.5 to 2.7 times slower; a batched `lower_bounds` of 8 with the next node prefetched: 42.58 to 37.57 ns at 63.5M keys (0.88) but 2.04 to 3.87 at 1,000 and slower at every size below 4M in the crate's example (static_search.rs; the probe's own baseline was `#[inline(never)]` and overstated the gain) |
| `core::hint::select_unpredictable` | stable | a select where LLVM would branch | only where the assembly shows a branch | use when the assembly asks | binary search: the plain `if` already compiles to `csel`, the two functions are the same instructions (hints.rs) |
| `core::hint::cold_path`, `likely`/`unlikely` | stable; `likely_unlikely` unstable | block layout of the rare arm | rare arms in hot loops | no measured effect; use only with a twin | probe loop with tombstones: 0.913 to 1.033 (cold_path), 0.935 to 0.966 (unlikely); blaze-text's float specials and escape controls: f64 Display 28.8 to 28.1 ns, Debug 35.1 to 34.7, under the 5% bar, reverted (cold_error.rs, hints.rs, blaze-text cost) |
| `extern "rust-cold"` (`rust_cold_cc`) | plain unstable | an error path the caller calls and then goes on from: the callee saves everything, so the caller spills less | a lexer or parser that records an error and continues | try behind a measured twin | the recovering loop: 66 to 58 instructions, 15 to 7 stack saves and loads; time 0.962 (noise); an error path that returns is a tail branch already, no change (cold_error.rs) |
| `#[optimize(size)]` | stable | smaller cold code | cold constructors | use on cold code for size, not speed | 0.947 and 1.029 (noise) (cold_error.rs) |
| `gen_blocks` | plain unstable | an iterator written as a loop with `yield` | set-bit walks (blaze-kit's `ChunkedBitSet`), worklists, tree walks | use now where the iterator's size does not matter | 64M bits: 24.79 against 24.97 ms (0.993); the state is 64 bytes against the hand-written 32 (gen_blocks.rs) |
| `fn_align`, `static_align` | plain unstable | `#[rustc_align(64)]` on a function, `#[rustc_align_static(64)]` on a static: a hot loop or table on its own line, so benchmarks stop moving with unrelated code | hot loops measured by cost examples; lookup tables | use now on measured hot code | the aligned function and static sit at 0 mod 64; the plain static at 32 (align.rs) |
| `vec_push_within_capacity` | plain unstable | a push that refuses instead of growing | std `Vec` callers that sized ahead (blaze-own's vectors have their own `try_`) | use now | 0.251 against 0.250 ns a push (1.005) (library.rs) |
| `unix_file_vectored_at` | plain unstable | `pwritev`/`preadv` from std | the store's record writes and reads | use now | wrote and read 9 bytes in two slices at an offset (library.rs) |

### Language

| feature | at the pin | what it buys | where | verdict | the probe's result |
|---|---|---|---|---|---|
| `macro_derive`, `macro_attr`, `macro_metavar_expr`, `macro_metavar_expr_concat` | plain unstable | derives and attributes in `macro_rules!`: no proc-macro crate, nothing to sandbox or compile to wasm | small derives (field counts and names, getters, variant names), span attributes | use now for items without generics; a generic item must be parsed by hand (not probed) | a struct's 3 fields and names, `get_<field>` getters by `${concat}`, `${index()}`, an enum's 3 variant names, a `#[traced("kit")]` attribute: all right (macros.rs) |
| `decl_macro` (`pub macro`) | plain unstable | a macro that is an item: named by path, private by default | every crate with a tool attribute at its root (agents/annoyances/crate-root-tool-attribute-hides-macros.json) | use now in place of `#[macro_export]` in such crates | under `#![blaze::sans_io]`: `crate::macros::value!` from another module and from another crate both compile; the `#[macro_export]` twin is refused with the recorded error (decl_macro_root.rs, decl_macro_lib.rs, macro_export_root.rs) |
| `fn_delegation` | plain unstable | `reuse Trait::{a, b} { self.0 }` forwards a trait through a newtype | newtype ids and wrappers in blaze-kit and blaze-own | use now | three methods forwarded, right answers (ergonomics.rs) |
| `default_field_values` | plain unstable | `Opts { x: 1, .. }` with defaults at the field | options whose defaults are decisions; not bounds the caller must choose (`Limits` stays without) | use now | right (ergonomics.rs) |
| `deref_patterns` | plain unstable | match through an interned handle | the front end's interned types | use now; the type needs `unsafe impl DerefPure` (`deref_pure_trait`), an obligation to state | right through a user `Deref` (ergonomics.rs) |
| `arbitrary_self_types` | plain unstable | methods on a handle (`self: Handle<'_, Node>`) through `Receiver` | arena and table handles | use now | right (ergonomics.rs) |
| `super_let`, `try_blocks`, `impl_trait_in_assoc_type` | plain unstable | a borrowed temporary from a block; `?` without a closure; an unboxed iterator type in a trait | everywhere they read better | use now | right (ergonomics.rs) |
| `const_closures` | plain unstable | closures called at compile time | `const` tables built by a function | use now; the bound needs `[const] Destruct` (`const_destruct`) | right (ergonomics.rs) |
| `step_trait` | plain unstable | `a..b` of typed ids iterates ids | blaze-kit's `id!` types | use now (five methods to write, `forward_overflowing` and `backward_overflowing` among them) | `ItemId(3)..ItemId(7)` gives four ids (ids.rs) |
| `pattern_types` | internal | a niche without a transform | ids | not yet: no gain | `Option` of a `u32 is 0..=0xFFFF_FFFE` is 4 bytes and sums 16M at 0.998 of blaze-kit's complement `NonMax`; `#[rustc_layout_scalar_valid_range_end]` is gone at the pin ("cannot find attribute") (ids.rs) |
| `unsafe_fields` | incomplete | a field whose every write is an `unsafe` block: an obligation the type states (rule 76) | lengths and counts that memory safety rests on (blaze-own, blaze-kit's `InlineVec`) | try in one crate | right (incomplete.rs) |
| `generic_const_items`, `sized_hierarchy` | incomplete; plain unstable | `const SLOTS<T>`; a `MetaSized` bound | per-type constants; size-of-value bounds | try where it reads better | right (incomplete.rs) |
| `ergonomic_clones` | incomplete | `.use` and `use ||` | none | not yet: blaze keeps each clone explicit with its reason | right (incomplete.rs) |
| `coroutines` | plain unstable | resumable functions | none | not yet: blaze writes sans-IO machines by hand (rule 14), `gen` covers iterators | right (incomplete.rs) |
| `guard_patterns` | incomplete | a guard inside an or-pattern | none | **never at this pin: wrong code** | `(b'"' if !in_string) \| b'\''` matched every byte (`'7'` and `'x'` too) and the guard's variable was reported unused, with and without `-O`; the arm-guard twin is right (guard_patterns_bug.rs) |
| `contracts` | incomplete | `requires`/`ensures` on a function | invariants stated once (rule 53) | not yet: off by default (a broken `ensures` returned unchecked); with `-Zcontract-checks=yes` a broken one is a non-unwinding panic that aborts the process (rule 20 wants one job failed); an `ensures` closure may not borrow an argument ("requires that `'1` must outlive `'static`"); blaze-std turns it on: check what it relies on | cost on a hot call 0.979 to 0.985 (noise) (contracts.rs) |
| `custom_test_frameworks` | plain unstable | `#[test_case]` items handed to a runner of ours | gathering `blaze_sim::check` cases | not now: the cases are functions already | the runner got both cases (test_frameworks.rs) |

### Library

| feature | verdict | the probe's result (library.rs) |
|---|---|---|
| `iter_array_chunks` | use now on iterators (slices have the stable `as_chunks`) | right |
| `maybe_uninit_fill` | use now | right |
| `ascii_char` | use in blaze-text for ASCII-only buffers | right |
| `uint_gather_scatter_bits` (`extract_bits`, `deposit_bits`) | try for packed keys; cost on arm64 not measured | a 12-bit field out and back, right |
| `nonpoison_mutex` | not used: blaze's locks are its own (rule 12); note it is not `RefUnwindSafe` | the value after a panic under the lock is kept |
| `sync_unsafe_cell` | use now for statics with their own synchronization | right |
| `thread_spawn_hook` | not yet: an ambient hook (rule 75) | the hook ran for the named child |
| `ptr_metadata` | use now | a slice pointer's length round-trips |
| `const_heap` | try for compile-time tables | a 4-element table allocated in `const` |

### Two more

| feature | at the pin | what it buys | where | verdict | the probe's result |
|---|---|---|---|---|---|
| `large_assignments` with `#![move_size_limit = "N"]` | plain unstable | a warning at every move of a value over N bytes, after monomorphization | a ratchet over the whole tree, a limit per crate | use now at 256 as a finding, not a deny: it names real copies | a 300-byte struct through three functions: at 128 and 256, 7 warnings in a debug build (each of the three passes, the construction, the request's 496-byte profile made and returned) and 4 at `-O` (the passes inside the `#[inline(never)]` functions are gone); none at 1024; none without the attribute. Mirrors of blaze-drive's `Profile` (496 bytes, `Copy`, returned by value from the request) and `Request` (520) pass both 128 and 256; its `Condition` (66 bytes) passes neither; by `-Zprint-type-sizes`, 3 of the probe's 65 types are over 128 bytes, 3 over 256, none over 1024 (large_moves.rs) |
| `read_buf`, `core_io_borrowed_buf` (`Read::read_buf` into a `BorrowedBuf` of `Vec::spare_capacity_mut`) | plain unstable | reading into capacity without zeroing it first | the store's record reads into a frame | use now where the caller sizes the buffer itself; `read_to_end` already reads unzeroed | 64 MB from the page cache: 2.73 ms, `read_to_end` 2.80 (1.022), zero then `read_exact` 3.04 (1.112) (read_buf.rs) |

No probe panicked the compiler; no `rustc-ice-*.txt` was left.

### Flags for building blaze itself

The coordinator runs these on the whole tree; each names what to measure.

- `-Zthreads=N`, the parallel front end: a cold `cargo check` of the workspace at N = 1, 8 and
  the core count, wall time and peak memory; worth it if the critical-path crates (blaze-own,
  blaze-hash, the front end) check faster without memory passing the ceiling.
- `-Zcodegen-backend=cranelift` for test builds: `cargo test --no-run` time against LLVM at
  `opt-level=0`, and that every test still passes; the tests' own run time is the cost.
- cargo's checksum freshness (`-Zchecksum-freshness`): the incremental rebuild after a
  `touch` with no content change, which mtimes rebuild and checksums do not; measure the no-op
  build time.
- `-Zprint-type-sizes`: one pass over the tree, sorted by size, the types above 64 bytes that
  are held per item; each fat type found is a cost example's row.
- `-Zrandomize-layout` in test builds: any test that starts failing relied on a layout no type
  promised; the count of failures is the measure.
- Miri's many-seeds mode (`-Zmiri-many-seeds`): the crates with `unsafe` (blaze-own,
  blaze-kit, blaze-swiss) under 64 seeds; the measure is seeds that fail.

### Round 2 (2026-11-16)

Same pin, an Apple M5 Max. Logs are under /Volumes/Projects/blaze-int-1112-notes/nightly-probes/:
`flags/` for the binary, `out/` for the probes.

#### The `blaze` binary's own build

Each variant was built by `flagbuild.sh` from blaze-cli's green snapshot b3c8abd52b into its own
target directory (the build line and time: `flags/<name>.txt`). `flags/measure.py` ran each binary
on copies of two inputs: the hello package, and core's module tree as one package without
dependencies (267 files; `mod.rs` files renamed to the sibling form and `io`, `panic`, `f16` to
`f64` left out, since this blaze stops on `#[path]` and on `x/mod.rs`). Instructions retired and
footprint are the median of 5 runs under `/usr/bin/time -l`; wall time is the best of 50 runs,
the variants alternated. Instructions are the column to trust: wall times of one binary moved by
several percent between rounds. Results: `flags/round-a.txt` (and `.json`).

| variant | build (s) | size (bytes) | imports | no command | hello repeat | cold | repeat | edit | cold wall (ms) | edit wall (ms) |
|---|---|---|---|---|---|---|---|---|---|---|
| release as it is (incremental, 16 units) | 22 cold, 14 rebuild | 5,255,856 | 187 | 23.88M | 27.07M | 1,970.9M | 47.26M | 1,796.7M | 128.8 | 119.0 |
| incremental off, 1 unit | 31 | 4,499,920 | 187 | 23.88M | 26.91M | 1,909.6M | 47.01M | 1,754.5M | 126.9 | 115.7 |
| + thin LTO (`dist` as it is) | 29 | 4,490,208 | 187 | 23.78M | 27.05M | 1,798.7M | 46.91M | 1,640.5M | 120.6 | 112.4 |
| + fat LTO | 40 cold, 34 rebuild | 4,061,472 | 187 | 23.72M | 26.73M | 1,798.9M | 47.05M | 1,634.5M | 121.1 | 111.9 |
| fat + `target-cpu=apple-m1` | 41 | 4,061,472 | 187 | 23.75M | 26.81M | 1,803.4M | 46.77M | 1,627.0M | 121.1 | 111.2 |
| fat + `target-cpu=native` (apple-m5) | 40 | 4,028,752 | 187 | 23.79M | 26.78M | 1,795.3M | 46.87M | 1,624.5M | 124.0 | 112.1 |
| fat + `panic=abort` | 39 | 3,492,336 | 185 | 23.55M | 26.57M | 1,783.5M | 46.56M | 1,616.0M | 124.1 | 112.7 |
| + `-Zlocation-detail=none -Zfmt-debug=none` | 36 | 3,245,104 | 185 | 23.36M | 26.43M | 1,783.1M | 46.36M | 1,615.8M | 123.5 | 109.0 |
| fat + `-Zbuild-std=std,panic_unwind` | 45 | 4,055,520 | 187 | 23.71M | 26.75M | 1,793.7M | 46.65M | 1,632.6M | 123.0 | 110.1 |
| previous row's flags + std built `optimize_for_size`, `panic=immediate-abort` | 38 | 2,522,464 | 168 | 22.22M | 25.17M | 1,796.7M | 45.46M | 1,637.0M | 123.8 | 112.1 |

Profile-guided, a fair pair at the later snapshot f1c9352cc3 (`flags/round-b.txt`): fat 1,800.9M
cold, 1,631.8M edit; fat with `-Cprofile-use` 1,800.7M and 1,628.0M (0.998 on the edit); the release
profile as it is 1,948.9M and 1,780.9M. The profile came from 390 runs of the workloads
(`flags/train.sh`); the instrumented build took 89 s.

What the columns are. "cold" is a check of the large package with an empty `.blaze` (267 runs);
"repeat" a check with nothing changed (the certificate stands); "edit" a check after a comment is
appended to `src/lib.rs`, from a fresh store each time: it reruns 266 of the 267 files, so it is a
near-cold check, not an incremental one. Footprint was 65 to 66 MB for cold and edit and 2.9 to
3.1 MB for repeat in every variant. "no command" prints the usage line: 23.9M instructions and
2.4 ms, against 15.8M for a Rust program that prints one line and exits and 9.0M for
`/usr/bin/true` (`flags/startup.txt`); no build setting moves it.

What it says:
- Thin LTO with one unit is where the work goes down: 8.7% fewer instructions cold and 8.7% on the
  edit than the release profile, 6% less wall. Fat LTO does the same work as thin, in a binary 9.5%
  smaller, for 11 s more build. Nothing else moves the work by more than 1%.
- `target-cpu`: `apple-m1` is already the target's default; `native` saves 0.2%. Neither is worth it,
  and `native` would refuse older Macs.
- `panic=abort` saves 0.9% of the work and 14% of the size. blaze-std's task runner catches each
  task's panic (crates/blaze-std/src/task.rs, `catch_unwind` at lines 506 and 576) so one job
  fails and the build goes on (rule 20): abort breaks that. Never. `immediate-abort` and
  `optimize_for_size` take the binary to 2.52 MB (48% of the release profile's) but need abort too.
- `-Zlocation-detail=none` and `-Zfmt-debug=none` take 7% more off the size and nothing off the
  work, and take the file and line out of every panic message (rule 19). Not for blaze.
- Rebuilding std inside the LTO: 0.3% cold, 0.1% edit. Not worth a build of std.
- Profile-guided: 0.2% on the edit at 2 builds and a training run. Not now; the `dist` comment's
  PGO buys nothing on these workloads today.
- `-Zshare-generics`, `-Zcross-crate-inline-threshold`, `-Zmir-opt-level=3`: not built. Fat LTO
  did no less work than thin, so whole-program inlining had nothing left to give; these three
  act on what LTO already sees.

Recommended `dist`: `incremental = false`, `codegen-units = 1`, `lto = "fat"`, unwinding kept,
no `target-cpu`, no `-Z` flags. Against the release profile it buys 8.7% cold, 9.0% edit, 6%
wall, and 1.19 MB (23%) of size; against `dist` as it is (thin), 0.43 MB of size for 11 s of build.
The everyday release profile stays as it is: its rebuild after this snapshot change was 14 s
against fat LTO's 34 s, and the 9% is not worth 2.4 times the rebuild in the edit loop.

Found on the way, for the owning lanes: in this blaze an edit of any file but the root, or a
second edit in one store, stops with "a part's edition byte is no edition"; a cold check of the
large package reports no findings, the edit check of the same text reports 5 (parse errors in
`future/async_drop.rs` and `iter/traits/iterator.rs`); at f1c9352cc3 a repeat check sometimes
misses its certificate ("3 store reads (2 found), certificate missed"). For profile-guided builds:
Xcode's `llvm-profdata` (Apple LLVM 21) refuses this pin's raw format ("version = 11; expected
version = 10"); the llvm-tools of nightly-2026-10-25 (LLVM 23.1.4) merge it. `-Cprofile-generate`
in `RUSTFLAGS` is passed on by cc 1.4.3 to Apple clang for zstd, blake3's NEON part and ring,
whose format-10 counters make the runtime write empty profiles ("Runtime and instrumentation
version mismatch"); `CARGO_PROFILE_RELEASE_RUSTFLAGS` with `-Zprofile-rustflags` keeps it to Rust
(`flags/findings.txt`, `flags/pgogen2.txt`).

#### Features, round 2

Probes are one file each, built by `run.sh` (`-O -C target-cpu=native`) unless the row says
otherwise; times are best of 5 to 15, the noise band as above (0.90 to 1.10 is no effect).
Instruction counts and folded functions are read from the `.s` files in `out/round2_asm.log`.

| feature | the probe's result (log) | verdict | where |
|---|---|---|---|
| `link_llvm_intrinsics` | `llvm.prefetch.p0` write/keep on a scatter into 256 MB: 3.52 against 3.46 ns (1.017); read/no-locality (`pldl1strm`) 4.90 (1.416, worse); `llvm.expect.with.probability` at 0.999: same instructions, LLVM folded the two functions into one (llvm_intrinsics.log) | never: `core::intrinsics::prefetch_*` emit the same instructions | none |
| `hint::assert_unchecked` | a gather by ids into a 64K table: the bounds check and its panic call go, the loop unrolls (21 to 46 instructions), 0.359 to 0.343 ns (0.955) (intrinsics2.log) | behind a measured twin, where the assembly shows a check | typed-id table lookups |
| `raw_eq`, `compare_bytes`, `carrying_mul_add`, `disjoint_bitor` | each compiled to the same instructions as `==`, `Ord`, the `u128` form and `\|`: 0.995, 1.001, 1.003, and one `orr` (intrinsics2.log) | never: no gain on arm64 | none |
| `three_way_compare`, `typed_swap_nonoverlapping`, `unchecked_add` | right answers (intrinsics2.log) | use where they read better | |
| `const_eval_select` | a byte count with a `while` loop in `const` and a 16-lane `portable_simd` path at run time: 3 in a `const`, 143 at run time, both right (intrinsics2.log) | use now for `const fn`s with a vector path | blaze-text, blaze-hash |
| `adt_const_params` | a `SizeClass { bytes, align }` as a pool's const parameter, 30 classes: right; compile 0.248 against 0.238 s for the `u32, u32` twin (const_params_time.log) | use now | blaze-own's size classes |
| `generic_const_exprs` | `[u64; (W * N).div_ceil(64)]` columns: right and exactly sized, but the crate falls back from the next trait solver: "`generic_const_exprs` is not supported with the next-generation trait solver ... reverted the setting to `-Znext-solver=coherence`" (const_params.log) | not yet | |
| `asm_goto_with_outputs` | `adds` then `b.cs` to a label: the same loop as `checked_add` (`adds`, `b.hs`), 1.009 (asm_goto.log) | never for arithmetic; only for a sequence no intrinsic gives | |
| CRC-32C by instruction (stable) | `crc32cx` 5.85 ms over 64 MB (11.47 GB/s) against slicing-by-8 29.27 ms (2.29 GB/s) and a byte table 131.66 ms: 0.200 of slicing-by-8 (aarch64.log) | use now, with the table as its twin | the store's record checksums |
| carry-less multiply fold (stable `vmull_p64`) | a dependent 64-bit hash fold: 3.453 ns against 1.260 for `mul` + `umulh` (2.741 slower) (aarch64.log) | never for a scalar fold | |
| unstable AArch64 families | at the pin unstable: SVE (5,562 intrinsics), SM4, RNG (`__rndr`), MTE, `_prefetch`, f16, FCMA, I8MM, LUT, RCPC3, FAMINMAX, FP8; detecting `sme`, `lut`, `rcpc3`, `faminmax`, `fp8` needs `stdarch_aarch64_feature_detection`; no pointer-authentication intrinsics. CRC, AES/PMULL, SHA3 are stable. All the unstable ones compiled (aarch64_unstable.log). This CPU has CRC, AES, SHA3, SME, MTE, PAuth, BTI, I8MM, FCMA; not SVE outside streaming mode, SM4, RNG, LUT (aarch64.log) | not yet: nothing blaze needs is unstable | |
| `naked_functions_rustic_abi` | `#[unsafe(naked)] extern "Rust"`: `add x0, x0, #1; ret` called right; a trampoline template `ldr x16, 8; br x16; .quad` read back as bytes equals the hand encoding `0x58000050 0xd61f0200` (naked.log) | use now: the assembler encodes stubs, their bytes copied from the template | blaze-link's thunk.rs and table.rs, blaze-launch-rt's arch.rs |
| `effective_target_features` | `#[unsafe(force_target_feature(enable = "crc"))]` on a safe trait method, built with `-Ctarget-feature=-crc`: right CRC; without it E0133 (effective_tf_nocrc.log) | try where a trait implementation needs a feature its `Cpu` was checked for | the `Cpu`-chosen implementations (lane rule 7) |
| `used_with_arg` | entries in a `__DATA,__blzreg` section walked by `section$start`/`section$end`, no initialiser: the 2 `#[used(linker)]` entries survive `-dead_strip`, the `#[used(compiler)]` one is dropped (linkage_U.log) | use now, `used(linker)` only | link-time tables (rules, commands) |
| `linkage = "extern_weak"` | on Mach-O the link fails, "Undefined symbols", unless `-Wl,-U,_name`; then the address is null (linkage.log, linkage_U.log) | not yet | |
| `fn_align`, `-Zmin-function-alignment` | a byte-class loop at 4, 16, 32 and 0 mod 64: 0.998 to 1.022 against the `#[rustc_align(64)]` copy; the local `__text` moved under 1 KB (align2.log) | `fn_align` stays as round 1 says; the flag: no | |
| `-Zrandomize-layout` | a `repr(Rust)` header read at byte 12 and a size assumed to be 8: both hold without it; under 8 seeds the offset assumption broke 8 times and the size one 5 (16 bytes became 24) (randomize.log) | use now in a test build | the flag list above |
| `-Zprint-type-sizes` | mirrors of blaze-drive's `Job` 280 bytes (no padding), `Task` 312 (1 byte), `Print` 160 (7), `Part` 72 (2), `Condition` 66 (1); a `Name` is 33 bytes at alignment 1 (type_sizes.log) | no padding to win; the size is four 32-byte `Vec`s and the 33-byte names | |

No probe panicked the compiler; no `rustc-ice-*.txt` was left.
