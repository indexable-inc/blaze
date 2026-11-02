//! Dependency images ([`crate::premap`]): a prelink at a fixed base and what it says of every
//! piece, and a workspace link beside an [`Outside`] resolving as a whole link of both would.

use std::sync::Arc;

use blaze_codegen::{Init, InstanceKey, RelocKind, Static, Target};
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use object::write::{Object as Builder, Relocation, StandardSection, Symbol as ObjSymbol, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, RelocationFlags, SectionKind, SymbolFlags, SymbolKind, SymbolScope, elf};

use super::{CALLER, Disk, function, id, key, name, output, rel32, returns};
use crate::premap::{
    Bind, BindKind, Def, InitEntry, Kind, Placed, PlacedStatic, Prot, Request, SegmentPlace, StaticAt, Strength, layout,
    prelink_placed,
};
use crate::state::State;
use crate::{Change, Image, Native, NativeKind, Output, OutputKind, Outside};

const BASE: u64 = 0x7000_0000;
/// The run's thread-local area starts 64 KiB below the thread pointer.
const BIAS: i64 = -(64 << 10);

/// Each segment's place and bytes, written by [`Placed::write_into`] one after another, each
/// at a multiple of 64 KiB (every format's page divides it).
fn segments(placed: &Placed) -> Vec<(SegmentPlace, Vec<u8>)> {
    const STEP: usize = 64 << 10;
    let mut offsets = Vec::with_capacity(placed.segments.len());
    let mut end = 0;
    for it in &placed.segments {
        offsets.push(end);
        end += (it.len as usize).div_ceil(STEP) * STEP;
    }
    let mut image = vec![0xaa; end];
    placed.write_into(&mut image, &offsets).unwrap();
    placed.segments.iter().zip(&offsets).map(|(it, at)| (*it, image[*at..][..it.len as usize].to_vec())).collect()
}

fn shared() -> Output {
    Output {
        kind: OutputKind::Shared { name: "libdeps.so".into(), exports: Vec::new() },
        interpreter: None,
        ..output(false)
    }
}

fn reloc(at: u32, kind: RelocKind, target: Target) -> blaze_codegen::Relocation {
    blaze_codegen::Relocation { at, kind, target, addend: 0 }
}

fn a_static(n: u8, bytes: Vec<u8>, relocs: Vec<blaze_codegen::Relocation>) -> Static {
    Static {
        item: ItemKey(id(n)),
        instance: id(n + 100),
        align: 8,
        init: Init::Bytes { bytes, relocs: relocs.into() },
        data: Vec::new(),
        mutable: true,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

fn thread_local(n: u8, bytes: Vec<u8>, align: u32) -> Static {
    Static { align, mutable: false, thread_local: true, ..a_static(n, bytes, Vec::new()) }
}

fn symbol(name: &str, value: u64, size: u64, kind: SymbolKind, weak: bool, section: object::write::SectionId) -> ObjSymbol {
    ObjSymbol {
        name: name.as_bytes().to_vec(),
        value,
        size,
        kind,
        scope: SymbolScope::Dynamic,
        weak,
        section: SymbolSection::Section(section),
        flags: SymbolFlags::None,
    }
}

/// An x86-64 ELF object: a weak `hook` (`ret`), and `table`, a pointer to it in `.data`.
fn hooked() -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = b.add_section(Vec::new(), b".text.hook".to_vec(), SectionKind::Text);
    let at = b.append_section_data(text, &[0xc3], 16);
    let hook = b.add_symbol(symbol("hook", at, 1, SymbolKind::Text, true, text));
    let data = b.section_id(StandardSection::Data);
    let at = b.append_section_data(data, &[0; 8], 8);
    b.add_symbol(symbol("table", at, 8, SymbolKind::Data, false, data));
    b.add_relocation(data, Relocation {
        offset: at,
        symbol: hook,
        addend: 0,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
    })
    .unwrap();
    b.write().unwrap()
}

/// An x86-64 ELF object defining each name, weakly or not, as a `ret` in a section of its own.
fn rets(names: &[(&str, bool)]) -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    for (it, weak) in names {
        let text = b.add_section(Vec::new(), format!(".text.{it}").into_bytes(), SectionKind::Text);
        let at = b.append_section_data(text, &[0xc3], 16);
        b.add_symbol(symbol(it, at, 1, SymbolKind::Text, *weak, text));
    }
    b.write().unwrap()
}

