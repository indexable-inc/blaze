---
status: plan
summary: lane integrate-engine-rustc's speed of light: what a build script's probe of `rustc` costs under blaze's stand-in (crates/blaze-engine/src/as_rustc.rs), the bound, and what stands between the two.
date: 2026-10-30
---
# integrate-engine-rustc: speed of light

A build script that probes `rustc` runs blaze's executable under the name `rustc` (the daemon's
`tools/rustc`, `serve/unit.rs` `stand_ins`). Two kinds of run:

| run | bound | where the stand-in is |
|---|---|---|
| a question (`-vV`, `--version`, `--print cfg`) | one process start, about 1 ms: the answer is a table lookup (`blaze_cargo::answers`, `blaze_units::stdlib::target`) | at the bound: nothing is loaded |
| a probe (autocfg's `probe_type`, anyhow's backtrace probe, rustix's) | the check of one small crate against a std the daemon has already loaded and lowered: the probe's own items only, well under 10 ms | each probe loads `library/` (std's units through `blaze_units::query::Plan`), lowers std into a fresh front end and checks the probe against it: a cold front end per probe, seconds |

The gap is std's front end, rebuilt by every probe process. autocfg alone probes a dozen times
per build script, so a crate graph with many autocfg users pays it dozens of times.

## What closes it

The daemon has std's lowered front end for the target already. Reaching it from the probe
needs a way out of the sandbox the script runs in: the sandbox refuses the machine's Unix
sockets, so the stand-in can't ask the daemon over its socket. Two designs reach the bound:

- the daemon gives each script a socket of its own inside the sandbox, over which the stand-in
  asks for a probe's verdict in the agent API's CBOR schema (docs/roadmap.md, the agent API);
  this needs the sandbox (`blaze-cargo/src/sandbox`), the daemon's serve loop and the API
  schema, none of them this lane's;
- the stand-in keeps a store of std's lowered front end per (library commit, target, cfgs) and
  maps it in. This needs the front end's database to be read back from the store, which it is
  not yet.

Until then each probe is a cold check, correct and slow. The probe's verdict does not depend on
how it is reached, so either change keeps `as_rustc.rs`'s tests as they are.

## Known divergence (not speed)

The stand-in's load runs no build script. std's (`std/build.rs`: `restricted_std`,
`backtrace_in_libstd`, `STD_ENV_ARCH`) is ported (`std_script` in `as_rustc/probe.rs`) and
keyed to the script's SHA-256, so a toolchain bump that changes it stops every probe with an
error, and `std_script_is_the_ported_one` fails, until the port follows. compiler-builtins' and
libc's cfgs are not applied: they change only those crates' insides, never what a probe can
name, so no verdict depends on them. The socket design above replaces the port with the
daemon's own run of std's script.
