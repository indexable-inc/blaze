//! A build's function or static made the record a launcher takes (`blaze_launch::record`).
//!
//! The record is made from the compiled function as the build has it: its code and
//! relocations, its data, its unwind information. What a relocation names becomes a place:
//!
//! | `blaze_codegen::Target` | in the record |
//! |---|---|
//! | `Function`, `Direct` of another function | a function place; [`Named::Function`] says which, and the program gives its slot when it binds the record |
//! | `Direct` of the function itself | the record's own bytes (`To::Own`): a jump into itself |
//! | `Static` | a static place; [`Named::Static`] says which |
//! | `Data` | the datum of that id among the record's data |
//! | `Extern`, `Weak` | a function or static place filled by the program's own symbol of that name, when the program defines it ([`Made::symbols`]: the reader resolves the name); else an outside symbol by name |
//! | `Position` | a datum made here: the `Location` the program observes (`file!()`, `line!()`, a panic's place), with its file name another datum |
//!
//! Nothing here reads a whole build: a record is made of one function and what its maker says
//! of the positions its code names and of which symbols the program itself defines, so a record
//! of a sealed function is the same in every build that gives the same inputs, and is kept by
//! its bind key ([`crate::bind`]).
//!
//! A position is data of the record, so a record's id follows the lines of what it names: a
//! function whose own line moved is bound again though its code is the code it was. What
//! that costs an edit is one record a bound function below the edit in its file, each made
//! from the template the store already has and compiled by no one. [`Made::anchors`] says
//! which span tables a record read, so the program knows which records to make again.
//!
//! What the launcher does not take is refused by name: code that counts (tier counters, PGO,
//! SanitizerCoverage), Windows unwind information, wasm, and a relocation kind the record has
//! none for.

use std::alloc::Global;

use blaze_codegen::{
    Arch, CompiledFunction, Data, Init, InstanceKey, RelocKind, Relocs, StaticId, Target, Unwind,
};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_launch::record::{Datum, Fixup, Kind, Place, Record, To, What};
use blaze_launch::wire::{self, List, RecordId};
use blaze_link::{LOCATION_ALIGN, Position};

/// A function or static a record names by place, in the order of its numbered places: what
/// the program gives a slot or a static's number for when it binds the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Named {
    Function(InstanceKey),
    Static(StaticId),
}

/// A record, made.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's copy
pub struct Made {
    /// The BLAKE3 hash of `bytes`.
    pub id: RecordId,
    pub bytes: Vec<u8>,
    /// A function's placed length (its code and its pool); a static's size.
    pub len: u64,
    pub align: u32,
    /// Its function and static places that name a function or static of the build, in order.
    pub named: Vec<Named>,
    /// Its function and static places that the program's own symbol fills (an `Extern` or
    /// `Weak` relocation against a name the program defines: `#[no_mangle]`,
    /// `#[export_name]`), each with its name: the place is its index among the record's
    /// numbered places ([`blaze_launch::record::Place::numbered`]), sorted by place. With
    /// `named` they are every numbered place, `named` filling the places this leaves.
    pub symbols: Vec<(u32, String)>,
    /// The anchors of the span tables its positions were read from, sorted, each once.
    pub anchors: Vec<ContentId>,
}

/// What a symbol of the program itself is: a function or a static.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Defines {
    Function,
    Static,
}

/// What a record is made against: where the positions its code names are (the span tables
/// of the functions it was made from, by anchor), and which symbols the program itself
/// defines. Neither is a whole build: the tables are those of the record's own anchors, and
/// the symbols the program's (name, kind) pairs, which the bind key covers
/// ([`crate::bind::Under::symbols`]).
pub trait Positions {
    /// The file name and the position of `span`, or why there is none.
    fn of(&self, span: &SpanRef) -> Result<(&str, Position), String>;

    /// Whether the program defines the symbol `name` (`#[no_mangle]`, `#[export_name]`), and
    /// as what: a relocation against the name is then a place the program's own function or
    /// static fills, as a link resolves it, and never asked of the system's libraries. Which
    /// one is the reader's to say ([`Made::symbols`]).
    fn defines(&self, name: &str) -> Option<Defines>;
}

/// The file name and the absolute position of span `index` of a span table, read in place:
/// what a [`Positions::of`] over the build's tables answers. The one reader of the table's
/// layout is the link's (`blaze_link::position`); until 2026-11-13 this crate kept a copy of
/// it, which the item-relative layout would have left reading bases as rows.
pub use blaze_link::position_in;

