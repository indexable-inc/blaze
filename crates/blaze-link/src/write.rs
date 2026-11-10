//! Relocation, in parallel, and the lists the writer reads.
//!
//! Before the parallel loop, every symbol the pieces to write refer to is resolved once into
//! a dense column (16 bytes a symbol: its value and kind, and where its GOT cells and PLT
//! stub are), so a fixup costs one load and a match on a byte: no hash, no allocation, and
//! strings only when it fails. The pieces are cut into jobs in address order: each gets a
//! disjoint slice of the bytes, a piece with many fixups is split at fixup boundaries into
//! several, and small ones run in batches. A full link writes each region into one zeroed
//! buffer; an incremental one writes its pieces into one scratch buffer, then into the
//! regions' pages, so only the pages it writes are copied.
//!
//! The lists the writer reads (rebased addresses, binds, GOT cells, functions) are kept
//! across links, sorted, with the piece each entry is of: an incremental link drops the
//! entries of the pieces it wrote or removed and merges in their new ones (a plan from the
//! changes, then a parallel copy of the kept runs), and the writer borrows them. The debug sections are made again only when a function or a
//! span table changed.

use std::ops::Range;
use std::sync::Arc;

use blaze_codegen::Arch;
use blaze_graph::par;
use blaze_hash::HashMap;
use blaze_ids::radix;
use std::alloc::Global;

use crate::arena::{FrameVec, collect_in};
use crate::cow::{CowBytes, IdSet};
use crate::model::{
    Apply, Bind, Class, Delta, Export, Fixup, Formats, Function, GotEntry, Holds, Import, Pages,
    Placed, Region, Special, Splice, Sym, SymbolKind, Tls, WindowsUnwind, Writer,
};
use crate::place::Totals;
use crate::reloc::{self, Bad, Ctx, GotKind};
use crate::stage::{Content, Stage, error, function_of, named_in};
use crate::state::{self, Def, PieceId, PieceRef, Space, State, What};
use crate::symbols::{IdNo, SymKey, Symbols};
use crate::thunk;
use crate::{Debug, Format, LinkError, OutputKind, Refusal};

/// What a symbol is worth to a fixup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Value {
    /// An address in the image: slid by the loader.
    Image(u64),
    /// A number: not slid.
    Absolute(u64),
    Import(u32),
    /// An offset in the image's TLS block.
    Tls(u64),
    /// A thread-local of an image this one is linked beside: its offset from the thread
    /// pointer ([`Def::TpOff`]).
    TpOff(i64),
}

/// Why a symbol has no value; made into words only when an error is reported.
#[derive(Debug)]
pub(crate) enum Why {
    Undefined(Sym),
    Removed,
    Alias(Sym),
    Special(String),
}

impl Why {
    fn say(&self, symbols: &Symbols) -> String {
        self.say_with(&|sym| symbols.describe(sym))
    }

    /// [`Why::say`] with each symbol named by `describe` ([`named_in`] where the writer has
    /// the state and the readers).
    fn say_with(&self, describe: &dyn Fn(Sym) -> String) -> String {
        match self {
            // alloc: error path
            Why::Undefined(sym) => format!("{} is not defined", describe(*sym)),
            Why::Removed => "a symbol in a removed piece".into(),
            // alloc: error path
            Why::Alias(sym) => format!("{} is an alias of itself", describe(*sym)),
            // clone: error path
            Why::Special(what) => what.clone(),
        }
    }
}

/// What a GOT cell of a fixup holds the address of: a thread-local static's TLV descriptor
/// for the TLV kinds, else the target ([`Stage::got_target`], without the stage).
pub(crate) fn got_target(symbols: &Symbols, fixup: &Fixup) -> Sym {
    if matches!(fixup.apply, Apply::TlvPage21 | Apply::TlvLo12 | Apply::TlvRel32) {
        if let SymKey::Static(item, instance) = symbols.key(fixup.target) {
            if let Some(tlv) = symbols.find(SymKey::Tlv(item, instance)) {
                return tlv;
            }
        }
    }
    fixup.target
}

/// Whether a fixup to import `at` reaches it at its PLT stub ([`Stage::through_plt`]).
#[inline]
fn through_plt(apply: Apply, got_by_library: bool, imports: &[Import], at: u32) -> bool {
    reloc::is_branch(apply)
        || (got_by_library
            && matches!(apply, Apply::Abs64 | Apply::ImageRel32 | Apply::Rel32)
            && imports.get(at as usize).is_some_and(|it| it.kind == SymbolKind::Function))
}

/// Symbol values as the image is placed: `Sync`, so the parallel passes share it.
#[derive(Clone, Copy)]
pub(crate) struct Resolver<'s> {
    symbols: &'s Symbols,
    state: &'s State,
    regions: &'s [Region],
    base: u64,
}

impl Resolver<'_> {
    /// The address of a name the image defines: [`Placed::symbol`].
    pub(crate) fn address_of(&self, name: &str) -> Option<u64> {
        match self.value(self.symbols.find_sym_name(name)?) {
            Ok(Value::Image(addr)) => Some(addr),
            _ => None,
        }
    }

    pub(crate) fn value(&self, mut sym: Sym) -> Result<Value, Why> {
        for _ in 0..64 {
            return Ok(match self.state.def(sym) {
                Def::Piece { piece, offset } => {
                    Value::Image(self.state.piece(piece).ok_or(Why::Removed)?.addr + offset)
                }
                Def::Import(at) => Value::Import(at),
                Def::Absolute(value) => Value::Absolute(value),
                Def::TlsOffset(value) => Value::Tls(value),
                Def::TpOff(value) => Value::TpOff(value),
                Def::Alias(to) => {
                    sym = to;
                    continue;
                }
                Def::Special(at) => self.special(sym, &self.state.specials[at as usize])?,
                Def::None => return Err(Why::Undefined(sym)),
            });
        }
        Err(Why::Alias(sym))
    }

    /// The first address of the first region `matches` takes and the end of the last; `what`
    /// names them in the error when there is none.
    fn bounds(
        &self,
        matches: impl Fn(&Class) -> bool,
        what: impl std::fmt::Display,
    ) -> Result<(u64, u64), Why> {
        let mut found = self.regions.iter().filter(|it| matches(&it.class));
        // alloc: error path
        let first = found.next().ok_or_else(|| Why::Special(format!("the image has no {what}")))?;
        let last = found.last().unwrap_or(first);
        Ok((first.addr, last.addr + last.size))
    }

    /// The value of `special`, the linker's symbol `sym` is.
    fn special(&self, sym: Sym, special: &Special) -> Result<Value, Why> {
        Ok(match special {
            Special::ImageBase => Value::Image(self.base),
            Special::Start(class) => {
                Value::Image(self.bounds(|it| it == class, format_args!("{class:?}"))?.0)
            }
            Special::Stop(class) => {
                Value::Image(self.bounds(|it| it == class, format_args!("{class:?}"))?.1)
            }
            Special::Section { name, end } => {
                let (start, stop) = self.bounds(
                    |it| matches!(it, Class::Named { name: n, .. } if n == name),
                    format_args!("section `{name}`"),
                )?;
                Value::Image(if *end { stop } else { start })
            }
            Special::Placed(at) => Value::Image(at(self.regions)),
            Special::Absolute(value) => Value::Absolute(*value),
            Special::LiveDescriptor => {
                let sym = self
                    .symbols
                    .find(SymKey::Descriptor)
                    .ok_or_else(|| Why::Special("an image with no live descriptor".into()))?;
                self.value(sym)?
            }
            Special::Got { .. } => {
                let name =
                    self.symbols.name_of(sym).and_then(|it| special.got_target(it)).ok_or_else(
                        || {
                            // alloc: error path
                            Why::Special(format!(
                                "{} names no GOT cell's target",
                                self.symbols.describe(sym)
                            ))
                        },
                    )?;
                let target = self
                    .symbols
                    .find_sym_name(name)
                    // alloc: error path
                    .ok_or_else(|| Why::Special(format!("`{name}` is not defined")))?;
                let cell = self.cell(GotKind::Address, target).ok_or_else(|| {
                    // alloc: error path
                    Why::Special(format!("no GOT cell of {}", self.symbols.describe(target)))
                })?;
                Value::Image(cell)
            }
        })
    }

    fn cell(&self, kind: GotKind, target: Sym) -> Option<u64> {
        self.state.addr_of(self.symbols.find(SymKey::Got(kind, target))?)
    }

    fn plt(&self, target: Sym) -> Option<u64> {
        self.state.addr_of(self.symbols.find(SymKey::Plt(target))?)
    }
}

const UNSET: u64 = 0;
const IMAGE: u64 = 1;
const ABSOLUTE: u64 = 2;
const IMPORT: u64 = 3;
const TLS: u64 = 4;
const WHY: u64 = 5;
const TP_OFF: u64 = 6;
/// No such cell or stub.
const NONE: u64 = u64::MAX;

/// Where a symbol's GOT cells (by [`GotKind`]) and PLT stub are.
#[derive(Clone, Copy, Debug)]
struct Cells {
    got: [u64; 4],
    plt: u64,
}

const NO_CELLS: Cells = Cells { got: [NONE; 4], plt: NONE };

/// The value of every symbol a stage's writes refer to, by [`Symbols::split`]: per symbol
/// its value and a word of its kind (low byte) and its [`Cells`] (index + 1, high half).
struct Column {
    /// Every shard's slots, one after another: shard `s` at `starts[s]..starts[s] + lens[s]`.
    slots: Vec<[u64; 2]>,
    starts: [usize; Symbols::SHARDS],
    lens: [usize; Symbols::SHARDS],
    whys: Vec<Why>,
    cells: Vec<Cells>,
}

/// What a column must hold: symbols, GOT cells, and the targets that may go through a PLT
/// stub.
#[derive(Default)]
struct Wants {
    syms: Vec<Sym>,
    got: Vec<(GotKind, Sym)>,
    plt: Vec<Sym>,
}

impl Wants {
    fn join(mut self, other: Wants) -> Wants {
        self.syms.extend(other.syms);
        self.got.extend(other.got);
        self.plt.extend(other.plt);
        self
    }
}

impl Column {
    /// A column with a slot for each of `syms` (and the ids below them in their shards), all
    /// unset, and room for `cells` symbols' cells.
    fn sized(syms: &[Sym], cells: usize) -> Column {
        let mut lens = [0usize; Symbols::SHARDS];
        for sym in syms {
            let (shard, at) = Symbols::split(*sym);
            lens[shard] = lens[shard].max(at + 1);
        }
        let mut starts = [0usize; Symbols::SHARDS];
        let mut total = 0;
        for (start, len) in starts.iter_mut().zip(lens) {
            *start = total;
            total += len;
        }
        let mut slots = Vec::with_capacity(total);
        slots.resize(total, [0, UNSET]);
        Column {
            slots,
            starts,
            lens,
            // alloc: none until a symbol has no value (an error, or a removed function's unwind record)
            whys: Vec::new(),
            cells: Vec::with_capacity(cells),
        }
    }

