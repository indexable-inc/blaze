//! A name Rust defines weakly (`Change::weak_names`: `#[linkage = "weak"]`, as compiler-builtins'
//! intrinsics are) beside a native definition of it, resolved as ld64 and lld resolve the same
//! objects: a strong definition beats Rust's weak one and every reference binds to it, Rust's
//! own included; Rust's weak one, first on the line, beats a weak native; two strong ones are
//! the duplicate symbol error. The case is ghostty's: its `libghostty-vt.a` holds Zig's
//! `compiler_rt.o`, pulled for a symbol the program calls, and that member defines
//! `__adddf3` strongly (and hidden) where compiler-builtins defines it weakly.

use std::sync::Arc;

use blaze_codegen::Target;
use blaze_live::patch::Message;
use object::write::{Object as Builder, Symbol as Def, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
};

use super::natives::archive;
use super::{
    Change, Disk, Image, Linked, Native, NativeKind, TO, function, id, key, live, memory_of,
    name, output, patch_in, rel32, returns,
};
use crate::LinkError;

/// The intrinsic both sides define.
const INTRINSIC: &str = "__adddf3";
/// What Rust's weak copy returns.
const RUST: [u8; 6] = [0xb8, 1, 0, 0, 0, 0xc3];
/// What the archive's `other` returns: the symbol the member is pulled for.
const OTHER: u8 = 5;
/// What the archive member's copy of the intrinsic returns.
const NATIVE: u8 = 7;
const LAZY: NativeKind = NativeKind::Archive { whole: false };

/// An x86-64 ELF object defining each `(name, value, weak)` as `mov eax, value; ret`, hidden
/// (`SymbolScope::Linkage`), as Zig's compiler_rt defines its intrinsics.
fn member(defines: &[(&str, u8, bool)]) -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = b.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    for (it, value, weak) in defines {
        let at = b.append_section_data(text, &returns(*value), 16);
        b.add_symbol(Def {
            name: it.as_bytes().to_vec(),
            value: at,
            size: 6,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: *weak,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
    }
    b.write().unwrap()
}

/// `libvt.a`: one member, `compiler_rt.o`, defining `other` and, when `intrinsic` says how
/// (`Some(weak)`), the intrinsic; its index names all it defines.
fn libvt(intrinsic: Option<bool>) -> Arc<[u8]> {
    let mut defines = vec![("other", OTHER, false)];
    let mut names = vec!["other"];
    if let Some(weak) = intrinsic {
        defines.push((INTRINSIC, NATIVE, weak));
        names.push(INTRINSIC);
    }
    let bytes = member(&defines);
    archive(&[("compiler_rt.o", &bytes, &names)]).into()
}

/// `_start` (1): `call other; call __adddf3; ret`, both by name, as codegen refers to a weak
/// definition ([`blaze_codegen::Linkage::Weak`]).
fn start() -> blaze_codegen::CompiledFunction {
    let code = [0xe8, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0, 0xc3];
    let relocs = vec![
        rel32(1, Target::Extern("other".into())),
        rel32(6, Target::Extern(INTRINSIC.into())),
    ];
    function(1, &code, relocs)
}

/// Where the call whose field is at `field` goes, through the jump table's stub when it is one.
fn callee(linked: &Linked, field: u64) -> u64 {
    let to = linked.rel32(field);
    match linked.at(to, 3) == [0x4c, 0x8b, 0x1d] {
        true => linked.through_stub(to),
        false => to,
    }
}

/// What the program's two calls reach: the code at each.
fn reached(linked: &Linked) -> ([u8; 6], [u8; 6]) {
    let start = linked.symbol("start");
    let at = |field: u64| -> [u8; 6] { linked.at(callee(linked, field), 6).try_into().unwrap() };
    (at(start + 1), at(start + 6))
}

/// The first link: `_start` and Rust's copy of the intrinsic (2), named `rust_adddf3` for
/// debuggers, weak when `weak`, with `natives`.
fn first<'a>(
    start: &'a blaze_codegen::CompiledFunction,
    rust: &'a blaze_codegen::CompiledFunction,
    weak: bool,
    natives: Vec<Native<'a>>,
) -> Change<'a> {
    Change {
        added: vec![start, rust],
        names: vec![
            ("_start".into(), Target::Direct(key(1))),
            (INTRINSIC.into(), Target::Direct(key(2))),
        ],
        weak_names: if weak { vec![INTRINSIC.into()] } else { Vec::new() },
        debug_names: vec![(key(1), "start".into()), (key(2), "rust_adddf3".into())],
        natives,
        ..Change::default()
    }
}

fn lib(n: u8, bytes: &Arc<[u8]>) -> Native<'_> {
    Native { id: id(n), name: "libvt.a", bytes, kind: LAZY, order: 0 }
}

fn link_once(weak: bool, intrinsic: Option<bool>) -> Result<Linked, LinkError> {
    let (start, rust) = (start(), function(2, &RUST, Vec::new()));
    let library = libvt(intrinsic);
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &rust, weak, vec![lib(100, &library)]))?;
    Ok(Linked::of(disk.file(TO)))
}

