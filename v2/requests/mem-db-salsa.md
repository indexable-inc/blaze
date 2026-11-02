
## dedup: salsa's SwissTable probes onto `blaze_mem::swiss`

From `dedup-intern` (report `/tmp/blaze-lanes/v2-dedup-intern.md`). The group probe
(`tag`/`matching`/`empty`/`live` and the probe loops) is copied in `intern/src/arena.rs`,
`intern/src/table.rs`, `vendor/salsa/src/graph/nodes.rs` and `vendor/salsa/src/interned/table.rs`.
The one copy is now `crates/blaze-mem/src/swiss.rs` (`blaze_mem::swiss`, re-exported as
`intern::swiss`): atomic-free word math plus `find`/`vacant` probes that take a closure loading a
group's word, so salsa keeps its own (shuttle) atomics and orderings. salsa already depends on
`blaze-mem`: no manifest change. `matching` is exact (no borrow false positives), so the
`tag != byte` re-check goes away. The old tests (`matching_finds_every_equal_byte`,
`tags_are_never_dead`, `live_skips_empty_and_dead`) moved into `blaze_mem::swiss`'s tests, with a
proptest differential against the old copies.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): both files below, as written. Not built.

### `vendor/salsa/src/graph/nodes.rs`

1. Module doc: `The map from a key to its node is the SwissTable of `crates/intern/src/arena.rs``
   becomes `The map from a key to its node is a SwissTable probed with `blaze_mem::swiss``.
2. Imports: add `use blaze_mem::swiss::{self, Ctrl};`.
3. Delete `const LOW_BITS`, `const HIGH_BITS`, `fn tag`, `fn matching`, `fn empty` (keep
   `const GROUP: usize = 7;`, its doc becomes "Slots in a group: tag bytes 0 to 6 of the control
   word; byte 7 stays 0.").
4. `Table::find` body becomes:
```rust
        let ctrl = |at: usize| Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Acquire));
        swiss::find(hash, swiss::tag(hash), self.groups.len(), ctrl, |at| {
            // Relaxed: the tag's `Acquire` above ordered the slot's and its key's stores
            // before us; only tags published in the loaded word are matched.
            let node = self.groups[at.group].nodes[at.slot].load(Ordering::Relaxed);
            (keys.parts(node) == (id, ingredient)).then_some(node)
        })
```
5. `Table::put` body becomes:
```rust
        let at = swiss::vacant(hash, self.groups.len(), |at: usize| {
            Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Relaxed))
        });
        let group = &self.groups[at.group];
        group.nodes[at.slot].store(node, Ordering::Relaxed);
        group.ctrl.store(at.tagged(swiss::tag(hash)).0, publish);
```
6. In `Nodes::table_for_one_more`, the copy loop becomes:
```rust
            for group in &old.groups {
                for slot in Ctrl::<GROUP>(group.ctrl.load(Ordering::Relaxed)).full() {
                    let node = group.nodes[slot].load(Ordering::Relaxed);
                    let (id, ingredient) = self.parts(node);
                    grown.put(hash_parts(id, ingredient), node, Ordering::Relaxed);
                }
            }
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): as written (the old `tag`/`matching`/`empty` and their constants deleted). Not built.

### `vendor/salsa/src/interned/table.rs`

1. Module doc: `Ported from `crates/intern/src/arena.rs` (salsa can't depend on `intern`), the
   SwissTable layout of` becomes `Probed with `blaze_mem::swiss`, the SwissTable layout of`.
2. Imports: add `use blaze_mem::swiss::{self, Ctrl};`.
3. Delete `const LOW_BITS`, `const HIGH_BITS`, `const DEAD`, `fn tag`, `fn matching`, `fn empty`,
   `fn live`. Uses of `DEAD` become `swiss::DEAD`; keys are tagged with `swiss::live_tag` (the
   old `tag`: `0x80 | (hash >> 57).max(1)`, never DEAD).
4. `Groups::find` body becomes:
```rust
        let ctrl = |at: usize| Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Acquire));
        swiss::find(hash, swiss::live_tag(hash), self.groups.len(), ctrl, |at| {
            // Relaxed: the tag's `Acquire` above ordered the slot's writes before us; only tags
            // published in the loaded word are matched.
            let value = NonNull::new(self.groups[at.group].slots[at.slot].load(Ordering::Relaxed))
                .expect("a published slot holds a value");
            matches(Candidate { value, hash: &self.hashes[at.index()] }).then_some(value)
        })
```
5. `Groups::put` body becomes:
```rust
        let at = swiss::vacant(hash, self.groups.len(), |at: usize| {
            Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Relaxed))
        });
        let group = &self.groups[at.group];
        group.slots[at.slot].store(value.as_ptr(), Ordering::Relaxed);
        self.hashes[at.index()].store(hash, Ordering::Relaxed);
        group.ctrl.store(at.tagged(swiss::live_tag(hash)).0, publish);
```
6. `Groups::kill` body becomes:
```rust
        let ctrl = |at: usize| Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Relaxed));
        let found = swiss::find(hash, swiss::live_tag(hash), self.groups.len(), ctrl, |at| {
            let same = self.groups[at.group].slots[at.slot].load(Ordering::Relaxed) == value.as_ptr()
                && self.hashes[at.index()].load(Ordering::Relaxed) == hash;
            same.then_some(at)
        });
        let Some(at) = found else {
            return false;
        };
        self.groups[at.group].ctrl.store(at.tagged(swiss::DEAD).0, Ordering::Release);
        true
```
7. In `Locked::table_for_one_more`, the copy loop's
   `let mut keys = live(group.ctrl.load(Ordering::Relaxed)); while keys != 0 { let byte = ...; keys &= keys - 1; ... }`
   becomes `for byte in Ctrl::<GROUP>(group.ctrl.load(Ordering::Relaxed)).live() { ... }`
   with the body unchanged.
8. Tests: delete `tags_are_never_dead`, `matching_finds_every_equal_byte` and
   `live_skips_empty_and_dead` (now in `blaze_mem::swiss`'s tests); the rest stay.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): as written, the three moved tests deleted. Not built.

# From mem-db-salsa (to lane db, owner of `vendor/salsa*`; window `db-next2`)

salsa onto `crates/blaze-mem` (mem-db's contract, `docs/v2/plan/lanes/mem/db.md`). Notes and the
inventory behind each change: `docs/v2/plan/lanes/mem/db-salsa.md`. Speed-salsa's audit
(`docs/v2/plan/lanes/db/speed-salsa.md`, and S1 to S5 in `v2/requests/db-salsa.md`) is applied
and not refiled here: the lock-free hits (S1, S2), the shard count (S3), the repeated-read skip
(S4) and the edge array, node table, 24-byte header and claim-free verify (S5). These changes
are what is left. M1 to M4 are part 1, M5 and M6 part 2, M7 part 3, M8 the tests. They are
independent of each other except M8, which tests M1 to M4 and M7. Lines are as of 10-29;
another lane's uncommitted edits in `vendor/salsa-macros` (the concrete `&Db` work) are taken
as the base.

## M1. Every run of the user's code is a frame task (`function.rs`, `function/execute.rs`, `function/fetch.rs`, `lib.rs`)

The user's function runs in four places, all of them inside the claim: `C::execute`
(`execute.rs:340`, every ordinary run and every fixpoint iteration, which loops through
`execute_query`), `C::cycle_initial` (`fetch.rs:283` for the first provisional value,
`execute.rs:207` and `execute.rs:262` for `FallbackImmediate`) and `C::recover_from_cycle`
(`execute.rs:272`). Each becomes a `blaze_mem::frame::task` of the function's tag. The input is
made outside the task: it is owned (`id_to_input` clones a multi-argument function's arguments)
and the frame is for what the user's code makes. `frame::task`'s closure is
`impl FnOnce(&Frame) -> R` with `R` free of the frame's lifetime, so the compiler already
refuses a result that borrows the frame: the memoized value is owned, per contract item 4.
`values_equal` and `fingerprint` (the user's `Eq` and `Hash`, called by backdating) run outside
any task, and must not call `frame::current`: they compare and hash, they don't build.

1. `src/function.rs`, in `pub unsafe trait Configuration`, after `const PERSIST: bool;` (line 64):
```rust
    /// The subsystem a run of this function counts against: each run of its code (an
    /// execution, a fixpoint iteration, its cycle functions) is a `blaze_mem::frame::task` of
    /// this tag. Set with `#[salsa::tracked(mem_tag = Infer)]`.
    const MEM_TAG: blaze_mem::Tag = blaze_mem::Tag::Query;
