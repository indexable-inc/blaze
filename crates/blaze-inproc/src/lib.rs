//! Dev runs and tests that patch running code with one store (docs/v2/plan/design/compile-time.md
//! change 6, docs/v2/plan/lanes/fastcompile/inproc.md).
//!
//! A [`CodeMemory`] is one OS memory object (Linux `memfd_create`, macOS a Mach memory entry,
//! Windows a pagefile-backed section) mapped writable in the daemon and executable in every
//! run process. Its first part holds the program's executable as the linker lays out its file
//! (a file offset is an offset here); an arena after it is handed out for code placed while
//! the program runs. Changing code is a write into the daemon's view, [`CodeMemory::publish`]
//! (the instruction-cache maintenance the architecture asks for), and one aligned atomic
//! store of the word that makes it reachable ([`CodeMemory::store_word`]): no `mprotect`, no
//! link, no rewrite of the file.
//!
//! Replaced code stays valid while a thread may be in it. [`CodeMemory::retire`] puts a range
//! behind a fence ([`fence`]): it is reused only once every thread of every run process has
//! passed a quiescent point after the store that unlinked it with no frame of its stack in
//! the range, and every live thread is known to the fence.
//!
//! # The start of a run
//!
//! A run is exact by construction: the program is its own executable at the path cargo gives
//! it, started by the kernel, so `current_exe`, `/proc/self/exe`, dyld's image 0, the PEB,
//! `dl_iterate_phdr` and backtraces all see the program there, as in a whole build.
//!
//! 1. The daemon writes the executable's bytes, as the memory object holds them, to cargo's
//!    path once per content key ([`host::Host::write_executable`]): not again while the key
//!    is the same, never for a patch.
//! 2. It spawns that file through `blaze-core`'s `Effect::Spawn` in the run's sandbox, with
//!    `BLAZE_INPROC` naming its host and a token ([`host::Program::command`]).
//! 3. Before `main` and before any other code of the program, the program's runtime
//!    ([`runtime`], linked into the executable) runs its start hook: an ELF `.preinit_array`
//!    entry, the first pointer of the Mach-O `__mod_init_func`, a PE TLS callback in
//!    `.CRT$XLAB` (the first; the CRT's start runs after TLS callbacks). Only the dynamic
//!    linker's own work comes before it: mapping and relocating, including the IFUNC
//!    resolvers of the executable, which therefore must not be lazy.
//! 4. The hook takes `BLAZE_INPROC` out of the environment (the program never sees it, nor
//!    do its children), connects, and says where its image's header is. The daemon answers
//!    with the objects and the overlays: the image's code ranges, the arena, cells and fixed
//!    maps. The hook checks each overlay against its own image's headers ([`image`]): a code
//!    range must be one of the image's executable segments at the same file offset, every
//!    other overlay must be inside a reservation the executable carries. Then it maps the
//!    fence's control words and the asks ([`ask`]: how the program asks the daemon for
//!    anything once it runs), and replaces each range with the shared mapping of the memory
//!    object (Linux `mmap` `MAP_FIXED|MAP_SHARED`, macOS `mach_vm_map` with
//!    `VM_FLAGS_OVERWRITE` of the entry it got through the rendezvous; on Windows the daemon
//!    rebuilds the image's region in the process from the section and the bytes it held).
//!    The bytes are equal at the moment of the swap: the file was written from the object,
//!    and the daemon writes only code no thread can reach yet. The hook says STARTED and
//!    returns; the program starts as it would anywhere.
//! 5. A program started otherwise (no `BLAZE_INPROC`: run by hand) runs its file as it is;
//!    [`runtime::attach`] says so, and [`runtime::quiesce`] does nothing.
//!
//! A hook that can't attach stops the process with exit code 127 and the reason on stderr
//! (and to the daemon when the channel is up): never a run of the file's code in place of
//! the object's.
//!
//! # Addresses
//!
//! Every address this crate deals in is *linked*: as the linker placed it. Each run process
//! has its own slide (ASLR), the same for the whole image: where its header is minus the
//! linked header ([`image::Layout::header`]), as `blaze-live`'s descriptor finds its own.
//! The arena, cells and fixed maps are mapped into reservations the executable carries in its
//! own address range ([`image::Layout::reserved`], first the arena, then the cells, then
//! fixed maps), so they slide with it. Hence:
//!
//! - bytes shared by every process (code, the arena, stub words, cells) must not depend on the
//!   slide: code is position-independent, and a word that names code names it relative to
//!   itself, never by absolute address;
//! - [`CodeMemory::linked`] gives an offset's linked address; a process's address is that
//!   plus its slide ([`host::Session::slide`], [`runtime::Attached::slide`]);
//! - the fence's ranges are linked addresses, and each thread subtracts its slide from its
//!   return addresses before comparing.
//!
//! The crate knows nothing of stubs, functions or dependency graphs: `blaze-lazy` and
//! `blaze-premap` build those on it.
//!
//! Linux on x86-64 and AArch64, macOS on AArch64 and Windows on x86-64. Elsewhere every
//! constructor stops with an [`Error`] naming the platform.

