//! The export's tests: a tiny synthetic program (`main` calls `second`, which returns 42, then
//! calls the C library's `exit` with it) exported for both platforms, twice and from either
//! input order, byte-equal, and read back by the independent checkers; the planted bug (the
//! signature's page hashes taken before the last fixup is written) caught; the refusals named.
//! Each test prints the line its doc comment names.

use blaze_code::a64::{adrp_add_target, adrp_ldr_target, bl_target, read32};
use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target};
use blaze_own::{AVec, Alloc, Vec};

use crate::check::{self, CheckError};
use crate::error::ExportError;
use crate::export::export;
use crate::input::{Data, DataPlace, DataWord, Entry, Function, Panic, Platform, Profile, ProgramIn, Tier, TlsRuntime};
use crate::layout::{FixTo, Member, TlsPlan, lay_out, lay_out_with_tls};
use crate::plant::{self, Plant};
use crate::{elf, macho};

const STP: u32 = 0xA9BF_7BFD;
const MOV_FP: u32 = 0x9100_03FD;
const LDP: u32 = 0xA8C1_7BFD;
const RET: u32 = 0xD65F_03C0;
const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const ADRP_X0: u32 = 0x9000_0000;
const LDR_X0: u32 = 0xF940_0000;
const LDR_W0_X0: u32 = 0xB940_0000; // ldr w0, [x0]
const MOV_W0_42: u32 = 0x5280_0540; // mov w0, #42
const NOP: u32 = 0xD503_201F;

fn sym(name: &str) -> Symbol {
    Symbol::of_name(name)
}

fn func(name: &str) -> Target {
    Target { space: Space::Func, symbol: sym(name) }
}

/// A sealed, encoded record of `code` with sites `(at, kind, target)` and calls of C names.
fn rec(code: &[u32], sites: &[(u32, SiteKind, Target)], outside: &[(u32, &str)]) -> AVec<'static, u8> {
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
    let mut out = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut out);
    out
}

/// `main`: calls `second`, then `exit` with what it returned. `second`: returns 42.
fn tiny() -> (AVec<'static, u8>, AVec<'static, u8>) {
    let main = rec(
        &[STP, MOV_FP, ADRP_X16, LDR_X16, BLR_X16, ADRP_X16, LDR_X16, BLR_X16, LDP, RET],
        &[(8, SiteKind::Call, func("second"))],
        &[(20, "exit")],
    );
    let second = rec(&[MOV_W0_42, RET], &[], &[]);
    (main, second)
}

fn program<'r>(functions: &'r [Function<'r>], data: &'r [Data<'r>], profile: Option<Profile<'r>>) -> ProgramIn<'r> {
    ProgramIn { name: "tiny", functions, data, entry: Entry::CMain { main: sym("main") }, panic: Panic::Unwind, personality: None, tls: None, profile, natives: None, exports: &[], symbol_names: &[] }
}

fn functions<'r>(main: &'r [u8], second: &'r [u8]) -> [Function<'r>; 2] {
    [
        Function { symbol: sym("main"), record: CodeView::read(main).unwrap(), tier: Tier::Fast },
        Function { symbol: sym("second"), record: CodeView::read(second).unwrap(), tier: Tier::Fast },
    ]
}

