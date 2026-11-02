---
status: plan
summary: query-unify's design for moving the front end off the salsa fork onto blaze-query (the dense, typed engine `blaze_query::db` and its macros `blaze-query-macros`), feature by feature, how each beats the fork, and the order of the port that deletes vendor/salsa*.
date: 2026-10-30
---
# blaze v2: one query database (query-unify)

Andrew, 2026-10-30: "move everything to blaze-query and delete salsa. It should be state of
the art and super super fast." Lane `db`, child `query-unify`. The inventory this rests on
(counts of 14:27) is in the lane report's appendix, `/tmp/blaze-lanes/v2-query-unify-inventory.md`.

## Where things stand

`crates/blaze-query` ([query-database.md](query-database.md)) is the build side's database:
keys are content IDs of CBOR-encoded arguments, values are `Persist` bytes, views are VFS
roots. The front end (hir-*, base-db, blaze-items and ~25 more crates, 286 files) runs on the
salsa fork: keys are dense `u32` IDs of interned and input structs, values are plain Rust
values (`Arc<DefMap>`, `InferenceResult<'db>`), inputs are mutated in place under `&mut Db`,
and queries recurse through cycles with fixpoint recovery. Neither database depends on the
other.

The front end can't move onto the content-keyed engine as it is: hashing a key into a 32-byte
content ID costs more than the whole hit budget (a BLAKE3 of 16 bytes is ~50 ns; a hit must be
a few ns), and few front-end values are `Persist`. So `blaze-query` gets a second key space, the
dense one, in the same crate and on the same rules (a fingerprint the reader checks on every
dependency, early cutoff that eviction can't break, a `Sync` database read by `&`, single flight
through the plane, `blaze_mem` regions and frames). It is built fresh, from the fork's measured
lessons ([speed-salsa.md](speed-salsa.md)), not copied: no `dyn Ingredient`, no attach
thread-local, no downcasters, no revision-keyed claim table.

## The engine: `blaze_query::db`

### Ingredients and the storage

- Every tracked function, input struct, interned struct and tracked struct is an
  **ingredient**, numbered process-wide on first use: a `static INDEX: AtomicU32` in its
  generated code (0 until the first call registers it under a lock, then never changes).
- `blaze_query::db::Storage` holds one slot per ingredient number in a reserved region
  (`MAX_INGREDIENTS` = 8192 pointers), filled with the ingredient's table on first use with one
  compare-and-swap. No `TypeId`, no hash map, no downcast: the slot's number is the type's.
