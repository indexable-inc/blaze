use parking_lot::Mutex;

use blaze_bytes::le::{u32 as u32_at, u64 as u64_at};
use blaze_codegen::Arch;
use object::LittleEndian as LE;
use object::Object as _;
use object::pe::{self, ImageNtHeaders64};
use object::read::pe::{ExportTarget, Import as PeImport, ImageNtHeaders as _, ImageOptionalHeader as _, PeFile64};

use blaze_alloc_count::{assert_at_most, measure};
use blaze_format::cursor::Put;

use super::coff::{ImportName, asked_libraries, words};
use super::image::{self, Buf, Image, Tables, alignment, pdb_stem};
use super::{
    DEBUG, GRANULE, IDATA, OBJ_PDATA, PDATA, Pe, RELOC, START_HOOK, TLS_DIR, TLS_INDEX, place, rank, writer,
};
use crate::model::{
    Apply, Bind, Binding, Class, Counts, Data, Delta, Directive, Export, Function, GotEntry, Holds, Import, Library,
    NativeFile, NativeFixup, Placed, Plan, Prot, Region, Reserve, SectionKind, Special, Splice, SymbolDef, SymbolKind,
    Tls, WindowsUnwind, Writer,
};
use crate::output::Debug;
use crate::{DevRun, Format, LinkError, NativeKind, Output, OutputKind};

fn output(kind: OutputKind) -> Output {
    Output {
        format: Format::Pe,
        arch: Arch::X86_64,
        kind,
        interpreter: None,
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
        triple: "x86_64-pc-windows-msvc".into(),
    }
}

fn exe() -> Output {
    output(OutputKind::Executable { entry: "mainCRTStartup".into() })
}

fn dll() -> Output {
    output(OutputKind::Shared { name: "lib.dll".into(), exports: Vec::new() })
}

fn pe(output: Output) -> Pe {
    let last = output.incremental.then(Mutex::default);
    Pe { output, descriptors: Mutex::default(), last }
}

fn pdata_class() -> Class {
    Class::Named { name: OBJ_PDATA.into(), write: false, exec: false }
}

fn import(name: &str, library: u32, version: &str) -> Import {
    Import {
        name: name.into(),
        library: Some(library),
        version: Some(version.into()),
        kind: SymbolKind::Function,
        weak: false,
        wasm: None,
    }
}

fn function(addr: u64, size: u64, info: u64) -> Function {
    Function {
        addr,
        size,
        fde: None,
        windows: Some(WindowsUnwind { info, handler: None, lsda: None }),
        name: None,
    }
}

/// An image placed as the core places one: classes in `Writer::order`, a page boundary at each
/// change of protection, each region 16-aligned, the writer's synthetic regions sized by
/// `Writer::synthetic`.
struct Fixture {
    pe: Pe,
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
    symbols: Vec<(&'static str, u64)>,
}

impl Fixture {
    fn new(output: Output, pieces: Vec<(Class, Data)>, counts: &Counts) -> Fixture {
        let pe = pe(output);
        let plan = pe.plan();
        let mut all = pieces;
        for (class, size) in pe.synthetic(counts) {
            all.push((class, Data::Zero(size)));
        }
        let mut classes: Vec<Class> = all.iter().map(|it| it.0.clone()).collect();
        pe.order(&mut classes);
        let mut regions = Vec::new();
        let mut contents = Vec::new();
        let mut addr = plan.base + plan.headers;
        let mut last = None;
        for class in classes {
            let data = &all.iter().find(|it| it.0 == class).unwrap().1;
            let prot = pe.prot(&class);
            if last != Some(prot) {
                addr = addr.next_multiple_of(plan.page);
                last = Some(prot);
            }
            addr = addr.next_multiple_of(16);
            let size = data.len();
            contents.push(match data.bytes(&[]).unwrap() {
                Some(bytes) => bytes.to_vec(),
                None if super::zeroed(&class) => Vec::new(),
                None => vec![0; size as usize],
            });
            regions.push(Region { class, addr, capacity: size, size, prot, align: 16 });
            addr += size;
        }
        Fixture {
            pe,
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
            symbols: Vec::new(),
        }
    }

    fn at(&self, class: &Class) -> u64 {
        self.regions.iter().find(|it| &it.class == class).unwrap().addr
    }

    fn rva(&self, addr: u64) -> u32 {
        (addr - self.plan.base) as u32
    }

    fn set(&mut self, class: &Class, bytes: Vec<u8>) {
        let index = self.regions.iter().position(|it| &it.class == class).unwrap();
        assert_eq!(bytes.len() as u64, self.regions[index].size);
        self.contents[index] = bytes;
    }

    fn placed<T>(&self, then: impl FnOnce(&Placed<'_>) -> T) -> T {
        self.placed_at(0, None, then)
    }

    /// [`Fixture::placed`] as synthesis `synthesis`, with what changed since the last.
    fn placed_at<T>(&self, synthesis: u64, delta: Option<&Delta<'_>>, then: impl FnOnce(&Placed<'_>) -> T) -> T {
        let contents: Vec<&[u8]> = self.contents.iter().map(Vec::as_slice).collect();
        let symbols = &self.symbols;
        let symbol = |name: &str| symbols.iter().find(|it| it.0 == name).map(|it| it.1);
        then(&Placed {
            output: &self.pe.output,
            name: "app.exe",
            plan: &self.plan,
            regions: &self.regions,
            contents: &contents,
            paged: &[],
            libraries: &self.libraries,
            imports: &self.imports,
            got: &self.got,
            rebase: &self.rebase,
            binds: &self.binds,
            exports: &self.exports,
            functions: &self.functions,
            entry: self.entry,
            init: None,
            fini: None,
            tls: self.tls,
            id: *b"blaze image id!!",
            debug: &[],
            symbol: &symbol,
            synthesis,
            delta,
            encoded: &[],
        })
    }

    /// The file as the core writes it: the synthetic regions filled and sized to their bytes,
    /// every region at its offset, the writer's parts.
    fn image(&mut self) -> Result<Vec<u8>, LinkError> {
        let synthesized = self.placed(|placed| self.pe.synthesize(placed))?;
        for (class, bytes) in synthesized {
            let index = self.regions.iter().position(|it| it.class == class).unwrap();
            assert!(bytes.len() as u64 <= self.regions[index].capacity, "{class:?} outgrew its region");
            self.regions[index].size = bytes.len() as u64;
            self.contents[index] = bytes.contiguous().into_owned();
        }
        self.placed(|placed| {
            let file = self.pe.file(placed)?;
            let mut out = vec![0; file.len as usize];
            for (at, bytes) in &file.parts {
                bytes.copy_to(0, &mut out[*at as usize..][..bytes.len()]);
            }
            for (index, region) in placed.regions.iter().enumerate() {
                if let Some(offset) = self.pe.offset(placed, region) {
                    let bytes = placed.bytes(index);
                    out[offset as usize..][..bytes.len()].copy_from_slice(bytes);
                }
            }
            Ok(out)
        })
    }

    fn fails(&mut self, what: &str) {
        let error = self.image().unwrap_err();
        assert!(error.what.contains(what), "{}", error.what);
    }
}

fn section_names(file: &PeFile64<'_>) -> Vec<String> {
    file.section_table()
        .iter()
        .map(|it| String::from_utf8_lossy(&it.name).trim_end_matches('\0').to_owned())
        .collect()
}

fn directory(file: &PeFile64<'_>, index: usize) -> (u32, u32) {
    file.data_directory(index).map_or((0, 0), |it| (it.virtual_address.get(LE), it.size.get(LE)))
}

/// An executable importing from two DLLs (one import by ordinal), with data, zeroed data,
/// thread-locals, two functions with unwind info and one native `.pdata` entry (and one of a
/// dropped function), and the live descriptor exported.
fn executable() -> Fixture {
    let counts = Counts {
        imports: 3,
        import_names: 60,
        libraries: 2,
        library_names: 22,
        got: 3,
        absolute: 2,
        exports: 1,
        export_names: 12,
        functions: 2,
        unwind: 24,
        ..Counts::default()
    };
    let pieces = vec![
        (Class::Text, Data::Bytes(vec![0xcc; 0x40])),
        (Class::Rodata, Data::Bytes(vec![1; 0x20])),
        (Class::Data, Data::Bytes(vec![0; 0x10])),
        (Class::Got, Data::Bytes(vec![0; 40])),
        (Class::Bss, Data::Zero(0x100)),
        (pdata_class(), Data::Bytes(vec![0; 24])),
        (Class::Tdata, Data::Bytes(vec![7; 8])),
    ];
    let mut f = Fixture::new(exe(), pieces, &counts);
    let (text, rodata, data, got, tdata) =
        (f.at(&Class::Text), f.at(&Class::Rodata), f.at(&Class::Data), f.at(&Class::Got), f.at(&Class::Tdata));
    f.libraries = vec![
        Library { name: "KERNEL32.dll".into(), as_needed: false },
        Library { name: "WS2_32.dll".into(), as_needed: false },
    ];
    f.imports = vec![
        import("ExitProcess", 0, "356 ExitProcess"),
        import("GetStdHandle", 0, "700 GetStdHandle"),
        import("WSAStartup", 1, "#115"),
    ];
    f.got = vec![
        GotEntry { addr: got, holds: Holds::Import(0) },
        GotEntry { addr: got + 8, holds: Holds::Import(1) },
        GotEntry { addr: got + 24, holds: Holds::Import(2) },
    ];
    f.rebase = vec![data, data + 8];
    f.exports = vec![Export { name: "__blaze_live".into(), addr: data, size: 16, kind: SymbolKind::Data }];
    f.functions = vec![function(text, 0x10, rodata), function(text + 0x10, 0x20, rodata + 8)];
    f.entry = Some(text);
    f.tls = Some(Tls { addr: tdata, init: 8, size: 8, align: 8 });
    let mut native = Vec::new();
    for value in [f.rva(text + 0x30), f.rva(text + 0x38), f.rva(rodata + 0x10), 0, 0, 0] {
        native.extend_from_slice(&value.to_le_bytes());
    }
    f.set(&pdata_class(), native);
    f
}

#[test]
fn executable_headers_and_sections() {
    let mut f = executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let header = file.nt_headers().file_header();
    assert_eq!(header.machine.get(LE), pe::IMAGE_FILE_MACHINE_AMD64);
    let characteristics = header.characteristics.get(LE).0;
    assert_eq!(characteristics & pe::IMAGE_FILE_DLL.0, 0);
    assert_ne!(characteristics & pe::IMAGE_FILE_LARGE_ADDRESS_AWARE.0, 0);
    let optional = file.nt_headers().optional_header();
    assert_eq!(optional.image_base(), 0x1_4000_0000);
    assert_eq!(optional.address_of_entry_point(), f.rva(f.at(&Class::Text)));
    assert_eq!(optional.subsystem(), pe::IMAGE_SUBSYSTEM_WINDOWS_CUI);
    let expected = pe::IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA
        | pe::IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE
        | pe::IMAGE_DLLCHARACTERISTICS_NX_COMPAT
        | pe::IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE;
    assert_eq!(optional.dll_characteristics(), expected);
    assert_eq!(optional.size_of_stack_reserve(), 0x10_0000);
    assert_eq!(optional.section_alignment(), 0x1000);
    assert_eq!(optional.size_of_headers(), 0x1000);
    assert_eq!(&image[0x40..0x4e], b"\x0e\x1f\xba\x0e\x00\xb4\x09\xcd\x21\xb8\x01\x4c\xcd\x21");

    assert_eq!(section_names(&file), [".text", ".rdata", ".data", ".pdata", ".tls", ".reloc"]);
    for section in file.section_table().iter() {
        let va = section.virtual_address.get(LE);
        assert_eq!(va % 0x1000, 0);
        if section.size_of_raw_data.get(LE) > 0 {
            assert_eq!(section.pointer_to_raw_data.get(LE), va, "a section's file offset is its RVA");
        }
    }
    let flags: Vec<u32> = file.section_table().iter().map(|it| it.characteristics.get(LE).0).collect();
    assert_ne!(flags[0] & pe::IMAGE_SCN_MEM_EXECUTE.0, 0);
    assert_eq!(flags[1] & (pe::IMAGE_SCN_MEM_WRITE.0 | pe::IMAGE_SCN_MEM_EXECUTE.0), 0);
    assert_ne!(flags[2] & pe::IMAGE_SCN_MEM_WRITE.0, 0);
    assert_ne!(flags[5] & pe::IMAGE_SCN_MEM_DISCARDABLE.0, 0);
    let last = file.section_table().iter().last().unwrap();
    let end = last.virtual_address.get(LE) + last.virtual_size.get(LE);
    assert_eq!(optional.size_of_image(), end.next_multiple_of(0x1000));
}

#[test]
fn import_directory_names_each_dll_and_function() {
    let mut f = executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let table = file.import_table().unwrap().unwrap();
    let mut descriptors = table.descriptors().unwrap();
    let mut seen = Vec::new();
    while let Some(descriptor) = descriptors.next().unwrap() {
        let dll = String::from_utf8_lossy(table.name(descriptor.name.get(LE)).unwrap()).into_owned();
        let mut thunks = table.thunks(descriptor.original_first_thunk.get(LE)).unwrap();
        let mut imports = Vec::new();
        while let Some(thunk) = thunks.next::<ImageNtHeaders64>().unwrap() {
            imports.push(match table.import::<ImageNtHeaders64>(thunk).unwrap() {
                PeImport::Name(hint, name) => format!("{hint} {}", String::from_utf8_lossy(name)),
                PeImport::Ordinal(ordinal) => format!("#{ordinal}"),
            });
        }
        seen.push((dll, descriptor.first_thunk.get(LE), imports));
    }
    let got = f.rva(f.at(&Class::Got));
    assert_eq!(
        seen,
        [
            ("KERNEL32.dll".to_owned(), got, vec!["356 ExitProcess".to_owned(), "700 GetStdHandle".to_owned()]),
            ("WS2_32.dll".to_owned(), got + 24, vec!["#115".to_owned()]),
        ]
    );
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_IAT), (got, 40));
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_IMPORT), (f.rva(f.at(&Class::Synthetic(IDATA))), 60));
}

