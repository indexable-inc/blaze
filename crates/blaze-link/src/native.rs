//! Native inputs: C objects, archives and shared libraries, and the names they and Rust
//! code share.
//!
//! The rules are lld's: objects are loaded in link-line order, archive members are pulled for
//! what is strongly undefined wherever the archive is on the line (in rounds, each round's
//! members in link-line order), the first COMDAT group of a name wins, a strong definition
//! beats a weak one beats a common one, and sections nothing reachable refers to are dropped
//! (`--gc-sections`). Rust's names are first on the line: a strong one beats every weak
//! native and is the duplicate symbol error beside a strong one; a weak one
//! ([`crate::state::RustName::weak`], `#[linkage = "weak"]`) beats weak natives and commons and
//! loses to a strong native, whose definition every reference to the name, Rust's included
//! (codegen refers to a weak definition only by its name), then binds to.
//!
//! What a stage pays is what changed. An input is read, checked and indexed once
//! ([`Parsed`]: its symbols interned, each section's referenced and defined symbols, its
//! groups and call frame records as columns), in parallel, and kept for the image's life in
//! [`Stage::objects`]. A resolution ([`Graph`]: what every name is, which members are pulled,
//! which groups win) and its collection ([`Marks`]) are kept in [`Resolved`] with how many of
//! Rust's pieces refer to each name, counted from the pieces made and removed since the last
//! stage, never by walking them. A stage then does one of three things:
//!
//! - nothing, when no native input or Rust definition changed and no name Rust refers to
//!   came or went in a way that changes what it resolves to;
//! - grows the kept resolution, when Rust refers to names it did not: they are resolved
//!   against the kept graph, and what they newly reach is marked and made, unless that needs
//!   an archive member, an alias, a special that keeps sections or a library the kept one
//!   does not have;
//! - builds the graph again from the kept columns (no input is read again) and compares it
//!   with the image's: only sections, frame records and commons that came or went, and
//!   symbols whose definition changed, are touched.

#![forbid(unsafe_code)]

use blaze_hash::hash_map::Entry;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use blaze_graph::par;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::{
    bits::locate,
    radix::{self, Csr},
};
use smallvec::SmallVec;

use crate::cow::{CowList, CowVec, ListRef};
use crate::model::{
    Apply, Archive, Binding, Class, Data, Directive, Exported, Fixup, Formats, Import, Library,
    Name, Names, NativeFile, Object, SectionKind, Shared, Special, Sym, Symbol, SymbolDef, SymbolKind, Tie,
    Writer,
};
use crate::premap::{Outside, Strength, outside_def};
use crate::stage::{Content, Stage, error};
use crate::state::{Def, Input, PieceId, State, What};
use crate::symbols::{NameId, Obj, SymKey, Symbols, Wide};
use crate::unwind;
use crate::{Change, Format, LinkError, NativeKind, OutputKind};

const NONE: u32 = u32::MAX;
const NONE64: u64 = u64::MAX;

#[inline]
fn pack(high: u32, low: u32) -> u64 {
    u64::from(high) << 32 | u64::from(low)
}

#[inline]
fn unpack(it: u64) -> (u32, u32) {
    ((it >> 32) as u32, it as u32)
}

/// A name's hash in these tables: keyed, as an object's names are the program's (or a C
/// dependency's) to pick ([`crate::symbols::keyed_name_hash`]).
fn hash(name: &str) -> u64 {
    crate::symbols::keyed_name_hash(name)
}

/// Sections kept whatever refers to them, by name, as lld keeps them.
fn kept_by_name(name: &str) -> bool {
    [".init", ".fini", ".ctors", ".dtors", ".jcr", ".preinit_array", ".init_array", ".fini_array"]
        .iter()
        .any(|it| name == *it || name.strip_prefix(it).is_some_and(|rest| rest.starts_with('.')))
        || name.starts_with(".note")
}

/// Names the core roots that an image need not define.
fn optional(name: &str) -> bool {
    name == "_init" || name == "_fini"
}

/// A symbol only its object sees: a local, or a section's own.
#[inline]
fn is_local(symbol: &Symbol) -> bool {
    symbol.binding == Binding::Local || symbol.kind == SymbolKind::Section
}

/// Whether a section of this kind is a piece of the image.
fn makes_piece(what: &SectionKind) -> bool {
    matches!(what, SectionKind::Alloc(..) | SectionKind::Unwind(_))
}

/// Whether a section is kept only through another ([`Object::ties`], [`Object::live_support`]):
/// it is no root, and no `__start_X` keeps it.
#[inline]
fn bound(object: &Object, section: u32) -> bool {
    tie_of(object, section).is_some() || object.live_support.binary_search(&section).is_ok()
}

/// What `section` is tied to, if it is.
#[inline]
fn tie_of(object: &Object, section: u32) -> Option<Tie> {
    object.ties.binary_search_by_key(&section, |it| it.section).ok().map(|it| object.ties[it])
}

/// Flags by flat index, a bit each, copied on write by chunk: a stage that marks a few more
/// sections copies the words it writes. Not a `blaze_ids::bits` set, whose words are one
/// slice: these are a [`CowVec`]'s chunks, shared with the states the stage was cloned from.
#[derive(Clone, Default)]
pub(crate) struct CowFlags(CowVec<u64>);

impl CowFlags {
    #[inline]
    fn get(&self, at: usize) -> bool {
        let (word, mask) = locate(at);
        self.0.get(word).is_some_and(|it| it & mask != 0)
    }

    /// Sets the flag; whether it was clear.
    fn set(&mut self, at: usize) -> bool {
        let (word, mask) = locate(at);
        match self.0.get(word) {
            Some(it) if it & mask != 0 => false,
            Some(_) => {
                *self.0.get_mut(word).expect("in range") |= mask;
                true
            }
            None => {
                self.0.set(word, mask, || 0);
                true
            }
        }
    }
}

/// The high half of a name's hash: what a [`Table`] slot keeps of it.
const HIGH: u64 = 0xffff_ffff_0000_0000;
/// A [`Table`] slot that holds no value.
const EMPTY: u64 = NONE as u64;

/// Names to `u32`s, open-addressed: a name is hashed once, compared only when the high half of
/// its hash matches, and the first value given a name keeps it. The names live with the
/// caller (`eq` compares a stored value's name with the one looked for).
///
/// A slot is one `u64`, 8 bytes: the high 32 bits of the hash over the value ([`EMPTY`] for
/// none), so a probe reads one cache line, and the table is at most half full: 16 to 32 bytes
/// a name. The probe starts at the hash's high bits, as well mixed as its low ones, and a table has
/// at most 2^32 slots, so the high half alone places a slot again when the table grows.
#[derive(Default, Debug)]
pub(crate) struct Table {
    slots: Vec<u64>,
    len: usize,
}

impl Table {
    fn with_capacity(len: usize) -> Table {
        let room = (len.max(4) * 2).next_power_of_two();
        // alloc: its one column, once per table (an archive's index, the exports, a build's groups)
        Table { slots: vec![EMPTY; room], len: 0 }
    }

    /// Where a hash starts probing: its high bits.
    #[inline]
    fn start(&self, hash: u64) -> usize {
        (hash >> (64 - self.slots.len().trailing_zeros())) as usize
    }

    fn find(&self, hash: u64, mut eq: impl FnMut(u32) -> bool) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let (mask, high) = (self.slots.len() - 1, hash & HIGH);
        let mut at = self.start(hash);
        loop {
            let slot = self.slots[at];
            if slot == EMPTY {
                return None;
            }
            if slot & HIGH == high && eq(slot as u32) {
                return Some(slot as u32);
            }
            at = (at + 1) & mask;
        }
    }

    /// Gives the name `value` unless a value has it already: then that value.
    fn insert(
        &mut self,
        hash: u64,
        value: u32,
        mut eq: impl FnMut(u32) -> bool,
    ) -> Result<(), u32> {
        debug_assert_ne!(value, NONE);
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let (mask, high) = (self.slots.len() - 1, hash & HIGH);
        let mut at = self.start(hash);
        loop {
            let slot = self.slots[at];
            if slot == EMPTY {
                self.slots[at] = high | u64::from(value);
                self.len += 1;
                return Ok(());
            }
            if slot & HIGH == high && eq(slot as u32) {
                return Err(slot as u32);
            }
            at = (at + 1) & mask;
        }
    }

    fn grow(&mut self) {
        let room = (self.slots.len() * 2).max(8);
        // alloc: doubling: amortized over the inserts, one column per growth
        let old = std::mem::replace(self, Table { slots: vec![EMPTY; room], len: 0 });
        let mask = room - 1;
        for slot in old.slots {
            if slot == EMPTY {
                continue;
            }
            let mut at = self.start(slot);
            while self.slots[at] != EMPTY {
                at = (at + 1) & mask;
            }
            self.slots[at] = slot;
            self.len += 1;
        }
    }
}

/// An archive's index by name, the first member of a name keeping it.
fn archive_index(archive: &Archive) -> Table {
    let mut table = Table::with_capacity(archive.index.len());
    for (at, (name, _)) in archive.index.iter().enumerate() {
        let name = archive.name(*name);
        let _ = table
            .insert(hash(name), at as u32, |it| archive.name(archive.index[it as usize].0) == name);
    }
    table
}

/// The member an archive's index says defines `name`, whose [`hash`] is `hashed`: a name
/// looked for in every searched archive is hashed once for all of them.
fn archive_find(table: &Table, archive: &Archive, hashed: u64, name: &str) -> Option<u32> {
    table
        .find(hashed, |it| archive.name(archive.index[it as usize].0) == name)
        .map(|it| archive.index[it as usize].1)
}

/// The first searched archive (in link-line order) whose index names `name`, and the member
/// it says defines it: one hash of the name, one probe of each archive's index.
fn search(archives: &[(u32, Arc<Table>)], inputs: &[Input], name: &str) -> Option<(u32, u32)> {
    let hashed = hash(name);
    archives.iter().find_map(|(input, table)| match &*inputs[*input as usize].file {
        NativeFile::Archive(archive) => {
            archive_find(table, archive, hashed, name).map(|member| (*input, member))
        }
        _ => None,
    })
}

/// Checks every index and range a reader gave, once, so that the rest reads them in place.
/// `len` is the length of the slice the file was read from.
fn check_file(file: &NativeFile, len: usize) -> Result<(), String> {
    match file {
        NativeFile::Object(object) => check_object(object, len as u64),
        NativeFile::Archive(archive) => check_archive(archive, len),
        NativeFile::Shared(shared) => check_shared(shared),
    }
}

fn check_object(object: &Object, len: u64) -> Result<(), String> {
    let names = &object.names;
    let (sections, symbols) = (object.sections.len(), object.symbols.len());
    for (at, section) in object.sections.iter().enumerate() {
        let Some(name) = names.get(section.name) else {
            // alloc: error path
            return Err(format!("section {at} has a name outside the object's names"));
        };
        match &section.data {
            Data::Input(range) if range.start > range.end || range.end > len => {
                // alloc: error path
                return Err(format!("{name}: bytes {range:?}, past the input's {len}"));
            }
            Data::Made(range)
                if range.start > range.end || range.end > object.made.len() as u64 =>
            {
                // alloc: error path
                return Err(format!(
                    "{name}: bytes {range:?}, past the {} the reader made",
                    object.made.len()
                ));
            }
            _ => {}
        }
        let fixups = &section.fixups;
        if fixups.start > fixups.end || fixups.end as usize > object.fixups.len() {
            // alloc: error path
            return Err(format!(
                "{name}: relocations {fixups:?}, past the object's {}",
                object.fixups.len()
            ));
        }
    }
    for (at, fixup) in object.fixups.iter().enumerate() {
        if fixup.symbol as usize >= symbols || fixup.minus.is_some_and(|it| it as usize >= symbols)
        {
            // alloc: error path
            return Err(format!("relocation {at} refers to a symbol past the object's {symbols}"));
        }
    }
    for (at, symbol) in object.symbols.iter().enumerate() {
        let Some(name) = names.get(symbol.name) else {
            // alloc: error path
            return Err(format!("symbol {at} has a name outside the object's names"));
        };
        match symbol.def {
            SymbolDef::Section { section, .. } if section as usize >= sections => {
                // alloc: error path
                return Err(format!(
                    "`{name}` is in section {section}, past the object's {sections}"
                ));
            }
            SymbolDef::Fallback { symbol: to, .. } if to as usize >= symbols => {
                // alloc: error path
                return Err(format!(
                    "`{name}` falls back to symbol {to}, past the object's {symbols}"
                ));
            }
            _ => {}
        }
    }
    for (at, group) in object.groups.iter().enumerate() {
        let Some(name) = names.get(group.name) else {
            // alloc: error path
            return Err(format!("COMDAT group {at} has a name outside the object's names"));
        };
        let range = &group.sections;
        if range.start > range.end || range.end as usize > object.grouped.len() {
            // alloc: error path
            return Err(format!(
                "COMDAT group `{name}`: sections {range:?}, past the object's {}",
                object.grouped.len()
            ));
        }
    }
    if let Some(it) = object.grouped.iter().find(|it| **it as usize >= sections) {
        // alloc: error path
        return Err(format!("a COMDAT group has section {it}, past the object's {sections}"));
    }
    for (at, tie) in object.ties.iter().enumerate() {
        if tie.section as usize >= sections || tie.to as usize >= sections {
            // alloc: error path
            return Err(format!(
                "section {} is tied to section {}, past the object's {sections}",
                tie.section, tie.to
            ));
        }
        if tie.section == tie.to {
            // alloc: error path
            return Err(format!("section {} is tied to itself", tie.section));
        }
        if at > 0 && object.ties[at - 1].section >= tie.section {
            // alloc: error path
            return Err(format!(
                "section {} is tied twice, or the ties are not in section order",
                tie.section
            ));
        }
    }
    if let Some(it) = object.live_support.iter().find(|it| **it as usize >= sections) {
        // alloc: error path
        return Err(format!("live support section {it}, past the object's {sections}"));
    }
    if object.live_support.windows(2).any(|it| it[0] >= it[1]) {
        // alloc: error path
        return Err(
            "the live support sections are not in section order, or one is listed twice".to_owned()
        );
    }
    for (at, import) in &object.imports {
        let Some(symbol) = object.symbols.get(*at as usize) else {
            // alloc: error path
            return Err(format!("a wasm import of symbol {at}, past the object's {symbols}"));
        };
        let name = names.at(symbol.name);
        if symbol.def != SymbolDef::Undefined || symbol.kind != SymbolKind::Function {
            // alloc: error path
            return Err(format!("a wasm import of `{name}`, which is not an undefined function"));
        }
        if names.get(import.module).is_none() || names.get(import.field).is_none() {
            // alloc: error path
            return Err(format!(
                "the wasm import of `{name}` has a name outside the object's names"
            ));
        }
        let ty = &import.ty;
        if ty.start > ty.end || ty.end > len {
            // alloc: error path
            return Err(format!(
                "the wasm import of `{name}`: its type, bytes {ty:?}, past the input's {len}"
            ));
        }
    }
    Ok(())
}

fn check_archive(archive: &Archive, len: usize) -> Result<(), String> {
    for (at, member) in archive.members.iter().enumerate() {
        let Some(name) = archive.names.get(member.name) else {
            // alloc: error path
            return Err(format!("member {at} has a name outside the archive's names"));
        };
        if member.bytes.start > member.bytes.end || member.bytes.end > len {
            // alloc: error path
            return Err(format!("member {name} past the end of the archive"));
        }
    }
    for (name, member) in &archive.index {
        let Some(name) = archive.names.get(*name) else {
            // alloc: error path
            return Err("the archive's index has a name outside its names".to_owned());
        };
        if *member as usize >= archive.members.len() {
            // alloc: error path
            return Err(format!(
                "the archive's index puts `{name}` in member {member}, past its {}",
                archive.members.len()
            ));
        }
    }
    Ok(())
}

fn check_shared(shared: &Shared) -> Result<(), String> {
    for symbol in &shared.symbols {
        let Some(name) = shared.names.get(symbol.name) else {
            // alloc: error path
            return Err(format!("{}: a symbol with a name outside its names", shared.name));
        };
        if let Some(version) = symbol.version.filter(|it| *it as usize >= shared.versions.len()) {
            // alloc: error path
            return Err(format!(
                "{}: `{name}` has version {version}, past its {}",
                shared.name,
                shared.versions.len()
            ));
        }
    }
    if shared.undefined.iter().any(|it| shared.names.get(*it).is_none()) {
        // alloc: error path
        return Err(format!("{}: an undefined symbol with a name outside its names", shared.name));
    }
    shared.more.iter().try_for_each(check_shared)
}

/// Whether the line, by its inputs' `names`, has what a linker script names as `wanted`: a
/// path as written or ending in `/wanted`, `-lNAME` as an input whose file is `libNAME.so` or
/// `libNAME.a`, `-l:FILE` as one whose file is `FILE`.
fn line_has(names: &HashSet<&str, std::alloc::Global>, wanted: &str) -> bool {
    fn file(it: &str) -> &str {
        it.rsplit('/').next().unwrap_or(it)
    }
    match wanted.strip_prefix("-l") {
        Some(lib) => match lib.strip_prefix(':') {
            Some(exact) => names.iter().any(|it| file(it) == exact),
            None => names.iter().any(|it| {
                file(it)
                    .strip_prefix("lib")
                    .and_then(|it| it.strip_prefix(lib))
                    .is_some_and(|it| it == ".so" || it == ".a")
            }),
        },
        None => {
            names.contains(wanted)
                || names.iter().any(|it| {
                    it.len() > wanted.len()
                        && it.ends_with(wanted)
                        && it.as_bytes()[it.len() - wanted.len() - 1] == b'/'
                })
        }
    }
}

/// Whether `a` and `b` name one library as link.exe matches a `/DEFAULTLIB`: by file name,
/// case-insensitively, with or without `.lib`.
fn same_library(a: &str, b: &str) -> bool {
    fn stem(it: &str) -> &str {
        let file = it.rsplit(['/', '\\']).next().unwrap_or(it);
        match file.len().checked_sub(4) {
            Some(at) if file.is_char_boundary(at) && file[at..].eq_ignore_ascii_case(".lib") => {
                &file[..at]
            }
            _ => file,
        }
    }
    stem(a).eq_ignore_ascii_case(stem(b))
}

/// link.exe's default libraries: every `/DEFAULTLIB` of the objects and archive members
/// `graph` loaded is on the line (`inputs`), but those a loaded one's `/NODEFAULTLIB` takes
/// out, and none after a bare `/NODEFAULTLIB`. The linker reads no file itself:
/// [`crate::target::locate`] puts them on the line, so one missing is an error naming it and
/// the member that asked.
fn default_libraries(graph: &Graph, inputs: &[Input]) -> Result<(), LinkError> {
    let mut left_out: SmallVec<[&str; 4]> = SmallVec::new();
    for load in &graph.loads {
        for directive in &load.parsed.object().directives {
            match directive {
                Directive::NoDefaultLib(None) => return Ok(()),
                Directive::NoDefaultLib(Some(library)) => left_out.push(library),
                _ => {}
            }
        }
    }
    let mut checked: SmallVec<[&str; 8]> = SmallVec::new();
    for load in &graph.loads {
        for library in &load.parsed.object().libraries {
            if checked.iter().any(|it| same_library(it, library)) {
                continue;
            }
            checked.push(library);
            if left_out.iter().any(|it| same_library(it, library))
                || inputs.iter().any(|it| same_library(&it.name, library))
            {
                continue;
            }
            return Err(error(
                None,
                // alloc: error path
                format!(
                    "{} asks for {library} (/DEFAULTLIB), which the link line does not have: the link \
                     was located without the libraries its archives' members ask for",
                    asker(inputs, load)
                ),
            ));
        }
    }
    Ok(())
}

/// ld64's autolinking: every library an `LC_LINKER_OPTION` of the objects and archive members
/// `graph` loaded asks for is on the line (`inputs`), by the file ld64 finds it as
/// ([`autolinked`]). The linker reads no file itself: [`crate::target::locate`] puts them on
/// the line, so one missing, or an option blaze does not follow ([`crate::macho::autolink`]),
/// is an error naming it and the member that asked.
fn linker_options(graph: &Graph, inputs: &[Input]) -> Result<(), LinkError> {
    let mut checked: SmallVec<[&str; 8]> = SmallVec::new();
    for load in &graph.loads {
        for option in &load.parsed.object().libraries {
            if checked.contains(&option.as_str()) {
                continue;
            }
            // alloc: error path
            let asked = crate::macho::autolink(option)
                .map_err(|what| error(None, format!("{}: {what}", asker(inputs, load))))?;
            if !autolinked(inputs, asked) {
                return Err(error(
                    None,
                    // alloc: error path
                    format!(
                        "{} asks for {option} (LC_LINKER_OPTION), which the link line does not have: the \
                         link was located without the libraries its objects ask for",
                        asker(inputs, load)
                    ),
                ));
            }
            checked.push(option);
        }
    }
    Ok(())
}

