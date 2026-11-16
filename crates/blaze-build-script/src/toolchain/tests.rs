use blaze_own::{AString, AVec, Alloc, Vec};
use blaze_unit_graph::UnitId;

use super::{ClangPin, Mode, NATIVE_EVERY_MODE, PIN, PinError, Sysroot, clang_pin, depfile_reads, headers_digest, placement, sysroot};
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

/// D69 (3): native for build and run; check keeps wasm but for the five that give nothing so.
#[test]
fn the_mode_decides_the_runner() {
    let native = Runner::Native { because: UnitId(3) };
    assert!(NATIVE_EVERY_MODE.is_sorted());
    for mode in [Mode::Check, Mode::Build, Mode::Run] {
        assert_eq!(placement(Runner::Wasm, mode, "sha3-asm"), Runner::Wasm);
        assert_eq!(placement(native, mode, "sha3-asm"), native);
    }
    assert_eq!(placement(native, Mode::Check, "zstd-sys"), Runner::Wasm);
    assert_eq!(placement(native, Mode::Build, "zstd-sys"), native);
    assert_eq!(placement(native, Mode::Run, "zstd-sys"), native);
}
