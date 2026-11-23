//! The sweep follows what was read; nothing walks the checkout (drive.md, "The system side").
//!
//! The leaf table (the pointer file's second part, seal.rs) holds exactly what the last answer
//! read, as rows ([`Leaf`]): each source file the walk opened (a crate root, or a `mod x;` slot,
//! whose `x/mod.rs` probe rides in its row), each manifest the workspace's layout read or found
//! absent, and each directory it listed, by the digest of the names it was handed. A repeat
//! ([`Follow`]) stats those paths and nothing else: its cost is in proportion to what the build
//! reads, and a path no row names (`.venv`, `target`, a symlinked `.claude`) is never touched. A
//! file is read and hashed only when its [`Stamp`] does not prove it unchanged: unknown,
//! different, or racy (changed within the clock's granularity of the sweep). A directory is
//! listed again only when its own stamp moved. A new file matters only if something probes for
//! it, which is Rust's rule: a `mod x;` probes `x.rs` and `x/mod.rs`, a manifest's layout probes
//! the manifests and lists the directories it names.
//!
//! A cold run (no table) has nothing to follow: the layout discovers forward from the root
//! manifest (units.rs), and the drive opens each file as a record names it (`Effect::Open`,
//! [`probe_slot`], [`opened`]); each read or probe becomes a row ([`Rows`]). Stamps come from a [`Stamps`]
//! strategy (rule 78): [`FromMeta`] in the binary, the simulation's own in the test.

use blaze_drive::{WireError, WireWhy};
use blaze_ids::ContentId;
use blaze_own::{BTreeMap, Texts, Vec};
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Dir, FsKind, Meta, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;
use crate::home::{self, Older};
use crate::work::Unread;

/// Files in the runner's fact mount use absolute logical names. Checkout file observations
/// can only create relative names (engine_script_reads.rs `observed`). No part of a saved
/// question is ever opened as a host path.
pub(crate) const FACT_MOUNT: &str = "/blaze/facts/";
pub(crate) const REPLY_FILES: &str = "/blaze/facts/reply/";
pub(crate) const SHIM_FILE: &str = "/blaze/facts/bin/blaze-fact-shim";
pub(crate) const PINS_FILE: &str = "/blaze/facts/pins";
pub(crate) const EXECUTOR_FILE: &str = "/blaze/facts/executor";

/// The command reads the pins once, however many question files share them. These are the
/// same pins used by ScriptSetup::open before a script can receive a successful reply.
#[derive(Default)]
struct FactFiles<'a> {
    setting: Option<blaze_proc_macro::spawn_route::SpawnSetting>,
    exe_dir: Option<&'a str>,
}

impl<'a> FactFiles<'a> {
    fn read(&mut self, path: &str, alloc: blaze_own::Alloc<'_>) -> Result<ContentId, CliError> {
        if path == SHIM_FILE {
            let installed = || {
                if let Some(dir) = self.exe_dir {
                    return Ok::<_, CliError>(std::path::Path::new(dir).join(blaze_build_script::fact_shim::SHIM_NAME));
                }
                let exe = std::env::current_exe().map_err(|_| CliError::Text { what: "cannot locate blaze to follow the native fact shim image" })?;
                let dir = exe.parent().ok_or(CliError::Text { what: "blaze has no parent folder for its native fact shim" })?;
                Ok::<_, CliError>(dir.join(blaze_build_script::fact_shim::SHIM_NAME))
            };
            #[cfg(test)]
            let shim = self.exe_dir.is_none().then(|| TEST_FACT_INPUTS.with_borrow(|inputs| inputs.as_ref().and_then(|(_, path)| path.clone()))).flatten().map_or_else(installed, Ok)?;
            #[cfg(not(test))]
            let shim = installed()?;
            return match blaze_proc_macro::native_facts::image_digest(&shim) {
                Ok(digest) => Ok(digest),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(crate::engine_script_reads::NONE),
                Err(_) => Err(CliError::Text { what: "cannot read blaze-fact-shim beside blaze while following a native build-script probe" }),
            };
        }
        use blaze_proc_macro::fact_pins::{FactNow, command_spawn_setting, fact_now};
        if self.setting.is_none() {
            let from_host = || command_spawn_setting(crate::graph::TRIPLE, alloc)
                .map_err(|_| CliError::Text { what: "cannot obtain the command's pinned userland and clang setting to replay a build-script fact" });
            #[cfg(test)]
            let read = TEST_FACT_INPUTS.with_borrow(|inputs| inputs.as_ref().map(|(pins, _)| blaze_proc_macro::spawn_route::SpawnSetting::facts_only(pins.clone())))
                .map_or_else(from_host, Ok)?;
            #[cfg(not(test))]
            let read = from_host()?;
            self.setting = Some(read);
        }
        let setting = self.setting.as_ref().expect("opened the command's fact pins above");
        if path == PINS_FILE {
            return Ok(blaze_proc_macro::native_facts::pins_digest(setting.pins.as_ref(), alloc));
        }
        if path == EXECUTOR_FILE {
            return Ok(setting.executor_id(alloc).unwrap_or(crate::engine_script_reads::NONE));
        }
        let subject = path.strip_prefix(REPLY_FILES).ok_or(CliError::Text { what: "unknown file in the saved build-script fact mount" })?;
        match fact_now(subject, setting.pins.as_ref(), alloc) {
            FactNow::Answer(Some(answer)) => Ok(answer),
            FactNow::Answer(None) | FactNow::NoPins => Ok(crate::engine_script_reads::NONE),
            FactNow::NotAFact => Err(CliError::Text { what: "saved build-script reply names no question in the pinned fact table" }),
        }
    }
}

/// Verifies every [`FACT_MOUNT`] leaf row in `leaves` against today's host shim image (under
/// `exe_dir` when provided, else beside `current_exe`) and pinned userland facts. Returns `true`
/// iff every `FACT_MOUNT` row matches its stored digest.
pub(crate) fn verify_fact_leaves(leaves: &Leaves<'_>, exe_dir: Option<&str>, frame: &Frame) -> bool {
    let erased = blaze_own::Erased::new(frame);
    let mut fact_files = FactFiles { setting: None, exe_dir };
    for i in 0..leaves.len() {
        let row = leaves.row(i);
        let path = leaves.path(i);
        if row.kind == LeafKind::ScriptFile && path.starts_with(FACT_MOUNT) {
            match fact_files.read(path, erased.handle()) {
                Ok(id) if id == row.id => {}
                _ => return false,
            }
        }
    }
    true
}

#[cfg(test)]
type TestFactInputs = (blaze_proc_macro::spawn_route::FactPins, Option<std::path::PathBuf>);
#[cfg(test)]
std::thread_local! {
    static TEST_FACT_INPUTS: core::cell::RefCell<Option<TestFactInputs>> = const { core::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) fn with_fact_inputs<R>(pins: &blaze_proc_macro::spawn_route::FactPins, image: Option<&std::path::Path>, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<TestFactInputs>);
    impl Drop for Restore {
        fn drop(&mut self) { TEST_FACT_INPUTS.set(self.0.take()); }
    }
    let _restore = Restore(TEST_FACT_INPUTS.replace(Some((pins.clone(), image.map(std::path::Path::to_owned)))));
    run()
}

/// What a stat says of a file, enough to prove it unchanged when every part is equal: git's
/// index fields less the mode and owner, which a content id does not depend on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub len: u64,
    /// The last change of the content, and of the inode, in ns since the epoch.
    pub mtime_ns: u64,
    pub ctime_ns: u64,
    pub ino: u64,
    pub dev: u64,
}

impl Stamp {
    /// Bytes in a leaf row.
    const BYTES: usize = 40;

    /// The stamp proves the content only when its last change is more than `granularity` before
    /// the sweep began: a change after the stat, in the same tick, would leave the stamp equal.
    pub fn settled(self, swept_at_ns: u64, granularity_ns: u64) -> bool {
        self.mtime_ns.max(self.ctime_ns).saturating_add(granularity_ns) < swept_at_ns
    }

    fn put(self, out: &mut Vec<u8, &Frame>) {
        for part in [self.len, self.mtime_ns, self.ctime_ns, self.ino, self.dev] {
            out.extend_from_slice(&part.to_le_bytes());
        }
    }

    fn get(bytes: &[u8]) -> Stamp {
        let part = |at: usize| {
            let mut word = [0; 8];
            word.copy_from_slice(&bytes[at * 8..at * 8 + 8]);
            u64::from_le_bytes(word)
        };
        Stamp { len: part(0), mtime_ns: part(1), ctime_ns: part(2), ino: part(3), dev: part(4) }
    }
}

/// Where a sweep's stamps come from, and the clock they are on.
pub trait Stamps {
    /// The stamp of what is at `path`, whose stat gave `meta`; `None` when unknown.
    fn stamp(&self, path: &str, meta: &Meta) -> Option<Stamp>;
    /// When the sweep began, on the stamps' clock.
    fn swept_at_ns(&self) -> u64;
    /// The coarsest step of the stamps' clock.
    fn granularity_ns(&self) -> u64;
    /// Whether a batched look ([`bstd::fs::Look`]) of a path proves it the version `old` was
    /// stamped of: a file (`dir` false) or a directory. `false`, the default: a source whose
    /// stamps are not the system's (a test's model) proves nothing by a look, and every row is
    /// statted one by one as before ([`Stamps::proves_by_looks`]).
    fn look_proves(&self, _old: Stamp, _look: &bstd::fs::Look, _dir: bool) -> bool {
        false
    }
    /// Whether [`Stamps::look_proves`] can prove anything: else no look is asked.
    fn proves_by_looks(&self) -> bool {
        false
    }
}

/// The stamps bstd's `Meta` gives, set against the wall clock the program is handed
/// ([`Clock::wall`]: the file system's time base, virtual in the simulation), read once as the
/// sweep begins.
#[derive(Clone, Copy)]
pub struct FromMeta {
    pub swept_at_ns: u64,
}

impl FromMeta {
    pub fn now(clock: &Clock) -> FromMeta {
        FromMeta { swept_at_ns: clock.wall().as_ns() }
    }
}

impl Stamps for FromMeta {
    fn stamp(&self, _: &str, meta: &Meta) -> Option<Stamp> {
        let it = meta.stamp;
        // An inode of 0 is a file system that gives none: its stamps prove nothing.
        (it.ino != 0).then_some(Stamp { len: it.len, mtime_ns: it.modified_ns, ctime_ns: it.changed_ns, ino: it.ino, dev: it.dev })
    }

    fn swept_at_ns(&self) -> u64 {
        self.swept_at_ns
    }

    fn granularity_ns(&self) -> u64 {
        // Linux's file times step with the kernel's tick (`current_time`, up to 10 ms at
        // HZ=100); APFS's in ns. The coarser, until bstd says the file system's.
        10_000_000
    }

    /// What a stat proves, no more and no less: the kind, and every part of the stamp equal
    /// to the look's (the length, both times, the inode and the device: blaze-core's batched
    /// stamp says the device as its stat's `Meta` does), the stamp settled; an inode of 0
    /// proves nothing (as [`FromMeta::stamp`]).
    fn look_proves(&self, old: Stamp, look: &bstd::fs::Look, dir: bool) -> bool {
        let kind = if dir { bstd::fs::EntryKind::Dir } else { bstd::fs::EntryKind::File };
        look.kind == kind
            && look.ino != 0
            && old == Stamp { len: look.len, mtime_ns: look.modified_ns, ctime_ns: look.changed_ns, ino: look.ino, dev: look.dev }
            && old.settled(self.swept_at_ns, self.granularity_ns())
    }

    fn proves_by_looks(&self) -> bool {
        true
    }
}

