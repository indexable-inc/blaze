---
status: plan
summary: The request certificate: one store record a request shape and content that says "this request is answered already, by these outputs", checked by the daemon's loop before any plan, view walk or job; the walk is what happens when it does not hold, and it starts at the units that read the input that moved.
date: 2026-11-12
---
# Request certificates

Lanes certificate-1112, certificate2, certificate3, certificate4, certificate5. Code: `crates/blaze-cargo/src/serve/job/certificate.rs`
(the record in `blaze-record`'s form, its codec, the local evidence) and
`serve/job/request.rs` (the request machine: the check and the write), both sans-IO, with their tests. The owner, on a repeated `blaze run` of a hello-world
being a whole walk (1,162 `job` and `task` spans, 1,195 `admission`, 160–700 ms; reth's no-op
2.0–2.8 s): "is our architecture jank? what does the competitor do". makepad Relax answers a
hello-world no-op in 52 ms with nothing held between runs: one certificate for the whole program
(identity, every input with its hash and the stat it was taken under, the paths probed), read,
checked, and the saved image mapped. blaze had the same idea bolted on as an exception
(`noop::LastAnswer`: memory only, gone at a restart, kept only of a build that made nothing, with
eight ways to be refused). This turns it round: the certificate is the answer; the walk is the
miss path.

## What a certificate holds

It is filed in `blaze-record`'s one form (end-state.md, shared page 2): base key → read-lists →
answer.

| part | what | where |
|---|---|---|
| base | `base(Shape, tool)`: the request's shape (its config as the plan reads it, its config less the environment, what it says past those; each relocated: the checkout's directory is a marker, `relocated`) and the blaze that serves it. An identity: no root id, no content | the key the lists are under |
| read-list | the names of what the build read, sorted, each once, no value (`Read`): `Node(path)` a node of the checkout's view (a package root's directory by its Merkle id: every file below and every absent name; or one file: `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml`); `Env(name)` each variable a step asked for, and each variable read unasked (plan variables, `BLAZE_*`) that was set; `Outside(path)` a file outside every view; `Listing(path)` a directory outside by its listing; `Drawn` how the client is drawn, only of a build that said a diagnostic. The note is the layout's word | `list_key(base, slot)`; past the chain's bound the oldest list is no longer searched |
| answer | `Answer`: the outputs (base, path, the hash of the bytes), the units' record keys in plan order, what a build with nothing to do says (`said`, every text relocated). Content only: equal builds are equal bytes | under the list's bytes and the digest each read had when the build read it (nothing there is a digest) |
| evidence | local, never stored, never a condition (`Evidence`, one a checkout and shape): each output's stamp with the content hashed under it (kept only when seen 2 s past its change time; else hashed at each check), and the reads and digests of the checkout's last build, which is how a miss names what moved | the daemon's memory; lost at a restart, remade by one hash an output |

## Where it lives

In the store (`Kind::Query`), write-once, as above. A restarted daemon and a second checkout of
the same content compute the same base, value the same reads and find the same answer; the
second checkout's outputs are not there, so it misses on an output and they are placed from the
store. Nothing is keyed by a directory and nothing moves.

## The check

The request machine (`request::Request`), stepped by the daemon's loop before any plan, view walk
or job, after the checkout's watcher's flush for this request is in:

1. the read-lists under the base (`blaze_record::walk`; at most 11);
2. each read they name valued now: asks the driver answers (a pinned node's id, a variable's
   hash, an outside file's or listing's digest). A list that does not name a variable read
   unasked that is set now is passed over (it was unset at that list's build);
3. the answer under the first list's bytes and those digests;
4. each output: its stamp; where the evidence does not vouch for the content under that stamp:
   stat, hash, stat again;
5. `Hit(answer)`: `said` is said again, no `job`, `task` or `admission` span; or `Miss(why)`.

Every wait is a named state (`Request::waiting`) and ends, after `MOST_IDLE` steps with nothing
told, in `Stopped::Stalled`.

The thin client does NOT check without the daemon: only the daemon can flush its watcher. One
small request, answered by the loop; the client then execs.

## Why a request misses (the invariant)

**A hit is given only when a fresh build would say the same bytes and leave the same outputs.**
A request misses only for (`request::Why`):

1. `Unfiled`: no build of this shape by this blaze answered yet (the request's shape, the
   toolchain or blaze's identity is another: another base);
2. `Moved(reads)`: a recorded read's digest differs: a source, a manifest, a file a step read, a
   probe that flipped (absent → present or back), a variable's value, a variable read unasked
   set since. The reads named are those that differ from this checkout's last build;
3. `Unnamed`: as 2, where this daemon holds no build of the checkout to compare with (a restart,
   then a change): nothing is named and the engine's walk asks its own records;
4. `Output`: an output is gone, holds other bytes or is being written: placed again from the
   store, no read moved;
5. `Traced`: the request asks for the build's trace.

Nothing else: not a restart, not another request, not the build's own writes (the nodes are
valued in the view the build read with its own writes in it, `View::Built`; a file saved under
the build by anyone else is not in that view, so the next request misses), not a file touched
and left as it was, not a variable or file nothing read, not the order steps ran in.

Not written (`request::Unwritten`), each said by name, never silent: a failed build; an untraced
step (the units named, one warning a build, a profile count); a read seen at two values; two
answers under one key (the build read something its list does not name: rule 21); the chain at
its bound.

## Granularity (rule 65)

A certificate is per request: the covering first check that nothing moved; it is no entry's key.
On a miss the machine hands the engine the reads that moved, by name; the one walk is the
engine's (shared page 5), and each unit asks its own per-item records. There is no walk over an
old plan.

## When it is written

At the end of every build that answered, from a build that compiled too: the driver hands the
machine `Built` (the nodes read; every other read with the digest the build read it at: outside
files from the traced steps' own recorded hashes, never from a look at the build's end; the
outputs with the placement record's stamps; the units' keys; `said` as a no-op build says it),
the machine values the nodes in `View::Built`, places the list (`Filing::place`) and keeps the
answer (`keep_answer`).

## `run` and `test`

The client sends both as a build. The build part is answered by the certificate (`said` holds the
artifact's path); the client then starts the program in its own environment, which is no input
of the build. `test` with a selection: the selection is the program's arguments, not the
build's, so the same certificate answers; what else it needs is nothing for the build. (Kept test
results by test are another record, lane test-results; not this one.)

## What of noopkeep's code goes

- `noop::Answers`, `MOST_SHAPES`, `Shaped`: go. The store holds the certificates; the bound is the
  store's.
- `noop::LastAnswer`, `Recording`, `Walk`, `Unkept`: go; what one machine knows beside the record is
  `certificate::Evidence`, never a condition for an answer; `Unkept::{Planned, Made, Said, Looked, Outputs}` go
  (they are restrictions of the exception, not reasons a certificate fails); `Unkept::Failed` and
  the untraced case stay.
- `NotReplayed`, `Moved`: replaced by `request::Why`.
- Kept: the comment-only-edit lane (`code_unmoved`, `StillFile`, `FileEdit`) as a refinement of
  reason 1 (a group moved, but every changed file's code text is as it was: the certificate is
  rewritten under the new root, no unit walked); and the simulation model and oracle in
  `noop/tests.rs`, extended with: restart, `build, run, build`, `build, test, build`, second
  checkout, lock files written by the first build, a linked output directory, a path dependency
  outside, an unread variable changing every command, a write the watcher has not delivered.

## Tests and controls

`request/tests.rs`: a model world (files in two package directories and at the root, three
variables, two outside files, one output; a write-once store) and a Hegel property over edits,
variables, probes, outputs changed by hand, waits, restarts and saves under a build, in a seeded
order of the driver's answers: every hit equals a fresh build, and nothing read changed since a
recorded build ⇒ hit. Planted bugs the oracle catches: a variable left out, a negative probe
left out, the root taken at the build's end, outputs not looked at, a racy stamp trusted, an
unasked variable set since. Also: the named misses, an untraced step, a list of the previous
layout refused by name, the stall bound. `certificate/tests.rs`: the codec and its layouts,
sorted lists, the base, relocation, racy stamps. To write with the wiring: the daemon-level
restore case and the revision cases `a_repeated_run_executes_no_job` and
`a_no_op_after_a_restart_executes_no_job` (max jobs 0).

## Expected numbers (not measured: guesses to be checked)

Hello-world and the bstd hello-world, repeated `blaze build` / `blaze run`: 0 `job`, 0 `task`,
0 `admission` spans; daemon side under 2 ms (one record read, about 220 stats of outputs on the
first request after a restart, none while the watches vouch); end to end under 20 ms, most of it
the client's start. reth no-op: from 2.0–2.8 s to under 50 ms (about 30 groups, a few thousand
output stats cold). First request after a restart: the same, plus the tree's restore.

## State (2026-11-12, lane certificate5)

