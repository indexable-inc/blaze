---
status: plan
date: 2026-10-30
summary: no-fallback in crates/blaze-cargo (not sandbox; codegen.rs, compile.rs, job.rs are the parent's): the daemon image, its start and settle, the watchers, the plane; cli/toolchain/socket in cargo-cli.md and serve/unit + fetch in cargo-units.md.
---
# no-fallback: blaze-cargo

Kinds as in [../../no-fallback.md](../../no-fallback.md). Line numbers are as of 2026-10-30 after the
edits. Two parts of the crate have their own tables, one row per site, not repeated here:
[cargo-cli.md](cargo-cli.md) (`cli/`, `toolchain/`, `socket*`, `answers*`, `stub.rs` (deleted),
`raw*`: 48 rows) and [cargo-units.md](cargo-units.md) (`serve/unit*` but codegen and compile,
`serve/fetch*`: 56 rows, with the parent's codegen.rs and compile.rs rows marked). This table is
the rest of `serve/`.

The daemon image rule: an image another blaze wrote (another layout, or a section of another
schema) is legitimately unreadable, so that state starts empty, and the start says so as a
typed reason (`daemon::Started`, printed once by `blaze serve`). An image that is damaged, cut
short or does not match its journal stops the start (`daemon::StartError::Image`); one found
corrupt while the daemon runs stops it (`daemon::Failed`).

| file:line | kind | what it does | what it becomes |
|---|---|---|---|
| `serve/daemon.rs:388` `opening` (was `:273-279`) | error / rule | any `Image::open` error printed "starting with an empty database" and went on | `OpenError::Layout` is the rule `Cold::Layout`; `Header`, `Hash`, `Section` are `StartError::Image { fault: ImageFault::Open }`. Done |
| `serve/daemon.rs:397` `unrestored` (was `:295-298`) | error / rule | any symbol-table restore error printed and started empty | `RestoreError::Refused` is `Cold::Refused`; `Started` is `Cold::Interned` (a second daemon in one process, the simulation's restarts; `blaze serve` bails on it, `serve.rs:246`); `Corrupt` and `Invalid` are `ImageFault::Symbols`. Done |
| `serve/daemon.rs:406` `unopened` (was `:305-309`) | error / rule | `DbError::is_image()` errors printed and started empty | `DbError::Refused` is `Cold::Refused`; `Corrupt`, `Invalid`, `Symbols` and every other are `ImageFault::Db`. Done |
| `serve/daemon.rs:414` `vfs_used` (was `image_use`, `:289-291`, `:701-720`) | error / rule | every `ImageUse` printed and went on | `Refused` (tree of another layout) is `Started.tree`; `Resumed` (a stop between rename and journal rewrite) is `Started.resumed`; `Corrupt`, `Malformed`, `Unnamed` (the journal is of another image: loading it alone loses the tree) are `ImageFault::Vfs`. `image_use` deleted. Done |
| `serve/daemon.rs` `Started.disks` (`ImageUse::{Used,Resumed}.disks`) | rule + error | the watchers' metadata not used, so they scan, for any reason: blaze-vfs gave it as a `String` | blaze-vfs gives `DisksUnused`: `Refused` (missing, another schema) is the rule, `Started.disks: Option<Refused>`, said at start; `Corrupt`, `Outside` and `Malformed` are `VfsFault::Disks`, which stops the start naming the image. Queue 01169. Done |
| `serve/daemon.rs:542-543` (was `:283-288`) | rule | journal's bytes after its last whole record cut, heads whose root was in them dropped: what a stop in an append leaves | `Started.journal_cut`, `Started.heads_dropped`, said at start. Done |
| `serve/image.rs:76` `history` (was `:39-56`) | rule | history section missing or of another version printed and started empty | `Result<(History, Option<Unlearned>), HistoryError>`: `Unlearned::{Refused, Version}` into `Started.unlearned`; corrupt or undecodable is `ImageFault::History`. Done |
| `serve/daemon.rs:919` (was `:654-656`) | error | a database group found corrupt printed "what it held was computed again" (the requests that read it already fail with it) | the daemon stops: `Failed::Corrupt`. Done |
| `serve/daemon.rs:919` (was `:657-664`) | error | a VFS tree group found corrupt printed and rescanned every checkout | the daemon stops: `Failed::Corrupt`. `Heads::rescan` and `Daemon::vfs_corrupt` deleted (dead). Done |
| `serve/daemon.rs:947` (was `:676-678`) | error | a failed settle printed and the cold build's arenas stayed | the daemon stops: `Failed::Settle(DbError)`. Done |
| `serve/daemon.rs:934` | error | an image a cold build asked for that was not made was only printed ("the image before it stays") | the daemon stops: `Failed::Image(ImageFailed)`. Done |
| `serve/daemon.rs:960` | error | the stop's image not made: printed, the stop exited 0 | the stop ends failed with `Failed::Image`, `blaze serve` exits non-zero. Done |
| `serve/image.rs:256` `Images::fail` (was `:182-186`) | error | every image failure `eprintln!`ed and kept as a `String` | typed `ImageFailed { file, why: Unmade }` (Busy, Vfs, Db, TooLarge, Write, Map, Open), no stderr; its asker fails with it (snapshot via `job.rs` through `From<&ImageFailed> for String`, cold build and stop above). Done |
| `serve/daemon.rs` `Daemon::start` | error | returned `String` | `StartError { Image { file, fault }, Other(String) }`; `Other` for the existing setup errors. Done |
| `serve/daemon.rs` `failed` | error | `Option<String>` | `Option<Failed>` (`Server`, `Corrupt`, `Settle`, `Image`). Done |
| `serve/daemon.rs:508-509` | rule | dial fails: no other daemon; unlink of the socket fails: taken as free | an unlink that fails for another reason makes the bind fail, naming the socket. Stays |
| `serve/daemon.rs:541` `read.journal.unwrap_or_default()` | rule | the loop leaves only once the journal is read (`Some(Vec::new())` when there is none) | never `None` there. Stays |
| `serve/heads.rs:411` (was `:410-412`) | error | a file that changed and can't be read printed "the head keeps what it had": builds then read its old bytes | ends the checkout's watch like an unreadable root: the requests waiting fail naming `<head>/<path>`, the next request watches again. Done |
| `serve/heads.rs:405` `Watched::Conflict` | rule | a file changed on disk and by a commit since: the head keeps the commit's, said on stderr | the later write wins, as the session committed it after the disk's change. Stays |
| `serve.rs:195` (was `:192`) | error | `available_parallelism()` failing guessed 4 workers | an error naming it. Done |
| `serve.rs:202-209` | error | `$PATH`/`$HOME` that is not Unicode dropped by `.ok()` | absent is left out (optional); not Unicode is an error naming the variable. Done |
| `serve.rs:244-251` | rule | | `Started` printed once when not whole; `Cold::Interned` is an error in `blaze serve`. Done |
| `serve/plane.rs:711-714` | error | a finished request with no stats gave zero stats | panics naming the request, as every other plane invariant here does. Done |
| `serve/plane.rs:237` | rule | an IO task that gave way and does not read runs again on its thread | the plane's give-way protocol, the task's own wait; not another path. Unbounded by design, flagged for the plane's owner |
| `serve/plane.rs:260,412` | rule | `as_nanos` past `u64` saturates | 584 years. Stays |
| `serve/plane.rs:270` | rule | a panic with no message | an optional text. Stays |
| `serve/mounts.rs:95` | rule | `remove_head`'s ticket dropped | a removal's durability is not waited on; `None` means no such head. Stays |
| `serve/work.rs:481` `Writer::keep` (+ `serve/unit.rs:408`, `serve/tests.rs:431`, parent's `serve/job.rs:2205`) | error | `None` when a write fails to apply or a reader is alive: the kept front end is dropped and the head's next build lowers in full, no reason given | `Result<S::KeptFront, ApiError>` naming why (a reader still alive, another holder, the apply's error); the build ends with it. Queue 01166. Done |
| `serve/unit.rs` `seed` (+ `blaze-lower` `Lowered::refresh`) | error | edit seeds handed over after the refresh's write, whose nameres maintainer had already parsed the files: every seed refused, `retain` dropped them, each file parsed whole | `refresh` takes a `seed` closure run inside the same `Db::batch` after the texts are set; a refused seed is a `LowerError` naming the file. Queue 01185. Done |
| `serve/plane.rs` `panicked` (+ `serve/job.rs` `panicked`, `serve.rs`) | error | a task's panic dropped in the units stage: the job ended failed with no error, the client exited 101 printing nothing, the panic only in `serve.log` | `plane::Panic` holds the message and the thread and `file:line:column` a hook `serve` installs saw it raised at (a panic re-raised from rayon's thread is where it was first raised); `Happened::Panicked` fails a units job with an error naming the unit. Queue 01287. Done |
| `cli.rs` `drive`, `cli/daemon.rs` `died` (+ `serve/signals.rs` `record_ends`, `raw/process.rs` `reap`) | error | a daemon that died mid-request (abort, signal, OOM kill) said only "the daemon hung up": how it died was only in `serve.log` | the client finds the daemon's last start in its log (`listening on … (pid N)`), says how it ended (waited for when this command started it, else gone or still running) and quotes the log since; the daemon writes a line for each fatal signal it can catch. Queue 01328. Done |
| `lib.rs` `refused_error` (`REFUSED_SHOWN`) | rule | the refused-functions error named the first 16 and nothing could raise it | `--refused N|all` fills `blaze_api::Cargo::refused` (16 by default, `None` for every one), carried in `JobInput` and in the unit's work key. Queue 01335. Done |
| `serve/unit/codegen.rs`, `serve/unit/compile.rs`, `serve/job.rs` | parent's | | in the parent's table, `no-fallback/link.md` |

Tests: `serve/daemon.rs` `mod tests` (each classification: layout vs damaged open, symbols,
database sections, every `ImageUse`, the `Started` text, a damaged image naming its file);
`serve/image.rs` (`Unlearned` for a missing, another-schema and another-version history,
`HistoryError::Decode`, `ImageFailed`'s text).
