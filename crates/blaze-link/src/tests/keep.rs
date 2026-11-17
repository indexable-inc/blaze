//! An image's state kept as bytes and opened again (`keep.rs`): an opened image stages and
//! commits as the image that kept it would, byte for byte in its file and its kept state, and
//! a state that is not as written is refused by name, never opened as something else.

use std::alloc::Global;
use std::mem::offset_of;

use blaze_core::Mapping;
use blaze_format::Header;
use blaze_sim::{Config, Tape};

use super::patch::{Built, Op, Plant, Program, built, change, op};
use super::{Format, Image, Output, TO, name, output};
use crate::keep::{KeepRefusal, Section, Verified, first_difference, verify};
use crate::output::Platform;

/// The ELF image the tests link: an x86-64 executable with an interpreter (its `.rela.dyn` is
/// patched by slot).
fn elf() -> Output {
    output(true)
}

/// The same program as a Mach-O x86-64 executable: the patch model's code is x86-64, and the
/// writer is the same on every host, so this runs wherever the tests do.
fn macho() -> Output {
    Output {
        format: Format::MachO,
        interpreter: None,
        platform: Some(Platform { platform: 1, minimum: 0x000b_0000, sdk: 0x000f_0000 }),
        triple: "x86_64-apple-darwin".into(),
        ..output(true)
    }
}

/// Links `to` as a change of the program `from` was built from, and gives the whole file.
fn link(image: &mut Image, from: Option<&Built>, to: &Built) -> Result<Vec<u8>, String> {
    let staged = image.stage(change(from, to, Plant::None)).map_err(|it| format!("the stage: {it}"))?;
    image.bytes(staged).map_err(|it| format!("the commit: {it}"))
}

/// The image's kept state, verified as a later command would.
fn kept(image: &Image) -> Result<Verified, String> {
    let bytes = image.keep(Global).map_err(|it| format!("the keep: {it}"))?;
    verify(TO, Mapping::copy(&bytes)).map_err(|it| format!("the verify: {it}"))
}

/// `Err` naming the first section in which the two images' kept states differ.
fn same_state(a: &Image, b: &Image, when: &str) -> Result<(), String> {
    match first_difference(&kept(a)?, &kept(b)?) {
        None => Ok(()),
        Some(section) => Err(format!("{when}: the kept states differ in the section {section}")),
    }
}

/// The little-endian word of `len` bytes at `from` in `file`.
fn word(file: &[u8], from: usize, len: usize) -> Option<u64> {
    let bytes = file.get(from..from + len)?;
    Some(bytes.iter().rev().fold(0u64, |value, byte| value << 8 | u64::from(*byte)))
}

/// What byte `at` of the file `file` (written by `image`) is: its section, by the file's own
/// headers, the offset in it, and the piece of `image`'s state at that address, so a failure
/// names the region and the piece that differ.
fn whose(image: &Image, file: &[u8], at: usize) -> String {
    match file.get(..4) {
        Some(b"\x7fELF") => whose_elf(image, file, at),
        Some([0xcf, 0xfa, 0xed, 0xfe]) => whose_macho(image, file, at),
        _ => "in a file neither ELF nor Mach-O".to_owned(),
    }
}

/// The piece of `image`'s state that holds `address`, or that none does.
fn piece_at(image: &Image, address: u64) -> String {
    image
        .state
        .live_pieces()
        .find(|it| it.hot().addr <= address && address < it.hot().addr + it.hot().size)
        .map_or_else(
            || "in no piece (the writer's own bytes)".to_owned(),
            |it| format!("in {} +{:#x}", image.symbols.describe(it.hot().sym), address - it.hot().addr),
        )
}

/// [`whose`] for a Mach-O file: the section, by its `LC_SEGMENT_64` commands.
fn whose_macho(image: &Image, file: &[u8], at: usize) -> String {
    let word = |from: usize, len: usize| word(file, from, len);
    let name = |from: usize| {
        let bytes = file.get(from..from + 16).unwrap_or_default();
        String::from_utf8_lossy(&bytes[..bytes.iter().position(|it| *it == 0).unwrap_or(bytes.len())]).into_owned()
    };
    let Some(count) = word(16, 4) else { return "in a Mach-O file with no header".to_owned() };
    let mut command = 32usize;
    for _ in 0..count {
        let (Some(kind), Some(size)) = (word(command, 4), word(command + 4, 4)) else { break };
        // `LC_SEGMENT_64`
        if kind == 0x19 {
            let (Some(fileoff), Some(filesize), Some(sections)) =
                (word(command + 40, 8), word(command + 48, 8), word(command + 64, 4))
            else {
                break;
            };
            if (fileoff..fileoff + filesize).contains(&(at as u64)) {
                for nth in 0..sections as usize {
                    let section = command + 72 + 80 * nth;
                    let (Some(addr), Some(len), Some(offset)) =
                        (word(section + 32, 8), word(section + 40, 8), word(section + 48, 4))
                    else {
                        break;
                    };
                    if offset != 0 && (offset..offset + len).contains(&(at as u64)) {
                        let within = at as u64 - offset;
                        let address = addr + within;
                        return format!(
                            "section `{},{}` +{within:#x} (address {address:#x}), {}",
                            name(section + 16),
                            name(section),
                            piece_at(image, address)
                        );
                    }
                }
                return format!("segment `{}` +{:#x}, in no section of it", name(command + 8), at as u64 - fileoff);
            }
        }
        command += size as usize;
    }
    "in no segment (the headers and load commands)".to_owned()
}

