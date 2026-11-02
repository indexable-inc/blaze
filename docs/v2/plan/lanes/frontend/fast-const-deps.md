---
status: plan
summary: frontend-fast-const-deps — a dependency's `const` item with no generic arguments, once evaluated, is kept in the store (and so the global cache) under its crate's check-result key, its stable item key and the encoding's version, as its bytes and a type with no salsa id; `const_eval_query` asks for it before reading the body, and a miss records the value for the driver to write after the build.
date: 2026-10-29
---
# frontend-fast-const-deps: dependencies' constants by content

Section 3.3 of [fast-const.md](fast-const.md). Written 2026-10-29; nothing was built or run, so
costs are counted from the code and every claim of agreement is what a named test checks once
the tree builds. Code: `crates/hir-ty/src/consteval/deps.rs` (which constants, the key, the
store's input, `cached`, `record`, `drain`) and `consteval/deps/codec.rs` (the value's bytes);
tests in `consteval/deps/tests.rs` and `crates/hir-ide/src/tests/consteval/deps.rs`.

## 1. Which constants

A `const` item `def` asked with empty generic arguments, of a crate the driver lists in
`KeptConsts` (`deps::set`): a dependency (`hir_def::deps::eligible`: no workspace member in
its closure) whose check results are filed by frontend-fast-bodies-cache. A constant in a block
has no item key and is not kept. Every other constant (a generic or associated one with
arguments, a workspace crate's, a static, an anonymous constant) is evaluated as today; that is
the one path for it (a split by kind), not a fallback.

**The value does not depend on where it is asked.** `const_eval_query` is keyed by `trait_env`
as well, and `cached` ignores it: every `trait_env` gets the kept value. That is sound because
the evaluation of a constant with no generic arguments only selects impls for trait references
whose types and traits are all nameable from the constant's own crate (its body has no
parameter to stand for a downstream type). By the orphan rule, an impl that applies to such a
trait reference is in the crate of the trait or of one of the types (or of a fundamental
wrapper's argument), each of which the constant's crate depends on; a downstream crate cannot
add one, and coherence forbids two that overlap. So the impls selected, and hence the value, are
the same from every dependent. This is why rustc evaluates such a constant once, in its own
crate, and puts the value in `.rmeta` (`eval_to_const_value_raw`, encoded for every `const`
item), which every dependent reads. The evaluator's other inputs are the owner crate's: its
target layout and its const-eval budget (`crate_id.workspace_data(db).const_eval`,
`Evaluator::with_machine`), both in the crate's image key through `CrateWorkspaceData::content`.

`Mode::Check` (set by the driver as it sets `deps::serve`'s mode for the daily check run)
serves nothing: `cached` answers `None`, the constant is evaluated, and `record` compares the
value's bytes with the kept bytes, recording a difference through
`hir_def::nameres::frozen::record_difference` (kind `"constant values"`). `Mode::Verify` serves
the kept value and re-encodes it, recording a difference from the kept bytes.

## 2. The key

`deps::key(base, item) = BLAKE3("blaze v2 dependency constant value 1"; base, item, VERSION)`:

- **`base`, the crate's check-result key** (frontend-fast-bodies-cache's
  `blaze_diagnostics::cache::bodies_key`, `hir_ty::check_cache::key` of the image key and the
  borrow checker's mode; not `cache::key`, whose lint flags change no constant): the
  image's key as filed (`hir_def::deps::Image::key`: the crate's content and files, its
  dependencies' keys, the toolchain, the target, the const-eval budget, `build`, and the values
  of the crate's reads) and what the check reads beyond it. The image key alone is not enough:
  frontend-deps' reads (`deps/reads.rs`, `Reads::of`) cover the macro calls of module scopes and
  signatures, not those inside bodies, so a proc macro or an `include!` in a constant's body (or
  in a `const fn` it calls) is an input the image key leaves out. The check-result key has to
  cover body expansions' reads and each dependency's own check-result key anyway (a check
  evaluates dependencies' constants); told frontend-fast-bodies-cache and frontend-deps
  (2026-10-29, tmux, and `v2/requests/frontend-deps.md`). The driver hands the base over: at
  session start for each crate whose outcome was served (`set`), at the end for each crate
  checked in this session (`drain`), since a fresh crate's base is known only once its bodies are
  expanded.
- **`item`**, frontend-deps' item key (`hir_def::deps::const_key`, asked in
  `v2/requests/frontend-deps.md` and applied): the item's container (module index in the crate's def map,
  impl or trait by module and ast id) and ast id, with no salsa id and no path.
- **`VERSION`** of the bytes below (`codec::VERSION`).

The kept bytes start with the version and the full key, so a value filed under another key is
refused (an error naming the constant), never served.

## 3. The value

