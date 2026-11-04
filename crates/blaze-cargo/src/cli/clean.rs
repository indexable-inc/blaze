//! `blaze clean`: a workspace's build directory removed. blaze writes no build file into the
//! checkout: a workspace builds into its directory under blaze's root
//! ([`blaze_manifest::home::build_dir`], `<root>/build/<name>-<id>`), unless `--target-dir`,
//! `CARGO_TARGET_DIR` or `build.target-dir` names one, and that directory is what is removed.
//! `--list` shows every workspace's directory under blaze's root with its size, `--all`
//! removes them all. `--dry-run` says what would go and removes nothing.
//!
//! It reads the workspace here, as `cargo metadata` does ([`blaze_manifest::Workspace`]), so
//! the directory is the one the daemon builds into: the same root, the same id of the
//! workspace's root. It asks nothing of the daemon, and goes through the directory's lease as
//! the collector does ([`blaze_gc::lease`], docs/gc.md). What it removes, and why it is safe:
//!
//! 1. The directory is what it names ([`guard`]): a symlink is refused, and the canonical
//!    path is refused when it is, or holds, the workspace's checkout, `$HOME`, `/`, the current
//!    directory, `CARGO_HOME`, `RUSTUP_HOME`, blaze's root, its `build/` itself or the
//!    machine's shared directory (so `..` and
//!    `../repo` are refused, however spelled), or is inside `CARGO_HOME`, the daemon's state
//!    (`<root>/v2`) or the std stores (`<machine>/std`): the list the daemon's marks refuse too
//!    ([`blaze_gc::fs::Protected`], review N13).
//! 2. It is a build directory ([`blaze_gc::record::is_marked`]): it carries blaze's lease or
//!    record, or a `CACHEDIR.TAG` with its signature (cargo's and blaze's build directories
//!    have one). Anything else is refused, named.
//! 3. It is on a local file system ([`blaze_gc::fs::check_local`]), where leases hold.
//! 4. A directory inside the workspace's checkout (a project's `./target`) that no build has
//!    leased is refused unless `--force`: a daemon's build leases such a directory (review S2),
//!    so one without a lease may be written by a cargo or a blaze from before leases.
//! 5. No mount is at or under it ([`blaze_gc::fs::mount_inside`]), and the removal never leaves
//!    its file system ([`blaze_gc::fs::remove_tree`]).
//! 6. The leases inside it (another build's dir, a `--target-dir` named inside this one) are
//!    found by a walk that takes no lock ([`blaze_gc::fs::nested_lease_dirs`], review round 6
//!    P1). Then its own lease is taken exclusively, made first when it has none, and checked to
//!    be the directory's own ([`blaze_gc::lease::take_exclusive`]); one a daemon's build or a
//!    running binary holds is refused, naming it. Then each lease found inside is taken
//!    exclusively too ([`blaze_gc::fs::take_leases`]), and one held refuses the whole tree,
//!    naming that dir (review round 5 B1).
//! 7. All those locks are held through the whole removal: the rename (into `<root>/.blaze-trash/`,
//!    the collector's trash, for a directory under blaze's `build/` on the root's file system,
//!    else beside it as `.<name>.blaze-removing-<pid>-<20 digits>`), then a second walk of the
//!    renamed tree that takes any lease made inside since the first walk; when a build holds
//!    one (it took it between the walk and the lock), the tree is renamed back at once and the
//!    clean fails naming that dir ([`blaze_gc::fs::recheck_renamed`], review rounds 6 and 7
//!    C1); then the unlink of what was renamed. So no build ever sees it half removed, and no build that
//!    locked a lease inside it meanwhile loses its files: what is left stays until that build
//!    ends. What a clean killed mid-removal left beside a directory is removed by the next clean
//!    of it, once its pid no longer runs and every lease in it is taken exclusively
//!    ([`sweep_leftovers`], review N6).

use std::{
    ffi::OsString,
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
};

use blaze_core::exit::Exit;
use blaze_manifest::{Config, Context, Workspace, metadata::root_manifest};

use super::say;

const USAGE_TEXT: &str = "blaze clean [--dry-run] [--force] [--manifest-path PATH] [--target-dir DIR] | blaze clean --list | blaze clean --all [--dry-run]";

/// What `blaze clean` is asked.
#[derive(Debug, Default, PartialEq, Eq)]
struct Asked {
    list: bool,
    all: bool,
    dry_run: bool,
    /// Removes an unleased directory inside the checkout too (step 4).
    force: bool,
    manifest_path: Option<String>,
    target_dir: Option<String>,
}

