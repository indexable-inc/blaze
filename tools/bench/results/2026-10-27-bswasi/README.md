# Build scripts of the bench corpus under WASI

Made by `harness/bswasi.py record` then `report` (see its docstring for how a run is
put in a category). One row per project; each run of a build script counts once.

| project | cargo | runs | works | host cfg | threads | other | native failed |
|---|---|---|---|---|---|---|---|
| axum | 0 | 55 | 46 | 0 | 0 | 9 | 0 |
| bat | 0 | 28 | 22 | 0 | 0 | 6 | 0 |
| bevy | 0 | 72 | 64 | 1 | 0 | 7 | 0 |
| biome | 101 | 62 | 45 | 1 | 0 | 8 | 8 |
| cargo | 101 | 41 | 28 | 0 | 0 | 12 | 1 |
| fd | 0 | 9 | 8 | 0 | 0 | 1 | 0 |
| helix | 101 | 29 | 22 | 0 | 0 | 7 | 0 |
| hyperfine | 0 | 20 | 13 | 0 | 0 | 7 | 0 |
| just | 0 | 18 | 15 | 0 | 0 | 3 | 0 |
| nushell | 0 | 90 | 66 | 4 | 0 | 20 | 0 |
| ripgrep | 0 | 10 | 8 | 0 | 0 | 2 | 0 |
| zed | 101 | 190 | 136 | 0 | 0 | 53 | 1 |
| total | | 624 | 473 | 6 | 0 | 135 | 10 |

## works (473 runs, 312 crates)

