---
status: plan
summary: "How a body, a surface and a unit manifest are found: by what their work read, not by their text and everything reachable from it by name, so an unrelated edit moves no key, nothing is refiled or copied, and correctness rests on one claim (the read set is complete). Decided as D39 and built in blaze-keyed. The change, why it is sound, its cost against the floor, tests, and the end state of what an edit costs, with the pieces of it built and left."
date: 2026-11-16
---
# Read-addressed records: find a result by what it read

Decided as D39 and built in `crates/blaze-keyed` (`driver.rs`, `record.rs`, `kind.rs`,
`front.rs`, `memo.rs` with `Audit`, the simulation in `tests.rs`). Where this page and the code
differ, the code's doc comments say what was built: the certificate's 64-shape cap stays (it is
an outside record) but costs a mark lookup per unit on the next request, never a failed build.

## The change

A derived record is found by what its work read. Its key says what it is (its own text and
settings); its *shape* is the set of facts it asked for; the key, the shape and the digests
those facts have now hash to the *address* of its value. A changed fact is a new address; an
undone one finds its old value; a fact it never asked for moves nothing.

- **Body.** Key: tool, settings, the item's identity and its own text. Shape: the facts it
  read: a binding in a scope, a signature, fields, a layout, the impl rows for (trait, self
  head), a const's value, an expansion's result, absence included. Value: inference, MIR,
  verdict, diagnostics.
- **Surface.** Key: tool, settings, the unit's own pieces. Shape: what it imported from other
  units' surfaces, and its late reads. Value: the unit's facts, one digest each, indexed so an
  ask is one lookup in the mapped table. Private fields and inner attributes that change how
  code is checked (`recursion_limit`) are facts too (review).
- **Facts a body makes** (review). Some of what others read is a body's result, which is why
  `text.rs` counts those bodies as interface: an `async fn`'s or return-position `impl Trait`'s
  hidden type with its auto traits; a `const fn`'s MIR as const evaluation reads it; the items
  and impls declared inside a body. Each is a named fact its body's task writes and its readers
  ask for. This is finer than today: an edit to an async body that keeps its future `Send`
  moves no reader.
- **UnitManifest.** Key: tool, settings, the unit's file keys and late reads. Value: a root over
  one chunk per file; each entry is (item, kind, key, address) and its shape as ordinals into
  the unit's dictionary of outside facts; beside it, postings from ordinal to items, inside the
  unit only. Filed by content.
- **Shapes live in the manifest** (review). The numbered slots of `outside.rs` do not carry
  over: they cap at 64 and fail the build there (`outside.rs:35-38`, `344-347`), a quiet unit
  under a busy one fills them in weeks, a lookup scans them oldest first, and two builders
  racing on one key file different bytes at slot 0. An item's shape is found in its unit's
  manifest entry; a shape is also filed at the hash of its key and its own digest, for a
  machine with no manifest. No cap: an item whose shape is not found is redone.
- **A mark**, tiny: (unit manifest key, the unit's universe digest) names the manifest. The
  universe digest is the unit's own surface result digest hashed with its direct dependencies'
  universe digests, so it is made once per unit (review). It is the fast path: nothing the unit
  can read moved, so one small lookup settles it. The shared cache indexes CI's marks by
  manifest key, which is made from text, so a first fetch is one batched round trip (review).
- **Nothing is keyed by another record's key.** What a dependent sees of a unit is its
  surface's facts. An instance plan is keyed by its body's result digest.
- **Shapes are sets, sorted** (review): order is not needed for the argument below, and a set
  cannot differ with thread timing or replay order (rule deterministic-output).
- **A cache shared between tasks carries what it read** (review). The reused front end keeps
  results across bodies (the solver's caches, layouts). A hit there adds that entry's reads to
  the asker's shape, or the shape is not complete.

What goes: `text.rs`'s closure keys, `Trace::Reuse`, `reheaded` and the refiling, soundness
claim (a) of bottom.md and with it the holes in (4). What stays: surfaces as the firewall, unit
manifests as the coarse proof, verify once, no graph in memory, no postings across units,
records immutable and shared by content, the outside form for what a run reads outside.

## A request, step by step

1. The certificate, as now.
2. Units in dependency order. A unit whose files and universe digest are as they were is settled
   by its mark: one lookup.
3. A unit under a surface that changed: the facts that changed are the difference of that
   surface's old and new tables when the old one is at hand (it is named by the last request on
   this machine), merged against the unit's dictionary; with no old table (a new machine, a
   branch switch) the dictionary is answered whole. No changed fact in the dictionary: the
   manifest stands, write a new mark, look at no item.
