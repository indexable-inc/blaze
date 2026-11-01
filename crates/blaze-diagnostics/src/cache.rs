//! A dependency's check computed once for everyone (docs/v2/plan/lanes/frontend/fast-bodies-cache.md;
//! compile-time.md idea 8): what [`crate::check_crate`] gives for a crate read from a dependency
//! image ([`hir_def::deps`]), and per body what codegen reads of the check
//! ([`hir_ty::check_cache`]), kept in the image's store and read back by every later build, on
//! this machine or, through the global cache, on another.
//!
//! - The crate's check key: [`bodies_key`] of the key its image is filed under
//!   ([`hir_def::deps::Image::key`]: the crate's content, files, dependencies' image keys,
//!   toolchain, the blaze reading, and the values of what its module scopes and signatures read),
//!   its dependencies' check keys (a body evaluates their constants and `const fn`s, whose bodies
//!   the image keys do not cover) and the borrow checker's mode; filed by what the macro calls in
//!   its bodies read (an `env!` of a process variable, a proc macro reading a file) as an image is
//!   by its reads: the names in a slot under it, the part under the key their values make
//!   (`hir_def::deps::file_by_reads`), looked up with their values now
//!   (`hir_def::deps::find_by_reads`), so a changed value is a miss. The bodies part is under it,
//!   and `hir_ty::consteval::deps` files a crate's constants by it.
//! - [`key`]: the outcome's, the check key and what the check reads beyond it ([`Inputs`]): the
//!   lint flags rustc is given, `keep_duplicates` and the borrow checker's mode.
//! - [`serve`], once a session's images are served: each crate's bodies part and outcome read
//!   from the store in parallel, a crate after its dependencies; the bodies given to the
//!   database, the check keys and outcomes kept for the units ([`Served`]).
//! - [`read`]: a unit's outcome, its paths named as its build names them.
//! - [`store`]: the bodies and the outcomes of the crates checked this session, a crate after its
//!   dependencies.
//!
//! An outcome is the check's `Diag`s before rendering, so colour, `--short` and the unit's name
//! are applied at replay exactly as after a check. It is written portably: the crate's package
//! directory and `OUT_DIR` are markers wherever they are in a path or a text, and a span's file
//! is written whole, then named from the reader's workspace root as [`Files::relative`] names it.
//! An outcome holding any other path of the machine is refused ([`Refused::MachinePath`]), as is
//! the outcome of a crate read from no image ([`Refused::NoImage`]): frontend-deps refused the
//! image, for a reason `hir_def::deps::Stored::refused` gives, and the same reason holds here.

use std::sync::{Arc, LazyLock, OnceLock};

use base_db::{Crate, Db};
use blaze_format::cursor::{self, Put as _, Reader};
use blaze_ids::{ContentId, Hasher};
use hir_def::deps::{Held, ImageBuf, ImageStore, absolute, crate_name};
use hir_ty_borrowck::Mode;
use blaze_hash::{HashMap, HashSet};

use crate::{
    Files, Options,
    model::{Diag, MultiSpan, Span},
};

#[cfg(test)]
mod tests;

/// The domain of an outcome's key and of its encoding's version.
pub const DOMAIN: &str = "blaze v2 dependency check outcome 1";

/// The first bytes of an outcome: its encoding's name and version.
const MAGIC: [u8; 4] = *b"BZC1";

/// The header: [`MAGIC`], the flags (bit 0: the check failed), the count of diagnostics and the
/// length of the body, each a `u32` LE.
const HEADER: usize = 16;

const FAILED: u32 = 1;

/// What stands for the package directory and `OUT_DIR` in an outcome. No path or text the check
/// writes holds a NUL ([`Refused::Nul`]).
const DIR: &str = "\0PKG";
const OUT: &str = "\0OUT_DIR";

/// The shortest workspace root looked for in an outcome's texts: a shorter one (`/w` in a test)
/// is a few bytes any text holds by chance.
const SHORTEST_ROOT: usize = 8;

/// The version of the outcome's encoding: an outcome of another is never found. The check's own
/// code is in the image's key (`build`, the blaze that reads).
pub static PRODUCER: LazyLock<ContentId> = LazyLock::new(|| {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"producer").part(&MAGIC).part(DIR.as_bytes()).part(OUT.as_bytes());
    hasher.finish()
});

