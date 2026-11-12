//! The kernel steps of this crate: `parse(file, edition)`, `items(file)`, `lines(file)` and
//! `modules(crate)`, over any world that gives a file's text by its stable name, whether a
//! file exists, and a crate's cfg ([`Source`]).
//!
//! - [`Parse`] reads the file's text ([`FileText`]); its value is [`crate::parse::parse`]'s
//!   record. The edition and the syntax extensions are in its identity's flags (a full
//!   parse's record does not name its edition).
//! - [`Items`] and [`Lines`] read [`Parse`]'s value and the same text (the record shares it,
//!   it does not hold it); their values are the two tables of [`crate::items::file_items`]
//!   (`Ask::TABLE`), read by part: an item's entry under its id, its body under
//!   [`crate::entry::body_key`]. They are two steps so that a comment edit, which changes only
//!   the line part, leaves `items(file)`'s value equal and cuts every reader off there.
//! - [`Modules`] is [`crate::modules::modules`] over parts of [`Items`], [`Exists`] and
//!   [`Cfg`]; its value is a table too, a part a module path.
//!
//! The step numbers are the kernel's (`blaze_kernel::steps`, the one table of them). The
//! kinds of read ([`reads`]) are this world's own: the kernel keeps only the kinds it
//! answers itself.
//!
//! Cost against the floor (`crate::cutoffs`, each number pinned there): a comment edit reruns
//! [`Parse`] (`O(file)`: the floor's reparse) and [`Items`] (one read of the record, one
//! walk), whose value is equal: cut, no reader reached. [`Lines`] reruns and changes: its
//! readers only. A body edit reaches the readers of that body's part; [`Modules`] reads the
//! list of declared modules and their entries, so it is reached by neither.

#![blaze::sans_io]

use alloc::{format, string::String, vec::Vec};
use core::marker::PhantomData;

use blaze_cas::Kind as StoreKind;
use blaze_ids::ContentId;
use blaze_kernel::{
    Ask, Compute, Fact, Read, ReadKind, Reads, Seen, Step, StepError, StepId, Subject, World,
    WorldError, steps as ids,
};
use syntax::{Edition, Extensions};

use crate::{
    items::file_items,
    modules::{self, ModuleReads},
    parse::{Reading, parse, tree},
};

/// The kinds of this crate's reads of the world: a [`Source`]'s `hash` answers them.
pub mod reads {
    use blaze_kernel::ReadKind;

    /// A file's text; its key is the file's stable name. Gone when there is no such file.
    pub const FILE: ReadKind = ReadKind(0x0201);
    /// Whether a file exists; its key is the file's stable name. Gone when it does not: a
    /// read that saw nothing holds only while nothing is there, so creating the file reaches
    /// its readers.
    pub const EXISTS: ReadKind = ReadKind(0x0202);
    /// Whether a cfg predicate holds for a crate; its key is the crate's stable name, a zero
    /// byte, and the predicate's tokens joined by one space.
    pub const CFG: ReadKind = ReadKind(0x0203);
}

/// The world these steps read: the pinned file tree and the crates' cfg. A file's stable
/// name is its path in the pinned tree, `/` between its parts: never a path in a checkout.
pub trait Source: World {
    /// The text of the file named `name`; `None` when there is no such file.
    fn file(&self, name: &str) -> Result<Option<&str>, WorldError>;

    /// Whether `predicate` (a cfg predicate's tokens joined by one space) holds for `krate`.
    fn cfg(&self, krate: &str, predicate: &str) -> Result<bool, WorldError>;
}

/// A file's text.
pub struct FileText;

impl<W: Source + ?Sized> Fact<W> for FileText {
    const KIND: ReadKind = reads::FILE;
    type Key = str;
    type Value<'w>
        = Option<&'w str>
    where
        W: 'w;

    fn key(key: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(key.as_bytes());
    }

    fn get<'w>(world: &'w W, key: &str) -> Result<(Option<&'w str>, Seen), WorldError> {
        let seen = world.hash(Read { kind: reads::FILE, key: key.as_bytes() })?;
        Ok((world.file(key)?, seen))
    }
}

/// Whether a file exists.
pub struct Exists;

impl<W: Source + ?Sized> Fact<W> for Exists {
    const KIND: ReadKind = reads::EXISTS;
    type Key = str;
    type Value<'w>
        = bool
    where
        W: 'w;

    fn key(key: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(key.as_bytes());
    }

    fn get<'w>(world: &'w W, key: &str) -> Result<(bool, Seen), WorldError> {
        let seen = world.hash(Read { kind: reads::EXISTS, key: key.as_bytes() })?;
        Ok((seen != Seen::Gone, seen))
    }
}

/// Whether a cfg predicate holds for a crate: the key is the crate, a zero byte, the
/// predicate.
pub struct Cfg;

