//! `blaze_native_load::{plan, bind}` over images `blaze_native::layout::lay` makes (objects as
//! values, as blaze-native's own layout tests build them: what its readers give), then placed
//! into a model of the process's memory the way a driver would: the copy, then every store.
//! Each instruction is read back with `a64` and must name the symbol's absolute address.
//!
//! The allocator is a frame made at the top of each test (no ambient heap).

use blaze_mem::frame::Frame;
use blaze_mem::tag::Tag;
use blaze_native::Platform;
use blaze_native::image::{FixTo, Fixup, Image, Import, Placed, Region, TlsReloc};
use blaze_native::layout::{Layout, lay};
use blaze_native::object::{Bind, Def, Object, Reloc, RelocKind, RelocTo, Section, SectionKind, Symbol};
use blaze_native::resolve::{Input, InputKind, Origin, Plan, Pulled, Resolution};
use blaze_native_load::a64;
use blaze_native_load::bind::{BindError, Site, Writes, bind, export};
use blaze_native_load::plan::{Cell, LoadPlan, PlanError, Prot, plan};
use blaze_own::{AVec, Alloc, Erased, Vec};
use blaze_sim::{Config as SimConfig, Tape};

const ORIGIN: Origin<'static> = Origin { package: "t@0.1.0", file: "libt.a" };
const NOP: u32 = 0xd503_201f;
const BL: u32 = 0x9400_0000;
const BASE: u64 = 0x2_0000_0000;

fn frame() -> Frame {
    Frame::with_reserve(1 << 30, Tag::Link).expect("a frame")
}

fn le<'a>(words: &[u32], a: Alloc<'a>) -> &'a [u8] {
    let mut b: AVec<'a, u8> = Vec::new_in(a);
    for w in words {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b.leak()
}

fn zeros<'a>(n: usize, a: Alloc<'a>) -> &'a [u8] {
    let mut b: AVec<'a, u8> = Vec::new_in(a);
    b.resize(n, 0);
    b.leak()
}

fn section<'a>(segment: &'static [u8], name: &'static [u8], kind: SectionKind, align_log2: u8, bytes: &'a [u8], relocs: core::ops::Range<u32>) -> Section<'a> {
    Section { segment, name, kind, align_log2, bytes, size: bytes.len() as u64, relocs }
}

fn sym(name: &'static [u8], def: Def, bind: Bind) -> Symbol<'static> {
    Symbol { name, def, bind, hidden: false }
}

fn reloc(section: u32, offset: u32, kind: RelocKind, symbol: u32) -> Reloc {
    Reloc { section, offset, kind, to: RelocTo::Symbol(symbol), addend: 0 }
}

fn object<'a, const S: usize, const Y: usize, const R: usize>(a: Alloc<'a>, sections: [Section<'a>; S], symbols: [Symbol<'a>; Y], relocs: [Reloc; R]) -> Object<'a> {
    Object {
        sections: Vec::from_array_in(sections, a),
        symbols: Vec::from_array_in(symbols, a),
        relocs: Vec::from_array_in(relocs, a),
        unwind: Vec::new_in(a),
        groups: Vec::new_in(a),
    }
}

