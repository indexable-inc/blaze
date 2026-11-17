//! The gather: a unit's files and late answers from its root, the one copy every caller takes
//! (blaze-command's `front::Command::gather`, the std test, hir-def's std census). It reads
//! nothing itself: it asks its [`Source`] for a file's bytes by path and whether a path is a
//! file, files each image in the caller's [`Images`], and hashes a late read's answer by the
//! caller's [`Digests`]. So the core is sans-IO and the three agree by construction.
//!
//! # What it gathers
//!
//! 1. The root, imaged, then each file's active out-of-line modules (`mods::mod_files`, under
//!    the unit's cfg) breadth first, each read and imaged in turn.
//! 2. The files the bodies of the unit's own macros spell at each call (`mods::macro_sites`,
//!    `mods::macro_files`; `mods`, "A macro the unit defines"), then 1 again for what they
//!    declare, until no site is new: the fixpoint.
//! 3. The unit's late reads (`unit::asks`), each answered by the one rule ([`crate::late`]).
//!
//! A declaration in a macro call's tokens or a macro body (`ModFile::in_call`) takes every
//! candidate file that exists and none is no error; any other takes exactly one, or is refused
//! ([`MISSING_FILE`], [`TWO_MODULE_FILES`]). A file below itself is a cycle ([`CIRCULAR`]); the
//! same file under one module path again is one file, another file under a module path taken
//! is suffixed (`mods::gathered_module`, `mods::module_twice`). A file reached only through a
//! module a macro spells is a candidate ([`FileText::candidate`]): what the gather cannot answer
//! in it (a `mod` with no file or two, declarations that do not resolve, a late read that is not
//! a literal or names no file) is [`Deferred`], not refused, when [`Gather::defer`] is set;
//! a deferred read is answered in the keys by [`deferred_answer`] and has no late-table fact.
//!
//! # Cost against the floor
//!
//! Floor: one read and one image per file of the unit, one stat per candidate path, one pass of
//! `unit::asks`, one read per included file. Here: that, plus a file imaged only when the
//! caller's [`Images`] lacks it (once per content a process), plus each fixpoint round reopening
//! every image (bounds checks, O(files) per round; std's crates take one or two rounds), plus
//! `mods::gathered_module`'s O(files) per declaration. Ratio about 1 for every crate of std.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    kind::{FileImage, Key, Late, Settings, ToolId},
    record::{self, Fact},
};
use cfg::CfgOptions;

use crate::{
    image::{self, Reading},
    late,
    mods::{self, FileKind, ModFile},
    read::Image,
    unit,
};

/// A file the root or a `mod` declaration names is not there.
pub const MISSING_FILE: &str = "gathering a unit: a file its root or a `mod` declaration names does not exist";
/// Both `name.rs` and `name/mod.rs` exist.
pub const TWO_MODULE_FILES: &str = "gathering a unit: a `mod` declaration has both `name.rs` and `name/mod.rs` (rustc E0761)";
/// A module file reached below itself.
pub const CIRCULAR: &str =
    "gathering a unit: a module file declares itself, directly or through its modules (a `mod` or `#[path]` cycle)";
/// A path that is not UTF-8.
pub const NOT_UTF8_PATH: &str = "gathering a unit: a module or include path is not UTF-8";
/// A source file that does not image.
pub const NOT_IMAGED: &str = "gathering a unit: a source file does not image (`blaze_unitkey::image::ImageError`)";
/// A file's declarations that cannot be named.
pub const MODS: &str = "gathering a unit: a file's `mod` declarations do not resolve (`blaze_unitkey::mods::ModError`)";
/// A late read whose argument is not a literal.
pub const NOT_LITERAL: &str =
    "gathering a unit: a late read (`env!`, `include!`, ...) whose argument is not a literal (macro arguments are not evaluated here)";
/// An included file that is not there.
pub const MISSING_INCLUDE: &str = "gathering a unit: an `include!`, `include_str!` or `include_bytes!` names no file";
/// The unit's late reads did not come out of its images.
pub const ASKS: &str = "gathering a unit: its late reads do not read out of its images (`blaze_unitkey::unit::asks`)";

/// What a [`Source`] refused: the rule.
pub type SourceError = &'static str;

