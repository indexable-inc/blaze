//! AArch64 branches past their 128 MiB reach (`crate::thunk`): a caller and its callee with a
//! 200 MiB function between them link, each `bl` landing on a thunk that jumps to its target,
//! in a release image (direct calls) and a dev image with a jump table (calls to slot stubs,
//! past the patch room too), and in an incremental link's new code; and a `bl` from a native
//! object's `.init`, which ELF lays out after the code and its islands. A branch to a weak name
//! nothing defines, 200 MiB into the code, takes no thunk: it lands on the next instruction.
//! An edit that moves a direct callee to the patch region, 200 MiB from a caller the link
//! keeps in place, is refused, typed, and the link from scratch takes it; the same edit of a
//! callee reached through its slot is a splice, which writes no caller. The invariant of a
//! splice's reach, over drawn edit sequences on such an image, with a control per planted bug.

use blaze_codegen::{Arch, CompiledFunction, RelocKind, Relocation, Target};

use object::write::{Object as Builder, Relocation as Reloc, Symbol as Def, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationFlags, SectionKind, SymbolFlags, SymbolKind,
    SymbolScope, elf,
};

use super::{
    Change, Disk, Image, Linked, Native, NativeKind, PAGE, Sparse, TO, function, id, key, name,
    output, section_of,
};
use crate::Output;
use crate::symbols::SymKey;

const MIB: usize = 1 << 20;
/// `ret`.
const RET: [u8; 4] = [0xc0, 0x03, 0x5f, 0xd6];

fn a64(incremental: bool) -> Output {
    Output {
        arch: Arch::Aarch64,
        interpreter: Some("/lib/ld-linux-aarch64.so.1".into()),
        triple: "aarch64-unknown-linux-gnu".into(),
        ..output(incremental)
    }
}

/// `bl to; ret`.
fn caller(n: u8, to: Target) -> CompiledFunction {
    let call = Relocation { at: 0, kind: RelocKind::Call26, target: to, addend: 0 };
    let code = [[0, 0, 0, 0x94], RET].concat();
    CompiledFunction { arch: Arch::Aarch64, ..function(n, &code, vec![call]) }
}

/// 200 MiB of code ending in `ret`: more than a `bl` reaches, between the callers.
fn filler(n: u8) -> CompiledFunction {
    let mut code = vec![0u8; 200 * MIB];
    let end = code.len() - 4;
    code[end..].copy_from_slice(&RET);
    CompiledFunction { arch: Arch::Aarch64, ..function(n, &code, Vec::new()) }
}

/// Where the `bl` at `at` goes.
fn bl(linked: &Linked, at: u64) -> u64 {
    let word = linked.u32_at(at);
    assert_eq!(word >> 26, 0x25, "a `bl` at {at:#x}: {word:#010x}");
    let imm = ((word & 0x03ff_ffff) << 6) as i32 >> 6;
    at.wrapping_add((i64::from(imm) * 4) as u64)
}

/// Where the thunk at `at` (`adrp x16; add x16, x16, lo12; br x16`) jumps.
fn thunk(linked: &Linked, at: u64) -> u64 {
    let (adrp, add, br) = (linked.u32_at(at), linked.u32_at(at + 4), linked.u32_at(at + 8));
    assert_eq!(adrp & 0x9f00_001f, 0x9000_0010, "`adrp x16` at {at:#x}: {adrp:#010x}");
    assert_eq!(add & 0xffc0_03ff, 0x9100_0210, "`add x16, x16, #lo12` at {at:#x}: {add:#010x}");
    assert_eq!(br, 0xd61f_0200, "`br x16` at {:#x}", at + 8);
    let pages = (((adrp >> 5) & 0x7_ffff) << 2 | (adrp >> 29) & 3) as i64;
    let pages = pages << 43 >> 43;
    let page = (at & !0xfff).wrapping_add((pages << 12) as u64);
    page + u64::from((add >> 10) & 0xfff)
}