fn one<'a, const N: usize, const W: usize, const I: usize>(
    a: Alloc<'a>,
    object: Object<'a>,
    names: [(&'a [u8], Resolution); N],
    words: [&'a [u8]; W],
    imports: [&'a [u8]; I],
) -> Plan<'a> {
    Plan {
        objects: Vec::from_array_in([Pulled { input: 0, member: 0, object }], a),
        names: Vec::from_array_in(names, a),
        words: Vec::from_array_in(words, a),
        imports: Vec::from_array_in(imports, a),
        dropped: Vec::new_in(a),
        libraries: Vec::new_in(a),
    }
}

fn laid<'a>(p: &Plan<'a>, a: Alloc<'a>) -> Image<'a> {
    let inputs = [Input { from: ORIGIN, bytes: &[], kind: InputKind::Archive { whole: false } }];
    match lay(p, &inputs, Layout::MACOS_ARM64, a) {
        Ok(image) => image,
        Err(e) => panic!("lay refused: {e:?}"),
    }
}

fn planned<'a>(image: &Image<'a>, a: Alloc<'a>) -> LoadPlan<'a> {
    match plan(image, a) {
        Ok(p) => p,
        Err(e) => panic!("plan refused: {e:?}"),
    }
}

fn site<'s>(base: u64, imports: &'s [Option<u64>], words: &'s [u64]) -> Site<'s> {
    Site { base, imports, words, tlv_thunk: None, tlv_key: None, tlsdesc_resolver: None }
}

fn bound<'a>(p: &LoadPlan<'a>, s: &Site<'_>, a: Alloc<'a>) -> Writes<'a> {
    match bind(p, s, a) {
        Ok(w) => w,
        Err(e) => panic!("bind refused: {e:?}"),
    }
}

/// The process's memory after the driver's steps 2 and 3: `[base, base + span)`, zeroed, the
/// image's bytes copied, every store made.
fn placed<'a>(image: &Image<'_>, w: &Writes<'_>, a: Alloc<'a>) -> AVec<'a, u8> {
    let mut m: AVec<'a, u8> = Vec::new_in(a);
    m.resize(w.span as usize, 0);
    let at = (w.copy.at - w.base) as usize;
    m[at..at + image.bytes.len()].copy_from_slice(&image.bytes[..]);
    for s in w.stores.iter() {
        let i = (s.at - w.base) as usize;
        m[i..i + 8].copy_from_slice(&s.value.to_le_bytes());
    }
    m
}

fn rd32(m: &[u8], base: u64, at: u64) -> u32 {
    let i = (at - base) as usize;
    u32::from_le_bytes(m[i..i + 4].try_into().expect("four bytes"))
}

fn rd64(m: &[u8], base: u64, at: u64) -> u64 {
    let i = (at - base) as usize;
    u64::from_le_bytes(m[i..i + 8].try_into().expect("eight bytes"))
}

/// The 12-byte stub at `stub` (`adrp x16, cell@PAGE; ldr x16, [x16, cell@PAGEOFF]; br x16`):
/// the value of its cell.
fn through_stub(m: &[u8], base: u64, stub: u64) -> u64 {
    let cell = a64::page_pair(rd32(m, base, stub), stub, rd32(m, base, stub + 4)).expect("adrp then ldr");
    assert_eq!(rd32(m, base, stub + 8), 0xd61f_0200, "br x16");
    rd64(m, base, cell)
}

