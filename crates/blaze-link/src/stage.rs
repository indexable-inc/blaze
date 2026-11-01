//! A stage: a [`Change`] applied to a copy of the image's state, placed and relocated.
//!
//! The order is fixed: what is removed goes first, then what is added is made into pieces
//! (their symbols interned in one batch, shard by shard in parallel), the native inputs and
//! every name are resolved ([`crate::native`]), GOT cells and PLT stubs made for what needs
//! them, the shared pieces counted and the unused swept, everything placed
//! ([`crate::place`]), and every piece that is new or refers to something that changed
//! relocated, in parallel ([`crate::write`]).
//!
//! What a stage costs is what it touches: the sets below are bitsets over piece, slot and
//! symbol ids, the bytes of what it makes are borrowed from the change or allocated in its
//! arena (freed when the stage ends), and nothing walks every piece except a full link.

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_codegen::{
    Arch, CompiledFunction, Covered, Data, Init, InstanceKey, Relocs, Static, Target, Unwind,
};
use blaze_format::cursor::{self, Reader};
use blaze_graph::par;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::{Hasher, radix};
use blaze_live::patch::exec;
use blaze_mem::Frame;

use crate::arena::{FrameVec, collect_in};
use crate::cow::{IdSet, SymSet};
use crate::model::{Apply, Class, Fixup, Formats, Plan, Special, Sym, Writer};
use crate::position::{self, Spans};
use crate::reloc::{self, GotKind};
use crate::state::{ClassId, Def, NewPiece, PieceId, PieceRef, State, What};
use crate::symbols::{IdNo, NameId, Obj, SymKey, Symbols, Wide};
use crate::table;
use crate::unwind::{self, Field};
use crate::{Change, Format, LinkError, Output, Refusal};

const CIE_DOMAIN: &str = "blaze v2 link cie 1";

/// The relocations of a zeroed static.
static NO_RELOCS: Relocs = Relocs::new();

/// SanitizerCoverage's sections as LLVM's `getSectionName` names them in each format: the
/// counters, flags and guards (one array per covered function each), then the PC table.
pub(crate) const SANCOV_ELF: [&str; 4] = ["__sancov_cntrs", "__sancov_bools", "__sancov_guards", "__sancov_pcs"];
pub(crate) const SANCOV_MACHO: [&str; 4] =
    ["__DATA,__sancov_cntrs", "__DATA,__sancov_bools", "__DATA,__sancov_guards", "__DATA,__sancov_pcs"];
pub(crate) const SANCOV_COFF: [&str; 4] = [".SCOV$CM", ".SCOV$BM", ".SCOV$GM", ".SCOVP$M"];
/// [`Stage::sancov`]'s index of the PC table, after the arrays.
const SANCOV_PCS: usize = Covered::ARRAYS as usize;

/// The dev runtime's start hook (`blaze_inproc::runtime`), which a dev run's first initializer
/// points at.
pub(crate) const START_HOOK: &str = "blaze_inproc_start";

/// What a stage made or rewrote a piece as: bytes borrowed from the change or the stage's
/// arena, or zeros (a GOT cell, the cells array, a descriptor: nothing allocated).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Content<'a> {
    Bytes(&'a [u8]),
    Zero,
}

impl<'a> Content<'a> {
    /// Fills `out` (the piece's size): the bytes, zeros after them.
    pub fn copy_to(self, out: &mut [u8]) {
        match self {
            Content::Bytes(bytes) => {
                let n = bytes.len().min(out.len());
                out[..n].copy_from_slice(&bytes[..n]);
                out[n..].fill(0);
            }
            Content::Zero => out.fill(0),
        }
    }

    pub fn bytes(self) -> Option<&'a [u8]> {
        match self {
            Content::Bytes(bytes) => Some(bytes),
            Content::Zero => None,
        }
    }
}

/// The contents of the pieces a stage made (dense, by id from the first it made) or
/// rewrote (a few: positions, the descriptor, pieces a relayout moved).
pub(crate) struct Contents<'a> {
    first: PieceId,
    fresh: FrameVec<'a, Option<Content<'a>>>,
    other: HashMap<PieceId, Content<'a>>,
}

impl<'a> Contents<'a> {
    pub fn new(first: PieceId, arena: &'a Frame) -> Contents<'a> {
        Contents { first, fresh: Vec::new_in(arena), other: HashMap::default() }
    }

    #[inline]
    pub fn get(&self, id: PieceId) -> Option<Content<'a>> {
        match id.checked_sub(self.first) {
            Some(at) => self.fresh.get(at as usize).copied().flatten(),
            None => self.other.get(&id).copied(),
        }
    }

    pub fn insert(&mut self, id: PieceId, content: Content<'a>) {
        match id.checked_sub(self.first) {
            Some(at) => {
                let at = at as usize;
                if self.fresh.len() <= at {
                    self.fresh.resize(at + 1, None);
                }
                self.fresh[at] = Some(content);
            }
            None => {
                self.other.insert(id, content);
            }
        }
    }

    pub fn remove(&mut self, id: PieceId) {
        match id.checked_sub(self.first) {
            Some(at) => {
                if let Some(it) = self.fresh.get_mut(at as usize) {
                    *it = None;
                }
            }
            None => {
                self.other.remove(&id);
            }
        }
    }

    pub fn contains(&self, id: PieceId) -> bool {
        self.get(id).is_some()
    }
}

pub(crate) struct Stage<'a> {
    pub output: &'a Output,
    pub writer: &'a Formats,
    pub plan: &'a Plan,
    pub symbols: &'a mut Symbols,
    pub objects: &'a mut HashMap<Obj, crate::native::Read>,
    /// Each native input's bytes, borrowed for the stage ([`crate::native`]).
    pub native: crate::native::Pass<'a>,
    pub old: &'a State,
    pub new: State,
    /// The image's file name.
    pub name: &'a str,
    /// The stage's scratch: freed, all at once, when the stage ends.
    pub arena: &'a Frame,
    /// The contents of pieces this stage made or rewrote.
    pub contents: Contents<'a>,
    /// Pieces this stage made, still live.
    pub fresh: IdSet<&'a Frame>,
    /// Pieces to relocate (with [`Stage::contents`] or their bytes where they were).
    pub write: IdSet<&'a Frame>,
    /// Symbols whose definition changed: what refers to them is relocated again.
    pub changed: SymSet<&'a Frame>,
    /// Slots whose cell is written: put in this stage, or running a body that moved.
    pub dirty_cells: IdSet<&'a Frame>,
    /// Pieces removed this stage: [`State::any`] still reads them.
    pub removed: FrameVec<'a, PieceId>,
    /// Slots whose function was removed: [`Stage::dangling`] fails if one is still empty
    /// and its slot still called.
    pub emptied: FrameVec<'a, u32>,
    /// Shared pieces that lost a user outside [`Stage::removed`], and symbols that gained
    /// one outside [`Stage::fresh`]: [`Stage::sweep`] counts them.
    pub touched: FrameVec<'a, PieceId>,
    pub counted: FrameVec<'a, Sym>,
    /// Whether everything is placed anew.
    pub full: bool,
    pub table: bool,
    pub functions: u64,
    pub statics: u64,
    /// The classes [`Writer::own`] gave, asked once per stage.
    pub owned: FrameVec<'a, (Class, ClassId, u32)>,
    /// Bytes reused for keys and frame records written out, kept by the image across stages.
    pub scratch: &'a mut Vec<u8>,
    /// Whether [`Stage::spans`] replaced any span table.
    pub spans_changed: bool,
    /// The class and rank of each SanitizerCoverage section, asked of the writer once per
    /// stage: the arrays by [`blaze_codegen::Target::Coverage`]'s index, then the PC table.
    pub sancov: [Option<(ClassId, u32)>; SANCOV_PCS + 1],
    /// Where the zero length ending the call frame entries this patch placed in
    /// [`Class::Patch`] is ([`crate::place`]).
    pub frames_end: Option<u64>,
    /// A lazy image's session ([`crate::lazy`]).
    pub lazy: Option<&'a crate::lazy::Lazy>,
}

pub(crate) fn error(function: Option<InstanceKey>, what: impl Into<String>) -> LinkError {
    LinkError { function, what: what.into(), refusal: None }
}

/// An incremental image's refusal of a stage its layout has no room for ([`Refusal`]).
pub(crate) fn refused(function: Option<InstanceKey>, refusal: Refusal, what: impl Into<String>) -> LinkError {
    LinkError { function, what: what.into(), refusal: Some(refusal) }
}

/// The key a codegen target is, and whether it is referred to weakly. `of` is the function
/// whose code or data refers to it: a coverage array or a counter pair is that function's
/// own, and outside a function (`None`) there is none to name. Every counter pair is an
/// offset of [`SymKey::Counters`], which [`Stage::fixups`] adds to the addend.
fn wide<'t>(target: &'t Target, table: bool, of: Option<&'t InstanceKey>) -> Option<(Wide<'t>, bool)> {
    Some(match target {
        Target::Function(key) if table => (Wide::Slot(key), false),
        Target::Function(key) | Target::Direct(key) => (Wide::Body(key), false),
        Target::Static { item, instance } => (Wide::Static(item, instance), false),
        Target::Data(id) => (Wide::Data(id), false),
        Target::Extern(name) => (Wide::Name(name), false),
        Target::Weak(name) => (Wide::Name(name), true),
        Target::Position(span) => (Wide::Position(span), false),
        Target::Coverage(array) => (Wide::Coverage(of?, *array), false),
        Target::Counter(_) => {
            of?;
            (Wide::Key(SymKey::Counters), false)
        }
        Target::ProfileCounters => return None,
    })
}

