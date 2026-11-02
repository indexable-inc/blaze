//! The parallel, in-place linker (docs/v2/plan/lanes/link/notes.md; #275, #232, #1496).
//!
//! An [`Image`] is an executable or shared library kept linked. A [`Change`] says which
//! functions, statics, names and native inputs came or went; [`Image::stage`] places the new
//! pieces in room the image keeps and relocates them in parallel, on a copy of the image's
//! state, and nothing is visible until [`Image::commit`]: one atomic rename of the file. A
//! live image is committed into the code memory its running programs map instead
//! ([`Image::commit_into`]), which gives the one message they take to switch every jump-table
//! slot at once. Callers reach
//! a function through its [`Slot`]'s stub, so a function that moved changes one table entry.
//! [`Image::roll_back`] returns to the image before the last commit: a patch that panicked.
//! Positions the program observes (`line!()`, `Location`) and debug info are filled here
//! from the span side tables, so a reformat compiles nothing.
//!
//! The one `unsafe` block is `intern::name_at`'s `from_utf8_unchecked` of names it appended as
//! `&str`s.

#![feature(allocator_api, allocator_ext)]
#![deny(unsafe_code)]

mod arena;
mod commit;
mod counter;
mod cow;
mod dwarf;
mod elf;
mod file;
mod intern;
mod lazy;
mod live;
mod macho;
mod model;
mod native;
mod output;
mod pe;
mod place;
mod position;
pub mod premap;
mod reloc;
mod reset;
mod sanitize;
mod stage;
mod state;
mod symbols;
mod table;
pub mod target;
mod unwind;
mod wasm;
mod write;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;

use blaze_codegen::{CompiledFunction, InstanceKey, Static, Target};
use blaze_core::{Effect, FileName};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use blaze_mem::Frame;

pub use lazy::{LazyPlacer, Slots};
pub use output::{Debug, DevRun, Format, Live, Native, NativeKind, Output, OutputKind, Platform};
pub use position::{
    COLUMN, LINE, LOCATION_ALIGN, LOCATION_SIZE, Position, SpanTable, file_name, location,
};
pub use premap::{NoOutside, Outside};

use commit::{Pending, Undo, Writes};
use model::{Formats, NativeFile, Plan, Writer};
use stage::{Contents, Stage};
use state::{State, has_table};
use symbols::{Obj, SymKey, Symbols};

/// A function's place in the jump table: stable for as long as the image lives.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Slot(pub u32);

/// What a link changes.
#[derive(Default)]
pub struct Change<'a> {
    pub added: Vec<&'a CompiledFunction>,
    pub removed: Vec<InstanceKey>,
    /// Slots that run another function than the one they are named for, as `(slot, function)`:
    /// a tiered function's LLVM code in the slot its Cranelift code filled (codegen's
    /// `TieredUp::replaced`), an edited function in the slot its callers call. The function
    /// is in `added` or already linked; only an image with a jump table has slots.
    pub slots: Vec<(InstanceKey, InstanceKey)>,
    pub statics: Vec<&'a Static>,
    pub removed_statics: Vec<(ItemKey, ContentId)>,
    /// Names Rust code defines (`#[no_mangle]`, `#[export_name]`, the entry point's callee),
    /// for native code and the loader, and names no longer defined.
    pub names: Vec<(String, Target)>,
    pub unnamed: Vec<String>,
    /// Names for debuggers, symbol tables and backtraces.
    pub debug_names: Vec<(InstanceKey, String)>,
    pub natives: Vec<Native<'a>>,
    pub removed_natives: Vec<ContentId>,
    /// The span side tables the positions are filled from, by anchor ([`SpanTable::encode`]).
    pub spans: Vec<(ContentId, &'a [u8])>,
}

/// The output of a link, made visible only by a commit.
pub struct Staged {
    base: u64,
    state: State,
    writes: Writes,
    /// The ranges of the old file the commit overwrites.
    undo: Vec<(u64, u64)>,
}

impl Staged {
    /// The generation the image is at once this is committed.
    pub fn generation(&self) -> u64 {
        self.state.generation
    }

    /// The length of the file this commits.
    pub fn len(&self) -> u64 {
        self.writes.len
    }

    /// Whether this lays the whole image out again (its first link, or an output that keeps
    /// no room), rather than placing what changed into the room the image kept.
    pub fn full(&self) -> bool {
        self.writes.full
    }
}