/// C code calling malloc through its GOT cell and free by a branch (a stub), and C data holding
/// a pointer to its own code: three segments (code, Got, data), two imports, one rebase. Once
/// placed at 0x2_0000_0000 with malloc at 0x1_8000_1000 and free at 0x1_8000_2000, the ADRP+LDR
/// pair reads a cell holding malloc's address, the BL goes to a stub whose cell holds free's,
/// and the data cell holds the base plus the code's image address.
/// Prints "segments_cells_and_imports: RX 0x4000+.., R 0x....+0x4000, RW ..; malloc 0x180001000 free 0x180002000".
#[test]
fn segments_cells_and_imports() {
    let f = frame();
    let erased = Erased::new(&f);
    let a = erased.handle();
    // adrp x0, _malloc@GOTPAGE; ldr x0, [x0, _malloc@GOTPAGEOFF]; bl _free
    let code = le(&[0x9000_0000, 0xf940_0000, BL], a);
    let p = one(
        a,
        object(
            a,
            [section(b"__TEXT", b"__text", SectionKind::Code, 2, code, 0..3), section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(8, a), 3..4)],
            [
                sym(b"_malloc", Def::Undefined, Bind::Global),
                sym(b"_free", Def::Undefined, Bind::Global),
                sym(b"_c_entry", Def::In { section: 0, offset: 0 }, Bind::Global),
            ],
            [reloc(0, 0, RelocKind::GotPage21, 0), reloc(0, 4, RelocKind::GotPageOff12, 0), reloc(0, 8, RelocKind::Branch26, 1), reloc(1, 0, RelocKind::Abs64, 2)],
        ),
        [
            (&b"_c_entry"[..], Resolution::Defined { object: 0, symbol: 2 }),
            (&b"_free"[..], Resolution::Import { index: 0, weak: false }),
            (&b"_malloc"[..], Resolution::Import { index: 1, weak: false }),
        ],
        [],
        [&b"_free"[..], &b"_malloc"[..]],
    );
    let image = laid(&p, a);
    let lp = planned(&image, a);
    let prots: AVec<'_, Prot> = {
        let mut v = Vec::new_in(a);
        for s in lp.segments.iter() {
            v.push(s.prot);
        }
        v
    };
    assert_eq!(prots.as_slice(), &[Prot::ReadExec, Prot::Read, Prot::ReadWrite], "code, then the Got (RELRO), then data");
    for w in lp.segments.windows(2) {
        assert_eq!(w[0].at + w[0].len, w[1].at, "segments are contiguous");
    }
    for s in lp.segments.iter() {
        assert_eq!(s.at % 0x4000, 0, "every segment starts on a 16 KiB page");
    }
    assert!(lp.patches.iter().any(|q| matches!(q.cell, Cell::Rebase { .. })), "the data pointer is a rebase");
    assert_eq!(lp.imports.len(), 2);

    let (malloc, free) = (0x1_8000_1000u64, 0x1_8000_2000u64);
    let answers = [Some(free), Some(malloc)];
    let w = bound(&lp, &site(BASE, &answers, &[]), a);
    let m = placed(&image, &w, a);
    let entry = export(&lp, BASE, b"_c_entry").expect("the code is exported");
    let cell = a64::page_pair(rd32(&m, BASE, entry), entry, rd32(&m, BASE, entry + 4)).expect("adrp then ldr");
    assert_eq!(rd64(&m, BASE, cell), malloc, "the GOT cell holds malloc's address");
    let bl = rd32(&m, BASE, entry + 8);
    assert!(a64::is_bl(bl));
    let stub = a64::branch26(bl, entry + 8).expect("a branch");
    assert_eq!(through_stub(&m, BASE, stub), free, "the stub's cell holds free's address");
    let data = image.region(Region::Data).expect("listed").at + BASE;
    let pointer = w.stores.iter().find(|s| s.value == entry && s.at >= data).expect("the data cell is stored");
    assert_eq!(rd64(&m, BASE, pointer.at), entry);
    println!(
        "segments_cells_and_imports: RX {:#x}+{:#x}, R {:#x}+{:#x}, RW {:#x}+{:#x}; malloc {malloc:#x} free {free:#x}",
        lp.segments[0].at, lp.segments[0].len, lp.segments[1].at, lp.segments[1].len, lp.segments[2].at, lp.segments[2].len
    );
}

/// The image of one BL to `_free`, its reference `weak` or not.
fn calls_free<'a>(a: Alloc<'a>, weak: bool) -> Plan<'a> {
    let code = le(&[BL], a);
    one(
        a,
        object(
            a,
            [section(b"__TEXT", b"__text", SectionKind::Code, 2, code, 0..1)],
            [sym(b"_free", Def::Undefined, if weak { Bind::WeakUndefined } else { Bind::Global })],
            [reloc(0, 0, RelocKind::Branch26, 0)],
        ),
        [(&b"_free"[..], Resolution::Import { index: 0, weak })],
        [],
        [&b"_free"[..]],
    )
}

