//! The start of an exported file: what the platform's loader calls, made as code records of the
//! same form as every function's, so the layout places and completes them like any other.
//!
//! - macOS: dyld calls `LC_MAIN`'s entry as `int main(int argc, char **argv, char **envp, char
//!   **apple)` and passes what it returns to `exit` (`<mach-o/loader.h>`, `entry_point_command`).
//!   A `CMain` program's `main` is that entry itself; the other forms get a wrapper.
//! - Linux: the kernel and `ld-linux-aarch64.so.1` jump to `e_entry` with `sp` at `argc` and the
//!   loader's finalizer in `x0` (the AArch64 ELF ABI's process start). `_start` hands glibc's
//!   `__libc_start_main(main, argc, argv, init, fini, rtld_fini, stack_end)` the C-style main:
//!   `main` itself for `CMain`, else the same wrapper as macOS's. Its order of arguments is
//!   glibc's sysdeps/aarch64/start.S's (to verify by the Linux run test).
//!
//! The wrapper for `RustMain` calls `main` and returns 0; for `LangStart` it calls
//! `lang_start(main, argc, argv, sigpipe)` and returns its value. `sigpipe` is 0, the library's
//! `sigpipe::DEFAULT` (to verify against the pinned library's library/std/src/rt.rs and
//! sys/pal/unix: the value the compiler passes when no `#[unix_sigpipe]` is written).

use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target};
use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, Vec};

use crate::error::ExportError;
use crate::input::{Entry, Platform};

const START_DOMAIN: &str = "blaze-export start 1";

const STP_FP_LR: u32 = 0xA9BF_7BFD; // stp x29, x30, [sp, #-16]!
const MOV_FP_SP: u32 = 0x9100_03FD; // mov x29, sp
const LDP_FP_LR: u32 = 0xA8C1_7BFD; // ldp x29, x30, [sp], #16
const RET: u32 = 0xD65F_03C0;
const MOV_W0_0: u32 = 0x5280_0000; // mov w0, #0
const MOV_W3_0: u32 = 0x5280_0003; // mov w3, #0
const MOV_X2_X1: u32 = 0xAA01_03E2; // mov x2, x1
const MOV_X1_X0: u32 = 0xAA00_03E1; // mov x1, x0
const ADRP_X0: u32 = 0x9000_0000; // adrp x0, _
const LDR_X0: u32 = 0xF940_0000; // ldr x0, [x0, _]
const ADRP_X16: u32 = 0x9000_0010; // adrp x16, _
const LDR_X16: u32 = 0xF940_0210; // ldr x16, [x16, _]
const BLR_X16: u32 = 0xD63F_0200; // blr x16
const MOV_X29_0: u32 = 0xD280_001D; // mov x29, #0
const MOV_X30_0: u32 = 0xD280_001E; // mov x30, #0
const MOV_X5_X0: u32 = 0xAA00_03E5; // mov x5, x0 (the loader's finalizer)
const LDR_X1_SP: u32 = 0xF940_03E1; // ldr x1, [sp] (argc)
const ADD_X2_SP_8: u32 = 0x9100_23E2; // add x2, sp, #8 (argv)
const MOV_X6_SP: u32 = 0x9100_03E6; // mov x6, sp (stack_end)
const MOV_X3_0: u32 = 0xD280_0003; // mov x3, #0 (init: none)
const MOV_X4_0: u32 = 0xD280_0004; // mov x4, #0 (fini: none)
const BRK_0: u32 = 0xD420_0000; // brk #0: `__libc_start_main` never returns

/// The start's records, encoded, and the one the loader enters.
pub struct Start<'a> {
    /// Each made record's symbol and encoded bytes, read with [`CodeView::read`].
    pub records: AVec<'a, (Symbol, AVec<'a, u8>)>,
    /// What the file's entry is: a made record's symbol, or the program's `main` itself.
    pub entry: Symbol,
}

