---
status: plan
summary: "Facts: what blaze proves about an item is one record, read by the optimizer and codegen through facts_of and covered by keys. Slice 1 (the record, unwind edges and repeated pure calls in MIR, ProgramLifetime's check, Ref32, three lints) is written and unbuilt; the rest is specified."
date: 2026-11-10
---
# Facts: what is proved is one record, and it is spent

blaze proves things about code (a function cannot panic, is pure, cannot allocate; a type lives
until exit) and until now used most of them only to refuse programs. This is the mechanism that
makes each proof pay everywhere, and the rule that it must.

## The audit: who consumes a proved fact today

| consumer | fact | from | keyed by |
|---|---|---|---|
| `hir-ty-codegen/llvm/release.rs:801` | callee nounwind, willreturn, readonly, noreturn | LLVM function-attrs, SCC meet (`blaze-codegen/src/imports.rs:347`) | `Imports::blob_key` (`summary.rs:424`) |
| `release.rs:774`, `:836` | every caller passes one constant; a parameter is never read | `imports.rs:297`, `:451` | `blob_key` |
| `hir-ty-codegen/llvm/lower.rs:1009` | `noalias`/`readonly` of a `&T` (Freeze) or `&mut T` parameter | `mod.rs:6791` | type fact (`hir-ty-share/src/fact.rs:26`) |
| `hir-ty-mir/.../devirt.rs:119` | closed-world dyn facts | codegen's whole-program walk | `Inlined::devirtualized` → `v2/refs.rs:338` |
| `.../escape.rs:343`, `:976` | callee cannot re-enter; std methods that neither grow nor leak | a name list (trusted) | settings and the body |
| `.../inline.rs:509`, `eval/unwind.rs:199` | callee cannot unwind | `fn_can_unwind`: ABI and `#[rustc_nounwind]` only | inlined code |
| `.../cfg.rs:346`, `copy_prop.rs:167`, `box_in_place.rs:204` | no drop glue; Freeze | `needs_drop`, `is_freeze` | type fact |
| `hir-ty-codegen/mod.rs:6202`, `:15397` | `panic=abort`; a cleanup that only resumes | settings; own body | settings; body |
| `hir-ty-codegen/fncache.rs:156` | everything a cached closure reaches is pure | its own walk, not `hir-ty-pure` | code id |
| `hir-ty-identity/src/walk.rs:513` | `#[blaze::pure(trusted)]` | attribute | not keyed, by design |
| `blaze-cargo/.../script/pure.rs:172` | a build script is pure | `hir_ty_pure::script_proof` | the pure key |
| `blaze-lints/src/blaze/effects.rs:140`, `expect.rs` | effects | `hir_ty_pure::check_generic` | diagnostics only |
| `hir-ty-contracts/src/pass.rs:33` | a proved `ensures r < K` | Verus tier (being removed, D40) | no caller |

