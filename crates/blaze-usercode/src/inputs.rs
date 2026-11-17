//! What a run's reads are hashed as ([`crate::trace::Inputs`]): the machine's files as they
//! are on disk ([`Disk`]), for the launcher as a run ends and for paths outside every view
//! (a toolchain, the C compiler, `/usr/include`: #1480); and a view's files through the query
//! database ([`ViewInputs`]), which records each as an edge of the query that checks them.
//!
//! The machine's files are the one thing of user code read outside a view: they are hashed
//! once per change of what `stat` says of them, in one table every check of the process shares
//! ([`Files`]), and a file that is there and can't be read is an error naming it, never left
//! out. Every check is one `stat` (an `lstat` for an entry or a listing) and nothing else while
//! the stamp holds ([`blaze_vfs::outside::holds`]): the floor, since nothing watches a
//! directory outside a checkout. A file is read again only when its stamp moved or was racy.

use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use blaze_core::Stamp;
use blaze_ids::{ContentId, Hasher, Interner, Sharded};
use blaze_mem::{Tag, frame};
use blaze_query::{Cancelled, Cx};
use blaze_vfs::outside::{LookKind, Looks};

use crate::trace::{DIRECTORY, Entry, Inputs, Names, entry_digest, listing, os_bytes};

/// What a traced path that is neither a file nor a directory hashes as.
pub const SPECIAL: [u8; 32] = *b"blaze v2 traced: not a file\0\0\0\0\0";

/// A path that is there and could not be read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unreadable {
    pub path: PathBuf,
    pub error: String,
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is an input of a build script or proc macro and can't be read: {}",
            self.path.display(),
            self.error
        )
    }
}

/// A stamp's bytes as part of a key ([`Stamp::encode`]'s layout), on the stack.
fn stamp_bytes(stamp: &Stamp) -> [u8; Stamp::BYTES] {
    let mut out = [0; Stamp::BYTES];
    out[..8].copy_from_slice(&stamp.ino.to_le_bytes());
    out[8..16].copy_from_slice(&stamp.size.to_le_bytes());
    out[16..24].copy_from_slice(&stamp.mtime_ns.to_le_bytes());
    out[24..32].copy_from_slice(&stamp.ctime_ns.to_le_bytes());
    out[32..].copy_from_slice(&stamp.mode.to_le_bytes());
    out
}

/// The wall clock, the one a kernel stamps a write with ([`blaze_core::system_wall_ns`]).
fn wall_ns() -> u64 {
    blaze_core::system_wall_ns()
}

/// The stamp of a metadata, as every driver makes one: the one definition
/// ([`blaze_vfs::outside::stamp_of`]).
fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    blaze_vfs::outside::stamp_of(meta)
}

/// When a run began, for asking whether what it read can have moved since ([`Began::moved`]).
/// A run's reads are hashed when its trace is assembled, after it ends: a file written between
/// the run's read and that hash would be kept under bytes the run never saw, and a pass kept
/// under them reported cached for an input that can fail it. What changed after the assembly
/// is the next check's to find; what changed before it is this one's.
#[derive(Clone, Copy, Debug)]
pub struct Began {
    /// The wall clock ([`wall_ns`]) before the run, as a kernel stamps a write.
    wall_ns: u64,
}

impl Began {
    /// Now, read before a run starts.
    pub fn now() -> Began {
        Began { wall_ns: wall_ns() }
    }

    /// The first thing `traced` read that can have changed since the run began, so the hashes
    /// it was assembled with need not be what the run read; `None` when nothing can have.
    /// Each path is asked of by every name its answer rests on ([`names`]): the name and
    /// every directory above it are `lstat`ed, and one whose last change can share a tick
    /// with the run's start or come after it ([`blaze_vfs::racy`]) has moved. A path whose names
    /// can't be told has moved: it is never taken as still. Conservative: a change beside the
    /// name in its directory, or the run's own write of what it read, counts as a move, and
    /// the pass is not kept, never a stale one kept.
    pub fn moved(&self, traced: &blaze_core::Traced) -> Option<Moved> {
        // A `stat` of each name the watches can't vouch for, and of every directory above it:
        // a row naming how many paths were asked, so its time is never unnamed.
        let _span = tracing::info_span!(
            target: "blaze",
            "ask whether what a run read changed since it began",
            files = traced.files.len() as u64,
            dirs = traced.dirs.len() as u64,
        )
        .entered();
        let files = traced.files.iter().map(|(path, answer)| (path, *answer));
        let dirs = traced.dirs.iter().map(|(path, _)| (path, Some(DIRECTORY)));
        files.chain(dirs).find_map(|(path, answer)| self.path_moved(path, answer))
    }

    fn path_moved(&self, path: &Path, answer: Option<[u8; 32]>) -> Option<Moved> {
        let Some(names) = names(path, answer) else {
            tracing::info!(
                target: "blaze",
                read = %path.display(),
                "a run's read is taken as changed since the run began: the names its answer rests on can't be told"
            );
            // alloc: once per run not kept, the path its line names
            return Some(Moved { read: path.to_path_buf(), at: path.to_path_buf(), why: MovedWhy::Untold });
        };
        let last = names.len() - 1; // `names` ends with the name the path leads to
        names.iter().enumerate().find_map(|(n, name)| {
            let at = stamped_since(name, self.wall_ns)?;
            let why = MovedWhy::of(path, name, n == last, at);
            tracing::info!(
                target: "blaze",
                read = %path.display(),
                at = %at.display(),
                why = why.word(),
                "a run's read is taken as changed since the run began: this name's stamp can share a tick with the run's start or is after it, or it can't be asked"
            );
            // alloc: once per run not kept, the paths its line names
            Some(Moved { read: path.to_path_buf(), at: at.to_path_buf(), why })
        })
    }
}

/// What a run read that can have changed since it began ([`Began::moved`]): the one answer
/// to "what moved", for a build script's run kept under no key and a test's pass not kept
/// alike, each saying it in its own words ([`Moved::said`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Moved {
    /// The path the run read, as its trace names it.
    pub read: PathBuf,
    /// The name whose stamp says it can have changed: `read` itself for [`MovedWhy::Itself`]
    /// and [`MovedWhy::Untold`].
    pub at: PathBuf,
    /// How `at` stands to `read`.
    pub why: MovedWhy,
}

/// How the name a stamp said moved stands to the path a run read ([`Moved`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MovedWhy {
    /// The path read itself.
    Itself,
    /// A directory above the path read, in its own name.
    Above,
    /// A link the path read is named through.
    Link,
    /// What a link on the way to the path read leads to, or a directory above that.
    Through,
    /// The names the read's answer rests on can't be told (a path that is not absolute,
    /// has `..` in it, more than [`MOST_LINKS`] links or a part that can't be read): it is
    /// taken as changed.
    Untold,
}

impl MovedWhy {
    /// How `at`, the name [`stamped_since`] found of `name` (the last of [`names`] of `path`
    /// when `last`), stands to `path`.
    fn of(path: &Path, name: &Path, last: bool, at: &Path) -> MovedWhy {
        if at == path {
            MovedWhy::Itself
        } else if !last && at == name {
            MovedWhy::Link
        } else if path.starts_with(at) {
            MovedWhy::Above
        } else {
            MovedWhy::Through
        }
    }