const FILE_NAME_DOMAIN: &str = "blaze launch file name 1";
const LOCATION_DOMAIN: &str = "blaze launch location 1";

/// One datum of a record being made.
struct Owned<'a> {
    id: [u8; 32],
    align: u32,
    bytes: std::borrow::Cow<'a, [u8]>,
    fixups: Vec<Fixup>,
    writable: bool,
}

/// A place of a record being made.
enum Outside<'a> {
    Function,
    Static,
    Extern(&'a str),
    Weak(&'a str),
}

/// A record being made: its places and its data, as its relocations name them.
struct Maker<'a, P> {
    /// What the record is of, for a refusal.
    name: &'a str,
    positions: &'a P,
    /// The function's own key: a `Direct` of it is the record's own bytes.
    own: Option<InstanceKey>,
    places: Vec<Outside<'a>>,
    /// What each place is of, to find one named twice.
    targets: Vec<&'a Target>,
    named: Vec<Named>,
    symbols: Vec<(u32, String)>,
    /// The data made here (file names, then locations follow as they are named), then the
    /// function's or static's own.
    data: Vec<Owned<'a>>,
    /// The ids of `data`, in its order.
    ids: Vec<[u8; 32]>,
    anchors: Vec<ContentId>,
}

impl<'a, P: Positions> Maker<'a, P> {
    fn new(name: &'a str, positions: &'a P, own: Option<InstanceKey>) -> Maker<'a, P> {
        // alloc: one record's lists, for the length of its making
        Maker {
            name,
            positions,
            own,
            places: Vec::new(),
            targets: Vec::new(),
            named: Vec::new(),
            symbols: Vec::new(),
            data: Vec::new(),
            ids: Vec::new(),
            anchors: Vec::new(),
        }
    }

    fn refuse(&self, what: std::fmt::Arguments<'_>) -> String {
        let what = what.to_string(); // alloc: an error
        blaze_report::say!(blaze_report::say::Code(&self.name), " cannot run under the launcher: ", blaze_report::say::Words(&what))
            .flat()
    }

    /// The datum of `id`, added when it is not there yet.
    fn datum(&mut self, datum: Owned<'a>) -> u32 {
        if let Some(at) = self.ids.iter().position(|it| *it == datum.id) {
            return at as u32;
        }
        self.ids.push(datum.id);
        self.data.push(datum);
        (self.data.len() - 1) as u32
    }

    /// What a fixup against `target` is against.
    fn to(&mut self, target: &'a Target) -> Result<To, String> {
        if let Some(at) = self.targets.iter().position(|it| *it == target) {
            return Ok(To::Place(at as u32));
        }
        let place = match target {
            Target::Direct(key) if Some(*key) == self.own => return Ok(To::Own),
            Target::Function(key) | Target::Direct(key) => {
                self.named.push(Named::Function(*key));
                Outside::Function
            }
            Target::Static { item, instance } => {
                self.named.push(Named::Static((*item, *instance)));
                Outside::Static
            }
            Target::Extern(name) | Target::Weak(name) => match self.positions.defines(name) {
                // The program's own symbol fills it: a numbered place, which the reader
                // gives the function's slot or the static's number by the name.
                Some(defines) => {
                    let place = (self.named.len() + self.symbols.len()) as u32;
                    self.symbols.push((place, name.clone())); // clone: a name the record's reader resolves
                    match defines {
                        Defines::Function => Outside::Function,
                        Defines::Static => Outside::Static,
                    }
                }
                None if matches!(target, Target::Weak(_)) => Outside::Weak(name),
                None => Outside::Extern(name),
            },
            Target::Data(id) => {
                return match self.ids.iter().position(|it| *it == id.0) {
                    Some(at) => Ok(To::Data(at as u32)),
                    None => Err(self.refuse(format_args!(
                        "it names datum {id:?}, which is not among its data before the one that names it"
                    ))),
                };
            }
            Target::Position(span) => {
                let (file, position) = self.positions.of(span).map_err(|why| {
                    self.refuse(format_args!("its position {} of {:?}: {why}", span.index, span.anchor))
                })?;
                if let Err(at) = self.anchors.binary_search(&span.anchor) {
                    self.anchors.insert(at, span.anchor);
                }
                // The name, with the NUL rustc's `Location` ends it with.
                let mut name = Vec::with_capacity(file.len() + 1); // alloc: a datum's bytes
                name.extend_from_slice(file.as_bytes());
                name.push(0);
                let name_id = ContentId::of(FILE_NAME_DOMAIN, &name).0;
                let name = self.datum(Owned {
                    id: name_id,
                    align: 1,
                    bytes: name.into(),
                    fixups: Vec::new(), // alloc: none, empty
                    writable: false,
                });
                let record = blaze_link::location(file.len(), position);
                let mut keyed = Vec::with_capacity(64); // alloc: the id's input
                keyed.extend_from_slice(&name_id);
                keyed.extend_from_slice(&record);
                let pointer = Fixup { at: 0, kind: Kind::Abs64, to: To::Data(name), addend: 0 };
                let location = self.datum(Owned {
                    id: ContentId::of(LOCATION_DOMAIN, &keyed).0,
                    align: LOCATION_ALIGN as u32,
                    bytes: record.to_vec().into(), // alloc: a datum's bytes
                    fixups: vec![pointer],         // alloc: its one fixup
                    writable: false,
                });
                return Ok(To::Data(location));
            }
            Target::Coverage(_) | Target::Counter(_) | Target::ProfileCounters => {
                return Err(self.refuse(format_args!(
                    "it counts into {target:?}, which only a linked image has"
                )));
            }
        };
        self.places.push(place);
        self.targets.push(target);
        Ok(To::Place((self.places.len() - 1) as u32))
    }

    /// The fixups of `relocs`, in code or (`in_data`) in data.
    fn fixups(&mut self, relocs: &'a Relocs, in_data: bool) -> Result<Vec<Fixup>, String> {
        let mut fixups = Vec::with_capacity(relocs.len()); // alloc: a record's fixups
        for reloc in relocs.iter() {
            let kind = match kind(reloc.kind) {
                Some(kind) if !in_data || kind == Kind::Abs64 => kind,
                _ => {
                    return Err(self.refuse(format_args!(
                        "a {:?} relocation at offset {} of its {}, which a record has no kind for",
                        reloc.kind,
                        reloc.at,
                        if in_data { "data" } else { "code" }
                    )));
                }
            };
            let to = self.to(reloc.target)?;
            fixups.push(Fixup { at: reloc.at, kind, to, addend: reloc.addend });
        }
        Ok(fixups)
    }

    /// Adds the function's or static's own data, each after what it names.
    fn own_data(&mut self, data: &'a [Data]) -> Result<(), String> {
        for datum in data {
            let fixups = self.fixups(&datum.relocs, true)?;
            self.datum(Owned {
                id: datum.id.0,
                align: datum.align,
                bytes: (&datum.bytes[..]).into(),
                fixups,
                writable: datum.writable,
            });
        }
        Ok(())
    }

    /// The record of these parts, encoded.
    fn finish(
        self,
        arch: Arch,
        what: What,
        align: u32,
        size: u64,
        bytes: &[u8],
        fixups: &[Fixup],
        unwind: Option<blaze_launch::record::Unwind<'_>>,
    ) -> Result<Made, String> {
        let arch = match arch {
            Arch::X86_64 => wire::Arch::X86_64,
            Arch::Aarch64 => wire::Arch::Aarch64,
            Arch::Wasm32 | Arch::Wasm64 => {
                return Err(self.refuse(format_args!("it is WebAssembly, which no launcher runs")));
            }
        };
        // alloc: the record's lists as its encoder borrows them
        let places: Vec<Place<'_>> = self
            .places
            .iter()
            .map(|it| match it {
                Outside::Function => Place::Function,
                Outside::Static => Place::Static,
                Outside::Extern(name) => Place::Extern(name),
                Outside::Weak(name) => Place::Weak(name),
            })
            .collect();
        let data: Vec<Datum<'_>> = self // alloc: as above
            .data
            .iter()
            .map(|it| Datum {
                id: it.id,
                align: it.align,
                bytes: &it.bytes,
                fixups: List::of(&it.fixups),
                writable: it.writable,
            })
            .collect();
        let record = Record {
            arch,
            what,
            align,
            size,
            bytes,
            fixups: List::of(fixups),
            places: List::of(&places),
            data: List::of(&data),
            unwind,
            name: self.name,
        };
        let mut out = Vec::with_capacity(bytes.len() + 256); // alloc: the record's bytes
        record.encode(&mut out);
        let len = match what {
            What::Code => blaze_launch::place::pool(&record, Global)
                .map_err(|why| self.refuse(format_args!("{why}")))?
                .len,
            What::Static { .. } => size,
        };
        let id = RecordId(*blake3::hash(&out).as_bytes());
        Ok(Made { id, bytes: out, len, align, named: self.named, symbols: self.symbols, anchors: self.anchors })
    }
}