impl fmt::Debug for Staged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Staged")
            .field("base", &self.base)
            .field("generation", &self.generation())
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

/// Why a link failed: names the function and the relocation.
#[derive(PartialEq, Eq, Debug)]
pub struct LinkError {
    pub function: Option<InstanceKey>,
    pub what: String,
    /// Why, typed, where the caller can act on it ([`Refusal`]).
    pub refusal: Option<Refusal>,
}

/// Why a link was refused, where the caller can act on it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// A region the image kept needs `need` bytes and has room for `room`: only a link of the
    /// image from scratch ([`Image::new`]) can take the change, as for the next three.
    NoRoom { need: u64, room: u64 },
    /// The stage needs a region the image was laid out without.
    NoRegion,
    /// Every one of the jump table's `slots` is taken.
    TableFull { slots: u32 },
    /// The image's thread-locals would move in the run's area.
    TlsMoved,
    /// A native library on the link line is in none of the directories searched.
    NotFound(Box<NotFound>),
}

/// A native library the link line names that no directory searched has.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NotFound {
    /// The library as the line names it: a `-l` name, a `/DEFAULTLIB`, an `LC_LINKER_OPTION`'s.
    pub library: String,
    /// The file names tried in each directory, in the order tried.
    pub files: Vec<String>,
    /// Every directory searched, in order, and what named it.
    pub searched: Vec<(PathBuf, Searched)>,
}

/// What named a directory a native library was searched in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Searched {
    /// The link's own `-L`s: the crates' `cargo:rustc-link-search`, `-L` flags.
    Link,
    /// The C driver's linker job (`-L`, `-F`): nix's `NIX_LDFLAGS`, `LIBRARY_PATH`, the
    /// driver's own.
    CDriver,
    /// The toolchain's own: the SDK's, `-print-search-dirs`', `LIB`'s, wasi-libc's.
    Toolchain,
    /// The directory of the install name a Mach-O library re-exports (`LC_REEXPORT_DYLIB`).
    InstallName,
}

impl LinkError {
    /// The refusal, when the image refused the stage for want of room: [`Refusal::NoRoom`],
    /// `NoRegion`, `TableFull` or `TlsMoved`.
    pub fn no_room(&self) -> Option<&Refusal> {
        self.refusal.as_ref().filter(|it| !matches!(it, Refusal::NotFound(_)))
    }

    /// The native library not found, and every directory it was searched in.
    pub fn not_found(&self) -> Option<&NotFound> {
        match &self.refusal {
            Some(Refusal::NotFound(it)) => Some(it),
            _ => None,
        }
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.function {
            Some(function) => write!(f, "linking {function:?}: {}", self.what),
            None => write!(f, "linking: {}", self.what),
        }
    }
}

impl std::error::Error for LinkError {}

/// One linked executable or shared library.
pub struct Image {
    to: FileName,
    next: FileName,
    name: String,
    output: Output,
    /// The format's writer, dispatched by a tag `match` ([`Formats`]).
    writer: Formats,
    plan: Plan,
    symbols: Symbols,
    objects: HashMap<Obj, native::Read>,
    state: State,
    history: VecDeque<Undo>,
    pending: Option<Pending>,
    tags: u32,
    /// The last generation given out: generations are never reused.
    generations: u64,
    /// Each stage's scratch, reset when the next begins: a stage like the last one finds its
    /// room already there.
    arena: Frame,
    /// Bytes reused for keys written out to be hashed, across stages.
    scratch: Vec<u8>,
    /// Where a dependency image or the image beside one is fixed ([`premap`]); `None` for
    /// every other image.
    place: Option<premap::Place>,
    /// The offset of the cells part of the code memory the image was last committed into
    /// ([`Image::commit_into`]), which [`Image::counter`] counts from.
    cells: Option<u64>,
    /// A lazy image's session ([`Image::stage_lazy`]); `None` for every other image.
    lazy: Option<lazy::Lazy>,
    /// The counter pairs [`LazyPlacer`] gave since the last commit, which a commit of a stage
    /// made before them carries over ([`Image::carry_placed_pairs`]).
    placed_pairs: Vec<(symbols::IdNo, u32)>,
}

impl Image {
    /// A new image to be written at `to`, from scratch, linked as `output` says.
    pub fn new(to: FileName, output: Output) -> Result<Image, LinkError> {
        Image::placed(to, output, None)
    }

