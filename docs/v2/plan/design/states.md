---
status: plan
summary: "The implicit state machines found on blaze's live path (165, by six read-only audits on 2026-11-20), the eight forms that encode a state in a type, and the 36 that can give a wrong answer, lose data or hang, in the order they are built."
date: 2026-11-20
---
# States in types (D154)

Rule 76 says a value with an obligation or a meaning is a type saying so. This is the list of
places where it is not yet, found by six audits of the live path, and the plan to encode each.
The audits' full reports (every machine with file:line, the illegal state that compiles today,
the evidence, the type design and its size) are in the notes, `rounds/wave-1121-sm/`:
S1 engine and drive (24 machines), S2 the command's engine (28), S3 run, test and start (34),
S4 store, core and std (28), S5 the front end's item layer (25), S6 names, macros and units
(26). Not yet audited: the type checker and flow crates, the code builder, crates off the live
path.

## The forms

Each finding is encoded by one of these. The tree already holds a model of each: copy it.

| form | when | model in the tree |
|---|---|---|
| **State enum**: one variant a state, each holding only its own data | several `bool`/`Option`/flag fields of which some mixes are illegal | blaze-engine code_pool.rs `State`; blaze-core threads.rs `Life` |
| **Typestate**: a transition consumes the value and returns the next | call order ("before", "once", "only after") | `Commit::finish(self)`; `PhaseMem::end(self)` |
| **Proof**: a private-constructor value only the check can make; later code takes it | something checked, then relied on through plain data | blaze-drive `Verified<'b>`; bstd `Written`, `Flushed`; blaze-cas `Verified` segments |
| **Linear value** (`#[blaze::linear]`): dropping it is an error | an obligation to publish, release, commit, settle exactly once | bstd `File`, `Scope`, `Task` |
| **Typed id**: one newtype an index space or grain | one `u32`/key type indexing several tables | blaze-surface `CrateIndex` |
| **Recording host**: the only way to ask is the one that records | a read that must be in the read list | blaze-scope `Logged`/`LoggedScope` |
| **Whole-value constructor**: every part in the signature | "the caller fills the rest before use" | `Cfg::from_owned` |
| **No sentinel**: `Option`, an enum, or a checked conversion | `u32::MAX`, a zero key, an empty string meaning "none" | engine_lines.rs `NO_PATH` decoded to `Option` |

What a type cannot carry stays a simulation's (rule 43): whether a sync or a lease survives a
crash, atomics, another process. A type proves the step was asked and completed in order.

## The three causes behind most of them

1. **One fact in several maps**, kept in step by convention (a head's regions in two to five
   maps; a task in `waiting`, `running` and `answered`). One map of a state enum.
2. **"Made" tested by one marker row** while other rows are filed before a later step can still
   fail. Build the parts as an owned value, commit once, return the proof.
3. **A restored value has the type of a made one and weaker contents** (a `Preds` read from a
   record names no sources; an adopted answer is not verified). Two types.

## What can give a wrong answer, lose data or hang (P0)

Engine and drive (S1):
1. A merge that fails half way leaves an engine `finish` accepts: `merge(self, Batch)` returns
   the merged engine or a failure that has no `finish`; the open batch is linear.
2. Each merged grower owes one `grow`: a linear `Merged` consumed only by growing each.
3. A row's FAILED/OWED/INLINE/UNMADE bits: `RowState { Made, OwedMade, OwedUnmade }`, the flags
   byte made from it and checked on read. A lost OWED bit is a certificate that should not stand.
4. A result and how it stands: `Outcome { Made { defines, stands }, Failed(ByKey | ByReads |
   Fault) }`, fields private, the read list a sorted-once newtype (one construction site skips
   the sort today).
5. A row write owes its reverse-index run: `put_row` returns a linear `ReadsOwed`.
6. An adopted answer is read unverified: `AnswerView<Opened>` to `AnswerView<Verified>`.
7. `Answer::from_pages` handed over before its owed columns are filled: every column in the
   signature.
8. A `Cond` or `Var` used in another arena: an invariant-lifetime brand.

The command (S2):
9. A read of the checkout before the journal's place is taken (one was live until 2026-11-20:
   the root manifest's lint rules): `journal_sweep::take` returns `Checkout<Journaled>`, the
   only thing layout, sweep, plan, lints and outlines read through.
10. `certified: true` built from plain bools: a `Stood` proof only the sweep's verdict makes.
11. A file's stamp stored before it settled: `Settled(Stamp)`.
12. A pool slot taken and released by convention: a linear `SlotLease`.

Run, test and start (S3):
13. A body stepped with no proof it may run (an unknown crate prefix, a row placed from the
    start list): `Checked::runs(self) -> Result<Runnable, NotRunnable>`; the walk takes `Runnable`.
