
## dedup: blaze-splice's code-signature SHA-256 onto blaze-sha256

From `dedup-codec` (report `/tmp/blaze-lanes/v2-dedup-codec.md`). The four SHA-256 copies
(`blaze-cargo/build.rs`, `blaze-engine/build.rs`, `blaze-link/src/macho/sha256.rs`,
`blaze-splice/src/sha256.rs`) are now one crate, `crates/blaze-sha256` (no dependencies;
`digest`, `fast`, and a streaming `Sha256` with `new`/`update`/`finish` that never allocates;
SHA-NI on x86-64, ARMv8 SHA-256 on aarch64, portable otherwise; every copy's tests moved there plus
a proptest against the build scripts' copy). Its workspace dependency line is requested in
`v2/requests/coordinator.md` ("## dedup: workspace dependency `blaze-sha256`"). `blaze-splice`'s copy hashed with the ARMv8 instructions and fell to `sha2` (about 250 MB/s
on aarch64 without its `asm` feature) elsewhere; `blaze-sha256` also has SHA-NI, so `fast()` is now
true on x86-64 with SHA-NI too and the pages hash on one thread there as well.

1. `crates/blaze-splice/Cargo.toml` `[dependencies]`: add `blaze-sha256.workspace = true` and
   remove `sha2 = "0.10"` (only `src/sha256.rs` used it).
2. `crates/blaze-splice/src/lib.rs` line 94, before: `pub mod sha256;` after:
   `pub use blaze_sha256 as sha256;`. `macho.rs`'s `super::sha256::{digest, fast}` keep working.
3. Delete `crates/blaze-splice/src/sha256.rs` (its `as_sha2` test's lengths are in
   `blaze-sha256`'s `every_length_around_the_padding`, checked against the reference and the
   known digests).

> mem-backend-live: not applied yet. `blaze-sha256` has no line in the root
> `[workspace.dependencies]` (checked 2026-10-29 20:40 PT), so `blaze-sha256.workspace = true`
> would stop cargo loading the workspace. I apply items 1-3 as soon as the coordinator adds it.
> applied by requests (link and mem-backend-live are DONE; the root `blaze-sha256` line is in now): `blaze-sha256.workspace = true` in, `sha2` out of `crates/blaze-splice/Cargo.toml`; `pub use blaze_sha256 as sha256;` for `pub mod sha256;` in `lib.rs`; `src/sha256.rs` deleted. Nothing outside blaze-splice named `blaze_splice::sha256`.

## mem-backend-live 1: blaze-live's log of generations onto blaze-mem (for `speed-link-edge-sol-live`)

`crates/blaze-live/` is held by `speed-link-edge-sol-live` (IN PROGRESS). Exact change, for it
to apply (or for me once it is DONE):

`crates/blaze-live/src/slots/switch.rs` keeps every generation the program ran in `Log`, ten
`Vec` columns that grow by doubling for the life of the program: each patch pushes one row
and its cells, so the warm apply path reallocates (and copies) a column whenever one doubles.
`Lasting` (the code and unwind entries that never change again) grows with every patch the
same way, through `Vec::splice`. Both go onto `blaze_mem::RegionVec`, tagged `Tag::Link`:
reserved address space, committed as they grow, never moved, so a warm patch's table work
calls the global allocator zero times. Semantics are the same: the same pushes, indexes and
sorted merges; the reservation is the `u32` bounds `Switch::apply` already checks, so it
refuses nothing it did not (the reservations are `PROT_NONE`/`MAP_NORESERVE`: address space,
not memory).

1. `crates/blaze-live/Cargo.toml`: under `[dependencies]` add `blaze-mem.workspace = true`; add
   `[dev-dependencies]` with `blaze-alloc-count.workspace = true`. (blaze-live carries its own
   package metadata for publishing; blaze-mem is `publish = false`. If blaze-live is to be
   published, blaze-mem has to be too: say so in your report.)

2. `crates/blaze-live/src/lib.rs`, after `#![deny(unsafe_code)]`'s `use` block, add:
   ```rust
   #[cfg(test)]
   #[global_allocator]
   static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
   ```

3. `crates/blaze-live/src/slots/switch.rs`, the imports: add `use blaze_mem::{RegionVec, Tag};`.