/// A GNU archive of `members` (name, bytes, the names it defines), with its symbol index.
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

/// `len` bytes of the image at `addr`.
fn read(state: &State, addr: u64, len: usize) -> Vec<u8> {
    let space = state
        .spaces
        .iter()
        .find(|it| (it.region.addr..it.region.addr + it.region.size).contains(&addr))
        .unwrap_or_else(|| panic!("{addr:#x} is in no region"));
    let mut out = vec![0; len];
    space.bytes.copy_to((addr - space.region.addr) as usize, &mut out);
    out
}

fn u64_at(state: &State, addr: u64) -> u64 {
    u64::from_le_bytes(read(state, addr, 8).try_into().unwrap())
}

fn i32_at(state: &State, addr: u64) -> i32 {
    i32::from_le_bytes(read(state, addr, 4).try_into().unwrap())
}

/// Where the call at `at` in the code at `code` goes.
fn call(state: &State, code: u64, at: u64) -> u64 {
    assert_eq!(read(state, code + at - 1, 1), [0xe8], "a call at {at}");
    (code + at + 4).wrapping_add_signed(i64::from(i32_at(state, code + at)))
}

fn tie(item: u8, instance: u8) -> [u8; 64] {
    let mut tie = [0; 64];
    tie[..32].copy_from_slice(&id(item).0);
    tie[32..].copy_from_slice(&id(instance).0);
    tie
}