    fn build(resolver: &Resolver<'_>, mut wants: Wants) -> Column {
        radix::sort_copy_by_key(&mut wants.syms, |it| it.0);
        wants.syms.dedup();
        radix::sort_copy_by_key(&mut wants.got, |&(kind, sym)| radix::pack_32(kind as u32, sym.0));
        wants.got.dedup();
        radix::sort_copy_by_key(&mut wants.plt, |it| it.0);
        wants.plt.dedup();
        let mut values = Vec::with_capacity(wants.syms.len());
        par::extend(&mut values, &wants.syms, |it| resolver.value(*it));
        let mut column = Column::sized(&wants.syms, wants.got.len() + wants.plt.len());
        for (sym, value) in wants.syms.iter().zip(values) {
            let slot = match value {
                Ok(Value::Image(it)) => [it, IMAGE],
                Ok(Value::Absolute(it)) => [it, ABSOLUTE],
                Ok(Value::Import(it)) => [u64::from(it), IMPORT],
                Ok(Value::Tls(it)) => [it, TLS],
                Ok(Value::TpOff(it)) => [it as u64, TP_OFF],
                Err(why) => {
                    column.whys.push(why);
                    [column.whys.len() as u64 - 1, WHY]
                }
            };
            *column.slot_mut(*sym) = slot;
        }
        let mut got = Vec::with_capacity(wants.got.len());
        par::extend(&mut got, &wants.got, |(kind, sym)| resolver.cell(*kind, *sym).unwrap_or(NONE));
        for ((kind, sym), at) in wants.got.iter().zip(got) {
            column.cells_mut(*sym).got[*kind as usize] = at;
        }
        wants.plt.retain(|it| column.is_import(*it));
        for sym in wants.plt {
            column.cells_mut(sym).plt = resolver.plt(sym).unwrap_or(NONE);
        }
        column
    }

    #[inline]
    fn slot(&self, sym: Sym) -> [u64; 2] {
        let (shard, at) = Symbols::split(sym);
        if at < self.lens[shard] { self.slots[self.starts[shard] + at] } else { [0, UNSET] }
    }

    fn slot_mut(&mut self, sym: Sym) -> &mut [u64; 2] {
        let (shard, at) = Symbols::split(sym);
        assert!(at < self.lens[shard], "a symbol the column was not built for");
        &mut self.slots[self.starts[shard] + at]
    }

    fn cells_mut(&mut self, sym: Sym) -> &mut Cells {
        let meta = self.slot(sym)[1];
        let at = match meta >> 32 {
            0 => {
                self.cells.push(NO_CELLS);
                let at = self.cells.len();
                self.slot_mut(sym)[1] = meta | (at as u64) << 32;
                at
            }
            at => at as usize,
        };
        &mut self.cells[at - 1]
    }

    /// A symbol's value, or the index of why it has none (`u32::MAX`: not resolved, a bug).
    #[inline]
    fn value(&self, sym: Sym) -> Result<Value, u32> {
        let [value, meta] = self.slot(sym);
        match meta & 0xff {
            IMAGE => Ok(Value::Image(value)),
            ABSOLUTE => Ok(Value::Absolute(value)),
            IMPORT => Ok(Value::Import(value as u32)),
            TLS => Ok(Value::Tls(value)),
            TP_OFF => Ok(Value::TpOff(value as i64)),
            WHY => Err(value as u32),
            _ => Err(u32::MAX),
        }
    }

    /// Whether the symbol is an import, its cells or none.
    #[inline]
    fn is_import(&self, sym: Sym) -> bool {
        matches!(self.value(sym), Ok(Value::Import(_)))
    }

    #[inline]
    fn cells(&self, sym: Sym) -> Cells {
        match self.slot(sym)[1] >> 32 {
            0 => NO_CELLS,
            at => self.cells[at as usize - 1],
        }
    }

    /// Whether the symbol of `value`'s error `at` is in a piece the stage removed.
    fn removed(&self, at: u32) -> bool {
        matches!(self.whys.get(at as usize), Some(Why::Removed))
    }

    fn why(&self, describe: &dyn Fn(Sym) -> String, sym: Sym, at: u32) -> String {
        match self.whys.get(at as usize) {
            Some(why) => why.say_with(describe),
            // alloc: error path
            None => format!("{} was not resolved before relocation", describe(sym)),
        }
    }
}

/// Where a job's bytes come from before its fixups are applied.
#[derive(Clone, Copy)]
enum Source<'s> {
    Content(Content<'s>),
    /// A region's bytes at this offset, of this length (the piece where it was).
    Bytes(&'s CowBytes, usize, u64),
    /// Nothing: the field is zeros already.
    Zero,
}

/// A piece, or a run of its fixups: its bytes from `lo`, its fixups `fixups`.
struct Job<'f, 's> {
    piece: PieceId,
    lo: u64,
    fixups: Range<u32>,
    field: &'f mut [u8],
    source: Source<'s>,
}

/// wasm's `call`, `return_call`, `unreachable` and `nop` opcodes.
const CALL: u8 = 0x10;
const RETURN_CALL: u8 = 0x12;
const UNREACHABLE: u8 = 0x00;
const NOP: u8 = 0x01;

/// A piece is split into jobs of this many fixups when it has more.
const SPLIT: usize = 2048;

/// Where a piece's fixups split into jobs: each job's first byte and first fixup, when they
/// can (no format relocation, sorted, none overlapping the next); a single job otherwise.
fn split(fixups: &[Fixup]) -> impl Iterator<Item = (u64, u32)> + '_ {
    let whole = fixups.len() <= SPLIT
        || fixups.last().is_some_and(|it| matches!(it.apply, Apply::Native(_)))
        || fixups.windows(2).any(|pair| {
            matches!(pair[0].apply, Apply::Native(_))
                || pair[0].at + reloc::width(pair[0].apply) as u64 > pair[1].at
        });
    let rest = if whole { 0..0 } else { SPLIT..fixups.len() };
    std::iter::once((0, 0)).chain(rest.step_by(SPLIT).map(|at| (fixups[at].at, at as u32)))
}

/// What relocation left for the loader, per batch of jobs: each job's lists as ranges of
/// these.
#[derive(Default)]
struct Loader {
    rebase: Vec<u64>,
    binds: Vec<state::Bind>,
    /// Job, rebase range, binds range.
    records: Vec<(u32, u32, u32, u32, u32)>,
    /// The first failing job's error.
    error: Option<(u32, LinkError)>,
}

/// What every job reads: `Sync`.
struct Reloc<'s> {
    symbols: &'s Symbols,
    state: &'s State,
    column: &'s Column,
    writer: &'s Formats,
    arch: Arch,
    format: Format,
    base: u64,
    tls: Option<Tls>,
    /// [`State::tls_at`].
    tls_at: Option<i64>,
    got_by_library: bool,
    imports: &'s [Import],
    /// wasm: code addresses in function index order, after the imports.
    wasm: &'s [u64],
}

