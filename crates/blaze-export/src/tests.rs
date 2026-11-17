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
use crate::input::{Data, DataPlace, DataWord, Entry, Function, Panic, Platform, Profile, ProgramIn, Tier};
use crate::layout::{FixTo, Member, lay_out};
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
    ProgramIn { name: "tiny", functions, data, entry: Entry::CMain { main: sym("main") }, panic: Panic::Unwind, personality: None, profile, natives: None, exports: &[] }
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
