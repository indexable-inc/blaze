//! The side file against the debug info it replaces: for random programs and edit sequences,
//! a side file kept by patches resolves every address as [`crate::dwarf::sections`] of the
//! same program does (what an image with its debug info inside carries), carries the
//! executable's build id and has the CRC-32 the executable names; what a one-function edit
//! writes does not grow with the program.

use std::collections::{BTreeMap, BTreeSet};

use gimli::{EndianSlice, LittleEndian};
use hegel::generators as gs;
use object::read::elf::ElfFile64;
use object::{Endianness, Object as _, ObjectSection as _};

use super::*;
use crate::dwarf::DebugFunction;

/// Bytes as `object` reads a file: from 8-aligned words.
struct Aligned {
    words: Vec<u64>,
    len: usize,
}

impl Aligned {
    fn of(bytes: &[u8]) -> Aligned {
        let words = bytes
            .chunks(8)
            .map(|chunk| {
                let mut word = [0u8; 8];
                word[..chunk.len()].copy_from_slice(chunk);
                u64::from_ne_bytes(word)
            })
            .collect();
        Aligned { words, len: bytes.len() }
    }

    fn bytes(&self) -> &[u8] {
        &zerocopy::IntoBytes::as_bytes(&self.words[..])[..self.len]
    }
}

const FILES: u8 = 4;
const NAMES: u8 = 6;

/// One function of the model: where it is, what it is called and its rows (code offset,
/// file, line, column).
#[derive(Clone, PartialEq, Eq, Debug)]
struct F {
    addr: u64,
    size: u64,
    name: Option<String>,
    rows: Vec<(u32, u8, u32, u32)>,
}

/// The reference model: a program is its functions by number; what a debugger sees of it is
/// what [`inside`] reads of its debug info made whole.
#[derive(Clone, Debug)]
struct Program {
    funcs: BTreeMap<u32, F>,
    /// Where the next function made is placed: code is never placed where code was.
    next: u64,
    generation: u64,
}

fn file(n: u8) -> String {
    format!("src/f{n}.rs")
}

impl Program {
    fn start() -> Program {
        let mut program = Program { funcs: BTreeMap::new(), next: 0x1000, generation: 0 };
        program.make(0, 0);
        program.make(1, 1);
        program
    }

    /// Function `n` made again from `seed`: a new place, size and rows, in files below 2.
    fn make(&mut self, n: u32, seed: u8) {
        let size = 8 + 4 * u64::from(seed % 5);
        let count = u32::from(seed % 4);
        let rows = (0..count)
            .map(|row| {
                let at = (row * 3).min(size as u32 - 1);
                (at, (seed + row as u8) % 2, 10 + u32::from(seed) * 7 + row * 300 * (row % 2), row)
            })
            .collect();
        let name = (seed % 7 != 6).then(|| format!("_RNvCs{n}_1f{}", "x".repeat(seed as usize % 3)));
        self.funcs.insert(n, F { addr: self.next, size, name, rows });
        self.next += size + 4 * u64::from(seed % 3);
    }

    fn apply(&mut self, op: &Op) {
        match *op {
            Op::Edit(n, seed) | Op::Add(n, seed) => self.make(u32::from(n), seed),
            Op::Remove(n) => {
                if self.funcs.len() > 1 {
                    self.funcs.remove(&u32::from(n));
                }
            }
            // A line edit above: every row of the file moves, and no code does.
            Op::Shift(file, by) => {
                for f in self.funcs.values_mut() {
                    for row in &mut f.rows {
                        if row.1 == file % 2 {
                            row.2 += u32::from(by);
                        }
                    }
                }
            }
            Op::Rename(n) => {
                if let Some(f) = self.funcs.get_mut(&u32::from(n)) {
                    f.name = match f.name.take() {
                        Some(name) => (name.len() < 40).then(|| format!("{name}y")),
                        None => Some("named".into()),
                    };
                }
            }
            // A row in a file no function named before: the line header grows.
            Op::File(n, file) => {
                if let Some(f) = self.funcs.get_mut(&u32::from(n)) {
                    f.rows = vec![(0, 2 + file % (FILES - 2), 5, 1)];
                }
            }
        }
    }

    fn id(&self) -> [u8; 16] {
        let mut id = [0x5a; 16];
        id[..8].copy_from_slice(&self.generation.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_le_bytes());
        id
    }

