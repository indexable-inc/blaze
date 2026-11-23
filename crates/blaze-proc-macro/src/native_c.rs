//! `ShelfClang`: the C compiler strategy of blaze-build-script `c_tasks` (rule 78; c.md D174,
//! design A) that runs the shelf's pinned clang (`<shelf>/bin/clang-23`, pins/clang.json) on
//! ONE object of a script's recorded requests, confined by bstd's hermetic run at
//! `Level::Inputs` (D177: the one way blaze runs a program confined) with the grant a native
//! build script gets (native_script.rs `run_script`), narrowed to one compile:
//!
//! - the program: `bin/clang-23` of the shelf, run with `-fintegrated-cc1 -c` and
//!   `--no-default-config`, so it starts no helper (no `helpers`: it may exec nothing) and reads
//!   no `clang.cfg`;
//! - reads: the package's folder, the script's `OUT_DIR` as its record laid, each dependency's
//!   laid `OUT_DIR` (`DEP_*_INCLUDE`), the SDK, the shelf: each a tree root named by its
//!   digest ([`Laid`]); nothing else but macOS's loader reads;
//! - writes: one task folder for the object and its depfile (the output root, emptied first);
//!   no network, no channel;
//! - environment: `SOURCE_DATE_EPOCH=0` alone (`ClockSpec::Fixed { unix_secs: 0 }`: clang takes
//!   `__DATE__`, `__TIME__` and `__TIMESTAMP__` from it), no `PATH`, no `TMPDIR`
//!   (`-fintegrated-cc1 -c` writes the object to `-o` and no temporary file);
//! - the budget `NATIVE_BUDGET.of(deadline)`, the deadline the caller's for each compile;
//! - the read list: clang's own depfile (`-MD -MF`), parsed by [`Depfile`] and checked by the
//!   hermetic run against the roots: a read outside them fails the task by name.
//!
//! The process is started only by the passed-in [`Hermetic`] (its spawner and clock are the
//! command's: rule 75).
//!
//! The argv is the unit's cooked (guest) argv with each guest root's spelling replaced by its
//! host folder, then `-ffile-prefix-map=<host>=<guest>` for every root and the task folder
//! (`__FILE__`, the debug info's file names and its compilation folder, which is the task
//! folder, the run's cwd, shown as the guest `OUT_DIR`), so two checkouts at different folders
//! write equal bytes (rule 25). The depfile names host paths; it is never in the object, and
//! its reads are made guest paths before any key sees them.

use core::time::Duration;
use std::io::Read as _;

use blaze_build_script::c_tasks::{Area, CCompiler, Compile, CompilerKind, Compiled, ERROR_LINES, FactError, FileRef, GuestRoots, Lang, Refused, StoreFailed, Today};
use blaze_build_script::outputs::{FileLimits, Outputs};
use blaze_core::lock::Lock;
use blaze_core::wait::Deadline;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_roots::{Digest, Mount, RelPath, Root, RootId, RootKind, Roots, Seen};
use bstd::procs::{
    Exit,
    hermetic::{
        Capture, ClockSpec, Fault, FilesError, FilesOp, Hermetic, HermeticOpts, HermeticSpec, HermeticStreams, Level, Program, ReportError,
        ReportParser, ReportSpec, RootFiles,
    },
};

use crate::native_script::{NATIVE_BUDGET, OutDir, listed_under, store_out_dir};

/// The longest host path a fact is asked of: macOS's `PATH_MAX`. A longer one is refused by
/// name ([`TOO_LONG`]), never cut.
const PATH_MOST: usize = 1024;

/// [`FactError::code`]s that are no OS error: a guest path under no laid root.
pub const NOT_LAID: u16 = u16::MAX;
/// The facts' memo was not had within [`LOCK_MOST`] (rule 18).
pub const BUSY: u16 = u16::MAX - 1;
/// A folder's names were not in byte order (listed_under's contract broken).
pub const UNSORTED: u16 = u16::MAX - 2;
/// A host path longer than [`PATH_MOST`].
pub const TOO_LONG: u16 = u16::MAX - 3;

/// How long a fact's memo is waited for.
const LOCK_MOST: Duration = Duration::from_secs(10);

/// The depfile's name in the task's folder.
const DEPFILE: &str = ".c-task.d";

