//! A crate's module tree as its own small record: each module's place (parent, name, file,
//! directories), its `mod` lines with their `#[macro_use]` bit, what the walk read of its text
//! ([`WalkRow`]) and how many macros it exports; with the crate root's record as the whole solve
//! wrote it before the cells ran, and the root's tools and attributes.
//!
//! Every names solve writes it (`CrateNames::tree_record`). A one-group solve reads it instead of
//! outlining and walking every file and lowering every module (`resolve_one` in members mode): it
//! reads, outlines and lowers its members' files alone, and checks each member against its row
//! here; a member whose walk rows, `mod` lines or macro exports moved, or that is in no row,
//! moved the record's inputs: the solve makes the tree again from the files (every file walked
//! and lowered, as a whole solve does) and its `tree_record` is the new one.
//!
//! It also lists every enum a module's own text declares, with its variants: a one-group solve
//! answers an enum of a module it does not lower from here (a member whose enums are not its
//! rows' moved the record's inputs too).
//!
//! What it is a function of: the `mod` lines, enums and inner attributes of every module's text, the
//! file set (which of `x.rs` and `x/mod.rs` exist), the cfg, the root's attributes and the
//! dependencies' roots (the crate root). A caller hands it to a one-group solve only while the
//! texts outside the members and the file set are the ones it was written from.

use blaze_format::cursor::{self, Reader};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::task::{Variant, WalkRow};

const MAGIC: [u8; 4] = *b"BZMT";
const VERSION: u32 = 3;
const NONE: u32 = u32::MAX;

/// One module of the tree, root first, each after its parent (the walk's order).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeModule<'r> {
    pub key: ItemKey,
    /// Its parent's index in [`TreeRecord::modules`].
    pub parent: Option<u32>,
    pub name: Option<&'r str>,
    /// The path of the file it is in.
    pub file: &'r str,
    /// The inline `mod` item it is in `file`; `None` for a file's own items.
    pub item: Option<u32>,
    pub dir: &'r str,
    pub path_dir: &'r str,
    pub no_implicit_prelude: bool,
    /// `krate::a::b`.
    pub path: &'r str,
    /// Its `mod` lines as its `ModuleDecls` hold them: each child and its `#[macro_use]` bit.
    pub lines: AVec<'r, (ItemKey, bool)>,
    /// What the walk read of its text.
    pub rows: AVec<'r, WalkRow<'r>>,
    /// For a file's own items: the file's inner `#![cfg]` verdict and `#![no_implicit_prelude]`.
    pub file_attrs: Option<(bool, bool)>,
    /// How many `#[macro_export]` macros its text defines (the crate root lists them).
    pub exports: u32,
    /// How many macros (`macro_rules!`, `macro`) its own text defines: a one-group solve
    /// lowers each module with any for the crate's macro table (a member's call expands by
    /// the definition's text as it is now; its digest is the call's expansion read), never
    /// for its scope, which is its record's.
    pub macros: u32,
}

/// An enum a module's own text declares, with its variants as the lowering made them: what a
/// one-group solve answers a path or a glob through an enum of a module it does not lower by
/// (`use Cow::*;` in a module outside the members: alloc's `borrow`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeEnum<'r> {
    pub key: ItemKey,
    /// Its module's index in [`TreeRecord::modules`].
    pub module: u32,
    /// In declaration order.
    pub variants: AVec<'r, Variant<&'r str>>,
}

/// The module tree record, decoded (or to be written).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeRecord<'r> {
    pub modules: AVec<'r, TreeModule<'r>>,
    /// The enums the modules' own texts declare, sorted by key, each once (an enum an
    /// expansion writes is not here: its call's solve makes it).
    pub enums: AVec<'r, TreeEnum<'r>>,
    /// The root's `#![register_tool]`s.
    pub tools: AVec<'r, &'r str>,
    pub no_std: bool,
    pub no_core: bool,
    pub arbitrary_ints: bool,
    /// The crate root's `CrateRoot` record as the solve read it (before the cells added the
    /// macro-written `extern crate`s and exports).
    pub crate_root: &'r [u8],
}

/// Why a tree record was not written or read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeRecordError {
    /// A list or a text longer than a `u32` counts.
    TooLong { what: &'static str },
    /// The bytes ended early.
    Short(cursor::Error),
    /// Not a tree record of this version.
    NotTree,
    /// A parent index past the modules before it, or a flag byte not 0 or 1.
    Malformed { what: &'static str },
}

struct Out<'b> {
    bytes: AVec<'b, u8>,
}

