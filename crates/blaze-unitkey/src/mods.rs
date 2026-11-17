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
//! # Modules a macro call writes
//!
//! std declares its platform modules inside `cfg_select!` calls (`sys/pal/mod.rs`,
//! `core/src/io/io_slice.rs:5-28`'s `#[path = "io_slice/repr_iovec.rs"] mod repr;`), and the
//! vendored libc inside `cfg_if!`, a `macro_rules!` of its own, which no key task can expand
//! (expansion needs name resolution, which needs the surface, which needs the files: the key
//! comes first). One rule covers both, and every other macro.
//!
//! The rule: the image reads the tokens of every item-level macro call, and of the calls
//! nested in its token tree ([`crate::image`]'s `Walk::call_mods`). Every `mod <name>;` there,
//! with every `path = ".."` in the attributes right before it in the tokens, and every
//! `mod <name> { .. }` there, with the directory its nesting gives the modules inside it, is a
//! declaration of the file the call is in, at the call's place ([`IN_CALL`]). Every candidate
//! file of it that exists on disk is one of the unit's files, whatever cfg the tokens around it
//! say: `name.rs`, `name/mod.rs` and each `#[path]`'s file (each a [`ModFile`] with `in_call`;
//! the caller takes the ones that exist, and none is an error). The files found are read the
//! same way, so their own modules, in calls or not, are found too. Only the call's own row's
//! `#[cfg]` is applied: a call that is off is not expanded at all. An inline module in the
//! tokens with two `#[path]`s gives its modules its name's directory.
//!
//! A macro the unit defines. The vendored libc declares `types` in the body of its own
//! `macro_rules! prelude` (`libc-0.2.191/src/macros.rs:63-66`), called by `prelude!();` in five
//! `cfg_if!` arms (`lib.rs:245..277`). The same rule reaches it: a `mod <name>;` (or an inline
//! `mod <name> { .. }`) in the body of a `macro_rules!` or `macro` the unit defines
//! ([`IN_MACRO`], its tokens read as a call's are) is a declaration at every call in the unit
//! of a macro of that name ([`macro_sites`], [`macro_files`]): matched by the call path's last
//! segment, the item-level calls and the calls nested in a call's tokens alike, each relative
//! to the calling file's directory and the call's inline modules, `#[path]` as above. The
//! gather takes the sites each time its files grow, until none is new, since a file a site
//! names may hold more calls or definitions. A definition that is off and a call that is off
//! name nothing. A call inside a definition's body is not followed (no key task can know the
//! body is used); a module only such a nest names, and one in the body of a macro another
//! crate defines, which no key task of this unit sees, is not gathered: the surface task
//! refuses it by name (hir-def's `deps::Refused`), as for `mod $name;`. Cost: for each
//! definition with modules, its calls' count x its modules (libc: `prelude`, 5 x 1).
//!
//! A file only reached this way (a candidate) cannot fail the unit by what it holds: the
//! gather defers what it cannot answer in it, and the surface refuses that only if the def
//! map uses the file (`blaze-command`'s `front::Deferred`).
//!
//! Why it is sound. It over-approximates: a unit's files are then every file expansion could
//! name by these forms, and more inputs than the program reads never serve a stale record
//! (rule 21); keys are still made from text before any work (rule 65). A file the expansion
//! names by another form (a name made by `concat_idents!` or a paste macro, `mod $name;`) is
//! not gathered; the front then finds a module whose file it was not given and refuses it by
//! name (hir-def's `deps::Refused`, the surface task), never a wrong answer. The call's tokens,
//! all of them, are in its row's header digest and in its file image's key.
//!
//! A file only an unused arm reaches is imaged and keyed like any other: its items have body
//! keys and pieces in the surface key, and no def of the def map. That is intended: those body
//! keys are never asked, and the surface key covers a file the surface does not read, which
//! only keys more. Nothing in the key task (`crate::unit`) or the manifest asks that an imaged
//! file be a module of the def map. Two files under one module path (`repr` from three arms)
//! are told apart by the gather, which gives the later ones a suffixed module path
//! ([`module_twice`]), so their stable ids differ.
//!
//! What it costs: the unused arms' files are read, hashed and imaged once, then stand by their
//! marks like any unchanged file. An estimate at this pin, by counting the source tree: core's
//! `io_slice` adds three files (6.5 KB; `repr_iovec.rs` is the one a Unix host reads); the
//! vendored libc's `cfg_if!` modules name at most all of its `src/`, 442 files and 4.5 MB,
//! against a few dozen one target reads; std's `sys/` and `os/` arms at most 555 files and
//! 3.2 MB. Imaging is O(bytes), once per content per store.
//!
//! Cost: O(declarations x depth of inline modules), no token read. Floor: the same.

