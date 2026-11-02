//! Switching an image's jump table between generations, apart from how a platform writes
//! its memory ([`Memory`]).
//!
//! A patch is checked whole, viewed in place in its message's bytes, with scratch buffers the
//! switch keeps from patch to patch: once they have grown, taking a patch allocates nothing.
//! Its cells go into one of [`POOL`] tables the switch owns, the one the cell left longest
//! ago, brought to the new generation by storing only the slots that differ: undoing the
//! generations it held down to where they meet the new one's, then redoing those up to it,
//! from the [`Log`] of every generation's cells. One release store then points the cell at
//! it. A rollback is the same towards an older generation, and only the cell's store when a
//! table holds that generation still, or when it is the one the program started at (the
//! loader's table).
//!
//! A patch's code is not in its message: the commit wrote it into the code memory the
//! program runs (blaze-inproc), and the switch only reads it there, hashing each function's
//! bytes against the message's ([`crate::patch::Code`]). One that differs refuses the patch,
//! naming the function. Code holds no address to rebase: every run of the program maps the
//! same copy, whatever its slide.
//!
//! What a patch may write over, or switch to:
//! - Code is only in the patch code region, and never over code of a generation whose
//!   unwind entries were registered or that the cell may have pointed at ([`Lasting`]): a
//!   thread can still be in that code however long ago the program left it, rolled back or
//!   not.
//! - Nothing goes over an unwind entry once registered, nor over the zero length ending a
//!   registered run: the unwinder reads them from then on.
//! - Data goes outside the patch code region, the table's header (its cell, the current
//!   table's word, the execution log's table and map: [`exec`]) and the loader's table; the
//!   platform checks that it is in the image.
//!
//! The switch stores a table first to the header's current word, which the execution log's
//! stubs read, then to the cell, unless the image was linked with the log: there the cell
//! stays on the log table, whose words are either a slot's log stub or the target a first
//! call copied (`super::exec`). Every table the switch makes is one [`Tables`] names, so
//! [`super::exec::publish`] writes a slot into all of them and the loader's.
//! - Data is written in place with atomic stores ([`Writes::store`]): one relaxed store per
//!   naturally aligned piece of at most 8 bytes, each holding its final, rebased value, so no
//!   write races another thread's load of that piece. A thread reading a static wider than
//!   8 bytes while `poll` rewrites it can see part of it old and part new, as with any static
//!   another thread changes; data no code reaches before the cell's store (a new static, a
//!   new unwind entry) is seen whole.

use std::{
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    Outcome,
    column::Column,
    exec::{Header, Tables},
    hook,
};
use crate::patch::{
    ARCH_AARCH64, ARCH_X86_64, Answer, DESCRIPTOR, DESCRIPTOR_VERSION, Descriptor, MessageView,
    PatchView, exec,
};

/// How many generations are kept to roll back to, as Relax keeps.
pub(crate) const KEPT: usize = 64;
/// How many tables of its own a switch writes generations into, reused oldest first. A
/// table is written while the cell points at another, and a thread that loaded the cell
/// before it left this one reads each slot whole, of the generation it was in or the one
/// it is brought to, both code that is written and never written again ([`Lasting`]).
///
/// Right after each switch, the table the cell left longest ago is brought to the new
/// generation ([`Switch::ready`]), or a new one copied while the pool isn't full, so the
/// next patch stores only its own cells before its switch.
pub(crate) const POOL: usize = 4;
const MOST_SLOTS: u64 = 1 << 26;

/// [`Descriptor::arch`] of what this program runs on; none matches elsewhere.
pub(crate) const HOST_ARCH: u32 = if cfg!(target_arch = "x86_64") {
    ARCH_X86_64
} else if cfg!(target_arch = "aarch64") {
    ARCH_AARCH64
} else {
    u32::MAX
};

/// What an image's descriptor says, with its slide. Addresses are as linked unless a method
/// slides them. 80 bytes, made once at start.
#[derive(Debug)]
pub(crate) struct Layout {
    pub(crate) id: [u8; 16],
    /// The generation the image was linked at.
    pub(crate) generation: u64,
    pub(crate) slide: u64,
    /// The cell holding the current table's address.
    pub(crate) table: u64,
    pub(crate) slots: u64,
    pub(crate) code: Range<u64>,
    pub(crate) data: Range<u64>,
}

impl Layout {
    /// The layout of the descriptor found in memory at `at`.
    pub(crate) fn new(descriptor: &Descriptor, at: u64) -> Result<Layout, String> {
        if descriptor.version != DESCRIPTOR_VERSION {
            // alloc: error path
            return Err(format!(
                "{DESCRIPTOR} is of version {}, where this blaze-live reads {DESCRIPTOR_VERSION}",
                descriptor.version
            ));
        }
        if descriptor.arch != HOST_ARCH {
            // alloc: error path
            return Err(format!(
                "{DESCRIPTOR} is for architecture {}, where this program runs on {}",
                descriptor.arch,
                std::env::consts::ARCH
            ));
        }
        let [code_start, code_end] = descriptor.patch_code;
        let [data_start, data_end] = descriptor.patch_data;
        if code_start > code_end || data_start > data_end {
            // alloc: error path
            return Err(format!(
                "{DESCRIPTOR} has a patch region that ends before it starts: code {code_start:#x}..\
                 {code_end:#x}, data {data_start:#x}..{data_end:#x}"
            ));
        }
        if descriptor.slots > MOST_SLOTS {
            // alloc: error path
            return Err(format!("{DESCRIPTOR} has {} slots", descriptor.slots));
        }
        let slide = at.wrapping_sub(descriptor.this);
        if !descriptor.table.wrapping_add(slide).is_multiple_of(8) {
            // alloc: error path
            return Err(format!(
                "{DESCRIPTOR}'s table cell at {:#x} is not aligned",
                descriptor.table
            ));
        }
        Ok(Layout {
            id: descriptor.id,
            generation: descriptor.generation,
            slide,
            table: descriptor.table,
            slots: descriptor.slots,
            code: code_start..code_end,
            data: data_start..data_end,
        })
    }

    pub(crate) fn slid(&self, address: u64) -> u64 {
        address.wrapping_add(self.slide)
    }

    /// Where the patch code region is in memory.
    pub(crate) fn code_in_memory(&self) -> Range<u64> {
        self.slid(self.code.start)..self.slid(self.code.end)
    }

    /// Where the table cell is in memory.
    pub(crate) fn cell(&self) -> u64 {
        self.slid(self.table)
    }
}

/// How the platform's unwinder takes DWARF call frame information.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dwarf {
    /// `__register_frame` of a run of entries ending in a zero length (libgcc, and LLVM's
    /// libunwind on ELF).
    Sections,
    /// `__register_frame` of one FDE (Apple's libunwind).
    Entries,
    /// None: the platform unwinds by other tables.
    Not,
}

