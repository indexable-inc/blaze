---
status: plan
summary: Lane clone-out: a second checkout's build directory costs only its own bytes, because every output of 16 KiB or more is a clone of the store's read-only copy, and each build reports the bytes it cloned, wrote and copied.
date: 2026-11-02
---
# Lane clone-out: build directories as clones of the store

The goal: a second checkout of a project costs almost no disk. Each workspace builds into
`<root>/build/<name>-<id>` ([cargo.md](../../../cargo.md#where-a-build-writes)), which holds
only what must be a real path; the bytes live once in `$BLAZE_HOME/v2/store`. For scale, kache
measured a second Firefox worktree at about 3 GB instead of 16.7 GB by cloning blobs.

## What writes into a build directory (audit, 2026-11-02)

| writer | how before | in the store | after this lane |
|---|---|---|---|
| linked executables, test binaries, build-script programs (`compile.rs` puts `Kind::Output`, `job.rs` places) | written whole (chunked from 256 KiB), the deps path and the uplifted copy each written | yes, as chunks | cloned from `placed/<content>`, written once per content |
| rlibs, rmetas, `.d` files | written whole | yes | cloned from 16 KiB, written below |
| proc-macro wasm (`Kind::Machine`) | never placed: `file-<content>` mapped | yes | unchanged |
| `OUT_DIR` restore (`blaze-usercode` `run.rs` `restore`) | written fresh with `std::fs`, outside the driver | yes, inline in one `Kind::Run` value | unchanged: next step below |
| cargo's run files (`script.rs`: `output`, `stderr`, `root-output`) | written fresh (`Truncate`, `WriteAt`) | no | unchanged: small |
| `invoked.timestamp` | written fresh, `std::fs` | no | unchanged: empty |
| `doc` pages (`cli/doc.rs`) | written by the client, `std::fs::write` | no | unchanged: the client's |
| `.blaze-record`, `CACHEDIR.TAG`, `.blaze-lease`, layout maps | written fresh | no | unchanged: small, the build dir's own |

No path is ever a hard link of a stored file, so no build-directory write can reach one
through a shared inode. The one link the store makes (2026-11-12) is between two paths of
one build that hold the same program: `Store::place_also` links the first path's staged file
under the second's staged name, and both are renamed in. Both are new inodes outside the
store.

## Design

- **The chunks stay canonical.** Outputs of 256 KiB or more stay FastCDC chunks in the packs,
  so versions of a binary keep sharing chunks. A whole file can't be cloned from chunks, so
  each output of 16 KiB or more (`CLONE_FROM`) also gets one whole copy, `placed/<content>`,
  named by content, written atomically, made read-only (0o444), not in the head. Every
  placement of those bytes, in any checkout, clones it. Below 16 KiB a clone saves less than a
  block or two and costs an extra file; those are written.
- **Bounded.** `Store::unplace` runs when a job places other bytes at a path: the old
  content's placed file goes (once no clone of it is in flight), so relinking doesn't leave one
  whole file per version. Clones already placed keep their blocks.
- **One way a store, decided once.** Whether the store's file system shares blocks is found
  as the store opens, by one clone of a small file in its own directory
  (`blaze_cas::PlaceBy`, `Store::ask_shares`), said once in the daemon's log and in `blaze
  daemon status` (`placing: ...`), and every place after is made that way. Where it shares
  blocks: clones, as below. Where it does not (ext4, tmpfs): every place writes the value's
  bytes from the store's records, and the store keeps no whole file to clone from. A patched
  place is the clone route with one effect swapped (2026-11-12). The kernel copies the path's
  file's data extents into a new inode beside it (`Effect::CopyData`: `SEEK_DATA`/`SEEK_HOLE`
  and `copy_file_range`; on macOS `lseek` with `pread`/`pwrite` through one bounded buffer).
  Its holes stay holes. Then the cut, one write a run, and the rename. The path's file is
  never written in place on any file system: a running program keeps its inode whole. Until
  then the store read the whole file and wrote it back: 158 MB for a 10 KB patch, twice a
  build. The build dir then costs its bytes. Never one tried and the other done for a file: until 2026-11-12 a first
  place was a write and every later one a clone, so a home on tmpfs or ext4 failed every build
  after its first. A path is never a hard link of a store file: that is the store's own inode, which a
  program writing its output or a `chmod` would change. A clone that still fails (a
  path on another volume than the store's) fails its place, naming both volumes (APFS's
  container, `f_type` and the device on Linux). The build dir and the store are both under
  `<root>`, so they share a volume unless `--target-dir` moves the build dir.
- **Read-only, not immutable.** On APFS, `clonefile` copies the source's mode, owner and
  flags onto the clone (checked 2026-11-02: a 0o444 + `UF_IMMUTABLE` source gives a 0o444 +
  `UF_IMMUTABLE` clone), and writing the clone leaves the source unchanged (copy on write).
  So:
  - Copy on write already keeps a build directory's writes out of the store. 0o444 only guards
    against a writer that opens the store's file itself.
  - The macOS driver sets a clone to 0o644 after `clonefile`, as Linux's `FICLONE` into a new
    file and a `Write` leave it.
  - `UF_IMMUTABLE` isn't used: every clone would inherit it, and it stops unlink, rename and
    hard link, which breaks gc, `blaze clean` and `rm -rf`. `FS_IMMUTABLE_FL` needs
    `CAP_LINUX_IMMUTABLE`.
- **Stamped.** A place through `publish::Writing` (made, patched, a second path) looks at its
  staged file once before the rename. Its answer carries that look: `Placed.stamp` holds the
  inode, size and mtime that vouch for the path, so nothing hashes the file again to vouch
  for it. The ctime moves at the rename, so it does not vouch.
- **Read ahead only data.** The prewarm of a started program's clone reads only its source's
  data extents. Measured on xfs, it read 9.4 MB to 158 MB a file, 134 MB of that the two
  reserves of zeros.
- **Reported.** Every `Placed` carries `PlacedBytes { cloned, written, copied, kernel }`.
  `kernel` is the data a patched place's extent copy moved in the kernel. Each job sums
  them and reports them:
  - in the `build placed` span (target `blaze`), and
  - in `Worked::placed` (API schema 27), which the client prints in the `blaze-work` JSON line.

  The `Work` report moved from the plan's finish to the job's last finish, so it counts every
  placement.

## An output the build made (2026-11-10; written, not built or run)

Supersedes, for the edit path, "cloned from `placed/<content>`, written once per content" and
the handoff of 4f260cd351 (write `placed/<id>`, write it back, clone it). Measured on xfs
after a one-line body edit of hello world: the answer held 3.2 s while the loop thread sat in
`ioctl(FICLONE)` for 0.9 s and 1.4 s, the second from a 162 MB `placed/` file made in that
build. A clone of a file with data not written back waits for all of it; a place that can be
made only of a clone waits for the file to be made, sealed and written back first.

- **The rule.** An output the build that asks just linked is placed from the bytes its maker
  holds, by `Store::place_made(to, Sparse { len, runs })`: remove the staged name beside the
  path, truncate it to `len` (a file of holes), one `pwrite` a run, rename onto the path
  unsynced. No file of the store is read, cloned, derived or waited for, and nothing is
  synced. There is one route, chosen by what the caller has (bytes), never tried after
  another. `Store::place` (clone from a file written back) stays for a value with no bytes at
  hand: a second checkout, a record replayed.
- **Patch room is not written.** The runs are `Image::file_runs`: the regions' bytes, gaps past
  a page left out, so the two 64 MiB patch regions are holes. Hello world's edit writes about
  28 MB a path where it wrote or cloned 162 MB. The room out of the file's length altogether
  is [patch-in-place.md](patch-in-place.md)'s layout change, still not built.
- **Not in place, and not only the changed blocks.** Writing the path's file in place is
  refused while a program runs it (`ETXTBSY`) and changes its mapped pages under it; a private
  copy to write changed blocks into is a clone of the path's last file, which was written an
  edit ago with no sync, so it is the same wait. So: a new file, whole content, renamed over.
  Writing only changed blocks needs a source known written back; that is a later step
  (write-back of the output after the answer, then clone-and-patch when it has completed).
- **Only the changed blocks, since 2026-11-11** (lane edit2-1111; measured on xfs, hello world:
  the answer waited 24.9 ms for its four places and waits 8.9). The step above named as later
  is built: every made place asks for its file's write-back as it ends (`Effect::WriteBack`,
  waited for by nothing), so the path's file is a source written back by the time the next
  edit clones it, or the clone waits for what is left of the last patch and no more. A link
  taken as a patch hands the loop its ranges (`work::Made::Patch`); the loop looks that the
  path still holds the file it placed there (`Own::LookBase`: its size and time), and then
  `Store::place_patched` clones the path's own file under the staged name, writes the
  ranges and renames it over: 0.3 MB written a path where it was 21 MB. Where the path does
  not hold that file (nothing placed there by this daemon, a place that failed, a file changed
  since), the whole file's runs are made from the link's state and written as before. The
  simulation holds a patch's clone to what its path's last place wrote
  (`tests::place_made`), and the control takes the write-back out.
- **The store's copy.** The value is put as a recipe: of the link's ranges over the file
  before it, a few KB, or on a first link of the file's runs over the base of holes
  ([patch-in-place.md](patch-in-place.md), "The patch room"), never the file at its length.
  It is durable with the store's next commit, after the
  answer (`Settling`). No `placed/` file is made on the edit path; `Store::unplace_made` lets
  go of the one the path's last value was cloned from.
- **Crash safety.** After a crash the path holds its old file, the new one, or what the power
  left of the new one. Nothing believes it: the daemon records a path as holding a key only
  after the place completed and a `stat` of it (`paths.placed`, in memory), and a daemon that
  starts knows of no placed file, so the next build places again. No hash of the file is
  taken: the check is the recorded length and mtime against a `stat`.
- **One writer per path** (2026-11-14). An output path is written only by its job's
  placement (`Jobs::place`, `place_made`, `place_patched`, `place_also`, each under the
  store's `place` span naming the path). A dev executable's launch (a test binary, `blaze
  run`) writes nothing: it is started from the placed file with the key the path's record
  holds (`Jobs::placed_key`, read once the job's placements have ended), and refused naming
  the path and both keys where that is not its code memory's file
  (`blaze_inproc::host::StartError::NotPlaced`). So a build after a test run finds the file
  as placed and places nothing. A request certificate names an output by the key its own
  build left there as it answered (`Jobs::left_by`), and is not written where the record
  holds another by the time the build has settled (another request placed it). Its check
  refuses, from the record alone and with no look at the file, an output whose path's record
  holds another key or has a placement in flight (`certify::placed_elsewhere`): that request
  goes to its job, which clones the answer's file back. Alternating two commands that share
  a path places one file per switch, and a repeat places nothing. A value placed over stays
  in the store, its record and its home (`blaze_cas::Store::names_step` asks no forget of it
  since 2026-11-14; until then the first `run` after run, build, run, build failed on
  "the store has nothing under .. to place"): only the collector, by budget, lets it go, and a
  unit record whose outputs the store no longer holds is not replayed (`header::check`).
- **The loop thread.** `FICLONE` runs on a thread off the ring with a span `io clone <file>`;
  the ring's eventfd is written only while its thread is parked. Still in line on the ring's
  thread, each one metadata call: mode, seal, times, readlink, flock, truncate, mmap.
- **Proven by** `crates/blaze-cas/src/tests/place_made.rs`: the invariant
  `a_made_output_is_placed_with_no_clone_and_no_wait_for_a_write_back` and its two controls.
  Guards: `loop stalled` spans (a step of a loop turn over 20 ms) fail an edit's budget, and
  `harness/v2/loop_syscalls.py` holds the loop thread's system calls to 20 ms (ledger T16).

## What the answer waits for (2026-11-11; written, not built or run)

A build's answer waits for the files it names and for no other output. The files it names are
each unit's artifact files (`Built`'s `path`, `filenames`, `executable`: the uplifted program,
a library's files) and the lock files it writes: what a person or a script opens the moment
the command returns, straight from the file system, so the path itself has to hold the file.
Every other output (a program's copy under `deps` with its hash, dep-info, a file cargo's
artifact lists none of) is placed after the answer (`Settling::placing` in `serve/job.rs`):

- It goes on exactly as under the job (its mode, the `stat` for its record). A failure is a
  loss said in the log, `blaze status` and the checkout's next `blaze why`, and the next
  build that makes the output places it again.
- A later job's output at a path with an effect in flight waits for it (`Queued`), so two
  placements of one path never race; a run of the daemon's own (`blaze run`, `blaze test`, a
  build script) still starts only once its job has nothing in flight, named or not.
- A stop waits for it as for anything settling; the work between requests (the image, the
  collections) does not run beside it; the job's build dir leases are held until it ends.
- A crash before it lands leaves the old file or the new one whole (a rename), and the
  restarted daemon, knowing of no file it placed, places again: no ordering of anything
  durable changed.
- Spans: `the job's answer waits for what it still does` (`function` `its outputs, being
  placed`, with `named_path=`, `needed=`, `placing=` in `fields`) is now the named files'
  wait alone; `a build's other outputs are placed after its answer` covers the rest; `an
  output waits for the path's placement in flight` is a later job's wait for one.
- Proven by `serve/tests.rs`: `a_builds_answer_waits_for_the_files_it_names_and_places_the_rest_after`,
  `a_run_asked_right_after_an_answer_waits_for_the_output_still_being_placed`, their controls,
  `an_output_that_fails_to_be_placed_after_the_answer_is_a_loss_said`, and the crash test
  `a_crash_after_any_turn_keeps_what_had_settled_and_restores_to_a_fresh_build`, whose crash
  points now include an output in flight after its answer.

## Checked by

`harness/v2/clone_out.py`, check `a_second_checkout_adds_only_its_own_bytes`. It builds one
project in two checkouts against one `BLAZE_HOME` and measures the second build directory's
new blocks:

- macOS: the volume's free space before and after, since APFS has no unprivileged per-file
  shared-block query.
- Linux (btrfs or XFS): `FIEMAP` shared extents.

It also reads `placed` from both builds' `blaze-work` lines.

## Next

- **`OUT_DIR` restore through the store.** `collect` puts each `OUT_DIR` file of 16 KiB or more
  as its own `Kind::Output` value, and the run record keeps its key. `restore` places those
  through `Store::place` from the job, not with `std::fs`. Small files stay inline.
- **A restarted daemon forgets which path held which bytes.** `placed/` survives a restart:
  the open keeps each file named for a content (and removes partial writes), and a kept file is
  hashed against its name before it is first cloned from, so a rebuild after a restart clones
  rather than writes. But `paths.placed` isn't persisted, so the placed file of an output
  replaced after a restart is never let go of. A sweep of the `placed/` files no value of the
  store has (or none placed for a while) belongs in gc's store pass.
- **One whole file per content, not per holder.** `unplace` drops a content's file when one
  path moves off it, though another checkout's path may still hold those bytes; that checkout's
  clone keeps its blocks, and a third checkout writes the file again. A count of holders per
  content would keep it.
- **New bytes are written twice into the store**, as chunks and as the placed file. Writing
  the placed file first and chunking from it (or chunking lazily, at the first cross-version
  dedup) would make that once.