4. Else the postings give the items that read a changed fact. Each: its address from its key,
   shape and the answers now; a hit (an undo) or the body is redone and its shape filed.
5. A unit whose own file changed: its surface by the same steps; items whose text is unchanged
   and whose shape holds no changed fact keep their entries; the edited item is redone, and a
   fact it makes (above) that changed is a changed fact for its readers.

## Why it is sound

One claim: a record's shape holds every fact its work read, absences included, and what the
work asks first depends only on its key. Then answers equal to a recorded run's mean the work
would ask the same questions and reach the same result. Trace (b) of bottom.md already rests on
this; the key rule for names, with its list of what the language looks up implicitly, is no
longer needed. The claim is harder to keep than it reads, because the front end blaze reuses
was built around shared caches (review), so it is guarded three ways, built with the change:

- the task reads facts only through the reader that records them, and a shared cache entry
  replays its reads;
- the simulation's model reads by value (what one answer is decides the next ask, as method
  resolution does), with a plant each for: a dropped ask, a dropped absence, a cache hit that
  does not replay, a fact a body makes that its reader did not ask for, a dictionary missing a
  fact;
- an `Audit` memo (a third instantiation of `Driver<M, X>`) treats every hit as a miss, redoes
  it and compares bytes, in CI over edit scripts on the acceptance projects.

## Prior art

Build Systems à la Carte (Mokhov, Mitchell, Peyton Jones, 2018): a key over everything
reachable is a deep constructive trace, which cannot cut off early; a key over the values read
is a constructive trace, which can, at the price of settling dependencies first. Shake and
Bazel's discovered inputs work this way; ccache's direct mode is the same two steps. Relax's
text keys are the deep form; bottom.md added read lists beside them, and this keeps the read
lists and drops the text closure.

**Lost alternative.** Keep text-closure keys and repair around them: an index entry that
aliases bytes in place of a copy, surfaces and manifests keyed by result digests, a per-unit
summary of facts read. The review weighed it: it fixes the whole-record copy on reuse, the cascade of keys through
units and the whole-record read per candidate, and keeps the candidate
scan on every interface edit, the implicit-lookup holes and a name graph built per request.

**What text keys give that this gives up** (review): every key exists before any work, so a
remote action is named and a fetch batched without settling dependencies first (the marks in
the shared cache answer the fetch; a remote action is named by its address once its unit's
surfaces are settled); keys do not depend on what a store happens to hold. What this gains that
the first draft missed: a body's key is its own text alone, so a machine whose workspace
differs low in the graph still finds CI's bodies, where today all those keys move.

## Cost against the floor (estimates; U units, d a unit's dictionary, a million lines)

| Operation | Floor | bottom.md as coded | This |
|---|---|---|---|
| Private body edit | 1 body | 1 body + the unit's keys rebuilt from the name graph | 1 body + U marks (one lookup each) + one file's chunk of the manifest |
| Signature edit, N readers of C candidates | N bodies | N bodies + key rebuild over the reverse name graph + C records read whole, verified and copied + every unit above re-keyed and copied | N bodies + per unit above, the changed facts merged against its dictionary (changed + d) + postings |
| Edit in a crate everything depends on, read by nothing else | 1 body or 1 fact | as above over the whole workspace | U marks rewritten |
| Undo | nothing | found by key if the keys return | found by address |
| First fetch from the shared cache | one batched round trip | one batched round trip per shard | one for the marks, one for what they name |
| Disk per edit | the new records | a copy of every reused record | the new records, the touched chunks, a mark per unit under a changed surface |

What does not reach the floor, plainly: a signature edit recomputes its own unit's whole
surface (name resolution is per unit); a unit under a changed surface costs a merge against its
dictionary even when it read none of the changes; a manifest for a 100,000-line crate is 5 to
7 MB (5,000 bodies of about 150 reads, a dictionary of 20,000 to 50,000 facts), which is why it
is chunked by file; a remote action cannot be named before its surfaces are settled.

## Tests

Keep with each function a reach digest: its instance key (which covers what it inlined), its
statics and its callees' reach digests, hashed, a strongly connected group as one (review: the
first draft left out statics and inlined instances). A test's key is its entry's reach digest.
After an edit the digests are redone upward from the changed functions through the manifest's
caller table, and a group that merged or split is recomputed over its ancestors: that is the
affected tests, with no walk per test.