/// The one place a gathered file is read: the command's driver hands a disk, a test a map.
pub trait Source {
    /// Reads `path` (absolute, `/` separated) into `into`, which is cleared first. `Ok(false)`:
    /// no such file.
    fn read<B: Allocator>(&mut self, path: &str, into: &mut Vec<u8, B>) -> Result<bool, SourceError>;
    /// Whether `path` is a file.
    fn exists(&mut self, path: &str) -> Result<bool, SourceError>;
}

/// Where the gather files the images it makes, and finds those made before (by any unit of the
/// process): the command's map by content, a test's list.
pub trait Images {
    /// The image filed under `key`, 16-aligned; `None` before it is filed.
    fn get(&self, key: &Key<FileImage>) -> Option<&[u8]>;
    /// Files `record`, the image of `key`.
    fn put(&mut self, key: Key<FileImage>, record: &[u8]);
}

/// How a late read's answer is hashed: the request's own rule (blaze-host's `outside`), which
/// the gather does not own. A strategy, so a type parameter (rule 78).
pub trait Digests {
    /// The answer to a variable read whose value is `value` (`None`: absent, a value too).
    fn var(value: Option<&[u8]>) -> ContentId;
    /// The answer to a file read whose bytes are `bytes`.
    fn file(bytes: &[u8]) -> ContentId;
}

/// What the gather is given of a unit.
pub struct Gather<'a, E> {
    /// The image key's tool and the reading every file of the unit is imaged with.
    pub tool: ToolId,
    pub reading: Reading,
    /// The unit's cfg: which declarations and late reads are active.
    pub cfg: &'a CfgOptions,
    /// The root file, absolute, `/` separated.
    pub root: &'a [u8],
    /// The unit's variables (its plan's and its build script's `rustc-env`), sorted by name,
    /// each a pair through `pair`; `member`: a workspace member (`CARGO_PRIMARY_PACKAGE`).
    pub env: &'a [E],
    pub pair: for<'e> fn(&'e E) -> (&'e [u8], &'e [u8]),
    pub member: bool,
    /// Whether what cannot be answered in a candidate file is [`Deferred`]; `false` refuses it
    /// as in any other file (a test's planted control).
    pub defer: bool,
}

/// One gathered file of a unit.
pub struct FileText<A: Allocator> {
    /// Absolute, UTF-8 (checked at gathering).
    pub path: Vec<u8, A>,
    /// Module path (`` for the root, `a::b`, `a::b{1}` for a second file under `a::b`).
    pub module: Vec<u8, A>,
    pub text: Vec<u8, A>,
    pub image: Key<FileImage>,
    /// Reached only through a module a macro call or a macro definition's body spells, here
    /// or in an ancestor (`mods`, "Modules a macro call writes"): an over-approximated file,
    /// which the program may not use. What the gather cannot answer in it is [`Deferred`].
    pub candidate: bool,
}

/// What the gather could not answer in a candidate file ([`FileText::candidate`]): a late read
/// whose argument is not a literal or whose file is not there, a `mod` with no file or two, a
/// file's declarations that do not resolve. It fails nothing while the program does not use
/// the file: the key answers such a read by [`deferred_answer`], and a later step (the
/// command's surface task, once the def map is made) refuses the unit only if a module of the
/// def map is in that file.
pub struct Deferred<A: Allocator> {
    /// The file's index in [`UnitText::files`].
    pub file: u32,
    /// The rule the gather would have refused by.
    pub rule: &'static str,
    /// What it is, for a told line: `include!(concat!(..))`, ``mod `foo` ``.
    pub what: Vec<u8, A>,
}

/// One answered late read of a unit, in `unit::LateAnswer` order.
pub struct Answer<A: Allocator> {
    pub file: u32,
    pub late: Late,
    pub name: Vec<u8, A>,
    pub value: Option<ContentId>,
}

/// A file a late read includes.
pub struct Included<A: Allocator> {
    pub path: Vec<u8, A>,
    pub text: Vec<u8, A>,
}

/// A unit's text: its files (the root first), its late answers, what they include, its late
/// table (sorted by id, each once), and what was deferred, in the gather's order.
pub struct UnitText<A: Allocator> {
    pub files: Vec<FileText<A>, A>,
    pub answers: Vec<Answer<A>, A>,
    pub included: Vec<Included<A>, A>,
    pub late: Vec<Fact, A>,
    pub deferred: Vec<Deferred<A>, A>,
}