/// What a crate's check reads beyond its image: the outcome's key holds each.
#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    /// The lint flags rustc is given, in order: cargo's `[lints]`, those of `RUSTFLAGS`,
    /// `--check-cfg` and `--cap-lints` (`blaze_cargo`'s `lint_flags`), as [`Options::lints`] was
    /// parsed from.
    pub lint_flags: &'a [String],
    /// [`Options::keep_duplicates`].
    pub keep_duplicates: bool,
    /// The borrow checker's mode ([`hir_ty_borrowck::mode`], which `hir-ty-wfcheck` reads too).
    pub borrowck: Mode,
}

impl<'a> Inputs<'a> {
    /// The inputs of a check run with `options`, parsed from `lint_flags`, in this process.
    pub fn of(lint_flags: &'a [String], options: &Options) -> Inputs<'a> {
        Inputs { lint_flags, keep_duplicates: options.keep_duplicates, borrowck: hir_ty_borrowck::mode() }
    }
}

fn mode_byte(mode: Mode) -> u8 {
    match mode {
        Mode::Nll => 0,
        Mode::Polonius => 1,
        Mode::Off => 2,
    }
}

/// The key of the outcome of the crate whose check key is `check` (the key its bodies part is
/// filed under, [`bodies_key`] and its bodies' reads), checked with `inputs`. The global cache's
/// rule makes it again with this function.
pub fn key(check: ContentId, inputs: &Inputs<'_>) -> ContentId {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"key").id(&PRODUCER).id(&check);
    hasher.update(&(inputs.lint_flags.len() as u64).to_le_bytes());
    for flag in inputs.lint_flags {
        hasher.part(flag.as_bytes());
    }
    hasher.update(&[u8::from(inputs.keep_duplicates), mode_byte(inputs.borrowck)]);
    hasher.finish()
}

/// The key of the bodies part of the crate whose image is filed under `image`, whose
/// dependencies' check keys are `deps` (in its dependency list's order), checked with the borrow
/// checker's mode `borrowck`, before its bodies' reads: the part is under it, or under one of its
/// slots' filed keys, which is the crate's check key. The image's key covers its dependencies'
/// image keys, not what their bodies read, and a body evaluates its dependencies' constants and
/// `const fn`s: their check keys are in it. The lint flags are not: they change no record.
pub fn bodies_key(image: ContentId, deps: &[ContentId], borrowck: Mode) -> ContentId {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"bodies").id(&image);
    hasher.update(&(deps.len() as u64).to_le_bytes());
    for dep in deps {
        hasher.id(dep);
    }
    hir_ty::check_cache::key(hasher.finish(), mode_byte(borrowck))
}

/// The key the image `krate` is read from is filed under, if it is read from one.
pub fn image_key(db: &Db, krate: Crate) -> Option<ContentId> {
    hir_def::deps::image_of(db, krate).as_ref().map(|it| it.key())
}

/// A crate's check: its diagnostics in the order the check gives them, and whether it failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub failed: bool,
    pub diags: Vec<Diag>,
}

impl Outcome {
    pub fn new(diags: Vec<Diag>) -> Outcome {
        Outcome { failed: diags.iter().any(Diag::is_error), diags }
    }
}

/// Why an outcome is not kept: each an input its key does not cover, so it would read otherwise
/// in another checkout or session with the same key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The crate is read from no image, so its outcome has no key: frontend-deps refused the
    /// image (`hir_def::deps::Stored::refused` says why), or the crate is a workspace member.
    NoImage,
    /// A path the outcome holds is neither below the crate's package directory nor its
    /// `OUT_DIR` (another crate's file in a macro's definition, the workspace root in a text).
    MachinePath { path: String },
    /// A diagnostic points into a file the front end has no path for.
    NoPath { file: String },
    /// A text holds a NUL, which the markers are written with.
    Nul,
    /// The diagnostics do not encode.
    Encode { why: String },
    /// What the crate's bodies read is not known or cannot be filed
    /// (`hir_def::deps::checked_reads`, `hir_def::deps::file_by_reads`).
    Reads(hir_def::deps::Refused),
    /// A dependency, `name`, has no check key, which the crate's is made from.
    Dependency { name: String },
    /// Its bodies part does not encode (`hir_ty::check_cache::encode`), so it has no check key.
    Bodies { why: String },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NoImage => f.write_str("it is read from no dependency image, whose key its outcome is filed by"),
            Refused::MachinePath { path } => {
                write!(f, "its diagnostics hold the path {path}, outside its package and `OUT_DIR`")
            }
            Refused::NoPath { file } => write!(f, "a diagnostic of it is in {file}, which has no path"),
            Refused::Nul => f.write_str("a diagnostic of it holds a NUL"),
            Refused::Encode { why } => write!(f, "its diagnostics do not encode: {why}"),
            Refused::Reads(why) => write!(f, "what its bodies read: {why}"),
            Refused::Dependency { name } => write!(f, "its dependency {name} has no check key"),
            Refused::Bodies { why } => write!(f, "its bodies do not encode: {why}"),
        }
    }
}

