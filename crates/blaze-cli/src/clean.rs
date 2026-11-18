//! `blaze clean [dir] [--store]` (engine.md E12: a user is never stuck on the store).
//!
//! With no flag it removes the checkout's own state, `<dir>/.blaze/v2`; `--store` also removes
//! the home's store, `<HOME>/.blaze/v2/store`. Everything it removes is derived: the next command
//! makes it again. It removes only what it can show is blaze's, and refuses by name otherwise
//! (rule 3, rule 19):
//!
//! - the checkout's `.blaze` and `.blaze/v2` must be folders, not links, and `.blaze/v2` must hold
//!   only names blaze writes there ([`CHECKOUT_ENTRIES`]): a folder that holds anything else (a
//!   checkout that is the home, whose `.blaze/v2` holds the store and the toolchain shelf) is
//!   refused naming the entry, and nothing is removed;
//! - the store must be a folder, not a link, holding blaze's cache directory tag
//!   (blaze-cas `CACHEDIR_TAG`, its first two lines equal to `CACHEDIR_SIGNATURE`'s). Its
//!   siblings under the home's `.blaze/v2` (the toolchain shelf, the sources, the old `own` and
//!   `shared`) are never looked at.
//!
//! Bounded (rule 18): a tree deeper than [`DEPTH_MOST`] is refused naming the folder, a listing
//! is blaze-core's (capped), the tag is read only when it is at most [`TAG_MOST`] bytes. No link
//! is followed: a link inside a removed folder is unlinked itself, never what it points at; a
//! FIFO, socket or device is refused by name.
//!
//! The store has no lock that says another command has it open: the plain store takes its `flip`
//! lock for the tens of microseconds of a commit only (blaze-cas plain.rs, "A commit, in
//! order"), and a reader maps segments without any lock. A command running while `--store`
//! removes keeps reading what it mapped; a commit of it that lands mid-removal leaves a file
//! the removal did not list, and the folder's removal then fails naming it.

use core::fmt;

use bstd::fs::{Dir, EntryKind, FsError, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::{Path, PathError};
use bstd::Cx;

/// The names blaze writes under a checkout's `.blaze/v2`: `profile.txt` (print.rs `PROFILE`),
/// and the forms before the one store (`last`, `leaves`, `packs`, `records`, and `at`: store.rs
/// `OLD_CHECKOUT`). Any other name there is not blaze's, and the clean is refused.
pub const CHECKOUT_ENTRIES: [&str; 6] = ["at", "last", "leaves", "packs", "profile.txt", "records"];

/// The checkout's state, beneath the checkout.
pub const CHECKOUT_STATE: &str = ".blaze/v2";

/// The store, beneath the home: `.blaze/` and store.rs `STORE_DIR` (a test holds them equal).
pub const HOME_STORE: &str = ".blaze/v2/store";

/// How deep a removed tree may go below the folder removed.
pub const DEPTH_MOST: u32 = 8;

/// The most bytes a cache directory tag is read at: blaze's is under 200.
pub const TAG_MOST: u64 = 4096;

/// Why `blaze clean` refused, naming the path (rule 19). Paths are the checkout's (beneath
/// it) or the home's (beneath its `.blaze`), as [`Place`] says.
#[derive(Debug)]
pub enum CleanError {
    /// `--store` with no `HOME`: the store is beneath it.
    NoHome,
    /// The home could not be opened.
    Home(bstd::fs::HomeError),
    /// A folder the clean would enter or remove is a link: never followed.
    Link { place: Place, path: Path },
    /// Something other than a folder where blaze keeps one.
    NotAFolder { place: Place, path: Path },
    /// A name under the checkout's `.blaze/v2` that blaze does not write there.
    Unknown { path: Path },
    /// The store's folder does not hold blaze's cache directory tag (`why`).
    NotTheStore { path: Path, why: &'static str },
    /// A FIFO, socket, device or an entry the system says nothing of, inside a removed folder.
    Special { place: Place, path: Path },
    /// A folder past [`DEPTH_MOST`] below the one removed.
    TooDeep { place: Place, path: Path },
    /// A path could not be made (a name with a `\`).
    Path(PathError),
    /// The file system refused, naming the path.
    Fs(FsError),
}

/// Which tree a path is beneath.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// The checkout (the folder `blaze clean` was given, or the one it started in).
    Checkout,
    /// blaze's folder in the home, `<HOME>/.blaze`.
    Home,
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Place::Checkout => f.write_str("the checkout's "),
            Place::Home => f.write_str("the home's "),
        }
    }
}