/// The address of `n`'s slot stub, or its code without a table.
fn callee(image: &Image, linked: &Linked, n: u8, debug: &str) -> u64 {
    let Some(id) = image.symbols.find_id(&key(n).0) else { panic!("no function {n}") };
    match image.symbols.find(SymKey::Slot(id)).and_then(|it| image.state.addr_of(it)) {
        Some(stub) => stub,
        None => linked.symbol(debug),
    }
}

#[test]
fn calls_200_mib_apart_go_through_thunks() {
    for incremental in [false, true] {
        let to = if incremental { Target::Function } else { Target::Direct };
        let (one, big, three) = (caller(1, to(key(3))), filler(2), caller(3, to(key(1))));
        let mut image = Image::new(name(TO), a64(incremental)).unwrap();
        let mut disk = Disk::default();
        let change = Change {
            added: vec![&one, &big, &three],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            debug_names: vec![
                (key(1), "one".into()),
                (key(2), "big".into()),
                (key(3), "three".into()),
            ],
            ..Change::default()
        };
        disk.link(&mut image, change)
            .unwrap_or_else(|it| panic!("incremental {incremental}: {it}"));
        let linked = Linked::of(disk.file(TO));
        let (a, c) = (linked.symbol("one"), linked.symbol("three"));
        assert!(c - a > 200 * MIB as u64, "incremental {incremental}: 200 MiB between them");
        for (from, n, debug) in [(a, 3, "three"), (c, 1, "one")] {
            let via = bl(&linked, from);
            assert!(
                via.abs_diff(from) < 1 << 27,
                "incremental {incremental}: the `bl` at {from:#x} reaches {via:#x}"
            );
            assert_eq!(
                thunk(&linked, via),
                callee(&image, &linked, n, debug),
                "incremental {incremental}: from {from:#x}"
            );
        }
        if !incremental {
            continue;
        }
        // New code in the patch region, 200 MiB of room away from the slot stubs: a thunk of
        // its own island, in the patch region after it.
        // It calls `big`, whose slot no thunk serves yet (a thunk to `one`'s at the end of the
        // code would be in reach, and used).
        let four = caller(4, Target::Function(key(2)));
        let edit = Change {
            added: vec![&four],
            removed: vec![key(3)],
            slots: vec![(key(3), key(4))],
            debug_names: vec![(key(4), "four".into())],
            ..Change::default()
        };
        let islands = image.state.islands;
        disk.link(&mut image, edit).unwrap_or_else(|it| panic!("the edit: {it}"));
        assert_eq!(image.state.islands, islands + 1, "the edit's own island");
        let linked = Linked::of(disk.file(TO));
        let d = linked.symbol("four");
        let via = bl(&linked, d);
        assert!(via > d && via - d < 1 << 27, "the edit's thunk is after its code: {via:#x}");
        assert_eq!(thunk(&linked, via), callee(&image, &linked, 2, "big"));
    }
}

#[test]
fn an_image_with_islands_is_the_same_whatever_order_its_functions_came_in() {
    let (one, big, three) =
        (caller(1, Target::Direct(key(3))), filler(2), caller(3, Target::Direct(key(1))));
    let link = |added: Vec<&CompiledFunction>| {
        let mut image = Image::new(name(TO), a64(false)).unwrap();
        let mut disk = Disk::default();
        let names = vec![("_start".into(), Target::Direct(key(1)))];
        disk.link(&mut image, Change { added, names, ..Change::default() }).unwrap();
        disk
    };
    let (first, second) = (link(vec![&one, &big, &three]), link(vec![&three, &big, &one]));
    assert!(first.file(TO).same(second.file(TO)));
}

/// `mov x0, #42`: what `call_weak_fn` starts with, to tell it from the other code.
const MOV_42: u32 = 0xd280_0540;

