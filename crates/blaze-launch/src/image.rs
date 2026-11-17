//! An image of placed code: what a launcher's first edit leaves in its memory, made once by
//! the daemon and copied in by every later run of the same build
//! (docs/v2/plan/design/launcher.md, "Placed code kept across runs").
//!
//! A launcher that starts is told its statics and the functions the runs before it called,
//! each as a record, and places them: it checks every record against its id, lays out its
//! pool and its data, writes every fixup and builds every unwind entry. That is the same
//! work every run of one build, and all of it is a function of what the daemon sends. So the
//! daemon does it once ([`lay`]) against addresses of no process and keeps the outcome, an
//! [`Image`]:
//!
//! - the bytes of the code area from its first offset, and of the program's data;
//! - which of their words hold an absolute address, and of what ([`Fix`]): the launcher adds
//!   where its own mappings, the outside symbols and its thread-local thunk are
//!   ([`Image::rebase`]);
//! - the tables a launcher keeps of what it placed: each body (its slot, range, code
//!   identity, name and unwind entry), each datum by id, each static by number, each
//!   thread-local, each outside symbol by name.
//!
//! What makes this sound is that placed code is position-independent but for those words:
//! a body reaches a slot's word, an entry thunk and its own pool relative to itself, all in
//! the launcher's one mapping, and reaches anything else through a pool cell
//! ([`crate::place`]). [`crate::place::absolutes`] names the cells; a fixup that is relative
//! to something outside the mapping is refused, and then there is no image.
//!
//! What a launcher checks before it uses one (`blaze-launch-rt`): the layout version, its
//! instruction set, system and page size; that the table's and the area's offsets in the
//! mapping are its own ([`Geometry`]); that its core hands each body the range the image
//! has it at; that every outside symbol is in the process. It does not hash the bytes: they
//! come from the daemon on the socket every record comes on, and the daemon made them from
//! its own records. An image is in the daemon's memory, never a file: a program does not
//! outlive its daemon's code (`Tell::Unheld`).
//!
//! The bytes ([`Image::encode`]) are little-endian fields in one fixed order, read back
//! whole ([`Image::decode`]); an image of another [`VERSION`] is refused by name.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::place::{self, Abs, Bound};
use crate::record::{Place as Named, Record, To, What};
use crate::wire::{Arch, Bind, List, Os, RecordId, StaticDef};
use crate::{CodeId, KEPT, Slot, Tier, frame};

/// The version of an image's layout: after its magic. An image of another is refused.
pub const VERSION: u32 = 1;

const MAGIC: &[u8; 8] = b"blazeimg";

/// The address the program's data has in an image nothing was rebased in: far from the code
/// mapping (which is at 0), so a fixup relative to data is past every reach and refused.
pub const DATA: u64 = 1 << 44;

/// Where the parts of a launcher's one mapping are, from its first byte: the slot words are
/// at 0. A function of the instruction set and the page size, the same in the launcher
/// (`blaze-launch-rt`'s `drive`), which checks an image's against its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry {
    /// The offset of slot 0's entry thunk.
    pub thunks: u64,
    /// The bytes of one entry thunk.
    pub thunk: u64,
    /// The offset of the code area's first byte.
    pub area: u64,
}

impl Geometry {
    /// The most slots a program's table holds (`Config::PROGRAM`).
    pub const SLOTS: u64 = 1 << 20;
    const STUB_HEAD: u64 = 32;

    /// A program's launcher's, for `arch` in pages of `page` bytes.
    pub fn of(arch: Arch, page: u64) -> Geometry {
        let (stub, thunk) = match arch {
            Arch::Aarch64 => (8, 16),
            Arch::X86_64 => (16, 8),
        };
        let stubs = (Geometry::SLOTS * 8).next_multiple_of(16);
        let thunks = stubs + Geometry::STUB_HEAD + Geometry::SLOTS * stub;
        let area = (thunks + Geometry::SLOTS * thunk).next_multiple_of(page.max(1));
        Geometry { thunks, thunk, area }
    }
}

/// What an absolute word is an address of: what a launcher adds to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base {
    /// The launcher's code mapping.
    Code,
    /// The program's data.
    Data,
    /// An outside symbol, by its place in [`Image::outside`].
    Outside(u32),
    /// The launcher's thunk for thread-locals.
    TlsThunk,
    /// What names a thread-local to that thunk, by its place in [`Image::tls`].
    Tls(u32),
}

/// One absolute word: 8 bytes at `at` of the area's bytes, or of the data's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fix {
    pub data: bool,
    pub at: u64,
    pub base: Base,
}

/// Where a launcher's mappings and what its process defines are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bases<'a> {
    /// The code mapping's first byte: slot 0's word.
    pub code: u64,
    /// The data's first byte.
    pub data: u64,
    /// Each outside symbol's address, as [`Image::outside`] lists them; 0 for a weak one
    /// nothing defines.
    pub outside: &'a [u64],
    pub tls_thunk: u64,
    /// What names each thread-local to the thunk, as [`Image::tls`] lists them.
    pub tls: &'a [u64],
}