/// A file's content id: the same domain as `blaze_vfs::FILE_DOMAIN` (one digest of a file in the
/// tree). blaze-vfs is not a dependency here (std, and much else); the constant belongs in
/// blaze-ids (a gap for its lane, rule 30).
pub const FILE_DOMAIN: &str = "blaze v2 vfs file 1";
const LISTING_DOMAIN: &str = "blaze-cli listing 1";
const BOTH_DOMAIN: &str = "blaze-cli module at both 1";
const UNITS_SOURCE_DOMAIN: &str = "blaze-cli units source 2";

/// What a row is of.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LeafKind {
    /// A source file the walk opened, by the path its part names: a crate root, or a `mod x;`
    /// slot `x.rs` ([`MODULE`]).
    Slot = 0,
    /// A manifest the layout read or probed.
    Manifest = 1,
    /// A directory the layout listed: the digest of the names it was handed ([`list_dir`]).
    Listing = 2,
    /// A directory under blaze's folder in the home (`<home>/.blaze`), by its path there: the
    /// proc-macro module store's listing ([`crate::macros::MODULE_LISTING`]), whose names are
    /// the modules there by identity (each renamed into place whole). A fact the answer reads
    /// outside the checkout: in the units' source, so the certificate and the dependency
    /// graph's source move when a module comes or goes, and a `MacroModule` lookup that found
    /// none is a filed failure asked again only then ([`read_home_listing`]).
    HomeListing = 3,
    /// A workspace member proc-macro crate's folder, whole (macro_files.rs): its id is
    /// `blaze_proc_macro::module::files_digest` of every file under it, the digest its module
    /// key holds. In the units' source, so an edit of any of its files moves the dependency
    /// graph's source and each `MacroModule` part's. No stamp of its own: it stands while every
    /// [`LeafKind::MacroFile`] and [`LeafKind::MacroFolder`] row beneath it does
    /// (`macro_files::proven`), so an unchanged crate is read no byte.
    MacroFiles = 4,
    /// A file of a member proc-macro crate: its stamp and its id (`FILE_DOMAIN`), read with the
    /// crate's digest from one read; followed as a manifest is.
    MacroFile = 5,
    /// A folder of a member proc-macro crate: its stamp and the [`listing_id`] of the entries
    /// the files digest walks (`macro_files::walked`); followed as a listing is, so a file made
    /// or removed beneath the crate moves a row.
    MacroFolder = 6,
    /// A checkout file or a file of the runner's [`FACT_MOUNT`]: its id the answer the run saw,
    /// the file's bytes as the sandbox digests them (engine_script_reads.rs). The script reads
    /// its `Given` cell; nothing of the layout does, so it moves no layout.
    ScriptFile = 7,
    /// A checkout folder a member's build script listed (`dir:`): its id the sandbox's listing
    /// answer, every entry one level (engine_script_reads.rs).
    ScriptListing = 8,
    /// A checkout path a member's build script stat'd (`stat:`): its id its kind, and a file's
    /// size; a folder's does not depend on what is in it (engine_script_reads.rs).
    ScriptStat = 9,
    /// A git fact a member's build script asked (`git:`): its id the answer the run saw, as the
    /// sandbox digests it (engine_script_reads.rs `ReadKind::Git`). Read from `.git` files the
    /// journal may not see, so it is always followed (journal_sweep.rs).
    ScriptGit = 10,
}

impl LeafKind {
    /// A row a build script's read made: followed by engine_script_reads.rs `now`, read by a
    /// `Given` cell and never by the layout.
    pub fn by_script(self) -> bool {
        matches!(self, LeafKind::ScriptFile | LeafKind::ScriptListing | LeafKind::ScriptStat | LeafKind::ScriptGit)
    }
}

/// A slot of `mod x;`: `x.rs` and `x/mod.rs` are both probed; the row's stamp and id are the one
/// found.
pub const MODULE: u8 = 1;
/// The slot was found at `x/mod.rs`.
pub const AT_MOD_RS: u8 = 2;
/// Nothing is there: a probe that came back absent. Its stamp is none and its id zeros.
pub const ABSENT: u8 = 4;
/// Both `x.rs` and `x/mod.rs` are there (rustc's E0761): a finding whatever they hold, so
/// neither is read; the id is of the slot's path, the stamp none.
pub const BOTH: u8 = 8;
/// Set on a `ScriptFile` / `ScriptStat` / `ScriptListing` row when every path component from
/// the checkout root to this path was verified not to be a symlink, so the journal's own reach
/// (`Index::reaches`) suffices instead of re-reading every script file on unrelated edits.
pub const DIRECT_PATH: u8 = 16;
const FLAGS: u8 = MODULE | AT_MOD_RS | ABSENT | BOTH | DIRECT_PATH;

/// One row: what was read (or probed) at a path, and how it was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Leaf {
    pub kind: LeafKind,
    pub flags: u8,
    /// The path's index in its table's names.
    pub path: u32,
    pub stamp: Option<Stamp>,
    pub id: ContentId,
}

impl Leaf {
    pub fn absent(&self) -> bool {
        self.flags & ABSENT != 0
    }

    /// What a reader of the row sees: the flags that say where it was found, and the id.
    pub(crate) fn state(&self) -> (u8, ContentId) {
        (self.flags & (AT_MOD_RS | ABSENT | BOTH), self.id)
    }
}

/// A bug the sweep's test plants, to show the test catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A racy stamp is trusted: a change in the tick of the last sweep is missed.
    RacyTrusted,
    /// A probe that came back absent is taken as still absent, unstatted: a file created at a
    /// probed path is missed.
    AbsentForgotten,
}

/// Rows in a frame: the paths in one buffer.
pub struct Rows<'f> {
    pub names: Texts<&'f Frame>,
    pub rows: Vec<Leaf, &'f Frame>,
    /// The checkout's workspace, when this request read its lock's bytes (the layout's
    /// [`read_manifest`] of it, or the follow of its row when its stamp did not prove it): what
    /// the save keys the home's bases by (engine_answer.rs `BASES_ROOT`). `None` when the lock
    /// was not read, or names no path package.
    pub workspace: Option<crate::engine_answer::Workspace>,
}

/// The checkout's lock, at its root: the one manifest row whose bytes name the workspace.
const LOCK_ROW: &str = "Cargo.lock";

impl<'f> Rows<'f> {
    pub fn new_in(frame: &'f Frame) -> Rows<'f> {
        Rows { names: Texts::new_in(frame), rows: Vec::new_in(frame), workspace: None }
    }

    /// The workspace of the lock read as `text`, when `path` is the checkout's lock.
    fn lock_read(&mut self, path: &str, text: &[u8], frame: &Frame) {
        if path == LOCK_ROW {
            self.workspace = crate::engine_answer::Workspace::of_lock(text, frame);
        }
    }

    /// The workspace of the members' own lock (graph.rs `members_lock`), for a layout that reads
    /// no `Cargo.lock` (nothing outside the workspace, units.rs `lay`).
    pub(crate) fn members_read(&mut self, members_lock: &[u8], frame: &Frame) {
        self.workspace = crate::engine_answer::Workspace::of_lock(members_lock, frame);
    }

    /// The id of the checkout's lock row, when there is one and a file was there.
    pub(crate) fn lock_id(&self) -> Option<ContentId> {
        self.rows.iter().rev().find(|leaf| leaf.kind == LeafKind::Manifest && self.path(leaf) == LOCK_ROW && !leaf.absent()).map(|leaf| leaf.id)
    }

    pub fn path(&self, leaf: &Leaf) -> &str {
        self.names.text(leaf.path as usize)
    }

    pub fn push(&mut self, kind: LeafKind, flags: u8, path: &str, stamp: Option<Stamp>, id: ContentId) {
        let path = self.names.push(path) as u32;
        self.rows.push(Leaf { kind, flags, path, stamp, id });
    }

    /// The rows of `kind`, in (path) order.
    pub fn of(&self, kind: LeafKind) -> impl Iterator<Item = &Leaf> + '_ {
        self.rows.iter().filter(move |leaf| leaf.kind == kind)
    }
}

/// The units' source: every manifest and listing row (path, where found, id), in path order.
/// What the layout read: equal sources, equal layouts.
pub fn units_source<'r>(rows: impl Iterator<Item = (&'r str, &'r Leaf)>, frame: &Frame) -> ContentId {
    let mut sorted: Vec<(&str, &Leaf), &Frame> = Vec::new_in(frame);
    // A build script's read is a cell its script reads, never the layout's source.
    sorted.extend(rows.filter(|(_, leaf)| leaf.kind != LeafKind::Slot && !leaf.kind.by_script()));
    sorted.sort_unstable_by(|a, b| (a.1.kind, a.0).cmp(&(b.1.kind, b.0)));
    let mut h = blaze_ids::Hasher::new(UNITS_SOURCE_DOMAIN);
    for (path, leaf) in sorted.iter() {
        let (flags, id) = leaf.state();
        h.update(&[leaf.kind as u8, flags]).part(path.as_bytes()).update(&id.0);
    }
    h.finish()
}

/// What is at `path`: the stamp (settled, else none) and the content id, reusing `old`'s id when
/// its stamp proves the file unchanged; `None` when no file is there (a directory is none).
/// The stat comes before the read: `bytes` holds what was read, whenever one was.
pub(crate) fn look(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    plant: Plant,
    path: &str,
    old: Option<(Stamp, ContentId)>,
    bytes: &mut Bytes<'_>,
) -> Result<Option<(Option<Stamp>, ContentId)>, CliError> {
    match file_meta(cx, root, path)? {
        None => Ok(None),
        Some(meta) => Ok(look_at(cx, root, stamps, plant, path, &meta, old, bytes)?),
    }
}

/// The stat of the file at `path`; `None` when no file is there (a directory is none). Its
/// errors, and [`look_at`]'s and [`slot_now`]'s, are [`Unread`]: `Send`, so a pool worker
/// opens a slot.
fn file_meta(cx: &Cx, root: &Dir<ReadWrite>, path: &str) -> Result<Option<Meta>, Unread> {
    Ok(root.stat(cx, &Path::new(path)?)?.filter(|meta| !meta.dir))
}

/// [`look`], its stat `meta` taken.
fn look_at(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    plant: Plant,
    path: &str,
    meta: &Meta,
    old: Option<(Stamp, ContentId)>,
    bytes: &mut Bytes<'_>,
) -> Result<Option<(Option<Stamp>, ContentId)>, Unread> {
    let at = Path::new(path)?;
    let seen = stamps.stamp(path, meta);
    let stamp = match plant {
        Plant::RacyTrusted => seen,
        Plant::None | Plant::AbsentForgotten => seen.filter(|it| it.settled(stamps.swept_at_ns(), stamps.granularity_ns())),
    };
    if let (Some(now), Some((was, id))) = (stamp, old)
        && now == was
    {
        return Ok(Some((stamp, id)));
    }
    bytes.truncate(0);
    match root.read_all(cx, &at, bytes) {
        Ok(_) => Ok(Some((stamp, ContentId::of(FILE_DOMAIN, bytes.as_slice())))),
        // A symbolic link to a directory: no file.
        Err(error) if error.kind == FsKind::IsADirectory || error.kind == FsKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// `x/mod.rs` of the slot `x.rs`.
pub fn mod_rs_of<'f>(slot: &str, frame: &'f Frame) -> Vec<u8, &'f Frame> {
    let stem = slot.strip_suffix(".rs").unwrap_or(slot);
    let mut out = Vec::with_capacity_in(stem.len() + 7, frame);
    out.extend_from_slice(stem.as_bytes());
    out.extend_from_slice(b"/mod.rs");
    out
}

/// The file a slot was found at: `x.rs`, or `x/mod.rs` ([`AT_MOD_RS`]).
pub fn real_path<'f>(slot: &str, flags: u8, frame: &'f Frame) -> Vec<u8, &'f Frame> {
    match flags & AT_MOD_RS != 0 {
        true => mod_rs_of(slot, frame),
        false => Vec::from_slice_in(slot.as_bytes(), frame),
    }
}

