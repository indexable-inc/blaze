# What directive validation refuses over the bench corpus

Made by `harness/bscache.py directives` over 587 build-script runs (`run/stdout` of each).
Rules as `cargo/directives.rs` has them; `strict-*` is the rule as first written (Andrew,
12:10Z), before these counts set its edges.

| rule | lines | scripts |
|---|---|---|
| link-arg | 0 | 0 |
| link-search | 0 | 0 |
| rustc-env | 0 | 0 |
| strict-link-arg (no -pthread, no absolute rpath) | 3 | 2 |
| strict-link-search (system dirs not allowed) | 24 | 19 |
| strict-rustc-env (any CARGO_*/RUSTC*) | 3 | 1 |

## strict-link-arg (no -pthread, no absolute rpath)

- `tikv-jemalloc-sys 0.6.0+5.3.0-1-ge13ca993e8ccb9ba9847cc330696e02839f328f7` (fd): `cargo:rustc-link-arg=-pthread`
- `zed 1.24.4` (zed): `cargo:rustc-link-arg=-Wl,-rpath,/nix/store/*-libglvnd-1.7.0/lib`
- `zed 1.24.4` (zed): `cargo:rustc-link-arg=-Wl,-rpath,/nix/store/*-libva-2.24.1/lib`

## strict-link-search (system dirs not allowed)

- `alsa-sys 0.3.1` (zed): `cargo:rustc-link-search=native=/nix/store/*-alsa-lib-1.2.16.1/lib`
- `alsa-sys 0.4.0` (bevy): `cargo:rustc-link-search=native=/nix/store/*-alsa-lib-1.2.16.1/lib`
- `bzip2-sys 0.1.13+1.0.8` (zed): `cargo:rustc-link-search=native=/nix/store/*-bzip2-1.0.8/lib`
- `freetype-sys 0.20.1` (zed): `cargo:rustc-link-search=native=/nix/store/*-freetype-2.14.3/lib`
- `gio-sys 0.21.5` (zed): `cargo:rustc-link-search=native=/nix/store/*-glib-2.88.3/lib`
- `glib-sys 0.21.5` (zed): `cargo:rustc-link-search=native=/nix/store/*-glib-2.88.3/lib`
- `gobject-sys 0.21.5` (zed): `cargo:rustc-link-search=native=/nix/store/*-glib-2.88.3/lib`
- `libudev-sys 0.1.4` (bevy): `cargo:rustc-link-search=native=/nix/store/*-systemd-minimal-libs-262/lib`
- `libz-sys 1.1.20` (nushell): `cargo:rustc-link-search=native=/nix/store/*-zlib-1.3.2-static/lib`
- `libz-sys 1.1.20` (nushell): `cargo:rustc-link-search=native=/nix/store/*-zlib-1.3.2/lib`
- `libz-sys 1.1.21` (bat): `cargo:rustc-link-search=native=/nix/store/*-zlib-1.3.2-static/lib`
- `libz-sys 1.1.21` (bat): `cargo:rustc-link-search=native=/nix/store/*-zlib-1.3.2/lib`
- `libz-sys 1.1.22` (cargo): `cargo:rustc-link-search=native=/nix/store/*-zlib-1.3.2-static/lib`
- `libz-sys 1.1.22` (cargo): `cargo:rustc-link-search=native=/nix/store/*-zlib-1.3.2/lib`
- `openssl-sys 0.9.107` (nushell): `cargo:rustc-link-search=native=/nix/store/*-openssl-3.5.9/lib`
- `openssl-sys 0.9.109` (axum): `cargo:rustc-link-search=native=/nix/store/*-openssl-3.5.9/lib`
- `openssl-sys 0.9.109` (cargo): `cargo:rustc-link-search=native=/nix/store/*-openssl-3.5.9/lib`
- `openssl-sys 0.9.117` (zed): `cargo:rustc-link-search=native=/nix/store/*-openssl-3.5.9/lib`
- `wayland-sys 0.31.11` (bevy): `cargo:rustc-link-search=native=/nix/store/*-wayland-1.26.0/lib`
- `x11 2.21.0` (zed): `cargo:rustc-link-search=native=/nix/store/*-libx11-1.8.13/lib`
- `yeslogic-fontconfig-sys 6.0.0` (zed): `cargo:rustc-link-search=native=/nix/store/*-fontconfig-2.18.3-lib/lib`
- `yeslogic-fontconfig-sys 6.0.0` (zed): `cargo:rustc-link-search=native=/nix/store/*-freetype-2.14.3/lib`
- `zstd-sys 2.0.16+zstd.1.5.7` (zed): `cargo:rustc-link-search=native=/nix/store/*-zstd-1.5.7/lib`

## strict-rustc-env (any CARGO_*/RUSTC*)

- `cargo 0.92.0` (cargo): `cargo:rustc-env=CARGO_COMMIT_DATE=2025-10-10`
- `cargo 0.92.0` (cargo): `cargo:rustc-env=CARGO_COMMIT_HASH=ea2d97820c16195b0ca3fadb4319fe512c199a43`
- `cargo 0.92.0` (cargo): `cargo:rustc-env=CARGO_COMMIT_SHORT_HASH=ea2d97820`
