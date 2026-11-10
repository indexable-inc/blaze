//! A link as a patch of its file (`patch.rs`, docs/v2/plan/design/patch-in-place.md): the file
//! the patches make behaves as a fresh link's, its digest is the file's, what a body edit
//! writes does not grow with the program, and a file a patch did not wholly reach is never
//! taken for the last link's.

use std::collections::BTreeMap;

use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, Relocs, Static, Target, Unwind};
use blaze_core::{Completion, Effect};
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use hegel::generators as gs;
use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

use super::{
    Change, Disk, Image, Linked, Sparse, TO, edit, first, function, name, output, program, rel32,
    returns,
};
use crate::Patch;
use crate::digest::{BLOCK, Digest, Hash};

/// How many functions a drawn program names, `f0` (the entry) to `f5`, and how many statics.
const NAMES: u8 = 6;
const STATICS: u8 = 2;

/// One function of the model: `call` each callee through its slot, `lea` its static if it
/// reads one, `mov eax, version; ret`, then `pad` bytes of `nop`.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Func {
    version: u8,
    pad: u8,
    callees: Vec<u8>,
    framed: bool,
    reads: Option<u8>,
}

impl Func {
    fn new() -> Func {
        Func { version: 0, pad: 0, callees: Vec::new(), framed: false, reads: None }
    }

    fn len(&self) -> usize {
        5 * self.callees.len() + if self.reads.is_some() { 7 } else { 0 } + 6 + usize::from(self.pad)
    }
}

/// The reference model: a program is its functions by name and its statics' values; what it
/// does is what [`normal`] reads of a fresh link of it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Program {
    funcs: BTreeMap<u8, Func>,
    values: BTreeMap<u8, u8>,
}

impl Program {
    /// `f0` calling `f1`.
    fn start() -> Program {
        let mut funcs = BTreeMap::new();
        funcs.insert(0, Func { callees: vec![1], ..Func::new() });
        funcs.insert(1, Func::new());
        Program { funcs, values: BTreeMap::new() }
    }

    fn value(&self, of: u8) -> u8 {
        self.values.get(&of).copied().unwrap_or(0xa0 + of)
    }
}

/// A function's key, as codegen keys one: its code and settings (here its name, version, size,
/// frame, static and call sites), not which key each callee has now.
fn key_of(n: u8, f: &Func) -> InstanceKey {
    let mut bytes = [0u8; 32];
    bytes[0] = n + 1;
    bytes[1] = f.version;
    bytes[2] = f.pad;
    bytes[3] = u8::from(f.framed);
    bytes[4] = f.reads.map_or(0, |it| it + 1);
    for (at, callee) in f.callees.iter().enumerate() {
        bytes[5 + at] = callee + 1;
    }
    InstanceKey(ContentId(bytes))
}

fn static_target(of: u8) -> Target {
    Target::Static { item: ItemKey(ContentId([100 + of; 32])), instance: ContentId([200 + of; 32]) }
}

