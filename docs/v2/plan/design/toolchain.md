---
status: plan
summary: How blaze gives every crate its standard libraries with nothing in the manifest. The toolchain is records: one source pack per crate (core, alloc, std and what they are built from, bstd and the crates beneath it) in blaze-fetch's pack form, listed by one manifest record whose digest the binary is built with and the units record's key holds. An installed blaze finds them on a per-user read-only shelf, verified when opened; the implicit dependencies are one rule in blaze-unit-graph; a blaze-edition manifest that still names bstd gets a finding with its fix.
date: 2026-11-16
---
# The toolchain as records

Under a blaze edition a package names nothing of its standard library: blaze gives `core`,
`alloc` and bstd with its prelude (std.md, "The blaze edition: opting in", row S5d). A Rust
edition crate is given `core`, `alloc` and Rust's `std` the same way. Where those sources come
from is this page; what they are given to is blaze-unit-graph's.

## 1. Identity

- **A pack per crate.** Each crate's files are one blaze-fetch pack (`BLZFPK03`, fetch.md), read
  in place by `PackView`. A crate cut from blaze's tree (library/, library/vendor, crates/) is
  kind 3, *a tree of blaze's toolchain*: its source digest is its own listing digest. Its pack
  holds its folder and what its sources reach outside it by `#[path]` (the package folder: core
  takes stdarch's `core_arch`, std takes `backtrace`) or by `include*!` (the one file), at paths
  relative to its tree, with the crate's `dir` in its row; a file two crates reach is in both. A
  registry crate beneath bstd (libc) is kind 1, made by blaze-fetch's unpack from the `.crate`
  the lock pins, so it is byte for byte the record `Fetch` would store. A pack is named by its
  footer, the BLAKE3 of its bytes, which `PackView::open` checks: opening is verifying.
- **One manifest record** (`BLZTCH02`, crates/blaze-toolchain) lists every crate: name,
  version, edition, role (`core`, `alloc`, `std`, `proc_macro`, `bstd`, `library`, `blaze`,
  `registry`), library name and root file, its pack's digest and length; and per target triple
  the sysroot's units as blaze-unit-graph's `Resolution` made them on library/'s lock and on
  the workspace's lock for bstd (features chosen once, when the toolchain is cut, as rust-src's
  prebuilt std is one configuration), with the target's cfg atoms as the pinned nightly prints
  them (`ToolchainView::cfg`; a reader needs no rustc answers of its own). Its footer is the BLAKE3 of its bytes: the **toolchain
  id**. Two cuts of one tree write equal bytes (rule 25).
- **Made by a tool** with one typed JSON input (scripts.md, tier 0's input form):
  `blaze-toolchain {"tree": .., "targets": [..], "registry": .., "shelf": .., "write_pin": true}`
  cuts the packs, writes the manifest, places them on the shelf, and writes the id as 64 hex
  digits to `crates/blaze-toolchain/PINNED` (data, not Rust). `"check"` instead recomputes the
  id and fails when `PINNED` differs: the gate's row.
- **The binary knows its toolchain by that id**: `blaze_toolchain::PINNED` is `PINNED`'s bytes
  read at compile time, to know which manifest to open. The id is in exactly one key: that of
  the record saying which crates and packs make the sysroot of a unit graph (the units record),
  so another toolchain gives another graph. Everything downstream is keyed and validated by the
  text it read, as for a registry dependency (rules 65, 77): an upgrade that leaves an item's
  text unchanged leaves the records that read it valid. It is not in the tool key.

## 2. Where an installed blaze finds it

The **shelf**: `<home>/.blaze/v2/toolchain/`, flat, one file per record named by its 64-hex
digest: the manifest and every pack. Read-only to a build. Decided against the others:

- *beside the binary*: bstd has no capability naming the executable's folder, and `~/.cargo/bin`
  is no place for 100 MB of data;
- *the checkout's store* (`.blaze/v2/records`): one copy per project, not per user;
- *the shared cache*: no bucket exists, and a private build must work offline. It is designed
  as a way to fill the shelf, never a build's dependency: `blaze toolchain fetch <id>`, started
  in the shelf (its start folder, the only place it writes), reads the manifest and each pack by
  digest through blaze-remote's read path, verifies each as `open` does, and publishes it.

The home is a driver fact: `main` reads `HOME` at the edge, beside `PWD`, and the shelf is
reached through bstd's read-only `Fs::ancestor` (the nearest folder above the start that holds
it), or under `Fs::root` when the shelf lies beneath the start folder (blaze-cli `toolchain.rs`).
Above the start it is confined by convention: every read is under the shelf's prefix and every
name is 64 hex digits. Confining it by type needs a bstd `Ancestor::folder` giving a read-only
folder handle; that is bstd's lane. Neither the path nor the home enters a
key or a record. Two blaze versions coexist: two ids, two manifests, shared packs. `~/.blaze`'s
other folders and `~/.cache/blaze` are the old blaze's and are never read.

