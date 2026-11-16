//! The `blaze` binary links libSystem and nothing else, read from its Mach-O load commands: a
//! library linked is one dyld loads and initialises at every start, so a build tool that links a
//! framework it calls on one path pays for it on all of them. A framework wanted on some paths
//! is loaded at first use instead (std.md, "What a program pays for at start"; blaze-core's
//! FSEvents driver is the pattern), and `build.rs` drops what nothing is bound to.
//!
//! macOS only: on Linux the same rule would read the ELF `DT_NEEDED` entries; not yet written.

#![cfg(target_os = "macos")]

/// What the binary may link: the C library, the kernel's calls and libdispatch, which every
/// macOS program links.
const ALLOWED: &[&str] = &["/usr/lib/libSystem.B.dylib"];

const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_REQ_DYLD: u32 = 0x8000_0000;
/// Every load command that names a library dyld loads with the binary.
const LOADS: &[u32] = &[
    0x0c,                // LC_LOAD_DYLIB
    0x18 | LC_REQ_DYLD,  // LC_LOAD_WEAK_DYLIB
    0x1f | LC_REQ_DYLD,  // LC_REEXPORT_DYLIB
    0x20,                // LC_LAZY_LOAD_DYLIB
    0x23 | LC_REQ_DYLD,  // LC_LOAD_UPWARD_DYLIB
];

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let word: [u8; 4] = bytes[at..at + 4].try_into().expect("four bytes");
    u32::from_le_bytes(word)
}

/// The paths of every library the Mach-O image `bytes` loads, in load-command order.
fn libraries(bytes: &[u8]) -> Vec<String> {
    assert_eq!(u32_at(bytes, 0), MH_MAGIC_64, "a thin 64-bit little-endian Mach-O image");
    let commands = u32_at(bytes, 16) as usize;
    // `mach_header_64` is 32 bytes; the commands follow it.
    let mut at = 32;
    let mut found = Vec::new();
    for _ in 0..commands {
        let (cmd, size) = (u32_at(bytes, at), u32_at(bytes, at + 4) as usize);
        if LOADS.contains(&cmd) {
            // `dylib_command`: cmd, cmdsize, then `dylib.name`, an offset from the command's
            // start to a NUL-terminated path inside it.
            let name = at + u32_at(bytes, at + 8) as usize;
            let end = bytes[name..at + size].iter().position(|b| *b == 0).expect("a NUL-terminated path");
            found.push(String::from_utf8_lossy(&bytes[name..name + end]).into_owned());
        }
        at += size;
    }
    found
}

#[test]
fn blaze_links_only_libsystem() {
    let exe = env!("CARGO_BIN_EXE_blaze");
    let bytes = std::fs::read(exe).unwrap_or_else(|error| panic!("reading {exe}: {error}"));
    let linked = libraries(&bytes);
    assert!(linked.iter().any(|it| it == ALLOWED[0]), "{exe} links no libSystem: {linked:?}");
    let extra: Vec<&String> = linked.iter().filter(|it| !ALLOWED.contains(&it.as_str())).collect();
    assert!(
        extra.is_empty(),
        "{exe} links {extra:?} beyond {ALLOWED:?}: each is loaded and initialised at every start. Load it at first use (std.md, \"What a program pays for at start\") or put the code that needs it behind a feature blaze-cli does not turn on"
    );
}
