//! The files a file's out-of-line `mod` declarations name, by rustc's rules, from the image
//! alone: the caller reads them and images them in turn.
//!
//! The directory a file's modules are in: a `mod.rs`-like file (the crate root, a `mod.rs`, a
//! file reached by `#[path]`) has its own directory; any other file `dir/foo.rs` has `dir/foo`.
//! An inline module `mod a { .. }` adds `a` (or its `#[path]`) to the directory of what it
//! holds. `mod b;` is `dir/b.rs` or `dir/b/mod.rs` (rustc refuses both present and neither;
//! the caller, who stats them, says which). `#[path = "p"]` on `mod b;` is `p` relative to the
//! file's own directory at the top level, or to the inline modules' directory inside one; one
//! `#[path]` at most may hold under the unit's cfg.
//!
//! Cost: O(declarations x depth of inline modules), no token read. Floor: the same.

#![blaze::sans_io]

use core::alloc::Allocator;

use cfg::CfgOptions;

use crate::{
    cfgs,
    read::{Image, OutOfPool},
    rows::{INLINE, NONE},
};

/// What a module file is to the directory of the modules it declares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    /// The crate root, a `mod.rs`, a file reached by `#[path]`: its modules are in its directory.
    ModRs,
    /// `dir/foo.rs`: its modules are in `dir/foo`.
    NonModRs,
}

/// A file one module may be in, and what that file is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Candidate<A: Allocator> {
    pub path: Vec<u8, A>,
    pub kind: FileKind,
}

/// One active `mod name;` of a file: its module path, and the files it may be in (one for a
/// `#[path]`, two otherwise: `name.rs` and `name/mod.rs`, exactly one of which must exist).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModFile<A: Allocator> {
    /// The mod row ([`Image::mods`]).
    pub decl: u32,
    /// The child module's path, `parent::name`.
    pub module: Vec<u8, A>,
    pub first: Candidate<A>,
    pub second: Option<Candidate<A>>,
}

/// Why a declaration has no file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModError {
    /// The image is not one this crate wrote.
    Pool(OutOfPool),
    /// Mod row `decl` has more than one `#[path]` that holds under the unit's cfg (rustc: "multiple
    /// `path` attributes").
    Paths { decl: u32 },
}

/// The files the active out-of-line modules of a file name. `file`: the file's path (`/`
/// separated, as the caller names files), `kind`: what it is, `module`: its module path (`` for
/// the crate root, `a::b`).
pub fn mod_files<A: Allocator + Clone>(
    image: &Image<'_>,
    file: &[u8],
    kind: FileKind,
    module: &[u8],
    options: &CfgOptions,
    frame: A,
) -> Result<Vec<ModFile<A>, A>, ModError> {
    let mut out = Vec::new_in(frame.clone());
    let own_dir = parent_dir(file);
    let mods = image.mods();
    for (decl, row) in mods.iter().enumerate() {
        let decl = decl as u32;
        if row.flags & INLINE != 0 || !image.active(row.row, options).map_err(ModError::Pool)? {
            continue;
        }
        let name = image.str(row.name).map_err(ModError::Pool)?;
        // The inline modules around the declaration, outermost first, as directory steps.
        let mut inline: Vec<u32, A> = Vec::new_in(frame.clone());
        let mut up = row.parent;
        while up != NONE {
            inline.push(up);
            let parent = mods.get(up as usize).ok_or(ModError::Pool(OutOfPool { pool: "the mod table", at: up }))?;
            if parent.parent != NONE && parent.parent >= up {
                return Err(ModError::Pool(OutOfPool { pool: "a parent mod after its child", at: up }));
            }
            up = parent.parent;
        }
        inline.reverse();
        let mut child_module = Vec::new_in(frame.clone());
        child_module.extend_from_slice(module);
        for &at in &inline {
            push_segment(&mut child_module, image.str(mods[at as usize].name).map_err(ModError::Pool)?);
        }
        push_segment(&mut child_module, name);
        let own_path = held_path(image, decl, row.paths, options)?;
        let mut dir = Vec::new_in(frame.clone());
        match (own_path.is_some(), inline.is_empty()) {
            // `#[path]` at the top level: relative to the file's own directory.
            (true, true) => dir.extend_from_slice(own_dir),
            _ => {
                dir.extend_from_slice(own_dir);
                if kind == FileKind::NonModRs {
                    join(&mut dir, stem(file));
                }
                for &at in &inline {
                    let parent = &mods[at as usize];
                    match held_path(image, at, parent.paths, options)? {
                        Some(path) => join(&mut dir, path),
                        None => join(&mut dir, image.str(parent.name).map_err(ModError::Pool)?),
                    }
                }
            }
        }
        let entry = match own_path {
            Some(path) => {
                let mut full = Vec::new_in(frame.clone());
                match path.first() {
                    Some(b'/') => full.extend_from_slice(path),
                    _ => {
                        full.extend_from_slice(&dir);
                        join(&mut full, path);
                    }
                }
                ModFile {
                    decl,
                    module: child_module,
                    first: Candidate { path: full, kind: FileKind::ModRs },
                    second: None,
                }
            }
            None => {
                let mut flat = dir.clone();
                join(&mut flat, name);
                flat.extend_from_slice(b".rs");
                let mut nested = dir;
                join(&mut nested, name);
                join(&mut nested, b"mod.rs");
                ModFile {
                    decl,
                    module: child_module,
                    first: Candidate { path: flat, kind: FileKind::NonModRs },
                    second: Some(Candidate { path: nested, kind: FileKind::ModRs }),
                }
            }
        };
        out.push(entry);
    }
    Ok(out)
}

/// The one `#[path]` of mod row `decl` that holds, if any.
fn held_path<'b>(
    image: &Image<'b>,
    decl: u32,
    paths: crate::rows::Span,
    options: &CfgOptions,
) -> Result<Option<&'b [u8]>, ModError> {
    let mut held = None;
    for row in image.paths(paths).map_err(ModError::Pool)? {
        let cond = image.cfg(row.cond).map_err(ModError::Pool)?;
        let holds = cfgs::holds(cond, options)
            .map_err(|_| ModError::Pool(OutOfPool { pool: "a `#[path]` condition that does not decode", at: decl }))?;
        if !holds {
            continue;
        }
        if held.is_some() {
            return Err(ModError::Paths { decl });
        }
        held = Some(image.str(row.path).map_err(ModError::Pool)?);
    }
    Ok(held)
}

/// `a/b/c.rs` -> `a/b`; `c.rs` -> ``.
fn parent_dir(file: &[u8]) -> &[u8] {
    match file.iter().rposition(|byte| *byte == b'/') {
        Some(at) => &file[..at],
        None => &[],
    }
}

/// `a/b/c.rs` -> `c`.
fn stem(file: &[u8]) -> &[u8] {
    let name = match file.iter().rposition(|byte| *byte == b'/') {
        Some(at) => &file[at + 1..],
        None => file,
    };
    name.strip_suffix(b".rs").unwrap_or(name)
}

fn join<A: Allocator>(dir: &mut Vec<u8, A>, step: &[u8]) {
    if !dir.is_empty() && !step.is_empty() {
        dir.push(b'/');
    }
    dir.extend_from_slice(step);
}

fn push_segment<A: Allocator>(module: &mut Vec<u8, A>, name: &[u8]) {
    if !module.is_empty() {
        module.extend_from_slice(b"::");
    }
    module.extend_from_slice(name);
}