#[test]
fn base_relocations_cover_every_rebase_and_the_tls_directory() {
    let mut f = executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let mut blocks = file.data_directories().relocation_blocks(&image[..], &file.section_table()).unwrap().unwrap();
    let mut seen = Vec::new();
    while let Some(block) = blocks.next().unwrap() {
        assert_eq!(block.virtual_address() % 0x1000, 0);
        assert_eq!(block.size() % 4, 0);
        for relocation in block {
            assert_eq!(relocation.typ, pe::IMAGE_REL_BASED_DIR64);
            seen.push(relocation.virtual_address);
        }
    }
    let data = f.rva(f.at(&Class::Data));
    let tls = f.rva(f.at(&Class::Synthetic(TLS_DIR)));
    let mut expected = vec![data, data + 8, tls, tls + 8, tls + 16, tls + 24];
    expected.sort_unstable();
    assert_eq!(seen, expected);
}

#[test]
fn pdata_merges_functions_and_native_entries_sorted() {
    let mut f = executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let (at, size) = directory(&file, pe::IMAGE_DIRECTORY_ENTRY_EXCEPTION);
    assert_eq!(at, f.rva(f.at(&Class::Synthetic(PDATA))));
    let bytes = &image[at as usize..][..size as usize];
    let entries: Vec<(u32, u32, u32)> = bytes
        .chunks_exact(12)
        .map(|it| (u32_at(it, 0), u32_at(it, 4), u32_at(it, 8)))
        .collect();
    let text = f.rva(f.at(&Class::Text));
    let rodata = f.rva(f.at(&Class::Rodata));
    assert_eq!(
        entries,
        [(text, text + 0x10, rodata), (text + 0x10, text + 0x30, rodata + 8), (text + 0x30, text + 0x38, rodata + 0x10)]
    );
}

#[test]
fn debug_directory_has_rsds() {
    let mut f = executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let codeview = file.pdb_info().unwrap().unwrap();
    assert_eq!(&codeview.guid(), b"blaze image id!!");
    assert_eq!(codeview.age(), 1);
    assert_eq!(codeview.path(), b"app.pdb");
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_DEBUG), (f.rva(f.at(&Class::Synthetic(DEBUG))), 28));
}

#[test]
fn own_tls_directory_when_the_crt_has_none() {
    let mut f = executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let dir = f.at(&Class::Synthetic(TLS_DIR));
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_TLS), (f.rva(dir), 40));
    let bytes = &image[f.rva(dir) as usize..][..48];
    let tdata = f.at(&Class::Tdata);
    assert_eq!(u64_at(bytes, 0), tdata);
    assert_eq!(u64_at(bytes, 8), tdata + 8);
    assert_eq!(u64_at(bytes, 16), f.at(&Class::Synthetic(TLS_INDEX)));
    assert_eq!(u64_at(bytes, 24), dir + 40);
    assert_eq!(u32_at(bytes, 32), 0);
    assert_eq!(u32_at(bytes, 36), pe::IMAGE_SCN_ALIGN_8BYTES.0);
    assert_eq!(u64_at(bytes, 40), 0, "the callback list is empty");
}

#[test]
fn executable_exports_the_live_descriptor() {
    let mut f = executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let exports = file.export_table().unwrap().unwrap().exports().unwrap();
    assert_eq!(exports.len(), 1);
    assert_eq!(exports[0].name, Some(&b"__blaze_live"[..]));
    assert!(matches!(exports[0].target, ExportTarget::Address(at) if at == f.rva(f.at(&Class::Data))));
}

#[test]
fn dll_exports_sorted_by_name() {
    let counts = Counts { exports: 3, export_names: 14, ..Counts::default() };
    let pieces = vec![(Class::Text, Data::Bytes(vec![0xc3; 0x30])), (Class::Data, Data::Bytes(vec![0; 0x10]))];
    let mut f = Fixture::new(dll(), pieces, &counts);
    let text = f.at(&Class::Text);
    f.exports = ["zeta", "alpha", "mid"]
        .iter()
        .enumerate()
        .map(|(at, name)| Export { name: (*name).into(), addr: text + 0x10 * at as u64, size: 0x10, kind: SymbolKind::Function })
        .collect();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let header = file.nt_headers().file_header();
    assert_ne!(header.characteristics.get(LE).0 & pe::IMAGE_FILE_DLL.0, 0);
    let optional = file.nt_headers().optional_header();
    assert_eq!(optional.image_base(), 0x1_8000_0000);
    assert_eq!(optional.address_of_entry_point(), 0);
    assert_eq!(optional.dll_characteristics().0 & pe::IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE.0, 0);
    assert_eq!(section_names(&file), [".text", ".rdata", ".data", ".reloc"]);
    assert!(file.import_table().unwrap().is_none());

    let table = file.export_table().unwrap().unwrap();
    assert_eq!(table.ordinal_base(), 1);
    let names: Vec<(String, u32)> = table
        .exports()
        .unwrap()
        .into_iter()
        .map(|it| {
            let ExportTarget::Address(at) = it.target else { panic!("a forwarder") };
            (String::from_utf8_lossy(it.name.unwrap()).into_owned(), at)
        })
        .collect();
    let rva = f.rva(text);
    assert_eq!(
        names,
        [("alpha".to_owned(), rva + 0x10), ("mid".to_owned(), rva + 0x20), ("zeta".to_owned(), rva)]
    );
    let sorted: Vec<u32> = table.name_pointers().iter().map(|it| it.get(LE)).collect();
    let mut by_name = Vec::new();
    for pointer in sorted {
        by_name.push(String::from_utf8_lossy(table.name_from_pointer(pointer).unwrap()).into_owned());
    }
    assert_eq!(by_name, ["alpha", "mid", "zeta"]);
    let directory = table.directory();
    let name = table.name_from_pointer(directory.name.get(LE)).unwrap();
    assert_eq!(name, b"app.exe");
}

#[test]
fn reloc_after_pdata_without_a_page_joins_it() {
    let counts = Counts { functions: 1, absolute: 1, ..Counts::default() };
    let pieces = vec![
        (Class::Text, Data::Bytes(vec![0xc3; 0x10])),
        (Class::Rodata, Data::Bytes(vec![1; 8])),
        (Class::Data, Data::Bytes(vec![0; 8])),
    ];
    let mut f = Fixture::new(exe(), pieces, &counts);
    let (text, rodata, data) = (f.at(&Class::Text), f.at(&Class::Rodata), f.at(&Class::Data));
    f.functions = vec![function(text, 0x10, rodata)];
    f.rebase = vec![data];
    f.entry = Some(text);
    assert_ne!(f.at(&Class::Synthetic(RELOC)) % 0x1000, 0);
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    assert_eq!(section_names(&file), [".text", ".rdata", ".data", ".pdata"]);
    let pdata = file.section_table().iter().nth(3).unwrap();
    let (at, size) = directory(&file, pe::IMAGE_DIRECTORY_ENTRY_BASERELOC);
    assert_eq!(at, f.rva(f.at(&Class::Synthetic(RELOC))));
    assert!(at + size <= pdata.virtual_address.get(LE) + pdata.virtual_size.get(LE));
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_TLS), (0, 0));
}

#[test]
fn crt_tls_directory_is_used_when_defined() {
    let counts = Counts::default();
    let pieces = vec![
        (Class::Text, Data::Bytes(vec![0xc3; 0x10])),
        (Class::Rodata, Data::Bytes(vec![0; 40])),
        (Class::Tdata, Data::Bytes(vec![5; 16])),
    ];
    let crt = |start: u64, end: u64| {
        let mut bytes = Buf::default();
        for value in [start, end, 0, 0] {
            bytes.put_u64(value);
        }
        bytes.put_u32(0);
        bytes.put_u32(0);
        bytes.0
    };
    let mut f = Fixture::new(exe(), pieces, &counts);
    let (text, rodata, tdata) = (f.at(&Class::Text), f.at(&Class::Rodata), f.at(&Class::Tdata));
    f.entry = Some(text);
    f.tls = Some(Tls { addr: tdata, init: 16, size: 16, align: 8 });
    f.symbols = vec![("_tls_used", rodata)];
    f.set(&Class::Rodata, crt(tdata, tdata + 16));
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_TLS), (f.rva(rodata), 40));
    let ours = f.rva(f.at(&Class::Synthetic(TLS_DIR))) as usize;
    assert!(image[ours..ours + 48].iter().all(|it| *it == 0));

    f.set(&Class::Rodata, crt(tdata + 8, tdata + 16));
    f.fails("_tls_used");
    f.set(&Class::Rodata, crt(tdata, tdata + 24));
    f.fails("_tls_used");
}

/// As tlssup.obj lays it out: `_tls_start` in `.tls`, the thread-locals, then `_tls_end`, a byte
/// in `.tls$ZZZ` past the range; and an incremental image's room after it.
#[test]
fn crt_tls_range_ends_before_its_end_marker() {
    let pieces = vec![
        (Class::Text, Data::Bytes(vec![0xc3; 0x10])),
        (Class::Rodata, Data::Bytes(vec![0; 40])),
        (Class::Tdata, Data::Bytes(vec![5; 17])),
    ];
    let mut f = Fixture::new(exe(), pieces, &Counts::default());
    let (text, rodata, tdata) = (f.at(&Class::Text), f.at(&Class::Rodata), f.at(&Class::Tdata));
    f.entry = Some(text);
    f.tls = Some(Tls { addr: tdata, init: 17, size: 17 + 0x1_0000, align: 8 });
    f.symbols = vec![("_tls_used", rodata)];
    let mut used = Buf::default();
    for value in [tdata, tdata + 16, 0, 0] {
        used.put_u64(value);
    }
    used.put_u32(0);
    used.put_u32(0);
    f.set(&Class::Rodata, used.0);
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_TLS), (f.rva(rodata), 40));
}

#[test]
fn load_config_directory_is_sized_by_its_first_word() {
    let pieces = vec![(Class::Text, Data::Bytes(vec![0xc3; 0x10])), (Class::Rodata, Data::Bytes(vec![0; 0x140]))];
    let mut f = Fixture::new(exe(), pieces, &Counts::default());
    let (text, rodata) = (f.at(&Class::Text), f.at(&Class::Rodata));
    f.entry = Some(text);
    f.symbols = vec![("_load_config_used", rodata)];
    let mut config = vec![0; 0x140];
    config[..4].copy_from_slice(&0x140u32.to_le_bytes());
    f.set(&Class::Rodata, config);
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG), (f.rva(rodata), 0x140));
}

#[test]
fn what_pe_cannot_say_is_an_error() {
    let mut f = executable();
    f.binds = vec![Bind { addr: f.at(&Class::Data), import: 0, addend: 0 }];
    f.fails("ExitProcess");

    let mut f = executable();
    f.imports[2].library = None;
    f.fails("imported from no DLL");

    let mut f = executable();
    let got = f.at(&Class::Got);
    f.got[1].addr = got + 16;
    f.fails("not consecutive");

    let mut f = executable();
    f.got.push(GotEntry { addr: f.at(&Class::Got) + 16, holds: Holds::Local(0) });
    f.fails("no zero cell");

    let mut f = executable();
    f.got.push(GotEntry { addr: f.at(&Class::Got) + 32, holds: Holds::TlsGd { import: None, offset: 0 } });
    f.fails("thread-locals");

    let mut f = executable();
    f.entry = None;
    f.fails("mainCRTStartup");

    let mut f = executable();
    f.rebase.push(f.at(&Class::Bss));
    f.fails("not in the file");

    let mut f = executable();
    let text = f.at(&Class::Text);
    f.functions.push(function(text + 8, 0x10, text));
    f.fails("overlap");

    let mut f = executable();
    let first = &f.exports[0];
    let twice = Export { name: first.name.clone(), addr: first.addr, size: first.size, kind: first.kind };
    f.exports.push(twice);
    f.fails("exported twice");
}

