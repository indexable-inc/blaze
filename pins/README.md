---
status: current
summary: Every pinned toolchain fact blaze has, in one folder: what each file pins, what writes it and what reads it.
---

# pins/

Every fact blaze takes from the toolchain it is pinned to lives here, once (end-state.md D162).
Nothing else in the tree keeps a copy. Rust reads these files through `crates/blaze-pins`, which
parses them at compile time from `include_str!`: a missing or malformed pin fails the build and
names the file and its line. Python reads them through `harness/toolchain_pins.py`. The flake,
`tools/nix/units.nix` and CI read the files by path. No script reads a pin from inside a crate.

`crates/blaze-producers` digests every file under this folder but the `.md` ones into each
producer's id (the `pins` part of a row). So a change to any pin re-identifies everything blaze produced from it,
and `scripts/src/bin/changed.rs` and `gate_changes.rs` treat `pins/` as an input of everything.

| File | What it pins | Written by | Read by |
|---|---|---|---|
| `toolchain` | The nightly's name, `nightly-YYYY-MM-DD` | `harness/toolchain_bump.py` | `blaze_pins::TOOLCHAIN` (its `const` check holds it equal to `rust.txt`'s release); `toolchain_pins.toolchain()`; `flake.nix`; CI (`.github/actions/toolchain`, `blaze.yaml`'s `cargo` job, `.github/actions/script/key.sh`); `site/install.sh` names it by URL |
| `rust.txt` | The release: `rustc -V` and `cargo -V` text, the rust-lang/rust commit, the channel manifest's sha256 and size, and each host's archives (sha256 and size) | `harness/toolchain_pins.py '{"write": true}'` | `blaze_pins::{RUST, RUSTC_V, RUSTC_VERSION, RELEASE, COMMIT_HASH, COMMIT_DATE, CARGO_V, CARGO_VERSION, ARCHIVES, archive}` (`RUST_TXT` is the raw text); `toolchain_pins.rust()`; `harness/toolchain_bump.py` |
| `llvm.json` | The LLVM blaze links: fork, ref, commit, version, release bucket, and each target's archive name and sha256 | `.github/scripts/llvm.py '{"command": "pin", ...}'` (the `llvm` workflow) | `blaze_pins::{LLVM_JSON, LLVM_VERSION, LLVM_COMMIT}`; `vendor/llvm-sys/pin.rs` by path (a vendored crate takes no new dependency); `Cargo.toml`'s pinned native input; `flake.nix`; `.github/scripts/llvm.py`; `tools/cpus.py` |
| `clang.json` | The C toolchain that native build scripts run with (clang, libclang, llvm-ar, the shelf's programs), each host's rows and the sysroots | By hand, from the LLVM project's release of `llvm.json`'s version | `blaze_pins::{CLANG_JSON, CLANG_VERSION}` (blaze-build-script, blaze-proc-macro); `toolchain_pins.clang()` |
| `cfront.json` | blaze-cfront, the C toolchain helper of D174 (crates/blaze-cc/helper): its protocol version, its member in the blaze LLVM archive, and each target's sha256 of it | `.github/scripts/llvm.py '{"command": "pin", ...}'` (the `llvm` workflow), from builds that carry it | `scripts/src/bin/toolchain_cut.rs` (`cfront`, by path); crates/blaze-cc's caller, once wired (by `blaze_pins`, not yet: listed in the D174 report) |
| `base-tools.json` | The base tool set of D192 per platform (docs/v2/plan/design/base-tools.md): the archive's name, SHA-256 and size (PENDING until CI builds it), the programs of its `bin/` by group, and the fixed answers of its `uname` and `hostname` | By hand; CI's archive build fills `digest` and `size` | `blaze_pins::{BASE_TOOLS_JSON, BASE_TOOLS_PATH}` (crates/blaze-base-tools `pins`) |
| `answers/rustc.json` | What the pinned rustc answers to the questions blaze answers in its place (`-vV`, `--print cfg`, target specs, CPUs, file names) | `harness/answers.py --write` on each host | `blaze_pins::RUSTC_LLVM` (the LLVM line of `-vV`, held equal to `rust.txt`); `scripts/src/bin/toolchain_cut.rs` (by `blaze_pins::ANSWERS_PATH`); `tools/cpus.py` |
| `answers/own_targets.json` | The same answers for blaze's own targets, which no rustc has | By hand | `tools/cpus.py` |
| `answers/clang.json` | What the pinned clang answers to the compiler probes a wasm build script makes (cc's family detection, `-?`, each `is_flag_supported`), each row keyed by the clang archive, the program, the arguments, the clang-read environment and the probe files' digests | `scripts/src/bin/clang_answers.rs` fills PENDING rows by running the shelf's clang; a row is added from the refusal a missing one prints | `blaze_pins::{CLANG_ANSWERS_JSON, CLANG_ANSWERS_PATH}` (blaze-build-script `c_tasks_probe`; its digest is in every wasm script's base key) |

The `const` checks in `crates/blaze-pins/src/lib.rs` hold the files to one another: `toolchain`
equals `rust.txt`'s release, and `answers/rustc.json`'s `version` and commit hash equal `rust.txt`'s.
A bump that moves one file and not the others does not compile.

Moving the pin: [docs/toolchain.md, "Moving the pin"](../docs/toolchain.md#moving-the-pin).