fn static_of(of: u8, value: u8) -> Static {
    Static {
        item: ItemKey(ContentId([100 + of; 32])),
        instance: ContentId([200 + of; 32]),
        align: 4,
        init: Init::Bytes { bytes: vec![value; 4], relocs: Relocs::new() },
        data: Vec::new(),
        mutable: true,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

fn compiled(program: &Program, n: u8) -> CompiledFunction {
    let f = &program.funcs[&n];
    let mut code = Vec::new();
    let mut relocs = Vec::new();
    for callee in &f.callees {
        let target = Target::Function(key_of(*callee, &program.funcs[callee]));
        relocs.push(rel32(code.len() as u32 + 1, target));
        code.extend_from_slice(&[0xe8, 0, 0, 0, 0]);
    }
    if let Some(of) = f.reads {
        relocs.push(rel32(code.len() as u32 + 3, static_target(of)));
        code.extend_from_slice(&[0x48, 0x8d, 0x05, 0, 0, 0, 0]);
    }
    code.extend_from_slice(&[0xb8, f.version, 0, 0, 0, 0xc3]);
    code.resize(code.len() + usize::from(f.pad), 0x90);
    assert_eq!(code.len(), f.len());
    CompiledFunction {
        key: key_of(n, f),
        arch: Arch::X86_64,
        align: 16,
        code,
        relocs: relocs.into(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: f.framed.then(|| Unwind::Dwarf {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            cie: vec![0x0c, 7, 8, 0x90, 1],
            fde: vec![0x41, 0x0e, 0x10],
            personality: None,
            lsda: None,
        }),
        lines: Vec::new(),
        coverage: None,
        profile: None,
    }
}

/// What a build of a program hands its link: every function as filled, every static some
/// function reads, and each function's name.
struct Built {
    functions: Vec<(String, CompiledFunction)>,
    statics: Vec<Static>,
}

fn built(program: &Program) -> Built {
    let functions =
        program.funcs.keys().map(|n| (format!("f{n}"), compiled(program, *n))).collect();
    let mut read: Vec<u8> = program.funcs.values().filter_map(|it| it.reads).collect();
    read.sort_unstable();
    read.dedup();
    let statics = read.into_iter().map(|of| static_of(of, program.value(of))).collect();
    Built { functions, statics }
}

/// A bug planted in the delta a link is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A function gone from the program is not removed from the image.
    KeepsRemoved,
}

/// The delta from `old` to `new`, as blaze-cargo's relink makes it: a function or static
/// whose row changed is removed and added, one put again as it was is in neither.
fn change<'a>(old: Option<&Built>, new: &'a Built, plant: Plant) -> Change<'a> {
    let was = |it: &CompiledFunction| old.is_some_and(|old| old.functions.iter().any(|f| f.1 == *it));
    let is = |it: &CompiledFunction| new.functions.iter().any(|f| f.1 == *it);
    let named = |name: &str| new.functions.iter().any(|f| f.0 == name);
    let mut removed: Vec<InstanceKey> = old
        .map(|old| {
            old.functions
                .iter()
                .filter(|f| !is(&f.1))
                .filter(|f| plant != Plant::KeepsRemoved || named(&f.0))
                .map(|f| f.1.key)
                .collect()
        })
        .unwrap_or_default();
    removed.sort_unstable();
    removed.dedup();
    let entry = new.functions.iter().find(|f| f.0 == "f0").expect("every program has f0").1.key;
    Change {
        added: new.functions.iter().map(|f| &f.1).filter(|it| !was(it)).collect(),
        removed,
        statics: new
            .statics
            .iter()
            .filter(|it| !old.is_some_and(|old| old.statics.contains(it)))
            .collect(),
        removed_statics: old
            .map(|old| {
                old.statics
                    .iter()
                    .filter(|it| !new.statics.contains(it))
                    .map(|it| (it.item, it.instance))
                    .collect()
            })
            .unwrap_or_default(),
        names: vec![("_start".into(), Target::Direct(entry))],
        debug_names: new.functions.iter().map(|f| (f.1.key, f.0.clone())).collect(),
        ..Change::default()
    }
}

/// An image kept across links, each link taken as a patch and applied to the file held here:
/// what a caller that keeps the last file does.
struct Kept {
    image: Image,
    file: Vec<u8>,
    built: Option<Built>,
    digest: Option<Hash>,
}

impl Kept {
    fn new() -> Kept {
        Kept {
            image: Image::new(name(TO), output(true)).unwrap(),
            file: Vec::new(),
            built: None,
            digest: None,
        }
    }

    /// Links `program` as a change of the last one. `Err` when the linker refuses, when the
    /// patch is not of the file held, or when the digest it gives is not the patched file's.
    fn link(&mut self, program: &Program, plant: Plant) -> Result<Patch, String> {
        let next = built(program);
        let staged = self
            .image
            .stage(change(self.built.as_ref(), &next, plant))
            .map_err(|it| format!("the stage: {it}"))?;
        let patch = self.image.patch(staged).map_err(|it| format!("the patch: {it}"))?;
        if patch.base != self.digest {
            return Err(format!(
                "the patch is of the file {:?}, and the file held is {:?}",
                patch.base, self.digest
            ));
        }
        patch.apply(&mut self.file);
        if self.file.len() as u64 != patch.len {
            return Err(format!("the patched file is {} bytes, the patch says {}", self.file.len(), patch.len));
        }
        let whole = Digest::of(&self.file);
        if whole.root() != patch.digest {
            return Err(format!(
                "the kept digest is not the patched file's: generation {}, {} bytes in {} writes",
                patch.generation,
                patch.written(),
                patch.writes.len()
            ));
        }
        self.digest = Some(patch.digest);
        self.built = Some(next);
        Ok(patch)
    }
}