/// [`whose`] for an ELF file: the section, by its section headers.
fn whose_elf(image: &Image, file: &[u8], at: usize) -> String {
    let word = |from: usize, len: usize| word(file, from, len);
    let (Some(shoff), Some(entsize), Some(count), Some(names)) =
        (word(0x28, 8), word(0x3a, 2), word(0x3c, 2), word(0x3e, 2))
    else {
        return "not in an ELF file".to_owned();
    };
    let header = |index: u64| (shoff + index * entsize) as usize;
    let strtab = word(header(names) + 24, 8);
    let name_of = |name: u64| {
        let from = strtab? as usize + name as usize;
        let rest = file.get(from..)?;
        std::str::from_utf8(&rest[..rest.iter().position(|it| *it == 0)?]).ok()
    };
    for index in 0..count {
        let at_header = header(index);
        let (Some(name), Some(kind), Some(addr), Some(offset), Some(size)) = (
            word(at_header, 4),
            word(at_header + 4, 4),
            word(at_header + 16, 8),
            word(at_header + 24, 8),
            word(at_header + 32, 8),
        ) else {
            break;
        };
        // `SHT_NOBITS` holds no bytes of the file.
        if kind == 8 || !(offset..offset + size).contains(&(at as u64)) {
            continue;
        }
        let within = at as u64 - offset;
        let name = name_of(name).unwrap_or("?");
        if addr == 0 {
            return format!("section `{name}` +{within:#x}, not loaded");
        }
        let address = addr + within;
        return format!("section `{name}` +{within:#x} (address {address:#x}), {}", piece_at(image, address));
    }
    "in no section (the headers, or between sections)".to_owned()
}

/// The image `a` keeps, opened, with the section `plant` (a test's planted bug) written from
/// an empty state; then, when `check`, checked to keep the state `a` keeps.
fn open_from(a: &mut Image, out: fn() -> Output, plant: Option<&'static str>, check: bool) -> Result<Image, String> {
    a.plant_keep_empty = plant;
    let state = kept(a);
    a.plant_keep_empty = None;
    let b = Image::open(name(TO), out(), &state?).map_err(|it| format!("the open: {it}"))?;
    if check {
        same_state(a, &b, "on open")?;
    }
    Ok(b)
}

/// Links the start program and then each edit with image A; keeps A after `keep_at` edits and
/// opens B from it; from then on links each edit with both, the files and kept states equal.
fn round_trip(ops: &[Op], keep_at: usize, plant: Option<&'static str>) -> Result<(), String> {
    round_trip_in(elf, ops, keep_at, plant, true)
}

/// [`round_trip`], the opened state compared with the kept one on open only when `check`: a
/// planted section the stages read is then caught by a stage, not by the comparison.
fn round_trip_checked(ops: &[Op], keep_at: usize, plant: Option<&'static str>, check: bool) -> Result<(), String> {
    round_trip_in(elf, ops, keep_at, plant, check)
}

/// [`round_trip_checked`] of the image `out` gives.
fn round_trip_in(
    out: fn() -> Output,
    ops: &[Op],
    keep_at: usize,
    plant: Option<&'static str>,
    check: bool,
) -> Result<(), String> {
    let mut program = Program::start();
    let mut last = built(&program);
    let mut a = Image::new(name(TO), out()).map_err(|it| format!("the image: {it}"))?;
    link(&mut a, None, &last)?;
    let keep_at = keep_at.min(ops.len());
    for op in &ops[..keep_at] {
        program = op.apply(&program);
        let next = built(&program);
        link(&mut a, Some(&last), &next)?;
        last = next;
    }
    let mut b = open_from(&mut a, out, plant, check)?;
    for (at, op) in ops[keep_at..].iter().enumerate() {
        program = op.apply(&program);
        let next = built(&program);
        let from_a = link(&mut a, Some(&last), &next)?;
        let from_b = link(&mut b, Some(&last), &next).map_err(|it| format!("the opened image: {it}"))?;
        let when = format!("edit {} after the open ({op:?})", at + 1);
        if from_a != from_b {
            let first = from_a.iter().zip(&from_b).position(|(x, y)| x != y);
            let place = first.map_or_else(|| "past the shorter file's end".to_owned(), |at| whose(&a, &from_a, at));
            return Err(format!(
                "{when}: the files differ: {} and {} bytes, first at {first:?}: {place}",
                from_a.len(),
                from_b.len()
            ));
        }
        same_state(&a, &b, &when)?;
        last = next;
    }
    Ok(())
}