/// [`real_path`] allocated directly in `alloc`, with exact capacity and no frame copy.
pub fn real_path_in<A: core::alloc::Allocator>(slot: &str, flags: u8, alloc: A) -> Vec<u8, A> {
    if flags & AT_MOD_RS != 0 {
        let stem = slot.strip_suffix(".rs").unwrap_or(slot);
        let mut out = Vec::with_capacity_in(stem.len() + 7, alloc);
        out.extend_from_slice(stem.as_bytes());
        out.extend_from_slice(b"/mod.rs");
        out
    } else {
        let mut out = Vec::with_capacity_in(slot.len(), alloc);
        out.extend_from_slice(slot.as_bytes());
        out
    }
}

/// Where a slot is today: its flags and stamp and id, or absent. `old` is its last row.
fn slot_now(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    plant: Plant,
    slot: &str,
    module: bool,
    old: Option<&Leaf>,
    bytes: &mut Bytes<'_>,
    frame: &Frame,
) -> Result<(u8, Option<Stamp>, ContentId), Unread> {
    let was = |at_mod_rs: bool| {
        old.filter(|leaf| !leaf.absent() && leaf.flags & BOTH == 0 && (leaf.flags & AT_MOD_RS != 0) == at_mod_rs)
            .and_then(|leaf| leaf.stamp.map(|stamp| (stamp, leaf.id)))
    };
    let base = if module { MODULE } else { 0 };
    let absent = (base | ABSENT, None, ContentId([0; 32]));
    let rs = file_meta(cx, root, slot)?;
    if !module {
        return Ok(match rs {
            Some(meta) => match look_at(cx, root, stamps, plant, slot, &meta, was(false), bytes)? {
                Some((stamp, id)) => (base, stamp, id),
                None => absent,
            },
            None => absent,
        });
    }
    // The planted bug: a probe absent last time is not looked at again.
    let forgotten = plant == Plant::AbsentForgotten && old.is_some_and(|leaf| leaf.flags & (AT_MOD_RS | BOTH) == 0);
    let mod_rs = mod_rs_of(slot, frame);
    let mod_rs = core::str::from_utf8(&mod_rs).map_err(|_| Unread::Text("a module path is not UTF-8"))?;
    let at_mod = match forgotten {
        true => None,
        false => file_meta(cx, root, mod_rs)?,
    };
    // Each probe stats; only the one file found is read (and only when its stamp does not
    // prove it). Both found is the finding whatever they hold: neither is read.
    let found = match (rs, at_mod) {
        (Some(_), Some(_)) => return Ok((base | BOTH, None, ContentId::of(BOTH_DOMAIN, slot.as_bytes()))),
        (Some(meta), None) => look_at(cx, root, stamps, plant, slot, &meta, was(false), bytes)?.map(|(stamp, id)| (base, stamp, id)),
        (None, Some(meta)) => look_at(cx, root, stamps, plant, mod_rs, &meta, was(true), bytes)?.map(|(stamp, id)| (base | AT_MOD_RS, stamp, id)),
        (None, None) => None,
    };
    Ok(found.unwrap_or(absent))
}

/// What a slot the drive asks for (`Effect::Open`) holds: its flags, stamp and id, read in
/// `frame` (a pool worker's: the opens of a round run together, `work::open_all`).
pub fn probe_slot(cx: &Cx, root: &Dir<ReadWrite>, stamps: &impl Stamps, slot: &str, module: bool, frame: &Frame) -> Result<Probed, Unread> {
    let mut bytes = Bytes::new_in(frame, "a file the walk opened");
    slot_now(cx, root, stamps, Plant::None, slot, module, None, &mut bytes, frame)
}

/// A slot's flags, stamp and id, as [`slot_now`] found them.
pub type Probed = (u8, Option<Stamp>, ContentId);

/// A probed slot's row, pushed onto `rows`; its id, or none when nothing is there.
pub fn opened(slot: &str, (flags, stamp, id): Probed, rows: &mut Rows<'_>) -> Option<ContentId> {
    rows.push(LeafKind::Slot, flags, slot, stamp, id);
    (flags & ABSENT == 0).then_some(id)
}

/// The manifest at `path` for the layout: its text into `text` and its row onto `rows`; `false`
/// when none is there.
pub fn read_manifest(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    path: &str,
    rows: &mut Rows<'_>,
    text: &mut Bytes<'_>,
) -> Result<bool, CliError> {
    match look(cx, root, stamps, Plant::None, path, None, text)? {
        Some((stamp, id)) => {
            rows.push(LeafKind::Manifest, 0, path, stamp, id);
            // No `old` was given: the bytes were read, so `text` holds them.
            rows.lock_read(path, text.as_slice(), cx.frame());
            Ok(true)
        }
        None => {
            rows.push(LeafKind::Manifest, ABSENT, path, None, ContentId([0; 32]));
            Ok(false)
        }
    }
}

/// The names in directory `dir` the layout may read: each subdirectory, `lib.rs` and `main.rs`,
/// and in a directory named `bin` each `.rs` file; in a directory named `src`, only `lib.rs`,
/// `main.rs` and `bin`. What blaze-manifest's layout reads of a listing (member globs match
/// directories; targets are `src/lib.rs`, `src/main.rs`, `src/bin/*.rs`, `src/bin/*/main.rs`),
/// and all it is handed: a file that is none of these never reaches it, so adding one moves
/// nothing. `None` when no directory is there.
///
/// One level, from bstd's `Dir::entries` (sorted by name bytes): the folder's own entries and
/// nothing beneath them. A symbolic link counts as a file and is not followed; a FIFO, a
/// socket, a device or an entry gone since the listing is no name the layout reads.
/// blaze's home in the checkout's root (`.blaze/v2/...`): not listed. The rule's one home is
/// blaze_build_script::served.
pub const HOME: &str = blaze_build_script::served::BLAZE_FOLDER;
/// The ignore file of blaze's folder in the checkout's root, made by the first command (print.rs
/// `ensure_checkout_ignore`) so its state stays out of the checkout's git status.
pub const HOME_IGNORE: &str = ".blaze/.gitignore";
/// Its bytes: every name beneath blaze's folder ignored (journal_sweep.rs
/// `CheckoutChanges::ignored_whole` stands on exactly these).
pub const HOME_IGNORE_TEXT: &[u8] = b"*\n";
/// git's entry in the checkout's root: a directory in a clone, a file (`gitdir: ...`) in a
/// worktree or a submodule. Not listed, whichever it is.
const GIT: &str = ".git";
/// The tag cargo writes in its build directory in the checkout's root
/// (`Workspace::default_target_dir`, the root joined with `target`): a `target` holding it whose
/// first bytes are the Cache Directory Tagging signature is cargo's, no package's, and not
/// listed; any other `target` is a package's folder (blaze_build_script::served, rule 4).
pub(crate) const TARGET_TAG: &str = "target/CACHEDIR.TAG";

/// Whether the root's `target` is cargo's build directory: the first bytes of
/// [`TARGET_TAG`], as many as the signature has (blaze_build_script::served::tagged). Read inside
/// the root's listing, so the listing's row moves when the tag comes or goes. No tag (or no
/// `target`): `false`.
pub(crate) fn cargo_target_tagged(cx: &Cx, root: &Dir<ReadWrite>, frame: &Frame) -> Result<bool, CliError> {
    use blaze_build_script::served::{CACHEDIR_SIGNATURE, tagged};
    let file = match root.open_read(cx, &Path::new(TARGET_TAG)?) {
        Ok(file) => file,
        Err(error) if matches!(error.kind, FsKind::NotFound | FsKind::NotADirectory) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut head = Bytes::new_in(frame, "cargo's CACHEDIR.TAG");
    file.read_at(cx, 0, CACHEDIR_SIGNATURE.len(), &mut head)?;
    Ok(tagged(Some(head.as_slice())))
}

pub fn list_dir<'f>(cx: &Cx, root: &Dir<ReadWrite>, dir: &str, frame: &'f Frame) -> Result<Option<Listing<'f>>, CliError> {
    let mut names: Texts<&'f Frame> = Texts::new_in(frame);
    let mut kinds: Vec<bool, &Frame> = Vec::new_in(frame);
    // The checkout's root is the `Dir` itself: always there (the command opened it).
    let listed = match dir.is_empty() {
        true => root.own_entries(cx)?,
        false => match root.entries(cx, &Path::new(dir)?)? {
            Some(listed) => listed,
            None => return Ok(None),
        },
    };
    // Only the root's listing asks: only there is a `target` cargo's.
    let target_tagged = dir.is_empty() && cargo_target_tagged(cx, root, frame)?;
    let last = dir.rsplit('/').next().unwrap_or(dir);
    for i in 0..listed.len() {
        let first = listed.name(i);
        let is_dir = match listed.kind(i) {
            bstd::fs::EntryKind::Dir => true,
            bstd::fs::EntryKind::File | bstd::fs::EntryKind::Link | bstd::fs::EntryKind::Unknown => false,
            bstd::fs::EntryKind::Other | bstd::fs::EntryKind::Gone => continue,
        };
        if wanted(last, first, is_dir, target_tagged) {
            names.push(first);
            kinds.push(is_dir);
        }
    }
    Ok(Some(Listing { names, dirs: kinds }))
}

/// Whether `name`, in the checkout's root, is blaze's, git's or cargo's folder (`target` only
/// when `target_tagged`, [`cargo_target_tagged`]): no row is under it, and the journal's names
/// there move none (journal_sweep.rs).
pub(crate) fn not_the_checkouts(name: &str, target_tagged: bool) -> bool {
    use blaze_build_script::served::{CACHEDIR_SIGNATURE, served_at_root};
    name == GIT || !served_at_root(name.as_bytes(), target_tagged.then_some(CACHEDIR_SIGNATURE))
}

/// Whether the layout reads entry `name` of the folder whose last component is `last` (`""`
/// for the checkout's root); `target_tagged`: the root's `target` is cargo's
/// ([`cargo_target_tagged`]).
pub(crate) fn wanted(last: &str, name: &str, is_dir: bool, target_tagged: bool) -> bool {
    match (last, is_dir) {
        // blaze's own home is derived data, never the checkout's: a first command makes it,
        // and a listing that held it would move the layout under the second (a certificate
        // missed after every cold run, the probe's "sometimes", 2026-11-16). The same for what
        // git and cargo keep in the root: the listing is in the units' source
        // (`units_source`), so a worktree (whose `.git` is a file, a clone's a directory) or a
        // checkout where cargo made `target/` laid out its units again (`Units` and
        // `UnitGraph` rerun on a second checkout's first check, 2026-11-17). No manifest is
        // under either: a member glob that matched one named a missing manifest. A `target`
        // without cargo's tag is a package's folder and listed (rule 4).
        ("", _) if not_the_checkouts(name, target_tagged) => false,
        ("src", true) => name == "bin",
        ("src", false) => name == "lib.rs" || name == "main.rs",
        ("bin", false) => name.ends_with(".rs"),
        // A member's test, example and bench targets are found by their files
        // (blaze-manifest layout/extra.rs `inferred`).
        ("tests" | "examples" | "benches", false) => name.ends_with(".rs"),
        (_, true) => true,
        // A package's directory: `build.rs` is its build script when `package.build` is not
        // written (blaze-manifest layout `RawBuild::Auto`). Left out, a member's script was
        // never seen and its `rustc-cfg` never applied (tests/build_scripts.rs, 2026-11-17).
        (_, false) => name == "lib.rs" || name == "main.rs" || name == "build.rs",
    }
}

/// A directory's entries as the layout is handed them, in name order.
pub struct Listing<'f> {
    pub names: Texts<&'f Frame>,
    /// Whether entry `i` is a directory.
    pub dirs: Vec<bool, &'f Frame>,
}