#[test]
fn a_prelink_places_the_image_at_its_base_and_says_where_everything_is() {
    let f1 = function(1, &returns(1), Vec::new());
    let f2 = function(2, &CALLER, vec![rel32(1, Target::Direct(key(1)))]);
    let pointers = a_static(20, vec![0; 16], vec![
        reloc(0, RelocKind::Abs64, Target::Direct(key(1))),
        reloc(8, RelocKind::Abs64, Target::Extern("table".into())),
    ]);
    let init = Static {
        mutable: false,
        section: Some(".init_array".into()),
        ..a_static(21, vec![0; 8], vec![reloc(0, RelocKind::Abs64, Target::Direct(key(1)))])
    };
    let local = thread_local(22, vec![7, 0, 0, 0], 4);
    let object: Arc<[u8]> = hooked().into();
    let output = shared();
    let request = Request {
        output: &output,
        base: BASE,
        tls_bias: BIAS,
        functions: &[&f1, &f2],
        statics: &[&pointers, &init, &local],
        names: &[&("dep_entry".into(), Target::Direct(key(2)))],
        debug_names: &[],
        natives: &[Native { id: id(60), name: "hooked.o", bytes: &object, kind: NativeKind::Object, order: 0 }],
        spans: &[],
    };
    let placed = prelink_placed(&request).unwrap();
    let linked = &placed.linked;
    let written = segments(&placed);

    let prots: Vec<Prot> = placed.segments.iter().map(|it| it.prot).collect();
    assert_eq!(prots, [Prot::ReadOnly, Prot::Code, Prot::ReadOnly, Prot::Data], "read, code, relro, write");
    assert_eq!(placed.segments[0].addr, BASE, "never slid");
    let mut end = BASE;
    for it in &placed.segments {
        assert!(it.addr % 4096 == 0 && it.addr >= end && it.len <= it.size, "{it:?}");
        end = it.addr + it.size;
    }
    let (code, code_bytes) = &written[1];

    let (one, two) = (linked.functions[0], linked.functions[1]);
    assert_eq!((one.0, one.2, two.0, two.2), (key(1), 6, key(2), 6));
    assert_eq!(two.1, one.1 + 16, "functions in key order, 16-aligned");
    let at = |addr: u64, len: usize| code_bytes[(addr - code.addr) as usize..][..len].to_vec();
    assert_eq!(at(one.1, 6), returns(1));
    assert_eq!(at(two.1, 6), [0xe8, 0xeb, 0xff, 0xff, 0xff, 0xc3], "the call reaches f1, 21 bytes back");

    let names: Vec<&str> = linked.names.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["dep_entry", "hook", "table"]);
    let def = |name: &str| linked.names.iter().find(|it| it.name == name).unwrap().def;
    assert_eq!(def("dep_entry"), Def { value: two.1, size: 6, kind: Kind::Function, strength: Strength::Strong, order: u32::MAX });
    let (hook, table) = (def("hook"), def("table"));
    assert_eq!((hook.size, hook.kind, hook.strength, hook.order), (1, Kind::Function, Strength::Weak, 0));
    assert_eq!((table.size, table.kind, table.strength, table.order), (8, Kind::Data, Strength::Strong, 0));
    assert_eq!(at(hook.value, 1), [0xc3], "hook is in the code");

    let (data, data_bytes) = &written[3];
    let word = |addr: u64| u64::from_le_bytes(data_bytes[(addr - data.addr) as usize..][..8].try_into().unwrap());
    assert_eq!(word(table.value), hook.value, "an address, never slid");
    assert_eq!(
        linked.binds,
        [Bind { place: table.value, name: "hook".into(), addend: 0, kind: BindKind::Addr, weak: false }],
        "a pointer to a weak definition is bound, so a strong one beside the image wins"
    );

    let static_at = |n: u8| linked.statics.iter().find(|it| it.item == ItemKey(id(n))).unwrap().clone();
    let pointers = static_at(20);
    let StaticAt::Addr(pointers_at) = pointers.at else { panic!("{pointers:?}") };
    assert_eq!(pointers.size, 16);
    assert_eq!((word(pointers_at), word(pointers_at + 8)), (one.1, table.value));
    assert_eq!(
        static_at(22),
        PlacedStatic { item: ItemKey(id(22)), instance: id(122), at: StaticAt::ThreadLocal(BIAS as u64), size: 4 },
        "the image's thread-locals start at the bias"
    );
    let StaticAt::Addr(init_at) = static_at(21).at else { panic!("the init piece is no thread-local") };
    assert_eq!(
        linked.init,
        [InitEntry { rank: 65536, order: [u32::MAX; 3], tie: tie(21, 121), at: init_at, count: 1 }]
    );
    assert!(linked.fini.is_empty());

    assert!(linked.reached_directly.is_empty(), "hook is reached only through table, a bind");
    let tls = linked.tls.unwrap();
    assert_eq!((tls.init, tls.size, tls.align), (4, 4, 4));
    assert!(data.addr <= tls.template, "the template is in the writable segment");
    assert!(linked.libraries.is_empty() && linked.unwind.is_empty() && linked.thread_vars.is_empty());

    let offsets: Vec<usize> = (1..=placed.segments.len()).map(|it| it * 8 * 4096).collect();
    let mut image = vec![0xaa; (placed.segments.len() + 1) * 8 * 4096];
    placed.write_into(&mut image, &offsets).unwrap();
    for ((segment, bytes), offset) in written.iter().zip(&offsets) {
        assert_eq!(&image[*offset..][..bytes.len()], bytes.as_slice(), "{segment:?}");
        assert!(image[*offset + bytes.len()..*offset + 4096 * 8].iter().all(|it| *it == 0xaa));
    }
    assert!(image[..offsets[0]].iter().all(|it| *it == 0xaa), "the caller's own bytes");

    let refused = |offsets: &[usize], len: usize| placed.write_into(&mut vec![0; len], offsets).unwrap_err().what;
    let mut unaligned = offsets.clone();
    unaligned[1] += 8;
    assert!(refused(&unaligned, image.len()).contains("not on a page"));
    let mut overlapping = offsets.clone();
    overlapping[1] = offsets[0];
    assert!(refused(&overlapping, image.len()).contains("overlapping"));
    assert!(refused(&offsets, offsets[offsets.len() - 1]).contains("past the buffer's end"));
    assert!(refused(&offsets[1..], image.len()).contains("offsets for"));
}

#[test]
fn a_prelink_lists_the_weak_names_its_code_reaches_directly() {
    let f1 = function(1, &CALLER, vec![rel32(1, Target::Extern("hook".into()))]);
    let f2 = function(2, &CALLER, vec![rel32(1, Target::Extern("table".into()))]);
    let object: Arc<[u8]> = hooked().into();
    let output = shared();
    let request = Request {
        output: &output,
        base: BASE,
        tls_bias: BIAS,
        functions: &[&f1, &f2],
        statics: &[],
        names: &[],
        debug_names: &[],
        natives: &[Native { id: id(60), name: "hooked.o", bytes: &object, kind: NativeKind::Object, order: 0 }],
        spans: &[],
    };
    let linked = prelink_placed(&request).unwrap().linked;
    assert_eq!(linked.reached_directly, ["hook"], "a call to the weak hook; table is strong");
    assert_eq!(linked.binds.len(), 1, "table's pointer to hook is still bound");
}

