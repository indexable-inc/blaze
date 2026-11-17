---
status: plan
summary: "A save is built when the watcher commits it, not when a command asks (lane onsave-1112) - the daemon sends itself the checkout's last build or check, exactly as its client sent it, on a connection of its own; one such build at a time, never beside a request's; a request for the same thing waits for it and then walks, any other request cancels it and goes on at once. Built, behind `BLAZE_AHEAD=1` - the decisions are a sans-IO core (`serve/ahead.rs`) with a schedule test and four planted bugs. Not built - the default, battery and load signals, a barrier on macOS that needs no flush. The watcher's flush stays on every request; when it could go is said here."
date: 2026-11-12
---
# Built at the save

An edit then `blaze run` pays its build after the command arrives (55 to 120 ms on a small
project) where a run with nothing changed is 12 ms. The daemon hears the save tens to hundreds
of milliseconds before the command: the build belongs there.

## What the daemon does

**It sends itself the last request.** For each checkout the daemon keeps the last
`Request::Build` or `Request::Check` a client sent of it, whole (`blaze build`, `run`, `test`
and `check` are these: the command, its arguments, features, profile, directory, environment
and how diagnostics are drawn; the CLI, not the daemon, runs the program and the tests). When
the watcher commits a save (`heads::Out::Moved`) and no answer of that request is kept for the
head's new root (`Jobs::answer_known`), it arrives again, unchanged, from the daemon's own
connection (`ahead::CONN`), through the same door as a client's: the watcher's flush, the
round outside views, the session, the no-op lane, a job. Nothing is a second kind of job.
Its replies go to the core and nowhere else: nothing is shown until asked. A build ahead that
made something leaves no kept answer (`Unkept::Made`), so once it ends the same request is sent
once more: a walk that makes nothing, whose answer the no-op lane keeps. The command that
follows is then answered by the lane.

**One at a time, below every request.** At most one build ahead exists in the daemon, and it
starts only when no job is under way, no request is held for a watcher and no output is being
placed. A request that reads a checkout or builds, read off any connection while a build
ahead runs, is one of two things, decided as it is read and again as its watcher's flush lets
it through:

- *the same request on the same root*: it is held until the build ahead ends, then handed to
  the session, where it walks and finds every unit made (or is answered by the lane). The job
  it waits for is the one it would have started, begun earlier, at the same priority, alone.
  Its wait is bounded by that build plus one walk that makes nothing (4.5 ms measured, lane
  edit3-1111, row edit3-14).
- *anything else* (another command, package, environment, checkout, or a root that moved): the
  build ahead is cancelled as the request is read, and the request goes to the session as
  soon as its own flush is in. It never waits for the cancelled job to end. What that job
  finished (records in the store, memos in the database) is keyed by content and found again.

A newer save cancels a build ahead of the older root. Nothing ahead is started after
`STARTS` (3) of one root, after one that failed (its diagnostics are in the units' records:
the failure is the answer waiting), or while the daemon stops.

**Saves are coalesced.** A build ahead starts `QUIET` (5 ms) after the checkout's last commit:
an editor's write-then-rename lands in one or two commits a millisecond apart; a formatter's
rewrite 30 to 300 ms later cancels and starts again, which costs the part of one small build.
A commit that changes more than `MASS` (64) files since the root last built is a branch
switch or a rebase: nothing is built ahead of it, and the next request builds it. Building
only its front end would need a job of another shape on the view, which is what failed before.

**User code.** The request sent is one a client sent, so build scripts and proc macros run
exactly as that client's build ran them, sandboxed. No program and no test is run ahead: the
CLI runs those. `Request::Test` and `Request::Run` (run inside the daemon) are never sent.

**Resources.** A build ahead is a job like any other: admitted by footprint, counted by the
ceiling, covered by the stall watchdog; each says one line in the daemon's log as it ends.
Not built: holding off on battery (macOS `IOPSGetProvidingPowerSourceType`, Linux
`/sys/class/power_supply/*/online`) and under load (`getloadavg` against the cores).

**The switch.** `BLAZE_AHEAD=1` in the daemon's environment turns it on. It is off without
it for one reason: a save built ahead leaves the request after it nothing to make, and two
readers still take a request's own build for the edit's whole work: `blaze why`, and the
revision tests' `redone` (25 cases, lane sound-1112's oracle). Once both count a build
ahead as the edit's, the default flips (`serve::build_ahead`). The variable is the daemon's:
set it on the command that starts the daemon, not on every command. Today any `BLAZE_*`
variable but `BLAZE_HOME` on a command sends the thin client through `blaze-engine`
(`blaze-thin/src/route.rs`, `refuses`; 6 to 8 ms a request, lane launch's measure). A
daemon's setting should never change the client's route: the thin client should refuse only
the variables the client's own path reads, by name (row onsave-1112-4).

## Why this cannot fail as the save built ahead did

The save built ahead of 2026-11-11 (e82b3ff856, reverted in f3998289a2; docs,
incremental-walk.md) started *another job* on the view: a check where the request was a build,
as an intent's job, then promoted beside the request's own. Measured there: the check
"resolved the dependencies again as crates of its own (3,733 item trees run new) and wrote
`AllCrates`, `SourceRootInput`, `PathBucket` and `ProcMacros`; the run after it walked from
fields the whole program reads and never ended", and a build in the check's place "let the
first edit through in 2.4 s and hung the second". Its rule: "a second job on a view must first
write no input the first job's rows read".

Here there is no second job and no other shape. The build ahead is the client's request,
byte for byte, alone in the daemon; the request that follows runs after it, as a second
`blaze run` typed by hand runs after the first. Every state the engine reaches is one two
commands typed in a row reach today.

## Correctness

A request's answer is made by the request's own job or by the no-op lane, never by the build
ahead: the build ahead only fills stores keyed by content (`keys-cover-every-input`). So the
answer rests on what it rested on before: the flush (every save made before the request is
in the head when the session resolves it), the lane's checks, the walk's keys. A half-applied
change is a root like any other: a build ahead of it makes records under that root's keys,
and the request's root is another. A cancelled build ahead is a cancelled build: what a
Ctrl-C leaves, which the revision oracle's `cancel` step holds to a clean build.

## The watcher's wait

A flush covers a request only if it was asked after the request was sent: an event of a write
made a moment before may still be in flight, however long the watcher has been quiet (row
edit3-14: a flush told 8 s late under load). So no request can go without its own flush, and
speculation changes nothing about that. On Linux the flush drains inotify's queue, which the
kernel fills in the write itself: one loop turn. On macOS it is a cookie through fseventsd,
6 ms median (row edit3-4). What would remove it soundly there: a kqueue `EVFILT_VNODE` watch
of each directory and source of a small checkout beside the FSEvents stream; a knote is posted
inside the write, so one `kevent` with no wait is a barrier for those vnodes. Not built.

## A launched program

Under `BLAZE_LAUNCH=1` every build of a program is told to its running launchers
(`serve/unit/run/launch.rs`), so a build ahead of a `blaze run` applies the edit to the
running program at the save. That is the default while `BLAZE_LAUNCH` is itself the opt-in:
a save is a deliberate act, and a save that does not compile applies nothing.