**Opening** (`blaze_toolchain::shelf`, sans-IO over a `Shelf` trait the cli implements):
the manifest by its id, held to 4 MiB before it is read, its footer compared with the pinned
id; a pack only when a task needs a toolchain crate's text (a miss of its records), its length
compared with the manifest's before a byte is read, then `PackView::open`. Verified once per
command that opens it; once per boot is later work (memory-and-data's rule). Absent or other
bytes is `ToolchainError` naming the id or the crate, the digest and the folder looked in.
Never rustup, a path, `RUSTC` or another version (rules 3, 8).

## 3. What each unit is given (blaze-unit-graph, `implicit`)

One function, `implicit(edition, kind) -> Implicit`, used by the driver, the names library and
the ladder:

| unit | `core` | `alloc` | `std` names | prelude |
|---|---|---|---|---|
| Rust edition | yes | by `extern crate alloc` | Rust's `std` | `std::prelude::rust_20xx` |
| blaze edition (lib, bin, test) | yes | by `extern crate alloc` | bstd; Rust's `std` has no name | `bstd::prelude::blaze_2026_11` |
| its build script, its proc-macro target | as a Rust edition crate | | | |
| a proc macro | also `proc_macro` | | | |

std.md ("The blaze edition: opting in", the `std` row) decides the name: "`std` names the
dependency called `bstd`; Rust's `std` has no name in the crate", and S5d: bstd "is given to
every crate of the edition as `std` is given". Neither doc has an opt-in to Rust's `std`; none
is added. Decided 2026-11-16: `std` is bstd's one extern name; `bstd::..` in a blaze-edition
crate is a finding whose fix rewrites it to `std::..`; canonical paths stay `bstd::cx::Cx`.

The toolchain's units stay the manifest's list per target (`ToolchainView::units`), indexed by
`ToolchainUnit`. `blaze_toolchain::sysroot(view, triple, for_host)` names the five a unit may be
given, as a `Sysroot`, and `Implicit::externs(&Sysroot)` and `extern_crate(name, &Sysroot)`
give what each unit has with no line, in name order. The workspace's graph is not merged with
them, so a package the toolchain holds (bstd) is never two units. The driver's units record
hashes `PINNED` and each target's `implicit` (decision above), so a change to either reruns
every reader once.

## 4. The finding

A blaze-edition member whose `[dependencies]` or `[dev-dependencies]` names `bstd` or the package
`blaze-std` gets Cli rule 9, `toolchain-crate-named`, an error over the whole line including
its newline: the line shadows the toolchain's bstd and can silently be another version. Its
fix deletes the line. The new layer has no `--fix` yet, so the fix is drawn as a preview with
"by hand for now" and is not counted as fixable. A Rust-edition package that names bstd keeps
its line.

## Install

`install-blaze.sh` runs the tool first (it writes `PINNED` and fills the shelf), then builds and
copies the binary. Nothing else is placed; a removed shelf file is reported by name.
