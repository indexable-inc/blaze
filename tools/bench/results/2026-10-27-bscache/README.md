# Build scripts of the bench corpus under the sandbox's record

Made by `harness/bscache.py record` then `report` (see its docstring for how each line is
classed). One row per project; a script counts once per class it has a line in.

| project | cargo | scripts | failed | no record | clean | network | writes | executables | reads |
|---|---|---|---|---|---|---|---|---|---|
| axum | 0 | 55 | 0 | 0 | 55 | 0 | 0 | 0 | 0 |
| bat | 0 | 28 | 0 | 0 | 28 | 0 | 0 | 0 | 0 |
| bevy | 0 | 72 | 0 | 0 | 72 | 0 | 0 | 0 | 0 |
| biome | 101 | 25 | 1 | 0 | 25 | 0 | 0 | 0 | 0 |
| bun | 101 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| cargo | 0 | 41 | 0 | 0 | 38 | 0 | 0 | 3 | 3 |
| fd | 0 | 9 | 0 | 0 | 8 | 0 | 0 | 1 | 1 |
| helix | 101 | 29 | 1 | 0 | 27 | 0 | 1 | 2 | 2 |
| hyperfine | 0 | 20 | 0 | 0 | 20 | 0 | 0 | 0 | 0 |
| just | 0 | 18 | 0 | 0 | 18 | 0 | 0 | 0 | 0 |
| nushell | 0 | 90 | 0 | 0 | 86 | 0 | 1 | 3 | 4 |
| ripgrep | 0 | 10 | 0 | 0 | 9 | 0 | 0 | 1 | 1 |
| zed | 101 | 193 | 1 | 0 | 187 | 0 | 0 | 5 | 6 |
| total |  | 590 | 3 | 0 | 573 | 0 | 2 | 15 | 17 |

## Scripts that failed

### biome: `biome_yaml_analyze v0.1.0 (/root/bs/work/src/biome/crates/biome_yaml_analyze)`

```
error: failed to run custom build command for `biome_yaml_analyze v0.1.0 (/root/bs/work/src/biome/crates/biome_yaml_analyze)`
Error: Os { code: 30, kind: ReadOnlyFilesystem, message: "Read-only file system" }
```

### helix: `helix-term v25.7.1 (/root/bs/work/src/helix/helix-term)`

```
thread 'main' (2) panicked at helix-term/build.rs:5:26:
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
note: the build script of `helix-term` failed in the sandbox, which refused it writing `/root/bs/work/src/helix/runtime/grammars/sources` (the machine is read-only in a sandbox); if it needs that, run it outside: `unsandboxed-build-scripts = ["helix-term"]` under `[workspace.metadata.blaze.sandbox]` in /root/bs/work/src/helix/Cargo.toml (or `build-script = false` under `[package.metadata.blaze.sandbox]` in its own manifest, for a package of the workspace), or `BLAZE_SANDBOX=off` for every build script and test of the command
```

### zed: `webrtc-sys v0.3.23 (https://github.com/zed-industries/livekit-rust-sdks?rev=0a1c519cfce9b365229026b55de9b9dbdb6fed3c#0a1c519c)`

