//! Lazy images ([`crate::lazy`]) through whole dev-run links, an x86-64 ELF one and an AArch64
//! Mach-O one: every call of a lazy function reaching its slot stub, each lazy slot starting at
//! its entry in the stub region, the region and its hook cell, and [`LazyPlacer`] placing,
//! publishing and replacing code in a code memory, its counters, the history and the errors.
//!
//! The link checks run on every host, reading the image's placed bytes. The placer's need a
//! code memory laid out over the image ([`CodeMemory::lay_out`]), which reads the image's own
//! headers: they run where the image's format and architecture are the host's (x86-64 Linux
//! for the ELF case, AArch64 macOS for the Mach-O one), and elsewhere check only that the
//! image links.

use std::collections::BTreeMap;
use std::sync::Arc;

use blaze_codegen::plan::{Mode, OptLevel, Override, Policy, Profile};
use blaze_codegen::settings::{DebugInfo, Panic, Pgo, Relocation as Pic, Sanitize, Toolchain};
use blaze_codegen::{Arch, CompiledFunction, InstanceKey, RelocKind, Relocation, Target};
use blaze_ids::ContentId;
use blaze_inproc::{CodeMemory, CodeRange};
use blaze_items::{CodeHash, ItemKey};
use blaze_lazy::plan::Function as Walked;
use blaze_lazy::runner::Placer as _;
use blaze_lazy::stub::{Abi, Isa, Stubs, X86Save};
use blaze_lazy::{Plan, Purpose, Session, hook};
use blaze_live::patch::Message;
use parking_lot::Mutex;

use super::{Change, Disk, GROWTH, Image, Output, OutputKind, TO, function, id, key, name, output};
use crate::model::{Class, Reserve};
use crate::output::Platform;
use crate::{DevRun, Format, LazyPlacer, Live, Slot, Slots};

/// The entry point, the start hook, the lazy hook and two functions the image links.
const ENTRY: u8 = 1;
const HOOK: u8 = 9;
const RESOLVE: u8 = 8;
const CALLER: u8 = 2;
const CALLER2: u8 = 3;
/// A function the image links that counts, given its pair by the stage.
const F: u8 = 4;
/// A function a later stage adds, counting.
const G: u8 = 5;
/// The lazy functions, in plan order.
const L0: u8 = 20;
const L1: u8 = 21;
const L2: u8 = 22;
const L3: u8 = 23;
/// The keys of the code that replaces a lazy function's tier 0.
const T0: u8 = 30;
/// A key nothing links.
const NOWHERE: u8 = 77;

const LAZY: [u8; 4] = [L0, L1, L2, L3];

/// A multiple of every page here (4 KiB ELF x86-64, 16 KiB Mach-O AArch64 and macOS).
const GRANULE: u64 = 16 << 10;

/// What one placement may allocate, its buffers grown already: the slot's entry in the
/// placer's map, and what the code memory's arena and publication take.
const PLACE_BUDGET: u64 = 16;

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

    fn isa(self) -> Isa {
        match self {
            Case::Elf => Isa::X86_64 { abi: Abi::SysV, save: X86Save::Fx },
            Case::MachO => Isa::Aarch64 { sve: false },
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

    /// A lazy image's output: a dev run with room for four counter pairs.
    fn lazy(self) -> Output {
        self.output(Some(DevRun { arena: GRANULE * 4, cells: 4 * 16, fixed: 0 }))
    }

    fn entry(self) -> &'static str {
        match self {
            Case::Elf => "_start",
            Case::MachO => "main",
        }
    }

    /// The program: `functions`, named as the entry point, the start hook and the lazy hook
    /// ask.
    fn change<'a>(self, functions: &[&'a CompiledFunction]) -> Change<'a> {
        let mut change = self.unhooked(functions);
        change.names.push((hook::SYMBOL.into(), Target::Direct(key(RESOLVE))));
        change
    }

    /// [`Case::change`] without the lazy hook.
    fn unhooked<'a>(self, functions: &[&'a CompiledFunction]) -> Change<'a> {
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

