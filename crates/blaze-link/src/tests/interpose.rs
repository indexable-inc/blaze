//! What an ELF executable exports for the shared libraries it links, as GNU ld and lld
//! export it: each name the executable defines, by Rust or by an object, that a library on
//! its line also defines or leaves undefined. The case is a program's own allocator:
//! `malloc` and `free` defined in the executable replace libc's only when the executable's
//! `.dynsym` has them, since the loader binds libc's own calls (`strdup`'s) through it. A
//! name no library names, and one that is hidden, stay out.

use std::sync::Arc;

use blaze_codegen::Target;
use object::read::elf::Sym as _;
use object::write::{Object as Builder, Symbol as Def, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, Object as _, ObjectSymbol as _, SectionKind,
    SymbolFlags, SymbolKind, SymbolScope, elf,
};

use super::{
    CALLER, Change, Disk, Image, Linked, Native, NativeKind, TO, contents, function, id, key,
    name, output, rel32, returns,
};
use crate::{Output, OutputKind};

const SHARED: NativeKind = NativeKind::Shared { as_needed: true };
/// Default visibility, and hidden, as `object` writes them.
const DEFAULT: SymbolScope = SymbolScope::Dynamic;
const HIDDEN: SymbolScope = SymbolScope::Linkage;

/// The shared library `soname`, linked by the linker itself: it exports each of `exports`
/// (returning 0). With `refers`, a name and the library defining it, its first export calls
/// that name, which its `.dynsym` then leaves undefined.
fn library(soname: &str, exports: &[&str], refers: Option<(&str, &Arc<[u8]>)>) -> Arc<[u8]> {
    let kind = OutputKind::Shared {
        name: soname.into(),
        exports: exports.iter().map(|it| (*it).to_owned()).collect(),
    };
    let to = format!("out/{soname}");
    let mut image =
        Image::new(name(&to), Output { kind, interpreter: None, ..output(false) }).unwrap();
    let functions: Vec<blaze_codegen::CompiledFunction> = (0..exports.len() as u8)
        .map(|at| match refers {
            Some((callee, _)) if at == 0 => {
                function(40, &CALLER, vec![rel32(1, Target::Extern(callee.into()))])
            }
            _ => function(40 + at, &returns(0), Vec::new()),
        })
        .collect();
    let names = exports
        .iter()
        .enumerate()
        .map(|(at, it)| ((*it).to_owned(), Target::Direct(key(40 + at as u8))))
        .collect();
    let natives = refers
        .iter()
        .map(|&(_, bytes)| Native { id: id(90), name: "libbeside.so", bytes, kind: SHARED, order: 0 })
        .collect();
    let mut disk = Disk::default();
    let change =
        Change { added: functions.iter().collect(), names, natives, ..Change::default() };
    disk.link(&mut image, change).unwrap_or_else(|it| panic!("{soname}: {it}"));
    contents(disk.file(&to)).into()
}

