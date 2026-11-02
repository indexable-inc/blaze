---
status: plan
summary: blaze v2's daemon is socket-activated by launchd and systemd from one typed unit description, takes the socket it is passed or stops naming why, and proof's budgets time restart to first answer after a clean stop and after kill -9 on blaze itself.
date: 2026-10-29
---
# Socket activation and the restart budgets (frontend-activate)

The last two bullets of [restore.md](../../design/restore.md): the daemon starts on demand,
and `proof`'s budgets time restart to first answer after a clean shutdown and after `kill -9`
on the corpus's biggest workspace.

## Design

- **Taking the socket** (`crates/blaze-api/src/activation.rs`, `activation/sys.rs`).
  `blaze serve --activated` takes the one socket named `blaze` that its manager passed, or
  stops with an `ActivationError` naming what is wrong. It never binds a fresh socket in its
  place (no-fallback).
  - systemd: `LISTEN_PID` must be this process, `LISTEN_FDS` exactly 1 (fd 3), and
    `LISTEN_FDNAMES` exactly `blaze`; all three are cleared whatever they said. The numbers are
    decimal digits only, as systemd writes them, so `+1`, ` 1` and `0x1` are refused.
  - launchd: `launch_activate_socket("blaze")`. Its `ESRCH`, `ENOENT` and `EALREADY` answers
    and a count other than 1 are each named. The `malloc`ed array is freed once.
  - Each fd is checked open and made close-on-exec before it is owned, so no build script,
    proc macro or test the daemon starts inherits the listening socket.

  The parsing is pure (`activation::systemd`, `activation::launchd`), and `take` is the one
  unsafe call (`#![deny(unsafe_code)]` crate-wide, allowed in `activation::{passed, sys}`).
- **Listening on it** (`server::Unix::{Bind, Passed}`). A passed socket becomes
  `blaze_core::Address::Inherited { fd, at }`, which the driver listens on through an
  `F_DUPFD_CLOEXEC` copy. Before that it checks that the socket is a listening `SOCK_STREAM`,
  `AF_UNIX` socket bound at `at`; a socket elsewhere, of another kind or not listening fails
  the listen naming it. The manager's socket file is never unlinked, and the `Passed` fd stays
  open for the server's life. The blaze-core side is requested of frontend-journal.
