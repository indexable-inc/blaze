//! The record of one build-script run (target decision 7; outside.md's `StepRecord`).
//!
//! **Key** (the core's `ScriptRun` key, `ScriptInputs`, with [`script_env`] and
//! [`script_args`]): the script's code identity (its unit's code key, made from
//! its source closure before anything compiles, never the binary's bytes, so a lookup needs no
//! compile), the toolchain, the host (what no file shows: OS build, architecture, the
//! sandbox's version), every variable blaze gives it (cargo's `CARGO_*`, `TARGET`, `PROFILE`,
//! `OPT_LEVEL`, the features, the package's manifest fields cargo exposes, `OUT_DIR` as a
//! root), and its arguments. Nothing it has not been given: a variable passed through from the
//! user's shell is a traced read, by name.
//!
//! **Read list**: every file opened, path looked at, directory listed, variable read by name
//! (absence included) and the environment's walk set, as the sandbox traced them
//! ([`crate::host`]'s `Trace*` rows), each with the digest of its answer as read; and every
//! `rerun-if-changed` and `rerun-if-env-changed` it printed, as a read of its own
//! ([`add_declarations`]). The trace is the truth: a declaration only adds reads (cargo would
//! rerun on it, so blaze does), it never removes one, and a traced read no declaration covers
//! is reported ([`undeclared`]), not dropped.
//!
//! **Value** ([`ScriptValue`]): how it exited, the directives it printed (parsed once, by
//! cargo's reader in blaze-build-script, and kept typed), its `OUT_DIR` as a content-addressed
//! tree ([`OutTree`]: each file's bytes a blob of its own, restored byte for byte), and its
//! stderr.

use std::{alloc::Global, borrow::Cow};

use blaze_build_script::output::{Directive, LinkArgTarget, Severity};
use blaze_ids::Hasher;
use blaze_keyed::kind::ScriptRun;
use crate::{
    Digest,
    flat::{Corrupt, Reader, Why, Writer},
    reads::{Answers, Ask, AskBuf, ReadList, Reads, Unanswerable},
};

/// The `env` of a run's key (`blaze_keyed::kind::ScriptInputs`): each variable blaze gives the
/// script, by name and value, given in any order and hashed sorted by name. A name given twice
/// is the driver's bug, and panics.
pub fn script_env(given: &[(&[u8], &[u8])]) -> Digest {
    let mut order = Vec::with_capacity_in(given.len(), Global);
    order.extend(0..given.len());
    order.sort_unstable_by(|&a, &b| given[a].0.cmp(given[b].0));
    assert!(order.windows(2).all(|it| given[it[0]].0 != given[it[1]].0), "a script is given each variable once");
    let mut hasher = Hasher::new("blaze-expansion script env 1");
    hasher.update(&(order.len() as u64).to_le_bytes());
    for at in order {
        let (name, value) = given[at];
        hasher.part(name).part(value);
    }
    hasher.finish()
}

/// The `args` of a run's key: the arguments, in order.
pub fn script_args(args: &[&[u8]]) -> Digest {
    let mut hasher = Hasher::new("blaze-expansion script args 1");
    hasher.update(&(args.len() as u64).to_le_bytes());
    for arg in args {
        hasher.part(arg);
    }
    hasher.finish()
}

/// How a run ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

/// What an entry of `OUT_DIR` is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Mode {
    File,
    Executable,
    /// Its bytes are the link's target.
    Link,
    /// An empty directory (a directory with entries is implied by them).
    Dir,
}

impl Mode {
    fn byte(self) -> u8 {
        match self {
            Mode::File => 0,
            Mode::Executable => 1,
            Mode::Link => 2,
            Mode::Dir => 3,
        }
    }

    fn of(byte: u8) -> Option<Mode> {
        Some(match byte {
            0 => Mode::File,
            1 => Mode::Executable,
            2 => Mode::Link,
            3 => Mode::Dir,
            _ => return None,
        })
    }
}

/// The digest a blob of `OUT_DIR` is kept under: its bytes.
pub fn blob_digest(bytes: &[u8]) -> Digest {
    Digest::of("blaze-expansion out-dir blob 1", bytes)
}

/// A run's `OUT_DIR` as a tree: each entry's path from `OUT_DIR`, mode, blob digest and length,
/// sorted by path, each path once.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OutTree {
    entries: Vec<(Vec<u8, Global>, Mode, Digest, u64), Global>,
}