impl Reloc<'_> {
    fn run(&self, index: u32, job: &mut Job<'_, '_>, out: &mut Loader) {
        let (rebase, binds) = (out.rebase.len(), out.binds.len());
        if let Err(it) = self.job(job, out) {
            out.rebase.truncate(rebase);
            out.binds.truncate(binds);
            if out.error.as_ref().is_none_or(|(at, _)| index < *at) {
                out.error = Some((index, it));
            }
            return;
        }
        out.records.push((
            index,
            rebase as u32,
            out.rebase.len() as u32,
            binds as u32,
            out.binds.len() as u32,
        ));
    }

    fn job(&self, job: &mut Job<'_, '_>, out: &mut Loader) -> Result<(), LinkError> {
        let piece = self.state.piece(job.piece).expect("a live piece to write");
        let field = &mut *job.field;
        let symbols = self.symbols;
        let mismatch = |len: u64| {
            error(
                function_of(piece),
                // alloc: error path
                format!(
                    "{} has {len} bytes for a piece of {}",
                    symbols.describe(piece.sym),
                    piece.size
                ),
            )
        };
        match job.source {
            Source::Content(Content::Bytes(bytes)) => {
                if bytes.len() as u64 > piece.size {
                    return Err(mismatch(bytes.len() as u64));
                }
                let have = bytes.get(job.lo as usize..).unwrap_or(&[]);
                let n = have.len().min(field.len());
                field[..n].copy_from_slice(&have[..n]);
            }
            Source::Bytes(bytes, at, len) => {
                if len != piece.size {
                    return Err(mismatch(len));
                }
                let at = at + job.lo as usize;
                let n = bytes.len().saturating_sub(at).min(field.len());
                if n > 0 {
                    bytes.copy_to(at, &mut field[..n]);
                }
            }
            Source::Content(Content::Zero) | Source::Zero => {}
        }
        let what = piece.what();
        if what == What::Cells {
            return self.cells(piece, field, out);
        }
        if let What::Got { target, .. } = what {
            if matches!(self.column.value(target), Ok(Value::Import(_))) {
                field.fill(0);
                return Ok(());
            }
        }
        let unwind = matches!(what, What::Native { unwind: true, .. });
        let tls_base = self.tls.map_or(0, |it| it.addr);
        let fixups = &piece.fixups()[job.fixups.start as usize..job.fixups.end as usize];
        // Every fixup that can push to the loader's lists is an `Abs64`: room for them all
        // now, so no fixup allocates.
        let abs = fixups.iter().filter(|it| it.apply == Apply::Abs64).count();
        out.rebase.reserve(abs);
        out.binds.reserve(abs);
        let describe = |sym: Sym| named_in(symbols, &[self.state], self.writer, sym);
        let fail = |fixup: &Fixup, what: String| {
            error(
                function_of(piece),
                // alloc: error path
                format!("{:?} at {:#x} in {}: {what}", fixup.apply, fixup.at, describe(piece.sym)),
            )
        };
        // A value its field can't hold, or a branch no thunk reaches: a reach, whose message
        // the stage gives the image's regions ([`crate::shape::explained`]).
        let past = |fixup: &Fixup, what: String| LinkError {
            refusal: Some(Refusal::FieldRange),
            ..fail(fixup, what)
        };
        for fixup in fixups {
            let p = piece.addr + fixup.at;
            let Some(place) =
                fixup.at.checked_sub(job.lo).and_then(|at| field.get_mut(at as usize..))
            else {
                return Err(fail(fixup, "past the piece's end".into()));
            };
            let value = match self.column.value(fixup.target) {
                Ok(value) => value,
                Err(at) if unwind && self.column.removed(at) => {
                    let width = reloc::width(fixup.apply).min(place.len());
                    place[..width].fill(0);
                    continue;
                }
                Err(at) => return Err(fail(fixup, self.column.why(&describe, fixup.target, at))),
            };
            let s = match value {
                Value::Image(s) | Value::Absolute(s) => s,
                Value::Tls(offset) => tls_base + offset,
                Value::Import(_) | Value::TpOff(_) => 0,
            };
            let tls_kind = reloc::is_tls(fixup.apply);
            let tp = match (value, self.tls_at) {
                (Value::TpOff(tp), _) if tls_kind => Some(tp),
                (Value::TpOff(_), _) => {
                    return Err(fail(
                        fixup,
                        // alloc: error path
                        format!(
                            "{} is a thread-local of the image this one is linked beside, which only a \
                             thread-local relocation reaches",
                            symbols.describe(fixup.target)
                        ),
                    ));
                }
                (Value::Image(_) | Value::Tls(_), Some(at)) if tls_kind => {
                    let Some(offset) = s.checked_sub(tls_base).filter(|_| self.tls.is_some())
                    else {
                        return Err(fail(
                            fixup,
                            "a thread-local relocation to what is not thread-local".into(),
                        ));
                    };
                    Some(at + offset as i64)
                }
                _ => None,
            };
            let m = match fixup.minus {
                Some(minus) => match self.column.value(minus) {
                    Ok(Value::Image(m) | Value::Absolute(m)) => m,
                    Ok(Value::Tls(offset)) => tls_base + offset,
                    Ok(Value::TpOff(_)) => {
                        return Err(fail(
                            fixup,
                            "the difference of another image's thread-local".into(),
                        ));
                    }
                    Ok(Value::Import(_)) => {
                        return Err(fail(fixup, "the difference of an import".into()));
                    }
                    Err(at) => return Err(fail(fixup, self.column.why(&describe, minus, at))),
                },
                None => 0,
            };
            if let Apply::Native(r_type) = fixup.apply {
                let s = match (self.format, r_type) {
                    (
                        Format::Wasm,
                        reloc::WASM_FUNCTION_INDEX_LEB
                        | reloc::WASM_FUNCTION_INDEX_I32
                        | reloc::WASM_TABLE_INDEX_SLEB
                        | reloc::WASM_TABLE_INDEX_I32
                        | reloc::WASM_TABLE_INDEX_SLEB64
                        | reloc::WASM_TABLE_INDEX_I64,
                    ) => {
                        let slot = u64::from(reloc::wasm_is_table_slot(r_type));
                        match value {
                            Value::Import(at) => u64::from(at) + slot,
                            Value::Image(s) => match self.wasm.binary_search(&s) {
                                Ok(at) => (self.imports.len() + at) as u64 + slot,
                                Err(_) => return Err(fail(fixup, "not a function's start".into())),
                            },
                            Value::Absolute(0) if slot == 1 => 0,
                            // A direct call of an undefined weak function, which C guards
                            // with a test of its address (wasi-libc's preopens.c calls
                            // `__wasilibc_find_relpath_alloc` only when `chdir` is linked):
                            // the call becomes a trap, as wasm-ld's `undefined_weak:` stub
                            // traps, `unreachable` and the index's five bytes as `nop`s.
                            // `unreachable` takes any arguments and gives any results.
                            Value::Absolute(0)
                                if r_type == reloc::WASM_FUNCTION_INDEX_LEB && place.len() >= 5 =>
                            {
                                // The opcode is the piece's byte before the index: a piece with
                                // a format relocation is never cut into jobs (`split`), so this
                                // job is the whole piece and `field` its bytes.
                                if job.lo != 0 {
                                    return Err(fail(
                                        fixup,
                                        // alloc: error path
                                        format!(
                                            "{} is an undefined weak function called from a job cut at {:#x} \
                                             of its piece, whose call opcode is before the cut",
                                            symbols.describe(fixup.target),
                                            job.lo
                                        ),
                                    ));
                                }
                                let at = fixup.at as usize;
                                match at.checked_sub(1).map(|it| field[it]) {
                                    // `call` and `return_call` alike: `unreachable` ends the
                                    // function either way.
                                    Some(CALL | RETURN_CALL) => {
                                        field[at - 1] = UNREACHABLE;
                                        field[at..at + 5].fill(NOP);
                                        continue;
                                    }
                                    // The same call rewritten by the link before, whose bytes
                                    // a relink starts from (`Source::Bytes`).
                                    Some(UNREACHABLE)
                                        if field.get(at..at + 5) == Some(&[NOP; 5][..]) =>
                                    {
                                        continue;
                                    }
                                    _ => {
                                        return Err(fail(
                                            fixup,
                                            // alloc: error path
                                            format!(
                                                "{} is an undefined weak function, reached by an index \
                                                 that is no call's, which has no null function to go to",
                                                symbols.describe(fixup.target)
                                            ),
                                        ));
                                    }
                                }
                            }
                            _ => {
                                return Err(fail(
                                    fixup,
                                    // alloc: error path
                                    format!(
                                        "{} is not defined, and a call by index has no null function to go to",
                                        symbols.describe(fixup.target)
                                    ),
                                ));
                            }
                        }
                    }
                    _ if matches!(value, Value::Import(_)) => {
                        return Err(fail(fixup, "a format relocation to an import".into()));
                    }
                    _ => s,
                };
                self.writer
                    .apply(r_type, place, p, s, fixup.addend)
                    .map_err(|it| fail(fixup, it))?;
                continue;
            }
            let kind = reloc::got_kind(fixup.apply);
            let g = match kind {
                Some(kind) => {
                    let target = got_target(symbols, fixup);
                    match self.column.cells(target).got[kind as usize] {
                        // alloc: error path
                        NONE => {
                            return Err(fail(
                                fixup,
                                format!("no GOT cell of {}", symbols.describe(target)),
                            ));
                        }
                        at => at,
                    }
                }
                None => 0,
            };
            let mut slid = matches!(value, Value::Image(_));
            let mut plt = false;
            let s = match value {
                Value::Import(at)
                    if through_plt(fixup.apply, self.got_by_library, self.imports, at) =>
                {
                    slid = true;
                    plt = true;
                    match self.column.cells(fixup.target).plt {
                        // alloc: error path
                        NONE => {
                            return Err(fail(
                                fixup,
                                format!("no PLT stub of {}", symbols.describe(fixup.target)),
                            ));
                        }
                        at => at,
                    }
                }
                Value::Import(_) if self.got_by_library && kind.is_none() => {
                    return Err(fail(
                        fixup,
                        // alloc: error path
                        format!(
                            "{} is imported data, which is reached only through its import address table \
                             cell (`__imp_`)",
                            symbols.describe(fixup.target)
                        ),
                    ));
                }
                Value::Import(at) if fixup.apply == Apply::Abs64 => {
                    let Some(place) = place.get_mut(..8) else {
                        // alloc: error path
                        return Err(fail(
                            fixup,
                            Bad::PastEnd { apply: fixup.apply, need: 8 }.to_string(),
                        ));
                    };
                    place.fill(0);
                    out.binds.push((fixup.at, at, fixup.addend));
                    continue;
                }
                Value::Import(_) if kind.is_some() => 0,
                Value::Import(_) => {
                    return Err(fail(
                        fixup,
                        // alloc: error path
                        format!(
                            "{} is imported: reaching it this way needs a copy relocation, which blaze \
                             does not make; reach it through the GOT",
                            symbols.describe(fixup.target)
                        ),
                    ));
                }
                // A branch to a weak name nothing defines ([`thunk::weak_branch`]).
                Value::Absolute(0) if piece.weak().binary_search(&fixup.target).is_ok() => {
                    thunk::weak_branch(fixup.apply, p).unwrap_or(s)
                }
                _ => s,
            };
            if fixup.apply == Apply::Abs64 && slid {
                out.rebase.push(fixup.at);
            }
            let (s, a) = if slid
                && thunk::extends(fixup.apply)
                && !thunk::reaches(p, s.wrapping_add(fixup.addend as u64), thunk::REACH)
            {
                self.thunk(fixup, plt, p, s).map_err(|it| past(fixup, it))?
            } else {
                (s, fixup.addend)
            };
            let ctx = Ctx {
                arch: self.arch,
                format: self.format,
                s,
                a,
                p,
                g,
                m,
                base: self.base,
                tls: self.tls,
                tp,
            };
            reloc::apply(fixup.apply, &ctx, place).map_err(|it| match it.reach() {
                // alloc: error path
                true => past(fixup, it.to_string()),
                // alloc: error path
                false => fail(fixup, it.to_string()),
            })?;
        }
        Ok(())
    }

    /// Where a branch at `p` out of reach of its target (at `s`, its PLT stub when `plt`) goes
    /// instead: the nearest thunk of it in reach ([`thunk::through`]), with no addend. An error
    /// naming the target when there is none.
    fn thunk(&self, fixup: &Fixup, plt: bool, p: u64, s: u64) -> Result<(u64, i64), String> {
        let target =
            if plt { self.symbols.find(SymKey::Plt(fixup.target)) } else { Some(fixup.target) };
        if let Some(at) =
            target.and_then(|it| thunk::through(self.symbols, self.state, it, fixup.addend, p))
        {
            return Ok((at, 0));
        }
        let to = s.wrapping_add(fixup.addend as u64);
        // alloc: error path
        Err(format!(
            "a branch to {}{} at {to:#x}, {} away: past its 128 MiB reach, and no thunk of it is in reach \
             (the image has {} thunk islands)",
            named_in(self.symbols, &[self.state], self.writer, fixup.target),
            if plt { " through its PLT stub" } else { "" },
            thunk::away(p, to),
            self.state.islands,
        ))
    }

    /// The jump table's cells in a full link: each filled slot's body's address, rebased.
    fn cells(
        &self,
        piece: PieceRef<'_>,
        field: &mut [u8],
        out: &mut Loader,
    ) -> Result<(), LinkError> {
        out.rebase.reserve(field.len() / 8);
        for (slot, body) in self.state.filled_cells() {
            let at = slot as usize * 8;
            let addr = self
                .state
                .addr_of(body)
                // alloc: error path
                .ok_or_else(|| {
                    error(None, format!("{} is not defined", self.symbols.describe(body)))
                })?;
            let cell = field.get_mut(at..at + 8).ok_or_else(|| {
                // alloc: error path
                error(None, format!("slot {slot} is past the jump table's {} bytes", piece.size))
            })?;
            cell.copy_from_slice(&addr.to_le_bytes());
            out.rebase.push(at as u64);
        }
        Ok(())
    }
}

/// The most regions [`write_order`] has room for: the region index gets 31 bits.
const MAX_REGIONS: usize = 1 << 31;

/// The order a full link writes pieces in: by region, then address; at one address the
/// zero-sized pieces first, then by piece. [`Stage::put`] lays a piece with no bytes at the
/// address the next one starts at, and piece IDs follow creation, not layout: written after
/// its neighbour, an empty piece's offset is below what the neighbour took and reads as an
/// overlap. With the empty pieces first every piece starts at or past the end of the one
/// written before it, and a real overlap is still one. Unique, so the order is one whatever
/// the input's.
fn write_order(region: u32, addr: u64, filled: bool, id: PieceId) -> u128 {
    debug_assert!((region as usize) < MAX_REGIONS);
    (u128::from(region) << 97)
        | (u128::from(addr) << 33)
        | (u128::from(filled) << 32)
        | u128::from(id)
}

/// A full link's piece that starts before the end of the one written before it in its
/// region: both named, with their sizes, alignments and offsets in the region.
#[cold]
fn overlap(
    symbols: &Symbols,
    state: &State,
    space: &Space,
    piece: PieceRef<'_>,
    last: Option<PieceId>,
) -> LinkError {
    let class = &space.region.class;
    let at = |it: PieceRef<'_>| {
        let start = it.addr - space.region.addr;
        // alloc: error path
        format!(
            "{} ({} bytes, align {}) at [{class:?}+{start:#x}, {class:?}+{:#x})",
            symbols.describe(it.sym),
            it.size,
            it.align(),
            start + it.size
        )
    };
    let before = last.and_then(|it| state.piece(it));
    let what = match before {
        // alloc: error path
        Some(it) => format!("{} overlaps the piece before it, {}", at(piece), at(it)),
        // alloc: error path
        None => format!(
            "{} overlaps the piece before it, though nothing was written before it",
            at(piece)
        ),
    };
    error(function_of(piece).or_else(|| before.and_then(function_of)), what)
}

