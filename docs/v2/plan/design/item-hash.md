---
status: plan
summary: blaze v2's item identities: a body's type check, MIR and borrow check are filed in the store under an a-priori hash of everything they read (its own name-resolved HIR with no spans, the signature hashes of what it names, the crate's inputs) plus a constructive trace of the global facts they read (impl sets, method candidates, field names, constant values), so a result is a store read with no deep verify, shared across branches, machines and reverted edits; the hash is anonymous (Unison-style), with names and positions in a presentation layer, locations as a per-crate data table, debug info off the edit path, and item IDs in blaze's own reports.
date: 2026-10-30
---
# Item identities: a front-end result is a lookup by hash

Lane item-hash, approved by Andrew. Builds on lane items' hashes
([lanes/items.md](../lanes/items.md), [lanes/items/notes.md](../lanes/items/notes.md)), the
persistent goal cache (`hir_ty::next_solver::goal_store`), the dependency bodies part
(`hir_ty::check_cache`) and the HIR codec (`hir_def::hir::persist`). Works with lanes firewall
(which body reads are allowed across items), dbsp (incremental impl and name tables), graph and
tree-store (one mapped store format).

## 1. What and why

Today a body's inference, MIR and borrow check are salsa memos: valid in one database, verified by
walking their dependencies after each edit, gone on restart, never shared between branches,
machines or a revert. The goal: each is a value in blaze's content-addressed store under a key
computed before running it, so that

- a hit is two store reads and a few memoized hashes, with no walk of the dependency graph;
- the same body in another branch, another checkout, another machine or after a revert is a hit;
- moving an item inside its file, reformatting or editing another body changes nothing it reads.

Prior art, and what is taken from each:

| prior art | taken |
|---|---|
| Unison: a definition is the hash of its AST, names are metadata | the identity hashes content with every reference by hash, no names of the item itself, no positions |
| Build Systems à la Carte: constructive traces (Shake's cloud, Bazel's action cache) | dynamic dependencies (which impls a solve read) are a trace stored beside the key: a shape lists the facts read, the answer is filed under their values |
| Bazel, Nix: actions keyed by declared inputs, outputs by content | the a-priori part of the key is everything a body can read that is known before running it |
| rustc: `DefPathHash`, `hir_owner` hashes, the SVH | `ItemKey` is our `DefPathHash`; the identity is a `hir_owner` hash with resolved references in place of `DefPathHash`es, so it is stable across sessions as rustc's is not across crate edits |
| blaze's goal store | the same shape/answer record, at the granularity of a body instead of a goal |

## 2. The three names of an item

- **Key** (`blaze_items::ItemKey`): *which* item. Its crate's key, its parent's key, kind, name and
  raw AST id. Stable across edits of its content; a rename or a move to another module gives a
  new one. Never in an identity except where the program observes a name (section 3.6).
- **Interface hash** (`blaze_items::InterfaceHash`): *what a user sees*. Signature, generics and
  their bounds and where clauses, a type's fields and variants by position with their
  visibility, a trait's items, layout-affecting attributes (`repr`, `align`, `packed`,
  `transparent`, `non_exhaustive`), visibility as "public, crate, or the module n levels up",
  plus what a body leaks (section 4). Computed by `blaze_items::hash::interface_hash`.
- **Identity** (new, `blaze_item_hash::identity`): *what a body's results are a function of*,
  known before running them. Section 3.

An item's **body hash** is lane items' code hash (normalized MIR, callees by interface hash):
an output, used to key codegen. The identity is the input-side twin of it: it keys the steps that
produce the MIR.

## 3. The identity of a body

`identity(def) = H("blaze v2 item identity 1", crate inputs, owner, content, entries, names,
traits in scope, module chain)`, one salsa tracked query per `DefWithBodyId` with early cutoff (a
recomputed identity equal to the old one stops there). Every part below is listed with the reads
of inference, MIR building and the borrow check it covers: those reads are *covered* (section 6).

### 3.1 Crate inputs (`crate_inputs(krate)`, tracked, one per crate)