impl Bases<'static> {
    /// The addresses of no process: what [`lay`] makes an image against.
    pub const NONE: Bases<'static> = Bases { code: 0, data: DATA, outside: &[], tls_thunk: 0, tls: &[] };
}

/// A static of the program, by its number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StaticAt {
    /// Its offset in the data.
    Data(u64),
    /// A thread-local: its place in [`Image::tls`].
    Tls(u32),
}

/// A thread-local static: its size and alignment, and its initial bytes in the data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TlsVar {
    pub size: u64,
    pub align: u64,
    /// The offset in the data of its initial bytes; `init_len` is 0 for one all zeroes.
    pub init: u64,
    pub init_len: u64,
}

/// One body of the area.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BodyAt {
    pub slot: Slot,
    /// Its offset in the area, and its length placed.
    pub at: u64,
    pub len: u64,
    pub align: u32,
    pub tier: Tier,
    pub code: CodeId,
    /// What a backtrace calls it.
    pub name: String,
    /// Its unwind entry: its offset in the data, and its FDE's offset in the entry.
    pub frame: Option<(u64, u32)>,
}

/// An image. See the module's documentation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Image {
    pub arch: Arch,
    pub os: Os,
    pub page: u64,
    pub geometry: Geometry,
    /// How many slots are declared.
    pub slots: u32,
    /// The slot of the program's entry.
    pub entry: Slot,
    /// The statics of the program's initializer sections, by number, in the order they run.
    pub inits: Vec<u32>,
    /// The code area from offset 0.
    pub area: Vec<u8>,
    /// The program's data from offset 0.
    pub data: Vec<u8>,
    pub fixes: Vec<Fix>,
    /// Each outside symbol by name, and whether it is linked weakly.
    pub outside: Vec<(String, bool)>,
    pub tls: Vec<TlsVar>,
    /// By number: `None` for a number no static has.
    pub statics: Vec<Option<StaticAt>>,
    /// Each datum placed, by id, with its offset in the data.
    pub datums: Vec<([u8; 32], u64)>,
    /// In the order their binds were given: the order a launcher's core hands them ranges.
    pub bodies: Vec<BodyAt>,
}

/// What a launcher's first edit is made of: what the daemon's `Welcome`, `Statics`, `Edit`
/// and `Start` say, for a launcher of `arch` and `os` in pages of `page` bytes.
#[derive(Clone, Copy)]
pub struct Input<'a> {
    pub arch: Arch,
    pub os: Os,
    pub page: u64,
    pub slots: u32,
    pub statics: &'a [StaticDef<'a>],
    pub binds: &'a [Bind<'a>],
    pub entry: Slot,
    pub inits: &'a [u32],
}

/// Each record an [`Input`] names, decoded, by id: `None` for one that was not given.
pub type Records<'r> = dyn Fn(&RecordId) -> Option<Record<'r>> + 'r;

/// The cases in which there is no image, each by name. The first four are a program's: its
/// launchers are then sent its records, and the daemon says which case and of what
/// (`blaze-launch-host`). The last two are a fault: of whoever made the input, or of bytes
/// that are no image.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Why {
    /// A static or a datum aligned past a page: the data's first byte is aligned to a page
    /// and no more, so its offset is not an aligned address in every process.
    Alignment,
    /// A thread-local static on an instruction set the launcher has none on yet.
    ThreadLocal,
    /// Code that reaches a static, a datum or an outside symbol relative to itself, with no
    /// word between: it cannot be moved apart from what it reaches.
    Relative,
    /// More bytes than one message carries ([`LONGEST`]).
    Long,
    /// What the image was to be made of does not hold together: a record that was not
    /// given, a number past what is declared, a record that does not place.
    Input,
    /// Bytes that are no image of this layout.
    Layout,
}

impl Why {
    /// The case's name, as a person and a count are given it.
    pub const fn name(self) -> &'static str {
        match self {
            Why::Alignment => "aligned past a page",
            Why::ThreadLocal => "a thread-local here",
            Why::Relative => "code relative to data",
            Why::Long => "longer than a message",
            Why::Input => "records that do not hold together",
            Why::Layout => "no image of this layout",
        }
    }
}

/// The most bytes of code and data an image holds: under a frame's 16 MiB with room for
/// its tables.
pub const LONGEST: usize = 12 << 20;

/// Why there is no image, or why bytes are not one: the case, and what of.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImageError {
    pub why: Why,
    pub what: String,
}

impl core::fmt::Display for ImageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.what)
    }
}

impl core::error::Error for ImageError {}

fn no(what: String) -> ImageError {
    ImageError { why: Why::Input, what }
}

fn because(why: Why, what: String) -> ImageError {
    ImageError { why, what }
}

