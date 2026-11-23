//! `blaze clean [dir] [--store]` (engine.md E12: a user is never stuck on the store).
//!
//! With no flag it removes the checkout's own state, `<dir>/.blaze/v2`, and the home's stores of
//! OTHER FORMS (D160: `<HOME>/.blaze/v2/store/f<N>s<M>` of forms not this blaze's, and the
//! legacy layout's files directly in `v2/store`: store/forms.rs), which this blaze never reads;
//! `--store` removes the home's whole store root, `<HOME>/.blaze/v2/store`, every form in it.
//! `--all` is `--store` and every folder of layout.rs's `Scope::All` (the derived caches), and
//! says each folder of the home it keeps and why. Every folder it removes is a row of the one
//! layout table (layout.rs), found by that row's exact path ([`walk`]), and each is checked
//! before any is removed.
//! The cleanup after a command's answer removes a store of another form once it is unused for
//! forms.rs `EXPIRY_DAYS` ([`sweep_other_forms`]). Everything it removes is derived: the next
//! command makes it again. It removes only what it can show is blaze's, and refuses by name
//! otherwise (rule 3, rule 19):
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

/// THE ONE LIST of what blaze writes under a checkout's `.blaze/v2` ([`CHECKOUT_STATE`]). Each
/// writer names its file from here ([`CheckoutEntry::path`]: print.rs `PROFILE`, engine_start.rs
/// `START_PROFILE`, store.rs `OLD_CHECKOUT`), and `clean` accepts exactly these names
/// ([`CHECKOUT_ENTRIES`]), so a file a writer names is one `clean` knows: a new one is a new
/// variant, which the exhaustive `name` match makes the writer and `clean` agree on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckoutEntry {
    /// `profile.txt`: the check's profile (print.rs `PROFILE`).
    Profile,
    /// `start.txt`: the start's profile, written after every `run` and `test` (engine_start.rs
    /// `START_PROFILE`).
    Start,
    /// `at`: the old pointer folder (blaze-drive pointers.rs `FOLDER`; store.rs `OLD_CHECKOUT`).
    At,
    /// `packs`: the checkout store before the one store (store.rs `OLD_CHECKOUT`).
    Packs,
    /// `fsevents`: the file watch's cookies, `<pid>-<n>` (blaze-core fsevents.rs `COOKIE_AREA`,
    /// journal_ask.rs `OWN_TRAFFIC`). Before it was listed, a clean of any checkout a watch had
    /// run in was refused by this name.
    Cookies,
    /// `last`, `leaves`, `records`: older forms a checkout may still hold.
    Last,
    Leaves,
    Records,
}

impl CheckoutEntry {
    pub const ALL: [CheckoutEntry; 8] = [
        CheckoutEntry::At,
        CheckoutEntry::Cookies,
        CheckoutEntry::Last,
        CheckoutEntry::Leaves,
        CheckoutEntry::Packs,
        CheckoutEntry::Profile,
        CheckoutEntry::Records,
        CheckoutEntry::Start,
    ];

    /// Its name in `.blaze/v2`.
    pub const fn name(self) -> &'static str {
        match self {
            CheckoutEntry::Profile => "profile.txt",
            CheckoutEntry::Start => "start.txt",
            CheckoutEntry::At => "at",
            CheckoutEntry::Packs => "packs",
            CheckoutEntry::Cookies => "fsevents",
            CheckoutEntry::Last => "last",
            CheckoutEntry::Leaves => "leaves",
            CheckoutEntry::Records => "records",
        }
    }

    /// Its path beneath the checkout: [`CHECKOUT_STATE`], `/`, [`CheckoutEntry::name`] (a test
    /// holds every one so).
    pub const fn path(self) -> &'static str {
        match self {
            CheckoutEntry::Profile => ".blaze/v2/profile.txt",
            CheckoutEntry::Start => ".blaze/v2/start.txt",
            CheckoutEntry::At => ".blaze/v2/at",
            CheckoutEntry::Packs => ".blaze/v2/packs",
            CheckoutEntry::Cookies => ".blaze/v2/fsevents",
            CheckoutEntry::Last => ".blaze/v2/last",
            CheckoutEntry::Leaves => ".blaze/v2/leaves",
            CheckoutEntry::Records => ".blaze/v2/records",
        }
    }
}