/// Where a piece goes when its region has bytes: not a zeroed or a synthetic region.
fn has_bytes(space: &Space) -> bool {
    !space.zero && !matches!(space.region.class, Class::Synthetic(_))
}

impl<'a> Stage<'a> {
    /// The function indexes of wasm code by code address: the imports first (every wasm
    /// import is a function), then the pieces of the code space and the functions the
    /// writer synthesizes (one at the start of each of its code regions), by address.
    fn wasm_code(&self, regions: &[Region]) -> &'a [u64] {
        let mut code = collect_in(
            self.new
                .live_pieces()
                .filter(|it| regions.get(it.region as usize).is_some_and(|r| r.prot.exec))
                .map(|it| it.addr)
                .chain(
                    regions
                        .iter()
                        .filter(|it| it.prot.exec && matches!(it.class, Class::Synthetic(_)))
                        .map(|it| it.addr),
                ),
            self.arena,
        );
        radix::sort(&mut code);
        code.dedup();
        code.leak()
    }

    /// Where a piece's bytes come from: what the stage made it, else (in a full link or
    /// where it moved) its old bytes, else (written in place) its bytes now.
    fn source(&self, piece: PieceRef<'_>) -> Source<'_> {
        if let Some(content) = self.contents.get(piece.id) {
            return Source::Content(content);
        }
        let Some(old) = self.old.piece(piece.id) else { return Source::Zero };
        let moved = self.full || old.addr != piece.addr || old.region != piece.region;
        let (state, at) = if moved { (self.old, old) } else { (&self.new, piece) };
        match state.spaces.get(at.region as usize) {
            Some(space) if !space.zero && !at.zero() => {
                Source::Bytes(&space.bytes, (at.addr - space.region.addr) as usize, at.size)
            }
            _ => Source::Zero,
        }
    }

    /// Copies and relocates every piece to write, in parallel.
    pub fn relocate_all(&mut self) -> Result<(), LinkError> {
        let full = self.full;
        let short =
            |space: &Space| has_bytes(space) && (space.bytes.len() as u64) < space.region.size;
        if !full && self.new.spaces.iter().any(short) {
            for space in Arc::make_mut(&mut self.new.spaces).iter_mut().filter(|it| short(&**it)) {
                space.bytes.resize(space.region.size as usize);
            }
        }
        let arena = self.arena;
        let mut ids: FrameVec<'a, (u32, u64, bool, PieceId)> =
            Vec::with_capacity_in(self.write.len(), arena);
        ids.extend(
            self.write.iter().filter_map(|id| {
                self.new.piece(id).map(|it| (it.region, it.addr, it.size != 0, id))
            }),
        );
        if self.new.spaces.len() > MAX_REGIONS {
            // alloc: error path
            return Err(error(
                None,
                format!("{} regions, past the writer's {MAX_REGIONS}", self.new.spaces.len()),
            ));
        }
        radix::sort_copy_by_key(&mut ids, |&(region, addr, filled, id)| {
            write_order(region, addr, filled, id)
        });
        let regions = self.new.regions();
        let mut buffers: Vec<Vec<u8>> =
            Vec::with_capacity(if full { self.new.spaces.len() } else { 0 });
        if full {
            for space in self.new.spaces.iter() {
                // alloc: a full link's regions, one zeroed buffer each, kept as the image's bytes
                buffers.push(if has_bytes(space) {
                    vec![0; space.region.size as usize]
                } else {
                    Vec::new()
                });
            }
        }
        let mut scratch: &'a mut [u8] = &mut [];
        let mut cells: Option<PieceId> = None;
        let mut owners: FrameVec<'a, PieceId> = Vec::with_capacity_in(ids.len(), arena);
        let mut back: FrameVec<'a, (u32, usize, Range<usize>)> = Vec::new_in(arena);
        let loaders = {
            let state = &self.new;
            let symbols: &Symbols = self.symbols;
            let resolver = Resolver { symbols, state, regions: &regions, base: self.plan.base };
            let got_by_library = self.plan.got_by_library;
            let grain = ids.len().div_ceil(par::threads() * 8).max(1);
            let wants = par::map(par::chunks(&ids, grain), |ids| {
                ids.iter().fold(Wants::default(), |mut wants, &(_, _, _, id)| {
                    let piece = state.piece(id).expect("live");
                    let fixups = piece.fixups();
                    // At most a target, a minus and a GOT target each: one growth per piece at
                    // most, never one per fixup.
                    wants.syms.reserve(1 + 3 * fixups.len());
                    wants.got.reserve(fixups.len());
                    wants.plt.reserve(fixups.len());
                    if let What::Got { target, .. } = piece.what() {
                        wants.syms.push(target);
                    }
                    for fixup in fixups {
                        wants.syms.push(fixup.target);
                        wants.syms.extend(fixup.minus);
                        if let Some(kind) = reloc::got_kind(fixup.apply) {
                            let target = got_target(symbols, fixup);
                            wants.syms.push(target);
                            wants.got.push((kind, target));
                        }
                        if reloc::is_branch(fixup.apply)
                            || (got_by_library
                                && matches!(
                                    fixup.apply,
                                    Apply::Abs64 | Apply::ImageRel32 | Apply::Rel32
                                ))
                        {
                            wants.plt.push(fixup.target);
                        }
                    }
                    wants
                })
            })
            .into_iter()
            .fold(Wants::default(), Wants::join);
            let column = Column::build(&resolver, wants);
            let wasm: &[u64] =
                if self.output.format == Format::Wasm { self.wasm_code(&regions) } else { &[] };
            if !full {
                let len: u64 = ids
                    .iter()
                    .filter_map(|it| {
                        let piece = state.piece(it.3)?;
                        let space = state.spaces.get(piece.region as usize)?;
                        (has_bytes(space) && piece.what() != What::Cells).then_some(piece.size)
                    })
                    .sum();
                scratch = arena.alloc_slice_fill_with(len as usize, |_| 0);
            }
            let mut rest = collect_in(buffers.iter_mut().map(Vec::as_mut_slice), arena);
            let taken = arena.alloc_slice_fill_with(state.spaces.len(), |_| 0u64);
            // The piece written last in each region: what an overlap names.
            let written = arena.alloc_slice_fill_with(state.spaces.len(), |_| None::<PieceId>);
            let mut free: &mut [u8] = &mut *scratch;
            let mut used = 0usize;
            let mut jobs: Vec<Job<'_, '_>> = Vec::with_capacity(ids.len());
            for &(region, _, _, id) in ids.iter() {
                let piece = state.piece(id).expect("live");
                let Some(space) = state.spaces.get(region as usize) else {
                    // alloc: error path
                    return Err(error(
                        function_of(piece),
                        format!("{} is in no region", symbols.describe(piece.sym)),
                    ));
                };
                if !has_bytes(space) {
                    if piece.fixups().is_empty() {
                        continue;
                    }
                    return Err(error(
                        function_of(piece),
                        // alloc: error path
                        format!(
                            "{} has relocations in a zeroed region",
                            symbols.describe(piece.sym)
                        ),
                    ));
                }
                if !full && piece.what() == What::Cells {
                    cells = Some(id);
                    continue;
                }
                let offset = piece.addr - space.region.addr;
                let field: &mut [u8] = if full {
                    let Some(skip) = offset.checked_sub(taken[region as usize]) else {
                        return Err(overlap(
                            symbols,
                            state,
                            space,
                            piece,
                            written[region as usize],
                        ));
                    };
                    written[region as usize] = Some(id);
                    let buffer = std::mem::take(&mut rest[region as usize]);
                    let (_, tail) = buffer.split_at_mut(skip as usize);
                    let (mine, after) = tail.split_at_mut(piece.size as usize);
                    taken[region as usize] = offset + piece.size;
                    rest[region as usize] = after;
                    mine
                } else {
                    let (mine, after) = std::mem::take(&mut free).split_at_mut(piece.size as usize);
                    free = after;
                    back.push((region, offset as usize, used..used + piece.size as usize));
                    used += piece.size as usize;
                    mine
                };
                let source = self.source(piece);
                let fixups = piece.fixups();
                let mut cuts = split(fixups).peekable();
                let mut field = field;
                while let Some((lo, first)) = cuts.next() {
                    let (hi, last) =
                        cuts.peek().map_or((piece.size, fixups.len() as u32), |it| (it.0, it.1));
                    let (mine, after) = std::mem::take(&mut field).split_at_mut((hi - lo) as usize);
                    field = after;
                    owners.push(id);
                    jobs.push(Job { piece: id, lo, fixups: first..last, field: mine, source });
                }
            }
            let reloc = Reloc {
                symbols,
                state,
                column: &column,
                writer: self.writer,
                arch: self.output.arch,
                format: self.output.format,
                base: self.plan.base,
                tls: state.tls,
                tls_at: state.tls_at,
                got_by_library,
                imports: &state.resolved.imports,
                wasm,
            };
            let grain = jobs.len().div_ceil(par::threads() * 8).max(1);
            let loaders: Vec<Loader> =
                par::map(par::enumerate(par::chunks_mut(&mut jobs, grain)), |(batch, jobs)| {
                    let mut out = Loader::default();
                    for (at, job) in jobs.iter_mut().enumerate() {
                        reloc.run((batch * grain + at) as u32, job, &mut out);
                    }
                    out
                }); // alloc: one loader per pool batch of jobs, a few per thread, never one per job
            loaders
        };
        let mut loaders = loaders;
        if let Some((_, it)) =
            loaders.iter_mut().filter_map(|it| it.error.take()).min_by_key(|it| it.0)
        {
            return Err(it);
        }
        if full {
            for (space, buffer) in Arc::make_mut(&mut self.new.spaces).iter_mut().zip(buffers) {
                if has_bytes(space) {
                    space.bytes = CowBytes::from_vec(buffer);
                }
            }
        } else if !back.is_empty() {
            let spaces = Arc::make_mut(&mut self.new.spaces);
            for (region, offset, range) in back {
                spaces[region as usize].bytes.write(offset, &scratch[range]);
            }
        }
        let mut records: FrameVec<'a, (u32, u32, u32, u32, u32, u32)> =
            Vec::with_capacity_in(loaders.iter().map(|it| it.records.len()).sum(), arena);
        for (at, loader) in loaders.iter().enumerate() {
            records
                .extend(loader.records.iter().map(|it| (it.0, at as u32, it.1, it.2, it.3, it.4)));
        }
        radix::sort_copy_by_key(&mut records, |it| it.0);
        let mut rebase: FrameVec<'a, u64> = Vec::new_in(arena);
        let mut binds: FrameVec<'a, state::Bind> = Vec::new_in(arena);
        let mut at = 0;
        while at < records.len() {
            let piece = owners[records[at].0 as usize];
            let end =
                at + records[at..].iter().take_while(|it| owners[it.0 as usize] == piece).count();
            let lists = |it: &(u32, u32, u32, u32, u32, u32)| {
                let loader = &loaders[it.1 as usize];
                (
                    &loader.rebase[it.2 as usize..it.3 as usize],
                    &loader.binds[it.4 as usize..it.5 as usize],
                )
            };
            if end - at == 1 {
                let (r, b) = lists(&records[at]);
                self.new.set_loader(piece, r, b);
            } else {
                rebase.clear();
                binds.clear();
                for it in &records[at..end] {
                    let (r, b) = lists(it);
                    rebase.extend_from_slice(r);
                    binds.extend_from_slice(b);
                }
                self.new.set_loader(piece, &rebase, &binds);
            }
            at = end;
        }
        if let Some(id) = cells {
            self.write_cells(id)?;
        }
        Ok(())
    }

    /// The jump table's cells in an incremental link: only the slots set, 8 bytes each.
    fn write_cells(&mut self, id: PieceId) -> Result<(), LinkError> {
        let piece = self.new.piece(id).expect("live");
        let region = piece.region as usize;
        let start = (piece.addr - self.new.spaces[region].region.addr) as usize;
        let mut writes: FrameVec<'a, (usize, [u8; 8])> =
            Vec::with_capacity_in(self.dirty_cells.len(), self.arena);
        for slot in self.dirty_cells.iter() {
            let Some(body) = self.new.cell(slot) else { continue };
            let addr = self
                .new
                .addr_of(body)
                // alloc: error path
                .ok_or_else(|| {
                    error(None, format!("{} is not defined", self.symbols.describe(body)))
                })?;
            if u64::from(slot) * 8 + 8 > piece.size {
                // alloc: error path
                return Err(error(
                    None,
                    format!("slot {slot} is past the jump table's {} bytes", piece.size),
                ));
            }
            writes.push((start + slot as usize * 8, addr.to_le_bytes()));
        }
        if !writes.is_empty() {
            let bytes = &mut Arc::make_mut(&mut self.new.spaces)[region].bytes;
            for (at, cell) in writes {
                bytes.write(at, &cell);
            }
        }
        if self.filled_slots_changed(id) {
            let rebase = collect_in(
                self.new.filled_cells().map(|(slot, _)| u64::from(slot) * 8),
                self.arena,
            );
            self.new.set_loader(id, &rebase, &[]);
        }
        Ok(())
    }

    /// Whether the cells piece `id`'s rebase list (every filled slot's cell, `filled_cells`)
    /// may differ from the last link's: a slot this stage set or emptied went from empty to
    /// filled or back, or the piece is new. A slot that held a body and holds another keeps
    /// its entry, so the common edit (a body moved) costs its dirty slots, not every slot.
    fn filled_slots_changed(&self, id: PieceId) -> bool {
        let (old, new) = (self.old, &self.new);
        if self.fresh.contains(id) || old.piece(id).is_none() {
            return !self.dirty_cells.is_empty() || !self.emptied.is_empty();
        }
        let flipped = |slot: u32| old.cell(slot).is_some() != new.cell(slot).is_some();
        self.dirty_cells.iter().any(flipped) || self.emptied.iter().any(|slot| flipped(*slot))
    }
}