/// An edit of the patch model's ([`op`]), or one of `f0`'s composed text: added, renamed (its
/// span table's file moves, nothing recompiled) or dropped.
fn edit(tape: &Tape) -> Op {
    match tape.int_in(0..=3u8, "a text's edit") {
        0 => Op::Text(tape.int_in(0..=3u8, "text file version").checked_sub(1)),
        _ => op(tape),
    }
}

#[test]
fn an_opened_image_links_as_the_one_that_kept_it() {
    blaze_sim::check(Config::cases(32), |tape| {
        let (format, out): (&str, fn() -> Output) = match tape.below(2, "format") {
            0 => ("ELF", elf),
            _ => ("Mach-O", macho),
        };
        let ops: Vec<Op> = tape.vec(0..=8, "ops", edit);
        let at = usize::from(tape.int_in(0..=8u8, "kept after"));
        if let Err(why) = round_trip_in(out, &ops, at, None, true) {
            panic!("{format}: {why}");
        }
    });
}

/// The Mach-O writer's kept tables (its symbol table, the reused `__LINKEDIT` tables,
/// `__unwind_info` and the signature's page hashes) are not kept: an opened image builds them
/// whole, which must be the bytes the image never closed patched. The fixed cases of the ELF
/// tests, as Mach-O.
#[test]
fn a_macho_image_opened_links_as_never_closed() {
    for (ops, keep_at) in [(&AFTER_OPEN[..], 2), (&SLOT_EMPTIED[..], 3)] {
        if let Err(why) = round_trip_in(macho, ops, keep_at, None, true) {
            panic!("{ops:?} kept after {keep_at}: {why}");
        }
    }
}

/// The control: one column written from an empty state is caught, and named.
#[test]
fn a_column_left_out_of_keep_is_named() {
    let error = round_trip(&[], 0, Some("hot")).expect_err("a state missing its hot column opened as the kept one");
    assert!(error.contains("section hot"), "{error}");
}

/// Kept after `f0` gained its text and one edit; then four stages after the open, each
/// compared in bytes with the image never closed: the text's file renamed (a span table's
/// change alone, which composes the text again), a body grown past its room, a function added
/// (the first stages after an open rebuild the native graph and the writer's tables), and a
/// second rename.
const AFTER_OPEN: [Op; 6] =
    [Op::Text(Some(0)), Op::Edit(1, 2), Op::Text(Some(1)), Op::Size(1, 120), Op::Add(3), Op::Text(Some(2))];

#[test]
fn stages_after_an_open_and_a_renamed_file_link_as_never_closed() {
    if let Err(why) = round_trip(&AFTER_OPEN, 2, None) {
        panic!("{why}");
    }
}

/// The control, by its cause: the opened state's recipes written from an empty state, and not
/// compared on open, so only a stage can see them gone. The rename right after the open must
/// then make another file: the stage read the kept recipe.
#[test]
fn a_text_left_out_of_keep_is_not_composed_again() {
    let error = round_trip_checked(&AFTER_OPEN, 2, Some("texts"), false)
        .expect_err("an opened image without its texts renamed the file as the kept one did");
    assert!(error.contains("edit 1 after the open") && error.contains("the files differ"), "{error}");
}

/// The random test's shrunk case (seed 4): kept after `f0` gained a text, so its `&str` record
/// holds a pointer the loader slides (a `RELATIVE` entry); the first stage after the open drops
/// a function, which empties that function's slots in `.rela.dyn` in the image never closed.
const SLOT_EMPTIED: [Op; 5] = [Op::Text(None), Op::Text(Some(0)), Op::Edit(0, 0), Op::Remove(1), Op::Text(None)];

/// Functions come and go after the open: each takes or frees a place in the symbol table, a
/// later one the place an earlier one freed. The places are history, kept with the state
/// (`elf/image.rs`'s `KeptSymtab::keep`): the invariant is that an opened image links as the
/// one never closed, byte for byte, and a patched file is a full link's by name
/// (`elf/tests.rs`'s `named`; `patch.rs`'s `as_fresh`), not byte for byte.
const PLACES_AFTER_OPEN: [Op; 7] =
    [Op::Add(2), Op::Add(3), Op::Remove(2), Op::Add(4), Op::Remove(3), Op::Add(5), Op::Add(2)];

#[test]
fn symbol_places_after_an_open_link_as_never_closed() {
    if let Err(why) = round_trip(&PLACES_AFTER_OPEN, 2, None) {
        panic!("{why}");
    }
}