/// A strong import no library defines is refused by name before anything is placed; the
/// control (rule 23): the same image with the reference weak binds, and its cell holds 0.
/// Prints "strong_import_missing_refused: Unresolved { name: \"_free\" }; weak: cell 0".
#[test]
fn strong_import_missing_refused() {
    let f = frame();
    let erased = Erased::new(&f);
    let a = erased.handle();
    let strong = laid(&calls_free(a, false), a);
    let lp = planned(&strong, a);
    let refused = bind(&lp, &site(BASE, &[None], &[]), a).err();
    assert_eq!(refused, Some(BindError::Unresolved { name: &b"_free"[..] }));

    let weak = laid(&calls_free(a, true), a);
    assert_eq!(weak.imports.as_slice(), &[Import { name: &b"_free"[..], weak: true }]);
    let lp = planned(&weak, a);
    let w = bound(&lp, &site(BASE, &[None], &[]), a);
    let imports: AVec<'_, u64> = {
        let mut v = Vec::new_in(a);
        for (q, s) in lp.patches.iter().zip(w.stores.iter()) {
            if matches!(q.cell, Cell::Import { .. }) {
                v.push(s.value);
            }
        }
        v
    };
    assert_eq!(imports.as_slice(), &[0], "a weak import no library defines is 0");
    println!("strong_import_missing_refused: {:?}; weak: cell 0", refused.expect("refused"));
}

/// C calling Rust (a branch to a word's stub) and C data holding a Rust function's address
/// (a pointer row): both become the slot's entry thunk, one address a function. Without the
/// word handed in: `WordCount`.
/// Prints "rust_words_and_pointer_rows: word cell 0x7000_0000 via stub, data cell 0x7000_0000".
#[test]
fn rust_words_and_pointer_rows() {
    let f = frame();
    let erased = Erased::new(&f);
    let a = erased.handle();
    let code = le(&[BL], a);
    let p = one(
        a,
        object(
            a,
            [section(b"__TEXT", b"__text", SectionKind::Code, 2, code, 0..1), section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(8, a), 1..2)],
            [sym(b"_rust_cb", Def::Undefined, Bind::Global), sym(b"_c", Def::In { section: 0, offset: 0 }, Bind::Global)],
            [reloc(0, 0, RelocKind::Branch26, 0), reloc(1, 0, RelocKind::Abs64, 0)],
        ),
        [(&b"_c"[..], Resolution::Defined { object: 0, symbol: 1 }), (&b"_rust_cb"[..], Resolution::Rust { word: 0 })],
        [&b"_rust_cb"[..]],
        [],
    );
    let image = laid(&p, a);
    assert!(image.fixups.iter().any(|x| matches!(x.to, FixTo::Word { index: 0 })), "the data cell is a pointer row");
    let lp = planned(&image, a);
    assert_eq!(lp.words, 1);
    assert_eq!(bind(&lp, &site(BASE, &[], &[]), a).err(), Some(BindError::WordCount { want: 1, given: 0 }));

    let thunk = 0x7000_0000u64;
    let w = bound(&lp, &site(BASE, &[], &[thunk]), a);
    let m = placed(&image, &w, a);
    let c = export(&lp, BASE, b"_c").expect("exported");
    let stub = a64::branch26(rd32(&m, BASE, c), c).expect("a branch");
    assert_eq!(through_stub(&m, BASE, stub), thunk, "the branch reaches the word");
    let rows = w.stores.iter().filter(|s| s.value == thunk).count();
    assert_eq!(rows, 2, "the word's own cell and the data cell");
    println!("rust_words_and_pointer_rows: word cell {thunk:#x} via stub, data cell {thunk:#x}");
}