#[test]
fn a_prelink_is_of_a_whole_shared_image_on_a_page() {
    let f1 = function(1, &returns(1), Vec::new());
    let request = |output: &Output, base: u64| -> String {
        let request = Request {
            output,
            base,
            tls_bias: BIAS,
            functions: &[&f1],
            statics: &[],
            names: &[],
            debug_names: &[],
            natives: &[],
            spans: &[],
        };
        prelink_placed(&request).err().expect("refused").what
    };
    assert!(request(&output(false), BASE).contains("shared image"));
    assert!(request(&Output { incremental: true, ..shared() }, BASE).contains("linked whole"));
    assert!(request(&shared(), BASE + 8).contains("not on a page"));
}

/// The dependency image beside the workspace's: a function, a static, a thread-local and
/// names of every strength.
struct Dep;

const DEP_END: u64 = 0x7001_2345;
const DEP_TLS_END: u64 = 12;

const fn named(value: u64, strength: Strength, order: u32) -> Def {
    Def { value, size: 1, kind: Kind::Function, strength, order }
}

const DEP_NAMES: [(&str, Def); 5] = [
    ("dep_strong", named(0x7000_0200, Strength::Strong, u32::MAX)),
    ("strong_over_weak", named(0x7000_0300, Strength::Strong, 0)),
    ("weak_late", named(0x7000_0400, Strength::Weak, 1)),
    ("weak_both", named(0x7000_0500, Strength::Weak, 5)),
    ("local_strong", named(0x7000_0600, Strength::Weak, 0)),
];

impl Outside for Dep {
    fn function(&self, it: &InstanceKey) -> Option<u64> {
        (*it == key(1)).then_some(0x7000_0100)
    }

    fn static_at(&self, item: &ItemKey, instance: &ContentId) -> Option<StaticAt> {
        match (item.0, *instance) {
            (it, at) if it == id(40) && at == id(140) => Some(StaticAt::Addr(0x7000_8000)),
            (it, at) if it == id(41) && at == id(141) => Some(StaticAt::ThreadLocal(BIAS.wrapping_add(8) as u64)),
            _ => None,
        }
    }

    fn name(&self, name: &str) -> Option<Def> {
        DEP_NAMES.iter().find(|it| it.0 == name).map(|it| it.1)
    }

    fn reached_directly(&self, _: &str) -> bool {
        false
    }

    fn tls_end(&self) -> u64 {
        DEP_TLS_END
    }

    fn end(&self) -> u64 {
        DEP_END
    }
}