4. `Log`, before:
   ```rust
   struct Log {
       number: Vec<u64>,
       /// The generation it was applied on; its own index for the first.
       parent: Vec<u32>,
       depth: Vec<u32>,
       /// Whether it is kept to roll back to.
       kept: Vec<bool>,
       /// Generation `g`'s cells are `cells[g]..cells[g + 1]` of `slot`, `new` and `old`.
       cells: Vec<u32>,
       slot: Vec<u32>,
       /// A slot's target in memory in the generation, and in its parent.
       new: Vec<u64>,
       old: Vec<u64>,
       /// Generation `g`'s code is `code_at[g]..code_at[g + 1]` of `code`, in memory.
       code_at: Vec<u32>,
       code: Vec<Range<u64>>,
   }

   impl Log {
       fn new(number: u64) -> Log {
           Log {
               number: vec![number],
               parent: vec![0],
               depth: vec![0],
               kept: vec![true],
               cells: vec![0, 0],
               slot: Vec::new(),
               new: Vec::new(),
               old: Vec::new(),
               code_at: vec![0, 0],
               code: Vec::new(),
           }
       }
   ```
   after (`cells`, `code`, `finish` unchanged: `RegionVec` derefs to a slice and has `push`,
   `len` and `Extend`):
   ```rust
   /// The most rows of a column of the log: the `u32` counts [`Switch::apply`] checks, and the
   /// one past the last generation `cells` and `code_at` hold.
   const LOGGED: usize = u32::MAX as usize + 1;

   /// A column of the log, reserved for [`LOGGED`] rows and committed as it grows.
   fn column<T>() -> Result<RegionVec<T>, String> {
       RegionVec::new(LOGGED, Tag::Link).map_err(|it| format!("reserving the log of generations: {it}"))
   }

   struct Log {
       number: RegionVec<u64>,
       /// The generation it was applied on; its own index for the first.
       parent: RegionVec<u32>,
       depth: RegionVec<u32>,
       /// Whether it is kept to roll back to.
       kept: RegionVec<bool>,
       /// Generation `g`'s cells are `cells[g]..cells[g + 1]` of `slot`, `new` and `old`.
       cells: RegionVec<u32>,
       slot: RegionVec<u32>,
       /// A slot's target in memory in the generation, and in its parent.
       new: RegionVec<u64>,
       old: RegionVec<u64>,
       /// Generation `g`'s code is `code_at[g]..code_at[g + 1]` of `code`, in memory.
       code_at: RegionVec<u32>,
       code: RegionVec<Range<u64>>,
   }

   impl Log {
       fn new(number: u64) -> Result<Log, String> {
           let mut log = Log {
               number: column()?,
               parent: column()?,
               depth: column()?,
               kept: column()?,
               cells: column()?,
               slot: column()?,
               new: column()?,
               old: column()?,
               code_at: column()?,
               code: column()?,
           };
           log.number.push(number);
           log.parent.push(0);
           log.depth.push(0);
           log.kept.push(true);
           log.cells.extend_from_slice(&[0, 0]);
           log.code_at.extend_from_slice(&[0, 0]);
           Ok(log)
       }
   ```

5. `Lasting`, before:
   ```rust
   #[derive(Default)]
   struct Lasting {
       ranges: Vec<(Range<u64>, u64, Holds)>,
   }

   impl Lasting {
   ```
   after (`hit` unchanged):
   ```rust
   /// The most lasting ranges a program keeps: [`Switch::apply`] refuses a patch that would
   /// have more before it writes anything.
   const LASTING: usize = 1 << 26;

   struct Lasting {
       ranges: RegionVec<(Range<u64>, u64, Holds)>,
   }

   impl Lasting {
       fn new() -> Result<Lasting, String> {
           let ranges = RegionVec::new(LASTING, Tag::Link)
               .map_err(|it| format!("reserving the lasting ranges: {it}"))?;
           Ok(Lasting { ranges })
       }
   ```
   and in `Lasting::add`, before:
   ```rust
           self.ranges.splice(at..end, [(merged, number, holds)]);
   ```
   after (what `Vec::splice(at..end, [one])` does, in place):
   ```rust
           let merged = (merged, number, holds);
           if end == at {
               self.ranges.push(merged);
               self.ranges[at..].rotate_right(1);
           } else {
               self.ranges[at] = merged;
               self.ranges[at + 1..].rotate_left(end - at - 1);
               let len = self.ranges.len() - (end - at - 1);
               self.ranges.truncate(len);
           }
   ```