impl Listing<'_> {
    pub fn entries(&self) -> impl Iterator<Item = (&str, bool)> + '_ {
        self.dirs.iter().enumerate().map(|(i, is_dir)| (self.names.text(i), *is_dir))
    }
}

/// The digest of a listing as handed to the layout: (name, is a directory), in name order.
pub fn listing_id<'e>(entries: Option<impl Iterator<Item = (&'e str, bool)>>) -> (u8, ContentId) {
    let Some(entries) = entries else { return (ABSENT, ContentId([0; 32])) };
    let mut h = blaze_ids::Hasher::new(LISTING_DOMAIN);
    for (name, is_dir) in entries {
        h.part(name.as_bytes()).update(&[u8::from(is_dir)]);
    }
    (0, h.finish())
}

/// The stamp of directory `dir` (its entries' last change), settled, or none.
pub(crate) fn dir_stamp(cx: &Cx, root: &Dir<ReadWrite>, stamps: &impl Stamps, plant: Plant, dir: &str) -> Result<Option<Option<Stamp>>, CliError> {
    let meta = match dir.is_empty() {
        // The checkout's root: its stamp is not asked (a listing of it is listed again).
        true => return Ok(Some(None)),
        false => root.stat(cx, &Path::new(dir)?)?,
    };
    Ok(match meta {
        Some(meta) if meta.dir => Some(match plant {
            Plant::RacyTrusted => stamps.stamp(dir, &meta),
            Plant::None | Plant::AbsentForgotten => {
                stamps.stamp(dir, &meta).filter(|it| it.settled(stamps.swept_at_ns(), stamps.granularity_ns()))
            }
        }),
        _ => None,
    })
}

/// A listing for the layout: its entries, and its row onto `rows`.
pub fn read_listing<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    dir: &str,
    rows: &mut Rows<'_>,
    frame: &'f Frame,
) -> Result<Option<Listing<'f>>, CliError> {
    let stamp = dir_stamp(cx, root, stamps, Plant::None, dir)?.flatten();
    let entries = list_dir(cx, root, dir, frame)?;
    let (flags, id) = listing_id(entries.as_ref().map(Listing::entries));
    rows.push(LeafKind::Listing, flags, dir, if entries.is_some() { stamp } else { None }, id);
    Ok(entries)
}

/// The row of `dir` under blaze's folder in the home (`home`; none with no home, or no such
/// folder: an absent row), onto `rows`: a [`LeafKind::HomeListing`]. Read once per layout; the
/// sweep follows it as it follows a listing in the checkout.
pub fn read_home_listing(cx: &Cx, home: Option<&Dir<ReadWrite>>, stamps: &impl Stamps, dir: &str, rows: &mut Rows<'_>, frame: &Frame) -> Result<(), CliError> {
    let (flags, stamp, id) = home_listing_now(cx, home, stamps, Plant::None, dir, None, frame)?;
    rows.push(LeafKind::HomeListing, flags, dir, stamp, id);
    Ok(())
}

/// A home listing's row today: `old`'s id kept when its stamp proves the folder unchanged.
fn home_listing_now(
    cx: &Cx,
    home: Option<&Dir<ReadWrite>>,
    stamps: &impl Stamps,
    plant: Plant,
    dir: &str,
    old: Option<&Leaf>,
    frame: &Frame,
) -> Result<(u8, Option<Stamp>, ContentId), CliError> {
    let Some(home) = home else { return Ok((ABSENT, None, ContentId([0; 32]))) };
    Ok(match dir_stamp(cx, home, stamps, plant, dir)? {
        None => (ABSENT, None, ContentId([0; 32])),
        Some(Some(stamp)) if old.is_some_and(|old| old.stamp == Some(stamp) && !old.absent()) => (0, Some(stamp), old.expect("matched above").id),
        Some(stamp) => {
            let entries = list_dir(cx, home, dir, frame)?;
            let (flags, id) = listing_id(entries.as_ref().map(Listing::entries));
            (flags, stamp.filter(|_| flags & ABSENT == 0), id)
        }
    })
}

// ---- The batched looks (thoughts2 1.1): the follow's stats in one call a folder ----
//
// Until 2026-11-21 the follow statted each row it looked at with its own `Dir::stat`: one trip
// through the effect layer a path (a kqueue file thread and back on macOS), in a serial loop,
// so a sweep after a restart (no journal: every row looked at) cost the SUM of its paths'
// trips (tempo, 2026-11-16: 8.8 ms on one worker for its rows, the trip not the call). Now,
// before the loop, every path the loop would stat for a slot, a module slot's two probes, a
// manifest, a macro crate's file or a listing's folder is asked at once ([`Looked::of`], bstd
// `Dir::facts_of`): grouped by folder, a folder with 8 or more asked names listed whole by one
// `getattrlistbulk` on macOS, every other path in ONE `Effect::Stamp` (io_uring: one
// submission of `statx`es, 64 a chunk, blaze-core uring.rs `STAMP_CHUNK`; kqueue: `fstatat`s
// relative to the root on a file thread, blaze-core threads/inline.rs), every effect handed
// to the driver before the first is waited for. A row whose look proves it the version its
// stamp was of ([`Stamps::look_proves`]), or a row absent last time whose paths are all gone,
// stands as the table has it with nothing more asked; every other row (moved, unstamped, a
// symbolic link, a look the system said nothing of) takes the loop's path as before: its stat,
// and its read where the stamp does not prove it.
//
// THE FLOOR (rule 67): one batched call a folder (a listing on macOS, a stamp chunk on Linux),
// the wall time bounded by the slowest folder, not the sum of the paths; the row decisions are
// a merge over sorted columns in the request's frame. The counts say how near ([`LookCounts`],
// the `sweep:` line).
//
// THE SEAM (rule 78): [`Walk`], `Batched` the default, `Serial` the control (each row statted
// one by one), tested equal on a simulated world whose completions are shuffled
// (tests/sweep_looks_tests.rs). A home listing, a macro crate's folder and a script's read stay
// serial: a handful of rows, each in another directory or the scripts' own reader.

/// How the follow asks the system for its rows' stamps: the one seam (rule 78).
pub trait Walk {
    /// Every path asked in batched looks before the loop ([`Looked::of`]).
    const BATCHED: bool;
    /// The bug the looks' test plants in the walk; [`LookPlant::None`] for every real one.
    const PLANT: LookPlant = LookPlant::None;
}

/// A bug the looks' test (tests/sweep_looks_tests.rs) plants in a batched walk, to show it is
/// caught.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LookPlant {
    None,
    /// A file's look proves it by its length alone: its times, inode and device unread.
    #[cfg_attr(not(test), allow(dead_code, reason = "planted by the looks' test alone (`TimesIgnored`)"))]
    TimesIgnored,
    /// A look proves a row with its device unread: a file replaced by one on another mount
    /// with the same inode number, length and times is missed.
    #[cfg_attr(not(test), allow(dead_code, reason = "planted by the looks' test alone (`DevIgnored`)"))]
    DevIgnored,
}

/// The batched looks: the default.
pub enum Batched {}

impl Walk for Batched {
    const BATCHED: bool = true;
}

/// Each row statted one by one, as before 2026-11-21: the control.
pub enum Serial {}

impl Walk for Serial {
    const BATCHED: bool = false;
}

/// [`Batched`] with [`LookPlant::TimesIgnored`].
#[cfg(test)]
pub(crate) enum TimesIgnored {}

#[cfg(test)]
impl Walk for TimesIgnored {
    const BATCHED: bool = true;
    const PLANT: LookPlant = LookPlant::TimesIgnored;
}

/// [`Batched`] with [`LookPlant::DevIgnored`].
#[cfg(test)]
pub(crate) enum DevIgnored {}

#[cfg(test)]
impl Walk for DevIgnored {
    const BATCHED: bool = true;
    const PLANT: LookPlant = LookPlant::DevIgnored;
}

/// What the follow's batched looks did: the `sweep:` line's `looks:` part.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LookCounts {
    /// Paths asked, each once.
    pub asked: u32,
    /// Asks of `facts_of` (one a `STAMP_MOST` paths).
    pub batches: u32,
    /// Folders listed whole (macOS `getattrlistbulk`), and names stamped in the batches' stamp
    /// effects (bstd `Facts::listed`, `Facts::stamped`).
    pub listed: u32,
    pub stamped: u32,
    /// Rows the looks proved: nothing more asked of them.
    pub proved: u32,
}

/// The looks of the rows' paths ([`Looked::of`]): per row, the index of its path's look (a
/// module slot's second, its `x/mod.rs`), [`NO_LOOK`] where none was asked.
struct Looked<'f> {
    at: Vec<[u32; 2], &'f Frame>,
    looks: Vec<bstd::fs::Look, &'f Frame>,
    counts: LookCounts,
}

const NO_LOOK: u32 = u32::MAX;

impl<'f> Looked<'f> {
    fn none(frame: &'f Frame) -> Looked<'f> {
        Looked { at: Vec::new_in(frame), looks: Vec::new_in(frame), counts: LookCounts::default() }
    }

    /// Every path of the rows to be looked at (`asked(i)`), sorted and each once, in batches
    /// of at most `STAMP_MOST` (blaze-core): the module's doc.
    fn of(cx: &Cx, root: &Dir<ReadWrite>, last: &Leaves<'_>, asked: impl Fn(usize) -> bool, frame: &'f Frame) -> Result<Looked<'f>, CliError> {
        let cx = cx.span("sweep: the rows' paths looked at in one batch a folder (bstd facts_of)");
        let mut at = Vec::with_capacity_in(last.len(), frame);
        // (path, row, which): which 1 is a module slot's `x/mod.rs`.
        let mut wanted: Vec<(&str, u32, u8), &Frame> = Vec::new_in(frame);
        for i in 0..last.len() {
            at.push([NO_LOOK; 2]);
            if !asked(i) {
                continue;
            }
            let old = last.row(i);
            let path = last.path(i);
            let row = u32::try_from(i).map_err(|_| CliError::Text { what: "sweep.rs looks: a leaf table past 2^32 rows" })?;
            match old.kind {
                LeafKind::Slot => {
                    wanted.push((path, row, 0));
                    if old.flags & MODULE != 0 {
                        let mod_rs = mod_rs_of(path, frame).into_bump_slice();
                        let mod_rs = core::str::from_utf8(mod_rs).map_err(|_| CliError::Text { what: "a module path is not UTF-8" })?;
                        wanted.push((mod_rs, row, 1));
                    }
                }
                LeafKind::Manifest | LeafKind::MacroFile => wanted.push((path, row, 0)),
                // The checkout's root is listed again whatever its stamp (`dir_stamp`).
                LeafKind::Listing if !path.is_empty() => wanted.push((path, row, 0)),
                _ => {}
            }
        }
        wanted.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut paths: Vec<Path, &Frame> = Vec::new_in(frame);
        for (text, row, which) in wanted.iter() {
            if paths.last().is_none_or(|last: &Path| last.as_str() != *text) {
                paths.push(Path::new(text)?);
            }
            let index = u32::try_from(paths.len() - 1).map_err(|_| CliError::Text { what: "sweep.rs looks: past 2^32 paths" })?;
            at[*row as usize][usize::from(*which)] = index;
        }
        let mut looks = Vec::with_capacity_in(paths.len(), frame);
        let mut counts = LookCounts { asked: u32::try_from(paths.len()).unwrap_or(u32::MAX), ..LookCounts::default() };
        for batch in paths.chunks(blaze_core::STAMP_MOST) {
            let facts = root.facts_of(&cx, batch, frame).map_err(facts_error)?;
            counts.batches = counts.batches.saturating_add(1);
            counts.listed = counts.listed.saturating_add(facts.listed());
            counts.stamped = counts.stamped.saturating_add(facts.stamped());
            looks.extend(facts.iter());
        }
        Ok(Looked { at, looks, counts })
    }