#[test]
fn writer_is_x86_64_pe_only() {
    let mut arm = exe();
    arm.arch = Arch::Aarch64;
    let error = writer(&arm).err().unwrap();
    assert!(error.what.contains("Aarch64"), "{}", error.what);
    let writer = writer(&exe()).unwrap();
    assert_eq!(writer.format(), Format::Pe);
    let plan = writer.plan();
    assert_eq!((plan.base, plan.page, plan.headers), (0x1_4000_0000, 0x1000, 0x1000));
    assert!(plan.got_by_library && !plan.tls_descriptors && !plan.indexed_code);
    assert_eq!(super::writer(&dll()).unwrap().plan().base, 0x1_8000_0000);
}

#[test]
fn grouped_sections_sort_by_suffix() {
    let pe = pe(exe());
    let section = |name: &str| pe.section(name, false, false, false).unwrap();
    let crt = |rank| (Class::Named { name: ".CRT".into(), write: false, exec: false }, rank);
    let (xca, xcu, xcz) = (section(".CRT$XCA"), section(".CRT$XCU"), section(".CRT$XCZ"));
    assert_eq!(xca, crt(rank(Some("XCA"))));
    assert!(xca.1 < xcu.1 && xcu.1 < xcz.1);
    assert!(section(".CRT$XLA").1 < section(".CRT$XLB").1);
    assert!(section(".CRT$XCZ").1 < section(".CRT$XIA").1);
    let (tls, tls_, tls_end) = (section(".tls"), section(".tls$"), section(".tls$ZZZ"));
    assert_eq!((tls.0.clone(), tls_.0.clone(), tls_end.0.clone()), (Class::Tdata, Class::Tdata, Class::Tdata));
    assert!(tls.1 < tls_.1 && tls_.1 < tls_end.1);
    assert_eq!(tls_.1, 1);
    assert_eq!(pe.own(&Class::Tbss), (Class::Tdata, 1));
    assert_eq!(pe.own(&Class::Text), (Class::Text, u32::MAX));

    let (a, b, c) = (section(".lme$a"), section(".lme$b"), section(".lme$c"));
    assert_eq!(a.0, Class::Named { name: ".lme".into(), write: false, exec: false });
    assert!(a.1 < b.1 && b.1 < c.1);
    let items = pe.section("linkme_ITEMS$b", true, false, false).unwrap();
    assert_eq!(items, (Class::Named { name: "linkme_ITEMS".into(), write: true, exec: false }, rank(Some("b"))));

    assert_eq!(section(".text$mn"), (Class::Text, rank(Some("mn"))));
    assert_eq!(section(".rdata$r"), (Class::Rodata, rank(Some("r"))));
    assert_eq!(section(".xdata").0, Class::Rodata);
    assert_eq!(section(".data").0, Class::Data);
    assert_eq!(section(".bss"), (Class::Bss, 0));
    assert_eq!(pe.section(".mytls", true, false, true).unwrap().0, Class::Tdata);
    assert!(pe.section(".idata$5", false, false, false).is_err());
    assert!(pe.section(".reloc", false, false, false).is_err());

    assert_eq!(rank(None), 0);
    assert_eq!(rank(Some("")), 1);
    assert_eq!(rank(Some("\0")), 2);
    assert_eq!(rank(Some("XCU")), u32::from_be_bytes(*b"XCU\0"));
}

#[test]
fn classes_in_link_exe_order() {
    let pe = pe(exe());
    let mut classes = vec![
        Class::Synthetic(RELOC),
        Class::Tdata,
        Class::Bss,
        pdata_class(),
        Class::Got,
        Class::Synthetic(IDATA),
        Class::RelRo,
        Class::Plt,
        Class::Data,
        Class::Rodata,
        Class::Text,
        Class::Synthetic(PDATA),
        Class::Named { name: ".CRT".into(), write: false, exec: false },
    ];
    pe.order(&mut classes);
    assert_eq!(
        classes,
        [
            Class::Text,
            Class::Plt,
            Class::Rodata,
            Class::RelRo,
            Class::Synthetic(IDATA),
            Class::Named { name: ".CRT".into(), write: false, exec: false },
            Class::Data,
            Class::Got,
            Class::Bss,
            pdata_class(),
            Class::Synthetic(PDATA),
            Class::Tdata,
            Class::Synthetic(RELOC),
        ]
    );
    assert_eq!(pe.prot(&Class::Text), crate::model::Prot::RX);
    assert_eq!(pe.prot(&Class::RelRo), crate::model::Prot::R);
    assert_eq!(pe.prot(&Class::Got), crate::model::Prot::RW);
    assert_eq!(pe.prot(&Class::Tdata), crate::model::Prot::RW);
    assert_eq!(pe.prot(&pdata_class()), crate::model::Prot::R);
}

#[test]
fn specials() {
    let pe = pe(exe());
    assert_eq!(pe.special("__ImageBase"), Some(Special::ImageBase));
    assert_eq!(pe.special("_tls_used"), Some(Special::Start(Class::Synthetic(TLS_DIR))));
    assert_eq!(pe.special("_tls_index"), Some(Special::Start(Class::Synthetic(TLS_INDEX))));
    assert_eq!(pe.special("__blaze_live"), Some(Special::LiveDescriptor));
    assert_eq!(pe.special("__guard_flags"), Some(Special::Absolute(0)));
    assert_eq!(pe.special("__guard_fids_table"), Some(Special::Absolute(0)));
    let got = pe.special("__imp_ExitProcess");
    assert_eq!(got, Some(Special::Got { from: 6 }));
    assert_eq!(got.as_ref().and_then(|it| it.got_target("__imp_ExitProcess")), Some("ExitProcess"));
    assert_eq!(pe.special("__imp_"), None);
    assert_eq!(pe.special("main"), None);
    assert_eq!(pe.shared_entry(), None);
    assert_eq!(self::pe(dll()).shared_entry(), Some("_DllMainCRTStartup"));
}