fn parse(words: Vec<OsString>) -> Result<Asked, String> {
    let mut asked = Asked::default();
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let Some(text) = word.to_str() else {
            return Err(format!("{USAGE_TEXT}, not `{}`", word.to_string_lossy()));
        };
        let (flag, attached) = match text.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (text, None),
        };
        let mut value = |flag: &str| match attached.clone() {
            Some(value) => Ok(value),
            None => words
                .next()
                .and_then(|it| it.into_string().ok())
                .ok_or_else(|| format!("`{flag}` takes a value: {USAGE_TEXT}")),
        };
        match flag {
            "--list" => asked.list = true,
            "--all" => asked.all = true,
            "--dry-run" | "-n" => asked.dry_run = true,
            "--force" => asked.force = true,
            "--manifest-path" => asked.manifest_path = Some(value(flag)?),
            "--target-dir" => asked.target_dir = Some(value(flag)?),
            "--release" | "-r" | "--profile" | "--doc" | "-p" | "--package" | "--workspace"
            | "--target" => {
                return Err(format!(
                    "blaze clean removes a workspace's whole build directory: `{flag}`, a part of it, is not read yet"
                ));
            }
            _ => return Err(format!("{USAGE_TEXT}, not `{text}`")),
        }
    }
    if asked.list && (asked.all || asked.dry_run || asked.force) {
        return Err(format!("`--list` removes nothing and takes no other flag: {USAGE_TEXT}"));
    }
    if (asked.list || asked.all) && (asked.manifest_path.is_some() || asked.target_dir.is_some()) {
        return Err(format!(
            "`--list` and `--all` are of every workspace's directory under blaze's root, not one workspace's: {USAGE_TEXT}"
        ));
    }
    Ok(asked)
}

/// `blaze clean`, with the words after it.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    let refused = |what: &str, code: i32| {
        eprint!("{}", say::error(what, color));
        code
    };
    let asked = match parse(words) {
        Ok(asked) => asked,
        Err(what) => return refused(&what, Exit::Usage.code()),
    };
    let done = match (asked.list, asked.all) {
        (true, _) => list(),
        (false, true) => all(asked.dry_run, color),
        (false, false) => workspace(&asked, color),
    };
    match done {
        Ok(()) => 0,
        Err(what) => refused(&what, Exit::Failed.code()),
    }
}

/// Where every workspace's build directory is: `build` under blaze's root.
fn builds() -> Result<PathBuf, String> {
    crate::serve::builds().map_err(|error| format!("{error:#}"))
}

/// The workspace's build directory removed: the one `--target-dir`, `CARGO_TARGET_DIR` or
/// `build.target-dir` names, else its directory under blaze's root; and `build.build-dir`'s
/// when that is another.
fn workspace(asked: &Asked, color: bool) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|error| format!("the current directory: {error}"))?;
    // What cargo reads of the environment: its variables are UTF-8 (`std::env::var`). blaze's
    // root is read apart, as the daemon reads it ([`crate::serve::builds`]).
    let env = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)));
    let read = || -> anyhow::Result<(PathBuf, Vec<PathBuf>)> {
        let ctx = Context::new(Config::load(&cwd, env, &[])?);
        let manifest = root_manifest(asked.manifest_path.as_deref().map(Path::new), &ctx)?;
        let ws = Workspace::new(&manifest, &ctx)?;
        let target = match (&asked.target_dir, ws.explicit_target_dir()) {
            (Some(dir), _) => cwd.join(dir),
            (None, Some(dir)) => dir.to_path_buf(),
            (None, None) => blaze_manifest::home::build_dir(&crate::serve::builds()?, ws.root()),
        };
        let mut dirs = vec![target];
        if let Some(build) = ws.explicit_build_dir()
            && asked.target_dir.is_none()
            && !dirs.iter().any(|it| it == build)
        {
            dirs.push(build.to_path_buf());
        }
        Ok((ws.root().to_path_buf(), dirs))
    };
    let (root, dirs) = read().map_err(|error| format!("{error:#}"))?;
    let builds = builds()?;
    let protected = protected(Some(&root), &cwd, &builds)?;
    let mut removed = Tally::default();
    let at = Removing {
        protected: &protected,
        builds: &builds,
        checkout: Some(&root),
        force: asked.force,
        dry_run: asked.dry_run,
    };
    for dir in &dirs {
        removed.add(remove(dir, &at)?);
    }
    eprint!("{}", removed.said(asked.dry_run, color));
    Ok(())
}