impl fmt::Display for CleanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CleanError::NoHome => f.write_str("blaze clean --store: HOME is not set, and the store is under the home's .blaze/v2/store"),
            CleanError::Home(error) => write!(f, "blaze clean --store: the home: {error}"),
            CleanError::Link { place, path } => write!(f, "blaze clean: {place}{path} is a link; blaze never follows one out of its folder, so nothing was removed"),
            CleanError::NotAFolder { place, path } => write!(f, "blaze clean: {place}{path} is not a folder, so it is not blaze's; nothing was removed"),
            CleanError::Unknown { path } => {
                write!(f, "blaze clean: the checkout's {path} is not a name blaze writes under {CHECKOUT_STATE} (it writes")?;
                for (i, name) in CHECKOUT_ENTRIES.iter().enumerate() {
                    f.write_str(if i == 0 { " " } else { ", " })?;
                    f.write_str(name)?;
                }
                f.write_str("), so the folder is not blaze's alone; nothing was removed")
            }
            CleanError::NotTheStore { path, why } => write!(f, "blaze clean --store: the home's {path} {why}, so it is not blaze's store; nothing was removed"),
            CleanError::Special { place, path } => write!(f, "blaze clean: {place}{path} is a FIFO, socket, device or unknown entry, never one of blaze's files; removal stopped there"),
            CleanError::TooDeep { place, path } => write!(f, "blaze clean: {place}{path} is more than {DEPTH_MOST} folders deep, deeper than blaze writes; removal stopped there"),
            CleanError::Path(error) => write!(f, "blaze clean: {error}"),
            CleanError::Fs(error) => write!(f, "blaze clean: {error}"),
        }
    }
}

impl From<FsError> for CleanError {
    fn from(error: FsError) -> CleanError {
        CleanError::Fs(error)
    }
}

impl From<PathError> for CleanError {
    fn from(error: PathError) -> CleanError {
        CleanError::Path(error)
    }
}

/// A folder shown to be blaze's ([`checkout_state`], [`store`]): the one value [`remove`]
/// takes (rule 76), so nothing is removed that was not checked first.
#[derive(Debug)]
pub struct Found {
    pub place: Place,
    pub path: Path,
}

/// One folder removed: its path beneath its [`Place`], and what it held.
#[derive(Debug)]
pub struct Removed {
    pub place: Place,
    pub path: Path,
    /// Files and links unlinked.
    pub files: u64,
    /// Their lengths together.
    pub bytes: u64,
}

/// The line `blaze clean` prints for a folder removed: `lead` is what comes before the path
/// (the checkout as given and a `/`, or the home and `/.blaze/`).
pub struct Said<'a> {
    pub lead: &'a str,
    pub removed: &'a Removed,
}

impl fmt::Display for Said<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Removed { path, files, bytes, .. } = self.removed;
        let noun = if *files == 1 { "file" } else { "files" };
        write!(f, "removed {}{path}: {files} {noun}, {bytes} bytes freed", self.lead)
    }
}

/// What `blaze clean` prints when no folder was there to remove.
pub const NOTHING: &str = "nothing to remove";

/// `blaze clean [dir] [--store]`: each folder removed printed as it goes ("removed <path>: <n>
/// files, <bytes> bytes freed"), or "nothing to remove"; exit status 0. A refusal stops it
/// before anything more is removed.
pub fn run(cx: &Cx, io: &bstd::Io, args: &bstd::args::Args, out: &bstd::out::Out) -> Result<bstd::entry::Status, crate::CliError> {
    // The home, read here at the edge (rule 75).
    let env = io.env();
    let home = env.as_ref().and_then(|env| env.get(&bstd::env::VarName::new("HOME").ok()?).ok().flatten());
    run_with(cx, io, args.each().skip(2), home, out)
}