/// What a place of a record is, for a fix.
#[derive(Clone, Copy)]
enum Class {
    Function,
    Data,
    Outside(u32),
    Tls(u32),
}

struct Layer<'a, 'b> {
    input: Input<'a>,
    bases: Bases<'b>,
    image: Image,
    /// One datum's, static's, body's or unwind entry's bytes on their way in.
    scratch: Vec<u8>,
}

impl Layer<'_, '_> {
    /// Hands out `size` bytes of the data aligned to `align`, zeroed: their offset.
    fn reserve(&mut self, size: u64, align: u64, of: &str) -> Result<u64, ImageError> {
        let align = align.max(1);
        if !align.is_power_of_two() || align > self.input.page {
            // The data's first byte is aligned to a page and no more: an offset aligned
            // past it is not an aligned address in every process.
            return Err(because(
                Why::Alignment,
                format!(
                    "`{of}` is aligned to {align} bytes, past a page ({}): its address in one process is not \
                     aligned in another",
                    self.input.page
                ),
            ));
        }
        let at = (self.image.data.len() as u64).next_multiple_of(align);
        let Ok(end) = usize::try_from(at + size.max(1)) else {
            return Err(no(format!("`{of}` is {size} bytes, past the address space")));
        };
        self.image.data.resize(end, 0);
        Ok(at)
    }

    fn outside(&mut self, name: &str, weak: bool) -> u32 {
        match self.image.outside.iter().position(|it| it.0 == name && it.1 == weak) {
            Some(at) => at as u32,
            None => {
                self.image.outside.push((String::from(name), weak));
                self.image.outside.len() as u32 - 1
            }
        }
    }

    /// What each place of `record` is, the numbers of its function and static places taken
    /// from `refs` in order.
    fn bounds(&mut self, record: &Record<'_>, refs: List<'_, u32>) -> Result<(Vec<Bound>, Vec<Class>), ImageError> {
        if refs.len() != record.numbered() {
            return Err(no(format!(
                "`{}` names {} functions and statics and its bind gives {} numbers",
                record.name,
                record.numbered(),
                refs.len()
            )));
        }
        let mut refs = refs.iter();
        let mut bounds = Vec::with_capacity(record.places.len());
        let mut classes = Vec::with_capacity(record.places.len());
        for named in record.places.iter() {
            // The count was checked above: each numbered place has its number.
            let mut number = || refs.next().unwrap_or(u32::MAX);
            let (bound, class) = match named {
                Named::Function => {
                    let slot = number();
                    if slot >= self.input.slots {
                        return Err(no(format!(
                            "`{}` names slot {slot}, and {} are declared",
                            record.name, self.input.slots
                        )));
                    }
                    let geometry = self.image.geometry;
                    let word = self.bases.code + u64::from(slot) * 8;
                    let thunk = self.bases.code + geometry.thunks + u64::from(slot) * geometry.thunk;
                    (Bound::Function { word, thunk }, Class::Function)
                }
                Named::Static => {
                    let number = number();
                    match self.image.statics.get(number as usize).copied().flatten() {
                        Some(StaticAt::Data(at)) => (Bound::At(self.bases.data + at), Class::Data),
                        Some(StaticAt::Tls(var)) => {
                            let arg = self.bases.tls.get(var as usize).copied().unwrap_or(0);
                            (Bound::Tls { thunk: self.bases.tls_thunk, arg }, Class::Tls(var))
                        }
                        None => {
                            return Err(no(format!(
                                "`{}` names static {number}, which was not defined",
                                record.name
                            )));
                        }
                    }
                }
                Named::Extern(name) | Named::Weak(name) => {
                    let outside = self.outside(name, matches!(named, Named::Weak(_)));
                    let at = self.bases.outside.get(outside as usize).copied().unwrap_or(0);
                    (Bound::At(at), Class::Outside(outside))
                }
            };
            bounds.push(bound);
            classes.push(class);
        }
        Ok((bounds, classes))
    }

    /// What an absolute word against `to` is an address of.
    fn base(to: To, classes: &[Class], of: &str) -> Result<Base, ImageError> {
        match to {
            To::Own => Ok(Base::Code),
            To::Data(_) => Ok(Base::Data),
            To::Place(place) => match classes.get(place as usize) {
                Some(Class::Function) => Ok(Base::Code),
                Some(Class::Data) => Ok(Base::Data),
                Some(Class::Outside(outside)) => Ok(Base::Outside(*outside)),
                Some(Class::Tls(_)) | None => Err(no(format!(
                    "`{of}` holds the address of place {place}, which is a thread-local or none of its places"
                ))),
            },
        }
    }

    /// The fixes of bytes whose every fixup is an absolute address ([`place::place_datum`],
    /// [`place::place_static`]), written at `at` of the data.
    fn absolute(
        &mut self,
        fixups: impl Iterator<Item = crate::record::Fixup>,
        at: u64,
        classes: &[Class],
        of: &str,
    ) -> Result<(), ImageError> {
        for fixup in fixups {
            let base = Layer::base(fixup.to, classes, of)?;
            self.image.fixes.push(Fix { data: true, at: at + u64::from(fixup.at), base });
        }
        Ok(())
    }