/// The paths an outcome writes as markers: the crate's package directory and its `OUT_DIR`.
pub struct Paths {
    dir: Option<String>,
    out: Option<String>,
}

impl Paths {
    pub fn of(db: &Db, krate: Crate) -> Paths {
        let env = krate.env(db);
        Paths::new(env.get("CARGO_MANIFEST_DIR"), env.get("OUT_DIR"))
    }

    pub fn new(dir: Option<String>, out: Option<String>) -> Paths {
        let trimmed = |it: String| {
            let trimmed = it.trim_end_matches(['/', '\\']);
            // alloc: setup: once per crate whose outcome is written or read with diagnostics
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        };
        Paths { dir: dir.and_then(trimmed), out: out.and_then(trimmed) }
    }

    /// Each path and its marker, the longest path first so a path below the other is written
    /// whole.
    fn markers(&self) -> impl Iterator<Item = (&str, &'static str)> {
        let mut found = [(self.out.as_deref(), OUT), (self.dir.as_deref(), DIR)];
        found.sort_by_key(|(path, _)| std::cmp::Reverse(path.map_or(0, str::len)));
        found.into_iter().filter_map(|(path, marker)| Some((path?, marker)))
    }

    fn path_of(&self, marker: &str) -> Option<&str> {
        match marker {
            DIR => self.dir.as_deref(),
            _ => self.out.as_deref(),
        }
    }

    /// `file`, a span's file as [`Files::name`] gave it from `root`, written whole and from its
    /// marker.
    fn portable_file(&self, file: &mut String, root: &str) -> Result<(), Refused> {
        if file.starts_with('<') {
            // clone: why an outcome is refused, once per refused crate
            return Err(Refused::NoPath { file: file.clone() });
        }
        if file.contains('\0') {
            return Err(Refused::Nul);
        }
        let root = root.trim_end_matches('/');
        let whole = match absolute(file) || root.is_empty() {
            // clone: a path of an outcome being written, once per span per crate kept
            true => file.clone(),
            // alloc: as above
            false => format!("{root}/{file}"),
        };
        for (path, marker) in self.markers() {
            if let Some(rest) = whole.strip_prefix(path)
                && (rest.starts_with('/') || rest.starts_with('\\'))
            {
                // alloc: as above
                *file = format!("{marker}{rest}");
                return Ok(());
            }
        }
        Err(Refused::MachinePath { path: whole })
    }

    /// `text` with each of the paths in it written as its marker; a path that is the start of a
    /// longer name (`/reg/serde` of `/reg/serde_derive`) is another path, which is refused, as is
    /// the workspace root `root`.
    fn portable_text(&self, text: &mut String, root: &str) -> Result<(), Refused> {
        if text.contains('\0') {
            return Err(Refused::Nul);
        }
        for (path, marker) in self.markers() {
            if text.contains(path) {
                *text = replaced(text, path, marker)?;
            }
        }
        let root = root.trim_end_matches('/');
        if root.len() >= SHORTEST_ROOT && text.contains(root) {
            // alloc: why an outcome is refused, once per refused crate
            return Err(Refused::MachinePath { path: root.to_owned() });
        }
        Ok(())
    }

    /// A span's file written by [`Paths::portable_file`], named from `root` as this checkout's
    /// check would name it.
    fn local_file(&self, file: &mut String, root: &str) -> Result<(), String> {
        for marker in [DIR, OUT] {
            if let Some(rest) = file.strip_prefix(marker) {
                let path = self.local_path(marker)?;
                // alloc: a path of an outcome read with diagnostics, once per span
                let whole = format!("{path}{rest}");
                // alloc: the span's file name, kept in the diagnostic
                *file = Files::relative(&whole, root).to_owned();
                return Ok(());
            }
        }
        // alloc: error text
        Err(format!("a span in {file:?}, which is below neither marker"))
    }

    fn local_text(&self, text: &mut String) -> Result<(), String> {
        for marker in [DIR, OUT] {
            if text.contains(marker) {
                *text = text.replace(marker, self.local_path(marker)?);
            }
        }
        match text.contains('\0') {
            // alloc: error text
            true => Err("a text holding a NUL that is no marker".to_owned()),
            false => Ok(()),
        }
    }

