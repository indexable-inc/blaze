//! Member selection and name resolution (NATIVE.md 3.3 and 3.5, M4): which archive members a
//! program's native image holds, and what every global name of them resolves to. Sans-IO: the
//! inputs' bytes in, a [`Plan`] out; nothing of a map's order reaches the plan (every list is
//! sorted or in pull order).
//!
//! The rules are lld's (ld64's where the platforms differ), as NATIVE.md 3.3 decides:
//! - ld(1) on macOS: "ld will only pull .o files out of a static library if needed to resolve
//!   some symbol reference." The lazy table holds each global definition of each member, the
//!   first archive in link order winning the name; a plain object is always pulled.
//! - The Rust Reference (items/external-blocks.html): "+whole-archive means that the static
//!   library is linked as a whole archive without throwing any object files away": every member
//!   of such an archive is pulled.
//! - A FIFO worklist from the roots: an undefined name that is lazy pulls its member, whose own
//!   undefined names join the list. ELF: a weak undefined reference does not pull (lld ELF);
//!   Mach-O: it does (ld64).
//! - Strong over weak over common; two strong: [`ResolveError::Duplicate`]; weak against weak:
//!   the first pulled; common against common: the largest size and the largest alignment. A Rust
//!   export also defined by a pulled member: [`ResolveError::RustAndNative`].
//! - A common definition does not enter the lazy table (ranlib(1) leaves commons out of the
//!   table of contents unless `-c`; lld's default `--no-fortran-common`).
//! - An undefined name of a pulled member resolves to a pulled definition, else a Rust export (a
//!   word, NATIVE.md 3.4), else a name the linker defines (synthetic.rs: `___dso_handle`, the
//!   header names, ld64's boundary names), else an import (weak when every reference is weak). A root nothing
//!   defines and no member references is not an import and is not in [`Plan::names`]: Rust binds
//!   it at run through the image handle.
//! - ELF COMDAT groups: the first group of a signature, in pull order, wins; the sections of the
//!   rest are [`Plan::dropped`], and their symbols neither define nor reference anything.
//! - Every library comes resolved ([`Linked`]: its install name read from the SDK's stub on
//!   macOS, glibc's soname on Linux); one resolved for another platform fails before any input
//!   is read.
//!
//! Iteration only: the worklist is a vector read from a cursor.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{AVec, Alloc};

use crate::Platform;
use crate::ar::{self, ArError};
use crate::libraries::Linked;
use crate::object::{self, Bind, Def, Object, ReadError};

/// Where an input came from: the package (`name@version`) whose build script made it, and the
/// file's name under that script's OUT_DIR.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Origin<'a> {
    pub package: &'a str,
    pub file: &'a str,
}

/// What an input is: an archive (whole when `+whole-archive`), or one plain object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputKind {
    Archive { whole: bool },
    Object,
}

/// One input on the link line, in link order (NATIVE.md 3.2).
pub struct Input<'a> {
    pub from: Origin<'a>,
    pub bytes: &'a [u8],
    pub kind: InputKind,
}

/// What the program asks of its native image: the names its crates' foreign items declare
/// (`roots`), the names Rust exports (`rust`), both platform-mangled, and the libraries its
/// crates declare, resolved, in link order.
pub struct Wanted<'a> {
    pub roots: &'a [&'a [u8]],
    pub rust: &'a [&'a [u8]],
    pub libraries: &'a [Linked<'a>],
}

/// One object the image holds: input `input`'s member `member` (`u32::MAX` for a plain object).
pub struct Pulled<'a> {
    pub input: u32,
    pub member: u32,
    /// The member's name in its archive, or a plain object's file name: what a refusal about
    /// one of its relocations names (layout.rs [`crate::layout::Far`]).
    pub name: &'a [u8],
    pub object: Object<'a>,
}

/// What one global name resolves to. `object` indexes [`Plan::objects`]; `symbol` that object's
/// symbols; `word` [`Plan::words`]; `index` [`Plan::imports`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolution {
    Defined { object: u32, symbol: u32 },
    Common { object: u32, symbol: u32, size: u64, align_log2: u8 },
    Rust { word: u32 },
    Import { index: u32, weak: bool },
    /// A name the linker defines (synthetic.rs: `___dso_handle`, a header, a boundary): no
    /// pulled member defines it and no Rust word is it; the layout gives its address from the
    /// name (layout.rs `Target::Linker`).
    Linker,
}