- The front end's `base_db::Db` holds a `Storage` and implements `blaze_query::db::Database`
  (`fn storage(&self) -> &Storage`). Generated code is generic over `D: Database + ?Sized`,
  so with the one concrete `Db` each query is a static, inlined call (frontend/db.md's design).

### IDs

- `blaze_query::db::Id` is a `NonZeroU32`, half of salsa's `u64` (index and generation): the
  front end never frees an interned value (every interned struct says `revisions =
  usize::MAX`), so there is no generation. `Option<Id>` is 4 bytes.
- IDs come from one ID space per storage, in pages of 1024 slots; each page belongs to one
  ingredient (`Storage::owner(id)`, one load). That is what an enum of IDs (`GenericDefId`,
  `#[derive(Supertype)]`) needs to say which variant an `Id` is, what `NamesIds` and
  `blaze-item-hash`'s read classification need, and what keeps any two keys of different types
  apart in a memo column.
- A page's rows are its ingredient's own type (an interned struct's fields, an input's fields
  and their stamps), in a column reserved once: `id → page base + slot × size`, two loads.

### Memos

- A tracked function's table is a column of **rows by key ID**, in pages that mirror the ID
  pages (a page of rows is committed when the first key of that ID page is asked). A function of
  more than one argument, or of an argument that is no ID, interns its arguments into a hidden
  interned ingredient first (one lock-free probe), so every memo row is found by an `Id`.
- A row is: `verified: AtomicU32` (the view it was last verified in, plus flags), `changed:
  u32` (the view its value last changed in: its fingerprint, below), `durability: u8`, `node:
  u32` (its number in the dependency graph), its edge span (`AtomicU64`: start and length),
  its value in place (`UnsafeCell<MaybeUninit<V>>`), and for an `lru` function the value's
  64-bit hash. `const` assertions hold the header to 32 bytes.
- **A warm hit** (`verified == view`): the ingredient's static number, the storage's slot, the
  row page, the row's header, and the value read in place: four dependent loads, no lock, no
  write to shared memory, no allocation; inside a query, a push of the row's `u32` node onto the
  thread's edge stack. The fork's hit is the same loads plus the attach thread-local, the task
  depth and the downcaster's view check.
- A value is written only by the run that holds the key's flight, and only while the row's
  `verified` names an earlier view. A reader reads it only after seeing `verified == view`
  (acquire). A value published in a view is never written in that view, and a new view is
  opened only under `&mut` (no reader left), so `&'db V` lives as long as `&'db Db`.

### Fingerprints and early cutoff

- The fingerprint an edge saw is the dependency's `changed` stamp: an edge on node `n` read
  by a memo verified in view `v` saw a fingerprint unchanged since `v` exactly when
  `changed(n) <= v`. A stamp is a fingerprint that needs no bytes on the edge: it is the
  red-green rule (rustc's `try_mark_green`, salsa's `changed_at`), and it makes an edge 4 bytes
  where blaze-query's content-keyed edges are 56.
- A run that produces a value equal to the old one keeps the old stamp (backdating), so its
  readers verify without running. Equal means `==` while the old value is in memory, and equal
  64-bit hashes once it was evicted: an `lru` function's value is hashed (`V: Hash`, through
  `blaze_ids`' stable hasher) as it is evicted, the hash kept in its row, and a rerun of an
  evicted row hashes its new value to compare. A value that stays in memory is never hashed.
  So an evicted value that comes back the same keeps its stamp and **eviction never counts as
  a change** (the `INCREMENTAL.md` bug: every macro of a file expanded again on each edit). A `no_eq` function's value is never compared,
  so it is never hashed either (no `V: Hash` with `lru`): every run of it, evicted or not, is a
  change (R19, qu-check8: hir-ty-codegen's `Option<Inlinable>`).
- No backdating when the new run's durability is lower than the old one's, or for a value made
  inside a cycle (#350).

### Views and durability

- The front end has one view at a time: `Storage::view()`, a `u32` bumped by every input write
  (under `&mut`). Old views are gone because `&mut` ended every borrow of them; what-if views
  of the front end are a database per candidate, as now.
- `Durability::{LOW, MEDIUM, HIGH}` are **input kinds**: workspace files, workspace structure,
  libraries and frozen tables. A row's durability is the lowest kind it read, transitively; the
  storage keeps the last view each kind changed in. A row whose kind has not changed since it
  was verified is verified by one compare and one store of `verified` (the first read in a
  view), never by walking its edges: an edit to a workspace file never walks a library query.

### Transitive-input summaries (Andrew, 15:56)

Every node has a fixed 128-bit **summary**: a Bloom word of the inputs its value can depend on,
transitively. A memo whose summary shares no bit with the inputs changed since it was verified
is green with no edge walk; otherwise it walks its edges as below. A false positive costs a
walk, never a wrong answer. (The fork had a 62-bit form of it, `graph` closure filters with a
change log; this replaces it at 128 bits and with the refresh rule below.)

- **Where.** A column of `[u64; 2]` by node (a `blaze_mem` region beside the colors), not in
  the row header: the hit reads 32 bytes of header and never the summary.
- **An input field's** summary is two bits: two hashes of its node number (`(n × K1) >> 57`
  and `(n × K2) >> 57`, two odd 64-bit constants), set when the field's node is made.
- **A memo's** is the OR of the summaries of every edge it recorded, ORed into the running
  frame as each read is recorded (one load from the column per edge, on the run path only),
  and stored when the memo is written. A **tracked field's** is its creator's at the write: a
  field changes only when its creator runs, so its readers must see its creator's inputs.
- **Refresh.** A memo verified by a walk (its edges in read order, each dependency verified or
  run) stores, with its new `verified`, the OR of its edges' current summaries: a dependency
  that ran again may read inputs it didn't before, and a summary of the old reads would then
  miss them. A memo made green by its summary keeps it; none of its dependencies can have run.
  Invariant: a memo verified in view `v` has a summary that covers every dependency's summary
  as of `v`.
- **Masks.** Each `&mut` that bumps the view ORs the bits of every input field it set into the
  new view's mask. The storage keeps the masks of the last 64 views as suffix ORs (`since[i]` =
  the OR of the masks of the newest `i + 1` views, 64 ORs of 16 bytes per bump), so the mask
  of everything changed after view `v` is one load, `since[now − v − 1]`. A memo verified more
  than 64 views ago takes the all-ones mask and walks.
- **All ones.** A memo that made an untracked read (`report_untracked_read`), a provisional row
  (in a cycle) and a `Reads` captured while either was open have the all-ones summary. A
  `synthetic_write(d)` puts all ones in its view's mask.
- **Order of the checks** on a memo verified in an earlier view: (1) durability (no input of
  its kind changed since: green, one compare); (2) `summary & since[..] == 0`: green, two
  loads and a 128-bit AND; (3) the walk, which refreshes the summary.
- `Reads` (capture/replay) carries the summary of what it read, so a replay ORs it in: `Reads`
  is 32 bytes, not 16.

Bench (`query_hit`, query-unify-bench): a memo at depth 64 of a graph of 1M edges, one input
outside its closure changed: verified by its summary in tens of ns, against the fork's walk.

### Verifying

- A row verified in an earlier view, and not made green by its durability or its summary
  (above), walks its edges in read order. For each edge, the node's
  color (a `u64` per node in one column: the view it was verified in and its `changed`) says in
  one load whether it is verified now and its stamp; only a node not yet verified in this view
  is verified first (recursively, through its own ingredient, by its node's `(ingredient, id)`).
  The walk stops at the first edge with `changed > verified`: the row runs again. Nothing is
  hashed on this path. Colors are prefetched four edges ahead.
- **Input summaries** (Andrew, 15:56). Each node has a fixed 128-bit word, a Bloom filter
  of the input fields under it: an input field sets two bits (two hashes of its node, so per
  field, finer than per input), a run's is the OR of its edges' words when it is written, a
  tracked field's is its creator's so far, a cycle's or untracked run's is all ones. The
  storage keeps the OR of the written fields' bits of each of the last 64 views. A row whose
  durability check fails is green, with no walk, when its word ANDs to zero with the fields
  written since it was verified; else it walks as below, and a walk that proves it ORs its
  dependencies' current words into its own (a dependency that reran with new reads grew its
  cone, and the next check must see that). A false positive costs a walk, never a wrong answer:
  every field in a row's cone at its last verify sets a bit of its word, and a dependency's
  cone grows only by a run that a change in the old cone caused. The word is a column by node
  beside the colors, so the header stays 32 bytes and the hit doesn't load it.
- Verifying takes no claim (two tasks verifying the same row reach the same answer and store
  the same view); running does (single flight, below). A row made inside a cycle is never
  verified by its edges: it runs again (its cycle runs again with it).
- Edges are one append-only array of `u32` nodes per storage (blaze-mem region), each run's a
  contiguous span in read order; a read equal to the last one recorded is not recorded again.
  `&mut` (`Storage::compact`) settles the live spans once the array is more than twice what is
  live.

### Single flight, cancellation, tasks

- A run of a key goes through the plane's single flight (`blaze_graph::pump::Flights`, keyed
  by the row's node): one run at a time, other tasks wait pumping, and a wait that would close a
  cycle of tasks unwinds with `Cancelled::WouldWaitOnItself`, which `base_db::run_on_pool`
  posts again (as today). No thread is ever spawned by a query.
- A thread's query stack is a thread-local stack of frames. A wait that pumps (the flight's,
  `Db::run_all`'s) pushes a barrier first, so a job pumped above it starts a stack of its own:
  a hit never asks the pump for its depth.
- `Storage::stop_readers()` sets a flag every query call checks (one relaxed load from the line
  the view is on): set, it unwinds with `Cancelled::PendingWrite`. The next view clears it, and
  so does `Storage::resume_readers(&mut self)` (R14, 01211) for a host that took `&mut` and
  wrote nothing (base-db's `Shared::write` and `idle`, which set only fields that differ): with
  `&mut` no reader is left, so the stop has done its work, and clearing it opens no view and
  re-verifies nothing. `unwind_if_cancelled(db)` is the same check for long loops.

### Cycles

The two forms the front end uses keep salsa's semantics, so every result is the same:

- `cycle_result = f` (63 queries): when a key is asked on its own task's stack, the asker gets
  `f(db, id, args..)`, and so does **every** query of the cycle (the head and each query between
  it and the re-entry), so the result doesn't depend on which query of the cycle was asked first.
- `cycle_initial = f` with or without `cycle_fn = g` (7 + 2 queries): fixpoint iteration. The
  re-entered key (the head) answers `f(..)` provisionally, the queries between run on it, the
  head runs again on their results and `g(db, &Cycle, last, new, args..)` (or `new`, without
  `g`) is compared with the last provisional value; equal, the head and every query of the cycle
  are final, else each runs again, at most 200 rounds (then a panic naming the head).
- A provisional row belongs to its head: another task that reaches one waits on the head's
  flight. Nested cycles merge into the outermost head. A panic inside a cycle leaves each of its
  rows empty (never a provisional value taken as final).
- A cycle across tasks is a wait that would close a cycle of tasks: one task gives way (above),
  runs again later and meets the cycle on its own stack, so a cycle is always resolved by one
  task, deterministically from its head.

### Inputs, interned values, tracked structs

- **Inputs** (33): a row per instance in its own ID page: each field's value, `changed` stamp,
  durability and node. `Input::new(db, ..)`, `Input::builder(..).durability(d).new(db)`,
  `input.field(db)` (records the field's edge), `input.set_field(db).with_durability(d).to(v)`
  (under `&mut`, returns the old value, bumps the view), `#[get(name)]`, `#[set(name)]`,
  `#[default]`, `#[returns(..)]`, `singleton` (`Input::get(db)`, `try_get`).
- **Interned** (24): a lock-free probe of a `blaze_mem::swiss::Index` by the value's hash, then
  the row compare (one cache line; no lock, no write on a hit); an insert takes the shard's
  lock. Never freed, so a read records no edge (salsa records one for its GC). `constructor =`,
  `unsafe(no_lifetime)`, `debug`, `#[returns(..)]`, lifetimes `<'db>` as salsa has them;
  `revisions` is accepted for nothing (there is no GC) only until the port drops it, then
  refused.
- **Tracked structs** (6, all in hir-def): ported by hand to interned structs (identity) plus,
  for a `#[tracked]` field, an input-like field written by the query that makes it, read with an
  edge. Their creation's identity is the creating query's key plus the struct's own fields.

### Singletons (R18, 01228)

A singleton's absence is observable (`try_get` gives `None`), so it is a read and its creation
a write. Its table reserves the one ID and its field nodes when made, stamped 0 ("never
changed") and unmade; `try_get` of the unmade row records a read of field 0's node and gives
`None`. `new`/`builder(..).new` of a singleton take `&mut D` and open a view (kind `LOW`, the
field nodes' bits written) that repaints every field node: exactly the queries that saw it
absent run again, in the view right after it is made. `&` can't make a singleton (the fork's
`&` creation left readers of the absence valid forever).

### What the front end reads of the graph

- `capture_reads(db, f) -> (T, Option<Reads>)`, `replay_reads(db, reads)`,
  `unshareable_reads(db)` (the trait solver's `DepNodeIndex`, the goal, pick and body stores):
  a capture is a frame whose edges are copied out as a span of the edge array, with their
  lowest durability, highest stamp and summary (`Reads`, 32 bytes, valid in its view only; replaying
  one of another view panics). `None` when the capture read a provisional row.
- `Reads::for_each_read` and `Storage::ingredient_of(node)`, `ingredient_name`, `read_name`
  (a field read by its field's name, as salsa named it), `owner(id)`:
  what `blaze-item-hash`, `blaze-items` and `hir-firewall` classify reads with, in place of
  `DatabaseKeyIndex`, `zalsa().lookup_ingredient`, `lookup_page_type_id` and `ids_of`. A
  function of several arguments gives its key back as its tuple (`Query::key_of(db, id)`).
- Events (`WillExecute`, `DidValidateMemoizedValue`, `DidInternValue`) go to a hook the
  storage is made with, checked by one load on the cold paths only.
- `report_untracked_read(db)`: the running row gets durability `LOW` and a stamp that changes
  every view (it reruns each view). `synthetic_write(db, d)` bumps the view as a write of kind
  `d` does.

### LRU and eviction

- `lru = N`: the value cap, not a correctness knob. At a view boundary (`&mut`) a function's
  rows past `N`, oldest `verified` first, drop their value and keep stamp, hash and edges;
  nothing is written on a hit (no recency list). `lru = 0`: no cap, evictable on demand
  (`Query::evict_values(db)`, `hir/src/evict.rs`).
- The cap is a runtime value (R15, 01212): `N` is its start, held in the function's table, and
  `f::set_lru_capacity(db: &mut D, capacity: usize)` replaces it (0: no cap), applied at the
  next view's sweep. Every `lru` function is on the sweep list whatever its cap, so a cap set
  later on a function declared `lru = 0` is swept too. Users: base-db's `Db::admit_parses` (the
  parse budget per revision) and hir-ty `providers.rs`'s MIR-body caps. The cap is read only by
  the sweep, never on a hit.

### Memory, tags, frames

- Pages, edge array and colors are `blaze_mem` regions tagged `Tag::Query` (interned rows
  `Tag::Intern`); a run is a `blaze_mem::frame::task(mem_tag, ..)` as on the fork.

### The daemon image

The fork's interned image (`salsa::image`, `#[salsa::interned(image = ..)]`) is used by no
front-end crate today; the port keeps the same section (`SectionId::SALSA_INTERNED`, renamed
`QUERY_INTERNED`) for interned rows, written from the ID pages, so IDs stay stable across
restarts. Front-end results in the image need their values to be `Persist`; that is the work
of the lanes that own those types, not of this port, and the report says which are not.

## Lane db's six decisions (queue 01104), as settled here

1. **Keys.** Agreed: the warm path never hashes (key → dense ID → row). A content ID of an
   interned value is computed when the image is written (its fields, with each ID inside
   replaced by that ID's own content ID, items by blaze-items' `item_key`), and kept beside the
   row from then on, never at intern time: most interned values (solver types) never reach an
   image, and hashing each at intern time would cost every intern ~50 ns.
2. **Values.** Typed in-memory values, returned by reference as salsa's `returns(..)`. Their
   fingerprint in-process is the `changed` stamp (above), which costs no hashing; a value hash
   is made only where it pays: `lru` functions (cutoff after eviction) and kinds that are
   `Persist` (the image, where a stamp means nothing in another process). Moving kinds to
   `Persist` is per kind, through the persist code that exists, by the lanes that own the types.
3. **The handle.** `&Db` stays the handle; the running query is a frame on the thread-local
   stack (above), so the port is an attribute swap. `blaze_query::db::attach(db, f)` and
   `with_attached(f)` put a storage pointer in a thread-local for `Debug` impls and tests; hir-ty
   keeps its own `attach_db` for the solver's printing.
4. **Interning.** As hir-ty's next-solver `Index` (64 shards, open addressing, lock-free hit,
   insert under the shard's lock), over the ID pages' rows; IDs `u32` and dense.
5. **Cycles.** salsa's semantics as above (fallback for every participant, fixpoint with
   `cycle_initial`/`cycle_fn`, 200 rounds, deterministic from the head).
6. **Inputs.** base-db's inputs become `#[blaze_query::input]`s of the dense space; `Durability`
   is the input's kind (workspace file, workspace structure, library). Reading file text through
   `blaze_query::tree::Tree` is the content-keyed side's and stays there. Room is left for
   queue 00758's `lint_options` input (one more field of `Crate`).

## The macro surface: `blaze-query-macros`

One proc-macro crate (syn 3, quote, proc-macro2), re-exported as `blaze_query::{tracked,
input, interned, Supertype}`. The grammar is salsa's for every option the front end uses, so
the port of a file is `salsa::` → `blaze_query::` (for attributes, `Durability`, `Setter`,
`Id`, `Supertype`, `Cycle`) and nothing else:

- `#[blaze_query::tracked(returns(ref|copy|clone|deref|as_ref|as_deref), lru = N,
  mem_tag = Tag, cycle_result = f, cycle_initial = f | closure, cycle_fn = g, no_eq,
  self_ty = T)]` on a free fn, on fns of a `#[blaze_query::tracked] impl`, and on a
  `struct` (tracked structs, above).
- `#[blaze_query::input(singleton, debug)]` with field attributes `returns(..)`, `get(..)`,
  `set(..)`, `default`.
- `#[blaze_query::interned(debug, constructor = name, unsafe(no_lifetime))]`.
- `#[derive(blaze_query::Supertype)]` on an enum of ID types.
- Refused, with an error naming the option: everything the front end doesn't use
  (`specify`, accumulators, `persist`, `heap_size`, `unsafe(non_update_types)`).

Generated code calls only `blaze_query::db::plumbing` (one module, documented as the macros'
own), so the front end's sources name no engine internals.

## The contract: what the engine gives the macros and the front end

Fixed here so the engine (`crates/blaze-query/src/db/`) and the macros
(`crates/blaze-query-macros`) are written at once. Both sides hold to these names and
signatures; a change goes through this section first.

Behaviour the front end may rely on beyond the signatures: a memo is verified in a new view
by, in order, its durability kind (one compare), its 128-bit input summary ANDed with the
fields written since (green when zero; "Input summaries" above), and only then its edges in
read order. None of it changes a result, only how much is walked.

### Public, in `blaze_query::db` (the front end names these as `blaze_query::db::X`)

The root `blaze_query::` has the content-keyed side's own `Database`, so the dense names are
not re-exported there; only the macros are (`blaze_query::{tracked, input, interned,
Supertype, Value}`), and generated code names `::blaze_query::db::..` (queue 01119).

```rust
pub struct Id(NonZeroU32);                       // db/id.rs as it is: from_u32, as_u32, MAX_U32
pub trait AsId { fn as_id(&self) -> Id; }
pub trait FromId: AsId { fn from_id(id: Id) -> Self; }
pub trait FromIdIn: AsId { fn from_id_in(storage: &Storage, id: Id) -> Option<Self>; }

pub trait Database: Sync + 'static {             // base_db::Db implements it, nothing else in a build
    fn storage(&self) -> &Storage;
    fn storage_mut(&mut self) -> &mut Storage;   // a write: ends every reader's borrow
}
pub struct Storage;                              // Send + Sync; Storage::new(hook: Option<Hook>)
pub type Hook = Box<dyn Fn(&Storage, Event) + Send + Sync>; // dyn: set once at setup, called on cold paths
impl Storage {
    pub fn view(&self) -> View;
    pub fn synthetic_write(&mut self, durability: Durability);
    pub fn stop_readers(&self);                  // readers unwind Cancelled::PendingWrite
    pub fn readers_stopped(&self) -> bool;       // stop_readers since the last view or resume (01120)
    pub fn resume_readers(&mut self);            // clears the stop flag (clock bit 32); no view (R14)
    pub fn owner(&self, id: Id) -> Option<u32>;  // the ingredient that owns id's page
    pub fn ingredient_name(&self, ingredient: u32) -> &'static str;
    pub fn ingredient_file(&self, ingredient: u32) -> &'static str;   // `file!()` of its definition (01125)
    pub fn field_name(&self, ingredient: u32, field: u32) -> &'static str;   // by a field read's number (01295)
    pub fn read_name(&self, key: DatabaseKeyIndex, kind: ReadKind) -> &'static str;  // a field read by its
                                                 // field's name, as salsa's, any other by its ingredient's
    pub fn ids_of(&self, id: Id, out: &mut Vec<Id>);  // an interned row's fields' ids (01127)
    pub fn keep_every_value(&mut self, keep: bool);   // no lru eviction while set (01128)
    pub fn memory_usage(&self, out: &mut Vec<MemoryUsage>);   // one per ingredient (01129)
    pub fn compact(&mut self);
}
#[derive(Clone, Copy, Debug)]
pub struct MemoryUsage { pub name: &'static str, pub rows: u64, pub row_bytes: u64, pub value_bytes: u64 }
/// What an interned field names besides itself being an id: `ids_of` pushes an `AsId` field's
/// id and calls this for a field whose type implements it (hir-def lib.rs, hir-ty next_solver/ty.rs).
pub trait NamesIds { fn names_ids(&self, out: &mut Vec<Id>); }
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct View(u64);                            // replaces salsa::Revision; View::as_u64(): views opened,
                                                 // never reused (R17, 01243: rows' 28-bit stamps renumber at
                                                 // their limit, this count doesn't; caches key on it)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Durability(u8);                       // LOW = 0, MEDIUM = 1, HIGH = 2; Durability::KINDS = 3

pub trait Setter: Sized {
    type FieldTy;
    fn with_durability(self, durability: Durability) -> Self;
    fn to(self, value: Self::FieldTy) -> Self::FieldTy;   // returns the old value
}

pub struct Cycle<'a>;                            // head_ids() -> impl Iterator<Item = Id>, id() -> Id, iteration() -> u32

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reads;                                // 32 bytes: edge span, durability, view, changed, 128-bit summary
impl Reads {
    pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool;
    pub fn for_each_read<B>(&self, storage: &Storage, f: impl FnMut(Read) -> ControlFlow<B>) -> Option<B>;
}
#[derive(Clone, Copy, Debug)]
pub struct Read { pub key: DatabaseKeyIndex, pub kind: ReadKind }
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadKind { Function, InputField(u32), TrackedField(u32), Interned }   // the field's number;
                                                 // an interned read records no edge, so `Interned` is never reported
pub fn capture_reads<D: Database, T>(db: &D, f: impl FnOnce() -> T) -> (T, Option<Reads>);
pub fn replay_reads<D: Database>(db: &D, reads: Reads);
pub fn unshareable_reads<D: Database>(db: &D);   // every capture open in the running query gives `None`
pub fn report_untracked_read<D: Database>(db: &D);
pub fn unwind_if_cancelled<D: Database>(db: &D);
pub fn pump_barrier<R>(f: impl FnOnce() -> R) -> R;   // around a wait that pumps other jobs (`Db::run_all`)

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DatabaseKeyIndex { ingredient: u32, key: Id }   // Debug prints `name(Id(n))`
impl DatabaseKeyIndex { pub fn ingredient_index(self) -> u32; pub fn key_index(self) -> Id; }

#[derive(Clone, Debug)]
pub struct Event { pub kind: EventKind }
#[derive(Clone, Debug)]
pub enum EventKind {
    WillExecute { database_key: DatabaseKeyIndex },
    DidValidateMemoizedValue { database_key: DatabaseKeyIndex },
    DidInternValue { key: DatabaseKeyIndex },
    WillIterateCycle { database_key: DatabaseKeyIndex, iteration: u32 },
    WillCheckCancellation,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cancelled { PendingWrite, WouldWaitOnItself, PropagatedPanic }   // see below
impl Cancelled { pub fn throw(self) -> !; pub fn catch<T>(f: impl FnOnce() -> T + UnwindSafe) -> Result<T, Cancelled>; }
pub fn attach<D: Database, R>(db: &D, f: impl FnOnce() -> R) -> R;
pub fn with_attached<R>(f: impl FnOnce(Option<&Storage>) -> R) -> R;

/// A value that may stay in a row across views though it names `'db`: interned handles and
/// values of them, never a `&'db` into a memo. `#[derive(blaze_query::Value)]` checks it field
/// by field, as `salsa::SalsaValue`'s derive did; `'static` types need nothing.
pub unsafe trait Value {}
```

`blaze_graph::Cancelled` is a unit struct (blaze-graph's lib.rs), so the engine defines
`blaze_query::db::Cancelled` as the enum above, unwinds with it (`resume_unwind`, no panic hook),
and maps the plane's `Cancelled` (a wait that would close a cycle of tasks) into
`WouldWaitOnItself`; base-db's `run_on_pool` downcasts to it. `PropagatedPanic` is for hosts
that rethrow a job's panic; the engine itself runs a key again after its runner panicked.

### `blaze_query::db::plumbing` (generated code only)

Every ingredient is a `static` of its generated code, so a call is `static → slot → row`:

```rust
pub use super::ingredient::{Ingredient, Kind, Vtable};   // Kind: Function, Input, Interned, Tracked
pub use blaze_mem::Tag;

/// The vtables the macros put in their statics (`Vtable`'s fields are the engine's):
/// `static VTABLE: Vtable = Vtable::function::<C>();`
/// `static INGREDIENT: Ingredient = Ingredient::new(&VTABLE);`
impl Vtable {
    pub const fn function<F: Function>() -> Vtable;
    pub const fn input<I: Input>() -> Vtable;
    pub const fn interned<I: Interned>() -> Vtable;
    pub const fn tracked<T: Tracked>() -> Vtable;
}

pub enum CycleKind { Panic, Result, Fixpoint }

/// `returns(as_ref)` and `returns(as_deref)` (salsa's `SalsaAsRef`/`SalsaAsDeref`), for
/// `Option<T>` and `Result<T, E>`: the macro returns `<V as ReturnsAsRef>::AsRef<'db>`.
pub trait ReturnsAsRef { type AsRef<'a> where Self: 'a; fn as_ref(&self) -> Self::AsRef<'_>; }
pub trait ReturnsAsDeref { type AsDeref<'a> where Self: 'a; fn as_deref(&self) -> Self::AsDeref<'_>; }

/// One tracked function. `Key` is `Id`-backed: the one argument's type, or the hidden
/// interned tuple of several (or of none: a singleton key). `FromIdIn`, not `FromId`, so an
/// enum of IDs (`#[derive(Supertype)]`, e.g. `GenericDefId`) can be a key; every macro-made
/// struct's `from_id_in` checks its page's owner.
pub trait Function: 'static {
    type Db: Database;
    type Key: AsId + FromIdIn + Copy + Send + Sync + 'static;
    type Value: Send + Sync + 'static;            // `'db` erased to `'static` by the macro
    const NAME: &'static str;                     // `f`, or `SelfTy::f` in a tracked impl / with `self_ty`
    const FILE: &'static str;                     // `file!()` at the definition (every trait here has it)
    const LRU: usize;                             // the starting cap; 0: no cap (R15: the live cap is the table's)
    const EVICTABLE: bool;                        // `lru = N` given (0 too): `evict_values*` exist; `hash` is real unless NO_EQ (R19)
    const NO_EQ: bool;
    const CYCLE: CycleKind;
    const TAG: Tag;
    fn ingredient() -> &'static Ingredient;
    fn execute(db: &Self::Db, key: Self::Key) -> Self::Value;
    fn cycle_result(db: &Self::Db, id: Id, key: Self::Key) -> Self::Value;   // CycleKind::Result
    fn cycle_initial(db: &Self::Db, id: Id, key: Self::Key) -> Self::Value;  // CycleKind::Fixpoint
    fn cycle_recover(db: &Self::Db, cycle: &Cycle<'_>, last: &Self::Value, new: Self::Value,
        key: Self::Key) -> Self::Value;           // Fixpoint without cycle_fn: returns `new`
    fn equal(old: &Self::Value, new: &Self::Value) -> bool;   // `==`; `false` when NO_EQ
    fn hash(value: &Self::Value) -> u64;          // EVICTABLE and not NO_EQ only (else it panics): blaze_ids' stable hasher
}
pub fn value_hash<V: Hash + ?Sized>(value: &V) -> u64;   // what `hash` calls: the stable hasher
pub fn fetch<'db, F: Function>(db: &'db F::Db, key: F::Key) -> &'db F::Value;
pub fn evict_values<F: Function>(db: &mut F::Db) -> usize;
pub fn evict_values_of<F: Function>(db: &mut F::Db, keys: &[F::Key]) -> usize;
pub fn set_lru_capacity<F: Function>(db: &mut F::Db, capacity: usize);   // EVICTABLE only; next sweep (R15)

// Inputs, interned and tracked structs name no database (their attributes don't): their
// functions take the `Storage` (`Database::storage(db)`), so their methods are generic over
// `D: Database + ?Sized` and their `Debug` impls read through `with_attached`.

/// One input struct. Its fields are numbered 0..FIELDS; each has its own stamp and node.
pub trait Input: 'static {
    type Fields: Send + Sync + 'static;           // the tuple of its fields
    const NAME: &'static str;
    const FILE: &'static str;
    const FIELDS: u32;
    const FIELD_NAMES: &'static [&'static str];   // in field order; `Storage::field_name`
    const SINGLETON: bool;                        // a second `input_new` panics naming NAME
    fn ingredient() -> &'static Ingredient;
}
pub fn input_new<I: Input, const N: usize>(storage: &Storage, fields: I::Fields,
    durability: [Durability; N]) -> Id;           // N == FIELDS, the macro writes it
pub fn input_fields<'s, I: Input>(storage: &'s Storage, id: Id) -> &'s I::Fields;  // records no edge
pub fn input_read<I: Input>(storage: &Storage, id: Id, field: u32);                 // records field's edge
pub fn input_singleton<I: Input>(storage: &Storage) -> Option<Id>;
pub struct InputSetter<'s, I: Input, T> { /* storage: &'s mut Storage, id, field, durability, at: fn(&mut I::Fields) -> &mut T */ }
pub fn input_set<'s, I: Input, T>(storage: &'s mut Storage, id: Id, field: u32,
    at: fn(&mut I::Fields) -> &mut T) -> InputSetter<'s, I, T>;   // implements Setter<FieldTy = T>