    /// A new image linked beside `outside`, a dependency image the same run maps
    /// ([`premap`]): its first byte at `outside.end()` rounded up to the page
    /// ([`premap::page`]), its thread-locals at `tls_bias` plus `outside.tls_end()` rounded
    /// up to their alignment, and its references resolved against `outside` as a whole link
    /// of both would resolve them. Staged only with [`Image::stage_beside`], against the
    /// same `outside`.
    pub fn beside<O: Outside>(to: FileName, output: Output, outside: &O, tls_bias: i64) -> Result<Image, LinkError> {
        let page = premap::page(output.format, output.arch);
        let base = blaze_mem::align::checked_align_up_u64(outside.end(), page)
            .ok_or_else(|| LinkError {
                function: None,
                // alloc: error path
                what: format!("an image beside one ending at {:#x}", outside.end()),
                refusal: None,
            })?;
        let place = premap::Place { base, page, tls_bias, tls_from: outside.tls_end(), end: outside.end(), beside: true };
        Image::placed(to, output, Some(place))
    }

    fn placed(to: FileName, output: Output, place: Option<premap::Place>) -> Result<Image, LinkError> {
        let fail = |what: String| LinkError { function: None, what, refusal: None };
        if output.tags & 0xffff_ffff != 0 {
            // alloc: error path
            return Err(fail(format!("the effect tags {:#x} have low bits set: they are the image's", output.tags)));
        }
        if output.live.is_some() && !output.incremental {
            return Err(fail("a live image must be incremental: it keeps room for patches".into()));
        }
        let writer = model::writer(&output)?;
        let mut plan = writer.plan();
        if let Some(place) = &place {
            premap::fix(&mut plan, place).map_err(fail)?;
        }
        if output.live.is_some() && plan.indexed_code {
            // alloc: error path
            return Err(fail(format!("live patching of {:?} images", output.format)));
        }
        if let Some(dev_run) = &output.dev_run {
            dev_run_fits(&output, dev_run, &plan, &writer).map_err(fail)?;
        }
        // alloc: once per image, the name its next file is written at
        let next = FileName::new(&format!("{}.blaze-next", to.as_str()))
            // alloc: error path
            .ok_or_else(|| fail(format!("{to}: no name beside it to write the next image at")))?;
        // alloc: once per image, its file name for the writer
        let name = to.as_str().rsplit('/').next().unwrap_or(to.as_str()).to_owned();
        Ok(Image {
            to,
            next,
            name,
            output,
            writer,
            plan,
            // alloc: error path
            symbols: Symbols::new().map_err(|error| fail(error.to_string()))?,
            objects: HashMap::default(),
            state: State::new(),
            history: VecDeque::with_capacity(commit::HISTORY + 1),
            pending: None,
            tags: 0,
            generations: blaze_live::patch::FIRST_GENERATION,
            // alloc: error path
            arena: arena::stage_frame().map_err(|error| fail(error.to_string()))?,
            scratch: Vec::with_capacity(64),
            place,
            cells: None,
            lazy: None,
            // alloc: none until the lazy placer gives a pair
            placed_pairs: Vec::new(),
        })
    }

    /// Places and relocates what `change` says, in parallel, into address space not yet
    /// visible. The image is as it was until the result is committed; a failed stage leaves
    /// nothing behind.
    pub fn stage(&mut self, change: Change<'_>) -> Result<Staged, LinkError> {
        if self.place.is_some_and(|it| it.beside) {
            return Err(LinkError {
                function: None,
                what: "an image linked beside a dependency image is staged with `stage_beside`, against it".into(),
                refusal: None,
            });
        }
        self.stage_with(change, &NoOutside)
    }

