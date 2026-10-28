# No-op `blaze build` asks no rustc (lane nopin, #837)

ix VM `nopin-r4x`, x86_64, 64 vCPU. Release builds of ra-jit with `--features llvm` and the
proc-macro server beside it (`.github/scripts/build_release.py --server-beside`): before is
ee8870419e (#837's merge base), after is #837 at 57b7ac5cb4. A fresh hello-world project per
arm (written by hand, so nothing needs cargo to make it), each arm with a `BLAZE_HOME` of its
own, no `RUSTUP_*`, `CARGO*`, `RUSTC*` or `BLAZE_*` in the environment. 3 warm-up runs, then 30
rounds of `blaze build -q` alternated across the arms, each a fresh process timed whole from
Python. Execs: `strace -f -e trace=execve` over the client of one more no-op build and attached
to the running daemon and workers (`bench.py`, `bench.sh`).

| setup | arm | median ms (min, p90) | rustc/rustup/cargo execs |
|---|---|---|---|
| rustup-init, default `nightly-2026-10-27` + rust-src, `1.80.0` installed, project `rust-toolchain.toml` pins `1.80.0` | before | 18.4 (16.8, 20.4) | 2 rustc |
| | after | **7.4** (6.3, 8.1) | **0** |
| | plain `cargo build -q` | 13.3 (12.1, 14.4) | 2 (cargo through rustup) |
| no Rust: no rustup, every rustc/cargo/rustdoc on `PATH` bind-mounted to `/dev/null` (`unshare -m`) | before | 8.6 (7.4, 9.2) | 1 rustc (fails to exec) |
| | after | **8.4** (7.0, 9.0) | **0** |

`noop-rustup.json` and `noop-norust.json` have every run. Before printed, on the first build of
the pinned project, "blaze builds with its own toolchain, blaze-2026-10-27, not rustc 1.80.0";
after prints nothing. Both downloaded blaze's Rust (159 MB, once) with no Rust installed.

What is left of 7 to 8 ms is starting the programs, not a question: on this VM `ra-jit --version`
alone takes 6 ms and `blaze --version` 7 ms, timed the same way. #833's 4.7 ms was arm64.

## Build scripts against the stand-in (`probes.sh`, `probes-execs.txt`)

A cold `blaze build` with no Rust installed of a binary using anyhow, thiserror, rustversion,
num-traits (autocfg), indexmap, version_check and proc-macro-error2, at #837 plus bec1f9476a:
builds in 96 s and runs (`since 1.60 nightly {1: 2} 2`, `Some("1.101.0")`, `ctx: bad 3`).
Every one of the 11 `rustc` execs is blaze's stand-in (`.../ra-jit-rustc/rustc`: `--version`,
`-vV`, autocfg's probes from stdin, anyhow's, thiserror's and proc-macro2's `*_build_probe`
checks). No rustup, cargo or real rustc ran. Before bec1f9476a the same build tried
`rustup which cargo` 15 times (the front end's `Sysroot::tool_path`).
