---
status: plan
summary: The request certificate: one store record a request shape and content that says "this request is answered already, by these outputs", checked by the daemon's loop before any plan, view walk or job; the walk is what happens when it does not hold, and it starts at the units that read the input that moved.
date: 2026-11-12
---
# Request certificates

Lane certificate-1112. Code: `crates/blaze-cargo/src/serve/job/certificate.rs` (the record, its
codec, the check; sans-IO) and its tests. The owner, on a repeated `blaze run` of a hello-world
being a whole walk (1,162 `job` and `task` spans, 1,195 `admission`, 160–700 ms; reth's no-op
2.0–2.8 s): "is our architecture jank? what does the competitor do". makepad Relax answers a
hello-world no-op in 52 ms with nothing held between runs: one certificate for the whole program
(identity, every input with its hash and the stat it was taken under, the paths probed), read,
checked, and the saved image mapped. blaze had the same idea bolted on as an exception
(`noop::LastAnswer`: memory only, gone at a restart, kept only of a build that made nothing, with
eight ways to be refused). This turns it round: the certificate is the answer; the walk is the
miss path.

## What a certificate holds

One per request shape and content (`Certificate`):

| field | what | compared by |
|---|---|---|
| `blaze` | the blaze that served it (release version; the toolchain is in `config`: the library and std mounts are by content id) | equality |
| `shape`, `config`, `request`, `drawn` | the request: its config as the plan reads it; its config less the environment; what it says past those; how its client is drawn (an input only when the build said a diagnostic). The checkout's own directory is replaced by a marker first (`relocated`), so no absolute checkout path is in a key | equality |
| `root` | the view's root id, read AFTER the build's own writes into the view (lock files: `Job::built_root`) | one compare; equal ends the file check |
| `groups` | the checkout's files the build read, as nodes of the VFS tree: a directory by its Merkle id (`Pinned::dir_id(root, path)`: every file below and every absent name: a new `build.rs`, a module file, a member's `Cargo.toml` change it), or one file by its id (`Cargo.toml`, `Cargo.lock`, `.cargo/config.toml` at the root); `None` for a node read as absent. Each with `readers`: the units that read it. Which: one a package root directory of the plan (less nothing: the tree already leaves out what the watcher ignores, `target/` and `.git/`: to be confirmed by the runner, see State), plus the workspace root's files. A path dependency outside the workspace (bstd) is a group like any other: id from its mount in the tree when its watch vouches, from a direct read of the directory when none does (span `request certificate: a directory read, no watch vouches`, with the path) | `O(groups)` id compares, only when `root` differs; never a stat a file |
| `env` | each variable a step read (`read_by_plan`, the walk set, `BLAZE_*`, and each name a build script, proc macro or `env!` asked for) with the hash of its value, `None` unset | per name; a variable nothing read is in no certificate |
| `outside` | each file outside every view a step read (a header, the C compiler) with its content hash; `None`: looked for and absent (a negative probe). A link on the way to a directory the build named is an entry by where it points | one look each |
| (listings) | a directory outside the views a build script listed (blake3's, ring's, zstd-sys's globs): an `outside` entry by the hash of its listing (direct names, which are directories); a listing through a link records the link as a probe and the resolved directory's listing. In memory the watches' generation vouches for these (lane reth-1112's rules 1–4); the record holds the content, so a restart re-reads each once | one listing each, cold only |
| `outputs` | each output: base (target dir, build dir, std's builds), path under it, store key, and the stamp (length, change time) it was placed under | one stat each |
| `units` | the units' record keys in plan order: what `blaze why` reads of an answered request and what `readers` index | not compared |
| `said` | what a build with nothing to do says (diagnostics, units, artifacts) and its `Work`, deterministic CBOR | replayed |

## Where it lives

A store record, `Kind::Query`, domain `serve request certificate`:
`key(shape, root, blaze)`: content alone, so a restarted daemon finds it, and a second checkout
of the same content finds it too (its outputs are not placed: the failure names an output and no
unit; they are placed from the store by their keys, no unit walked). Beside it one slot a
checkout and shape, domain `serve request certificate slot`, `slot(shape, dir)` → the 32 bytes
of the key of the certificate that checkout's last build left: how a request whose content has
no certificate (an edit) still finds the last one and learns which group moved. The slot is the
one record keyed by a directory: local to the machine, never sent to a remote store.
`noop::LastAnswer` becomes a cache of the decoded record with the watches' word beside it (a hit
in memory skips the looks at `outside` and `outputs` while the watches vouch); it is never the
only copy.