/// An x86-64 ELF object defining each `(name, value, scope, weak)` as `mov eax, value; ret`
/// in its own `.text.<name>` section, so collection keeps each for itself.
fn object(defines: &[(&str, u8, SymbolScope, bool)]) -> Arc<[u8]> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    for (it, value, scope, weak) in defines {
        let text = b.add_section(Vec::new(), format!(".text.{it}").into_bytes(), SectionKind::Text);
        let at = b.append_section_data(text, &returns(*value), 16);
        b.add_symbol(Def {
            name: it.as_bytes().to_vec(),
            value: at,
            size: 6,
            kind: SymbolKind::Text,
            scope: *scope,
            weak: *weak,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
    }
    b.write().unwrap().into()
}

/// What the loader finds `name` as through the image's `.gnu.hash`: a defined symbol's
/// address, size, type and whether it is weak.
fn found(linked: &Linked, name: &str) -> Option<(u64, u64, elf::SymbolType, bool)> {
    let file = linked.elf();
    let (endian, data) = (file.endian(), file.data());
    let sections = file.elf_section_table();
    let dynsyms = sections.symbols(endian, data, elf::SHT_DYNSYM).unwrap();
    let versions = sections.versions(endian, data).unwrap().unwrap();
    let (hash, _) = sections.gnu_hash(endian, data).unwrap().unwrap();
    let bytes = name.as_bytes();
    let (_, symbol) = hash.find(endian, bytes, elf::gnu_hash(bytes), None, &dynsyms, &versions)?;
    assert!(!symbol.is_undefined(endian), "`{name}` is in the hash table, undefined");
    Some((
        symbol.st_value(endian),
        symbol.st_size(endian),
        symbol.st_type(),
        symbol.st_bind() == elf::STB_WEAK,
    ))
}

/// The code an exported address is: itself, or, in an incremental image, where its jump
/// table's stub goes.
fn code(linked: &Linked, addr: u64) -> [u8; 6] {
    let at = match linked.at(addr, 3) == [0x4c, 0x8b, 0x1d] {
        true => linked.through_stub(addr),
        false => addr,
    };
    linked.at(at, 6).try_into().unwrap()
}

/// Whether `.dynsym` has `name` at all, defined or not.
fn dynamic(linked: &Linked, name: &str) -> bool {
    linked.elf().dynamic_symbols().any(|it| it.name().ok() == Some(name))
}

/// What the executable's object defines: `malloc`, a weak `free`, a hidden `calloc`, `hook`
/// (which libc only refers to) and `only_ours` (which no library names).
fn allocator() -> Arc<[u8]> {
    object(&[
        ("malloc", 11, DEFAULT, false),
        ("free", 12, DEFAULT, true),
        ("calloc", 13, HIDDEN, false),
        ("hook", 14, DEFAULT, false),
        ("only_ours", 15, DEFAULT, false),
    ])
}

/// A libc: it defines the allocator and `strdup`, and its `malloc` calls `hook`, which it
/// leaves to whoever defines it.
fn libc() -> Arc<[u8]> {
    let beside = library("libhook.so", &["hook"], None);
    library(
        "libc.so.6",
        &["malloc", "free", "calloc", "realloc", "strdup"],
        Some(("hook", &beside)),
    )
}

/// `_start` (1), Rust's `realloc` (2) and Rust's `rust_only` (3), which no library names.
fn rust() -> [blaze_codegen::CompiledFunction; 3] {
    [
        function(1, &returns(0), Vec::new()),
        function(2, &returns(16), Vec::new()),
        function(3, &returns(17), Vec::new()),
    ]
}

fn first<'a>(
    rust: &'a [blaze_codegen::CompiledFunction; 3],
    natives: Vec<Native<'a>>,
) -> Change<'a> {
    Change {
        added: rust.iter().collect(),
        names: vec![
            ("_start".into(), Target::Direct(key(1))),
            ("realloc".into(), Target::Direct(key(2))),
            ("rust_only".into(), Target::Direct(key(3))),
        ],
        debug_names: vec![(key(1), "start".into())],
        natives,
        ..Change::default()
    }
}

/// A libc on the line after the object, as the input `n`.
fn libc_at(n: u8, bytes: &Arc<[u8]>) -> Native<'_> {
    Native { id: id(n), name: "libc.so.6", bytes, kind: SHARED, order: 1 }
}

fn natives<'a>(object: &'a Arc<[u8]>, n: u8, library: &'a Arc<[u8]>) -> Vec<Native<'a>> {
    let object =
        Native { id: id(100), name: "allocator.o", bytes: object, kind: NativeKind::Object, order: 0 };
    vec![object, libc_at(n, library)]
}

/// The exports of a whole link with libc on the line: each name the libraries define or
/// refer to, at the executable's definition.
fn assert_interposed(linked: &Linked) {
    // Defined by an object and by libc; weak stays weak; the object's size and type.
    for (name, value, weak) in [("malloc", 11, false), ("free", 12, true), ("hook", 14, false)] {
        let (addr, size, kind, is_weak) =
            found(linked, name).unwrap_or_else(|| panic!("`{name}` is not in the hash table"));
        assert_eq!(code(linked, addr), returns(value), "`{name}` is the executable's");
        assert_eq!((size, kind, is_weak), (6, elf::STT_FUNC, weak), "`{name}`");
    }
    // Defined by Rust and by libc.
    let (addr, _, kind, weak) = found(linked, "realloc").expect("`realloc` is in the hash table");
    assert_eq!(code(linked, addr), returns(16), "`realloc` is Rust's");
    assert_eq!((kind, weak), (elf::STT_FUNC, false));
    // No library names these; `calloc` is hidden; `strdup` is only libc's.
    for name in ["only_ours", "rust_only", "_start", "calloc", "strdup"] {
        assert!(found(linked, name).is_none(), "`{name}` is in the hash table");
        assert!(!dynamic(linked, name), "`{name}` is in `.dynsym`");
    }
}

