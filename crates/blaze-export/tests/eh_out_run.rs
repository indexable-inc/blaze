//! An exported file unwinds on this Mac: `thrower` raises through libSystem's
//! `_Unwind_RaiseException`, the system unwinder walks out of `thrower` by its compact frame
//! encoding, finds `main`'s FDE through `__unwind_info`'s DWARF mode, calls the personality
//! routine its CIE names with the LSDA its FDE names, and the routine lands at the landing that
//! LSDA's one call site gives: `main` then exits 42. A wrong FDE, a wrong personality pointer
//! or a wrong LSDA pointer ends the run another way (an abort from the unwinder, or `brk`).
//!
//! The personality here is a hand-written routine of the Itanium C++ ABI's form
//! (`_Unwind_Reason_Code (*)(int version, _Unwind_Action actions, uint64_t class,
//! _Unwind_Exception *, _Unwind_Context *)`, "Level II: C++ ABI", 2.5.2): in the search phase
//! (`_UA_SEARCH_PHASE`, 1) it answers `_URC_HANDLER_FOUND` (6); in the cleanup phase it reads
//! the first call site's landing pad from the LSDA (`_Unwind_GetLanguageSpecificData`, at
//! offset 12: the start encoding, the type encoding, the call-site encoding, the table's
//! length, then `cs_start`, `cs_len`, `cs_lpad`, as the pinned library's `find_eh_action` reads
//! them), adds `_Unwind_GetRegionStart`, sets the IP there and answers
//! `_URC_INSTALL_CONTEXT` (7). std's `rust_eh_personality` takes this place in a Rust program;
//! the tables it reads are the same.
//!
//! Ignored: it runs the exported file. The integrator runs it by name on a Mac:
//! `cargo test -p blaze-export --test eh_out_run -- --ignored`.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target};
use blaze_code::unwind::{self, Action, Frame, Row};
use blaze_export::{Entry, Function, Panic, Platform, ProgramIn, Tier, export};
use blaze_own::{AVec, Alloc, Vec};

const STP: u32 = 0xA9BF_7BFD; // stp x29, x30, [sp, #-16]!
const MOV_FP: u32 = 0x9100_03FD; // mov x29, sp
const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const RET: u32 = 0xD65F_03C0;

fn sym(name: &str) -> Symbol {
    Symbol::of_name(name)
}

fn rec(code: &[u32], sites: &[(u32, SiteKind, Target)], outside: &[(u32, &str)], frame: Option<Frame>, rows: &[Row]) -> AVec<'static, u8> {
    let alloc = Alloc::global();
    let mut r = CodeRecord::new_in(alloc);
    for &inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    for &(at, kind, target) in sites {
        let index = r.target(target);
        r.sites.push(Site { at, kind, target: index, addend: 0 });
    }
    for &(at, name) in outside {
        let index = r.outside(name);
        r.sites.push(Site { at, kind: SiteKind::Call, target: index, addend: 0 });
    }
    if let Some(frame) = frame {
        unwind::fill(&mut r, frame, rows).unwrap();
    }
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

