---
status: plan
summary: The fresh core. One process per command answers from records keyed from text before the work and filed at the address of what their work read; no daemon, no query graph, no memory admission. Says the scope, the processes, verifying once, the records and their key rules, memory, positions, sharing and remote execution, what is deleted and reused, the order, the floors and how the finished system is judged. Interface crate crates/blaze-keyed.
date: 2026-11-13
---
# The target: records keyed before the work, in processes that die

Owner: the main agent, on the owner's instruction. Built from the outside audit
(`/Volumes/Projects/blaze-int-1112-notes/audit-1113.md`) and `target-decisions-1113.md` beside it
(which holds where they differ), and Relax (MIT; its text-closure key rules were copied with
credit into blaze-keyed's `text.rs`, deleted under decision D39: a derived record is found by
what it read, read-addressed.md). Supersedes `end-state.md` and `end-state/`. Every number is an
**estimate**. The owner: "dont measure just build": nothing waits for a measurement.

**The order when two things trade** (the owner, for "a huge mono repo, a million lines of rust"):
incremental compile short > memory use > cold compile > disk collection good and not locking >
disk use. Machines: best on 128 cores (Graviton 5, EPYC); many people on typical Macs (8-16
cores, 16-64 GB). One code path sized by a machine descriptor (`blaze_keyed::machine::Machine`).

## Scope (the owner's answers, 2026-11-13; decision D36)

- No `blaze watch`, nothing builds on save: edits come in transactions whose middle states are
  invalid. A build runs when asked and may leave a helper with an expiry, for a faster next build;
  it holds only what records rebuild and never builds on its own.
- Live patching that keeps a running program's state is later: an edit patches the file and the
  program runs again. Manifests stay general enough for a launcher driver; only the file driver
  is built.
- `run` and `build` check only what the program reaches, `check` all that is asked: checking is
  demand-driven like code generation, so code the program never reaches costs it nothing.
- Dependencies and everything from the bucket are maximally optimized, being built once and
  shared. Workspace code gets the fast backend first, tiered up in the background at low
  priority; the top tier optimizes across functions and crates at function grain, to stay
  incremental: an optimized function is keyed by its body and the keys of every body it took in.
- Acceptance: reth, tempo and blaze, as cargo workspaces (no Buck or Bazel mode). Hosts: macOS
  arm64 and Linux, arm64 first; x86-64 is CI's (no x86 VM now); Windows is out.
- The bucket (R2, nearly free) is read without a login; only CI, and later the remote workers,
  write, since only a trusted builder can vouch for a value. It turns over with the daily nightly.
- Fully local, private builds always work: nothing leaves the machine unless an organisation opts
  in. An old home after an upgrade is detected, reported and rebuilt, never migrated: records are
  a cache, and a migration is code that must stay right forever.
- New crates move to bstd once blaze builds blaze (cargo builds it until then); code that stays is
  sans-IO, so the simulation runs it. Every design aims within 10% of the floor from the start:
  missing is allowed, aiming low is not.

## Processes

- One process per command: it checks the certificate (change detection from the FSEvents journal
  by stored event id where the OS keeps one, else a stat sweep). On a hit it exits; on a miss it
  runs item tasks, files records and exits. That is the baseline.
- A build's helper (with an expiry) is the same binary staying alive with the `Resident` memo,
  holding only mapped record pages and the edited crates' arenas. No daemon, no `watch`. Build
  scripts and tests are sandboxed children; proc macros run in Wasmtime inside the process.
- Work is at item grain (one body, instance, macro call or script run per task), batched per file
  so scheduling stays under 1% of task time.
  - Workers start in proportion to queued work; a no-op or a one-body edit runs on the main thread.
  - Each worker has a frame, reset and never unmapped, and its own counters, merged when a task
    ends. Nothing shared and mutable is touched per item.
  - Peak memory is workers times the frame cap; nothing admits by memory.
- QoS: the edit path at user-interactive, cold fan-out on every core, compaction at utility.

## On disk: one store, records, two traces

- Store form (the store lane builds it on `blaze_keyed::store`): immutable segments and a
  generation manifest, mapped once per process, never unmapped in a run, read with no lock; each
  process writes its own segment and the flip is the only lock; no sync on the edit path (a
  rebuildable cache: checksums, the flip, a torn tail dropped and reported by name); collection is
  compaction under a byte cap then unlink, never stopping builds; only cold segments compressed.
