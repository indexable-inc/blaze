---
status: superseded
superseded_by: end-state.md#d159-one-body-one-store-one-answer-form-code-made-ahead
summary: (crate deleted 2026-11-20) lane storemodel's model checker of the store and kernel protocol (crate blaze-storemodel): the real blaze-cas Store, Writers and Kernel driven through every schedule of a few actors, a crash after every IO step on a file system that models what a crash keeps, and a restore checked against six properties.
date: 2026-11-02
---
# A model checker of the store and kernel protocol

Andrew approved this on 2026-11-02: an exhaustive model check of blaze's store and kernel
protocol, run on the real Rust state machines rather than a separate TLA+ spec. The crate is
`crates/blaze-storemodel`.

## What it drives

- **The store.** `blaze_cas::Store` is already sans-IO (rule 12):
  - it hands back effects (`Store::open`, `flush`);
  - it takes completions (`complete`, `owns`);
  - it reports durability by ticket (`durable`).
  - Each `Writer` and `Reader` is lock-free over shared state.
  - The model needs no change to the store, only a driver.
- **The kernel.** `blaze_kernel::Kernel` is a pure library over the `Get`/`Put` traits. It has
  no IO of its own. Its store calls are the protocol's steps:
  - probe the whole entry;
  - walk the shape slots;
  - put the entry;
  - put the shape, taking a `Mismatch` as "another writer took the slot".