/// One data write of a patch: `len` bytes of its message from `from`, to `at` in memory. 24
/// bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Span {
    pub(crate) at: u64,
    pub(super) from: usize,
    pub(super) len: usize,
}

const _: () = assert!(size_of::<Span>() == 24);

impl Span {
    pub(crate) fn end(&self) -> u64 {
        self.at + self.len as u64
    }

    pub(crate) fn range(&self) -> Range<u64> {
        self.at..self.end()
    }

    #[cfg(test)]
    pub(crate) fn new(at: u64, from: usize, len: usize) -> Span {
        Span { at, from, len }
    }
}

/// What a platform writes, every address in memory: spans of the message's bytes sorted by
/// address, none empty and none overlapping, and the addresses within them of 8-byte fields
/// holding an image address, to add the slide to (sorted, each inside one span).
pub(crate) struct Writes<'a> {
    pub(super) message: &'a [u8],
    pub(crate) spans: &'a [Span],
    pub(super) rebase: &'a [u64],
    pub(super) slide: u64,
}

impl<'a> Writes<'a> {
    #[cfg(test)]
    pub(crate) fn new(
        message: &'a [u8],
        spans: &'a [Span],
        rebase: &'a [u64],
        slide: u64,
    ) -> Writes<'a> {
        Writes { message, spans, rebase, slide }
    }

    pub(crate) fn bytes(&self, span: &Span) -> &'a [u8] {
        &self.message[span.from..span.from + span.len]
    }
}

/// What a platform does to the memory of a running image. Every address is in memory
/// (slid). The switch has checked that data is outside the patch code region, the table's
/// header and the loader's table, and that it is over no lasting memory (the module's rules).
/// Whether data is in the image is the platform's to check. The platform is also where the
/// switch reads the image: the header, the loader's table and the code the commit wrote. No
/// platform writes code: the daemon wrote it into the code memory the image's patch code
/// region is mapped from, and made it runnable there (blaze-inproc's `CodeMemory`).
pub(crate) trait Memory {
    const DWARF: Dwarf;
    /// Whether the unwinder takes Windows `RUNTIME_FUNCTION`s.
    const SEH: bool;
    /// What the table's word at `cell` (8-aligned, in the header `Layout::new` names) holds,
    /// by one acquire load.
    fn cell(&self, cell: u64) -> u64;
    /// Checks that `range` is in the image, as the platform checks data.
    fn in_image(&self, range: &Range<u64>) -> Result<(), String>;
    /// [`Memory::in_image`] of each of `spans`, sorted and none overlapping: a platform may
    /// walk its mappings once, forward, for all of them.
    fn in_image_all(&self, spans: &[Span]) -> Result<(), String> {
        spans.iter().try_for_each(|it| self.in_image(&it.range()))
    }
    /// The image's bytes at `range`, which the platform checks are in the image and
    /// readable: a patch's code, which the switch hashes and reads unwind entries from.
    fn bytes(&self, range: &Range<u64>) -> Result<&'static [u8], String>;
    /// The loader's table, `slots` addresses at `at`, which [`Switch::new`] checked is
    /// 8-aligned and in the image. Only [`super::exec::publish`] writes it, atomically: the
    /// switch stores only to the header and its own tables, and refuses data over it.
    fn loaders(&self, at: u64, slots: u64) -> &[AtomicU64];
    /// Writes data anywhere in the image with [`Writes::store`], making read-only pages
    /// writable for the write and back.
    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String>;
    /// Registers with the unwinder what [`Memory::DWARF`] says it takes, at each address.
    fn frames(&mut self, starts: &[u64]) -> Result<(), String>;
    /// Adds `RUNTIME_FUNCTION`s (begin, end, unwind information).
    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String>;
    /// Stores `table` in the header's word `cell` (the cell or the current table's word)
    /// with one release store. An error means the word may or may not hold `table`.
    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String>;
}

/// The most rows the log and [`Lasting`] hold: their columns reserve address space for these
/// when the switch is made and commit it as they fill, so no patch moves a row or calls the
/// allocator, and a patch past one is refused naming the column. The arithmetic, and the
/// address space it takes, are in docs/v2/plan/lanes/link/speed-of-light.md ("The live
/// runtime").
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Generations, the first included.
    pub(crate) generations: usize,
    /// Cells set, over every generation.
    pub(crate) cells: usize,
    /// Writes of code, over every generation.
    pub(crate) code: usize,
    pub(crate) lasting: usize,
}

impl Limits {
    /// A long session's: about 1M generations, 16M cells, 4M writes of code and 1M lasting
    /// ranges, in about 435 MiB of address space.
    pub(crate) const LONG: Limits =
        Limits { generations: 1 << 20, cells: 1 << 24, code: 1 << 22, lasting: 1 << 20 };
}

// Indexes into the log are `u32`s.
const _: () = assert!(
    Limits::LONG.generations < u32::MAX as usize
        && Limits::LONG.cells <= u32::MAX as usize
        && Limits::LONG.code <= u32::MAX as usize
);

/// Every generation the program ran, by index (0 the one it started at), in columns: 25 bytes
/// each, 20 per cell its patch set and 16 per write of code. Lookups by index are direct.
struct Log {
    number: Column<u64>,
    /// The generation it was applied on; its own index for the first.
    parent: Column<u32>,
    depth: Column<u32>,
    /// Whether it is kept to roll back to: 1 if it is.
    kept: Column<u8>,
    /// Generation `g`'s cells are `cells[g]..cells[g + 1]` of `slot`, `new` and `old`.
    cells: Column<u32>,
    slot: Column<u32>,
    /// A slot's target in memory in the generation, and in its parent.
    new: Column<u64>,
    old: Column<u64>,
    /// Generation `g`'s code is `code_at[g]..code_at[g + 1]` of `code_start` and `code_end`,
    /// in memory.
    code_at: Column<u32>,
    code_start: Column<u64>,
    code_end: Column<u64>,
}

impl Log {
    fn new(number: u64, limits: &Limits) -> Result<Log, String> {
        let (generations, cells, code) = (limits.generations, limits.cells, limits.code);
        let mut log = Log {
            number: Column::new("the log's generation numbers", generations)?,
            parent: Column::new("the log's parents", generations)?,
            depth: Column::new("the log's depths", generations)?,
            kept: Column::new("the log's kept marks", generations)?,
            cells: Column::new("the log's cell runs", generations + 1)?,
            slot: Column::new("the log's slots", cells)?,
            new: Column::new("the log's new targets", cells)?,
            old: Column::new("the log's old targets", cells)?,
            code_at: Column::new("the log's code runs", generations + 1)?,
            code_start: Column::new("the log's code starts", code)?,
            code_end: Column::new("the log's code ends", code)?,
        };
        log.number.push(number)?;
        log.parent.push(0)?;
        log.depth.push(0)?;
        log.kept.push(1)?;
        log.cells.extend_from_slice(&[0, 0])?;
        log.code_at.extend_from_slice(&[0, 0])?;
        Ok(log)
    }