Wired; compiled by the main agent, no test run on the wiring as of this writing: the driver
`serve/job/certify.rs`. `Jobs::begin` checks a build or a check against its certificate before
it is a job (`certify_begin`; outputs' stamps and bytes through the loop's driver,
`Own::Certify`); a hit says `Answer.said` again and finishes with no job; a miss goes on into
`begin_job`, its reason the span `request certificate: miss` and the build's `no-op answer: not
given because`. Every build of a checkout is heard from its view's last open to its `Finish`
(`certify::Building`, `Jobs::certify_heard`), and one that answered, having compiled or not,
hands the machine `Built` at the loop's next step (`certify_written`).

Closed by certificate5:

- **The tool is the binary.** The base's tool is the BLAKE3 of the running executable
  (`blaze_profile::identity::binary_hash`, the one home: what `builds.blaze` names a build's
  blaze by; hashed once a process, before the first request by the daemon's history). A blaze
  that cannot read itself fails the request, by name. (`BuildConfig::blaze`, which the units'
  keys read, is still the version: not this record's.)
- **A build that compiled writes its certificate.** `said` is made by `serve/job/said.rs`
  (sans-IO) from what any build that answered said: a package's line as `Fresh`, each artifact
  fresh, nothing of fetches or progress, the `Work` of no work (every unit found; no count,
  time, link or build id of the build that wrote it), in the plan's order (by package, by
  unit), never the scheduler's. The invariant, tested with three planted bugs (`KeepsVerbs`,
  `KeepsOrder`, `KeepsWork`): two builds of one request, whatever each compiled and in
  whatever order, leave one `said` byte for byte; else the second would be refused as two
  answers under one key. A hit therefore says a walk's words in the plan's order, where a
  walk says them in its scheduler's.
- **The placement's stamp is handed over at the write** (`Placement::asked_ns`: the clock
  as its look was asked for, so no later than the look), under the racy rule: an output the
  build found placed is a stat from the first check on; one it wrote within two seconds is
  hashed once by a later check.

What the wiring does not do yet (each a known gap, in the order to close them):

1. **A daemon's first request is not checked**: the config names where the standard library
   is mounted (`/rustc/<commit>/library`) and holds each mount by content id; `Jobs::library`
   is set by that daemon's first task (`Does::Library`: a stat walk of the library's
   directory, its fingerprint, compared with the mounts the VFS's heads restored). Needed:
   the library a read of the certificate (its fingerprint, valued off the loop once a daemon's
   life before the first check) with the base made of the restored mounts; or the walk done as
   the daemon starts. Derived from the tool alone is not complete: `BLAZE_LIBRARY`-style
   overrides and a checkout's `library/` change under one binary.
2. **Outputs are hashed on the loop** where no stamp vouches (`Effect::ReadAll`, then the
   hash, in the span `request certificate: hash an output no stamp vouches for`): every
   output at a daemon's first check, and each output a build wrote less than two seconds
   before its stamp was taken. Needed: a pool task, bounded, with a span.
3. **Outside reads are valued at the build's end**, through `Disk`, for the paths the steps'
   records name (`Writer::read_outside`, which names only paths under the build's directories
   of outputs: a build script's read of a system header is in no list); the build's own looks
   carry their hashes. A build in which a build script RAN (not found kept) says none of its
   reads (`Writer::env_named`/`read_outside` are `Err`): `Untraced`, no certificate until the
   next build. Needed: the runs' own recorded reads with their hashes
   (`blaze-usercode/src/index.rs`, `inputs`), and a unit with none named `Untraced`.
4. **Not relocated**: the base holds the checkout's directory (`Viewed::bare`, `shape`), an
   output's path is absolute (base 0), `said` is as said. One chain a checkout directory.
5. **One node**, the view's root (`Read::Node("")`): any change in the checkout misses, and
   the miss names the root.
6. `Unwritten` and every other reason no certificate is written are a span
   (`request certificate: not written`, `why`) and a `warn` event; the client is not shown a
   warning (the job has finished by then) and no profile counter is added. A build whose
   outputs are still being placed at its `Finish` writes none.
7. **Delete** (nothing of it answers or feeds the write now; `Jobs::record_answer` is not
   called, so no recording exists): `Jobs::answer_again`, `Jobs::edited`, `record_answer`,
   `keep_answer`, `keep_last`, `Recorded`, `Jobs::recordings`, the made-nothing blocks at the
   units' end and the job's answer, `Roots::answers`, `noop::Answers`, `Shaped`,
   `MOST_SHAPES` and its assert, `noop::ask`, `NotReplayed`, `Moved`, `Recording`, `Walk`,
   `Unkept`, `LastAnswer`, their tests; `noop::Inputs` moves to `certify.rs`.
8. Under `-v` the notes of what the store served a build (`noted`) are diagnostics of one
   build, kept in `said`: a second build of the same reads may be refused as two answers.
9. The daemon-level simulation test and the revision cases (not written).

An output's content is the store key of its bytes (`Placement.key`): `output_key(bytes)` for a
file hashed whole, `linked_key(root)` for one blaze's linker made (the root of
`blaze_link::digest`), both functions of the bytes; the driver answers `Ask::Content` in the
form the record names (`Request::wanted`). The view a walk read (`Inputs::view`) is a root id
of content, as its last `open` had it after its lock-file writes: never the head at its end.
