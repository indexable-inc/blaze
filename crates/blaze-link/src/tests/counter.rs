//! Tier-up counter pairs ([`crate::counter`]) through whole dev-run links, an x86-64 ELF one and
//! an AArch64 Mach-O one: each counting function's code reaching its own pair in the cells
//! reservation, the pairs kept across relinks, tier-ups and rollbacks, [`Image::counter`]
//! where the code counts, and the errors.

use blaze_codegen::{Arch, CompiledFunction, Data, InstanceKey, RelocKind, Relocation, Target};
use blaze_inproc::{CodeMemory, CodeRange};

use super::{Change, Disk, GROWTH, Image, Output, OutputKind, STAGE_BUDGET, TO, function, id, key, name, output};
use crate::model::{Class, Reserve};
use crate::output::Platform;
use crate::{DevRun, Format, LinkError};

/// The entry point, the start hook and three counting functions.
const ENTRY: u8 = 1;
const HOOK: u8 = 9;
const F: u8 = 2;
const G: u8 = 3;
const G2: u8 = 4;
const H: u8 = 5;

/// A multiple of every page here (4 KiB ELF x86-64, 16 KiB Mach-O AArch64 and macOS).
const GRANULE: u64 = 16 << 10;

/// `add qword ptr [rip + disp32], 1`: the field at 3, five bytes before the instruction's end.
const X86_ADD: [u8; 8] = [0x48, 0x83, 0x05, 0, 0, 0, 0, 0x01];

/// The calls word loaded, added to and stored, then the back edges word's address made:
/// `adrp x16; ldr x17, [x16, lo12]; add x17, x17, #1; str x17, [x16, lo12]; adrp x16;
/// add x16, x16, lo12; ret`.
const A64_COUNT: [u32; 7] = [0x9000_0010, 0xf940_0211, 0x9100_0631, 0xf900_0211, 0x9000_0010, 0x9100_0210, 0xd65f_03c0];

#[derive(Clone, Copy, Debug)]
enum Case {
    Elf,
    MachO,
}

const CASES: [Case; 2] = [Case::Elf, Case::MachO];

impl Case {
    fn arch(self) -> Arch {
        match self {
            Case::Elf => Arch::X86_64,
            Case::MachO => Arch::Aarch64,
        }
    }

    fn output(self, dev_run: Option<DevRun>) -> Output {
        match self {
            Case::Elf => Output { dev_run, ..output(true) },
            Case::MachO => Output {
                format: Format::MachO,
                arch: Arch::Aarch64,
                kind: OutputKind::Executable { entry: "main".into() },
                interpreter: None,
                platform: Some(Platform { platform: 1, minimum: 0x000b_0000, sdk: 0x000f_0000 }),
                triple: "aarch64-apple-darwin".into(),
                dev_run,
                ..output(true)
            },
        }
    }

    fn entry(self) -> &'static str {
        match self {
            Case::Elf => "_start",
            Case::MachO => "main",
        }
    }

    /// The program: `functions`, named as the entry point and the start hook ask.
    fn change<'a>(self, functions: &[&'a CompiledFunction]) -> Change<'a> {
        Change {
            added: functions.to_vec(),
            names: vec![
                (self.entry().into(), Target::Direct(key(ENTRY))),
                (crate::stage::START_HOOK.into(), Target::Direct(key(HOOK))),
            ],
            ..Change::default()
        }
    }

    /// Whether this host's code memory reads this image's headers: it runs it.
    fn runs_here(self) -> bool {
        match self {
            Case::Elf => cfg!(all(target_os = "linux", target_arch = "x86_64")),
            Case::MachO => cfg!(all(target_os = "macos", target_arch = "aarch64")),
        }
    }

    fn format(self) -> blaze_inproc::Format {
        match self {
            Case::Elf => blaze_inproc::Format::Elf,
            Case::MachO => blaze_inproc::Format::MachO,
        }
    }
}

/// A dev run with room for `pairs` counter pairs.
fn dev(pairs: u64) -> DevRun {
    DevRun { arena: GRANULE, cells: pairs * 16, fixed: 0 }
}

