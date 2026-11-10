---
status: plan
summary: blaze v2 codegen lane's design: per-function units, instance keys, templates (the back-end memo up to callee renaming), code dedupe, folding by constants, settings and tiering, release summaries and imports.
date: 2026-10-29
---
# blaze v2: the codegen lane's design

How [codegen.md](codegen.md) is built, as of 2026-10-29. The crate is `blaze-codegen`
(backend-agnostic, no front end); the backends are `hir-ty-codegen`'s `v2` module (Cranelift,
LLVM) over the code generator that exists.

## The unit is one function instance

A build ([`driver`](../../../../crates/blaze-codegen/src/driver.rs)) starts from its roots (a `main`,
the tests, a library's exported functions) and walks what each instance refers to. Nothing is
grouped by crate: every instance is one task on all cores, and a crate's size never makes a
tail.

For each instance the front end ([`Frontend::describe`](../../../../crates/blaze-codegen/src/backend.rs))
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

So the store and the memo hold a [`Template`](../../../../crates/blaze-codegen/src/template.rs): the
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
relocations name one representative (the least key), to a fixpoint, as ICF merges. The code ID
takes each relocation by what it names, never an address. Every relocation that named a
function merged away is renamed, in code, data, unwind tables and statics' values, and a
renamed static's digest is made again, so the linker places one copy and every reference
reaches it. The fixpoint ends within one round per candidate and one more; past that it is a
`DedupeError` and the build stops. Each round rehashes only the functions it renamed. Never
merged: a function whose address is `Address::Significant` (an exported symbol), any static
(statics are `Target::Static`, by which item they are), a function of a live build reached
through its slot (edited, rebound or tiered: each keeps its slot), and a covered, counting or
instrumented function. Rust
functions' addresses are not unique (rustc marks them `unnamed_addr`), so merging the rest
changes no program. Data objects are one per content ID. The span `codegen dedupe` records the
folded count, rounds and bytes saved. See
[icf.md](icf.md).

## Folding functions that differ only in constants (#1292)

On Cranelift units, when `Spec::fold`: shapes with their constants masked are grouped; a group
whose members differ in at most 16 constants and saves more than the thunks cost becomes one
body that takes the differing constants as parameters, and each member a thunk that calls it.
Off by default for LLVM's levels. See [`fold`](../../../../crates/blaze-codegen/src/fold.rs).

## Settings and the plan (#635, #807, #1404)

- wasm32 is not yet a v2 target: a unit for `Arch::Wasm32` is refused with an error naming
  it, never built another way.

- The profile's `opt-level` with cargo's package overrides and `build-override` picks the
  backend per origin; tests are built at the profile's level.
- The standard library's own functions are compiled as rustc ships std (`-O3`, no overflow
  checks, no debug assertions, line tables); a generic of std as the crate instantiating it.
- In a live build a function is reached through its slot when an edit can replace its code
  (a member's own function, or one an edit fills again under its own key: "Slots and a splice's
  reach" below); everything else is called directly.
- A live build starts LLVM-level functions at Cranelift and replaces them in their slots when
  LLVM is done; a build whose output is a file compiles each function once at its level, so
  its bytes never depend on timing.
- `Spec::policy` is the policy's content ID: a caller's relocations name callees by keys made
  with the callees' settings, so a change to how anything is compiled is in every key.

## Slots and a splice's reach

A splice (an incremental link, `blaze-link`) never moves code the image holds: new code goes to
the patch region (`__blaze_patch`, `Class::Patch`), and a caller of code that moved is written
again where it is. On AArch64 a `bl` reaches 128 MiB, and a splice's thunks are in the patch
region only, so a direct caller kept in place more than 128 MiB from the patch region can reach
neither its moved callee nor a thunk of it. That is `Refusal::NoThunk` and a link from scratch
(found 2026-11-09 on a 750 MiB dev image).

What moved there was not edited code. The edit changed a closure in a member's function; the
closure, a member's code, got a new key and is reached through its slot. But the instances of
`core`'s generics over that closure (`Iterator::any::check::<.., {closure#0}>::{closure#0}`,
`map_try_fold::<..>`) keep their keys: an instance key holds its body's code and what the code
reads of its type arguments, and a closure or function item is a type argument by which one it
is, never by its body. Their *fills* still changed, since a fill names each callee by key and
the closure's key is new. A function filled again under its own key is removed and added, so
its new code is in the patch region, and it was called directly: `Described::editable` (its
body is written in a workspace crate) was the one test for a slot, on the reasoning "no edit
changes its body, and an edit of its arguments' types gives another instance". The second half
is false for a closure or function item argument, and for any callee whose key changes while
the caller's does not.