    /// Places the data of `record` that are not placed yet, and gives every datum's address.
    fn data_of(&mut self, record: &Record<'_>, bounds: &[Bound], classes: &[Class]) -> Result<Vec<u64>, ImageError> {
        let mut addresses = Vec::with_capacity(record.data.len());
        for datum in record.data.iter() {
            if let Some((_, at)) = self.image.datums.iter().find(|it| it.0 == datum.id) {
                addresses.push(self.bases.data + at);
                continue;
            }
            let mut scratch = core::mem::take(&mut self.scratch);
            let placed = place::place_datum(&datum, bounds, &addresses, &mut scratch);
            let at = match placed {
                Ok(()) => self.reserve(scratch.len() as u64, u64::from(datum.align), record.name),
                Err(why) => Err(no(format!("a datum of `{}`: {why}", record.name))),
            };
            if let Ok(at) = at {
                self.image.data[at as usize..at as usize + scratch.len()].copy_from_slice(&scratch);
            }
            self.scratch = scratch;
            let at = at?;
            self.absolute(datum.fixups.iter(), at, classes, record.name)?;
            self.image.datums.push((datum.id, at));
            addresses.push(self.bases.data + at);
        }
        Ok(addresses)
    }

    fn statics<'r>(&mut self, records: &Records<'r>) -> Result<(), ImageError> {
        let defs = self.input.statics;
        // Every static's memory first, so that their values may name one another.
        for def in defs {
            let record = held(records, &def.record)?;
            let What::Static { thread_local, .. } = record.what else {
                return Err(no(format!("static {} is defined by `{}`, a function's record", def.number, record.name)));
            };
            let number = def.number as usize;
            if self.image.statics.get(number).is_some_and(Option::is_some) {
                return Err(no(format!("static {} (`{}`) is defined twice", def.number, record.name)));
            }
            if self.image.statics.len() <= number {
                self.image.statics.resize(number + 1, None);
            }
            self.image.statics[number] = Some(match thread_local {
                false => StaticAt::Data(self.reserve(record.size, u64::from(record.align), record.name)?),
                true => {
                    if self.input.arch != Arch::Aarch64 {
                        return Err(because(
                            Why::ThreadLocal,
                            format!(
                                "`{}` is a thread-local static, and the launcher has no thread-locals on {:?} yet",
                                record.name, self.input.arch
                            ),
                        ));
                    }
                    self.image.tls.push(TlsVar {
                        size: record.size,
                        align: u64::from(record.align),
                        init: 0,
                        init_len: 0,
                    });
                    StaticAt::Tls(self.image.tls.len() as u32 - 1)
                }
            });
        }
        for def in defs {
            let record = held(records, &def.record)?;
            let (bounds, classes) = self.bounds(&record, def.refs)?;
            let addresses = self.data_of(&record, &bounds, &classes)?;
            if record.bytes.is_empty() {
                // Zeroes are what the data holds already.
                continue;
            }
            let mut scratch = core::mem::take(&mut self.scratch);
            let placed = place::place_static(&record, &bounds, &addresses, &mut scratch)
                .map_err(|why| no(format!("the static `{}`: {why}", record.name)));
            let at = placed.and_then(|()| match self.image.statics[def.number as usize] {
                Some(StaticAt::Data(at)) => Ok(at),
                // A thread-local's initial bytes, which each thread's copy is made of.
                Some(StaticAt::Tls(var)) => {
                    let at = self.reserve(scratch.len() as u64, u64::from(record.align), record.name)?;
                    let var = &mut self.image.tls[var as usize];
                    (var.init, var.init_len) = (at, scratch.len() as u64);
                    Ok(at)
                }
                None => Err(no(format!("static {} has no place", def.number))),
            });
            if let Ok(at) = at {
                self.image.data[at as usize..at as usize + scratch.len()].copy_from_slice(&scratch);
            }
            self.scratch = scratch;
            self.absolute(record.fixups.iter(), at?, &classes, record.name)?;
        }
        Ok(())
    }

    fn bodies<'r>(&mut self, records: &Records<'r>) -> Result<(), ImageError> {
        let binds = self.input.binds;
        for bind in binds {
            let record = held(records, &bind.record)?;
            let pool = place::pool(&record, KEPT).map_err(|why| no(format!("`{}`: {why}", record.name)))?;
            if pool.len != bind.len || bind.len == 0 {
                return Err(no(format!(
                    "`{}` is {} bytes placed, and its bind says {}",
                    record.name, pool.len, bind.len
                )));
            }
            let align = u64::from(bind.align);
            if !align.is_power_of_two() {
                return Err(no(format!("`{}` is aligned to {align} bytes, which is no power of two", record.name)));
            }
            // As the core's area hands ranges out: each after the last, aligned.
            let at = (self.image.area.len() as u64).next_multiple_of(align);
            let Ok(end) = usize::try_from(at + bind.len) else {
                return Err(no(format!("`{}` ends past the address space", record.name)));
            };
            self.image.area.resize(end, 0);
            let address = self.bases.code + self.image.geometry.area + at;
            let (bounds, classes) = self.bounds(&record, bind.refs)?;
            let addresses = self.data_of(&record, &bounds, &classes)?;
            // Which of its words are addresses, before it is placed: code that cannot be moved
            // is refused as that, not as a fixup past its reach against addresses of no process.
            let mut refused = None;
            let mut fixes = Vec::new();
            place::absolutes(&record, &pool, |offset, abs| {
                let base = match abs {
                    Abs::Own => Ok(Base::Code),
                    Abs::Of(to) => Layer::base(to, &classes, record.name),
                    Abs::TlsThunk => Ok(Base::TlsThunk),
                    Abs::TlsArg(To::Place(place)) => match classes.get(place as usize) {
                        Some(Class::Tls(var)) => Ok(Base::Tls(*var)),
                        _ => Err(no(format!("`{}`: a thread-local access of place {place}, which is none", record.name))),
                    },
                    Abs::TlsArg(_) => Err(no(format!("`{}`: a thread-local access of its own bytes", record.name))),
                };
                match base {
                    Ok(base) => fixes.push(Fix { data: false, at: at + offset, base }),
                    Err(why) => refused = Some(why),
                }
            })
            .map_err(|why| because(Why::Relative, format!("`{}`: {why}", record.name)))?;
            if let Some(why) = refused {
                return Err(why);
            }
            let mut scratch = core::mem::take(&mut self.scratch);
            let placed = place::place(&record, &pool, address, &bounds, &addresses, &mut scratch)
                .map_err(|why| no(format!("`{}`: {why}", record.name)));
            if placed.is_ok() {
                self.image.area[at as usize..end].copy_from_slice(&scratch);
            }
            self.scratch = scratch;
            placed?;
            self.image.fixes.append(&mut fixes);
            let frame = match &record.unwind {
                None => None,
                Some(unwind) => {
                    let personality = unwind.personality.and_then(|it| match bounds.get(it as usize) {
                        Some(Bound::Function { thunk, .. }) => Some(*thunk),
                        _ => None,
                    });
                    let lsda = unwind.lsda.and_then(|it| addresses.get(it as usize).copied());
                    let mut scratch = core::mem::take(&mut self.scratch);
                    let made = frame::entry(unwind, address, record.size, personality, lsda, &mut scratch)
                        .map_err(|why| no(format!("`{}`: its unwind entry: {why}", record.name)));
                    let entry = made.and_then(|made| {
                        let entry = self.reserve(scratch.len() as u64, 8, record.name)?;
                        self.image.data[entry as usize..entry as usize + scratch.len()].copy_from_slice(&scratch);
                        Ok((entry, made))
                    });
                    self.scratch = scratch;
                    let (entry, made) = entry?;
                    let fixes = &mut self.image.fixes;
                    fixes.push(Fix { data: true, at: entry + made.start as u64, base: Base::Code });
                    if let Some(at) = made.personality {
                        fixes.push(Fix { data: true, at: entry + at as u64, base: Base::Code });
                    }
                    if let Some(at) = made.lsda {
                        fixes.push(Fix { data: true, at: entry + at as u64, base: Base::Data });
                    }
                    Some((entry, made.fde as u32))
                }
            };
            self.image.bodies.push(BodyAt {
                slot: bind.slot,
                at,
                len: bind.len,
                align: bind.align,
                tier: bind.tier,
                code: bind.code,
                name: String::from(record.name),
                frame,
            });
        }
        Ok(())
    }
}

