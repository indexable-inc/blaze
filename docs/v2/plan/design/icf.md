---
status: plan
summary: Identical code folding in blaze v2 (lane icf, 2026-11-02) - equal instances are one before codegen (the share key) and one after it (dedupe by code ID, relocations by what they name, to a bounded fixpoint), the linker given one copy with every relocation renamed to it, statics included; what never folds, the guarantee relied on, and how it is measured.
date: 2026-11-02
---
# Identical code folding

Two instances whose code is equal are one function in a blaze build: one compile, one store
entry, one copy in the image, one address. That follows from blaze keying code by content, at
two points:

1. **Before codegen, by key.** `hir_ty_share::share_key` ([share.md](compile-time.md))
   is in `Described::args` (`crates/hir-ty-codegen/v2/frontend.rs`), so instances whose MIR
   reads nothing that tells their arguments apart (`Vec<&A>::push`, `Vec<&B>::push`) have one
   `InstanceKey`. The build compiles that key once, and the content-addressed store
   (`crates/blaze-cas`) holds one entry under it, which every later build, crate and checkout
   hits. `instances_with_one_share_key_are_one` (`crates/hir-ty-codegen/v2/tests.rs`) checks it.
2. **After codegen, by code.** Instances with different keys can still compile to the same
   bytes: `Vec<u32>::push` and `Vec<i32>::push` move four bytes alike, but a scalar is its own
   identity in the share key. `blaze_codegen::dedupe` (`crates/blaze-codegen/src/dedupe.rs`)
   groups a build's functions by `content::code_id` and keeps the least key of each group. The
   code ID hashes each relocation by what it names (an instance's key, a datum's content ID, a
   static's item, an extern's name), never an address, so equal code with equal targets is one
   ID on every machine.

The lane `identity` (the content-keyed kernel's, 2026-11-02; its interface was not yet written
when this was) gives instances a code identity that is stable across checkouts. Lane
identity-port (2026-11-02) moved the share walk into it: `hir_ty_identity::instance_identity` is
now what `Described::args` holds, the share key is deleted, and nothing here changes: folding
before codegen is "one key", whatever the key is made of.

## The fixpoint

A merge renames every relocation that named the function merged away, so two callers that
differed only in which of two equal callees they called become equal, one round later. Dedupe
repeats until a round merges nothing, as lld's and mold's ICF do.

- **Bounded (rule 15).** Every round but the last merges at least one candidate, so
  `candidates + 1` rounds always suffice. A round past that is `DedupeError::NoFixpoint`,
  naming the rounds, the candidates and a pair the last round merged. The driver turns it into
  a `CodegenError` and the build stops. There is no fallback to the unfolded build.
- **Cost.** Each candidate's code ID is hashed once. A later round rehashes only the functions
  its renames changed, then regroups the IDs, which is one hash-map pass over keys. The loop it
  replaced rehashed every function every round.
- **Deterministic (rule 22).** Each group keeps its least key, `merged` is a `BTreeMap`, and the
  functions come back sorted by key, so the result is the same whatever order the build made
  its functions in. `the_result_is_the_same_whatever_the_order`,
  `many_groups_merge_the_same_whatever_the_order` and
  `a_folded_image_is_the_same_bytes_whatever_order_its_pieces_came_in` check it.

## What is renamed

Everything the linker is given names the kept function, so the linker places one copy and
every symbol a relocation reaches resolves to it:

- each function's code relocations, its data's relocations and its unwind personality or
  handler, as before;
- **statics' values (new).** A `static TABLE: [fn() -> u32; 2] = [f::<u32>, f::<i32>]` names
  both instances. Before this change dedupe left the static naming the instance it merged
  away, which no piece defines, and the link failed with an undefined symbol. Now the value's
  relocations and its data's are renamed. Its digest (`digests::static_definition`, which
  `blaze_premap` keys and checks images by) is made again from the renamed value;
- **data, in one forward pass.** A datum's ID is its content, relocations included, so
  renaming one renames whatever points at it. A datum names only data before it
  (`Template::of` refuses others, `TemplateError::DataOrder`), so one pass in list order gives
  every datum its new ID, and a datum that names itself or a later one is
  `DedupeError::DataOrder`. A static's writable datum (`Data::owned`) keeps an ID of its own:
  its old ID and its new content (`RENAMED_OWNED_DOMAIN`), so it stays its static's object and
  is a new object once it points elsewhere (rule 18).

## Why the linker does not fold

The task's brief asked for the linker (`crates/blaze-link`) to emit one copy and point every
symbol at it. It does both, because codegen hands it one copy with every relocation renamed.
The linker itself does not fold, as [linker.md](../../plan.md) says ("It does no code
folding (ICF) and no LTO, which keeps 'what changed' equal to 'what codegen produced anew'").
An incremental link patches by piece. If the linker folded, an edit to one function could
move another function's address, and the linker would have to know `mergeable` (which
functions are exported, patchable or tiered), which is codegen's knowledge. Folding in codegen
also covers the paths that never reach `blaze-link`: the in-process test binary (`exec.rs`)
and the wasm linker, both of which already read `Built::deduped`.

`Def::Alias` in the linker could give a folded key a symbol of its own pointing at the kept
piece. Nothing references a folded key any more, so that would only matter for names (below).

## What never folds

`dedupe`'s `mergeable` (`driver.rs`, `Node::external`) and the candidate filter:

- **A function whose address is significant**, `Address::Significant`: one with a symbol
  (`#[no_mangle]`, `#[export_name]`, an exported `extern "C" fn`, the entry point's callee).
  Another language may compare its address or take it by name.
  `equal_code_is_one_function_unless_its_address_is_significant` (driver) and
  `equal_instances_are_one_address_and_an_exported_one_keeps_its_own` (linker, end to end)
  check it.
- **A patchable function of a live build**, and a tiered one: each keeps its slot, which an
  edit or a tier-up replaces alone.
- **A covered, counting or PGO-instrumented function**: merged, two would share counters.
- **Statics, never.** A static is `Target::Static`, by its item, and is never a candidate.
  `#[used]` applies only to statics, so it is covered. A static's writable data has an ID of
  its own (`Data::owned`), so two statics' equal mutable bytes stay two objects
  (`a_statics_writable_data_is_its_own_and_writable`). Read-only data are one per content ID,
  as rustc's `unnamed_addr` constants are.

## The guarantee relied on

Rust does not guarantee that distinct functions have distinct addresses:

- rustc declares every Rust function `unnamed_addr`, with this comment in
  `rustc_codegen_llvm::declare::declare_fn`: "Function addresses in Rust are never
  significant, allowing functions to be merged". So LLVM's MergeFunctions (on by default in
  optimized builds, `-Z merge-functions=aliases`) and lld's `--icf` may give two functions one
  address.
- The standard library's [`fn` docs](https://doc.rust-lang.org/std/primitive.fn.html):
  "comparing function pointers is unreliable: pointers to the same function can compare
  inequal (because functions are duplicated in multiple codegen units), and pointers to
  different functions can compare equal (since identical functions can be deduplicated within
  a codegen unit)".
- [`std::ptr::fn_addr_eq`](https://doc.rust-lang.org/std/ptr/fn.fn_addr_eq.html): the comparison
  "May return `true` unexpectedly, in cases where functions are equivalent ... This is
  particularly likely with generic functions".

So folding a Rust function whose address Rust code compares is allowed, and the program
behaves as one rustc's optimized build may (rule 4). What the program can rely on still holds:
a pointer to a function equals itself, and two pointers that compare equal call the same code.
The functions whose distinctness something outside Rust may rely on are the exported ones, and
they never fold.

One caveat for later: under CFI (`-Zsanitizer=cfi`/`kcfi`), rustc attaches a type ID to each
function, and folding two with different type IDs would break the check. blaze doesn't build
with CFI. When it does, the type ID goes into `code_id`.

## Measuring

Nothing here was run (lanes don't build). The coordinator runs:

- **Folded count and bytes, from spans (rule 14).** Each build's dedupe is one
  `info_span!(target: "blaze", "codegen dedupe")` with `functions`, `candidates`, `folded`,
  `rounds` and `bytes` (code bytes the image no longer holds). After `blaze build -p
  blaze-engine`:

  ```sql
  select name, fields->>'functions' functions, fields->>'folded' folded, fields->>'rounds' rounds,
         fields->>'bytes' bytes, self_cpu_ns / 1e6 cpu_ms, dur_ns / 1e6 wall_ms
  from '<BLAZE_HOME>/v2/profile/*.spans.parquet' where name = 'codegen dedupe';
  ```

  (Column names as the profile's schema has them; see [profiling.md](../../../profiling.md).)
- **Binary size and build time before and after.** Make two trees from the same checkpoint:
  `after` with this lane's diff and `before` with it reversed (`git apply -R` on a copy, never
  a stash). Build blaze from each, use each blaze to `blaze build -p blaze-engine` from an
  empty target directory, and record the binary's size, the build's wall time and the span
  above. If `before` fails to link with an undefined symbol for an instance, that is the static
  bug this lane fixes. Note the instance and the static.
- **Dedupe's own speed.** `crates/blaze-bench/benches/dedupe.rs`, `dedupe/generic_chains`
  (32,768 functions, nine rounds), paired against the base with blaze-bench's `compare`. The
  base's `dedupe` takes `(functions, mergeable)` and returns `Deduped`, so the base binary is
  that file with the one line in `fold` changed to
  `blaze_codegen::dedupe::dedupe(functions, |it| !exported(it)).merged.len()`.

## Left for later

- **Names of folded instances in symbol tables.** The linker writes one symbol per placed
  function, named by its key's debug name, so a folded instance's name (`Vec<i32>::push`) is
  in no `.symtab`/`LC_SYMTAB`. A backtrace or profiler names the kept one, as with lld's ICF in
  practice. Writing aliases means extra local entries at the kept address in
  `macho.rs` `symbols` (whose `locals_of` assumes one local per function, for in-place
  patching), `elf/dynamic.rs` and the PE writer.
- **A finer share key.** `Vec<u32>::push` and `Vec<i32>::push` are folded after codegen, so a
  cold build compiles both. The share key could read a scalar that is only moved, never
  computed on, as its layout, not its identity. That belongs to `hir-ty-share`.
- **Dependency images.** If a dependency's function folds into a workspace function with a
  smaller key, a dependency static that named it now names the workspace function, and
  `blaze_premap::split` refuses that ("points at ... which is the workspace's"), as it already
  does for dependency code. Whether dev builds can fold across that line, and whether
  `mergeable` should stop at it, is open. Code compiled after the build (`driver/later.rs`, a
  live patch) still names keys merged away. Both predate this lane.
- **Lines.** `code_id` hashes the function's line table, so two *different* source functions
  with equal code never fold in a debug build. That keeps backtraces true. A release build
  without line tables folds them.