- `ahash v0.8.11` (nushell)
- `ahash v0.8.12` (axum, bevy, helix, zed)
- `alsa-sys v0.3.1` (zed)
- `alsa-sys v0.4.0` (bevy)
- `anyhow v1.0.100` (axum, ripgrep)
- `anyhow v1.0.104` (bevy, biome, zed)
- `anyhow v1.0.93` (hyperfine)
- `anyhow v1.0.94` (nushell)
- `anyhow v1.0.97` (bat)
- `anyhow v1.0.98` (fd, helix)
- `anyhow v1.0.99` (cargo)
- `ash v0.38.0+1.3.281` (bevy, zed)
- `assert_cmd v2.0.16` (bat, hyperfine, nushell)
- `auto_update_helper v0.1.0` (zed)
- `aws-lc-rs v1.14.1` (axum)
- `aws-lc-rs v1.18.1` (zed)
- `aws-types v1.3.12` (zed)
- `aws-types v1.3.3` (nushell)
- `axum v0.6.20` (zed)
- `axum-core v0.3.4` (zed)
- `biome_diagnostics_categories v0.7.0` (biome)
- `blake3 v0.3.8` (axum)
- `bugreport v0.5.1` (bat)
- `bzip2-sys v0.1.13+1.0.8` (zed)
- `camino v1.1.12` (cargo)
- `camino v1.2.0` (just)
- `camino v1.2.6` (bevy, biome, zed)
- `cap-fs-ext v4.0.3` (zed)
- `cap-primitives v4.0.3` (zed)
- `cap-std v4.0.3` (zed)
- `cargo-test-support v0.8.1` (cargo)
- `codspeed-criterion-compat v5.0.1` (biome)
- `codspeed-divan-compat v5.0.1` (biome)
- `color-spantrace v0.3.0` (bevy)
- `cookie v0.18.1` (axum)
- `cpal v0.17.1` (zed)
- `cpal v0.17.3` (bevy)
- `cpp_demangle v0.5.1` (zed)
- `cranelift-assembler-x64 v0.135.1` (zed)
- `cranelift-isle v0.135.1` (zed)
- `crc32fast v1.3.2` (biome)
- `crc32fast v1.5.0` (axum, cargo, zed)
- `crc32fast v1.5.3` (bevy)
- `crossbeam-deque v0.8.8` (bevy)
- `crossbeam-epoch v0.9.20` (biome)
- `crossbeam-epoch v0.9.21` (bevy)
- `crossbeam-utils v0.8.20` (helix)
- `crossbeam-utils v0.8.21` (axum, bat, biome, cargo, fd, just, nushell, ripgrep, zed)
- `crossbeam-utils v0.8.23` (bevy)
- `crunchy v0.2.2` (nushell)
- `crunchy v0.2.4` (axum, zed)
- `curl v0.4.47` (nushell)
- `curl v0.4.49` (cargo)
- `doc-comment v0.3.3` (bat, hyperfine, nushell)
- `erased-serde v0.4.11` (bevy)
- `escargot v0.5.15` (cargo)
- `fd-find v10.3.0` (fd)
- `float16 v0.1.7` (biome)
- `freetype-sys v0.20.1` (zed)
- `generic-array v0.14.7` (axum, cargo, helix, just, nushell, zed)
- `getrandom v0.3.1` (bat, helix, nushell)
- `getrandom v0.3.3` (axum, cargo, fd, just)
- `getrandom v0.3.4` (bevy, biome, zed)
- `getrandom v0.4.1` (zed)
- `getrandom v0.4.3` (bevy, biome)
- `gilrs v0.11.2` (bevy)
- `gpui v0.2.2` (zed)
- `gpui_apple v0.1.0` (zed)
- `gpui_windows v0.1.0` (zed)
- `guppy-workspace-hack v0.1.0` (zed)
- `heapless v0.7.17` (bevy)
- `heapless v0.8.0` (cargo, helix)
- `heapless v0.9.1` (nushell)
- `heapless v0.9.2` (zed)
- `heed v0.21.0` (zed)
- `htmlize v1.1.0` (zed)
- `httparse v1.10.1` (axum, bevy, biome, zed)
- `httparse v1.9.5` (nushell)
- `hyperfine v1.19.0` (hyperfine)
- `icu_collator_data v2.3.0` (zed)
- `icu_locale_data v2.1.2` (bevy)
- `icu_locale_fallback_data v2.3.0` (zed)
- `icu_normalizer_data v2.0.0` (axum, cargo)
- `icu_normalizer_data v2.1.1` (bevy)
- `icu_normalizer_data v2.3.0` (biome, zed)
- `icu_properties_data v2.0.1` (axum, cargo)
- `icu_properties_data v2.1.2` (bevy)
- `icu_properties_data v2.3.0` (biome, zed)
- `icu_segmenter_data v2.1.1` (bevy)
- `im-rc v15.1.0` (cargo)
- `inotify-sys v0.1.8` (bevy)
- `instability v0.3.10` (bevy)
- `io-extras v0.19.0` (zed)
- `io-lifetimes v2.0.4` (zed)
- `io-lifetimes v3.0.1` (zed)
- `khronos-egl v6.0.0` (zed)
- `lalrpop-util v0.23.1` (zed)
- `lalrpop-util v0.23.2` (bevy)
- `libc v0.2.162` (hyperfine)
- `libc v0.2.174` (cargo, fd, helix, nushell)
- `libc v0.2.175` (bat)
- `libc v0.2.176` (axum, just)
- `libc v0.2.177` (ripgrep)
- `libc v0.2.186` (zed)
- `libc v0.2.190` (biome)
- `libc v0.2.191` (bevy)
- `libm v0.2.11` (nushell)
- `libm v0.2.15` (axum)
- `libm v0.2.16` (bevy, zed)
- `libudev-sys v0.1.4` (bevy)
- `libz-sys v1.1.8` (biome)
- `livekit v0.7.32` (zed)
- `logos-codegen v0.15.1` (zed)
- `mime_guess v2.0.5` (axum, nushell, zed)
- `minidump-writer v0.13.0` (zed)
- `moka v0.12.10` (nushell)
- `moka v0.12.11` (zed)
- `mongocrypt-sys v0.1.4+1.12.0` (axum)
- `multer v3.1.0` (axum)
- `naga v29.0.4` (zed)
- `naga v30.0.1` (bevy)
- `native-tls v0.2.14` (axum)
- `native-tls v0.2.18` (zed)
- `nix v0.29.0` (hyperfine, nushell)
- `nix v0.30.1` (bat, fd, just, nushell, zed)
- `nix v0.31.2` (biome)
- `nix v0.31.3` (bevy)
- `nu v0.108.0` (nushell)
- `num-bigint-dig v0.8.6` (zed)
- `num-bigint-dig v0.9.1` (zed)
- `object v0.37.3` (bevy, biome, zed)
- `object v0.39.1` (zed)
- `openssl v0.10.73` (axum)
- `openssl v0.10.81` (zed)
- `owo-colors v4.1.0` (nushell)
- `owo-colors v4.3.0` (biome)
- `palette v0.7.6` (zed)
- `parking_lot_core v0.9.10` (bat, nushell)
- `parking_lot_core v0.9.11` (axum, cargo, helix)
- `parking_lot_core v0.9.12` (bevy, biome, zed)
- `paste v1.0.15` (nushell)
- `pathfinder_simd v0.5.6` (zed)
- `pest_stack_guard v1.0.0` (bevy)
- `pet v0.1.0` (zed)
- `polars v0.51.0` (nushell)
- `polars-arrow v0.51.0` (nushell)
- `polars-compute v0.51.0` (nushell)
- `polars-core v0.51.0` (nushell)
- `polars-lazy v0.51.0` (nushell)
- `polars-ops v0.51.0` (nushell)
- `polars-plan v0.51.0` (nushell)
- `polars-schema v0.51.0` (nushell)
- `polars-stream v0.51.0` (nushell)
- `polars-utils v0.51.0` (nushell)
- `portable-atomic v1.10.0` (nushell)
- `portable-atomic v1.11.1` (zed)
- `portable-atomic v1.15.0` (bevy, biome)
- `portable-atomic v1.9.0` (hyperfine)
- `pq-sys v0.7.2` (axum)
- `prettyplease v0.2.35` (bat)
- `prettyplease v0.2.37` (biome, zed)
- `proc-macro-error v1.0.4` (axum, zed)
- `proc-macro-error-attr v1.0.4` (axum, zed)
- `proc-macro2 v1.0.101` (axum, cargo, just, ripgrep)
- `proc-macro2 v1.0.107` (bevy, biome, zed)
- `proc-macro2 v1.0.89` (hyperfine)
- `proc-macro2 v1.0.92` (nushell)
- `proc-macro2 v1.0.95` (bat, fd, helix)
- `proc-macro2-diagnostics v0.10.1` (zed)
- `procfs v0.17.0` (nushell)
- `prometheus v0.14.0` (zed)
- `protobuf v3.7.2` (zed)
- `pulldown-cmark v0.10.3` (zed)
- `pulldown-cmark v0.13.0` (cargo, helix)
- `pulldown-cmark v0.13.3` (biome)
- `pulldown-cmark v0.13.4` (zed)
- `pulldown-cmark v0.9.6` (just)
- `quote v1.0.41` (ripgrep)
- `quote v1.0.47` (bevy, biome, zed)
- `radium v0.7.0` (axum, bevy, biome)
- `rayon-core v1.12.1` (bat, helix)
- `rayon-core v1.13.0` (bevy, biome, cargo, just, nushell, zed)
- `ref-cast v1.0.23` (nushell)
- `ref-cast v1.0.25` (biome, zed)
- `release_channel v0.1.0` (zed)
- `rstest_macros v0.23.0` (nushell)
- `rust_decimal v1.36.0` (hyperfine, nushell)
- `rustc_version_runtime v0.3.0` (axum)
- `rustix v0.38.40` (hyperfine)
- `rustix v0.38.42` (nushell)
- `rustix v0.38.43` (bat)
- `rustix v0.38.44` (bevy, helix)
- `rustix v1.0.1` (biome)
- `rustix v1.0.7` (fd, helix, nushell)
- `rustix v1.0.8` (cargo)
- `rustix v1.1.4` (zed)
- `rustix v1.1.6` (bevy)
- `rustls v0.21.12` (axum, nushell)
- `rustls v0.23.28` (nushell)
- `rustls v0.23.32` (axum)
- `rustls v0.23.45` (zed)
- `rustls v0.23.46` (bevy, biome)
- `rustversion v1.0.18` (nushell)
- `rustversion v1.0.19` (biome)
- `rustversion v1.0.22` (axum, just, zed)
- `rustversion v1.0.23` (bevy)
- `scratch v1.0.9` (zed)
- `selectors v0.37.0` (zed)
- `self_update v1.3.1` (biome)
- `semver v1.0.23` (nushell)
- `semver v1.0.25` (bat)
- `semver v1.0.26` (cargo)
- `serde v1.0.214` (hyperfine)
- `serde v1.0.217` (bat)
- `serde v1.0.219` (cargo, helix)
- `serde v1.0.225` (nushell)
- `serde v1.0.228` (axum, just, ripgrep)
- `serde v1.0.229` (bevy, biome, zed)
- `serde_core v1.0.225` (nushell)
- `serde_core v1.0.228` (axum, just, ripgrep)
- `serde_core v1.0.229` (bevy, biome, zed)
- `serde_json v1.0.132` (hyperfine)
- `serde_json v1.0.133` (nushell)
- `serde_json v1.0.135` (bat)
- `serde_json v1.0.140` (helix)
- `serde_json v1.0.143` (cargo)
- `serde_json v1.0.145` (axum, just, ripgrep)
- `serde_json v1.0.151` (biome, zed)
- `serde_json v1.0.154` (bevy)
- `serde_json_lenient v0.2.4` (zed)
- `serde_yml v0.0.12` (bevy)
- `signal-hook v0.3.17` (nushell)
- `signal-hook v0.3.18` (bat, bevy, helix)
- `signal-hook v0.4.4` (zed)
- `slotmap v1.0.7` (helix, nushell, zed)
- `slotmap v1.1.1` (bevy)
- `smithay-client-toolkit v0.19.2` (bevy)
- `snap v1.1.1` (nushell)
- `stacker v0.1.17` (nushell)
- `stacker v0.1.25` (bevy, zed)
- `syn v1.0.109` (axum, biome, cargo, nushell, zed)
- `tango-bench v0.6.0` (nushell)
- `target-lexicon v0.13.5` (zed)
- `target-spec v3.7.0` (zed)
- `target-triple v0.1.4` (axum)
- `target-triple v1.0.0` (biome)
- `thiserror v1.0.63` (biome)
- `thiserror v1.0.69` (axum, bat, bevy, cargo, nushell, zed)
- `thiserror v2.0.12` (helix, nushell)
- `thiserror v2.0.16` (axum, bat, cargo)
- `thiserror v2.0.17` (zed)
- `thiserror v2.0.20` (biome)
- `thiserror v2.0.21` (bevy)
- `thiserror v2.0.3` (hyperfine)
- `tiny-keccak v2.0.2` (axum, nushell, zed)
- `tokio-openssl v0.6.5` (axum)
- `tree-sitter-language v0.1.8` (zed)
- `typeid v1.0.2` (nushell)
- `typeid v1.0.3` (bevy, biome, cargo)
- `typenum v1.17.0` (nushell)
- `typenum v1.18.0` (axum, cargo, helix, just)
- `typenum v1.19.0` (zed)
- `ui v0.1.0` (zed)
- `unicase v2.6.0` (biome)
- `unicase v2.7.0` (helix)
- `unicode-general-category v1.0.0` (helix)
- `unicode-general-category v1.1.0` (zed)
- `v_htmlescape v0.15.8` (nushell)
- `wasm-bindgen-shared v0.2.117` (biome)
- `wasm-encoder v0.227.1` (zed)
- `wasmparser v0.227.1` (zed)
- `wasmtime-internal-component-macro v48.0.1` (zed)
- `wasmtime-internal-core v48.0.1` (zed)
- `wasmtime-internal-cranelift v48.0.1` (zed)
- `wasmtime-internal-unwinder v48.0.1` (zed)
- `wayland-backend v0.3.17` (bevy)
- `wayland-client v0.31.11` (zed)
- `wayland-client v0.31.15` (bevy)
- `wayland-sys v0.31.11` (bevy, zed)
- `web_atoms v0.2.6` (zed)
- `wgpu v29.0.4` (zed)
- `wgpu v30.0.1` (bevy)
- `wgpu-core v29.0.4` (zed)
- `wgpu-core v30.0.1` (bevy)
- `wgpu-hal v29.0.4` (zed)
- `wgpu-hal v30.0.1` (bevy)
- `winit v0.30.13` (bevy)
- `wit-bindgen-rt v0.22.0` (zed)
- `wit-bindgen-rt v0.41.0` (zed)
- `wit-bindgen-rust v0.22.0` (zed)
- `wit-bindgen-rust v0.41.0` (zed)
- `wit-bindgen-rust-macro v0.22.0` (zed)
- `wit-bindgen-rust-macro v0.41.0` (zed)
- `x11 v2.21.0` (zed)
- `x11-dl v2.21.0` (bevy)
- `xcb v1.6.0` (zed)
- `xim-parser v0.2.1` (zed)
- `yeslogic-fontconfig-sys v6.0.0` (zed)
- `zed-font-kit v0.14.1-zed` (zed)
- `zed_extension_api v0.1.0` (zed)
- `zed_extension_api v0.7.0` (zed)
- `zed_extension_api v0.8.0` (zed)
- `zerocopy v0.8.23` (nushell)
- `zerocopy v0.8.25` (helix)
- `zerocopy v0.8.26` (cargo)
- `zerocopy v0.8.27` (axum, just, zed)
- `zerocopy v0.8.63` (bevy)
- `zmij v1.0.2` (biome)
- `zmij v1.0.23` (bevy, zed)
- `zstd-safe v7.2.1` (nushell)
- `zstd-safe v7.2.4` (axum)
- `ztracing v0.1.0` (zed)

