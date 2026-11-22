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

A build (`driver`) starts from its roots (a `main`,
the tests, a library's exported functions) and walks what each instance refers to. Nothing is
grouped by crate: every instance is one task on all cores, and a crate's size never makes a
tail.

For each instance the front end (`Frontend::describe`)
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

So the store and the memo hold a `Template`: the
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
Off by default for LLVM's levels. See `fold`.

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

## The baseline's cost: a stack cell per value (2026-11-16)

`blaze-emit`, the tier that runs the ladder, gives every LIR value its own 16-byte stack cell:
each instruction loads its operands, computes and stores its result. Measured on the Mac
(arm64) by `blaze-emit`'s `the_cell_baseline_against_registers`, `sum(n)` through block
parameters against the same loop written with every value in a register (`cmp; b.hi; add;
add; b`, the floor any register allocator reaches here):

> cell baseline: sum(100000000) code 168 bytes (42 instructions); cells 1.031 ns/iter,
> registers 0.234 ns/iter, ratio 4.41

So a loop-bound program runs about 4.4 times its floor: every iteration is a chain of
store-to-load forwards through the frame, plus the block-argument copies. That is fine for the
ladder and for code run once; it is not fine for workspace code the edit loop runs, which the
design wants at the fast tier from the start.

The smallest fix, in the emitter alone and still one forward pass: **linear-scan register
allocation over the LIR's value order**. Values are numbered in definition order and blocks in
layout order, so a value's live range is one interval from its definition to its last use
(extended to the end of any loop it is used in by its back edge); a pass over the use lists
gives the intervals, a second assigns `x9..x15`, `x19..x28` (callee-saved across calls) and
spills the longest-lived on pressure into the cell it has today. Block parameters get their
registers at the head, so a jump's argument copies become register moves (the parallel-move
order the emitter already has for cells). Expected: this loop at the floor's five
instructions an iteration, code about a third the size. Not built; the stencil tier
(`blaze-stencil`) is the other route to the same floor.

## A `match` by value on an enum that needs drop (2026-11-16)

The rule, from the Reference ("Match expressions", "Destructors", "Temporary scopes"):

- The scrutinee is a place expression. A local named there is matched in place, and is not
  moved. A value expression (`match f() { .. }`) gets a temporary, and the temporary's scope is
  the enclosing statement.
- A by-value binding of a field moves that field out of the place: a partial move. The `_`
  pattern, `..` and a unit variant move nothing. A binding of the whole scrutinee moves all of
  it.
- What is not moved out is dropped when the place's own scope ends, field by field, for the
  live variant only. For a local that is the local's scope end. For a temporary it is the
  statement's end.
- Bindings drop before the temporary, at the end of their arm. An enum with its own `Drop`
  allows no move out (E0509).

So after a `match x { .. }` on a local, what `x` still owns depends on which arm ran and what
that arm moved. That state lasts past the arms' join, to `x`'s scope end, and there is any code
in between.

**Built: drop flags per move path.** A local that some arm partially moves gets one 64-bit word,
one bit per field of all its variants:

- the bit is set at the `let` and at every assignment;
- a moving arm clears its fields' bits, with one `and`;
- every drop of `x` (its scope end, a `break`, `continue` or `return`, an assignment over it)
  switches on the discriminant and drops each field of the live variant whose bit is set.

A whole move uses the flag the binding already has.

A temporary is elaborated per arm, with no flag. The arm knows its variant and what it moved,
statically. The remainder joins the arm's drop scope ahead of its bindings, so it drops after
them at the arm's end, and on any early exit from the arm. The arm's end is the statement's end
when the `match` is a statement's whole expression, a `let`'s initializer, an arm's body, or a
block's tail in edition 2024 (where tail temporaries drop before the block's locals). Anywhere
else, a later operand would run before the temporary's drop, so that case is refused by name.

Why flags and not per-arm elaboration for a local: per-arm elaboration has to copy everything
after the match up to `x`'s scope end once per arm state. Sequential matches multiply those
copies; flags grow linearly. Flags are also the floor here:

- one store at the `let`;
- one `and` per moving arm;
- one load, `and` and branch per droppable field at the drop.

A pass that knows a flag's value at a drop (one arm, or no moves on any path) folds it to the
static form, which is the temporary's case. Static elaboration where the state is known and a
flag where it is not is the end state; only that fold is left to build.

Refused by name, each with its next step:

- a by-value pattern below a field that needs drop (`A((a, _))`, a nested variant pattern): a
  move path deeper than one field;
- a guard on an arm that moves: Rust binds by reference for the guard and moves only after it
  passes;
- an enum with more than 64 fields across its variants: a second flag word.

Unwinding is not built. blaze-build has no landing pads, and a panic calls the panic lang fn,
which never returns, so no drop runs: `panic = "abort"`'s semantics. A panic in an arm therefore
drops nothing, as in Rust built with abort. A program that needs unwinding (a `catch_unwind`
that must catch) is a refusal for the unwinding lane, not for the match.

## Trait objects: vtables and dynamic calls

**A wide pointer** (`&dyn Tr`, `*const dyn Tr`, `Box<dyn Tr>`, `Rc`, `Arc`) is two words: the
data address at 0 and the vtable address at 8. `dyn Tr` itself is unsized.

**The vtable** is an array of 8-byte words:

- word 0: `drop_in_place::<T>`, or 0 when `T` needs no drop;
- word 1: the size; word 2: the alignment;
- then the methods.

Method order is a function of the traits' text alone, so two builders agree. Walk the
supertraits depth first in their declaration order, the principal last. Each trait contributes
its methods in declaration order, and a trait reached twice contributes them only once. A
method with `where Self: Sized`, or with generics, gets no slot.

So the first supertrait chain is a prefix of the table, and upcasting along it is free: the
pointer stays the same. Every other supertrait `S` gets one word, right after `S`'s own methods,
holding the address of `(T, S)`'s vtable. Upcasting to `S` is one load.

The alternatives cost more. Pointers to every supertrait table make every inherited call two
loads. Copying every supertrait's whole table inline makes the code bigger and still needs the
pointers for a diamond.

Adding a method changes the vtables of that trait and the traits below it, nothing else. Auto
traits and `ExProj` bindings add no slot: `dyn Tr + Send` uses `dyn Tr`'s table.

**Identity.** One data record per `(T, principal trait with its arguments)` per program. It is
keyed by those two ground types, so builders share it by content across units. Its method words
are relocations to instance keys (`DataTarget::Fn`), never addresses. An edit to one impl method
therefore rebuilds that function alone. The vtable's bytes and key do not change, and the
patcher retargets the instance's entry.

The key also covers the principal's *shape*: the world's hash of the method lists and
supertraits the table reads (`vtable::ObjectTrait::shape`). Without it, an added method would
put two tables under one key (rule 21). A method appended to a trait changes the keys of that
trait's tables and of those below it. A method inserted in the middle also moves the slot of
every method after it, in those same tables. Built in crates/blaze-build/src/vtable.rs
(`Shape::of`, `Vtable::build`, `Vtable::record`, `vtable_key`), with tests in vtable/tests.rs.
Layout is in layout.rs (`Tail::Dyn`). `size_of_val` and `align_of_val` are in intrinsics.rs.

**Checks the builder stands in for.** The rebuilt path has no move checker or borrow checker.
Until one exists on the item layer, the builder's refusals are the only stop for E0509 (a move
out of a type that implements `Drop`, `a_match_by_value_refuses_drop_impl_enums_and_wide_enums`).

**A dynamic call** (the trait's method with `Self`, its first recorded argument, the `Dynamic`
type: the checker records no other mark) is one load
and one indirect call: `f = load [vtable + 8 * (3 + slot)]`, then `f(data, args..)`. The vtable
address is already in a register, as the wide pointer's second word. This is the floor.

For every dispatchable receiver (`&self`, `&mut self`, `Box<Self>`, `Rc<Self>`, `Arc<Self>`,
`Pin<P>` of these), the callee receives the data word: the receiver's thin form, which is
`DispatchFromDyn`'s rule.