impl Default for OutTree {
    fn default() -> OutTree {
        OutTree { entries: Vec::new_in(Global) }
    }
}

/// Two entries of one path in a tree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TwoEntries(pub Vec<u8, Global>);

impl OutTree {
    /// Adds an entry of the tree the run left, as the driver walked it, in any order.
    pub fn push(&mut self, path: &[u8], mode: Mode, digest: Digest, len: u64) {
        let mut owned = Vec::with_capacity_in(path.len(), Global);
        owned.extend_from_slice(path);
        self.entries.push((owned, mode, digest, len));
    }

    /// Sorts the entries, refusing a path given twice.
    pub fn seal(&mut self) -> Result<(), TwoEntries> {
        self.entries.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in self.entries.windows(2) {
            if pair[0].0 == pair[1].0 {
                let mut path = Vec::new_in(Global);
                path.extend_from_slice(&pair[0].0);
                return Err(TwoEntries(path));
            }
        }
        Ok(())
    }

    /// The tree's digest: what a reader of `OUT_DIR` (an `include!`) is keyed by as a whole.
    pub fn digest(&self) -> Digest {
        let mut hasher = Hasher::new("blaze-expansion out-dir tree 1");
        hasher.update(&(self.entries.len() as u64).to_le_bytes());
        for (path, mode, digest, len) in &self.entries {
            hasher.part(path).update(&[mode.byte()]).id(digest).update(&len.to_le_bytes());
        }
        hasher.finish()
    }

    fn write(&self, w: &mut Writer) {
        w.u32(self.entries.len() as u32);
        for (path, mode, digest, len) in &self.entries {
            w.bytes(path).u8(mode.byte()).digest(digest).u64(*len);
        }
    }
}

/// The store's blobs: a file of `OUT_DIR` by its digest. The driver's.
pub trait Blobs {
    fn blob(&self, digest: &Digest) -> Option<&[u8]>;
}

/// An entry of a kept tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutEntry<'a> {
    pub path: &'a [u8],
    pub mode: Mode,
    pub digest: Digest,
    pub len: u64,
}

/// Why a kept tree can't be restored.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unrestorable {
    /// The store has no blob of this entry's digest.
    Missing(Vec<u8, Global>),
    /// The blob's bytes are not what the entry says.
    Wrong(Vec<u8, Global>),
}

/// A kept tree, read in place.
#[derive(Clone)]
pub struct OutTreeRef<'a> {
    count: u32,
    entries: Reader<'a>,
}

