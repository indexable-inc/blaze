---
status: plan
summary: blaze v2 codegen lane's design: per-function units, instance keys, templates (the back-end memo up to callee renaming), code dedupe, folding by constants, settings and tiering, release summaries and imports.
date: 2026-10-29
---
# blaze v2: the codegen lane's design

How [codegen.md](v2/plan/lanes/codegen.md) is built, as of 2026-10-29. The crate is `blaze-codegen`
(backend-agnostic, no front end); the backends are `hir-ty-codegen`'s `v2` module (Cranelift,
LLVM) over the code generator that exists.

## The unit is one function instance

A build ([`driver`](../crates/blaze-codegen/src/driver.rs)) starts from its roots (a `main`,
the tests, a library's exported functions) and walks what each instance refers to. Nothing is
grouped by crate: every instance is one task on all cores, and a crate's size never makes a
tail.

For each instance the front end ([`Frontend::describe`](../crates/blaze-codegen/src/backend.rs))
says, without compiling it: its item, its code hash, the content ID of its type arguments'
facts, whose code it is (`plan::Origin`), and everything it refers to in the order of its MIR
(`Described::refs`). The build's policy (`plan::Policy::choose`) gives it a backend and its
`settings::Spec`; `Spec::settings` hashes every field. Those make its `Instance` and
`InstanceKey`.

## Templates: why the store never holds a function's callees

A `CompiledFunction`'s relocations name callees by `InstanceKey`, which holds the callee's
code hash. The caller's key takes the callee by its interface hash. So a callee's body edit
changes the callee's key and not the caller's: anything kept under the caller's key that named
the callee would be stale.

So the store and the memo hold a [`Template`](../crates/blaze-codegen/src/template.rs): the
function with each function, static and outside symbol it refers to by its place in
`Described::refs`, and each of its own data objects by its place among its data. A build
instantiates the template with the keys this build gives the places. A callee edit renames
relocations of its callers, and compiles none of them.

The places come from the front end's walk of the MIR, not from the IR build. A backend that
needs a reference the list lacks fails the unit: the list is what makes templates exact. The
one exception is an outside symbol the backend calls of its own accord, which no MIR names
(LLVM's loop-idiom `memcpy`, `bcmp`, libm, compiler-rt, stack probes): the backend says so in
`Emitted::extra`, and the template keeps it by name (`Template::externs`), since it follows
from the code and the settings alone.

`Target::Function` and `Target::Direct` are written as one kind of instruction (a direct
call, or a direct address); a `Function` target is the callee's jump-table stub, which the
linker owns. Which kind a place is changes no byte of the template.

A function's unwind information is in its template too: the personality routine (or
Windows handler) by place, so it must be among the refs, and the language-specific data
area by its index among the function's data. Its lines (`Line`: code offset to
`SpanRef`) are kept as they are, since spans are position-independent. Both are part of
`code_id` and `Template::id`: functions that unwind differently, or show other lines in a
backtrace, are never one function.

A static's template is kept under its *definition*: the front end's value ID together with
its mutability, thread-locality and `#[link_section]` (`DescribedStatic`), with the
settings. A compiled `Static` that disagrees with what the front end described is an error.

A store entry that is there but does not read is an error naming the unit, never a reason
to compile again.

## The back-end memo up to callee renaming (#1291)

Two instances whose IR, with references by place, is equal compile once: the backend gives
the prepared IR's canonical bytes (`Shape`), and the memo maps `Shape::key` (with the
settings and architecture) to the template. `Vec<A>::drop` and `Vec<B>::drop` differ only in
which `drop_in_place` they call: one shape, one compile, two instantiations. Constants,
layouts and symbols are in the shape, so the sharing is exact. Within one build, the first
instance of a shape compiles and the others wait for its template.

The memo is shared by every worker and every build of the daemon, and a read takes no lock
and writes no shared memory (docs/v2/plan/design/concurrency.md): an append-only
open-addressed table indexed by the shape's BLAKE3 bits, each slot a pointer to an immutable
template published by one compare-and-swap, growing by levels of twice the slots. A hit is a
borrow of the template for as long as the build borrows the memo; collection (`retain`) runs
between builds, with the memo borrowed by nothing.

The store key of an instance's template is its `InstanceKey` (dev) or
`Imports::blob_key` (release), so a warm build builds no IR for an unchanged instance.

## Instance keys

`Instance::args` is the content ID of the type arguments, each written by content: an ADT by
its item hashes, never by name or path, so the same instance is one key across crates,
branches and projects. Where two instances read only facts their type arguments share
(`Vec<A>::push` and `Vec<B>::push` with one layout), the memo above makes them one compile.
Finer keys from the facts a generic's code reads are a refinement of `args` the items lane's
facts allow without changing anything else here.

## Code dedupe

After instantiation, functions whose `content::code_id` is equal are one function: callers'
relocations name one representative (the least key), to a fixpoint, as ICF merges. Never
merged: a function whose address is `Address::Significant` (an exported symbol), any static
(statics are `Target::Static`, by which item they are), and a patchable function of a live
build (each keeps its slot). Rust functions' addresses are not unique (rustc marks them
`unnamed_addr`), so merging the rest changes no program. Data objects are one per content ID.

## Folding functions that differ only in constants (#1292)

On Cranelift units, when `Spec::fold`: shapes with their constants masked are grouped; a group
whose members differ in at most 16 constants and saves more than the thunks cost becomes one
body that takes the differing constants as parameters, and each member a thunk that calls it.
Off by default for LLVM's levels. See [`fold`](../crates/blaze-codegen/src/fold.rs).

## Settings and the plan (#635, #807, #1404)

- wasm32 is not yet a v2 target: a unit for `Arch::Wasm32` is refused with an error naming
  it, never built another way.

- The profile's `opt-level` with cargo's package overrides and `build-override` picks the
  backend per origin; tests are built at the profile's level.
- The standard library's own functions are compiled as rustc ships std (`-O3`, no overflow
  checks, no debug assertions, line tables); a generic of std as the crate instantiating it.
- Only members of a live build are reached through slots; everything else is called directly.
- A live build starts LLVM-level functions at Cranelift and replaces them in their slots when
  LLVM is done; a build whose output is a file compiles each function once at its level, so
  its bytes never depend on timing.
- `Spec::policy` is the policy's content ID: a caller's relocations name callees by keys made
  with the callees' settings, so a change to how anything is compiled is in every key.

## Release: summaries, then imports, then compiles (#899, #176)

1. Every LLVM unit's summary (`summary::Summary`, from its prepared IR after LLVM's pre-link
   pipeline) and export (that IR, as importers link it), stored under its instance key.
2. `imports::decide` over all summaries: which callees each function imports (ThinLTO's
   evolution factor, depth and budget), arguments every caller passes as one constant, callee
   attributes, dead arguments. Only functions whose every caller has a summary get
   whole-program facts. Before deciding, the driver marks external every function that
   something without a summary refers to (a Cranelift unit, a fold thunk, a static's
   value), along with exported, patchable and tiered ones.
3. Each compile with its imports, its template stored under `Imports::blob_key`; the callees
   it built in are `CompiledFunction::inlined`.

## Queries

The codegen query kinds are in [`query`](../crates/blaze-codegen/src/query.rs), in
`blaze_query::kinds::BACK`.