/// Every workspace's build directory under blaze's root removed.
fn all(dry_run: bool, color: bool) -> Result<(), String> {
    let builds = builds()?;
    let cwd = std::env::current_dir().map_err(|error| format!("the current directory: {error}"))?;
    let protected = protected(None, &cwd, &builds)?;
    let mut removed = Tally::default();
    // Each directory is tried; what is refused is said at the end, all of it.
    let mut refused = String::new();
    let at =
        Removing { protected: &protected, builds: &builds, checkout: None, force: false, dry_run };
    for (dir, _) in entries(&builds)? {
        match remove(&dir, &at) {
            Ok(tally) => removed.add(tally),
            Err(why) => _ = writeln!(refused, "  {why}"),
        }
    }
    eprint!("{}", removed.said(dry_run, color));
    match refused.is_empty() {
        true => Ok(()),
        false => Err(format!("blaze clean --all left some directories:\n{refused}")),
    }
}

/// Every workspace's build directory under blaze's root, each with its size, whether a build
/// holds it now, when it was last used and the checkout it builds (its record,
/// [`blaze_gc::record`]), by name.
fn list() -> Result<(), String> {
    let builds = builds()?;
    let now = blaze_gc::record::now_ns();
    let mut out = String::new();
    for (dir, tally) in entries(&builds)? {
        let state = match blaze_gc::lease::take_exclusive(&dir)? {
            blaze_gc::lease::Taken::Held => "in use",
            blaze_gc::lease::Taken::Exclusive(_) => "idle",
            blaze_gc::lease::Taken::Moved => "moving",
            blaze_gc::lease::Taken::Missing => "no lease",
        };
        let (used, checkout) = match blaze_gc::record::read(&dir) {
            Ok(Some(record)) => (
                ago(now.saturating_sub(record.used_ns)),
                match record.checkout {
                    Some(checkout) if Path::new(&checkout).exists() => checkout,
                    Some(checkout) => format!("{checkout} (gone)"),
                    None => "-".to_owned(),
                },
            ),
            Ok(None) => ("-".to_owned(), "(no record)".to_owned()),
            Err(why) => ("-".to_owned(), format!("(record unreadable: {why})")),
        };
        _ = writeln!(
            out,
            "{:>10}  {state:<8}  {used:>9}  {}  {checkout}",
            size(tally.bytes),
            dir.display()
        );
    }
    print!("{out}");
    Ok(())
}

/// `ns` ago, in the largest unit that fits: `3d ago`.
fn ago(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// The directories in `builds`, sorted by name, each with what it holds; none when `builds`
/// is not there yet.
fn entries(builds: &Path) -> Result<Vec<(PathBuf, Tally)>, String> {
    let read = match std::fs::read_dir(builds) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("listing {}: {error}", builds.display())),
    };
    let mut dirs = Vec::new();
    for entry in read {
        let entry = entry.map_err(|error| format!("listing {}: {error}", builds.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| format!("the type of {}: {error}", path.display()))?;
        if kind.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    dirs.into_iter()
        .map(|dir| {
            let tally = walk(&dir)?;
            Ok((dir, tally))
        })
        .collect()
}

/// Files and bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tally {
    files: u64,
    bytes: u64,
}

impl Tally {
    fn add(&mut self, other: Tally) {
        self.files += other.files;
        self.bytes += other.bytes;
    }

    /// cargo's summary: `     Removed 12 files, 3.4MiB total`.
    fn said(self, dry_run: bool, color: bool) -> String {
        let files = match self.files {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        let word = if dry_run { "Summary" } else { "Removed" };
        let tail = if dry_run { " (dry run: nothing removed)" } else { "" };
        say::status(word, &format!("{files}, {} total{tail}", size(self.bytes)), color)
    }
}

/// What is under `dir`, symlinks not followed; nothing for a `dir` that is not there.
fn walk(dir: &Path) -> Result<Tally, String> {
    let mut tally = Tally::default();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(at) = todo.pop() {
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("reading {}: {error}", at.display())),
        };
        if !meta.is_dir() {
            tally.files += 1;
            tally.bytes += meta.len();
            continue;
        }
        let read =
            std::fs::read_dir(&at).map_err(|error| format!("listing {}: {error}", at.display()))?;
        for entry in read {
            todo.push(entry.map_err(|error| format!("listing {}: {error}", at.display()))?.path());
        }
    }
    Ok(tally)
}