/// The record `id`, for the image's instruction set.
fn held<'r>(records: &Records<'r>, id: &RecordId) -> Result<Record<'r>, ImageError> {
    records(id).ok_or_else(|| no(format!("record {:02x}{:02x}{:02x}{:02x} is named and was not given", id.0[0], id.0[1], id.0[2], id.0[3])))
}

/// What a launcher that starts with nothing has in its memory once it carried out `input`
/// (its statics defined, then its binds placed in one edit), with its mappings and its
/// process's symbols at `bases`: the image. `records` gives each record named, decoded.
///
/// With [`Bases::NONE`] this is the image the daemon keeps. With a process's own bases it
/// is, in `area` and `data`, what that image is after [`Image::rebase`] to them: the
/// refinement its test holds (`tests::image`).
pub fn lay<'r>(input: &Input<'_>, records: &Records<'r>, bases: &Bases<'_>) -> Result<Image, ImageError> {
    if !input.page.is_power_of_two() {
        return Err(no(format!("a page of {} bytes is no power of two", input.page)));
    }
    if u64::from(input.slots) > Geometry::SLOTS {
        return Err(no(format!("{} slots, past the {} a launcher's table holds", input.slots, Geometry::SLOTS)));
    }
    let mut layer = Layer {
        input: *input,
        bases: *bases,
        image: Image {
            arch: input.arch,
            os: input.os,
            page: input.page,
            geometry: Geometry::of(input.arch, input.page),
            slots: input.slots,
            entry: input.entry,
            inits: input.inits.to_vec(),
            area: Vec::new(),
            data: Vec::new(),
            fixes: Vec::new(),
            outside: Vec::new(),
            tls: Vec::new(),
            statics: Vec::new(),
            datums: Vec::new(),
            bodies: Vec::new(),
        },
        scratch: Vec::new(),
    };
    for record in input.statics.iter().map(|it| &it.record).chain(input.binds.iter().map(|it| &it.record)) {
        let record = held(records, record)?;
        if record.arch != input.arch {
            return Err(no(format!("`{}` is code for {:?}, and the launcher is {:?}", record.name, record.arch, input.arch)));
        }
    }
    layer.statics(records)?;
    layer.bodies(records)?;
    if input.entry.0 >= input.slots {
        return Err(no(format!("the entry is slot {}, and {} are declared", input.entry.0, input.slots)));
    }
    for number in input.inits {
        if !matches!(layer.image.statics.get(*number as usize), Some(Some(StaticAt::Data(_)))) {
            return Err(no(format!("static {number} is an initializer and has no address")));
        }
    }
    let bytes = layer.image.area.len() + layer.image.data.len();
    if bytes > LONGEST {
        return Err(because(
            Why::Long,
            format!("{bytes} bytes of code and data, more than the {LONGEST} one message carries"),
        ));
    }
    Ok(layer.image)
}