impl<W: Source + ?Sized> Fact<W> for Cfg {
    const KIND: ReadKind = reads::CFG;
    type Key = str;
    type Value<'w>
        = bool
    where
        W: 'w;

    fn key(key: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(key.as_bytes());
    }

    fn get<'w>(world: &'w W, key: &str) -> Result<(bool, Seen), WorldError> {
        let read = Read { kind: reads::CFG, key: key.as_bytes() };
        let seen = world.hash(read)?;
        let Some((krate, predicate)) = key.split_once('\0') else {
            return Err(WorldError::new(read, "a cfg read's key is a crate, a zero byte, a predicate"));
        };
        Ok((world.cfg(krate, predicate)?, seen))
    }
}

/// A file as a step reads it: its stable name and how it is read.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileKey {
    pub name: String,
    pub reading: Reading,
}

const FILE_DOMAIN: &str = "blaze v2 front file 1";
const READING_DOMAIN: &str = "blaze v2 front reading 1";

fn reading_bytes(reading: Reading) -> [u8; 2] {
    [reading.edition as u8, reading.extensions.bits()]
}

fn file_subject(input: &FileKey) -> Subject {
    Subject {
        of: ContentId::of(FILE_DOMAIN, input.name.as_bytes()),
        flags: ContentId::of(READING_DOMAIN, &reading_bytes(input.reading)),
    }
}

/// A file key's stable bytes: the name, then the edition and the extensions, one byte each.
fn file_key(input: &FileKey, out: &mut Vec<u8>) {
    out.extend_from_slice(input.name.as_bytes());
    out.extend_from_slice(&reading_bytes(input.reading));
}

fn file_input(key: &[u8]) -> Result<FileKey, String> {
    let [name @ .., edition, extensions] = key else {
        // alloc: error path
        return Err(format!("{} bytes, under the two a file key ends with", key.len()));
    };
    let Ok(name) = core::str::from_utf8(name) else {
        // alloc: error path
        return Err(String::from("a file's name that is not UTF-8"));
    };
    if *edition > Edition::LATEST as u8 {
        // alloc: error path
        return Err(format!("edition byte {edition}, which names none"));
    }
    let Some(extensions) = Extensions::from_bits(*extensions) else {
        // alloc: error path
        return Err(format!("syntax extensions {extensions:#x}, which name no set"));
    };
    // alloc: the input a key decodes to
    let name = String::from(name);
    Ok(FileKey { name, reading: Reading { edition: Edition::from_u32(u32::from(*edition)), extensions } })
}

fn file_label(what: &str, input: &FileKey) -> String {
    // alloc: an error's or a check's label, never on a hit's path
    format!("{what} of `{}` (edition {})", input.name, input.reading.edition.number())
}

/// The text of `input`'s file, read and recorded. No such file is refused naming it.
fn text_of<'w, W: Source + ?Sized>(
    ctx: &mut Reads<'w, W>,
    input: &FileKey,
) -> Result<&'w str, StepError> {
    match ctx.get::<FileText>(&input.name)? {
        Some(text) => Ok(text),
        // alloc: error path
        None => Err(StepError::Refused(format!("no file `{}` in the pinned tree", input.name))),
    }
}

macro_rules! file_step {
    ($(#[$doc:meta])* $name:ident, $id:expr, $text:literal, table: $table:literal) => {
        $(#[$doc])*
        pub struct $name<W: ?Sized>(PhantomData<fn(&W)>);

        impl<W: Source + ?Sized + 'static> Step for $name<W> {
            const ID: StepId = $id;
            const NAME: &'static str = $text;
            const KIND: StoreKind = StoreKind::Query;
            type Input = FileKey;

            fn subject(input: &FileKey) -> Subject {
                file_subject(input)
            }

            fn label(input: &FileKey) -> String {
                file_label($text, input)
            }
        }

        impl<W: Source + ?Sized + 'static> Ask for $name<W> {
            const TABLE: bool = $table;

            fn key(input: &FileKey, out: &mut Vec<u8>) {
                file_key(input, out);
            }

            fn input(key: &[u8]) -> Result<FileKey, String> {
                file_input(key)
            }
        }
    };
}

file_step!(
    /// `parse(file, edition)`: the tree's record.
    Parse, ids::PARSE, "parse", table: false
);
file_step!(
    /// `items(file)`: the table of the file's items, an entry under each item's id and a
    /// body under each [`crate::entry::body_key`].
    Items, ids::ITEMS, "items", table: true
);
file_step!(
    /// `lines(file)`: the file's line part, a table: a row under each item's id.
    Lines, ids::LINES, "lines", table: true
);

impl<W: Source + ?Sized + 'static> Compute for Parse<W> {
    type World = W;

    fn compute(ctx: &mut Reads<'_, W>, input: &FileKey) -> Result<Vec<u8>, StepError> {
        Ok(parse(text_of(ctx, input)?, input.reading))
    }
}

