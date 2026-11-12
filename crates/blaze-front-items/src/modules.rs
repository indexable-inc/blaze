//! `modules(crate)`: the crate's file set as a step (end-state.md, review point 3).
//!
//! From the root file's `items`, follow each `mod name;` declaration to its file, by rustc's
//! rules: beside a file that owns its directory (the root, a `mod.rs`, a file a `#[path]`
//! named) the candidates are `dir/name.rs` and `dir/name/mod.rs`; beside any other file
//! `dir/stem/name.rs` and `dir/stem/name/mod.rs`; each inline `mod` around the declaration
//! adds its name (or its `#[path]`) to the directory; a `#[path]` on the declaration names the
//! file itself, from the file's directory (from the inline directory when it is inside inline
//! modules). A `cfg` on the declaration or on an inline module around it is a read of the
//! crate's cfg, and so is the predicate of a `cfg_attr` that writes a `path` or a `cfg`.
//!
//! The existence of BOTH candidates is read whichever is found, as absence reads: creating
//! the other one later reaches this step (and makes the module [`Status::Ambiguous`], which
//! rustc refuses).
//!
//! The value is a [`crate::table`] of [`MODULES`]: under the hash of each module's path, its
//! [`Module`]. A module whose file is missing or ambiguous is an entry saying so, with both
//! candidates' names, for the diagnostic; it is not followed.
//!
//! Pure over its reads ([`ModuleReads`]): the items of a file, whether a file exists, whether
//! a cfg predicate holds. It reads of `items(file)` only the list of declared modules, their
//! entries and the entries of the inline modules around them, so with part reads a body edit
//! does not reach it.
//!
//! Cost against the floor: one read of each file's declared modules, two existence reads a
//! declaration, one cfg read a distinct predicate met. The floor for "a file was added" is
//! the declarations whose candidates it is: with part reads this step reruns for the crate
//! (`O(declarations)` table probes) and its value changes in that module's entry alone.
//! Bounded (rule 18): [`MOST_DEPTH`] modules deep and [`MOST_MODULES`] in all, refused by name
//! past either.

#![blaze::sans_io]

use alloc::{format, string::String, vec::Vec};

use blaze_ids::ContentId;

use crate::{
    entry::{self, DECLARED, Entry, Gate, ItemId, Kind},
    items::ITEMS,
    parse::Reading,
    table::{self, Family, Key, Table},
};

/// The modules table's family.
pub const MODULES: Family = Family { number: 3, layout: 1, name: "modules" };

/// How deep a module may be declared: past it is a `#[path]` cycle, refused naming the path.
pub const MOST_DEPTH: u32 = 128;
/// How many modules one crate may have.
pub const MOST_MODULES: usize = 1 << 20;

const PATH_DOMAIN: &str = "blaze v2 front module path 1";

/// What `modules(crate)` reads.
pub trait ModuleReads {
    type Error;

    /// The `items(file)` table of the file named `file`.
    fn items(&mut self, file: &str) -> Result<Vec<u8>, Self::Error>;
    /// Whether the file named `file` exists.
    fn exists(&mut self, file: &str) -> Result<bool, Self::Error>;
    /// Whether the cfg predicate (its tokens joined by one space) holds for the crate.
    fn cfg(&mut self, predicate: &str) -> Result<bool, Self::Error>;
    /// The error of a refusal that names what and where.
    fn refused(what: String) -> Self::Error;
}

/// What became of a declared module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Status {
    /// `file` is the module's file.
    Found = 1,
    /// Neither candidate exists: `file` and `other` are the two.
    Missing = 2,
    /// Both candidates exist: `file` and `other`.
    Ambiguous = 3,
    /// Declared in a block with no `#[path]`, where rustc takes none.
    NeedsPath = 4,
}

/// One module of a crate, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Module<'a> {
    pub status: Status,
    /// Whether the file owns its directory: its own declarations are looked up beside it.
    pub owns_dir: bool,
    pub reading: Reading,
    /// The module's path from the crate's root, `::` between names; empty for the root.
    pub path: &'a str,
    pub file: &'a str,
    /// The other candidate, of a missing or an ambiguous module.
    pub other: &'a str,
}

/// The key of the module at `path`.
pub fn key(path: &str) -> Key {
    ContentId::of(PATH_DOMAIN, path.as_bytes()).0[..16].try_into().expect("a content id is 32 bytes")
}