/// An AArch64 object shaped as glibc's `crti.o`: `.init` calls the local `call_weak_fn`, at
/// the start of its own `.text`, which is 200 MiB long.
fn crti() -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::Aarch64, Endianness::Little);
    let text = b.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    let mut code = vec![0u8; 200 * MIB];
    code[..4].copy_from_slice(&MOV_42.to_le_bytes());
    code[4..8].copy_from_slice(&RET);
    b.append_section_data(text, &code, 16);
    let weak = b.add_symbol(Def {
        name: b"call_weak_fn".to_vec(),
        value: 0,
        size: 8,
        kind: SymbolKind::Text,
        scope: SymbolScope::Compilation,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    let init = b.add_section(Vec::new(), b".init".to_vec(), SectionKind::Text);
    b.append_section_data(init, &[[0, 0, 0, 0x94], RET].concat(), 4);
    let call = Reloc {
        offset: 0,
        symbol: weak,
        addend: 0,
        flags: RelocationFlags::Elf { r_type: elf::R_AARCH64_CALL26 },
    };
    b.add_relocation(init, call).unwrap();
    b.write().unwrap()
}

#[test]
fn a_bl_from_init_past_its_reach_goes_through_a_thunk() {
    let object: std::sync::Arc<[u8]> = crti().into();
    let start = CompiledFunction { arch: Arch::Aarch64, ..function(1, &RET, Vec::new()) };
    for incremental in [false, true] {
        let mut image = Image::new(name(TO), a64(incremental)).unwrap();
        let mut disk = Disk::default();
        let change = Change {
            added: vec![&start],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            natives: vec![Native {
                id: id(60),
                name: "crti.o",
                bytes: &object,
                kind: NativeKind::Object,
                order: 0,
            }],
            ..Change::default()
        };
        disk.link(&mut image, change)
            .unwrap_or_else(|it| panic!("incremental {incremental}: {it}"));
        let linked = Linked::of(disk.file(TO));
        let (init, _) = section_of(&linked, ".init");
        let via = bl(&linked, init);
        assert!(
            via.abs_diff(init) < 1 << 27,
            "incremental {incremental}: the `bl` at {init:#x} reaches {via:#x}"
        );
        let to = thunk(&linked, via);
        assert_eq!(linked.u32_at(to), MOV_42, "incremental {incremental}: `call_weak_fn`");
        assert!(to.abs_diff(init) > 1 << 27, "incremental {incremental}: out of a `bl`'s reach");
    }
}

/// An AArch64 object whose `.text`, `far_code`, is 200 MiB of code ending in `bl hook; b hook`,
/// `hook` being a weak name nothing defines: zstd's `ZSTD_trace_compress_begin` call, which C
/// guards with a test of its address, past a branch's reach of address 0.
fn far_weak_call() -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::Aarch64, Endianness::Little);
    let text = b.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    let mut code = vec![0u8; 200 * MIB];
    let end = code.len();
    code[end - 8..end - 4].copy_from_slice(&[0, 0, 0, 0x94]);
    code[end - 4..].copy_from_slice(&[0, 0, 0, 0x14]);
    b.append_section_data(text, &code, 16);
    // What `_start` calls, so the section is live.
    b.add_symbol(Def {
        name: b"far_code".to_vec(),
        value: 0,
        size: code.len() as u64,
        kind: SymbolKind::Text,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    let hook = b.add_symbol(Def {
        name: b"hook".to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Text,
        scope: SymbolScope::Unknown,
        weak: true,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    for (at, r_type) in [(end - 8, elf::R_AARCH64_CALL26), (end - 4, elf::R_AARCH64_JUMP26)] {
        let flags = RelocationFlags::Elf { r_type };
        b.add_relocation(text, Reloc { offset: at as u64, symbol: hook, addend: 0, flags })
            .unwrap();
    }
    b.write().unwrap()
}

#[test]
fn a_branch_to_an_undefined_weak_past_128_mib_lands_on_the_next_instruction() {
    let object: std::sync::Arc<[u8]> = far_weak_call().into();
    let start = caller(1, Target::Extern("far_code".into()));
    for incremental in [false, true] {
        let mut image = Image::new(name(TO), a64(incremental)).unwrap();
        let mut disk = Disk::default();
        let change = Change {
            added: vec![&start],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            natives: vec![Native {
                id: id(61),
                name: "zstd_compress.o",
                bytes: &object,
                kind: NativeKind::Object,
                order: 0,
            }],
            ..Change::default()
        };
        disk.link(&mut image, change)
            .unwrap_or_else(|it| panic!("incremental {incremental}: {it}"));
        let linked = Linked::of(disk.file(TO));
        let (text, size) = section_of(&linked, ".text");
        // The object's code is the 200 MiB run ending at the last `bl`'s `b`.
        let at = (0..size / 4)
            .map(|it| text + size - 4 * (it + 1))
            .find(|it| linked.u32_at(*it) == 0x1400_0001)
            .unwrap_or_else(|| panic!("incremental {incremental}: no `b .+4` in the code"));
        let call = at - 4;
        let past = call > 1 << 27;
        assert!(past, "incremental {incremental}: the `bl` at {call:#x} is in reach of 0");
        assert_eq!(bl(&linked, call), at, "incremental {incremental}: `bl hook` steps past itself");
    }
}

/// `nop; ret`: the edited callee, to tell it from the first.
const NOP: u32 = 0xd503_201f;

/// The dogfood report's shape (2026-11-09, `blaze test -p blaze-shader`): `one` calls `two`
/// directly, next to it, so the full link makes no thunk of `two`; 200 MiB of code are
/// between them and the patch region. An edit refills `two` under its own key: its new code
/// goes to the patch region, `one` is written again where it is, and its `bl` reaches
/// neither `two` nor the patch region, where the stage's thunks go. The stage is refused,
/// typed, naming both functions and the distance signed in MiB, with nothing staged; a link
/// of the same functions from scratch lays them next to each other again.
#[test]
fn an_edit_moving_a_direct_callee_out_of_its_callers_reach_is_refused_for_a_link_from_scratch() {
    let plain = |n: u8, code: &[u8]| CompiledFunction {
        arch: Arch::Aarch64,
        ..function(n, code, Vec::new())
    };
    let (one, two, big) = (caller(1, Target::Direct(key(2))), plain(2, &RET), filler(3));
    let two2 = plain(2, &[NOP.to_le_bytes(), RET].concat());
    let whole = |two: &CompiledFunction| -> (Image, Disk) {
        let mut image = Image::new(name(TO), a64(true)).unwrap();
        let mut disk = Disk::default();
        let change = Change {
            added: vec![&one, two, &big],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            debug_names: vec![
                (key(1), "one".into()),
                (key(2), "two".into()),
                (key(3), "big".into()),
            ],
            ..Change::default()
        };
        disk.link(&mut image, change).unwrap_or_else(|it| panic!("from scratch: {it}"));
        (image, disk)
    };
    let (mut image, disk) = whole(&two);
    let linked = Linked::of(disk.file(TO));
    let (a, b) = (linked.symbol("one"), linked.symbol("two"));
    assert_eq!(bl(&linked, a), b, "next to each other: a direct `bl`, no thunk");
    assert!(linked.symbol("big") > b, "the 200 MiB are after them, before the patch region");

    let generation = image.generation();
    let edit = Change { added: vec![&two2], removed: vec![key(2)], ..Change::default() };
    let error = match image.stage(edit) {
        Ok(_) => panic!("staged: `one` is 200 MiB from the patch region's thunks"),
        Err(error) => error,
    };
    let branch = error.no_thunk().unwrap_or_else(|| panic!("not typed: {error}"));
    assert!(
        branch.contains("the function `one`") && branch.contains("the function `two`"),
        "names the caller and the callee: {branch}"
    );
    assert!(
        branch.contains(", +2") && branch.ends_with(" MiB away"),
        "the distance is signed, in MiB: {branch}"
    );
    assert!(!error.what.contains("0xffffffff"), "no distance as an unsigned number: {error}");
    assert_eq!(error.function, Some(key(1)), "the error names the caller");
    assert_eq!(error.no_room(), None, "no want of room");
    assert_eq!(image.generation(), generation, "nothing staged");

    // What the build does with the refusal: the same functions linked from scratch.
    let (_, disk) = whole(&two2);
    let linked = Linked::of(disk.file(TO));
    let (a, b) = (linked.symbol("one"), linked.symbol("two"));
    assert_eq!(bl(&linked, a), b, "from scratch: next to each other again");
    assert_eq!(linked.u32_at(b), NOP, "the edited `two`");
}

/// `ret` as a word.
const RET_WORD: u32 = 0xd65f_03c0;
/// The leaves of [`Far`], and what a debugger calls them.
const LEAVES: [(u8, &str); 2] = [(2, "two"), (5, "five")];
/// Each call of [`Far`]: the caller, which of its `bl`s, the callee.
const CALLS: [(u8, u64, u8); 4] = [(1, 0, 2), (1, 1, 5), (4, 0, 2), (4, 1, 5)];

/// A `bl` to each of `to`, then `ret`.
fn calls(n: u8, to: &[Target]) -> CompiledFunction {
    let mut code = Vec::new();
    let mut relocs = Vec::new();
    for (at, target) in to.iter().enumerate() {
        code.extend_from_slice(&[0, 0, 0, 0x94]);
        // clone: a test's target
        let target = target.clone();
        relocs.push(Relocation { at: at as u32 * 4, kind: RelocKind::Call26, target, addend: 0 });
    }
    code.extend_from_slice(&RET);
    CompiledFunction { arch: Arch::Aarch64, ..function(n, &code, relocs) }
}

/// `version` `nop`s, then `ret`: a leaf an edit fills again under its own key, told from
/// its other versions by its code.
fn leaf(n: u8, version: u8) -> CompiledFunction {
    let mut code = Vec::new();
    for _ in 0..version {
        code.extend_from_slice(&NOP.to_le_bytes());
    }
    code.extend_from_slice(&RET);
    CompiledFunction { arch: Arch::Aarch64, ..function(n, &code, Vec::new()) }
}

/// A linked image read by address through its file's pages, with the words a control planted
/// over it: the image a linker with that bug would have written.
struct Peek<'a> {
    file: &'a Sparse,
    planted: Vec<(u64, u32)>,
}

impl Peek<'_> {
    fn u32_at(&self, at: u64) -> u32 {
        if let Some(planted) = self.planted.iter().find(|it| it.0 == at) {
            return planted.1;
        }
        let mut bytes = [0u8; 4];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            let at = at + offset as u64;
            *byte = self.file.page(at / PAGE)[(at % PAGE) as usize];
        }
        u32::from_le_bytes(bytes)
    }

    fn u64_at(&self, at: u64) -> u64 {
        u64::from(self.u32_at(at)) | u64::from(self.u32_at(at + 4)) << 32
    }
}