/// The resolved link: what the layout (W3) and the record's builder (W7) take.
pub struct Plan<'a> {
    /// In pull order.
    pub objects: AVec<'a, Pulled<'a>>,
    /// Every global name referenced or defined by a pulled object, sorted by name.
    pub names: AVec<'a, (&'a [u8], Resolution)>,
    /// The Rust names C references, sorted.
    pub words: AVec<'a, &'a [u8]>,
    /// The names the image imports, sorted.
    pub imports: AVec<'a, &'a [u8]>,
    /// (object, section) of the losing COMDAT groups, in pull order.
    pub dropped: AVec<'a, (u32, u32)>,
    /// The libraries, in the order declared.
    pub libraries: AVec<'a, Linked<'a>>,
}

/// Why the link was refused, naming the input, the member or the name, and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolveError<'a> {
    /// The input is not an archive W1's reader takes (or holds more members than one plan can
    /// index: `TooMany`).
    Ar { from: Origin<'a>, why: ArError },
    /// A member (or a plain object, named by its file) is not an object W1's reader takes.
    Read { from: Origin<'a>, member: &'a [u8], why: ReadError },
    /// Two strong definitions of `name` (lld: "duplicate symbol").
    Duplicate { name: &'a [u8], first: Origin<'a>, second: Origin<'a> },
    /// A Rust export also defined by a pulled member.
    RustAndNative { name: &'a [u8], from: Origin<'a> },
    /// A library resolved for another platform than the plan's (NATIVE.md 3.5).
    Library { name: &'a str, resolved_for: Platform },
    /// An allocation was refused.
    Alloc,
}

/// A name that begins with this byte is the exact symbol that follows it, written with no
/// platform prefix. LLVM's rule (its `Mangler`: a global whose name starts with `\1` is emitted
/// as the rest, as is), which rustc passes through for a `#[link_name]` or `#[export_name]`
/// beginning with `\u{1}`: bindgen writes `#[link_name = "\u{1}_proc_listpids"]` for every name
/// it read from a header already mangled (on Mach-O the `_` is in the rest).
pub const VERBATIM: u8 = 0x01;

/// `name` as the platform's symbol tables write it: a leading `_` on Mach-O, as is on ELF; a
/// name that begins with [`VERBATIM`] is the rest, as is, on both.
pub fn mangle<'a>(platform: Platform, name: &str, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, ResolveError<'a>> {
    let (prefix, rest): (&[u8], &[u8]) = match name.as_bytes().split_first() {
        Some((&VERBATIM, rest)) => (b"", rest),
        _ => match platform {
            Platform::MacosArm64 => (b"_", name.as_bytes()),
            Platform::LinuxArm64 => (b"", name.as_bytes()),
        },
    };
    let mut out = AVec::try_with_capacity_in(prefix.len() + rest.len(), alloc).map_err(|_| ResolveError::Alloc)?;
    out.try_extend_from_slice(prefix).map_err(|_| ResolveError::Alloc)?;
    out.try_extend_from_slice(rest).map_err(|_| ResolveError::Alloc)?;
    Ok(out)
}

/// The members to pull and what every name resolves to. `seed` is the hasher of the lookup
/// tables (blaze-hash has no type named `Seed`; its seed for ordinary keys is [`FixedState`],
/// zero-sized, its own value). Nothing of the tables' order reaches the plan.
pub fn plan<'a>(
    platform: Platform,
    inputs: &[Input<'a>],
    wanted: Wanted<'a>,
    seed: FixedState,
    alloc: Alloc<'a>,
) -> Result<Plan<'a>, ResolveError<'a>> {
    let mut libraries = AVec::new_in(alloc);
    for &library in wanted.libraries {
        if library.platform() != platform {
            return Err(ResolveError::Library { name: library.library().name, resolved_for: library.platform() });
        }
        libraries.try_push(library).map_err(|_| ResolveError::Alloc)?;
    }

    let mut resolver = Resolver {
        platform,
        inputs,
        slots: AVec::new_in(alloc),
        lazy: Map::new(MapOpts { hasher: seed, alloc: alloc, capacity: None }),
        rust: Map::new(MapOpts { hasher: seed, alloc: alloc, capacity: None }),
        names: Map::new(MapOpts { hasher: seed, alloc: alloc, capacity: None }),
        groups: Map::new(MapOpts { hasher: seed, alloc: alloc, capacity: None }),
        queue: AVec::new_in(alloc),
        objects: AVec::new_in(alloc),
        dropped: AVec::new_in(alloc),
        here: AVec::new_in(alloc),
    };
    for &name in wanted.rust {
        resolver.rust.try_get_or_insert_with(&name, |n| (*n, ())).map_err(|_| ResolveError::Alloc)?;
    }
    let eager = resolver.read_inputs(alloc)?;
    for &root in wanted.roots {
        let state = name_state(&mut resolver.names, root)?;
        if !state.queued {
            state.queued = true;
            resolver.queue.try_push(root).map_err(|_| ResolveError::Alloc)?;
        }
    }
    for &slot in eager.iter() {
        resolver.pull(slot)?;
    }
    resolver.drain_queue()?;
    resolver.finish(libraries, alloc)
}

/// A definition's strength: strong over weak over common.
#[derive(Clone, Copy)]
enum Strength {
    Strong,
    Weak,
    Common { size: u64, align_log2: u8 },
}

#[derive(Clone, Copy)]
struct Definition<'a> {
    object: u32,
    symbol: u32,
    strength: Strength,
    from: Origin<'a>,
}

/// What is known of one global name.
#[derive(Clone, Copy)]
struct NameState<'a> {
    def: Option<Definition<'a>>,
    /// A pulled object references it (roots alone do not make an import).
    member_ref: bool,
    /// Some pulled object's reference to it is strong.
    strong_ref: bool,
    /// It has been put on the worklist.
    queued: bool,
}