    /// [`Image::stage`] of a lazy image ([`lazy`]): the dev run whose lazy functions `session`
    /// lists and whose stub region is `stubs` (both from [`blaze_lazy::Session::new`], for the
    /// image's architecture). Its first link places the region and starts each lazy function's
    /// slot at its entry; every reference to a lazy function, [`Target::Direct`] too, reaches
    /// its slot stub, and [`LazyPlacer`] places its code. Later links are [`Image::stage`]'s
    /// or this with the same session. An image that is not a dev run with a jump table, or a
    /// different session than the first link's, is an error.
    pub fn stage_lazy(
        &mut self,
        change: Change<'_>,
        session: &blaze_lazy::Session,
        stubs: &blaze_lazy::stub::Stubs,
    ) -> Result<Staged, LinkError> {
        if self.state.laid_out {
            if !self.lazy.as_ref().is_some_and(|it| it.is(session, stubs)) {
                return Err(LinkError {
                    function: None,
                    refusal: None,
                    // alloc: error path
                    what: format!(
                        "{}: staged lazy with another session than its first link's: an image is made lazy, for one \
                         session, at its first link",
                        self.to
                    ),
                });
            }
            return self.stage(change);
        }
        let table = has_table(&self.output, self.plan.indexed_code);
        let was = self.lazy.replace(lazy::Lazy::new(&self.output, table, session, stubs)?);
        let staged = self.stage(change);
        if staged.is_err() {
            self.lazy = was;
        }
        staged
    }

    /// The linked address of a lazy image's stub region: the `base` its session's
    /// [`blaze_lazy::Session::slots`] and [`blaze_lazy::Session::entry`] take, as the code
    /// memory has it at [`blaze_inproc::CodeMemory::offset_of`]. `None` before the first link
    /// and for an image that is not lazy.
    pub fn lazy_stubs(&self) -> Option<u64> {
        self.lazy.as_ref()?;
        self.state.addr_of(self.symbols.find(SymKey::LazyStubs)?)
    }

    /// [`Image::stage`] of an image made by [`Image::beside`]: names, functions and statics
    /// the change does not define resolve against `outside`, which must be the one the image
    /// was made beside (the same end and thread-local end).
    pub fn stage_beside<O: Outside>(&mut self, change: Change<'_>, outside: &O) -> Result<Staged, LinkError> {
        match self.place {
            Some(place) if place.beside && place.end == outside.end() && place.tls_from == outside.tls_end() => {}
            Some(place) if place.beside => {
                return Err(LinkError {
                    function: None,
                    refusal: None,
                    // alloc: error path
                    what: format!(
                        "the image was made beside one ending at {:#x}, thread-locals at {:#x}, and is staged against \
                         one ending at {:#x}, thread-locals at {:#x}: make it again beside that one",
                        place.end,
                        place.tls_from,
                        outside.end(),
                        outside.tls_end()
                    ),
                });
            }
            _ => {
                return Err(LinkError {
                    function: None,
                    what: "`stage_beside` of an image not made by `Image::beside`".into(),
                    refusal: None,
                });
            }
        }
        self.stage_with(change, outside)
    }

    fn stage_with<O: Outside>(&mut self, change: Change<'_>, outside: &O) -> Result<Staged, LinkError> {
        if self.pending.is_some() {
            return Err(LinkError { function: None, what: "a commit is in flight".into(), refusal: None });
        }
        let full = !self.state.laid_out || !self.output.incremental;
        let table = has_table(&self.output, self.plan.indexed_code);
        // clone: reference counts; columns, the regions list and pages are shared until written (state.rs)
        let mut new = self.state.clone();
        new.generation = self.generations + 1;
        if new.wants_compaction() {
            new.compact();
        }
        self.arena.reset();
        let arena = &self.arena;
        let contents = Contents::new(new.piece_count(), arena);
        let mut stage = Stage {
            output: &self.output,
            writer: &self.writer,
            plan: &self.plan,
            symbols: &mut self.symbols,
            objects: &mut self.objects,
            native: native::Pass::default(),
            old: &self.state,
            new,
            name: &self.name,
            arena,
            contents,
            fresh: cow::IdSet::new_in(arena),
            write: cow::IdSet::new_in(arena),
            changed: cow::SymSet::new_in(arena),
            dirty_cells: cow::IdSet::new_in(arena),
            removed: Vec::new_in(arena),
            emptied: Vec::new_in(arena),
            touched: Vec::new_in(arena),
            counted: Vec::new_in(arena),
            full,
            table,
            functions: 0,
            statics: 0,
            owned: Vec::new_in(arena),
            scratch: &mut self.scratch,
            spans_changed: false,
            sancov: [None; 4],
            frames_end: None,
            lazy: self.lazy.as_ref(),
        };
        stage.inputs(&change)?;
        stage.apply(&change)?;
        stage.sanitize(&change)?;
        stage.beside_targets(outside)?;
        stage.descriptor();
        stage.start_hook()?;
        stage.counter_cells();
        stage.resolve(outside)?;
        stage.lazy_hook()?;
        stage.referrers();
        stage.needs()?;
        stage.sweep();
        stage.asan_globals(&change)?;
        stage.dangling()?;
        // Each round places what the last made; GOT cells and PLT stubs need nothing past the
        // next, so a stage that still makes pieces after `ROUNDS` would write them unplaced.
        const ROUNDS: usize = 8;
        let mut settled = false;
        for _ in 0..ROUNDS {
            stage.place()?;
            stage.referrers();
            let before = stage.fresh.len();
            stage.needs()?;
            if stage.fresh.len() == before {
                settled = true;
                break;
            }
        }
        if !settled {
            return Err(stage::error(
                None,
                format!("the GOT cells and PLT stubs the image needs were still growing after {ROUNDS} placements"),
            ));
        }
        stage.coverage_moved();
        if let Some(place) = self.place {
            stage.fix_tls(&place)?;
        }
        if !self.state.laid_out {
            stage.new.lineage = lineage(&stage, &self.to);
        }
        stage.fill_descriptor()?;
        stage.relocate_all()?;
        let base = self.state.generation;
        let finished = stage.finish()?;
        Ok(Staged {
            base,
            state: finished.state,
            writes: Writes {
                full: finished.full,
                len: finished.len,
                old_len: finished.old_len,
                writes: finished.writes,
                message: if self.state.laid_out { finished.message } else { None },
            },
            undo: finished.undo,
        })
    }