## The check

`Certificate::holds(now, looks)`: one record read, the compares above, the looks. Done by the
daemon's loop in `Jobs::begin` before anything else (today's `answer_again`), after the
checkout's watcher's flush for this request is in (`serve::heads` already holds a request for
it), so the root compared is never older than a write made before the request was sent. On a hit:
no `job`, `task` or `admission` span, no plan, no view opened in the database.

The thin client does NOT check without the daemon. It could map a file holding "root id R at
watcher cursor C", but a write the watcher has not delivered is exactly what that file cannot
vouch for, and only the daemon can flush its watcher. So: one small request, answered by the loop
with the certificate's `said` and the program's path; the client then execs. If a later
measurement shows the round trip matters (it is a Unix-socket request: expected well under a
millisecond), the daemon can answer "root R as of cursor C, flushed for you" and the client do the
compares; not built now. Correctness first.

## Why a certificate fails (the invariant)

**A certificate is given only when a fresh walk would produce the same outputs and say the
same.** And it stops holding ONLY when one of these differs, naming which (`Failed`):

1. a recorded input's content: a source, a manifest, a file a build script or proc macro read, an
   `include!`d file (`Failed::Group`, `Failed::Outside`);
2. a probe that flipped: absent → present (a `build.rs`, a module file, a member's manifest: in a
   group's directory id; outside the views: `Failed::Outside { appeared: true }`), or present →
   gone;
3. the value of a variable the build read (`Failed::Env`);
4. the request shape: profile, target, features, packages, arguments (`Failed::Request`; a
   request for a trace is such a request);
5. the toolchain or blaze's identity (`Failed::Blaze`, or `config` for the toolchain);
6. an output it promised: deleted or changed (`Failed::Output`).

Nothing else: not a restart (the record is read back), not another request's walk (its outputs
are other paths; no "alone" condition), not the build's own writes (the root is taken after the
lock files), not a link in a path (recorded as a probe with the resolved directory's id), not a
file outside the workspace (a group, above), not a variable nothing read, not the order of equal
values (every list is sorted before it is encoded).

The one hard case: a step blaze could not trace (a build script run outside the sandbox with no
record of its reads). Its inputs cannot be claimed, so that build writes NO certificate and says
so once per build, naming the unit (a warning diagnostic, and the span `request certificate: not
written, an untraced step`), and the count of such units is a column of the build's profile row.
Never a silent walk forever (rule 3). The fix is tracing that step.

## Granularity (rule 65)

A certificate is per request: it is the quick first check that nothing moved, which rule 65
allows; it is no entry's key. When it fails on a group it hands the walk that group's `readers`:
the walk starts at those units and goes on to their dependents only (`Certificate::walk_from`),
each of which then asks its own per-item records. Units visited ≤ readers + their dependents;
tested at two graph sizes. A failure that names no unit (an output gone) walks none and places.

## When it is written

At the end of every build that answered (passed), by the after-answer settle, never delaying the
answer: the settle derives `said` as a no-op build says it (from the units' records: the same
rendering the walk uses when it finds every record), stamps the outputs it placed, and puts the
record and the slot in the settle's own store batch. Not only after a build that made nothing: a
build that compiled writes the certificate of the state it left, so the very next request is
answered. Not written: a failed build; an untraced step (above).

## `run` and `test`

The client sends both as a build. The build part is answered by the certificate (`said` holds the
artifact's path); the client then starts the program in its own environment, which is no input
of the build. `test` with a selection: the selection is the program's arguments, not the
build's, so the same certificate answers; what else it needs is nothing for the build. (Kept test
results by test are another record, lane test-results; not this one.)

## What of noopkeep's code goes

- `noop::Answers`, `MOST_SHAPES`, `Shaped`: go. The store holds the certificates; the bound is the
  store's.
- `noop::LastAnswer`, `Recording`, `Walk`, `Unkept`: become the in-memory cache entry of a
  `Certificate` plus the watch generation; `Unkept::{Planned, Made, Said, Looked, Outputs}` go
  (they are restrictions of the exception, not reasons a certificate fails); `Unkept::Failed` and
  the untraced case stay.
- `NotReplayed`, `Moved`: replaced by `certificate::Failed` (six reasons).
- Kept: the comment-only-edit lane (`code_unmoved`, `StillFile`, `FileEdit`) as a refinement of
  reason 1 (a group moved, but every changed file's code text is as it was: the certificate is
  rewritten under the new root, no unit walked); and the simulation model and oracle in
  `noop/tests.rs`, extended with: restart, `build, run, build`, `build, test, build`, second
  checkout, lock files written by the first build, a linked output directory, a path dependency
  outside, an unread variable changing every command, a write the watcher has not delivered.

## Tests and controls

In `certificate/tests.rs` (written): each of reasons 1–4 and 6 fails and names the thing, with a
planted check that gives it and the oracle catching it; reason 5; restart from bytes; an unread
file and an unread variable do not fail it; a second checkout has the same key and fails only on
outputs; the walk after an edit visits readers and dependents at 4 and 64 units; round trip with
identical bytes, layout 0 refused by name, every cut refused at a field. To write with the
wiring: the cases listed above in `noop/tests.rs`, and the revision cases
`a_repeated_run_executes_no_job` and `a_no_op_after_a_restart_executes_no_job` (max jobs 0).

## Expected numbers (not measured: guesses to be checked)

Hello-world and the bstd hello-world, repeated `blaze build` / `blaze run`: 0 `job`, 0 `task`,
0 `admission` spans; daemon side under 2 ms (one record read, about 220 stats of outputs on the
first request after a restart, none while the watches vouch); end to end under 20 ms, most of it
the client's start. reth no-op: from 2.0–2.8 s to under 50 ms (about 30 groups, a few thousand
output stats cold). First request after a restart: the same, plus the tree's restore.