Lines leave the digest only if a test's reading of them can be seen. Location data is read at
run time, so the launcher, not the test process, takes the fault of a first touch of a location
table kept in its own page-aligned segment (on macOS by exception ports set at spawn, on Linux
by a handler chained after std's or userfaultfd), and a read of the binary's debug sections
(a backtrace) counts as reading lines too (review). Until that is built, lines stay in the
digest, and that cost is stated: a line added above a reached function re-keys the test.

## End state: what an edit costs (architect, 2026-11-16)

What a command redoes and reuses, stage by stage. Mechanism: the drive's (`key::task_key` =
H(tool, kind, subject, source), `key::value_key` = H(key, sorted reads), answers.rs); the
pipeline stands: drive.md, "End state: the whole pipeline on the command". Rules 77, 21, 65, 71,
74, 52, 25. the certificate stays superseded (bottom.md 9); caches.md inventories the old layer
(daemon, query memos, crate images) and goes with it, its hash domains moved beside `domains`;
caches.md's rules 1-3 (typed, place-free key fields) stand.

| Stage | Key (text, before the work) | Value | Read list | Proof left | (a) none | (b) body | (c) signature | (d) comment above |
|---|---|---|---|---|---|---|---|---|
| Change detection | none: a pointer per (tool, request shape) | leaf table: path, stamp, content id | none | `Unchanged` per leaf | journal or stats | 1 file hashed | 1 file hashed | 1 file hashed |
| Manifests, lock (`Units`) | manifests', lock's bytes, listings read | units | absent manifests probed | units digest | certificate | stands | stands | stands |
| Unit graph | lock bytes, manifests, target, host, toolchain id (`record::Source`) | graph (BUG1) | none | graph digest | certificate | stands | stands | stands |
| Outline (`FileItems`) | file bytes, edition | items; per item a names, a signature and a body digest, position-free; file starts; line table | none | the three digests | certificate | 1 file (1 item when large) | 1 file | 1 file: the only stage that runs |
| Surface (file) | the file's names digests (kind, name, visibility, name-making attributes; whole hash under a derive or attribute macro) | out = key | none | surface digest | certificate | equal: stop | equal unless a name moved: stop | equal: stop |
| Crate names | crate identity, its surfaces' digests | names, items, exports records | questions to other crates, answer digests (asked.rs) | exports digest | certificate | stands | stands | stands |
| Macro expansion | expander code id, input tokens, call-site hygiene | tokens with origins | env, cfg, files read | tokens digest | certificate | stands (a call in the body: redone) | stands unless its input moved | stands |
| Signature (split) | crate identity, item key, signature digest | lowered head with lifetimes | each path's answer, aliases' heads | head digest | certificate | stands | 1 head redone | stands |
| Impl candidates | per crate: crate identity, items-record digest | `NoImplsOf`, directory, buckets (blaze-surface impl_index.rs) | each header's trait path and self head answers | bucket digests | certificate | stands | 1 bucket when a header moved | stands |
| Body type check | crate identity, item key, body and signature digests (self as "self") | types, findings item-relative, made facts | (question, answer digest), blaze-typeck `ReadList` | result digest; made facts' digest | certificate | 1 body | readers replayed; moved ones redone | stands |
| Borrow check | in the body task, after types | verdict, findings | heads' lifetimes | `Checked` proof | certificate | 1 body | as types | stands |
| Lowered code | body result digest | LIR | layouts | code identity | certificate | 1 body | bodies whose result moved | stands |
| Instance code | body result digest, type-argument keys, settings, inlinees' instance keys | code, relocations | layouts | instance key | certificate | 1 instance | as above | stands |
| Program manifest | link settings, instance keys | instances, image digest | none | manifest key | certificate | 1 entry patched | entries patched | stands |
| Test result | entry's reach digest (read-addressed.md, "Tests") | pass, output | outside reads | none | certificate | tests reaching it | same | stands once lines leave the digest |
| Finding printing | finding id: lint, item, message, arguments | none | none | none | stored, printed | new and fixed only | same | re-placed by the new line table, nothing checked |