    fn local_path(&self, marker: &str) -> Result<&str, String> {
        self.path_of(marker).ok_or_else(|| {
            let what = if marker == DIR { "package directory" } else { "`OUT_DIR`" };
            // alloc: error text
            format!("the outcome names the crate's {what}, which this build gives it none of")
        })
    }
}

/// `text` with each `path` in it that ends a name written as `marker`; `Err` when one is the
/// start of a longer name.
fn replaced(text: &str, path: &str, marker: &str) -> Result<String, Refused> {
    // alloc: a text of an outcome being written, once per text holding a path
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(path) {
        let after = &rest[at + path.len()..];
        let ends = after.chars().next().is_none_or(|it| !(it.is_alphanumeric() || matches!(it, '-' | '_' | '.')));
        if !ends {
            let end = after.find(|it: char| it.is_whitespace() || matches!(it, '`' | '\'' | '"')).unwrap_or(after.len());
            // alloc: why an outcome is refused, once per refused crate
            return Err(Refused::MachinePath { path: format!("{path}{}", &after[..end]) });
        }
        out.push_str(&rest[..at]);
        out.push_str(marker);
        rest = after;
    }
    out.push_str(rest);
    Ok(out)
}

/// What a string of a diagnostic is.
#[derive(Clone, Copy)]
enum Field {
    /// A span's file.
    File,
    /// Any other text: a message, a label, a suggestion's code, a macro's name.
    Text,
}

/// Calls `f` on every string of `diag` that can hold a path, in one order.
fn walk<E>(diag: &mut Diag, f: &mut impl FnMut(Field, &mut String) -> Result<(), E>) -> Result<(), E> {
    f(Field::Text, &mut diag.message)?;
    walk_multi(&mut diag.span, f)?;
    for child in &mut diag.children {
        f(Field::Text, &mut child.message)?;
        walk_multi(&mut child.span, f)?;
    }
    for suggestion in &mut diag.suggestions {
        f(Field::Text, &mut suggestion.message)?;
        for parts in &mut suggestion.substitutions {
            for (span, code) in parts {
                walk_span(span, f)?;
                f(Field::Text, code)?;
            }
        }
    }
    for read in &mut diag.facts {
        f(Field::Text, &mut read.what)?;
    }
    Ok(())
}

fn walk_multi<E>(spans: &mut MultiSpan, f: &mut impl FnMut(Field, &mut String) -> Result<(), E>) -> Result<(), E> {
    for span in &mut spans.primary {
        walk_span(span, f)?;
    }
    for (span, label) in &mut spans.labels {
        walk_span(span, f)?;
        f(Field::Text, label)?;
    }
    Ok(())
}

fn walk_span<E>(span: &mut Span, f: &mut impl FnMut(Field, &mut String) -> Result<(), E>) -> Result<(), E> {
    f(Field::File, &mut span.file)?;
    if let Some(expansion) = &mut span.expansion {
        walk_span(&mut expansion.call_site, f)?;
        f(Field::Text, &mut expansion.name)?;
        if let Some(def_site) = &mut expansion.def_site {
            walk_span(def_site, f)?;
        }
    }
    Ok(())
}

/// The outcome `diags` of a crate whose paths are `paths`, its spans' files named from `root`
/// ([`Files`]'s root), as the store keeps it.
pub fn encode(diags: &[Diag], paths: &Paths, root: &str) -> Result<Vec<u8>, Refused> {
    // clone: an outcome is written once per crate checked, its strings rewritten in a copy
    let mut portable: Vec<Diag> = diags.to_vec();
    for diag in &mut portable {
        walk(diag, &mut |field, text| match field {
            Field::File => paths.portable_file(text, root),
            Field::Text => paths.portable_text(text, root),
        })?;
    }
    // alloc: the outcome's bytes, once per crate kept
    let body = serde_json::to_vec(&portable).map_err(|why| Refused::Encode { why: why.to_string() })?;
    // alloc: error text
    let too_big = || Refused::Encode { why: "an outcome of 4 GiB or more".to_owned() };
    let count = u32::try_from(diags.len()).map_err(|_| too_big())?;
    let len = u32::try_from(body.len()).map_err(|_| too_big())?;
    let flags = if diags.iter().any(Diag::is_error) { FAILED } else { 0 };
    // alloc: as above
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.put_bytes(&MAGIC);
    out.put_u32(flags);
    out.put_u32(count);
    out.put_u32(len);
    out.put_bytes(&body);
    Ok(out)
}