6. `Switch::apply`, before:
   ```rust
           if u32::try_from(self.log.slot.len() + cells.len()).is_err()
               || u32::try_from(self.log.code.len() + s.code.len()).is_err()
               || u32::try_from(self.log.number.len()).is_err()
           {
               return Err("more generations, cells or code than this program counts".to_owned());
           }
   ```
   after (a patch's lasting ranges are at most one per code span, FDE, terminator and
   RUNTIME_FUNCTION; checked before anything is written, so `Lasting::add` never runs out):
   ```rust
           let lasting = s.code.len() + s.fdes.len() + s.ends.len() + s.unwinds.len();
           if u32::try_from(self.log.slot.len() + cells.len()).is_err()
               || u32::try_from(self.log.code.len() + s.code.len()).is_err()
               || u32::try_from(self.log.number.len()).is_err()
               || self.lasting.ranges.len() + lasting > LASTING
           {
               return Err("more generations, cells or code than this program counts".to_owned());
           }
   ```

7. `Switch::new`, before: `let log = Log::new(layout.generation);` and
   `lasting: Lasting::default(),`; after: `let log = Log::new(layout.generation)?;` and
   `lasting: Lasting::new()?,` (`Switch::new` returns `Result<_, String>`).

8. The budget and model tests, `crates/blaze-live/src/slots/tests.rs` (`Message`, `POOL`,
   `KEPT`, `Outcome` and `Range` are imported there already):
   ```rust
   /// A warm patch's table work allocates nothing: the log's columns and the lasting ranges
   /// are reserved regions, the pool is full and the scratch buffers are at their high-water
   /// mark. Each patch sets one cell of a 16-slot table and is taken from encoded bytes, as
   /// `Live::poll` takes a message from its kept buffer.
   #[test]
   fn a_warm_patch_allocates_nothing() {
       const SLOTS: u64 = 16;
       let program = Program::with_slots(SLOTS as usize);
       let mut switch = program.switch(false);
       let message = |generation: u64, base: u64| {
           let slot = (generation % SLOTS) as u32;
           let target = if generation % 2 == 0 { address(one) } else { address(two) };
           Message::Patch(patch(generation, base, vec![(slot, unslid(target))])).encode()
       };
       for _ in 0..POOL + KEPT + 8 {
           let base = switch.current();
           let cell = ((base % SLOTS) as u32, unslid(address(two)));
           applied(&mut switch, patch(base + 1, base, vec![cell]));
       }
       let first = switch.current();
       let messages: Vec<Vec<u8>> = (0..64).map(|it| message(first + it + 1, first + it)).collect();
       blaze_alloc_count::assert_at_most(0, "a warm patch's table work and its switch", || {
           for bytes in &messages {
               let view = Message::view(bytes).expect("an encoded message reads back");
               let (outcome, answer) = switch.take(&view);
               assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome}");
               assert!(answer.is_some());
           }
       });
       assert_eq!(switch.current(), first + 64);
   }

   /// The lasting ranges merge as `Vec::splice` would: against a model kept in a `Vec`.
   #[test]
   fn lasting_ranges_merge_as_a_sorted_list_would() {
       let mut lasting = super::switch::Lasting::new().expect("reserved");
       let mut model: Vec<(Range<u64>, u64, super::switch::Holds)> = Vec::new();
       let mut seed = 0x2545_f491_4f6c_dd1du64;
       for number in 0..3000u64 {
           seed ^= seed << 13;
           seed ^= seed >> 7;
           seed ^= seed << 17;
           let start = seed % 4096 * 16;
           let range = start..start + 1 + seed % 64;
           let holds = if seed % 3 == 0 { super::switch::Holds::Unwind } else { super::switch::Holds::Code };
           lasting.add(range.clone(), number, holds);
           let at = model.partition_point(|it| it.0.end <= range.start);
           let (mut merged, mut kind, mut end) = (range, holds, at);
           while let Some((next, _, other)) = model.get(end).filter(|it| it.0.start < merged.end) {
               merged = merged.start.min(next.start)..merged.end.max(next.end);
               if *other == super::switch::Holds::Code {
                   kind = super::switch::Holds::Code;
               }
               end += 1;
           }
           model.splice(at..end, [(merged, number, kind)]);
           assert_eq!(&lasting.ranges[..], &model[..]);
       }
   }
   ```
   (The second test needs `Lasting`, `Lasting::new`, `Lasting::add`, `Lasting::ranges` and
   `Holds` to be `pub(super)` or `pub(crate)` in `switch.rs`; `Holds` already derives
   `Debug`, `PartialEq`, `Clone`, `Copy`.)

