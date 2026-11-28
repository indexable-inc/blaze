use blaze_own::{AString, AVec, Alloc, Vec};
use blaze_pins::CLANG_JSON as PIN;
use blaze_unit_graph::UnitId;

use super::{
    CInputs, ClangPin, Mode, NATIVE_EVERY_MODE, PinError, SHELF_MEMBERS_LINUX, Shelf, Sysroot, c_env, clang_major, clang_pin, depfile_reads,
    for_each_nix_lib_candidate, for_each_nix_rpath_in_flags, for_each_nix_sysroot_candidate, headers_digest, nix_dynamic_linker_in_flags,
    placement, shelf_missing_for, sysroot,
};
use crate::native::Runner;

fn hex(bytes: &[u8; 32]) -> std::string::String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Both rows of the checked-in table, as D69 pins them.
#[test]
fn the_pin_table_holds_both_archives() {
    let mac = clang_pin(PIN, "aarch64-apple-darwin").expect("the macOS row");
    let linux = clang_pin(PIN, "aarch64-unknown-linux-gnu").expect("the Linux row");
    assert_eq!(
        (mac.name, mac.size, hex(&mac.sha256).as_str()),
        ("LLVM-23.1.4-macOS-ARM64.tar.xz", 1_570_051_292, "5d3179db39adeca5b17a34d82708fe976bc60e055b05fe557878fb3fb64d39ef")
    );
    assert_eq!(
        (linux.name, linux.size, hex(&linux.sha256).as_str()),
        ("LLVM-23.1.4-Linux-ARM64.tar.xz", 1_829_686_400, "9b27cf263d711aa6ebfd4045ab241d947bd09b3cd19ffa5728f48a80c9eb3a91")
    );
    assert_eq!(mac.release, "https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.4");
    let _: ClangPin<'_> = mac;
    assert_eq!(clang_pin(PIN, "x86_64-pc-windows-msvc"), Err(PinError::NoRow { target: "x86_64-pc-windows-msvc" }));
}

/// macOS takes the machine's SDK, Linux ARM64 uses the host `/usr` sysroot, and Linux x86_64
/// remains unpinned (`null`: a typed refusal).
#[test]
fn linux_has_no_sysroot_pinned_yet() {
    assert_eq!(sysroot(PIN, "aarch64-apple-darwin"), Ok(Sysroot::MacosSdk));
    assert_eq!(sysroot(PIN, "aarch64-unknown-linux-gnu"), Ok(Sysroot::LinuxHost));
    let refused = sysroot(PIN, "x86_64-unknown-linux-gnu").expect_err("no sysroot is pinned for x86_64");
    assert_eq!(refused, PinError::NoSysroot { target: "x86_64-unknown-linux-gnu" });
    assert!(refused.to_string().contains("no sysroot pinned for x86_64-unknown-linux-gnu"), "{refused}");
}

/// Linux shelves do not ship macOS-only `bin/ld64.lld` or `lib/libclang.dylib`.
#[test]
fn linux_shelf_members_do_not_require_macos_only_files() {
    assert!(SHELF_MEMBERS_LINUX.is_sorted());
    assert_eq!(shelf_missing_for(Sysroot::LinuxHost, |m| m != "lib/libclang.dylib" && m != "bin/ld64.lld"), None);
    assert_eq!(shelf_missing_for(Sysroot::LinuxHost, |m| m != "bin/clang-23"), Some("bin/clang-23"));
    assert_eq!(shelf_missing_for(Sysroot::MacosSdk, |m| m != "lib/libclang.dylib"), Some("lib/libclang.dylib"));
}

#[test]
fn a_depfile_lists_what_the_compile_read() {
    let alloc = Alloc::global();
    let text = "out/a.o: /pkg/a.c /SDK/usr/include/stdio.h \\\n  /SDK/usr/include/a\\ b.h\n";
    let mut reads: AVec<'_, AString<'_>> = Vec::new_in(alloc);
    depfile_reads(text, &mut reads, alloc);
    let reads: std::vec::Vec<&str> = reads.iter().map(|r| r.as_str()).collect();
    assert_eq!(reads, ["/pkg/a.c", "/SDK/usr/include/stdio.h", "/SDK/usr/include/a b.h"]);
}