    /// As one word, for an event or a profile row.
    pub fn word(self) -> &'static str {
        match self {
            MovedWhy::Itself => "itself",
            MovedWhy::Above => "above",
            MovedWhy::Link => "link",
            MovedWhy::Through => "through",
            MovedWhy::Untold => "untold",
        }
    }
}

impl Moved {
    /// What moved, as a clause of a sentence about `who` (`the test`, `the run`): "/a/b,
    /// above /a/b/c which it read, changed after the test began".
    pub fn said<'a>(&'a self, who: &'a str) -> impl fmt::Display + 'a {
        Said { moved: self, who }
    }
}

struct Said<'a> {
    moved: &'a Moved,
    who: &'a str,
}

impl fmt::Display for Said<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Said { moved: Moved { read, at, why }, who } = self;
        let (read, at) = (read.display(), at.display());
        match why {
            MovedWhy::Itself => write!(f, "{read} changed after {who} began"),
            MovedWhy::Above => write!(f, "{at}, above {read} which it read, changed after {who} began"),
            MovedWhy::Link => write!(f, "{at}, a link on the way to {read} which it read, changed after {who} began"),
            MovedWhy::Through => {
                write!(f, "{at}, reached through a link from {read} which it read, changed after {who} began")
            }
            MovedWhy::Untold => write!(
                f,
                "{read}, which it read, can't be told unchanged since {who} began: the links and \
                 directories its name rests on can't be resolved"
            ),
        }
    }
}

/// `name` (with no link in it, as [`names`] gives it) or the first directory above it that
/// last changed within its racy window ([`blaze_vfs::racy`]) of `since_ns` or after it; `None`
/// when none did. A name
/// that is not there is a name whose removal its directory's stamp says; one that can't be
/// `stat`ed has moved.
fn stamped_since(name: &Path, since_ns: u64) -> Option<&Path> {
    name.ancestors().find(|at| match std::fs::symlink_metadata(at) {
        Ok(meta) => blaze_vfs::racy(&stamp_of(&meta), since_ns),
        Err(error) => !missing(&error),
    })
}

/// The hashes of the machine's files, each under its path and what `stat` said of it when it
/// was hashed. Append-only: a lookup takes no lock and writes nothing shared, a file is hashed
/// outside any lock, and an edit adds an entry under its new stamp rather than changing one.
///
/// The floor of a check: one `stat` (an `lstat` for an entry or a listing) and nothing read
/// while the stamp is known; a check costs exactly that (ratio 1.0). A listing kept under its
/// directory's stamp is listed again only when a name was made, removed or renamed in it.
pub struct Files {
    stamped: Sharded<[u8; 32]>,
    /// What [`Inputs::names`] last listed of a directory, under the directory's stamp.
    listings: blaze_core::lock::Lock<blaze_hash::HashMap<PathBuf, Listed, std::alloc::Global>>,
    /// The wall clock a stamp is judged racy against ([`blaze_vfs::racy`]): [`wall_ns`], or a
    /// test's own ([`Files::with_clock`]), so a file just written is not racy to a test that
    /// says its clock is past the racy window.
    clock: fn() -> u64,
}

thread_local! {
    /// A planted bug, for a test's control: a path missing a part before its last has no
    /// names ([`names`]), so it is looked at every time, as until 2026-11-12. Never set
    /// outside a test.
    #[doc(hidden)]
    pub static MISSING_PARENT_IS_UNVOUCHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A directory's listing as kept: the directory's `lstat` stamp before it was listed, the
/// instant of that look, and each name with whether it is a directory.
type Listed = (Stamp, u64, std::sync::Arc<[(OsString, bool)]>);

/// The most symlinks one path is followed through, as the kernel's own bound is (`MAXSYMLINKS`
/// is 32 on macOS, 40 on Linux): past it the path is not vouched for.
pub(crate) const MOST_LINKS: usize = 40;

/// Every name the answer about `path` rests on, each as the watch of its directory hears it
/// (absolute, with no symlink above its last part): `path` itself when it is canonical; for a
/// path named through symlinks (the macOS SDK's `MacOSX27.2.sdk` is a link to `MacOSX.sdk`, its
/// `usr/lib/libm.tbd` one to `libSystem.tbd`), each link where it is and the file they end at.
/// A link pointed elsewhere is a change in the link's own directory, and the file changed is
/// one in its own: with a watch over each, nothing the answer depends on moves unheard. A
/// missing path's name is the first part of it that is not there: the name that would be made.
/// `None` for a path that is not absolute, has `..` in it, has more than [`MOST_LINKS`]
/// links, or of which a part before the last is unreadable. Cold: on a miss only, and once
/// per path a kept test pass read ([`Began::moved`]).
fn names(path: &Path, answer: Option<[u8; 32]>) -> Option<Vec<PathBuf>> {
    use std::path::Component;
    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir) {
        return None;
    }
    // alloc: cold, once per path on its first miss: the parts not yet resolved, last first
    let mut left: Vec<std::ffi::OsString> = Vec::new();
    for part in parts {
        match part {
            Component::Normal(name) => left.push(name.to_owned()),
            _ => return None,
        }
    }
    left.reverse();
    let mut names = Vec::new(); // alloc: cold, as above
    let mut at = PathBuf::from("/"); // alloc: cold, as above
    // Bounded: each turn takes one part, and only a link, of which there are at most
    // `MOST_LINKS`, adds any.
    while let Some(part) = left.pop() {
        if part == ".." {
            // Of a link's target: `at` has no link in it, so its parent is the name's.
            at.pop();
            continue;
        }
        let name = at.join(&part);
        match std::fs::symlink_metadata(&name) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if names.len() == MOST_LINKS {
                    return None;
                }
                let target = std::fs::read_link(&name).ok()?;
                names.push(name);
                for part in target.components().rev() {
                    match part {
                        Component::RootDir => at = PathBuf::from("/"),
                        Component::CurDir => {}
                        Component::ParentDir => left.push("..".into()),
                        Component::Normal(name) => left.push(name.to_owned()),
                        Component::Prefix(_) => return None,
                    }
                }
            }
            Ok(_) => at = name,
            // The name that would be made, for a path that is not there: its last part, or
            // the first part of it that is missing (nothing below a name that is not there
            // is, until that name is made, which its directory hears). Until 2026-11-12 a
            // path missing a part before its last was never vouched for: a tool that asks of
            // `src/lib.rs`, `src/main.rs` and `src/bin` in each package of a workspace, some
            // with no `src`, was looked at on the disk for each, in every build.
            Err(error) if missing(&error) && answer.is_none() && !MISSING_PARENT_IS_UNVOUCHED.get() => {
                at = name;
                break;
            }
            Err(error) if missing(&error) && left.is_empty() && answer.is_none() => at = name,
            Err(_) => return None,
        }
    }
    names.push(at);
    Some(names)
}

impl Files {
    pub fn new() -> Files {
        Files {
            stamped: Sharded::new(),
            listings: blaze_core::lock::Lock::new(
                "usercode.inputs.listings",
                blaze_hash::HashMap::new_in(std::alloc::Global),
            ),
            clock: wall_ns,
        }
    }

    /// The same table judging stamps against `clock` in place of the wall clock.
    #[cfg(test)]
    pub(crate) fn with_clock(self, clock: fn() -> u64) -> Files {
        Files { clock, ..self }
    }