fn policy(arch: Arch) -> Policy {
    Policy {
        toolchain: Toolchain {
            id: ContentId([1; 32]),
            target: "blaze-link lazy test".to_owned(),
            arch,
            cpu: "generic".to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: None,
            stencils: None,
            frame_pointers: true,
            relocation: Pic::Pic,
        },
        profile: Profile {
            opt_level: OptLevel::O0,
            overflow_checks: true,
            debug_assertions: true,
            debuginfo: DebugInfo::Full,
            panic: Panic::Unwind,
            packages: BTreeMap::new(),
            build_override: Override::default(),
        },
        mode: Mode::Live,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
    }
}

/// The session whose lazy functions are `lazy`, in that order, and its stub region.
fn session(case: Case, lazy: &[u8]) -> (Session, Stubs) {
    let functions: Vec<Walked> =
        lazy.iter().map(|n| Walked { key: key(*n), code: CodeHash(id(100 + n)), item: ItemKey(id(150 + n)) }).collect();
    let plan = Plan::new(&functions, &[], &[]).unwrap();
    let policy = policy(case.arch());
    Session::new(&Purpose { profile: "dev", policy: &policy, output: None }, plan, &case.isa(), 0).unwrap()
}

fn ret(arch: Arch) -> Vec<u8> {
    match arch {
        Arch::X86_64 => vec![0xc3],
        _ => 0xd65f_03c0u32.to_le_bytes().to_vec(),
    }
}

fn plain(n: u8, arch: Arch) -> CompiledFunction {
    CompiledFunction { arch, ..function(n, &ret(arch), Vec::new()) }
}

/// Function `n`, calling each of `targets` in turn (`call rel32`, `bl`), then returning.
fn calls(n: u8, arch: Arch, targets: &[Target]) -> CompiledFunction {
    let mut code = Vec::with_capacity(targets.len() * 5 + 4);
    let mut relocs = Vec::with_capacity(targets.len());
    for target in targets {
        match arch {
            Arch::X86_64 => {
                relocs.push(Relocation { at: code.len() as u32 + 1, kind: RelocKind::PcRel32, target: target.clone(), addend: -4 });
                code.extend_from_slice(&[0xe8, 0, 0, 0, 0]);
            }
            _ => {
                relocs.push(Relocation { at: code.len() as u32, kind: RelocKind::Call26, target: target.clone(), addend: 0 });
                code.extend_from_slice(&0x9400_0000u32.to_le_bytes());
            }
        }
    }
    code.extend_from_slice(&ret(arch));
    CompiledFunction { arch, ..function(n, &code, relocs) }
}

/// Where the first `count` calls of [`calls`]'s code at `addr` go, decoded from its bytes.
fn called(arch: Arch, addr: u64, code: &[u8], count: usize) -> Vec<u64> {
    let word = |at: usize| u32::from_le_bytes(code[at..][..4].try_into().unwrap());
    (0..count)
        .map(|n| match arch {
            Arch::X86_64 => (addr + n as u64 * 5 + 5).wrapping_add_signed(i64::from(word(n * 5 + 1) as i32)),
            _ => {
                let words = ((word(n * 4) & 0x03ff_ffff) << 6) as i32 >> 6;
                (addr + n as u64 * 4).wrapping_add_signed(i64::from(words) * 4)
            }
        })
        .collect()
}