impl<'a> OutTreeRef<'a> {
    fn read(r: &mut Reader<'a>) -> Result<OutTreeRef<'a>, Corrupt> {
        let count = r.u32()?;
        let entries = r.clone();
        for _ in 0..count {
            r.bytes()?;
            let byte = r.u8()?;
            Mode::of(byte).ok_or(r.corrupt(Why::Tag(byte)))?;
            r.digest()?;
            r.u64()?;
        }
        Ok(OutTreeRef { count, entries })
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn entries(&self) -> impl Iterator<Item = OutEntry<'a>> + 'a {
        let mut r = self.entries.clone();
        (0..self.count).map(move |_| OutEntry {
            path: r.bytes().expect("checked at read"),
            mode: Mode::of(r.u8().expect("checked at read")).expect("checked at read"),
            digest: r.digest().expect("checked at read"),
            len: r.u64().expect("checked at read"),
        })
    }

    /// Gives each entry with its bytes to `place`, in path order, checking each blob is the
    /// bytes its entry names: the tree restored byte for byte, or why not.
    pub fn restore<B: Blobs>(
        &self,
        blobs: &B,
        mut place: impl FnMut(OutEntry<'a>, &[u8]),
    ) -> Result<(), Unrestorable> {
        for entry in self.entries() {
            let owned = || {
                let mut path = Vec::with_capacity_in(entry.path.len(), Global);
                path.extend_from_slice(entry.path);
                path
            };
            if entry.mode == Mode::Dir {
                place(entry, &[]);
                continue;
            }
            let Some(bytes) = blobs.blob(&entry.digest) else {
                return Err(Unrestorable::Missing(owned()));
            };
            if blob_digest(bytes) != entry.digest || bytes.len() as u64 != entry.len {
                return Err(Unrestorable::Wrong(owned()));
            }
            place(entry, bytes);
        }
        Ok(())
    }
}

/// Writes a run's value.
pub struct ScriptValue<'w> {
    pub exit: Exit,
    pub directives: &'w [Directive<'w>],
    pub out_dir: &'w OutTree,
    pub stderr: &'w [u8],
}

fn put_target(w: &mut Writer, target: &LinkArgTarget) {
    match target {
        LinkArgTarget::All => w.u8(0),
        LinkArgTarget::Cdylib => w.u8(1),
        LinkArgTarget::Bin => w.u8(2),
        LinkArgTarget::SingleBin(name) => w.u8(3).str(name),
        LinkArgTarget::Test => w.u8(4),
        LinkArgTarget::Bench => w.u8(5),
        LinkArgTarget::Example => w.u8(6),
    };
}

impl ScriptValue<'_> {
    pub fn encode(&self) -> Vec<u8, Global> {
        let mut w = Writer::new::<ScriptRun>();
        match self.exit {
            Exit::Code(code) => w.u8(0).i64(i64::from(code)),
            Exit::Signal(signal) => w.u8(1).i64(i64::from(signal)),
        };
        w.u32(self.directives.len() as u32);
        for directive in self.directives {
            match directive {
                Directive::LinkSearch(it) => w.u8(0).str(it),
                Directive::LinkLib(it) => w.u8(1).str(it),
                Directive::LinkArg(target, it) => {
                    w.u8(2);
                    put_target(&mut w, target);
                    w.str(it)
                }
                Directive::Cfg(it) => w.u8(3).str(it),
                Directive::CheckCfg(it) => w.u8(4).str(it),
                Directive::Env(name, value) => w.u8(5).str(name).str(value),
                Directive::Metadata(key, value) => w.u8(6).str(key).str(value),
                Directive::RerunIfChanged(it) => w.u8(7).str(it),
                Directive::RerunIfEnvChanged(it) => w.u8(8).str(it),
                Directive::Log(Severity::Error, it) => w.u8(9).str(it),
                Directive::Log(Severity::Warning, it) => w.u8(10).str(it),
            };
        }
        self.out_dir.write(&mut w);
        w.bytes(self.stderr);
        w.finish()
    }
}

/// A run's value, read in place.
#[derive(Clone)]
pub struct ScriptValueRef<'a> {
    pub exit: Exit,
    directive_count: u32,
    directives: Reader<'a>,
    pub out_dir: OutTreeRef<'a>,
    pub stderr: &'a [u8],
}

fn read_directive<'a>(r: &mut Reader<'a>) -> Result<Directive<'a>, Corrupt> {
    fn s<'b>(r: &mut Reader<'b>) -> Result<Cow<'b, str>, Corrupt> {
        r.str().map(Cow::Borrowed)
    }
    Ok(match r.u8()? {
        0 => Directive::LinkSearch(s(r)?),
        1 => Directive::LinkLib(s(r)?),
        2 => {
            let target = match r.u8()? {
                0 => LinkArgTarget::All,
                1 => LinkArgTarget::Cdylib,
                2 => LinkArgTarget::Bin,
                // alloc: cargo's `LinkArgTarget` owns a bin's name as a `String`.
                3 => LinkArgTarget::SingleBin(r.str()?.to_owned()),
                4 => LinkArgTarget::Test,
                5 => LinkArgTarget::Bench,
                6 => LinkArgTarget::Example,
                tag => return Err(r.corrupt(Why::Tag(tag))),
            };
            Directive::LinkArg(target, s(r)?)
        }
        3 => Directive::Cfg(s(r)?),
        4 => Directive::CheckCfg(s(r)?),
        5 => {
            let name = s(r)?;
            Directive::Env(name, s(r)?)
        }
        6 => {
            let key = s(r)?;
            Directive::Metadata(key, s(r)?)
        }
        7 => Directive::RerunIfChanged(s(r)?),
        8 => Directive::RerunIfEnvChanged(s(r)?),
        9 => Directive::Log(Severity::Error, s(r)?),
        10 => Directive::Log(Severity::Warning, s(r)?),
        tag => return Err(r.corrupt(Why::Tag(tag))),
    })
}

