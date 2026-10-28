# Changelog

All notable changes to blaze are written here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions after 0.1.0 are dated,
`0.YYYYMMDD.N` with N counting the releases of that day from 0. Up to 0.20261028.9 the release
train wrote each one's section here; since then no release commits to main (docs/release.md,
"Versions"), and a release's notes, in its GitHub release, are the lines `[Unreleased]` gained
since the release before and the title of each change merged since.

## [Unreleased]

### Changed

- `blaze-engine cache build` prints what this build's store keys are made with apart from the toolchain (the store's format and the hash of blaze's sources), and fails for a build keyed by its executable's bytes: CI will name the remote's project by it instead of by the release, so a release whose compiled-in sources did not change keeps the store (docs/cache.md).
- Releases: a canary is cut as soon as main's Linux x86-64 build of a commit passes, without waiting for the unit tests; stable now also needs main's merge gate on that commit green, with the unit tests run and passed on Linux x86-64, Linux ARM64 and macOS (docs/release.md).

## [0.20261028.9] - 2026-10-28

### Merged

- canary.yaml: a canary of every green main, by a tag on main's own commit (no release commit racing merges); release.yaml stamps the tag's version into its checkout; one promote gate at a time, the newest canary first (#953)
- docs/driver.md: the build driver as a sans-IO build graph (design note) (#955)
- install.sh: the archive's folder check as an if (shellcheck SC2015) (#954)
- vectors in the Win64 C ABI; corpus const_traits builds without nightly (#945)
- promote.yaml: the one-gate-at-a-time group is main's runs only, so a dispatch on a branch (a check of the gate) never holds a canary up (#958)
- a workspace the daemon could not load names the functions blaze could not compile (#905)
- const macros stage 0: derive expanded by the const evaluator (#915)
- answers test asks a spec every host agrees on (Windows tests) (#957)
- blaze-graph: the build driver's graph as a state machine that does no IO, its executor and replayable traces (docs/driver.md step 1) (#956)
- canary.py cut: the newest commit of main whose ci is green (HEAD is seldom green yet: main moves faster than its gate), and a release.yaml run on a branch does not hold a cut (#962)
- const macros: the const-macros key loads a file as the package's macro crate, behind experimental = ["const-macros"] (#959)
- canary.py stamp runs on Python 3.10: release.yaml's ubuntu-22.04 builds of the first canary v0.20261028.4 failed on 'No module named tomllib' (#966)
- NVIDIA Dynamo in the stable gate: smoke.json entry (workspace, Python bindings built and imported, CPU-only mocker server, tests, excluded list with why), bench.py dynamo against cargo on its pinned 1.96.1 (#965)
- blaze test: the walk after an edit keeps what it found (#901); memchr's dispatch keeps tests hermetic (#910)
- harness/nightly_features.py: one issue per unstable language feature of the pinned nightly, under a sub-issue tree (#1025)
- const eval: every call of a constant's evaluation counts against the crate's recursion limit, as in rustc (#862) (#961)
- install.sh and the site's install page: drop the rename's 'blaze run is the same' leftovers; the page explains canary and stable and --channel canary (#1247)
- docs: const macros' status, stage 0's limits, measured numbers, and Graph::map with a const fn (#1251)
- consteval: the train holds main's 727 of 767 evaluation and 222 of 222 run-pass (#862) (#1248)
- The merge gate builds and tests blaze with blaze, from blaze's R2 store; no sccache or rust-cache (#896)
- No rust-cache in any workflow: blaze where it tests, cargo with no cache where the binary is a tool (#899) (#1253)
- build UI round 2: the live view is the build's crate graph, every step named with counts, a slow build says why (#1249)
- harness: constmacros.py, a const derive through blaze run (cold, warm, edit, experiment off) (#1250)
- answers test asks aarch64-unknown-linux-musl, a spec every host agrees on (Windows tests) (#1254)
- splices.py shows what a wrong output printed (x64 llvm core-4 thread-local rows, train 40669942920) (#1252)
- a build script's kept run is found from any checkout, CI or remote (it was kept for its own checkout alone) (#1255)
- blaze-ui: a header, a line of every step, and what held the build up (#1257)
- A build script's rustc probe reads std's frozen def maps from the store (#1262)
- bench/results: #830's sessions against #918 on bat and just (#908) (#1256)
- blaze: how busy the cores are, live and per step, and cold from the start (#1263)
- main pushes blaze's store (secrets: inherit; the train pushes nothing), and the gate's blaze is pinned in .github/blaze-version (#1264)
- Build UI piece 3a: piped output says only what took time; blaze status is the live view (#1267)
- blaze_cli.py counts the tests blaze test kept the pass of (#836) as cargo's (#1266)
- experience.py: a second or more at no named step is a P1 rough edge (#1268)
- Build UI piece 3c: a function count that adds up; a Took line that fits (#1270)
- Build UI piece 3d: blaze why lists where the time went, as the live view names it (#1271)
- The borrow checker's trait solver answers no constraint of an error region it reads nothing of (#1273)
- const heap: freed memory is reused, stack frames are freed, and a Box or Vec may not be in a constant's value (#920)
- Build UI piece 3b: the functions slow to compile, under their crate as they go, five at the end, all in blaze why (#1274)
- the build scripts' standard library is made of the shipped one, fetched before the load (#1280)
- blaze-remote says blaze-engine's exit status when a pull or push fails (#1278)
- blaze tags its target directory as cargo does, so a no-change blaze test reruns no build script and relinks nothing (#83) (#1285)
- bench: ci built and tested with blaze, before and after (#1281)
- a build script's sandbox has the store readable below /tmp (#1283)
- blaze cache push|pull: a reset from the remote is an error, not a silent exit 141 (#1282)
- docs/cargo.md: the BLAZE_TESTS table said the default is all; it is affected (affected::on treats anything but all as on, and the tests section already says so). Refs #227. (#1289)
- the codegen says every function it puts in the program as Event::Functions records, for the build page (#1272)
- const eval: stop freeing a frame's stack on return (P0 hotfix for #920). Before: smoke (std) on a warm run of #920's code fails every case, hashmap included: <u8 as Display>::fmt and the rest have 'layout of [MaybeUninit<u8>; {const error}]' (core's MAX.ilog10() + 1), and the bisect drafts #946-948 put it on frame freeing. After: the stack is kept as before #920 (FREES_FRAMES = false), the heap's reuse and the constant cache stay; stack_frames_are_freed is ignored until the read of a returned frame is found. (#1287)

## [0.20261028.3] - 2026-10-28

### Added

- Git-index registries can be resolved and fetched, including offline use of Cargo's index and archive caches; failed index updates are reported instead of using an old tree (#316).
- A build shows what it does while it does it. The daemon and its workers speak deterministic
  CBOR frames (RFC 8949 §4.2.1) instead of JSON lines, and a worker publishes the build as
  events: each step (loading the workspace, resolving names, compiling, checking, linking),
  each crate's functions compiled and reused, and every function an edit rebuilt with why (its
  body was edited, or it uses what was). On a terminal a live region is redrawn, never silent
  for more than a second, and the build ends with one line (`Rebuilt 3 fns: render
  (src/main.rs edited), draw, tick (uses what changed); 0.21s`); off a terminal cargo-like
  lines. `-q` shows none.
- `blaze why [fn|crate]` says what the last build rebuilt and why.
- The built-in `gca!` macro of the pinned nightly (generic const arguments,
  `#![feature(gca_min_const_items)]`) expands to its expression, as rustc evaluates it (#862).
  A constant using it was refused ("this built-in macro is not implemented").
- `--events json|cbor` writes the build's events to stdout, one record a line or frame.
- `variant_count` in a constant evaluates, as rustc's does: an enum's variants, 1 for a
  struct or union, 0 for any other type (#862). A function using it was refused.

### Changed

- Wasm proc macros expand in a session per calling crate only when they keep state (#908).
  The guest's allocator counts live bytes, and an expansion that leaves some behind marks its
  module stateful in the store. A module none of whose expansions did (serde_derive,
  thiserror and most derives) goes back to a fresh instance per expansion: its expansions are
  kept, and crates that call it are frozen again. A module not seen yet expands in a session
  until a build has run it.
- `blaze test` keeps a hermetic test's pass by its code, with no opt-in (#836): a test whose
  reachable code reaches no file, environment variable, socket, other process, foreign code,
  inline assembly, unbounded indirect call, writable shared static or code blaze can't compile
  runs again only when that code changes. Every other test runs every time. Each test, whether
  it is hermetic and why not (a kind, what it reaches and the fewest calls to it) is written to
  `target/blaze/tests.ndjson`, with counts and the reasons that keep the most tests running in
  `target/blaze/tests.summary.json`; `blaze test --why` prints them as a table, and
  `--why --json` as the NDJSON lines. `BLAZE_TESTS=all` runs every test. Sandboxed tests stay an
  experimental opt-in.
- Release profiles build with LLVM at the profile's level (#831): `opt-level` 2 and 3
  (`release`, `bench`, a profile that inherits them) build at `-O2` and `-O3`, `s` and `z` at
  `-O2`, and 0 and 1 (`dev`, `test`) stay on Cranelift. `BLAZE_BACKEND` and `BLAZE_OPT` still
  choose. A dev build and a release build of one project keep separate workers. A blaze built
  without its `llvm` feature refuses a profile that asks for LLVM instead of using Cranelift.

### Fixed

- Build scripts run on a stock Ubuntu 23.10 or later, where AppArmor keeps a user from making a
  user namespace: every crate with a build script failed with `the sandbox could not be made:
  /proc/self/setgroups: Permission denied`. Where no user namespace can be made, the sandbox is
  confined with Landlock and the seccomp supervisor instead: writes only to `OUT_DIR` and a
  `TMPDIR` of its own, no network, signals only to its own processes, reads recorded as before
  (docs/cargo.md says what it does not hide). Sandboxed tests still need user namespaces.
- A constant, static or discriminant whose own arithmetic overflows fails to evaluate, as
  rustc's does, in a crate built without `-C overflow-checks` too (`--release`); it was given
  the wrapped value (#862). A `const fn`'s arithmetic still follows the crate.
- Preserve WASM proc-macro state and stored token spans within each consuming crate, including napi-rs constructor registration. Reconstruct declaration state after edits instead of reusing output-only or frozen declaration cache entries; compiled-module and function caching remain enabled.
- A workspace the daemon could not load because it reaches functions blaze could not compile
  names those functions and why; the message stopped at "so it is not built:".
- Native proc macros receive real source positions, text and file paths through the bidirectional server protocol; a server that cannot enable source spans is refused (#823).
- `unchecked_add`, `unchecked_sub`, `unchecked_mul`, `unchecked_shl`, `unchecked_shr`,
  `unchecked_div`, `unchecked_rem` and `exact_div` in a constant fail to evaluate, as rustc's
  do, where the operation is undefined: an overflow, a shift past the width, a division by
  zero or of the least value by -1, and an `exact_div` with a remainder (#862). Some gave a
  wrong value instead: `unchecked_div(-128i8, -1)`, and `exact_div` with a remainder.
- A static whose initializer writes to a static, another or itself, fails to evaluate, as rustc's
  does (#862). It was evaluated as if the write had not been made.
- A constant or static that recurses without end fails to evaluate once its calls are deeper than
  the crate's `recursion_limit` (128 unless set), as rustc's does (#862). The calls were not
  counted, so the evaluation went on until the step budget ran out, keeping every frame: rustc's
  tests/ui/consts/recursive.rs took 15 GB.
- Invalid built-in derives now fail `blaze check` and `blaze build`, including `Default` on an enum without a default variant and derives on unsupported item shapes (#751).
- `Location::caller()` in a constant is evaluated, as rustc does: the place of the first call up
  the stack that does not track its caller, under the file name the build uses (#862). A function
  with such a constant was refused. docs/support.md no longer lists it, nor `%` on floats or
  `type_id` of a type with lifetimes in a constant, which already matched rustc.

### Merged

- answers test: an Apple target's spec under MACOSX_DEPLOYMENT_TARGET is answered (main's gate red) (#902)
- doctests run no cargo, rustc or rustdoc: the doctest load's build scripts and proc macros are blaze's, and a compile_fail example passes only on an error rustc gives too (else it fails with the reason) (#897)
- build events: the daemon speaks deterministic CBOR frames, workers publish typed build events (steps, per-crate fns, every fn an edit rebuilt and why), blaze renders them live on a TTY and as cargo-like lines off one; blaze why; --events json|cbor (supersedes #851's transport) (#878)
- promote.yaml: cargo, zed, servo and bun in the gate to stable (works, and faster than cargo) (#887)
- hir-ty-mir: MIR's passes are a crate of their own (crate split 4c); a pass edit 54 -> 40 s (#904)
- release profiles build with LLVM at the profile's opt-level; dev stays Cranelift (#831)
- blaze test: hermetic tests are kept by their code by default; why each test is not, as NDJSON (tests.ndjson, --why) (#836)
- Preserve stateful WASM proc macros within each calling crate (#830)
- const macros design doc (#909)
- site: the home page is a Brilliant-style course on how blaze works, with lesson sounds (#911)
- site: wrangler names the real Cloudflare resources so deploys work after the rename (#913)
- consteval tests of const traits, derive_const, const closures and fn pointers against nightly (#917)
- proc macros: require bidirectional source-span queries (#889)
- library/: the standard library's source in the repository (nightly-2026-10-27), merged by the daily bump (#922)
- corpus case for const traits, derive_const, const closures and a const hasher; docs/const.md (#919)
- const eval: step and memory budget per evaluation, [workspace.metadata.blaze] const-eval (#912)
- registry: read and cache git registry indexes (#891)
- Windows CI builds blaze with cargo's stable build-dir layout (#921)
- const purity query (#860): fn-cache stops at const-pure const fns; blaze check warns of const fns that read mutable state (#924)
- a failed build's error is said once; blaze_cli.py checks fit #878's build UI and #837's silence on pins (#928)
- diagnostics: fail builds on built-in derive refusals (#895)
- nix/units: the manifest's exclude has library (nix unit graphs check, red since #922) (#934)
- build events: a function an edit took away is not listed as rebuilt (edits.py smoke AstIdMap panic, from #878) (#937)
- sandbox: confine build scripts with Landlock where no user namespace can be made (stock Ubuntu 23.10+) (#925)
- Location::caller() in a constant, as rustc evaluates it; the const-eval rows of docs/support.md go (#916)
- const eval: unchecked_* and exact_div fail where they are undefined, as in rustc (#862) (#933)
- const eval: the variant_count intrinsic (#862) (#936)
- const eval: the generic simd intrinsics lane by lane, with float results that depend on no NaN payload (#935)
- wasm proc macros: a session per calling crate only for modules seen to keep state (#908) (#918)
- const eval: a constant's own overflow fails, as in rustc, without -C overflow-checks too (#862) (#929)
- const traits: effect-carrying [const] bounds in hir-ty, derive_const as const impls, non-const call check (#940)
- consteval_ui.py: blaze's const eval against rustc's tests/ui/consts and const-generics, held by the train (#862) (#941)
- answer host-independent target specs on Windows (#942)
- hir-expand: the gca! built-in macro (#862) (#938)
- fuzz: generated const fns, evaluated as constants and at run time (#862) (#923)
- a static's initializer that writes to a static fails to evaluate (#862) (#943)
- rustc's answers under a link's flags and with the warnings of dropped crate types (#837) (#927)
- experience: a step cargo fails and blaze passes is noted, not a P0 (#944)
- const eval: core::arch's AES, CRC-32, CLMUL and SHA intrinsics (llvm.x86.*, llvm.aarch64.*) (#926)
- const eval: the compare_bytes intrinsic, which <[u8] as Ord>::cmp calls. Before: basic-toml parsing in a const was refused with 'intrinsic compare_bytes is not implemented' (smoke ce_toml on #931). After: it evaluates; test compare_bytes checks -1/0/1 as rustc's evaluation gives. (#949)
- README: the two release channels, and blaze's Rust is its own (rustup's copy only reused) (#952)
- install.sh: unpack the archive's one folder, whatever the file is named; refuse an archive with no blaze (#951)

## [0.20261028.2] - 2026-10-28

### Added

- A build or check that waits for the daemon says what it waits for: on a terminal one status
  line, written over as the worker loads the workspace, resolves names, compiles, checks and
  links (`Loading workspace (38 s)`), and cleared when it is done; off a terminal one line per
  step. Before, a first build of a large workspace printed nothing for a minute. `-q` shows
  none.

### Removed

- `blaze enable` and `blaze disable`, and plain cargo building through blaze (#93, no-rustc.md's
  step M): `blaze` is the only way in. A plain `cargo build` is cargo's and rustc's alone. On a
  machine where `enable` was run, the next `blaze` takes `build.rustc-workspace-wrapper` out of
  cargo's `config.toml` (the file as it was before, when nobody edited it since), puts back the
  `cargo fmt` and `cargo clippy` its links replaced, and says so in one line.
  `BLAZE_RUSTC_BASE` is gone with it: the first edits no longer patch an executable rustc
  linked, which only that mode still had. `blaze-rustc` stays, as the wrapper of a load into
  cargo's target directory.

### Merged

- blaze pin: a workspace pins a release of blaze, which blaze runs (#841)
- llvm.yaml: an archive already on the release is left alone (main's build jobs 404 on the pinned LLVM) (#879)
- install.sh --channel canary: the newest dated tag whose release has its SHA256SUMS. releases.atom lists plain tags too, so #873 picked v0.20261028.0 while release.yaml was still building it (install.yaml macos-15, run 40638482080: 404 on its SHA256SUMS). Now each dated tag of the feed is tried newest first until one's checksums download, which also skips a release whose assets are still uploading. (#876)
- ci.py: stdstatics.py's measured seconds (#880)
- smoke corpus: programs blaze run/test must get right, std cases in the merge gate (#875)
- blaze-cargo: the cargo drop-in is a crate of its own (crate split 4b) (#858)
- promote.yaml: a canary that passes the heavy tests becomes stable, the same release flipped (#883)
- pin: version = "stable" or "canary" runs the release that channel names (#882)
- promote.yaml: a run with promote=false opens no issue (#885)
- answers: a non-Apple target's spec is answered with SDKROOT set (macos arm64 / build: proc-macro server, release v0.20261028.1) (#884)
- blaze-splice: the splice is a crate of its own (crate split 4b, second half) (#888)
- answers: an Apple target's spec is answered with SDKROOT set; it names no deployment target (macos arm64 / build after #884) (#890)
- Delete blaze enable and plain cargo's wrapper: blaze is the only way in (#93) (#886)
- A workspace's target error keeps its cause; test of release v0.20261028.1's question (wasm32-wasip1 spec under MACOSX_DEPLOYMENT_TARGET) (#881)
- agents: any agent takes any pull request to merged, drafts and other lanes' included (rule own-prs-to-merge) (#898)
- blaze shows what the daemon is at while a build or check waits (#851)

## [0.20261028.1] - 2026-10-28

### Fixed

- A program that spawns a thread or makes a `HashMap` builds with the standard library a
  release ships (#840, #224). Its build was refused with "writable static `MIN` (or
  `__RUST_STD_INTERNAL_VAL`) without a rustc symbol": a static declared in a body of std
  (`thread::min_stack`'s `MIN`, each `thread_local!`'s, `RandomState::new`'s `KEYS`) is named
  after where it is written, and the name had the path of std's sources on the machine that
  built the unit (CI's runner, `/Users/runner/.rustup/...`), so a program built anywhere else
  asked for names the unit does not define. The place now names a file of the standard library
  from `library/` on, and one cargo fetched under `/cargo`, the same on every machine, and the
  program links to the unit's one copy of each static. The unit's store key changes, so it is
  built or fetched once again. `harness/stdstatics.py` makes the unit from std's sources in
  another directory and runs the three programs, dev and `--release`.

### Merged

- std statics in bodies: name their place by a path every machine shares (#840) (#857)
- experience.py flags the names blaze no longer has, not every mention of blaze (#869)
- nix/units: blaze-protocol and blaze-ui (main's unit-graph check, stale since #855/#856) (#871)
- agents: scripts take one typed JSON input, not flags (#874)

## [0.20261028.0] - 2026-10-28

### Changed

- `cargo blaze` is gone: run `blaze` (same subcommands and flags). The program `cargo-blaze`,
  and `cargo-ra-jit` before it, are no longer built or shipped; install.sh removes the one it
  finds and says so in one line.

- ra-jit is blaze everywhere (#270, [docs/rename.md](docs/rename.md)). The engine `blaze` runs
  is `blaze-engine` (was `ra-jit`), and cargo's rustc wrapper is `blaze-rustc` (was
  `ra-jit-rustc`; `ra-jit-rustc-first` is gone). The crates are `blaze-engine` and
  `blaze-<name>`, and crates.io gets `blaze-engine` (was `cargo-blaze`): `cargo install
  blaze-engine`. `#[ra_jit::cache]` is `#[blaze::cache]`, `[package.metadata.ra-jit]` is
  `[package.metadata.blaze]`, and the state directories are `~/.cache/blaze`,
  `$XDG_RUNTIME_DIR/blaze` and `target/blaze`. blaze starts these afresh, without moving the
  old ones: the first command that finds one says where it is and how big, and that it can be
  deleted. install.sh over a blaze from before disables the old one when it was enabled,
  removes its programs and enables the new one; `blaze enable` replaces the links an earlier
  `enable` made to `cargo-blaze`.

- The forks of crates.io crates take blaze's names (#270). rowan is `syntax-tree`
  (`crates/syntax-tree`, `use syntax_tree::...`), and rust-analyzer's `ra-ap-rustc_abi`,
  `ra-ap-rustc_type_ir` and `ra-ap-rustc_next_trait_solver` are `rustc_abi`, `rustc_type_ir`
  and `rustc_next_trait_solver`. They are path dependencies of the workspace at its version,
  no longer patches over crates.io, and publish as `blaze-syntax-tree`, `blaze-rustc_abi` and so
  on.

- blaze's settings are `BLAZE_<NAME>` alone (#270). A setting under its old name,
  `RA_JIT_<NAME>`, is no longer read; blaze stops when one is set, with a line for each:
  `RA_JIT_CACHE was renamed BLAZE_CACHE`. The programs blaze builds read `BLAZE_FN_CACHE*`
  and `BLAZE_TIER` alone too.

### Merged

- a build keeps its plan while what it was made of is unchanged, not resolving the workspace on every no-op (#814)
- BLAZE_REMOTE never holds up a build; lazy packs waited for 250 ms at most; gateway connects within 2 s (#815)
- nix: the flake's default is blaze, so nix profile install github:indexable-inc/blaze installs it (#816)
- release: notes under GitHub's limit, publish_crates fixes, crates.io off until the rename (#819)
- release: notes longer than a GitHub release allows keep the entries that fit and link CHANGELOG.md (#820)
- install: the check reads what `blaze toolchain` says now (#821)
- one workspace: vendor/rust-analyzer's crates are root members with one Cargo.lock, the fork's unused crates and fork sync removed (#818)
- settings are BLAZE_* alone; RA_JIT_* stops blaze, naming the new name (#270) (#817)
- build(deps): bump actions/setup-python from 6 to 7 in the actions group (#822)
- wasm proc macros: a kept expansion loads no module, expansions that ask a span's line/text/file kept with their answers, counters per edit (#824)
- site: redesign in the ix docs layout: sidebar, animated fire logo, a home page around four goals with drawings, install and usage pages, client-side page swaps (#825)
- publish_crates.py: a version-string dependency and the toolchain's include_str! in toolchain.rs (crates.io job of v0.20261027.0) (#826)
- move the front end's crates (from vendor/rust-analyzer), rowan and rustc_abi into crates/ (#829)
- an Apple link writes again the headerpad file a clean took (#721 on macOS) (#835)
- blaze builds wasmtime again: System V vectors in XMM registers, negative float literal patterns (#834)
- a checkout's git asks DNS for nothing (#838)
- harness: cargo_ra_jit lays out every side as stable cargo (fd/ripgrep/just tests found their program, train #61) (#845)
- ra-jit is blaze everywhere; blaze is the only command; forks renamed (#270) (#832)
- COFF statics keep a section each, so the PE splice reads their size (splices.py 59 on Windows LLVM) (#846)

## [0.20261027.0] - 2026-10-27

### Added

- Credential providers are asked as cargo asks them (#315): `cargo:token-from-stdout`, and
  credential processes in cargo's protocol, version 1, in the order of
  `registry.global-credential-providers`, for the index, `.crate` files and the web API
  commands alike. `cargo:wincred`, `cargo:macos-keychain` and `cargo:libsecret` on their own
  platform are refused, naming the provider and what to set instead; they were refused with
  every other provider but `cargo:token`. `credentials.toml` gives only tokens, below their
  environment variables, as in cargo 1.101 (`harness/own_registry.py`, 101 of 101 cases).

- Nightly's `#[derive(From)]` (`#![feature(derive_from)]`, `std::from::From`) expands, as
  rustc's does: `From` of the type of a struct's one field, keeping the struct's generics.
  Every use of the conversion was refused ("no impl of the trait was selected for it"). On an
  enum, a union, or a struct without exactly one field it is an error, as in rustc (#600).

- `blaze lock --fetches [--target <triple>]...` pins what build scripts download, as Nix pins a
  fixed-output derivation: each download by its sha256 in `Cargo.fetches.lock` beside
  `Cargo.lock`, kept by the package's name, version and `Cargo.lock` checksum, the bytes in
  blaze's store (so a remote cache carries them). A build, which has no network, hands each to
  its build script through the crate's own variable (`RUSTY_V8_ARCHIVE` for `v8`,
  `ORT_CACHE_DIR` for `ort-sys`, `LK_CUSTOM_WEBRTC` for `webrtc-sys`;
  `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1` for `helix-term`, which then downloads nothing), and a
  missing or stale entry fails naming the command. A script refused the network that blaze has
  no override for is told which variable an override would set
  (`bench/results/2026-10-27-bscache/fetches.md`).

- `cargo blaze build` links a library's `cdylib` (#262): a package whose crate types include
  `cdylib`, such as a napi-rs Node addon or a pyo3 extension, gets its shared library at
  cargo's path (`target/debug/lib<name>.so`, `.dylib` on macOS), exporting its
  `#[no_mangle]` and `#[export_name]` functions and statics, with `cargo:rustc-cdylib-link-arg`
  honoured. Before, the build printed `Finished` and wrote no library. A build that would write
  a `staticlib` stops with an error saying blaze does not link one yet. Reported by @bompus.

- blaze is its own toolchain, named `blaze-<date>` (`blaze-2026-10-25`, today the Rust nightly
  of that date): `blaze toolchain` names it and `blaze toolchain --path` prints its directory,
  and a project that pins another toolchain is told so once, as a note, while blaze builds
  with its own (docs/toolchain.md; #402, #403 and #404 for building, hosting and bumping it ourselves).
- `cargo blaze doc` (and `blaze doc`) writes the HTML documentation of the packages and their
  dependencies from rust-analyzer's HIR (`crates/blaze-doc`), with no rustdoc: a page for
  every item with linked signatures, fields, variants, inherent, trait, blanket and auto
  impls, implementors and the members an impl leaves as the trait provides them; docs as
  rustdoc's Markdown with intra-doc links resolved (an unresolved one warns as rustdoc's
  does); source pages, an "All items" page, a search box (`s` or `/`) over every crate
  documented, light and dark themes. cargo's flags (`-p`, `--workspace`, `--no-deps`,
  `--document-private-items`, `--open`, features, `--target`, `--lib`, `--bin`, ...); the
  workspace loads once and the pages are written in parallel. `harness/docdiff.py` compares
  it with the pinned nightly's `cargo doc`: on fd and its dependencies blaze writes 100% of
  rustdoc's pages, 99.9% of the members and impls on them and 99.4% of the resolved links
  ([docs/doc.md](docs/doc.md)).

- `nonstandard_macro_braces` no longer lints a call inside another macro's arguments
  (`vec!(..)` in `assert!(..)`), which stock clippy leaves alone; `new_without_default`
  takes a type to be exported only when another crate can name it; the format lints read
  macros marked `#[clippy::format_args]`, as `log`'s are. helix's 20 diagnostics of the
  ported lints are now all written as stock clippy writes them, with none extra.
- `cargo blaze clippy` (and `blaze clippy`) lints with blaze's own lints, `crates/blaze-lints`:
  24 of clippy's ported onto rust-analyzer's analysis and run by the daemon, with no cargo,
  rustc or clippy-driver started. They are written as `cargo clippy` writes them (rustc's
  rendering, cargo's summary line and `--fix` hint, `--message-format=json`, status 101 for an
  error), at the levels `#[allow]`, `#[expect]`, `[lints.clippy]`, `clippy.toml` and `-- -D`
  give. On ripgrep and fd every diagnostic stock clippy writes of those lints, 567, is written
  the same, held so by `harness/clippydiff.py`; helix and biome lint nothing yet, as
  `cargo blaze check` of them fails. docs/cargo.md has the numbers.
- `cargo blaze fmt` (and `blaze fmt`) is blaze's own: blaze-fmt run in process, with
  cargo-fmt's options over the workspace's targets, or rustfmt's over the `.rs` files named
  (`blaze fmt src/main.rs`). No rustfmt or cargo-fmt is started.
- `crates/blaze-fmt`: blaze's own formatter, rustfmt and cargo-fmt at rust-lang/rust 12433c307
  ported onto a syntax tree of blaze's own, which rust-analyzer's parser fills with the spans
  rustc would give each node. It runs in process and starts no rustfmt; what it cannot
  lower yet (nightly syntax rust-analyzer does not parse, a `macro` with parameters, the
  `ignore` option) is refused with the reason, never formatted differently.
  `harness/fmtdiff.py` holds it to stock rustfmt byte for byte: ripgrep, nushell, fd, helix
  and biome with their own rustfmt.toml (6068 of 6068 files, `--check` exit codes and diffs
  too), the same with `max_width = 60` (6068 of 6068), and rustfmt's `tests/source` (773 of
  793; the rest are those refusals).
- `cargo blaze search`, `owner`, `yank`, `login` and `logout` are blaze's own: blaze-registry
  has a client of a registry's web API (crates-io 0.41's), with cargo's choice of registry, its
  token and `credentials.toml` as cargo writes it, and its errors (`harness/own_registry.py`,
  73 of 73 cases against a mock registry, requests and credentials files too).
- `cargo blaze vendor` is blaze's own for registry packages: each unpacked from its `.crate`
  with its `.cargo-checksum.json`, and the `[source]` configuration printed
  (`harness/own_vendor.py`, 18 of 18 cases, every file by its sha256). A git package is
  refused with the reason.
- `cargo blaze remove` is blaze's own: the dependencies out of the manifest, layout and
  comments kept, what named them cleaned up, and the lock file resolved again with every
  version it keeps (`harness/own_remove.py`, 43 of 43 cases, manifests and lock files byte
  for byte).
- `cargo blaze config get` is blaze's own: the configuration cargo reads, in TOML or JSON,
  merged or file by file, with where each value is from (`harness/own_config.py`, 41 of 41
  cases).
- `cargo blaze report future-incompatibilities` is blaze's own: the report a build left, as
  cargo prints it (`harness/own_report.py`, 23 of 23 cases). Every own command colors its
  lines as `--color` or `CARGO_TERM_COLOR` says.
- `cargo blaze uninstall` and `cargo blaze read-manifest` are blaze's own. `uninstall` is cargo
  1.99's, with its install tracker: `.crates.toml` and `.crates2.json` are written byte for
  byte as cargo writes them (`harness/own_uninstall.py`, 18 of 18 cases equal to cargo's).
  `read-manifest` is the package's entry of `metadata --no-deps` (`harness/own_commands.py`,
  now 130 of 130 over ripgrep, fd, helix, nushell and biome).
- `enable` makes plain `cargo fmt` and `cargo clippy` blaze's, for each of the two that is
  blaze's own: links named `cargo-fmt` and `cargo-clippy` in `~/.cargo/bin`, which cargo looks
  in before `PATH`, to `cargo-blaze`. What they replace, rustup's proxies, is kept and
  `disable` puts it back; `status` says whose each command is. Not an `[alias]`, which cargo
  warns of on every run (rust-lang/cargo#10049).
- `cargo blaze tree`, `pkgid`, `fetch`, `clean`, `new` and `init` are blaze's own, with no
  cargo: `tree` from the new `crates/blaze-tree` (cargo 1.101's, on blaze-resolve), `new` and
  `init` from the new `crates/blaze-new`. `harness/own_tree.py` (130 of 130 equal over
  ripgrep, fd, helix, nushell and biome), `harness/own_commands.py` (120 of 120, with
  `metadata`, `locate-project` and `verify-project`) and `harness/own_new.py` (50 of 50) hold
  them equal to cargo's. `cargo blaze metadata` has cargo 1.101's `min-opt-level` in a
  package's `hints`, so nushell's and biome's full metadata are equal too.

- `RA_JIT_WHOLE_BUILD=1`: the whole build runs in the daemon. Its worker loads the workspace,
  builds and links each program, and answers once they are linked; the next build asks the
  same worker, which builds the edits since on what it has loaded and links again, with no
  load and no `cargo build`. A whole build starts no spare worker, and its objects keep each
  function in a section of its own, so the link leaves out what the program never reaches.
- A whole build's edits are spliced into the executable the daemon linked, as the edits of a
  build over rustc's are, instead of compiling and linking the whole program again.
- A whole build plans its programs with blaze's own resolution (blaze-metadata's `programs`)
  rather than `cargo build --unit-graph`: the members' binaries with their
  `required-features`, or the `--bin` or `--example` named, held equal to the unit graph's by
  `harness/own_metadata.py`.
- The daemon's loads read the workspace with blaze's own resolution (`crates/blaze-metadata`),
  not `cargo locate-project` and `cargo metadata`: every locked package fetched by
  blaze-registry, and the graph of the build the load is for, with the features `cargo build`
  of its selection would have, not those of every member unified. A load of axum's example
  no longer reaches crates whose build scripts its build never ran (`env!` of `mime_guess`'s
  variable), and its whole build (`RA_JIT_WHOLE_BUILD=1`) builds. What blaze does not read yet
  (no lock file or a stale one, `[replace]`, a git index) goes to cargo with the reason;
  `RA_JIT_OWN_RESOLVE=0` sends every load there. `harness/own_metadata.py` holds the output
  equal to `cargo metadata --filter-platform` and to `cargo build --unit-graph`'s packages,
  features and edges.
- blaze on Linux needs glibc 2.34 or newer (RHEL 9, Debian 12, Ubuntu 22.04), and the release
  holds it there (#351): `build_release.py` refuses a Linux binary, the proc-macro server's
  included, that asks for a newer glibc; install.sh says what a system below it lacks instead
  of a program that does not start; and the install workflow builds this commit as the release
  does, on Ubuntu 22.04 with the server, and installs it with no Rust in Debian 13, Debian 12,
  Ubuntu 22.04 and RHEL 9 (UBI 9) containers, each running `blaze run` and a derive from
  crates.io expanded by blaze's server, edited once.
- `RA_JIT_WHOLE_BUILD=1` (or `BLAZE_WHOLE_BUILD=1`): `cargo blaze build` and `run` from nothing
  build each program whole with blaze, every crate compiled by blaze over the shipped standard
  library and linked by blaze, and put it where cargo does, with no `cargo build`
  ([no-rustc.md](docs/no-rustc.md), step K, #91). A function the program can reach that blaze
  cannot compile fails the build with which and why, and nothing goes to cargo.
  `harness/coldbuild.py` holds the programs of a fixture equal to cargo's, and with `bench`
  does bench.py's projects.
- `crates/blaze-registry` fetches every kind of source a lock file names, as cargo 1.99
  does (#298): `[source]` replacement as cargo's `SourceConfigMap` (`replace-with` chains to a
  registry, a local registry, a directory, a git repository or a `[registries]` name, and
  cargo's refusals); vendored directories, each file checked against `.cargo-checksum.json`;
  and git dependencies with the `git` command, a database per repository at
  `$CARGO_HOME/git/db/<name>-<hash>` and a checkout per commit at
  `git/checkouts/<name>-<hash>/<short id>`, branch, tag, rev and locked commits, GitHub's fast
  path, submodules from databases of their own, offline. `harness/fetchdiff.py` holds it to
  `cargo fetch` and `cargo vendor` on dev-aws-8 over 314 lock files (the bench projects at
  their pins but bun, the 300 most downloaded crates, git dependencies of every kind of
  reference, a second sparse registry): 3684/3684 unpacked packages and 38/38 git checkouts
  the same as cargo's, file by file, with the same directory names; crates.io replaced by a
  mirror 42/42; ripgrep and zed vendored, 51/51 and 1533/1533 read where cargo vendor put
  them.
- `cargo blaze` reads the workspace with `blaze-manifest`, as cargo 1.99.0 reads it, where it
  ran `cargo metadata --no-deps` for every command (#298): equal to cargo on 385 of 385
  workspaces of `harness/resolve.py`'s corpus. The command's `--config` arguments now count,
  and a manifest cargo refuses ends the command with cargo's message and status. The daemon's
  loads still ask cargo.
- `ra-jit std` ships the standard library's unit for a `dev` program too, not only a build
  script's: a whole build of a dev program from an empty store (step K, #91) fetches it where
  it compiled the library again: ripgrep from nothing 19.4 s to 9.9 s on dev-aws-8.
- Live edits on Windows x86-64: `ra-jit-live` takes patches in a running PE program, named by
  the GUID of its PDB, and registers their unwind tables, and the daemon lays a patch of a PE
  executable out for it. An executable a program runs is spliced through a copy put in its
  place, as on Unix, instead of being linked again; the daemon wakes the program through a
  named event; and `ra_jit_live::edit` reaches the daemon, so `demo/life`'s editor builds as you
  type. life's edit to running is 26.6 ms at p50 (`socket`) on a Windows 11 desktop
  ([live.md](docs/live.md#latency-under-typing)). A static with another value still asks for a
  restart.

- `crates/blaze-build-script`: the environment cargo 1.99.0 gives a build script, computed
  from a plain description of the run with no cargo, ported from cargo's own rules and tests
  (no-rustc step I, #89; #298). Nothing uses it yet; blaze's own runs of scripts come next.
- `blaze-build-script` reads what a build script prints as cargo 1.99.0 does (both
  `cargo:` and `cargo::`, the `rust-version` rule of the latter, every directive and cargo's
  errors for each) and says who receives it: the scripts each unit is linked with, the `-L`,
  `-l`, link arguments, cfgs and variables of each target, and when a script runs again.
- `blaze-build-script` runs a build script as cargo 1.99.0 does (its package's directory, no
  input, `OUT_DIR` made first, a failed exit or a `cargo::error=` line failing the build,
  cargo's `output`, `stderr` and `root-output` files beside `OUT_DIR`), and
  `harness/scripts.py` holds it to cargo over 463 script runs of the benchmark applications and
  crates whose scripts build C or read others' metadata. It found two things cargo gives a
  script that the port had missed: a `cargo check` puts no `target/debug` in the library path,
  and on Linux cargo's libgit2 hands every script the `SSL_CERT_FILE` and `SSL_CERT_DIR` it
  found.
- blaze needs no Rust and no rustup installed (#298). With neither, it downloads Rust 1.99.0
  once into `~/.blaze/toolchains` (rustc, cargo, the standard library and its source: 97 MB on
  macOS, 133 MB on Linux x86-64), laid out as rustup lays out a toolchain, and builds with it.
  Each file's sha256 is built into blaze from the release's published manifest, and a file
  with other bytes is refused. Downloads run together over blaze's own HTTPS client, unpack
  into a directory beside the toolchain's and are renamed into place, concurrent builds
  download once, and a download that stopped goes on from where it stopped. rustup's 1.99.0,
  when installed, is still the one used. `blaze build`, `run`, `test`, `check` and `stop` are
  `cargo blaze`'s commands without cargo; `blaze toolchain` says which Rust blaze builds with.
- install.sh installs blaze on a machine with no Rust (#298). It no longer runs rustup: after
  installing, blaze downloads its own Rust (none when rustup has 1.99.0), `--no-toolchain`
  leaves that to the first build, it says what to install when there is no C compiler to link
  with, and its last line is `blaze run`. `BLAZE_ARCHIVE` installs an archive from a file.
  install.yaml installs this commit's build in a Debian container with no Rust and runs
  `blaze run` in a project written by hand.
- `crates/blaze-resolve`, cargo's resolver owned by blaze (#298), starts with cargo 1.99.0's package
  model, used by nothing yet: `SourceId` (path, git and sparse registries, git at a branch, tag or
  rev with its commit, directory, local registry) compared by cargo's canonical URL and written as
  lock files, package ids and messages write it; `PackageId`, `PackageIdSpec` (`-p` and
  `cargo metadata`'s ids), `Dependency` with renames, kinds, targets and its locked or `--precise`
  requirement; `Summary` and its feature map, with the implicit features of optional dependencies
  and cargo's errors for features; and the `Registry` the resolver asks, with yanked and offline
  versions marked. Its tests include cargo's own for these types.
- `blaze-resolve`'s `Registry` is cargo 1.99.0's: `query(&self, ..)` is an `async fn` that
  awaits what it fetches, in place of answering `Poll::Pending` and a `block_until_ready` (#298).
- `blaze-resolve` reads and writes `Cargo.lock` as cargo 1.99.0 does (#298): every encoding
  from 1 (checksums in `[metadata]`) to 4 (git references URL-encoded), `[[patch.unused]]` and
  `[replace]`, the comments above cargo's header kept, and a lock file written back byte for
  byte. As in cargo, an unchanged lock file keeps its encoding, a changed one is raised to the
  workspace's default (from its lowest `rust-version`), under `--locked` or `--frozen` a change
  is an error, and the checksums of the old file must agree with the new resolve. Its tests write
  back the lock files of ripgrep and fd releases and of git workspaces cargo generated, one of
  each encoding, and port cargo's `lockfile_compat` tests; `BLAZE_RESOLVE_LOCKFILES=dir` adds a
  directory of lock files (24 releases of twelve projects, zed's 70 git sources among them,
  write back byte for byte). A registry read from a lock file before its manifest is now named
  as cargo names it.
- `crates/blaze-registry`, registries read and packages fetched as cargo 1.99 does, with no
  cargo, used by nothing yet (`docs/self-contained.md`, #298). The sparse
  index (crates.io's and any other's): `config.json` and its `dl` templates, the index paths,
  each line read as cargo reads it (schema `v`, `features2`, `rust_version`, `pubtime`,
  `yanked`, `links`, renamed and other-registry dependencies, unknown fields, and the lines
  cargo skips as invalid or unsupported), with cargo's cache of each file in cargo's format and
  its `If-None-Match` / `If-Modified-Since` requests, so cargo and blaze share
  `$CARGO_HOME/registry/index`. A `.crate` file is checked against its sha256, kept in the
  store by that hash and in cargo's cache, and unpacked where cargo unpacks it
  (`registry/src/index.crates.io-1949cf8c6b5b557f/<name>-<version>/`, the directory named by
  cargo's hash of the source), with cargo's checks (entries under `<name>-<version>/` only, no
  links, the size limit, the umask, `.cargo-ok`), several at once, retried as `net.retry`
  says, under cargo's package cache lock.
- `crates/blaze-manifest`, `Cargo.toml` and `.cargo/config.toml` read as cargo 1.99.0 reads
  them, without running cargo: `cargo metadata --no-deps` computed by blaze (its `metadata`
  example prints the same JSON, warnings and errors), on cargo's own manifest schema
  (`cargo-util-schemas` 0.14.3) ([resolve.md](docs/resolve.md)). `harness/resolve.py` compares
  it with cargo on the bench projects, 300 crates.io crates and 72 workspace repositories at
  pinned commits: all 385 equal, key order included. Nothing in blaze uses it yet.
- blaze-manifest reads `.cargo/config.toml` into cargo's own config types through a port of
  cargo's config deserializer: `[build]`, `[net]`, `[http]`, `[env]`, `[target]` (rustflags in
  cargo's precedence, runners and linkers), `[registry]`, `[registries]`, `[source]`
  replacement, `[resolver]` and `[profile]`, each field from its file or its `CARGO_*`
  variable, with cargo's errors and unused-key warnings. Its `config` example is
  `cargo config get`, and `harness/resolve.py config` compares the two on config trees and
  every corpus repository: 103 of 103 equal over 767 runs ([resolve.md](docs/resolve.md)).
- blaze-manifest computes profiles as cargo 1.99 gives them to units (`blaze_manifest::profiles`,
  a port of cargo's): `dev`, `release`, `test`, `bench` and custom profiles through `inherits`,
  package overrides, `build-override`, profiles from config files, `--config` and
  `CARGO_PROFILE_*`, `CARGO_INCREMENTAL` and `CI`, with cargo's errors and warnings, and checks
  each manifest's profiles as cargo does reading it. `harness/resolve.py profiles` compares
  every unit's profile with cargo's `--unit-graph`: 86 of 89 trees and repositories equal over
  546 runs and 235,924 units, none with a different profile, the other three refused by
  cargo or nightly-only ([resolve.md](docs/resolve.md)).
- `cargo ra-jit test` with `RA_JIT_TESTS=affected` runs, on Linux, each test that reaches a
  file, the environment, a process or the network in a sandbox of its own (#257): a process
  in new user, mount, network, IPC and PID namespaces, the machine's files read-only, the
  workspace and target directory overlaid so its writes vanish, an empty `/tmp`, loopback
  only, and a fixed environment. A seccomp supervisor records every path it touches; once it
  passes, what was there is kept, and it runs again only when its code, its environment or one
  of those changed, where it ran every time. `-v` names the file. Its output, panics,
  `#[should_panic]` and exit status stay libtest's, as ra-jit writes the harness. On ripgrep
  the tests run with no edit go from 493 to 24 of 1,168 (the 469 that ran every time are
  sandboxed and skipped); the first run, which runs those 469 sandboxed, takes 85.5 s where it
  took 83.6 s. `ra-jit sandbox` is the command, for build scripts and proc macros next (#147).
  `RA_JIT_SANDBOX=off` runs them every time as before; where no sandbox can be made (macOS and
  Windows for now, Ubuntu's AppArmor userns restriction) the command says why once.
- `vendor/proc-macro-srv`, the 1.99.0 release's proc-macro server with a loader that reads
  the list of macros ra-jit writes into a proc-macro library, built by ra-jit with nothing of
  rustc's. `RA_JIT_PROC_MACRO_COMPARE` and `RA_JIT_PROC_MACRO_LIBS` expand each call again
  with it and ra-jit's library of the crate and compare the output with the toolchain's;
  `harness/macroserver.py` holds serde_derive's, thiserror's, tokio's and clap's equal
  ([no-rustc.md](docs/no-rustc.md), step J1). A proc-macro crate's `fn main` is no longer
  taken for an entry point.
- `RA_JIT_PROC_MACRO_SERVER` naming that server has a load into ra-jit's own target directory
  link its proc macros with ra-jit and expand them with the server, the toolchain's server
  and rustc's libraries left for what ra-jit did not link (#90). `macroserver.py all` builds
  fd, bat, hyperfine, just, nushell and helix both ways: 3,465 calls the same, 12 of
  `pest_consume_macros` not compared, as ra-jit cannot compile it yet (#327).
- The Linux release has blaze's proc-macro server, `blaze-proc-macro-srv`, beside `ra-jit`,
  built whole by the release's ra-jit (`build_release.py --server`);
  `RA_JIT_PROC_MACRO_SERVER=1` uses it, and says so when it is not there. A load whose proc
  macros are all ra-jit's starts no toolchain server.
- The SSE and AVX compares compile (#87): `llvm.x86.sse.cmp.ps` and `.ss`, `sse2.cmp.pd` and
  `.sd` and AVX's 256-bit ones give a lane of all ones where the immediate's predicate holds, as
  rustc's do. glam's `Vec4::cmpeq` and `is_finite_mask`, which bevy calls, were left to rustc;
  `harness/breakers` checks each predicate, NaN lanes included, against rustc's program.
- `cargo ra-jit check --message-format=json` (and `short`, `json-diagnostic-short`,
  `json-diagnostic-rendered-ansi`, `json-render-diagnostics`) writes the daemon's verdict as
  `cargo check` does: each error a `compiler-message` with rustc's code, level, spans (bytes,
  lines, columns counted as rustc counts them, the source lines and their highlights) and
  `rendered`, cargo's package id and target, the failure notes, `build-finished` and the
  `could not compile` lines. rust-analyzer's check-on-save and editors read it, so they show
  ra-jit's verdict and not rustc's. `-Z`, `--exclude` and several `--target`s are the
  checker's too (a checker for each target, four held at once), where they went to cargo.
  `harness/check_json.py` compares it with `cargo check` on the same errors: 7 packages, every
  format, every compared field the same; the labels, help and notes, other spans and
  `code.explanation` are rustc's only, and the messages are rust-analyzer's wording.

- `RA_JIT_UNITS=1` compiles a whole program's functions many to an LLVM module, as rustc
  compiles a crate's codegen units (#176): the functions are found from their IR before any
  is compiled, cut into units of about 20,000 instructions in the order they were found in,
  and compiled on every core, each unit with the union of its functions' imports linked in.
  An import LLVM leaves a call of is kept as the code of its function, so the functions are
  found in one round, and each function's code is read out of the unit's object and put in
  the program's as a compile of it alone is. It is not the default: on 16 cores helix builds
  in 71.1 s against 69.7 s and runs 1.28x of cargo's release build against 1.21x, and LLVM
  takes four times rustc's time for code of the same size either way
  ([codegen-units.md](docs/research/codegen-units.md)). Imports are linked into a compile's
  module once, after being linked to each other two at a time, which took the lowering of a
  module of many functions from 23.7 s to 8.1 s summed over ripgrep's cores.

- An edit that needs a build in full no longer links in full when the daemon has no base of
  its own (it gave it up, or hasn't built it yet) (#159): on Linux the program is built onto
  the executable rustc linked and spliced into it whole, and the edits after it are patches of
  that. nushell's signature edit with its base given up at 2 GB is spliced in 1.5 s, and the
  edits after splice in about 10 ms. macOS still links such an edit in full, but no longer
  twice: the link takes the room for splices the executable there wanted. `rustcbase.py`
  checks that every build in full of the rustc phase is spliced whole on Linux, and
  `own-base-budget-full` a given-up base's.
- Const eval answers nightly's reflection, `core::mem::type_info` (#34): a type's variants
  and fields, a field's type, name and offset, a type's size, whether it is signed, an array's
  length and element, what a pointer points to, a variant's name, as rustc does, and two of
  its `TypeId`s compare. Bun's `MultiArrayList` asks it of every type it holds, which left 907
  of bun's functions to rustc; `harness/bun-repros/type_info` asks what it does and now
  compiles. A constant that holds a `TypeId` is still refused: the evaluator's ids are not
  rustc's hashes.
- Const eval answers every reflection intrinsic the pinned nightly's core declares
  (`core::intrinsics::reflection`), as rustc's const eval does: `type_id_size_of` and
  `type_id_variant_name` under their new names, and `type_id_type_of` (`Type::of`'s kind, a
  `dyn`'s principal and auto traits), `type_id_generics`, `type_id_function_ptr`,
  `type_id_non_exhaustive` and `type_id_variant_non_exhaustive`. `type_id_vtable` answers
  `None` where the solver rules the trait out and is refused otherwise: rustc proves it in a
  typing mode the solver here does not have. The `Type` of a tuple is refused, as rustc at
  the pinned nightly ICEs on it.
- The daemon's workers hold at most half of the machine's memory together, or what
  `RA_JIT_MEMORY_MAX` says (a size, or `off`). Over it, the worker asked longest ago is
  stopped; `cargo ra-jit status` shows the limit and what each worker holds. `docs/reports/SOAK.md`
  described the variable, but it was never brought over from the soak branch (#60).
- `RA_JIT_OTHER_TOOLCHAIN=1` has `cargo ra-jit` build with a rustc other than 1.99.0, for a
  project pinned to a nightly of the next release that 1.99.0 can't build (bun's
  `nightly-2026-09-15`). Without it such a build goes to cargo, as before, and says why. The
  builds of a nightly cargo are the daemon's: `-Z embed-metadata=no`, which it passes every
  crate, no longer sends them to cargo, and build scripts' output and the seeded target are
  found in its second build-directory layout (`soakrows.py`, "a project pinned to a nightly").
- The `bench-bun` workflow measures bun nightly on the benchmark machine (edits and runtime),
  in `nix develop .#bench-bun`: bun's nightly and what its own build of its C and C++ needs.
  Too slow for every push.
- `benchreport.py --name NAME` stores a project measured on its own in
  `docs/benchmarks/NAME-<os>-<arch>.json` (`runtime-NAME-…` for the runtime), and every README
  table and chart shows it as a project like the others, in the geometric means too, with a
  footnote naming the machine, commit and date it was measured at.
- `bench.py bun` times the edits on bun (body, signature, leaf, generic, new function) in a copy
  of the tree `bunprep.py` prepares, under bun's pinned nightly; bun is measured only when
  named.
- bun (its Rust port, 1.4.3: 1.25M lines in 103 crates, with a JavaScript runtime's C and C++)
  in `apps.json`, so `apps.py bun` and `runbench.py bun` build it whole-program and compare
  its runs with cargo's. An entry's `prepare` names a module of the harness that makes what
  the program's own build makes before cargo: `bunprep.py` runs bun's build of its C and C++
  once per revision and adds a member, `bun-exe`, that rustc links with them.
- `RA_JIT_MEMORY` reports after every request a worker answers too (`request-N`), and on
  macOS its resident figure is the physical footprint rather than 0. A `heap-profile` build
  writes where it was loaded beside each heap profile, which `harness/heapfold.py` needs to
  name the stacks on macOS, and `harness/heap.py` gives the bytes to the salsa query whose
  value or record holds them. What they found
  ([#39](https://github.com/indexable-inc/ra-jit/issues/39),
  [docs/research/daemon-memory.md](docs/research/daemon-memory.md)): nushell's worker holds
  10.0 GB live once ready, 2.7 GB of it what a debugger is told of types; ripgrep's 1.6 GB;
  and the spare worker the daemon starts after the first build holds 940 MB more on ripgrep.
  salsa's `lru` cannot evict `infer` or MIR (they have cycle handling), and on `described`
  it freed 206 MB of ripgrep's live heap and none of its footprint, so no eviction is on.
- Proc-macro expansions are kept in the store
  ([#38](https://github.com/indexable-inc/ra-jit/issues/38)): a derive or attribute macro's
  output is found again by the next daemon, keyed by the macro library's content, the
  macro, its input tokens and the shape of their spans, the environment and the directory.
  An expansion that asked the compiler anything, or failed, is not kept. Function-like macros
  are kept with `RA_JIT_PROC_MACRO_CACHE=all`, a macro is left out by name with
  `RA_JIT_PROC_MACRO_CACHE_SKIP`, and `RA_JIT_PROC_MACRO_CACHE=check` expands anyway and
  reports any that differ ([docs/cache.md](docs/cache.md), `harness/expansions.py`). On a
  warm start helix's 1455 kept expansions take resolve from 6.0 to 4.3 s and the time to
  ready from 9.6 to 8.0 s, nushell's 1588 from 5.8 to 4.5 s and from 10.1 to 8.8 s (Linux
  x86-64, 16 cores of an otherwise idle host, medians of four runs each); cold starts
  (helix 35.5 and 35.2 s, nushell 36.8 and 36.7 s) and memory do not change.
- `#[ra_jit::cache]` ([#17](https://github.com/indexable-inc/ra-jit/issues/17)): the
  `ra-jit-cache` crate's attribute keeps what a pure function returns, in memory and under
  `target/ra-jit/fn-cache`, by its arguments and a hash of the code ra-jit compiled for it and
  for everything it reaches. Comments, formatting and edits elsewhere keep the results; an
  edit to anything it reaches drops them, live edits too. Functions that read statics that
  change, thread-locals or the system (I/O, time, randomness) are refused with where that is
  written. Under plain cargo it does nothing. `cargo ra-jit cache clear` forgets what was kept
  ([docs/fn-cache.md](docs/fn-cache.md), `harness/fncache.py`).
- `harness/abbench.py` runs builds of ra-jit against each other on `bench.py`'s edits, in
  interleaved rounds: edit and cold-start times, the workers' cycles on macOS and their peak
  memory. With `--train` it is the training of the release build's PGO.
- rustc's MIR optimizations, optionally: `RA_JIT_MIR_OPT=1` runs what rustc runs on a debug
  build's MIR (copy propagation, removing unused locals, storage markers and drops of what
  needs none, folding constant branches, simplifying the control flow and instructions),
  `RA_JIT_MIR_OPT=2` what it runs above `-Copt-level=0` that is ported (also dead stores and
  unreachable blocks). Off by default. The interpreter's tests run every program at all three
  levels and require the same result ([docs/research/mir-gap.md](docs/research/mir-gap.md)).
- `ra-jit mir <dir> --crate <name> [--level N]` prints a crate's MIR as ra-jit compiles it,
  in rustc's `-Zunpretty=mir` format, and `harness/mirparity.py` compares it with rustc's
  function by function: at level 1, 21.5% of functions are identical to rustc's, against 4.0%
  without the passes.
- The README has a table of how fast programs ra-jit builds with LLVM run against
  `cargo build --release`, written by `bench.py --runtime` and `benchreport.py` and kept current
  by the `bench-readme` workflow on the Linux benchmark machine. A run in which any program's
  output differs from cargo's is never published. The first, on Linux x86-64, has every
  program's output the same as cargo's; on macOS arm64, ripgrep, fd, bat and jsonrt do not
  build whole yet (NEON intrinsics ra-jit does not have).
- [docs/research/](docs/research/README.md): what 2026's Rust compiler work means for ra-jit,
  and the MIR gap with rustc with the production gaps ranked; rustc's pass sources in
  [docs/upstream/rustc-mir-transform/](docs/upstream/rustc-mir-transform/SOURCE.md).

- Tiers ([#6](https://github.com/indexable-inc/ra-jit/issues/6)): a program that takes
  edits with `ra-jit-live` is sampled while it runs, and the functions it spends its time in
  are compiled again with LLVM, at `-O3` with what they call built in (level 2) and then with
  the branch counts level 2 took as `!prof` (level 3), and swapped in as patches without a
  restart. An edit sends an edited function back to Cranelift in the same build.
  `RA_JIT_TIER=off` turns it off. The Game of Life window shows each hot function's level
  and the promotions as they happen; `demo/lifebench` and `harness/tiers.py` measure and
  check it ([docs/tiers.md](docs/tiers.md)).
- `ra_jit_live::tiers()`, and the sampler that the daemon turns on in a running program.
  It stops once the worker that reads it is gone.
- Benchmarks on real projects that keep the README current: `harness/bench.py` measures
  ripgrep, fd, bat, hyperfine, just, axum and nushell at pinned commits (cold build, no-op,
  four kinds of edit, and how fast ripgrep's program runs) against incremental cargo, on both
  tiers, keeping every sample; `harness/benchreport.py` writes the README table and
  [docs/benchmarks.md](docs/benchmarks.md) from the result; the `bench-readme` workflow runs
  them on a dedicated Linux x86-64 machine after pushes to main and nightly, and commits the
  numbers to main when they moved beyond noise.
- `harness/bench.py` measures what [#7](https://github.com/indexable-inc/ra-jit/issues/7)
  asks of real projects: helix, a bevy example and zed (1.7M lines) join the seven, every project gets two more
  edits (a new function, a struct field set wherever the struct is built), the cold build is
  run `--cold-runs` times against fresh cargo, each with an empty store, and as many times
  in a second checkout with the store the first filled, the store's size is kept, and `--tests`
  runs each project's test suite under `cargo ra-jit test` and `cargo test` and fails on any
  test that ends differently. A project whose build fails keeps what it measured, with the
  error, and the run goes on to the next. A run with anything wrong or failed still writes its
  result and ends with exit code 3; `benchreport.py` publishes a test suite that ended differently in the tests
  table (a program that prints something else is still refused), and the workflow commits the
  numbers, then fails.
- `bench.py --setups` builds every step again with the fastest ways to build Rust there
  already are ([#8](https://github.com/indexable-inc/ra-jit/issues/8)): mold, wild,
  `-Zthreads=8`, rustc_codegen_cranelift on a pinned nightly, all three at once, and
  `cargo check`, each into its own target directory; `harness/hotpatch.py` times an edit of
  a running program under Dioxus's subsecond (`dx serve --hotpatch`) against `cargo ra-jit
  build` and `cargo build` with a restart, stopping the program before the build as `cargo
  watch` does. The bench shell has mold, wild, `dx` and the nightly, on Linux and on macOS.
  Measured on a MacBook (`docs/benchmarks/hotpatch/macbook.json`): subsecond 346 ms
  median, `cargo ra-jit build` with a restart 469 ms, `cargo build` with a restart 520 ms; a
  live patch through `cargo ra-jit build` 12.5 ms and through the editor's socket 4.0 ms
  (`docs/benchmarks/live/macbook-probe-cargo.json`). On Linux, 16 cores of a c7i.16xlarge
  (`docs/benchmarks/hotpatch/linux-x86_64.json`): subsecond 289 ms, `cargo ra-jit build`
  with a restart 40 ms, `cargo build` with a restart 361 ms. On ripgrep's cold build mold,
  wild, `-Zthreads=8` and cg_clif together are fastest (3.25 s against cargo's 5.56 s), and
  every edit there and on nushell builds faster under ra-jit than under any of them, `cargo
  check` included; with nothing changed, on ripgrep, cargo with mold is faster (28 ms against 33).
- Benchmark history and regression alerts
  ([#10](https://github.com/indexable-inc/ra-jit/issues/10)): `harness/benchhistory.py`
  appends each run's medians to `docs/benchmarks/history/*.jsonl` and checks the last against
  the runs before it on the same machine; the `bench-readme` workflow commits the history on
  every run, starts the site's deploy, and on a regression past the noise band opens (or
  comments on) an issue labelled `bench-regression` and fails. The live-edit latency workflow
  runs every night on main on a macOS arm64 runner and does the same with its history.
- [ix.dev/blaze/bench](https://ix.dev/blaze/bench): the benchmark history over commits, from
  `docs/benchmarks/history`: the latest run's table per project, and charts of each edit,
  the cold build, the daemon's peak memory, the store size and live edit-to-running latency
  ([#10](https://github.com/indexable-inc/ra-jit/issues/10)).
- LLVM is given what rustc gives it: reference parameters marked `noalias`, `readonly`,
  `dereferenceable`, `align`, `nonnull` and `noundef` as rustc marks them, addresses as
  pointers rather than integers, branches threaded before `-O3`, unsigned overflow checked as
  rustc checks it, a second round of cleanup after `-O3`, and, at a level above 1, the bodies
  of the callees it keeps calling and of what those call. Level 3 of `demo/lifebench` runs
  at 88% of a release build and 94 to 96% of one that checks overflow as the dev profile
  does, from 71 to 74% and 77 to 81%, measured side by side on a loaded machine
  ([docs/tiers.md](docs/tiers.md)).
- `harness/tierbench.py`, the benchmark table of docs/tiers.md; `--together` runs the builds
  of a round at once, so that a loaded machine slows them alike.
- `RA_JIT_DUMP_LLVM=after` prints the LLVM IR as the passes left it.
- Live-edit latency under real typing ([#9](https://github.com/indexable-inc/ra-jit/issues/9)):
  `harness/live_latency.py replay` types scripted edits one key at a time at 30 to 100 ms a
  key, with pauses at word ends, typos and backspaces, sends the whole file on each key as an
  editor does, and times every key whose text compiles to the program running it, by patch or
  by a restart as a launcher would do one; it counts the keys that do not compile, the
  superseded edits and the restarts. `restart` times a session's struct changes against
  `cargo run`. Every table has p95 and p99, and `LATENCY_JSON` carries the machine, its
  battery, the commit and the run, which `benchhistory.py live` reads. `harness/live-bevy`
  (bevy 0.20, generic systems) and `harness/live-egui` (egui 0.37, generic widgets) are real
  programs that take edits. Measured on a MacBook Pro in
  [docs/live.md](docs/live.md#latency-under-typing).
- A borrow checker, the crate `hir-ty-borrowck` in the rust-analyzer subtree, written to give
  rustc's verdict: Polonius alpha by default as nightly rustc does, NLL with
  `RA_JIT_BORROWCK=nll`. It gives rustc's errors on
  1541 of 1581 of rustc's own borrow-check tests that it can run, and none on any of the 246
  rustc accepts, scored by `harness/borrowck_ui.py`, and finds nothing in ripgrep, fd, bat,
  hyperfine, just, axum, nushell, delta, eza, tokei, starship and cargo. Its errors, and rustc's
  `unused_mut` warning, are rust-analyzer diagnostics and `ra-jit borrowck` prints them ([docs/borrowck.md](docs/borrowck.md)).
- Item well-formedness, the crate `hir-ty-wfcheck` next to the borrow checker: rustc's E0309
  and E0310 outside function bodies, for struct, enum and union fields (`&'static T` without
  `T: 'static`), a trait impl's associated types, an impl of a trait bounded by `'static`,
  and the where clauses of ADTs, impls, traits and functions. Its errors are refused as the borrow checker's are.
- The daemon refuses to build an edit with a borrow-check error, as it does one with a type
  error; `RA_JIT_BORROWCK_REFUSE=0` turns that off. The checker still misses about a third
  of rustc's borrow-check errors, so `cargo check` stays the final word.
- `ra-jit borrowck`: the borrow-check errors of single files, or of a cargo workspace with
  `--project`.
- Inlining across crates, on by default in a program built whole with LLVM at `-O2` or
  `-O3`: it has what it calls in every crate built into it or shown to LLVM, the standard
  library's generic and `#[inline]` functions as rustc does across crates.
  `RA_JIT_INLINE=crate` keeps that to the function's own crate, `RA_JIT_INLINE=all` does it
  in other builds too. On Linux x86-64 (AWS c6i.16xlarge, at 13ee5a5339) the geometric mean
  of 14 runtime benchmarks was 1.15x the time of `cargo build --release` at `-O3` and 1.16x
  at `-O2`, against 3.89x with each crate alone; the programs other than the kernels took
  2.3 to 3.7 times as long to build as with each crate alone (ripgrep 30.1 s against 8.5 s).
  The README's table, measured at the merge into main, has 1.21x over 15 benchmarks
  ([README](README.md#how-fast)): numeric's ra-jit build took 57 ms in three of its five
  runs and 32 ms in the other two, where release's took 31 ms in all. A whole program's
  build from cold against `cargo build --release`'s from cold: ripgrep 31.7 s against
  8.4 s, tokei 50.7 s against 13.2 s, bat 125 s against 53.3 s, eza 38.9 s against 34.6 s.
  Commit 99cbb902ad's message
  says the levels alone turned it on; until b460bbe327 they did not, and it took
  `RA_JIT_INLINE=all`.
- `blaze-resolve` resolves as cargo 1.99.0 does (#298): its resolver is cargo's, ported, with
  backtracking, the conflict cache, `links`, prereleases, yanked versions only when locked,
  `[replace]`, and versions preferred by `rust-version` or lowest first; the errors are cargo's.
  Its tests are cargo's `resolver-tests`, the 37 of `resolve.rs` and the 20 of `validated.rs`
  (whose check against a SAT solver is left out). cargo's unstable `-Zdirect-minimal-versions`
  and publish-age policy are not ported.
- `blaze-resolve` picks features as cargo 1.99.0 does (#298): the members `-p`, `--features`,
  `--all-features` and `--no-default-features` select, by resolver 1's rules (`--features` for
  the package in the current directory, `member/feat` for another) or resolver 2's, with
  cargo's errors and suggestions; and cargo's feature resolver, which under resolver 2 keeps the
  features of build dependencies and proc macros apart from the rest, leaves out those of
  dev-dependencies when nothing is tested, and of dependencies for platforms not built for, and
  enables `dep?/feat` once `dep` is. A package's targets and the platforms' `cfg` come from
  whoever calls it. Tests take cases of cargo's `features2`, `weak_dep_features` and
  `package_features` tests.
- `blaze-resolve` writes the `resolve` of `cargo metadata` as cargo 1.99.0 does (#298): each
  package reached from the members, its dependencies by the name its code uses, their kinds and
  platforms, and its features, filtered by `--filter-platform`. The test compares the JSON with
  cargo's for a workspace with a renamed dependency, one used as both normal and dev, a build
  dependency and a `cfg(unix)` one. `harness/resolve_graph.py` compares, over the corpus of
  `harness/resolve`, the `resolve` blaze-resolve makes of the packages cargo lists with cargo's,
  with each project's lock file and with none.
- `blaze-resolve` resolves a workspace as cargo 1.99.0 does around its resolver (#298): the
  resolve `Cargo.lock` is written from (every member, every feature), guided by the lock file,
  then the one asked for, guided by the first, with cargo's `PackageRegistry` applying
  `[patch]` and locking each summary to the previous resolve. So a lock file keeps the versions
  it names, and the graph is cargo's where the two resolves differ, as when a patched member
  is reached both as itself and as the patch. `harness/resolve_graph.py` resolves each
  project so, from the lock file it ships or none: 322 of 324 equal with it and 381 of 385
  without (from 292 and 362), the rest projects cargo itself does not resolve there.

### Changed

- Sandboxed tests are an experiment a workspace opts into: `experimental = ["sandboxed-tests"]`
  under `[workspace.metadata.blaze]` in its root manifest, or `tests = true` under
  `[package.metadata.blaze.sandbox]` in a package's. Elsewhere `cargo blaze test` runs tests as
  `cargo test` does, so a test that writes `$HOME` passes where it failed with the sandbox's
  denial. A test outside the sandbox has no record of what it read, so `RA_JIT_TESTS=affected`
  keeps no pass of it and it runs on every commit; when no test of the command is sandboxed, a
  note says what to add and to which manifest. `experimental` is the one list of opt-in
  features: an unknown name, or a value that is not a list of names, is an error naming the
  manifest and the known names. `tests = false`, `unsandboxed-tests` and `RA_JIT_SANDBOX=off`
  work as before; build scripts stay sandboxed and proc macros WebAssembly. blaze's own
  workspace opts in (docs/cargo.md, "Sandboxed tests (an experiment, opt-in)").
- The programs print `blaze`, not `ra-jit` (#270, "What the programs print"): the `blaze:`
  prefix of their notes, `Finished ... profile [blaze]`, `blaze is enabled`, `plain cargo:
  through blaze`, `blaze could not compile`, `blaze cache gc` and the other errors and notes.
  The harnesses that read these lines read the new ones. The binaries, crates, directories,
  `RA_JIT_*` variables and symbol names keep their names for now.
- A build script's `RUSTC` is blaze, so its probes run no rustc: `rustc -vV` and `--print`
  are blaze's answers, and a probe compile (`autocfg`, `anyhow`, `thiserror`, `rustix`,
  `proc-macro2`) gets `cargo blaze check`'s verdict, or an error naming the command where blaze
  can't give one. cargo's `-Clinker-features=-lld` no longer sends a question to rustc
  (docs/no-rustc.md, step I).

- The check of an edited file asks rust-analyzer only for the bodies of the functions written
  otherwise since its last check, when that check found nothing wrong and the file is the same
  but for bodies nothing else sees into (`outline::skeleton`): a body's errors are its own.
  Every body is asked for otherwise. On a 13,000-line `main.rs` of 2,200 functions the check
  of a one-function edit takes 32 ms, from 123 to 165 ms (1 of 2,201 bodies asked), and the
  edit 90 ms, from 183 to 221 ms; a type error in the edited body is still refused. On a
  19,000-line `main.rs` of a real program (clap, serde) the whole-file check was 2.5 to 3 s
  of each 3 s edit.
- LLVM's inliner runs at threshold 500 (was the level's 225 or 250): the runtime set's
  geomean goes from 1.091 to 1.080 of rustc's (tokei 1.28 to 1.26, ripgrep 1.14 to 1.11) for
  2 to 5% more code and no more compile time; docs/tiers.md has the sweep (225, 500, 1000, and
  nothing built in by blaze). `RA_JIT_LLVM_INLINE_THRESHOLD` is in the store's key.
- The LLVM backend is on LLVM 23.1.4 (was 20.1.8), our fork's `ix/23.x` at the commit
  wasm-host's ix-aot pins, so both run one LLVM: inkwell 0.11 and llvm-sys 231. Blaze
  builds that LLVM itself for each target (`.github/workflows/llvm.yaml`) and pins the
  archives by sha256 in `crates/hir-ty-codegen/llvm.json`; the flake, CI and Windows fetch
  them. It is linked in statically, with libstdc++ on Linux, so a binary built with
  `--features llvm` loads no libLLVM. llvm-sys links it (`vendor/llvm-sys`), so GNU ld sees the
  archives after inkwell, which calls them. LLVM 21 removed `nocapture`: an `sret` parameter is
  `captures(none)`, and an attribute name the linked LLVM does not know now panics naming it
  rather than crashing LLVM's verifier.
- The site moved to [ix.dev/blaze](https://ix.dev/blaze/). https://ra-jit.ix.dev and
  https://blaze.ix.dev answer every path with a 301 to the same path and query under
  https://ix.dev/blaze, so old links (the README's updates button, `/bench`, RFCs) keep
  working. Every page, script, stylesheet, image, narration file and form sits under `/blaze`
  (`Served` on `site` in `site/content/site.flecs`); pages carry a canonical URL and Open Graph
  tags, `/blaze/sitemap.xml` lists them, and `/blaze/install.sh` serves `install.sh`.
- The site says blaze, not ra-jit: page titles, the top bar, `og:site_name`, the home page, the
  charter, the RFCs, `/bench` and `/updates`, and their narration, voiced again. ra-jit is left
  only where it is still the literal name: the `ra-jit-live` crate and `#[ra_jit::cache]`, and
  the line that blaze was called ra-jit until 2026-10-24 and `cargo ra-jit` still works.
  Comments keep their pages and blocks (ids unchanged).
- No `cargo blaze` command is passed to cargo any more (Andrew, 2026-10-26: commands that are
  no builds "should go to us and not cargo"). `metadata`, `locate-project`, `verify-project`,
  `version` and `help` are blaze's own, from blaze-manifest, blaze-registry and blaze-resolve
  with no cargo or rustc started; every other command of cargo's (`fmt`, `clippy`, `doc`,
  `tree`, `publish` ...) fails with status 101, saying blaze has none yet and that nothing was
  done, and a command cargo does not have is `no such command` rather than a cargo plugin run.
  `cargo blaze --list` and the table in docs/cargo.md ("Every subcommand") say which is which;
  flags typed before the command are read after it.
- `cargo blaze test` is built whole by blaze too, with no first build by cargo and no rustc:
  blaze's resolution plans what cargo would test (each member's library, binaries and
  integration tests with `test = true`, the examples built beside them, and the binaries an
  integration test runs, `CARGO_BIN_EXE_<name>`), or what `--lib`, `--bin`, `--example` and
  one `--test` name. The unit tests are built by a worker that loads each tested crate twice,
  its copy of tests alone with `cfg(test)` as rustc's `--test` has it, the rest by a worker
  that loads it once, and each test runs as before, in its package's
  directory and in the sandbox. A program of tests compiles libtest (`test`, and what only it
  links) with its own crates, as the library built apart leaves it out and no rlib of the
  toolchain is linked. A unit test with `harness = false`, several `--test`s and the
  other target flags fail the build and say so. The tests run in cargo's order, a package's
  library first, and each run asks for what each test reaches, so `RA_JIT_TESTS=affected`
  picks as before. The doc tests (and `--doc` alone) are run by blaze with no cargo, rustdoc
  or rustc: blaze writes down for each library the run of rustdoc cargo would make, and one
  run loads the workspace and builds every example. An example blaze cannot build (one
  rust-analyzer finds an error in, one that mixes copies of the library, one that calls what
  blaze cannot compile) fails, saying why, where rustc built it before; a `compile_fail` one
  passes when rust-analyzer finds an error in it, and fails, saying so, when it finds none.
  `harness/doctests.py` counts those apart from the examples that end otherwise than
  cargo's, and ra-jit is no longer cargo's `RUSTDOC`.
- `cargo blaze build` and `run` have one path, and it runs no rustc: each program is built
  whole by blaze, as `RA_JIT_WHOLE_BUILD=1` had it, and the setting is gone. No first build
  by cargo, no rustc base under the first edits, and no fallback: what blaze cannot compile,
  plan (`RA_JIT_OWN_RESOLVE=0` no longer asks cargo for the programs) or build yet fails the
  build and says what (the targets `--bins`, `--tests` and their kin pick, several
  `--target`s, `--target-dir`, `--message-format`, `-Z`, `--build-plan`). A build of
  libraries alone is the daemon's check of them. The cold build is slower than cargo's for
  now (docs/self-contained.md has the figures). blaze's link gives the program a GNU build ID on Linux, by which it takes live edits. On macOS a program finds the statics and functions of the standard library blaze built once by their names without Mach-O's leading underscore, as it asks for them, so std's `GLOBAL_PANIC_COUNT` is no longer refused as having no symbol. A patch compiles its own copy of a function of the program the link left out, as LLVM does one it inlined into every caller, instead of asking for it by name and restarting the program. A
  patch the splice declines (one that calls what the link left out as unreached) has the
  program built in full and linked, where the link of the object alone left the edit out. A
  workspace with no lock file, or one its manifests no longer agree with, has it written by
  blaze's resolver as cargo would, when no registry package is needed at a version the lock
  file lacks (#321). `harness/live_fallback.py`, `harness/aheadready.py` and
  `harness/renamedstatics.py`, of cargo's first build, are gone; `harness/unreached.py` and
  `harness/storetracked.py` take a program built in full where its edit calls what the link
  left out.
- A whole build's cold compile no longer waits on itself where a hundred threads meet. What
  the store is given of a function names each function and static it reaches by a name its
  crate is listed to find again: each crate is listed once, by the first thread to ask and
  outside the lock the others take, a name with types for arguments is not looked for in any
  listing, a name that is exactly one root of the program is taken as that root (a callee's
  lifetimes erased), and the statics are listed by the threads compiling, beside the compile,
  not by every thread at once after it. The threads compiling ahead also warm the values of the
  statics and the places of the panics the object thread asks for, are woken one for each new
  job rather than all for each, and the object thread waits on a condvar of its own. A crate's
  content id is hashed once, the type sets debuginfo tells of are sharded by environment, each
  thread keeps the values it interned in front of the shared shards, and the frozen parts are
  encoded on every core (their line says how long writing took). nushell on dev-aws-14,
  `harness/coldbuild.py bench`: the store's entries are made in 0.84 s where they took 1.6 to
  1.8 s (the threads' wall in them 48 s where 310 s), the frozen parts written in 1.3 s; the
  cold build is 47.9 s (cargo 24.0 s), still waiting on single long jobs and on roots found one
  after another, which [no-rustc.md](docs/no-rustc.md) (K) records.
- blaze builds with one Rust, the latest nightly: `nightly-2026-10-25` (rust-lang/rust
  12433c307), pinned in `harness/toolchain` and moved with `harness/toolchain_bump.py`, in
  place of stable 1.99.0. `vendor/rustc_abi` and the proc-macro server are taken from that
  commit, the answers to rustc's questions are that nightly's, and `TypeId` hashes, the abort
  intrinsic and the `core::arch` LLVM intrinsics follow it. A crate with `#![feature(...)]`
  builds like any other (`harness/live-alloc` runs a program on `allocator_api` with live
  edits); deref patterns are lowered as `rustc_mir_build` lowers them; every `-Z` flag of the
  nightly is ignored, modelled or refused by one table (`-Zthreads` is ignored); and cargo's
  second build-directory layout (`build/<pkg>/<hash>/{out,fingerprint,run}`) is followed
  beside the first.
- The first edit after a daemon starts on the store no longer compiles every function from
  the store in the edited file to tell what changed. Each entry names the items of the
  checkout its code comes from, by a hash of their text: the body it is written in, the
  bodies whose places it tells (with where each begins), those it mentions a coroutine of,
  and those that give an `impl Trait` in it its type. After an edit of nothing but bodies,
  code whose items read as they did is taken as it was (the timeline's `asked:` line says how
  much). warmstart.py's restart of ripgrep, dev-aws-7: the patch asks 17 functions where it
  asked 382, and the edit takes 0.22 to 0.23 s where it took 0.30 to 0.32 s. nushell's asks
  18 where 156 and takes as long, 0.53 to 0.70 s: what is left on both is the code that
  changed, compiled on a cold database (main before #360 compiled everything at the start and
  took 0.09 s and 0.37 s, its start 4.8 s longer on nushell)
  ([warm-start.md](docs/research/warm-start.md)). A stored mark no longer hashes where the
  named locals are kept for a debugger, which a full build and a later compile of the same
  code can tell apart, and which had every edit after a restart compile such code again.
  Entries are kept under new keys. `storetracked.py` has the edits of one body take some
  code by its items, a daemon that checks everything find none of it changed, and the
  closure an `impl Trait` is change.
- A closure's stable name is its place among the closures of its body, as rust-analyzer
  interns it, and a hash of its source, where it was the index of its expression, which an
  edit above it moved (one with no source, as an `async fn`'s coroutine, keeps that). What
  calls a closure does not say what it takes, so the hash names a closure that takes another's
  place apart from it. An edit above a closure renamed it and every instance generic over
  it, and a patch compiled again each of those that came from the store: the first edit
  after warmstart.py's restart of ripgrep patches 19 functions where it patched 176 (nushell
  17 where 66), and takes 0.26 s where it took 0.28 to 0.29 s (nushell 0.54 to 0.56 s where
  0.57 to 0.58 s). A patch says how many instances it compiled that the base had no code
  for. `storetracked.py` holds the edits of a generic function's body and of a line above a
  closure to none, and has a closure added above another, which took that one's name and was
  called with its code until the hash, rename it.
- A function whose code calls rustc's copy of a function of the checkout is kept in the
  store, and so is what calls it (#216). Such a copy is of a small function that is not
  generic, which rustc compiled into the crate that calls it, as ripgrep's `Match::start`
  in `rg`; the entry names the function and the copy, and the build that takes it from the
  store calls the copy as one that compiled the caller would. A restart of nushell with
  nothing changed (warmstart.py, dev-aws-5, two interleaved rounds) compiles 17 functions
  where it compiled 25,844, finds 7,622 of its 12,559 roots in the store where it found
  4,888, and takes 19.7 to 19.9 s where it took 22.8 to 22.9 s; ripgrep's compiles none
  where it compiled 2,375 and takes 4.25 to 4.33 s where it took 4.61 to 4.68 s. The first
  edit after such a start takes longer (nushell 0.58 s where 0.33 s, ripgrep 0.29 s where
  0.08 s): a closure is named by where in its body it is written, so an edit above it
  renames it and what is generic over it, which a patch compiles again when it came from the
  store. `storetracked.py` checks that a restart keeps such calls and that an edit of the
  copied function reaches the callers from the store.
- Proc macros are blaze's by default where its server is beside `ra-jit`: the daemon's
  loads, `--check` and build scripts expand with libraries ra-jit makes from each crate's
  record, kept in the store by what they were linked from, so another target directory takes
  them from there (#90). `RA_JIT_PROC_MACRO_SERVER=0` goes back to rustc's libraries and the
  toolchain's server. `macroserver.py all` builds each app again over the first store: no
  library linked again, the same 3,465 calls.
- The first `cargo blaze build` of a workspace is blaze's, not cargo's: once cargo's build
  succeeds it waits for the daemon to load the workspace ("Waiting for blaze to load the
  workspace") and build the programs, where it handed back cargo's and the next build was the
  first the daemon made. On a dev host (16 cores, 2 runs) it ends at 6.1–6.7 s on ripgrep,
  where cargo's ended at 5.4–6.7 s and the daemon was ready at 5.9–7.5 s, and at 41.0–41.9 s on
  nushell, where cargo's ended at 36.6–37.9 s and the daemon was ready at 41.6–42.5 s. The
  daemon still patches what rustc linked by default: building its own base instead
  (`RA_JIT_RUSTC_BASE=0`) has it ready at 7.5–10.3 s on ripgrep and 58.0–58.5 s on nushell,
  as it compiles every function of the program again after cargo.
- `cargo blaze` builds with Rust 1.99.0, the release it compiles, whatever toolchain the
  project names (#269). A project pinned to another release, or on a machine whose default
  is another, no longer has every build go to cargo: cargo, rustc and the daemon run under
  1.99.0 (`RUSTUP_TOOLCHAIN` and `CARGO` name it), and plain `cargo` keeps the project's
  toolchain in the same target directory, which cargo keys by the rustc. Code that only the
  project's own toolchain builds (a nightly's `#![feature]`, a newer release's library) fails
  with rustc's error and a note naming both releases. Without 1.99.0 or its `rust-src`, the
  build fails saying the `rustup` command that installs it, where it went to cargo. A
  changed toolchain file no longer sends the next build to cargo. `BLAZE_OTHER_TOOLCHAIN=1`
  still builds with the project's toolchain. `cargo_ra_jit.py` builds and edits projects
  pinned to an older stable and to a nightly, through toolchains whose rustc compiles nothing.
- A daemon that links what rustc built beside its own code (every cargo daemon) takes the
  checkout's roots from the store too, where only a build with nothing of rustc's did. A
  build spliced into rustc's executable still compiles them. ripgrep's `restart`
  (warmstart.py, dev-aws-7, three interleaved rounds) compiles 2,377 functions where it
  compiled 3,618 and takes 4.96 s where it took 5.09 s; nushell's compiles 25,843 where it
  compiled 30,039 and takes the same 25.7 s, as finding its roots in the store (0.8 s) costs
  what it saves while most are refused for calling rustc's copy of a function of the checkout
  ([#216](https://github.com/indexable-inc/blaze/issues/216)). `harness/storetracked.py` checks
  that a start found roots.
- The store is keyed by what ra-jit was built from, not by the bytes of its executable
  ([#105](https://github.com/indexable-inc/blaze/issues/105)): `crates/ra-jit/build.rs`
  hashes the workspace's crates, the vendored ones, the manifest, lock file, cargo config and
  `harness/toolchain`, the features, the target and whether debug assertions
  are on. Another build of the same sources, on another machine or after a commit that
  changes only harnesses or docs, finds what the first kept: `harness/storebuilds.py` builds
  its workspace into an empty store, then starts a copy of ra-jit with other bytes, which
  finds 66 entries and compiles 0 of the 701 the first added, where a build keyed by its bytes
  found 0 and compiled all 701. A ra-jit built outside the workspace keys by its bytes as
  before. A crate fetched by cargo is named by its place below `CARGO_HOME`, not by where
  `CARGO_HOME` is, so its frozen def map and functions are found from another home. A build
  script's run is keyed with its package's directory and the target directory apart, and
  what it printed and wrote is put back with this checkout's; the key still hashes the
  script's executable, which differs between target directories, so a second checkout still
  runs its scripts (none restored in warmstart's `warm` step on ripgrep or nushell) until a
  script is keyed by what it was built from.
- A daemon that starts on a store holding the checkout's code no longer compiles that code
  again before the first edit can be patched ([#105](https://github.com/indexable-inc/blaze/issues/105)).
  Each function of the checkout goes into the store with a mark of its code and of what it
  calls and reads, by stable names, so the patch base takes what it found as it is and an
  edit recompiles only what it touched; what an edit leaves no caller of (a closure renamed by
  a line above it, and what is generic over it) is left uncalled, as an instance is. nushell's
  `restart` (warmstart.py, dev-aws-5, `RA_JIT_RUSTC_BASE=0`) goes from 30.8 s to 21.4 s, the
  73,586 functions and 9.4 s of "compiling what the store had of the checkout" gone, and its
  `warm` step is ready in 15.4 s where it took 25.3 s; ripgrep's `restart` from 5.67 s to
  4.63 s. The first edit after is a patch as before (nushell 0.30 s, was 0.23 s; ripgrep
  0.08 s). A program linked with rustc's code, an LLVM build in units and
  `#[ra_jit::cache]` functions keep no marks and still take that build. `harness/storetracked.py`
  holds it.

- ra-jit is now **blaze**. The command is `cargo blaze` (`cargo blaze build`, `test`, `run`,
  `check`, `status`, `stop`); `cargo ra-jit` keeps working and, on a terminal, says once per run
  that it was renamed. The crate on crates.io is `cargo-blaze` (`cargo install cargo-blaze`),
  release archives are `blaze-v<version>-<target>.tar.gz`, and the flake's package and app are
  `blaze` (`ra-jit` stays as another name for them). Every setting can be given as `BLAZE_<NAME>`
  as well as `RA_JIT_<NAME>`, and the `BLAZE_` one wins. `--version` says `blaze`. The engine
  binary, the crates it is made of, the daemon's directory and the store keep their names, so an
  upgrade keeps what the store holds.
- `install.sh` installs blaze on macOS (Apple silicon) and Linux (x86-64, ARM64) with one line,
  `curl -fsSL https://raw.githubusercontent.com/indexable-inc/blaze/main/install.sh | sh`: the
  newest release (or `--version`), checked against its SHA256SUMS, into `~/.cargo/bin`, with Rust
  1.99.0 and `rust-src` through rustup. The README starts with it; the `install` workflow runs it
  on each platform.
- A build with a toolchain other than 1.99.0 says how to switch the project to it
  (`rustup override set 1.99.0`).
- A start on a warm store with nothing of rustc's (`RA_JIT_RUSTC_BASE=0`) is no longer held up
  by the store and the interner (#41): each store entry and set of types is read once however
  many threads ask for it, a value already interned is found under the shard's read lock, the
  walk of the types an instance mentions goes into each type once (a future's type reached the
  same few along exponentially many ways), and an edit does not wait for the store to have what
  the build before it kept. `harness/warmstart.py helix` on Linux x86-64 (dev-aws-7,
  c7i.16xlarge, 64 cores, one run each, main `c48ca3f800` against it with this change): a
  restarted daemon's build on a store that has everything takes 38.3 s instead of 106.5, its
  "store read" 130.8 s of thread time instead of 561.1, the first edit's rebuild 12.2 s instead
  of 45.5, and a second checkout is ready 59.0 s after its build instead of 113.0; nushell's
  takes 35.4 s instead of 37.8 ([research/warm-start.md](docs/research/warm-start.md)).
- `unused_mut` a source denies fails the build, with the seven deny lints before it (#84).
  `harness/lints_ui.py --deny unused_mut` scores it on every rustc UI test: 51 right, 0 wrong,
  5 missed against the nightly, 49, 0, 5 against 1.99.0. `RA_JIT_BORROWCK_REFUSE=0` turns it
  off with the borrow checker's errors. `fn f(mut self)` gets it too, where none was given.
- Each debug type is described once, by itself, and the types made of each other are kept
  together as one group that every set of debug types reaching them shares
  ([#29](https://github.com/indexable-inc/ra-jit/issues/29)); the DWARF is the same. On Linux
  x86-64 (16 cores, `memsplit.py`, one run each) the daemon after its own base and edits holds
  7.36 GB instead of 7.63 on nushell, 5.87 instead of 6.55 on helix and 14.47 instead of
  15.97 on bun, at most 7.9, 6.1 and 15.8 GB at an edit instead of 11.7, 9.8 and 22.9, and the
  first edit after the base takes 0.37 s instead of 2.06 on nushell, 0.19 instead of 1.71 on
  helix and 1.51 instead of 7.23 on bun. zed's own base, which passed 64 GB, finishes with no
  bound of its own at 62.6 GB in 662 s; at the default bound, a quarter of memory, it is still
  given up (#188).
- A whole program's compiles under LLVM import what they call, as ThinLTO's backends do,
  where each built its callees again from MIR and optimized them from scratch. Each function
  a compile calls is lowered and run through LLVM's pre-link pipeline once in the process,
  and a compile links in the bodies small enough to be built in (100 instructions, 300 for
  `#[inline]`, the whole budget of 40 000 for `#[inline(always)]`), and those of what they
  call with 0.7 of the room each call deeper, 8 deep, as `available_externally`; what LLVM
  does not build in is dropped. Bun on 16 cores (c8a.24xlarge) builds in 118 s where it took
  564 s, against 135 s for cargo's release build, and runs 1.244x cargo's where it ran 1.267x.
  `RA_JIT_IMPORT=0` builds the callees in as before.
- The daemon keeps what each debug type is told as once, rather than in every set of debug
  types it is in ([#55](https://github.com/indexable-inc/ra-jit/issues/55)); the DWARF it
  writes is the same. On Linux x86-64 (16 cores, `memsplit.py`, one run each) the daemon
  after its own base and edits holds 7.5 GB instead of 9.7 on nushell and 6.4 instead of 11.3
  on helix, and bun's own base, which was given up at a quarter of a 124 GiB host's memory,
  is built at 18.9 GB, so its signature edit is a 6.3 s patch instead of a 32.8 s full link.
  The own base asks the allocator to give back what it freed before it gives up for memory.
- ra-jit allocates with jemalloc on Linux and macOS, and with mimalloc on Windows only
  ([#39](https://github.com/indexable-inc/ra-jit/issues/39)). mimalloc 3 kept more of what a
  build freed, transparent huge pages included: on Linux x86-64 (16 cores, `bench.py`, two
  interleaved rounds) the daemon after its edits holds 1.57 GB instead of 2.03 on ripgrep, 9.52
  instead of 10.80 on nushell, 11.07 instead of 12.13 on helix and 24.3 instead of 27.0 on bun,
  with the same edit times. `RA_JIT_MEMORY` reports the live heap without the `heap-profile`
  feature, which now only adds jemalloc's profiler.
- The daemon no longer keeps a spare worker loaded for the next project
  ([#56](https://github.com/indexable-inc/ra-jit/issues/56)). It starts one as it starts, for
  the build that started it, and one each time a build of a new workspace begins, which loads
  the workspace beside cargo as before; none waits once a workspace is loaded. On Linux x86-64
  (16 cores, 3 runs), the daemon after the edits of `bench.py`: ripgrep 3.63 to 2.31 GB, fd
  3.42 to 2.04 GB, nushell 12.70 to 11.36 GB, with the same edit times; an idle daemon 2.64
  to 1.28 GB. A second project's cold start is as before (ripgrep 4.74 and 4.86 s, nushell 30.5
  and 30.7 s); a warm one, which cargo had built, loads the library first: ripgrep 1.29 to
  2.62 s, fd 2.19 to 3.48 s, nushell 8.62 to 9.51 s (`harness/spare.py`, 3 interleaved rounds).
  `RA_JIT_SPARE=ready` keeps one loaded as before; `RA_JIT_SPARE=0` still starts none.
- With LLVM, `memcmp` is called by its own name in the function's module rather than as one
  of the functions asked for, so LLVM writes a comparison of a few bytes it knows the number of
  as loads, and one only for equality as `bcmp`, as it does for rustc. On Linux x86-64 (AMD
  EPYC 9R45) tokei calls the C library's comparisons 155.9 million times instead of 278.6
  million (cargo's release build: 155.1 million) and runs at 1.34 times the release build
  instead of 1.51. words' machine code is the same, 48 bytes further on.
- With LLVM at `-O3` the bodies of the functions a function calls are built into it before
  the default pipeline, and the branches on what they return threaded and the values
  numbered again, as is done for what Cranelift builds in: a loop over `i + 1..n` inside one
  over `0..n` counted with `phi + 1` before, which alias analysis can't see past. On
  Graviton4 (Linux aarch64) nbody runs at 1.28 times cargo's release build instead of 1.97,
  and words at 1.27 instead of 1.36; on Linux x86-64 (c8a.metal) words at 1.28 instead of
  1.37. Whole-program builds take 1-4% longer. x86-64's nbody is still 1.27: LLVM 20 weighs
  the five-body nest above its full-unroll threshold, and rustc's LLVM 23 below it.
- On macOS a patch whose unwind frames are larger than the program's unwind table is spliced
  rather than linked in full: both of the unwinder's tables move into the room the link leaves
  after its load commands, the program's frames where the table has them and the patch's
  behind. ripgrep's first edit (244 KB of frames, a table of 218 KB) is a patch there too
  (`harness/rustcbase.py`'s `moved-panic` and `cached-reached`).

- After a fresh build through cargo the daemon is ready for edits once it has loaded the
  workspace and read the symbols of the executable rustc linked, instead of after building
  every program once more itself ([#11](https://github.com/indexable-inc/ra-jit/issues/11)):
  the first edits are patches spliced into rustc's executable, and the daemon builds its own
  base in the background once nothing has waited for a second. On Linux x86-64 (16 cores),
  ready after a cold build from an empty store: just 10.4 to 8.9 s, ripgrep 7.8 to 6.6 s,
  nushell 58.8 to 47.0 s, against 7.4, 5.0 and 35.1 s for plain `cargo build`; from a filled
  store 10.7 to 8.7 s, 8.2 to 6.6 s and 64.1 to 46.3 s. That is 0.25 to 0.5 s after the
  command returns on just and ripgrep, and 3 to 4 s on nushell, which reads the symbols of
  233 000 functions. On macOS, nushell 43 to 36 s against 25 s for cargo. The command itself
  is as slow as before: the spare worker that loads the workspace beside cargo still costs
  1.0 s on just, 1.3 s on ripgrep and 8 to 9 s on nushell.
  The first edit over rustc's executable is slower than over the daemon's own build, since
  it reads the edited file's functions and resolves its names for the first time: just 0.08
  to 0.4 s, nushell 0.3–0.45 to 1.6–1.9 s. An edit that is more than function bodies, of a
  generic, `#[inline]` or default trait function or one that moves such a function's lines
  (rustc copies those into their callers, line numbers and all), of a static, or of a file
  whose functions were not read before is built in full, and the log says why
  (`no patch: ...`): ripgrep's `main` sits above a generic function, so its first edit takes
  1.4 to 1.7 s.
  The own base is done some 2.5 s after ready on just and ripgrep and 11 s on nushell.
  `RA_JIT_RUSTC_BASE=0` keeps the old behaviour. On macOS rustc's link of a program is given
  `-Wl,-headerpad,0x400000` and `-Wl,-bind_at_load`, which the daemon's own links leave out.
  `harness/rustcbase.py` checks every kind of first edit against cargo, and
  `harness/coldprof.py --max-wait S` fails when the daemon is ready more than S seconds after
  the command.
- `cargo ra-jit build` from cold is closer to cargo's, and readiness follows it sooner
  ([#11](https://github.com/indexable-inc/ra-jit/issues/11)). The daemon tells the spare
  worker what cargo built from a thread of its own and the spare reads its input from the
  start: before, a spare still loading the standard library let the pipe fill, which held up
  the daemon and with it cargo, whose output went unread. While cargo builds, the spare
  resolves the standard library and compiles the functions of each crate cargo is done with on
  a quarter of the cores, leaving the generic instances they reach to the edits, reads the
  symbols of each library as cargo writes it, and once cargo is done compiles nothing more
  before it is ready. The first edit reads the edited file's functions on every core. An edit
  of a generic, `#[inline]` or default trait function, or one that moves such a function's
  lines, is now a patch too when every copy rustc made of it is found in the executable: the
  patch replaces the copies, and ripgrep's first edit is a patch. Thread-locals of a patch
  over rustc's executable are reached as rustc's code reaches them (on Linux x86-64 the
  general-dynamic sequence becomes local-exec, on macOS through the variable's descriptor).
  On Linux x86-64 (16 vCPUs, 8 cores), interleaved runs from an empty store, command and
  ready: just 8.5 to 8.2 s and 9.0 to 8.6 s, ripgrep 6.6 to 5.7 s and 6.9 to 5.9 s, nushell
  43.5 to 37.6 s and 46.7 to 39.2 s, against 7.6, 5.4 and 35.7 s for plain `cargo build`. The
  first edit: just 0.36 to 0.5 s, ripgrep 1.7 s built in full to 1.5 s patched, nushell 2.0 s
  as before. The command's remaining cost is the spare's own: what it resolves of the
  standard library and the workspace takes cores from cargo (docs/cargo.md).
- An edit in a package whose build script cargo runs again for it (one that names no
  `rerun-if-changed`, as ripgrep's, fd's and hyperfine's) no longer waits for a
  `cargo check` that runs every build script of the workspace: the daemon runs that one
  script again as cargo ran it and compares what it prints and writes with what it loaded,
  as `cargo ra-jit` already did, and asks cargo only when the two differ, the script is older
  than its source, or it was compiled reading a variable cargo did not set. A script
  `cargo ra-jit` ran again for the same build, with nothing it reads written since, is not
  run a second time. On Linux x86-64 an edit of ripgrep's `search` goes from 72 to 48 ms,
  fd's `run` from 78 to 42 ms and hyperfine's `run` from 80 to 35 ms.
- ra-jit is built one way, whether downloaded or built by nix: the new `dist` profile (thin
  LTO), optimized with a profile of ra-jit's own work (PGO) trained on edits of fd, bat, just
  and hyperfine. `.github/scripts/build_release.py` is the recipe; the release workflow runs it
  for every target and the flake's `daemon` (so `nix run`, `nix profile install` and the demo)
  runs it inside its derivation. The profiles are trained by the new `pgo` workflow, which
  runs again whenever Cargo.lock, a manifest or the toolchain changes, and pinned by sha256 in
  `pgo/profiles.json`, so every build uses the same file; the build says how much of the code
  changed since its profile was trained, and a release refuses a profile of other crates.
  `cargo install --locked --profile dist ra-jit` builds the same profile without PGO, which
  starts about 10% and edits about 3% slower than a release's binaries: cargo takes no compiler
  flags from a crate, and crates.io's crates are not the ones a profile can be trained on.
  Against the plain `release` build, on Linux: the
  cold start 14% shorter on nushell and on ripgrep, an edit of a function 8% shorter on
  nushell, the same peak memory. mimalloc v3 stays the allocator: jemalloc, mimalloc v2 and
  snmalloc start colder and edit no faster (an edit spends 1.8% of its time allocating, 73%
  revalidating salsa's memos). `release` itself is unchanged and stays the build to work with,
  so rebuilding ra-jit after a change takes as long as before
  ([docs/reports/RELEASE-BUILD.md](docs/reports/RELEASE-BUILD.md)).
- Under `RA_JIT_BACKEND=llvm`, a body of more than 10 000 Cranelift instructions is compiled
  by Cranelift. Since LLVM lowers a handler that takes more than the exception (tiers), it
  compiled every function, and an edit of nushell's `main` (35 778 instructions) waited
  229 ms for LLVM: on Linux x86-64 the edit went from 343 ms to 472 ms, and is 280 ms now.
  65 of nushell's 160 177 functions are over the limit, none of ripgrep's or of
  `demo/lifebench`'s. A tier still compiles such a body with LLVM.
- `vendor/rust-analyzer` and `vendor/rustc_abi` are git subtrees instead of submodules: a
  plain `git clone` builds, and the flake builds from its own tree with no second pin of the
  forks.
- The first build of a workspace through cargo has the daemon load it and compile ahead
  while cargo builds ([#11](https://github.com/indexable-inc/ra-jit/issues/11)): a spare
  worker reads the workspace and compiles what it can while cargo runs, hears what the
  build scripts said and where the proc macros are, and is the worker the build is handed
  to. On nushell, Linux (32 cores): ready for edits 58 s after a cold build from an empty
  store instead of 71 s; cargo's own build takes about 3 s longer, 40 s instead of 37 s,
  while the worker loads beside it.
- A build through cargo asks the toolchain's `rustc` beside `cargo` for its version and
  sysroot, not the rustup proxy: 16.6 ms to 7.3 ms each of its three calls on Linux.
- An edit to function bodies only lists no functions again and finds the program's tests
  as they were: 5.5 ms to 0.5 ms of an edit of nushell's `main`.
- `RA_JIT_STATS` shows where a splice and the refusals before a link spend their time
  (`splice-us`, `link-us`, `refuse-us`).
- `harness/realbench.py --pause S` waits once the daemon is ready, as a person would before
  the first edit.
- `vendor/subtree.py` replaces `vendor/subtree.sh`. `push` sends what main changed under
  `vendor/` to the fork, one fork commit per commit of main, and only fast-forwards the fork;
  the `fork sync` workflow runs it on every push to main, so the forks no longer fall behind
  (rust-analyzer's had: "warm edits" never reached it). `upstream` merges an upstream
  rust-analyzer release into `vendor/rust-analyzer` in one command, with conflicts left in
  the tree as `git merge` leaves them; before, that took a clone of the fork and a pull.

- Code ra-jit gives LLVM is faster: SIMD intrinsics on 128-bit lanes of integers (add,
  subtract, bitwise, saturating, compare, `simd_bitmask`) are vector instructions instead of a
  loop over lanes, and LLVM's own; a slot whose address is rounded up to an alignment above 16
  (`__m256i`) is an alloca of that alignment, which LLVM can take apart; `memcpy`, `memmove`
  and `memset` are LLVM's intrinsics rather than calls it knows nothing of, so a copy of a
  known size is a few loads and stores.
- A program built whole runs faster with LLVM: a landing pad whose cleanup only resumes is
  left out, so LLVM sees a call that cannot unwind into anything; what a check that is off
  (`ub_checks()` and its kind) guards is not compiled, so its panics do not use up what may be
  built in; a `match` is a chain of
  comparisons of up to 256 that LLVM makes a switch of; bodies are built in and shown up to
  16 calls deep (an iterator's adapters reach `Vec::push` that deep); `offset` is a
  `getelementptr inbounds`, also once Cranelift has inlined it; and a body built to be
  measured and then not used gives back what its build counted of its call's share, so that
  `vec![false; n]`'s `from_elem` is shown to LLVM, which then knows the vector's length and
  drops the sieve kernel's bounds checks. A body shown to LLVM carries its `#[inline]`,
  `#[inline(always)]` or `#[inline(never)]` as rustc emits it. An address the code computed
  as an integer (from `ptrtoint`s, through adds, phis and selects) is rebuilt after the passes
  as pointers and `getelementptr`s on the pointers it came from, so that LLVM can follow what
  is stored and loaded through it: nbody's array of deltas leaves the stack. On Linux x86-64 the geometric mean at `-O3` goes
  from 1.20x (ra-jit f3eeb5295e) to 1.15x of release's time at 13ee5a5339.
- On macOS arm64, ripgrep, fd, bat and jsonrt build whole: NEON's pairwise maximum, minimum
  and add, its table lookups (`tbl1`, `tbx1`), `sqdmulh`, `uaddlp` and `saddlp`, the CRC-32
  instructions, and the generic `simd_reduce_max`, `simd_reduce_min` and `simd_ctpop` are
  compiled, lane by lane or as one instruction. On a MacBook Pro (Apple M5 Max, macOS 27.0.1),
  median of three runs: ripgrep 1.15x, fd 1.01x, bat 1.16x and jsonrt 1.26x of release's
  time, every output the same as release's.
- x86-64's `simd_select_bitmask` (AVX-512's masked blends), `simd_fma`, the AVX-512 two-table
  permutes (`vpermi2var` of bytes, words, dwords and qwords), SSE4.1's `phminposuw` and
  `vzeroupper` are compiled, lane by lane, which nushell's dependencies reach for
  (corpus `simd_x86`).
- AArch64's SHA-1, SHA-256, SHA-512 and AES instructions (`vsha256hq_u32`, `vsha512hq_u64`,
  `vaeseq_u8` and the rest, as sha2 calls them) are compiled, each as a function of the one
  instruction, and NEON's sums of every lane made wider (`vaddlvq_u8`) and doubled products
  made wider (`vqdmull_s16`) lane by lane. `simd_reduce_add_ordered` and `_unordered` (and
  `mul`) are compiled, which `vaddv_u8` and AVX-512's `_mm512_reduce_add_epi32` are, and
  `simd_masked_load` and `simd_masked_store` (AVX-512's `_mm512_mask_loadu_epi8`, in fff_search
  under nushell): a lane the mask leaves out is not read or written (corpus `crypto_lanes`).
- A daemon started on a filled store finds the functions it has stored faster: looking an
  entry up takes a lock of its own shard rather than one for all of them, and the types of an
  entry's variables are read when the function is described for a debugger, on the threads
  that describe, rather than when the entry is looked up. nushell's restart on dev-aws-14
  (64 cores, shared), five rounds each, interleaved with main's: 15.6 s (15.5 to 15.7) on main, 14.7 s (14.5 to 14.7)
  with this; the stored roots take 1.3 s, from 3.5 s, and the object 1.8 s, from 1.0 s.
- The def maps and impls of crates that use procedural macros, and those of the crates of
  procedural macros, are frozen in the store like any other dependency's, so a build reads
  them rather than resolving their names again. A macro whose library is not loaded, or is
  turned off, is so only in that process, and its crate is not frozen. nushell's restart
  resolves all 434 of its dependencies' def maps from the store, in 1.8 s rather than 3.1 s,
  and takes 12.6 s (12.5 to 12.7) with the entry above, against main's 15.6 s.

### Fixed

- `Self::Variant` in a pattern of an `impl` whose `Self` type is an alias or projection to an
  enum (`impl Deserialize<E, D> for <E as Archive>::Archived`, as `#[derive(rkyv::Deserialize)]`
  writes) resolves the variant instead of failing MIR lowering with `unresolved variant`.
- A closure that captures nothing, coerced to `fn` and borrowed as part of a constant, is
  promoted as rustc promotes it: minijinja's `type_erase!` vtable, so `tract-linalg`'s build
  script runs. blaze lowered it as a temporary of the frame, so the reference outlived it: the
  borrow checker said E0515, and without it the call through the table jumped to 0.
  `harness/breakers` has the case (`promoted_closure_table`).
- A proc-macro server that dies or fails an expansion fails the build with what it said, the
  macro and its library (`ra_jit_expand::server_failed`), at the end of the cold build and on
  every build after. Before, the build went on without the expansion and failed far from it:
  fd with a server killed after its first five derives said `fd::run: frontend: mir:
  MissingFunctionDefinition`, as `Opts::parse` had no `#[derive(Parser)]` impl to resolve to;
  now it says the server failed to expand `Parser` of clap_derive, and how it exited.
- A build after `cargo blaze clean`, `cargo clean` or `rm -rf target` while the daemon runs
  builds, by the same worker (#721, reported by @maddada). The worker read the standard
  library's unit it had written under `target/ra-jit/whole/` again, and every build until
  `blaze stop` failed "a build of src/main.rs panicked: the standard library blaze built:
  reading .../<name>.std.o: No such file or directory". Before each build the worker now
  writes the unit again when it is gone or another file than it wrote (from the store when it
  has it), and builds a program in full when its object is gone; a unit it can't read is an
  error, not a panic (`harness/cargo_ra_jit.py`, `cleaned`).
- blaze finds the toolchain behind Homebrew's rustup. Homebrew installs `rustc` as a script
  that starts rustup, beside no toolchain, and blaze took a proxy only by the name `rustup`,
  so the script's directory was read as a toolchain with no standard library: every build
  script failed with "the toolchain has no source of the standard library". A `rustc` that is
  not in the `bin` of a sysroot with `lib/rustlib` is now a proxy, whatever it is called.
- A crate blaze builds itself (a proc macro, a build script, or what either links) reads the
  variables its build script set with `cargo:rustc-env`: `mime_guess`'s
  `MIME_TYPES_GENERATED_PATH`, so `rust-embed-impl` builds. blaze kept only cargo's own
  `CARGO_*` and `OUT_DIR`; it now reads the names from the script's output, `output` in cargo's
  layout of `target` and `run/stdout` in the build-dir layout.
- An integration test that names a program (`env!("CARGO_BIN_EXE_app")`) builds when its
  build starts the daemon's worker: the worker was started with the variables of its build
  but not the places cargo set, so its load, which compiles before the first request is read,
  failed `` `env!("CARGO_BIN_EXE_app")` in it names a variable that is not set ``. A cold
  `cargo ra-jit test --test it` failed every time; under plain cargo after `ra-jit enable`, the
  `cargo test` after an edit failed when the edit's load won the race (the release train's
  plain-cargo row "`cargo test`, edited").
- A daemon that patches what rustc linked no longer stops at the first build that reaches a
  thread-local, or a static written at run time, of a crate edited before any build reached
  it (``static `__RUST_STD_INTERNAL_VAL` of crate `smid` with state, whose first value is not
  the one rustc built it with``, `harness/programs.py` on every platform since #527). The first
  values of a crate's statics with state are noted just before the first edit reaches it, so
  rustc's copy is still known to start from them, and a change to a file an `include_str!` or
  `include_bytes!` reads leaves the first values of crates that do not include it known (it
  had made `alloc`'s `STATIC_INNER_SLICE` unknown after a checkout switch,
  `worktrees.py --join --together` on macOS).
- A build that code generation refuses (a static whose first value is not known to be rustc's,
  a function rustc's copy of is from before the edit) is that build's refusal, with the reason,
  not the daemon's end: every checkout's daemon had died with it
  (`worktrees.py --join --together`, "another edited while it is broken"). Nothing is built in
  its place, and the next build of that program is a whole one.
- A spliced edit no longer runs a closure's old code (#582). A closure is named by its place
  among the closures of its body, so after an edit that moves the call of a generic function
  such as `caught(|| ...)` from one closure into the body, the patch called the base's
  unchanged copy of `caught::<{closure#N}>`, and through it the base's code of the closure
  there before: nothing the patch compiled had reached that closure's new code. A patch now
  compiles what such an unchanged copy reaches that the edit changed, and refuses when that is
  code which gained or lost a body of its own (`harness/fuzz/found/edits/closure-called-again.rs`;
  `fuzz/edits.py` seed `push1`, round 27, had 7 wrong checksums and has none).
- A whole build of other features or flags no longer leaks into the next: every worker linked
  its programs into one `target/ra-jit/whole/<name>`, so after `cargo blaze run --features
  loud` a plain `cargo blaze run`, whose worker had nothing edited to build again, installed
  the loud program (`HELLO WORLD`). Each worker now links in a directory of its own, named by
  what it is loaded with (`harness/cargo_ra_jit.py`, flags group).
- Three programs that ran differently from rustc now run the same (#73): a diverging type
  variable falls back to `!` in every edition, as rustc at the pinned nightly does, not to `()`
  before 2024; `if let` in a closure captures only the places its pattern moves, as `let` and
  `match` do, not its whole scrutinee; and a value assigned through `*(&mut x)` after a `match`
  that ruled `x` out is dropped, since a place borrowed `&mut` may hold a value from then on
  (rust-lang/rust#90752). `tests/ui` `editions/never-type-fallback.rs`,
  `closures/2229_closure_analysis/run_pass/if-let-capture.rs` and `drop/issue-90752.rs` are
  pinned.
- `cargo blaze test --doc` runs an edited library's examples again: it took an example's
  program from the run before while the example's own text was unchanged, since blaze's run
  names no library with `--extern` for the key to stamp, so after an edit that broke them the
  doc tests passed where cargo's failed. The key now has the cfgs of every crate the library is
  built from and the files of those of the workspace. `cargo blaze test --doc -q` shows the
  results tersely, as cargo's `-q` has libtest do (`harness/cargo_ra_jit.py`, flags group).
- The daemon's own link passes the linker what `-C link-arg` and `-C link-args` say in
  `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS` or the configuration's `rustflags`, after the native
  libraries as rustc does; it left them out. A program linked with
  `-Wl,-no_compact_unwind` had `__unwind_info` all the same, and a `-Wl,-headerpad` was not
  the least room the splice's header file asks for (`harness/nocompact.py`,
  `harness/headerpad.py` on macOS).
- A red train opens or comments on the "Release train is red" issue again: train_issue.py read
  each harness job's log with `gh api`, which the runner's gh refuses without
  `--allow-escape-sequences`, so the `train issue` job failed and no issue was opened.
- A build that compiles the standard library's crates itself no longer takes from the store
  code that a build over ra-jit's prebuilt unit of the library kept: such an entry's calls
  into the unit (`ra-jit.<hash>.…` symbols) are now checked when it is taken, where its link
  failed on them as undefined (#328). Entries written before are not read (entry format 12).

- A load's proc macros with and without `RA_JIT_PROC_MACRO_SERVER=0` (#460): each mode has
  its own target directory, `target/rust-analyzer` with ra-jit's proc macros and
  `target/rust-analyzer-rustc` without, so flipping the variable no longer has rustc read the
  empty libraries a load with ra-jit's proc macros gives cargo (E0786, "memory map must have a
  non-zero length"). The variable is part of which build and which worker it is, read from
  the command that asks, so it takes effect without `blaze stop`; a spare worker no longer
  keeps the daemon's. A load whose proc macro has no `blaze-proc-macro-srv` beside ra-jit
  says where it looked, the command that builds it there, and `RA_JIT_PROC_MACRO_SERVER=0`.
- A test that runs cargo runs in the sandbox: every cargo takes its locks in its home and
  records there what it used (`.package-cache`, `.global-cache`), which the read-only machine
  refused, so trybuild's tests and a cargo plugin's own failed. Cargo's home (`CARGO_HOME`, else
  `~/.cargo`) is now an overlay, as the workspace and target directory are: the test writes it
  for itself alone, and nothing of it is kept (docs/cargo.md, "Every test runs in a sandbox").

- The borrow checker: lifetime elision counts none of the impl's lifetimes in a parameter
  that writes `Self`, as rustc counts only what a parameter's type writes or elides, so
  `fn store(self, db: &dyn Db) -> &Store` in `impl Id<'_>` returns a borrow of `db`. It
  gave "lifetime may not live long enough" there (`InferBodyId::store_and_root_expr`, which
  failed blaze's own build of `hir-ty`), and at a call took the result as borrowing nothing.
  blaze's own tree: 10 false errors to 2.
- The borrow checker's implied bounds: a trait object's `'r` outlives what a supertrait
  requires of `Self` (`dyn 'db + SourceDatabase` that `'db: 'static`, through salsa's
  `ZalsaDatabase: Any`), and the types in its arguments imply what they do
  (`&dyn Fn() -> &'a G<'db>` that `'db: 'a`), as rustc's well-formedness walks into them.
  The last 2 false errors in blaze's own tree (`module_for_assoc_item_loc`, `path_to_const`).
- On macOS an edit is spliced into the program the daemon linked itself, not built in full.
  That link (`link.rs`, the one with nothing of rustc's) asked for neither fixups in tables nor
  room after the load commands, so `cc` linked for the Mac it runs on (macOS 26) with fixups
  in chains, and the splice refused every edit ("an executable whose addresses are fixed up in
  chains", then "the header has no room for two more segments"); each edit compiled the whole
  crate again. It now asks for both as rustc's line does, the room through the same
  `.headerpad` file. A one-function edit of a 13,000-line `main.rs` of no dependencies takes
  0.17 s, from 0.36 s.
- An `extern` item whose `link_name` starts with `\u{1}`, the symbol as the object has it
  (bindgen writes `"\u{1}_aws_lc_0_44_0_SHA384"` for macOS), is declared without the target's
  global prefix, which the object writer puts back: it was prefixed again, so the call named
  `_\u{1}_aws_lc_0_44_0_SHA384` and a program using aws-lc-rs (rustls, reqwest) failed to link
  on macOS when blaze linked it ("symbol(s) not found ... __aws_lc_0_44_0_SHA384").
- The LLVM backend: an error LLVM diagnoses while it writes an object is an error naming
  what LLVM said, not an object with an instruction left out. LLVM 23's assembler printed
  "can't encode 'ah'" for `mov ah, sil` in an `asm!` block and wrote the object without it
  (LLVM 20 ended the process). A `reg_byte` in a block that names a high byte register is one
  of a, b, c and d, which always encodes (corpus case `inline_asm::high_of`).
- The LLVM backend's store: a function's key has the `asm!` blocks LLVM puts in place of its
  calls of their wrappers. Two functions that each pass a `u64` through a block of their own
  were one key, and the second got the first's code.

- On macOS the doctests are ra-jit's, as on Linux
  ([#255](https://github.com/indexable-inc/blaze/issues/255)). Every example was left to rustc
  for a writable static of std (`CLEANUP`, `GLOBAL_PANIC_COUNT`) "without a rustc symbol": the
  standard library's unit was weakened like every object ra-jit writes, which makes each name
  of Rust's a private external that `defined_symbols` does not count as defined, so the example
  found nothing of the unit, compiled std's startup itself and stopped at its first writable
  static. The unit is linked with nothing of rustc's and gives way to nothing, so it is written
  as it is; one an older ra-jit kept in the store is not taken. `harness/doctestonce.py` no
  longer accepts that reason on macOS.

- A guarded match nested inside another match arm's guard preserves the enclosing guard's
  binding references, including with or-patterns. Later reads in the outer guard no longer
  use uninitialized by-value bindings, which could produce wrong results or a crash.
- A whole-program build gives the same object every time ([#200](https://github.com/indexable-inc/blaze/issues/200)).
  Functions went into the object in the order the compile-ahead threads finished them, so
  their names, the ids of what they call and their places in the program changed from one
  build of the same source to the next; the layout alone moved a benchmark by 8%. The object
  is now given roots and instances in the order they were asked for, the thread that puts
  them in it waiting for the next in line while the others compile on, and the threads take
  instances in that order too. `harness/wholeprogram.py` builds each case twice and holds the
  objects to the same bytes.
- `cargo blaze doc` documents the crates cargo does: of two of one name the one cargo keeps
  (none of a proc macro's two `syn`s), nothing only a left-out one or a test needs. It links
  a crate not documented at its `html_root_url`, finds blanket impls of every crate linked and
  applies them as solved in the documented crate, leaves out more provided methods that
  cannot apply, warns of a broken link where rustdoc does (lint levels, `cfg_attr`, no
  warning in another crate's macro), and documents what a macro expands to past
  rust-analyzer's token limit (biome_wasm's 2271 types). Against `cargo doc`, biome goes from
  93.5% to 99.0% of the pages, helix from 98.2% to 99.0% of the members and impls; docs/doc.md
  has all five bench apps.
- The borrow checker reports nothing on helix (14 false errors before) or biome (534), as
  rustc does; ripgrep, fd and nushell stay at none, and the UI scores are unchanged (Polonius
  1545 of 1585, NLL 1542 of 1583). It follows rustc where it did not: the outlives
  components of a trait object's projections and of fn pointers; an opaque's arguments
  related by what it captures; `use<>` on an `impl Trait` inside the return type; implied
  bounds through a projection and from a type's inferred outlives; a trait object's default
  lifetime not counted by lifetime elision; one lifetime for an alias's elided lifetime
  wherever the alias uses it; a closure's annotated references and the pointer fn items
  coerce to as higher-ranked; promotion by the value, not its type, with `Freeze` seen
  through projections.

- A macro call in parentheses or brackets with no `;` after it has to expand to one
  expression, as rustc parses it ([#154](https://github.com/indexable-inc/blaze/issues/154)):
  `twice!(twice!(n += 1;));`, whose outer call expands to two such calls with nothing between,
  built and ran where rustc reports an error. A function with such a call whose expansion is
  statements is now refused with the reason, as one with a call that failed to expand is.

- `cargo install --git https://github.com/indexable-inc/blaze ra-jit` builds ra-jit
  ([#242](https://github.com/indexable-inc/blaze/issues/242), #293, by @gakonst). It failed
  before compiling anything: for a git source cargo checks that every package is a member of
  the workspace above it, and the root claimed `vendor/rust-analyzer`'s crates and rowan, which
  are members of the fork's own workspace. The root now excludes `vendor/rust-analyzer`, so the
  fork's crates are tested there (in `vendor/rust-analyzer`, as
  `harness/unittests.py` does), and that workspace patches salsa to `vendor/salsa*` as the root
  does.

- A local that no place of its body mentions has no stack slot. rust-analyzer's MIR gives a
  closure's body a local for each capture and for each binding of the function it is written
  in, and a closure called by reference never touches them; each had a slot, so a closure
  over a large value had room for it twice in every frame. rgb's audio callback captures a
  synthesizer of 120 KB, and cpal's two closures around it each reserved 240 KB, overflowing
  CoreAudio's 512 KB IO thread; they now reserve what rustc's do. Breaker `captured_frames`.

- An integration test or benchmark the daemon runs has `CARGO_BIN_EXE_<name>` of each program
  of its package set, as cargo sets it since rust-lang/cargo#16421. With cargo's second layout
  the test is no longer beside the programs, so hyperfine's and bat's tests (assert_cmd's
  `cargo_bin`) found none and every one failed under ra-jit.

- The `TypeId` of a type that holds a closure, `async` block or `async fn` future of a crate
  rustc compiled is rustc's: its `DefPath` (`{closure#n}` under the function, as its symbols
  are named) with all of its arguments, the kind, signature or resume type and captures
  included. It was refused, as was every function that asks for one: tonic's
  `Streaming::new` builds its body from `body.map_frame(|frame| ..)`, and axum-core's
  `try_downcast` asks for its `TypeId`. An `async` closure's future and an `async gen` one
  are still refused.

- The `TypeId` of a type that holds a constructor used as a function (`stream.map(Ok)`,
  `iter.map(Some)`, a tuple struct's name) is hashed as rustc hashes it, as the `Ctor` child
  of the variant or struct with the ADT's parameters
  ([#369](https://github.com/indexable-inc/blaze/issues/369)). It was refused, and with it
  every edit that rebuilt a function giving `axum::body::Body::from_stream` such a stream,
  and tonic's clients, which map requests through `Ok`.

- On x86-64, an edit that replaces a function shorter than a jump (5 bytes) is spliced
  instead of built in full ([#242](https://github.com/indexable-inc/blaze/issues/242)).
  The jump goes over the `int3`s and `nop`s the linker padded the function with. When there
  is no padding, as for a getter packed against the next function, the function gets a
  2-byte `jmp` to a run of `int3`s within 127 bytes, which jumps on. biome's leaf edit
  (`JsFormatOptions::quote_style`, whose patch also replaces `line_ending` and
  `trailing_commas`, 4 bytes each) goes from 360.7 s and a 26.4 GB peak to 0.50 s (dev-aws-5,
  16 cores, cranelift, 1 run; cargo 12.1 s). `harness/rustcbase.py` `short` and `hop` hold it.
- A function a build left to rustc, and an instance of a function generic over nothing that
  did not compile, keep rustc's copy when no edit since the daemon started reached its crate
  or the crates below it. Each was taken for stale when a build in full left it to rustc for
  the first time, and the build went to cargo ([#242](https://github.com/indexable-inc/blaze/issues/242)).
  With the jumps above, biome's generic edit (`format_array_node`) is the daemon's (204.5 s;
  it went to cargo after 190.9 s, refused on `biome_analyze::profiling::is_enabled`) and its
  struct edit is spliced in 0.51 s, where it went to cargo after 176.7 s (dev-aws-5, 16 cores,
  cranelift, 1 run).
- A patch asks rustc's executable whether it has a function of a name once per name, where
  each asking searched every symbol of it. biome's body edits after the first take 1.6–1.7 s,
  where they took 2.2–2.8 s (cargo 2.3 s).
- On Windows x86-64 (MSVC), `cargo blaze build` and `run` rebuild through the daemon and print
  `[ra-jit]` instead of handing every build to cargo. The daemon takes COFF programs (also with
  `--target x86_64-pc-windows-msvc`), links them with the `rust-lld` the repository's cargo
  configuration names, and closes an existing directory of its own that others can enter, as
  on Unix.
- A profile with `debug = "line-tables-only"` (or `1`, or `"limited"`) no longer has the type
  of every variable worked out. Only the lines were written, but the types were described for
  each function compiled and then thrown away. On Biome (`biome_cli`, 518 crates, whose dev
  profile is `line-tables-only`), 0.1.0's worker went past 44 GB in its cold build and was
  killed before it was ready. On main, the cold build's peak goes from 40.4 GB to 30.6 GB and
  its time from 139.5 s to 98.8 s, with 726,516 `told_type` memos down to none.
  `harness/debuginfo.py`'s cost part checks that no type is told of at `line-tables-only`. What
  such a build keeps in the store, which has no variables, is kept apart from what a build with
  them keeps, so a `debug = true` build later takes none of it; the cost part builds at
  `line-tables-only` and then at `true` from one store and checks that it has every variable
  one at `true` after another at `true` has.
- A registry crate with a build script has the same frozen def map in every checkout (#105).
  Its key held the absolute `OUT_DIR`, so another checkout of nushell took 221 of 478 crates
  frozen where a restart of the first took 374, and of ripgrep 41 of 51; both now take all of
  them (374 and 51). The key holds `OUT_DIR` from cargo's build directory on and the files
  written there, and the bytes name a file below the `OUT_DIR` of any crate of the closure,
  and a literal such as the path `include!(concat!(env!("OUT_DIR"), ..))` reads, from that
  `OUT_DIR`, so another checkout reads what its own build scripts wrote; bytes that still name
  a target directory are not written. `frozenwarm.py` builds a second checkout with such a
  crate, the first's target directory put away.
- The first start of a daemon after a cold `cargo ra-jit build` finds what that build stored
  (#41). The two named the toolchain differently (the sysroot's libstd alone, or rustc's link
  line with libcore and liballoc too) and the store hashes that name into every key: on
  nushell the restart found 0 functions, took no frozen def map and expanded every proc macro
  again. It now finds 103,750 functions, 374 frozen crates and 1,586 expansions, and its cold
  compile is 11.9 s where it was 17.9 s. `warmstart.py` fails when the restart takes nothing.
  docs/cargo.md no longer says the store is off unless `RA_JIT_CACHE` is set.
- A daemon that stops lets its workers finish writing to the store what their last build kept,
  a minute at most each, where it killed them (#41). A stop just after an edit lost it: the
  next checkout of helix compiled 131,609 functions instead of 115,706.
- helix builds again: since #212 the borrow checker said "higher-ranked lifetime error" of
  `v.sort_by(match .. { a => |x: &T, y: &T| .., b => |x: &T, y: &T| .. })` and of
  `match .. { a => render_mode, b => render_spinner }` returned as an `impl Fn(&mut ..)`.
  rust-analyzer gives the lifetimes a signature elides as errors, where rustc makes each
  late-bound, so the fn pointer such a `match` makes of fn items or closures had no
  `for<'a>`; the placeholders of the bound were then held to those lifetimes. The checker
  now takes them for the lifetimes rustc would bind. The corpus's `higher_ranked` case does
  both (three errors before), and ide-diagnostics has a test.
- The borrow checker no longer panics on rustc's UI tests (`lints_ui.py --deny unused_mut`
  over all 20,450 files: 4 panics before, none now, and ra-jit's time from 67 s to 14 s). The
  causes were in the trait solver and the lowering it reads: a trait object's projections
  that become the same after substitution were deduplicated before being related, so
  `dyn A<X = T> + B<X = T>` failed to relate to itself (rustc relates them all, #136458); the
  `for<'b>` of an associated type's bound in a `for<'a>` was lowered as a binder of its own
  instead of concatenated onto the outer one, as rustc does; and upcasting a higher-ranked
  trait object built its supertraits outside the source's binder. Each has a regression test
  in ide-diagnostics, which panicked as rustc's test did before the fix.
- A 5,204-arm `else if` chain (rustc's `expr-stack-overflow.rs`, #206) is borrow checked in
  4.5 s, where it took over 300 s and then overflowed the stack: finding each expression's
  syntax walked down from the body's root, quadratic in the chain, and lowering, inference
  and MIR building recursed once an arm. The walk now goes to the child that contains the
  range, and the recursions grow their stack as rustc's `ensure_sufficient_stack` does
  (`stdx::ensure_sufficient_stack`). 1,000 arms take 2.2 s (5.35 s before), 2,000 arms 2.5 s
  (26 s); ide-diagnostics tests the 5,204-arm chain under 60 s.
- A destructuring assignment that moves out of a reference (`S { b, .. } = *r`) is an error
  at `*r`, as rustc reports it, and one that copies is not an error.
- `&[]` and `&mut []` are `'static` whatever their element type, as rustc promotes them: the
  borrow checker asked the type, `[T; 0]`, whether it needs dropping or is mutable inside, and
  reported `fn f<T>() -> &'static [T] { &[] }` as returning a reference to a local.
- ide-diagnostics's 24 failing tests pass (761 pass, 1 ignored, slow tests included), and CI's
  fast tier runs them in the fork job. Its `minicore_smoke_test`, which runs only under `CI` or
  `RUN_SLOW_TESTS`, found the `&[]` above and minicore's `ptr::read`, which moved out of a raw
  pointer as rustc refuses to and now calls `read_via_copy` as core does.
- The first build after one of cargo's is the daemon's where a proc macro or build script is
  built on what a build script wrote to `OUT_DIR`, as biome's macros are on
  `biome_aria_metadata`'s ARIA tables: the file was written while cargo built, after the build
  began, so it counted as changed since, and the build went to cargo ("what runs during the
  build: …/out/roles_and_attributes.rs changed"). Cargo's build replaced the worker, which built
  the program in full again: biome's next edit after that took 130 s and 24 GB. Such a file is
  only written again when the script runs again, which the script's own guards are for.
- The first build after one of cargo's is the daemon's where a build script makes a source of
  its crate newer, as biome's analyzer crates touch `src/lint/nursery.rs` for cargo to build
  them again when a rule is added: the build still went to cargo ("what runs during the build:
  …/biome_yaml_analyze/src/lint/nursery.rs changed") and its worker built the program in full
  again, 232 s at a 27 GB peak. A source that runs during the build is now changed when it is
  newer than cargo's last run of rustc on it, as cargo has it, not than the build's start.
  Every edit of memsplit.py's biome is now the daemon's, the first body edit 0.7 s and the
  ones after it 0.4 to 0.5 s (16 cores). soakrows.py's `proc macro its script touches` does it.
  soakrows.py's `proc macro on generated code` has the proc macro `include!` its script's
  output and checks that the first edit after the build is the daemon's.
- On Linux the daemon's bounds on memory count what its control group allows, where they
  counted the machine's: the workers' `RA_JIT_MEMORY_MAX` (half) and the own base's
  `RA_JIT_OWN_BASE_MEMORY` (a quarter) were of the host's memory in a container, a CI job or a
  systemd unit that allows less, so the kernel killed the worker before either was reached. It
  is the least of `MemTotal` and the `memory.max` (version 2) or `memory.limit_in_bytes`
  (version 1) of the process's group and those above it. soakrows.py's `control group's
  memory` starts the daemon in a 6 GB scope and checks that its workers hold at most 3072 MB;
  it said 96,021 MB on a 187 GB host before.
- A function the daemon compiles ahead, while it reads the symbols rustc built, is given up on
  once they are read, where the worker used to finish it before it was ready. bevy's second
  warm-store run began its derived `release_state` of a read-only query struct, which the
  trait solver takes about two minutes over, and was ready 148 s after the build in place of
  26 s; it is now ready in 26.5 s. A worker's own cancellation was not enough: salsa ignores it
  inside a query that recovers from cycles, as inference and MIR building do, so the threads are
  stopped as a write stops them, and the trait solver checks for that at each goal. A bin over
  bevy_ecs with that struct stopped compiling ahead 9 to 20 s after its symbols were read, and now
  within a second. `harness/aheadready.py` holds each function compiled ahead for ten minutes
  and checks that the daemon is ready within two.
- An expression of type `()` that computes nothing (an assignment, a block without a tail, a
  `break` or `return` without a value, an `if` without an `else`) gives its place the `()`, as
  rustc's MIR does: `let z: () = x = 1; let w = z;` was E0381, a build failed on code rustc
  accepts. A comment-only `asm!` writes its `inout` registers back, so the variable is
  mutated.
- A daemon's link that fails on names nothing defines, used only by code rustc's own link
  takes out, is run again as rustc's line has it, with `--gc-sections` (or `-dead_strip`,
  `/OPT:REF`). The daemon leaves those out so a patch can call what the executable's code no
  longer reaches, and zed's webrtc then needs X11's `libXtst`, which nothing links: its own
  base failed to link after 662 s ([#29](https://github.com/indexable-inc/ra-jit/issues/29)).
  An executable linked so takes edits that call into what it left out as builds in full.
- The borrow checker holds a struct or enum to what its fields need of its lifetimes,
  rustc's inferred outlives bounds, where it is named or built: `B(x)` with
  `struct B<'a, 'b>(&'b mut F<'a, 'b>)` needs `'a: 'b`. A closure passed for a bound with
  `for<'r>`, `impl FnOnce(&mut T)`, takes the bound's other lifetimes, so what it needs of
  them reaches its creator. A function item's return type is well-formed but for what names
  a late-bound lifetime, as rustc's: `foo` returning `(&'c (), PhantomData<&'b &'a ()>)`
  needs `'a: 'b` where it is a method's receiver. A trait object's associated type is held
  to the bounds its trait declares for it: `dyn Trait<Assoc = &'a ()>` with
  `type Assoc: 'static` needs `'a: 'static`. 1523 of 1580 of rustc's tests now (was 1520).
- A match arm with a guard binds what it binds by value once the guard holds, as rustc does,
  so `(mut long, ref short) if g => long.0 = &short` is accepted: `short` is declared first
  and dropped last. In a `const` or `static` a mutable borrow is promoted only of `[]` and a
  `const fn` call only where the item certainly runs it, so `helper(&mut 42)` gives E0716, as
  does `&(ALL[i] + 1)` in a function. An `impl Trait` inside the return type,
  `(impl Iterator, &'b str)`, gets the member constraints a returned one does, and the
  lifetime chosen for its hidden type is applied, so what that needs of a type parameter is
  checked (E0309). `borrowck_ui.py` reads each unit with its revision's `--cfg`, so
  `#[cfg($rev)]` in a macro is set. 1532 of 1581 of rustc's tests now (was 1523).
- Assigning `x` in `x[1][{ x = y; 2 }]`, where `x[1]` indexes a slice, is rustc's E0510: the
  bounds check already done must hold. A returned
  `impl IntoIterator<Item = impl Cap<'a> + Cap<'b>>` gets its member constraints, so a
  lifetime it does not capture gives E0700 with early-bound lifetimes as with late-bound
  ones. A closure assigning `bx.0` of a `Box` it captured from a `bx` declared without `mut`
  is rustc's E0594, which names the capture by the place it captured, `*bx.0`, as ra-jit now
  does. 1535 of 1581 of rustc's tests now (was 1532).
- A returned `impl Trait` holds the borrows of the lifetimes elided in its function's
  parameters, so mutating `self` while the iterator of
  `fn evens(&self) -> impl Iterator<Item = &i32> + '_` is live gives rustc's E0502; it was
  missed. Edition 2024 captures every elided lifetime, earlier editions the one `'_` or an
  elided lifetime in the bounds stands for (the lifetime of `&self`, else the parameters'
  only one), and `use<'_>` that one too. Type lowering in rust-analyzer gives the
  `impl Trait` a lifetime parameter for each, as rustc does (corpus `elided_captures`).
  1536 of 1581 of rustc's tests now (was 1535).
- The borrow checker proves `for<'a>` bounds for placeholders, as rustc does, and those an
  impl's where clauses need on the way: `&'static str: DeserializeOwned`, which needs
  `&'static str: for<'de> Deserialize<'de>` where only `'de: 'static` is implemented, gives
  rustc's "higher-ranked lifetime error", as does a forwarding impl that needs
  `T: for<'a> Bar<&'a isize>` of a `T` that is only `Bar<&'b isize>`. They were missed
  (corpus `higher_ranked`). 1539 of 1581 of rustc's tests now (was 1536).
- The borrow checker checks the constants of a signature: an array length or a const
  generic's default written as a block, `[u8; { .. }]` or `struct X<const N: usize = { .. }>`,
  which no item's body holds. `const N: usize = { let s: String; s.len() }` is rustc's E0381,
  and a move out of a reference in an array length its E0507; both were missed. 1541 of 1581
  of rustc's tests now (was 1539).
- A returned `impl Trait` hiding a type parameter is checked when the function's lifetimes
  are late-bound or elided: `fn f<T>(d: T, p: &mut ()) -> impl Sized + '_ { (d, p) }` gives
  rustc's E0311, and with `p: &'b mut ()` its E0309. The borrow checker relates the hidden
  type to an elided `+ '_` bound, so `dest: &mut T` returned behind `impl FnOnce() + '_ + 'a`
  gives E0621. 1520 of 1580 of rustc's tests now (was 1517).
- The borrow checker reports what normalizing a signature's associated type needs of its
  lifetimes (`-> <&'a () as Visitor<'d>>::Value` proven by `impl<'a, 'd: 'a>`), and relates
  an associated type through a where clause's projection (`T: Fun<F<'a> = [u8]>` borrowing for
  `'a`); item checking reports what the impl of an associated type in a field needs
  (`<T as Dummy<'a>>::Out` that `T: 'a`). 1517 of 1580 of rustc's tests now (was 1511).
- A body imported through another was called with the importer's type for it: an export
  declares what it calls with the types it calls it through, and one has as an integer what
  another has as a pointer. LLVM takes a call of a body through a type it does not have to be
  undefined, and of `ptr::write::<ForOf>` imported through a function that had the address as
  an integer it kept nothing of what was written, so bun's parser crashed. Such a call is now
  made through the callee's own type, the arguments converted (a unit test of `llvm.rs`).
- On Linux AArch64, an `asm!` of a feature LLVM's assembler has no `.arch_extension` for was
  refused: constant_time_eq's `mrs {}, dit` under `#[target_feature(enable = "dit")]`, which
  failed bun's build there. Such a feature (`dit`, `bti`, `jsconv` and the others of a later
  Armv8 or Armv9, and `v8.4a` and the like themselves) now starts the block with `.arch` of
  the version that has it.
- `runbench.py` runs cargo's release build from beside ra-jit's, in the workspace, where it ran
  it from cargo's target directory. helix looks for its `runtime` next to itself, so only
  cargo's build warned that there was none, and helix was reported as running differently.
- The borrow checker panicked on the corpus's `release_state`, whose impl's where clauses
  name associated types of concrete types (`Option<&'static C>: QueryData<ReadOnly: ..>`), so
  every error in the corpus's main file went unreported. As rustc, it now normalizes those
  where clauses before using them, and relates two associated types by what they normalize to
  (a GAT's `Self::Item<'w, '_>` and `Self::Item<'w, 'static>` are the same `&'w T`, where it
  said "lifetime may not live long enough"). A panic now fails the check where it was
  skipped: `ra-jit borrowck --project` exits with an error and counts panics,
  `borrowck_ui.py` fails on a panic or on any error in a test rustc accepts, and CI checks the
  corpus and `mixfutures` crates.
- The borrow checker no longer reports "temporary value dropped while borrowed" on every
  field of a `tracing::trace!` with tracing 0.1.44, which names fields with a `const` in a
  block inside its callsite's `static`: a `const fn` method call there is promoted, as rustc
  promotes it, so cargo's edit build goes through again
  ([#109](https://github.com/indexable-inc/ra-jit/issues/109)). A trait object given for a
  type parameter bounded `T: 'a` lives for `'a`, as rustc's object lifetime default, where
  rust-analyzer made it `'static` and the checker said "lifetime may not live long enough"
  ([#84](https://github.com/indexable-inc/ra-jit/issues/84)). All of cargo 0.99 now checks
  clean, where five false errors were left: rusqlite's `params![..]` of locals (an impl
  header's `&dyn ToSql` is for any lifetime, not only `'static`), `unwrap_or(&|_| true)` (a
  closure capturing nothing is promoted), git2 callbacks borrowing a counter declared after
  them (a panic's path drops nothing for a value with no drop glue), and an `async fn`
  returning `impl Iterator + 'b` of `&'a self` with `'a: 'b`.
- A program whose `main` is C's, as bun's, is built as a whole program: LLVM gets every body
  and inlines across crates, where each function was compiled alone before. `derive(PartialEq)`
  of an enum compares the discriminants first, as rustc's does. bun's whole program, every
  function ra-jit's, runs at 1.28x cargo's release build on Linux x86-64 (was 5.80x) and 1.25x
  on macOS arm64, where it now links: `runbench.py` no longer passes `-C link-dead-code`
  (which dropped `-dead_strip`), and the names of C in ra-jit's Mach-O objects stay
  exportable, as bun's `napi_*` must be
  ([#35](https://github.com/indexable-inc/ra-jit/issues/35)). The build takes longer, 565 s
  against 87 s on 16 cores: every compile carries the bodies it may inline.
- Adding a function to bun took 19.8 s, more than cargo's 14.5 s; it is a patch now, 0.64 s on
  Linux x86-64 and 0.56 s on macOS arm64. An added function in a program whose every function
  is an entry no longer forces a full build, and a crate the daemon listed before the files
  its `include!`s name were loaded is listed again once they are (`harness/latelisted.py`), so
  bun's 2,834 generated C-ABI functions are entries from the start. The edit's peak memory
  falls from 33.5 to 27.3 GB.
- helix's own build threw its first round away (about 30 s) because std's `Context::with`
  closure was refused for "a writable static without a rustc symbol": the static is declared by
  `thread_local!` in a body, which rust-analyzer expands twice, and ra-jit looked for the
  static's place in the other expansion. It finds rustc's symbol now, and the build takes one
  round: the daemon's own build in the background is done at 114.5 s instead of 130.3 s, a
  second daemon's at 85.9 s instead of 109.4 s, and with `RA_JIT_RUSTC_BASE=0` the daemon is
  ready at 116.2 s instead of 142.3 s (Linux x86-64, 16 cores, medians of three). Nothing is
  left to rustc for it, and `harness/norustc` counts the same functions left to rustc on
  ripgrep (53) and nushell (534) (#37, no-rustc step E).
- A whole build (`cargo blaze build`, `run`, `test`) that the source's errors fail writes them
  as rustc and cargo do: each error, then `error: could not compile `app` (bin "app") due to 1
  previous error`, and the note naming blaze's toolchain when the project pins another, where
  it said `error: could not build `app`: ` before the first error.
- An error in code nothing reaches, written while no daemon ran, fails the first whole build:
  with no rustc to check the source, the worker checks every file of the crates the build
  compiles once it has loaded them (`check: N files in ...` in its log), and every build of the
  checkout fails with the error until a source is written. It built the program before.
- A `#[global_allocator]` in an inline `#[cfg(test)] mod test { .. }` of a library (ureq's)
  gave the library's stand-in an allocator outside the test build, whose functions nothing
  defined: every allocation of the program reached the stand-in's trap, so nushell, which links
  ureq, stopped at its first allocation. The `#[cfg]`s of the inline modules an allocator is in
  are now the forwarding allocator's.
- A constant that read a static holding `&dyn` of another static failed to evaluate
  (`InvalidVTableId`): the vtable was the id the static's own evaluation gave the type, which
  meant nothing to the reading evaluation, and a static the reference pointed into was not
  looked through for it. rustls's `ALL_CIPHER_SUITES` is one, which kept nushell from linking
  (corpus `dyn_static_suites`).
- A tuple struct with fields seen in other modules than the struct, both `pub(in path)` (ring's
  `aes::Counter`, `pub(in super::super)` with a `pub(super)` field), had no constructor:
  the least of those visibilities was folded from none, which is none. Its construction and
  patterns were refused, and with them ring's AES-GCM under nushell (corpus
  `narrow_constructors`).
- An `async fn` or block that awaited the `impl Future` of a provided method of a trait,
  which the impl leaves as the trait has it, had no layout: the trait's opaque type was left
  for the solver, which reveals nothing of a trait's, and came out an error. It is the hidden
  type of the trait's body now. rmcp's `ServiceExt::serve` is one, in nushell's MCP server
  (corpus `provided_futures`).
- `-C force-frame-pointers` in a build's flags keeps frame pointers in ra-jit's code too: LLVM
  gave the register to values, so code that walks frames by it read garbage. bun, built with
  every function ra-jit's, faulted in `bun_core::return_address` on its first `-e`; it now
  gives cargo's output on each of its runs. `harness/apps/apps.json` gives bun its own build's
  `-Cforce-frame-pointers=yes`. `harness/bun-repros/frame_pointers`.
- On ELF, a build that no edits follow puts each function and datum of its object in a
  section of its own, as rustc does, so the link takes out what nothing reaches. A program
  whose `main` is the C runtime's has every function compiled, and bun's link was missing
  what only such code calls and nothing defines on Linux: `GetLastError` of
  `bun_windows_sys`, and `ShellInterpreter__getConstructor` of a binding bun's C++ leaves
  out. The daemon's builds keep one section, as their links keep everything.
  `harness/bun-repros/unreached_externs`.
- A crate a build names after the program is one the program links, also when a binary of
  another package has its name: a build of bun with every crate ra-jit's took `--crate
  bun_sema` for the library and for `bun_sema_standalone`'s `bun-sema` too, whose code calls a
  library bun does not link, and the link failed on `bun_sema_standalone::parse`.
  `harness/bun-repros/same_name`.
- A `#[no_mangle]` or `#[export_name]` static of a program whose every crate is ra-jit's is
  defined under that name, also when it is private and when no Rust code uses it. Our copy had
  a name of its own, as rustc's was the one code of another language found, and one only that
  code reads was not compiled at all: bun's link was missing `Bun__version`,
  `Bun__Node__ZeroFillBuffers` and the other statics its C++ reads.
  `harness/bun-repros/exported_statics`.
- An `$n:expr` passed on to another macro is one operand there, as rustc has it: in
  `tags!(@count (0u8 + 1u8) + 1u8,)` the rule `(@ $n:expr,)` took `count (0u8 + 1u8)` for a
  call and matched, so bun's `task_tag::COUNT` expanded to nothing and `TaskTag::name`, the one
  function of bun's left to rustc for a reason other than having no instance, failed on
  `[&str; {const error}]`. The parser now stops an expression where such a fragment starts.
  `harness/bun-repros/macro_fragments`.
- A `macro_rules!` that a macro of another crate defines, called by a name a `use` imports
  (`use cfg_if::cfg_if; cfg_if! { .. macro_rules! trace { .. } }`), is in the textual scope of
  what follows. The call waited for imports to be resolved, by when `#[macro_use]` had taken
  the module's macros to its parent without it, and every `trace!` of lol_html's parser was a
  missing expression: 43 of bun's functions (the `StateMachine` impls of `Lexer` and
  `TagScanner`, `TransformStream::write`) were left to rustc. Such a call is now expanded where
  it is written, as one by a path to the other crate was. `harness/bun-repros/cfg_if_macros`.
- `--crate NAME` names no crate of the standard library's when a crate of the program has the
  name. std vendors foldhash 0.2.0, libc and hashbrown, and its foldhash, which rustc does not
  build (hashbrown has it only with its default hasher), sees no `core` in rust-analyzer's
  graph: bun's whole-program build compiled it beside bun's two foldhash, and its 27 functions
  that did not lower were counted as left to rustc. `harness/bun-repros/std_copies` checks it.
- A dependency's impl that a macro writes in a function body is found: dtoa's `write` calls
  `dtoa!`, which calls `diyfp!`, which writes `impl DiyFp` there, and `normalize_boundary`
  went unresolved because no `impl` is in `write`'s own text. A dependency's function is now
  looked into for impls when it calls a `macro_rules!` of its crate or of a dependency that
  writes one, in its text or through another macro. The dtoa functions bun left to rustc
  compile; `harness/bun-repros/macro_impls` builds dtoa.
- On Linux AArch64, an `asm!` of an instruction that the function's `#[target_feature]`
  enables and the target's baseline lacks (zlib-rs's `crc32w`, in bat) was refused by the
  assembler: the block is assembled apart from its function, so it now starts with the
  `.arch_extension` of each such feature. The runtime benchmark's whole-program builds of
  tokei and eza no longer fail to link there: they kept rustc's object of the names a
  program keeps out of the line, which is how GNU ld takes the panic runtime (#78).
- A checkout that an edit broke could be answered ok, linked with rustc's copy of the function
  from before the edit: a program whose build was interrupted, or panicked, or whose checkout
  was loaded again with the same link, forgot what rustc's copies are copies of, and its next
  build took the source as it was then for what rustc compiled. It keeps that now (#71).
- An edit of code two programs share ran wrong with the LLVM backend when the daemon compiled a
  program's `main` itself (on macOS and Windows, whose temporary paths go through a link): it
  took that build for one of a whole program and inlined across crates, and a caller holding
  the old body of an edited function was kept, as what is kept is compared by each function's
  own code. A daemon or `--watch` no longer inlines across crates; `programs.py` with
  `RA_JIT_ALONE=1` builds both programs that way in CI (#63).
- Under plain cargo, a test that ran the workspace's program could find none: the build the
  daemon loads afterwards ran beside the tests, and linking the program into place again
  deletes it first. It now waits for the user's cargo to exit (#61).
- `cargo ra-jit stop` returns once the daemon has exited, with its workers stopped and its log
  written, rather than once its socket is gone; nothing starts a spare worker while the daemon
  ends, where one could outlive it. A client that replaces a daemon of another version waits
  the same way (#65).
- A daemon's socket is free as soon as the daemon lets go of it, also while another thread of
  the process starts a program, which shares the lock's file until it runs (#58).
- The daemon's own base, built once nothing is asked after a cold build, started over every
  second and was never done when a crate it loads includes a file that is not there: zed's
  `livekit_api` reads `OUT_DIR`, which only a dev-dependency's feature brings into the
  daemon's crates, and whose build script cargo never ran. A file an `include!`,
  `include_str!` or `include_bytes!` names and that cannot be read is now read again once it
  is there, rather than taken for one still to load before each request
  (`harness/rustcbase.py`'s `unbuilt-member`). And since a base nobody waits for must not
  take the machine, a worker that holds more than a quarter of its memory while it builds one
  gives it up and says so; edits go on patching rustc's executable (`RA_JIT_OWN_BASE_MEMORY`,
  `own-base-budget`). zed's base, which the loop had hidden, passes it: 51 GB of 74 GB live are
  debuginfo types
  ([#55](https://github.com/indexable-inc/ra-jit/issues/55)).
- bat builds whole: an `impl` inside a function body of a dependency (bincode's `EnumAccess`
  for `&mut Deserializer`, inside `deserialize_enum`) was not found, as rust-analyzer only
  looked into the bodies of workspace crates; a dependency's file is now lexed for an `impl`
  inside a function's braces, and only the functions holding one are lowered.
- A whole program's build no longer builds a body it found too large again at each call
  that allows no more: with what such a body counted given back, ripgrep's build across
  crates took 416 s on Linux x86-64 and numeric's 151 s; 30.1 s and 8.7 s now.
- LLVM's `instcombine`, as ra-jit's pipelines name it, runs without verifying that it reached
  its fixpoint, as the one in `default<O3>` does: an LLVM built with that check ended the
  process on a function of jsonrt, and of nushell at `-O3` across crates.
- fd built with inlining across crates crashed the LLVM backend on Linux and macOS: an address
  rebuilt as a `getelementptr` may be a sum that is a constant (a global's address plus an
  offset), which is in no block, and LLVM read freed memory when the builder was put before it.
- A `const { .. }` block in a function is a constant evaluated for each instance of the
  function, as rustc's inline consts are, and no longer code the function runs: what only
  compile time may run in one (`#[rustc_comptime]`, `type_info`'s `field`, bun's
  `MultiArrayList::check`) was refused with "intrinsic `type_id_field_representing_type`",
  and every function reaching it with it (591 of bun's 680 refusals on Linux). A block met
  while evaluating a constant runs in that evaluation, so its `TypeId`s compare. `n += i` on an
  integer variable whose operator was not looked up while its type was unknown is lowered as
  the built-in it is, where it was "adjustment of binary op was missing" (`bun_repros.py`
  `type_info`).
- A constant of a struct, enum, tuple or array type given as a const generic argument
  (`adt_const_params`: `check::<{ Requirement::Within { low: 3, high: 9 } }>`,
  `filtered::<{ Filter { mask: 0xf0, strict: true } }>`) compiles and evaluates, and
  `#[derive(ConstParamTy)]` expands. Every function reaching one was refused ("constant of type
  JSType", bun's `alloc`, `test_correct_state` and `completions`) (`bun_repros.py`
  `adt_const_params`).
- A path through a type alias of an inherent impl (`ThreadPool::Worker::get(..)`, `Self::Slot`,
  `Slots<u32>::Slot`, nightly's `inherent_associated_types`) is the alias's type: the
  functions that name one had no type for it and were left to rustc, and rustc's copy of
  bun's `run_nested_rename_task` sent every edit of its crate to cargo (`bun_repros.py`
  `inherent_associated_types`).
- A build in full no longer keeps rustc's copy of an instance that did not compile once an
  edit changed it, as `located::<u8>` of a generic function with a `Location::caller()`
  constant, which `main` calls: the program printed what it did before the edit. Such an edit
  goes to cargo, which names the function (`soakrows.py`, "an instance rustc's copy stands in
  for, edited").
- A static declared in a function's body, as the `FLAG` of a method of bun's
  `impl<const SSL: bool, ..> NewServer<SSL, ..>`, is rustc's in a crate with a function left
  to rustc that reaches what ra-jit can't tell: rustc's copy of that function reaches the
  static through rustc's copy of the other. A build in full had its own copy beside rustc's,
  and the two counted apart, in the base the daemon builds after the first patch
  (`soakrows.py`, "statics one codegen unit uses").
- A borrowed constant with a cast in it, `&[b"node" as &[u8]][..]` returned for `'static`, is
  promoted as rustc promotes it, where it was built on the stack and the reference outlived
  it: every cast is promotable but of a pointer to an integer. bun's `new Bun.Transpiler()`
  crashed on it (breakers, `promoted_index_of_casts`).
- A `match` arm's guard with `let` (`if let` guards and their chains) dropped what it bound
  and its temporaries at the end of the guard, before the arm ran, in editions before 2024;
  rustc keeps them until the arm ends or the next arm is tried
  ([#46](https://github.com/indexable-inc/ra-jit/issues/46)). The four rustc tests that
  caught it run on every pull request now (`harness/ui-pinned.txt`).
- The harnesses set `RUST_BACKTRACE=0` for what they run rather than unsetting it: cargo's
  `[env]` (as in `~/.cargo/config.toml`) fills in only what is unset, and a backtrace changed
  what a panicking program printed.
- Large workspaces are ready in a fraction of the time and memory
  ([#29](https://github.com/indexable-inc/ra-jit/issues/29)): a closure's type holds its inner
  closure twice, so the store's name for a type of nested closures doubled with each level
  (one of helix's was 665 MB). A type is now spelled once per daemon, and one spelled longer
  than 1 KB is named by the SHA-256 of its spelling; shorter names, and the store entries
  under them, are unchanged. helix is ready in 109 s at 18.4 GB of memory instead of 631 s at
  100.7 GB, its store is 1.6 GB instead of 14 GB, and its first `cargo ra-jit run` takes
  46.9 s instead of 640.8 s (`harness/deepnames.py`).
- A `Box` beside an allocator that has a size (`Box<[T], &Arena>`, as bun's arenas) is
  dereferenced and dropped through its pointer wherever its layout put it: a deref of one was
  left to rustc, and its drop read the pointer from the start of the `Box`. `breakers.py`
  runs the crates of `NIGHTLY_CRATES` with RUSTC_BOOTSTRAP=1 (`allocated`, `allocator_api`).
- `simd_reduce_max` and `simd_reduce_min` of integer lanes are compiled (AArch64's `vmaxvq_u8`
  and the other `vmaxv`/`vminv`, AVX-512's `_mm512_reduce_max_*`): a function that reached
  them was left to rustc (bun's ASCII checks; `simd_reduce_max` breaker).
- `RA_JIT_OPT=3`'s last instruction combining no longer aborts the process when one run of it
  leaves something to combine ("Instruction Combining … did not reach a fixpoint after 1
  iterations", bun's whole-program build on AArch64): the default pipelines never ask LLVM to
  check that, and a pass named in a pipeline did.
- `apps.py`'s and `runbench.py`'s whole-program build of a program whose C calls its Rust
  by an unmangled name (a `#[no_mangle]` function) goes on when cargo's link of the stand-ins
  fails for want of those functions: what is taken from it is the link line, and ra-jit's
  objects define them. It ended at "cargo failed" (bun; `nativecalls` in `apps.json`).
- What stands in for a crate in cargo's build (the source with no code that cargo links when
  ra-jit compiles the crate: `apps.py`'s and `runbench.py`'s whole-program builds, and the
  crates the daemon builds while cargo builds the rest) builds with a nightly cargo that names
  each dependency twice, its `.rlib` and its `.rmeta`. It had an `extern crate` for each
  (`E0259`, bun 1.4.3 on `nightly-2026-09-15`).
- A `macro_rules!` that a macro of another crate defines (`dep::define_log!(log)`, as bun's
  `bun_core::define_scoped_log!` at the top of a file) is in the textual scope of the modules
  after it, as in rustc. rust-analyzer expanded a call by a path only after the file's modules
  were collected, so their calls of the macro were missing expressions and their functions were
  left to rustc (`IncompleteExpr`); in bun one such function in `bun_runtime` sent every
  signature edit of the crate to cargo (`harness/breakers`, `macro_defined_by_foreign_macro`).
- `harness/apps.py`'s perf stage ends with its timings again: since the rename from rustd it
  stored ra-jit's as `ra-jit_s` and printed `ra_jit_s`, a `KeyError` after every workload.
  wasmrun builds wasmtime 36.0.19 (was 25.0.3, which Dependabot's alerts name); its workload
  is the same as rustc's on both.
- `cargo ra-jit run` from many clients of one checkout at once starts each client's program,
  on macOS too. A package with a `default-run` (or one program of several) went to plain
  `cargo run`, which on macOS removes and copies again the program cargo has under
  `target/debug` before it starts it, so a client that started it meanwhile found no file
  ("No such file or directory") or hung before its first instruction; and rustc built the
  edit again. ra-jit now runs the `default-run` itself, holds cargo's lock on the build
  directory from putting the daemon's programs in place until they have started (a `cargo
  test` executable too), takes it to give cargo back its own, and on macOS puts a program in
  place as a clone, as cargo does: a program started from a hard link just made can be
  killed as it starts ([rust-lang/cargo#7821](https://github.com/rust-lang/cargo/issues/7821)).
  `cargo_ra_jit.py`'s 16 clients on 4 worktrees run 8 rounds of edits, all by the daemon.
- On Linux a program `cargo ra-jit` started just after a full link failed now and then with
  "could not execute" (exit 1; another row of `cargo_ra_jit.py`'s flags each run): the daemon
  writes what a debugger reads of a new executable in place while it waits for the next edit,
  and the kernel refuses to start a file open for writing ("text file busy"). The daemon now
  writes an executable in place under an exclusive lock on it, and a client starts a program
  under a shared one.
- A program or test `cargo ra-jit` starts itself has what the `[env]` of cargo's
  configuration sets, as under `cargo run` and `cargo test`: the nearest `.cargo/config.toml`
  (or `config`) says a variable, then `$CARGO_HOME`'s, with `force` and `relative`.
- `cargo_ra_jit.py`'s life workspace no longer races with itself on macOS: the test that runs
  `cargo build` inside and the one that starts `other` take turns, since cargo removes and
  copies each program again there on every build and the second sometimes found none (plain
  cargo failed it as often as ra-jit).
- `harness/ui-known.txt` names the 15 tests of rustc's tests/ui run-pass that differ on the
  whole suite, each with why (11 assert a bug of rustc's, 4 are ra-jit's to do), so the
  `claims` gate says only what is new; a line can name the backends it is known on.
- `harness/debuginfo.py`'s perf check takes a sample in a naked function without a line as
  rustc has it (its body is assembly, which rustc gives no line either), not as lost debug
  info: one of the corpus's on Linux arm64 failed it now and then. It reads every frame perf
  prints, those of inlined functions and those whose names have spaces (`<T as Trait>::f`)
  too: it skipped them, so where perf looks CFI up a page off the frame (both Linux runners)
  the frame it took for the one sampled could be one unwound to anywhere, a static on Linux
  x86-64 once. rustc's corpus binary has as many frames unwound to data there.
- `breakers.py`'s `frame_of_tables`, `frame_of_results` and `frame_of_array` ask whether
  ra-jit's frames fit where rustc's do, not whether they are as large: a whole program's
  build with LLVM makes them smaller than rustc's, which each case's crate has room for too,
  and they differed. `frame_of_array` is open on Cranelift alone, whose frames keep three
  arrays where rustc's keep two.
- `cargo_ra_jit.py` says what a client that failed or hung said, and what it was doing,
  rather than ending on a timeout with nothing.
- A workspace the daemon loads after a change whose build scripts did not all run is not
  loaded: the crates of a script that failed were read without its `OUT_DIR`, so thiserror's
  `include!` went missing, every derived `Error` was unimplemented and `?` resolved to the
  body-less `FromResidual::from_residual` for the rest of the daemon's life (what hyperfine's
  edits under plain cargo met now and then on Linux before the edit stopped running every
  script through cargo). It says which script failed instead, as an edit that runs the
  scripts again already did.
- `bench.py` without `--reference` built plain cargo into `target-ref` inside the checkout,
  which cargo takes for a source of the package: ripgrep's, fd's and hyperfine's build scripts
  name no `rerun-if-changed`, so cargo ran them again and built the package again at every
  build, and "nothing changed" took cargo 300 to 390 ms instead of 30 to 60. The README run of
  2026-10-20 at `c8f58ac7d9` had it; cargo's target directories are now outside the checkout
  (`WORK/reference` by default).
- `nix profile install github:indexable-inc/ra-jit#ra-jit`, as the README says to run it,
  installs ra-jit and the programs it runs; the flake had no package of that name.
- A release's archive and the flake's build have `ra-jit-rustc-first`, which `cargo ra-jit`
  runs in the first build of a workspace; they had only ra-jit, cargo-ra-jit and ra-jit-rustc.
- A module whose file is outside its package, by `#[path = "../../shared/outside.rs"]` as
  bun's runtime has, is compiled by ra-jit and its edits are built. The file was in no
  package's directory and so not loaded: the functions that called into it were left to
  rustc, an edit to it was not seen and the program kept running the old code, and in a
  binary crate the daemon could not load the workspace ("unresolved module ... [E0583]").
  The file is now put in the source root of the one that declares it, as a file an
  `include!` names is. `soakrows.py`'s "a module file outside its package" fails without
  this.
- An edit that could change a function ra-jit left to rustc from the start, such as an item
  added elsewhere in its crate, is built by cargo, as docs/cargo.md says. It failed the
  build: when the function was one the frontend turned down, ra-jit took that for an error
  of the source ("has an error ra-jit cannot say more of"), and every build after it failed
  too until cargo built once. A function the frontend turns down only after an edit is still
  reported as an error. Found building bun; `soakrows.py`'s "an item added beside a function
  left to rustc" fails without this.
- A worker that loaded its workspace beside cargo's first build
  ([#11](https://github.com/indexable-inc/ra-jit/issues/11)) ends when its daemon does, as
  every other worker does: once told what cargo built, it waited for a lock on its standard
  input that it held itself, so a daemon that crashed or was killed left the worker running,
  holding its memory (2 GB on fd) and its socket, and the next daemon's first build fell
  back to cargo ("another daemon serves"). `cargo_ra_jit.py`'s "workers of the killed
  daemon are gone" fails without this.
- fd, tokei and eza build whole and run as cargo's builds do: a crate rustc built nothing
  of keeps its `static`s ra-jit's own (fd's `INIT`); a package built only for a build script
  or a proc macro, which `cargo metadata`'s single graph can still show as a dependency of
  the program's, checks overflow as the program does, so rust-analyzer's view of it no
  longer mixes two profiles; and `runbench.py` turns the release profile's `lto` off for the
  whole build, which needs rustc's libraries. bat and nushell do not link yet.
- A program ra-jit builds whole, with its own `main` (`runbench.py`'s builds), ran: on macOS
  the code-free stand-in rustc builds for the program had a weak `main` of its own, and
  ra-jit's object was weakened too, so the linker kept the stand-in's and the program
  exited 0 at once, printing nothing. ra-jit's `main` now stays strong. On macOS and Linux
  a `#[global_allocator]` was ignored in such a program, as the stand-ins declare none and
  rustc's allocator shim went to the default allocator; a stand-in now declares one that
  forwards to functions rust-analyzer's expansion of the attribute defines, under the
  `#[cfg]`s of the static and of the modules it is in, and not for one in a string, as
  heapless's docs have (`wholeprogram.py`).
- `&(1 / 2)`, a division by a constant, and `&mut []` are promoted to constants as rustc
  promotes them, so a reference to one lives as long as the program.
- Before the 2024 edition, a panic in a block's tail drops the tail's temporaries after the
  block's locals, as rustc does; ra-jit dropped them first, so a `Drop` that printed or
  locked saw the other order (`breakers.py unwind_tail_after_local`).
- What leaving a scope early drops (`break`, `continue`, `return`, a future dropped at an
  `await`) is placed on the line where the scope ends, as rustc places it, so a backtrace or
  a debugger's step through those drops shows the same lines under ra-jit as under rustc.
- On macOS, signing a splice reuses the hash of each appended page that is the same as the
  last splice's: 21 ms to 5 ms of each edit of nushell.
- The value of a `match` arm's guard was marked dead before the branch that reads it; with
  storage markers kept, as rustc's optimized MIR keeps them above `-Copt-level=0`, the guard
  could take the wrong arm.
- The code of a function grows in a straight line with the arms of a match that leave
  through `?`, as `#[derive(Serialize, Deserialize)]` writes for an enum: it grew with
  their square, 329 MB for one function of 285 arms, which an AArch64 branch cannot reach
  across ([#5](https://github.com/indexable-inc/ra-jit/issues/5)). `harness/growth.py`
  checks it.
- An edit that arrives while a function is being compiled at a higher level no longer waits
  for LLVM: the compile lets go of the database before LLVM runs.
- A workspace under a directory whose name has a space in it is built against what rustc
  built of it ([#14](https://github.com/indexable-inc/ra-jit/issues/14)). The daemon cut
  the dep-info file that names a crate's sources at every space, found none of the
  workspace's crates among rustc's, and named their functions itself, so a function it
  refused had no copy of rustc's to link to. Dep-info is now read and written as rustc writes
  it, in one place: a space in a path is `\ `, and `$`, `#` and backslashes are themselves;
  the values of the variables in it are unescaped too. `harness/spaces.py` builds a
  workspace under such a directory end to end.
- On macOS a program whose unwind table is larger than 1 MB takes edits as splices, and so
  live in the running program ([#13](https://github.com/indexable-inc/ra-jit/issues/13)).
  The daemon gave Apple's linker a fixed `-headerpad` of 1 MiB, and a splice moves the
  table into that room, so a GUI app of 570 crates with 3 MB of table, or nushell with 3.2
  MiB, linked every edit in full. The link now reads its headerpad from a file beside the
  executable, last on the line: 1 MiB, or what the line's own flags ask for. After each full
  link the daemon reads the header, and when the room is short it links once again with what
  the table needs and a quarter more, which the links after start from, in later daemons
  too. nushell's edit of `main` went from 878 ms to 276 ms on an M5 Max, and a library edit
  from 693 ms to 166 ms. `harness/headerpad.py` checks it.
- The first edit after the daemon is ready is as fast as the ones after it
  ([#12](https://github.com/indexable-inc/ra-jit/issues/12)). Two things made it slow.
  On Linux the first splice after a full link took 2.5 s of nushell's: it copied every
  debug section it added to, about 500 MB, behind the code it appended, and it cut the
  file to size, which on XFS writes out at the close all of the file the linker had not
  yet written. The sections now go where splices write them right after the link, and a
  splice only lengthens the file, so the first splice's link takes 7 ms, as those after it
  do. And code the daemon took from the store does not say where in the checkout it is
  from, so the first edit of a crate it came from compiled all of it and wrote the whole
  object again: 71,661 functions of nushell. The worker now compiles that code again
  by itself after its first build, and the daemon says the checkout is ready once it has.
  nushell's first edit of `main` after a cold build from a warm store went from 9.1 s to
  0.37 s on Linux and from 7-8 s to 0.3-0.4 s on an M5 Max. `harness/storefirst.py`
  checks it.
- On macOS a spliced executable lists its sections in the order of their addresses, and
  each segment a splice adds has a section, as `dyld_info` requires: it read none of the
  spliced executables on macOS 27 (`harness/splices.py`: 11 of 71 right, now 71).
- Tiered code that calls what the executable has no symbol of (`memcmp` where rustc's code
  calls `bcmp`, a `pthread_` function only rustc's inlined code reached) no longer has the
  program linked in full and started over at level 1, after which every later patch of it
  was linked in full too: that function stays where it was, and says why
  ([docs/tiers.md](docs/tiers.md), "Code no patch can hold").
- The thread that compiles at a higher level has a stack of 64 MiB, and the bodies a body
  built in shows are ten deep at most: a deep chain of them overflowed the 2 MiB default and
  took the worker down.
- Tiered code that calls what the executable has no symbol of (`memcmp` where rustc's code
  calls `bcmp`, a `pthread_` function only rustc's inlined code reached) no longer has the
  program linked in full and started over at level 1, after which every later patch of it
  was linked in full too: that function stays where it was, and says why
  ([docs/tiers.md](docs/tiers.md), "Code no patch can hold").
- The thread that compiles at a higher level has a stack of 64 MiB, and the bodies a body
  built in shows are ten deep at most: a deep chain of them overflowed the 2 MiB default and
  took the worker down.
- `RA_JIT_RUSTC_BASE=0` reaches the worker of the build that sets it, which is a worker of its
  own: it reached one only when the build that set it had started the daemon, so behind a
  daemon another build started the first edits were patches of rustc's executable, and an edit
  that adds a type asked for a restart (`live_types.py` in the gate, after the other live
  tests).
- The first edit after the daemon is ready, while it is still rustc's executable that edits
  patch, is faster when it is a build in full, as one that moves an `async` body is (reth's
  `fn main`, [#259](https://github.com/indexable-inc/blaze/issues/259), by @gakonst): the
  pass that puts the code in the object, which runs on one thread, wanted every function
  compiled on demand again for each static rustc's code has, and walked a whole definition with
  its macros for each closure and each static in a body (tracing's `META` and `__CALLSITE` in
  every event). Each crate's on-demand functions are wanted once, and each definition is
  walked once for all its closures or all its statics of a name. reth's first edit on 48 cores:
  64.4 s and 64.3 s before, 58.0 s and 57.5 s after.
- `blaze disable` removes the `bin` directory of cargo's home that `enable` made for the links
  of `cargo fmt` and `cargo clippy`, when nothing else is in it: the home is as it was.
- A load refused for configuration is retried on the next build, so editing or deleting
  `.cargo/config.toml` no longer leaves `cargo blaze run` reporting an old refusal until
  `cargo blaze stop` (#597). Reported by @enricoschaaf.

- A plain `cargo blaze check` leaves integration tests, examples and benchmarks out of its
  diagnostics, as `cargo check` does. Those targets were checked without their
  dev-dependencies, producing false unresolved imports and method errors; explicitly
  selecting dev targets still checks them with their dependencies (#634, reported by
  @om606bosch; also #632).

### Merged

- ra-jit-live 0.1.0 is on crates.io: a README for it, its metadata, and the README asks for it by version
- demo/life: the editor is back, on the right of the window
- Mach-O splice: the unwinder's tables of the program are renamed with seven bytes again
- CI: hosted runners, nix, unit graph, live demo checks (#1)
- Dependabot leaves vendor/ alone: it read the submodule's crates/* glob against this repository's crates/ and failed on hir-ty-codegen, which has no Cargo.toml
- nix/units: ra-jit and ra-jit-bench say rust-version 1.99, as Cargo.toml has since the crates.io commit; the units were stale and every nix build refused to evaluate
- A build that panics is the build's error, not the worker's end; the daemon logs a worker's end; a daemon of another nix build is replaced
- live_e2e.py: the running program takes LIVE_EDITS edits one after another; live-debug workflow keeps the logs
- The daemon keeps its workers while a program that takes edits runs
- A location or debug row in the installed standard library on Windows names the file as rustc does, <sysroot>\lib/rustlib/src/rust\library\.., not with backslashes throughout
- On main a CI run is no longer cancelled by the next push: it finishes and the newest push waits behind it, so main gets a verdict while several agents push
- rust-analyzer fork with rustc-shaped MIR (unwind trees, Assert, promotion); futures mix by default
- The daemon links rustc's symbols.o, the object that names what a program keeps, from the copy it takes of it: GNU ld reads each archive once, and without that object it never took the panic runtime, which rustc puts before the standard library, so a panic=abort program failed to link on aarch64 Linux. A Windows path counts as symbols.o by its end again, and cargo_ra_jit.py says a failed cargo install is a failure instead of crashing
- LLVM no longer folds a comparison of two statics' addresses to false: a static's global has a type without a size, so an extern static and the #[no_mangle] one it links to compare equal as under rustc
- operands unwind before the temporaries they borrowed
- The store may take 32 GiB by default, not 8: the cache rarely evicts on a dev machine, which matters more than how it evicts
- vendor/ as subtrees, flake builds from self alone
- Merge commit '8dbb3d6d3d8b2b6584fdbfb725c1a3e2ca8eb8a5' into HEAD
- debuginfo.py prints what gdb said after the breakpoint when its steps never leave the line, as it does when gdb does not stop
- A function a macro of the crate's own writes is declared, for the debugger, where the macro writes it, as rustc declares it. Its lead line row was the call site, and gdb on aarch64 takes every later row with a smaller line for prologue, so a step into it stopped past its calls and ran on to the exit when the panic was in one of them. The panic Location still reports the outermost call site.
- backtrace_line is open on macOS: std's backtrace finds a line through ld64's debug map, which ld64 writes only for objects with DWARF in them, and ra-jit writes DWARF for ELF and COFF only, so the frame has a name and no line.
- a simple let lowers straight into its binding, as rustc's MIR does
- debuginfo.py no longer calls perf's short stacks a fault of the CFI when perf looks the CFI up in the wrong place. perf's libdw unwinder takes a module's base as the mapping's start less its file offset, a page off for the executable segment lld lays out a page above its offset, so each frame's CFI is read from the code a page below it and a stack stops where that code is in its prologue (ra-jit::fold::2274 on the corpus, looked up in new::2229's push of rbp). The CFI is right: eu-stack and std's unwinder reach main. Where perf is told the right base, a stack that does not reach main is still NOT UNWOUND, and perf's stacks are printed.
- A 128-bit integer argument that finds one integer register left on x86-64 is passed in memory on the stack, alone or as half of a pair, so Cranelift places the arguments after it where rustc and LLVM do, and the LLVM backend leaves any other signature of that shape to Cranelift.
- Live edits are timed step by step: the worker and the program stamp each edit, harness/live_latency.py splits save-to-running, live-latency.yaml runs it on macOS arm64
- Edits go to the daemon as text, and the program applies the patch as it lands
- live_latency.py reads the log of a worker that was a spare
- Live edits time the refusal checks and the patch's layout for the program apart
- Live edits time the change to the database, the statics and the patch's object apart; live_latency.py takes new code that runs before its outcome is said
- Live edits time the setup of a patch's object and the outline of the edited file apart; one file's bodies are lowered and hashed without threads
- A worker's socket fits macOS's 104 bytes under a long RA_JIT_HOME
- Installing the daemon's programs holds cargo's lock on the build directory
- The harness counts only the disabled build's own calls of the user's wrapper
- The toolchain's commit is read from rustup's manifest once while it is unchanged, not on every build: a patch spent 2 ms on it
- Merge branch 'parity-let-fix' into merge-letfix-rustd
- Merge branch 'rustd-issues' into merge-letfix-rustd
- Merge branch 'fix-issue5-quadratic-drops' into merge-letfix-rustd
- site: the ra-jit-updates D1 database is created and migrated; /updates is live
- README: the signup button is centred and the mp4 link is gone
- README: live-edit and build tables with speedups (24x for a one-function live edit), and what is not measured yet
- A panic unwinds through a spliced macOS executable again: dyld on macOS 15.4 to 15.7 finds `__unwind_info` and `__eh_frame` only by the name of their segment's load command, and the splice had put both tables in `__RA_JIT_TEXT` and renamed the program's, so the unwinder found none and every panic aborted. Now the tables stay in `__TEXT`: `__unwind_info`, which the linker puts right in front of `__eh_frame`, moves into the header's spare room, and the patch's frames take its place, so `__eh_frame` starts with them and no section overlaps another, which `strip` refuses.
- main.rs is formatted the way rustfmt wants, so the format check passes.
- Real projects measured: cargo ra-jit build against cargo build on ripgrep 15.1.0, just 1.43.0 and nushell 0.108.0 on a 64-core x86-64 host. A library edit is 10x, 14x and 26x faster; the first build until the daemon is ready takes about twice cargo's. harness/realbench.py, docs/results.md section 11 (issue #7).
- where the time of a build goes: spans of client, daemon and worker, RA_JIT_TRACE, a breakdown that sums to the total
- warm edits: nushell's main edit 0.40 s to 0.30 s, a library edit 0.21 s to 0.18 s
- results: warm edits before and after on dev-aws-6, with where the time goes
- an edit inside one function lowers only that body; panic places and debugger rows come from the body alone; harness/movedlines.py
- LLVM gets rustc's attributes (tiered Life step 528 to 763 generations/s); edits never wait for LLVM; the sampler stops when its reader is gone; harness/tierbench.py
- Cargo.lock: ra-jit depends on tracing, so cargo no longer rewrites the lock on every build
- README and site: credits thank the projects ra-jit stands on
- macOS edits spliced for programs with a large unwind table; faster code signing of splices
- dep-info read as rustc writes it (#14); the macOS headerpad sized from the program and kept beside it (#13)
- harness/live_e2e.py: the new answer the program prints before `outcome:` counts, and the harness builds with the toolchain the others pin
- rust-analyzer: the solver grows its stack as rustc's does, so an unoptimized build infers as deep as an optimized one
- docs/harness.md: spaces.py and headerpad.py
- nix/units: the daemon's unit graph with stacker and psm, which hir-ty's solver grows its stack with
- CI: one build per platform, harnesses in parts of a few minutes, unit tests with the known failures named, one 'ci' check
- rust-analyzer: a tail's temporaries before the 2024 edition are dropped after the block's locals also when the tail unwinds, as rustc's (breakers: unwind_tail_after_local)
- CI: unit tests read cargo's colored output and run rust-analyzer's slow tests; breakers names the open cases that pass now, and frame_of_tables and backtrace_line on macOS LLVM do
- rust-analyzer: what leaving a scope early drops (break, continue, return, a future dropped at an await) is written at the scope's end, as rustc writes every drop it schedules (debuginfo: the corpus's async_drops frames)
- debuginfo.py: what is known to be wrong is named in OPEN with why: rung2's ra-jit build polls a future rustc's code makes and lays out apart
- buildmodel.py: a case known to come out WRONG says why in its case.json's `open` (all hosts, or by host), shows as open, and fails once it does not: doctest_future_generic, no_std_no_main on Linux, lto_single_linked on Windows
- splices.py: Windows splices 60 of the 71 steps, not 61: the step with nothing edited links nothing, and the ten full links are the ones the count was set for
- storefull.py: only rooms smaller than two thirds of what a cold build writes are tried, as LLVM on Windows writes under 512 KB and fills none of that
- storelib: the store harnesses work in /tmp on macOS, whose temporary directory is too long for a daemon's socket (storecorrupts)
- cargo_ra_jit.py: the second-user check shows what the request said on stderr when it printed nothing
- CI: the action bumps of dependabot's #3 (checkout 7, upload-artifact 7, download-artifact 8, cache 6, wrangler-action 4), which closed when main was rewritten
- cargo_ra_jit.py: a daemon that turns the second user away can close before the request is sent, a broken pipe the probe now reports as its answer (the uid check's '' on macOS and Linux)
- docs/harness.md: CI, what runs when and how long, ci.py as the one list of the gate, the known-wrong lists, the ruleset; CHANGELOG: tail temporaries on unwind, the lines of early-exit drops
- the daemon skips a build script's cargo check when cargo ra-jit just ran it, a script runs once per edit, the bench measures Cranelift as released; README bench table from dev-aws-11
- script.rs tests: the script a test has rustc compile is stamped as compiled now, as a copy on macOS keeps the time of the file it was copied from (a_script_newer_than_its_source..., a_script_that_read_a_variable...)
- Merge origin/main into the linux-speed merge: the runtime bench, new on main, takes the LLVM build, now that RA_JIT_BIN is the build without it
- Merge origin/main into the linux-speed merge
- #[ra_jit::cache] keeps the results of pure functions across runs, keyed by the code they reach (#17)
- release binaries are thin LTO (`dist`) with PGO; a worker loaded beside cargo's first build ends with its daemon
- benchreport: the build table says it times ra-jit's `cargo build --release`, and that release binaries are `dist` with PGO (docs/reports/RELEASE-BUILD.md)
- Merge origin/main into the build-profile merge (CHANGELOG: both Changed entries kept)
- the first edits patch what rustc linked, so the daemon is ready as cargo finishes (#11)
- Merge origin/main into the cold-start merge
- Merge origin/main into the cold-start merge again
- Merge origin/main into the cold-start merge once more
- cargo: rustc's link for Apple without chained fixups, so the first edits splice under nix (#30)
- daemon: an edit that comes as the own base starts is not lost when it is interrupted
- rustcbase.py: an edit that interrupts the own base may be either a patch or in full
- Merge origin/main into the cold-start merge, before pushing
- Merge origin/main into the cold-start merge, before pushing
- async blocks and async fn bodies borrow-checked (1172/1165)
- a program the daemon is writing in place is handed on once the write is over (#44)
- lifetime errors in async fn bodies, E0621 as rustc (1188/1181)
- Add Linux x86-64 live edits (#19)
- E0381 for indexed arrays and captures (1191/1184)
- the daemon's links keep code nothing reaches on every platform (after #23)
- script.rs tests: a script a test runs is written by another process
- hir-ty-wfcheck, item well-formedness E0309/E0310 (1200/1193)
- where-clause and GAT well-formedness (1204/1197)
- cargo is no longer stalled by the spare, lighter work beside cargo, generic copies patched (#11)
- on macOS a patch whose frames outgrow the unwind table is spliced, both tables in the header's room
- region constraints of proving trait bounds through the solver (Polonius 1223, NLL 1216)
- struct literal where clauses, array repeat Copy, method turbofishes, well-formed type-relative paths (Polonius 1230, NLL 1223)
- memory reports per request and on macOS, and what the daemon's memory is (#39)
- a closure's creator's late-bound lifetimes, annotated variables in closures, function items called through variables (Polonius 1241, NLL 1234)
- annotations in patterns, constructors called through variables (Polonius 1248, NLL 1241)
- bun 1.4.3's Rust port built by cargo ra-jit, in the harness, the bench and nightly CI
- assignment's drop is a write (E0506), match scrutinee read for moved values (1259/1253)
- includes.py: the steps begin once the daemon's own build from the store is over (#54)
- inlining across crates and runtime parity (1.21x of release's time over 15 benchmarks at the merge), cold build times beside it, AVX-512/NEON intrinsics
- match tests at the scrutinee, E0713, E0373 for block-bound closures, E0597 for static impl Trait returns (1278/1272)
- cargo fmt
- cargo_ra_jit.py: wait_warm names the checkout by its real path (#59)
- Workers hold at most half the machine's memory, or RA_JIT_MEMORY_MAX (#60)
- a crate outside the workspace named through a link is found by the link (#51)
- a file written while a worker loads is built as written (#53)
- the daemon links rustc's symbols.o from its copy (GNU ld, panic=abort)
- 'static in signatures and fields, Copy bounds, loops unwind, one E0596 per variable (1294/1288)
- axum's futures, bevy's examples and main, doctests with -q (#26, #27, #28, #32)
- well-formedness in bodies (1312 of 1579 Polonius, 1304 NLL)
- buildmodel: no_std_no_main is no longer open on Linux
- a fast CI tier for every change, the slow suites nightly, no run on main cancelled; parallel-lane workflow doc (#67)
- CI: a cargo ra-jit job that fails or hangs keeps the daemons' logs and what was running (#64)
- socket lock released on drop, plain cargo's settle waits for its cargo, stop waits for the daemon and its workers (#68)
- Linux live edits splice a running executable as a copy, every Linux program gets a build ID (#70, closes #16)
- an edit through a blanket impl's `impl Trait` is built, and an edit only a function left to rustc reaches goes to cargo (#25)
- with `panic = "abort"` no call unwinds, and none has a landing pad (#66)
- a source written since rustc built it is found through a link (#50)
- the pinned run-pass tests run under a link, on Linux x64 and macOS (#49)
- macOS: an executable linked with `-no_compact_unwind` is spliced (#33)
- fn-cache: core's functions that turn an address into data are not pure (#18)
- a daemon or --watch build does not inline across crates (#63)
- an interrupted build keeps what rustc's copies are of (#71); full-tier expectations; perf count on arm64; worktrees.py diagnostics (#72)
- Merge Bun nightly blocker E2E fixtures (#75)
- every README benchmark a matrix over macOS arm64, Linux aarch64 and Linux x86-64
- the own base is built when an include is not there (zed restarted it every second), and given up past a quarter of memory (#55)
- glancer.md says who has each item, with the no-rustc, flat-syntax and share-types lanes
- ra-jit never runs rustc (#80)
- nocompact.py asks one instruction in (macOS 15's unwinder backs the pc up, #74); every harness script imports in the fast tier
- CI: harness imports after the toolchain is installed (some scripts ask rustc for the host), failing on an import that fails only
- dyn bindings invariant, unsizing proves the object's trait, Self constructors, constructors called through variables, const fn promotion in const items (1317 Polonius, 1309 NLL)
- asm! takes its function's AArch64 extensions (bat's crc32w); whole-program links keep symbols.o so GNU ld takes the panic runtime (tokei, eza) (#78)
- E0793 for unaligned references to packed fields, E0507 for moves in match guards and out of enum casts (1329 Polonius, 1321 NLL)
- live latency: a number is judged once three runs before it show the runners' spread (benchhistory.py check --least); the same runner's medians differ by twice between rounds of one binary (#77)
- closures called through variables, fn items through references, self of impl dyn Trait (1337 Polonius, 1329 NLL)
- where ra-jit runs rustc, how often, and the plan to remove it (#94)
- bevy's last steps in no-rustc.md (#95)
- constructor-extended temporaries, expression statements (1339 of 1579)
- the glancer lane owns #37's part of no-rustc step E
- LLVM -O3 builds the shown bodies in first and cleans them as Cranelift's (nbody 1.97 -> 1.28 of release on Graviton4)
- x86-64 SIMD intrinsics and one region-erasure cache from runtime-parity-2, without its in-bounds addresses and SLP
- associated types in signatures, for loops over &mut (1363 of 1579)
- a patch of rustc's executable calls no local of an optimized object (#52)
- no syntax tree outlives its query; what syntax holds and costs (#42)
- no program size is a reason to link in full; bevy's panic in the drop passes (#79, #96)
- a spare worker only for a build that asks for one (#56); salsa lru with cycle handling measured, not merged (#57)
- raw pointer relations, let fake reads, statics, Fn::call; 1373 of 1579
- count every rustc and rustdoc run, refuse new ones; doctests built with their package's features (#97)
- written lifetimes of constants and closure signatures, SCC representatives, impl Trait meeting its bounds; 1392 of 1579
- nushell's forced-collect times
- an error the daemon finds fails the build as rustc writes it, with no rustc (#84)
- trait object upcasts, implied bounds of projections, raw pointer casts (borrowck 1400/1395, floor 1397)
- dropck of associated types and opaques, const values (borrowck 1405/1401, floor 1402)
- a flat syntax tree behind rowan's cursor; the compiler builds no green tree (#42)
- the README's benchmark charts in colour, a hue per platform, cargo the neutral reference
- associated types' alias bounds, impl anonymous lifetimes (borrowck 1414/1410, floor 1411)
- every benchmark chart ra-jit against cargo around 1x, with each platform's geometric mean
- opaque types' outlives, 'static from well-formedness, closure &dyn parameters, move error groups (borrowck 1422/1418, floor 1419)
- the merge queue lands every change, a pull request runs fmt alone; macOS norustc.py back from 25 minutes to 79 s; apt packages cached (#106)
- ra-jit answers rustc's questions: each asked of rustc once per toolchain (no-rustc B) (#100)
- FnMut escapes through by-reference captures, closure E0521 naming, annotated locals (borrowck 1429/1425, floor 1426) (#108)
- asm: no rustc to assemble asm! and global_asm! (no-rustc C) (#102)
- proc-macro expansions kept in the store (#38) (#110)
- bun built whole by ra-jit on Linux x86-64: every function compiled, linked, runs as cargo's (frame pointers, ELF sections, exported statics, same-name crates, macro fragments, cfg_if, dtoa, std copies, inline consts, adt_const_params, type_info); bun's Linux bench (#107)
- docs: steps A, B and C done, their owners and what is left; counts on ripgrep and nushell after them (#112)
- memcmp by its own name (tokei 1.51 -> 1.34 of release on Linux x86-64), and nushell links and runs: AArch64 crypto and NEON sums, masked loads and stores, dyn vtables of statics, narrow tuple constructors, provided-method futures, test-only global allocators (#111)
- a let's written type keeps the lifetimes an alias or a _ shares, constant patterns ascribed contravariantly (borrowck 1430/1426, floor 1427) (#113)
- a static a body's macro declares finds rustc's symbol: helix's own base in one round (#37, no-rustc E) (#114)
- const { .. } blocks checked with their function, as rustc checks inline consts (borrowck 1433/1429, floor 1430) (#115)
- docs/glancer.md: #56 merged, #57 closed, #55 restarted (#116)
- a callee's projection bounds relate regions, array subslice moves checked per element (borrowck 1440/1436, floor 1437) (#117)
- bun as a regular project in every README benchmark table and chart, and in the geometric means (#118)
- Runtime numbers of main 9c31e64d83 on macOS arm64, Linux aarch64 (first time) and Linux x86-64, every program matching cargo's output (#119)
- ra-jit answers rustc's -vV and --print questions from its own data, no first asks (fixture cold build 18 to 6, check 8 to 5) (#120)
- borrowck: match guards (E0510, guard E0596), array-to-slice unsizing keeps the element type, compound assignments through fields with lifetimes report read and write; 1451/1447 of rustc's tests, floor 1448 (#121)
- Linux AArch64's target specs in ra-jit's answers (cold build 7 to 6 there), and codegen options of no bearing answered without rustc (#122)
- asm! is never assembled by rustc: without LLVM or a C compiler the build fails naming clang to install (rustc start sites 20 to 18) (#123)
- doctests build the library with the features of the build cargo was asked for: ripgrep's globset example no longer crashes (#101) (#124)
- borrowck: unsizing inside a struct, closures typed against an Fn bound take its arguments, annotated variables related as type annotations; 1455/1451 of rustc's tests, floor 1452 (#125)
- the compiler path's hot walks read flat trees' arrays, not rowan's cursor (tree walking 4.9% to 2.05% of nushell's cold-start CPU, 10.0% to 5.0% of ripgrep's)
- borrowck: errors in an expression a macro writes are reported at the macro's definition, and by-value captures in a move closure returned as impl Trait give E0597 as rustc; 1459/1455 of rustc's tests, floor 1456 (#126)
- doctests: what rustc compiled of an example is reused while nothing it read has changed; the fixture's doctests 6 to 3 (step L, first part) (#127)
- steps F and G: the standard library compiled by ra-jit and served from the store, every crate of ripgrep compiled by ra-jit and linked with nothing rustc built (#86, #87) (#128)
- borrowck: function items used as values prove their where clauses, locals are Sized, a returned impl Trait's bounds hold of the type it hides, and closures see their impl's anonymous lifetimes; 1463/1459 of rustc's tests, floor 1460 (#130)
- tests: ra-jit writes the test harness, so a test added is a build the daemon makes; a library's change in its tests alone keeps rustc's build; the fixture's tests 12 to 7 (step H) (#129)
- bench: count each block of the disk once, so reflinked copies are not counted twice (#131)
- borrowck: async blocks and nested impl Trait in returned hidden types, an async fn's returned impl Trait in member constraints, C variadics' VaList; 1471/1467 of rustc's tests, floor 1468 (#132)
- bevy edits faster than cargo (body 0.90 s vs 3.0 s, leaf 0.27 s vs 10.4 s, new function 0.75 s vs 3.0 s; the first edit no longer waits for the whole-program compile), in_set and bitflags' contains compiled; D: deny lints, errors written before the daemon loaded, cargo ra-jit check (#98, #84)
- no-rustc G: ra-jit's own link line derived from the target and #[link] attributes, the global allocator shim, build-script cfgs computed by ra-jit, panic_abort, bodiless intrinsics skipped (#86, #87) (#133)
- taking away a function an edit added, or one rustc compiled, is a patch: no full link (new-fn-removed, fn-removed in rustcbase.py) (#137)
- macOS builds the llvm feature against the flake's pinned LLVM (#138)
- borrowck: iterator adapters' closures normalized in trait bounds, trait objects' 'static bound at unsizing and not from fields, aliases under a binder live by all arguments, Trait<'static> supertrait outlives; 1476/1472 of rustc's tests, floor 1473 (#134)
- H follow-up: integration tests get ra-jit's harness, cfg_attr on tests expanded, rustc's harness kept until the tests change (test-edit back to main's timings), and widen only takes over crates rustc linked (fixes the link failure after adding a test). ripgrep test-added 18 -> 0 rustc, nushell 11 -> 0. (#135)
- LLVM from the flake only: one pinned LLVM 20.1.8 for dev shells, CI and release builds; build.rs refuses any other (#140)
- cargo ra-jit check with no build loaded is the daemon's verdict, not cargo check (#141)
- borrowck 1499/1495 of rustc's tests (floor 1496): a shared borrow of a temporary built from constants is of rustc's promoted copy, loop exits ordered as rustc reports them, destructor conflicts through enum variants, and dropck of what coroutines and async closures capture (#144)
- store: compress sealed packs in zstd blocks (about a fifth of the size), bound the store to a tenth of its disk with ra-jit cache gc and the size in ra-jit status, and have bench.py say what each project adds to one store per machine (#136)
- deprecated_safe_2024, elided_lifetimes_in_paths and unsafe_op_in_unsafe_fn fail a build that denies them, 0 wrong on rustc's UI tests and 85 probes (#145)
- Merge borrowck-7: borrowck 1506/1502 of rustc's tests (floor 1503): rustc's unused_mut lint, and rustc's false edges between match candidates in the MIR lowering, so a later arm's guard or pattern is checked where an earlier one is tested (#148)
- frozen dependency artifacts: each crate's def map, impls and lang items served from the store with check and verify modes; ripgrep with a warm remote 4.25 s against cargo's 5.5 s (#40) (#149)
- #[ra_jit::cache] keeps each function's results for its last three codes, and cargo ra-jit cache lists and clears stale ones (#143)
- bun's whole program at 1.28x release (was 5.9x) and linking on macOS (#35); adding a function to bun a 0.6 s patch (was 19.8 s); a regression test for the rust-src check (#142)
- ra-jit allocates with jemalloc on Unix: the daemon after edits 1.57 GB instead of 2.03 on ripgrep, 9.52 instead of 10.80 on nushell, 11.07 instead of 12.13 on helix (#39) (#146)
- the daemon loads build scripts and proc macros from cargo's build instead of building them again (bevy's target 16.2 to 15.8 GB, ready 30 to 20 s), packs go to a remote compressed once, and docs/research/disk.md after F/G projected (#153)
- #[ra_jit::cache] takes Rc, Arc and HashMap arguments, keys by the sysroot's metadata hash, and looks through the MIR of core and alloc: _rdtsc and the like refused, inlining hides no address (#18 items 1-3) (#150)
- Linux x86-64 bench at 28f22ff4a8: bevy edits faster than cargo (a function 3.15 s to 650 ms, a widely used one 10.69 s to 240 ms) (#156)
- docs say the workflows that commit to main push with RA_JIT_BOT_TOKEN past the ruleset, not GITHUB_TOKEN (#157)
- Merge borrowck-7: borrowck 1509/1505 of rustc's tests (floor 1506): the super lets of format_args! and pin! extend their temporaries only when a let extends them, as rustc since #145784, a variable another crate's macro declares is a temporary in the messages, and an error at what a macro lowers to is at the macro call (#151)
- link only what is reached of the standard library: the unit at line tables, split into an archive of 17 parts lld reads in parallel; hello's link 160 -> 130 ms, executables 20.4 -> 15.2 MB and ripgrep's 55.1 -> 50.1 MB (#87) (#152)
- the daemon keeps ra-jit.o zstd-packed between the links that read it on Linux (helix's target 5.0 to 4.5 GB, nushell 6.2 to 5.8), and docs/research/disk.md with it (#158)
- Release parity: a returned local is built in the caller's memory and a moved argument shares its slot (NRVO), rustc's ThinLTO pre-link and link pipelines, the SLP vectorizer on again, NEON's 16-byte addp as iadd_pairwise, and a trait's provided method's inner functions compiled with the crate ra-jit names. Runtime against cargo's release: nbody 1.26 to 0.98 on x86-64 and 1.29 to 1.05 on Graviton4, qsort 1.11 to 1.04, eza 1.08 to 1.00-1.06, words 1.28 to 1.24 on Graviton4 (#160)
- Debug type sets share what each type is told as (#55); the own base gives memory back before giving up (#163)
- borrowck: no false errors on tracing 0.1.44's trace! fields (#109) or on a trait object given for a parameter bounded T: 'a (#84) (#164)
- doctests: ra-jit compiles and links the examples with no probe (L's second part); kept programs and parallel compiles; the fixture's doctests 3 to 0 (#162)
- tests: ra-jit's harness always, written when a program of tests is first asked for; test-edit 0.2 s on ripgrep, 1.4 s on nushell; fixture tests 7 -> 6 rustc runs (#169)
- borrowck: all of cargo 0.99 checks clean: impl header object lifetimes, promoted closures, unwind drops of values without drop glue, async fn hidden type lifetimes (#170)
- LLVM is told what each read-only constant holds; tokei 1.39 to 1.34 on x86 and 1.29 to 1.21 on Graviton4, bun 1.27 to 1.24 (#171)
- land.py's waiting survives the caller and a tool that ends its process group, logs to .git/land, and land.py --wait follows it again (#172)
- Roots from the store: a checkout's own functions put in the object from the store when it has them whole, asked about once on every core, the crates listed on every core; ripgrep, nushell and helix with a warm remote under cargo's cold build (4.0 s against 5.5, 31.5 against 33.1, 26.0 against 31.3) (#166)
- main green again: storefull.py on a 64 MB disk, which takes the 18 MB of frozen def maps a cold build now keeps (fixes #174), run in the merge queue's fast tier; vendor/'s ra-jit edits formatted, and CI's fmt checks the whole workspace (#175)
- borrowck: no panic on concrete associated types in where clauses, and a checker panic fails the gate (#177)
- splice: one copy of what a debugger reads, the linker's let go of once copied and a section moving only behind every room: bevy's target on ext4 23,487 → 21,930 MB, helix's 5,471 → 4,964, XFS and edits unchanged; disk.md: the daemon's program against rustc's by section, compressed debug information off (#182)
- runtime on Linux x86-64 and aarch64 and bun's on x86-64, at e8d8a63da4 (#183)
- frozen def maps name the file each include! reads: bevy's second checkout no longer panics reading crunchy's def map before its OUT_DIR file is loaded (#178)
- borrowck: lifetimes needed by normalizing signatures and where-clause projections, and item check of projections in fields (1517 of 1580) (#181)
- build scripts compiled and linked by ra-jit (step I, first part): every crates.io script through the wrapper, runs kept in the store, the standard library's unit made once per store; fixture cold build 6 -> 5 rustc runs, ripgrep's scripts 7 -> 1, nushell's 50 -> 5 (#186)
- Frozen def maps name the files their include!s read, so a warm store loads them before serving (#161, #40); harness/frozenwarm.py covers a warm-store second run over macro_rules from an included file (#179)
- an edit never links in full for want of an own base: a build in full is spliced whole into rustc's executable on Linux, and the edits after it patch (#159) (#184)
- a whole program's compiles import what they call, as ThinLTO's backends: bun builds in 129.2 s against cargo release's 135.6 s on x86-64 (was 666.9 s) at 1.22x, and 160.2 s against 167.4 s on Graviton4 (was 694.1 s) at 1.17x; ripgrep, helix and nushell 3-8x faster to build; bun's Linux aarch64 rows; a transitive import's call type and dit asm fixed; norustc clears a target's RUSTFLAGS (#185)
- borrowck: impl Trait with late-bound or elided lifetimes, inferred outlives bounds, for<'r> closure bounds, function item return types and trait-object associated type bounds (1523 of 1580) (#187)
- README for people using ra-jit on their own projects; binstall metadata (#191)
- nix: unit graphs with hir-ty-borrowck's new hir-ty-wfcheck dependency (#192)
- borrowck: guarded arms bind by value after the guard, const promotion as rustc, nested impl Trait member constraints applied, unit cfgs in the scorer (1532 of 1581) (#193)
- Fix the miscompiles fuzzing found on main: on aarch64 a 128-bit integer that goes to the stack in the Rust ABI takes the integer registers left out of use, as LLVM and rustc do, where Cranelift gave them to the arguments after it (#189: told-rpa-41's wrong checksum and land2-rpa-33/-85/-96's crashes); reify and fn pointer shims place the arguments after a receiver as their callers do (#190: told-rpx-57 c20 on x86). The 7 reduced programs go in harness/fuzz/found/, and CI's new Linux part quick-3 replays all of found/ under each backend (a small change to harness/ci.py) (#194)
- the standard library shipped with each release and fetched over HTTPS before a build with nothing of rustc's first reads the store: empty-store cold build ripgrep 11.6 -> 4.4 s, nushell 38.9 -> 27.7 s; push compresses packs (std 65 -> 18.8 MB); cmpxchg16b compiles (#195)
- borrowck: E0510 in indexing expressions, member constraints through a nested impl Trait, closures mutating through a captured Box (1535 of 1581) (#196)
- each debug type described once, in groups of the types made of each other that every set shares: zed's own base finishes (62.6 GB, 662 s) and links when a link that keeps unreached code fails on what only that code uses; after the base and edits nushell 7.63 → 7.36 GB, helix 6.55 → 5.87, bun 15.97 → 14.47 (#29) (#197)
- MIR ways out share their drops as rustc's drop trees do, and a call's arguments end with it: Xlib's open (x11-dl, 781 fields each ?) 2,986,013 -> 29,201 blocks, bevy's ra-jit --crate breakout --dev-deps 364 GB OOM -> 76 GB peak, 516 s; growth.py covers a struct of fields (#198)
- Linux aarch64 bench at c93e530e47: bevy edits faster than cargo (a function 22.81 s to 884 ms, a widely used one 32.13 s to 352 ms); aarch64 headline 17x to 24x (#201)
- build scripts that include! files or read variables with env!/option_env! are compiled by ra-jit: their dep-info names the files and variables (# env-dep), include_str!/include_bytes! files key the store; ripgrep cold 39 -> 38 rustc runs, nushell 493 -> 490 (#199)
- A returned impl Trait captures the lifetimes elided in its function's parameters, so mutating self while an iterator from fn evens(&self) -> impl Iterator<Item = &i32> + '_ is live is E0502 (1536 of 1581) (#204)
- the axis hints under the edits chart no longer overlap when nothing is worse than 1× (#205)
- proc macros, J0 and the plan: a build script that links a proc macro is compiled by ra-jit, the macro expanded with rustc's library of it; nushell cold 490 -> 488 rustc runs; docs/no-rustc.md plans J1 (ra-jit's own proc-macro server) and J2 (proc macros compiled by ra-jit) (#207)
- Build exports of bodies up to 16 Cranelift instructions per LLVM instruction of room, not 4: Cranelift's IR of small std calls is many times LLVM's (SipHash's d_rounds is 1792 Cranelift instructions and about 50 LLVM ones), so at 4 no export of it was built and every hasher called it out of line. kernels words: x86 1.27x -> 1.14x, aarch64 1.38x -> 1.23x; geomean of kernels, numeric, jsonrt, ripgrep, fd, tokei, bat: x86 1.14x -> 1.12x, aarch64 1.15x -> 1.14x; build time +0-5%. (#210)
- unused_mut fails the build, scored on rustc's UI tests with --deny; unit-typed expressions give their place () (#211)
- J2: a program links proc_macro, ra-jit links shared libraries and writes a proc-macro crate's list of macros into its library; difftest fixtures build in their own target dir (#213)
- RA_JIT_UNITS=1 compiles a whole program in codegen units, many functions to an LLVM module, found from IR in one round with kept imports; off by default, as helix builds in 71.1 s against 69.7 s and runs 1.28x against 1.21x on x86-64, and LLVM takes 4x rustc's time for code of the same size either way; imports linked to each other two at a time, then into the module once (#176) (#209)
- Prove for<'a> bounds for placeholders, as rustc does: &'static str: DeserializeOwned and a forwarding impl that needs more than the where clause gives are rustc's higher-ranked lifetime error (1539 of 1581) (#212)
- compile-ahead gives up on what it compiles once the symbols are read: bevy's second warm-store run ready in 26.5 s, was 148 s; salsa ignores a thread's own cancellation in cycle-recovering queries, so the threads are stopped as a write stops them and the trait solver checks per goal (#217)
- the SSE and AVX compares lowered (cmp.ps/ss/pd/sd and the 256-bit ones): glam's Vec4::cmpeq and is_finite_mask, which bevy calls, compiled (#218)
- Linux x86-64 bench at 42cb85270f with the standard library fetched as a release does: the daemon after edits helix 12.3 -> 6.0 GB, bevy 39.6 -> 15.0 GB, nushell 10.9 -> 7.2 GB; cold builds slower and larger (#203) (#221)
- LLVM builds put asm! blocks where they are called, as rustc does, instead of calling their wrappers: each block's template, operands, clobbers and options become LLVM inline asm in the caller (x86_64 and aarch64; blocks with labels, sym operands, clobber_abi, noreturn or may_unwind, and vector registers keep the wrapper; RA_JIT_INLINE_ASM=0 turns it off). bun's stack check (1.5% of its samples as a call) is gone from its profile; bun's run time is unchanged at 1.21x. A small change to vendored rust-analyzer's MIR: inline asm keeps its clobber_abi and whether each register operand may be written. (#208)
- Borrow-check the constants of a signature (array lengths, const generic defaults); 1541 of 1581 of rustc's tests (#219)
- J1: the release's proc-macro server built by ra-jit, loading ra-jit's proc-macro libraries, held against the toolchain's server (#220)
- f32::max in a constant is evaluated (core's fallback body), so nsh apps' builds stay whole-program and their edits patch; a trait method left without an impl names its Self type (#224) (#228)
- biome: the first edit after a cargo build stays the daemon's, memory bounds follow the control group, biome in the bench set (#229)
- cargo ra-jit test: RA_JIT_TESTS=affected runs only the tests an edit reached since they passed, from the daemon's hash of each test's code and all it calls (#227) (#230)
- docs/research: the store's block cache measured on helix's warm start (no gain), and bbtree against papaya and DashMap for ra-jit's shared maps (#231)
- AGENTS.md: own the whole stack, and stay a superset of Rust (#234)
- cargo ra-jit check writes rustc's JSON diagnostics as cargo check does (424/424 fields against rustc), -Z, --exclude and several targets checked by the daemon; the borrow checker's 4 trait-solver panics fixed at their causes; the 5,204-arm else-if chain in 4.5 s (#206); ide-diagnostics passes and runs in the fast tier (#222)
- Affected tests: TypeId::of and the dyn Any methods are leaves of the hash walk (#238)
- build lists the crates it asks for on every core: a signature edit's roots stage 53 to 30 ms, edit-to-binary 410 to 373 ms on nushell (#236) (#239)
- harness: leanprof.py and cpufold.py --phases; research: lean-frontend.md, where nushell's cold start, edits and heap go and the lean path ranked (#236) (#240)
- warm start with nothing of rustc's: store entries read once, interner found under a read lock, future types walked once, edits do not wait on store writes, a stopping daemon lets workers finish writing; helix's restarted build on a warm store 106.5 -> 38.3 s, store reads 561 -> 131 s of thread time (#41) (#235)
- Affected tests: say why a test runs every time as the call path to what it reaches (cargo ra-jit test -v, a count in the summary); a HashMap's seed and a backtrace count as the test's code, ripgrep's always-run tests 881 -> 469 of 1,144 (#227) (#241)
- the patch base is asked one mark per root and per instance, not one per row (#243)
- salsa is vendored, and an interned value already verified this revision is checked without the shard's lock (#244)
- a build verifies the impls of the crates it builds on every core, beside the stale check (#245)
- borrowck: the locals initialized at drops are found from the initialized paths, not by walking every local (#246)
- Affected tests: a failing test program forgets only the tests libtest says failed, helix-core with no edit 170 -> 14 run of 170 (#227) (#247)
- helix builds again: the fn pointer a match of fn items or closures coerces to holds for any lifetime they elide (#212's higher-ranked lifetime error) (#251)
- biome's first edit after a cargo build is the daemon's: a source a build script touches before cargo's rustc reads it is no change (biome's body edit 232 s and 27 GB to 0.7 s) (#250)
- a build names its crates through queries of the crate graph, not by walking it for every name (#252)
- Cranelift: the self-relocation placeholder FuncId is u32::MAX - 1, so debug builds of ra-jit no longer refuse every function (split from #237) (#254)
- Test harness: a test in a private module named as a public function beside it is reached; helix-core's edits are the daemon's again (#227) (#256)
- a tracked function's interned arguments can be kept for good, and six hot ones whose arguments salsa never collects are; cpufold.py counts samples by period; lean-frontend.md, slices 2 to 4 (#253)
- Doctests: one load of the workspace for every library's examples, and the daemon's store; warm cargo ra-jit test --doc on ripgrep 82.6 -> 3.3 s, helix 125.3 -> 9.7 s (#28, #227) (#248)
- docs/ci-runner.md: our own CI runner on ix, a warm ra-jit seed per repository and a copy-on-write fork per branch (#264) (#266)
- a daemon's first start after a cold build finds what that build stored: one toolchain identity either way (nushell restart 0 to 103,750 functions found, cold compile 17.9 s to 11.9 s) (#267)
- tests that reach outside their code run in a Linux sandbox and rerun only when what they touched changed (#257) (#263)
- ra-jit is renamed blaze: `cargo blaze` is the command, `cargo-blaze` the crate and binary, BLAZE_* the settings; `cargo ra-jit` and RA_JIT_* keep working (#268)
- docs/roadmap.md: blaze-next, the floor of an edit for coding agents: Merkle VFS, sans-IO core, push invalidation, image state, agent API, lints (#279) (#280)
- frozen def maps of a crate with a build script are shared by every checkout: OUT_DIR keyed from cargo's build dir and named by a mark in the bytes (nushell second checkout 221 to 374 of 478 crates) (#281)
- research: lean-frontend.md, slice 4's four functions measured alone, and #236's rest left for blaze-next (#282)
- Unit tests: an edit to a library they link is the daemon's, not cargo's; ripgrep's edits run the tests they affect (#258, #84) (#265)
- roadmap: the agent API in deterministic CBOR, and the concurrency model (a shared-nothing control and IO plane, a compute plane of snapshots, work stealing and one committer, a deterministic scheduler) (#284)
- README and install.sh: blaze installs with one line (install.sh, checked on Linux and macOS), and the docs say `cargo blaze` (#285)
- Repo renamed indexable-inc/blaze: land.py, release, bench, site, templates, workflows and crate metadata name it (#288)
- Test harness: ra-jit writes it for benches and an ignore with a list too, so a test added there is the daemon's (#88, #258) (#286)
- biome joins the README's benchmarks and charts (Linux x86-64, both backends) (#289)
- bench: biome in the README chart and tables, Linux x86-64 at 1a005421b7 (biome body 2.4 s, leaf 0.38 s against cargo's 9.9 s) (#290)
- Debuginfo: work out the types of variables only when variables are written (#225)
- cargo ra-jit: a build that goes to cargo says it is a bug, and testselectbench fails on one for ripgrep and helix (#84, #258) (#287)
- patch marks keyed by stable names: a daemon started on a store takes the checkout's code as it is, no rebuild before the first edit (nushell restart 30.8 to 21.4 s) (#295)
- README heading: ix mark vertically centered on the blaze wordmark (#297)
- cargo blaze builds with Rust 1.99.0 whatever toolchain the project pins (#269): no build goes to cargo for it; plain errors when 1.99.0 or rust-src is missing (#296)
- docs/self-contained.md: the plan for a blaze with no rustup and no cargo that resolves as cargo does (#298) (#301)
- blaze-build-script: the environment cargo 1.99.0 gives a build script, computed with no cargo (#89, #298) (#304)
- README: an animated pixel-fire header and a short, scannable README; the long parts move to docs/install.md, docs/usage.md and docs/benchmarks.md (#308)
- docs: J, K and M planned together (#298): J first, K behind it, M last (#313)
- A cargo daemon takes the checkout's roots from the store too, not only a build with nothing of rustc's (#320)
- Update tagline in README for clarity
- blaze-build-script: build script output read as cargo 1.99.0 reads it, and who receives what (#89, #298) (#306)
- README: the three commands to try in a plain block (no builtin colour on `test`) (#323)
- Make the first build the daemon's: after cargo's build, wait for the daemon to load the workspace and build the program, with one quiet Waiting line (#310)
- splice: a function shorter than a jump takes it over its padding, or hops to traps near it: biome's leaf edit 360.7 s to 0.50 s (#242) (#303)
- ra-jit std ships the standard library's unit of a dev program too: a whole build from nothing no longer compiles it (#91) (#330)
- J1: proc macros of a load linked by ra-jit and expanded by its server behind RA_JIT_PROC_MACRO_SERVER (#90); macroserver.py compares every proc-macro call of the bench apps, 3,465 the same (#329)
- blaze-manifest, blaze-registry, blaze-resolve and blaze-build-script together (#304 #305 #306 #307 #309 #311 #312), workspace members as crates/* (#298) (#314)
- cargo blaze reads the workspace with blaze-manifest, not `cargo metadata --no-deps` (#298): equal to cargo on 385 of 385 workspaces (#319)
- J: blaze's proc-macro server ships in the Linux release beside ra-jit, built by ra-jit; RA_JIT_PROC_MACRO_SERVER=1 finds it there and the toolchain's server starts only for a library blaze's does not serve (#343)
- install.sh without rustup, blaze brings its Rust; install.yaml on a machine with no Rust (#298) (#340)
- J: proc macros are blaze's by default: the daemon's loads, the checker and build scripts expand with libraries ra-jit makes from each crate's record, kept in the store by content; macroserver.py builds each app again over the store (#348)
- blaze-registry fetches every source a lock file names as cargo 1.99 does ([source] replacement, vendored directories, git checkouts), held to cargo fetch by harness/fetchdiff.py (#298) (#324)
- docs/self-contained.md: the no-rustup install measured on a clean machine, the metadata switch (#298) (#352)
- a workspace resolved as cargo 1.99.0 does around its resolver: two resolves, [patch] and the lock file's locks (#298) (#339)
- a name is looked up in an index of the names in rustc's function symbols: biome's first body edit 6.63 s to 5.25 s (#325) (#356)
- The store keyed by what ra-jit was built from, crate ids below CARGO_HOME, build-script runs keyed with the checkout and target directory apart (#105) (#326)
- cargo blaze build and run from nothing build the programs whole with blaze, no cargo build, behind RA_JIT_WHOLE_BUILD (no-rustc K, #91) (#345)
- keep calls of rustc's copy of a function of the checkout in the store (#216) (#360)
- blaze-manifest: .cargo/config.toml as cargo 1.99 reads it (config get 103/103) and profiles as cargo computes them (86/89 over 235,924 units) (#298) (#354)
- Linux builds held to glibc 2.34 (RHEL 9), and install.sh to blaze run checked in Debian 12, Ubuntu 22.04 and RHEL 9 containers (#351) (#355)
- a build over rustc's base compiles the statics and TypeIds it needs in one round, and one with thread-locals of its own is linked in full at once: biome's signature edit no longer falls back to cargo (#299) (#358)
- blaze-build-script: build scripts run as cargo runs them, held to cargo by harness/scripts.py over 463 runs (#89, #298) (#341)
- closures named by their place and their source, so an edit above one keeps what the store has (#362)
- site: Flecs content, Rust renderer in wasm, RFCs with inline comments, charter, narration (#363)
- whole build: names, debug info and store entries made on every core after the compile, the store written on several threads, roots and data-only instances given to the threads that compile ahead; nushell's cold whole build 88 s to 70 s on dev-aws-8 (#361)
- the daemon's loads read the workspace with blaze's own resolution (blaze-metadata): no cargo metadata, the graph of the build, axum's whole build builds (#364)
- whole build: a request to the daemon, whose worker loads the workspace, links each program before it answers, and builds the edits after on what it loaded (#91) (#365)
- the first edit after a restart: code from the store told unchanged by its items; ripgrep 382 to 17 functions asked, 0.31 to 0.22 s (#366)
- whole build: edits spliced into the executable the daemon linked, and the programs planned by blaze's own resolution (#91) (#367)
- added and removed types and new statics go out as live patches (#368)
- blaze builds with the latest nightly, nightly-2026-10-25, in place of stable 1.99.0: rustc_abi and the proc-macro server from it, deref patterns, -Z flag table, cargo's second build-dir layout, TypeId numbering, nightly cargo-features (#372)
- Cranelift probes the stack on every target rustc does (#374)
- a static in a const _ block is looked up under rustc's _ segment (#376)
- Stateful statics stay rustc's wherever rustc linked them (#373)
- A cargo build of one program leaves the others the daemon built as they run (#375)
- A local no place mentions has no stack slot (bug D: closure frames held dead copies of their captures) (#377)
- every test runs in a sandbox by default, on Linux and macOS (Seatbelt and an interposer), one per test executable, with per-test records and opt-outs (#257) (#383)
- five external PRs (tobi #370 #371, gakonst #292 #293 #294) (#381)
- splice: a stub for a call of a patch's function that replaces one of the executable (#380)
- whole build: the cold compile's store names listed once per crate, frozen parts on every core, sharded type sets and interner front caches; RA_JIT_RUSTC_BASE reaches its worker (#384)
- let go of the own base's MIR and code after a build, give pages back (#261) (#385)
- rustcbase typeid-ctor: an edit under axum's Body::from_stream(stream.map(Ok)) is built by the daemon, not cargo (#369) (#389)
- keep an interrupted own base's work, set lru only when serving, test salsa's patch (#261) (#387)
- stateful statics LLVM renamed or kept private are rustc's copy, or the function is refused (bug E) (#379)
- reth's first edit after ready: the define pass's statics and closures counted once a definition, on-demand functions wanted once a crate (#259) (#391)
- restart: stored roots' lookup sharded, their types read when described, def maps of crates with proc macros frozen (nushell restart 15.6 s to 12.6 s) (#394)
- AGENTS.md: no fallback, ever; fail fast with a clear error so the bug gets fixed (#395)
- No fallback: a stateful static whose copy in rustc's code can't be shared stops the build with an error naming it (CodegenError::Refused), instead of leaving the function to rustc (#396)
- every cargo blaze subcommand is blaze's own: none is passed to cargo (#397)
- build scripts and proc-macro servers run in the sandbox by default, with per-package opt-outs, read-recorded reruns, and hardening: own session, no TIOCSTI, host Unix sockets, Mach services and io_uring refused (#147, #257) (#390)
- cargo blaze tree, pkgid, fetch, clean, new and init are blaze's own (#398)
- blaze-fmt: blaze's own formatter, rustfmt ported onto blaze's syntax tree, byte-equal to stock rustfmt on the bench apps (#400)
- AGENTS.md: blaze is its own toolchain (one nightly it controls), moving off rustc; never follows the project's toolchain or pins stable (#407)
- CI as a merge gate of minutes and a release train on main that releases when green (#419)
- serve the site at ix.dev/blaze; ra-jit.ix.dev and blaze.ix.dev 301 there (#411)
- cargo blaze fmt is blaze's own: blaze-fmt in process, cargo-fmt over the targets or rustfmt over the files named (#415)
- borrowck: no false errors on helix or biome (#406)
- route ix.dev/blaze* so /blaze?query reaches the worker; document the manual deploy (#424)
- agent rules move to typed agents/rules.ts; a SessionStart hook injects the numbered rules without reasons; AGENTS.md is generated and checked in CI (#417)
- cargo blaze fmt and clippy are blaze's own, enable links them; search, owner, yank, login, logout, uninstall, read-manifest, report, config, remove and vendor are blaze's own; borrowck fix (#416)
- macOS: the sandbox's library carries an arm64e slice; the prebuilt std's names are read without Mach-O's underscore (#425)
- cargo blaze doc writes HTML documentation from rust-analyzer's HIR, with no rustdoc (#410)
- codegen: finish the block before building unwind pads (drop_ladder, guarded) (#426)
- say blaze, not ra-jit, on every page and in the narration (#428)
- cargo blaze doc documents the crates cargo does, links undocumented crates at html_root_url, applies blanket impls as rustdoc does, lifts rust-analyzer's macro token limit (#427)
- AGENTS.md rule blaze-builds-blaze (#432)
- build(deps): bump the actions group with 4 updates (#430)
- README: Using blaze at work? (#422)
- cargo blaze: Windows MSVC builds go through the daemon (#346)
- ra-jit-live: live edits on Windows x86-64 (#347)
- Whole-program builds lay functions out in the order they were asked for, so every build of the same source is the same object (#200) (#409)
- blaze is its own toolchain: blaze-<date>, linked into rustup as blaze, a note when a project pins another (#405)
- A macro call in parentheses or brackets with no `;` after it has to expand to one expression, as rustc parses it (#154) (#429)
- Fix binding references in nested match guards (#393)
- One path: build, run and test are the whole build, and blaze runs the doc tests: no cargo build, rustc or rustdoc (#408)
- the LLVM backend on our fork's LLVM 23.1.4, linked statically and shipped in release binaries; LLVM's diagnosed errors are errors; asm texts in the store key (#431)
- LLVM's inliner at threshold 500 by default, measured against 225, 1000 and nothing built in by blaze; the threshold is in the store key (#434)
- No-rustc E: generic functions aren't roots and pathless functions are named by place, so std's 12,108 'no rustc symbol' are gone (#439)
- On macOS the doctests are ra-jit's: the standard library's unit is not weakened (#255) (#440)
- define the injected extern crate std in a 2015 crate root (#437)
- No-rustc E: #[rustc_comptime] functions aren't roots; the const-heap intrinsics run rustc's runtime bodies (#441)
- x86 float compares read their predicate as an immediate (literal, inline const, or a const generic cast) (#438)
- docs: remote-cache.md, design of the shared remote cache (Worker gateway, public/private CAS, signed index) (#444)
- x86 crc32, pternlog and 512-bit pshufb intrinsics, so crc-fast and blaze's own whole build compile (#442)
- Install line: curl -fsSL https://ix.dev/blaze/install.sh | sh (the site serves install.sh from main; blaze.ix.dev redirects there) (#446)
- x86 avx512 permvar intrinsics, the last function of blaze's own whole build left without code (#447)
- Shorter install line: curl -fsSL https://ix.dev/blaze | sh (site worker serves install.sh at / to curl/wget, HTML to browsers) (#448)
- in a load nothing is rustc's: ra-jit links the build scripts and proc macros and only describes their libraries (#450)
- Fix three refusals of blaze's own whole build: asm! const operands as anonymous constants, AVX-512 IFMA intrinsics, stand-in messages read from their own section (#452)
- Build scripts run with blaze as RUSTC: probes, -vV and --print answered by blaze, no rustc (#451)
- whole build: later programs read the store in batches, keep entries across rounds and learn the others' rejections (230 s -> ~13 s a program) (#456)
- ci.py: no timing line for a harness that is gone, and stop at one (#413) (#449)
- borrowck: no false errors in blaze's own tree (elision ignores the impl's lifetimes in Self; a trait object's implied bounds) (#457)
- agents: heavy work runs in ix VMs, not AWS dev hosts (#459)
- No-rustc F: the toolchain's commit from its channel manifest or blaze's pins, not rustc's libstd rlib (#436)
- No-rustc E: lane-by-lane simd intrinsics, funnel shifts, frem_algebraic and one-lane vectors by value are compiled (#85) (#443)
- on macOS an edit is spliced into the daemon's own link, not built in full (#445)
- the check of an edit asks only for the bodies it changed (#455)
- a link_name written as the symbol is not prefixed again (aws-lc-rs on macOS) (#454)
- test: cover LLVM store checksum regression from #418 (#464)
- harness: borrowck_apps.py, borrow-check the bench apps (#465)
- one train concurrency group per ref (#472)
- blaze clippy reads the terminal's width only on unix, so ra-jit builds on Windows again (#471)
- borrowck: check nested bodies in unreachable code (#469)
- borrowck: buffer E0382 across a function and its closures as rustc does (#470)
- CI's ra-jit.tar carries cargo-blaze, which ra-jit enable needs (#473)
- wfcheck: the bounds of an impl Trait a trait's method returns are well-formed (E0310/E0311) (#474)
- fix two borrowck test fixtures that only fail in fork alone (#476)
- borrowck: a type that must outlive any placeholder must outlive 'static (#475)
- the standard library's metadata is blaze's, no cargo metadata of the sysroot (#477)
- a program that reaches a function ra-jit refused is an error naming it and why, not a link failure (#479)
- config_get test stops the config walk at its scratch dir (#478)
- borrowck: prove a closure's creator's where clauses at its creation (snocat-regression) (#480)
- doctests and whole-workspace link line as nightly-2026-10-25 has them (buildmodel WRONG x3) (#483)
- No-rustc E: NEON intrinsics, barriers, rndr, rounding simd intrinsics and small vectors by value compile on aarch64 (#85) (#482)
- borrowck: relate function item types with the item's variances (#481)
- borrowck: a cast's user type annotation is the type written (#487)
- borrowck: report a capture's borrow at the first use of the place (#486)
- remote.py a remote that lies changes every pack A sent (.pack.zst too) (#489)
- testselect: count "that test taken out of the sandbox" again (#495)
- debuginfo.py counts a value gdb cannot print as optimized out as optimized out, not wrong (#493)
- Fn-trait vtable slots of written impls take the arguments one by one (self-test segfaults) (#492)
- wfcheck: a signature's and impl header's trait bounds are well-formed (E0310, E0477) (#497)
- std's foldhash, rand and nested define_opaque no longer refuse (No-rustc E, #85) (#494)
- borrowck: higher-ranked subtyping of fn pointer types (expect-fn-supply-fn) (#499)
- borrowck: a slice's bounds check reads its length where it is indexed (#501)
- a link that fails on a name the standard library's unit refused (an extern call to an exported function nothing in the library calls) is an error naming the function and why (#503)
- borrowck: a function an asm! names by sym has its where clauses proven (E0309) (#498)
- borrowck: normalize the return slot's type with regions of its own (#500)
- CI's ra-jit.tar carries blaze's proc-macro server, so a load links its proc macros (#491)
- the whole build gives integration tests CARGO_TARGET_TMPDIR, as cargo does (#504)
- hir-ty-codegen: count each constant once in the unit read-back test (#506)
- Windows tests read paths as cargo is given them; ra-jit finds a quoted response file and a crate asked by a /-path on Windows (#507)
- borrowck: check builtin derives as rustc expands them, packed structs' fields copied out (#505)
- sandbox: say how on when a container's seccomp refuses the user namespace (#420) (#510)
- A build that would write a cdylib or staticlib stops with an error, not a silent Finished (#262) (#511)
- No-rustc E: SVE/SVE2 scalable vectors, their layout and a first family of intrinsics compile on aarch64 (#85) (#514)
- borrowck timings mode; an edit re-checks only the bodies it changed (#512)
- f16, f16b and f128 compile on AArch64 Linux (#502)
- a workspace load asks cargo nothing of the toolchain or the configuration (#485)
- borrowck: a temporary may be unsized, as rustc's as_temp makes one; a place statement moves into it (#515)
- borrowck: closure-to-fn-pointer casts and written projections under for<'a> (issue-102800) (#509)
- blaze's proc-macro server builds on macOS; CI's ra-jit.tar and the macOS release ship it (#508)
- a red train opens its issue again: train_issue.py reads job logs with gh api --allow-escape-sequences (#517)
- 128-bit atomics on AArch64 Linux compile (No-rustc E, #85) (#496)
- borrowck: a returned impl Trait's hidden type is well-formed at the signature in its defining body (#519)
- blaze-gateway v0, the Worker in front of R2, deployed (#520)
- cargo_ra_jit.py's warm-up check asks whether the warm-up linked build scripts or proc macros again (#522)
- Windows tests expect config and dependency paths joined part by part (#523)
- mir eval reads a slice's length as the target's usize (wasm32 consts) (#524)
- blaze-units makes cargo's unit graph with no I/O; plan.rs runs no cargo build --unit-graph (#516)
- the gateway's blob store (/has /get /blob /put), D1 locations, private HMAC namespaces (#526)
- a toolchain without rust-src is turned down saying so again (rustsrc.py, core-2) (#528)
- A stateful static whose first value changed, which ours can't replace, stops the build instead of keeping rustc's old value (#392) (#527)
- nix-live runs beside the train, not in its run: a cold nix build no longer holds main's next train (#532)
- Daily toolchain bump to the latest nightly, gated by the train (#404) (#529)
- Keep a load's two proc-macro modes apart, take the mode from the build, and name the way through (#460) (#530)
- ra-jit reads and writes the cache gateway (Place::Gateway), RA_JIT_REMOTE_REQUIRED, e2e numbers (#525)
- wasm objects: a segment is aligned as the code generator asked, not by its size (#534)
- No stale program after an edit of the global allocator or a daemon restart (#386) (#536)
- No-rustc E: SVE and SVE2 intrinsics but gathers and scatters compile on aarch64 (#85) (#537)
- the daemon's own link passes -C link-arg and -C link-args, as rustc's does (#488)
- toolchain-bump: verdict runs main's scripts; --checks corpus invocation (#540)
- No-rustc E: unsized values taken by value compile (#85) (#539)
- hir-def: expand a std prelude macro at item level as it is collected (fixes #327) (#538)
- No-rustc E: SVE gathers, scatters, gather prefetches and svrev compile on aarch64 (#85) (#554)
- what each build script of the bench corpus reaches in the sandbox (590 scripts, 17 would be denied) (#556)
- the sandbox record test is Unix-only again (lost in #510's merge) (#565)
- splices.py holds Windows to 57 splices, the count since thread-locals carry their own copy (#562)
- borrowck: a closure parameter's &dyn Trait of a 'static trait defaults to 'static (#558)
- buildmodel crate_staticlib links synchronization.lib on MSVC (#564)
- borrowck: lifetime elision counts only lifetimes the written signature has (#559)
- docs: wasm32 measured at the pinned nightly (#555)
- borrowck: offset_of! takes no reference (no false E0793 on packed structs) (#553)
- CI's blaze-remote action: pull blaze's store from the gateway, push from main (slice 1 wiring) (#552)
- A crate-local writable static (OnceLock, Mutex, atomic, static mut) reached from main builds in a --release whole build (#551)
- The build manifest (#41): an unchanged build on a fresh machine writes the program from the store and links (#550)
- toolchain: remove the nightly an older blaze downloaded once the new one is in (#548)
- a whole build's marker of a program of tests is not the program (fd's `tests` was truncated by every cargo ra-jit test) (#568)
- borrowck: a body's trait object under an inferred lifetime has its own lifetime (#547)
- borrowck: a fn item's elided lifetime is late-bound when it is reified to a for<'a> fn (train: fork alone) (#546)
- borrowck: interval sets, SCC region inference, block-wise loan scopes (speed, no rule changes) (#545)
- the half-precision NEON intrinsics compile on AArch64 (#544)
- Link a library's cdylib as a shared library at cargo's path (#262) (#513)
- borrowck: Self written in a trait object's bounds or a fn pointer is the impl's self type (#557)
- buildmodel finds the whole workspace's link line on Windows (#563)
- borrowck --project refuses a workspace cargo metadata fails on; apps sweep fetches bun's vendored crates (#570)
- stringify! spells tokens that came out of a proc macro as rustc does (#560)
- no merge queue: pull requests merge directly once ci passes, not required to be up to date (#566)
- a state another ra-jit wrote is a cache miss; cargo blaze has no cargo-fallback path left (#571)
- locals whose storage never overlaps share their stack slots (#561)
- blaze disable removes the bin directory of cargo's home that enable made (#572)
- a whole build's tests are where cargo puts them, in its build directory and its second layout (#569)
- toolchain-bump: compare the bump's train with the train of the main commit it is on (#578)
- wasm32: functions that take ..., their list the address of what the caller wrote (#573)
- train: a wasm32 job (harness/wasm32.py under the flake's wasm shell) (#535)
- cargo_ra_jit.py's checks of the daemon's life follow the one-path whole build (#575)
- borrowck: impl Trait captures in traits, hidden-type WF, OpaqueType blame, async closure captures (#490)
- debuginfo.py counts a value gdb prints in part, the rest optimized out, as optimized out (#577)
- borrowck speed: timings measure an edit; wfcheck's inferred outlives is a query; is_copy once per type (#576)
- a whole build fails on an error as rustc and cargo write it, also one in code nothing reaches written while no daemon ran (#574)
- borrowck: promote by the qualifs of the value, as rustc's check_consts::qualifs (#579)
- A patch's own statics take names of their own: no TLS DataDeclaration::merge panic after a regen edit (#291) (#581)
- docs agree on Windows x86-64 MSVC: builds and live edits go through blaze (#462) (#587)
- No-rustc E: the AVX-512 intrinsics and asm kreg operands compile on x86_64 (#85) (#584)
- nix/units: regenerated, the flake evaluates again (#531)
- borrowck --project: a test of the refusal of failed metadata; bun's vendored crates get their patches (#583)
- store: a build that compiles std's crates refuses entries that call into the std unit (#328) (#533)
- link proc_macro into a build script or program whose crates name it (bat) (#588)
- No-rustc G: blaze builds and links wasm32 programs (#567)
- Doctests: keep the line after an example's crate attributes, as rustdoc does (#249) (#589)
- build scripts and proc macros, built and run, are store entries keyed by what they read (interim: the Nix-model closure key replaces the run read set next) (#585)
- wasm32: v128 and the simd128 / relaxed-simd operations lowered to wasm SIMD, 128-bit integer operations, vectors in the wasm C ABI (#590)
- doc tests run again after an edit to the library, and -q shows them tersely (#594)
- Codegen: fma keeps the NaN rustc keeps on AArch64 (#586)
- MIR: a call that writes only the const generics of a #[rustc_legacy_const_generics] function as arguments passes them as generics, as memory_size(0) (#591)
- cargo's home is an overlay in the test sandbox, so a test that runs cargo passes (#602)
- a sandboxed build script's directives are checked before cargo reads them (link-search, link-arg, rustc-env rules; 0 of 587 corpus runs refused) (#603)
- integer constants are immediates, not loads of data (#606)
- a cdylib exports the C-level symbols of every crate it links, as rustc's (#605)
- inline(always) library callees are inlined in wasm objects (#607)
- wasm objects carry rustc's custom sections: link_section statics and target_features (#604)
- a build script run for host and target is read as the target's run (#609)
- wasm32: writable data of only zeros is .bss, which the linker leaves out of the module, as LLVM makes it (#610)
- wasm C ABI: an aggregate of one scalar goes as that scalar (#608)
- nix/units merge when two PRs change Cargo.lock: units keyed by name, lock and manifest mirrored (#353) (#611)
- a sandbox can see a view in place of the machine: its binds alone on a root of its own (Linux: tmpfs root, pivot_root, own /proc /dev /etc /tmp), for build scripts under the Nix model (#612)
- borrowck: a _ inside an associated type in a user type annotation is solved by equating with the inferred type (#518)
- No-rustc E: svdupq_n_* compile on aarch64, the last of the SVE class (#85) (#613)
- No-rustc E: f16 and f128 compile on x86-64 Linux (#85) (#614)
- toolchain-bump: run the bump train's failed jobs again once before red blocks it (#615)
- borrowck: promote the autoref'd mutable borrow of an empty array (&mut [][..]) (#580)
- each whole-build worker links in a directory of its own, so features and flags don't leak into the next build (#593)
- borrowck: wfcheck normalizes fn signatures in an inference context, as rustc's WfCheckingCtxt (#618)
- growth.py measures both sizes under one register allocator (#619)
- closure: nixpkgs store paths for build scripts, fetched and verified by blaze itself (no nix binary), kept as lasting store entries, laid out per path (#617)
- remote.py reads push/pull's outcome after #525's traffic line (#616)
- corpus's packed struct of offset_of! is PackedHeader, not compared with std's Windows allocator Header (#621)
- std unit: a #![no_std] program links core and compiler_builtins, not std, as rustc puts them in (#622)
- remote-cache gateway: signed index, refresh inside head writes, 80-day expiry, lifecycle rules (slice 4 server side) (#620)
- ra-jit assembles intrinsic stubs in parallel: std unit cold compile 15.4 s -> 3.0 s (#85) (#624)
- Programs print blaze, not ra-jit (#270, what the programs print) (#626)
- a daemon that starts from a manifest takes the store's code while nobody asks, so its first edit is a patch (storetracked, #550) (#627)
- LLVM_SYS_231_PREFIX is given to build scripts, so blaze builds itself in nix develop again (#630)
- hir-ty: never-type fallback to ! in every edition, if-let captures as let, &mut borrow makes a place maybe-init (#73) (#623)
- docs: wasm32 measured at main: nothing of rustc's for wasip1 programs, wasm-host guests and Cloudflare Workers (#625)
- build-script downloads pinned by sha256 in Cargo.fetches.lock (blaze lock --fetches) and handed to the script through its crate's own variable, with no network (#637)
- No-rustc E: x86-64 SSE to AVX2, crypto and machine-state intrinsics and __rust_probestack compile (#85) (#640)
- build scripts' builds make the standard library once a store (#203) (#642)
- nocompact.py finds answer by the name the daemon's own link gives it (#638)
- tier events show own functions by their path; tiers.py reads next_age's demotion (#639)
- buildmodel and storeguards registries are local-registries, which blaze reads (no git index) (#641)
- a patch compiles the changed code an unchanged copy of the base reaches again (#582) (#628)
- the fake gateway of the remote tests hashes each body once, so the parts test keeps within the client's deadline in blaze's own build (#649)
- No-rustc E: f16 intrinsics, AVX512-FP16, AVX512-BF16, AVX-NE-CONVERT and f16b compile on x86-64 (#85) (#644)
- borrowck: wfcheck needs what normalizing a signature needs; no projection from impls with E0207 params (#646)
- the sandbox view tests run outside cargo blaze test's sandbox; a sandbox nested in another says why it can't be made (#650)
- test passes kept in the store and shared through the remote, keyed like a build; tests hermetic by default (#647)
- harness: check_apps.py, cargo blaze check against cargo check on the bench apps (#653)
- breakers.py's procmacro and overlap cases pass (stringify! keeps a proc macro's re-spanned parentheses; LLVM shares the frame slots of locals that never overlap) (#648)
- toolchain-bump: count std refusals against toolchain/base's build, not main's newest (#657)
- no cap on the tokens one macro expands to, as rustc has none (#423: biome_wasm's #[wasm_bindgen] block) (#654)
- borrowck speed: a trait reference is proven by a canonical query (rustc's type_op_prove_predicate) (#651)
- a dependency named only in another crate's macro output is loaded, so its impls are seen (#423: biome_analyze's derived Deserializable) (#643)
- the first load's check reads only modules of a crate, not rustfmt.toml (macOS fd mended 101) (#660)
- a codegen refusal refuses that build, not the daemon (#656)
- CHANGELOG has no stray conflict marker (#665)
- Codegen: fma keeps the NaN rustc keeps on x86-64 (#85) (#661)
- a SIMD vector is copied, and 16 bytes of lanes stored, whole: sha2's SHA-NI rounds 3 times faster (#662)
- bench/remote-cache: blaze's own store through the gateway, and the 16 GB limit on a cold self-build (#664)
- x86-64 inline asm drops a template's $ in Intel syntax, as rustc's LLVM does (Windows doctests build) (#663)
- blaze builds and runs a load's build scripts and proc macros; a cargo blaze build or test runs no cargo but the typed one (#658)
- coldbuild.py's uncompilable function is a `do yeet`, refused on every target (#666)
- a named local keeps its stack slot to itself through its scope, for the debugger (#667)
- hir-ty: E0525 when a closure's kind does not allow the Fn* trait it must implement (#670)
- docs: cargo blaze builds the wasm32 workers and the no_std simd guest (#672)
- hir-def: a name bound twice in one pattern is a binding of its own, as rustc's fresh_binding (#671)
- a build script is given the variables it declares with rerun-if-env-changed, keyed by their values (#585) (#673)
- allrajit_wasm.py reads the link line as blaze prints it (#270) (#674)
- allrajit_wasm.py: the link line says blaze since #626 (#675)
- the first load's check reads the crates the build compiles when the program came from the store (macOS fd mended 101) (#676)
- allrajit_wasm.py: the cdylib and the no_std guest by cargo blaze build too (#677)
- closure lay-out renames before sealing (macOS), borrowck test path separator (Windows) (#678)
- test passes on a fresh runner, measured on 7 bench apps (verify clean; reach walk is the cost left) (#679)
- docs: resolve.py ctdeps results on main after #658 (#682)
- a build from the store describes what a cold build would (#242) (#681)
- hir-ty: a failed match guard ends its pattern's bindings, so E0384 needs no exception for bindings (#680)
- the store reads a pack through a buffer to check it, holding its index, not the pack (#685)
- start a worker with the places cargo set, so its load compiles env!("CARGO_BIN_EXE_<name>") (#687)
- tests' walk by stable names: a fresh daemon with main's store compiles no dependency to hash the tests (axum walk 6.2 s -> 0.94 s) (#688)
- a constant's raw pointer keeps what the pointers it points at point to (io::Error of a message no longer segfaults) (#689)
- No-rustc E: x86-64 FMA wrappers compiled as LLVM folds them at -O0, 90 refusals fewer (#85) (#690)
- toolchain-bump: retry GitHub's 502s, say so when the verdict cannot decide, -f train=<id> to decide again (#691)
- borrowck speed: is_copy and normalizing are canonical queries, dominators by semi-NCA (#692)
- wasm: lower a memory routine reached through try_call as call does (derive_more_impl builds as wasm) (#695)
- Codegen: NEON fused multiply-adds keep the NaN rustc keeps in NEON f64 lane fma (#85) (#694)
- hir-def: an expansion of one expression and a `;`, called where rustc parses an expression, is that expression (#693)
- toolchain: pin nightly-2026-10-27 (#543)
- toolchain-bump: a green bump merges on its own and its merge starts main's workflows (#699)
- ra-jit --run-tool, blaze's std::process for wasm32-wasip1 build scripts (#698)
- build scripts under WASI, an opt-in runner (RA_JIT_BUILD_SCRIPTS=wasi) (#700)
- docs/toolchain.md: measured lag and bump time; the bump keeps the docs' pin current (#702)
- build scripts under WASI over the bench corpus (473 of 624 unchanged) (#701)
- git worktrees share a whole build's worker again; a worktree's build is linked before it answers (#645)
- a whole build's integration test gets CARGO_BIN_EXE_<name> at run time, as cargo gives it (#659)
- give plain cargo its own copy of the daemon's executable (no Text file busy) (#703)
- borrowck: a reified fn item's elided `&dyn Trait` is `&'a (dyn Trait + 'a)`, rustc's object lifetime default (#704)
- testselect expects the provenance ra-jit keeps (commit and run under GitHub Actions) (#705)
- DWARF units are written as each is full (nushell cold build 19.0 -> 17.5 GB peak, #38 part 1) (#707)
- cargo_ra_jit.py's other build of ra-jit has the proc-macro server beside it (daemon replace rows) (#708)
- a load's unit graphs are made once in that load, so a reload after a manifest edit sees its features (buildmodel reload_feature_toggled) (#709)
- a daemon's worker has its build's RUSTFLAGS and profiles, not the daemon starter's (#710)
- Copy, not move, a Copy value a closure captures by value (#711)
- own answers: rustc --print cfg with -C target-feature and -C target-cpu (#706)
- doctests are named by the line of their fence where rustdoc finds it (nightly-2026-10-27) (#712)
- breakers.py counts neon_fma_lanes' splatted function as left to rustc on purpose (#713)
- docs/borrowck.md: counts at 68be5dec, every app clean, what is left (#715)
- the store keeps a program's files without copying them (nushell manifest 17.4 -> 14.8 GB) (#716)
- borrowck speed: drop liveness as rustc's dropck finds it (SCCs, adt_dtorck_constraint query, drop glue query) (#718)
- borrowck speed: a goal whose answer names a placeholder is proven by the query too, as rustc's canonical response (#727)
- Proc macros run as WebAssembly by default (watt-style, Wasmtime in the daemon; warm corpus 4.01 s native -> 2.61 s) (#726)
- a whole build's object is not held twice while it is written and weakened (#729)
- Windows: the whole build builds and live-edits demo/life (#697)
- check: leave dev targets out of a plain check (#684)
- cargo: retry failed workspace loads on the next build (#652)
- remote.py's lying-remote check allows B's own pack to be one of A's to the byte (#733)
- docs no longer describe the merge queue as current (#736)
- install.md describes the dated releases, not a moving nightly build (#737)
- build scripts run in their closure and are kept by what they are made of (opt-in): 425/434 corpus runs, 371/371 cross-host hits (#714)
- docs no longer say cargo blaze hands builds to cargo (#739)
- Name build-script files that differ on rerun (#725)
- train: nushell's projects jobs do not block the verdict until #734 (16 GB runner OOM) (#735)
- macOS sandbox makes the directories above a /tmp overlay (nested cargo dep-info ENOENT) (#738)
- One SourceId: blaze-source holds it, blaze-registry drops its own (#321) (#740)
- harness: cargo_ra_jit.py keeps a failed run's work directory, so CI uploads its workers' logs (#741)
- roots: list a crate's roots with the crate, not on every edit (#696)
- blaze-resolve's Registry over the index, fetched concurrently (#321) (#742)
- borrowck speed: the walk order of drop live locals is found once per local (#743)
- clippy's warnings and denied lints outside the hot paths, no behaviour change (#744)
- one hex helper and one package_dir in ra-jit instead of their copies, no behaviour change (#745)
- chunks of a constant size taken with as_chunks where clippy asks, outside the hot paths, no behaviour change (#746)
- nightly's #[derive(From)] expands as rustc's does (#600) (#747)
- proc-macro server: one missing-server message, which says macOS releases ship it too (#460) (#749)
- macOS: a first link has the header room its unwind information wants (#173) (#748)
- a sandboxed build script may forward CARGO_CFG_* and CARGO_ENCODED_RUSTFLAGS to its crate, as cargo accepts (#719) (#753)
- sandboxed tests are an experiment a workspace opts into; by default tests run as cargo test runs them (#754)
- resolver.feature-unification = "workspace" builds -p app with the features other members enable, as cargo does (#596) (#752)
- cargo: crates blaze builds read the variables their build scripts set (#467)
- Fork sync runs in the forks, with their own token: blaze needs no secret for it (#755)
- corpus: every const-generic NEON intrinsic at each end of its range (#756)
- farsplices: an edit of an 8-byte AArch64 function past a branch's reach is spliced (#300) (#757)
- answers: a rustc outside a toolchain is rustup's proxy, whatever it is called (#466)
- auth-required registries' index and .crate files fetched with the token cargo sends, from one credential lookup (#315) (#758)
- keep the names read of an executable across a splice (#453) (#750)
- train: by hand only for now; releases are cut by hand (#765)
- opening the store reads each pack's index, not its values, and holds 16 bytes a key (#686) (#759)
- blaze toolchain says nothing of a project's toolchain; the first build says it once (#762)
- say Compiling only of what is built, and a new-user experience harness (#760)
- a workspace that fails to load says why, not only its outermost error (#764)
- blaze's own toolchain has wasm32-wasip1's std, so proc macros build without rustup (#763)
- docs/cargo.md names cargo blaze and the BLAZE_* settings (#270) (#770)
- macOS sandbox stand-ins are named as what they stand for, so realpath is right in an overlay (#767)
- the messages blaze prints name blaze and the BLAZE_* settings (#270), no behaviour change (#768)
- the daemon requests only the stand-in first build sent are gone, no behaviour change (#772)
- the runtime crates read BLAZE_* settings too, and the user docs name them (#270) (#769)
- experience.py finds blaze's processes by the program they run (macOS pgrep -a is ancestors) (#774)
- a worker writes again what a clean took from under it (#721) (#761)
- statics that name one section share it, so macOS programs with >255 #[link_section] statics build (#724) (#766)
- AArch64 16-byte vector copies only at frame offsets an immediate reaches (valgrind SIGILL in difftest corpus) (#777)
- credential processes and cargo:token-from-stdout asked as cargo asks them; keychain providers refused by name (#315) (#778)
- a proc-macro server that failed an expansion fails the build with what it said (#521)
- release cut: the train's release job by hand, while the train runs by hand only (#782)

## [0.1.0] - 2026-10-20

The first release.

### Added

- `ra-jit`, a compiler daemon that keeps a workspace loaded in rust-analyzer and generates
  code from its MIR with Cranelift. After the first build, an edit rebuilds only the
  functions that changed.
- `cargo ra-jit build|run|test|check`, with cargo's own flags. Anything the daemon cannot
  build falls back to cargo, and says so.
- `ra-jit enable` and `ra-jit disable`: route plain `cargo build|run|test|check` through
  ra-jit, and put cargo's config back.
- `ra-jit-live` (on crates.io), a crate that applies edits to a running program, keeping
  its state (macOS on Apple silicon).
- A nix flake: `nix run github:indexable-inc/ra-jit` opens a Game of Life you can edit
  while it runs; `#ra-jit` is the cargo wrapper.
- Prebuilt binaries for Linux (x86_64, aarch64) and macOS (aarch64).
- `cargo install ra-jit` from crates.io (Cranelift; `--features llvm` needs LLVM 20). The
  rust-analyzer and rustc_abi forks it builds on are published as `ra-jit-<name>`.

### Known limits

- Rust 1.99.0 only.
- No borrow checker: keep running `cargo check`.
- Changing a struct the running program uses asks for a restart.

[Unreleased]: https://github.com/indexable-inc/blaze/compare/v0.20261028.9...HEAD
[0.20261028.9]: https://github.com/indexable-inc/blaze/releases/tag/v0.20261028.9
[0.20261028.3]: https://github.com/indexable-inc/blaze/releases/tag/v0.20261028.3
[0.20261028.2]: https://github.com/indexable-inc/blaze/releases/tag/v0.20261028.2
[0.20261028.1]: https://github.com/indexable-inc/blaze/releases/tag/v0.20261028.1
[0.20261028.0]: https://github.com/indexable-inc/blaze/releases/tag/v0.20261028.0
[0.20261027.0]: https://github.com/indexable-inc/blaze/releases/tag/v0.20261027.0
[0.1.0]: https://github.com/indexable-inc/ra-jit/releases/tag/v0.1.0