## host cfg (6 runs, 6 crates)

- `chrono-tz v0.10.0` (nushell): in a build dependency: phf_codegen 0.11's tables from rand's SmallRng, another generator where target_pointer_width is 32 (timezones.rs)
- `chrono-tz v0.9.0` (bevy): in a build dependency: phf_codegen 0.11's tables from rand's SmallRng, another generator where target_pointer_width is 32 (timezones.rs)
- `htmlize v1.0.6` (biome): in a build dependency: phf_codegen 0.11's tables from rand's SmallRng, another generator where target_pointer_width is 32 (entities.rs)
- `markup5ever v0.12.1` (nushell): in a build dependency: phf_codegen 0.11's tables from rand's SmallRng, another generator where target_pointer_width is 32 (generated.rs, named_entities.rs)
- `selectors v0.31.0` (nushell): in a build dependency: phf_codegen 0.11's tables from rand's SmallRng, another generator where target_pointer_width is 32 (ascii_case_insensitive_html_attributes.rs)
- `web_atoms v0.1.3` (nushell): in a build dependency: phf_codegen 0.11's tables from rand's SmallRng, another generator where target_pointer_width is 32 (generated.rs, named_entities.rs)

## other: CC/AR/CXX not given (cargo does not document them): the same OUT_DIR, and only cc-rs's report of them and its rerun-if-env-changed lines differ (64 runs, 52 crates)