    /// Makes room for one more generation with `cells` cells and `code` writes of code, or
    /// says which column is full.
    fn room(&mut self, cells: usize, code: usize) -> Result<(), String> {
        self.number.room(1)?;
        self.parent.room(1)?;
        self.depth.room(1)?;
        self.kept.room(1)?;
        self.cells.room(1)?;
        self.code_at.room(1)?;
        self.slot.room(cells)?;
        self.new.room(cells)?;
        self.old.room(cells)?;
        self.code_start.room(code)?;
        self.code_end.room(code)
    }

    /// Readies the pages one more generation with `cells` cells and `code` writes of code
    /// goes on ([`Column::ahead`]).
    fn ahead(&mut self, cells: usize, code: usize) {
        self.number.ahead(1);
        self.parent.ahead(1);
        self.depth.ahead(1);
        self.kept.ahead(1);
        self.cells.ahead(1);
        self.code_at.ahead(1);
        self.slot.ahead(cells);
        self.new.ahead(cells);
        self.old.ahead(cells);
        self.code_start.ahead(code);
        self.code_end.ahead(code);
    }

    fn cells(&self, generation: u32) -> Range<usize> {
        let g = generation as usize;
        self.cells[g] as usize..self.cells[g + 1] as usize
    }

    /// Whether generation `generation` wrote code at one of `pcs`.
    fn has_code(&self, generation: u32, pcs: &[u64]) -> bool {
        let g = generation as usize;
        let writes = self.code_at[g] as usize..self.code_at[g + 1] as usize;
        let (starts, ends) = (&self.code_start[writes.start..writes.end], &self.code_end[writes]);
        starts.iter().zip(ends).any(|(start, end)| pcs.iter().any(|pc| start <= pc && pc < end))
    }

    /// Ends a generation whose cells and code were pushed after the last one's; its index.
    fn finish(&mut self, number: u64, parent: u32) -> Result<u32, String> {
        let generation = self.number.len() as u32;
        let depth = self.depth[parent as usize] + 1;
        self.number.push(number)?;
        self.parent.push(parent)?;
        self.depth.push(depth)?;
        self.kept.push(0)?;
        self.cells.push(self.slot.len() as u32)?;
        self.code_at.push(self.code_start.len() as u32)?;
        Ok(generation)
    }
}

/// What a lasting range holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Holds {
    Code = 0,
    Unwind = 1,
}

impl Holds {
    fn of(byte: u8) -> Holds {
        if byte == Holds::Code as u8 { Holds::Code } else { Holds::Unwind }
    }
}

/// Memory that never changes again, in memory, sorted and none overlapping: the code of
/// every generation whose unwind entries were registered or that the cell may have pointed
/// at (a thread can be in it however long ago the program left it), and every unwind entry
/// registered (the unwinder reads it from then on, and not every platform can take one
/// back). Each range is of one generation, by number. In columns, 25 bytes a range: a check
/// reads only `end`, by binary search.
struct Lasting {
    start: Column<u64>,
    end: Column<u64>,
    number: Column<u64>,
    holds: Column<u8>,
}

impl Lasting {
    fn new(limit: usize) -> Result<Lasting, String> {
        Ok(Lasting {
            start: Column::new("the lasting ranges' starts", limit)?,
            end: Column::new("the lasting ranges' ends", limit)?,
            number: Column::new("the lasting ranges' generations", limit)?,
            holds: Column::new("the lasting ranges' kinds", limit)?,
        })
    }

    /// Makes room for `more` ranges, which is as many as `more` calls of [`Lasting::add`]
    /// can need, or says which column is full.
    fn room(&mut self, more: usize) -> Result<(), String> {
        self.start.room(more)?;
        self.end.room(more)?;
        self.number.room(more)?;
        self.holds.room(more)
    }

    /// Readies the pages `more` ranges go on ([`Column::ahead`]).
    fn ahead(&mut self, more: usize) {
        self.start.ahead(more);
        self.end.ahead(more);
        self.number.ahead(more);
        self.holds.ahead(more);
    }

    /// The first lasting range `range` overlaps: a binary search, after one load of the last
    /// end. The linker places each patch after every earlier one's code, so most writes start
    /// past every lasting range, and the last end settles them without the search's cold
    /// misses through a column of up to 8 MiB; any other write takes the search.
    fn hit(&self, range: &Range<u64>) -> Option<(Range<u64>, u64, Holds)> {
        if self.end.last().is_none_or(|end| *end <= range.start) {
            return None;
        }
        let at = self.end.partition_point(|end| *end <= range.start);
        let start = *self.start.get(at).filter(|start| **start < range.end)?;
        Some((start..self.end[at], self.number[at], Holds::of(self.holds[at])))
    }

    /// Adds `range` of generation `number`, merged with what it overlaps: only ranges of the
    /// same generation can, since a patch over a lasting range is refused. In place, after
    /// [`Lasting::room`].
    fn add(&mut self, range: Range<u64>, number: u64, holds: Holds) -> Result<(), String> {
        if range.is_empty() {
            return Ok(());
        }
        let at = self.end.partition_point(|end| *end <= range.start);
        let (mut merged, mut holds, mut end) = (range, holds, at);
        let (starts, ends, kinds) = (&self.start[..], &self.end[..], &self.holds[..]);
        while end < starts.len() && starts[end] < merged.end {
            merged = merged.start.min(starts[end])..merged.end.max(ends[end]);
            if Holds::of(kinds[end]) == Holds::Code {
                holds = Holds::Code;
            }
            end += 1;
        }
        self.start.splice(at..end, merged.start)?;
        self.end.splice(at..end, merged.end)?;
        self.number.splice(at..end, number)?;
        self.holds.splice(at..end, holds as u8)
    }
}

/// Why a write of `kind` over `hit` is refused, in linked addresses.
fn over(kind: &str, hit: (Range<u64>, u64, Holds), slide: u64) -> String {
    let (range, number, holds) = hit;
    let (start, end) = (range.start.wrapping_sub(slide), range.end.wrapping_sub(slide));
    match holds {
        // alloc: error path
        Holds::Code => format!(
            "{kind} over generation {number}'s code at {start:#x}..{end:#x}, which the program \
             can be running"
        ),
        // alloc: error path
        Holds::Unwind => format!(
            "{kind} over generation {number}'s unwind entries at {start:#x}..{end:#x}, which \
             the unwinder reads"
        ),
    }
}