pub mod ask;
pub mod channel;
pub mod exec;
pub mod fence;
pub mod frame;
#[cfg(feature = "host")]
pub mod host;
pub mod image;
pub mod runtime;

mod alloc;
mod bind;
mod handshake;
mod sys;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use fence::{Fences, Grace};
pub use image::{Layout, Segment};

/// Why something of this crate could not be done, and on which platform.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    pub what: String,
}

impl Error {
    pub(crate) fn new(what: impl Into<String>) -> Error {
        Error { what: what.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze-inproc on {}-{}: {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.what
        )
    }
}

impl std::error::Error for Error {}

/// The object format of a program's executable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Format {
    Elf,
    MachO,
    Pe,
}

impl Format {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Format::Elf => 1,
            Format::MachO => 2,
            Format::Pe => 3,
        }
    }

    pub(crate) fn from_tag(tag: u8) -> Option<Format> {
        match tag {
            1 => Some(Format::Elf),
            2 => Some(Format::MachO),
            3 => Some(Format::Pe),
            _ => None,
        }
    }

    /// The format of programs on this platform.
    pub fn host() -> Option<Format> {
        if cfg!(target_os = "linux") {
            Some(Format::Elf)
        } else if cfg!(target_os = "macos") {
            Some(Format::MachO)
        } else if cfg!(windows) {
            Some(Format::Pe)
        } else {
            None
        }
    }
}

/// How a range of a memory object is mapped in a run process.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Access {
    /// Readable and executable, shared with the daemon's writable view: code.
    Execute,
    /// Readable, shared: read-only data the daemon may still change.
    Read,
    /// Readable and writable, a private copy: data. Nothing relies on whether the daemon's
    /// later writes show through: on Linux and Windows they do in pages the process has not
    /// written (copy on write), on macOS never (a snapshot taken when it is mapped, since a
    /// mapping of a Mach memory entry can't gain more rights than the entry has).
    Copy,
    /// Readable and writable, shared both ways: the fence's control words, and cells.
    Shared,
}

impl Access {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Access::Execute => 1,
            Access::Read => 2,
            Access::Copy => 3,
            Access::Shared => 4,
        }
    }

    pub(crate) fn from_tag(tag: u8) -> Option<Access> {
        match tag {
            1 => Some(Access::Execute),
            2 => Some(Access::Read),
            3 => Some(Access::Copy),
            4 => Some(Access::Shared),
            _ => None,
        }
    }
}

/// A range of a [`CodeMemory`], by offset in its object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CodeRange {
    pub offset: u64,
    pub len: u64,
}

impl CodeRange {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }

    pub fn contains(&self, offset: u64) -> bool {
        offset >= self.offset && offset < self.end()
    }
}

/// A range of another memory object a run process maps into its executable's address space
/// before the program starts: `blaze-premap`'s dependency image, at the same linked address
/// in every run (each process adds its slide).
#[derive(Clone, Copy)]
pub struct FixedMap<'a> {
    pub memory: &'a CodeMemory,
    pub range: CodeRange,
    /// The linked address it goes at: a multiple of [`CodeMemory::page`], inside a reservation
    /// of the executable that the arena and cells don't use. Anywhere else is an error naming
    /// it, never another address.
    pub at: u64,
    pub access: Access,
}

/// An 8-byte word of a fixed map's run-private copy that the start hook stores before any code
/// of the map runs: `blaze-premap`'s binding of a dependency image's open cells to a definition
/// the image's link could not see (`blaze_premap::start::Write`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ImageWrite {
    /// Linked, inside a [`FixedMap`] of [`Access::Copy`]; the run's slide is added.
    pub place: u64,
    pub value: u64,
    /// `value` is a linked address, which moves with the run's slide as `place` does; a
    /// thread-local's offset from the thread pointer does not.
    pub address: bool,
}