**Unsizing** (`&T -> &dyn Tr`, raw pointers, and `Box`/`Rc`/`Arc` through `CoerceUnsized`)
pairs the thin pointer with the address of `(T, Tr)`'s vtable. That happens at the coercion the
checker records. `Box` waits until the checker records `CoerceUnsized`.

**Through the object:** `size_of_val` loads word 1 and `align_of_val` word 2. A drop calls word
0 on the data address when the word is not 0. A `Box` then frees with words 1 and 2.

**What a function's key holds of a trait** (built: blaze-build `object_reads`, `vtable::record_key`).
A dynamic call's code holds its slot, and an upcast off the first supertrait chain holds its word's
offset; both are functions of the principal's shape, which the world gives before the build, so the
key holds the finer fact, not the shape hash: (principal, method, slot) per dynamic call and (from,
to, word or "prefix") per upcast, in expression order. Adding a method at the end of a trait then rekeys no
caller of an existing method of that trait, only callers whose slot moved (a subtrait's own methods,
which follow the supertrait's words); the whole shape hash would rekey every caller. An unsizing to
`dyn Tr` holds nothing of the shape: its table is a relocation by (concrete, principal, args), which the
link resolves to today's vtable key. `size_of_val`, `align_of_val` and drop through an object read the
fixed words 0 to 2. The cost of the finer form is one pass over the checked body before the build.

## Closures and the Fn traits (2026-11-16)

State after runner 3 (supersedes the survey below where they disagree; details in the ladder
README "Code builder: closures"): values, environments, direct calls, bodies by kind, the thin
`call_mut`/`call_once` entries, calls through a bound's trait (the checker's callee row wins over
the closure's kind: `closure.rs through_fn_trait`), by-move captures with drop glue, and
`ClosureFnPointer` (the closure's `call_once`, zero-sized `self`) are built and run natively.
Open: SHIM instances of `Fn*` methods at a fn item or fn pointer `self`, and a closure called in
a body other than its owner's.

What exists in blaze-build (walk.rs, by search; the strings are the refusals each form hits):
- A closure VALUE: not built. `repr` refuses its type (`TyErr::Unsupported("a closure")`) and the walk
  its expression (`Expr::Closure` -> "a closure"). No environment struct, no closure body fn.
- A DIRECT call of a closure value: refused by name, "a direct call of a closure".
- A call of a fn POINTER: built (`call_pointer`, `call_indirect`); since today a "rust-call" pointer too.
- A call through `Fn*` on a type parameter (`f(x)`, `F: Fn(u8) -> u8`): the checker hands a callee row
  `Fn::call` (or `call_mut`/`call_once`) at `[F, (A,)]` and a `Borrow` adjustment on the callee
  (typeck rules/call.rs `overloaded_call`); the arguments stay written apart. Built since today as a
  call of that instance with the callee as `self` (walk.rs `call`, `overloaded`). NOT natively tested:
  the world must resolve `Fn::call` at a concrete `F` (an impl, or a built-in shim: below).
- The closure's kind is not in `Checked`: the checker derives it from `captures` (strongest of
  `ByRef`/`ByMutRef`/`ByValue` -> `Fn`/`FnMut`/`FnOnce`, rules/closure.rs `analyze_closures`); the
  builder must read it the same way or the checker must record it (one home: ask the checker).
- `PNode::Closure(ExprId, sig)` names no owner item or parent arguments: as a ground type it cannot
  key an instance across bodies. A closure's instance key needs (owner's code id, closure expr's
  item-relative id, the owner instance's args): a checker/portable-type change, owed before shims.

blaze's "rust-call" convention (rust-call is the compiler's own, not a stable ABI; core's
library/core/src/ops/function.rs: `extern "rust-call" fn call_once(self, args: Args) -> Self::Output`):
the last parameter, a tuple, crosses the call as its FIELDS, each a Rust-ABI parameter of its own,
after the others (`self` first). A zero-sized tuple is nothing. The callee puts the tuple back in a slot
of its own; a caller that wrote the tuple whole (`(**self).call(args)`) spreads it, field by field from
its copy (an aggregate field by its address inside that copy); an overloaded call `f(a, b)` passes `a`
and `b` as they are. So a call through the trait and a direct call of the same body agree, and a
closure's body fn can take `(env, a, b)` with no repacking. Built: lib.rs `Abi::RustCall`, walk.rs
`rust_pass`, `rust_call_fields`, `spread_tuple`, `run` (callee), `call` and `call_pointer` (callers).
Test `a_rust_call_fn_takes_its_tuples_fields_as_parameters` (native 75321); with the caller's spread
removed it fails: "the caller spreads too ... sig s1 = c (i32, ptr) -> (i32)".

