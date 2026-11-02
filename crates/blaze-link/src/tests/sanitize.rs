//! The link of a sanitized image: what LLVM puts in each module beside the functions and the
//! link makes here (MemorySanitizer's options, AddressSanitizer's redzones, descriptors and
//! constructors, SanitizerCoverage's init calls), and the references a runtime's code makes.

use std::sync::Arc;

use blaze_codegen::settings::{Coverage, Sanitize, Sanitizers};
use blaze_codegen::{Arch, Covered, Init, RelocKind, Relocs, Static, Target};
use blaze_items::ItemKey;
use object::read::elf::{Rela as _, SectionHeader as _};
use object::write::{Object as Builder, StandardSection, Symbol as Def, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, Object as _, ObjectSection as _, SectionFlags, SectionKind, SymbolFlags,
    SymbolKind, SymbolScope, elf,
};

use super::{CALLER, Change, Disk, Image, Linked, Native, NativeKind, TO, covered, function, id, key, name, output, rel32, returns, section_of};
use crate::model::Apply;
use crate::sanitize::{
    self, BEG, GLOBAL, HEADER, MAX_REDZONE, MIN_REDZONE, MODULE_NAME, NAME, ODR_INDICATOR, SIZE, SIZE_WITH_REDZONE,
    instruments_section, redzone,
};
use crate::{Format, Output};

fn sanitized(sanitize: Sanitize, incremental: bool) -> Output {
    Output { sanitize, ..output(incremental) }
}

fn asan() -> Sanitize {
    Sanitize { sanitizers: Sanitizers::ADDRESS, ..Sanitize::default() }
}

/// An x86-64 ELF object defining each of `defines` as a function returning its index + 1.
fn runtime(defines: &[&str]) -> Arc<[u8]> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = b.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    for (n, it) in defines.iter().enumerate() {
        let at = b.append_section_data(text, &returns(n as u8 + 1), 16);
        b.add_symbol(Def {
            name: it.as_bytes().to_vec(),
            value: at,
            size: 6,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
    }
    b.write().unwrap().into()
}

fn native<'a>(bytes: &'a Arc<[u8]>, kind: NativeKind) -> Native<'a> {
    Native { id: id(80), name: "librustc-nightly_rt.asan.a", bytes, kind, order: 0 }
}

/// Which of [`runtime`]'s functions is at `addr`.
fn runtime_fn(linked: &Linked, addr: u64) -> u8 {
    let code = linked.at(addr, 6);
    assert_eq!((code[0], &code[2..]), (0xb8, &[0, 0, 0, 0xc3][..]), "a runtime function at {addr:#x}");
    code[1]
}