/// Where a branch to `to` lands: past the veneer (`adrp x16; add x16, x16, lo12; br x16`) in
/// the arena that AArch64 placed code reaches a target in the image past a `bl`'s reach by.
fn past_veneer(placed: &Placed, arch: Arch, to: u64) -> u64 {
    let in_arena = {
        let memory = placed.memory.lock();
        memory.offset_of(to).is_some_and(|it| it >= memory.image_len())
    };
    if arch == Arch::X86_64 || !in_arena {
        return to;
    }
    let code = placed.code(to, 12);
    let word = |at: usize| u32::from_le_bytes(code[at..][..4].try_into().unwrap());
    assert_eq!(word(8), 0xd61f_0200, "`br x16` at {:#x}", to + 8);
    let imm = (((word(0) >> 5) & 0x7_ffff) << 2) | ((word(0) >> 29) & 3);
    let pages = i64::from(((imm << 11) as i32) >> 11);
    (to & !0xfff).wrapping_add_signed(pages << 12) + u64::from((word(4) >> 10) & 0xfff)
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

/// The addresses [`counting`]'s code at `addr` reaches: x86-64 its two `add`s' targets,
/// AArch64 its `ldr`'s, its `str`'s and its second `add`'s.
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

/// `len` bytes of the image at `addr`, as the image holds them (plain values, before a
/// writer encodes any).
fn at(image: &Image, addr: u64, len: u64) -> Vec<u8> {
    let space = image.state.spaces.iter().find(|it| (it.region.addr..it.region.addr + it.region.size).contains(&addr));
    let space = space.unwrap_or_else(|| panic!("{addr:#x} is in no region"));
    space.bytes.read((addr - space.region.addr) as usize, len as usize).into_owned()
}

fn word_at(image: &Image, addr: u64) -> u64 {
    u64::from_le_bytes(at(image, addr, 8).try_into().unwrap())
}

/// `n`'s code: its address and bytes.
fn code_of(image: &Image, n: u8) -> (u64, Vec<u8>) {
    let body = image.symbols.find_body(&key(n)).expect("linked");
    let (piece, _) = image.state.home(body).expect("placed");
    let piece = image.state.piece(piece).expect("live");
    (piece.addr, at(image, piece.addr, piece.size))
}

fn body_addr(image: &Image, n: u8) -> u64 {
    image.state.addr_of(image.symbols.find_body(&key(n)).expect("linked")).expect("placed")
}

fn slot_stub(image: &Image, n: u8) -> u64 {
    image.state.addr_of(image.symbols.find_slot(&key(n)).expect("a slot")).expect("placed")
}

/// What slot `slot`'s cell holds.
fn cell(image: &Image, slot: Slot) -> u64 {
    let cells = image.state.addr_of(image.symbols.find(crate::symbols::SymKey::Cells).expect("a jump table")).unwrap();
    word_at(image, cells + u64::from(slot.0) * 8)
}

/// The address of the image's cells reservation.
fn cells_at(image: &Image) -> u64 {
    let space = image.state.spaces.iter().find(|it| it.region.class == Class::Reserve(Reserve::Cells));
    space.expect("a cells reservation").region.addr
}

/// A [`Slots`] that stores nothing and records each publication.
#[derive(Clone)]
struct Recorder(Arc<Mutex<Vec<(Slot, u64)>>>);

impl Recorder {
    fn new() -> Recorder {
        // Room for every test's publications: one inside an allocation budget grows nothing.
        Recorder(Arc::new(Mutex::new(Vec::with_capacity(64))))
    }

    fn published(&self) -> Vec<(Slot, u64)> {
        self.0.lock().clone()
    }
}

impl Slots for Recorder {
    fn publish(&mut self, slot: Slot, target: u64) -> Result<(), String> {
        self.0.lock().push((slot, target));
        Ok(())
    }
}

/// A [`Slots`] whose run processes all refuse.
struct Refusing;

impl Slots for Refusing {
    fn publish(&mut self, _: Slot, _: u64) -> Result<(), String> {
        Err("the run process exited".into())
    }
}

/// A lazy image of `functions` and the session's lazy functions, linked once.
fn linked(case: Case, output: Output, functions: &[&CompiledFunction]) -> (Image, Session, Stubs) {
    let (session, stubs) = session(case, &LAZY);
    let mut image = Image::new(name(TO), output).unwrap();
    let mut disk = Disk::default();
    let staged = image.stage_lazy(case.change(functions), &session, &stubs).unwrap();
    let mut effects = Vec::new();
    image.commit(staged, &mut effects).unwrap();
    disk.run(&mut image, effects).unwrap();
    (image, session, stubs)
}

/// The program the placer tests link: the entry, both hooks, a caller of a lazy function and
/// a function that counts.
fn program(arch: Arch) -> Vec<CompiledFunction> {
    vec![
        plain(ENTRY, arch),
        plain(HOOK, arch),
        plain(RESOLVE, arch),
        calls(CALLER, arch, &[Target::Function(key(L0))]),
        counting(F, arch),
    ]
}

/// A placer over a lazy image of [`program`] committed into a laid-out code memory, at slide
/// 0, or `None` where this host can't lay the image's memory out.
struct Placed {
    image: Arc<Mutex<Image>>,
    memory: Arc<Mutex<CodeMemory>>,
    placer: LazyPlacer<Recorder>,
    recorder: Recorder,
    session: Session,
}

fn placed(case: Case, output: Output, label: &str) -> Option<Placed> {
    if !case.runs_here() {
        return None;
    }
    let arch = case.arch();
    let functions = program(arch);
    let (session, stubs) = session(case, &LAZY);
    let dev_run = output.dev_run.expect("a dev run");
    let mut image = Image::new(name(TO), output).unwrap();
    let refs: Vec<&CompiledFunction> = functions.iter().collect();
    let staged = image.stage_lazy(case.change(&refs), &session, &stubs).unwrap();
    // Room for what later links add to the file.
    let len = 2 * staged.len();
    let mut memory = CodeMemory::with_cells(&format!("blaze-link-lazy-{label}-{case:?}"), len, dev_run.arena, dev_run.cells).unwrap();
    assert_eq!(image.commit_into(staged, &mut memory), Ok(None));
    memory.lay_out(case.format()).unwrap();
    let (image, memory) = (Arc::new(Mutex::new(image)), Arc::new(Mutex::new(memory)));
    let recorder = Recorder::new();
    let placer = LazyPlacer::new(image.clone(), memory.clone(), recorder.clone(), 0).unwrap();
    Some(Placed { image, memory, placer, recorder, session })
}

impl Placed {
    /// The code the memory holds at `target`, `len` bytes.
    fn code(&self, target: u64, len: usize) -> Vec<u8> {
        let memory = self.memory.lock();
        let offset = memory.offset_of(target).expect("in the code memory");
        memory.bytes(CodeRange { offset, len: len as u64 }).unwrap().to_vec()
    }

    fn slot(&self, n: u8) -> Slot {
        self.image.lock().slot(&key(n)).expect("a slot")
    }
}

#[test]
fn every_call_of_a_lazy_function_reaches_its_slot_stub_and_its_slot_starts_at_its_entry() {
    for case in CASES {
        let arch = case.arch();
        let (entry, start, resolve) = (plain(ENTRY, arch), plain(HOOK, arch), plain(RESOLVE, arch));
        let caller = calls(CALLER, arch, &[Target::Function(key(L0)), Target::Direct(key(L0)), Target::Direct(key(L1))]);
        let (image, session, stubs) = linked(case, case.lazy(), &[&entry, &start, &resolve, &caller]);

        let (addr, code) = code_of(&image, CALLER);
        let stubs_of = vec![slot_stub(&image, L0), slot_stub(&image, L0), slot_stub(&image, L1)];
        assert_eq!(called(arch, addr, &code, 3), stubs_of, "{case:?}: by slot and directly, the slot stub");
        assert_eq!(body_addr(&image, L0), slot_stub(&image, L0), "{case:?}: a lazy function's address is its slot stub");

        for (n, lazy) in LAZY.iter().enumerate() {
            assert_eq!(image.slot(&key(*lazy)), Some(Slot(n as u32)), "{case:?}: first, in plan order");
        }
        let region = image.lazy_stubs().expect("a lazy image's region");
        assert!(region.is_multiple_of(crate::lazy::REGION_ALIGN), "{case:?}: {region:#x}");
        let entries: Vec<(InstanceKey, u64)> = session.slots(region).collect();
        assert_eq!(entries.len(), LAZY.len());
        for (lazy, entry) in entries {
            assert_eq!(cell(&image, image.slot(&lazy).unwrap()), entry, "{case:?}: {lazy:?} starts at its entry");
        }
        assert_eq!(cell(&image, image.slot(&key(CALLER)).unwrap()), body_addr(&image, CALLER), "{case:?}: others at their bodies");

        let cell_at = u64::from(blaze_lazy::stub::CELL);
        let held = at(&image, region, stubs.bytes.len() as u64);
        assert_eq!(&held[..cell_at as usize], &stubs.bytes[..cell_at as usize], "{case:?}");
        assert_eq!(&held[cell_at as usize + 8..], &stubs.bytes[cell_at as usize + 8..], "{case:?}: the region's code as generated");
        let offset = i64::from_le_bytes(held[cell_at as usize..][..8].try_into().unwrap());
        assert_eq!((region + cell_at).wrapping_add_signed(offset), body_addr(&image, RESOLVE), "{case:?}: the cell refers to the hook");
        let space = image.state.spaces.iter().find(|it| (it.region.addr..it.region.addr + it.region.size).contains(&region)).unwrap();
        assert!(space.region.prot.exec && !space.region.prot.write, "{case:?}: the region is code: {:?}", space.region);
    }
}

#[test]
fn a_lazy_image_is_a_dev_run_with_a_jump_table_and_the_hook() {
    for case in CASES {
        let arch = case.arch();
        let (session, stubs) = session(case, &LAZY);
        let (entry, start, resolve) = (plain(ENTRY, arch), plain(HOOK, arch), plain(RESOLVE, arch));
        let functions = [&entry, &start, &resolve];

        let mut image = Image::new(name(TO), case.output(None)).unwrap();
        let error = image.stage_lazy(case.change(&functions), &session, &stubs).err().expect("no dev run");
        assert!(error.what.contains("without a dev run"), "{case:?}: {error}");

        let whole = Output { incremental: false, ..case.lazy() };
        let mut image = Image::new(name(TO), whole).unwrap();
        let error = image.stage_lazy(case.change(&functions), &session, &stubs).err().expect("no jump table");
        assert!(error.what.contains("without a jump table"), "{case:?}: {error}");

        let mut image = Image::new(name(TO), case.lazy()).unwrap();
        let error = image.stage_lazy(case.unhooked(&functions), &session, &stubs).err().expect("no hook");
        let hook = crate::lazy::hook_name(case.output(None).format);
        assert!(error.what.contains(&format!("`{hook}`")), "{case:?}: {error}");
        assert_eq!(hook == "_blaze_lazy_resolve", matches!(case, Case::MachO), "{case:?}: {hook}");

        let (_, other) = self::session(case, &LAZY[..2]);
        let error = image.stage_lazy(case.change(&functions), &session, &other).err().expect("another region");
        assert!(error.what.contains("not the session's"), "{case:?}: {error}");
    }
}

#[test]
fn a_later_link_keeps_its_hands_off_lazy_slots() {
    for case in CASES {
        let arch = case.arch();
        let (entry, start, resolve) = (plain(ENTRY, arch), plain(HOOK, arch), plain(RESOLVE, arch));
        let (mut image, session, stubs) = linked(case, case.lazy(), &[&entry, &start, &resolve]);

        let l0 = plain(L0, arch);
        let error = image.stage(Change { added: vec![&l0], ..Change::default() }).err().expect("a lazy function added");
        assert_eq!(error.function, Some(key(L0)), "{case:?}: {error}");
        assert!(error.what.contains("lazy placer places"), "{case:?}: {error}");

        let t0 = plain(T0, arch);
        let tier_up = Change { added: vec![&t0], slots: vec![(key(L0), key(T0))], ..Change::default() };
        let error = image.stage(tier_up).err().expect("a lazy slot put");
        assert!(error.what.contains("lazy placer replaces"), "{case:?}: {error}");

        let error = image.stage(Change { removed: vec![key(L1)], ..Change::default() }).err().expect("a lazy function removed");
        assert_eq!(error.function, Some(key(L1)), "{case:?}: {error}");
        assert!(error.what.contains("keeps every lazy function's slot"), "{case:?}: {error}");

        let (other, other_stubs) = self::session(case, &LAZY[..3]);
        let error = image.stage_lazy(Change::default(), &other, &other_stubs).err().expect("another session");
        assert!(error.what.contains("another session"), "{case:?}: {error}");
        assert!(image.stage_lazy(Change::default(), &session, &stubs).is_ok(), "{case:?}: the same session");
    }
}

#[test]
fn the_placer_stores_the_hook_in_the_cell() {
    for case in CASES {
        let Some(placed) = placed(case, case.lazy(), "cell") else { continue };
        let image = placed.image.lock();
        let memory = placed.memory.lock();
        let region = image.lazy_stubs().unwrap();
        let at = memory.offset_of(region + u64::from(blaze_lazy::stub::CELL)).expect("in the code");
        assert!(at < memory.image_len(), "{case:?}: the region is in the image part");
        assert_eq!(memory.load_word(at).unwrap(), body_addr(&image, RESOLVE), "{case:?}: the hook's address");
        for (lazy, entry) in placed.session.slots(region) {
            assert_eq!(cell(&image, image.slot(&lazy).unwrap()), entry, "{case:?}");
        }
    }
}

#[test]
fn place_writes_code_that_resolves_publishes_it_and_stores_its_slot() {
    for case in CASES {
        let arch = case.arch();
        let Some(mut placed) = placed(case, case.lazy(), "place") else { continue };
        let l0 = calls(L0, arch, &[Target::Function(key(L1)), Target::Direct(key(L1)), Target::Direct(key(CALLER)), Target::Direct(key(L0))]);
        let target = placed.placer.place(&l0).unwrap();
        assert_eq!(placed.recorder.published(), vec![(placed.slot(L0), target)], "{case:?}: published in its slot");
        {
            let memory = placed.memory.lock();
            let offset = memory.offset_of(target).expect("in the code memory");
            assert!(offset >= memory.image_len(), "{case:?}: in the arena, at {offset:#x}");
            assert!(target.is_multiple_of(u64::from(l0.align)), "{case:?}: {target:#x}");
        }
        let code = placed.code(target, l0.code.len());
        let slot = placed.slot(L0);
        let image = placed.image.lock();
        let expected = vec![slot_stub(&image, L1), slot_stub(&image, L1), body_addr(&image, CALLER), target];
        let mut went = called(arch, target, &code, 4);
        went[2] = past_veneer(&placed, arch, went[2]);
        assert_eq!(went, expected, "{case:?}: a lazy callee not placed yet, its slot stub; a linked one its body");
        let entry = placed.session.slots(image.lazy_stubs().unwrap()).next().unwrap().1;
        assert_eq!(cell(&image, slot), entry, "{case:?}: the image's cell is still the entry: the run processes store the slot");
    }
}

#[test]
fn replace_stores_the_new_code_and_retires_the_old_behind_the_fence() {
    for case in CASES {
        let arch = case.arch();
        let Some(mut placed) = placed(case, case.lazy(), "replace") else { continue };
        placed.memory.lock().attach_forking(std::process::id());
        let tier0 = calls(L0, arch, &[Target::Function(key(L1))]);
        let old = placed.placer.place(&tier0).unwrap();
        assert_eq!(placed.memory.lock().held(), 0, "{case:?}: nothing retired");

        let tier1 = calls(T0, arch, &[Target::Direct(key(L1)), Target::Function(key(L2))]);
        let new = placed.placer.replace(key(L0), &tier1).unwrap();
        assert_ne!(new, old, "{case:?}");
        let slot = placed.slot(L0);
        assert_eq!(placed.recorder.published(), vec![(slot, old), (slot, new)], "{case:?}: the tier-up in the lazy slot");
        let code = placed.code(new, tier1.code.len());
        let image = placed.image.lock();
        assert_eq!(called(arch, new, &code, 2), vec![slot_stub(&image, L1), slot_stub(&image, L2)], "{case:?}");
        drop(image);
        let held = placed.memory.lock().held();
        assert!(held >= tier0.code.len() as u64, "{case:?}: the tier-0 code waits behind the fence, {held} bytes");

        let error = placed.placer.replace(key(NOWHERE), &tier1).unwrap_err();
        assert!(error.contains("which the image has none of"), "{case:?}: {error}");
    }
}

#[test]
fn a_placed_function_counts_in_its_own_pair_and_a_linked_one_in_the_stage_s() {
    for case in CASES {
        let arch = case.arch();
        let Some(mut placed) = placed(case, case.lazy(), "counters") else { continue };
        let (cells, stage_pair) = {
            let image = placed.image.lock();
            let (addr, code) = code_of(&image, F);
            (cells_at(&image), reached(arch, addr, &code)[0])
        };
        assert_eq!(stage_pair, cells, "{case:?}: the stage gave F the first pair");

        let f = counting(F, arch);
        let at = placed.placer.place(&f).unwrap();
        assert_eq!(reached(arch, at, &placed.code(at, f.code.len())), expected(arch, cells), "{case:?}: placed, the stage's pair");

        let lazy = counting(L1, arch);
        let at = placed.placer.place(&lazy).unwrap();
        let top = cells + 3 * 16;
        assert_eq!(reached(arch, at, &placed.code(at, lazy.code.len())), expected(arch, top), "{case:?}: a new pair, from the top");
        let offset = placed.placer.cells(&key(L1)).expect("a pair");
        {
            let image = placed.image.lock();
            let memory = placed.memory.lock();
            assert_eq!(Some(offset), image.counter(&key(L1)), "{case:?}");
            assert_eq!(memory.offset_of(top), Some(offset), "{case:?}: the offset of the pair the code reaches");
        }
        assert_eq!(placed.placer.cells(&key(NOWHERE)), None);

        let g = counting(G, arch);
        let staged = placed.image.lock().stage(Change { added: vec![&g], ..Change::default() }).unwrap();
        let l2 = counting(L2, arch);
        placed.placer.place(&l2).unwrap();
        let mut image = placed.image.lock();
        let mut memory = placed.memory.lock();
        assert!(image.commit_into(staged, &mut memory).is_ok(), "{case:?}: a stage from before the placement commits");
        let pair = |n: u8| image.counter(&key(n)).map(|it| (it - memory.cells().offset) / 16);
        assert_eq!((pair(F), pair(G), pair(L1), pair(L2)), (Some(0), Some(1), Some(3), Some(2)), "{case:?}: kept and carried");
        let (addr, code) = code_of(&image, G);
        assert_eq!(reached(arch, addr, &code), expected(arch, cells + 16), "{case:?}");
    }
}

#[test]
fn the_placer_never_stores_a_slot_of_an_armed_execution_log() {
    for case in CASES {
        // The runtime finds the log through the live descriptor.
        let Some(mut placed) = placed(case, Output { exec_log: true, live: Some(Live), ..case.lazy() }, "log") else { continue };
        let snapshot = |placed: &Placed| {
            let memory = placed.memory.lock();
            memory.bytes(CodeRange { offset: 0, len: memory.image_len() }).unwrap().to_vec()
        };
        let before = snapshot(&placed);
        let l0 = calls(L0, case.arch(), &[Target::Function(key(L1))]);
        let target = placed.placer.place(&l0).unwrap();
        let t0 = calls(T0, case.arch(), &[Target::Function(key(L2))]);
        let again = placed.placer.replace(key(L0), &t0).unwrap();
        assert_eq!(before, snapshot(&placed), "{case:?}: no table of the image written, the log's least of all");
        let slot = placed.slot(L0);
        assert_eq!(placed.recorder.published(), vec![(slot, target), (slot, again)], "{case:?}: only by the run processes' rule");
    }
}

#[test]
fn a_commit_and_a_rollback_never_name_a_lazy_slot() {
    for case in CASES {
        let arch = case.arch();
        let Some(mut placed) = placed(case, Output { live: Some(Live), ..case.lazy() }, "history") else { continue };
        let l0 = calls(L0, arch, &[Target::Function(key(L1))]);
        let target = placed.placer.place(&l0).unwrap();
        let lazy: Vec<Slot> = LAZY.iter().map(|n| placed.slot(*n)).collect();

        let caller2 = calls(CALLER2, arch, &[Target::Function(key(L0)), Target::Direct(key(L1))]);
        let mut image = placed.image.lock();
        let mut memory = placed.memory.lock();
        let cells_before: Vec<u64> = lazy.iter().map(|slot| cell(&image, *slot)).collect();
        let tier_up = Change { added: vec![&caller2], slots: vec![(key(CALLER), key(CALLER2))], ..Change::default() };
        let staged = image.stage(tier_up).unwrap();
        let message = image.commit_into(staged, &mut memory).unwrap().expect("a live image's patch");
        let Ok(Message::Patch(patch)) = Message::decode(&message) else { panic!("{case:?}: not a patch") };
        let caller = image.slot(&key(CALLER)).unwrap();
        assert!(patch.cells.contains(&(caller.0, body_addr(&image, CALLER2))), "{case:?}: the tier-up's slot: {:?}", patch.cells);
        assert!(patch.cells.iter().all(|(slot, _)| !lazy.contains(&Slot(*slot))), "{case:?}: {:?}", patch.cells);

        let message = image.roll_back_into(&mut memory).unwrap().expect("a live image's rollback");
        assert!(matches!(Message::decode(&message), Ok(Message::RollBack { .. })), "{case:?}");
        let cells_after: Vec<u64> = lazy.iter().map(|slot| cell(&image, *slot)).collect();
        assert_eq!(cells_after, cells_before, "{case:?}: every generation's lazy cells are their entries");
        drop((image, memory));
        assert_eq!(placed.recorder.published(), vec![(lazy[0], target)], "{case:?}: the placement's store, and no other");
    }
}

#[test]
fn placing_what_the_image_cannot_reach_is_an_error_naming_the_function() {
    for case in CASES {
        let arch = case.arch();
        if let Some(mut placed) = placed(case, case.lazy(), "errors") {
            let stray = calls(L0, arch, &[Target::Function(key(NOWHERE))]);
            let error = placed.placer.place(&stray).unwrap_err();
            assert!(error.contains(&format!("{:?}", key(L0))) && error.contains(&format!("{:?}", key(NOWHERE))), "{case:?}: {error}");
            assert!(error.contains("no slot in the image"), "{case:?}: {error}");
            let direct = calls(L0, arch, &[Target::Direct(key(NOWHERE))]);
            let error = placed.placer.place(&direct).unwrap_err();
            assert!(error.contains("neither linked nor lazy"), "{case:?}: {error}");
            let foreign = calls(L0, if arch == Arch::X86_64 { Arch::Aarch64 } else { Arch::X86_64 }, &[]);
            assert!(placed.placer.place(&foreign).unwrap_err().contains("compiled for"), "{case:?}");
            let other = counting(F, arch);
            let mut theirs = counting(L1, arch);
            theirs.relocs = other.relocs.clone();
            assert!(placed.placer.place(&theirs).unwrap_err().contains("another function's code"), "{case:?}");
            assert_eq!(placed.recorder.published(), Vec::new(), "{case:?}: nothing published");
            assert_eq!(placed.memory.lock().held(), 0, "{case:?}: nothing left reserved behind the fence");

            let mut refused = LazyPlacer::new(placed.image.clone(), placed.memory.clone(), Refusing, 0).unwrap();
            let error = refused.place(&calls(L0, arch, &[])).unwrap_err();
            assert!(error.contains("publishing it in slot 0") && error.contains("exited"), "{case:?}: {error}");
        }

        if case.runs_here() {
            let (entry, start, resolve) = (plain(ENTRY, arch), plain(HOOK, arch), plain(RESOLVE, arch));
            let mut image = Image::new(name(TO), case.lazy()).unwrap();
            let staged = image.stage(case.change(&[&entry, &start, &resolve])).unwrap();
            let dev_run = case.lazy().dev_run.unwrap();
            let mut memory =
                CodeMemory::with_cells(&format!("blaze-link-lazy-plain-{case:?}"), staged.len(), dev_run.arena, dev_run.cells).unwrap();
            image.commit_into(staged, &mut memory).unwrap();
            memory.lay_out(case.format()).unwrap();
            let error = LazyPlacer::new(Arc::new(Mutex::new(image)), Arc::new(Mutex::new(memory)), Recorder::new(), 0).err();
            assert!(error.expect("not lazy").what.contains("not a lazy image"), "{case:?}");
        }
    }
}

/// What placing a lazy function of `calls` calls to another lazy function allocates, with a
/// placer that placed one of 2048 already.
fn placing(placed: &mut Placed, arch: Arch, n: u8, count: usize) -> blaze_alloc_count::Used {
    let targets = vec![Target::Function(key(L1)); count];
    let function = calls(n, arch, &targets);
    let (target, used) = blaze_alloc_count::measure(|| placed.placer.place(&function));
    target.unwrap();
    used
}

#[test]
fn a_placement_allocates_nothing_per_relocation() {
    for case in CASES {
        let arch = case.arch();
        let Some(mut placed) = placed(case, case.lazy(), "budget") else { continue };
        placing(&mut placed, arch, L0, 2048);
        let few = placing(&mut placed, arch, L2, 64);
        let many = placing(&mut placed, arch, L3, 2048);
        eprintln!("{case:?}: a placement: {few:?} with 64 relocations, {many:?} with 2048");
        assert!(many.allocations <= PLACE_BUDGET, "{case:?}: {many:?}, budget {PLACE_BUDGET}");
        assert!(many.allocations <= few.allocations + GROWTH, "{case:?}: per relocation: {few:?} with 64, {many:?} with 2048");
    }
}
