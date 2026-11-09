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

Nothing placed by hard link (`Effect::Link` has no producer), so no build-directory write can
reach a stored file through a shared inode.

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
- **No fallback.** A clone that can't share blocks (no reflink, or across volumes) fails the
  place, naming both files and both volumes (mount point and file system on macOS; the
  `f_type` and the device on Linux). Nothing copies instead. The build dir and the store are
  both under `<root>`, so they share a volume unless `--target-dir` moves the build dir.
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
- **Reported.** Every `Placed` carries `PlacedBytes { cloned, written, copied }`. Each job sums
  them and reports them:
  - in the `build placed` span (target `blaze`), and
  - in `Worked::placed` (API schema 27), which the client prints in the `blaze-work` JSON line.

  The `Work` report moved from the plan's finish to the job's last finish, so it counts every
  placement.

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