/// The digest is of the set read, not of the order the compiles finished in.
#[test]
fn the_headers_digest_is_of_the_set() {
    let mut one = [("usr/include/stdio.h", [1; 32]), ("usr/include/stdlib.h", [2; 32])];
    let mut two = [("usr/include/stdlib.h", [2; 32]), ("usr/include/stdio.h", [1; 32]), ("usr/include/stdio.h", [1; 32])];
    let mut changed = [("usr/include/stdio.h", [1; 32]), ("usr/include/stdlib.h", [3; 32])];
    assert_eq!(headers_digest(&mut one), headers_digest(&mut two));
    assert_ne!(headers_digest(&mut one), headers_digest(&mut changed));
}

/// D112 N5: bindgen's libclang is told the shelf's resource dir by the pin's major (reth-mdbx-sys:
/// "'stdarg.h' file not found" without it); a table without a numeric `version` is refused.
/// Prints "test toolchain::tests::bindgen_is_told_the_shelf_resource_dir ... ok".
#[test]
fn bindgen_is_told_the_shelf_resource_dir() {
    let alloc = blaze_own::testing::heap().alloc();
    let major = clang_major(PIN).expect("the pin's version");
    assert_eq!(major, "23");
    let shelf = "/h/.blaze/v2/toolchain/clang/5d3179db";
    let c = CInputs {
        shelf: Shelf { dir: shelf, clang_major: major },
        sysroot_kind: Sysroot::MacosSdk,
        sysroot: "/sdk",
        sysroot_extra_reads: &[],
        linux_clang_cfg: None,
        out_dir: "/o",
        package: "/p",
        seen: blaze_expand::seen::SeenRoot::Package,
        macos_min: "11.0",
    };
    let env = c_env(&c, alloc);
    let args = env.iter().find(|(k, _)| k.as_str() == "BINDGEN_EXTRA_CLANG_ARGS").map(|(_, v)| v.as_str());
    assert_eq!(args, Some("-isysroot /sdk -resource-dir /h/.blaze/v2/toolchain/clang/5d3179db/lib/clang/23"));
    assert_eq!(clang_major("{\"version\": \"x.1\"}"), Err(PinError::NoVersion));
    assert_eq!(clang_major("{\"version\": \"23\"}"), Err(PinError::NoVersion));
    assert_eq!(clang_major("{}"), Err(PinError::NoVersion));
}

#[test]
fn c_env_linux_omits_macos_flags_and_sets_ccc_override_options() {
    let alloc = blaze_own::testing::heap().alloc();
    let shelf = "/h/.blaze/v2/toolchain/clang/9b27cf26";
    let cfg = "/h/.blaze/v2/native-work/blaze/cfg/k/clang.cfg";
    let c = CInputs {
        shelf: Shelf { dir: shelf, clang_major: "23" },
        sysroot_kind: Sysroot::LinuxHost,
        sysroot: "/nix/store/abc-glibc-dev",
        sysroot_extra_reads: &["/nix/store/def-linux-headers"],
        linux_clang_cfg: Some(cfg),
        out_dir: "/h/work/out/k",
        package: "/h/work/pkg/k",
        seen: blaze_expand::seen::SeenRoot::Package,
        macos_min: "11.0",
    };
    let env = c_env(&c, alloc);
    let keys: std::vec::Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.is_sorted(), "c_env must remain sorted: {keys:?}");
    let get = |name: &str| env.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| v.as_str());
    assert_eq!(get("SDKROOT"), None, "Linux c_env must not emit macOS SDKROOT");
    assert_eq!(get("MACOSX_DEPLOYMENT_TARGET"), None, "Linux c_env must not emit MACOSX_DEPLOYMENT_TARGET");
    assert_eq!(
        get("CCC_OVERRIDE_OPTIONS"),
        Some("#^--config=/h/.blaze/v2/native-work/blaze/cfg/k/clang.cfg s/^-isysroot$/--sysroot/ s/^-isysroot(.+)/--sysroot=\\1/")
    );
    assert_eq!(
        get("BINDGEN_EXTRA_CLANG_ARGS"),
        Some("--sysroot=/nix/store/abc-glibc-dev -resource-dir /h/.blaze/v2/toolchain/clang/9b27cf26/lib/clang/23")
    );
    let cflags = get("CFLAGS").expect("CFLAGS");
    assert!(!cflags.contains("-isysroot"), "Linux CFLAGS must not contain -isysroot: {cflags}");
    assert!(!cflags.contains("/nix/store"), "Linux CFLAGS must be host-path-free: {cflags}");
    assert!(!cflags.contains("/h/work"), "Linux CFLAGS must be host-path-free: {cflags}");
    assert_eq!(cflags, get("CXXFLAGS").expect("CXXFLAGS"));

    // FHS `/usr` sysroot strips `/usr` -> `--sysroot=/` and omits `CCC_OVERRIDE_OPTIONS` when no cfg staged.
    let fhs = CInputs { sysroot: "/usr", linux_clang_cfg: None, ..c };
    let fhs_env = c_env(&fhs, alloc);
    let fhs_get = |name: &str| fhs_env.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| v.as_str());
    assert_eq!(fhs_get("CCC_OVERRIDE_OPTIONS"), None);
    assert_eq!(
        fhs_get("BINDGEN_EXTRA_CLANG_ARGS"),
        Some("--sysroot=/ -resource-dir /h/.blaze/v2/toolchain/clang/9b27cf26/lib/clang/23")
    );

    // Control: `Sysroot::MacosSdk` preserves macOS flags and `SDKROOT`/`MACOSX_DEPLOYMENT_TARGET`.
    let mac = CInputs { sysroot_kind: Sysroot::MacosSdk, sysroot: "/sdk", linux_clang_cfg: None, ..c };
    let mac_env = c_env(&mac, alloc);
    let mac_get = |name: &str| mac_env.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| v.as_str());
    assert_eq!(mac_get("SDKROOT"), Some("/sdk"));
    assert_eq!(mac_get("MACOSX_DEPLOYMENT_TARGET"), Some("11.0"));
    assert_eq!(mac_get("CCC_OVERRIDE_OPTIONS"), None);
    assert!(mac_get("CFLAGS").expect("CFLAGS").contains("-isysroot /sdk"));
}