    /// Makes `staged` the image, all at once, through effects: the new file renamed over the
    /// old. Completions go to [`Image::complete`]. A commit with a message for running
    /// programs (a later link of a [`Output::live`] image) is refused before anything
    /// changes: its patches reach them only through the code memory ([`Image::commit_into`]).
    pub fn commit(&mut self, mut staged: Staged, effects: &mut Vec<Effect>) -> Result<(), LinkError> {
        self.can_commit(&staged)?;
        self.carry_placed_pairs(&mut staged.state)?;
        let full = staged.writes.full;
        let steps = self.steps(staged.writes)?;
        let revert = self.take(staged.state, full, staged.undo);
        self.placed_pairs.clear();
        self.begin(steps, revert, effects);
        Ok(())
    }

    /// Carries the counter pairs [`LazyPlacer`] gave while `state` was staged into it: they are
    /// its keys' for as long as the image lives. A stage that gave one of those keys another
    /// pair, or gave pairs into the ones the placer took, is refused: stage it again.
    fn carry_placed_pairs(&self, state: &mut State) -> Result<(), LinkError> {
        let capacity = counter::capacity(&self.output);
        for (id, pair) in &self.placed_pairs {
            if !state.pairs.carry(*id, *pair, &self.state.pairs, capacity) {
                return Err(LinkError {
                    function: Some(self.symbols.instance(*id)),
                    refusal: None,
                    what: "given a tier-up counter pair by the lazy placer while this was staged, which gave counter \
                           pairs that clash with it: stage it again"
                        .into(),
                });
            }
        }
        Ok(())
    }

    /// Makes `staged` the image at once, its file's bytes written into `memory`'s image part at
    /// their file offsets as [`Image::commit`] would write the file, each range published: a
    /// dev run's executable, written once from the memory and overlaid by it
    /// ([`blaze_inproc::CodeMemory`]). The image's first commit is into a new memory; each
    /// later one into the memory that holds its last, which keeps zeros past the file's end.
    /// The history is [`Image::commit`]'s, so [`Image::roll_back_into`] undoes it. Gives the
    /// encoded message that takes programs running the memory to the new image: its code is
    /// in the memory, the message names it by address, length and hash (`None` for the
    /// image's first commit, which no program runs yet, and without [`Output::live`]).
    pub fn commit_into(
        &mut self,
        mut staged: Staged,
        memory: &mut blaze_inproc::CodeMemory,
    ) -> Result<Option<Vec<u8>>, LinkError> {
        self.can_commit(&staged)?;
        self.carry_placed_pairs(&mut staged.state)?;
        let cells = self.cells_in(memory)?;
        self.write_into(&staged.writes, memory)?;
        let message = staged.writes.message;
        self.take(staged.state, staged.writes.full, staged.undo);
        self.placed_pairs.clear();
        self.cells = Some(cells);
        Ok(message)
    }