/// An 8-byte word of a fixed map's run-private copy bound to `name` in the run process's
/// global scope, which the system loader filled from the executable's libraries
/// (`blaze_premap::start::Lookup`): the start hook stores its address plus `addend`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ImageLookup<'a> {
    /// Linked, inside a [`FixedMap`] of [`Access::Copy`]; the run's slide is added.
    pub place: u64,
    /// As the image's symbol table has it: on Mach-O with its leading `_`, which the hook takes
    /// off for `dlsym`.
    pub name: &'a str,
    pub addend: i64,
    /// 0 when nothing defines it; a strong one nothing defines fails the start, naming it.
    pub weak: bool,
}

/// The run processes a code memory is mapped in.
struct Process {
    pid: u32,
    /// Its threads, as the OS listed them at the last [`CodeMemory::collect`]; reused.
    threads: Vec<u64>,
    /// It forks children the fence does not see ([`CodeMemory::attach_forking`]).
    forks: bool,
}

/// Where the parts after the image go in the executable's address space, once laid out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Placed {
    /// The linked address of the arena's first byte (offset `image`).
    arena: u64,
    /// The linked address of the cells' first byte (offset `image + arena`).
    cells: u64,
}

/// One memory object of code, writable here and executable in each run process it is mapped
/// in.
///
/// Its first `image` bytes are the program's executable as the linker lays out its file; the
/// `arena` after them is handed out by [`CodeMemory::reserve`] for code placed while the
/// program runs; the `cells` after that are words shared both ways with every run process
/// ([`CodeMemory::cell_view`]). The cells are offsets of the same memory to every caller, but
/// an OS object of their own: a run maps the image and arena's object never writable.
pub struct CodeMemory {
    object: sys::Object,
    view: sys::View,
    cells: Option<(sys::Object, Arc<sys::View>)>,
    control: sys::Object,
    fences: Fences<sys::View>,
    image: u64,
    arena_len: u64,
    cells_len: u64,
    arena: alloc::CodeHeap,
    /// Ranges retired behind a fence: with their grace once their entry is in the control
    /// table, waiting for room in it before.
    limbo: Vec<(Option<(Grace, u32)>, CodeRange)>,
    layout: Option<Layout>,
    placed: Option<Placed>,
    /// Nothing changed the image part since [`CodeMemory::lay_out`]: it is still the file the
    /// linker wrote, so it may be written out as the executable.
    pristine: bool,
    processes: Vec<Process>,
    page: u64,
}

impl CodeMemory {
    /// A new memory object of `image + arena` bytes (each rounded up to [`CodeMemory::page`]),
    /// zero, mapped writable here. Pages take memory only once written.
    pub fn new(name: &str, image: u64, arena: u64) -> Result<CodeMemory, Error> {
        CodeMemory::with_cells(name, image, arena, 0)
    }

    /// As [`CodeMemory::new`], with a third part of `cells` bytes (rounded up to the page)
    /// after the arena, zero, which every run process maps shared both ways.
    pub fn with_cells(name: &str, image: u64, arena: u64, cells: u64) -> Result<CodeMemory, Error> {
        let page = sys::page();
        let round = |len: u64, what: &str| {
            len.checked_next_multiple_of(page).ok_or_else(|| Error::new(format!("{what} size overflows")))
        };
        let image = round(image, "image")?;
        let arena = round(arena, "arena")?;
        let cells = round(cells, "cells")?;
        let len = image
            .checked_add(arena)
            .and_then(|it| it.checked_add(cells))
            .ok_or_else(|| Error::new("code memory size overflows"))?;
        if len == 0 {
            return Err(Error::new("a code memory of no bytes"));
        }
        sys::check_icache()?;
        if image + arena == 0 {
            return Err(Error::new("a code memory of cells only: no image and no arena"));
        }
        let object = sys::Object::create(name, image + arena)?;
        let view = object.map_here(Access::Shared)?;
        let control_len = fence::CONTROL_BYTES.next_multiple_of(page);
        let control = sys::Object::create(&format!("{name}.fence"), control_len)?;
        let control_view = control.map_here(Access::Shared)?;
        let fences = Fences::new(control_view);
        let cells_object = match cells {
            0 => None,
            _ => {
                let object = sys::Object::create(&format!("{name}.cells"), cells)?;
                let view = Arc::new(object.map_here(Access::Shared)?);
                Some((object, view))
            }
        };
        Ok(CodeMemory {
            object,
            view,
            cells: cells_object,
            control,
            fences,
            image,
            arena_len: arena,
            cells_len: cells,
            arena: alloc::CodeHeap::new(image, arena),
            limbo: Vec::new(),
            layout: None,
            placed: None,
            pristine: false,
            processes: Vec::new(),
            page,
        })
    }

