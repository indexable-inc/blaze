---
status: plan
summary: blaze v2: minimal invalidation: item hashes and facts.
date: 2026-10-29
---
# blaze v2: items

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 6. Minimal invalidation (Unison, and finer)

Unison's idea: a definition is its content hash; names are metadata pointing at hashes. A
rename or move changes no hash, and a change to one definition is a new hash that dependents
refer to only if they're updated. Rust's semantics need dependents updated when meaning changes,
so v2 takes Unison's identity and adds facts that say exactly when meaning changed.

### 6.1 Items by content

From blaze-next's `blaze-items` (#344), made the key of everything:

- Each fn, type, trait, impl, const, static and macro hashed in normalized form: after
  expansion, resolution and inference, with no names, spans or order, references as hashes of
  what they refer to, cycles as Unison's (a cycle hashed as one unit, members by position).
- Four hashes per item:
  - **interface**: what a user of it can see (a fn's signature and bounds; a type's fields,
    variants and generics; a trait's items);
  - **layout**: size, alignment, field offsets, niche, ABI (types only);
  - **code**: the normalized MIR body;
  - **deep**: the code with every callee's code hash, for what inlines.
- A name, a module path, a file and a position are **not** in any hash unless the program can
  observe them. Rename, move and reformat change nothing the compiler reads, except:
  - **names are observable** through `type_name`, `module_path!`, symbol names (mangling),
    `#[no_mangle]`, `#[export_name]` and `#[link_name]`; a query that reads a name records the
    fact "the name of X" and reruns when it changes;
  - **positions are observable** through `line!()`, `column!()`, `file!()` and
    `#[track_caller]`'s `Location`; these become references into the span side table, filled
    at link time, so a reformat patches values without compiling.
- **What a body leaks into its interface** is part of the interface hash:
  - the hidden type of an `impl Trait` return and of an `async fn`'s future, and whether it is
    `Send`, `Sync`, `Unpin` (auto traits leak through opaque types);
  - a `const fn`'s body, for the constants that evaluate it;
  - a closure's inferred signature and captures, for its users.
- **Layout is deep**: a type's layout hash covers its fields' layouts, niches, `repr` and the
  target, transitively.

### 6.2 Dependencies on facts, not items

A query records the narrowest fact it read, not the item it read it from:

| fact | read by | invalidated when |
|---|---|---|
| signature of fn F | callers' type checking | F's interface hash changes |
| layout of type T | codegen of everything that moves or stores a T | T's layout hash changes, not its methods or docs |
| field f of T: its type and offset | field accesses | that field changes, not the others |
| method resolution of `x.m()` on type T | the caller | an impl or trait adds, removes or changes an `m` reachable for T |
| does T implement Trait | trait solving | an impl of Trait whose simplified self type unifies with T is added, removed or changed |
| name N in scope S resolves to X | name resolution | an item named N appears or disappears in a scope S sees (including through globs) |
| code of F | codegen of F; callers that inlined F (release) | F's code hash changes |
| a const's value | its users | the evaluated value changes, not its expression |

Rules that reach across items get facts of their own, each with edit kinds in the fuzzer
(section 11) aimed at it: coherence and orphan checks, specialization, inference fallback, drop
check, vtable layout, derives, glob imports and shadowing, macro hygiene, auto-trait leakage.

Negative facts ("no impl of Trait for T", "N is not in S") are kept in indexes keyed by
(trait, simplified self type) and (scope, name), so adding an impl or an item touches only the
queries that asked a question it now answers differently.

Early cutoff at every level: each result stores a 128-bit fingerprint. A rerun that produces
the same fingerprint stops there. Fingerprints stay when values are evicted (section 7), so
eviction never breaks cutoff.

### 6.3 Per edit, what reruns

| edit | reruns |
|---|---|
| formatting, comments, whitespace | the span side table of the file and the positions it fills; no code |
| rename or move of an item | the name index, and only queries that read the name (section 6.1); no code otherwise |
| body of F (dev) | F's inference, MIR, codegen; F's blob patched in place; callers too only if F's body leaks into its interface (opaque type, auto traits, `const fn`) and that changed |
| body of F (release) | the same, plus callers whose deep hash includes F |
| signature of F | callers whose resolved call depends on what changed |
| a private field of T | users of that field; codegen of users of T's layout if it changed |
| an impl added | queries whose (trait, self type) or method lookups it now answers |
| a generic fn's body | its instances that are still reached, by their (code hash, type-argument facts) |
| Cargo.toml features | the crates whose cfg set changed, at item granularity |

### 6.4 Monomorphization and sharing

- An instance is keyed by (generic's code hash, the facts of its type arguments it reads), so
  identical instances across crates, branches and projects are compiled once.
- Identical normalized MIR anywhere is one compiled function (code dedupe).

## Issues this lane takes in

- **#906**: a stale sibling closure after a capture change gives silent wrong results. Closure
  captures are a fact of their own (section 6.1), and this repro goes into the oracle's corpus.
  (`v2-items`, `v2-proof`)

## Done when

- every item gets interface, layout, code and deep hashes, names and positions only where observable
- every fact kind of section 6.2 and its negative index exists and is read by its queries
- rename and move keep code; #906's closure-capture case is a fact
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
