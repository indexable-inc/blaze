//! What a run's reads are hashed as ([`crate::trace::Inputs`]): the machine's files as they
//! are on disk ([`Disk`]), for the launcher as a run ends and for paths outside every view
//! (a toolchain, the C compiler, `/usr/include`: #1480); and a view's files through the query
//! database ([`ViewInputs`]), which records each as an edge of the query that checks them.
//!
//! The machine's files are the one thing of user code read outside a view: they are hashed
//! once per change of what `stat` says of them, in one table every check of the process shares
//! ([`Files`]), and a file that is there and can't be read is an error naming it, never left
//! out. A path under a directory the vfs watches ([`blaze_vfs::outside`]) is not even
//! `stat`ed while the watch has heard nothing under it.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    hash::Hasher as _,
    io,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering, fence},
    },
};

use blaze_hash::FixedHasher;
use blaze_ids::{ContentId, Hasher, Interner, Sharded};
use blaze_mem::{Tag, frame};
use blaze_query::{Cancelled, Cx};
use blaze_vfs::outside::{Cover, Dirs};

use crate::trace::{DIRECTORY, Entry, Inputs, Names, listing, os_bytes};

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

/// What `stat` says of a file: a hash made when it said this is the file's hash now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    modified: i128,
    changed: i128,
    inode: u64,
    device: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Stamp {
                len: meta.len(),
                modified: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
                changed: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
                inode: meta.ino(),
                device: meta.dev(),
            }
        }
        #[cfg(not(unix))]
        {
            let nanos = |time: io::Result<std::time::SystemTime>| {
                time.ok()
                    .and_then(|it| it.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(-1, |it| it.as_nanos() as i128)
            };
            Stamp {
                len: meta.len(),
                modified: nanos(meta.modified()),
                changed: nanos(meta.created()),
                inode: 0,
                device: 0,
            }
        }
    }

    fn bytes(&self) -> [u8; 56] {
        let mut out = [0; 56];
        out[..8].copy_from_slice(&self.len.to_le_bytes());
        out[8..24].copy_from_slice(&self.modified.to_le_bytes());
        out[24..40].copy_from_slice(&self.changed.to_le_bytes());
        out[40..48].copy_from_slice(&self.inode.to_le_bytes());
        out[48..].copy_from_slice(&self.device.to_le_bytes());
        out
    }
}

/// The hashes of the machine's files, each under its path and what `stat` said of it when it
/// was hashed. Append-only: a lookup takes no lock and writes nothing shared, a file is hashed
/// outside any lock, and an edit adds an entry under its new stamp rather than changing one.
///
/// Each path's last answer is also kept with the watched directories covering it
/// ([`blaze_vfs::Cover`]: its own, and for a path named through symlinks each link's and the
/// file's they end at, [`names`]) and the generation read before it was looked at: while the
/// watches have heard nothing under those directories since, the answer is returned with no
/// syscall.
pub struct Files {
    stamped: Sharded<[u8; 32]>,
    vouched: Sharded<Vouched>,
    /// What [`Files::entry`] last said of a path whose directory has no link in its name,
    /// with the generation read before it looked: vouched for while the watches heard
    /// nothing under a directory at or above it since.
    entries: blaze_core::lock::Lock<
        blaze_hash::HashMap<PathBuf, (u64, Entry), std::alloc::Global>,
    >,
    /// What [`Inputs::names`] last listed of a directory (each name and whether it is a
    /// directory), with the generation read before it listed.
    listings: blaze_core::lock::Lock<blaze_hash::HashMap<PathBuf, Listed, std::alloc::Global>>,
    dirs: &'static Dirs,
}

