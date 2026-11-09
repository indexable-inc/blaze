---
status: plan
summary: lane storemodel's model checker of the store and kernel protocol (crate blaze-storemodel): the real blaze-cas Store, Writers and Kernel driven through every schedule of a few actors, a crash after every IO step on a file system that models what a crash keeps, and a restore checked against six properties.
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
- **Warm restart's rule.** The rule is "a build answers once what it leaves is durable, and a
  build asked mid-store waits". It is modelled as each actor's wait before it answers, in one of
  two forms (`store::Wait`):
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
  - A failed sync dooms what it covered ("fsyncgate").
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
| `after_a_build_answers_its_results_survive_any_crash` | two kernel gets racing for one shape slot, one IO failure, effects in any order |
| `the_restore_after_any_crash_equals_a_fresh_build` | the restore, built again, equals a fresh build, and so does a clean restart of it |
| `every_wait_ends` | two flushes and two IO failures: every wait for durability ends, and so does every restore's open |

The restore's comparison with a fresh build is value for value:

- for each key, its kind and bytes;
- for the kernel, each get's value.

It does not compare the files' bytes. Pack layout follows the order puts were made in, and two
histories legitimately lay out the same values differently.

Further tests:

- `larger_scenarios_sampled_keep_every_property`: seeded random search over bigger scenarios.
- `the_model_catches_an_answer_given_before_its_store_is_durable`: the model's own check (rule
  20). A planted early answer must be caught at the crash that loses it.
- `a_crash_keeps_what_the_file_system_model_says`: the crash model's rules.

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
   (`what_a_build_left_for_the_store_is_durable_once_it_answers_and_survives_a_crash`) should
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

```
blaze test -p blaze-storemodel
blaze test -p blaze-storemodel -- --ignored     # the findings: expected to fail today
STOREMODEL_RANDOM=5000 STOREMODEL_SEED=7 blaze test -p blaze-storemodel -- larger_scenarios
STOREMODEL_PREEMPTIONS=none STOREMODEL_EXECUTIONS=2000000 blaze test -p blaze-storemodel
STOREMODEL_REPLAY='<the line a report printed>' blaze test -p blaze-storemodel -- <test>
```
