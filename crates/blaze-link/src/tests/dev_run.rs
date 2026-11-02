//! Dev executables ([`crate::DevRun`]) through a full x86-64 ELF link: the start hook's pointer
//! first among the initializers, the reservations after everything and where they were across
//! incremental stages, and commits into a code memory holding the very bytes of the file.

use std::sync::Arc;

use blaze_codegen::{CompiledFunction, Target};
use blaze_inproc::{CodeMemory, CodeRange};
use blaze_live::patch::{Message, hash};
use object::read::elf::ProgramHeader as _;
use object::write::{Object as Builder, Relocation, StandardSection, Symbol as Def, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, Object as _, ObjectSection as _, RelocationFlags, SectionFlags, SectionKind,
    SymbolFlags, SymbolKind, SymbolScope, elf,
};

use super::{
    Change, Disk, Image, Linked, Native, NativeKind, Output, OutputKind, PAGE, Sparse, TO, code_memory, edit, first,
    function, id, key, live, name, output, patch_in, program, returns,
};
use crate::DevRun;
use crate::model::{Class, Reserve};

/// The start hook's function, which returns its own number.
const HOOK: u8 = 9;

const NO_RESERVATIONS: DevRun = DevRun { arena: 0, cells: 0, fixed: 0 };

fn dev(base: Output, dev_run: DevRun) -> Output {
    Output { dev_run: Some(dev_run), ..base }
}

fn hook() -> CompiledFunction {
    function(HOOK, &returns(HOOK), Vec::new())
}

/// `first`'s program with the dev runtime's start hook defined by Rust code.
fn hooked<'a>(start: &'a CompiledFunction, g: &'a CompiledFunction, hook: &'a CompiledFunction) -> Change<'a> {
    let mut change = first(start, g);
    change.added.push(hook);
    change.names.push((crate::stage::START_HOOK.into(), Target::Direct(key(HOOK))));
    change.debug_names.push((key(HOOK), "hook".into()));
    change
}

