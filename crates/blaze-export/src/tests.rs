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
    ProgramIn { name: "tiny", functions, data, entry: Entry::CMain { main: sym("main") }, panic: Panic::Unwind, personality: None, tls: None, profile, natives: None, exports: &[] }
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
        Data { target: st, bytes: &answer, align: 8, words: &[], place: DataPlace::ReadOnly },
        Data { target: vt, bytes: &table, align: 8, words: &words, place: DataPlace::ReadOnly },
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
    assert_eq!(checked.func_count, 1);
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
        Data { target: st_target, bytes: &shared_bytes, align: 8, words: &[], place: DataPlace::ReadOnly },
        Data { target: tls_init, bytes: &init_bytes, align: 8, words: &[], place: DataPlace::ReadOnly },
        Data { target: tls_zero, bytes: &[], align: 16, words: &[], place: DataPlace::Zeroed { len: 16 } },
        Data { target: tls_ptr, bytes: &ptr_bytes, align: 8, words: &ptr_words, place: DataPlace::ReadOnly },
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
    let tls_datum = [Data { target: tls_a, bytes: &val, align: 8, words: &[], place: DataPlace::ReadOnly }];
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
        Data { target: tls_a, bytes: &val, align: 8, words: &[], place: DataPlace::ReadOnly },
        Data { target: bad_st, bytes: &val, align: 8, words: &bad_words, place: DataPlace::ReadOnly },
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
    let collide_data = [Data { target: desc_target, bytes: &val, align: 8, words: &[], place: DataPlace::ReadOnly }];
    let fs_plain_ender = [
        Function { symbol: sym("main"), record: CodeView::read(&main_plain).unwrap(), tier: Tier::Fast },
        Function { symbol: ender_sym, record: CodeView::read(&ender).unwrap(), tier: Tier::Fast },
    ];
    let p = ProgramIn { functions: &fs_plain_ender, data: &collide_data, tls: rt, ..p };
    assert_eq!(export(&p, Platform::MacosArm64, &mut out, alloc), Err(ExportError::TwoData { target: desc_target }));
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
    let data = [Data { target: ro, bytes: &magic, align: 8, words: &[], place: DataPlace::ReadOnly }];
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