/// Whether an input is the file ld64 takes `asked` as: `libNAME.tbd`, `.dylib`, `.so` or `.a`
/// for `-lNAME`, `NAME.framework/NAME.tbd` or `NAME.framework/NAME` for `-framework NAME`.
fn autolinked(inputs: &[Input], asked: crate::macho::Autolink<'_>) -> bool {
    inputs.iter().any(|input| {
        let mut parts = input.name.rsplit('/');
        let file = parts.next().unwrap_or_default();
        match asked {
            crate::macho::Autolink::Library(name) => file
                .strip_prefix("lib")
                .and_then(|it| it.strip_prefix(name))
                .is_some_and(|it| matches!(it, ".tbd" | ".dylib" | ".so" | ".a")),
            crate::macho::Autolink::Framework(name) => {
                (file == name || file.strip_suffix(".tbd") == Some(name))
                    && parts.next().and_then(|it| it.strip_suffix(".framework")) == Some(name)
            }
        }
    })
}

/// What errors call the object `load`: its input's name, and the member's in an archive's.
fn asker(inputs: &[Input], load: &Load) -> String {
    let input = &inputs[load.input as usize];
    let member = match (&*input.file, load.parsed.obj.member.checked_sub(1)) {
        (NativeFile::Archive(archive), Some(at)) => {
            archive.members.get(at as usize).map(|it| archive.name(it.name))
        }
        _ => None,
    };
    match member {
        // alloc: error path
        Some(member) => format!("{}({member})", input.name),
        // alloc: error path
        None => input.name.to_string(),
    }
}

/// The bytes of each native input this stage, by its index in [`State::inputs`]: what native
/// sections and frame records borrow, so that they are never copied.
#[derive(Default)]
pub(crate) struct Pass<'a> {
    bytes: Vec<&'a [u8]>,
    filled: bool,
}

impl<'a> Pass<'a> {
    /// Input `input`'s bytes: as [`Stage::inputs`] filled them, or, when the change named no
    /// native and the inputs are the image's, `old`'s own.
    #[inline]
    fn input(&self, old: &'a State, input: u32) -> &'a [u8] {
        if self.filled { self.bytes[input as usize] } else { &old.inputs[input as usize].bytes[..] }
    }
}

/// A native input as the image keeps it across stages, by [`Obj`].
#[derive(Clone)]
pub(crate) enum Read {
    Object(Arc<Parsed>),
    /// An import library's member: one DLL's symbols.
    Shared(Arc<NativeFile>),
    /// A searched archive's index (at member 0, which no object of an archive is).
    Index(Arc<Table>),
}

/// A record of a native `.eh_frame` section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Frame {
    section: u32,
    start: u32,
    end: u32,
    fde: bool,
    /// An FDE's CIE: its index among the object's frame records (NONE when the section has
    /// no record there).
    cie: u32,
    /// An FDE's function: the symbol its address (at 8) is relocated by, NONE for none.
    function: u32,
}

/// An object read, checked, interned and indexed once, kept for the image's life.
pub(crate) struct Parsed {
    obj: Obj,
    file: Arc<NativeFile>,
    /// Where the object starts in its input's bytes (an archive member's offset).
    base: usize,
    /// Each symbol's image symbol, then each section's, then each frame record's.
    syms: Vec<Sym>,
    sections_at: usize,
    frames_at: usize,
    /// How many of its symbols are globals: the most slots it adds to a graph.
    globals: usize,
    /// Of each section, the symbols its relocations refer to (and subtract), each once.
    refs: Csr<u32>,
    /// Of each section, the symbols defined in it.
    defs: Csr<u32>,
    /// Of each section, its COMDAT group; NONE for none.
    group_of: Vec<u32>,
    /// Of each section, the sections tied to it and the one it is tied to ([`Object::ties`]):
    /// each is kept when the other is. No rows when the object has no ties.
    ties: Csr<u32>,
    /// Sections kept whatever refers to them.
    roots: Vec<u32>,
    /// Unwind sections and the symbol of their first relocation: what each describes.
    unwind: Vec<(u32, u32)>,
    /// `Class::Named` sections, by their names.
    named: Vec<(NameId, u32)>,
    frames: Vec<Frame>,
    /// Of each frame record, its relocations (indexes into `Object::fixups`).
    frame_fixups: Csr<u32>,
}

impl Parsed {
    /// Indexes `file`, an object: `syms` its symbols' symbols, then its sections', then those
    /// of `records` (each `.eh_frame` section's, in order); `named` the names of its
    /// `Class::Named` sections, in order.
    fn new(
        obj: Obj,
        file: Arc<NativeFile>,
        base: usize,
        syms: Vec<Sym>,
        named: &[NameId],
        records: &[(u32, Vec<unwind::Native>)],
    ) -> Parsed {
        let NativeFile::Object(object) = &*file else { unreachable!("only objects are parsed") };
        let sections = object.sections.len();
        let minus = object.fixups.iter().filter(|it| it.minus.is_some()).count();
        let mut pairs: Vec<(u32, u32)> = Vec::with_capacity(
            (object.fixups.len() + minus).max(object.symbols.len()).max(2 * object.ties.len()),
        );
        for (at, section) in object.sections.iter().enumerate() {
            let start = pairs.len();
            for fixup in object.section_fixups(section) {
                pairs.push((at as u32, fixup.symbol));
                if let Some(it) = fixup.minus {
                    pairs.push((at as u32, it));
                }
            }
            pairs[start..].sort_unstable();
            let mut kept = start;
            for next in start..pairs.len() {
                if kept == start || pairs[kept - 1] != pairs[next] {
                    pairs[kept] = pairs[next];
                    kept += 1;
                }
            }
            pairs.truncate(kept);
        }
        let refs = Csr::build(sections, pairs.iter().copied());
        pairs.clear();
        for (at, symbol) in object.symbols.iter().enumerate() {
            if let SymbolDef::Section { section, .. } = symbol.def {
                pairs.push((section, at as u32));
            }
        }
        let defs = Csr::build(sections, pairs.iter().copied());
        let ties = if object.ties.is_empty() {
            Csr::default()
        } else {
            pairs.clear();
            pairs.extend(
                object.ties.iter().flat_map(|it| [(it.to, it.section), (it.section, it.to)]),
            );
            Csr::build(sections, pairs.iter().copied())
        };
        // alloc: once per object read, kept with it
        let mut group_of = vec![NONE; sections];
        for (at, group) in object.groups.iter().enumerate() {
            for section in object.group_sections(group) {
                let it = &mut group_of[*section as usize];
                if *it == NONE {
                    *it = at as u32;
                }
            }
        }
        // A section kept only through another is a root only when it says so itself (lld:
        // `SHF_GNU_RETAIN` before `SHF_LINK_ORDER`).
        let root = |at: u32, section: &crate::model::Section| {
            section.retain
                || (!bound(object, at)
                    && (kept_by_name(object.name(section.name))
                        || matches!(
                            section.what,
                            SectionKind::Alloc(
                                Class::PreinitArray | Class::InitArray | Class::FiniArray,
                                _
                            )
                        )))
        };
        let (mut root_count, mut unwind_count) = (0, 0);
        for (at, section) in object.sections.iter().enumerate() {
            match &section.what {
                SectionKind::Dropped | SectionKind::EhFrame => continue,
                SectionKind::Unwind(_) => unwind_count += 1,
                SectionKind::Alloc(..) => {}
            }
            root_count += usize::from(root(at as u32, section));
        }
        let mut roots = Vec::with_capacity(root_count);
        let mut unwind = Vec::with_capacity(unwind_count);
        let mut named_sections = Vec::with_capacity(named.len());
        let mut names = named.iter();
        for (at, section) in object.sections.iter().enumerate() {
            let at = at as u32;
            match &section.what {
                SectionKind::Dropped | SectionKind::EhFrame => continue,
                SectionKind::Unwind(_) => {
                    let first = object.section_fixups(section).iter().min_by_key(|it| it.at);
                    unwind.push((at, first.map_or(NONE, |it| it.symbol)));
                }
                SectionKind::Alloc(Class::Named { .. }, _) => {
                    let name = *names.next().expect("a name per named section");
                    // `__start_X` keeps no section kept only through another (lld's
                    // `cNamedSections` leave out `SHF_LINK_ORDER`).
                    if !bound(object, at) {
                        named_sections.push((name, at));
                    }
                }
                SectionKind::Alloc(..) => {}
            }
            if root(at, section) {
                roots.push(at);
            }
        }
        let frame_count: usize = records.iter().map(|(_, it)| it.len()).sum();
        let frame_relocs = |section: &u32| object.sections[*section as usize].fixups.len();
        let mut frames: Vec<Frame> = Vec::with_capacity(frame_count);
        let mut fixup_pairs: Vec<(u32, u32)> =
            Vec::with_capacity(records.iter().map(|(it, _)| frame_relocs(it)).sum());
        let mut by_at: Vec<(u64, u32)> =
            Vec::with_capacity(records.iter().map(|(it, _)| frame_relocs(it)).max().unwrap_or(0));
        for (section, records) in records {
            let s = &object.sections[*section as usize];
            by_at.clear();
            by_at.extend(
                (s.fixups.start..s.fixups.end).map(|it| (object.fixups[it as usize].at, it)),
            );
            radix::sort_copy_by_key(&mut by_at, |&(at, it)| radix::pack_64(at, u64::from(it)));
            let first = frames.len();
            for record in records {
                let (start, end) = (record.range.start as u64, record.range.end as u64);
                let (lo, hi) = (
                    by_at.partition_point(|it| it.0 < start),
                    by_at.partition_point(|it| it.0 < end),
                );
                let index = frames.len() as u32;
                let mut function = NONE;
                for &(at, fixup) in &by_at[lo..hi] {
                    fixup_pairs.push((index, fixup));
                    if at == start + 8 && function == NONE {
                        function = object.fixups[fixup as usize].symbol;
                    }
                }
                let cie = record.cie.map_or(NONE, |cie| {
                    frames[first..]
                        .binary_search_by_key(&(cie as u32), |it| it.start)
                        .map_or(NONE, |it| (first + it) as u32)
                });
                frames.push(Frame {
                    section: *section,
                    start: start as u32,
                    end: end as u32,
                    fde: record.cie.is_some(),
                    cie,
                    function,
                });
            }
        }
        let frame_fixups = if frames.is_empty() {
            Csr::default()
        } else {
            Csr::build(frames.len(), fixup_pairs.iter().copied())
        };
        let sections_at = object.symbols.len();
        let frames_at = sections_at + sections;
        debug_assert_eq!(syms.len(), frames_at + frames.len());
        let globals = object.symbols.iter().filter(|it| !is_local(it)).count();
        Parsed {
            obj,
            base,
            syms,
            sections_at,
            frames_at,
            globals,
            refs,
            defs,
            group_of,
            ties,
            roots,
            unwind,
            named: named_sections,
            frames,
            frame_fixups,
            file,
        }
    }

    fn object(&self) -> &Object {
        match &*self.file {
            NativeFile::Object(it) => it,
            _ => unreachable!("only objects are parsed"),
        }
    }

    #[inline]
    fn sym(&self, symbol: u32) -> Sym {
        self.syms[symbol as usize]
    }

    #[inline]
    fn section_sym(&self, section: u32) -> Sym {
        self.syms[self.sections_at + section as usize]
    }

    #[inline]
    fn frame_sym(&self, frame: u32) -> Sym {
        self.syms[self.frames_at + frame as usize]
    }
}

/// The keys of an object's symbols, sections and frame records, in [`Parsed::new`]'s order.
fn object_keys<'f>(
    obj: Obj,
    object: &'f Object,
    records: &[(u32, Vec<unwind::Native>)],
    out: &mut Vec<Wide<'f>>,
) {
    for (at, symbol) in object.symbols.iter().enumerate() {
        out.push(match (symbol.binding, symbol.kind, symbol.def) {
            (_, SymbolKind::Section, SymbolDef::Section { section, .. }) => {
                Wide::Key(SymKey::Section(obj, section))
            }
            (Binding::Local, _, _) => Wide::Key(SymKey::Local(obj, at as u32)),
            _ => Wide::Name(object.name(symbol.name)),
        });
    }
    out.extend((0..object.sections.len() as u32).map(|it| Wide::Key(SymKey::Section(obj, it))));
    for (section, records) in records {
        out.extend(
            records.iter().map(|it| Wide::Key(SymKey::Frame(obj, *section, it.range.start as u32))),
        );
    }
}

/// A native object's symbol, section or call frame record as an error names it: the input by
/// its path (`lib.a(member.o)` for an archive's member), a symbol by its name and where it
/// is, a record of `.eh_frame`/`__eh_frame` by its offset and the function its FDE covers.
/// Any other symbol as [`Symbols::describe`] says it. The input is the first of `states`'
/// with it; an archive's member is read again, so this is for errors only
/// ([`crate::stage::named_in`]).
pub(crate) fn describe(symbols: &Symbols, states: &[&State], writer: &Formats, sym: Sym) -> String {
    let key = symbols.key(sym);
    let obj = match key {
        SymKey::Local(obj, _) | SymKey::Section(obj, _) | SymKey::Frame(obj, ..) => obj,
        _ => return symbols.describe(sym),
    };
    let found = states
        .iter()
        .flat_map(|it| it.inputs.iter())
        .find(|it| symbols.find_id(&it.id) == Some(obj.native));
    let Some(input) = found else {
        // alloc: error path
        return format!("{} (of a native input no longer on the link line)", symbols.describe(sym));
    };
    let read;
    let (path, object) = match (&*input.file, obj.member) {
        // alloc: error path
        (NativeFile::Object(object), 0) => (input.name.to_string(), object),
        (NativeFile::Archive(archive), member)
            if member as usize <= archive.members.len() && member > 0 =>
        {
            let entry = &archive.members[member as usize - 1];
            // alloc: error path
            let path = format!("{}({})", input.name, archive.name(entry.name));
            read = input
                .bytes
                .get(entry.bytes.clone())
                // alloc: error path
                .ok_or_else(|| "the member is past the archive's end".to_owned())
                .and_then(|bytes| writer.read(&input.name, bytes, &NativeKind::Object));
            match &read {
                Ok(NativeFile::Object(object)) => (path, object),
                // alloc: error path
                Ok(_) => {
                    return format!("{} of {path}, which is not an object", symbols.describe(sym));
                }
                // alloc: error path
                Err(why) => {
                    return format!(
                        "{} of {path}, which doesn't read: {why}",
                        symbols.describe(sym)
                    );
                }
            }
        }
        // alloc: error path
        _ => {
            return format!(
                "{} of {}, which has no member {}",
                symbols.describe(sym),
                input.name,
                obj.member
            );
        }
    };
    match key {
        // alloc: error path
        SymKey::Local(_, at) => format!("{} of {path}", symbol_in(object, at)),
        // alloc: error path
        SymKey::Section(_, at) => format!("{} of {path}", section_in(object, at)),
        SymKey::Frame(_, section, start) => {
            let fde = (object.sections.get(section as usize)).and_then(|it| {
                object.section_fixups(it).iter().find(|it| it.at == u64::from(start) + 8)
            });
            // alloc: error path
            let record = format!(
                "the call frame record at {start:#x} of {} of {path}",
                section_in(object, section)
            );
            match fde {
                // alloc: error path
                Some(it) => format!("{record} (the FDE of {})", symbol_in(object, it.symbol)),
                None => record,
            }
        }
        _ => unreachable!("only a native object's keys get here"),
    }
}

/// Symbol `at` of `object` for an error: its name (a section's symbol by the named symbol at
/// the same place, when there is one) and where it is defined.
fn symbol_in(object: &Object, at: u32) -> String {
    let Some(symbol) = object.symbols.get(at as usize) else {
        // alloc: error path
        return format!("symbol {at}, which the object doesn't have");
    };
    let SymbolDef::Section { section, value, .. } = symbol.def else {
        // alloc: error path
        return format!("`{}` (symbol {at}, not defined in a section)", object.name(symbol.name));
    };
    let named = if symbol.kind == SymbolKind::Section {
        (object.symbols.iter())
            .filter(|it| it.kind != SymbolKind::Section && !object.name(it.name).is_empty())
            .find(|it| matches!(it.def, SymbolDef::Section { section: s, value: v, .. } if s == section && v == value))
            .map(|it| object.name(it.name))
    } else {
        Some(object.name(symbol.name))
    };
    let place = section_in(object, section);
    match named {
        // alloc: error path
        Some(name) => format!("`{name}` (symbol {at}, at {value:#x} of {place})"),
        // alloc: error path
        None => format!("{value:#x} of {place}"),
    }
}

/// Section `at` of `object` for an error: its index and name.
fn section_in(object: &Object, at: u32) -> String {
    match object.sections.get(at as usize) {
        // alloc: error path
        Some(section) => format!("section {at} (`{}`)", object.name(section.name)),
        // alloc: error path
        None => format!("section {at}, which the object doesn't have"),
    }
}

/// What the shared libraries of a link export: every library they stand for, flattened, and
/// of each name the first library's symbol; and the names they leave undefined. Built when
/// the libraries change, not per stage.
#[derive(Default)]
pub(crate) struct Exports {
    /// File, `as_needed`, place on the link line ([`crate::Native::order`]).
    roots: Vec<(Arc<NativeFile>, bool, u32)>,
    libraries: Vec<Library>,
    /// Of each library, its root's place on the link line.
    orders: Vec<u32>,
    /// Of each library, the first of its name (an import library's members of one DLL).
    canonical: Vec<u32>,
    names: Names,
    versions: Vec<String>,
    symbols: Vec<Export>,
    table: Table,
    /// The names the libraries refer to and leave undefined, each once, by name in `wanted`.
    undefined: Vec<Name>,
    wanted: Table,
}

#[derive(Clone, Copy)]
struct Export {
    name: Name,
    /// Its library's canonical index.
    library: u32,
    /// Index into `Exports::versions`; NONE for none.
    version: u32,
    kind: SymbolKind,
    weak: bool,
}

impl Exports {
    /// Once per change of the link's shared libraries: every buffer is sized once, but for the
    /// flattened list (as deep as the libraries' re-exports) and each distinct version's name.
    fn build(roots: Vec<(Arc<NativeFile>, bool, u32)>) -> Result<Exports, String> {
        let mut flat: Vec<(&Shared, bool)> = Vec::with_capacity(roots.len());
        let mut orders: Vec<u32> = Vec::with_capacity(roots.len());
        for (file, as_needed, order) in &roots {
            let NativeFile::Shared(shared) = &**file else { continue };
            let mut at = flat.len();
            flat.push((shared, *as_needed));
            while at < flat.len() {
                let (this, as_needed) = flat[at];
                flat.extend(this.more.iter().map(|it| (it, as_needed)));
                at += 1;
            }
            orders.resize(flat.len(), *order);
        }
        let mut first: HashMap<&str, u32, std::alloc::Global> =
            HashMap::with_capacity_in(flat.len(), std::alloc::Global);
        let mut canonical: Vec<u32> = Vec::with_capacity(flat.len());
        canonical.extend(
            flat.iter()
                .enumerate()
                .map(|(at, (it, _))| *first.entry(it.name.as_str()).or_insert(at as u32)),
        );
        let mut libraries = Vec::with_capacity(flat.len());
        // alloc: each library's own name (the writers' `Library`), once per change of the libraries
        libraries.extend(
            flat.iter()
                .map(|(it, as_needed)| Library { name: it.name.clone(), as_needed: *as_needed }),
        );
        let count: usize = flat.iter().map(|it| it.0.symbols.len()).sum();
        let refers: usize = flat.iter().map(|it| it.0.undefined.len()).sum();
        let mut names = Names::with_capacity(flat.iter().map(|it| it.0.names.bytes()).sum());
        let mut undefined: Vec<Name> = Vec::with_capacity(refers);
        let mut wanted = if refers == 0 { Table::default() } else { Table::with_capacity(refers) };
        let mut table = Table::with_capacity(count);
        let mut symbols: Vec<Export> = Vec::with_capacity(count);
        let distinct: usize = flat.iter().map(|it| it.0.versions.len()).sum();
        let mut versions: Vec<String> = Vec::with_capacity(distinct);
        let mut version_of: HashMap<&str, u32, std::alloc::Global> =
            HashMap::with_capacity_in(distinct, std::alloc::Global);
        for (at, (shared, _)) in flat.iter().enumerate() {
            for symbol in &shared.symbols {
                let name = shared.symbol_name(symbol);
                // One probe: the slot is taken for the export pushed next, unless an earlier
                // library's symbol has the name.
                let index = symbols.len() as u32;
                if table
                    .insert(hash(name), index, |it| names.at(symbols[it as usize].name) == name)
                    .is_err()
                {
                    continue;
                }
                let version = match shared.version(symbol) {
                    None => NONE,
                    Some(version) => *version_of.entry(version).or_insert_with(|| {
                        // alloc: each distinct version's name, once per change of the libraries
                        versions.push(version.to_owned());
                        (versions.len() - 1) as u32
                    }),
                };
                let stored = names.push(name)?;
                symbols.push(Export {
                    name: stored,
                    library: canonical[at],
                    version,
                    kind: symbol.kind,
                    weak: symbol.weak,
                });
            }
            for name in &shared.undefined {
                let name = shared.names.at(*name);
                let index = undefined.len() as u32;
                if wanted
                    .insert(hash(name), index, |it| names.at(undefined[it as usize]) == name)
                    .is_ok()
                {
                    undefined.push(names.push(name)?);
                }
            }
        }
        Ok(Exports {
            roots,
            libraries,
            orders,
            canonical,
            names,
            versions,
            symbols,
            table,
            undefined,
            wanted,
        })
    }