/// The page an `adrp x16` at `at` names.
fn adrp_page(adrp: u32, at: u64) -> u64 {
    let pages = (((adrp >> 5) & 0x7_ffff) << 2 | (adrp >> 29) & 3) as i64;
    let pages = pages << 43 >> 43;
    (at & !0xfff).wrapping_add((pages << 12) as u64)
}

/// The address of the table entry the slot stub at `stub` jumps through, in its shortest
/// form (a slot below 4096): `adrp x16, cell; ldr x16, [x16, lo12]; ldr x16, [x16, #slot * 8];
/// br x16`.
fn slot_entry(peek: &Peek<'_>, stub: u64) -> u64 {
    let (adrp, cell, entry) = (peek.u32_at(stub), peek.u32_at(stub + 4), peek.u32_at(stub + 8));
    assert_eq!(adrp & 0x9f00_001f, 0x9000_0010, "`adrp x16` at {stub:#x}: {adrp:#010x}");
    assert_eq!(cell & 0xffc0_03ff, 0xf940_0210, "`ldr x16, [x16, cell]`: {cell:#010x}");
    assert_eq!(entry & 0xffc0_03ff, 0xf940_0210, "`ldr x16, [x16, slot]`: {entry:#010x}");
    assert_eq!(peek.u32_at(stub + 12), 0xd61f_0200, "`br x16` at {:#x}", stub + 12);
    let cell = adrp_page(adrp, stub) + u64::from((cell >> 10) & 0xfff) * 8;
    peek.u64_at(cell) + u64::from((entry >> 10) & 0xfff) * 8
}

