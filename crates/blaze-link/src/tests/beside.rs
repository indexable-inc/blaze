//! An image with its debug info beside it (docs/v2/plan/design/debug-side-file.md): the
//! executable carries none and names its side file, the side file resolves every function
//! and row as the same program linked with its debug info inside does, through an edit, and
//! an image's build id is of what it is, not of where it is written.

use std::collections::BTreeSet;

use blaze_codegen::{CompiledFunction, Line, Target};
use blaze_format::SpanRef;
use object::{Object as _, ObjectSection as _};

use super::{
    Change, Debug, Disk, Format, Image, Linked, Output, Position, SpanTable, Sparse, TO, function,
    id, name, output, wide_key,
};

/// Each function by name with its rows as (offset in the function, file, line): what a
/// debugger resolves, whatever addresses the image gave.
type Resolved = BTreeSet<(String, u64, String, u64)>;

/// The DWARF sections `get` gives by name, resolved.
fn resolved<'a>(get: impl Fn(&str) -> &'a [u8]) -> Resolved {
    let dwarf = gimli::Dwarf::load(|id| -> Result<_, ()> {
        Ok(gimli::EndianSlice::new(get(id.name()), gimli::LittleEndian))
    })
    .unwrap();
    let unit = dwarf.unit(dwarf.units().next().unwrap().unwrap()).unwrap();
    let mut functions = Vec::new();
    let mut entries = unit.entries();
    while let Some(entry) = entries.next_dfs().unwrap() {
        if entry.tag() != gimli::DW_TAG_subprogram {
            continue;
        }
        let Some(gimli::AttributeValue::Addr(low)) = entry.attr_value(gimli::DW_AT_low_pc) else {
            panic!("a subprogram with no address")
        };
        let size = entry.attr_value(gimli::DW_AT_high_pc).and_then(|it| it.udata_value()).unwrap();
        let name = entry
            .attr_value(gimli::DW_AT_linkage_name)
            .map(|it| dwarf.attr_string(&unit, it).unwrap().to_string_lossy().into_owned());
        if size != 0 {
            functions.push((low, size, name.expect("every function here is named")));
        }
    }
    let mut out = Resolved::new();
    for (_, _, name) in &functions {
        out.insert((name.clone(), u64::MAX, String::new(), 0));
    }
    let mut rows = unit.line_program.clone().unwrap().rows();
    while let Some((header, row)) = rows.next_row().unwrap() {
        if row.end_sequence() {
            continue;
        }
        let file = header.file(row.file_index()).unwrap();
        let path =
            dwarf.attr_string(&unit, file.path_name()).unwrap().to_string_lossy().into_owned();
        let (low, _, name) = functions
            .iter()
            .find(|it| (it.0..it.0 + it.1).contains(&row.address()))
            .unwrap_or_else(|| panic!("a row at {:#x}, in no function", row.address()));
        out.insert((name.clone(), row.address() - low, path, row.line().unwrap().get()));
    }
    out
}

/// `count` functions that call nothing, function `n` with rows in two files, and the span
/// table they name.
fn program(count: u32) -> (Vec<CompiledFunction>, Vec<u8>) {
    let spans = SpanTable {
        files: vec!["src/main.rs".into(), "src/lib.rs".into()],
        spans: (0..16).map(|it| Position { file: it % 2, line: 3 + it * 5, column: 1 }).collect(),
    }
    .encode();
    let functions = (0..count).map(|n| lined(n, n)).collect();
    (functions, spans)
}

/// Function `n` whose rows are spans `seed` and `seed + 1` of the table.
fn lined(n: u32, seed: u32) -> CompiledFunction {
    // `mov eax, n; ret`: each function its own code.
    let code = [0xb8, n as u8, (n >> 8) as u8, 0, 0, 0xc3];
    let mut it = CompiledFunction { key: wide_key(n), ..function(1, &code, Vec::new()) };
    it.lines = vec![
        Line { at: 0, span: SpanRef { anchor: id(70), index: seed % 16 } },
        Line { at: 5, span: SpanRef { anchor: id(70), index: (seed + 1) % 16 } },
    ];
    it
}

fn first<'a>(functions: &'a [CompiledFunction], spans: &'a [u8]) -> Change<'a> {
    Change {
        added: functions.iter().collect(),
        names: vec![("_start".into(), Target::Direct(functions[0].key))],
        debug_names: functions.iter().enumerate().map(|(n, it)| (it.key, format!("f{n}"))).collect(),
        spans: vec![(id(70), spans)],
        ..Change::default()
    }
}

/// An image of `debug` linked from `change`, with its disk.
fn linked(debug: Debug, to: &str, change: Change<'_>) -> (Image, Disk) {
    let mut image = Image::new(name(to), Output { debug, ..output(true) }).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, change).unwrap();
    (image, disk)
}

/// A side file's bytes as `object` reads them (from 8-aligned words).
fn side_file(side: &[u8]) -> Linked {
    let mut file = Sparse::default();
    file.write(0, side);
    Linked::of(&file)
}