/// `program` linked from scratch: the whole file.
fn fresh(program: &Program) -> Result<Vec<u8>, String> {
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let all = built(program);
    let staged =
        image.stage(change(None, &all, Plant::None)).map_err(|it| format!("the fresh stage: {it}"))?;
    image.bytes(staged).map_err(|it| format!("the fresh link's bytes: {it}"))
}

fn linked_of(file: &[u8]) -> Linked {
    let mut sparse = Sparse::default();
    sparse.write(0, file);
    Linked::of(&sparse)
}

/// What one function is in a file, with no address in it.
#[derive(PartialEq, Eq, Debug)]
struct Body {
    /// Its code, the fields the linker fills zeroed.
    code: Vec<u8>,
    /// The function each call reaches, through its slot's stub and the jump table, by name.
    calls: Vec<String>,
    /// The bytes of the static it reads.
    reads: Option<Vec<u8>>,
    /// The length its call frame entry covers, found from its address as an unwinder finds
    /// it (`.eh_frame_hdr`'s table); `None` with no entry at its address.
    unwind: Option<u64>,
}

/// The normal form two links of one program agree on, whatever their layout: the entry, each
/// function of the program by name, and every function name the symbol table has, each as
/// many times as it has it.
#[derive(PartialEq, Eq, Debug)]
struct Normal {
    entry: String,
    bodies: BTreeMap<String, Option<Body>>,
    exported: Vec<String>,
}

/// Whether `name` is one the model gives a function: `f` and a number.
fn of_model(name: &str) -> bool {
    name.strip_prefix('f').is_some_and(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit()))
}

/// The address of `.eh_frame_hdr`'s table entry for the function at `addr`, and the header's
/// own address, which the entry's two words are relative to.
fn hdr_entry(linked: &Linked, addr: u64) -> Option<(u64, u64)> {
    let elf = linked.elf();
    let hdr = elf.section_by_name(".eh_frame_hdr")?;
    let (base, bytes) = (hdr.address(), hdr.data().ok()?);
    if bytes.len() < 12 {
        return None;
    }
    assert_eq!(
        bytes[..4],
        [1, 0x1b, 0x03, 0x3b],
        "`.eh_frame_hdr`'s version and encodings are not the ones this oracle reads"
    );
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let count = word(8) as usize;
    (0..count)
        .map(|at| 12 + 8 * at)
        .take_while(|entry| entry + 8 <= bytes.len())
        .find(|entry| (base as i64 + i64::from(word(*entry) as i32)) as u64 == addr)
        .map(|entry| (base + entry as u64, base))
}

fn unwind_of(linked: &Linked, addr: u64) -> Option<u64> {
    let (entry, base) = hdr_entry(linked, addr)?;
    let fde = (base as i64 + i64::from(linked.u32_at(entry + 4) as i32)) as u64;
    // The FDE's `pc_begin` is relative to its own field (`pcrel|sdata4`), its range after it.
    let begin = ((fde + 8) as i64 + i64::from(linked.u32_at(fde + 8) as i32)) as u64;
    (begin == addr).then(|| u64::from(linked.u32_at(fde + 12)))
}

fn normal(file: &[u8], program: &Program) -> Normal {
    let linked = linked_of(file);
    let elf = linked.elf();
    let mut by_name: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for symbol in elf.symbols() {
        if let Ok(name) = symbol.name()
            && of_model(name)
        {
            by_name.entry(name.to_owned()).or_default().push(symbol.address());
        }
    }
    let name_at = |addr: u64| {
        by_name
            .iter()
            .find(|it| it.1.contains(&addr))
            .map_or_else(|| format!("{addr:#x}, which no function is at"), |it| it.0.clone())
    };
    let mut bodies = BTreeMap::new();
    for (n, f) in &program.funcs {
        let name = format!("f{n}");
        let Some(&addr) = by_name.get(&name).and_then(|it| it.first()) else {
            bodies.insert(name, None);
            continue;
        };
        let mut code = linked.at(addr, f.len()).to_vec();
        let mut calls = Vec::new();
        for at in 0..f.callees.len() as u64 {
            let field = addr + 5 * at + 1;
            calls.push(name_at(linked.through_stub(linked.rel32(field))));
            code[(5 * at + 1) as usize..][..4].fill(0);
        }
        let reads = f.reads.map(|_| {
            let field = 5 * f.callees.len() + 3;
            code[field..field + 4].fill(0);
            linked.at(linked.rel32(addr + field as u64), 4).to_vec()
        });
        bodies.insert(name, Some(Body { code, calls, reads, unwind: unwind_of(&linked, addr) }));
    }
    let exported =
        by_name.iter().flat_map(|(name, at)| std::iter::repeat_n(name.clone(), at.len())).collect();
    Normal { entry: name_at(elf.entry()), bodies, exported }
}