What it shows: `hir-ty-pure` proves no-panic, no-alloc, no-block, termination and purity for
every function it walks, and no optimizer pass or code generator read any of it. Native code gave
every call an unwind edge unless the crate was `panic=abort`, and `uwtable` to every function
(`lower.rs:635`). Three consumers re-derive a fact privately (LLVM's attributes, `fncache`'s walk,
`escape`'s name list); pattern-type ranges, `&uniq` and `sealed` have no consumer at all.

## The mechanism

A **fact** is one statement about one item, proved by a named analysis. `hir_ty::facts`:

- `Fact`: `NoUnwind`, `NoAlloc`, `NoBlock`, `Returns`, `Pure` of a function; `ProgramLifetime` of
  a type. The tag is its identity in keys and is never reused.
- `Facts`: every fact of one item in three disjoint sets, by how it is established. **inferred**:
  proved from the code alone. **declared**: written on the item and held by a check that refuses
  the item. **trusted**: holds only if a `trusted = "why"` is true; counted, never proved.
- `facts_of(db, item) -> &Facts`: the one query. `hir-ty-pure` computes it (`fn_facts` from the
  one walk's summary, `adt_facts` from the declared checks), tracked, and registers it with
  `inventory` as hir-ty-mir registers MIR.
- `Spend`: a change a pass may make at a call (`UnwindEdge`, `MergeRepeat`, `DeadCall`, `Hoist`),
  each naming every fact it needs. `Facts::license(spend)` gives the digests to record or `None`;
  a pass has no other way to act on a fact. Only inferred facts are spendable: a declared fact's
  check is a lint an `#[allow]` lowers, so it is spent once its check is a hard error.
- Only a `Callee::Direct` is asked: never a `dyn` or function-pointer call, a closure, foreign
  code, a non-Rust ABI (`extern "C-unwind"`), an unresolved trait method or an intrinsic.

This is the extensions lane's `Fact`/`Analysis` (`extensions-1110`, design/extensions.md): a
value per item, keyed by the item and the facts it asked for. blaze's own facts move onto that
fixpoint when it holds store records (its slice 2); `Fact` here then implements `blaze_ext::Fact`.

**The key rule.** A function's code identity includes each fact of each callee its passes relied
on, by identity (`hash(item key, fact tag)`), and nothing of a fact it did not rely on. A pass
records the digests in `MirBody::inlined.relied`; the inliner carries a copy's into the caller;
`hir-ty-identity` hashes them into the inlined code and `v2/refs.rs` lists them into the unit's
code hash. So a callee that starts to panic changes exactly the callers whose code relied on it
not panicking, and proving more of a callee recompiles nothing.

**What the walk's check allows and a fact must not.** A check of a root closes a `dyn` call over
the vtables the root's graph makes. A fact is asked by any caller, which may pass an object made
elsewhere: so `Indirect`, `Open` and `Unchecked` break every fact (`hir-ty-pure/src/facts.rs`).

## Facts and what each licenses

| fact | licenses | precondition | why it is sound |
|---|---|---|---|
| cannot panic (`NoUnwind`) | the call has no unwind edge and no cleanup; `nounwind` on the definition | inferred; direct call; Rust ABI on both sides; the summary has no `Panic`, `Overflow`, `ForeignUnwind`, `Foreign*`, `Indirect`, `Open`, `Unchecked` | an edge never taken and the code only it reaches cannot run |
| pure | `MergeRepeat`: a second call with the same plain arguments right after the first takes its result; `memory(read)`/`readnone` once level 1 of memory effects says which | inferred; scalar arguments and result; nothing between the calls | a pure callee's outcome (value, panic, hang) is a function of its arguments, and the first returned |
| pure + cannot panic + returns | `DeadCall` (an unused call goes), `Hoist` (above its guard, out of a loop) | all three inferred | skipping or adding a run changes nothing observable only if it can neither fail nor hang |
| returns | `willreturn`; a loop with no other effect is dead | inferred | LLVM's definition |
| cannot allocate | escape analysis: an argument cannot be stored in a new heap object | inferred | replaces `escape.rs`'s name list |
| cannot block | nothing in code yet: a fiber may call it without a park point | inferred | scheduler's rule |
| linear (`#[blaze::linear]`) | no drop glue, no drop flags | `hir-ty-linear`'s check as a hard error | every path consumes the value |
| proved contract, pattern type | range facts; a bounds or overflow check removed; `ensures` assumed at callers | the clause is proved (Lean, D40), never merely written | the check's condition is implied |
| `ProgramLifetime` | `Ref32<T>`; no drop glue (true by shape already); later, no refcount | declared and checked | below |
| `&uniq` | `noalias` without the Freeze/Unpin conditions | the borrow checker's own proof | no other pointer exists while it lives |
| devirt's closed world | a `dyn` call made direct | already keyed (`Inlined::devirtualized`) | docs/v2/plan/design/devirt.md |

**Unwind tables.** Removing an edge removes the landing pad and cleanup (code size). The unwind
*table* row of a function can go only when nothing can unwind *through* it either: it is
`NoUnwind` itself and every frame it could be asked to unwind for (a backtrace, a foreign
exception, a debugger, a profiler's stack walk) does not need the row. A backtrace does. So slice
2 sets `nounwind` and keeps `uwtable`; dropping rows is a measured option for release builds
without backtraces, never a default.

## The erasure principle

Proposed AGENTS.md rule (to add in `tools/agents/rules.ts`, not edited here):

> **spend-what-is-proved**: Whatever blaze has proved about code is spent everywhere it is sound:
> a check the proof makes redundant is removed, an edge that cannot be taken is not emitted, an
> attribute the proof justifies is set, in every tier. A proved fact that changes no code is a
> bug with an issue. Every spend goes through `facts_of` and `Facts::license`, so the code's key
> names the fact. Skill: `compiler-correctness`.

**Inferred facts are spent in crates that did not opt in.** Yes. An inferred fact is a property
of the code as written, as much as its type is; spending it changes no program's behavior
(`superset-of-rust`), only its size and speed. What makes it sound across crate and edit
boundaries is the key rule: the caller's code is stored under the facts it relied on, so an
upstream edit that breaks a fact misses the cache. Declared and trusted facts are opt-in by
nature and are not spent until their check cannot be allowed away.

## ProgramLifetime, Ref32, Acyclic

`#[blaze::program_lifetime]` on a type: every instance lives until exit. Two halves:

- **Shape** (`hir_ty_pure::program_lifetime::shape`): no parameter, no `Drop`, every field plain
  data, `&'static`, or itself accepted. This is a function of the definition: reflection can
  state it. It becomes a `const fn` over `type_info` behind a blanket impl of
  `blaze_mem::ProgramLifetime` when reflection phase 2 (`lang-int-1110`) is on main, and the
  front-end walk is deleted (rule 56).
- **Allocation** (lint `blaze::program_lifetime`): every record literal is the argument of
  `ProgramArena::alloc`, a leak, or a frozen `static`'s initializer; not exported unless the
  crate root has `#![blaze::program_lifetime]`. No shape says where a value is built: this
  stays a checked attribute. Missing: tuple-struct constructors; a hand-written `unsafe impl
  ProgramLifetime` without the attribute is not yet refused (needs the attribute to derive the
  impl: ledger E15).

`blaze_mem::Ref32<T>` (`crates/blaze-mem/src/program.rs`): a `NonZeroU32` offset in 8-byte units
into one range `ProgramArena::establish` reserves once (at most 32 GiB); `Copy`, derefs to
`&'static T`, `Option<Ref32<T>>` is 4 bytes; exhaustion is `MemError::Exhausted`. The base is
published once; allocation needs the handle and reading needs a `Ref32`, so the base is an
address, not ambient authority.

`Acyclic` is no attribute and no fact of this crate: the trait is derived from a type's shape
by reflection with a blanket impl, and asserted for one type with `unsafe impl Acyclic for X {}`
([ownership.md](ownership.md)).

## The two lints (group `blaze::edition`, allow)

`could_be_program_lifetime`: a private struct with an accepted shape whose every record literal
is leaked or in a frozen static. `could_be_ref32`: a field (or its collection's element) of a
holder stored many times holding a program-lifetime type by `&'static`, `Arc`, `Rc` or `Box`;
machine-applicable fix for a private `&'static T` field every literal writes from `Ref32::get`.
Not followed (silent): a table in a static filled at run time; that needs a removal analysis on
the holding collection (slice 2).

## What asking a fact costs, and the record that ends it

2026-11-12, lane monowalk-1112. Read from the code and from lane reth's profile of tempo's cold
build (eb3376ba8c on blaze d45e70ddc); the fix below is written and not yet compiled or measured.

**What happened.** `SpendFacts` asks `facts_of(FactItem::Fn(callee))` for each direct callee of
the body it optimizes. `fn_facts` answers from `walk::summary` of that function at its identity
arguments, and a summary asks the summary of every instance its body can run, inside its own
run: the callee's whole closure, depth first, on one query stack, so on one worker. Each summary
read its instance's body from `dispatch::body_of`, the memo the interpreter uses, which keeps one
monomorphized body an instance until the build lets every MIR go. For tempo's last unit that was
one task alone for over 5 minutes, 4,197,786 MIR bodies let go by the release that followed, and
`monomorphized` under `summary_in` as the top allocation site of the `mir` tag.

**Now (written).** A summary reads its body through `dispatch::body_once`: the generic runtime
body's memo (one a body), instantiated into a value the summary owns and drops. What a walk
leaves held is then summaries and generic bodies, by code written, never bodies by instance
(`hir-ty-pure` `tests::held`, with the memo as its control). The memo family stays for the
interpreter, which runs one instance's body many times.

**Next: the closure off the query stack.** The walk over a callee's closure becomes a keyed
worklist on the pool (as the build's walk is, `blaze-codegen` `driver/worklist.rs`): an instance
is an item, its visit summarizes its own body and sends on the instances it reaches that have no
entry, and a second pass in dependency order joins callees' summaries into callers', with the
existing fixpoint for a cycle. Its span carries `done` and `known`. The result is a function of
the instances alone, so it is the same on any pool and any completion order, which its test
shuffles. Not written.

**The end state: a function's facts are a record.** `fn_facts` is per function item at its
identity arguments, and its value follows from the function's code and the code it reaches and
from nothing else: the definition of a value keyed by code identity (rule cache-by-code-id). So
`Facts` is a record beside the function's MIR record (lane records-bodies), under the function's
code identity. A unit that optimizes a body calling a dependency's function then reads that
function's facts from its record and never summarizes its closure; only functions of the crate
being built, and generics whose `Open` kinds an instance decides, are walked, each once, by the
unit that owns them. What the record must hold: the three fact sets, and for each fact the
identity a caller records in `relied`, which is already `hash(item key, fact tag)`. What it must
not hold: a summary's witnesses and places, which are diagnostics and stay a query of the crate
that reports them. Open with records-bodies: the key's form (the same as its MIR record's), and
whether a generic function's `Open` kinds are stored as the list of bounds an instance must
answer, so that an instance's facts are a join of records and never a walk.

## Slices

1. **Written, unbuilt (2026-11-10).** `Facts`/`facts_of`/`license`; `SpendFacts` in MIR
   (`UnwindEdge`, `MergeRepeat`), Release only; keys; `ProgramLifetime` shape and lint; `Ref32`;
   the two lints; the refinement test on a model (`hir-ty-pure/src/facts/model.rs`) with three
   planted bugs.
2. `nounwind` on definitions (`relied_on_own_no_unwind` is the hook; `Defined` in
   `llvm/lower.rs` needs the bit), `Drop` terminators whose glue cannot panic, closures,
   `DeadCall` and `Hoist` in MIR, trait bounds (`#[blaze::expect]` on a trait as a hard error) for
   `dyn` calls, `escape.rs` and `fncache.rs` on `facts_of`.
3. Contracts and pattern types as range facts; linear; `&uniq`.

Language features this waits on, by row (rule 55): reflection phase 2 for the shape trait
(`lang-int-1110`); effects as hard errors, not lints (E7); `&uniq` (E5b).

## Measure

On ripgrep and blaze itself, release, same tree with `SpendFacts` on and off: text size and
`__eh_frame`/LSDA size; count of calls with and without an edge; `mir optimize` self time and
the walk's (`facts_of` runs the summary of every callee: the cost to watch); after a one-line
edit that makes a leaf panic, how many functions recompile (expected: its direct relying
callers, transitively only where the fact was relied on). How often `Overflow` alone blocks
`NoUnwind` in a dev profile.