/// One interned struct. `Fields` with `'db` erased to `'static`; equality and hash are the
/// fields' own. A function's hidden key is one too, named `f::interned_arguments`; the macro
/// gives the function `f::arguments(db, id) -> Option<(A, B)>` for readers of its keys.
pub trait Interned: 'static {
    type Fields: Eq + Hash + Send + Sync + 'static;
    const NAME: &'static str;
    const FILE: &'static str;
    fn ingredient() -> &'static Ingredient;
    /// What `Storage::ids_of` gives for a row: each `AsId` field's id, and `names_ids` of each
    /// field that implements `NamesIds`, in field order.
    fn ids_of(fields: &Self::Fields, out: &mut Vec<Id>);
}
pub fn intern<I: Interned>(storage: &Storage, fields: I::Fields) -> Id;
pub fn intern_by<I: Interned, Q: Hash>(storage: &Storage, key: Q,
    eq: impl Fn(&Q, &I::Fields) -> bool, make: impl FnOnce(Q) -> I::Fields) -> Id;  // lookup by a borrowed
                                                 // form (`T::new(db, "name")` for a `String` field);
                                                 // `Q`'s `Hash` feeds a hasher exactly as `Fields`' does
/// What an interned constructor's arguments may be, as salsa's (interned.rs): `L: Lookup<F>`
/// with `F: HashEqLike<L>` for each field `F`, with salsa's impls (`T` for `T`, `&T` for `T:
/// Clone`, `&T` for `Box<T>`/`Arc<T>`/`triomphe::Arc<T>`, `&str` for `String`, `&[A]` and
/// `[A; N]` for `Vec<T>`, `&Path` for `PathBuf`, `Cow` for its owned forms).
pub trait Lookup<O> { fn into_owned(self) -> O; }
pub trait HashEqLike<O> { fn eq(&self, data: &O) -> bool; }
/// `Interned::ids_of`'s dispatch, as salsa's: `(&&&IdsOf(&field)).push_ids(out)` pushes the
/// field's id when it is `AsId` (`IdOfAsId`, on `&&IdsOf`), calls `names_ids` when it is
/// `NamesIds` (`IdOfNamed`, on `&IdsOf`), and does nothing otherwise (`IdOfOther`, on `IdsOf`).
pub struct IdsOf<'a, T: ?Sized>(pub &'a T);
pub trait IdOfAsId { fn push_ids(&self, out: &mut Vec<Id>); }
pub trait IdOfNamed { fn push_ids(&self, out: &mut Vec<Id>); }
pub trait IdOfOther { fn push_ids(&self, out: &mut Vec<Id>); }
pub fn interned_fields<'s, I: Interned>(storage: &'s Storage, id: Id) -> &'s I::Fields;
pub fn interned_arguments(storage: &Storage, id: Id) -> Option<&'static str>;  // a hidden tuple's function name:
                                                 // its owner's NAME without the `::interned_arguments` suffix