/// [`Dep`], its own code reaching its weak definition of one name directly.
struct Reaching(&'static str);

impl Outside for Reaching {
    fn function(&self, it: &InstanceKey) -> Option<u64> {
        Dep.function(it)
    }

    fn static_at(&self, item: &ItemKey, instance: &ContentId) -> Option<StaticAt> {
        Dep.static_at(item, instance)
    }

    fn name(&self, name: &str) -> Option<Def> {
        Dep.name(name)
    }

    fn reached_directly(&self, name: &str) -> bool {
        name == self.0
    }

    fn tls_end(&self) -> u64 {
        Dep.tls_end()
    }

    fn end(&self) -> u64 {
        Dep.end()
    }
}

/// `mov eax, fs:[disp32]; ret`, the displacement the thread-local offset of `target`.
fn thread_load(n: u8, target: Target) -> blaze_codegen::CompiledFunction {
    function(n, &[0x64, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0xc3], vec![reloc(4, RelocKind::TlsOffset, target)])
}

fn outside_static(n: u8) -> Target {
    Target::Static { item: ItemKey(id(n)), instance: id(n + 100) }
}

/// A call to each of `targets`, then `ret`.
fn calls(n: u8, targets: &[Target]) -> blaze_codegen::CompiledFunction {
    let mut code = Vec::new();
    let mut relocs = Vec::new();
    for target in targets {
        relocs.push(rel32(code.len() as u32 + 1, target.clone()));
        code.extend_from_slice(&[0xe8, 0, 0, 0, 0]);
    }
    code.push(0xc3);
    function(n, &code, relocs)
}

#[test]
fn a_workspace_link_resolves_against_the_image_beside_it_as_one_whole_link() {
    let extern_ = |name: &str| Target::Extern(name.into());
    let start = calls(10, &[
        Target::Direct(key(1)),
        extern_("dep_strong"),
        extern_("strong_over_weak"),
        extern_("weak_late"),
        extern_("weak_both"),
        extern_("local_strong"),
        extern_("arch_only"),
    ]);
    let theirs = thread_load(11, outside_static(41));
    let ours = thread_load(12, outside_static(31));
    let pointer = a_static(30, vec![0; 8], vec![reloc(0, RelocKind::Abs64, outside_static(40))]);
    let local = thread_local(31, vec![9; 8], 8);
    let object: Arc<[u8]> =
        rets(&[("weak_both", true), ("strong_over_weak", true), ("weak_late", true), ("local_strong", false)]).into();
    let (dup, only) = (rets(&[("dep_strong", false)]), rets(&[("arch_only", false)]));
    let library: Arc<[u8]> = archive(&[("dup.o", &dup, &["dep_strong"]), ("only.o", &only, &["arch_only"])]).into();
    let mut image = Image::beside(name("out/ws"), output(false), &Dep, BIAS).unwrap();
    assert_eq!(image.plan.base, 0x7001_3000, "the image starts on the page after the one beside");
    let change = Change {
        added: vec![&start, &theirs, &ours],
        statics: vec![&pointer, &local],
        names: vec![("_start".into(), Target::Direct(key(10)))],
        natives: vec![
            Native { id: id(61), name: "rets.o", bytes: &object, kind: NativeKind::Object, order: 3 },
            Native { id: id(62), name: "libdup.a", bytes: &library, kind: NativeKind::Archive { whole: false }, order: 4 },
        ],
        ..Change::default()
    };
    let staged = image.stage_beside(change, &Dep).unwrap();
    let state = &staged.state;
    let symbols = &image.symbols;
    let addr = |name: &str| state.addr_of(symbols.find_sym_name(name).unwrap()).unwrap();
    let code = state.addr_of(symbols.find_body(&key(10)).unwrap()).unwrap();
    assert!(code >= 0x7001_3000);
    let targets: Vec<u64> = (0..7).map(|it| call(state, code, 1 + 5 * it)).collect();
    assert_eq!(targets[..4], [0x7000_0100, 0x7000_0200, 0x7000_0300, 0x7000_0400], "the image beside's");
    assert_eq!(targets[4], addr("weak_both"), "of two weak ones the first on the line");
    assert_eq!(targets[5], addr("local_strong"), "a strong one beats a weak one beside");
    assert_eq!(targets[6], addr("arch_only"), "an archive member pulled for what is still undefined");
    assert!(targets[4..].iter().all(|it| *it >= 0x7001_3000));

    let at = |n: u8| state.addr_of(symbols.find_body(&key(n)).unwrap()).unwrap() + 4;
    assert_eq!(i32::try_from(BIAS + 8).unwrap(), i32_at(state, at(11)), "the image beside's thread-local");
    assert_eq!(i32::try_from(BIAS + 16).unwrap(), i32_at(state, at(12)), "ours, after its 12 bytes, 8-aligned");
    let pointer_sym = symbols.find(crate::symbols::SymKey::Static(symbols.find_id(&id(30)).unwrap(), symbols.find_id(&id(130)).unwrap()));
    assert_eq!(u64_at(state, state.addr_of(pointer_sym.unwrap()).unwrap()), 0x7000_8000, "the image beside's static");
    assert_eq!(state.tls_at, Some(BIAS + 16));

    let mut disk = Disk::default();
    let mut effects = Vec::new();
    image.commit(staged, &mut effects).unwrap();
    disk.run(&mut image, effects).unwrap();
    let layout = layout(&image);
    let names: Vec<&str> = layout.names.iter().map(|it| it.0.as_str()).collect();
    assert_eq!(names, ["_start", "arch_only", "local_strong", "weak_both"], "only what this image defines");
    let def = |name: &str| layout.names.iter().find(|it| it.0 == name).unwrap().1;
    assert_eq!(def("_start"), Def { value: code, size: 36, kind: Kind::Function, strength: Strength::Strong, order: u32::MAX });
    assert_eq!((def("weak_both").strength, def("weak_both").order), (Strength::Weak, 3));
    assert_eq!((def("local_strong").strength, def("arch_only").order), (Strength::Strong, 4));
    assert_eq!(layout.tls_offset, Some(16));
    assert_eq!(layout.tls.map(|it| (it.init, it.size, it.align)), Some((8, 8, 8)));
    assert!(layout.init.is_empty() && layout.libraries.is_empty() && layout.thread_vars.is_empty());
}

#[test]
fn two_strong_definitions_across_the_images_are_the_duplicate_symbol_error() {
    let start = calls(10, &[Target::Extern("dep_strong".into())]);
    let object: Arc<[u8]> = rets(&[("dep_strong", false)]).into();
    let mut image = Image::beside(name("out/ws"), output(false), &Dep, BIAS).unwrap();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(10)))],
        natives: vec![Native { id: id(61), name: "strong.o", bytes: &object, kind: NativeKind::Object, order: 3 }],
        ..Change::default()
    };
    let error = image.stage_beside(change, &Dep).unwrap_err();
    assert!(error.what.contains("duplicate symbol `dep_strong`") && error.what.contains("strong.o"), "{error}");

    let mut image = Image::beside(name("out/ws"), output(false), &Dep, BIAS).unwrap();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(10))), ("dep_strong".into(), Target::Direct(key(10)))],
        ..Change::default()
    };
    let error = image.stage_beside(change, &Dep).unwrap_err();
    assert!(error.what.contains("duplicate symbol `dep_strong`: Rust code defines it"), "{error}");
}