    /// The offset of `memory`'s cells part, which holds every counter pair the dev run's cells
    /// do ([`DevRun::cells`]).
    fn cells_in(&self, memory: &blaze_inproc::CodeMemory) -> Result<u64, LinkError> {
        let cells = memory.cells();
        let asked = self.output.dev_run.map_or(0, |it| it.cells);
        if cells.len < asked {
            return Err(LinkError {
                function: None,
                refusal: None,
                // alloc: error path
                what: format!(
                    "{}: its dev run has {asked:#x} bytes of counter cells, and its code memory's cells part {:#x}: \
                     make the code memory with the dev run's cells",
                    self.to, cells.len
                ),
            });
        }
        Ok(cells.offset)
    }

    /// The offset of `key`'s tier-up counter pair ([`counter`]) in the code memory the image
    /// was last committed into ([`Image::commit_into`]): the memory's cells part's offset
    /// ([`blaze_inproc::CodeMemory::cells`]) plus 16 bytes for each pair given before it. That
    /// is the offset [`blaze_inproc::CellView::load`] and `blaze_stencil::tier::Counters::bind`
    /// take: the calls word there, the back edges word 8 after. A run process has the pair at
    /// the same 16 bytes per pair from the start of the image's cells reservation, where the
    /// function's code reaches it. `None` for a key given no pair, and before a commit into a
    /// code memory. A pair stays its key's after the function is removed or tiered up.
    pub fn counter(&self, key: &InstanceKey) -> Option<u64> {
        let pair = self.state.pairs.of(self.symbols.find_id(&key.0)?)?;
        Some(self.cells? + counter::offset(pair))
    }

    /// Back to the image before the last commit: a patch that panicked, or one whose borrow
    /// check, run beside it, refused its body. Each call undoes one more commit, so a refusal
    /// that comes in after later commits rolls them back too; their changes are then staged
    /// again without the refused body. A commit in flight must complete first. The rollback
    /// of a [`Output::live`] image is refused before anything changes, as [`Image::commit`]
    /// of one is: running programs switch back only through the code memory
    /// ([`Image::roll_back_into`]).
    pub fn roll_back(&mut self, effects: &mut Vec<Effect>) -> Result<(), LinkError> {
        let writes = self.roll_back_writes()?;
        let steps = self.steps(writes)?;
        let revert = self.take_undo_keeping_pairs();
        self.begin(steps, revert, effects);
        Ok(())
    }

    /// [`Image::take_undo`], the counter pairs given since kept: code given one may still be
    /// running and counting in it, so no other key gets it.
    fn take_undo_keeping_pairs(&mut self) -> commit::Revert {
        // clone: a reference count per shard of the column
        let pairs = self.state.pairs.clone();
        let revert = self.take_undo();
        self.state.pairs = pairs;
        revert
    }

    /// [`Image::roll_back`] into the code memory the image is committed into
    /// ([`Image::commit_into`]): the undo's bytes written and published there, the image back
    /// at once. Gives the encoded message that switches programs running the memory back to
    /// its table (`None` without [`Output::live`]).
    pub fn roll_back_into(&mut self, memory: &mut blaze_inproc::CodeMemory) -> Result<Option<Vec<u8>>, LinkError> {
        let writes = self.roll_back_writes()?;
        let cells = self.cells_in(memory)?;
        self.write_into(&writes, memory)?;
        self.take_undo_keeping_pairs();
        self.cells = Some(cells);
        Ok(writes.message)
    }

    /// Makes `staged` the image at once, as [`Image::commit`] does (the same checks and
    /// history, so [`Image::roll_back`] undoes it), and gives the whole file's bytes instead
    /// of writing it: for a caller that puts the file in its store. A commit that would tell
    /// running programs (a later link of a [`Output::live`] image) is refused, since its
    /// patches reach them only through the code memory ([`Image::commit_into`]).
    pub fn bytes(&mut self, mut staged: Staged) -> Result<Vec<u8>, LinkError> {
        self.can_commit(&staged)?;
        self.carry_placed_pairs(&mut staged.state)?;
        if staged.writes.message.is_some() {
            return Err(LinkError {
                function: None,
                refusal: None,
                // alloc: error path
                what: format!(
                    "{}: a live image's patches reach running programs only through the code memory: commit it with \
                     `commit_into`",
                    self.to
                ),
            });
        }
        let runs = if staged.writes.full { staged.writes.writes } else { file::whole(&staged.state) };
        // alloc: the file, which the caller owns
        let mut out = vec![0; staged.writes.len as usize];
        for (at, bytes) in &runs {
            let at = *at as usize;
            out[at..at + bytes.len()].copy_from_slice(bytes);
        }
        self.take(staged.state, staged.writes.full, staged.undo);
        self.placed_pairs.clear();
        Ok(out)
    }