/// Whether `file` behaves as a fresh link of `program` does, by [`normal`]. The fresh link
/// is held to the model first, so an oracle that reads nothing can't pass: every function is
/// there, and a framed one's entry is found and covers its code.
fn as_fresh(file: &[u8], program: &Program) -> Result<(), String> {
    let from_scratch = normal(&fresh(program)?, program);
    for (n, f) in &program.funcs {
        let Some(Some(body)) = from_scratch.bodies.get(&format!("f{n}")) else {
            return Err(format!("the fresh link has no f{n}: the oracle can't see it"));
        };
        if f.framed && body.unwind != Some(f.len() as u64) {
            return Err(format!(
                "the fresh link's frame entry for f{n} covers {:?}, and its code is {} bytes: the oracle can't see \
                 unwinding",
                body.unwind,
                f.len()
            ));
        }
    }
    let patched = normal(file, program);
    if patched != from_scratch {
        return Err(format!("the patched file is\n{patched:#?}\nand a fresh link of the program is\n{from_scratch:#?}"));
    }
    Ok(())
}

/// One edit of a program.
#[derive(Clone, Debug)]
enum Op {
    /// A body edit of the same size.
    Edit(u8, u8),
    /// A body edit to another size: smaller, or larger than the room its old body had.
    Size(u8, u8),
    Add(u8),
    Remove(u8),
    /// Its callees are the functions of this mask.
    Calls(u8, u8),
    /// Gains or loses its call frame entry.
    Frame(u8),
    /// Reads this static, or none.
    Reads(u8, Option<u8>),
    /// A static's value.
    Value(u8, u8),
}

#[hegel::composite]
fn op(tc: &hegel::TestCase) -> Op {
    let n = tc.draw(gs::integers::<u8>().max_value(NAMES - 1));
    match tc.draw(gs::integers::<u8>().max_value(7)) {
        0 => Op::Edit(n, tc.draw(gs::integers::<u8>().max_value(3))),
        1 => Op::Size(n, tc.draw(gs::integers::<u8>().max_value(200))),
        2 => Op::Add(n),
        3 => Op::Remove(n),
        4 => Op::Calls(n, tc.draw(gs::integers::<u8>().max_value((1 << NAMES) - 1))),
        5 => Op::Frame(n),
        6 => Op::Reads(n, tc.draw(gs::integers::<u8>().max_value(STATICS)).checked_sub(1)),
        _ => Op::Value(
            tc.draw(gs::integers::<u8>().max_value(STATICS - 1)),
            tc.draw(gs::integers::<u8>()),
        ),
    }
}

impl Op {
    /// `program` after this edit; `program` itself where the edit names a function it has
    /// not, or changes nothing.
    fn apply(&self, program: &Program) -> Program {
        let mut next = program.clone();
        match *self {
            Op::Edit(n, version) => {
                if let Some(f) = next.funcs.get_mut(&n) {
                    f.version = version;
                }
            }
            Op::Size(n, pad) => {
                if let Some(f) = next.funcs.get_mut(&n) {
                    f.pad = pad;
                }
            }
            Op::Add(n) => {
                next.funcs.entry(n).or_insert_with(Func::new);
            }
            Op::Remove(n) => {
                if n != 0 && next.funcs.remove(&n).is_some() {
                    for f in next.funcs.values_mut() {
                        f.callees.retain(|it| *it != n);
                    }
                }
            }
            Op::Calls(n, mask) => {
                let callees: Vec<u8> = (1..NAMES)
                    .filter(|&it| mask & (1 << it) != 0 && it != n && next.funcs.contains_key(&it))
                    .take(4)
                    .collect();
                if let Some(f) = next.funcs.get_mut(&n) {
                    f.callees = callees;
                }
            }
            Op::Frame(n) => {
                if let Some(f) = next.funcs.get_mut(&n) {
                    f.framed = !f.framed;
                }
            }
            Op::Reads(n, of) => {
                if let Some(f) = next.funcs.get_mut(&n) {
                    f.reads = of;
                }
            }
            Op::Value(of, value) => {
                next.values.insert(of, value);
            }
        }
        next
    }
}