/// An executable defining `malloc` and `free` beside a libc that defines them exports both,
/// and with them what Rust defines of libc's names and what libc leaves undefined; nothing
/// in the image calls any of them, so each is kept for the loader alone. Two links of the
/// same inputs are the same bytes.
#[test]
fn an_executable_exports_what_its_shared_libraries_name() {
    let (object, libc, rust) = (allocator(), libc(), rust());
    let link = || {
        let mut image = Image::new(name(TO), output(false)).unwrap();
        let mut disk = Disk::default();
        disk.link(&mut image, first(&rust, natives(&object, 101, &libc))).unwrap();
        contents(disk.file(TO))
    };
    let (a, b) = (link(), link());
    assert!(a == b, "two links of the same inputs differ");
    let mut file = super::Sparse::default();
    file.write(0, &a);
    assert_interposed(&Linked::of(&file));
}

/// A name hidden by any object that mentions it is hidden (ELF takes the most constraining
/// visibility), and is not exported though its definition is default.
#[test]
fn a_name_another_object_hides_is_not_exported() {
    let (object, libc, rust) = (allocator(), libc(), rust());
    // `malloc`, undefined and hidden: a reference from an object built with
    // `-fvisibility=hidden` declarations.
    let hides: Arc<[u8]> = {
        let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        b.add_symbol(Def {
            name: b"malloc".to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Unknown,
            scope: HIDDEN,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        b.write().unwrap().into()
    };
    let mut on_line = natives(&object, 101, &libc);
    on_line.push(Native {
        id: id(102),
        name: "hides.o",
        bytes: &hides,
        kind: NativeKind::Object,
        order: 2,
    });
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&rust, on_line)).unwrap();
    let linked = Linked::of(disk.file(TO));
    assert!(!dynamic(&linked, "malloc"), "a hidden `malloc` is in `.dynsym`");
    assert!(found(&linked, "free").is_some(), "`free`, which nothing hides, is exported");
}

/// The exports are the graph's: an incremental image whose library changes exports what a
/// whole link of its new inputs exports, never the list it had. A library that names none
/// of the executable's names gives none; libc in its place gives them all; and back.
#[test]
fn a_relink_exports_what_its_new_libraries_name() {
    let (object, libc, rust) = (allocator(), libc(), rust());
    let other = library("libother.so", &["strdup"], None);
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&rust, natives(&object, 101, &other))).unwrap();
    let none = |linked: &Linked| {
        for name in ["malloc", "free", "realloc", "hook", "calloc", "only_ours", "rust_only"] {
            assert!(!dynamic(linked, name), "`{name}` is in `.dynsym`, and no library names it");
        }
    };
    none(&Linked::of(disk.file(TO)));

    let gain = Change {
        natives: vec![libc_at(103, &libc)],
        removed_natives: vec![id(101)],
        ..Change::default()
    };
    disk.link(&mut image, gain).unwrap();
    assert_interposed(&Linked::of(disk.file(TO)));

    let lose = Change {
        natives: vec![libc_at(101, &other)],
        removed_natives: vec![id(103)],
        ..Change::default()
    };
    disk.link(&mut image, lose).unwrap();
    none(&Linked::of(disk.file(TO)));
}

/// The exports follow Rust's names too: a `malloc` Rust comes to define, weakly, beside
/// libc is exported weak by the link that gains it.
#[test]
fn a_relink_exports_a_name_rust_comes_to_define() {
    let (libc, rust) = (libc(), rust());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&rust, vec![libc_at(101, &libc)])).unwrap();
    let before = Linked::of(disk.file(TO));
    assert!(found(&before, "realloc").is_some() && !dynamic(&before, "malloc"));

    let malloc = function(4, &returns(18), Vec::new());
    let gain = Change {
        added: vec![&malloc],
        names: vec![("malloc".into(), Target::Direct(key(4)))],
        weak_names: vec!["malloc".into()],
        ..Change::default()
    };
    disk.link(&mut image, gain).unwrap();
    let after = Linked::of(disk.file(TO));
    let (addr, _, kind, weak) = found(&after, "malloc").expect("`malloc` is in the hash table");
    assert_eq!(code(&after, addr), returns(18), "`malloc` is Rust's");
    assert_eq!((kind, weak), (elf::STT_FUNC, true));
    assert!(found(&after, "realloc").is_some(), "`realloc` is still exported");
}