/// What a call went through last on its way to where it lands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Via {
    Direct,
    Thunk,
    Slot,
}

/// Where the `bl` at `from` lands: its target, through a thunk and through a slot's stub
/// where it goes to one.
fn lands(peek: &Peek<'_>, from: u64) -> (u64, Via) {
    let word = peek.u32_at(from);
    assert_eq!(word >> 26, 0x25, "a `bl` at {from:#x}: {word:#010x}");
    let imm = ((word & 0x03ff_ffff) << 6) as i32 >> 6;
    let mut at = from.wrapping_add((i64::from(imm) * 4) as u64);
    let mut via = Via::Direct;
    // A thunk at most, then a stub at most.
    for _ in 0..2 {
        let (first, second) = (peek.u32_at(at), peek.u32_at(at + 4));
        if first & 0x9f00_001f != 0x9000_0010 {
            break;
        }
        if second & 0xffc0_03ff == 0x9100_0210 && via == Via::Direct {
            at = adrp_page(first, at) + u64::from((second >> 10) & 0xfff);
            via = Via::Thunk;
        } else if second & 0xffc0_03ff == 0xf940_0210 {
            at = peek.u64_at(slot_entry(peek, at));
            via = Via::Slot;
            break;
        } else {
            break;
        }
    }
    (at, via)
}