    /// Row `i`'s look of its path (`which` 0) or of a module slot's `x/mod.rs` (1).
    fn look(&self, i: usize, which: usize) -> Option<&bstd::fs::Look> {
        let index = *self.at.get(i)?.get(which)?;
        (index != NO_LOOK).then(|| self.looks.get(index as usize)).flatten()
    }

    /// Row `old`'s (at `i`) state as the table has it, when its looks prove it stands; `None`:
    /// the loop's own path decides it.
    fn proves<W: Walk>(&self, stamps: &impl Stamps, i: usize, old: &Leaf) -> Option<(u8, Option<Stamp>, ContentId)> {
        let gone = |which: usize| self.look(i, which).is_some_and(|look| look.kind == bstd::fs::EntryKind::Gone);
        let proved = |which: usize, dir: bool| match (old.stamp, self.look(i, which)) {
            (Some(stamp), Some(look)) => match W::PLANT {
                LookPlant::None => stamps.look_proves(stamp, look, dir),
                LookPlant::TimesIgnored if !dir => look.kind == bstd::fs::EntryKind::File && look.len == stamp.len,
                LookPlant::TimesIgnored => stamps.look_proves(stamp, look, dir),
                LookPlant::DevIgnored => stamps.look_proves(stamp, &bstd::fs::Look { dev: stamp.dev, ..*look }, dir),
            },
            _ => false,
        };
        let module = old.flags & MODULE != 0;
        let stands = match old.kind {
            LeafKind::Slot if old.flags & BOTH != 0 => false,
            LeafKind::Slot if old.absent() => gone(0) && (!module || gone(1)),
            LeafKind::Slot if old.flags & AT_MOD_RS != 0 => module && gone(0) && proved(1, false),
            LeafKind::Slot => proved(0, false) && (!module || gone(1)),
            LeafKind::Manifest | LeafKind::MacroFile if old.absent() => gone(0),
            LeafKind::Manifest | LeafKind::MacroFile => proved(0, false),
            LeafKind::Listing if old.absent() => gone(0),
            LeafKind::Listing => proved(0, true),
            _ => false,
        };
        stands.then_some((old.flags, old.stamp, old.id))
    }
}

/// A batched look's refusal as the command's error. Unsorted and too many cannot be (the paths
/// are sorted, each once, and cut at the cap above): named, never quietly passed.
fn facts_error(error: bstd::fs::FactsError) -> CliError {
    match error {
        bstd::fs::FactsError::Fs(error) => CliError::Fs(error),
        bstd::fs::FactsError::OutOfMemory(_) => CliError::Text { what: "sweep.rs looks: a column of the batched looks had no room in the request's frame" },
        bstd::fs::FactsError::Unsorted { .. } => CliError::Text { what: "sweep.rs looks: the paths asked were not sorted (a bug of `Looked::of`)" },
        bstd::fs::FactsError::TooMany { .. } => CliError::Text { what: "sweep.rs looks: more paths than one batch takes (a bug of `Looked::of`)" },
    }
}

/// A repeat of the last answer's reads: each row looked at again.
pub struct Follow<S> {
    pub stamps: S,
    pub plant: Plant,
}

/// The last answer's reads as they are today.
pub struct Followed<'f> {
    /// Today's rows, one per row of the table, in its order.
    pub now: Rows<'f>,
    /// Each row's state in the table: what changed is what differs.
    pub was: Vec<(u8, ContentId), &'f Frame>,
    /// A manifest or a listing moved: the layout is read again.
    pub layout_moved: bool,
    /// What the batched looks did ([`Walk`]); all zero under [`Serial`].
    pub looks: LookCounts,
}

impl Followed<'_> {
    /// Row `i` changed since the table.
    pub fn changed(&self, i: usize) -> bool {
        self.now.rows[i].state() != self.was[i]
    }
}

impl<S: Stamps> Follow<S> {
    /// The rows' paths in batched looks first ([`Batched`], the module's "batched looks"; on
    /// the bstd pool, each stat a task, it was slower: tempo, 2026-11-16, 8.8 ms on one worker,
    /// 11.9 to 12.3 ms on 18, the cost being each stat's trip through the effect layer, not
    /// the call). `home`: blaze's folder in the home, where a [`LeafKind::HomeListing`] row is
    /// (none: such a row is absent today).
    pub fn follow<'f>(&self, cx: &Cx, root: &Dir<ReadWrite>, home: Option<&Dir<ReadWrite>>, last: &Leaves<'_>, frame: &'f Frame) -> Result<Followed<'f>, CliError> {
        self.follow_marked(cx, root, home, last, None, frame).map(|(followed, _)| followed)
    }

    /// [`Follow::follow`] of the rows `marked` (one mark a row of `last`; `None`: all) and the
    /// rows looked at: an unmarked row is today's row as the table has it, nothing asked of
    /// it (journal_sweep.rs says why that is sound: the journal named no change to it since
    /// its read). A [`LeafKind::MacroFiles`] row is decided by its files' and folders' rows
    /// either way, and not counted.
    pub fn follow_marked<'f>(&self, cx: &Cx, root: &Dir<ReadWrite>, home: Option<&Dir<ReadWrite>>, last: &Leaves<'_>, marked: Option<&[bool]>, frame: &'f Frame) -> Result<(Followed<'f>, u32), CliError> {
        self.follow_served(cx, root, home, last, marked, None, frame)
    }

    /// [`Follow::follow_marked`] by the walk `W` ([`Batched`] or the control [`Serial`]).
    #[cfg_attr(not(test), allow(dead_code, reason = "the walks' test (tests/sweep_looks_tests.rs) asks each walk by name"))]
    pub(crate) fn follow_marked_by<'f, W: Walk>(&self, cx: &Cx, root: &Dir<ReadWrite>, home: Option<&Dir<ReadWrite>>, last: &Leaves<'_>, marked: Option<&[bool]>, frame: &'f Frame) -> Result<(Followed<'f>, u32), CliError> {
        self.follow_served_by::<W>(cx, root, home, last, marked, None, false, frame)
    }

    /// [`Follow::follow_served_by`] with the batched looks.
    pub(crate) fn follow_served<'f>(
        &self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        home: Option<&Dir<ReadWrite>>,
        last: &Leaves<'_>,
        marked: Option<&[bool]>,
        served: Option<&mut crate::engine_script_reads::CheckoutSweep<'_, '_>>,
        frame: &'f Frame,
    ) -> Result<(Followed<'f>, u32), CliError> {
        self.follow_served_by::<Batched>(cx, root, home, last, marked, served, false, frame)
    }

    /// [`Follow::follow_served`] with optional `facts_vouched` proof from the resident daemon's
    /// synchronous vnode watcher (skipping redundant 800 KB `blaze-fact-shim` re-hashing when
    /// `KqueueVnode` proved all [`FACT_MOUNT`] inputs unchanged with 0 events).
    #[expect(clippy::too_many_arguments, reason = "follow_served with explicit facts_vouched proof")]
    pub(crate) fn follow_served_with_facts<'f>(
        &self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        home: Option<&Dir<ReadWrite>>,
        last: &Leaves<'_>,
        marked: Option<&[bool]>,
        served: Option<&mut crate::engine_script_reads::CheckoutSweep<'_, '_>>,
        facts_vouched: bool,
        frame: &'f Frame,
    ) -> Result<(Followed<'f>, u32), CliError> {
        self.follow_served_by::<Batched>(cx, root, home, last, marked, served, facts_vouched, frame)
    }

    /// A served script observation can depend on paths beyond its recorded name, including
    /// symlink targets and, for a git fact, every tracked file: whether the journal reaches it
    /// is journal_sweep.rs `ServedReach`'s, folded into `marked`, so an unmarked row is kept
    /// here as any other. Each served row's ask is bracketed by two events in a span naming its
    /// kind, so the profile says what the served reads cost.
    #[expect(clippy::too_many_arguments, reason = "sweep follow: each input one capability or proof")]
    pub(crate) fn follow_served_by<'f, W: Walk>(
        &self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        home: Option<&Dir<ReadWrite>>,
        last: &Leaves<'_>,
        marked: Option<&[bool]>,
        mut served: Option<&mut crate::engine_script_reads::CheckoutSweep<'_, '_>>,
        facts_vouched: bool,
        frame: &'f Frame,
    ) -> Result<(Followed<'f>, u32), CliError> {
        let cx = cx.span("sweep: stat each path the last answer read, and read only what its stamp does not prove");
        let mut statted = 0u32;
        let mut now = Rows::new_in(frame);
        let mut was = Vec::with_capacity_in(last.len(), frame);
        let mut bytes = Bytes::new_in(frame, "a file the last answer read");
        let mut layout_moved = false;
        let erased = blaze_own::Erased::new(frame);
        let mut fact_files = FactFiles::default();
        // The `MacroFiles` rows, decided after the loop: their files' and folders' rows come
        // after them in the table's (kind, path) order.
        let mut crates: Vec<usize, &Frame> = Vec::new_in(frame);
        let unmarked = |i: usize| marked.is_some_and(|marked| !marked.get(i).copied().unwrap_or(true));
        // The rows' paths in one batch a folder, first (the module's "batched looks").
        let mut looked = match W::BATCHED && self.plant == Plant::None && self.stamps.proves_by_looks() {
            true => Looked::of(&cx, root, last, |i| !unmarked(i), frame)?,
            false => Looked::none(frame),
        };
        for i in 0..last.len() {
            let old = last.row(i);
            let path = last.path(i);
            let old_state = old.state();
            let fact_file = old.kind == LeafKind::ScriptFile && path.starts_with(FACT_MOUNT);
            if unmarked(i) && (!fact_file || facts_vouched) && old.kind != LeafKind::MacroFiles {
                now.push(old.kind, old.flags, path, old.stamp, old.id);
                was.push(old_state);
                continue;
            }
            if old.kind != LeafKind::MacroFiles {
                statted = statted.saturating_add(1);
            }
            if let Some((flags, stamp, id)) = looked.proves::<W>(&self.stamps, i, &old) {
                looked.counts.proved = looked.counts.proved.saturating_add(1);
                now.push(old.kind, flags, path, stamp, id);
                was.push(old_state);
                continue;
            }
            let (flags, stamp, id) = match old.kind {
                LeafKind::Slot => slot_now(&cx, root, &self.stamps, self.plant, path, old.flags & MODULE != 0, Some(&old), &mut bytes, frame)?,
                LeafKind::Manifest if old.absent() && self.plant == Plant::AbsentForgotten => (ABSENT, None, old.id),
                LeafKind::Manifest | LeafKind::MacroFile => {
                    let reuse = old.stamp.filter(|_| !old.absent()).map(|stamp| (stamp, old.id));
                    // Emptied first: after the look, bytes are there only when it read them.
                    bytes.truncate(0);
                    match look(&cx, root, &self.stamps, self.plant, path, reuse, &mut bytes)? {
                        Some((stamp, id)) => {
                            if old.kind == LeafKind::Manifest && !bytes.as_slice().is_empty() {
                                now.lock_read(path, bytes.as_slice(), frame);
                            }
                            (0, stamp, id)
                        }
                        None => (ABSENT, None, ContentId([0; 32])),
                    }
                }
                LeafKind::Listing if old.absent() && self.plant == Plant::AbsentForgotten => (ABSENT, None, old.id),
                LeafKind::Listing => match dir_stamp(&cx, root, &self.stamps, self.plant, path)? {
                    None => (ABSENT, None, ContentId([0; 32])),
                    Some(Some(stamp)) if old.stamp == Some(stamp) && !old.absent() => (0, Some(stamp), old.id),
                    Some(stamp) => {
                        let entries = list_dir(&cx, root, path, frame)?;
                        let (flags, id) = listing_id(entries.as_ref().map(Listing::entries));
                        (flags, stamp.filter(|_| flags & ABSENT == 0), id)
                    }
                },
                LeafKind::HomeListing => home_listing_now(&cx, home, &self.stamps, self.plant, path, Some(&old), frame)?,
                // Kept here and decided below, when its files' and folders' rows are followed.
                LeafKind::MacroFiles => {
                    crates.push(i);
                    (old.flags, None, old.id)
                }
                LeafKind::MacroFolder => {
                    // A crate's own folder is the one its `MacroFiles` row names: those rows
                    // come first in the table's (kind, path) order, so all are in `crates` here.
                    let own = crates.iter().any(|&at| last.path(at) == path);
                    let place = if own { blaze_build_script::served::InPackage::Root } else { blaze_build_script::served::InPackage::Beneath };
                    crate::macro_files::folder_now(&cx, root, &self.stamps, self.plant, path, place, Some(&old), frame)?
                }
                LeafKind::ScriptFile if fact_file => (0, None, fact_files.read(path, erased.handle())?),
                LeafKind::ScriptFile | LeafKind::ScriptListing | LeafKind::ScriptStat | LeafKind::ScriptGit => {
                    let kind = crate::engine_script_reads::ReadKind::of_leaf(old.kind).ok_or(CliError::Text { what: "a script read's leaf row has no read kind (sweep.rs `follow_served`)" })?;
                    match served.as_deref_mut() {
                        Some(checkout) => {
                            // The served tree's reads go around the effect layer (std paths,
                            // a git status's walk): timed by the two events, not by a wait.
                            let asked = cx.span(match old.kind {
                                LeafKind::ScriptGit => "sweep: a build script's git fact asked again of its served tree",
                                LeafKind::ScriptListing => "sweep: a build script's listing asked again of its served tree",
                                LeafKind::ScriptStat => "sweep: a build script's stat asked again of its served tree",
                                _ => "sweep: a build script's file asked again of its served tree",
                            });
                            let before_stamp = match kind {
                                crate::engine_script_reads::ReadKind::File | crate::engine_script_reads::ReadKind::Listing => {
                                    crate::engine_script_reads::settled_path_stamp(&cx, root, &self.stamps, self.plant, path)
                                }
                                crate::engine_script_reads::ReadKind::Stat | crate::engine_script_reads::ReadKind::Git => None,
                            };
                            asked.event("asked");
                            let (flags, _, id) = checkout.row(kind, path)?;
                            asked.event("answered");
                            let stamp = match crate::engine_script_reads::settled_meta_stamp(&cx, root, &self.stamps, self.plant, kind, path, id) {
                                Some(stamp) => Some(stamp),
                                None if matches!(kind, crate::engine_script_reads::ReadKind::File | crate::engine_script_reads::ReadKind::Listing)
                                    && before_stamp.is_some()
                                    && id != crate::engine_script_reads::NONE
                                    && id != crate::engine_script_reads::DENIED =>
                                {
                                    let after_stamp = crate::engine_script_reads::settled_path_stamp(&cx, root, &self.stamps, self.plant, path);
                                    (after_stamp == before_stamp).then_some(before_stamp).flatten()
                                }
                                None => None,
                            };
                            (flags, stamp, id)
                        }
                        None => crate::engine_script_reads::now(&cx, root, &self.stamps, self.plant, kind, path, Some((old.stamp, old.id)), frame)?,
                    }
                }
            };
            now.push(old.kind, flags, path, stamp, id);
            was.push(old_state);
            if old.kind != LeafKind::Slot && !old.kind.by_script() && now.rows[i].state() != old_state {
                layout_moved = true;
            }
        }
        // A member macro crate's digest stands, no file of it read, while every row of its files
        // and folders does; else the layout reads the crate whole once (macro_files.rs
        // `read_row`: the digest and the files' ids from one read), and the row reads as moved
        // (zeros) until then.
        for &i in crates.iter() {
            let stands = crate::macro_files::proven(&now, &was, now.path(&now.rows[i]));
            if !stands {
                now.rows[i].flags = 0;
                now.rows[i].id = ContentId([0; 32]);
                layout_moved = true;
            }
        }
        Ok((Followed { now, was, layout_moved, looks: looked.counts }, statted))
    }
}

