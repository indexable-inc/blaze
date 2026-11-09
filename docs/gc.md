---
status: current
summary: What blaze removes from disk and when: the collector of build dirs, shared std stores, temporaries and trash; the pacer that works harder as a volume fills, leases, records, roots and the grace, and why nothing in use is removed.
---
# Collecting what blaze keeps on disk

blaze writes outside the checkout only: a build dir per workspace under blaze's root
(`<root>/build/<name>-<id>`, [cargo.md](cargo.md#where-a-build-writes)), the daemon's state
(`<root>/v2`), and one standard library store per code of what makes std's records (the front
end's, the code generator's and blaze-cargo's code identities, `serve::RecordMakers` in
crates/blaze-cargo, never the executable's bytes), shared by every daemon of the machine
(`<machine>/std/<makers>`). The collector, `crates/blaze-gc`, removes what is no
longer used of the first and the last, so a machine that builds many checkouts does not fill its
disk. The daemon's state (its store, image and journal) is never listed, so never a candidate;
eviction inside a store is not done yet ([v2-store.md](v2/plan/design/store.md)).

It is automatic: nobody needs `blaze clean` or `blaze cache gc` to keep a disk from filling.
The daemon measures each area's volume and collects more, and sooner, the closer free space
gets to its floor ([Pacing](#pacing-how-hard-it-works)). `blaze clean` stays as an escape
hatch.

## What it removes

Per area (`<root>`, and the machine's directory when that is another; `build/` is listed only
in the root's, `std/` only in the machine's):

| Class | Where | Goes when |
|---|---|---|
| workspace | `build/<dir>` | its checkout is gone (and it was unused for a day), or it was unused past its TTL (14 days) |
| profile | each directory in a workspace (`debug`, `release`, a target's) | its workspace is past its cap (16 GiB, least recently used first), or its volume is under pressure; its last use is the newer of its files' times and the record's stamp for it, written by each build that uses it |
| std | `std/<makers>` | no daemon has started on it for the TTL, or its volume is under pressure (least recently started first, after every profile); a store a running daemon holds the lease of stays |
| temporary | a `*.tmp` file in a workspace | it is older than the grace and its workspace's lease is free; renamed under that lease |
| trash | `.blaze-trash/<entry>` | it is unlinked once moved there |

The trash directory carries blaze's marker (`.blaze-trash-of-blaze`); a `.blaze-trash` without it
is not blaze's, so nothing is moved into it and nothing in it is touched. In a marked trash only
entries named as blaze names them (`<20 digits>-<6 letters or digits>-<name>`) are listed or
unlinked; anything else there is left and reported.

Under pressure, what goes goes in this order: trash first (unlinked before any rename), then
workspaces whose checkout is gone, then what is past its TTL or a cap, then the least recently
used profiles across every workspace, then each workspace's last transactions' profiles (none
in the daemon yet: nothing feeds the collector transactions), then whole shared std stores and
build dirs, least recently used first. What is leased, a live head's workspace directory and the
profiles it keeps (below), and anything within the grace never goes, however full the disk.

A checkout counts as gone only when its path is not found and its parent is there; any other
error (a permission, an unmounted volume) refuses the workspace. A workspace whose checkout is
gone keeps a day's grace, so a checkout moved away for a while keeps its build.

Nothing goes within the grace (an hour since its last use), and neither does a directory that
holds something within it: a workspace whose checkout is gone stays while a profile in it was
used within the hour. A workspace whose checkout is a live head of the daemon (a checkout it
watches), or that something leases, stays. A live head keeps its workspace directory, its
temporaries, and of its profiles only those used within `head_recent_ns` (a day in the daemon)
or by its last build (a profile whose last use is at least its workspace's, as the record
stamps both on each build); its other profiles yield to the cap and to pressure like any
workspace's, so a daemon that lives for weeks on one checkout still frees what that checkout's
old profiles hold.

## Pacing: how hard it works

Collection is gradual, like Go's GC pacer, never switched on or off at one threshold
(`crates/blaze-gc/src/pace.rs`). Each area's volume is measured: free space to an unprivileged
user and size (`statvfs`'s `f_bavail` on unix, `GetDiskFreeSpaceExW` on Windows;
`crates/blaze-gc/src/space.rs`), and areas on one volume count once. A volume is known by its
pool of free space, so two volumes that share it do not each ask to free the whole gap: on macOS
its APFS container (the device after the last `/dev/` of `statfs`'s `f_mntfromname`, so a
snapshot mount such as `com.apple.os.update-...@/dev/disk1s5` works), on Linux a btrfs
volume's mount source (from `/proc/self/mountinfo`, as each subvolume has its own `f_fsid`),
else `statfs`'s `f_fsid` when it is not 0, else the device (`st_dev`; `f_fsid` is 0 on overlay
and tmpfs). An area whose volume does not measure is named in the run's problems and the other
areas are collected all the same. Pressure, in permille:

- 0 at the comfortable level, max(15%, 30 GiB) free, and full (1000) at the floor, max(10%,
  20 GiB) free, linear between;
- also from blaze's share: blaze's own bytes on a volume past 80% of half the volume raise it,
  full at half; what is to be freed for the share is what is past that 80%, so it is never 0
  while the share holds pressure on;
- with a band of 5% of the gap between the levels either side: it comes on at 5% pressure and
  goes off only once free space is 5% past comfortable, so free space hovering at a level does
  not switch it on and off. Free space's pressure runs below 0 past comfortable; blaze's share
  counts only when it is above 0, so a share within bounds never holds collection on.

Work is done in slices, each a job of the worker pool bounded by fuel, bytes and a deadline
(5 s). A slice's scan gets half the slice: it stops after the unit that crosses that time,
having walked at least one, and the cursor moves past what it walked, so a scan slower than its
slice still makes progress; an answer that comes back after the slice's deadline still counts
up to the run's hard deadline (the longer of the slice and the run bound). The pool class
follows the collector's priority (`Collector::priority`): a slice below half pressure is
`Background` (taken only when no other work is queued), a slice at or above it and a run asked
for are `Normal` (with builds), an urgent run is `Critical` (ahead of them). From no pressure to full, linearly:
the gap between slices goes from 60 s to 1 s, a slice's steps from 4 to 256 renames and unlinks,
its scan from 4 to 64 build dirs and stores, and its bytes from 16 MiB/s to 1 GiB/s times the
gap. Below half pressure a slice runs only while the daemon is idle; above it a slice also runs
while builds run. At most one slice runs at a time, so collection takes at most one worker. A
decision of an ended run still in the pool does not hold the next run back: its answer is dropped
when it comes, and a new run waits for it at most the run bound (the urgent run not at all).
`next_deadline` gives a deadline only when a start is possible then, so the loop never spins on
one it will refuse.

A slice's scan walks only its window of units (build dirs and stores, in path order), from a
cursor the last slice left; a lap is the slices from the first unit to the last. What a lap
saw of each object (its size, last use, whether it may be evicted) is kept, so the least
recently used across every workspace can be chosen while each slice walks a few: a slice
evicts what it walked that is older than the lap's cutoff, up to its bytes; what it evicted, and
everything inside it, leaves what the lap saw. Sizes are bytes on disk (`st_blocks` × 512, each
inode once, so a reflinked or hard-linked output counts once) and are kept between slices: a
directory whose inode and newest stamp are unchanged is not walked again for 15 minutes.

A lap under pressure that freed nothing makes the next wait the period (6 hours), so a disk full
of what is in use is not walked over and over; a lease let go, a change of heads, or free space
falling by the band since ends that wait. Without pressure a lap runs once each idle stretch
(5 minutes idle) and every 6 hours.

The daemon measures the volumes as a build starts, every second while builds run (every 30 s
while idle) and right after each run ends: a build that writes many MiB a second is seen within
a second. Free space at a volume's floor while a build runs (free space only: a share overrun
is paced, never urgent) asks for an urgent run: one run that walks everything with its caps
lifted (fuel 4096, 10 minutes), still never touching what is leased, rooted or within the grace.
A paced run under way when one is asked for ends at once (`Stop::Preempted`), its answers still
out dropped, and the urgent run starts in the same step. The driver's gate counts the preempted
run as ended right then (`Collector::ended_through`), before its record is drained, so none of
its renames still queued on the pool starts after the preempt.
At most one a minute unless free space, as measured since the last one started, rose by 1% of
what it was to free, or a lease was let go since; what a run unlinks is never credited as freed,
since reflinked outputs free less than their size. An urgent run that leaves a volume below its
floor says so in its report: the volume, its free space and floor, and the largest objects kept
with what keeps each (`NoSpace`); `Collector::no_space` gives it until no volume is below its
floor, and never one restored from the ledger.

A build that fails for want of space (the OS's `ENOSPC` or `EDQUOT`, `ERROR_DISK_FULL` on
Windows, `blaze_gc::space::is_no_space`) is held only when what is full is a volume the
collector collects: the error's place, when it is a path, must be on the volume of one of the
areas (compared by volume id); with no path, a collected volume must be below its floor. A disk
full elsewhere (`/tmp`, another mount) fails the build at once. A held build's finish is kept and
an urgent run asked for (`Collector::hold_for_space`), ending a paced run under way. The finish
is told, its error saying what came of it, as soon as one of these happens:

- the urgent run, past its scan, freed bytes and every area was measured since it began, and the
  build's volume is above its floor again (`Released::Freed`; the volumes are measured every
  second while a build is held, even when the daemon is idle);
- the run ended and every area was measured after it: still below the floor, with what holds
  the space and the areas another collector holds that were not looked at (`NoSpace`), or freed;
- the run's scan failed (`Released::ScanFailed`, naming why);
- the hold's bound passed (`Released::TimedOut`, naming the volume): twice the last full walk's
  time, at least `Config::hold_ns` (2 minutes) and at most the run bound plus that;
- the daemon stopped.

A build that runs out of space again right after an urgent run that freed nothing, with no lease
let go since and the volume still below its floor, is told at once with that run's `NoSpace`
instead of starting another walk, so an agent that retries does not start one walk a retry.

The collector reads the wall clock and, where there is one, the boot clock (`CLOCK_BOOTTIME`
on Linux, `CLOCK_MONOTONIC` on macOS) through one `plan::Clock`: a wall clock more than 5
minutes off the boot clock's time since the clock was anchored (ahead or behind) is held to
within 5 minutes of it, and the report says so, so a clock set forward while builds run does not
make what they used look old, and one stepped back does not hold every deadline off. A wall clock
that stays ahead by the same amount (within 5 minutes) for an hour of boot time is taken as
right, and the clock re-anchors to it, saying so. Each decision the pool runs carries the clock it
was asked under and reads its deadline (and a scan's stop time) against it, never against the raw
wall clock, so a VM paused and stepped forward by NTP does not cancel every rename.

Each slice has a span, `gc slice`, with its pressure, the bytes it freed and its time; the
phases' spans (`gc scan`, `gc mark`, `gc sweep`, `gc unlink`) carry the pressure too.

The levels are set in `<root>/gc.json`, one JSON object whose fields are all optional and whose
unknown fields are refused, naming them (`crates/blaze-gc/src/settings.rs`):

```json
{ "comfortable_percent": 15, "comfortable_gib": 30, "floor_percent": 10, "floor_gib": 20, "share_percent": 50, "auto": true }
```

`"auto": false` turns the daemon's own collection off, saying so as it starts (a build out of
space is then not held); `blaze cache gc` still runs when asked. A file that does not read, a
share of 0%, a comfortable level or floor of 100% or more, or a floor above the comfortable
level fails the daemon's start and `blaze cache gc`, naming the file and the field.

## Leases: why nothing in use is removed

The invariant: **a directory is renamed into trash only by a holder of the exclusive lock on its
`.blaze-lease`, and every process that uses the directory holds a shared lock on that file for
as long as it does.** So a pin or lease taken at any point, even after the collector marked the
directory, keeps it: the rename needs the exclusive lock, which the collector only tries for
(`try_lock`, never waiting) and does not get while the lease is held.

- A daemon's job takes its build dir's lease before anything is built into it and holds it until
  no task, placing or run of the job is left (`crates/blaze-cargo/src/serve/job.rs`,
  `lease_build_dirs`, reached through `Steps::lease` so the daemon's simulated tests, whose
  build dirs are not on disk, take none). It is taken on the worker pool, as the job's first step, never on the
  daemon's loop: it only tries the lock, and while a collector holds it exclusively, or its
  lease file's nonce is not whole yet, the step gives way and is posted again, for at most
  `lease::GIVE_WAY` (11 s: the 10 s after which a short nonce is abandoned and repaired, and a
  second), then a typed error naming the dir and what it waited on (`lease::Wait`). A
  daemon holds its shared std store's lease for its life (`crates/blaze-cargo/src/serve.rs`),
  taken once at start, waiting at most a second (`SHARED_WAIT`) and checking inside the wait
  that the file it waits on is still the dir's. The locks are the kernel's `flock`s, so they hold
  across processes, and a crash leaves none behind.
- The collector holds the exclusive lock only for one probe (lock, then let go) during its scan,
  or, on a dir and every lease inside it, for one `rename(2)` of the tree, or, on every lease
  inside a trash entry, for that entry's unlink. The leases inside a tree are found by a walk
  that takes no lock; once the tree is renamed, a second walk with the locks held takes any
  lease made inside since. One a build holds (taken between the first walk and the lock: nothing
  orders a nested build's lease after the outer lock) renames the tree back at once, and the
  rename is refused naming that dir (`fs::recheck_renamed`); every unlink takes every lease
  inside first, so nothing a build holds is ever unlinked. `blaze clean` holds its locks through the rename and the unlink.
  So a build waits on a collector for at most a rename, and on `blaze clean` for its removal.
- Either side may lock a lease file that is no longer its directory's: a user that opened it
  just before the rename, or a collector that opened it before someone else renamed the
  directory and made it again. After locking, each checks that the open file is the one the path
  names now (device and inode, on unix) and that the nonce read through each is the same, whole
  line. The nonce is written once, as the file is made with an exclusive create (no hard link, so
  FAT, exFAT and FUSE file systems work). A reader that finds it short does not spin: the lock is
  let go and the lease is repaired, then taken again (a build's lowering gives way and is posted
  again; a daemon's std store lease waits, under a span, at most `GIVE_WAY`). Repair takes the file's exclusive lock without waiting, so no one holds it, and a file
  still without a whole nonce 10 s after it was made is abandoned (a maker that crashed between
  create and write): its nonce is written then, and a warning names it. A short file younger
  than that, or one someone holds, is left for its maker. When the check fails the lock is not
  the directory's: a user makes the directory afresh and takes the lease again (at most 4 times);
  the collector does not rename (`crates/blaze-gc/src/lease.rs`).
- A job never makes or marks a dir that is `/` or right under it (a foreign workspace's
  `/@blaze-foreign-target` among them), that is or holds its checkout, or that is or holds
  `$HOME`, `CARGO_HOME`, `RUSTUP_HOME`, blaze's root, its directory of build dirs (`<root>/build`)
  or the machine's shared directory, or lies in `CARGO_HOME`, `RUSTUP_HOME`, blaze's state
  (`<root>/v2`) or the shared std stores (`<machine>/std`), one
  list both the job and `blaze clean` check (`blaze_gc::fs::Protected`), each compared after resolving
  symlinks (the nearest part that is there resolved, the rest as given; `..` past it is an error
  naming the dir), before and after making it. Such a dir gets no lease and no file of blaze's; a
  target dir that is or holds `<root>/build` fails the build, naming it. A relative `HOME`,
  `CARGO_HOME`, `RUSTUP_HOME` or `BLAZE_HOME` is taken against the current directory, as cargo
  takes it. The list is made once, at the daemon's start, which fails naming why when it cannot
  be made; a default `~/.cargo` or `~/.rustup` that cannot be searched protects nothing, since
  nothing this process could name is in it. A
  dir strictly inside the checkout (a project's `./target`) is leased only when blaze makes it
  now or it already carries a build directory's mark (cargo's `CACHEDIR.TAG` among them), so
  `blaze clean` sees a build that writes into it; a directory of the user's never is.
- A directory with no lease file, or a workspace with no record of its checkout, is refused:
  never collected, as no blaze that takes leases used it.
- A job that stalls keeps its leases until its tasks let go of it, so a task still writing
  holds its build dir.
- Network and FUSE file systems (NFS, SMB, AFS, FUSE, virtiofs, vboxsf, GFS2, OCFS2, BeeGFS,
  macFUSE and their kind, by `statfs`) emulate `flock` with locks that may not hold; a `build/`,
  `std/` or trash directory on one fails the scan, naming it, and `blaze clean` refuses a
  directory on one.
- A `build/` or `std/` on another file system than its area (a symlink to another volume) is not
  collected, and the scan says so: its trash, `<area>/.blaze-trash`, would be across devices.
  A unit with a mount point inside is never renamed or unlinked, and removal walks files first,
  never following a link or leaving the root's device, stopping with an error naming a directory
  on another one (`crates/blaze-gc/src/fs.rs`, `remove_tree`).
- One collector at a time collects an area: each holds `<area>/.blaze-collector` exclusively
  (`lease::collector_lock`, never waiting) and writes who it is into it (`blaze serve pid N`,
  `blaze cache gc pid N`). A second daemon on the same root, or `blaze cache gc` while a daemon
  runs, leaves the areas it does not hold alone, naming their holder; a `NoSpace` names them as
  not collected; the daemon tries again at each scan.

A job of the pool may run a while after it was asked for, so the rename carries a gate the job
checks once it holds the exclusive lock, just before it renames: a lease taken in the daemon's
process since the ask, or a head that changed, stops it (the object is kept as used after the
mark), and so does the run's end or its hard deadline.

The daemon's own roots (live heads, and the profiles a head keeps) are checked twice: when the
run marks, and again before each rename is asked for. Anything rooted or used after the mark
stays (`UsedAfterMark`). The core also keeps pins, an agent's holds and a workspace's last
transactions, checked the same way, but the daemon feeds it none of them yet (Not done yet).

## Crash safety

Removal is a rename into `<area>/.blaze-trash/` (one file system, so atomic) and, later in the run, an
unlink of the trash entry. A trash entry's name carries when its run began and a tag of the run's
number and its step (`<20 digits>-<6 base-36 letters or digits>-<name>`), so two runs never pick
one name, and a rename onto an entry that is there already is refused, never merged. The trash directory is made whole with its marker: a temporary
directory with the marker in it, renamed into place; one another process made first is used. A crash leaves each directory where it was or whole in trash; trash is
unlinked by the next run. Nothing outside trash is ever partly removed.

## A run

The core is a synchronous state machine (`crates/blaze-gc/src/plan.rs`, `Collector`): it reads
time only from `advance_clock`, owns no file or thread, and hands back decisions (`Scan`, `Trash`,
`Unlink`, `Record`) and `next_deadline`. The driver (`crates/blaze-gc/src/fs.rs`) does the file
system work.

1. Scan: every object of the areas, with its size, last use and whether its lease is held.
2. Mark and plan: each object's fate, in order: trash past its age goes; refused stays; rooted
   stays; within the grace stays; then by class (above). What is in a directory that goes goes
   with it, and a directory holding anything kept stays. Then each workspace's cap, least
   recently used profiles first. Then each volume under pressure: the bytes to bring it back
   past comfortable, least recently used first ([Pacing](#pacing-how-hard-it-works)).
3. Purge: the trash the scan found is unlinked first.
4. Sweep: one rename at a time, garbage first, then the cap, then the pressure, oldest first; the
   roots are checked before each.
5. Unlink: the run's own trash.

A slice is bounded by its fuel, bytes and deadline (above); a run asked for (`blaze cache gc`)
or urgent walks everything, bounded by fuel (4096 steps) and time (10 minutes). Each ends in a
typed outcome: `Done`, or `Stopped` naming the phase, why (fuel, deadline, the scan
failed, the clock, preempted by an urgent run) and how many steps it did and had left. What it did not reach stays
(`NotReached`). A run whose clock is past every build dir's last use by more than the TTL is
refused at the mark (`Stop::Clock`): a clock set forward would otherwise make everything look
unused. With no build dir found, the end of the last run the check passed (kept in the ledger,
with whether the check ran) stands in for it, so a clock set forward while the daemon was down
is refused too; a run that stopped before the check, or failed it, vouches for no time.

In the daemon (`crates/blaze-cargo/src/serve/gc.rs`) the loop only feeds the collector and takes
its decisions; each scan, rename and unlink runs as a job of the worker pool in the class the
collector's priority names ([Pacing](#pacing-how-hard-it-works)). Each phase has a span (`gc
scan`, `gc mark`, `gc sweep`, `gc unlink`, `gc ledger write`).

## Records and the ledger

Each build dir has a `.blaze-record` (JSON: format, checkout, last use in ns since the Unix
epoch, and each profile's or target's directory a build used with when), written whole
(a temporary named for the process and a count, sync, rename) by the daemon as a job takes its
lease, at most once a minute, on the worker pool. A shared std store's record says when a daemon
last started on it. blaze also writes a `CACHEDIR.TAG` into a build dir it leases, so backups
skip it and `blaze clean` knows it as a build dir: only into one blaze made just now or one under
`<root>/build`, never into a directory of the user's.

The collector's ledger, `<state>/gc-ledger.json`, keeps when its last run ended (so the periodic
deadline survives a restart) and its last 16 runs' reports. An asked or urgent run's is written
as it ends, a paced slice's at most once a minute. A ledger that does not read is logged
and begun afresh, never silently.

## Commands

- `blaze cache gc` runs one collection in the CLI to its end and prints every object's fate and
  why, and each volume's free space, its comfortable level and floor, blaze's bytes and share,
  the pressure and what is to be freed; `--dry-run` plans, prints the same and removes nothing;
  `--last` prints the daemon's last runs from its ledger. The CLI knows no daemon's live heads, so it keeps every workspace whose checkout exists. It collects only the areas whose collector lock it takes, naming which it left and who holds them. When a running daemon holds them all, it asks that daemon's collector for the run (`Request::Gc`, answered with `Response::Collected` as the run records) and prints its report, which knows the daemon's heads; with no daemon answering it fails, naming the holders.
- `blaze clean` is normally not needed (the daemon collects on its own); it stays as an escape
  hatch. `blaze clean --list` shows each build dir's size, whether it is in use (its lease held), when it
  was last used and its checkout. `blaze clean` and `--all` remove a directory only when all of
  these hold (`crates/blaze-cargo/src/cli/clean.rs`):
  1. it is no symlink, and its canonical path is not (by path, or by device and inode) and does
     not hold the checkout, `$HOME`, `/` or the current directory (so `--target-dir ..` is
     refused);
  2. it carries a build directory's mark: blaze's lease or record, or a `CACHEDIR.TAG` with its
     signature; anything else is refused, named;
  3. it is on a local file system;
  4. a dir inside the checkout that no build leased is refused unless `--force` (cargo, or a
     blaze from before leases, may be writing into it);
  5. no mount is at or under it;
  6. its lease, made first when it has none, is taken exclusively and checked to be its own; a
     dir in use is refused, naming it;
  7. every other lease inside it (another build's dir, a `--target-dir` named inside this one),
     found by a walk before any lock is taken (`blaze_gc::fs::nested_lease_dirs`), is taken
     exclusively too and held with its own; one held refuses the whole tree, naming the dir
     inside. The collector's renames do the same (`Keep::NestedLeasedAtSweep`). A dir inside on
     another device (a btrfs subvolume, which no mount table lists) refuses the tree, naming
     both devices.
  The locks are then held through the whole removal: the rename (into `<root>/.blaze-trash/`, the
  collector's trash beside `build/` as named, for a dir under blaze's `build/` on the root's file
  system, named as the collector names trash, else to `.<name>.blaze-removing-<pid>-<ns>` beside
  it, `<ns>` as 20 digits), a second walk of the renamed tree that takes any lease made inside
  since the first and, when a build holds one, renames the tree back and fails naming it (the
  collector's unlink of trash and the sweep of leftovers refuse such a tree too), and
  the unlink, which never leaves the dir's file system. Every such
  rename refuses an existing target in the same call (`renameat2(RENAME_NOREPLACE)` on Linux,
  `renamex_np(RENAME_EXCL)` on macOS). An entry named exactly `.<name>.blaze-removing-<pid>-<20 digits>`
  left beside it by a `blaze clean` that crashed is removed by the next one, once its pid is
  not alive (a pid this user may not signal counts as alive) and every lease in it is taken exclusively, and never when a mount is inside it; one it
  cannot remove is named.
  `--all` tries every dir and names each it left.

## Not done yet

- Eviction inside a store (`crates/blaze-cas` units and packs, least recently used): it needs the
  store to drop units from its index before their packs go; settling already retires half-dead
  packs.
- A build out of space is recognised by the OS error's code in its error's text (`(os error
  28)`); one whose error lost that text is not held. The code does not travel through the API's
  error type, so the volume is found from the error's place (`ApiError::at`), and an error
  that names no path is held only while a collected volume is below its floor.
- The bytes builds write are not fed to the collector between measures (`Collector::written`
  exists); the measure every second while builds run stands in.
- The std store's writer directories inside a store are not collected on their own; a whole
  store goes only by the TTL rule.
- `blaze run`'s binaries hold no lease while they run; a build dir is kept by its last use's
  grace and TTL, not by the running binary.
- The daemon's status does not report the last run or the pressure yet; `blaze cache gc --last`
  reads the runs from the ledger.
- Pins, holds and transactions are in the core (`Collector::pin`, `hold`, `transaction`) but
  the daemon feeds it none of them: nothing is kept as pinned, held or a last transaction, and
  budget pass 2 (the last transactions' profiles) finds nothing. Leases, the directories leased
  in the process (marked used in the collector each turn), and the profiles a live head keeps
  (used within `head_recent_ns` or by its last build) keep what builds use.
- Untested on Windows.