/// Standard output and error kept of one compile.
const MOST_STREAM: u32 = 1 << 20;

/// One guest root laid at a host folder.
#[derive(Clone, Copy, Debug)]
pub struct Laid<'l> {
    /// The guest spelling (`/package`, the script's `OUT_DIR`, a dependency's `OUT_DIR`, the
    /// guest sysroot, the shelf's).
    pub guest: &'l str,
    /// The resolved host folder (Seatbelt matches resolved paths), canonical.
    pub host: &'l str,
    /// What names its tree root: the package tree's digest, the script record's key, the SDK's
    /// identity, the shelf's digest. Nothing beneath it changes while this is the same.
    pub digest: [u8; 32],
    /// The root's name in errors and in the roots' identity, never a host path.
    pub name: &'l str,
}

/// `parts` joined into `buf`, or `None` past [`PATH_MOST`].
fn join<'b>(buf: &'b mut [u8; PATH_MOST], parts: &[&str]) -> Option<&'b str> {
    let mut len = 0;
    for part in parts {
        let end = len + part.len();
        buf.get_mut(len..end)?.copy_from_slice(part.as_bytes());
        len = end;
    }
    core::str::from_utf8(&buf[..len]).ok()
}

/// The laid root `guest` is under and the rest of it (empty, or starting with `/`).
fn laid_of<'l, 'g>(laid: impl IntoIterator<Item = Laid<'l>>, guest: &'g str) -> Option<(Laid<'l>, &'g str)> {
    laid.into_iter().find_map(|l| guest.strip_prefix(l.guest).filter(|rest| rest.is_empty() || rest.starts_with('/')).map(|rest| (l, rest)))
}

// ---------------------------------------------------------------------------------------------
// Today's facts of the host's files

/// Today's facts of host files: the digest (BLAKE3) and length of a file, the digest of a
/// folder's names. One per command, shared by every compile and every look-up, so a header
/// read by a thousand objects is hashed once.
pub trait HostFacts {
    fn file(&self, host: &str) -> Result<Option<(Digest, u64)>, u16>;
    /// The digest of every name under `host`, every depth, in byte order (`listed_under`'s
    /// names, `blaze_roots::listing_digest`); `None` when there is no folder.
    fn listing(&self, host: &str) -> Result<Option<Digest>, u16>;
}

#[derive(Clone, Copy, Debug)]
enum Fact {
    File(Option<(Digest, u64)>),
    Listing(Option<Digest>),
}

/// [`HostFacts`] read from disk, each kept for the command in a sorted memo in the caller's
/// allocator.
pub struct DiskFacts<'a> {
    memo: Lock<AVec<'a, (ContentId, Fact)>>,
}

impl<'a> DiskFacts<'a> {
    pub fn new(alloc: Alloc<'a>) -> DiskFacts<'a> {
        DiskFacts { memo: Lock::new("native_c.facts", Vec::new_in(alloc)) }
    }

    fn held(&self, key: ContentId) -> Result<Option<Fact>, u16> {
        let memo = self.memo.try_lock_for(LOCK_MOST).ok_or(BUSY)?;
        Ok(memo.binary_search_by(|(k, _)| k.0.cmp(&key.0)).ok().map(|at| memo[at].1))
    }

    fn keep(&self, key: ContentId, fact: Fact) -> Result<(), u16> {
        let mut memo = self.memo.try_lock_for(LOCK_MOST).ok_or(BUSY)?;
        if let Err(at) = memo.binary_search_by(|(k, _)| k.0.cmp(&key.0)) {
            memo.insert(at, (key, fact));
        }
        Ok(())
    }
}

fn os_code(error: &std::io::Error) -> u16 {
    error.raw_os_error().and_then(|code| u16::try_from(code).ok()).unwrap_or(0)
}

impl HostFacts for DiskFacts<'_> {
    fn file(&self, host: &str) -> Result<Option<(Digest, u64)>, u16> {
        let key = ContentId::of("native_c host file", host.as_bytes());
        if let Some(Fact::File(fact)) = self.held(key)? {
            return Ok(fact);
        }
        let fact = match std::fs::File::open(host) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(os_code(&error)),
            Ok(mut opened) => {
                let mut hasher = bstd::digest::Blake3::new();
                let mut buf = [0u8; 64 << 10];
                let mut len = 0u64;
                loop {
                    let n = opened.read(&mut buf).map_err(|e| os_code(&e))?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                    len += n as u64;
                }
                Some((Digest(hasher.finalize()), len))
            }
        };
        self.keep(key, Fact::File(fact))?;
        Ok(fact)
    }

    fn listing(&self, host: &str) -> Result<Option<Digest>, u16> {
        let key = ContentId::of("native_c host listing", host.as_bytes());
        if let Some(Fact::Listing(fact)) = self.held(key)? {
            return Ok(fact);
        }
        let fact = match std::fs::metadata(host) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(os_code(&error)),
            Ok(meta) if !meta.is_dir() => None,
            Ok(_) => {
                let listed = listed_under(host).map_err(|_| 0u16)?;
                Some(blaze_roots::listing_digest(listed.iter().map(|l| l.name.as_str())).map_err(|_| UNSORTED)?)
            }
        };
        self.keep(key, Fact::Listing(fact))?;
        Ok(fact)
    }
}