/// A Mach-O thread-local: the descriptor's thunk is the launcher's `tlv_get_addr`, its key the
/// image's key, its offset the variable's place in the template; `__tlv_bootstrap` is never
/// looked up. The control: no thunk handed in is `NoTlvThunk`.
/// Prints "thread_vars_thunk_and_key: thunk 0x6000_0000, key 0x2a, template 8 bytes".
#[test]
fn thread_vars_thunk_and_key() {
    let f = frame();
    let erased = Erased::new(&f);
    let a = erased.handle();
    let p = one(
        a,
        object(
            a,
            [
                section(b"__DATA", b"__thread_vars", SectionKind::ThreadVars, 3, zeros(24, a), 0..2),
                section(b"__DATA", b"__thread_data", SectionKind::ThreadData, 3, le(&[7, 0], a), 2..2),
            ],
            [sym(b"__tlv_bootstrap", Def::Undefined, Bind::Global), sym(b"_v$tlv$init", Def::In { section: 1, offset: 0 }, Bind::Local)],
            [reloc(0, 0, RelocKind::Abs64, 0), reloc(0, 16, RelocKind::Abs64, 1)],
        ),
        [(&b"__tlv_bootstrap"[..], Resolution::Import { index: 0, weak: false })],
        [],
        [&b"__tlv_bootstrap"[..]],
    );
    let image = laid(&p, a);
    let lp = planned(&image, a);
    assert!(!lp.imports[0].lookup, "__tlv_bootstrap is the launcher's, not a library's");
    let vars = image.region(Region::ThreadVars).expect("listed").at;
    assert!(lp.patches.iter().any(|q| q.at == vars && q.cell == Cell::TlvThunk));
    assert!(lp.patches.iter().any(|q| q.at == vars + 8 && q.cell == Cell::TlvKey));
    let t = lp.template.expect("a template");
    assert_eq!((t.init.len, t.zero_len), (8, 0));

    assert_eq!(bind(&lp, &site(BASE, &[None], &[]), a).err(), Some(BindError::NoTlvThunk));
    let s = Site { tlv_thunk: Some(0x6000_0000), tlv_key: Some(42), ..site(BASE, &[None], &[]) };
    let w = bound(&lp, &s, a);
    let m = placed(&image, &w, a);
    assert_eq!(rd64(&m, BASE, BASE + vars), 0x6000_0000);
    assert_eq!(rd64(&m, BASE, BASE + vars + 8), 42);
    assert_eq!(rd64(&m, BASE, BASE + vars + 16), 0, "the variable is the template's first byte");
    println!("thread_vars_thunk_and_key: thunk 0x6000_0000, key 0x2a, template {} bytes", t.init.len);
}

/// Initializers in link order, as absolute addresses: the first thread calls them before the
/// entry (bind.rs step 8). Prints "initializers_in_link_order: [0x2000040.., 0x2000040..+4]".
#[test]
fn initializers_in_link_order() {
    let f = frame();
    let erased = Erased::new(&f);
    let a = erased.handle();
    let code = le(&[NOP, NOP], a);
    let p = one(
        a,
        object(
            a,
            [section(b"__TEXT", b"__text", SectionKind::Code, 2, code, 0..0), section(b"__DATA", b"__mod_init_func", SectionKind::InitPointers, 3, zeros(16, a), 0..2)],
            [sym(b"_init_a", Def::In { section: 0, offset: 0 }, Bind::Local), sym(b"_init_b", Def::In { section: 0, offset: 4 }, Bind::Local)],
            [reloc(1, 0, RelocKind::Abs64, 0), reloc(1, 8, RelocKind::Abs64, 1)],
        ),
        [],
        [],
        [],
    );
    let image = laid(&p, a);
    assert_eq!(image.inits, 2);
    let lp = planned(&image, a);
    let text = image.region(Region::Text).expect("listed").at;
    assert_eq!(lp.inits.as_slice(), &[text, text + 4]);
    let w = bound(&lp, &site(BASE, &[], &[]), a);
    assert_eq!(w.inits.as_slice(), &[BASE + text, BASE + text + 4]);
    println!("initializers_in_link_order: [{:#x}, {:#x}]", w.inits[0], w.inits[1]);
}

