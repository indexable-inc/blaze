//! The table switching on an image made in the test's own memory: a descriptor, a table's
//! header (with an execution log's table and map, or without), a two-slot table of two
//! functions, and buffers for the patch regions, with the platform's writes done by
//! [`Fake`]. Calls through a log stub are made as the linker's stub makes them
//! ([`Program::target`]).

#![allow(unsafe_code)]

use std::{
    collections::HashMap,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_ids::rng::SplitMix64;

use super::{
    Outcome,
    exec::{self, Header, Tables},
    hook, image,
    switch::{Dwarf, HOST_ARCH, KEPT, Layout, Limits, Memory, POOL, Span, Switch, Writes},
};
use crate::patch::{
    Answer, Code, DESCRIPTOR_MAGIC, DESCRIPTOR_VERSION, Descriptor, FIRST_GENERATION, Message,
    Patch,
    exec::{self as words, CELL, CURRENT, LOADER, MAGIC, MAP, REGION_HEADER, SLOTS, STUBS},
};

/// The slide of the fake image: every address it says is this much below where it is.
const SLIDE: u64 = 0x7_0000;
const ID: [u8; 16] = [0x42; 16];

extern "C-unwind" fn one() -> u32 {
    1
}

extern "C-unwind" fn two() -> u32 {
    2
}

#[inline(never)]
extern "C-unwind" fn boom() -> u32 {
    panic!("boom in a patch")
}

fn address(f: extern "C-unwind" fn() -> u32) -> u64 {
    f as usize as u64
}

fn unslid(address: u64) -> u64 {
    address.wrapping_sub(SLIDE)
}

#[derive(Default)]
struct Fake {
    frames: Vec<u64>,
    functions: Vec<(u64, u64, u64)>,
    published: usize,
    /// Memory the fake image does not have: a range touching it is refused as a platform
    /// refuses an address outside the image.
    outside: Option<Range<u64>>,
}

impl Memory for Fake {
    const DWARF: Dwarf = Dwarf::Sections;
    const SEH: bool = true;

    fn cell(&self, cell: u64) -> u64 {
        // SAFETY: a word of the `Program`'s header, an 8-aligned `AtomicU64` that outlives
        // the switch.
        unsafe { image::cell(cell) }.load(Ordering::Acquire)
    }

    fn in_image(&self, range: &Range<u64>) -> Result<(), String> {
        match &self.outside {
            Some(outside) if range.start < outside.end && outside.start < range.end => {
                Err(format!("{:#x}..{:#x}, outside the image", range.start, range.end))
            }
            _ => Ok(()),
        }
    }

    fn bytes(&self, range: &Range<u64>) -> Result<&'static [u8], String> {
        self.in_image(range)?;
        // SAFETY: what the switch reads is a patch's code, in the patch code region: the
        // `Program`'s code buffer or a function's own code, readable and outliving the switch,
        // and written only by the test between patches, as the daemon's commit writes it.
        Ok(unsafe { image::view(range.start, (range.end - range.start) as usize) })
    }

    fn loaders(&self, at: u64, slots: u64) -> &[AtomicU64] {
        // SAFETY: the table is the `Program`'s `AtomicU64`s, `slots` long (`Switch::new`
        // checked the header names it), and outlives the switch.
        unsafe { image::words(at, slots as usize) }
    }

    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        // SAFETY: every data write the tests make is to a buffer of their `Program`, which
        // outlives the switch; the switch checks only that it misses the code region, the
        // cell, the loader's table and lasting memory, so the tests keep to the buffers.
        unsafe { writes.store_in_place() };
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        self.frames.extend_from_slice(starts);
        Ok(())
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        self.functions.extend_from_slice(functions);
        Ok(())
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        // SAFETY: a word of the `Program`'s header, an `AtomicU64` that outlives the switch.
        unsafe { AtomicU64::from_ptr(cell as *mut u64) }.store(table, Ordering::Release);
        self.published += 1;
        Ok(())
    }
}

/// Where the fake log stubs are said to be, and how far apart: never run, only compared.
const LOG_STUBS: u64 = 0x5_0000_0000;
const LOG_STRIDE: u64 = 16;

fn log_stub(slot: u64) -> u64 {
    LOG_STUBS + slot * LOG_STRIDE
}

/// The parts of an image a switch works on, in the test's memory.
struct Program {
    /// The table's header as the linker lays it out ([`words`]): with a log, its log table
    /// and private map follow.
    header: Box<[AtomicU64]>,
    /// The loader's table.
    table: Box<[AtomicU64]>,
    code: Box<[u64]>,
    data: Box<[u64]>,
    /// The patch code region: `code`'s buffer, or a function's own code.
    region: Range<u64>,
    /// The tables of the program's switch.
    tables: &'static Tables,
}

impl Program {
    fn new() -> Program {
        Program::with_slots(2)
    }

    /// A program whose table has `slots` slots, of `one` and `two` in turn.
    fn with_slots(slots: usize) -> Program {
        Program::linked(slots, false)
    }

    /// A program of `slots` slots linked with an execution log, or without one.
    fn linked(slots: usize, logged: bool) -> Program {
        let table: Box<[AtomicU64]> = (0..slots)
            .map(|it| AtomicU64::new(if it % 2 == 0 { address(one) } else { address(two) }))
            .collect();
        let len = words::len(slots as u64, logged) / 8;
        let header: Box<[AtomicU64]> = (0..len).map(|_| AtomicU64::new(0)).collect();
        let at = header.as_ptr() as u64;
        let word = |offset: u64| &header[(offset / 8) as usize];
        let loader = table.as_ptr() as u64;
        word(CURRENT).store(loader, Ordering::Relaxed);
        word(LOADER).store(loader, Ordering::Relaxed);
        if logged {
            word(CELL).store(at + words::log_word(0), Ordering::Relaxed);
            word(MAP).store(at + words::private_map(slots as u64), Ordering::Relaxed);
            word(STUBS).store(LOG_STUBS, Ordering::Relaxed);
            word(SLOTS).store(slots as u64 | LOG_STRIDE << 32, Ordering::Relaxed);
            for slot in 0..slots as u64 {
                word(words::log_word(slot)).store(log_stub(slot), Ordering::Relaxed);
            }
        } else {
            word(CELL).store(loader, Ordering::Relaxed);
            word(SLOTS).store(slots as u64, Ordering::Relaxed);
        }
        let code = vec![0u64; 512].into_boxed_slice();
        let region = code.as_ptr() as u64..code.as_ptr() as u64 + 512 * 8;
        let data = vec![0u64; 64].into_boxed_slice();
        let tables = Box::leak(Box::new(Tables::new()));
        Program { header, table, code, data, region, tables }
    }

    /// The table cell, the header's first word.
    fn cell(&self) -> &AtomicU64 {
        &self.header[0]
    }

    fn word(&self, offset: u64) -> &AtomicU64 {
        &self.header[(offset / 8) as usize]
    }

