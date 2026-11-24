//! The tests of export at the optimized tier (optimized.rs): today's export programs, their
//! records brought into `bzcode02` by `v1_to_v2` (blaze-tier's `from_v1`), written byte for byte
//! as `export` writes them (whose file the smoke example runs: exit status 42); a `bzcode02`
//! record whose calls are `Branch26` placed with direct `bl`s and one stub for a C name; a poll
//! removed with its page left unbound. Each test prints the line its doc comment names.

use blaze_code::a64::{adrp_ldr_target, bl_target, read32};
use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::record::{Declare, DeclaredKind, Declares, Emitted, LoopHeader, Poll, Reloc, RelocKind, declare, encode, read, seal};

use crate::check;
use crate::export::export;
use crate::input::{Data, DataPlace, DataWord, Entry, Function, Panic, Platform, Profile, ProgramIn, Tier, TlsRuntime};
use crate::macho;
use crate::optimized::{Function02, Member02, ProgramIn02, export_optimized, lay_out_records, v1_to_v2};

const STP: u32 = 0xA9BF_7BFD;
const MOV_FP: u32 = 0x9100_03FD;
const LDP: u32 = 0xA8C1_7BFD;
const RET: u32 = 0xD65F_03C0;
const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const ADRP_X0: u32 = 0x9000_0000;
const LDR_X0: u32 = 0xF940_0000;
const LDR_W0_X0: u32 = 0xB940_0000;
const MOV_W0_42: u32 = 0x5280_0540;
const NOP: u32 = 0xD503_201F;
const BL_0: u32 = 0x9400_0000;
const LDR_XZR_X16: u32 = 0xF940_021F;
const BR_X16: u32 = 0xD61F_0200;

fn sym(name: &str) -> Symbol {
    Symbol::of_name(name)
}

fn func(name: &str) -> Target {
    Target { space: Space::Func, symbol: sym(name) }
}

/// A sealed, encoded `bzcode01` record (as tests.rs builds them).
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

/// These tests' declarer: each in-program target declared by its space (the export reads no
/// declared row; the section is blaze-tier declared_tests.rs's subject).
struct BySpace;

impl Declares for BySpace {
    fn declared(&self, t: Target) -> Option<Declare<'_>> {
        let d = |kind, name: &'static [u8]| Some(Declare { kind, item: 1, name });
        match t.space {
            Space::Func => d(DeclaredKind::Instance, b"an instance"),
            Space::Static => d(DeclaredKind::Static, b""),
            Space::Vtable => d(DeclaredKind::Vtable, b"a vtable"),
            Space::Tls => d(DeclaredKind::ThreadLocal, b""),
            Space::Outside => None,
        }
    }
}

/// `v1`'s record in `bzcode03`, encoded.
fn v2(v1: &[u8]) -> AVec<'static, u8> {
    let alloc = Alloc::global();
    let mut out = Vec::new_in(alloc);
    v1_to_v2(CodeView::read(v1).unwrap(), &BySpace, &mut out, alloc).unwrap();
    out
}