fn exported(program: &ProgramIn<'_>, platform: Platform) -> AVec<'static, u8> {
    let mut out = Vec::new_in(Alloc::global());
    export(program, platform, &mut out, Alloc::global()).unwrap();
    out
}

/// Prints `layout: main calls second by bl, exit through the GOT word at 0x4000, entry = main`.
#[test]
fn calls_in_reach_are_direct() {
    let (main, second) = tiny();
    let fs = functions(&main, &second);
    let members: [Member<'_>; 2] = fs.map(|f| Member { symbol: f.symbol, record: f.record, tier: Some(f.tier) });
    let laid = lay_out(&members, &[], sym("main"), None, macho::plan(), 0, Alloc::global()).unwrap();
    let home = |name: &str| laid.placed.iter().find(|p| p.symbol == sym(name)).unwrap().at;
    let (m, s) = (home("main"), home("second"));
    assert_eq!(laid.entry, m);
    let at = |pc: u64| read32(&laid.text, pc - laid.text_at);
    assert_eq!((at(m + 8), at(m + 12)), (NOP, NOP));
    assert_eq!(bl_target(at(m + 16), m + 16), Some(s));
    let word = adrp_ldr_target(at(m + 20), at(m + 24), m + 20).unwrap();
    assert_eq!(word, laid.got_at);
    assert_eq!(laid.imports.len(), 1);
    assert_eq!(laid.imports[0].name, "exit");
    assert_eq!(laid.fixups[0].to, FixTo::Bind { import: 0 });
    println!("layout: main calls second by bl, exit through the GOT word at {:#x}, entry = main", laid.got_at);
}

/// Prints `macho: <N> bytes, <P> pages hashed, 1 bind, byte-equal across two exports and either
/// input order` and the report's first line, `exported at the fast tier: 2 functions; the
/// optimized tier is not built yet`.
#[test]
fn macho_is_deterministic_and_valid() {
    let (main, second) = tiny();
    let fs = functions(&main, &second);
    let one = exported(&program(&fs, &[], None), Platform::MacosArm64);
    let two = exported(&program(&fs, &[], None), Platform::MacosArm64);
    let reversed = [fs[1], fs[0]];
    let three = exported(&program(&reversed, &[], None), Platform::MacosArm64);
    assert_eq!(&one[..], &two[..]);
    assert_eq!(&one[..], &three[..]);
    let facts = check::macho(&one).unwrap();
    assert_eq!((facts.binds, facts.rebases), (1, 0));
    assert_eq!(facts.entry % 16, 0);
    let mut out = Vec::new_in(Alloc::global());
    let summary = export(&program(&fs, &[], None), Platform::MacosArm64, &mut out, Alloc::global()).unwrap();
    let mut report = blaze_own::String::new_in(Alloc::global());
    summary.report(&mut report).unwrap();
    let first = report.lines().next().unwrap();
    assert_eq!(first, "exported at the fast tier: 2 functions; the optimized tier is not built yet");
    println!(
        "macho: {} bytes, {} pages hashed, {} bind, byte-equal across two exports and either input order",
        one.len(),
        facts.pages,
        facts.binds
    );
    println!("{first}");
}

/// Prints `elf: <N> bytes, 2 binds (exit, __libc_start_main), byte-equal across two exports`.
#[test]
fn elf_is_deterministic_and_valid() {
    let (main, second) = tiny();
    let fs = functions(&main, &second);
    let one = exported(&program(&fs, &[], None), Platform::LinuxArm64);
    let reversed = [fs[1], fs[0]];
    let two = exported(&program(&reversed, &[], None), Platform::LinuxArm64);
    assert_eq!(&one[..], &two[..]);
    let facts = check::elf(&one).unwrap();
    assert_eq!((facts.binds, facts.relative), (2, 0));
    assert_eq!(facts.entry % 16, 0);
    println!("elf: {} bytes, {} binds (exit, __libc_start_main), byte-equal across two exports", one.len(), facts.binds);
}

#[test]
fn elf_synthesizes_dso_handle_as_relative_not_dynsym() {
    let main = rec(
        &[STP, MOV_FP, ADRP_X16, LDR_X16, BLR_X16, ADRP_X16, LDR_X16, BLR_X16, LDP, RET],
        &[],
        &[(8, "__dso_handle"), (20, "exit")],
    );
    let second = rec(&[MOV_W0_42, RET], &[], &[]);
    let fs = functions(&main, &second);
    let bytes = exported(&program(&fs, &[], None), Platform::LinuxArm64);
    let facts = check::elf(&bytes).unwrap();
    // `__dso_handle` becomes an `R_AARCH64_RELATIVE` (ELF header, addend 0), leaving only
    // `exit` and `__libc_start_main` as dynamic binds.
    assert_eq!((facts.binds, facts.relative), (2, 1));
    assert!(!bytes.windows(b"__dso_handle".len()).any(|w| w == b"__dso_handle"), "__dso_handle must not appear in .dynstr");
}

/// The control: the writer with its page hashes taken before the last fixup is written. Prints
/// `planted: check: the code directory's hash of page <P> is not the page's SHA-256`.
#[test]
fn signing_before_the_last_fixup_is_caught() {
    let (main, second) = tiny();
    let fs = functions(&main, &second);
    let clean = exported(&program(&fs, &[], None), Platform::MacosArm64);
    assert!(check::macho(&clean).is_ok());
    let planted = plant::with(Plant::SignBeforeLastFixup, || exported(&program(&fs, &[], None), Platform::MacosArm64));
    let Err(error) = check::macho(&planted) else { panic!("the checker passed a file signed before its last fixup") };
    assert!(matches!(error, CheckError::PageHash { .. }), "{error}");
    println!("planted: {error}");
}

/// Prints `refused: export: function <main> names Func <second>, which no function or datum
/// given places (rule: export never leaves a hole (run.md, "Export"))`.
#[test]
fn a_call_out_of_reach_is_refused() {
    let (main, _) = tiny();
    let fs = [Function { symbol: sym("main"), record: CodeView::read(&main).unwrap(), tier: Tier::Fast }];
    let mut out = Vec::new_in(Alloc::global());
    let Err(error) = export(&program(&fs, &[], None), Platform::MacosArm64, &mut out, Alloc::global()) else {
        panic!("a call of a function not given was exported");
    };
    assert_eq!(error, ExportError::OutOfReach { from: sym("main"), target: func("second") });
    println!("refused: {error}");
}

/// Prints `folded: 2 equal records, 1 copy of code`.
#[test]
fn equal_records_are_placed_once() {
    let a = rec(&[MOV_W0_42, RET], &[], &[]);
    let main = rec(
        &[STP, MOV_FP, ADRP_X16, LDR_X16, BLR_X16, ADRP_X16, LDR_X16, BLR_X16, LDP, RET],
        &[(8, SiteKind::Call, func("f")), (20, SiteKind::Call, func("g"))],
        &[],
    );
    let members = [
        Member { symbol: sym("main"), record: CodeView::read(&main).unwrap(), tier: Some(Tier::Fast) },
        Member { symbol: sym("f"), record: CodeView::read(&a).unwrap(), tier: Some(Tier::Fast) },
        Member { symbol: sym("g"), record: CodeView::read(&a).unwrap(), tier: Some(Tier::Fast) },
    ];
    let laid = lay_out(&members, &[], sym("main"), None, macho::plan(), 0, Alloc::global()).unwrap();
    let home = |name: &str| *laid.placed.iter().find(|p| p.symbol == sym(name)).unwrap();
    assert_eq!(home("f").at, home("g").at);
    assert_eq!(laid.placed.iter().filter(|p| p.folded).count(), 1);
    println!("folded: 2 equal records, 1 copy of code");
}

/// Prints `profile: second placed first; the identity moves with the profile's digest`.
#[test]
fn the_profile_orders_and_is_in_the_identity() {
    let (main, second) = tiny();
    let fs = functions(&main, &second);
    let members: [Member<'_>; 2] = fs.map(|f| Member { symbol: f.symbol, record: f.record, tier: Some(f.tier) });
    let hot = [sym("second")];
    let profile = Profile { digest: [7; 32], hot: &hot };
    let laid = lay_out(&members, &[], sym("main"), Some(profile), elf::plan(), 0, Alloc::global()).unwrap();
    assert_eq!(laid.placed[0].symbol, sym("second"));
    let mut out = Vec::new_in(Alloc::global());
    let with = export(&program(&fs, &[], Some(profile)), Platform::MacosArm64, &mut out, Alloc::global()).unwrap();
    let without = export(&program(&fs, &[], None), Platform::MacosArm64, &mut out, Alloc::global()).unwrap();
    assert_ne!(with.identity, without.identity);
    assert_eq!(with.profile, Some([7; 32]));
    println!("profile: second placed first; the identity moves with the profile's digest");
}

/// A static read through `adrp; add` and a vtable holding `main`'s address (one rebase).
/// Prints `data: the static's load is adrp+add to its home; 1 rebase for the vtable's word`.
#[test]
fn statics_and_vtables_are_placed_and_fixed() {
    let st = Target { space: Space::Static, symbol: sym("ANSWER") };
    let vt = Target { space: Space::Vtable, symbol: sym("VTABLE") };
    let main = rec(&[ADRP_X0, LDR_X0, LDR_W0_X0, RET], &[(0, SiteKind::Load, st)], &[]);
    let answer = 42u64.to_le_bytes();
    let table = [0u8; 8];
    let words = [DataWord { at: 0, target: Some(func("main")), addend: 0 }];
    let data = [
        Data { target: st, bytes: &answer, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly },
        Data { target: vt, bytes: &table, align: 8, words: &words, outside: &[], place: DataPlace::ReadOnly },
    ];
    let members = [Member { symbol: sym("main"), record: CodeView::read(&main).unwrap(), tier: Some(Tier::Fast) }];
    let laid = lay_out(&members, &data, sym("main"), None, macho::plan(), 0, Alloc::global()).unwrap();
    let m = laid.entry;
    let x = |pc: u64| read32(&laid.text, pc - laid.text_at);
    let to = adrp_add_target(x(m), x(m + 4), m).unwrap();
    assert_eq!(&laid.konst[(to - laid.const_at) as usize..][..8], &answer);
    assert_eq!(laid.fixups.len(), 1);
    assert!(matches!(laid.fixups[0].to, FixTo::Rebase { target } if target == m));
    let fs = [Function { symbol: sym("main"), record: CodeView::read(&main).unwrap(), tier: Tier::Fast }];
    let file = exported(&program(&fs, &data, None), Platform::MacosArm64);
    let facts = check::macho(&file).unwrap();
    assert_eq!((facts.rebases, facts.binds), (1, 0));
    println!("data: the static's load is adrp+add to its home; {} rebase for the vtable's word", facts.rebases);
}

#[test]
fn wasm64_export_is_deterministic_and_valid_memory64() {
    use crate::wasm64::{
        ValType, Wasm64Data, Wasm64Error, Wasm64Func, Wasm64Opts, Wasm64Program, check_wasm64,
        export_wasm64,
    };

    // `_start() -> i32 { 0 }` (i32.const 0 = 0x41 0x00)
    let funcs = [Wasm64Func {
        export_name: Some("_start"),
        params: &[],
        results: &[ValType::I32],
        locals: &[],
        body: &[0x41, 0x00],
    }];
    let data = [Wasm64Data { offset: 0x1000, bytes: b"hello wasm64" }];
    let prog = Wasm64Program {
        name: "hello-wasm64",
        funcs: &funcs,
        data: &data,
        indirect_table: &[],
        extra_types: &[],
        omit_blaze_host_v1: false,
        extra_imports: &[],
        extra_globals: &[],
        profile_digest: Some([0xAB; 32]),
        opts: Wasm64Opts::DEFAULT,
    };
    let mut out1 = Vec::new_in(Alloc::global());
    let mut out2 = Vec::new_in(Alloc::global());
    let meta1 = export_wasm64(&prog, &mut out1, Alloc::global()).expect("wasm64 exports");
    let meta2 = export_wasm64(&prog, &mut out2, Alloc::global()).expect("wasm64 exports");
    assert_eq!(out1.as_slice(), out2.as_slice(), "wasm64 export must be byte-deterministic");
    assert_eq!(meta1.identity, meta2.identity);

    let checked = check_wasm64(&out1).expect("wasm64 spec validator accepts module");
    assert!(checked.memory64);
    assert_eq!(checked.min_pages, Wasm64Opts::DEFAULT.min_pages);
    assert_eq!(checked.max_pages, Wasm64Opts::DEFAULT.max_pages);
    assert_eq!(checked.host_imports, 3);
    assert_eq!(checked.import_count, 3);
    assert!(checked.has_blaze_host_v1);
    assert_eq!(checked.func_count, 1);
    assert_eq!(checked.global_count, 3);
    assert_eq!(checked.data_segments, 1);
    assert!(checked.has_memory_export);
    assert!(checked.has_start_export);
    assert!(checked.has_profile_section);

    // Corrupted memory32 module (flag 0x01 instead of 0x05) is refused by check_wasm64.
    let mut bad = out1.clone();
    let mem_sec_pos = bad.windows(3).position(|w| w == [5, 8, 1]).or_else(|| {
        bad.windows(2).position(|w| w[0] == 0x05 && (w[1] == 0x10))
    });
    if let Some(p) = mem_sec_pos {
        bad[p] = 0x01; // clear memory64 bit 0x04
        assert!(matches!(check_wasm64(&bad), Err(Wasm64Error::NotMemory64 { .. })));
    }
}

#[test]
fn wasm64_indirect_table_and_extra_types_validate() {
    use crate::wasm64::{
        HOST_IMPORT_COUNT, ValType, Wasm64Error, Wasm64Func, Wasm64Opts, Wasm64Program,
        check_wasm64, export_wasm64,
    };

    let funcs = [
        Wasm64Func {
            export_name: Some("_start"),
            params: &[],
            results: &[ValType::I32],
            locals: &[],
            body: &[0x41, 0x00],
        },
        Wasm64Func {
            export_name: None,
            params: &[ValType::I64],
            results: &[ValType::I64],
            locals: &[],
            body: &[0x20, 0x00],
        },
    ];
    let extra_types: [(&[ValType], &[ValType]); 1] = [(&[ValType::I32, ValType::I32], &[ValType::I32])];
    let indirect = [HOST_IMPORT_COUNT + 1];
    let prog = Wasm64Program {
        name: "indirect-wasm64",
        funcs: &funcs,
        data: &[],
        indirect_table: &indirect,
        extra_types: &extra_types,
        omit_blaze_host_v1: false,
        extra_imports: &[],
        extra_globals: &[],
        profile_digest: None,
        opts: Wasm64Opts::DEFAULT,
    };
    assert_eq!(prog.import_count(), HOST_IMPORT_COUNT);
    assert_eq!(prog.type_index_for(&[ValType::I64], &[ValType::I64]), Some(HOST_IMPORT_COUNT + 1));
    assert_eq!(
        prog.type_index_for(&[ValType::I32, ValType::I32], &[ValType::I32]),
        Some(prog.extra_type_index(0))
    );

    let mut out = Vec::new_in(Alloc::global());
    export_wasm64(&prog, &mut out, Alloc::global()).expect("exports with table + elem");
    let checked = check_wasm64(&out).expect("check_wasm64 accepts table + elem sections");
    assert!(checked.has_blaze_host_v1);
    assert_eq!(checked.table_size, 2);
    assert_eq!(checked.elem_count, 1);

    // Out-of-bounds indirect_table func index is refused cleanly.
    let bad_table = [HOST_IMPORT_COUNT + 99];
    let bad_prog = Wasm64Program { indirect_table: &bad_table, ..prog };
    assert!(matches!(
        export_wasm64(&bad_prog, &mut out, Alloc::global()),
        Err(Wasm64Error::InvalidTableFunc { .. })
    ));
}

#[test]
fn wasm64_reactor_extra_imports_and_extra_globals_validate() {
    use crate::wasm64::{
        ValType, Wasm64Error, Wasm64ExportGlobal, Wasm64Func, Wasm64Import, Wasm64Opts,
        Wasm64Program, check_wasm64, export_wasm64,
    };

    let imports = [
        Wasm64Import {
            module: "wasi_snapshot_preview1_64",
            field: "fd_write",
            params: &[ValType::I32, ValType::I64, ValType::I64, ValType::I64],
            results: &[ValType::I32],
        },
        Wasm64Import {
            module: "blaze_proc_macro_64",
            field: "dispatch",
            params: &[ValType::I64, ValType::I64],
            results: &[ValType::I64],
        },
    ];
    let funcs = [Wasm64Func {
        export_name: Some("blaze_proc_macro_expand"),
        params: &[ValType::I64],
        results: &[ValType::I64],
        locals: &[],
        body: &[0x20, 0x00], // local.get 0
    }];
    let globals = [
        Wasm64ExportGlobal {
            name: "__blaze_proc_macro_decls",
            val_type: ValType::I64,
            mutable: false,
            init_i64: 0x40000,
        },
        Wasm64ExportGlobal {
            name: "__blaze_proc_macro_abi",
            val_type: ValType::I32,
            mutable: true,
            init_i64: 2,
        },
    ];
    let extra_types: [(&[ValType], &[ValType]); 1] = [(&[ValType::F64], &[ValType::F64])];
    let indirect = [2u32]; // defined func 0 is at index `import_count() == 2`
    let prog = Wasm64Program {
        name: "reactor-wasm64",
        funcs: &funcs,
        data: &[],
        indirect_table: &indirect,
        extra_types: &extra_types,
        omit_blaze_host_v1: true,
        extra_imports: &imports,
        extra_globals: &globals,
        profile_digest: None,
        opts: Wasm64Opts::DEFAULT,
    };

    assert_eq!(prog.import_count(), 2);
    assert_eq!(
        prog.type_index_for(
            &[ValType::I32, ValType::I64, ValType::I64, ValType::I64],
            &[ValType::I32]
        ),
        Some(0)
    );
    assert_eq!(prog.type_index_for(&[ValType::I64, ValType::I64], &[ValType::I64]), Some(1));
    assert_eq!(prog.type_index_for(&[ValType::I64], &[ValType::I64]), Some(2));
    assert_eq!(
        prog.type_index_for(&[ValType::F64], &[ValType::F64]),
        Some(prog.extra_type_index(0))
    );
    assert_eq!(prog.extra_type_index(0), 3);

    let mut out = Vec::new_in(Alloc::global());
    export_wasm64(&prog, &mut out, Alloc::global()).expect("reactor wasm64 exports");
    let checked = check_wasm64(&out).expect("check_wasm64 accepts reactor module");
    assert!(checked.memory64);
    assert!(!checked.has_blaze_host_v1);
    assert_eq!(checked.host_imports, 0);
    assert_eq!(checked.import_count, 2);
    assert_eq!(checked.func_count, 1);
    assert_eq!(checked.global_count, 5);
    assert_eq!(checked.export_count, 5);
    assert!(checked.has_memory_export);
    assert!(!checked.has_start_export);
    assert_eq!(checked.table_size, 2);
    assert_eq!(checked.elem_count, 1);

    // Duplicate global export name is refused cleanly.
    let dup_globals = [Wasm64ExportGlobal {
        name: "blaze_proc_macro_expand",
        val_type: ValType::I64,
        mutable: false,
        init_i64: 0,
    }];
    let dup_prog = Wasm64Program { extra_globals: &dup_globals, ..prog };
    assert_eq!(
        export_wasm64(&dup_prog, &mut out, Alloc::global()),
        Err(Wasm64Error::DuplicateExport)
    );
}

/// Rust `Space::Tls` layout and format checks on both Mach-O arm64 and ELF arm64:
/// initialized TLS (`ReadOnly`), zero-initialized TLS (`Zeroed`), TLS with a relocated pointer
/// (`DataWord` to a regular static), and a relro `Word` naming a `Space::Tls` index plus addend.
#[test]
fn rust_tls_macho_and_elf_layout_and_determinism() {
    let alloc = blaze_own::testing::heap().alloc();
    let desc_sym = sym("TLS_DESC");
    let ender_sym = sym("tls_ender");
    let desc_target = Target { space: Space::Static, symbol: desc_sym };
    let st_target = Target { space: Space::Static, symbol: sym("SHARED_VAL") };
    let tls_init = Target { space: Space::Tls, symbol: sym("TLS_INIT") };
    let tls_zero = Target { space: Space::Tls, symbol: sym("TLS_ZERO") };
    let tls_ptr = Target { space: Space::Tls, symbol: sym("TLS_PTR") };

    // Build `main`: loads DESC, TLS_INIT, TLS_ZERO, TLS_PTR; also has a relro word naming
    // TLS_ZERO with addend +5 (must fold to integer index + 5, not a rebase).
    let main_bytes = {
        let mut r = CodeRecord::new_in(alloc);
        let code = [
            ADRP_X0, LDR_X0, // 0: DESC
            ADRP_X0, LDR_X0, // 8: TLS_INIT
            ADRP_X0, LDR_X0, // 16: TLS_ZERO
            ADRP_X0, LDR_X0, // 24: TLS_PTR
            MOV_W0_42, RET,  // 32, 36
        ];
        for inst in code {
            r.code.extend_from_slice(&inst.to_le_bytes());
        }
        r.relro.resize(8, 0);
        let i_desc = r.target(desc_target);
        let i_init = r.target(tls_init);
        let i_zero = r.target(tls_zero);
        let i_ptr = r.target(tls_ptr);
        r.sites.push(Site { at: 0, kind: SiteKind::Load, target: i_desc, addend: 0 });
        r.sites.push(Site { at: 8, kind: SiteKind::Load, target: i_init, addend: 0 });
        r.sites.push(Site { at: 16, kind: SiteKind::Load, target: i_zero, addend: 0 });
        r.sites.push(Site { at: 24, kind: SiteKind::Load, target: i_ptr, addend: 0 });
        r.words.push(blaze_code::record::Word { at: 0, target: i_zero, addend: 5 });
        let mut out = Vec::new_in(alloc);
        r.seal().unwrap().encode(&mut out);
        out
    };
    let ender_bytes = rec(&[RET], &[], &[]);
    let shared_bytes = 99u64.to_le_bytes();
    let init_bytes = 40u64.to_le_bytes();
    let ptr_bytes = [0u8; 8];
    let ptr_words = [DataWord { at: 0, target: Some(st_target), addend: 0 }];
    let data = [
        Data { target: st_target, bytes: &shared_bytes, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly },
        Data { target: tls_init, bytes: &init_bytes, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly },
        Data { target: tls_zero, bytes: &[], align: 16, words: &[], outside: &[], place: DataPlace::Zeroed { len: 16 } },
        Data { target: tls_ptr, bytes: &ptr_bytes, align: 8, words: &ptr_words, outside: &[], place: DataPlace::ReadOnly },
    ];
    let data_rev = [data[3], data[2], data[1], data[0]];
    let fs = [
        Function { symbol: sym("main"), record: CodeView::read(&main_bytes).unwrap(), tier: Tier::Fast },
        Function { symbol: ender_sym, record: CodeView::read(&ender_bytes).unwrap(), tier: Tier::Fast },
    ];
    let fs_rev = [fs[1], fs[0]];
    let tls_rt = Some(TlsRuntime { descriptor: desc_sym, ender: ender_sym });

    // Check layout internals on macOS plan directly.
    let members = [
        Member { symbol: sym("main"), record: CodeView::read(&main_bytes).unwrap(), tier: Some(Tier::Fast) },
        Member { symbol: ender_sym, record: CodeView::read(&ender_bytes).unwrap(), tier: Some(Tier::Fast) },
    ];
    let laid = lay_out_with_tls(
        &members,
        &data,
        sym("main"),
        None,
        macho::plan(),
        0,
        Some(TlsPlan { descriptor: desc_sym, mod_init_fn: None, initial_root: 0 }),
        alloc,
    )
    .unwrap();
    assert!(laid.has_tls);
    assert_eq!(laid.tls_count, 3);
    let m = laid.entry;
    let at = |pc: u64| read32(&laid.text, pc - laid.text_at);
    // DESC is relaxed to adrp+add into writable data; check header fields (key=0, root=0, count=3, cap=3).
    let desc_at = adrp_add_target(at(m), at(m + 4), m).unwrap();
    let desc_off = usize::try_from(desc_at - laid.data_at).unwrap();
    let read_u64 = |buf: &[u8], off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
    assert_eq!(read_u64(&laid.data, desc_off), 0);
    assert_eq!(read_u64(&laid.data, desc_off + 8), 0);
    assert_eq!(read_u64(&laid.data, desc_off + 16), 3);
    assert_eq!(read_u64(&laid.data, desc_off + 24), 3);
    // All three Space::Tls sites keep `adrp; ldr` pointing to distinct index words holding 0, 1, 2.
    let mut loaded = [0u64; 3];
    for (idx, site_off) in [8u64, 16, 24].into_iter().enumerate() {
        let word_at = adrp_ldr_target(at(m + site_off), at(m + site_off + 4), m + site_off).unwrap();
        let off = usize::try_from(word_at - laid.const_at).unwrap();
        loaded[idx] = read_u64(&laid.konst, off);
    }
    let zero_idx = loaded[1];
    loaded.sort_unstable();
    assert_eq!(loaded, [0, 1, 2]);
    // Relro word for TLS_ZERO + 5 holds integer `zero_idx + 5`, NOT a fixup.
    let relro_at = laid.const_at + u64::try_from(laid.konst.len()).unwrap();
    let found_relro = laid.konst.windows(8).any(|w| u64::from_le_bytes(w.try_into().unwrap()) == zero_idx + 5);
    assert!(found_relro && relro_at > laid.const_at);

    // Full export on macOS arm64: deterministic across reversed input order, valid Mach-O with `__mod_init_func`.
    let prog_a = ProgramIn {
        name: "tls-test",
        functions: &fs,
        data: &data,
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: None,
        tls: tls_rt,
        profile: None,
        natives: None,
        exports: &[],
        symbol_names: &[],
    };
    let prog_b = ProgramIn { functions: &fs_rev, data: &data_rev, ..prog_a };
    let mut mac_a = Vec::new_in(alloc);
    let mut mac_b = Vec::new_in(alloc);
    export(&prog_a, Platform::MacosArm64, &mut mac_a, alloc).unwrap();
    export(&prog_b, Platform::MacosArm64, &mut mac_b, alloc).unwrap();
    assert_eq!(&mac_a[..], &mac_b[..], "Mach-O TLS export must be deterministic");
    let mac_facts = check::macho(&mac_a).unwrap();
    assert!(matches!(mac_facts.mod_init, Some((_, 8))), "Mach-O TLS export must emit 8-byte __mod_init_func");
    assert_eq!(mac_facts.binds, 1, "Mach-O TLS export binds pthread_key_create");
    // Rebases: __mod_init_func (1) + DESC.vars (1) + 3 VarRow.init (3) + TLS_PTR DataWord (1) = 6.
    assert_eq!(mac_facts.rebases, 6);

    // Full export on Linux arm64: deterministic across reversed input order, valid ELF with PT_TLS.
    let mut elf_a = Vec::new_in(alloc);
    let mut elf_b = Vec::new_in(alloc);
    export(&prog_a, Platform::LinuxArm64, &mut elf_a, alloc).unwrap();
    export(&prog_b, Platform::LinuxArm64, &mut elf_b, alloc).unwrap();
    assert_eq!(&elf_a[..], &elf_b[..], "ELF TLS export must be deterministic");
    let elf_facts = check::elf(&elf_a).unwrap();
    assert_eq!(elf_facts.tls, Some((0, 8, 16)), "ELF TLS export must emit PT_TLS (filesz=0, memsz=8, align=16)");
    assert_eq!(elf_facts.binds, 2, "ELF TLS export binds __libc_start_main and pthread_key_create");
    // Rebases: DESC.vars (1) + 3 VarRow.init (3) + TLS_PTR DataWord (1) = 5.
    assert_eq!(elf_facts.relative, 5);
}

/// Every Rust `Space::Tls` refusal names the offending item and rule.
#[test]
fn rust_tls_refusals_are_named() {
    let alloc = blaze_own::testing::heap().alloc();
    let desc_sym = sym("TLS_DESC");
    let ender_sym = sym("tls_ender");
    let tls_a = Target { space: Space::Tls, symbol: sym("TLS_A") };
    let val = 7u64.to_le_bytes();
    let main_tls = rec(&[ADRP_X0, LDR_X0, MOV_W0_42, RET], &[(0, SiteKind::Load, tls_a)], &[]);
    let main_plain = rec(&[MOV_W0_42, RET], &[], &[]);
    let ender = rec(&[RET], &[], &[]);
    let tls_datum = [Data { target: tls_a, bytes: &val, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly }];
    let mut out = Vec::new_in(alloc);

    // 1. Space::Tls datum without TlsRuntime -> NoTlsRuntime.
    let fs_plain = [Function { symbol: sym("main"), record: CodeView::read(&main_plain).unwrap(), tier: Tier::Fast }];
    let p = ProgramIn {
        name: "t",
        functions: &fs_plain,
        data: &tls_datum,
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: None,
        tls: None,
        profile: None,
        natives: None,
        exports: &[],
        symbol_names: &[],
    };
    assert_eq!(export(&p, Platform::MacosArm64, &mut out, alloc), Err(ExportError::NoTlsRuntime { target: tls_a }));

    // 2. Space::Tls target in code without TlsRuntime -> NoTlsRuntime.
    let fs_tls = [Function { symbol: sym("main"), record: CodeView::read(&main_tls).unwrap(), tier: Tier::Fast }];
    let p = ProgramIn { functions: &fs_tls, data: &[], tls: None, ..p };
    assert_eq!(export(&p, Platform::MacosArm64, &mut out, alloc), Err(ExportError::NoTlsRuntime { target: tls_a }));

    // 3. TlsRuntime whose ender is not placed -> TlsEnderNotPlaced.
    let rt = Some(TlsRuntime { descriptor: desc_sym, ender: ender_sym });
    let p = ProgramIn { functions: &fs_tls, data: &tls_datum, tls: rt, ..p };
    assert_eq!(export(&p, Platform::MacosArm64, &mut out, alloc), Err(ExportError::TlsEnderNotPlaced { ender: ender_sym }));

    // 4. Space::Tls target in code with no datum -> OutOfReach.
    let fs_both = [
        Function { symbol: sym("main"), record: CodeView::read(&main_tls).unwrap(), tier: Tier::Fast },
        Function { symbol: ender_sym, record: CodeView::read(&ender).unwrap(), tier: Tier::Fast },
    ];
    let p = ProgramIn { functions: &fs_both, data: &[], tls: rt, ..p };
    assert_eq!(export(&p, Platform::MacosArm64, &mut out, alloc), Err(ExportError::OutOfReach { from: sym("main"), target: tls_a }));

    // 5. DataWord pointing at Space::Tls -> DataTlsTarget.
    let bad_words = [DataWord { at: 0, target: Some(tls_a), addend: 0 }];
    let bad_st = Target { space: Space::Static, symbol: sym("BAD_ST") };
    let data_bad_word = [
        Data { target: tls_a, bytes: &val, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly },
        Data { target: bad_st, bytes: &val, align: 8, words: &bad_words, outside: &[], place: DataPlace::ReadOnly },
    ];
    let p = ProgramIn { functions: &fs_both, data: &data_bad_word, tls: rt, ..p };
    assert_eq!(export(&p, Platform::MacosArm64, &mut out, alloc), Err(ExportError::DataTlsTarget { from: bad_st, target: tls_a }));

    // 6. SiteKind::Call on Space::Tls -> CallOfData.
    let main_call = rec(&[ADRP_X16, LDR_X16, BLR_X16, RET], &[(0, SiteKind::Call, tls_a)], &[]);
    let fs_call = [
        Function { symbol: sym("main"), record: CodeView::read(&main_call).unwrap(), tier: Tier::Fast },
        Function { symbol: ender_sym, record: CodeView::read(&ender).unwrap(), tier: Tier::Fast },
    ];
    let p = ProgramIn { functions: &fs_call, data: &tls_datum, tls: rt, ..p };
    assert_eq!(export(&p, Platform::MacosArm64, &mut out, alloc), Err(ExportError::CallOfData { from: sym("main"), at: 0 }));

    // 7. User static colliding with TlsRuntime::descriptor -> TwoData.
    let desc_target = Target { space: Space::Static, symbol: desc_sym };
    let collide_data = [Data { target: desc_target, bytes: &val, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly }];
    let fs_plain_ender = [
        Function { symbol: sym("main"), record: CodeView::read(&main_plain).unwrap(), tier: Tier::Fast },
        Function { symbol: ender_sym, record: CodeView::read(&ender).unwrap(), tier: Tier::Fast },
    ];
    let p = ProgramIn { functions: &fs_plain_ender, data: &collide_data, tls: rt, ..p };
    assert_eq!(export(&p, Platform::MacosArm64, &mut out, alloc), Err(ExportError::TwoData { target: desc_target }));

    // 8. `OWN_DATUM` (`target: None`) in Space::Tls (an immutable `'static` sub-allocation
    //    appended inside the read-only TLS template by `make_template_entry`) rebases cleanly
    //    into `__DATA_CONST` so every thread's copy shares the immutable pointee.
    let self_words = [DataWord { at: 0, target: None, addend: 8 }];
    let mut tls_with_tail = [0u8; 16];
    tls_with_tail[8..10].copy_from_slice(b"hi");
    let self_tls_data = [Data { target: tls_a, bytes: &tls_with_tail, align: 8, words: &self_words, outside: &[], place: DataPlace::ReadOnly }];
    let p = ProgramIn { functions: &fs_both, data: &self_tls_data, tls: rt, ..p };
    let exported = export(&p, Platform::MacosArm64, &mut out, alloc).expect("TLS template OWN_DATUM tail rebase");
    assert_eq!(exported.imports, 1, "pthread_key_create");
    check::macho(&out).expect("Mach-O valid with TLS OWN_DATUM tail rebase");

    // 9. Space::Outside import referenced only by a Space::Tls template is gathered and bound.
    let ext_sym = sym("puts");
    let ext_target = Target { space: Space::Outside, symbol: ext_sym };
    let ext_words = [DataWord { at: 0, target: Some(ext_target), addend: 0 }];
    let ext_outside = [(ext_sym, "puts")];
    let ext_tls_data = [Data { target: tls_a, bytes: &val, align: 8, words: &ext_words, outside: &ext_outside, place: DataPlace::ReadOnly }];
    let p = ProgramIn { functions: &fs_both, data: &ext_tls_data, tls: rt, ..p };
    let exported = export(&p, Platform::MacosArm64, &mut out, alloc).expect("TLS template outside import");
    assert_eq!(exported.imports, 2, "pthread_key_create + puts");
    check::macho(&out).expect("Mach-O valid with TLS outside import and 16B-aligned __const");
}

/// Regression: when `eh_room` reserves more pages after `__text` than `eh_sections` actually
/// needs (`up(eh.end(), PAGE) < laid.const_at`), Mach-O must still place `__DATA_CONST`'s bytes
/// at `laid.const_at` rather than right after `text_size`.
#[test]
fn macho_const_segment_lands_at_const_at_when_eh_leaves_page_gap() {
    let alloc = blaze_own::testing::heap().alloc();
    let magic = 0xCAFE_BABE_1234_5678u64.to_le_bytes();
    let ro = Target { space: Space::Static, symbol: sym("RO_MAGIC") };
    let main = rec(&[ADRP_X0, LDR_X0, MOV_W0_42, RET], &[(0, SiteKind::Load, ro)], &[]);
    let members = [Member { symbol: sym("main"), record: CodeView::read(&main).unwrap(), tier: Some(Tier::Fast) }];
    let data = [Data { target: ro, bytes: &magic, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly }];
    let tail = 0x8000u64;
    let laid = lay_out(&members, &data, sym("main"), None, macho::plan_for(None), tail, alloc).unwrap();
    let eh = crate::eh_out::eh_sections(&laid, &members, None, Platform::MacosArm64, alloc).unwrap();
    let text_size = (eh.end() + 0x3FFF) & !0x3FFF;
    assert!(
        laid.const_at > text_size,
        "test setup must leave at least one page gap between text_size ({text_size:#x}) and const_at ({:#x})",
        laid.const_at
    );

    let mut out = Vec::new_in(alloc);
    macho::write(&laid, &eh, "const-gap", &[0u8; 32], &mut out, alloc).unwrap();
    check::macho(&out).unwrap();
    let const_at = laid.const_at as usize;
    assert_eq!(
        &out[const_at..const_at + laid.konst.len()],
        &laid.konst[..],
        "__DATA_CONST bytes must be written at laid.const_at, not text_size"
    );
    assert!(
        out[const_at..const_at + laid.konst.len()].windows(magic.len()).any(|w| w == magic),
        "RO_MAGIC payload must be present inside __DATA_CONST at laid.const_at"
    );
}

/// Builds a synthetic `NativePlatform::LinuxArm64` `Image` matching `blaze_native::layout::lay`'s
/// `Layout::LINUX_ARM64` shape:
/// - `Text` (`0x1_0000`, 8 bytes): `c_entry` (`0x1_0000`), `c_init` (`0x1_0004`).
/// - `EhFrame` (`0x1_0008`, 32 bytes): 1 CIE + 1 FDE at `0x1_0010` covering `c_init` (`0x1_0004`).
/// - `Got` (`0x2_0000`, 0x20 bytes): `puts` (`GLOB_DAT`), optional `TlsReloc::TpRel` (offset 0) & `Desc` (offset 8).
/// - `Data` (`0x3_0000`, 0x68 bytes): BNL2 descriptor, names (`rust_cb`), pointer row, rebase (`0x3_0050 -> c_entry`),
///   weak import (`0x3_0058 -> weak_hook + 8`), `FixTo::Word { index: 0 }` (`0x3_0060`).
/// - `Words` (`0x3_0068`, 8 bytes): word 0 (`rust_cb`).
/// - `Inits` (`0x3_0070`, 8 bytes): init 0 (`c_init`).
/// - Optional `ThreadData` (`0x3_0080` or `0x3_0088`, 8 bytes, value `0x1122_3344_5566_7788`) + `ThreadZero` (8 bytes).
/// - `Zero` (16 bytes).
/// - Libraries: `c`, `pthread` (both `libc.so.6`, deduplicated), `m` (`libm.so.6`), `gcc_s` (`libgcc_s.so.1`).
fn linux_native_image(
    alloc: Alloc<'static>,
    with_tls: bool,
    tdata_at: u64,
    tls_align_log2: u8,
) -> blaze_native::image::Image<'static> {
    use blaze_native::Platform as NativePlatform;
    use blaze_native::image::{EhPointer, FixTo as NativeFix, Fixup as NativeFixup, Frame as NativeFrame, Image, Import as NativeImport, Placed as NativePlaced, Region, TlsReloc};
    use blaze_native::libraries::{Library, LibraryKind, Linked, Source, source};

    const START: u64 = 0x1_0000;
    let put32 = |bytes: &mut [u8], at: u64, value: u32| {
        let i = (at - START) as usize;
        bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
    };
    let put64 = |bytes: &mut [u8], at: u64, value: u64| {
        let i = (at - START) as usize;
        bytes[i..i + 8].copy_from_slice(&value.to_le_bytes());
    };

    let (tdata_len, tbss_at, tbss_len, init_end, zero_at) = if with_tls {
        (8u64, tdata_at + 8, 8u64, tdata_at + 8, tdata_at + 16)
    } else {
        (0u64, 0x3_0078u64, 0u64, 0x3_0078u64, 0x3_0078u64)
    };

    let mut regions: AVec<'static, NativePlaced> = Vec::new_in(alloc);
    let rows: [(Region, u64, u64, u8); 12] = [
        (Region::Text, 0x1_0000, 8, 2),
        (Region::Const, 0x1_0008, 0, 0),
        (Region::EhFrame, 0x1_0008, 32, 3),
        (Region::UnwindInfo, 0x1_0028, 0, 2),
        (Region::Got, 0x2_0000, 0x20, 4),
        (Region::Data, 0x3_0000, 0x68, 3),
        (Region::Words, 0x3_0068, 8, 3),
        (Region::Inits, 0x3_0070, 8, 3),
        (Region::ThreadVars, 0x3_0078, 0, 3),
        (Region::ThreadData, if with_tls { tdata_at } else { 0x3_0078 }, tdata_len, tls_align_log2),
        (Region::ThreadZero, tbss_at, tbss_len, tls_align_log2),
        (Region::Zero, zero_at, 16, 3),
    ];
    for (region, at, len, align_log2) in rows {
        regions.push(NativePlaced { region, at, len, align_log2 });
    }
    let mut bytes: AVec<'static, u8> = Vec::new_in(alloc);
    bytes.resize((init_end - START) as usize, 0);
    put32(&mut bytes, 0x1_0000, RET);
    put32(&mut bytes, 0x1_0004, RET);
    // Minimal 8-byte CIE at 0x1_0008 (len=4, id=0) + 24-byte FDE at 0x1_0010 (len=20, cie_ptr=12, pc_begin at 0x1_0018, range=4).
    put32(&mut bytes, 0x1_0008, 4);
    put32(&mut bytes, 0x1_000C, 0);
    put32(&mut bytes, 0x1_0010, 20);
    put32(&mut bytes, 0x1_0014, 12);
    put32(&mut bytes, 0x1_0018, (0x1_0004i64 - 0x1_0018) as i32 as u32);
    put32(&mut bytes, 0x1_001C, 4);
    // BNL2 descriptor header at 0x3_0000.
    put32(&mut bytes, 0x3_0000, 0x424e_4c32);
    put32(&mut bytes, 0x3_0004, 1);
    put32(&mut bytes, 0x3_0008, 1);
    put32(&mut bytes, 0x3_000C, 1);
    put64(&mut bytes, 0x3_0010, 0x3_0068);
    put64(&mut bytes, 0x3_0018, 0x3_0070);
    put64(&mut bytes, 0x3_0020, 0x3_0040);
    put64(&mut bytes, 0x3_0028, 0x3_0030);
    // Plant non-zero values in relocated slots to verify RELA zeroing.
    put64(&mut bytes, 0x2_0000, 0x1111_2222_3333_4444);
    put64(&mut bytes, 0x2_0008, 0x5555_6666_7777_8888);
    put64(&mut bytes, 0x2_0010, 0x9999_AAAA_BBBB_CCCC);
    put64(&mut bytes, 0x2_0018, 0xDDDD_EEEE_FFFF_0001);
    put64(&mut bytes, 0x3_0050, 0xDEAD);
    put64(&mut bytes, 0x3_0058, 0xCAFE);
    put64(&mut bytes, 0x3_0060, 0xBEEF);
    put64(&mut bytes, 0x3_0068, 0xF00D);
    put64(&mut bytes, 0x3_0070, 0x1234);
    if with_tls {
        put64(&mut bytes, tdata_at, 0x1122_3344_5566_7788);
    }

    let mut fixups: AVec<'static, NativeFixup> = Vec::new_in(alloc);
    fixups.push(NativeFixup { at: 0x2_0000, to: NativeFix::Import { index: 0, addend: 0 } });
    fixups.push(NativeFixup { at: 0x3_0050, to: NativeFix::rebase(0x1_0000) });
    fixups.push(NativeFixup { at: 0x3_0058, to: NativeFix::Import { index: 1, addend: 8 } });
    fixups.push(NativeFixup { at: 0x3_0060, to: NativeFix::Word { index: 0 } });
    fixups.push(NativeFixup { at: 0x3_0070, to: NativeFix::rebase(0x1_0004) });

    let mut tls: AVec<'static, TlsReloc> = Vec::new_in(alloc);
    if with_tls {
        tls.push(TlsReloc::TpRel { got_at: 0x2_0008, offset: 0 });
        tls.push(TlsReloc::Desc { got_at: 0x2_0010, offset: 8 });
    }

    let mut imports: AVec<'static, NativeImport<'static>> = Vec::new_in(alloc);
    imports.push(NativeImport { name: b"puts", weak: false });
    imports.push(NativeImport { name: b"weak_hook", weak: true });

    let mut exports: AVec<'static, (&'static [u8], u64)> = Vec::new_in(alloc);
    exports.push((b"__blaze_native_v1", 0x3_0000));
    exports.push((b"c_entry", 0x1_0000));
    exports.push((b"c_init", 0x1_0004));

    let mut words: AVec<'static, &'static [u8]> = Vec::new_in(alloc);
    words.push(b"rust_cb");

    let mut libraries: AVec<'static, Linked<'static>> = Vec::new_in(alloc);
    for name in ["c", "pthread", "m", "gcc_s"] {
        match source(NativePlatform::LinuxArm64, Library { kind: LibraryKind::Dylib, name }) {
            Ok(Source::Linked(linked)) => libraries.push(linked),
            other => panic!("unexpected library resolution for `{name}`: {other:?}"),
        }
    }

    let mut frames: AVec<'static, NativeFrame> = Vec::new_in(alloc);
    frames.push(NativeFrame { at: 0x1_0010, len: 24, start: 0x1_0004, range: 4 });
    let mut eh_pointers: AVec<'static, EhPointer> = Vec::new_in(alloc);
    eh_pointers.push(EhPointer { at: 0x1_0018, width: 4, to: 0x1_0004, minus: 0x1_0018 });

    Image {
        platform: NativePlatform::LinuxArm64,
        regions,
        bytes,
        file_start: START,
        end: zero_at + 16,
        fixups,
        tls,
        imports,
        exports,
        words,
        inits: 1,
        descriptor_at: 0x3_0000,
        libraries,
        unwind: Vec::new_in(alloc),
        frames,
        eh_pointers,
    }
}

/// Full ELF arm64 native C image merge: Rust->C direct calls (`c_entry` dropped from `.dynsym`
/// into `R_AARCH64_RELATIVE`), C->Rust `Word` callbacks (`rust_cb` direct `R_AARCH64_RELATIVE` in
/// both `Words` and `FixTo::Word`), `DT_INIT_ARRAY`, deduplicated `DT_NEEDED`, `GLOB_DAT` /
/// `ABS64` (including `STB_WEAK` + addend), `R_AARCH64_TLS_TPREL64` + `R_AARCH64_TLSDESC` +
/// `PT_TLS`, `.eh_frame_hdr` native FDE integration, RELA cell zeroing, and determinism.
#[test]
fn elf_native_merge_full_end_to_end() {
    use crate::input::CExport;

    let alloc = blaze_own::testing::heap().alloc();
    let img = linux_native_image(alloc, true, 0x3_0080, 3);
    // `main` calls `c_entry` (defined by native image -> direct RELATIVE!) and `exit` (external).
    let main_rec = rec(
        &[STP, MOV_FP, ADRP_X16, LDR_X16, BLR_X16, ADRP_X16, LDR_X16, BLR_X16, LDP, RET],
        &[],
        &[(8, "c_entry"), (20, "exit")],
    );
    let rust_cb_rec = rec(&[MOV_W0_42, RET], &[], &[]);
    let fs = [
        Function { symbol: sym("main"), record: CodeView::read(&main_rec).unwrap(), tier: Tier::Fast },
        Function { symbol: sym("rust_cb"), record: CodeView::read(&rust_cb_rec).unwrap(), tier: Tier::Fast },
    ];
    let fs_rev = [fs[1], fs[0]];
    let c_exports = [CExport { name: b"rust_cb", symbol: sym("rust_cb") }];
    let prog = ProgramIn {
        name: "elf-native",
        functions: &fs,
        data: &[],
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: None,
        tls: None,
        profile: None,
        natives: Some(&img),
        exports: &c_exports,
        symbol_names: &[],
    };
    let prog_rev = ProgramIn { functions: &fs_rev, ..prog };

    let mut out1 = Vec::new_in(alloc);
    let mut out2 = Vec::new_in(alloc);
    let summary = export(&prog, Platform::LinuxArm64, &mut out1, alloc).unwrap();
    export(&prog_rev, Platform::LinuxArm64, &mut out2, alloc).unwrap();
    assert_eq!(&out1[..], &out2[..], "ELF native merge export must be deterministic across input orders");

    // 1. Validate with `check::elf`.
    let facts = check::elf(&out1).unwrap();
    assert_eq!(facts.needed, 3, "DT_NEEDED: libc.so.6, libm.so.6, libgcc_s.so.1");
    assert_eq!(facts.relative, 5, "1 direct program import (c_entry) + 4 native relatives (rebase, Word ptr, Words[0], Inits[0])");
    assert_eq!(facts.binds, 4, "__libc_start_main, exit, puts, weak_hook");
    assert_eq!(facts.tls_relocs, 2, "TlsTpRel + TlsDesc");
    assert_eq!(facts.tls, Some((8, 16, 8)), "PT_TLS: 8B .tdata + 8B .tbss, align 8");
    assert_eq!(summary.imports, 4, "Exported::imports drops direct c_entry and counts the 4 remaining binds");

    // 2. Independently parse program headers, `.dynamic`, `.dynsym`, `.rela.dyn`, `PT_TLS`, `PT_GNU_EH_FRAME`.
    let u16_at = |at: usize| u16::from_le_bytes(out1[at..at + 2].try_into().unwrap());
    let u32_at = |at: usize| u32::from_le_bytes(out1[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(out1[at..at + 8].try_into().unwrap());
    let cstr = |at: usize| -> &[u8] {
        let len = out1[at..].iter().position(|&b| b == 0).unwrap();
        &out1[at..at + len]
    };

    let phoff = u64_at(32) as usize;
    let phnum = u16_at(56) as usize;
    let mut loads: AVec<'_, (u32, u64, u64, u64, u64)> = Vec::new_in(alloc);
    let mut dynamic_ph = None;
    let mut tls_ph = None;
    let mut eh_ph = None;
    for k in 0..phnum {
        let p = phoff + 56 * k;
        let kind = u32_at(p);
        let flags = u32_at(p + 4);
        let off = u64_at(p + 8);
        let va = u64_at(p + 16);
        let filesz = u64_at(p + 32);
        let memsz = u64_at(p + 40);
        match kind {
            1 => loads.push((flags, off, va, filesz, memsz)),
            2 => dynamic_ph = Some((off, filesz)),
            7 => tls_ph = Some((off, va, filesz, memsz)),
            0x6474_E550 => eh_ph = Some((off, va, filesz)),
            _ => {}
        }
    }
    // Program text (r-x), const (rw-), native text (r-x), native data (rw-), meta (r--) = 5 loads.
    assert_eq!(loads.len(), 5, "expected 5 PT_LOAD segments (no program data segment)");
    assert_eq!(loads[0].0, 5); // program text r-x
    assert_eq!(loads[1].0, 6); // program const rw-
    assert_eq!(loads[2].0, 5); // native text r-x
    assert_eq!(loads[3].0, 6); // native data rw-
    assert_eq!(loads[4].0, 4); // meta r--
    let shift = loads[2].2 - 0x1_0000;
    assert_eq!(shift % elf::PAGE, 0);
    assert_eq!(loads[3].2, 0x2_0000 + shift, "native data PT_LOAD starts at shifted Got");
    assert!(loads[3].4 > loads[3].3, "native data memsz covers ThreadZero + Zero past filesz");

    let vaddr_to_off = |va: u64| -> usize {
        let l = loads.iter().find(|l| va >= l.2 && va < l.2 + l.3).unwrap_or_else(|| panic!("vaddr {va:#x} not in any PT_LOAD"));
        (l.1 + (va - l.2)) as usize
    };

    // Check PT_TLS `.tdata` contents.
    let (tls_off, tls_va, _, _) = tls_ph.expect("PT_TLS present");
    assert_eq!(tls_va, 0x3_0080 + shift);
    assert_eq!(u64_at(tls_off as usize), 0x1122_3344_5566_7788, "PT_TLS preserves .tdata bytes");

    // Check `.dynamic` tags.
    let (dyn_off, dyn_len) = dynamic_ph.expect("PT_DYNAMIC present");
    let mut dyn_tags: AVec<'_, (u64, u64)> = Vec::new_in(alloc);
    let mut off = dyn_off as usize;
    while off < (dyn_off + dyn_len) as usize {
        let (t, v) = (u64_at(off), u64_at(off + 8));
        if t == 0 {
            break;
        }
        dyn_tags.push((t, v));
        off += 16;
    }
    let find_tag = |tag: u64| dyn_tags.iter().find(|&&(t, _)| t == tag).map(|&(_, v)| v);
    let strtab = vaddr_to_off(find_tag(5).unwrap());
    let symtab = vaddr_to_off(find_tag(6).unwrap());
    let rela_off = vaddr_to_off(find_tag(7).unwrap());
    let relasz = find_tag(8).unwrap() as usize;
    assert_eq!(find_tag(25), Some(0x3_0070 + shift), "DT_INIT_ARRAY points to shifted Inits");
    assert_eq!(find_tag(27), Some(8), "DT_INIT_ARRAYSZ == 8");
    assert_eq!(facts.init_array, Some((0x3_0070 + shift, 8)));
    assert_eq!(find_tag(0x6FFF_FFF9), Some(5), "DT_RELACOUNT == 5");
    assert_eq!(find_tag(30), Some(0x8 | 0x10), "DT_FLAGS sets DF_BIND_NOW | DF_STATIC_TLS");

    let mut needed_names: AVec<'_, &[u8]> = Vec::new_in(alloc);
    for &(t, v) in dyn_tags.iter() {
        if t == 1 {
            needed_names.push(cstr(strtab + v as usize));
        }
    }
    assert_eq!(&needed_names[..], &[&b"libc.so.6"[..], &b"libm.so.6"[..], &b"libgcc_s.so.1"[..]]);

    // Check `.dynsym` entries (index 0 is null, then kept program imports, then native imports).
    let hash_off = vaddr_to_off(find_tag(4).unwrap());
    let nsyms = u32_at(hash_off + 4) as usize;
    assert_eq!(nsyms, 5, "null + 2 kept program imports + 2 native imports");
    let mut sym_names: AVec<'_, (&[u8], u8)> = Vec::new_in(alloc);
    for s in 1..nsyms {
        let base = symtab + 24 * s;
        let name = cstr(strtab + u32_at(base) as usize);
        let bind = out1[base + 4] >> 4;
        sym_names.push((name, bind));
    }
    assert!(sym_names.iter().all(|&(n, _)| n != b"c_entry"), "c_entry defined by native image must be dropped from .dynsym");
    assert!(sym_names.iter().any(|&(n, b)| n == b"__libc_start_main" && b == 1));
    assert!(sym_names.iter().any(|&(n, b)| n == b"exit" && b == 1));
    assert!(sym_names.iter().any(|&(n, b)| n == b"puts" && b == 1));
    assert!(sym_names.iter().any(|&(n, b)| n == b"weak_hook" && b == 2), "weak_hook keeps STB_WEAK");

    // Check every RELA entry and confirm every native relocated cell is zero in the file.
    let mut relas: AVec<'_, (u64, u64, u64, i64)> = Vec::new_in(alloc);
    for r in 0..relasz / 24 {
        let base = rela_off + 24 * r;
        let r_off = u64_at(base);
        let info = u64_at(base + 8);
        let addend = u64_at(base + 16) as i64;
        relas.push((r_off, info >> 32, info & 0xFFFF_FFFF, addend));
    }
    assert_eq!(relas.len(), 11);
    // First 5 are R_AARCH64_RELATIVE (1027).
    for r in &relas[..5] {
        assert_eq!((r.1, r.2), (0, 1027), "first DT_RELACOUNT entries must be R_AARCH64_RELATIVE");
    }
    // Direct program import for `c_entry` rebases to `0x1_0000 + shift`.
    assert!(relas[..5].iter().any(|r| r.0 < loads[2].2 && r.3 as u64 == 0x1_0000 + shift), "program c_entry GOT cell rebases to native c_entry");
    // Native rebase cell 0x3_0050 + shift -> 0x1_0000 + shift.
    assert!(relas[..5].iter().any(|r| r.0 == 0x3_0050 + shift && r.3 as u64 == 0x1_0000 + shift));
    // Native Word pointer cell 0x3_0060 + shift and Words[0] 0x3_0068 + shift both rebase to the same Rust `rust_cb` address.
    let word_ptr_target = relas[..5].iter().find(|r| r.0 == 0x3_0060 + shift).unwrap().3 as u64;
    let words0_target = relas[..5].iter().find(|r| r.0 == 0x3_0068 + shift).unwrap().3 as u64;
    assert_eq!(word_ptr_target, words0_target, "FixTo::Word and Words[0] target the same Rust function");
    assert!(word_ptr_target >= elf::TEXT_AT && word_ptr_target < loads[1].2, "rust_cb sits in program text");
    // Native Inits[0] 0x3_0070 + shift -> 0x1_0004 + shift (`c_init`).
    assert!(relas[..5].iter().any(|r| r.0 == 0x3_0070 + shift && r.3 as u64 == 0x1_0004 + shift));
    // Native GLOB_DAT for puts (0x2_0000 + shift, sym 3, addend 0), TPREL64 (0x2_0008 + shift, sym 0, addend 0),
    // TLSDESC (0x2_0010 + shift, sym 0, addend 8), ABS64 for weak_hook (0x3_0058 + shift, sym 4, addend 8).
    assert!(relas[5..].iter().any(|r| r.0 == 0x2_0000 + shift && r.1 == 3 && r.2 == 1025 && r.3 == 0));
    assert!(relas[5..].iter().any(|r| r.0 == 0x2_0008 + shift && r.1 == 0 && r.2 == 1030 && r.3 == 0));
    assert!(relas[5..].iter().any(|r| r.0 == 0x2_0010 + shift && r.1 == 0 && r.2 == 1031 && r.3 == 8));
    assert!(relas[5..].iter().any(|r| r.0 == 0x3_0058 + shift && r.1 == 4 && r.2 == 257 && r.3 == 8));
    // Every relocated native cell holds 0 in the file (including both words of TLSDESC pair).
    for &va in &[0x2_0000, 0x2_0008, 0x2_0010, 0x2_0018, 0x3_0050, 0x3_0058, 0x3_0060, 0x3_0068, 0x3_0070] {
        assert_eq!(u64_at(vaddr_to_off(va + shift)), 0, "RELA slot at {va:#x} must be 0 in file");
    }

    // Check `.eh_frame_hdr` includes the native FDE for `c_init` (0x1_0004 + shift) with re-encoded pc_begin.
    let (eh_off, eh_va, eh_len) = eh_ph.expect("PT_GNU_EH_FRAME present");
    assert!(eh_len >= 20, "eh_frame_hdr has header + at least the native FDE entry");
    let fde_count = u32_at(eh_off as usize + 8) as usize;
    assert_eq!(fde_count, 1, "one FDE (the native c_init FDE)");
    let init_loc = (eh_va as i64 + i64::from(u32_at(eh_off as usize + 12) as i32)) as u64;
    let fde_va = (eh_va as i64 + i64::from(u32_at(eh_off as usize + 16) as i32)) as u64;
    assert_eq!(init_loc, 0x1_0004 + shift, "eh_frame_hdr entry points to shifted c_init");
    let fde_off = vaddr_to_off(fde_va);
    assert_eq!(u32_at(fde_off), 20, "copied native FDE length");
    let pc_begin = (fde_va + 8).wrapping_add(i64::from(u32_at(fde_off + 8) as i32) as u64);
    assert_eq!(pc_begin, 0x1_0004 + shift, "re-encoded FDE pc_begin reaches shifted c_init");
}

/// ELF `PT_TLS` coexistence: native C TLS (`.tdata` + `.tbss`) and Rust `Space::Tls` share the
/// executable's single `PT_TLS`, including when `.tdata` is 8-aligned (upgraded to 16-byte align
/// via read-only `meta` copy) and when C TLS requires 32-byte alignment (shifting `DESC.root`).
#[test]
fn elf_native_merge_coexists_with_rust_tls() {
    use crate::input::CExport;

    let alloc = blaze_own::testing::heap().alloc();
    let desc_sym = sym("TLS_DESC");
    let ender_sym = sym("tls_ender");
    let desc_target = Target { space: Space::Static, symbol: desc_sym };
    let tls_a = Target { space: Space::Tls, symbol: sym("RUST_TLS_A") };
    let val = 123u64.to_le_bytes();
    let main_rec = rec(
        &[ADRP_X0, LDR_X0, ADRP_X0, LDR_X0, MOV_W0_42, RET],
        &[(0, SiteKind::Load, desc_target), (8, SiteKind::Load, tls_a)],
        &[],
    );
    let ender_rec = rec(&[RET], &[], &[]);
    let rust_cb_rec = rec(&[MOV_W0_42, RET], &[], &[]);
    let fs = [
        Function { symbol: sym("main"), record: CodeView::read(&main_rec).unwrap(), tier: Tier::Fast },
        Function { symbol: ender_sym, record: CodeView::read(&ender_rec).unwrap(), tier: Tier::Fast },
        Function { symbol: sym("rust_cb"), record: CodeView::read(&rust_cb_rec).unwrap(), tier: Tier::Fast },
    ];
    let data = [Data { target: tls_a, bytes: &val, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly }];
    let c_exports = [CExport { name: b"rust_cb", symbol: sym("rust_cb") }];
    let tls_rt = Some(TlsRuntime { descriptor: desc_sym, ender: ender_sym });

    let inspect_root = |file: &[u8]| -> (u64, u64, u64, u64, u64) {
        let u16_at = |at: usize| u16::from_le_bytes(file[at..at + 2].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(file[at..at + 8].try_into().unwrap());
        let phoff = u64_at(32) as usize;
        let phnum = u16_at(56) as usize;
        let mut prog_data_off = None;
        let mut tls_ph = None;
        let mut load_idx = 0usize;
        for k in 0..phnum {
            let p = phoff + 56 * k;
            match u32_at(p) {
                1 => {
                    if load_idx == 2 {
                        prog_data_off = Some(u64_at(p + 8) as usize);
                    }
                    load_idx += 1;
                }
                7 => tls_ph = Some((u64_at(p + 8), u64_at(p + 16), u64_at(p + 32), u64_at(p + 40), u64_at(p + 48))),
                _ => {}
            }
        }
        let (tls_off, tls_va, filesz, memsz, align) = tls_ph.expect("single PT_TLS");
        assert_eq!(tls_off % align, 0, "PT_TLS p_offset must be p_align-aligned");
        assert_eq!(tls_va % align, 0, "PT_TLS p_vaddr must be p_align-aligned");
        if filesz == 8 {
            assert_eq!(u64_at(tls_off as usize), 0x1122_3344_5566_7788, ".tdata bytes preserved");
        }
        // `DESC` is the only writable datum in program data; `DESC.root` is at offset +8.
        let desc_root = u64_at(prog_data_off.expect("program data load") + 8);
        (filesz, memsz, align, tls_va, desc_root)
    };

    // Case 1: 8B .tdata + 8B .tbss with 8-byte aligned tdata at 0x3_0088 (not 16-aligned -> copied into meta).
    let img_unaligned = linux_native_image(alloc, true, 0x3_0088, 3);
    let prog1 = ProgramIn {
        name: "tls-coexist-16",
        functions: &fs,
        data: &data,
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: None,
        tls: tls_rt,
        profile: None,
        natives: Some(&img_unaligned),
        exports: &c_exports,
        symbol_names: &[],
    };
    let mut elf1 = Vec::new_in(alloc);
    export(&prog1, Platform::LinuxArm64, &mut elf1, alloc).unwrap();
    let facts1 = check::elf(&elf1).unwrap();
    assert_eq!(facts1.tls, Some((8, 24, 16)), "PT_TLS: 8B .tdata, 16B C TLS + 8B Rust ROOT = 24B, align 16");
    let (_, _, _, _, root1) = inspect_root(&elf1);
    assert_eq!(root1, (16 + 16) | blaze_code::tls::ROOT_OWN, "DESC.root = (p_align 16 + c_memsz 16) | ROOT_OWN");

    // Case 2: 8B .tdata + 8B .tbss with 32-byte alignment (log2 = 5 -> p_align = 32, DESC.root = 32 + 16 = 48).
    let img_align32 = linux_native_image(alloc, true, 0x3_0080, 5);
    let prog2 = ProgramIn { natives: Some(&img_align32), ..prog1 };
    let mut elf2 = Vec::new_in(alloc);
    export(&prog2, Platform::LinuxArm64, &mut elf2, alloc).unwrap();
    let facts2 = check::elf(&elf2).unwrap();
    assert_eq!(facts2.tls, Some((8, 24, 32)), "PT_TLS honors C's 32-byte alignment");
    let (_, _, _, _, root2) = inspect_root(&elf2);
    assert_eq!(root2, (32 + 16) | blaze_code::tls::ROOT_OWN, "DESC.root = (p_align 32 + c_memsz 16) | ROOT_OWN");

    // Control: native image with NO C TLS + Rust Space::Tls stays (0, 8, 16) with DESC.root = 16 | ROOT_OWN.
    let img_no_tls = linux_native_image(alloc, false, 0x3_0078, 0);
    let prog3 = ProgramIn { natives: Some(&img_no_tls), ..prog1 };
    let mut elf3 = Vec::new_in(alloc);
    export(&prog3, Platform::LinuxArm64, &mut elf3, alloc).unwrap();
    let facts3 = check::elf(&elf3).unwrap();
    assert_eq!(facts3.tls, Some((0, 8, 16)));
    let (_, _, _, _, root3) = inspect_root(&elf3);
    assert_eq!(root3, 16 | blaze_code::tls::ROOT_OWN);
}

/// ELF native merge refusals name the exact cause: platform mismatch, missing Rust word, and
/// Mach-O `__thread_vars` in a Linux image.
#[test]
fn elf_native_merge_refusals_are_named() {
    use crate::native_merge::MergeError;

    let alloc = blaze_own::testing::heap().alloc();
    let (main_rec, second_rec) = tiny();
    let fs = functions(&main_rec, &second_rec);
    let mut out = Vec::new_in(alloc);

    // 1. Missing Rust word export (`rust_cb` not in `exports`).
    let img = linux_native_image(alloc, false, 0x3_0078, 0);
    let prog = ProgramIn { natives: Some(&img), ..program(&fs, &[], None) };
    assert_eq!(
        export(&prog, Platform::LinuxArm64, &mut out, alloc),
        Err(ExportError::Native(MergeError::RustWordMissing { word: 0 }))
    );

    // 2. Wrong platform: Linux image passed to macOS export -> Platform.
    assert_eq!(
        export(&prog, Platform::MacosArm64, &mut out, alloc),
        Err(ExportError::Native(MergeError::Platform))
    );

    // 3. Mach-O ThreadVars non-empty in Linux image -> Shape.
    let mut bad_tlv = linux_native_image(alloc, false, 0x3_0078, 0);
    for r in bad_tlv.regions.iter_mut() {
        if r.region == blaze_native::image::Region::ThreadVars {
            r.len = 24;
        }
    }
    let c_exports = [crate::input::CExport { name: b"rust_cb", symbol: sym("second") }];
    let prog_bad = ProgramIn { natives: Some(&bad_tlv), exports: &c_exports, ..program(&fs, &[], None) };
    assert!(matches!(
        export(&prog_bad, Platform::LinuxArm64, &mut out, alloc),
        Err(ExportError::Native(MergeError::Shape { .. }))
    ));
}

/// Passing `symbol_names` emits local Mach-O `N_SECT` (`0x0e`) symbols in section 1
/// (`__TEXT,__text`) before undefined imports, shifts `LC_DYSYMTAB` (`iundefsym`, indirect
/// symtab rows), sanitizes embedded NUL bytes, stays deterministic across input orders, and
/// remains 100% byte-identical to an unnamed export when empty/NUL-only.
#[test]
fn macho_emits_local_n_sect_function_symbols() {
    let (main, second) = tiny();
    let fs = functions(&main, &second);
    let main_sym = sym("main");
    let second_sym = sym("second");

    // Helper: parse LC_SYMTAB and LC_DYSYMTAB from a Mach-O image.
    let parse_symtab = |file: &[u8]| {
        let u16_at = |at: usize| u16::from_le_bytes(file[at..at + 2].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(file[at..at + 8].try_into().unwrap());
        let ncmds = u32_at(16);
        let mut at = 32usize;
        let mut symtab = None;
        let mut dysymtab = None;
        for _ in 0..ncmds {
            let cmd = u32_at(at);
            let cmdsize = usize::try_from(u32_at(at + 4)).unwrap();
            match cmd {
                0x2 => {
                    symtab = Some((
                        usize::try_from(u32_at(at + 8)).unwrap(),  // symoff
                        usize::try_from(u32_at(at + 12)).unwrap(), // nsyms
                        usize::try_from(u32_at(at + 16)).unwrap(), // stroff
                        usize::try_from(u32_at(at + 20)).unwrap(), // strsize
                    ));
                }
                0xB => {
                    let mut fields = [0u32; 18];
                    for (i, slot) in fields.iter_mut().enumerate() {
                        *slot = u32_at(at + 8 + 4 * i);
                    }
                    dysymtab = Some(fields);
                }
                _ => {}
            }
            at += cmdsize;
        }
        let (symoff, nsyms, stroff, strsize) = symtab.expect("LC_SYMTAB present");
        let dysym = dysymtab.expect("LC_DYSYMTAB present");
        let nlist = |idx: usize| -> (u32, u8, u8, u16, u64) {
            let row = symoff + idx * 16;
            (u32_at(row), file[row + 4], file[row + 5], u16_at(row + 6), u64_at(row + 8))
        };
        let indirect_off = usize::try_from(dysym[12]).unwrap();
        let indirect_n = usize::try_from(dysym[13]).unwrap();
        let mut indirect = Vec::with_capacity_in(indirect_n, Alloc::global());
        for k in 0..indirect_n {
            indirect.push(u32_at(indirect_off + 4 * k));
        }
        let mut entries = Vec::with_capacity_in(nsyms, Alloc::global());
        for i in 0..nsyms {
            entries.push(nlist(i));
        }
        (symoff, nsyms, stroff, strsize, dysym, entries, indirect)
    };

    // 1. Named `main` -> single local N_SECT symbol before undefined `exit`.
    let names = [(main_sym, "my_hot_kernel")];
    let prog = ProgramIn { symbol_names: &names, ..program(&fs, &[], None) };
    let file = exported(&prog, Platform::MacosArm64);
    let facts = check::macho(&file).unwrap();
    assert_eq!((facts.binds, facts.rebases), (1, 0));

    let members: [Member<'_>; 2] = fs.map(|f| Member { symbol: f.symbol, record: f.record, tier: Some(f.tier) });
    let laid = lay_out(&members, &[], main_sym, None, macho::plan(), 0, Alloc::global()).unwrap();
    let main_placed = laid.placed.iter().find(|p| p.symbol == main_sym).unwrap();
    let second_placed = laid.placed.iter().find(|p| p.symbol == second_sym).unwrap();
    let fn_off = main_placed.at - laid.text_at;
    let expected_addr = macho::BASE + laid.text_at + fn_off;
    assert_eq!(expected_addr, macho::BASE + facts.entry);

    let (_symoff, nsyms, stroff, strsize, dysym, entries, indirect) = parse_symtab(&file);
    assert_eq!(nsyms, 2, "1 local N_SECT symbol + 1 undefined import");
    // LC_DYSYMTAB: ilocalsym=0, nlocalsym=1, iextdefsym=1, nextdefsym=0, iundefsym=1, nundefsym=1, nindirectsyms=1.
    assert_eq!(dysym[0], 0, "ilocalsym");
    assert_eq!(dysym[1], 1, "nlocalsym");
    assert_eq!(dysym[2], 1, "iextdefsym");
    assert_eq!(dysym[3], 0, "nextdefsym");
    assert_eq!(dysym[4], 1, "iundefsym");
    assert_eq!(dysym[5], 1, "nundefsym");
    assert_eq!(dysym[13], 1, "nindirectsyms");
    assert_eq!(&indirect[..], &[1u32], "indirect symtab row 0 points to undefined import at index 1");

    // Local symbol row 0: N_SECT (0x0e) in section 1 (__TEXT,__text), n_desc=0, n_value=BASE+text_at+fn_off.
    let (s0_strx, s0_type, s0_sect, s0_desc, s0_val) = entries[0];
    assert_eq!(s0_strx, 2);
    assert_eq!(s0_type, 0x0e, "N_SECT local symbol");
    assert_eq!(s0_sect, 1, "__TEXT,__text is section 1");
    assert_eq!(s0_desc, 0);
    assert_eq!(s0_val, expected_addr);

    // Undefined import row 1: N_UNDF | N_EXT (0x01) at libSystem ordinal (1 << 8).
    let (s1_strx, s1_type, s1_sect, s1_desc, s1_val) = entries[1];
    assert_eq!(s1_type, 0x01);
    assert_eq!(s1_sect, 0);
    assert_eq!(s1_desc, 1 << 8);
    assert_eq!(s1_val, 0);

    // String table starts with ` \0`, followed by `_my_hot_kernel\0` and `_exit\0`.
    let strtab = &file[stroff..stroff + strsize];
    assert_eq!(&strtab[0..2], b" \0");
    let local_start = usize::try_from(s0_strx).unwrap();
    assert_eq!(&strtab[local_start..local_start + b"_my_hot_kernel\0".len()], b"_my_hot_kernel\0");
    let import_start = usize::try_from(s1_strx).unwrap();
    assert_eq!(&strtab[import_start..import_start + b"_exit\0".len()], b"_exit\0");

    // 2. Determinism across input orders + NUL byte sanitization.
    let two_a = [(second_sym, "sec\0ond"), (main_sym, "my_hot_kernel")];
    let two_b = [(main_sym, "my_hot_kernel"), (second_sym, "sec\0ond")];
    let out_a = exported(&ProgramIn { symbol_names: &two_a, ..program(&fs, &[], None) }, Platform::MacosArm64);
    let out_b = exported(&ProgramIn { symbol_names: &two_b, ..program(&fs, &[], None) }, Platform::MacosArm64);
    assert_eq!(&out_a[..], &out_b[..], "Mach-O with local symbols must be byte-deterministic across input order");
    check::macho(&out_a).unwrap();
    let (_, nsyms2, stroff2, strsize2, dysym2, entries2, indirect2) = parse_symtab(&out_a);
    assert_eq!(nsyms2, 3);
    assert_eq!((dysym2[1], dysym2[4], dysym2[5]), (2, 2, 1));
    assert_eq!(&indirect2[..], &[2u32]);
    assert_eq!(entries2[1].4, macho::BASE + second_placed.at);
    let strtab2 = &out_a[stroff2..stroff2 + strsize2];
    let sec_start = usize::try_from(entries2[1].0).unwrap();
    assert_eq!(&strtab2[sec_start..sec_start + b"_second\0".len()], b"_second\0");

    // 3. Empty / NUL-only names produce bytes 100% identical to `symbol_names: &[]`.
    let baseline = exported(&program(&fs, &[], None), Platform::MacosArm64);
    let blank_names = [(main_sym, ""), (second_sym, "\0\0")];
    let blank = exported(&ProgramIn { symbol_names: &blank_names, ..program(&fs, &[], None) }, Platform::MacosArm64);
    assert_eq!(&baseline[..], &blank[..], "empty/NUL-only symbol_names must be byte-identical to empty");
}

/// End-to-end `Space::WeakOutside` export test (Rule 23):
/// 1. ELF emits `(STB_WEAK << 4) | STT_NOTYPE` (`0x20`) for weak-only imports and `GLOBAL_FUNC`
///    (`0x12`) for strong imports.
/// 2. Mach-O sets `weak_import = 1` in `DYLD_CHAINED_IMPORT` (bit 8) and `N_WEAK_REF` (`0x0040`)
///    in `nlist_64::n_desc` for weak-only imports, and clears both for strong imports.
/// 3. Strong overrides weak in BOTH `lay_out` and `lay_out_records`, across BOTH function orderings
///    and across Code + Data references!
#[test]
fn extern_weak_elf_macho_and_strong_overrides_weak() {
    let alloc = Alloc::global();
    let make_load_rec = |strong: &[&str], weak: &[&str]| -> AVec<'static, u8> {
        let mut r = CodeRecord::new_in(alloc);
        let mut at = 0u32;
        for &name in strong {
            for inst in [ADRP_X0, LDR_X0] {
                r.code.extend_from_slice(&inst.to_le_bytes());
            }
            let idx = r.outside(name);
            r.sites.push(Site { at, kind: SiteKind::Load, target: idx, addend: 0 });
            at += 8;
        }
        for &name in weak {
            for inst in [ADRP_X0, LDR_X0] {
                r.code.extend_from_slice(&inst.to_le_bytes());
            }
            let idx = r.weak_outside(name);
            r.sites.push(Site { at, kind: SiteKind::Load, target: idx, addend: 0 });
            at += 8;
        }
        r.code.extend_from_slice(&RET.to_le_bytes());
        let mut out = Vec::new_in(alloc);
        let Ok(sealed) = r.seal() else { panic!("record must seal") };
        sealed.encode(&mut out);
        out
    };

    // Case 1: fn A references `weak_only` (weak) + `shared` (weak);
    //         fn B references `strong_only` (strong) + `shared` (strong).
    let fn_a = make_load_rec(&[], &["weak_only", "shared"]);
    let fn_b = make_load_rec(&["strong_only", "shared"], &[]);
    let Ok(view_a) = CodeView::read(&fn_a) else { panic!("view_a must decode") };
    let Ok(view_b) = CodeView::read(&fn_b) else { panic!("view_b must decode") };

    // Verify strong-overrides-weak in BOTH function orders for `lay_out`.
    for pair in [
        [
            Member { symbol: sym("main"), record: view_a, tier: Some(Tier::Fast) },
            Member { symbol: sym("helper"), record: view_b, tier: Some(Tier::Fast) },
        ],
        [
            Member { symbol: sym("helper"), record: view_b, tier: Some(Tier::Fast) },
            Member { symbol: sym("main"), record: view_a, tier: Some(Tier::Fast) },
        ],
    ] {
        let Ok(laid) = lay_out(&pair, &[], sym("main"), None, elf::plan(), 0, alloc) else {
            panic!("lay_out must succeed");
        };
        assert_eq!(laid.imports.len(), 3);
        let is_weak = |name: &str| {
            let Some(imp) = laid.imports.iter().find(|imp| imp.name == name) else {
                panic!("missing import {name}");
            };
            imp.weak
        };
        assert!(is_weak("weak_only"), "weak-only symbol must stay weak");
        assert!(!is_weak("strong_only"), "strong-only symbol must stay strong");
        assert!(!is_weak("shared"), "strong reference must override weak reference");
    }

    // Verify strong-overrides-weak across Code (weak) + Data (strong), and Data (weak) alone.
    let zero_word = [0u8; 8];
    let data_sym_strong = sym("data_override");
    let data_sym_weak = sym("data_weak_only");
    let fn_c = make_load_rec(&[], &["data_override"]);
    let Ok(view_c) = CodeView::read(&fn_c) else { panic!("view_c must decode") };
    let words_strong = [DataWord { at: 0, target: Some(Target { space: Space::Outside, symbol: data_sym_strong }), addend: 0 }];
    let outside_strong = [(data_sym_strong, "data_override")];
    let words_weak = [DataWord { at: 0, target: Some(Target { space: Space::WeakOutside, symbol: data_sym_weak }), addend: 0 }];
    let outside_weak = [(data_sym_weak, "data_weak_only")];
    let datums = [
        Data { target: Target { space: Space::Static, symbol: sym("S_STRONG") }, bytes: &zero_word, align: 8, words: &words_strong, outside: &outside_strong, place: DataPlace::ReadOnly },
        Data { target: Target { space: Space::Static, symbol: sym("S_WEAK") }, bytes: &zero_word, align: 8, words: &words_weak, outside: &outside_weak, place: DataPlace::ReadOnly },
    ];
    let members_c = [Member { symbol: sym("main"), record: view_c, tier: Some(Tier::Fast) }];
    let Ok(laid_data) = lay_out(&members_c, &datums, sym("main"), None, elf::plan(), 0, alloc) else {
        panic!("lay_out with data must succeed");
    };
    let find_weak = |name: &str| laid_data.imports.iter().find(|i| i.name == name).map(|i| i.weak);
    assert_eq!(find_weak("data_override"), Some(false), "strong data ref overrides weak code ref");
    assert_eq!(find_weak("data_weak_only"), Some(true), "weak-only data ref stays weak");

    // Verify `optimized::lay_out_records` (bzcode10) also enforces strong-overrides-weak!
    {
        use blaze_tier::record::{Declare, DeclaredKind, Declares, read as read_v2};
        struct EmptyDeclares;
        impl Declares for EmptyDeclares {
            fn declared(&self, t: Target) -> Option<Declare<'_>> {
                match t.space {
                    Space::Func => Some(Declare { kind: DeclaredKind::Instance, item: 1, name: b"fn" }),
                    Space::Static => Some(Declare { kind: DeclaredKind::Static, item: 1, name: b"" }),
                    Space::Vtable => Some(Declare { kind: DeclaredKind::Vtable, item: 1, name: b"vt" }),
                    Space::Tls => Some(Declare { kind: DeclaredKind::ThreadLocal, item: 1, name: b"" }),
                    Space::Outside | Space::WeakOutside => None,
                }
            }
        }
        let mut v2_a = Vec::new_in(alloc);
        let mut v2_b = Vec::new_in(alloc);
        assert!(crate::optimized::v1_to_v2(view_a, &EmptyDeclares, &mut v2_a, alloc).is_ok());
        assert!(crate::optimized::v1_to_v2(view_b, &EmptyDeclares, &mut v2_b, alloc).is_ok());
        let Ok(rv_a) = read_v2(&v2_a) else { panic!("v2_a must decode") };
        let Ok(rv_b) = read_v2(&v2_b) else { panic!("v2_b must decode") };
        let m02 = [
            crate::optimized::Member02 { symbol: sym("main"), record: rv_a, tier: Some(Tier::Fast) },
            crate::optimized::Member02 { symbol: sym("helper"), record: rv_b, tier: Some(Tier::Fast) },
        ];
        let Ok(laid02) = crate::optimized::lay_out_records(&m02, &[], sym("main"), None, elf::plan(), 0, alloc) else {
            panic!("lay_out_records must succeed");
        };
        let is_weak02 = |name: &str| laid02.imports.iter().find(|i| i.name == name).map(|i| i.weak);
        assert_eq!(is_weak02("weak_only"), Some(true));
        assert_eq!(is_weak02("strong_only"), Some(false));
        assert_eq!(is_weak02("shared"), Some(false));
    }

    // Export ELF and inspect `.dynsym` `st_info` for each symbol!
    let fs = [
        Function { symbol: sym("main"), record: view_a, tier: Tier::Fast },
        Function { symbol: sym("helper"), record: view_b, tier: Tier::Fast },
    ];
    let prog = program(&fs, &[], None);
    let elf_bytes = exported(&prog, Platform::LinuxArm64);
    assert!(check::elf(&elf_bytes).is_ok(), "exported ELF with weak imports must validate");
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let u64_at = |b: &[u8], o: usize| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&b[o..o + 8]);
        u64::from_le_bytes(buf)
    };
    let e_phoff = usize::try_from(u64_at(&elf_bytes, 32)).unwrap_or(0);
    let e_phnum = usize::from(u16_at(&elf_bytes, 56));
    let mut dyn_off = 0usize;
    let mut dyn_sz = 0usize;
    let mut meta_vaddr = 0u64;
    let mut meta_off = 0u64;
    for i in 0..e_phnum {
        let p = e_phoff + i * 56;
        let p_type = u32_at(&elf_bytes, p);
        let p_off = u64_at(&elf_bytes, p + 8);
        let p_vaddr = u64_at(&elf_bytes, p + 16);
        let p_filesz = u64_at(&elf_bytes, p + 32);
        if p_type == 2 {
            // PT_DYNAMIC
            dyn_off = usize::try_from(p_off).unwrap_or(0);
            dyn_sz = usize::try_from(p_filesz).unwrap_or(0);
        } else if p_type == 1 {
            // Last PT_LOAD holds meta (dynsym/dynstr)
            meta_off = p_off;
            meta_vaddr = p_vaddr;
        }
    }
    let mut symtab_va = 0u64;
    let mut strtab_va = 0u64;
    let mut hash_va = 0u64;
    let mut cur = dyn_off;
    while cur + 16 <= dyn_off + dyn_sz {
        let tag = u64_at(&elf_bytes, cur);
        let val = u64_at(&elf_bytes, cur + 8);
        match tag {
            4 => hash_va = val,   // DT_HASH
            5 => strtab_va = val, // DT_STRTAB
            6 => symtab_va = val, // DT_SYMTAB
            0 => break,
            _ => {}
        }
        cur += 16;
    }
    let va_to_off = |va: u64| usize::try_from(va - meta_vaddr + meta_off).unwrap_or(0);
    let symtab_off = va_to_off(symtab_va);
    let strtab_off = va_to_off(strtab_va);
    let nchain = usize::try_from(u32_at(&elf_bytes, va_to_off(hash_va) + 4)).unwrap_or(0);
    let mut elf_info_weak_only = None;
    let mut elf_info_strong_only = None;
    let mut elf_info_shared = None;
    for i in 1..nchain {
        let s = symtab_off + i * 24;
        let st_name = usize::try_from(u32_at(&elf_bytes, s)).unwrap_or(0);
        let st_info = elf_bytes[s + 4];
        let start = strtab_off + st_name;
        let mut end = start;
        while end < elf_bytes.len() && elf_bytes[end] != 0 {
            end += 1;
        }
        let name = core::str::from_utf8(&elf_bytes[start..end]).unwrap_or("");
        match name {
            "weak_only" => elf_info_weak_only = Some(st_info),
            "strong_only" => elf_info_strong_only = Some(st_info),
            "shared" => elf_info_shared = Some(st_info),
            _ => {}
        }
    }
    assert_eq!(elf_info_weak_only, Some(0x20), "weak_only must be (STB_WEAK << 4) | STT_NOTYPE");
    assert_eq!(elf_info_strong_only, Some(0x12), "strong_only must be GLOBAL_FUNC");
    assert_eq!(elf_info_shared, Some(0x12), "shared (strong+weak) must be GLOBAL_FUNC");

    // Export Mach-O and inspect BOTH `DYLD_CHAINED_IMPORT` `weak_import` bit AND `nlist_64::n_desc`!
    let macho_bytes = exported(&prog, Platform::MacosArm64);
    assert!(check::macho(&macho_bytes).is_ok(), "exported Mach-O with weak imports must validate");
    let ncmds = usize::try_from(u32_at(&macho_bytes, 16)).unwrap_or(0);
    let mut off = 32usize;
    let mut fixups_off = 0usize;
    let mut symoff = 0usize;
    let mut nsyms = 0usize;
    let mut stroff = 0usize;
    for _ in 0..ncmds {
        let cmd = u32_at(&macho_bytes, off);
        let sz = usize::try_from(u32_at(&macho_bytes, off + 4)).unwrap_or(0);
        if cmd == macho::LC_DYLD_CHAINED_FIXUPS {
            fixups_off = usize::try_from(u32_at(&macho_bytes, off + 8)).unwrap_or(0);
        } else if cmd == macho::LC_SYMTAB {
            symoff = usize::try_from(u32_at(&macho_bytes, off + 8)).unwrap_or(0);
            nsyms = usize::try_from(u32_at(&macho_bytes, off + 12)).unwrap_or(0);
            stroff = usize::try_from(u32_at(&macho_bytes, off + 16)).unwrap_or(0);
        }
        off += sz;
    }
    let imports_off = fixups_off + usize::try_from(u32_at(&macho_bytes, fixups_off + 8)).unwrap_or(0);
    let symbols_off = fixups_off + usize::try_from(u32_at(&macho_bytes, fixups_off + 12)).unwrap_or(0);
    let imports_count = usize::try_from(u32_at(&macho_bytes, fixups_off + 16)).unwrap_or(0);
    assert_eq!(imports_count, 3);
    let mut chain_weak_only = None;
    let mut chain_strong_only = None;
    let mut chain_shared = None;
    for i in 0..imports_count {
        let word = u32_at(&macho_bytes, imports_off + i * 4);
        let weak_bit = ((word >> 8) & 1) != 0;
        let name_off = usize::try_from(word >> 9).unwrap_or(0);
        let start = symbols_off + name_off;
        let mut end = start;
        while end < macho_bytes.len() && macho_bytes[end] != 0 {
            end += 1;
        }
        let name = core::str::from_utf8(&macho_bytes[start..end]).unwrap_or("");
        match name {
            "_weak_only" => chain_weak_only = Some(weak_bit),
            "_strong_only" => chain_strong_only = Some(weak_bit),
            "_shared" => chain_shared = Some(weak_bit),
            _ => {}
        }
    }
    assert_eq!(chain_weak_only, Some(true), "Mach-O chained import weak_only must have weak_import=1");
    assert_eq!(chain_strong_only, Some(false), "Mach-O chained import strong_only must have weak_import=0");
    assert_eq!(chain_shared, Some(false), "Mach-O chained import shared must have weak_import=0");

    // Also verify `nlist_64` `N_WEAK_REF` (`0x0040`) matches!
    let mut nlist_weak_only = None;
    let mut nlist_strong_only = None;
    let mut nlist_shared = None;
    for i in 0..nsyms {
        let row = symoff + i * 16;
        let strx = usize::try_from(u32_at(&macho_bytes, row)).unwrap_or(0);
        let n_desc = u16_at(&macho_bytes, row + 6);
        let start = stroff + strx;
        let mut end = start;
        while end < macho_bytes.len() && macho_bytes[end] != 0 {
            end += 1;
        }
        let name = core::str::from_utf8(&macho_bytes[start..end]).unwrap_or("");
        let has_weak_ref = (n_desc & 0x0040) != 0;
        match name {
            "_weak_only" => nlist_weak_only = Some(has_weak_ref),
            "_strong_only" => nlist_strong_only = Some(has_weak_ref),
            "_shared" => nlist_shared = Some(has_weak_ref),
            _ => {}
        }
    }
    assert_eq!(nlist_weak_only, Some(true));
    assert_eq!(nlist_strong_only, Some(false));
    assert_eq!(nlist_shared, Some(false));
}

#[test]
fn unused_strong_outside_target_does_not_override_weak_import() {
    let alloc = Alloc::global();
    // Build a record with a USED `WeakOutside("opt_sym")` load site and an UNUSED strong
    // `Outside("opt_sym")` target retained in the target table by `CodeRecord::seal`.
    let make_rec = |use_strong: bool| {
        let mut r = CodeRecord::new_in(alloc);
        let code = if use_strong {
            &[STP, MOV_FP, ADRP_X0, LDR_X0, ADRP_X16, LDR_X16, LDP, RET][..]
        } else {
            &[STP, MOV_FP, ADRP_X0, LDR_X0, LDP, RET][..]
        };
        for &inst in code {
            r.code.extend_from_slice(&inst.to_le_bytes());
        }
        let weak_idx = r.weak_outside("opt_sym");
        let strong_idx = r.outside("opt_sym");
        r.sites.push(Site { at: 8, kind: SiteKind::Load, target: weak_idx, addend: 0 });
        if use_strong {
            r.sites.push(Site { at: 16, kind: SiteKind::Load, target: strong_idx, addend: 0 });
        }
        let mut out = Vec::new_in(alloc);
        r.seal().unwrap().encode(&mut out);
        out
    };

    let only_weak_used = make_rec(false);
    let laid_weak = lay_out(
        &[Member { symbol: sym("main"), record: CodeView::read(&only_weak_used).unwrap(), tier: Some(Tier::Fast) }],
        &[],
        sym("main"),
        None,
        elf::plan(),
        0,
        alloc,
    )
    .expect("layout with unused strong target should succeed");
    assert_eq!(laid_weak.imports.len(), 1);
    assert_eq!(laid_weak.imports[0].name, "opt_sym");
    assert!(
        laid_weak.imports[0].weak,
        "unused strong target must not override a referenced weak import",
    );

    // Control: once a site actually references the strong target, strong overrides weak.
    let both_used = make_rec(true);
    let laid_strong = lay_out(
        &[Member { symbol: sym("main"), record: CodeView::read(&both_used).unwrap(), tier: Some(Tier::Fast) }],
        &[],
        sym("main"),
        None,
        elf::plan(),
        0,
        alloc,
    )
    .expect("layout with both strong and weak used should succeed");
    assert_eq!(laid_strong.imports.len(), 1);
    assert_eq!(laid_strong.imports[0].name, "opt_sym");
    assert!(!laid_strong.imports[0].weak, "control: referenced strong target must override weak");
}

#[test]
fn macho_refuses_out_of_range_chained_import_index() {
    let alloc = Alloc::global();
    let (main, second) = tiny();
    let members = [
        Member { symbol: sym("main"), record: CodeView::read(&main).unwrap(), tier: Some(Tier::Fast) },
        Member { symbol: sym("second"), record: CodeView::read(&second).unwrap(), tier: Some(Tier::Fast) },
    ];
    let mut laid = lay_out(&members, &[], sym("main"), None, macho::plan(), 0x1000, alloc).unwrap();
    let eh = crate::eh_out::eh_sections(&laid, &members, None, Platform::MacosArm64, alloc).unwrap();
    let mut out = Vec::new_in(alloc);
    // Control: valid laid-out program writes cleanly.
    macho::write(&laid, &eh, "valid", &[0u8; 32], &mut out, alloc).expect("valid macho write");

    // Corrupt a Bind fixup to point past `laid.imports.len()`.
    let bind = laid
        .fixups
        .iter_mut()
        .find(|f| matches!(f.to, FixTo::Bind { .. }))
        .expect("tiny program must have a Bind fixup");
    bind.to = FixTo::Bind { import: 99 };
    out.clear();
    let err = macho::write(&laid, &eh, "invalid", &[0u8; 32], &mut out, alloc)
        .expect_err("out-of-range Bind import index must fail with typed ExportError");
    assert!(
        matches!(err, ExportError::TooLarge { len: 99, .. }),
        "expected ExportError::TooLarge for index 99, got {err:?}",
    );

    // Also test `macho::chained_fixups` directly with an out-of-range `Chain::Bind`.
    let segs = [macho::ChainSeg { vm: 0x4000, vm_size: 0x4000, fixups: true }];
    let fixups = [(0x4000u64, macho::Chain::Bind { import: 1 })];
    let imports = [macho::ChainImport { lead: b"_", name: b"exit", ordinal: 1, weak: false, addend: 0 }];
    let chain_err = macho::chained_fixups(
        &fixups,
        |c: &(u64, macho::Chain)| *c,
        &segs,
        macho::ImportRows::Plain,
        &imports,
        alloc,
    )
    .expect_err("chained_fixups must reject out-of-range bind ordinal");
    assert_eq!(chain_err.len, 1);
    assert_eq!(chain_err.at, Some(0x4000));
}