/// D69 (3): native for build and run; check keeps wasm but for the five that give nothing so.
#[test]
fn the_mode_decides_the_runner() {
    let native = Runner::Native { because: UnitId(3) };
    let wasm = crate::native::Closure { runner: Runner::Wasm, no_wasm: None };
    let spawns = crate::native::Closure { runner: native, no_wasm: None };
    let no_wasm = crate::native::Closure { runner: native, no_wasm: Some(UnitId(3)) };
    assert!(NATIVE_EVERY_MODE.is_sorted());
    assert!(crate::native::NO_WASM_CLASS.is_sorted());
    assert!(crate::native::TOOLCHAIN_CLASS.is_sorted());
    for mode in [Mode::Check, Mode::Build, Mode::Run] {
        assert_eq!(placement(wasm, mode, "sha3-asm"), Runner::Wasm);
        assert_eq!(placement(spawns, mode, "sha3-asm"), native);
        // turso's pg_query: bindgen in its closure, native in every mode, by no name.
        assert_eq!(placement(no_wasm, mode, "pg_query"), native);
    }
    assert_eq!(placement(spawns, Mode::Check, "zstd-sys"), Runner::Wasm);
    assert_eq!(placement(spawns, Mode::Build, "zstd-sys"), native);
    assert_eq!(placement(spawns, Mode::Run, "zstd-sys"), native);
    // CONTROL (rule 23): pg_query's name with a closure that compiles to wasm checks as wasm:
    // the closure's class, not the name, placed it.
    assert_eq!(placement(spawns, Mode::Check, "pg_query"), Runner::Wasm);
    // N7: libusb1-sys checks as wasm (no sw_vers there; pkg-config from the pins) and builds
    // native (its C compile).
    assert_eq!(placement(spawns, Mode::Check, "libusb1-sys"), Runner::Wasm);
    assert_eq!(placement(spawns, Mode::Build, "libusb1-sys"), native);
}