/// c_tasks' [`Today`] over [`HostFacts`]: each guest path at its laid host folder.
pub struct GuestToday<'t> {
    pub laid: &'t [Laid<'t>],
    pub facts: &'t dyn HostFacts,
}

impl GuestToday<'_> {
    fn host<'b>(&self, guest: &str, buf: &'b mut [u8; PATH_MOST]) -> Result<&'b str, FactError> {
        let (laid, rest) = laid_of(self.laid.iter().copied(), guest).ok_or(FactError { code: NOT_LAID })?;
        join(buf, &[laid.host, rest]).ok_or(FactError { code: TOO_LONG })
    }
}

impl Today for GuestToday<'_> {
    fn file(&self, path: &str) -> Result<Option<[u8; 32]>, FactError> {
        let mut buf = [0u8; PATH_MOST];
        let host = self.host(path, &mut buf)?;
        Ok(self.facts.file(host).map_err(|code| FactError { code })?.map(|(digest, _)| digest.0))
    }

    fn listing(&self, path: &str) -> Result<Option<[u8; 32]>, FactError> {
        let mut buf = [0u8; PATH_MOST];
        let host = self.host(path, &mut buf)?;
        Ok(self.facts.listing(host).map_err(|code| FactError { code })?.map(|digest| digest.0))
    }
}

// ---------------------------------------------------------------------------------------------
// The file area

/// The home's file area (`<area>/<sha256 hex>`, scripts.rs `file_area`) as c_tasks' [`Area`]:
/// an archive's bytes written beside the area and renamed in; read back by name.
pub struct FileArea<'f> {
    /// Canonical, existing.
    pub folder: &'f str,
}

fn hex(digest: &[u8; 32]) -> [u8; 64] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (at, byte) in digest.iter().enumerate() {
        out[2 * at] = HEX[usize::from(byte >> 4)];
        out[2 * at + 1] = HEX[usize::from(byte & 15)];
    }
    out
}

impl FileArea<'_> {
    fn path<'b>(&self, sha256: &[u8; 32], suffix: &str, buf: &'b mut [u8; PATH_MOST]) -> Result<&'b str, StoreFailed> {
        let name = hex(sha256);
        let name = core::str::from_utf8(&name).map_err(|_| StoreFailed)?;
        join(buf, &[self.folder, "/", name, suffix]).ok_or(StoreFailed)
    }
}