/// What no removal may be, hold or be inside: the list the daemon's marks refuse too
/// ([`blaze_gc::fs::Protected`]: `/`, `$HOME`, `CARGO_HOME` and what is in it, blaze's root
/// (`builds`' parent), `builds` itself and the root's state, the machine's shared directory and
/// its std stores; review N13 and round 5's B1), and the workspace's checkout and `cwd`. A path
/// of it that cannot be resolved is an error naming it.
fn protected(
    checkout: Option<&Path>,
    cwd: &Path,
    builds: &Path,
) -> Result<blaze_gc::fs::Protected, String> {
    let root = builds.parent().ok_or_else(|| {
        format!("{} has no parent: it is not blaze's build directories", builds.display())
    })?;
    let machine = crate::serve::machine().map_err(|error| {
        format!("the machine's shared directory, which blaze clean never removes: {error:#}")
    })?;
    let mut protected = blaze_gc::fs::Protected::of_env(root, Some(&machine))?;
    if let Some(checkout) = checkout {
        protected.hold(checkout)?;
    }
    protected.hold(cwd)?;
    Ok(protected)
}

/// `dir` as it is on disk: its canonical path, or `None` when it is not there. Refuses a
/// symlink, a file, and a directory `protected` refuses (one that is or holds the checkout,
/// `$HOME`, `/`, `cwd` or blaze's root, or is inside `CARGO_HOME`, blaze's state or the std
/// stores).
fn guard(dir: &Path, protected: &blaze_gc::fs::Protected) -> Result<Option<PathBuf>, String> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("reading {}: {error}", dir.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink: blaze clean removes only a directory it names itself",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "{} is not a directory: blaze clean removes only a build directory",
            dir.display()
        ));
    }
    let canonical = std::fs::canonicalize(dir)
        .map_err(|error| format!("resolving {}: {error}", dir.display()))?;
    // By path, and by device and inode (a bind mount or a second name of the same directory).
    if let Some(why) = protected.refuses(&canonical)? {
        return Err(format!("{why}: blaze clean does not remove it"));
    }
    Ok(Some(canonical))
}

/// What a removal is checked against.
struct Removing<'a> {
    /// What no removal may be, hold or be inside ([`protected`]).
    protected: &'a blaze_gc::fs::Protected,
    /// blaze's directory of build directories.
    builds: &'a Path,
    /// The workspace's checkout, for a workspace's own directories.
    checkout: Option<&'a Path>,
    /// An unleased directory inside the checkout is removed too.
    force: bool,
    dry_run: bool,
}