/// The header of `bytes`: whether the check failed and how many diagnostics follow, its length
/// checked against the bytes.
fn header(bytes: &[u8]) -> Result<(bool, u32), &'static str> {
    let short = |_: cursor::Error| "shorter than its header";
    let mut read = Reader::new(bytes);
    if read.left() < HEADER {
        return Err("shorter than its header");
    }
    if *read.array::<4>().map_err(short)? != MAGIC {
        return Err("not an outcome of this encoding");
    }
    let flags = read.u32().map_err(short)?;
    if flags & !FAILED != 0 {
        return Err("flags it does not know");
    }
    let (count, len) = (read.u32().map_err(short)?, read.u32().map_err(short)?);
    if len as usize != read.left() {
        return Err("a body whose length is not its header's");
    }
    // Each diagnostic is a JSON object, more than one byte.
    if count > len {
        return Err("more diagnostics than bytes");
    }
    Ok((flags & FAILED != 0, count))
}

/// The outcome in `bytes`, [`encode`]'s, its paths those `paths` gives (asked only when it has
/// diagnostics), its spans' files named from `root`. Checked as it is read: the store checked
/// only the bytes' hash.
pub fn decode(bytes: &[u8], paths: impl FnOnce() -> Paths, root: &str) -> Result<Outcome, String> {
    // alloc: error text
    let (failed, count) = header(bytes).map_err(str::to_owned)?;
    if count == 0 {
        if failed {
            // alloc: error text
            return Err("a failed outcome with no diagnostics".to_owned());
        }
        if &bytes[HEADER..] != b"[]" {
            // alloc: error text
            return Err("an outcome of no diagnostics whose body is not empty".to_owned());
        }
        // alloc: none, an empty `Vec` holds no buffer
        return Ok(Outcome { failed, diags: Vec::new() });
    }
    // alloc: an outcome with diagnostics, read once per crate per build
    let mut diags: Vec<Diag> = serde_json::from_slice(&bytes[HEADER..]).map_err(|why| format!("its body: {why}"))?;
    if diags.len() != count as usize {
        // alloc: error text
        return Err(format!("{} diagnostics where its header says {count}", diags.len()));
    }
    if diags.iter().any(Diag::is_error) != failed {
        // alloc: error text
        return Err("a failed flag that is not its diagnostics'".to_owned());
    }
    let paths = paths();
    for diag in &mut diags {
        walk(diag, &mut |field, text| match field {
            Field::File => paths.local_file(text, root),
            Field::Text => paths.local_text(text),
        })?;
    }
    Ok(Outcome { failed, diags })
}

/// What [`serve`] found, over every call of a session: each adds to it.
#[derive(Default)]
pub struct Served {
    /// The check key ([`bodies_key`] filed by its bodies' reads) of each crate asked whose
    /// bodies part was found: what its dependents' keys and its constants' values
    /// (`hir_ty::consteval::deps`) are made from. Its bodies were given to the database.
    pub keys: HashMap<Crate, ContentId>,
    outcomes: HashMap<Crate, ImageBuf>,
    /// The bodies part of each crate of `keys`: what the database was given.
    parts: HashMap<Crate, ImageBuf>,
    /// What each crate asked was looked up under: its base key ([`bodies_key`]) and its outcome's
    /// key, so asking it again under the same is no lookup.
    asked: HashMap<Crate, (ContentId, Option<ContentId>)>,
}

impl Served {
    /// Whether `krate`'s outcome was found: a unit of it runs no check.
    pub fn has(&self, krate: Crate) -> bool {
        self.outcomes.contains_key(&krate)
    }
}