    /// Each table the image keeps as `(name, elements, bytes)`, for `blaze inspect tables`
    /// (`blaze_api::Response::Tables`): `relocation` and the rest.
    pub fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        self.state.tables()
    }

    /// How many jump-table slots the image has given out: every [`Image::slot`] index is below it.
    pub fn slots(&self) -> u32 {
        self.state.slots
    }

    pub fn slot(&self, function: &InstanceKey) -> Option<Slot> {
        let sym = self.symbols.find_slot(function)?;
        let (piece, offset) = self.state.home(sym)?;
        let piece = self.state.piece(piece)?;
        if !matches!(self.symbols.key(piece.sym), SymKey::Stubs) {
            return None;
        }
        Some(Slot((offset / table::stub_size(self.output.arch)) as u32))
    }

    /// The generation the image is at: counts commits, never reused.
    pub fn generation(&self) -> u64 {
        self.state.generation
    }
}

/// The further inputs a native input names (a linker script's `INPUT`, `GROUP` objects and
/// archives such as `libc_nonshared.a`), which the caller reads and passes as natives too.
pub fn script_inputs(output: &Output, native: &Native<'_>) -> Result<Vec<String>, LinkError> {
    let writer = model::writer(output)?;
    let file = writer
        .read(native.name, native.bytes, &native.kind)
        // alloc: error path
        .map_err(|what| LinkError { function: None, what: format!("{}: {what}", native.name), refusal: None })?;
    // alloc: none, an empty `Vec` holds no buffer
    let NativeFile::Shared(shared) = file else { return Ok(Vec::new()) };
    let mut out = Vec::with_capacity(shared.inputs.len());
    let mut all = smallvec::SmallVec::<[model::Shared; 2]>::new();
    all.push(shared);
    while let Some(it) = all.pop() {
        out.extend(it.inputs);
        all.extend(it.more);
    }
    Ok(out)
}

/// Reads `bytes` as a native input of `format` (for a fuzz target): whether it read. A
/// reader returns an error, never panics, on any bytes.
#[cfg(feature = "fuzz")]
pub fn fuzz_read(format: Format, arch: blaze_codegen::Arch, kind: NativeKind, bytes: &[u8]) -> bool {
    let output = Output {
        format,
        arch,
        // alloc: a fuzz target's setup, once per input
        kind: OutputKind::Executable { entry: "_start".to_owned() },
        interpreter: None,
        platform: (format == Format::MachO).then_some(Platform { platform: 1, minimum: 0x000b_0000, sdk: 0x000b_0000 }),
        incremental: false,
        debug: Debug::None,
        live: None,
        tags: 0,
        stack: None,
        windows_subsystem: false,
        dev_run: None,
        exec_log: false,
        sanitize: blaze_codegen::settings::Sanitize::default(),
        // alloc: a fuzz target's setup, once per input
        rpaths: Vec::new(),
        triple: match (format, arch) { (Format::Elf, blaze_codegen::Arch::Aarch64) => "aarch64-unknown-linux-gnu", (Format::Elf, _) => "x86_64-unknown-linux-gnu", (Format::MachO, blaze_codegen::Arch::X86_64) => "x86_64-apple-darwin", (Format::MachO, _) => "aarch64-apple-darwin", (Format::Pe, _) => "x86_64-pc-windows-msvc", (Format::Wasm, _) => "wasm32-unknown-unknown" }.into(),
    };
    model::writer(&output).is_ok_and(|writer| writer.read("fuzz", bytes, &kind).is_ok())
}

/// `macho::tbd::read("t.tbd", text, arch, platform)` against the reference reader: `Ok` with
/// whether it accepted when both give the same symbols, inputs and install name or the same
/// error (or the reference refuses nesting past 256), and the arena is exact
/// (`arena_is_exact`); `Err` naming both answers otherwise.
#[cfg(feature = "fuzz")]
#[doc(hidden)]
pub fn fuzz_tbd_differential(text: &str, arch: blaze_codegen::Arch, platform: u32) -> Result<bool, String> {
    macho::tbd_differential(text, arch, platform)
}