/// `ops` applied one by one to [`Program::start`], each linked into one kept image as a
/// patch; `Err` naming the first step after which the patched file is not as a fresh link of
/// the program, a step laid the image out whole, or the linker refused.
fn patched_is_fresh(ops: &[Op], plant: Plant) -> Result<(), String> {
    let mut program = Program::start();
    let mut kept = Kept::new();
    let first = kept.link(&program, Plant::None)?;
    if !first.full || first.base.is_some() {
        return Err("the first link is not a file written whole from nothing".into());
    }
    as_fresh(&kept.file, &program)?;
    for (step, op) in ops.iter().enumerate() {
        let next = op.apply(&program);
        if next == program {
            continue;
        }
        let said = |why: String| format!("after step {step} ({op:?}): {why}");
        let patch = kept.link(&next, plant).map_err(said)?;
        if patch.full {
            return Err(said("the image was laid out whole: an edit is a patch".into()));
        }
        program = next;
        as_fresh(&kept.file, &program).map_err(said)?;
    }
    Ok(())
}

/// The refinement: after any sequence of edits (a body changed at the same size, smaller or
/// larger than the room it had, a function added or removed, a callee set changed, a frame
/// entry gained or lost, a static read, dropped or given another value), the file the
/// patches make behaves as a fresh full link of the same program: the same entry, each
/// function's code, each call resolved through its slot to the same function, each static
/// read holding the same bytes, an unwind lookup of each function finding an entry that
/// covers it, and the same function names in the symbol table. Each patch is of the file
/// the last one left, never lays the image out whole, and the digest it gives is the
/// patched file's hashed whole.
#[hegel::test(test_cases = 64)]
fn a_patched_file_behaves_as_a_fresh_link(tc: hegel::TestCase) {
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(8));
    if let Err(why) = patched_is_fresh(&ops, Plant::None) {
        panic!("{why}");
    }
}

/// One of each kind of edit, in an order that makes each do something: the directed case
/// beside the drawn ones, so each kind is linked in every run.
#[test]
fn each_kind_of_edit_patches() {
    let ops = [
        Op::Edit(1, 2),
        Op::Size(1, 3),
        Op::Size(1, 200),
        Op::Add(2),
        Op::Calls(0, 0b110),
        Op::Frame(2),
        Op::Reads(1, Some(0)),
        Op::Value(0, 0x77),
        Op::Calls(2, 0b10),
        Op::Reads(1, None),
        Op::Remove(2),
        Op::Frame(1),
    ];
    assert_eq!(patched_is_fresh(&ops, Plant::None), Ok(()));
}

fn address(linked: &Linked, name: &str) -> u64 {
    linked.symbol(name)
}