struct Out<'a>(&'a mut Vec<u8>);

impl Out<'_> {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        self.0.extend_from_slice(bytes);
    }
}

struct In<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> In<'a> {
    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], ImageError> {
        let end = self.at.checked_add(len).filter(|end| *end <= self.bytes.len());
        let Some(end) = end else {
            return Err(no(format!("the image ends inside {what} (byte {} of {})", self.at, self.bytes.len())));
        };
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self, what: &str) -> Result<u8, ImageError> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &str) -> Result<u32, ImageError> {
        let mut word = [0; 4];
        word.copy_from_slice(self.take(4, what)?);
        Ok(u32::from_le_bytes(word))
    }

    fn u64(&mut self, what: &str) -> Result<u64, ImageError> {
        let mut word = [0; 8];
        word.copy_from_slice(self.take(8, what)?);
        Ok(u64::from_le_bytes(word))
    }

    fn id(&mut self, what: &str) -> Result<[u8; 32], ImageError> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(32, what)?);
        Ok(id)
    }

    fn bytes(&mut self, what: &str) -> Result<&'a [u8], ImageError> {
        let len = self.u64(what)?;
        let Ok(len) = usize::try_from(len) else {
            return Err(no(format!("{what} of {len} bytes, past the address space")));
        };
        self.take(len, what)
    }

    /// A count of items of at least `least` bytes each: no more than the image has left.
    fn count(&mut self, least: usize, what: &str) -> Result<usize, ImageError> {
        let count = self.u64(what)?;
        let left = (self.bytes.len() - self.at) / least.max(1);
        match usize::try_from(count) {
            Ok(count) if count <= left => Ok(count),
            _ => Err(no(format!("{count} {what}, more than the image has bytes for"))),
        }
    }

    fn text(&mut self, what: &str) -> Result<String, ImageError> {
        let bytes = self.bytes(what)?;
        match core::str::from_utf8(bytes) {
            Ok(text) => Ok(String::from(text)),
            Err(_) => Err(no(format!("{what} that is not UTF-8"))),
        }
    }
}