    /// The length of all three parts: offsets run `0..len()`.
    pub fn len(&self) -> u64 {
        self.image + self.arena_len + self.cells_len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The image part's length: the linker writes `0..image_len()`.
    pub fn image_len(&self) -> u64 {
        self.image
    }

    /// The granule of a mapping: offsets and addresses of a [`FixedMap`] are multiples of it.
    pub fn page(&self) -> u64 {
        self.page
    }

    /// Reads the executable's headers from the image part (written whole before) and places
    /// the arena and the cells in its reservations: the arena in the first, the cells in the
    /// next. An image whose headers can't be read, or whose reservations are missing or too
    /// small, is an error naming what the link must give it.
    pub fn lay_out(&mut self, format: Format) -> Result<&Layout, Error> {
        let bytes = self.view.bytes(0..self.image as usize)?;
        let layout = image::layout(format, bytes, self.page)?;
        for segment in &layout.code {
            if segment.offset.checked_add(segment.len).is_none_or(|end| end > self.image) {
                return Err(Error::new(format!(
                    "the code segment at file offset {:#x} of {:#x} bytes runs past the image part of {:#x}",
                    segment.offset, segment.len, self.image
                )));
            }
        }
        let mut reserved = layout.reserved.iter();
        let mut take = |len: u64, what: &str| -> Result<u64, Error> {
            if len == 0 {
                return Ok(0);
            }
            let Some(range) = reserved.next() else {
                return Err(Error::new(format!(
                    "the executable reserves no address space for the {what} ({len:#x} bytes): the link must give \
                     it a reservation segment"
                )));
            };
            if range.end - range.start < len {
                return Err(Error::new(format!(
                    "the executable reserves {:#x} bytes at {:#x} for the {what}, which needs {len:#x}",
                    range.end - range.start,
                    range.start
                )));
            }
            Ok(range.start)
        };
        let arena = take(self.arena_len, "arena")?;
        let cells = take(self.cells_len, "cells")?;
        self.placed = Some(Placed { arena, cells });
        self.pristine = true;
        Ok(self.layout.insert(layout))
    }

    /// Nothing wrote the image part since [`CodeMemory::lay_out`] read it.
    pub fn is_pristine(&self) -> bool {
        self.pristine
    }

    /// The executable's layout, once [`CodeMemory::lay_out`] read it.
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }

    /// Writes `bytes` at `offset` of the image or the arena: code no thread can reach yet (the
    /// image before its run, a reserved range before its slot is stored). Call
    /// [`CodeMemory::publish`] after. Cells change only by [`CodeMemory::store_word`].
    pub fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        let range = self.checked(offset, bytes.len() as u64)?;
        self.touch(offset);
        self.view.bytes_mut(range)?.copy_from_slice(bytes);
        Ok(())
    }

    /// Sets `len` bytes at `offset` to `byte` (a trap, to fill what a retired range held).
    pub fn fill(&mut self, offset: u64, len: u64, byte: u8) -> Result<(), Error> {
        let range = self.checked(offset, len)?;
        self.touch(offset);
        self.view.bytes_mut(range)?.fill(byte);
        Ok(())
    }

    /// The bytes at `range` of the image or the arena, as the daemon's view has them.
    pub fn bytes(&self, range: CodeRange) -> Result<&[u8], Error> {
        let range = self.checked(range.offset, range.len)?;
        self.view.bytes(range)
    }

    /// Makes what was written at `range` the code a thread runs once it reaches it: the
    /// data cache cleaned and the instruction cache invalidated to the point of unification
    /// on AArch64, nothing on x86-64 (each thread serializes at its next quiescent point).
    /// Call it after the writes and before the store that makes the range reachable.
    pub fn publish(&self, range: CodeRange) -> Result<(), Error> {
        let range = self.checked(range.offset, range.len)?;
        self.view.publish(range);
        Ok(())
    }

    /// Stores `value` in the aligned 8-byte word at `offset` with one release store: a slot,
    /// or the target word a stub loads. A thread that loads it sees the old value or the new,
    /// whole. Every process sees the same value, so it must not depend on a slide.
    pub fn store_word(&mut self, offset: u64, value: u64) -> Result<(), Error> {
        self.word(offset, "store")?.store(value, std::sync::atomic::Ordering::Release);
        self.touch(offset);
        Ok(())
    }

    /// The aligned 8-byte word at `offset`.
    pub fn load_word(&self, offset: u64) -> Result<u64, Error> {
        Ok(self.word(offset, "load")?.load(std::sync::atomic::Ordering::Acquire))
    }

    fn word(&self, offset: u64, what: &str) -> Result<&std::sync::atomic::AtomicU64, Error> {
        if !offset.is_multiple_of(8) {
            return Err(Error::new(format!("a word {what} at {offset:#x}, which is not 8-byte aligned")));
        }
        let cells = self.image + self.arena_len;
        match &self.cells {
            Some((_, view)) if offset >= cells && offset - cells < self.cells_len => Ok(view.word((offset - cells) as usize)),
            _ => {
                self.checked(offset, 8)?;
                Ok(self.view.word(offset as usize))
            }
        }
    }

    /// The image part is no longer the linker's file once anything writes into it.
    fn touch(&mut self, offset: u64) {
        if offset < self.image {
            self.pristine = false;
        }
    }

    /// Stores the AArch64 instruction `instruction` at the aligned `offset` with one 4-byte
    /// store, then publishes its line: for a `B` or a `NOP`, which the architecture lets one
    /// thread change while another runs it. x86-64 code changes through target words
    /// ([`CodeMemory::store_word`]): there this is an error.
    pub fn store_instruction(&mut self, offset: u64, instruction: u32) -> Result<(), Error> {
        if !cfg!(target_arch = "aarch64") {
            return Err(Error::new(
                "an instruction store: x86-64 code changes through the target words its stubs load",
            ));
        }
        if !is_concurrently_modifiable(instruction) {
            return Err(Error::new(format!(
                "the instruction {instruction:#010x} at {offset:#x} is not one AArch64 lets a thread \
                 change while another runs it (B, BL, NOP, BRK, ISB, SVC)"
            )));
        }
        let range = self.checked(offset, 4)?;
        if !offset.is_multiple_of(4) {
            return Err(Error::new(format!("an instruction at {offset:#x}, which is not 4-byte aligned")));
        }
        self.touch(offset);
        self.view.word32(offset as usize).store(instruction, std::sync::atomic::Ordering::Release);
        self.view.publish(range);
        Ok(())
    }

    /// A range of the arena of `len` bytes aligned to `align` (a power of two), for code placed
    /// while the program runs. Full is an error saying how much is free and how much the
    /// fences still hold.
    pub fn reserve(&mut self, len: u64, align: u64) -> Result<CodeRange, Error> {
        if len == 0 || !align.is_power_of_two() {
            return Err(Error::new(format!("a reservation of {len} bytes aligned to {align}")));
        }
        if let Some(range) = self.arena.take(len, align) {
            return Ok(range);
        }
        self.collect()?;
        self.arena.take(len, align).ok_or_else(|| {
            Error::new(format!(
                "the code memory's arena is full: {len} bytes asked, {} free in pieces of at most {}, \
                 {} held by fences ({} ranges) until every thread of {} passes a quiescent point",
                self.arena.free(),
                self.arena.largest(),
                self.held(),
                self.limbo.len(),
                self.describe_processes(),
            ))
        })
    }

    /// Gives back a reserved range nothing could ever have reached (its slot was never
    /// stored): free at once.
    pub fn unreserve(&mut self, range: CodeRange) -> Result<(), Error> {
        self.arena_range(range)?;
        self.arena.give(range);
        Ok(())
    }

    /// Retires `range` of the arena, unlinked by stores already made: it is reused once every
    /// thread that could be in it has left it ([`CodeMemory::collect`]).
    pub fn retire(&mut self, range: CodeRange) -> Result<(), Error> {
        self.arena_range(range)?;
        if self.placed.is_none() || self.processes.is_empty() {
            self.arena.give(range);
            return Ok(());
        }
        self.limbo.push((None, range));
        self.post();
        Ok(())
    }

    /// Frees every retired range whose fence has passed, and posts waiting ones to the
    /// control table. Says how many ranges it freed.
    pub fn collect(&mut self) -> Result<usize, Error> {
        if self.limbo.is_empty() {
            return Ok(0);
        }
        self.post();
        if self.processes.iter().any(|it| it.forks) {
            return Ok(0);
        }
        let births = self.fences.births();
        for process in &mut self.processes {
            sys::threads(process.pid, &mut process.threads)?;
        }
        let lists: Vec<(u32, &[u64])> =
            self.processes.iter().map(|it| (it.pid, it.threads.as_slice())).collect();
        let oldest = self.fences.oldest(births, &lists);
        let Some(oldest) = oldest else { return Ok(0) };
        let mut freed = 0;
        let mut index = 0;
        while index < self.limbo.len() {
            match self.limbo[index] {
                (Some((grace, entry)), range) if grace.passed_by(oldest) => {
                    self.fences.clear(entry);
                    self.arena.give(range);
                    self.limbo.swap_remove(index);
                    freed += 1;
                }
                _ => index += 1,
            }
        }
        if freed != 0 {
            self.post();
        }
        Ok(freed)
    }

    /// Stores `value` where every run process reads it with one load
    /// ([`runtime::Attached::announced`]): `blaze-live`'s patch generation, so its poll asks
    /// the daemon for a patch over the channel only once there is one.
    pub fn announce(&self, value: u64) {
        self.fences.announce(value);
    }

    /// The bytes retired and not yet free.
    pub fn held(&self) -> u64 {
        self.limbo.iter().map(|(_, range)| range.len).sum()
    }

    /// The linked address of `offset`: in one of the image's code segments, the arena or the
    /// cells, once [`CodeMemory::lay_out`] placed them. A run process has it at this plus its
    /// slide.
    pub fn linked(&self, offset: u64) -> Option<u64> {
        let placed = self.placed?;
        let arena = self.image..self.image + self.arena_len;
        let cells = arena.end..arena.end + self.cells_len;
        if arena.contains(&offset) {
            return Some(placed.arena + (offset - arena.start));
        }
        if cells.contains(&offset) {
            return Some(placed.cells + (offset - cells.start));
        }
        let layout = self.layout.as_ref()?;
        let segment = layout.code.iter().find(|it| offset >= it.offset && offset - it.offset < it.len)?;
        Some(segment.linked + (offset - segment.offset))
    }

    /// The offset of the linked address `linked`, as [`CodeMemory::linked`] maps them.
    pub fn offset_of(&self, linked: u64) -> Option<u64> {
        let placed = self.placed?;
        if linked >= placed.arena && linked - placed.arena < self.arena_len {
            return Some(self.image + (linked - placed.arena));
        }
        if linked >= placed.cells && linked - placed.cells < self.cells_len {
            return Some(self.image + self.arena_len + (linked - placed.cells));
        }
        let layout = self.layout.as_ref()?;
        let segment = layout.code.iter().find(|it| linked >= it.linked && linked - it.linked < it.len)?;
        Some(segment.offset + (linked - segment.linked))
    }

    /// Where the cells part is ([`CodeMemory::with_cells`]); empty when there is none.
    pub fn cells(&self) -> CodeRange {
        CodeRange { offset: self.image + self.arena_len, len: self.cells_len }
    }

    /// The cells part, to read with no lock while compiles and placements hold this memory.
    pub fn cell_view(&self) -> Option<CellView> {
        let (_, view) = self.cells.as_ref()?;
        Some(CellView { view: Arc::clone(view), start: self.image + self.arena_len, len: self.cells_len })
    }

    /// Records that the run process `pid` mapped the memory.
    pub fn attach(&mut self, pid: u32) {
        if !self.processes.iter().any(|it| it.pid == pid) {
            self.processes.push(Process { pid, threads: Vec::new(), forks: false });
        }
    }

    /// As [`CodeMemory::attach`], for a process that forks children which run the program's
    /// code (a test binary's fork server): the fence can't see a child before it is told of
    /// it, so nothing retired is freed until this process and its children have exited
    /// ([`CodeMemory::exited`], called once the last of them ended).
    pub fn attach_forking(&mut self, pid: u32) {
        self.attach(pid);
        if let Some(process) = self.processes.iter_mut().find(|it| it.pid == pid) {
            process.forks = true;
        }
    }

    /// Records that the run process `pid` exited: its threads hold no fence any more.
    pub fn exited(&mut self, pid: u32) {
        self.processes.retain(|it| it.pid != pid);
        self.fences.forget(pid);
        if self.processes.is_empty() {
            for (entry, range) in std::mem::take(&mut self.limbo) {
                if let Some((_, entry)) = entry {
                    self.fences.clear(entry);
                }
                self.arena.give(range);
            }
        }
    }

    /// The arena's offset, length and linked address, once laid out.
    pub(crate) fn arena_part(&self) -> Option<(CodeRange, u64)> {
        let placed = self.placed?;
        Some((CodeRange { offset: self.image, len: self.arena_len }, placed.arena))
    }

    /// The cells' offset, length and linked address, once laid out.
    pub(crate) fn cells_part(&self) -> Option<(CodeRange, u64)> {
        let placed = self.placed?;
        Some((self.cells(), placed.cells))
    }

    /// The image and arena's object.
    pub(crate) fn object(&self) -> &sys::Object {
        &self.object
    }

    pub(crate) fn cells_object(&self) -> Option<&sys::Object> {
        self.cells.as_ref().map(|(object, _)| object)
    }

    pub(crate) fn control(&self) -> &sys::Object {
        &self.control
    }

    /// Posts what waits in limbo to the control table, under one grace.
    fn post(&mut self) {
        let Some(placed) = self.placed else { return };
        let mut posted = false;
        for (entry, range) in &mut self.limbo {
            if entry.is_some() {
                continue;
            }
            let start = placed.arena + (range.offset - self.image);
            let Some(slot) = self.fences.post(start, start + range.len) else { break };
            *entry = Some((Grace::NONE, slot));
            posted = true;
        }
        if posted {
            let grace = self.fences.start();
            for (entry, _) in &mut self.limbo {
                if let Some((g, _)) = entry
                    && *g == Grace::NONE
                {
                    *g = grace;
                }
            }
        }
    }

    fn describe_processes(&self) -> String {
        let pids: Vec<String> = self
            .processes
            .iter()
            .map(|it| if it.forks { format!("{} (which forks: until it and its children exit)", it.pid) } else { it.pid.to_string() })
            .collect();
        format!("processes {}", pids.join(", "))
    }

    /// The range of the image and arena's object at `offset`.
    fn checked(&self, offset: u64, len: u64) -> Result<std::ops::Range<usize>, Error> {
        let code = self.image + self.arena_len;
        match offset.checked_add(len) {
            Some(end) if end <= code => Ok(offset as usize..end as usize),
            _ => Err(Error::new(format!(
                "{len} bytes at {offset:#x}: not in the image and arena 0..{code:#x} of a code memory (the cells \
                 after them change only by word)"
            ))),
        }
    }

    fn arena_range(&self, range: CodeRange) -> Result<(), Error> {
        let end = self.image + self.arena_len;
        if range.offset < self.image || range.end() > end || range.len == 0 {
            return Err(Error::new(format!(
                "{:#x}..{:#x} is not in the arena {:#x}..{end:#x}",
                range.offset,
                range.end(),
                self.image,
            )));
        }
        Ok(())
    }
}