/// The keys of a function's symbols, in the order [`Stage::add_function`] reads them: its
/// body, its relocations' targets (each once, by index), then each datum and its
/// relocations' targets.
fn function_keys<'f>(function: &'f CompiledFunction, table: bool, out: &mut [Wide<'f>]) {
    let mut at = 0;
    let mut put = |key: Wide<'f>| {
        out[at] = key;
        at += 1;
    };
    let of = Some(&function.key);
    put(Wide::Body(&function.key));
    for target in function.relocs.targets() {
        put(wide(target, table, of).expect("a function's own target").0);
    }
    for data in &function.data {
        put(Wide::Data(&data.id));
        for target in data.relocs.targets() {
            put(wide(target, table, of).expect("a function's own target").0);
        }
    }
}

fn key_count(function: &CompiledFunction) -> usize {
    1 + function.relocs.targets().len()
        + function.data.iter().map(|it| 1 + it.relocs.targets().len()).sum::<usize>()
}

impl<'a> Stage<'a> {
    pub fn sym(&mut self, key: SymKey) -> Sym {
        self.symbols.intern(key)
    }

    pub fn sym_name(&mut self, name: &str) -> Sym {
        self.symbols.sym_name(name)
    }

    /// The symbol of a target outside a function's code and data: a static's, a name's, a
    /// personality routine's.
    fn target_sym(&mut self, target: &Target) -> Result<Sym, LinkError> {
        let Some((key, _)) = wide(target, self.table, None) else {
            return Err(match target {
                Target::Counter(key) => error(
                    Some(*key),
                    "its tier-up counter cells named outside its own code: nothing but a function's code names any",
                ),
                Target::ProfileCounters => error(None, "PGO counters named outside the instrumented function's own code"),
                _ => error(None, "a SanitizerCoverage array named outside the function it is of"),
            });
        };
        let key = match key {
            Wide::Key(key) => key,
            Wide::Name(name) => SymKey::Name(self.symbols.name(name)),
            Wide::Body(key) => SymKey::Body(self.symbols.id(&key.0)),
            Wide::Slot(key) => SymKey::Slot(self.symbols.id(&key.0)),
            Wide::Data(id) => SymKey::Data(self.symbols.id(id)),
            Wide::Static(item, instance) => SymKey::Static(self.symbols.id(&item.0), self.symbols.id(instance)),
            Wide::Position(span) => SymKey::Position(self.symbols.id(&span.anchor), span.index),
            Wide::Coverage(key, array) => SymKey::Coverage(self.symbols.id(&key.0), array),
        };
        Ok(self.symbols.intern(key))
    }