14. `Certificate(pub [u8; 32])`: private, made only from the checked root.
15. The parent's message order is checked by the child: `RunWriter<Opened>` to `<Native>` to
    `<Serving>`; only `Serving` publishes.
16. `kill` can answer "deadline": `kill(self) -> Exit`; waiting returns `Ended(Exit)` or the
    child back.
17. A placement's act and its bytes in two queues: one linear `PlaceTicket`.
18. Two copies of the serve loop (run and test): one `ServeLoop<H>`.

Store, core and std (S4):
19. `Lent::get` lends bytes of an uncommitted batch that a commit frees: `Got { Held, Mapped }`,
    only `Mapped` lent for the store's life.
20. A failed journal cut is swallowed and issued again without a count: a bounded try, then a
    typed stop.
21. A written segment reaches the flip without its sync: `Synced<Writing>`.
22. A watcher's read committed under its first stamp: `Read::verify(after) -> Verified<Look>`.
23. A panic in a commit loses the lane's batches: a guard whose drop gives them back.
24. A claim never published hangs its waiters: a linear `Claim` and `Woken`.
25. A wait with no deadline compiles (about 100 sites): a required `Deadline`, and an
    `Unbounded` proof only the watchdog's registration makes.
26. One span in two effects in flight: a linear, non-overlapping `Leased`.

The front end's item layer (S5):
27. A head's regions: a required `Regions { Laid, Unlaid }` on the six kinds that hold them, one
    `RegionTable` of one state enum, filed in one step with the head.
28. A restored trait with no scope fails its members hard: the scope is a required field of the
    trait's and the impl's head record.
29. A lifetime part asked before its head is made caches "no head" for good: the makers take
    `&Made<FnHead>`.
30. **A count that is known only if another head was made first** (three fixes to this one
    mechanism, a fourth failure on 2026-11-20: stop, rule 66): a declared count is a fact of
    the item's text, so its one home is its crate's items record, read through the names source
    in any store (D155); `Count::NotKnown` stays for an opaque's captures alone, and
    `unmade_counts`, `CountsNoting` and the heads' count maps as the walk's source are deleted.
31. Positions and slots compared as bare `u32`s (the `zerovec` refusal): `Position`, `Slot` and
    their counts as distinct types; end state, the older lifetime part is deleted for D148's
    columns.
32. A `Preds` read from a record passed where a lowered one is needed: two types.

Names, macros and units (S6):
33. An item whose `#[cfg]` cannot be read is dropped by names and refused by bodies:
    `CfgVerdict { Kept(KeptAttrs), Gone, Unread }`. With D152 (test.md) this is the recording
    cfg host.
34. A crate with a build script resolved without its script's cfgs: `UnjoinedInput` becomes a
    `CrateInput` only through `joined(unjoined, ScriptRead)`, and making `ScriptRead` records
    the read.
35. A unit graph with missing manifests answers `Done`: `Pass::Partial(missing)`, which has no
    `units()`.
36. An expansion's reads pushed by hand at each builtin: a `Recorder` that is the only host.

The P1 and P2 findings (129: a panic or a "cannot happen" error; clarity) are in the reports.
Each P1 that is S-size is taken with the P0 in its file.

## Lints, before types where a lint exists (rule 73)

`blaze::held_across_wait`, `blaze::thread_local_in_waiting_fn` and `blaze::lock_across_wait`
exist and are set to `Allow` (blaze-lint-defs lints.rs:58, :202, :299): they are turned on,
their findings fixed. New lints with fixes: an error variant used as "cannot happen" (`Unseen`,
35 sites in blaze-cli) outside its documented arm; `.expect("checked ..")` on bytes a proof type
exists for; a `pub` field on a type this document lists.

## Order

1. Done by hand as found: the manifest read before the journal (9, 2026-11-20).
2. The S-size P0s, one writer a file group, with the next wave: 7, 8, 10, 12, 14, 16, 20, 21, 23,
   24, 28, 29, 32, 35, 36, and the three lints.
3. The M-size P0s, each a whole port of its callers (rule 24), simulated where the tree has a
   simulation of it (1 to 5 in blaze-engine's and blaze-answer's; 19, 21, 22, 26 in
   blaze-cas's and blaze-core's): 1, 2, 3, 4, 5, 6, 9, 11, 13, 17, 18, 19, 22, 26, 27, 31, 34.
4. The L-size: 30 (with D152's key change, which touches the same nodes), 15, 25, 33.
5. The second audit: blaze-typeck, blaze-flow, blaze-flow-lower, blaze-lower, blaze-body, the
   builder.

Judge of each: the illegal state named in its report no longer compiles (a `compile_fail` test
or a deleted error variant), the planted-bug control that guarded it is deleted or moved to a
test-only constructor, and the tree's tests and tempo's scoreboard are no worse.