/// `crates` in layers, each crate in a later layer than every one of them it depends on; a
/// dependency in `known` is in none. A crate depending on a crate in neither is left out, with
/// that dependency (and so is every crate depending on it).
fn layers(
    db: &Db,
    crates: impl Iterator<Item = Crate>,
    known: &HashMap<Crate, ContentId>,
) -> (Vec<Vec<Crate>>, Vec<(Crate, Crate)>) {
    fn depth(
        db: &Db,
        krate: Crate,
        asked: &HashSet<Crate>,
        memo: &mut HashMap<Crate, Result<usize, Crate>>,
        known: &HashMap<Crate, ContentId>,
    ) -> Result<usize, Crate> {
        if let Some(&it) = memo.get(&krate) {
            return it;
        }
        let mut at = 0;
        let mut missing = None;
        for dep in &krate.data(db).dependencies {
            let dep = dep.crate_id;
            if !asked.contains(&dep) {
                if !known.contains_key(&dep) {
                    missing = Some(dep);
                    break;
                }
                continue;
            }
            match depth(db, dep, asked, memo, known) {
                Ok(it) => at = at.max(it + 1),
                Err(it) => {
                    missing = Some(it);
                    break;
                }
            }
        }
        let it = missing.map_or(Ok(at), Err);
        memo.insert(krate, it);
        it
    }
    // alloc: setup: once per session
    let asked: HashSet<Crate> = crates.collect();
    let mut memo = HashMap::default();
    // alloc: setup: once per session
    let mut layers: Vec<Vec<Crate>> = Vec::new();
    // alloc: setup: once per session
    let mut left = Vec::new();
    for &krate in &asked {
        match depth(db, krate, &asked, &mut memo, known) {
            Ok(at) => {
                if layers.len() <= at {
                    layers.resize_with(at + 1, Vec::new);
                }
                layers[at].push(krate);
            }
            Err(dep) => left.push((krate, dep)),
        }
    }
    for layer in &mut layers {
        layer.sort_unstable();
    }
    left.sort_unstable();
    (layers, left)
}

/// The check keys of `krate`'s dependencies, in its dependency list's order, `None` when one has
/// none.
fn dep_keys(db: &Db, krate: Crate, keys: &HashMap<Crate, ContentId>) -> Option<Vec<ContentId>> {
    // alloc: once per crate per session
    krate.data(db).dependencies.iter().map(|dep| keys.get(&dep.crate_id).copied()).collect()
}

/// What one task of [`serve`] found of a crate: its check key, its bodies part and its outcome.
type Fetched = Result<Option<(ContentId, ImageBuf, Option<ImageBuf>)>, String>;