impl Out<'_> {
    fn u32(&mut self, v: usize, what: &'static str) -> Result<(), TreeRecordError> {
        let v = u32::try_from(v).map_err(|_| TreeRecordError::TooLong { what })?;
        self.bytes.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn opt(&mut self, v: Option<u32>) {
        self.bytes.extend_from_slice(&v.unwrap_or(NONE).to_le_bytes());
    }

    fn flag(&mut self, v: bool) {
        self.bytes.push(u8::from(v));
    }

    fn key(&mut self, k: ItemKey) {
        self.bytes.extend_from_slice(&k.0.0);
    }

    fn str(&mut self, s: &str, what: &'static str) -> Result<(), TreeRecordError> {
        self.u32(s.len(), what)?;
        self.bytes.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

fn flag(r: &mut Reader<'_>) -> Result<bool, TreeRecordError> {
    match r.u8().map_err(TreeRecordError::Short)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(TreeRecordError::Malformed { what: "flag" }),
    }
}

fn opt(r: &mut Reader<'_>) -> Result<Option<u32>, TreeRecordError> {
    let v = r.u32().map_err(TreeRecordError::Short)?;
    Ok((v != NONE).then_some(v))
}

fn key(r: &mut Reader<'_>) -> Result<ItemKey, TreeRecordError> {
    Ok(ItemKey(ContentId(*r.array::<32>().map_err(TreeRecordError::Short)?)))
}

fn count(r: &mut Reader<'_>) -> Result<usize, TreeRecordError> {
    Ok(r.u32().map_err(TreeRecordError::Short)? as usize)
}

fn text<'r>(r: &mut Reader<'r>) -> Result<&'r str, TreeRecordError> {
    r.str_u32().map_err(TreeRecordError::Short)
}