/// The control, by its cause: with the kept tables written as absent and not compared on
/// open, the opened image makes its symbol table whole in name order where the image never
/// closed patched it by place, and the files differ.
#[test]
fn symbol_places_left_out_of_keep_make_another_file() {
    let error = round_trip_checked(&PLACES_AFTER_OPEN, 2, Some("rela"), false)
        .expect_err("the opened image linked as never closed without its kept places");
    assert!(error.contains("the files differ"), "{error}");
}

#[test]
fn a_slot_emptied_after_an_open_links_as_never_closed() {
    if let Err(why) = round_trip(&SLOT_EMPTIED, 3, None) {
        panic!("{why}");
    }
}

/// The control, by name: the writer's `.rela.dyn` slots written as absent are caught on open.
#[test]
fn a_rela_table_left_out_of_keep_is_named() {
    let error = round_trip(&SLOT_EMPTIED, 3, Some("rela"))
        .expect_err("a state missing its `.rela.dyn` slots opened as the kept one");
    assert!(error.contains("section rela"), "{error}");
}

/// The control, by its cause: the same state not compared on open, so only the stage that
/// empties a slot can see it gone; the opened image then makes the table whole, in address
/// order, where the image never closed empties the slot, and the files differ.
#[test]
fn a_rela_table_left_out_of_keep_makes_another_file() {
    let error = round_trip_checked(&SLOT_EMPTIED, 3, Some("rela"), false)
        .expect_err("an opened image without its `.rela.dyn` slots patched the table as the kept one did");
    assert!(error.contains("edit 1 after the open") && error.contains("the files differ"), "{error}");
}

/// The range of the ELF file `file`'s section `wanted`, by its section headers.
fn section_range(file: &[u8], wanted: &str) -> Option<(u64, u64)> {
    let (shoff, entsize, count, names) = (word(file, 0x28, 8)?, word(file, 0x3a, 2)?, word(file, 0x3c, 2)?, word(file, 0x3e, 2)?);
    let header = |index: u64| (shoff + index * entsize) as usize;
    let strtab = word(file, header(names) + 24, 8)? as usize;
    (0..count).find_map(|index| {
        let at = header(index);
        let name = strtab + word(file, at, 4)? as usize;
        let rest = file.get(name..)?;
        let name = &rest[..rest.iter().position(|it| *it == 0)?];
        (name == wanted.as_bytes()).then_some((word(file, at + 24, 8)?, word(file, at + 32, 8)?))
    })
}

/// As the driver links: every request opens the state the last one kept ([`crate::step`]), so
/// no image lives between stages, and gives a patch that is applied to a clone of the last
/// file. Each file so made, and each state, equals the image never closed's. With
/// `drop_symtab_at`, the planted bug: that request's patch leaves out the writes into
/// `.symtab` (a moved function's local symbol is there).
fn requested(ops: &[Op], drop_symtab_at: Option<usize>) -> Result<(), String> {
    let mut program = Program::start();
    let mut last = built(&program);
    let mut a = Image::new(name(TO), elf()).map_err(|it| it.to_string())?;
    link(&mut a, None, &last)?;
    let first = crate::step(TO, name(TO), elf(), None, change(None, &last, Plant::None), Global)
        .map_err(|error| format!("the first request: {error}"))?;
    if first.patch.applies_to() != crate::PatchBase::Empty {
        return Err("the first request's patch is not the whole file".into());
    }
    let mut file = Vec::new();
    first.patch.apply(&mut file);
    let mut state = first.kept;
    for (at, op) in ops.iter().enumerate() {
        let when = format!("request {} ({op:?})", at + 1);
        program = op.apply(&program);
        let next = built(&program);
        let from_a = link(&mut a, Some(&last), &next)?;
        let mut stepped =
            crate::step(TO, name(TO), elf(), Some(Mapping::copy(&state)), change(Some(&last), &next, Plant::None), Global)
                .map_err(|error| format!("{when}: {error}"))?;
        let patch = &mut stepped.patch;
        if patch.applies_to() != crate::PatchBase::Clone {
            return Err(format!("{when}: a request after the first gave the whole file"));
        }
        if patch.base != Some(crate::digest::Digest::of(&file).root()) {
            return Err(format!("{when}: the patch is not of the last request's file"));
        }
        if drop_symtab_at == Some(at) {
            let Some((from, len)) = section_range(&from_a, ".symtab") else { return Err("no `.symtab`".into()) };
            let before = patch.writes.len();
            patch.writes.retain(|(at, bytes)| *at + bytes.len() as u64 <= from || *at >= from + len);
            if patch.writes.len() == before {
                return Err(format!("{when}: the planted bug found no write into `.symtab` to leave out"));
            }
        }
        // The clone, patched.
        patch.apply(&mut file);
        if let Some(first) = from_a.iter().zip(&file).position(|(x, y)| x != y) {
            return Err(format!("{when}: the files differ first at {first}: {}", whose(&a, &from_a, first)));
        }
        if file.len() != from_a.len() {
            return Err(format!("{when}: the files' lengths differ: {} and {}", file.len(), from_a.len()));
        }
        let ours = verify(TO, Mapping::copy(&stepped.kept)).map_err(|it| format!("{when}: the verify: {it}"))?;
        if let Some(section) = first_difference(&kept(&a)?, &ours) {
            return Err(format!("{when}: the kept states differ in the section {section}"));
        }
        state = stepped.kept;
        last = next;
    }
    Ok(())
}