/// Why a unit was not gathered: the rule, the file it is about (the root, the file holding the
/// declaration or the read, or the path the source refused), and what in it (``mod `foo` ``,
/// `include!(..)`; empty when the file is the whole of it).
#[derive(Debug)]
pub struct GatherError<A: Allocator> {
    pub rule: &'static str,
    pub path: Vec<u8, A>,
    pub what: Vec<u8, A>,
}

/// A deferred late read's answer in the keys: the same for every such read of one rule and
/// argument, so keys stay a function of text, and never a value a read gives.
pub fn deferred_answer(rule: &str, argument: &[u8]) -> ContentId {
    let mut hasher = Hasher::new("blaze-command deferred late read 1");
    hasher.part(rule.as_bytes()).part(argument);
    hasher.finish()
}

/// A late read's macro, as spelled.
pub fn late_name(late: Late) -> &'static str {
    match late {
        Late::Env => "env!",
        Late::OptionEnv => "option_env!",
        Late::Include => "include!",
        Late::IncludeStr => "include_str!",
        Late::IncludeBytes => "include_bytes!",
    }
}

/// Gathers the unit `unit` describes through `source`, filing images in `images` and hashing
/// late answers by `D`; every vector in `alloc`.
pub fn gather<S: Source, I: Images, D: Digests, E, A: Allocator + Clone>(
    unit: &Gather<'_, E>,
    source: &mut S,
    images: &mut I,
    alloc: A,
) -> Result<UnitText<A>, GatherError<A>> {
    let mut run = Run {
        unit,
        source,
        images,
        files: Vec::new_in(alloc.clone()),
        kinds: Vec::new_in(alloc.clone()),
        parents: Vec::new_in(alloc.clone()),
        deferred: Vec::new_in(alloc.clone()),
        alloc: alloc.clone(),
    };
    let root = run
        .load(unit.root, Vec::new_in(alloc.clone()), false)?
        .ok_or_else(|| error(MISSING_FILE, unit.root, &[], alloc.clone()))?;
    run.files.push(root);
    run.kinds.push(FileKind::ModRs);
    run.parents.push(u32::MAX);
    let mut done = Vec::new_in(alloc.clone());
    let mut next = 0;
    loop {
        while next < run.files.len() {
            let declared = {
                let image = run.open(next)?;
                let file = &run.files[next];
                mods::mod_files(&image, &file.path, run.kinds[next], &file.module, unit.cfg, alloc.clone())
            };
            let declared = match declared {
                Ok(declared) => declared,
                Err(_) if run.defers(next) => {
                    run.defer(next, MODS, &[&b"its `mod` declarations"[..]]);
                    Vec::new_in(alloc.clone())
                }
                Err(_) => return Err(error(MODS, &run.files[next].path, &[], alloc.clone())),
            };
            for declared in declared.iter() {
                run.take(next, declared)?;
            }
            next += 1;
        }
        // The modules macro definitions' bodies spell, at each call of them in the unit, until
        // no site is new: a file a site names may hold more calls or definitions.
        let found = {
            let mut opened = Vec::with_capacity_in(run.files.len(), alloc.clone());
            for at in 0..run.files.len() {
                opened.push(run.open(at)?);
            }
            let root = unit.root;
            let sites = mods::macro_sites(&opened, unit.cfg, &mut done, alloc.clone())
                .map_err(|_| error(MODS, root, b"the macros that spell modules", alloc.clone()))?;
            let mut found = Vec::new_in(alloc.clone());
            for site in sites.iter() {
                let call = site.call_file as usize;
                let file = &run.files[call];
                let declared = mods::macro_files(
                    &opened[site.def_file as usize],
                    site.defined,
                    &opened[call],
                    site.called,
                    &file.path,
                    run.kinds[call],
                    &file.module,
                    unit.cfg,
                    alloc.clone(),
                )
                .map_err(|_| error(MODS, &file.path, b"the modules of a macro it calls", alloc.clone()))?;
                found.extend(declared.into_iter().map(|declared| (call, declared)));
            }
            found
        };
        if found.is_empty() {
            break;
        }
        for (call, declared) in found.iter() {
            run.take(*call, declared)?;
        }
    }
    let (answers, included, late) = run.answer::<D>()?;
    let Run { files, deferred, .. } = run;
    Ok(UnitText { files, answers, included, late, deferred })
}