**Who read this.** The answer table (answers.rs) is the reverse index on disk: a row per name
(key, address, `readers`), paged by the subject's leading bits under a root; a changed name's
readers are one page read; an edit writes the touched pages, the root and the pointer, no
fsync. End state: pages by bytes (at most 64 KiB), not 512 rows (a body row reads ~150 facts
of 65 bytes: a 512-row page would be ~5 MB); a row holds its shape's digest and the shape is
its own record, so rows are fixed-width; past 4,096 pages the root is two-level (8 KiB
rewritten, not 32 bytes a page). Size: tempo's names read lists 35.8 MB (measured), readers
the same again. No row holds a file position (rule 71): `Row::place` does today.

**Early cutoff.** An out digest is compared where its readers' rows are read; equal ends the
walk (32 bytes). Surface: names digests, not signature digests, so a type edit never re-solves
names; crate names: exports digest, then question-grain answers (asked.rs); signature: head
digest; body: made-facts digest (empty for most), so a body edit dirties no reader; instance:
code digest. An equal digest is the proof that covers everything below it.

**Floors** (estimates: stat ~1.5 us, pread ~5 us, outline 1 us a line, a body 0.4 to 1.6 ms
from the measured 640 to 2,578 a second). tempo, ~870 checkout files (drive.md): (a) exec 1 ms
+ pointer 1 pread + journal 1 ms or 870 stats 1.3 ms + certificate key (BLAKE3, ~60 KB) + 1
probe and 1 pread = 3 ms (today 27.9 to 37 ms); (b) (a) + 1 outline (1k lines, 1 ms) + 1 body
typed and borrow-checked (~2 ms) + ~20 head preads + 3 page writes and the pointer = 7 ms
(`run`: + 1 instance, ~2 ms); (c) (b) less the body + 1 head (65 us) + 1 readers page + N
replays (~20 us each) + the moved bodies / cores = 6 ms + N x 1.6 ms / cores (today 138.1 ms,
one crate's names re-solved; with names digests names do not run); (d) (a) + 1 outline + 1
record = 5 ms. blaze (4,581 files): (a) 3 ms by journal, 9 ms by stats (installed build:
120 ms); (b) to (d) as tempo plus the larger sweep.

**One home, many builds.** Every record is valid by its key, which holds the tool; nothing is
named `last`. The pointer is a hint for the forward walk: one per (tool, request shape) in
`<checkout>/.blaze/v2/at/`, 8 kept. Two tool builds, two targets, `check` and `test` keep their
own; two worktrees share every record (no path in a key). A missing pointer costs a walk.

**Fresh clone.** The certificate key is the tool, the request and every (path, content id): a
clone hashes its files (tempo's 409,322 lines, ~15 MB, ~2 ms on 8 cores) and finds what the
machine's own content-deduplicated store holds. Fetching from the bucket is an option, off unless
chosen; chosen, a clone of a commit CI checked asks the bucket once. Hit: the answer, no task. Miss: one batched round trip for what the
walk names; public crates from the bucket, the workspace's only where the organisation opts in.

**Against the best.** Salsa and rust-analyzer: early cutoff by out digest is matched;
durability is "a checksummed archive or the toolchain is never statted"; the graph is on disk,
so a restart costs nothing, at a pread per page where salsa chases a pointer. rustc
incremental decodes its whole dep graph each session; here only touched pages, at item grain.
Bazel, Buck2 (DICE): file and action digests and a shared cache matched, without their daemon
or target grain. Zig: per-declaration dependencies, here kept on disk. Hack: saved naming table
and on-disk fan-out table are this design. Sorbet's fast path is the Surface cutoff.
TypeScript build info: signature cutoff per file, here per item. Unison: a body's key is its
own text with itself as "self". Shake, Incremental: value cutoff; Shake's traces are the read
lists. No daemon costs ~1 ms of process start and ~5 us page reads, a few hundred an edit; it
buys a free restart, no memory between edits, and no invalidation bug living in a process.

### Pieces

Built: the pointer per (tool, request shape) (`crates/blaze-drive/src/pointers.rs`), the per-item
names digest (`crates/blaze-file-items/src/names_digest.rs`), the journal sweep on macOS
(`crates/blaze-cli/src/journal_sweep.rs`), and body replay (`crates/blaze-typeck/src/replay.rs`:
a kept `ReadList` asked again of today's world, `Holds` or the first question that moved), which
is built, not wired. Left: the read list as bytes (answer digests widened to 32 bytes, rule 21),
body records kept and replayed in the body task, impl index records, answer pages by bytes with
no position in a row, and a per-item re-outline of a large file; they are pieces of the engine
(end-state.md D157) in end-state.md's order (D157, D159).