- **The rule after an answer** (2026-11-10; before, "a build answers once what it leaves is
  durable"). A build answers at once, durable or not; what it left settles after the answer
  and is kept for reuse across a crash only once it is durable, or said lost when the store
  stopped (`store::Op::Answer`, `store::Op::Settle`). A snapshot still waits before it answers
  (`store::Op::AwaitAndAnswer`). What is waited on before keeping is one of two forms
  (`store::Wait`):
  - `Own`: the tickets of its own puts;
  - `Everything`: every put made before it asked, which is the hold.
- The daemon's `Jobs` bookkeeping itself lives in `blaze-cargo`'s private `serve` module, so it
  is not driven yet. See "The cut warm restart needs" below.

## How

- **`fs::CrashFs`** is a deterministic driver of file effects. It keeps three states apart:
  - what the process sees;
  - what is durable;
  - what is in between: each file's unsynced changes, and the directory entries not yet synced.
- The rules are `blaze_core::Effect`'s contract. **`CrashFs::crash_states`** lists every disk a
  power cut may leave:
  - A prefix of each file's unsynced changes is kept, the last perhaps torn at its first byte,
    its middle, or one byte short. This is ALICE's model (Pillai et al., OSDI 2014).
  - Each effect in flight may have happened or not.
  - A failed sync dooms what it covered ("fsyncgate"), and so does a failed barrier.
  - A barrier (`Effect::Barrier`, which a commit asks of its packs and checks in place of a
    sync) makes nothing durable and orders: a crash that keeps anything asked after it
    completed keeps all it covers, whole, and a sync asked after it makes that durable.
  - A write or a rename that waits for no sync (`WriteUnsynced`, `RenameUnsynced`) is a pending
    entry: a crash leaves the name as it was or as it is now, the new file's bytes torn by block.
  - Two models of blocks (`Clones`), each searched by every scenario, since a store's open
    tries one clone to decide how it places (`blaze_cas::PlaceBy`):
    - `Made`: a clone is a second file holding its source's bytes whole, its entry pending.
    - `Refused`: every clone fails saying the file system cannot share blocks (ext4, tmpfs).
  - Every variant of `Effect` is named in one match with no arm for the rest (`Files::change`):
    an effect added to `blaze_core` does not compile until it has a rule here.
  - Two models of directory entries (`Persist`):
    - `Ordered`: journaled in order, and any sync commits the journal (ext4, XFS, APFS).
    - `Posix`: an entry is durable only once its directory is synced, and a crash keeps any
      subset of the rest.
  - Past a bound, the list is a typed error and never cut short.
- **Actors** (`actors`) are threads that run only when handed the turn, so an execution is a
  function of the explorer's choices.
  - A writer or a kernel get runs real code on its own `Writer` and guard, as a worker does.
  - A schedule point sits before each put. The kernel's puts go through a `Turn`, so two
    `Kernel::get`s interleave inside the shape walk. That is the race the keep-first path
    exists for.
- **The controller** (`store::Run`) owns the store, as the daemon does. At each step the
  explorer picks one of:
  - an actor's next step;
  - a flush it asks for;
  - the wake of a wait that has ended;
  - an answer;
  - the completion of an effect in flight (in any order with `reorder_io`);
  - the failure of an effect in flight, within a budget.
- **After every step that moved IO**, every crash state is restored and checked (`store::restore`).
  A check is a function of the disk and of what had to hold, so each pair is checked once per
  search (`explore::Memo`).
- **The explorer** (`explore`):
  - It runs a stateless depth-first search over the choices (VeriSoft, CHESS, loom).
  - The search has bounds:
    - executions;
    - steps per execution;
    - crash states per moment;
    - preemptions (CHESS's context bound, 3 by default; `none` lifts it).
  - An exhaustive search that hits its execution bound fails as incomplete; it never passes.
  - A seeded random mode covers larger scenarios.
  - A failure is shrunk by taking steps out of its schedule and replaying by step name.
  - The report gives the property, what broke it, every step, the crash point, and a
    `STOREMODEL_REPLAY` line.

Prior art, searched on GitHub:

- stateright (actor model checking, BFS/DFS with path reports);
- CrashMonkey (crash only after persistence points, ACE workloads);
- ALICE and tove (crash states from a trace's prefixes, under persistence models);
- LazyFS (losing unsynced data);
- knossos and elle (history checkers);
- madsim.

None runs on blaze's sans-IO cores. So the model takes:

- ALICE's crash-state model;
- CrashMonkey's "crash after each persistence point";
- CHESS and loom's bounded stateless search;
- stateright's report of the path.

blaze-check (docs/v2/plan/design/model-checker.md) checks what this cannot: the atomics inside
one put. The daemon's sim tests (`blaze-cargo` `serve/tests.rs`) are where the warm-restart cut
lands.

## Properties, one test each (`src/tests.rs`)

| Test | Scenario |
| --- | --- |
| `a_restore_never_serves_a_value_no_writer_fully_wrote` | two builds and a reader |
| `a_restore_never_silently_starts_empty` | crash states, plus three kinds of damage after a clean stop: a pack cut short, the head's bytes changed, a pack gone |
| `two_values_under_one_identity_are_refused_never_overwritten` | two writers on one key, one IO failure; two gets of an impure step |
| `what_is_kept_after_an_answer_survives_any_crash_and_no_lost_value_is_served` | two kernel gets racing for one shape slot, one IO failure, effects in any order |
| `the_restore_after_any_crash_equals_a_fresh_build` | the restore, built again, equals a fresh build, and so does a clean restart of it |
| `every_wait_ends` | two flushes and two IO failures: every wait for durability ends, and so does every restore's open |

The restore's comparison with a fresh build is value for value:

- for each key, its kind and bytes;
- for the kernel, each get's value.

It does not compare the files' bytes. Pack layout follows the order puts were made in, and two
histories legitimately lay out the same values differently.

Further tests:

- `larger_scenarios_sampled_keep_every_property`: seeded random search over bigger scenarios.
- `the_oracle_catches_a_kept_answer_relied_on_before_its_tickets_were_durable` and `the_oracle_catches_a_settling_failure_dropped_silently`: the model's own checks (rule
  23). What is kept before it is durable must be caught at the crash that loses it, and a
  settling that ends with the store stopped and says nothing must be caught.
- `an_open_cut_short_by_a_crash_opens_again_and_leaves_nothing_of_its_try_behind`: a store that
  stopped cleanly opened again, its open's IO in every order with a failure anywhere and a crash
  after every step. The files of the clone the open tries, which a crash leaves, are removed by
  the next open, which decides how it places as its file system says; the test fails unless
  the search saw a crash leave each of them.
- `a_crash_keeps_what_the_file_system_model_says`, `a_crash_may_undo_or_tear_a_write_that_waited_for_no_sync`,
  `a_clone_is_made_whole_or_refused_as_the_file_system_says` and
  `a_barrier_orders_what_it_covers_before_what_is_asked_after` (with its control: no barrier,
  no order): the crash model's rules.

## Findings (by reading the code; the ignored tests pin them)

1. **A lost head opens an empty store and says nothing.**
   - Where: `Store::read_head`, `(true, 0) => Head::default()`.
   - The store cannot tell a first commit that never finished from a head that is gone. Its next
     commit truncates `pack-000000` to 0 and writes over the durable values.
   - No crash makes this state: the head is written atomically. Deleting the head, or a bad
     restore of the directory, does.
   - Fix: write an empty head when the directory is made, and refuse "no head, but packs" naming
     both.
   - Pinned by `a_store_whose_head_is_lost_says_so` (ignored).
2. **A new pack's directory entry is never synced before the head names it.**
   - `Truncate` makes `pack-000000`, `Sync` syncs only the file, and the only directory sync is
     the head's `Write`.
   - Under the letter of POSIX, the head's rename may persist and the pack's entry may not. The
     restore then finds a head naming a missing pack, and the store does not open.
   - ext4, XFS and APFS order the two, so `Ordered` passes.
   - Fix: a directory-sync effect after a pack is made, or `Sync` of a file it created syncing
     its directory.
   - Pinned by `a_restore_on_a_posix_file_system_opens` (ignored).
3. **A hit carries no ticket.**
   - A build that answers once its own puts are durable can answer with a value another build
     put and has not yet made durable. Two cases:
     - a `Got::Hit`;
     - `keep` returning early because another get kept the same shape.
   - A crash then loses what it answered with.
   - Warm restart's hold (wait for every store in flight) covers it, and the default model
     (`Wait::Everything`) passes.
   - The kernel's API gives a caller no way to wait for exactly what it read. Possible fixes:
     return the ticket of the entry and shape a hit used (`Reader::get` would hand back the
     record's ticket), or document that callers hold.
   - Pinned by `a_build_that_waits_on_its_own_puts_alone_keeps_what_it_answered` (ignored).
4. **A shape chain can have a hole after a crash, and that is harmless.**
   - A commit drains segments in order. Writer B's slot-1 shape can be committed before writer
     A's slot-0 shape.
   - A crash between those commits leaves slot 0 empty and slot 1 full. Lookup stops at the
     first empty slot, so it misses, and the next keep fills slot 0 with a shape the chain may
     already hold.
   - The result is misses and duplicate shapes, never a wrong value. The entry key covers every
     read's hash.
   - Not reachable at the granularity of whole puts. It needs blaze-check inside the drain.
5. **An entry and its shape from one writer cannot be torn apart by a crash.** Both go through
   that writer's one segment, in order, so a commit never holds the shape without the entry.
6. **A pack removal is not synced, and that is harmless.** A retired pack may reappear after a
   crash, and the next open removes it again.

## The cut warm restart needs

The warm-restart rule lives in `blaze-cargo`'s `serve::job` (`Jobs::store_kept`,
`stored_kept`, `wind_up`, and the hold in `ready()`). The rule is only reachable through the
whole daemon. That lane owns those files, so the cut is proposed, not made:

1. Take the store bookkeeping out of `Jobs` into a sans-IO core, `serve/storing.rs`, with this
   interface:
   - `Storing::begin(root, puts) -> Vec<Decision>`;
   - `Storing::flushed(ticket)`;
   - `Storing::durable(tickets)`;
   - `Storing::lower(root) -> Lower { Now | After(job) }`, which is the hold;
   - `take_decisions`.
   - It reads no clock, and `Jobs` calls it.
2. Run it as one more actor kind in `blaze-storemodel`, through a `pub` re-export behind
   `#[cfg(any(test, feature = "model"))]`.
3. Until then, the daemon's sim test
   (`what_a_build_left_for_the_store_is_durable_once_it_settles_and_survives_a_crash`) should
   crash at every IO step and in every crash state, rather than once with a random seed. The
   test would put a `CrashFs` (made `Driver`) in for `Sim`, and `Sim` would gain a
   `crash_with(chooser)`.

## Not modelled

- Interleavings inside one put or one drain (blaze-check's job).
- `settle` and the cold tiers.
- Shared stores and their peers, and the remote tier.
- Externals (values large enough to be files of their own) and index compaction past
  `INDEX_AFTER` records. Scenarios are too small for either; raise the counts to reach them.
- The daemon's `Jobs`.

## Running

While cargo builds blaze (AGENTS.md, blaze-builds-blaze) the same with `cargo test`. A change to
`crates/blaze-cas`, `crates/blaze-kernel`, `crates/blaze-record` or `blaze_core::Effect` runs
it: on 2026-11-12 ten of its tests were found stopping at their first step, at an effect the
store's open had begun to ask and the model had no rule for, and nothing had run them
(`agents/annoyances/storemodel-fix-1.json`).

```
blaze test -p blaze-storemodel
blaze test -p blaze-storemodel -- --ignored     # the findings: expected to fail today
STOREMODEL_RANDOM=5000 STOREMODEL_SEED=7 blaze test -p blaze-storemodel -- larger_scenarios
STOREMODEL_PREEMPTIONS=none STOREMODEL_EXECUTIONS=2000000 blaze test -p blaze-storemodel
STOREMODEL_REPLAY='<the line a report printed>' blaze test -p blaze-storemodel -- <test>
```

## The disk world's proofs under the strict rule (owed since 2026-11-16; needs a Lean host)

`blaze_core::world::disk`, the model the store's crash tests and blaze-core's simulation refine,
now keeps the strict name rule: a create, rename or unlink is durable only once its directory is
synced (`SyncDir`, or a synced write or rename, which syncs its own directory); a file's sync
keeps its bytes, never its name. The Lean models under `lean/BlazeModel/blaze_core/world/disk`
are regenerated from it, and the proofs in `crates/blaze-core/lean/BlazeCore/World/` are left
not building against them: nothing is papered over (no `sorry`, no axiom, no theorem removed or
weakened), so the `lean proofs` job is red by module name until they are redone (decision D55).
What changed, old to new:
- `Undo { was, from }` and `File.undo: Option<Undo>` are gone. The disk keeps
  `Disk.names: Vec<Named>`, oldest first, where
  `Named = Put { file, was: Option<Vec<u8>> } | Gone { file, was } | Moved { from, to, was }`.
  `Named::touches(name)` and `Named::shares(other)` say which names a change touches.
- `File::sync(covers)` no longer clears an undo ("nothing is left to undo" is gone). The new
  `Disk::sync_names()` empties `names`, and `Disk::named(file)` says whether any change of a name
  not synced touches `file`. `Disk::write` (a whole-file write) and `Disk::rename` push a `Named`
  in place of setting `File.undo`; `Disk::remove` pushes `Gone`.
- A cut was `&[Kept]` with `Kept.undone: bool`. It is now `Crash { kept: Vec<Kept>, undone:
  Vec<bool> }`, with one flag per entry of `names`, and `Kept` has no `undone`.
- `Disk::allows(&[Kept])` is now `Disk::allows(&Crash)`. It checks that `undone.len() ==
  names.len()`, that the undone set is closed forward (undoing an entry undoes every later entry
  that shares a name with it), and then `Disk::keeps(&crash.kept)`, which holds the old fits and
  barrier checks.
- `Disk::crash(&[Kept])` is now `Disk::crash(&Crash)`. It cuts each file, then undoes the undone
  entries newest first through `Disk::undo(Named)`, replacing the `Undone { name, undo }` pass.
  What is left of `names` is durable.
- The three theorems take `cut: Crash`. `a_crash_keeps_a_synced_file` and
  `a_crash_cuts_an_appended_file_to_a_prefix` return `true` early when `disk.named(file)`.
- `world/asks.rs` (the asks world) follows the same change. Its model under
  `lean/BlazeModel/blaze_core/world/asks` is regenerated by no test (hir-ty-lean's
  `blaze_cores_world_models_are_its_sources` covers only the clock and disk worlds), so it is
  stale too, and `a_synced_append_survives_a_crash` must be regenerated (add it to that test) and
  re-proved with the rest.

Lemmas expected to break, and why:
- `DiskMap` (`wf_spec`, `small_spec`, `find_spec`, `open_spec`, `slot_spec`): `Disk` gains
  `names`, `File` loses `undo`, and `small` now bounds the `was` bytes in `names`.
- `DiskAllows` (`allows_spec`, `allows_total`, `allows_orders`, `fits_total`): `allows` now
  takes `Crash`, adds the closed-forward loop over `names`, and moves the old body into `keeps`.
- `DiskBytes` (`cut_synced`, `cut_appended`, `sync_all`): each says "nothing to undo left"
  through `File.undo`, which is gone.
- `DiskCrash` (`crash_spec`, `disk_sync`, `synced_proof`, `appended_proof`): the crash's undo
  pass now runs over `names` newest first, and the synced and appended proofs need the new
  premise `disk.named(file) = false` to show that no undo touches `file`.
- `Disk.lean`'s three `.spec.proof`s: their statements' `cut` is now a `Crash`.
- `Asks`/`AsksLib` (`a_synced_append_survives_a_crash`): its model calls the disk's `sync`,
  `allows` and `crash`.

The statement the new proof must reach (`a_crash_keeps_a_synced_file`, as generated): for every
fuel past the bound, disk, file and `Crash`, when the disk is well formed and small, the file is
there and `disk.named(file)` is false, then after the sync covering all the file's changes and
any crash the disk allows, `kept(read, disk.contents(file))` holds and nothing panics or runs out
of fuel. Its control, `a_synced_file_whose_name_was_not_synced_may_be_gone` (a Rust test in
disk.rs), shows that the premise is needed: with the name not synced, a crash that undoes it
loses the file.

### The crash tears by sectors (2026-11-16, D55)

The disk world's crash no longer cuts unsynced data to a prefix. POSIX leaves unsynced data
after a crash unspecified; real file systems drop and reorder unsynced block writes and leave
zero-filled blocks under a grown size. Of each file a crash keeps a prefix of its changes whole
and, of the changes after them up to any one (`upto`), each 512-byte `SECTOR` on its own is the
new bytes or the old (zeros past the old end), the length anywhere between the two
(`Torn::Sectors { upto, len, landed }`, built by `mixed`). A prefix cut is one outcome among
them. The simulation draws the world's own `Torn` and applies it with the world's `mixed`.

- `a_crash_cuts_an_appended_file_to_a_prefix` is false as stated: an append may now lose its
  middle and keep its end. It is deleted. Its replacement, in words: for every fuel past the
  bound, a well-formed small disk, and an only-appended file whose name no unsynced change
  touches, after any crash the disk allows the file is there, begins with all its synced bytes
  (synced bytes and synced length survive), is no longer than what it read before, and each byte
  past the synced ones is the byte read there or a zero (`keeps_what_was_synced`). The Rust
  function is `a_crash_keeps_what_was_synced_of_an_appended_file`; its control is
  `the_oracle_catches_a_file_that_lost_what_was_synced`.
- Lemmas that break: every one over `Torn::Prefix`, `Torn::Sectors(mask)`, `File::reach`,
  `File::fits`, `File::cut` and `cut_to_a_prefix` (DiskAllows `fits_total`, DiskBytes
  `cut_to_a_prefix_spec`, `cut_appended`, DiskCrash `appended_proof`), and `Disk.lean`'s proof
  of the old theorem.
- The generated models under `lean/BlazeModel/blaze_core/world/{disk,asks}` must be regenerated
  on a Lean host (std.md item 7, "Lean models, for main", has the recipe) and the new theorem
  proved; this is not built on the Mac.