/// Both parts of `input`'s file, from its tree's record and its text.
fn parts<W: Source + ?Sized + 'static>(
    ctx: &mut Reads<'_, W>,
    input: &FileKey,
) -> Result<crate::items::FileItems, StepError> {
    let record = ctx.step::<Parse<W>>(input)?;
    let text = text_of(ctx, input)?;
    let parsed = tree(&record, text, input.reading)
        // alloc: error path
        .map_err(|what| StepError::Refused(format!("`{}`: {what}", input.name)))?;
    // alloc: error path
    file_items(parsed.root()).map_err(|what| StepError::Refused(format!("`{}`: {what}", input.name)))
}

impl<W: Source + ?Sized + 'static> Compute for Items<W> {
    type World = W;

    fn compute(ctx: &mut Reads<'_, W>, input: &FileKey) -> Result<Vec<u8>, StepError> {
        Ok(parts(ctx, input)?.items)
    }
}

impl<W: Source + ?Sized + 'static> Compute for Lines<W> {
    type World = W;

    fn compute(ctx: &mut Reads<'_, W>, input: &FileKey) -> Result<Vec<u8>, StepError> {
        Ok(parts(ctx, input)?.lines)
    }
}

/// A crate as `modules(crate)` reads it: its stable name (what its cfg is asked by) and its
/// root file.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CrateKey {
    pub krate: String,
    pub root: FileKey,
}

const CRATE_DOMAIN: &str = "blaze v2 front crate 1";

/// `modules(crate)`: the crate's file set, the table module path → file.
pub struct Modules<W: ?Sized>(PhantomData<fn(&W)>);

impl<W: Source + ?Sized + 'static> Step for Modules<W> {
    const ID: StepId = ids::MODULES;
    const NAME: &'static str = "modules";
    const KIND: StoreKind = StoreKind::Query;
    type Input = CrateKey;

    fn subject(input: &CrateKey) -> Subject {
        let mut hasher = blaze_ids::Hasher::new(CRATE_DOMAIN);
        hasher.part(input.krate.as_bytes()).part(input.root.name.as_bytes());
        Subject {
            of: hasher.finish(),
            flags: ContentId::of(READING_DOMAIN, &reading_bytes(input.root.reading)),
        }
    }

    fn label(input: &CrateKey) -> String {
        // alloc: an error's or a check's label, never on a hit's path
        format!("modules of crate `{}` from `{}`", input.krate, input.root.name)
    }
}

impl<W: Source + ?Sized + 'static> Ask for Modules<W> {
    const TABLE: bool = true;

    /// The crate's name, a zero byte, then the root's file key. A crate's name holds no zero
    /// byte (it is an identifier), so the first one ends it.
    fn key(input: &CrateKey, out: &mut Vec<u8>) {
        out.extend_from_slice(input.krate.as_bytes());
        out.push(0);
        file_key(&input.root, out);
    }

    fn input(key: &[u8]) -> Result<CrateKey, String> {
        let Some(end) = key.iter().position(|byte| *byte == 0) else {
            // alloc: error path
            return Err(String::from("a crate key with no zero byte after the crate's name"));
        };
        let Ok(krate) = core::str::from_utf8(&key[..end]) else {
            // alloc: error path
            return Err(String::from("a crate's name that is not UTF-8"));
        };
        // alloc: the input a key decodes to
        Ok(CrateKey { krate: String::from(krate), root: file_input(&key[end + 1..])? })
    }
}

/// [`modules::modules`]'s reads as reads of the kernel's.
struct Asked<'c, 'w, W: Source + ?Sized + 'static> {
    ctx: &'c mut Reads<'w, W>,
    input: &'c CrateKey,
}

impl<W: Source + ?Sized + 'static> ModuleReads for Asked<'_, '_, W> {
    type Error = StepError;

    fn part(&mut self, file: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StepError> {
        // alloc: the key of the step asked for
        let input = FileKey { name: String::from(file), reading: self.input.root.reading };
        self.ctx.part::<Items<W>>(&input, key)
    }

    fn exists(&mut self, file: &str) -> Result<bool, StepError> {
        Ok(self.ctx.get::<Exists>(file)?)
    }

    fn cfg(&mut self, predicate: &str) -> Result<bool, StepError> {
        // alloc: the read's key
        let key = format!("{}\0{predicate}", self.input.krate);
        Ok(self.ctx.get::<Cfg>(&key)?)
    }

    fn refused(what: String) -> StepError {
        StepError::Refused(what)
    }
}

impl<W: Source + ?Sized + 'static> Compute for Modules<W> {
    type World = W;

    fn compute(ctx: &mut Reads<'_, W>, input: &CrateKey) -> Result<Vec<u8>, StepError> {
        let mut asked = Asked { ctx, input };
        modules::modules(&input.root.name, input.root.reading, &mut asked)
    }
}