/// An entry of a list the writer reads, by its address.
pub(crate) trait Addr {
    fn addr(&self) -> u64;
}

impl Addr for u64 {
    fn addr(&self) -> u64 {
        *self
    }
}

impl Addr for Bind {
    fn addr(&self) -> u64 {
        self.addr
    }
}

impl Addr for GotEntry {
    fn addr(&self) -> u64 {
        self.addr
    }
}

impl Addr for Function {
    fn addr(&self) -> u64 {
        self.addr
    }
}

/// A list the writer reads, in address order (ties by piece), and the piece each entry is
/// of. Cloning it shares it.
#[derive(Clone, Debug)] // clone: two `Arc`s, reference counts only
pub(crate) struct Owned<T> {
    pub items: Arc<Vec<T>>,
    owners: Arc<Vec<PieceId>>,
}

impl<T> Default for Owned<T> {
    fn default() -> Owned<T> {
        Owned { items: Arc::default(), owners: Arc::default() }
    }
}

impl<T: Addr + Clone + Send + Sync> Owned<T> {
    fn build(mut entries: Vec<(PieceId, T)>) -> Owned<T> {
        radix::sort_by_key(&mut entries, |(id, it)| radix::pack_64(it.addr(), u64::from(*id)));
        let (owners, items) = entries.into_iter().unzip();
        // alloc: once per list per full link; the state shares the list
        Owned { items: Arc::new(items), owners: Arc::new(owners) }
    }

    /// The list without the entries of `drop`'s pieces, and with `entries`. Each run it
    /// replaces is pushed to `runs`, unless the run's new entries equal its old ones.
    ///
    /// The plan costs the change: the dropped entries come from one parallel scan of the
    /// owners, each new entry's place from a binary search. Then the kept entries between the
    /// runs are copied in parallel. A one-function edit so costs a copy of the list at memory
    /// bandwidth, where a serial walk cost about 5 ns an entry. Same lists and runs as
    /// [`Owned::splice_serial`], the reference the tests hold it to.
    fn splice(
        &self,
        drop: &IdSet,
        mut entries: Vec<(PieceId, T)>,
        runs: &mut FrameVec<'_, Splice>,
    ) -> Owned<T>
    where
        T: PartialEq,
    {
        if drop.is_empty() && entries.is_empty() {
            // clone: two reference counts; the list is shared, not copied
            return self.clone();
        }
        radix::sort_by_key(&mut entries, |(id, it)| radix::pack_64(it.addr(), u64::from(*id)));
        let (old, old_owners) = (&self.items[..], &self.owners[..]);
        // alloc: once per list a stage changes, the dropped entries' indexes
        let dropped: Vec<u32> = if drop.is_empty() {
            Vec::new() // alloc: empty, which allocates nothing
        } else {
            par::filter_map(par::enumerate(old_owners), |(at, owner)| {
                drop.contains(*owner).then_some(at as u32)
            }) // alloc: as above
        };
        let gaps = gaps(old, old_owners, &dropped, &entries);
        let len = old.len() - dropped.len() + entries.len();
        let mut items = Vec::with_capacity(len);
        let mut owners = Vec::with_capacity(len);
        let mut entries = entries.into_iter();
        let mut from = 0;
        for gap in &gaps {
            extend_par(&mut items, &old[from..gap.old.start]);
            extend_par(&mut owners, &old_owners[from..gap.old.start]);
            let at = items.len();
            for (id, it) in entries.by_ref().take(gap.new) {
                items.push(it);
                owners.push(id);
            }
            // clone: a range, two words
            run(&old[gap.old.clone()], &items[at..], gap.old.start, at, runs);
            from = gap.old.end;
        }
        extend_par(&mut items, &old[from..]);
        extend_par(&mut owners, &old_owners[from..]);
        debug_assert_eq!(items.len(), len);
        // alloc: once per list a stage changes; the state shares the list
        Owned { items: Arc::new(items), owners: Arc::new(owners) }
    }

    /// [`Owned::splice`] as one serial pass: the reference its tests hold it to.
    #[cfg(test)]
    fn splice_serial(
        &self,
        drop: &IdSet,
        mut entries: Vec<(PieceId, T)>,
        runs: &mut FrameVec<'_, Splice>,
    ) -> Owned<T>
    where
        T: PartialEq,
    {
        if drop.is_empty() && entries.is_empty() {
            // clone: two reference counts
            return self.clone();
        }
        radix::sort_by_key(&mut entries, |(id, it)| radix::pack_64(it.addr(), u64::from(*id)));
        let mut items = Vec::with_capacity(self.items.len() + entries.len());
        let mut owners = Vec::with_capacity(items.capacity());
        let mut new = entries.into_iter().peekable();
        // Where the open run started: in the old list, in the new one.
        let mut open: Option<(usize, usize)> = None;
        for (at, (item, owner)) in self.items.iter().zip(self.owners.iter()).enumerate() {
            if drop.contains(*owner) {
                open.get_or_insert((at, items.len()));
                continue;
            }
            while let Some((id, it)) =
                new.next_if(|(id, it)| (it.addr(), *id) < (item.addr(), *owner))
            {
                open.get_or_insert((at, items.len()));
                items.push(it);
                owners.push(id);
            }
            if let Some((old, from)) = open.take() {
                run(&self.items[old..at], &items[from..], old, from, runs);
            }
            // clone: an entry is addresses and ids, and a function's name an `Arc<str>`: no allocation
            items.push(item.clone());
            owners.push(*owner);
        }
        for (id, it) in new {
            open.get_or_insert((self.items.len(), items.len()));
            items.push(it);
            owners.push(id);
        }
        if let Some((old, from)) = open {
            run(&self.items[old..], &items[from..], old, from, runs);
        }
        // alloc: the two shared lists the writers and the next link borrow, once per splice
        Owned { items: Arc::new(items), owners: Arc::new(owners) }
    }
}

/// Below this many entries a list's scan or copy runs on one thread.
const SPLICE_GRAIN: usize = 1 << 14;

/// Functions a task of [`Stage::debug`] makes the rows of.
const DEBUG_CHUNK: usize = 1024;

/// `rows` cut before every [`DEBUG_CHUNK`]th function's first row, `starts` being where each
/// function's rows start and, last, where they end.
fn debug_chunks<'r>(
    mut rows: &'r mut [crate::dwarf::Row],
    starts: &[usize],
) -> Vec<&'r mut [crate::dwarf::Row]> {
    let functions = starts.len() - 1;
    // alloc: once per stage that makes debug info, a slice per task
    let mut out = Vec::with_capacity(functions.div_ceil(DEBUG_CHUNK));
    for from in (0..functions).step_by(DEBUG_CHUNK) {
        let to = (from + DEBUG_CHUNK).min(functions);
        let (mine, rest) = std::mem::take(&mut rows).split_at_mut(starts[to] - starts[from]);
        out.push(mine);
        rows = rest;
    }
    out
}

/// `from` cloned onto `out`, in parallel when it is long: `par` writes a source's
/// items in place, in order.
fn extend_par<T: Clone + Send + Sync>(out: &mut Vec<T>, from: &[T]) {
    if from.len() < SPLICE_GRAIN {
        out.extend_from_slice(from);
    } else {
        par::extend(out, from, |it| it.clone());
    }
}

/// One run a splice replaces: `old` in the last list (its dropped entries, consecutive), and
/// the next `new` of the sorted new entries, which go where it was.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Gap {
    old: Range<usize>,
    new: usize,
}

