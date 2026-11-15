//! [`State`] as sections of a kept state ([`crate::keep`]): each field written and read back
//! as its own section. [`State::keep_section`] and [`State::open_section`] both destructure the
//! whole state, so a field added to it and kept by neither fails to compile.

#![forbid(unsafe_code)]

use std::alloc::{Allocator, Global};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::cow::{Chunked, CowVec};
use crate::keep::{Bad, In, Kept, Out, Section, Unkept, kept_enum, kept_struct, static_name};
use crate::model::{Class, Formats, Special, Sym, Writer};
use crate::native::Resolved;
use crate::position::Spans;
use crate::symbols::{IdNo, NameId, SHARDS, Symbols};

use super::{
    Def, FunctionInfo, Hole, Hot, IdCold, Input, Lists, Loader, Pairs, Recipe, RustName, Sort,
    Space, State, SymCold, What,
};

/// What opening a state reads beside its bytes: the symbols opened before it, whose names a
/// special the writer places is found by, and the writer, which parses the native inputs and
/// gives those specials again.
pub(crate) struct Opening<'a> {
    pub symbols: &'a Symbols,
    pub writer: &'a Formats,
}

kept_struct!(Hot { addr, size, sym, region, class, align_log2, flags });
kept_struct!(Sort { rank, order });
kept_struct!(Lists { fixups, weak, defines, wasm });
kept_struct!(Loader { rebase, binds });
kept_struct!(SymCold { refs, room, slot, seen });
kept_struct!(FunctionInfo { key, id, fde, windows, lines, made });
kept_struct!(RustName { target, weak });
kept_struct!(Space { region, bytes, zero, offset });
kept_struct!(Hole { at, anchor, index });

/// A recipe: its bytes as one run, then its holes.
impl Kept for Recipe {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.bytes(&self.bytes);
        out.len(self.holes.len());
        for hole in &self.holes {
            hole.put(out);
        }
    }

    fn take(input: &mut In<'_>) -> Result<Recipe, Bad> {
        let bytes = input.bytes()?.to_vec();
        let count = input.count(12)?;
        let mut holes = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            holes.push(Hole::take(input)?);
        }
        Ok(Recipe { bytes, holes })
    }
}

kept_enum!(What {
    0 => Function(id),
    1 => Shared,
    2 => Static { thread_local },
    3 => Native { obj, section, unwind },
    4 => NativeFrame { obj, section, fde },
    5 => Common,
    6 => Got { kind, target },
    7 => Plt { target },
    8 => Thunk { target, island },
    9 => Fde,
    10 => WinInfo,
    11 => Tlv,
    12 => Fixed,
    13 => Cells,
});

kept_enum!(Def {
    0 => None,
    1 => Piece { piece, offset },
    2 => Import(index),
    3 => Absolute(value),
    4 => TlsOffset(offset),
    5 => TpOff(offset),
    6 => Alias(sym),
    7 => Special(index),
});

/// A span table as its encoding, checked and indexed again when read ([`Spans::new`]).
impl Kept for IdCold {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        self.debug_name.put(out);
        match &self.spans {
            None => out.u8(0),
            Some(spans) => {
                out.u8(1);
                out.bytes(spans.encoding());
            }
        }
    }

    fn take(input: &mut In<'_>) -> Result<IdCold, Bad> {
        // The trait's `take`, named: `Option::take` is the inherent one.
        let debug_name = <Option<NameId> as Kept>::take(input)?;
        let spans = match input.u8()? {
            0 => None,
            1 => Some(Spans::new(input.bytes()?).map_err(Bad::Text)?),
            _ => return Err(Bad::What("an option's tag that is neither 0 nor 1")),
        };
        Ok(IdCold { debug_name, spans })
    }
}

fn put_shards<T: Kept + Chunked, A: Allocator>(shards: &[CowVec<T>; SHARDS], out: &mut Out<A>) {
    for shard in shards {
        shard.put(out);
    }
}

fn take_shards<T: Kept + Chunked>(input: &mut In<'_>) -> Result<[CowVec<T>; SHARDS], Bad> {
    let mut shards: [CowVec<T>; SHARDS] = std::array::from_fn(|_| CowVec::default());
    for shard in &mut shards {
        *shard = CowVec::take(input)?;
    }
    Ok(shards)
}

impl Kept for Pairs {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.u32(self.given);
        out.u32(self.placed);
        put_shards(&self.of, out);
    }

    fn take(input: &mut In<'_>) -> Result<Pairs, Bad> {
        let (given, placed) = (input.u32()?, input.u32()?);
        Ok(Pairs { given, placed, of: take_shards(input)? })
    }
}

