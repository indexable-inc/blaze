---
status: plan
date: 2026-10-30
summary: no-fallback sites in blaze-cargo's CLI, socket, toolchain, answers and stub (lane no-fallback-link-cargo-cli), each an error, a rule or dead, and what it becomes.
---
# no-fallback: blaze-cargo's CLI, socket, toolchain and answers

Ranked: the daemon's connection (every build goes through it) first, then the toolchain the
link reads, then the command line, then the rest. Line numbers are as of 2026-10-30 before the
edits.

| file:line | kind | what it does | what it becomes |
|---|---|---|---|
| `crates/blaze-cargo/src/cli/daemon.rs:63` | error | `Err(_) if start == Never => Ok(None)`: any failed connect (permission denied, a peer refused, a path too long) is "no daemon is running" | only `NotFound`/`ConnectionRefused` (nothing listens) is no daemon; anything else is `DaemonError::Connect { socket, error }` |
| `crates/blaze-cargo/src/cli/daemon.rs:61-87` | error | any failed connect starts a second `blaze serve` | a daemon is started only when nothing listens; any other failure is `DaemonError::Connect` |
| `crates/blaze-cargo/src/cli/daemon.rs:89-97` | error | the wait for a started daemon retries every error until `PATIENCE`, and the error is a string | only "nothing listens yet" is retried; the end is `DaemonError::NotListening { socket, log, tries, error }`, naming the socket and how many connects were tried |
| `crates/blaze-cargo/src/cli/daemon.rs:196` | error | `answers`: any connect error is "the daemon is gone" while `blaze stop` waits | `Result<bool, DaemonError>`: gone only when nothing listens |
| `crates/blaze-cargo/src/cli/place.rs:264` | error | `--target` given twice: the first is built and the rest are dropped silently | `place::Refused::Targets`: the daemon builds one target a request, so two are refused naming them |
| `crates/blaze-cargo/src/toolchain.rs:186` | error | rustup's toolchain directory for this host not found: the first of another host's is chosen | `.or(found.first())` removed; no copy for this host is no rustup copy, and blaze's own copy is looked for (the documented next place) |
| `crates/blaze-cargo/src/toolchain/library.rs:142` | error | `current_exe` failing skips the program's `share/blaze/library` silently | `look` says why: the error names the failed `current_exe` |
| `crates/blaze-cargo/src/toolchain/library.rs:177` | error | `BLAZE_LIBRARY_FROM` that is not UTF-8 is treated as unset, and the checkout or the release is used | an error naming the variable |
| `crates/blaze-cargo/src/toolchain/library.rs:287-299` | error | the archive's download gives up after `TRIES` with only the last error | the last error carries `GaveUp { file, tries }` (typed `anyhow` context) |
| `crates/blaze-cargo/src/toolchain/library.rs:97` | dead | `why_not`: no caller; it gave `""` for a library that was found | deleted |
| `crates/blaze-cargo/src/toolchain/own.rs:329` | error | `BLAZE_RUST_DIST` that is not UTF-8 is treated as unset, and the Rust project's site is used | an error naming the variable |
| `crates/blaze-cargo/src/toolchain/own.rs:346` | error | a file of `BLAZE_RUST_DIST`'s directory that can't be opened (any error) is "not there" | the open's own error, with the file |
| `crates/blaze-cargo/src/toolchain/own.rs:487-512` | error | a file's download gives up after `TRIES` with only the last error | the last error carries `GaveUp { file, tries }` |
| `crates/blaze-cargo/src/toolchain/own.rs:252-261` | error | an older toolchain that can't be removed is printed and the command goes on | `fetch` fails naming the directory and why |
| `crates/blaze-cargo/src/toolchain/own.rs:195` | dead (left) | `own::fetch` has no caller since the v2 checkpoint (`9886895b2d`): `toolchain::found` never downloads, and `serve/unit/codegen.rs:206,720` says "blaze's toolchain is not installed" | not deleted: it is the only way blaze gets its release on a machine with no rustup; wiring it into `sysroot` (or deleting it with docs/self-contained.md) is for the parent, `serve/` is not this lane's |
| `crates/blaze-cargo/src/stub.rs:49-608` | dead | v1's `RUSTC_WORKSPACE_WRAPPER`: runs rustc on a stand-in for each crate (no-rustc); nothing calls `stub::command` | deleted; its one live item, `VALUED` (rustc's flags with a value), moves to `answers.rs` |
| `crates/blaze-cargo/src/socket.rs:150` | error | `incoming().flatten()` drops every failed `accept` (a full fd table spins on it unseen) | `incoming` yields `io::Result`; the caller (only this file's tests today) sees the error |
| `crates/blaze-cargo/src/socket.rs:59` | rule | `let _ = unlock()` in `Drop` | a drop can't fail; closing the file releases the lock too |
| `crates/blaze-cargo/src/socket.rs:111` | rule | an address too long for the socket's first place: bound through a short `/tmp` link | the same socket at the same path, not another path |
| `crates/blaze-cargo/src/socket.rs:126` | rule | `let _ = remove_dir_all(short)` | cleanup of an empty private directory; nothing reads it |
| `crates/blaze-cargo/src/socket.rs:155` | rule | a connection past `MOST_CONNECTIONS` closed and said | the refusal itself, not another path |
| `crates/blaze-cargo/src/cli.rs:103` | rule | `auto_color` `unwrap_or(false)`: an unknown `CARGO_TERM_COLOR` uncolors clap's own error | clap's: its error is said before the variable is read; the command refuses the variable after |
| `crates/blaze-cargo/src/cli.rs:255`, `cli/inspect.rs:173` | rule | an `io::Error` writing stdout/stderr is exit 101 with nothing said | the stream that would say it is the one that failed |
| `crates/blaze-cargo/src/cli/args.rs:309` | rule | `command.unwrap_or(Build)` | `None` only in `Invocation::default()`; `parse` gives help for no command |
| `crates/blaze-cargo/src/cli/args.rs:324-330` | rule | the profile when none is given | cargo's: `release` under `--release`, else the command's (`dev`, `bench` for bench) |
| `crates/blaze-cargo/src/cli/args.rs:397,427,431` | rule | `unwrap_or_default`/`.ok()` on flag values | each value is checked by `check_value` before `Flags::read` |
| `crates/blaze-cargo/src/cli/place.rs:204-212` | rule | no width: rustc's own | cargo's `NoTty` |
| `crates/blaze-cargo/src/cli/place.rs:87,92` | rule | the checkout when there is no `.git`: the highest directory with a `Cargo.toml` | the module's documented rule; the manifest's own directory always has one |
| `crates/blaze-cargo/src/cli/place.rs:232` | rule | `-v` count saturated at 255 | cargo's verbosity tops out at 2 |
| `crates/blaze-cargo/src/cli/json.rs:645` | rule | a span's file that can't be read renders without its lines | rustc's emitter does the same |
| `crates/blaze-cargo/src/cli/test.rs:98,253` | rule | `executable.unwrap_or(path)` | `executables` keeps only artifacts with an executable |
| `crates/blaze-cargo/src/cli/say.rs:451` | rule | `code().unwrap_or(1)` | no code only for a signal, handled above on Unix |
| `crates/blaze-cargo/src/cli/print.rs:90` | rule | `--message-format` default `human` | cargo's |
| `crates/blaze-cargo/src/answers/own.rs:379,397,402` | rule | `--target` absent: the host; `--crate-name` absent: `rust_out` | rustc's |
| `crates/blaze-cargo/src/answers/own.rs` (`.ok()?` throughout) | rule | a question not understood whole is `None` | `answers::ours_said` turns `None` into an error naming the question; no rustc runs. The reason `target::*` gave is dropped (later: carry it) |
| `crates/blaze-cargo/src/toolchain.rs:92-101` | rule | rustup's copy of the release, else blaze's own | the documented order of places; neither is another toolchain |
| `crates/blaze-cargo/src/toolchain.rs:171` | rule | no `toolchains` directory under rustup's home: no rustup copy | a place looked in, then the next |
| `crates/blaze-cargo/src/toolchain/library.rs:78-79,133-136` | rule | a candidate directory without a readable `DIGEST`/`UPSTREAM`, or of another digest, is passed over | the documented order of places; none found is an error naming the digest |
| `crates/blaze-cargo/src/toolchain/library.rs:178-183` | rule | `BLAZE_LIBRARY_FROM`, else the checkout's, else the release's | the documented order; each is checked against the digest |
| `crates/blaze-cargo/src/toolchain/own.rs:472-475` | rule | a partial download longer than the file published starts over | the sha256 decides; nothing else is used |
| `crates/blaze-cargo/src/toolchain/own.rs:333` | rule | `BLAZE_RUST_DIST` unset: `static.rust-lang.org` | where the Rust project publishes it |
| `crates/blaze-cargo/build.rs:30,66` | rule | `CC` unset: `cc` | cc-rs's and cargo's rule |
| `crates/blaze-cargo/build.rs:67` | rule | `MACOSX_DEPLOYMENT_TARGET` unset: `11.0` | rustc's default for `aarch64-apple-darwin` |
| `crates/blaze-cargo/src/release.rs:18`, `toolchain.rs:103,113` | rule | `OnceLock::set` a second time ignored | set once by `main` (or the same value found again) |
| `crates/blaze-cargo/src/raw/process.rs:97-98,109` | rule | a negative `timeval` or tick rate is 0 / none | the kernel gives none |
| `crates/blaze-cargo/src/directives.rs:273` | rule | `-L` with no directory: `""` | `""` is no allowed place, so the line is refused |
| `crates/blaze-cargo/src/unstable.rs:329` | rule | a `-Z` flag not in the table is refused | the refusal itself |
| `crates/blaze-cargo/src/lib.rs:61` | rule | a poisoned mutex is taken anyway | the shared brief's own exception |