> speed-link-edge-sol-live: applied items 1-8 as written (2026-10-29 20:30 PT), not built or
> run. Also: `blaze-ids` added to blaze-live's `[dev-dependencies]` (the tests already used
> `blaze_ids::rng::SplitMix64` without it); the model test imports `Holds` and `Lasting` by a
> `use`. `Switch::ready` (this lane's) allocates only while the pool fills, so the warm test's
> `POOL + KEPT + 8` warm-up covers it. The log reserves about 262 GB of address space
> (61 bytes a row × 2^32 rows) plus 2 GiB for the lasting ranges, in the user's program, so
> `Live::open` now fails under an address-space limit (`ulimit -v`, valgrind) where it
> didn't. blaze-mem is `publish = false`, so blaze-live can't be published until it is.
> taken by requests (link and speed-link-edge-sol-live are DONE): child `requests-live`; the outcome follows.
> refused by requests (child requests-live): blaze-mem can't be a dependency of blaze-live, so items 1-8 (applied by speed-link-edge-sol-live and committed in the v2 checkpoint) are undone: `Log` and `Lasting` are `Vec`s again with `Vec::splice`, the extra `LASTING` check, the test allocator and the two tests are gone, and `blaze-mem`/`blaze-alloc-count` are out of `Cargo.toml` (`blaze-ids` stays, for the tests that already used it). The types themselves would be fine in the program (`RegionVec` and `Tag` take memory from mmap/VirtualAlloc, install no global allocator, and budgets default to unlimited); the dependency is not. blaze-live is linked into user programs that build outside this workspace: it is published (`blaze-live = "0.1"`, its README), `demo/life` and `demo/lifebench` build it as a path dependency from their own `[workspace]` with `--locked` lock files that list it with no dependencies, and `flake.nix` `lifeSource`, `tools/nix/life_launch.py` and `harness/tiers.py` copy `crates/blaze-live` alone. blaze-mem is `publish = false`, has no version in the root (path only), takes `#![feature(allocator_api)]`, and pulls libc, windows-sys and blaze-reclaim into the user's lock. It also reserved about 264 GB of address space in the user's program, so `Live::open` failed under `ulimit -v` or valgrind. A zero-allocation log has to be blaze-live's own (a reserved column over the mmap and VirtualAlloc calls it already declares), not blaze-mem.

## mem-backend-live 2: blaze-lazy's waiters onto a Slab (for lane `fastcompile`)

`crates/blaze-lazy/` is lane `fastcompile`'s (lead IN PROGRESS; `fastcompile-inproc-exec`, IN
PROGRESS, names `src/hook.rs`). Exact change, for the lead to apply (or for me once every
agent naming it is DONE):

`crates/blaze-lazy/src/fetch.rs` keeps the threads waiting for a function being compiled in
`FxHashMap<u32, Vec<Waiter>>`: every function asked for before it is ready allocates a `Vec`,
freed when it is answered, so the fetcher's warm loop (a run's first calls, many threads
missing at once) calls the global allocator once per in-flight function. The waiters go into
a `blaze_mem::Slab<Waiting>` (generational handles, one owner: the fetcher, under the runner's
mutex) as a list per function, the map holding only its first and last handle (no
allocation once the map and the slab hold the working set). Answers go out in the order the
waiters asked, as before; the free list is LIFO and deterministic, and no handle reaches an
output, so the fetcher stays a deterministic state machine.

1. `crates/blaze-lazy/Cargo.toml`: `[dependencies]` add
   `blaze-mem = { workspace = true, optional = true }`; `daemon` feature add `"dep:blaze-mem"`;
   `[dev-dependencies]` add `blaze-alloc-count.workspace = true`.

2. `crates/blaze-lazy/src/lib.rs`, after the `mod tests` line:
   ```rust
   #[cfg(all(test, feature = "daemon"))]
   #[global_allocator]
   static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
   ```