impl Area for FileArea<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<FileRef, StoreFailed> {
        let mut sha = blaze_sha256::Sha256::new();
        sha.update(bytes);
        let file = FileRef { sha256: sha.finish(), len: bytes.len() as u64 };
        let mut at = [0u8; PATH_MOST];
        let at = self.path(&file.sha256, "", &mut at)?;
        if std::fs::metadata(at).is_ok() {
            return Ok(file);
        }
        // Written beside its name, then renamed: a reader never sees part of it.
        let mut part = [0u8; PATH_MOST];
        let part = self.path(&file.sha256, ".part", &mut part)?;
        std::fs::write(part, bytes).map_err(|_| StoreFailed)?;
        std::fs::rename(part, at).map_err(|_| StoreFailed)?;
        Ok(file)
    }

    fn read(&self, file: FileRef, into: &mut AVec<'_, u8>) -> Result<(), StoreFailed> {
        let mut at = [0u8; PATH_MOST];
        let at = self.path(&file.sha256, "", &mut at)?;
        let mut opened = std::fs::File::open(at).map_err(|_| StoreFailed)?;
        let len = usize::try_from(file.len).map_err(|_| StoreFailed)?;
        into.clear();
        into.resize(len, 0);
        opened.read_exact(into).map_err(|_| StoreFailed)?;
        // One byte more would be a file other than the one named.
        if opened.read(&mut [0u8; 1]).map_err(|_| StoreFailed)? != 0 {
            return Err(StoreFailed);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The run's files and report

/// clang's `-MD` depfile as the hermetic run's read report: every word after the first `: `,
/// line continuations skipped. blaze-build-script toolchain.rs `depfile_reads` owns its words
/// with escapes undone; the report gives borrowed words, so a word holding an escape (`\ `,
/// `\#`) or a `$$` is refused here by its byte (`ReportError::Malformed`): no pinned crate's
/// paths hold a space, `#` or `$`.
pub struct Depfile;

impl ReportParser for Depfile {
    fn paths<'t>(&self, text: &'t [u8], each: &mut dyn FnMut(&'t [u8]) -> Result<(), Fault>) -> Result<(), Fault> {
        // No depfile written (CFiles `read_output`): no reads; `compile` refuses an exit 0 so.
        if text.is_empty() {
            return Ok(());
        }
        let Some(colon) = text.windows(2).position(|w| w == b": ") else {
            return Err(Fault::Report(ReportError::Malformed { at: 0 }));
        };
        let mut at = colon + 1;
        while at < text.len() {
            match text[at] {
                b' ' | b'\t' | b'\n' | b'\r' => at += 1,
                b'\\' if matches!(text.get(at + 1), Some(b'\n' | b'\r')) => at += 2,
                _ => {
                    let end = text[at..].iter().position(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r')).map_or(text.len(), |n| at + n);
                    let word = &text[at..end];
                    if let Some(bad) = word.iter().position(|b| matches!(b, b'\\' | b'$')) {
                        return Err(Fault::Report(ReportError::Malformed { at: (at + bad) as u32 }));
                    }
                    each(word)?;
                    at = end;
                }
            }
        }
        Ok(())
    }
}

/// The run's files: each tree root's facts from [`HostFacts`], the depfile read from the task
/// folder, the task folder listed and cleared as native_script.rs's `OUT_DIR` is.
struct CFiles<'f> {
    /// Each tree root's host folder, by its [`RootId`].
    folders: &'f [&'f str],
    out: OutDir<'f>,
    facts: &'f dyn HostFacts,
}

impl RootFiles for CFiles<'_> {
    fn package_files(&self, root: RootId, _: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        Err(Fault::Files(FilesError { op: FilesOp::PackageFiles, root, code: 0 }))
    }

    fn seen(&self, root: RootId, path: RelPath<'_>) -> Result<Seen, Fault> {
        let refused = |code: u16| Fault::Files(FilesError { op: FilesOp::Seen, root, code });
        let folder = self.folders.get(root.index()).copied().ok_or(refused(NOT_LAID))?;
        let mut buf = [0u8; PATH_MOST];
        let host = join(&mut buf, &[folder, "/", path.as_str()]).ok_or(refused(TOO_LONG))?;
        Ok(match self.facts.file(host).map_err(refused)? {
            Some((digest, len)) => Seen::File { digest, len },
            None => Seen::Absent,
        })
    }

    fn read_output(&self, path: RelPath<'_>, most: u32, into: &mut AVec<'_, u8>) -> Result<(), Fault> {
        let refused = |code: u16| Fault::Files(FilesError { op: FilesOp::ReadOutput, root: self.out.root, code });
        let mut buf = [0u8; PATH_MOST];
        let host = join(&mut buf, &[self.out.folder, "/", path.as_str()]).ok_or(refused(TOO_LONG))?;
        // The run reads the report on every exit; a clang that stopped before its depfile (an
        // output it could not open, an argument it refused) left none. An absent depfile is an
        // empty report, so the run ends and clang's own error is what the compile says
        // (`Compiled::Failed`); an exit 0 with no read is refused by name in `compile`.
        let mut opened = match std::fs::File::open(host) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path.as_str() == DEPFILE => {
                into.clear();
                return Ok(());
            }
            opened => opened.map_err(|e| refused(os_code(&e)))?,
        };
        let len = opened.metadata().map_err(|e| refused(os_code(&e)))?.len();
        if len > u64::from(most) {
            return Err(refused(0));
        }
        into.clear();
        into.resize(len as usize, 0);
        opened.read_exact(into).map_err(|e| refused(os_code(&e)))
    }

    fn list_output(&self, each: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        self.out.list_output(each)
    }

    fn clear_output(&self) -> Result<(), Fault> {
        self.out.clear_output()
    }
}