fn put(file: &mut [u8], at: u64, bytes: &[u8]) {
    // A region's file offset is its address in these images (`Linked::at`).
    file[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
}

/// A control of [`a_patched_file_behaves_as_a_fresh_link`]: a body edit of a framed function
/// patches and the oracle finds nothing; with its `.eh_frame_hdr` entry left on the address
/// its old body had (an unwind table not updated for the patch), the oracle finds no entry
/// for the function.
#[test]
fn the_oracle_catches_a_stale_unwind_entry() {
    let mut program = Program::start();
    program.funcs.get_mut(&1).unwrap().framed = true;
    let mut kept = Kept::new();
    kept.link(&program, Plant::None).unwrap();
    let old = address(&linked_of(&kept.file), "f1");
    program.funcs.get_mut(&1).unwrap().version = 2;
    kept.link(&program, Plant::None).unwrap();
    assert_eq!(as_fresh(&kept.file, &program), Ok(()));

    let linked = linked_of(&kept.file);
    let new = address(&linked, "f1");
    assert_ne!(new, old, "new code never goes where old code was");
    let (entry, base) = hdr_entry(&linked, new).expect("the patched image finds f1's frame entry");
    let mut planted = kept.file.clone();
    put(&mut planted, entry, &((old as i64 - base as i64) as i32).to_le_bytes());
    let why = as_fresh(&planted, &program).expect_err("the oracle missed a stale unwind entry");
    assert!(why.contains("unwind: None"), "{why}");
}

/// A control of [`a_patched_file_behaves_as_a_fresh_link`]: with `f1`'s jump-table entry
/// left on the body the edit replaced (a patch that placed the new code and did not move the
/// slot), the oracle names the call that reaches it.
#[test]
fn the_oracle_catches_a_slot_left_on_the_old_body() {
    let mut program = Program::start();
    let mut kept = Kept::new();
    kept.link(&program, Plant::None).unwrap();
    let old = address(&linked_of(&kept.file), "f1");
    program.funcs.get_mut(&1).unwrap().version = 2;
    kept.link(&program, Plant::None).unwrap();
    assert_eq!(as_fresh(&kept.file, &program), Ok(()));

    let linked = linked_of(&kept.file);
    let stub = linked.rel32(address(&linked, "f0") + 1);
    let table = linked.u64_at(linked.rel32(stub + 3));
    let slot = u64::from(linked.u32_at(stub + 10));
    assert_eq!(linked.u64_at(table + slot), address(&linked, "f1"));
    let mut planted = kept.file.clone();
    put(&mut planted, table + slot, &old.to_le_bytes());
    let why = as_fresh(&planted, &program).expect_err("the oracle missed a slot on the old body");
    assert!(why.contains("which no function is at"), "{why}");
}

/// A control of [`a_patched_file_behaves_as_a_fresh_link`]: with a delta that does not
/// remove a function gone from the program, the image still has it, and the oracle says the
/// symbol table names a function the fresh link has not.
#[test]
fn the_oracle_catches_a_removed_function_still_exported() {
    let ops = [Op::Add(2), Op::Calls(0, 0b110), Op::Remove(2)];
    assert_eq!(patched_is_fresh(&ops, Plant::None), Ok(()));
    let why = patched_is_fresh(&ops, Plant::KeepsRemoved)
        .expect_err("the oracle missed a removed function still exported");
    assert!(why.contains("after step 2") && why.contains("\"f2\""), "{why}");
}

/// A control of [`a_patched_file_behaves_as_a_fresh_link`]'s digest check: with an image
/// whose digest skips a block a commit wrote, the patch's digest is not the patched file's,
/// and the link says so.
#[test]
fn the_oracle_catches_a_write_the_digest_missed() {
    let mut program = Program::start();
    let mut kept = Kept::new();
    kept.link(&program, Plant::None).unwrap();
    kept.image.filed.plant_skips_a_block();
    program.funcs.get_mut(&1).unwrap().version = 2;
    let why = kept.link(&program, Plant::None).expect_err("the oracle missed a stale digest");
    assert!(why.contains("the kept digest is not the patched file's"), "{why}");
}

/// A program of `f0` calling `f1` and `fillers` more functions nothing calls, linked, then
/// `f1`'s body edited at the same size: the patch of the edit, and `f1`'s size.
fn body_edit_among(fillers: u8) -> (Patch, usize) {
    let mut program = Program::start();
    for n in 2..2 + fillers {
        program.funcs.insert(n, Func { version: n, pad: 40, ..Func::new() });
    }
    let mut kept = Kept::new();
    kept.link(&program, Plant::None).unwrap();
    program.funcs.get_mut(&1).unwrap().version = 9;
    let patch = kept.link(&program, Plant::None).unwrap();
    assert_eq!(as_fresh(&kept.file, &program), Ok(()));
    (patch, program.funcs[&1].len())
}

/// The most a body edit writes beside the function itself: its caller filled again, its
/// jump-table cell, and the blocks of the headers and tables its entries are in (the build
/// id, its symbol and name, the dynamic tables' kept blocks), each written as a
/// [`BLOCK`]-sized block and merged with a neighbour closer than a block.
const EDIT_CONSTANT: u64 = 16 * BLOCK as u64;

/// A body edit at the same size writes the function and a constant, however many functions
/// the program has: the bytes a patch writes for the edit among 200 other functions are
/// within [`EDIT_CONSTANT`] of the function's size, and no more than among 20 but for one
/// block of rounding.
#[test]
fn a_same_size_body_edit_writes_the_function_and_a_constant() {
    let (few, size) = body_edit_among(20);
    let (many, _) = body_edit_among(200);
    let ranges = |patch: &Patch| -> Vec<(u64, usize)> {
        patch.writes.iter().map(|it| (it.0, it.1.len())).collect()
    };
    for patch in [&few, &many] {
        assert!(!patch.full);
        assert!(
            patch.written() <= size as u64 + EDIT_CONSTANT,
            "{} bytes written for a body of {size}: {:x?}",
            patch.written(),
            ranges(patch)
        );
    }
    assert!(
        many.written() <= few.written() + BLOCK as u64,
        "{} bytes written among 200 functions, {} among 20: what an edit writes grows with the program: {:x?} \
         against {:x?}",
        many.written(),
        few.written(),
        ranges(&many),
        ranges(&few)
    );
    assert!(many.len > few.len, "the larger program is the larger file");
}

fn block_reader(file: &[u8]) -> impl FnMut(u64, &mut [u8; BLOCK]) + '_ {
    move |block, out| {
        let from = (block as usize).saturating_mul(BLOCK).min(file.len());
        let to = (from + BLOCK).min(file.len());
        out[..to - from].copy_from_slice(&file[from..to]);
    }
}

