---
status: plan
summary: blaze why, which says exactly why the last build computed something again instead of reusing it, by key part, read or reason, never by guess.
date: 2026-11-02
---
# Lane why-miss: `blaze why`

## What it is for

A warm build that recompiles too much is slow for a reason somebody has to find. `blaze why`
says what the last build computed again and why, by name and never by hash:

```
$ blaze why
build 12 computed 3 things again:

  2 codegen dev template: `the code of `app::Frame::new`, built in` changed
    app::render
    app::draw
  1 build script run: `env CC` changed
    ring v0.17.8

$ blaze why app::render
build 12: app::render (codegen dev template) was computed again: `the code of `app::Frame::new`, built in` (3f.. -> 9a..) changed since build 11 (of 7 in its key)
  because app::Frame::new (codegen dev template) was computed again: `its code` changed since build 11 (of 5 in its key)
```

`--json` prints the API's answer as it is.

With `--all`, every group lists every item; without it the daemon sends each group's first 8
and its count, so the answer stays small after a cold build. The answer is of the last build of
the workspace holding the current directory, and names that build's command and workspace.
Before any build of it has ended since the daemon started, it says so; earlier builds' reasons
are in the history's `misses` table. A `blaze why` asked while builds that ended before it are
still being explained on the history's thread waits for those builds, never for ones that end
after it was asked, so it is never answered from the build before and a stream of builds can't
hold it. It waits at most 30 seconds on the loop's clock (`WHY_WAIT`); past that it fails,
naming the oldest build still being explained and what the history's thread is doing (restoring
the ledger, explaining or appending build N, flushing the history for build N). Once the
history's thread has ended, a `blaze why` asked after a build it never explained fails, naming
that build and that `blaze-history` ended, never answered with an older build's reasons.

## How a reason is made

1. **At the miss, per build.** Each producer makes a `blaze_kernel::Miss` where its lookup
   found nothing, never on a hit, and gives it back with what it built, so a build's misses are
   its own: codegen's `Built::misses` (only when asked, `Build::explain_misses`, which the
   daemon's units are and nothing else is), a unit's `Compiled::misses`, a run's
   `RunPuts::misses`. The loop gathers them into the job and hands them over sorted in
   `Ended::misses`. A miss carries the step, a **subject**, the item by path (for a person
   only), the key, and the key's parts by name, each with its own hash, each name once.
   - **The subject** is what an item is across edits, telling apart instances side by side:
     a unit by its workspace's root, package, target name and kind, mode, platform, profile and
     whether it is std's or checked as a test (one `BLAZE_HOME`'s ledger holds every
     workspace's, and a registry crate's package id is the same in each); a function by its
     identity (item, which instance, type arguments) and whose code it is, within its unit's
     subject (its settings are a part, so a settings change is named, not read as a first
     compute); a build script's run by its run unit's subject.
   - **codegen dev template** (`blaze-codegen` driver, on a store miss): the toolchain, the
     code, the type arguments' facts, the settings, the kinds of its references, and each body
     it builds in, named by path (and by whose code it is where two share a path).
   - **unit** (`serve/unit/compile.rs`, when the record lookup misses): each `RecordInputs`
     field, and each dependency by the name it is used under. The parts are made from inputs
     gathered again and refused unless they make the very key the record was looked up under.
     Past the key, a record is filed under the values of the variables and proc-macro reads it
     traced (`beyond`). A unit whose front end can't say its inputs is `Always`; one whose
     identity was not set when looked up is `Failed`, naming that.
   - **build script run** (`serve/unit/script.rs`, a sandboxed run that starts): the script's
     code and each part of `Given` (`env CC`, its arguments, its standard input, each named
     part). Past the key are what it traced when it last ran. A run outside the sandbox is
     `Always`.
   - **proc-macro expansion**: not given back. Its key is the call's input tokens, nothing
     about the call site ties it to its last expansion, and the expander has no per-build out
     to give a miss through.