fn plain(n: u8, arch: Arch) -> CompiledFunction {
    let code = match arch {
        Arch::X86_64 => vec![0xc3],
        _ => 0xd65f_03c0u32.to_le_bytes().to_vec(),
    };
    CompiledFunction { arch, ..function(n, &code, Vec::new()) }
}

fn counter(at: u32, kind: RelocKind, of: u8, addend: i64) -> Relocation {
    Relocation { at, kind, target: Target::Counter(key(of)), addend }
}

/// Function `n`, adding one to its calls word and reaching its back edges word.
fn counting(n: u8, arch: Arch) -> CompiledFunction {
    match arch {
        Arch::X86_64 => {
            let mut code = Vec::with_capacity(17);
            code.extend_from_slice(&X86_ADD);
            code.extend_from_slice(&X86_ADD);
            code.push(0xc3);
            let relocs = vec![counter(3, RelocKind::PcRel32, n, -5), counter(11, RelocKind::PcRel32, n, 8 - 5)];
            function(n, &code, relocs)
        }
        _ => {
            let code: Vec<u8> = A64_COUNT.iter().flat_map(|it| it.to_le_bytes()).collect();
            let relocs = vec![
                counter(0, RelocKind::AdrpPage21, n, 0),
                counter(4, RelocKind::PageOffset12, n, 0),
                counter(12, RelocKind::PageOffset12, n, 0),
                counter(16, RelocKind::AdrpPage21, n, 8),
                counter(20, RelocKind::PageOffset12, n, 8),
            ];
            CompiledFunction { arch, ..function(n, &code, relocs) }
        }
    }
}

/// The addresses [`counting`]'s code at `addr` reaches, decoded from its bytes: x86-64 its
/// two `add`s' targets, AArch64 its `ldr`'s, its `str`'s and its second `add`'s.
fn reached(arch: Arch, addr: u64, code: &[u8]) -> Vec<u64> {
    let word = |at: u64| u32::from_le_bytes(code[at as usize..][..4].try_into().unwrap());
    match arch {
        Arch::X86_64 => [3u64, 11].iter().map(|field| (addr + field + 5).wrapping_add_signed(i64::from(word(*field) as i32))).collect(),
        _ => {
            let page = |at: u64| {
                let insn = word(at);
                let imm = (((insn >> 5) & 0x7_ffff) << 2) | ((insn >> 29) & 3);
                let pages = i64::from(((imm << 11) as i32) >> 11);
                ((addr + at) & !0xfff).wrapping_add_signed(pages << 12)
            };
            let low = |at: u64| {
                let insn = word(at);
                u64::from((insn >> 10) & 0xfff) << crate::reloc::a64_lo12_shift(insn).unwrap()
            };
            vec![page(0) + low(4), page(0) + low(12), page(16) + low(20)]
        }
    }
}

/// What [`reached`] is for a function whose pair is at `pair`.
fn expected(arch: Arch, pair: u64) -> Vec<u64> {
    match arch {
        Arch::X86_64 => vec![pair, pair + 8],
        _ => vec![pair, pair, pair + 8],
    }
}

/// `key`'s code: its address, and its bytes in `file`.
fn code_of<'f>(image: &Image, file: &'f [u8], key: &InstanceKey) -> (u64, &'f [u8]) {
    let body = image.symbols.find_body(key).expect("linked");
    let (piece, _) = image.state.home(body).expect("placed");
    let piece = image.state.piece(piece).expect("live");
    let space = &image.state.spaces[piece.region as usize];
    let at = space.offset.expect("in the file") + (piece.addr - space.region.addr);
    (piece.addr, &file[at as usize..][..piece.size as usize])
}

/// What function `n`'s code reaches, as linked in `file`.
fn reaches(image: &Image, file: &[u8], n: u8) -> Vec<u64> {
    let (addr, code) = code_of(image, file, &key(n));
    reached(image.output.arch, addr, code)
}

/// The address of the image's cells reservation.
fn cells_at(image: &Image) -> u64 {
    let space = image.state.spaces.iter().find(|it| it.region.class == Class::Reserve(Reserve::Cells));
    space.expect("a cells reservation").region.addr
}

/// The pair function `n` was given.
fn pair_of(image: &Image, n: u8) -> Option<u32> {
    image.state.pairs.of(image.symbols.find_id(&id(n))?)
}