    /// The one table of the process.
    pub fn shared() -> &'static Files {
        static FILES: OnceLock<Files> = OnceLock::new();
        FILES.get_or_init(Files::new)
    }

    /// The key of a hash kept under `stamp` for `path`. The domain is new with the stamp's
    /// layout ([`blaze_core::Stamp`], which has the mode and no device).
    fn key(path: &Path, stamp: &Stamp) -> ContentId {
        Hasher::new(crate::trace::os_domain!("blaze v2 disk stamp 2"))
            .part(&os_bytes(path.as_os_str()))
            .part(&stamp_bytes(stamp))
            .finish()
    }

    /// What `path` hashes as, links followed: one `stat`, and a read only when no hash is kept
    /// under its stamp. The look is pushed to `looks` with what it said and the stamp it said
    /// it under ([`Looks::push_stamped`]; [`Stamp::UNKNOWN`] for a file that moved while it
    /// was read, which is never held); a file that can't be read pushes nothing.
    fn file(&self, path: &Path, looks: &Looks) -> Result<Option<[u8; 32]>, Unreadable> {
        let looked_ns = (self.clock)();
        let (answer, stamp) = self.look(path)?;
        looks.push_stamped(path, LookKind::File, answer, stamp, looked_ns);
        Ok(answer)
    }

    /// What `path` hashes as on the disk now, and the stamp (a `stat`) it held under while it
    /// was looked at: [`Stamp::GONE`] for nothing there, [`Stamp::UNKNOWN`] when the stamp
    /// moved during the read.
    fn look(&self, path: &Path) -> Result<(Option<[u8; 32]>, Stamp), Unreadable> {
        // alloc: error path
        let unreadable =
            |error: io::Error| Unreadable { path: path.to_owned(), error: error.to_string() };
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if missing(&error) => return Ok((None, Stamp::GONE)),
            Err(error) => return Err(unreadable(error)),
        };
        let stamp = stamp_of(&meta);
        if meta.is_dir() {
            return Ok((Some(DIRECTORY), stamp));
        }
        if !meta.is_file() {
            // A pipe (the jobserver's), a socket or a device: reading it would take what is in
            // it from whoever it is for, or never end. What comes out of one is no file's
            // contents: the sandbox notes a read of the random devices as `T random`.
            return Ok((Some(SPECIAL), stamp));
        }
        let key = Files::key(path, &stamp);
        if let Some(found) = self.stamped.find(&key) {
            return Ok((Some(*self.stamped.get(found)), stamp));
        }
        let file = std::fs::File::open(path).map_err(unreadable)?;
        let hash = hash_reader(file).map_err(unreadable)?;
        // A file changed while it was read is hashed again at its next check.
        let after = std::fs::metadata(path).map(|it| stamp_of(&it)).ok();
        if after != Some(stamp) {
            return Ok((Some(hash), Stamp::UNKNOWN));
        }
        // Kept only under a stamp no later write can share ([`blaze_vfs::racy`]).
        if !blaze_vfs::racy(&stamp, (self.clock)()) {
            self.stamped.intern(key, || hash);
        }
        Ok((Some(hash), stamp))
    }

    /// What `path` is as a view holds it ([`Inputs::entry`]), a link not followed, as
    /// blaze-cargo's fetch reads a tree into one: a link is its target, `/`-separated; a file
    /// is executable when any of its execute bits is set. A file's content ID is kept under
    /// what `stat` says of it, as its hash is.
    ///
    /// One `lstat`, and a read only when no id is kept under the stamp; the look is pushed to
    /// `looks` with what it said ([`entry_digest`]) and that stamp; a path that can't be read
    /// as an entry pushes nothing.
    fn entry(&self, path: &Path, looks: &Looks) -> Result<Entry, Unreadable> {
        // alloc: error path
        let refused = |error: &str| Unreadable { path: path.to_owned(), error: error.to_owned() };
        if path.file_name().is_some_and(|it| it.to_str().is_none()) {
            return Err(refused("its name is not UTF-8, which no view holds"));
        }
        let looked_ns = (self.clock)();
        let (entry, stamp) = self.entry_on_disk(path)?;
        looks.push_stamped(path, LookKind::Entry, entry_digest(&entry), stamp, looked_ns);
        Ok(entry)
    }

    /// What the disk says `path` itself is, a link not followed, and its `lstat` stamp from
    /// before anything of it was read ([`Stamp::UNKNOWN`] when a file's moved while it was
    /// read).
    fn entry_on_disk(&self, path: &Path) -> Result<(Entry, Stamp), Unreadable> {
        // alloc: error path
        let refused = |error: &str| Unreadable { path: path.to_owned(), error: error.to_owned() };
        // alloc: error path
        let unreadable =
            |error: io::Error| Unreadable { path: path.to_owned(), error: error.to_string() };
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if missing(&error) => return Ok((Entry::Missing, Stamp::GONE)),
            Err(error) => return Err(unreadable(error)),
        };
        let stamp = stamp_of(&meta);
        let kind = meta.file_type();
        if kind.is_dir() {
            return Ok((Entry::Dir, stamp));
        }
        if kind.is_symlink() {
            let target = std::fs::read_link(path).map_err(unreadable)?;
            #[cfg(unix)]
            let id = ContentId::of(
                blaze_vfs::FILE_DOMAIN,
                std::os::unix::ffi::OsStrExt::as_bytes(target.as_os_str()),
            );
            #[cfg(not(unix))]
            let id = {
                let target =
                    target.to_str().ok_or_else(|| refused("its link's target is not UTF-8"))?;
                // alloc: a link off Unix, its target with `/` for each `\`, as a view has it.
                ContentId::of(blaze_vfs::FILE_DOMAIN, target.replace('\\', "/").as_bytes())
            };
            // A link is never written in place, only replaced: its own stamp moves with it.
            return Ok((Entry::File(id, blaze_vfs::Mode::Symlink), stamp));
        }
        if !kind.is_file() {
            return Err(refused(
                "it is neither a file, a directory nor a link, which no view holds",
            ));
        }
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        let mode = if executable { blaze_vfs::Mode::Executable } else { blaze_vfs::Mode::Regular };
        let key = Hasher::new(crate::trace::os_domain!("blaze v2 disk view id stamp 2"))
            .part(&os_bytes(path.as_os_str()))
            .part(&stamp_bytes(&stamp))
            .finish();
        if let Some(found) = self.stamped.find(&key) {
            return Ok((Entry::File(ContentId(*self.stamped.get(found)), mode), stamp));
        }
        let file = std::fs::File::open(path).map_err(unreadable)?;
        let mut hasher = Hasher::new(blaze_vfs::FILE_DOMAIN);
        read_all(file, |bytes| _ = hasher.update(bytes)).map_err(unreadable)?;
        let id = hasher.finish();
        // A file changed while it was read is hashed again at its next check.
        if !std::fs::symlink_metadata(path).is_ok_and(|it| stamp_of(&it) == stamp) {
            return Ok((Entry::File(id, mode), Stamp::UNKNOWN));
        }
        // And kept only under a stamp no later write can share ([`blaze_vfs::racy`]).
        if !blaze_vfs::racy(&stamp, (self.clock)()) {
            self.stamped.intern(key, || id.0);
        }
        Ok((Entry::File(id, mode), stamp))
    }
}

impl Default for Files {
    fn default() -> Files {
        Files::new()
    }
}