/// What the image's file at `to` resolves from its own debug sections.
fn inside(disk: &Disk, to: &str) -> Resolved {
    let linked = Linked::of(disk.file(to));
    let elf = linked.elf();
    resolved(|name| elf.section_by_name(name).map_or(&[][..], |it| it.data().unwrap()))
}

/// What a side file resolves, after checking it is the executable's at `to`: its build id
/// note is the image's, and its CRC-32 is the one the executable's `.gnu_debuglink` names
/// under the side file's name.
fn beside(image: &Image, disk: &Disk, to: &str, side: &[u8]) -> Resolved {
    let exe = Linked::of(disk.file(to));
    let exe = exe.elf();
    assert!(
        exe.sections().all(|it| !it.name().unwrap().starts_with(".debug_")),
        "the executable carries debug info"
    );
    let link = exe.section_by_name(".gnu_debuglink").expect("the executable names its side file");
    let link = link.data().unwrap();
    assert_eq!(&link[..10], b"app.debug\0");
    let crc = u32::from_le_bytes(link[link.len() - 4..].try_into().unwrap());
    assert_eq!(crate::debuglink_crc32(side), crc, "the side file's CRC is the one the executable names");
    assert_eq!(crate::debug_file_id(Format::Elf, side), Some(image.build_id()));
    let note = exe.section_by_name(".note.gnu.build-id").unwrap();
    let read = side_file(side);
    let elf = read.elf();
    let held = elf.section_by_name(".note.gnu.build-id").unwrap();
    assert_eq!(held.data().unwrap(), note.data().unwrap(), "the build id note is the executable's");
    for section in exe.sections().filter(|it| it.address() != 0) {
        let named = (section.name().unwrap(), section.address(), section.size());
        assert!(
            elf.sections().any(|it| (it.name().unwrap(), it.address(), it.size()) == named),
            "the side file lacks the executable's section {named:?}"
        );
    }
    // The executable's program headers, each where it is in memory and with nothing in the
    // file but the build id's note: what `objcopy --only-keep-debug` leaves.
    let (exe_bytes, side_bytes) = (Linked::of(disk.file(to)), side_file(side));
    let headers = |bytes: &[u8]| -> Vec<[u64; 5]> {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let count = usize::from(u16::from_le_bytes(bytes[0x38..0x3a].try_into().unwrap()));
        (0..count)
            .map(|nth| word(0x20) as usize + nth * 56)
            .map(|at| [word(at) & 0xffff_ffff, word(at + 16), word(at + 40), word(at + 32), word(at + 8)])
            .collect()
    };
    let (of_exe, of_side) = (headers(exe_bytes.bytes()), headers(side_bytes.bytes()));
    assert!(!of_exe.is_empty() && of_exe.len() == of_side.len());
    for (exe, side) in of_exe.iter().zip(&of_side) {
        assert_eq!(exe[..2], side[..2], "a segment's type and address");
        match (side[0], side[3]) {
            (_, 0) => assert_eq!(exe[2], side[2], "its size in memory"),
            (4, len) => {
                let at = side[4] as usize;
                assert_eq!(&side_bytes.bytes()[at + 16..at + len as usize], &image.build_id()[..]);
            }
            (kind, len) => panic!("segment {kind} has {len} bytes in the side file"),
        }
    }
    assert_eq!(
        elf.section_by_name(".symtab").unwrap().data().unwrap(),
        exe.section_by_name(".symtab").unwrap().data().unwrap(),
        "the side file's symbols are the executable's"
    );
    resolved(|name| elf.section_by_name(name).map_or(&[][..], |it| it.data().unwrap()))
}

/// The refinement: an executable and its side file resolve every function's rows to the
/// same (function, file, line) as the same program linked with its debug info inside, at
/// its first link, after a body edit taken as a patch of both, and after a rollback.
#[test]
fn an_image_and_its_side_file_resolve_as_debug_info_inside() {
    let (functions, spans) = program(12);
    let (mut with, mut with_disk) = linked(Debug::Lines, TO, first(&functions, &spans));
    let (mut image, mut disk) = linked(Debug::Beside, TO, first(&functions, &spans));
    let write = image.debug_file().unwrap().expect("an image with its debug info beside it");
    assert_eq!((write.name.as_str(), write.base), ("app.debug", None));
    let side = write.apply(Format::Elf, None).unwrap();
    let want = inside(&with_disk, TO);
    assert_eq!(want.len(), 12 * 3, "every function and both rows of each");
    assert_eq!(beside(&image, &disk, TO, &side), want);

    // Function 5 edited: another body, other lines.
    let edited = CompiledFunction { key: wide_key(100), ..lined(5, 9) };
    let edit = || Change {
        added: vec![&edited],
        removed: vec![wide_key(5)],
        debug_names: vec![(wide_key(100), "f5".into())],
        ..Change::default()
    };
    with_disk.link(&mut with, edit()).unwrap();
    disk.link(&mut image, edit()).unwrap();
    let write = image.debug_file().unwrap().unwrap();
    assert_eq!((write.whole, write.records), (None, 2), "the function made and the one it replaced");
    assert_eq!((write.all, write.encoded), (None, 1), "the update followed the link's change");
    assert_eq!(write.base, crate::debug_file_id(Format::Elf, &side));
    let patched = write.apply(Format::Elf, Some(&side)).unwrap();
    let want = inside(&with_disk, TO);
    assert_eq!(beside(&image, &disk, TO, &patched), want);
    assert!(want.contains(&("f5".into(), 0, "src/lib.rs".into(), 3 + 9 * 5)));

    // Rolled back: the side file follows the image with nothing said, every function compared.
    with_disk.roll_back(&mut with).unwrap();
    disk.roll_back(&mut image).unwrap();
    let write = image.debug_file().unwrap().unwrap();
    assert!(write.all.is_some() && write.encoded == 12, "{:?}, {} encoded", write.all, write.encoded);
    let back = write.apply(Format::Elf, Some(&patched)).unwrap();
    assert_eq!(beside(&image, &disk, TO, &back), inside(&with_disk, TO));

    // The side file gone from the disk: the patch is refused, and the whole file is made.
    let write = image.debug_file().unwrap().unwrap();
    assert!(write.apply(Format::Elf, None).is_err());
    let whole = image.debug_file_whole().unwrap().unwrap();
    assert_eq!(whole.base, None);
    assert_eq!(beside(&image, &disk, TO, &whole.apply(Format::Elf, None).unwrap()), inside(&with_disk, TO));
}

