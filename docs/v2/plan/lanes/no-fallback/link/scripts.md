---
status: plan
date: 2026-10-30
summary: no-fallback in blaze-build-script and blaze-engine (lane no-fallback-link-scripts) — every site where they guessed a value, dropped an error or took another path, its kind (error, rule, dead) and what it became.
---
# no-fallback: blaze-build-script and blaze-engine

Kinds as in [no-fallback.md](../../no-fallback.md): **error** becomes a typed error, **rule** is
cargo's or rustc's own behavior and stays, **dead** is deleted. Ranked by the build path first
(a build script's environment and the reading of its output, which the daemon's build takes),
then the `blaze` launcher and the `rustc` stand-in. Neither crate has a path that runs rustc,
cargo or the v1 splicer (`rg 'Command::new'` finds only the pinned release's exec and `git` in
blaze-engine's build script).

blaze-build-script's `Error` is cargo's message as a `String`, used by blaze-cargo,
blaze-usercode and the fuzz targets through `.0`. The new errors are a typed enum,
`unit::Unfilled`, which converts into `Error` at `env()`'s and `read()`'s boundary; making
`Error` an enum is left for later. blaze-engine's commands return `anyhow::Error`; the new
errors are typed enums (`pin::PinError`, `program::ChannelError`, `versions::FetchError`,
`as_rustc::Refused`) carried inside it, so a caller can downcast them.

| file:line | kind | what it does | what it becomes |
|---|---|---|---|
| `crates/blaze-build-script/src/env.rs:478` (`env`) | error | a unit with an empty `TARGET`, `HOST`, `OUT_DIR`, `RUSTC`, `RUSTDOC`, `CARGO`, script, manifest dir or path, name, version or `OPT_LEVEL` ran the script with that value empty: a missing value guessed as "" | done: `Unit::check` (`unit.rs:100`) refuses it, `Unfilled::Empty { package, field }`; test `a_value_missing_is_an_error` |
| `crates/blaze-build-script/src/env.rs:724` (`Version::of`) | error | a version missing its minor or patch number got `"0"` for it (`CARGO_PKG_VERSION_MINOR=0`), and one with letters was taken as numbers | done: semver's `MAJOR.MINOR.PATCH` or `Unfilled::Version`; same test |
| `crates/blaze-build-script/src/env.rs:266` (`cfg_of`) | error | a target cfg with an unquoted value was taken as written, a value `rustc --print cfg` never prints | done: `Unfilled::Cfg`; same test |
| `crates/blaze-build-script/src/env.rs:504` | error | an empty `sysroot_libdir` (blaze-cargo always gives one: blaze has no sysroot library dir) put an empty entry on `LD_LIBRARY_PATH`, which the loader reads as the working directory | done: an empty one adds nothing (field doc says so); test `no_sysroot_library_directory_adds_none`. Queued 01174 (cargo): make it `Option<PathBuf>` |
| `crates/blaze-build-script/src/output.rs:329` (`older_than_new_syntax`) | error | a `rust-version` that is not a version was taken to allow `cargo::` | done: `Unfilled::RustVersion`, at the first `cargo::` line, as cargo's manifest reader refuses it; test in `new_syntax_and_rust_version` |
| `crates/blaze-build-script/src/main.rs` (whole file) | dead | the driver of `harness/scripts.py`, removed with the cargo comparison harnesses; it execs a script outside any sandbox, and drops a failed record (`let _ = fs::write`, `current_exe().unwrap_or_default()`, cwd `unwrap_or_default()`) | done: deleted, with its dependencies `jobserver` and `serde_json` |
| `crates/blaze-build-script/src/run.rs` (`run`, `elided`, `Invocation`, `Ran::finish`, `Ran::write`, `Ran::logged`; `env.rs` `Env::apply`) | dead | ran a script directly, outside the sandbox, and wrote cargo's files of it: nothing but `main.rs` called them; blaze-cargo judges with `judge` and writes `files` itself (two ways of one job) | done: deleted; `run/tests.rs` judges the same runs from their bytes with `judge`, `begin`, `files` |
| `crates/blaze-build-script/src/env.rs:131` | rule | `DYLD_FALLBACK_LIBRARY_PATH`: the variable's name, cargo's `dylib_path_envvar` on macOS | stays |
| `crates/blaze-build-script/src/env.rs:516-521` | rule | with no dynamic library path inherited on macOS, `$HOME/lib:/usr/local/lib:/usr/lib` added, `$HOME/lib` only when `HOME` is set | stays: cargo's `fill_env` |
| `crates/blaze-build-script/src/env.rs:509`, `:614` | rule | no inherited library path is an empty list | stays: cargo's `unwrap_or_default` |
| `crates/blaze-build-script/src/env.rs:534` (`field`) | rule | an absent description, homepage, license, readme or `rust-version` is set empty | stays: cargo sets each, empty when absent |
| `crates/blaze-build-script/src/env.rs:126-142` (`HostOs::of`) | rule | the library path variable by host: `PATH`, `DYLD_FALLBACK_LIBRARY_PATH`, `LIBPATH`, `LIBRARY_PATH`, else `LD_LIBRARY_PATH` | stays: cargo-util's table |
| `crates/blaze-build-script/src/output.rs:414-420` (`directive`) | rule | a line that is not UTF-8 or not `cargo:` is the script's own | stays: cargo's `BuildOutput::parse` |
| `crates/blaze-build-script/src/output.rs:487-496` | rule | an unknown `cargo::KEY` | stays an error, cargo's words ("Unknown key") |
| `crates/blaze-build-script/src/output.rs:497-507` | rule | an unknown `cargo:KEY=VALUE` is metadata | stays: cargo's old syntax, where every unreserved key is metadata |
| `crates/blaze-build-script/src/output.rs:535-551` | rule | `rustc-link-arg-cdylib` without a cdylib target warns and is kept | stays: cargo's warning, word for word |
| `crates/blaze-build-script/src/output.rs:604-632` | rule | `rustc-env=RUSTC_BOOTSTRAP` warns where nightly features are allowed, else fails | stays: cargo's rule |
| `crates/blaze-build-script/src/apply.rs:363-398` (`newest`) | rule | what can't be read below a watched directory is left out of its newest time | stays: cargo's `mtime_recursive` skips unreadable entries |
| `crates/blaze-build-script/src/apply.rs:267` | rule | a watched path outside the package directory is kept whole | stays: cargo's |
| `crates/blaze-build-script/src/elide.rs:17` | rule | a script elision can't prove is compiled and run | stays: elision is a proof for a subset with the same bytes (`BLAZE_ELIDE=verify` checks), like SIMD and scalar dispatch; the run is the semantics |
| `crates/blaze-build-script/src/unit.rs:494` (`Package::descr`) | rule | a package with no `descr` is named `name vversion` | stays: cargo's name for a registry package |
| `crates/blaze-engine/src/as_rustc.rs:241` (`Probe::of`) | error | a probe with no `--crate-type` was checked as a library; rustc builds a binary (and fails one with no `main`), so a script could see a verdict rustc would not give | done: `Refused::NotALibrary { crate_type: None }`; the existing `--crate-type bin` refusal is the same variant; tests in `probes_read` |
| `crates/blaze-engine/src/as_rustc/probe.rs:400` (`library_names`) | error | a file of the standard library whose name is not UTF-8 was skipped | done: the check stops with it, as for any library file that can't be read (`ProbeFiles::fail`, a `String` today) |
| `crates/blaze-engine/src/program.rs:147` (`channel_version`) | error | a pin to a channel whose pointer could not be read again ran the version it named when last read, with a line on stderr: an older cached result | done: `ChannelError::Unread { why, last }`, naming the stale version and `blaze version-resolve` |
| `crates/blaze-engine/src/program.rs:60` (`pinned_elsewhere`) | error | a working directory that can't be read ran this blaze, whatever the workspace pins | done: `PinError::NoWorkingDirectory` |
| `crates/blaze-engine/src/pin.rs` (`root_manifest`, `pinned`) | error | a manifest that can't be read, or is not TOML, was taken to pin nothing, and this blaze ran in the pinned one's place | done: `PinError::Unread` / `PinError::NotToml`; a missing one is still none; tests in `the_root_manifest_holds_the_pin` |
| `crates/blaze-engine/src/pin.rs` (`channel_known`) | error | a channel file dated in the future had age 0, so it was fresh forever | done: an age that can't be told is not fresh, and the channel is read again; test in `a_pin_names_a_channel` |
| `crates/blaze-engine/src/versions.rs` (`resolve`) | error | a channel's version that could not be kept for the launcher was said on stderr and the command went on; no home directory kept nothing | done: `FetchError::ChannelUnkept`, `FetchError::NoHome` |
| `crates/blaze-engine/src/versions.rs` (`read_at`) | error | `BLAZE_RELEASES` set to what is not UTF-8 was ignored and releases were fetched from GitHub | done: `FetchError::ReleasesNotText` |
| `crates/blaze-engine/src/versions.rs` (`unpacked`) | error | an earlier fetch's staging directory that could not be removed was unpacked into, mixing two archives | done: `FetchError::Staging`; test in `a_release_from_a_directory_is_checked_and_unpacked` |
| `crates/blaze-engine/src/versions.rs` (`why_this_one`) | error | `blaze --version` said "no workspace is here" when the working directory could not be read | done: it says why |
| `crates/blaze-engine/src/versions.rs` (`unpacked`, after the move) | rule | the staging directory's removal after a move is not checked | stays: cleanup of a directory named by this process; no result reads it |
| `crates/blaze-engine/src/versions.rs` (`unpacked`, `Err(_) if dir.is_dir()`) | rule | a move that fails because another blaze moved the same release in first | stays: a release directory appears only whole, by `rename` |
| `crates/blaze-engine/src/versions.rs` (`pin`, `was`) | rule | an old pin that is not a version is not named in "in place of" | stays: `blaze pin` replaces it; only the message |
| `crates/blaze-engine/src/as_rustc.rs` (`answer`, `style`) | rule | no `--edition` is 2015, no `--target` the host, no `--out-dir` the working directory, `--color auto` none (stderr is cargo's pipe) | stays: rustc's defaults |
| `crates/blaze-engine/src/as_rustc/probe.rs:155` | rule | process variables that are not UTF-8 are not given to the probe's `env!` | stays: rustc's `env!` reads `env::var(..).ok()`, the same |
| `crates/blaze-engine/src/as_rustc/probe.rs:246` (`written`) | rule | a diagnostic into a library file that can't be read is shown with no snippet | stays: rustc does the same for a source it can't read |
| `crates/blaze-engine/src/program.rs:47` | rule | an exit code over 255 ends as 1 | stays: every code here is 0 or 1 |
| `crates/blaze-engine/build.rs:53-86` | rule | a build of the tree with no git says no commit in `--version` | stays: optional display; a release must give both and panics otherwise |

Left for later: blaze-build-script's `Error(String)` as a typed enum (users across three
crates); `ProbeFiles::fail` keeps a `String`; blaze-cargo's own guesses in `script_unit` (queued
01174).