/// A native input: its bytes as given, parsed again when read
/// ([`crate::native::open_input`]).
fn put_input<A: Allocator>(input: &Input, out: &mut Out<A>) {
    let Input { id, name, kind, order, bytes, file: _ } = input;
    id.put(out);
    name.put(out);
    kind.put(out);
    order.put(out);
    out.bytes(bytes);
}

/// Whether every name the linker made that `class` holds is one a kept state can name.
fn known(class: &Class) -> Result<(), Unkept> {
    match class {
        Class::Synthetic(name) if static_name(name).is_none() => Err(Unkept::Synthetic(*name)),
        _ => Ok(()),
    }
}

/// The specials: each as itself, but an address the writer places ([`Special::Placed`], a
/// function of this process) as the name that defines it, which the writer gives again.
fn put_specials<A: Allocator>(
    specials: &[Special],
    defs: &[CowVec<Def>; SHARDS],
    symbols: &Symbols,
    out: &mut Out<A>,
) -> Result<(), Unkept> {
    let mut named: Vec<Option<Sym>> = Vec::with_capacity_in(specials.len(), Global);
    named.resize(specials.len(), None);
    for (shard, column) in defs.iter().enumerate() {
        for (at, def) in column.iter().enumerate() {
            if let Def::Special(index) = def
                && let Some(slot) = named.get_mut(*index as usize)
            {
                slot.get_or_insert(Symbols::join(shard, at));
            }
        }
    }
    out.len(specials.len());
    for (index, special) in specials.iter().enumerate() {
        match special {
            Special::ImageBase => out.u8(0),
            Special::Start(class) => {
                known(class)?;
                out.u8(1);
                class.put(out);
            }
            Special::Stop(class) => {
                known(class)?;
                out.u8(2);
                class.put(out);
            }
            Special::Section { name, end } => {
                out.u8(3);
                name.put(out);
                end.put(out);
            }
            Special::WeakSection { name, end } => {
                out.u8(4);
                name.put(out);
                end.put(out);
            }
            Special::Placed(_) => {
                let name =
                    named[index].and_then(|sym| symbols.name_of(sym)).ok_or(Unkept::UnnamedSpecial(index))?;
                out.u8(5);
                out.text(name);
            }
            Special::Absolute(value) => {
                out.u8(6);
                value.put(out);
            }
            Special::LiveDescriptor => out.u8(7),
            Special::Got { from } => {
                out.u8(8);
                from.put(out);
            }
        }
    }
    Ok(())
}

fn take_specials<A: Allocator + Clone>(
    input: &mut In<'_>,
    writer: &Formats,
    alloc: A,
) -> Result<Arc<Vec<Special, A>, A>, Bad> {
    let count = input.count(1)?;
    let mut specials = Vec::with_capacity_in(count, alloc.clone());
    for _ in 0..count {
        specials.push(match input.u8()? {
            0 => Special::ImageBase,
            1 => Special::Start(Kept::take(input)?),
            2 => Special::Stop(Kept::take(input)?),
            3 => Special::Section { name: Kept::take(input)?, end: Kept::take(input)? },
            4 => Special::WeakSection { name: Kept::take(input)?, end: Kept::take(input)? },
            5 => match writer.special(input.text()?) {
                Some(placed @ Special::Placed(_)) => placed,
                _ => return Err(Bad::What("a placed address the writer no longer gives its name")),
            },
            6 => Special::Absolute(Kept::take(input)?),
            7 => Special::LiveDescriptor,
            8 => Special::Got { from: Kept::take(input)? },
            _ => return Err(Bad::What("a tag that is no Special")),
        });
    }
    Ok(Arc::new_in(specials, alloc))
}

