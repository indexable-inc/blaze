---
status: plan
summary: Where unsafe code may be in blaze-live, blaze-usercode, blaze-build-script and the sandbox drivers, what each untrusted message is checked against, and the model, reference and fuzz tests that hold them (lane speed-unsafe-back-run, 2026-10-30).
date: 2026-10-30
---
# Unsafe code in the runtime and the sandbox

Lane `speed-unsafe-back-run` (a child of `speed-unsafe-back`) applied items 1-6 of the
speed-unsafe brief to `crates/blaze-live`, `crates/blaze-usercode`, `crates/blaze-build-script`
and `crates/blaze-cargo/src/sandbox/`. Nothing here was compiled; the coordinator builds it.

## Where unsafe is allowed

| Crate or module | Root | Allowed in |
|---|---|---|
| blaze-build-script | `#![forbid(unsafe_code)]` (lib and main) | nowhere: no block |
| blaze-usercode | `#![forbid(unsafe_code)]` | nowhere in the crate; the fork server is text (`test/fork_server*.rs`, `include_str!`) compiled into each test harness |
| blaze-live | `#![deny(unsafe_code)]` | the v1 platforms (`linux`, `macos`, `windows`), their `stop` modules, `sample`, and `slots`' `linux`, `macos`, `windows`, `unix`, `image`, `exec`: each `#![allow(unsafe_code, reason = ..)]`; `apply` forbids it |
| sandbox `linux.rs`, `macos.rs`, `v2.rs` | `#![deny(unsafe_code)]` | `mod raw` (each system call behind a safe function) and the functions that fork |
| sandbox `windows.rs` (with `reads`, `serve`) | allowed whole on its `mod` line | the Win32 driver |
| sandbox `signal.rs` | allowed on its `mod` line | `sigaction` and the handler's `kill` |

The `mod` lines in `sandbox.rs`, and the removal of the blanket allow on `pub mod sandbox` in
`lib.rs`, are `cargo`'s (`v2/requests/cargo.md`, "speed-unsafe-back-run").

std has no safe version of what these modules do: it maps no executable memory, stops no
thread, installs no signal handler, makes no namespace, AppContainer or seatbelt profile, and
gives no view of the image's words at the addresses the linker placed them. The two lock-free
structures have a safe version, a `Mutex` around a map, which would cost one shared lock per
call or per read check.

## Untrusted input

- **v1 patches** (`format::Patch::decode`, from the daemon's process): every count is capped by
  the bytes left before anything is reserved, and bytes past the end are now refused. The
  checks in `apply` (`in_image`, `Overwrites::plan`, `claim`, `slide_at`, `frame_run`, `fdes`,
  `functions`) run before anything is mapped or written. `fuzz_checks` (feature `fuzz`) runs
  them in the platforms' order for the fuzz target `live_patch`.
- **Jump-table messages** (`patch::Message::view`, from the linker): read in place by safe
  code, every length checked against the message in one pass, with no trailing bytes allowed.
  `Switch::apply` checks the id, base and generation, every code span against the patch code
  region, and every data span against the code region, the cell, the loader's table, lasting
  memory and now the image (`Memory::in_image`, before any write: the platforms' `data` used to
  check a span at a time as it wrote). It also checks slot indices against the table, that no
  slot or rebase is given twice, that every rebase is inside a write, and the unwind entries and
  `RUNTIME_FUNCTION`s. Test: `data_outside_the_image_is_refused_before_any_write`. Fuzz target:
  `live_message`.
- **Fork-server frames** (`test::wire`): a length past `LONGEST` is refused before its body;
  kinds are range-checked; every read names its field ("a frame ends early, in its test
  index"); nothing panics. Fuzz target `usercode_wire` (it exists).
- **Change records** on Windows (`reads.rs` `told`, from the kernel): read through `.get`,
  with a record that runs past what was read letting go of every check of the volume.

## Lock-free structures and their model tests

- `slots`: the cell switch and lazy `publish` racing `Switch::ready`'s copy. The tables are
  std atomics at the linker's addresses, so `slots/model.rs` takes the same steps with the same
  orderings on blaze-check's atomics.
- `sandbox/windows/reads.rs`: the read checks' `Table` and its pending-then-publish state word.
  Under `cfg(blaze_check)` the module's own atomics are blaze-check's, so three model tests run
  the real `claim`, `find`, `pend`, `publish` and `forget`.