/// A record's kind of a relocation, when it has one.
fn kind(kind: RelocKind) -> Option<Kind> {
    Some(match kind {
        RelocKind::Abs64 => Kind::Abs64,
        RelocKind::PcRel32 => Kind::PcRel32,
        RelocKind::GotPcRel32 => Kind::GotPcRel32,
        RelocKind::Call26 => Kind::Call26,
        RelocKind::AdrpPage21 => Kind::AdrpPage21,
        RelocKind::PageOffset12 => Kind::PageOffset12,
        RelocKind::GotPage21 => Kind::GotPage21,
        RelocKind::GotPageOffset12 => Kind::GotPageOffset12,
        RelocKind::TlvRel32 => Kind::TlvRel32,
        RelocKind::TlvPage21 => Kind::TlvPage21,
        RelocKind::TlvPageOffset12 => Kind::TlvPageOffset12,
        RelocKind::TlsDescPage21 => Kind::TlsDescPage21,
        RelocKind::TlsDescLd64Lo12 => Kind::TlsDescLd64Lo12,
        RelocKind::TlsDescAddLo12 => Kind::TlsDescAddLo12,
        RelocKind::TlsDescCall => Kind::TlsDescCall,
        _ => return None,
    })
}

/// The record of `function`, which a backtrace calls `name`.
pub fn function<P: Positions>(
    function: &CompiledFunction,
    name: &str,
    positions: &P,
) -> Result<Made, String> {
    let mut maker = Maker::new(name, positions, Some(function.key));
    if function.coverage.is_some() || function.profile.is_some() {
        return Err(maker.refuse(format_args!(
            "it is instrumented (SanitizerCoverage or PGO), which only a linked image runs"
        )));
    }
    maker.own_data(&function.data)?;
    let fixups = maker.fixups(&function.relocs, false)?;
    let unwind = match &function.unwind {
        None => None,
        Some(Unwind::Windows { .. }) => {
            return Err(maker.refuse(format_args!("its unwind information is Windows'")));
        }
        Some(Unwind::Dwarf {
            code_align,
            data_align,
            return_address,
            cie,
            fde,
            personality,
            lsda,
        }) => {
            let personality = match personality {
                None => None,
                Some(routine) => match maker.to(routine)? {
                    To::Place(place) if matches!(maker.places[place as usize], Outside::Function) => {
                        Some(place)
                    }
                    _ => {
                        return Err(maker.refuse(format_args!(
                            "its personality routine {routine:?} is no function of the program"
                        )));
                    }
                },
            };
            let lsda = match lsda {
                None => None,
                Some(id) => match maker.ids.iter().position(|it| *it == id.0) {
                    Some(at) => Some(at as u32),
                    None => {
                        return Err(maker.refuse(format_args!(
                            "its unwind data area {id:?} is not among its data"
                        )));
                    }
                },
            };
            Some(blaze_launch::record::Unwind {
                code_align: *code_align,
                data_align: *data_align,
                return_address: *return_address,
                cie,
                fde,
                personality,
                lsda,
            })
        }
    };
    let size = function.code.len() as u64;
    maker.finish(function.arch, What::Code, function.align, size, &function.code, &fixups, unwind)
}

/// The record of the static `of`, which errors call `name`, for code of `arch`.
pub fn of_static<P: Positions>(
    of: &blaze_codegen::Static,
    name: &str,
    arch: Arch,
    positions: &P,
) -> Result<Made, String> {
    let mut maker = Maker::new(name, positions, None);
    if !of.imports.is_empty() {
        return Err(maker.refuse(format_args!("it has WebAssembly imports")));
    }
    maker.own_data(&of.data)?;
    let what = What::Static { mutable: of.mutable, thread_local: of.thread_local };
    match &of.init {
        Init::Zeroed { size } => maker.finish(arch, what, of.align, *size, &[], &[], None),
        Init::Bytes { bytes, relocs } => {
            let fixups = maker.fixups(relocs, true)?;
            maker.finish(arch, what, of.align, bytes.len() as u64, bytes, &fixups, None)
        }
    }
}