| input | why | covers |
|---|---|---|
| `blaze_items::key::crate_key` (name, version, origin, kind of target, every cfg, features) | cfg decides what was lowered; features change std's and the crate's code | crate graph input fields |
| edition | method resolution (`IntoIterator` for arrays), prelude, closure captures, `if let` rescope, never-type fallback | `Crate::data` edition reads |
| the target: tuple, data layout, arch, pointer width, endian | `size_of` in constants, layout of types in casts, `cfg` | `target_data_layout_query` |
| the toolchain: `goal_store::solver_version(build)` (pinned nightly, the solver's patch series, the front end's own code) | everything | the code itself |
| lang items, each by item key and signature hash | operators, `?`, `for`, `async`, closures, `Box`, `Copy`/`Sized` | `lang_items` |
| the crates it loads, in order, by crate key | which crates' impls and preludes are seen | `loaded_crates` |
| crate attributes: `#![feature]` set, `recursion_limit`, `type_length_limit`, `no_std`, `no_core`, `no_implicit_prelude`, `#![register_tool]` | solver depth, feature-gated semantics, preludes | `crate_attrs` / `AttrFlags` of the crate root |
| borrow checker mode (`hir_ty_borrowck::mode`), MIR options (opt level, overflow checks, debug assertions, panic strategy) | the outputs of MIR and borrow check | the session's options |

### 3.2 Owner