Not built, in order: (1) a closure's environment as a struct of its captures in capture order (`&T`,
`&mut T` or `T` by `CaptureKind`), its body as a fn `(env by kind, args..)` of the rust-call shape;
(2) the shims per instance, `Kind::Shim`: `call_once` on a `Fn`/`FnMut` closure borrows from the moved
environment then drops it; a fn ITEM's shim calls the item (self is zero-sized); a fn POINTER's makes an
indirect call; `impl<A, F: Fn<A>> Fn<A> for &F` and siblings are ordinary library impls; (3)
`ClosureFnPointer`: a fn of the pointer's signature whose body is the closure's with an empty environment.

The design (2026-11-16, built in blaze-build src/closure.rs, a child module of walk.rs):
- ENVIRONMENT: a tuple with one field per capture in the checker's order (`Checked::captures`, sorted
  by binding): `&T` for `ByRef`, `&mut T` for `ByMutRef`, `T` for `ByValue`; laid out by the walk's own
  tuple layout (zero-sized when nothing is captured); its drop glue is the tuple's.
- GROUND FORM: the checker's `Closure(e, sig)` names no owner and no captures, so the builder grounds it
  self-describing: `Closure(e, (sig, env, FnDef(owner, owner_args), kind))`, `kind` a `u8` const value
  (0 `Fn`, 1 `FnMut`, 2 `FnOnce`). Any body (a generic `apply<F>` at `F` = it) lays it out from the type
  alone, and its instance key (owner, item-relative `e`, the owner's ground arguments) is unique across
  bodies: the identity gap closed on the builder's side, as the brief's step 5 keys it.
- KIND: the strongest capture (`ByRef` -> `Fn`, `ByMutRef` -> `FnMut`, `ByValue` -> `FnOnce`), the
  checker's own rule (rules/closure.rs `analyze_closures`), until it records the kind.
- BODY: the instance of the kind's own method (`Fn::call` / `FnMut::call_mut` / `FnOnce::call_once`) at
  `[C, (A..)]`: `(&env | &mut env | env, a..)`, the rust-call shape, so a direct call `f(x)` and a call
  through the trait reach the same code. Inside it a captured binding lives at its field (through the
  field's pointer for a by-reference capture); by-value captures are owned by the body and dropped there.
- ENTRIES: the weaker kinds' methods are thin instances of their own key over the body: `call_mut` on an
  `Fn` closure passes its pointer on; `call_once` on an `Fn`/`FnMut` closure takes the environment by
  value, calls the body with its address and drops it after.
- IDENTITY (11:40, supersedes GROUND FORM and KIND above): the ground type is the checker's
  `Closure { owner, ordinal, args }`; kind, signature and environment are FACTS asked through
  `LayoutFacts::closure(owner, ordinal)`, made from the owner's record by
  `layout::closure_facts`, grounded at `args` as an ADT's fields are. No expression id leaves
  one body's walk. Entries built (`closure.rs run_entry`); state in the ladder README, "Code
  builder: closures".
- STATE (11:15): environment, ground form, value, direct call and body built; native tests in
  blaze-build src/tests/closures.rs (3, 42, 1313). Not built yet: the entries, the generic `apply<F>` test,
  the fn item / fn pointer shims, `ClosureFnPointer` (ladder README "Code builder: closures").

## Queries

The codegen query kinds are in `query`, in
`blaze_query::kinds::BACK`.
