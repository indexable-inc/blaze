---
status: current
summary: What blaze removes from disk and when: the collector of build dirs, shared std stores, temporaries and trash; the pacer that works harder as a volume fills, leases, records, roots and the grace, and why nothing in use is removed; the daemon's store under the home's budget, its pins and what a build says of it.
---
# Collecting what blaze keeps on disk

blaze writes outside the checkout only: a build dir per workspace under blaze's root
(`<root>/build/<name>-<id>`, [cargo.md](cargo.md#where-a-build-writes)), the daemon's state
(`<root>/v2`), and one standard library store per code of what makes std's records (the front
end's, the code generator's and blaze-cargo's code identities, `serve::RecordMakers` in
crates/blaze-cargo, never the executable's bytes), shared by every daemon of the machine
(`<machine>/std/<makers>`). The collector, `crates/blaze-gc`, removes what is no
longer used of the first and the last, so a machine that builds many checkouts does not fill its
disk. The daemon's state (its store, image and journal) is counted against the home's budget and
its store is collected record by record
([The store's budget and collector](#the-stores-budget-and-collector)).

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
profiles it keeps (below), and anything within the grace never goes, however full the disk;
the grace itself shrinks with pressure ([Every byte has an owner](#every-byte-has-an-owner)).

A checkout counts as gone only when its path is not found and its parent is there; any other
error (a permission, an unmounted volume) refuses the workspace. A workspace whose checkout is
gone keeps a day's grace, so a checkout moved away for a while keeps its build.

Nothing goes within the grace (an hour since its last use up to half pressure, then linearly
less, a minute at full pressure and in an urgent run), and neither does a directory that
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
- also from blaze's budget: blaze's own bytes on a volume past 80% of its budget (a quarter of
  the volume, and no more than 64 GiB) raise it, full at the budget; what is to be freed for it
  is what is past that 80%, so it is never 0 while the budget holds pressure on;
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
{ "comfortable_percent": 15, "comfortable_gib": 30, "floor_percent": 10, "floor_gib": 20, "share_percent": 25, "budget_gib": 64, "auto": true }
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
- A std store with no lease file is refused: never collected, as no blaze that takes leases
  used it. A directory under `build/` with no lease file is adopted, not refused: `build/` is
  blaze's, so the scan makes its lease (a dry run too: it removes nothing) and from then on it
  is a workspace like any other. With no record it names no checkout: its last use is its
  newest stamp, it goes unused past the TTL or under pressure, and while a cargo holds a
  `.cargo-lock` in it or in a profile of it, it counts as leased (`fs.rs`, `workspace`,
  `cargo_locked`). Before 2026-11-12 it was refused for ever: thirteen cargo target dirs
  someone had pointed at `build/` held 464 GB of a full disk that way.
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
deadline survives a restart) and its last 16 runs' reports. An asked run's is written as it
ends, as is an urgent run's with no build under way; an urgent run's that ended beside a build
is written once no build has been under way for a second (never beside the build's outputs),
a paced slice's at most once a minute. A ledger that does not read is logged
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

## The store's budget and collector

Built 2026-11-12 (lane storegc-1112): the core is `crates/blaze-cas/src/collect.rs`, its
driver `crates/blaze-cargo/src/serve/collect.rs`, its simulation
`crates/blaze-cas/src/tests/collect.rs`. What is not built is under [Not done yet](#not-done-yet).

**Budget.** One number for a home (`<root>`), the tightest of three, each set in its
`gc.json`:

| Bound | Setting | Default |
|---|---|---|
| an absolute size | `budget_gib` (or `budget_mib`), `blaze gc --budget <size>` | 64 GiB |
| a share of the volume | `share_percent` | 25% |
| what leaves the floor free (held + free − floor) | `floor_percent`, `floor_gib` | max(10%, 20 GiB) free |

The defaults are one row of code, `blaze_gc::pace::BUDGET`; the rule is
`Levels::budget`. Held is the build dirs the collector lists and what the daemon says its
state holds (`Collector::holds`: the store as its head names it, and every other file under
`<root>/v2`, measured on a worker once a minute), so the collector of build dirs and the
store's work against one number. Pressure is 0 under four fifths of the budget and full at it.

**Collectable.** A record of the store that no root keeps, in a pack not used for the grace
(an hour; `store_grace_s` in `gc.json`). Use is kept two ways. Per pack: when a record of it was last read, or a new value
appended to it, in seconds (`PackRef::used`, in the head since store format 6; a head of
format 4 or 5 is read with none and its packs are stamped by the store's clock as it opens).
Per record: the four flags the cold tier keeps (`Read`, `ReadBefore`, `New`, `NewBefore`),
which a read sets, and `has`, and a put of a value the store already has. What moves cold is
written to packs of its own (`Store::cold_unit`), so a pack of what nothing reads ages as a
whole and is retired without a copy. The roots, never forgotten: (1) a record with one of the
four flags; (2) a record a pin names; (3) what a record that stays names inside the store (a
chunked output's chunks, a derived value's base): what only a forgettable record names goes
with it in the same seal, never before; (4) the key placed at a path until the path holds
another (`Store::place`, `unplace`); (5) a record the index table does not hold yet (the delta
holds the newest 65,536); (6) a dictionary a pack is retired around is written again first. A
pack is sealed only while no build is under way, queued, stored or placed, and only as scanned
since the last one began, so a build never finds a key and then misses it. A reader that pinned
a record's pages (`Pinned`, a kept front end's images) keeps reading them: the pool gives a
retired pack's pages back only after a view without it is published, its grace has passed and
no pin holds them.

**Kept set.** A pin is a record of the store (`Kind::Pin`: a name, why, when, its keys; the
newest of a name is the name's, one with no keys lets it go; `collect::Pin`). The daemon
writes two unasked at a build's end: `toolchain` (the standard library's unit records, their
slots and outputs, as builds find them in the store) and `bstd` (the same of bstd's units).
The pin `vfs image` names the listings (`Kind::Set`) of the texts the daemon image on disk
names, and while an image is made the next one's beside it (blaze-cas `sets::FileSet`): an
image's texts are a root from before a byte of it is written until the image after it is in
place. The pin of a name made last is the name's, whatever the clocks did (`Store::pin`).
`blaze gc --dry-run` (or `--kept`) prints each pin with its keys, bytes and why, and what else
is kept and by what, and removes nothing. `blaze clean` is this project's build dir, as before;
`blaze clean --unkept` asks the daemon's store to forget every record no root keeps, whatever
the budget and the grace.

**Forgetting a key.** By retiring its pack, never by a tombstone: the tables stay
append-only. The collector takes one pack (`Store::collect_take`; nothing is appended to it
from then on), a worker reads it and judges each record (`CollectBatch::work`), the store
copies the kept ones on a few at a time (`collect_copy`), and the seal (`collect_seal`) names
the pack retired: a slot of the index table that points into a retired pack finds nothing
(`TierView::gone`), at once in memory and after any reopen, and the next index table leaves it
out. A record the delta holds is never forgotten, so no spot ever points into a retired pack.
A pack more than three quarters kept is passed by; one that is half dead bytes is rewritten
whatever the budget (this replaces the settle step's rewrite on the loop).

**Crash safety.** The copies and their checks are behind a barrier before the head; the head
is one whole write and names the pack removed; the file is removed after it is durable, and an
open removes those a head names removed. A crash before the head leaves the old store (the
copies are a tail no head names); after it, the new one.

**Pacing.** `Collector` is a core on the daemon's clock: it owns no file and no thread. The
loop gives it the facts each turn (held, budget, whether a build is under way, builds begun,
the time) and takes one step: a batch handed to a worker (`store gc read`: `Background` under
half pressure, `Normal` from it), or a bounded piece of work on the store (`store gc step`:
at most 256 KiB copied, a pack sealed, a commit asked for). The bound of a step is 1 ms; a
step past it is counted and said. Under half pressure it steps only while no build runs; from
half on it also copies while one does, and seals between builds. The gap between two packs
goes from 60 s at no pressure to 50 ms at full. While a build is under way the collector
begins no commit: what it copied rides the build's own. Between builds it commits after every
256 KiB copied, with no index table in that commit, so a request that arrives meanwhile waits
at most for the rest of one such commit (counted, `Counts::flush_waits`). The period-end walk
of the store's rows, which plans the cold tier and marks the roots, runs on a worker
(`PeriodScan`, `store period scan`); the loop hands it out and takes its plan back, so its
cost on the loop no longer grows with the store. When every pack was looked at and the home is
still at its budget, the collector says `OverBudget`, removes nothing in use, and looks again
after ten minutes.

**What a person sees.** Every build's end carries `Worked::storage` (`store_bytes`,
`store_delta_bytes`, `held_bytes`, `budget_bytes`, `collectable_bytes`, `volume_free_bytes`,
`over_budget`). Its `warning` is a line only when the home holds 85% of its budget or more,
or its volume is under its comfortable level, at most once in six hours (once an hour when
over), naming `blaze gc --budget <size>` and `blaze clean --unkept` (`serve::collect::Warner`).
There is no prompt, during a build or after it: keys typed ahead for the next command would
answer it, `blaze run` has handed the terminal to the program, an agent on a terminal would
hang or answer by accident, and nothing waits on the answer since the collector is already at
work. At the budget the build proceeds and the line says what is kept; a build fails only
when a write fails for want of space, as [above](#pacing-how-hard-it-works).

**Measured by** `harness/storegc.py` (one JSON value): a home filled by several generated
programs, the budget set to a share of what it holds, body edits before, during and after the
collection, the bytes at each step, and what `blaze gc --dry-run` says is kept.

## Every byte has an owner

The class this closes: bytes under a home that no collector knows, and bytes a collector knows
and will never free. Measured on one machine on 2026-11-12, its disk at 2.4 GB free: the home
held 1,337 GB; the collector tracked 498 GB of it, all `Refused`, and freed nothing at full
pressure; 840 GB was in names it never listed (lane scratch, a nested home, a store set aside
by hand after a format change, the directories of the daemon before v2).

The rule: **every entry right under a home has exactly one owner, every byte under the home is
counted to its entry's owner, and when not enough can be freed the report names the largest
holders with the reason each stays.** The registry is `blaze_gc::roots::ROOTS`
(`crates/blaze-gc/src/roots.rs`), one line a name:

| Owner | Names | Who removes it |
|---|---|---|
| builds | `build/` | the collector above; a directory in it with no lease is adopted |
| std | `std/` | the collector above |
| state | `v2/` | the store's collector, record by record; the rest is bounded by its writers |
| trash | `.blaze-trash/` | the collector's trash phase |
| fetched | `sources/`, `toolchains/`, `library/` | nothing yet: counted, and named as held for that reason |
| fixed | sockets, locks, `gc.json`, `.blaze-collector`, `homes.json` | nobody: small, fixed size |
| backup | `<root>.<label>.backup-<digits>` of a registered root (`v2.store-format-3.backup-20261111`) | the home's pass: 7 days after its last write, or under pressure |
| retired | what the daemon before v2 wrote (`workers/`, `checkers/`, `spares/`, `sandbox/`, `store/`, `daemon.log`, `last-build`, ...) | the home's pass: 14 days after its last write, or under pressure |
| foreign | any other name | nobody: no blaze made it, so blaze does not remove it; it is counted and named |

The home's pass (`roots::account`, a function of the listing, the time and the pressure; its
driver `roots_fs::pass`) runs before a collection (`fs::home_pass`, from `fs::run_now`). It
lists every entry right under the home with its bytes on disk, its newest stamp and whether a
lock file in it (right inside or one directory down) is held. In order: what a lock holds
stays; what was written within the grace at this pressure stays; a backup or a retired name
past its lifetime goes; then, while the volume still has bytes to free, backups and then
retired names go, least recently written first. What goes is renamed into the home's trash
while every lock file in it is held by the pass; the collection that follows unlinks it, so
there is one unlink path and a crash leaves an entry where it was or whole in trash. All of the
home's bytes, a foreign name's too, count against blaze's share of the volume, so what is to
be freed is no longer computed from a part of the home.

The graces shrink by one rule (`roots::grace_at`): whole up to half pressure, then linearly
down to a minute at full pressure; an urgent run is at full pressure. The hour's grace, and the
day's for a checkout that is gone, covered the moment before a new directory's lease is taken
and a checkout moved away for a while; neither is worth a full disk, and a minute still covers
the first. A lease, a live head, a pin and a held lock keep what is in use at any pressure, so
a disk can fill only with what is in use or what is not blaze's, and then the report says which.

The report (`blaze cache gc`, `--last`, `--dry-run`) carries the account: the home's bytes and
how many are in what no blaze made, each entry that goes with why, and the twelve largest
entries that stay, each with its reason (`Account::lines`).

Nothing may be written right under a home outside a registered name. The guard:
`roots_fs::unregistered(home)` lists the names no blaze makes; a test that builds into a fresh
home fails on any (not wired into a daemon test yet, below). An agent's or a person's files do
not belong under a blaze home: a foreign name there is theirs to move out.

Evidence: `crates/blaze-gc/src/roots_tests.rs` plays the pass on a model home under 2,000
seeds, crashing after every rename and every unlink and restarting. The oracle: nothing a lock,
the grace or another owner keeps is removed or changed, nothing outside trash is partly
removed, no byte leaves the count, a restart empties trash; its dual: at full pressure with
garbage present, bytes are freed, and either enough are or no garbage is left. Controls the
oracle catches: the grace not shortened under pressure, a foreign directory left out of the
account, a held lock not seen. Not run yet (written 2026-11-12 in a lane that builds nothing).

Not built: the daemon's own collection does not run the home's pass yet (only `blaze cache gc`
does, when no daemon holds the home); a build's output does not warn with the holders; the
guard is not called from a test that builds; `sources/` and `toolchains/` have no collector;
a home made for a lane or a test has no lifetime. The design for the last: each home registers
itself in the machine's list (`<machine>/homes.json`: path, creator, made, expires; written
whole under the list's lock as a daemon first starts on the home; `BLAZE_HOME_EXPIRES` or a
default of 7 days for a home outside `~/.cache`, none for the user's own), `blaze cache homes`
lists them with sizes and expiry, and a collection on any home removes a listed home that is
past its expiry, whose collector lock is free and whose directory still carries the list's
nonce, by the same rename into its own trash.

## Not done yet

- The store's collector (above), what is designed and not built: a store whose head is of a
  format this binary does not read is still refused at the daemon's start, not moved aside as
  `v2/store.format-<n>` and trashed after a day; a stale image is replaced by the next, never
  collected on its own; `blaze clean --everything`; `blaze clean` does not forget the
  project's own records (records carry no project); the machine's shared std stores have no
  record-level collection (a whole store goes by its TTL). Its limits: an index swap carries the
  flags of use over on a worker (`FlagCarry`, `store flags carry`), and until that is back the
  store forgets nothing; a store with no index table yet (under 65,536 records) forgets
  nothing; the index table is built on a worker (`blaze-cas/src/rebuild.rs`, 2026-11-12, not yet run; a daemon with no collector still builds it in a commit); the period's scan
  walks every row and reads every chunk list each period (on a worker); the daemon's own
  measurement (`harness/storegc.py`) has not been run.

- Records an older blaze made (every record's key covers the code of what made it, so a
  rebuilt blaze reads none of an older one's: a home used by rebuilt compilers for a day held
  2.2 GB of them, measured 2026-11-11, lane disk-1111) are not known as such: they go cold
  unread and are forgotten a pack at a time when the home nears its budget, not at once. The
  design that would drop them whole: the store's directory named for the makers' identity
  (`v2/store-<makers>`, as `std/<makers>` is).
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