/// `trie_exports(trie, 0, sections)` against the reference walk, `sections` being
/// `dylib::tests::test_sections`: `Ok` with whether it accepted when the exports or the errors
/// are equal, `Err` naming both answers otherwise.
#[cfg(feature = "fuzz")]
#[doc(hidden)]
pub fn fuzz_trie_differential(trie: &[u8]) -> Result<bool, String> {
    macho::trie_differential(trie)
}

/// The address space a dev run's reservations may take in all: what a process has below
/// its 48-bit (x86-64, AArch64) top half.
const ADDRESS_SPACE: u64 = 1 << 47;

/// Why `output` can't be the dev run `dev_run` asks for: a shared image (only an executable
/// has a start hook), a format with none, or reservations past a process's address space.
fn dev_run_fits(output: &Output, dev_run: &DevRun, plan: &Plan, writer: &Formats) -> Result<(), String> {
    if let OutputKind::Shared { name, .. } = &output.kind {
        // alloc: error path
        return Err(format!("a dev run of the shared image {name}: only an executable starts in the dev runtime"));
    }
    writer.start_hook()?;
    let mut total: u64 = 0;
    for (what, size) in [("arena", dev_run.arena), ("cells", dev_run.cells), ("fixed maps", dev_run.fixed)] {
        match blaze_mem::align::checked_align_up_u64(size, plan.page).and_then(|it| total.checked_add(it)) {
            Some(sum) if sum <= ADDRESS_SPACE => total = sum,
            _ => {
                // alloc: error path
                return Err(format!(
                    "the dev run's reservations, with {size:#x} bytes for the {what}, take more than a process's \
                     {ADDRESS_SPACE:#x} bytes of address space"
                ));
            }
        }
    }
    Ok(())
}

/// What an image is across its generations: its first full link's pieces and inputs. Once
/// per image.
fn lineage(stage: &Stage<'_>, to: &FileName) -> [u8; 16] {
    let pieces = stage.new.piece_count() as usize;
    let mut bytes: Vec<u8> = Vec::with_capacity(pieces * 40);
    let mut keys: Vec<(u32, u32)> = Vec::with_capacity(pieces);
    for piece in stage.new.live_pieces() {
        let start = bytes.len();
        stage.symbols.write_key(piece.sym, &mut bytes);
        keys.push((start as u32, (bytes.len() - start) as u32));
    }
    let key = |it: &(u32, u32)| &bytes[it.0 as usize..(it.0 + it.1) as usize];
    let order = lineage_order(&keys, key);
    let mut hasher = Hasher::new("blaze v2 link lineage 2");
    // alloc: once per image, at its first full link
    hasher.part(to.as_str().as_bytes()).part(format!("{:?}", stage.output).as_bytes());
    for (_, at) in &order {
        hasher.part(key(&keys[*at as usize]));
    }
    for input in stage.new.inputs.iter() {
        hasher.id(&input.id);
    }
    let id = hasher.finish();
    let mut out = [0; 16];
    out.copy_from_slice(&id.0[..16]);
    out
}

/// `items` in the order of their byte strings `key(item)`, as `(prefix, index)`: a radix sort
/// by each key's first 8 bytes, big-endian and zero-padded, then a comparison of whole keys
/// only within a run of one prefix. A smaller prefix is a smaller key, so this is the order
/// a comparison sort of the keys gives, at a radix pass's cost when the prefixes differ.
fn lineage_order<'k, T>(items: &'k [T], key: impl Fn(&'k T) -> &'k [u8]) -> Vec<(u64, u32)> {
    let prefix = |bytes: &[u8]| {
        let mut word = [0u8; 8];
        let len = bytes.len().min(8);
        word[..len].copy_from_slice(&bytes[..len]);
        u64::from_be_bytes(word)
    };
    // alloc: once per image, at its first full link
    let mut order: Vec<(u64, u32)> = items.iter().enumerate().map(|(at, it)| (prefix(key(it)), at as u32)).collect();
    blaze_ids::radix::sort_pairs(&mut order);
    let mut from = 0;
    while from < order.len() {
        let to = from + order[from..].iter().take_while(|it| it.0 == order[from].0).count();
        if to - from > 1 {
            order[from..to].sort_unstable_by(|a, b| key(&items[a.1 as usize]).cmp(key(&items[b.1 as usize])));
        }
        from = to;
    }
    order
}