#[test]
fn nix_sysroot_candidates_parses_isystem_and_idirafter_flags() {
    fn collect(flags: &str) -> std::vec::Vec<&str> {
        let mut out = std::vec::Vec::new();
        for_each_nix_sysroot_candidate(flags, |dir| out.push(dir));
        out
    }

    // Real NixOS `nix-support/libc-cflags` shape (trailing space + -fmacro-prefix-map ignored).
    let libc_cflags = "-idirafter /nix/store/2gh2-glibc-2.44-57-dev/include -fmacro-prefix-map=/nix/store/2gh2-glibc-2.44-57-dev/include=/nix/store/eeee-glibc-2.44-57-dev/include ";
    assert_eq!(collect(libc_cflags), ["/nix/store/2gh2-glibc-2.44-57-dev"]);

    // Joined, quoted, trailing-slash, and multiple flags in `NIX_CFLAGS_COMPILE`.
    let nix_cflags = r#"-O2 -isystem/nix/store/aaa-foo-dev/include -isystem "/nix/store/bbb-glibc-dev/include/" -idirafter '/nix/store/ccc-bar-dev/include' -idirafter/nix/store/ddd-baz-dev/include"#;
    assert_eq!(
        collect(nix_cflags),
        [
            "/nix/store/aaa-foo-dev",
            "/nix/store/bbb-glibc-dev",
            "/nix/store/ccc-bar-dev",
            "/nix/store/ddd-baz-dev",
        ]
    );

    // Negative controls: non-matching flags, relative paths, non-`/include` dirs, bare `/include`, dangling flag.
    let non_matching = "-I/nix/store/xxx/include -isystem relative/include -isystem /nix/store/xxx/not-include -isystem /nix/store/xxx/myinclude -idirafter /include -isystem";
    assert!(collect(non_matching).is_empty(), "non-matching flags must yield no candidates");
}

#[test]
fn nix_lib_candidates_parses_crt_and_library_flags() {
    fn collect(flags: &str) -> std::vec::Vec<&str> {
        let mut out = std::vec::Vec::new();
        for_each_nix_lib_candidate(flags, |dir| out.push(dir));
        out
    }

    assert_eq!(
        collect("-B/nix/store/nhph-glibc-2.44-57/lib/ "),
        ["/nix/store/nhph-glibc-2.44-57/lib"]
    );
    assert_eq!(
        collect(r#"-rpath /nix/store/x/lib -L /nix/store/a-glibc/lib/ -L"/nix/store/b-glibc/lib" -B '/nix/store/c-glibc/lib/'"#),
        ["/nix/store/a-glibc/lib", "/nix/store/b-glibc/lib", "/nix/store/c-glibc/lib"]
    );
    // Negative controls: relative paths, bare `/`, unrelated flags, dangling `-L`.
    assert!(collect("-I/nix/store/x/include -L relative/lib -B / -Wl,-rpath=/nix/store/x/lib -L").is_empty());
}

#[test]
fn nix_dynamic_linker_and_rpath_flags_parse_raw_ld_and_wl_forms() {
    fn rpaths(flags: &str) -> std::vec::Vec<&str> {
        let mut out = std::vec::Vec::new();
        for_each_nix_rpath_in_flags(flags, |dir| out.push(dir));
        out
    }

    assert_eq!(
        nix_dynamic_linker_in_flags("-dynamic-linker /nix/store/g-glibc/lib/ld-linux-aarch64.so.1 -rpath /nix/store/g-glibc/lib"),
        Some("/nix/store/g-glibc/lib/ld-linux-aarch64.so.1")
    );
    assert_eq!(
        nix_dynamic_linker_in_flags("-Wl,--dynamic-linker=/nix/store/g-glibc/lib/ld-linux-x86-64.so.2"),
        Some("/nix/store/g-glibc/lib/ld-linux-x86-64.so.2")
    );
    assert_eq!(
        nix_dynamic_linker_in_flags("-Wl,-dynamic-linker,/nix/store/g-glibc/lib/ld-linux-aarch64.so.1"),
        Some("/nix/store/g-glibc/lib/ld-linux-aarch64.so.1")
    );
    assert_eq!(nix_dynamic_linker_in_flags("-dynamic-linker relative/ld.so -L/nix/store/g/lib"), None);

    assert_eq!(
        rpaths("-rpath /nix/store/a/lib/ -Wl,-rpath,/nix/store/b/lib:/nix/store/c/lib -R/nix/store/d/lib --rpath=/nix/store/e/lib"),
        ["/nix/store/a/lib", "/nix/store/b/lib", "/nix/store/c/lib", "/nix/store/d/lib", "/nix/store/e/lib"]
    );
    assert!(rpaths("-rpath relative/lib -Wl,-rpath,/ -L/nix/store/a/lib").is_empty());
}