/// A table of the switch's own: the generation whose slots it holds, and the count of
/// publishes when the cell last pointed at it.
struct Pooled {
    table: Box<[AtomicU64]>,
    holds: u32,
    at: u64,
}

/// Buffers a patch is checked and written with, kept from one to the next.
#[derive(Default)]
struct Scratch {
    /// The patch's code, in memory.
    code: Vec<Range<u64>>,
    data: Vec<Span>,
    rebase: Vec<u64>,
    slots: Vec<u32>,
    fdes: Vec<(u64, u64)>,
    starts: Vec<u64>,
    /// The zero length ending each run of entries registered, for `Dwarf::Sections`.
    ends: Vec<u64>,
    functions: Vec<(u64, u64, u64)>,
    /// Each RUNTIME_FUNCTION's unwind information.
    unwinds: Vec<Range<u64>>,
    path: Vec<u32>,
}

/// The generations of a running image and which one its table cell points at.
pub(crate) struct Switch<M: Memory> {
    pub(crate) memory: M,
    layout: Layout,
    /// The generation the cell points at.
    current: u32,
    log: Log,
    /// The generations kept, the oldest applied first.
    kept: Vec<u32>,
    /// The loader's table, which holds generation 0.
    image: u64,
    /// The table's header, in memory.
    header: Header,
    /// Where [`super::exec::publish`] finds the pool's tables.
    tables: &'static Tables,
    pool: Vec<Pooled>,
    /// The table the cell points at: the image's, or one of the pool.
    on: Option<usize>,
    /// The table of the pool [`Switch::ready`] brought to the current generation for the
    /// next patch, which the cell doesn't point at.
    next: Option<usize>,
    published: u64,
    lasting: Lasting,
    scratch: Scratch,
    /// How many slots the last switch stored before its release store.
    #[cfg(test)]
    stored: usize,
}

impl<M: Memory> Switch<M> {
    /// The process's one switch, its tables named in [`super::exec::TABLES`].
    pub(crate) fn new(memory: M, layout: Layout) -> Result<Switch<M>, String> {
        Switch::with_limits(memory, layout, &Limits::LONG, &super::exec::TABLES)
    }

    /// A switch whose log and lasting ranges hold at most `limits`, its tables named in
    /// `tables`.
    pub(crate) fn with_limits(
        memory: M,
        layout: Layout,
        limits: &Limits,
        tables: &'static Tables,
    ) -> Result<Switch<M>, String> {
        let at = layout.cell();
        let in_image = |range: &Range<u64>| memory.in_image(range);
        // SAFETY: `at` is 8-aligned (`Layout::new` checked) and the platform's `in_image`
        // refuses what is not the image, whose header and loader's table are the linker's
        // `Class::Table`, writable data for the life of the program.
        #[allow(unsafe_code, reason = "the execution log's header, in place in the image")]
        let header = unsafe { Header::checked(at, layout.slots, in_image) }?;
        let table = header.loader();
        let current = memory.cell(at + exec::CURRENT);
        let cell = memory.cell(at + exec::CELL);
        let expected = if header.logged() { at + exec::log_word(0) } else { table };
        if current != table || cell != expected {
            // alloc: error path, at start
            return Err(format!(
                "the jump table at {at:#x} starts on {cell:#x} with {current:#x} current, where \
                 its loader's table is at {table:#x}"
            ));
        }
        let log = Log::new(layout.generation, limits)?;
        let lasting = Lasting::new(limits.lasting)?;
        let mut kept = Vec::with_capacity(KEPT + 1);
        kept.push(0);
        let mut switch = Switch {
            memory,
            layout,
            current: 0,
            log,
            kept,
            image: table,
            header,
            tables,
            pool: Vec::with_capacity(POOL),
            on: None,
            next: None,
            published: 0,
            lasting,
            scratch: Scratch::default(),
            #[cfg(test)]
            stored: 0,
        };
        let mut path = std::mem::take(&mut switch.scratch.path);
        switch.ready(&mut path);
        switch.scratch.path = path;
        Ok(switch)
    }

    pub(crate) fn current(&self) -> u64 {
        self.log.number[self.current as usize]
    }

    /// Takes one message: what became of it, and what to answer the linker, if anything.
    pub(crate) fn take(&mut self, message: &MessageView<'_>) -> (Outcome, Option<Answer>) {
        match *message {
            MessageView::Patch(patch) => {
                let generation = patch.generation;
                let mut scratch = std::mem::take(&mut self.scratch);
                let applied = self.apply(patch, &mut scratch);
                self.scratch = scratch;
                match applied {
                    Ok(()) => (
                        Outcome::Applied {
                            generation,
                            functions: patch.functions,
                            statics: patch.statics,
                        },
                        Some(Answer::Applied { generation }),
                    ),
                    Err(why) => declined(generation, why),
                }
            }
            MessageView::RollBack { id, to } => {
                if let Err(why) = self.ours(&id) {
                    return declined(to, why);
                }
                let from = self.current();
                match self.roll_back(to) {
                    Ok(()) => {
                        // alloc: once per rollback, the reason the program and linker get
                        let why = "the linker took it back".to_owned();
                        (
                            // clone: the same, one copy each (see above)
                            Outcome::RolledBack { generation: from, to, why: why.clone() },
                            Some(Answer::RolledBack { generation: from, to, why }),
                        )
                    }
                    Err(why) => declined(to, why),
                }
            }
            MessageView::Restart { id, generation, why } => match self.ours(&id) {
                // alloc: once per restart, the reason handed to the program
                Ok(()) => (Outcome::Restart { generation, why: why.to_owned() }, None),
                Err(other) => declined(generation, other),
            },
        }
    }

    /// [`Switch::take`] of an owned message, as the linker writes it.
    #[cfg(test)]
    pub(crate) fn handle(&mut self, message: crate::patch::Message) -> (Outcome, Option<Answer>) {
        let bytes = message.encode();
        let view = crate::patch::Message::view(&bytes).expect("an encoded message reads back");
        self.take(&view)
    }

    fn ours(&self, id: &[u8; 16]) -> Result<(), String> {
        if *id == self.layout.id {
            Ok(())
        } else {
            // alloc: error path
            Err(format!("for image {}, where this program runs {}", hex(id), hex(&self.layout.id)))
        }
    }

    /// The kept generation numbered `number`.
    fn find(&self, number: u64) -> Option<u32> {
        self.kept.iter().copied().find(|&it| self.log.number[it as usize] == number)
    }