/// An environment, sorted by name, each name once (the first, as `getenv` finds it), and
/// what a walk of it sees: all of it ([`Env::new`]), or, for a run that is lent its
/// environment, the variables its sandbox shows a walk ([`Env::lent`]).
#[derive(PartialEq, Eq, Default, Debug)]
pub struct Env {
    vars: Vec<(OsString, OsString)>,
    /// What a walk sees of `vars`, in their order, when that is not all of them.
    walk: Option<Vec<(OsString, OsString)>>,
}

impl Env {
    pub fn new(mut vars: Vec<(OsString, OsString)>) -> Env {
        vars.sort_by(|a, b| a.0.cmp(&b.0));
        vars.dedup_by(|later, first| later.0 == first.0);
        Env { vars, walk: None }
    }

    /// The environment of a run that is lent it ([`crate::confine::GIVEN`]): a read by name
    /// finds any variable of `vars`, and a walk sees those `sees` says alone (the walk set,
    /// [`crate::confine::walk_sees`], and for the launcher the given names as well). So what
    /// a run that read them all is keyed by is the walk's variables, never the whole of the
    /// environment the build was started in.
    pub fn lent(vars: Vec<(OsString, OsString)>, sees: impl Fn(&OsStr) -> bool) -> Env {
        let mut env = Env::new(vars);
        // clone: once per run prepared or checked, the variables its walk sees
        let walk = env.vars.iter().filter(|(name, _)| sees(name)).cloned().collect(); // alloc: as above
        env.walk = Some(walk);
        env
    }

    pub fn get(&self, name: &OsStr) -> Option<&OsStr> {
        let at = self.vars.binary_search_by(|it| it.0.as_os_str().cmp(name)).ok()?;
        Some(&self.vars[at].1)
    }

    /// What a walk of the environment sees: sorted by name, each name once.
    pub fn vars(&self) -> &[(OsString, OsString)] {
        self.walk.as_deref().unwrap_or(&self.vars)
    }
}

/// The machine's files and an environment, as one check sees them: what [`Files`] it hashes
/// with and what [`Env`] it answers from, both shared, and the looks of the build the check
/// is made for ([`Looks::none`] for a check no build keeps the looks of).
///
/// The invariant: each look at the disk this makes is pushed to `looks` with the answer it
/// gave, and asking the same of an unchanged disk gives that answer again: a file's is what
/// [`Inputs::file`] returns, a listing's what [`Inputs::dir`] returns, an entry's
/// [`entry_digest`] of what [`Inputs::entry`] returns.
#[derive(Clone, Copy)]
pub struct Now<'a> {
    pub files: &'a Files,
    pub env: &'a Env,
    pub looks: &'a Looks,
}

/// The machine's files as they are, through [`Files::shared`], an environment of its own, and
/// the looks of the build it reads for.
pub struct Disk {
    files: &'static Files,
    env: Env,
    looks: Looks,
}

impl Disk {
    /// `looks` is the list of the build this reads for: each look at the disk is pushed there.
    pub fn new(env: Vec<(OsString, OsString)>, looks: &Looks) -> Disk {
        // clone: an Arc, the build's list
        Disk { files: Files::shared(), env: Env::new(env), looks: looks.clone() }
    }

    /// The files with the environment of a run that is lent it ([`Env::lent`], whose `sees`
    /// this takes): what a build script's run is assembled and checked against. `looks` as
    /// [`Disk::new`]'s.
    pub fn lent(env: Vec<(OsString, OsString)>, sees: impl Fn(&OsStr) -> bool, looks: &Looks) -> Disk {
        // clone: an Arc, the build's list
        Disk { files: Files::shared(), env: Env::lent(env, sees), looks: looks.clone() }
    }

    /// The same files with another environment.
    pub fn set_env(&mut self, env: Vec<(OsString, OsString)>) {
        self.env = Env::new(env);
    }

    pub fn now(&self) -> Now<'_> {
        Now { files: self.files, env: &self.env, looks: &self.looks }
    }
}

fn missing(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
        || error.raw_os_error() == Some(NOT_A_DIRECTORY)
}

#[cfg(unix)]
const NOT_A_DIRECTORY: i32 = libc::ENOTDIR;
#[cfg(not(unix))]
const NOT_A_DIRECTORY: i32 = 267;

/// The BLAKE3 hash of what `reader` reads.
pub fn hash_reader(reader: impl io::Read) -> io::Result<[u8; 32]> {
    let mut hasher = blaze_ids::b3::Hasher::new();
    read_all(reader, |bytes| _ = hasher.update(bytes))?;
    Ok(hasher.finalize())
}

/// Everything `reader` reads, lent to `take` a buffer at a time.
fn read_all(mut reader: impl io::Read, mut take: impl FnMut(&[u8])) -> io::Result<()> {
    let mut buffer = [0; 1 << 16];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => take(&buffer[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

impl Inputs for Now<'_> {
    type Error = Unreadable;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        self.files.file(path, self.looks)
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        frame::task(Tag::Usercode, |frame| {
            let mut names = Names::new_in(frame);
            Ok(self.names(path, &mut names)?.then(|| listing(names.iter().map(|it| it.0))))
        })
    }

    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, Unreadable> {
        // alloc: error path
        let unreadable =
            |error: io::Error| Unreadable { path: path.to_owned(), error: error.to_string() };
        // The invariant: the disk is listed only when the directory's stamp moved or was racy.
        // A listing is the names directly in the directory and whether each is a directory:
        // it moves only when a name is made, removed or renamed in it, which moves the
        // directory's own `lstat` stamp. Kept for a directory that is not a link and lists no
        // link (what a link leads to is not in its directory, and its stamp is not the
        // directory's); one `lstat` a check. Until 2026-11-12 every ask listed: a build
        // script that globs its sources (ring's `crypto/**`, zstd-sys's `zstd/lib/*`) kept
        // every build of anything above it from being answered again without a walk.
        let looked_ns = (self.files.clock)();
        let stamp = blaze_vfs::outside::lstat(path);
        let kept = self.files.listings.lock().get(path).cloned(); // clone: an `Arc`
        if let Some((was, at, names)) = &kept
            && blaze_vfs::outside::holds(was, *at, &stamp)
        {
            for (name, dir) in names.iter() {
                out.push(name, *dir);
            }
            out.sort();
            if self.looks.keeps() {
                let answer = listing(names.iter().map(|it| it.0.as_os_str()));
                self.looks.push_stamped(path, LookKind::Listing, Some(answer), *was, *at);
            }
            return Ok(true);
        }
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if missing(&error) => {
                self.looks.push_stamped(path, LookKind::Listing, None, stamp, looked_ns);
                return Ok(false);
            }
            Err(error) => return Err(unreadable(error)),
        };
        // alloc: the listing kept, once a directory listed off the disk
        let mut listed: Vec<(OsString, bool)> = Vec::new();
        let mut links = false;
        for entry in entries {
            let entry = entry.map_err(unreadable)?;
            // The listing says what each entry is, without a `stat`; a link is what it leads
            // to, as the program that opens it sees, which only a `stat` of it says.
            let kind = entry.file_type().map_err(unreadable)?;
            links |= kind.is_symlink();
            let dir = match kind.is_symlink() {
                true => std::fs::metadata(entry.path()).is_ok_and(|it| it.is_dir()),
                false => kind.is_dir(),
            };
            // alloc: std's listing owns each name: a walk of the disk allocates what `read_dir` does.
            let name = entry.file_name();
            out.push(&name, dir);
            listed.push((name, dir));
        }
        out.sort();
        // The look, once the whole listing was read: what [`Inputs::dir`] says of these names,
        // under the directory's stamp from before it was listed, or none when the listing
        // rests on more than that stamp (a link in it, or the directory itself a link).
        let held = !links && stamp.is_dir();
        let said = if held { stamp } else { Stamp::UNKNOWN };
        if self.looks.keeps() {
            let answer = listing(listed.iter().map(|it| it.0.as_os_str()));
            self.looks.push_stamped(path, LookKind::Listing, Some(answer), said, looked_ns);
        }
        if held {
            self.files.listings.lock().insert(path.to_path_buf(), (stamp, looked_ns, listed.into()));
        }
        Ok(true)
    }

    fn entry(&mut self, path: &Path) -> Result<Entry, Unreadable> {
        self.files.entry(path, self.looks)
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.env.get(name)
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        self.env.vars()
    }
}