```
error: failed to run custom build command for `webrtc-sys v0.3.23 (https://github.com/zed-industries/livekit-rust-sdks?rev=0a1c519cfce9b365229026b55de9b9dbdb6fed3c#0a1c519c)`
```


## What each script would be refused

### `cargo 0.92.0` (cargo)

- executables: `/nix/store/*-git-2.56.0/bin/git`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`

### `cli 0.1.0` (zed)

- executables: `/nix/store/*-git-2.56.0/bin/git`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`
- reads: workspace outside the package: `$HOME/bs/work/src/zed/.git`, `$HOME/bs/work/src/zed/.git/HEAD`, `$HOME/bs/work/src/zed/.git/config`, `$HOME/bs/work/src/zed/.git/objects`, `$HOME/bs/work/src/zed/.git/refs`

### `cranelift-codegen 0.135.1` (zed)

- executables: `/nix/store/*-git-2.56.0/bin/git`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`

### `curl-sys 0.4.78+curl-8.11.0` (nushell)

- executables: `/nix/store/*-gettext-1.0/bin/envsubst`, `/nix/store/*-gettext-1.0/bin/gettext`, `/nix/store/*-gettext-1.0/bin/gettext.sh`, `/nix/store/*-git-2.56.0/bin/git`, `/nix/store/*-git-2.56.0/libexec/git-core/git`, `/nix/store/*-git-2.56.0/libexec/git-core/git-submodule`, `/nix/store/*-gnused-4.10/bin/sed`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/libexec/git-core/git-sh-i18n`, `/nix/store/*-git-2.56.0/libexec/git-core/git-sh-setup`, `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`

### `curl-sys 0.4.83+curl-8.15.0` (cargo)

- executables: `/nix/store/*-gettext-1.0/bin/envsubst`, `/nix/store/*-gettext-1.0/bin/gettext`, `/nix/store/*-gettext-1.0/bin/gettext.sh`, `/nix/store/*-git-2.56.0/bin/git`, `/nix/store/*-git-2.56.0/libexec/git-core/git`, `/nix/store/*-git-2.56.0/libexec/git-core/git-submodule`, `/nix/store/*-gnused-4.10/bin/sed`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/libexec/git-core/git-sh-i18n`, `/nix/store/*-git-2.56.0/libexec/git-core/git-sh-setup`, `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`

### `helix-loader 25.7.1` (helix)

- executables: `/nix/store/*-git-2.56.0/bin/git`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`
- reads: workspace outside the package: `$HOME/bs/work/src/helix/.git`, `$HOME/bs/work/src/helix/.git/HEAD`, `$HOME/bs/work/src/helix/.git/config`, `$HOME/bs/work/src/helix/.git/objects`, `$HOME/bs/work/src/helix/.git/refs`

### `helix-term 25.7.1` (helix)

- executables: `/nix/store/*-git-2.56.0/bin/git`
- reads: workspace outside the package: `$HOME/bs/work/src/helix/.git`
- writes: `$HOME/bs/work/src/helix/runtime/grammars/sources`

### `libssh2-sys 0.3.0` (nushell)

- executables: `/nix/store/*-gettext-1.0/bin/envsubst`, `/nix/store/*-gettext-1.0/bin/gettext`, `/nix/store/*-gettext-1.0/bin/gettext.sh`, `/nix/store/*-git-2.56.0/bin/git`, `/nix/store/*-git-2.56.0/libexec/git-core/git`, `/nix/store/*-git-2.56.0/libexec/git-core/git-submodule`, `/nix/store/*-gnused-4.10/bin/sed`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/libexec/git-core/git-sh-i18n`, `/nix/store/*-git-2.56.0/libexec/git-core/git-sh-setup`, `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`

### `libssh2-sys 0.3.1` (cargo)

- executables: `/nix/store/*-gettext-1.0/bin/envsubst`, `/nix/store/*-gettext-1.0/bin/gettext`, `/nix/store/*-gettext-1.0/bin/gettext.sh`, `/nix/store/*-git-2.56.0/bin/git`, `/nix/store/*-git-2.56.0/libexec/git-core/git`, `/nix/store/*-git-2.56.0/libexec/git-core/git-submodule`, `/nix/store/*-gnused-4.10/bin/sed`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/libexec/git-core/git-sh-i18n`, `/nix/store/*-git-2.56.0/libexec/git-core/git-sh-setup`, `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`

### `nu-cmd-lang 0.108.0` (nushell)

- executables: `/nix/store/*-gawk-5.4.1/bin/awk`, `/nix/store/*-git-2.56.0/bin/git`
- reads: /etc: `/etc/gitconfig`, `/etc/pki/tls`, `/etc/pki/tls/certs`, `/etc/pki/tls/certs/ca-bundle.crt`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`
- reads: cargo home (other packages): `$CARGO_HOME/.global-cache`, `$CARGO_HOME/.package-cache`, `$CARGO_HOME/registry`, `$CARGO_HOME/registry/CACHEDIR.TAG`, `$CARGO_HOME/registry/cache/index.crates.io-1949cf8c6b5b557f/adler2-2.0.0.crate`, `$CARGO_HOME/registry/cache/index.crates.io-1949cf8c6b5b557f/aho-corasick-1.1.3.crate`, `$CARGO_HOME/registry/cache/index.crates.io-1949cf8c6b5b557f/allocator-api2-0.2.21.crate`, `$CARGO_HOME/registry/cache/index.crates.io-1949cf8c6b5b557f/arrayvec-0.7.6.crate`, `$CARGO_HOME/registry/cache/index.crates.io-1949cf8c6b5b557f/autocfg-1.4.0.crate`, `$CARGO_HOME/registry/cache/index.crates.io-1949cf8c6b5b557f/bit-set-0.8.0.crate`, `$CARGO_HOME/registry/cache/index.crates.io-1949cf8c6b5b557f/bit-vec-0.8.0.crate`, `$CARGO_HOME/registry/cache/index.crates.io-1949cf8c6b5b557f/bitflags-2.6.0.crate` and 1409 more
- reads: target directory: `$HOME/bs/work/target/nushell/.rustc_info.json`
- reads: workspace outside the package: `$HOME/bs/work/src/nushell/.git`, `$HOME/bs/work/src/nushell/.git/FETCH_HEAD`, `$HOME/bs/work/src/nushell/.git/HEAD`, `$HOME/bs/work/src/nushell/.git/config`, `$HOME/bs/work/src/nushell/.git/index`, `$HOME/bs/work/src/nushell/.git/info/exclude`, `$HOME/bs/work/src/nushell/.git/logs/HEAD`, `$HOME/bs/work/src/nushell/.git/objects`, `$HOME/bs/work/src/nushell/.git/objects/pack`, `$HOME/bs/work/src/nushell/.git/objects/pack/pack-0046651caccca405d41e43692d9ab0565cceec21.idx`, `$HOME/bs/work/src/nushell/.git/objects/pack/pack-0046651caccca405d41e43692d9ab0565cceec21.pack`, `$HOME/bs/work/src/nushell/.git/refs` and 2447 more
- writes: `$CARGO_HOME/.global-cache`, `$CARGO_HOME/.package-cache`, `$CARGO_HOME/registry`, `$HOME/bs/work/target/nushell/.rustc_info.json`

### `octocrab 0.49.7` (zed)

- reads: /etc: `/etc/pki/tls`, `/etc/pki/tls/certs`, `/etc/pki/tls/certs/ca-bundle.crt`

### `remote_server 0.1.0` (zed)

- executables: `/nix/store/*-git-2.56.0/bin/git`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`
- reads: workspace outside the package: `$HOME/bs/work/src/zed/.git`, `$HOME/bs/work/src/zed/.git/HEAD`, `$HOME/bs/work/src/zed/.git/config`, `$HOME/bs/work/src/zed/.git/objects`, `$HOME/bs/work/src/zed/.git/refs`

### `ripgrep 15.1.0` (ripgrep)

- executables: `/nix/store/*-git-2.56.0/bin/git`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`

### `tikv-jemalloc-sys 0.6.0+5.3.0-1-ge13ca993e8ccb9ba9847cc330696e02839f328f7` (fd)

- executables: `/bin/arch`, `/bin/sh`, `/bin/uname`, `/nix/store/*-diffutils-3.12/bin/cmp`, `/nix/store/*-diffutils-3.12/bin/diff`, `/nix/store/*-gawk-5.4.1/bin/awk`, `/nix/store/*-gawk-5.4.1/bin/gawk`, `/nix/store/*-gnumake-4.4.1/bin/make`, `/nix/store/*-gnused-4.10/bin/sed`, `/usr/bin/arch`, `/usr/bin/uname`
- reads: Nix store outside the C compiler: `/nix/store/*-gnumake-4.4.1/include`

### `uucore 0.2.2` (nushell)

- reads: cargo home (other packages): `$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/uu_cp-0.2.2/locales/en-US.ftl`, `$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/uu_mkdir-0.2.2/locales/en-US.ftl`, `$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/uu_mktemp-0.2.2/locales/en-US.ftl`, `$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/uu_mv-0.2.2/locales/en-US.ftl`, `$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/uu_touch-0.2.2/locales/en-US.ftl`, `$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/uu_uname-0.2.2/locales/en-US.ftl`, `$CARGO_HOME/registry/src/index.crates.io-1949cf8c6b5b557f/uu_whoami-0.2.2/locales/en-US.ftl`

### `wasmtime-c-api-impl 48.0.1` (zed)

- executables: `/nix/store/*-cmake-4.4.4/bin/cmake`
- reads: Nix store outside the C compiler: `/nix/store/*-cmake-4.4.4/share/cmake-4.4/Modules/CMake.cmake`, `/nix/store/*-curl-8.22.0/lib`, `/nix/store/*-libarchive-3.8.9-lib/lib`, `/nix/store/*-libuv-1.53.0/lib`, `/nix/store/*-rhash-1.4.6/lib`, `/nix/store/*-xz-5.8.4/lib`

### `zed 1.24.4` (zed)

- executables: `/nix/store/*-git-2.56.0/bin/git`
- reads: /etc: `/etc/gitconfig`
- reads: Nix store outside the C compiler: `/nix/store/*-git-2.56.0/share/locale`, `/nix/store/*-zlib-ng-2.3.3/lib`
- reads: workspace outside the package: `$HOME/bs/work/src/zed/.git`, `$HOME/bs/work/src/zed/.git/HEAD`, `$HOME/bs/work/src/zed/.git/config`, `$HOME/bs/work/src/zed/.git/objects`, `$HOME/bs/work/src/zed/.git/refs`