#![blaze::sans_io]

use core::alloc::Allocator;

use cfg::CfgOptions;

use crate::{
    cfgs,
    read::{Image, OutOfPool},
    rows::{CALLED, DEFINES, IN_CALL, IN_MACRO, INLINE, ModRow, NONE},
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
    /// Spelled in a macro call's tokens or a macro definition's body ("Modules a macro call
    /// writes"): every candidate that exists is the unit's, and none existing is no error.
    pub in_call: bool,
}

/// The module path the gather gives the file at `path` that a declaration names as `module`,
/// given the files it already has (`(path, module)` each, in its order): `None` when that file
/// is there under that module path already (two arms of one call naming one file: one file,
/// once), else `module`, or [`module_twice`]'s when earlier files of other paths have it. The
/// command's gather and the std test both take it, so they name files alike.
///
/// Cost: O(files) per declaration; a unit's gather is O(files^2) comparisons of module paths
/// (std: ~10^6 short compares). Floor: a map by module path, O(files); worth it past ~10^4 files.
pub fn gathered_module<'f, A: Allocator>(
    path: &[u8],
    module: &[u8],
    files: impl Iterator<Item = (&'f [u8], &'f [u8])>,
    alloc: A,
) -> Option<Vec<u8, A>> {
    let mut k = 0u32;
    for (there, named) in files {
        if unsuffixed(named) != module {
            continue;
        }
        if there == path {
            return None;
        }
        k += 1;
    }
    match k {
        0 => {
            let mut out = Vec::with_capacity_in(module.len(), alloc);
            out.extend_from_slice(module);
            Some(out)
        }
        k => Some(module_twice(module, k, alloc)),
    }
}

/// `segment` without a [`module_twice`] suffix `{k}`; itself when it has none.
pub fn unsuffixed(segment: &[u8]) -> &[u8] {
    let Some(body) = segment.strip_suffix(b"}") else { return segment };
    match body.iter().rposition(|&byte| byte == b'{') {
        Some(open) if open + 1 < body.len() && body[open + 1..].iter().all(u8::is_ascii_digit) => &segment[..open],
        _ => segment,
    }
}