    /// The current generation and the kept ones it was applied on, down to the first or the
    /// first no longer kept.
    fn lineage(&self) -> impl Iterator<Item = u32> + '_ {
        let mut at = Some(self.current);
        std::iter::from_fn(move || {
            let generation = at.filter(|it| self.log.kept[*it as usize] != 0)?;
            let parent = self.log.parent[generation as usize];
            at = (parent != generation).then_some(parent);
            Some(generation)
        })
    }

    /// Checks the whole patch, then writes it and switches the table to it. A patch that
    /// fails a check leaves the program as it was.
    fn apply(&mut self, patch: PatchView<'_>, s: &mut Scratch) -> Result<(), String> {
        self.ours(&patch.id)?;
        let current = self.current();
        if patch.base != current {
            // alloc: error path
            return Err(format!(
                "a patch on generation {}, where this program is at {current}",
                patch.base
            ));
        }
        if patch.generation <= patch.base {
            // alloc: error path
            return Err(format!(
                "generation {} on base {}: a generation counts up from its base",
                patch.generation, patch.base
            ));
        }
        if self.find(patch.generation).is_some() {
            // alloc: error path
            return Err(format!("generation {} was applied already", patch.generation));
        }
        let layout = &self.layout;
        let slide = layout.slide;
        let message = patch.message;
        s.code.clear();
        for code in patch.code {
            let range = span(code.at, code.len)?;
            if !inside(&range, &layout.code) {
                // alloc: error path
                return Err(format!(
                    "`{}`'s code at {:#x}..{:#x}, outside the patch code region {:#x}..{:#x}",
                    code.name, range.start, range.end, layout.code.start, layout.code.end
                ));
            }
            if range.is_empty() {
                continue;
            }
            let slid = span(range.start.wrapping_add(slide), code.len)?;
            if let Some(hit) = self.lasting.hit(&slid) {
                return Err(over("code", hit, slide));
            }
            let bytes = self.memory.bytes(&slid).map_err(|why| {
                // alloc: error path
                format!("`{}`'s code at {:#x}..{:#x}: {why}", code.name, range.start, range.end)
            })?;
            if crate::patch::hash(bytes) != code.hash {
                // alloc: error path
                return Err(format!(
                    "`{}`'s code at {:#x}..{:#x} is not what the daemon committed: the code memory \
                     this program runs holds other bytes there",
                    code.name, range.start, range.end
                ));
            }
            s.code.push(slid);
        }
        let header = exec::len(layout.slots, self.header.logged());
        let cell = layout.table..layout.table.saturating_add(header);
        let loaders = self.image..self.image.saturating_add(layout.slots.saturating_mul(8));
        s.data.clear();
        for (at, bytes) in patch.data {
            let range = span(at, bytes.len() as u64)?;
            if overlap(&range, &layout.code) {
                // alloc: error path
                return Err(format!(
                    "data at {:#x}..{:#x}, in the patch code region",
                    range.start, range.end
                ));
            }
            if overlap(&range, &cell) {
                // alloc: error path
                return Err(format!("data over the jump table's cell at {:#x}", cell.start));
            }
            if !bytes.is_empty() {
                let placed = place(message, at, bytes, slide)?;
                if overlap(&placed.range(), &loaders) {
                    // alloc: error path
                    return Err(format!(
                        "data over the loader's jump table at {:#x}",
                        loaders.start.wrapping_sub(slide)
                    ));
                }
                if let Some(hit) = self.lasting.hit(&placed.range()) {
                    return Err(over("data", hit, slide));
                }
                s.data.push(placed);
            }
        }
        // Code is inside the code region and data outside it, so only code can overlap code
        // and data data.
        s.code.sort_unstable_by_key(|it| it.start);
        if let Some(pair) = s.code.windows(2).find(|it| it[1].start < it[0].end) {
            // alloc: error path
            return Err(format!("two functions' code at {:#x}", pair[1].start.wrapping_sub(slide)));
        }
        s.data.sort_unstable_by_key(|it| it.at);
        if let Some(pair) = s.data.windows(2).find(|it| it[1].at < it[0].end()) {
            // alloc: error path
            return Err(format!("two writes at {:#x}", pair[1].at.wrapping_sub(slide)));
        }
        // The platform's `data` refuses the same while it writes, a span at a time: checked
        // here, a bad span refuses the patch before any is written.
        self.memory.in_image_all(&s.data)?;
        s.slots.clear();
        for (slot, _) in patch.cells() {
            if u64::from(slot) >= layout.slots {
                // alloc: error path
                return Err(format!("slot {slot}, where the table has {}", layout.slots));
            }
            s.slots.push(slot);
        }
        s.slots.sort_unstable();
        if let Some(pair) = s.slots.windows(2).find(|it| it[0] == it[1]) {
            // alloc: error path
            return Err(format!("slot {} set twice", pair[0]));
        }
        s.rebase.clear();
        s.rebase.extend(patch.rebase().map(|it| it.wrapping_add(slide)));
        s.rebase.sort_unstable();
        if let Some(pair) = s.rebase.windows(2).find(|it| it[0] == it[1]) {
            // alloc: error path
            return Err(format!("the address at {:#x} rebased twice", pair[0].wrapping_sub(slide)));
        }
        for &at in &s.rebase {
            if in_code(&s.code, at, 8).is_some() {
                // alloc: error path
                return Err(format!(
                    "an address to rebase at {:#x}, in code: every run of the program maps one \
                     copy of its code, whatever its slide",
                    at.wrapping_sub(slide)
                ));
            }
            if find(message, &s.data, at, 8).is_none() {
                // alloc: error path
                return Err(format!(
                    "an address to rebase at {:#x}, in none of the patch's writes",
                    at.wrapping_sub(slide)
                ));
            }
        }
        let memory = &self.memory;
        let (code, data) = (&s.code, &s.data);
        let written = |at: u64, len: u64| written_at(memory, message, code, data, at, len);
        let (fdes, starts, ends) = (&mut s.fdes, &mut s.starts, &mut s.ends);
        frames::<M>(patch, &written, slide, fdes, starts, ends)?;
        if patch.windows().len() != 0 && !M::SEH {
            // alloc: error path
            return Err("RUNTIME_FUNCTIONs, which this platform's unwinder doesn't take".to_owned());
        }
        s.functions.clear();
        s.unwinds.clear();
        for (begin, end, unwind) in patch.windows() {
            let (slid_begin, slid_unwind) = (begin.wrapping_add(slide), unwind.wrapping_add(slide));
            if begin >= end || in_code(code, slid_begin, end - begin).is_none() {
                // alloc: error path
                return Err(format!("a RUNTIME_FUNCTION of {begin:#x}..{end:#x}, not in the code"));
            }
            let written = |len: u64| written(slid_unwind, len);
            let Some(header) = written(4) else {
                // alloc: error path
                return Err(format!("unwind information at {unwind:#x}, in none of the writes"));
            };
            let len = unwind_len(header);
            if written(len).is_none() {
                // alloc: error path
                return Err(format!(
                    "unwind information at {unwind:#x} runs past the patch's writes ({len} bytes)"
                ));
            }
            s.functions.push((slid_begin, end.wrapping_add(slide), slid_unwind));
            s.unwinds.push(slid_unwind..slid_unwind + len);
        }
        // Room in every column is made before anything is written (each added lasting range
        // takes at most one row), so a full one refuses the patch whole and nothing after the
        // writes can run out.
        let cells = patch.cells();
        let set = cells.len();
        let lasting = s.code.len() + s.fdes.len() + s.ends.len() + s.unwinds.len();
        self.log.room(set, s.code.len())?;
        self.lasting.room(lasting)?;

        self.memory.data(&Writes { message, spans: &s.data, rebase: &s.rebase, slide })?;
        // From here the unwinder or a thread may read what this generation wrote, whatever
        // becomes of the rest.
        let number = patch.generation;
        for range in &s.code {
            // clone: a Range<u64> of two words; Range isn't Copy
            self.lasting.add(range.clone(), number, Holds::Code)?;
        }
        for &(at, len) in &s.fdes {
            self.lasting.add(at..at + len, number, Holds::Unwind)?;
        }
        for &at in &s.ends {
            self.lasting.add(at..at + 4, number, Holds::Unwind)?;
        }
        for unwind in &s.unwinds {
            self.lasting.add(unwind.start..unwind.end, number, Holds::Unwind)?;
        }
        self.memory.frames(&s.starts)?;
        self.memory.functions(&s.functions)?;

        for (slot, target) in cells {
            let old = self.value(slot);
            self.log.slot.push(slot)?;
            self.log.new.push(target.wrapping_add(slide))?;
            self.log.old.push(old)?;
        }
        for range in &s.code {
            self.log.code_start.push(range.start)?;
            self.log.code_end.push(range.end)?;
        }
        let generation = self.log.finish(patch.generation, self.current)?;
        let at = self.standby(generation, &mut s.path);
        self.publish(Some(at))?;
        self.log.kept[generation as usize] = 1;
        self.kept.push(generation);
        self.current = generation;
        while self.kept.len() > KEPT {
            let Some(oldest) = self.kept.iter().position(|it| *it != self.current) else {
                break;
            };
            let evicted = self.kept.remove(oldest);
            self.log.kept[evicted as usize] = 0;
        }
        self.ready(&mut s.path);
        // After the switch, the pages a next patch of this size writes in the log, so its
        // pushes find them committed and resident.
        self.log.ahead(set, s.code.len());
        self.lasting.ahead(lasting);
        Ok(())
    }

    /// Readies a table for the next patch, after the switch: a new one, a copy of the
    /// cell's, while the pool isn't full, else the one the cell left longest ago brought to
    /// the current generation. The next patch then stores only its own cells into it.
    ///
    /// A new table is named in [`Tables`] while still zero, then each slot copied by a
    /// compare-and-swap from zero: a slot [`super::exec::publish`] wrote meanwhile keeps
    /// what it wrote, and one it wrote before is read here (both sides are sequentially
    /// consistent).
    fn ready(&mut self, path: &mut Vec<u32>) {
        if self.pool.len() < POOL {
            let slots = self.layout.slots as usize;
            // alloc: one table per pool entry, at most `POOL`, kept for the program's life
            let table: Box<[AtomicU64]> = (0..slots).map(|_| AtomicU64::new(0)).collect();
            self.tables.register(self.pool.len(), table.as_ptr() as u64);
            let from = match self.on {
                None => self.image(),
                Some(on) => &self.pool[on].table[..],
            };
            for (to, from) in table.iter().zip(from) {
                let value = from.load(Ordering::SeqCst);
                _ = to.compare_exchange(0, value, Ordering::Relaxed, Ordering::Relaxed);
            }
            self.pool.push(Pooled { table, holds: self.current, at: self.published });
            self.next = Some(self.pool.len() - 1);
            return;
        }
        let Some(at) = self.spare() else { return };
        self.retarget(at, self.current, path);
        self.next = Some(at);
    }

    /// The table of the pool the cell left longest ago, other than the one it points at.
    fn spare(&self) -> Option<usize> {
        (0..self.pool.len()).filter(|it| Some(*it) != self.on).min_by_key(|it| self.pool[*it].at)
    }

    /// The loader's table, which the cell pointed at when the program started.
    fn image(&self) -> &[AtomicU64] {
        self.memory.loaders(self.image, self.layout.slots)
    }

    /// The target of `slot` in the current table.
    fn value(&self, slot: u32) -> u64 {
        match self.on {
            None => self.image()[slot as usize].load(Ordering::Relaxed),
            Some(at) => self.pool[at].table[slot as usize].load(Ordering::Relaxed),
        }
    }

    /// A table of the pool brought to `target`: one holding it already, else the one
    /// [`Switch::ready`] brought to the current generation, else the one the cell left
    /// longest ago (`ready` made one at the start and after every patch, and the cell points
    /// at one table at most).
    fn standby(&mut self, target: u32, path: &mut Vec<u32>) -> usize {
        #[cfg(test)]
        {
            self.stored = 0;
        }
        if let Some(at) = self.pool.iter().position(|it| it.holds == target) {
            return at;
        }
        let ready = (self.next.take())
            .filter(|it| Some(*it) != self.on && self.pool[*it].holds == self.current);
        let at = ready
            .or_else(|| self.spare())
            .expect("a table the cell doesn't point at, made by `ready`");
        let _stored = self.retarget(at, target, path);
        #[cfg(test)]
        {
            self.stored = _stored;
        }
        at
    }

    /// Brings the pool's table `at` from the generation it holds to `to`: the generations
    /// from the one it holds down to where the two meet are undone, the newest first, and
    /// those from there up to `to` redone, the oldest first. Each slot is one relaxed store,
    /// which the release store of the cell publishes. How many it stored.
    fn retarget(&mut self, at: usize, to: u32, path: &mut Vec<u32>) -> usize {
        let log = &self.log;
        let (slot, old, new) = (&log.slot[..], &log.old[..], &log.new[..]);
        let (depths, parents) = (&log.depth[..], &log.parent[..]);
        let table = &self.pool[at].table;
        let mut stored = 0;
        let mut undo = |generation: u32| {
            let cells = log.cells(generation);
            stored += cells.len();
            for cell in cells {
                table[slot[cell] as usize].store(old[cell], Ordering::Relaxed);
            }
        };
        let depth = |generation: u32| depths[generation as usize];
        let parent = |generation: u32| parents[generation as usize];
        let (mut from, mut onto) = (self.pool[at].holds, to);
        path.clear();
        while depth(from) > depth(onto) {
            undo(from);
            from = parent(from);
        }
        while depth(onto) > depth(from) {
            path.push(onto);
            onto = parent(onto);
        }
        while from != onto {
            undo(from);
            from = parent(from);
            path.push(onto);
            onto = parent(onto);
        }
        for &generation in path.iter().rev() {
            let cells = log.cells(generation);
            stored += cells.len();
            for cell in cells {
                table[slot[cell] as usize].store(new[cell], Ordering::Relaxed);
            }
        }
        self.pool[at].holds = to;
        stored
    }

    /// Makes the image's table (`None`) or the pool's table `on` current: the header's current
    /// word, then the cell, which stays on the execution log's table in an image linked with
    /// one.
    fn publish(&mut self, on: Option<usize>) -> Result<(), String> {
        let table = match on {
            None => self.image,
            Some(at) => self.pool[at].table.as_ptr() as u64,
        };
        let at = self.layout.cell();
        let stored = self.memory.publish(at + exec::CURRENT, table);
        let stored = match stored {
            Ok(()) if !self.header.logged() => self.memory.publish(at + exec::CELL, table),
            stored => stored,
        };
        self.published += 1;
        if let Some(at) = on {
            // After a failed store the cell may point at it: as the one used last, it is the
            // last of the pool to be written again.
            self.pool[at].at = self.published;
        }
        stored?;
        self.on = on;
        Ok(())
    }

    /// Points the cell at the table of `to`, a kept generation.
    pub(crate) fn roll_back(&mut self, to: u64) -> Result<(), String> {
        let Some(target) = self.find(to) else {
            let kept: Vec<String> =
                // alloc: error path
                self.kept.iter().map(|it| self.log.number[*it as usize].to_string()).collect();
            // alloc: error path
            return Err(format!("generation {to} is not kept; kept are {}", kept.join(", ")));
        };
        if target == self.current {
            return Ok(());
        }
        let on = if target == 0 {
            None
        } else {
            let mut path = std::mem::take(&mut self.scratch.path);
            let at = self.standby(target, &mut path);
            self.scratch.path = path;
            Some(at)
        };
        self.publish(on)?;
        self.current = target;
        Ok(())
    }

    /// The newest generation of the current lineage with code at one of `pcs` (in memory),
    /// and its base, when that is kept to go back to.
    pub(crate) fn tie(&self, pcs: &[u64]) -> Option<(u64, u64)> {
        let log = &self.log;
        self.lineage()
            .filter(|it| {
                let parent = log.parent[*it as usize];
                parent != *it && log.kept[parent as usize] != 0
            })
            .filter(|it| log.has_code(*it, pcs))
            .map(|it| (log.number[it as usize], log.number[log.parent[it as usize] as usize]))
            .max()
    }

    /// Runs `f`; a panic in the code of a generation rolls that generation back to its base
    /// and is told to `answer` and returned, any other is resumed as it was.
    pub(crate) fn guard<R>(
        &mut self,
        f: impl FnOnce() -> R,
        // dyn: called only after a panic in patched code, once per rollback
        answer: &mut dyn FnMut(&Answer),
    ) -> Result<R, Outcome> {
        let (payload, panic) = match hook::catch(f) {
            Ok(result) => return Ok(result),
            Err(caught) => caught,
        };
        let tied = panic.as_ref().and_then(|it| self.tie(&it.pcs));
        let (Some(panic), Some((generation, to))) = (panic, tied) else {
            std::panic::resume_unwind(payload);
        };
        if let Err(why) = self.roll_back(to) {
            // alloc: error path, once per failed rollback after a panic
            let why = format!("{}; rolling back to generation {to}: {why}", panic.why);
            answer(&Answer::Declined { generation, why });
            std::panic::resume_unwind(payload);
        }
        drop(payload);
        // clone: once per rollback after a panic, the reason for the linker and the program
        answer(&Answer::RolledBack { generation, to, why: panic.why.clone() });
        Err(Outcome::RolledBack { generation, to, why: panic.why })
    }

    #[cfg(test)]
    pub(crate) fn kept(&self) -> Vec<u64> {
        self.kept.iter().map(|it| self.log.number[*it as usize]).collect()
    }

    /// How many tables of its own the switch has.
    #[cfg(test)]
    pub(crate) fn pooled(&self) -> usize {
        self.pool.len()
    }

    /// How many slots the last switch stored before its release store.
    #[cfg(test)]
    pub(crate) fn stored(&self) -> usize {
        self.stored
    }

    /// The address of the table of the generation numbered `number`, if one holds it.
    #[cfg(test)]
    pub(crate) fn table_of(&self, number: u64) -> Option<u64> {
        let generation = self.find(number)?;
        if generation == 0 {
            return Some(self.image);
        }
        let at = self.pool.iter().position(|it| it.holds == generation)?;
        Some(self.pool[at].table.as_ptr() as u64)
    }
}