#[test]
fn encoders() {
    assert_eq!(alignment(1), Ok(pe::IMAGE_SCN_ALIGN_1BYTES.0));
    assert_eq!(alignment(0), Ok(pe::IMAGE_SCN_ALIGN_1BYTES.0));
    assert_eq!(alignment(16), Ok(pe::IMAGE_SCN_ALIGN_16BYTES.0));
    assert!(alignment(3).is_err());
    assert!(alignment(16384).is_err());

    assert_eq!(pdb_stem("app.exe"), "app");
    assert_eq!(pdb_stem("lib.v2.dll"), "lib.v2");
    assert_eq!(pdb_stem("tool"), "tool");
    assert_eq!(pdb_stem(".hidden"), ".hidden");

    let all_words = |text: &str| {
        let (mut seen, mut buf) = (Vec::new(), String::new());
        words(text, &mut buf, |word| {
            seen.push(word.to_owned());
            Ok::<(), ()>(())
        })
        .unwrap();
        seen
    };
    assert_eq!(all_words(r#" /A "/B:x y"  /C:"q r"  "#), ["/A", "/B:x y", "/C:q r"]);
    assert_eq!(all_words("\0/X\0"), ["/X"]);
    assert_eq!(all_words(r#"/"a" b"#), ["/a", "b"]);

    assert_eq!(ImportName::Name { hint: 356, name: "ExitProcess" }.version(), "356 ExitProcess");
    assert_eq!(ImportName::Name { hint: 0, name: "f" }.version(), "0 f");
    assert_eq!(ImportName::Ordinal(65535).version(), "#65535");
    for import in [ImportName::Name { hint: 7, name: "g" }, ImportName::Ordinal(115)] {
        let version = import.version();
        assert_eq!(version.capacity(), version.len(), "one allocation of the exact size");
        assert_eq!(ImportName::parse("g", Some(&version)), Ok(import));
    }

    assert_eq!(ImportName::parse("f", Some("12 f")), Ok(ImportName::Name { hint: 12, name: "f".into() }));
    assert_eq!(ImportName::parse("f", Some("3 g")), Ok(ImportName::Name { hint: 3, name: "g".into() }));
    assert_eq!(ImportName::parse("f", Some("#7")), Ok(ImportName::Ordinal(7)));
    assert_eq!(ImportName::parse("f", None), Ok(ImportName::Name { hint: 0, name: "f".into() }));
    assert!(ImportName::parse("f", Some("f")).is_err());
    assert!(ImportName::parse("f", Some("#x")).is_err());

    let mut buf = Buf::default();
    buf.put_u16(0x0102);
    buf.string("ab");
    buf.pad_to(8);
    buf.put_u32(7);
    assert_eq!(buf.0, [2, 1, b'a', b'b', 0, 0, 0, 0, 7, 0, 0, 0]);
}

#[test]
fn synthetic_sizes() {
    let classes =
        |counts: &Counts| -> Vec<Class> { image::synthetic(counts, false).into_iter().map(|it| it.0).collect() };
    assert_eq!(
        classes(&Counts::default()),
        [Class::Synthetic(DEBUG), Class::Synthetic(TLS_DIR), Class::Synthetic(TLS_INDEX), Class::Synthetic(RELOC)]
    );
    let counts = Counts { libraries: 1, imports: 1, import_names: 10, library_names: 12, ..Counts::default() };
    let sizes = image::synthetic(&counts, false);
    let idata = sizes.iter().find(|it| it.0 == Class::Synthetic(IDATA)).unwrap().1;
    // Two descriptors, a lookup table of 2, a hint/name entry, the DLL name.
    assert_eq!(idata, 40 + 16 + 14 + 13);
}

#[test]
fn base_relocation_blocks_encode() {
    let pieces = vec![(Class::Text, Data::Bytes(vec![0xc3; 0x10])), (Class::Data, Data::Bytes(vec![0; 0x2000]))];
    let mut f = Fixture::new(exe(), pieces, &Counts { absolute: 3, image: 0x2000, ..Counts::default() });
    let data = f.at(&Class::Data);
    assert_eq!(data % 0x1000, 0);
    f.rebase = vec![data + 0x1008, data, data + 0x10, data];
    let tables = f.placed(|placed| Tables::of(&f.pe, placed)).unwrap();
    let reloc = &tables.regions.iter().find(|it| it.0 == Class::Synthetic(RELOC)).unwrap().1;
    let page = f.rva(data);
    let mut expected = Buf::default();
    expected.put_u32(page);
    expected.put_u32(12);
    expected.put_u16(0xa000);
    expected.put_u16(0xa010);
    expected.put_u32(page + 0x1000);
    expected.put_u32(12);
    expected.put_u16(0xa008);
    expected.put_u16(0);
    assert_eq!(&*reloc.contiguous(), &expected.0[..]);
}

#[test]
fn base_relocations_are_the_same_bytes_in_any_number_of_pieces() {
    let pieces = vec![(Class::Text, Data::Bytes(vec![0xc3; 0x10])), (Class::Data, Data::Bytes(vec![0; 0x40000]))];
    let mut f = Fixture::new(exe(), pieces, &Counts::default());
    let data = f.at(&Class::Data);
    // Dense pages, sparse pages, duplicates, and an odd count per page (a pad entry), unsorted.
    let mut rebase: Vec<u64> = (0..0x2000).map(|it| data + it * 8).collect();
    rebase.extend((0..20).map(|it| data + 0x20000 + it * 0x1800));
    rebase.extend([data + 0x3f000, data + 0x3f000, data + 0x10, data + 0x3fff8]);
    rebase.reverse();
    f.rebase = rebase;
    let extra = [data + 0x3f008, data + 0x18, data + 0x25000];
    let whole = f.placed(|placed| image::relocations(Image::of(placed), &extra, usize::MAX)).unwrap();
    for piece in [1, 2, 7, 512, 4096] {
        let split = f.placed(|placed| image::relocations(Image::of(placed), &extra, piece)).unwrap();
        assert_eq!(split, whole, "pieces of {piece}");
    }
    let threads = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    let one = threads.install(|| f.placed(|placed| image::relocations(Image::of(placed), &extra, 7))).unwrap();
    assert_eq!(one, whole);

    let mut blocks = 0;
    let mut at = 0;
    while at < whole.len() {
        let size = u32_at(&whole, at + 4) as usize;
        assert!(size >= 8 && size.is_multiple_of(4));
        assert_eq!(u32_at(&whole, at) % 0x1000, 0);
        at += size;
        blocks += 1;
    }
    assert_eq!(at, whole.len());
    // The 16 pages of the dense run, the 20 sparse ones, 0x25000 and 0x3f000.
    assert_eq!(blocks, 16 + 20 + 1 + 1);
}

/// Each synthetic table's bytes, in the order `Tables::of` gives them.
fn tables(pe: &Pe, f: &Fixture, synthesis: u64, delta: Option<&Delta<'_>>) -> Vec<(Class, Vec<u8>)> {
    let tables = f.placed_at(synthesis, delta, |placed| Tables::of(pe, placed)).unwrap();
    tables.regions.into_iter().map(|(class, bytes)| (class, bytes.contiguous().into_owned())).collect()
}

fn table(pe: &Pe, f: &Fixture, synthesis: u64, delta: Option<&Delta<'_>>, name: &'static str) -> Vec<u8> {
    let class = Class::Synthetic(name);
    tables(pe, f, synthesis, delta).into_iter().find(|it| it.0 == class).unwrap().1
}

/// What changed since synthesis `from`: no region moved, the same names, no GOT cell or bind.
fn delta<'a>(functions: &'a [Splice], rebase: &'a [Splice], from: u64) -> Delta<'a> {
    Delta { functions, rebase, got: &[], binds: &[], same_names: true, same_regions: true, written: &[], from }
}

fn splice(at: u32) -> Splice {
    Splice { old: at..at + 1, new: at..at + 1 }
}

fn incremental() -> Pe {
    pe(Output { incremental: true, ..exe() })
}

#[test]
fn incremental_tables_are_patched_to_a_builds_bytes() {
    let mut f = executable();
    let (built, kept) = (pe(exe()), incremental());
    assert_eq!(tables(&kept, &f, 1, None), tables(&built, &f, 0, None));

    // A function shrinks, a rebased address moves within its page: each table patched or reused.
    let data = f.at(&Class::Data);
    f.functions[1].size = 0x18;
    f.rebase[1] = data + 4;
    let (functions, rebase) = ([splice(1)], [splice(1)]);
    assert_eq!(tables(&kept, &f, 2, Some(&delta(&functions, &rebase, 1))), tables(&built, &f, 0, None));
    let counts = |pe: &Pe| {
        let last = pe.last().unwrap();
        (last.pdata.patched, last.reloc.patched, last.idata.reused, last.edata.reused)
    };
    assert_eq!(counts(&kept), (1, 1, 1, 1));

    // A function moved past the others' entries, a rebased address onto the one before: built.
    let (text, rodata) = (f.at(&Class::Text), f.at(&Class::Rodata));
    f.functions[0] = function(text + 0x38, 8, rodata);
    f.rebase[1] = f.rebase[0];
    let (functions, rebase) = ([splice(0)], [splice(1)]);
    assert_eq!(tables(&kept, &f, 3, Some(&delta(&functions, &rebase, 2))), tables(&built, &f, 0, None));
    assert_eq!(counts(&kept), (1, 1, 2, 2));

    // Changes from a synthesis the tables are not of: every table built.
    assert_eq!(tables(&kept, &f, 4, Some(&delta(&[], &[], 9))), tables(&built, &f, 0, None));
    assert_eq!(counts(&kept), (1, 1, 2, 2));
}

#[test]
fn pdata_in_pieces_is_the_sorted_merge() {
    let (n, natives) = (3 * 4096 + 77, 1000u64);
    let counts = Counts { functions: n, unwind: natives * 12, ..Counts::default() };
    let pieces = vec![
        (Class::Text, Data::Bytes(vec![0xcc; n as usize * 0x10])),
        (Class::Rodata, Data::Bytes(vec![1; 0x10])),
        (pdata_class(), Data::Bytes(vec![0; natives as usize * 12])),
    ];
    let mut f = Fixture::new(exe(), pieces, &counts);
    let (text, rodata) = (f.at(&Class::Text), f.at(&Class::Rodata));
    f.functions = (0..n).map(|it| function(text + it * 0x10, 8, rodata)).collect();
    for function in f.functions.iter_mut().step_by(3) {
        function.windows = None;
    }
    // The objects' entries in the functions' gaps, last first.
    let mut objects = Vec::new();
    let mut bytes = Vec::new();
    for it in (0..natives).rev() {
        let at = text + it * 0xc0 + 8;
        let entry = (f.rva(at), f.rva(at + 8), f.rva(rodata + 8));
        for value in [entry.0, entry.1, entry.2] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        objects.push(entry);
    }
    f.set(&pdata_class(), bytes);
    let expected = |f: &Fixture| {
        let mut entries = objects.clone();
        entries.extend(
            (f.functions.iter().filter(|it| it.windows.is_some()))
                .map(|it| (f.rva(it.addr), f.rva(it.addr + it.size), f.rva(rodata))),
        );
        entries.sort_unstable();
        entries.iter().flat_map(|&(begin, end, info)| [begin, end, info]).flat_map(u32::to_le_bytes).collect::<Vec<u8>>()
    };
    let built = pe(exe());
    let whole = table(&built, &f, 0, None, PDATA);
    assert_eq!(whole, expected(&f));
    let threads = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    assert_eq!(threads.install(|| table(&built, &f, 0, None, PDATA)), whole);

    let kept = incremental();
    assert_eq!(table(&kept, &f, 1, None, PDATA), whole);
    f.functions[5000].size = 4;
    let functions = [splice(5000)];
    assert_eq!(table(&kept, &f, 2, Some(&delta(&functions, &[], 1)), PDATA), expected(&f));
    assert_eq!(kept.last().unwrap().pdata.patched, 1);

    // Functions out of order: sorted and merged on one thread, the same bytes.
    f.functions.swap(10, 9001);
    assert_eq!(table(&built, &f, 0, None, PDATA), expected(&f));
}

#[test]
fn incremental_base_relocations_are_a_builds_bytes() {
    let pieces = vec![(Class::Text, Data::Bytes(vec![0xc3; 0x10])), (Class::Data, Data::Bytes(vec![0; 0x40000]))];
    let mut f = Fixture::new(exe(), pieces, &Counts { absolute: 0x4000, image: 0x40000, ..Counts::default() });
    let data = f.at(&Class::Data);
    f.rebase = (0..0x4000).map(|it| data + it * 16).collect();
    let (built, kept) = (pe(exe()), incremental());
    assert_eq!(table(&kept, &f, 1, None, RELOC), table(&built, &f, 0, None, RELOC));

    f.rebase[100] += 8;
    f.rebase[9000] += 4;
    let rebase = [splice(100), splice(9000)];
    assert_eq!(table(&kept, &f, 2, Some(&delta(&[], &rebase, 1)), RELOC), table(&built, &f, 0, None, RELOC));
    assert_eq!(kept.last().unwrap().reloc.patched, 1);

    // Onto the address after it: a build has one entry for the two.
    f.rebase[101] = f.rebase[102];
    let rebase = [splice(101)];
    assert_eq!(table(&kept, &f, 3, Some(&delta(&[], &rebase, 2)), RELOC), table(&built, &f, 0, None, RELOC));
    // And back: the address after it needs its entry again.
    f.rebase[101] = data + 101 * 16;
    assert_eq!(table(&kept, &f, 4, Some(&delta(&[], &rebase, 3)), RELOC), table(&built, &f, 0, None, RELOC));
    // The first of a page into the page before, in order still: its block is another.
    f.rebase[256] = data + 0xff8;
    let rebase = [splice(256)];
    assert_eq!(table(&kept, &f, 5, Some(&delta(&[], &rebase, 4)), RELOC), table(&built, &f, 0, None, RELOC));
    assert_eq!(kept.last().unwrap().reloc.patched, 1);
}

// Inputs.

const TEXT: u32 = pe::IMAGE_SCN_CNT_CODE.0 | pe::IMAGE_SCN_MEM_EXECUTE.0 | pe::IMAGE_SCN_MEM_READ.0 | pe::IMAGE_SCN_ALIGN_16BYTES.0;
const DATA: u32 =
    pe::IMAGE_SCN_CNT_INITIALIZED_DATA.0 | pe::IMAGE_SCN_MEM_READ.0 | pe::IMAGE_SCN_MEM_WRITE.0 | pe::IMAGE_SCN_ALIGN_8BYTES.0;
const RDATA: u32 = pe::IMAGE_SCN_CNT_INITIALIZED_DATA.0 | pe::IMAGE_SCN_MEM_READ.0 | pe::IMAGE_SCN_ALIGN_4BYTES.0;
const BSS: u32 =
    pe::IMAGE_SCN_CNT_UNINITIALIZED_DATA.0 | pe::IMAGE_SCN_MEM_READ.0 | pe::IMAGE_SCN_MEM_WRITE.0 | pe::IMAGE_SCN_ALIGN_8BYTES.0;
const INFO: u32 = pe::IMAGE_SCN_LNK_INFO.0 | pe::IMAGE_SCN_LNK_REMOVE.0 | pe::IMAGE_SCN_ALIGN_1BYTES.0;
const FUNCTION: u16 = pe::IMAGE_SYM_DTYPE_FUNCTION.0 << 4;

struct CoffSection {
    name: &'static str,
    flags: u32,
    data: Vec<u8>,
    zero: u32,
    relocations: Vec<(u32, u32, u16)>,
}

/// A COFF object built by hand: its sections, their relocations and the symbol table.
#[derive(Default)]
struct Coff {
    sections: Vec<CoffSection>,
    symbols: Vec<[u8; 18]>,
    strings: Vec<u8>,
}

impl Coff {
    /// Adds a section; its number (from 1).
    fn section(&mut self, name: &'static str, flags: u32, data: Vec<u8>) -> i16 {
        self.sections.push(CoffSection { name, flags, data, zero: 0, relocations: Vec::new() });
        self.sections.len() as i16
    }

    fn bss(&mut self, name: &'static str, flags: u32, size: u32) -> i16 {
        self.sections.push(CoffSection { name, flags, data: Vec::new(), zero: size, relocations: Vec::new() });
        self.sections.len() as i16
    }

    fn relocate(&mut self, section: i16, at: u32, symbol: u32, typ: u16) {
        self.sections[section as usize - 1].relocations.push((at, symbol, typ));
    }

    /// Adds a symbol and its aux records; its index in the table.
    fn symbol(&mut self, name: &str, value: u32, section: i16, typ: u16, class: u8, aux: &[[u8; 18]]) -> u32 {
        let index = self.symbols.len() as u32;
        let mut record = [0u8; 18];
        if name.len() <= 8 {
            record[..name.len()].copy_from_slice(name.as_bytes());
        } else {
            let offset = 4 + self.strings.len() as u32;
            record[4..8].copy_from_slice(&offset.to_le_bytes());
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
        }
        record[8..12].copy_from_slice(&value.to_le_bytes());
        record[12..14].copy_from_slice(&section.to_le_bytes());
        record[14..16].copy_from_slice(&typ.to_le_bytes());
        record[16] = class;
        record[17] = aux.len() as u8;
        self.symbols.push(record);
        self.symbols.extend_from_slice(aux);
        index
    }

    /// A section's symbol with its aux record.
    fn section_symbol(&mut self, section: i16, number: u16, selection: u8) -> u32 {
        let it = &self.sections[section as usize - 1];
        let (name, length) = (it.name, it.data.len() as u32 + it.zero);
        let mut aux = [0u8; 18];
        aux[0..4].copy_from_slice(&length.to_le_bytes());
        aux[4..6].copy_from_slice(&(it.relocations.len() as u16).to_le_bytes());
        aux[12..14].copy_from_slice(&number.to_le_bytes());
        aux[14] = selection;
        self.symbol(name, 0, section, 0, pe::IMAGE_SYM_CLASS_STATIC.0, &[aux])
    }

    fn bytes(&self) -> Vec<u8> {
        let mut offset = 20 + 40 * self.sections.len() as u32;
        let mut data_at = Vec::new();
        for section in &self.sections {
            data_at.push(if section.data.is_empty() { 0 } else { offset });
            offset += section.data.len() as u32;
        }
        let mut relocations_at = Vec::new();
        for section in &self.sections {
            relocations_at.push(if section.relocations.is_empty() { 0 } else { offset });
            offset += 10 * section.relocations.len() as u32;
        }
        let mut out = Buf::default();
        out.put_u16(pe::IMAGE_FILE_MACHINE_AMD64.0);
        out.put_u16(self.sections.len() as u16);
        out.put_u32(0);
        out.put_u32(offset);
        out.put_u32(self.symbols.len() as u32);
        out.put_u16(0);
        out.put_u16(0);
        for (index, section) in self.sections.iter().enumerate() {
            let mut name = [0u8; 8];
            name[..section.name.len()].copy_from_slice(section.name.as_bytes());
            out.put_bytes(&name);
            out.put_u32(0);
            out.put_u32(0);
            out.put_u32(section.data.len() as u32 + section.zero);
            out.put_u32(data_at[index]);
            out.put_u32(relocations_at[index]);
            out.put_u32(0);
            out.put_u16(section.relocations.len() as u16);
            out.put_u16(0);
            out.put_u32(section.flags);
        }
        for section in &self.sections {
            out.put_bytes(&section.data);
        }
        for section in &self.sections {
            for (at, symbol, typ) in &section.relocations {
                out.put_u32(*at);
                out.put_u32(*symbol);
                out.put_u16(*typ);
            }
        }
        for record in &self.symbols {
            out.put_bytes(record);
        }
        out.put_u32(4 + self.strings.len() as u32);
        out.put_bytes(&self.strings);
        out.0
    }
}

fn weak_aux(default: u32, search: u32) -> [u8; 18] {
    let mut aux = [0u8; 18];
    aux[0..4].copy_from_slice(&default.to_le_bytes());
    aux[4..8].copy_from_slice(&search.to_le_bytes());
    aux
}

/// An object with code, data, a `.drectve`, a COMDAT function with its associative `.pdata`
/// and `.xdata`, a thread-local, zeroed data, a weak external, a common symbol and a long
/// name.
fn object() -> Vec<u8> {
    let mut coff = Coff::default();
    let mut text = vec![0u8; 16];
    text[5..9].copy_from_slice(&4u32.to_le_bytes());
    let text = coff.section(".text", TEXT, text);
    let mut data = vec![0u8; 16];
    data[0..8].copy_from_slice(&16u64.to_le_bytes());
    let data = coff.section(".data", DATA, data);
    let directives = " /DEFAULTLIB:msvcrt /DEFAULTLIB:\"oldnames\" /DEFAULTLIB:msvcrt.lib /INCLUDE:keep_me \
                      /FAILIFMISMATCH:_MSC_VER=1900 /ALTERNATENAME:alias=target /MERGE:.a=.b";
    let drectve = coff.section(".drectve", INFO, directives.as_bytes().to_vec());
    let inline = coff.section(".text$mn", TEXT | pe::IMAGE_SCN_LNK_COMDAT.0, vec![0xc3; 4]);
    let mut entry = vec![0u8; 12];
    entry[4..8].copy_from_slice(&4u32.to_le_bytes());
    let pdata = coff.section(".pdata", RDATA | pe::IMAGE_SCN_LNK_COMDAT.0, entry);
    let xdata = coff.section(".xdata", RDATA | pe::IMAGE_SCN_LNK_COMDAT.0, vec![1, 0, 0, 0, 0, 0, 0, 0]);
    let tls = coff.section(".tls$", DATA, vec![0; 8]);
    let bss = coff.bss(".bss", BSS, 32);

    coff.relocate(text, 1, 13, pe::IMAGE_REL_AMD64_REL32.0);
    coff.relocate(text, 5, 2, pe::IMAGE_REL_AMD64_ADDR32NB.0);
    coff.relocate(text, 9, 2, pe::IMAGE_REL_AMD64_REL32_4.0);
    coff.relocate(text, 12, 14, pe::IMAGE_REL_AMD64_SECREL.0);
    coff.relocate(data, 0, 13, pe::IMAGE_REL_AMD64_ADDR64.0);
    coff.relocate(data, 8, 15, pe::IMAGE_REL_AMD64_ADDR32.0);
    coff.relocate(pdata, 0, 8, pe::IMAGE_REL_AMD64_ADDR32NB.0);
    coff.relocate(pdata, 4, 8, pe::IMAGE_REL_AMD64_ADDR32NB.0);
    coff.relocate(pdata, 8, 11, pe::IMAGE_REL_AMD64_ADDR32NB.0);

    let external = pe::IMAGE_SYM_CLASS_EXTERNAL.0;
    assert_eq!(coff.section_symbol(text, 0, 0), 0);
    assert_eq!(coff.section_symbol(data, 0, 0), 2);
    assert_eq!(coff.section_symbol(drectve, 0, 0), 4);
    assert_eq!(coff.section_symbol(inline, 0, pe::IMAGE_COMDAT_SELECT_ANY.0), 6);
    assert_eq!(coff.symbol("inline_fn", 0, inline, FUNCTION, external, &[]), 8);
    assert_eq!(coff.section_symbol(pdata, inline as u16, pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE.0), 9);
    assert_eq!(coff.section_symbol(xdata, inline as u16, pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE.0), 11);
    assert_eq!(coff.symbol("callee", 0, 0, FUNCTION, external, &[]), 13);
    assert_eq!(coff.symbol("tls_var", 0, tls, 0, external, &[]), 14);
    assert_eq!(coff.symbol("main_fn", 0, text, FUNCTION, external, &[]), 15);
    let weak = weak_aux(15, pe::IMAGE_WEAK_EXTERN_SEARCH_ALIAS.0);
    assert_eq!(coff.symbol("weak_fn", 0, 0, FUNCTION, pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL.0, &[weak]), 16);
    assert_eq!(coff.symbol("common_v", 24, 0, 0, external, &[]), 18);
    assert_eq!(coff.symbol("a_very_long_symbol_name", 8, data, 0, external, &[]), 19);
    assert_eq!(coff.symbol("zeroed", 0, bss, 0, external, &[]), 20);
    coff.bytes()
}

fn read_object(bytes: &[u8]) -> crate::model::Object {
    match pe(exe()).read("test.obj", bytes, &NativeKind::Object).unwrap() {
        NativeFile::Object(object) => object,
        _ => panic!("not read as an object"),
    }
}

#[test]
fn coff_object_sections_and_groups() {
    let bytes = object();
    let object = read_object(&bytes);
    let kinds: Vec<(&str, &SectionKind, bool)> =
        object.sections.iter().map(|it| (object.name(it.name), &it.what, it.retain)).collect();
    let pdata = Class::Named { name: ".pdata".into(), write: false, exec: false };
    assert_eq!(
        kinds,
        [
            (".text", &SectionKind::Alloc(Class::Text, 0), true),
            (".data", &SectionKind::Alloc(Class::Data, 0), true),
            (".drectve", &SectionKind::Dropped, false),
            (".text$mn", &SectionKind::Alloc(Class::Text, rank(Some("mn"))), false),
            (".pdata", &SectionKind::Unwind(pdata), false),
            (".xdata", &SectionKind::Unwind(Class::Rodata), false),
            (".tls$", &SectionKind::Alloc(Class::Tdata, 1), true),
            (".bss", &SectionKind::Alloc(Class::Bss, 0), true),
        ]
    );
    assert_eq!(object.sections[0].align, 16);
    assert_eq!(object.sections[1].align, 8);
    assert_eq!(object.sections[7].data, Data::Zero(32));
    assert!(matches!(object.sections[0].data, Data::Input(_)));
    let text = object.sections[0].data.bytes(&bytes).unwrap().unwrap();
    assert_eq!(text.len(), 16);
    assert_eq!(text[5..9], 4u32.to_le_bytes());
    assert_eq!(object.sections[4].data.bytes(&bytes).unwrap().unwrap()[4..8], 4u32.to_le_bytes());
    assert_eq!(object.sections[7].data.bytes(&bytes).unwrap(), None);
    assert_eq!(object.groups.len(), 1);
    assert_eq!(object.name(object.groups[0].name), "inline_fn");
    assert_eq!(object.group_sections(&object.groups[0]), [3, 4, 5]);
    assert!(!object.groups[0].largest);
    assert_eq!(object.libraries, ["msvcrt.lib", "oldnames.lib"]);
    assert_eq!(
        object.directives,
        [
            Directive::Include("keep_me".into()),
            Directive::Mismatch { key: "_MSC_VER".into(), value: "1900".into() },
        ]
    );
    let tie = |section, to| crate::model::Tie { section, to, ordered: false };
    assert_eq!(object.ties, [tie(4, 3), tie(5, 3)], "`.pdata` and `.xdata` are kept with `.text$mn`");
}

/// SanitizerCoverage's tables of a COMDAT function: counters associative to the function,
/// its PC table associative to the counters, and counters of a function outside any COMDAT.
#[test]
fn coff_associative_sections_are_tied_to_their_parent() {
    let mut coff = Coff::default();
    let text = coff.section(".text", TEXT, vec![0xc3; 4]);
    let inline = coff.section(".text$mn", TEXT | pe::IMAGE_SCN_LNK_COMDAT.0, vec![0xc3; 4]);
    let counters = coff.section(".SCOV$CM", DATA | pe::IMAGE_SCN_LNK_COMDAT.0, vec![0; 2]);
    let pcs = coff.section(".SCOVP$M", RDATA | pe::IMAGE_SCN_LNK_COMDAT.0, vec![0; 16]);
    let plain = coff.section(".SCOV$CM", DATA | pe::IMAGE_SCN_LNK_COMDAT.0, vec![0; 1]);
    coff.section_symbol(text, 0, 0);
    coff.section_symbol(inline, 0, pe::IMAGE_COMDAT_SELECT_ANY.0);
    coff.symbol("inline_fn", 0, inline, FUNCTION, pe::IMAGE_SYM_CLASS_EXTERNAL.0, &[]);
    coff.section_symbol(counters, inline as u16, pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE.0);
    coff.section_symbol(pcs, counters as u16, pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE.0);
    coff.section_symbol(plain, text as u16, pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE.0);
    let object = read_object(&coff.bytes());
    let tie = |section, to| crate::model::Tie { section, to, ordered: false };
    assert_eq!(object.ties, [tie(2, 1), tie(3, 2), tie(4, 0)], "each to its own parent");
    assert_eq!(object.group_sections(&object.groups[0]), [1, 2, 3], "and in its root's group");
    let retain: Vec<bool> = object.sections.iter().map(|it| it.retain).collect();
    assert_eq!(retain, [true, false, false, false, true], "a non-COMDAT parent is always kept, so is its child");
}

#[test]
fn coff_object_symbols() {
    let object = read_object(&object());
    let names: Vec<&str> = object.symbols.iter().map(|it| object.name(it.name)).collect();
    assert_eq!(
        names,
        [
            ".text",
            ".data",
            ".drectve",
            ".text$mn",
            "inline_fn",
            ".pdata",
            ".xdata",
            "callee",
            "tls_var",
            "main_fn",
            "weak_fn",
            "common_v",
            "a_very_long_symbol_name",
            "zeroed",
            "target",
            "alias",
        ]
    );
    let symbol = |name: &str| object.symbols.iter().find(|it| object.name(it.name) == name).unwrap();
    assert_eq!(symbol(".text").kind, SymbolKind::Section);
    assert_eq!(symbol(".text").def, SymbolDef::Section { section: 0, value: 0, size: 16 });
    assert_eq!(symbol(".text").binding, Binding::Local);
    assert_eq!(symbol("inline_fn").def, SymbolDef::Section { section: 3, value: 0, size: 4 });
    assert_eq!(symbol("inline_fn").kind, SymbolKind::Function);
    assert_eq!(symbol("callee").def, SymbolDef::Undefined);
    assert_eq!(symbol("callee").binding, Binding::Global);
    assert_eq!(symbol("main_fn").def, SymbolDef::Section { section: 0, value: 0, size: 16 });
    assert_eq!(symbol("tls_var").def, SymbolDef::Section { section: 6, value: 0, size: 8 });
    assert_eq!(symbol("weak_fn").def, SymbolDef::Fallback { symbol: 9, search: true });
    assert_eq!(symbol("weak_fn").binding, Binding::Weak);
    assert_eq!(symbol("common_v").def, SymbolDef::Common { size: 24, align: 32 });
    assert_eq!(symbol("a_very_long_symbol_name").def, SymbolDef::Section { section: 1, value: 8, size: 8 });
    assert_eq!(symbol("zeroed").def, SymbolDef::Section { section: 7, value: 0, size: 32 });
    assert_eq!(symbol("target").def, SymbolDef::Undefined);
    assert_eq!(symbol("alias").def, SymbolDef::Fallback { symbol: 14, search: false });
}

#[test]
fn coff_object_relocations() {
    let object = read_object(&object());
    let fixup = |at, apply, symbol, addend| NativeFixup { at, apply, symbol, addend, minus: None };
    let fixups = |section: usize| object.section_fixups(&object.sections[section]);
    assert_eq!(
        fixups(0),
        [
            fixup(1, Apply::X86Branch32, 7, -4),
            fixup(5, Apply::ImageRel32, 1, 4),
            fixup(9, Apply::Rel32, 1, -8),
            fixup(12, Apply::SecRel32, 8, 0),
        ]
    );
    assert_eq!(fixups(1), [fixup(0, Apply::Abs64, 7, 16), fixup(8, Apply::Abs32, 9, 0)]);
    assert_eq!(
        fixups(4),
        [fixup(0, Apply::ImageRel32, 4, 0), fixup(4, Apply::ImageRel32, 4, 4), fixup(8, Apply::ImageRel32, 6, 0)]
    );
    assert!(fixups(2).is_empty());
    assert!(fixups(7).is_empty());
    // One column, each section's a run of it in section order.
    assert_eq!(object.fixups.len(), 9);
    assert_eq!(object.sections[1].fixups, 4..6);
}

#[test]
fn what_blaze_does_not_link_is_named() {
    let read = |coff: &Coff| pe(exe()).read("bad.obj", &coff.bytes(), &NativeKind::Object).err().unwrap();

    let mut coff = Coff::default();
    let text = coff.section(".text", TEXT, vec![0; 8]);
    coff.section_symbol(text, 0, 0);
    let target = coff.symbol("f", 0, 0, FUNCTION, pe::IMAGE_SYM_CLASS_EXTERNAL.0, &[]);
    coff.relocate(text, 0, target, pe::IMAGE_REL_AMD64_SSPAN32.0);
    let error = read(&coff);
    assert!(error.contains("SSPAN32") && error.contains("bad.obj") && error.contains('f'), "{error}");

    let mut coff = Coff::default();
    let text = coff.section(".text$x", TEXT | pe::IMAGE_SCN_LNK_COMDAT.0, vec![0; 8]);
    coff.section_symbol(text, 0, pe::IMAGE_COMDAT_SELECT_NEWEST.0);
    coff.symbol("g", 0, text, FUNCTION, pe::IMAGE_SYM_CLASS_EXTERNAL.0, &[]);
    let error = read(&coff);
    assert!(error.contains("selection 7"), "{error}");

    let mut coff = Coff::default();
    let text = coff.section(".text$y", TEXT | pe::IMAGE_SCN_LNK_COMDAT.0, vec![0; 8]);
    coff.symbol("h", 0, text, FUNCTION, pe::IMAGE_SYM_CLASS_EXTERNAL.0, &[]);
    let error = read(&coff);
    assert!(error.contains("COMDAT section .text$y has no section symbol"), "{error}");

    let mut coff = Coff::default();
    coff.section(".drectve", INFO, b"/ENTRY:start".to_vec());
    let error = read(&coff);
    assert!(error.contains("/ENTRY:start"), "{error}");

    let mut coff = Coff::default();
    let text = coff.section(".text", TEXT, vec![0; 8]);
    coff.symbol("w", 0, 0, 0, pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL.0, &[weak_aux(0, 4)]);
    coff.symbol("d", 0, text, 0, pe::IMAGE_SYM_CLASS_EXTERNAL.0, &[]);
    let error = read(&coff);
    assert!(error.contains("search type 4"), "{error}");
}

/// A short import member (`IMPORT_OBJECT_HEADER`).
fn short_import(symbol: &str, dll: &str, hint: u16, typ: u16, name: u16) -> Vec<u8> {
    let mut out = Buf::default();
    out.put_u16(0);
    out.put_u16(0xffff);
    out.put_u16(0);
    out.put_u16(pe::IMAGE_FILE_MACHINE_AMD64.0);
    out.put_u32(0);
    out.put_u32((symbol.len() + dll.len() + 2) as u32);
    out.put_u16(hint);
    out.put_u16(typ | (name << pe::IMPORT_OBJECT_NAME_SHIFT));
    out.string(symbol);
    out.string(dll);
    out.0
}

fn member_header(name: &str, size: usize) -> Vec<u8> {
    let header = format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", "0", "", "", "644");
    assert_eq!(header.len(), 60);
    header.into_bytes()
}

/// A COFF archive: the first and second linker members, then the members, each indexed by
/// the symbols given.
fn archive(members: &[(&str, Vec<u8>, &[&str])]) -> Vec<u8> {
    let mut symbols: Vec<(&str, u16)> = Vec::new();
    for (index, (_, _, names)) in members.iter().enumerate() {
        symbols.extend(names.iter().map(|it| (*it, index as u16 + 1)));
    }
    symbols.sort();
    let names: usize = symbols.iter().map(|it| it.0.len() + 1).sum();
    let first = 4 + 4 * symbols.len() + names;
    let second = 4 + 4 * members.len() + 4 + 2 * symbols.len() + names;
    let padded = |len: usize| len + len % 2;
    let mut offset = 8 + 60 + padded(first) + 60 + padded(second);
    let mut offsets = Vec::new();
    for (_, bytes, _) in members {
        offsets.push(offset as u32);
        offset += 60 + padded(bytes.len());
    }
    let mut out = Buf::default();
    out.put_bytes(b"!<arch>\n");
    out.put_bytes(&member_header("/", first));
    out.put_bytes(&(symbols.len() as u32).to_be_bytes());
    for (_, member) in &symbols {
        out.put_bytes(&offsets[*member as usize - 1].to_be_bytes());
    }
    for (name, _) in &symbols {
        out.string(name);
    }
    out.pad_to(2);
    out.put_bytes(&member_header("/", second));
    out.put_u32(members.len() as u32);
    for offset in &offsets {
        out.put_u32(*offset);
    }
    out.put_u32(symbols.len() as u32);
    for (_, member) in &symbols {
        out.put_u16(*member);
    }
    for (name, _) in &symbols {
        out.string(name);
    }
    out.pad_to(2);
    for (name, bytes, _) in members {
        out.put_bytes(&member_header(&format!("{name}/"), bytes.len()));
        out.put_bytes(bytes);
        out.pad_to(2);
    }
    out.0
}

#[test]
fn short_import_members() {
    let pe = pe(exe());
    let read = |bytes: &[u8]| match pe.read("k32.lib(k32.dll)", bytes, &NativeKind::Object).unwrap() {
        NativeFile::Shared(shared) => shared,
        _ => panic!("an import member not read as a DLL's import"),
    };
    let shared = read(&short_import("ExitProcess", "KERNEL32.dll", 356, pe::IMPORT_OBJECT_CODE.0, pe::IMPORT_OBJECT_NAME.0));
    assert_eq!(shared.name, "KERNEL32.dll");
    assert_eq!(shared.symbols.len(), 1);
    assert_eq!(shared.symbol_name(&shared.symbols[0]), "ExitProcess");
    assert_eq!(shared.symbols[0].version, Some(0));
    assert_eq!(shared.versions.len(), 1);
    assert_eq!(shared.version(&shared.symbols[0]), Some("356 ExitProcess"));
    assert_eq!(shared.symbols[0].kind, SymbolKind::Function);
    assert!(!shared.symbols[0].weak);

    let shared = read(&short_import("_f@8", "X.dll", 2, pe::IMPORT_OBJECT_CODE.0, pe::IMPORT_OBJECT_NAME_UNDECORATE.0));
    assert_eq!(shared.symbol_name(&shared.symbols[0]), "_f@8");
    assert_eq!(shared.version(&shared.symbols[0]), Some("2 f"));
    let shared = read(&short_import("_g", "X.dll", 3, pe::IMPORT_OBJECT_DATA.0, pe::IMPORT_OBJECT_NAME_NO_PREFIX.0));
    assert_eq!(shared.version(&shared.symbols[0]), Some("3 g"));
    assert_eq!(shared.symbols[0].kind, SymbolKind::Data);
    let shared = read(&short_import("ord", "X.dll", 115, pe::IMPORT_OBJECT_CODE.0, pe::IMPORT_OBJECT_ORDINAL.0));
    assert_eq!(shared.version(&shared.symbols[0]), Some("#115"));

    let error = pe.read("x.lib(x)", &short_import("h", "X.dll", 0, 3, 1), &NativeKind::Object).err().unwrap();
    assert!(error.contains("import type 3"), "{error}");
    let error = pe.read("x.lib(x)", &short_import("h", "X.dll", 0, 0, 5), &NativeKind::Object).err().unwrap();
    assert!(error.contains("import name type 5"), "{error}");
}

#[test]
fn a_name_that_is_not_utf8_is_an_error() {
    let pe = pe(exe());
    let mut import = short_import("hello", "X.dll", 0, pe::IMPORT_OBJECT_CODE.0, pe::IMPORT_OBJECT_NAME.0);
    import[20] = 0xff;
    let error = pe.read("x.lib(x)", &import, &NativeKind::Object).err().unwrap();
    assert!(error.contains("x.lib(x)") && error.contains("not UTF-8"), "{error}");

    let mut coff = Coff::default();
    coff.section(".drectve", INFO, b"/INCLUDE:\xff".to_vec());
    let error = pe.read("bad.obj", &coff.bytes(), &NativeKind::Object).err().unwrap();
    assert!(error.contains("bad.obj") && error.contains("not UTF-8"), "{error}");
    let error = asked_libraries("x.lib", &archive(&[("bad.obj", coff.bytes(), &[])])).unwrap_err();
    assert!(error.contains("x.lib(bad.obj)") && error.contains("not UTF-8"), "{error}");
}

#[test]
fn archive_index_names_members() {
    let import = short_import("ExitProcess", "KERNEL32.dll", 356, pe::IMPORT_OBJECT_CODE.0, pe::IMPORT_OBJECT_NAME.0);
    let bytes = archive(&[
        ("test.obj", object(), &["main_fn", "inline_fn"]),
        ("k32.dll", import.clone(), &["ExitProcess", "__imp_ExitProcess"]),
    ]);
    let pe = pe(exe());
    let NativeFile::Archive(archive) = pe.read("t.lib", &bytes, &NativeKind::Archive { whole: false }).unwrap() else {
        panic!("not read as an archive")
    };
    let members: Vec<&str> = archive.members.iter().map(|it| archive.name(it.name)).collect();
    assert_eq!(members, ["test.obj", "k32.dll"]);
    assert_eq!(&bytes[archive.members[1].bytes.clone()], &import[..]);
    let mut index: Vec<(&str, u32)> =
        archive.index.iter().map(|&(name, member)| (archive.name(name), member)).collect();
    index.sort();
    assert_eq!(index, [("ExitProcess", 1), ("__imp_ExitProcess", 1), ("inline_fn", 0), ("main_fn", 0)]);
    let member = &bytes[archive.members[0].bytes.clone()];
    let object = read_object(member);
    assert_eq!(object.groups.len(), 1);
    // A member's sections are ranges of the member's own bytes.
    let text = object.sections[0].data.bytes(member).unwrap().unwrap();
    assert_eq!(text[5..9], 4u32.to_le_bytes());
}

#[test]
fn import_library_is_a_shared_per_dll() {
    let code = pe::IMPORT_OBJECT_CODE.0;
    let name = pe::IMPORT_OBJECT_NAME.0;
    let bytes = archive(&[
        ("k32.dll", short_import("ExitProcess", "KERNEL32.dll", 1, code, name), &["ExitProcess", "__imp_ExitProcess"]),
        ("u32.dll", short_import("MessageBoxW", "USER32.dll", 2, code, name), &["MessageBoxW", "__imp_MessageBoxW"]),
        ("k32.dll", short_import("Sleep", "KERNEL32.dll", 3, code, name), &["Sleep", "__imp_Sleep"]),
    ]);
    let pe = pe(exe());
    let NativeFile::Shared(shared) = pe.read("mixed.lib", &bytes, &NativeKind::Shared { as_needed: true }).unwrap() else {
        panic!("not read as an import library")
    };
    assert_eq!(shared.name, "KERNEL32.dll");
    let names: Vec<&str> = shared.symbols.iter().map(|it| shared.symbol_name(it)).collect();
    assert_eq!(names, ["ExitProcess", "Sleep"]);
    let versions: Vec<Option<&str>> = shared.symbols.iter().map(|it| shared.version(it)).collect();
    assert_eq!(versions, [Some("1 ExitProcess"), Some("3 Sleep")]);
    assert_eq!(shared.more.len(), 1);
    let user32 = &shared.more[0];
    assert_eq!(user32.name, "USER32.dll");
    assert_eq!(user32.symbol_name(&user32.symbols[0]), "MessageBoxW");
    assert_eq!(user32.version(&user32.symbols[0]), Some("2 MessageBoxW"));

    let error = pe.read("x.dll", b"MZ\x90\0", &NativeKind::Shared { as_needed: true }).err().unwrap();
    assert!(error.contains("import library"), "{error}");
}

/// Grouped on every thread, an import library reads as one pass over its members would: the
/// DLLs in the order each is first met, each DLL's imports in member order, and the first
/// failing member's error.
#[test]
fn import_library_groups_as_one_pass_would() {
    let pe = pe(exe());
    let (members, dlls) = (3 * 1000 + 11, 7);
    let bytes = import_archive(members, dlls);
    let NativeFile::Shared(shared) = pe.read("x.lib", &bytes, &NativeKind::Shared { as_needed: true }).unwrap() else {
        panic!("not read as an import library")
    };
    assert_eq!(shared.more.len(), dlls - 1);
    for (dll, read) in std::iter::once(&shared).chain(&shared.more).enumerate() {
        assert_eq!(read.name, format!("D{dll}.dll"));
        let names: Vec<&str> = read.symbols.iter().map(|it| read.symbol_name(it)).collect();
        let expected: Vec<String> = (dll..members).step_by(dlls).map(|at| format!("F{at}")).collect();
        assert_eq!(names, expected, "D{dll}.dll");
    }

    let code = pe::IMPORT_OBJECT_CODE.0;
    let name = pe::IMPORT_OBJECT_NAME.0;
    let bytes = archive(&[
        ("k32.dll", short_import("ExitProcess", "KERNEL32.dll", 1, code, name), &["ExitProcess"]),
        ("first.obj", object(), &["main_fn"]),
        ("u32.dll", short_import("MessageBoxW", "USER32.dll", 2, code, name), &["MessageBoxW"]),
        ("second.obj", object(), &["other_fn"]),
    ]);
    let error = pe.read("mixed.lib", &bytes, &NativeKind::Shared { as_needed: true }).err().unwrap();
    assert!(error.contains("first.obj") && !error.contains("second.obj"), "{error}");
}

// Allocation budgets.

/// What reading `bytes` as `kind` allocates on this thread.
/// An object whose only section is a `.drectve` of `directives`.
fn directives_only(directives: &str) -> Vec<u8> {
    let mut coff = Coff::default();
    coff.section(".drectve", INFO, directives.as_bytes().to_vec());
    coff.bytes()
}

#[test]
fn archive_members_ask_for_their_default_libraries() {
    let import = short_import("ExitProcess", "KERNEL32.dll", 356, pe::IMPORT_OBJECT_CODE.0, pe::IMPORT_OBJECT_NAME.0);
    let bytes = archive(&[
        ("a.obj", directives_only("/DEFAULTLIB:vcruntime -defaultlib:\"ucrt.lib\" /DEFAULTLIB:libcmt /nodefaultlib:LIBCMT.LIB /INCLUDE:x"), &[]),
        ("k32.dll", import, &[]),
        ("b.obj", directives_only("/DEFAULTLIB:VCRUNTIME.lib /DEFAULTLIB:oldnames"), &[]),
        ("c.obj", directives_only("/DEFAULTLIB:gone /NODEFAULTLIB"), &[]),
        ("test.obj", object(), &["main_fn"]),
    ]);
    let asked = asked_libraries("msvcrt.lib", &bytes).unwrap();
    let asked: Vec<(&str, &str)> = asked.iter().map(|(member, library)| (member.as_str(), library.as_str())).collect();
    assert_eq!(
        asked,
        [("a.obj", "vcruntime.lib"), ("a.obj", "ucrt.lib"), ("b.obj", "oldnames.lib"), ("test.obj", "msvcrt.lib")],
        "each once, in member order, less its member's own /NODEFAULTLIBs"
    );

    let error = asked_libraries("x.lib", &archive(&[("bad.obj", directives_only("/DEFAULTLIB"), &[])])).unwrap_err();
    assert!(error.contains("x.lib(bad.obj)") && error.contains("/DEFAULTLIB needs an argument"), "{error}");
    assert!(asked_libraries("empty.lib", b"!<arch>\n").unwrap().is_empty());
}

fn allocations(pe: &Pe, name: &str, bytes: &[u8], kind: NativeKind) -> u64 {
    let (file, used) = measure(|| pe.read(name, bytes, &kind));
    assert!(file.is_ok(), "{:?}", file.err());
    used.allocations
}

/// An object of `functions` COMDAT functions (`.text$mn`, selection any), each calling the
/// next, and a `.data` holding the address of each: two relocations per function.
fn functions_object(functions: usize) -> Vec<u8> {
    let mut coff = Coff::default();
    let data = coff.section(".data", DATA, vec![0; 8 * functions]);
    let texts: Vec<i16> = (0..functions)
        .map(|_| coff.section(".text$mn", TEXT | pe::IMAGE_SCN_LNK_COMDAT.0, vec![0xe8, 0, 0, 0, 0, 0xc3]))
        .collect();
    // `.data`'s symbol and its aux record, then per function its section's symbol, the aux
    // record and the function.
    let function = |at: usize| 4 + 3 * at as u32;
    for (at, &text) in texts.iter().enumerate() {
        coff.relocate(text, 1, function((at + 1) % functions), pe::IMAGE_REL_AMD64_REL32.0);
        coff.relocate(data, 8 * at as u32, function(at), pe::IMAGE_REL_AMD64_ADDR64.0);
    }
    assert_eq!(coff.section_symbol(data, 0, 0), 0);
    for (at, &text) in texts.iter().enumerate() {
        assert_eq!(coff.section_symbol(text, 0, pe::IMAGE_COMDAT_SELECT_ANY.0), function(at) - 2);
        let name = format!("f{at}");
        assert_eq!(coff.symbol(&name, 0, text, FUNCTION, pe::IMAGE_SYM_CLASS_EXTERNAL.0, &[]), function(at));
    }
    coff.bytes()
}

/// An import library of `members` short import members (`F<n>` and `__imp_F<n>` in its index)
/// spread over `dlls` DLLs.
fn import_archive(members: usize, dlls: usize) -> Vec<u8> {
    let names: Vec<String> = (0..members).map(|at| format!("m{at}.dll")).collect();
    let symbols: Vec<[String; 2]> = (0..members).map(|at| [format!("F{at}"), format!("__imp_F{at}")]).collect();
    let indexed: Vec<[&str; 2]> = symbols.iter().map(|[a, b]| [a.as_str(), b.as_str()]).collect();
    let list: Vec<(&str, Vec<u8>, &[&str])> = (0..members)
        .map(|at| {
            let dll = format!("D{}.dll", at % dlls);
            let bytes = short_import(&symbols[at][0], &dll, 1, pe::IMPORT_OBJECT_CODE.0, pe::IMPORT_OBJECT_NAME.0);
            (names[at].as_str(), bytes, &indexed[at][..])
        })
        .collect();
    archive(&list)
}

/// Reading a COFF object allocates each of its columns once, whatever its number of
/// sections, symbols and relocations, and wherever its bytes are (an archive member is only
/// 2-aligned): nothing per symbol, per relocation or per COMDAT.
#[test]
fn coff_object_read_allocates_a_constant_number_of_buffers() {
    // The object's sections, relocations, names; the reader's section names, its symbols
    // (which become the object's), per-section marks and defined symbols; and for COMDATs the
    // groups' members, the object's groups and grouped sections. The symbol map and each
    // section's group are the worker's scratch buffers, warm once a larger object was read
    // on this thread. The radix sorts find their keys in order and allocate nothing.
    const BUDGET: u64 = 10;
    let pe = pe(exe());
    let objects = [functions_object(8), functions_object(200)];
    let warm = allocations(&pe, "t.obj", &objects[1], NativeKind::Object);
    assert!(warm <= BUDGET + 2, "a cold read grows the two scratch buffers: {warm} allocations");
    for (bytes, functions) in objects.iter().zip([8, 200]) {
        let object = read_object(bytes);
        assert_eq!(object.groups.len(), functions);
        assert_eq!(object.fixups.len(), 2 * functions);
        assert_eq!(object.name(object.groups[functions - 1].name), format!("f{}", functions - 1));
        let mut shifted = vec![0u8];
        shifted.extend_from_slice(bytes);
        for input in [&bytes[..], &shifted[1..]] {
            let used = allocations(&pe, "t.obj", input, NativeKind::Object);
            assert_eq!(used, BUDGET, "{functions} functions");
        }
    }
}

/// Reading an archive allocates its three columns (members, index, names), each sized by a
/// first pass over the member headers and the index, whatever its number of members.
#[test]
fn archive_read_allocates_its_three_columns() {
    const BUDGET: u64 = 3;
    let pe = pe(exe());
    for members in [8, 200] {
        let bytes = import_archive(members, 3);
        let used = allocations(&pe, "x.lib", &bytes, NativeKind::Archive { whole: false });
        assert_eq!(used, BUDGET, "{members} members");
    }
}

/// Reading an import library allocates per member: each import's `Shared::versions` entry,
/// and the amortized growth of its DLL's columns. The rest is per DLL or per library: the
/// archive's three columns, the DLLs and their index, and each DLL's name.
#[test]
fn import_library_read_allocates_per_member() {
    let pe = pe(exe());
    // One worker, measured on it: the members are parsed and grouped on the pool's threads.
    let pool = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    let read = |members: usize| {
        let bytes = import_archive(members, 3);
        let used = pool
            .install(|| allocations(&pe, "x.lib", &bytes, NativeKind::Shared { as_needed: true }));
        assert!(used <= 2 * members as u64 + 16, "{members} members: {used} allocations");
        used
    };
    let (small, large) = (read(64), read(256));
    assert!(large - small <= 2 * (256 - 64), "{small} and {large} allocations: at most two more per member");
}

/// The writer's lookups by name allocate nothing: what a name is to the link, a section's
/// class and rank, an import's export name.
#[test]
fn name_lookups_allocate_nothing() {
    let pe = pe(exe());
    assert_at_most(0, "PE's name lookups", || {
        let specials = [
            "__ImageBase",
            "_tls_used",
            "_tls_index",
            "__blaze_live",
            "__guard_flags",
            "__enclave_config",
            "__imp_",
            "__imp_ExitProcess",
            "main",
        ];
        for name in specials {
            std::hint::black_box(pe.special(name));
        }
        let sections = [".text$mn", ".rdata$r", ".xdata", ".data", ".bss", ".tls$", ".tls$ZZZ", ".CRT$XCU", ".pdata", ".rsrc$01"];
        for name in sections {
            std::hint::black_box(pe.section(name, false, false, false).is_ok());
        }
        std::hint::black_box(ImportName::parse("f", Some("356 ExitProcess")));
        std::hint::black_box(ImportName::parse("f", Some("#115")));
        std::hint::black_box((rank(Some("XCU")), place(&Class::Got), pe.prot(&Class::Text), pe.own(&Class::Tbss)));
    });
}

/// `.reloc` of one piece (fewer rebased addresses than a thread's share) is one buffer, its
/// bytes, whatever the number of addresses.
#[test]
fn base_relocations_of_one_piece_are_one_buffer() {
    for count in [16u64, 4096] {
        let pieces =
            vec![(Class::Text, Data::Bytes(vec![0xc3; 0x10])), (Class::Data, Data::Bytes(vec![0; count as usize * 8]))];
        let mut f = Fixture::new(exe(), pieces, &Counts::default());
        let data = f.at(&Class::Data);
        f.rebase = (0..count).map(|it| data + it * 8).collect();
        let (relocations, used) =
            f.placed(|placed| measure(|| image::relocations(Image::of(placed), &[data], usize::MAX)));
        assert!(relocations.is_ok());
        assert_eq!(used.allocations, 1, "{count} addresses: the bytes of `.reloc` alone");
    }
}

fn dev_exe() -> Output {
    Output { dev_run: Some(DevRun { arena: 0x2_0000, cells: 0x1_0000, fixed: 0x3_0000 }), ..exe() }
}

fn crt_class() -> Class {
    Class::Named { name: ".CRT".into(), write: false, exec: false }
}

/// Where `blaze_inproc_start` is in `.text`, and another TLS callback (std's).
const HOOK: u64 = 0x10;
const OTHER_CALLBACK: u64 = 0x20;

/// A dev run's executable as the core places one: code holding the start hook, room for a load
/// configuration, the CRT's TLS callbacks (`__xl_a`, the hook's pointer from `.CRT$XLAB`, std's
/// from `.CRT$XLB`, `__xl_z`), data, a thread-local and the three reservations, the arena
/// `arena` bytes.
fn dev_executable_with(arena: u64) -> Fixture {
    let counts = Counts { absolute: 3, ..Counts::default() };
    let pieces = vec![
        (Class::Text, Data::Bytes(vec![0xcc; 0x40])),
        (Class::Rodata, Data::Bytes(vec![0; 0x140])),
        (crt_class(), Data::Bytes(vec![0; 32])),
        (Class::Data, Data::Bytes(vec![0; 0x10])),
        (Class::Tdata, Data::Bytes(vec![7; 8])),
        (Class::Reserve(Reserve::Fixed), Data::Zero(0x3_0000)),
        (Class::Reserve(Reserve::Arena), Data::Zero(arena)),
        (Class::Reserve(Reserve::Cells), Data::Zero(0x1_0000)),
    ];
    let mut f = Fixture::new(dev_exe(), pieces, &counts);
    let (text, crt, tdata) = (f.at(&Class::Text), f.at(&crt_class()), f.at(&Class::Tdata));
    f.set(&crt_class(), callbacks(&[0, text + HOOK, text + OTHER_CALLBACK, 0]));
    f.rebase = vec![crt + 8, crt + 16];
    f.entry = Some(text);
    f.tls = Some(Tls { addr: tdata, init: 8, size: 8, align: 8 });
    f.symbols = vec![("__xl_a", crt), (START_HOOK, text + HOOK)];
    f
}

fn dev_executable() -> Fixture {
    dev_executable_with(0x2_0000)
}

fn callbacks(cells: &[u64]) -> Vec<u8> {
    let mut bytes = Buf::default();
    for cell in cells {
        bytes.put_u64(*cell);
    }
    bytes.0
}

fn rebased(file: &PeFile64<'_>, image: &[u8]) -> Vec<u32> {
    let mut blocks = file.data_directories().relocation_blocks(image, &file.section_table()).unwrap().unwrap();
    let mut seen = Vec::new();
    while let Some(block) = blocks.next().unwrap() {
        seen.extend(block.map(|it| it.virtual_address));
    }
    seen
}

/// (VirtualAddress, VirtualSize, SizeOfRawData, PointerToRawData, Characteristics) of each section.
fn section_headers(file: &PeFile64<'_>) -> Vec<(u32, u32, u32, u32, u32)> {
    file.section_table()
        .iter()
        .map(|it| {
            (
                it.virtual_address.get(LE),
                it.virtual_size.get(LE),
                it.size_of_raw_data.get(LE),
                it.pointer_to_raw_data.get(LE),
                it.characteristics.get(LE).0,
            )
        })
        .collect()
}

const ACCESS: u32 = pe::IMAGE_SCN_MEM_READ.0 | pe::IMAGE_SCN_MEM_WRITE.0 | pe::IMAGE_SCN_MEM_EXECUTE.0;

#[test]
fn dev_run_start_hook_is_the_first_tls_callback_after_xl_a() {
    let writer = pe(dev_exe());
    let (class, hook) = writer.start_hook().unwrap();
    assert_eq!((class.clone(), hook), writer.section(".CRT$XLAB", false, false, false).unwrap());
    assert_eq!(class, crt_class());
    let xla = writer.section(".CRT$XLA", false, false, false).unwrap().1;
    let xlb = writer.section(".CRT$XLB", false, false, false).unwrap().1;
    assert!(xla < hook && hook < xlb, "after `__xl_a`, before std's callback");

    // The writer's own directory, over the CRT's callbacks.
    let mut f = dev_executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let (text, crt, dir) = (f.at(&Class::Text), f.at(&crt_class()), f.at(&Class::Synthetic(TLS_DIR)));
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_TLS), (f.rva(dir), 40));
    let list = u64_at(&image[f.rva(dir) as usize..], 24);
    assert_eq!(list, crt + 8, "the callbacks are the CRT's run after `__xl_a`");
    assert_eq!(u64_at(&image, f.rva(list) as usize), text + HOOK);
    assert!(rebased(&file, &image).contains(&f.rva(crt + 8)), "the hook's pointer is data the loader rebases");

    // The CRT's `_tls_used`.
    let mut f = dev_executable();
    let rodata = f.at(&Class::Rodata);
    let mut used = callbacks(&[0, 0, 0, crt + 8]);
    used.resize(0x140, 0);
    f.set(&Class::Rodata, used);
    f.tls = None;
    f.symbols.push(("_tls_used", rodata));
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_TLS), (f.rva(rodata), 40));

    // No CRT: the writer's own list holds the hook and its terminator, the hook rebased.
    for tls in [true, false] {
        let mut f = dev_executable();
        f.symbols.retain(|it| it.0 != "__xl_a");
        if !tls {
            f.tls = None;
        }
        let image = f.image().unwrap();
        let file = PeFile64::parse(&image[..]).unwrap();
        assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_TLS), (f.rva(dir), 40), "a directory, thread-locals or not");
        let bytes = &image[f.rva(dir) as usize..][..56];
        assert_eq!(u64_at(bytes, 24), dir + 40);
        assert_eq!(u64_at(bytes, 40), text + HOOK);
        assert_eq!(u64_at(bytes, 48), 0);
        assert!(rebased(&file, &image).contains(&f.rva(dir + 40)));
    }

    let mut f = dev_executable();
    f.set(&crt_class(), callbacks(&[0, text + OTHER_CALLBACK, text + HOOK, 0]));
    f.fails("not the start hook");

    let mut f = dev_executable();
    f.symbols.retain(|it| it.0 != START_HOOK);
    f.fails(START_HOOK);
    let mut f = dev_executable();
    f.symbols.clear();
    f.fails(START_HOOK);
}