#[test]
fn each_request_opens_the_last_state_and_links_as_never_closed() {
    if let Err(why) = requested(&AFTER_OPEN, None) {
        panic!("{why}");
    }
}

/// The control: request 4 grows `f1` past its room, so it moves and its local symbol's value
/// with it; a patch without its `.symtab` writes makes another file.
#[test]
fn a_patch_missing_a_moved_symbol_is_caught() {
    let error = requested(&AFTER_OPEN, Some(3)).expect_err("a patch without its `.symtab` writes made the same file");
    assert!(error.contains("request 4") && error.contains("the files differ"), "{error}");
}

/// The ELF file `file`'s sections that hold bytes in the file: name, offset and size.
fn sections(file: &[u8]) -> Vec<(String, u64, u64)> {
    let (Some(shoff), Some(entsize), Some(count), Some(names)) =
        (word(file, 0x28, 8), word(file, 0x3a, 2), word(file, 0x3c, 2), word(file, 0x3e, 2))
    else {
        return Vec::new();
    };
    let header = |index: u64| (shoff + index * entsize) as usize;
    let Some(strtab) = word(file, header(names) + 24, 8) else { return Vec::new() };
    (0..count)
        .filter_map(|index| {
            let at = header(index);
            // `SHT_NOBITS` holds no bytes of the file.
            if word(file, at + 4, 4)? == 8 {
                return None;
            }
            let rest = file.get(strtab as usize + word(file, at, 4)? as usize..)?;
            let name = String::from_utf8_lossy(&rest[..rest.iter().position(|it| *it == 0)?]).into_owned();
            Some((name, word(file, at + 24, 8)?, word(file, at + 32, 8)?))
        })
        .collect()
}

/// The Mach-O file `file`'s regions that hold bytes in the file: its load commands, each
/// section of each `LC_SEGMENT_64` as `segment,section`, and the tables of `__LINKEDIT` by the
/// load commands that point at them.
fn macho_regions(file: &[u8]) -> Vec<(String, u64, u64)> {
    let word = |from: usize, len: usize| word(file, from, len);
    let name = |from: usize| {
        let bytes = file.get(from..from + 16).unwrap_or_default();
        String::from_utf8_lossy(&bytes[..bytes.iter().position(|it| *it == 0).unwrap_or(bytes.len())]).into_owned()
    };
    let (Some(count), Some(commands_len)) = (word(16, 4), word(20, 4)) else { return Vec::new() };
    let mut regions = vec![("(load commands)".to_owned(), 0, 32 + commands_len)];
    let mut command = 32usize;
    for _ in 0..count {
        let (Some(kind), Some(size)) = (word(command, 4), word(command + 4, 4)) else { break };
        let data = |what: &str, at: usize| match (word(command + at, 4), word(command + at + 4, 4)) {
            (Some(offset), Some(len)) if len > 0 => Some((what.to_owned(), offset, len)),
            _ => None,
        };
        match kind {
            // `LC_SEGMENT_64`
            0x19 => {
                let sections = word(command + 64, 4).unwrap_or(0) as usize;
                for nth in 0..sections {
                    let section = command + 72 + 80 * nth;
                    let (Some(len), Some(offset), Some(flags)) =
                        (word(section + 40, 8), word(section + 48, 4), word(section + 64, 4))
                    else {
                        break;
                    };
                    // `S_ZEROFILL`, `S_GB_ZEROFILL` and `S_THREAD_LOCAL_ZEROFILL` hold no bytes.
                    if offset != 0 && !matches!(flags & 0xff, 0x01 | 0x0c | 0x12) {
                        regions.push((format!("{},{}", name(section), name(section + 16)), offset, len));
                    }
                }
            }
            // `LC_SYMTAB`
            0x02 => {
                if let (Some(symoff), Some(nsyms)) = (word(command + 8, 4), word(command + 12, 4)) {
                    regions.push(("(symbols)".to_owned(), symoff, 16 * nsyms));
                }
                regions.extend(data("(strings)", 16));
            }
            // `LC_DYSYMTAB`
            0x0b => {
                if let (Some(offset), Some(count)) = (word(command + 56, 4), word(command + 60, 4)) {
                    regions.push(("(indirect symbols)".to_owned(), offset, 4 * count));
                }
            }
            // `LC_DYLD_INFO_ONLY`
            0x8000_0022 => {
                regions.extend(data("(rebase opcodes)", 8));
                regions.extend(data("(bind opcodes)", 16));
                regions.extend(data("(weak bind opcodes)", 24));
                regions.extend(data("(lazy bind opcodes)", 32));
                regions.extend(data("(export trie)", 40));
            }
            0x1d => regions.extend(data("(code signature)", 8)),
            0x26 => regions.extend(data("(function starts)", 8)),
            0x29 => regions.extend(data("(data in code)", 8)),
            0x8000_0033 => regions.extend(data("(export trie)", 8)),
            0x8000_0034 => regions.extend(data("(chained fixups)", 8)),
            _ => {}
        }
        command += size as usize;
    }
    regions
}