/// Counter: the executable of a program with its debug info beside it has no section that
/// grows with the debug info: none named `.debug_*`, and a `.gnu_debuglink` of the same
/// bytes' length however many functions it has; the side file is what grows.
#[test]
fn the_executable_has_no_section_that_grows_with_debug_info() {
    let mut sizes = Vec::new();
    for count in [4u32, 400] {
        let (functions, spans) = program(count);
        let (mut image, disk) = linked(Debug::Beside, TO, first(&functions, &spans));
        let (_, none) = linked(Debug::None, TO, first(&functions, &spans));
        let exe = Linked::of(disk.file(TO));
        let exe = exe.elf();
        assert!(exe.sections().all(|it| !it.name().unwrap().starts_with(".debug_")));
        let link = exe.section_by_name(".gnu_debuglink").unwrap().size();
        let more = disk.file(TO).len - none.file(TO).len;
        let side = image.debug_file().unwrap().unwrap().len;
        sizes.push((link, more, side));
    }
    assert_eq!(sizes[0].0, sizes[1].0, "the name of the side file and its CRC");
    assert!(sizes[0].1 < 256 && sizes[1].1 < 256, "past an image with no debug info: {sizes:?}");
    assert!(sizes[1].2 > sizes[0].2, "the side file holds the debug info: {sizes:?}");
}

/// `split-debuginfo = "off"`: the debug info is inside, laid out as before, and the same
/// program linked twice is the same bytes.
#[test]
fn debug_info_inside_is_the_same_file_twice() {
    let (functions, spans) = program(40);
    let (mut a, disk_a) = linked(Debug::Lines, TO, first(&functions, &spans));
    let (_, disk_b) = linked(Debug::Lines, TO, first(&functions, &spans));
    assert!(disk_a.file(TO).same(disk_b.file(TO)), "two links of one program differ");
    let exe = Linked::of(disk_a.file(TO));
    let exe = exe.elf();
    for section in [".debug_abbrev", ".debug_info", ".debug_line", ".debug_str"] {
        assert!(exe.section_by_name(section).is_some_and(|it| it.size() > 0), "{section}");
    }
    assert!(exe.section_by_name(".gnu_debuglink").is_none());
    assert_eq!(a.debug_file().unwrap(), None, "an image with its debug info inside has no side file");
}

/// One program linked for two directories is one image: the same build id and the same
/// bytes, so its file is placed at both. A program of another name is another.
#[test]
fn an_images_build_id_is_of_the_program_not_of_its_directory() {
    let (functions, spans) = program(6);
    for debug in [Debug::Beside, Debug::Lines] {
        let (out, out_disk) = linked(debug, "build/app/0123/out/app", first(&functions, &spans));
        let (top, top_disk) = linked(debug, "debug/app", first(&functions, &spans));
        assert_eq!(out.build_id(), top.build_id());
        assert_eq!(out.lineage(), top.lineage());
        assert!(out_disk.file("build/app/0123/out/app").same(top_disk.file("debug/app")));
        let (other, _) = linked(debug, "debug/other", first(&functions, &spans));
        assert_ne!(out.build_id(), other.build_id(), "the file's name is in the file");
        // Two images of one lineage that hold other things (the same program, edited apart in
        // two checkouts) are told apart by what their caller says they hold.
        let holding = |n: u8| Change { given: Some(id(n)), ..first(&functions, &spans) };
        let (a, _) = linked(debug, "debug/app", holding(1));
        let (b, _) = linked(debug, "debug/app", holding(2));
        let (again, _) = linked(debug, "debug/app", holding(1));
        assert_eq!(a.lineage(), b.lineage());
        assert_ne!(a.build_id(), b.build_id(), "one build id for two programs");
        assert_eq!(a.build_id(), again.build_id());
    }
}