/// How a caller checks a file before it patches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Check {
    /// [`Image::verify`]: the file's digest.
    Digest,
    /// The planted bug: its length alone, as a `stat` would.
    Length,
}

/// A body edit's patch reaching the file only in part, as a power loss leaves a file whose
/// writes were not synced: write `i` lands when bit `i` of `landed` is set, the first that
/// does not lands its first `cut` bytes (fewer if it is shorter). `Err` when the check takes
/// a file that is not the link's for the link's, refuses the one that is, or the link after a
/// refusal is not a whole one that gives the program's file.
fn torn_is_refused(landed: u64, cut: usize, check: Check) -> Result<(), String> {
    let mut program = Program::start();
    let mut kept = Kept::new();
    kept.link(&program, Plant::None)?;
    let before = kept.file.clone();
    program.funcs.get_mut(&1).unwrap().version = 2;
    let patch = kept.link(&program, Plant::None)?;

    let mut torn = before;
    torn.resize(patch.len as usize, 0);
    let mut cut = Some(cut);
    for (at, (offset, bytes)) in patch.writes.iter().enumerate() {
        let lands = match landed >> (at % 64) & 1 {
            1 => bytes.len(),
            _ => cut.take().map_or(0, |it| it.min(bytes.len())),
        };
        torn[*offset as usize..*offset as usize + lands].copy_from_slice(&bytes[..lands]);
    }
    let whole = torn == kept.file;
    let taken = match check {
        Check::Digest => match kept.image.verify(torn.len() as u64, &mut block_reader(&torn)) {
            Ok(()) => true,
            Err(error) if error.torn() => false,
            Err(error) => return Err(format!("the check failed, and not for a torn file: {error}")),
        },
        Check::Length => torn.len() == kept.file.len(),
    };
    match (whole, taken) {
        (true, true) => return Ok(()),
        (true, false) => return Err("the link's own file was refused".into()),
        (false, true) => return Err("a torn file was taken for the link's".into()),
        (false, false) => {}
    }
    // The kept image is let go: the next link is from scratch, of the whole program.
    let mut again = Kept::new();
    let patch = again.link(&program, Plant::None)?;
    if !patch.full {
        return Err("the link after a torn file is not a whole one".into());
    }
    again
        .image
        .verify(again.file.len() as u64, &mut block_reader(&again.file))
        .map_err(|it| format!("the whole link's file: {it}"))?;
    as_fresh(&again.file, &program)
}

/// The invariant: a file a patch did not wholly reach is never taken for the last link's.
/// Whatever part of a patch's writes a crash kept, [`Image::verify`] refuses the file unless
/// it is byte for byte the link's, naming it torn, and the link from scratch that follows
/// writes the whole file, which verifies and behaves as the program.
#[hegel::test(test_cases = 64)]
fn a_torn_patch_is_never_taken_for_the_links_file(tc: hegel::TestCase) {
    let landed = tc.draw(gs::integers::<u64>());
    let cut = tc.draw(gs::integers::<usize>().max_value(2 * BLOCK));
    if let Err(why) = torn_is_refused(landed, cut, Check::Digest) {
        panic!("{why}");
    }
}