/// The module path a file gets when an earlier file of the unit already has `module`: the
/// `k`-th file after the first under one module path (two arms of one call naming two files
/// as `repr`) is `module{k}`. No module path as written has `{` in a segment, and an image's
/// in-file path holds `{` only at a braced segment's start, so the suffixed path names no
/// other file's items. The first keeps its path, so a crate with no such file names every file
/// by its module path as written. `k` counts in the gather's order, which is the files' text
/// order (deterministic, rule 25).
pub fn module_twice<A: Allocator>(module: &[u8], k: u32, alloc: A) -> Vec<u8, A> {
    let mut out = Vec::with_capacity_in(module.len() + 4, alloc);
    out.extend_from_slice(module);
    out.push(b'{');
    let mut digits = [0u8; 10];
    let mut len = 0;
    let mut rest = k;
    loop {
        digits[len] = b'0' + (rest % 10) as u8;
        len += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    digits[..len].reverse();
    out.extend_from_slice(&digits[..len]);
    out.push(b'}');
    out
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
    let mut inline: Vec<Step<'_, '_>, A> = Vec::new_in(frame.clone());
    for (decl, row) in image.mods().iter().enumerate() {
        // A module a macro definition's body spells is declared at the macro's calls.
        if row.flags & (INLINE | IN_MACRO) != 0 || !image.active(row.row, options).map_err(ModError::Pool)? {
            continue;
        }
        inline.clear();
        chain(image, row.parent, &mut inline)?;
        declared(image, decl as u32, row, &inline, file, kind, module, options, frame.clone(), &mut out)?;
    }
    Ok(out)
}

/// One call in the unit of a macro the unit defines whose body spells a `mod`: the calling
/// file and its [`MacroRow`] index, the defining file and its [`MacroRow`] index (files by
/// their index in the caller's file list).
///
/// [`MacroRow`]: crate::rows::MacroRow
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroSite {
    pub call_file: u32,
    pub called: u32,
    pub def_file: u32,
    pub defined: u32,
}

/// The sites in `images` (the unit's files so far, in its file order) not in `done`: each
/// active call matched by name to each active definition with a module in its body. The new
/// ones are appended to `done` and returned, in (call file, call, definition file, definition)
/// order, so a gather that calls this each time its files grow, until it returns none, sees
/// every site once (the module doc's fixpoint).
///
/// Cost: O(macro rows + definitions x their file's mod rows) to find the definitions, then
/// O(calls x definitions) name compares and O(sites) per `done` lookup. libc at this pin: one
/// definition with a module (`prelude`), five calls. Floor: a map by name, O(calls); worth it
/// past ~10^3 definitions with modules.
pub fn macro_sites<A: Allocator + Clone>(
    images: &[Image<'_>],
    options: &CfgOptions,
    done: &mut Vec<MacroSite, A>,
    frame: A,
) -> Result<Vec<MacroSite, A>, ModError> {
    let mut defs: Vec<(u32, u32, &[u8]), A> = Vec::new_in(frame.clone());
    for (file, image) in images.iter().enumerate() {
        for (at, row) in image.macros().iter().enumerate() {
            if row.flags & DEFINES == 0 {
                continue;
            }
            let spells = image.mods().iter().any(|it| it.flags & IN_MACRO != 0 && it.row == row.row);
            if !spells || !image.active(row.row, options).map_err(ModError::Pool)? {
                continue;
            }
            defs.push((file as u32, at as u32, image.str(row.name).map_err(ModError::Pool)?));
        }
    }
    let mut out = Vec::new_in(frame);
    if defs.is_empty() {
        return Ok(out);
    }
    for (file, image) in images.iter().enumerate() {
        for (at, row) in image.macros().iter().enumerate() {
            if row.flags & CALLED == 0 {
                continue;
            }
            let name = image.str(row.name).map_err(ModError::Pool)?;
            // Decided once per call, and only for a call some definition's name matches.
            let mut active = None;
            for &(def_file, defined, def_name) in defs.iter() {
                if def_name != name {
                    continue;
                }
                let on = match active {
                    Some(on) => on,
                    None => *active.insert(image.active(row.row, options).map_err(ModError::Pool)?),
                };
                if !on {
                    break;
                }
                let site = MacroSite { call_file: file as u32, called: at as u32, def_file, defined };
                if !done.contains(&site) {
                    done.push(site);
                    out.push(site);
                }
            }
        }
    }
    Ok(out)
}

/// The modules a macro definition's body spells, declared at one call of it in the unit
/// (`IN_MACRO`; the module doc's "Modules a macro call writes"). `def` and `defined`: the
/// definition's file image and its `MacroRow` index; `call` and `called`: the calling file's
/// image and the call's index; `file`, `kind`, `module`: the calling file's. Each [`ModFile`]'s
/// `decl` is the mod row in `def`, and every one is `in_call`. The caller decides that the call
/// and the definition are active and that their names agree.
///
/// Cost: O(the definition's mod rows x depth of inline modules). Floor: the same.
#[allow(clippy::too_many_arguments)]
pub fn macro_files<A: Allocator + Clone>(
    def: &Image<'_>,
    defined: u32,
    call: &Image<'_>,
    called: u32,
    file: &[u8],
    kind: FileKind,
    module: &[u8],
    options: &CfgOptions,
    frame: A,
) -> Result<Vec<ModFile<A>, A>, ModError> {
    let pool = |pool, at| ModError::Pool(OutOfPool { pool, at });
    let definition = def.macros().get(defined as usize).ok_or(pool("the macro table", defined))?;
    let site = call.macros().get(called as usize).ok_or(pool("the macro table", called))?;
    let mut out = Vec::new_in(frame.clone());
    let mut inline: Vec<Step<'_, '_>, A> = Vec::new_in(frame.clone());
    for (decl, row) in def.mods().iter().enumerate() {
        if row.flags & IN_MACRO == 0 || row.flags & INLINE != 0 || row.row != definition.row {
            continue;
        }
        // The call site's inline modules, then the body's around the declaration.
        inline.clear();
        chain(call, site.parent, &mut inline)?;
        chain(def, row.parent, &mut inline)?;
        declared(def, decl as u32, row, &inline, file, kind, module, options, frame.clone(), &mut out)?;
    }
    Ok(out)
}

/// One inline module around a declaration: its image and its mod row there.
#[derive(Clone, Copy)]
struct Step<'i, 'b> {
    image: &'i Image<'b>,
    at: u32,
}

/// The inline modules from mod row `up` of `image` out to its file's top level, appended to
/// `out` outermost first.
fn chain<'i, 'b, A: Allocator>(image: &'i Image<'b>, mut up: u32, out: &mut Vec<Step<'i, 'b>, A>) -> Result<(), ModError> {
    let start = out.len();
    let mods = image.mods();
    while up != NONE {
        out.push(Step { image, at: up });
        let parent = mods.get(up as usize).ok_or(ModError::Pool(OutOfPool { pool: "the mod table", at: up }))?;
        if parent.parent != NONE && parent.parent >= up {
            return Err(ModError::Pool(OutOfPool { pool: "a parent mod after its child", at: up }));
        }
        up = parent.parent;
    }
    out[start..].reverse();
    Ok(())
}

/// The [`ModFile`]s of `mod name;` at mod row `decl` of `image` (`row`), under the inline
/// modules `inline`, declared in the file `file` (`kind`, module path `module`): pushed to
/// `out`. A module spelled in tokens ([`IN_CALL`], [`IN_MACRO`]) names its name's two files
/// and each `#[path]`'s; any other the one its held `#[path]` or its name names.
#[allow(clippy::too_many_arguments)]
fn declared<A: Allocator + Clone>(
    image: &Image<'_>,
    decl: u32,
    row: &ModRow,
    inline: &[Step<'_, '_>],
    file: &[u8],
    kind: FileKind,
    module: &[u8],
    options: &CfgOptions,
    frame: A,
    out: &mut Vec<ModFile<A>, A>,
) -> Result<(), ModError> {
    let own_dir = parent_dir(file);
    let in_call = row.flags & (IN_CALL | IN_MACRO) != 0;
    let name = image.str(row.name).map_err(ModError::Pool)?;
    let step_row = |step: &Step<'_, '_>| -> Result<ModRow, ModError> {
        step.image.mods().get(step.at as usize).copied().ok_or(ModError::Pool(OutOfPool { pool: "the mod table", at: step.at }))
    };
    let mut child_module = Vec::new_in(frame.clone());
    child_module.extend_from_slice(module);
    for step in inline {
        push_segment(&mut child_module, step.image.str(step_row(step)?.name).map_err(ModError::Pool)?);
    }
    push_segment(&mut child_module, name);
    // The directory of the modules declared where the declaration is: the file's own, or its
    // module directory and the inline modules' steps.
    let mut nested_dir = Vec::new_in(frame.clone());
    nested_dir.extend_from_slice(own_dir);
    if kind == FileKind::NonModRs {
        join(&mut nested_dir, stem(file));
    }
    for step in inline {
        let parent = step_row(step)?;
        match inline_path(step.image, step.at, &parent, options)? {
            Some(path) => join(&mut nested_dir, path),
            None => join(&mut nested_dir, step.image.str(parent.name).map_err(ModError::Pool)?),
        }
    }
    // A `#[path]` at the top level is relative to the file's own directory.
    let path_dir: &[u8] = match inline.is_empty() {
        true => own_dir,
        false => &nested_dir,
    };
    let by_path = |path: &[u8]| -> ModFile<A> {
        let mut full = Vec::new_in(frame.clone());
        match path.first() {
            Some(b'/') => full.extend_from_slice(path),
            _ => {
                full.extend_from_slice(path_dir);
                join(&mut full, path);
            }
        }
        let mut module = Vec::with_capacity_in(child_module.len(), frame.clone());
        module.extend_from_slice(&child_module);
        ModFile { decl, module, first: Candidate { path: normal(&full, frame.clone()), kind: FileKind::ModRs }, second: None, in_call }
    };
    let by_name = || -> ModFile<A> {
        let mut flat = nested_dir.clone();
        join(&mut flat, name);
        flat.extend_from_slice(b".rs");
        let mut nested = nested_dir.clone();
        join(&mut nested, name);
        join(&mut nested, b"mod.rs");
        let mut module = Vec::with_capacity_in(child_module.len(), frame.clone());
        module.extend_from_slice(&child_module);
        ModFile {
            decl,
            module,
            first: Candidate { path: normal(&flat, frame.clone()), kind: FileKind::NonModRs },
            second: Some(Candidate { path: normal(&nested, frame.clone()), kind: FileKind::ModRs }),
            in_call,
        }
    };
    if in_call {
        // Every file the tokens could name: its name's two, and each `#[path]`'s.
        out.push(by_name());
        for path_row in image.paths(row.paths).map_err(ModError::Pool)? {
            out.push(by_path(image.str(path_row.path).map_err(ModError::Pool)?));
        }
        return Ok(());
    }
    match held_path(image, decl, row.paths, options)? {
        Some(path) => out.push(by_path(path)),
        None => out.push(by_name()),
    }
    Ok(())
}

/// The directory step `#[path]` of inline mod row `at`, if any: the one that holds, or for an
/// inline module in a macro call's tokens or a definition's body its one `#[path]` (two or
/// more: none, its name's directory; the module doc says why that is sound).
fn inline_path<'b>(image: &Image<'b>, at: u32, row: &ModRow, options: &CfgOptions) -> Result<Option<&'b [u8]>, ModError> {
    if row.flags & (IN_CALL | IN_MACRO) == 0 {
        return held_path(image, at, row.paths, options);
    }
    let paths = image.paths(row.paths).map_err(ModError::Pool)?;
    match paths {
        [one] => Ok(Some(image.str(one.path).map_err(ModError::Pool)?)),
        _ => Ok(None),
    }
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

/// `path` lexically normal: no `.` part, no empty part, and a `..` only where nothing before
/// it can be taken back (at the start of a relative path; above `/` it is dropped). So one file
/// reached by two spellings (`unix/weak/./tests.rs`, `core/src/../../stdarch/..`) is one path,
/// which the gather compares and names (the crate doc's "How a file is named").
pub fn normal<A: Allocator>(path: &[u8], frame: A) -> Vec<u8, A> {
    let absolute = path.first() == Some(&b'/');
    let mut out = Vec::with_capacity_in(path.len(), frame);
    // The length of `out` before each part kept, so a `..` takes its part back.
    let mut kept = 0usize;
    for part in path.split(|&byte| byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." if kept > 0 => {
                let cut = out.iter().rposition(|&byte| byte == b'/').unwrap_or(0);
                out.truncate(cut);
                kept -= 1;
            }
            b".." if absolute => {}
            _ => {
                if !out.is_empty() || absolute {
                    out.push(b'/');
                }
                out.extend_from_slice(part);
                if part != b".." {
                    kept += 1;
                }
            }
        }
    }
    if out.is_empty() && absolute {
        out.push(b'/');
    }
    out
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