- One representation. A record's bytes are the bytes the compiler computes with:
  position-independent, read in place, no decode. Small records are read by pread into the
  task's frame; plain arrays are mapped.
- Every record's root starts with a head: the full 32-byte key, the kind's tag and version.

| Record (`Kind`) | Key, computed before the work | Filed at | Contents |
|---|---|---|---|
| Certificate | tool, request base | its shape's answers (outside form) | the manifest roots, in plan order |
| FileImage | tool, edition, file bytes | the key | tokens, flat tree, piece hashes, names, line table |
| Surface | tool, settings, unit, own pieces | key + its shape's answers | one digest per exported fact (and the unit's seen row), sorted, read in place; its shape |
| Body | tool, settings, unit, item, own text | key + its shape's answers | inference, MIR, verdict, item-relative diagnostics; the facts it makes; its shape |
| Shape | (a trie node) | `shape_root(key)`, then `shape_next(address)` | the fact ids one run of the key asked |
| UnitManifest | tool, settings, unit, file keys, late reads | its content | the root: dictionary, files, surface, made facts and their makers |
| UnitChunk | (one per file) | its content | entries (item, key, address, shape as ordinals), postings |
| Mark | the unit's text key, its view (its deps' universe digests) | the key | the manifest root, the surface, the universe digest |
| Instance | body *result* digest, type-argument keys, settings, inlinees' instance keys | the key | code |
| Expansion | expander code id, input tokens, attribute, visible context | outside form | tokens |
| ScriptRun, TestRun | script program / reached instances, package, declared env, settings | outside form | outputs; traced outside reads |
| ProgramManifest | link settings, instance keys | the key | the instances; image digest |

Validity is two traces, both built from the start (decision D39, read-addressed.md):

- **The key** (trace a), computed from text before the work: what the record is (its own text and
  settings), never what it reads. An equal key with equal answers is the same address.
- **The shape** (trace b), recorded while the work ran: the sorted set of (fact id, digest) it
  asked, absence a digest too (a signature, fields, a layout, impl rows for (trait, self head), a
  binding in a scope, a const's value, a late input). The record is filed at
  `H(key, shape ids, answers)`. To find it again, the driver answers a candidate shape now (one
  lookup per fact in a mapped table, `front::Universe::answer`) and reads that address: a hit is
  the record, with nothing refiled or copied; an undone edit finds its old record. Candidates
  are the item's last shape (from its unit's chunk), then its key's shape trie, at most 64 nodes.
- **Postings** inside a unit say which items read a fact; a unit's dictionary holds every fact
  its records read from outside it. A unit whose dependencies' universe digests and text are as
  they were is settled by its mark: one lookup.

**Key rules, exactly.**

- A key covers what the record is, never what it reads: the item's own text (piece and body,
  with its unit's identity), the settings, and the toolchain identity (blaze and its pinned
  nightly, the target, the namespace); a unit's text key adds its file images and its late
  values (`env!`, `option_env!`, `include*!`). Nothing of the machine: no path, host name, time
  or address. Names a text mentions are not in its key: what they resolve to is read, and so in
  the shape.
- A fact belongs to its owner: a surface's to its unit (the id's first eight bytes are the
  owner's), a made fact (a const's value, a hidden type, what escapes a body, its effects) to the
  unit whose body makes it, listed with its maker in the unit's manifest root; what the asking
  unit sees as a whole is its own surface's row, else its seen row; a late read is the
  request's. A maker settles before its readers; an edge found only on a redo redoes the reader;
  a cycle is `Failure::MadeCycle` naming the items.
- A unit's mark is keyed by its text key and its view (its direct dependencies' universe
  digests), never by another record's key; the universe digest hashes its surface result, its
  made table and its dependencies' universe digests.
- Soundness (rule 21): a record's shape is every fact its work asked, absences included, and
  what the work asks first depends only on its key. Then equal answers mean the same questions
  and the same result, so any candidate shape is sound to try. The simulation holds the driver to
  a from-scratch build with a model that reads by value, and the `Audit` memo redoes every hit
  and compares bytes.
- Firewalls: a body edit changes no surface fact, so no other unit is looked at unless a fact it
  makes moved. What crosses: a signature, a field, an impl, a const's value, an inlinable body's
  code key (code records only).
