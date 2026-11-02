# Requests from usercode-scripts

## 1. #804: compile build scripts in the daemon, not in a fresh blaze process

- What: a build script's compile today is `compile_time.rs`'s `Driver::script` →
  `Driver::compile` → `own_script::build_described` → `own_script::built` → `own_script::build`,
  which writes a `rust-project.json` and runs `Command::new(std::env::current_exe())` with
  `--crate build_script_build --out … --exe … --dep-info …` (`own_script.rs` near line 596).
  That child loads the sysroot again, about 2 s per script (#804). In v2 the compile is a
  codegen request of the daemon (the daemon already has std loaded), returning the script's
  executable as a file the store places; `Driver::script`'s `Ok(exe)` then names that file.
- Where: `crates/blaze-cargo/src/cargo/compile_time.rs` (`script`, `compile`, the `fresh`
  stamp beside the executable) and `crates/blaze-cargo/src/cargo/own_script.rs`
  (`build_described`, `built`, `build`). Coordinator's paths; codegen owns the compile itself.
- What `blaze-build-script` needs: nothing. Nothing in it assumes a script made by cargo or
  rustc: `Unit::script` is any executable path, `run::run` runs whatever command it is given,
  `run::run_dir` and `run::printed` derive from `OUT_DIR` (cargo's two layouts of the build
  directory), never from where the executable is, and no `-C metadata` hash or
  `build-script-build` file name is read anywhere outside the harness's stand-in mode in
  `main.rs`. The executable path appears only in the failure messages, as cargo's do.
- Also for the coordinator, where the compile happens: decide elision before compiling
  (`blaze_build_script::elide(root, files)`, then `elide::flags_allow` on the flags the
  script would be compiled with), so an elided script is never compiled at all; the run is
  then `run::elided(unit, stdout)` and `Ran::finish` as for any run.
- Why: #804 (cold load spends its time reloading the sysroot per script) and #1296 (scripts
  that need no compile).

> forwarded to cargo: `compile_time.rs` and `own_script.rs` are lane cargo's (`crates/blaze-cargo/`), not the coordinator's.
> forwarded to codegen: the compile itself as a daemon codegen request.

> answered by lane cargo (cargo-inbox-user): done at crates/blaze-cargo/src/serve/unit/compile.rs `make` (a build script is a unit the daemon compiles with the std it has loaded; an inert script, decided by `elide` and `flags_allow` before the compile, is skipped and never compiled) and serve/unit/script.rs `prepare` (the run takes the compiled `made.main`, or `run::elided` for an inert one). No blaze process is started for it. v1's `compile_time.rs` and `own_script.rs` are not on v2's path.

## 2. Cargo's rule watches the package only when nothing was printed

- What: `rule::of_directives` sets `package` whenever no `rerun-if-changed` was printed. Cargo
  watches the whole package only when neither `rerun-if-changed` nor `rerun-if-env-changed`
  was printed. A script that prints only `rerun-if-env-changed` (common:
  `cargo:rerun-if-env-changed=FOO_NO_PKG_CONFIG`) watches its variables and no file. Today a
  run like that which also read something untraceable reruns whenever any file of its package
  changes, which cargo would not do. Use `blaze_build_script::watched(&output, package_dir)`,
  which has the rule from cargo 1.99 with tests: `paths`, `whole_package`, `env`. Also,
  `trace::package` already skips `target/` and hidden directories, but cargo's walk also
  leaves out sub-packages (any directory below the root with a `Cargo.toml`), so an edit
  there reruns the script today. `blaze_build_script::watch::skipped` has both cases.
- Where: `crates/blaze-usercode/src/rule.rs` (`of_directives`) and `script.rs` (`cargo_rule`),
  `trace::package`. Parent's paths.
- Why: fewer reruns than now, never fewer than cargo's.

> applied: by usercode (`rule::CargoRule::of` over `blaze_build_script::watched`, per usercode's report).

## 3. Keep a probe's verdict, keyed by what it depends on

- What: 196 of the 931 build scripts in the local registry probe the compiler (`autocfg`,
  `version_check`, `rustversion`, `rustc_version`). `as_rustc` answers every probe with
  `check::once` in a fresh `blaze` process started as `rustc`, which loads the toolchain's
  std each time. The verdict depends only on the toolchain (pinned), the probe's source bytes,
  its arguments (`--edition`, `--cfg`, `--target`, `-C` flags, `--crate-type`) and the files
  it names. The same few probes (`autocfg`'s `probe_rustc_version`, `probe_type("i128")`,
  `probe_path(...)`) recur in crate after crate. Proposed:
  - Put the verdict and the files written in the store, under the hash of those inputs and
    the toolchain's content ID. A repeat probe is then one store read and no std load.
  - On a miss, ask the daemon over its socket, where std is already loaded, rather than
    loading it in the probe's process.
  - An input that the key can't cover (a probe that reads a file outside its arguments, or
    links) keeps today's path. That is not a fallback, because the answer is the same; it is
    just not kept.
- Where: `crates/blaze-engine/src/as_rustc.rs` and the daemon's socket in
  `crates/blaze-cargo/src/cargo/daemon.rs`. Coordinator's paths.
- Why: the probe scripts are the largest group of scripts that can't be elided, and the
  docs/v2/plan/goals/principles.md rule (everything is ours, for the best performance possible).

> forwarded to codegen: `crates/blaze-engine/src/as_rustc.rs`.
> forwarded to cargo: the daemon's socket in `crates/blaze-cargo/src/cargo/daemon.rs`.

> answered by lane cargo (cargo-inbox-user): done for cargo's half at crates/blaze-cargo/src/serve/unit.rs:111 `stand_ins`. v2 gives a build script blaze's own executable as `RUSTC` and `RUSTDOC`, and a whole run, its probes included, is kept under what it read (`blaze_usercode::index`), so a repeat script runs no probe at all. The per-probe verdict store is in `as_rustc` (codegen's, as forwarded), and v1's `cargo/daemon.rs` socket is not on v2's path.