/// The names [`CheckoutEntry::ALL`] gives: any other name under a checkout's `.blaze/v2` is not
/// blaze's, and the clean is refused.
pub const CHECKOUT_ENTRIES: [&str; CheckoutEntry::ALL.len()] = {
    let mut names = [""; CheckoutEntry::ALL.len()];
    let mut i = 0;
    while i < names.len() {
        names[i] = CheckoutEntry::ALL[i].name();
        i += 1;
    }
    names
};

/// The checkout's state, beneath the checkout.
pub const CHECKOUT_STATE: &str = ".blaze/v2";

/// The store root, beneath the home: `.blaze/` and plain_place.rs `STORE_ROOT` (a test holds
/// them equal); this blaze's store is the folder of its forms in it (`STORE_DIR`).
pub const HOME_STORE: &str = ".blaze/v2/store";

// The derived per-user cache folders `--all` removes besides [`HOME_STORE`] are layout.rs's
// `Scope::All` rows (one table, rule 30); `toolchain` and `sources`, fetched upstream, are kept.

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

/// The line `blaze clean` prints for a store of another form removed (D160): a folder's
/// "removed <lead>v2/store/f4s2, a store of another blaze's forms: <n> files, <bytes> bytes
/// freed"; the legacy layout's "removed the format 4 store's files directly in <lead>v2/store:
/// ...".
pub struct SaidOther<'a> {
    pub lead: &'a str,
    pub legacy: bool,
    pub removed: &'a Removed,
}

impl fmt::Display for SaidOther<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Removed { path, files, bytes, .. } = self.removed;
        let noun = if *files == 1 { "file" } else { "files" };
        match self.legacy {
            true => write!(f, "removed the format 4 store's files directly in {}{path}: {files} {noun}, {bytes} bytes freed", self.lead),
            false => write!(f, "removed {}{path}, a store of another blaze's forms: {files} {noun}, {bytes} bytes freed", self.lead),
        }
    }
}

/// The line `blaze clean` (no `--store`) prints when the home's store root is not shown
/// blaze's: the checkout was cleaned, the root was not looked into.
pub struct NotLooked<'a> {
    pub path: &'a Path,
    pub why: &'static str,
}

impl fmt::Display for NotLooked<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the home's {} {}, so it is not shown to be blaze's store: no store in it was looked at or removed", self.path, self.why)
    }
}

/// What `blaze clean` prints when no folder was there to remove.
pub const NOTHING: &str = "nothing to remove";

/// `blaze clean [dir] [--store]`: each folder removed printed as it goes ("removed <path>: <n>
/// files, <bytes> bytes freed"), or "nothing to remove"; exit status 0. A refusal stops it
/// before anything more is removed.
pub fn run(cx: &Cx, fs: &bstd::fs::Fs, env: Option<&bstd::env::Env>, args: &bstd::args::Args, out: &bstd::out::Out) -> Result<bstd::entry::Status, crate::CliError> {
    // The home, read here at the edge (rule 75).
    let home = env.and_then(|env| env.get(&bstd::env::VarName::new("HOME").ok()?).ok().flatten());
    run_with(cx, fs, args.each().skip(2), home, out)
}