2. **A step the kernel computes** (`Kernel::why`): lazy, and it writes nothing. The caller
   hands in the key of the entry it saw kept last under the identity (`Got::Computed::key`,
   kept as `Miss::kept` and remembered by the daemon's ledger); the kernel reads that entry,
   rehashes its reads and names the first one that changed, with its old and new hash and how
   many of its reads changed in all. Nothing handed in is `New`, or `Unrecorded` when shapes
   are kept; an entry not in the store is `Collected` (that compute did not keep it); every
   read the same yet no shape names it is `Unshaped` (a keep that did not finish). No
   production step is a kernel `Compute` yet; codegen's dev template is whole-keyed, so it
   uses parts.
3. **Against the last compute** (`blaze-cargo` `serve/why.rs`, `Ledger`, sans-IO): a miss
   with parts is compared with what the ledger holds of the same step and subject, from the
   last build that computed it, however that build ended: a failed or cancelled build moves
   the ledger on too (a keep-going build that failed computed what it computed, under the key
   it names), recorded as not ended well; the next miss of the same key, nothing kept under it,
   reads as `Unfinished`: that compute likely failed or was stopped. Recording only builds that ended well left a run of failed builds with every miss
   read as `First`. Builds between may have reused it under another key,
   so each cause names the build it is compared with ("since it was last computed, in build N").
   - `First`: the ledger has no record of the subject.
   - `Parts`: the key changed, and the parts that changed are named.
   - `Unnamed`: the key changed but every named part is the same, so the key has a part it
     does not name. That is a bug in the producer.
   - `Lost`: the key is the same, yet nothing is kept under it now: that compute did not keep
     it, or it was collected since.
   - `Unfinished`: the key is the same, nothing is kept under it, and the build that last
     computed it did not end well: that compute likely failed or was stopped.
   - `Beyond`: the key is the same, so what the value is filed under past the key differs, or
     that compute did not keep it. Which of the two it was is not explained yet.
   - `Twice`: two different misses of one step under one subject in one build. Neither is
     compared or recorded (rule 18): the subject told them apart by less than their keys.
   - A record from before a **gap** (a build whose record did not reach the file: an append
     failed, or the daemon stopped during one) is compared with nothing: `NotExplained`, naming
     both builds, as the subject may have been computed in the gap.
4. **Where it runs**: the ledger lives on the `blaze-history` thread (`serve/why/file.rs`),
   which explains each build's misses, sends the answer to the loop, then appends. The loop
   only moves the misses over and keeps each workspace's last answer (`Answers`, at most 16
   workspaces).
5. **The answer** (`Answers::rebuilt`, on the loop, in a span): sorted by item, so an item's
   reasons are a binary search and a chain walks each item once. Without an item, groups by
   step and reason are made on the daemon.

## Where it is kept

- **History**: the `misses` table (docs/profiling.md), one row for each thing computed again,
  written by the build's flush.
- **Ledger**: `why.ledger` in the history's directory. It starts with a 32-byte magic,
  `blaze why ledger` and the first 16 bytes of the version: a hash of the encoding of a record
  with every field set and of a naming version (bumped when part names or subjects are made
  another way), so any change is a new version. Only types the ledger owns are written: one
  framed deterministic-CBOR record per build, however it ended, the subjects it computed with
  their key, parts and kept entry; a compacted record also carries the gap and the newest build
  a dropped subject was last computed in.
  - **Restore**, on the history's thread after the daemon has started (answers say "still
    restoring" meanwhile): an empty file or part of the magic is begun again. A last record cut
    short, or zeros after the last whole record (a file grown before its data landed), is a torn
    tail: the bytes past the last whole record are copied to `why.ledger.tail.<n>` first, and a
    gap after the newest record read is marked. The ledger, gap in it, is then written whole
    over the file (hidden file, renamed), never cut in place first, so a crash before that
    write finds the tail and the gap again. Only if the whole write fails is the file cut back,
    and only once the gap is in `why.ledger.gap`; if that can't be written either, the file is
    left as it was. A tail with no record before it holds no gap and is cut in place. A
    restart otherwise never rewrites the ledger before answering: a due compaction waits for
    the first append. A file that can't
    be read, is another version's or has a record that does not decode is moved aside to
    `why.ledger.<why>.<n>` and never written over; for a record that does not decode, a gap is
    marked after the records read before it. Asides keep the newest 4 of each kind. Each is
    reported in `blaze daemon status`, and a file moved aside with every answer
    (`Rebuilt::torn`). Stale hidden files of a cut-short whole write are removed. Of two gaps
    the later is kept.
  - **Append**: synced, the directory too when it made the file. One that fails is cut back,
    and the build is marked a gap in memory and in `why.ledger.gap`, which a restart reads (a
    gap file already there that names an earlier build is replaced whole by the later one);
    each build then writes the whole ledger until one succeeds.
  - **Compaction**: once the file is past 4 MiB and twice its last whole write, the whole
    ledger is written (hidden file, synced, renamed, directory synced), its subjects last
    computed longest ago dropped past about 64 MiB. Answers note the drop when they hold an
    item read as computed first (`Rebuilt::dropped`).
