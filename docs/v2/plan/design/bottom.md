---
status: plan
summary: "The item layer and the commands above it, rebuilt from scratch, one consolidated spec: goal order, principles, keys and records, files, the name rules R1 to R11, expansion, signatures and bodies, commands, code rules, Relax, the ladder and floors. Pull at item grain; names, the driver and the store are as decided in end-state.md (D157 to D161), linked from sections 6, 9 and 10. run, test, check and build run code records in the program's own process. Zero third-party crates, no std or alloc in blaze's own code, with dated exceptions."
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
questions about a few hundred of them. That layer was deleted on 2026-11-15 (#2197).

Each decision is stated once, in its final form; detail lives in
[run.md](run.md), [typeck.md](typeck.md), [end-state.md](end-state.md),
[std.md](std.md#two-builders), and [third-party.md](../third-party.md).

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
   made by its own task. Nothing is computed that no one asked for; names too are decided per
   cell on demand (section 6).
2. **A task never waits** (Andrew: "why not explicit state machine sans io"). Logic whose states
   are the design is a hand-written sans-IO machine (`step(event) -> effects`; rule 14): name
   resolution, the run, the drivers. A compiler task that is recursive code (the type checker
   asks for a callee's signature in the middle of an expression) is not turned inside out and
   holds no worker while it lacks a fact: it returns `Need`, parks, and resumes when the
   producers of what it lacks merge; nothing starts over (section 9).
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

- **Identity is `blaze_id::ItemKey`**: the hash of what the item is
  in (its module, impl, trait, enum or body), its kind, its name and where it is written (an
  index among equals, `Item::disambiguator`; a macro-made item adds the call's place). Bytes 0..8
  are its crate key's first 8; the top bit of byte 8 marks an item inside a body.
- **A rename is a new item**, whose work is reused by content: a body's key writes its own item
  as "self". No rename detection. A task's `Local` number is not `Portable`.
- **An interned id stays in its snapshot.** It compares in one instruction and is what the type
  checker uses, but it does not meet `blaze_id::Portable`, which keys meet and nothing holding a
  `Gid` does; blaze-format's record impls and any table that outlives a snapshot
  (`blaze_id::Carried`) require it. Every record's encoder and decoder goes between id and key
  through one pair, `blaze_id::key_of` and `blaze_id::resolve` (a lookup in the crate's key
  index; `NoItem` when the key names nothing). Built, not wired: nothing outside crates/blaze-id
  calls `key_of`, `resolve` or `Carried` yet.
- **Check.** The surface records of `core`, `alloc` and `std`, written in two processes with
  different interning order (one thread and N), are equal bytes. Open: whether a second `impl`
  of one trait for one type name is told apart by more than its index among equals (adding an
  equal impl before it moves the index today).
- **A name or a place is in a code key only where the code reads it.** A callee, a type or a
  constant enters a body's code key by the content of its resolved definition (its interface
  for a call; its layout and interface for a type), never by its path or name. Locals are keyed
  by binding index, not by spelling. What does read a name or a place is keyed by it:
  `type_name`/`TypeId` (the nominal arguments' item keys), `module_path!` and `stringify!`
  (literals), `file!()`, `line!()`/`column!()` where they are values (`#[track_caller]` bodies,
  consts, promoteds), an exported, `#[no_mangle]` or `export_name` symbol, and a told debug
  type. Positions are late-bound: a panic location or a line row is a `SpanRef { anchor, index }`
  into the anchor's span table (`blaze_format_core`), so moving code changes the table, not the
  code. A local symbol is named by its instance's content hash, so two builds of one tree give
  one name.
- **The crate key** (`CrateItemKey`) is the prefix of every item key of the crate. It never holds
  cfg, features or the target, so an item has one key on every target and feature set: it is the
  package's identity ([end-state.md, D158](end-state.md#d158-names-at-module-grain-one-crate-key-cfg-is-read)).
  `blaze_id::CrateItemKey`'s doc still lists cfg among its inputs: to change.
- **A key is what the record is; a read list is what it read.** Every record has two columns,
  as in [run.md](run.md): the key, computed from its own text, the
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
- **Names and positions that reach the output are a layer of their own.** Debuginfo names,
  panic `Location`s, `line!()`/`column!()`/`file!()`/`module_path!()`, `#[track_caller]`
  locations, symbol names and diagnostics stay exactly rustc's, but no code or check record holds
  them: a record keeps a location slot or a node id, filled when read or printed from a
  presentation record keyed by its own hash (binding and label names by position, the item's
  name and module path, its file's path, positions). A move or a rename reruns only what reads
  that record; the binary stays byte-identical to a fresh build.
- **Stable item IDs in blaze's reports** (owner, 2026-10-30; not built). An ID is the item's
  semantic hash (a signature's: its signature hash) and the node's place in its anonymous body; a
  move or rename keeps it, an edit of the item gives a new one. It appears beside
  `file:line:col` in blaze's own records only (test results, event records, diagnostics records,
  the agent interface), never in the program's stdout, stderr or panic text: the test runner's
  panic hook maps a `&Location` to its entry and writes the ID to blaze's side channel. A
  `locate(id)` request answers the item's current place from an identity-to-item index and a log
  of each item's past identities; an ID of an edited item says `edited: true` with no node
  position.
- **Code has two identities, beside its item key.** *Compile identity* is what one instance's
  generated code depends on: a type argument by its fact (layout and shape, so `Vec<&A>` and
  `Vec<&B>` are one), a constant by value, a callee by its own compile identity, every setting
  the code generator reads. It keys code records and code folding. *Behaviour identity*
  (`CodeId`) is what running the code can depend on: full type identity, each instance's
  effective overflow-check bit, and everything dispatch says it can run (calls, `Fn*` of closures
  and fn items, drop glue, reified pointers, vtables, an `async` block's `poll`); a debug and a
  release build of one step have one `CodeId`. It keys `#[blaze::cache]` and
  `fn_info(f).code_id`. Neither holds a position, a path, a name's spelling or item order.
- **Recursion is one component.** Both are Merkle graphs: a node is its own content plus its
  callees' identities, hashed once and shared by every root that reaches it; a strongly
  connected component is hashed as one unit, members in a canonical order (local hash, ties by a
  stable name, never an interned id), so a node on no cycle is the one-member case of the same
  formula. Code that asks for the identity of code that runs it is a typed `Unidentified`
  (self-read), never a fixpoint.
- **Property for the edit fuzzer.** An edit to comments, whitespace, formatting or item order
  keeps every compile identity and every `CodeId`; an edit that changes behaviour changes the
  `CodeId` of every root that reaches the edited instance and of no other; asking in another
  order, process or checkout path gives the same bytes.

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
| `NoImplsOf`, `LangItems` | the crate key | each module's impl digest and `#[lang]` digest, from its `ModuleDecls` and expansions | "crate C has no impl of T"; lang items by name | surface; built from a list the caller hands |
| `CrateGraph` | the lock file and workspace manifests by content | none | crates by key, dependencies by name | surface; built |
| `BodyFacts` | the body's text | the signatures it read | what a body gives others: hidden types, const values, discriminants | not built |
| Checked body | the body's text, with its own item as "self" | the `World` reads, sealed (typeck.md) | types, adjustments, callees, captures, diagnostics; `needs` for next time | typeck; in writing |
| Finding | as its check's | as its check's | one lint or error, item-relative range, no level | not built |
| Fast code, optimized code, program manifest, test result | see [run.md](run.md) | see run.md | code records and what a run reached | run; machines built, drivers not |
| Certificate | the tool, the request's shape | the request's reads | the answer: findings' and manifest's keys | not built on the new layer |

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

Decided in [end-state.md, D158](end-state.md#d158-names-at-module-grain-one-crate-key-cfg-is-read):
a module's links are a record keyed by its text, and a lookup decides cells (module, namespace,
name) on demand, each hop a logged read; the cell core is `decide` in `crates/blaze-scope/src/cells/`.

**Globs are edges, not copies.** A module's record holds what its module declares, what its
named imports bind, and its glob edges (source module or enum, visibility, expansion depth),
never a name copied through a glob. A name reached through a glob is found at lookup: the
module's own binding first (an explicit binding shadows a glob), else the union over its edges
of each source's value, each filtered by what the importer may name and narrowed by the edge's
visibility, a glob cycle contributing only what some member binds (the least fixpoint). One item
by two routes is one binding, as visible as the widest; two items are ambiguous, an error only
where used. A lookup's read list holds each `(module, name)` and each glob edge set it probed,
found or not, so a `pub` item added to a module others glob rewrites no importer's record.

### Name resolution: the rules

The rules the name solver and the reference solver (`crates/blaze-scope/src/tests/oracle.rs`)
both hold. Each has fixtures in `crates/blaze-scope/src/tests/name_rules.rs` (`rNx_*`), solved in
the calm order and 64 drawn completion orders; the answer must be the rule's in every order.
Sources: the Rust Reference's expansion-time name resolution (`names.resolution.expansion.*`),
`items.use`, and RFC 1560; no compiler's behaviour is a source.

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
   was decided first (r2e; planted `R2BoundStopsWaiting`). Fixtures r2a-r2e; oracle.rs `two_globs_*`.
3. **R3 glob cycles** are legal; the answer is the least fixed point (RFC 1560: resolution runs
   "until fixed point"). A name in no member of a cycle is not found, and nothing hangs. r3a;
   oracle.rs `a_glob_cycle_through_a_found_binding_resolves`.
4. **R4 privacy.** A glob brings only the names visible at the importing module
   (`items.use.glob`). r4a; oracle.rs `a_glob_that_brings_in_private_items_is_caught`.
5. **R5 the stall rule**: at a quiet point, groups that wait only on
   themselves (the sink components) fail as not found; repeat. Refinement (approved): a member
   that waits on ONE macro call of the same group through a textual read is spared that round;
   the order is program text alone. In a stalled member what is undecided is taken as absent,
   so a plain first segment whose inner scope was undecided goes outward (R6) and the import may
   RESOLVE: then only the members that resolved are decided that round, and the rest are tried
   again with the facts ("nothing more comes" holds only for what fails); with none resolved,
   all fail. The cell core needs no such round: it decides a stalled group from the group's
   least fixed point, where the rest already read what the resolved members bring (the
   oracle keeps the round). A spared call still expands, so "nothing more comes" is false for whatever waits
   on it: in a round with a spared call only the calls a spared call's textual read waits on
   fail (they define nothing first), and the rest are tried again ("names in the fully expanded
   AST must resolve to the same definition"; r5e, r5f). r5a-r5f;
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
   (`...imports.shadowing`). The ambiguity is at the first segment: the import fails there, as ambiguous, whatever the
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
   segment's too, takes the undecided as absent. r5g.

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
- **Proc macros**: blaze compiles each proc-macro crate itself to a wasm64 (memory64) module
  (`blaze-lir` to wasm64) and runs it in the process under Wasmtime (rule 10;
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
- **Build scripts** run sandboxed (rule 9) as a `ScriptRun` record:
  keyed by the script's program, its package, its declared environment and the settings, with its
  traced outside reads. blaze-build-script holds the environment cargo gives, the parse of what
  it prints and who receives it; constant-directive scripts are read, not run. A pure-Rust
  script is compiled to wasm64 and run like a proc macro; one that runs native tools or links C
  goes to the hypervisor micro-VM sandbox, still experimental ([hvbox.md](hvbox.md),
  [build-scripts.md](build-scripts.md)).

## 8. Signatures, impls, lang items, bodies

- **Signatures** are split records (table) so each is cut off on its own (head records:
  `crates/blaze-ladder/src/front2/head_record.rs`).
- **Impls.** "Which impls of T could apply to a type of shape S" (`blaze_impls::candidates`)
  reads, of each crate that can hold one, its `NoImplsOf`, and only where that does not rule the
  crate out, its directory and the one or two buckets it names: never a crate's whole table.
  Blanket impls are indexed under the trait. Every read, negative ones too, is handed to the
  caller's read list. **Lang items**: one record per crate, one binary search by name.
- **`BodyFacts`**: what a body gives others (hidden types, const values, discriminants), made by
  the body's task.
- **The type checker** is `crates/blaze-typeck` ([typeck.md](typeck.md)) over the Rust project's
  next-generation trait solver: one body in, one record out, every outside fact read through a
  sealed recorder so a result cannot depend on what its read list does not hold, the walk an
  explicit work stack in rustc's order of inference (which is the specification). The borrow
  check is blaze-flow's ([flow.md](flow.md)).

## 9. The driver

Decided in [end-state.md, D157](end-state.md#d157-one-driver-a-stage-is-a-kind-of-task) and
[D161](end-state.md#d161-a-task-that-lacks-a-fact-waits-on-it-nothing-starts-over): one ready-set
driver in blaze-engine, each stage a kind of task; a task that lacks a fact returns `Need`, parks
and resumes when its producers merge. The edit path's floor is
[D168](end-state.md#d168-an-edit-costs-the-body-it-changed-the-edit-paths-floor-2026-11-20).

## 10. Storage

Decided in [end-state.md, D159](end-state.md#d159-one-body-one-store-one-answer-form-code-made-ahead)
and [D160](end-state.md#d160-a-stores-format-is-part-of-its-place): one store, records written once
and read in place (small ones by `pread` into the task's frame, large plain arrays mapped
read-only), the page cache the only cache; how a restart opens it is [store-next.md](store-next.md).

- **Writes.** Each process appends its own segment; the generation flip is the only lock; nothing
  syncs on the edit path (the store is rebuildable: checksums, the flip, a torn tail dropped and
  reported by name). A put of bytes the store holds writes nothing. Collection never stops a build;
  only cold segments are compressed.
- **Determinism is part of the protocol.** Two makers of one key write byte-identical values: a
  second, different value is `Put::Other` (crates/blaze-keyed/src/store.rs), refused with the kind
  and key and never written over (rule 21); a test builds twice and compares bytes.
- **Verified once.** A segment this process wrote is trusted; one from an earlier boot is checked
  once and the check recorded; a read of a record in a verified segment checks nothing (rule 74;
  end-state.md D163).
- **Remote**: a pack's index once, then byte ranges in parallel; one format
  for R2 and a peer.
- **Trust.** A key names inputs, not a value, so only a statement signed by a trusted builder (CI,
  later the remote workers) binds a key to a digest; the signature, digest and size limits are
  checked before a byte is mapped. A developer's machine never writes a shared namespace; private
  code, read lists and names never leave their organisation's namespace.
- **std's records** are built by CI, per toolchain, target, std features and profile settings,
  and published as a signed bundle. Offline, or with no bundle for the target, std is filled
  locally like any dependency, and the command says so by name (rule 3: said, not quiet).

## 11. Commands

Andrew, 2026-11-15 18:45: "blaze run / test / check that are fast"; "everything in JIT world".
The commands are `run`, `test`, `check` and `build`.
Detail: [run.md](run.md).

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
- **`blaze build --release`** writes one native executable (Mach-O, ELF): every function the
  program reaches, at the optimized tier, no stubs, laid out as the launcher lays them out,
  written once, whole. Not incremental; keeps nothing.
- **Gone:** the linker's patching of a file in place (kept slots in a file, spare
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
- **Tests that pick themselves**: section 1 ([test.md](test.md)).
  **Discovery**: a test is an item whose attributes hold `#[test]` once cfg and expansion are
  done (an attribute macro can make one); each module's tests are read from its `ModuleDecls`
  and expansions, not by walking the crate. The harness entry is part of the test run's program
  manifest, made by blaze, calling each selected test (run.md, `Tests`). Open: libtest's command
  line and output, `harness = false`, and doctests ([doctests.md](doctests.md)).
- **Debug info for a run.** Each code record carries its unwind table and symbol; the launcher
  registers placed code as one more image so the program's own backtrace finds names
  (blaze-launch-rt's `unwind` and `symbols`). Open: registration with a debugger (lldb, gdb) for
  code placed while the program runs, and `blaze build`'s side file ([debug-side-file.md](debug-side-file.md)).

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
| Wasmtime (`vendor/wasmtime`) | the in-process runner of wasm64 proc-macro and build-script modules (rule 10) | stays vendored (Andrew, 2026-11-15 20:20: vendoring is fine for the hard ones); replaced only if a profile names it |
| Proc-macro modules from a bootstrap builder (2026-11-16, D52; rule 1's named interim exception) | `scripts/src/bin/proc_macro_modules.rs` builds each proc-macro crate of a lock, unchanged, for `wasm32-wasip1` with the pinned nightly's rustc (`-Zwasm-proc-macros`), build scripts sandboxed, paths remapped, and writes module records; never the `blaze` binary, never spawned by it. Owner's word: allow proc macros so real projects compile unchanged | ends when blaze's own wasm64 (memory64) compile of proc-macro crates, which runs on a missing module (`crates/blaze-cli/src/macros.rs`), builds every module; a module blaze builds is another key and supersedes by decision |
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

**Encoding inside a column.** A node is a `u8` kind in one column and a `u32` data word in
another; a variant that needs more keeps it in one shared `extra: [u32]` array and stores its
index (Zig's encoding), never a Rust `enum` with payloads in a hot array. Children, arguments,
edges and projections are `(start, len)` ranges into one flat array per kind (CSR), never a
`Vec` per node. Names are symbol ids, never `String`; long-lived data holds no `Box`, `Arc` or
`Option<Box<_>>`. Sizes are held by `const` asserts beside each type: a token 4 bytes, a syntax
event 8, an LIR instruction's hot part 8 (`crates/parser/src/lexed_str.rs`,
`crates/parser/src/event.rs`, `crates/blaze-lir/src/func.rs`). A phase runs over every item of
one kind at once (hash every changed body in one BLAKE3 batch, lex every file), and tiny tasks
are grouped so the scheduler's cost per task stays below the task's work.

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

- **The ladder.** `blaze run` of `fn main() {}` and of `println!` (every merge's smoke, rule 68);
  then ripgrep; then reth and tempo, their own tests passing (rule 7). No executable is written
  for a run and there is no link step.
- **Stops are the work list**: a question not answerable yet is `NotBuilt { method, item }`.
- **Referee** for the item layer: the tree before the delete (#2197), kept as a reference
  checkout: for every item of real `core`, `alloc` and `std`, each `Items` question answers equal.
- **How the work runs**: one directory for all lanes; lanes write and commit, and the coordinator
  builds and admits (rules 35, 48). A merge's proof is a release blaze running, checking and
  testing hello-world on the exact tree, quoted (rule 68).

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
| A private fn added rewrites about three records | count records rewritten per edit class |
| Glob cycles are small | the size distribution on std, reth, tempo |
| A repeat touches a bounded number of pages | count pages touched by a repeat check after a restart |
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

- Disagreeing with this page, to fix in their own changes: AGENTS.md rule 60 (no remote in its
  order) and rule 12 (names papaya and crossbeam); `blaze_id::CrateItemKey`'s doc (cfg in the
  crate key); `CrateGraph` is defined in blaze-scope and blaze-world, and `Edition` in crates/edition, blaze-surface, blaze-typeck,
  blaze-manifest and blaze-fmt (rule 30).
- Old section names that code cites: "Also decided on 2026-11-15" is now sections 13 and 14; "The
  commands" and "Decide at run time", 11; "How a task waits", 2; "Layout: columns" and "Known
  cost", 13; "A binding has states", 6.