/// [`run`] on the words after `clean` and the home `HOME` names.
pub fn run_with<'w>(cx: &Cx, fs: &bstd::fs::Fs, words: impl Iterator<Item = &'w str>, home: Option<&str>, out: &bstd::out::Out) -> Result<bstd::entry::Status, crate::CliError> {
    let mut dir = None;
    let mut with_store = false;
    let mut with_all = false;
    for arg in words {
        match arg {
            "--store" => with_store = true,
            "--all" => {
                with_store = true;
                with_all = true;
            }
            flag if flag.starts_with('-') => return Err(crate::CliError::Usage("`blaze clean` takes a folder and `--store` or `--all`")),
            _ if dir.is_some() => return Err(crate::CliError::Usage("`blaze clean` takes one folder")),
            folder => dir = Some(folder),
        }
    }
    let frame = cx.frame();
    let checkout = crate::checkout::checkout(dir, frame)?;
    // Best-effort non-blocking shutdown of any resident daemon(s) before removing checkout or
    // shared store state (Wave 7, rule 70). `--store` / `--all` deletes the HOME-wide store
    // shared by every checkout, so shut down every daemon under `<HOME>/.blaze/v2/daemon/`.
    #[cfg(unix)]
    if !cx.is_simulated()
        && let Some(home_text) = home
    {
        if with_store {
            let below = home_text.trim_matches('/');
            if let Ok(path) = Path::new(below)
                && let Ok(home_dir) = fs.home(cx, "/", &path)
                && let Ok(daemon_rel) = Path::new(".blaze/v2/daemon")
                && let Ok(Some(listed)) = home_dir.entries(cx, &daemon_rel)
            {
                let base = home_text.trim_end_matches('/');
                for i in 0..listed.len() {
                    let name = listed.name(i);
                    if name.ends_with(".sock") {
                        let full = bstd::mem::format_in!(frame, "{base}/.blaze/v2/daemon/{name}");
                        crate::daemon_client::send_shutdown_socket(full.as_bytes());
                    }
                }
            }
        } else {
            let checkout_rel = match &checkout {
                crate::checkout::Checkout::Under(under) => under.as_str(),
                crate::checkout::Checkout::Here => "",
            };
            if let Ok(served) = crate::engine_script_reads::Served::at_edge(false, checkout_rel, frame)
                && let Some(root_abs) = served.root_text()
            {
                crate::daemon_client::send_shutdown(home_text, root_abs);
            }
        }
    }
    let (root, lead) = match &checkout {
        crate::checkout::Checkout::Under(under) => (fs.open_dir(cx, under)?, bstd::mem::format_in!(frame, "{under}/")),
        crate::checkout::Checkout::Here => (fs.root(), bstd::mem::format_in!(frame, "")),
    };
    // Both folders are checked before either is removed: a refusal removes nothing.
    let state = checkout_state(cx, &root).map_err(crate::CliError::Clean)?;
    let mut not_blazes = None;
    let mut home_root_dir = None;
    // The home's store root, when HOME names one and it is blaze's (its cache directory tag):
    // `--store` removes it whole; `--all` also removes derived caches under `.blaze/v2/`;
    // without either, only stores of other forms go (D160).
    let held = match home {
        None if with_store => return Err(crate::CliError::Clean(CleanError::NoHome)),
        None => None,
        Some(home) if fs.root().stat(cx, &Path::new(home.trim_matches('/'))?)?.is_none() => None,
        Some(home) => {
            let below = home.trim_matches('/');
            let dir = fs.home(cx, "/", &Path::new(below)?).map_err(|error| crate::CliError::Clean(CleanError::Home(error)))?;
            let lead_str = bstd::mem::format_in!(frame, "/{below}/");
            if with_all {
                home_root_dir = Some((dir.clone(), bstd::mem::format_in!(frame, "/{below}/")));
            }
            match store(cx, &dir, frame) {
                Ok(found) => found.map(|found| (dir, lead_str, found)),
                Err(refusal @ CleanError::NotTheStore { .. }) if !with_store => {
                    not_blazes = Some(refusal);
                    None
                }
                Err(refusal) => return Err(crate::CliError::Clean(refusal)),
            }
        }
    };
    // `--all`: each of layout.rs's `All` folders checked (a folder, no link on its way) before
    // anything is removed, so a refusal removes nothing.
    if let Some((home_dir, _)) = &home_root_dir {
        for row in crate::layout::of(crate::layout::Scope::All) {
            walk(cx, home_dir, row).map_err(crate::CliError::Clean)?;
        }
    }
    let mut any = false;
    if let Some(found) = state {
        let removed = remove(cx, &root, found).map_err(crate::CliError::Clean)?;
        out.line(cx, format_args!("{}", Said { lead: lead.as_str(), removed: &removed }))?;
        any = true;
    }
    if let Some((home_dir, lead, found)) = held {
        if with_store {
            let removed = remove(cx, &home_dir, found).map_err(crate::CliError::Clean)?;
            out.line(cx, format_args!("{}", Said { lead: lead.as_str(), removed: &removed }))?;
            any = true;
        } else {
            let at = found.path;
            for form in crate::store::plain_place::forms::list(cx, &home_dir, &at)?.iter() {
                let removed = remove_other(cx, &home_dir, &at, form, SweepPlant::None).map_err(crate::CliError::Clean)?;
                out.line(cx, format_args!("{}", SaidOther { lead: lead.as_str(), legacy: matches!(form.kind, crate::store::plain_place::forms::FormKind::Legacy), removed: &removed }))?;
                any = true;
            }
        }
    }
    if let Some((home_dir, lead)) = home_root_dir {
        // Each row's folder by its exact path, never a name matched in a listing: a folder
        // beside them that no row names is never looked at.
        for row in crate::layout::of(crate::layout::Scope::All) {
            let Some(found) = walk(cx, &home_dir, row).map_err(crate::CliError::Clean)? else { continue };
            let removed = remove(cx, &home_dir, found).map_err(crate::CliError::Clean)?;
            out.line(cx, format_args!("{} ({})", Said { lead: lead.as_str(), removed: &removed }, row.holds))?;
            any = true;
        }
        // What `--all` keeps of the home, each said with why (a look only: nothing is entered
        // or removed, so a link there is no refusal).
        for (row, why) in crate::layout::kept_at_home() {
            if home_dir.stat(cx, &Path::new(row.path)?)?.is_some() {
                out.line(cx, format_args!("kept: {}: {why}", row.short()))?;
            }
        }
    }
    if let Some(CleanError::NotTheStore { path, why }) = &not_blazes {
        out.line(cx, format_args!("{}", NotLooked { path, why }))?;
    }
    if !any {
        out.line(cx, format_args!("{NOTHING}"))?;
    }
    Ok(bstd::entry::Status::OK)
}