/// What the oracle of [`every_call_reaches_the_code_an_edit_left`] finds wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Finding {
    /// A call lands where an earlier version of its callee was: on code an edit replaced.
    Replaced { caller: u8, callee: u8, via: Via },
    /// A call lands on neither its callee nor code that ever was it.
    Elsewhere { caller: u8, callee: u8, via: Via },
    /// A leaf's address holds another version's code.
    OldCode { callee: u8 },
}

/// An incremental AArch64 image larger than a branch's reach, as the dogfood report's: `one`
/// (1) and `two` (2) at the start of the code, 200 MiB of `big` (3), then `four` (4) and
/// `five` (5), and the patch region past them. `one` and `four` each call the leaves `two`
/// and `five`, through their slots or, with `direct`, directly (the build before the rule
/// that an edit's callee has a slot). Beside it, the model: each leaf's version, and every
/// address a leaf's code was at before an edit replaced it.
struct Far {
    image: Image,
    disk: Disk,
    direct: bool,
    versions: [u8; 2],
    was: Vec<(u8, u64)>,
}

impl Far {
    fn linked(direct: bool, versions: [u8; 2]) -> Far {
        let to: fn(blaze_codegen::InstanceKey) -> Target =
            if direct { Target::Direct } else { Target::Function };
        let both = [to(key(2)), to(key(5))];
        let (one, four, big) = (calls(1, &both), calls(4, &both), filler(3));
        let (two, five) = (leaf(2, versions[0]), leaf(5, versions[1]));
        let mut image = Image::new(name(TO), a64(true)).unwrap();
        let mut disk = Disk::default();
        let change = Change {
            added: vec![&one, &two, &big, &four, &five],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            debug_names: vec![
                (key(1), "one".into()),
                (key(2), "two".into()),
                (key(3), "big".into()),
                (key(4), "four".into()),
                (key(5), "five".into()),
            ],
            ..Change::default()
        };
        disk.link(&mut image, change).unwrap_or_else(|it| panic!("from scratch: {it}"));
        Far { image, disk, direct, versions, was: Vec::new() }
    }