```
2. `src/lib.rs`, in `pub mod plumbing`, after `pub use crate::zalsa_local::ZalsaLocal;`:
```rust
    pub use blaze_mem::Tag as MemTag;
```
3. `src/function/execute.rs`, `execute_query` (line 340). Before:
```rust
        let new_value = C::execute(
            db,
            C::id_to_input(zalsa, active_query.database_key_index.key_index()),
        );
```
After:
```rust
        let input = C::id_to_input(zalsa, active_query.database_key_index.key_index());
        let new_value = blaze_mem::frame::task(C::MEM_TAG, |_| C::execute(db, input));
```
4. `src/function/execute.rs`, the participant branch (line 207) and the head branch (line 262),
   each. Before:
```rust
                            new_value = C::cycle_initial(db, id, C::id_to_input(zalsa, id));
```
After (indentation as at each site):
```rust
                            let input = C::id_to_input(zalsa, id);
                            new_value = blaze_mem::frame::task(C::MEM_TAG, |_| {
                                C::cycle_initial(db, id, input)
                            });
```
5. `src/function/execute.rs`, line 272. Before:
```rust
                new_value = C::recover_from_cycle(
                    db,
                    &cycle,
                    last_provisional_value,
                    new_value,
                    C::id_to_input(zalsa, id),
                );
```
After:
```rust
                let input = C::id_to_input(zalsa, id);
                new_value = blaze_mem::frame::task(C::MEM_TAG, |_| {
                    C::recover_from_cycle(db, &cycle, last_provisional_value, new_value, input)
                });
```
6. `src/function/fetch.rs`, `fetch_cold_cycle` (line 283). Before:
```rust
                let initial_value = C::cycle_initial(db, id, C::id_to_input(zalsa, id));
```
After:
```rust
                let input = C::id_to_input(zalsa, id);
                let initial_value =
                    blaze_mem::frame::task(C::MEM_TAG, |_| C::cycle_initial(db, id, input));
```

No manifest change: salsa already depends on `blaze-mem` (`vendor/salsa/Cargo.toml`), and
`frame::task` needs no `allocator_api` in salsa. Held by M8's `tests/mem_frames.rs`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): all five sites, `Configuration::MEM_TAG`, `plumbing::MemTag`. Not built.

## M2. The `mem_tag` option (`salsa-macros`, `salsa-macro-rules`)

`#[salsa::tracked(mem_tag = Infer)]` on a function or a tracked method; the value is a variant
of `blaze_mem::Tag`. An unknown name fails where the macro expands it
(`$zalsa::MemTag::$mem_tag`), with the error at the user's identifier, so the list of tags lives
only in blaze-mem. Tracked impls round-trip their options through `Options::to_tokens` into the
inner `#[salsa::tracked(..)]` (`tracked_impl.rs:100` to `135`), so methods get it from step 1's
`to_tokens` change.

1. `salsa-macros/src/options.rs`:
   - in `pub(crate) struct Options<A>`, after `pub self_ty: Option<syn::Type>,`:
```rust

    /// The `mem_tag = <Tag>` option sets the `blaze_mem::Tag` a tracked function's runs count
    /// against.
    ///
    /// If this is `Some`, the value is the tag's variant name.
    pub mem_tag: Option<syn::Ident>,
```
   - in `Default for Options<A>`, after `self_ty: Default::default(),`: `mem_tag: Default::default(),`
   - in `trait AllowedOptions`, after `const SELF_TY: bool;`: `const MEM_TAG: bool;`
   - in `parse`, before the final `} else {` (the "unrecognized option" branch, line 532):
```rust
            } else if ident == "mem_tag" {
                if A::MEM_TAG {
                    let _eq = Equals::parse(input)?;
                    let tag = syn::Ident::parse(input)?;
                    if let Some(old) = options.mem_tag.replace(tag) {
                        return Err(syn::Error::new(
                            old.span(),
                            "option `mem_tag` provided twice",
                        ));
                    }
                } else {
                    return Err(syn::Error::new(
                        ident.span(),
                        "`mem_tag` option not allowed here",
                    ));
                }
```
   - in `to_tokens`, add `mem_tag,` to the destructuring (after `self_ty,`), and after the
     `self_ty` block:
```rust
        if let Some(mem_tag) = mem_tag {
            tokens.extend(quote::quote! { mem_tag = #mem_tag, });
        }
```
2. `const MEM_TAG: bool = false;` after `const SELF_TY: bool = false;` in the `AllowedOptions`
   impls of `input.rs`, `accumulator.rs`, `interned.rs` and `tracked_struct.rs`;
   `const MEM_TAG: bool = true;` after `const SELF_TY: bool = true;` in `tracked_fn.rs`.
3. `salsa-macros/src/tracked_fn.rs`, `try_fn`: after `let heap_size_fn = self.args.heap_size_fn.iter();`
   add `let mem_tag = self.args.mem_tag.iter();`, and in the `setup_tracked_fn!` invocation after
   `heap_size_fn: #(#heap_size_fn)*,` add `mem_tag: #(#mem_tag)*,`.
4. `salsa-macro-rules/src/setup_tracked_fn.rs`: in the matcher, after
   `heap_size_fn: $($heap_size_fn:path)?,` add:
```rust

        // The `blaze_mem::Tag` variant the function's runs count against, when given.
        mem_tag: $($mem_tag:ident)?,
```
   and in `unsafe impl $zalsa::function::Configuration for $Configuration`, after
   `const PERSIST: bool = $persist;`:
```rust
                $(
                    const MEM_TAG: $zalsa::MemTag = $zalsa::MemTag::$mem_tag;
                )?
```

Which tag each front-end query takes is contract item 3 (`docs/v2/plan/lanes/mem/db.md`); the
front-end lanes add `mem_tag = ..` to their tracked attributes (mem-db-def, mem-db-infer,
mem-db-solver, mem-db-checks file those).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): in the amended form below (`mem_tag` takes a path; the glob `use $zalsa::MemTag::*`). Not built.

> mem-db, amending M2: `mem_tag` takes a path, so both `mem_tag = Infer` and
> `mem_tag = crate::db::INFER_TAG` work. mem-db-infer's files use the second form (80 places).
> Three changes to the steps above:
> - step 1's field becomes `pub mem_tag: Option<syn::Path>,` ("the tag: a `blaze_mem::Tag` variant's
>   name, or a path to a `Tag` constant"), and its parse
>   `let tag = syn::Path::parse_mod_style(input)?;`, with the span from `old.segments[0].ident.span()`.
> - step 4's matcher becomes `mem_tag: $($mem_tag:path)?,`.
> - step 4's item becomes
>   `$( const MEM_TAG: $zalsa::MemTag = { #[allow(unused_imports)] use $zalsa::MemTag::*; $mem_tag }; )?`.
>   The glob brings the variants into scope for a bare name; a longer path resolves as written.
>   An unknown name still fails at the user's tokens.
> - Test (M8, `tests/`): one tracked fn with `mem_tag = Parse` and one with
>   `mem_tag = crate::PARSE_TAG` (a `const PARSE_TAG: salsa::plumbing::MemTag = ..Mir`). Each one's
>   body answers `blaze_mem::frame::current(|f| f.tag())`, which is `Parse` and `Mir`.