impl<M: Memory> Drop for Switch<M> {
    /// Leaks the pool's tables: the cell or a thread may point at one still, and
    /// [`super::exec::publish`] writes them for the life of the program.
    fn drop(&mut self) {
        for pooled in self.pool.drain(..) {
            Box::leak(pooled.table);
        }
    }
}

fn declined(generation: u64, why: String) -> (Outcome, Option<Answer>) {
    // clone: a declined patch's reason, one for the program and one for the linker
    (Outcome::Declined { generation, why: why.clone() }, Some(Answer::Declined { generation, why }))
}

/// The span of `bytes`, a slice of `message`, written at `at` as linked.
fn place(message: &[u8], at: u64, bytes: &[u8], slide: u64) -> Result<Span, String> {
    let slid = at.wrapping_add(slide);
    if slid.checked_add(bytes.len() as u64).is_none() {
        // alloc: error path
        return Err(format!("a write at {at:#x} of {} bytes, past the end of memory", bytes.len()));
    }
    let from = bytes.as_ptr() as usize - message.as_ptr() as usize;
    Ok(Span { at: slid, from, len: bytes.len() })
}

/// Checks the FDEs of `patch` against the bytes it has (`written`: its code in memory and its
/// data writes) and puts in `starts` what to register at, in memory: each FDE, or the start
/// of each run of entries that ends in a zero length, and in `ends` where each such zero
/// length is.
fn frames<'m, M: Memory>(
    patch: PatchView<'_>,
    written: &impl Fn(u64, u64) -> Option<&'m [u8]>,
    slide: u64,
    fdes: &mut Vec<(u64, u64)>,
    starts: &mut Vec<u64>,
    ends: &mut Vec<u64>,
) -> Result<(), String> {
    fdes.clear();
    starts.clear();
    ends.clear();
    if patch.frames().len() == 0 {
        return Ok(());
    }
    if M::DWARF == Dwarf::Not {
        // alloc: error path
        return Err("DWARF unwind entries, which this platform's unwinder doesn't take".to_owned());
    }
    let linked = |at: u64| at.wrapping_sub(slide);
    let bytes = written;
    let length = |at: u64| -> Result<u32, String> {
        let field = bytes(at, 4).ok_or_else(|| {
            // alloc: error path
            format!("unwind entries reach {:#x}, in none of the patch's writes", linked(at))
        })?;
        Ok(le32(field))
    };
    fdes.extend(patch.frames().map(|(at, len)| (at.wrapping_add(slide), len)));
    fdes.sort_unstable();
    if let Some(pair) = fdes.windows(2).find(|it| it[1].0 < it[0].0.saturating_add(it[0].1)) {
        // alloc: error path
        return Err(format!("two unwind entries at {:#x}", linked(pair[1].0)));
    }
    for &(at, len) in fdes.iter() {
        let field = length(at)?;
        if field == 0 || field == u32::MAX || u64::from(field) + 4 != len {
            // alloc: error path
            return Err(format!(
                "the unwind entry at {:#x} is {len} bytes by the patch, and its length says {}",
                linked(at),
                u64::from(field) + 4
            ));
        }
        if bytes(at, len).is_none() {
            // alloc: error path
            return Err(format!(
                "the unwind entry at {:#x} runs past the patch's writes",
                linked(at)
            ));
        }
    }
    match M::DWARF {
        Dwarf::Entries | Dwarf::Not => starts.extend(fdes.iter().map(|it| it.0)),
        Dwarf::Sections => {
            let mut next = 0;
            while next < fdes.len() {
                let start = fdes[next].0;
                let mut at = start;
                loop {
                    let field = length(at)?;
                    if field == 0 {
                        ends.push(at);
                        break;
                    }
                    if field == u32::MAX {
                        // alloc: error path
                        return Err(format!("a 64-bit unwind entry at {:#x}", linked(at)));
                    }
                    if next < fdes.len() && fdes[next].0 == at {
                        next += 1;
                    } else if next < fdes.len() && fdes[next].0 < at {
                        // alloc: error path
                        return Err(format!(
                            "the unwind entry at {:#x} is not where the entries before it end",
                            linked(fdes[next].0)
                        ));
                    }
                    at = at.checked_add(4 + u64::from(field)).ok_or_else(|| {
                        // alloc: error path
                        format!("unwind entries from {:#x} overflow", linked(start))
                    })?;
                }
                starts.push(start);
            }
        }
    }
    Ok(())
}