const UNKNOWN: NameState<'static> = NameState { def: None, member_ref: false, strong_ref: false, queued: false };

/// One readable object: an archive member or a plain object, not yet pulled while `object` is
/// `Some`.
struct Slot<'a> {
    input: u32,
    member: u32,
    /// [`Pulled::name`].
    name: &'a [u8],
    object: Option<Object<'a>>,
}

type Table<'a, V> = Map<&'a [u8], V, FixedState, Alloc<'a>>;

struct Resolver<'i, 'a> {
    platform: Platform,
    inputs: &'i [Input<'a>],
    slots: AVec<'a, Slot<'a>>,
    /// Name -> the slot whose member defines it, first archive in link order winning.
    lazy: Table<'a, u32>,
    rust: Table<'a, ()>,
    names: Table<'a, NameState<'a>>,
    /// COMDAT signatures already taken.
    groups: Table<'a, ()>,
    queue: AVec<'a, &'a [u8]>,
    objects: AVec<'a, Pulled<'a>>,
    dropped: AVec<'a, (u32, u32)>,
    /// The sections of the object being pulled that a losing group dropped (scratch).
    here: AVec<'a, u32>,
}

fn name_state<'m, 'a>(names: &'m mut Table<'a, NameState<'a>>, name: &'a [u8]) -> Result<&'m mut NameState<'a>, ResolveError<'a>> {
    names.try_get_or_insert_with(&name, |n| (*n, UNKNOWN)).map_err(|_| ResolveError::Alloc)
}

/// Whether `bind` makes an undefined reference weak.
fn weak_bind(bind: Bind) -> bool {
    matches!(bind, Bind::Weak | Bind::WeakUndefined)
}

/// The lld/ld64 order of two definitions of one name: strong over weak over common.
fn merge<'a>(name: &'a [u8], old: Definition<'a>, new: Definition<'a>) -> Result<Definition<'a>, ResolveError<'a>> {
    match (old.strength, new.strength) {
        (Strength::Strong, Strength::Strong) => Err(ResolveError::Duplicate { name, first: old.from, second: new.from }),
        (Strength::Strong, _) | (Strength::Weak, Strength::Weak | Strength::Common { .. }) => Ok(old),
        (Strength::Weak, Strength::Strong) | (Strength::Common { .. }, Strength::Strong | Strength::Weak) => Ok(new),
        (Strength::Common { size: a, align_log2: x }, Strength::Common { size: b, align_log2: y }) => {
            let keep = if b > a { new } else { old };
            Ok(Definition { strength: Strength::Common { size: a.max(b), align_log2: x.max(y) }, ..keep })
        }
    }
}