    /// The loader's table as it is now.
    fn loaders(&self) -> Vec<u64> {
        self.table.iter().map(|it| it.load(Ordering::Relaxed)).collect()
    }

    /// The header as the runtime reads it.
    fn exec_header(&self) -> Header {
        let at = self.header.as_ptr() as u64;
        // SAFETY: the header and the loader's table are the program's `AtomicU64`s, which
        // outlive the test; the fake image has all of memory.
        unsafe { Header::checked(at, self.table.len() as u64, |_| Ok(())) }.expect("a header")
    }

    /// Where the private map is, which the log stubs mark until a region is armed.
    fn private_map(&self) -> u64 {
        self.header.as_ptr() as u64 + words::private_map(self.table.len() as u64)
    }

    /// Where the call through `slot` lands, as the linker's stubs take it: the cell, then
    /// the slot's word, and when that is the slot's log stub, what the stub does (marks the
    /// slot in the map, copies the current table's word into the log table and lands there).
    fn target(&self, slot: usize) -> u64 {
        let table = self.cell().load(Ordering::Acquire);
        // SAFETY: the cell holds the loader's table, a table of the pool or the log table,
        // each `slots` 8-aligned `AtomicU64`s that outlive the test.
        let word = unsafe { image::cell(table + slot as u64 * 8) };
        let target = word.load(Ordering::Acquire);
        if target != log_stub(slot as u64) {
            return target;
        }
        let map = self.word(MAP).load(Ordering::Acquire);
        // SAFETY: the map is the private map or an armed region, `16 + slots` bytes that
        // outlive the test.
        unsafe { image::byte(map + REGION_HEADER + slot as u64) }.store(1, Ordering::Relaxed);
        let current = self.word(CURRENT).load(Ordering::Acquire);
        // SAFETY: the current table, as above.
        let target = unsafe { image::cell(current + slot as u64 * 8) }.load(Ordering::Acquire);
        word.store(target, Ordering::Relaxed);
        target
    }

    fn code_at(&self, offset: u64) -> u64 {
        self.region.start + offset
    }

    /// Writes `bytes` at `offset` of the code buffer, as the daemon's commit writes a patch's
    /// code into the code memory: the patch's entry for it, named `name`.
    fn commit(&mut self, offset: u64, bytes: &[u8], name: &str) -> Code {
        let buffer = self.code.as_mut_ptr() as u64;
        assert_eq!(self.region.start, buffer, "the patch code region is the code buffer");
        assert!(offset as usize + bytes.len() <= self.code.len() * 8, "within the code buffer");
        // SAFETY: `offset..offset + len` is within `code`'s buffer (asserted), borrowed mutably
        // here; the switch reads it only while it takes a patch.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), (buffer + offset) as *mut u8, bytes.len())
        };
        Code::of(unslid(buffer + offset), bytes, name)
    }

    /// The entry of `len` bytes of the patch code region at `offset`, as they are now: code
    /// the region has already, such as a function's own.
    fn present(&self, offset: u64, len: u64, name: &str) -> Code {
        let at = self.code_at(offset);
        // SAFETY: `at..at + len` is in the patch code region, the code buffer or a function's
        // own code, readable for the test.
        let bytes = unsafe { image::view(at, len as usize) };
        Code::of(unslid(at), bytes, name)
    }

    fn data_at(&self, offset: u64) -> u64 {
        self.data.as_ptr() as u64 + offset
    }

    fn switch(&self) -> Switch<Fake> {
        self.switch_at(FIRST_GENERATION)
    }

    /// A switch of the program, from its descriptor as the linker writes it for an image
    /// linked at `generation`.
    fn switch_at(&self, generation: u64) -> Switch<Fake> {
        self.switch_with(Fake::default(), generation).expect("a switch")
    }

    /// A switch of the program whose log and lasting ranges hold at most `limits`.
    fn switch_limited(&self, limits: &Limits) -> Switch<Fake> {
        self.switch_with_limits(Fake::default(), FIRST_GENERATION, limits).expect("a switch")
    }

    fn switch_with(&self, fake: Fake, generation: u64) -> Result<Switch<Fake>, String> {
        self.switch_with_limits(fake, generation, &Limits::LONG)
    }

    fn switch_with_limits(
        &self,
        fake: Fake,
        generation: u64,
        limits: &Limits,
    ) -> Result<Switch<Fake>, String> {
        let mut bytes = vec![0u8; Descriptor::SIZE];
        let at = bytes.as_ptr() as u64;
        let descriptor = Descriptor {
            magic: DESCRIPTOR_MAGIC,
            version: DESCRIPTOR_VERSION,
            arch: HOST_ARCH,
            id: ID,
            generation,
            this: unslid(at),
            table: unslid(self.cell().as_ptr() as u64),
            slots: self.table.len() as u64,
            patch_code: [unslid(self.region.start), unslid(self.region.end)],
            patch_data: [unslid(self.data_at(0)), unslid(self.data_at(64 * 8))],
        };
        bytes.copy_from_slice(&descriptor.encode());
        let decoded = Descriptor::decode(&bytes).expect("a descriptor");
        let layout = Layout::new(&decoded, at).expect("a layout");
        assert_eq!(layout.slide, SLIDE);
        Switch::with_limits(fake, layout, limits, self.tables)
    }

    /// Calls slot `slot` as code the linker made does: the cell, then the slot, through its
    /// log stub while that is there.
    fn call(&self, slot: usize) -> u32 {
        let f = self.target(slot);
        // SAFETY: the table holds addresses of `extern "C-unwind" fn() -> u32`.
        let f: extern "C-unwind" fn() -> u32 = unsafe { std::mem::transmute(f as usize) };
        f()
    }

    fn calls(&self) -> (u32, u32) {
        (self.call(0), self.call(1))
    }

    /// Every slot of the table the cell points at.
    fn slots(&self) -> Vec<u64> {
        let table = self.cell().load(Ordering::Acquire);
        // SAFETY: the cell holds a table of `self.table.len()` 8-aligned `AtomicU64`s that
        // outlive the test.
        let word = |slot: usize| unsafe { image::cell(table + slot as u64 * 8) };
        (0..self.table.len()).map(|it| word(it).load(Ordering::Acquire)).collect()
    }
}

fn patch(generation: u64, base: u64, cells: Vec<(u32, u64)>) -> Patch {
    Patch { id: ID, generation, base, functions: cells.len() as u64, cells, ..Patch::default() }
}

/// An owned copy of `answer`, read back from its bytes as the linker reads it.
fn read_back(answer: &Answer) -> Answer {
    Answer::decode(&answer.encode()).expect("an answer reads back")
}

fn applied(switch: &mut Switch<Fake>, patch: Patch) {
    let generation = patch.generation;
    let (outcome, answer) = switch.handle(Message::Patch(patch));
    assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome}");
    assert_eq!(answer, Some(Answer::Applied { generation }));
}