/// The `len` bytes at `at` of whichever of `spans` (sorted) has all of them: a binary search.
fn find<'m>(message: &'m [u8], spans: &[Span], at: u64, len: u64) -> Option<&'m [u8]> {
    let span = spans.get(spans.partition_point(|it| it.end() <= at))?;
    let offset = at.checked_sub(span.at)?;
    let end = offset.checked_add(len)?;
    (end <= span.len as u64)
        .then(|| &message[span.from + offset as usize..span.from + end as usize])
}

/// `at..at + len`, when one of `code` (sorted, in memory) has all of it: a binary search.
fn in_code(code: &[Range<u64>], at: u64, len: u64) -> Option<Range<u64>> {
    let range = code.get(code.partition_point(|it| it.end <= at))?;
    let end = at.checked_add(len)?;
    (range.start <= at && end <= range.end).then_some(at..end)
}

/// The `len` bytes at `at` of the patch: of its code, read from `memory`, which
/// [`Switch::apply`] checked is readable and hashed whole, or of its data writes.
fn written_at<'m, M: Memory>(
    memory: &M,
    message: &'m [u8],
    code: &[Range<u64>],
    data: &[Span],
    at: u64,
    len: u64,
) -> Option<&'m [u8]> {
    match in_code(code, at, len) {
        Some(range) => memory.bytes(&range).ok(),
        None => find(message, data, at, len),
    }
}