    /// The executable's symbol table and names: one entry per function, by number.
    fn symbols(&self) -> (Vec<u8>, Vec<u8>) {
        let (mut symtab, mut strtab) = (vec![0u8; 24], vec![0u8]);
        for (n, f) in &self.funcs {
            let mut symbol = [0u8; 24];
            symbol[..4].copy_from_slice(&(strtab.len() as u32).to_le_bytes());
            symbol[4] = 2;
            symbol[6] = 1;
            symbol[8..16].copy_from_slice(&f.addr.to_le_bytes());
            symbol[16..24].copy_from_slice(&f.size.to_le_bytes());
            symtab.extend_from_slice(&symbol);
            strtab.extend_from_slice(format!("f{n}\0").as_bytes());
        }
        (symtab, strtab)
    }

    /// The executable, as far as a side file reads it: its header, its sections (code, the
    /// build id note, the symbol table and its names) and their names.
    fn exe(&self) -> Vec<u8> {
        let (symtab, strtab) = self.symbols();
        let names = b"\0.text\0.note.gnu.build-id\0.symtab\0.strtab\0.shstrtab\0";
        let name = |it: &str| {
            let it = format!("{it}\0");
            names.windows(it.len()).position(|at| at == it.as_bytes()).unwrap() as u32
        };
        let mut out = vec![0u8; 64];
        let note_at = out.len() as u64;
        for word in [4u32, 16, 3] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(b"GNU\0");
        out.extend_from_slice(&self.id());
        let symtab_at = out.len() as u64;
        out.extend_from_slice(&symtab);
        let strtab_at = out.len() as u64;
        out.extend_from_slice(&strtab);
        let names_at = out.len() as u64;
        out.extend_from_slice(names);
        out.resize(out.len().next_multiple_of(8), 0);
        let shoff = out.len() as u64;
        let header = |name: u32, kind: u32, flags: u64, addr: u64, at: u64, size: u64, link: u32| {
            let mut it = [0u8; 64];
            it[..4].copy_from_slice(&name.to_le_bytes());
            it[4..8].copy_from_slice(&kind.to_le_bytes());
            it[8..16].copy_from_slice(&flags.to_le_bytes());
            it[16..24].copy_from_slice(&addr.to_le_bytes());
            it[24..32].copy_from_slice(&at.to_le_bytes());
            it[32..40].copy_from_slice(&size.to_le_bytes());
            it[40..44].copy_from_slice(&link.to_le_bytes());
            it
        };
        out.extend_from_slice(&[0; 64]);
        out.extend_from_slice(&header(name(".text"), 1, 6, 0x1000, 0x1000, self.next, 0));
        out.extend_from_slice(&header(name(".note.gnu.build-id"), 7, 2, 0x200, note_at, 32, 0));
        let mut table = header(name(".symtab"), 2, 0, 0, symtab_at, symtab.len() as u64, 4);
        table[44..48].copy_from_slice(&1u32.to_le_bytes());
        table[56..64].copy_from_slice(&24u64.to_le_bytes());
        out.extend_from_slice(&table);
        out.extend_from_slice(&header(name(".strtab"), 3, 0, 0, strtab_at, strtab.len() as u64, 0));
        out.extend_from_slice(&header(name(".shstrtab"), 3, 0, 0, names_at, names.len() as u64, 0));
        out[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
        out[16..18].copy_from_slice(&3u16.to_le_bytes());
        out[18..20].copy_from_slice(&62u16.to_le_bytes());
        out[20..24].copy_from_slice(&1u32.to_le_bytes());
        out[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        out[52..54].copy_from_slice(&64u16.to_le_bytes());
        out[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        out[0x3c..0x3e].copy_from_slice(&6u16.to_le_bytes());
        out[0x3e..0x40].copy_from_slice(&5u16.to_le_bytes());
        out
    }
}

/// What a debugger resolves of a program: each function's name and range, and each row's
/// address, file, line and column.
#[derive(PartialEq, Eq, Debug, Default)]
struct Seen {
    functions: BTreeSet<(Option<String>, u64, u64)>,
    rows: BTreeSet<(u64, String, u64, u64)>,
}

/// The DWARF sections `get` gives by name, read with gimli: every subprogram of a range that
/// is not empty, and every row.
fn seen<'a>(get: impl Fn(&str) -> &'a [u8]) -> Result<Seen, String> {
    let bad = |what: gimli::Error| format!("the debug info does not read: {what}");
    let dwarf = gimli::Dwarf::load(|id| -> Result<_, gimli::Error> {
        Ok(EndianSlice::new(get(id.name()), LittleEndian))
    })
    .map_err(bad)?;
    let mut out = Seen::default();
    let mut units = dwarf.units();
    while let Some(header) = units.next().map_err(bad)? {
        let unit = dwarf.unit(header).map_err(bad)?;
        if let Some(program) = unit.line_program.clone() {
            let mut rows = program.rows();
            while let Some((header, row)) = rows.next_row().map_err(bad)? {
                if row.end_sequence() {
                    continue;
                }
                let file = header.file(row.file_index()).ok_or("a row in a file its header lacks")?;
                let path = dwarf
                    .attr_string(&unit, file.path_name())
                    .map_err(bad)?
                    .to_string_lossy()
                    .into_owned();
                let column = match row.column() {
                    gimli::ColumnType::LeftEdge => 0,
                    gimli::ColumnType::Column(it) => it.get(),
                };
                let line = row.line().map_or(0, |it| it.get());
                out.rows.insert((row.address(), path, line, column));
            }
        }
        let mut entries = unit.entries();
        while let Some(entry) = entries.next_dfs().map_err(bad)? {
            if entry.tag() != gimli::DW_TAG_subprogram {
                continue;
            }
            let name = match entry.attr_value(gimli::DW_AT_linkage_name) {
                Some(it) => {
                    Some(dwarf.attr_string(&unit, it).map_err(bad)?.to_string_lossy().into_owned())
                }
                None => None,
            };
            let low = match entry.attr_value(gimli::DW_AT_low_pc) {
                Some(gimli::AttributeValue::Addr(it)) => it,
                other => return Err(format!("a subprogram's low pc is {other:?}")),
            };
            let size = entry
                .attr_value(gimli::DW_AT_high_pc)
                .and_then(|it| it.udata_value())
                .ok_or("a subprogram with no size")?;
            if size != 0 {
                out.functions.insert((name, low, size));
            }
        }
    }
    Ok(out)
}

/// What `program`'s debug info linked inside its image resolves: [`crate::dwarf::sections`]
/// of its functions.
fn inside(program: &Program) -> Result<Seen, String> {
    let files: Vec<String> = (0..FILES).map(file).collect();
    let names: Vec<&str> = files.iter().map(String::as_str).collect();
    let (mut rows, mut functions) = (Vec::new(), Vec::new());
    for f in program.funcs.values() {
        let start = rows.len() as u32;
        rows.extend(f.rows.iter().map(|&(at, file, line, column)| Row {
            at,
            file: u32::from(file),
            line,
            column,
        }));
        functions.push(DebugFunction {
            addr: f.addr,
            size: f.size,
            name: f.name.as_deref(),
            lines: start..rows.len() as u32,
        });
    }
    let sections = crate::dwarf::sections(&functions, &rows, &names, 8)?;
    seen(|name| sections.iter().find(|it| it.0 == name).map_or(&[][..], |it| &it.1[..]))
}

/// A bug planted in how a patch reaches the disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The headers are not written again: the side file keeps the last build id.
    KeepsId,
    /// A changed function's old rows are left where they were.
    KeepsRows,
    /// The last four bytes are not written again: the file keeps the last CRC's.
    KeepsCrc,
    /// A function whose name or rows changed in place is left out of the link's change.
    DropsChanged,
}

/// The oracle: `side` describes `program` as its debug info inside would, and is the
/// executable's: its build id, its symbols, and the CRC-32 the executable's
/// `.gnu_debuglink` names.
fn check(program: &Program, side: &[u8]) -> Result<(), String> {
    let id = program.id();
    let aligned = Aligned::of(side);
    let elf = ElfFile64::<Endianness>::parse(aligned.bytes())
        .map_err(|it| format!("the side file is no ELF file: {it}"))?;
    let data = |name: &str| elf.section_by_name(name).and_then(|it| it.data().ok()).unwrap_or(&[]);
    let note = data(ELF_BUILD_ID);
    if note.len() < 32 || note[16..32] != id || id_of(Format::Elf, side) != Some(id) {
        return Err(format!("build id: the side file carries {note:02x?}, and the executable's is {id:02x?}"));
    }
    let (got, want) = (seen(&data)?, inside(program)?);
    if got.functions != want.functions {
        return Err(format!("functions: the side file names {:?}, debug info inside {:?}", got.functions, want.functions));
    }
    if got.rows != want.rows {
        let extra: Vec<_> = got.rows.difference(&want.rows).collect();
        let missing: Vec<_> = want.rows.difference(&got.rows).collect();
        return Err(format!("rows: the side file has {extra:?} that debug info inside lacks, and lacks {missing:?}"));
    }
    let (symtab, strtab) = program.symbols();
    if data(".symtab") != symtab || data(".strtab") != strtab {
        return Err("symbols: the side file's are not the executable's".into());
    }
    if elf.section_by_name(".text").is_none_or(|it| it.address() != 0x1000) {
        return Err("sections: the side file lacks the executable's .text at its address".into());
    }
    let named = debuglink("app", &id);
    let crc = u32::from_le_bytes(named[named.len() - 4..].try_into().unwrap());
    if named[..10] != *b"app.debug\0" || crc32(side) != crc {
        return Err(format!("CRC: the side file's is {:#x}, and the executable's .gnu_debuglink names {crc:#x}", crc32(side)));
    }
    Ok(())
}

/// A function's key, as codegen keys one: what it is and where its code is (an edit's code
/// is new code, under a new key).
fn key_of(n: u32, f: &F) -> InstanceKey {
    let mut key = [0u8; 32];
    key[..4].copy_from_slice(&n.to_le_bytes());
    key[4..12].copy_from_slice(&f.addr.to_le_bytes());
    InstanceKey(ContentId(key))
}

/// The model's functions as an [`Input`]'s, by address.
fn views<'a>(program: &'a Program, files: &'a [String]) -> (Vec<Func<'a>>, Vec<Lined<'a>>) {
    views_of(program, files, |_, _| true)
}

