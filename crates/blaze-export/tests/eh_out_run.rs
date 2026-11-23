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
use blaze_export::{
    Data, DataPlace, Entry, Function, Function02, Panic, Platform, ProgramIn, ProgramIn02, Tier, TlsRuntime, export, export_optimized, v1_to_v2,
};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::record::{Declare, DeclaredKind, Declares, read};

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

fn run_exported(file: &[u8], name: &str) -> Option<i32> {
    let path = std::env::temp_dir().join(format!("blaze-export-{name}-{}", std::process::id()));
    std::fs::write(&path, file).unwrap();
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
    status.code()
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
        tls: None,
        profile: None,
        natives: None,
        exports: &[],
    };
    let mut file = Vec::new_in(Alloc::global());
    export(&program, Platform::MacosArm64, &mut file, Alloc::global()).unwrap();
    let code = run_exported(&file[..], "eh-out-run");
    assert_eq!(code, Some(42), "the exported program ended with {code:?}, not by main's landing");
    println!("eh_out: the exported program unwound from thrower into main's landing and exited 42");
}

/// Builds a `fn() -> *mut u64` using the exact `blaze-emit/src/pass/tls_index.rs` macOS arm64
/// sequence: inline fast path via `tpidrro_el0 & ~7` + `DESC.root`, slow path calling `getter`.
fn tls_addr_rec(desc: Symbol, getter: Symbol, tls_var: Target, alloc: Alloc<'static>) -> AVec<'static, u8> {
    let code = [
        STP,          // 0: stp x29, x30, [sp, #-16]!
        MOV_FP,       // 4: mov x29, sp
        0x9000_0009,  // 8: adrp x9, DESC
        0xF940_0129,  // 12: ldr x9, [x9]
        0xF940_052A,  // 16: ldr x10, [x9, #8] (DESC.root = key * 8)
        0x9240_F94A,  // 20: and x10, x10, #~(1 << 63)
        0xD53B_D06B,  // 24: mrs x11, tpidrro_el0
        0x927D_F16B,  // 28: and x11, x11, #~7
        0x8B0A_016B,  // 32: add x11, x11, x10 (root_at)
        0xF940_016C,  // 36: ldr x12, [x11] (table)
        0x9000_000D,  // 40: adrp x13, TLS
        0xF940_01AD,  // 44: ldr x13, [x13] (index)
        0xB400_00EC,  // 48: cbz x12, +28 (76: slow)
        0xF940_018E,  // 52: ldr x14, [x12] (table.len)
        0xEB0E_01BF,  // 56: cmp x13, x14
        0x5400_0082,  // 60: b.hs +16 (76: slow)
        0x9100_818E,  // 64: add x14, x12, #32 (table.slots)
        0xF86D_79C0,  // 68: ldr x0, [x14, x13, lsl #3]
        0xB500_00E0,  // 72: cbnz x0, +28 (100: done)
        0xAA09_03E0,  // 76: mov x0, x9 (desc)
        0xAA0D_03E1,  // 80: mov x1, x13 (index)
        0xAA0B_03E2,  // 84: mov x2, x11 (root_at)
        ADRP_X16,     // 88: adrp x16, GETTER
        LDR_X16,      // 92: ldr x16, [x16]
        BLR_X16,      // 96: blr x16
        0xA8C1_7BFD,  // 100: ldp x29, x30, [sp], #16
        RET,          // 104: ret
    ];
    let mut r = CodeRecord::new_in(alloc);
    for inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    let i_desc = r.target(Target { space: Space::Static, symbol: desc });
    let i_tls = r.target(tls_var);
    let i_getter = r.target(Target { space: Space::Func, symbol: getter });
    r.sites.push(Site { at: 8, kind: SiteKind::Load, target: i_desc, addend: 0 });
    r.sites.push(Site { at: 40, kind: SiteKind::Load, target: i_tls, addend: 0 });
    r.sites.push(Site { at: 88, kind: SiteKind::Load, target: i_getter, addend: 0 });
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

/// Real AArch64 TLS getter (`fn(desc, index, root_at) -> *mut u8`) implementing `blaze_code::tls`
/// table allocation + `VarRow` copy/zero-fill via libSystem (`pthread_getspecific`,
/// `pthread_setspecific`, `calloc`, `aligned_alloc`, `memset`, `memcpy`).
fn tls_getter_rec(alloc: Alloc<'static>) -> AVec<'static, u8> {
    let code = [
        0xA9BC_7BFD, // 0: stp x29, x30, [sp, #-64]!
        MOV_FP,      // 4: mov x29, sp
        0xA901_53F3, // 8: stp x19, x20, [sp, #16]
        0xA902_5BF5, // 12: stp x21, x22, [sp, #32]
        0xA903_63F7, // 16: stp x23, x24, [sp, #48]
        0xAA00_03F3, // 20: mov x19, x0 (desc)
        0xAA01_03F4, // 24: mov x20, x1 (index)
        0xF940_0260, // 28: ldr x0, [x19] (desc.key)
        ADRP_X16,    // 32: pthread_getspecific
        LDR_X16,     // 36
        BLR_X16,     // 40
        0xAA00_03F5, // 44: mov x21, x0 (table)
        0xB500_0195, // 48: cbnz x21, +48 (96: have_table)
        0xD280_0020, // 52: mov x0, #1
        0xD280_1401, // 56: mov x1, #160 (32 header + 16*8 slots)
        ADRP_X16,    // 60: calloc
        LDR_X16,     // 64
        BLR_X16,     // 68
        0xAA00_03F5, // 72: mov x21, x0 (table)
        0xD280_0209, // 76: mov x9, #16
        0xF900_02A9, // 80: str x9, [x21] (table.len = 16)
        0xF900_0AB3, // 84: str x19, [x21, #16] (table.desc = desc)
        0xF940_0260, // 88: ldr x0, [x19] (desc.key)
        0xAA15_03E1, // 92: mov x1, x21
        ADRP_X16,    // 96: wait -> shift have_table!
        LDR_X16,     // 100
        BLR_X16,     // 104
        // have_table (108):
        0xF940_1276, // 108: ldr x22, [x19, #32] (desc.vars)
        0x8B14_16D6, // 112: add x22, x22, x20, lsl #5 (&vars[index])
        0xD280_0200, // 116: mov x0, #16 (align)
        0xD280_0201, // 120: mov x1, #16 (size >= 16)
        ADRP_X16,    // 124: aligned_alloc
        LDR_X16,     // 128
        BLR_X16,     // 132
        0xAA00_03F7, // 136: mov x23, x0 (copy)
        0xAA17_03E0, // 140: mov x0, x23
        0x5280_0001, // 144: mov w1, #0
        0xF940_02C2, // 148: ldr x2, [x22] (row.size)
        ADRP_X16,    // 152: memset
        LDR_X16,     // 156
        BLR_X16,     // 160
        0xAA17_03E0, // 164: mov x0, x23
        0xF940_0AC1, // 168: ldr x1, [x22, #16] (row.init)
        0xF940_0EC2, // 172: ldr x2, [x22, #24] (row.init_len)
        ADRP_X16,    // 176: memcpy
        LDR_X16,     // 180
        BLR_X16,     // 184
        0x9100_82A9, // 188: add x9, x21, #32 (table.slots)
        0xF834_7937, // 192: str x23, [x9, x20, lsl #3]
        0xAA17_03E0, // 196: mov x0, x23
        0xA943_63F7, // 200: ldp x23, x24, [sp, #48]
        0xA942_5BF5, // 204: ldp x21, x22, [sp, #32]
        0xA941_53F3, // 208: ldp x19, x20, [sp, #16]
        0xA8C4_7BFD, // 212: ldp x29, x30, [sp], #64
        RET,         // 216: ret
    ];
    let mut fixed = code;
    // Patch cbnz x21 at byte 48 to target byte 108 (+60 = +15 words -> 15 << 5 = 0x1E0).
    fixed[12] = 0xB500_01F5;
    let mut r = CodeRecord::new_in(alloc);
    for inst in fixed {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    for (at, name) in [
        (32u32, "pthread_getspecific"),
        (60, "calloc"),
        (96, "pthread_setspecific"),
        (124, "aligned_alloc"),
        (152, "memset"),
        (176, "memcpy"),
    ] {
        let idx = r.outside(name);
        r.sites.push(Site { at, kind: SiteKind::Call, target: idx, addend: 0 });
    }
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

/// Real AArch64 TLS destructor (`fn(table: *mut u8)`): frees slot 0, slot 1, and `table`.
fn tls_ender_rec(alloc: Alloc<'static>) -> AVec<'static, u8> {
    let code = [
        0xA9BE_7BFD, // 0: stp x29, x30, [sp, #-32]!
        MOV_FP,      // 4: mov x29, sp
        0xF900_0BF3, // 8: str x19, [sp, #16]
        0xAA00_03F3, // 12: mov x19, x0 (table)
        0xF940_1260, // 16: ldr x0, [x19, #32] (slot[0])
        ADRP_X16,    // 20: free(slot[0]) (free(NULL) is a no-op)
        LDR_X16,     // 24
        BLR_X16,     // 28
        0xF940_1660, // 32: ldr x0, [x19, #40] (slot[1])
        ADRP_X16,    // 36: free(slot[1])
        LDR_X16,     // 40
        BLR_X16,     // 44
        0xAA13_03E0, // 48: mov x0, x19
        ADRP_X16,    // 52: free(table)
        LDR_X16,     // 56
        BLR_X16,     // 60
        0xF940_0BF3, // 64: ldr x19, [sp, #16]
        0xA8C2_7BFD, // 68: ldp x29, x30, [sp], #32
        RET,         // 72: ret
    ];
    let mut r = CodeRecord::new_in(alloc);
    for inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    for at in [20u32, 36, 52] {
        let idx = r.outside("free");
        r.sites.push(Site { at, kind: SiteKind::Call, target: idx, addend: 0 });
    }
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

/// Worker pthread: reads fresh `TLS_INIT` (30) and `TLS_ZERO` (0), mutates its own copies to 99
/// and 77, and returns `old_init + old_zero` (= 30).
fn tls_worker_rec(addr_init: Symbol, addr_zero: Symbol, alloc: Alloc<'static>) -> AVec<'static, u8> {
    let code = [
        0xA9BE_7BFD, // 0: stp x29, x30, [sp, #-32]!
        MOV_FP,      // 4: mov x29, sp
        0xA901_53F3, // 8: stp x19, x20, [sp, #16]
        ADRP_X16,    // 12: call addr_init()
        LDR_X16,     // 16
        BLR_X16,     // 20
        0xF940_0013, // 24: ldr x19, [x0] (old_init = 30)
        0xD280_0C69, // 28: mov x9, #99
        0xF900_0009, // 32: str x9, [x0] (*TLS_INIT = 99 on worker thread)
        ADRP_X16,    // 36: call addr_zero()
        LDR_X16,     // 40
        BLR_X16,     // 44
        0xF940_0014, // 48: ldr x20, [x0] (old_zero = 0)
        0xD280_09A9, // 52: mov x9, #77
        0xF900_0009, // 56: str x9, [x0] (*TLS_ZERO = 77 on worker thread)
        0x8B14_0260, // 60: add x0, x19, x20 (return 30 + 0 = 30)
        0xA941_53F3, // 64: ldp x19, x20, [sp, #16]
        0xA8C2_7BFD, // 68: ldp x29, x30, [sp], #32
        RET,         // 72: ret
    ];
    let mut r = CodeRecord::new_in(alloc);
    for inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    let i_init = r.target(Target { space: Space::Func, symbol: addr_init });
    let i_zero = r.target(Target { space: Space::Func, symbol: addr_zero });
    r.sites.push(Site { at: 12, kind: SiteKind::Call, target: i_init, addend: 0 });
    r.sites.push(Site { at: 36, kind: SiteKind::Call, target: i_zero, addend: 0 });
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

/// Main: mutates main thread's `TLS_INIT` (30 -> 35 -> 37, testing slow + fast path) and
/// `TLS_ZERO` (0 -> 5), spawns & joins `worker` (asserting worker saw untouched 30 + 0 = 30 while
/// mutating its own copies to 99 and 77), re-reads main thread's untouched copies (37 + 5 = 42),
/// and calls `exit(42)`.
fn tls_main_rec(addr_init: Symbol, addr_zero: Symbol, worker: Symbol, alloc: Alloc<'static>) -> AVec<'static, u8> {
    let code = [
        0xA9BC_7BFD, // 0: stp x29, x30, [sp, #-64]!  (sp+16: tid, sp+24: worker_ret, sp+32: x19/x20)
        MOV_FP,      // 4: mov x29, sp
        0xA902_53F3, // 8: stp x19, x20, [sp, #32]
        // 1. Slow path: TLS_INIT (30 -> 35)
        ADRP_X16,    // 12: call addr_init()
        LDR_X16,     // 16
        BLR_X16,     // 20
        0xF940_0009, // 24: ldr x9, [x0] (30)
        0x9100_1529, // 28: add x9, x9, #5 (35)
        0xF900_0009, // 32: str x9, [x0]
        // 2. Fast path hit: TLS_INIT (35 -> 37)
        ADRP_X16,    // 36: call addr_init()
        LDR_X16,     // 40
        BLR_X16,     // 44
        0xF940_0009, // 48: ldr x9, [x0] (35)
        0x9100_0929, // 52: add x9, x9, #2 (37)
        0xF900_0009, // 56: str x9, [x0]
        // 3. Slow path: TLS_ZERO (0 -> 5)
        ADRP_X16,    // 60: call addr_zero()
        LDR_X16,     // 64
        BLR_X16,     // 68
        0xF940_0009, // 72: ldr x9, [x0] (0)
        0x9100_1529, // 76: add x9, x9, #5 (5)
        0xF900_0009, // 80: str x9, [x0]
        // 4. Spawn worker pthread: pthread_create(&tid, NULL, worker, NULL)
        0x9100_43E0, // 84: add x0, sp, #16 (&tid)
        0xD280_0001, // 88: mov x1, #0
        0x9000_0002, // 92: adrp x2, worker (SiteKind::Load -> adrp+add)
        0xF940_0042, // 96: ldr x2, [x2]
        0xD280_0003, // 100: mov x3, #0
        ADRP_X16,    // 104: pthread_create
        LDR_X16,     // 108
        BLR_X16,     // 112
        // 5. Join worker pthread: pthread_join(tid, &worker_ret)
        0xF940_0BE0, // 116: ldr x0, [sp, #16] (tid)
        0x9100_63E1, // 120: add x1, sp, #24 (&worker_ret)
        ADRP_X16,    // 124: pthread_join
        LDR_X16,     // 128
        BLR_X16,     // 132
        // 6. Verify worker_ret == 30 (else exit 99)
        0xF940_0FE9, // 136: ldr x9, [sp, #24]
        0xF100_793F, // 140: cmp x9, #30
        0x5400_0060, // 144: b.eq +12 (156: ok)
        0x5280_0C60, // 148: mov w0, #99
        0x1400_000B, // 152: b +44 (196: do_exit)
        // ok (156): re-read main thread's TLS_INIT (37) and TLS_ZERO (5) -> sum = 42
        ADRP_X16,    // 156: call addr_init()
        LDR_X16,     // 160
        BLR_X16,     // 164
        0xF940_0013, // 168: ldr x19, [x0] (37)
        ADRP_X16,    // 172: call addr_zero()
        LDR_X16,     // 176
        BLR_X16,     // 180
        0xF940_0014, // 184: ldr x20, [x0] (5)
        0x8B14_0260, // 188: add x0, x19, x20 (42)
        0xD503_201F, // 192: nop
        // do_exit (196):
        ADRP_X16,    // 196: exit(w0)
        LDR_X16,     // 200
        BLR_X16,     // 204
    ];
    let mut r = CodeRecord::new_in(alloc);
    for inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    let i_init = r.target(Target { space: Space::Func, symbol: addr_init });
    let i_zero = r.target(Target { space: Space::Func, symbol: addr_zero });
    let i_worker = r.target(Target { space: Space::Func, symbol: worker });
    for at in [12u32, 36, 156] {
        r.sites.push(Site { at, kind: SiteKind::Call, target: i_init, addend: 0 });
    }
    for at in [60u32, 172] {
        r.sites.push(Site { at, kind: SiteKind::Call, target: i_zero, addend: 0 });
    }
    r.sites.push(Site { at: 92, kind: SiteKind::Load, target: i_worker, addend: 0 });
    for (at, name) in [(104u32, "pthread_create"), (124, "pthread_join"), (196, "exit")] {
        let idx = r.outside(name);
        r.sites.push(Site { at, kind: SiteKind::Call, target: idx, addend: 0 });
    }
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

struct TlsTestDeclares;

impl Declares for TlsTestDeclares {
    fn declared(&self, t: Target) -> Option<Declare<'_>> {
        match t.space {
            Space::Func => Some(Declare { kind: DeclaredKind::Instance, item: 1, name: b"fn" }),
            Space::Static => Some(Declare { kind: DeclaredKind::TlsDescriptor, item: 0, name: &[] }),
            Space::Vtable => Some(Declare { kind: DeclaredKind::Vtable, item: 1, name: b"vt" }),
            Space::Tls => Some(Declare { kind: DeclaredKind::ThreadLocal, item: 1, name: &[] }),
            Space::Outside => None,
        }
    }
}

/// Live end-to-end execution test on macOS arm64: exports a real Mach-O executable with BOTH
/// initialized (`30`) and zero-initialized (`0`) Rust `Space::Tls` variables using `blaze-emit`'s
/// indexed TLS sequence, runs it live with a spawned second pthread (proving per-thread isolation,
/// fast-path `tpidrro_el0` hits, `__mod_init_func` startup, and `ender` cleanup), and verifies
/// exit status 42 for BOTH `export` and `export_optimized`.
#[test]
fn rust_tls_exported_program_runs_multithreaded_and_exits_42() {
    let alloc = blaze_own::testing::heap().alloc();
    let desc_sym = sym("TLS_DESC");
    let getter_sym = sym("tls_getter");
    let ender_sym = sym("tls_ender");
    let addr_init_sym = sym("addr_init");
    let addr_zero_sym = sym("addr_zero");
    let worker_sym = sym("worker");
    let main_sym = sym("main");
    let tls_init = Target { space: Space::Tls, symbol: sym("TLS_INIT") };
    let tls_zero = Target { space: Space::Tls, symbol: sym("TLS_ZERO") };

    let r_main = tls_main_rec(addr_init_sym, addr_zero_sym, worker_sym, alloc);
    let r_worker = tls_worker_rec(addr_init_sym, addr_zero_sym, alloc);
    let r_addr_init = tls_addr_rec(desc_sym, getter_sym, tls_init, alloc);
    let r_addr_zero = tls_addr_rec(desc_sym, getter_sym, tls_zero, alloc);
    let r_getter = tls_getter_rec(alloc);
    let r_ender = tls_ender_rec(alloc);

    let init_bytes = 30u64.to_le_bytes();
    let data = [
        Data { target: tls_init, bytes: &init_bytes, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly },
        Data { target: tls_zero, bytes: &[], align: 8, words: &[], outside: &[], place: DataPlace::Zeroed { len: 8 } },
    ];
    let tls = Some(TlsRuntime { descriptor: desc_sym, ender: ender_sym });

    let v1_pairs: [(Symbol, &[u8]); 6] = [
        (main_sym, &r_main),
        (worker_sym, &r_worker),
        (addr_init_sym, &r_addr_init),
        (addr_zero_sym, &r_addr_zero),
        (getter_sym, &r_getter),
        (ender_sym, &r_ender),
    ];
    let functions: [Function<'_>; 6] = v1_pairs.map(|(symbol, bytes)| Function {
        symbol,
        record: CodeView::read(bytes).unwrap(),
        tier: Tier::Fast,
    });
    let prog = ProgramIn {
        name: "rust-tls-run",
        functions: &functions,
        data: &data,
        entry: Entry::CMain { main: main_sym },
        panic: Panic::Unwind,
        personality: None,
        tls,
        profile: None,
        natives: None,
        exports: &[],
    };
    let mut file_v1 = Vec::new_in(alloc);
    export(&prog, Platform::MacosArm64, &mut file_v1, alloc).unwrap();
    let code_v1 = run_exported(&file_v1[..], "rust-tls-v1");
    assert_eq!(code_v1, Some(42), "v1 exported TLS program ended with {code_v1:?}");

    // Also run the exact same program exported via `export_optimized` (`bzcode02`)!
    let mut converted: AVec<'_, AVec<'static, u8>> = Vec::with_capacity_in(6, alloc);
    for (_, bytes) in v1_pairs {
        let mut out = Vec::new_in(alloc);
        v1_to_v2(CodeView::read(bytes).unwrap(), &TlsTestDeclares, &mut out, alloc).unwrap();
        converted.push(out);
    }
    let mut funcs_v2: AVec<'_, Function02<'_>> = Vec::with_capacity_in(6, alloc);
    for (k, (symbol, _)) in v1_pairs.iter().enumerate() {
        funcs_v2.push(Function02 { symbol: *symbol, record: read(&converted[k]).unwrap(), tier: Tier::Fast });
    }
    let prog_v2 = ProgramIn02 {
        name: "rust-tls-run",
        functions: &funcs_v2,
        data: &data,
        entry: Entry::CMain { main: main_sym },
        panic: Panic::Unwind,
        personality: None,
        tls,
        profile: None,
    };
    let mut file_v2 = Vec::new_in(alloc);
    export_optimized(&prog_v2, Platform::MacosArm64, &mut file_v2, alloc).unwrap();
    let code_v2 = run_exported(&file_v2[..], "rust-tls-v2");
    assert_eq!(code_v2, Some(42), "v2 exported TLS program ended with {code_v2:?}");
}
