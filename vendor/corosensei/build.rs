//! Special build-time detection for ARM targets.
//!
//! We need to determine some properties of the target for inline assembly that
//! are not otherwise easily available. Specifically:
//! - Does the target use the ARM or Thumb instruction set by default?
//! - Does the target support Thumb2?
//! - Does the target use R7 or R11 for its frame pointer?
//! - Does the target treat R9 as a reserved register?

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    assemble_trampolines();

    // We only care about ARM.
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if arch != "arm" {
        return;
    }

    // Probe some registers using asm! to see if they are reserved.
    let cfg = autocfg::new();
    let r7_available = cfg.probe_expression("unsafe { core::arch::asm!(\"\", out(\"r7\") _) }");
    let r9_available = cfg.probe_expression("unsafe { core::arch::asm!(\"\", out(\"r9\") _) }");
    let r11_available = cfg.probe_expression("unsafe { core::arch::asm!(\"\", out(\"r11\") _) }");

    if !r9_available {
        autocfg::emit("r9_reserved");
    }

    match (r7_available, r11_available) {
        (true, false) => {}
        (false, true) => autocfg::emit("fp_is_r7"),
        _ => panic!("could not determine whether frame pointer is r7 or r11"),
    }

    // The most reliable way is to check for the thumb-mode feature in
    // CARGO_CFG_TARGET_FEATURE but this is only available on nightly. As a
    // fallback we just check if the target name starts with "thumb".
    let (is_thumb, has_thumb2) = if let Ok(target_features) = env::var("CARGO_CFG_TARGET_FEATURE") {
        (
            target_features.split(',').any(|s| s == "thumb-mode"),
            target_features.split(',').any(|s| s == "thumb2"),
        )
    } else {
        // thumbv6m-none-eabi is the only thumb target that doesn't support
        // thumb2.
        let target = env::var("TARGET").unwrap();
        (target.starts_with("thumb"), !target.starts_with("thumbv6"))
    };
    if is_thumb {
        autocfg::emit("is_thumb");
    }
    if has_thumb2 {
        autocfg::emit("has_thumb2");
    }
}

/// blaze (SOURCE.md): the stack trampolines of src/arch/aarch64.rs and src/arch/x86_64.rs, which
/// upstream writes with `global_asm!`, assembled from src/arch/asm/ instead, because the pinned
/// stage-0 blaze predates `global_asm!` and drops it (blaze issue #1659). These are the targets
/// src/arch/mod.rs sends to those two files (x86_64 and aarch64, not Windows); UEFI keeps the
/// `global_asm!` (its SEH directives), so it is left out here as there.
fn assemble_trampolines() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let vendor = env::var("CARGO_CFG_TARGET_VENDOR").unwrap();
    let family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let windows = family.split(',').any(|f| f == "windows");
    if windows || os == "uefi" || !(arch == "x86_64" || arch == "aarch64") {
        return;
    }
    // Mach-O on Apple targets, ELF everywhere else, as src/arch/mod.rs's asm_function_begin!.
    let format = if vendor == "apple" { "macho" } else { "elf" };
    let file = format!("src/arch/asm/{arch}-{format}.S");
    println!("cargo:rerun-if-changed={file}");
    cc::Build::new().file(&file).compile("corosensei_trampolines");
}