/// A header or a loader's table the image doesn't have is refused before the switch reads
/// it, as are a header naming no loader's table and one that starts elsewhere.
#[test]
fn a_table_outside_the_image_is_refused() {
    let program = Program::new();
    let cell = program.cell().as_ptr() as u64;
    let table = program.table.as_ptr() as u64;
    for (outside, named) in [(cell..cell + 8, cell), (table + 8..table + 9, table)] {
        let fake = Fake { outside: Some(outside), ..Fake::default() };
        let refused = program.switch_with(fake, FIRST_GENERATION).err().expect("refused");
        assert!(refused.contains(&format!("{named:#x}")), "{refused}");
    }
    let zero = Program::new();
    zero.word(LOADER).store(0, Ordering::Relaxed);
    let refused = zero.switch_with(Fake::default(), FIRST_GENERATION).err().expect("refused");
    assert!(refused.contains("holds 0x0"), "{refused}");
    for logged in [false, true] {
        let elsewhere = Program::linked(2, logged);
        elsewhere.cell().store(elsewhere.data_at(0), Ordering::Relaxed);
        let refused = elsewhere.switch_with(Fake::default(), FIRST_GENERATION).err();
        let refused = refused.expect("refused");
        assert!(refused.contains(&format!("starts on {:#x}", elsewhere.data_at(0))), "{refused}");
    }
    let other = Program::new();
    other.word(SLOTS).store(3, Ordering::Relaxed);
    let refused = other.switch_with(Fake::default(), FIRST_GENERATION).err().expect("refused");
    assert!(refused.contains("has 3 slots"), "{refused}");
}

#[test]
fn patches_switch_the_table_and_roll_back() {
    let mut program = Program::new();
    let mut switch = program.switch();
    let image_table = program.cell().load(Ordering::Relaxed);
    assert_eq!(program.calls(), (1, 2));

    // An FDE and the zero length after it, in the code the commit wrote; a static holding an
    // address to rebase in the data.
    let mut code = vec![0u8; 32];
    code[8..12].copy_from_slice(&12u32.to_le_bytes());
    code[12..16].copy_from_slice(&1u32.to_le_bytes());
    let mut first = patch(1, 0, vec![(0, unslid(address(two))), (1, unslid(address(one)))]);
    first.code = vec![program.commit(0, &code, "first")];
    first.data =
        vec![(unslid(program.data_at(0)), unslid(program.data_at(8)).to_le_bytes().into())];
    first.rebase = vec![unslid(program.data_at(0))];
    first.frames = vec![(unslid(program.code_at(8)), 16)];
    first.windows =
        vec![(unslid(program.code_at(0)), unslid(program.code_at(8)), unslid(program.code_at(24)))];
    first.statics = 1;
    let (outcome, answer) = switch.handle(Message::Patch(first));
    assert_eq!(outcome, Outcome::Applied { generation: 1, functions: 2, statics: 1 });
    assert_eq!(answer, Some(Answer::Applied { generation: 1 }));
    assert_eq!(program.calls(), (2, 1));
    assert_eq!(program.data[0], program.data_at(8));
    assert_eq!(switch.memory.frames, vec![program.code_at(8)]);
    assert_eq!(
        switch.memory.functions,
        vec![(program.code_at(0), program.code_at(8), program.code_at(24))]
    );
    assert_eq!(switch.current(), 1);

    let (outcome, _) = switch.handle(Message::Patch(patch(2, 0, vec![(0, unslid(address(one)))])));
    assert!(
        matches!(&outcome, Outcome::Declined { generation: 2, why } if why.contains("is at 1")),
        "{outcome}"
    );
    assert_eq!(program.calls(), (2, 1));

    let mut second = patch(2, 1, vec![(1, unslid(address(two)))]);
    second.code = vec![program.commit(64, &[0xcc; 16], "second")];
    applied(&mut switch, second);
    assert_eq!(program.calls(), (2, 2));

    let mut over = patch(3, 2, vec![]);
    over.code = vec![program.present(16, 8, "over")];
    let (outcome, _) = switch.handle(Message::Patch(over));
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. } if why.contains("can be running")),
        "{outcome}"
    );

    let (outcome, answer) = switch.handle(Message::RollBack { id: ID, to: 1 });
    let why = "the linker took it back".to_owned();
    assert_eq!(outcome, Outcome::RolledBack { generation: 2, to: 1, why: why.clone() });
    assert_eq!(answer, Some(Answer::RolledBack { generation: 2, to: 1, why }));
    assert_eq!(program.calls(), (2, 1));
    switch.handle(Message::RollBack { id: ID, to: 0 });
    assert_eq!(program.calls(), (1, 2));
    assert_eq!(program.cell().load(Ordering::Relaxed), image_table);

    // Code rolled back stays: a thread can be in it still.
    let mut over = patch(3, 0, vec![(1, unslid(address(one)))]);
    over.code = vec![program.present(0, 8, "over")];
    let (outcome, _) = switch.handle(Message::Patch(over));
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. }
            if why.contains("generation 1's code") && why.contains("can be running")),
        "{outcome}"
    );
    let mut third = patch(3, 0, vec![(1, unslid(address(one)))]);
    third.code = vec![program.commit(128, &[0x90; 8], "third")];
    applied(&mut switch, third);
    assert_eq!(switch.kept(), vec![0, 1, 2, 3]);
    assert_eq!(program.calls(), (1, 1));
    switch.handle(Message::RollBack { id: ID, to: 2 });
    assert_eq!(program.calls(), (2, 2));
    switch.handle(Message::RollBack { id: ID, to: 3 });

    let restart = Message::Restart { id: ID, generation: 3, why: "a layout changed".into() };
    let (outcome, answer) = switch.handle(restart);
    assert_eq!(outcome, Outcome::Restart { generation: 3, why: "a layout changed".into() });
    assert_eq!(answer, None);
    let (outcome, _) = switch.handle(Message::RollBack { id: [0; 16], to: 0 });
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. } if why.contains("for image")),
        "{outcome}"
    );
    assert_eq!(program.calls(), (1, 1));
}

#[test]
fn a_program_starts_at_the_generation_it_was_linked_at() {
    let program = Program::new();
    let mut switch = program.switch_at(7);
    assert_eq!(switch.current(), 7);
    let (outcome, _) = switch.handle(Message::Patch(patch(1, 0, vec![])));
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. } if why.contains("where this program is at 7")),
        "{outcome}"
    );
    applied(&mut switch, patch(8, 7, vec![(0, unslid(address(two)))]));
    assert_eq!(program.calls(), (2, 2));
    switch.handle(Message::RollBack { id: ID, to: 7 });
    assert_eq!(program.calls(), (1, 2));
    assert_eq!(switch.kept(), vec![7, 8]);
}