fn file(disk: &Disk) -> Vec<u8> {
    let it = disk.file(TO);
    (0..it.len).map(|at| it.page(at / super::PAGE)[(at % super::PAGE) as usize]).collect()
}

#[test]
fn two_counting_functions_reach_distinct_pairs_in_the_cells_reservation() {
    for case in CASES {
        let arch = case.arch();
        let (entry, hook, f, g) = (plain(ENTRY, arch), plain(HOOK, arch), counting(F, arch), counting(G, arch));
        let mut image = Image::new(name(TO), case.output(Some(dev(4)))).unwrap();
        let mut disk = Disk::default();
        disk.link(&mut image, case.change(&[&entry, &hook, &f, &g])).unwrap();
        let bytes = file(&disk);
        let cells = cells_at(&image);
        assert_eq!((pair_of(&image, F), pair_of(&image, G)), (Some(0), Some(1)), "{case:?}: in the order first seen");
        assert_eq!(pair_of(&image, ENTRY), None, "{case:?}: a function that doesn't count has none");
        assert_eq!(image.state.pairs.given(), 2);
        assert_eq!(reaches(&image, &bytes, F), expected(arch, cells), "{case:?}: the first pair");
        assert_eq!(reaches(&image, &bytes, G), expected(arch, cells + 16), "{case:?}: the second pair");
        let space = image.state.spaces.iter().find(|it| it.region.class == Class::Reserve(Reserve::Cells)).unwrap();
        assert!(space.zero && space.offset.is_none() && space.region.size == 0, "{case:?}: {:?}", space.region);
        assert_eq!(image.counter(&key(F)), None, "{case:?}: not committed into a code memory");
    }
}

#[test]
fn a_relink_keeps_pairs_and_a_tier_up_gets_a_new_one_while_the_old_stays() {
    for case in CASES {
        let arch = case.arch();
        let (entry, hook, f, g, g2) =
            (plain(ENTRY, arch), plain(HOOK, arch), counting(F, arch), counting(G, arch), counting(G2, arch));
        let mut image = Image::new(name(TO), case.output(Some(dev(4)))).unwrap();
        let mut disk = Disk::default();
        disk.link(&mut image, case.change(&[&entry, &hook, &f, &g])).unwrap();
        let cells = cells_at(&image);

        let tier_up = || Change { added: vec![&g2], slots: vec![(key(G), key(G2))], ..Change::default() };
        disk.link(&mut image, tier_up()).unwrap();
        let bytes = file(&disk);
        assert_eq!(cells_at(&image), cells, "{case:?}: the reservation stays");
        assert_eq!(reaches(&image, &bytes, F), expected(arch, cells), "{case:?}: a kept function keeps its pair");
        assert_eq!(reaches(&image, &bytes, G), expected(arch, cells + 16), "{case:?}: the tier-0 code keeps its pair");
        assert_eq!(reaches(&image, &bytes, G2), expected(arch, cells + 32), "{case:?}: the tier-up's new key, a new pair");

        disk.link(&mut image, Change { removed: vec![key(G)], ..Change::default() }).unwrap();
        assert_eq!(pair_of(&image, G), Some(1), "{case:?}: a removed function's pair stays its own");
        assert_eq!(image.state.pairs.given(), 3);

        disk.roll_back(&mut image).unwrap();
        disk.roll_back(&mut image).unwrap();
        assert_eq!(image.state.pairs.given(), 3, "{case:?}: a rollback gives no pair back");
        assert_eq!(pair_of(&image, G2), Some(2), "{case:?}: the rolled-back tier-up's key keeps its pair");
        let h = counting(H, arch);
        disk.link(&mut image, Change { added: vec![&h], ..Change::default() }).unwrap();
        assert_eq!(pair_of(&image, H), Some(3), "{case:?}: a new key never gets a rolled-back key's pair");
        disk.link(&mut image, tier_up()).unwrap();
        let bytes = file(&disk);
        assert_eq!(reaches(&image, &bytes, G2), expected(arch, cells + 32), "{case:?}: tiered up again, the same pair");
        assert_eq!(reaches(&image, &bytes, H), expected(arch, cells + 48));
    }
}