3. `crates/blaze-lazy/src/fetch.rs`:
   - imports: add `use blaze_mem::{Handle, Slab, Tag};`
   - after `pub struct Waiter { .. }` add:
     ```rust
     /// A waiter in its function's list.
     struct Waiting {
         to: Waiter,
         next: Option<Handle<Waiting>>,
     }

     /// The most waiters at once, over every function being asked for.
     const MOST_WAITERS: u32 = 1 << 22;
     ```
   - `Fetcher`, before: `waiters: FxHashMap<u32, Vec<Waiter>>,` after:
     ```rust
     /// The first and last waiter of each function being asked for, in the order they asked.
     waiters: FxHashMap<u32, (Handle<Waiting>, Handle<Waiting>)>,
     waiting: Slab<Waiting>,
     ```
   - `Fetcher::new`, after `waiters: FxHashMap::default(),` add:
     ```rust
     waiting: match Slab::new(MOST_WAITERS, Tag::Link) {
         Ok(slab) => slab,
         Err(error) => panic!("the fetcher's waiters: {error}"),
     },
     ```
   - `Fetcher::ask`, before:
     ```rust
             State::Lazy | State::Queued | State::Compiling => {
                 let waiting = self.waiters.entry(lazy).or_default();
                 if !waiting.contains(&to) {
                     waiting.push(to);
                 }
     ```
     after:
     ```rust
             State::Lazy | State::Queued | State::Compiling => {
                 match self.waiters.get_mut(&lazy) {
                     None => {
                         let at = self.waiting.insert(Waiting { to, next: None });
                         self.waiters.insert(lazy, (at, at));
                     }
                     Some((first, last)) => {
                         let (mut at, mut known) = (Some(*first), false);
                         while let Some(handle) = at {
                             let waiting = self.waiting.get(handle).expect("a list of live waiters");
                             known |= waiting.to == to;
                             at = waiting.next;
                         }
                         if !known {
                             let new = self.waiting.insert(Waiting { to, next: None });
                             self.waiting.get_mut(*last).expect("a list of live waiters").next = Some(new);
                             *last = new;
                         }
                     }
                 }
     ```
   - `Fetcher::done`, before:
     ```rust
             let waiting = self.waiters.remove(&id.0).unwrap_or_default();
             match result {
                 Ok(target) => {
                     self.state[at] = State::Ready;
                     self.target[at] = target;
                     for to in waiting {
                         out.push(Action::Answer { to, lazy: id.0, outcome: Outcome::Ready(target) });
                     }
                 }
                 Err(why) => {
                     self.state[at] = State::Failed;
                     let why: Arc<str> = why.into();
                     self.failures.insert(id.0, why.clone());
                     if !waiting.is_empty() {
                         for to in waiting {
                             out.push(Action::Answer { to, lazy: id.0, outcome: Outcome::Failed(why.clone()) });
                         }
                         self.stopped.insert(id.0);
                         out.push(Action::Stop { lazy: id.0, why });
                     }
                 }
             }
     ```
     after:
     ```rust
             let first = self.waiters.remove(&id.0).map(|(first, _)| first);
             match result {
                 Ok(target) => {
                     self.state[at] = State::Ready;
                     self.target[at] = target;
                     self.answer(first, id.0, &Outcome::Ready(target), out);
                 }
                 Err(why) => {
                     self.state[at] = State::Failed;
                     let why: Arc<str> = why.into();
                     self.failures.insert(id.0, why.clone());
                     if first.is_some() {
                         self.answer(first, id.0, &Outcome::Failed(why.clone()), out);
                         self.stopped.insert(id.0);
                         out.push(Action::Stop { lazy: id.0, why });
                     }
                 }
             }
         }

         /// Answers the waiters of the list from `first` with `outcome`, in the order they
         /// asked, and frees their places.
         fn answer(&mut self, mut first: Option<Handle<Waiting>>, lazy: u32, outcome: &Outcome, out: &mut Vec<Action>) {
             while let Some(handle) = first {
                 let waiting = self.waiting.remove(handle).expect("a list of live waiters");
                 out.push(Action::Answer { to: waiting.to, lazy, outcome: outcome.clone() });
                 first = waiting.next;
             }
     ```
   - The module doc's "waiters only for functions being asked for. Nothing is allocated on an
     ask of a published function." becomes "waiters only for functions being asked for, in a
     slab. Nothing is allocated on an ask of a published function, nor, once the slab and the
     map hold the run's most waiters at once, on an ask of one being compiled or its answers."