// ---------------------------------------------------------------------------------------------
// The compiler

/// What every compile of one crate runs against. One per worker: `task_out` is its own.
pub struct ShelfClang<'s> {
    pub hermetic: &'s Hermetic,
    pub cx: &'s bstd::cx::Cx,
    /// The command's clock: each compile's deadline starts at its start.
    pub clock: &'s bstd::clock::Clock,
    /// `<shelf>/bin/clang-23`, canonical.
    pub clang: &'s str,
    /// The shelf (its digest is the compiler's identity in every object's key) and the SDK:
    /// read, never in a read list (the toolchain's identity covers them).
    pub shelf: Laid<'s>,
    pub sdk: Laid<'s>,
    /// The package, the script's `OUT_DIR` as its record laid, each dependency's laid
    /// `OUT_DIR`: the roots a read list names.
    pub sources: &'s [Laid<'s>],
    pub roots: GuestRoots<'s>,
    /// This worker's task folder, canonical, emptied before each compile.
    pub task_out: &'s str,
    /// The home's file area, canonical, on the task folder's volume.
    pub area: &'s str,
    pub facts: &'s dyn HostFacts,
    /// Each compile's deadline (rule 18).
    pub deadline: Duration,
}

/// The dependency-file flags of a unit's argv: the run passes its own.
const DEPFILE_FLAGS: &[&str] = &["-MD", "-MMD", "-MP"];
const DEPFILE_VALUED: &[&str] = &["-MF", "-MT", "-MQ"];

/// The prefix-map flags: only the folder before the `=` is a path the compiler reads.
const PREFIX_MAPS: &[&str] = &["-ffile-prefix-map=", "-fdebug-prefix-map=", "-fmacro-prefix-map=", "-fcoverage-prefix-map="];

/// Flags whose joined value is a path.
const JOINED: &[&str] = &["-isystem", "-iquote", "-idirafter", "-include", "-imacros", "-isysroot", "-I"];

impl<'s> ShelfClang<'s> {
    /// Every laid root, in [`RootId`] order: the sources, then the SDK and the shelf.
    fn laid(&self) -> impl Iterator<Item = Laid<'s>> + '_ {
        self.sources.iter().copied().chain([self.sdk, self.shelf])
    }

    /// The guest path `guest` at its host folder.
    fn host<'a>(&self, guest: &str, alloc: Alloc<'a>) -> Result<AString<'a>, Refused<'a>> {
        let Some((laid, rest)) = laid_of(self.laid(), guest) else {
            return Err(refused(alloc, &["a path under no laid root: ", guest]));
        };
        let mut out = String::from_str_in(laid.host, alloc);
        out.push_str(rest);
        Ok(out)
    }

    /// One argument with its guest paths made host paths.
    fn host_arg<'a>(&self, arg: &str, alloc: Alloc<'a>) -> Result<AString<'a>, Refused<'a>> {
        if arg.starts_with('/') {
            return self.host(arg, alloc);
        }
        if let Some(flag) = PREFIX_MAPS.iter().find(|flag| arg.starts_with(**flag)) {
            let (from, to) = arg[flag.len()..].split_once('=').unwrap_or((&arg[flag.len()..], ""));
            let mut out = String::from_str_in(flag, alloc);
            if from.starts_with('/') {
                let host = self.host(from, alloc)?;
                out.push_str(host.as_str());
            } else {
                out.push_str(from);
            }
            out.push('=');
            out.push_str(to);
            return Ok(out);
        }
        if arg.contains('=') {
            let mut out = String::new_in(alloc);
            for (at, part) in arg.split('=').enumerate() {
                if at > 0 {
                    out.push('=');
                }
                match part.starts_with('/') {
                    true => out.push_str(self.host(part, alloc)?.as_str()),
                    false => out.push_str(part),
                }
            }
            return Ok(out);
        }
        if let Some(flag) = JOINED.iter().find(|flag| arg.len() > flag.len() && arg.starts_with(**flag) && arg[flag.len()..].starts_with('/')) {
            let mut out = String::from_str_in(flag, alloc);
            out.push_str(self.host(&arg[flag.len()..], alloc)?.as_str());
            return Ok(out);
        }
        Ok(String::from_str_in(arg, alloc))
    }

    /// `text` with each laid host folder (and the task folder) shown as its guest spelling.
    fn shown<'a>(&self, text: &str, alloc: Alloc<'a>) -> AString<'a> {
        let mut shown = String::from_str_in(text, alloc);
        for laid in self.laid() {
            shown = replace_all(shown.as_str(), laid.host, laid.guest, alloc);
        }
        replace_all(shown.as_str(), self.task_out, self.roots.out, alloc)
    }
}