impl State {
    /// Writes `section`, one of [`Section::STATE`], of this state. An error says what of the
    /// state a kept state cannot hold yet.
    pub(crate) fn keep_section<A: Allocator>(
        &self,
        section: Section,
        symbols: &Symbols,
        out: &mut Out<A>,
    ) -> Result<(), Unkept> {
        let State {
            generation,
            specials,
            spaces,
            names,
            wasm_imports,
            inputs,
            resolved,
            slots,
            slot_syms,
            capacity,
            pairs,
            tls,
            tls_at,
            file_len,
            parts,
            debug,
            exports,
            // A number of this process's ([`crate::write::next_synthesis`]): an opened state's is
            // 0, so its writer makes its tables again rather than patching ones it never made.
            synthesis: _,
            encoded,
            lists,
            lineage,
            given,
            debug_delta,
            laid_out,
            islands,
            makes,
            texts,
            hot,
            what,
            sort,
            piece_lists,
            loader,
            users,
            fixups,
            syms,
            wasm_names,
            wasm_declarers,
            rebase,
            binds,
            defs,
            cold,
            ref_items,
            seen,
            classes,
            functions,
            lines,
            ids,
            cells,
            slot_next,
            removals,
        } = self;
        match section {
            Section::Image | Section::Symbols | Section::Filed | Section::Rela => {
                panic!("the section `{}` is the image's, not its state's", section.name())
            }
            Section::State => {
                if !wasm_imports.is_empty() {
                    return Err(Unkept::WasmImport);
                }
                generation.put(out);
                slots.put(out);
                capacity.put(out);
                tls.put(out);
                tls_at.put(out);
                file_len.put(out);
                lineage.put(out);
                given.put(out);
                laid_out.put(out);
                islands.put(out);
            }
            Section::Specials => put_specials(specials, defs, symbols, out)?,
            Section::Spaces => {
                for space in spaces.iter() {
                    known(&space.region.class)?;
                }
                spaces.put(out);
            }
            Section::Names => {
                out.len(names.len());
                for (name, rust) in names.iter() {
                    name.put(out);
                    rust.put(out);
                }
            }
            Section::Inputs => {
                out.len(inputs.len());
                for input in inputs.iter() {
                    put_input(input, out);
                }
            }
            Section::Resolved => resolved.keep(out),
            Section::SlotSyms => slot_syms.put(out),
            Section::Pairs => pairs.put(out),
            Section::Parts => parts.put(out),
            Section::Debug => {
                out.len(debug.len());
                for (at, index) in debug.iter() {
                    out.u64(*at);
                    out.len(*index);
                }
            }
            Section::Exports => exports.put(out),
            Section::Encoded => encoded.put(out),
            Section::Lists => lists.keep(out)?,
            Section::DebugDelta => (**debug_delta).put(out),
            Section::Makes => makes.put(out),
            Section::Classes => {
                for class in classes.iter() {
                    known(class)?;
                }
                classes.put(out);
            }
            Section::Hot => hot.put(out),
            Section::What => what.put(out),
            Section::Sort => sort.put(out),
            Section::PieceLists => piece_lists.put(out),
            Section::Loader => loader.put(out),
            Section::Users => users.put(out),
            Section::Fixups => fixups.put(out),
            Section::Syms => syms.put(out),
            Section::WasmNames => wasm_names.put(out),
            Section::WasmDeclarers => put_shards(wasm_declarers, out),
            Section::Rebase => rebase.put(out),
            Section::Binds => binds.put(out),
            Section::Defs => put_shards(defs, out),
            Section::Cold => put_shards(cold, out),
            Section::RefItems => ref_items.put(out),
            Section::Seen => seen.put(out),
            Section::Functions => functions.put(out),
            Section::Lines => lines.put(out),
            Section::Ids => put_shards(ids, out),
            Section::Cells => cells.put(out),
            Section::SlotNext => slot_next.put(out),
            Section::Removals => removals.put(out),
            Section::Texts => {
                out.len(texts.len());
                for (id, recipe) in texts.iter() {
                    id.put(out);
                    recipe.put(out);
                }
            }
        }
        Ok(())
    }