/// [`run`] on the words after `clean` and the home `HOME` names.
pub fn run_with<'w>(cx: &Cx, io: &bstd::Io, words: impl Iterator<Item = &'w str>, home: Option<&str>, out: &bstd::out::Out) -> Result<bstd::entry::Status, crate::CliError> {
    let mut dir = None;
    let mut with_store = false;
    for arg in words {
        match arg {
            "--store" => with_store = true,
            flag if flag.starts_with('-') => return Err(crate::CliError::Usage("`blaze clean` takes a folder and `--store`")),
            _ if dir.is_some() => return Err(crate::CliError::Usage("`blaze clean` takes one folder")),
            folder => dir = Some(folder),
        }
    }
    let frame = cx.frame();
    let fs = io.need_fs()?;
    let checkout = crate::checkout::checkout(dir, frame)?;
    let (root, lead) = match &checkout {
        crate::checkout::Checkout::Under(under) => (fs.open_dir(cx, under)?, bstd::mem::format_in!(frame, "{under}/")),
        crate::checkout::Checkout::Here => (fs.root(), bstd::mem::format_in!(frame, "")),
    };
    // Both folders are checked before either is removed: a refusal removes nothing.
    let state = checkout_state(cx, &root).map_err(crate::CliError::Clean)?;
    let held = match with_store {
        false => None,
        true => {
            let Some(home) = home else { return Err(crate::CliError::Clean(CleanError::NoHome)) };
            // The home as a folder of the filesystem's root as a second tree (as roster.rs
            // opens a binary's folder), so its `.blaze` is listed before it is entered.
            let below = home.trim_matches('/');
            let dir = fs.home(cx, "/", &Path::new(below)?).map_err(|error| crate::CliError::Clean(CleanError::Home(error)))?;
            let found = store(cx, &dir, frame).map_err(crate::CliError::Clean)?;
            found.map(|found| (dir, bstd::mem::format_in!(frame, "/{below}/"), found))
        }
    };
    let mut any = false;
    if let Some(found) = state {
        let removed = remove(cx, &root, found).map_err(crate::CliError::Clean)?;
        out.line(cx, format_args!("{}", Said { lead: lead.as_str(), removed: &removed }))?;
        any = true;
    }
    if let Some((blaze, lead, found)) = held {
        let removed = remove(cx, &blaze, found).map_err(crate::CliError::Clean)?;
        out.line(cx, format_args!("{}", Said { lead: lead.as_str(), removed: &removed }))?;
        any = true;
    }
    if !any {
        out.line(cx, format_args!("{NOTHING}"))?;
    }
    Ok(bstd::entry::Status::OK)
}

/// The checkout's `.blaze/v2`, when it is there and blaze's: `root` is the checkout. `None`
/// when there is nothing to remove.
pub fn checkout_state(cx: &Cx, root: &Dir<ReadWrite>) -> Result<Option<Found>, CleanError> {
    let place = Place::Checkout;
    // Each step looks at its folder in its parent's listing, which never follows a link.
    let top = root.own_entries(cx)?;
    let Some(at) = top.find(".blaze") else { return Ok(None) };
    folder(place, top.kind(at), ".blaze")?;
    let Some(blaze) = root.entries(cx, &Path::new(".blaze")?)? else { return Ok(None) };
    let Some(at) = blaze.find("v2") else { return Ok(None) };
    folder(place, blaze.kind(at), CHECKOUT_STATE)?;
    let state = Path::new(CHECKOUT_STATE)?;
    let Some(entries) = root.entries(cx, &state)? else { return Ok(None) };
    for (i, (name, _)) in entries.iter().enumerate() {
        if !CHECKOUT_ENTRIES.contains(&name) {
            return Err(CleanError::Unknown { path: entries.path(i)? });
        }
    }
    Ok(Some(Found { place, path: state }))
}