- `alloca v0.4.0` (bevy, nushell, zed): AR, CC
- `blake3 v1.8.2` (cargo, just, nushell): AR, CC
- `blake3 v1.8.7` (bevy): AR, CC
- `cxx v1.0.187` (zed): AR, CXX, CXXFLAGS
- `libgit2-sys v0.18.0+1.9.0` (bat, nushell): AR, CC
- `libgit2-sys v0.18.2+1.9.1` (cargo): AR, CC
- `libgit2-sys v0.18.5+1.9.4` (biome): CC
- `libnghttp2-sys v0.1.11+1.64.0` (cargo): AR, CC
- `libsqlite3-sys v0.30.1` (zed): AR, CC
- `libsqlite3-sys v0.35.0` (cargo, nushell): AR, CC
- `libz-sys v1.1.20` (nushell): CC
- `libz-sys v1.1.21` (bat): CC
- `libz-sys v1.1.22` (cargo): CC
- `link-cplusplus v1.0.12` (zed): AR, CXX, CXXFLAGS
- `lmdb-master-sys v0.2.5` (zed): AR, CC
- `lz4-sys v1.11.1+lz4-1.10.0` (nushell): AR, CC
- `onig_sys v69.9.1` (bat): AR, CC
- `openssl-sys v0.9.107` (nushell): CC
- `openssl-sys v0.9.109` (axum, cargo): CC
- `openssl-sys v0.9.117` (zed): CC
- `pcre2-sys v0.2.10` (ripgrep): AR, CC
- `psm v0.1.24` (bevy, nushell): AR, CC
- `psm v0.1.30` (zed): AR, CC
- `ring v0.17.13` (biome, nushell): CC
- `ring v0.17.14` (axum, bevy, zed): CC
- `tree-house-bindings v0.2.1` (helix): AR, CC
- `tree-sitter v0.27.0` (zed): AR, CC
- `tree-sitter-bash v0.25.1` (zed): AR, CC
- `tree-sitter-c v0.24.1` (zed): AR, CC
- `tree-sitter-cpp v0.23.4` (zed): AR, CC
- `tree-sitter-css v0.23.2` (zed): AR, CC
- `tree-sitter-diff v0.1.0` (zed): AR, CC
- `tree-sitter-elixir v0.3.4` (zed): AR, CC
- `tree-sitter-embedded-template v0.23.2` (zed): AR, CC
- `tree-sitter-gitcommit v0.0.1` (zed): AR, CC
- `tree-sitter-go v0.25.0` (zed): AR, CC
- `tree-sitter-gomod v1.1.1` (zed): AR, CC
- `tree-sitter-gowork v0.0.1` (zed): AR, CC
- `tree-sitter-heex v0.0.1` (zed): AR, CC
- `tree-sitter-html v0.23.2` (zed): AR, CC
- `tree-sitter-jsdoc v0.23.2` (zed): AR, CC
- `tree-sitter-json v0.24.8` (zed): AR, CC
- `tree-sitter-md v0.3.2` (zed): AR, CC
- `tree-sitter-python v0.25.0` (zed): AR, CC
- `tree-sitter-regex v0.24.3` (zed): AR, CC
- `tree-sitter-ruby v0.23.1` (zed): AR, CC
- `tree-sitter-rust v0.24.2` (zed): AR, CC
- `tree-sitter-typescript v0.23.2` (zed): AR, CC
- `tree-sitter-yaml v0.6.1` (zed): AR, CC
- `wayland-backend v0.3.15` (zed): AR, CC
- `zstd-sys v2.0.13+zstd.1.5.6` (nushell): AR, CC
- `zstd-sys v2.0.16+zstd.1.5.7` (axum): AR, CC