/// Rule 25: the plan holds nothing of a base or a machine. Two lays of one input give equal
/// plan bytes; binding at two bases changes the rebases by exactly the bases' difference and
/// the imports not at all. Prints "plan_holds_no_base: N bytes equal; rebases moved by 0x10000000".
#[test]
fn plan_holds_no_base() {
    let f = frame();
    let erased = Erased::new(&f);
    let a = erased.handle();
    let make = || {
        let code = le(&[0x9000_0000, 0xf940_0000, BL], a);
        one(
            a,
            object(
                a,
                [section(b"__TEXT", b"__text", SectionKind::Code, 2, code, 0..3), section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(8, a), 3..4)],
                [sym(b"_malloc", Def::Undefined, Bind::Global), sym(b"_free", Def::Undefined, Bind::Global), sym(b"_c_entry", Def::In { section: 0, offset: 0 }, Bind::Global)],
                [reloc(0, 0, RelocKind::GotPage21, 0), reloc(0, 4, RelocKind::GotPageOff12, 0), reloc(0, 8, RelocKind::Branch26, 1), reloc(1, 0, RelocKind::Abs64, 2)],
            ),
            [
                (&b"_c_entry"[..], Resolution::Defined { object: 0, symbol: 2 }),
                (&b"_free"[..], Resolution::Import { index: 0, weak: false }),
                (&b"_malloc"[..], Resolution::Import { index: 1, weak: false }),
            ],
            [],
            [&b"_free"[..], &b"_malloc"[..]],
        )
    };
    let (p1, p2) = (make(), make());
    let (i1, i2) = (laid(&p1, a), laid(&p2, a));
    let (l1, l2) = (planned(&i1, a), planned(&i2, a));
    let mut b1: AVec<'_, u8> = Vec::new_in(a);
    let mut b2: AVec<'_, u8> = Vec::new_in(a);
    l1.encode(&mut b1).expect("encoded");
    l2.encode(&mut b2).expect("encoded");
    assert_eq!(b1.as_slice(), b2.as_slice());

    let answers = [Some(0x1_8000_2000), Some(0x1_8000_1000)];
    let other = BASE + 0x1000_0000;
    let w1 = bound(&l1, &site(BASE, &answers, &[]), a);
    let w2 = bound(&l1, &site(other, &answers, &[]), a);
    for ((q, s1), s2) in l1.patches.iter().zip(w1.stores.iter()).zip(w2.stores.iter()) {
        assert_eq!(s2.at - s1.at, other - BASE);
        match q.cell {
            Cell::Rebase { .. } => assert_eq!(s2.value - s1.value, other - BASE),
            _ => assert_eq!(s2.value, s1.value),
        }
    }
    let mut b3: AVec<'_, u8> = Vec::new_in(a);
    l1.encode(&mut b3).expect("encoded");
    assert_eq!(b1.as_slice(), b3.as_slice(), "binding leaves the plan as it was");
    println!("plan_holds_no_base: {} bytes equal; rebases moved by {:#x}", b1.len(), other - BASE);
}