    /// Where function `n`'s code is.
    fn body(&self, n: u8) -> u64 {
        let body = self.image.symbols.find_body(&key(n));
        body.and_then(|it| self.image.state.addr_of(it)).unwrap_or_else(|| panic!("no {n}"))
    }

    /// An edit: leaf `which` filled again under its own key as `version`, as a splice. A
    /// refusal leaves the image and the model as they were.
    fn edit(&mut self, which: usize, version: u8) -> Result<(), crate::LinkError> {
        let (n, debug) = LEAVES[which];
        let new = leaf(n, version);
        let old = self.body(n);
        let change = Change {
            added: vec![&new],
            removed: vec![key(n)],
            debug_names: vec![(key(n), debug.into())],
            ..Change::default()
        };
        self.disk.link(&mut self.image, change)?;
        self.was.push((n, old));
        self.versions[which] = version;
        Ok(())
    }

    /// The oracle: every call lands on its callee's code as the image has it now, directly,
    /// through a thunk or through its slot, and that code is the callee's last version.
    /// `planted` is written over the image first.
    fn finding(&self, planted: Vec<(u64, u32)>) -> Option<Finding> {
        let peek = Peek { file: self.disk.file(TO), planted };
        for (caller, place, callee) in CALLS {
            let (at, via) = lands(&peek, self.body(caller) + 4 * place);
            if at == self.body(callee) {
                continue;
            }
            return Some(match self.was.contains(&(callee, at)) {
                true => Finding::Replaced { caller, callee, via },
                false => Finding::Elsewhere { caller, callee, via },
            });
        }
        for ((leaf, _), version) in LEAVES.iter().zip(self.versions) {
            let at = self.body(*leaf);
            let nops = (0..4u64).take_while(|it| peek.u32_at(at + 4 * it) == NOP).count();
            if nops != usize::from(version) || peek.u32_at(at + 4 * nops as u64) != RET_WORD {
                return Some(Finding::OldCode { callee: *leaf });
            }
        }
        None
    }
}

/// The invariant: after any splice that succeeds, every branch in the image reaches its
/// target directly or through a thunk, and every call to code an edit replaced reaches the
/// replacement.
///
/// Drawn: whether the callers call the leaves through their slots or directly, and up to
/// three edits, each a leaf filled again under its own key. A splice refused must be the
/// typed one for a thunk out of reach, and the link from scratch that takes its place is
/// held to the same oracle. Each case links 200 MiB, so there are few.
#[hegel::test(test_cases = 4)]
fn every_call_reaches_the_code_an_edit_left(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let direct = tc.draw(gs::integers::<u8>().max_value(1)) == 1;
    let edits: Vec<(u8, u8)> = tc.draw(
        gs::vecs(gs::tuples!(gs::integers::<u8>().max_value(1), gs::integers::<u8>().max_value(3)))
            .max_size(3),
    );
    let mut far = Far::linked(direct, [0, 0]);
    assert_eq!(far.finding(Vec::new()), None, "from scratch, direct {direct}");
    for (which, version) in edits {
        let which = usize::from(which);
        if far.versions[which] == version {
            continue;
        }
        if let Err(error) = far.edit(which, version) {
            assert!(error.no_thunk().is_some(), "refused, and not for a thunk: {error}");
            assert!(direct, "a callee reached through its slot was refused: {error}");
            let mut versions = far.versions;
            versions[which] = version;
            far = Far::linked(direct, versions);
        }
        assert_eq!(far.finding(Vec::new()), None, "after {which} as {version}, direct {direct}");
    }
}