- Content-addressed code, as Unison has it, at the grain that keeps edits fast: code records are
  shared by content across crates, versions and machines; names and positions are not code
  identity. Source stays in files.

## In memory

- Interners and dense ids are per process. Only what a task creates while checking is interned
  (the types of the edited bodies), in the task's frame or the session's arena. A dependency is
  used by mapping its surface image; nothing of it is copied to the heap.
- The memo is a static policy of `Driver<M: Memo, X: Executor>`. `RecordsOnly` (one-shot
  commands): every ask goes to the store. `Resident` (a build's helper): a plain table per crate
  arena; a change drops the arenas of changed crates and of dependents whose imported digests
  changed, wholesale, and they refill from records. No dependency tracking in memory.
- What stays resident is file-backed pages the kernel can take back: almost no private memory,
  so the second priority is met without giving up the first. Cost: the generic code twice in the
  binary (estimate under 1 MB); zero per ask (`RecordsOnly::get` is a constant `None`).
- No resident session for workspace crates unless a later number shows a need. Soundness:
  nothing holds a record's value except through the memo, which answers only by full key; the
  lint to write: no field of a type outliving a request holds a `Kind::Root` or store bytes.

## Positions

Never in a key or a body record; item-relative in records; absolute only from a file's line table
when printed. The positions lane builds the compiler side.

## Shared cache and remote execution (the owner's decisions 12-17)

- A record is the unit of sharing. CI pushes everything it builds to a bucket and anyone pulls it,
  so std and blaze-std come precompiled.
- **Determinism is a protocol requirement.** Two builders of one key must write byte-identical
  values. Checks: locally, `Put::Other` refuses a second value; the CI push refuses a key whose
  bytes differ from the bucket's; a test builds twice and compares bytes.
- The segment is the wire format: an object, read by ranged GET, mapped like a local one. Index
  shards (sorted fixed-width entries under a signed root) answer "which of these keys" in one
  round trip per shard; bundles (std, blaze-std, a toolchain) are signed manifests of whole
  segments. Seams: `store::Source` (`find`, `fetch`) and `store::Verifier` (`verify`).
- Trust. A key names inputs, not a value, so only a statement signed by a trusted builder (CI, the
  remote workers) binds a key to a digest. The client verifies the signature, the digest and the
  size limits before mapping. A developer's machine never writes a shared namespace.
- Namespaces. Public holds what comes from a registry archive known by its checksum, or from the
  toolchain. Everything else (a path, a workspace member, git, a private registry) is under its
  organisation (`kind::namespace_of`), so private code never lands in public. Read lists and
  names never leave their namespace.
- Remote execution is the same driver with another executor. An action is (kind, key, the keys
  of the records it may read); its task gets only an `exec::ActionReader` over those inputs. The
  driver's rule `stays_local`: key tasks, and every task while fewer than 64 are queued or
  running, stay on the machine; only a cold fan-out goes remote, in batches; results return as
  segments.
- Lifetime: objects are immutable; cleanup by generation and last use per namespace; no lock.

## Verify once, by construction (the owner's decision 18; rule 74)

The owner: "don't re-verify things that don't need to be verified; ... or have a type system that
makes it not necessary, or correct by construction; disk too." Every check has one place and a
value that proves it happened; code that needs the checked thing takes the proof, so checking
twice cannot be written and skipping it cannot either.

- A proof is a type only its checker can make (`Verified<Segment>`, `Certified`, `Holds<K>`),
  branded with what it is valid for (the snapshot, the boot, the generation) so it cannot outlive
  it. Checking consumes the unchecked value and returns the proof.
- A coarse proof covers everything under it: a certified request checks nothing below; an
  unchanged universe digest means no dependent is looked at; a mark that holds settles its unit
  and every record its manifest names; only under a parent that moved is the next level compared.
- A record read at the address its shape's answers make is followed by no other check; a shape
  is answered at most once per (item, snapshot), and nothing is refiled.
- Bytes are verified once: a segment this process wrote is trusted; one from an earlier boot is
  checked in full once, recorded in the manifest; a fetched one's signature and digest at fetch,
  recorded. A read of a record in a verified segment checks nothing.
- A file is hashed again only when its stat stamp moved (the racy-stamp rule decides the edge);
  with a watcher journal, only the paths it names are looked at.
- A put of bytes the store holds writes nothing. Nothing syncs on the edit path.
- Counters: checks per kind per request; a second check of one thing in one snapshot is a bug the
  simulation's oracle catches.

## Findings (`crates/blaze-findings`)

A lint finding is a record keyed like the check it comes from, never a side effect of one.

- **Records.** Per (body, pass): `BodyFindings` (tag 16, key = toolchain, body key, pass id and
  version, and the ids of what the pass read beyond the body). Per item for a workspace fold:
  `ItemRows` (17, same key). The fold's leaves: `FoldFindings` (18, key = pass, scope, leaf,
  rows digest). An analysis that reads beyond its item: `DeferredFindings` (19, key = pass,
  item, the ids it read). A finding holds the lint, its item, a range relative to the item, a
  message id with typed arguments and a fix of item-relative edits; no level and no file
  position, so a level change or a moved item re-keys nothing. Its id hashes the lint, item,
  message, arguments and ordinal: `new` and `fixed` are a diff of ids.