/// Tracked structs: an interned identity `(creator, occurrence, identity fields)` plus, per
/// `#[tracked]` field, a cell the creating query writes and readers read with an edge.
pub trait Tracked: 'static {
    type Identity: Eq + Hash + Send + Sync + 'static;   // the untracked fields' tuple
    type Fields: Send + Sync + 'static;           // the `#[tracked]` fields' tuple
    const NAME: &'static str;
    const FILE: &'static str;
    const TRACKED_FIELDS: u32;
    const TRACKED_FIELD_NAMES: &'static [&'static str];   // the `#[tracked]` ones, by number
    fn ingredient() -> &'static Ingredient;
    /// `field` here and in `tracked_fields` is the field's number among the `#[tracked]` ones
    /// (0..TRACKED_FIELDS); `false` for a `#[no_eq]` field.
    fn equal_field(field: u32, old: &Self::Fields, new: &Self::Fields) -> bool;
}
pub fn tracked_new<T: Tracked>(storage: &Storage, identity: T::Identity, fields: T::Fields) -> Id;
pub fn tracked_identity<'s, T: Tracked>(storage: &'s Storage, id: Id) -> &'s T::Identity;
pub fn tracked_fields<'s, T: Tracked>(storage: &'s Storage, id: Id, field: u32) -> &'s T::Fields; // records the field's edge