#[test]
fn dev_run_code_is_on_its_own_granules() {
    let writer = pe(dev_exe());
    let plan = writer.plan();
    assert_eq!((plan.page, plan.headers), (GRANULE, GRANULE));

    let mut f = dev_executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let optional = file.nt_headers().optional_header();
    assert_eq!(optional.size_of_headers(), 0x1_0000);
    assert_eq!(optional.base_of_code(), 0x1_0000);
    assert_eq!(optional.section_alignment(), 0x1000);
    let sections = section_headers(&file);
    let (va, size, raw, at, flags) = sections[0];
    assert_eq!(va, 0x1_0000, "the code starts on a granule");
    assert_ne!(flags & pe::IMAGE_SCN_MEM_EXECUTE.0, 0);
    assert_eq!(flags & pe::IMAGE_SCN_MEM_WRITE.0, 0);
    assert_eq!(at, va);
    assert!(size <= raw, "the code is all raw data");
    assert_eq!(sections[1].0, 0x2_0000, "the next section starts on the next granule");
    assert!(sections[1..].iter().all(|it| it.4 & pe::IMAGE_SCN_MEM_EXECUTE.0 == 0), "code before all else");
    assert!(sections.iter().all(|it| it.4 & pe::IMAGE_SCN_MEM_EXECUTE.0 == 0 || it.4 & pe::IMAGE_SCN_MEM_WRITE.0 == 0));
    assert_eq!(optional.address_of_entry_point(), f.rva(f.at(&Class::Text)));
    for pair in sections.windows(2) {
        assert_eq!(pair[0].0 + pair[0].1, pair[1].0, "adjacent, as the NT loader wants");
    }
    for &(va, _, raw, at, _) in &sections {
        if raw > 0 {
            assert_eq!(at, va, "a section's file offset is its RVA");
        }
    }
    assert_eq!(&image[..2], b"MZ");
    assert!(image[0x1_0000..0x1_0040].iter().all(|it| *it == 0xcc), "the code at its file offset");

    let pieces = vec![
        (Class::Text, Data::Bytes(vec![0xcc; 0x40])),
        (Class::Named { name: ".wx".into(), write: true, exec: true }, Data::Bytes(vec![0x90; 0x10])),
    ];
    let mut f = Fixture::new(dev_exe(), pieces, &Counts::default());
    f.entry = Some(f.at(&Class::Text));
    f.symbols = vec![(START_HOOK, f.at(&Class::Text))];
    f.fails("writable and executable");
}