impl Image {
    /// Writes the image onto the end of `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut to = Out(out);
        to.0.extend_from_slice(MAGIC);
        to.u32(VERSION);
        to.u8(match self.arch {
            Arch::X86_64 => 0,
            Arch::Aarch64 => 1,
        });
        to.u8(match self.os {
            Os::Linux => 0,
            Os::Macos => 1,
        });
        to.u64(self.page);
        to.u64(self.geometry.thunks);
        to.u64(self.geometry.thunk);
        to.u64(self.geometry.area);
        to.u32(self.slots);
        to.u32(self.entry.0);
        to.u64(self.inits.len() as u64);
        for number in &self.inits {
            to.u32(*number);
        }
        to.bytes(&self.area);
        to.bytes(&self.data);
        to.u64(self.fixes.len() as u64);
        for fix in &self.fixes {
            to.u8(u8::from(fix.data));
            let (kind, index) = match fix.base {
                Base::Code => (0, 0),
                Base::Data => (1, 0),
                Base::Outside(at) => (2, at),
                Base::TlsThunk => (3, 0),
                Base::Tls(at) => (4, at),
            };
            to.u8(kind);
            to.u32(index);
            to.u64(fix.at);
        }
        to.u64(self.outside.len() as u64);
        for (name, weak) in &self.outside {
            to.u8(u8::from(*weak));
            to.bytes(name.as_bytes());
        }
        to.u64(self.tls.len() as u64);
        for var in &self.tls {
            to.u64(var.size);
            to.u64(var.align);
            to.u64(var.init);
            to.u64(var.init_len);
        }
        to.u64(self.statics.len() as u64);
        for of in &self.statics {
            match of {
                None => {
                    to.u8(0);
                    to.u64(0);
                }
                Some(StaticAt::Data(at)) => {
                    to.u8(1);
                    to.u64(*at);
                }
                Some(StaticAt::Tls(var)) => {
                    to.u8(2);
                    to.u64(u64::from(*var));
                }
            }
        }
        to.u64(self.datums.len() as u64);
        for (id, at) in &self.datums {
            to.0.extend_from_slice(id);
            to.u64(*at);
        }
        to.u64(self.bodies.len() as u64);
        for body in &self.bodies {
            to.u32(body.slot.0);
            to.u64(body.at);
            to.u64(body.len);
            to.u32(body.align);
            to.u8(match body.tier {
                Tier::Zero => 0,
                Tier::Optimized => 1,
            });
            to.0.extend_from_slice(&body.code.0);
            match body.frame {
                None => {
                    to.u8(0);
                    to.u64(0);
                    to.u32(0);
                }
                Some((entry, fde)) => {
                    to.u8(1);
                    to.u64(entry);
                    to.u32(fde);
                }
            }
            to.bytes(body.name.as_bytes());
        }
    }

    /// Reads an image's bytes, all of them, and checks every offset in its tables against
    /// the bytes it is an offset into: nothing read from a decoded image is out of range.
    pub fn decode(bytes: &[u8]) -> Result<Image, ImageError> {
        Image::read(bytes).map_err(|refused| because(Why::Layout, refused.what))
    }

    fn read(bytes: &[u8]) -> Result<Image, ImageError> {
        let mut from = In { bytes, at: 0 };
        if from.take(8, "its magic")? != MAGIC {
            return Err(no(String::from("the bytes are no image of placed code (their first 8 are not `blazeimg`)")));
        }
        let version = from.u32("its version")?;
        if version != VERSION {
            return Err(no(format!(
                "an image of layout {version}; this launcher reads layout {VERSION}: the daemon and the \
                 launcher are of two versions of blaze"
            )));
        }
        let arch = match from.u8("its instruction set")? {
            0 => Arch::X86_64,
            1 => Arch::Aarch64,
            other => return Err(no(format!("instruction set {other}, which this layout has not"))),
        };
        let os = match from.u8("its system")? {
            0 => Os::Linux,
            1 => Os::Macos,
            other => return Err(no(format!("system {other}, which this layout has not"))),
        };
        let page = from.u64("its page size")?;
        let geometry = Geometry {
            thunks: from.u64("its geometry")?,
            thunk: from.u64("its geometry")?,
            area: from.u64("its geometry")?,
        };
        let slots = from.u32("its slots")?;
        let entry = Slot(from.u32("its entry")?);
        let count = from.count(4, "initializers")?;
        let mut inits = Vec::with_capacity(count);
        for _ in 0..count {
            inits.push(from.u32("an initializer")?);
        }
        let area = from.bytes("its code")?.to_vec();
        let data = from.bytes("its data")?.to_vec();
        let within = |data_space: bool, at: u64, len: u64, what: &str| {
            let space = if data_space { data.len() } else { area.len() } as u64;
            match at.checked_add(len) {
                Some(end) if end <= space => Ok(()),
                _ => Err(no(format!(
                    "{what} at {at} of {len} bytes, past the {space} bytes of its {}",
                    if data_space { "data" } else { "code" }
                ))),
            }
        };
        let count = from.count(14, "fixes")?;
        let mut fixes = Vec::with_capacity(count);
        for _ in 0..count {
            let in_data = from.u8("a fix")? != 0;
            let kind = from.u8("a fix")?;
            let index = from.u32("a fix")?;
            let at = from.u64("a fix")?;
            let base = match kind {
                0 => Base::Code,
                1 => Base::Data,
                2 => Base::Outside(index),
                3 => Base::TlsThunk,
                4 => Base::Tls(index),
                other => return Err(no(format!("a fix of kind {other}, which this layout has not"))),
            };
            within(in_data, at, 8, "a fix")?;
            fixes.push(Fix { data: in_data, at, base });
        }
        let count = from.count(9, "outside symbols")?;
        let mut outside = Vec::with_capacity(count);
        for _ in 0..count {
            let weak = from.u8("an outside symbol")? != 0;
            outside.push((from.text("an outside symbol's name")?, weak));
        }
        let count = from.count(32, "thread-locals")?;
        let mut tls = Vec::with_capacity(count);
        for _ in 0..count {
            let var = TlsVar {
                size: from.u64("a thread-local")?,
                align: from.u64("a thread-local")?,
                init: from.u64("a thread-local")?,
                init_len: from.u64("a thread-local")?,
            };
            within(true, var.init, var.init_len, "a thread-local's initial bytes")?;
            if var.init_len > var.size || !var.align.max(1).is_power_of_two() {
                return Err(no(String::from("a thread-local whose initial bytes are longer than it, or whose alignment is no power of two")));
            }
            tls.push(var);
        }
        let count = from.count(9, "statics")?;
        let mut statics = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = from.u8("a static")?;
            let value = from.u64("a static")?;
            statics.push(match kind {
                0 => None,
                1 => {
                    within(true, value, 1, "a static")?;
                    Some(StaticAt::Data(value))
                }
                2 if (value as usize) < tls.len() => Some(StaticAt::Tls(value as u32)),
                _ => return Err(no(format!("a static of kind {kind} and value {value}, which this image has not"))),
            });
        }
        let count = from.count(40, "data")?;
        let mut datums = Vec::with_capacity(count);
        for _ in 0..count {
            let id = from.id("a datum")?;
            let at = from.u64("a datum")?;
            within(true, at, 0, "a datum")?;
            datums.push((id, at));
        }
        let count = from.count(78, "bodies")?;
        let mut bodies = Vec::with_capacity(count);
        for _ in 0..count {
            let slot = Slot(from.u32("a body")?);
            let at = from.u64("a body")?;
            let len = from.u64("a body")?;
            let align = from.u32("a body")?;
            let tier = match from.u8("a body")? {
                0 => Tier::Zero,
                1 => Tier::Optimized,
                other => return Err(no(format!("a body of tier {other}, which this layout has not"))),
            };
            let code = CodeId(from.id("a body")?);
            let has = from.u8("a body")?;
            let entry = from.u64("a body")?;
            let fde = from.u32("a body")?;
            let name = from.text("a body's name")?;
            within(false, at, len, "a body")?;
            if slot.0 >= slots {
                return Err(no(format!("`{name}` is in slot {}, and the image declares {slots}", slot.0)));
            }
            let frame = match has {
                0 => None,
                _ => {
                    within(true, entry, u64::from(fde) + 8, "an unwind entry")?;
                    Some((entry, fde))
                }
            };
            bodies.push(BodyAt { slot, at, len, align, tier, code, name, frame });
        }
        if from.at != bytes.len() {
            return Err(no(format!("{} bytes after the image", bytes.len() - from.at)));
        }
        if entry.0 >= slots {
            return Err(no(format!("the entry is slot {}, and the image declares {slots}", entry.0)));
        }
        for fix in &fixes {
            let known = match fix.base {
                Base::Outside(at) => (at as usize) < outside.len(),
                Base::Tls(at) => (at as usize) < tls.len(),
                Base::Code | Base::Data | Base::TlsThunk => true,
            };
            if !known {
                return Err(no(format!("a fix against {:?}, which the image does not list", fix.base)));
            }
        }
        for number in &inits {
            if !matches!(statics.get(*number as usize), Some(Some(StaticAt::Data(_)))) {
                return Err(no(format!("static {number} is an initializer and has no address in the image")));
            }
        }
        Ok(Image { arch, os, page, geometry, slots, entry, inits, area, data, fixes, outside, tls, statics, datums, bodies })
    }

    /// Makes `area` and `data` what they are in a process whose mappings and symbols are at
    /// `bases`: adds to each absolute word where what it is an address of is. For an image
    /// made against [`Bases::NONE`] ([`lay`]'s), once.
    pub fn rebase(&mut self, bases: &Bases<'_>) -> Result<(), ImageError> {
        if bases.outside.len() != self.outside.len() || bases.tls.len() != self.tls.len() {
            return Err(no(format!(
                "the image names {} outside symbols and {} thread-locals, and addresses were given for {} and {}",
                self.outside.len(),
                self.tls.len(),
                bases.outside.len(),
                bases.tls.len()
            )));
        }
        for fix in &self.fixes {
            let add = match fix.base {
                Base::Code => bases.code,
                Base::Data => bases.data.wrapping_sub(DATA),
                Base::Outside(at) => bases.outside[at as usize],
                Base::TlsThunk => bases.tls_thunk,
                Base::Tls(at) => bases.tls[at as usize],
            };
            let bytes = if fix.data { &mut self.data } else { &mut self.area };
            // In range: `decode` checked every fix, and `lay` made each inside what it wrote.
            let Some(field) = bytes.get_mut(fix.at as usize..fix.at as usize + 8) else {
                return Err(no(format!("a fix at {} past its bytes", fix.at)));
            };
            let mut word = [0; 8];
            word.copy_from_slice(field);
            field.copy_from_slice(&u64::from_le_bytes(word).wrapping_add(add).to_le_bytes());
        }
        Ok(())
    }
}