The signature hash (section 4) of the item the body is of and of its parent (the impl or trait of an
associated item, the enum of a variant's discriminant): the signature, generics and predicates
inference starts from. Covers `fn_sig_for_fn`, `generic_predicates_of`, `ImplSignature`,
`TraitSignature`, `generics` of the owner and its parent.

### 3.3 Content (own tokens, expanded and lowered, anonymous, no spans)

The body as `hir_def`'s codec writes it (`impl Persist for Body`) inside a relocating scope, so
that every salsa id is written as its index in a table of first appearance (`Relocation`), then
**anonymized** as Unison hashes a term (Andrew, 2026-10-30):

- locals, parameters, `let` bindings, closure captures and labels by binding position (their
  index in the body's arenas, de Bruijn style), never by name;
- generic and lifetime parameters by index in the owner's generics;
- a path by what it resolves to (section 3.5), never by its text, so a rename of a local, a
  parameter or the item itself, and a `use .. as` alias, keep the hash (a callee's rename does
  not yet, section 3.7);
- what is resolved *by name* after inference stays text: a field (`s.f`), a method (`x.m()`), the
  segments of a type-relative path after its qualifier (`Vec::new`'s `new`), and a path that
  resolves to nothing (its diagnostic names it);
- a `line!()`, `column!()`, `file!()`, `module_path!()` or `concat!` literal in a plain value
  position is a slot, not its value (section 3.7); in an array length, a type, a const argument,
  a const block, a pattern or `asm!` it stays by value, since there it becomes part of a type.

The source map is a separate buffer and is never read. Moving an item within its file,
reformatting, comments and renames keep the hash; what the body is after macro expansion is hashed,
so a macro's output change is a content change.

### 3.7 The presentation layer

Names and positions that reach the output stay exactly rustc's: debuginfo variable names, panic
`Location`s, `line!()`/`column!()`/`file!()`/`module_path!()` constants, `#[track_caller]`
locations, symbol names and diagnostics. They are a small layer of their own, keyed by its own
hash, `presentation(def)`: the names of the body's bindings and labels by position, the owner's
generic and lifetime parameter names, the item's name and module path, its file's path, and the
positions of the body's source map. A rename or a move changes the presentation hash and reruns
only what reads it:

- **MIR** keeps each location constant as a slot (its kind and `MirSpan`); the MIR record stores
  the slot, and reading the record fills it from the presentation layer, so moved code gets
  rustc's values without rebuilding MIR;
- **diagnostics** hold expression and pattern ids, never text or positions, and are rendered from
  the current database;
- **codegen** (lane codegen): debuginfo names, symbol names and location constants are keyed by
  the presentation hash, apart from the code hash, so the binary is byte-identical to a fresh build.

**Records name items relative to the body.** A record names the body's own item, its closures,
coroutines, opaque types and anonymous constants as "self", and each item one of its paths names as
"the target of path i", decoded through the current database's resolutions; everything else by item
key (those reads are facts keyed by item key, section 5). So a rename hits.

**What a target is in the hash** (section 3.5): its item key and signature hash. The key stays
because the trace's facts about a target (its constant value, its impls) are keyed by item key: with
the key out of the hash, `[u8; A]` edited to `[u8; B]` (two constants of one signature) would find
A's record and check A's fact. Dropping it needs facts about a body's targets keyed relative to
the body ("the value of target i"), as records are.

**Left, with why:** renaming an item changes the identity of the bodies that name it (above). A
body's module chain (section 3.5) is by module key, so moving an item to
*another module* or renaming an enclosing module changes the identity: privacy and `super::` can
change meaning there. Facts are keyed by item key, so renaming an item a body reaches only through
a type or an impl changes that fact's key and the body misses once.

### 3.4 Entries (the table's ids, each by stable identity)

| entry kind | hashed as |
|---|---|
| an item (`Function`, `Struct`, ..., `EnumVariant`) | its item key and its signature hash (a variant: its enum's) |
| a macro (`Macro2`, `MacroRules`, `ProcMacro`) | its item key and its signature hash (its rules or its expander's identity) |
| `Crate` | its crate key |
| `Block` | its index among the body's blocks (blocks are the body's own) and whether it holds items |
| `MacroCall` | its index among the body's calls and its macro's item key |
| `Context` (a syntax context) | its index among the body's contexts, its edition, its transparency, its outer expansion's call index and its parent context's index, recursively, so two bodies whose hygiene differs hash apart |
| `File`, `SpanFile` | "the owner's own file" or the file's key (`blaze_items::key::file_key`): never its path or position |

A dense id is never hashed: each entry is replaced by what it is in every database.

### 3.5 Names (what every path of the body resolves to)

Name resolution reads the crate's def map, one table for the whole crate: as a fact it would
change on every edit in the crate. The identity instead hashes what each name the body writes
resolves to, so a def map read by inference is covered. For each path of the body's expressions,
patterns, record literals, type refs and `asm!` operands, at the scope it is written in:

- the first segment, in the type, value and macro namespaces, through the body's resolver at that
  scope (`Resolver::resolve_path_in_type_ns`, `resolve_path_in_value_ns`, with the path's
  hygiene): a local binding by its index in the body, a generic parameter by its index, an item by
  key and signature hash (section 3.7), "none" with the path's text;
- each next segment, while the prefix is a module or an enum: its resolution in that module's
  scope (each namespace) or as that enum's variant;
- once the prefix is a type, the rest is type-relative: inference resolves it through impls and
  traits, which are facts (section 5), not the def map.

Plus the **traits in scope** at each distinct scope of the body (the owner's module, each block
with items, `use` inside blocks), as a sorted set of (key, signature hash): method calls and
type-relative paths search exactly these. And the **module chain** of the owner (its module's and
each ancestor's key up to the crate root): `pub(super)`, `pub(in path)` and private fields and
methods are visible by ancestry.

### 3.6 Names and positions the program observes

`type_name`, `module_path!`, symbol names, `#[no_mangle]`, `#[export_name]`, `#[track_caller]`
locations are read by const eval and codegen, never by inference, MIR building or the borrow
check of a body; the macros that expand to a name or position are presentation slots (section 3.7).
Codegen keys stay lane items' (`Fact::Name`) plus the presentation hash.

## 4. Where a caller reads a body (with lane firewall)

An identity is computed inside the body's own inference (`infer_body_query` asks the store
first), so it may not read any body's inference: an interface hash includes what a body leaks
(lane items' notes, "What a body leaks into an interface"), which for an `async fn`, a `const fn`
or anything naming one reaches back to the body being inferred, a salsa cycle. So the identity
names every item (the owner, its parent, each entry and resolution, each trait in scope, the
lang items) by key and **signature hash** (`blaze_items::signature_hash`, `SignatureHash`): the
interface hash's form with a fixed marker in place of each leak, each ref by its signature
hash. What a body then reads of a leak goes into its trace as a fact. The cases, each with
rustc's rule; lane firewall lists the same set as the only allowed cross-body reads
(`docs/v2/plan/lanes/firewall/notes.md`):

| leak | rustc's rule | in the signature hash | how a read enters |
|---|---|---|---|
| `impl Trait` return's hidden type, `async fn`'s future, a TAIT's hidden type | `type_of(opaque)` is computed from the defining body's typeck | its bounds; the hidden type is a marker | a read of `rpit_hidden_types` or `tait_hidden_types` is `Fact::HiddenTypes(owner)` in the trace |
| auto traits of an opaque type (`Send`, `Sync`, `Unpin`, `Freeze`) | auto-trait leakage: the solver looks through the opaque | a marker | `Fact::AutoTraits(opaque)` in the trace when the solver asks |
| const eval of a `const`, a `const fn`, an associated const, a static's initializer, an explicit discriminant | CTFE reads the MIR of what it calls | its type and flags; the body is a marker | a value read is `Fact::ConstValue` in the trace (a const, a static or a variant's discriminant by its key); CTFE's reads of MIR are its own dependencies |
| a closure's or coroutine's captures and held locals, where a signature's type names one | inferred with the enclosing body | a marker | the closure is inside its body: content of that body's identity; read from another body, `Fact::Captures` |
| inlining in release | codegen reads callee MIR (`deep` hash) | not in it | codegen only; not a front-end input |
| specialization (`default fn`) | the specializing impl's items | the impl's items by key | `Fact::Specializes` in the trace |

An anonymous constant of a signature (an array length, a const argument) is the signature's
own: its MIR stays in the signature form. The interface hash and every other hash of lane items
are unchanged; the signature hash is a fourth graph beside them.

## 5. Global tables: impl sets and method candidates in the trace

Trait solving and method resolution read tables keyed by trait and by self type, which no
a-priori key can name: which impls are relevant is known only once a goal is asked. They enter as
a **constructive trace**, the goal store's record one level up:

- after a body's step ran, every read it made is classified (section 6); each fact read is kept
  in a **shape**: the sorted list of facts. Under `shape_key(identity, step, slot)` one of 4 slots
  holds a shape;
- the **answer** (the result's bytes) is filed under `H(shape key, each fact's value now)`;
- a lookup reads the shapes of the identity, computes each fact's value now (each a memoized
  salsa query), and reads the answer under them. A shape whose facts now differ misses.

The facts are lane items' kinds (`blaze_items::Fact`), each with a value
(`blaze_items::facts::value`):

| fact | value | read where |
|---|---|---|
| `Implements { trait, self_ty }` | the hash of the impls of `trait` whose simplified self type unifies with `self_ty`, each by key and signature hash, and the blanket impls: the **relevant impl set** | `relevant_impls`, `impls_for_trait`, coherence |
| `Method { self_ty, name }` | the hash of the inherent and trait candidates named `name` for `self_ty` | inherent impls, method probing |
| `Signature(item)` | its signature hash | a callee or a type the content did not name (reached through a type) |
| `Fields(item)` (new, item-hash's) | the names of the variants and fields of an ADT, in order | field access, record literals and patterns, which name fields |
| `ConstValue(item)` | the evaluated value | const eval |
| `AutoTraits(opaque)`, `Specializes`, `Target`, `Crate` | as lane items defines | the solver |

**Maintained incrementally by lane dbsp.** Each value is a salsa query over dbsp's maintained
relations (impls per trait and per simplified self type, inherent impls per self-type head, trait
candidates per method name). dbsp keeps one salsa input per relation slice, so adding an impl
re-computes only the `Implements` values of its (trait, self type) slice and every body whose
shape lists another slice hits unchanged. Until dbsp lands, the values are computed by
`blaze_items::index` over all crates: the same values, more work per edit.

## 6. Every read is covered, a fact, or refuses the store

The step runs inside `salsa::capture_reads`. Each direct read (a tracked query, an input field, a
tracked struct field; interned values are pure functions of their fields and are skipped) is
classified by `BodyFacts::classify`:

- **covered**: its value is a function of the identity's parts. The list is closed and each
  entry names the part of section 3 that covers it (the body and its scopes by content, the def
  map by names, the owner's signature by owner, the crate's data by crate inputs).
- **fact**: it stands for one fact of section 5; the fact goes into the shape.
- **refused**: anything else. The result is not stored and the refusal is counted by query name
  (as the goal store's `inexpressible`). A refused body is inferred as it always is; nothing is
  answered another way.

The only way to miss an input is a wrong "covered" or a fact whose value does not change when the
read's value does. Both are what the tests of section 9 aim at.

## 7. Store records: one format with graph and tree-store

- Records are `blaze_format` buffers, read in place (zero copy) from the `ImageBuf` the store
  hands out; kinds `0x3800..0x3840` of `blaze_query::kinds::FRONT` (shape, inference answer, MIR
  answer, borrow-check answer); a record's store key is `QueryKey { kind, input }.id()`, as every
  blaze-query result is filed, so one key space and one store serve both.
- The store is `blaze-cas` through `hir_def::deps::ImageStore` (the local store over the global
  cache, `ReadThrough`), as the goal store, the dependency images and the bodies part are. On a
  restart the store's packs are mapped (`blaze-core/src/fs/mapped.rs`); the daemon image (lane
  graph's frozen graph, lane tree-store's trees, `crates/blaze-image` sections) holds no copy of
  these records: they are content-addressed and are read from the store's mapping. The frozen
  graph (section `SALSA_GRAPH`, 0x0501) holds node keys, filter words and edges, no memo values;
  if memo restore (queue 00822) links a frozen node to a record, the link is the record's store
  key (`QueryKey { kind, input }.id()`), never a copy.
- An inference answer is `hir_ty::check_cache`'s per-body record (the inference result in stable
  form, its ids through a portable table by item key). A MIR answer is the MIR body in the same
  stable form. A borrow-check answer is the verdict. Only passing verdicts are stored; a failing
  one is checked again, which is the build's error path.
- A record that does not open under its key is an error naming the body and the key, never a
  silent rerun (no-fallback).

## 8. Lookup and offer

In `infer_body_query`, `mir_body_query` and the borrow check's gate, when a store is installed
for the database (`body_store::install`, by the driver, beside the goal store):

1. `identity(def)`: memoized; after an edit recomputed only where a part changed.
2. For each slot: read the shape (one store read), compute its facts' values (memoized), read the
   answer (one store read). A hit returns the decoded result.
3. On a miss, run the step inside `capture_reads`, classify, and buffer the offer; the driver's
   `flush` writes shapes and answers on the store's threads after the build's front-end work.

A hit's salsa dependencies are the identity and the facts: in the same database the next edit
re-verifies only those, which makes the store the in-memory firewall too.

`BLAZE_ITEM_STORE=verify` runs the step on every hit as well and stops with an error naming the
body when the two results differ: the differential mode the harness runs.

## 9. Tests

- **Perturbation** (`crates/blaze-item-hash/src/tests/perturb.rs`): for each input of section 3
  (each crate input, the owner's signature and its parent's, a token of the body, each entry
  kind, each namespace of a resolution, a trait in scope, the module chain) and each fact kind of
  section 5, a fixture pair differing in exactly that input: the identity (or the fact's value)
  differs. And the negative: moving the item in its file, reformatting, renaming a local
  variable's unrelated sibling item, editing another body leave it equal.
- **Classification closure**: on the harness programs, every read of every body's inference is
  covered or a fact, or is in the refusal tally with its name; a covered read's query is in the
  closed list.
- **Differential, random edits** (`harness/v2/frontend/item_store.py` over `harness/apps` and the
  corpus projects, with `harness/apps/pick_edits.py`'s edits): build with the store, apply a random
  edit sequence (body edits, signature edits, impl added and removed, item moved, reverted), build
  after each with `BLAZE_ITEM_STORE=verify`: every hit equals a fresh run, and each program's
  output equals what blaze builds of the same state from nothing (a fresh home, store and
  target, `BLAZE_ITEM_STORE=off`); no harness compares with rustc or cargo (Andrew, 2026-10-30,
  queue 00955).
- **Allocation budget**: a hit allocates only the decoded result.

## 10. Speed of light

In `docs/v2/plan/lanes/item-hash/speed-of-light.md`. Summary: the identity hashes a body's
portable bytes (BLAKE3 at ~4-8 GB/s with SIMD, a 2 KB body in ~0.5 µs) and k ≈ 10-50 entries and
names (each a memoized lookup, ~20 ns); a hit is two store reads from mapped memory (~1 µs each,
page cache) plus the decode, against an inference of 50-500 µs: a 50-200x saving per body.

## 11. Paths

| path | owner |
|---|---|
| `crates/blaze-item-hash/` (identity, read classification, fact values, the `BodyFacts` impl, perturbation tests) | item-hash |
| `crates/hir-def/src/item_hash.rs` (the portable body bytes and table, public) | item-hash (new file in db's crate) |
| `crates/hir-ty/src/body_store.rs`, `crates/hir-ty/src/body_store/` (install, lookup, offer, flush, records) | item-hash (new files in db's crate) |
| hooks: `infer_body_query` (`hir-ty/src/infer.rs`), MIR (`hir-ty-mir`), borrow check gate (`hir-ty-borrowck/src/gate.rs`), the driver's session (`blaze-cargo/src/serve/unit/deps.rs`) | their owners; one targeted edit each, told by tmux |
| `harness/v2/frontend/item_store.py` | item-hash (in proof's `harness/v2/`, by request) |
| sections 12-14: location table, lazy debug info, item IDs in reports | codegen and link (12, 13), the daemon and the agent API (14), by request; item-hash owns the IDs' definition |

## 12. Locations as data

Andrew, 2026-10-30. Code never holds a location's value: it loads its `&'static Location` (and
each `line!()`, `column!()`, `file!()` value in a plain value position, section 3.3) from a
per-crate **location table**, so moving or editing lines rewrites only that table, and a live patch
of a move writes data, not code.

- **Today**: a `Location` is a `DataRequest::Location(owner, span)` filled when stitching
  (`hir-ty-codegen/fragment.rs`), and `Target::Position(SpanRef)` is a link-time entry of the span
  side table. But a function's patch identity hashes the location it reports (`patch.rs`,
  `Mark::new`'s `locations`, `patch_hash`'s `DataRequest::Location`), so a move re-patches the
  function's code, and `line!()`'s value, refilled into MIR from the presentation layer (section
  3.7), is compiled as an immediate.
- **The table**: one per crate, writable only by the loader and the patcher, read-only to the
  program. An entry is a `core::panic::Location` (`file: &'static str`, `line: u32`, `col: u32`, the
  layout of the pinned nightly's `core`) or, for a lone `line!()`/`column!()`, a `u32`, and for
  `file!()` a `&'static str`; the file strings are in a string pool the table points into. An entry
  is named by `(item identity, slot)`: the body's semantic identity (section 3) and the slot's
  index in its `location_literals` or its MIR spans, never by value. A function's code names its
  entries by relocation; its code hash and patch identity leave the values out.
- **Code**: a `Location` operand becomes the entry's address; a `line!()` value a load of the
  entry; a `#[track_caller]` call passes the entry's address as the hidden argument, as rustc
  passes its constant's. The optimizer must not fold a load from the table (it is external to the
  function). Release builds, which are never live-patched, bake the values as rustc does; both
  forms print the same.
- **A move or a line edit**: the identity and the code hash are unchanged (section 3.7), the
  presentation hash changes, the location table's entries of that crate are rewritten, and a live
  patch is a data write into the table (plus the string pool when a file is renamed), no code. A
  location inside a type (an array length, a const argument, section 3.3) is by value in the
  identity, so a move there changes the code, as it must.
- **Unchanged output**: `line!()`, `column!()`, `file!()`, `Location::caller()`, `#[track_caller]`
  and the panic message read the same bytes as under rustc.
- **Tests**: a harness program printing `line!()`, `column!()`, `file!()`, `Location::caller()`
  through `#[track_caller]` chains, and panicking (caught and uncaught), is moved, edited above,
  and its file renamed; after each, its stdout, stderr and exit status equal a fresh build's of
  the same state from nothing, and the live patch the daemon sends has no code section (only table and
  string-pool writes).

## 13. Debug info off the edit path

Andrew, 2026-10-30. Line tables and variable names are regenerated cheaply after an edit, or
served lazily when a debugger asks; an edit never waits on them.

- **Today**: split DWARF exists (`hir-ty-codegen/debuginfo/split.rs`: the object keeps lines and
  addresses, the rest goes to `.dwo`), a function's variables and inlined frames are a side record
  (`hir-ty-codegen/v2/side.rs`), and a function's patch identity includes its line rows when
  `debug` is set (`patch.rs` `Mark::of_function`), so a move re-patches it.
- **Keying**: a function's line rows and variable names are a debug record keyed by (its code
  hash, its body's presentation hash, section 3.7): code keyed by the code hash alone never carries
  them, and a rename or a move changes only the debug record.
- **Off the edit path**: the link step writes only a debug map (each function's address range
  and its debug record's key), as Apple's linker writes `N_OSO` stabs pointing at objects; the
  debug records are built after the edit at low priority, or on demand.
- **Served lazily**: the daemon answers a debugger's request for a debug record: on Linux, split
  DWARF `.dwo` files materialized by the daemon on first open, with a debuginfod-style lookup by
  build ID (the daemon's endpoint on a Unix socket, or loopback reachable only by the user, the
  store's security rule); on macOS, the debug map's objects, written by the daemon when `lldb`
  opens them (`dsymutil` packs them for a shipped build); on Windows, the PDB the same way.
- **Equal to a fresh build**: a debug record built after an edit is byte-identical to a fresh
  build's; `blaze build --release` and a `dsymutil`/`.dwp` package wait for all of them.
- **Tests**: after a move and a rename, the edit's time does not include debug info (the daemon's
  events show the build finishing before any debug record is made); `lldb`/`gdb` scripts break on a
  line and print a renamed local after the edit; the debug records equal a fresh build's.

## 14. Stable item IDs in blaze's reports

Andrew, 2026-10-30. On by default, out of band only: the program's stdout, stderr and panic text
never change. A later opt-in extension may print IDs in a program's own panic text; not now.

- **The ID**: `(item identity, node)`: the item's semantic hash (section 3; for a signature, its
  signature hash) and the node's place in its anonymous body (an `ExprId`, `PatId`, binding,
  `TypeRefId` or location slot, by kind and arena index). A move or a rename keeps it (section
  3.7); an edit of the item gives a new one.
- **Where it appears**: next to `file:line:col`, in blaze's own records only: test results, the
  `--events` records, diagnostics records (which already hold ids, not text: the store's records,
  section 3.7), and the agent API. A panic's ID comes from its `Location`'s place in the location
  table (section 12): the test runner's panic hook, which blaze owns, maps the `&Location` address
  to its entry and so to `(identity, slot)`, and writes it to blaze's side channel, never to the
  program's streams. A test failure is the test function's ID plus the panic's.
- **Back to a location**: the agent API's `locate(id)` answers the item's current file, line and
  column and its path now: the daemon keeps an index from identity to item (renames and moves
  keep the identity, so they are a lookup) and a log of each item's past identities (an edit of an
  item records old → new under its item key). An ID of an edited item maps to the item now and
  says the node is of an older version (`edited: true`, no position for the node); an item that is
  gone says so. One versioned schema in deterministic CBOR, as every agent API request
  (no-mcp).
- **Tests**: a failing test and a diagnostic in a harness project report IDs; after a move, a
  rename and an edit of the item, `locate` gives the current place, the program's output is
  byte-identical to a build with IDs off, and the `--events` records name the same ID after the
  move and the rename.