- **Classes** (the class rule). *Answer*: computed before the reply; every pass with a lint at
  deny anywhere is this class whatever it declares, so a gate never passes unchecked. *Cheap*:
  advisory per-body passes and the delta of each workspace fold, right after the answer at
  interactive QoS on the performance cores. *Deferred*: expensive analyses (value ranges,
  near-duplicate search, mutation testing), after the cheap class drains, at utility QoS on a
  quarter of the cores, remote-eligible. A pass declares its expected cost; one over its class's
  bound (100 µs answer, 1 ms cheap per subject) is reported by name with both numbers.
- **Cancellation.** All classes run on the command's snapshot. A newer command cancels work
  that read something that moved or is no longer asked for, and what depends on it; it adopts
  in-flight work of the same (pass, subject, key). A file change alone starts nothing (rule 80);
  nothing is computed twice.
- **Waits.** `Wait::Answer` replies with the answer class; `Wait::Cheap` (the default) also
  waits for cheap work up to 500 ms; `Wait::All { budget }` waits for everything up to the
  budget. Every reply lists what is pending (lint, item, class, estimate) and which passes were
  slow; a superseded command's reply says so.
- **Adoption ladder.** `step` in `[workspace.metadata.blaze.lints]`: `rust` (general at warn),
  `suggest` (and `blaze::suggest`: where a blaze feature or a tighter type would pay), `edition`,
  `strict` (blaze's own table). Moving up baselines the new step's findings per lint and crate.
- **Cost against the floor.** A body edit re-runs only that body's passes (the floor) plus, per
  fold, one leaf of ~64 rows and the root of 32 bytes per leaf: at 2^20 rows ~0.5 MB rewritten
  and ~1 ms per edit (estimates), the root far over the floor of one leaf; a middle level of the
  tree brings it to the floor and is not built yet. Not wired into `blaze check` yet; the
  scheduler is sans-IO and its simulation is the proof.

## Deleted and reused

- **Deleted** (the audit's list), each after its replacement passes: blaze-query's validity,
  blaze-kernel, blaze-dbsp; the nameres and method-resolution deltas, implied writes,
  `Shared::write`; dependency images, frozen pages, `Awaited`, `eligible`, `check_cache`; the body
  and goal stores; the daemon parts of `serve/`; blaze-graph's plane, admission, give-back and
  ledger; `heaps.rs`, `malloc.rs`; blaze-mem's gate and budget; blaze-cas's pool, collector and
  tenants; blaze-gc; codegen units; CLIF once LIR covers.
- **Reused:** syntax-tree, the parser, mbe/tt, the hir-def and hir-ty rules, the next solver, MIR
  building and borrowck; blaze-certify (its no-daemon `Solo`), blaze-lir, blaze-stencil,
  blaze-link, blaze-launch/live; the LLVM release path; blaze-macro-eval with blaze-wasi; the
  sandboxes; manifest/resolve/registry; blaze-mem frames.
- **Changes needed:** blaze-format's `Writer` and `Aligned` take an allocator, and its `// alloc:`
  comments go. In blaze-certify, the slot becomes local evidence, `CodeHolder::Memory` goes, and
  daemon-view reads give way to the journal.

## Order (nothing waits for a measurement)

0. A working binary: the memory control stack deleted; the hello-world smoke passes on the VM.
1. In parallel: this page and `blaze-keyed`; the store's physical form; the in-process mode; the
   lints, positions and text keys ported.
2. In parallel on `blaze-keyed`, as new crates beside the old code: the front end over surface
   images; body records with key and shape; `blaze-memo`; `blaze-program` (instance records,
   the streaming monomorphization walk, programs as patched manifests); `blaze-expansion`
   (macro-call and script-run records); test records.
3. Deletions, each after its replacement passes. 4. The fetch seam with std's records; the matrix.

## Decisions made where the audit left a choice

- Both traces from the start, not keys alone: incremental time comes first and disk last. A unit
  manifest is filed after every record it names, so one read settles a unit.
- An instance (and its plan) is keyed by its body's *result* digest, so a body found at a new
  address with the same result keeps its code.
- At the certificate's 64 shapes the request ends `Built { filed: Full }`: it costs the next
  request a mark lookup per unit, not a failed build (before D39 the cap failed the request).
- Commands run `RecordsOnly`; a build's helper `Resident`. The crate is `blaze-keyed`:
  `blaze-record` (the read-list form it replaces) has the name.

## Floors and expected cost, a million-line workspace (estimates, rule 67)

Floor: an oracle that redoes only what changed and its direct readers, plus unavoidable IO.

| Operation | Floor | This design, 128-core | This design, 16 GB Mac | Ratio |
|---|---|---|---|---|
| No-op | journal read + certificate: 2 ms | 3-5 ms | 3-5 ms (FSEvents) | ~2 |
| Comment edit | reparse one file + line table | + manifest key, all hits: 5-10 ms | same | ~1.2 |
| Private body edit | one body check | start + key + 1 body + 1 instance: 10-30 ms | 15-40 ms | ~1.2 |
| Signature edit, N readers | N bodies | N bodies + the units' dictionary rows answered + N shape answers | same, fewer cores | ~1.1 |
| Add an item | its body + names now found | the same as a signature edit of its name | same | ~1.1 |
| Add a derive | one expansion + its item | one Wasmtime call + the derived items | same | ~1.2 |
| Restart then edit | the edit | the edit (no state to restore) | same | ~1 |
| Memory between edits | 0 private | 0 one-shot; file-backed pages when resident | same | ~1 |
| Disk | the records | records + shapes + chunks and a mark per unit state | same | ~1.3 |

What cannot reach the floor, plainly: a unit whose dependency moved answers every row of its
dictionary owned by others (r lookups), where the floor is the changed facts; the rows are not
indexed by the dependency fact that moved. Bounded by the unit's own reads.

## What blaze must do that Relax does not, and its cost here

- The real std: demand std, shipped as public records; the key's sysroot is one unit.
- AOT binaries and LLVM release builds: ProgramManifest, and an LLVM path keyed like Instance.
- Tests: TestRun records with outside reads. Cross targets: the target is in every key, with
  per-target bundles. `check` checks every body asked and runs every lint: body records carry
  diagnostics, and lints are item-grain records. Sandboxes always: scripts and tests as children, macros in wasm.
- 800-crate graphs: unit manifests make a no-op one read per crate.

## How the finished system is judged (taken at the end, not a gate)

- Machines: the owner's Mac; the 128-core arm64 VM; an EPYC VM by exception, asked of the owner.
  Projects: hello-world, the six-crate project, reth, tempo, blaze.
- Operations: the table's rows; a cold build and peak memory; a build's reads while compaction
  runs; cold-build and signature-edit scaling at 1-128 workers; and, per record kind, the
  edit-loop rows under `RecordsOnly` and `Resident` (the in-process mode's script), to choose
  `Kind::KEEP`. Each result names its tree and binary and is reported against its floor.
- The correctness gate is the simulation (`crates/blaze-keyed/src/tests.rs`). It runs generated
  edits, restarts and kills under both memos. Every hit and every reuse must equal a from-scratch
  build, `Resident` must refine `RecordsOnly`, and `Audit` faults on nothing. Each plant in
  `blaze_keyed::Plant` is caught by name (late inputs left out of a key, a dropped read, a dropped
  absence, a hit that does not replay, a made fact not asked, a dictionary missing a fact, a
  reader before its maker, an over-committed pool, two values at one address, an early
  manifest, a stale resident entry, a lookup under a mark), and a case no run reaches fails by
  name.