/// The flags a source's language adds to every compile of it (see `compile`): `.S` keeps its
/// preprocessed text in the task folder, never in a temporary folder the grant does not give.
fn lang_flags(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::AsmCpp => &["-save-temps=obj"],
        Lang::C | Lang::Cxx | Lang::Asm => &[],
    }
}

fn refused<'a>(alloc: Alloc<'a>, parts: &[&str]) -> Refused<'a> {
    let mut words = String::new_in(alloc);
    for part in parts {
        words.push_str(part);
    }
    Refused { words }
}

impl CCompiler for ShelfClang<'_> {
    const KIND: CompilerKind = CompilerKind::ShelfClang;

    fn compile<'a>(&mut self, unit: &Compile<'_, '_>, alloc: Alloc<'a>) -> Result<Compiled<'a>, Refused<'a>> {
        let object_name = unit.output.rsplit('/').next().unwrap_or(unit.output);
        let mut held: AVec<'a, AString<'a>> = Vec::new_in(alloc);
        let text = |parts: &[&str]| {
            let mut out = String::new_in(alloc);
            for part in parts {
                out.push_str(part);
            }
            out
        };
        held.push(text(&["-fintegrated-cc1"]));
        held.push(text(&["--no-default-config"]));
        let mut skip_value = false;
        let mut has_sysroot = false;
        for arg in unit.args.iter().copied() {
            if core::mem::take(&mut skip_value) {
                continue;
            }
            if DEPFILE_FLAGS.contains(&arg) {
                continue;
            }
            if DEPFILE_VALUED.contains(&arg) {
                skip_value = true;
                continue;
            }
            if DEPFILE_VALUED.iter().any(|flag| arg.starts_with(flag)) {
                continue;
            }
            has_sysroot |= arg.starts_with("-isysroot") || arg.starts_with("--sysroot");
            held.push(self.host_arg(arg, alloc)?);
        }
        if !has_sysroot {
            held.push(text(&["-isysroot"]));
            held.push(text(&[self.sdk.host]));
        }
        // The maps last: a later map wins where two match (clang applies them last first).
        for laid in self.laid() {
            held.push(text(&["-ffile-prefix-map=", laid.host, "=", laid.guest]));
        }
        held.push(text(&["-ffile-prefix-map=", self.task_out, "=", self.roots.out]));
        let depfile = text(&[self.task_out, "/", DEPFILE]);
        held.push(text(&["-MD"]));
        held.push(text(&["-MF"]));
        held.push(depfile.clone());
        // Assembly through the preprocessor (`.S`: aws-lc's, ring's) is two jobs in one clang:
        // the preprocessor writes its text to a TEMPORARY file, which the assembler reads. With
        // no `TMPDIR` that file is under the user's temporary folder, which the grant does not
        // let it write, so the preprocessor failed and wrote no depfile (the run's ReadOutput of
        // `.c-task.d`, ENOENT). `-save-temps=obj` names it `<task>/<source stem>.s` instead: in
        // the folder it may write, under a name of the source's (a random temporary name would
        // be in the object's debug info: rule 25), mapped to the guest `OUT_DIR` by the prefix
        // map below; it is not stored (only the object is).
        for flag in lang_flags(unit.lang) {
            held.push(text(&[flag]));
        }
        held.push(text(&["-c"]));
        held.push(self.host(unit.source, alloc)?);
        held.push(text(&["-o"]));
        // Flat in the task folder: cc-rs's objects are in a folder of `OUT_DIR` the run does not
        // make; the object's path in the archive is its file name alone.
        held.push(text(&[self.task_out, "/", object_name]));
        let mut args: AVec<'a, &str> = Vec::with_capacity_in(held.len(), alloc);
        args.extend(held.iter().map(|a| a.as_str()));

        // The roots: each laid folder a tree root by its digest; the task folder the output.
        let mut roots: AVec<'a, Root<'_>> = Vec::new_in(alloc);
        let mut mounts: AVec<'a, Mount<'_>> = Vec::new_in(alloc);
        let mut folders: AVec<'a, &str> = Vec::new_in(alloc);
        for laid in self.laid() {
            let at = u8::try_from(roots.len()).map_err(|_| refused(alloc, &["more than 255 folders laid"]))?;
            roots.push(Root { name: laid.name, kind: RootKind::Tree { tree: Digest(laid.digest) } });
            mounts.push(Mount { spelling: laid.host.as_bytes(), root: RootId(at) });
            folders.push(laid.host);
        }
        let output = RootId(u8::try_from(roots.len()).map_err(|_| refused(alloc, &["more than 255 folders laid"]))?);
        roots.push(Root { name: "a C task's folder", kind: RootKind::Output });
        mounts.push(Mount { spelling: self.task_out.as_bytes(), root: output });
        let roots = Roots::new(&roots).map_err(|_| refused(alloc, &["the roots of a C task were refused"]))?;

        let files = CFiles { folders: &folders, out: OutDir { root: output, folder: self.task_out, package_root: None }, facts: self.facts };
        files.clear_output().map_err(|_| refused(alloc, &["the task folder could not be emptied: ", self.task_out]))?;
        let env = [("SOURCE_DATE_EPOCH", "0")];
        let report = RelPath::new(DEPFILE, blaze_roots::Limits::SHIPPED.most_path).map_err(|_| refused(alloc, &["the depfile's name"]))?;
        let spec = HermeticSpec::new(HermeticOpts {
            program: Program::Stored { name: "clang", digest: Digest(self.shelf.digest), path: self.clang },
            args: &args,
            env: &env,
            roots,
            mounts: &mounts,
            cwd: self.task_out,
            helpers: &[],
            helper_folders: &[],
            channels: &[],
            clock: ClockSpec::Fixed { unix_secs: 0 },
            seed: 0,
            budget: NATIVE_BUDGET.of(self.deadline),
            deadline: Deadline::after(self.clock.now().since_start(), self.deadline),
            streams: HermeticStreams { stdout: Capture::Keep { most: MOST_STREAM }, stderr: Capture::Keep { most: MOST_STREAM } },
            level: Level::Inputs,
            report: ReportSpec::Program { file: report, parser: &Depfile },
        })
        .map_err(|error| {
            let mut words = String::from_str_in("the C task's spec was refused: ", alloc);
            _ = core::fmt::Write::write_fmt(&mut words, format_args!("{:?} ({:?})", error.fault, error.rule));
            Refused { words }
        })?;
        let ran = self.hermetic.run(self.cx, &spec, &files, alloc).map_err(|error| {
            let mut words = String::from_str_in("the compile was stopped or refused: ", alloc);
            _ = core::fmt::Write::write_fmt(&mut words, format_args!("{:?} ({:?})", error.fault, error.rule));
            Refused { words }
        })?;
        if ran.exit != Exit::Exited(0) {
            let status = match ran.exit {
                Exit::Exited(code) => code,
                Exit::Signaled(signal) => -signal,
            };
            let stderr = core::str::from_utf8(&ran.stderr).unwrap_or("(standard error is not UTF-8)");
            let mut first_lines = String::new_in(alloc);
            for line in stderr.lines().take(ERROR_LINES) {
                first_lines.push_str(self.shown(line, alloc).as_str());
                first_lines.push('\n');
            }
            return Ok(Compiled::Failed { status, first_lines });
        }
        // The reads as guest paths; the SDK's and the shelf's are covered by the toolchain's
        // identity in the pre key.
        let mut reads = Vec::new_in(alloc);
        for entry in ran.reads.checked().entries() {
            let Some(laid) = self.sources.get(entry.root.index()) else { continue };
            reads.push(text(&[laid.guest, "/", entry.path.as_str()]));
        }
        // A compile reads its source at least: none read is a clang that wrote no depfile and
        // still exited 0, whose object no read list could find again (rule 3).
        if reads.is_empty() {
            return Err(refused(alloc, &["clang exited 0 but wrote no depfile naming its reads: ", unit.source]));
        }
        // The depfile was the report; the object alone goes to the file area, renamed in.
        std::fs::remove_file(depfile.as_str()).map_err(|_| refused(alloc, &["the depfile could not be removed: ", depfile.as_str()]))?;
        let mut listed = listed_under(self.task_out).map_err(|e| refused(alloc, &["the task folder could not be listed: ", &e.to_string()]))?;
        // The object alone: `-save-temps=obj`'s preprocessed text is the compile's own step.
        listed.retain(|entry| entry.name.as_str() == object_name);
        let mut stored = Outputs::new_in(self.roots.out, alloc);
        store_out_dir(self.task_out, self.area, &listed, &mut stored, FileLimits { bytes: u64::MAX, files: 1 }, alloc)
            .map_err(|e| refused(alloc, &["the object could not be stored: ", &e.to_string()]))?;
        let Some(object) = stored.stored.iter().find(|file| file.name.as_str() == object_name) else {
            return Err(refused(alloc, &["the compile left no object named ", object_name]));
        };
        Ok(Compiled::Made { object: FileRef { sha256: object.sha256, len: object.len }, reads })
    }
}