impl Inputs for Disk {
    type Error = Unreadable;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        self.now().file(path)
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        self.now().dir(path)
    }

    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, Unreadable> {
        self.now().names(path, out)
    }

    fn entry(&mut self, path: &Path) -> Result<Entry, Unreadable> {
        self.now().entry(path)
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.env.get(name)
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        self.env.vars()
    }
}

/// Why a check against a view did not end.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ViewError {
    Cancelled,
    Unreadable(Unreadable),
}

impl From<Cancelled> for ViewError {
    fn from(_: Cancelled) -> ViewError {
        ViewError::Cancelled
    }
}

impl From<Unreadable> for ViewError {
    fn from(it: Unreadable) -> ViewError {
        ViewError::Unreadable(it)
    }
}

/// The plain BLAKE3 hashes of a view's files, by their content ID: shared by every check, as
/// [`Files`] is.
#[derive(Default)]
pub struct Hashes(Sharded<[u8; 32]>);

impl Hashes {
    fn of(&self, id: ContentId, bytes: &[u8]) -> [u8; 32] {
        if let Some(found) = self.0.find(&id) {
            return *self.0.get(found);
        }
        let hash = blaze_ids::b3::hash(bytes);
        self.0.intern(id, || hash);
        hash
    }
}

/// A view's files where the sandbox sees the view (`root`, the directory it is placed at),
/// read through `cx` so the checking query depends on each; the machine's everywhere else;
/// the environment of the request.
pub struct ViewInputs<'a, 'db> {
    pub cx: &'a mut Cx<'db>,
    pub root: &'a Path,
    pub now: Now<'a>,
    pub hashes: &'a Hashes,
}

/// `path` as the view at `root` names it, lent to `f` (`None` when it is not in the view):
/// `path`'s own text when its rest is already the view's name for it, `/`-separated with
/// nothing to drop, else built in the thread's string ([`blaze_mem::scratch::with_string`]).
fn in_view<R>(root: &Path, path: &Path, f: impl FnOnce(Option<&str>) -> R) -> R {
    let Ok(rest) = path.strip_prefix(root) else { return f(None) };
    let named = |text: &str| {
        text.is_empty()
            || (std::path::MAIN_SEPARATOR == '/'
                && text.split('/').all(|part| !matches!(part, "" | "." | "..")))
    };
    if let Some(text) = rest.to_str()
        && named(text)
    {
        return f(Some(text));
    }
    blaze_mem::scratch::with_string(|name| {
        for part in rest.components() {
            let std::path::Component::Normal(part) = part else { return f(None) };
            let Some(part) = part.to_str() else { return f(None) };
            if !name.is_empty() {
                name.push('/');
            }
            name.push_str(part);
        }
        f(Some(name))
    })
}

impl Inputs for ViewInputs<'_, '_> {
    type Error = ViewError;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ViewError> {
        in_view(self.root, path, |name| {
            let Some(name) = name else { return Ok(self.now.file(path)?) };
            if !name.is_empty()
                && let Some(file) = self.cx.file(name)?
            {
                return Ok(Some(self.hashes.of(file.id, file.bytes)));
            }
            Ok(self.cx.dir(name)?.map(|_| DIRECTORY))
        })
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ViewError> {
        in_view(self.root, path, |name| match name {
            None => Ok(self.now.dir(path)?),
            Some(name) => Ok(self
                .cx
                .dir(name)?
                .map(|entries| listing(entries.iter().map(|it| OsStr::new(&it.0))))),
        })
    }

    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, ViewError> {
        in_view(self.root, path, |name| {
            let Some(name) = name else { return Ok(self.now.names(path, out)?) };
            // Sorted by name already, as `Names::sort` sorts.
            let Some(entries) = self.cx.dir(name)? else { return Ok(false) };
            for (name, dir) in entries {
                out.push(OsStr::new(name), *dir);
            }
            Ok(true)
        })
    }

    fn entry(&mut self, path: &Path) -> Result<Entry, ViewError> {
        in_view(self.root, path, |name| {
            let Some(name) = name else { return Ok(self.now.entry(path)?) };
            if !name.is_empty()
                && let Some(file) = self.cx.file(name)?
            {
                return Ok(Entry::File(file.id, file.mode));
            }
            Ok(match self.cx.dir(name)? {
                Some(_) => Entry::Dir,
                None => Entry::Missing,
            })
        })
    }

    /// The view's own hash below `dir`, kept per directory, so an edit hashes again only the
    /// directories above it: the same as [`crate::trace::walk`]'s of the same tree.
    fn tree(&mut self, dir: &Path, package: bool) -> Result<Option<[u8; 32]>, ViewError> {
        in_view(self.root, dir, |name| match name {
            None => Ok(self.now.tree(dir, package)?),
            Some(name) => Ok(self.cx.tree(name, package)?),
        })
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.now.env(name)
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        self.now.env.vars()
    }
}

#[cfg(test)]
mod tests {
    use blaze_core::temp::TempDir;

    use super::*;

    /// Past every racy window ([`blaze_vfs::COARSE_NS`] is the widest).
    const RACY_NS: u64 = blaze_vfs::COARSE_NS;

    /// A file just written is hashed from the disk each time it is looked at, so a rewrite of
    /// the same length under the same stamp is seen; nothing was kept for it to hide behind.
    #[test]
    fn a_file_rewritten_at_once_to_the_same_length_is_seen() {
        let dir = TempDir::for_test("blaze-usercode-racy-");
        let path = dir.join("input");
        let files = Files::new();
        std::fs::write(&path, b"aaaa").unwrap();
        let (first, _) = files.look(&path).unwrap();
        std::fs::write(&path, b"bbbb").unwrap();
        let (second, _) = files.look(&path).unwrap();
        drop(dir);
        assert!(first.is_some() && second.is_some());
        assert_ne!(first, second, "the second write's bytes are what is hashed");
    }

