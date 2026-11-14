---
status: plan
summary: A proposal to change how target.md finds a body, a surface and a unit manifest. Today each is keyed by its text and everything reachable from it by name, with a read list checked and the record copied under a new key when the key moves. Proposed, each is found by what it read, so an unrelated edit moves no key, nothing is refiled or copied, and correctness rests on one claim (the read set is complete) and not two. Says what was found in the code, the change as an outside review amended it, why it is sound, its cost against the floor, and what it does to the crates built on blaze-keyed. Not decided.
date: 2026-11-13
---
# Read-addressed records: find a result by what it read

Proposed by the main agent of the test-engine work (branch dst-1113), on the owner's invitation
to change what in the target design makes it uneasy, 2026-11-13. It changes the mechanism of
decision D35 and of `crates/blaze-keyed`, which another session is building on, so it is a
proposal until the owner decides. An outside review (a fresh agent with this page and the code,
not the author's reasoning) said: adopt with changes; its changes are folded in below and marked
(review). Every number is an estimate; nothing here was measured.

## What was found (main at 4630d471fa, read not run)

1. **A reuse copies the whole record.** On a key miss whose read list verifies, the driver reads
   the whole old record, copies it three times in memory and puts all of it again with 32 bytes
   changed (`blaze-keyed/src/driver.rs:1059-1083`, `record.rs:305-312`). For a body that is its
   MIR and inference. Every read path checks the head's key (`record.rs:238`), so a key cannot
   name another key's bytes.
2. **Most keys move on most interface edits.** A body's key hashes, for each name its text
   mentions, the closure of every node of that simple name in its universe, transitively
   (`text.rs:189-258`). One changed signature moves the key of everything upstream of it in the
   reverse name graph. target.md calls this "the honest over-approximation"; with (1) it is a
   copy of most of the workspace's body records per such edit.
3. **Keys cascade through units.** A surface is keyed by its dependencies' surface keys
   (`kind.rs:452-454`) and a unit manifest by them too (`kind.rs:779-795`), so an unrelated
   signature edit re-keys, verifies and copies the surface and manifest of every unit above it.
4. **The key rule has holes for real Rust, and nothing checks it.** Keys cover names the text
   mentions. `a + b` does not mention `Add`; nor do `?`, `for`, a deref, an auto trait or a
   "did you mean" suggestion mention what they look up. An equal key is a hit with no check, so
   a hole is a stale answer. The simulation cannot see one: its model's reads are the bindings
   of exactly the names its key covers (`tests.rs:299-305`, `373-382`). There is no check that a
   record's reads are covered by its key and no mode that recomputes a hit.
5. **Computing keys does not scale as written.** The graph has an edge from each mention to each
   node of that name (`text.rs:189-203`), is built per request over every crate with nothing
   kept, and each (body, name) hashes all nodes under the name again (`text.rs:226-258`).
6. **To check one candidate the driver reads its whole record** (`driver.rs:716-727`, `1059`);
   read lists sit inside the payload's record (`record.rs:46-51`); no unit holds a summary of
   what its bodies read.
7. **Tests.** A test's key is a digest of everything its entry reaches, found by a walk per test
   over a manifest decoded whole (`blaze-program/src/reach.rs:53-130`). Every reached span's
   (file, line) is in the digest, so a line added above a reached function re-keys the test.
8. **Nothing runs the driver.** No crate outside blaze-keyed's tests constructs `Driver` or
   implements `Pool`; no front-end crate depends on blaze-keyed; no surface is made from source.
9. **The plan key disagrees with itself**: by the body's key (`kind.rs:511-524`), by its result
   in the doc beside it (`kind.rs:558`) and in target.md.

Also found, for the store lane: a new process touches every index page of every segment
(`blaze-cas/src/plain/core.rs:1698-1704`); every `Store::put` takes one writer lock and the
per-worker batches are unused on that path (`handle.rs:186-203`); a miss probes every segment.

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
claim (a) of target.md and with it the holes in (4). What stays: surfaces as the firewall, unit
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
would ask the same questions and reach the same result. Trace (b) of target.md already rests on
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
text keys are the deep form; target.md added read lists beside them, and this keeps the read
lists and drops the text closure.

**Lost alternative.** Keep text-closure keys and repair around them: an index entry that
aliases bytes in place of a copy, surfaces and manifests keyed by result digests, a per-unit
summary of facts read. The review weighed it: it fixes (1), (3) and (6) and keeps the candidate
scan on every interface edit, the implicit-lookup holes and a name graph built per request.

**What text keys give that this gives up** (review): every key exists before any work, so a
remote action is named and a fetch batched without settling dependencies first (the marks in
the shared cache answer the fetch; a remote action is named by its address once its unit's
surfaces are settled); keys do not depend on what a store happens to hold. What this gains that
the first draft missed: a body's key is its own text alone, so a machine whose workspace
differs low in the graph still finds CI's bodies, where today all those keys move.

## Cost against the floor (estimates; U units, d a unit's dictionary, a million lines)

| Operation | Floor | target.md as coded | This |
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

## What it changes in the tree

- `blaze-keyed`: `kind.rs` (Body, Surface, UnitManifest keys; the mark; the facts a body makes;
  plan by result digest), `driver.rs` (the reuse path goes), `record.rs` (`reheaded` goes; a
  manifest's root, chunks, dictionary, ordinals and postings), `outside.rs` (stays for outside
  reads; inside kinds do not use its slots), `text.rs` (deleted), `tests.rs` (the model reads
  by value; the plants above).
- `blaze-program`: a delta is the items whose body result moved; reach digests per function.
- `blaze-findings`: per-body records keyed by the body's result digest.
- `blaze-memo`, `blaze-cas`, `blaze-remote`, `blaze-expansion`: no interface change.
- target.md: the table, "Validity is two traces", "Key rules, exactly", the floors.

## Order

1. The owner's decision.
2. `blaze-keyed` rewritten as above with its simulation, as one piece, by the session that owns
   it or with its agreement; the crates on it ported in the same piece.
3. In the same piece, not after it (review): the driver hosted for real on hello-world, a
   surface from source and one body checked from records, so that how many facts a body reads
   and how often a shape changes are seen before more is built on the interface.