    /// The symbols of a table's targets, by target index, in the stage's arena.
    fn reloc_syms(&mut self, relocs: &Relocs) -> Result<&'a [Sym], LinkError> {
        let mut out = Vec::with_capacity_in(relocs.targets().len(), self.arena);
        for target in relocs.targets() {
            out.push(self.target_sym(target)?);
        }
        Ok(out.leak())
    }

    /// Defines `sym`, noting a change.
    pub fn define(&mut self, sym: Sym, def: Def) {
        if self.new.def(sym) != def {
            self.changed.insert(sym);
            self.new.set_def(sym, def);
        }
    }

    /// Adds a piece with its contents, defining its own symbol at its start.
    pub fn make(&mut self, piece: &NewPiece<'_>, content: Content<'a>) -> PieceId {
        let id = self.new.add(piece);
        self.contents.insert(id, content);
        self.fresh.insert(id);
        self.write.insert(id);
        self.define(piece.sym, Def::Piece { piece: id, offset: 0 });
        id
    }

    /// Removes a piece; [`Stage::sweep`] counts what it referred to.
    pub fn unmake(&mut self, id: PieceId) {
        if !self.new.remove(id) {
            return;
        }
        let piece = self.new.any(id).expect("just removed");
        self.changed.insert(piece.sym);
        for sym in piece.defines() {
            self.changed.insert(*sym);
        }
        self.fresh.remove(id);
        self.write.remove(id);
        self.contents.remove(id);
        self.removed.push(id);
    }

    /// A piece's fields, with rank 0, the order of codegen's pieces and not zeroed.
    pub fn piece<'x>(
        &mut self,
        sym: Sym,
        class: &Class,
        align: u64,
        size: u64,
        fixups: &'x [Fixup],
        what: What,
    ) -> NewPiece<'x> {
        let class = self.new.class_id(class);
        Self::piece_in(sym, class, align, size, fixups, what)
    }

    fn piece_in<'x>(sym: Sym, class: ClassId, align: u64, size: u64, fixups: &'x [Fixup], what: What) -> NewPiece<'x> {
        NewPiece {
            sym,
            class,
            rank: 0,
            order: [u32::MAX; 3],
            align: align.max(1),
            size,
            zero: false,
            fixups,
            weak: &[],
            defines: &[],
            what,
        }
    }

    /// The class and rank the writer gives codegen's pieces of `class`.
    fn own(&mut self, class: Class) -> (ClassId, u32) {
        if let Some((_, id, rank)) = self.owned.iter().find(|it| it.0 == class) {
            return (*id, *rank);
        }
        let (to, rank) = self.writer.own(&class);
        let id = self.new.class_id(&to);
        self.owned.push((class, id, rank));
        (id, rank)
    }

    pub fn arch(&self) -> Arch {
        self.output.arch
    }

    fn fixup_slice(&self, fixups: impl IntoIterator<Item = Fixup>) -> &'a [Fixup] {
        collect_in(fixups, self.arena).leak()
    }

    /// Applies everything `change` says to the copy of the state.
    pub fn apply(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        self.spans(change)?;
        if self.table && !self.new.laid_out {
            let lazy = self.lazy.map_or(0, |it| it.keys.len() as u64);
            self.fixed(change.added.len() as u64 + lazy)?;
            self.lazy_slots()?;
        }
        for key in &change.removed {
            if let Some(sym) = self.symbols.find_body(key) {
                if self.is_lazy(sym) {
                    return Err(error(Some(*key), "removed, and it is lazy: the image keeps every lazy function's slot"));
                }
                if let Some((piece, _)) = self.new.home(sym) {
                    self.remove_function(piece);
                }
            }
        }
        for (item, instance) in &change.removed_statics {
            let (Some(item), Some(instance)) = (self.symbols.find_id(&item.0), self.symbols.find_id(instance)) else {
                continue;
            };
            for key in [SymKey::Static(item, instance), SymKey::Tlv(item, instance)] {
                if let Some(sym) = self.symbols.find(key) {
                    if let Def::Piece { piece, .. } = self.new.def(sym) {
                        self.unmake(piece);
                    }
                }
            }
        }
        for (key, name) in &change.debug_names {
            let (id, name) = (self.symbols.id(&key.0), self.symbols.name(name));
            self.new.set_debug_name(id, name);
        }
        self.functions(&change.added)?;
        self.slots(&change.slots)?;
        for it in &change.statics {
            self.add_static(it)?;
        }
        let mut named = Vec::with_capacity_in(change.names.len(), self.arena);
        for (name, target) in &change.names {
            let name = self.symbols.name(name);
            let sym = self.target_sym(target)?;
            named.push((name, sym));
        }
        let symbols = &*self.symbols;
        let names = &self.new.names;
        let differs = named.iter().any(|(name, sym)| names.get(name) != Some(sym))
            || change.unnamed.iter().any(|it| symbols.find_name(it).is_some_and(|it| names.contains_key(&it)));
        if differs {
            let names = Arc::make_mut(&mut self.new.names);
            for name in &change.unnamed {
                if let Some(name) = symbols.find_name(name) {
                    names.remove(&name);
                }
            }
            for (name, sym) in named.iter() {
                names.insert(*name, *sym);
            }
        }
        self.positions()?;
        let mut seen: FrameVec<'a, Sym> = Vec::new_in(self.arena);
        for id in self.fresh.iter() {
            let piece = self.new.piece(id).expect("a fresh piece");
            seen.extend(
                piece.fixups().iter().map(|it| it.target).filter(|it| matches!(self.symbols.key(*it), SymKey::Name(_))),
            );
        }
        for (name, _) in named {
            seen.push(self.sym(SymKey::Name(name)));
        }
        for sym in seen {
            self.new.see(sym);
        }
        Ok(())
    }

    /// The live descriptor, filled once the image is placed.
    pub fn descriptor(&mut self) {
        if self.output.live.is_none() {
            return;
        }
        let descriptor = self.sym(SymKey::Descriptor);
        if self.new.home(descriptor).is_none() {
            let size = blaze_live::patch::Descriptor::SIZE as u64;
            let piece = self.piece(descriptor, &Class::Rodata, 8, size, &[], What::Fixed);
            self.make(&piece, Content::Zero);
        }
    }

    /// A dev run's pointer to [`START_HOOK`] ([`crate::DevRun`]), first among the pieces of the
    /// class and rank the writer gives it: made once, kept by every later stage. Its fixup is a
    /// reference of the core's, so the name is resolved, and the native section defining it
    /// kept, as any strong reference is.
    pub fn start_hook(&mut self) -> Result<(), LinkError> {
        if self.output.dev_run.is_none() {
            return Ok(());
        }
        let sym = self.sym(SymKey::StartHook);
        if self.new.home(sym).is_some() {
            return Ok(());
        }
        let (class, rank) = self.writer.start_hook().map_err(|what| error(None, what))?;
        let target = self.sym_name(START_HOOK);
        let fixups = self.fixup_slice([Fixup { at: 0, apply: Apply::Abs64, target, addend: 0, minus: None }]);
        let mut piece = self.piece(sym, &class, 8, 8, fixups, What::Fixed);
        piece.rank = rank;
        piece.order = [0; 3];
        self.make(&piece, Content::Zero);
        self.new.see(target);
        Ok(())
    }

    /// The jump table's header (its cell, [`blaze_live::patch::exec`]), cells and stubs, with
    /// room for `functions` and what incremental links add; with [`Output::exec_log`], the
    /// header's log table and private map and the log stubs after the slot stubs too.
    fn fixed(&mut self, functions: u64) -> Result<(), LinkError> {
        let capacity = table::capacity(functions, true);
        let capacity = u32::try_from(capacity).map_err(|_| error(None, "a jump table past 2^32 slots"))?;
        self.new.capacity = capacity;
        let slots = u64::from(capacity);
        let arch = self.arch();
        let logged = self.output.exec_log;
        if logged && arch == Arch::Aarch64 && slots > table::A64_LOG_SLOTS {
            return Err(error(
                None,
                // alloc: error path
                format!(
                    "a jump table of {slots} slots with an execution log, where AArch64's log stubs reach {}",
                    table::A64_LOG_SLOTS
                ),
            ));
        }
        let cells = self.sym(SymKey::Cells);
        let generation = self.sym(SymKey::Generation);
        let stubs = self.sym(SymKey::Stubs);
        let size = table::stub_size(arch);
        let log_stubs = (slots * size) as i64;
        let stride = table::log_stub_size(arch);
        let abs = |at: u64, target: Sym, addend: i64| Fixup { at, apply: Apply::Abs64, target, addend, minus: None };
        let (cell, cell_addend) = if logged { (generation, exec::HEADER as i64) } else { (cells, 0) };
        let always = [abs(exec::CELL, cell, cell_addend), abs(exec::CURRENT, cells, 0), abs(exec::LOADER, cells, 0)];
        let log = [
            abs(exec::MAP, generation, exec::private_map(slots) as i64),
            abs(exec::STUBS, stubs, log_stubs),
        ];
        let words = (0..if logged { slots } else { 0 })
            .map(|slot| abs(exec::log_word(slot), stubs, log_stubs + (slot * stride) as i64));
        let fixups = self.fixup_slice(always.into_iter().chain(log.into_iter().filter(|_| logged)).chain(words));
        let header: &'a mut [u8] = self.arena.alloc_slice_fill_with(exec::HEADER as usize, |_| 0);
        header[exec::SLOTS as usize..][..4].copy_from_slice(&capacity.to_le_bytes());
        let stride_field = if logged { stride as u32 } else { 0 };
        header[exec::STRIDE as usize..][..4].copy_from_slice(&stride_field.to_le_bytes());
        let piece = self.piece(generation, &Class::Table, 8, exec::len(slots, logged), fixups, What::Fixed);
        self.make(&piece, Content::Bytes(header));
        let mut piece = self.piece(cells, &Class::Table, 8, slots * 8, &[], What::Cells);
        piece.rank = 1;
        self.make(&piece, Content::Zero);
        let log_len = if logged { table::log_size(arch, slots) } else { 0 };
        let code: &'a mut [u8] = self.arena.alloc_slice_fill_with((slots * size + log_len) as usize, |_| 0);
        let (slot_code, log_code) = code.split_at_mut((slots * size) as usize);
        table::stubs(arch, slot_code);
        if logged {
            table::log_stubs(arch, slots, log_code);
        }
        let stub = table::stub_fixups(arch);
        let routine = log_stubs as u64 + table::log_routine(arch, slots);
        let routine_fixups = table::log_fixups(arch).iter().filter(|_| logged).map(move |it| Fixup {
            at: routine + it.at,
            apply: it.apply,
            target: generation,
            addend: it.addend,
            minus: None,
        });
        let fixups = self.fixup_slice(
            (0..slots)
                .flat_map(|slot| {
                    stub.iter().map(move |it| Fixup {
                        at: slot * size + it.at,
                        apply: it.apply,
                        target: generation,
                        addend: it.addend,
                        minus: None,
                    })
                })
                .chain(routine_fixups),
        );
        let piece = self.piece(stubs, &Class::Stubs, 16, code.len() as u64, fixups, What::Fixed);
        self.make(&piece, Content::Bytes(code));
        Ok(())
    }

    fn remove_function(&mut self, piece: PieceId) {
        let Some((body, info)) = self.new.piece(piece).and_then(|it| Some((it.sym, *it.function()?))) else {
            return;
        };
        self.unmake(piece);
        // Its FDE, its UNWIND_INFO, its coverage arrays and its PC table entries.
        let mut owned = [None; SANCOV_PCS + 3];
        owned[0] = info.fde;
        owned[1] = info.windows.map(|it| it.0);
        for (kind, out) in owned[2..].iter_mut().enumerate() {
            let key = if kind == SANCOV_PCS { SymKey::CoveragePcs(info.id) } else { SymKey::Coverage(info.id, kind as u32) };
            *out = self.symbols.find(key);
        }
        for sym in owned.into_iter().flatten() {
            if let Def::Piece { piece, .. } = self.new.def(sym) {
                self.unmake(piece);
            }
        }
        if self.table {
            self.unslot(body);
        }
    }

    /// The jump table's cells piece.
    fn cells_piece(&self) -> Option<PieceId> {
        match self.new.def(self.symbols.find(SymKey::Cells)?) {
            Def::Piece { piece, .. } => Some(piece),
            _ => None,
        }
    }

    /// Points a slot's cell at a body.
    fn cell(&mut self, slot: Sym, body: Sym) {
        let Some((_, offset)) = self.new.home(slot) else { return };
        let index = (offset / table::stub_size(self.arch())) as u32;
        let Some(cells) = self.cells_piece() else { return };
        self.new.set_cell(index, Some(body));
        self.dirty_cells.insert(index);
        self.write.insert(cells);
    }

    /// Empties every cell holding `body`, which is gone: the cell keeps its bytes (a running
    /// program may still be in the old code) until a function is put in its slot. The cells
    /// piece is written again all the same: its rebase list is every filled slot's cell.
    fn unslot(&mut self, body: Sym) {
        let slots = collect_in(self.new.slots_of(body), self.arena);
        if !slots.is_empty() {
            if let Some(cells) = self.cells_piece() {
                self.write.insert(cells);
            }
        }
        for slot in slots {
            self.new.set_cell(slot, None);
            self.dirty_cells.remove(slot);
            self.emptied.push(slot);
        }
    }

    /// Puts each function in the slot `change` names for it.
    fn slots(&mut self, slots: &[(InstanceKey, InstanceKey)]) -> Result<(), LinkError> {
        if slots.is_empty() {
            return Ok(());
        }
        if !self.table {
            let (slot, _) = slots[0];
            return Err(error(
                Some(slot),
                "a function put in another's slot, in an image linked without a jump table",
            ));
        }
        for (name, function) in slots {
            if self.symbols.find_body(name).is_some_and(|it| self.is_lazy(it)) {
                return Err(error(
                    Some(*name),
                    "another function put in its slot, and it is lazy: the lazy placer replaces its code \
                     (`Placer::replace`)",
                ));
            }
            let body = self
                .symbols
                .find_body(function)
                .filter(|it| self.new.home(*it).is_some())
                .ok_or_else(|| {
                    // alloc: error path
                    error(Some(*function), format!("put in the slot of {name:?}, and neither added nor linked"))
                })?;
            let id = self.symbols.id(&name.0);
            let slot = self.sym(SymKey::Slot(id));
            if self.new.home(slot).is_none() {
                self.new_slot(slot, *name)?;
            }
            self.cell(slot, body);
        }
        Ok(())
    }

    /// The functions added, made into pieces: their keys built in parallel and interned in
    /// one batch. They are made in key order, never the change's: slots are given out in the
    /// order functions are made, and an image is the same whatever order its functions came in.
    fn functions(&mut self, added: &[&'a CompiledFunction]) -> Result<(), LinkError> {
        if added.is_empty() {
            return Ok(());
        }
        let arena = self.arena;
        let mut seen: HashSet<&InstanceKey> = HashSet::default();
        seen.reserve(added.len());
        let mut todo: FrameVec<'a, &'a CompiledFunction> = Vec::with_capacity_in(added.len(), arena);
        for function in added {
            if function.arch != self.arch() {
                return Err(error(
                    Some(function.key),
                    // alloc: error path
                    format!("compiled for {:?}, linked into an image for {:?}", function.arch, self.arch()),
                ));
            }
            if !seen.insert(&function.key) {
                return Err(error(Some(function.key), "added twice in one change"));
            }
            let profiled = function.profile.as_ref().is_some_and(|it| it.counted().is_some())
                || std::iter::once(&function.relocs)
                    .chain(function.data.iter().map(|it| &it.relocs))
                    .any(|relocs| relocs.targets().iter().any(|it| matches!(it, Target::ProfileCounters)));
            if profiled {
                return Err(error(
                    Some(function.key),
                    "instrumented for PGO counters (`-Cprofile-generate`): the link places no `__llvm_prf_*` sections yet",
                ));
            }
            let body = self.symbols.find_body(&function.key);
            if body.is_some_and(|it| self.is_lazy(it)) {
                return Err(error(Some(function.key), "added, and it is lazy: the lazy placer places its code"));
            }
            if body.and_then(|it| self.new.home(it)).is_some() {
                continue;
            }
            todo.push(function);
        }
        todo.sort_unstable_by_key(|it| it.key);
        let counts = collect_in(todo.iter().map(|it| key_count(it)), arena);
        let keys: &mut [Wide<'a>] = arena.alloc_slice_fill_with(counts.iter().sum(), |_| Wide::Key(SymKey::Generation));
        let mut parts: FrameVec<'_, &mut [Wide<'a>]> = Vec::with_capacity_in(todo.len(), arena);
        let mut rest = &mut keys[..];
        for count in counts.iter() {
            let (part, after) = std::mem::take(&mut rest).split_at_mut(*count);
            parts.push(part);
            rest = after;
        }
        let table = self.table;
        par::for_each(par::zip(&mut parts[..], &todo[..]), |(out, function)| function_keys(*function, table, &mut **out));
        drop(parts);
        let syms = self.symbols.intern_wide(keys);
        let mut at = 0;
        for (function, count) in todo.iter().zip(counts) {
            self.add_function(*function, &syms[at..at + count])?;
            at += count;
        }
        self.functions += todo.len() as u64;
        Ok(())
    }

    /// `relocs` as fixups, `syms` being the symbols of its targets by index
    /// ([`Stage::reloc_syms`]), and the weak ones among them. `counter` is the offset of the
    /// function's counter pair from [`SymKey::Counters`] ([`Stage::counter`]) when `relocs` are
    /// its code's and name it.
    fn fixups(
        &self,
        relocs: &Relocs,
        syms: &[Sym],
        function: Option<InstanceKey>,
        in_code: bool,
        counter: Option<u64>,
    ) -> Result<(&'a [Fixup], &'a [Sym]), LinkError> {
        let mut fixups = Vec::with_capacity_in(relocs.len(), self.arena);
        let mut weak = Vec::new_in(self.arena);
        for (target, sym) in relocs.targets().iter().zip(syms) {
            if matches!(target, Target::Weak(_)) {
                weak.push(*sym);
            }
        }
        for (row, reloc) in relocs.rows().iter().zip(relocs.iter()) {
            let apply = reloc::of_kind(reloc.kind, self.output.format, self.arch(), in_code).map_err(|it| error(function, it))?;
            let target = syms[row.target as usize];
            let addend = match reloc.target {
                Target::Counter(key) => {
                    let Some(at) = counter.filter(|_| in_code && function == Some(*key)) else {
                        return Err(error(
                            function.or(Some(*key)),
                            // alloc: error path
                            format!(
                                "the tier-up counter cells of {key:?} named in {}: only a function's own code names its cells",
                                if in_code { "another function's code" } else { "data" }
                            ),
                        ));
                    };
                    if !crate::counter::reaches(apply) {
                        return Err(error(
                            function,
                            // alloc: error path
                            format!(
                                "its tier-up counter cells reached by a {:?} relocation: cells are reached rip-relative \
                                 (x86-64) or by ADRP and a page offset (AArch64)",
                                reloc.kind
                            ),
                        ));
                    }
                    reloc.addend + at as i64
                }
                _ => reloc.addend,
            };
            fixups.push(Fixup { at: u64::from(reloc.at), apply, target, addend, minus: None });
        }
        radix::sort_copy_by_key(&mut weak, |it: &Sym| it.0);
        weak.dedup();
        Ok((fixups.leak(), weak.leak()))
    }

    /// The offset of `function`'s counter pair from [`SymKey::Counters`], given now if it has
    /// none ([`crate::counter::pair`]), when its code names its counter cells.
    fn counter(&mut self, function: &CompiledFunction, id: IdNo) -> Result<Option<u64>, LinkError> {
        if !function.relocs.targets().iter().any(|it| matches!(it, Target::Counter(_))) {
            return Ok(None);
        }
        let pair = crate::counter::pair(self.output, &mut self.new, id, &function.key)?;
        Ok(Some(crate::counter::offset(pair)))
    }

    /// The piece [`SymKey::Counters`] is defined in: no bytes, at the start of a dev run's
    /// cells reservation, which keeps its region's size 0 ([`crate::model::reserved`]); each
    /// pair is at its offset ([`crate::counter`]). Made once, kept by every later stage.
    pub fn counter_cells(&mut self) {
        if crate::counter::capacity(self.output) == 0 {
            return;
        }
        let sym = self.sym(SymKey::Counters);
        if self.new.home(sym).is_some() {
            return;
        }
        let class = Class::Reserve(crate::model::Reserve::Cells);
        let mut piece = self.piece(sym, &class, crate::counter::PAIR, 0, &[], What::Fixed);
        piece.zero = true;
        self.make(&piece, Content::Zero);
    }

    /// A lazy image's stub region and its lazy functions' slots ([`crate::lazy`]), at its first
    /// link, right after the table is made: the region one piece after the slot stubs, its hook
    /// cell a reference to [`blaze_lazy::hook::SYMBOL`] (its offset from the cell, which
    /// [`crate::LazyPlacer::new`] replaces with the hook's address); each lazy function's slot
    /// in plan order, its cell holding its entry, its body an alias of its slot stub.
    fn lazy_slots(&mut self) -> Result<(), LinkError> {
        let Some(lazy) = self.lazy else { return Ok(()) };
        let region = self.sym(SymKey::LazyStubs);
        let hook = self.sym_name(blaze_lazy::hook::SYMBOL);
        let cell = Fixup { at: u64::from(blaze_lazy::stub::CELL), apply: Apply::Rel64, target: hook, addend: 0, minus: None };
        let fixups = self.fixup_slice([cell]);
        let bytes: &'a [u8] = &lazy.bytes;
        let mut piece = self.piece(region, &Class::Stubs, crate::lazy::REGION_ALIGN, bytes.len() as u64, fixups, What::Fixed);
        piece.rank = 1;
        let id = self.make(&piece, Content::Bytes(bytes));
        // Interned once per lazy function, at the image's first link.
        for (at, key) in lazy.keys.iter().enumerate() {
            let function = self.symbols.id(&key.0);
            let slot = self.sym(SymKey::Slot(function));
            let entry = self.sym(SymKey::LazyEntry(function));
            let body = self.sym(SymKey::Body(function));
            self.new_slot(slot, *key)?;
            let offset = u64::from(lazy.layout.entry(blaze_lazy::LazyId(at as u32)));
            self.define(entry, Def::Piece { piece: id, offset });
            self.define(body, Def::Alias(slot));
            self.cell(slot, entry);
        }
        Ok(())
    }

    /// Whether `body` is a lazy function's: only those are an alias of a slot stub.
    fn is_lazy(&self, body: Sym) -> bool {
        self.lazy.is_some()
            && matches!(self.new.def(body), Def::Alias(to) if matches!(self.symbols.key(to), SymKey::Slot(_)))
    }

    /// A lazy image's hook, which its stubs call ([`blaze_lazy::hook::SYMBOL`]), resolved to
    /// code in the image: an error naming it otherwise.
    pub fn lazy_hook(&self) -> Result<(), LinkError> {
        if self.lazy.is_none() {
            return Ok(());
        }
        let hook = self.symbols.find_name(blaze_lazy::hook::SYMBOL).and_then(|it| self.symbols.find(SymKey::Name(it)));
        if hook.is_some_and(|it| self.new.home(it).is_some()) {
            return Ok(());
        }
        Err(crate::lazy::no_hook(self.output.format))
    }

    fn add_function(&mut self, function: &'a CompiledFunction, syms: &[Sym]) -> Result<(), LinkError> {
        let key = Some(function.key);
        let body = syms[0];
        let id = self.symbols.id(&function.key.0);
        let counter = self.counter(function, id)?;
        let (fixups, weak) = self.fixups(&function.relocs, &syms[1..=function.relocs.targets().len()], key, true, counter)?;
        let mut at = 1 + function.relocs.targets().len();
        for data in &function.data {
            let sym = syms[at];
            let relocs = &syms[at + 1..at + 1 + data.relocs.targets().len()];
            at += 1 + data.relocs.targets().len();
            self.add_data(data, sym, relocs, key)?;
        }
        let mut fde_sym = None;
        let mut windows = None;
        match &function.unwind {
            Some(Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
                let personality = personality.as_ref().map(|it| self.target_sym(it)).transpose()?;
                let mut hasher = Hasher::new(CIE_DOMAIN);
                hasher
                    .part(&code_align.to_le_bytes())
                    .part(&data_align.to_le_bytes())
                    .part(&return_address.to_le_bytes())
                    .part(cie);
                if let Some(personality) = personality {
                    self.scratch.clear();
                    self.symbols.write_key(personality, &mut *self.scratch);
                    hasher.part(self.scratch.as_slice());
                }
                let cie_id = self.symbols.id(&hasher.finish());
                let cie_sym = self.sym(SymKey::Cie(cie_id));
                if self.new.home(cie_sym).is_none() {
                    let fields = unwind::cie_into(
                        *code_align,
                        *data_align,
                        *return_address,
                        cie,
                        personality.is_some(),
                        &mut *self.scratch,
                    )
                    .map_err(|it| error(key, it))?;
                    let fixups = self.fixup_slice(fields.iter().map(|(at, _)| Fixup {
                        at: *at,
                        apply: Apply::GotRel32,
                        target: personality.expect("a personality field"),
                        addend: 0,
                        minus: None,
                    }));
                    let bytes: &'a [u8] = self.arena.alloc_slice_copy(self.scratch.as_slice());
                    let piece = self.piece(cie_sym, &Class::EhFrame, 4, bytes.len() as u64, fixups, What::Shared);
                    self.make(&piece, Content::Bytes(bytes));
                }
                let lsda = match lsda {
                    Some(id) => {
                        if !function.data.iter().any(|it| it.id == *id) {
                            return Err(error(key, "its LSDA is not one of its data"));
                        }
                        let id = self.symbols.id(id);
                        Some(self.sym(SymKey::Data(id)))
                    }
                    None => None,
                };
                let fields = unwind::fde_into(
                    function.code.len() as u64,
                    fde,
                    personality.is_some(),
                    lsda.is_some(),
                    &mut *self.scratch,
                )
                .map_err(|it| error(key, it))?;
                let fde_key = self.symbols.id(&function.key.0);
                let sym = self.sym(SymKey::Fde(fde_key));
                let fixups = self.fixup_slice(fields.iter().map(|(at, field)| match field {
                    Field::CiePointer => {
                        Fixup { at: *at, apply: Apply::Delta32, target: sym, addend: *at as i64, minus: Some(cie_sym) }
                    }
                    Field::Function => Fixup { at: *at, apply: Apply::Rel32, target: body, addend: 0, minus: None },
                    Field::Lsda => Fixup {
                        at: *at,
                        apply: Apply::Rel32,
                        target: lsda.expect("an LSDA field"),
                        addend: 0,
                        minus: None,
                    },
                    Field::Personality => unreachable!("a CIE's field"),
                }));
                let bytes: &'a [u8] = self.arena.alloc_slice_copy(self.scratch.as_slice());
                let mut piece = self.piece(sym, &Class::EhFrame, 4, bytes.len() as u64, fixups, What::Fde);
                piece.rank = 1;
                self.make(&piece, Content::Bytes(bytes));
                fde_sym = Some(sym);
            }
            Some(Unwind::Windows { info: bytes, handler, lsda }) => {
                let handler = handler.as_ref().map(|it| self.target_sym(it)).transpose()?;
                let lsda = lsda.map(|id| {
                    let id = self.symbols.id(&id);
                    self.sym(SymKey::Data(id))
                });
                let padded = bytes.len().next_multiple_of(4);
                let targets = handler.into_iter().chain(lsda);
                let size = padded + 4 * (usize::from(handler.is_some()) + usize::from(lsda.is_some()));
                let out: &'a mut [u8] = self.arena.alloc_slice_fill_with(size, |_| 0);
                out[..bytes.len()].copy_from_slice(bytes);
                let fixups = self.fixup_slice(targets.enumerate().map(|(n, target)| Fixup {
                    at: (padded + 4 * n) as u64,
                    apply: Apply::ImageRel32,
                    target,
                    addend: 0,
                    minus: None,
                }));
                let info_key = self.symbols.id(&function.key.0);
                let sym = self.sym(SymKey::WinInfo(info_key));
                let piece = self.piece(sym, &Class::Rodata, 4, size as u64, fixups, What::WinInfo);
                self.make(&piece, Content::Bytes(out));
                windows = Some((sym, handler, lsda));
            }
            None => {}
        }
        self.add_coverage(function, id, body)?;
        let info = self.new.add_function(function.key, id, fde_sym, windows, &function.lines);
        let (class, rank) = self.own(Class::Text);
        let mut piece = Self::piece_in(
            body,
            class,
            u64::from(function.align),
            function.code.len() as u64,
            fixups,
            What::Function(info),
        );
        piece.rank = rank;
        piece.weak = weak;
        let made = self.make(&piece, Content::Bytes(&function.code));
        if self.arch() == Arch::Wasm32 && wasm_imports_in(&function.code).map_err(|it| error(key, it))? {
            let (code, _) = blaze_codegen::wasm::Code::decode(&function.code)
                // alloc: error path
                .map_err(|it| error(key, format!("its wasm header: {it}")))?;
            self.declare_wasm(made, key, &code.imports)?;
        }
        if self.table {
            let slot = self.sym(SymKey::Slot(id));
            if self.new.home(slot).is_none() {
                self.new_slot(slot, function.key)?;
            }
            self.cell(slot, body);
        }
        Ok(())
    }

    fn new_slot(&mut self, slot: Sym, function: InstanceKey) -> Result<(), LinkError> {
        if self.new.slots >= self.new.capacity {
            return Err(refused(
                Some(function),
                Refusal::TableFull { slots: self.new.capacity },
                // alloc: error path
                format!("the jump table is full ({} slots): link the image from scratch", self.new.capacity),
            ));
        }
        let stubs = self.symbols.find(SymKey::Stubs).expect("the table was made");
        let Def::Piece { piece, .. } = self.new.def(stubs) else {
            return Err(error(Some(function), "the jump table's stubs are gone"));
        };
        let index = self.new.new_slot(slot);
        let offset = u64::from(index) * table::stub_size(self.arch());
        self.define(slot, Def::Piece { piece, offset });
        Ok(())
    }

    /// The class and rank of SanitizerCoverage's section `kind` ([`Stage::sancov`]).
    fn sancov_class(&mut self, kind: usize, function: InstanceKey) -> Result<(ClassId, u32), LinkError> {
        if let Some(it) = self.sancov[kind] {
            return Ok(it);
        }
        let names = match self.output.format {
            Format::Elf => &SANCOV_ELF,
            Format::MachO => &SANCOV_MACHO,
            Format::Pe => &SANCOV_COFF,
            Format::Wasm => {
                return Err(error(Some(function), "SanitizerCoverage on wasm: a module has no sections for its arrays"));
            }
        };
        // Writable, PC table too: it holds addresses the loader relocates, and LLVM marks it so.
        let (class, rank) = self.writer.section(names[kind], true, false, false).map_err(|it| error(Some(function), it))?;
        let it = (self.new.class_id(&class), rank);
        self.sancov[kind] = Some(it);
        Ok(it)
    }

    /// A covered function's arrays (zeroed) and its PC table entries, a piece each in its
    /// kind's section. A section's pieces are in the order of their functions' keys, as the
    /// functions' code is in a full link, so every section lists the functions alike; they are
    /// the function's, made and removed with it.
    fn add_coverage(&mut self, function: &'a CompiledFunction, id: IdNo, body: Sym) -> Result<(), LinkError> {
        let Some(covered) = &function.coverage else { return Ok(()) };
        let key = Some(function.key);
        if covered.guards % 4 != 0 {
            // alloc: error path
            return Err(error(key, format!("{} bytes of SanitizerCoverage guards, which are 4 bytes each", covered.guards)));
        }
        let arrays = [(covered.counters, 1), (covered.flags, 1), (covered.guards, 4)];
        for (kind, (size, align)) in arrays.into_iter().enumerate() {
            if size == 0 {
                continue;
            }
            let (class, rank) = self.sancov_class(kind, function.key)?;
            let sym = self.sym(SymKey::Coverage(id, kind as u32));
            let mut piece = Self::piece_in(sym, class, align, u64::from(size), &[], What::Fixed);
            piece.rank = rank;
            self.make(&piece, Content::Zero);
        }
        if covered.pcs.is_empty() {
            return Ok(());
        }
        let len = function.code.len() as u64;
        if let Some((at, _)) = covered.pcs.iter().find(|(at, _)| u64::from(*at) >= len) {
            // alloc: error path
            return Err(error(key, format!("a SanitizerCoverage PC at {at}, past its {len} bytes of code")));
        }
        let (class, rank) = self.sancov_class(SANCOV_PCS, function.key)?;
        let bytes: &'a mut [u8] = self.arena.alloc_slice_fill_with(covered.pcs.len() * 16, |_| 0);
        for (out, (_, flags)) in bytes.chunks_exact_mut(16).zip(&covered.pcs) {
            out[8..].copy_from_slice(&flags.to_le_bytes());
        }
        let fixups = self.fixup_slice(covered.pcs.iter().enumerate().map(|(n, (at, _))| Fixup {
            at: 16 * n as u64,
            apply: Apply::Abs64,
            target: body,
            addend: i64::from(*at),
            minus: None,
        }));
        let sym = self.sym(SymKey::CoveragePcs(id));
        let mut piece = Self::piece_in(sym, class, 8, bytes.len() as u64, fixups, What::Fixed);
        piece.rank = rank;
        self.make(&piece, Content::Bytes(bytes));
        Ok(())
    }

    fn add_data(
        &mut self,
        data: &'a Data,
        sym: Sym,
        relocs: &[Sym],
        function: Option<InstanceKey>,
    ) -> Result<(), LinkError> {
        if self.new.home(sym).is_some() {
            return Ok(());
        }
        let (fixups, weak) = self.fixups(&data.relocs, relocs, function, false, None)?;
        // A static's writable object is written as the program runs, as rustc's global of a
        // nested mutable allocation is: never read-only, its ID its static's alone.
        let class = match (data.writable, fixups.is_empty()) {
            (true, _) => Class::Data,
            (false, true) => Class::Rodata,
            (false, false) => Class::RelRo,
        };
        let (class, rank) = self.own(class);
        let mut piece =
            Self::piece_in(sym, class, u64::from(data.align), data.bytes.len() as u64, fixups, What::Shared);
        piece.rank = rank;
        piece.weak = weak;
        self.make(&piece, Content::Bytes(&data.bytes));
        Ok(())
    }

    fn add_static(&mut self, it: &'a Static) -> Result<(), LinkError> {
        let (item, instance) = (self.symbols.id(&it.item.0), self.symbols.id(&it.instance));
        let sym = self.sym(SymKey::Static(item, instance));
        if let Def::Piece { piece, .. } = self.new.def(sym) {
            self.unmake(piece);
        }
        for data in &it.data {
            let id = self.symbols.id(&data.id);
            let data_sym = self.sym(SymKey::Data(id));
            let relocs = self.reloc_syms(&data.relocs)?;
            self.add_data(data, data_sym, relocs, None)?;
        }
        let (bytes, relocs, size, zero): (&'a [u8], &'a Relocs, u64, bool) = match &it.init {
            Init::Bytes { bytes, relocs } => (&bytes[..], relocs, bytes.len() as u64, false),
            Init::Zeroed { size } => (&[][..], &NO_RELOCS, *size, true),
        };
        let syms = self.reloc_syms(relocs)?;
        let (fixups, weak) = self.fixups(relocs, syms, None, false, None)?;
        // alloc: error path
        let static_error = |what: String| error(None, format!("the static {:?}: {what}", it.item));
        let (class, rank) = match &it.section {
            Some(name) => {
                let (class, rank) = self
                    .writer
                    .section(name, it.mutable || !fixups.is_empty(), false, it.thread_local)
                    .map_err(static_error)?;
                (self.new.class_id(&class), rank)
            }
            None => self.own(match (it.thread_local, it.mutable, zero, fixups.is_empty()) {
                (true, _, false, _) => Class::Tdata,
                (true, _, true, _) => Class::Tbss,
                (false, true, true, _) => Class::Bss,
                (false, true, false, _) => Class::Data,
                (false, false, _, true) => Class::Rodata,
                (false, false, _, false) => Class::RelRo,
            }),
        };
        let zeroed = crate::state::zeroed(self.new.class(class));
        let content = match (zero, zeroed) {
            (false, true) => {
                let class = self.new.class(class);
                // alloc: error path
                return Err(static_error(format!("initialized bytes in the zeroed section {class:?}")));
            }
            (false, false) => Content::Bytes(bytes),
            (true, _) => Content::Zero,
        };
        let mut piece =
            Self::piece_in(sym, class, u64::from(it.align), size, fixups, What::Static { thread_local: it.thread_local });
        piece.rank = rank;
        piece.zero = zeroed;
        piece.weak = weak;
        let made = self.make(&piece, content);
        if self.arch() == Arch::Wasm32 {
            self.declare_wasm(made, None, &it.imports)?;
        }
        if it.thread_local && self.plan.tls_descriptors {
            let tlv = self.sym(SymKey::Tlv(item, instance));
            if let Def::Piece { piece, .. } = self.new.def(tlv) {
                self.unmake(piece);
            }
            let bootstrap = self.sym_name("_tlv_bootstrap");
            let fixups = self.fixup_slice([
                Fixup { at: 0, apply: Apply::Abs64, target: bootstrap, addend: 0, minus: None },
                Fixup { at: 16, apply: Apply::DtpOff64, target: sym, addend: 0, minus: None },
            ]);
            let piece = self.piece(tlv, &Class::ThreadVars, 8, 24, fixups, What::Tlv);
            self.make(&piece, Content::Zero);
        }
        self.statics += 1;
        Ok(())
    }

    /// The span tables the change carries, checked and kept once each; the panic locations
    /// of a changed one rewritten in place. Finding them costs the changed table's spans.
    fn spans(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        if change.spans.is_empty() {
            return Ok(());
        }
        let mut changed: FrameVec<'a, (IdNo, u32)> = Vec::new_in(self.arena);
        for (anchor, bytes) in &change.spans {
            let id = self.symbols.id(anchor);
            let before = match self.new.span_table(id) {
                Some(it) if it.is(bytes) => continue,
                Some(it) => it.len(),
                None => 0,
            };
            // alloc: error path
            let spans = Spans::new(bytes).map_err(|it| error(None, format!("the span table of {anchor:?}: {it}")))?;
            let len = before.max(spans.len());
            self.new.set_span_table(id, spans);
            self.spans_changed = true;
            changed.push((id, len));
        }
        for (anchor, len) in changed {
            for index in 0..len {
                let Some(sym) = self.symbols.find(SymKey::Position(anchor, index)) else { continue };
                let Some(id) = self.new.home(sym).map(|it| it.0).filter(|it| self.new.piece(*it).is_some()) else {
                    continue;
                };
                let (bytes, file) = self.location(anchor, index)?;
                let fixups = [Fixup { at: 0, apply: Apply::Abs64, target: file, addend: 0, minus: None }];
                self.retarget(id, &fixups);
                self.contents.insert(id, Content::Bytes(bytes));
                self.write.insert(id);
            }
        }
        Ok(())
    }

    /// Replaces the fixups of a piece that was there before the stage, counting the shared
    /// pieces it stops and starts referring to.
    fn retarget(&mut self, id: PieceId, fixups: &[Fixup]) {
        let before = State::targets(self.new.piece(id).expect("a live piece"));
        self.new.set_fixups(id, fixups);
        let after = State::targets(self.new.piece(id).expect("a live piece"));
        for target in before.iter().filter(|it| !after.contains(it)) {
            if let Some(shared) = self.shared(*target) {
                let users = self.new.users_mut(shared);
                *users = users.saturating_sub(1);
                self.touched.push(shared);
            }
        }
        for target in after.iter().filter(|it| !before.contains(it)) {
            self.counted.push(*target);
        }
    }

    /// A span's `Location` record and the symbol of its file name, made when missing.
    fn location(&mut self, anchor: IdNo, index: u32) -> Result<(&'a [u8], Sym), LinkError> {
        let arena = self.arena;
        let Some(table) = self.new.span_table(anchor) else {
            let anchor = self.symbols.content(anchor);
            // alloc: error path
            return Err(error(None, format!("a position in {anchor:?}, whose span table the link was not given")));
        };
        let Some(at) = table.span(index) else {
            let len = table.len();
            let anchor = self.symbols.content(anchor);
            // alloc: error path
            return Err(error(None, format!("span {index} of {anchor:?}, whose table has {len}")));
        };
        let name = table.file(at.file);
        let record: &'a [u8] = arena.alloc_slice_copy(&position::location(name.len(), at));
        let file_name = self.symbols.name(name);
        let file = self.symbols.intern(SymKey::FileName(file_name));
        let bytes: Option<&'a [u8]> = self.new.home(file).is_none().then(|| {
            let out = arena.alloc_slice_fill_with(name.len() + 1, |_| 0);
            out[..name.len()].copy_from_slice(name.as_bytes());
            &*out
        });
        if let Some(bytes) = bytes {
            let piece = self.piece(file, &Class::Rodata, 1, bytes.len() as u64, &[], What::Shared);
            self.make(&piece, Content::Bytes(bytes));
        }
        Ok((record, file))
    }

    /// The `Location` records the fresh pieces refer to and that don't exist yet.
    fn positions(&mut self) -> Result<(), LinkError> {
        let mut wanted: FrameVec<'a, (Sym, IdNo, u32)> = Vec::new_in(self.arena);
        for id in self.fresh.iter() {
            let piece = self.new.piece(id).expect("a fresh piece");
            for fixup in piece.fixups() {
                if let SymKey::Position(anchor, index) = self.symbols.key(fixup.target) {
                    wanted.push((fixup.target, anchor, index));
                }
            }
        }
        radix::sort_copy_by_key(&mut wanted, |it| it.0.0);
        wanted.dedup_by_key(|it| it.0);
        let class = self.new.class_id(&Class::RelRo);
        for (sym, anchor, index) in wanted {
            if self.new.home(sym).is_some() {
                continue;
            }
            let (bytes, file) = self.location(anchor, index)?;
            let fixups = self.fixup_slice([Fixup { at: 0, apply: Apply::Abs64, target: file, addend: 0, minus: None }]);
            let piece = Self::piece_in(
                sym,
                class,
                position::LOCATION_ALIGN,
                position::LOCATION_SIZE,
                fixups,
                What::Shared,
            );
            self.make(&piece, Content::Bytes(bytes));
        }
        Ok(())
    }

    /// Counts the fixups to shared pieces that this stage added and removed, and removes the
    /// shared pieces nothing refers to any more.
    pub fn sweep(&mut self) {
        let arena = self.arena;
        let mut touched = std::mem::replace(&mut self.touched, Vec::new_in(arena));
        let mut fresh = collect_in(self.fresh.iter(), arena);
        radix::sort(&mut fresh);
        let mut targets = std::mem::replace(&mut self.counted, Vec::new_in(arena));
        for id in fresh.iter() {
            let piece = self.new.piece(*id).expect("a fresh piece");
            targets.extend(State::targets(piece));
            if matches!(piece.what(), What::Shared) {
                touched.push(*id);
            }
        }
        for target in targets {
            if let Some(shared) = self.shared(target) {
                *self.new.users_mut(shared) += 1;
            }
        }
        let mut at = 0;
        while at < self.removed.len() || !touched.is_empty() {
            while at < self.removed.len() {
                let id = self.removed[at];
                at += 1;
                if self.old.piece(id).is_none() && fresh.binary_search(&id).is_err() {
                    continue;
                }
                let targets = State::targets(self.new.any(id).expect("a removed piece"));
                for target in targets {
                    if let Some(shared) = self.shared(target).or_else(|| self.old_shared(target)) {
                        if shared < self.new.piece_count() {
                            let users = self.new.users_mut(shared);
                            *users = users.saturating_sub(1);
                        }
                        touched.push(shared);
                    }
                }
            }
            radix::sort(&mut touched);
            touched.dedup();
            for id in std::mem::replace(&mut touched, Vec::new_in(arena)) {
                let live = self.new.piece(id).is_some_and(|it| matches!(it.what(), What::Shared));
                if live && self.new.users(id) == 0 {
                    self.unmake(id);
                }
            }
        }
    }

    /// The shared piece a symbol is at the start of, in the new state.
    fn shared(&self, sym: Sym) -> Option<PieceId> {
        match self.new.def(sym) {
            Def::Piece { piece, offset: 0 } => matches!(self.new.piece(piece)?.what(), What::Shared).then_some(piece),
            _ => None,
        }
    }

    /// The same, as it was before the stage: a removed piece's references were counted then.
    fn old_shared(&self, sym: Sym) -> Option<PieceId> {
        match self.old.def(sym) {
            Def::Piece { piece, offset: 0 } => matches!(self.old.piece(piece)?.what(), What::Shared).then_some(piece),
            _ => None,
        }
    }

    /// The GOT cell of a target, if one was made.
    pub fn got_cell(&self, kind: GotKind, target: Sym) -> Option<Sym> {
        self.symbols.find(SymKey::Got(kind, target))
    }

    /// The PLT stub of a target, if one was made.
    pub fn plt_stub(&self, target: Sym) -> Option<Sym> {
        self.symbols.find(SymKey::Plt(target))
    }

    /// The GOT cells and PLT stubs the pieces to write need, made when missing.
    pub fn needs(&mut self) -> Result<(), LinkError> {
        let arena = self.arena;
        let mut cells: FrameVec<'a, (GotKind, Sym)> = Vec::new_in(arena);
        let mut plt: FrameVec<'a, Sym> = Vec::new_in(arena);
        let mut got_specials: FrameVec<'a, (Sym, u32)> = Vec::new_in(arena);
        for id in self.write.iter() {
            let Some(piece) = self.new.piece(id) else { continue };
            for fixup in piece.fixups() {
                if let Some(kind) = reloc::got_kind(fixup.apply) {
                    cells.push((kind, self.got_target(fixup)));
                } else if self.through_plt(fixup) {
                    plt.push(fixup.target);
                } else if let Some((sym, at)) = special_at(&self.new, fixup.target) {
                    if matches!(self.new.specials.get(at as usize), Some(Special::Got { .. })) {
                        got_specials.push((sym, at));
                    }
                }
            }
        }
        radix::sort_copy_by_key(&mut got_specials, |it| it.0.0);
        got_specials.dedup();
        for (sym, at) in got_specials {
            let Some(special) = self.new.specials.get(at as usize) else { continue };
            let Some(name) = self.symbols.name_of(sym).and_then(|it| special.got_target(it)) else { continue };
            // Copied out of the table it is interned into.
            let name: &str = arena.alloc_str(name);
            let target = self.symbols.sym_name(name);
            cells.push((GotKind::Address, target));
        }
        radix::sort_copy_by_key(&mut plt, |it| it.0);
        plt.dedup();
        cells.extend(plt.iter().map(|it| (GotKind::Address, *it)));
        radix::sort_copy_by_key(&mut cells, |&(kind, sym)| radix::pack_32(kind as u32, sym.0));
        cells.dedup();
        let got = self.new.class_id(&Class::Got);
        for (kind, target) in cells {
            let sym = self.sym(SymKey::Got(kind, target));
            if self.new.home(sym).is_some() {
                continue;
            }
            let size = kind.cells() * 8;
            let fixups = self.got_fixups(kind, target);
            let mut piece = Self::piece_in(sym, got, 8, size, fixups, What::Got { kind, target });
            piece.order = [u32::MAX, 0, 0];
            self.make(&piece, Content::Zero);
        }
        let arch = self.arch();
        let class = self.new.class_id(&Class::Plt);
        for target in plt {
            let sym = self.sym(SymKey::Plt(target));
            if self.new.home(sym).is_some() {
                continue;
            }
            let (code, stub) = table::plt(arch);
            let fixups = self.fixup_slice(stub.iter().map(|it| Fixup {
                at: it.at,
                apply: match it.apply {
                    Apply::Rel32 => Apply::X86GotPcRel32,
                    Apply::A64Page21 => Apply::A64GotPage21,
                    Apply::A64Lo12 { .. } => Apply::A64GotLo12,
                    other => other,
                },
                target,
                addend: it.addend,
                minus: None,
            }));
            let piece = Self::piece_in(sym, class, 16, code.len() as u64, fixups, What::Plt { target });
            self.make(&piece, Content::Bytes(code));
        }
        if self.plan.got_by_library {
            self.iat_ends();
        }
        Ok(())
    }

    /// The zero cell after each needed DLL's import address table cells, which the GOT's
    /// layout puts after them ([`Stage::iat_end`]); those of DLLs no longer needed removed.
    /// The cells there are are the last resolution's libraries' ([`Stage::old`]).
    fn iat_ends(&mut self) {
        let arena = self.arena;
        let mut names: FrameVec<'a, crate::symbols::NameId> =
            collect_in(self.new.resolved.libraries.iter().map(|it| self.symbols.name(&it.name)), arena);
        for library in self.old.resolved.libraries.iter() {
            let Some(name) = self.symbols.find_name(&library.name) else { continue };
            if names.contains(&name) {
                continue;
            }
            if let Some(Def::Piece { piece, .. }) = self.symbols.find(SymKey::IatEnd(name)).map(|it| self.new.def(it)) {
                self.unmake(piece);
            }
        }
        names.sort_by(|a, b| self.symbols.str(*a).cmp(self.symbols.str(*b)));
        names.dedup();
        let got = self.new.class_id(&Class::Got);
        for name in names {
            let sym = self.sym(SymKey::IatEnd(name));
            if self.new.home(sym).is_some() {
                continue;
            }
            let mut piece = Self::piece_in(sym, got, 8, 8, &[], What::Fixed);
            piece.order = [u32::MAX, 0, 0];
            self.make(&piece, Content::Zero);
        }
    }

    /// Of an import address table's zero cell, the index of its DLL in the image's libraries.
    pub fn iat_end(&self, sym: Sym) -> Option<u32> {
        let SymKey::IatEnd(name) = self.symbols.key(sym) else { return None };
        let name = self.symbols.str(name);
        self.new.resolved.libraries.iter().position(|it| it.name == name).map(|it| it as u32)
    }

    /// What a GOT cell of a fixup holds the address of: a thread-local static's TLV
    /// descriptor for the TLV kinds, else the target.
    pub fn got_target(&self, fixup: &Fixup) -> Sym {
        crate::write::got_target(&self.symbols, fixup)
    }

    fn imported(&self, sym: Sym) -> bool {
        self.import_of(sym).is_some()
    }

    /// The import a symbol is, through its aliases.
    pub fn import_of(&self, mut sym: Sym) -> Option<u32> {
        for _ in 0..64 {
            match self.new.def(sym) {
                Def::Import(at) => return Some(at),
                Def::Alias(to) => sym = to,
                _ => return None,
            }
        }
        None
    }

    /// The linker's symbol a symbol is, through its aliases.
    pub fn special_of(&self, sym: Sym) -> Option<&Special> {
        self.new.specials.get(special_at(&self.new, sym)?.1 as usize)
    }

    /// Whether a fixup reaches its target at the target's PLT stub: a branch to an import,
    /// and where imports are reached only through their GOT cells (PE), any reference to an
    /// imported function that is not through its cell, as link.exe's import thunk.
    pub fn through_plt(&self, fixup: &Fixup) -> bool {
        let Some(at) = self.import_of(fixup.target) else { return false };
        if reloc::is_branch(fixup.apply) {
            return true;
        }
        self.plan.got_by_library
            && matches!(fixup.apply, Apply::Abs64 | Apply::ImageRel32 | Apply::Rel32)
            && self.new.resolved.imports.get(at as usize).is_some_and(|it| it.kind == crate::model::SymbolKind::Function)
    }

    /// Records that the piece `id` just made (a function's body or a static) declares what
    /// codegen says each of `imports` is imported as on wasm. A name's import is the one its
    /// live declarers agree on: a different one is an error naming both, and an edit that
    /// changes a name's import removes the old declarer in the same change ([`Stage::apply`]
    /// removes first). The map is copied only when a name is new.
    fn declare_wasm(
        &mut self,
        id: PieceId,
        function: Option<InstanceKey>,
        imports: &[(String, blaze_codegen::wasm::Import)],
    ) -> Result<(), LinkError> {
        if imports.is_empty() {
            return Ok(());
        }
        let mut names = Vec::with_capacity_in(imports.len(), self.arena);
        for (name, import) in imports {
            let name = self.symbols.name(name);
            // A piece declares a few names: a linear check finds one it declared already.
            let ours = names.contains(&name.0);
            if ours || self.new.wasm_declarers(name) > 0 {
                let agreed = self.new.wasm_imports.get(&name).expect("a declared name's import");
                if agreed != import {
                    return Err(self.wasm_conflict(id, function, name, agreed, import, ours));
                }
            } else if self.new.wasm_imports.get(&name) != Some(import) {
                // clone: the state's map owns its imports; only a new one is copied
                Arc::make_mut(&mut self.new.wasm_imports).insert(name, import.clone());
            }
            if !ours {
                names.push(name.0);
            }
        }
        self.new.declare_wasm(id, &names);
        Ok(())
    }

    /// The error of the piece `id` declaring `name` imported as `import`, when a live piece
    /// (`ours`: `id` itself) declares it as `agreed`.
    fn wasm_conflict(
        &self,
        id: PieceId,
        function: Option<InstanceKey>,
        name: NameId,
        agreed: &blaze_codegen::wasm::Import,
        import: &blaze_codegen::wasm::Import,
        ours: bool,
    ) -> LinkError {
        let by = self.symbols.describe(self.new.piece(id).expect("just made").sym);
        let other = if ours {
            // clone: error path
            by.clone()
        } else {
            // An error's: one walk of the pieces finds the other declarer, which a count above
            // 0 says is live, and is not `id`, which declares nothing yet.
            let other = self.new.live_pieces().find(|it| it.wasm_names().contains(&name.0)).expect("a live declarer");
            self.symbols.describe(other.sym)
        };
        let shown = |it: &blaze_codegen::wasm::Import| {
            if agreed.ty == import.ty {
                // alloc: error path
                format!("`{}`.`{}`", it.module, it.field)
            } else {
                // alloc: error path
                format!("`{}`.`{}` of type {:?}", it.module, it.field, it.ty)
            }
        };
        // alloc: error path
        let what = format!(
            "`{}` is imported as {} by {other} and as {} by {by}: one name is one wasm import, so \
             each module's import needs a name of its own",
            self.symbols.str(name),
            shown(agreed),
            shown(import),
        );
        error(function, what)
    }

    /// A GOT cell's fixups: the core writes what it can; an import's cells are the loader's.
    fn got_fixups(&self, kind: GotKind, target: Sym) -> &'a [Fixup] {
        if self.imported(target) {
            return &[];
        }
        let fixup = |at, apply| Fixup { at, apply, target, addend: 0, minus: None };
        match kind {
            GotKind::Address => self.fixup_slice([fixup(0, Apply::Abs64)]),
            GotKind::TpOff => self.fixup_slice([fixup(0, Apply::TpOff64)]),
            GotKind::TlsGd => self.fixup_slice([fixup(8, Apply::DtpOff64)]),
            GotKind::TlsDesc => &[],
        }
    }

    /// Rewrites every piece referring to a symbol whose definition changed, and every cell
    /// running a body that moved. Costs the changed symbols' referrers.
    pub fn referrers(&mut self) {
        for sym in self.changed.iter() {
            for id in self.new.refs(sym) {
                self.write.insert(id);
            }
            for slot in self.new.slots_of(sym) {
                self.dirty_cells.insert(slot);
            }
        }
        if !self.dirty_cells.is_empty() {
            if let Some(cells) = self.cells_piece() {
                self.write.insert(cells);
            }
        }
    }

    /// Fails when a removed function or static is still referred to, directly or through
    /// its slot.
    pub fn dangling(&self) -> Result<(), LinkError> {
        for slot in self.emptied.iter() {
            if self.new.cell(*slot).is_some() {
                continue;
            }
            let Some(slot_sym) = self.new.slot_syms.get(*slot as usize).copied() else { continue };
            if let Some(caller) = self.new.refs(slot_sym).find_map(|it| self.new.piece(it)) {
                return Err(error(
                    function_of(caller),
                    // alloc: error path
                    format!(
                        "{} is removed with nothing put in its slot, and {} still calls it",
                        self.symbols.describe(slot_sym),
                        self.symbols.describe(caller.sym),
                    ),
                ));
            }
        }
        for id in self.removed.iter() {
            let piece = self.new.any(*id).expect("a removed piece");
            for sym in std::iter::once(piece.sym).chain(piece.defines().iter().copied()) {
                if self.new.home(sym).is_some() || !matches!(self.new.def(sym), Def::None) {
                    continue;
                }
                if let Some(user) = self.new.refs(sym).find_map(|it| self.new.piece(it)) {
                    return Err(error(
                        function_of(user),
                        // alloc: error path
                        format!(
                            "{} is removed, and {} still refers to it",
                            self.symbols.describe(sym),
                            self.symbols.describe(user.sym),
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn function_of(piece: PieceRef<'_>) -> Option<InstanceKey> {
    piece.function().map(|it| it.key)
}

/// The linker's symbol a symbol is, through its aliases, and its index in [`State::specials`].
fn special_at(state: &State, mut sym: Sym) -> Option<(Sym, u32)> {
    for _ in 0..64 {
        match state.def(sym) {
            Def::Special(at) => return Some((sym, at)),
            Def::Alias(to) => sym = to,
            _ => return None,
        }
    }
    None
}

/// A code header's read error, as the stage says it.
fn in_header(error: cursor::Error) -> String {
    // alloc: error path
    format!("wasm: {error} in a code header")
}

/// Moves past a function type (`0x60`, the parameters, the results).
fn skip_signature(header: &mut Reader<'_>) -> Result<(), String> {
    if header.u8().map_err(in_header)? != 0x60 {
        // alloc: error path
        return Err("wasm: a type that is not a function type".to_owned());
    }
    for _ in 0..2 {
        header.bytes_uleb().map_err(in_header)?;
    }
    Ok(())
}

/// Whether a wasm function's code header ([`blaze_codegen::wasm::Code`]) lists imports:
/// read past its type, types, type fixups and stack pointer fixups without decoding them,
/// so a function without imports allocates nothing.
fn wasm_imports_in(code: &[u8]) -> Result<bool, String> {
    let mut header = Reader::new(Reader::new(code).bytes_uleb().map_err(in_header)?);
    skip_signature(&mut header)?;
    for _ in 0..header.uleb().map_err(in_header)? {
        skip_signature(&mut header)?;
    }
    for _ in 0..header.uleb().map_err(in_header)? {
        header.uleb().map_err(in_header)?;
        header.uleb().map_err(in_header)?;
    }
    for _ in 0..header.uleb().map_err(in_header)? {
        header.uleb().map_err(in_header)?;
    }
    Ok(header.uleb().map_err(in_header)? > 0)
}

#[cfg(test)]
mod tests {
    use blaze_codegen::wasm::{Code, Import, Signature, ValType};

    use super::*;

    /// The header scan agrees with the full decode on whether there are imports.
    #[test]
    fn the_wasm_header_scan_finds_imports_as_the_decoder_does() {
        let sig = Signature { params: vec![ValType::I32, ValType::I64], results: vec![ValType::F64] };
        let bare_code = || Code {
            ty: sig.clone(),
            types: vec![sig.clone(), Signature::default()],
            type_fixups: vec![(3, 0), (300, 1)],
            stack_pointer: vec![1, 70_000],
            imports: Vec::new(),
            export: Some("f".to_owned()),
        };
        let bare = bare_code();
        let with = Code {
            imports: vec![("puts".to_owned(), Import { module: "env".to_owned(), field: "puts".to_owned(), ty: sig.clone() })],
            ..bare_code()
        };
        for code in [&bare, &with] {
            let (bytes, _) = code.encode(&[0x0b]);
            let (decoded, _) = Code::decode(&bytes).unwrap();
            assert_eq!(wasm_imports_in(&bytes), Ok(!decoded.imports.is_empty()));
            for cut in 0..bytes.len() - 1 {
                let _ = wasm_imports_in(&bytes[..cut]);
            }
        }
    }

    #[test]
    fn contents_are_dense_for_fresh_pieces_and_mapped_for_others() {
        let arena = crate::arena::stage_frame().unwrap();
        let mut contents = Contents::new(10, &arena);
        contents.insert(12, Content::Zero);
        contents.insert(3, Content::Bytes(b"abc"));
        assert!(matches!(contents.get(12), Some(Content::Zero)));
        assert!(contents.get(11).is_none());
        assert_eq!(contents.get(3).and_then(Content::bytes), Some(&b"abc"[..]));
        contents.remove(12);
        contents.remove(3);
        assert!(!contents.contains(12) && !contents.contains(3));
        let mut out = [9u8; 5];
        Content::Bytes(b"ab").copy_to(&mut out);
        assert_eq!(out, [b'a', b'b', 0, 0, 0]);
    }
}
