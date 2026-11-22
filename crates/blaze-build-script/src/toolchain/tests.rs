use blaze_own::{AString, AVec, Alloc, Vec};
use blaze_pins::CLANG_JSON as PIN;
use blaze_unit_graph::UnitId;

use super::{
    CInputs, ClangPin, Mode, NATIVE_EVERY_MODE, PinError, Shelf, Sysroot, c_env, clang_major, clang_pin, depfile_reads, headers_digest, placement,
    sysroot,
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

/// macOS takes the machine's SDK; Linux's sysroot row is there and empty: a typed refusal.
#[test]
fn linux_has_no_sysroot_pinned_yet() {
    assert_eq!(sysroot(PIN, "aarch64-apple-darwin"), Ok(Sysroot::MacosSdk));
    let refused = sysroot(PIN, "aarch64-unknown-linux-gnu").expect_err("no sysroot is pinned");
    assert_eq!(refused, PinError::NoSysroot { target: "aarch64-unknown-linux-gnu" });
    assert!(refused.to_string().contains("no sysroot pinned for aarch64-unknown-linux-gnu"), "{refused}");
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
        sysroot: "/sdk",
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