/// `dir` and everything under it removed (unless `dry_run`): what it held. Refused unless it
/// passes [`guard`], carries blaze's marks, is on a local file system, is leased or `--force`n
/// when inside the checkout, and holds no mount, and while anyone holds its lease; otherwise
/// renamed away and unlinked while its lease is held exclusively (the module's steps 1 to 6).
fn remove(dir: &Path, at: &Removing<'_>) -> Result<Tally, String> {
    let Some(dir) = guard(dir, at.protected)? else { return Ok(Tally::default()) };
    if !blaze_gc::record::is_marked(&dir) {
        return Err(format!(
            "{} carries none of a build directory's marks ({}, {}, or a {} with its signature): blaze clean does not remove it",
            dir.display(),
            blaze_gc::lease::LEASE,
            blaze_gc::record::RECORD,
            blaze_gc::record::CACHEDIR_TAG
        ));
    }
    blaze_gc::fs::check_local(&dir)?;
    let checkout =
        at.checkout.map(|it| std::fs::canonicalize(it).unwrap_or_else(|_| it.to_path_buf()));
    if let Some(checkout) = &checkout
        && dir.starts_with(checkout)
        && !blaze_gc::lease::has_lease(&dir)
        && !at.force
    {
        return Err(format!(
            "{} is inside the checkout {} and no build has leased it ({}): a cargo, or a blaze from before leases, may be writing into it. `blaze clean --force` removes it anyway",
            dir.display(),
            checkout.display(),
            blaze_gc::lease::lease_of(&dir).display()
        ));
    }
    if let Some(mount) = blaze_gc::fs::mount_inside(&dir)? {
        return Err(format!(
            "{} is a mount point at or under {}: blaze clean does not remove it",
            mount.display(),
            dir.display()
        ));
    }
    let tally = walk(&dir)?;
    let in_use = |dir: &Path| {
        format!(
            "{} is in use: a build holds its lease ({}). blaze clean removes it once that build ends",
            dir.display(),
            blaze_gc::lease::lease_of(dir).display()
        )
    };
    let changed = |dir: &Path| {
        format!(
            "{} was moved or made again while blaze clean took its lease: nothing of it is removed; run it again",
            dir.display()
        )
    };
    let refused = |why: blaze_gc::fs::Nested| format!("{why}: blaze clean does not remove it");
    // The build dirs inside it (a `--target-dir` named inside this one), found before any lock
    // is taken, so the walk holds no build off (review round 6, P1); one made since is found by
    // the walk after the rename.
    let inside = blaze_gc::fs::nested_lease_dirs(&dir, Some(&dir)).map_err(refused)?;
    if at.dry_run {
        return match blaze_gc::lease::take_exclusive(&dir)? {
            blaze_gc::lease::Taken::Held => Err(in_use(&dir)),
            _ => {
                // alloc: a dry run's locks inside the tree, a few, let go at once
                let mut locks = Vec::new();
                blaze_gc::fs::take_leases(&dir, &inside, &mut locks).map_err(refused)?;
                Ok(tally)
            }
        };
    }
    if !blaze_gc::lease::has_lease(&dir) {
        // A build directory no daemon leased (a `--target-dir`, or one from before leases):
        // leased now, so a build that starts in it meanwhile waits on the lock below.
        blaze_gc::lease::make(&dir)?;
    }
    // Held to the end of the removal: through the rename and the unlink.
    let own = match blaze_gc::lease::take_exclusive(&dir)? {
        blaze_gc::lease::Taken::Exclusive(file) => file,
        blaze_gc::lease::Taken::Held => return Err(in_use(&dir)),
        blaze_gc::lease::Taken::Moved | blaze_gc::lease::Taken::Missing => {
            return Err(changed(&dir));
        }
    };
    // Every other build dir inside it (a `--target-dir` named inside this one): each lease
    // taken exclusively and held with its own through the rename and the unlink, so a tree
    // holding a build in use is never moved (review round 5, B1). The renamed tree is walked
    // again, these held, before anything is unlinked (review round 6, C1).
    let mut locks = vec![own]; // alloc: a removal's locks, a few
    blaze_gc::fs::take_leases(&dir, &inside, &mut locks).map_err(refused)?;
    let name = dir.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    let parent = dir.parent().ok_or_else(|| format!("{} has no parent", dir.display()))?;
    let now = blaze_gc::record::now_ns();
    // The collector's trash, `<root>/.blaze-trash` beside `build/` as named (not as resolved:
    // the collector sweeps the root's), and only when `dir` is on the root's file system, so
    // the rename is atomic (reverses round 2's M3, which put it beside the resolved `build/`,
    // where no collector sweeps when `build/` is a link).
    let under_builds = std::fs::canonicalize(at.builds).ok().is_some_and(|it| it == parent);
    let root = at.builds.parent().filter(|root| under_builds && same_device(root, &dir));
    match root {
        Some(root) => {
            // The collector's own naming of trash ([`blaze_gc::plan::trash_name`]).
            let to = blaze_gc::fs::trash_dir(root)?.join(blaze_gc::plan::trash_name(
                now,
                u64::from(std::process::id()),
                &dir.to_string_lossy(),
            ));
            match blaze_gc::fs::rename_into_trash(&dir, &to) {
                blaze_gc::plan::Swept::Moved => {}
                blaze_gc::plan::Swept::Failed(why) => return Err(why),
                _ => return Err(changed(&dir)),
            }
            // A lease a build took inside between the walk and the lock: the tree goes back,
            // named (review round 7, C1).
            blaze_gc::fs::recheck_renamed(&dir, &to, &mut locks).map_err(refused)?;
            blaze_gc::fs::unlink(&to, locks).map_err(|why| {
                format!("removing {} (renamed from {}): {why}; the collector removes it once it is free", to.display(), dir.display())
            })?;
        }
        None => {
            // What an earlier clean of it left, killed mid-removal (review N6).
            for why in sweep_leftovers(parent, &name) {
                eprintln!("blaze clean: {why}");
            }
            // Beside it, on its own file system: the rename is atomic, and nothing else is named so.
            let to = parent.join(format!(".{name}{REMOVING}{}-{now:020}", std::process::id()));
            match blaze_gc::fs::rename_no_replace(&dir, &to) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(changed(&dir)),
                Err(error) => {
                    return Err(format!("renaming {} to {}: {error}", dir.display(), to.display()));
                }
            }
            // A lease a build took inside between the walk and the lock: the tree goes back,
            // named (review round 7, C1); one it could not move back is left, and the next clean
            // of it sweeps it once that build ends.
            blaze_gc::fs::recheck_renamed(&dir, &to, &mut locks).map_err(refused)?;
            blaze_gc::fs::remove_tree(&to).map_err(|why| {
                format!(
                    "removing {} (renamed from {}): {why}; what is left can be removed by hand",
                    to.display(),
                    dir.display()
                )
            })?;
            drop(locks);
        }
    }
    Ok(tally)
}

