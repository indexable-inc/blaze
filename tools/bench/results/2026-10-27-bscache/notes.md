# What the corpus's build scripts would be denied (2026-10-27, lane bscache)

`README.md` and `denied.json` are `harness/bscache.py report` over the records `record` collected
on one arm64 ix VM (16 cpus, 16 GB), blaze at 4e3510c84d, with every script run by
`blaze-engine build-script` in the sandbox of main (machine read-only, OUT_DIR writable, no network,
reads recorded). The run per project is `cargo check -Zunstable-options --compile-time-deps
--workspace --all-targets` through `blaze-rustc`, in `nix develop .#bench` (`.#bench-zed` with
nixpkgs' lld on PATH for zed, whose `.cargo/config.toml` links aarch64 with `-fuse-ld=lld`).

590 scripts ran; 573 reached nothing a hermetic sandbox would deny. No script reached the
network (no `N` line): the two that would fetch failed before they did.

## The 17 that would be denied, by what they do

- **git for a version string** (9): ripgrep, cargo, helix-loader, helix-term, zed, cli,
  remote_server, cranelift-codegen, nu-cmd-lang run `git` (rev-parse, describe), reading the
  workspace's `.git` and `/etc/gitconfig`. Each works without git (it prints no hash). A
  hermetic run gives them no git; if a hash is wanted, `.git/HEAD` and the ref it names are the
  input, not git's whole reach.
- **git submodule checks of vendored C** (4): curl-sys and libssh2-sys (two versions each) run
  `git submodule` through sh, sed and gettext to see if their vendored sources are checked out.
  Outside a git checkout of theirs (a registry package) it is a no-op.
- **configure and make** (1): tikv-jemalloc-sys runs its own `configure` with `/bin/sh`,
  `make`, `awk`, `diff`, `cmp`, `uname`, `arch`: a C build's tools beside the C compiler, which
  the allowed set should name (stdenv's `coreutils`, `gnumake`, `gawk`, `diffutils`, `gnused`,
  `gnugrep`).
- **cmake** (1): wasmtime-c-api-impl runs `cmake` (and reads its modules).
- **cargo itself** (1): nu-cmd-lang (shadow-rs) runs cargo, which reads the whole registry and
  `.git`, and tries to write `CARGO_HOME`'s locks and the target's `.rustc_info.json` (refused;
  the script carries on).
- **CA certificates** (1): octocrab reads `/etc/pki/tls/certs/ca-bundle.crt`.
- **other packages' sources** (1): uucore reads `locales/en-US.ftl` of uu_cp, uu_mkdir, ... in
  the registry: inputs of another package, not declared.

## The 3 that failed

- **helix-term** writes the tree-sitter grammars it fetches (with git, over the network) into
  `runtime/grammars/sources` of the workspace. Helix's own off switch is
  `HELIX_DISABLE_AUTO_GRAMMAR_BUILD`; a locked fetch would hand it the grammars instead.
- **biome_yaml_analyze** writes generated code into its own `src/` and fails with EROFS. The
  record has no `W` line for it: the sandbox refuses it by the read-only mount, before the
  recorder sees it (a gap of the recorder, lane testcache's sandbox).
- **webrtc-sys** (zed) did not start: its executable needs `libzstd.so.1`, which it was linked
  against with no run path and which `.#bench-zed` does not put on `LD_LIBRARY_PATH` (outside
  the sandbox too). Once it runs it downloads a prebuilt libwebrtc: a locked fetch, handed over
  by its own override `LK_CUSTOM_WEBRTC`.

## Not measured

- **bun**: its Rust needs `harness/bunprep.py`'s native build (WebKit, BoringSSL, ...) first;
  without it cargo stops at the missing `vendor/lolhtml`.
- **executables are a guess**: the record does not tell an exec from a stat, so an executable is
  an `R` line of a regular file with the executable bit and an ELF or `#!` start. Files git only
  looked at in a workspace are not counted (they are reads of the workspace).