/// `bl` at `from` to `to`.
fn bl_to(from: u64, to: u64) -> u32 {
    0x9400_0000 | ((to.wrapping_sub(from) as i64 >> 2) as u32 & 0x03ff_ffff)
}

/// A control of [`every_call_reaches_the_code_an_edit_left`]: the edit splices and the
/// oracle finds nothing; with `one`'s call written as a direct `bl` to where `two` was before
/// the edit (a caller the splice left behind), it names that call.
#[test]
fn the_oracle_catches_a_stale_direct_branch_to_replaced_code() {
    let mut far = Far::linked(false, [0, 0]);
    let (one, old) = (far.body(1), far.body(2));
    far.edit(0, 1).unwrap_or_else(|it| panic!("the edit: {it}"));
    assert_eq!(far.finding(Vec::new()), None);
    assert_eq!(
        far.finding(vec![(one, bl_to(one, old))]),
        Some(Finding::Replaced { caller: 1, callee: 2, via: Via::Direct })
    );
}

/// A control of [`every_call_reaches_the_code_an_edit_left`]: with `two`'s slot left on
/// the code the edit replaced (a splice that placed the new code and did not change the
/// slot's entry), the oracle names the first call through it.
#[test]
fn the_oracle_catches_a_slot_left_on_replaced_code() {
    let mut far = Far::linked(false, [0, 0]);
    let old = far.body(2);
    far.edit(0, 2).unwrap_or_else(|it| panic!("the edit: {it}"));
    assert_eq!(far.finding(Vec::new()), None);
    let stub = far.image.symbols.find_slot(&key(2)).and_then(|it| far.image.state.addr_of(it));
    let stub = stub.expect("an incremental image has a slot per function");
    let entry = slot_entry(&Peek { file: far.disk.file(TO), planted: Vec::new() }, stub);
    assert_eq!(
        far.finding(vec![(entry, old as u32), (entry + 4, (old >> 32) as u32)]),
        Some(Finding::Replaced { caller: 1, callee: 2, via: Via::Slot })
    );
}

/// A control of [`every_call_reaches_the_code_an_edit_left`]: a leaf's version the model
/// has and the image does not (a splice that reported an edit it did not place).
#[test]
fn the_oracle_catches_an_edit_the_image_does_not_hold() {
    let mut far = Far::linked(false, [0, 0]);
    assert_eq!(far.finding(Vec::new()), None);
    far.versions[1] = 3;
    assert_eq!(far.finding(Vec::new()), Some(Finding::OldCode { callee: 5 }));
}

/// The dogfood report's edit with the callee reached through its slot, as the codegen
/// driver now has every function an edit fills again under its own key: the stage is a
/// splice, not a refusal. `one`, 200 MiB from the patch region, is not written again (its
/// words are what the link from scratch wrote), `two`'s new code is out of its reach, and
/// its call still lands on it. Edits after it splice too.
#[test]
fn an_edit_of_a_callee_reached_through_its_slot_splices_past_its_callers_reach() {
    let mut far = Far::linked(false, [0, 0]);
    let one = far.body(1);
    let words = |far: &Far| {
        let peek = Peek { file: far.disk.file(TO), planted: Vec::new() };
        [peek.u32_at(one), peek.u32_at(one + 4), peek.u32_at(one + 8)]
    };
    let before = words(&far);
    let generation = far.image.generation();
    for (which, version) in [(0, 1), (1, 2), (0, 3)] {
        far.edit(which, version).unwrap_or_else(|it| panic!("{which} as {version}: {it}"));
        assert_eq!(far.finding(Vec::new()), None, "{which} as {version}");
    }
    assert!(far.image.generation() > generation, "three stages committed");
    assert_eq!(far.body(1), one, "the caller is where it was");
    assert_eq!(words(&far), before, "and no word of it was written again");
    assert!(far.body(2).abs_diff(one) > 1 << 27, "the new code is out of a `bl`'s reach");
    assert_eq!(far.was.len(), 3);
}