/// The folder layout.rs's `row` names, beneath `dir` (the root of the row's place), when it is
/// there: each folder on its way looked at in its parent's listing, which never follows a
/// link, and refused by name when it is not a folder. The one way `clean` finds what it
/// removes: only a row's exact path is ever returned, never a name matched in a listing.
fn walk(cx: &Cx, dir: &Dir<ReadWrite>, row: &crate::layout::Row) -> Result<Option<Found>, CleanError> {
    let path = row.path;
    let mut listing = dir.own_entries(cx)?;
    let mut start = 0;
    // Bounded by the row's path: one listing a `/` in it.
    loop {
        let end = path[start..].find('/').map_or(path.len(), |at| start + at);
        let Some(at) = listing.find(&path[start..end]) else { return Ok(None) };
        folder(row.place, listing.kind(at), &path[..end])?;
        if end == path.len() {
            return Ok(Some(Found { place: row.place, path: Path::new(path)? }));
        }
        let Some(next) = dir.entries(cx, &Path::new(&path[..end])?)? else { return Ok(None) };
        listing = next;
        start = end + 1;
    }
}

/// The checkout's `.blaze/v2` (layout.rs `CHECKOUT`), when it is there and blaze's: `root` is
/// the checkout. `None` when there is nothing to remove.
pub fn checkout_state(cx: &Cx, root: &Dir<ReadWrite>) -> Result<Option<Found>, CleanError> {
    let Some(found) = walk(cx, root, &crate::layout::CHECKOUT)? else { return Ok(None) };
    let Some(entries) = root.entries(cx, &found.path)? else { return Ok(None) };
    for (i, (name, _)) in entries.iter().enumerate() {
        if !CHECKOUT_ENTRIES.contains(&name) {
            return Err(CleanError::Unknown { path: entries.path(i)? });
        }
    }
    Ok(Some(found))
}

