---
status: plan
summary: The item layer and the commands above it, rebuilt from scratch, one consolidated spec. Pull at item grain, except names, whose per-module records are filled crate-wide the first time and updated per module after; a task never waits (it returns Need(keys) and is entered again); a dependency is only its records; the type checker asks through Items and Source. run, test and check (export later) run code records in the program's own process. Zero third-party crates, no std or alloc in blaze's own code, with dated exceptions. Floors with ratios, open questions and the docs it supersedes.
date: 2026-11-15
---
# The bottom layer, from scratch

Andrew, 2026-11-15: "we can scrap everything imo and build the lints FOR this new design ...
delete old code and just start from scratch"; "start with the right design and then iterate ...
iterate fast".

**Why.** The inherited item layer (hir-def, hir-expand, base-db: 189k lines) resolved a crate
whole, named items by arena slots, kept syntax trees alive, and let any question about any item
fall into source text. On 2026-11-15 a cold `check --no-daemon` of hello-world cost 21 to 45 s
and 3.4 to 4.6 GiB, because every item of `core`, `alloc` and `std` was resolved to answer
questions about a few hundred of them. That layer was deleted on 2026-11-15 (#2197); the type
checker was parked, not deleted.

Each decision is stated once, in its final form; detail lives in [identity.md](identity.md),
[run.md](run.md), [typeck.md](typeck.md), [reconnect.md](reconnect.md),
[std.md](std.md#two-builders), [third-party.md](../third-party.md) and [target.md](target.md).

## 1. Goal order

The statement the design is judged by, Andrew, 2026-11-15 19:53: "assume you want best
performance on 128 core machines (say graviton 5 or epyc) but a lot of people will be using on
typical macs too. we want to have fast incremental compilation for realistic huge rust monorepos
with millions of lines of code. needs to work with enormous crates. SoTa at least, structure of
arrays etc. incremental comp being short (including on large crates and large files in crates
even after restart computer so nothing running etc) = lints being fast > being able to get cached
stuff remote (r2/peer to peer etc) > memory usage > cold comp > disk gc being good/not locking >
disk usage. also need incremental test (only rerun ones that changed)".

The order, the earlier winning when two trade: (1) incremental compile short, large crates and
files and after a restart included, with lints as fast; (2) records from remote; (3) memory;
(4) cold compile; (5) disk cleanup that never blocks; (6) disk use. Replaced: the five-item order
of the first quote, because the later statements add remote as second. What each clause asks:

- **128 cores, and typical Macs**: no global lock or single-threaded stage on a hot path:
  interners read without a lock, caches sharded, a task's scratch memory its own, the first
  crate-wide fill of names parallel across module groups (section 6); one code path sized by the
  machine. Relax is what not to do here: it measured one mutex taking 20% of an app's cycles
  spinning, and its collection and import fixpoint are single-threaded.
- **Enormous crates and files**: nothing is recomputed per crate on an edit; the per-crate
  records (section 4) are lookups valid by per-module digests. A large file is re-outlined whole
  on an edit, at about 1 microsecond a line (measured): 30 ms for 30k lines; open: re-outline only
  the changed item's range (section 5).
- **Incremental test**: a test's result is a record whose read list is the code identities its
  run entered. After an edit the tests run again are exactly those whose list holds a changed
  function, plus tests with no record; every other test's stored result is reported, printed with
  its verdict and marked as from its record (not run again). Floor: one postings lookup per changed
  function plus the run of the affected tests; with nothing changed, the certificate alone.
  Mechanism: [run.md](run.md) (`Tests`).

- **After a restart** (no process, page cache cold): a repeat check reads one certificate and,
  when it stands, nothing else; records are packed with an index, never a file per record; the
  sweep is by stat; no source is re-hashed that a stat proves unchanged (sections 9, 10). **SoTA,
  structure of arrays**: columns everywhere (section 13).
- **Lints**: a task keyed from the facts it reads of one item; none walks a crate (section 12).
- **Remote**: a record holds nothing of the machine, so any builder's record is any reader's;
  never a request per item: a pack's index once, then byte ranges in parallel (section 10).
- **Memory**: records read in place; a task holds only what it pulled; the page cache is the only
  cache. **Cold**: std and public dependencies arrive as CI's records. **Cleanup, disk**:
  immutable records; collection is compaction and unlink, never a lock.

## 2. Principles

1. **Pull, at item grain.** The driver asks one question (check this body; run this program).
   Answering it asks for records; a record that exists is read in place; one that does not is
   made by its own task. Nothing is computed that no one asked for, with one stated exception:
   names (section 6), whose per-module records are filled crate-wide the first time a crate is
   seen. Replaced: "a crate is never resolved whole", because the producer decision of 18:37
   (section 6) resolves a new crate's modules together once.
2. **A task never waits** (Andrew: "why not explicit state machine sans io"). Logic whose states
   are the design is a hand-written sans-IO machine (`step(event) -> effects`; rule 14): name
   resolution, the run, the drivers. A compiler task that is recursive code (the type checker
   asks for a callee's signature in the middle of an expression) is not turned inside out and
   does not park: it is a pure function of its inputs that returns its result or `Need(keys)`.
   The driver gets those records and enters the task again from the top; what it read is a
   record, so re-entry is cheap; with no stack to keep, it can run on another machine. Replaced:
   "made by one small task, which asks the same way" and "two askers share one task (the store's
   flight)", because a task that asks and waits parks; the flight lives in the driver (section 9).
3. **A body's needs are remembered.** A checked body's record lists what its check needed; the
   driver has those before it enters the task the next time, so the usual check is one pass with
   no re-entry.
4. **A dependency is only its records.** A missing record of another crate is made by that
   crate's own task (which has its text) or is a typed stop naming the fact and the item; never a
   parse by the asker.
5. **Two traits** are everything the type checker may ask (blaze-surface):
   - `Items`: any crate; every method reads records; no method can reach text.
   - `Source: Items`: the crate a task compiles; its text methods take a `HasText` proof.
   A stage's entry point takes `&impl Items` or `&impl Source`; helpers behind it stay on the
   concrete database. Each question by `ItemKey` has a form by `Local`, the task's own number.
6. **A fork shares by content.** Two checkouts with equal text for an item find equal records.
7. **Decide at run time** (section 11): what a flag fixes elsewhere is a version in a slot here.

## 3. Identity and keys

- **Identity is `blaze_id::ItemKey`** ([identity.md](identity.md)): the hash of what the item is
  in (its module, impl, trait, enum or body), its kind, its name and where it is written (an
  index among equals, `Item::disambiguator`; a macro-made item adds the call's place). Bytes 0..8
  are its crate key's first 8; the top bit of byte 8 marks an item inside a body.
- **A rename is a new item**, whose work is reused by content: a body's key writes its own item
  as "self". No rename detection. A task's `Local` number is not `Portable`.
- **The crate key** (`CrateItemKey`) is the prefix of every item key of the crate. It never holds
  cfg, features or the target, so an item has one key on every target and feature set. Open:
  whether edition, crate attributes and dependencies belong in it (an edit of `#![..]` would then
  move every item key) or are read-list facts of `CrateRoot`. The code today hashes all of
  them, cfg included (`blaze_id::CrateItemKey`'s doc): to change.
- **A key is what the record is; a read list is what it read.** Every record has two columns,
  as in [run.md](run.md) and [target.md](target.md): the key, computed from its own text, the
  tool (blaze and its pinned nightly) and the namespace before the work; and the read list, the
  (fact, digest) pairs the work asked, absences included. A key never holds another record's
  key; a fact of another record is a read. Replaced: the first table's "key from" column, which
  held "the module's `FileItems`", "its own `ModuleDecls`" and "the keys its paths resolve to",
  because those are reads, and a key made of them cascades (read-addressed.md).
- **cfg, features, target**: each record's read list holds the cfg outcomes it read (as
  `ReadKind::Cfg` in blaze-expand); target layout (sizes, alignments, ABI) is in code keys only.
- **Positions** are item-relative in every record (rule 71). Only `FileItems` holds
  file-relative positions (an item's start), and it is keyed by the file's bytes, so a position
  moves exactly when its key does. `file:line:col` exists only when printed (section 12). Nothing
  is ordered by a local number; a sort for output or for a key is by item key.

## 4. The records

`K` is the key, `R` the read list. Crates: surface = blaze-surface, scope = blaze-scope, expand
= blaze-expand, impls = blaze-impls, typeck = blaze-typeck, run = blaze-run.

| Record | K (before the work) | R (validity) | Holds | Crate, state |
|---|---|---|---|---|
| `FileItems` | the file's bytes, edition, outline version | none | items in order: kind, name, disambiguator, content and signature hashes, item-relative ranges of signature, body and attributes; `mod`/`use`/macro-call entries. No tree | surface; built (from blaze-file-items' `Outline`) |
| `ModuleDecls` | the module's place in its file's text, the file's content, the edition | the cfg outcomes it read | names the module declares, by namespace; an item under an attribute macro is not a name until it expands | surface; built |
| `ModuleFile` | the `mod` item | negative reads of `foo.rs` and `foo/mod.rs` | which file, relative to the crate root | surface; built |
| `TextualEntry` | the module | the parent's entry, earlier `#[macro_use]` siblings | the `macro_rules!` scope at the `mod` line | surface; built |
| `CrateRoot` | the crate key, the root file's content | cfg outcomes | edition, `no_std`, preludes, extern prelude (with `extern crate self as`), `register_tool` | surface; built |
| `ModuleScope` | the module's key | its own `ModuleDecls`; per-name reads: `(module, ns, name)`, `Exports` digests, textual entries, derive helpers | bindings by (namespace, name): target key, visibility, origin, state; an `Exports` digest per visibility class | surface (form), scope (maker); built, 16/2 tests on 2026-11-15 |
| `KeyIndex` page | the module's key | its `ModuleDecls` and expansions | item key to place: `resolve` is one probe | surface; built |
| `MacroDef` | the definition's content (`MacroContent`), never its item key | none | kind, tokens, edition, flags, a derive's helpers, transparency | surface, expand; built |
| `Expansion` | the call's tokens, the definition's content, the call-site hygiene | env, cfg, included files, positions inside eager input | tokens with origins, hygiene contexts by structure, `Hole`s for `line!()` and the like outside eager input | expand; written, tests not compiling on 2026-11-15 |
| `Signature`, `Fields`, `Variants`, `AssocItems`, `Predicates`, `Supertraits`, `TypeAttrs` | the item's signature text (`signature_hash`) | what its paths resolve to, and each named item's key and signature hash (D51) | the lowered signature (types by item key), split so each is cut off on its own; `repr`, `non_exhaustive`, `fundamental`, stability, `#![feature]` | not built |
| `ImplHeader` | the impl's header text | resolutions of its paths | trait ref, self type, generics | not built |
| Impl directory and bucket | the crate key; the bucket key (trait for a self shape, blanket, inherent of a shape) | each module's impl digest (below) | the impls of one bucket, sorted by impl key | impls; built |
| `NoImplsOf`, `LangItems` | the crate key | each module's impl digest and `#[lang]` digest, from its `ModuleDecls` and expansions | "crate C has no impl of T"; lang items by name | impls and surface (duplicated); built from a list the caller hands |
| `CrateGraph` | the lock file and workspace manifests by content | none | crates by key, dependencies by name | impls and surface (duplicated); built |
| `BodyFacts` | the body's text | the signatures it read | what a body gives others: hidden types, const values, discriminants | not built |
| Checked body | the body's text, with its own item as "self" | the `World` reads, sealed (typeck.md) | types, adjustments, callees, captures, diagnostics; `needs` for next time | typeck; in writing |
| Finding | as its check's | as its check's | one lint or error, item-relative range, no level | not built |
| Fast code, optimized code, program manifest, test result | see [run.md](run.md) | see run.md | code records and what a run reached | run; machines built, drivers not |
| Certificate | the tool, the request's shape | the request's reads (section 9) | the answer: findings' and manifest's keys | not built on the new layer |

Replaced: `NoImplsOf`, `LangItems` and `CrateGraph` keyed by "the crate's roots", because that key
misses the impls and `#[lang]` items written anywhere in the crate (rule 21). Replaced: "nothing
is keyed per crate except its file list and root module": per-crate records exist where a lookup
must rule a crate out in one read (`NoImplsOf`, the impl directory, `LangItems`), each valid by
per-module digests.

Each record is one blaze-format buffer, written once in the caller's allocator, read in place by a
view that checks it once; two builders write equal bytes (rule 25; lists sorted at `finish`).

## 5. Files

- **The outline** (`blaze_file_items::outline`): text to items in one pass over the parser's
  events, no tree kept, in the caller's allocator; every item at module-level nesting; items in a
  function body belong to the body's record (the entry says whether it holds any). Section 17.
- **`FileItems`** is built from an outline and keyed by the file's bytes. Taken from Relax: packed
  tokens (4 bytes each) and LEB128 position tables for `FileItems` and expansions.
- **Bodies skipped**: a body is parsed when its own task runs (today the outline parses bodies in
  full: to build). **Not read yet**: 9 syntax errors in `core` and 1,531 in blaze's crates are
  syntax of blaze's extensions (the outline parses with none): to fix.
- **A large file** (section 1). Open: re-outline from the item boundary before the edit, reusing
  entries whose bytes did not move. Test: outline the largest files of reth and tempo.

## 6. Names

**For a successor** (the names lead, 2026-11-16, handing step 5 on).

Latest state (scope lead 18, 2026-11-16; runs `port27/scope-lead-18-*.txt`; steps 1 and 2 done):
- `fellows` left the record (`ModuleScope` form 3). Reads between group members are ordinary
  read-list entries: `finish.rs` `settle_reads` runs each member's final pass alone, its fellows
  answered from the group's finished scopes; a different scope is `ResolveError::Unsettled`.
  Textual digests have one home, `blaze_surface::textual_digest`.
- Value-compared read lists cannot see circular justification (`c: pub fn x; p: pub use c::x;
  q: pub use p::x`, edit `p` to `pub use q::x`: kept records keep `c::x`, a cold fill leaves both
  unresolved). `again.rs` (`Again`, `Touched`) closes it: each wave is widened by the kept records
  that read what the rerun may derive anew and that the rerun reads, transitively, solved again
  from text with it. The test driver and `pipeline --edit` both use it.
- Tests: 48 pass, among them `an_edit_under_any_schedule_gives_the_cold_records` (200 tapes, two
  solvers), `an_edit_that_closes_an_import_cycle_gives_the_cold_records`,
  `a_new_name_in_a_cycle_reruns_its_module_alone`, and the controls `fellow_reads_left_out_are_caught`
  and the `Plant::KeepSuspects` half of the cycle test.
- Edit rows (`scope-lead-18-ruleC-edit.txt`, 1 thread), before (`scope-lead-18-base-edit.txt`)
  then after: (c) pub fn in a glob source 8 records, 42.7 ms, then 1 record, 12.3 ms; (d) pub use
  inside the 192 group 192 records, 38.5 ms, not equal to cold, then 1 record (0 suspects),
  2.6 ms, equal. All six rows equal a cold fill. Floor for (d): the one edited record (nobody
  read the new name), ratio 1.0 in records.
- Cold cost of settling: stage 4 1,572 ms, then 1,790 ms; allocated 999.5 MB, then 1,080 MB;
  peak live 40.3 MB, then 43.0 MB; the same 12 unresolved, no `Unsettled`. Cold is about 1,800
  times its floor (16k events, about 1 ms); step 3 (counters) is what targets it.

**The cell core (2026-11-16; beside the old solver in `crates/blaze-scope/src/cells/`).**
- A CELL is (module, namespace, name, layer), made the first time it is asked for, in an
  ordered table. Layer `Full` is what a path reads; layer `Globs` is the same name through the
  module's globs alone (what a named import of that very name reads, since an import never
  justifies itself). Full's inputs: the module's explicit entry (an item, a child module), its
  named imports that bind the name, then its Globs cell. Globs' inputs: one GLOB EDGE per
  resolved glob (the source's Full cell, filtered by visibility from here, narrowed by the
  glob's visibility) and the GROWTH: the module's globs not yet resolved (stage 2 adds the
  unexpanded calls).
- A cell is DECIDED ONCE AND FINALLY: an explicit entry is Found at once (it shadows globs,
  R1); a named import of the name decides it when the import is decided; a Globs cell decides
  only when its growth is zero and every source is decided: no candidate NotFound, one item
  Found, two items Ambiguous (R2). Nothing is lowered or re-queued: there is no relax round.
- An ITEM (a named import, a glob's path) is a task run from the top over FINAL reads; reading an
  undecided cell leaves one waiter record and stops the item. A decided cell wakes its waiters
  (items and cells). A lookup reads ONE cell; it never walks globs.
- At a quiet point the graph of UNDECIDED nodes (cells, items) is condensed by an iterative
  Tarjan pass (no recursion). A sink component of cells alone is a legal glob cycle (R3): its
  cells are decided together as the least fixed point of their candidates. A sink component
  with items is a true stall (R5): its items fail as not found, which ends the growth they held;
  the loop goes on.
- Order: the worklist is two ordered sets (cells by key, items by (module, index)); every table
  iterated is ordered; so the outcome and every counter are the same for any push order.
- Counters: cells made, decisions, waiter records, wakes, evaluations per item (mean, max),
  cycle passes and their sizes, stalls.
- Stage 1 (built): imports and globs, no macro calls. Stage 2: calls as growth, expansion through
  the existing hook, the textual scope, the macro-namespace and lexical-outward read modes, the
  seam, tempo, then deleting the old core.

**Scope lead 20 (2026-11-16; runs in the scratchpad as `sl20-*.txt`).**
- Step 1 done. Tests 55 pass, 0 fail. The one-level `TextualEntry` closed the refusals: no
  `resolver refused` line anywhere (std set and hello set, 1 thread, `sl20-std1.txt`,
  `sl20-hello1.txt`). Unresolved imports / macro paths: core 0/0 (1,938 ms), alloc 0/0, bstd 0/0,
  std 22/0, libc 66/23, rustc_std_workspace_core 1/0. Classes: std 9 (`c_int`, `c_char`,
  `dirent` in `libc::..`) cascade from libc; std 7 `is_*_feature_detected` in `std_detect` (its
  macro-written `#[macro_export]` macros are not added to its root's answers: suspected, not
  traced); std 1 `Frame` in a `libunwind` module of no crate in the graph; 5 not printed (the
  example prints ten rows). libc 37 `core`, 4 `mem`, 4 `c_*`, 5 `sys`: libc's `extern crate
  rustc_std_workspace_core as core` is written by a root `cfg_if!`, and the extern prelude is
  fixed in the CrateRoot record from the syntactic graph (`root_plan`), so a macro-written root
  `extern crate` never enters it (rustc adds it). libc 13 `cfg_if!`, 3 `Debug!`, 2
  `s_no_extra_traits2!`, all in `unix/bsd/apple/mod.rs` (three levels of modules declared in
  `cfg_if!` expansions; untraced). rustc_std_workspace_core 1: `use compiler_builtins` (left
  out of the graph on purpose).
- Step 2, the cold-fill oracle (`src/tests/oracle.rs`): generated crates of up to six modules
  (mods and `mk!`-declared mods, items in two namespaces, every visibility incl. `pub(in ..)`,
  named imports, renames, globs, chains and cycles, `macro_rules!` with export and
  `#[macro_use]`) against a plain reference sharing no code with the solver; a disagreement
  is printed as Rust (`replay_one_tape`) and settled by the written rule below (four were
  first settled by running rustc on them: that was against rule 7, is withdrawn, and the
  files are deleted).
  Green on 600 cases; the control catches `Plant::GlobLeaksPrivate` (shrunk: 71 choices from
  206, `BLAZE_SIM_TAPE='5 0*7 1 0*25 2 0 1 0*6 2 0*2 1 2 1 1 1 2 1'`, the reference `Crate`,
  the solver `Public`).
- Fixed: a glob waited on itself (`use crate::m::m::*` reaching `m` through `m`'s own
  `use crate::*`): `Query::ignore` is now `lookup::Asking::{Import, Glob}` and `glob_lookup`
  skips the asking glob (an import never justifies itself). The forced pass was changed to
  force one call at a time; the rebuild below deletes it.
- Three stall faults, `a_found_glob_binding_does_not_wait_on_a_pending_glob`,
  `a_failed_sibling_glob_frees_the_other`, `a_failed_call_in_a_macro_use_child_keeps_the_grandchild`:
  ruled (coordinator, rule 66) one defect, rebuilt as the stall rule below. Fault 3 traced: the
  root and its `#[macro_use]` child are one group; the forced pass took the root's `mk!` first
  and failed it, its textual walk passing the child's pending `mx!()`. Also: the Reference
  (`items.use.glob.self-import`) refuses a glob of a module into itself; both sides accept the
  root's `use crate::*`: a finding, not yet fixed.
- **The stall rule** (replaces the forced pass and `fail_pending`). RFC 1560 resolves by a
  work list run "until fixed point", where a name is used when it "has exactly one best
  binding", a glob binding only once it is safe ("it may be shadowed"; "how a name resolves
  should be consistent under any ordering"), and what is left at the fixed point is an error;
  the Reference (`items.use.visibility.unambiguous`): redirections that "form a cycle or cannot
  be resolved unambiguously" are an error; "items and named imports are allowed to shadow
  names from glob imports" (`items.use.glob.shadowing`), and a glob never shadows: two globs
  bringing different items under one name are an ambiguity, an error only where the name is
  used. So undecided item i waits on undecided item j when deciding i reads name n (namespace
  ns) in module M and either n has no binding in M and j is a pending glob into M, a pending
  named import that can define n there, or an unexpanded call that can add to M (for a textual
  read: a pending call before i's place in the chain); or n is bound in M only through globs
  and j is a pending named import of n in M or such a call. Never a glob behind a glob once n
  is bound: a later glob bringing a different item makes the binding ambiguous, and every
  import that read it is solved again (its read list wakes it) to an ambiguity error. At a
  quiet point the sink components of the graph's condensation fail as not found, each note
  naming the other members it waited on; what waited on them is woken and solved normally;
  repeat until nothing is undecided. The RFC reports the whole list left at the fixed point and
  leaves open what the rest sees once some fail; sinks are a property of the graph, not of an
  order, so the answer is the same in any order. Fixtures: faults 1 and 2 each have one
  failing import (`self::m0::*`, `crate::m2::m0::*`); fault 3's `mx!` is a sink and `mk!`
  resolves; `mod m1 {} mod m2 { use crate::m3::*; pub use crate::*; } mod m3 { pub use
  crate::m2::m1::*; }` resolves with no error (the rule-4 guard); two globs with different
  items under a name read by a third import give one ambiguity error, and none when unread.
- Tempo before the rebuild (`blaze-world run names`, store s08-v3-all, 1022 crates;
  `sl21-tempo-before-{18,1}.txt`): 18 threads, tree c4c848c1ca: whole graph 19245.6 ms, libc
  0.2.190 6069.2 ms, core 1592.1 ms (2529 imports, 5352 macro paths, 0 unresolved); 1 thread,
  tree 1956152ad1: whole graph 19902.4 ms, libc 0.2.190 5956.6 ms, core 1782.4 ms. The graph
  barely gains from 18 threads: libc 0.2.190 alone is a third of the wall time.
- The reference (src/tests/oracle.rs) now implements the stall rule plainly: every lookup
  returns bound, absent, or the pending items it waits on (`Bk`); a decided import is tried
  again each round and only ever moves to an ambiguity failure; when nothing moves, `v` is in a
  sink when everything it reaches reaches it back, and the sinks fail (an import with what is
  undecided read as absent, a call as not found). The stuck/spared/force hacks and
  `HOLD_OUT_FAILING_CALLS` are gone, so generated calls may fail again; 2,000 cases per run,
  20,000 by hand. Fixtures: faults 1, 2 and 3, the rule-4 guard, the two-glob pair.
  `through` gained `Sources::OPEN`: an unreadable source is an open node, and the walk stops
  only if nothing reaches the asked source.

**Scope lead 19 (2026-11-16, the one-walk rebuild; runs in the scratchpad as `sl19-*.txt`).**
- (a) verified: before, 51 pass and 4 fail, every one `GlobBug::NoFixpointRow` at seed 518.
- Built: ONE walk. `lookup.rs` `member_node`: a member reached by `blaze_surface::through` is a
  node by its CURRENT facts (own binding, else its resolved glob edges `glob_edges()`; a visible
  pending import or glob, or an unexpanded call outside the macro namespace, stops the walk:
  `Wait` if blocked, else `Undet`), every fact read is touched so a change wakes the reader.
  Records and members are one least fixpoint. DELETED: `group_fixpoint`, `glob_closure`,
  `fellow_res`, `Memo`/`MemoMap`/`MemoKey`, the sink's `memo`, `hits`, `computing`,
  `fresh_memos`, `unsettled`, `settle_hits`, `Waiter::Memo`, `GlobBug::{FixpointBound,
  NotAMember, NoFixpointRow}`, `FillStats::{fixpoints, fixpoint_evals}`.
- Then seed 654 (masked by 518 before): a kept record that NAMES a removed module (root's glob
  edge to a removed `m::m`) was walked by the edited `m`. The test driver's `gone` now dirties a
  record naming a removed module by a glob edge or a binding; a read of a removed module is a
  typed failure naming its readers. After: 55 pass, 0 fail (`sl19-t4.txt`), plants caught.
- (b) verified in shape, not as written: std 118 and libc 20 refuse with `MemberUnfinished`
  ("the final pass waited with every answer in"), the same at 02:19 (`s1-std-fill3.txt`), so not
  the walk. Cause (being tested by removing it): `textual_entry` goes live only when the PARENT
  is a fellow; with a finished parent and a member grandparent, the library's `entry` answers
  `InProgress{grandparent}`, a wait on a member. Fix by construction: `TextualEntry{m}` means ONE
  level (the parent's own `textual[..mark(m)]`, waits on the parent only), the resolver walks the
  chain (live at the first fellow ancestor). Agreed with the names library and BUILT: `lookup.rs`
  `textual_entry` loops up `chain`; one level in `task/answer.rs` `entry`, `finish.rs`
  (`member_answer`, `owner_of`: the parent is the next module on a member's chain), the tests'
  `entry`, `pipeline.rs` and `crates.rs` `FillCx::entry`; `read.rs` docs say so. Compiles
  (`check ok`); its test run and the std re-run wait on crates.rs (library lane mid-edit, `Cfg`
  not Sync). SHOWN after (`sl19-t7.txt`: mixcheck 55 pass, 0 fail; `sl19-std2.txt`, 1 thread,
  std set): cause removed, effect gone: std 118 refused -> 0, libc 20 -> 0, core 0; unresolved
  imports core 0, alloc 0, std 542 -> 22 (6 `libc::c_int`, 2 `c_char`, 2
  `is_x86_feature_detected`, 1 libunwind `Frame`: names behind libc's own misses), libc 66
  (37 `core::` first segment, 13 `cfg_if!`: libc's own, not this walk). Core 1,613.6 ms, total
  2,524 ms, peak 440.5 MB (253.7 MB before, when 138 modules refused; the rise is unprofiled).
- Left for the successor, in order: (1) run tests and `examplerun.sh blaze-scope crates --solver
  worklist --loop fill --threads 1 std` (and bstd) and quote failed/unresolved per crate; (2)
  the NEW cold-fill property (oracle 3): a plain reference solver in the test file (add every
  binding any rule allows, whole crate, to a fixpoint), compared on generated crates with glob
  cycles, re-export chains, shadowing, with a planted bug it must catch; (3) delete `Passes`
  (item 8 below; keep `Solver`/`Worklist` until the library drops `S` from `solve`/`fill`, then
  tell it); (4) the counters/quiet-point fill (item 7) with the floor stated first. Measured so
  far, with the walk switched (`sl19-std1.txt`, 1 thread, std set, before the one-level entry):
  core 440 modules, 2,529 imports 0 unresolved, 5,350 macro paths 0 unresolved, 1,758.9 ms,
  0 failed; alloc 0 unresolved, 47.4 ms; std 542 unresolved imports (118 refused modules);
  total 2,219 ms, peak 253.7 MB. Core's time is still about 1.8 s: the walk change did not move it.

**For a successor (2026-11-16), the scope lead.** Tests at hand-off: blaze-scope 51 pass and 3 fail
(the three edit-property tests, all at seed 518, item 1); blaze-surface 19 pass.

1. **Glob fixpoint overrun: cause found (successor, 2026-11-16 02:40), fix held for rule 66.**
   - The one `overrun` flag had five causes under one message; it is now `GlobBug`, and
     `ResolveError::GlobLookup { module, why }` names which. Every refusal seen is
     `GlobBug::NoFixpointRow`: the 154 core and 38 std modules (`crates --threads 1 std`, both
     loops) and seed 518, no fixpoint ever changed past its bound.
   - Cause (backtrace in the fixture): `read_name` walks a FINISHED non-member's record
     (`Globs` edges) back into a member `f` that is no direct glob source of the walker;
     `fellow_res` assumes a row `(f, me)` in `glob_closure(me)` and finds none. Globs as edges
     let a module finish before its glob sources (its record needs no names of theirs), so a
     glob cycle can pass through finished records. Fixture (red until fixed):
     `a_walk_through_a_finished_record_back_into_its_group_is_answered`.
   - Second fix to the `Live`/`fellow_res` walk (seed 385 was the first): stopped for an outside
     verdict. Proposed end state: one least fixpoint, `blaze_surface::through`, for members and
     records alike (a member is a node with its own value, its edges and its determinacy), and
     `group_fixpoint`, `glob_closure` and the memo deleted.
   - Was: what is seen: "a glob fixpoint changed past its bound" refuses 154 core and 38 std modules in
     both loops (the names-library lane's runs). It is absent from the 01:46 `run12.txt`.
   - Which change brought it is NOT known. Candidates, all uncommitted in the tree: the
     `Undetermined` removal (lookup.rs, finish.rs, resolver.rs `fail_pending`), and other lanes'
     edits.
   - Note that both examples default to `Passes` (`WORKLIST` false unless `--solver worklist`),
     so the core numbers are Passes numbers.
   - The way in: the edit property reproduces it small.
     - Worklist, seed 518: `BLAZE_SIM_TAPE='2 0*3 1 0*3 3 0 2 1 0 2 0 1 0 1 2'` in
       `tests::edits::any_edit_sequence_gives_the_cold_records`.
     - Passes only, seed 517: `fn x; use crate::x; use crate::m::x; mod n { pub mod m { use
       crate::x; } } mod m { use crate::n::m::*; }`.
   - The bound is `4 * fresh.len() + 4` in `lookup.rs` `group_fixpoint`. Find which `Res` value
     oscillates: the lattice must be monotone.
2. **Cycles named on finished modules: fixed at the protocol (02:55), one failure moved.**
   `Answer::InProgress { module }` (the answerer names what it waits on), `Cached::InProgress(m)`,
   `resolver::owner` deleted, `cycle_of` reads the stored module (agreed with the library lane).
   std under the fill (`crates --threads 1 --loop fill std`): the "cycle names ..., held and no
   member" refusals are gone; 118 std modules now fail with `MemberUnfinished` ("the final pass
   waited with every answer in", `finish::settle_reads`), e.g. `std::sys::stdio`: next to find.
   libc still 20 failed. Core under the fill: 440 scopes, 0 failed, digest aadc0a9b017d5603.
   The lead it came from: which module a read
   waits on has several homes that disagree for `TextualEntry`: `task::answer::entry` waits on
   whichever ancestor is pending, `resolver::owner` names the parent only (also `again::owner`,
   `finish::owner_of`, the tests' `answer`, crates.rs). The rounds-loop log shows `TextualEntry`
   reads of made modules answered `InProgress` for an ancestor slot. Proposed: the answerer
   names it, `Answer::InProgress { module }`, and `resolver::owner` goes (with the library lane).
   Seen in the fill too: std 120 failed, "a cycle names ..., held and no member".
   - `resolver.rs` `cycle_of` builds `Step::Cycle` from every `sink.waits`, unanswered reads
     included. The contract (read.rs `Step`) is only modules answered `InProgress`.
   - It shows in libc's 20 modules ("a cycle names ... held and no member") and std's root.
   - Repro: build `runlocked.sh build --release -q -p blaze-scope --example crates`, then run
     `CRATES_DEBUG_CYCLE=1 EXAMPLE_TIMEOUT=600 bash examplerun.sh blaze-scope crates --threads 1 std`
     and grep `^cycle:` and `^inprogress:`.
   - Also suspected: a stale `InProgress` across a group merge for macro-made modules.
3. **Seed 385: fixed.** `read_name` dropped the ignored import, so a lookup coming back to its
   module through a kept record's glob waited on the import being resolved. It now passes `ignore`
   on. Fixture: `an_import_reaching_itself_through_a_kept_record_ignores_itself`.
4. **The edit property** (`src/tests/edits.rs`).
   - What it covers:
     - a generated crate of up to 5 modules, private, pub and pub(crate) items in three
       namespaces;
     - named imports, renames, globs, re-exports and cycles;
     - up to 4 edits, of 11 kinds;
     - a drawn schedule;
     - after every edit, records byte-equal to a cold fill.
   - Size and speed: `CASES` = 2,000, about 0.4 to 1.7 s per test. Worklist only: Passes dropped
     from its oracle (seed 517).
   - What it found:
     - Undetermined in a finished answer, now unrepresentable (seed 44, fixture
       `an_import_found_only_around_its_glob_cycle_is_one_error`);
     - Private versus NotFound decided by schedule (finish.rs takes it from the final state;
       fixture `a_private_import_reached_around_a_cycle_is_private`);
     - seed 385;
     - seed 518 (item 1).
   - Plants: `FellowReadsLeftOut` was caught. `KeepSuspects` was missed in 400 cases; at 2,000 it
     is untested until item 1 is fixed. The hand fixture in props.rs catches it.
5. **Row (c) and the floor table: not measured.** Only the earlier run exists:
   `scope-lead-18-ruleC-edit.txt`, row (c) 1 record, 5,951 items, 2,318 events, 12.3 ms. Profile
   it before any claim.
6. **`pipeline-fill.diff`** (names-library lane, in the scratchpad): NOT applied. It waits for a
   green property and item 1. Apply it with the Edit tool; `grouped` is not needed.
7. **The cold-fill redesign (counters).**
   - The idea, as far as it goes:
     - each pending item (import, glob, call) holds a counter of its hard waits (unanswered
       reads, unexpanded calls);
     - an answer decrements, and the counter reaching 0 fires that item's evaluation once;
     - a quiet point is an empty queue with counters still above 0;
     - at a quiet point, one backward reachability pass from the stuck items over the wait edges
       finds the set no outside answer can move, settled at once by the least fixpoint;
     - this replaces the group re-solve at each of the ~1,257 waves.
   - Report quiet points, events, wakes and items evaluated. The floor (one pass over core's 2,510
     imports and 5,266 macro paths) is still to be stated and measured.
   - It must fit tonight's decision: the fill is ordered by the syntactic import graph from the
     declaration records before any scope task runs. First entries go in dependency order, and
     each SCC is a group from the start (re-entry measured R = 0.58 by tasks on core, against
     0.10).
   - So the counters run inside a group's task. Re-entry is only for what text cannot decide
     (paths through re-exports, macro-written imports).
8. **`Passes`.** Still used by:
   - `resolver.rs` `step` (its default);
   - `group.rs` `step`;
   - both examples' default (`step_by::<Passes>` and `fill.run::<Passes>`);
   - the tests' `solve_with`, which compares both solvers, and the plants `PlanAll` and
     `StaleNames`.

   Deleting it (rule 30):
   - make Worklist the default everywhere and drop `--solver`;
   - delete `Passes`, `run`'s pass loop and the planned-pass machinery only it uses (`Plan`,
     wake lists);
   - drop the two plants with their controls;
   - simplify `solve_with` to one solver.

Earlier state (scope lead 17):
- `work.rs` is the worklist solver, behind `Solver` (`Passes` | `Worklist`; `step_with::<S>`).
  The tests run both and compare them. 39 pass, among them `any_event_order_gives_one_result`,
  `waking_the_parked_gives_waking_everything` and the controls `a_dropped_wake_is_caught` and
  `forgetting_glob_probes_is_caught`. Its core result matches the pass solver's (12 unresolved,
  the same bindings by origin).
  - Speed: not faster. 1,639 ms cold (passes 1,766 ms), allocated 2,375 MB (4,044), peak live
    43 MB once the parked lists are compacted (709 MB before compaction).
  - The cost is 1,257 relax rounds on the 192-module group: mode W blocks every item after
    each move. Blocking only the dirty set was tried and was 2x slower (1.2M events, still
    1,257 rounds), then reverted.
  - The coordinator's design for the flags (counters, waits-on graph, one backward pass per
    quiet point) is not built. Do not delete `Passes` until the worklist is faster.
- `pipeline --edit` runs six edits incrementally from the first fill's records (rows in
  `scope-lead-17-edit.txt`): body 0 ms, private fn 1.6 ms, pub fn in a glob source 43 ms,
  pub use inside the 192 group 39 ms (all 192 re-solved).
- Open correctness work:
  - The `fellows` field depends on the driver's schedule (rule 25): row (d) differs from a
    cold fill in `fellows` only.
  - Reads between members are kept out of read lists, so any edit in a group re-solves all
    of it.
  - Fix both: the group becomes the SCC of the final read graph, and reads between members
    go into the read lists.
- Cold split (`scope-lead-17-split.txt`, timers in the example), 1 thread: expansion 259 ms,
  solver steps 1,534 ms, 1,802 ms wall. The solver dominates.
  - The same 192 modules re-solve in 39 ms when every expansion is already there.
  - So the cold cost comes from re-solving the whole group at each expansion wave (1,472
    passes, 1,257 relax rounds), not from the size of the work. The counters design is what
    removes it.
- Next: build the counters (or expand eagerly, so the waves come in fewer, larger batches);
  fix `fellows` and the reads between members; then `--dump-bindings`.

Where things are (all in /Volumes/Projects/blaze-fresh-1115, uncommitted on top of `dbae95611d`):
- `crates/blaze-scope`: the resolver. It is a sans-IO machine (`Resolver`, and `Group` for one
  strongly connected set) speaking the protocol in `read.rs`: `Read::{Name, Globs, TextualEntry,
  TextualOut, DeriveHelpers}`, `Answer`, `Step::{Ask, Expand, Cycle, Done}`.
  - `resolver.rs`: state, `apply`, `absorb`/`cached`, wake stamps.
  - `lookup.rs`:
    - `lookup_module` = `own_lookup` then `globs_res`/`glob_lookup`;
    - `Live` is the `Sources` impl that reads fellows live and other modules by records;
    - `read_name`, `fellow_res`/`group_fixpoint`;
    - path, import, glob and call resolution; `plan()`.
  - `finish.rs`: `finalize` writes own bindings plus `glob_edges()`; `undetermined_through`
    gives the unresolved-glob error.
  - `watch.rs`: wake lists. `scope.rs`: `ReadList`, `Resolved`, `Saw`.
- `crates/blaze-surface/src/through.rs`: `Asker`, `SeenFrom` (`visible_from`, `narrower`,
  `wider`), `through()`, the lookup through glob edges (a least fixpoint per name over the
  reached sources, `merge` = same def widest vis, different defs `Ambiguous(GlobVsGlob)` under
  the smaller def). It is the one home for the resolver and every reader.
- `crates/blaze-surface/src/module_scope.rs`: `ModuleScope` form 2 (own bindings, `globs`
  edges, errors, textual, marks, fellows, traits, macro exports); `GlobEdge`; `globs_digest`.
- Tests: `crates/blaze-scope/src/tests/` (35: rules.rs one per rule, more.rs, props.rs edit and
  order properties with tapes, records.rs). The driver is `solve`/`solve_with`;
  `Solved::binding` reads through the edges; `Plant` holds the planted bugs.
- `crates/blaze-scope/examples/pipeline.rs` runs the whole layer on a real crate. Command:
  `EXAMPLE_TIMEOUT=600 bash /Volumes/Projects/blaze-int-1112-notes/examplerun.sh blaze-scope
  pipeline [--threads 1,18] library/core`.
  - Its "events:" line gives the floor counts; "work" gives passes, lookups and fixpoints.
  - Checks go only through `bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh <crate>
    [test]`.
- Runs: `/Volumes/Projects/blaze-int-1112-notes/port27/scope-lead-*.txt`.
  - 12: copy form, 2,330 ms, 991 MB RSS.
  - 13: 1 and 18 threads; the 192-module group alone is 1,536 ms.
  - 14: event counts.
  - 16: edge form, 1,646 ms, peak live 36 MB, RSS 127 MB, records 2.83 MB, the same 12
    unresolved (`legacy_int_modules`, modules written by a macro, not walked).
- hir-def's `ModuleScope` (`crates/hir-def/src/nameres.rs`) reads own bindings only and does
  not follow glob edges. It is an old-layer crate that does not compile for other reasons (an
  older `Visibility`; base-db broken under it); it is left as is.

Owed before step 5 is done: the oracle that the edge form lost nothing.
- Build `dbae95611d`'s blaze-scope and blaze-surface (copy form; its bindings equal run 12's)
  in a worktree, and dump every (module, ns, name, def, vis, state) of `core`.
- Check that `Solved`-style `through` over the new records gives the same binding for each of
  the 86,671 glob-copied names, and that the 2 undetermined stay undetermined.
- Keep a small-fixture version as a test.

Step 5 needs:
- A forward worklist in one arena per group: columns with `u32` ids, nothing allocated per
  event.
- It sits beside the pass solver behind one type parameter, and is held equal on the 35 tests
  and on `core` (byte-equal records, the same 12 unresolved).
- A planted bug in it must be caught by a test. Then switch over and delete the pass solver:
  `watch.rs`, `group_fixpoint`, `plan`/passes.
- Report against 1,646 ms and 127 MB, on 1 and 18 threads. The floor is about 16k events,
  about 1 ms.

Where Rust is not monotone, and how each is handled today (keep each, with its test):
- **An explicit or expanded item shadows a glob's name.** `own_lookup` answers first. Its
  determinacy follows rustc: a named import still pending gives `Wait` or `Undet`, and a call
  not yet expanded makes a glob answer `Undet`/`Wait`. A glob name is final only at
  quiescence. Tests: `rules::explicit_shadows_glob`; expanded-vs-outer ambiguity
  (`MoreExpandedVsOuter`, `GlobVsExpanded`).
- **Textual `macro_rules!` and `#[macro_use]` scope.** Its own ordered sequence
  (`TextualEntry`/`TextualOut` reads, `marks`), not the lattice.
- **"No namespace resolved" for an import.** Decided only at quiescence (`fail_pending` in the
  final pass), as `Unresolved::NotFound`/`Undetermined`.
- **One item through two glob routes** is one binding (`through::merge`, widest vis). Two items
  are `Ambiguous(GlobVsGlob)`, an error only where used (`rules::two_globs_ambiguous_only_when_used`).
- **The prelude and builtins** are the lowest layer (`layer_name`, `Layer::Prelude`, the
  builtin types).
- **An undetermined answer** at quiescence is an unresolved import of the glob that reached it
  (`more::an_undetermined_answer_is_an_unresolved_import`, via `undetermined_through`).
- **Blocking.** An item is blocked only if its wait chain reaches an outside answer or an
  expansion (one reachability walk).

Reviewed by an outsider on 2026-11-15. No per-name, demand-driven resolver of Rust is known to
exist; rustc and rust-analyzer resolve a crate whole because expansion and imports interleave.
Replaced: the first form of this section, an "import group" found before resolution, because what
`use a::b::*` names is itself a resolution, item macros add `use`, `mod` and `#[macro_export]` only
after they expand, and grouping by module invents cycles that are not cycles of names.

**The producer** (decided with Andrew, 2026-11-15 18:37). Three grains were weighed: one record
per crate (any new name redoes the crate), per module updated incrementally, and per-name pull
from cold. The choice is the middle one:

- **First fill.** The first time a crate is seen (not fetched), all its modules are resolved
  together by the known fixpoint, in parallel across module groups (strongly connected sets).
  Each module's scope is its own record with its per-name read list.
- **After an edit**, only the modules whose read lists no longer hold run again: the edited
  module, and readers of a name that changed, appeared or went. A module in a glob cycle runs
  with its partners.
- **One machine per module** (`blaze_scope::Resolver`), stepped by the driver: `Ask(reads)`,
  `Expand(calls)`, `Cycle(modules)` (join a `Group`), `Done(scope, read list)`. The same machine
  serves "all modules of this crate" and "these modules again". It never computes another
  module's scope; its result is a function of the records alone, not of the order answers came.

The rules it holds:

- **Read per name.** A reader asks `(module, namespace, name)` or a module's glob edges, per
  name through them (below). A private fn added to a module moves no reader that did not ask
  for that name, and no glob outside it.
- **A binding has states.** Target key, visibility, origin (declared, explicit import, glob,
  expanded), and `undetermined` or `ambiguous` as values, not errors. Explicit bindings are a
  layer below globs, so the fixpoint is well defined; an import waits until its answer is final;
  a macro path resolved early records the scope it hit and is checked against the final scope
  with rustc's ambiguity kinds, which makes any evaluation order agree with rustc.
- **Cycles are found at run time.** A glob read that reaches a module still being computed joins
  a strongly connected set, solved as a least fixpoint; each member is written as its own record
  with its reads of its fellows in its read list, the same whatever the schedule.
- **What an edit redoes** (estimates, to count per edit class): a body edit, one `FileItems` and
  no scope; a private fn added, about three records; a `pub use x::*` at the root, the root's set
  and the modules that glob it; an edited `macro_rules!`, its N expansions and the scopes whose
  names changed.

**The floor of a fill, counted on `core`** (2026-11-15, `scope-lead-14.txt` in
/Volumes/Projects/blaze-int-1112-notes/port27): 406 modules, 18 cycle groups (the largest 192
modules); 13,568 declarations (expansions' included), 2,436 named imports with 4,629 path
segments, 74 globs, 5,263 macro calls (3,962 expansions). Out: 102,688 bindings, of which 86,671
came through a glob (2 not determined), 13,272 declared, 2,702 by a named import. A forward
fill makes each binding once: about 103k events, plus 4.6k segment steps and at most 2.4k import
wakes, so about 110k table operations. At 20 to 50 ns an operation on `u32`-id columns, the floor
is 2 to 6 ms on one thread, plus cutting 11.3 MB of records (about 5 ms), with expansion measured
apart; peak live is the tables, about 10 MB. Measured with the pass-based group solver: 2,330
ms, 593 MB peak live, 991 MB RSS, about 300 times the floor. The aim (rule 67) is under 15 ms and
20 MB for names on one thread, within 1.5 times the floor, and 1.1 times when the records are
cut in place. Glob flows are 84% of the events: if a scope record held a glob edge rather than
its 1,170 average copied names, the floor would drop by the same share, which is a question for
the record format and not for the solver.

**Globs are edges, not copies** (decided by the coordinator, 2026-11-15, from the counts above).
A `ModuleScope` record holds what its module declares, what its named imports bind, and its glob
edges (source module or enum, visibility, expansion depth), never a name copied through a glob.
A name reached through a glob is found at lookup: the module's own binding first (an explicit
binding shadows a glob), else the union over its edges of each source's value, each filtered by
what the importer may name and narrowed by the edge's visibility, a glob cycle contributing only
what some member binds (the least fixpoint, solved per name over the edges it reaches). One item
by two routes is one binding, as visible as the widest; two items are ambiguous, an error only
where used (Rust's own rule). The protocol reads it per name: `Name` answers a module's own
binding, `Globs` its edges, and a lookup's read list holds each `(module, name)` and each `Globs`
it probed, found or not, so a new shadowing or newly ambiguous name moves exactly the lookups it
reaches, and a `pub` item added to a module others glob rewrites no importer's record.

New floor on `core`: about 16k events (13,568 declarations and 2,436 named imports, against
103k), so about 1 ms of table operations on one thread.

Measured on `core` (2026-11-16, `ModuleScope` form 2 without the export-class column, still
the pass solver; run `port27/scope-lead-16.txt` against `scope-lead-12.txt`): records 2.83 MB
against 11.3 MB (16,017 own bindings and 74 edges, against 103k bindings); names 1,646 ms
against 2,330 ms on one thread; allocated 2.02 GB against 4.82 GB; peak live 36 MB against
593 MB; maximum RSS of the whole run 127 MB against 991 MB. Unchanged: the same 12 unresolved imports, the same failures,
and stage 5 resolves the same 10,462 impl rows through the edges. The lookup through edges is
`blaze_surface::through`, one home for the resolver and every reader of the records. The time
left is the pass solver's (1.85M items planned, 1.99M lookups): what the group solve replaces.

**The group solve** (replacing passes, 2026-11-15): one arena per strongly connected set by a
forward worklist. An item that cannot advance parks on the exact keys it read: `(module, name)`,
`(module, globs)` (its edges and whether a call there may still add one), `(module, textual)`;
an event on a key wakes only its waiters. What is not monotone stays out of the worklist and is
decided when the group is quiet: textual `macro_rules!` scope is ordered by position; whether an
import binds no namespace, a glob's ambiguity, and a macro-expanded item shadowing a name already
taken through a glob (rustc's `MoreExpandedVsOuter` and `GlobVsExpanded` errors, found from the
reads recorded per site) are decided at quiescence; the prelude and builtins are the lowest
layer. Quiescence with items still parked is the cycle case: an item is blocked only if its wait
chain reaches an outside answer or an expansion, by one reachability walk, not rounds.

**The gate for pull from cold.** Pull from cold (resolving only the modules a program reaches)
saves work only for a crate never seen and not fetched, and is the one part no Rust compiler has
built. It is built only if the measured first fill costs more than about 10 ms on reth's or
tempo's largest crate. The measurement is the whole fill: outline, expansion of every item macro,
and resolution, each reported; not item lists alone (outlining `core` alone took 132.5 ms). With
it, measure the size distribution of glob cycles on std, reth and tempo: if real cyclic sets are
large, per-module work collapses into a crate-wide task. Replaced: "The risk ... before the pull
producer is trusted", because the producer is per module and the risk now gates only pull from
cold.

### Name resolution: the rules (2026-11-16)

The rules the name solver and the reference solver (src/tests/oracle.rs) both hold. Each has
fixtures in src/tests/name_rules.rs (`rNx_*`), solved in the calm order and 64 drawn completion
orders; the answer must be the rule's in every order. Sources: the Rust Reference's
expansion-time name resolution (`names.resolution.expansion.*`, saved in
port27/world-1116/nr.txt), `items.use`, and RFC 1560; no compiler's behaviour is a source.

1. **R1 shadowing.** In a module, an item or a named import shadows a glob import of the same
   name in the same namespace ("items and named imports are allowed to shadow names from glob
   imports", `items.use.glob.shadowing`). An expanded item shadows a glob binding no early
   resolution used; one that was used is `GlobVsExpanded`. Fixtures r1a, r1b, r1c.
2. **R2 glob vs glob.** Two globs bringing the same item (after following re-exports) under one
   name are not ambiguous; different items are ambiguous, an error only where the name is used
   ("The errors occur at time of use, not time of import", `...ambiguity.glob-vs-glob`).
   A use at ANY segment counts (`self::m2::m0::*` uses `m2`: ambiguous, not NotFound at `m0`).
   A name bound through one glob still waits on the module's pending globs (and on sources the
   glob walk meets undecided): one may bring another item, and the answer must not hang on which
   was decided first (2026-11-16, r2e: the reference took the root's `m2` before `crate::m0::*`
   brought crate 3's; planted `R2BoundStopsWaiting`). Fixtures r2a-r2e; oracle.rs `two_globs_*`.
3. **R3 glob cycles** are legal; the answer is the least fixed point (RFC 1560: resolution runs
   "until fixed point"). A name in no member of a cycle is not found, and nothing hangs. r3a;
   oracle.rs `a_glob_cycle_through_a_found_binding_resolves`.
4. **R4 privacy.** A glob brings only the names visible at the importing module
   (`items.use.glob`). r4a; oracle.rs `a_glob_that_brings_in_private_items_is_caught`.
5. **R5 the stall rule** (Scope lead 20, above): at a quiet point, groups that wait only on
   themselves (the sink components) fail as not found; repeat. Refinement (approved): a member
   that waits on ONE macro call of the same group through a textual read is spared that round;
   the order is program text alone. In a stalled member what is undecided is taken as absent,
   so a plain first segment whose inner scope was undecided goes outward (R6) and the import may
   RESOLVE: then only the members that resolved are decided that round, and the rest are tried
   again with the facts ("nothing more comes" holds only for what fails); with none resolved,
   all fail. The cell core needs no such round: it decides a stalled group from the group's
   least fixed point, where the rest already read what the resolved members bring
   (d116-decide-4: without the round, 0 differences on std's 12 crates and tempo's 506; the
   oracle keeps the round). A spared call still expands, so "nothing more comes" is false for whatever waits
   on it: in a round with a spared call only the calls a spared call's textual read waits on
   fail (they define nothing first), and the rest are tried again (2026-11-16 08:00: the
   reference failed `use crate::m2::*` beside a spared `mk!` writing `mod m2`, contradicting
   "names in the fully expanded AST must resolve to the same definition"; r5e, r5f). r5a-r5f;
   oracle.rs faults 1-3; planted `R5NoStall`, `R5SparesAll`.
6. **R6 lexical lookup** (D56). A path's first segment `n` that misses in the inner scope goes
   outward: module, extern prelude, standard prelude, built-in types. If the inner scope can still
   grow BY EXPANSION ONLY (unexpanded calls in that module; for the extern prelude, unexpanded
   calls at the crate root), the lookup takes the outer binding; if an expansion not enclosing
   the use later writes `n` into the inner scope, that is an ambiguity error at the new
   definition, in every completion order; written by an enclosing expansion, the inner wins;
   with no outer binding, the inner is used when it appears. The Reference gives the principle
   only: a name "must resolve to the same definition regardless of the order in which macros are
   expanded and imports are resolved". r6a-r6d.
7. **R7 pending imports are waited for.** A pending glob or named import into the inner scope is
   not growth by expansion: the lookup waits for it. A glob candidate beside an outer candidate
   is an ambiguity ("Names in imports and macro invocations may not be resolved through glob
   imports when there is another candidate available in an outer scope",
   `...ambiguity.glob-vs-outer`); a named import shadows the outer one
   (`...imports.shadowing`). The fast solver as read at 06:10 goes outward on an unblocked
   pending glob (`globs_res` weak): r7a decides whether that changes the answer by order.
   The ambiguity is at the first segment: the import fails there, as ambiguous, whatever the
   later segments hold, and binds nothing (r7d, r7f). A glob candidate that IS the outer
   candidate's definition is one candidate, no error (D57: "another candidate"; r7e).
   r7a-r7g.
8. **R8 the extern prelude.** A name from the unit's dependencies is final; a macro-written root
   `extern crate X as n;` may add a name (`names.preludes.extern`), seen from every module, never
   replace one (an error, whether or not a path reads it; D56). Until the root's calls expand, a
   first segment with no candidate waits on them. r8a-r8d.
9. **R9 built-in types.** A one-segment type path naming a built-in type whose lexical binding
   is a module resolves to the built-in type (`use core::str; fn f(_: &str)`); as a path prefix
   the module stands; a struct of the name shadows the built-in. r9a (both sides: the reference's
   `type_path`, the solver's records through `names::path`); tests/entry.rs
   `builtin_type_name_beside_a_module_of_its_name`.
10. **Expansions' text (2026-11-16).** An import, item, `extern crate` or call an expansion writes
   is the module's (its call stands there) from the moment the call expands; before that it is
   the module's growth by expansion (R6). Imports are keyed by the fragment they are written in
   (`UseSite { expansion }`). A call an expansion writes nests; its expansion is ENCLOSED by the
   outer one, and R6's error is for a definition (an item, `mod`, `extern crate`, or an import
   bringing the name) written by an expansion that does not enclose the use. r6f-r6k.
11. **R5, later segments (2026-11-16).** In a stalled try every undecided lookup, a later path
   segment's too, takes the undecided as absent (the reference failed it as not found: the cell
   core lane's seed 1983). r5g.

Owed (the reference): it still models one crate, `crate`/`self`/`super` paths and a macro that
writes only `mod` items; R6-R8 are checked by fixtures against the rule, not yet by the
2,000-case comparison. Widening it (dependencies, preludes, built-in types, lexical first
segments, `mk!` writing items, uses, `extern crate` and calls, enclosure) is step 1 of the
reference's brief, with one planted bug per rule.

For a successor (name rules lane, 06:21). `mixcheck blaze-scope test name_rules`: "19 passed,
0 failed" at tree 3a48978ca1 (18 fixtures, each calm plus 64 orders `Plant::Shuffle(seed)` with
a drawn answer tape, plus `control_a_wrong_answer_is_caught`). The fast solver passed every
fixture in every order, r7a (pending glob) included: no order-dependence shown yet, `KNOWN` is
empty. Not done: steps 1 and 3 (widen oracle.rs: crates, preludes, built-in types, lexical
first segments, `mk!` writing anything, enclosure; a planted bug per rule; the 2,000-case run
classified); R6 through the standard prelude and macro paths (no fixture yet); r7a's glob is
only one link deep: deepen it (a glob behind a pending macro call) to stress the early-outward
path; message the solver-rebuild lane (OWNERS.md) once `KNOWN` has rows.

For a successor (name rules lane, 07:00). Step 1 done: `r7_stress_pending_glob_sources` (11
pending-glob sources: chains of 2 and 3 globs, a named import, a macro call, a dependency's glob;
outer `n` in the extern and the standard prelude; with and without `n`: 22 cases, calm + 64
orders): "20 passed, 0 failed": no order-dependence. Step 2 written in oracle.rs: `draw_in(tape,
wide, calls)` (old distribution = `draw`, still 2,000 green); wide = a dependency (extern name
`m1`), a standard prelude (`m1`, `m3`), plain first segments (`Ref::first_plain`, `outer`,
`r6_findings`), `mk!` writing items; `Bug` (R1-R4, R6-R8) planted in the reference. The seam for
the new core: `crate::tests::{reference_world, reference_imports, Imp, B}`. Two disagreements,
both tests `#[ignore]`d with the reason: (a) no-calls seed 29: `use m1::m0::*` where `m1` comes
by a glob and from the extern prelude: the old solver NotFound, the rule R7 glob-vs-outer: the
SOLVER is wrong (write it as fixture r7d, put it in `KNOWN`); (b) wide seed 107: `y` in Types,
reference None, solver a crate-1 binding: unclassified (replay it). Then un-ignore
`every_planted_reference_bug_is_caught`. Not modelled yet: `extern crate` by `mk!`, `mk!` writing
uses and calls, enclosure, built-in types (R9), R5's bug.

For a successor (name rules lane, 07:00 runner, handing on at 06:58). Uncommitted in the tree.
Verdicts: no-calls seed 29 = fixture r7d (glob-vs-outer at the first segment beats a later
NotFound; the glob's `m1` is always crate 1's own module in that distribution, never the
dependency: no import there binds a crate root); the same-definition case is D57 (one candidate,
no error; r7e passes the old solver). Wide seed 107 = TWO findings: r7f (the old solver reports
glob-vs-outer early but still resolves the import through the glob candidate and binds `y`: an
error is reported, so not a silent accept) and a REFERENCE bug, fixed: a stalled plain first
segment failed NotFound instead of going outward (R5 text above, r5c, r5d). No-calls seed 156 =
r7g, a WRONG ACCEPT by the old solver (no error at all). Wide seed 39 = r6e. Wide seed 110/196:
the reference's R6 finding now ignores its own order's outcome, and after an R6 error bindings
are not compared. `KNOWN` = r6e, r7d, r7f, r7g; the comparisons skip exactly those classes
(oracle.rs `known_import`, and r6e in the R6 check) and count them: "no calls: 2000 cases; cases
with a KNOWN difference, by class: {"r7d": 23, "r7f": 7, "r7g": 1}" and the test passes. The
comparison is now strict on Pending (a reference that leaves an import or call undecided fails).
Open: the wide run fails at seed 225: `use m3::*` (glob) in module ...17: the solver NotFound,
the reference Glob(crate 1): replay `ORACLE_DIST=wide ORACLE_TAPE='5 0*8 1 1 0 1 0*2 1 0*4 2 0*4
1 0 3 0*4 3 0*40 1 0 1 3'` through `replay_one_tape` (it now prints plain paths and the preludes)
and classify by hand. Then `every_planted_reference_bug_is_caught` (un-ignored, now with
`R5NoStall` and `R5SparesAll`; it prints "planted .. caught: <first difference>") needs the clean
wide run green. Not started: step 3 (mk! writing uses, extern crate, nested calls, enclosure,
R9 and its planted bug) and the message to the new core's lane. The other lane's cells/mod.rs
breaks the crate's build often: mixcheck's output file is shared, copy it at once after the run.

For a successor (name rules lane, 08:00 runner; handing on). Uncommitted in the tree.
`mixcheck test blaze-scope` (filter `tests::`): "exit 0; 129 passed, 0 failed", with both
comparisons and `every_planted_reference_bug_is_caught` back on. Wide seed 225 is NOT a false
NotFound: root `use m1::*` is an R6 error (`mk!` writes `mod m1` beside the extern `m1`), and
m4's `crate::m3::*` follows the order through it. The comparison now skips every import's
recovery after an R6 error ("R6 error, recovery not compared"). Wide seed 1402 = r3b (the old
solver gives no answer). Seed 644 was a REFERENCE bug in R5: it failed members waiting on a
spared call. Fixed, with the R5 text above and fixture r5e. That exposed r5f, an old-solver
FALSE NotFound (seed 9; main told). The generator, at the coordinator's request (the new core's
`core` bugs): module keys are XOR-mixed (`World::key_mix`, drawn last, so old tapes replay), and
modules and imports draw `pub(super)` / `pub(in path)` (`any_vis`). Planted: R7's catch
needed a new comparison arm: a solver's early ambiguity on a plain first segment that the
reference resolved. Quotes, all "by the wide": R1 `q` in Macros; R2 `m2` amb; R3 `z` None vs
Some; R4 `z` Some vs None; R6 `["m3","m4"]` NotFound vs Glob; R7 `["m1"]` Glob vs early
ambiguity; R8 `["m2"]` Failed vs None; R5NoStall and R5SparesAll: Pending. Run the planted test's
binary with CARGO_MANIFEST_DIR set, or the prints differ. Not started (step 3): `mk!` writing
`use` (named and glob), root `extern crate dep as n;`, nested `mk!`, enclosure, and R9 with its
planted bug. Plan: key imports by CONTAINER (a module's text or a Done call's expansion: `Bk::Imp(container, i)`,
compare with `UseSite { expansion: Some(..) }`); make `own`/`globs`/`children`/`calls_of`/`live`
walk Done calls recursively; R9 needs a type-position query on both sides (the solver's names
library: tests/entry.rs `builtin_type_name_beside_a_module_of_its_name`). New core's lane
(a3b5ce47fc4092767) has the step-4 message.

For a successor (name rules lane, ~08:30 runner; handing on). Uncommitted in the tree. Done in
oracle.rs: imports keyed by fragment (`frags`, `uses_of`, `written_in`, `encloses`, `mod_site`);
`own`/`globs`/`calls_of`/`children`/`live`/`textual` walk expanded calls recursively;
`extern_added`/`extern_replacing` (R8); `type_path` and `solver_type_path` (R9, over
`names::path`); R6 findings with enclosure and import candidates; the R5 later-segment fix
(`lookup_absent`). Generator: `draw_deep` (new distribution, `ORACLE_DIST=deep`; wide and
no-calls tapes unchanged), `a_deep_cold_fill_equals_the_plain_reference`. Planted bugs added:
`R5StallFailsMiddle`, `ExpansionImportsIgnored`, `NestedNotWalked`, `R6IgnoresEnclosure`,
`R8MacroReplaces`, `R9ModuleIsTheType` (their catches not yet quoted: run
`every_planted_reference_bug_is_caught -- --nocapture`). Fixtures r5g, r6f-r6k, r8c, r8d, r9a,
r2c. New KNOWN: r5g, r8d, r2c (the old solver). `replay_one_tape` writes its report to
`ORACLE_OUT` (the checker cuts test output). Runs: "135 passed, 4 failed" -> after r5g/r8d
"141 passed, 1 failed" (deep only) -> deep's first two disagreements classified: a generator bug
(two `pub type y`: wide's mk item not in `items`, fixed) and r2c. OPEN, in this order: (1) the
names runner's run (port27/world-1116/r15-scope-test0.txt) says r2c's first fixture PASSES the
old solver: rewritten to reach `k` through `a`'s own glob of the root, as the oracle program
(deep-seed-amb.txt) does: re-run `test r2c`; if it still passes, find the program's real
difference, never weaken. (2) A new deep disagreement: `use m2::m0::*` (plain, glob), the solver
Ambiguous(GlobVsGlob), the reference Failed(NotFound); tape in r15-scope-test0.txt; replay with
`ORACLE_OUT=<file> ORACLE_DIST=deep ORACLE_TAPE=..` via lanecheck (mixcheck does not pass the env)
and resolve by hand. (3) Quote the planted catches, the three 2,000-case summaries, then message
the new core's lane (ae1c3be3921c99701 per OWNERS.md 07:36): sent once at hand-off, see there.
The crate failed to build at hand-off from resolve.rs (names library lane's).

For a successor (name rules lane, ~09:30 runner; handing on). Uncommitted in the tree. Runs
quoted from port27/world-1116/r16-*.txt (`ORACLE_OUT=<file>` now also keeps every KNOWN verdict,
2,000-case summary and planted catch: oracle.rs `keep`; lanecheck only, mixcheck drops the env).
State at start (r16-state.txt): "142 passed, 2 failed" (r2c "passes now: take it off `KNOWN`";
deep `use m2::m0::*` solver GlobVsGlob, reference NotFound); ignored: `a_cold_fill_..._in_full`
("long"), `the_planted_leak_shrunk` ("fails by design"). Done: (a) r2c is now the generator's
program exactly; the old solver fails it: "KNOWN failure r2c: calm order: no ambiguity of `m2`:
errors [Import { .. segments: ["m1", "m0"] }, segment: 1, why: NotFound }, ..]" (a FALSE NotFound
upstream hides R2; the plain form is r2d and passes). (b) the deep `m2::m0` case: the REFERENCE was
wrong (R2, and rule (f): a name bound through one glob stopped waiting on the module's pending
globs, so it took the root's `m2` before `crate::m0::*` brought crate 3's): fixed in
`lookup_parts` (not the macro namespace: there it made a sink of `mx!` and a glob, oldest seed
364; open), and a stalled plain first segment now keeps the decided globs' value; fixture r2e,
planted `R2BoundStopsWaiting`. The next deep tape (r16-deep2.txt, `self::m2::m0::*` in r2c's
program) is class r2c (known_import: GlobVsGlob vs NotFound). Last run (r16-full.txt): "146
passed, 1 failed"; "no calls: 2000 cases; cases with a KNOWN difference, by class: {"r7d": 25,
"r7f": 7, "r7g": 3}"; "wide: 2000 cases; ..: {"R6 error, recovery not compared": 47, "r5f": 15,
"r5g": 3, "r6e": 34, "r7d": 27, "r7f": 10, "r7g": 1}". OPEN, in order: (1) deep fails at
`ORACLE_TAPE='1 0 0 0 0 0 1 1 0*7 2 1 0*6 1 1 3 0*27'`: "R6 says `m1` is ambiguous (outer, then
written by an expansion), the solver reports nothing: []": replay, resolve by hand (a one-module
crate: likely R8's `extern crate e3 as m1` beside a path read of `m1`: r8d's class or the
reference's); until deep is green its planted catches are NOT proof: R2BoundStopsWaiting and
R8MacroReplaces were "caught" by an R6 line like it. Catches by the wide/no-calls (clean, green):
R1 `q` in Macros; R2 `m2` amb; R3 `z`; R4 `z`; R6 `["m3","m4"]` NotFound vs Glob; R7 `["m1"]`;
R8 `["m2"]`; R5NoStall, R5SparesAll Pending; R5StallFailsMiddle `["m2"]`; by the deep:
ExpansionImportsIgnored `m0`, NestedNotWalked `y`, R6IgnoresEnclosure `m3` R6, R9ModuleIsTheType
`str` (r16-proof.txt). (2) step 4 checked against the code: r8c (root `mk!{extern crate other as
k;}`, read from `mod b`), r6h/r6i/r6j (enclosure), r9a (type position both sides) exist and run
64 orders; nothing was missing. (3) the macro namespace's R2 wait.

### For a successor (name solver, runner after 06:10; read first)

Steps 1-4 still open; nothing of this runner's is left in the code (one fix tried, taken out).
- Suite: `mixcheck blaze-scope test` 92 passed, 0 failed at 06:05 (the `per_crate` failure seen
  at 06:00 was the names library's test mid-edit; it passed at 06:01 with no solver change).
- Baseline p7 (`port27/world-1116/run-libc.sh <tag> <roots> <sample-secs>` runs `names` at one
  thread with `--crates` and samples it; `kids.py <sample> <fn-substring>` lists a function's
  direct callees and self): "15 crates resolved in 4926.6 ms"; totals imports 7058, unresolved
  23, macro paths 15388, unresolved 0. Rows of 3254 samples: `through` 965 (`Sources::node`
  366, the rest the walk's own loops and its three vectors' growth), expand hook 872 (names
  library's), `definition_of` 377. libc 0.2.190's plan counts vary run to run (12667, 12678)
  while the totals do not: a nondeterminism to find.
- Tried and taken out (rule g): a hash index in `through` (p8, a loaded machine, load 16.8):
  `through` stayed 28.5% of samples (29.7% before) and the map itself cost 285+88 of its 1311.
  The walk's loops are not the cost; the number of walks is: member nodes 359754 over ~13k plans
  on libc 0.2.190, ~28 per walk, each walk from scratch. The floor needs a memo of a walk's
  value per (start, ns, name) that one touch reads: entries as a second waiter kind in
  `Lists` (`Waiter::Walk`), dropped and fired as `Key::Walk` when a key the walk touched fires,
  kept only for walks with no blame, no `pend` and no stop. Replaying the walk's touches on a hit
  would keep the parks (the `hash_one Key` row) and save only `node`.
- Core's 3082 relax rounds are its 3567 steps: each step expands a call, nothing waits, so
  `relax` lowers every flag and queues every pending item (~206). Re-queuing only items whose
  last relaxed evaluation's reads fired is NOT sound as it stands: a lookup stops at the first
  blocked candidate, so its read list can omit a flag its raised state depends on (S waits on A,
  A on S, both raised; C, which S would read with A lowered, is lowered by a change, and nothing
  wakes S; from the bottom S and A would be lowered). A sound incremental relax needs either
  read lists that cover the lowered evaluation (evaluate with the dirty set lowered first and
  park on what that reads) or DRed-style over-lowering by support, not by reads.
- Step 2 (record the rule as D56, NOT D55: D55 is taken; take the file's next free id and cite
  it in the stall rule's text). What the solver does today, read from lookup.rs (not yet
  tested case by case): an inner module miss while the module has unexpanded calls, or an
  unblocked pending glob, is `Res::Undet { weak: true }` (`globs_res`); `lexical_layers` goes
  on and an early scan returns the FIRST outer hit at once (`layer`, `Scan::Final` excepted),
  so the item is decided with the outer binding and, being decided and not via a glob, is never
  evaluated again: whether the final pass then reports the later inner definition depends on
  the final scan rerunning that lookup (`may_be_ambiguous` reads the outer layers only in
  `Scan::Final`, for a hit from an expansion not enclosing the use): check that first, and that
  the error lands at the new definition. The extern prelude: `extern_prelude_hit` (the unit's
  dependencies) answers before the root's macro-written `extern crate` is read, so a macro can
  add a name and never replace one (already the rule); an unknown name while the root has
  unexpanded calls is weak (goes outward). A pending glob making the miss weak is wider than the
  stated rule (expansion only): decide it in the same save set.

Step 1 (speed) begun; steps 2-4 not started. Runs in
/Volumes/Projects/blaze-int-1112-notes/port27/world-1116/ (`pN-libc.txt` with `pN-libc.sample.txt`,
a macOS `sample` of the run; `w10-t18.txt`). Profile rows: `python3 -I incl.py <sample> 60`
(inclusive samples per function; `incl.py` is kept beside the runs).
- Counters: `FillStats` gained touches/parks, relax_queued, stalls (items, edges), plan_imports/
  plan_globs; `FillReport.fill` sums them; blaze-world `names` prints "solver counters" for the
  five slowest crates.
- `--roots libc`, 1 thread: 13874.6 ms (p3) -> 13107.0 (p4: parks dedup by the list head, no
  sort of the read list; the sort row, 1877 samples, gone) -> 8224.2 (p5: `calls_state` counts
  up to two, so an expansion fires `CallsState` only when a `calls_but` answer can change; libc
  0.2.190 plans 66827 -> 12667, wakes 62296 -> 8030) -> 5028.2 (p6: `Fragment.textual`/`open`,
  `textual_back` passes over an expanded call whose fragment defines no textual name and waits on
  nothing; textual row 1734 -> 90 samples). Totals unchanged: imports 7058, unresolved 23, macro
  paths 15388, unresolved 0.
- Rows left (p6, 3812 samples): worklist 2123 (eval 1115, nearly all glob walks `through` from
  import evaluations: libc ~28 member nodes per plan); the expand hook 993 (`MacroDefinition::
  hash`, `rules`, `with_names` per CALL: resolve.rs, the names library's: tell it, cache by def);
  `definition_of` 445; outline 377. core: 3082 relax rounds, 636522 items queued (queue_all per
  round: the work list keyed by what changed is still owed).
- Whole graph, 18 threads (w10): "1022 crates resolved in 53461.9 ms; peak footprint so far
  1841.5 MB"; "unresolved imports 419", macro paths 100333, unresolved 13 (was 83475.4 ms, 1549).
  The slowest crate is 1978.7 ms, so the 53 s is not the solver per crate: profile the whole run
  first (critical path, proc macros). One-thread whole run not done. Target 18.3 s, floor 0.47 s:
  ratio 114.
- `per_crate` tests (names library's, written 05:51) fail 2 of 91 under `mixcheck test
  blaze-scope` (89 pass); not checked whether they fail without these changes.

### For a successor (name solver, runner after 05:25)

Saved, `lanecheck test blaze-scope: exit 0; 85 passed, 0 failed`. Runs in
/Volumes/Projects/blaze-int-1112-notes/port27/world-1116/ (x1-cs, x2-std, x3-std-nofix; r1-cs,
r2-cs are the same `--roots commonware-storage` command before the fix; nr.txt is the
Reference's name-resolution chapter as text).
- Step 1, ONE class, not two: a module other than the crate root, solved in a group without the
  root, read the extern prelude only from the CrateRoot record (built before expansion), so a
  root `extern crate` written by a macro (libc's `cfg_if!`, commonware's `stability_scope!`
  `alloc`) was NotFound there. Fix: `NameAnswer::Found` carries the binding's `origin`
  (read.rs; all constructors ported), and `extern_prelude_res` (lookup.rs) on a non-live root
  does a `Read::Name { root, Types, name }`: `Origin::ExternCrate` is a hit, any other answer
  a miss, no answer yet a Wait. Fixture `libc_shape_macro_written_extern_crate_and_modules`
  green. Same `--roots commonware-storage` command: total unresolved imports 1246 -> 169;
  commonware-storage 992 -> 0; std 108 -> 23 (31 before the regression). The glob-vs-glob
  lead was wrong: `glob_lookup` already compares the final def after `through` (lookup.rs, "if
  h0.def == hit.def"), and std's `libc::EBADF` rows went with this fix alone.
  (`--roots std` printed total 0 with and without the fix: that set does not show std's
  rows; do not use it as evidence.)
- Why the oracle missed it: the generator (tests/oracle.rs `draw`) makes ONE crate, paths begin
  only at `crate`/`self`/`super`, and `mk!` writes only `mod` items: no extern prelude, no
  dependency, no macro-written `extern crate`; and the reference has no extern-prelude step at
  all (the rule-(h) debt). OWED, first: a second generated crate, plain first segments, `mk!`
  that may write `extern crate dep as n;` at the root; the reference's extern prelude = deps +
  the root's final `extern crate` items; planted bug = this fix removed (`return Res::None` on
  a non-live root: measured to fail the fixture).
- The Reference today (nr.txt) states order stability ([names.resolution.expansion.
  expansion-order-stability]: "names in the fully expanded AST must resolve to the same
  definition regardless of the order in which macros are expanded and imports are resolved")
  and speculative early resolution re-validated after expansion
  ([..expansion.speculation]); glob-vs-glob allows "the same item (following reexports)";
  [..macros.ambiguity.more-expanded-vs-outer] is a heading with no text. Step 3's rule is not
  written there for imports or the extern prelude: it is blaze's rule (tell the coordinator).
- Not started: steps 2 (time), 3 (outward-lookup rule), 4 (stall notes), 5.

### For a successor (name solver, 05:25)

Saved, `mixcheck test blaze-scope: exit 0; 82 passed, 0 failed` (tree 8ed9c75cfb):
- Leak gone: `Resolver::edges`, kept current by `set_glob_source` (the one writer of a glob's
  source); `member_node` borrows it. `queue_all(ms, decided)`: a relax round no longer
  re-queues decided via-glob items (they wake by read list); only the planted flood passes true.
  FillStats: `member_nodes`, `member_edge_bytes`, `agains`, `agains_spared` (pipeline example).
- `$crate` of a crate not in deps -> `root_key(k)` (fixes tests/exports.rs).
- `extern_prelude_res` (lookup.rs): a miss reads the live root's own `Origin::ExternCrate`
  entry, else is a WEAK undet while the root (self or fellow) has unexpanded calls other than
  the asker. A finished root is the names library's half. NOT in the oracle yet: owed (rule h).
- `--roots libc`, 1 thread: before 17434.7 ms, peak 16532.4 MB (libc 0.2.190 8439.7 ms, core
  1041.5 ms); after 13553.2 ms, peak 305.5 MB (libc 6578.2, core 843.2). With the relax
  re-queue put back: 18523.4 ms, libc 8588.5: suspect 2 was real time. Tempo w9 (names lane):
  peak 1905.0 MB (was 20178.3) but 83475.4 ms, and unresolved imports 1549 (was 1038).
  Runs: `ns-libc-before.txt`, `ns-libc-after.txt`, `ns-libc-requeue.txt`, `ns-t2.txt` in
  /Volumes/Projects/blaze-int-1112-notes/port27/world-1116/ (beside w7..w9).
- OPEN: std unresolved 31 -> 108 (`use libc::EBADF` etc.) is NOT the re-queue change (put back,
  still 108). Lead: libc 0.2.191's root has `pub use new::*;` beside `pub use unix::*`-style
  globs; now that `new` resolves (derives find `core`), the two globs likely meet on one name:
  check the glob-vs-glob ambiguity compares the final def (a re-export of the same item is not
  ambiguous, RFC 1560). commonware-storage 17 -> 992 (`crate::merkle`) is probably the same.
- Not started: steps 2-6 of the brief (spared-member rule text, stall notes in `names`,
  DeriveHelpers, incremental rows).

### For a successor (name solver, 04:50)

**FIRST: a memory and time regression on tempo.** The whole graph at tree 79fe8a9b2c (18
threads) took 60070.7 ms and peaked at 14219.9 MB. An hour earlier it took 18.3 s and 3.5 GB.
libc 0.2.190 went from 6.4 s to 8.6 s. Other changes landed in that hour too, so the cause is
not known. The names lane is splitting it by suspect.

What I know of the rebuild's share:
- **Blame records** (`resolver.rs` `Blame`): one per pending item a lookup meets.
  - Each is a small enum: an `ItemKey` plus one or two `u32`s, so about 16–24 bytes. Its size
    has not been measured.
  - `Sink::blame` pushes ONLY when `scan == Scan::Stall`. Ordinary lookups record nothing.
  - `lookup_module` and `lexical` truncate it when the result is decided.
  - It is cleared per item inside `stall()`, and its vectors are freed when `stall()`
    returns.
- **`stall()`** runs once per quiet point, and only when nothing else moves. It evaluates
  each pending item once. It does not run per wake.
- **Suspect 1: `member_node` calls `f.glob_edges().leak()` on every call.** Since the rebuild,
  a fellow's pending globs no longer stop the walk: they only set `Live::pend`, and an open
  node no longer stops it either. So walks reach more members, and each reach leaks one edge
  vector into the arena for the whole fill. This is the first thing to measure.
- **Suspect 2: the via-glob re-solve.** A decided import or glob that read through globs is
  evaluated again on every wake of a key it read. Each evaluation parks it again (list
  nodes), and the walk is redone.
- **The intended shape:**
  - Blame is recorded only for a lookup that comes back undecided, as a fixed-size edge in a
    per-group table, freed when the group is solved.
  - The wait graph is built from those edges only at a stall.
  - Walks must not leak per call: borrow the member's edges, or cache them per member and
    drop them on a change.

The stall rebuild, as of 2026-11-16 04:50, tree 7bc371e5a6. blaze-scope checks. Tests: "72
passed, 6 failed".

**The rule** is scope lead 20 above. It is implemented in two places, plainly in the reference
(`src/tests/oracle.rs`, `Ref::lookup`, `try_import`, `textual`, `try_call`, `run`) and in the
solver as follows. An undecided item i waits on an undecided item j when deciding i reads
name n (in namespace ns) of module M, and one of these holds:
- n has no binding in M, and j is one of: a pending glob into M; a pending named import that
  can define n; a call of M not expanded yet. For a textual read, j is a pending call before
  i's place.
- n is bound in M only through globs, and j is a pending named import of n in M, or (outside
  the macro namespace) a call of M not expanded yet.
- Never a glob behind a glob once bound. A call never waits on itself.

A later glob that brings a different item makes the binding ambiguous. A decided import that
read it is solved again and fails as ambiguous. At a quiet point the sink SCCs of the wait
graph fail as NotFound, and the loop goes on.

**Where it lives in the solver.**
- `resolver.rs`:
  - `Blame` is an edge of the wait graph: `Import`, `Glob`, `Call`, or `Calls(m, except)`.
  - `Scan::Stall` replaces `Force`.
  - The sink keeps `blame` and `via_glob`; `Sink::blame` records only in a Stall scan.
  - `Import::via_glob` and `Glob::via_glob`.
  - Counters: `Resolver::blocked_calls` beside `unexpanded`. They are kept only through
    `set_call_state` and `set_call_blocked`. Every write of a call's state or blocked flag goes
    through these setters.
  - `fail_stalled(item)` fails one member of a sink.
- `lookup.rs`:
  - `calls_state()` returns the two counts (a change to either is a `CallsState` event).
    `calls_but(except)` is O(1). `asking_call` gives the call a lookup is made for.
  - `Asking::Call(c)`: `resolve_call` passes it, so a call does not wait on itself.
  - Blame is recorded in five places:
    - `blame_named` blames pending named imports.
    - `glob_lookup` blames a pending glob.
    - `globs_res` blames `Calls`.
    - `member_node` blames a fellow's pending globs and its calls.
    - `textual_back` and `derive_helper` blame a pending call.
  - `lookup_module` and `lexical` truncate blame when the result is decided.
  - `globs_res`: a binding found through globs waits on no glob, only on the calls (outside
    the macro namespace and unless restricted).
  - `Live` is `OPEN`. A fellow's pending globs go to `Live::pend` and decide only when nothing
    is found (`Live::settle`).
  - The textual scope's `Tex::Undet` is non-weak.
- `blaze-surface/src/through.rs`: `Sources::OPEN`.
- `work.rs`:
  - `stall()` re-evaluates every pending item in a Stall scan, maps blame to items, runs
    `components` (Tarjan, explicit stack), fails the sinks' members, then fires their keys.
  - `drain` evaluates a decided item again when it has `via_glob`. `ambiguous_again` moves it
    only to an ambiguity failure.
  - `run` is: drain, asks, waits/expansions/cycle, relax, `stall`, `finish`.
- Deleted: the forced pass, `Work::fail_pending`, `Resolver::fail_pending`, `Scan::Force`, and
  `Passes`/`Solver` (with watch.rs).
- Not done: the stall note naming the other members of the SCC (`ScopeError` in
  blaze-surface `module_scope.rs`).
- Not done: `via_glob` is not set on outcomes applied in `relax`.

**Update at 05:04: ALL GREEN.** "mixcheck test blaze-scope: exit 0; 80 passed, 0 failed"
(tree 08551d6fee). That run includes the oracle at 2,000 cases, the planted private-glob
control, all fixtures and the three property tests. The reference-side fix for case 1303
below is in. The tempo-after measurements and the regression above are still owed.

**Update at 05:00 (tree 923398e489): two tests fail, both the oracle.**

Fixed since the list below:
- The three property tests pass. Two fixes did it. `via_glob` is now sticky (set at every
  evaluation that read through globs, so outcomes from relax and stall are covered).
  `queue_all` also queues decided via-glob items. A third fix found what the property tests
  showed: a record not given yet (`Stop::Answer`) is never an open node; `Sources::open(stop)`
  replaces `OPEN`.
- The test harness (`tests/mod.rs` `entry`) no longer panics for a module whose declaring
  call failed.

A rule refinement, applied to both the solver and the reference: in a sink SCC, a member that
waits on ONE call of the same sink (a textual read past it, or a derive before it) is spared
that round. The call fails first and defines nothing (the Reference, textual scope), and text
order keeps a member to fail. This makes `a_failed_call_before_a_declaring_call_keeps_the_grandchild`
right: without it, `mx!` and `mk!` form one SCC and both fail. The coordinator has not
approved it yet.

**The oracle's first failing case: seed 1302, case 1303 of 2000.** Tape:
`2 0*11 2 0*5 1 2 0*8 1 0*4 2 0*2 1 0*5 2 0*5 2 0*3 1 2 1 0*6 1 1`. It prints with
`ORACLE_TAPE=... mixcheck.sh blaze-scope test "replay_one_tape -- --nocapture"`.

```rust
macro_rules! mk { ($($t:tt)*) => { $($t)* } }
mk! { mod m1 { type x = u8; #[macro_export] macro_rules! mx { () => {} } #[macro_export] macro_rules! my { () => {} } } }
mk! { mod m2 { type x = u8; use crate::my; use crate::*; my! {} } }
use crate::*; use crate::m2::*;
```

- The solver expands `my!`. The reference fails it as NotFound.
- The reference has `use crate::my` and `my!` in one SCC. The import's Types lookup reaches
  `m2` through the root's glob, and `m2` is open while `my!` is pending. The call waits on
  the import.
- The solver gives the import its Macros binding as soon as that namespace is found
  (per-namespace determinacy), and `my!` resolves.
- Judgment: the REFERENCE is wrong. An import's binding in one namespace is a fact once
  found. The fix in progress is `Ref` keeping per-namespace partial results of pending
  imports (`own` returns them), with `try_import` returning them beside its waits. An
  ambiguous namespace fails the import at once, as the solver does.

**The six failing tests at 04:50** (what each showed then):
- `oracle::a_cold_fill_equals_the_plain_reference` (2,000 cases): disagreement at
  oracle.rs:1216. The first case and its reduced program are still to be read.
- `oracle::a_glob_that_brings_in_private_items_is_caught`, the planted-bug control
  (oracle.rs:1235): the oracle does not catch the planted leak, so the control fails.
- `oracle::a_failed_call_before_a_declaring_call_keeps_the_grandchild`: panics at
  tests/mod.rs:366 ("a child's mod line is marked in its parent's scope"). The parent's scope
  record lacks the mark for a grandchild declared by a call after a failed call.
- `props::any_answer_order_and_batching_give_one_result` (props.rs:29),
  `props::waking_the_parked_gives_waking_everything` (props.rs:191) and
  `props::a_dropped_wake_is_caught` (props.rs:199) are the wake and order checks. Likely
  suspects: the `via_glob` re-solve (a decided item woken), and `CallsState` now firing on
  count changes.

**Owed measurements:**
- Tempo after the rebuild, with the same commands as the before numbers above: libc 0.2.190,
  core and the whole graph, on 1 and 18 threads, into `sl21-tempo-after-*.txt`.
- Then step 4: core's floor (2,529 imports, 5,350 macro paths), the profile, and the six
  incremental rows, none slower.

### For a successor (2026-11-16)

The names library, as of the hand-off.

**The seam.** It lives in `crates/blaze-scope/src/task/` and is in the tree; blaze-scope checks.

- `answer.rs`: `answer(read, records: &impl Records<N>, here, alloc) -> Result<(Own, Option<Note>), Lack>`.
  - `Records` has two methods: `held(key) -> Held::{Scope, Variants, Failed, Pending, Unknown}` and `parent(module)`.
  - `helpers_digest` is defined here.
- `solve.rs`: `solve::<S, N, F, R, X>(members, facts, records, expand: &impl Expand, here, alloc) -> Result<(Outcome::{Done(Vec<Resolved>), Need(Vec<Want::{Scope, Expansion}>)}, Tally), TaskError>`.
  - Every entry restarts from the top.
  - A read of a member is answered `InProgress`.
  - A cycle that names a held module outside the members is the typed error `TaskError::CycleOutside`.
- `fill.rs`: `Fill::new_in(alloc)`, then `.run::<S, _, _>(env: &impl FillEnv<N>, par: &impl Par, modules)`.
  - It runs waves of ready tasks.
  - A task that ends in `Need` waits on what it named, then re-enters.
  - When nothing is ready, each sink SCC of the wait graph becomes one group task, widened to the whole SCC at once.
  - `.report` holds R by both counts.
- `FillEnv` provides `facts`, `here`, `parent`, `outer` (enums and other crates), `made` and `Expand`.
- `examples/crates.rs` implements `FillEnv` as `FillCx`, with `Threads` as its `Par`.
  - `--loop fill|rounds` selects the loop.
  - Each crate prints `names digest <crate>: <hex>`, a digest of its scope records and read lists.

**Proof.** Hello set at 18 threads, all 46 crates' digests equal between the two loops:

| crate | digest | scopes |
|---|---|---|
| core | 555c4c382e1a0b56 | 257 |
| alloc | 42a792ee1a7a4ceb | 93 |
| std | cf34f87b8a06d449 | 144 |
| bstd | 94d134fdf5881843 | 106 |

Wall time is 1.85 s for the fill against 2.57 s for the rounds loop.

**R today misses 0.10:**

| crate | by entries | by steps |
|---|---|---|
| core | 0.580 | 0.168 |
| alloc | 0.733 | 0.590 |
| bstd | 0.581 | 0.378 |

**Decision on R (coordinator, 2026-11-16): the syntactic prefetch.** Do not order by a previous fill's wait graph: that is a heuristic cache that depends on a past schedule (rule 25).

- The wait graph comes from text, before the work. Once every module's declarations record and crate root exist (kind order), each module's `use` paths and globs name the modules whose scopes it will read: `crate::a::b`, `self`, `super`, an extern crate's root, a glob's source. Each is resolved along the module tree as far as the tree alone decides.
- First entries are ordered by that graph: a module's scope task is entered when those scopes are done.
- Each strongly connected component of the graph is one group task from the start.
- What stays dynamic (a path through a re-export, imports a macro writes) may still re-enter.
- Measure R again after this, by both counts. The threshold stays 0.10.

**`resolve_crates`, built (names-library lane, 2026-11-16).**
- **The entry:** `blaze_scope::resolve::resolve_crates(crates: &[CrateInput], sysroot, sources: &impl SourceTexts, par, alloc) -> Result<Names, NamesError>`.
  - `Names` implements `names::World`, so `names::path` returns `Resolved{def, consumed}`, the longest prefix that names something.
  - `Names` also gives `items`, `modules`, `place`, `outline_at` and `macro_src`.
- **The example:** examples/crates.rs now calls it, and its rounds loop is deleted.
  - 45 of 46 digests are equal.
  - std differs because the library now finds the macro-written `libunwind.rs` that the old example missed.
- **The ladder's hello world resolves.** "59 crates resolved in 8.859749209s" (Serial, Passes); `Cx`, `Io` and `Error` resolve to bstd's items, and `Result` and `Ok` to core's. Fixture: crates/blaze-ladder/tests/names.rs.
- **Where the time goes.** A `sample` over 7 s of that run puts 4,386 of 5,027 samples in the group fixpoint.
  - About 42% is `member_node` → `calls_state()`, which scans all of a module's calls on every glob member lookup.
  - The fix is counters on the Resolver; I sent it to the solver lane.
  - It runs on one thread, and the pack read takes 34 ms.

**Cfg of a crate (unit-graph lane, 2026-11-16).** `Unit::cfg` holds the platform atoms,
`feature="f"`, debug_assertions, test and proc_macro. It does not hold a build script's
`cargo::rustc-cfg` lines. Those come from blaze-build-script's outputs record after the script
runs. The cfg a crate resolves under is `unit.cfg` joined with that record, and the ModuleScope
key takes the joined digest (`task::Cfg::from_atoms(..).digest()`). Until the join is built,
units with `build_script: Some(_)` are counted apart: 106 of tempo's 1,263.

**Prefetch, built (names-library successor, 2026-11-16).** `task/prefetch.rs` (`prefetch(modules,
facts, alloc) -> Prefetch{components, reads, edges}`, the one Tarjan `sccs`); `Fill.first` is
`First::Prefetch` (default) or `First::Alone`; the example takes `--first alone`. Same tree, hello
set, 18 threads (scratchpad `s1-fill.txt` prefetch, `s2-alone.txt` alone):

| crate | alone: R entries / steps, failed | prefetch: R entries / steps, failed |
|---|---|---|
| core | 0.580 / 0.168, 154 | 0.043 / 0.014, 0 |
| alloc | 0.733 / 0.590, 0 | 0.000 / 0.000, 0 |
| bstd | 0.591 / 0.390, 0 | 0.000 / 0.000, 0 |
| std | 0.632 / 0.398, 38 | 0.392 / 0.314, 120 |

After the solver's one-level TextualEntry change, re-run on one tree (scratchpad
`r3-prefetch.txt`, `r3-alone.txt`). Every crate fails 0 in both modes, and the 46 `names digest`
lines are equal, so the prefetch does not change results.

| crate | alone: R entries / steps | prefetch: R entries / steps |
|---|---|---|
| core | 0.619 / 0.143 | 0.043 / 0.014 |
| alloc | 0.733 / 0.588 | 0 / 0 |
| bstd | 0.591 / 0.336 | 0 / 0 |
| std | 0.710 / 0.621 | 0.453 / 0.566 |
| libc | 0.630 / 0.700 | 0.614 / 0.699 |

std and libc are still over 0.10. The prefetch finds 7 components and 7 edges in libc. Its waits
look like items written by macros (`cfg_if!`), which a syntactic graph cannot see. That is not
traced yet.

- Wall 1.93 s and 233.9 MB alone; 3.88 s and 337.4 MB with the prefetch: core now solves its
  192-module group instead of refusing 154 of it (`GlobBug::NoFixpointRow`), item 7 profiles it.
- Digests differ between the two (core 440 scopes against 257); every crate reads core, so
  equality under regrouping is unproven until core refuses nothing in both. Open.
- What still waits: core 10 `Need`; std's 120 failed are item 2 above (cycles named on finished
  modules), not the prefetch. TextualEntry adds an edge to every ancestor: the large SCCs.
- Where the hello set's 3.88 s goes (per-crate column, outline to names, `s1-fill.txt` against
  `s2-alone.txt`): core 2,190.1 ms (130.1 ms alone, when it refused 154 modules), libc 1,277.3
  ms (804.8 alone; R by steps 0.931, its 20 failed modules redone), std 194.5, every other crate
  under 50. So 3.47 s of 3.88 is core's one 192-module group and libc's refusals: both the
  solver's group fixpoint (item 1 above, the rebuild on `through`), not lowering or the prefetch.
  A span profile inside the group solve is owed once that rebuild lands.
- `blaze_scope::names` (read side, compiles): `path`, `macro_path`, `traits_in_scope` over a
  `World` of finished records; `Why::NotRead` is "dependency source not read". Wording rows
  Cli 5 to 8 (unresolved, private, ambiguous import; `bstd` named `std`) are in blaze-report;
  Cli 9 is the toolchain lane's (`toolchain-crate-named`, the manifest line).
- Item 9's source of crates: not the example's library/ reading but the toolchain's shelf
  (crates/blaze-toolchain: `shelf::open_manifest` then `ToolchainView::open`, `view.units(triple)`
  gives each unit's features and deps by extern name, `shelf::open_pack` plus
  `blaze_fetch::PackView::open_verified` its files). A shelf cut from this tree:
  scratchpad `tc/shelf`, id 582bd43e...c63164. `plan_of`'s feature fixpoint goes when the
  example reads that.

**To do:**

1. Move the lowering into the library as `task/lower.rs`: `walk`, `Lower`, `crate_root`, `make_modules` and the cfg, with no file reading. The example's run state (expansions, macro and call sources, enums, made modules) becomes records keyed from text.
2. ~~Build the syntactic prefetch above.~~ Done, see above.
3. Driver side, agreed with the driver lane:
   - `crates/blaze-cli/src/names.rs`, with match arms `Kind::ModuleScope | Kind::Group | Kind::Expansion` in `work.rs::run`.
   - No new `Kind`.
   - Working memory in the per-task `scratch: &Frame`; copy only the record into `frame`.
   - `blaze-drive/src/scope_task.rs` becomes a thin call into the library, or is deleted.
4. Unresolved imports become finding records (rule 81): an item-relative range, the path, and the reason (not found, private, ambiguous).
5. Tempo runs on a copy under `/Volumes/Projects/blaze-tempo-1116/` (never `/Volumes/Projects/ref-tempo/tempo`), with time, peak memory and R for each run:
   - cold;
   - a repeat, which must do no work;
   - a function-body edit, after which names must not rerun;
   - a `pub fn` added to a glob-imported module, counting the records that rerun against the floor.
   Send the numbers to the tempo lane.
6. The one-call entry for the ladder lane:
   - `resolve_crates(crates: &[CrateInput], alloc) -> Result<Names, NamesError>`.
   - `CrateInput { name, edition, root, files: &[(path, text)], deps: &[(extern name, crate index)], cfg: &dyn blaze-lower Cfg }`.
   - `Names`:
     - `path(module, ns, segments) -> Result<Def, PathError{module, path}>`
     - `traits_in_scope(module)`
     - `place(key) -> Place{krate, file, ItemIndex, module_path}`
     - `key_at(krate, file, ItemIndex)`
     - `module_of(key)`: the enclosing module, for an item in a body or written by a macro
     - `ctor(key) -> Ctor{owner, index, kind}`
     - `root(krate)`
   - Preludes come from the `edition` crate.
7. Profile the hello slowdown: plus 1.2 s since 2026-11-15, with a profile that names where the time goes.
8. Delete `crates.rs`'s old rounds loop: `resolve`, `resolve_some`, `drive`, `sccs`, `DriveCx`, `Own`.
9. Take unit-graph's `Unit` into `graph.rs`: add a `cfg` field to `GraphCrate`, and delete the example's feature fixpoint in `plan_of`.
10. Follow up the `pipeline.rs` diff, which is with the solver lane. It is in the scratchpad as `pipeline-fill.diff`, and the solver lane applies it once its property test is green.

Two copies of the loop exist until items 8 and 10 are done.

**Failures in the tree, the solver lane's:**

- The glob fixpoint bound is overrun on 154 core and 38 std modules, in both loops ("a glob fixpoint changed past its bound"). It is absent from the 01:46 run (`run12.txt`), so it is probably new since the removal of `Undetermined`.
- libc's 20 modules: a `Step::Cycle` names finished modules (`cycle_of` counts unanswered reads).
- `tests::edits::edit_sequence_catches_kept_suspects` fails at seed 385.

**Evidence** is in `/private/tmp/claude-501/-Volumes-Projects-blaze/65dedbdf-09f9-429d-b209-308a28ee95a1/scratchpad/`:

- `old.txt`: the rounds loop.
- `new.txt`: the fill, with its `fill <crate>:` R lines.
- `pipeline-fill.diff`.
- `run10.txt` and `run12.txt`: the crate-graph lane's baselines.

**For a successor (names library, 2026-11-16 ~04:00, the lead after the hand-off above).**
Tests: "mixcheck test blaze-scope: exit 0; 64 passed, 0 failed" (blaze-core from its green
snapshot; it was broken underneath). New tests: `src/tests/entry.rs` (others add to it too),
`src/tests/items.rs`.
- Done, on `resolve::Names`: `path` and `item_at(module, ns, Written<&str>, alloc) ->
  Result<ItemAt{krate, def, macro_src}, PathError>` (segments of any lifetime: `Names` is
  covariant), `members(owner) -> &[task::Member]` (traits and impls, cfg applied, declaration
  order; keys equal the old assoc lang keys), `ctor(struct) -> Some(struct)` for tuple/unit
  structs (`tables.ctors`), `variants(enum)`, `traits_in_scope`, `macro_definition(module,
  shape, written, alloc) -> Result<MacroDefinition, MacroDefError>`; `definition_of` is the one
  home of a MacroSrc -> MacroDefinition (body lexed in the defining crate's edition). Told to the
  expansion lane, front2 (ladder) and its runner.
- Item-position expansion: derives run (`CallSrc.input` = the item's text; `expand_decls` finds
  the derive in `ItemAttrs::derives` by path; builtin derives via `builtin_derive`): test
  `a_builtin_derive_writes_its_impl`. Attribute macros: `Attrs::attr_macro` (lowering) is the
  outermost attribute that is not built in, not a tool's (`TOOLS`, `blaze` included) and not a
  single name after a derive (a helper); the item is then not declared, a `CallShape::Attr`
  call takes it, and `expand_decls` walks `ItemAttrs::next` to it and calls `Call::attr`.
  Blocked: every macro_rules binding is `Macro(FnLike)`, so `#[dep::wrap]` is `WrongMacroKind`
  (test `attribute_macros_take_their_item`, ignored). Agreed with the solver lane
  (ab0bcfea227e16d44): a bit set `MacroShapes` (FnLike|Attr|Derive) in blaze-surface carried by
  `DefKind::Macro`; THIS lane writes the surface + lowering half (shapes from the arms into
  `MacroRulesDecl`, `TextualDef`, CrateRoot `macro_exports`), the solver lane the lookup half
  (`kind_ok`, `textual_hit`, resolver.rs macro_exports) in one batch (rule 24). Not yet the
  hello-set proof (bstd `Placement`/`Reach` Clone impls; `bstd_hosted_start`) nor the
  example's derive/impl counts.
- Not started: the macro-written root `extern crate` into the extern prelude (test
  `libc_shape_macro_written_extern_crate_and_modules`, ignored; the solver lane says the fill
  must add the name before any non-root module reads it, non-root lookups Undet until known);
  proc-macro defs in `macro_definition` (needs `blaze_proc_macro::hook::crate_key`); call-site
  edition per call; build-script outputs into `Host` (lane a9a7de3afe1016690 sends a diff:
  `ScriptCx { inner: Host, out }` for `include!(concat!(env!("OUT_DIR"), ..))`).
- Step 3 (worklist default, threads): the solver lane says do not switch yet; Passes is not
  deleted, and the calls_state counters come after that. It will message.

**For a successor (names library, 2026-11-16 ~04:20).** Tests: "mixcheck test blaze-scope:
exit 0; 67 passed, 0 failed" (tree f2302b54e6); blaze-surface 22 passed.
- Done: `blaze_surface::MacroShapes` (a non-empty bit set of `CallShape`, one checked record
  byte; non-macros write `FN_LIKE`, agreed with the solver lane). `DefKind::Macro(MacroShapes)`;
  `TextualDef`, `MacroRulesDecl` and CrateRoot `macro_exports` (now `(name, key, shapes)`,
  `ExportRow`) carry it. The shapes are read from the arms by `task::rules_shapes(body,
  edition, alloc)` (lexed only when the text holds `attr`/`derive`); `LowerCx` gained
  `edition`. Tests in `src/tests/items.rs`: `attribute_macros_take_their_item` (un-ignored),
  `a_function_like_macro_used_as_an_attribute_is_a_finding` (a `ScopeError::Macro` with
  `WrongMacroKind` naming `dep::plain`, its call's shape `Attr`), `rule_shapes_are_read_from_the_arms`.
  Open, the solver lane's: lookup.rs `textual: call.kind == FnLike` keeps `#[m]` of a
  crate-local attr `macro_rules!` out of the textual scope; it will change that after Passes.
- Hello proof (scratchpad package `hello-entry`: the owner's main with `#[std::main]`; the
  owner's own main.rs has no attribute): `crates --threads 18 hello=<dir>` prints "hello `main`:
  made by an expansion true; holds `bstd_hosted_start` true" (nested in the new main's body, not
  a module item); "bstd `Placement`: 1 Clone impls", "bstd `Reach`: 1 Clone impls"; bstd derives
  applied/used "Clone 250/250, Debug 200/200, Eq 192/192, PartialEq 192/192, Copy 120/120, Hash
  28/28, Ord 18/18, PartialOrd 18/18, Default 11/11"; the whole set "Clone 1233/1234, Debug
  1001/1004, Copy 827/828, ..., Deserialize 0/7, Serialize 0/7"; total time 2800 ms on 18
  threads. The earlier use counts (Clone 523 ...) were not reproduced: they count something
  other than the bstd crate's or the set's derive calls (not traced). Ladder runner told.
- Build-script outputs (the coordinator's (C)): applied: `CrateInput.script:
  Option<OutputsRef>`, `Base.script`, `ScriptCx` at the expand call, `script: None` at every
  site (blaze-ladder, blaze-world, blaze-lower too). The cfg join was dropped: blaze-world
  joins a script's cfgs into `CrateInput.cfg`. blaze-world filling `script` is main's area.
- The solver lane is deleting Passes in resolve.rs, task/solve.rs, task/fill.rs,
  examples/crates.rs and workspace.rs (it asked; yes). Stay off those lines until it sends its
  mixcheck line.
- Next, in the coordinator's order (tempo, whole18b.txt): (A) declare proc macros from a
  proc-macro crate's outline (`#[proc_macro_derive(Name, attributes(..))]`,
  `#[proc_macro_attribute]`, `#[proc_macro]`) with their shapes and helpers, expanded through
  `ExpandCx::run_proc_macro` (lane a3c0fcccebe508508; `krate = hook::crate_key(module key)`);
  (B) the "Tempo misses from blaze-world" reproductions in this file, then twenty samples; (D)
  macro-written root `extern crate` into the extern prelude (libc 66, std 22). Also the
  coordinator wants the 5.1 GB tempo peak explained from a measurement.
- Tempo misses from blaze-world (adeffe8e234d87bf3, `resolve_crates` on 1,022 crates,
  crates/blaze-world/examples/names.rs), largest first, each with a repro in its message:
  (1) no proc macro is ever declared: the lowering never makes a macro def from a proc-macro
  crate's `#[proc_macro]`, `#[proc_macro_attribute]`, `#[proc_macro_derive(N)]` fns (3,967
  derives, 833 attributes, 55 bang calls, 73 re-export paths unresolved; items they hide unseen);
  (2) `#[macro_export(local_inner_macros)]` not honoured (lazy_static's `__lazy_static_internal!`);
  (3) `$crate::x!` in another crate's expansion gives NoCrate (fluent-bundle); (4) builtin
  derives not found inside a macro's output, and `cfg_if!` out of scope, in toolchain libc
  0.2.191 (0.2.190 resolves all), not reduced. Item 1 is the largest single miss on tempo.

**For a successor (names library, runner four, 2026-11-16 ~04:50).** All of this is uncommitted
in /Volumes/Projects/blaze-fresh-1115.
- Done, item 1 above: proc macros are declared and run. The lowering (`ProcAttr`,
  `Lower::proc_macro`) makes each proc fn a pub Macros def with its shape, named by the derive's
  first argument for a derive. `MacroBody::Proc{kind, helpers}` builds `MacroDefinition::proc_macro`
  from `CrateNames::proc_key`. The caller supplies the key and the runner through
  `SourceTexts::proc_macro_key` and `run_proc_macro`, both defaulted. The tests are
  src/tests/procs.rs ("3 passed"). blaze-world examples/names.rs takes `--procs <module store>
  --procs-out <dir>` (`procs_of`, built on `ProcMacros::add_reached`) and prints the notes by kind.
- Done: `#[macro_export]` macro_rules written by an expansion (tokio `select!`, alloy-primitives
  `address!`/`b256!`, tempo-hardfork's `tempo_post_genesis_hardforks!`) join the root's bindings
  and `crate_root.macro_exports` once the crate is done: `task/exports.rs`, test
  src/tests/exports.rs ("1 passed").
- Saved, NOT YET RUN: blaze-scope went red on the solver lane's in-flight lookup.rs/work.rs.
  (a) `head` reads a path with spaces between tokens (as expansion text has them), and
  `prelude_attr` maps `::core::prelude::v1::derive` (core/std/alloc, prelude, any module) to
  `derive`. Before this, every libc `s!` struct was DROPPED ("401 Lower AttrMacroNotLowered
  { attr: \"::\" }"), which explains libc `siginfo_t`/`timespec`/`sockaddr`. Test:
  src/tests/attrs.rs. (b) `FillEnv::derive_helpers` with FillCx's side; the solver lane does
  answer.rs.
- Tempo whole graph, 18 threads, `--procs --scripts bs/exported4` (the `bs/exported` dir is
  an old outputs layout and panics): "unresolved imports 1324, macro paths 91217, unresolved
  macro paths 69", plus "not asked: inside macro crates: ... imports 6163 (3716 not found)".
  Those 3,716 are not resolution failures: nothing asks for them, because the bootstrap tool
  compiles macro crates today. A macro crate's internals will be resolved when blaze compiles
  macro crates itself. Before step A: 10,306 and 5,115.
- Next, in order: rerun tempo after (a). Then the REAL MISS rows. evm2 `Evm` sits under
  `#[derive_where(Debug)]`, an attribute proc macro whose struct has field attributes
  `#[derive_where(skip)]`: check that the expansion declares it. alloy-provider `Provider`
  sits under `async_trait`/`auto_impl`. Also check commonware `spawn_cell`/`non_empty`.
  Then the ladder's report: in graph.rs `externs`, `not_self` silently drops `std` for a
  dependency of bstd (blaze_mem). Make that a typed note naming the crate, and ask the
  unit-graph owner which std bstd's dependency cone gets.

**For a successor (names library, runner five, 2026-11-16 ~05:10).** Uncommitted in
/Volumes/Projects/blaze-fresh-1115. Whole-graph runs: `MIXCHECK_RUN_LOCKED=1 mixcheck.sh
blaze-world run names /Volumes/Projects/blaze-tempo-1116/tempo $S/stores/s08-v3-all --threads 18
--crates --procs $S/pm/store-c --procs-out $S/procs-out-wN --scripts
/Volumes/Projects/blaze-int-1112-notes/port27/scripts-1116/exported5` ($S = this session's
scratchpad; outputs w7.txt, w8.txt copied to /Volumes/Projects/blaze-int-1112-notes/port27/world-1116/).
- Done: (a) libc `s!` structs lowered (runner four's attrs fix, now run); graph.rs `Externs::dropped`
  + `NamesNote::ExternDropped{krate,name,why: ExternGone}` (79 on tempo: `proc_macro` of
  proc-macro crates, NotInGraph); blaze_mem answer: Rust 2024, no no_std, `use std::ffi::OsStr`,
  must get Rust std; the drop was blaze-ladder crates.rs `Sysroot{std: None}` (ladder fixed it).
  task/exports.rs: `#[macro_export]` in a file of a macro-declared module reaches the root
  (commonware `spawn_cell`). resolve.rs `render_with(stream, here, dollar)`: `$crate` inside a
  written `macro_rules!`/`macro` definition kept as `__dollar_crate_<key>` also for the own
  crate, and a `$crate` with no mark is `here` (tokio `doc!{try_join}`). Fixtures in
  tests/exports.rs (3; the dollar one fails with the cause removed); graph.rs
  `a_dropped_implicit_crate_is_named`. task/lower.rs: a `#[proc_macro*]` fn under another attribute
  macro (alloy-sol-macro `#[proc_macro] #[proc_macro_error] fn sol`) is declared from its text and
  the attr call is not made: NOT YET BUILT OR RUN; it should cut the 534+306 "not written as an
  item" imports (sol! never expands); add a fixture in tests/procs.rs.
- Runs: w7 (tree 79fe8a9b2c) "unresolved imports 1039 ... unresolved macro paths 248", "resolved in
  60070.7 ms; peak footprint 14219.9 MB"; w8 (tree 2829a224ec, + blaze-expand's prelude-path derive)
  "unresolved imports 1038, macro paths 92826, unresolved macro paths 621", "70600.2 ms; peak
  20178.3 MB". The macro paths are libc 0.2.191's `::core::{clone::Clone,marker::Copy,fmt::Debug}`
  derives: libc is `#![no_core]` under rustc-dep-of-std and gets `core` only from
  `cfg_if!{ extern crate rustc_std_workspace_core as core; }`.
- REGRESSION, coordinator's top item, NOT TRACED: 18.3 s / 3.5 GB -> 60-70 s / 14-20 GB. Asked: trace,
  not fix; repro libc 0.2.190 alone (`--roots libc`, 6.4 s before, 8.6 s in w7); split by suspect
  with counters: (a) solver's stall rebuild (resolver.rs `Sink::blame`, wait graph; no counter is
  printed yet: add bytes/count of blame and wait edges to FillReport and print it in names.rs),
  (b) libc structs and derives now existing, (c) --procs/--scripts inputs held, (d) std in more
  graphs. Report the table to main; to the solver's lane once if (a) leads.
- The solver lane (handed on) gave two lookup.rs pieces to this lane (its note in this section):
  `PathKind::DollarCrate(k)` for a non-direct crate (needs a root lookup beside `deps`; fluent-bundle
  7, x509-cert, p256), and macro-written root `extern crate` into `Resolver::extern_prelude` with a
  `Key::ExternPrelude(root)` wake (libc: 54 imports, ~600 macro paths). Then step 3: sample thirty.

**For a successor (names library, runner six, 2026-11-16 ~05:25).** Uncommitted in
/Volumes/Projects/blaze-fresh-1115. Runs now in /Volumes/Projects/blaze-int-1112-notes/port27/world-1116/
(w7, w8, w9); `--roots` runs r1-cs.txt, r2-cs.txt in this session's scratchpad. Same command as
runner five, `--procs-out $S/procs-out-wN`.
- Seam agreed with the solver (a5d2d83e5b75ed405, now handing on): a macro-written root `extern
  crate X as N` is recorded as the root module's held Types entry with `Origin::ExternCrate`
  (resolver.rs add_fragment, unchanged); lookup.rs reads it on an extern-prelude miss and waits on
  the root's unexpanded calls. The solver saved it and the `$crate` fix (`start_module`:
  `DollarCrate(k)` is `root_key(k)` for any crate). Fixtures: tests/exports.rs
  `dollar_crate_of_a_crate_that_is_not_a_dependency_of_the_caller_is_the_defining_crate` (green);
  tests/procs.rs `a_proc_macro_under_another_attribute_macro_is_declared_and_runs` (green; fails
  with the lower.rs `proc_fn` filter removed: checked); tests/entry.rs
  `libc_shape_macro_written_extern_crate_and_modules` UN-IGNORED and still RED: from the
  macro-written `mod unix`, `use core::a::Kept` and `use ::core::a::Named` fail at segment 0
  NotFound (root's `use core::a::open` passes): the non-root half of the extern-prelude lookup
  is missing: solver's lookup.rs, for its successor. Test state: "82 passed, 1 failed" (that one).
- w9 (tree 8833c230a6): "1022 crates resolved in 83475.4 ms; ... peak footprint so far 1905.0 MB";
  "unresolved imports 1549, macro paths 100333, unresolved macro paths 13"; "not asked: inside
  macro crates: ... imports 6163 (3716 not found), macro paths 1181 (82 not found)". sol! now
  expands: "not written as an item" 534 -> 207 (dependency) and 306 -> 132 (this crate); libc
  0.2.191 macro paths 562 -> 0.
- REGRESSION, solver side, not mine: "commonware-storage 2026.10.0: ... unresolved 992" (w8: 17):
  `crate::merkle::Location`, `crate::qmdb::..`, `alloc::string::String` from submodules all NotFound
  at segment 0; its root items are written by `stability_scope!` (commonware-macros-impl
  `#[proc_macro]`, output `#[cfg(not(any(..)))] pub mod merkle;` etc.). Proven not the sol
  lowering change (r2: removed, still 992). Same shape as the red libc fixture: a submodule
  reading a root binding that a macro wrote. And std 31 -> 108 (`use libc::EBADF`, 74 rows): the
  solver's lead is the glob-vs-glob check now that libc's `pub use new::*` resolves.
- Next: when the solver's successor fixes the above, ONE whole-graph run, then step 4 (sample
  thirty unresolved imports across crates, classify before fixing). Ladder (a774482257e88a158)
  reported the same libc `::core` impl paths (171) and 14 `ToTokens`/`Parse` in proc-macro crates
  (host-side deps not held: the 3,716 "not asked" class).

**Per-crate entry (2026-11-16, runner seven; uncommitted).** `blaze_scope::crate_scope::resolve_crate(input,
deps: &[&[u8]], sysroot, sources, source, par, alloc) -> Result<CrateScope { names, record }, CrateScopeError>`.
`input.deps` and `sysroot` index `deps` (`CrateId(deps.len())`: the crate itself); `source` is its index for
`sources`. A dependent reads of a dependency only what resolve.rs's `Built` serves (now an enum `Dep`: a crate
finished in this process, or a record): key, root, edition, proc key; each module's finished scope and chain;
enum variants; macro sources. The `CrateScope` record (crate_scope.rs docs give the bytes): those, sorted by key,
each module's scope as blaze-surface's `ModuleScope` record in a 16-aligned blob, plus `reaches`: every other
crate its scopes name an item of, and what those reach. Transitive reads: the record names the target crate's
key; a dependent given a record whose `reaches` it lacks answers `Need(keys)` before any work, the driver adds
those records and enters again from the top (the library asks all at once). Parse step: the tables into hash
maps and each module scope `ScopeView::read` in place then `ScopeBuilder::from` (one pass over the bytes).
Private names are in the record with their visibility (a glob of a public module walks private modules); the
reader checks visibility. Unresolved imports and macro paths stay in each module scope's errors. A dependency
cycle cannot be given: the driver keys a dev-dependency's test unit apart. `resolve_by_records` drives a whole
set this way (a measurement, not the driver). Proof: tests/per_crate.rs (equal answers to `resolve_crates` on a
four-crate fixture with re-exports, globs, a macro crate and a script cfg; bytes equal across fill orders; the
planted bug "every binding written pub" caught; `Need` names the reached crate); `crates --per-crate hello`:
"47 crates ... crates differing from the whole set: 0; records 5270176 bytes", 1 vs 8 threads "records unequal
... 0", 7702 ms against the whole set's 7469 ms (1 thread). Owed: libc and tempo runs, body-edit and `pub fn` cost.

### Per crate, as built on tempo (2026-11-16)

- Two records per crate (crate_scope.rs `Records { exports, inner }`). `exports` is what another crate can read:
  `pub` names always; `pub(crate)` or restricted ones only where a 2.0 `macro`'s definition site sees them (a
  `macro_rules!` through `$crate` resolves with the caller's privacy, so only `pub`); a hidden name kept when a
  visible glob of its module may bring the same name (shadowing); only modules walkable from the root or a macro
  2.0's site; enums and macros named by kept names or `#[macro_export]`. `inner` is the rest: errors, textual
  scopes, private names, unwalked modules. A dependent reads and keys `exports` only, so a private edit changes no
  dependent's key (tests/per_crate.rs: a body edit changes no record; an added `pub fn` only its crate's exports;
  added private items only its inner). A path into another crate that stops at a name only `inner` holds reads as
  not found; OWED: the reporter says "private" from that crate's inner record.
- The key fix: an item's key once counted its outline position, so inserting any item re-keyed every later
  sibling and every dependent's record. `Lower::key(owner, tag, name)` counts only earlier entries of the same
  owner, kind and name.
- The ready queue (`Ready { workers, rank, wait, hooks }`): a crate is ready when its dependencies' `exports`
  exist and the sysroot crates before it in graph order are done (what makes records independent of order); N
  workers pull; a wait has a deadline (`CrateScopeError::Stalled`); a `Need` on a crate not waited on is
  `NeedNotWaitedOn`. Records are equal byte for byte across 1 and 18 workers and shuffled ranks.
- Include rules: `include!("x.rs")` of a package file is read relative to the calling file's folder inside the
  package, through `SourceTexts::read` (the sandbox rule: a crate sees its own package's files only);
  `include!(concat!(env!("OUT_DIR"), ..))` reads the build script's outputs record, never the disk.
- Member expansion: a macro call in an impl or trait body expands into members of that owner
  (`Fragment::members_of`, `Member::written_by`), joined in (written_by, item) order.
- `#[macro_export(local_inner_macros)]`: a single-name macro call in such a macro's body is rendered as
  `$crate::name!` of the defining crate (`PathScope::DefCrate`), as the Reference says.
- A proc-macro crate's exports come from its built module's list (`SourceTexts::proc_exports`: name, kind,
  helpers, what the module declares to the host); its text's `#[proc_macro*]` attributes are read only when no
  module is held (one finding per crate then). A derive declared through `synstructure::decl_derive!` is in the
  list and in no text.

## 7. Expansion

- **One pure function per call** (`blaze_expand::expand`): the definition and the call's tokens
  to an `Expansion`, or `NeedsExpanded` for an eager macro's nested calls. Every outside fact is
  asked through `ExpandCx` and recorded as a read (env, included files, cfg, positions inside
  eager input). `line!()`, `column!()`, `file!()` and `module_path!()` outside eager input are a
  placeholder and a `Hole`, filled from the outermost call when printed or lowered.
- **Hygiene** is by structure: a chain of marks over an edition root, in the record's own table,
  so `$crate` and def-site names resolve with no interned id.
- **Declarative macros** run on mbe's matcher and transcriber. The old layer is still underneath
  (mbe, tt, span, syntax-bridge, and through span `syntax` and `blaze-memo`): to cut.
- **Builtins** (derives and function-like macros) are written in blaze-expand, token to token.
- **Proc macros** compile to `wasm32-wasip1` and run in the process under a wasm runtime (rule 10;
  [proc-macros.md](proc-macros.md)). blaze-expand hands a `ProcCall` (kind, input, attribute
  arguments) to a runner it does not hold and takes back `Produced`: tokens, origins and the
  outside reads the sandbox recorded. The sandbox is the wasm module's WASI imports: no network,
  reads only through the runner, each one a read of the expansion's record. Decided (Andrew,
  2026-11-15 20:20): proc macros are the long tail, not a path to make fast. In order: (1) the
  derives that make up most calls in real workspaces (serde's, thiserror's, clap's, the std
  ones) are builtins in blaze-expand, used only when the macro crate's content digest is one
  the builtin was written against, so the result is the macro's own; (2) every other proc
  macro runs in the vendored Wasmtime (`vendor/wasmtime`, kept: no interpreter of our own is
  planned until a profile shows the runner is where the time goes); (3) an expansion is a
  record keyed by its input, so a call runs once and is shared; (4) a proc macro that
  compile-time reflection replaces is an advice finding naming the reflection form
  (rule 56), an error only in blaze's own crates. A workspace's dependencies still need
  (2): reth and tempo cannot be built without it (rule 4).
- **Build scripts** run sandboxed (rule 9) as a `ScriptRun` record ([target.md](target.md)):
  keyed by the script's program, its package, its declared environment and the settings, with its
  traced outside reads. blaze-build-script holds the environment cargo gives, the parse of what
  it prints and who receives it; constant-directive scripts are read, not run. Open: how the
  script's program runs with no `blaze build` (a `blaze run` of it inside the sandbox is the
  candidate), and the sandbox on macOS and Linux for a program placed by the launcher.

## 8. Signatures, impls, lang items, bodies

- **Signatures** are split records (table) so each is cut off on its own. Not built; the parked
  type checker answers `NotBuilt` for them on a crate without text (reconnect.md).
- **Impls.** "Which impls of T could apply to a type of shape S" (`blaze_impls::candidates`)
  reads, of each crate that can hold one, its `NoImplsOf`, and only where that does not rule the
  crate out, its directory and the one or two buckets it names: never a crate's whole table.
  Blanket impls are indexed under the trait. Every read, negative ones too, is handed to the
  caller's read list. **Lang items**: one record per crate, one binary search by name.
- **`BodyFacts`**: what a body gives others (hidden types, const values, discriminants), made by
  the body's task. The first stop of the reconnected checker on 2026-11-15 was a discriminant.
- **The type checker** (Andrew, 2026-11-15: "feel free to remake type checker from scratch in
  ideal one ... our best design from scratch will be better as fewer incremental issues").
  Kept: the type tables, the Rust project's next-generation trait solver, MIR building and the
  borrow checker. Written new: the per-body driver, `crates/blaze-typeck` ([typeck.md](typeck.md)):
  one body in, one record out, every outside fact read through a sealed recorder so a result
  cannot depend on what its read list does not hold, the walk an explicit work stack in rustc's
  order of inference (which is the specification). The old driver (hir-ty's `infer` and
  `method_resolution`) is the referee: both give the same types for every body of real `core`,
  `alloc` and `std` before the new one replaces it. Until then the parked hir-ty asks the new
  records through base-db's facade, unchanged at its call sites ([reconnect.md](reconnect.md)).

## 9. The driver

One loop per command, a sans-IO machine under a thin driver:

1. **Request.** The command's question: check these crates; run this entry; run these tests.
2. **Certificate.** One record for the request: its key is the tool and the request's shape (the
   config and arguments, the checkout's directory as a marker); its read list is what the last
   answer read (each file by content id under its stat stamp, the variables read, files outside
   the checkout); its answer is the keys of the findings and the program manifest. Checked in the
   command's own process, before anything else is loaded; when it stands, `check` prints the
   stored findings and exits, `run` places the manifest. Replaced: certificate.md's request
   certificate "checked by the daemon's loop", because no daemon exists (rule 70).
3. **Stat sweep.** The OS's journal by a stored event id where it keeps one (FSEvents), else a
   stat of every file against one small table of stamps; a file is hashed only when its stat
   changed or its stamp is inside the racy window (from Relax). Open: the window's length.
4. **Tasks, forward from what changed**: file items, declarations, scopes with expansion,
   signatures, the checker, then, for `run`, code. Each task is a pure function that returns its
   record or `Need(keys)`; it is entered with the needs its last record remembered.
5. **The shared flight.** The driver keeps one table of keys being made: a key is made once
   however many tasks need it; a task with needs is `Blocked{left}`, holds no worker, and is
   entered again from the top when it lacks nothing (run.md's Parent does exactly this for
   compiles). Every wait has a deadline (rule 18). The parked checker raises a `Need` by
   unwinding with a typed payload (reconnect.md); new code returns `Outcome::Need` (typeck.md).
6. **Scheduling.** Workers start in proportion to queued work; a no-op or one-body edit runs on
   the main thread ([target.md](target.md)). Each worker has a frame, reset per task.

Open (regret 2): a cold body with k unknowns found one after another runs k times. Test: serial
`Need` rounds per body on std with an empty store; if they cost, a stopped run saves its state.

## 10. Storage

- **One store** ([target.md](target.md)): immutable segments and a generation manifest, mapped
  once per process, read with no lock; each process writes its own segment; collection is
  compaction under a byte cap then unlink. A "pack" here is target.md's segment (local file and
  wire form); store.md's packs are the older store it replaces; blaze-format is each record's
  byte form; blaze-pack is the signed transfer pack (sorted manifest, per-record BLAKE3).
- **Reads**: small records by `pread` into the task's frame, large arrays mapped; verified once
  per fetch or boot. **Remote**: a pack's index once, then byte ranges in parallel; one format
  for R2 and a peer.
- **Restart checks.** A read-addressed key already carries the digests of what its record
  read, so a lookup that finds the key has checked the reads: the index holds no second
  digest (it would grow each entry from 48 to 64 bytes for nothing). What made a restart slow
  was elsewhere and is changed ([store-next.md](store-next.md)): a segment is sealed once
  (store format 4), so the first open after a boot verifies only the unsealed tail, not
  every byte; the open reads no index page ahead of need; a lookup reads no page beyond the
  one its probe lands on. Floor, from reading the code, not yet measured: about 6 pages for
  a repeat check of hello-world and 45 for reth; after the changes at most about 6 times the
  floor on hello-world and 1.7 on reth, the gap being lookups that miss in older segments
  (a filter per segment would close it; not built). Test: count the pages a repeat check
  touches after a restart, on hello-world and reth.
- **Trust.** A key names inputs, not a value, so only a statement signed by a trusted builder (CI,
  later the remote workers) binds a key to a digest; the signature, digest and size limits are
  checked before a byte is mapped. A developer's machine never writes a shared namespace; private
  code, read lists and names never leave their organisation's namespace.
- **std's records** are built by CI, per toolchain, target, std features and profile settings,
  and published as a signed bundle. Offline, or with no bundle for the target, std is filled
  locally like any dependency, and the command says so by name (rule 3: said, not quiet).

## 11. Commands

Andrew, 2026-11-15 18:45: "no blaze build and just blaze export ... and blaze run / test / check
that are fast"; "everything in JIT world". The commands are `run`, `test`, `check`, and later
`export`. Detail: [run.md](run.md).

- **`blaze run` and `blaze test`** never write an executable. Code is records; the launcher places
  them in the program's own process behind one slot word a function. A function not yet compiled
  is a stub: its first call asks the run's parent for the code record (found by key, or made
  then), which is published in the slot; what is likely to run next is compiled behind it. No
  link step. A test run is a program run. **`blaze check`** makes no code.
- **A run is a check too** (Andrew, 2026-11-15 20:43: "by default blaze run would fail if any
  blaze check does not work ... so dont need separate check that you would infrequently run").
  `blaze run` and `blaze test` fail when the check of their program fails, lints included, so
  no tree is ever "green but unchecked". Scope: the workspace packages the program's package
  depends on, whole (not only what the run reaches; not the rest of a monorepo, which
  `blaze check` and CI cover). The mechanism is the coordinator's, to be confirmed by Andrew:
  (1) a finding already stored from an earlier command costs nothing to know, so a tree known
  bad never starts; (2) otherwise the program starts at once, and the rest of the check runs
  beside it on the cores it leaves free; code is still checked before it first runs, item by
  item; (3) the first error found stops the program, prints the findings and fails the
  command, and a run that ends first waits for the check before it reports success: a test's
  pass is not printed as a pass until then. The cost is in proportion to the edit: findings
  are records, an unchanged item's are replayed, and on a fresh clone the proofs come from
  the shared cache, so only the diff is checked. `--reached` is the opt-out for the middle of
  a refactor (check only what runs), and it says so in its verdict. The plain alternative,
  one flag away, is to finish the check before the program starts.
- **One check covers every configuration** (Andrew, 2026-11-15 20:49: "check will test all
  possible configurations combinations ... instead of having to run multiple times for
  different targets"). A configuration is a target (which fixes every `target_*` value), a
  set of features per package (any set closed under the manifest's feature dependencies),
  and the build's switches (`test`, `debug_assertions`, `--cfg` flags). The design already
  holds what this needs: the outline keeps `#[cfg]` as an attribute instead of deciding it,
  an item's key holds no cfg, and every record's read list names the cfg outcomes it read.
  So the mechanism is the driver's ask-and-re-enter with one more kind of need
  (the coordinator's, to be built): a task runs under a *condition*, a partial assignment of
  cfg atoms, starting from "anything the request allows"; when it reads an atom its
  condition does not fix, it returns `Need(Cfg(atom))` and the driver re-enters it once per
  outcome the request still allows, each under the narrower condition; its record is stored
  with the condition it actually read, and a lookup takes a record whose condition the
  asker's implies. An item that reads no cfg is checked once for every configuration; one
  that reads two atoms is checked at most four times; the cost of N configurations is one
  check plus the differences, not N checks. A finding carries its condition, reduced to the
  atoms that matter: "when feature `x` is off on windows". Bounds and holes, stated:
  (1) atoms of one variable are one choice (`target_os` has one value), so targets multiply
  by their count, not by two per atom; the targets are a list the workspace declares
  (default: the three hosts in scope); (2) an item whose reads would fork past a cap
  (64 conditions) is a typed finding naming the item and its atoms, never a silent sample;
  (3) a build script can set cfgs, so it runs per target and its outputs are reads;
  (4) another target's `std` is needed as records, from the shared cache; (5) layout
  differs by target, so constant evaluation that reads a size reads the target; (6) this is
  a check: code is made and tests run only for the host. `blaze run`'s gate covers the
  configuration being run by default and every configuration with `check`; whether the gate
  should cover all is open.
- **`blaze export`** (later) writes one native executable (Mach-O, ELF): every function the
  program reaches, at the optimized tier, no stubs, laid out as the launcher lays them out,
  written once, whole. Not incremental; keeps nothing. Until built, the command says it is not.
- **Gone:** `blaze build`; the linker's patching of a file in place (kept slots in a file, spare
  room, growth bounds, patched unwind tables, a signature made again per edit); compiling
  everything reachable before the first instruction runs.
- **Tiers.** A function's first code is the fast tier, which calls through slots. A background
  tier optimizes hot functions across functions (inlining); its record lists, in its read list,
  the code identity of every callee body it inlined, so an edit of a callee makes the caller's
  optimized record stale and the caller runs its fast code until the tier makes it again. A
  better version is one slot word written.
- **Hot counters.** Fast code's prologue adds one to its slot's counter; the control thread
  samples the counters and, past a bound, queues the function for the optimizing tier, once per
  run. No profiler is needed. Replaced: "a function's stub counts its calls", because a stub runs
  once. Open (run.md 1): the prologue's add against sampling the program counter.
- **Decide at run time** (Andrew: "this is a JIT! ideally these things could be toggled
  dynamically at runtime"). A slot can hold another version of the same function at any moment:
  tracing a function or module, recording effects, exact counts and times, a breakpoint that
  parks the thread with its stack, coverage, allocation tracking, extra checks (overflow, bounds,
  contracts) on one function. The plain version tests no flag. A version is a code record keyed
  by the function and its set of hooks, made once and shared. A build-time switch is the
  exception and says why. What stays at compile time is what decides whether the program is
  right: types, borrows, capabilities. A toggle is a request to a live run from a paired viewer.
- **Optimizing from what was observed** (a `dyn` call's real target under a guard, real counts
  for layout, argument values a function keeps getting): the observed facts go into the
  version's key, the version runs behind a guard that leaves for the plain version when the fact
  does not hold, and such records stay local: never published to a shared namespace. Replaced:
  "the optimizing tier works from what was observed" with no word on keys, because a record that
  depends on facts outside its key breaks rule 25 and run.md's optimized key. Open (regret 3):
  measure its speedup over the plain optimized tier on reth before building it.
- **Tests that pick themselves**: section 1 ([test-selection.md](test-selection.md)).
  **Discovery**: a test is an item whose attributes hold `#[test]` once cfg and expansion are
  done (an attribute macro can make one); each module's tests are read from its `ModuleDecls`
  and expansions, not by walking the crate. The harness entry is part of the test run's program
  manifest, made by blaze, calling each selected test (run.md, `Tests`). Open: libtest's command
  line and output, `harness = false`, and doctests ([doctests.md](doctests.md)).
- **Debug info for a run.** Each code record carries its unwind table and symbol; the launcher
  registers placed code as one more image so the program's own backtrace finds names
  (blaze-launch-rt's `unwind` and `symbols`). Open: registration with a debugger (lldb, gdb) for
  code placed while the program runs, and `export`'s side file ([debug-side-file.md](debug-side-file.md)).

## 12. Diagnostics and lints

- **A finding is a record** (rule 81), keyed like its check, with no level and an item-relative
  range. A user's error is a value in the record, never a failure of the task (typeck.md).
- **Printing.** A range is placed through today's `FileItems` (the item's start) and the file's
  line table; `file:line:col` exists only on the terminal. Open: whether the line table is a
  column of `FileItems` (keyed by the same bytes, so printing after a restart needs no read of
  the source) or made from the text when printed.
- **The early record** (from Relax): a check whose certificate stands prints its stored findings
  and exits before anything is loaded. **Lints are rebuilt** as per-item tasks on the new layer. An edit re-runs the lints of the items
  whose facts changed and no others; a lint over a whole crate or program reads per-item summary
  records. The old lints were deleted on 2026-11-15. The short list first: `Send` across a wait
  (section 13; the first rebuilt), string errors, implied allocators, dropped errors, ambient
  reads, unnamed or unbounded waits, map-order output, a key ignoring an input, `todo!`.
- **Relax's failures as a checklist.** Its `KNOWN_GAPS.md` and tests (MIT) are the list of Rust
  that trips a front end written from scratch; each becomes a test here.

## 13. Code rules

**Zero third-party crates** (Andrew, 2026-11-15: "we should have 0 third party and all the things
you said worth adding"). The end state: every crate the `blaze` binary links depends only on
crates of this workspace. A gate ratchet (scripts/src/bin/thirdparty.rs, its list
tools/ratchets/third-party.txt) holds the count per crate and lets it only fall; a new crate may
depend only on workspace crates already at zero. Measured on main after the delete, 2026-11-15:
413 third-party packages in the lock graph, 102 used directly (serde by 25 crates, serde_json 16,
tracing 14, libc 12, crossbeam-channel 10); `blaze-id` pulled 58 through `blaze-ids` and
`blaze-hash` (blake3, hashbrown, crossbeam, futures). Since then blaze-blake3 and blaze-swiss
replace blake3 and the probe; hashbrown is the one edge left under blaze-id ([third-party.md](../third-party.md)).

The exceptions, each dated 2026-11-15:

| Exception | What it is | How it ends |
|---|---|---|
| The trait solver (`vendor/rustc_next_trait_solver`, `rustc_type_ir` and their small dependencies: arrayvec, bitflags, derive-where, ena, indexmap, ra-ap-rustc_index, rustc-hash, smallvec, thin-vec, tracing) | a fork of the Rust project's solver | becomes ours by stripping its dependencies and marking it `ours = true`; it is NOT rewritten from a blank page |
| Wasmtime (`vendor/wasmtime`) | the proc-macro runner (rule 10) | stays vendored (Andrew, 2026-11-15 20:20: vendoring is fine for the hard ones); replaced only if a profile names it |
| Proc-macro modules from a bootstrap builder (2026-11-16, D52; rule 1's named interim exception) | `scripts/src/bin/proc_macro_modules.rs` builds each proc-macro crate of a lock, unchanged, for `wasm32-wasip1` with the pinned nightly's rustc (`-Zwasm-proc-macros`), build scripts sandboxed, paths remapped, and writes module records; never the `blaze` binary, never spawned by it. Owner's word: allow proc macros so real projects compile unchanged | ends when blaze compiles syn to wasm; a module blaze builds is another key and supersedes by decision |
| zstd (`vendor/zstd-sys`, C) | compression of stored records | stays vendored; first measure whether records should be compressed at all |
| LLVM | the optimizing backend | the Rust bindings go (declared `extern "C"` from LLVM's headers); LLVM stays a C++ library until blaze's own backend is the top tier |
| What third-party.md marks hard | TLS and signatures, zstd, the profile's file format, tree-sitter, Cargo's formats | the option third-party.md names for each |
| `ring` for signing (2026-11-16) | blaze-remote's Ed25519 signer (builder and anchor seeds) and SigV4's SHA-256 and HMAC, linked only under its `sign` feature (and `bucket`); verification is blaze-ed25519 in every shape, and the `blaze` binary links no `ring` (remote-read.md, "Ed25519, written here") | signing stays on `ring`: code holding a secret needs constant time, and blaze writes no signer; SigV4 moves to `crates/blaze-sha2` |

**No `std` and no `alloc` in blaze's own code** (Andrew: "can we not even depend on the std library
for anything here? our own std library ... blaze-std"; "no alloc! we should have our own").
`core` stays: it is the language. `std` is replaced by bstd (`crates/blaze-std`), split so rustc
builds its base and blaze adds what needs its extensions under `cfg(blaze)`
([std.md](std.md#two-builders)). `alloc` is replaced by collections of our own (crates/blaze-own:
`Vec`, `Box`, `Arc`, `Rc`, `VecDeque`, sorted maps, `Cow`, the column vector `Soa`) with no
default allocator: `new_in(alloc)` is the only constructor, so rule 11 is held by the types.
bstd's entry installs the global allocator. Nightly features are allowed for it:
`coerce_unsized`, `dispatch_from_dyn`, `arbitrary_self_types`, `deref_patterns`,
`dropck_eyepatch`, `min_specialization`. Known cost: the compiler's own `Box` moves out with `*b`;
ours uses a method.

The interim, replaced crate by crate: under D25 (2026-11-11), while cargo builds blaze a new crate
may sit on `core` and `alloc` and says so in its header (blaze-id, blaze-scope, blaze-impls and
blaze-run name `alloc::vec::Vec` today; they move to blaze-own); blaze-surface is on `std` because
blaze-format's `record!` names `::std::vec::Vec`, blaze-file-items because the parser is on `std`,
and each turns `no_std` when its base does. Parked crates are moved by astlog rules once blaze
checks itself. Replaced: "a new crate is `#![no_std]` with no `extern crate alloc`", because D25
and the code hold this interim.

**Layout: columns** (Andrew: "use soa as much as possible ... and other things that will
optimize"). A record, and an in-memory table of more than a few rows, is a struct of arrays: one
packed column per field, one length, one buffer. Rows of structs only where every access reads
the whole row. With it: fields read on every lookup apart from fields read rarely; a `u32` index
into a column, never a pointer; a sorted column with binary search or a perfect hash, never a
linked map; a bitset for a yes-or-no fact; variable data in one side table by offset; no
allocation per item; LEB128 for positions. The in-memory container is `blaze_own::Soa`, declared
by `soa!`. The type checker's own trees are not reshaped by this rule; that is done where a
profile says it pays.

**Cleanup on every exit is `core::mem::DropGuard`** (stable in the pinned library; Andrew: "like
zig's drop thing"): `new(value, |v| ..)` runs the closure on every exit path, `dismiss(guard)`
gives the value back and runs nothing. It replaces a hand-written `Drop` struct, for cleanup that
cannot fail or wait; what must finish with a result or a deadline is a linear type
(`#[blaze::linear]`) with `finish(cx)`. `async_drop` is not used.

**Fibers and `Send`** (Andrew: "wont we not have proper protection against using things like ...
Rc"). Fibers are bstd's convenience for sequential code of an ordinary program (no `async`, one
kind of function), until blaze generates the state machine from straight-line code; the
compiler's own tasks do not use them (section 2). Rust checks `Send` across an `.await`, not
across a fiber's wait, so a fiber holding an `Rc`, a lock guard or a thread-local borrow that
wakes on another thread is a data race rustc accepts. So: built by rustc, a fiber of bstd never
changes its thread after it starts (new tasks are spread, a started one stays), which is sound
unchecked; built by blaze, "a value that is not `Send` is live across a call that can wait" is a
compile error (rule 27; std.md's "What the compiler checks"), and a fiber may move. That check is
the first lint rebuilt. bstd has no thread-local storage; its one-thread types check in debug
builds that they end on the thread that made them.

## 14. Observability

- **Every run answers**: it registers (run.md's `Registry`) and answers on a local socket; the
  loader is ours, so a run can pause on a panic with its stack, say why it hangs, sample itself.
- **The viewer** (Andrew: "doing separate blaze view seems annoying ... ix.dev thing show
  automatically whatever is running or building"). No command of its own: any blaze command
  starts it when nothing listens on its fixed local port. It lists live runs, serves their events
  and the profile history, never builds, and holds nothing a record does not. It exits a fixed
  time after the last registered run ended; a connected page does not keep it alive (rules 70 and
  18). Replaced: "it ends when no blaze process is alive and no page is connected", because an
  open tab would keep it alive with no expiry. Open: the length of the expiry.
- **The page and privacy.** The page is hosted on ix.dev. The viewer answers only that origin,
  read-only, after a browser was paired once (the page shows a code, the next blaze command asks).
  Data goes to the browser and no further. Because the browser runs the page's script against
  local data, the script must be fixed by the blaze release: pinned by digest, or served by the
  viewer itself. Open: which of the two.
  Toggles (section 11) are a permitted action of a paired browser only, sent to the run's parent.
- **Many machines** (Andrew: "a monitor of what might be happening even on multiple machines").
  The events (a command began or ended, what it runs, compiles or tests, on which machine and
  checkout, time and memory) are one stream. When an organisation opts in, the viewer also sends
  it to a relay on ix.dev tied to an ix account, and the page shows every machine; machines make
  outgoing connections only. Off by default; activity only, not code or item names, unless chosen.
  Makepad's devhub (warm machines per agent, a warm cache at the tip) is the comparison.
- **Every effect is an event** (Andrew: "it can log the system calls ... and where they came from
  ... in the panel and in data"). In bstd a program reaches the system only through `Io`; each
  call is one `Effect` given to the one driver under a `Cx` that knows its task and span. Effect
  logging is a hooked version of the `Io` entry functions (section 11), so with the hook off the
  entry runs its plain code. Logged per effect: its kind and argument sizes, when it was asked,
  started and done, its outcome, the task and span, and the call site. Argument values (paths,
  hosts, bytes) are kept only when asked and never leave the machine without the opt-in. The
  simulation implements the same driver, so a test can state what a function may do ("no
  network", "writes one file"). A program on Rust's own `std` does not pass through it; since
  blaze compiles `std` itself, its system layer can carry the same hook: later. Replaced: "always
  on" and "off costs nothing" inside the driver, because recording in the driver costs a test on
  every effect. Open: the call site by `#[track_caller]` passes a hidden argument; its cost is
  untested (a microbenchmark), and the hooked version may take the site from its return address
  instead.

## 15. Relax

Relax (makepad, MIT; read at commit f05459f on 2026-11-15, source only) is the compiler this one
is judged against. What its code does:

- **Names**: eager collection, an import fixpoint over the whole program's import list
  (`front/src/program.rs`), lookups memoised per (module, name, namespace), and every binding
  change clears the memos of the whole program: it resolves whole, as rustc does. It keeps a full
  AST of every file for the process's life; positions are absolute byte offsets, so a line
  inserted above a function changes that function's code key.
- **Cache**: one mmap'd file per crate version (a header, a sorted index, append-only payloads of
  raw in-memory structs, readable only by the same build), keyed by absolute worktree paths: not
  shareable; std is compiled per workspace. `check`'s verdict is per crate, keyed by every file's
  text; a signature edit drops every body verdict of the crate; a body with a warning is never
  stored.
- **Its speed** comes from elsewhere: compile on first call (stubs), a warm start that maps the
  resolved program in place after a stat-only certificate, stored diagnostics printed with no
  load, and no link step.

Taken: compile on first call (section 11; the largest part of its lead on time to first run);
the early record (12); stat stamps with a racy window (9); one mapped pack per crate with a sorted
index (10); packed tokens and LEB128 position tables, bodies skipped at load (5); a key that
covers what a name can find (its per-item name graph with a Merkle hash per strongly connected
set is the per-name read list; blanket impls indexed under the trait); its known gaps as tests.
Not taken: absolute positions and paths in keys, crate-grain verdicts, raw in-memory structs as
the stored form, whole-program memo flushes, ASTs kept alive, dependencies read from source.

### More taken from Relax (second survey, 2026-11-15 20:00; file names are Relax's)

Adopt, by value to the goal order:

| What | Why | Size | Where in Relax |
|---|---|---|---|
| Relocated fields emitted as zeros, filled at install | code bytes do not depend on their address, so a code record is shareable and cacheable | small | backend rir.rs:542-760 |
| Slot table: thunk, lazy stub, table word, tier record per id; a thunk head patched to a direct jump when in reach | first-call slots and hooks | medium | rir.rs:588; jit.rs:19596 |
| Store file: two index slots with a header flip, append-only payloads, a new generation per rewrite, never renamed over | crash safety and restart | medium | compiler store.rs:1-38 |
| Startup image: what ran before the first frame last time, relocations against identity tables, linked in one pass | warm restart | medium | cache/image.rs |
| A backend memo keyed up to renaming of slots: generic instances that differ only in callees share one codegen result | cold time, memory | medium | bmemo.rs |
| Two views of the same pages for W^X (memfd on Linux, `mach_vm_remap` on macOS), not `MAP_JIT`: a program's own `pthread_jit_write_protect_np` flips every `MAP_JIT` page of its thread | correctness | small | os/mem.rs:301, 672 |
| An entry-mark thunk in the table word, a byte in a per-thread bitmap: which functions a test entered, at no cost outside live tests | incremental tests | small | harness_reach.rs |
| Invalidated code in page-aligned groups set `PROT_NONE`, addresses never reused; a stale access parks its thread | safety of retirement | medium | invalid.rs |
| Hooks (count, break, heap) as a patch group whose removal rolls back with no recompile | decide at run time | medium | instrument.rs; heap.rs |
| Channels with credit flow control, a replay ring, a hub that reopens feeds by sequence number, an ssh relay | the monitor and the relay | medium | libs/remote |
| A flight recorder of lock-free per-thread rings; a stack sampled once a wait passes a bound | the monitor | medium | flightrec.rs |
| A fixed-table wait registry written only on blocking paths: why a program hangs | tooling | small | base waits.rs; hang.rs |
| An unwind trailer per function instead of DWARF | cold time | medium | rir.rs:2340 |
| A one-pass baseline register allocator and assembler for the fast tier | cold time | medium | regalloc.rs:3085 |
| A process-wide CPU budget so a person's work never waits behind background builds | the edit loop on a Mac | small | libs/cpu_budget |
| Random IR programs run through every tier and compared with an evaluator | correctness of tiers | medium | codegen tests/fuzz_tiers.rs |

Also ready to adapt for "no third-party crates" (MIT, with sizes): TOML (2.8k lines), a small
serializer (4.5k), strict JSON (0.8k), base64, inflate and deflate (5k, a miniz port), sha256,
tar, an HTTP client over the OS's TLS (1k), git over smart HTTP (2.3k), an ELF/Mach-O/DWARF
reader (1.9k), disassemblers for x64 and arm64 (5.7k).

Not taken: a non-SSA IR for the top tier (their own list of why their code is slower starts
with register copies); one owner thread that every change goes through (their waits of
374 to 583 ms; against the 128-core rule); code that only grows (memory); running real cargo
for C dependencies; their sandbox by address masking.

## 16. Ladder, referee, switch

- **The ladder.** First rung: `blaze run` of `fn main() {}` through the launcher, on the new
  layer, with the parked type checker reconnected. Then `println!`; then ripgrep; then reth and
  tempo, their own tests passing (rule 7). Replaced: "to a running binary through the existing
  codegen and linker", because no executable is written and there is no link step.
- **Stops are the work list**: a question not answerable yet is `NotBuilt { method, item }`.
- **Referees.** For the item layer: the tree before the delete (#2197), kept as a reference
  checkout: for every item of real `core`, `alloc` and `std`, each `Items` question answers equal.
  For bodies: the old driver against blaze-typeck (section 8). Replaced: "the old layer is the
  referee until the switch" and "until the switch the old layer is frozen", because the old item
  layer was deleted on 2026-11-15 rather than kept beside the new one.
- **Deleted** on 2026-11-15 (#2197, then a second pass): the daemon, blaze-query, blaze-lints,
  hir-def's name resolution and persistence, hir-expand's database layer, blaze-engine; 51 crates
  parked ([parked.md](../parked.md)). **Still to delete**: hir-ty's old persistence forms (about
  33k lines), the daemon-side run crates (run.md), every portable form but the item key. The type
  checker, MIR, borrowck and codegen are not deleted without Andrew's explicit word.
- **How the work runs** (Andrew, 2026-11-15 19:11): one directory and one pull request for the
  rebuild; lanes write in it in parallel, never wait on a compile or a yes, and the main agent
  compiles the tree in batches (rules 35, 48). Until `blaze run` exists again, a merge's proof is
  the whole build and its tests on the exact tree, quoted (rule 68).

## 17. Measurements, floors, open experiments

**Measured.** `blaze_file_items::outline` (a file's text to its items, hashes and ranges; one
thread, an M-series Mac, release, 2026-11-15, bodies parsed in full): `core` 299 files, 180,413
lines, 13,443 items in 132.5 ms; `alloc` 64.2 ms; `std` 170.9 ms; all of `library/` (1.53 M lines,
207,552 items) 1,671 ms; blaze's own crates (2.63 M lines, 321,674 items) 3,332 ms: about 1
microsecond a line, 32 to 51 MB/s. The old path, 2026-11-15: cold hello-world check 21 to 45 s
and 3.4 to 4.6 GiB; repeat with nothing changed 2.2 to 3.3 s.

**Floors** (rule 67). The floor is the work this design says the case is; each floor time below
is an estimate from that work, not a measurement. Each case's target is its measured floor plus
10%; the milestone is the old text's number, kept as a first checkpoint, and misses rule 67 by
the ratio shown.

| Case | Floor: the work | Floor estimate | Milestone | Milestone / floor | Measure by |
|---|---|---|---|---|---|
| Repeat check, nothing changed, warm | exec, one certificate read, the stat sweep | about 2 ms for hello-world (exec 1 ms, a few `pread`s, a few stats) | 20 ms | about 10 | profile hello-world and reth on the ladder |
| Repeat after a restart | the above plus the index pages and certificate from disk | open: needs the page count | 20 ms | open | count pages touched |
| Cold check of hello-world, std's records local | the items reached (a few hundred), each one record read and one scope probe; main's body checked | about 5 to 10 ms (a few thousand reads of 1 to 5 us); memory: the mapped records reached plus the process, tens of MiB | 1 s, 200 MiB | about 100 to 200 | profile hello-world |
| Body edit | stat sweep, re-outline of one file (1 us a line), one body task, one finding record | about 2 ms for a 1k-line file; 30 ms for 30k lines unless re-outline is per item | 100 ms | about 50 (small file) | profile a body edit on reth's largest files |
| `blaze run` to main's first instruction | certificate, one spawn, socket, mapping, entry and pre-main records placed (run.md) | about 3 ms past the certificate | 25 ms in all | about 5 | run.md's floors |
| One stub miss, record present | resolver, queue, wake, one `pread`, fixups, store, wake (run.md) | about 10 us | — | — | run.md |

**Unmeasured, and what settles each:**

| Claim | Measurement |
|---|---|
| The outline divides by N cores (a file is independent) | outline `library/` at 1, 8 and 16 threads |
| With bodies skipped the outline falls further | time the outline with bodies skipped |
| Relax builds in about a minute with no third-party crate | time it |
| `#[track_caller]` costs nothing at run time | a microbenchmark |
| The 10 ms gate for pull from cold | the whole first fill on reth's and tempo's largest crates (section 6) |
| A private fn added rewrites about three records | count records rewritten per edit class |
| Glob cycles are small | the size distribution on std, reth, tempo |
| A repeat touches a bounded number of pages | count pages touched by a repeat check after a restart |
| Re-entry is cheap (reviewed 2026-11-16: restart stays, parking stays refused; made rare by kind order, a syntactic prefetch, and solver goals as their own tasks) | log every `Reads::ask` per body (kind, item or body fact, position, state bytes) on core, alloc, std, then reth and tempo; R = work repeated by restarts over total work, under pull order, kind order, and kind order with goal tasks. Keep re-entry if R is at most 0.10 and serial rounds add at most 10% to the 128-core cold time; above that, save state for those bodies only (explicit stacks, never fibers). Watch async code first: a `Send` obligation on a future needs its callee's body, down the chain |
| Observed-value optimization pays | its speedup over the plain optimized tier on reth |
| A prologue counter and a test-lane store are worth their cost | reth's suite against program-counter sampling (run.md 1 and 3) |
| Zero third-party is affordable before hello-world | add up third-party.md's replacement lines; schedule after the first rung |

### Structures and algorithms, by the goal they serve (2026-11-15)

Started on 2026-11-15, each with a floor and a measurement owed:

| what | where | why |
|---|---|---|
| Presence conditions as decision diagrams (`blaze-cond`, conditions.md) | every record entry, for the all-configurations check | one real crate root reads 21 (tokio), 33 (rustix), 48 (std) cfg switches; a fork per switch cannot hold that |
| Entry digests over raw bytes, and a file outlined per top-level item | the outline | bodies need no lexing; an edit to a 10 MB file costs the edited item |
| Batched lookups with prefetch (`get_many`) | the hash tables | cache misses of independent lookups overlap |
| `find_many`: finds in key order with read-ahead | the store, after a restart | index pages touched once, read in parallel, not one fault at a time |
| A discrimination tree over impl headers | the impl index and the solver's prefilter | one walk returns the impls that can unify, where rustc filters one level |
| Dictionary and delta encodings in the Parquet writer | profile files | plain files are 22 times the old size |

Not started: each waits for the piece it serves.

| what | waits for | why |
|---|---|---|
| Git's index as a source of content digests and per-directory digests | a measured file sweep on a real monorepo | a tracked file whose stat matches the index needs no read; a tree id proves a whole crate unchanged; filters and SHA-1 need care |
| Content-defined chunking of large records | the shared cache | a transfer in proportion to the change |
| Set reconciliation between peers (rateless IBLT) | peer-to-peer records | finding what a peer lacks costs the difference, not the set |
| A history-independent tree for a program's manifest | the first linked program | two builders write equal bytes; two manifests share all but the edit |
| Heartbeat scheduling for bstd's parallel loops | bstd's runtime passing its tests | latent parallelism at item grain with no task per item |
| An acyclic e-graph middle end with proved rules | the optimizing tier | one pass of rewrites, numbering and hoisting |
| Quantile and heavy-hitter sketches in tracing | the trace lane's floor | always-on summaries in constant memory |

## 18. Superseded and disagreeing docs

- Superseded: [certificate.md](certificate.md) by section 9 (checked by the daemon's loop) and
  [store-pool.md](store-pool.md) by section 10 (its own pool; a task yields on a miss).
- Disagreeing with this page, to fix in their own changes: AGENTS.md rule 60 (no remote in its
  order) and rule 12 (names papaya and crossbeam); target.md's key rules (the target in every
  key); `blaze_id::CrateItemKey`'s doc (cfg in the crate key); blaze-surface and blaze-impls both
  define `LangItems`, `CrateGraph` and `NoImplsOf`, and surface, scope, impls and expand each
  define `Edition` (rule 30); blaze-hash's hashbrown against tools/ratchets/third-party.txt's
  `(all) 0 0 0`.
- Old section names that code cites: "Also decided on 2026-11-15" is now sections 13 and 14; "The
  commands" and "Decide at run time", 11; "How a task waits", 2; "Layout: columns" and "Known
  cost", 13; "A binding has states", 6.