/// `rule` about the file at `path`, what in it the parts of `what` spell.
fn error<A: Allocator + Clone>(rule: &'static str, path: &[u8], what: &[u8], alloc: A) -> GatherError<A> {
    let mut owned = Vec::with_capacity_in(path.len(), alloc.clone());
    owned.extend_from_slice(path);
    let mut said = Vec::with_capacity_in(what.len(), alloc);
    said.extend_from_slice(what);
    GatherError { rule, path: owned, what: said }
}

/// A gather in progress: its files (the root first), what each is to its modules, the file each
/// was declared in (`u32::MAX` for the root: its ancestors), what was deferred.
struct Run<'r, 'u, S, I, E, A: Allocator + Clone> {
    unit: &'r Gather<'u, E>,
    source: &'r mut S,
    images: &'r mut I,
    files: Vec<FileText<A>, A>,
    kinds: Vec<FileKind, A>,
    parents: Vec<u32, A>,
    deferred: Vec<Deferred<A>, A>,
    alloc: A,
}

impl<S: Source, I: Images, E, A: Allocator + Clone> Run<'_, '_, S, I, E, A> {
    /// Reads `path` and images it, once per content. `Ok(None)`: no such file.
    fn load(&mut self, path: &[u8], module: Vec<u8, A>, candidate: bool) -> Result<Option<FileText<A>>, GatherError<A>> {
        let alloc = self.alloc.clone();
        let name = core::str::from_utf8(path).map_err(|_| error(NOT_UTF8_PATH, path, &[], alloc.clone()))?;
        let mut text = Vec::new_in(alloc.clone());
        if !self.source.read(name, &mut text).map_err(|rule| error(rule, path, &[], alloc.clone()))? {
            return Ok(None);
        }
        let key = image::key(&self.unit.tool, self.unit.reading, &text);
        if self.images.get(&key).is_none() {
            let imaged = image::image(&self.unit.tool, self.unit.reading, &text, alloc.clone())
                .map_err(|_| error(NOT_IMAGED, path, &[], alloc.clone()))?;
            self.images.put(key, &imaged.record);
        }
        let mut owned = Vec::with_capacity_in(path.len(), alloc);
        owned.extend_from_slice(path);
        Ok(Some(FileText { path: owned, module, text, image: key, candidate }))
    }