/// An x86-64 ELF object whose `.preinit_array` entry points at its own `c_preinit`, which
/// returns 7.
fn preinit_object() -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = b.section_id(StandardSection::Text);
    let at = b.append_section_data(text, &returns(7), 16);
    let init = b.add_symbol(Def {
        name: "c_preinit".into(),
        value: at,
        size: 6,
        kind: SymbolKind::Text,
        scope: SymbolScope::Compilation,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    let preinit = b.add_section(Vec::new(), b".preinit_array".to_vec(), SectionKind::Data);
    b.section_mut(preinit).flags =
        SectionFlags::Elf { sh_type: elf::SHT_PREINIT_ARRAY, sh_flags: elf::SHF_ALLOC | elf::SHF_WRITE };
    let slot = b.append_section_data(preinit, &[0; 8], 8);
    b.add_relocation(preinit, Relocation {
        offset: slot,
        symbol: init,
        addend: 0,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
    })
    .unwrap();
    b.write().unwrap()
}

/// The two `.preinit_array` entries of a linked image.
fn preinit_entries(linked: &Linked) -> (u64, u64) {
    let file = linked.elf();
    let section = file.section_by_name(".preinit_array").expect("a .preinit_array");
    assert_eq!(section.size(), 16, "the start hook's pointer and the object's entry");
    let at = section.address();
    (linked.u64_at(at), linked.u64_at(at + 8))
}

#[test]
fn the_start_hook_is_the_first_preinit_entry_and_stays_first() {
    let (start, g) = program(Target::Function);
    let (hook, g2) = (hook(), function(3, &returns(2), Vec::new()));
    let object: Arc<[u8]> = preinit_object().into();
    let mut image = Image::new(name(TO), dev(output(true), NO_RESERVATIONS)).unwrap();
    let mut disk = Disk::default();
    let mut change = hooked(&start, &g, &hook);
    change.natives.push(Native { id: id(61), name: "preinit.o", bytes: &object, kind: NativeKind::Object, order: 0 });
    disk.link(&mut image, change).unwrap();
    let a = Linked::of(disk.file(TO));
    let (hook_at, object_at) = preinit_entries(&a);
    assert_eq!(a.at(hook_at, 6), returns(HOOK), "the start hook's pointer is first");
    assert_eq!(a.at(object_at, 6), returns(7), "the object's entry, of the same rank, after it");

    disk.link(&mut image, edit(&g2)).unwrap();
    let b = Linked::of(disk.file(TO));
    assert_eq!(preinit_entries(&b), (hook_at, object_at), "an incremental stage keeps the pointer first");
    assert_eq!(b.at(hook_at, 6), returns(HOOK));
}

#[test]
fn a_dev_run_without_the_dev_runtime_or_of_a_shared_image_is_an_error() {
    let (start, g) = program(Target::Function);
    let mut image = Image::new(name(TO), dev(output(true), NO_RESERVATIONS)).unwrap();
    let error = image.stage(first(&start, &g)).err().expect("no start hook to point at");
    assert!(error.what.contains("`blaze_inproc_start`") && error.what.contains("dev runtime"), "{error}");

    let shared = Output {
        kind: OutputKind::Shared { name: "libapp.so".into(), exports: Vec::new() },
        interpreter: None,
        ..dev(output(true), NO_RESERVATIONS)
    };
    let error = Image::new(name(TO), shared).err().expect("a shared image has no start hook");
    assert!(error.what.contains("shared image libapp.so"), "{error}");

    let huge = dev(output(true), DevRun { arena: u64::MAX, cells: 0, fixed: 0 });
    let error = Image::new(name(TO), huge).err().expect("past the address space");
    assert!(error.what.contains("address space"), "{error}");
}

/// Each `PT_LOAD`: its address, its size in memory and in the file, and its flags.
fn loads(linked: &Linked) -> Vec<(u64, u64, u64, u32)> {
    let file = linked.elf();
    let endian = file.endian();
    file.elf_program_headers()
        .iter()
        .filter(|it| it.p_type(endian) == elf::PT_LOAD)
        .map(|it| (it.p_vaddr(endian), it.p_memsz(endian), it.p_filesz(endian), it.p_flags(endian).0))
        .collect()
}

/// The image's reservations: each, its address and its size.
fn reservations(image: &Image) -> Vec<(Reserve, u64, u64)> {
    image
        .state
        .spaces
        .iter()
        .filter_map(|it| match &it.region.class {
            Class::Reserve(reserve) => Some((*reserve, it.region.addr, it.region.capacity)),
            _ => None,
        })
        .collect()
}

#[test]
fn reservations_follow_everything_page_aligned_in_order_and_stay_where_they_are() {
    let (start, g) = program(Target::Function);
    let (hook, g2) = (hook(), function(3, &returns(2), Vec::new()));
    let sizes = DevRun { arena: 3 * PAGE + 1, cells: 0, fixed: 5 * PAGE };
    let mut image = Image::new(name(TO), dev(output(true), sizes)).unwrap();
    let page = image.plan.page;
    let mut disk = Disk::default();
    disk.link(&mut image, hooked(&start, &g, &hook)).unwrap();

    let before = reservations(&image);
    let sized: Vec<(Reserve, u64)> = before.iter().map(|it| (it.0, it.2)).collect();
    let want = [(Reserve::Arena, (3 * PAGE + 1).next_multiple_of(page)), (Reserve::Fixed, (5 * PAGE).next_multiple_of(page))];
    assert_eq!(sized, want, "rounded up to the page, and no region for the cells' 0");
    let [(_, arena, arena_len), (_, fixed, fixed_len)] = before[..] else { panic!("{before:?}") };
    assert!(arena.is_multiple_of(page) && fixed.is_multiple_of(page), "{arena:#x} {fixed:#x}");
    assert_eq!(arena + arena_len, fixed, "the fixed maps right after the arena");
    let others = image.state.spaces.iter().filter(|it| !matches!(it.region.class, Class::Reserve(_)));
    let end = others.map(|it| it.region.addr + it.region.capacity.max(it.region.size)).max().unwrap();
    assert!(end <= arena, "every other region and its room ({end:#x}) before the reservations ({arena:#x})");
    let reserved = image.state.spaces.iter().filter(|it| matches!(it.region.class, Class::Reserve(_)));
    for space in reserved {
        assert!(space.zero && space.offset.is_none() && space.region.size == 0, "{:?}", space.region);
    }

    let linked = Linked::of(disk.file(TO));
    let all = loads(&linked);
    let (rest, tail) = all.split_at(all.len() - 2);
    assert_eq!(tail, [(arena, arena_len, 0, 0), (fixed, fixed_len, 0, 0)], "no access, no file bytes, last");
    assert!(rest.iter().all(|it| it.0 + it.1 <= arena), "{all:x?}");

    disk.link(&mut image, edit(&g2)).unwrap();
    assert_eq!(reservations(&image), before, "an incremental stage keeps them where they were");
    let after = loads(&Linked::of(disk.file(TO)));
    assert_eq!(after[after.len() - 2..], *tail);
}

/// Whether `memory`'s image part holds `file`'s bytes, and zeros past its end.
fn holds(memory: &CodeMemory, file: &Sparse) -> bool {
    let bytes = memory.bytes(CodeRange { offset: 0, len: memory.image_len() }).unwrap();
    bytes.chunks(PAGE as usize).enumerate().all(|(page, it)| *it == file.page(page as u64)[..it.len()])
}

#[test]
fn a_commit_into_a_code_memory_holds_the_file_and_a_rollback_restores_it() {
    let (start, g) = program(Target::Function);
    let (hook, g2) = (hook(), function(3, &returns(2), Vec::new()));
    let out = dev(output(true), DevRun { arena: PAGE, cells: 0, fixed: 0 });
    let mut on_disk = Image::new(name(TO), out.clone()).unwrap();
    let mut image = Image::new(name(TO), out).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut on_disk, hooked(&start, &g, &hook)).unwrap();

    let mut staged = image.stage(hooked(&start, &g, &hook)).unwrap();
    let len = staged.writes.len;
    let Some(mut memory) = code_memory("blaze-link-dev-run", 2 * len) else { return };
    let mut small = CodeMemory::new("blaze-link-dev-run-small", 1, 0).unwrap();
    if len > small.image_len() {
        let error = image.commit_into(staged, &mut small).err().expect("a file past the image part");
        assert!(error.what.contains("image part"), "{error}");
        assert!(small.bytes(CodeRange { offset: 0, len: small.image_len() }).unwrap().iter().all(|it| *it == 0));
        staged = image.stage(hooked(&start, &g, &hook)).unwrap();
    }
    assert_eq!(image.commit_into(staged, &mut memory), Ok(None), "no program runs the first link");
    assert!(holds(&memory, disk.file(TO)), "the first link's file");
    assert_eq!(image.generation(), on_disk.generation());

    disk.link(&mut on_disk, edit(&g2)).unwrap();
    let staged = image.stage(edit(&g2)).unwrap();
    let message =
        blaze_alloc_count::assert_at_most(0, "a commit into a code memory", || image.commit_into(staged, &mut memory));
    assert!(holds(&memory, disk.file(TO)), "the patch's bytes, as the file commit wrote them");
    assert_eq!(message, Ok(None), "an image that is not live tells no program");
    assert_eq!(image.generation(), on_disk.generation());

    disk.roll_back(&mut on_disk).unwrap();
    let message = image.roll_back_into(&mut memory);
    assert!(holds(&memory, disk.file(TO)), "the file before the patch");
    assert_eq!(message, Ok(None));
    assert_eq!(image.generation(), on_disk.generation());
    assert!(image.roll_back_into(&mut memory).is_err(), "nothing before the first commit");
}