/// What `#[derive(Supertype)]` builds on: each variant's `FromIdIn`, tried in order (one
/// page-owner load each).
pub fn owner_is(storage: &Storage, id: Id, ingredient: &'static Ingredient) -> bool;
```

The macros generate, for a tracked function `fn f(db: &Db, a: A, b: B) -> V`: a hidden
`struct Key(Id)` interned over `(A, B)` (none for one `Id`-backed argument), a braced
`struct f { _priv: Infallible }` (the type namespace only, so it sits beside `fn f`, as the
fork's does), a private `struct Configuration` implementing `Function` with `execute` calling
the user's body (private, so a private value type in a `pub fn` is no E0446), its `static
VTABLE` and `static INGREDIENT`, and `fn f(db, a, b) -> <returns>` doing
`plumbing::fetch::<Configuration>(db, key)` then the return mode. `evict_values`/`evict_values_of`/
`set_lru_capacity` (functions with `lru = N`) and `arguments` (several arguments) are inherent associated functions
of `struct f`, so `mir_body_query::evict_values(db)` (hir/src/evict.rs) reads as it does now. In a
tracked `impl`, the user's body stays in the impl as a private method (so `Self` and the impl's
`'db` mean what they did), the struct is `TypeName_method` beside the impl, and the method calls
it.

Names (01121): an ingredient's `NAME` is the fork's string, so `Db::log` expectations, the
kind tables (hir-ty `db.rs` `"ItemBounds::of"`), blaze-item-hash's `RULES` and hir-firewall's
matches don't change: a free fn `f`; a tracked impl's method `SelfTy::method` (`self_ty`'s
text when given); a hidden key `f::interned_arguments`; an input, interned or tracked struct
its type's name. `Vtable` carries `file!()` too (01125).

Owner checks (01131): `input_fields`, `interned_fields`, `tracked_identity`, `tracked_fields`
and every generated `from_id_in` check `owner(id) == I::ingredient().index()` (one load beside
the row's) and panic naming both ingredients otherwise: ids come from persisted `u32`s (span
hygiene, hir-def persist, hir-ty check_cache), and a row is never read as another type's.
`FromId::from_id` on a macro-made struct is unchecked and `#[doc(hidden)]`: only the engine and
`from_id_in` call it.

Hand-written databases (01123): a test may define `struct TestDb(Storage)` with `impl
Database`; a build has one, `base_db::Db`.

`'db` erasure: a key, field or value type naming the database lifetime is stored as its
`'static` form and transmuted back at the boundary; the macros check such a type is
`blaze_query::Value` (`'static` types need nothing). The engine implements `Value` for what
salsa's `SalsaValue` covered (primitives, `str`, `String`, `PathBuf`, `Box`/`Arc`/`Rc`/`Vec`/
`Option`/`Result`/tuples/arrays/slices of `Value`s, `&'static T`, `PhantomData`, the std and
hashbrown maps and sets, `smallvec`, `thin_vec`, `triomphe::Arc`, `indexmap` where it depends
on them). `#[derive(blaze_query::Value)]`'s per-field proof is `#[value(unsafe(prove(P, ..)))]`
or `#[value(unsafe(prove_safe_to_retain_manually))]` (salsa's `#[salsa_value(..)]`).

## How it is measured

- `crates/blaze-bench/benches/query_hit.rs`: the same query shape on the fork and on
  `blaze_query::db` (a function of one interned ID returning `u64`, 64K keys, scattered and
  sequential), warm hit and hit inside a query (with the edge push), deep verify of 16K rows of
  16 edges, an interned hit, and a cycle of 8 to fixpoint. Both numbers go in
  [speed-of-light.md](speed-of-light.md) and the report. The fork is measured before it is deleted.
- `blaze_query::db`'s tests: unit tests of each part; Hegel properties (a random graph of
  queries over random inputs, a random sequence of writes: every result equals a from-scratch
  run, each query runs at most once a view, a backdated value's readers don't run); allocation
  budgets (`blaze-alloc-count`: zero allocations for a warm hit, a hit inside a query once the
  stacks have grown, an interned hit, and a verify of an unchanged graph); cycle tests ported
  from the fork's (`cycle_fallback_immediate`, fixpoint, nested, panicking in a cycle); and a
  parallel test of single flight and of a cycle across tasks.

