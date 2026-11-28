//! Tests for deterministic pre-run Tier-2 native build-script classification
//! (`crate::native::NO_WASM_CLASS`, `source_requires_native`, `placement_with_source`).

use blaze_manifest::lock::LockId;
use blaze_own::{AVec, Alloc};
use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::PackageEdition;

use crate::native::{
    Closure, NO_WASM_CLASS, NativeSourceReason, PackageSource, Runner, closure,
    closure_with_source, placement_with_source, source_requires_native,
    source_tree_requires_native,
};
use crate::toolchain::Mode;

fn t(s: &str) -> Text<'static> {
    Text::from_str_in(s, Alloc::global())
}

fn unit(name: &str, kind: UnitKind<'static>, deps: &[u32]) -> Unit<'static> {
    let mut d = AVec::new_in(Alloc::global());
    for u in deps {
        d.push(UnitDep { name: t("dep"), unit: UnitId(*u) });
    }
    Unit {
        package: LockId(0),
        name: t(name),
        version: t("1.0.0"),
        kind,
        for_host: true,
        feature_side: blaze_unit_graph::HostFeatureSide::Target,
        edition: PackageEdition::of_manifest("2021").expect("an edition"),
        crate_name: t(name),
        root: t("build.rs"),
        features: AVec::new_in(Alloc::global()),
        cfg: AVec::new_in(Alloc::global()),
        deps: d,
        build_script: None,
        links: None,
    }
}

#[test]
fn no_wasm_class_is_sorted_and_covers_protobuf_codegen_deps() {
    assert!(NO_WASM_CLASS.is_sorted(), "NO_WASM_CLASS must stay sorted for binary_search");
    for expected in [
        "bindgen",
        "clang-sys",
        "libloading",
        "prost-build",
        "protobuf-build",
        "protoc-bin-vendored",
        "tonic-build",
        "tonic-prost-build",
    ] {
        assert!(
            NO_WASM_CLASS.binary_search(&expected).is_ok(),
            "NO_WASM_CLASS missing {expected}"
        );
    }
}

#[test]
fn raft_proto_and_codex_code_mode_protocol_closures_place_native_in_every_mode() {
    let alloc = Alloc::global();
    let mut units = AVec::new_in(alloc);
    // 0: raft-proto build.rs -> protobuf-build (1) -> prost-build (2)
    units.push(unit("raft-proto", UnitKind::BuildScript, &[1]));
    units.push(unit("protobuf-build", UnitKind::Lib, &[2]));
    units.push(unit("prost-build", UnitKind::Lib, &[]));
    // 3: codex-code-mode-protocol build.rs -> tonic-prost-build (4), protoc-bin-vendored (5)
    units.push(unit("codex-code-mode-protocol", UnitKind::BuildScript, &[4, 5]));
    units.push(unit("tonic-prost-build", UnitKind::Lib, &[6]));
    units.push(unit("protoc-bin-vendored", UnitKind::Lib, &[]));
    units.push(unit("tonic-build", UnitKind::Lib, &[]));
    // 7: pure build.rs with 0 build-deps
    units.push(unit("pure", UnitKind::BuildScript, &[]));
    let graph = UnitGraph {
        units,
        resolver: Resolver::V3,
        meta: AVec::new_in(alloc),
        left_out: AVec::new_in(alloc),
    };

    let raft = closure(&graph, UnitId(0), alloc);
    assert_eq!(raft.no_wasm, Some(UnitId(1)));
    let codex = closure(&graph, UnitId(3), alloc);
    assert_eq!(codex.no_wasm, Some(UnitId(4)));
    let pure = closure(&graph, UnitId(7), alloc);
    assert_eq!(pure, Closure { runner: Runner::Wasm, no_wasm: None });

    for mode in [Mode::Check, Mode::Build, Mode::Run] {
        assert_eq!(
            placement_with_source(raft, mode, "raft-proto", UnitId(0), None),
            Runner::Native { because: UnitId(1) },
            "raft-proto {mode:?}"
        );
        assert_eq!(
            placement_with_source(codex, mode, "codex-code-mode-protocol", UnitId(3), None),
            Runner::Native { because: UnitId(4) },
            "codex-code-mode-protocol {mode:?}"
        );
        assert_eq!(
            placement_with_source(pure, mode, "pure", UnitId(7), Some("fn main() {}")),
            Runner::Wasm,
            "pure {mode:?}"
        );
    }
}

#[test]
fn kvm_ioctls_host_os_std_and_unsafe_extern_c_classify_native() {
    let kvm_ioctls = r#"
        fn main() {
            if std::path::Path::new("/dev/sev").exists() && kvm_supports_memory_encrypt() {
                println!("cargo:rustc-cfg=has_sev");
            }
        }
        fn kvm_supports_memory_encrypt() -> bool {
            use std::os::unix::io::AsRawFd;
            let Ok(kvm) = std::fs::File::open("/dev/kvm") else { return false; };
            unsafe extern "C" {
                fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
            }
            unsafe { ioctl(kvm.as_raw_fd(), 0xAE03, 120) > 0 }
        }
    "#;
    assert_eq!(source_requires_native(kvm_ioctls), Some(NativeSourceReason::HostOsStd));

    let extern_only = r#"
        fn main() {
            extern "C" {
                fn access(path: *const i8, mode: i32) -> i32;
            }
        }
    "#;
    assert_eq!(source_requires_native(extern_only), Some(NativeSourceReason::ExternC));

    let unsafe_extern_system = r#"
        unsafe extern "system" {
            fn GetVersion() -> u32;
        }
        fn main() {}
    "#;
    assert_eq!(source_requires_native(unsafe_extern_system), Some(NativeSourceReason::ExternC));

    let global_std_os_fd = r#"
        fn main() {
            let _ = ::std::os::fd::AsRawFd::as_raw_fd;
        }
    "#;
    assert_eq!(source_requires_native(global_std_os_fd), Some(NativeSourceReason::HostOsStd));

    let grouped_std_os = r#"
        use std::os::{unix::io::AsRawFd};
        fn main() {}
    "#;
    assert_eq!(source_requires_native(grouped_std_os), Some(NativeSourceReason::HostOsStd));

    let wasm_closure = Closure { runner: Runner::Wasm, no_wasm: None };
    for mode in [Mode::Check, Mode::Build, Mode::Run] {
        assert_eq!(
            placement_with_source(wasm_closure, mode, "kvm-ioctls", UnitId(9), Some(kvm_ioctls)),
            Runner::Native { because: UnitId(9) }
        );
    }
}

#[test]
fn rustler_host_family_cfg_classifies_native() {
    let rustler = r#"
        fn main() {
            let target_family_or_current =
                std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_else(|_| std::env::consts::FAMILY.to_string());
            let target_family = if target_family_or_current == "windows" {
                "win"
            } else if cfg!(target_family = "unix") {
                "unix"
            } else {
                panic!("Unsupported Operational System Family")
            };
            let _ = target_family;
        }
    "#;
    assert_eq!(source_requires_native(rustler), Some(NativeSourceReason::HostFamilyCfg));
    assert_eq!(
        source_requires_native("fn main() { if cfg!(unix) { println!(\"unix\"); } }"),
        Some(NativeSourceReason::HostFamilyCfg)
    );
    assert_eq!(
        source_requires_native("fn main() { if cfg!(all(target_family = \"unix\", feature = \"x\")) {} }"),
        Some(NativeSourceReason::HostFamilyCfg)
    );

    let wasm_closure = Closure { runner: Runner::Wasm, no_wasm: None };
    assert_eq!(
        placement_with_source(wasm_closure, Mode::Check, "rustler", UnitId(4), Some(rustler)),
        Runner::Native { because: UnitId(4) }
    );
}

#[test]
fn negative_controls_stay_wasm() {
    // 1. Guarded std::os::unix import (single statement).
    let guarded_use = r#"
        #[cfg(unix)]
        use std::os::unix::io::AsRawFd;
        fn main() {}
    "#;
    assert_eq!(source_requires_native(guarded_use), None);

    // 2. Real psm-0.1.32 pattern: #[cfg(unix)] let (...) = { use std::os::unix::fs::MetadataExt; ... };
    let psm_guarded_let = r#"
        fn main() {
            #[cfg(unix)]
            let (uid, gid, perms) = {
                use std::os::unix::fs::MetadataExt;
                (0u32, 0u32, 0u32)
            };
            println!("cargo:rerun-if-changed=build.rs");
        }
    "#;
    assert_eq!(source_requires_native(psm_guarded_let), None);

    // 3. Guarded extern "C" block (target_os, not(target_family = "wasm"), all(unix, ...)).
    let guarded_extern = r#"
        #[cfg(target_os = "linux")]
        extern "C" {
            fn foo();
        }
        #[cfg(not(target_family = "wasm"))]
        unsafe extern "C" {
            fn bar();
        }
        #[cfg(all(unix, feature = "extra"))]
        fn helper() {
            use std::os::macos::fs::MetadataExt;
            extern "C" { fn baz(); }
        }
        fn main() {}
    "#;
    assert_eq!(source_requires_native(guarded_extern), None);

    // 4. Inner #![cfg(unix)] inside a submodule.
    let inner_cfg_mod = r#"
        mod unix_only {
            #![cfg(unix)]
            use std::os::unix::io::AsRawFd;
            extern "C" { fn close(fd: i32) -> i32; }
        }
        fn main() {}
    "#;
    assert_eq!(source_requires_native(inner_cfg_mod), None);

    // 5. CARGO_CFG_TARGET_FAMILY env check and dtolnay's cfg!(target_os = "linux") cleanup check
    //    (anyhow, proc-macro2, thiserror) MUST stay Wasm.
    let env_and_dtolnay_linux_check = r#"
        fn main() {
            let fam = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
            if fam == "unix" {
                println!("cargo:rustc-cfg=is_unix");
            }
            if cfg!(target_os = "linux") || cfg!(windows) || cfg!(any(unix, target_os = "wasi")) {
                println!("ok");
            }
        }
    "#;
    assert_eq!(source_requires_native(env_and_dtolnay_linux_check), None);

    // 6. Comments (line + nested block comments) and string / raw-string / byte-string / char literals
    //    (including libsqlite3-sys's raw_line(r#"extern "C" { ... "#)).
    let comments_and_strings = r####"
        // use std::os::unix::io::AsRawFd;
        // extern "C" { fn ioctl(); }
        /* outer /* nested */ use std::os::linux::fs::MetadataExt; extern "C" { fn x(); } */
        fn main() {
            let a = "use std::os::unix::io::AsRawFd; extern \"C\" { fn foo(); }";
            let b = r#"extern "C" { pub fn sqlite3_auto_extension(); } std::os::unix::io::AsRawFd"#;
            let c = br##"cfg!(target_family = "unix") std::os::fd::AsRawFd"##;
            let d = 'a';
            let e = '\'';
            let f: &'static str = "cfg!(unix)";
            let _ = (a, b, c, d, e, f);
        }
    "####;
    assert_eq!(source_requires_native(comments_and_strings), None);

    // 7. `extern "C" fn` signatures and `extern crate` are NOT extern blocks.
    let extern_fn_and_crate = r#"
        extern crate core;
        type Cb = unsafe extern "C" fn(i32) -> i32;
        pub extern "C" fn exported(x: i32) -> i32 { x }
        fn main() {
            let _: Cb = exported;
        }
    "#;
    assert_eq!(source_requires_native(extern_fn_and_crate), None);

    // 8. Guard on one item does NOT leak to the next item!
    let guard_does_not_leak = r#"
        #[cfg(unix)]
        use std::os::unix::io::AsRawFd;
        use std::os::linux::fs::MetadataExt;
        fn main() {}
    "#;
    assert_eq!(source_requires_native(guard_does_not_leak), Some(NativeSourceReason::HostOsStd));

    let fn_guard_does_not_leak = r#"
        #[cfg(unix)]
        fn guarded() {
            use std::os::unix::io::AsRawFd;
        }
        fn unguarded() {
            extern "C" { fn foo(); }
        }
        fn main() {}
    "#;
    assert_eq!(source_requires_native(fn_guard_does_not_leak), Some(NativeSourceReason::ExternC));

    // 9. #[cfg(not(windows))] is TRUE on WASI, so host-OS std inside it is NOT guarded against WASI.
    let not_windows_active_on_wasi = r#"
        #[cfg(not(windows))]
        use std::os::unix::io::AsRawFd;
        fn main() {}
    "#;
    assert_eq!(
        source_requires_native(not_windows_active_on_wasi),
        Some(NativeSourceReason::HostOsStd)
    );

    // 10. Unused macro_rules! definitions containing extern blocks / std::os::unix / cfg!(unix)
    //     MUST stay Wasm (unexpanded macro bodies are not compiled code).
    let unused_macro_rules = r#"
        macro_rules! ffi {
            () => {
                use std::os::unix::io::AsRawFd;
                extern "C" { fn f(); }
                let _ = cfg!(unix);
            };
        }
        macro_rules! ffi_parens ( () => ( extern "C" { fn g(); } ) );
        macro_rules! ffi_brackets [ () => [ extern "C" { fn h(); } ] ];
        fn main() {
            println!("cargo:rerun-if-changed=build.rs");
        }
    "#;
    assert_eq!(source_requires_native(unused_macro_rules), None);

    // 11. Identical-on-host-and-WASI cfg! macros stay Wasm (`windows`, `not(windows)`,
    //     `any(unix, target_os = "wasi")`, `all(windows, unix)`).
    let identical_cfgs = r#"
        fn main() {
            if cfg!(windows)
                || cfg!(not(windows))
                || cfg!(any(unix, target_os = "wasi"))
                || cfg!(all(windows, unix))
                || cfg!(feature = "foo")
                || cfg!(debug_assertions)
            {
                println!("ok");
            }
        }
    "#;
    assert_eq!(source_requires_native(identical_cfgs), None);
}

#[test]
fn negated_and_diverging_cfg_macros_and_crate_level_cfg_classify_native() {
    // Negated & host-vs-WASI diverging cfg! macros must classify Native.
    for src in [
        r#"fn main() { if cfg!(not(unix)) { panic!("non-unix"); } }"#,
        r#"fn main() { if cfg!(not(target_family = "unix")) {} }"#,
        r#"fn main() { if cfg!(target_os = "wasi") {} }"#,
        r#"fn main() { if cfg!(not(target_os = "wasi")) {} }"#,
        r#"fn main() { if cfg!(target_family = "wasm") {} }"#,
        r#"fn main() { if cfg!(any(unix, windows)) {} }"#,
        r#"fn main() { if cfg!(all(not(windows), not(unix))) {} }"#,
        r#"fn main() { if cfg!(all(unix, feature = "extra")) {} }"#,
    ] {
        assert_eq!(
            source_requires_native(src),
            Some(NativeSourceReason::HostFamilyCfg),
            "expected HostFamilyCfg for: {src}"
        );
    }

    // Crate-level `#![cfg(...)]` gating the script to Unix/Linux/not-wasm must classify Native.
    for src in [
        "#![cfg(unix)]\nfn main() {}",
        "#![cfg(target_os = \"linux\")]\nfn main() {}",
        "#![cfg(not(target_family = \"wasm\"))]\nfn main() {}",
        "#![cfg(all(unix, feature = \"foo\"))]\nfn main() {}",
    ] {
        assert_eq!(
            source_requires_native(src),
            Some(NativeSourceReason::HostFamilyCfg),
            "expected crate-level HostFamilyCfg for: {src}"
        );
    }

    // Crate-level `#![cfg(windows)]` is dead on both supported Unix hosts and WASI -> None.
    assert_eq!(source_requires_native("#![cfg(windows)]\nfn main() {}"), None);
}

#[test]
fn source_tree_inspects_submodules_path_attr_and_include_files() {
    let alloc = Alloc::global();

    let scan = |files: &[(&str, &str)]| -> Result<Option<NativeSourceReason>, &'static str> {
        source_tree_requires_native("build.rs", alloc, |rel, _is_root, visit| {
            if let Some((_, text)) = files.iter().find(|(p, _)| *p == rel) {
                visit(text);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    };

    // 1. `mod host;` -> `host.rs` using std::os::unix
    assert_eq!(
        scan(&[
            ("build.rs", "mod host;\nfn main() { host::probe(); }"),
            ("host.rs", "pub fn probe() { use std::os::unix::io::AsRawFd; }"),
        ]),
        Ok(Some(NativeSourceReason::HostOsStd))
    );

    // 2. Nested `mod host;` -> `host/mod.rs` -> `mod ffi;` -> `host/ffi.rs` with extern "C"
    assert_eq!(
        scan(&[
            ("build.rs", "mod host;\nfn main() {}"),
            ("host/mod.rs", "mod ffi;"),
            ("host/ffi.rs", "unsafe extern \"C\" { fn ioctl(); }"),
        ]),
        Ok(Some(NativeSourceReason::ExternC))
    );

    // 3. `#[path = "helpers/probe.rs"] mod probe;`
    assert_eq!(
        scan(&[
            ("build.rs", "#[path = \"helpers/probe.rs\"] mod probe;\nfn main() {}"),
            ("helpers/probe.rs", "pub fn f() { let _ = cfg!(not(unix)); }"),
        ]),
        Ok(Some(NativeSourceReason::HostFamilyCfg))
    );

    // 4. `include!("extra.rs");`
    assert_eq!(
        scan(&[
            ("build.rs", "include!(\"extra.rs\");\nfn main() {}"),
            ("extra.rs", "extern \"C\" { fn access(); }"),
        ]),
        Ok(Some(NativeSourceReason::ExternC))
    );

    // 5. Negative control: `#[cfg(unix)] mod unix_only;` MUST NOT be followed under WASI.
    assert_eq!(
        scan(&[
            ("build.rs", "#[cfg(unix)]\nmod unix_only;\nfn main() {}"),
            ("unix_only.rs", "use std::os::unix::io::AsRawFd;"),
        ]),
        Ok(None)
    );

    // 6. Negative control: cyclic include! terminates cleanly.
    assert_eq!(
        scan(&[
            ("build.rs", "include!(\"a.rs\");\nfn main() {}"),
            ("a.rs", "include!(\"build.rs\");"),
        ]),
        Ok(None)
    );

    // 7. Read error propagation: caller error is returned, never collapsed to Wasm.
    let err = source_tree_requires_native("build.rs", alloc, |_rel, _is_root, _visit| -> Result<bool, &'static str> {
        Err("corrupt pack")
    });
    assert_eq!(err, Err("corrupt pack"));
}

#[test]
fn no_wasm_class_requires_crates_io_source_identity() {
    let alloc = Alloc::global();
    let mut units = AVec::new_in(alloc);
    let mut app_bs = unit("app", UnitKind::BuildScript, &[1]);
    app_bs.package = LockId(0);
    let mut dep = unit("prost-build", UnitKind::Lib, &[]);
    dep.package = LockId(1);
    units.push(app_bs);
    units.push(dep);
    let graph = UnitGraph {
        units,
        resolver: Resolver::V3,
        meta: AVec::new_in(alloc),
        left_out: AVec::new_in(alloc),
    };

    // Local/workspace/git/alt-registry `prost-build` (`PackageSource::Other`) stays Wasm!
    let local_closure = closure_with_source(&graph, UnitId(0), alloc, |_| PackageSource::Other);
    assert_eq!(local_closure, Closure { runner: Runner::Wasm, no_wasm: None });
    assert_eq!(
        placement_with_source(local_closure, Mode::Check, "app", UnitId(0), Some("fn main() {}")),
        Runner::Wasm
    );

    // Crates.io `prost-build` (`PackageSource::CratesIo`) is forced Native in every mode!
    let crates_io_closure = closure_with_source(&graph, UnitId(0), alloc, |u| {
        if u.package == LockId(1) { PackageSource::CratesIo } else { PackageSource::Other }
    });
    assert_eq!(crates_io_closure.no_wasm, Some(UnitId(1)));
    for mode in [Mode::Check, Mode::Build, Mode::Run] {
        assert_eq!(
            placement_with_source(crates_io_closure, mode, "app", UnitId(0), Some("fn main() {}")),
            Runner::Native { because: UnitId(1) }
        );
    }
}