#[test]
fn running_out_of_pairs_is_an_error_naming_the_function_and_how_many_there_are() {
    for case in CASES {
        let arch = case.arch();
        let (entry, hook, f, g, h) =
            (plain(ENTRY, arch), plain(HOOK, arch), counting(F, arch), counting(G, arch), counting(H, arch));
        let mut image = Image::new(name(TO), case.output(Some(dev(2)))).unwrap();
        let error = image.stage(case.change(&[&entry, &hook, &f, &g, &h])).err().expect("three pairs of two");
        assert_eq!(error.function, Some(key(H)), "{case:?}: {error}");
        assert!(error.what.contains("hold 2 pairs"), "{case:?}: {error}");

        let mut disk = Disk::default();
        disk.link(&mut image, case.change(&[&entry, &hook, &f, &g])).unwrap();
        let error = image.stage(Change { added: vec![&h], ..Change::default() }).err().expect("a third pair of two");
        assert_eq!(error.function, Some(key(H)), "{case:?}: {error}");
        assert!(error.what.contains("hold 2 pairs"), "{case:?}: {error}");
        assert_eq!(image.state.pairs.given(), 2, "{case:?}: a failed stage gives out nothing");

        let mut none = Image::new(name(TO), case.output(Some(dev(0)))).unwrap();
        let error = none.stage(case.change(&[&entry, &hook, &f])).err().expect("a dev run without cells");
        assert_eq!(error.function, Some(key(F)), "{case:?}: {error}");
        assert!(error.what.contains("hold 0 pairs"), "{case:?}: {error}");
    }
}

#[test]
fn counting_code_outside_a_dev_run_is_an_error_naming_the_function() {
    for case in CASES {
        let arch = case.arch();
        let (entry, hook, f) = (plain(ENTRY, arch), plain(HOOK, arch), counting(F, arch));
        let mut image = Image::new(name(TO), case.output(None)).unwrap();
        let error = image.stage(case.change(&[&entry, &hook, &f])).err().expect("not a dev run");
        assert_eq!(error.function, Some(key(F)), "{case:?}: {error}");
        assert!(error.what.contains("not a dev run"), "{case:?}: {error}");
    }
}

#[test]
fn counter_cells_named_by_another_function_by_data_or_by_another_kind_are_errors() {
    let case = Case::Elf;
    let (entry, hook) = (plain(ENTRY, Arch::X86_64), plain(HOOK, Arch::X86_64));
    let stage = |function: &CompiledFunction| -> LinkError {
        let mut image = Image::new(name(TO), case.output(Some(dev(4)))).unwrap();
        image.stage(case.change(&[&entry, &hook, function])).err().expect("refused")
    };
    let mut code = X86_ADD.to_vec();
    code.push(0xc3);
    let other = function(F, &code, vec![counter(3, RelocKind::PcRel32, G, -5)]);
    let error = stage(&other);
    assert_eq!(error.function, Some(key(F)), "{error}");
    assert!(error.what.contains("another function's code"), "{error}");

    let absolute = function(F, &[0; 9], vec![counter(0, RelocKind::Abs64, F, 0)]);
    let error = stage(&absolute);
    assert_eq!(error.function, Some(key(F)), "{error}");
    assert!(error.what.contains("Abs64"), "{error}");

    let mut in_data = plain(F, Arch::X86_64);
    in_data.data.push(Data { id: id(70), align: 8, bytes: vec![0; 8], relocs: vec![counter(0, RelocKind::Abs64, F, 0)].into(), writable: false });
    let error = stage(&in_data);
    assert_eq!(error.function, Some(key(F)), "{error}");
    assert!(error.what.contains("named in data"), "{error}");
}

const SUPPORTED: bool = cfg!(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
));

/// A code memory with room for `len` bytes of image and `dev_run`'s arena and cells, or none
/// where blaze-inproc does not run (and then making one says so).
fn code_memory(name: &str, len: u64, dev_run: DevRun) -> Option<CodeMemory> {
    match CodeMemory::with_cells(name, len, dev_run.arena, dev_run.cells) {
        Ok(memory) => Some(memory),
        Err(error) => {
            assert!(!SUPPORTED, "{error}");
            None
        }
    }
}