impl<'r> TreeRecord<'r> {
    /// The record's bytes: one order (the walk's), so two writers of one tree write equal bytes.
    pub fn write<'b>(&self, alloc: Alloc<'b>) -> Result<AVec<'b, u8>, TreeRecordError> {
        let mut out = Out { bytes: AVec::new_in(alloc) };
        out.bytes.extend_from_slice(&MAGIC);
        out.u32(VERSION as usize, "version")?;
        out.flag(self.no_std);
        out.flag(self.no_core);
        out.flag(self.arbitrary_ints);
        out.u32(self.tools.len(), "tools")?;
        for t in self.tools.iter() {
            out.str(t, "tool")?;
        }
        out.u32(self.crate_root.len(), "crate root")?;
        out.bytes.extend_from_slice(self.crate_root);
        out.u32(self.modules.len(), "modules")?;
        for m in self.modules.iter() {
            out.key(m.key);
            out.opt(m.parent);
            out.opt(m.item);
            out.u32(m.exports as usize, "exports")?;
            out.u32(m.macros as usize, "macros")?;
            out.flag(m.no_implicit_prelude);
            out.flag(m.name.is_some());
            out.str(m.name.unwrap_or(""), "name")?;
            out.str(m.file, "file")?;
            out.str(m.dir, "dir")?;
            out.str(m.path_dir, "path dir")?;
            out.str(m.path, "path")?;
            out.flag(m.file_attrs.is_some());
            let (cfg_ok, nip) = m.file_attrs.unwrap_or((false, false));
            out.flag(cfg_ok);
            out.flag(nip);
            out.u32(m.lines.len(), "mod lines")?;
            for &(k, macro_use) in m.lines.iter() {
                out.key(k);
                out.flag(macro_use);
            }
            out.u32(m.rows.len(), "walk rows")?;
            for row in m.rows.iter() {
                out.str(row.name, "row name")?;
                out.flag(row.inline);
                out.flag(row.no_implicit_prelude);
                out.flag(row.path.is_some());
                out.str(row.path.unwrap_or(""), "row path")?;
            }
        }
        out.u32(self.enums.len(), "enums")?;
        for e in self.enums.iter() {
            out.key(e.key);
            out.u32(e.module as usize, "enum module")?;
            out.u32(e.variants.len(), "variants")?;
            for v in e.variants.iter() {
                out.key(v.key);
                out.flag(v.record);
                out.str(v.name, "variant name")?;
            }
        }
        Ok(out.bytes)
    }

    /// The enum `key` a module's own text declares, if the record holds it.
    pub fn enum_of(&self, key: ItemKey) -> Option<&TreeEnum<'r>> {
        self.enums.binary_search_by_key(&key, |e| e.key).ok().map(|i| &self.enums[i])
    }

    /// The record in `bytes`, its texts borrowed from them.
    pub fn read(bytes: &'r [u8], alloc: Alloc<'r>) -> Result<TreeRecord<'r>, TreeRecordError> {
        let mut r = Reader::starting_at(bytes, 0).map_err(TreeRecordError::Short)?;
        if *r.array::<4>().map_err(TreeRecordError::Short)? != MAGIC || r.u32().map_err(TreeRecordError::Short)? != VERSION {
            return Err(TreeRecordError::NotTree);
        }
        let (no_std, no_core, arbitrary_ints) = (flag(&mut r)?, flag(&mut r)?, flag(&mut r)?);
        let n = count(&mut r)?;
        let mut tools = AVec::with_capacity_in(n.min(bytes.len()), alloc);
        for _ in 0..n {
            tools.push(text(&mut r)?);
        }
        let len = count(&mut r)?;
        let crate_root = r.take(len).map_err(TreeRecordError::Short)?;
        let n = count(&mut r)?;
        // Each module takes more than 32 bytes: a count past that is a short record, not memory.
        let mut modules: AVec<'r, TreeModule<'r>> = AVec::with_capacity_in(n.min(bytes.len() / 32), alloc);
        for at in 0..n {
            let module = key(&mut r)?;
            let parent = opt(&mut r)?;
            if parent.is_some_and(|p| p as usize >= at) || (parent.is_none() != (at == 0)) {
                return Err(TreeRecordError::Malformed { what: "parent" });
            }
            let item = opt(&mut r)?;
            let exports = r.u32().map_err(TreeRecordError::Short)?;
            let macros = r.u32().map_err(TreeRecordError::Short)?;
            let no_implicit_prelude = flag(&mut r)?;
            let named = flag(&mut r)?;
            let name = text(&mut r)?;
            let (file, dir, path_dir, path) = (text(&mut r)?, text(&mut r)?, text(&mut r)?, text(&mut r)?);
            let has_file_attrs = flag(&mut r)?;
            let (cfg_ok, nip) = (flag(&mut r)?, flag(&mut r)?);
            let n = count(&mut r)?;
            let mut lines = AVec::with_capacity_in(n.min(bytes.len() / 33), alloc);
            for _ in 0..n {
                lines.push((key(&mut r)?, flag(&mut r)?));
            }
            let n = count(&mut r)?;
            let mut rows = AVec::with_capacity_in(n.min(bytes.len() / 11), alloc);
            for _ in 0..n {
                let name = text(&mut r)?;
                let (inline, no_implicit_prelude, has_path) = (flag(&mut r)?, flag(&mut r)?, flag(&mut r)?);
                let path = text(&mut r)?;
                rows.push(WalkRow { name, inline, no_implicit_prelude, path: has_path.then_some(path) });
            }
            modules.push(TreeModule {
                key: module,
                parent,
                name: named.then_some(name),
                file,
                item,
                dir,
                path_dir,
                no_implicit_prelude,
                path,
                lines,
                rows,
                file_attrs: has_file_attrs.then_some((cfg_ok, nip)),
                exports,
                macros,
            });
        }
        let n = count(&mut r)?;
        // Each enum takes more than 32 bytes, each variant more than 32 too.
        let mut enums: AVec<'r, TreeEnum<'r>> = AVec::with_capacity_in(n.min(bytes.len() / 32), alloc);
        for _ in 0..n {
            let enum_key = key(&mut r)?;
            if enums.last().is_some_and(|e: &TreeEnum<'r>| e.key >= enum_key) {
                return Err(TreeRecordError::Malformed { what: "enum order" });
            }
            let module = r.u32().map_err(TreeRecordError::Short)?;
            if module as usize >= modules.len() {
                return Err(TreeRecordError::Malformed { what: "enum module" });
            }
            let v = count(&mut r)?;
            let mut variants = AVec::with_capacity_in(v.min(bytes.len() / 32), alloc);
            for _ in 0..v {
                let variant = key(&mut r)?;
                let record = flag(&mut r)?;
                let name = text(&mut r)?;
                variants.push(Variant { name, key: variant, record });
            }
            enums.push(TreeEnum { key: enum_key, module, variants });
        }
        r.finish().map_err(TreeRecordError::Short)?;
        Ok(TreeRecord { modules, enums, tools, no_std, no_core, arbitrary_ints, crate_root })
    }
}