impl<'a> ScriptValueRef<'a> {
    pub fn read(bytes: &'a [u8]) -> Result<ScriptValueRef<'a>, Corrupt> {
        let mut r = Reader::open::<ScriptRun>(bytes)?;
        let exit = match r.u8()? {
            0 => Exit::Code(r.i64()? as i32),
            1 => Exit::Signal(r.i64()? as i32),
            tag => return Err(r.corrupt(Why::Tag(tag))),
        };
        let directive_count = r.u32()?;
        let directives = r.clone();
        for _ in 0..directive_count {
            read_directive(&mut r)?;
        }
        let out_dir = OutTreeRef::read(&mut r)?;
        let stderr = r.bytes()?;
        r.end()?;
        Ok(ScriptValueRef { exit, directive_count, directives, out_dir, stderr })
    }

    /// The directives, in the order printed (checked when read).
    pub fn directives(&self) -> impl Iterator<Item = Directive<'a>> + 'a {
        let mut r = self.directives.clone();
        (0..self.directive_count).map(move |_| read_directive(&mut r).expect("checked at read"))
    }
}

/// `path` as a declaration names it, against the package's root: as written when absolute,
/// else below `package`.
fn declared_path(package: &[u8], path: &str, out: &mut Vec<u8, Global>) {
    out.clear();
    if !path.starts_with('/') {
        out.extend_from_slice(package);
        out.push(b'/');
    }
    out.extend_from_slice(path.as_bytes());
}

/// Adds each `rerun-if-changed` (as everything below the path) and `rerun-if-env-changed` the
/// run printed to its reads, with the answer each has now: cargo reruns on them, so a record is
/// valid only while they answer the same, whether or not the script read them itself.
pub fn add_declarations<'d, A: Answers>(
    reads: &mut Reads,
    package: &[u8],
    directives: impl IntoIterator<Item = &'d Directive<'d>>,
    answers: &mut A,
) -> Result<(), (AskBuf, Unanswerable)> {
    let mut path = Vec::new_in(Global);
    for directive in directives {
        let ask = match directive {
            Directive::RerunIfChanged(it) => {
                declared_path(package, it, &mut path);
                Ask::Tree(&path)
            }
            Directive::RerunIfEnvChanged(it) => Ask::Env(it.as_bytes()),
            _ => continue,
        };
        let answer = answers.answer(&ask).map_err(|why| (AskBuf::of(&ask), why))?;
        reads.push(&ask, answer);
    }
    Ok(())
}

/// A traced read no declaration covers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Undeclared {
    pub ask: AskBuf,
    /// Whether it is below the package: cargo, with the same declarations, would miss a
    /// change there; one outside (a C compiler's headers) cargo never watches at all.
    pub in_package: bool,
}

/// Whether `path` is `dir` or below it.
fn below(path: &[u8], dir: &[u8]) -> bool {
    path.starts_with(dir) && (path.len() == dir.len() || path[dir.len()] == b'/' || dir.ends_with(b"/"))
}

/// The reads of `list` that the run's declarations do not cover, as cargo reads them: with no
/// `rerun-if-changed` printed, the whole package is watched; with some, only what they name.
/// A variable is covered by its `rerun-if-env-changed`; the environment's walk never is. A
/// declaration's own read ([`add_declarations`]) covers itself.
pub fn undeclared<'d>(
    list: &ReadList,
    package: &[u8],
    directives: impl IntoIterator<Item = &'d Directive<'d>> + Clone,
) -> Vec<Undeclared, Global> {
    let mut out = Vec::new_in(Global);
    let watches_package =
        !directives.clone().into_iter().any(|it| matches!(it, Directive::RerunIfChanged(_)));
    let mut declared = Vec::new_in(Global);
    for (ask, _) in list.iter() {
        let covered = match ask {
            Ask::File(path) | Ask::Look(path) | Ask::List(path) | Ask::Tree(path) => {
                let in_package = below(path, package);
                let covered = (watches_package && in_package)
                    || directives.clone().into_iter().any(|it| match it {
                        Directive::RerunIfChanged(dir) => {
                            declared_path(package, dir, &mut declared);
                            below(path, &declared)
                        }
                        _ => false,
                    });
                if !covered {
                    out.push(Undeclared { ask: AskBuf::of(&ask), in_package });
                }
                continue;
            }
            Ask::Env(name) => directives.clone().into_iter().any(
                |it| matches!(it, Directive::RerunIfEnvChanged(var) if var.as_bytes() == name),
            ),
            Ask::EnvWalk => false,
            // A script asks no span question.
            Ask::Span(..) | Ask::Join(..) | Ask::Edition(_) => true,
        };
        if !covered {
            out.push(Undeclared { ask: AskBuf::of(&ask), in_package: false });
        }
    }
    out
}