/// The length of the UNWIND_INFO whose first 4 bytes are `header`: the fixed part, the unwind
/// codes (2 bytes each, an even count), then the handler's address or the chained
/// RUNTIME_FUNCTION. Not counted: a handler's data after its address, whose length only the
/// handler knows.
fn unwind_len(header: &[u8]) -> u64 {
    const EHANDLER: u8 = 1;
    const UHANDLER: u8 = 2;
    const CHAININFO: u8 = 4;
    let flags = header[0] >> 3;
    let codes = u64::from(header[2]).next_multiple_of(2);
    let after = if flags & CHAININFO != 0 {
        12
    } else if flags & (EHANDLER | UHANDLER) != 0 {
        4
    } else {
        0
    };
    4 + 2 * codes + after
}

fn span(at: u64, len: u64) -> Result<Range<u64>, String> {
    at.checked_add(len)
        .map(|end| at..end)
        // alloc: error path
        .ok_or_else(|| format!("a write at {at:#x} of {len} bytes, past the end of memory"))
}

fn inside(range: &Range<u64>, region: &Range<u64>) -> bool {
    range.start >= region.start && range.end <= region.end
}

fn overlap(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

fn le32(bytes: &[u8]) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(value)
}

pub(super) fn le64(bytes: &[u8]) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(value)
}

fn hex(bytes: &[u8]) -> String {
    // alloc: error path
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Lasting::hit`, with its look at the last end first, finds what a scan of every range
    /// finds, over seeded ranges added in place and probes before, among and past them.
    #[test]
    fn a_lasting_hit_is_what_a_scan_finds() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        let mut lasting = Lasting::new(4096).expect("reserved");
        let mut model: Vec<(Range<u64>, u64, Holds)> = Vec::new();
        for number in 1..400u64 {
            let start = next(1 << 20);
            let range = start..start + 1 + next(64);
            if model.iter().any(|it| overlap(&it.0, &range)) {
                continue;
            }
            let holds = if next(2) == 0 { Holds::Code } else { Holds::Unwind };
            lasting.room(1).expect("room");
            lasting.add(range.clone(), number, holds).expect("added");
            model.push((range, number, holds));
            model.sort_by_key(|it| it.0.start);
            for _ in 0..8 {
                let at = next((1 << 20) + 256);
                let probe = at..at + 1 + next(128);
                let scanned = model.iter().find(|it| overlap(&it.0, &probe)).cloned();
                assert_eq!(lasting.hit(&probe), scanned, "{probe:x?}");
            }
            let past = model.last().map_or(0, |it| it.0.end);
            assert_eq!(lasting.hit(&(past..past + 8)), None);
        }
    }
}