#[test]
fn a_bad_patch_is_declined_before_anything_is_written() {
    let mut program = Program::new();
    let mut unterminated = vec![0u8; 16];
    unterminated[..4].copy_from_slice(&12u32.to_le_bytes());
    unterminated[4..8].copy_from_slice(&1u32.to_le_bytes());
    let unterminated = program.commit(256, &unterminated, "unterminated");
    let unwinds = program.commit(384, &[0, 0, 0, 0, 0, 0, 4, 0], "unwinds");
    let mut switch = program.switch();
    let table = program.cell().load(Ordering::Relaxed);
    let region = unslid(program.code_at(0));
    let data = unslid(program.data_at(0));
    let with = |change: &dyn Fn(&mut Patch)| {
        let mut patch = patch(1, 0, vec![]);
        change(&mut patch);
        patch
    };
    let cases: Vec<(Patch, &str)> = vec![
        (with(&|it| it.id = [0; 16]), "for image"),
        (patch(1, 1, vec![]), "where this program is at 0"),
        (patch(0, 0, vec![]), "counts up from its base"),
        (
            with(&|it| it.code = vec![Code::of(unslid(program.region.end), &[1], "past")]),
            "`past`'s code at",
        ),
        (
            with(&|it| it.code = vec![Code { hash: [0; 32], ..program.present(0, 8, "edited") }]),
            "is not what the daemon committed",
        ),
        (with(&|it| it.data = vec![(region, vec![1])]), "in the patch code region"),
        (
            with(&|it| it.data = vec![(unslid(program.cell().as_ptr() as u64), vec![0; 8])]),
            "the jump table's cell",
        ),
        (patch(1, 0, vec![(2, 0)]), "slot 2, where the table has 2"),
        (patch(1, 0, vec![(0, 0), (0, 0)]), "slot 0 set twice"),
        (with(&|it| it.rebase = vec![region]), "in none of the patch's writes"),
        (
            with(&|it| {
                it.data = vec![(data, vec![1; 8])];
                it.rebase = vec![data, data];
            }),
            "rebased twice",
        ),
        (
            with(&|it| {
                it.code = vec![program.present(0, 8, "f")];
                it.rebase = vec![region];
            }),
            "an address to rebase at",
        ),
        (
            with(&|it| it.code = vec![program.present(0, 16, "f"), program.present(8, 8, "g")]),
            "two functions' code",
        ),
        (
            with(&|it| {
                it.code = vec![program.present(0, 16, "f")];
                it.frames = vec![(region, 16)];
            }),
            "its length says 4",
        ),
        (
            with(&|it| {
                it.code = vec![unterminated.clone()];
                it.frames = vec![(unterminated.at, 16)];
            }),
            "unwind entries reach",
        ),
        (with(&|it| it.windows = vec![(region, region, region)]), "a RUNTIME_FUNCTION"),
        (
            with(&|it| {
                it.code = vec![unwinds.clone()];
                it.windows = vec![(unwinds.at, unwinds.at + 4, unwinds.at + 4)];
            }),
            "runs past the patch's writes (12 bytes)",
        ),
        (
            with(&|it| it.data = vec![(unslid(program.table.as_ptr() as u64), vec![0; 8])]),
            "the loader's jump table",
        ),
    ];
    for (bad, says) in cases {
        let (outcome, answer) = switch.handle(Message::Patch(bad));
        match &outcome {
            Outcome::Declined { why, .. } => {
                assert!(why.contains(says), "{why:?} doesn't say {says:?}")
            }
            other => panic!("{other}, where it should say {says:?}"),
        }
        assert!(matches!(answer, Some(Answer::Declined { .. })), "{answer:?}");
        assert_eq!(program.cell().load(Ordering::Relaxed), table);
        assert_eq!(switch.memory.published, 0);
        assert_eq!(switch.current(), 0);
    }
    assert!(program.code[..32].iter().chain(program.data.iter()).all(|it| *it == 0));
    assert!(switch.memory.frames.is_empty() && switch.memory.functions.is_empty());
}

/// Data the image doesn't have, after data it has and code: refused with none of it written,
/// where the platform's own check of each span as it writes would have written the first.
#[test]
fn data_outside_the_image_is_refused_before_any_write() {
    let program = Program::new();
    let outside = program.data_at(8)..program.data_at(16);
    let fake = Fake { outside: Some(outside), ..Fake::default() };
    let mut switch = program.switch_with(fake, FIRST_GENERATION).expect("a switch");
    let mut bad = patch(1, 0, vec![(0, unslid(address(two)))]);
    bad.code = vec![program.present(0, 8, "f")];
    bad.data =
        vec![(unslid(program.data_at(0)), vec![1; 8]), (unslid(program.data_at(8)), vec![1; 8])];
    let (outcome, _) = switch.handle(Message::Patch(bad));
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. } if why.contains("outside the image")),
        "{outcome}"
    );
    assert!(program.code.iter().chain(program.data.iter()).all(|it| *it == 0));
    assert_eq!(switch.memory.published, 0);
    assert_eq!(program.calls(), (1, 2));
}

#[test]
fn the_last_generations_are_kept() {
    let program = Program::new();
    let mut switch = program.switch();
    for generation in 1..=100u64 {
        let target = if generation % 2 == 0 { address(one) } else { address(two) };
        applied(&mut switch, patch(generation, generation - 1, vec![(0, unslid(target))]));
        assert!(switch.pooled() <= POOL);
    }
    assert_eq!(program.call(0), 1);
    assert_eq!(switch.kept(), (101 - KEPT as u64..=100).collect::<Vec<_>>());
    let (outcome, _) = switch.handle(Message::RollBack { id: ID, to: 1 });
    assert!(matches!(outcome, Outcome::Declined { generation: 1, .. }), "{outcome}");
    switch.handle(Message::RollBack { id: ID, to: 99 });
    assert_eq!(program.call(0), 2);
    assert_eq!(switch.current(), 99);
}

#[test]
fn a_panic_is_tied_to_the_newest_generation_of_its_code() {
    let program = Program::new();
    let mut switch = program.switch();
    let mut first = patch(1, 0, vec![(0, unslid(address(two)))]);
    first.code = vec![program.present(0, 64, "first")];
    applied(&mut switch, first);
    let mut second = patch(2, 1, vec![]);
    second.code = vec![program.present(64, 64, "second")];
    applied(&mut switch, second);

    assert_eq!(switch.tie(&[program.code_at(10)]), Some((1, 0)));
    assert_eq!(switch.tie(&[program.code_at(70), program.code_at(10)]), Some((2, 1)));
    assert_eq!(switch.tie(&[program.code_at(10), program.code_at(127)]), Some((2, 1)));
    assert_eq!(switch.tie(&[program.code_at(128), address(one)]), None);
    assert_eq!(switch.tie(&[]), None);
    switch.roll_back(1).expect("generation 1 is kept");
    assert_eq!(switch.tie(&[program.code_at(70)]), None, "not what the program runs");
    assert_eq!(switch.tie(&[program.code_at(70), program.code_at(10)]), Some((1, 0)));
}