/// Those of the model's functions that `takes`, as an [`Input`]'s, by address.
fn views_of<'a>(
    program: &'a Program,
    files: &'a [String],
    takes: impl Fn(u32, &F) -> bool,
) -> (Vec<Func<'a>>, Vec<Lined<'a>>) {
    let (mut functions, mut lines) = (Vec::new(), Vec::new());
    for (n, f) in program.funcs.iter().filter(|it| takes(*it.0, it.1)) {
        let start = lines.len() as u32;
        lines.extend(f.rows.iter().map(|&(at, file, line, column)| Lined {
            at,
            file: &files[file as usize],
            line,
            column,
        }));
        functions.push(Func {
            key: key_of(*n, f),
            addr: f.addr,
            size: f.size,
            name: f.name.as_deref(),
            lines: start..lines.len() as u32,
        });
    }
    functions.sort_by_key(|it| (it.addr, it.key));
    (functions, lines)
}

/// `program`'s functions `functions` as an update is given them.
fn input_of<'a>(
    program: &Program,
    functions: &'a [Func<'a>],
    lines: &'a [Lined<'a>],
    given: Given<'a>,
) -> Input<'a> {
    Input {
        format: Format::Elf,
        name: "app",
        id: program.id(),
        generation: program.generation,
        functions,
        lines,
        given,
    }
}