#[test]
fn counter_is_the_offset_in_the_code_memory_where_the_code_counts() {
    for case in CASES {
        let arch = case.arch();
        let (entry, hook, f, g) = (plain(ENTRY, arch), plain(HOOK, arch), counting(F, arch), counting(G, arch));
        let mut image = Image::new(name(TO), case.output(Some(dev(4)))).unwrap();
        let staged = image.stage(case.change(&[&entry, &hook, &f, &g])).unwrap();
        let len = staged.len();
        let small = code_memory(&format!("blaze-link-counter-small-{case:?}"), len, DevRun { cells: 0, ..dev(4) });
        let Some(mut small) = small else { return };
        let error = image.commit_into(staged, &mut small).err().expect("no cells part");
        assert!(error.what.contains("cells part"), "{case:?}: {error}");

        let staged = image.stage(case.change(&[&entry, &hook, &f, &g])).unwrap();
        let mut memory = code_memory(&format!("blaze-link-counter-{case:?}"), len, dev(4)).expect("made above");
        assert_eq!(image.commit_into(staged, &mut memory), Ok(None));
        let bytes = memory.bytes(CodeRange { offset: 0, len }).unwrap().to_vec();
        let cells = cells_at(&image);
        let laid_out = case.runs_here();
        if laid_out {
            memory.lay_out(case.format()).unwrap();
        }
        for n in [F, G] {
            let calls = reaches(&image, &bytes, n)[0];
            let at = image.counter(&key(n)).expect("a pair");
            assert_eq!(at, memory.cells().offset + (calls - cells), "{case:?}: function {n}");
            assert!(at.is_multiple_of(16) && at + 16 <= memory.cells().end(), "{case:?}: {at:#x}");
            if laid_out {
                assert_eq!(memory.offset_of(calls), Some(at), "{case:?}: the run's address of the calls word");
                assert_eq!(memory.linked(at), Some(calls));
            }
        }
        assert_eq!(image.counter(&key(ENTRY)), None, "{case:?}: a function that doesn't count");
        assert_eq!(image.counter(&key(77)), None, "{case:?}: a key the image never saw");
        let at = blaze_alloc_count::assert_at_most(0, "Image::counter", || image.counter(&key(F)));
        assert!(at.is_some());
    }
}

/// What staging a tier-up of a counting function allocates, its code adding `adds` times to
/// its calls word, in an incremental dev run already holding one. One worker thread.
fn staged_counting(adds: u32) -> blaze_alloc_count::Used {
    let pool = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    pool.install(|| {
        let case = Case::Elf;
        let many = |n: u8| {
            let mut code = Vec::with_capacity(adds as usize * 8 + 1);
            let mut relocs = Vec::with_capacity(adds as usize);
            for at in 0..adds {
                relocs.push(counter(at * 8 + 3, RelocKind::PcRel32, n, -5));
                code.extend_from_slice(&X86_ADD);
            }
            code.push(0xc3);
            function(n, &code, relocs)
        };
        let (entry, hook, g, g2) = (plain(ENTRY, Arch::X86_64), plain(HOOK, Arch::X86_64), many(G), many(G2));
        let mut image = Image::new(name(TO), case.output(Some(dev(4)))).unwrap();
        let mut disk = Disk::default();
        disk.link(&mut image, case.change(&[&entry, &hook, &g])).unwrap();
        let next = Change { added: vec![&g2], slots: vec![(key(G), key(G2))], ..Change::default() };
        let (staged, used) = blaze_alloc_count::measure(|| image.stage(next));
        let mut effects = Vec::new();
        image.commit(staged.unwrap(), &mut effects).unwrap();
        disk.run(&mut image, effects).unwrap();
        used
    })
}

#[test]
fn counter_relocations_allocate_nothing_per_relocation() {
    let few = staged_counting(64);
    let many = staged_counting(2048);
    eprintln!("a counting tier-up stage: {few:?} with 64 counter relocations, {many:?} with 2048");
    assert!(many.allocations <= STAGE_BUDGET, "{many:?}, budget {STAGE_BUDGET}");
    assert!(many.allocations <= few.allocations + GROWTH, "per counter relocation: {few:?} with 64, {many:?} with 2048");
}
