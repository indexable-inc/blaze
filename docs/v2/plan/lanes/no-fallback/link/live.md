---
status: plan
date: 2026-10-30
summary: no-fallback sites in blaze-live and blaze-stencil (lane no-fallback-link-live), each an error, a rule or dead, and what it becomes; the v1 runtime and its sampler are dead in v2 and queued for deletion with their callers.
---
# no-fallback: blaze-live and blaze-stencil

Part of [no-fallback](../../no-fallback.md), area link. Ranked: the v2 live runtime
(`blaze_live::slots::Live`, which takes blaze-link's messages) first, then stencil selection
and tier-up, then v1's runtime. Line numbers are as of 2026-10-30, after the edits. Nothing
here was built or run.

v1's runtime (`blaze_live::Live` in `lib.rs`, `apply.rs`, `linux.rs`, `macos.rs`,
`windows.rs`, their `stop.rs`, the sampler `sample.rs`, and `format.rs`'s `Patch`, `Process`,
`Sample`, `Profile` and `Tiers`) has no v2 counterpart. Nothing in the v2 tree writes a
`format::Patch` (only blaze-live's own tests and `fuzz/targets/live_patch.rs` do), nothing writes
the sampler's `sample` request or reads its profile, nothing writes the `tiers` file
`blaze_live::tiers` reads, and no v2 daemon reads the `edit\t` lines `blaze_live::edit` sends
(the v2 daemon speaks CBOR). Its callers are outside these paths: `demo/life`,
`demo/lifebench`, `harness/live-types`, `fuzz/targets/live_patch.rs` (with the `fuzz` feature
in `fuzz/Cargo.toml`), `docs/usage.md`, `docs/live.md`, `docs/tiers.md`,
`site/content/usage.flecs` and `crates/blaze-live/README.md`. Its deletion is queued to the
coordinator; its fallbacks are listed below as dead rather than fixed.

| file:line | kind | what it does | what it becomes |
|---|---|---|---|
| `crates/blaze-live/src/slots.rs:340` `Answers::write` | error | an answer the linker waits for that could not be written was an `eprintln!`, and the program carried on as if it had answered | **done**: `write` returns the `io::ErrorKind`; the outcome comes back as `Outcome::Unanswered { outcome, path, error }` from `read_message`, `unread` and `guard`. Now an answer frame (`ask::ANSWER`): one the channel can't take is `Outcome::Unanswered { outcome, why }` (2026-10-31, queue 01110 phase 2) |
| `crates/blaze-live/src/slots/linux.rs:515` watcher | error | a removed directory was re-watched every second forever, and a failed `poll` or `read` of the inotify was treated as "nothing came" | **done**: `IN_IGNORED` stops the watch with `Unwatched::Gone`; a `poll`/`read` failure other than `EINTR`/`EAGAIN` with `Unwatched::Failed { call, code }`; `Live::poll` says it once as `Outcome::Unwatched`. Since deleted with the message files and the watcher (2026-10-31, queue 01110 phase 2): patches come over blaze-inproc's channel. A failed ask cuts the channel, said once as `Outcome::Disconnected` |
| `crates/blaze-live/src/slots/macos.rs:482` watcher | error | the same: `NOTE_DELETE`/`NOTE_RENAME` re-registered the directory every second forever; a failed `kevent` was treated as a timeout | **done**: `Unwatched::Gone`, or `Unwatched::Failed` for a `kevent` failure other than `EINTR`. Since deleted with the message files and the watcher (2026-10-31, queue 01110 phase 2): patches come over blaze-inproc's channel. |
| `crates/blaze-live/src/slots/windows.rs:198` watcher | error | the same: a failed `FindNextChangeNotification` slept and re-watched forever; `WAIT_FAILED` was treated as a timeout | **done**: either stops the watch with `Unwatched::Failed`. Since deleted with the message files and the watcher (2026-10-31, queue 01110 phase 2): patches come over blaze-inproc's channel. |
| `crates/blaze-live/src/slots.rs:310` `Identity::of` | error | a message file whose time written could not be read was identified without it (`modified().ok()`), so a rewritten file of the same length and node would be taken as the one already taken and silently skipped | **done**: the error declines the message (`unread`). Since deleted with the message files and the watcher (2026-10-31, queue 01110 phase 2): patches come over blaze-inproc's channel. |
| `crates/blaze-live/src/slots.rs:417` sampler start | dead | on macOS the v2 runtime started v1's sampler thread, which polls a `sample` file nothing in v2 writes | **done**: not started; `slots/macos.rs` `Os::image`, its one use, removed |
| `crates/blaze-live/src/slots.rs:512` | rule | no message file for the generation: nothing came yet | gone with the message files; now `NO_PATCH`, the daemon has none newer |
| `crates/blaze-live/src/slots.rs:526` | rule | the same file at the same generation, read already: one message is taken once | gone with the message files; now an announcement and generation looked at already are not asked again |
| `crates/blaze-live/src/slots.rs:538` | rule | a capacity hint; the read is bounded by `take(meta.len())` either way | gone with the message files; a message's length is the daemon's piece header, refused past 1 GiB |
| `crates/blaze-live/src/slots/linux.rs:578` | rule | an inotify name past the read: the kernel hands whole events only | gone with the watcher |
| `crates/blaze-live/src/slots/macos.rs:160` | rule | the protection of an empty patch code region, which no code write ever uses (every write is checked inside the region) | stays |
| `crates/blaze-live/src/slots/switch.rs:944` `ready` | rule | no spare table: unreachable while `POOL` > 1, and `standby` expects one | stays |
| `crates/blaze-live/src/slots/switch.rs:1104` `guard` | rule | a panic not in a generation's code is resumed as it was, not rolled back | stays; the one answer lost is that of a rollback that itself failed, whose panic is resumed |
| `crates/blaze-live/src/slots/column.rs:4` | rule | "fallback to the heap" names what columns never do: a column past its rows errors | stays |
| `crates/blaze-stencil/src/patch/select.rs:5` | rule | the base shape (every operand from its slot) is a complete selection in the same backend; accumulator, fused and confined shapes are taken only where the library has them | stays |
| `crates/blaze-stencil/src/shape.rs:1281` `kept` | rule | the generator keeps every base shape, so the base selection always exists | stays |
| `crates/blaze-stencil/src/patch/emit.rs:246` `missing` | rule | a stencil the library lacks is an error naming the instruction and shape, never a compile by Cranelift; `hir-ty-codegen/v2/backend.rs` `stenciled` passes it up as the unit's error | stays |
| `crates/blaze-stencil/src/patch/emit.rs:260` | rule | the size of a missing stencil counts 0 in `Buf::reserve`'s hint; emission then refuses it (`missing`) | stays |
| `crates/blaze-stencil/src/arch/x64/inst.rs:696-733` `bound` | rule | the byte bound of glue whose ABI walk fails is short; it is a `Vec::reserve` hint, and emitting that glue fails with the same error | stays |
| `crates/blaze-stencil/src/tier/runner.rs:187` `stop` | error | a tiering thread's panic was dropped at its join (`let _ =`) | **done**: recorded in `Closed::stopped` |
| `crates/blaze-stencil/src/tier/runner.rs:222` `read` | error | a panic reading the cells killed the reading thread; tiering stopped and `Runner::stopped` said nothing until the close | **done**: caught, stops the tiering with the reason at once |
| `crates/blaze-stencil/src/tier/runner.rs:245` recompile failure | rule | a recompile that fails keeps the function at its tier and is a `Failure` in `Closed::failures`, named with the reason; tier-up is the program running faster, not a build step, and whether a failure fails the run is blaze-lazy's | stays; `Closed::stopped` and `Failure::why` are strings, typed later |
| `crates/blaze-stencil/src/tier/instrument.rs:80` | rule | an entry block with no instruction has no source location to copy: the default location | stays |
| `crates/blaze-stencil/src/tier/runner.rs:84` | rule | a poisoned mutex taken as it is | stays |
| `crates/blaze-stencil/gen/*.rs` | rule | the build script's `unwrap_or_else(panic!)`: each failure stops the build naming the stencil | stays |
| `crates/blaze-live/src/linux.rs:615`, `windows.rs:518` | dead | v1: a patch that does not decode, or a file that can't be read, is dropped silently | deleted with v1 (2026-10-31, queue 01110) |
| `crates/blaze-live/src/macos.rs:562` | dead | v1: a patch that does not decode is an `eprintln!` | deleted with v1 (2026-10-31, queue 01110) |
| `crates/blaze-live/src/linux.rs:565`, `macos.rs:509` | dead | v1: a wake socket that can't be bound falls back to polling every 30 ms | deleted with v1 (2026-10-31, queue 01110) |
| `crates/blaze-live/src/linux.rs:592`, `windows.rs:480` | dead | v1: an executable linked again that can't be read is ignored | deleted with v1 (2026-10-31, queue 01110) |
| `crates/blaze-live/src/linux.rs:168`, `macos.rs:206`, `windows.rs:156` `take` | dead | v1: a patch with an older sequence is dropped with no outcome | deleted with v1 (2026-10-31, queue 01110) |
| `crates/blaze-live/src/format.rs:260` | dead | v1: a patch with no count of statics' first values is read as having none | deleted with v1 (2026-10-31, queue 01110) |
| `crates/blaze-live/src/sample.rs:187`, `:200` | dead | v1's sampler: an unreadable or malformed request is "not asked"; a profile that can't be written is an `eprintln!` | deleted with v1 (2026-10-31, queue 01110) |
| `crates/blaze-live/src/lib.rs:246` `tiers` | dead | an unreadable or malformed `tiers` file is `None`, the same as "not tiering"; nothing in v2 writes the file | deleted with v1 (2026-10-31, queue 01110) |
| `crates/blaze-live/src/lib.rs:257` `edit` | dead | v1's daemon protocol; no v2 daemon reads it | deleted with `src/socket.rs` (2026-10-31, queue 01170): demo/life's editor commits its text through blaze-api (`Request::Commit` of an `Op::Write`, then `Request::Build`) |
