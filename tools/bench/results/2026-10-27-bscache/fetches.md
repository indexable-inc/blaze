# What build scripts of the bench corpus download, and how blaze hands it to them

A build script runs with no network. One that downloads a prebuilt library is given it instead,
pinned in `Cargo.fetches.lock` by `blaze lock --fetches` and handed through the crate's own
variable for a local copy (`crates/blaze-engine/src/cargo/fetches.rs`, `OVERRIDES`).

Found by `/root/scan.py` over every build script of the corpus's `Cargo.lock`s (after
`cargo fetch`; bevy has no `Cargo.lock` upstream and bun's `cargo fetch` fails on its missing
`vendor/lolhtml`, so what resolved): a script is a candidate when its source names an HTTP
client, `curl`/`wget`/`git`/`powershell`, or an `https://` address, then each was read.

## Scripts that download

| crate | project | target | what | given by | override |
|---|---|---|---|---|---|
| `webrtc-sys` 0.3.23 (zed's livekit fork) | zed | all | `webrtc-{os}-{arch}-release.zip` of `WEBRTC_TAG` (148 MB on aarch64 Linux) | `LK_CUSTOM_WEBRTC` (a directory with `include`, `lib`) | yes: `Fetch`, zip unpacked |
| `helix-term` 25.7.1 | helix | all | ~200 tree-sitter grammars by `git`, compiled | `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1` skips it (as nixpkgs' helix) | yes: `Value` |
| `zed` 1.24.4 (its own `build.rs`) | zed | windows | ConPTY nupkg by `powershell` | none | no: windows only, a patch to read `ZED_CONPTY_NUPKG` |
| `mach-dxcompiler-rs` 0.1.7 | bevy (wgpu-hal `static-dxc`) | x86_64-pc-windows-msvc | dxcompiler static library by `curl`, after asking GitHub's API the release is immutable | none | no: windows-msvc only, a patch to read a local archive (`MACH_DXCOMPILER_ARCHIVE`) and skip the API check |

Not in the corpus, but in the table (read at the versions named): `v8` 152.2.0
(`RUSTY_V8_ARCHIVE`, the `.a.gz`/`.lib.gz` of the release) and `ort-sys` 2.0.0-rc.15
(`ORT_CACHE_DIR`, the archive of its `build/download/dist.tsv` unpacked into
`dfbin/{target}/{sha256}`; `cuda` is refused, its download is picked by the CUDA on the machine).

Candidates read and not downloading: `aws-lc-sys`, `openssl-sys`, `pq-sys`, `curl-sys`,
`libssh2-sys`, `rustversion`, `portable-atomic`, `rust_decimal`, `selectors`, `markup5ever`,
`web_atoms`, `cranelift-codegen`, `rav1e`, `av-scenechange`, `cap-*`, `io-extras` (addresses in
messages and comments); `libgit2-sys`, `wasm-bindgen-shared`, `cargo`, `ripgrep`, `nu-cmd-lang`,
`helix-loader`, `cli`, `remote_server` (`git` for a commit hash, reading the checkout);
`biome_aria_metadata`, `biome_wasm` (addresses in generated data). `prost-build`-style `protoc`
use is the `PATH`'s `protoc`, which the sandbox shows; it downloads nothing.

## Patches proposed where a crate reads no local copy

Both only download on Windows, which blaze's sandbox does not run yet (#257), so neither blocks
a build today. Once each reads a variable, its row in `OVERRIDES` is a `Give::Fetch` as
`v8`'s is.

`mach-dxcompiler-rs` 0.1.7 `build.rs` `main`: read the archive from a variable, keeping the
checksum it already checks, and ask GitHub's API nothing then:

```rust
let file_path = match env::var_os("MACH_DXCOMPILER_ARCHIVE") {
    Some(local) => PathBuf::from(local),
    None => {
        verify_release_is_immutable();
        let file_path = out_dir.join("machdxcompiler.tar.gz");
        download_released_library(&artifact.url, &file_path);
        file_path
    }
};
println!("cargo:rerun-if-env-changed=MACH_DXCOMPILER_ARCHIVE");
verify_checksum(&file_path, artifact.sha256);
```

zed `crates/zed/build.rs` (windows): take the nupkg from `ZED_CONPTY_NUPKG` when set, in place
of the `Invoke-WebRequest`, and extract it as now:

```rust
let nupkg_path = match std::env::var_os("ZED_CONPTY_NUPKG") {
    Some(local) => std::path::PathBuf::from(local),
    None => { /* the Invoke-WebRequest download, as now */ out_dir.join("conpty.nupkg.zip") }
};
println!("cargo:rerun-if-env-changed=ZED_CONPTY_NUPKG");
```

## Measured (VM, aarch64 Linux, 16 cpus)

| what | time |
|---|---|
| `blaze lock --fetches` in zed, nothing in the store (148 MB download) | 7.6 s (10.7 s with blaze's one-time toolchain download) |
| `blaze lock --fetches` in zed again, served by the store | 1.0 s (the resolve) |
| `blaze lock --fetches` in zed for aarch64-apple-darwin, aarch64-unknown-linux-gnu and x86_64-pc-windows-msvc (474 MB downloaded, 2.3 GB peak memory) | 24.1 s |
| the same again, served by the store | 2.6 s |
| `blaze lock --fetches` in helix (a `Value` override only: no file is written) | 6 ms |