`codec.rs`, little-endian, no alignment needed (read where the store keeps it, no copy):

    value  := VERSION:u8 key:[u8;32] ty memory map
    memory := len:u32 bytes
    ty     := 0 bool | 1 char | 2 str | 3 never | 4..=9 int (isize i8 i16 i32 i64 i128)
            | 10..=15 uint (usize ..) | 16..=19 float (f16 f32 f64 f128)
            | 20 array ty len:scalar | 21 slice ty | 22 tuple n:u32 ty^n
            | 23 ref region:u8 (0 'static, 1 erased) mut:u8 ty | 24 raw-ptr mut:u8 ty
    scalar := size:u8 (1..=16) data:u128
    map    := 0 (empty) | 1 len:u32 bytes (simple)
            | 2 n:u32 (address:u64 len:u32 bytes)^n m:u32 (address:u64 align:u64)^m (complex)

Kept: every scalar (integers, `bool`, `char`, floats), `str`, `!`, arrays (length a `usize`
value), slices, tuples, references with a `'static` or erased region and raw pointers of those,
with an empty or simple memory map, or a complex one of pieces and alignments only. That covers
`&'static str` and `&'static [u8]` constants, whose bytes are addresses into their map: the
map is kept whole, so the decoded allocation is the evaluated one, address for address.

Not kept, so evaluated every time (the one path for them): a type naming a definition (an ADT, a
function, a closure, a trait object, an alias, a parameter), any other region, an array length
that is not a `usize` value, and a complex map with a static's memory, a vtable or a `TypeId`
(each holds a salsa id or a type the table would have to relocate). ADTs could go through
frontend-deps' relocation table (`deps/signatures/table.rs` writes items portably), but the
table and its `Enc`/`Dec` are `pub(super)` in hir-def and an ADT value's layout reads the ADT's
fields again in the reader; that is left for a later change, and never served partially: the
encoder answers `None` for the whole value at the first part it cannot write.

Decoding checks every length against the bytes, a type's depth (at most 64), addresses in
increasing order, and refuses trailing bytes: damaged bytes are refused within bounds, never a
panic. The store checks the content hash on every read.

## 4. The store

- **Read.** `KeptConsts` is a salsa input (singleton, `Durability::HIGH`) holding the driver's
  store (`hir_def::deps::ImageStore`, the local store or `ReadThrough` over the global cache, as
  an `Arc<dyn ImageStore + Send + Sync>`: `// dyn:` at that boundary only, one probe per constant
  per session), the mode, and the crates kept with their base keys. `cached` reads it, hashes the
  key and makes one `get`.
- **Write.** `record` encodes the value (on a miss only) and pushes it onto one of 16 pending
  lists, chosen by the item key's first byte, each behind its own mutex: a push holds it for a
  `Vec::push`, and no read path takes it. No I/O on a worker. After the build the driver calls
  `drain(db, bases)`: it takes every list, files each value under its key (the bases now include
  crates checked this session), drops values of crates with no base (their check results were
  refused, so their constants are too, and it counts them), sorts and dedups by key (a key with
  two different values is an error naming the constant: evaluation is not deterministic), and
  `put`s each on the store's pool (`ImageStore::for_each`).
- **Requests to lane cargo** (`v2/requests/cargo.md`, `## frontend-fast-const-deps`): an owned
  adapter (`Arc<dyn ImageStore + Send + Sync>`, the same store `deps::serve` reads), `set` right
  after `deps::serve` and the check-result serve, `drain` after the check-result store, its
  error stopping the build like `deps::store`'s.

## 5. Speed of light

A hit: hash the key (one BLAKE3 compression of 96 bytes, about 60 ns; frontend-deps' item key
is another, about 60 ns over a memo hit of `module_indexes`), one store probe of a mapped table
(about one DRAM miss and a 32-byte compare, 110 to 150 ns), and decode: a header compare, a few
bytes of type, a `Box<[u8]>` of the value's bytes and its interning (about 45 ns,
[fast-const.md](fast-const.md) section 1). About **0.3 µs**, against parsing, lowering, inferring
and evaluating the body (5 to 20 µs for a literal constant, far more for one calling a
`const fn`). The bound is the probe and the result's interning; the hashing could be cut by
keying a crate's values in one table opened once per crate, but a content-addressed store keeps
one value per key, and a table rewritten as constants are added would be a new value every
session. A miss costs the evaluation plus one encode (a walk of the type and a copy of the
bytes) and one push.

Prior art: rustc's `.rmeta` constant values (per crate, per machine); this is that, shared
across machines through the global cache, as Bazel's remote cache shares actions.

## 6. What proves it

`consteval/deps/tests.rs` (no evaluation):

- `every_kept_kind_reads_back`: each scalar, `str`, `!`, arrays, slices, tuples (nested),
  references, raw pointers, each map kind: encode, decode, the same `Allocation`.
- `what_is_not_kept_is_not_encoded`: an ADT, a parameter, a reference with another region, a
  complex map with a static: `None`.
- `damaged_values_are_refused_within_bounds`: every truncation and a flip of each byte either
  reads back some allocation or is refused, never a panic; a value under another key is refused.
- `the_key_changes_with_each_input`: base, item, version.
- `a_hit_allocates_only_its_result`: with a database, a kept value served by `cached` allocates
  at most the result's bytes (`blaze-alloc-count`), and returns the kept value.
- `a_recorded_value_is_drained_under_its_key`: `record` then `drain` puts the value under
  `key(base, item)`, once, and a crate with no base is counted and not written.

`crates/hir-ide/src/tests/consteval/deps.rs` (with evaluation):

- `a_served_constant_equals_the_evaluated_one`: a dependency's constants (integer, `bool`,
  `char`, float, tuple, array, `&str`) evaluated in one database and drained, then served in a
  fresh one: equal `Allocation`s, and no evaluation in the second (the value served is the
  store's: a tampered value is what `const_eval` gives).
- `check_mode_records_a_difference`: a tampered kept value under `Mode::Check` is evaluated and
  the difference recorded.
- `an_unreadable_kept_value_is_an_error`: damaged bytes under the key give an error naming the
  constant, not a re-evaluation.