## other: WASI has no permission bits (autocfg sees OUT_DIR read-only) (31 runs, 16 crates)

- `async-io v2.6.0` (bevy, zed): cargo:warning: cargo:warning=async-io: failed to detect compiler features: output path is not a writable directory
- `bigdecimal v0.4.8` (nushell): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `eyre v0.6.14` (bevy): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `fs-err v3.1.2` (axum): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `lock_api v0.4.12` (bat, nushell): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `lock_api v0.4.13` (axum, cargo, helix): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `memoffset v0.9.1` (zed): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `num-bigint v0.2.6` (hyperfine): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `num-complex v0.2.4` (hyperfine): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `num-rational v0.2.4` (hyperfine): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `num-traits v0.2.19` (axum, bat, bevy, biome, cargo, helix, hyperfine, just, nushell, zed): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `rand v0.6.5` (hyperfine): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `rand_chacha v0.1.1` (hyperfine): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `rand_pcg v0.1.2` (hyperfine): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }
- `slab v0.4.9` (helix, nushell): cargo:warning: cargo:warning=slab: failed to detect compiler features: output path is not a writable directory
- `smartstring v1.0.1` (helix): called `Result::unwrap()` on an `Err` value: Error { kind: Other("output path is not a writable directory") }

## other: an operation WASI std does not support (4 runs, 4 crates)