/// A side file kept on a disk across the model's generations.
struct Disk {
    kept: Kept,
    side: Option<Vec<u8>>,
    /// The program the side file was last brought to.
    last: Option<Program>,
    /// Whether an update is given the link's change (what differs from `last`), as
    /// [`crate::Image::debug_file`] gives it when it can, or every function.
    follow: bool,
}

impl Disk {
    fn new() -> Disk {
        Disk { kept: Kept::new(), side: None, last: None, follow: true }
    }

    fn all() -> Disk {
        Disk { follow: false, ..Disk::new() }
    }

    /// `program`'s next generation: its side file brought to it, as `plant` writes it, and
    /// what was written.
    fn link(&mut self, program: &mut Program, plant: Plant) -> Result<DebugWrite, String> {
        program.generation += 1;
        let exe = program.exe();
        let files: Vec<String> = (0..FILES).map(file).collect();
        let read = |at: u64, out: &mut [u8]| {
            let from = (at as usize).min(exe.len());
            let len = out.len().min(exe.len() - from);
            out[..len].copy_from_slice(&exe[from..from + len]);
        };
        // The link's change: the functions that are not as they were, and the keys gone.
        let followed = match (&self.last, self.follow) {
            (Some(last), true) => {
                let (mut functions, lines) = views_of(program, &files, |n, f| last.funcs.get(&n) != Some(f));
                if plant == Plant::DropsChanged
                    && let Some(at) = functions.iter().position(|it| {
                        last.funcs.iter().any(|(n, f)| key_of(*n, f) == it.key)
                    })
                {
                    functions.remove(at);
                }
                let gone: Vec<InstanceKey> = (last.funcs.iter())
                    .map(|(n, f)| key_of(*n, f))
                    .filter(|key| !program.funcs.iter().any(|(n, f)| key_of(*n, f) == *key))
                    .collect();
                let given = Given::Changed { gone: &gone, count: program.funcs.len() };
                let write =
                    self.kept.update(&input_of(program, &functions, &lines, given), &read, None)?;
                if let Some(write) = &write
                    && write.encoded > functions.len() as u64
                {
                    return Err(format!("{} functions encoded of a change of {}", write.encoded, functions.len()));
                }
                write
            }
            _ => None,
        };
        let mut write = match followed {
            Some(write) => write,
            None => {
                let (functions, lines) = views(program, &files);
                (self.kept.update(&input_of(program, &functions, &lines, Given::All), &read, None)?)
                    .ok_or("an update given every function asked for every function")?
            }
        };
        if self.kept.at() != Some(program.generation) {
            return Err("the side file is not at the generation it was brought to".into());
        }
        let honest = write.clone();
        if write.base.is_some() {
            match plant {
                Plant::None | Plant::KeepsRows | Plant::DropsChanged => {}
                Plant::KeepsId => write.writes.retain(|it| it.0 != 0),
                Plant::KeepsCrc => {
                    for (at, bytes) in &mut write.writes {
                        if *at + bytes.len() as u64 == write.len {
                            bytes.truncate(bytes.len() - 4);
                        }
                    }
                }
            }
        }
        let mut side = write.apply(Format::Elf, self.side.as_deref())?;
        // The program the last side file had, left as it was: every sequence overwritten
        // since is back, after the length the new one says.
        if plant == Plant::KeepsRows
            && write.base.is_some()
            && let Some(old) = &self.side
        {
            let aligned = Aligned::of(old);
            let elf = ElfFile64::<Endianness>::parse(aligned.bytes()).map_err(|it| it.to_string())?;
            let (at, len) = elf.section_by_name(".debug_line").and_then(|it| it.file_range()).unwrap();
            let range = at as usize + 4..(at + len) as usize;
            side[range.clone()].copy_from_slice(&old[range]);
        }
        if plant == Plant::None && side != self.kept.bytes() {
            return Err("the file the writes make is not the file the model holds".into());
        }
        check(program, &side)?;
        self.side = Some(side);
        // clone: the model, a few functions
        self.last = Some(program.clone());
        Ok(honest)
    }
}