## The port, bottom up

1. `blaze_query::db` and `blaze-query-macros`, with their tests and the bench (no front-end
   file changes).
2. `tools/v2/salsa_to_query.py` rewrites a crate's sources (the paths above, `salsa::Id` to
   `blaze_query::db::Id`, `as_bits`/`from_bits` to `as_u32`/`from_u32`), then prints what is left:
   every use of `salsa::plumbing`, `capture_reads`, events, `zalsa`. (2026-10-31: it and its
   replay records, `tools/v2/salsa_port/`, were deleted once the port merged, 04cca11739.)
3. The crates are switched in one window the coordinator holds for them, in this order, each
   checked (`cargo check -p <crate> --all-targets`) before the next: span, intern, base-db,
   hir-expand, hir-def, hir-ty, hir-ty-* , blaze-items, blaze-item-hash, hir-firewall, hir,
   blaze-lints, blaze-lower, blaze-macro-*, blaze-cargo, the rest. A crate can't be switched
   alone (an ID type of base-db is a key of hir-def's queries), so the window covers all of
   them, and no crate is left on salsa when it ends.
4. `vendor/salsa*`, the root manifest's `salsa*` entries, patch, lint cfg and `Cargo.lock` lines
   go; [crates.md](../../reference/crates.md) and [db.md](../db.md) say what is where.