/// A real panic, in a function standing for a generation's code, reached through its slot.
#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
))]
#[test]
fn a_panic_in_a_generations_code_rolls_it_back() {
    hook::install();
    let mut program = Program::new();
    program.region = address(boom)..address(boom) + 256;
    let mut switch = program.switch();
    let mut first = patch(1, 0, vec![(0, unslid(address(boom)))]);
    first.code = vec![program.present(0, 256, "boom")];
    applied(&mut switch, first);

    let mut answers = Vec::new();
    let result = switch.guard(|| program.call(0), &mut |it| answers.push(read_back(it)));
    let why = match result {
        Err(Outcome::RolledBack { generation: 1, to: 0, why }) => why,
        other => panic!("{other:?}"),
    };
    assert!(why.starts_with("panicked at ") && why.contains("tests.rs"), "{why}");
    assert!(why.ends_with(": boom in a patch"), "{why}");
    assert_eq!(answers, vec![Answer::RolledBack { generation: 1, to: 0, why }]);
    assert_eq!(switch.current(), 0);
    assert_eq!(program.call(0), 1);
    assert_eq!(switch.guard(|| program.call(0), &mut |_| {}), Ok(1));
}

#[test]
fn a_panic_elsewhere_is_resumed() {
    hook::install();
    let program = Program::new();
    let mut switch = program.switch();
    let mut first = patch(1, 0, vec![]);
    first.code = vec![program.present(0, 64, "first")];
    applied(&mut switch, first);

    let mut answers = Vec::new();
    let resumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        switch.guard(|| -> u32 { panic!("not in a patch") }, &mut |it| answers.push(read_back(it)))
    }));
    let payload = resumed.expect_err("the panic is resumed");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"not in a patch"));
    assert!(answers.is_empty());
    assert_eq!(switch.current(), 1);
    assert_eq!(switch.guard(|| 7, &mut |_| {}), Ok(7));
}

#[test]
fn registered_unwind_entries_are_never_written_again() {
    let program = Program::new();
    let mut switch = program.switch();
    // An FDE of 16 bytes, then the zero length ending its run, in the data.
    let mut entries = vec![0u8; 20];
    entries[..4].copy_from_slice(&12u32.to_le_bytes());
    let mut first = patch(1, 0, vec![(0, unslid(address(two)))]);
    first.data = vec![(unslid(program.data_at(0)), entries)];
    first.frames = vec![(unslid(program.data_at(0)), 16)];
    applied(&mut switch, first);
    assert_eq!(switch.memory.frames, vec![program.data_at(0)]);

    for (at, len) in [(0, 8), (12, 8), (16, 4), (18, 8)] {
        let mut over = patch(2, 1, vec![]);
        over.data = vec![(unslid(program.data_at(at)), vec![1; len])];
        let (outcome, _) = switch.handle(Message::Patch(over));
        assert!(
            matches!(&outcome, Outcome::Declined { why, .. }
                if why.contains("generation 1's unwind entries")),
            "{at}: {outcome}"
        );
    }
    let mut after = patch(2, 1, vec![]);
    after.data = vec![(unslid(program.data_at(20)), vec![1; 4])];
    applied(&mut switch, after);

    // Rolled back, the generation's entries stay registered, and stay as they are.
    switch.roll_back(0).expect("generation 0 is kept");
    let mut over = patch(3, 0, vec![]);
    over.data = vec![(unslid(program.data_at(0)), vec![0; 20])];
    let (outcome, _) = switch.handle(Message::Patch(over));
    assert!(matches!(outcome, Outcome::Declined { .. }), "{outcome}");
    assert_eq!(program.data[0], 12);
}

/// The pool's tables, brought from one generation to another by undoing and redoing cells,
/// against a whole table of every generation, over patches and rollbacks that branch.
#[test]
fn the_pool_holds_what_a_whole_table_per_generation_would() {
    const SLOTS: usize = 16;
    let program = Program::with_slots(SLOTS);
    let mut switch = program.switch();
    let mut model = HashMap::from([(FIRST_GENERATION, program.loaders())]);
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    let mut next = FIRST_GENERATION + 1;
    for _ in 0..2000 {
        if rng.one_in(3) {
            let kept = switch.kept();
            let to = kept[rng.index(kept.len())];
            switch.roll_back(to).expect("a kept generation");
        } else {
            let base = switch.current();
            let mut table = model[&base].clone();
            let mut cells = Vec::new();
            for slot in 0..SLOTS {
                if rng.one_in(4) {
                    let target = 0x10_0000 + rng.below(1 << 20) * 8;
                    table[slot] = target;
                    cells.push((slot as u32, unslid(target)));
                }
            }
            applied(&mut switch, patch(next, base, cells));
            model.insert(next, table);
            next += 1;
        }
        assert!(switch.pooled() <= POOL);
        assert_eq!(program.slots(), model[&switch.current()], "at {}", switch.current());
    }
    assert_eq!(switch.pooled(), POOL);
}

#[test]
fn a_rollback_to_a_generation_a_table_holds_writes_no_table() {
    let program = Program::new();
    let image = program.cell().load(Ordering::Relaxed);
    let mut switch = program.switch();
    applied(&mut switch, patch(1, 0, vec![(0, unslid(address(two)))]));
    let first = switch.table_of(1).expect("a table holds generation 1");
    applied(&mut switch, patch(2, 1, vec![(1, unslid(address(one)))]));
    let second = switch.table_of(2).expect("a table holds generation 2");
    assert_ne!(first, second);
    assert_eq!(switch.table_of(1), Some(first));
    assert_eq!(program.calls(), (2, 1));
    let pooled = switch.pooled();

    switch.roll_back(1).expect("generation 1 is kept");
    assert_eq!(program.cell().load(Ordering::Relaxed), first);
    assert_eq!(program.calls(), (2, 2));
    switch.roll_back(0).expect("generation 0 is kept");
    assert_eq!(program.cell().load(Ordering::Relaxed), image);
    assert_eq!(switch.table_of(0), Some(image));
    switch.roll_back(2).expect("generation 2 is kept");
    assert_eq!(program.cell().load(Ordering::Relaxed), second);
    assert_eq!(program.calls(), (2, 1));
    assert_eq!(switch.pooled(), pooled);
}

/// Before its switch, a patch stores its own cells and nothing else: the table it goes
/// into was brought to its base right after the switch before, however full the pool.
#[test]
fn a_patch_stores_only_its_own_cells_before_the_switch() {
    const SLOTS: usize = 64;
    let program = Program::with_slots(SLOTS);
    let mut switch = program.switch();
    let mut table = program.loaders();
    for generation in 1..=4 * POOL as u64 {
        let cells: Vec<(u32, u64)> = (0..generation as u32 % 7 + 1)
            .map(|it| {
                let slot = (it * 9 + generation as u32) % SLOTS as u32;
                (slot, unslid(0x20_0000 + generation * 0x100 + u64::from(it) * 8))
            })
            .collect();
        for &(slot, target) in &cells {
            table[slot as usize] = target.wrapping_add(SLIDE);
        }
        let count = cells.len();
        applied(&mut switch, patch(generation, generation - 1, cells));
        assert_eq!(switch.stored(), count, "generation {generation}");
        assert_eq!(program.slots(), table, "generation {generation}");
    }
    assert_eq!(switch.pooled(), POOL);
}