- **The units** (`crates/blaze-api/src/units.rs`), all generated from one `Unit` (program,
  socket, log, the captured environment `BLAZE_CACHE`, `BLAZE_HOME`, `HOME`, `PATH`,
  `XDG_CACHE_HOME`; never `BLAZE_SANDBOX`):
  - launchd agent `inc.indexable.blaze`: `Sockets.blaze.SockPathName`, mode 0600,
    `RunAtLoad` false, `ThrottleInterval` 1;
  - systemd user units: `blaze.socket` (`ListenStream`, `FileDescriptorName=blaze`, 0600,
    `RemoveOnStop`) and `blaze.service` (`ExecStart=... serve --activated --socket ...`,
    `Requires`/`After` the socket, `StartLimitIntervalSec=0`, `KillMode=mixed`).

  Anything a unit file can't hold is refused: control characters, relative paths, a socket
  path over `sun_path`, whitespace or `\` in a systemd path, a variable given twice. `%`, `$`,
  quotes and XML entities are escaped. `install` and `uninstall` are lists of `Step`s that
  `run` executes in order, stopping at the first failure and naming it. `installed` reads the
  main file back to tell `Yes`, `No` or `Elsewhere(socket)`.
- **The CLI** (requested of cargo, `v2/requests/cargo.md`, "frontend-activate"):
  - `blaze daemon install|uninstall`;
  - `blaze serve --activated`;
  - `connect` never spawns where a unit serves the socket: a dead activated socket is an
    error saying `blaze daemon install` reloads it. The spawn stays only where no unit is
    installed, and says so on stderr;
  - `stop` under activation waits for the daemon to hang up, since the manager keeps
    answering the socket;
  - `Status.pid` is printed first by `blaze status`.

  Warm process pools already start lazily on first use (`crates/blaze-usercode/src/pool.rs`).

## The restart budgets

`harness/v2/budgets.py`, budget `restart`, rows `restart-first-answer-clean` and
`restart-first-answer-kill9`. They run on blaze's own checkout: 2.9M lines of Rust,
the biggest workspace of the corpus (blaze, nushell, ripgrep, helix and the top crates in
[proof.md](../proof.md)). Each row runs after blaze's cold build. Until the first answers
settle the budget (at least `runs` and at most `most_runs` times; the median's 95% interval
within 10% of the limit or clear of it, as every timed budget does), it repeats:

1. a warm no-op `blaze check`, timed;
2. the daemon's pid read from `blaze status` (v2's `pid: N`, v1's `daemon: pid N`);
3. the stop: `blaze stop` until the pid is gone, or `SIGKILL` to the daemon's process group
   until the group is empty. A group equal to the harness's own is refused;
4. a no-op `blaze check`, timed from the command to its result: the first answer.

The row is the median of the first answers against
`median(warm) + restart_slack_ms (5 ms) + [Linux] 0.5 us x files`. The last term is the
statx sweep restore.md prices at about 50 ms for 100k files, since no journal survives a
restart there; macOS (FSEvents `sinceWhen`) and Windows (USN) replay a journal instead. The
statistics (the median's 95% interval and the outliers) are `timed`'s, as for every other row.
`activation: true` puts the daemon under `blaze daemon install` for the rows, with
`uninstall` after; it is for an ix VM, where replacing the user's units is fine. Without it,
the command line starts the daemon. The detail of each outcome says which way it was started.

Why 5 ms: the target is "the warm answer plus a few milliseconds". The speed of light below
adds up to about 2 ms over the warm answer, and 5 ms leaves headroom for exec variance without
hiding a real regression: a deserialized image or an eager rescan costs tens of ms at least.

## Speed of light

What a restart adds over the warm answer, on a warm page cache (a restart, not a reboot):

| step | cost |
|---|---|
| connect to the manager's socket; the request waits in the backlog | ~5 us |
| manager's fork and exec of `blaze serve` (static Rust binary, pages mapped lazily) | 0.5-1.5 ms |
| `take`: 3 getenv, 3 unsetenv, 2 fcntl; or one `launch_activate_socket` XPC round trip | 5-100 us |
| inherited listen: dup, 2 getsockopt, getsockname | ~2 us |
| image: mmap and one header check | ~10 us |
| first-touch faults of what the no-op check reads (fault-around of 16 pages, ~0.5 us each; ~50 MB) | ~0.4 ms |
| Linux only: one batched statx sweep through io_uring | 0.5 us a file |

That totals about 1-2 ms plus the sweep. Against the CLI's own spawn path, activation saves
the poll: the client's connect is already queued when the daemon listens, so the first
`accept` returns it at once, with no retry interval and no race. The reference to match or
beat is systemd and launchd socket activation as sshd and docker.socket use it: zero lost
connections and no added round trip. Bazel's and Gradle's servers instead spawn and poll,
taking hundreds of ms before the first answer.

A known risk, measured by the kill -9 row with `activation: true` on macOS: launchd's
`ThrottleInterval` of 1 s (the least it takes) delays a respawn within 1 s of the last start.
If the row fails for that reason, the fix is in the unit, not the harness.

## What proves what

- `activation/tests.rs`:
  - one valid systemd passing;
  - another process's `LISTEN_PID`;
  - missing variables named;
  - too many fds, with their range;
  - digits-only numbers;
  - names that are missing, mismatched, too many or empty;
  - every launchd answer;
  - `Passed` closes its fd.
- `units/tests.rs`:
  - launchd plist and systemd socket and service golden tests;
  - escaping of XML, `%`, `$` and quotes, with `socket_of` reading back what was written;
  - refusals: relative paths, control characters, socket length, duplicate or bad variable
    names, systemd whitespace and `\`;
  - the exact install and uninstall steps for both managers;
  - `installed` read from the main file: `No`, `Yes`, `Elsewhere` and a broken file;
  - captured variables (never `BLAZE_SANDBOX`) and unit directories;
  - `run` in order, mode 0600/0700, no temporary left, the first failure named, `Undo`.
- `server/tests.rs`, `client/tests.rs`, `session/tests.rs`: `Unix::Bind` and `Passed`,
  `until_hung_up`, `Status.pid` at `SCHEMA` 10.
- blaze-core's `net/tests.rs` (requested): an inherited socket is listened on through a
  close-on-exec copy, and its file and fd outlive the listener; one elsewhere, not listening,
  not a socket, TCP or closed is refused, and nothing is made in its place.
- `budgets.py selftest`:
  - the pid read from v1's and v2's status, and a status naming none or two refused;
  - the limit arithmetic with and without the sweep;
  - a row per way of stopping, on blaze, at most, in ms;
  - `restart_slack_ms` and `activation` parsed, with their defaults and bad values refused;
  - `alive` on a live and a reaped process;
  - a SIGKILL to the harness's own group refused.