/// The infix of a directory renamed beside itself for removal:
/// `.<name>.blaze-removing-<pid>-<ns>`, `<ns>` as 20 digits.
const REMOVING: &str = ".blaze-removing-";

/// The pid of a clean's leftover named `.<name>.blaze-removing-<pid>-<20 digits>` from what
/// follows the infix (`<pid>-<20 digits>`), or `None` for anything else, which is no leftover of
/// a clean's and is never touched (review round 5, L3).
fn leftover_pid(rest: &str) -> Option<u32> {
    let (pid, ns) = rest.split_once('-')?;
    let digits = |it: &str| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit());
    if !digits(pid) || ns.len() != 20 || !digits(ns) {
        return None;
    }
    pid.parse().ok()
}

/// What a `blaze clean` of `name` killed mid-removal left in `parent`
/// (exactly `.<name>.blaze-removing-<pid>-<20 digits>`, [`remove`], [`leftover_pid`]) removed, each only once its pid no longer
/// runs and its lease, which moved with it, is taken exclusively: a clean still removing it
/// holds that lease (review N6). Each that is left says why; none fails this clean.
fn sweep_leftovers(parent: &Path, name: &str) -> Vec<String> {
    let prefix = format!(".{name}{REMOVING}");
    // alloc: once per clean, what is left said
    let mut problems = Vec::new();
    let read = match std::fs::read_dir(parent) {
        Ok(read) => read,
        Err(error) => {
            problems.push(format!(
                "listing {} for what an earlier clean left: {error}",
                parent.display()
            ));
            return problems;
        }
    };
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                problems.push(format!(
                    "listing {} for what an earlier clean left: {error}",
                    parent.display()
                ));
                continue;
            }
        };
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = file_name.strip_prefix(&prefix) else { continue };
        let Some(pid) = leftover_pid(rest) else { continue };
        let left = entry.path();
        if crate::raw::process::alive(pid) || pid == std::process::id() {
            continue;
        }
        // A symlink or a file of that name is no leftover of a clean's.
        if !std::fs::symlink_metadata(&left).is_ok_and(|it| it.is_dir()) {
            continue;
        }
        // alloc: a leftover's locks, a few
        let mut locks = Vec::new();
        match blaze_gc::lease::take_exclusive(&left) {
            Ok(blaze_gc::lease::Taken::Exclusive(file)) => locks.push(file),
            // A dir from before leases has none; its pid is gone, so no clean removes it now.
            Ok(blaze_gc::lease::Taken::Missing) => {}
            Ok(_) => {
                problems.push(format!("{} is left: its lease is held or moved", left.display()));
                continue;
            }
            Err(why) => {
                problems.push(format!("{} is left: {why}", left.display()));
                continue;
            }
        }
        // A build dir inside it that a build locked since: it stays until that build ends,
        // these locks held through the removal (review round 6, C1).
        if let Err(why) = blaze_gc::fs::take_nested_leases(&left, None, &mut locks) {
            problems.push(format!("{} is left: {why}", left.display()));
            continue;
        }
        match blaze_gc::fs::mount_inside(&left) {
            Ok(None) => {}
            Ok(Some(mount)) => {
                problems.push(format!(
                    "{} is left: {} is a mount point inside it",
                    left.display(),
                    mount.display()
                ));
                continue;
            }
            Err(why) => {
                problems.push(format!("{} is left: {why}", left.display()));
                continue;
            }
        }
        if let Err(why) = blaze_gc::fs::remove_tree(&left) {
            problems.push(format!(
                "removing {}, left by a clean that ended (pid {pid}): {why}",
                left.display()
            ));
        }
        drop(locks);
    }
    problems
}

/// Whether `a` and `b` are on one file system (one device on unix; Windows renames within a
/// volume, and a walk there never enters a mounted one).
fn same_device(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        a.components().next() == b.components().next()
    }
}

