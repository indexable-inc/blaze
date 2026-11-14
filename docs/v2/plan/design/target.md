---
status: plan
summary: The fresh core. One process per command answers from records keyed from text before the work, each with the read list its work made; no daemon, no query graph, no memory admission. Says the scope, the processes, verifying once, the records and their key rules, memory, positions, sharing and remote execution, what is deleted and reused, the order, the floors and how the finished system is judged. Interface crate crates/blaze-keyed.
date: 2026-11-13
---
# The target: records keyed before the work, in processes that die

Owner: the main agent, on the owner's instruction. Built from the outside audit
(`/Volumes/Projects/blaze-int-1112-notes/audit-1113.md`) and `target-decisions-1113.md` beside it
(which holds where they differ), and Relax (MIT; its key rules copied with credit in
`crates/blaze-keyed/src/text.rs`). Supersedes `end-state.md` and `end-state/`. Every number is an
**estimate**. The owner: "dont measure just build": nothing waits for a measurement.

**The order when two things trade** (the owner, for "a huge mono repo, a million lines of rust"):
incremental compile short > memory use > cold compile > disk collection good and not locking >
disk use. Machines: best on 128 cores (Graviton 5, EPYC); many people on typical Macs (8-16
cores, 16-64 GB). One code path sized by a machine descriptor (`blaze_keyed::machine::Machine`).

## Scope (the owner's answers, 2026-11-13; decision D35)

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

| Record (`Kind`) | Key, computed before the work | Contents |
|---|---|---|
| Certificate | tool, request base (blaze-certify) | what the last answer read |
| FileImage | tool, edition, file bytes | tokens, flat tree, piece hashes, names, line table |
| Surface | tool, settings, own pieces, dep surface keys, late reads | scopes, exports, impl index by (trait, simplified self), signatures, fields, layouts, const values: one digest per exported fact; import table; read in place |
| Body | text-closure key (below) | inference, MIR, borrow-check verdict, item-relative diagnostics; read list |
| Instance | body *result* digest, type-argument keys, settings, inlinees' instance keys | code |
| Expansion | expander code id, input tokens, attribute, visible context | tokens |
| ScriptRun, TestRun | script program / reached instances, package, declared env, settings | outputs; traced outside reads |
| UnitManifest | file keys, late reads, dep manifest keys | (item, kind, key) of every record of the unit |
| ProgramManifest | link settings, instance keys | the instances; image digest |

Validity is two traces, both built from the start:

- **The key** (trace a), computed from text before the work: an equal key is a hit, no check.
- **The read list** (trace b), recorded while the work ran: (fact id, digest) at item grain,
  absence a read (a signature, fields, a layout, impl rows for (trait, self head), a binding in a
  scope, a const's value, the record's own text, a late input). On a key miss the driver checks
  the list of the record the last manifest names for the item against the current surfaces (an
  O(1) lookup per read in a mapped fact table): all equal, and the old result is filed under the
  new key, byte for byte what redoing it writes; else the body is redone. No reverse index, no
  postings, no dependency graph: candidates are found forward. Cost: candidates × reads compares,
  and the lists on disk, accepted because disk is last.

**Key rules, exactly.**

