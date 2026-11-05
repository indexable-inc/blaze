# ai-dynamo/dynamo's Python bindings: cargo against blaze main (lane tejas, #730)

`lib/bindings/python` of ai-dynamo/dynamo at 4943a66, a pyo3 cdylib (`lib_core.so`) over the
`dynamo-llm` crate and about 900 others. The edit is the body of `TokenBlock::remaining` in
`lib/llm/src/tokens.rs`, a dependency of the cdylib:
`(self.block_size as usize).saturating_sub(self.tokens.0.len())` wrapped in
`std::hint::black_box(...)`, then reverted. Each arm has its own `CARGO_TARGET_DIR` and daemon
home. Each step is one `cargo build` (or `blaze build`) timed whole, followed by the
sha256 of the cdylib (first 12 hex digits).

The machine was an ix VM: arm64 Neoverse-V3, 8 vCPU, 16 GiB committed, `CARGO_BUILD_JOBS=6`, in a systemd
unit with `MemoryMax=10G`. It isn't a dedicated bench VM, so the numbers are diagnostic.
x86_64 could not be placed (`memory_full`, ix#5992), so there are no x86_64 numbers. The project
pins `rust-toolchain.toml` 1.96.1, and its `.cargo/config.toml` sets `--cfg tokio_unstable` and
`-C target-cpu=neoverse-n1 -C force-frame-pointers=yes` for aarch64. The cargo arm is rustup's
1.96.1, as the pin asks. The blaze arm is main at 63aaa237db's `blaze` with its own
nightly-2026-10-27, which `toolchain::settle` picks over the pin. Run it with `PROTOC`
(nixpkgs protobuf) and `LIBCLANG_PATH` (nixpkgs libclang, for bindgen) set.

## dev

| step | cargo 1.96.1 | sha | blaze main | sha |
|---|---|---|---|---|
| cold | 305.3 s | 24a7c4affcc6 | 70.2 s, exit 101 | none |
| nothing edited | 0.81 s | 24a7c4affcc6 | 2.5 s, exit 101 | none |
| edit | 73.3 s | 69e8a4c5c15c | 2.2 s, exit 101 | none |
| revert | 80.1 s | 24a7c4affcc6 | 3.1 s, exit 101 | none |

## release

| step | cargo 1.96.1 | sha | blaze main | sha |
|---|---|---|---|---|
| cold | 483.8 s | 92554e002ca8 | 28.4 s, exit 101 | none |
| nothing edited | 0.65 s | 92554e002ca8 | 4.3 s, exit 101 | none |
| edit | 219.8 s | 65505b983606 | 3.7 s, exit 101 | none |
| revert | 218.5 s | 92554e002ca8 | 5.6 s, exit 101 | none |

Cargo's artifact changes on the edit and returns to the cold build's bytes on the revert.

blaze main does not build dynamo. It fails loudly, never reporting a build it did not do:
`error: could not build lib_core.so: Failed to build the build scripts of ...`. The cause is the
proc macro `macro_rules_attribute-proc_macro 0.2.2`, of which 3 functions do not get through the
frontend (`frontend: mir: UnresolvedMethod("vec")`, `("flat_map")`, `("vec")`). The crate
defines `impl<T: IntoIterator> CollectVec for T {}` inside the body of the trait's own default
method, and a method call outside that block does not find the impl. The 10-line repro is on
#771. Expanding natively (`BLAZE_PROC_MACRO_WASM=0`, with blaze-proc-macro-srv beside blaze)
fails the same way, because blaze links the proc macro there too. The blaze dev cold time
includes downloading blaze's toolchain (6.4 s) and a crates.io retry.

With plain cargo, blaze's nightly-2026-10-27 does not compile `dynamo-llm` either: E0277,
`dyn NixlLayout<StorageType = S>: NixlLayout` is not satisfied
(`block_manager/distributed/worker.rs:788`). Under its pinned 1.96.1 it builds.

## The pyo3 case of #730, small

A workspace of one pyo3 0.27 cdylib over one dependency crate, with the same kind of edit (a
`saturating_sub` body in the dependency). Measured on an earlier VM (arm64, 16 vCPU, about
6.5 GiB granted, `CARGO_BUILD_JOBS=4`), main at 82db94d:

| step | cargo | blaze main |
|---|---|---|
| cold | 7.8 s | 51.8 s |
| nothing edited | 0.02 s | 0.14 s |
| edit | 0.12 s, sha a0f929a5c280 | 7.7 s, sha bac7cf7535ec to da8f35132403 |
| revert | 0.12 s, sha b88baeb19484 | 4.2 s, sha d5ef064f541c |

blaze's `.so` changes on every edit, and the module imports and computes the edited value. The
stale artifact #730 reports does not happen on main. A cdylib is relinked on every edit, not
spliced, which is why its edit costs 7.7 s against cargo's 0.12 s.
