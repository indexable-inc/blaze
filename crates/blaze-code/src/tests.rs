//! One test per piece: the record's canonical form, its refusal, the bundle's relaxed call,
//! folding, and an edit's patch. Each prints the line its doc comment names.

use blaze_own::{AVec, Alloc, Vec};

use crate::image::{Image, Kind, Member, Write, lay_out};
use crate::record::{CodeRecord, CodeView, RecordError, Site, SiteKind, Space, Symbol, Target};

const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const RET: u32 = 0xD65F_03C0;
const NOP: u32 = 0xD503_201F;
const PAGE: u64 = 16384;

fn func(name: &str) -> Target {
    Target { space: Space::Func, symbol: Symbol::of_name(name) }
}

/// A record of `code`, whose call sites (`adrp; ldr; blr` at the offset given) name `calls`,
/// with targets added in the order given. Encoded.
fn record(code: &[u32], calls: &[(u32, Target)], alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for &inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    for &(at, target) in calls {
        let index = r.target(target);
        r.sites.push(Site { at, kind: SiteKind::Call, target: index, addend: 0 });
    }
    let sealed = r.seal().unwrap();
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

fn inst(code: &[u8], at: u64) -> u32 {
    let a = at as usize;
    u32::from_le_bytes([code[a], code[a + 1], code[a + 2], code[a + 3]])
}

/// Prints `record: 2 targets, 2 sites, equal bytes from either order`.
#[test]
fn record_is_canonical() {
    let alloc = blaze_own::testing::heap().alloc();
    let code = [ADRP_X16, LDR_X16, BLR_X16, ADRP_X16, LDR_X16, BLR_X16, RET];
    let one = record(&code, &[(0, func("g")), (12, func("h"))], alloc);
    // The same calls, the targets added the other way round.
    let mut r = CodeRecord::new_in(alloc);
    for &i in &code {
        r.code.extend_from_slice(&i.to_le_bytes());
    }
    let h = r.target(func("h"));
    let g = r.target(func("g"));
    r.sites.push(Site { at: 12, kind: SiteKind::Call, target: h, addend: 0 });
    r.sites.push(Site { at: 0, kind: SiteKind::Call, target: g, addend: 0 });
    let mut two = Vec::new_in(alloc);
    r.seal().unwrap().encode(&mut two);
    assert_eq!(&one[..], &two[..]);
    let view = CodeView::read(&one).unwrap();
    assert_eq!(view.targets(), 2);
    assert_eq!(view.sites(), 2);
    assert_eq!(view.target(view.site(0).target as usize), func("g"));
    assert_eq!(view.target(view.site(1).target as usize), func("h"));
    assert_eq!(view.code().len(), 28);
    println!("record: {} targets, {} sites, equal bytes from either order", view.targets(), view.sites());
}

/// Prints `refused: a code record's site at 4 covers 12 bytes past its 8 bytes of code`.
#[test]
fn site_past_the_code_is_refused() {
    let mut r = CodeRecord::new_in(blaze_own::testing::heap().alloc());
    r.code.extend_from_slice(&ADRP_X16.to_le_bytes());
    r.code.extend_from_slice(&LDR_X16.to_le_bytes());
    let g = r.target(func("g"));
    r.sites.push(Site { at: 4, kind: SiteKind::Call, target: g, addend: 0 });
    let Err(error) = r.seal() else { panic!("a site past the code was sealed") };
    assert_eq!(error, RecordError::SiteOutside { at: 4, len: 12, code: 8 });
    println!("refused: {error}");
}

/// Prints `bundle: f calls g by bl +8, 0 imports; delta: 1 import`.
#[test]
fn bundle_relaxes_calls_within_it() {
    let alloc = blaze_own::testing::heap().alloc();
    let f = record(&[ADRP_X16, LDR_X16, BLR_X16, RET], &[(0, func("g"))], alloc);
    let g = record(&[RET], &[], alloc);
    let members = [
        Member { symbol: Symbol::of_name("f"), record: CodeView::read(&f).unwrap() },
        Member { symbol: Symbol::of_name("g"), record: CodeView::read(&g).unwrap() },
    ];
    let bundle = lay_out(&members, Kind::Bundle, PAGE, alloc).unwrap();
    assert_eq!(bundle.homes[1], (Symbol::of_name("g"), 16));
    assert_eq!(inst(&bundle.code, 0), NOP);
    assert_eq!(inst(&bundle.code, 4), NOP);
    // `bl` at 8 to 16: 8 bytes, two instructions.
    assert_eq!(inst(&bundle.code, 8), 0x9400_0002);
    assert!(bundle.imports.is_empty());
    let delta = lay_out(&members, Kind::Delta, PAGE, alloc).unwrap();
    assert_eq!(&delta.imports[..], &[func("g")]);
    assert_eq!(delta.imports_at, PAGE);
    // `adrp x16` at 0 to the page at 16384: four 4 KiB pages, immlo 0, immhi 1.
    assert_eq!(inst(&delta.code, 0), 0x9000_0030);
    assert_eq!(inst(&delta.code, 4), LDR_X16);
    assert_eq!(inst(&delta.code, 8), BLR_X16);
    println!("bundle: f calls g by bl +8, {} imports; delta: {} import", bundle.imports.len(), delta.imports.len());
}

/// Prints `folded: 2 functions, 1 body, 4 bytes of code`.
#[test]
fn equal_records_are_placed_once() {
    let alloc = blaze_own::testing::heap().alloc();
    let a = record(&[RET], &[], alloc);
    let b = record(&[RET], &[], alloc);
    let members = [
        Member { symbol: Symbol::of_name("a"), record: CodeView::read(&a).unwrap() },
        Member { symbol: Symbol::of_name("b"), record: CodeView::read(&b).unwrap() },
    ];
    let segment = lay_out(&members, Kind::Delta, PAGE, alloc).unwrap();
    assert_eq!(segment.homes[0].1, segment.homes[1].1);
    assert_eq!(segment.code.len(), 4);
    println!("folded: {} functions, 1 body, {} bytes of code", segment.homes.len(), segment.code.len());
}

/// Prints `edit: g moved to 0x100008000, 1 word rewritten at 0x100004000`.
#[test]
fn edit_rewrites_only_the_words_naming_what_moved() {
    let alloc = blaze_own::testing::heap().alloc();
    let base = 0x1_0000_0000;
    let f = record(&[ADRP_X16, LDR_X16, BLR_X16, RET], &[(0, func("g"))], alloc);
    let g = record(&[RET], &[], alloc);
    let first = lay_out(
        &[
            Member { symbol: Symbol::of_name("f"), record: CodeView::read(&f).unwrap() },
            Member { symbol: Symbol::of_name("g"), record: CodeView::read(&g).unwrap() },
        ],
        Kind::Delta,
        PAGE,
        alloc,
    )
    .unwrap();
    let mut image = Image::new(base, PAGE, alloc).unwrap();
    let one = image.append(&first).unwrap();
    assert_eq!(one.base, base);
    assert_eq!(&one.writes[..], &[Write { at: base + PAGE, value: base + 16 }]);
    assert!(one.moved.is_empty());
    // The edit: g's body changes; only g is in the new segment.
    let g2 = record(&[NOP, RET], &[], alloc);
    let second = lay_out(&[Member { symbol: Symbol::of_name("g"), record: CodeView::read(&g2).unwrap() }], Kind::Delta, PAGE, alloc).unwrap();
    let two = image.append(&second).unwrap();
    assert_eq!(two.base, base + 2 * PAGE);
    assert_eq!(&two.moved[..], &[Symbol::of_name("g")]);
    // Exactly f's import word, now naming the new g; nothing else, and no word of f's code.
    assert_eq!(&two.writes[..], &[Write { at: base + PAGE, value: base + 2 * PAGE }]);
    assert_eq!(image.home(func("g")), Some(base + 2 * PAGE));
    println!("edit: g moved to {:#x}, {} word rewritten at {:#x}", two.base, two.writes.len(), two.writes[0].at);
}

/// The one `adrp` encoder refuses a page past its 21-bit field by type, for every caller (the
/// launcher's thunk once trusted a comment, "the pages are within 4 GiB"): the last page in
/// reach, forward and back, encodes and reads back to its page through `adrp_add_target`; one
/// page further is `OutOfReach` naming the place and the target. Control: an 8-aligned low-12
/// load offset encodes and a misaligned one is refused.
///
/// Expected line: `test tests::adrp_refuses_a_page_past_its_field_by_type ... ok`
#[test]
fn adrp_refuses_a_page_past_its_field_by_type() {
    use crate::a64::{ADD64, ADRP_REACH, OutOfReach, adrp_add_target, adrp_to, lo12};
    let pc = 0x10_0000_0000u64;
    for (to, back) in [(pc + ((1 << 20) - 1) * 4096 + 0x10, false), (pc - (1 << 20) * 4096 + 0x10, true)] {
        let inst = adrp_to(ADRP_X16, pc, to).unwrap_or_else(|e| panic!("the last page in reach ({back}): {e}"));
        assert_eq!(inst & 0x1F, 16, "the register stands");
        let add = lo12(ADD64.1 | (16 << 5) | 16, to, 0).expect("an add takes any low 12 bits");
        assert_eq!(adrp_add_target(inst, add, pc), Some(to), "the pair reads back to its target ({back})");
        let past = if back { to - 4096 } else { to + 4096 };
        assert_eq!(adrp_to(ADRP_X16, pc, past), Err(OutOfReach { what: ADRP_REACH, from: pc, to: past }), "one page further ({back})");
    }
    assert!(lo12(LDR_X16, pc + 0x18, 3).is_some(), "control: an 8-aligned load offset");
    assert_eq!(lo12(LDR_X16, pc + 0x1C, 3), None, "a load offset the 8-byte scale cannot say");
}