#[test]
fn dev_run_reservations_are_the_last_sections_with_no_access() {
    let writer = pe(dev_exe());
    let mut classes = vec![
        Class::Reserve(Reserve::Fixed),
        Class::Synthetic(RELOC),
        Class::Reserve(Reserve::Cells),
        Class::Text,
        Class::Reserve(Reserve::Arena),
        Class::Data,
    ];
    writer.order(&mut classes);
    assert_eq!(classes[3..], [
        Class::Reserve(Reserve::Arena),
        Class::Reserve(Reserve::Cells),
        Class::Reserve(Reserve::Fixed)
    ]);
    for reserve in [Reserve::Arena, Reserve::Cells, Reserve::Fixed] {
        assert_eq!(writer.prot(&Class::Reserve(reserve)), Prot::NONE);
    }

    let mut f = dev_executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let names = section_names(&file);
    assert_eq!(names[names.len() - 3..], [".barena", ".bcells", ".bfixed"]);
    let sections = section_headers(&file);
    let arena = f.at(&Class::Reserve(Reserve::Arena));
    let mut at = f.rva(arena);
    for (&(va, size, raw, raw_at, flags), want) in sections[sections.len() - 3..].iter().zip([0x2_0000, 0x1_0000, 0x3_0000]) {
        assert_eq!((va, size, raw, raw_at), (at, want, 0, 0));
        assert_eq!(flags & ACCESS, 0, "no access");
        assert_eq!(u64::from(va) % GRANULE, 0);
        at += want;
    }
    assert_eq!(file.nt_headers().optional_header().size_of_image(), at, "the image covers the reservations");
    f.placed(|placed| {
        for region in placed.regions.iter().filter(|it| matches!(it.class, Class::Reserve(_))) {
            assert_eq!(f.pe.offset(placed, region), None, "not in the file");
        }
    });

    let mut f = dev_executable_with(0x1000);
    f.fails("not whole");
}