## M3. A run's fingerprint buffer is reused, the memo gets an exact copy (`active_query.rs`, `function/execute.rs`)

The run's per-thread state is already reused: `QueryStack` (`active_query.rs:473`) keeps every
`ActiveQuery` slot it ever pushed and resets it (`reset_for`), and each slot's `input_outputs`
(`FxIndexSet`, drained into the edge array at `finish`), `disambiguator_map` and
`tracked_struct_ids` (cleared, `HashTable::drain` keeps the table) and `captures` (frames kept
in `spare`) keep their capacity. `FLATTEN_MAPS` (`execute.rs:941`) pools a cycle head's
flattening sets. The one buffer made fresh per run is `edge_fingerprints`
(`active_query.rs:49`): `mem::take` hands it to the memo at `prepare_completion`
(`active_query.rs:366`) and `shrink_to_fit` reallocates it, so a run that reads `lru` functions
grows a new `ThinVec` by doubling (1, 2, 4.. entries: `log2 n` allocations) and shrinks it (one
more). The fingerprints outlive the run (the memo's), so the buffer is reused, not framed, and
the memo gets one allocation at its exact length; a run with none allocates nothing, as before.
The cycle head's `flattened_edge_fingerprints` (`execute.rs:991`) `collect`s the same way, once
per iteration: it fills a pooled buffer too.

What else a run makes and keeps is owned and stays so: the cycle heads (`CycleHeads`, a
`ThinVec` only while a cycle iterates, moved into the memo's extra), the accumulated values, the
active tracked struct IDs (`ThinVec::with_capacity` at their count, `tracked_struct.rs:343`) and
the stale ones (`Vec`, only when an execution stops creating structs it made before, consumed by
`diff_outputs` and the next iteration's seed).

1. `src/active_query.rs`, the field (line 47). Before:
```rust
    /// The value fingerprints of the inputs in `input_outputs` that were read with one, keyed by
    /// the input's position there.
    edge_fingerprints: ThinVec<EdgeFingerprint>,
```
After:
```rust
    /// The value fingerprints of the inputs in `input_outputs` that were read with one, keyed by
    /// the input's position there. Kept by the stack slot across runs; the memo gets a copy at
    /// its exact length.
    edge_fingerprints: Vec<EdgeFingerprint>,
```
2. `ActiveQuery::new` (line 318). Before: `            edge_fingerprints: ThinVec::new(),`
   After:
```rust
            // alloc: `Vec::new` allocates nothing; the slot's runs reuse the buffer
            edge_fingerprints: Vec::new(),
```
3. `prepare_completion` (line 366). Before:
```rust
        let mut edge_fingerprints = mem::take(edge_fingerprints);
        edge_fingerprints.shrink_to_fit();
```
After:
```rust
        let edge_fingerprints = exact_thin_vec(edge_fingerprints);
```
4. New, after `impl ActiveQuery { fn new .. fn reset_for .. }` (after line 462):
```rust
/// The memo's copy of a run's buffer, at its exact length and with nothing allocated for none,
/// leaving the buffer empty and its capacity for the next run.
pub(crate) fn exact_thin_vec<T>(buffer: &mut Vec<T>) -> ThinVec<T> {
    if buffer.is_empty() {
        return ThinVec::new();
    }
    // alloc: the memo's own fingerprints, made once at their exact length
    let mut exact = ThinVec::with_capacity(buffer.len());
    exact.extend(buffer.drain(..));
    exact
}
```
   `clear` (line 399, `edge_fingerprints.clear()`), `reset_for`'s `is_empty` assertion, the
   `push`es in `add_read` and `replay`, `close_capture`'s `&self.edge_fingerprints` (taken as
   `&[EdgeFingerprint]`, `capture.rs:406`) and the unit test's `.as_slice()` are unchanged and
   type-check on a `Vec`.
5. `src/function/execute.rs`. The pool (line 941). Before:
```rust
    static FLATTEN_MAPS: std::cell::Cell<Option<(FxIndexSet<QueryEdge>, FxHashSet<DatabaseKeyIndex>)>> = const { std::cell::Cell::new(None) };
```
After:
```rust
    #[allow(clippy::type_complexity)]
    static FLATTEN_MAPS: std::cell::Cell<Option<(FxIndexSet<QueryEdge>, FxHashSet<DatabaseKeyIndex>, Vec<EdgeFingerprint>)>> = const { std::cell::Cell::new(None) };
```
   In `complete_cycle_query`: `let (mut flattened, mut seen) = FLATTEN_MAPS.take().unwrap_or_default();`
   becomes `let (mut flattened, mut seen, mut fingerprints) = FLATTEN_MAPS.take().unwrap_or_default();`,
   `let edge_fingerprints = flattened_edge_fingerprints(zalsa, &flattened);` becomes
   `let edge_fingerprints = flattened_edge_fingerprints(zalsa, &flattened, &mut fingerprints);`,
   and `FLATTEN_MAPS.set(Some((flattened, seen)));` becomes
   `FLATTEN_MAPS.set(Some((flattened, seen, fingerprints)));`.
   `flattened_edge_fingerprints` (line 991) becomes:
```rust
fn flattened_edge_fingerprints(
    zalsa: &Zalsa,
    flattened: &FxIndexSet<QueryEdge>,
    fingerprints: &mut Vec<EdgeFingerprint>,
) -> ThinVec<EdgeFingerprint> {
    fingerprints.extend(flattened.iter().enumerate().filter_map(|(position, edge)| {
        if !matches!(edge.kind(), QueryEdgeKind::Input) {
            return None;
        }
        let input = edge.key();
        let memo = zalsa
            .lookup_ingredient(input.ingredient_index())
            .as_function()?
            .memo(zalsa, input.key_index())?;
        let header = memo.header();
        let fingerprint = header.revisions.value_fingerprint();
        (fingerprint.is_value() && !header.may_be_provisional()).then(|| EdgeFingerprint {
            edge: u32::try_from(position)
                .ok()
                .filter(|&edge| edge != EdgeFingerprint::VALUE)
                .expect("exceeded `u32::MAX - 1` query edges; this should never happen"),
            fingerprint,
        })
    }));
    crate::active_query::exact_thin_vec(fingerprints)
}
```
   Its doc comment stays. Held by `tests/lru_fingerprint.rs` and `tests/cycle*.rs` (the values),
   and by M8's warm re-check budget.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): as written. Not built.

## M4. Frames past the spare depth (to lane mem, through mem-db; not a salsa change)

`blaze_mem::frame::task` keeps `DEPTH = 8` spare frames per thread (`crates/blaze-mem/src/frame.rs:35`,
`give_spare` at line 421) and reserves a new `FRAME_RESERVE = 4 GB` frame for a task nested
deeper. With one task per query run (contract item 1), salsa's runs nest as deep as its query
stack, which is 50 to 300 in rust-analyzer-shaped front ends (`infer` to `trait_solve` to
`impl_datum` to `generic_predicates` to `lower` to `def_map` and back). Every run past depth 8
then maps 4 GB, faults in fresh zeroed pages and unmaps them on return (`give_spare` drops
frames beyond 8): two system calls and a page fault per page touched per run, in the warm loop,
and 4 GB of address space per level per thread (300 levels on 100 threads is 120 TB, past what
a 47-bit address space holds).

Runs on a thread nest strictly last-in first-out (a query that blocks waits on its own stack;
rayon work stolen inside a run also nests on that stack), so a nested task need not have a frame
of its own. The change asked of lane mem: a task started while the thread is already in a task
shares the thread's frame: it takes a `Marker` (`Frame::mark`) at entry, sets the frame's tag
for its accounting, and on exit `reset_to`s the marker and restores the outer task's tag and
peak, so an inner reset never frees what the outer run holds (contract item 1's guarantee). One
frame per thread, one reservation, warm pages reused at every depth. `frame::current` then
names that frame and its current tag. M8's `a_nested_run_has_a_frame_of_its_own` holds both
designs to the contract (the outer run's bytes and tag are unchanged by an inner run).

> forwarded to db (from requests, pass 16; told by tmux).
> speed-salsa (window 17, 2026-10-29 22:20): nothing for salsa, as mem-db says below.
> mem-db: the problem is real; the shared frame is unsound (an outer frame vector pushed during an
> inner task lands above its marker), so it goes to lane mem as a requirement instead:
> `v2/requests/mem-db.md` request 10. Nothing for salsa to change; M1 stands.
> not applied by db-audit-mem (22:47): overtaken, nothing for salsa or lane db: the change is lane mem's `crates/blaze-mem/src/frame.rs`, filed as `v2/requests/mem-db.md` request 10 and written there by mem-frame.

## M5. Tables onto reserved regions (`table.rs`, `interned.rs`)

What of the tables still comes from the global allocator, by file and line:

| what | where | when | change |
|---|---|---|---|
| the page list, a `boxcar::Vec<Page>` | `table.rs:29`, `184` | a bucket per doubling of the page count | `blaze_mem::Column<Page>` tagged `Query` (many appenders, lock-free readers, never moves); below |
| each page's 128 slots, `Box<PageData<T>>` | `table.rs:599` (shuttle: `605`) | one allocation per 128 inputs, tracked structs or interned values | a bump arena per tag over one `Region`: `Query` for inputs and tracked structs, `Intern` for interned values; below |
| the free page lists, `Mutex<FxHashMap<IngredientIndex, Vec<PageIndex>>>` | `table.rs:31` | when a task gives back a page it didn't fill (`take_unfilled_pages`) | stays: once per ingredient per task that ends, never on a read |
| a task's last page per ingredient, `FxHashMap` | `zalsa_local.rs:45` | once per ingredient a task first allocates in | stays: grows to the ingredients one task touches, then is reused |
| interned key tables (`Groups`) | `interned/table.rs:124`, `450` | one per doubling of a shard, freed at the next revision | stays: a shard's first tables are 128 bytes; a region each would take a whole OS page (16 KB on Apple silicon) for each of 4x-cores shards in every interned ingredient |
| a shard's reusable values (`ShardValues::values`) | `interned.rs:133` | doubling, on a miss (a new value) | stays: the miss path, amortized; the values themselves are slots, on the `Intern` arena |
| node key tables (`graph/nodes.rs` `Table`) | `graph/nodes.rs:158` | one per doubling of the node count, freed at the next revision | stays, as the interned ones |
| node key pages and edge pages (`zeroed`) | `graph.rs:183` to `215` | one per doubling of the node or edge count (20 at most each) | M6 |
| a tracked struct's free IDs, `SegQueue<Id>` | `tracked_struct.rs:205` | a 31-entry block per 31 structs deleted | stays: deletion, at most one allocation per 31 deletions, never on a hit or a steady re-check |
| memo ingredient indices (`Box<[IngredientIndex]>`, `Box<[MemoIngredientIndex]>`), `Zalsa`'s ingredient vectors, jar map, `MemoTableTypes` `Arc`s, sync shards, interned revision queues | `memo_ingredient_indices.rs:14`, `130`; `zalsa.rs:151` to `166`; `input.rs:134`, `tracked_struct.rs:500`, `interned.rs:553`; `function/sync.rs:80` | setup, once per ingredient when its jar registers; immutable after | stays (`// alloc: setup`): a few hundred bytes per ingredient, read-only, in lines the hits already load; a region each would be a page each |
| memo slots and rows | `table/memo.rs` | | already regions (`Tag::Query`) |

The interned values' own fields (a `Box<str>`, a `Vec` the user interns) and the inputs' and
tracked structs' fields are owned values, which lane mem's ruling (mem-db's contract item 4)
keeps on the general-purpose allocator until memos and values persist into the image.

IDs and determinism don't change: a page's index is still the order of its push (boxcar's
`push` and `Column::push` both claim it with one `fetch_add`), and an `Id` is still
`page << 7 | slot`. Page data moves from one heap allocation per page to an offset in its tag's
arena, which nothing observes. The memo-hit path loses a load: boxcar's `pages[i]` loads the
bucket pointer and then the entry and its `active` flag, a `Column::get` loads the ready word
(64 pages share one, read-only after publication, so always cached) and computes the page's
address from the region's base, so the page load no longer waits on a bucket pointer.

Needs `Column::get_mut` from lane mem (M9).

1. `src/table.rs`, imports. Before: `use crate::sync::{Arc, Mutex};`. After:
```rust
use blaze_mem::{Column, Region, Tag};

use crate::sync::{Arc, Mutex};
```
2. The trait `Slot`, after `fn memos_mut(&mut self) -> &mut MemoTable;`:
```rust

    /// The subsystem the table's pages of these slots count against: `Query`, or `Intern` for
    /// interned values.
    const MEM_TAG: Tag = Tag::Query;
```
3. `SlotVTable::of`'s `drop_impl` and its doc (lines 79 to 100). Before:
```rust
    /// A drop impl to call when the own page drops
    /// SAFETY: The caller is required to supply a valid pointer to a `Box<PageDataEntry<T>>`, and
    /// the correct initialized length and memo types.
    drop_impl: unsafe fn(data: *mut (), initialized: usize, memo_types: &MemoTableTypes),
```
```rust
                drop_impl: |data, initialized, memo_types| {
                    // SAFETY: The caller is required to provide a valid data pointer.
                    let data = unsafe { Box::from_raw(data.cast::<PageData<T>>()) };
                    for i in 0..initialized {
                        let item = data[i].get().cast::<T>();
                        // SAFETY: The caller is required to provide a valid initialized length.
                        unsafe {
                            memo_types.attach_memos_mut((*item).memos_mut()).drop();
                            ptr::drop_in_place(item);
                        }
                    }
                },
```
After:
```rust
    /// Drops the page's slots in place; their memory is the arena's, freed with the table.
    /// SAFETY: The caller is required to supply the page's data pointer (a `PageData<T>` in a
    /// [`PageArena`]), and the correct initialized length and memo types, once.
    drop_impl: unsafe fn(data: *mut (), initialized: usize, memo_types: &MemoTableTypes),
```
```rust
                drop_impl: |data, initialized, memo_types| {
                    let data = data.cast::<PageDataEntry<T>>();
                    for i in 0..initialized {
                        // SAFETY: The caller provides the page's data and its initialized length,
                        // so entry `i` is in the page and holds an initialized `T` that nothing
                        // else drops.
                        unsafe {
                            let item = (*data.add(i)).get().cast::<T>();
                            memo_types.attach_memos_mut((*item).memos_mut()).drop();
                            ptr::drop_in_place(item);
                        }
                    }
                },
```
4. `pub struct Table` (line 28) and `impl Default for Table` (line 180). Before:
```rust
pub struct Table {
    pages: boxcar::Vec<Page>,
    /// Map from ingredient to non-full pages that are up for grabs
    non_full_pages: Mutex<FxHashMap<IngredientIndex, Vec<PageIndex>>>,
}
```
```rust
impl Default for Table {
    fn default() -> Self {
        Self {
            // alloc: setup, once per database
            pages: boxcar::Vec::new(),
            non_full_pages: Default::default(),
        }
    }
}
```
After:
```rust
pub struct Table {
    /// Declared before the arenas, so dropped first: a page's drop drops its slots in place,
    /// in an arena.
    pages: Column<Page>,
    /// Map from ingredient to non-full pages that are up for grabs
    non_full_pages: Mutex<FxHashMap<IngredientIndex, Vec<PageIndex>>>,
    /// The slots of pages of a type tagged `Query`: inputs and tracked structs.
    query_slots: PageArena,
    /// The slots of pages of a type tagged `Intern`: interned values.
    intern_slots: PageArena,
}

/// The most address space one arena of slot pages reserves: 64 GB, committed only as pages are
/// made.
const SLOT_BYTES: usize = 1 << 36;

/// The slot pages of one tag, in one reserved region: bumped by one compare-and-swap per page
/// (per 128 slots), never moved, and freed with the table.
struct PageArena {
    region: Region,
    used: AtomicUsize,
}

impl PageArena {
    fn new(tag: Tag) -> PageArena {
        PageArena {
            region: Region::reserve(SLOT_BYTES, tag).unwrap_or_else(|err| {
                panic!("salsa: reserving the table's `{tag}` slot pages: {err}")
            }),
            used: AtomicUsize::new(0),
        }
    }

    /// Committed, uninitialized room for one page of `layout`, the caller's until the arena
    /// drops.
    fn alloc(&self, layout: Layout) -> NonNull<()> {
        assert!(
            layout.align() <= blaze_mem::page_size(),
            "salsa: a slot type is aligned past a page"
        );
        let mut used = self.used.load(Ordering::Relaxed);
        let (start, end) = loop {
            let start = used.next_multiple_of(layout.align());
            let end = start + layout.size();
            match self
                .used
                .compare_exchange_weak(used, end, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break (start, end),
                Err(now) => used = now,
            }
        };
        self.region.ensure(end).unwrap_or_else(|err| {
            panic!("salsa: committing a page of `{}` slots: {err}", self.region.tag())
        });
        self.region.at(start).cast::<()>()
    }
}

/// The arena for pages of `T`, by its tag.
fn arena_of<'a, T: Slot>(query: &'a PageArena, intern: &'a PageArena) -> &'a PageArena {
    match T::MEM_TAG {
        Tag::Query => query,
        Tag::Intern => intern,
        other => panic!("salsa: table slots are tagged `Query` or `Intern`, not `{other}`"),
    }
}

#[cold]
#[inline(never)]
fn page_missing(page: usize) -> ! {
    panic!("salsa: page `{page}` is not allocated")
}
```
```rust
impl Default for Table {
    fn default() -> Self {
        Self {
            pages: Column::new(MAX_PAGES, Tag::Query)
                .unwrap_or_else(|err| panic!("salsa: reserving the table's page list: {err}")),
            non_full_pages: Default::default(),
            query_slots: PageArena::new(Tag::Query),
            intern_slots: PageArena::new(Tag::Intern),
        }
    }
}
```
   The page list reserves `MAX_PAGES` (2^25 less 2) pages of `size_of::<Page>()` (about 80
   bytes): 2.7 GB of address space, committed a page of the list at a time.
5. In `impl Table`, add at its top:
```rust
    #[inline]
    fn page_at(&self, page: usize) -> &Page {
        match self.pages.get(page as u32) {
            Some(page) => page,
            None => page_missing(page),
        }
    }

    fn arena<T: Slot>(&self) -> &PageArena {
        arena_of::<T>(&self.query_slots, &self.intern_slots)
    }

    fn push(&self, page: Page) -> usize {
        match self.pages.push(page) {
            Ok(index) => index as usize,
            Err(err) => panic!("salsa: adding a page to the table: {err}"),
        }
    }
```
   and replace, in `ingredient_index` (line 195), `page` (262), `memos` (354), `dyn_memos` (375)
   and `dyn_memos_unchecked` (390), `self.pages[X.0]` with `self.page_at(X.0)` (`page_idx.0`,
   `page.0`).
6. `page_count` (line 226): `self.pages.count()` becomes `self.pages.len()`.
7. `force_page` (line 277). `let page = self.pages.get_mut(page_idx.0);` becomes
   `let page = self.pages.get_mut(page_idx.0 as u32);`, and
   `*page = Page::new::<T>(ingredient, memo_types.clone());` becomes
```rust
                    let arena = arena_of::<T>(&self.query_slots, &self.intern_slots);
                    // alloc: one `Arc` count per page; the page's slots are in its tag's arena
                    *page = Page::new::<T>(ingredient, memo_types.clone(), arena);
```
   (the `Some` arm borrows `self.pages` mutably and the arenas shared: disjoint fields).
8. `push_page` (line 317). Before:
```rust
        PageIndex::new(self.pages.push(Page::new::<T>(ingredient, memo_types)))
```
After:
```rust
        PageIndex::new(self.push(Page::new::<T>(ingredient, memo_types, self.arena::<T>())))
```
   `push_uninit_page` (line 328). Before:
```rust
        PageIndex::new(self.pages.push(Page::new::<DummySlot>(
            IngredientIndex::new(0),
            // alloc: one page and one `Arc` count per 128 slots, when a page fills
            Arc::new(MemoTableTypes::default()),
        )))
```
After:
```rust
        PageIndex::new(self.push(Page::new::<DummySlot>(
            IngredientIndex::new(0),
            // alloc: one `Arc` per placeholder page, when persistence restores a database
            Arc::new(MemoTableTypes::default()),
            self.arena::<DummySlot>(),
        )))
```
9. `memos_mut` (line 401): `.get_mut(page_index)` becomes `.get_mut(page_index as u32)`.
   `for_each_memo_table_mut` (line 418): `for page_index in 0..self.pages.count() {` becomes
   `for page_index in 0..self.pages.len() {` and `self.pages.get_mut(page_index)` becomes
   `self.pages.get_mut(page_index as u32)`.
10. `ErasedSlots` (line 483) takes the column's iterator:
```rust
struct ErasedSlots<'db, P> {
    pages: P,
    current_page: Option<(PageIndex, &'db Page, std::ops::Range<usize>)>,
    slot_type_id: TypeId,
}

impl<'db, P: Iterator<Item = (u32, &'db Page)>> Iterator for ErasedSlots<'db, P> {
```
    and in `next`, `PageIndex::new(page_index),` becomes `PageIndex::new(page_index as usize),`.
    `slots_of`'s `pages: self.pages.iter(),` and `page_infos`' `for (_, page) in self.pages.iter()`
    are unchanged.
11. `Page::new` (line 597). Before:
```rust
    fn new<T: Slot>(ingredient: IngredientIndex, memo_types: Arc<MemoTableTypes>) -> Self {
        #[cfg(not(feature = "shuttle"))]
        let data: Box<PageData<T>> =
            // alloc: one page and one `Arc` count per 128 slots, when a page fills
            Box::new([const { UnsafeCell::new(MaybeUninit::uninit()) }; PAGE_LEN]);

        #[cfg(feature = "shuttle")]
        let data = {
            ...
        };

        Self {
            ingredient,
            memo_columns: memo_types.columns(),
            memo_types,
            slot_vtable: SlotVTable::of::<T>(),
            slot_type_id: TypeId::of::<T>(),
            allocated: AtomicUsize::new(0),
            data: NonNull::from(Box::leak(data)).cast::<()>(),
        }
    }
```
After (both configurations: uninitialized slots need no writes, so shuttle's stack concern is
gone):
```rust
    fn new<T: Slot>(
        ingredient: IngredientIndex,
        memo_types: Arc<MemoTableTypes>,
        arena: &PageArena,
    ) -> Self {
        Self {
            ingredient,
            memo_columns: memo_types.columns(),
            memo_types,
            slot_vtable: SlotVTable::of::<T>(),
            slot_type_id: TypeId::of::<T>(),
            allocated: AtomicUsize::new(0),
            data: arena.alloc(Layout::new::<PageData<T>>()),
        }
    }
```
    The `data` field's doc (line 130) becomes "The potentially uninitialized data of this page,
    a `PageData<SlotType>` in its tag's [`PageArena`]. As we initialize new entries, we
    increment `allocated`."
12. `src/interned.rs`, `unsafe impl<C> Slot for Value<C>` (line 1453), first item:
```rust
    const MEM_TAG: blaze_mem::Tag = blaze_mem::Tag::Intern;
```
13. Tests, in `table.rs`'s `#[cfg(test)]` module (add one if absent):
```rust
    #[test]
    fn pages_keep_their_order_and_their_arena() {
        let table = Table::default();
        let types = Arc::new(MemoTableTypes::default());
        let first = table.push_page::<DummySlot>(IngredientIndex::new(3), types.clone());
        let second = table.push_page::<DummySlot>(IngredientIndex::new(4), types);
        assert_eq!((first.as_usize(), second.as_usize()), (0, 1));
        assert_eq!(table.page_count(), 2);
        assert_eq!(table.ingredient_index(make_id(second, SlotIndex::new(0))), IngredientIndex::new(4));
        assert_eq!(table.query_slots.region.tag(), Tag::Query);
        assert_eq!(table.intern_slots.region.tag(), Tag::Intern);
    }
```
    The rest is held by every existing test (each makes inputs, tracked structs and interned
    values on these pages, `tests/persistence.rs` restores through `force_page` and
    `push_uninit_page`, `tests/deletion-drops.rs` drops slots through `drop_impl`) and by
    `tests/memory-usage.rs`, whose slot sizes don't change.

`boxcar` stays a dependency (`views.rs:18`).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): adapted to rows by page (`drop_impl` keeps its `first_row`, `Page::new` takes the arena after `memo_types` and still sets `memo_rows`); `PageArena::alloc` returns the start it claimed. Needs M9 (`Column::get_mut`, lane mem, `v2/requests/mem-db.md` request 11) to compile. Not built.

## M6. Graph pages from the kernel, not the allocator (`graph.rs`, `graph/edges.rs`, `graph/nodes.rs`)

The edge array's and node key columns' pages (`graph.rs:183` to `229`, used at
`graph/edges.rs:51`, `161` and `graph/nodes.rs:84`, `356`) are `alloc_zeroed` boxes so their
untouched tails stay out of the resident set. A region gives the same (kernel-zeroed, faulted in
on first write), tagged `Query`, without the general-purpose allocator. They are 4096 words and
up (16 KB and more), so a region each wastes nothing, and at most 20 of each exist. Shuttle's
atomics are not plain words, so that configuration keeps the boxes it builds today (the split
already exists; it is not a second path for the same build).

1. `src/graph.rs`: the three non-shuttle functions (`zeroed_u32s`, `zeroed_u64s`, `zeroed`,
   lines 180 to 216) become:
```rust
/// The words of a page of the node or edge columns.
#[cfg(not(feature = "shuttle"))]
pub(crate) type Words<T> = Zeroed<T>;
/// The words of a page of the node or edge columns; shuttle's atomics are objects of their own.
#[cfg(feature = "shuttle")]
pub(crate) type Words<T> = Box<[T]>;

/// `len` zeroed words for a page of the node or edge columns.
#[cfg(not(feature = "shuttle"))]
pub(crate) fn zeroed_u32s(len: usize) -> Words<AtomicU32> {
    // SAFETY: all-zero bits are an `AtomicU32` of 0, which needs no drop.
    unsafe { Zeroed::new(len) }
}

/// `len` zeroed words, as [`zeroed_u32s`].
#[cfg(not(feature = "shuttle"))]
fn zeroed_u64s(len: usize) -> Words<AtomicU64> {
    // SAFETY: all-zero bits are an `AtomicU64` of 0, which needs no drop.
    unsafe { Zeroed::new(len) }
}

/// `len` `T`s on a region of their own, tagged `Query`: committed whole and zeroed by the
/// kernel, so the part of a page never written is never touched and stays out of the resident
/// set.
#[cfg(not(feature = "shuttle"))]
pub(crate) struct Zeroed<T> {
    region: blaze_mem::Region,
    len: usize,
    _owns: std::marker::PhantomData<T>,
}

#[cfg(not(feature = "shuttle"))]
impl<T> Zeroed<T> {
    /// # Safety
    ///
    /// All-zero bits must be a valid `T`, and `T` must need no drop.
    unsafe fn new(len: usize) -> Zeroed<T> {
        debug_assert!(!std::mem::needs_drop::<T>());
        assert!(std::mem::align_of::<T>() <= blaze_mem::page_size());
        let bytes = std::mem::size_of::<T>()
            .checked_mul(len)
            .expect("salsa: a graph page fits in memory");
        let region = blaze_mem::Region::reserve(bytes.max(1), blaze_mem::Tag::Query)
            .unwrap_or_else(|err| panic!("salsa: reserving a graph page: {err}"));
        region
            .ensure(bytes)
            .unwrap_or_else(|err| panic!("salsa: committing a graph page: {err}"));
        Zeroed { region, len, _owns: std::marker::PhantomData }
    }
}

#[cfg(not(feature = "shuttle"))]
impl<T> std::ops::Deref for Zeroed<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: `new` committed `len * size_of::<T>()` bytes from the region's base, which is
        // page-aligned (so aligned for `T`, checked in `new`); the kernel zeroed them, zero bits
        // are a valid `T` (`new`'s contract), they change only through `T`'s interior
        // mutability, and they live as long as `self.region`.
        unsafe { std::slice::from_raw_parts(self.region.base().cast::<T>().as_ptr(), self.len) }
    }
}
```
   The shuttle `zeroed_u32s` and `zeroed_u64s` (lines 218 to 230) return `Words<AtomicU32>` and
   `Words<AtomicU64>` (the same `Box<[..]>` they return now).
2. `src/graph/edges.rs:51`: `pages: Box<[OnceLock<Box<[AtomicU32]>>]>,` becomes
   `pages: Box<[OnceLock<super::Words<AtomicU32>>]>,`. `fn page` (line 160) is unchanged: its
   `get_or_init` returns `&Words<AtomicU32>`, which derefs to its `&[AtomicU32]`.
3. `src/graph/nodes.rs:84`: `ids: Box<[AtomicU64]>,` and `ingredients: Box<[AtomicU32]>,`
   become `ids: super::Words<AtomicU64>,` and `ingredients: super::Words<AtomicU32>,`; their
   uses index through `Deref`.
4. Test, in `graph.rs`'s test module:
```rust
    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn zeroed_pages_are_zero_and_writable() {
        let words = super::zeroed_u32s(5000);
        assert_eq!(words.len(), 5000);
        assert!(words.iter().all(|word| word.load(Ordering::Relaxed) == 0));
        words[4999].store(7, Ordering::Relaxed);
        assert_eq!(words[4999].load(Ordering::Relaxed), 7);
    }
```
   (`Ordering` from `crate::sync::atomic`, as the module's other tests use it.) The edge and
   node tests (`graph.rs`, `graph/edges.rs`, `graph/nodes.rs`) hold the rest.

dedup-intern's request above rewrites the probe bodies of `graph/nodes.rs` and
`interned/table.rs`; M5 and M6 touch neither those bodies nor the key tables' fields, so the
two apply in either order.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): as written, plus the generic `zeroed` it replaces deleted. Not built.

## M7. The claim-free walk reuses the thread's stack (`function/maybe_changed_after.rs`)

The per-call buffers on the hit and revalidation paths, after S1 to S5:

| path | buffer | today |
|---|---|---|
| memo hit, `fetch_hot` (`fetch.rs:76`) | none | 0 allocations (`tests/alloc_budget.rs`) |
| claim-free deep verify, `deep_verify_unclaimed` (`maybe_changed_after.rs:392`) | the walk's stack, `SmallVec<[UnclaimedFrame; 8]>` | spills to the heap for a chain deeper than 8, and regrows (16, 32, 64 ..) on every verify of it: `log2(depth / 8)` allocations per verify |
| claimed deep verify, `deep_verify_memo` | recursion on the thread's stack | none |
| claims, `SyncTable::try_claim` (`sync.rs:111`) | a `HashTable` per shard behind its lock | grows once to the most claims a shard held at once, `remove` keeps it: 0 warm |
| backdating (`backdate.rs`) | none | 0 |
| cycle heads (`cycle.rs:222`) | `ThinVec` | only while a cycle iterates; owned by the memo |
| blocking on another task (`runtime.rs:124`, `373`; `dependency_graph.rs`) | a box and map entries | contention only, not a hit |
| recording a read (`active_query.rs:122`) | the slot's `FxIndexSet` | reused (M3 for the fingerprints) |

So the one change is the walk's stack. `UnclaimedFrame<'db>` borrows memos for the walk's `'db`,
so the thread keeps the stack's allocation as an empty `Vec<UnclaimedFrame<'static>>` and lends
it to each walk for that walk's lifetime.

1. Imports: delete `use smallvec::SmallVec;` (line 17; its only use is line 392) and add
   `use std::cell::Cell;` and `use std::mem::ManuallyDrop;`.
2. After `impl<'db> UnclaimedFrame<'db> { .. }` (after line 344):
```rust
thread_local! {
    /// The claim-free walk's stack, empty between walks, so a deep chain grows it once per
    /// thread.
    static UNCLAIMED_STACK: Cell<Vec<UnclaimedFrame<'static>>> = const { Cell::new(Vec::new()) };
}

/// The thread's walk stack, empty, for frames that borrow for `'db`. A walk inside a walk gets
/// an empty `Vec`, which allocates only if that walk grows it.
fn take_stack<'db>() -> Vec<UnclaimedFrame<'db>> {
    let stack = UNCLAIMED_STACK.take();
    debug_assert!(stack.is_empty());
    let mut stack = ManuallyDrop::new(stack);
    // SAFETY: the vector holds no frame (`give_stack` clears it, `Vec::new` starts empty), so no
    // borrow of another lifetime is carried over; `UnclaimedFrame<'static>` and
    // `UnclaimedFrame<'db>` differ only in a lifetime, so they have one layout, and the
    // allocation is the global allocator's, as it was.
    unsafe { Vec::from_raw_parts(stack.as_mut_ptr().cast(), 0, stack.capacity()) }
}

/// Gives the stack's allocation back to the thread, emptied.
fn give_stack(mut stack: Vec<UnclaimedFrame<'_>>) {
    stack.clear();
    let mut stack = ManuallyDrop::new(stack);
    // SAFETY: as in `take_stack`: empty, the same layout, the same allocation.
    let stack = unsafe { Vec::from_raw_parts(stack.as_mut_ptr().cast(), 0, stack.capacity()) };
    UNCLAIMED_STACK.set(stack);
}
```
3. `deep_verify_unclaimed` (line 383): its doc's last paragraph "The walk keeps its own stack
   (eight memos inline), so a chain of any depth is walked once, in the order the claimed check
   walks it." becomes "The walk keeps an explicit stack the thread reuses, so a chain of any
   depth is walked once, in the order the claimed check walks it, and allocates nothing once the
   thread has walked one as deep." Its body becomes:
```rust
        let Some(edges) = self.unclaimed_edges(zalsa) else {
            return false;
        };
        let mut stack = take_stack();
        stack.push(UnclaimedFrame::new(self, database_key_index, edges));
        let verified = walk_unclaimed(zalsa, db, &mut stack);
        give_stack(stack);
        verified
```
   and the loop moves, unchanged but for its first line, into a new free function after the
   `impl MemoHeader` block:
```rust
/// Walks `stack` (its bottom the memo checked) until that memo is verified (true) or only a
/// claimed check can decide (false).
fn walk_unclaimed<'db>(
    zalsa: &'db Zalsa,
    db: RawDatabase<'_>,
    stack: &mut Vec<UnclaimedFrame<'db>>,
) -> bool {
    while let Some(frame) = stack.last_mut() {
        // ... lines 395 to 451 of today's `deep_verify_unclaimed`, as they are ...
    }
    unreachable!("salsa: the claim-free walk returns when it pops its first memo")
}
```
   (In the moved lines `stack` is the parameter, already `&mut`; nothing else names the old
   local.) A walk that unwinds drops the lent `Vec`; the next one starts an empty one.
4. Held by M8's `a_deep_verify_that_changes_nothing_allocates_nothing` and by
   `tests/unclaimed_verify.rs` (a 300-deep chain, backdating, eight threads).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): as written (`walk_unclaimed`, `take_stack`/`give_stack`; the thread-local has an `// alloc:` line). Not built.

## M8. Tests

1. `vendor/salsa/tests/alloc_budget.rs`: add `use salsa::Setter;` after the `blaze_alloc_count`
   import, and:
```rust
#[salsa::tracked(returns(copy))]
fn link0(db: &dyn salsa::Database, input: Input) -> u32 {
    input.n(db)
}

#[salsa::tracked(returns(copy))]
fn link1(db: &dyn salsa::Database, input: Input) -> u32 {
    link0(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link2(db: &dyn salsa::Database, input: Input) -> u32 {
    link1(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link3(db: &dyn salsa::Database, input: Input) -> u32 {
    link2(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link4(db: &dyn salsa::Database, input: Input) -> u32 {
    link3(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link5(db: &dyn salsa::Database, input: Input) -> u32 {
    link4(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link6(db: &dyn salsa::Database, input: Input) -> u32 {
    link5(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link7(db: &dyn salsa::Database, input: Input) -> u32 {
    link6(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link8(db: &dyn salsa::Database, input: Input) -> u32 {
    link7(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link9(db: &dyn salsa::Database, input: Input) -> u32 {
    link8(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link10(db: &dyn salsa::Database, input: Input) -> u32 {
    link9(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn link11(db: &dyn salsa::Database, input: Input) -> u32 {
    link10(db, input) + 1
}

#[salsa::tracked(returns(copy))]
fn parity(db: &dyn salsa::Database, input: Input) -> u32 {
    input.n(db) % 2
}

#[salsa::tracked(returns(copy))]
fn above_parity(db: &dyn salsa::Database, input: Input) -> u32 {
    parity(db, input) + 1
}

/// A chain of twelve memos, deeper than the eight the walk's stack held inline, verified
/// unchanged after an edit elsewhere: the claim-free walk reuses the thread's stack.
#[test]
fn a_deep_verify_that_changes_nothing_allocates_nothing() {
    let mut db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 1);
    let other = Input::new(&db, 0);
    assert_eq!(link11(&db, input), 12);
    other.set_n(&mut db).to(1);
    assert_eq!(link11(&db, input), 12);
    other.set_n(&mut db).to(2);
    let value = assert_at_most(0, "a deep verify that changes nothing", || link11(&db, input));
    assert_eq!(value, 12);
}

/// The warm steady state of a re-check: an edit that `parity` absorbs runs it again (a frame
/// task, its edges into the edge array, its memo into its column's slab) and backdates it, so
/// `above_parity` is only verified. Three rounds warm the slab's free list and the frame.
#[test]
fn a_warm_recheck_allocates_nothing() {
    let mut db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 0);
    assert_eq!(above_parity(&db, input), 1);
    for n in [2, 4, 6] {
        input.set_n(&mut db).to(n);
        assert_eq!(above_parity(&db, input), 1);
    }
    input.set_n(&mut db).to(8);
    let value =
        assert_at_most(0, "the warm steady state of a re-check", || above_parity(&db, input));
    assert_eq!(value, 1);
}
```
2. New `vendor/salsa/tests/mem_frames.rs`, and in `vendor/salsa/Cargo.toml` after the
   `[[test]] name = "memory-usage"` entry:
```toml
[[test]]
name = "mem_frames"
path = "tests/mem_frames.rs"
```
   The file (needs `blaze_mem::frame::current`, mem-db's request 1 to lane mem in
   `v2/requests/mem-db.md`):
```rust
#![cfg(feature = "inventory")]

//! Each run of a tracked function's code is a `blaze_mem::frame::task` of the function's
//! `mem_tag` (`Query` by default): an execution, each fixpoint iteration, its cycle functions;
//! and a run nested in another leaves the outer run's frame as it found it.

use blaze_mem::{Tag, frame};

#[salsa::input]
struct Input {
    #[returns(copy)]
    n: u32,
}

fn tag_now() -> Tag {
    frame::current(|frame| frame.tag())
}

#[salsa::tracked(returns(copy))]
fn plain(db: &dyn salsa::Database, input: Input) -> Tag {
    let _ = input.n(db);
    tag_now()
}

#[salsa::tracked(returns(copy), mem_tag = Infer)]
fn inferred(db: &dyn salsa::Database, input: Input) -> Tag {
    let _ = input.n(db);
    tag_now()
}

#[test]
fn a_run_is_a_task_of_its_functions_tag() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 1);
    assert_eq!(plain(&db, input), Tag::Query);
    assert_eq!(inferred(&db, input), Tag::Infer);
}

#[salsa::tracked(returns(copy), mem_tag = Mir)]
fn inner(db: &dyn salsa::Database, input: Input) -> Tag {
    let n = u64::from(input.n(db));
    frame::current(|frame| {
        let _ = frame.alloc_slice_copy(&[n; 64]);
        frame.tag()
    })
}

/// The bytes the outer run's frame holds before and after an inner run, the tag the inner run
/// saw, and the outer run's tag after it.
#[salsa::tracked(returns(copy))]
fn outer(db: &dyn salsa::Database, input: Input) -> (usize, usize, Tag, Tag) {
    let before = frame::current(|frame| {
        let _ = frame.alloc_slice_copy(&[0u64; 16]);
        frame.used()
    });
    let inner_tag = inner(db, input);
    let (after, tag) = frame::current(|frame| (frame.used(), frame.tag()));
    (before, after, inner_tag, tag)
}

#[test]
fn a_nested_run_has_a_frame_of_its_own() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 3);
    let (before, after, inner_tag, tag) = outer(&db, input);
    assert!(before >= 128, "the outer run's bytes are in its frame");
    assert_eq!(before, after, "the inner run's bytes were not left in the outer run's frame");
    assert_eq!(inner_tag, Tag::Mir);
    assert_eq!(tag, Tag::Query);
}

#[salsa::tracked(returns(copy), cycle_initial = initial, cycle_fn = recover, mem_tag = Infer)]
fn fixpoint(db: &dyn salsa::Database, input: Input) -> u32 {
    assert_eq!(tag_now(), Tag::Infer);
    (fixpoint(db, input) + 1).min(input.n(db))
}

fn initial(_db: &dyn salsa::Database, _id: salsa::Id, _input: Input) -> u32 {
    assert_eq!(tag_now(), Tag::Infer);
    0
}

fn recover(
    _db: &dyn salsa::Database,
    _cycle: &salsa::Cycle,
    _last: &u32,
    value: u32,
    _input: Input,
) -> u32 {
    assert_eq!(tag_now(), Tag::Infer);
    value
}

#[test]
fn fixpoint_iterations_and_cycle_functions_run_in_frames() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 3);
    assert_eq!(fixpoint(&db, input), 3);
}

#[salsa::tracked(returns(copy), cycle_result = fallback, mem_tag = Nameres)]
fn immediate(db: &dyn salsa::Database, input: Input) -> u32 {
    assert_eq!(tag_now(), Tag::Nameres);
    immediate(db, input) + input.n(db)
}

fn fallback(_db: &dyn salsa::Database, _id: salsa::Id, _input: Input) -> u32 {
    assert_eq!(tag_now(), Tag::Nameres);
    7
}

#[test]
fn an_immediate_fallback_runs_in_a_frame() {
    let db = salsa::DatabaseImpl::new();
    let input = Input::new(&db, 1);
    assert_eq!(immediate(&db, input), 7);
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by speed-salsa (window 17, 2026-10-29 22:20): both `alloc_budget.rs` tests, `tests/mem_frames.rs` and its `[[test]]` entry, plus the amendment's test (`mem_tag = Parse` and `mem_tag = crate::PARSE_TAG`, a `Mir` constant). Not built.

## M9. `Column::get_mut` (to lane mem, `crates/blaze-mem/src/column.rs`; window `mem-next1`)

M5's table needs, for its `&mut` paths (`force_page`, `memos_mut`, `for_each_memo_table_mut`),
the published value at an index mutably. In `impl<T> Column<T>`, after `get`:
```rust
    /// The value at `index` once it is published, for the column's only user.
    #[inline]
    pub fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        let index = index as usize;
        let word = self.word(index)?;
        if word.load(Ordering::Relaxed) & (1 << (index % 64)) == 0 {
            return None;
        }
        // SAFETY: the ready bit is set, so the value was written whole; `&mut self` excludes
        // every other reader and writer, and the value lives as long as `self`.
        Some(unsafe { self.values.at(index * Self::SIZE).cast::<T>().as_mut() })
    }
```
Test, in blaze-mem's column tests:
```rust
#[test]
fn get_mut_changes_a_published_value() {
    let mut column = Column::<u64>::new(1 << 10, Tag::Query).unwrap();
    let index = column.push(1).unwrap();
    *column.get_mut(index).unwrap() = 9;
    assert_eq!(column.get(index), Some(&9));
    assert!(column.get_mut(index + 1).is_none());
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> speed-salsa (window 17, 2026-10-29 22:20): lane mem's (`crates/blaze-mem`), not salsa's; forwarded as mem-db.md request 11 and still unapplied there. M5 is written against it.

> done by mem (2026-10-29): as written (`crates/blaze-mem/src/column.rs`), with its test.