/// A live dev run's patches reach its programs only through the code memory: its file commit
/// and file rollback are refused, and the message a commit into the memory gives names code
/// the memory holds, by hash.
#[test]
fn a_live_dev_runs_message_names_the_code_its_memory_holds() {
    let (start, g) = program(Target::Function);
    let (hook, g2) = (hook(), function(3, &returns(2), Vec::new()));
    let out = dev(live(), DevRun { arena: PAGE, cells: 0, fixed: 0 });
    let mut on_disk = Image::new(name(TO), out.clone()).unwrap();
    let mut image = Image::new(name(TO), out).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut on_disk, hooked(&start, &g, &hook)).unwrap();

    let staged = image.stage(hooked(&start, &g, &hook)).unwrap();
    let Some(mut memory) = code_memory("blaze-link-live-dev-run", 2 * staged.writes.len) else { return };
    assert_eq!(image.commit_into(staged, &mut memory), Ok(None), "no program runs the first link");
    assert!(holds(&memory, disk.file(TO)), "the first link's file, its descriptor in it");

    let error = disk.link(&mut on_disk, edit(&g2)).unwrap_err();
    assert!(error.what.contains("only through the code memory") && error.what.contains("`commit_into`"), "{error}");
    assert_eq!(on_disk.generation(), 1, "a refused commit leaves the image as it was");
    let staged = image.stage(edit(&g2)).unwrap();
    let message =
        blaze_alloc_count::assert_at_most(0, "a commit into a code memory", || image.commit_into(staged, &mut memory));
    let message = message.unwrap().expect("a message for the programs running generation 1");
    let patch = patch_in(&message, &memory);
    assert_eq!((patch.base, patch.generation), (1, 2));
    assert!(patch.code.iter().any(|it| (it.len, it.hash) == (6, hash(&returns(2)))), "g2's code: {:?}", patch.code);

    let mut effects = Vec::new();
    let error = image.roll_back(&mut effects).unwrap_err();
    assert!(error.what.contains("`roll_back_into`"), "{error}");
    assert!(effects.is_empty() && image.generation() == 2, "a refused rollback leaves the image as it was");
    let message = image.roll_back_into(&mut memory).unwrap().expect("a live image's rollback tells its programs");
    assert_eq!(Message::decode(&message), Ok(Message::RollBack { id: patch.id, to: 1 }));
    assert!(holds(&memory, disk.file(TO)), "the first link's file again");
    assert_eq!(image.generation(), 1);
}