**The invariant.** In a build whose edited code is reached through slots (`Mode::Live`,
`Mode::Tiered`), every function whose fill an edit can change while its key stays is reached
through its slot by every caller. A splice then never writes a direct branch to code it
replaced: the callers of replaced code hold a branch to a slot's stub, which no splice moves,
and the splice changes the slot's entry.

**The rule** (`blaze-codegen`'s driver, `rebind`; the front end gives the facts). A function is
reached through its slot (`FnNode::slotted`) when it is

1. *edited*: its body is written in a workspace crate (`Described::editable`), as before; or
2. *rebound*: no edit changes its body, but an edit can change which keys its references
   resolve to:
   - one of its type arguments names a workspace crate's type, closure, coroutine or function
     item (`Described::over_editable`, the walk `Owner` already uses, over crates an edit
     reaches). This covers a callee keyed by a fact of the argument this instance's own code
     does not read (`Vec<T>::extend` forwarding to `Vec<T>::reserve` after `T`'s layout
     changed), not only a closure's body; or
   - it refers to an edited function directly (a dependency's function devirtualized to a
     member's impl, the C `main` of a member's `fn main`); or
   - it builds in (`Described::built_in`, at any depth) a rebound function, whose references
     its own fill then holds.

Both are decided once per build from sorted nodes, so the same program gives the same slots.
The call's instruction is the same either way (`Target::Function` and `Target::Direct` are one
kind of place in a template), so no template or instance key changes: the fill's id covers
which target a place names (`content::target_id`), and that is what the link diffs.

**What keeps instances apart.** A function reached through its slot is never merged with
another (`FnNode::external`, the ICF exclusion in [icf.md](icf.md)): two instances over two
closures that compile to equal bytes stay two functions with two slots, so an edit to one
closure changes neither the other's code nor which key the other's callers name. A release
unit imports a rebound function only into a function that is itself reached through a slot
(the importer's fill holds the import's references); an edited function is still never
imported or built in.

**Why not the alternatives.**

| | Code size | Per call | Per edit | Complexity | x86-64 |
|---|---|---|---|---|---|
| 1. Slots for what an edit refills (chosen) | none: every function of a table image already has a slot and stub | the stub: 4 instructions and 2 loads on AArch64 (`adrp, ldr, ldr, br`), 2 instructions and 2 loads on x86-64, for calls to rebound instances only | the edited function and the instances that name it are placed in the patch region; slot entries change; no caller is written | a classification in the driver; nothing new in the linker, loader or runtime | same rule; a `call rel32` past 2 GiB never arises for replaced code |
| 2. Patch islands every N MiB of `__text` | N-th of the code as free executable room, kept in the file's layout | none | a thunk allocated in the nearest island; an island that fills is a link from scratch | address-directed placement (the linker has none: an incremental link only appends to a region), island cursors in the persisted image state, their restore, and reuse rules for memory a running thread may be in | needs the same islands past 2 GiB |
| 3. Slack next to each caller | 12 bytes per call site, or per function | none | as 2 | as 2, per function | as 2 |

1 makes the common case right by construction with no new unsafe code and no new persisted
state. 2 and 3 only make an out-of-reach branch reachable after the fact, and add placement
the linker does not have. They would be the safety net for a direct branch 1 does not cover;
what is left for that net is small (below), so it is not built: `Refusal::NoThunk` stays as
the typed last resort, and the build says which branch took it, which names the function the
rule missed.

**What still reaches `NoThunk`.** An incremental image whose build has no slots in its calls
(`Mode::Output`: a test binary or program that does not run in process) when its code is past
128 MiB from its patch region; and a fill that changes under its key for a reason the rule
does not name, which is then a bug in the rule, reported by name. Both link from scratch and
say so (`Linking::Full(Full::NoThunk)`).

**Release builds** have no slots in their calls (`Choice::patchable` is false in
`Mode::Output`, and no function is edited there), so nothing is rebound and their bytes are
unchanged.

**A running program.** Nothing new is written into running code. A slot's stub loads the
table's address from the table cell and jumps through the slot's entry; a link builds a new
table and the runtime switches every entry at once by storing the new table's address in the
cell (`blaze-live`, `table.rs`), so a thread sees the old function or the new one, and old code
stays where it was for the frames still in it. Rebound functions use exactly that path; the
case removed is the one where a caller's `bl` had to be rewritten in place.

**Measured by** the span `codegen classify` (per unit: `edited`, `over_editable`, `refers`,
`tiered`, `direct`, the count of functions reached each way), `link.thunks` and the link line's
kind. **Checked by** `rebound_functions_are_reached_through_their_slots` (the invariant over
generated programs and edits, with its control) in the driver, and in the linker the oracle of
`thunks.rs` over generated edit sequences on an image larger than a branch's reach, with a
control per planted bug.

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

The codegen query kinds are in [`query`](../../../../crates/blaze-codegen/src/query.rs), in
`blaze_query::kinds::BACK`.