#[derive(Clone, Debug)]
enum Op {
    Edit(u8, u8),
    Add(u8, u8),
    Remove(u8),
    Shift(u8, u8),
    Rename(u8),
    File(u8, u8),
}

#[hegel::composite]
fn op(tc: &hegel::TestCase) -> Op {
    let n = tc.draw(gs::integers::<u8>().max_value(NAMES - 1));
    let seed = tc.draw(gs::integers::<u8>());
    match tc.draw(gs::integers::<u8>().max_value(9)) {
        0..=3 => Op::Edit(n, seed),
        4 => Op::Add(n, seed),
        5 => Op::Remove(n),
        6 | 7 => Op::Shift(seed, 1 + n),
        8 => Op::Rename(n),
        _ => Op::File(n, seed),
    }
}

/// `ops` applied one link each, the side file patched each time as `plant` writes it; the
/// oracle after each.
fn refines(ops: &[Op], plant: Plant) -> Result<(), String> {
    refines_by(ops, plant, true)
}

/// [`refines`], each update given the link's change (`follow`) or every function.
fn refines_by(ops: &[Op], plant: Plant, follow: bool) -> Result<(), String> {
    let mut program = Program::start();
    let mut disk = Disk { follow, ..Disk::new() };
    disk.link(&mut program, Plant::None)?;
    for op in ops {
        program.apply(op);
        disk.link(&mut program, plant)?;
    }
    Ok(())
}

/// The refinement: after any edit sequence (bodies edited, added and removed, lines moved,
/// functions renamed, new files), the side file kept by patches resolves every function and
/// every row as the same program's debug info linked inside does, is the bytes the model
/// holds, and carries the executable's build id, symbols and CRC.
#[hegel::test(test_cases = 128)]
fn a_side_file_kept_by_patches_resolves_as_debug_info_inside(tc: hegel::TestCase) {
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(12));
    let follow = tc.draw(gs::integers::<u8>().max_value(1)) == 1;
    if let Err(why) = refines_by(&ops, Plant::None, follow) {
        panic!("{why}");
    }
}

/// A function renamed in place and left out of the change keeps its old record, and the
/// records still number the image's functions: nothing but the oracle sees it.
#[test]
fn the_oracle_catches_a_changed_function_left_out_of_the_change() {
    let ops = [Op::Rename(1)];
    assert_eq!(refines(&ops, Plant::None), Ok(()));
    let why = refines(&ops, Plant::DropsChanged).expect_err("the oracle missed a record left stale");
    assert!(why.starts_with("functions:"), "{why}");
}