impl<'a> Start<'a> {
    /// The made records as functions to place, read in place.
    pub fn views(&self) -> impl Iterator<Item = (Symbol, CodeView<'_>)> {
        self.records.iter().map(|(symbol, bytes)| match CodeView::read(bytes) {
            Ok(view) => (*symbol, view),
            Err(_) => unreachable!("a start record was sealed and encoded by `start` just now"),
        })
    }
}

/// The symbol of a made start record: its own domain, so no program symbol can equal it.
pub fn start_symbol(what: &str) -> Symbol {
    Symbol(Hasher::new(START_DOMAIN).update(what.as_bytes()).finish().0)
}

fn func(symbol: Symbol) -> Target {
    Target { space: Space::Func, symbol }
}

/// Code of `insts` with sites `(at, kind, target)`, sealed and encoded.
fn made<'a>(insts: &[u32], sites: &[(u32, SiteKind, Target)], outside: Option<(u32, &str)>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, ExportError> {
    let mut r = CodeRecord::new_in(alloc);
    for &inst in insts {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    for &(at, kind, target) in sites {
        let index = r.target(target);
        r.sites.push(Site { at, kind, target: index, addend: 0 });
    }
    if let Some((at, name)) = outside {
        let index = r.outside(name);
        r.sites.push(Site { at, kind: SiteKind::Call, target: index, addend: 0 });
    }
    let sealed = r.seal().map_err(ExportError::StartRecord)?;
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    Ok(out)
}

/// The wrapper with dyld's (and `__libc_start_main`'s) C main signature, for the forms that need
/// one; `None` for `CMain`, whose `main` is that already.
fn wrapper<'a>(entry: Entry, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, ExportError> {
    match entry {
        Entry::CMain { .. } => Ok(None),
        Entry::Hosted { start } => Err(ExportError::Hosted { start }),
        Entry::RustMain { main } => {
            let code = [STP_FP_LR, MOV_FP_SP, ADRP_X16, LDR_X16, BLR_X16, MOV_W0_0, LDP_FP_LR, RET];
            made(&code, &[(8, SiteKind::Call, func(main))], None, alloc).map(Some)
        }
        Entry::LangStart { lang_start, main } => {
            let code = [
                STP_FP_LR, MOV_FP_SP, MOV_X2_X1, MOV_X1_X0, ADRP_X0, LDR_X0, MOV_W3_0, ADRP_X16, LDR_X16, BLR_X16, LDP_FP_LR, RET,
            ];
            let sites = [(16, SiteKind::Load, func(main)), (28, SiteKind::Call, func(lang_start))];
            made(&code, &sites, None, alloc).map(Some)
        }
    }
}

/// The program's `main`, by entry form.
pub fn main_of(entry: Entry) -> Symbol {
    match entry {
        Entry::RustMain { main } | Entry::CMain { main } | Entry::LangStart { main, .. } => main,
        Entry::Hosted { start } => start,
    }
}

/// The start records `entry` needs on `platform`.
pub fn start<'a>(entry: Entry, platform: Platform, alloc: Alloc<'a>) -> Result<Start<'a>, ExportError> {
    let mut records: AVec<'a, (Symbol, AVec<'a, u8>)> = Vec::new_in(alloc);
    let wrapper_symbol = start_symbol("wrapper");
    let c_main = match wrapper(entry, alloc)? {
        Some(bytes) => {
            records.push((wrapper_symbol, bytes));
            wrapper_symbol
        }
        None => main_of(entry),
    };
    let entry = match platform {
        Platform::MacosArm64 => c_main,
        Platform::LinuxArm64 => {
            let code = [
                MOV_X29_0, MOV_X30_0, MOV_X5_X0, LDR_X1_SP, ADD_X2_SP_8, MOV_X6_SP, ADRP_X0, LDR_X0, MOV_X3_0, MOV_X4_0, ADRP_X16,
                LDR_X16, BLR_X16, BRK_0,
            ];
            let bytes = made(&code, &[(24, SiteKind::Load, func(c_main))], Some((40, "__libc_start_main")), alloc)?;
            let symbol = start_symbol("_start");
            records.push((symbol, bytes));
            symbol
        }
    };
    Ok(Start { records, entry })
}