/// Reads from `store` the bodies part and the outcome of each of `crates` that is read from a
/// dependency image (after `hir_def::deps::serve`, with the same store), each crate checked with
/// its [`Inputs`], and adds them to `served`: in layers of the crate graph, a crate after its
/// dependencies, whose check keys its own is made from (asked now, or found by an earlier call),
/// one task a crate of a layer ([`ImageStore::for_each`]). A crate one of whose dependencies has
/// no check key has none. A crate asked again under the keys it was found or missed under is not
/// looked up again; one under other keys is, and loses what it had when nothing is found. The
/// bodies of every crate of `served` are given to the database
/// ([`hir_ty::check_cache::set_served`]) when they changed; the check keys and outcomes are kept
/// in `served`, for [`read`] and [`store`].
///
/// The driver asks a crate once its inputs are final, after its dependencies, as it serves
/// images (`hir_def::deps::serve` a crate at a time), so a crate's dependencies' check keys are
/// final when it is keyed.
///
/// A value under its key that does not open is an error naming the crate and the key, never a
/// crate checked instead: the key covers what the value was made from and the store checked
/// its bytes, so such a value is a bug in the key or the encoding.
pub fn serve(
    db: &mut Db,
    crates: &[(Crate, Inputs<'_>)],
    // dyn: the driver's store, one call per crate per session
    store: &dyn ImageStore,
    served: &mut Served,
) -> Result<(), String> {
    // alloc: setup: once per serve
    let images: HashMap<Crate, (ContentId, Inputs<'_>)> = crates
        .iter()
        .filter_map(|&(krate, inputs)| Some((krate, (image_key(db, krate)?, inputs))))
        // alloc: setup: once per serve
        .collect();
    let (layers, _) = layers(db, images.keys().copied(), &served.keys);
    let mut changed = false;
    for layer in layers {
        // alloc: setup: once per layer per serve
        let mut asked: Vec<(Crate, ContentId)> = Vec::with_capacity(layer.len());
        for krate in layer {
            let (image, inputs) = images[&krate];
            let Some(deps) = dep_keys(db, krate, &served.keys) else { continue };
            let base = bodies_key(image, &deps, inputs.borrowck);
            let outcome_at = served.keys.get(&krate).map(|&check| key(check, &inputs));
            if served.asked.get(&krate) == Some(&(base, outcome_at)) {
                continue;
            }
            asked.push((krate, base));
        }
        // alloc: setup: one slot per crate, once per serve
        let found: Vec<OnceLock<Fetched>> = asked.iter().map(|_| OnceLock::new()).collect();
        {
            let db = &*db;
            store.for_each(asked.len(), &|at| {
                let (krate, base) = asked[at];
                _ = found[at].set(fetch(db, krate, base, &images[&krate].1, store));
            });
        }
        for (&(krate, base), found) in asked.iter().zip(found) {
            let found = found.into_inner().unwrap_or_else(|| {
                panic!("`ImageStore::for_each` did not run the task of crate {}", crate_name(db, krate))
            });
            served.outcomes.remove(&krate);
            let Some((check, part, outcome)) = found? else {
                changed |= served.parts.remove(&krate).is_some();
                served.keys.remove(&krate);
                served.asked.insert(krate, (base, None));
                continue;
            };
            served.keys.insert(krate, check);
            changed |= served.parts.get(&krate).is_none_or(|it| !Arc::ptr_eq(it, &part));
            served.parts.insert(krate, part);
            served.asked.insert(krate, (base, Some(key(check, &images[&krate].1))));
            if let Some(outcome) = outcome {
                served.outcomes.insert(krate, outcome);
            }
        }
    }
    if changed {
        // clone: one handle a crate, once per serve that changed a part
        hir_ty::check_cache::set_served(db, served.parts.clone());
    }
    Ok(())
}

/// The bodies part of `krate` filed by `base` and its bodies' reads as they are now, and the
/// outcome under the check key it is filed by, each opened.
// dyn: the driver's store, one call per crate per session
fn fetch(db: &Db, krate: Crate, base: ContentId, inputs: &Inputs<'_>, store: &dyn ImageStore) -> Fetched {
    let get = |key: ContentId, what: &str| {
        store
            .get(&key)
            // alloc: error text
            .map_err(|why| format!("reading the {what} of crate {} (key {key:?}): {why}", crate_name(db, krate)))
    };
    let found = hir_def::deps::find_by_reads(db, krate, base, store, |at| {
        let Some(bytes) = get(at, "checked bodies")? else { return Ok(None) };
        match hir_ty::check_cache::open(bytes.bytes()) {
            Ok(()) => Ok(Some((at, bytes))),
            // alloc: error text
            Err(why) => Err(format!(
                "the checked bodies of crate {} (key {at:?}) do not read: {why}; the store checked \
                 its bytes, so the key leaves out an input or the encoding wrote what it does not read",
                crate_name(db, krate)
            )),
        }
    })?;
    let Some((check, part)) = found else { return Ok(None) };
    let at = key(check, inputs);
    let outcome = get(at, "check outcome")?;
    if let Some(bytes) = &outcome
        && let Err(why) = header(bytes.bytes())
    {
        // alloc: error text
        return Err(format!(
            "the check outcome of crate {} (key {at:?}) does not read: {why}; the store checked \
             its bytes, so the key leaves out an input or the encoding wrote what it does not read",
            crate_name(db, krate)
        ));
    }
    Ok(Some((check, part, outcome)))
}

/// The outcome of `krate` [`serve`] found, its spans' files named from `root` ([`Files`]'s root
/// of the unit's build); `None` when it found none, and the crate is checked.
pub fn read(db: &Db, served: &Served, krate: Crate, root: &str) -> Option<Result<Outcome, String>> {
    let bytes = served.outcomes.get(&krate)?;
    Some(decode(bytes.bytes(), || Paths::of(db, krate), root).map_err(|why| {
        // alloc: error text
        format!(
            "the check outcome of crate {} (check key {:?}) does not read: {why}",
            crate_name(db, krate),
            served.keys.get(&krate)
        )
    }))
}

/// A crate checked this session, for [`store`].
#[derive(Clone, Copy)]
pub struct Checked<'a> {
    pub krate: Crate,
    /// The key its image is filed under: [`image_key`] of a served crate,
    /// `hir_def::deps::Stored::filed` of one imaged this session.
    pub image: ContentId,
    pub inputs: Inputs<'a>,
    /// What the check gave, its spans' files named from [`store`]'s `root`.
    pub diags: &'a [Diag],
}

/// What [`store`] kept.
#[derive(Debug, Default)]
pub struct Stored {
    /// The check key of each crate whose bodies part was kept, besides those [`Served::keys`]
    /// has: what its constants' values are filed under.
    pub keys: HashMap<Crate, ContentId>,
    /// Each crate with a value not kept, and why: with no key, nothing of it is kept; with one,
    /// its bodies part is, and the reason is its outcome's.
    pub refused: Vec<(Crate, Refused)>,
}

/// What one task of [`store`] kept of a crate: its check key and why its outcome was not kept,
/// or why it has no key.
type Made = Result<Result<(ContentId, Option<Refused>), Refused>, String>;

/// Keeps in `store` the bodies part (each body's record and borrow check verdict) and the
/// outcome of each of `checked`, after its check: in layers of the crate graph, a crate after
/// its dependencies, whose check keys ([`Served::keys`] or kept here) its own is made from, one
/// task a crate of a layer. Each part is filed by what its crate's bodies read as `db` checked
/// them. A crate one of whose dependencies has no check key is refused
/// ([`Refused::Dependency`]), as is one whose reads or bodies do not keep. A store that will not
/// keep a value is an error naming the crate.
pub fn store(
    db: &Db,
    checked: &[Checked<'_>],
    served: &Served,
    root: &str,
    // dyn: the driver's store, one call per crate per session
    store: &dyn ImageStore,
) -> Result<Stored, String> {
    let mut stored = Stored::default();
    // alloc: setup: once per session
    let by_crate: HashMap<Crate, &Checked<'_>> = checked.iter().map(|it| (it.krate, it)).collect();
    let (layers, left) = layers(db, by_crate.keys().copied(), &served.keys);
    for (krate, dep) in left {
        // alloc: why, once per crate refused
        stored.refused.push((krate, Refused::Dependency { name: crate_name(db, dep) }));
    }
    // alloc: setup: once per session
    let mut keys = served.keys.clone();
    for layer in layers {
        // alloc: setup: once per layer per session
        let mut asked: Vec<(&Checked<'_>, Vec<ContentId>)> = Vec::with_capacity(layer.len());
        for krate in layer {
            match dep_keys(db, krate, &keys) {
                Some(deps) => asked.push((by_crate[&krate], deps)),
                None => {
                    let dep = (krate.data(db).dependencies.iter())
                        .find(|dep| !keys.contains_key(&dep.crate_id))
                        // alloc: the refusal's crate name, on the refusal path
                        .map_or_else(String::new, |dep| crate_name(db, dep.crate_id));
                    stored.refused.push((krate, Refused::Dependency { name: dep }));
                }
            }
        }
        // alloc: setup: one slot per crate, once per session
        let made: Vec<OnceLock<Made>> = asked.iter().map(|_| OnceLock::new()).collect();
        store.for_each(asked.len(), &|at| {
            let (check, deps) = &asked[at];
            _ = made[at].set(keep(db, check, deps, root, store));
        });
        for ((check, _), made) in asked.iter().zip(made) {
            let made = made.into_inner().unwrap_or_else(|| {
                panic!("`ImageStore::for_each` did not run the task of crate {}", crate_name(db, check.krate))
            });
            match made? {
                Ok((key, outcome)) => {
                    keys.insert(check.krate, key);
                    stored.keys.insert(check.krate, key);
                    if let Some(why) = outcome {
                        stored.refused.push((check.krate, why));
                    }
                }
                Err(why) => stored.refused.push((check.krate, why)),
            }
        }
    }
    Ok(stored)
}

/// Keeps the bodies part and the outcome of `check`, whose dependencies' check keys are `deps`:
/// one task of [`store`].
// dyn: the driver's store, one call per crate per session
fn keep(db: &Db, check: &Checked<'_>, deps: &[ContentId], root: &str, store: &dyn ImageStore) -> Made {
    let krate = check.krate;
    let put = |at: ContentId, what: &str, bytes: Vec<u8>| {
        store.put(at, bytes).map_err(|why| {
            // alloc: error text
            format!("keeping the {what} of crate {} (key {at:?}): {why}", crate_name(db, krate))
        })
    };
    let reads = match hir_def::deps::checked_reads(db, krate) {
        Ok(reads) => reads,
        Err(why) => return Ok(Err(Refused::Reads(why))),
    };
    let passed = |def| hir_ty_borrowck::checked(db, def).is_ok();
    let part = match hir_ty::check_cache::encode(db, krate, &passed) {
        Ok(part) => part,
        Err(why) => return Ok(Err(Refused::Bodies { why })),
    };
    let base = bodies_key(check.image, deps, check.inputs.borrowck);
    let filed = match hir_def::deps::file_by_reads(db, krate, base, &reads, store)? {
        Ok(filed) => filed,
        Err(why) => return Ok(Err(Refused::Reads(why))),
    };
    put(filed, "checked bodies", part)?;
    let outcome = match encode(check.diags, &Paths::of(db, krate), root) {
        Ok(bytes) => {
            put(key(filed, &check.inputs), "check outcome", bytes)?;
            None
        }
        Err(why) => Some(why),
    };
    Ok(Ok((filed, outcome)))
}