/// A change that does not say a function is gone leaves one record too many: the update
/// asks for every function, says why, and the file is right.
#[test]
fn a_change_that_misses_a_removed_function_is_made_from_every_function() {
    let mut program = Program::start();
    program.make(2, 5);
    let mut disk = Disk::new();
    disk.link(&mut program, Plant::None).unwrap();
    // The model's change is exact, so the function is taken out behind its back.
    disk.last.as_mut().unwrap().funcs.remove(&2);
    program.funcs.remove(&2);
    let write = disk.link(&mut program, Plant::None).unwrap();
    assert!(write.all.is_some_and(|it| it.contains("not one a function")), "{:?}", write.all);
    assert_eq!(write.encoded, 2, "every function of the image");
}

/// A fresh side file is a function of the program alone: made twice, the same bytes; and
/// the file after an edit sequence laid out whole again resolves the same.
#[test]
fn a_whole_side_file_is_the_same_bytes_twice() {
    let ops = [Op::Edit(1, 9), Op::Add(3, 77), Op::Shift(0, 3), Op::Remove(0)];
    let make = || {
        let mut program = Program::start();
        for op in &ops {
            program.apply(op);
        }
        let mut disk = Disk::new();
        let write = disk.link(&mut program, Plant::None).unwrap();
        assert_eq!(write.whole, Some("no side file was written yet"));
        disk.side.unwrap()
    };
    assert_eq!(make(), make());
}

#[test]
fn the_oracle_catches_a_side_file_of_another_build_id() {
    let ops = [Op::Edit(1, 9)];
    assert_eq!(refines(&ops, Plant::None), Ok(()));
    let why = refines(&ops, Plant::KeepsId).expect_err("the oracle missed a side file of the last build id");
    assert!(why.contains("build id"), "{why}");
}

#[test]
fn the_oracle_catches_a_line_row_left_for_a_moved_function() {
    // Function 1 has rows (its seed gives three), and the edit places it elsewhere.
    let ops = [Op::Edit(1, 7), Op::Edit(1, 11)];
    assert_eq!(refines(&ops, Plant::None), Ok(()));
    let why = refines(&ops, Plant::KeepsRows).expect_err("the oracle missed rows left where the function was");
    assert!(why.starts_with("rows:"), "{why}");
}

#[test]
fn the_oracle_catches_a_debuglink_crc_not_updated() {
    let ops = [Op::Edit(1, 9)];
    let why = refines(&ops, Plant::KeepsCrc).expect_err("the oracle missed a side file of the last CRC");
    assert!(why.starts_with("CRC:"), "{why}");
}

/// A patch is of the side file it was made over and of no other: one applied to a file of
/// another build id, or to none, is refused by name, and nothing is made.
#[test]
fn a_patch_of_another_side_file_is_refused() {
    let mut program = Program::start();
    let mut disk = Disk::new();
    disk.link(&mut program, Plant::None).unwrap();
    let first = disk.side.clone().unwrap();
    program.apply(&Op::Edit(1, 9));
    disk.link(&mut program, Plant::None).unwrap();
    program.apply(&Op::Edit(0, 9));
    let write = disk.link(&mut program, Plant::None).unwrap();
    assert!(write.base.is_some() && write.whole.is_none());
    let refused = write.apply(Format::Elf, Some(&first)).unwrap_err();
    assert!(refused.contains("app.debug") && refused.contains("build id"), "{refused}");
    assert!(write.apply(Format::Elf, None).unwrap_err().contains("no side file"));
}