    /// The invariant: a run's reads, hashed after it ends, are taken for what it read only when
    /// nothing they name changed since it began: a file written, one made and one removed
    /// after the run began have moved, and a listed directory a name was made in has. The
    /// control is the same files asked of by a run that began after they last changed (its
    /// clock past the racy window), which nothing moved for: the cause removed, the effect
    /// goes.
    #[test]
    fn a_read_changed_after_its_run_began_has_moved() {
        let dir = TempDir::for_test("blaze-usercode-moved-");
        let (written, made, removed) = (dir.join("written"), dir.join("made"), dir.join("removed"));
        std::fs::write(&written, b"before").unwrap();
        std::fs::write(&removed, b"there").unwrap();
        let began = Began::now();
        std::fs::write(&written, b"after").unwrap();
        std::fs::write(&made, b"new").unwrap();
        std::fs::remove_file(&removed).unwrap();
        let read = |path: &Path, answer: Option<[u8; 32]>| blaze_core::Traced {
            files: vec![(path.to_path_buf(), answer)],
            ..blaze_core::Traced::default()
        };
        let listed = blaze_core::Traced { dirs: vec![(dir.path().to_path_buf(), [1; 32])], ..Default::default() };
        let asked = [
            read(&written, Some([1; 32])),
            read(&made, Some([2; 32])),
            read(&made, None),
            read(&removed, None),
            listed,
        ];
        let later = Began { wall_ns: wall_ns() + 3 * RACY_NS };
        for traced in &asked {
            assert!(began.moved(traced).is_some(), "changed after the run began: {traced:?}");
            assert_eq!(later.moved(traced), None, "the control: nothing changed after it began: {traced:?}");
        }
        drop(dir);
    }

    /// The invariant: what moved names the read and the name whose stamp says so, and how
    /// the two stand: the file itself, a directory above it (here the parent of a missing
    /// part, whose making that directory would hear), a link it is named through, and a path
    /// whose names can't be told, which is taken as moved and said so. The control: a run
    /// that began after all of it says nothing moved.
    #[test]
    fn what_moved_names_the_read_and_the_name_whose_stamp_says_so() {
        let dir = TempDir::for_test("blaze-usercode-moved-said-");
        // Canonical, so a link above the temporary directory (macOS's `/var`) is none of these.
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let began = Began::now();
        let file = root.join("file");
        std::fs::write(&file, b"after").unwrap();
        let read = |path: &Path, answer: Option<[u8; 32]>| blaze_core::Traced {
            files: vec![(path.to_path_buf(), answer)],
            ..blaze_core::Traced::default()
        };
        let moved = |read: &Path, at: &Path, why: MovedWhy| Moved { read: read.to_path_buf(), at: at.to_path_buf(), why };

        let itself = began.moved(&read(&file, Some([1; 32])));
        assert_eq!(itself, Some(moved(&file, &file, MovedWhy::Itself)));
        assert_eq!(
            itself.unwrap().said("the test").to_string(),
            format!("{} changed after the test began", file.display())
        );

        // `root/none` is not there: its name is the one that would be made, which `root`'s
        // stamp says, and `root` was just made.
        let below = root.join("none/below");
        let above = began.moved(&read(&below, None));
        assert_eq!(above, Some(moved(&below, &root, MovedWhy::Above)));
        assert_eq!(
            above.unwrap().said("the test").to_string(),
            format!("{}, above {} which it read, changed after the test began", root.display(), below.display())
        );

        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&root, &link).unwrap();
            let through = link.join("file");
            assert_eq!(began.moved(&read(&through, Some([1; 32]))), Some(moved(&through, &link, MovedWhy::Link)));
        }

        for untold in [Path::new("relative/file"), Path::new("/a/../b")] {
            let said = began.moved(&read(untold, Some([1; 32])));
            assert_eq!(said, Some(moved(untold, untold, MovedWhy::Untold)));
            let later = Began { wall_ns: wall_ns() + 3 * RACY_NS };
            assert_eq!(later.moved(&read(untold, Some([1; 32]))), said, "untold is never taken as still");
        }

        // How a name stands to the read, for a file that is there: a directory above it whose
        // stamp moved while the file's did not, and what a link on its way leads to.
        let (c, f) = (Path::new("/a/b/c"), Path::new("/a/l/f"));
        assert_eq!(MovedWhy::of(c, c, true, Path::new("/a/b")), MovedWhy::Above);
        assert_eq!(MovedWhy::of(c, c, true, c), MovedWhy::Itself);
        assert_eq!(MovedWhy::of(f, Path::new("/a/l"), false, Path::new("/a/l")), MovedWhy::Link);
        assert_eq!(MovedWhy::of(f, Path::new("/a/l"), false, Path::new("/a")), MovedWhy::Above);
        assert_eq!(MovedWhy::of(f, Path::new("/x/f"), true, Path::new("/x/f")), MovedWhy::Through);
        assert_eq!(MovedWhy::of(f, Path::new("/x/f"), true, Path::new("/x")), MovedWhy::Through);