thread_local! {
    /// A planted bug, for a test's control: a path missing a part before its last has no
    /// names ([`names`]), so it is looked at every time, as until 2026-11-12. Never set
    /// outside a test.
    #[doc(hidden)]
    pub static MISSING_PARENT_IS_UNVOUCHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A directory's listing as kept: the generation read before it was listed, and each name
/// with whether it is a directory.
type Listed = (u64, std::sync::Arc<[(OsString, bool)]>);

/// The name a listing's watch is asked of: one no entry of a directory has (a NUL in it).
const LISTING: &str = "\0the listing";

thread_local! {
    /// A planted bug, for a test's control: a directory is listed off the disk on every ask,
    /// as until 2026-11-12. Never set outside a test.
    #[doc(hidden)]
    pub static LISTING_LOOKS_EVERY_TIME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// A planted bug, for a test's control: [`Files::entry`] looks at the disk on every ask,
    /// as until 2026-11-12. Never set outside a test.
    #[doc(hidden)]
    pub static ENTRY_LOOKS_EVERY_TIME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A path's last answer, under a sequence lock: written on a miss (rare, and a writer that
/// finds another writing skips), read with three loads and a compare.
struct Vouched {
    path: Box<OsStr>,
    /// One for each name the answer rests on ([`names`]): all must have heard nothing. `None`
    /// for a path with `..` in it, one whose links don't resolve, or one a name of
    /// which no watched directory covers: each of its checks looks at the disk.
    names: Option<Box<[PathBuf]>>,
    sequence: AtomicU64,
    generation: AtomicU64,
    present: AtomicBool,
    hash: [AtomicU64; 4],
}

impl Vouched {
    fn read(&self) -> Option<(u64, Option<[u8; 32]>)> {
        let sequence = self.sequence.load(Ordering::Acquire);
        if sequence & 1 == 1 {
            return None;
        }
        let generation = self.generation.load(Ordering::Relaxed);
        let present = self.present.load(Ordering::Relaxed);
        let mut hash = [0; 32];
        for (at, word) in self.hash.iter().enumerate() {
            hash[at * 8..at * 8 + 8].copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        fence(Ordering::Acquire);
        (self.sequence.load(Ordering::Relaxed) == sequence)
            .then_some((generation, present.then_some(hash)))
    }

    fn write(&self, generation: u64, answer: Option<[u8; 32]>) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        if sequence & 1 == 1
            || self
                .sequence
                .compare_exchange(sequence, sequence + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        fence(Ordering::Release);
        self.generation.store(generation, Ordering::Relaxed);
        self.present.store(answer.is_some(), Ordering::Relaxed);
        let hash = answer.unwrap_or_default();
        for (at, word) in self.hash.iter().enumerate() {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(&hash[at * 8..at * 8 + 8]);
            word.store(u64::from_le_bytes(bytes), Ordering::Relaxed);
        }
        self.sequence.store(sequence + 2, Ordering::Release);
    }
}

/// splitmix64's finalizer: every bit of the result depends on every bit of `word`, as the table's
/// shard and probe bits need.
fn mix(mut word: u64) -> u64 {
    word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    word ^ (word >> 31)
}

/// The most symlinks one path is followed through, as the kernel's own bound is (`MAXSYMLINKS`
/// is 32 on macOS, 40 on Linux): past it the path is not vouched for.
const MOST_LINKS: usize = 40;

/// Every name the answer about `path` rests on, each as the watch of its directory hears it
/// (absolute, with no symlink above its last part): `path` itself when it is canonical; for a
/// path named through symlinks (the macOS SDK's `MacOSX27.2.sdk` is a link to `MacOSX.sdk`, its
/// `usr/lib/libm.tbd` one to `libSystem.tbd`), each link where it is and the file they end at.
/// A link pointed elsewhere is a change in the link's own directory, and the file changed is
/// one in its own: with a watch over each, nothing the answer depends on moves unheard. A
/// missing path's name is the first part of it that is not there: the name that would be made.
/// `None` for a path that is not absolute, has `..` in it, has more than [`MOST_LINKS`]
/// links, or of which a part before the last is unreadable. Cold: on a miss only.
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
        Files::watched_by(blaze_vfs::outside::dirs())
    }

    /// A table whose answers are vouched for by `dirs`' watches.
    pub fn watched_by(dirs: &'static Dirs) -> Files {
        Files {
            stamped: Sharded::new(),
            vouched: Sharded::new(),
            entries: blaze_core::lock::Lock::new(
                "usercode.inputs.entries",
                blaze_hash::HashMap::new_in(std::alloc::Global),
            ),
            listings: blaze_core::lock::Lock::new(
                "usercode.inputs.listings",
                blaze_hash::HashMap::new_in(std::alloc::Global),
            ),
            dirs,
        }
    }

    /// The one table of the process.
    pub fn shared() -> &'static Files {
        static FILES: OnceLock<Files> = OnceLock::new();
        FILES.get_or_init(Files::new)
    }

    /// Where `path`'s answer is kept: two seeded `FixedHasher` passes, mixed, in place of BLAKE3,
    /// which costs ten times the rest of a check. The table never leaves the process, and
    /// [`Vouched::path`] is compared on each find, so two paths that collide are told apart
    /// (the later one is looked at on the disk every time).
    fn path_id(path: &Path) -> ContentId {
        let bytes = path.as_os_str().as_encoded_bytes();
        let pass = |seed: u64| {
            let mut hasher = FixedHasher::with_seed(seed);
            hasher.write(bytes);
            mix(hasher.finish())
        };
        let (a, b) = (pass(0x243f_6a88_85a3_08d3), pass(0x1319_8a2e_0370_7344));
        let mut id = [0; 32];
        // identity: two seeded passes over one path's bytes mixed into one key of a table that never leaves the process and compares the path on each find; not the identity of a set
        for (at, word) in [a, b, mix(a ^ b), mix(a.wrapping_add(b))].into_iter().enumerate() {
            id[at * 8..at * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        ContentId(id)
    }

    fn key(path: &Path, stamp: &Stamp) -> ContentId {
        Hasher::new(crate::trace::os_domain!("blaze v2 disk stamp 1"))
            .part(&os_bytes(path.as_os_str()))
            .part(&stamp.bytes())
            .finish()
    }

    fn file(&self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        let id = Files::path_id(path);
        let found = self.vouched.find(&id).map(|it| self.vouched.get(it));
        let known = found.filter(|it| *it.path == *path.as_os_str());
        if let Some(known) = known
            && let Some(names) = &known.names
            && let Some((generation, answer)) = known.read()
            && names.iter().all(|name| self.dirs.name_unchanged_since(name, generation))
        {
            return Ok(answer);
        }
        let generation = self.dirs.generation();
        // No watch vouched for the answer kept, or none was kept: the disk is looked at, and
        // counted, so a stretch of checks that looked at nothing can be told
        // (`blaze_vfs::outside::looks`).
        blaze_vfs::outside::looked(path);
        let (answer, settled) = self.look(path)?;
        blaze_vfs::outside::looked_file(path, answer);
        if settled {
            match (known, found) {
                (Some(known), _) => known.write(generation, answer),
                (None, None) => self.vouch(id, path, generation, answer),
                (None, Some(_)) => {}
            }
        }
        Ok(answer)
    }

    /// Keeps a first answer for `path`, with the directory to vouch for it if it is canonical:
    /// its parent is watched from the driver's next round.
    fn vouch(&self, id: ContentId, path: &Path, generation: u64, answer: Option<[u8; 32]>) {
        // Every name the answer rests on has its directory wanted, and all are covered or the
        // answer is not vouched for.
        // Every name the answer rests on is asked of by name ([`Dirs::watch_name`]): what
        // is written elsewhere under its directory does not move it.
        let names = names(path, answer).map(|names| {
            for name in &names {
                self.dirs.watch_name(name);
            }
            names.into_boxed_slice()
        });
        let at = self.vouched.intern(id, || Vouched {
            path: path.as_os_str().into(), // alloc: once per path, on its first miss
            names,
            sequence: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            present: AtomicBool::new(false),
            hash: Default::default(),
        });
        let kept = self.vouched.get(at);
        if *kept.path == *path.as_os_str() {
            kept.write(generation, answer);
        }
    }

    /// What `path` hashes as on the disk now, and whether that held while it was looked at.
    fn look(&self, path: &Path) -> Result<(Option<[u8; 32]>, bool), Unreadable> {
        // alloc: error path
        let unreadable =
            |error: io::Error| Unreadable { path: path.to_owned(), error: error.to_string() };
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if missing(&error) => return Ok((None, true)),
            Err(error) => return Err(unreadable(error)),
        };
        if meta.is_dir() {
            return Ok((Some(DIRECTORY), true));
        }
        if !meta.is_file() {
            // A pipe (the jobserver's), a socket or a device: reading it would take what is in
            // it from whoever it is for, or never end. What comes out of one is no file's
            // contents: the sandbox notes a read of the random devices as `T random`.
            return Ok((Some(SPECIAL), true));
        }
        let stamp = Stamp::of(&meta);
        let key = Files::key(path, &stamp);
        if let Some(found) = self.stamped.find(&key) {
            return Ok((Some(*self.stamped.get(found)), true));
        }
        let file = std::fs::File::open(path).map_err(unreadable)?;
        let hash = hash_reader(file).map_err(unreadable)?;
        // A file changed while it was read is hashed again at its next check.
        let after = std::fs::metadata(path).map(|it| Stamp::of(&it)).ok();
        let settled = after == Some(stamp);
        if settled {
            self.stamped.intern(key, || hash);
        }
        Ok((Some(hash), settled))
    }

    /// What `path` is as a view holds it ([`Inputs::entry`]), a link not followed, as
    /// blaze-cargo's fetch reads a tree into one: a link is its target, `/`-separated; a file
    /// is executable when any of its execute bits is set. A file's content ID is kept under
    /// what `stat` says of it, as its hash is.
    fn entry(&self, path: &Path) -> Result<Entry, Unreadable> {
        // alloc: error path
        let refused = |error: &str| Unreadable { path: path.to_owned(), error: error.to_owned() };
        // alloc: error path
        let unreadable =
            |error: io::Error| Unreadable { path: path.to_owned(), error: error.to_string() };
        if path.file_name().is_some_and(|it| it.to_str().is_none()) {
            return Err(refused("its name is not UTF-8, which no view holds"));
        }
        // The invariant: the disk is looked at only while no watch vouches for the answer
        // kept. An entry is the path itself, a link not followed, so its answer rests on the
        // names of its directory alone: kept when that directory's name has no link in it
        // (it is its own canonical name, the form a watch is asked in), with the generation
        // read before the look. Until 2026-11-12 every ask looked: a link's native inputs
        // (glibc's `crt*.o` and `lib*.so`, ten each a build) kept every build on NixOS from
        // being answered again without a walk.
        let kept = self.entries.lock().get(path).copied();
        if let Some((generation, entry)) = kept
            && self.dirs.name_unchanged_since(path, generation)
            && !ENTRY_LOOKS_EVERY_TIME.get()
        {
            return Ok(entry);
        }
        let generation = self.dirs.generation();
        let plain = path.parent().is_some_and(|dir| {
            let canonical = std::fs::canonicalize(dir).is_ok_and(|it| it == dir);
            if canonical && kept.is_none() {
                self.dirs.watch_name(path);
            }
            canonical
        });
        let entry = self.entry_now(path)?;
        if plain {
            // alloc: once per path asked of, its name as the key of what was said of it
            self.entries.lock().insert(path.to_path_buf(), (generation, entry));
        }
        Ok(entry)
    }

    /// [`Files::entry`] of the disk now: a look, counted.
    fn entry_now(&self, path: &Path) -> Result<Entry, Unreadable> {
        // alloc: error path
        let refused = |error: &str| Unreadable { path: path.to_owned(), error: error.to_owned() };
        // alloc: error path
        let unreadable =
            |error: io::Error| Unreadable { path: path.to_owned(), error: error.to_string() };
        blaze_vfs::outside::looked(path);
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if missing(&error) => return Ok(Entry::Missing),
            Err(error) => return Err(unreadable(error)),
        };
        let kind = meta.file_type();
        if kind.is_dir() {
            return Ok(Entry::Dir);
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
            return Ok(Entry::File(id, blaze_vfs::Mode::Symlink));
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
        let stamp = Stamp::of(&meta);
        let key = Hasher::new(crate::trace::os_domain!("blaze v2 disk view id stamp 1"))
            .part(&os_bytes(path.as_os_str()))
            .part(&stamp.bytes())
            .finish();
        if let Some(found) = self.stamped.find(&key) {
            return Ok(Entry::File(ContentId(*self.stamped.get(found)), mode));
        }
        let file = std::fs::File::open(path).map_err(unreadable)?;
        let mut hasher = Hasher::new(blaze_vfs::FILE_DOMAIN);
        read_all(file, |bytes| _ = hasher.update(bytes)).map_err(unreadable)?;
        let id = hasher.finish();
        // A file changed while it was read is hashed again at its next check.
        if std::fs::symlink_metadata(path).is_ok_and(|it| Stamp::of(&it) == stamp) {
            self.stamped.intern(key, || id.0);
        }
        Ok(Entry::File(id, mode))
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
/// with and what [`Env`] it answers from, both shared.
#[derive(Clone, Copy)]
pub struct Now<'a> {
    pub files: &'a Files,
    pub env: &'a Env,
}

/// The machine's files as they are, through [`Files::shared`], and an environment of its own.
pub struct Disk {
    files: &'static Files,
    env: Env,
}

impl Disk {
    pub fn new(env: Vec<(OsString, OsString)>) -> Disk {
        Disk { files: Files::shared(), env: Env::new(env) }
    }

    /// The files with the environment of a run that is lent it ([`Env::lent`], whose `sees`
    /// this takes): what a build script's run is assembled and checked against.
    pub fn lent(env: Vec<(OsString, OsString)>, sees: impl Fn(&OsStr) -> bool) -> Disk {
        Disk { files: Files::shared(), env: Env::lent(env, sees) }
    }

    /// The same files with another environment.
    pub fn set_env(&mut self, env: Vec<(OsString, OsString)>) {
        self.env = Env::new(env);
    }

    pub fn now(&self) -> Now<'_> {
        Now { files: self.files, env: &self.env }
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
        self.files.file(path)
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
        // The invariant: the disk is listed only while no watch vouches for the listing kept.
        // A listing is the names directly in the directory and whether each is a directory:
        // it moves only with a change that names an entry of it, the directory itself or
        // one above it, which is what the watches keep of a name in it
        // ([`Dirs::watch_name`], asked here of a name no entry has). Kept for a directory
        // whose own name has no link in it and that lists no link (what a link leads to is
        // not in its directory). Until 2026-11-12 every ask listed: a build script that
        // globs its sources (ring's `crypto/**`, zstd-sys's `zstd/lib/*`) kept every build
        // of anything above it from being answered again without a walk.
        let asked = path.join(LISTING);
        let kept = self.files.listings.lock().get(path).cloned(); // clone: an `Arc`
        if let Some((generation, names)) = &kept
            && self.files.dirs.name_unchanged_since(&asked, *generation)
            && !LISTING_LOOKS_EVERY_TIME.get()
        {
            for (name, dir) in names.iter() {
                out.push(name, *dir);
            }
            out.sort();
            return Ok(true);
        }
        let generation = self.files.dirs.generation();
        blaze_vfs::outside::looked(path);
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if missing(&error) => return Ok(false),
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
        if !links && std::fs::canonicalize(path).is_ok_and(|it| it == path) {
            if kept.is_none() {
                self.files.dirs.watch_name(&asked);
            }
            // alloc: once a directory listed off the disk, its path as the key
            self.files.listings.lock().insert(path.to_path_buf(), (generation, listed.into()));
        }
        Ok(true)
    }

    fn entry(&mut self, path: &Path) -> Result<Entry, Unreadable> {
        self.files.entry(path)
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
    use super::*;

    #[test]
    fn the_disk_hashes_files_lists_directories_and_refuses_nothing_quietly() {
        let dir = std::env::temp_dir().join(format!("blaze-disk-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), b"one").unwrap();
        let mut disk = Disk::new(vec![("B".into(), "2".into()), ("A".into(), "1".into())]);
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
        _ = std::fs::remove_dir_all(&dir);
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
        let mut disk = Disk::lent(vars(), |name| name != "TERM");
        assert_eq!(disk.env(OsStr::new("TERM")), Some(OsStr::new("x")));
        assert_eq!(disk.env_all().len(), 2);
        // The control.
        assert_eq!(Env::new(vars()).vars().len(), 3);
    }

    #[test]
    fn every_check_shares_one_table_and_an_edit_is_a_new_entry() {
        let dir = std::env::temp_dir().join(format!("blaze-files-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a");
        std::fs::write(&path, b"one").unwrap();
        let files = Files::new();
        let (one, two) = (Env::new(vec![("X".into(), "1".into())]), Env::default());
        let hashes: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|n| {
                    let (files, env, path) = (&files, if n % 2 == 0 { &one } else { &two }, &path);
                    scope.spawn(move || Now { files, env }.file(path).unwrap())
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert!(hashes.iter().all(|it| *it == Some(blaze_ids::b3::hash(b"one"))));
        assert_eq!(files.stamped.len(), 1);
        std::fs::write(&path, b"two, longer").unwrap();
        let mut now = Now { files: &files, env: &one };
        assert_eq!(now.file(&path).unwrap(), Some(blaze_ids::b3::hash(b"two, longer")));
        assert_eq!(files.stamped.len(), 2);
        assert_eq!(now.env(OsStr::new("X")), Some(OsStr::new("1")));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_canonical_miss_wants_its_parent_and_nothing_is_vouched_before_the_watch_settles() {
        let made = std::env::temp_dir().join(format!("blaze-files-vouch-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&made);
        std::fs::create_dir_all(made.join("sub")).unwrap();
        let dir = std::fs::canonicalize(&made).unwrap();
        let path = dir.join("a");
        std::fs::write(&path, b"one").unwrap();
        let dirs: &'static Dirs = Box::leak(Box::new(Dirs::new()));
        let files = Files::watched_by(dirs);
        let mut now = Now { files: &files, env: &Env::default() };
        assert_eq!(now.file(&path).unwrap(), Some(blaze_ids::b3::hash(b"one")));
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs.dir(0).path(), dir.as_path());
        std::fs::write(&path, b"two, longer").unwrap();
        assert_eq!(now.file(&path).unwrap(), Some(blaze_ids::b3::hash(b"two, longer")));
        assert_eq!(now.file(&dir.join("gone")).unwrap(), None);
        assert_eq!(dirs.len(), 1);
        let roundabout = dir.join("sub").join("..").join("a");
        assert_eq!(now.file(&roundabout).unwrap(), Some(blaze_ids::b3::hash(b"two, longer")));
        let id = Files::path_id(&roundabout);
        assert!(files.vouched.get(files.vouched.find(&id).unwrap()).names.is_none());
        assert_eq!(dirs.len(), 1);
        _ = std::fs::remove_dir_all(&made);
    }

    /// A path named through symlinks rests on each link and on the file they end at: every
    /// one's directory is wanted, so its answer is vouched for as a canonical path's is (the
    /// macOS SDK's `MacOSX27.2.sdk/usr/lib/libm.tbd` is two links, and a link looks at it in
    /// every build).
    #[cfg(unix)]
    #[test]
    fn a_path_through_symlinks_rests_on_each_link_and_the_file_they_end_at() {
        let made = std::env::temp_dir().join(format!("blaze-files-links-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&made);
        let dir = {
            std::fs::create_dir_all(&made).unwrap();
            std::fs::canonicalize(&made).unwrap()
        };
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

        let dirs: &'static Dirs = Box::leak(Box::new(Dirs::new()));
        let files = Files::watched_by(dirs);
        let path = dir.join("sdks/named/lib/m");
        assert_eq!(files.file(&path).unwrap(), hash);
        let vouched = files.vouched.get(files.vouched.find(&Files::path_id(&path)).unwrap());
        assert_eq!(vouched.names.as_ref().map(|it| it.len()), Some(3));
        let wanted: Vec<&Path> = (0..dirs.len()).map(|at| dirs.dir(at).path()).collect();
        assert!(wanted.contains(&dir.join("sdks").as_path()), "{wanted:?}");
        _ = std::fs::remove_dir_all(&made);
    }

    /// A file asked again, its answer vouched for or its stamp known, allocates nothing: the
    /// path is hashed in place and `stat`ed through a name on the stack.
    #[cfg(unix)]
    #[test]
    fn a_warm_file_allocates_nothing() {
        let made = std::env::temp_dir().join(format!("blaze-files-warm-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&made);
        std::fs::create_dir_all(&made).unwrap();
        let path = std::fs::canonicalize(&made).unwrap().join("a");
        std::fs::write(&path, b"one").unwrap();
        let dirs: &'static Dirs = Box::leak(Box::new(Dirs::new()));
        let files = Files::watched_by(dirs);
        let hash = Some(blaze_ids::b3::hash(b"one"));
        assert_eq!(files.file(&path).unwrap(), hash);
        let warm =
            blaze_alloc_count::assert_at_most(0, "a warm file", || files.file(&path).unwrap());
        assert_eq!(warm, hash);
        let missing = path.with_file_name("gone");
        assert_eq!(files.file(&missing).unwrap(), None);
        let warm = blaze_alloc_count::assert_at_most(0, "a warm missing file", || {
            files.file(&missing).unwrap()
        });
        assert_eq!(warm, None);
        _ = std::fs::remove_dir_all(&made);
    }

    #[test]
    fn a_vouched_answer_reads_back_as_written() {
        let dirs: &'static Dirs = Box::leak(Box::new(Dirs::new()));
        let vouched = Vouched {
            path: OsStr::new("/nowhere/a").into(),
            names: Some(vec![PathBuf::from("/nowhere/a")].into()),
            sequence: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            present: AtomicBool::new(false),
            hash: Default::default(),
        };
        vouched.write(7, Some([3; 32]));
        assert_eq!(vouched.read(), Some((7, Some([3; 32]))));
        vouched.write(9, None);
        assert_eq!(vouched.read(), Some((9, None)));
        vouched.sequence.store(5, Ordering::Relaxed);
        assert_eq!(vouched.read(), None);
        vouched.write(11, Some([1; 32]));
        assert_eq!(vouched.read(), None);
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
            let now = Now { files: Files::shared(), env: &env };
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

        let dir = std::env::temp_dir().join(format!("blaze-tree-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
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
        let mut disk = Disk::new(Vec::new());
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
        _ = std::fs::remove_dir_all(&dir);
    }
}