#[test]
fn dev_run_has_no_base_relocation_in_code_and_no_cfg() {
    let mut f = dev_executable();
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let (va, size, ..) = section_headers(&file)[0];
    assert!(rebased(&file, &image).iter().all(|it| !(va..va + size).contains(it)));
    let optional = file.nt_headers().optional_header();
    assert_eq!(optional.dll_characteristics().0 & pe::IMAGE_DLLCHARACTERISTICS_GUARD_CF.0, 0);

    let mut f = dev_executable();
    let text = f.at(&Class::Text);
    f.functions = vec![Function { addr: text, size: 0x10, fde: None, windows: None, name: Some("absolute".into()) }];
    f.rebase.insert(0, text + 8);
    f.fails("`absolute` (+0x8)");
    let mut f = dev_executable();
    f.rebase.insert(0, f.at(&Class::Text) + 0x30);
    f.fails("in Text");
    let mut f = executable();
    f.rebase.insert(0, f.at(&Class::Text) + 8);
    f.image().expect("outside a dev run, the loader may write code");

    let config = |table: u64, count: u64, flags: u32| {
        let mut config = vec![0; 0x140];
        config[..4].copy_from_slice(&0x140u32.to_le_bytes());
        config[0x80..0x88].copy_from_slice(&table.to_le_bytes());
        config[0x88..0x90].copy_from_slice(&count.to_le_bytes());
        config[0x90..0x94].copy_from_slice(&flags.to_le_bytes());
        config
    };
    let mut f = dev_executable();
    let rodata = f.at(&Class::Rodata);
    f.symbols.push(("_load_config_used", rodata));
    f.set(&Class::Rodata, config(0, 0, 0));
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG), (f.rva(rodata), 0x140));
    for (table, count, flags) in [(rodata, 1, 0x100), (0, 0, 0x100), (rodata, 0, 0)] {
        f.set(&Class::Rodata, config(table, count, flags));
        f.fails("control-flow guard");
    }
}