/// An ELF image written by hand (the twelve regions; a Got of two cells at 0x20000): every
/// region empty but Text and Got.
fn elf_image<'a>(a: Alloc<'a>, tls: TlsReloc) -> Image<'a> {
    let p = |region, at, len| Placed { region, at, len, align_log2: 3 };
    let regions = [
        p(Region::Text, 0x1_0000, 0x10),
        p(Region::Const, 0x1_0010, 0),
        p(Region::EhFrame, 0x1_0010, 0),
        p(Region::UnwindInfo, 0x1_0010, 0),
        p(Region::Got, 0x2_0000, 0x10),
        p(Region::Data, 0x3_0000, 0),
        p(Region::Words, 0x3_0000, 0),
        p(Region::Inits, 0x3_0000, 0),
        p(Region::ThreadVars, 0x3_0000, 0),
        p(Region::ThreadData, 0x3_0000, 8),
        p(Region::ThreadZero, 0x3_0008, 0),
        p(Region::Zero, 0x3_0008, 0),
    ];
    Image {
        platform: Platform::LinuxArm64,
        regions: Vec::from_array_in(regions, a),
        bytes: {
            let mut b = Vec::new_in(a);
            b.resize(0x2_0008, 0);
            b
        },
        file_start: 0x1_0000,
        end: 0x3_0008,
        fixups: Vec::new_in(a),
        tls: Vec::from_array_in([tls], a),
        imports: Vec::new_in(a),
        exports: Vec::new_in(a),
        words: Vec::new_in(a),
        inits: 0,
        descriptor_at: 0x3_0000,
        libraries: Vec::new_in(a),
        unwind: Vec::new_in(a),
        frames: Vec::new_in(a),
        eh_pointers: Vec::new_in(a),
    }
}

/// ELF: a TLS descriptor pair is the launcher's resolver and the variable's offset; an
/// initial-exec reference (a fixed offset from the thread pointer) cannot be given by a loader
/// that is not the C library, and is refused by its cell. Control: the same image with the
/// descriptor form plans and binds. Prints "elf_tls: InitialExecTls { got_at: 0x20000, offset: 0 } refused; desc binds".
#[test]
fn elf_tls() {
    let f = frame();
    let erased = Erased::new(&f);
    let a = erased.handle();
    let ie = elf_image(a, TlsReloc::TpRel { got_at: 0x2_0000, offset: 0 });
    let refused = plan(&ie, a).err();
    assert_eq!(refused, Some(PlanError::InitialExecTls { got_at: 0x2_0000, offset: 0 }));

    let desc = elf_image(a, TlsReloc::Desc { got_at: 0x2_0000, offset: 0 });
    let lp = planned(&desc, a);
    assert_eq!(lp.page, 0x1_0000, "Linux arm64 pages are 64 KiB here");
    assert_eq!(bind(&lp, &site(0x40_0000_0000, &[], &[]), a).err(), Some(BindError::NoTlsDescResolver));
    let s = Site { tlsdesc_resolver: Some(0x5000), ..site(0x40_0000_0000, &[], &[]) };
    let w = bound(&lp, &s, a);
    assert_eq!(w.stores.len(), 2);
    assert_eq!((w.stores[0].at, w.stores[0].value), (0x40_0002_0000, 0x5000));
    assert_eq!((w.stores[1].at, w.stores[1].value), (0x40_0002_0008, 0));
    assert_eq!(bind(&lp, &site(0x40_0000_4000, &[], &[]), a).err(), Some(BindError::BaseUnaligned { base: 0x40_0000_4000, page: 0x1_0000 }));
    println!("elf_tls: {:?} refused; desc binds", refused.expect("refused"));
}

/// A lying image is refused by the cell, never trusted: a fixup inside the code segment (a text
/// relocation), and one past the image's end. Prints "lying_images_refused: CellInText, CellOutside".
#[test]
fn lying_images_refused() {
    let f = frame();
    let erased = Erased::new(&f);
    let a = erased.handle();
    let mut in_text = elf_image(a, TlsReloc::Desc { got_at: 0x2_0000, offset: 0 });
    in_text.fixups.push(Fixup { at: 0x1_0008, to: FixTo::Rebase { target: 0x1_0000 } });
    assert_eq!(plan(&in_text, a).err(), Some(PlanError::CellInText { at: 0x1_0008 }));
    let mut past = elf_image(a, TlsReloc::Desc { got_at: 0x2_0000, offset: 0 });
    past.fixups.push(Fixup { at: 0x3_0008, to: FixTo::Rebase { target: 0x1_0000 } });
    assert_eq!(plan(&past, a).err(), Some(PlanError::CellOutside { at: 0x3_0008 }));
    println!("lying_images_refused: CellInText, CellOutside");
}