- **API**: `Request::Rebuilt { all, cwd, item }` and `Response::Rebuilt(Rebuilt)` (schema 26),
  answered by the daemon's loop. `blaze why` is a client of it.

### Answered before durable

The history's thread sends a build's answer to the loop before it appends the build's record
to the ledger. A `blaze why` can therefore name a build whose record a crash then loses: if the
daemon dies after the answer is sent and before the append starts, the file has no trace of the
build, no gap is marked, and the next daemon compares that build's subjects' next misses with
the record before it. The window is one explain-to-append step on one thread, and the history's
own `builds` row for that build is lost in the same window (it is written by the flush after
the append), so the history and the ledger stay consistent with each other. Closing it would
mean appending before answering, which puts an fsync on every `blaze why` held for a build.
A failed append is covered: it marks `why.ledger.gap`. A crash during one is covered only once
some of its bytes reached the disk: a torn tail the restore turns into a gap. A power loss
before its `sync_data` can leave no trace of it at all, which is the same window as above: no
gap, and the build's subjects compared with the record before it.

### Known rule-22 violation: which unit a shared generic's miss goes to

Dev template keys don't depend on the unit, but codegen misses are re-subjected under the unit
that reports them (`compile.rs`). Which unit's subject a shared generic's record lands under
depends on which unit's put landed first, so the ledger and the `misses` rows can change from
run to run (rule 22), and a later miss under the other unit reads `First` ("no record of
computing it before"), which is misleading. Making the subject unit-independent brings back
`Twice` for one function built at two settings, since settings left the subject, so the fix
needs the hits per template key (the same gap the existing hit and miss counts have). Tracked
as a known violation: issue to file. Interim wording to consider: for the `codegen dev template`
step, `First` reads "no record of this unit's instance".

## Not yet

- The values behind `Beyond`: a unit's traced variables and proc-macro reads, and a build
  script's traced reads. Their slots keep the names, not the values, so naming the one that
  changed needs the read's value kept beside its hash.
- A call-site identity for proc-macro expansions.
- Test selection's dropped `Why` (`serve/unit/test.rs`) and link's `LinkHow::Full { why }` are
  not in the answer yet.
- A unit that fails before its store phase gives back no misses.
- Hits are not tracked: after A, B, A (reused), C, C is compared with B (and says build B).
- Which unit reports a shared generic's miss depends on which unit's put landed first (rule
  22, see above; issue to file).
- Frames have no checksum: a bit-rotted length in a middle record reads as a torn tail. The
  bytes cut are copied aside first, so nothing is lost for good, but the later records are not
  replayed. A per-frame checksum (and a version bump), with a frame past the end or failing its
  checksum read as a torn tail only when it is the last one, would tell the two apart.
- A dependency part names no dependency unit, so a chain does not follow a dependency.
- `--all` after a cold build is not paged.
- No restart-to-first-answer budget test: the restore no longer holds up the start, and its
  own cost is bounded by the cap.
- The end-to-end path (build, edit, `blaze why`) has not been run.