/// What an [`Older`] leaf table is named as: it is the pointer file's second part (seal.rs).
const LEAVES: &str = "a pointer in .blaze/v2/at (its leaf table)";
/// What the last answer read, as rows read in place:
///
/// ```text
/// "BCL6" | rows u32 | name bytes u32 | 0 u32
/// names: the paths' bytes, then rows x end u32
/// rows:  rows x (kind u8, flags u8, 0 u16, stamp [40], digest [32])   sorted by (kind, path)
/// ```
///
/// A stamp of zeros is none (an inode of 0 proves nothing). BCL6 had no `ScriptFile`,
/// `ScriptListing` and `ScriptStat` kinds (a member build script's reads of the checkout,
/// engine_script_reads.rs). BCL5 had no `MacroFile` and
/// `MacroFolder` kinds (member-macro-stamp-1: a member proc-macro crate's files and folders, each
/// with its stamp). BCL4 had no `MacroFiles` kind
/// (member-macro-module-2: a member proc-macro crate's files, whole). BCL3 was the whole checkout's tree
/// (directories and files), the walk this replaces; BCL2 a row a file with its whole path; BCL1
/// had no stamp: each an older form, reported and rebuilt.
pub(crate) const LEAVES_MAGIC: &[u8; 4] = b"BCL7";

const HEADER: usize = 16;
const ROW: usize = 4 + Stamp::BYTES + 32;

/// The last answer's reads, read in place from the leaf table's bytes.
pub struct Leaves<'f> {
    bytes: &'f [u8],
    rows: u32,
    names: usize,
    ends: usize,
    row_at: usize,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl<'f> Leaves<'f> {
    pub const EMPTY: Leaves<'static> = Leaves { bytes: &[], rows: 0, names: 0, ends: 0, row_at: 0 };

    /// The table in `bytes`, the pointer file's (seal.rs): none when there are none, empty with
    /// an [`Older`] when they are another form's.
    pub fn of(bytes: &'f [u8]) -> Result<(Leaves<'f>, Option<Older>), CliError> {
        if bytes.is_empty() {
            return Ok((Leaves::EMPTY, None));
        }
        if let Err(older) = home::current(LEAVES, bytes, LEAVES_MAGIC) {
            return Ok((Leaves::EMPTY, Some(older)));
        }
        Leaves::parse(bytes).map(|leaves| (leaves, None))
    }

    /// Bytes of this form, checked once: every count, end, kind, flag and order (rule 74).
    fn parse(bytes: &'f [u8]) -> Result<Leaves<'f>, CliError> {
        let refuse = |at: usize, why| CliError::Leaves(WireError { what: "leaf table", at: u32::try_from(at).unwrap_or(u32::MAX), why });
        if bytes.len() < HEADER {
            return Err(refuse(bytes.len(), WireWhy::Short));
        }
        let (rows, name_bytes) = (word(bytes, 4), word(bytes, 8) as usize);
        let names = HEADER;
        let ends = names + name_bytes;
        let row_at = ends + 4 * rows as usize;
        let end = row_at + ROW * rows as usize;
        match bytes.len().cmp(&end) {
            core::cmp::Ordering::Less => return Err(refuse(bytes.len(), WireWhy::Short)),
            core::cmp::Ordering::Greater => return Err(refuse(end, WireWhy::Trailing)),
            core::cmp::Ordering::Equal => {}
        }
        let leaves = Leaves { bytes, rows, names, ends, row_at };
        let mut start = 0;
        for k in 0..rows as usize {
            let end = word(bytes, ends + 4 * k) as usize;
            if end < start || end > name_bytes {
                return Err(refuse(ends + 4 * k, WireWhy::Count(end as u32)));
            }
            if core::str::from_utf8(&bytes[names + start..names + end]).is_err() {
                return Err(refuse(names + start, WireWhy::Utf8));
            }
            start = end;
            let at = row_at + ROW * k;
            if bytes[at] > LeafKind::ScriptGit as u8 {
                return Err(refuse(at, WireWhy::Tag(bytes[at])));
            }
            if bytes[at + 1] & !FLAGS != 0 {
                return Err(refuse(at + 1, WireWhy::Tag(bytes[at + 1])));
            }
            if k > 0 && (bytes[at - ROW], leaves.path_bytes(k - 1)) >= (bytes[at], leaves.path_bytes(k)) {
                return Err(refuse(at, WireWhy::Unsorted));
            }
        }
        Ok(leaves)
    }

    pub fn len(&self) -> usize {
        self.rows as usize
    }

    fn path_bytes(&self, k: usize) -> &'f [u8] {
        let start = match k {
            0 => 0,
            k => word(self.bytes, self.ends + 4 * (k - 1)) as usize,
        };
        &self.bytes[self.names + start..self.names + word(self.bytes, self.ends + 4 * k) as usize]
    }

    /// Row `k`'s path: checked UTF-8 by [`Leaves::parse`].
    pub fn path(&self, k: usize) -> &'f str {
        core::str::from_utf8(self.path_bytes(k)).expect("checked when the table was parsed")
    }

    /// Row `k`, its `path` the row's own index.
    pub fn row(&self, k: usize) -> Leaf {
        let row = &self.bytes[self.row_at + ROW * k..][..ROW];
        let kind = match row[0] {
            0 => LeafKind::Slot,
            1 => LeafKind::Manifest,
            2 => LeafKind::Listing,
            3 => LeafKind::HomeListing,
            4 => LeafKind::MacroFiles,
            5 => LeafKind::MacroFile,
            6 => LeafKind::MacroFolder,
            7 => LeafKind::ScriptFile,
            8 => LeafKind::ScriptListing,
            9 => LeafKind::ScriptStat,
            _ => LeafKind::ScriptGit,
        };
        let stamp = Stamp::get(&row[4..4 + Stamp::BYTES]);
        let mut id = [0; 32];
        id.copy_from_slice(&row[4 + Stamp::BYTES..]);
        Leaf { kind, flags: row[1], path: k as u32, stamp: (stamp.ino != 0).then_some(stamp), id: ContentId(id) }
    }

    /// The table of `rows`: sorted by (kind, path), one row a path of a kind (the last pushed).
    pub fn bytes_in<'g>(rows: &Rows<'_>, frame: &'g Frame) -> Result<Vec<u8, &'g Frame>, CliError> {
        let mut order: Vec<u32, &Frame> = Vec::with_capacity_in(rows.rows.len(), frame);
        order.extend((0..rows.rows.len() as u32).rev());
        // Stable, from the last pushed: the dedup below keeps a path's newest row.
        order.sort_by(|a, b| {
            let (a, b) = (&rows.rows[*a as usize], &rows.rows[*b as usize]);
            (a.kind, rows.path(a)).cmp(&(b.kind, rows.path(b)))
        });
        order.dedup_by(|a, b| {
            let (a, b) = (&rows.rows[*a as usize], &rows.rows[*b as usize]);
            (a.kind, rows.path(a)) == (b.kind, rows.path(b))
        });
        let too_big = |_| CliError::Text { what: "a leaf table past 4 GiB" };
        let name_bytes: usize = order.iter().map(|r| rows.path(&rows.rows[*r as usize]).len()).sum();
        let mut bytes = Vec::with_capacity_in(HEADER + name_bytes + (4 + ROW) * order.len(), frame);
        bytes.extend_from_slice(LEAVES_MAGIC);
        bytes.extend_from_slice(&u32::try_from(order.len()).map_err(too_big)?.to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(name_bytes).map_err(too_big)?.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for r in order.iter() {
            bytes.extend_from_slice(rows.path(&rows.rows[*r as usize]).as_bytes());
        }
        let mut end = 0u32;
        for r in order.iter() {
            end += rows.path(&rows.rows[*r as usize]).len() as u32;
            bytes.extend_from_slice(&end.to_le_bytes());
        }
        for r in order.iter() {
            let leaf = rows.rows[*r as usize];
            bytes.extend_from_slice(&[leaf.kind as u8, leaf.flags, 0, 0]);
            leaf.stamp.unwrap_or(Stamp { len: 0, mtime_ns: 0, ctime_ns: 0, ino: 0, dev: 0 }).put(&mut bytes);
            bytes.extend_from_slice(&leaf.id.0);
        }
        Ok(bytes)
    }
}