/// The cells part of a [`CodeMemory`], read with no lock: a second mapping of the object
/// here, kept while any clone lives.
#[derive(Clone)]
pub struct CellView {
    view: Arc<sys::View>,
    start: u64,
    len: u64,
}

impl CellView {
    /// One relaxed load of the aligned word at `offset` of the object, which must be in the
    /// cells part.
    pub fn load(&self, offset: u64) -> Result<u64, Error> {
        if !offset.is_multiple_of(8) || offset < self.start || offset.saturating_add(8) > self.start + self.len {
            return Err(Error::new(format!(
                "a cell load at {offset:#x}: not an aligned word of the cells {:#x}..{:#x}",
                self.start,
                self.start + self.len
            )));
        }
        Ok(self.view.word((offset - self.start) as usize).load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// Whether AArch64 lets one thread change `instruction` while another may run it (Arm ARM
/// B2.2.5, "concurrent modification and execution of instructions").
fn is_concurrently_modifiable(instruction: u32) -> bool {
    let b_or_bl = instruction & 0x7c00_0000 == 0x1400_0000;
    let nop = instruction == 0xd503_201f;
    let isb = instruction == 0xd503_3fdf;
    let brk = instruction & 0xffe0_001f == 0xd420_0000;
    let svc = instruction & 0xffe0_001f == 0xd400_0001;
    b_or_bl || nop || isb || brk || svc
}