/// The bytes `writes` puts in each section of the ELF or Mach-O file `file` (for Mach-O, also
/// each `__LINKEDIT` table, [`macho_regions`]), and outside every section (padding, the room
/// between regions), each with how many write ranges reach it, and the ranges outside every
/// section by offset: what a patch is made of, by name. One range of a whole table and many
/// entry-sized ranges are different faults.
fn written_by_section(file: &[u8], writes: &[(u64, Vec<u8>)]) -> String {
    let sections = match file.get(..4) {
        Some(b"\x7fELF") => sections(file),
        _ => macho_regions(file),
    };
    let mut counts: std::collections::BTreeMap<String, (u64, u32)> = std::collections::BTreeMap::new();
    let mut outside = Vec::new();
    for (at, bytes) in writes {
        let (from, to) = (*at, *at + bytes.len() as u64);
        let mut inside = 0;
        for (name, offset, size) in &sections {
            let overlap = to.min(offset + size).saturating_sub(from.max(*offset));
            if overlap > 0 {
                let entry = counts.entry(name.clone()).or_default();
                (entry.0, entry.1) = (entry.0 + overlap, entry.1 + 1);
                inside += overlap;
            }
        }
        let rest = (to - from).saturating_sub(inside);
        if rest > 0 {
            let entry = counts.entry("(in no section)".to_owned()).or_default();
            (entry.0, entry.1) = (entry.0 + rest, entry.1 + 1);
            outside.push(format!("{from:#x}+{:#x}", to - from));
        }
    }
    let by_section =
        counts.iter().map(|(name, (bytes, ranges))| format!("{name} {bytes} in {ranges}")).collect::<Vec<_>>();
    format!("{}; writes reaching outside sections: {}", by_section.join(", "), outside.join(" "))
}

/// The common edits, each one request after a first that linked the program [`EditKind::before`]
/// makes: the patch of each is held to the same promise, proportional to the edit and not to
/// the image.
#[derive(Clone, Copy, Debug)]
enum EditKind {
    /// `f1`'s body edited at its size.
    Body,
    /// A function of 206 bytes added, called by none.
    Added,
    /// The last function removed, called by none.
    Removed,
    /// The last function removed and one of another name and the same size added: a rename, as
    /// a rename reaches the link (another key, another name).
    Renamed,
    /// `f1` reads a static it did not: its body and the static.
    StaticAdded,
    /// The static `f1` reads takes another value.
    StaticChanged,
    /// `f1` gains its call frame entry, every other function having one.
    FdeGained,
    /// `f1` loses its call frame entry, every other function keeping one.
    FdeLost,
}

impl EditKind {
    /// `f0` calling `f1`, then `f2` up to `functions` functions, each of 206 bytes (pad 200),
    /// and what the edit needs there before it.
    fn before(self, functions: u8) -> Program {
        let mut program = Program::start();
        for n in 1..functions {
            program = Op::Size(n, 200).apply(&Op::Add(n).apply(&program));
        }
        let setup: Vec<Op> = match self {
            EditKind::StaticChanged => vec![Op::Reads(1, Some(0))],
            EditKind::FdeGained => (0..functions).filter(|it| *it != 1).map(Op::Frame).collect(),
            EditKind::FdeLost => (0..functions).map(Op::Frame).collect(),
            _ => Vec::new(),
        };
        setup.iter().fold(program, |program, op| op.apply(&program))
    }

    /// The edit, as the ops of one request.
    fn ops(self, functions: u8) -> Vec<Op> {
        let last = functions - 1;
        match self {
            EditKind::Body => vec![Op::Edit(1, 1)],
            EditKind::Added => vec![Op::Add(functions), Op::Size(functions, 200)],
            EditKind::Removed => vec![Op::Remove(last)],
            EditKind::Renamed => vec![Op::Remove(last), Op::Add(functions), Op::Size(functions, 200)],
            EditKind::StaticAdded => vec![Op::Reads(1, Some(0))],
            EditKind::StaticChanged => vec![Op::Value(0, 0x77)],
            EditKind::FdeGained | EditKind::FdeLost => vec![Op::Frame(1)],
        }
    }

