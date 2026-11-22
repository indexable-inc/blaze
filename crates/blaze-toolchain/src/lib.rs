//! The toolchain as records (docs/v2/plan/design/toolchain.md).
//!
//! - [`record`]: the manifest record (`BLZTCH02`): every toolchain crate with its source pack's
//!   digest and length, and per target the sysroot's units as blaze-unit-graph made them. Its
//!   footer, the BLAKE3 of its bytes, is the [`ToolchainId`].
//! - [`PINNED`]: the id this build of blaze opens, `PINNED`'s 64 hex digits read at compile
//!   time (the tool writes that file; it is data, not Rust).
//! - [`shelf`]: opening the manifest and a pack from the per-user shelf through the [`Shelf`]
//!   trait the driver implements: sizes held to their limits before a byte is read, digests
//!   checked once, a typed [`ToolchainError`] naming the digest and the folder looked in.
//!
//! The tool that cuts a toolchain from the tree is the script `scripts/src/bin/toolchain_cut.rs`.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]


extern crate alloc;
pub mod implied;
pub mod record;
pub mod shelf;
pub mod side;

#[cfg(test)]
mod tests;

use core::fmt;

pub use implied::{ImplicitSysroot, RootFacts, SysrootUnits};
pub use record::{CrateRow, Role, ToolchainView, UnitKind, UnitRow};
pub use shelf::{Shelf, ToolchainError};
pub use side::{MACRO_TRIPLE, MacroSide, Side, SideName, SideSysroot, SideTriple, TargetSide};

/// A toolchain's identity: the BLAKE3 of its manifest record's bytes before the footer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ToolchainId(pub [u8; 32]);

impl ToolchainId {
    /// The 64 hex digits a shelf file is named by.
    pub fn hex(&self) -> Hex {
        Hex::of(&self.0)
    }
}

impl fmt::Display for ToolchainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.hex().as_str())
    }
}

/// The toolchain's library units for `triple` on one side (`for_host`: what build scripts and
/// proc macros are given), found by their crates' roles; `None` when the manifest has no graph
/// for the triple or lacks one of the five.
pub fn sysroot(view: &ToolchainView<'_>, triple: &str, for_host: bool) -> Option<blaze_unit_graph::Sysroot> {
    use blaze_unit_graph::ToolchainUnit;
    let mut found = [None; 5];
    for (at, unit) in view.units(triple)?.enumerate() {
        if unit.kind != UnitKind::Lib || unit.for_host != for_host {
            continue;
        }
        let slot = match view.krate(unit.krate).role {
            Role::Core => 0,
            Role::Alloc => 1,
            Role::Std => 2,
            Role::ProcMacro => 3,
            Role::Bstd => 4,
            Role::Library | Role::Blaze | Role::Registry => continue,
        };
        found[slot].get_or_insert(ToolchainUnit(at as u32));
    }
    Some(blaze_unit_graph::Sysroot { core: found[0]?, alloc: found[1]?, std: found[2]?, proc_macro: found[3]?, bstd: found[4]? })
}

/// 32 bytes as 64 lower-case hex digits, on the stack.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Hex([u8; 64]);

impl Hex {
    pub fn of(bytes: &[u8; 32]) -> Hex {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 64];
        for (at, byte) in bytes.iter().enumerate() {
            out[at * 2] = DIGITS[usize::from(byte >> 4)];
            out[at * 2 + 1] = DIGITS[usize::from(byte & 15)];
        }
        Hex(out)
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).expect("hex digits are ASCII")
    }
}

impl fmt::Display for Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 64 hex digits, then an optional line feed, as 32 bytes; `None` for anything else.
pub const fn parse_hex(text: &[u8]) -> Option<[u8; 32]> {
    let len = match text {
        [.., b'\n'] => text.len() - 1,
        _ => text.len(),
    };
    if len != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let mut at = 0;
    while at < 32 {
        let (Some(high), Some(low)) = (digit(text[at * 2]), digit(text[at * 2 + 1])) else { return None };
        out[at] = (high << 4) | low;
        at += 1;
    }
    Some(out)
}

const fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// The toolchain this build of blaze opens: `crates/blaze-toolchain/PINNED`, which the tool
/// writes when it cuts the toolchain from this tree. A file that is not 64 lower-case hex
/// digits stops the build here.
pub const PINNED: ToolchainId = match parse_hex(include_bytes!("../PINNED")) {
    Some(id) => ToolchainId(id),
    None => panic!("crates/blaze-toolchain/PINNED is not 64 lower-case hex digits"),
};