- A text-closure key covers the item's own text (piece and body, with its crate's identity);
  each name the text mentions, with the closure of every node found under it in the crate's
  universe (Relax's rules; an absent name is a key too); the crate's "always" closure (glob and
  `as _` imports, inner attributes, unexpanded item macros); late values (`env!`, `option_env!`,
  `include*!`) and the expansions' keys; the sysroot as one unit; and the toolchain identity
  (blaze and its pinned nightly, the target, the namespace). Nothing of the machine: no path,
  host name, time or address.
- Coarse, or a body counted as interface (it joins the piece, so its readers' keys move with it):
  a macro outside item position not on the quiet list (the crate's whole print); const fn, async
  fn and return-position `impl Trait` bodies; generic and `#[inline]` bodies instantiated
  elsewhere; trait impls selected by method calls, through the impl index under the self type's
  names (the trait's name for a generic, primitive, alias or non-path self), narrowed to impls of
  types reachable from the types the body names, plus blanket impls.
- Soundness (rule 21): (a) a checker can only look up names its text mentions and types in their
  closure, and the key covers each by content, absences included; (b) a read list is exactly
  what was read, negative reads included, its first read the record's own text, and reuse needs
  every one unchanged. The simulation holds both to a from-scratch build.
- The honest over-approximation: a signature edit to a common name (`new`, `from`) moves the key
  of every body in the universe that mentions it; each is a candidate (a fetch and a compare), and
  only the N true dependents of the C are redone.
- Firewalls: a body edit changes no surface fact, so no other crate is looked at. What crosses: a
  signature, a field, an impl, a const's value, an inlinable body's code key (code records only).
- Content-addressed code, as Unison has it, at the grain that keeps edits fast: an item's key is a
  hash over its own text and the interface digests of what it names (a DAG of hashes); code
  records are shared by content across crates, versions and machines; names and positions are not
  code identity. Not taken: a body's key holds the interfaces of what it calls, not their bodies
  (for code, only the bodies it inlined), because holding bodies would re-key every caller on
  every body edit. Source stays in files.

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
  unchanged surface means no dependent is looked at; a holding unit manifest settles every body it
  names; only under a parent that moved is the next level compared.
- An equal key is followed by no other check. A read list is verified at most once per (item,
  snapshot), and its result is filed under the new key so the next request hits by key.
- Bytes are verified once: a segment this process wrote is trusted; one from an earlier boot is
  checked in full once, recorded in the manifest; a fetched one's signature and digest at fetch,
  recorded. A read of a record in a verified segment checks nothing.
- A file is hashed again only when its stat stamp moved (the racy-stamp rule decides the edge);
  with a watcher journal, only the paths it names are looked at.
- A put of bytes the store holds writes nothing. Nothing syncs on the edit path.
- Counters: checks per kind per request; a second check of one thing in one snapshot is a bug the
  simulation's oracle catches.

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
   images; body records with key and read list; `blaze-memo`; `blaze-program` (instance records,
   the streaming monomorphization walk, programs as patched manifests); `blaze-expansion`
   (macro-call and script-run records); test records.
3. Deletions, each after its replacement passes. 4. The fetch seam with std's records; the matrix.

## Decisions made where the audit left a choice

- Both traces from the start, not keys alone: incremental time comes first and disk last. A unit
  manifest is filed after every record it names, so one read settles a unit.
- An instance is keyed by its body's *result* digest, so a reuse keeps its code; a reuse is a
  re-headed byte copy, not a pointer record (one read later, not two).
- Commands run `RecordsOnly`; a build's helper `Resident`. The crate is `blaze-keyed`:
  `blaze-record` (the read-list form it replaces) has the name.

## Floors and expected cost, a million-line workspace (estimates, rule 67)

Floor: an oracle that redoes only what changed and its direct readers, plus unavoidable IO.

| Operation | Floor | This design, 128-core | This design, 16 GB Mac | Ratio |
|---|---|---|---|---|
| No-op | journal read + certificate: 2 ms | 3-5 ms | 3-5 ms (FSEvents) | ~2 |
| Comment edit | reparse one file + line table | + manifest key, all hits: 5-10 ms | same | ~1.2 |
| Private body edit | one body check | start + key + 1 body + 1 instance: 10-30 ms | 15-40 ms | ~1.2 |
| Signature edit, N of C | N bodies | N bodies + C read-list compares (~1 µs each) | same, fewer cores | ~1.1 at C ≤ 100N |
| Add an item | its body + names now found | the same as a signature edit of its name | same | ~1.1 |
| Add a derive | one expansion + its item | one Wasmtime call + the derived items | same | ~1.2 |
| Restart then edit | the edit | the edit (no state to restore) | same | ~1 |
| Memory between edits | 0 private | 0 one-shot; file-backed pages when resident | same | ~1 |
| Disk | the records | records + read lists + a 40 B index entry each | same | ~1.3 |

What cannot reach the floor, plainly: the over-approximation of a common name's signature edit
costs C − N compares on top of the floor. That is cheap per compare, but unbounded in C.

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
  build, and `Resident` must refine `RecordsOnly`. Six planted bugs are caught: a name left out
  of a key, late inputs left out, a dropped read, an over-committed pool, an early manifest, and a
  stale resident entry.