        let later = Began { wall_ns: wall_ns() + 3 * RACY_NS };
        assert_eq!(later.moved(&read(&file, Some([1; 32]))), None, "the control");
        assert_eq!(later.moved(&read(&below, None)), None, "the control");
        drop(dir);
    }

    #[test]
    fn the_disk_hashes_files_lists_directories_and_refuses_nothing_quietly() {
        let tmp = TempDir::for_test("blaze-disk-test-");
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), b"one").unwrap();
        let mut disk = Disk::new(vec![("B".into(), "2".into()), ("A".into(), "1".into())], &Looks::none());
        assert_eq!(disk.file(&dir.join("a")).unwrap(), Some(blaze_ids::b3::hash(b"one")));
        assert_eq!(disk.file(&dir.join("sub")).unwrap(), Some(DIRECTORY));
        assert_eq!(disk.file(&dir.join("none")).unwrap(), None);
        assert_eq!(disk.file(&dir.join("a/below")).unwrap(), None);
        assert_eq!(disk.dir(&dir).unwrap(), Some(listing(["a", "sub"].map(OsStr::new))));
        assert_eq!(disk.dir(&dir.join("none")).unwrap(), None);
        std::fs::write(dir.join("a"), b"two, longer").unwrap();
        assert_eq!(disk.file(&dir.join("a")).unwrap(), Some(blaze_ids::b3::hash(b"two, longer")));
        assert_eq!(disk.env(OsStr::new("A")), Some(OsStr::new("1")));
        assert_eq!(disk.env_all()[0].0, "A");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::write(dir.join("secret"), b"x").unwrap();
            std::fs::set_permissions(dir.join("secret"), std::fs::Permissions::from_mode(0))
                .unwrap();
            // Root reads it anyway; anyone else is told it can't be read.
            if std::fs::read(dir.join("secret")).is_err() {
                assert!(disk.file(&dir.join("secret")).is_err());
            }
            std::fs::set_permissions(dir.join("secret"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
    }

    #[test]
    fn an_environment_has_each_name_once_as_getenv_finds_it() {
        let env = Env::new(vec![
            ("B".into(), "2".into()),
            ("A".into(), "1".into()),
            ("B".into(), "3".into()),
        ]);
        let expected = vec![
            (OsString::from("A"), OsString::from("1")),
            (OsString::from("B"), OsString::from("2")),
        ];
        assert_eq!(env.vars(), expected.as_slice());
        assert_eq!(env.get(OsStr::new("B")), Some(OsStr::new("2")));
        assert_eq!(env.get(OsStr::new("C")), None);
    }

    /// The invariant: a lent environment answers a read by name from all of it and a walk
    /// from the walk's variables alone, in order. The control: the same variables not lent
    /// show a walk every one.
    #[test]
    fn a_lent_environment_is_read_whole_by_name_and_walked_in_part() {
        let vars = || -> Vec<(OsString, OsString)> {
            vec![("TERM".into(), "x".into()), ("PATH".into(), "/bin".into()), ("CC".into(), "cc".into())]
        };
        let lent = Env::lent(vars(), |name| name != "TERM");
        assert_eq!(lent.get(OsStr::new("TERM")), Some(OsStr::new("x")));
        let walked: Vec<&OsStr> = lent.vars().iter().map(|it| it.0.as_os_str()).collect();
        assert_eq!(walked, [OsStr::new("CC"), OsStr::new("PATH")]);
        let mut disk = Disk::lent(vars(), |name| name != "TERM", &Looks::none());
        assert_eq!(disk.env(OsStr::new("TERM")), Some(OsStr::new("x")));
        assert_eq!(disk.env_all().len(), 2);
        // The control.
        assert_eq!(Env::new(vars()).vars().len(), 3);
    }

    #[test]
    fn every_check_shares_one_table_and_an_edit_is_a_new_entry() {
        let dir = TempDir::for_test("blaze-files-test-");
        let path = dir.join("a");
        std::fs::write(&path, b"one").unwrap();
        // A clock past the racy window of the writes here, so each hash is kept under its
        // stamp ([`blaze_vfs::racy`], which blaze-vfs's `outside` tests).
        let files = Files::new().with_clock(|| wall_ns() + 10 * RACY_NS);
        let (one, two) = (Env::new(vec![("X".into(), "1".into())]), Env::default());
        let hashes: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|n| {
                    let (files, env, path) = (&files, if n % 2 == 0 { &one } else { &two }, &path);
                    scope.spawn(move || Now { files, env, looks: &Looks::none() }.file(path).unwrap())
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert!(hashes.iter().all(|it| *it == Some(blaze_ids::b3::hash(b"one"))));
        assert_eq!(files.stamped.len(), 1);
        std::fs::write(&path, b"two, longer").unwrap();
        let mut now = Now { files: &files, env: &one, looks: &Looks::none() };
        assert_eq!(now.file(&path).unwrap(), Some(blaze_ids::b3::hash(b"two, longer")));
        assert_eq!(files.stamped.len(), 2);
        assert_eq!(now.env(OsStr::new("X")), Some(OsStr::new("1")));
    }

    /// The invariant: each look at the disk is pushed once to the build's [`Looks`], with its
    /// kind and the answer the ask returned (an entry's through [`entry_digest`]), and the
    /// same ask of the unchanged disk, through [`Looks::none`] as the daemon's later check
    /// makes it, gives that answer again, each look with a stamp that holds while nothing
    /// changed. The control: a handle that keeps nothing ([`Looks::none`]) holds no look.
    #[test]
    fn each_look_is_pushed_once_with_the_answer_a_second_ask_gives() {
        let made = TempDir::for_test("blaze-files-looks-");
        std::fs::create_dir_all(made.join("sub")).unwrap();
        let dir = std::fs::canonicalize(&made).unwrap();
        let (file, gone) = (dir.join("a"), dir.join("gone"));
        std::fs::write(&file, b"one").unwrap();
        let files = Files::new();
        let (env, looks, none) = (Env::default(), Looks::new(), Looks::none());
        let mut now = Now { files: &files, env: &env, looks: &looks };
        let mut again = Now { files: &files, env: &env, looks: &none };
        let mut expect: Vec<(PathBuf, LookKind, Option<[u8; 32]>)> = Vec::new();

        for path in [&file, &gone, &dir] {
            let answer = now.file(path).unwrap();
            assert_eq!(again.file(path).unwrap(), answer, "{}", path.display());
            expect.push((path.clone(), LookKind::File, answer));
        }
        assert_eq!(expect[0].2, Some(blaze_ids::b3::hash(b"one")));
        assert_eq!(expect[1].2, None);
        assert_eq!(looks.len(), 3);

        for path in [&file, &gone, &dir] {
            let answer = entry_digest(&now.entry(path).unwrap());
            assert_eq!(entry_digest(&again.entry(path).unwrap()), answer, "{}", path.display());
            expect.push((path.clone(), LookKind::Entry, answer));
        }
        assert_eq!(expect[4].2, None, "an entry that is not there says nothing");
        assert!(expect[3].2.is_some() && expect[5].2.is_some() && expect[3].2 != expect[5].2);
        assert_eq!(looks.len(), 6);

        for path in [&dir, &gone] {
            let answer = now.dir(path).unwrap();
            assert_eq!(again.dir(path).unwrap(), answer, "{}", path.display());
            expect.push((path.clone(), LookKind::Listing, answer));
        }
        assert_eq!(expect[6].2, Some(listing([OsStr::new("a"), OsStr::new("sub")])));
        assert_eq!(expect[7].2, None);

        expect.sort();
        let kept: Vec<_> = looks.all().into_iter().map(|it| (it.path, it.kind, it.answer)).collect();
        assert_eq!(kept, expect, "one look per ask, of its kind, with its answer");
        assert!(none.is_empty() && !none.keeps());
        assert!(Looks::twice(&looks.all()).next().is_none());

        // A second ask that says something else is a second look of the path, which
        // [`Looks::twice`] names.
        std::fs::write(&file, b"two, longer").unwrap();
        assert_eq!(now.file(&file).unwrap(), Some(blaze_ids::b3::hash(b"two, longer")));
        let all = looks.all();
        let twice: Vec<_> = Looks::twice(&all).collect();
        assert_eq!(twice.len(), 1);
        assert_eq!((twice[0].0.path.as_path(), twice[0].0.kind), (file.as_path(), LookKind::File));
    }

    /// A path named through symlinks rests on each link and on the file they end at
    /// ([`names`], what [`Began::moved`] stamps): the macOS SDK's
    /// `MacOSX27.2.sdk/usr/lib/libm.tbd` is two links.
    #[cfg(unix)]
    #[test]
    fn a_path_through_symlinks_rests_on_each_link_and_the_file_they_end_at() {
        let made = TempDir::for_test("blaze-files-links-");
        let dir = std::fs::canonicalize(&made).unwrap();
        for sub in ["sdks/real/lib", "other"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("sdks/real/lib/system"), b"one").unwrap();
        std::os::unix::fs::symlink("real", dir.join("sdks/named")).unwrap();
        std::os::unix::fs::symlink("system", dir.join("sdks/real/lib/m")).unwrap();
        std::os::unix::fs::symlink("../sdks/named/lib", dir.join("other/lib")).unwrap();
        let hash = Some(blaze_ids::b3::hash(b"one"));
        assert_eq!(
            names(&dir.join("sdks/named/lib/m"), hash),
            Some(vec![dir.join("sdks/named"), dir.join("sdks/real/lib/m"), dir.join("sdks/real/lib/system")])
        );
        assert_eq!(
            names(&dir.join("other/lib/m"), hash),
            Some(vec![
                dir.join("other/lib"),
                dir.join("sdks/named"),
                dir.join("sdks/real/lib/m"),
                dir.join("sdks/real/lib/system"),
            ])
        );
        assert_eq!(names(&dir.join("sdks/real/lib/system"), hash), Some(vec![dir.join("sdks/real/lib/system")]));
        assert_eq!(names(&dir.join("sdks/named/lib/gone"), None), Some(vec![dir.join("sdks/named"), dir.join("sdks/real/lib/gone")]));
        // The invariant: a path missing a part before its last rests on that part's name.
        assert_eq!(names(&dir.join("sdks/gone/lib/m"), None), Some(vec![dir.join("sdks/gone")]));
        // The control: as until 2026-11-12, such a path is vouched for by nothing.
        MISSING_PARENT_IS_UNVOUCHED.set(true);
        assert_eq!(names(&dir.join("sdks/gone/lib/m"), None), None, "planted");
        MISSING_PARENT_IS_UNVOUCHED.set(false);
        // A `.` names nothing (`Path::components` drops it); a `..` may cross a link, and is
        // not resolved here.
        assert_eq!(names(&dir.join("sdks/./real/lib/system"), hash), Some(vec![dir.join("sdks/real/lib/system")]));
        assert_eq!(names(&dir.join("sdks/named/../real/lib/system"), hash), None);
        assert_eq!(names(Path::new("relative/a"), hash), None);
        std::os::unix::fs::symlink("loop", dir.join("loop")).unwrap();
        assert_eq!(names(&dir.join("loop"), hash), None, "a link to itself ends at the bound");

        // Read through the links, the file's answer is the file's.
        let path = dir.join("sdks/named/lib/m");
        assert_eq!(Files::new().file(&path, &Looks::none()).unwrap(), hash);
    }

    /// A file asked again, its stamp known, allocates nothing: the path is hashed in place and
    /// `stat`ed through a name on the stack.
    #[cfg(unix)]
    #[test]
    fn a_warm_file_allocates_nothing() {
        let made = TempDir::for_test("blaze-files-warm-");
        let path = std::fs::canonicalize(&made).unwrap().join("a");
        std::fs::write(&path, b"one").unwrap();
        // A clock past the write's racy window, so its hash is kept under its stamp.
        let files = Files::new().with_clock(|| wall_ns() + 10 * RACY_NS);
        let hash = Some(blaze_ids::b3::hash(b"one"));
        assert_eq!(files.file(&path, &Looks::none()).unwrap(), hash);
        let warm =
            blaze_alloc_count::assert_at_most(0, "a warm file", || files.file(&path, &Looks::none()).unwrap());
        assert_eq!(warm, hash);
        let missing = path.with_file_name("gone");
        assert_eq!(files.file(&missing, &Looks::none()).unwrap(), None);
        let warm = blaze_alloc_count::assert_at_most(0, "a warm missing file", || {
            files.file(&missing, &Looks::none()).unwrap()
        });
        assert_eq!(warm, None);
    }

    blaze_format::record! {
        #[derive(Clone, Copy)]
        struct BothRoot: 1 {
            kept: ContentId,
            walked: ContentId,
        }
    }

    /// A view's tree hash both ways: its own ([`Cx::tree`]) and [`crate::trace::walk`]'s
    /// through [`ViewInputs`], of the view placed at `/v`.
    struct Both(ContentId, ContentId);

    impl blaze_format::Persist for Both {
        const SCHEMA: blaze_format::SchemaHash = <BothRoot as blaze_format::Record>::SCHEMA;
        type Ref<'a> = ([u8; 32], [u8; 32]);
        fn write(&self, out: &mut blaze_format::Writer) -> u64 {
            out.push(&BothRoot { kept: self.0, walked: self.1 })
        }
        fn read(buffer: &[u8]) -> Result<([u8; 32], [u8; 32]), blaze_format::FormatError> {
            blaze_format::open::<BothRoot>(buffer).map(|it| (it.kept.0, it.walked.0))
        }
    }

    struct ViewTree;

    impl blaze_query::Query for ViewTree {
        const KIND: blaze_query::QueryKind = blaze_query::QueryKind(0x5ff0);
        const NAME: &'static str = "view tree, both ways";
        const PRODUCER: ContentId = ContentId([0x7e; 32]);
        type Key = bool;
        type Value = Both;
        fn input(package: &bool) -> ContentId {
            ContentId::of("blaze v2 usercode test view tree 1", &[u8::from(*package)])
        }
        fn execute(cx: &mut Cx<'_>, package: &bool) -> Result<Both, Cancelled> {
            let kept = cx.tree("", *package)?.expect("the root is a directory");
            let (env, hashes) = (Env::new(Vec::new()), Hashes::default());
            let now = Now { files: Files::shared(), env: &env, looks: &Looks::none() };
            let mut inputs = ViewInputs { cx, root: Path::new("/v"), now, hashes: &hashes };
            let walked = match crate::trace::walk(&mut inputs, Path::new("/v"), *package) {
                Ok(it) => it.expect("the root is a directory"),
                Err(ViewError::Cancelled) => return Err(Cancelled),
                Err(ViewError::Unreadable(it)) => panic!("{it}"),
            };
            Ok(Both(ContentId(kept), ContentId(walked)))
        }
    }

    /// A tree on the disk hashes as the same tree in a view does: names, kinds, contents, an
    /// executable bit and a link's target, with a package's hidden names and `target` left out
    /// alike.
    #[cfg(unix)]
    #[test]
    fn a_tree_on_the_disk_hashes_as_the_same_tree_in_a_view() {
        use std::{os::unix::fs::PermissionsExt as _, sync::Arc};

        use blaze_vfs::Mode;
        let files: &[(&str, Mode, &[u8])] = &[
            ("Cargo.toml", Mode::Regular, b"[package]"),
            ("src/lib.rs", Mode::Regular, b"fn f() {}"),
            ("src/deep/a.rs", Mode::Regular, b""),
            ("run.sh", Mode::Executable, b"#!/bin/sh"),
            ("link", Mode::Symlink, b"src/lib.rs"),
            (".git/HEAD", Mode::Regular, b"ref"),
            ("target/out", Mode::Regular, b"bin"),
        ];
        let tree = Arc::new(blaze_query::MemTree::default());
        let config = blaze_query::Config { tree: Some(tree.clone()), dir: None, tags: 0 };
        let mut db = blaze_query::Database::new(config);
        db.register::<ViewTree>();
        let view = db.open(blaze_vfs::View { root: tree.root_with_modes(files) });

        let tmp = TempDir::for_test("blaze-tree-test-");
        let dir = tmp.path();
        for (path, mode, bytes) in files {
            let at = dir.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            match mode {
                Mode::Symlink => {
                    std::os::unix::fs::symlink(std::str::from_utf8(bytes).unwrap(), &at).unwrap()
                }
                _ => std::fs::write(&at, bytes).unwrap(),
            }
            if *mode == Mode::Executable {
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let mut disk = Disk::new(Vec::new(), &Looks::none());
        for package in [false, true] {
            let (kept, walked) = db.pin().get::<ViewTree>(view, &package).expect("not cancelled");
            assert_eq!(walked, kept, "package {package}: the view walked and its own hash");
            assert_eq!(
                disk.tree(&dir, package).unwrap(),
                Some(kept),
                "package {package}: the disk and the view"
            );
        }
        let plain = disk.tree(&dir, false).unwrap();
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert_ne!(disk.tree(&dir, false).unwrap(), plain, "the executable bit is in the hash");
        assert_eq!(disk.tree(&dir.join("none"), false).unwrap(), None);
    }
}