impl<'i, 'a> Resolver<'i, 'a> {
    /// Reads every input in link order into slots; fills the lazy table from the members of the
    /// archives that are not whole; returns the slots pulled whatever the roots (plain objects
    /// and whole archives' members), in link order.
    fn read_inputs(&mut self, alloc: Alloc<'a>) -> Result<AVec<'a, u32>, ResolveError<'a>> {
        let mut eager = AVec::new_in(alloc);
        let inputs = self.inputs;
        for (index, input) in inputs.iter().enumerate() {
            let too_many = ResolveError::Ar { from: input.from, why: ArError::TooMany { most: u32::MAX } };
            let input_index = u32::try_from(index).map_err(|_| too_many)?;
            match input.kind {
                InputKind::Object => {
                    let object = object::read(self.platform, input.bytes, alloc).map_err(|why| ResolveError::Read {
                        from: input.from,
                        member: input.from.file.as_bytes(),
                        why,
                    })?;
                    let slot = self.add_slot(Slot { input: input_index, member: u32::MAX, name: input.from.file.as_bytes(), object: Some(object) }, too_many)?;
                    eager.try_push(slot).map_err(|_| ResolveError::Alloc)?;
                }
                InputKind::Archive { whole } => {
                    let archive = ar::read(input.bytes, alloc).map_err(|why| ResolveError::Ar { from: input.from, why })?;
                    for (position, member) in archive.members.iter().enumerate() {
                        let member_index = u32::try_from(position).map_err(|_| too_many)?;
                        let object = object::read(self.platform, member.bytes, alloc).map_err(|why| ResolveError::Read {
                            from: input.from,
                            member: member.name,
                            why,
                        })?;
                        let slot = u32::try_from(self.slots.len()).map_err(|_| too_many)?;
                        if whole {
                            eager.try_push(slot).map_err(|_| ResolveError::Alloc)?;
                        } else {
                            for symbol in object.symbols.iter() {
                                let defines = matches!(symbol.def, Def::In { .. } | Def::Absolute(_));
                                if defines && matches!(symbol.bind, Bind::Global | Bind::Weak) {
                                    // First archive in link order wins: an existing entry is kept.
                                    self.lazy
                                        .try_get_or_insert_with(&symbol.name, |n| (*n, slot))
                                        .map_err(|_| ResolveError::Alloc)?;
                                }
                            }
                        }
                        self.add_slot(Slot { input: input_index, member: member_index, name: member.name, object: Some(object) }, too_many)?;
                    }
                }
            }
        }
        Ok(eager)
    }