/// The home's store, when it is there and blaze's: `home` is the home folder. `None` when
/// there is nothing to remove.
pub fn store(cx: &Cx, home: &Dir<ReadWrite>, frame: &Frame) -> Result<Option<Found>, CleanError> {
    let place = Place::Home;
    // Each step looks at its folder in its parent's listing, which never follows a link.
    let own = home.own_entries(cx)?;
    let Some(at) = own.find(".blaze") else { return Ok(None) };
    folder(place, own.kind(at), ".blaze")?;
    let Some(blaze) = home.entries(cx, &Path::new(".blaze")?)? else { return Ok(None) };
    let Some(at) = blaze.find("v2") else { return Ok(None) };
    folder(place, blaze.kind(at), ".blaze/v2")?;
    let Some(v2) = home.entries(cx, &Path::new(".blaze/v2")?)? else { return Ok(None) };
    let Some(at) = v2.find("store") else { return Ok(None) };
    folder(place, v2.kind(at), HOME_STORE)?;
    let path = Path::new(HOME_STORE)?;
    let Some(entries) = home.entries(cx, &path)? else { return Ok(None) };
    let refused = |why| Err(CleanError::NotTheStore { path: path.clone(), why });
    let Some(tag) = entries.find(blaze_cas::plain::CACHEDIR_TAG) else { return refused("holds no CACHEDIR.TAG") };
    if entries.kind(tag) != EntryKind::File {
        return refused("holds a CACHEDIR.TAG that is not a file");
    }
    if entries.len_of(tag) > TAG_MOST {
        return refused("holds a CACHEDIR.TAG longer than blaze's");
    }
    let mut bytes = Bytes::new_in(frame, "the store's cache directory tag");
    home.read_all(cx, &entries.path(tag)?, &mut bytes)?;
    if !blazes_tag(bytes.as_slice()) {
        return refused("holds a CACHEDIR.TAG that blaze did not write");
    }
    Ok(Some(Found { place, path }))
}

/// The folder `found` names (beneath `dir`, the directory it was found in) removed with
/// everything beneath it.
pub fn remove(cx: &Cx, dir: &Dir<ReadWrite>, found: Found) -> Result<Removed, CleanError> {
    let mut removed = Removed { place: found.place, path: found.path, files: 0, bytes: 0 };
    // clone: one path, so the walk can count into `removed` while it reads the path.
    let path = removed.path.clone();
    tree(cx, dir, &path, 0, &mut removed)?;
    Ok(removed)
}

/// Whether `tag` begins with the first two lines of blaze's cache directory tag: the standard
/// signature, and the line saying blaze made it.
pub fn blazes_tag(tag: &[u8]) -> bool {
    let theirs = blaze_cas::plain::CACHEDIR_SIGNATURE;
    let two_lines = |bytes: &[u8]| bytes.iter().enumerate().filter(|(_, b)| **b == b'\n').nth(1).map(|(at, _)| at + 1);
    match two_lines(theirs) {
        Some(end) => tag.get(..end) == Some(&theirs[..end]),
        None => false,
    }
}

/// That the entry of `kind` at `path` is a folder: a link is refused by name, never followed.
fn folder(place: Place, kind: EntryKind, path: &str) -> Result<(), CleanError> {
    match kind {
        EntryKind::Dir => Ok(()),
        EntryKind::Link => Err(CleanError::Link { place, path: Path::new(path)? }),
        _ => Err(CleanError::NotAFolder { place, path: Path::new(path)? }),
    }
}

/// Everything beneath `path` (a folder beneath `dir`), then `path` itself, removed, counted in
/// `removed`. Derived data: nothing is synced (a crash may bring a file back, and the next
/// command reads it as any other or makes it again).
fn tree(cx: &Cx, dir: &Dir<ReadWrite>, path: &Path, depth: u32, removed: &mut Removed) -> Result<(), CleanError> {
    if depth > DEPTH_MOST {
        return Err(CleanError::TooDeep { place: removed.place, path: path.clone() });
    }
    let Some(entries) = dir.entries(cx, path)? else { return Ok(()) };
    for i in 0..entries.len() {
        let child = entries.path(i)?;
        match entries.kind(i) {
            // A link is unlinked itself: what it points at is never reached.
            EntryKind::File | EntryKind::Link => {
                dir.remove(cx, &child, bstd::fs::Durable::No)?;
                removed.files += 1;
                removed.bytes += entries.len_of(i);
            }
            EntryKind::Dir => tree(cx, dir, &child, depth + 1, removed)?,
            // Gone between the listing and its stamp: nothing to remove.
            EntryKind::Gone => {}
            EntryKind::Other | EntryKind::Unknown => return Err(CleanError::Special { place: removed.place, path: child }),
        }
    }
    dir.remove_dir(cx, path, bstd::fs::Durable::No)?;
    Ok(())
}

#[cfg(test)]
#[path = "clean_tests.rs"]
mod tests;