/// `put` of every range against the bytes written whole and rebased one field at a time,
/// for ranges that cut spans and rebased fields anywhere.
#[test]
fn a_write_puts_any_part_of_its_spans_rebased() {
    const START: u64 = 0x1000;
    let message: Vec<u8> = (0..64u8).collect();
    let spans = [Span::new(START, 0, 24), Span::new(START + 30, 40, 16)];
    let rebase = [START + 4, START + 16, START + 36];
    let slide = 0x0123_4567_89ab_cdef;
    let writes = Writes::new(&message, &spans, &rebase, slide);

    let mut whole = [None::<u8>; 48];
    for span in &spans {
        for (offset, byte) in writes.bytes(span).iter().enumerate() {
            whole[(span.at - START) as usize + offset] = Some(*byte);
        }
    }
    for field in rebase {
        let at = (field - START) as usize;
        let bytes: Vec<u8> = whole[at..at + 8].iter().map(|it| it.expect("in a span")).collect();
        let value = u64::from_le_bytes(bytes.try_into().expect("8 bytes")).wrapping_add(slide);
        for (offset, byte) in value.to_le_bytes().into_iter().enumerate() {
            whole[at + offset] = Some(byte);
        }
    }
    for start in 0..whole.len() {
        for end in start..=whole.len() {
            let mut buffer = vec![0xee; end - start];
            let range = START + start as u64..START + end as u64;
            writes.fill(&range, &mut buffer);
            let expected: Vec<u8> = whole[start..end].iter().map(|it| it.unwrap_or(0xee)).collect();
            assert_eq!(buffer, expected, "{range:x?}");
        }
    }
}

#[test]
fn outcomes_say_what_became_of_a_patch() {
    let applied = Outcome::Applied { generation: 3, functions: 2, statics: 1 };
    assert_eq!(applied.to_string(), "generation 3 applied: 2 functions, 1 statics");
    let applied = Outcome::Applied { generation: 3, functions: 2, statics: 0 };
    assert_eq!(applied.to_string(), "generation 3 applied: 2 functions");
    let back = Outcome::RolledBack { generation: 3, to: 2, why: "panicked at a.rs:1:2: x".into() };
    assert_eq!(back.to_string(), "generation 3 rolled back to 2: panicked at a.rs:1:2: x");
    let restart = Outcome::Restart { generation: 3, why: "a layout changed".into() };
    assert_eq!(restart.to_string(), "restart needed at generation 3: a layout changed");
    let declined = Outcome::Declined { generation: 4, why: "no".into() };
    assert_eq!(declined.to_string(), "generation 4 not applied: no");
    let unanswered = Outcome::Unanswered { outcome: Box::new(declined), why: "gone".into() };
    assert_eq!(
        unanswered.to_string(),
        "generation 4 not applied: no; the daemon was not told: gone"
    );
    let disconnected = Outcome::Disconnected { generation: 4, why: "gone".into() };
    assert_eq!(disconnected.to_string(), "no patch after generation 4 is taken: gone");
}

/// A warm patch's table work allocates nothing, however many patches came before it: the
/// log's columns and the lasting ranges grow in place in address space reserved at start,
/// the pool is full and the scratch buffers are at their high-water mark. 9,000 patches of 4
/// cells each run past the 1,024 generations and 16,384 cells the columns had room for when
/// they were `Vec`s, and across a commit step of every column of cells (32,768 rows of 8
/// bytes). Each patch is taken from encoded bytes, as `Live::poll` takes a message from its
/// kept buffer.
#[test]
fn a_warm_patch_allocates_nothing() {
    const SLOTS: u64 = 16;
    const CELLS: u64 = 4;
    const PATCHES: u64 = 9_000;
    let program = Program::with_slots(SLOTS as usize);
    let mut switch = program.switch();
    let message = |generation: u64, base: u64| {
        let cells = (0..CELLS)
            .map(|it| {
                let slot = ((generation * CELLS + it) % SLOTS) as u32;
                let target = if (generation + it) % 2 == 0 { address(one) } else { address(two) };
                (slot, unslid(target))
            })
            .collect();
        Message::Patch(patch(generation, base, cells)).encode()
    };
    for _ in 0..POOL + KEPT + 8 {
        let base = switch.current();
        let cell = ((base % SLOTS) as u32, unslid(address(two)));
        applied(&mut switch, patch(base + 1, base, vec![cell]));
    }
    let first = switch.current();
    let messages: Vec<Vec<u8>> =
        (0..PATCHES).map(|it| message(first + it + 1, first + it)).collect();
    blaze_alloc_count::assert_at_most(0, "a warm patch's table work and its switch", || {
        for bytes in &messages {
            let view = Message::view(bytes).expect("an encoded message reads back");
            let (outcome, answer) = switch.take(&view);
            assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome}");
            assert!(answer.is_some());
        }
    });
    assert_eq!(switch.current(), first + PATCHES);
    let last = first + PATCHES;
    let slots = program.slots();
    for it in 0..CELLS {
        let target = if (last + it) % 2 == 0 { address(one) } else { address(two) };
        assert_eq!(slots[((last * CELLS + it) % SLOTS) as usize], target);
    }
}