## State (2026-11-12, lane certificate2, paused for `end-state.md`)

In the tree, not compiled by anyone yet: `serve/job/certificate.rs` and `certificate/tests.rs`
(record, codec, keys, `relocated`, `holds`, `walk_from`; `cargo test -p blaze-cargo
certificate::`), `mod certificate;` in `serve/job.rs` (the module is `#![allow(dead_code)]`), the
`request-certificates` row and Domains line in `caches.md`, the annoyance row. Nothing else: no
hunk of `job.rs` or `noop.rs` asks or writes a certificate. The wiring is paused (rule 66): the
request loop is being redesigned around the certificate as its own sans-IO state machine.

What reading the daemon showed, which the design above does not yet say and the end state must:

1. **The store is write-once.** `Store::put` of other bytes under a kept key is a `Mismatch`
   (`blaze-cas/src/lib.rs`, `put_in`). So neither `key(shape, root, blaze)` nor `slot(shape,
   dir)` can be one record as written above: a certificate's bytes are not a function of its
   key (the values of variables read, outside files' hashes, outputs' stamps and `said` are in
   the bytes and not the key), and a slot is a pointer that moves. The tree's idiom for this is
   a bounded chain (`blaze-usercode/src/index.rs`, `slot(run, n)`): `key(shape, root, blaze, n)`,
   the reader checks each until one holds, the writer puts at the first free `n`. Stamps in the
   record make the chain grow by one each time outputs are placed again for the same content (A,
   B, A, B). The clean split: the record holds content only (an output by the hash of its
   bytes; then equal builds are equal bytes and a put is idempotent, and the chain has one entry
   an environment); stamps are local evidence beside it (memory, or the daemon's image), made
   again by hashing an output once when missing. Whether `Placement.key`'s store value is the
   placed file's bytes (a patched image may not be) is not read.
2. **`said` names the checkout.** `Response::Built`'s artifacts are absolute paths, so a
   certificate found by a second checkout would say the first one's paths. Either `said` is
   relocated as the shape is (a CBOR transcode of every text), or a certificate whose `said`
   holds the directory is kept under `key(slot(shape, dir), root, blaze)` (local) and only one
   without it under the shared key; the reader asks local then shared.
3. **Where each part is known.** `Jobs::keep_answer` has no `World` (it is called from
   `Jobs::heard`): no store, no VFS. The parts: the responses said (`Recording`, whole at
   `heard`'s `Finish`); the view's root after lock files (`Recording`'s `inputs.view`, recorded
   again by `open` after `open_locked`); the variables read (`writer.env_named()`, asked in the
   lowering's end, about `job.rs:8100`, only of a build that made nothing so far); the whole list
   of outside reads (`writer.read_outside()`, same place: paths only, `(files, listed)`, no
   hashes; `Recording::vouch_reads` cuts it to those under output directories, so the whole list
   must be taken before); outputs and stamps (`Paths::placed[..].meta`, set by `Done::Stat`,
   memory only, empty after a restart); the plan (`Roots::plans[root]`, by shape:
   `units.packages[..].manifest_path`, `units.plans[..].outputs`); a `World` with the store and
   the VFS at `Jobs::step` and in `wind_up_timed` (where `recorded.walked` is set). Hashes of
   outside files taken at the build's end are not what the build read if a file changed during
   it: the traced runs' own recorded hashes (`serve/unit/reads.rs`) are the right source.
4. **Variables read unasked.** `read_unasked(name)` (plan variables, `BLAZE_*`, the walk set) is
   a predicate, not a list: a certificate's `env` must hold every such variable set at the build,
   and the check must fail on one set now that the record does not name (it was unset).
5. **Looks.** `Looks::node`: `w.vfs.reader().pin()`, `dir_id(root, path)` else `file(root,
   path)`'s id. `Looks::outside`: `blaze_usercode::inputs::Disk` (`file`, `dir` for a listing),
   as `Watched::same_file` does. `Looks::output`: the driver's `Meta` (`len`, `modified_ns`); a
   direct `std::fs` look from the loop is IO outside the driver and is not seen by the
   simulation.
6. **The relocated shape** (item 1 below) was written and taken out again at the pause, since
   nothing reads it: in `Jobs::viewed`, clone the config, `relocated` over `invocation.root`,
   `cwd`, each arg, each variable's value, each `files` path and each mount's `at`, sort the
   mounts again (where the checkout is must not order them), then `bare` and `shape_of` from the
   clone; `planned` and the plan's `key` stay as they are.
7. A path dependency outside the workspace (bstd) needs its mount's directory id from
   `Mounts`; how to read it is not looked up. Answered by lane records-restart2, by reading the code, not by a
   run: `Pinned::dir_id` is the node's own Merkle id, so it covers every entry the tree holds
   under the directory and leaves nothing out itself. What a checkout's view holds is decided
   by `serve/heads.rs` `excluded(rel)`: a `target/` at the checkout's ROOT and every `.git`
   at any depth are out of the view, so neither moves any directory's id. A `target/` that
   is not at the root (a member package's own `crates/x/target/`, a `--target-dir` elsewhere
   in the checkout) IS in the tree and moves the id of every directory above it; so does a
   nested package's edit move its enclosing package's id. blaze's default build directory is
   outside the checkout. So a group by a package root's `dir_id` is exact for a workspace
   whose only in-checkout build directory is the root's `target/`; for the rest the group
   needs the skipped sub-paths, as `blaze_lower::found::dir_key(mounts, dir, skipped)` does
   (the id with what the load skips left out: one node when nothing is skipped under the
   directory, a listing along the paths to what is). A mount's directory id: `Mounts::locate`
   gives `(root, path under it)`, then `Tree::dir(root, path)`. To confirm by a run: build,
   touch a file under a member's own `target/`, and compare the member directory's `dir_id`.

The work, for whoever the end-state spec assigns it to:

1. The relocated shape (6).
2. The check before any job: record chain read, `holds`, the unasked-variable check (4), `said`
   said again.
3. The record written after every build that answered, from a build that compiled too: needs
   `said` as a no-op build says it, which today only a build that made nothing records.
4. An untraced step: no certificate, one warning a build naming the unit, a profile count.
5. On `Failed::Group` the walk starts at `walk_from`; outputs placed from the store for a second
   checkout (`Group::readers` and `Certificate::units` need the unit of each package and the
   units' record keys: not looked up).
6. Deleting what it replaces (`noop::Answers`, `Shaped`, `MOST_SHAPES`, `Unkept::{Planned, Made,
   Said, Looked, Outputs}`, `NotReplayed`, `Moved`); the simulation cases and the revision cases
   listed under "Tests and controls".

To measure once wired: repeated `blaze build` and `blaze run` on hello-world, the bstd
hello-world and the smoke projects, in milliseconds, with the counts of `job`, `task` and
`admission` spans (0 on a hit); the same after a daemon restart; reth's no-op row.