- `gio-sys v0.21.5` (zed): std::env::join_paths: system-deps leaves out cargo:include
- `glib-sys v0.21.5` (zed): std::env::join_paths: system-deps leaves out cargo:include
- `gobject-sys v0.21.5` (zed): std::env::join_paths: system-deps leaves out cargo:include
- `html5ever v0.27.0` (nushell): called `Result::unwrap()` on an `Err` value: Os { code: 58, kind: Unsupported, message: "Not supported" }

## other: asks its working directory, which is / under WASI (wasi-libc), not the package's (1 runs, 1 crates)

- `aws-lc-sys v0.45.0` (zed): gives a command /aws-lc/tests/compiler_features_tests/c11.c

## other: failed (1 runs, 1 crates)

- `rustix v1.1.2` (just): not supported by WASI yet

## other: not built for wasm32-wasip1 (13 runs, 13 crates)

- `aws-lc-sys v0.32.1` (axum): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 2 functions blaze could not compile, and a build with nothing of rustc's has nothing else to compile them, so it is not built:
- `bat v0.26.0` (bat): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:
- `biome_aria_metadata v0.7.0` (biome): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:
- `encoding_rs v0.8.42` (biome): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:
- `helix-term v25.7.1` (helix): blaze did not build it (exit status: 1): Error: `env!("BUILD_TARGET")` in helix_loader names a variable that is not set
- `livekit_api v0.1.0` (zed): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 19 functions blaze could not compile, and a build with nothing of rustc's has nothing else to compile them, so it is not built:
- `octocrab v0.49.7` (zed): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:
- `oem_cp v2.1.0` (nushell): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:
- `pbjson-types v0.9.0` (zed): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:
- `proto v0.1.0` (zed): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 19 functions blaze could not compile, and a build with nothing of rustc's has nothing else to compile them, so it is not built:
- `wasm-bindgen v0.2.117` (biome): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:
- `wasmtime v48.0.1` (zed): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:
- `wasmtime-internal-fiber v48.0.1` (zed): blaze did not build it (exit status: 1): Error: `build_script_build` reaches 1 function blaze could not compile, and a build with nothing of rustc's has nothing else to compile it, so it is not built:

## other: reads outside its package and OUT_DIR (4 runs, 4 crates)

- `edit_prediction_cli v0.1.0` (zed): Failed to read crates/zed/Cargo.toml: Os { code: 63, kind: PermissionDenied, message: "Operation not permitted" }
- `eval_cli v0.1.0` (zed): Failed to read crates/zed/Cargo.toml: Os { code: 63, kind: PermissionDenied, message: "Operation not permitted" }
- `extension_host v0.1.0` (zed): Error: Os { code: 63, kind: PermissionDenied, message: "Operation not permitted" }
- `uucore v0.2.2` (nushell): Error: Os { code: 44, kind: NotFound, message: "No such file or directory" }

## other: runs a program a script under WASI may not (17 runs, 17 crates)

- `cargo v0.92.0` (cargo): git
- `cli v0.1.0` (zed): git
- `codspeed v5.0.1` (biome): cargo
- `cranelift-codegen v0.135.1` (zed): git
- `curl-sys v0.4.78+curl-8.11.0` (nushell): git
- `curl-sys v0.4.83+curl-8.15.0` (cargo): git
- `helix-loader v25.7.1` (helix): git
- `libssh2-sys v0.3.0` (nushell): git
- `libssh2-sys v0.3.1` (cargo): git
- `nu-cmd-lang v0.108.0` (nushell): cargo, git
- `remote_server v0.1.0` (zed): git
- `ripgrep v15.1.0` (ripgrep): git
- `tikv-jemalloc-sys v0.6.0+5.3.0-1-ge13ca993e8ccb9ba9847cc330696e02839f328f7` (fd): sh
- `tikv-jemalloc-sys v0.6.1+5.3.0-1-ge13ca993e8ccb9ba9847cc330696e02839f328f7` (biome): sh
- `varisat v0.2.2` (cargo): drat-trim, rate
- `wasmtime-c-api-impl v48.0.1` (zed): cmake
- `zed v1.24.4` (zed): git

## native failed (10 runs, 9 crates)

- `biome_css_analyze v0.7.0` (biome)
- `biome_graphql_analyze v0.7.0` (biome)
- `biome_html_analyze v0.5.7` (biome)
- `biome_js_analyze v0.5.7` (biome)
- `biome_json_analyze v0.7.0` (biome)
- `biome_markdown_analyze v0.1.0` (biome)
- `biome_yaml_analyze v0.1.0` (biome)
- `build-rs-test-lib v0.0.0` (cargo)
- `zstd-sys v2.0.16+zstd.1.5.7` (zed)