/// The slots found at `x/mod.rs` or at both, by their file name's subject: where a slot's file
/// is read and printed from.
pub struct Found<'f> {
    flags: BTreeMap<ContentId, u8, &'f Frame>,
}

impl<'f> Found<'f> {
    pub fn new_in(frame: &'f Frame) -> Found<'f> {
        Found { flags: BTreeMap::new_in(frame) }
    }

    pub fn note(&mut self, slot: &str, flags: u8) {
        if flags & (MODULE | AT_MOD_RS | BOTH) != 0 {
            self.flags.insert(blaze_drive::Name::file(slot.as_bytes()).subject, flags);
        }
    }

    /// The slot's flags: 0 for a file that is no `mod x;` slot (a crate root, a `#[path]` file).
    pub fn flags(&self, slot: &[u8]) -> u8 {
        self.flags.get(&blaze_drive::Name::file(slot).subject).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    //! The sweep on bstd's simulated file system, its stamps from a model of the file times: a
    //! crate whose root has a `mod a;` (`src/a.rs`, `src/a/mod.rs` probed absent), a manifest
    //! probed absent (`m/Cargo.toml`) and a listing (`src`), beside files nothing reads. Each
    //! round edits the read files at their length, writes files nothing reads, or creates a file
    //! at a probed-absent path, as the clock moves under, about or past a tick. Oracle: every
    //! row's id is today's; a round that touched only unread files changes no row and, once the
    //! stamps settled, reads no byte; a file created at a probed path changes exactly its
    //! prober's row. Planted: [`Plant::RacyTrusted`] (a change in the tick of the last sweep
    //! missed) and [`Plant::AbsentForgotten`] (a probe absent last time not looked at again).

    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap as StdMap;
    use std::string::{String, ToString};

    use blaze_sim::{Broke, Config, Tape, check, control};
    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    const TICK: u64 = 1_000;

    /// Files the build reads, and files nothing reads (one inside the listed `src`).
    const READ: [&str; 3] = ["Cargo.toml", "src/main.rs", "src/a.rs"];
    const UNREAD: [&str; 4] = ["notes.txt", ".venv/bin/node", "target/debug/x", "src/notes.md"];
    /// Probed absent: the second half of `mod a;`, and a manifest the layout asked for.
    const PROBED: [&str; 2] = ["src/a/mod.rs", "m/Cargo.toml"];

    struct Model {
        now: Cell<u64>,
        mtimes: RefCell<StdMap<String, u64>>,
    }

    impl Stamps for &Model {
        fn stamp(&self, path: &str, meta: &Meta) -> Option<Stamp> {
            let mtime_ns = match meta.dir {
                // A directory's time is its newest entry's (its own entries' last change).
                true => {
                    let prefix = std::format!("{path}/");
                    self.mtimes.borrow().iter().filter(|(p, _)| p.starts_with(&prefix)).map(|(_, t)| *t).max()?
                }
                false => *self.mtimes.borrow().get(path)?,
            };
            Some(Stamp { len: meta.len, mtime_ns, ctime_ns: mtime_ns, ino: 7, dev: 1 })
        }

        fn swept_at_ns(&self) -> u64 {
            self.now.get()
        }

        fn granularity_ns(&self) -> u64 {
            TICK
        }
    }

    fn harness<E: core::fmt::Display>(what: &str) -> impl Fn(E) -> Broke + '_ {
        move |error| Broke::Harness(format!("{what}: {error}"))
    }

    fn rounds(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let io = Io::simulated(SimWorld::new(tape.next_u64("sim seed"), Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).map_err(harness("a root context"))?;
        let fs = io.need_fs().map_err(harness("the file system"))?;
        let root = fs.open_dir(&cx, &Path::new("data").map_err(harness("a path"))?).map_err(harness("open"))?;
        let model = Model { now: Cell::new(10 * TICK), mtimes: RefCell::new(StdMap::new()) };
        let content: RefCell<StdMap<&str, std::vec::Vec<u8>>> = RefCell::new(StdMap::new());
        let write = |path: &'static str, bytes: &[u8]| -> Result<(), Broke> {
            let at = Path::new(path).map_err(harness("a path"))?;
            root.publish(&cx, &at, Content::bytes(bytes), Durable::No).map_err(harness("a write"))?;
            let now = model.now.get();
            model.mtimes.borrow_mut().insert(path.to_string(), now - now % TICK);
            content.borrow_mut().insert(path, bytes.to_vec());
            Ok(())
        };
        for path in READ.iter().chain(UNREAD.iter()) {
            write(path, &tape.bytes(1..=16, "a file's content"))?;
        }
        // The first answer's reads, as the cli makes them: the layout's, then the walk's opens.
        let frame = cx.frame();
        let mut first = Rows::new_in(frame);
        let mut text = Bytes::new_in(frame, "a manifest");
        read_manifest(&cx, &root, &&model, "Cargo.toml", &mut first, &mut text).map_err(harness("a manifest"))?;
        read_manifest(&cx, &root, &&model, "m/Cargo.toml", &mut first, &mut text).map_err(harness("a manifest"))?;
        read_listing(&cx, &root, &&model, "src", &mut first, frame).map_err(harness("a listing"))?;
        for (slot, module) in [("src/main.rs", false), ("src/a.rs", true)] {
            let probed = probe_slot(&cx, &root, &&model, slot, module, frame).map_err(CliError::from).map_err(harness("an open"))?;
            opened(slot, probed, &mut first);
        }
        let table: RefCell<std::vec::Vec<u8>> = RefCell::new(Leaves::bytes_in(&first, frame).map_err(harness("the table"))?.to_vec());
        // Today's id of a row, from the content model: the oracle.
        let expected = |kind: LeafKind, path: &str| -> (u8, ContentId) {
            let content = content.borrow();
            let id = |p: &str| content.get(p).map(|bytes| ContentId::of(FILE_DOMAIN, bytes));
            match kind {
                LeafKind::Slot if path == "src/a.rs" => match (id("src/a.rs"), id("src/a/mod.rs")) {
                    (Some(a), None) => (0, a),
                    (None, Some(b)) => (AT_MOD_RS, b),
                    (Some(_), Some(_)) => (BOTH, ContentId::of(BOTH_DOMAIN, b"src/a.rs")),
                    (None, None) => (ABSENT, ContentId([0; 32])),
                },
                LeafKind::Slot | LeafKind::Manifest => id(path).map_or((ABSENT, ContentId([0; 32])), |id| (0, id)),
                LeafKind::Listing => {
                    let mut names: std::vec::Vec<(&str, bool)> = std::vec::Vec::new();
                    for p in content.keys().filter_map(|p| p.strip_prefix("src/")) {
                        let (first, is_dir) = p.split_once('/').map_or((p, false), |(f, _)| (f, true));
                        let wanted = if is_dir { first == "bin" } else { first == "lib.rs" || first == "main.rs" };
                        if wanted && names.last().map(|l| l.0) != Some(first) {
                            names.push((first, is_dir));
                        }
                    }
                    listing_id(Some(names.into_iter()))
                }
                // No home is given to this sweep: a home row is absent.
                LeafKind::HomeListing => (ABSENT, ContentId([0; 32])),
                // No member proc-macro crate in this sweep's workspace: no such row is made.
                LeafKind::MacroFiles | LeafKind::MacroFile | LeafKind::MacroFolder => (ABSENT, ContentId([0; 32])),
                // No build script in this sweep's workspace: no such row is made.
                LeafKind::ScriptFile | LeafKind::ScriptListing | LeafKind::ScriptStat | LeafKind::ScriptGit => (ABSENT, ContentId([0; 32])),
            }
        };
        let sweep = |round: &str| -> Result<(std::vec::Vec<(LeafKind, String)>, u64), Broke> {
            let frame = cx.frame();
            let kept = table.borrow().clone();
            let (last, _) = Leaves::of(&kept).map_err(harness("the leaf table"))?;
            let reads = || io.disk().map(|disk| disk.read).ok_or(Broke::Harness("the world is not simulated".to_string()));
            let before = reads()?;
            let followed = Follow { stamps: &model, plant }.follow(&cx, &root, None, &last, frame).map_err(harness("the sweep"))?;
            let read = reads()? - before;
            let mut changed = std::vec::Vec::new();
            for (i, leaf) in followed.now.rows.iter().enumerate() {
                let path = followed.now.path(leaf);
                let want = expected(leaf.kind, path);
                if leaf.state().1 != want.1 || leaf.flags & (AT_MOD_RS | ABSENT | BOTH) != want.0 {
                    return Err(Broke::Oracle(format!("{round}: {path} swept with a stale digest (flags {:x}, want {:x})", leaf.flags, want.0)));
                }
                if followed.changed(i) {
                    changed.push((leaf.kind, path.to_string()));
                }
            }
            let moved = changed.iter().any(|(kind, _)| *kind != LeafKind::Slot);
            if moved != followed.layout_moved {
                return Err(Broke::Oracle(format!("{round}: layout moved {} with {changed:?} changed", followed.layout_moved)));
            }
            *table.borrow_mut() = Leaves::bytes_in(&followed.now, frame).map_err(harness("the table's write"))?.to_vec();
            Ok((changed, read))
        };
        sweep("the first repeat")?;
        for _ in 0..1 + tape.index(3, "rounds") {
            let step = [0, TICK / 2, 3 * TICK][tape.index(3, "the clock's step")];
            model.now.set(model.now.get() + step);
            match tape.index(3, "what the round touches") {
                // Files nothing reads: no row moves.
                0 => {
                    for path in UNREAD {
                        if tape.bool("write this unread file") {
                            write(path, &tape.bytes(1..=16, "its content"))?;
                        }
                    }
                    let settle = 3 * TICK;
                    model.now.set(model.now.get() + settle);
                    let (changed, _) = sweep("a round of unread files")?;
                    if !changed.is_empty() {
                        return Err(Broke::Oracle(format!("unread files changed {changed:?}")));
                    }
                    model.now.set(model.now.get() + settle);
                    let (_, read) = sweep("the settled repeat")?;
                    if read != 0 {
                        return Err(Broke::Oracle(format!("a settled repeat after unread files read {read} bytes")));
                    }
                }
                // A file at a probed-absent path: exactly its prober's row moves.
                1 => {
                    let k = tape.index(PROBED.len(), "which probed path");
                    let path = PROBED[k];
                    if content.borrow().contains_key(path) {
                        continue;
                    }
                    write(path, &tape.bytes(1..=16, "its content"))?;
                    let (changed, _) = sweep("a probed path created")?;
                    let prober = [(LeafKind::Slot, "src/a.rs"), (LeafKind::Manifest, "m/Cargo.toml")][k];
                    if changed != [(prober.0, prober.1.to_string())] {
                        return Err(Broke::Oracle(format!("{path} created, and {changed:?} changed, not its prober {prober:?}")));
                    }
                }
                // The read files edited at their length.
                _ => {
                    for path in READ {
                        if tape.bool("edit this file") {
                            let len = content.borrow()[path].len();
                            let mut bytes = tape.bytes(len..=len, "its new content");
                            bytes.resize(len, 0);
                            write(path, &bytes)?;
                        }
                    }
                    sweep("an edit round")?;
                }
            }
        }
        Ok(())
    }

    /// The invariant: the sweep checks every file, manifest, absent probe, and directory listing recorded in the leaf table without trusting racy timestamps.
    #[test]
    fn the_sweep_follows_what_was_read_and_nothing_else() {
        check(Config::cases(300), |tape| rounds(tape, Plant::None));
    }

    #[test]
    fn the_oracle_catches_a_racy_stamp_trusted() {
        control(Config::cases(2000), Plant::None, Plant::RacyTrusted, "stale digest", rounds);
    }

    #[test]
    fn the_oracle_catches_a_forgotten_absent_probe() {
        control(Config::cases(2000), Plant::None, Plant::AbsentForgotten, "stale digest", rounds);
    }

    /// An old home's leaf table (form BCL3) is reported and read as empty; a torn table of this
    /// form is refused by name; a table round-trips its rows.
    #[test]
    fn an_older_table_is_rebuilt_a_torn_one_refused_and_a_table_round_trips() {
        let io = Io::simulated(SimWorld::new(5, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let mut old = b"BCL3".to_vec();
        old.extend_from_slice(&3u32.to_le_bytes());
        let (leaves, older) = Leaves::of(&old).expect("an older form is not an error");
        assert_eq!((older.map(|it| it.found), leaves.len()), (Some(*b"BCL3"), 0));
        let mut rows = Rows::new_in(frame);
        let stamp = Stamp { len: 1, mtime_ns: 2, ctime_ns: 3, ino: 4, dev: 5 };
        rows.push(LeafKind::Listing, 0, "src", None, ContentId([3; 32]));
        rows.push(LeafKind::Slot, MODULE | AT_MOD_RS, "src/a.rs", Some(stamp), ContentId([1; 32]));
        rows.push(LeafKind::Manifest, ABSENT, "m/Cargo.toml", None, ContentId([0; 32]));
        rows.push(LeafKind::Slot, MODULE, "src/a.rs", Some(stamp), ContentId([2; 32]));
        let bytes = Leaves::bytes_in(&rows, frame).expect("written");
        let (leaves, older) = Leaves::of(&bytes).expect("read back");
        assert!(older.is_none());
        let read: std::vec::Vec<(LeafKind, &str, u8, ContentId)> =
            (0..leaves.len()).map(|k| (leaves.row(k).kind, leaves.path(k), leaves.row(k).flags, leaves.row(k).id)).collect();
        assert_eq!(read, [
            (LeafKind::Slot, "src/a.rs", MODULE, ContentId([2; 32])),
            (LeafKind::Manifest, "m/Cargo.toml", ABSENT, ContentId([0; 32])),
            (LeafKind::Listing, "src", 0, ContentId([3; 32])),
        ], "sorted by kind and path, a path's newest row kept");
        assert_eq!(leaves.row(0).stamp, Some(stamp));
        let mut torn = LEAVES_MAGIC.to_vec();
        torn.extend_from_slice(&9u32.to_le_bytes());
        torn.extend_from_slice(b"a.r");
        let refused = Leaves::of(&torn).err().expect("a malformed table of this form is refused");
        assert!(std::format!("{refused}").contains("leaf table"), "{refused}");
    }

    /// A stamp with no inode is none: such a file system's stamps prove nothing.
    #[test]
    fn a_stamp_with_no_inode_is_none() {
        let stamps = FromMeta { swept_at_ns: 0 };
        let stamp = bstd::fs::Stamp { len: 3, modified_ns: 1, changed_ns: 1, ino: 9, dev: 1 };
        let meta = Meta { len: 3, dir: false, stamp };
        assert!(stamps.stamp("src/main.rs", &meta).is_some());
        let none = Meta { stamp: bstd::fs::Stamp { ino: 0, ..stamp }, ..meta };
        assert_eq!(stamps.stamp("src/main.rs", &none), None);
    }

    /// The home a first command writes is not in the root's listing: the listing a cold run read
    /// is the next run's, and the layout stands (the certificate missed after each cold run).
    /// Its control: another directory made at the root moves the listing.
    #[test]
    fn the_home_a_command_writes_does_not_move_the_root_listing() {
        let io = bstd::io::Io::simulated(bstd::io::SimWorld::new(1, bstd::io::Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        let write = |path: &str| {
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(b"x"), Durable::No).expect("write");
        };
        let id = || {
            let listing = list_dir(&cx, &root, "", frame).expect("listed");
            listing_id(listing.as_ref().map(|listing| listing.entries())).1
        };
        write("Cargo.toml");
        write("crates/a/Cargo.toml");
        let before = id();
        write(".blaze/v2/last");
        assert_eq!(id(), before, "the home moved the root's listing");
        write("crates2/b/Cargo.toml");
        assert_ne!(id(), before, "control: a new directory at the root moves its listing");
    }

    /// Two checkouts of one tree, a clone (`.git` a directory) where cargo has run (`target/`)
    /// and a worktree (`.git` a file), give the layout the same root listing, so the same units'
    /// source: the worktree's first check lays out nothing. Its controls: a `src/` or a member
    /// folder made at the root, and a manifest read, move the source. Without the filter the
    /// first assertion fails: the clone's listing holds `.git` and `target` as directories.
    #[test]
    fn git_and_cargo_entries_leave_the_units_source_equal() {
        let io = Io::simulated(SimWorld::new(4, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let fs = io.need_fs().expect("files");
        let stamps = FromMeta { swept_at_ns: 0 };
        let open = |at: &str| fs.open_dir(&cx, &Path::new(at).expect("a path")).expect("open");
        let put = |dir: &Dir<ReadWrite>, path: &str| {
            dir.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(b"x"), Durable::No).expect("write");
        };
        let source = |dir: &Dir<ReadWrite>, manifests: &[&str]| {
            let mut rows = Rows::new_in(frame);
            let mut text = Bytes::new_in(frame, "a manifest");
            for manifest in manifests {
                read_manifest(&cx, dir, &stamps, manifest, &mut rows, &mut text).expect("the manifest");
            }
            read_listing(&cx, dir, &stamps, "", &mut rows, frame).expect("the root's listing");
            units_source(rows.rows.iter().map(|leaf| (rows.path(leaf), leaf)), frame)
        };
        let (clone, worktree) = (open("clone"), open("worktree"));
        for dir in [&clone, &worktree] {
            put(dir, "Cargo.toml");
            put(dir, "crates/a/Cargo.toml");
        }
        put(&clone, ".git/HEAD");
        put(&clone, "target/debug/.cargo-lock");
        put(&worktree, ".git");
        let base = source(&worktree, &["Cargo.toml"]);
        // cargo's `target/` is the one holding its tag; without it, it is a package's folder.
        assert_ne!(source(&clone, &["Cargo.toml"]), base, "control: an untagged `target/` is listed");
        clone.publish(&cx, &Path::new(TARGET_TAG).expect("a path"), Content::bytes(b"Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n"), Durable::No).expect("the tag");
        assert_eq!(source(&clone, &["Cargo.toml"]), base, "a clone's `.git` folder and cargo's `target/` moved the units' source");
        clone.publish(&cx, &Path::new(TARGET_TAG).expect("a path"), Content::bytes(b"Signature: not cargo's\n"), Durable::No).expect("the tag");
        assert_ne!(source(&clone, &["Cargo.toml"]), base, "control: a tag without the signature lists `target/` again");
        clone.publish(&cx, &Path::new(TARGET_TAG).expect("a path"), Content::bytes(b"Signature: 8a477f597d28d172789f06886806bc55\n"), Durable::No).expect("the tag");
        assert_ne!(source(&worktree, &["Cargo.toml", "crates/b/Cargo.toml"]), base, "control: a manifest probed absent is in the source");
        put(&worktree, "src/main.rs");
        let with_src = source(&worktree, &["Cargo.toml"]);
        assert_ne!(with_src, base, "control: a `src/` made at the root moves the source");
        put(&worktree, "crates2/b/Cargo.toml");
        assert_ne!(source(&worktree, &["Cargo.toml"]), with_src, "control: a member folder made at the root moves the source");
    }

    /// The proc-macro module store's listing under the home is a fact the answer reads: the
    /// sweep follows its `HomeListing` row, so a module renamed into the store moves the layout
    /// and the units' source (the certificate, the graph's source, each `MacroModule` part's
    /// key), and a sweep with the store unchanged moves neither: a lookup that found no module
    /// is then a filed failure, never asked again before the store moves (macros.rs). Expected:
    /// passes. Without the row (`read_home_listing` not called, the sweep blind to the home),
    /// the second assertion fails: "a module put in the store moved nothing".
    #[test]
    fn a_module_put_in_the_home_store_moves_the_units_source_and_nothing_else_does() {
        let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let fs = io.need_fs().expect("files");
        let root = fs.open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        let home = fs.open_dir(&cx, &Path::new("home/.blaze").expect("a path")).expect("open");
        let put = |dir: &Dir<ReadWrite>, path: &str| {
            dir.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(b"x"), Durable::No).expect("write");
        };
        put(&root, "Cargo.toml");
        put(&home, "v2/proc-macro/modules/aaaa/wasm-sha256");
        let model = Model { now: Cell::new(10 * TICK), mtimes: RefCell::new(StdMap::new()) };
        let mut rows = Rows::new_in(frame);
        read_home_listing(&cx, Some(&home), &&model, crate::macros::MODULE_LISTING, &mut rows, frame).expect("the home's listing");
        let source = |rows: &Rows<'_>| units_source(rows.rows.iter().map(|leaf| (rows.path(leaf), leaf)), frame);
        let table = Leaves::bytes_in(&rows, frame).expect("a table");
        let (last, _) = Leaves::of(&table).expect("the table reads");
        let follow = || Follow { stamps: &model, plant: Plant::None }.follow(&cx, &root, Some(&home), &last, frame).expect("the sweep");
        let same = follow();
        assert!(!same.layout_moved && source(&same.now) == source(&rows), "an unchanged store moved the layout");
        put(&home, "v2/proc-macro/modules/bbbb/wasm-sha256");
        let moved = follow();
        assert!(moved.layout_moved && source(&moved.now) != source(&rows), "a module put in the store moved nothing");
    }

    /// A member's `examples/mm.rs` (and `tests/`, `benches/` files) reaches the layout: without
    /// it a written `[[example]] name = "mm"` with no `path` is refused "no file
    /// `examples/mm.rs`" though the file is there (DRIVER runner 11's repro, 2026-11-16).
    #[test]
    fn target_folders_list_their_rust_files() {
        for folder in ["tests", "examples", "benches"] {
            assert!(super::wanted(folder, "mm.rs", false, false), "{folder}/mm.rs is not listed");
            assert!(!super::wanted(folder, "notes.txt", false, false), "control: {folder}/notes.txt is listed");
        }
        assert!(!super::wanted("m", "mm.rs", false, false), "control: a package folder lists only lib.rs, main.rs and build.rs");
        // A member's build script is found by its file (layout `RawBuild::Auto`); the root
        // package's folder too.
        assert!(super::wanted("app", "build.rs", false, false), "a member's build.rs is not listed");
        assert!(super::wanted("", "build.rs", false, false), "the root package's build.rs is not listed");
        // The root's `target`: cargo's only with its tag; `.blaze` and `.git` never listed.
        assert!(super::wanted("", "target", true, false) && !super::wanted("", "target", true, true), "a `target` is listed exactly when untagged");
        assert!(!super::wanted("", ".blaze", true, false) && !super::wanted("", ".git", true, false));
        assert!(super::wanted("crates", "target", true, true), "control: only the root's `target` can be cargo's");
    }
}

#[cfg(test)]
#[path = "tests/sweep_looks_tests.rs"]
mod sweep_looks_tests;