    /// Reads `section`, one of [`Section::STATE`], into this state, which holds the sections
    /// before it in [`Section::ALL`]'s order: the resolution shares the inputs and names read
    /// before it.
    pub(crate) fn open_section(
        &mut self,
        section: Section,
        input: &mut In<'_>,
        opening: &Opening<'_>,
    ) -> Result<(), Bad> {
        let State {
            generation,
            specials,
            spaces,
            names,
            // Empty: `keep` refuses a state with any ([`Image::keep`](crate::Image::keep)).
            wasm_imports: _,
            inputs,
            resolved,
            slots,
            slot_syms,
            capacity,
            pairs,
            tls,
            tls_at,
            file_len,
            parts,
            debug,
            exports,
            // 0, as [`State::new`] made it: see `keep_section`.
            synthesis: _,
            encoded,
            lists,
            lineage,
            given,
            debug_delta,
            laid_out,
            islands,
            makes,
            texts,
            hot,
            what,
            sort,
            piece_lists,
            loader,
            users,
            fixups,
            syms,
            wasm_names,
            wasm_declarers,
            rebase,
            binds,
            defs,
            cold,
            ref_items,
            seen,
            classes,
            functions,
            lines,
            ids,
            cells,
            slot_next,
            removals,
        } = self;
        match section {
            Section::Image | Section::Symbols | Section::Filed | Section::Rela => {
                panic!("the section `{}` is the image's, not its state's", section.name())
            }
            Section::State => {
                *generation = Kept::take(input)?;
                *slots = Kept::take(input)?;
                *capacity = Kept::take(input)?;
                *tls = Kept::take(input)?;
                *tls_at = Kept::take(input)?;
                *file_len = Kept::take(input)?;
                *lineage = Kept::take(input)?;
                *given = Kept::take(input)?;
                *laid_out = Kept::take(input)?;
                *islands = Kept::take(input)?;
            }
            Section::Specials => *specials = take_specials(input, opening.writer, Global)?,
            Section::Spaces => *spaces = Kept::take(input)?,
            Section::Names => {
                let count = input.count(4)?;
                let mut map = BTreeMap::new();
                for _ in 0..count {
                    let name = NameId::take(input)?;
                    if map.insert(name, RustName::take(input)?).is_some() {
                        return Err(Bad::What("a name Rust code defines kept twice"));
                    }
                }
                *names = Arc::new(map);
            }
            Section::Inputs => {
                let count = input.count(1)?;
                let mut kept = Vec::with_capacity_in(count, Global);
                for _ in 0..count {
                    kept.push(crate::native::open_input(input, opening.writer)?);
                }
                *inputs = Arc::new(kept);
            }
            Section::Resolved => {
                let mut opened = Resolved::open(input)?;
                opened.inputs = Arc::clone(inputs);
                opened.names = Arc::clone(names);
                *resolved = Arc::new(opened);
            }
            Section::SlotSyms => *slot_syms = Kept::take(input)?,
            Section::Pairs => *pairs = Kept::take(input)?,
            Section::Parts => *parts = Kept::take(input)?,
            Section::Debug => {
                let count = input.count(16)?;
                let mut kept = Vec::with_capacity_in(count, Global);
                for _ in 0..count {
                    kept.push((input.u64()?, input.len()?));
                }
                *debug = Arc::new(kept);
            }
            Section::Exports => *exports = Kept::take(input)?,
            Section::Encoded => *encoded = Kept::take(input)?,
            Section::Lists => *lists = Arc::new(crate::write::Lists::open(input)?),
            Section::DebugDelta => *debug_delta = Arc::new(Kept::take(input)?),
            Section::Makes => *makes = Kept::take(input)?,
            Section::Classes => *classes = Kept::take(input)?,
            Section::Hot => *hot = Kept::take(input)?,
            Section::What => *what = Kept::take(input)?,
            Section::Sort => *sort = Kept::take(input)?,
            Section::PieceLists => *piece_lists = Kept::take(input)?,
            Section::Loader => *loader = Kept::take(input)?,
            Section::Users => *users = Kept::take(input)?,
            Section::Fixups => *fixups = Kept::take(input)?,
            Section::Syms => *syms = Kept::take(input)?,
            Section::WasmNames => *wasm_names = Kept::take(input)?,
            Section::WasmDeclarers => *wasm_declarers = take_shards(input)?,
            Section::Rebase => *rebase = Kept::take(input)?,
            Section::Binds => *binds = Kept::take(input)?,
            Section::Defs => *defs = take_shards(input)?,
            Section::Cold => *cold = take_shards(input)?,
            Section::RefItems => *ref_items = Kept::take(input)?,
            Section::Seen => *seen = Kept::take(input)?,
            Section::Functions => *functions = Kept::take(input)?,
            Section::Lines => *lines = Kept::take(input)?,
            Section::Ids => *ids = take_shards(input)?,
            Section::Cells => *cells = Kept::take(input)?,
            Section::SlotNext => *slot_next = Kept::take(input)?,
            Section::Removals => *removals = Kept::take(input)?,
            Section::Texts => {
                let count = input.count(4)?;
                let mut map = BTreeMap::new();
                let mut last = None;
                for _ in 0..count {
                    let id = IdNo::take(input)?;
                    // Written in order: a recipe out of order or twice is not a kept state's.
                    if last.is_some_and(|it| it >= id) {
                        return Err(Bad::What("a text's recipe kept out of order or twice"));
                    }
                    last = Some(id);
                    map.insert(id, Recipe::take(input)?);
                }
                *texts = Arc::new(map);
            }
        }
        Ok(())
    }
}