    /// Whether a library refers to `name` and leaves it undefined.
    fn refers(&self, name: &str) -> bool {
        self.wanted
            .find(hash(name), |it| self.names.at(self.undefined[it as usize]) == name)
            .is_some()
    }

    fn find(&self, name: &str) -> Option<&Export> {
        self.table
            .find(hash(name), |it| self.names.at(self.symbols[it as usize].name) == name)
            .map(|it| &self.symbols[it as usize])
    }

    fn version(&self, export: &Export) -> Option<String> {
        // alloc: the writers' `Import` owns its version: once per import a resolution makes
        (export.version != NONE).then(|| self.versions[export.version as usize].clone())
    }

    /// Whether these are the libraries it was built from.
    fn same(&self, roots: &[(Arc<NativeFile>, bool, u32)]) -> bool {
        self.roots.len() == roots.len()
            && self
                .roots
                .iter()
                .zip(roots)
                .all(|(a, b)| Arc::ptr_eq(&a.0, &b.0) && a.1 == b.1 && a.2 == b.2)
    }
}

// A slot's flags.
/// Rust's definition is the name's: Rust defines it strongly, or weakly ([`RUST_WEAK`]) and
/// nothing defines it strongly.
const RUST: u16 = 1;
/// Archives are searched for it: strongly undefined in a loaded object, a searching
/// fallback, a directive's, or a strong root.
const PULL: u16 = 2;
/// The link must define it: `/INCLUDE`, `/EXPORT`, what a taken fallback falls back to.
const REQUIRED: u16 = 4;
/// Collection starts from it, and it is resolved: the entry, exports (a shared library's,
/// and what an ELF executable exports for the shared libraries it links), directives.
const ROOT: u16 = 8;
/// Referred to strongly whatever Rust does: the entry, exports.
const STRONG: u16 = 16;
/// Nothing defines it: it is an alias of its fallback.
const ALIAS: u16 = 32;
const QUEUED: u16 = 64;
/// Collection starts from it, but it is resolved only when something refers to it:
/// `_init`, `_fini`.
const LOOSE: u16 = 128;
/// Rust refers to it (a root, resolved).
const WANTED: u16 = 256;
/// The image this one is linked beside defines it, and wins ([`crate::Outside`]): its
/// definition is [`Graph::outside`]'s, and `best` is NONE64.
const OUTSIDE: u16 = 512;
/// Rust defines the name weakly ([`crate::state::RustName::weak`]). With [`RUST`] while
/// nothing defines it strongly; a strong native definition (`best`, then) or the image
/// beside's ([`OUTSIDE`]) takes the name, and [`RUST`] goes, so the slot is resolved as one
/// Rust does not define.
const RUST_WEAK: u16 = 1024;

/// A global of the graph: a name loaded objects mention, or Rust refers to or defines.
#[derive(Clone, Copy)]
struct Slot {
    sym: Sym,
    /// The winning definition, `pack(load, symbol)`; NONE64 for none.
    best: u64,
    /// Of `best`: 1 common, 2 weak, 3 strong.
    strength: u8,
    flags: u16,
    /// The first `SymbolDef::Fallback` of the name, `pack(load, symbol)`; NONE64 for none.
    fallback: u64,
}

/// An object of the graph.
struct Load {
    parsed: Arc<Parsed>,
    /// Its index in [`State::inputs`].
    input: u32,
    /// An archive member pulled for a name.
    pulled: bool,
    /// Where its sections and frame records start in the graph's flat numbering.
    sections: u32,
    frames: u32,
    /// Of each symbol, its slot; NONE for a local.
    slots: Vec<u32>,
}

static GRAPHS: AtomicU64 = AtomicU64::new(1);

/// A resolution of the natives: the objects loaded, what every global is, which COMDAT groups
/// won, and the indexes collection walks. Built when the natives or Rust's definitions
/// change, shared by the stages after.
pub(crate) struct Graph {
    /// A new number each time a graph is built (tests read it).
    #[cfg_attr(not(test), allow(dead_code))]
    id: u64,
    loads: Vec<Load>,
    by_obj: HashMap<Obj, u32, std::alloc::Global>,
    slot_of: HashMap<Sym, u32, std::alloc::Global>,
    slots: Vec<Slot>,
    /// Sections (flat) of a group that lost.
    discarded: CowFlags,
    sections: u32,
    frames: u32,
    /// Of each flat section, the unwind sections (flat) and FDEs (flat) describing it.
    unwinds: Csr<u32>,
    fdes: Csr<u32>,
    /// Of each flat section, the live support sections (flat) referring to it, which are kept
    /// once it is (ld64's `dontDeadStripIfReferencesLive`).
    supports: Csr<u32>,
    /// `Class::Named` sections (flat) by name, sorted: a name's are a run.
    named: Vec<(NameId, u32)>,
    /// Sections (flat) kept whatever refers to them.
    roots: Vec<u32>,
    /// The archives searched for names, in link-line order: input and index.
    archives: Vec<(u32, Arc<Table>)>,
    exports: Arc<Exports>,
    /// What resolution has the image export: the objects' `/EXPORT` directives', then, by
    /// name, what an ELF executable defines that a shared library it links defines or
    /// refers to ([`Stage::export_for_libraries`]).
    asked: Arc<Vec<Exported>>,
    /// wasm: what objects say a name is imported as, and the input saying it.
    wasm: HashMap<Sym, (u32, blaze_codegen::wasm::Import), std::alloc::Global>,
    entry: Option<Sym>,
    /// Of each [`OUTSIDE`] slot, what the image beside defines it as.
    outside: HashMap<u32, Def, std::alloc::Global>,
}

impl Graph {
    /// Empty: nothing is allocated until the graph is loaded; [`Graph::index`] builds the
    /// indexes collection walks before any walk.
    fn new() -> Graph {
        Graph {
            id: GRAPHS.fetch_add(1, Ordering::Relaxed),
            // alloc: none (an empty `Vec`); the loads, once per graph built
            loads: Vec::new(),
            by_obj: HashMap::new_in(std::alloc::Global),
            slot_of: HashMap::new_in(std::alloc::Global),
            // alloc: none (an empty `Vec`); the slots, once per graph built
            slots: Vec::new(),
            discarded: CowFlags::default(),
            sections: 0,
            frames: 0,
            unwinds: Csr::default(),
            fdes: Csr::default(),
            supports: Csr::default(),
            // alloc: none (an empty `Vec`), replaced by `Graph::index`
            named: Vec::new(),
            // alloc: none (an empty `Vec`), replaced by `Graph::index`
            roots: Vec::new(),
            // alloc: none (an empty `Vec`), replaced once per graph built
            archives: Vec::new(),
            exports: Arc::default(),
            asked: Arc::default(),
            wasm: HashMap::new_in(std::alloc::Global),
            entry: None,
            outside: HashMap::new_in(std::alloc::Global),
        }
    }

    fn slot(&mut self, sym: Sym) -> u32 {
        let next = self.slots.len() as u32;
        match self.slot_of.entry(sym) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                it.insert(next);
                self.slots.push(Slot {
                    sym,
                    best: NONE64,
                    strength: 0,
                    flags: 0,
                    fallback: NONE64,
                });
                next
            }
        }
    }

    #[inline]
    fn object(&self, load: u32) -> &Object {
        self.loads[load as usize].parsed.object()
    }

    #[inline]
    fn flat(&self, load: u32, section: u32) -> usize {
        (self.loads[load as usize].sections + section) as usize
    }

    fn unflat(&self, flat: u32) -> (u32, u32) {
        let load = self.loads.partition_point(|it| it.sections <= flat) - 1;
        (load as u32, flat - self.loads[load].sections)
    }

    fn unflat_frame(&self, flat: u32) -> (u32, u32) {
        let load = self.loads.partition_point(|it| it.frames <= flat) - 1;
        (load as u32, flat - self.loads[load].frames)
    }

    /// The winning definition, unless a later group's win dropped its section.
    fn best(&self, slot: u32) -> Option<(u32, u32)> {
        let it = self.slots[slot as usize].best;
        if it == NONE64 {
            return None;
        }
        let (load, symbol) = unpack(it);
        match self.object(load).symbols[symbol as usize].def {
            SymbolDef::Section { section, .. } if self.discarded.get(self.flat(load, section)) => {
                None
            }
            _ => Some((load, symbol)),
        }
    }

    /// Whether a load's symbol is its name's winning definition.
    fn wins(&self, load: u32, symbol: u32) -> bool {
        let slot = self.loads[load as usize].slots[symbol as usize];
        slot != NONE
            && self.slots[slot as usize].best == pack(load, symbol)
            && self.best(slot).is_some()
    }

    /// Whether the natives, Rust or the image beside define the name.
    fn defined(&self, slot: u32) -> bool {
        self.slots[slot as usize].flags & (RUST | ALIAS | OUTSIDE) != 0 || self.best(slot).is_some()
    }

    /// What a defined name is, as [`Outcome`] kinds: a definition of the image beside, fixed
    /// for this image's life, counts as Rust's.
    fn kind(&self, slot: u32) -> u8 {
        if self.slots[slot as usize].flags & (RUST | OUTSIDE) != 0 {
            return O_RUST;
        }
        match self.best(slot) {
            Some((load, symbol))
                if !self.loads[load as usize].pulled
                    && !matches!(
                        self.object(load).symbols[symbol as usize].def,
                        SymbolDef::Common { .. }
                    ) =>
            {
                O_FIXED
            }
            _ => O_PULLED,
        }
    }

    /// The section a load's symbol is defined in: its own for a local, else its name's.
    fn target(&self, load: u32, symbol: u32) -> Option<(u32, u32)> {
        let it = &self.object(load).symbols[symbol as usize];
        if is_local(it) {
            return match it.def {
                SymbolDef::Section { section, .. } => Some((load, section)),
                _ => None,
            };
        }
        self.home(self.loads[load as usize].slots[symbol as usize])
    }

    /// The section a name is defined in, through the fallbacks it is an alias of.
    fn home(&self, mut slot: u32) -> Option<(u32, u32)> {
        let at = |load: u32, symbol: u32| match self.object(load).symbols[symbol as usize].def {
            SymbolDef::Section { section, .. } => Some((load, section)),
            _ => None,
        };
        for _ in 0..64 {
            let it = &self.slots[slot as usize];
            if it.flags & RUST != 0 {
                return None;
            }
            if let Some((load, symbol)) = self.best(slot) {
                return at(load, symbol);
            }
            if it.flags & ALIAS == 0 {
                return None;
            }
            let (load, symbol) = unpack(it.fallback);
            if is_local(&self.object(load).symbols[symbol as usize]) {
                return at(load, symbol);
            }
            slot = self.loads[load as usize].slots[symbol as usize];
        }
        None
    }

    /// The indexes collection walks: describers, live support, named sections, roots.
    /// Once per graph built: each list is sized from the loads, but the live support's (as
    /// many as its sections' relocations).
    fn index(&mut self) {
        let sum =
            |of: fn(&Parsed) -> usize| self.loads.iter().map(|it| of(&it.parsed)).sum::<usize>();
        let mut unwinds = Vec::with_capacity(sum(|it| it.unwind.len()));
        let mut fdes = Vec::with_capacity(sum(|it| it.frames.len()));
        let mut named = Vec::with_capacity(sum(|it| it.named.len()));
        let mut roots = Vec::with_capacity(sum(|it| it.roots.len()));
        // alloc: none without live support; it grows once per graph built
        let mut supports = Vec::new();
        for (load, it) in self.loads.iter().enumerate() {
            let load = load as u32;
            for section in &it.parsed.object().live_support {
                for symbol in it.parsed.refs.row(*section as usize) {
                    match self.target(load, *symbol) {
                        Some((l, s)) if (l, s) != (load, *section) => {
                            supports.push((self.flat(l, s) as u32, it.sections + section));
                        }
                        _ => {}
                    }
                }
            }
            for (section, symbol) in &it.parsed.unwind {
                if *symbol == NONE {
                    continue;
                }
                if let Some((l, s)) = self.target(load, *symbol) {
                    unwinds.push((self.flat(l, s) as u32, it.sections + section));
                }
            }
            for (at, frame) in it.parsed.frames.iter().enumerate() {
                if !frame.fde || frame.function == NONE {
                    continue;
                }
                if let Some((l, s)) = self.target(load, frame.function) {
                    fdes.push((self.flat(l, s) as u32, it.frames + at as u32));
                }
            }
            named.extend(
                it.parsed.named.iter().map(|(name, section)| (*name, it.sections + section)),
            );
            roots.extend(it.parsed.roots.iter().map(|section| it.sections + section));
        }
        radix::sort_copy_by_key(&mut named, |&(name, flat)| radix::pack_32(name.0, flat));
        self.unwinds = Csr::build(self.sections as usize, unwinds.iter().copied());
        self.fdes = Csr::build(self.sections as usize, fdes.iter().copied());
        self.supports = if supports.is_empty() {
            Csr::default()
        } else {
            Csr::build(self.sections as usize, supports.iter().copied())
        };
        self.named = named;
        self.roots = roots;
    }
}

// What a resolved name was ([`Resolved::outcomes`]): a kind in the low 3 bits, then flags.
/// Rust defines it.
const O_RUST: u8 = 1;
/// An object loaded whatever is undefined defines it.
const O_FIXED: u8 = 2;
/// A pulled member defines it, or it is an alias or a common.
const O_PULLED: u8 = 3;
/// An import.
const O_IMPORT: u8 = 4;
/// An import from an `--as-needed` library.
const O_NEEDED: u8 = 5;
/// A special, 0 for a weak name, or left undefined.
const O_OTHER: u8 = 6;
/// A special that keeps sections: `__start_X`, `__imp_X`.
const O_MARKS: u8 = 7;
const O_KIND: u8 = 7;
/// Every reference to it was weak.
const O_WEAK: u8 = 8;
/// Required, or strongly referred to other than by Rust.
const O_STRONG: u8 = 16;
/// Referred to or rooted other than by Rust.
const O_OTHERS: u8 = 32;

/// What a change of how Rust refers to one name asks of a stage.
#[derive(PartialEq, Eq, Debug)]
enum Next {
    Nothing,
    /// A name the kept resolution never resolved.
    Gained,
    Rebuild,
}

/// What a name's Rust references going from `before` to `after` (0 none, 1 weak only, 2
/// strong) asks, given what it resolved to.
fn settled(outcome: u8, before: u8, after: u8, full: bool) -> Next {
    let kind = outcome & O_KIND;
    if kind == 0 {
        return if after > 0 { Next::Gained } else { Next::Nothing };
    }
    let still = after > 0 || outcome & O_OTHERS != 0;
    let flips = (outcome & O_STRONG == 0 && after != 2) != (outcome & O_WEAK != 0);
    let rebuild = match kind {
        O_RUST => false,
        // A root went: what it kept may go.
        O_FIXED | O_MARKS => after == 0,
        // The member may no longer be pulled.
        O_PULLED => after < before,
        // A full link drops an import nothing refers to.
        O_IMPORT => {
            if still {
                flips
            } else {
                full
            }
        }
        O_NEEDED => !still || flips,
        _ => still && flips,
    };
    if rebuild { Next::Rebuild } else { Next::Nothing }
}

/// How many of Rust's pieces refer to each name, strongly and weakly, and what each counted
/// piece referred to, so a removed one is taken off whatever became of its lists.
#[derive(Clone, Default)]
struct Refs {
    /// By [`Symbols::dense`]: strong and weak references.
    counts: CowVec<[u32; 2]>,
    /// By piece: its names, each `sym | weak << 32`.
    of: CowVec<ListRef>,
    lists: CowList<u64>,
    /// Every name ever referred to, once.
    names: CowVec<Sym>,
    listed: CowFlags,
}

impl Refs {
    /// 0 when Rust does not refer to the name, 1 when only weakly, 2 when strongly.
    fn class(&self, sym: Sym) -> u8 {
        match self.counts.get(Symbols::dense(sym)) {
            Some([strong, _]) if *strong > 0 => 2,
            Some([_, weak]) if *weak > 0 => 1,
            _ => 0,
        }
    }

    fn count(&mut self, entry: u64, by: i32) {
        let sym = Sym(entry as u32);
        let at = Symbols::dense(sym);
        let mut it = self.counts.get(at).copied().unwrap_or([0; 2]);
        let weak = (entry >> 32) as usize;
        it[weak] = it[weak].wrapping_add_signed(by);
        self.counts.set(at, it, || [0; 2]);
        if self.listed.set(at) {
            self.names.push(sym);
        }
    }

    /// Copies the live lists into a fresh arena.
    fn compact(&mut self) {
        let mut lists = CowList::default();
        for id in 0..self.of.len() {
            let list = *self.of.get(id).expect("in range");
            if !list.is_empty() {
                let fresh = lists.push(self.lists.get(list));
                self.of.set(id, fresh, || ListRef::EMPTY);
            }
        }
        self.lists = lists;
    }
}

/// Whether a piece's references are Rust's, not the natives' or the linker's
/// ([`What::is_linkers_cell`]).
fn counted(what: What) -> bool {
    !what.is_linkers_cell()
        && !matches!(what, What::Native { .. } | What::NativeFrame { .. } | What::Common)
}

/// What collection kept, over a [`Graph`]'s flat numbering.
#[derive(Clone, Default)]
pub(crate) struct Marks {
    sections: CowFlags,
    frames: CowFlags,
    /// Runs of `Graph::named` kept whole, by their first entry.
    named: CowFlags,
    /// Of each slot, what its name keeps as a writer's special, once asked: UNKNOWN,
    /// NOTHING, `NAMED | run`, or the slot `__imp_X`'s X is.
    special: CowVec<u32>,
    /// Of each slot: 1 when kept native code refers to it only weakly, 2 strongly.
    native: CowVec<u8>,
}

const UNKNOWN: u32 = u32::MAX;
const NOTHING: u32 = u32::MAX - 1;
const NAMED: u32 = 1 << 31;

impl Marks {
    fn native_of(&self, slot: u32) -> u8 {
        self.native.get(slot as usize).copied().unwrap_or(0)
    }
}

/// What a name keeps as a writer's special: the `Class::Named` sections `__start_X` bounds
/// (`NAMED | their run`), the definition `__imp_X` is the cell of (X's slot), or NOTHING.
fn special_mark(graph: &Graph, symbols: &Symbols, writer: &Formats, sym: Sym) -> u32 {
    let Some(name) = symbols.name_of(sym) else { return NOTHING };
    match writer.special(name) {
        Some(Special::Section { name, .. }) => symbols
            .find_name(&name)
            .and_then(|id| {
                let at = graph.named.partition_point(|it| it.0 < id);
                graph.named.get(at).is_some_and(|it| it.0 == id).then_some(NAMED | at as u32)
            })
            .unwrap_or(NOTHING),
        Some(got @ Special::Got { .. }) => got
            .got_target(name)
            .and_then(|target| symbols.find_sym_name(target))
            .and_then(|it| graph.slot_of.get(&it).copied())
            .unwrap_or(NOTHING),
        _ => NOTHING,
    }
}

/// A walk of collection: from roots, through relocations, groups and what describes a kept
/// section, marking what it reaches once.
struct Marker<'g> {
    graph: &'g Graph,
    marks: &'g mut Marks,
    symbols: &'g Symbols,
    writer: &'g Formats,
    work: Vec<u64>,
    /// What this walk marked: `pack(load, section)`, `pack(load, frame)`.
    sections: Vec<u64>,
    frames: Vec<u64>,
}