/// The home's store root (layout.rs `STORE`), when it is there and blaze's: `home` is the home
/// folder. `None` when there is nothing to remove.
pub fn store(cx: &Cx, home: &Dir<ReadWrite>, frame: &Frame) -> Result<Option<Found>, CleanError> {
    let Some(found) = walk(cx, home, &crate::layout::STORE)? else { return Ok(None) };
    let Some(entries) = home.entries(cx, &found.path)? else { return Ok(None) };
    let refused = |why| Err(CleanError::NotTheStore { path: found.path.clone(), why });
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
    Ok(Some(found))
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

// ---- stores of other forms (D160): `blaze clean`'s, and the sweep's once unused ----

/// The store of another form `form` (as store/forms.rs `list` found it in `dir` under the
/// store root `root`) removed: a folder of another form whole; the legacy layout's own files
/// alone (`head`, `flip`, `seg-*`, `manifests-*` directly in the root: never this blaze's
/// folder beside them, nor the cache directory tag, nor a name of no store's shape).
pub(crate) fn remove_other(cx: &Cx, dir: &Dir<ReadWrite>, root: &Path, form: &crate::store::plain_place::forms::OtherForm, plant: SweepPlant) -> Result<Removed, CleanError> {
    use crate::store::plain_place::forms::FormKind;
    match form.kind {
        FormKind::Folder(name) => {
            let path = root.join(&Path::new(name.as_str())?);
            remove(cx, dir, Found { place: Place::Home, path })
        }
        FormKind::Legacy => {
            let blaze = dir;
            let mut removed = Removed { place: Place::Home, path: root.clone(), files: 0, bytes: 0 };
            let Some(entries) = blaze.entries(cx, root)? else { return Ok(removed) };
            for i in 0..entries.len() {
                let name = entries.name(i);
                let ours = blaze_cas::plain::store_leaf(name) || (plant == SweepPlant::LegacyTakesTheTag && name == blaze_cas::plain::CACHEDIR_TAG);
                if entries.kind(i) == EntryKind::File && ours {
                    blaze.remove(cx, &entries.path(i)?, bstd::fs::Durable::No)?;
                    removed.files += 1;
                    removed.bytes += entries.len_of(i);
                }
            }
            Ok(removed)
        }
    }
}

/// A planted bug of the sweep, for its controls (clean_tests.rs): `None` in every build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SweepPlant {
    None,
    /// Takes this blaze's own folder for another form's.
    TakesItsOwn,
    /// Removes a store of another form whatever its last use.
    IgnoresUse,
    /// Takes the store root's cache directory tag for a legacy file.
    LegacyTakesTheTag,
}

/// THE SWEEP OF OTHER FORMS, run in the cleanup after a command's answer (never before it):
/// every store of another form under `blaze` (the home's `.blaze`; its store root
/// plain_place.rs `STORE_ROOT`) whose last use is more than store/forms.rs `EXPIRY_DAYS`
/// before `now_ns` removed (`expired`), each said on `err` with what it freed. This blaze's
/// folder is never one of them: `list` skips the folder of its own forms, and nothing of
/// another shape is listed. How many it removed.
pub(crate) fn sweep_other_forms(cx: &Cx, blaze: &Dir<ReadWrite>, now_ns: u64, err: &bstd::out::Out, plant: SweepPlant) -> Result<u32, crate::CliError> {
    use crate::store::plain_place::forms::{self, FormKind, OtherForm};
    let root = Path::new(crate::store::plain_place::STORE_ROOT)?;
    let mut found = forms::list(cx, blaze, &root)?;
    let own = blaze_cas::plain::PlaceName::THIS;
    let own_path = Path::new(crate::store::plain_place::STORE_DIR)?;
    if plant == SweepPlant::TakesItsOwn && blaze.stat(cx, &own_path).map_err(|error| crate::CliError::Clean(CleanError::Fs(error)))?.is_some() {
        found = forms::OtherForms::with(found, OtherForm { kind: FormKind::Folder(own), files: 0, bytes: 0, used_ns: 0 })?;
    }
    let mut removed = 0;
    for form in found.iter() {
        if !(forms::expired(form, now_ns) || plant == SweepPlant::IgnoresUse) {
            continue;
        }
        let gone = remove_other(cx, blaze, &root, form, plant).map_err(crate::CliError::Clean)?;
        err.line(
            cx,
            format_args!(
                "blaze: removed a store of another blaze's forms, unused for {} days: the home's .blaze/{}{}: {} files, {} bytes freed",
                forms::EXPIRY_DAYS,
                gone.path,
                if matches!(form.kind, FormKind::Legacy) { " (its format 4 files)" } else { "" },
                gone.files,
                gone.bytes
            ),
        )?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
#[path = "clean_tests.rs"]
mod tests;