#[test]
fn a_strong_definition_in_a_member_pulled_for_another_symbol_beats_rusts_weak_one() {
    let linked = link_once(true, Some(false)).unwrap();
    let (other, intrinsic) = reached(&linked);
    assert_eq!(other, returns(OTHER), "the member, pulled for `other`");
    assert_eq!(
        intrinsic,
        returns(NATIVE),
        "Rust's call by name binds to the member's strong `{INTRINSIC}`, not Rust's weak copy"
    );
}

#[test]
fn rusts_weak_definition_beats_a_weak_native_one_after_it_on_the_line() {
    let linked = link_once(true, Some(true)).unwrap();
    let (other, intrinsic) = reached(&linked);
    assert_eq!(other, returns(OTHER));
    assert_eq!(intrinsic, RUST, "of two weak ones the first on the line, Rust's");
}

#[test]
fn a_weak_rust_name_alone_is_rusts() {
    let linked = link_once(true, None).unwrap();
    assert_eq!(reached(&linked), (returns(OTHER), RUST));
}

#[test]
fn two_strong_definitions_are_still_the_duplicate_symbol_error() {
    let error = link_once(false, Some(false)).map(drop).unwrap_err();
    assert!(
        error.what.contains("duplicate symbol `__adddf3`: Rust code defines it")
            && error.what.contains("libvt.a"),
        "{error}"
    );
    let linked = link_once(false, Some(true)).unwrap();
    assert_eq!(reached(&linked).1, RUST, "a strong Rust name beats a weak native one");
}

#[test]
fn a_weak_name_the_change_does_not_name_is_refused() {
    let start = start();
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        weak_names: vec![INTRINSIC.into()],
        ..Change::default()
    };
    let error = image.stage(change).map(drop).unwrap_err();
    assert!(
        error.what.contains("`__adddf3` is among the change's weak names, and none of its names"),
        "{error}"
    );
}

/// An incremental image whose archive gains the strong definition, then loses it: each link
/// binds the program's call as a whole link of its inputs would, the callers rewritten.
#[test]
fn an_outside_strong_definition_coming_and_going_rebinds_rusts_references() {
    let (start, rust) = (start(), function(2, &RUST, Vec::new()));
    let (without, with) = (libvt(None), libvt(Some(false)));
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &rust, true, vec![lib(100, &without)])).unwrap();
    assert_eq!(reached(&Linked::of(disk.file(TO))), (returns(OTHER), RUST));

    let gain = Change {
        natives: vec![lib(101, &with)],
        removed_natives: vec![id(100)],
        ..Change::default()
    };
    disk.link(&mut image, gain).unwrap();
    assert_eq!(
        reached(&Linked::of(disk.file(TO))),
        (returns(OTHER), returns(NATIVE)),
        "the archive's strong definition, now on the line, wins"
    );

    let lose = Change {
        natives: vec![lib(100, &without)],
        removed_natives: vec![id(101)],
        ..Change::default()
    };
    disk.link(&mut image, lose).unwrap();
    assert_eq!(
        reached(&Linked::of(disk.file(TO))),
        (returns(OTHER), RUST),
        "with it gone, Rust's weak copy again"
    );
}

/// One live round: `add` (its id and bytes) on the line in place of `remove`, committed into
/// `memory`; what the new image's calls reach.
fn round(
    image: &mut Image,
    memory: &mut blaze_inproc::CodeMemory,
    add: u8,
    bytes: &Arc<[u8]>,
    remove: u8,
) -> ([u8; 6], [u8; 6]) {
    let change = Change {
        natives: vec![lib(add, bytes)],
        removed_natives: vec![id(remove)],
        ..Change::default()
    };
    let staged = image.stage(change).unwrap_or_else(|it| panic!("staged: {it}"));
    let message = image
        .commit_into(staged, memory)
        .unwrap_or_else(|it| panic!("committed: {it}"))
        .expect("a message for the programs running the image before");
    match Message::decode(&message) {
        Ok(Message::Patch(_)) => {
            patch_in(&message, memory);
        }
        Ok(Message::Restart { why, .. }) => assert!(!why.is_empty(), "a restart says why"),
        other => panic!("neither a patch nor a restart: {other:?}"),
    }
    reached(&Linked::in_memory(memory, image.state.file_len))
}

/// The same in a live image: the program running the image before is told either the patch
/// that rebinds its call or the restart that names why it can't take it, and the image a
/// program started now runs binds as a whole link would.
#[test]
fn a_live_image_gaining_the_strong_definition_patches_or_names_its_restart() {
    let (start, rust) = (start(), function(2, &RUST, Vec::new()));
    let (without, with) = (libvt(None), libvt(Some(false)));
    let mut image = Image::new(name(TO), live()).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &rust, true, vec![lib(100, &without)])).unwrap();
    assert_eq!(reached(&Linked::of(disk.file(TO))), (returns(OTHER), RUST));

    let Some(mut memory) = memory_of("blaze-link-weak-names", disk.file(TO)) else { return };
    assert_eq!(
        round(&mut image, &mut memory, 101, &with, 100),
        (returns(OTHER), returns(NATIVE)),
        "gained: the strong definition"
    );
    assert_eq!(
        round(&mut image, &mut memory, 100, &without, 101),
        (returns(OTHER), RUST),
        "lost: Rust's weak copy again"
    );
}