impl<'g> Marker<'g> {
    /// A walk over `graph`: what it marks has room for every section and frame record of the
    /// graph, so those two lists are allocated once (untouched room is never paged in); the
    /// work stack, one entry per edge waiting, grows by doubling past 64Ki entries.
    fn new(
        graph: &'g Graph,
        marks: &'g mut Marks,
        symbols: &'g Symbols,
        writer: &'g Formats,
    ) -> Marker<'g> {
        let (sections, frames) = (graph.sections as usize, graph.frames as usize);
        Marker {
            graph,
            marks,
            symbols,
            writer,
            work: Vec::with_capacity(sections.min(1 << 16)),
            sections: Vec::with_capacity(sections),
            frames: Vec::with_capacity(frames),
        }
    }

    fn push(&mut self, (load, section): (u32, u32)) {
        self.work.push(pack(load, section));
    }

    fn run(&mut self) {
        let graph = self.graph;
        while let Some(item) = self.work.pop() {
            let (load, section) = unpack(item);
            let flat = graph.flat(load, section);
            if graph.discarded.get(flat) || !self.marks.sections.set(flat) {
                continue;
            }
            self.sections.push(item);
            let parsed = &graph.loads[load as usize].parsed;
            let object = parsed.object();
            let group = parsed.group_of[section as usize];
            if group != NONE {
                for it in object.group_sections(&object.groups[group as usize]) {
                    self.work.push(pack(load, *it));
                }
            }
            for it in parsed.ties.get(section as usize).unwrap_or_default() {
                self.work.push(pack(load, *it));
            }
            for symbol in parsed.refs.row(section as usize) {
                self.symbol(load, *symbol);
            }
            for it in graph.unwinds.row(flat) {
                let at = graph.unflat(*it);
                self.push(at);
            }
            for it in graph.supports.get(flat).unwrap_or_default() {
                let at = graph.unflat(*it);
                self.push(at);
            }
            for it in graph.fdes.row(flat) {
                let (l, f) = graph.unflat_frame(*it);
                self.frame(l, f);
            }
        }
    }

    /// A relocation's symbol: its section, and what its name keeps as a special.
    fn symbol(&mut self, load: u32, symbol: u32) {
        let graph = self.graph;
        if let Some(at) = graph.target(load, symbol) {
            self.push(at);
        }
        let slot = graph.loads[load as usize].slots[symbol as usize];
        if slot != NONE {
            self.special(slot);
        }
    }

    /// A name: its definition's section, and what it keeps as a special.
    fn name(&mut self, slot: u32) {
        if let Some(at) = self.graph.home(slot) {
            self.push(at);
        }
        self.special(slot);
    }

    fn special(&mut self, slot: u32) {
        let mut it = self.marks.special.get(slot as usize).copied().unwrap_or(UNKNOWN);
        if it == UNKNOWN {
            it = special_mark(
                self.graph,
                self.symbols,
                self.writer,
                self.graph.slots[slot as usize].sym,
            );
            self.marks.special.set(slot as usize, it, || UNKNOWN);
        }
        if it == NOTHING {
            return;
        }
        let graph = self.graph;
        if it & NAMED != 0 {
            let run = (it & !NAMED) as usize;
            if self.marks.named.set(run) {
                let name = graph.named[run].0;
                for (n, flat) in &graph.named[run..] {
                    if *n != name {
                        break;
                    }
                    let at = graph.unflat(*flat);
                    self.push(at);
                }
            }
        } else if let Some(at) = graph.home(it) {
            self.push(at);
        }
    }

    /// An FDE kept for what it describes, and its CIE.
    fn frame(&mut self, load: u32, frame: u32) {
        let graph = self.graph;
        let base = graph.loads[load as usize].frames;
        if !self.marks.frames.set((base + frame) as usize) {
            return;
        }
        self.frames.push(pack(load, frame));
        let cie = graph.loads[load as usize].parsed.frames[frame as usize].cie;
        if cie != NONE && self.marks.frames.set((base + cie) as usize) {
            self.frames.push(pack(load, cie));
            self.frame_fixups(load, cie);
        }
        self.frame_fixups(load, frame);
    }

    fn frame_fixups(&mut self, load: u32, frame: u32) {
        let parsed = &self.graph.loads[load as usize].parsed;
        let object = parsed.object();
        for fixup in parsed.frame_fixups.row(frame as usize) {
            self.symbol(load, object.fixups[*fixup as usize].symbol);
        }
    }
}

/// Notes the names what a walk marked refers to; into `rose`, when given, the slots whose note
/// rose, from and to.
fn note_native(
    graph: &Graph,
    marks: &mut Marks,
    sections: &[u64],
    frames: &[u64],
    mut rose: Option<&mut Vec<(u32, u8, u8)>>,
) {
    let mut note = |marks: &mut Marks, load: u32, symbol: u32| {
        let it = &graph.loads[load as usize];
        let slot = it.slots[symbol as usize];
        if slot == NONE {
            return;
        }
        let level = if it.parsed.object().symbols[symbol as usize].binding == Binding::Weak {
            1
        } else {
            2
        };
        let was = marks.native_of(slot);
        if level > was {
            marks.native.set(slot as usize, level, || 0);
            if let Some(rose) = rose.as_deref_mut() {
                rose.push((slot, was, level));
            }
        }
    };
    for (load, section) in sections.iter().map(|it| unpack(*it)) {
        let parsed = &graph.loads[load as usize].parsed;
        for symbol in parsed.refs.row(section as usize) {
            note(marks, load, *symbol);
        }
    }
    for (load, frame) in frames.iter().map(|it| unpack(*it)) {
        let parsed = &graph.loads[load as usize].parsed;
        let object = parsed.object();
        for fixup in parsed.frame_fixups.row(frame as usize) {
            note(marks, load, object.fixups[*fixup as usize].symbol);
        }
    }
}

/// The symbols other than its own a kept section defines: its locals, and the globals it is
/// the winning definition of.
fn section_defines(graph: &Graph, load: u32, section: u32, out: &mut Vec<Sym>) {
    out.clear();
    let parsed = &graph.loads[load as usize].parsed;
    let object = parsed.object();
    let own = parsed.section_sym(section);
    for symbol in parsed.defs.row(section as usize) {
        let sym = parsed.sym(*symbol);
        if sym != own && (is_local(&object.symbols[*symbol as usize]) || graph.wins(load, *symbol))
        {
            out.push(sym);
        }
    }
    out.sort_unstable();
    out.dedup();
}

/// What is left of an undefined name.
enum Decision {
    Special(Special),
    /// An import, and its library's canonical index (NONE for none).
    Import(Import, u32),
    /// 0: every reference is weak.
    Zero,
    /// Left undefined: an optional name.
    Skip,
}

/// What resolving the native inputs and the names gave, and what the next stage reuses.
#[derive(Clone, Default)] // clone: every field `Copy`, an `Arc` or copy-on-write: reference counts only
pub(crate) struct Resolved {
    pub imports: Arc<Vec<Import>>,
    /// Each import's symbol.
    pub import_syms: Arc<Vec<Sym>>,
    pub libraries: Arc<Vec<Library>>,
    /// Of each of `libraries`, its place on the link line ([`crate::Native::order`]).
    pub library_orders: Arc<Vec<u32>>,
    /// What the graph has the image export ([`Graph::asked`]).
    pub exports: Arc<Vec<Exported>>,
    /// Whether an import is kept in place for an incremental link that no longer needs it.
    stale: bool,
    refs: Refs,
    /// The piece count and removals Rust's references were counted to.
    pieces: u32,
    removals: u64,
    /// What it was resolved from.
    inputs: Arc<Vec<Input>>,
    names: Arc<BTreeMap<NameId, crate::state::RustName>>,
    wasm: Arc<HashMap<NameId, blaze_codegen::wasm::Import, std::alloc::Global>>,
    graph: Option<Arc<Graph>>,
    marks: Marks,
    /// Of each name resolved, by [`Symbols::dense`]: `O_*` bits.
    outcomes: CowVec<u8>,
    /// The names `outcomes` holds.
    named: CowVec<Sym>,
    /// The names resolution defined.
    defined: CowVec<Sym>,
    /// Of each library of the graph's exports, its index in `libraries`; NONE when not needed.
    library_index: Arc<Vec<u32>>,
    /// The commons made: the name, size, alignment, sorted by name.
    commons: Arc<Vec<(Sym, u64, u64)>>,
}

impl Resolved {
    fn outcome(&self, sym: Sym) -> u8 {
        self.outcomes.get(Symbols::dense(sym)).copied().unwrap_or(0)
    }

    /// The global names the natives define, each once, as resolution chose them: neither
    /// Rust's nor the image beside's. One defined as an alias of its fallback is weak, at
    /// the fallback's place on the line. `inputs` are those it was resolved from.
    pub(crate) fn native_defs<'r>(
        &'r self,
        inputs: &'r [Input],
    ) -> impl Iterator<Item = NativeDef> + 'r {
        self.graph.as_deref().into_iter().flat_map(move |graph| {
            let order = move |load: u32| inputs[graph.loads[load as usize].input as usize].order;
            (0..graph.slots.len() as u32).filter_map(move |slot| {
                let it = &graph.slots[slot as usize];
                if it.flags & (RUST | OUTSIDE) != 0 {
                    return None;
                }
                if let Some((load, symbol)) = graph.best(slot) {
                    let size = match graph.object(load).symbols[symbol as usize].def {
                        SymbolDef::Section { size, .. } | SymbolDef::Common { size, .. } => size,
                        _ => 0,
                    };
                    return Some(NativeDef {
                        sym: it.sym,
                        weak: it.strength < 3,
                        order: order(load),
                        size,
                    });
                }
                (it.flags & ALIAS != 0).then(|| NativeDef {
                    sym: it.sym,
                    weak: true,
                    order: order(unpack(it.fallback).0),
                    size: 0,
                })
            })
        })
    }
}

/// A name the natives define ([`Resolved::native_defs`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeDef {
    pub sym: Sym,
    /// Weak, or a common.
    pub weak: bool,
    /// Its object's place on the link line.
    pub order: u32,
    pub size: u64,
}

/// A number that changes exactly when a stage built its resolution's graph again.
#[cfg(test)]
pub(crate) fn resolution_id(resolved: &Resolved) -> u64 {
    resolved.graph.as_ref().map_or(0, |it| it.id)
}

/// A resolution being built.
struct Build {
    graph: Graph,
    /// Slots to look for in the archives after the round.
    queue: Vec<u32>,
    pulled: HashSet<(u32, u32), std::alloc::Global>,
    /// COMDAT groups by name: the winner's `(load, group)` in `group_list`.
    groups: Table,
    group_list: Vec<(u32, u32)>,
    mismatches: HashMap<String, (String, u32), std::alloc::Global>,
    asked: HashSet<String, std::alloc::Global>,
    /// The line's shared libraries: file, `as_needed`, place on the line.
    shared: Vec<(Arc<NativeFile>, bool, u32)>,
    /// Import libraries' members, in load order, and their archive's place on the line.
    members: Vec<(Arc<NativeFile>, u32)>,
    commons: HashMap<u32, (u64, u64), std::alloc::Global>,
}

impl Build {
    fn new() -> Build {
        Build {
            graph: Graph::new(),
            // alloc: none (an empty `Vec`); it grows once per graph built
            queue: Vec::new(),
            pulled: HashSet::new_in(std::alloc::Global),
            groups: Table::default(),
            // alloc: none (an empty `Vec`); it grows once per graph built
            group_list: Vec::new(),
            mismatches: HashMap::new_in(std::alloc::Global),
            asked: HashSet::new_in(std::alloc::Global),
            // alloc: none (an empty `Vec`); it grows once per graph built
            shared: Vec::new(),
            // alloc: none (an empty `Vec`); it grows once per graph built
            members: Vec::new(),
            commons: HashMap::new_in(std::alloc::Global),
        }
    }

    fn slot(&mut self, sym: Sym) -> u32 {
        self.graph.slot(sym)
    }

    /// Room for `slots` more slots: the slot map is sized once a pull round, never doubled
    /// (and rehashed) while a round registers its objects.
    fn reserve(&mut self, slots: usize) {
        self.graph.slot_of.reserve(slots);
        self.graph.slots.reserve(slots);
    }

    fn flag(&mut self, slot: u32, flags: u16) {
        let it = &mut self.graph.slots[slot as usize];
        it.flags |= flags;
        if flags & PULL != 0 && it.flags & QUEUED == 0 {
            it.flags |= QUEUED;
            self.queue.push(slot);
        }
    }

    /// Drops a group's sections, and the sections tied to one dropped (lld drops a
    /// `SHF_LINK_ORDER` section whose `sh_link` is dropped).
    fn discard(&mut self, load: u32, group: u32) {
        let Graph { loads, discarded, .. } = &mut self.graph;
        let it = &loads[load as usize];
        let object = it.parsed.object();
        let members = object.group_sections(&object.groups[group as usize]);
        for section in members {
            discarded.set((it.sections + section) as usize);
        }
        if object.ties.is_empty() {
            return;
        }
        let mut work: SmallVec<[u32; 8]> = SmallVec::from_slice(members);
        while let Some(section) = work.pop() {
            for tied in it.parsed.ties.get(section as usize).unwrap_or_default() {
                let dependent = tie_of(object, *tied).is_some_and(|tie| tie.to == section);
                if dependent && discarded.set((it.sections + tied) as usize) {
                    work.push(*tied);
                }
            }
        }
    }

    /// Weak externals left undefined become aliases of what they fall back to, which the
    /// link must then define; a chain back to itself is an error.
    fn aliases(&mut self, symbols: &Symbols) -> Result<(), LinkError> {
        let graph = &mut self.graph;
        for slot in 0..graph.slots.len() as u32 {
            let it = graph.slots[slot as usize];
            if it.fallback != NONE64
                && it.flags & (RUST | OUTSIDE) == 0
                && graph.best(slot).is_none()
            {
                graph.slots[slot as usize].flags |= ALIAS;
            }
        }
        for slot in 0..graph.slots.len() as u32 {
            if graph.slots[slot as usize].flags & ALIAS == 0 {
                continue;
            }
            let mut seen: SmallVec<[u32; 8]> = SmallVec::new();
            let mut at = slot;
            loop {
                let (load, symbol) = unpack(graph.slots[at as usize].fallback);
                if is_local(&graph.object(load).symbols[symbol as usize]) {
                    break;
                }
                let next = graph.loads[load as usize].slots[symbol as usize];
                if next == slot {
                    let name = symbols.name_of(graph.slots[slot as usize].sym).unwrap_or("?");
                    // alloc: error path
                    return Err(error(
                        None,
                        format!("the weak external `{name}` falls back to itself"),
                    ));
                }
                if seen.contains(&next) || graph.slots[next as usize].flags & ALIAS == 0 {
                    break;
                }
                seen.push(next);
                at = next;
            }
            let (load, symbol) = unpack(graph.slots[slot as usize].fallback);
            let target = graph.loads[load as usize].slots[symbol as usize];
            if target != NONE {
                graph.slots[target as usize].flags |= REQUIRED;
            }
        }
        Ok(())
    }
}

/// A stage's scratch lists for making pieces.
#[derive(Default)]
struct Scratch {
    fixups: Vec<Fixup>,
    weak: Vec<Sym>,
    defines: Vec<Sym>,
}

impl<'a> Stage<'a> {
    /// The native inputs the change adds, removes and moves: each new one read and checked
    /// once, in parallel.
    pub fn inputs(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        if change.natives.is_empty() && change.removed_natives.is_empty() {
            return Ok(());
        }
        // An archive or library twice on the line is searched once (lld searches every
        // archive whatever its place); an object twice is an error.
        let mut given: HashMap<blaze_ids::ContentId, usize, std::alloc::Global> =
            HashMap::with_capacity_in(change.natives.len(), std::alloc::Global);
        let mut natives = Vec::with_capacity(change.natives.len());
        for (at, native) in change.natives.iter().enumerate() {
            match given.entry(native.id) {
                Entry::Vacant(it) => {
                    it.insert(at);
                    natives.push(at);
                }
                Entry::Occupied(_) if matches!(native.kind, NativeKind::Object) => {
                    // alloc: error path
                    return Err(error(
                        None,
                        format!("{}: the object is on the link line twice", native.name),
                    ));
                }
                Entry::Occupied(_) => {}
            }
        }
        let mut removed: HashSet<blaze_ids::ContentId, std::alloc::Global> =
            HashSet::with_capacity_in(change.removed_natives.len(), std::alloc::Global);
        removed.extend(change.removed_natives.iter().copied());
        let inputs = Arc::make_mut(&mut self.new.inputs);
        inputs.retain(|it| !removed.contains(&it.id));
        let mut have: HashMap<blaze_ids::ContentId, usize, std::alloc::Global> =
            HashMap::with_capacity_in(inputs.len(), std::alloc::Global);
        have.extend(inputs.iter().enumerate().map(|(at, it)| (it.id, at)));
        let mut fresh = Vec::with_capacity(natives.len());
        for at in natives {
            let native = &change.natives[at];
            match have.get(&native.id) {
                Some(it) => inputs[*it].order = native.order,
                None => fresh.push(at),
            }
        }
        let writer = self.writer;
        let listed = &change.natives;
        let mut files: Vec<Result<NativeFile, LinkError>> = Vec::with_capacity(fresh.len());
        files.clear();
        par::extend(&mut files, &fresh, |at| {
            let native = &listed[*at];
            // alloc: error path (the message is made only on `Err`)
            let file = writer
                .read(native.name, native.bytes, &native.kind)
                .map_err(|it| error(None, format!("{}: {it}", native.name)))?;
            // alloc: error path (the message is made only on `Err`)
            check_file(&file, native.bytes.len())
                .map_err(|it| error(None, format!("{}: {it}", native.name)))?;
            Ok(file)
        });
        let mut bytes: HashMap<blaze_ids::ContentId, &'a [u8], std::alloc::Global> =
            HashMap::with_capacity_in(inputs.len() + fresh.len(), std::alloc::Global);
        inputs.reserve(fresh.len());
        for (at, file) in fresh.iter().zip(files) {
            let native = &change.natives[*at];
            inputs.push(Input {
                id: native.id,
                name: native.name.into(),
                kind: native.kind,
                order: native.order,
                // clone: an `Arc`: the caller's bytes, shared
                bytes: native.bytes.clone(),
                // alloc: once per native file read, shared by every state after
                file: Arc::new(file?),
            });
            bytes.insert(native.id, &native.bytes[..]);
        }
        inputs.sort_by_key(|it| (it.order, it.id));
        let mut names: HashSet<&str, std::alloc::Global> =
            HashSet::with_capacity_in(inputs.len(), std::alloc::Global);
        names.extend(inputs.iter().map(|it| &*it.name));
        // A Mach-O library is on the line by its install name too, which is where an SDK's
        // `.tbd` stands for the `/usr/lib` dylib another library re-exports.
        names.extend(inputs.iter().filter_map(|it| match &*it.file {
            NativeFile::Shared(shared) if shared.name.starts_with('/') => Some(&*shared.name),
            _ => None,
        }));
        for input in inputs.iter() {
            let NativeFile::Shared(shared) = &*input.file else { continue };
            for wanted in &shared.inputs {
                if line_has(&names, wanted) {
                    continue;
                }
                let what = match wanted.strip_prefix("-l") {
                    Some(file) if file.starts_with(':') => {
                        // alloc: error path
                        format!(
                            "{} names {wanted}, and the link line has no {}",
                            input.name,
                            &file[1..]
                        )
                    }
                    // alloc: error path
                    Some(lib) => format!(
                        "{} names {wanted}, and the link line has no lib{lib}.so or lib{lib}.a",
                        input.name
                    ),
                    // alloc: error path
                    None => {
                        format!("{} names {wanted}, which the link line does not have", input.name)
                    }
                };
                return Err(error(None, what));
            }
        }
        let old: &'a State = self.old;
        for it in old.inputs.iter() {
            bytes.entry(it.id).or_insert(&it.bytes[..]);
        }
        self.native.bytes.clear();
        self.native.bytes.reserve(inputs.len());
        self.native.bytes.extend(inputs.iter().map(|it| bytes[&it.id]));
        self.native.filled = true;
        if !removed.is_empty() {
            let mut gone: HashSet<u32, std::alloc::Global> =
                HashSet::with_capacity_in(removed.len(), std::alloc::Global);
            gone.extend(
                removed
                    .iter()
                    .filter(|it| !given.contains_key(*it))
                    .filter_map(|it| self.symbols.find_id(it))
                    .map(|it| it.0),
            );
            if !gone.is_empty() {
                self.objects.retain(|obj, _| !gone.contains(&obj.native.0));
            }
        }
        Ok(())
    }