#[test]
fn without_a_dev_run_the_image_is_unchanged() {
    let writer = pe(exe());
    let plan = writer.plan();
    assert_eq!((plan.page, plan.headers), (0x1000, 0x1000));
    let tls = |dev_run| image::synthetic(&Counts::default(), dev_run).into_iter().find(|it| it.0 == Class::Synthetic(TLS_DIR));
    assert_eq!(tls(false), Some((Class::Synthetic(TLS_DIR), 48)));
    assert_eq!(tls(true), Some((Class::Synthetic(TLS_DIR), 56)));
    let mut dll = dev_exe();
    dll.kind = OutputKind::Shared { name: "lib.dll".into(), exports: Vec::new() };
    assert!(super::writer(&dll).err().unwrap().what.contains("DLL"));

    // The dev run's pieces, linked as `blaze build` links them.
    let counts = Counts { absolute: 3, ..Counts::default() };
    let pieces = vec![
        (Class::Text, Data::Bytes(vec![0xcc; 0x40])),
        (Class::Rodata, Data::Bytes(vec![0; 0x140])),
        (crt_class(), Data::Bytes(vec![0; 32])),
        (Class::Data, Data::Bytes(vec![0; 0x10])),
    ];
    let mut f = Fixture::new(exe(), pieces, &counts);
    let text = f.at(&Class::Text);
    f.entry = Some(text);
    let image = f.image().unwrap();
    let file = PeFile64::parse(&image[..]).unwrap();
    let optional = file.nt_headers().optional_header();
    assert_eq!(optional.size_of_headers(), 0x1000);
    assert_eq!(section_names(&file), [".text", ".rdata", ".data", ".reloc"]);
    let sections = section_headers(&file);
    assert_eq!((sections[0].0, sections[0].1, sections[0].2), (0x1000, 0x40, 0x1000));
    assert_eq!(sections[1].0, 0x2000);
    assert_eq!(directory(&file, pe::IMAGE_DIRECTORY_ENTRY_TLS), (0, 0), "no TLS directory with no thread-locals");
    let last = sections.last().unwrap();
    assert_eq!(optional.size_of_image(), (last.0 + last.1).next_multiple_of(0x1000));
}

#[test]
fn dev_run_image_is_what_the_start_hook_maps() {
    let mut f = dev_executable();
    let image = f.image().unwrap();
    let layout = blaze_inproc::image::layout(blaze_inproc::Format::Pe, &image, GRANULE).unwrap();
    let base = f.plan.base;
    let arena = f.at(&Class::Reserve(Reserve::Arena));
    assert_eq!(layout.header, base);
    assert_eq!(layout.code, [blaze_inproc::Segment { offset: 0x1_0000, linked: base + 0x1_0000, len: 0x1_0000 }]);
    assert_eq!(layout.reserved, [arena..arena + 0x2_0000, arena + 0x2_0000..arena + 0x3_0000, arena + 0x3_0000..arena + 0x6_0000]);
    assert_eq!(layout.span, base..arena + 0x6_0000);
    assert_eq!(blaze_inproc::image::headers_len(blaze_inproc::Format::Pe, &image), Some(0x1_0000));

    let mut f = executable();
    let image = f.image().unwrap();
    assert!(blaze_inproc::image::layout(blaze_inproc::Format::Pe, &image, GRANULE).is_err(), "a build's code shares a granule");
}

#[test]
fn an_executable_with_the_execution_log_starts_on_its_log_stubs() {
    let logged = crate::table::linked::logged(exe());
    assert!(logged.header.is_multiple_of(8));
}
