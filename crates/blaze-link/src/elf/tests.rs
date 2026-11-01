use object::read::elf::{Dyn as _, ElfFile64, FileHeader as _, ProgramHeader as _, Rela as _, SectionHeader as _, Sym as _};
use object::{Endianness, SymbolIndex, elf};

use super::dynamic::{elf_hash, gnu_hash};
use super::image::section_name;
use super::{Elf, Segment, in_file, section, writer};
use parking_lot::Mutex;

use crate::cow::CowBytes;
use crate::model::{
    Bind, Class, Counts, Delta, Export, Function, GotEntry, Holds, Import, Library, Pages, Placed, Plan, Region, Reserve,
    Special, Splice, SymbolKind, Tls, Writer,
};
use crate::output::Debug;
use crate::{DevRun, Format, Output, OutputKind};

fn output(arch: blaze_codegen::Arch, kind: OutputKind, interpreter: Option<&str>) -> Output {
    Output {
        format: Format::Elf,
        arch,
        kind,
        interpreter: interpreter.map(str::to_owned),
        platform: None,
        incremental: false,
        debug: Debug::None,
        live: None,
        tags: 0,
        stack: None,
        windows_subsystem: false,
        dev_run: None,
        exec_log: false,
        sanitize: blaze_codegen::settings::Sanitize::default(),
        rpaths: Vec::new(),
        triple: match arch { blaze_codegen::Arch::Aarch64 => "aarch64-unknown-linux-gnu", _ => "x86_64-unknown-linux-gnu" }.into(),
    }
}

fn executable() -> Output {
    output(
        blaze_codegen::Arch::X86_64,
        OutputKind::Executable { entry: "_start".into() },
        Some("/lib64/ld-linux-x86-64.so.2"),
    )
}

/// An image placed as the core places one: classes in `Writer::order`, a page boundary at each
/// change of protection, each region aligned.
struct Fixture {
    writer: Elf,
    plan: Plan,
    regions: Vec<Region>,
    contents: Vec<Vec<u8>>,
    libraries: Vec<Library>,
    imports: Vec<Import>,
    got: Vec<GotEntry>,
    rebase: Vec<u64>,
    binds: Vec<Bind>,
    exports: Vec<Export>,
    functions: Vec<Function>,
    entry: Option<u64>,
    tls: Option<Tls>,
    debug: Vec<(&'static str, Vec<u8>)>,
}

impl Fixture {
    /// An incremental `output` gets a writer that keeps its tables.
    fn new(output: Output, pieces: &[(Class, u64, u64)], counts: &Counts) -> Fixture {
        let writer = Elf { last: output.incremental.then(Mutex::default), output };
        let plan = writer.plan();
        let mut wanted = pieces.to_vec();
        for (class, size) in writer.synthetic(counts) {
            wanted.push((class, size, 8));
        }
        let mut classes: Vec<Class> = wanted.iter().map(|it| it.0.clone()).collect();
        writer.order(&mut classes);
        let mut regions = Vec::new();
        let mut addr = plan.base + plan.headers;
        let mut last = None;
        for class in classes {
            let (_, size, align) = wanted.iter().find(|it| it.0 == class).cloned().unwrap();
            let prot = writer.prot(&class);
            if last.is_some_and(|it| it != prot) {
                addr = addr.next_multiple_of(plan.page);
            }
            last = Some(prot);
            addr = addr.next_multiple_of(align);
            regions.push(Region { class, addr, capacity: size, size, prot, align });
            addr += size;
        }
        let contents =
            regions.iter().map(|it| if in_file(&it.class) { vec![0; it.size as usize] } else { Vec::new() }).collect();
        Fixture {
            writer,
            plan,
            regions,
            contents,
            libraries: Vec::new(),
            imports: Vec::new(),
            got: Vec::new(),
            rebase: Vec::new(),
            binds: Vec::new(),
            exports: Vec::new(),
            functions: Vec::new(),
            entry: None,
            tls: None,
            debug: Vec::new(),
        }
    }

    fn index(&self, class: &Class) -> usize {
        self.regions.iter().position(|it| &it.class == class).unwrap_or_else(|| panic!("no {class:?}"))
    }

    fn at(&self, class: &Class) -> u64 {
        self.regions[self.index(class)].addr
    }