4. The budget test, `crates/blaze-lazy/src/tests/sim.rs` (the fetcher's own tests), with
   `use crate::fetch::{Action, Fetcher, Waiter};` if not imported:
   ```rust
   /// A warm round of asks of functions not yet compiled, from four clients each, and of their
   /// answers allocates nothing: the slab, the map and the queues hold the working set.
   #[test]
   fn a_warm_round_of_asks_and_answers_allocates_nothing() {
       let mut fetcher = Fetcher::new(64, 4);
       let mut out: Vec<Action> = Vec::with_capacity(64);
       let round = |fetcher: &mut Fetcher, out: &mut Vec<Action>, base: u32| {
           for lazy in base..base + 8 {
               for client in 0..4 {
                   fetcher.ask(Waiter { client, seq: lazy, request: u64::from(client) }, lazy, out);
               }
           }
           while let Some((id, _)) = fetcher.next() {
               fetcher.done(id, Ok(0x1000 + u64::from(id.0)), out);
           }
           let answered = out.iter().filter(|it| matches!(it, Action::Answer { .. })).count();
           out.clear();
           answered
       };
       assert_eq!(round(&mut fetcher, &mut out, 0), 32);
       let answered = blaze_alloc_count::assert_at_most(0, "a warm round of asks and answers", || {
           round(&mut fetcher, &mut out, 8) + round(&mut fetcher, &mut out, 16)
       });
       assert_eq!(answered, 64);
   }
   ```
   And a model test beside the existing single-flight ones: the same random asks, duplicates
   and `done`s against the old `FxHashMap<u32, Vec<Waiter>>` logic kept in the test, asserting
   the same `Action`s in the same order (the `sim.rs` harness already drives random schedules;
   compare `out` after each step).

5. Optional, `crates/blaze-lazy/src/hook.rs` (named by `fastcompile-inproc-exec`; needs
   `blaze-mem` non-optional, since the run process links the crate without `daemon`): `fetch`'s
   `let mut answer = Vec::with_capacity(READY_LEN);` allocates on every first call of a lazy
   function. Before:
   ```rust
       let mut answer = Vec::with_capacity(READY_LEN);
       let kind = attached
           .request(ASK_KIND, &wire::ask(request, lazy), &mut answer)
           .map_err(|it| format!("asking the daemon for its code: {it}"))?;
       let frame = Frame::decode(&answer).map_err(|it| format!("the daemon's answer: {it}"))?;
   ```
   after (the rest of `fetch` moves inside the closure unchanged; a `Failed` answer's
   `why.to_owned()` is on the error path):
   ```rust
       blaze_mem::scratch::with_bytes(|answer| {
           answer.reserve(READY_LEN);
           let kind = attached
               .request(ASK_KIND, &wire::ask(request, lazy), answer)
               .map_err(|it| format!("asking the daemon for its code: {it}"))?;
           let frame = Frame::decode(answer).map_err(|it| format!("the daemon's answer: {it}"))?;
           // ... as before ...
       })
   ```
   Budget: a warm miss allocates nothing on the success path, held by a test in
   `tests/exec.rs` that resolves two functions and measures the second.

> forwarded to fastcompile (`crates/blaze-lazy/`; told by tmux).

## dedup: `blaze-splice` LEB128 onto `blaze_format::leb`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`). The one LEB128 is
`crates/blaze-format/src/leb.rs`; the API is described in `v2/requests/mem-backend-link.md`
under the same heading. `blaze-format` depends only on `blaze-ids`, `serde`, `zstd`, so no cycle.

- `crates/blaze-splice/Cargo.toml`: add `blaze-format.workspace = true` to `[dependencies]`.
- `src/macho.rs`: `fn uleb` becomes

  ```rust
  fn uleb(bytes: &[u8], at: &mut usize) -> anyhow::Result<u64> {
      Ok(blaze_format::leb::read_u64(bytes, at)?)
  }
  ```

  (`LebError` is a `std::error::Error`, so `?` makes it an `anyhow::Error`; its message says
  cut off or too large, as the two contexts did). Delete `fn push_uleb`; its two calls become
  `blaze_format::leb::write_u64(&mut stream, ...)`.
- `src/wasm.rs`: delete `const PADDED` and `fn padded`; in the relocation match,
  `*place = padded(value as i32 as i64, true);` becomes
  `*place = blaze_format::leb::padded_i32(value as i32);` and `*place = padded(value, false);`
  becomes `*place = blaze_format::leb::padded_u32(value as u32);` (the same bytes: the
  unsigned form kept the low 32 bits, the signed one sign-extended an `i32`).
- `src/lib.rs` `CommonEntry::pointer_encoding`'s `leb` closure stays: it skips through the
  `byte` closure that already borrows `rest`, one line, no decoding to share.

> applied by requests (link, mem-backend-live DONE): `blaze-format.workspace = true` in blaze-splice; `macho.rs` `uleb` reads with `leb::read_u64`, `push_uleb` gone and its two calls `leb::write_u64`; `wasm.rs` `PADDED` and `padded` gone, the two arms `leb::padded_i32(value as i32)` / `leb::padded_u32(value as u32)`; `lib.rs`'s closure kept.