    fn add_slot(&mut self, slot: Slot<'a>, too_many: ResolveError<'a>) -> Result<u32, ResolveError<'a>> {
        let index = u32::try_from(self.slots.len()).map_err(|_| too_many)?;
        self.slots.try_push(slot).map_err(|_| ResolveError::Alloc)?;
        Ok(index)
    }

    /// Pops the worklist in FIFO order until it is empty; a name that is neither defined nor a
    /// Rust export and is lazy pulls its member.
    fn drain_queue(&mut self) -> Result<(), ResolveError<'a>> {
        let mut head = 0;
        while head < self.queue.len() {
            let name = self.queue[head];
            head += 1;
            if self.names.get(&name).is_some_and(|state| state.def.is_some()) || self.rust.contains_key(&name) {
                continue;
            }
            if let Some(&slot) = self.lazy.get(&name) {
                self.pull(slot)?;
            }
        }
        Ok(())
    }

    /// Pulls one slot (nothing when it is pulled already): its COMDAT groups first, then its
    /// global definitions, then its undefined references onto the worklist.
    fn pull(&mut self, slot: u32) -> Result<(), ResolveError<'a>> {
        let Resolver { platform, inputs, slots, rust, names, groups, queue, objects, dropped, here, .. } = self;
        // `slot` came from `add_slot` (the lazy table or the eager list): always in range.
        let entry = &mut slots[slot as usize];
        let Some(object) = entry.object.take() else {
            return Ok(());
        };
        let (input, member, name) = (entry.input, entry.member, entry.name);
        let from = inputs[input as usize].from;
        let too_many = ResolveError::Ar { from, why: ArError::TooMany { most: u32::MAX } };
        let index = u32::try_from(objects.len()).map_err(|_| too_many)?;
        objects.try_push(Pulled { input, member, name, object }).map_err(|_| ResolveError::Alloc)?;
        let object = &objects[index as usize].object;

        here.clear();
        for group in object.groups.iter() {
            let mut fresh = false;
            groups
                .try_get_or_insert_with(&group.signature, |signature| {
                    fresh = true;
                    (*signature, ())
                })
                .map_err(|_| ResolveError::Alloc)?;
            if !fresh {
                for &section in group.sections.iter() {
                    dropped.try_push((index, section)).map_err(|_| ResolveError::Alloc)?;
                    here.try_push(section).map_err(|_| ResolveError::Alloc)?;
                }
            }
        }

        for (position, symbol) in object.symbols.iter().enumerate() {
            if symbol.bind == Bind::Local {
                continue;
            }
            if matches!(symbol.def, Def::In { section, .. } if here.contains(&section)) {
                continue;
            }
            let name = symbol.name;
            let strength = match symbol.def {
                Def::Undefined => {
                    let weak = weak_bind(symbol.bind);
                    let state = name_state(names, name)?;
                    state.member_ref = true;
                    state.strong_ref |= !weak;
                    // lld ELF: a weak undefined reference does not pull; ld64: it does.
                    let pulls = !weak || *platform == Platform::MacosArm64;
                    if pulls && !state.queued {
                        state.queued = true;
                        queue.try_push(name).map_err(|_| ResolveError::Alloc)?;
                    }
                    continue;
                }
                Def::Common { size, align_log2 } => Strength::Common { size, align_log2 },
                Def::Absolute(_) | Def::In { .. } => {
                    if weak_bind(symbol.bind) {
                        Strength::Weak
                    } else {
                        Strength::Strong
                    }
                }
            };
            if rust.contains_key(&name) {
                return Err(ResolveError::RustAndNative { name, from });
            }
            let symbol_index = u32::try_from(position).map_err(|_| ResolveError::Read {
                from,
                member: name,
                why: ReadError::Malformed { what: "more than u32::MAX symbols in one object", at: 0 },
            })?;
            let new = Definition { object: index, symbol: symbol_index, strength, from };
            let state = name_state(names, name)?;
            state.def = Some(match state.def {
                None => new,
                Some(old) => merge(name, old, new)?,
            });
        }
        Ok(())
    }

    /// The plan: names sorted, words and imports sorted, each name's index into them assigned by
    /// walking the sorted lists together (both are subsequences of the sorted names).
    fn finish(self, libraries: AVec<'a, Linked<'a>>, alloc: Alloc<'a>) -> Result<Plan<'a>, ResolveError<'a>> {
        let mut names = AVec::new_in(alloc);
        let mut words = AVec::new_in(alloc);
        let mut imports = AVec::new_in(alloc);
        for (&name, state) in self.names.iter() {
            let resolution = match state.def {
                Some(Definition { object, symbol, strength: Strength::Common { size, align_log2 }, .. }) => {
                    Resolution::Common { object, symbol, size, align_log2 }
                }
                Some(Definition { object, symbol, .. }) => Resolution::Defined { object, symbol },
                // A root nothing defines and no member references: bound through the image handle.
                None if !state.member_ref => continue,
                None if self.rust.contains_key(&name) => {
                    words.try_push(name).map_err(|_| ResolveError::Alloc)?;
                    Resolution::Rust { word: 0 }
                }
                // No member defines it: the linker does, when it is one of its names (lld adds a
                // synthetic symbol only where the name is still undefined).
                None if crate::synthetic::of(self.platform, name).is_some() => Resolution::Linker,
                None => {
                    imports.try_push(name).map_err(|_| ResolveError::Alloc)?;
                    Resolution::Import { index: 0, weak: !state.strong_ref }
                }
            };
            names.try_push((name, resolution)).map_err(|_| ResolveError::Alloc)?;
        }
        names.sort_unstable_by(|a, b| a.0.cmp(b.0));
        words.sort_unstable();
        imports.sort_unstable();
        let (mut next_word, mut next_import) = (0u32, 0u32);
        for (_, resolution) in names.iter_mut() {
            match resolution {
                Resolution::Rust { word } => {
                    *word = next_word;
                    next_word += 1;
                }
                Resolution::Import { index, .. } => {
                    *index = next_import;
                    next_import += 1;
                }
                Resolution::Defined { .. } | Resolution::Common { .. } | Resolution::Linker => {}
            }
        }
        Ok(Plan { objects: self.objects, names, words, imports, dropped: self.dropped, libraries })
    }
}