/// Counter: a one-function edit writes two records (the entry and rows of the function it
/// made, and those of the one it replaced), the block of the symbol table that names it, the
/// headers and the CRC, whether the program has a hundred functions or ten thousand.
#[test]
fn a_one_function_edit_rewrites_its_own_records() {
    let mut written = Vec::new();
    for count in [100u32, 10_000] {
        let mut program = Program::start();
        for n in 2..count {
            program.make(n, (n % 251) as u8);
        }
        // Ten functions of a source file of their own.
        for n in 10..20 {
            program.funcs.get_mut(&n).unwrap().rows = vec![(0, 3, 50, 1)];
        }
        let mut disk = Disk::new();
        let whole = disk.link(&mut program, Plant::None).unwrap();
        assert_eq!((whole.records, whole.encoded), (u64::from(count), u64::from(count)));
        program.make(count / 2, 13);
        let write = disk.link(&mut program, Plant::None).unwrap();
        assert_eq!((write.whole, write.records, write.encoded), (None, 2, 1), "{count} functions");
        assert_eq!(write.all, None, "the update followed the link's change");
        assert!(write.written() < 3 * 4096, "{count} functions: {} bytes written", write.written());
        // A line added above moves the rows of its file's functions: theirs are written, and
        // no other file's.
        for f in program.funcs.values_mut() {
            for row in f.rows.iter_mut().filter(|row| row.1 == 3) {
                row.2 += 2;
            }
        }
        let write = disk.link(&mut program, Plant::None).unwrap();
        assert_eq!((write.whole, write.records, write.encoded), (None, 10, 10), "{count} functions");
        // With no change at hand, every function is encoded to find the one that differs.
        let mut all = Disk::all();
        let mut again = program.clone();
        all.link(&mut again, Plant::None).unwrap();
        again.make(count / 2, 17);
        let write = all.link(&mut again, Plant::None).unwrap();
        assert_eq!((write.records, write.encoded), (2, u64::from(count)), "{count} functions");
        assert!(write.all.is_some());
        written.push(whole.written());
    }
    assert!(written[1] > 10 * written[0], "the whole file grows with the program: {written:?}");
}

/// Dead rows are bounded: once more than half of `.debug_line` is sequences of functions
/// since gone, the file is laid out whole, and says so.
#[test]
fn a_side_file_mostly_dead_rows_is_laid_out_again() {
    let mut program = Program::start();
    for n in 2..400 {
        program.make(n, 7);
    }
    let mut disk = Disk::new();
    disk.link(&mut program, Plant::None).unwrap();
    for n in 2..40 {
        program.funcs.remove(&n);
    }
    assert_eq!(disk.link(&mut program, Plant::None).unwrap().whole, None);
    for n in 40..360 {
        program.funcs.remove(&n);
    }
    let whole = disk.link(&mut program, Plant::None).unwrap().whole;
    assert!(whole.is_some_and(|it| it.contains("half of .debug_line")), "{whole:?}");
}

/// What is left where a sequence was reads as no row and moves no register, at every length
/// a sequence has.
#[test]
fn dead_rows_are_operations_that_do_nothing() {
    for len in (2..40).chain([100, 1000]) {
        let mut bytes = vec![0xaa; len];
        crate::dwarf::dead(&mut bytes);
        let mut at = 0;
        while at < len {
            assert_eq!(bytes[at], 0x02, "{len}: an advance of the address");
            let end = at + 1 + bytes[at + 1..].iter().position(|it| it & 0x80 == 0).unwrap();
            assert!(end - at <= 10 && bytes[at + 1..end].iter().all(|it| *it == 0x80), "{len}");
            assert_eq!(bytes[end], 0, "{len}: by 0");
            at = end + 1;
        }
    }
}

#[test]
fn crc_32_is_zlibs_and_four_bytes_force_it() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    for (bytes, want) in [(&b""[..], 0u32), (&b"blaze"[..], 0xdead_beef), (&[0xff; 100][..], 1)] {
        let mut whole = bytes.to_vec();
        whole.extend_from_slice(&crc_fix(crc32(bytes), want));
        assert_eq!(crc32(&whole), want);
    }
    let named = debuglink("app", &[7; 16]);
    assert_eq!(named, [&b"app.debug\0\0\0"[..], &[7u8; 4][..]].concat());
}