#[test]
fn a_definition_that_would_win_over_a_weak_one_the_image_beside_reaches_directly_is_an_error() {
    let start = calls(10, &[Target::Extern("local_strong".into()), Target::Extern("weak_both".into())]);
    let object: Arc<[u8]> = rets(&[("weak_both", true), ("local_strong", false)]).into();
    let link = |outside: &Reaching, names: Vec<(String, Target)>| {
        let mut image = Image::beside(name("out/ws"), output(false), outside, BIAS).unwrap();
        let change = Change {
            added: vec![&start],
            names,
            natives: vec![Native { id: id(61), name: "rets.o", bytes: &object, kind: NativeKind::Object, order: 3 }],
            ..Change::default()
        };
        image.stage_beside(change, outside).map(|_| ())
    };
    let start_name = || ("_start".to_owned(), Target::Direct(key(10)));

    let error = link(&Reaching("local_strong"), vec![start_name()]).unwrap_err();
    assert!(error.what.contains("`local_strong` is defined by rets.o in ws"), "{error}");
    assert!(error.what.contains("weakly by the dependency image"), "{error}");
    let error = link(&Reaching("weak_both"), vec![start_name()]).unwrap_err();
    assert!(error.what.contains("`weak_both` is defined by rets.o"), "a weak one first on the line too: {error}");
    let error = link(&Reaching("weak_late"), vec![start_name(), ("weak_late".into(), Target::Direct(key(10)))]);
    assert!(error.unwrap_err().what.contains("`weak_late` is defined by Rust code"));

    link(&Reaching("weak_late"), vec![start_name()]).expect("the image beside's own weak_late wins: nothing to refuse");
    link(&Reaching("dep_strong"), vec![start_name()]).expect("only a name that would lose is refused");
}

#[test]
fn an_image_beside_another_is_staged_only_against_it() {
    let start = calls(10, &[Target::Direct(key(1))]);
    let change = || Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(10)))],
        ..Change::default()
    };
    let mut image = Image::beside(name("out/ws"), output(false), &Dep, BIAS).unwrap();
    assert!(image.stage(change()).unwrap_err().what.contains("stage_beside"));
    let error = image.stage_beside(change(), &crate::NoOutside).unwrap_err();
    assert!(error.what.contains("make it again beside that one"), "{error}");
    let mut plain = Image::new(name("out/app"), output(false)).unwrap();
    assert!(plain.stage_beside(change(), &Dep).unwrap_err().what.contains("not made by `Image::beside`"));
}

#[test]
fn a_page_is_the_largest_a_system_of_the_target_maps_by() {
    use blaze_codegen::Arch;

    use crate::Format;
    use crate::premap::page;
    assert_eq!(page(Format::Elf, Arch::X86_64), 4 << 10);
    assert_eq!(page(Format::Elf, Arch::Aarch64), 64 << 10);
    assert_eq!(page(Format::MachO, Arch::Aarch64), 16 << 10);
    assert_eq!(page(Format::MachO, Arch::X86_64), 4 << 10);
    assert_eq!(page(Format::Pe, Arch::X86_64), 64 << 10);
}