fn global(n: u8, size: usize) -> Static {
    Static {
        item: ItemKey(id(n)),
        instance: id(n + 100),
        align: 8,
        init: Init::Bytes { bytes: vec![n; size], relocs: Relocs::new() },
        data: Vec::new(),
        mutable: true,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

fn of(n: u8) -> Target {
    Target::Static { item: ItemKey(id(n)), instance: id(n + 100) }
}

/// `lea rax, [rip + a]; lea rcx, [rip + b]; ret`.
const TWO_LEAS: [u8; 15] = [0x48, 0x8d, 0x05, 0, 0, 0, 0, 0x48, 0x8d, 0x0d, 0, 0, 0, 0, 0xc3];
/// `lea rax, [rip + a]; ret`.
const LEA: [u8; 8] = [0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3];

/// The NUL-terminated string at `addr`.
fn c_str(linked: &Linked, addr: u64) -> &[u8] {
    let rest = &linked.bytes()[addr as usize..];
    &rest[..rest.iter().position(|it| *it == 0).expect("a NUL")]
}

/// The one pointer of a one-entry `.init_array` or `.fini_array`.
fn only_pointer(linked: &Linked, section: &str) -> u64 {
    let (at, len) = section_of(linked, section);
    assert_eq!(len, 8, "one pointer in {section}");
    linked.u64_at(at)
}

/// An `__asan_global` of a descriptor array: `beg`, `size`, `size_with_redzone`.
fn entry(linked: &Linked, header: u64, n: u64) -> (u64, u64, u64) {
    let at = header + HEADER + GLOBAL * n;
    (linked.u64_at(at + BEG), linked.u64_at(at + SIZE), linked.u64_at(at + SIZE_WITH_REDZONE))
}

#[test]
fn redzones_follow_llvms_formula() {
    for (size, zone) in [(1, 31), (8, 24), (16, 16), (17, 47), (32, 32), (100, 60), (1000, 248), (1 << 30, MAX_REDZONE)] {
        assert_eq!(redzone(size), zone, "the redzone after {size} bytes");
    }
    for size in 1..4096 {
        let zone = redzone(size);
        assert_eq!((size + zone) % MIN_REDZONE, 0, "{size} bytes and their redzone end on a granule");
        assert!((MIN_REDZONE / 2..=MAX_REDZONE + MIN_REDZONE).contains(&zone), "{size}: {zone}");
    }
}

#[test]
fn asan_skips_the_sections_llvm_skips() {
    for (format, section, instrumented) in [
        (Format::Elf, ".data.my_global", true),
        (Format::Elf, "linkme_ITEMS", false),
        (Format::Elf, ".init_array.00100", false),
        (Format::Elf, "__llvm_prf_cnts", false),
        (Format::Elf, "llvm.metadata", false),
        (Format::Pe, ".data", true),
        (Format::Pe, ".CRT$XCU", false),
        (Format::MachO, "__DATA,__data", true),
        (Format::MachO, "__DATA,__objc_classlist", false),
        (Format::MachO, "__OBJC,__image_info", false),
        (Format::MachO, "__DATA,__cfstring", false),
        (Format::MachO, "__TEXT,__cstring,cstring_literals", false),
    ] {
        assert_eq!(instruments_section(format, section), instrumented, "{format:?} {section}");
    }
    let tls = Static { thread_local: true, ..global(1, 8) };
    assert_eq!(sanitize::instrumented_static(Format::Elf, &tls), None, "a thread-local");
    let aligned = Static { align: 64, ..global(1, 8) };
    assert_eq!(sanitize::instrumented_static(Format::Elf, &aligned), None, "aligned past a granule");
    let empty = Static { init: Init::Zeroed { size: 0 }, ..global(1, 8) };
    assert_eq!(sanitize::instrumented_static(Format::Elf, &empty), None, "empty");
    assert_eq!(sanitize::instrumented_static(Format::Elf, &global(1, 8)), Some(8));
}

#[test]
fn every_constructor_field_is_inside_its_code_on_both_architectures() {
    let width = |apply: Apply| match apply {
        Apply::Rel32 | Apply::X86Branch32 => 4,
        Apply::A64Page21 | Apply::A64Lo12 { .. } | Apply::A64Jump26 | Apply::A64Call26 => 4,
        other => panic!("a constructor field of {other:?}"),
    };
    let check = |what: &str, code: &[u8], fields: &[sanitize::Field], base: u64, arch: Arch| {
        for field in fields {
            let at = (base + field.at) as usize;
            assert!(at + width(field.apply) <= code.len(), "{what}: {field:?} past its code");
            match arch {
                Arch::X86_64 => assert_eq!(code[at..at + 4], [0; 4], "{what}: {field:?} is an empty displacement"),
                _ => assert_eq!(at % 4, 0, "{what}: {field:?} on an instruction"),
            }
        }
    };
    for arch in [Arch::X86_64, Arch::Aarch64] {
        let register = sanitize::register(arch).unwrap();
        check("register", register.code, register.fields, 0, arch);
        assert_eq!(register.fields.iter().map(|it| it.target).max(), Some(1), "the array and the callee");
        let at_exit = sanitize::at_exit(arch).unwrap();
        check("at exit", at_exit.code, at_exit.fields, 0, arch);
        assert_eq!(at_exit.fields.iter().map(|it| it.target).max(), Some(4));
        for format in [Format::Elf, Format::MachO, Format::Pe] {
            if format == Format::Pe && arch == Arch::Aarch64 {
                continue;
            }
            let calls = sanitize::calls(format, arch).unwrap();
            check("a coverage call", calls.call, calls.fields, 0, arch);
            assert_eq!(calls.fields.iter().map(|it| it.target).max(), Some(2), "start, end, init");
        }
    }
    assert!(sanitize::register(Arch::Wasm32).is_err());
    assert!(sanitize::calls(Format::Wasm, Arch::Wasm32).is_err());
}

#[test]
fn asan_registers_each_global_with_its_redzone_and_descriptor() {
    let start = function(1, &LEA, vec![rel32(3, of(10))]);
    let item = global(10, 8);
    let defines = ["__asan_register_globals", "__asan_unregister_globals"];
    let rt: Arc<[u8]> = archive(&[("asan.o", &runtime(&defines), &defines)]).into();
    let mut image = Image::new(name(TO), sanitized(asan(), false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start],
        statics: vec![&item],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        natives: vec![native(&rt, NativeKind::Archive { whole: true })],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let at = linked.rel32(linked.symbol("start") + 3);
    assert_eq!(at % MIN_REDZONE, 0, "an instrumented global is aligned to a granule");
    assert_eq!(linked.at(at, 32), [vec![10; 8], vec![0; 24]].concat(), "its bytes, then its redzone");

    let ctor = only_pointer(&linked, ".init_array");
    let register = sanitize::register(Arch::X86_64).unwrap();
    assert_eq!(linked.at(ctor, 3), &register.code[..3], "`lea rdi`");
    let header = linked.rel32(ctor + 3) - HEADER;
    assert_eq!(linked.rel32(ctor + 10), header, "`mov rsi, [count]`");
    assert_eq!(runtime_fn(&linked, linked.rel32(ctor + 15)), 1, "`jmp __asan_register_globals`");
    let dtor = only_pointer(&linked, ".fini_array");
    assert_eq!(linked.rel32(dtor + 3) - HEADER, header);
    assert_eq!(runtime_fn(&linked, linked.rel32(dtor + 15)), 2, "`jmp __asan_unregister_globals`");

    assert_eq!(linked.u64_at(header), 1, "one global");
    assert_eq!(entry(&linked, header, 0), (at, 8, 32));
    let fields = header + HEADER;
    assert_eq!(c_str(&linked, linked.u64_at(fields + NAME)), b"static 0a0a0a0a0a0a0a0a");
    assert_eq!(c_str(&linked, linked.u64_at(fields + MODULE_NAME)), b"app");
    assert_eq!(linked.at(fields + 40, 16), [0; 16], "no dynamic init, no source location");
    let odr = linked.u64_at(fields + ODR_INDICATOR);
    assert_ne!(odr, 0);
    assert_eq!(linked.at(odr, 1), [0], "an ODR indicator byte");

    let mut image = Image::new(name(TO), sanitized(asan(), false)).unwrap();
    let change = Change {
        added: vec![&start],
        statics: vec![&item],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        ..Change::default()
    };
    let error = image.stage(change).unwrap_err();
    assert!(error.what.contains("__asan_"), "the constructors' callees are the runtime's: {error}");
}

#[test]
fn a_relink_that_adds_a_static_adds_its_descriptor() {
    let (start, g) = (function(1, &CALLER, vec![rel32(1, Target::Function(key(2)))]), function(2, &LEA, vec![rel32(3, of(10))]));
    let (one, two) = (global(10, 8), global(11, 40));
    let rt = runtime(&["__asan_register_globals", "__asan_unregister_globals"]);
    let mut image = Image::new(name(TO), sanitized(asan(), true)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start, &g],
        statics: vec![&one],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into()), (key(2), "g".into())],
        natives: vec![native(&rt, NativeKind::Object)],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let a = Linked::of(disk.file(TO));
    let first = a.rel32(a.symbol("g") + 3);
    let header = a.rel32(only_pointer(&a, ".init_array") + 3) - HEADER;
    assert_eq!(a.u64_at(header), 1);
    assert_eq!(entry(&a, header, 0), (first, 8, 32));

    let g2 = function(3, &TWO_LEAS, vec![rel32(3, of(10)), rel32(10, of(11))]);
    let change = Change {
        added: vec![&g2],
        removed: vec![key(2)],
        slots: vec![(key(2), key(3))],
        statics: vec![&two],
        debug_names: vec![(key(3), "g2".into())],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let b = Linked::of(disk.file(TO));
    let g2_at = b.symbol("g2");
    assert_eq!(b.rel32(g2_at + 3), first, "the first static stays");
    let second = b.rel32(g2_at + 10);
    assert_eq!(second % MIN_REDZONE, 0);
    let ctor = only_pointer(&b, ".init_array");
    let header = b.rel32(ctor + 3) - HEADER;
    assert_eq!(b.rel32(ctor + 10), header, "the constructor reads the new array's count");
    assert_eq!(b.u64_at(header), 2, "both statics");
    let mut entries = [entry(&b, header, 0), entry(&b, header, 1)];
    entries.sort();
    let mut want = [(first, 8, 32), (second, 40, 40 + redzone(40))];
    want.sort();
    assert_eq!(entries, want);
    let dtor = only_pointer(&b, ".fini_array");
    assert_eq!(b.rel32(dtor + 3) - HEADER, header, "the destructor unregisters the new array");
}

#[test]
fn msan_options_are_read_only_i32s_only_when_set() {
    let start = function(
        1,
        &TWO_LEAS,
        vec![rel32(3, Target::Extern("__msan_track_origins".into())), rel32(10, Target::Extern("__msan_keep_going".into()))],
    );
    let msan = Sanitize { sanitizers: Sanitizers::MEMORY, recover: Sanitizers::MEMORY, track_origins: 2, ..Sanitize::default() };
    let mut image = Image::new(name(TO), sanitized(msan, false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let s = linked.symbol("start");
    let (origins, keep_going) = (linked.rel32(s + 3), linked.rel32(s + 10));
    assert_eq!((origins % 4, keep_going % 4), (0, 0));
    assert_eq!((linked.u32_at(origins), linked.u32_at(keep_going)), (2, 1));
    let file = linked.elf();
    for addr in [origins, keep_going] {
        let section = file
            .sections()
            .find(|it| it.address() <= addr && addr + 4 <= it.address() + it.size())
            .unwrap_or_else(|| panic!("{addr:#x} in no section"));
        let SectionFlags::Elf { sh_flags, .. } = section.flags() else { panic!("ELF flags") };
        assert_eq!(sh_flags & elf::SHF_WRITE, elf::SectionFlags(0), "{addr:#x} is read-only, in {:?}", section.name());
    }

    let plain = Sanitize { sanitizers: Sanitizers::MEMORY, ..Sanitize::default() };
    let mut image = Image::new(name(TO), sanitized(plain, false)).unwrap();
    let change = Change { added: vec![&start], names: vec![("_start".into(), Target::Direct(key(1)))], ..Change::default() };
    let error = image.stage(change).unwrap_err();
    assert!(error.what.contains("__msan_track_origins"), "no option without its setting: {error}");
}

#[test]
fn coverage_init_calls_only_the_sections_there_are_with_their_bounds() {
    let start = covered(1, 0, 0, Covered { counters: 2, flags: 0, guards: 0, pcs: vec![(0, 1)] });
    let rt = runtime(&["__sanitizer_cov_8bit_counters_init", "__sanitizer_cov_pcs_init"]);
    let coverage = Coverage { level: 1, flags: Coverage::INLINE_8BIT_COUNTERS | Coverage::PC_TABLE };
    let mut image = Image::new(name(TO), sanitized(Sanitize { coverage, ..Sanitize::default() }, false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        natives: vec![native(&rt, NativeKind::Object)],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let ctor = only_pointer(&linked, ".init_array");
    let (counters, counters_len) = section_of(&linked, "__sancov_cntrs");
    let (pcs, pcs_len) = section_of(&linked, "__sancov_pcs");
    assert_eq!((counters_len, pcs_len), (2, 16));
    assert_eq!(linked.at(ctor, 1), [0x50], "`push rax`");
    for (n, (start, len, init)) in [(counters, 2, 1), (pcs, 16, 2)].into_iter().enumerate() {
        let call = ctor + 1 + 19 * n as u64;
        assert_eq!(linked.rel32(call + 3), start, "call {n}: `lea rdi, [start]`");
        assert_eq!(linked.rel32(call + 10), start + len, "call {n}: `lea rsi, [end]`");
        assert_eq!(runtime_fn(&linked, linked.rel32(call + 15)), init, "call {n}'s init function");
    }
    assert_eq!(linked.at(ctor + 1 + 19 * 2, 2), [0x58, 0xc3], "two calls, then `pop rax; ret`");

    let mut image = Image::new(name(TO), sanitized(Sanitize { coverage, ..Sanitize::default() }, false)).unwrap();
    let plain = function(1, &returns(0), Vec::new());
    let change = Change {
        added: vec![&plain],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        natives: vec![native(&rt, NativeKind::Object)],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    assert!(linked.elf().section_by_name(".init_array").is_none(), "no sections, no constructor");
}

#[test]
fn a_runtimes_got_and_general_dynamic_tls_references_link() {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let data = b.section_id(StandardSection::Data);
    let at = b.append_section_data(data, &[7, 0, 0, 0], 4);
    let option = "__asan_option_detect_stack_use_after_return";
    let defs = [(option, data, at, 4, SymbolKind::Data)];
    let tls = b.section_id(StandardSection::Tls);
    let tls_at = b.append_section_data(tls, &[9; 8], 8);
    let param = "__msan_param_tls";
    for (name, section, value, size, kind) in defs.into_iter().chain([(param, tls, tls_at, 8, SymbolKind::Tls)]) {
        b.add_symbol(Def {
            name: name.as_bytes().to_vec(),
            value,
            size,
            kind,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        });
    }
    let member = b.write().unwrap();
    let rt: Arc<[u8]> = archive(&[("rt.o", &member, &[option, param])]).into();

    // `mov rax, [rip + option@GOTPCREL]; lea rdi, [rip + param@TLSGD]; ret`
    let code = [0x48, 0x8b, 0x05, 0, 0, 0, 0, 0x48, 0x8d, 0x3d, 0, 0, 0, 0, 0xc3];
    let reloc = |at, kind, name: &str| blaze_codegen::Relocation { at, kind, target: Target::Extern(name.into()), addend: -4 };
    let start = function(1, &code, vec![reloc(3, RelocKind::GotPcRel32, option), reloc(10, RelocKind::TlsGd32, param)]);
    let mut image = Image::new(name(TO), sanitized(Sanitize::default(), false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        natives: vec![native(&rt, NativeKind::Archive { whole: true })],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let s = linked.symbol("start");
    let cell = linked.rel32(s + 3);
    assert_eq!(linked.at(linked.u64_at(cell), 4), [7, 0, 0, 0], "the GOT cell holds the runtime's option");
    let pair = linked.rel32(s + 10);
    let file = linked.elf();
    let endian = file.endian();
    let (_, rela) = file.elf_section_table().section_by_name(endian, b".rela.dyn").expect(".rela.dyn");
    let (relas, _) = rela.rela(endian, linked.bytes()).unwrap().expect("relocations");
    let at = |offset: u64, r_type: elf::RelocationType| relas.iter().any(|it| it.r_offset(endian) == offset && it.r_type(endian, false) == r_type);
    assert!(at(pair, elf::R_X86_64_DTPMOD64), "the pair's module");
    assert!(at(pair + 8, elf::R_X86_64_DTPOFF64), "the pair's offset");
}

/// A GNU-format archive of `members` (name, bytes, the symbols it defines), with its symbol
/// index.
fn archive(members: &[(&str, &[u8], &[&str])]) -> Vec<u8> {
    let header = |name: &str, size: usize| format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644);
    let names: Vec<&str> = members.iter().flat_map(|it| it.2.iter().copied()).collect();
    let index_len = 4 + 4 * names.len() + names.iter().map(|it| it.len() + 1).sum::<usize>();
    let mut offsets = Vec::new();
    let mut at = 8 + 60 + index_len.next_multiple_of(2);
    for (_, bytes, defines) in members {
        offsets.extend(defines.iter().map(|_| at as u32));
        at += 60 + bytes.len().next_multiple_of(2);
    }
    let mut out = b"!<arch>\n".to_vec();
    out.extend_from_slice(header("/", index_len).as_bytes());
    out.extend_from_slice(&(names.len() as u32).to_be_bytes());
    for offset in offsets {
        out.extend_from_slice(&offset.to_be_bytes());
    }
    for it in &names {
        out.extend_from_slice(it.as_bytes());
        out.push(0);
    }
    if out.len() % 2 != 0 {
        out.push(b'\n');
    }
    for (member, bytes, _) in members {
        out.extend_from_slice(header(&format!("{member}/"), bytes.len()).as_bytes());
        out.extend_from_slice(bytes);
        if out.len() % 2 != 0 {
            out.push(b'\n');
        }
    }
    out
}