/// A Mach-O side file: the executable's `LC_UUID` first, its segments with nothing in the
/// file, `__DWARF` with the four sections where gimli reads the program back, and the
/// symbol table.
#[test]
fn a_mach_o_side_file_carries_the_uuid_the_segments_and_the_dwarf() {
    let mut program = Program::start();
    program.generation = 3;
    let id = program.id();
    let mut exe = Vec::new();
    for word in [MH_MAGIC_64, 0x0100_000c, 0, 2, 3, 24 + 152 + 24, 0, 0] {
        exe.extend_from_slice(&word.to_le_bytes());
    }
    exe.extend_from_slice(&LC_UUID.to_le_bytes());
    exe.extend_from_slice(&24u32.to_le_bytes());
    exe.extend_from_slice(&id);
    let mut segment = vec![0u8; 152];
    segment[..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
    segment[4..8].copy_from_slice(&152u32.to_le_bytes());
    segment[8..14].copy_from_slice(b"__TEXT");
    segment[24..32].copy_from_slice(&0x1_0000_0000u64.to_le_bytes());
    segment[32..40].copy_from_slice(&0x8000u64.to_le_bytes());
    segment[48..56].copy_from_slice(&0x8000u64.to_le_bytes());
    segment[64..68].copy_from_slice(&1u32.to_le_bytes());
    segment[72..78].copy_from_slice(b"__text");
    segment[88..94].copy_from_slice(b"__TEXT");
    segment[72 + 48..72 + 52].copy_from_slice(&0x4000u32.to_le_bytes());
    exe.extend_from_slice(&segment);
    let symbols_at = exe.len() as u32 + 24;
    exe.extend_from_slice(&LC_SYMTAB.to_le_bytes());
    exe.extend_from_slice(&24u32.to_le_bytes());
    for word in [symbols_at, 1, symbols_at + 16, 4] {
        exe.extend_from_slice(&word.to_le_bytes());
    }
    exe.extend_from_slice(&[9; 16]);
    exe.extend_from_slice(b"\0_f\0");
    let files: Vec<String> = (0..FILES).map(file).collect();
    let (functions, lines) = views(&program, &files);
    let input = Input {
        format: Format::MachO,
        name: "app",
        id,
        generation: 3,
        functions: &functions,
        lines: &lines,
        given: Given::All,
    };
    let read = |at: u64, out: &mut [u8]| {
        let from = (at as usize).min(exe.len());
        let len = out.len().min(exe.len() - from);
        out[..len].copy_from_slice(&exe[from..from + len]);
    };
    let mut kept = Kept::new();
    let write = kept.update(&input, &read, None).unwrap().unwrap();
    assert_eq!(write.name, "app.dSYM/Contents/Resources/DWARF/app");
    let plist = String::from_utf8(write.plist.clone().expect("a bundle has an Info.plist")).unwrap();
    assert!(plist.contains("<string>com.apple.xcode.dsym.app</string>"), "{plist}");
    assert!(plist.contains("<key>CFBundlePackageType</key>\n\t\t<string>dSYM</string>"), "{plist}");
    let side = write.apply(Format::MachO, None).unwrap();
    assert_eq!(id_of(Format::MachO, &side), Some(id));
    let aligned = Aligned::of(&side);
    let file = object::File::parse(aligned.bytes()).unwrap();
    assert_eq!(file.mach_uuid().unwrap(), Some(id));
    assert!(file.section_by_name("__text").is_some(), "the executable's sections are named");
    let got = seen(|name| {
        let name = format!("__{}", &name[1..]);
        file.section_by_name(&name).and_then(|it| it.data().ok()).unwrap_or(&[])
    })
    .unwrap();
    assert_eq!(got, inside(&program).unwrap());
    // The next generation is a patch: the UUID and nothing of the program.
    program.generation = 4;
    let (functions, lines) = views(&program, &files);
    exe[40..56].copy_from_slice(&program.id());
    let input = Input {
        format: Format::MachO,
        name: "app",
        id: program.id(),
        generation: program.generation,
        functions: &functions,
        lines: &lines,
        given: Given::All,
    };
    let read = |at: u64, out: &mut [u8]| {
        let from = (at as usize).min(exe.len());
        let len = out.len().min(exe.len() - from);
        out[..len].copy_from_slice(&exe[from..from + len]);
    };
    let write = kept.update(&input, &read, None).unwrap().unwrap();
    assert_eq!((write.base, write.whole, write.records), (Some(id), None, 0));
    let side = write.apply(Format::MachO, Some(&side)).unwrap();
    assert_eq!(id_of(Format::MachO, &side), Some(program.id()));
}

/// An executable with no build id has no side file: a debugger could not tell whose it is.
#[test]
fn an_executable_with_no_build_id_is_refused_by_name() {
    let program = Program::start();
    let mut exe = program.exe();
    let at = exe.windows(18).position(|it| it == b".note.gnu.build-id").unwrap();
    exe[at + 1] = b'x';
    let files: Vec<String> = (0..FILES).map(file).collect();
    let (functions, lines) = views(&program, &files);
    let input = Input {
        format: Format::Elf,
        name: "app",
        id: program.id(),
        generation: program.generation,
        functions: &functions,
        lines: &lines,
        given: Given::All,
    };
    let read = |at: u64, out: &mut [u8]| {
        let from = (at as usize).min(exe.len());
        let len = out.len().min(exe.len() - from);
        out[..len].copy_from_slice(&exe[from..from + len]);
    };
    let refused = Kept::new().update(&input, &read, None).unwrap_err();
    assert!(refused.contains(".note.gnu.build-id") && refused.contains("debugger"), "{refused}");
}