/// A sealed, encoded `bzcode03` record of `e`.
fn sealed(e: Emitted<'static>) -> AVec<'static, u8> {
    let mut e = e;
    declare(&mut e, &BySpace).unwrap();
    let mut out = Vec::new_in(Alloc::global());
    encode(&seal(e).unwrap(), &mut out);
    out
}

fn code_of(e: &mut Emitted<'_>, code: &[u32]) {
    for &inst in code {
        e.code.extend_from_slice(&inst.to_le_bytes());
    }
}

/// Exports `v1` records both ways: today's `export`, and `export_optimized` over the same records
/// converted. Returns (today's bytes, the optimized path's bytes).
fn both(named: &[(&str, &[u8])], data: &[Data<'_>], platform: Platform) -> (AVec<'static, u8>, AVec<'static, u8>) {
    let alloc = Alloc::global();
    let mut v1s: AVec<'_, Function<'_>> = Vec::new_in(alloc);
    for &(name, bytes) in named {
        v1s.push(Function { symbol: sym(name), record: CodeView::read(bytes).unwrap(), tier: Tier::Fast });
    }
    let program = ProgramIn { name: "tiny", functions: &v1s, data, entry: Entry::CMain { main: sym("main") }, panic: Panic::Unwind, personality: None, tls: None, profile: None, natives: None, exports: &[] };
    let mut today = Vec::new_in(alloc);
    export(&program, platform, &mut today, alloc).unwrap();
    let mut converted: AVec<'_, AVec<'static, u8>> = Vec::new_in(alloc);
    for &(_, bytes) in named {
        converted.push(v2(bytes));
    }
    let mut v2s: AVec<'_, Function02<'_>> = Vec::new_in(alloc);
    for (k, &(name, _)) in named.iter().enumerate() {
        v2s.push(Function02 { symbol: sym(name), record: read(&converted[k]).unwrap(), tier: Tier::Fast });
    }
    let program = ProgramIn02 { name: "tiny", functions: &v2s, data, entry: Entry::CMain { main: sym("main") }, panic: Panic::Unwind, personality: None, tls: None, profile: None };
    let mut optimized = Vec::new_in(alloc);
    export_optimized(&program, platform, &mut optimized, alloc).unwrap();
    (today, optimized)
}

/// Today's export programs (tests.rs's tiny program on both platforms, its static-and-vtable
/// program on macOS), their records converted to `bzcode02`: the optimized path writes the same
/// file byte for byte, so it runs as today's does (export_smoke: "exported file exited with
/// status 42"). A wrong relaxation (an `adrp` left where `export` writes a `nop`, a load made a
/// call), a lost stub or GOT word, or a different order fails the equality. Prints
/// `optimized: today's programs from bzcode02 records: macho <N> bytes, elf <M> bytes, data <D> bytes, each equal to export's`.
#[test]
fn optimized_equals_export_for_converted_records() {
    let main = rec(
        &[STP, MOV_FP, ADRP_X16, LDR_X16, BLR_X16, ADRP_X16, LDR_X16, BLR_X16, LDP, RET],
        &[(8, SiteKind::Call, func("second"))],
        &[(20, "exit")],
    );
    let second = rec(&[MOV_W0_42, RET], &[], &[]);
    let tiny: [(&str, &[u8]); 2] = [("main", &main), ("second", &second)];
    let (a, b) = both(&tiny, &[], Platform::MacosArm64);
    assert_eq!(&a[..], &b[..], "the Mach-O files differ");
    check::macho(&b).unwrap();
    let (c, d) = both(&tiny, &[], Platform::LinuxArm64);
    assert_eq!(&c[..], &d[..], "the ELF files differ");
    check::elf(&d).unwrap();

    let st = Target { space: Space::Static, symbol: sym("ANSWER") };
    let vt = Target { space: Space::Vtable, symbol: sym("VTABLE") };
    let reader = rec(&[ADRP_X0, LDR_X0, LDR_W0_X0, RET], &[(0, SiteKind::Load, st)], &[]);
    let answer = 42u64.to_le_bytes();
    let table = [0u8; 8];
    let words = [DataWord { at: 0, target: Some(func("main")), addend: 0 }];
    let data = [
        Data { target: st, bytes: &answer, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly },
        Data { target: vt, bytes: &table, align: 8, words: &words, outside: &[], place: DataPlace::ReadOnly },
    ];
    let (e, f) = both(&[("main", &reader)], &data, Platform::MacosArm64);
    assert_eq!(&e[..], &f[..], "the Mach-O files with data differ");
    println!(
        "optimized: today's programs from bzcode02 records: macho {} bytes, elf {} bytes, data {} bytes, each equal to export's",
        b.len(),
        d.len(),
        f.len()
    );
}

/// A `bzcode02` record as the optimized tier makes it: `main` calls `second` and the C library's
/// `exit` by `bl` (`Branch26`). `second` is reached by a direct `bl`; `exit` by a `bl` to its one
/// stub, whose GOT pair reads `exit`'s word; the file passes the Mach-O checker with one bind.
/// Without the stub the `bl` would have no address to reach. Prints
/// `optimized: main calls second by bl, exit by bl to its stub at <S>, which reads the GOT word at <G>; 1 bind`.
#[test]
fn branch_calls_are_direct_and_c_names_get_one_stub() {
    let alloc = Alloc::global();
    let mut m = Emitted::new_in(alloc);
    code_of(&mut m, &[STP, MOV_FP, BL_0, BL_0, LDP, RET]);
    m.targets.push(func("second"));
    m.targets.push(Target { space: Space::Outside, symbol: sym("exit") });
    m.relocs.push(Reloc { at: 8, in_relro: false, kind: RelocKind::Branch26, target: 0, addend: 0 });
    m.relocs.push(Reloc { at: 12, in_relro: false, kind: RelocKind::Branch26, target: 1, addend: 0 });
    m.text.extend_from_slice(b"exit");
    m.names.push((sym("exit"), 0, 4));
    let main = sealed(m);
    let mut s = Emitted::new_in(alloc);
    code_of(&mut s, &[MOV_W0_42, RET]);
    let second = sealed(s);
    let members = [
        Member02 { symbol: sym("main"), record: read(&main).unwrap(), tier: Some(Tier::Optimized) },
        Member02 { symbol: sym("second"), record: read(&second).unwrap(), tier: Some(Tier::Optimized) },
    ];
    let laid = lay_out_records(&members, &[], sym("main"), None, macho::plan(), 0, alloc).unwrap();
    let home = |name: &str| laid.placed.iter().find(|p| p.symbol == sym(name)).unwrap().at;
    let (mh, sh) = (home("main"), home("second"));
    let at = |pc: u64| read32(&laid.text, pc - laid.text_at);
    assert_eq!(bl_target(at(mh + 8), mh + 8), Some(sh));
    let stub = bl_target(at(mh + 12), mh + 12).unwrap();
    assert_eq!(stub % 16, 0);
    assert_eq!(adrp_ldr_target(at(stub), at(stub + 4), stub), Some(laid.got_at));
    assert_eq!(at(stub + 8), BR_X16);
    assert_eq!(laid.imports.len(), 1);
    assert_eq!(laid.imports[0].name, "exit");

    let fs = [
        Function02 { symbol: sym("main"), record: read(&main).unwrap(), tier: Tier::Optimized },
        Function02 { symbol: sym("second"), record: read(&second).unwrap(), tier: Tier::Optimized },
    ];
    let program = ProgramIn02 { name: "tiny", functions: &fs, data: &[], entry: Entry::CMain { main: sym("main") }, panic: Panic::Unwind, personality: None, tls: None, profile: None };
    let mut file = Vec::new_in(alloc);
    let summary = export_optimized(&program, Platform::MacosArm64, &mut file, alloc).unwrap();
    assert_eq!((summary.optimized, summary.fast), (2, 0));
    let facts = check::macho(&file).unwrap();
    assert_eq!(facts.binds, 1);
    println!(
        "optimized: main calls second by bl, exit by bl to its stub at {stub:#x}, which reads the GOT word at {:#x}; {} bind",
        laid.got_at, facts.binds
    );
}

/// A loop's poll (`adrp x16, poll@PAGE; ldr xzr, [x16, poll@PAGEOFF]`, its relocations naming
/// the runtime's poll page as a C name) becomes two `nop`s and the page is not bound: an exported
/// file never tiers up. Kept, the poll would name a page no loader can bind. Prints
/// `optimized: the poll is two nops and its page is not bound (0 imports)`.
#[test]
fn polls_are_removed_and_their_page_is_not_bound() {
    let alloc = Alloc::global();
    let mut m = Emitted::new_in(alloc);
    code_of(&mut m, &[MOV_W0_42, ADRP_X16, LDR_XZR_X16, RET]);
    m.targets.push(Target { space: Space::Outside, symbol: sym("blaze_poll_page") });
    m.relocs.push(Reloc { at: 4, in_relro: false, kind: RelocKind::Page21, target: 0, addend: 0 });
    m.relocs.push(Reloc { at: 8, in_relro: false, kind: RelocKind::PageOff12 { scale: 3 }, target: 0, addend: 0 });
    m.loops.push(LoopHeader { at: 0, first: 0, count: 0 });
    m.polls.push(Poll { at: 4, header: 0 });
    m.text.extend_from_slice(b"blaze_poll_page");
    m.names.push((sym("blaze_poll_page"), 0, 15));
    let main = sealed(m);
    let members = [Member02 { symbol: sym("main"), record: read(&main).unwrap(), tier: Some(Tier::Optimized) }];
    let laid = lay_out_records(&members, &[], sym("main"), None, macho::plan(), 0, alloc).unwrap();
    let at = |pc: u64| read32(&laid.text, pc - laid.text_at);
    assert_eq!((at(laid.entry + 4), at(laid.entry + 8)), (NOP, NOP));
    assert_eq!(laid.imports.len(), 0);
    println!("optimized: the poll is two nops and its page is not bound ({} imports)", laid.imports.len());
}

/// Rust `Space::Tls` exported from `bzcode02` records via `export_optimized` matches `export`
/// byte-for-byte on both Mach-O arm64 and ELF arm64 (initialized + zeroed TLS + relro word).
#[test]
fn optimized_equals_export_with_rust_tls() {
    let alloc = blaze_own::testing::heap().alloc();
    let desc_sym = sym("TLS_DESC");
    let ender_sym = sym("tls_ender");
    let desc_target = Target { space: Space::Static, symbol: desc_sym };
    let tls_init = Target { space: Space::Tls, symbol: sym("TLS_INIT") };
    let tls_zero = Target { space: Space::Tls, symbol: sym("TLS_ZERO") };
    let main_v1 = {
        let mut r = CodeRecord::new_in(alloc);
        for inst in [ADRP_X0, LDR_X0, ADRP_X0, LDR_X0, ADRP_X0, LDR_X0, MOV_W0_42, RET] {
            r.code.extend_from_slice(&inst.to_le_bytes());
        }
        r.relro.resize(8, 0);
        let i_desc = r.target(desc_target);
        let i_init = r.target(tls_init);
        let i_zero = r.target(tls_zero);
        r.sites.push(Site { at: 0, kind: SiteKind::Load, target: i_desc, addend: 0 });
        r.sites.push(Site { at: 8, kind: SiteKind::Load, target: i_init, addend: 0 });
        r.sites.push(Site { at: 16, kind: SiteKind::Load, target: i_zero, addend: 0 });
        r.words.push(blaze_code::record::Word { at: 0, target: i_init, addend: 3 });
        let mut out = Vec::new_in(alloc);
        r.seal().unwrap().encode(&mut out);
        out
    };
    let ender_v1 = rec(&[RET], &[], &[]);
    let main_v2 = v2(&main_v1);
    let ender_v2 = v2(&ender_v1);
    let init_bytes = 40u64.to_le_bytes();
    let data = [
        Data { target: tls_init, bytes: &init_bytes, align: 8, words: &[], outside: &[], place: DataPlace::ReadOnly },
        Data { target: tls_zero, bytes: &[], align: 16, words: &[], outside: &[], place: DataPlace::Zeroed { len: 16 } },
    ];
    let tls = Some(TlsRuntime { descriptor: desc_sym, ender: ender_sym });
    let v1_funcs = [
        Function { symbol: sym("main"), record: CodeView::read(&main_v1).unwrap(), tier: Tier::Fast },
        Function { symbol: ender_sym, record: CodeView::read(&ender_v1).unwrap(), tier: Tier::Fast },
    ];
    let v2_funcs = [
        Function02 { symbol: sym("main"), record: read(&main_v2).unwrap(), tier: Tier::Fast },
        Function02 { symbol: ender_sym, record: read(&ender_v2).unwrap(), tier: Tier::Fast },
    ];
    let prog_v1 = ProgramIn {
        name: "tls-opt",
        functions: &v1_funcs,
        data: &data,
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: None,
        tls,
        profile: None,
        natives: None,
        exports: &[],
    };
    let prog_v2 = ProgramIn02 {
        name: "tls-opt",
        functions: &v2_funcs,
        data: &data,
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: None,
        tls,
        profile: None,
    };
    for platform in [Platform::MacosArm64, Platform::LinuxArm64] {
        let mut a = Vec::new_in(alloc);
        let mut b = Vec::new_in(alloc);
        export(&prog_v1, platform, &mut a, alloc).unwrap();
        export_optimized(&prog_v2, platform, &mut b, alloc).unwrap();
        assert_eq!(&a[..], &b[..], "export and export_optimized must match byte-for-byte with Rust TLS on {platform:?}");
        match platform {
            Platform::MacosArm64 => {
                let facts = check::macho(&b).unwrap();
                assert!(matches!(facts.mod_init, Some((_, 8))));
            }
            Platform::LinuxArm64 => {
                let facts = check::elf(&b).unwrap();
                assert_eq!(facts.tls, Some((0, 8, 16)));
            }
            Platform::Wasm64 => {}
        }
    }
}

/// PGO C3 call-chain clustering + hot/cold partitioning + tail-call `b` (`Branch26`) patching:
/// given hot profile `[hot_a, hot_b, hot_c]` where `hot_a` tail-calls `hot_c` directly via `b`
/// (`0x1400_0000`, `Branch26`), `lay_out_records` clusters `hot_c` immediately after `hot_a`
/// (before unrelated `hot_b`), partitions all hot functions before cold `main`/`cold_err`, and
/// patches the `b` tail-call displacement to `hot_c`'s exact address in valid Mach-O & ELF outputs.
#[test]
fn pgo_c3_hot_chain_clustering_and_tail_call_branch26() {
    const B_0: u32 = 0x1400_0000;
    const MOV_W0_1: u32 = 0x5280_0020;
    const MOV_W0_2: u32 = 0x5280_0040;
    const MOV_W0_3: u32 = 0x5280_0060;
    let alloc = Alloc::global();

    // `main` (cold entry) calls `hot_a` and `cold_err`.
    let mut main_e = Emitted::new_in(alloc);
    code_of(&mut main_e, &[STP, MOV_FP, BL_0, BL_0, LDP, RET]);
    main_e.targets.push(func("hot_a"));
    main_e.targets.push(func("cold_err"));
    main_e.relocs.push(Reloc { at: 8, in_relro: false, kind: RelocKind::Branch26, target: 0, addend: 0 });
    main_e.relocs.push(Reloc { at: 12, in_relro: false, kind: RelocKind::Branch26, target: 1, addend: 0 });
    let main = sealed(main_e);

    // `hot_a` tail-calls `hot_c` via direct `b` (`Branch26`).
    let mut hot_a_e = Emitted::new_in(alloc);
    code_of(&mut hot_a_e, &[B_0]);
    hot_a_e.targets.push(func("hot_c"));
    hot_a_e.relocs.push(Reloc { at: 0, in_relro: false, kind: RelocKind::Branch26, target: 0, addend: 0 });
    let hot_a = sealed(hot_a_e);

    let mut hot_b_e = Emitted::new_in(alloc);
    code_of(&mut hot_b_e, &[MOV_W0_1, RET]);
    let hot_b = sealed(hot_b_e);

    let mut hot_c_e = Emitted::new_in(alloc);
    code_of(&mut hot_c_e, &[MOV_W0_2, RET]);
    let hot_c = sealed(hot_c_e);

    let mut cold_e = Emitted::new_in(alloc);
    code_of(&mut cold_e, &[MOV_W0_3, RET]);
    let cold_err = sealed(cold_e);

    let members = [
        Member02 { symbol: sym("main"), record: read(&main).unwrap(), tier: Some(Tier::Optimized) },
        Member02 { symbol: sym("hot_a"), record: read(&hot_a).unwrap(), tier: Some(Tier::Optimized) },
        Member02 { symbol: sym("hot_b"), record: read(&hot_b).unwrap(), tier: Some(Tier::Optimized) },
        Member02 { symbol: sym("hot_c"), record: read(&hot_c).unwrap(), tier: Some(Tier::Optimized) },
        Member02 { symbol: sym("cold_err"), record: read(&cold_err).unwrap(), tier: Some(Tier::Optimized) },
    ];
    let hot = [sym("hot_a"), sym("hot_b"), sym("hot_c")];
    let profile = Profile { digest: [0x5A; 32], hot: &hot };
    let laid = lay_out_records(&members, &[], sym("main"), Some(profile), macho::plan(), 0, alloc).unwrap();
    let syms: Vec<Symbol, _> = {
        let mut v = Vec::new_in(alloc);
        for p in laid.placed.iter() {
            v.push(p.symbol);
        }
        v
    };
    // C3 clusters `hot_a -> hot_c` tightly before unrelated `hot_b`, and all hot before cold!
    assert_eq!(syms[0], sym("hot_a"));
    assert_eq!(syms[1], sym("hot_c"));
    assert_eq!(syms[2], sym("hot_b"));
    assert!(syms[3] == sym("main") || syms[3] == sym("cold_err"));
    assert!(syms[4] == sym("main") || syms[4] == sym("cold_err"));

    // Verify the `b` tail-call instruction in `hot_a` reaches `hot_c` directly!
    let ha_at = laid.placed[0].at;
    let hc_at = laid.placed[1].at;
    let b_word = read32(&laid.text, ha_at - laid.text_at);
    assert_eq!(b_word >> 26, 0b000101, "tail call must remain an unconditional `b` (not `bl`)");
    let imm26 = (((b_word & 0x03FF_FFFF) as i32) << 6) >> 6;
    assert_eq!((ha_at as i64 + i64::from(imm26) * 4) as u64, hc_at);

    // Export both Mach-O and ELF images with this PGO profile and check both!
    let fs = [
        Function02 { symbol: sym("main"), record: read(&main).unwrap(), tier: Tier::Optimized },
        Function02 { symbol: sym("hot_a"), record: read(&hot_a).unwrap(), tier: Tier::Optimized },
        Function02 { symbol: sym("hot_b"), record: read(&hot_b).unwrap(), tier: Tier::Optimized },
        Function02 { symbol: sym("hot_c"), record: read(&hot_c).unwrap(), tier: Tier::Optimized },
        Function02 { symbol: sym("cold_err"), record: read(&cold_err).unwrap(), tier: Tier::Optimized },
    ];
    let program = ProgramIn02 {
        name: "pgo_c3",
        functions: &fs,
        data: &[],
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Abort,
        personality: None,
        tls: None,
        profile: Some(profile),
    };
    let mut macho_out = Vec::new_in(alloc);
    export_optimized(&program, Platform::MacosArm64, &mut macho_out, alloc).unwrap();
    check::macho(&macho_out).unwrap();
    let mut elf_out = Vec::new_in(alloc);
    export_optimized(&program, Platform::LinuxArm64, &mut elf_out, alloc).unwrap();
    check::elf(&elf_out).unwrap();
}