/// A patch that would take a column of the log or of the lasting ranges past its limit is
/// refused whole, naming the column and its limit, and the program runs on as it was; a
/// patch that fits in what is left still applies.
#[test]
fn a_full_column_refuses_the_patch() {
    let refused = |outcome: &Outcome, says: &str| match outcome {
        Outcome::Declined { why, .. } => {
            assert!(why.contains(says), "{why:?} doesn't say {says:?}")
        }
        other => panic!("{other}, where it should say {says:?}"),
    };
    let roomy = Limits { generations: 64, cells: 64, code: 64, lasting: 64 };

    let program = Program::new();
    let mut switch = program.switch_limited(&Limits { generations: 3, ..roomy });
    applied(&mut switch, patch(1, 0, vec![(0, unslid(address(two)))]));
    applied(&mut switch, patch(2, 1, vec![(1, unslid(address(one)))]));
    let table = program.cell().load(Ordering::Relaxed);
    let (outcome, answer) =
        switch.handle(Message::Patch(patch(3, 2, vec![(0, unslid(address(one)))])));
    refused(&outcome, "the log's generation numbers is full: it holds 3 rows at most");
    assert!(matches!(answer, Some(Answer::Declined { generation: 3, .. })), "{answer:?}");
    assert_eq!(program.cell().load(Ordering::Relaxed), table);
    assert_eq!((switch.current(), program.calls()), (2, (2, 1)));
    switch.roll_back(1).expect("generation 1 is kept");
    assert_eq!(program.calls(), (2, 2));

    let program = Program::new();
    let mut switch = program.switch_limited(&Limits { cells: 3, ..roomy });
    let both = |to: u64| vec![(0, unslid(to)), (1, unslid(to))];
    applied(&mut switch, patch(1, 0, both(address(two))));
    let (outcome, _) = switch.handle(Message::Patch(patch(2, 1, both(address(one)))));
    refused(&outcome, "the log's slots is full: it holds 3 rows at most");
    assert_eq!((switch.current(), program.calls()), (1, (2, 2)));
    applied(&mut switch, patch(2, 1, vec![(0, unslid(address(one)))]));
    assert_eq!(program.calls(), (1, 2));

    let program = Program::new();
    let mut switch = program.switch_limited(&Limits { code: 2, ..roomy });
    let mut three = patch(1, 0, vec![(0, unslid(address(two)))]);
    three.code = [0, 64, 128].map(|at| program.present(at, 8, "f")).to_vec();
    let (outcome, _) = switch.handle(Message::Patch(three));
    refused(&outcome, "the log's code starts is full: it holds 2 rows at most");
    assert_eq!((switch.current(), program.calls()), (0, (1, 2)));

    let program = Program::new();
    let mut switch = program.switch_limited(&Limits { lasting: 2, ..roomy });
    let mut two_spans = patch(1, 0, vec![(0, unslid(address(two)))]);
    two_spans.code = [0, 64].map(|at| program.present(at, 8, "f")).to_vec();
    applied(&mut switch, two_spans);
    let mut more = patch(2, 1, vec![(1, unslid(address(one)))]);
    more.code = vec![program.present(128, 8, "f")];
    let (outcome, _) = switch.handle(Message::Patch(more));
    refused(&outcome, "the lasting ranges' starts is full: it holds 2 rows at most");
    assert_eq!((switch.current(), program.calls()), (1, (2, 2)));
    applied(&mut switch, patch(2, 1, vec![(1, unslid(address(one)))]));
    assert_eq!(program.calls(), (2, 1));
}

/// Rolling back to a kept generation whose table the pool still holds, and forward again, is
/// the cell's store and nothing allocated.
#[test]
fn a_warm_roll_back_allocates_nothing() {
    let program = Program::with_slots(4);
    let mut switch = program.switch();
    for _ in 0..POOL + 2 {
        let base = switch.current();
        let cell = ((base % 4) as u32, unslid(address(two)));
        applied(&mut switch, patch(base + 1, base, vec![cell]));
    }
    let (newest, before) = (switch.current(), switch.current() - 1);
    let back = Message::RollBack { id: ID, to: before }.encode();
    let ahead = Message::RollBack { id: ID, to: newest }.encode();
    // Once each, so the outcome's reason and the answer are the only allocations left.
    for bytes in [&back, &ahead] {
        let view = Message::view(bytes).expect("an encoded message reads back");
        assert!(matches!(switch.take(&view).0, Outcome::RolledBack { .. }));
    }
    let what = "a warm rollback: its reason, once for the program and once for the linker";
    blaze_alloc_count::assert_at_most(2, what, || {
        let view = Message::view(&back).expect("an encoded message reads back");
        let (outcome, _) = switch.take(&view);
        assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
    });
    assert_eq!(switch.current(), before);
}

/// A region as the test server makes one: the record's magic, `slots`, `armed` zero, and a
/// byte per slot, in `len` bytes rounded up to whole words.
fn region(slots: u32, len: usize) -> Box<[AtomicU64]> {
    let mut bytes = vec![0u8; len.next_multiple_of(8)];
    bytes[..8].copy_from_slice(&MAGIC);
    bytes[8..12].copy_from_slice(&slots.to_le_bytes());
    let word = |it: &[u8]| AtomicU64::new(u64::from_le_bytes(it.try_into().expect("8 bytes")));
    bytes.chunks(8).map(word).collect()
}

fn region_byte(region: &[AtomicU64], at: usize) -> u8 {
    region[at / 8].load(Ordering::Relaxed).to_le_bytes()[at % 8]
}

/// The slots `region` marks.
fn marked(region: &[AtomicU64], slots: usize) -> Vec<usize> {
    let at = REGION_HEADER as usize;
    (0..slots).filter(|it| region_byte(region, at + it) != 0).collect()
}

/// `armed`, bytes 12..16 of the region.
fn armed(region: &[AtomicU64]) -> u32 {
    u32::from_le_bytes([12, 13, 14, 15].map(|it| region_byte(region, it)))
}

/// Arms the program's log into `region`, `len` bytes of it.
fn arm(program: &Program, region: &[AtomicU64], len: usize) -> i32 {
    assert!(len <= region.len() * 8);
    // SAFETY: `region` is `len` bytes or more of `AtomicU64`s that outlive the program's
    // calls, and nothing else writes it.
    unsafe { program.exec_header().arm(region.as_ptr() as *mut u8, len) }
}

/// The prelude's first calls are marked in the image's own map and carried into the region
/// when it is armed; every log stub is put back, so the test's first calls are marked in the
/// region, each slot once, and `armed` says the slot count last.
#[test]
fn arming_carries_the_prelude_and_logs_the_first_calls() {
    const SLOTS: usize = 12;
    let program = Program::linked(SLOTS, true);
    let _switch = program.switch();
    let header = program.header.as_ptr() as u64;
    assert_eq!(program.cell().load(Ordering::Relaxed), header + words::log_word(0));
    assert_eq!(program.call(1), 2);
    assert_eq!(program.call(9), 2);
    assert_eq!(program.call(1), 2);
    assert_eq!(program.word(words::log_word(1)).load(Ordering::Relaxed), address(two));
    let map = program.private_map();
    // SAFETY: the private map is the header's last `16 + SLOTS` bytes, rounded to words.
    let byte = |at: u64| unsafe { image::byte(map + at) }.load(Ordering::Relaxed);
    let prelude =
        || -> Vec<u64> { (0..SLOTS as u64).filter(|it| byte(REGION_HEADER + it) != 0).collect() };
    assert_eq!(prelude(), [1, 9]);

    let len = REGION_HEADER as usize + SLOTS;
    let record = region(SLOTS as u32, len);
    assert_eq!(arm(&program, &record, len), exec::ARMED);
    assert_eq!(armed(&record), SLOTS as u32);
    assert_eq!(marked(&record, SLOTS), [1, 9]);
    assert_eq!(program.word(MAP).load(Ordering::Relaxed), record.as_ptr() as u64);
    for slot in 0..SLOTS as u64 {
        assert_eq!(program.word(words::log_word(slot)).load(Ordering::Relaxed), log_stub(slot));
    }
    assert_eq!(program.cell().load(Ordering::Relaxed), header + words::log_word(0));

    assert_eq!(program.call(4), 1);
    assert_eq!(program.call(4), 1);
    assert_eq!(program.call(1), 2);
    assert_eq!(marked(&record, SLOTS), [1, 4, 9]);
    assert_eq!(prelude(), [1, 9], "the image's own map is left as it was");

    // Armed again, into another region: what the first region had is carried over.
    let again = region(SLOTS as u32, len);
    assert_eq!(arm(&program, &again, len), exec::ARMED);
    assert_eq!(marked(&again, SLOTS), [1, 4, 9]);
}