    /// The floor, in bytes: what an ELF patch of the edit must write whatever the image's size,
    /// by its parts at the grain a patch writes (whole 8-byte words). The body edit's, from the
    /// run of 2026-11-15: the body and its fill 222, the jump-table cells 16, the build id 16,
    /// the local symbol 32, the headers 16, one `.rela.dyn` entry 24: 326. The others from it:
    /// - added: the body 222, a new symbol 24 and its name 8, a cell filled 8, its `.rela.dyn`
    ///   entry 24, build id 16, headers 16: 318;
    /// - removed: the symbol emptied 24, the cell emptied 8, its entry emptied 24, build id 16,
    ///   headers 16: 88;
    /// - renamed: the added function's, the removed symbol's place and name taken by the new
    ///   one and its emptied slot's entry by the new cell's: 326, as a body edit;
    /// - static added: a body edit and the static's word: 334;
    /// - static changed: its word written where it is, build id 16, headers 16: 40;
    /// - FDE gained: a body edit, the FDE in the patch region (24) and its
    ///   `.eh_frame_hdr` entry (8, appended: the patch region is after `.text`, so its
    ///   functions sort last) with the count (4, one word): 366;
    /// - FDE lost: a body edit, and the old entry of `.eh_frame_hdr` made a copy of the entry
    ///   before it (8: the table stays sorted, no entry shifts): 334.
    ///
    /// Mach-O's floor is not stated per kind: an `nlist` entry is 16 bytes not 24, a chained
    /// slot and its predecessor's `next` replace the `.rela.dyn` entry, and every 4 KiB page the
    /// patch touches adds its 32-byte hash to the signature (about four pages an edit: 128).
    fn floor(self) -> u64 {
        match self {
            EditKind::Body | EditKind::Renamed => 326,
            EditKind::Added => 318,
            EditKind::Removed => 88,
            EditKind::StaticAdded | EditKind::FdeLost => 334,
            EditKind::StaticChanged => 40,
            EditKind::FdeGained => 366,
        }
    }
}

/// The patch of `edit` after a request that linked [`EditKind::before`] of `functions`
/// functions, written as `out` says: its bytes, and by section ([`written_by_section`]).
fn edit_patch(out: fn() -> Output, edit: EditKind, functions: u8) -> (u64, String) {
    let program = edit.before(functions);
    let last = built(&program);
    let first = crate::step(TO, name(TO), out(), None, change(None, &last, Plant::None), Global).unwrap();
    let mut file = Vec::new();
    first.patch.apply(&mut file);
    let next_program = edit.ops(functions).iter().fold(program, |program, op| op.apply(&program));
    let next = built(&next_program);
    let stepped =
        crate::step(TO, name(TO), out(), Some(Mapping::copy(&first.kept)), change(Some(&last), &next, Plant::None), Global)
            .unwrap();
    assert_eq!(stepped.patch.applies_to(), crate::PatchBase::Clone);
    stepped.patch.apply(&mut file);
    (stepped.patch.written(), written_by_section(&file, &stepped.patch.writes))
}

/// The most an edit's patch writes, whatever the image's size: a bound well above every
/// [`EditKind::floor`], which catches a table written whole (a 250-function image's `.symtab`
/// alone is 6 KB).
const BODY_EDIT_MOST: u64 = 8 * crate::digest::BLOCK as u64;

/// The floor's test, for one edit: its patch writes the same in an image of 8 functions as in
/// one of 250, within a block, and below the bound. On success it prints one line with both
/// sizes and the larger's ratio to the floor (rule 67). The body edit failed at 3,342 and
/// 18,014 bytes while `.symtab` listed the locals by address (the edited function moved to the
/// patch region, shifting every entry after it), then at 3,342 and 13,918 while a changed
/// table was written by the 4 KiB block its change was in (`file.rs`'s `narrowed`), then at
/// 534 and 6,286 while `.rela.dyn`'s slots were matched by position (`patch_relative`).
fn does_not_grow(out: fn() -> Output, format: &str, edit: EditKind) {
    let ((small, small_by), (large, large_by)) = (edit_patch(out, edit, 8), edit_patch(out, edit, 250));
    let said = format!("{format} {edit:?}: 8 functions: {small} bytes ({small_by}); 250: {large} bytes ({large_by})");
    assert!(small <= BODY_EDIT_MOST && large <= BODY_EDIT_MOST, "the bound {BODY_EDIT_MOST}: {said}");
    assert!(large <= small + crate::digest::BLOCK as u64, "{said}");
    let floor = edit.floor();
    println!(
        "{format} {edit:?}: 8 functions {small} bytes, 250 functions {large} bytes; floor {floor} (ELF's), ratio {:.2}",
        large as f64 / floor as f64
    );
}

/// The same program as a Mach-O fixed up by chains (macOS 12, ld-prime's choice and every arm64
/// host's): a changed slot rewrites its 8 bytes and its predecessor's `next`
/// (`macho/chained.rs`). An image fixed up by opcodes (below macOS 12) re-encodes its rebase
/// stream, which shifts after the change, and is not held to this bound.
fn macho_chained() -> Output {
    Output { platform: Some(Platform { platform: 1, minimum: 0x000c_0000, sdk: 0x000f_0000 }), ..macho() }
}

