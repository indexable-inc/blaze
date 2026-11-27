---
status: plan
summary: "Facts: what blaze proves about an item is one record, read by the optimizer and codegen through facts_of and covered by keys. Built: Ref32, ProgramArena and the ProgramLifetime trait in blaze-mem. Not built on the new layer: facts_of, the spends (unwind edges, repeated pure calls), the facts record and the could_be_ref32 lint."
date: 2026-11-10
---
# Facts: what is proved is one record, and it is spent

blaze proves things about code (a function cannot panic, is pure, cannot allocate; a type lives
until exit) and must spend each proof, not only refuse programs with it. This is the mechanism that
makes each proof pay everywhere, and the rule that it must.

## The mechanism

Not built on the new layer: no `facts_of`, `Spend` or `license` exists. (crates/blaze-facts is
another concept: the back end's questions to the front end, backend-next.md D119.) A proof
re-derived privately by one consumer, or proved and read by none, is what this ends.

A **fact** is one statement about one item, proved by a named analysis:

- `Fact`: `NoUnwind`, `NoAlloc`, `NoBlock`, `Returns`, `Pure` of a function; `ProgramLifetime` of
  a type. The tag is its identity in keys and is never reused.
- `Facts`: every fact of one item in three disjoint sets, by how it is established. **inferred**:
  proved from the code alone. **declared**: written on the item and held by a check that refuses
  the item. **trusted**: holds only if a `trusted = "why"` is true; counted, never proved.
- `facts_of(item) -> &Facts`: the one question, answered from the effect walk's summary for a
  function and from the declared checks for a type.
- `Spend`: a change a pass may make at a call (`UnwindEdge`, `MergeRepeat`, `DeadCall`, `Hoist`),
  each naming every fact it needs. `Facts::license(spend)` gives the digests to record or `None`;
  a pass has no other way to act on a fact. Only inferred facts are spendable: a declared fact's
  check is a lint an `#[allow]` lowers, so it is spent once its check is a hard error.
- Only a `Callee::Direct` is asked: never a `dyn` or function-pointer call, a closure, foreign
  code, a non-Rust ABI (`extern "C-unwind"`), an unresolved trait method or an intrinsic.

**The key rule.** A function's code identity includes each fact of each callee its passes relied
on, by identity (`hash(item key, fact tag)`), and nothing of a fact it did not rely on. A pass
records the digests beside the body; the inliner carries a copy's into the caller, and the
caller's code key hashes them. So a callee that starts to panic changes exactly the callers whose code relied on it
not panicking, and proving more of a callee recompiles nothing.

**What the walk's check allows and a fact must not.** A check of a root closes a `dyn` call over
the vtables the root's graph makes. A fact is asked by any caller, which may pass an object made
elsewhere: so `Indirect`, `Open` and `Unchecked` break every fact.

## Facts and what each licenses

| fact | licenses | precondition | why it is sound |
|---|---|---|---|
| cannot panic (`NoUnwind`) | the call has no unwind edge and no cleanup; `nounwind` on the definition | inferred; direct call; Rust ABI on both sides; the summary has no `Panic`, `Overflow`, `ForeignUnwind`, `Foreign*`, `Indirect`, `Open`, `Unchecked` | an edge never taken and the code only it reaches cannot run |
| pure | `MergeRepeat`: a second call with the same plain arguments right after the first takes its result; `memory(read)`/`readnone` once level 1 of memory effects says which | inferred; scalar arguments and result; nothing between the calls | a pure callee's outcome (value, panic, hang) is a function of its arguments, and the first returned |
| pure + cannot panic + returns | `DeadCall` (an unused call goes), `Hoist` (above its guard, out of a loop) | all three inferred | skipping or adding a run changes nothing observable only if it can neither fail nor hang |
| returns | `willreturn`; a loop with no other effect is dead | inferred | LLVM's definition |
| cannot allocate | escape analysis: an argument cannot be stored in a new heap object | inferred | replaces a trusted name list |
| cannot block | nothing in code yet | inferred | scheduler's rule |
| linear (`#[blaze::linear]`) | no drop glue, no drop flags | the linearity check as a hard error | every path consumes the value |
| proved contract, pattern type | range facts; a bounds or overflow check removed; `ensures` assumed at callers | the clause is proved (Lean, D40), never merely written | the check's condition is implied |
| `ProgramLifetime` | `Ref32<T>`; no drop glue (true by shape already); later, no refcount | declared and checked | below |
| `&uniq` | `noalias` without the Freeze/Unpin conditions | the borrow checker's own proof | no other pointer exists while it lives |
| devirt's closed world | a `dyn` call made direct | already keyed (`Inlined::devirtualized`) | docs/v2/plan/design/devirt.md |

**Unwind tables.** Removing an edge removes the landing pad and cleanup (code size). The unwind
*table* row of a function can go only when nothing can unwind *through* it either: it is
`NoUnwind` itself and every frame it could be asked to unwind for (a backtrace, a foreign
exception, a debugger, a profiler's stack walk) does not need the row. A backtrace does. So
`nounwind` is set and `uwtable` kept; dropping rows is a measured option for release builds
without backtraces, never a default.

## The erasure principle

Proposed AGENTS.md rule (to add in `scripts/src/bin/rules.rs`, not edited here):

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

`ProgramLifetime` of a type: every instance lives until exit. Its shape (no parameter, no
`Drop`, every field plain data, `&'static`, or itself accepted) is derived by reflection, and
the attribute is deleted (extensions.md section 6). Today `blaze_mem::ProgramLifetime` is an
`unsafe trait` asserted by hand; the derivation is not built.

`blaze_mem::Ref32<T>` (`crates/blaze-mem/src/program.rs`): a `NonZeroU32` offset in 8-byte units
into one range `ProgramArena::establish` reserves once (at most 32 GiB); `Copy`, derefs to
`&'static T`, `Option<Ref32<T>>` is 4 bytes; exhaustion is `MemError::Exhausted`. The base is
published once; allocation needs the handle and reading needs a `Ref32`, so the base is an
address, not ambient authority.

`Acyclic` is no attribute and no fact of this crate: the trait is derived from a type's shape
by reflection with a blanket impl, and asserted for one type with `unsafe impl Acyclic for X {}`
([ownership.md](ownership.md)).

## The lint (group `blaze::edition`, allow)

`could_be_ref32`: a field (or its collection's element) of a holder stored many times holding a
program-lifetime type by `&'static`, `Arc`, `Rc` or `Box`; machine-applicable fix for a private
`&'static T` field every literal writes from `Ref32::get`. Not built.

## A function's facts are a record

A walk that summarizes a callee's whole closure inside the caller's task (depth first, on one
worker, holding a body per instance) is what a fact must not cost: on the old layer it was one
task of over 5 minutes on tempo. A function's facts follow from its code and the code it reaches
and from nothing else: a value keyed by code identity (rule cache-by-code-id). So `Facts` is a
record beside the function's body record, under the function's code identity. A unit that optimizes a body calling a dependency's function then reads that
function's facts from its record and never summarizes its closure; only functions of the crate
being built, and generics whose `Open` kinds an instance decides, are walked, each once, by the
unit that owns them. What the record must hold: the three fact sets, and for each fact the
identity a caller records in `relied`, which is already `hash(item key, fact tag)`. What it must
not hold: a summary's witnesses and places, which are diagnostics and stay a query of the crate
that reports them. Open: whether a generic function's `Open` kinds are stored as the list of
bounds an instance must answer, so that an instance's facts are a join of records and never a
walk.

## Slices

Built: `Ref32`, `ProgramArena` and the `ProgramLifetime` trait (crates/blaze-mem/src/program.rs).
Not built:

1. `Facts`, `facts_of` and `license`; `UnwindEdge` and `MergeRepeat`, Release only; keys; the
   facts record; a refinement test on a model with three planted bugs.
2. `nounwind` on definitions, `Drop` terminators whose glue cannot panic, closures, `DeadCall`
   and `Hoist`, trait bounds (`#[blaze::expect]` on a trait as a hard error) for `dyn` calls,
   escape analysis on `facts_of`; the `ProgramLifetime` derivation; the lint.
3. Contracts and pattern types as range facts; linear; `&uniq`.

Language features this waits on, by row (rule 55): reflection for the shape trait; effects as
hard errors, not lints (E7); `&uniq` (E5b).

## Measure

On ripgrep and blaze itself, release, same tree with spending on and off: text size and
`__eh_frame`/LSDA size; count of calls with and without an edge; optimize self time and the
facts' cost; after a one-line
edit that makes a leaf panic, how many functions recompile (expected: its direct relying
callers, transitively only where the fact was relied on). How often `Overflow` alone blocks
`NoUnwind` in a dev profile.