/// `bytes` as cargo's `human_readable_bytes` prints them: `3.4MiB`.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes}B"),
        _ => format!("{value:.1}{}", UNITS[unit]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(it: &[&str]) -> Vec<OsString> {
        it.iter().map(OsString::from).collect()
    }

    #[test]
    fn clean_reads_its_flags_and_refuses_a_part_of_a_directory() {
        assert_eq!(parse(words(&[])), Ok(Asked::default()));
        let asked = parse(words(&["--dry-run", "--target-dir=out"])).unwrap();
        assert!(asked.dry_run && asked.target_dir.as_deref() == Some("out"));
        assert!(parse(words(&["--all"])).unwrap().all);
        assert!(parse(words(&["--list"])).unwrap().list);
        assert!(parse(words(&["--list", "--all"])).is_err());
        assert!(parse(words(&["--all", "--target-dir", "x"])).is_err());
        assert!(parse(words(&["--release"])).unwrap_err().contains("`--release`"));
        assert_eq!(size(512), "512B");
        assert_eq!(size(3 << 20), "3.0MiB");
    }

    /// A fresh directory of this test's own under the system's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blaze-clean-{name}-{}-{}",
            std::process::id(),
            blaze_gc::record::now_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn clean_refuses_dotdot_and_a_symlinked_checkout() {
        let top = scratch("guard");
        let repo = top.join("repo");
        std::fs::create_dir_all(repo.join("target")).unwrap();
        let kept = protected(Some(&repo), &top.join("elsewhere"), &top.join("build"))
            .unwrap_or_else(|why| panic!("{why}"));
        // `repo/target/..` is the checkout, `repo/target/../..` holds it.
        assert!(
            guard(&repo.join("target").join(".."), &kept)
                .unwrap_err()
                .contains("does not remove it")
        );
        assert!(guard(&repo.join("target").join("..").join(".."), &kept).is_err());
        #[cfg(unix)]
        {
            let link = top.join("link");
            std::os::unix::fs::symlink(&repo, &link).unwrap();
            assert!(guard(&link, &kept).unwrap_err().contains("symlink"));
        }
        assert_eq!(guard(&repo.join("target"), &kept).unwrap(), Some(repo.join("target")));
        assert_eq!(guard(&top.join("not-there"), &kept).unwrap(), None);
        std::fs::remove_dir_all(&top).unwrap();
    }

    /// A build directory's marks inside `CARGO_HOME` (cargo's `git/` carries a `CACHEDIR.TAG`)
    /// or blaze's state do not make it removable: named as a target dir, it is refused, and
    /// nothing in it goes (review N13).
    #[test]
    fn clean_never_removes_a_tagged_dir_inside_cargo_home_or_the_daemons_state() {
        let top = scratch("protected");
        let builds = top.join("build");
        let cargo_git = top.join("cargo").join("git");
        let state = top.join("v2").join("store");
        for dir in [&cargo_git, &state] {
            std::fs::create_dir_all(dir).unwrap();
            blaze_gc::record::tag_cache_dir(dir).unwrap_or_else(|why| panic!("{why}"));
            blaze_gc::lease::make(dir).unwrap_or_else(|why| panic!("{why}"));
        }
        let mut kept =
            blaze_gc::fs::Protected::new(None, Some(top.join("cargo")), None, &top, None)
                .unwrap_or_else(|why| panic!("{why}"));
        kept.hold(&top.join("elsewhere")).unwrap_or_else(|why| panic!("{why}"));
        let at = Removing {
            protected: &kept,
            builds: &builds,
            checkout: None,
            force: true,
            dry_run: false,
        };
        for dir in [&cargo_git, &state] {
            let refused = remove(dir, &at).unwrap_err();
            assert!(refused.contains("never removes or marks"), "{refused}");
            assert!(dir.join(blaze_gc::record::CACHEDIR_TAG).is_file());
        }
        std::fs::remove_dir_all(&top).unwrap();
    }

    /// `<root>/build` named as a target dir is refused whole, and a target dir holding another
    /// build's leased dir is not removed while that build holds it: nothing of it moves, and
    /// the error names the dir inside (review round 5, B1).
    #[test]
    fn clean_refuses_a_tree_holding_another_builds_lease() {
        let top = scratch("nested");
        let builds = top.join("build");
        let workspace = builds.join("ws-1");
        std::fs::create_dir_all(&workspace).unwrap();
        blaze_gc::record::tag_cache_dir(&workspace).unwrap_or_else(|why| panic!("{why}"));
        let in_use = blaze_gc::lease::Lease::hold(&workspace).unwrap_or_else(|why| panic!("{why}"));
        let kept =
            protected(None, &top.join("elsewhere"), &builds).unwrap_or_else(|why| panic!("{why}"));
        let at = Removing {
            protected: &kept,
            builds: &builds,
            checkout: None,
            force: true,
            dry_run: false,
        };
        // `CARGO_TARGET_DIR=<root>/build`: never removed.
        blaze_gc::record::tag_cache_dir(&builds).unwrap_or_else(|why| panic!("{why}"));
        let refused = remove(&builds, &at).unwrap_err();
        assert!(refused.contains("never removes or marks"), "{refused}");
        assert!(workspace.is_dir());
        // A `--target-dir` with another build's leased dir inside it.
        let outer = top.join("out");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("artifact"), b"in use").unwrap();
        blaze_gc::record::tag_cache_dir(&outer).unwrap_or_else(|why| panic!("{why}"));
        let inner_use = blaze_gc::lease::Lease::hold(&inner).unwrap_or_else(|why| panic!("{why}"));
        for dry_run in [true, false] {
            let refused = remove(&outer, &Removing { dry_run, ..at }).unwrap_err();
            assert!(
                refused.contains(&inner.display().to_string())
                    && refused.contains("holds its lease"),
                "{refused}"
            );
            assert!(inner.join("artifact").is_file());
        }
        // Once that build lets go, the tree goes whole.
        drop(inner_use);
        remove(&outer, &at).unwrap_or_else(|why| panic!("{why}"));
        assert!(!outer.exists());
        drop(in_use);
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn clean_refuses_an_unmarked_dir() {
        let top = scratch("marks");
        let builds = top.join("build");
        let plain = top.join("photos");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("a.jpg"), b"kept").unwrap();
        let kept =
            protected(None, &top.join("elsewhere"), &builds).unwrap_or_else(|why| panic!("{why}"));
        let at = Removing {
            protected: &kept,
            builds: &builds,
            checkout: None,
            force: false,
            dry_run: false,
        };
        let refused = remove(&plain, &at).unwrap_err();
        assert!(refused.contains("carries none of a build directory's marks"), "{refused}");
        assert!(plain.join("a.jpg").is_file());
        // A CACHEDIR.TAG without the signature is no mark either; with it, the directory goes.
        std::fs::write(plain.join(blaze_gc::record::CACHEDIR_TAG), b"Signature: wrong").unwrap();
        assert!(remove(&plain, &at).is_err());
        std::fs::remove_file(plain.join(blaze_gc::record::CACHEDIR_TAG)).unwrap();
        blaze_gc::record::tag_cache_dir(&plain).unwrap_or_else(|why| panic!("{why}"));
        // What it held before its lease was made: the photo and the tag.
        assert_eq!(remove(&plain, &at).unwrap().files, 2);
        assert!(!plain.exists());
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn clean_refuses_an_unleased_target_inside_the_checkout_unless_forced() {
        let top = scratch("inside");
        let repo = top.join("repo");
        let target = repo.join("target");
        std::fs::create_dir_all(&target).unwrap();
        blaze_gc::record::tag_cache_dir(&target).unwrap_or_else(|why| panic!("{why}"));
        let builds = top.join("build");
        let kept = protected(Some(&repo), &top.join("elsewhere"), &builds)
            .unwrap_or_else(|why| panic!("{why}"));
        let at = Removing {
            protected: &kept,
            builds: &builds,
            checkout: Some(&repo),
            force: false,
            dry_run: false,
        };
        let refused = remove(&target, &at).unwrap_err();
        assert!(
            refused.contains("no build has leased it") && refused.contains("--force"),
            "{refused}"
        );
        assert!(target.is_dir());
        // Leased by a build (that ended): removed without --force.
        blaze_gc::lease::make(&target).unwrap_or_else(|why| panic!("{why}"));
        assert_eq!(remove(&target, &at).unwrap().files, 2);
        assert!(!target.exists());
        // Unleased again, and forced.
        std::fs::create_dir_all(&target).unwrap();
        blaze_gc::record::tag_cache_dir(&target).unwrap_or_else(|why| panic!("{why}"));
        let forced = Removing { force: true, ..at };
        assert_eq!(remove(&target, &forced).unwrap().files, 1);
        assert!(!target.exists());
        std::fs::remove_dir_all(&top).unwrap();
    }
}