/// A region too short, null, without the record's magic or of another slot count is refused,
/// as is arming an image linked without a log, and nothing is changed.
#[test]
fn arming_refuses_a_region_it_cannot_use() {
    const SLOTS: usize = 3;
    let program = Program::linked(SLOTS, true);
    let _switch = program.switch();
    let map = program.word(MAP).load(Ordering::Relaxed);
    let len = REGION_HEADER as usize + SLOTS;
    let record = region(SLOTS as u32, len);
    assert_eq!(arm(&program, &record, len - 1), exec::SHORT);
    // SAFETY: a null region, refused before it is read.
    let null = unsafe { program.exec_header().arm(std::ptr::null_mut(), len) };
    assert_eq!(null, exec::SHORT);
    let other = region(SLOTS as u32 + 1, len);
    assert_eq!(arm(&program, &other, len), exec::OTHER_SLOTS);
    let unmarked = region(SLOTS as u32, len);
    unmarked[0].store(u64::from_le_bytes(*b"blzexec0"), Ordering::Relaxed);
    assert_eq!(arm(&program, &unmarked, len), exec::NOT_A_RECORD);
    for refused in [&record, &other, &unmarked] {
        assert_eq!(armed(refused), 0);
    }
    assert_eq!(program.word(MAP).load(Ordering::Relaxed), map);

    let plain = Program::linked(SLOTS, false);
    let _plain_switch = plain.switch();
    assert!(!plain.exec_header().logged());
    assert_eq!(arm(&plain, &record, len), exec::NO_LOG);
    assert_eq!(armed(&record), 0);
}

/// `publish` while armed writes the loader's table and every table of the pool, never the
/// log table: a slot called already keeps the target it copied, and one not called yet
/// still goes through its log stub, is marked and lands on what was published.
#[test]
fn publish_while_armed_leaves_the_log_table_alone() {
    const SLOTS: usize = 4;
    let program = Program::linked(SLOTS, true);
    let mut switch = program.switch();
    let len = REGION_HEADER as usize + SLOTS;
    let record = region(SLOTS as u32, len);
    assert_eq!(arm(&program, &record, len), exec::ARMED);
    assert_eq!(program.call(0), 1);

    let header = program.exec_header();
    header.publish(program.tables, 0, address(two)).expect("published");
    header.publish(program.tables, 2, address(two)).expect("published");
    let refused = header.publish(program.tables, SLOTS as u32, address(two)).err();
    assert!(refused.expect("refused").contains("slot 4 published"));
    assert_eq!(program.loaders(), [address(two), address(two), address(two), address(two)]);
    let log = |slot: u64| program.word(words::log_word(slot)).load(Ordering::Relaxed);
    assert_eq!((log(0), log(1), log(2)), (address(one), log_stub(1), log_stub(2)));
    assert_eq!(program.call(0), 1, "a slot called already keeps what it copied");
    assert_eq!(program.call(2), 2);
    assert_eq!(marked(&record, SLOTS), [0, 2]);

    // The pool's tables have it too: a switch to one of them keeps slot 2 published.
    let cells = vec![(1, unslid(address(one)))];
    applied(&mut switch, patch(FIRST_GENERATION + 1, FIRST_GENERATION, cells));
    let current = program.word(CURRENT).load(Ordering::Relaxed);
    assert_ne!(current, program.table.as_ptr() as u64);
    // SAFETY: the current table is one of the pool's, `SLOTS` `AtomicU64`s the switch keeps.
    let word = |slot: u64| unsafe { image::cell(current + slot * 8) }.load(Ordering::Relaxed);
    assert_eq!((word(0), word(1), word(2)), (address(two), address(one), address(two)));
    header.publish(program.tables, 3, address(one)).expect("published");
    assert_eq!(word(3), address(one));
}

/// A switch while armed stores the header's current table and leaves the cell on the log
/// table: a slot not called yet lands on the new generation's code and is marked, a rollback
/// is the current table's store again. Without a log, the cell moves with the current table.
#[test]
fn a_switch_while_armed_keeps_the_cell_on_the_log_table() {
    const SLOTS: usize = 2;
    let program = Program::linked(SLOTS, true);
    let mut switch = program.switch();
    let log_table = program.header.as_ptr() as u64 + words::log_word(0);
    let len = REGION_HEADER as usize + SLOTS;
    let record = region(SLOTS as u32, len);
    assert_eq!(arm(&program, &record, len), exec::ARMED);
    assert_eq!(program.call(0), 1);

    let swapped = vec![(0, unslid(address(two))), (1, unslid(address(one)))];
    applied(&mut switch, patch(FIRST_GENERATION + 1, FIRST_GENERATION, swapped.clone()));
    assert_eq!(program.cell().load(Ordering::Relaxed), log_table);
    assert_ne!(program.word(CURRENT).load(Ordering::Relaxed), program.table.as_ptr() as u64);
    assert_eq!(program.call(1), 1, "the new generation's code");
    assert_eq!(program.call(0), 1, "copied before the switch");
    assert_eq!(marked(&record, SLOTS), [0, 1]);

    switch.roll_back(FIRST_GENERATION).expect("rolled back");
    assert_eq!(program.cell().load(Ordering::Relaxed), log_table);
    assert_eq!(program.word(CURRENT).load(Ordering::Relaxed), program.table.as_ptr() as u64);
    assert_eq!(armed(&record), SLOTS as u32);

    let plain = Program::linked(SLOTS, false);
    let mut switch = plain.switch();
    applied(&mut switch, patch(FIRST_GENERATION + 1, FIRST_GENERATION, swapped));
    let current = plain.word(CURRENT).load(Ordering::Relaxed);
    assert_eq!(plain.cell().load(Ordering::Relaxed), current);
    assert_eq!(plain.calls(), (2, 1));
}

/// Code in the code memory that is not what the daemon committed, a byte of it changed
/// after the commit, refuses the patch naming the function, and nothing is switched.
#[test]
fn code_that_differs_from_the_commit_is_refused_naming_its_function() {
    let mut program = Program::new();
    let mut switch = program.switch();
    let table = program.cell().load(Ordering::Relaxed);
    let committed = program.commit(0, &[0x90; 16], "life::step");
    program.commit(15, &[0xcc], "a later write");
    let mut patch = patch(1, 0, vec![(0, unslid(address(two)))]);
    patch.code = vec![committed];
    let (outcome, answer) = switch.handle(Message::Patch(patch));
    let Outcome::Declined { generation: 1, why } = &outcome else { panic!("{outcome}") };
    assert!(why.starts_with("`life::step`'s code at "), "{why}");
    assert!(why.contains("is not what the daemon committed"), "{why}");
    assert!(matches!(answer, Some(Answer::Declined { generation: 1, .. })), "{answer:?}");
    assert_eq!(program.cell().load(Ordering::Relaxed), table);
    assert_eq!((switch.current(), program.calls()), (0, (1, 2)));
}