    /// File `at`'s image, opened in place.
    fn open(&self, at: usize) -> Result<Image<'_>, GatherError<A>> {
        let file = &self.files[at];
        let bytes = self.images.get(&file.image).ok_or_else(|| error(NOT_IMAGED, &file.path, &[], self.alloc.clone()))?;
        Image::open(bytes, &file.image).map_err(|_| error(NOT_IMAGED, &file.path, &[], self.alloc.clone()))
    }

    /// Whether what the gather cannot answer in file `at` is deferred, not refused.
    fn defers(&self, at: usize) -> bool {
        self.unit.defer && self.files[at].candidate
    }

    /// Defers `rule` in file `at`, what the parts of `what` spell.
    fn defer(&mut self, at: usize, rule: &'static str, what: &[&[u8]]) {
        let mut said = Vec::new_in(self.alloc.clone());
        for part in what {
            said.extend_from_slice(part);
        }
        self.deferred.push(Deferred { file: at as u32, rule, what: said });
    }

    /// Whether `path` is file `at`'s own path or one of its ancestors'. The chain is as long as
    /// the module tree is deep: a file reached twice under two module paths is not a cycle, one
    /// reached again below itself is.
    fn is_ancestor(&self, at: usize, path: &[u8]) -> bool {
        let mut up = at as u32;
        while let Some(file) = self.files.get(up as usize) {
            if file.path[..] == path[..] {
                return true;
            }
            up = self.parents.get(up as usize).copied().unwrap_or(u32::MAX);
        }
        false
    }

    /// The files of `declared`, a declaration of file `at` (in the file, or in the body of a
    /// macro it calls): each that exists for a module a macro spells, else the one that must.
    /// In a candidate file, a plain `mod` with no file or two is deferred.
    fn take(&mut self, at: usize, declared: &ModFile<A>) -> Result<(), GatherError<A>> {
        let alloc = self.alloc.clone();
        let from = |rule| error(rule, &self.files[at].path, &[], alloc.clone());
        let first = core::str::from_utf8(&declared.first.path).map_err(|_| from(NOT_UTF8_PATH))?;
        let second = match &declared.second {
            Some(second) => Some(core::str::from_utf8(&second.path).map_err(|_| from(NOT_UTF8_PATH))?),
            None => None,
        };
        let first_is = self.source.exists(first).map_err(|rule| error(rule, first.as_bytes(), &[], alloc.clone()))?;
        let second_is = match second {
            Some(second) => self.source.exists(second).map_err(|rule| error(rule, second.as_bytes(), &[], alloc.clone()))?,
            None => false,
        };
        let in_candidate = self.files[at].candidate;
        // A module a macro's tokens spell: every candidate there is the unit's, and none there
        // is no error. Any other: exactly one, or, in a candidate file, deferred.
        if !declared.in_call && first_is == second_is {
            let rule = if first_is { TWO_MODULE_FILES } else { MISSING_FILE };
            if !self.defers(at) {
                let mut what = Vec::new_in(alloc.clone());
                what.extend_from_slice(b"mod `");
                what.extend_from_slice(&declared.module);
                what.extend_from_slice(b"` at ");
                what.extend_from_slice(first.as_bytes());
                let mut path = Vec::new_in(alloc);
                path.extend_from_slice(&self.files[at].path);
                return Err(GatherError { rule, path, what });
            }
            self.defer(at, rule, &[&b"mod `"[..], &declared.module[..], &b"`"[..]]);
            return Ok(());
        }
        let there = [(first_is, Some(&declared.first)), (second_is, declared.second.as_ref())];
        for candidate in there.iter().filter(|(is, _)| *is).filter_map(|(_, candidate)| *candidate) {
            // One file may be two modules (two `#[path]`s naming it): legal Rust, each its own
            // module path. Only a file among its own ancestors is a cycle; one a macro's tokens
            // spell names no module expansion can make, so it is no file.
            if self.is_ancestor(at, &candidate.path) {
                match declared.in_call {
                    true => continue,
                    false => return Err(error(CIRCULAR, &candidate.path, &[], alloc)),
                }
            }
            let named = self.files.iter().map(|file| (&file.path[..], &file.module[..]));
            // The same file under the same module path again (two arms naming it) is one file;
            // another file under a module path taken gets it suffixed.
            let Some(module) = mods::gathered_module(&candidate.path, &declared.module, named, alloc.clone()) else {
                continue;
            };
            let file = self
                .load(&candidate.path, module, in_candidate || declared.in_call)?
                .ok_or_else(|| error(MISSING_FILE, &candidate.path, &[], alloc.clone()))?;
            self.files.push(file);
            self.kinds.push(candidate.kind);
            self.parents.push(at as u32);
        }
        Ok(())
    }

    /// The unit's late reads, answered: in `unit::LateAnswer` order, with the files they
    /// include and the unit's late table.
    #[allow(clippy::type_complexity)]
    fn answer<D: Digests>(&mut self) -> Result<(Vec<Answer<A>, A>, Vec<Included<A>, A>, Vec<Fact, A>), GatherError<A>> {
        let alloc = self.alloc.clone();
        // The asks, copied out of the images so that reading an include may file nothing new
        // while they are borrowed. Each: its file, which late read, its name, whether literal.
        let mut asks: Vec<(u32, Late, Vec<u8, A>, bool), A> = Vec::new_in(alloc.clone());
        {
            let mut inputs = Vec::with_capacity_in(self.files.len(), alloc.clone());
            for file in self.files.iter() {
                let image =
                    self.images.get(&file.image).ok_or_else(|| error(NOT_IMAGED, &file.path, &[], alloc.clone()))?;
                inputs.push(unit::UnitFile { key: file.image, image, module: &file.module });
            }
            // `unit::asks` reads the cfg and the files alone: the settings and the unit's
            // identity are the key's, which asking does not make.
            let input = unit::Unit {
                tool: self.unit.tool,
                settings: Settings(ContentId([0; 32])),
                unit: ContentId([0; 32]),
                cfg: self.unit.cfg,
                files: &inputs,
                answers: &[],
                names: &[],
            };
            let found = unit::asks(&input, alloc.clone()).map_err(|_| error(ASKS, self.unit.root, &[], alloc.clone()))?;
            for ask in found.iter() {
                let file = ask.file as usize;
                if !ask.literal && !(self.unit.defer && self.files[file].candidate) {
                    return Err(error(NOT_LITERAL, &self.files[file].path, &call(ask.late, ask.name, alloc.clone()), alloc));
                }
                let mut name = Vec::with_capacity_in(ask.name.len(), alloc.clone());
                name.extend_from_slice(ask.name);
                asks.push((ask.file, ask.late, name, ask.literal));
            }
        }
        let mut answers = Vec::with_capacity_in(asks.len(), alloc.clone());
        let mut included: Vec<Included<A>, A> = Vec::new_in(alloc.clone());
        let mut late = Vec::with_capacity_in(asks.len(), alloc.clone());
        for (file, which, name, literal) in asks {
            let at = file as usize;
            // A read of a candidate file the gather cannot answer: deferred, and answered in the
            // keys by its marker, which is no fact of the world, so it has no late-table row.
            if !literal {
                self.defer_read(file, which, name, NOT_LITERAL, &mut answers);
                continue;
            }
            let (value, digest) = match late::reads(which, &name, &self.files[at].path, alloc.clone()) {
                late::Reads::Var(var) => {
                    let found = late::var(self.unit.env, self.unit.pair, self.unit.member, var);
                    (found.map(|value| D::var(Some(value))), D::var(found))
                }
                late::Reads::File(path) => {
                    let path = mods::normal(&path, alloc.clone());
                    let shown = core::str::from_utf8(&path).map_err(|_| error(NOT_UTF8_PATH, &path, &[], alloc.clone()))?;
                    let mut text = Vec::new_in(alloc.clone());
                    if !self.source.read(shown, &mut text).map_err(|rule| error(rule, &path, &[], alloc.clone()))? {
                        if !self.defers(at) {
                            return Err(error(MISSING_INCLUDE, &self.files[at].path, &call(which, &name, alloc.clone()), alloc));
                        }
                        self.defer_read(file, which, name, MISSING_INCLUDE, &mut answers);
                        continue;
                    }
                    let id = D::file(&text);
                    if !included.iter().any(|it| it.path[..] == path[..]) {
                        included.push(Included { path, text });
                    }
                    (Some(id), id)
                }
            };
            late.push(Fact { id: record::late_id(which as u8, &name), digest });
            answers.push(Answer { file, late: which, name, value });
        }
        late.sort_unstable_by(|a, b| a.id.0.cmp(&b.id.0));
        late.dedup_by(|a, b| a.id == b.id);
        Ok((answers, included, late))
    }

    /// Defers the read `which` of `name` in file `file` by `rule`, answered by its marker.
    fn defer_read(&mut self, file: u32, which: Late, name: Vec<u8, A>, rule: &'static str, answers: &mut Vec<Answer<A>, A>) {
        let said = call(which, &name, self.alloc.clone());
        self.deferred.push(Deferred { file, rule, what: said });
        answers.push(Answer { file, late: which, value: Some(deferred_answer(rule, &name)), name });
    }
}

