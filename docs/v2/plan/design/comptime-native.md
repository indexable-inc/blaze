---
status: plan
summary: The grants and typed protocol every compile-time run is answered through (crates/blaze-roots, crates/blaze-comptime) and the native run's C-name system layer (crates/blaze-comptime-sys), built; the path today is wasm64 under in-process Wasmtime, so the native run is built, not wired. Left to build - the wasm hosts answering through the same host machine.
date: 2026-11-20
---
# Compile-time code: grants and protocol

Today blaze compiles proc-macro crates and pure-Rust build scripts itself to wasm64 (memory64)
modules and runs them in-process under Wasmtime, sandboxed (`crates/blaze-proc-macro/src/wasm64_runtime.rs`).
Native tools and C-linking build scripts are the remaining gap and go to the hypervisor micro-VM
sandbox. This doc holds what every route shares: what a run may reach, how it asks, and what keys
its record.

Paths below are under the tree root; `pm/` is `crates/blaze-proc-macro/src`.

## Built

- `crates/blaze-roots`: `Roots` (a digest-keyed tree, a package directory with globs, one output
  directory), `RelPath` (checked UTF-8, cannot escape its root), `Mounts`, and `ReadList` with
  its one check, `ReadList::under(&Roots) -> Result<Checked, Refused>`. Shared with the C
  compiler's driver (D174) and `cx.hermetic` (D177).
- `crates/blaze-comptime`: `Grants` (`grant.rs`), the typed `Call` and `Answer` (`call.rs`), the
  capped CBOR wire (`wire.rs`), the host's and the guest's sans-IO machines (`host.rs`,
  `guest.rs`), and the equality harness's `first_difference` (`equal.rs`). `PROTOCOL` is 2.
- `crates/blaze-comptime-sys`: the native run's system layer, a table of the C names std reaches
  (`table.rs`), each answered by the process itself, by the layer's own state, by one protocol
  call, or refused with the real errno; a name not in the table is refused when the record is
  bound, never bound to the real C library. Its core (`sys.rs`) is a sans-IO machine over the
  guest machine.
- The native run itself (`crates/blaze-launcher/src/comptime.rs`, `pm/native_script.rs`,
  `pm/native_bridge.rs`) and the wasm-against-native adapter (`pm/comptime_equal.rs`) are built,
  not wired: only their tests call them.

## The grants

One value (`grant.rs`) holds everything a run may reach: `roots` and `mounts`
(blaze-roots; read-only roots and at most one output), `env` (sorted, each `Set(value)` or
`Unset`; a name not there is unset to the run and asking it is a refusal), `args`, `spawns`
(`None`, or `CMode`: the build-script routing above, never the machine's programs), `output`
caps, `fuel`, `deadline`, `seed`, `clock`, the read list's `limits`. `Grants::proc_macro` (the
derive: no roots; `sol!`: the package root, the working directory its mount, so relative includes
resolve) refuses an output root; `Grants::build_script` (the package tree and `links` views,
`OUT_DIR` as the output root, cargo's variables, `build-script-build`, `CMode`) requires one.
Fuel (decided 2026-11-20): retired instructions, read by the driver as the kernel counts them
(macOS `proc_pid_rusage` `ri_instructions`, as blaze-meter reads it; Linux `perf_event_open`
where the system permits it), else CPU nanoseconds; the count of calls and the wall deadline
apply as well (`Event::Retired`, `Fuel { instructions, cpu_ns, calls }`). Fuel is never in a key,
so its inexactness is harmless: it decides only whether a run is stopped, and a stopped run
leaves no record. `RLIMIT_CPU` one second above is the kernel's own wall. The seed is BLAKE3's derivation
of the call's key; the random stream is the keyed extendable output; the clock is fixed at 0, as
the wasm host answers today.

**Refusals.** A call outside the grants is answered with the error the real system gives a process
that lacks the thing (`ENOENT` for a path, `ENOTSUP` for a thread or a socket, `EROFS` for a
write outside the output root, unset for a variable), so a crate that probes behaves as it does
today, and a `Finding` names the path or name and the `Grant` that would allow it (`Mount`,
`Glob { root }`, `Write`, `Env`, `Spawn`, `Thread`, `Network`, or `Unaddressable`). A refusal is
never an end: a macro's unknown variable is answered unset with a finding and goes on the facts
as asked-and-absent (so granting it later reruns the call); a script's write into its package is
`EROFS` with a finding. A name the system layer refuses outright (`dlopen`, `fork`, `chmod`)
crosses as `Call::Denied { name }` (wire tag 23, `Grant::Never`): a
finding, and the real errno to the code. A directory seen by `stat`, or opened where a file was
asked, is `Seen::Dir` (only its being a directory is an input); a listed one is `Seen::Listing`.

**What keys the record.** The call's key (module or code identity, export, inputs) plus the
checked read list's digest and the facts' digest (each variable answered with its value's digest
or unset, each command routed with its request and the digest of its answer). Randomness and the
clock need no entry: both are functions of the key.

## Left to build

The wasm hosts (`pm/sandbox.rs` for a macro, `pm/script_sandbox.rs` for a script, and the wasm64
host) answer their imports from their own code and build their own read lists. The design: each
import becomes a `Call` served by blaze-comptime's host machine, so every route has one grants
model, one refusal rule and one read list. Oracle: every expansion and script output of tempo
byte-equal before and after, and the read lists equal under the adapter. It deletes the
sandboxes' own answering code and read-list builders.

## Decided (owner, 2026-11-20)

- Fuel: retired instructions where the kernel gives them, else CPU nanoseconds, with the call
  count and the wall deadline; never in a key.
- An unknown variable: unset, a finding, and an asked-and-absent fact. A script's write into its
  package: `EROFS` and a finding.
- A native run's expansion key names the architecture until the equality harness shows tempo's
  expansions equal across the two architectures.