/// The runs a splice replaces, in order: each the dropped entries between two kept ones and
/// the new entries whose (address, piece) falls between those two. `dropped` is sorted, and
/// `entries` sorted by (address, piece); a new entry never ties a kept one, whose piece is
/// not dropped. Costs the dropped entries plus a binary search per new one.
fn gaps<T: Addr>(
    old: &[T],
    owners: &[PieceId],
    dropped: &[u32],
    entries: &[(PieceId, T)],
) -> Vec<Gap> {
    // The dropped entries as blocks of consecutive indexes; each block ends at a kept entry
    // or the list's end.
    // alloc: once per list a stage changes, O(dropped)
    let mut blocks: Vec<Range<usize>> = Vec::new();
    for &at in dropped {
        let at = at as usize;
        match blocks.last_mut() {
            Some(last) if last.end == at => last.end += 1,
            _ => blocks.push(at..at + 1),
        }
    }
    let key = |at: usize| (old[at].addr(), owners[at]);
    // alloc: once per list a stage changes, O(dropped + new)
    let mut out: Vec<Gap> = Vec::with_capacity(blocks.len() + entries.len());
    let mut block = 0;
    for (id, it) in entries {
        let want = (it.addr(), *id);
        let (mut lo, mut hi) = (0, old.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if key(mid) < want {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // The kept entry it goes before: past a block of dropped ones it lands in.
        while blocks.get(block).is_some_and(|it| it.end < lo) {
            // clone: a range, two words
            out.push(Gap { old: blocks[block].clone(), new: 0 });
            block += 1;
        }
        let end = match (out.last(), blocks.get(block)) {
            // Inside (or right after) the block the last gap already took.
            (Some(last), _) if lo <= last.old.end => last.old.end,
            (_, Some(it)) if it.start <= lo => it.end,
            _ => lo,
        };
        match out.last_mut() {
            Some(last) if last.old.end == end => last.new += 1,
            _ => match blocks.get(block) {
                Some(it) if it.end == end => {
                    // clone: a range, two words
                    out.push(Gap { old: it.clone(), new: 1 });
                    block += 1;
                }
                _ => out.push(Gap { old: end..end, new: 1 }),
            },
        }
    }
    // clone: a range, two words
    out.extend(blocks[block..].iter().map(|it| Gap { old: it.clone(), new: 0 }));
    out
}

/// Pushes the run of `old` (from `old_at` in the last list) replaced by `new` (from `new_at`),
/// unless they are the same entries.
fn run<T: PartialEq>(
    old: &[T],
    new: &[T],
    old_at: usize,
    new_at: usize,
    runs: &mut FrameVec<'_, Splice>,
) {
    if old == new {
        return;
    }
    let span = |at: usize, len: usize| at as u32..(at + len) as u32;
    runs.push(Splice { old: span(old_at, old.len()), new: span(new_at, new.len()) });
}

/// The runs each list's splice replaced in a stage that spliced them: [`Delta`]'s lists.
pub(crate) struct Changes<'f> {
    pub functions: FrameVec<'f, Splice>,
    pub rebase: FrameVec<'f, Splice>,
    pub got: FrameVec<'f, Splice>,
    pub binds: FrameVec<'f, Splice>,
}

/// The one run that replaces all of a list rebuilt whole, of `old` entries then `new`.
fn whole(old: usize, new: usize, runs: &mut FrameVec<'_, Splice>) {
    if old + new > 0 {
        runs.push(Splice { old: 0..old as u32, new: 0..new as u32 });
    }
}

/// A number for each synthesis in the process ([`crate::model::Placed::synthesis`]); 0 is none.
static SYNTHESES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The next synthesis's number.
pub(crate) fn next_synthesis() -> u64 {
    SYNTHESES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The lists the writer reads, kept across links: [`State::lists`]. Each list is shared on
/// its own, so a link that changes none of one keeps it.
#[derive(Clone, Default, Debug)] // clone: `Owned`s and an `Arc`, reference counts only
pub(crate) struct Lists {
    pub rebase: Owned<u64>,
    pub binds: Owned<Bind>,
    pub got: Owned<GotEntry>,
    pub functions: Owned<Function>,
    /// The debug sections, by DWARF name.
    pub debug: Arc<[(&'static str, Vec<u8>)]>,
    pub totals: Totals,
}

/// Which lists to collect from pieces.
#[derive(Clone, Copy)]
struct Want {
    loader: bool,
    got: bool,
    functions: bool,
}

/// The entries of some pieces.
#[derive(Default)]
struct Batch {
    rebase: Vec<(PieceId, u64)>,
    binds: Vec<(PieceId, Bind)>,
    got: Vec<(PieceId, GotEntry)>,
    functions: Vec<(PieceId, Function)>,
}

impl Batch {
    fn join(mut self, other: Batch) -> Batch {
        self.rebase.extend(other.rebase);
        self.binds.extend(other.binds);
        self.got.extend(other.got);
        self.functions.extend(other.functions);
        self
    }
}

/// What making list entries reads: `Sync`.
struct Entries<'s> {
    resolver: Resolver<'s>,
    arch: Arch,
    tls: Option<Tls>,
    /// [`State::tls_at`].
    tls_at: Option<i64>,
    indexed_code: bool,
}

impl Entries<'_> {
    fn of(&self, piece: PieceRef<'_>, want: Want, out: &mut Batch) -> Result<(), LinkError> {
        let state = self.resolver.state;
        let symbols = self.resolver.symbols;
        let regions = self.resolver.regions;
        if want.loader {
            out.rebase.extend(piece.rebase().iter().map(|at| (piece.id, piece.addr + at)));
            out.binds.extend(piece.binds().iter().map(|(at, import, addend)| {
                (piece.id, Bind { addr: piece.addr + at, import: *import, addend: *addend })
            }));
        }
        match piece.what() {
            What::Got { kind, target } if want.got => {
                out.got.push((
                    piece.id,
                    GotEntry { addr: piece.addr, holds: self.holds(kind, target)? },
                ));
            }
            What::Function(function) if want.functions => {
                let info = state.function(function);
                let fde = info
                    .fde
                    .and_then(|it| state.home(it))
                    .and_then(|(p, _)| state.piece(p))
                    .map(|p| p.addr);
                let windows = match info.windows {
                    Some((unwind, handler, lsda)) => {
                        let at = |sym: Sym| -> Result<u64, LinkError> {
                            match self
                                .resolver
                                .value(sym)
                                .map_err(|it| error(Some(info.key), it.say(symbols)))?
                            {
                                Value::Image(s) => Ok(s),
                                _ => {
                                    Err(error(Some(info.key), "an unwind handler not in the image"))
                                }
                            }
                        };
                        Some(WindowsUnwind {
                            info: at(unwind)?,
                            handler: handler.map(at).transpose()?,
                            lsda: lsda.map(at).transpose()?,
                        })
                    }
                    None => None,
                };
                let name = state.debug_name(info.id).map(|it| symbols.str(it).into());
                out.functions.push((
                    piece.id,
                    Function { addr: piece.addr, size: piece.size, fde, windows, name },
                ));
            }
            What::Native { unwind: false, .. }
                if want.functions
                    && self.indexed_code
                    && regions.get(piece.region as usize).is_some_and(|it| it.prot.exec) =>
            {
                let name =
                    piece.defines().iter().find_map(|sym| symbols.name_of(*sym)).map(Into::into);
                out.functions.push((
                    piece.id,
                    Function { addr: piece.addr, size: piece.size, fde: None, windows: None, name },
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn holds(&self, kind: GotKind, target: Sym) -> Result<Holds, LinkError> {
        let value =
            self.resolver.value(target).map_err(|it| error(None, it.say(self.resolver.symbols)))?;
        let tls = self.tls;
        Ok(match (kind, value) {
            (GotKind::TlsGd | GotKind::TlsDesc, _) if self.tls_at.is_some() => {
                return Err(error(
                    None,
                    "a dynamic thread-local access (a `__tls_get_addr` pair or a TLS descriptor) in an image \
                     whose thread-locals are at a fixed place beside another's: build it with the \
                     initial-exec or local-exec TLS model",
                ));
            }
            (GotKind::Address, Value::Import(at)) => Holds::Import(at),
            (GotKind::Address, Value::Image(s) | Value::Absolute(s)) => Holds::Local(s),
            (GotKind::TpOff, Value::Import(at)) => Holds::ImportTpOff(at),
            (GotKind::TlsGd, Value::Import(at)) => Holds::TlsGd { import: Some(at), offset: 0 },
            (GotKind::TlsDesc, Value::Import(at)) => Holds::TlsDesc { import: Some(at), offset: 0 },
            (GotKind::TpOff, Value::TpOff(tp)) => Holds::TpOff(tp),
            (_, Value::TpOff(_)) => {
                return Err(error(None, "an address GOT cell of another image's thread-local"));
            }
            (kind, value) => {
                let tls = tls.ok_or_else(|| {
                    error(None, "a thread-local GOT cell in an image with no TLS")
                })?;
                let at = match value {
                    Value::Tls(offset) => offset,
                    Value::Image(s) => s
                        .checked_sub(tls.addr)
                        .ok_or_else(|| error(None, "a TLS cell of what isn't thread-local"))?,
                    _ => return Err(error(None, "a TLS cell of an absolute symbol")),
                };
                match kind {
                    GotKind::TpOff => Holds::TpOff(match self.tls_at {
                        Some(tls_at) => tls_at + at as i64,
                        // alloc: error path
                        None => reloc::tp_offset(self.arch, tls, tls.addr + at)
                            .map_err(|it| error(None, it.to_string()))?,
                    }),
                    GotKind::TlsGd => Holds::TlsGd { import: None, offset: at },
                    GotKind::TlsDesc => Holds::TlsDesc { import: None, offset: at },
                    GotKind::Address => unreachable!("matched above"),
                }
            }
        })
    }

    /// The entries of `ids`' live pieces, in parallel.
    fn collect(
        &self,
        ids: impl IntoIterator<Item = PieceId>,
        want: Want,
    ) -> Result<Batch, LinkError> {
        let state = self.resolver.state;
        // alloc: once per collection, the piece ids split into ordered batches
        let ids: Vec<PieceId> = ids.into_iter().collect();
        let grain = ids.len().div_ceil(par::threads() * 8).max(1);
        par::map(par::chunks(&ids, grain), |ids| {
            ids.iter().try_fold(Batch::default(), |mut out, &id| {
                if let Some(piece) = state.piece(id) {
                    self.of(piece, want, &mut out)?;
                }
                Ok(out)
            })
        })
        .into_iter()
        .try_fold(Batch::default(), |a, b: Result<Batch, LinkError>| Ok(a.join(b?)))
    }
}

/// What the writer reads besides the lists.
pub(crate) struct Parts {
    pub regions: Vec<Region>,
    pub exports: Vec<Export>,
    pub entry: Option<u64>,
    pub init: Option<u64>,
    pub fini: Option<u64>,
    pub id: [u8; 16],
}

impl<'a> Stage<'a> {
    /// The writer's lists for the image as placed: made from every piece in a full link,
    /// spliced in an incremental one, and then the runs each splice replaced (`None` when the
    /// lists were made from every piece).
    pub fn update_lists(&mut self) -> Result<Option<Changes<'a>>, LinkError> {
        let totals = self.totals();
        let regions = self.new.regions();
        let entries = Entries {
            resolver: Resolver {
                symbols: self.symbols,
                state: &self.new,
                regions: &regions,
                base: self.plan.base,
            },
            arch: self.output.arch,
            tls: self.new.tls,
            tls_at: self.new.tls_at,
            indexed_code: self.plan.indexed_code,
        };
        let all = Want { loader: true, got: true, functions: true };
        let arena = self.arena;
        let (mut lists, functions_changed, changes) = if self.full || !self.old.laid_out {
            let batch = entries.collect(0..self.new.piece_count(), all)?;
            let lists = Lists {
                rebase: Owned::build(batch.rebase),
                binds: Owned::build(batch.binds),
                got: Owned::build(batch.got),
                functions: Owned::build(batch.functions),
                debug: Arc::default(),
                totals,
            };
            (lists, true, None)
        } else {
            let mut changes = Changes {
                functions: Vec::new_in(arena),
                rebase: Vec::new_in(arena),
                got: Vec::new_in(arena),
                binds: Vec::new_in(arena),
            };
            let old = &self.old.lists;
            let mut affected = IdSet::default();
            affected.extend(self.write.iter());
            affected.extend(self.fresh.iter());
            affected.extend(self.removed.iter().copied());
            let ids = collect_in(affected.iter(), arena);
            let (old_resolved, new_resolved) = (&self.old.resolved, &self.new.resolved);
            let imports = !Arc::ptr_eq(&old_resolved.imports, &new_resolved.imports)
                || !Arc::ptr_eq(&old_resolved.libraries, &new_resolved.libraries)
                || self.old.tls != self.new.tls
                || self.old.tls_at != self.new.tls_at;
            let batch = entries.collect(
                ids.iter().copied(),
                Want { loader: true, got: !imports, functions: true },
            )?;
            let (binds, got) = if imports {
                let count = self.new.piece_count();
                let more = entries
                    .collect(0..count, Want { loader: false, got: true, functions: false })?;
                let state = &self.new;
                let binds: Vec<(PieceId, Bind)> = par::map(0..count as usize, |id| {
                    state
                        .piece(id as PieceId)
                        .into_iter()
                        .flat_map(|piece| {
                            piece.binds().iter().map(move |(at, import, addend)| {
                                (
                                    piece.id,
                                    Bind {
                                        addr: piece.addr + at,
                                        import: *import,
                                        addend: *addend,
                                    },
                                )
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .into_iter()
                .flatten()
                // alloc: only when the imports change: every bind made again, as in a full link
                .collect();
                let (binds, got) = (Owned::build(binds), Owned::build(more.got));
                whole(old.binds.items.len(), binds.items.len(), &mut changes.binds);
                whole(old.got.items.len(), got.items.len(), &mut changes.got);
                (binds, got)
            } else {
                (
                    old.binds.splice(&affected, batch.binds, &mut changes.binds),
                    old.got.splice(&affected, batch.got, &mut changes.got),
                )
            };
            let mut stale = IdSet::default();
            if !self.changed.is_empty() {
                // The changed symbols as a sorted slice the workers share (the set is in the
                // stage's frame, which one thread owns); each function then costs a binary
                // search of the few, in parallel.
                let mut changed = collect_in(self.changed.iter(), arena);
                radix::sort_copy_by_key(&mut changed, |it| it.0);
                let (changed, state): (&[Sym], &State) = (&changed, &self.new);
                let is = |sym: Sym| changed.binary_search(&sym).is_ok();
                let affected = &affected;
                // alloc: once per stage that changed a symbol, the functions it left stale
                let found: Vec<PieceId> = par::filter_map(&old.functions.owners[..], |owner| {
                    if affected.contains(*owner) {
                        return None;
                    }
                    let info = state.piece(*owner).and_then(PieceRef::function)?;
                    let windows = info.windows.is_some_and(|(unwind, handler, lsda)| {
                        is(unwind) || handler.is_some_and(is) || lsda.is_some_and(is)
                    });
                    (windows || info.fde.is_some_and(is)).then_some(*owner)
                }); // alloc: once per stage, the stale functions found in parallel
                stale.extend(found);
            }
            let mut functions = batch.functions;
            if !stale.is_empty() {
                let ids = collect_in(stale.iter(), arena);
                functions.extend(
                    entries
                        .collect(
                            ids.iter().copied(),
                            Want { loader: false, got: false, functions: true },
                        )?
                        .functions,
                );
                affected.extend(ids);
            }
            let touched = affected.iter().any(|id| {
                let function = |state: &State| {
                    state.any(id).is_some_and(|it| matches!(it.what(), What::Function(_)))
                };
                function(self.old) || function(&self.new)
            });
            let lists = Lists {
                rebase: old.rebase.splice(&affected, batch.rebase, &mut changes.rebase),
                binds,
                got,
                functions: old.functions.splice(&affected, functions, &mut changes.functions),
                // clone: a reference count; the sections are shared until a function changes
                debug: old.debug.clone(),
                totals,
            };
            (lists, touched, Some(changes))
        };
        drop(entries);
        lists.debug = match self.output.debug {
            Debug::None => Arc::default(),
            Debug::Lines if functions_changed || self.spans_changed || self.full => {
                Arc::from(self.debug(&lists)?)
            }
            Debug::Lines => lists.debug,
        };
        // alloc: once per stage; the state shares the lists
        self.new.lists = Arc::new(lists);
        Ok(changes)
    }

    /// The image as the writer reads it, besides [`State::lists`].
    pub fn parts(&self) -> Result<Parts, LinkError> {
        let regions = self.new.regions();
        let resolver = Resolver {
            symbols: self.symbols,
            state: &self.new,
            regions: &regions,
            base: self.plan.base,
        };
        let exports = self.exports();
        let mut out = Vec::with_capacity(exports.len());
        for crate::place::Asked { name, symbol, data, weak, own } in exports {
            let Some(sym) = self.symbols.find_sym_name(symbol) else {
                // alloc: error path
                return Err(error(None, format!("the export `{name}` is not defined")));
            };
            let value = resolver
                .value(sym)
                // alloc: error path
                .map_err(|it| {
                    error(None, format!("the export `{name}`: {}", it.say(self.symbols)))
                })?;
            let Value::Image(addr) = value else {
                // alloc: error path
                return Err(error(None, format!("the export `{name}` is not in the image")));
            };
            let (kind, size) =
                match self.new.home(sym).and_then(|(p, o)| Some((self.new.piece(p)?, o))) {
                    Some((piece, offset)) => {
                        let class = &regions[piece.region as usize].class;
                        let kind = match (piece.what(), class) {
                            (_, Class::Tdata | Class::Tbss) => SymbolKind::Tls,
                            (What::Function(_), _)
                            | (_, Class::Text | Class::Patch | Class::Stubs) => {
                                SymbolKind::Function
                            }
                            _ => SymbolKind::Data,
                        };
                        (kind, if offset == 0 { piece.size } else { 0 })
                    }
                    None => (SymbolKind::Other, 0),
                };
            // What its definition states of itself is its own: an object's `malloc` among
            // other functions of one section is not the section's size.
            let (kind, size) = own.unwrap_or((kind, size));
            let kind = if data { SymbolKind::Data } else { kind };
            // alloc: the writer's export table owns its names (model.rs `Export`), one per export
            out.push(Export { name: name.to_owned(), addr, size, kind, weak });
        }
        let image = |name: &str| -> Option<u64> {
            match resolver.value(self.symbols.find_sym_name(name)?) {
                Ok(Value::Image(addr)) => Some(addr),
                _ => None,
            }
        };
        let entry = match &self.output.kind {
            OutputKind::Executable { entry } => Some(entry.as_str()),
            OutputKind::Shared { .. } => self.writer.shared_entry(),
        };
        let entry = match entry {
            Some(entry) => {
                // alloc: error path
                Some(image(entry).ok_or_else(|| {
                    error(None, format!("the entry point `{entry}` is not in the image"))
                })?)
            }
            None => None,
        };
        let id = self.new.image_id();
        let (init, fini) = (image("_init"), image("_fini"));
        Ok(Parts { regions, exports: out, entry, init, fini, id })
    }

    /// The debug sections of every Rust function, in address order: their lines made into
    /// rows, then [`crate::dwarf::sections`].
    ///
    /// The rows are made in parallel, [`DEBUG_CHUNK`] functions a task, each task numbering
    /// the files in the order it first meets them. Those orders joined in function order are
    /// the order one pass over every function meets them in, so the rows renumbered with
    /// them, in parallel, are that pass's rows, and the first error in function order is the
    /// one given.
    fn debug(&self, lists: &Lists) -> Result<[(&'static str, Vec<u8>); 4], LinkError> {
        let state = &self.new;
        let symbols: &Symbols = self.symbols;
        // alloc: once per stage that makes debug info: the Rust functions in list order
        let functions: Vec<_> = par::filter_map(
            par::zip(&lists.functions.owners[..], &lists.functions.items[..]),
            |(owner, function)| Some((state.piece(*owner).and_then(PieceRef::function)?, function)),
        ); // alloc: as above
        // alloc: as above, each function's line count, then where its rows start
        let counts: Vec<usize> = par::map(&functions, |(info, _)| state.lines(info).len()); // alloc: as above
        let mut starts: Vec<usize> = Vec::with_capacity(functions.len() + 1);
        let mut total = 0;
        starts.push(0);
        for count in counts {
            total += count;
            starts.push(total);
        }
        // alloc: as above, a view per function
        let views: Vec<crate::dwarf::DebugFunction<'_>> =
            par::map(par::enumerate(&functions), |(at, (info, function))| {
                crate::dwarf::DebugFunction {
                    addr: function.addr,
                    size: function.size,
                    name: state.debug_name(info.id).map(|it| symbols.str(it)),
                    lines: starts[at] as u32..starts[at + 1] as u32,
                }
            }); // alloc: as above
        let rows: &mut [crate::dwarf::Row] = self.arena.alloc_slice_fill_with(total, |_| {
            crate::dwarf::Row { at: 0, file: 0, line: 0, column: 0 }
        });
        // Each task's files, (anchor, file in its table) and name, in the order it met them.
        type Met<'s> = Vec<((IdNo, u32), &'s str)>;
        let met: Vec<Result<Met<'_>, LinkError>> = par::map(
            par::enumerate(debug_chunks(&mut *rows, &starts)),
            |(chunk, out)| {
                let from = chunk * DEBUG_CHUNK;
                // alloc: once per task of DEBUG_CHUNK functions, its files (a few)
                let mut met: Met<'_> = Vec::new();
                let mut file_of: HashMap<(IdNo, u32), u32, Global> = HashMap::new_in(Global);
                let mut last: Option<(&blaze_ids::ContentId, IdNo, &crate::position::Spans)> = None;
                let mut at = 0;
                for (info, _) in &functions[from..(from + DEBUG_CHUNK).min(functions.len())] {
                    for line in state.lines(info) {
                        let anchor = &line.span.anchor;
                        let (anchor_id, table) = match last {
                            Some((it, id, table)) if it == anchor => (id, table),
                            _ => {
                                let found = symbols
                                    .find_id(anchor)
                                    .and_then(|id| Some((id, state.span_table(id)?)));
                                let (id, table) = found.ok_or_else(|| {
                                    // alloc: error path
                                    error(Some(info.key), format!("a line in {anchor:?}, whose span table the link was not given"))
                                })?;
                                last = Some((anchor, id, table));
                                (id, table)
                            }
                        };
                        let span = table.span(line.span.index).ok_or_else(|| {
                            // alloc: error path
                            error(
                                Some(info.key),
                                format!("span {} of {anchor:?}, past its table", line.span.index),
                            )
                        })?;
                        let next = met.len() as u32;
                        let file = *file_of.entry((anchor_id, span.file)).or_insert(next);
                        if file == next {
                            met.push(((anchor_id, span.file), table.file(span.file)));
                        }
                        out[at] = crate::dwarf::Row {
                            at: line.at,
                            file,
                            line: span.line,
                            column: span.column,
                        };
                        at += 1;
                    }
                }
                Ok(met)
            },
        ); // alloc: once per stage that makes debug info, a result per task
        // alloc: once per stage that makes debug info, the files and each task's numbers for its own
        let mut files: Vec<&str> = Vec::new();
        let mut file_of: HashMap<(IdNo, u32), u32, Global> = HashMap::new_in(Global);
        let mut renumber: Vec<Vec<u32>> = Vec::with_capacity(met.len());
        for met in met {
            let numbers = met?
                .into_iter()
                .map(|(key, name)| {
                    let next = files.len() as u32;
                    let file = *file_of.entry(key).or_insert(next);
                    if file == next {
                        files.push(name);
                    }
                    file
                })
                .collect(); // alloc: as above, a task's numbers
            renumber.push(numbers);
        }
        par::for_each(par::zip(debug_chunks(&mut *rows, &starts), &renumber), |(out, numbers)| {
            for row in out {
                row.file = numbers[row.file as usize];
            }
        });
        let size = match self.output.arch {
            Arch::Wasm32 => 4,
            Arch::Wasm64 => 8,
            Arch::X86_64 | Arch::Aarch64 => 8,
        };
        // alloc: error path
        crate::dwarf::sections(&views, rows, &files, size)
            .map_err(|it| error(None, format!("debug info: {it}")))
    }

    /// What finds the address of a name the image defines, among `regions`: [`Placed::symbol`].
    pub(crate) fn resolver<'s>(&'s self, regions: &'s [Region]) -> Resolver<'s> {
        Resolver { symbols: self.symbols, state: &self.new, regions, base: self.plan.base }
    }
}

impl Parts {
    pub fn placed<'p>(
        &'p self,
        stage: &'p Stage<'_>,
        contents: &'p [&'p [u8]],
        paged: &'p [Option<Pages<'p>>],
        // dyn: `Placed::symbol`'s type (model.rs); a writer asks it a few fixed names per link
        symbol: &'p (dyn Fn(&str) -> Option<u64> + Sync),
        delta: Option<&'p Delta<'p>>,
        encoded: &'p [(u64, u64)],
    ) -> Placed<'p> {
        let lists = &stage.new.lists;
        Placed {
            output: stage.output,
            name: stage.name,
            plan: stage.plan,
            regions: &self.regions,
            contents,
            paged,
            libraries: &stage.new.resolved.libraries,
            imports: &stage.new.resolved.imports,
            got: &lists.got.items,
            rebase: &lists.rebase.items,
            binds: &lists.binds.items,
            exports: &self.exports,
            functions: &lists.functions.items,
            entry: self.entry,
            init: self.init,
            fini: self.fini,
            tls: stage.new.tls,
            id: self.id,
            debug: &lists.debug,
            symbol,
            synthesis: stage.new.synthesis,
            delta,
            encoded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixup(at: u64, apply: Apply) -> Fixup {
        Fixup { at, apply, target: Sym(0), addend: 0, minus: None }
    }

    /// An import called and also reached through a GOT cell (as every PLT stub's target is)
    /// keeps its PLT stub: its cells index sits in the slot's high half, not its kind.
    #[test]
    fn an_import_with_cells_is_still_an_import() {
        let (import, local) = (Sym(0), Sym(1));
        let mut column = Column::sized(&[import, local], 2);
        *column.slot_mut(import) = [4, IMPORT];
        *column.slot_mut(local) = [0x1000, IMAGE];
        assert!(column.is_import(import));
        column.cells_mut(import).got[GotKind::Address as usize] = 0x2000;
        column.cells_mut(local).got[GotKind::Address as usize] = 0x2008;
        assert!(column.is_import(import), "a GOT cell keeps it an import");
        assert!(!column.is_import(local));
        column.cells_mut(import).plt = 0x3000;
        let cells = column.cells(import);
        assert_eq!((cells.plt, cells.got[GotKind::Address as usize]), (0x3000, 0x2000));
    }

    #[test]
    fn many_fixups_split_at_their_boundaries_and_others_do_not() {
        let few: Vec<Fixup> = (0..10).map(|it| fixup(it * 8, Apply::Abs64)).collect();
        let cuts = |fixups: &[Fixup]| split(fixups).collect::<Vec<_>>();
        assert_eq!(cuts(&few), [(0, 0)]);
        let many: Vec<Fixup> =
            (0..SPLIT as u64 * 2 + 5).map(|it| fixup(it * 8, Apply::Abs64)).collect();
        assert_eq!(
            cuts(&many),
            [(0, 0), (SPLIT as u64 * 8, SPLIT as u32), (SPLIT as u64 * 16, 2 * SPLIT as u32)]
        );
        let mut overlapping = many.clone();
        overlapping[100].at = overlapping[99].at + 4;
        assert_eq!(cuts(&overlapping), [(0, 0)]);
        let mut native = many.clone();
        native[7].apply = Apply::Native(1);
        assert_eq!(cuts(&native), [(0, 0)]);
        let mut unsorted = many;
        unsorted.swap(3, 4);
        assert_eq!(cuts(&unsorted), [(0, 0)]);
    }

    /// A splice against rebuilding the list from what is left and what is new.
    #[test]
    fn a_spliced_list_is_the_list_built_again() {
        let old: Vec<(PieceId, u64)> =
            (0..200u32).map(|id| (id, u64::from(id % 50) * 64 + u64::from(id / 50))).collect();
        let list = Owned::build(old.clone());
        assert!(list.items.windows(2).all(|it| it[0] <= it[1]));
        let mut drop = IdSet::default();
        drop.extend((0..200).filter(|it| it % 7 == 0));
        let new: Vec<(PieceId, u64)> = (0..200u32)
            .filter(|it| it % 7 == 0)
            .map(|id| (id, u64::from(id) * 13 % 3000))
            .chain([(500, 0), (501, 9999)])
            .collect();
        let frame = crate::arena::stage_frame().unwrap();
        let mut runs = Vec::new_in(&frame);
        let spliced = list.splice(&drop, new.clone(), &mut runs);
        let mut want: Vec<(PieceId, u64)> =
            old.into_iter().filter(|it| !drop.contains(it.0)).chain(new).collect();
        want.sort_by_key(|(id, at)| (*at, *id));
        assert_eq!(*spliced.items, want.iter().map(|it| it.1).collect::<Vec<_>>());
        assert_eq!(*spliced.owners, want.iter().map(|it| it.0).collect::<Vec<_>>());
        assert!(!runs.is_empty());
        runs.clear();
        let same = spliced.splice(&IdSet::default(), Vec::new(), &mut runs);
        assert_eq!(*same.items, *spliced.items);
        assert!(runs.is_empty(), "a splice of nothing replaces no run");
    }

    /// The planned, parallel splice against the serial pass, on seeded lists below and above
    /// the parallel grain: pieces with several entries, entries sharing an address, dropped
    /// pieces given new entries at their old addresses or elsewhere, new pieces, and drops
    /// of none, some or all.
    #[test]
    fn a_planned_splice_gives_the_serial_lists_and_runs() {
        let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        let mut next = move |below: u64| rng.below(below);
        let frame = crate::arena::stage_frame().unwrap();
        for (round, pieces) in
            [0u32, 1, 3, 40, 500, 9_000, 30_000].into_iter().cycle().take(42).enumerate()
        {
            let mut old: Vec<(PieceId, u64)> = Vec::new();
            for id in 0..pieces {
                let base = next(u64::from(pieces) * 64);
                for _ in 0..1 + next(3) {
                    old.push((id, base + next(4) * 8));
                }
            }
            let list = Owned::build(old.clone());
            let mut drop = IdSet::default();
            let share = [0, 1, 7, 50, 100][round % 5];
            drop.extend((0..pieces).filter(|_| next(100) < share));
            let mut new: Vec<(PieceId, u64)> = Vec::new();
            for (id, at) in &old {
                if drop.contains(*id) && next(3) > 0 {
                    new.push((
                        *id,
                        if next(2) == 0 { *at } else { next(u64::from(pieces) * 64 + 64) },
                    ));
                }
            }
            for id in pieces..pieces + next(4) as u32 {
                drop.insert(id);
                new.push((id, next(u64::from(pieces) * 64 + 64)));
            }
            let (mut runs, mut serial_runs) = (Vec::new_in(&frame), Vec::new_in(&frame));
            let planned = list.splice(&drop, new.clone(), &mut runs);
            let serial = list.splice_serial(&drop, new, &mut serial_runs);
            let what = format!("round {round}: {pieces} pieces, {share}% dropped");
            assert_eq!(*planned.items, *serial.items, "{what}");
            assert_eq!(*planned.owners, *serial.owners, "{what}");
            assert_eq!(runs[..], serial_runs[..], "{what}");
        }
    }

    /// The column gives back what was put in it, and nothing for a symbol it was not given.
    #[test]
    fn a_column_reads_back_values_cells_and_errors() {
        let syms: Vec<Sym> = (0..6).map(|it| Symbols::join(it % 3, it * 5)).collect();
        let mut column = Column::sized(&syms, 0);
        *column.slot_mut(syms[0]) = [0x1000, IMAGE];
        *column.slot_mut(syms[1]) = [7, ABSOLUTE];
        *column.slot_mut(syms[2]) = [3, IMPORT];
        *column.slot_mut(syms[3]) = [16, TLS];
        column.whys.push(Why::Removed);
        *column.slot_mut(syms[4]) = [0, WHY];
        column.cells_mut(syms[2]).plt = 0x2000;
        column.cells_mut(syms[0]).got[GotKind::TpOff as usize] = 0x3000;
        assert_eq!(column.value(syms[0]), Ok(Value::Image(0x1000)));
        assert_eq!(column.value(syms[1]), Ok(Value::Absolute(7)));
        assert_eq!(column.value(syms[2]), Ok(Value::Import(3)));
        assert_eq!(column.value(syms[3]), Ok(Value::Tls(16)));
        assert_eq!(column.value(syms[4]), Err(0));
        assert_eq!(column.value(syms[5]), Err(u32::MAX));
        assert_eq!(column.value(Symbols::join(40, 1 << 20)), Err(u32::MAX));
        assert!(column.removed(0), "an unwind record's target in a removed piece is zeroed");
        assert!(
            !column.removed(u32::MAX),
            "any other unresolved target of an unwind record is an error"
        );
        assert_eq!(column.cells(syms[2]).plt, 0x2000);
        assert_eq!(column.cells(syms[2]).got, [NONE; 4]);
        assert_eq!(column.cells(syms[0]).got, [NONE, 0x3000, NONE, NONE]);
        assert_eq!(column.cells(syms[1]).plt, NONE);
        assert_eq!(column.value(syms[0]), Ok(Value::Image(0x1000)), "cells keep the kind");
    }
}