    fn put(&mut self, class: &Class, offset: usize, bytes: &[u8]) {
        let index = self.index(class);
        self.contents[index][offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    fn with<R>(&self, then: impl FnOnce(&Placed<'_>) -> R) -> R {
        self.with_in(&self.contents, 0, None, then)
    }

    /// The `Placed` of the image with `contents` for its regions' bytes, of synthesis
    /// `synthesis` with `delta`.
    fn with_in<R>(
        &self,
        contents: &[Vec<u8>],
        synthesis: u64,
        delta: Option<&Delta<'_>>,
        then: impl FnOnce(&Placed<'_>) -> R,
    ) -> R {
        self.with_pages(contents, &[], synthesis, delta, then)
    }

    /// [`Fixture::with_in`], the regions `paged` holds in pages (and none in `contents`).
    fn with_pages<R>(
        &self,
        contents: &[Vec<u8>],
        paged: &[Option<Pages<'_>>],
        synthesis: u64,
        delta: Option<&Delta<'_>>,
        then: impl FnOnce(&Placed<'_>) -> R,
    ) -> R {
        let contents: Vec<&[u8]> = (contents.iter().enumerate())
            .map(|(at, it)| if paged.get(at).is_some_and(Option::is_some) { &[][..] } else { it.as_slice() })
            .collect();
        then(&Placed {
            output: &self.writer.output,
            name: "image",
            plan: &self.plan,
            regions: &self.regions,
            contents: &contents,
            paged,
            libraries: &self.libraries,
            imports: &self.imports,
            got: &self.got,
            rebase: &self.rebase,
            binds: &self.binds,
            exports: &self.exports,
            functions: &self.functions,
            entry: self.entry,
            tls: self.tls,
            id: *b"blaze build id!!",
            debug: &self.debug,
            init: None,
            fini: None,
            symbol: &|_| None,
            synthesis,
            delta,
            encoded: &[],
        })
    }

    /// Synthesizes with `writer`, then makes the file's parts with the synthetic regions'
    /// bytes where the core puts them, both of synthesis `synthesis` with `delta`.
    fn build(&self, writer: &Elf, synthesis: u64, delta: Option<&Delta<'_>>) -> (Vec<(Class, Vec<u8>)>, Vec<(u64, Vec<u8>)>) {
        let (tables, parts) = self.shared(writer, synthesis, delta);
        (flat(tables), flat(parts))
    }

    /// [`Fixture::build`]'s tables and parts as the writer hands them over.
    fn shared(&self, writer: &Elf, synthesis: u64, delta: Option<&Delta<'_>>) -> (Vec<(Class, CowBytes)>, Vec<(u64, CowBytes)>) {
        self.shared_in(writer, synthesis, delta, &[])
    }

    /// [`Fixture::shared`] with the regions of `held` given to the writer in those pages, as
    /// the core gives a region it wrote since the full link.
    fn shared_in(
        &self,
        writer: &Elf,
        synthesis: u64,
        delta: Option<&Delta<'_>>,
        held: &[(Class, CowBytes)],
    ) -> (Vec<(Class, CowBytes)>, Vec<(u64, CowBytes)>) {
        let mut paged: Vec<Option<Pages<'_>>> = vec![None; self.regions.len()];
        for (class, bytes) in held {
            paged[self.index(class)] = Some(Pages::all(bytes));
        }
        let tables =
            self.with_pages(&self.contents, &paged, synthesis, delta, |placed| writer.synthesize(placed)).unwrap();
        let mut contents = self.contents.clone();
        for (class, bytes) in &tables {
            contents[self.index(class)] = bytes.contiguous().into_owned();
        }
        let file = self.with_pages(&contents, &paged, synthesis, delta, |placed| writer.file(placed)).unwrap();
        (tables, file.parts)
    }

    fn synthesize(&self) -> Result<Vec<(Class, Vec<u8>)>, crate::LinkError> {
        self.with(|placed| self.writer.synthesize(placed)).map(flat)
    }

    /// Synthesizes, then writes the file as the core does: regions at their offsets, then the
    /// writer's parts and the core's debug sections where the writer laid them.
    fn link(&mut self) -> Vec<u8> {
        for (class, bytes) in self.synthesize().unwrap() {
            let index = self.index(&class);
            assert!(bytes.len() as u64 <= self.regions[index].capacity, "{class:?} outgrew the size it asked for");
            self.contents[index] = bytes;
        }
        for (class, bytes) in self.synthesize().unwrap() {
            assert_eq!(self.contents[self.index(&class)], bytes, "{class:?} is not a function of the image");
        }
        let (file, offsets) = self.with(|placed| {
            let offsets: Vec<Option<u64>> = self.regions.iter().map(|it| self.writer.offset(placed, it)).collect();
            (self.writer.file(placed).unwrap(), offsets)
        });
        let again = self.with(|placed| self.writer.file(placed).unwrap());
        assert_eq!((again.parts, again.debug), (file.parts.clone(), file.debug.clone()));
        let parts = flat(file.parts);
        let mut bytes = vec![0; file.len as usize];
        for ((region, contents), offset) in self.regions.iter().zip(&self.contents).zip(offsets) {
            if let Some(offset) = offset {
                assert_eq!(offset, region.addr, "a region's file offset is its address");
                bytes[offset as usize..][..contents.len()].copy_from_slice(contents);
            }
        }
        for (offset, part) in &parts {
            bytes[*offset as usize..][..part.len()].copy_from_slice(part);
        }
        for &(offset, nth) in &file.debug {
            let data = &self.debug[nth].1;
            bytes[offset as usize..][..data.len()].copy_from_slice(data);
        }
        bytes
    }
}

/// Each of the writer's buffers as one vector, keyed as it was.
fn flat<K>(parts: Vec<(K, CowBytes)>) -> Vec<(K, Vec<u8>)> {
    parts.into_iter().map(|(key, bytes)| (key, bytes.contiguous().into_owned())).collect()
}

/// The debug sections are where the section headers say, with the bytes the core made, and
/// the writer copies none of them: the file allocates the same with 1 MiB of debug info as
/// with a few bytes.
#[test]
fn the_debug_sections_are_the_cores_not_copies() {
    let counts = Counts { functions: 1, function_names: 4, ..Counts::default() };
    let mut image = Fixture::new(executable(), &[(Class::Text, 64, 16)], &counts);
    let text = image.at(&Class::Text);
    image.functions = vec![Function { addr: text, size: 64, fde: None, windows: None, name: Some("main".into()) }];
    image.entry = Some(text);
    for (class, bytes) in image.synthesize().unwrap() {
        let index = image.index(&class);
        image.contents[index] = bytes;
    }
    let pool = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    let used = [3usize, 1 << 20].map(|len| {
        let line: Vec<u8> = (0..len).map(|it| (it * 7 % 251) as u8).collect();
        image.debug = vec![(".debug_line", line), (".debug_str", Vec::new()), (".debug_info", vec![9; len / 3 + 1])];
        let (file, used) = pool.install(|| blaze_alloc_count::measure(|| image.with(|placed| image.writer.file(placed))));
        let file = file.unwrap();
        assert_eq!(file.debug.iter().map(|it| it.1).collect::<Vec<_>>(), [0, 2], "an empty section is not laid");
        let bytes = image.link();
        let parsed = ElfFile64::<Endianness>::parse(&bytes[..]).unwrap();
        let endian = parsed.endian();
        let shdrs = parsed.elf_section_table();
        for (name, data) in &image.debug {
            let (_, shdr) = shdrs.section_by_name(endian, name.as_bytes()).unwrap_or_else(|| panic!("no {name}"));
            assert_eq!(shdr.data(endian, &bytes[..]).unwrap(), &data[..], "{name}");
        }
        used
    });
    assert_eq!(used[0].allocations, used[1].allocations, "{used:?}");
    assert!(used[1].bytes.abs_diff(used[0].bytes) < 4096, "the debug info is not copied: {used:?}");
}

fn c_string(data: &[u8], at: u64) -> &str {
    let rest = &data[at as usize..];
    std::str::from_utf8(&rest[..rest.iter().position(|&it| it == 0).unwrap()]).unwrap()
}

fn eh_frame(fdes: &[(u64, u32)]) -> Vec<u8> {
    use gimli::write::{Address, CommonInformationEntry, EhFrame, EndianVec, FrameDescriptionEntry, FrameTable};
    let mut table = FrameTable::default();
    let encoding = gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
    let cie = table.add_cie(CommonInformationEntry::new(encoding, 1, -8, gimli::Register(16)));
    for &(addr, len) in fdes {
        table.add_fde(cie, FrameDescriptionEntry::new(Address::Constant(addr), len));
    }
    let mut section = EhFrame(EndianVec::new(gimli::LittleEndian));
    table.write_eh_frame(&mut section).unwrap();
    section.0.into_vec()
}

fn import(name: &str, library: Option<u32>, version: Option<&str>, weak: bool) -> Import {
    Import {
        name: name.into(),
        library,
        version: version.map(str::to_owned),
        kind: SymbolKind::Function,
        weak,
        wasm: None,
    }
}

#[test]
fn an_executable_reads_back() {
    let fdes = eh_frame(&[(0, 32), (0, 32)]).len() as u64;
    let counts = Counts {
        imports: 4,
        import_names: 72,
        libraries: 3,
        library_names: 32,
        got: 3,
        absolute: 9,
        exports: 2,
        export_names: 16,
        functions: 2,
        function_names: 10,
        fdes: 2,
        ..Counts::default()
    };
    let pieces = [
        (Class::Text, 64, 16),
        (Class::Rodata, 16, 8),
        (Class::EhFrame, fdes, 8),
        (Class::Data, 16, 8),
        (Class::RelRo, 8, 8),
        (Class::Got, 32, 8),
        (Class::InitArray, 8, 8),
        (Class::Tdata, 8, 8),
        (Class::Tbss, 16, 8),
        (Class::Bss, 32, 8),
    ];
    let mut image = Fixture::new(executable(), &pieces, &counts);
    let text = image.at(&Class::Text);
    let rodata = image.at(&Class::Rodata);
    let data = image.at(&Class::Data);
    let got = image.at(&Class::Got);
    let init = image.at(&Class::InitArray);
    let tdata = image.at(&Class::Tdata);
    let bss = image.at(&Class::Bss);
    image.put(&Class::Text, 0, &[0xcc; 64]);
    image.put(&Class::EhFrame, 0, &eh_frame(&[(text + 32, 32), (text, 32)]));
    image.put(&Class::Data, 0, &rodata.to_le_bytes());
    image.put(&Class::Got, 8, &rodata.to_le_bytes());
    image.put(&Class::InitArray, 0, &text.to_le_bytes());
    let relro = image.at(&Class::RelRo);
    image.put(&Class::RelRo, 0, &(text + 32).to_le_bytes());
    image.put(&Class::Tdata, 0, &7u64.to_le_bytes());
    image.libraries = vec![
        Library { name: "libc.so.6".into(), as_needed: true },
        Library { name: "libm.so.6".into(), as_needed: true },
        Library { name: "libunused.so".into(), as_needed: true },
    ];
    image.imports = vec![
        import("malloc", Some(0), Some("GLIBC_2.2.5"), false),
        import("realpath", Some(0), Some("GLIBC_2.3"), false),
        import("__cxa_thread_atexit_impl", None, None, true),
        import("cos", Some(1), Some("GLIBC_2.2.5"), false),
    ];
    image.got = vec![
        GotEntry { addr: got, holds: Holds::Import(0) },
        GotEntry { addr: got + 8, holds: Holds::Local(rodata) },
        GotEntry { addr: got + 16, holds: Holds::TlsGd { import: None, offset: 4 } },
    ];
    image.rebase = vec![data, got + 8, init, relro];
    image.binds = vec![Bind { addr: data + 8, import: 0, addend: 8 }];
    image.exports = vec![
        Export { name: "main".into(), addr: text, size: 32, kind: SymbolKind::Function },
        Export { name: "__blaze_live".into(), addr: rodata, size: 16, kind: SymbolKind::Data },
    ];
    image.functions = vec![
        Function { addr: text, size: 32, fde: None, windows: None, name: Some("main".into()) },
        Function { addr: text + 32, size: 32, fde: None, windows: None, name: Some("_start".into()) },
    ];
    image.entry = Some(text + 32);
    image.tls = Some(Tls { addr: tdata, init: 8, size: 24, align: 8 });
    image.debug = vec![(".debug_line", vec![1, 2, 3])];

    let bytes = image.link();
    let data_bytes = &bytes[..];
    let file = ElfFile64::<Endianness>::parse(data_bytes).unwrap();
    let endian = file.endian();
    let header = file.elf_header();
    assert_eq!(header.e_type(endian), elf::ET_DYN);
    assert_eq!(header.e_machine(endian), elf::EM_X86_64);
    assert_eq!(header.e_entry(endian), text + 32);

    // Segments: four loads, R, RX, relro, RW, each on a page, covering every region.
    let phdrs = file.elf_program_headers();
    let loads: Vec<_> = phdrs.iter().filter(|it| it.p_type(endian) == elf::PT_LOAD).collect();
    assert_eq!(loads.iter().map(|it| it.p_flags(endian).0).collect::<Vec<_>>(), [4, 5, 6, 6]);
    for load in &loads {
        assert_eq!(load.p_offset(endian), load.p_vaddr(endian));
        assert_eq!(load.p_vaddr(endian) % 0x1000, 0);
        assert_eq!(load.p_align(endian), 0x1000);
    }
    assert_eq!(loads[0].p_vaddr(endian), 0);
    for region in &image.regions {
        let end = region.addr + region.size;
        let load = loads
            .iter()
            .find(|it| it.p_vaddr(endian) <= region.addr && end <= it.p_vaddr(endian) + it.p_memsz(endian))
            .unwrap_or_else(|| panic!("{:?} in no PT_LOAD", region.class));
        if in_file(&region.class) {
            assert!(end <= load.p_vaddr(endian) + load.p_filesz(endian), "{:?} past the file", region.class);
        }
        let flags = load.p_flags(endian).0;
        assert_eq!(flags, Segment::of(&region.class).flags(), "{:?}", region.class);
    }
    let phdr = |p_type| phdrs.iter().find(|it| it.p_type(endian) == p_type).unwrap();
    let interp = phdr(elf::PT_INTERP);
    assert_eq!(interp.data(endian, data_bytes).unwrap(), b"/lib64/ld-linux-x86-64.so.2\0");
    let relro_phdr = phdr(elf::PT_GNU_RELRO);
    for class in [Class::RelRo, Class::Got, Class::InitArray, Class::Synthetic(super::DYNAMIC)] {
        let addr = image.at(&class);
        assert!(relro_phdr.p_vaddr(endian) <= addr && addr < relro_phdr.p_vaddr(endian) + relro_phdr.p_memsz(endian));
    }
    assert!(relro_phdr.p_vaddr(endian) + relro_phdr.p_memsz(endian) <= data, "relro ends before the writable data");
    let tls = phdr(elf::PT_TLS);
    assert_eq!((tls.p_vaddr(endian), tls.p_filesz(endian), tls.p_memsz(endian)), (tdata, 8, 24));
    assert!(tdata < bss);
    let note = phdr(elf::PT_NOTE).data(endian, data_bytes).unwrap();
    assert_eq!(&note[12..16], b"GNU\0");
    assert_eq!(&note[16..], b"blaze build id!!");
    let hdr = image.at(&Class::Synthetic(super::EH_FRAME_HDR));
    assert_eq!(phdr(elf::PT_GNU_EH_FRAME).p_vaddr(endian), hdr);
    assert!(phdrs.iter().any(|it| it.p_type(endian) == elf::PT_GNU_STACK && it.p_flags(endian).0 == 6));

    // `.eh_frame_hdr`: both FDEs, sorted by the address they cover.
    let table = &bytes[hdr as usize..];
    assert_eq!(table[..4], [1, 0x1b, 0x03, 0x3b]);
    let word = |at: usize| i32::from_le_bytes(table[at..at + 4].try_into().unwrap()) as i64;
    assert_eq!(word(4) + hdr as i64 + 4, image.at(&Class::EhFrame) as i64);
    assert_eq!(word(8), 2);
    assert_eq!(word(12) + hdr as i64, text as i64);
    assert_eq!(word(20) + hdr as i64, text as i64 + 32);

    // `.dynamic`.
    let dynamic = phdr(elf::PT_DYNAMIC).dynamic(endian, data_bytes).unwrap().unwrap();
    let entries: Vec<(u64, u64)> = dynamic.iter().map(|it| (it.d_tag(endian).0 as u64, it.d_val(endian))).collect();
    let tag = |tag: elf::DynamicTag| entries.iter().find(|it| it.0 == tag.0 as u64).map(|it| it.1);
    let strtab = tag(elf::DT_STRTAB).unwrap();
    let needed: Vec<&str> = entries
        .iter()
        .filter(|it| it.0 == elf::DT_NEEDED.0 as u64)
        .map(|it| c_string(data_bytes, strtab + it.1))
        .collect();
    assert_eq!(needed, ["libc.so.6", "libm.so.6"], "an as-needed library nothing imports from is left out");
    assert_eq!(tag(elf::DT_FLAGS), Some(elf::DF_BIND_NOW.0));
    assert_eq!(tag(elf::DT_FLAGS_1), Some((elf::DF_1_NOW | elf::DF_1_PIE).0));
    assert_eq!(tag(elf::DT_DEBUG), Some(0));
    assert_eq!(tag(elf::DT_SONAME), None);
    assert_eq!(tag(elf::DT_INIT_ARRAY), Some(init));
    assert_eq!(tag(elf::DT_INIT_ARRAYSZ), Some(8));
    assert_eq!(tag(elf::DT_RELACOUNT), Some(4));
    assert_eq!(tag(elf::DT_VERNEEDNUM), Some(2));
    assert_eq!(tag(elf::DT_SYMTAB), Some(image.at(&Class::Synthetic(super::DYNSYM))));
    assert_eq!(tag(elf::DT_GNU_HASH), Some(image.at(&Class::Synthetic(super::GNU_HASH))));
    assert_eq!(entries.last(), Some(&(0, 0)));

    // `.rela.dyn`: the slid addresses first and sorted, then the binds by address.
    let sections = file.elf_section_table();
    let (_, rela) = sections.section_by_name(endian, b".rela.dyn").unwrap();
    let (relas, _) = rela.rela(endian, data_bytes).unwrap().unwrap();
    let relas: Vec<(u64, u32, u32, i64)> = relas
        .iter()
        .map(|it| (it.r_offset(endian), it.r_type(endian, false).0, it.r_sym(endian, false), it.r_addend(endian)))
        .collect();
    let mut relative = vec![
        (data, elf::R_X86_64_RELATIVE.0, 0, rodata as i64),
        (got + 8, elf::R_X86_64_RELATIVE.0, 0, rodata as i64),
        (init, elf::R_X86_64_RELATIVE.0, 0, text as i64),
        (relro, elf::R_X86_64_RELATIVE.0, 0, text as i64 + 32),
    ];
    relative.sort();
    let mut other = vec![
        (got, elf::R_X86_64_GLOB_DAT.0, 1, 0),
        (got + 16, elf::R_X86_64_DTPMOD64.0, 0, 0),
        (got + 24, elf::R_X86_64_DTPOFF64.0, 0, 4),
        (data + 8, elf::R_X86_64_64.0, 1, 8),
    ];
    other.sort();
    relative.extend(other);
    assert_eq!(relas, relative);

    // `.dynsym`, its versions and its hash table.
    let dynsyms = sections.symbols(endian, data_bytes, elf::SHT_DYNSYM).unwrap();
    let versions = sections.versions(endian, data_bytes).unwrap().unwrap();
    let version = |index: usize| {
        versions.version(versions.version_index(endian, SymbolIndex(index)).index()).unwrap().map(|it| it.name().to_vec())
    };
    let name = |index: usize| dynsyms.symbol(SymbolIndex(index)).unwrap().name(endian, dynsyms.strings()).unwrap();
    assert_eq!(name(1), b"malloc");
    assert_eq!(version(1).as_deref(), Some(&b"GLIBC_2.2.5"[..]));
    assert_eq!(name(2), b"realpath");
    assert_eq!(version(2).as_deref(), Some(&b"GLIBC_2.3"[..]));
    assert_eq!(name(3), b"__cxa_thread_atexit_impl");
    assert_eq!(dynsyms.symbol(SymbolIndex(3)).unwrap().st_bind(), elf::STB_WEAK);
    assert_eq!(version(3), None);
    assert_eq!(version(4).as_deref(), Some(&b"GLIBC_2.2.5"[..]));
    let (hash, _) = sections.gnu_hash(endian, data_bytes).unwrap().unwrap();
    for export in &image.exports {
        let name = export.name.as_bytes();
        let (_, symbol) = hash
            .find(endian, name, elf::gnu_hash(name), None, &dynsyms, &versions)
            .unwrap_or_else(|| panic!("`{}` is not in the hash table", export.name));
        assert_eq!(symbol.st_value(endian), export.addr);
        assert!(!symbol.is_undefined(endian));
    }
    assert!(hash.find(endian, b"malloc", elf::gnu_hash(b"malloc"), None, &dynsyms, &versions).is_none());

    // `.symtab` names the functions; every region has its section header.
    let symtab = sections.symbols(endian, data_bytes, elf::SHT_SYMTAB).unwrap();
    let named: Vec<(Vec<u8>, u64)> = symtab
        .symbols()
        .iter()
        .filter(|it| it.st_type() == elf::STT_FUNC)
        .map(|it| (it.name(endian, symtab.strings()).unwrap().to_vec(), it.st_value(endian)))
        .collect();
    assert!(named.contains(&(b"main".to_vec(), text)));
    assert!(named.contains(&(b"_start".to_vec(), text + 32)));
    for region in &image.regions {
        let (_, shdr) = sections
            .section_by_name(endian, section_name(&region.class).as_bytes())
            .unwrap_or_else(|| panic!("no section for {:?}", region.class));
        assert_eq!(shdr.sh_addr(endian), region.addr);
    }
    let (_, line) = sections.section_by_name(endian, b".debug_line").unwrap();
    let at = line.sh_offset(endian) as usize;
    assert_eq!(bytes[at..at + 3], [1, 2, 3]);
    let (_, verneed) = sections.section_by_name(endian, b".gnu.version_r").unwrap();
    assert_eq!(verneed.sh_info(endian), 2, "one Elf64_Verneed per library with versions");
}

#[test]
fn a_shared_library_exports_through_its_hash_table() {
    let names: Vec<String> = (0..100).map(|it| format!("blaze_export_{it}")).collect();
    let counts = Counts {
        exports: names.len() as u64,
        export_names: names.iter().map(|it| it.len() as u64).sum(),
        ..Counts::default()
    };
    let kind = OutputKind::Shared { name: "libblaze.so".into(), exports: names.clone() };
    let output = output(blaze_codegen::Arch::Aarch64, kind, None);
    let mut image = Fixture::new(output, &[(Class::Text, 400, 4), (Class::Data, 8, 8)], &counts);
    let text = image.at(&Class::Text);
    image.exports = names
        .iter()
        .enumerate()
        .map(|(at, name)| Export { name: name.clone(), addr: text + 4 * at as u64, size: 4, kind: SymbolKind::Function })
        .collect();

    let bytes = image.link();
    let data = &bytes[..];
    let file = ElfFile64::<Endianness>::parse(data).unwrap();
    let endian = file.endian();
    assert_eq!(file.elf_header().e_machine(endian), elf::EM_AARCH64);
    let phdrs = file.elf_program_headers();
    assert!(phdrs.iter().all(|it| it.p_type(endian) != elf::PT_INTERP));
    for load in phdrs.iter().filter(|it| it.p_type(endian) == elf::PT_LOAD) {
        assert_eq!(load.p_vaddr(endian) % 0x10000, 0);
        assert_eq!(load.p_align(endian), 0x10000);
    }
    let dynamic = phdrs.iter().find_map(|it| it.dynamic(endian, data).unwrap()).unwrap();
    let entries: Vec<(u64, u64)> = dynamic.iter().map(|it| (it.d_tag(endian).0 as u64, it.d_val(endian))).collect();
    let tag = |tag: elf::DynamicTag| entries.iter().find(|it| it.0 == tag.0 as u64).map(|it| it.1);
    assert_eq!(c_string(data, tag(elf::DT_STRTAB).unwrap() + tag(elf::DT_SONAME).unwrap()), "libblaze.so");
    assert_eq!(tag(elf::DT_FLAGS), Some((elf::DF_BIND_NOW | elf::DF_SYMBOLIC).0));
    assert_eq!(tag(elf::DT_FLAGS_1), Some(elf::DF_1_NOW.0));
    assert_eq!(tag(elf::DT_DEBUG), None);
    assert_eq!(tag(elf::DT_RELA), None);

    let sections = file.elf_section_table();
    let dynsyms = sections.symbols(endian, data, elf::SHT_DYNSYM).unwrap();
    let versions = sections.versions(endian, data).unwrap().unwrap();
    let (hash, _) = sections.gnu_hash(endian, data).unwrap().unwrap();
    for export in &image.exports {
        let name = export.name.as_bytes();
        let (_, symbol) = hash
            .find(endian, name, elf::gnu_hash(name), None, &dynsyms, &versions)
            .unwrap_or_else(|| panic!("`{}` is not in the hash table", export.name));
        assert_eq!(symbol.st_value(endian), export.addr);
        assert_eq!(symbol.st_type(), elf::STT_FUNC);
    }
    assert!(hash.find(endian, b"blaze_export_100", elf::gnu_hash(b"blaze_export_100"), None, &dynsyms, &versions).is_none());
}

#[test]
fn a_pointer_in_read_only_data_is_a_text_relocation() {
    let mut image = Fixture::new(executable(), &[(Class::Text, 16, 16), (Class::Rodata, 8, 8)], &Counts {
        absolute: 1,
        ..Counts::default()
    });
    let rodata = image.at(&Class::Rodata);
    image.rebase = vec![rodata];
    image.entry = Some(image.at(&Class::Text));
    let error = image.synthesize().unwrap_err();
    assert!(error.what.contains("text relocation"), "{}", error.what);
}

/// lld's initial-exec cell of a shared library: `TPOFF64` against the module at the variable's
/// offset in the template, and `DF_STATIC_TLS`.
#[test]
fn a_shared_librarys_initial_exec_cell_is_relocated_against_the_module() {
    use blaze_codegen::Arch::{Aarch64, X86_64};
    // The thread pointer offset of the variable at 4 in a 24-byte template aligned to 16.
    for (arch, tp, r_type) in [(X86_64, 4 - 32, elf::R_X86_64_TPOFF64), (Aarch64, 16 + 4, elf::R_AARCH64_TLS_TPREL)] {
        let kind = OutputKind::Shared { name: "libx.so".into(), exports: Vec::new() };
        let pieces = [(Class::Text, 16, 16), (Class::Got, 8, 8), (Class::Tdata, 24, 16)];
        let mut image = Fixture::new(output(arch, kind, None), &pieces, &Counts { got: 1, ..Counts::default() });
        let got = image.at(&Class::Got);
        image.got = vec![GotEntry { addr: got, holds: Holds::TpOff(tp) }];
        image.tls = Some(Tls { addr: image.at(&Class::Tdata), init: 24, size: 24, align: 16 });

        let bytes = image.link();
        let data = &bytes[..];
        let file = ElfFile64::<Endianness>::parse(data).unwrap();
        let endian = file.endian();
        let sections = file.elf_section_table();
        let (_, rela) = sections.section_by_name(endian, b".rela.dyn").unwrap();
        let (relas, _) = rela.rela(endian, data).unwrap().unwrap();
        let relas: Vec<(u64, u32, u32, i64)> = relas
            .iter()
            .map(|it| (it.r_offset(endian), it.r_type(endian, false).0, it.r_sym(endian, false), it.r_addend(endian)))
            .collect();
        assert_eq!(relas, [(got, r_type.0, 0, 4)], "{arch:?}");
        let dynamic = file.elf_program_headers().iter().find_map(|it| it.dynamic(endian, data).unwrap()).unwrap();
        let flags = dynamic.iter().find(|it| it.d_tag(endian) == elf::DT_FLAGS).unwrap().d_val(endian);
        assert_eq!(flags, (elf::DF_BIND_NOW | elf::DF_SYMBOLIC | elf::DF_STATIC_TLS).0, "{arch:?}");
    }
}

#[test]
fn an_initial_exec_cell_outside_the_template_is_an_error() {
    let kind = OutputKind::Shared { name: "libx.so".into(), exports: Vec::new() };
    let output = output(blaze_codegen::Arch::X86_64, kind, None);
    let pieces = [(Class::Text, 16, 16), (Class::Got, 8, 8), (Class::Tdata, 8, 8)];
    let mut image = Fixture::new(output, &pieces, &Counts { got: 1, ..Counts::default() });
    let got = image.at(&Class::Got);
    image.got = vec![GotEntry { addr: got, holds: Holds::TpOff(-16) }];
    image.tls = Some(Tls { addr: image.at(&Class::Tdata), init: 8, size: 8, align: 8 });
    let error = image.synthesize().unwrap_err();
    assert!(error.what.contains("outside the TLS template"), "{}", error.what);
    image.tls = None;
    let error = image.synthesize().unwrap_err();
    assert!(error.what.contains("no TLS"), "{}", error.what);
}

#[test]
fn hashes_are_the_elf_ones() {
    for name in [&b""[..], b"malloc", b"GLIBC_2.2.5", b"_ZN4core3fmt5write17h0123456789abcdefE"] {
        assert_eq!(gnu_hash(name), elf::gnu_hash(name));
        assert_eq!(elf_hash(name), elf::hash(name));
    }
}

#[test]
fn sections_are_placed_as_lld_places_them() {
    let named = |name: &str, write: bool, exec: bool| Class::Named { name: crate::model::SectionName::new(name), write, exec };
    let cases = [
        (".text", false, true, false, Class::Text, 0),
        (".text.unlikely._ZN3foo3barE", false, true, false, Class::Text, 0),
        (".rodata.str1.1", false, false, false, Class::Rodata, 0),
        (".gcc_except_table._ZN3foo", false, false, false, Class::Rodata, 0),
        (".data", true, false, false, Class::Data, 0),
        (".data.rel.local", true, false, false, Class::Data, 0),
        (".data.rel.ro.local", true, false, false, Class::RelRo, 0),
        (".data.rel.ro", true, false, false, Class::RelRo, 0),
        (".bss.foo", true, false, false, Class::Bss, 0),
        (".tdata.x", true, false, true, Class::Tdata, 0),
        (".tbss.x", true, false, true, Class::Tbss, 0),
        (".init_array", true, false, false, Class::InitArray, 65536),
        (".init_array.00099", true, false, false, Class::InitArray, 99),
        (".ctors.65435", true, false, false, Class::InitArray, 100),
        (".ctors", true, false, false, Class::InitArray, 65536),
        (".fini_array.5", true, false, false, Class::FiniArray, 5),
        (".dtors", true, false, false, Class::FiniArray, 65536),
        (".preinit_array", true, false, false, Class::PreinitArray, 0),
        (".preinit_array.5", true, false, false, Class::PreinitArray, 0),
        ("linkme_ITEMS", true, false, false, named("linkme_ITEMS", true, false), 0),
        ("my_code", false, true, false, named("my_code", false, true), 0),
        (".init", false, true, false, named(".init", false, true), 0),
        (".textual", false, false, false, named(".textual", false, false), 0),
    ];
    for (name, write, exec, tls, class, rank) in cases {
        assert_eq!(section(name, write, exec, tls), Ok((class, rank)), "{name}");
    }
    assert!(section("rwx", true, true, false).is_err());
    assert!(section("", false, false, false).is_err());
}

#[test]
fn order_makes_four_segments() {
    let writer = Elf { output: executable(), last: None };
    let mut classes = vec![
        Class::Bss,
        Class::Got,
        Class::Text,
        Class::Synthetic(super::DYNAMIC),
        Class::Tbss,
        Class::Named { name: "linkme_ITEMS".into(), write: true, exec: false },
        Class::Synthetic(super::DYNSYM),
        Class::Rodata,
        Class::Tdata,
        Class::Data,
        Class::Synthetic(super::INTERP),
        Class::EhFrame,
        Class::InitArray,
        Class::Stubs,
        Class::Table,
        Class::RelRo,
    ];
    writer.order(&mut classes);
    let segments: Vec<Segment> = classes.iter().map(Segment::of).collect();
    assert!(segments.is_sorted(), "{classes:?}");
    assert_eq!(classes[0], Class::Synthetic(super::INTERP));
    assert_eq!(classes[classes.len() - 3..], [Class::Tdata, Class::Tbss, Class::Bss]);
    // A change of protection at each change of segment, and none inside one.
    for pair in classes.windows(2) {
        let same = Segment::of(&pair[0]) == Segment::of(&pair[1]);
        assert_eq!(same, writer.prot(&pair[0]) == writer.prot(&pair[1]), "{pair:?}");
    }
}

#[test]
fn specials() {
    let writer = Elf { output: executable(), last: None };
    assert_eq!(writer.special("__ehdr_start"), Some(Special::ImageBase));
    assert_eq!(writer.special("__dso_handle"), Some(Special::ImageBase));
    assert_eq!(writer.special("__blaze_live"), Some(Special::LiveDescriptor));
    assert_eq!(
        writer.special("__start_linkme_ITEMS"),
        Some(Special::Section { name: "linkme_ITEMS".into(), end: false })
    );
    assert_eq!(writer.special("__stop_linkme_ITEMS"), Some(Special::Section { name: "linkme_ITEMS".into(), end: true }));
    assert_eq!(writer.special("__start_.data"), None);
    assert_eq!(writer.special("__start_"), None);
    assert_eq!(writer.special("malloc"), None);

    let region = |class: Class, addr: u64, size: u64| {
        let prot = writer.prot(&class);
        Region { class, addr, capacity: size, size, prot, align: 8 }
    };
    let regions = [
        region(Class::Synthetic(super::DYNSYM), 0x300, 0x40),
        region(Class::Text, 0x1000, 0x100),
        region(Class::InitArray, 0x2000, 0x10),
        region(Class::Synthetic(super::DYNAMIC), 0x2010, 0x100),
        region(Class::Got, 0x2110, 0x20),
        region(Class::Data, 0x3000, 0x18),
        region(Class::Bss, 0x3018, 0x40),
    ];
    let at = |name: &str| match writer.special(name) {
        Some(Special::Placed(address)) => address(&regions),
        other => panic!("`{name}` is {other:?}"),
    };
    assert_eq!(at("__init_array_start"), 0x2000);
    assert_eq!(at("__init_array_end"), 0x2010);
    assert_eq!(at("__fini_array_start"), 0, "an absent array is empty at the image's start");
    assert_eq!(at("__fini_array_end"), 0);
    assert_eq!(at("_DYNAMIC"), 0x2010);
    assert_eq!(at("_GLOBAL_OFFSET_TABLE_"), 0x2110);
    assert_eq!(at("etext"), 0x1100);
    assert_eq!(at("_edata"), 0x3018);
    assert_eq!(at("__bss_start"), 0x3018);
    assert_eq!(at("_end"), 0x3058);
    assert_eq!(at("end"), 0x3058);
}

#[test]
fn wasm_is_not_elf() {
    let mut output = executable();
    output.arch = blaze_codegen::Arch::Wasm32;
    assert!(writer(&output).is_err());
    assert!(writer(&executable()).is_ok());
}

fn insn(word: u32) -> [u8; 4] {
    word.to_le_bytes()
}

fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

#[test]
fn native_relocations() {
    use blaze_codegen::Arch::{Aarch64, X86_64};
    let apply = super::native::apply;

    let mut bytes = [0u8; 2];
    apply(X86_64, elf::R_X86_64_16.0, &mut bytes, 0x1000, 0xfff0, 0x0f).unwrap();
    assert_eq!(bytes, [0xff, 0xff]);
    assert!(apply(X86_64, elf::R_X86_64_16.0, &mut bytes, 0, 0x1_0000, 0).is_err());
    let mut bytes = [0u8; 1];
    apply(X86_64, elf::R_X86_64_PC8.0, &mut bytes, 0x1000, 0x1000, -2).unwrap();
    assert_eq!(bytes, [0xfe]);
    assert!(apply(X86_64, elf::R_X86_64_PC8.0, &mut bytes, 0x1000, 0x1080, 0).is_err());
    assert!(apply(X86_64, elf::R_X86_64_PC32.0, &mut [0; 4], 0, 0, 0).is_err(), "not a native type");

    // ADRP x0, page(S) with no range check.
    let mut bytes = insn(0x9000_0000);
    apply(Aarch64, elf::R_AARCH64_ADR_PREL_PG_HI21_NC.0, &mut bytes, 0x1_0000, 0x1_3456, 0).unwrap();
    let w = word(&bytes);
    let imm = ((w >> 29) & 3) | (((w >> 5) & 0x7_ffff) << 2);
    assert_eq!(imm, 3);
    assert_eq!(w & 0x9f00_001f, 0x9000_0000);

    // LDR x1, label: 19 bits of words.
    let mut bytes = insn(0x5800_0001);
    apply(Aarch64, elf::R_AARCH64_LD_PREL_LO19.0, &mut bytes, 0x1000, 0x900, 0).unwrap();
    assert_eq!((word(&bytes) >> 5) & 0x7_ffff, ((-0x700i32 >> 2) as u32) & 0x7_ffff);
    assert!(apply(Aarch64, elf::R_AARCH64_LD_PREL_LO19.0, &mut insn(0x5800_0001), 0x1000, 0x1002, 0).is_err());

    // A negative MOVZ becomes MOVN of the inverted bits; a MOVK keeps its opcode.
    let movz = 0xd280_0000u32;
    let mut bytes = insn(movz);
    apply(Aarch64, elf::R_AARCH64_MOVW_SABS_G0.0, &mut bytes, 0, 0, -2).unwrap();
    assert_eq!(word(&bytes), 0x9280_0000 | (1 << 5));
    let mut bytes = insn(movz);
    apply(Aarch64, elf::R_AARCH64_MOVW_SABS_G0.0, &mut bytes, 0, 0x1234, 0).unwrap();
    assert_eq!(word(&bytes), movz | (0x1234 << 5));
    assert!(apply(Aarch64, elf::R_AARCH64_MOVW_SABS_G0.0, &mut insn(movz), 0, 0x1_0000, 0).is_err());
    let movk = 0xf2a0_0000u32;
    let mut bytes = insn(movk);
    apply(Aarch64, elf::R_AARCH64_MOVW_PREL_G1_NC.0, &mut bytes, 0x1000, 0x1_2345_1000, 0).unwrap();
    assert_eq!(word(&bytes), movk | (0x2345 << 5));

    let mut bytes = [0u8; 2];
    apply(Aarch64, elf::R_AARCH64_PREL16.0, &mut bytes, 0x2000, 0x1000, 0).unwrap();
    assert_eq!(u16::from_le_bytes(bytes), (-0x1000i16) as u16);
}

/// A seeded Fisher-Yates shuffle.
fn shuffled<T>(mut items: Vec<T>, mut seed: u64) -> Vec<T> {
    for at in (1..items.len()).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        items.swap(at, (seed >> 33) as usize % (at + 1));
    }
    items
}

impl Fixture {
    fn synthetic(&self, name: &'static str) -> &[u8] {
        &self.contents[self.index(&Class::Synthetic(name))]
    }
}

/// `.gnu.hash` as it was built before the counting sort: a stable sort by bucket.
fn gnu_hash_model(names: &[String], symoffset: u32) -> (Vec<u8>, Vec<usize>) {
    let n = names.len() as u64;
    let (buckets, mask_words) = ((n / 4).max(1), (n * 12 / 64 + 1).next_power_of_two());
    let mut exports: Vec<(u32, u32, usize)> = names
        .iter()
        .enumerate()
        .map(|(at, it)| {
            let hash = elf::gnu_hash(it.as_bytes());
            (hash % buckets as u32, hash, at)
        })
        .collect();
    exports.sort_by_key(|&(bucket, _, _)| bucket);
    let mut out = Vec::new();
    for word in [buckets as u32, symoffset, mask_words as u32, 26] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    let mut bloom = vec![0u64; mask_words as usize];
    for &(_, hash, _) in &exports {
        bloom[((hash as u64 / 64) % mask_words) as usize] |= (1 << (hash % 64)) | (1 << ((hash >> 26) % 64));
    }
    for word in bloom {
        out.extend_from_slice(&word.to_le_bytes());
    }
    let mut first = vec![0u32; buckets as usize];
    for (at, &(bucket, _, _)) in exports.iter().enumerate().rev() {
        first[bucket as usize] = symoffset + at as u32;
    }
    for it in first {
        out.extend_from_slice(&it.to_le_bytes());
    }
    for (at, &(bucket, hash, _)) in exports.iter().enumerate() {
        let last = exports.get(at + 1).is_none_or(|next| next.0 != bucket);
        out.extend_from_slice(&((hash & !1) | last as u32).to_le_bytes());
    }
    (out, exports.iter().map(|it| it.2).collect())
}

/// Past the parallel threshold, `.gnu.hash` and the order of `.dynsym` are the bytes the
/// stable sort gave, and every export is found.
#[test]
fn many_exports_hash_as_the_model_does() {
    let names: Vec<String> = (0..3 * super::dynamic::CHUNK).map(|it| format!("_ZN5blaze6export{it}17h{it:016x}E")).collect();
    let counts = Counts {
        imports: 2,
        import_names: 6 + 4 + 2 * 11,
        libraries: 1,
        library_names: 9,
        exports: names.len() as u64,
        export_names: names.iter().map(|it| it.len() as u64).sum(),
        ..Counts::default()
    };
    let kind = OutputKind::Shared { name: "libmany.so".into(), exports: names.clone() };
    let output = output(blaze_codegen::Arch::X86_64, kind, None);
    let mut image = Fixture::new(output, &[(Class::Text, 4 * names.len() as u64, 4)], &counts);
    let text = image.at(&Class::Text);
    image.libraries = vec![Library { name: "libc.so.6".into(), as_needed: true }];
    image.imports = vec![import("malloc", Some(0), Some("GLIBC_2.2.5"), false), import("free", Some(0), Some("GLIBC_2.2.5"), false)];
    image.exports = names
        .iter()
        .enumerate()
        .map(|(at, name)| Export { name: name.clone(), addr: text + 4 * at as u64, size: 4, kind: SymbolKind::Function })
        .collect();
    let bytes = image.link();

    let (model, order) = gnu_hash_model(&names, 3);
    assert_eq!(image.synthetic(super::GNU_HASH), model);
    let dynsym = image.synthetic(super::DYNSYM);
    let dynstr = image.synthetic(super::DYNSTR);
    for (at, &export) in order.iter().enumerate() {
        let entry = &dynsym[24 * (3 + at)..][..24];
        let name = u32::from_le_bytes(entry[..4].try_into().unwrap());
        assert_eq!(c_string(dynstr, name.into()), names[export]);
        assert_eq!(u64::from_le_bytes(entry[8..16].try_into().unwrap()), text + 4 * export as u64);
    }
    let versym = image.synthetic(super::VERSYM);
    assert_eq!(versym.len(), 2 * (3 + names.len()));
    assert!(versym[6..].chunks(2).all(|it| it == elf::VER_NDX_GLOBAL.0.to_le_bytes()));

    let data = &bytes[..];
    let file = ElfFile64::<Endianness>::parse(data).unwrap();
    let endian = file.endian();
    let sections = file.elf_section_table();
    let dynsyms = sections.symbols(endian, data, elf::SHT_DYNSYM).unwrap();
    let versions = sections.versions(endian, data).unwrap().unwrap();
    let (hash, _) = sections.gnu_hash(endian, data).unwrap().unwrap();
    for (at, name) in names.iter().enumerate() {
        let (_, symbol) = hash
            .find(endian, name.as_bytes(), elf::gnu_hash(name.as_bytes()), None, &dynsyms, &versions)
            .unwrap_or_else(|| panic!("`{name}` is not in the hash table"));
        assert_eq!(symbol.st_value(endian), text + 4 * at as u64);
    }
}

/// `.rela.dyn` past the parallel threshold: the slid addresses sorted and first whatever order
/// they come in, the bytes the sequential build gave.
#[test]
fn many_slid_addresses_are_relocated_as_the_model_does() {
    let n = 3 * super::dynamic::CHUNK + 17;
    let counts = Counts { absolute: n as u64 + 1, imports: 1, import_names: 6, libraries: 1, library_names: 9, ..Counts::default() };
    let pieces = [(Class::Text, 16, 16), (Class::Data, 8 * n as u64 + 8, 8)];
    let mut image = Fixture::new(executable(), &pieces, &counts);
    let data = image.at(&Class::Data);
    let text = image.at(&Class::Text);
    for at in 0..n {
        image.put(&Class::Data, 8 * at, &(text + at as u64 * 3).to_le_bytes());
    }
    image.entry = Some(text);
    image.libraries = vec![Library { name: "libc.so.6".into(), as_needed: false }];
    image.imports = vec![import("malloc", Some(0), None, false)];
    image.binds = vec![Bind { addr: data + 8 * n as u64, import: 0, addend: 0 }];
    let sorted: Vec<u64> = (0..n as u64).map(|at| data + 8 * at).collect();

    let mut expected: Vec<(u64, elf::RelocationType, u32, i64)> =
        (0..n as u64).map(|at| (data + 8 * at, elf::R_X86_64_RELATIVE, 0, (text + at * 3) as i64)).collect();
    expected.sort();
    expected.push((data + 8 * n as u64, elf::R_X86_64_64, 1, 0));
    let mut model = Vec::new();
    for (at, r_type, sym, addend) in expected {
        model.extend_from_slice(&at.to_le_bytes());
        model.extend_from_slice(&((u64::from(sym) << 32) | u64::from(r_type.0)).to_le_bytes());
        model.extend_from_slice(&addend.to_le_bytes());
    }

    for (rebase, what) in [(sorted.clone(), "sorted"), (shuffled(sorted.clone(), 7), "shuffled")] {
        image.rebase = rebase;
        let synthesized = image.synthesize().unwrap();
        let rela = &synthesized.iter().find(|it| it.0 == Class::Synthetic(super::RELA_DYN)).unwrap().1;
        assert_eq!(*rela, model, "{what}");
        let dynamic = &synthesized.iter().find(|it| it.0 == Class::Synthetic(super::DYNAMIC)).unwrap().1;
        let count = dynamic
            .chunks(16)
            .find(|it| u64::from_le_bytes(it[..8].try_into().unwrap()) == elf::DT_RELACOUNT.0 as u64)
            .map(|it| u64::from_le_bytes(it[8..].try_into().unwrap()));
        assert_eq!(count, Some(n as u64), "{what}");
    }

    image.rebase = vec![data, text];
    let error = image.synthesize().unwrap_err();
    assert!(error.what.contains("text relocation"), "{}", error.what);
}

/// `.symtab` and `.strtab` built in parallel chunks: every function in order with its name,
/// unnamed functions left out, an empty name at offset 0, then the exports and imports.
#[test]
fn many_functions_are_named_in_the_symbol_table() {
    let n = 2 * super::dynamic::CHUNK + 100;
    let counts = Counts {
        functions: n as u64,
        imports: 1,
        import_names: 6,
        exports: 1,
        export_names: 19,
        libraries: 1,
        library_names: 9,
        ..Counts::default()
    };
    let mut image = Fixture::new(executable(), &[(Class::Text, 16 * n as u64, 16)], &counts);
    let text = image.at(&Class::Text);
    image.entry = Some(text);
    image.functions = (0..n)
        .map(|at| Function {
            addr: text + 16 * at as u64,
            size: 16,
            fde: None,
            windows: None,
            name: match at % 7 {
                0 => None,
                3 if at == 3 => Some("".into()),
                _ => Some(format!("_ZN4test8function{at}E").into()),
            },
        })
        .collect();
    image.exports = vec![Export { name: "_ZN4test8function1E".into(), addr: text + 16, size: 16, kind: SymbolKind::Function }];
    image.libraries = vec![Library { name: "libc.so.6".into(), as_needed: false }];
    image.imports = vec![import("malloc", Some(0), None, false)];
    let bytes = image.link();

    let data = &bytes[..];
    let file = ElfFile64::<Endianness>::parse(data).unwrap();
    let endian = file.endian();
    let sections = file.elf_section_table();
    let symtab = sections.symbols(endian, data, elf::SHT_SYMTAB).unwrap();
    let named: Vec<&Function> = image.functions.iter().filter(|it| it.name.is_some()).collect();
    let symbols = symtab.symbols();
    assert_eq!(symbols.len(), 1 + named.len() + 2);
    let (_, shdr) = sections.section_by_name(endian, b".symtab").unwrap();
    assert_eq!(shdr.sh_info(endian) as usize, 1 + named.len());
    for (symbol, function) in symbols[1..].iter().zip(&named) {
        assert_eq!(symbol.name(endian, symtab.strings()).unwrap(), function.name.as_deref().unwrap().as_bytes());
        assert_eq!(symbol.st_value(endian), function.addr);
        assert_eq!(symbol.st_size(endian), 16);
        assert_eq!(symbol.st_bind(), elf::STB_LOCAL);
        assert_eq!(symbol.st_shndx(endian).0, 1 + image.index(&Class::Text) as u16);
    }
    assert_eq!(symbols[1 + named.iter().position(|it| it.name.as_deref() == Some("")).unwrap()].st_name(endian), 0);
    let export = &symbols[1 + named.len()];
    assert_eq!(export.name(endian, symtab.strings()).unwrap(), b"_ZN4test8function1E");
    assert_eq!(export.st_bind(), elf::STB_GLOBAL);
    let import = &symbols[2 + named.len()];
    assert_eq!(import.name(endian, symtab.strings()).unwrap(), b"malloc");
    assert!(import.is_undefined(endian));
}

/// FDEs of two CIEs, each CIE parsed once: every FDE in the table, sorted by address.
#[test]
fn eh_frame_hdr_indexes_fdes_of_every_cie() {
    use gimli::write::{Address, CommonInformationEntry, EhFrame, EndianVec, FrameDescriptionEntry, FrameTable};
    let encoding = gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
    let build = |text: u64| {
        let mut table = FrameTable::default();
        let a = table.add_cie(CommonInformationEntry::new(encoding, 1, -8, gimli::Register(16)));
        let b = table.add_cie(CommonInformationEntry::new(encoding, 4, -8, gimli::Register(16)));
        for at in 0..40u64 {
            let cie = if at % 3 == 0 { b } else { a };
            table.add_fde(cie, FrameDescriptionEntry::new(Address::Constant(text + 16 * ((at * 17) % 40)), 16));
        }
        let mut section = EhFrame(EndianVec::new(gimli::LittleEndian));
        table.write_eh_frame(&mut section).unwrap();
        section.0.into_vec()
    };
    let len = build(0).len() as u64;
    let counts = Counts { fdes: 40, ..Counts::default() };
    let mut image = Fixture::new(executable(), &[(Class::Text, 16 * 40, 16), (Class::EhFrame, len, 8)], &counts);
    let text = image.at(&Class::Text);
    image.entry = Some(text);
    image.put(&Class::EhFrame, 0, &build(text));
    image.link();
    let hdr = image.at(&Class::Synthetic(super::EH_FRAME_HDR));
    let table = image.synthetic(super::EH_FRAME_HDR);
    let word = |at: usize| i32::from_le_bytes(table[at..at + 4].try_into().unwrap()) as i64;
    assert_eq!(word(8), 40);
    let pcs: Vec<i64> = (0..40).map(|at| word(12 + 8 * at) + hdr as i64).collect();
    assert_eq!(pcs, (0..40).map(|at| text as i64 + 16 * at).collect::<Vec<_>>());
}

/// A relocatable x86-64 object of `n` functions, each in a `.text.N` of its own, and `n`
/// undefined symbols, every one named in `.data` by an absolute relocation.
fn object_of(n: usize) -> Vec<u8> {
    use object::write::{Object as Builder, Relocation, Symbol as Def, SymbolSection};
    use object::{Architecture, BinaryFormat, RelocationFlags, SectionKind, SymbolFlags, SymbolKind as Type, SymbolScope};
    let mut builder = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let data = builder.add_section(Vec::new(), b".data".to_vec(), SectionKind::Data);
    builder.append_section_data(data, &vec![0; 16 * n], 8);
    let symbol = |name: String, section: SymbolSection| Def {
        name: name.into_bytes(),
        value: 0,
        size: 0,
        kind: if section == SymbolSection::Undefined { Type::Unknown } else { Type::Text },
        scope: SymbolScope::Dynamic,
        weak: false,
        section,
        flags: SymbolFlags::None,
    };
    for at in 0..n {
        let text = builder.add_section(Vec::new(), format!(".text.f{at}").into_bytes(), SectionKind::Text);
        builder.append_section_data(text, &[0xc3], 16);
        let function = builder.add_symbol(symbol(format!("f{at}"), SymbolSection::Section(text)));
        let external = builder.add_symbol(symbol(format!("ext{at}"), SymbolSection::Undefined));
        for (slot, target) in [(2 * at, function), (2 * at + 1, external)] {
            let flags = RelocationFlags::Elf { r_type: elf::R_X86_64_64 };
            builder.add_relocation(data, Relocation { offset: 8 * slot as u64, symbol: target, addend: 0, flags }).unwrap();
        }
    }
    builder.write().unwrap()
}

/// A relocatable object is read into a constant number of buffers whatever its size: the
/// object's sections, symbols, fixups and name arena (4); four per string table (`.shstrtab`,
/// `.strtab`): its NUL bitset, the NULs before each word, a slot per string and the strings
/// pushed (8); and the relocation sections' order (1). 13 here; the two group columns make 15
/// for an object with COMDAT groups. Every buffer is sized once from the section headers.
#[test]
fn an_object_read_allocates_a_constant_number_of_buffers() {
    let read = |n: usize| {
        let bytes = object_of(n);
        let (read, used) = blaze_alloc_count::measure(|| {
            super::read::read(blaze_codegen::Arch::X86_64, "f.o", &bytes, &crate::NativeKind::Object)
        });
        let Ok(crate::model::NativeFile::Object(object)) = read else { panic!("not read as an object") };
        assert_eq!(object.symbols.len(), 1 + 2 * n, "the null symbol and the object's own");
        assert_eq!(object.fixups.len(), 2 * n);
        used
    };
    let (small, large) = (read(8), read(1000));
    assert_eq!(small.allocations, large.allocations, "an object read allocates per symbol, section or relocation");
    assert!(large.allocations <= 13, "an object read: {} allocations, budget 13", large.allocations);
}

/// A shared library this writer made, exporting `n` functions: what its reader reads back.
fn shared_library(n: usize) -> Vec<u8> {
    let names: Vec<String> = (0..n).map(|it| format!("blaze_export_{it}")).collect();
    let counts = Counts {
        exports: n as u64,
        export_names: names.iter().map(|it| it.len() as u64).sum(),
        ..Counts::default()
    };
    let kind = OutputKind::Shared { name: "libblaze.so".into(), exports: names.clone() };
    let output = output(blaze_codegen::Arch::X86_64, kind, None);
    let mut image = Fixture::new(output, &[(Class::Text, 4 * n as u64, 4)], &counts);
    let text = image.at(&Class::Text);
    image.exports = names
        .iter()
        .enumerate()
        .map(|(at, name)| Export { name: name.clone(), addr: text + 4 * at as u64, size: 4, kind: SymbolKind::Function })
        .collect();
    image.link()
}

/// A shared library's symbols are read into its one `Names` arena with a constant number of
/// allocations: `object`'s version table, four for `.dynstr` (as for any string table), the
/// arena, the symbols, the version slots and the `DT_SONAME`. 9, whatever the number of
/// symbols; a version the library defines adds its name to `Shared::versions`.
#[test]
fn a_shared_library_read_allocates_a_constant_number_of_buffers() {
    let read = |n: usize| {
        let bytes = shared_library(n);
        let (read, used) = blaze_alloc_count::measure(|| {
            super::read::read(
                blaze_codegen::Arch::X86_64,
                "libblaze.so",
                &bytes,
                &crate::NativeKind::Shared { as_needed: false },
            )
        });
        let Ok(crate::model::NativeFile::Shared(shared)) = read else { panic!("not read as a shared library") };
        assert_eq!(shared.name, "libblaze.so");
        assert_eq!(shared.symbols.len(), n);
        for (at, symbol) in shared.symbols.iter().enumerate() {
            assert!(shared.symbol_name(symbol).starts_with("blaze_export_"), "symbol {at}");
        }
        let names = n * "blaze_export_".len() + (0..n).map(|it| it.to_string().len()).sum::<usize>();
        assert_eq!(shared.names.bytes(), names, "each name pushed once");
        used
    };
    let (small, large) = (read(16), read(2000));
    assert_eq!(small.allocations, large.allocations, "a shared library read allocates per symbol");
    assert!(large.allocations <= 9, "a shared library read: {} allocations, budget 9", large.allocations);
}

/// An executable importing `n` versioned functions from one library, exporting `n` and with
/// `n` slid addresses: every per-symbol table scales with `n`.
fn many_symbols(n: usize) -> Fixture {
    let imports: Vec<Import> = (0..n).map(|at| import(&format!("import_{at}"), Some(0), Some("GLIBC_2.2.5"), false)).collect();
    let exports: Vec<String> = (0..n).map(|at| format!("export_{at}")).collect();
    let counts = Counts {
        imports: n as u64,
        import_names: imports.iter().map(|it| (it.name.len() + "GLIBC_2.2.5".len()) as u64).sum(),
        libraries: 1,
        library_names: "libc.so.6".len() as u64,
        absolute: n as u64,
        exports: n as u64,
        export_names: exports.iter().map(|it| it.len() as u64).sum(),
        ..Counts::default()
    };
    let pieces = [(Class::Text, 4 * n as u64, 4), (Class::Data, 8 * n as u64, 8)];
    let mut image = Fixture::new(executable(), &pieces, &counts);
    let (text, data) = (image.at(&Class::Text), image.at(&Class::Data));
    for at in 0..n {
        image.put(&Class::Data, 8 * at, &(text + 4 * at as u64).to_le_bytes());
    }
    image.entry = Some(text);
    image.libraries = vec![Library { name: "libc.so.6".into(), as_needed: false }];
    image.imports = imports;
    image.exports = exports
        .into_iter()
        .enumerate()
        .map(|(at, name)| Export { name, addr: text + 4 * at as u64, size: 4, kind: SymbolKind::Function })
        .collect();
    image.rebase = (0..n as u64).map(|at| data + 8 * at).collect();
    image
}

/// `.dynsym`, `.dynstr`, `.gnu.hash`, the versions, `.rela.dyn` and `.dynamic` allocate
/// nothing per symbol beyond their output buffers. Measured in a one-thread pool, whose
/// worker does all the work, so the per-thread count sees every allocation. 22 in all, each
/// once: the library slots, `.dynstr`'s bytes and lookup, the needed names, the version list
/// and lookup, the hashes, bucket ends and order, the other nine regions' bytes (`.interp`, the
/// build ID, `.dynsym`, `.gnu.version`, `.gnu.version_r`, `.gnu.hash`, `.rela.dyn`, `.dynamic`,
/// `.eh_frame_hdr`), the relocations and their per-chunk results,
/// the `.preinit_array` class looked up, and the list `synthesize` returns; then 10 more, the
/// `Arc` of each region's `CowBytes`, which the core no longer makes when it takes them.
#[test]
fn dynamic_tables_allocate_nothing_per_symbol() {
    let pool = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    let used = |n: usize| {
        let image = many_symbols(n);
        pool.install(|| {
            image.with(|placed| {
                image.writer.synthesize(placed).unwrap();
                let (tables, used) = blaze_alloc_count::measure(|| image.writer.synthesize(placed));
                let tables = tables.unwrap();
                let region = |name: &'static str| &tables.iter().find(|it| it.0 == Class::Synthetic(name)).unwrap().1;
                assert_eq!(region(super::DYNSYM).len(), 24 * (1 + 2 * n));
                assert_eq!(region(super::RELA_DYN).len(), 24 * n);
                assert_eq!(region(super::VERSYM).len(), 2 * (1 + 2 * n));
                used
            })
        })
    };
    let (small, large) = (used(64), used(2000));
    assert_eq!(small.allocations, large.allocations, "the dynamic tables allocate per symbol");
    assert!(large.allocations <= 32, "the dynamic tables: {} allocations, budget 32", large.allocations);
}

/// The functions of [`relinked`].
const RELINKED: usize = 40;

fn incremental() -> Output {
    let mut output = executable();
    output.incremental = true;
    output
}

/// Function `at`'s name, all of one length.
fn relinked_name(at: usize, letter: char) -> String {
    format!("_ZN4test1{letter}{at:02}E")
}

/// An incremental image of [`RELINKED`] functions of 16 bytes, each named and with its FDE,
/// and a slid pointer to each in every other 8 bytes of `.data`; a GOT cell holding the
/// first, and a bind between the pointers.
fn relinked() -> Fixture {
    relinked_of(RELINKED)
}

/// [`relinked`] of `n` functions.
fn relinked_of(n: usize) -> Fixture {
    let names: Vec<String> = (0..n).map(|at| relinked_name(at, 'f')).collect();
    let eh_frame_len = eh_frame(&vec![(0, 16); n]).len() as u64;
    let counts = Counts {
        functions: n as u64,
        function_names: names.iter().map(|it| it.len() as u64).sum(),
        absolute: n as u64 + 4,
        got: 1,
        fdes: n as u64,
        imports: 1,
        import_names: 6,
        libraries: 1,
        library_names: 9,
        ..Counts::default()
    };
    let pieces = [
        (Class::Text, 16 * n as u64, 16),
        (Class::Data, 16 * n as u64, 8),
        (Class::Got, 8, 8),
        (Class::EhFrame, eh_frame_len, 8),
    ];
    let mut image = Fixture::new(incremental(), &pieces, &counts);
    let (text, data, got) = (image.at(&Class::Text), image.at(&Class::Data), image.at(&Class::Got));
    image.entry = Some(text);
    image.functions = (names.iter().enumerate())
        .map(|(at, name)| Function { addr: text + 16 * at as u64, size: 16, fde: None, windows: None, name: Some(name.as_str().into()) })
        .collect();
    for at in 0..n {
        image.put(&Class::Data, 16 * at, &(text + 16 * at as u64).to_le_bytes());
    }
    image.put(&Class::Got, 0, &text.to_le_bytes());
    image.got = vec![GotEntry { addr: got, holds: Holds::Local(text) }];
    image.rebase = std::iter::once(got).chain((0..n as u64).map(|at| data + 16 * at)).collect();
    image.libraries = vec![Library { name: "libc.so.6".into(), as_needed: false }];
    image.imports = vec![import("malloc", Some(0), None, false)];
    image.binds = vec![Bind { addr: data + 8, import: 0, addend: 0 }];
    let fdes: Vec<(u64, u32)> = (0..n as u64).map(|at| (text + 16 * at, 16)).collect();
    image.put(&Class::EhFrame, 0, &eh_frame(&fdes));
    image
}

fn splice(old: std::ops::Range<u32>, new: std::ops::Range<u32>) -> Splice {
    Splice { old, new }
}

fn delta<'a>(
    from: u64,
    functions: &'a [Splice],
    rebase: &'a [Splice],
    got: &'a [Splice],
    written: &'a [(u64, u64)],
) -> Delta<'a> {
    Delta { functions, rebase, got, binds: &[], same_names: true, same_regions: true, written, from }
}

/// How many builds of `.rela.dyn`, `.eh_frame_hdr` and the symbol table the image's writer
/// patched.
fn patched(image: &Fixture) -> [u32; 3] {
    let last = image.writer.last().unwrap();
    [last.rela.patched, last.eh_frame_hdr.patched, last.symtab.patched]
}

fn table<'t>(tables: &'t [(Class, Vec<u8>)], name: &'static str) -> &'t [u8] {
    &tables.iter().find(|it| it.0 == Class::Synthetic(name)).unwrap().1
}

/// The range of addresses where `new` differs from `old`, both at `at`.
fn changed(old: &[u8], new: &[u8], at: u64) -> (u64, u64) {
    let first = old.iter().zip(new).position(|(a, b)| a != b).unwrap();
    let last = old.iter().zip(new).rposition(|(a, b)| a != b).unwrap();
    (at + first as u64, (last + 1 - first) as u64)
}

/// Builds the image with its own writer, which kept its last tables, as synthesis
/// `synthesis` with `delta`, and with a fresh writer from nothing: the same tables and
/// file. The first's.
fn relinks_as_built(image: &Fixture, synthesis: u64, delta: &Delta<'_>) -> (Vec<(Class, Vec<u8>)>, Vec<(u64, Vec<u8>)>) {
    let relinked = image.build(&image.writer, synthesis, Some(delta));
    let fresh = Elf { output: image.writer.output.clone(), last: None };
    let built = image.build(&fresh, synthesis, None);
    assert_eq!(relinked.0, built.0, "the synthetic regions");
    assert_eq!(relinked.1, built.1, "the file's parts");
    relinked
}

/// A pointer moved with its slid address, one pointing elsewhere, and a GOT cell holding
/// another function: the `RELATIVE` run patched in place is the one a build makes.
#[test]
fn a_relink_patches_rela_dyn_as_a_build_makes_it() {
    let mut image = relinked();
    let (first, _) = image.build(&image.writer, 1, None);
    assert_eq!(patched(&image), [0, 0, 0]);
    let (text, data) = (image.at(&Class::Text), image.at(&Class::Data));
    // Function 5's pointer moves 8 bytes on and to function 6; function 9's points at
    // function 20; the GOT cell holds function 1.
    image.put(&Class::Data, 16 * 5, &[0; 8]);
    image.put(&Class::Data, 16 * 5 + 8, &(text + 16 * 6).to_le_bytes());
    image.rebase[1 + 5] = data + 16 * 5 + 8;
    image.put(&Class::Data, 16 * 9, &(text + 16 * 20).to_le_bytes());
    image.put(&Class::Got, 0, &(text + 16).to_le_bytes());
    image.got[0].holds = Holds::Local(text + 16);
    let rebase = [splice(6..7, 6..7)];
    let got = [splice(0..1, 0..1)];
    let written = [(data + 16 * 5, 16), (data + 16 * 9, 8)];
    let (tables, _) = relinks_as_built(&image, 2, &delta(1, &[], &rebase, &got, &written));
    assert_ne!(table(&tables, super::RELA_DYN), table(&first, super::RELA_DYN));
    assert_eq!(patched(&image), [1, 1, 1], "every table patched");
}

/// A function renamed to a name of its length and one resized: their local symbols
/// rewritten in the kept tail are the tail a build makes; a name of another length is built.
#[test]
fn a_relink_patches_the_symbol_table_as_a_build_makes_it() {
    let mut image = relinked();
    let (_, first) = image.build(&image.writer, 1, None);
    image.functions[7].name = Some(relinked_name(7, 'g').as_str().into());
    image.functions[3].size = 12;
    let functions = [splice(3..4, 3..4), splice(7..8, 7..8)];
    let (_, parts) = relinks_as_built(&image, 2, &delta(1, &functions, &[], &[], &[]));
    assert_ne!(parts, first);
    assert_eq!(patched(&image)[2], 1);

    image.functions[7].name = Some("_ZN4test1g7E".into());
    relinks_as_built(&image, 3, &delta(2, &[splice(7..8, 7..8)], &[], &[], &[]));
    assert_eq!(patched(&image)[2], 1, "a name of another length is built");
    image.functions[7].name = None;
    relinks_as_built(&image, 4, &delta(3, &[splice(7..8, 7..8)], &[], &[], &[]));
    assert_eq!(patched(&image)[2], 1, "a name dropped is built");
}

/// An FDE's target moved: its `.eh_frame_hdr` entry patched from the FDEs walked again is the
/// table a build makes; a written range before every FDE and an FDE out of address order are
/// built.
#[test]
fn a_relink_patches_eh_frame_hdr_as_a_build_makes_it() {
    let mut image = relinked();
    let (first, _) = image.build(&image.writer, 1, None);
    let (text, eh) = (image.at(&Class::Text), image.at(&Class::EhFrame));
    let fdes = |moved: usize, to: u64| -> Vec<(u64, u32)> {
        (0..RELINKED).map(|at| if at == moved { (to, 12) } else { (text + 16 * at as u64, 16) }).collect()
    };
    let old = image.contents[image.index(&Class::EhFrame)].clone();
    let new = eh_frame(&fdes(12, text + 16 * 12 + 4));
    image.put(&Class::EhFrame, 0, &new);
    let written = [changed(&old, &new, eh)];
    let (tables, _) = relinks_as_built(&image, 2, &delta(1, &[], &[], &[], &written));
    assert_ne!(table(&tables, super::EH_FRAME_HDR), table(&first, super::EH_FRAME_HDR));
    assert_eq!(patched(&image)[1], 1);

    relinks_as_built(&image, 3, &delta(2, &[], &[], &[], &[(eh, 4)]));
    assert_eq!(patched(&image)[1], 1, "a range before every FDE is built");

    let old = new;
    let new = eh_frame(&fdes(12, text + 16 * 30 + 4));
    image.put(&Class::EhFrame, 0, &new);
    let written = [changed(&old, &new, eh)];
    relinks_as_built(&image, 4, &delta(3, &[], &[], &[], &written));
    assert_eq!(patched(&image)[1], 1, "a table out of order is built");
}

/// A delta from a synthesis the writer's tables are not of, and one with splices of another
/// length, are built whole.
#[test]
fn a_relink_from_another_synthesis_or_of_another_length_is_built() {
    let mut image = relinked();
    image.build(&image.writer, 1, None);
    let (text, data) = (image.at(&Class::Text), image.at(&Class::Data));
    image.put(&Class::Data, 16 * 9, &(text + 16 * 20).to_le_bytes());
    image.functions[7].name = Some(relinked_name(7, 'g').as_str().into());
    let functions = [splice(7..8, 7..8)];
    let written = [(data + 16 * 9, 8)];
    relinks_as_built(&image, 2, &delta(7, &functions, &[], &[], &written));
    assert_eq!(patched(&image), [0, 0, 0], "the tables are of synthesis 1");

    // A slid address added after function 6's pointer, and the last function dropped.
    image.put(&Class::Data, 16 * 6 + 8, &(text + 16 * 6).to_le_bytes());
    image.rebase.insert(8, data + 16 * 6 + 8);
    image.functions.pop();
    let rebase = [splice(8..8, 8..9)];
    let functions = [splice(39..40, 39..39)];
    let written = [(data + 16 * 6 + 8, 8)];
    relinks_as_built(&image, 3, &delta(2, &functions, &rebase, &[], &written));
    assert_eq!(patched(&image), [0, 0, 0], "a splice of another length is built");
}

/// How many of the 16 KiB pages of `new` a writer copied rather than shared with `old`.
fn copied(new: &CowBytes, old: &CowBytes) -> usize {
    let mut ranges = Vec::new();
    new.changed_since(old, &mut ranges);
    ranges.iter().map(|it| it.len().div_ceil(CowBytes::PAGE)).sum()
}

/// How many of the 16 KiB pages of `new` hold bytes other than `old`'s there.
fn differing(new: &[u8], old: &[u8]) -> usize {
    let page = CowBytes::PAGE;
    (0..new.len().div_ceil(page))
        .filter(|at| {
            let range = at * page..((at + 1) * page).min(new.len());
            old.get(range.clone()) != Some(&new[range])
        })
        .count()
}

/// A one-function relink of an image of 4,098 functions: of its kept tables (`.rela.dyn`,
/// `.eh_frame_hdr` and the symbol table's tail), it copies only the pages whose bytes it
/// changes (the function's pointer's `RELATIVE` entry, its `.eh_frame_hdr` cell and its local
/// symbol) and shares every other with the last link's; the tables are a build's.
#[test]
fn a_one_function_relink_copies_only_the_pages_it_writes() {
    const N: usize = 4098;
    let mut image = relinked_of(N);
    let (first, first_parts) = image.shared(&image.writer, 1, None);
    let (text, data, eh) = (image.at(&Class::Text), image.at(&Class::Data), image.at(&Class::EhFrame));
    // Function `k` starts 4 bytes on and is 4 shorter: its FDE and its pointer in `.data`
    // move with it.
    let k = 2000;
    let moved = text + 16 * k as u64 + 4;
    (image.functions[k].addr, image.functions[k].size) = (moved, 12);
    image.put(&Class::Data, 16 * k, &moved.to_le_bytes());
    let old = image.contents[image.index(&Class::EhFrame)].clone();
    let fdes: Vec<(u64, u32)> = (0..N).map(|at| if at == k { (moved, 12) } else { (text + 16 * at as u64, 16) }).collect();
    let new = eh_frame(&fdes);
    image.put(&Class::EhFrame, 0, &new);
    let mut written = vec![(data + 16 * k as u64, 8), changed(&old, &new, eh)];
    written.sort_unstable();
    let functions = [splice(k as u32..k as u32 + 1, k as u32..k as u32 + 1)];
    let since = delta(1, &functions, &[], &[], &written);
    let (tables, parts) = image.shared(&image.writer, 2, Some(&since));
    assert_eq!(patched(&image), [1, 1, 1], "every table patched");
    let fresh = Elf { output: image.writer.output.clone(), last: None };
    assert_eq!((flat(tables.clone()), flat(parts.clone())), image.build(&fresh, 2, None), "a build's tables and parts");

    let region = |tables: &[(Class, CowBytes)], name: &'static str| {
        tables.iter().find(|it| it.0 == Class::Synthetic(name)).unwrap().1.clone()
    };
    let tail = |parts: &[(u64, CowBytes)]| parts.iter().find(|it| it.0 != 0).unwrap().1.clone();
    let kept = [
        (".rela.dyn", region(&tables, super::RELA_DYN), region(&first, super::RELA_DYN)),
        (".eh_frame_hdr", region(&tables, super::EH_FRAME_HDR), region(&first, super::EH_FRAME_HDR)),
        ("the symbol table's tail", tail(&parts), tail(&first_parts)),
    ];
    let (mut total, mut all_copied) = (0, 0);
    for (what, new, old) in &kept {
        let (copied, differing) = (copied(new, old), differing(&new.contiguous(), &old.contiguous()));
        assert!(copied <= differing, "{what}: {copied} pages copied, {differing} changed");
        assert!((1..=2).contains(&differing), "{what}: {differing} pages changed by one function");
        total += new.len().div_ceil(CowBytes::PAGE);
        all_copied += copied;
    }
    assert!(total >= 16, "{total} pages of kept tables: the image is not large");
    eprintln!("a one-function relink among {N} functions copies {all_copied} of its kept tables' {total} pages");
}

/// The same relink with the code and data regions as the core holds them after it: the full
/// link's buffers, with the function's code, its pointer and its FDE written into copies of the
/// pages they land in. Only those pages are copied (no region is flattened for the writer),
/// and the writer, reading `.data` and `.eh_frame` in their pages, makes the tables and parts a
/// build from flat bytes makes.
#[test]
fn a_one_function_relink_reads_its_code_regions_in_pages() {
    const N: usize = 4098;
    let mut image = relinked_of(N);
    let classes = [Class::Text, Class::Data, Class::EhFrame];
    // clone: the full link's bytes of each region, which the test's own buffers are
    let full: Vec<CowBytes> = classes.iter().map(|it| CowBytes::from_vec(image.contents[image.index(it)].clone())).collect();
    image.shared(&image.writer, 1, None);
    let (text, data, eh) = (image.at(&Class::Text), image.at(&Class::Data), image.at(&Class::EhFrame));
    let k = 2000;
    let moved = text + 16 * k as u64 + 4;
    (image.functions[k].addr, image.functions[k].size) = (moved, 12);
    image.put(&Class::Text, 16 * k + 4, &[0xcc; 12]);
    image.put(&Class::Data, 16 * k, &moved.to_le_bytes());
    let old = image.contents[image.index(&Class::EhFrame)].clone();
    let fdes: Vec<(u64, u32)> = (0..N).map(|at| if at == k { (moved, 12) } else { (text + 16 * at as u64, 16) }).collect();
    let new = eh_frame(&fdes);
    image.put(&Class::EhFrame, 0, &new);
    let mut written = vec![(moved, 12), (data + 16 * k as u64, 8), changed(&old, &new, eh)];
    written.sort_unstable();

    let mut held: Vec<(Class, CowBytes)> = classes.iter().cloned().zip(full.iter().cloned()).collect();
    for (class, bytes) in &mut held {
        let index = image.index(class);
        let region = &image.regions[index];
        for &(addr, len) in written.iter().filter(|it| it.0 >= region.addr && it.0 + it.1 <= region.addr + region.size) {
            let at = (addr - region.addr) as usize;
            bytes.write(at, &image.contents[index][at..at + len as usize]);
        }
    }
    let (mut total, mut all_copied) = (0, 0);
    for ((class, bytes), old) in held.iter().zip(&full) {
        let flat = &image.contents[image.index(class)];
        assert_eq!(&*bytes.contiguous(), &flat[..], "{class:?} holds the link's bytes");
        assert!(bytes.slice(0, bytes.len()).is_none(), "{class:?} is in pages");
        let (copied, differing) = (copied(bytes, old), differing(flat, &old.contiguous()));
        assert!(copied <= differing, "{class:?}: {copied} pages copied, {differing} changed");
        assert!((1..=2).contains(&copied), "{class:?}: {copied} pages copied by one function");
        total += bytes.len().div_ceil(CowBytes::PAGE);
        all_copied += copied;
    }
    assert!(total >= 8, "{total} pages of code and data: the image is not large");

    let functions = [splice(k as u32..k as u32 + 1, k as u32..k as u32 + 1)];
    let since = delta(1, &functions, &[], &[], &written);
    let (tables, parts) = image.shared_in(&image.writer, 2, Some(&since), &held);
    assert_eq!(patched(&image), [1, 1, 1], "every table patched");
    let fresh = Elf { output: image.writer.output.clone(), last: None };
    assert_eq!((flat(tables), flat(parts)), image.build(&fresh, 2, None), "a build's tables and parts");
    let (tables, parts) = image.shared_in(&fresh, 3, None, &held);
    assert_eq!((flat(tables), flat(parts)), image.build(&fresh, 3, None), "a build from pages is a build from flat bytes");
    eprintln!("a one-function relink among {N} functions copies {all_copied} of its code and data's {total} pages");
}

/// The architecture `blaze_inproc::image` reads ELF images of: this machine's.
fn host_arch() -> blaze_codegen::Arch {
    if cfg!(target_arch = "aarch64") { blaze_codegen::Arch::Aarch64 } else { blaze_codegen::Arch::X86_64 }
}

/// A dev executable of this machine reserving `pages` pages (arena, cells, fixed), its page
/// and the reservations' sizes.
fn dev_executable(pages: [u64; 3]) -> (Output, u64, [u64; 3]) {
    let arch = host_arch();
    let page = match arch {
        blaze_codegen::Arch::Aarch64 => 0x10000,
        _ => 0x1000,
    };
    let sizes = pages.map(|it| it * page);
    let mut output = output(arch, OutputKind::Executable { entry: "_start".into() }, Some("/lib/ld.so"));
    output.dev_run = Some(DevRun { arena: sizes[0], cells: sizes[1], fixed: sizes[2] });
    (output, page, sizes)
}

/// Each `PT_LOAD` as (flags, offset, vaddr, filesz, memsz), in order.
fn loads(bytes: &[u8]) -> Vec<(u32, u64, u64, u64, u64)> {
    let file = ElfFile64::<Endianness>::parse(bytes).unwrap();
    let endian = file.endian();
    (file.elf_program_headers().iter())
        .filter(|it| it.p_type(endian) == elf::PT_LOAD)
        .map(|it| (it.p_flags(endian).0, it.p_offset(endian), it.p_vaddr(endian), it.p_filesz(endian), it.p_memsz(endian)))
        .collect()
}

/// Every `.dynamic` entry as (tag, value).
fn dynamic_entries(bytes: &[u8]) -> Vec<(u64, u64)> {
    let file = ElfFile64::<Endianness>::parse(bytes).unwrap();
    let endian = file.endian();
    let dynamic = file.elf_program_headers().iter().find_map(|it| it.dynamic(endian, bytes).unwrap()).unwrap();
    dynamic.iter().map(|it| (it.d_tag(endian).0 as u64, it.d_val(endian))).collect()
}

/// The error `Writer::file` gives for the image, its synthetic regions made first.
fn file_error(image: &mut Fixture) -> String {
    for (class, bytes) in image.synthesize().unwrap() {
        let index = image.index(&class);
        image.contents[index] = bytes;
    }
    match image.with(|placed| image.writer.file(placed)) {
        Ok(_) => panic!("the file was made"),
        Err(error) => error.what,
    }
}

/// A dev executable: the start hook's pointer first in `.preinit_array` (an input's entry after
/// it), named by `DT_PREINIT_ARRAY`; its code on pages of its own, all in the file and never
/// writable; its three reservations last, in order, of no access and no file bytes; bound at
/// load. `blaze_inproc::image` reads the same code and reservations from the bytes.
#[test]
fn a_dev_executable_is_laid_out_for_its_start_hook() {
    let (output, page, sizes) = dev_executable([4, 1, 2]);
    let counts = Counts { absolute: 3, functions: 1, function_names: 4, ..Counts::default() };
    let pieces = [
        (Class::Text, 64, 16),
        (Class::Stubs, 32, 16),
        (Class::Rodata, 16, 8),
        (Class::RelRo, 8, 8),
        (Class::PreinitArray, 16, 8),
        (Class::InitArray, 8, 8),
        (Class::Data, 16, 8),
        (Class::Bss, 32, 8),
        (Class::Reserve(Reserve::Fixed), sizes[2], page),
        (Class::Reserve(Reserve::Arena), sizes[0], page),
        (Class::Reserve(Reserve::Cells), sizes[1], page),
    ];
    let mut image = Fixture::new(output, &pieces, &counts);
    // As the core places a reservation: nothing placed, the range its capacity.
    for region in image.regions.iter_mut().filter(|it| matches!(it.class, Class::Reserve(_))) {
        region.size = 0;
    }
    assert_eq!(image.plan.headers, 64 + 56 * 15, "room for the reservations' program headers");
    assert_eq!(image.writer.start_hook(), Ok((Class::PreinitArray, 0)));
    let classes: Vec<&Class> = image.regions.iter().map(|it| &it.class).collect();
    assert_eq!(
        classes[classes.len() - 4..],
        [&Class::Bss, &Class::Reserve(Reserve::Arena), &Class::Reserve(Reserve::Cells), &Class::Reserve(Reserve::Fixed)]
    );
    for class in [Class::Reserve(Reserve::Arena), Class::Reserve(Reserve::Cells), Class::Reserve(Reserve::Fixed)] {
        assert_eq!(image.writer.prot(&class), crate::model::Prot::NONE);
        assert!(!in_file(&class));
    }
    let (text, preinit, init) = (image.at(&Class::Text), image.at(&Class::PreinitArray), image.at(&Class::InitArray));
    image.put(&Class::PreinitArray, 0, &text.to_le_bytes());
    image.put(&Class::PreinitArray, 8, &(text + 16).to_le_bytes());
    image.put(&Class::InitArray, 0, &(text + 32).to_le_bytes());
    image.rebase = vec![preinit, preinit + 8, init];
    image.functions = vec![Function { addr: text, size: 64, fde: None, windows: None, name: Some("main".into()) }];
    image.entry = Some(text);
    let bss_end = image.at(&Class::Bss) + 32;
    let at = |name: &str| match image.writer.special(name) {
        Some(Special::Placed(address)) => address(&image.regions),
        other => panic!("`{name}` is {other:?}"),
    };
    assert_eq!((at("__preinit_array_start"), at("__preinit_array_end")), (preinit, preinit + 16));
    assert_eq!(at("_end"), bss_end, "the reservations are not the image's memory");
    let bytes = image.link();

    // `.preinit_array`, the start hook's pointer first, and `.dynamic`.
    let file = ElfFile64::<Endianness>::parse(&bytes[..]).unwrap();
    let endian = file.endian();
    let sections = file.elf_section_table();
    let (_, shdr) = sections.section_by_name(endian, b".preinit_array").unwrap();
    assert_eq!((shdr.sh_type(endian), shdr.sh_addr(endian), shdr.sh_size(endian)), (elf::SHT_PREINIT_ARRAY, preinit, 16));
    assert_eq!(shdr.sh_entsize(endian), 8);
    assert_eq!(bytes[preinit as usize..][..8], text.to_le_bytes(), "the start hook's pointer is first");
    let entries = dynamic_entries(&bytes);
    let tag = |tag: elf::DynamicTag| entries.iter().find(|it| it.0 == tag.0 as u64).map(|it| it.1);
    assert_eq!((tag(elf::DT_PREINIT_ARRAY), tag(elf::DT_PREINIT_ARRAYSZ)), (Some(preinit), Some(16)));
    assert_eq!(tag(elf::DT_INIT_ARRAY), Some(init));
    assert_ne!(tag(elf::DT_FLAGS).unwrap() & elf::DF_BIND_NOW.0, 0);
    assert_ne!(tag(elf::DT_FLAGS_1).unwrap() & elf::DF_1_NOW.0, 0);
    for lazy in [elf::DT_JMPREL, elf::DT_PLTREL, elf::DT_PLTRELSZ, elf::DT_TEXTREL] {
        assert_eq!(tag(lazy), None, "nothing is bound lazily or written in code: tag {lazy}");
    }
    assert_eq!(tag(elf::DT_FLAGS).unwrap() & elf::DF_TEXTREL.0, 0);

    // The segments.
    let loads = loads(&bytes);
    assert_eq!(loads.iter().map(|it| it.0).collect::<Vec<_>>(), [4, 5, 6, 6, 0, 0, 0]);
    let code: Vec<_> = loads.iter().filter(|it| it.0 & elf::PF_X.0 != 0).collect();
    assert_eq!(code.len(), 1);
    for &&(flags, offset, vaddr, filesz, memsz) in &code {
        assert_eq!(flags & elf::PF_W.0, 0, "code is never writable");
        assert_eq!((offset % page, vaddr % page), (0, 0), "code starts on a page in the file and in memory");
        assert_eq!(offset, vaddr);
        assert_eq!(filesz, memsz, "code is all file bytes");
        let end = (vaddr + memsz).next_multiple_of(page);
        for other in loads.iter().filter(|it| it.2 != vaddr) {
            let other_end = (other.2 + other.4).next_multiple_of(page);
            assert!(other_end <= vaddr || other.2 >= end, "the code at {vaddr:#x} shares a page with {other:x?}");
        }
    }
    let others_end = loads[..4].iter().map(|it| it.2 + it.4).max().unwrap();
    let reserved: Vec<std::ops::Range<u64>> = loads[4..].iter().map(|it| it.2..it.2 + it.4).collect();
    for (nth, (&(flags, _, vaddr, filesz, memsz), reserve)) in
        loads[4..].iter().zip([Reserve::Arena, Reserve::Cells, Reserve::Fixed]).enumerate()
    {
        assert_eq!((flags, filesz, memsz), (0, 0, sizes[nth]), "{reserve:?}");
        assert_eq!(vaddr, image.at(&Class::Reserve(reserve)));
        assert_eq!(vaddr % page, 0);
        assert!(vaddr >= others_end, "{reserve:?} is after every other segment");
    }
    assert!(reserved.is_sorted_by_key(|it| it.start));
    for (name, size) in [&b".blaze.reserve.arena"[..], b".blaze.reserve.cells", b".blaze.reserve.fixed"].into_iter().zip(sizes) {
        let (_, shdr) = sections.section_by_name(endian, name).unwrap();
        assert_eq!((shdr.sh_type(endian), shdr.sh_flags(endian).0, shdr.sh_size(endian)), (elf::SHT_NOBITS, 0, size));
    }

    // What the start hook reads of the same bytes.
    let layout = blaze_inproc::image::layout(blaze_inproc::Format::Elf, &bytes, page).unwrap();
    let &(_, offset, vaddr, _, memsz) = code[0];
    assert_eq!(layout.header, 0);
    assert_eq!(layout.code, [blaze_inproc::Segment { offset, linked: vaddr, len: memsz.next_multiple_of(page) }]);
    assert_eq!(layout.reserved, reserved);
}

/// An image that is not a dev run has the headers, segments and `.preinit_array` it had: an
/// input's `.preinit_array` is relro between `.data.rel.ro` and `.init_array`, and a
/// reservation in it is an error.
#[test]
fn an_image_without_a_dev_run_keeps_its_layout() {
    let writer = Elf { output: executable(), last: None };
    assert_eq!(writer.plan().headers, 64 + 56 * 12);
    let counts = Counts { absolute: 1, ..Counts::default() };
    let pieces = [(Class::Text, 16, 16), (Class::RelRo, 8, 8), (Class::PreinitArray, 8, 8), (Class::InitArray, 8, 8), (Class::Data, 8, 8)];
    let mut image = Fixture::new(executable(), &pieces, &counts);
    let (text, relro, preinit, init) =
        (image.at(&Class::Text), image.at(&Class::RelRo), image.at(&Class::PreinitArray), image.at(&Class::InitArray));
    assert!(relro < preinit && preinit < init);
    assert_eq!(Segment::of(&Class::PreinitArray), Segment::Relro);
    image.put(&Class::PreinitArray, 0, &text.to_le_bytes());
    image.rebase = vec![preinit];
    image.entry = Some(text);
    let bytes = image.link();
    assert_eq!(loads(&bytes).iter().map(|it| it.0).collect::<Vec<_>>(), [4, 5, 6, 6]);
    let entries = dynamic_entries(&bytes);
    let tag = |tag: elf::DynamicTag| entries.iter().find(|it| it.0 == tag.0 as u64).map(|it| it.1);
    assert_eq!((tag(elf::DT_PREINIT_ARRAY), tag(elf::DT_PREINIT_ARRAYSZ)), (Some(preinit), Some(8)));
    let file = ElfFile64::<Endianness>::parse(&bytes[..]).unwrap();
    let endian = file.endian();
    let (_, shdr) = file.elf_section_table().section_by_name(endian, b".preinit_array").unwrap();
    assert_eq!(shdr.sh_type(endian), elf::SHT_PREINIT_ARRAY);
    assert_eq!(shdr.sh_flags(endian), elf::SHF_ALLOC | elf::SHF_WRITE);

    let pieces = [(Class::Text, 16, 16), (Class::Reserve(Reserve::Arena), 0x1000, 0x1000)];
    let mut image = Fixture::new(executable(), &pieces, &Counts::default());
    image.entry = Some(image.at(&Class::Text));
    let error = file_error(&mut image);
    assert!(error.contains("not a dev run"), "{error}");
}

/// A shared library has no start hook: the loader never calls its `.preinit_array`.
#[test]
fn a_shared_library_has_no_start_hook() {
    let kind = OutputKind::Shared { name: "libx.so".into(), exports: Vec::new() };
    let writer = Elf { output: output(blaze_codegen::Arch::X86_64, kind, None), last: None };
    assert!(writer.start_hook().unwrap_err().contains("shared library"));
}

/// A reservation of part of a page is refused, never rounded.
#[test]
fn a_reservation_of_part_of_a_page_is_an_error() {
    let (output, page, _) = dev_executable([1, 0, 0]);
    let pieces = [(Class::Text, 16, 16), (Class::Reserve(Reserve::Arena), page / 2, page)];
    let mut image = Fixture::new(output, &pieces, &Counts::default());
    image.entry = Some(image.at(&Class::Text));
    let error = file_error(&mut image);
    assert!(error.contains("not whole") && error.contains("Arena"), "{error}");
}

/// A dynamic relocation in code is an error naming the symbol it is for and the section: the
/// function holding a slid address, the import of a bind.
#[test]
fn a_text_relocation_names_its_symbol_and_section() {
    let counts = Counts { absolute: 2, imports: 1, import_names: 6, libraries: 1, library_names: 9, ..Counts::default() };
    let mut image = Fixture::new(executable(), &[(Class::Text, 64, 16), (Class::Data, 8, 8)], &counts);
    let text = image.at(&Class::Text);
    image.entry = Some(text);
    image.functions = vec![
        Function { addr: text, size: 32, fde: None, windows: None, name: Some("main".into()) },
        Function { addr: text + 32, size: 32, fde: None, windows: None, name: Some("helper".into()) },
    ];
    image.rebase = vec![text + 40];
    let error = image.synthesize().unwrap_err().what;
    assert!(error.contains("`helper`") && error.contains("`.text`") && error.contains("text relocation"), "{error}");

    image.rebase.clear();
    image.libraries = vec![Library { name: "libc.so.6".into(), as_needed: false }];
    image.imports = vec![import("malloc", Some(0), None, false)];
    image.binds = vec![Bind { addr: text + 8, import: 0, addend: 0 }];
    let error = image.synthesize().unwrap_err().what;
    assert!(error.contains("`malloc`") && error.contains("`.text`") && error.contains("text relocation"), "{error}");
}

#[test]
fn an_executable_with_the_execution_log_starts_on_its_log_stubs_on_both_arches() {
    for (arch, interpreter) in [
        (blaze_codegen::Arch::X86_64, "/lib64/ld-linux-x86-64.so.2"),
        (blaze_codegen::Arch::Aarch64, "/lib/ld-linux-aarch64.so.1"),
    ] {
        let kind = OutputKind::Executable { entry: "_start".into() };
        let logged = crate::table::linked::logged(output(arch, kind, Some(interpreter)));
        assert!(logged.header.is_multiple_of(8), "{arch:?}");
    }
}