    /// A section's bytes, or `within` them: borrowed from the input, or copied to the arena
    /// when the reader made them; none for zeros.
    fn content(
        &self,
        object: &Object,
        data: &Data,
        input: u32,
        base: usize,
        within: Range<usize>,
    ) -> Content<'a> {
        match data {
            Data::Made(range) => {
                let arena: &'a blaze_mem::Frame = self.arena;
                let start = range.start as usize;
                Content::Bytes(
                    arena.alloc_slice_copy(&object.made[start + within.start..start + within.end]),
                )
            }
            Data::Input(range) => {
                debug_assert!(
                    self.native.filled || Arc::ptr_eq(&self.old.inputs, &self.new.inputs)
                );
                let bytes: &'a [u8] = self.native.input(self.old, input);
                let start = base + range.start as usize;
                Content::Bytes(&bytes[start + within.start..start + within.end])
            }
            Data::Bytes(bytes) => {
                let arena: &'a blaze_mem::Frame = self.arena;
                Content::Bytes(arena.alloc_slice_copy(&bytes[within]))
            }
            Data::Zero(_) => Content::Zero,
        }
    }

    /// The name of input `input` (an index into [`State::inputs`]), for an error.
    fn input_name(&self, input: u32) -> &str {
        &self.new.inputs[input as usize].name
    }

    fn piece_of(&self, sym: Sym) -> Option<PieceId> {
        match self.new.def(sym) {
            Def::Piece { piece, .. } if self.new.piece(piece).is_some() => Some(piece),
            _ => None,
        }
    }

    /// The objects and import libraries at `wanted` (input, and 0 for the input itself or
    /// member + 1): kept ones as they are, the rest read, checked, interned and indexed, the
    /// reading and indexing in parallel.
    fn read(&mut self, wanted: &[(u32, u32)]) -> Result<Vec<Read>, LinkError> {
        let mut objs = Vec::with_capacity(wanted.len());
        for (input, member) in wanted {
            let id = self.new.inputs[*input as usize].id;
            objs.push(self.symbols.obj(&id, *member));
        }
        let mut misses = Vec::with_capacity(wanted.len());
        misses.extend((0..wanted.len()).filter(|it| !self.objects.contains_key(&objs[*it])));
        if !misses.is_empty() {
            let fresh = self.read_misses(wanted, &objs, &misses)?;
            for (at, read) in misses.iter().zip(fresh) {
                self.objects.insert(objs[*at], read);
            }
        }
        let mut out = Vec::with_capacity(wanted.len());
        // clone: a `Read` is one `Arc`: the image keeps each object, and the graph shares it
        out.extend(objs.iter().map(|it| self.objects[it].clone()));
        Ok(out)
    }

    fn read_misses(
        &mut self,
        wanted: &[(u32, u32)],
        objs: &[Obj],
        misses: &[usize],
    ) -> Result<Vec<Read>, LinkError> {
        struct Got {
            file: Arc<NativeFile>,
            base: usize,
            records: Vec<(u32, Vec<unwind::Native>)>,
        }
        let writer = self.writer;
        let (inputs, native, old): (&[Input], &Pass<'a>, &'a State) =
            (&self.new.inputs[..], &self.native, self.old);
        let mut got: Vec<Result<Got, LinkError>> = Vec::with_capacity(misses.len());
        got.clear();
        par::extend(&mut got, misses, |at| {
            let (input, member) = wanted[*at];
            let it = &inputs[input as usize];
            let all = native.input(old, input);
            let (file, base, member_name) = if member == 0 {
                // clone: an `Arc`: the object is the input's own file
                (it.file.clone(), 0, None)
            } else {
                let NativeFile::Archive(archive) = &*it.file else {
                    unreachable!("a member of an archive")
                };
                let entry = &archive.members[member as usize - 1];
                let member_name = archive.name(entry.name);
                // alloc: error path
                let fail = |what: &str| error(None, format!("{}({member_name}): {what}", it.name));
                let slice = &all[entry.bytes.start..entry.bytes.end];
                // A reader names the object only in its errors: the archive's name; the member's is added here.
                let file = writer
                    .read(&it.name, slice, &NativeKind::Object)
                    .map_err(|what| fail(&what))?;
                check_file(&file, slice.len()).map_err(|what| fail(&what))?;
                if matches!(file, NativeFile::Archive(_)) {
                    return Err(fail("an archive in an archive"));
                }
                // alloc: once per member read, shared by every stage after
                (Arc::new(file), entry.bytes.start, Some(member_name))
            };
            // alloc: none for an object without `.eh_frame`; once per object read otherwise
            let mut records = Vec::new();
            if let NativeFile::Object(object) = &*file {
                for (at, section) in object.sections.iter().enumerate() {
                    if section.what != SectionKind::EhFrame {
                        continue;
                    }
                    let data = match &section.data {
                        Data::Input(range) => {
                            &all[base + range.start as usize..base + range.end as usize]
                        }
                        Data::Made(range) => &object.made[range.start as usize..range.end as usize],
                        Data::Bytes(bytes) => &bytes[..],
                        Data::Zero(_) => continue,
                    };
                    let split = unwind::split(data).map_err(|what| {
                        let section = object.name(section.name);
                        // alloc: error path
                        error(
                            None,
                            match member_name {
                                // alloc: error path
                                Some(member) => format!("{}({member}): {section}: {what}", it.name),
                                // alloc: error path
                                None => format!("{}: {section}: {what}", it.name),
                            },
                        )
                    })?;
                    records.push((at as u32, split));
                }
            }
            Ok(Got { file, base, records })
        });
        // alloc: one buffer per batch read, the objects of the batch
        let got: Vec<Got> = got.into_iter().collect::<Result<_, _>>()?;
        // One batch for every object's symbols, sections and frame records, and one for the
        // names of its named sections, each sized once.
        let (mut key_count, mut name_count) = (0, 0);
        for it in &got {
            let NativeFile::Object(object) = &*it.file else { continue };
            key_count += object.symbols.len() + object.sections.len();
            key_count += it.records.iter().map(|(_, records)| records.len()).sum::<usize>();
            name_count += object
                .sections
                .iter()
                .filter(|it| matches!(it.what, SectionKind::Alloc(Class::Named { .. }, _)))
                .count();
        }
        let mut starts = Vec::with_capacity(got.len() + 1);
        let syms;
        let name_ids;
        {
            let mut keys: Vec<Wide<'_>> = Vec::with_capacity(key_count);
            let mut names: Vec<&str> = Vec::with_capacity(name_count);
            for (it, at) in got.iter().zip(misses) {
                starts.push((keys.len(), names.len()));
                let NativeFile::Object(object) = &*it.file else { continue };
                object_keys(objs[*at], object, &it.records, &mut keys);
                for section in &object.sections {
                    if matches!(section.what, SectionKind::Alloc(Class::Named { .. }, _)) {
                        names.push(object.name(section.name));
                    }
                }
            }
            starts.push((keys.len(), names.len()));
            syms = self.symbols.intern_wide(&keys);
            name_ids = self.symbols.names_all(&names);
        }
        let mut reads: Vec<Read> = Vec::with_capacity(got.len());
        reads.clear();
        par::extend(&mut reads, par::enumerate(par::zip(got, misses)), |(n, (it, at))| {
            if matches!(*it.file, NativeFile::Shared(_)) {
                return Read::Shared(it.file);
            }
            let ((keys, names), (keys_end, names_end)) = (starts[n], starts[n + 1]);
            let parsed = Parsed::new(
                objs[*at],
                it.file,
                it.base,
                // alloc: the object's own symbols, once per object read, kept with it
                syms[keys..keys_end].to_vec(),
                &name_ids[names..names_end],
                &it.records,
            );
            // alloc: once per object read, shared by every stage after
            Read::Object(Arc::new(parsed))
        });
        Ok(reads)
    }

    /// The indexes of the searched archives, kept ones as they are, the rest built in
    /// parallel.
    fn indexes(&mut self, inputs: &[Input], lazy: &[u32]) -> Vec<(u32, Arc<Table>)> {
        let mut objs: Vec<Obj> = Vec::with_capacity(lazy.len());
        objs.extend(lazy.iter().map(|it| self.symbols.obj(&inputs[*it as usize].id, 0)));
        let objects = &*self.objects;
        let mut out: Vec<(u32, Arc<Table>)> = Vec::with_capacity(lazy.len());
        out.clear();
        par::extend(&mut out, par::zip(lazy, &objs), |(input, obj)| {
            let table = match objects.get(obj) {
                // clone: an `Arc`: the archive's index as the image keeps it
                Some(Read::Index(it)) => it.clone(),
                _ => {
                    let NativeFile::Archive(archive) = &*inputs[*input as usize].file else {
                        unreachable!("searched inputs are archives")
                    };
                    // alloc: once per archive searched, kept for the image's life
                    Arc::new(archive_index(archive))
                }
            };
            (*input, table)
        });
        for (obj, (_, table)) in objs.iter().zip(&out) {
            // clone: an `Arc`: the image's handle on the index
            self.objects.entry(*obj).or_insert_with(|| Read::Index(table.clone()));
        }
        out
    }

    /// Adds an object to the graph: its groups, directives, imports and globals, in lld's
    /// order of precedence (the graph's loads are in link-line order, then pull rounds).
    fn register(
        &mut self,
        b: &mut Build,
        parsed: &Arc<Parsed>,
        input: u32,
        pulled: bool,
    ) -> Result<(), LinkError> {
        let object = parsed.object();
        let load = b.graph.loads.len() as u32;
        // alloc: the load's slot of each symbol, once per object per graph built
        let mut slots = vec![NONE; object.symbols.len()];
        for (at, symbol) in object.symbols.iter().enumerate() {
            if !is_local(symbol) {
                slots[at] = b.slot(parsed.sym(at as u32));
            }
        }
        let (sections, frames) = (b.graph.sections, b.graph.frames);
        b.graph.sections += object.sections.len() as u32;
        b.graph.frames += parsed.frames.len() as u32;
        b.graph.by_obj.insert(parsed.obj, load);
        // clone: an `Arc`: the load keeps the object while this reads it and the graph grows
        b.graph.loads.push(Load { parsed: parsed.clone(), input, pulled, sections, frames, slots });

        let size = |graph: &Graph, load: u32, group: u32| -> u64 {
            let object = graph.object(load);
            object
                .group_sections(&object.groups[group as usize])
                .first()
                .map_or(0, |it| object.sections[*it as usize].data.len())
        };
        for (index, group) in object.groups.iter().enumerate() {
            let index = index as u32;
            let name = object.name(group.name);
            let entry = b.group_list.len() as u32;
            let (graph, list) = (&b.graph, &b.group_list);
            let found = b.groups.insert(hash(name), entry, |it| {
                let (l, g) = list[it as usize];
                let o = graph.object(l);
                o.name(o.groups[g as usize].name) == name
            });
            match found {
                Ok(()) => b.group_list.push((load, index)),
                Err(winner) => {
                    let (wl, wg) = b.group_list[winner as usize];
                    if wl == load {
                        continue;
                    }
                    if group.largest && size(&b.graph, load, index) > size(&b.graph, wl, wg) {
                        b.discard(wl, wg);
                        b.group_list[winner as usize] = (load, index);
                    } else {
                        b.discard(load, index);
                    }
                }
            }
        }

        for directive in &object.directives {
            match directive {
                Directive::Include(name) => {
                    let slot = b.slot(self.symbols.sym_name(name));
                    b.flag(slot, PULL | REQUIRED | ROOT);
                }
                Directive::Export { name, symbol, data } => {
                    let slot = b.slot(self.symbols.sym_name(symbol));
                    b.flag(slot, PULL | REQUIRED | ROOT);
                    // alloc: the set's own name, once per `/EXPORT` directive per graph built
                    if b.asked.insert(name.clone()) {
                        // alloc: the writers' own names, once per name exported per graph built
                        Arc::make_mut(&mut b.graph.asked).push(Exported {
                            name: name.clone(),
                            symbol: symbol.clone(),
                            data: *data,
                            weak: false,
                            own: None,
                        });
                    }
                }
                Directive::Mismatch { key, value } => match b.mismatches.get(key) {
                    Some((first, by)) if first != value => {
                        return Err(error(
                            None,
                            // alloc: error path
                            format!(
                                "`{key}` is `{first}` in {} and `{value}` in {} (/FAILIFMISMATCH)",
                                self.input_name(*by),
                                self.input_name(input)
                            ),
                        ));
                    }
                    Some(_) => {}
                    None => {
                        // alloc: once per `/FAILIFMISMATCH` key per graph built
                        b.mismatches.insert(key.clone(), (value.clone(), input));
                    }
                },
                Directive::NoDefaultLib(_) => {}
            }
        }

        let all = &self.native.input(self.old, input)[parsed.base..];
        for (at, it) in &object.imports {
            let sym = parsed.sym(*at);
            let bad = |what: String| {
                let name = object.name(object.symbols[*at as usize].name);
                // alloc: error path
                error(
                    None,
                    format!("{}: the wasm import of `{name}`: {what}", self.input_name(input)),
                )
            };
            let ty = it.type_bytes(all).map_err(bad)?;
            // alloc: the type's params and results, once per import per graph built; error path
            let ty = blaze_codegen::wasm::Signature::decode(ty, &mut 0)
                .map_err(|what| bad(what.to_string()))?;
            // alloc: the graph's own wasm import (module and field names, its type), once per import per graph built
            let import = blaze_codegen::wasm::Import {
                module: object.name(it.module).to_owned(),
                field: object.name(it.field).to_owned(),
                ty,
            };
            match b.graph.wasm.get(&sym) {
                Some((by, known)) if *known != import => {
                    let name = object.name(object.symbols[*at as usize].name);
                    return Err(error(
                        None,
                        // alloc: error path
                        format!(
                            "`{name}` is imported as {known:?} by {} and as {import:?} by {}",
                            self.input_name(*by),
                            self.input_name(input)
                        ),
                    ));
                }
                Some(_) => {}
                None => {
                    b.graph.wasm.insert(sym, (input, import));
                }
            }
        }

        for (at, symbol) in object.symbols.iter().enumerate() {
            if is_local(symbol) {
                continue;
            }
            let slot = b.graph.loads[load as usize].slots[at];
            let weak = symbol.binding == Binding::Weak;
            let strength = match symbol.def {
                SymbolDef::Undefined => {
                    if !weak {
                        b.flag(slot, PULL);
                    }
                    continue;
                }
                SymbolDef::Fallback { search, .. } => {
                    if search {
                        b.flag(slot, PULL);
                    }
                    let it = &mut b.graph.slots[slot as usize];
                    if it.fallback == NONE64 {
                        it.fallback = pack(load, at as u32);
                    }
                    continue;
                }
                SymbolDef::Section { section, .. } => {
                    if b.graph.discarded.get(b.graph.flat(load, section)) {
                        continue;
                    }
                    if weak { 2 } else { 3 }
                }
                SymbolDef::Absolute(_) => {
                    if weak {
                        2
                    } else {
                        3
                    }
                }
                SymbolDef::Common { size, align } => {
                    let it = b.commons.entry(slot).or_insert((0, 1));
                    it.0 = it.0.max(size);
                    it.1 = it.1.max(align);
                    1
                }
            };
            let it = b.graph.slots[slot as usize];
            if it.flags & RUST != 0 {
                // Rust's definition is first on the line: it beats a weak one and a common.
                if strength < 3 {
                    continue;
                }
                if it.flags & RUST_WEAK == 0 {
                    return Err(error(
                        None,
                        // alloc: error path
                        format!(
                            "duplicate symbol `{}`: Rust code defines it, and so does {}",
                            object.name(symbol.name),
                            self.input_name(input)
                        ),
                    ));
                }
                // A strong definition beats Rust's weak one, as ld64 and lld take it: the name is
                // this one's, and a later strong one is the duplicate symbol error with it.
                let it = &mut b.graph.slots[slot as usize];
                it.flags &= !RUST;
                it.best = pack(load, at as u32);
                it.strength = 3;
                continue;
            }
            let take = match b.graph.best(slot) {
                None => true,
                Some(_) if strength > it.strength => true,
                Some((l, i)) if strength == 3 && it.strength == 3 => {
                    let first = match b.graph.object(l).symbols[i as usize].def {
                        SymbolDef::Section { .. } => {
                            self.input_name(b.graph.loads[l as usize].input)
                        }
                        _ => "an absolute definition",
                    };
                    return Err(error(
                        None,
                        // alloc: error path
                        format!(
                            "duplicate symbol `{}`: in {first} and in {}",
                            object.name(symbol.name),
                            self.input_name(input)
                        ),
                    ));
                }
                Some(_) => false,
            };
            if take {
                let it = &mut b.graph.slots[slot as usize];
                it.best = pack(load, at as u32);
                it.strength = strength;
            }
        }
        Ok(())
    }

    /// What is left of a name nothing defines: the linker's, a shared library's, 0, or an
    /// error naming it and who refers to it.
    fn decide(&self, graph: &Graph, sym: Sym, weak: bool) -> Result<Decision, LinkError> {
        let Some(name) = self.symbols.name_of(sym) else { return Ok(Decision::Skip) };
        if self.output.dev_run.is_some() && name == crate::stage::START_HOOK {
            return Err(error(
                None,
                // alloc: error path
                format!(
                    "undefined symbol `{name}`, which a dev run's first initializer calls: blaze-live's dev runtime \
                     (`blaze_inproc::runtime`) must be linked into the executable itself"
                ),
            ));
        }
        if self.lazy.is_some() && name == blaze_lazy::hook::SYMBOL {
            return Err(crate::lazy::no_hook(self.output.format));
        }
        if let Some(special) = self.writer.special(name) {
            return Ok(Decision::Special(special));
        }
        if let Some(export) = graph.exports.find(name) {
            let import = Import {
                // alloc: the writers' `Import` owns its name: once per import a resolution makes
                name: name.to_owned(),
                library: None,
                version: graph.exports.version(export),
                kind: export.kind,
                weak: weak || export.weak,
                wasm: None,
            };
            return Ok(Decision::Import(import, export.library));
        }
        let executable = matches!(self.output.kind, OutputKind::Executable { .. });
        if self.output.format == crate::Format::Wasm {
            // rustc links wasm with `--allow-undefined`: a function something says the
            // import of is imported; a weak one is null.
            let native = graph.wasm.get(&sym);
            let rust = match self.symbols.key(sym) {
                SymKey::Name(id) => self.new.wasm_imports.get(&id),
                _ => None,
            };
            if let (Some((by, a)), Some(b)) = (native, rust) {
                if a != b {
                    return Err(error(
                        None,
                        // alloc: error path
                        format!(
                            "`{name}` is imported as {a:?} by {} and as {b:?} by Rust code",
                            self.new.inputs[*by as usize].name
                        ),
                    ));
                }
            }
            return match native.map(|it| &it.1).or(rust) {
                Some(it) if !weak => Ok(Decision::Import(
                    Import {
                        // alloc: the writers' `Import` owns its name: once per import a resolution makes
                        name: name.to_owned(),
                        library: None,
                        version: None,
                        kind: SymbolKind::Function,
                        weak: false,
                        // clone: the import's own module and field names, once per import made
                        wasm: Some(it.clone()),
                    },
                    NONE,
                )),
                _ if weak => Ok(Decision::Zero),
                _ => Err(error(
                    None,
                    // alloc: error path
                    format!(
                        "undefined symbol `{name}`{}, which no input says the wasm import of",
                        self.referrer(sym)
                    ),
                )),
            };
        }
        // Left for the loader to find in whatever is loaded (Mach-O's flat lookup, an ELF
        // undefined dynamic symbol): a shared library's by default, and an executable's under
        // `-undefined dynamic_lookup`, but for a core root it need not define.
        let lookup = match self.output.link.undefined {
            crate::Undefined::Default => !executable,
            crate::Undefined::DynamicLookup => !(executable && optional(name)),
            crate::Undefined::Error => false,
        };
        if weak && executable && !lookup {
            return Ok(Decision::Zero);
        }
        if lookup || weak {
            // alloc: the writers' `Import` owns its name: once per import a resolution makes
            let import = Import {
                name: name.to_owned(),
                library: None,
                version: None,
                kind: SymbolKind::Other,
                weak,
                wasm: None,
            };
            return Ok(Decision::Import(import, NONE));
        }
        if optional(name) {
            return Ok(Decision::Skip);
        }
        // alloc: error path
        Err(error(None, format!("undefined symbol `{name}`{}", self.referrer(sym))))
    }

    /// Who refers to an undefined symbol: only for an undefined symbol's error.
    fn referrer(&self, sym: Sym) -> String {
        let Some(piece) = self.new.refs(sym).find_map(|it| self.new.piece(it)) else {
            // alloc: none (an empty `String`), error path
            return String::new();
        };
        match piece.what() {
            What::Native { obj, .. } | What::NativeFrame { obj, .. } => {
                let id = self.symbols.content(obj.native);
                let name =
                    self.new.inputs.iter().find(|it| it.id == *id).map_or("?", |it| &*it.name);
                // alloc: error path
                format!(", referred to by {name}")
            }
            // alloc: error path
            _ => format!(", referred to by {}", self.symbols.describe(piece.sym)),
        }
    }

    /// Makes a kept section's piece, its bytes borrowed from its input.
    fn make_section(
        &mut self,
        graph: &Graph,
        load: u32,
        section: u32,
        scratch: &mut Scratch,
    ) -> Result<(), LinkError> {
        let it = &graph.loads[load as usize];
        let parsed = &it.parsed;
        let object = parsed.object();
        let s = &object.sections[section as usize];
        let (class, rank, unwind) = match &s.what {
            SectionKind::Alloc(class, rank) => (class, *rank, false),
            SectionKind::Unwind(class) => (class, 0, true),
            SectionKind::Dropped | SectionKind::EhFrame => return Ok(()),
        };
        let zeroed = crate::model::zeroed(class);
        if zeroed && !matches!(s.data, Data::Zero(_)) {
            let name = &self.new.inputs[it.input as usize].name;
            // alloc: error path
            return Err(error(
                None,
                format!("{name}: {} has bytes, in a zeroed section", object.name(s.name)),
            ));
        }
        scratch.fixups.clear();
        scratch.weak.clear();
        for f in object.section_fixups(s) {
            let target = parsed.sym(f.symbol);
            let symbol = &object.symbols[f.symbol as usize];
            if symbol.binding == Binding::Weak && symbol.def == SymbolDef::Undefined {
                scratch.weak.push(target);
            }
            scratch.fixups.push(Fixup {
                at: f.at,
                apply: f.apply,
                target,
                addend: f.addend,
                minus: f.minus.map(|m| parsed.sym(m)),
            });
        }
        scratch.weak.sort_unstable();
        scratch.weak.dedup();
        section_defines(graph, load, section, &mut scratch.defines);
        let content = if zeroed {
            Content::Zero
        } else {
            self.content(object, &s.data, it.input, parsed.base, 0..s.data.len() as usize)
        };
        let order = self.new.inputs[it.input as usize].order;
        let what = What::Native { obj: parsed.obj, section, unwind };
        let mut piece = self.piece(
            parsed.section_sym(section),
            class,
            s.align,
            s.data.len(),
            &scratch.fixups,
            what,
        );
        piece.rank = rank;
        // lld's `SHF_LINK_ORDER`: in the order of the sections tied to, so that parallel
        // tables (SanitizerCoverage's counters and PCs) stay one entry each per kept function.
        let placed =
            tie_of(object, section).filter(|tie| tie.ordered).map_or(section, |tie| tie.to);
        piece.order = [order, parsed.obj.member, placed];
        piece.zero = zeroed;
        piece.weak = scratch.weak.as_slice();
        piece.defines = scratch.defines.as_slice();
        self.make(&piece, content);
        Ok(())
    }

    /// Makes a kept frame record's piece: an FDE's CIE pointer is a fixup to its CIE's piece.
    fn make_frame(
        &mut self,
        graph: &Graph,
        load: u32,
        frame: u32,
        fixups: &mut Vec<Fixup>,
    ) -> Result<(), LinkError> {
        let it = &graph.loads[load as usize];
        let parsed = &it.parsed;
        let object = parsed.object();
        let record = parsed.frames[frame as usize];
        let s = &object.sections[record.section as usize];
        let (start, end) = (u64::from(record.start), u64::from(record.end));
        if (end - start) % 4 != 0 {
            let name = self.input_name(it.input);
            // alloc: error path
            return Err(error(
                None,
                format!("{name}: {}: a frame record not a multiple of 4 long", object.name(s.name)),
            ));
        }
        if record.fde && record.cie == NONE {
            let name = self.input_name(it.input);
            // alloc: error path
            return Err(error(
                None,
                format!(
                    "{name}: {}: an FDE at {start:#x} whose CIE is no record",
                    object.name(s.name)
                ),
            ));
        }
        let sym = parsed.frame_sym(frame);
        fixups.clear();
        for at in parsed.frame_fixups.row(frame as usize) {
            let f = &object.fixups[*at as usize];
            fixups.push(Fixup {
                at: f.at - start,
                apply: f.apply,
                target: parsed.sym(f.symbol),
                addend: f.addend,
                minus: f.minus.map(|m| parsed.sym(m)),
            });
        }
        if record.fde {
            let cie = parsed.frame_sym(record.cie);
            fixups.retain(|f| f.at != 4);
            fixups.push(Fixup {
                at: 4,
                apply: Apply::Delta32,
                target: sym,
                addend: 4,
                minus: Some(cie),
            });
        }
        fixups.sort_by_key(|f| f.at);
        let content =
            self.content(object, &s.data, it.input, parsed.base, start as usize..end as usize);
        let order = self.new.inputs[it.input as usize].order;
        let what = What::NativeFrame { obj: parsed.obj, section: record.section, fde: record.fde };
        let mut piece = self.piece(sym, &Class::EhFrame, 4, end - start, fixups.as_slice(), what);
        piece.rank = u32::from(record.fde);
        piece.order = [order, parsed.obj.member, record.section];
        self.make(&piece, content);
        Ok(())
    }

    fn make_common(&mut self, name: Sym, size: u64, align: u64) {
        let SymKey::Name(id) = self.symbols.key(name) else { return };
        let common = self.sym(SymKey::Common(id));
        let mut piece = self.piece(common, &Class::Bss, align, size, &[], What::Common);
        piece.zero = true;
        self.make(&piece, Content::Zero);
    }

    fn unmake_common(&mut self, name: Sym) {
        let SymKey::Name(id) = self.symbols.key(name) else { return };
        if let Some(piece) = self.symbols.find(SymKey::Common(id)).and_then(|it| self.piece_of(it))
        {
            self.unmake(piece);
        }
    }

    /// Defines the symbols a newly kept section holds: its locals, the globals it wins.
    fn define_in(&mut self, graph: &Graph, load: u32, section: u32) {
        let parsed = &graph.loads[load as usize].parsed;
        let Some(piece) = self.piece_of(parsed.section_sym(section)) else { return };
        let object = parsed.object();
        for symbol in parsed.defs.row(section as usize) {
            let s = &object.symbols[*symbol as usize];
            let sym = parsed.sym(*symbol);
            if sym == parsed.section_sym(section) || !(is_local(s) || graph.wins(load, *symbol)) {
                continue;
            }
            if let SymbolDef::Section { value, .. } = s.def {
                self.define(sym, Def::Piece { piece, offset: value });
            }
        }
    }

    /// Counts Rust's references to names from the pieces made and removed since `old`: the
    /// counts, and the names whose class (none, weak, strong) changed. Nothing is allocated
    /// when no piece came or went; otherwise one list, of the names touched.
    fn references(&self, old: &Resolved) -> (Refs, Vec<Sym>) {
        // clone: copy-on-write: a reference count per column, copied only where counted
        let mut refs = old.refs.clone();
        // alloc: none until a piece came or went; then the names touched, once per stage
        let mut touched: Vec<Sym> = Vec::new();
        for id in self.new.removed_since(old.removals) {
            let Some(list) = refs.of.get(id as usize).copied().filter(|it| !it.is_empty()) else {
                continue;
            };
            for at in 0..list.len() {
                let entry = refs.lists.get(list)[at];
                refs.count(entry, -1);
                touched.push(Sym(entry as u32));
            }
            refs.lists.free(list);
            refs.of.set(id as usize, ListRef::EMPTY, || ListRef::EMPTY);
        }
        let mut entries: SmallVec<[u64; 16]> = SmallVec::new();
        for id in old.pieces..self.new.piece_count() {
            let Some(piece) = self.new.piece(id) else { continue };
            if !counted(piece.what()) {
                continue;
            }
            entries.clear();
            for fixup in piece.fixups() {
                if matches!(self.symbols.key(fixup.target), SymKey::Name(_)) {
                    let weak = piece.weak().contains(&fixup.target);
                    entries.push(u64::from(fixup.target.0) | u64::from(weak) << 32);
                }
            }
            if entries.is_empty() {
                continue;
            }
            entries.sort_unstable();
            entries.dedup();
            for entry in &entries {
                refs.count(*entry, 1);
                touched.push(Sym(*entry as u32));
            }
            let list = refs.lists.push(&entries);
            refs.of.set(id as usize, list, || ListRef::EMPTY);
        }
        if refs.lists.garbage() > refs.lists.live() + (1 << 16) {
            refs.compact();
        }
        radix::sort_copy_by_key(&mut touched, |it| it.0);
        touched.dedup();
        touched.retain(|sym| old.refs.class(*sym) != refs.class(*sym));
        (refs, touched)
    }

    /// Resolves the natives and the names, reusing what the image's resolution can. A stage
    /// that made and removed no piece, on the image's inputs and names, keeps the image's
    /// resolution as it is and allocates nothing.
    pub fn resolve<O: Outside>(&mut self, outside: &O) -> Result<(), LinkError> {
        // clone: an `Arc`: the image's resolution, read while this stage's is made
        let old = self.new.resolved.clone();
        let (refs, changes) = self.references(&old);
        let same = old.graph.is_some()
            && Arc::ptr_eq(&old.inputs, &self.new.inputs)
            && (Arc::ptr_eq(&old.names, &self.new.names) || *old.names == *self.new.names)
            && (Arc::ptr_eq(&old.wasm, &self.new.wasm_imports)
                || *old.wasm == *self.new.wasm_imports)
            && !(self.full && old.stale);
        if same
            && changes.is_empty()
            && old.pieces == self.new.piece_count()
            && old.removals == self.new.removals()
        {
            return Ok(());
        }
        // alloc: none unless Rust newly refers to names, once per stage
        let mut gained = Vec::new();
        let mut rebuild = !same;
        if same {
            for sym in &changes {
                let (before, after) = (old.refs.class(*sym), refs.class(*sym));
                match settled(old.outcome(*sym), before, after, self.full) {
                    Next::Nothing => {}
                    Next::Gained => gained.push(*sym),
                    Next::Rebuild => {
                        rebuild = true;
                        break;
                    }
                }
            }
        }
        let grown = match (rebuild, gained.is_empty()) {
            (true, _) => None,
            // clone: every field an `Arc` or copy-on-write: reference counts, no allocation
            (false, true) => Some((*old).clone()),
            (false, false) => self.grow(&old, &refs, &gained, outside)?,
        };
        let mut next = match grown {
            Some(it) => it,
            None => self.rebuild(&old, &refs, outside)?,
        };
        next.refs = refs;
        next.pieces = self.new.piece_count();
        next.removals = self.new.removals();
        // alloc: one per stage that made or removed pieces: the resolution its state shares
        self.new.resolved = Arc::new(next);
        Ok(())
    }

    /// Resolves names Rust newly refers to against the kept graph, and makes what they newly
    /// reach; `None` when that needs what only building the graph again gives.
    fn grow<O: Outside>(
        &mut self,
        old: &Resolved,
        refs: &Refs,
        gained: &[Sym],
        outside: &O,
    ) -> Result<Option<Resolved>, LinkError> {
        // clone: an `Arc`: the kept graph, read while the stage makes what it reaches
        let Some(graph) = old.graph.clone() else { return Ok(None) };
        // clone: copy-on-write: reference counts, copied only where marked
        let mut marks = old.marks.clone();
        let mut outcomes: Vec<(Sym, u8)> = Vec::with_capacity(gained.len());
        let mut undefined: Vec<Sym> = Vec::with_capacity(gained.len());
        // alloc: none unless the image beside defines a gained name, once per grown resolution
        let mut beside: Vec<(Sym, Def)> = Vec::new();
        let (sections, frames) = {
            let symbols: &Symbols = &*self.symbols;
            let mut marker = Marker::new(&graph, &mut marks, symbols, self.writer);
            for &sym in gained {
                // A gained name is new to the names: nothing but Rust refers to it.
                let weak = if refs.class(sym) == 2 { 0 } else { O_WEAK };
                let slot = graph.slot_of.get(&sym).copied();
                // A weak Rust name a strong definition took is resolved as that one's.
                let taken = slot.is_some_and(|it| {
                    graph.slots[it as usize].flags & (RUST | RUST_WEAK) == RUST_WEAK
                });
                let rust = match symbols.key(sym) {
                    SymKey::Name(id) => self.new.names.contains_key(&id) && !taken,
                    _ => false,
                };
                if rust {
                    outcomes.push((sym, O_RUST | weak));
                    continue;
                }
                if O::ANY {
                    // No loaded object mentions a name new to the graph, so only archives
                    // could define it, and the image beside comes before them.
                    let def = match slot {
                        Some(slot) if graph.slots[slot as usize].flags & OUTSIDE != 0 => {
                            graph.outside.get(&slot).copied()
                        }
                        Some(_) => None,
                        None => symbols
                            .name_of(sym)
                            .and_then(|name| outside.name(name))
                            .map(outside_def),
                    };
                    if let Some(def) = def {
                        beside.push((sym, def));
                        outcomes.push((sym, O_RUST | weak));
                        continue;
                    }
                }
                if slot.is_some_and(|it| graph.slots[it as usize].flags & ALIAS != 0) {
                    return Ok(None);
                }
                if let Some(slot) = slot.filter(|it| graph.best(*it).is_some()) {
                    marker.name(slot);
                    outcomes.push((sym, graph.kind(slot) | weak));
                    continue;
                }
                if special_mark(&graph, symbols, self.writer, sym) != NOTHING {
                    return Ok(None);
                }
                if refs.class(sym) == 2 {
                    let name = symbols.name_of(sym).unwrap_or("");
                    if search(&graph.archives, &self.new.inputs, name).is_some() {
                        return Ok(None);
                    }
                }
                undefined.push(sym);
            }
            marker.run();
            (marker.sections, marker.frames)
        };
        // alloc: none unless a name's note rose, once per grown resolution
        let mut rose = Vec::new();
        note_native(&graph, &mut marks, &sections, &frames, Some(&mut rose));
        for (slot, _, level) in rose {
            let it = &graph.slots[slot as usize];
            let outcome = old.outcome(it.sym);
            let strong = if level == 2 { O_STRONG } else { 0 };
            if outcome & O_KIND == 0 {
                if graph.defined(slot) {
                    let weak = if level == 2 || refs.class(it.sym) == 2 { 0 } else { O_WEAK };
                    outcomes.push((it.sym, graph.kind(slot) | weak | strong | O_OTHERS));
                } else {
                    undefined.push(it.sym);
                }
                continue;
            }
            if !graph.defined(slot) && level == 2 && outcome & O_WEAK != 0 {
                return Ok(None);
            }
            outcomes.push((it.sym, outcome | strong | O_OTHERS));
        }

        radix::sort_copy_by_key(&mut undefined, |it| it.0);
        undefined.dedup();
        let mut decisions = Vec::with_capacity(undefined.len());
        for sym in undefined {
            let slot = graph.slot_of.get(&sym).copied();
            let flags = slot.map_or(0, |it| graph.slots[it as usize].flags);
            let native = slot.map_or(0, |it| marks.native_of(it));
            let strong = flags & (REQUIRED | STRONG) != 0 || native == 2;
            let others = strong || flags & ROOT != 0 || native > 0;
            let weak = !strong && refs.class(sym) != 2;
            let bits = if weak { O_WEAK } else { 0 }
                | if strong { O_STRONG } else { 0 }
                | if others { O_OTHERS } else { 0 };
            let decision = self.decide(&graph, sym, weak)?;
            let kind = match &decision {
                Decision::Special(Special::Section { .. } | Special::Got { .. }) => {
                    return Ok(None);
                }
                Decision::Special(_) | Decision::Zero | Decision::Skip => O_OTHER,
                Decision::Import(_, library) if *library == NONE => O_IMPORT,
                Decision::Import(_, library) if old.library_index[*library as usize] == NONE => {
                    return Ok(None);
                }
                Decision::Import(_, library)
                    if graph.exports.libraries[*library as usize].as_needed =>
                {
                    O_NEEDED
                }
                Decision::Import(..) => O_IMPORT,
            };
            decisions.push((sym, decision, kind | bits));
        }

        // Nothing below gives up: make and define what was reached and decided.
        let mut scratch = Scratch::default();
        let mut sections = sections;
        radix::sort(&mut sections);
        for (load, section) in sections.iter().map(|it| unpack(*it)) {
            let parsed = &graph.loads[load as usize].parsed;
            if !makes_piece(&parsed.object().sections[section as usize].what)
                || self.piece_of(parsed.section_sym(section)).is_some()
            {
                continue;
            }
            self.make_section(&graph, load, section, &mut scratch)?;
            self.define_in(&graph, load, section);
        }
        let mut frames = frames;
        frames.sort_unstable();
        for (load, frame) in frames.iter().map(|it| unpack(*it)) {
            if self.piece_of(graph.loads[load as usize].parsed.frame_sym(frame)).is_none() {
                self.make_frame(&graph, load, frame, &mut scratch.fixups)?;
            }
        }
        // clone: every field an `Arc` or copy-on-write: reference counts, no allocation
        let mut next = old.clone();
        for (sym, def) in beside {
            self.define(sym, def);
            next.defined.push(sym);
        }
        let mut imports: Vec<(Sym, Import)> = Vec::with_capacity(decisions.len());
        for (sym, decision, outcome) in decisions {
            match decision {
                Decision::Special(special) => {
                    let at = self.new.specials.len() as u32;
                    Arc::make_mut(&mut self.new.specials).push(special);
                    self.define(sym, Def::Special(at));
                    next.defined.push(sym);
                }
                Decision::Import(mut import, library) => {
                    import.library = (library != NONE).then(|| old.library_index[library as usize]);
                    imports.push((sym, import));
                }
                Decision::Zero => {
                    self.define(sym, Def::Absolute(0));
                    next.defined.push(sym);
                }
                Decision::Skip => {}
            }
            outcomes.push((sym, outcome));
        }
        if !imports.is_empty() {
            imports.sort_by(|a, b| a.1.name.cmp(&b.1.name));
            let list = Arc::make_mut(&mut next.imports);
            let syms = Arc::make_mut(&mut next.import_syms);
            for (sym, import) in imports {
                // An import kept in place for an earlier link is taken up again.
                let at = match syms.iter().position(|it| *it == sym) {
                    Some(at) => {
                        list[at] = import;
                        at
                    }
                    None => {
                        syms.push(sym);
                        list.push(import);
                        syms.len() - 1
                    }
                };
                self.define(sym, Def::Import(at as u32));
                next.defined.push(sym);
            }
        }
        for (sym, outcome) in outcomes {
            if next.outcome(sym) & O_KIND == 0 {
                next.named.push(sym);
            }
            next.outcomes.set(Symbols::dense(sym), outcome, || 0);
        }
        next.marks = marks;
        Ok(Some(next))
    }

    /// Where the image beside defines a name the loads mention, a whole link's rule picks
    /// between its definition and theirs: strong beats weak (a common is weaker than
    /// either), of two weak ones the first on the line wins, two strong ones are the
    /// duplicate symbol error. Rust's names are first on the line: a strong one loses to
    /// none, a weak one only to a strong definition.
    fn beside_names<O: Outside>(
        &self,
        b: &mut Build,
        inputs: &[Input],
        outside: &O,
    ) -> Result<(), LinkError> {
        for slot in 0..b.graph.slots.len() as u32 {
            let it = b.graph.slots[slot as usize];
            let Some(name) = self.symbols.name_of(it.sym) else { continue };
            let Some(theirs) = outside.name(name) else { continue };
            let strength = if theirs.strength == Strength::Strong { 3 } else { 2 };
            let duplicate = |ours: &str| {
                // alloc: error path
                error(
                    None,
                    format!(
                        "duplicate symbol `{name}`: {ours}, and so does the image this one is linked beside"
                    ),
                )
            };
            let wins = if it.flags & RUST != 0 {
                match (strength, it.flags & RUST_WEAK != 0) {
                    (3, false) => return Err(duplicate("Rust code defines it")),
                    (3, true) => true,
                    _ => false,
                }
            } else {
                match b.graph.best(slot) {
                    None => true,
                    Some((load, _)) if strength == 3 && it.strength == 3 => {
                        // alloc: error path
                        return Err(duplicate(&format!(
                            "{} defines it",
                            self.input_name(b.graph.loads[load as usize].input)
                        )));
                    }
                    Some(_) if strength != it.strength => strength > it.strength,
                    Some((load, _)) => {
                        theirs.order < inputs[b.graph.loads[load as usize].input as usize].order
                    }
                }
            };
            if !wins && theirs.strength == Strength::Weak && outside.reached_directly(name) {
                let ours = match b.graph.best(slot) {
                    Some((load, _)) if it.flags & RUST == 0 => {
                        self.input_name(b.graph.loads[load as usize].input)
                    }
                    _ => "Rust code",
                };
                return Err(error(
                    None,
                    // alloc: error path
                    format!(
                        "`{name}` is defined by {ours} in {}, and weakly by the dependency image it is linked beside, \
                         whose own code reaches its definition directly: a whole link would reach this one from there \
                         too",
                        self.name
                    ),
                ));
            }
            if wins {
                let it = &mut b.graph.slots[slot as usize];
                it.best = NONE64;
                it.strength = 0;
                it.flags = (it.flags & !RUST) | OUTSIDE;
                b.graph.outside.insert(slot, outside_def(theirs));
            }
        }
        Ok(())
    }

    /// An ELF executable exports each name it defines, by Rust or by a loaded object, that a
    /// shared library on its link line also defines or leaves undefined, as GNU ld and lld
    /// do: the dynamic linker then binds the library's own uses of the name to the
    /// executable's definition, which is how a program's `malloc` and `free` replace libc's.
    /// A name hidden where it is defined, or by any loaded object that mentions it (ELF
    /// merges visibility to the most constraining), is not exported. Each exported name is
    /// a root of collection, since the loader may call what nothing in the image does, and
    /// keeps its binding and, where its object states them, its type and size.
    ///
    /// The exports are in name order, after the directives', and are part of the graph: it
    /// is built again whenever the natives (the libraries among them) or Rust's names
    /// change, so no link reuses a list made from other inputs.
    fn export_for_libraries(&self, b: &mut Build) -> Result<(), LinkError> {
        let symbols: &Symbols = &*self.symbols;
        let graph = &b.graph;
        let exports = &graph.exports;
        if exports.symbols.is_empty() && exports.undefined.is_empty() {
            return Ok(());
        }
        // alloc: none unless a library names what the executable defines, once per graph built
        let mut found: Vec<(&str, u32)> = Vec::new();
        for (slot, it) in graph.slots.iter().enumerate() {
            let slot = slot as u32;
            if it.flags & (OUTSIDE | ALIAS) != 0 {
                continue;
            }
            if it.flags & RUST == 0 {
                let Some((load, symbol)) = graph.best(slot) else { continue };
                let symbol = &graph.object(load).symbols[symbol as usize];
                if symbol.hidden
                    || matches!(symbol.def, SymbolDef::Undefined | SymbolDef::Fallback { .. })
                {
                    continue;
                }
            }
            let Some(name) = symbols.name_of(it.sym) else { continue };
            if exports.find(name).is_some() || exports.refers(name) {
                found.push((name, slot));
            }
        }
        if found.is_empty() {
            return Ok(());
        }
        // alloc: once per graph built whose executable defines a name its libraries name
        let mut hidden = vec![false; graph.slots.len()];
        for load in &graph.loads {
            for (symbol, slot) in load.parsed.object().symbols.iter().zip(&load.slots) {
                if symbol.hidden && *slot != NONE {
                    hidden[*slot as usize] = true;
                }
            }
        }
        found.retain(|(_, slot)| !hidden[*slot as usize]);
        // Slots are numbered in the order names are met; the exports are in name order.
        found.sort_unstable();
        let mut made: Vec<Exported> = Vec::with_capacity(found.len());
        for &(name, slot) in &found {
            let it = &graph.slots[slot as usize];
            let (weak, own) = if it.flags & RUST != 0 {
                (it.flags & RUST_WEAK != 0, None)
            } else {
                let (load, symbol) = unpack(it.best);
                let symbol = &graph.object(load).symbols[symbol as usize];
                let own = match symbol.def {
                    SymbolDef::Section { size, .. } => Some((symbol.kind, size)),
                    SymbolDef::Common { .. } => None,
                    _ => {
                        return Err(error(
                            None,
                            // alloc: error path
                            format!(
                                "`{name}` is defined outside every section (an absolute symbol) by {}, and a shared                                  library {} links defines or refers to it, so the executable exports it: blaze's ELF                                  writer exports only what is in the image",
                                self.input_name(graph.loads[load as usize].input),
                                self.name
                            ),
                        ));
                    }
                };
                (symbol.binding == Binding::Weak, own)
            };
            // alloc: the writers' own names, once per name exported per graph built
            let (name, symbol) = (name.to_owned(), name.to_owned());
            made.push(Exported { name, symbol, data: false, weak, own });
        }
        for (export, &(_, slot)) in made.into_iter().zip(&found) {
            // alloc: the set's own name, once per name exported per graph built
            if b.asked.insert(export.name.clone()) {
                Arc::make_mut(&mut b.graph.asked).push(export);
            }
            b.graph.slots[slot as usize].flags |= ROOT;
        }
        Ok(())
    }

    /// Builds the graph again from the kept objects and makes the image's natives and names
    /// what it gives, touching only what differs.
    fn rebuild<O: Outside>(
        &mut self,
        old: &Resolved,
        refs: &Refs,
        outside: &O,
    ) -> Result<Resolved, LinkError> {
        let output = self.output;
        // clone: an `Arc`: the inputs, read while the stage defines and makes pieces
        let inputs = self.new.inputs.clone();
        // clone: an `Arc`: Rust's names, read while the stage defines them
        let names = self.new.names.clone();
        let mut b = Build::new();
        // Rust's names, the names it refers to, the entry, `_init` and `_fini`; the exports are
        // few and grow it at most once.
        b.reserve(names.len() + refs.names.len() + 3);
        let mut rust: Vec<(Sym, Sym)> = Vec::with_capacity(names.len());
        for (name, it) in names.iter() {
            let sym = self.symbols.intern(SymKey::Name(*name));
            rust.push((sym, it.target));
            let slot = b.slot(sym);
            b.flag(slot, if it.weak { RUST | RUST_WEAK } else { RUST });
        }
        for sym in refs.names.iter() {
            match refs.class(*sym) {
                0 => {}
                1 => {
                    let slot = b.slot(*sym);
                    b.flag(slot, WANTED);
                }
                _ => {
                    let slot = b.slot(*sym);
                    b.flag(slot, WANTED | PULL);
                }
            }
        }
        let entry = match &output.kind {
            OutputKind::Executable { entry } => Some(self.symbols.sym_name(entry)),
            OutputKind::Shared { .. } => {
                self.writer.shared_entry().map(|it| self.symbols.sym_name(it))
            }
        };
        if let Some(sym) = entry {
            let slot = b.slot(sym);
            b.flag(slot, ROOT | STRONG | PULL);
        }
        b.graph.entry = entry;
        if let OutputKind::Shared { exports, .. } = &output.kind {
            for name in exports {
                let slot = b.slot(self.symbols.sym_name(name));
                b.flag(slot, ROOT | STRONG | PULL);
            }
        }
        for name in ["_init", "_fini"] {
            let slot = b.slot(self.symbols.sym_name(name));
            b.flag(slot, LOOSE);
        }

        // Load: the line's objects and whole archives, then members for what is strongly
        // undefined, round by round.
        let whole = |input: &Input| match &*input.file {
            NativeFile::Archive(archive)
                if matches!(input.kind, NativeKind::Archive { whole: true }) =>
            {
                archive.members.len()
            }
            _ => 1,
        };
        let mut round: Vec<(u32, u32)> = Vec::with_capacity(inputs.iter().map(whole).sum());
        let mut lazy: Vec<u32> = Vec::with_capacity(inputs.len());
        for (at, input) in inputs.iter().enumerate() {
            let at = at as u32;
            match &*input.file {
                NativeFile::Object(_) => round.push((at, 0)),
                NativeFile::Archive(archive)
                    if matches!(input.kind, NativeKind::Archive { whole: true }) =>
                {
                    round.extend((1..=archive.members.len() as u32).map(|member| (at, member)));
                }
                NativeFile::Archive(_) => lazy.push(at),
                NativeFile::Shared(_) => {
                    let as_needed = matches!(input.kind, NativeKind::Shared { as_needed: true });
                    // clone: an `Arc`: the library's file, for the exports
                    b.shared.push((input.file.clone(), as_needed, input.order));
                }
            }
        }
        let archives = self.indexes(&inputs, &lazy);
        let mut pulled = false;
        loop {
            let reads = self.read(&round)?;
            b.reserve(
                reads
                    .iter()
                    .map(|it| if let Read::Object(parsed) = it { parsed.globals } else { 0 })
                    .sum(),
            );
            for (&(input, _), read) in round.iter().zip(reads) {
                match read {
                    Read::Object(parsed) => self.register(&mut b, &parsed, input, pulled)?,
                    Read::Shared(file) => b.members.push((file, inputs[input as usize].order)),
                    Read::Index(_) => {}
                }
            }
            // alloc: none when nothing is pulled; the members pulled, once per pull round
            let mut next: Vec<(u32, u32)> = Vec::new();
            for slot in std::mem::take(&mut b.queue) {
                if b.graph.slots[slot as usize].flags & RUST != 0 || b.graph.best(slot).is_some() {
                    continue;
                }
                let Some(name) = self.symbols.name_of(b.graph.slots[slot as usize].sym) else {
                    continue;
                };
                // A member is pulled only for what is still undefined after the image beside.
                if O::ANY && outside.name(name).is_some() {
                    continue;
                }
                if let Some((input, member)) = search(&archives, &inputs, name) {
                    if b.pulled.insert((input, member)) {
                        next.push((input, member + 1));
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            next.sort_unstable();
            round = next;
            pulled = true;
        }
        // A weak Rust name whose strong native definition a later COMDAT group's win dropped,
        // with no other strong one, is Rust's again: Rust's beats what weak ones are left.
        for (sym, _) in &rust {
            let slot = b.graph.slot_of[sym];
            if b.graph.slots[slot as usize].flags & (RUST | RUST_WEAK) == RUST_WEAK
                && b.graph.best(slot).is_none()
            {
                let it = &mut b.graph.slots[slot as usize];
                it.flags |= RUST;
                it.best = NONE64;
                it.strength = 0;
            }
        }
        match output.format {
            Format::Pe => default_libraries(&b.graph, &inputs)?,
            Format::MachO => linker_options(&b.graph, &inputs)?,
            Format::Elf | Format::Wasm => {}
        }
        if O::ANY {
            self.beside_names(&mut b, &inputs, outside)?;
        }
        b.aliases(&*self.symbols)?;
        let mut commons: Vec<(Sym, u64, u64)> = Vec::with_capacity(b.commons.len());
        for (slot, (size, align)) in &b.commons {
            let Some((load, symbol)) = b.graph.best(*slot) else { continue };
            if matches!(b.graph.object(load).symbols[symbol as usize].def, SymbolDef::Common { .. })
            {
                commons.push((b.graph.slots[*slot as usize].sym, *size, *align));
            }
        }
        commons.sort_unstable_by_key(|it| it.0);
        let mut shared = std::mem::take(&mut b.shared);
        shared.extend(b.members.drain(..).map(|(it, order)| (it, false, order)));
        b.graph.exports = match old.graph.as_ref().map(|it| &it.exports) {
            // clone: an `Arc`: the libraries are the image's, so are their exports
            Some(it) if it.same(&shared) => it.clone(),
            // alloc: once per change of the link's shared libraries
            _ => Arc::new(Exports::build(shared).map_err(|what| error(None, what))?),
        };
        if output.format == Format::Elf && matches!(output.kind, OutputKind::Executable { .. }) {
            self.export_for_libraries(&mut b)?;
        }
        b.graph.archives = archives;
        b.graph.index();
        // alloc: once per graph built, shared by the stages after
        let graph = Arc::new(b.graph);

        // Collect.
        let mut marks = Marks::default();
        let (sections, frames) = {
            let mut marker = Marker::new(&graph, &mut marks, &*self.symbols, self.writer);
            for slot in 0..graph.slots.len() as u32 {
                if graph.slots[slot as usize].flags & (WANTED | ROOT | LOOSE) != 0 {
                    marker.name(slot);
                }
            }
            for flat in &graph.roots {
                let at = graph.unflat(*flat);
                marker.push(at);
            }
            marker.run();
            (marker.sections, marker.frames)
        };
        note_native(&graph, &mut marks, &sections, &frames, None);

        // What went: sections, frame records and commons the image has and the graph does not
        // keep. Loads whose sections came or went, or whose globals' winners changed, have
        // their symbols defined again; the rest are as they were.
        let mut affected = CowFlags::default();
        // alloc: none when nothing went; what went, once per graph built
        let mut gone: Vec<Sym> = Vec::new();
        if let Some(was) = &old.graph {
            for (l, it) in was.loads.iter().enumerate() {
                let now = graph.by_obj.get(&it.parsed.obj).copied();
                for s in 0..it.parsed.object().sections.len() as u32 {
                    if old.marks.sections.get(was.flat(l as u32, s))
                        && !now.is_some_and(|n| marks.sections.get(graph.flat(n, s)))
                    {
                        gone.push(it.parsed.section_sym(s));
                        if let Some(n) = now {
                            affected.set(n as usize);
                        }
                    }
                }
                for f in 0..it.parsed.frames.len() as u32 {
                    let kept = now.is_some_and(|n| {
                        marks.frames.get((graph.loads[n as usize].frames + f) as usize)
                    });
                    if old.marks.frames.get((it.frames + f) as usize) && !kept {
                        gone.push(it.parsed.frame_sym(f));
                    }
                }
            }
        }
        for sym in gone {
            if let Some(piece) = self.piece_of(sym) {
                self.unmake(piece);
            }
        }
        let mut fresh_commons = Vec::with_capacity(commons.len());
        {
            let (was, now) = (&old.commons[..], &commons[..]);
            let (mut i, mut j) = (0, 0);
            loop {
                match (was.get(i), now.get(j)) {
                    (Some(x), Some(y)) if x.0 == y.0 => {
                        if x != y {
                            self.unmake_common(x.0);
                            fresh_commons.push(*y);
                        }
                        i += 1;
                        j += 1;
                    }
                    (Some(x), Some(y)) if x.0 < y.0 => {
                        self.unmake_common(x.0);
                        i += 1;
                    }
                    (Some(x), None) => {
                        self.unmake_common(x.0);
                        i += 1;
                    }
                    (_, Some(y)) => {
                        fresh_commons.push(*y);
                        j += 1;
                    }
                    (None, None) => break,
                }
            }
        }

        // What came.
        let mut scratch = Scratch::default();
        for (load, it) in graph.loads.iter().enumerate() {
            let load = load as u32;
            let object = it.parsed.object();
            for s in 0..object.sections.len() as u32 {
                if !marks.sections.get(graph.flat(load, s))
                    || !makes_piece(&object.sections[s as usize].what)
                {
                    continue;
                }
                if self.piece_of(it.parsed.section_sym(s)).is_none() {
                    self.make_section(&graph, load, s, &mut scratch)?;
                    affected.set(load as usize);
                }
            }
            for f in 0..it.parsed.frames.len() as u32 {
                if marks.frames.get((it.frames + f) as usize)
                    && self.piece_of(it.parsed.frame_sym(f)).is_none()
                {
                    self.make_frame(&graph, load, f, &mut scratch.fixups)?;
                }
            }
        }
        for (name, size, align) in fresh_commons {
            self.make_common(name, size, align);
        }

        // Every symbol's definition.
        for (load, it) in graph.loads.iter().enumerate() {
            if affected.get(load) {
                continue;
            }
            let was =
                old.graph.as_ref().and_then(|was| Some((was, *was.by_obj.get(&it.parsed.obj)?)));
            let changed = match was {
                None => true,
                Some((was, w)) => (0..it.slots.len() as u32).any(|i| {
                    it.slots[i as usize] != NONE && graph.wins(load as u32, i) != was.wins(w, i)
                }),
            };
            if changed {
                affected.set(load);
            }
        }
        for (load, it) in graph.loads.iter().enumerate() {
            if !affected.get(load) {
                continue;
            }
            let load = load as u32;
            let parsed = &it.parsed;
            let object = parsed.object();
            for (at, symbol) in object.symbols.iter().enumerate() {
                if !is_local(symbol) {
                    continue;
                }
                let sym = parsed.sym(at as u32);
                let def = match symbol.def {
                    SymbolDef::Section { section, value, .. } => {
                        if sym == parsed.section_sym(section) {
                            continue;
                        }
                        self.piece_of(parsed.section_sym(section))
                            .map_or(Def::None, |piece| Def::Piece { piece, offset: value })
                    }
                    SymbolDef::Absolute(value) if symbol.kind == SymbolKind::Tls => {
                        Def::TlsOffset(value)
                    }
                    SymbolDef::Absolute(value) => Def::Absolute(value),
                    _ => Def::None,
                };
                self.define(sym, def);
            }
            for s in 0..object.sections.len() as u32 {
                let Some(piece) = self.piece_of(parsed.section_sym(s)) else { continue };
                section_defines(&graph, load, s, &mut scratch.defines);
                if self
                    .new
                    .piece(piece)
                    .is_some_and(|it| it.defines() != scratch.defines.as_slice())
                {
                    self.new.set_defines(piece, &scratch.defines);
                }
            }
        }
        let mut defined: Vec<Sym> = Vec::with_capacity(graph.slots.len());
        for slot in 0..graph.slots.len() as u32 {
            let it = graph.slots[slot as usize];
            if it.flags & RUST != 0 {
                continue;
            }
            let def = if let Some((load, symbol)) = graph.best(slot) {
                let parsed = &graph.loads[load as usize].parsed;
                let s = &parsed.object().symbols[symbol as usize];
                match s.def {
                    SymbolDef::Section { section, value, .. } => self
                        .piece_of(parsed.section_sym(section))
                        .map_or(Def::None, |piece| Def::Piece { piece, offset: value }),
                    SymbolDef::Absolute(value) if s.kind == SymbolKind::Tls => {
                        Def::TlsOffset(value)
                    }
                    SymbolDef::Absolute(value) => Def::Absolute(value),
                    SymbolDef::Common { .. } => {
                        let SymKey::Name(name) = self.symbols.key(it.sym) else { continue };
                        Def::Alias(self.sym(SymKey::Common(name)))
                    }
                    SymbolDef::Undefined | SymbolDef::Fallback { .. } => continue,
                }
            } else if it.flags & ALIAS != 0 {
                let (load, symbol) = unpack(it.fallback);
                Def::Alias(graph.loads[load as usize].parsed.sym(symbol))
            } else if it.flags & OUTSIDE != 0 {
                graph.outside[&slot]
            } else {
                continue;
            };
            self.define(it.sym, def);
            defined.push(it.sym);
        }
        for (sym, target) in &rust {
            // A weak name a native or the image beside defines strongly is theirs, defined
            // above: Rust's own symbol stays its own, and nothing reaches it by this name.
            if graph.slots[graph.slot_of[sym] as usize].flags & RUST == 0 {
                continue;
            }
            self.define(*sym, Def::Alias(*target));
            defined.push(*sym);
        }
        let live =
            output.live.is_some().then(|| self.symbols.sym_name(blaze_live::patch::DESCRIPTOR));
        if let Some(live) = live {
            let descriptor = self.sym(SymKey::Descriptor);
            self.define(live, Def::Alias(descriptor));
            defined.push(live);
        }

        // What is left: the linker's, a shared library's, weak, or an error. `__imp_X` is
        // X's GOT cell: X is resolved as a strong reference is.
        let kind_of = |sym: Sym| -> Option<u8> {
            if Some(sym) == live {
                return Some(O_RUST);
            }
            let slot = *graph.slot_of.get(&sym)?;
            graph.defined(slot).then(|| graph.kind(slot))
        };
        // alloc: the names left to resolve, once per graph built
        let mut wanted: Vec<Sym> = Vec::new();
        for (slot, it) in graph.slots.iter().enumerate() {
            if it.flags & (ROOT | REQUIRED) != 0
                || marks.native_of(slot as u32) > 0
                || refs.class(it.sym) > 0
            {
                wanted.push(it.sym);
            }
        }
        let mut cells: HashSet<Sym, std::alloc::Global> = HashSet::new_in(std::alloc::Global);
        for sym in &wanted {
            if kind_of(*sym).is_some() {
                continue;
            }
            let Some(name) = self.symbols.name_of(*sym) else { continue };
            let Some(target) = self.writer.special(name).and_then(|it| it.got_target(name)) else {
                continue;
            };
            let target = match self.symbols.find_sym_name(target) {
                Some(it) => it,
                None => {
                    // Copied out of the table it is interned into.
                    let arena: &'a blaze_mem::Frame = self.arena;
                    let target: &str = arena.alloc_str(target);
                    self.symbols.sym_name(target)
                }
            };
            cells.insert(target);
        }
        wanted.extend(cells.iter().copied());
        wanted.sort_unstable();
        wanted.dedup();
        let mut outcomes: Vec<(Sym, u8)> = Vec::with_capacity(wanted.len());
        // alloc: none without imports; the imports decided, once per graph built
        let mut imports: Vec<(Sym, Import, u32)> = Vec::new();
        // alloc: none without the linker's names; those decided, once per graph built
        let mut specials: Vec<(Sym, Special)> = Vec::new();
        for sym in wanted {
            let slot = graph.slot_of.get(&sym).copied();
            let flags = slot.map_or(0, |it| graph.slots[it as usize].flags);
            let native = slot.map_or(0, |it| marks.native_of(it));
            let strong = flags & (REQUIRED | STRONG) != 0 || native == 2 || cells.contains(&sym);
            let others = strong || flags & ROOT != 0 || native > 0;
            let weak = !strong && refs.class(sym) != 2;
            let bits = if weak { O_WEAK } else { 0 }
                | if strong { O_STRONG } else { 0 }
                | if others { O_OTHERS } else { 0 };
            let kind = match kind_of(sym) {
                Some(kind) => kind,
                None => match self.decide(&graph, sym, weak)? {
                    Decision::Special(special) => {
                        let kind =
                            if matches!(special, Special::Section { .. } | Special::Got { .. }) {
                                O_MARKS
                            } else {
                                O_OTHER
                            };
                        specials.push((sym, special));
                        kind
                    }
                    Decision::Import(import, library) => {
                        let needed =
                            library != NONE && graph.exports.libraries[library as usize].as_needed;
                        imports.push((sym, import, library));
                        if needed { O_NEEDED } else { O_IMPORT }
                    }
                    Decision::Zero => {
                        self.define(sym, Def::Absolute(0));
                        defined.push(sym);
                        O_OTHER
                    }
                    Decision::Skip => O_OTHER,
                },
            };
            outcomes.push((sym, kind | bits));
        }
        match Arc::get_mut(&mut self.new.specials) {
            Some(list) => list.clear(),
            // alloc: once per graph built while an older state shares the list, which it keeps
            None => self.new.specials = Arc::default(),
        }
        for (sym, special) in specials {
            let at = self.new.specials.len() as u32;
            Arc::make_mut(&mut self.new.specials).push(special);
            self.define(sym, Def::Special(at));
            defined.push(sym);
        }

        // The libraries the image needs, and its imports: kept in their order across
        // incremental links so that no bind moves.
        let exports = &graph.exports;
        // alloc: one flag per shared library, once per graph built
        let mut used = vec![false; exports.libraries.len()];
        for (_, _, library) in &imports {
            if *library != NONE {
                used[*library as usize] = true;
            }
        }
        // alloc: one index per shared library, once per graph built; kept as the image's
        let mut index = vec![NONE; exports.libraries.len()];
        let mut needed: Vec<Library> = Vec::with_capacity(exports.libraries.len());
        let mut library_orders: Vec<u32> = Vec::with_capacity(exports.libraries.len());
        for (at, library) in exports.libraries.iter().enumerate() {
            if exports.canonical[at] == at as u32 && (!library.as_needed || used[at]) {
                index[at] = needed.len() as u32;
                // clone: the edge: the image's needed list owns its libraries' names, once per graph built
                needed.push(Library { name: library.name.clone(), as_needed: library.as_needed });
                library_orders.push(exports.orders[at]);
            }
        }
        let mut current: HashMap<Sym, Import, std::alloc::Global> =
            HashMap::with_capacity_in(imports.len(), std::alloc::Global);
        for (sym, mut import, library) in imports {
            import.library =
                (library != NONE).then(|| index[library as usize]).filter(|it| *it != NONE);
            current.insert(sym, import);
        }
        // An import an incremental link no longer needs keeps its index (nothing binds to it),
        // its library looked up again by name, so no other import's index moves. Its GOT cell
        // and stub go with it (`Stage::cells_of_removed`); on PE, where the GOT is the import
        // address table, the cells after it move, and their readers are written again
        // (`Stage::relay`).
        let mut order: Vec<(Sym, Import, bool)> =
            Vec::with_capacity(current.len() + old.imports.len());
        if !self.full {
            for (sym, import) in old.import_syms.iter().zip(old.imports.iter()) {
                match current.remove(sym) {
                    Some(it) => order.push((*sym, it, true)),
                    None => {
                        // clone: one per import kept in place, once per graph built
                        let mut stale = import.clone();
                        stale.library = import.library.and_then(|library| {
                            let name = &old.libraries.get(library as usize)?.name;
                            needed.iter().position(|it| &it.name == name).map(|it| it as u32)
                        });
                        stale.weak |= stale.library.is_none();
                        order.push((*sym, stale, false));
                    }
                }
            }
        }
        let mut new: Vec<(Sym, Import)> = Vec::with_capacity(current.len());
        new.extend(current);
        new.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        order.extend(new.into_iter().map(|(sym, import)| (sym, import, true)));
        let stale = order.iter().any(|it| !it.2);
        for (at, (sym, _, active)) in order.iter().enumerate() {
            if *active {
                self.define(*sym, Def::Import(at as u32));
                defined.push(*sym);
            }
        }
        let (import_syms, imports): (Vec<Sym>, Vec<Import>) =
            order.into_iter().map(|(sym, import, _)| (sym, import)).unzip();

        // A name the image's resolution defined and this one does not is undefined now.
        let mut now = CowFlags::default();
        for sym in &defined {
            now.set(Symbols::dense(*sym));
        }
        for sym in old.defined.iter() {
            if !now.get(Symbols::dense(*sym)) {
                self.define(*sym, Def::None);
            }
        }
        if let Some(entry) = graph.entry {
            if self.new.def(entry) == Def::None {
                // alloc: error path
                return Err(error(
                    None,
                    format!("the entry point {} is not defined", self.symbols.describe(entry)),
                ));
            }
        }

        // clone: copy-on-write: copied only where an outcome changes
        let mut table = old.outcomes.clone();
        for sym in old.named.iter() {
            table.set(Symbols::dense(*sym), 0, || 0);
        }
        let mut named = CowVec::default();
        for (sym, outcome) in outcomes {
            table.set(Symbols::dense(sym), outcome, || 0);
            named.push(sym);
        }
        let mut list = CowVec::default();
        for sym in defined {
            list.push(sym);
        }
        // The same lists keep their identity: the writer rebinds only when they change.
        Ok(Resolved {
            imports: keep(&old.imports, imports),
            import_syms: keep(&old.import_syms, import_syms),
            libraries: keep(&old.libraries, needed),
            library_orders: keep(&old.library_orders, library_orders),
            // clone: an `Arc`
            exports: graph.asked.clone(),
            stale,
            refs: Refs::default(),
            pieces: 0,
            removals: 0,
            // clone: an `Arc`
            inputs: self.new.inputs.clone(),
            // clone: an `Arc`
            names: self.new.names.clone(),
            // clone: an `Arc`
            wasm: self.new.wasm_imports.clone(),
            // alloc: once per graph built
            library_index: Arc::new(index),
            // alloc: once per graph built
            commons: Arc::new(commons),
            marks,
            outcomes: table,
            named,
            defined: list,
            graph: Some(graph),
        })
    }
}

/// `was` when it holds `now`, so the list keeps its identity; else `now`.
fn keep<T: PartialEq>(was: &Arc<Vec<T>>, now: Vec<T>) -> Arc<Vec<T>> {
    if **was == now {
        // clone: an `Arc`
        was.clone()
    } else {
        // alloc: once per graph built whose list changed
        Arc::new(now)
    }
}

#[cfg(test)]
mod ties_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Group, Section};
    use crate::symbols::IdNo;

    fn names(of: &[&str]) -> (Names, Vec<Name>) {
        let mut names = Names::default();
        let list = of.iter().map(|it| names.push(it).unwrap()).collect();
        (names, list)
    }

    fn symbol(name: Name, def: SymbolDef, binding: Binding, kind: SymbolKind) -> Symbol {
        Symbol { name, def, binding, kind, hidden: false }
    }

    fn section(name: Name, what: SectionKind, data: Data, fixups: Range<u32>) -> Section {
        Section { name, what, align: 1, data, fixups, retain: false }
    }

    #[test]
    fn a_table_keeps_the_first_value_of_a_name_and_finds_it_after_growing() {
        let keys: Vec<String> = (0..1000).map(|it| format!("name{it}")).collect();
        let mut table = Table::with_capacity(2);
        for (at, key) in keys.iter().enumerate() {
            let eq = |it: u32| keys[it as usize % keys.len()] == *key;
            assert_eq!(table.insert(hash(key), at as u32, eq), Ok(()));
        }
        for (at, key) in keys.iter().enumerate() {
            assert_eq!(
                table.insert(hash(key), (at + keys.len()) as u32, |it| keys
                    [it as usize % keys.len()]
                    == *key),
                Err(at as u32)
            );
            assert_eq!(
                table.find(hash(key), |it| keys[it as usize % keys.len()] == *key),
                Some(at as u32)
            );
        }
        assert_eq!(table.find(hash("absent"), |_| true), None);
        assert_eq!(Table::default().find(hash("x"), |_| true), None);
    }

    /// A slot keeps only the high half of a hash: names whose hashes share it are told apart
    /// by their names alone, across growth, and a hash differing only in its low half is
    /// another probe of the same names.
    #[test]
    fn names_whose_hashes_share_the_high_half_are_told_apart_by_name() {
        let shared = 0xdead_beef_0000_0000u64;
        let mut table = Table::with_capacity(2);
        for value in 0..300u32 {
            assert_eq!(table.insert(shared | u64::from(value), value, |it| it == value), Ok(()));
        }
        for value in 0..300u32 {
            assert_eq!(table.find(shared | 0xffff, |it| it == value), Some(value));
            assert_eq!(table.insert(shared, value + 1000, |it| it == value), Err(value));
        }
        assert_eq!(table.find(shared, |it| it == 300), None);
        assert_eq!(table.find(!shared & HIGH, |_| true), None, "no slot has another high half");
        assert_eq!(table.len, 300);
    }

    #[test]
    fn a_name_is_searched_for_in_line_order_as_a_scan_of_the_indexes_finds_it() {
        let mut rng = blaze_ids::rng::SplitMix64::new(0x5eed_a5c1);
        let pool: Vec<String> = (0..64).map(|it| format!("name{it}")).collect();
        for round in 0..200 {
            let count = 1 + rng.below(5) as usize;
            let mut inputs = Vec::with_capacity(count);
            let mut written: Vec<Vec<(String, u32)>> = Vec::with_capacity(count);
            for at in 0..count {
                let mut names = Names::default();
                let members: Vec<crate::model::Member> = (0..4)
                    .map(|it| crate::model::Member {
                        name: names.push(&format!("m{it}.o")).unwrap(),
                        bytes: 0..0,
                    })
                    .collect();
                let entries = rng.below(24) as usize;
                let (mut index, mut listed) =
                    (Vec::with_capacity(entries), Vec::with_capacity(entries));
                for _ in 0..entries {
                    let name = &pool[rng.below(pool.len() as u64) as usize];
                    let member = rng.below(4) as u32;
                    index.push((names.push(name).unwrap(), member));
                    listed.push((name.clone(), member));
                }
                written.push(listed);
                inputs.push(Input {
                    id: blaze_ids::ContentId([at as u8; 32]),
                    name: format!("lib{round}_{at}.a").into(),
                    kind: NativeKind::Archive { whole: false },
                    order: at as u32,
                    bytes: Arc::from(Vec::new()),
                    file: Arc::new(NativeFile::Archive(Archive { members, index, names })),
                });
            }
            let archives: Vec<(u32, Arc<Table>)> = inputs
                .iter()
                .enumerate()
                .map(|(at, it)| {
                    let NativeFile::Archive(archive) = &*it.file else {
                        unreachable!("made as archives")
                    };
                    (at as u32, Arc::new(archive_index(archive)))
                })
                .collect();
            for name in &pool {
                let scanned = written.iter().enumerate().find_map(|(at, listed)| {
                    listed.iter().find(|it| it.0 == *name).map(|it| (at as u32, it.1))
                });
                let found =
                    blaze_alloc_count::assert_at_most(0, "a search of the archives", || {
                        search(&archives, &inputs, name)
                    });
                assert_eq!(found, scanned, "{name}");
            }
        }
    }

    #[test]
    fn bits_set_once_across_words_and_copies() {
        let mut bits = CowFlags::default();
        assert!(bits.set(3));
        assert!(!bits.set(3));
        assert!(bits.set(64 * 2000 + 5));
        let copy = bits.clone();
        assert!(bits.set(7));
        assert!(bits.get(3) && bits.get(7) && bits.get(64 * 2000 + 5));
        assert!(!bits.get(4) && !bits.get(64 * 3000));
        assert!(!copy.get(7), "a copy does not see later writes");
    }

    #[test]
    fn rows_keep_their_items_in_order() {
        let csr = Csr::build(4, [(2, 7), (0, 1), (2, 3), (3, 9)]);
        assert_eq!(csr.row(0), &[1]);
        assert_eq!(csr.row(1), &[] as &[u32]);
        assert_eq!(csr.row(2), &[7, 3]);
        assert_eq!(csr.row(3), &[9]);
    }

    #[test]
    fn sections_kept_by_name_are_lld_s() {
        for it in [
            ".init",
            ".init_array",
            ".init_array.5",
            ".fini_array.00100",
            ".ctors",
            ".note.GNU-stack",
            ".jcr",
        ] {
            assert!(kept_by_name(it), "{it}");
        }
        for it in [".initx", ".text.init", ".data", ".init_arrayx"] {
            assert!(!kept_by_name(it), "{it}");
        }
    }

    #[test]
    fn a_reader_s_ranges_and_indexes_are_checked() {
        let good = || {
            let (names, list) = names(&[".text", "f"]);
            Object {
                sections: vec![section(
                    list[0],
                    SectionKind::Alloc(Class::Text, 0),
                    Data::Input(0..16),
                    0..1,
                )],
                symbols: vec![symbol(
                    list[1],
                    SymbolDef::Section { section: 0, value: 0, size: 4 },
                    Binding::Global,
                    SymbolKind::Function,
                )],
                fixups: vec![crate::model::NativeFixup {
                    at: 0,
                    apply: Apply::Abs64,
                    symbol: 0,
                    addend: 0,
                    minus: None,
                }],
                names,
                ..Object::default()
            }
        };
        assert_eq!(check_object(&good(), 16), Ok(()));
        assert!(check_object(&good(), 15).unwrap_err().contains("past the input's 15"));

        let mut bad = good();
        bad.fixups[0].symbol = 1;
        assert!(check_object(&bad, 16).unwrap_err().contains("past the object's 1"));

        let mut bad = good();
        bad.sections[0].fixups = 0..2;
        assert!(check_object(&bad, 16).unwrap_err().contains("relocations"));

        let mut bad = good();
        bad.symbols[0].def = SymbolDef::Section { section: 1, value: 0, size: 0 };
        assert!(check_object(&bad, 16).unwrap_err().contains("past the object's 1"));

        let mut bad = good();
        bad.groups = vec![Group { name: bad.symbols[0].name, sections: 0..1, largest: false }];
        bad.grouped = vec![4];
        assert!(check_object(&bad, 16).unwrap_err().contains("COMDAT"));

        let mut foreign = Names::default();
        foreign.push("a much longer name than any of these").unwrap();
        let mut bad = good();
        bad.symbols[0].name = foreign.push("x").unwrap();
        assert!(check_object(&bad, 16).unwrap_err().contains("outside"));
    }

    #[test]
    fn an_object_is_indexed_once_with_its_frame_records() {
        let cie = unwind::cie(1, -8, 16, &[], false, unwind::Pointers::Sdata4).unwrap();
        let fde = unwind::fde(4, &[], false, false, unwind::Pointers::Sdata4).unwrap();
        let fde_at = cie.bytes.len();
        let mut frame = cie.bytes.clone();
        frame.extend_from_slice(&fde.bytes);
        let back = (fde_at + 4) as u32;
        frame[fde_at + 4..fde_at + 8].copy_from_slice(&back.to_le_bytes());

        let (names, list) = names(&[".text", ".eh_frame", "f", "g", ".init_array", "my_section"]);
        let fixup = |at: u64, symbol: u32| crate::model::NativeFixup {
            at,
            apply: Apply::Abs64,
            symbol,
            addend: 0,
            minus: None,
        };
        let object = Object {
            sections: vec![
                section(
                    list[0],
                    SectionKind::Alloc(Class::Text, 0),
                    Data::Bytes(vec![0x90; 8]),
                    0..1,
                ),
                section(list[1], SectionKind::EhFrame, Data::Bytes(frame.clone()), 1..2),
                section(list[4], SectionKind::Alloc(Class::InitArray, 0), Data::Zero(8), 2..2),
                section(
                    list[5],
                    SectionKind::Alloc(
                        Class::Named { name: "my_section".into(), write: true, exec: false },
                        0,
                    ),
                    Data::Zero(8),
                    2..2,
                ),
            ],
            symbols: vec![
                symbol(
                    list[2],
                    SymbolDef::Section { section: 0, value: 0, size: 4 },
                    Binding::Global,
                    SymbolKind::Function,
                ),
                symbol(list[3], SymbolDef::Undefined, Binding::Weak, SymbolKind::Function),
            ],
            fixups: vec![fixup(4, 1), fixup(fde_at as u64 + 8, 0)],
            names,
            ..Object::default()
        };
        let records = vec![(1, unwind::split(&frame).unwrap())];
        assert_eq!(records[0].1.len(), 2);
        let syms: Vec<Sym> = (0..2 + 4 + 2).map(Sym).collect();
        let obj = Obj { native: IdNo(0), member: 0 };
        let parsed =
            Parsed::new(obj, Arc::new(NativeFile::Object(object)), 0, syms, &[NameId(9)], &records);

        assert_eq!(parsed.refs.row(0), &[1], "the text refers to `g`");
        assert_eq!(parsed.refs.row(1), &[0], "the frame records refer to `f`");
        assert_eq!(parsed.defs.row(0), &[0]);
        assert_eq!(parsed.roots, vec![2], "the init array is kept whatever refers to it");
        assert_eq!(parsed.named, vec![(NameId(9), 3)]);
        assert_eq!(parsed.section_sym(1), Sym(3));
        assert_eq!(parsed.frames.len(), 2);
        let (cie, fde) = (parsed.frames[0], parsed.frames[1]);
        assert!(!cie.fde && cie.cie == NONE && cie.start == 0);
        assert!(fde.fde && fde.cie == 0 && fde.start == fde_at as u32 && fde.function == 0);
        assert_eq!(parsed.frame_fixups.row(0), &[] as &[u32]);
        assert_eq!(parsed.frame_fixups.row(1), &[1]);
        assert_eq!(parsed.frame_sym(1), Sym(7));
    }

    /// An object of `n` text sections, each defining a symbol and referring to the next.
    fn chain(n: u32) -> Object {
        let (names, list) = names(&[".text", "f"]);
        Object {
            sections: (0..n)
                .map(|it| {
                    section(list[0], SectionKind::Alloc(Class::Text, 0), Data::Zero(8), it..it + 1)
                })
                .collect(),
            symbols: (0..n)
                .map(|it| {
                    symbol(
                        list[1],
                        SymbolDef::Section { section: it, value: 0, size: 8 },
                        Binding::Global,
                        SymbolKind::Function,
                    )
                })
                .collect(),
            fixups: (0..n)
                .map(|it| crate::model::NativeFixup {
                    at: 0,
                    apply: Apply::Abs64,
                    symbol: (it + 1) % n,
                    addend: 0,
                    minus: None,
                })
                .collect(),
            names,
            ..Object::default()
        }
    }

    #[test]
    fn a_native_read_allocates_per_object_not_per_section_or_symbol() {
        let obj = Obj { native: IdNo(0), member: 0 };
        let mut counts = Vec::new();
        for n in [10, 1000] {
            let object = chain(n);
            blaze_alloc_count::assert_at_most(0, "an object checked", || {
                check_object(&object, 0).unwrap()
            });
            {
                let mut keys = Vec::with_capacity(2 * n as usize);
                blaze_alloc_count::assert_at_most(0, "an object's keys into reserved room", || {
                    object_keys(obj, &object, &[], &mut keys)
                });
                assert_eq!(keys.len(), 2 * n as usize);
            }

            let file = Arc::new(NativeFile::Object(object));
            let syms: Vec<Sym> = (0..2 * n).map(Sym).collect();
            // The pairs, two indexes of two lists each and the groups: none per section or symbol.
            let (parsed, used) =
                blaze_alloc_count::measure(|| Parsed::new(obj, file, 0, syms, &[], &[]));
            assert!(used.allocations <= 8, "{n} sections and symbols: {used:?}");
            assert_eq!(parsed.refs.row(0), &[1]);
            assert_eq!(parsed.defs.row(n as usize - 1), &[n - 1]);
            counts.push(used.allocations);
        }
        assert_eq!(counts[0], counts[1], "the same count at 10 and 1000");
    }

    #[test]
    fn what_a_change_of_rust_s_references_asks() {
        let defined = O_FIXED | O_STRONG;
        assert_eq!(settled(0, 0, 2, false), Next::Gained);
        assert_eq!(settled(0, 0, 0, false), Next::Nothing);
        assert_eq!(settled(O_RUST, 2, 0, true), Next::Nothing);
        assert_eq!(settled(defined, 2, 0, false), Next::Rebuild, "a root went");
        assert_eq!(settled(defined, 1, 2, false), Next::Nothing);
        assert_eq!(settled(O_PULLED, 2, 1, false), Next::Rebuild, "the member may go");
        assert_eq!(settled(O_PULLED, 1, 2, false), Next::Nothing);
        assert_eq!(
            settled(O_IMPORT, 2, 0, false),
            Next::Nothing,
            "an incremental link keeps it in place"
        );
        assert_eq!(settled(O_IMPORT, 2, 0, true), Next::Rebuild, "a full link drops it");
        assert_eq!(settled(O_IMPORT, 2, 1, false), Next::Rebuild, "it becomes weak");
        assert_eq!(
            settled(O_IMPORT | O_STRONG | O_OTHERS, 2, 1, false),
            Next::Nothing,
            "still strong"
        );
        assert_eq!(
            settled(O_IMPORT | O_WEAK | O_OTHERS, 0, 2, false),
            Next::Rebuild,
            "it becomes strong"
        );
        assert_eq!(settled(O_NEEDED, 2, 0, false), Next::Rebuild, "its library may go");
        assert_eq!(settled(O_OTHER | O_WEAK, 1, 0, true), Next::Nothing);
        assert_eq!(settled(O_MARKS, 2, 0, false), Next::Rebuild);
    }
}