const NAMES: [&[u8]; 8] = [b"_f0", b"_f1", b"_f2", b"_f3", b"_f4", b"_f5", b"_f6", b"_f7"];

/// Random layouts: up to eight functions of random lengths, each starting with up to eight BLs
/// to random functions, and a data cell pointing at a random one; laid, planned, bound at a
/// random page-aligned base, placed. Every BL, decoded from the placed bytes, lands on its
/// symbol's absolute address, and the data cell holds it.
#[test]
fn every_branch_lands_on_its_symbol() {
    blaze_sim::check(SimConfig::DEFAULT, |tape: &Tape| {
        let f = frame();
        let erased = Erased::new(&f);
        let a = erased.handle();
        let n = tape.below(8, "functions") as usize + 1;
        let mut sections: AVec<'_, Section<'_>> = Vec::new_in(a);
        let mut symbols: AVec<'_, Symbol<'_>> = Vec::new_in(a);
        let mut relocs: AVec<'_, Reloc> = Vec::new_in(a);
        let mut calls: AVec<'_, (usize, u32, usize)> = Vec::new_in(a);
        for i in 0..n {
            let len = tape.below(4096, "words") as usize + 1;
            let branches = tape.below(len.min(8) as u64 + 1, "branches") as usize;
            let mut words: AVec<'_, u32> = Vec::new_in(a);
            words.resize(len, NOP);
            let start = relocs.len() as u32;
            for b in 0..branches {
                words[b] = BL;
                let to = tape.index(n, "callee");
                relocs.push(reloc(i as u32, 4 * b as u32, RelocKind::Branch26, to as u32));
                calls.push((i, 4 * b as u32, to));
            }
            let bytes = le(&words, a);
            sections.push(section(b"__TEXT", b"__text", SectionKind::Code, 2, bytes, start..relocs.len() as u32));
            symbols.push(sym(NAMES[i], Def::In { section: i as u32, offset: 0 }, Bind::Global));
        }
        let pointed = tape.index(n, "pointed");
        let start = relocs.len() as u32;
        relocs.push(reloc(n as u32, 0, RelocKind::Abs64, pointed as u32));
        sections.push(section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(8, a), start..start + 1));
        let mut names: AVec<'_, (&[u8], Resolution)> = Vec::new_in(a);
        for i in 0..n {
            names.push((NAMES[i], Resolution::Defined { object: 0, symbol: i as u32 }));
        }
        let object = Object { sections, symbols, relocs, unwind: Vec::new_in(a), groups: Vec::new_in(a) };
        let p = Plan {
            objects: Vec::from_array_in([Pulled { input: 0, member: 0, object }], a),
            names,
            words: Vec::new_in(a),
            imports: Vec::new_in(a),
            dropped: Vec::new_in(a),
            libraries: Vec::new_in(a),
        };
        let image = laid(&p, a);
        let lp = planned(&image, a);
        let base = (tape.below(1 << 20, "base page") + 1) * 0x4000;
        let w = bound(&lp, &site(base, &[], &[]), a);
        let m = placed(&image, &w, a);
        for &(from, offset, to) in calls.iter() {
            let place = export(&lp, base, NAMES[from]).expect("exported") + u64::from(offset);
            let target = export(&lp, base, NAMES[to]).expect("exported");
            let insn = rd32(&m, base, place);
            assert!(a64::is_bl(insn), "a BL at {place:#x}");
            assert_eq!(a64::branch26(insn, place), Some(target), "{}+{offset} -> {}", n, to);
        }
        let target = export(&lp, base, NAMES[pointed]).expect("exported");
        assert!(w.stores.iter().any(|s| s.value == target && rd64(&m, base, s.at) == target), "the data cell holds the pointed function");
    });
}