fn records() -> [AVec<'static, u8>; 3] {
    // main: call thrower (row: cleanup, landing 28); the normal path exits 1, the landing 42.
    let main = rec(
        &[
            STP, MOV_FP, // 0, 4
            ADRP_X16, LDR_X16, BLR_X16,     // 8: thrower (nop; nop; bl after layout)
            0x5280_0020, // 20: mov w0, #1
            0x1400_0002, // 24: b 32
            0x5280_0540, // 28: mov w0, #42 (the landing)
            ADRP_X16, LDR_X16, BLR_X16, // 32: exit
        ],
        &[(8, SiteKind::Call, Target { space: Space::Func, symbol: sym("thrower") })],
        &[(32, "exit")],
        Some(Frame::PLAIN),
        &[Row { start: 8, len: 12, landing: 28, action: Action::Cleanup }],
    );
    // thrower: a zeroed 32-byte _Unwind_Exception on the stack, raised; `brk` if it returns.
    let thrower = rec(
        &[
            STP, MOV_FP,     // 0, 4
            0xD100_83FF, // 8: sub sp, sp, #32
            0xA900_7FFF, // 12: stp xzr, xzr, [sp]
            0xA901_7FFF, // 16: stp xzr, xzr, [sp, #16]
            0x9100_03E0, // 20: mov x0, sp
            ADRP_X16, LDR_X16, BLR_X16, // 24: _Unwind_RaiseException
            0xD420_0000, // 36: brk #0
        ],
        &[],
        &[(24, "_Unwind_RaiseException")],
        Some(Frame::PLAIN),
        &[],
    );
    let personality = rec(
        &[
            0x1200_0029, // 0: and w9, w1, #1 (_UA_SEARCH_PHASE)
            0x3500_02E9, // 4: cbnz w9, 96
            0xA9BE_7BFD, // 8: stp x29, x30, [sp, #-32]!
            0x9100_03FD, // 12: mov x29, sp
            0xA901_53F3, // 16: stp x19, x20, [sp, #16]
            0xAA04_03F3, // 20: mov x19, x4 (the context)
            0xAA04_03E0, // 24: mov x0, x4
            ADRP_X16, LDR_X16, BLR_X16, // 28: _Unwind_GetLanguageSpecificData
            0xB940_0C14, // 40: ldr w20, [x0, #12] (cs_lpad of the first call site)
            0xAA13_03E0, // 44: mov x0, x19
            ADRP_X16, LDR_X16, BLR_X16, // 48: _Unwind_GetRegionStart
            0x8B14_0001, // 60: add x1, x0, x20
            0xAA13_03E0, // 64: mov x0, x19
            ADRP_X16, LDR_X16, BLR_X16, // 68: _Unwind_SetIP
            0x5280_00E0, // 80: mov w0, #7 (_URC_INSTALL_CONTEXT)
            0xA941_53F3, // 84: ldp x19, x20, [sp, #16]
            0xA8C2_7BFD, // 88: ldp x29, x30, [sp], #32
            RET,         // 92
            0x5280_00C0, // 96: mov w0, #6 (_URC_HANDLER_FOUND)
            RET,         // 100
        ],
        &[],
        &[(28, "_Unwind_GetLanguageSpecificData"), (48, "_Unwind_GetRegionStart"), (68, "_Unwind_SetIP")],
        None,
        &[],
    );
    [main, thrower, personality]
}

/// Prints `eh_out: the exported program unwound from thrower into main's landing and exited 42`.
#[test]
#[ignore = "runs the exported file; the integrator runs it by name on an arm64 Mac"]
fn eh_out_exported_program_lands_and_exits_42() {
    let records = records();
    let names = ["main", "thrower", "personality"];
    let functions: [Function<'_>; 3] = [0, 1, 2].map(|k| Function { symbol: sym(names[k]), record: CodeView::read(&records[k]).unwrap(), tier: Tier::Fast });
    let program = ProgramIn {
        name: "eh-out-run",
        functions: &functions,
        data: &[],
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: Some(sym("personality")),
        profile: None,
        natives: None,
        exports: &[],
    };
    let mut file = Vec::new_in(Alloc::global());
    export(&program, Platform::MacosArm64, &mut file, Alloc::global()).unwrap();
    let path = std::env::temp_dir().join(format!("blaze-export-eh-out-run-{}", std::process::id()));
    std::fs::write(&path, &file[..]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = std::process::Command::new(&path).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("the exported program at {} did not exit within 20 s: a hang is a bug", path.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    std::fs::remove_file(&path).unwrap();
    assert_eq!(status.code(), Some(42), "the exported program ended with {status:?}, not by main's landing");
    println!("eh_out: the exported program unwound from thrower into main's landing and exited 42");
}