fn replace_all<'a>(text: &str, from: &str, to: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    let mut rest = text;
    while let Some(at) = rest.find(from) {
        out.push_str(&rest[..at]);
        out.push_str(to);
        rest = &rest[at + from.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::{Depfile, Laid, PATH_MOST, join, laid_of, lang_flags};
    use blaze_build_script::c_tasks::Lang;
    use bstd::procs::hermetic::ReportParser;

    /// aws-lc's and ring's `.S`: clang's preprocessor writes a temporary file the assembler reads;
    /// in the grant's world only the task folder may be written, so `.S` names it there. Twin:
    /// a C source makes no temporary and adds nothing (its argv, and so its object, unchanged).
    #[test]
    fn preprocessed_assembly_keeps_its_temporary_in_the_task_folder() {
        assert_eq!(lang_flags(Lang::AsmCpp), ["-save-temps=obj"]);
        assert!(lang_flags(Lang::C).is_empty() && lang_flags(Lang::Cxx).is_empty() && lang_flags(Lang::Asm).is_empty());
        // A clang that stopped before its depfile: an empty report, so its own error is said.
        assert!(Depfile.paths(b"", &mut |_| panic!("no read")).is_ok());
    }

    #[test]
    fn a_depfile_gives_each_path() {
        let text = b"/t/a.o: /p/src/a.c /p/include/h.h \\\n  /sdk/usr/include/stdio.h\n";
        let mut seen = std::vec::Vec::new();
        Depfile.paths(text, &mut |word| {
            seen.push(core::str::from_utf8(word).expect("text").to_owned());
            Ok(())
        })
        .expect("parses");
        assert_eq!(seen, ["/p/src/a.c", "/p/include/h.h", "/sdk/usr/include/stdio.h"]);
        // An escaped space is refused by its byte, never split or guessed.
        assert!(Depfile.paths(b"/t/a.o: /p/a\\ b.h\n", &mut |_| Ok(())).is_err());
    }

    #[test]
    fn a_guest_path_finds_its_laid_root() {
        let laid = [
            Laid { guest: "/package", host: "/Users/a/tempo/x", digest: [1; 32], name: "package" },
            Laid { guest: "/target/debug/build/x-1/out", host: "/w/out/k", digest: [2; 32], name: "OUT_DIR" },
        ];
        let at = |guest: &'static str| laid_of(laid, guest).map(|(l, rest)| (l.name, rest));
        assert_eq!(at("/package/src/a.c"), Some(("package", "/src/a.c")));
        assert_eq!(at("/packages/a.c"), None);
        assert_eq!(at("/target/debug/build/x-1/out"), Some(("OUT_DIR", "")));
        let mut buf = [0u8; PATH_MOST];
        assert_eq!(join(&mut buf, &["/w", "/a"]), Some("/w/a"));
        let long = "x".repeat(PATH_MOST);
        assert_eq!(join(&mut buf, &["/", &long]), None);
    }
}