/// A control of [`a_torn_patch_is_never_taken_for_the_links_file`]: every write lost, the
/// file is the one before the edit, of the same length; the digest refuses it, and a check
/// of the length alone takes it, which the oracle says.
#[test]
fn the_oracle_catches_a_check_of_the_length_alone() {
    assert_eq!(torn_is_refused(0, 0, Check::Digest), Ok(()));
    assert_eq!(torn_is_refused(u64::MAX, 0, Check::Digest), Ok(()), "every write landed");
    let why = torn_is_refused(0, 0, Check::Length).expect_err("the oracle missed a torn file taken");
    assert!(why.contains("a torn file was taken"), "{why}");
}

/// The refusal names where: the block of the first write a crash lost.
#[test]
fn a_torn_file_is_refused_naming_its_first_torn_block() {
    let mut program = Program::start();
    let mut kept = Kept::new();
    kept.link(&program, Plant::None).unwrap();
    let before = kept.file.clone();
    program.funcs.get_mut(&1).unwrap().version = 2;
    let patch = kept.link(&program, Plant::None).unwrap();
    let mut torn = before;
    torn.resize(patch.len as usize, 0);
    let error = kept.image.verify(torn.len() as u64, &mut block_reader(&torn)).unwrap_err();
    let Some(crate::Refusal::Torn { block, len, kept: held }) = error.refusal else {
        panic!("not a torn file's refusal: {error}")
    };
    assert_eq!((len, held), (patch.len, patch.len));
    let first = (0..torn.len() / BLOCK + 1)
        .find(|at| {
            let range = at * BLOCK..((at + 1) * BLOCK).min(torn.len());
            torn[range.clone()] != kept.file[range]
        })
        .expect("the edit changed the file");
    assert_eq!(block, first as u64, "{error}");
    assert!(error.what.contains("link it from scratch") && error.what.contains(TO), "{error}");
    assert!(error.no_room().is_none(), "a torn file is no want of room");
}

/// The effects of a file commit of `change`, by kind, in the order they ran.
fn commit_kinds(image: &mut Image, disk: &mut Disk, change: Change<'_>) -> Vec<&'static str> {
    let staged = image.stage(change).unwrap();
    let mut effects = Vec::new();
    image.commit(staged, &mut effects).unwrap();
    let mut seen = Vec::new();
    loop {
        let effect = effects.pop().expect("the commit ended or asked for more");
        seen.push(match &effect {
            Effect::Sync { .. } => "sync",
            Effect::Clone { .. } => "clone",
            Effect::Rename { .. } => "rename",
            Effect::WriteAt { .. } => "write at",
            _ => "other",
        });
        let tag = effect.tag();
        let done = disk.done(&effect);
        let mut more = Vec::new();
        if let Some(result) = image.complete(&Completion { tag, done }, &mut more) {
            result.unwrap();
            return seen;
        }
        effects.extend(more);
    }
}

/// The edit path syncs nothing: a link placed into the room the image kept writes its ranges
/// into a clone and renames it, with no sync, and the digest after it is the file's. A file
/// written whole is synced before its rename.
#[test]
fn a_spliced_commit_is_not_synced_and_a_whole_one_is() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    let whole = commit_kinds(&mut image, &mut disk, first(&start, &g));
    let sync = whole.iter().position(|it| *it == "sync").expect("a whole file is synced");
    assert!(sync < whole.iter().position(|it| *it == "rename").unwrap(), "{whole:?}");

    let spliced = commit_kinds(&mut image, &mut disk, edit(&g2));
    assert!(!spliced.contains(&"sync"), "the edit path syncs nothing: {spliced:?}");
    assert!(spliced.contains(&"clone") && spliced.contains(&"rename"), "{spliced:?}");
    let file = super::contents(disk.file(TO));
    assert_eq!(image.digest().unwrap(), Digest::of(&file).root(), "the digest is the file's");
    image.verify(file.len() as u64, &mut block_reader(&file)).unwrap();

    // A rollback's file is the digest's too.
    disk.roll_back(&mut image).unwrap();
    let file = super::contents(disk.file(TO));
    assert_eq!(image.digest().unwrap(), Digest::of(&file).root(), "the digest follows a rollback");
}