#[test]
fn a_body_edits_patch_does_not_grow_with_the_image() {
    does_not_grow(elf, "ELF", EditKind::Body);
}

/// The locals are in the same name order as ELF's (`crate::model::local_order`), so the edited
/// function's `nlist` entry is the one rewritten; its page hashes, its cell's slot and the
/// function starts are the rest.
#[test]
fn a_macho_body_edits_patch_does_not_grow_with_the_image() {
    does_not_grow(macho_chained, "Mach-O", EditKind::Body);
}

/// A new function takes an empty `.symtab` entry and a free run of `.strtab` (`elf/image.rs`'s
/// kept places): no other local moves.
#[test]
fn an_added_functions_patch_does_not_grow_with_the_image() {
    does_not_grow(elf, "ELF", EditKind::Added);
}

#[test]
fn a_macho_added_functions_patch_does_not_grow_with_the_image() {
    does_not_grow(macho_chained, "Mach-O", EditKind::Added);
}

/// A removed function's local is zeroed where it was and its place freed: no other local moves.
#[test]
fn a_removed_functions_patch_does_not_grow_with_the_image() {
    does_not_grow(elf, "ELF", EditKind::Removed);
}

#[test]
fn a_macho_removed_functions_patch_does_not_grow_with_the_image() {
    does_not_grow(macho_chained, "Mach-O", EditKind::Removed);
}

#[test]
fn a_renamed_functions_patch_does_not_grow_with_the_image() {
    does_not_grow(elf, "ELF", EditKind::Renamed);
}

#[test]
fn a_macho_renamed_functions_patch_does_not_grow_with_the_image() {
    does_not_grow(macho_chained, "Mach-O", EditKind::Renamed);
}

#[test]
fn an_added_statics_patch_does_not_grow_with_the_image() {
    does_not_grow(elf, "ELF", EditKind::StaticAdded);
}

#[test]
fn a_macho_added_statics_patch_does_not_grow_with_the_image() {
    does_not_grow(macho_chained, "Mach-O", EditKind::StaticAdded);
}

#[test]
fn a_changed_statics_patch_does_not_grow_with_the_image() {
    does_not_grow(elf, "ELF", EditKind::StaticChanged);
}

#[test]
fn a_macho_changed_statics_patch_does_not_grow_with_the_image() {
    does_not_grow(macho_chained, "Mach-O", EditKind::StaticChanged);
}

/// Every function but `f1` has its FDE, so an `.eh_frame_hdr` that shifted its entries would
/// write 8 bytes a function.
#[test]
fn a_gained_fdes_patch_does_not_grow_with_the_image() {
    does_not_grow(elf, "ELF", EditKind::FdeGained);
}

#[test]
fn a_macho_gained_fdes_patch_does_not_grow_with_the_image() {
    does_not_grow(macho_chained, "Mach-O", EditKind::FdeGained);
}

#[test]
fn a_lost_fdes_patch_does_not_grow_with_the_image() {
    does_not_grow(elf, "ELF", EditKind::FdeLost);
}

#[test]
fn a_macho_lost_fdes_patch_does_not_grow_with_the_image() {
    does_not_grow(macho_chained, "Mach-O", EditKind::FdeLost);
}

#[test]
fn a_state_not_as_written_is_refused_by_name() {
    let program = Program::start();
    let mut image = Image::new(name(TO), output(true)).unwrap();
    link(&mut image, None, &built(&program)).unwrap();
    let bytes = image.keep(Global).unwrap();
    // The control: the bytes as written verify and open.
    let good = verify(TO, Mapping::copy(&bytes)).unwrap();
    Image::open(name(TO), output(true), &good).unwrap();

    let refused = |bytes: &[u8]| match verify(TO, Mapping::copy(bytes)) {
        Ok(_) => panic!("a changed state verified"),
        Err(error) => {
            assert_eq!(error.image, TO);
            error.why
        }
    };

    let mut other = bytes.clone();
    let at = offset_of!(Header, schema);
    other[at..at + 32].copy_from_slice(&blaze_format::schema("blaze-link kept state 0").0.0);
    assert!(matches!(refused(&other), KeepRefusal::Layout { .. }));

    let cut = &bytes[..bytes.len() - 1];
    assert!(matches!(refused(cut), KeepRefusal::Truncated { header: Some(_), .. }));

    let mut flipped = bytes.clone();
    let (at, len) = good.range(Section::Hot);
    assert!(len > 0, "the hot column is kept");
    flipped[at + len / 2] ^= 0x10;
    let why = refused(&flipped);
    assert!(matches!(why, KeepRefusal::Corrupt { section: "hot" }), "{why:?}");
}