/// `macro!(argument)`, as written.
fn call<A: Allocator>(late: Late, argument: &[u8], alloc: A) -> Vec<u8, A> {
    let name = late_name(late).as_bytes();
    let mut out = Vec::with_capacity_in(name.len() + argument.len() + 2, alloc);
    out.extend_from_slice(name);
    out.push(b'(');
    out.extend_from_slice(argument);
    out.push(b')');
    out
}

/// A list of images by key, for a caller with no map of its own (the std test, the census):
/// sorted by key, found by binary search, each 16-aligned.
pub struct ImageList<A: Allocator> {
    kept: Vec<(Key<FileImage>, blaze_format::Aligned), A>,
}

impl<A: Allocator> ImageList<A> {
    pub fn new_in(alloc: A) -> ImageList<A> {
        ImageList { kept: Vec::new_in(alloc) }
    }
}

impl<A: Allocator> Images for ImageList<A> {
    fn get(&self, key: &Key<FileImage>) -> Option<&[u8]> {
        let at = self.kept.binary_search_by(|(it, _)| it.id().0.cmp(&key.id().0)).ok()?;
        Some(&self.kept[at].1[..])
    }

    fn put(&mut self, key: Key<FileImage>, record: &[u8]) {
        if let Err(at) = self.kept.binary_search_by(|(it, _)| it.id().0.cmp(&key.id().0)) {
            self.kept.insert(at, (key, blaze_format::Aligned::copy_from(record)));
        }
    }
}