fn write_module(module: &Module<'_>) -> Vec<u8> {
    // alloc: one module's entry, kept in the step's value
    let mut out = Vec::new();
    out.extend_from_slice(&[
        module.status as u8,
        u8::from(module.owns_dir),
        module.reading.edition as u8,
        module.reading.extensions.bits(),
    ]);
    for text in [module.path, module.file, module.other] {
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    out
}

impl<'a> Module<'a> {
    /// The module `bytes` are; other bytes are refused saying why.
    pub fn read(bytes: &'a [u8]) -> Result<Module<'a>, String> {
        // alloc: error path
        let torn = || String::from("a module entry cut short");
        let [status, owns_dir, edition, extensions, rest @ ..] = bytes else { return Err(torn()) };
        let status = match status {
            1 => Status::Found,
            2 => Status::Missing,
            3 => Status::Ambiguous,
            4 => Status::NeedsPath,
            // alloc: error path
            other => return Err(format!("a module entry of status {other}, which is none")),
        };
        let (Some(extensions), true) =
            (syntax::Extensions::from_bits(*extensions), *edition <= syntax::Edition::LATEST as u8)
        else {
            // alloc: error path
            return Err(String::from("a module entry whose edition or extensions name none"));
        };
        let mut rest = rest;
        let mut texts = [""; 3];
        for text in &mut texts {
            let (len, after) = rest.split_first_chunk::<4>().ok_or_else(torn)?;
            let len = u32::from_le_bytes(*len) as usize;
            if after.len() < len {
                return Err(torn());
            }
            // alloc: error path
            *text = core::str::from_utf8(&after[..len]).map_err(|_| String::from("a module entry whose text is not UTF-8"))?;
            rest = &after[len..];
        }
        let reading = Reading { edition: syntax::Edition::from_u32(u32::from(*edition)), extensions };
        Ok(Module { status, owns_dir: *owns_dir != 0, reading, path: texts[0], file: texts[1], other: texts[2] })
    }
}

/// `dir` and the relative path `rel`, `.` and `..` resolved by name; an absolute `rel` names
/// no file of the pinned tree and is `None`.
fn join(dir: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None;
    }
    // alloc: the parts of one path, while it is joined
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            // alloc: the parts of one path
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn dir_of(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The string a literal's token writes, for a `#[path]`: plain and raw strings, `\\` and `\"`.
fn unquote(literal: &str) -> Option<String> {
    let raw = literal.strip_prefix('r').map(|rest| rest.trim_matches('#'));
    let inner = raw.unwrap_or(literal).strip_prefix('"')?.strip_suffix('"')?;
    // alloc: one path's text
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if raw.is_none() => match chars.next()? {
                c @ ('\\' | '"') => out.push(c),
                _ => return None,
            },
            c => out.push(c),
        }
    }
    Some(out)
}

/// What an item's gates come to: whether it is compiled, and its `#[path]`.
struct Gated {
    live: bool,
    path: Option<String>,
}

/// Applies one attribute's tokens (`cfg ( .. )`, `path = ".."`, `cfg_attr ( .. , .. )`).
fn apply<R: ModuleReads>(
    tokens: &[&str],
    of: &str,
    reads: &mut R,
    out: &mut Gated,
) -> Result<(), R::Error> {
    match tokens {
        ["cfg", "(", predicate @ .., ")"] => out.live &= reads.cfg(&predicate.join(" "))?,
        ["path", "=", literal] => match unquote(literal) {
            Some(path) => out.path = Some(path),
            None => {
                // alloc: error path
                return Err(R::refused(format!("`mod {of}`: `#[path = {literal}]` is no string this reads")));
            }
        },
        ["cfg_attr", "(", inner @ .., ")"] => {
            // alloc: one attribute's parts, while it is applied
            let mut parts: Vec<&[&str]> = Vec::new();
            let (mut depth, mut start) = (0u32, 0);
            for (at, token) in inner.iter().enumerate() {
                match *token {
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => depth = depth.saturating_sub(1),
                    "," if depth == 0 => {
                        parts.push(&inner[start..at]);
                        start = at + 1;
                    }
                    _ => {}
                }
            }
            parts.push(&inner[start..]);
            let Some((predicate, written)) = parts.split_first() else { return Ok(()) };
            let matters = |part: &&[&str]| matches!(part.first(), Some(&("cfg" | "path" | "cfg_attr")));
            if written.iter().any(matters) && reads.cfg(&predicate.join(" "))? {
                for part in written {
                    apply(part, of, reads, out)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn gated<R: ModuleReads>(entry: &Entry<'_>, reads: &mut R) -> Result<Gated, R::Error> {
    let mut out = Gated { live: true, path: None };
    for (class, tokens) in entry.gates() {
        if matches!(class, Gate::Cfg | Gate::CfgAttr | Gate::Path) {
            // alloc: one attribute's tokens, while it is applied
            let tokens: Vec<&str> = tokens.collect();
            apply(&tokens, entry.name, reads, &mut out)?;
        }
    }
    Ok(out)
}

/// A file whose declarations are still to follow.
struct Todo {
    path: String,
    file: String,
    owns_dir: bool,
    depth: u32,
}

/// The crate's modules, from its root file `root`, as the table of [`MODULES`].
pub fn modules<R: ModuleReads>(root: &str, reading: Reading, reads: &mut R) -> Result<Vec<u8>, R::Error> {
    // alloc: the crate's rows and its work list, while the table is made
    let (mut rows, mut todo) = (Vec::new(), Vec::new());
    let found = |path: &str, file: &str, owns_dir: bool| {
        write_module(&Module { status: Status::Found, owns_dir, reading, path, file, other: "" })
    };
    rows.push((key(""), found("", root, true)));
    // alloc: the root's names, held while it is followed
    todo.push(Todo { path: String::new(), file: String::from(root), owns_dir: true, depth: 0 });
    while let Some(Todo { path, file, owns_dir, depth }) = todo.pop() {
        if depth > MOST_DEPTH || rows.len() > MOST_MODULES {
            // alloc: error path
            return Err(R::refused(format!(
                "module `{path}` (file `{file}`) is {depth} modules deep among {} modules: past the bound of {MOST_DEPTH} deep and {MOST_MODULES} in all, as a `#[path]` cycle is",
                rows.len()
            )));
        }
        let bytes = reads.items(&file)?;
        // alloc: error path
        let refuse = |what: String| R::refused(format!("items of `{file}`: {what}"));
        let table = Table::read(ITEMS, &bytes).map_err(|error| refuse(format!("{error}")))?;
        let entry_of = |id: ItemId| -> Result<Entry<'_>, R::Error> {
            let bytes = table
                .get(&id.0)
                // alloc: error path
                .ok_or_else(|| refuse(format!("no entry under {id:?}, which another entry names")))?;
            Entry::read(bytes).map_err(refuse)
        };
        let declared = table.get(&ItemId::MODS.0).ok_or_else(|| refuse(String::from("no list of declared modules")))?;
        for id in entry::list(declared).map_err(refuse)? {
            let entry = entry_of(id)?;
            if entry.kind != Kind::Mod || !entry.is(DECLARED) {
                // alloc: error path
                return Err(refuse(format!("{id:?} is listed as a declared module and is a {:?}", entry.kind)));
            }
            let mut gates = gated(&entry, reads)?;
            // The inline modules around the declaration, innermost first; a container that is
            // no module is a block.
            // alloc: the few modules around one declaration
            let mut around: Vec<(&str, Option<String>)> = Vec::new();
            let (mut at, mut in_block) = (entry.container, false);
            while at != ItemId::ROOT {
                let container = entry_of(at)?;
                if container.kind == Kind::Mod {
                    let outer = gated(&container, reads)?;
                    gates.live &= outer.live;
                    around.push((container.name, outer.path));
                } else {
                    in_block = true;
                }
                at = container.container;
            }
            if !gates.live {
                continue;
            }
            let mut module = path.clone(); // clone: the parent's path starts the child's
            // alloc: error path
            let outside = |rel: &str| R::refused(format!("`mod {}` in `{file}`: `#[path = \"{rel}\"]` names no file of the pinned tree", entry.name));
            // alloc: the directory the declaration's file is looked up in
            let mut dir = if owns_dir { String::from(dir_of(&file)) } else { String::from(file.strip_suffix(".rs").unwrap_or(&file)) };
            for (name, inline_path) in around.iter().rev() {
                let step = inline_path.as_deref().unwrap_or(*name);
                dir = join(&dir, step).ok_or_else(|| outside(step))?;
                module = if module.is_empty() { String::from(*name) } else { format!("{module}::{name}") };
            }
            module = if module.is_empty() { String::from(entry.name) } else { format!("{module}::{}", entry.name) };
            let lost = |status, file: &str, other: &str| {
                write_module(&Module { status, owns_dir: false, reading, path: &module, file, other })
            };
            let (row, next) = if let Some(rel) = &gates.path {
                let from = if around.is_empty() { dir_of(&file) } else { dir.as_str() };
                let target = join(from, rel).ok_or_else(|| outside(rel))?;
                match reads.exists(&target)? {
                    true => (found(&module, &target, true), Some((target, true))),
                    false => (lost(Status::Missing, &target, ""), None),
                }
            } else if in_block {
                (lost(Status::NeedsPath, "", ""), None)
            } else {
                let flat = format!("{dir}{}{}.rs", if dir.is_empty() { "" } else { "/" }, entry.name);
                let nested = format!("{dir}{}{}/mod.rs", if dir.is_empty() { "" } else { "/" }, entry.name);
                // Both, whichever is there: the absence of the other is a read.
                match (reads.exists(&flat)?, reads.exists(&nested)?) {
                    (true, false) => (found(&module, &flat, false), Some((flat, false))),
                    (false, true) => (found(&module, &nested, true), Some((nested, true))),
                    (true, true) => (lost(Status::Ambiguous, &flat, &nested), None),
                    (false, false) => (lost(Status::Missing, &flat, &nested), None),
                }
            };
            rows.push((key(&module), row));
            if let Some((file, owns_dir)) = next {
                todo.push(Todo { path: module, file, owns_dir, depth: depth + 1 });
            }
        }
    }
    // alloc: error path
    table::write(MODULES, rows).map_err(|error| R::refused(format!("{error}")))
}
