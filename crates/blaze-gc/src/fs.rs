//! The collector's driver on `std::fs`: what a [`Decision`] asks, done, and the answer handed
//! back to the planner. It blocks on the file system only, never on a lock (every lease is
//! tried, not waited for), and is called off the daemon's loop, in a background job of the
//! pool, or by `blaze cache gc` in the CLI.
//!
//! What it lists (and so all the collector can ever touch):
//!
//! - In the first area, blaze's root: `build/<dir>`, a [`Class::Workspace`]; each directory in
//!   it a [`Class::Profile`], and each file in it ending `.tmp` a [`Class::Temp`]. A workspace
//!   with no lease file, no record that reads, or a checkout whose state cannot be told is
//!   refused, and so are its profiles and temporaries.
//! - In the last area, the machine's shared directory: `std/<makers>` (one per code identity of what makes std's records), a [`Class::Std`];
//!   refused without a lease file.
//! - In each area, `.blaze-trash/<entry>` ([`Class::Trash`]), only when the trash holds blaze's
//!   marker ([`trash_dir`]), and only entries named as blaze names them ([`is_trash_name`]).
//!
//! Nothing else under an area is listed: not the daemon's state (`<root>/v2`), its store or
//! image, nor anything in trash blaze did not put there. An area on a network file system is
//! refused whole ([`check_local`]): the locks leases rest on do not hold there.
//!
//! Nothing it removes crosses into another file system (review S1): an object with a mount at
//! or under it ([`mount_inside`], the kernel's mount table, so a bind mount of the same device
//! too) or a directory on another device inside it is refused, naming it; [`remove_tree`]
//! checks each directory's device again as it goes, and stops at one that differs. A `build/` or
//! `std/` on another file system than its area (a symlink to another disk) is not listed: its
//! trash would be on another file system, where a rename cannot go (review C9).

use std::{
    alloc::Global,
    fs::Metadata,
    io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use blaze_hash::{HashMap, HashSet};

use crate::{
    lease::{self, Taken},
    plan::{Checkout, Class, Clock, Collector, Decision, Inventory, Object, Swept, TRASH},
    record,
};

/// The marker in an area's trash that says it is blaze's: without it nothing in the directory is
/// touched, and nothing is moved there.
pub const TRASH_MARK: &str = ".blaze-trash-of-blaze";
const TRASH_TEXT: &str = "blaze's trash: blaze's collector unlinks each entry named <20 digits>-<6 letters or digits>-<name>, and nothing else here\n";

/// An answer to a decision, for [`answer`].
#[derive(Debug)]
pub enum Answer {
    Scanned { run: u64, found: Result<Inventory, String> },
    Trashed { run: u64, path: String, swept: Swept },
    Unlinked { run: u64, path: String, done: Result<u64, String> },
}

/// What a decision done in the background checks just before it acts, as its run and the
/// roots stand then rather than when it was asked for: the daemon's jobs wait in the pool's
/// queue behind any other work.
#[derive(Clone, Debug)]
pub struct Gate {
    /// [`lease::uses`] as the collector asked for it: a lease taken in this process since (a
    /// build began) or a root changed, and a rename is not done ([`Swept::Rooted`]).
    pub uses: u64,
    /// The run's deadline, in ns since the Unix epoch, on the collector's clock.
    pub deadline_ns: u64,
    /// The collector's clock as the decision was asked for: the deadline is read against it,
    /// never against the raw wall clock, which a step (a VM paused, then NTP) would put past
    /// every deadline (review N3).
    pub clock: Clock,
    /// The last run that ended: a decision of it, or of one before, is not done.
    pub ended: Arc<AtomicU64>,
    /// The pressure its slice was paced for, permille: on its span.
    pub pressure: u32,
}

impl Gate {
    fn cancelled(&self, run: u64) -> bool {
        self.cancelled_at(run, record::now_ns(), record::boot_ns())
    }

    /// Whether a decision of `run` is not done, with the wall and boot clocks reading
    /// `wall_ns` and `boot_ns`: its run ended, or the collector's clock is past its deadline.
    pub fn cancelled_at(&self, run: u64, wall_ns: u64, boot_ns: Option<u64>) -> bool {
        self.ended.load(Ordering::SeqCst) >= run
            || self.clock.read(wall_ns, boot_ns) >= self.deadline_ns
    }
}

/// Hands `answer` to the planner.
pub fn answer(collector: &mut Collector, answer: Answer) {
    match answer {
        Answer::Scanned { run, found } => collector.scanned(run, found),
        Answer::Trashed { run, path, swept } => collector.trashed(run, &path, swept),
        Answer::Unlinked { run, path, done } => collector.unlinked(run, &path, done),
    }
}

/// Does `decision`, over the areas `areas` (the planner's [`crate::Config::areas`], an area
/// another collector holds given as `""`, [`crate::lease::collector_lock`]), behind `gate` when
/// one is given, its scan measuring through `sizes` and stopping by `clock`, the collector's
/// ([`Collector::clock`]). `None` for a [`Decision::Record`], which is its caller's to keep.
pub fn execute(
    decision: &Decision,
    areas: &[String],
    gate: Option<&Gate>,
    clock: &Clock,
    sizes: &mut Sizes,
) -> Option<Answer> {
    let cancelled = |run: u64| gate.is_some_and(|it| it.cancelled(run));
    match decision {
        Decision::Scan { run, after, dirs, until_ns } => {
            if cancelled(*run) {
                return Some(Answer::Scanned {
                    run: *run,
                    found: Err("its run ended before the scan began".to_owned()),
                });
            }
            let span = tracing::info_span!(
                target: "blaze",
                "gc scan",
                run,
                pressure = gate.map_or(0, |it| it.pressure),
                after = after.as_deref().unwrap_or(""),
                dirs,
                objects = tracing::field::Empty,
                bytes = tracing::field::Empty,
            );
            let _entered = span.enter();
            let found = scan(areas, after.as_deref(), *dirs, *until_ns, clock, sizes);
            if let Ok(found) = &found {
                span.record("objects", found.objects.len());
                span.record("bytes", found.objects.iter().map(|it| it.bytes).sum::<u64>());
            }
            Some(Answer::Scanned { run: *run, found })
        }
        Decision::Trash { run, path, to, class } => {
            let pressure = gate.map_or(0, |it| it.pressure);
            let _span = tracing::info_span!(target: "blaze", "gc sweep", run, pressure, path = %path, to = %to).entered();
            let swept = trash(Path::new(path), Path::new(to), *class, gate.map(|it| (it, *run)));
            // clone: the answer's own path, once a rename
            Some(Answer::Trashed { run: *run, path: path.clone(), swept })
        }
        Decision::Unlink { run, path } => {
            let pressure = gate.map_or(0, |it| it.pressure);
            let span = tracing::info_span!(target: "blaze", "gc unlink", run, pressure, path = %path, bytes = tracing::field::Empty).entered();
            let done = match cancelled(*run) {
                true => {
                    Err("its run ended before the unlink began; the next run unlinks it".to_owned())
                }
                false => unlink(Path::new(path), Vec::new()),
            };
            if let Ok(bytes) = &done {
                span.record("bytes", bytes);
            }
            // clone: the answer's own path, once an unlink
            Some(Answer::Unlinked { run: *run, path: path.clone(), done })
        }
        Decision::Record { .. } => None,
    }
}

/// Runs one collection to its end on this thread: asked (a dry run plans and removes
/// nothing), each decision done in turn, until the run's report, over `areas` (the
/// collector's, an area another collector holds given as `""`). Bounded: the planner asks for
/// at most its fuel in renames and unlinks, and this stops after that many rounds and a few
/// more, naming how far it got.
pub fn run_now(
    collector: &mut Collector,
    dry_run: bool,
    areas: &[String],
) -> Result<crate::RunReport, String> {
    collector.advance_clocks(record::now_ns(), record::boot_ns());
    // An area that does not measure is named in the report; the rest are collected (review N12).
    measure(collector);
    collector.ask(dry_run);
    let mut sizes = Sizes::new();
    let bound = u64::from(collector.config().fuel) * 2 + 16;
    for _ in 0..bound {
        collector.advance_clocks(record::now_ns(), record::boot_ns());
        let decisions = collector.take_decisions();
        if decisions.is_empty() && !collector.running() {
            break;
        }
        for decision in &decisions {
            if let Decision::Record { report } = decision {
                // clone: the report handed out, once a run
                return Ok(report.clone());
            }
            // On this thread at once: nothing waits between the ask and the act.
            let clock = collector.clock();
            if let Some(it) = execute(decision, areas, None, &clock, &mut sizes) {
                answer(collector, it);
            }
        }
    }
    Err(format!(
        "the collection asked for more than the {bound} rounds its fuel allows and did not end"
    ))
}

/// Each area's volume measured into `collector` ([`crate::space::volume`]); one that does not
/// measure is noted in the next report, naming the area and why, and the others are measured
/// all the same (review N12: one snapshot mount's error ended `blaze cache gc` whole). What was
/// noted is returned too.
pub fn measure(collector: &mut Collector) -> Vec<String> {
    // clone: the areas, once a measure
    let areas = collector.config().areas.clone();
    // alloc: a measure's problems, rarely any
    let mut problems = Vec::new();
    for (at, area) in areas.iter().enumerate() {
        match crate::space::volume(Path::new(area)) {
            Ok(volume) => collector.volume(at, volume),
            Err(why) => {
                let why = format!(
                    "the volume of {area} was not measured, so its free space does not pace this run: {why}"
                );
                collector.note(why.clone()); // clone: the report's copy and the caller's
                problems.push(why);
            }
        }
    }
    problems
}

/// The objects of the areas, sorted by path: every trash entry, and the build dirs and stores
/// (units, in path order) past `after`, at most `dirs` of them, and none past the one being
/// walked once `clock` (the collector's, [`Collector::clock`]) reaches `until_ns`; [`Inventory::next`] is the last unit walked when
/// more remain. An area, or its `build/`, `std/` or trash, on a network file system fails the
/// scan, naming it. Sizes come through `sizes`, measured again only for what changed.
pub fn scan(
    areas: &[String],
    after: Option<&str>,
    dirs_at_most: u32,
    until_ns: Option<u64>,
    clock: &Clock,
    sizes: &mut Sizes,
) -> Result<Inventory, String> {
    let mut inventory = Inventory::default();
    let last = areas.len().saturating_sub(1);
    let mounts = crate::netfs::mounts()?;
    let now = record::now_ns();
    sizes.prune(now);
    // alloc: a scan, one a unit
    let mut units: Vec<(String, u16, Class)> = Vec::new();
    for (at, root) in areas.iter().enumerate() {
        let area = u16::try_from(at).map_err(|_| "more than 65536 areas".to_owned())?;
        // An area another collector holds is given as "": nothing of it is listed.
        if root.is_empty() {
            continue;
        }
        let root = Path::new(root);
        if !root.exists() {
            continue;
        }
        check_local(root)?;
        let mut listed =
            |sub: &str, class: Class, inventory: &mut Inventory| -> Result<(), String> {
                let dir = root.join(sub);
                if dir.exists() {
                    check_local(&dir)?;
                    // Its trash is the area's: a `build/` or `std/` on another file system (a
                    // symlink to another disk) could not be renamed into it, nor measured as the
                    // area's volume.
                    if let Some(why) = other_device(root, &dir)? {
                        inventory.problems.push(why);
                        return Ok(());
                    }
                }
                for it in dirs(&dir, &mut inventory.problems) {
                    units.push((text(&it), area, class));
                }
                Ok(())
            };
        if at == 0 {
            listed("build", Class::Workspace, &mut inventory)?;
        }
        if at == last {
            listed("std", Class::Std, &mut inventory)?;
        }
        if root.join(TRASH).exists() {
            check_local(&root.join(TRASH))?;
        }
        trash_entries(area, root, &mounts, &mut inventory);
    }
    units.sort();
    units.dedup_by(|a, b| a.0 == b.0);
    let start = after.map_or(0, |after| units.partition_point(|it| it.0.as_str() <= after));
    let most = start.saturating_add(dirs_at_most as usize).min(units.len());
    let mut end = start;
    for (path, area, class) in &units[start..most] {
        match class {
            Class::Std => std_store(*area, Path::new(path), &mounts, sizes, &mut inventory),
            _ => workspace(*area, Path::new(path), &mounts, sizes, &mut inventory),
        }
        end += 1;
        // A slice's scan stops at its time, after at least one unit: the cursor names where
        // the next goes on (review C2).
        if until_ns.is_some_and(|until| clock.now() >= until) {
            break;
        }
    }
    // clone: the cursor's own path, once a scan
    inventory.next = (end > start && end < units.len()).then(|| units[end - 1].0.clone());
    inventory.objects.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(inventory)
}

fn std_store(
    area: u16,
    dir: &Path,
    mounts: &[PathBuf],
    sizes: &mut Sizes,
    inventory: &mut Inventory,
) {
    let mut refused = (!lease::has_lease(dir)).then(|| no_lease(dir));
    if refused.is_none() {
        refused = mount_in(mounts, dir);
    }
    let used_ns = match record::read(dir) {
        Ok(Some(it)) => it.used_ns,
        Ok(None) => modified_ns(dir),
        Err(why) => {
            inventory.problems.push(why);
            return;
        }
    };
    let leased = refused.is_none() && probe(dir, &mut inventory.problems);
    let bytes = match sizes.of(dir, &mut inventory.problems) {
        Ok(bytes) => bytes,
        Err(why) => {
            refused = refused.or(Some(why));
            0
        }
    };
    inventory.objects.push(object(area, dir, Class::Std, bytes, used_ns, leased, refused));
}

/// The entries of `root`'s trash blaze put there; none when the trash is not blaze's.
fn trash_entries(area: u16, root: &Path, mounts: &[PathBuf], inventory: &mut Inventory) {
    let trash = root.join(TRASH);
    if !trash.exists() {
        return;
    }
    if !trash.join(TRASH_MARK).is_file() {
        inventory.problems.push(format!(
            "{} has no {TRASH_MARK}: it is not blaze's trash, and nothing in it is touched",
            trash.display()
        ));
        return;
    }
    let mut foreign = 0u32;
    for entry in entries(&trash, &mut inventory.problems) {
        let name =
            entry.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
        if !is_trash_name(&name) {
            foreign += u32::from(name != TRASH_MARK);
            continue;
        }
        // `<moved ns, 20 digits>-<seq>-<name>`: when it was moved there.
        let used_ns = name[..20].parse().unwrap_or_else(|_| modified_ns(&entry));
        // Trash is unlinked whatever its age says: one with a mount inside is left, named.
        let bytes = match mount_in(mounts, &entry)
            .map_or_else(|| size(&entry, &mut inventory.problems), Err)
        {
            Ok(bytes) => bytes,
            Err(why) => {
                inventory.problems.push(format!("{} is left in trash: {why}", entry.display()));
                continue;
            }
        };
        inventory.objects.push(object(area, &entry, Class::Trash, bytes, used_ns, false, None));
    }
    if foreign > 0 {
        inventory.problems.push(format!(
            "{foreign} entries of {} are not named as blaze names trash: left alone",
            trash.display()
        ));
    }
}

/// Whether `name` is one blaze gives a trash entry: `<20 digits>-<6 letters or digits>-<name>`,
/// the name of letters, digits, `-`, `_` and `.`.
pub fn is_trash_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 28
        && bytes[..20].iter().all(u8::is_ascii_digit)
        && bytes[20] == b'-'
        && bytes[21..27].iter().all(u8::is_ascii_alphanumeric)
        && bytes[27] == b'-'
        && bytes[28..]
            .iter()
            .all(|it| it.is_ascii_alphanumeric() || matches!(it, b'-' | b'_' | b'.'))
}

/// `area`'s trash, made with blaze's marker when it is not there; refused, naming it, when it
/// is there without the marker (a directory someone else made). Made whole: a temporary
/// directory with the marker in it, renamed into place, so no one sees the trash without its
/// marker (review C7); a maker that loses the race to another takes the other's.
pub fn trash_dir(area: &Path) -> Result<PathBuf, String> {
    let dir = area.join(TRASH);
    let mark = dir.join(TRASH_MARK);
    if mark.is_file() {
        return Ok(dir);
    }
    if dir.exists() {
        return Err(format!(
            "{} is there without {TRASH_MARK}: it is not blaze's trash, so nothing is moved there",
            dir.display()
        ));
    }
    std::fs::create_dir_all(area).map_err(|error| format!("making {}: {error}", area.display()))?;
    let temporary = area.join(record::temporary_name(TRASH));
    std::fs::create_dir(&temporary)
        .map_err(|error| format!("making {}: {error}", temporary.display()))?;
    let made =
        record::write_whole(&temporary.join(TRASH_MARK), TRASH_TEXT.as_bytes()).and_then(|()| {
            std::fs::rename(&temporary, &dir).map_err(|error| {
                format!("renaming {} to {}: {error}", temporary.display(), dir.display())
            })
        });
    if made.is_err() {
        _ = std::fs::remove_file(temporary.join(TRASH_MARK));
        _ = std::fs::remove_dir(&temporary);
        // Another maker's, whole, is as good.
        if mark.is_file() {
            return Ok(dir);
        }
    }
    made.map(|()| dir)
}

/// Refuses `path` on a network file system: NFS, SMB and their kind emulate `flock` with
/// locks a process loses on any close, so a lease would not hold.
pub fn check_local(path: &Path) -> Result<(), String> {
    match crate::netfs::network(path)? {
        None => Ok(()),
        Some(kind) => Err(format!(
            "{} is on a network file system ({kind}): blaze's leases do not hold there, so it collects nothing in it",
            path.display()
        )),
    }
}

fn workspace(
    area: u16,
    dir: &Path,
    mounts: &[PathBuf],
    sizes: &mut Sizes,
    inventory: &mut Inventory,
) {
    let problems = &mut inventory.problems;
    let read = record::read(dir);
    let record = read.as_ref().ok().and_then(Option::as_ref);
    let checkout = record.and_then(|it| it.checkout.as_deref()).map(checkout_of);
    let refused = if !lease::has_lease(dir) {
        Some(no_lease(dir))
    } else {
        match (&read, &checkout) {
            (Ok(Some(_)), Some(Ok(_))) => None,
            (Ok(Some(_)), Some(Err(why))) => Some(why.clone()),
            (Ok(Some(_)), None) => Some("its record names no checkout".to_owned()),
            (Ok(None), _) => Some("it has no record of its checkout and last use".to_owned()),
            (Err(why), _) => Some(why.clone()),
        }
    };
    // A mount anywhere in it refuses it whole, and so its profiles.
    let refused = refused.or_else(|| mount_in(mounts, dir));
    let used_ns = record.map_or_else(|| modified_ns(dir), |it| it.used_ns);
    let checkout = checkout.and_then(Result::ok);
    let leased = refused.is_none() && probe(dir, problems);
    let mut own = 0u64;
    for entry in entries(dir, problems) {
        let Ok(meta) = std::fs::symlink_metadata(&entry) else { continue };
        let name =
            entry.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
        let mut refused = refused.as_ref().map(|_| "its workspace is refused".to_owned());
        if meta.is_dir() {
            let bytes = match sizes.of(&entry, problems) {
                Ok(bytes) => bytes,
                Err(why) => {
                    refused = refused.or(Some(why));
                    0
                }
            };
            let recorded = record.and_then(|it| record::profile_used(it, &name)).unwrap_or(0);
            let used_ns = newest_ns(&entry).max(recorded);
            let mut profile = object(area, &entry, Class::Profile, bytes, used_ns, leased, refused);
            profile.workspace = Some(text(dir));
            // A head that builds the checkout keeps its profiles too (review C4).
            profile.checkout = checkout.clone(); // clone: each profile names its checkout
            inventory.objects.push(profile);
        } else if name.ends_with(".tmp") {
            // A temporary of a workspace in use may be one a build writes now: it stays, and it
            // is renamed only under its workspace's lease.
            let mut temp = object(
                area,
                &entry,
                Class::Temp,
                disk_bytes(&meta),
                modified_ns(&entry),
                leased,
                refused,
            );
            temp.workspace = Some(text(dir));
            temp.checkout = checkout.clone(); // clone: each temporary names its checkout
            inventory.objects.push(temp);
        } else {
            own += disk_bytes(&meta);
        }
    }
    let mut it = object(area, dir, Class::Workspace, own, used_ns, leased, refused);
    it.checkout = checkout;
    inventory.objects.push(it);
}

/// What a record's checkout is now: there, gone (not found, and its parent is there), or a
/// refusal naming why its state cannot be told (an unmounted volume, a permission, a file).
fn checkout_of(path: &str) -> Result<Checkout, String> {
    let exists = match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => true,
        Ok(_) => return Err(format!("its checkout {path} is no directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = Path::new(path).parent();
            match parent.map(std::fs::metadata) {
                Some(Ok(meta)) if meta.is_dir() => false,
                _ => {
                    return Err(format!(
                        "its checkout {path} is not found, nor is the directory it was in"
                    ));
                }
            }
        }
        Err(error) => return Err(format!("its checkout {path} cannot be read: {error}")),
    };
    Ok(Checkout { path: path.to_owned(), exists })
}

fn no_lease(dir: &Path) -> String {
    format!(
        "it has no {}: no blaze that takes leases used it, so nothing says when it is in use",
        lease::lease_of(dir).display()
    )
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn object(
    area: u16,
    path: &Path,
    class: Class,
    bytes: u64,
    used_ns: u64,
    leased: bool,
    refused: Option<String>,
) -> Object {
    Object {
        path: text(path),
        area,
        class,
        bytes,
        used_ns,
        workspace: None,
        checkout: None,
        leased,
        refused,
    }
}

/// Whether `dir`'s lease is held now; a probe that fails is a problem, and the dir counts as
/// held.
fn probe(dir: &Path, problems: &mut Vec<String>) -> bool {
    match lease::held(dir) {
        Ok(held) => held,
        Err(why) => {
            problems.push(why);
            true
        }
    }
}

/// The directories in `dir`, sorted; none when it is not there.
fn dirs(dir: &Path, problems: &mut Vec<String>) -> Vec<PathBuf> {
    let mut found = entries(dir, problems);
    found.retain(|it| std::fs::symlink_metadata(it).is_ok_and(|meta| meta.is_dir()));
    found
}

/// The entries of `dir`, sorted; none when it is not there.
fn entries(dir: &Path, problems: &mut Vec<String>) -> Vec<PathBuf> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            problems.push(format!("listing {}: {error}", dir.display()));
            return Vec::new();
        }
    };
    let mut found = Vec::new();
    for entry in read {
        match entry {
            Ok(entry) => found.push(entry.path()),
            Err(error) => problems.push(format!("listing {}: {error}", dir.display())),
        }
    }
    found.sort();
    found
}

fn modified_ns(path: &Path) -> u64 {
    std::fs::symlink_metadata(path)
        .and_then(|it| it.modified())
        .ok()
        .and_then(|it| it.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX))
}

/// The newest modification time of `dir` and the directories directly in it (`deps`,
/// `build`, ...), which a build writes into.
fn newest_ns(dir: &Path) -> u64 {
    let mut newest = modified_ns(dir);
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            newest = newest.max(modified_ns(&entry.path()));
        }
    }
    newest
}

/// Bytes on disk under `path` (allocated blocks, so a sparse or reflinked file counts what it
/// holds; a file with more than one link once), symlinks not followed. Refuses, naming it, a
/// directory inside on another device than `path`'s: a mount, which nothing of blaze's removes.
fn size(path: &Path, problems: &mut Vec<String>) -> Result<u64, String> {
    let root = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    let device = device_of(&root);
    // alloc: a walk, one a file with more than one link
    let mut linked: HashSet<u64, Global> = HashSet::new_in(Global);
    let mut bytes = 0u64;
    // alloc: a walk, its directories to go
    let mut todo = vec![path.to_path_buf()];
    while let Some(at) = todo.pop() {
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                problems.push(format!("reading {}: {error}", at.display()));
                continue;
            }
        };
        if device_of(&meta) != device {
            return Err(foreign(&at, path));
        }
        if !meta.is_dir() {
            if links_of(&meta) > 1 && !linked.insert(inode_of(&meta)) {
                continue;
            }
            bytes = bytes.saturating_add(disk_bytes(&meta));
            continue;
        }
        bytes = bytes.saturating_add(disk_bytes(&meta));
        match std::fs::read_dir(&at) {
            Ok(read) => todo.extend(read.flatten().map(|it| it.path())),
            Err(error) => problems.push(format!("listing {}: {error}", at.display())),
        }
    }
    Ok(bytes)
}

fn foreign(at: &Path, path: &Path) -> String {
    format!(
        "{} is on another file system than {} (a mount inside it): nothing under {} is removed",
        at.display(),
        path.display(),
        path.display()
    )
}

#[cfg(unix)]
fn device_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.dev()
}

/// Windows: a walk never enters a reparse point (a junction, a mounted volume), which
/// `symlink_metadata` reports as a link, so every directory it enters is on the root's volume.
#[cfg(not(unix))]
fn device_of(_meta: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn inode_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_meta: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn links_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.nlink()
}

#[cfg(not(unix))]
fn links_of(_meta: &Metadata) -> u64 {
    1
}

/// What a file holds on disk: its blocks on unix, its length elsewhere.
#[cfg(unix)]
fn disk_bytes(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn disk_bytes(meta: &Metadata) -> u64 {
    meta.len()
}

/// Why `sub` (an area's `build/` or `std/`) is not listed: on another device than its area.
fn other_device(area: &Path, sub: &Path) -> Result<Option<String>, String> {
    let device = |path: &Path| {
        std::fs::metadata(path)
            .map(|it| device_of(&it))
            .map_err(|error| format!("reading {}: {error}", path.display()))
    };
    if device(area)? == device(sub)? {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} is on another file system than its area {}, whose trash it would be moved to: nothing in it is collected (move the whole root instead of linking {} elsewhere)",
        sub.display(),
        area.display(),
        sub.display()
    )))
}

/// Profile and store sizes from earlier scans, by path: a directory whose inode and newest
/// stamp (its own and its children's modification times) are as they were, measured less than
/// [`Sizes::FRESH_NS`] ago, is not walked again (review P3). Sizes only pace eviction; what is
/// removed is checked by its lease and its device as it goes, never by a size kept here. The
/// driver's own, kept between scans (the daemon moves it into each scan's job and back), never
/// shared.
pub struct Sizes {
    known: HashMap<String, Measured, Global>,
}

struct Measured {
    inode: u64,
    stamp: u64,
    bytes: u64,
    at_ns: u64,
}

impl Default for Sizes {
    fn default() -> Sizes {
        Sizes::new()
    }
}

impl Sizes {
    /// A size measured this long ago is measured again whatever its stamps say: a file
    /// rewritten in place changes no directory's stamp.
    pub const FRESH_NS: u64 = 15 * 60 * 1_000_000_000;
    /// One not looked at this long is dropped: its directory is gone or no scan reaches it.
    pub const KEPT_NS: u64 = 60 * 60 * 1_000_000_000;

    pub fn new() -> Sizes {
        Sizes { known: HashMap::new_in(Global) }
    }

    /// The bytes under `dir` ([`size`]), from the last measure when nothing says it changed.
    fn of(&mut self, dir: &Path, problems: &mut Vec<String>) -> Result<u64, String> {
        let now = record::now_ns();
        let inode = std::fs::symlink_metadata(dir).map(|it| inode_of(&it)).unwrap_or(0);
        let stamp = newest_ns(dir);
        let key = text(dir);
        if let Some(it) = self.known.get_mut(&key)
            && it.inode == inode
            && it.stamp == stamp
            && now.saturating_sub(it.at_ns) < Sizes::FRESH_NS
        {
            return Ok(it.bytes);
        }
        let bytes = size(dir, problems);
        match &bytes {
            // alloc: one entry a directory measured
            Ok(bytes) => {
                _ = self.known.insert(key, Measured { inode, stamp, bytes: *bytes, at_ns: now })
            }
            Err(_) => _ = self.known.remove(&key),
        }
        bytes
    }

    fn prune(&mut self, now: u64) {
        self.known.retain(|_, it| now.saturating_sub(it.at_ns) < Sizes::KEPT_NS);
    }
}

/// Why `path` is refused for a mount at or under it, from `mounts` ([`crate::netfs::mounts`]).
fn mount_in(mounts: &[PathBuf], path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    mounts.iter().find(|it| it.starts_with(&canonical)).map(|it| {
        format!(
            "{} is a mount point at or under it: nothing of blaze's removes across a mount",
            it.display()
        )
    })
}

/// The mount point at or under `path` (the kernel's mount table, so a bind mount on the same
/// device too), if any: nothing of blaze's removes a directory that holds one.
pub fn mount_inside(path: &Path) -> Result<Option<PathBuf>, String> {
    let canonical = match std::fs::canonicalize(path) {
        Ok(it) => it,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("resolving {}: {error}", path.display())),
    };
    Ok(crate::netfs::mounts()?.into_iter().find(|it| it.starts_with(&canonical)))
}

/// Removes `path` and everything under it, files first, then each directory once empty, never
/// following a link and never leaving `path`'s file system: refused, naming it, when a mount is
/// at or under `path`, and stopped, naming it, at a directory on another device found as it
/// goes (one mounted since). The bytes on disk it removed. `blaze clean`'s and the collector's
/// one way to remove a tree (review S1).
pub fn remove_tree(path: &Path) -> Result<u64, String> {
    let root = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    if !root.is_dir() {
        return remove_entry(path, &root).map(|()| disk_bytes(&root));
    }
    if let Some(mount) = mount_inside(path)? {
        return Err(format!(
            "{} is a mount point at or under {}: it is not removed",
            mount.display(),
            path.display()
        ));
    }
    let device = device_of(&root);
    let failed = |at: &Path, error: io::Error| format!("removing {}: {error}", at.display());
    let mut freed = 0u64;
    // alloc: a removal, its directories to go; each is visited, then removed once emptied
    let mut todo: Vec<(PathBuf, bool)> = vec![(path.to_path_buf(), false)];
    while let Some((dir, emptied)) = todo.pop() {
        if emptied {
            match std::fs::remove_dir(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(failed(&dir, error)),
            }
            continue;
        }
        let meta = match std::fs::symlink_metadata(&dir) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(failed(&dir, error)),
        };
        if device_of(&meta) != device {
            return Err(foreign(&dir, path));
        }
        freed = freed.saturating_add(disk_bytes(&meta));
        todo.push((dir.clone(), true)); // clone: the directory, removed once emptied
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(failed(&dir, error)),
        };
        for entry in read {
            let entry = entry.map_err(|error| failed(&dir, error))?.path();
            let meta = match std::fs::symlink_metadata(&entry) {
                Ok(meta) => meta,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(failed(&entry, error)),
            };
            if meta.is_dir() {
                todo.push((entry, false));
                continue;
            }
            remove_entry(&entry, &meta)?;
            freed = freed.saturating_add(disk_bytes(&meta));
        }
    }
    Ok(freed)
}

/// Removes one entry that is no directory: a file, or a link (a directory link on Windows is
/// removed as a directory, never followed).
fn remove_entry(path: &Path, meta: &Metadata) -> Result<(), String> {
    #[cfg(windows)]
    let removed = {
        use std::os::windows::fs::FileTypeExt as _;
        if meta.file_type().is_symlink_dir() {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        }
    };
    #[cfg(not(windows))]
    let removed = {
        _ = meta;
        std::fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("removing {}: {error}", path.display())),
    }
}

/// The directories no removal (`blaze clean`) and no mark (a build dir's lease, record or
/// `CACHEDIR.TAG`, which make a directory `blaze clean` and the collector would take) may touch:
/// one list for both (review N13). A directory is refused when it is or holds one of
/// [`Protected::holds`] (`/`, `$HOME`, `CARGO_HOME`, `RUSTUP_HOME`, blaze's root, its directory of
/// build directories `<root>/build`, the machine's shared directory, and whatever the caller
/// adds: the checkout, the current directory), or is or is inside one of [`Protected::within`]
/// (`CARGO_HOME`, `RUSTUP_HOME`, the daemon's state `<root>/v2`, the std stores
/// `<machine>/std`): a `CACHEDIR.TAG` directory such as `~/.cargo/git` named as a target dir is
/// never removed, and `<root>/build` named as one is never made one build's (review round 5,
/// B1: its removal would take every workspace's build dir, leased or not). Compared resolved
/// ([`resolve`]) and, on unix, by device and inode too (a bind mount, a second name). A path of
/// the list that cannot be resolved (a relative `CARGO_HOME`) is an error naming it, never a
/// path that protects nothing.
#[derive(Clone, Debug, Default)]
pub struct Protected {
    pub holds: Vec<PathBuf>,
    pub within: Vec<PathBuf>,
}

/// The name of blaze's directory of build directories under its root (`blaze_manifest::home::builds`).
pub const BUILDS: &str = "build";

impl Protected {
    /// The list for blaze's root `root` and the machine's shared directory `machine`, with
    /// `home`, `cargo_home` and `rustup_home` as given (`None`: not known). Each must resolve:
    /// a relative one is refused, naming it and what it is.
    pub fn new(
        home: Option<PathBuf>,
        cargo_home: Option<PathBuf>,
        rustup_home: Option<PathBuf>,
        root: &Path,
        machine: Option<&Path>,
    ) -> Result<Protected, String> {
        let real = |what: &str, path: &Path| {
            resolve(path).map_err(|why| format!("{what} {}: {why}", path.display()))
        };
        // alloc: once per check, a few paths
        let mut it = Protected::default();
        it.holds.push(PathBuf::from("/"));
        if let Some(home) = home {
            it.holds.push(real("$HOME", &home)?);
        }
        for (what, path) in [("CARGO_HOME", cargo_home), ("RUSTUP_HOME", rustup_home)] {
            if let Some(path) = path {
                let path = real(what, &path)?;
                it.holds.push(path.clone()); // clone: one path in each list
                it.within.push(path);
            }
        }
        it.holds.push(real("blaze's root", root)?);
        // Nothing may be it or hold it: a target dir named `<root>/build` would make every
        // workspace's build dir part of one build's (review round 5, B1).
        it.holds.push(real("blaze's build directories", &root.join(BUILDS))?);
        it.within.push(real("the daemon's state", &root.join("v2"))?);
        // The packages the daemon fetched and mounts (`blaze_manifest::home::sources`): no
        // target dir inside them, nothing a clean deletes there.
        it.within.push(real("blaze's fetched sources", &root.join("sources"))?);
        if let Some(machine) = machine {
            it.holds.push(real("the machine's shared directory", machine)?);
            it.within.push(real("the std stores", &machine.join("std"))?);
        }
        Ok(it)
    }

    /// [`Protected::new`] with this process's `$HOME`, `CARGO_HOME` and `RUSTUP_HOME`. A relative
    /// one is taken against the current directory, as cargo and rustup take it (review round 6,
    /// C4); a current directory that cannot be read is an error naming it. When `CARGO_HOME` or
    /// `RUSTUP_HOME` is unset its default (`~/.cargo`, `~/.rustup`) is used, unless that default
    /// cannot be searched (`PermissionDenied`: another user's `HOME`), when it protects nothing
    /// and nothing is said (review round 7, C2); a set one is always protected. The daemon makes
    /// it once, at its start, and its start fails naming the error (review round 6, C5); `blaze
    /// clean` makes it once a run.
    pub fn of_env(root: &Path, machine: Option<&Path>) -> Result<Protected, String> {
        let absolute = |what: &str, path: PathBuf| {
            std::path::absolute(&path).map_err(|error| {
                format!("{what} {} against the current directory: {error}", path.display())
            })
        };
        let home = std::env::home_dir().map(|it| absolute("$HOME", it)).transpose()?;
        let set =
            |name: &str| std::env::var_os(name).filter(|it| !it.is_empty()).map(PathBuf::from);
        // A default home that cannot be searched (`HOME` another user's, `EACCES`): nothing in
        // it is reachable by this process, so no target dir it names can be in it, and there is
        // nothing to protect; nothing is said. A set one is always protected (review round 7, C2).
        let reachable = |it: &PathBuf| !matches!(std::fs::symlink_metadata(it), Err(error) if error.kind() == io::ErrorKind::PermissionDenied);
        let cargo_home = match set("CARGO_HOME") {
            Some(path) => Some(absolute("CARGO_HOME", path)?),
            None => home.as_ref().map(|it| it.join(".cargo")).filter(reachable),
        };
        let rustup_home = match set("RUSTUP_HOME") {
            Some(path) => Some(absolute("RUSTUP_HOME", path)?),
            // blaze never reads RUSTUP_HOME (own-nightly): it is protected only so a target dir
            // never takes a rustup's toolchains. The default one that cannot be searched (`HOME`
            // of another user, `EACCES`) holds nothing this process could reach, so nothing
            // there to protect, and nothing is said.
            None => home.as_ref().map(|it| it.join(".rustup")).filter(reachable),
        };
        Protected::new(home, cargo_home, rustup_home, root, machine)
    }

    /// `path` added to what may not be removed or held; a path that cannot be resolved is an
    /// error naming it.
    pub fn hold(&mut self, path: &Path) -> Result<(), String> {
        let resolved = resolve(path)
            .map_err(|why| format!("{} is not checked against: {why}", path.display()))?;
        self.holds.push(resolved);
        Ok(())
    }

    /// Why `dir` may not be removed or marked, naming what it is, holds or is inside; `None`
    /// when it may. `dir` is resolved here.
    pub fn refuses(&self, dir: &Path) -> Result<Option<String>, String> {
        let resolved = resolve(dir)?;
        let same_or_inside = |inner: &Path, outer: &Path| {
            inner.starts_with(outer) || inner.ancestors().any(|it| same_dir(it, outer))
        };
        if let Some(kept) = self.holds.iter().find(|kept| same_or_inside(kept, &resolved)) {
            return Ok(Some(format!(
                "{} ({}) is or holds {}, which blaze never removes or marks",
                dir.display(),
                resolved.display(),
                kept.display()
            )));
        }
        if let Some(kept) = self.within.iter().find(|kept| same_or_inside(&resolved, kept)) {
            return Ok(Some(format!(
                "{} ({}) is or is inside {}, which blaze never removes or marks",
                dir.display(),
                resolved.display(),
                kept.display()
            )));
        }
        Ok(None)
    }
}

/// Whether `a` and `b` are one directory: the same device and inode on unix; elsewhere (or
/// when either is not there) the same path.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => a == b,
        }
    }
    #[cfg(not(unix))]
    {
        a == b
    }
}

/// `path` with every symlink in it resolved, though it may not be there yet: its nearest
/// ancestor that is there, canonical, and the rest as given. Refused, naming it, when it is
/// relative or the rest holds `..`, which a directory not made yet cannot resolve (review C7).
pub fn resolve(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    // alloc: the parts not there yet, once a resolve
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut at = path;
    loop {
        match std::fs::canonicalize(at) {
            Ok(mut resolved) => {
                for part in rest.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("resolving {}: {error}", at.display())),
        }
        match at.components().next_back() {
            Some(Component::Normal(name)) => rest.push(name),
            Some(Component::CurDir) => {}
            _ => {
                return Err(format!(
                    "{} has `..` past the part of it that is there: it cannot be resolved before it is made",
                    path.display()
                ));
            }
        }
        at = at.parent().ok_or_else(|| format!("no part of {} is there", path.display()))?;
    }
}

/// Where `path`'s lease is: its own for a workspace or a standard library's store, its
/// workspace's for a profile or a temporary (every user of a workspace holds the workspace's),
/// none for trash.
fn lease_dir(path: &Path, class: Class) -> Option<&Path> {
    match class {
        Class::Workspace | Class::Std => Some(path),
        Class::Profile | Class::Temp => path.parent(),
        Class::Trash => None,
    }
}

/// Renames `path` to `to` (in its area's trash, [`trash_dir`]) while holding its lease
/// exclusively, verified to be its own; with `gate`, only when its run is still on and no lease
/// was taken in this process since the rename was asked for, checked while the lease is held.
pub fn trash(path: &Path, to: &Path, class: Class, gate: Option<(&Gate, u64)>) -> Swept {
    trash_between(path, to, class, gate, &mut |_| {})
}

/// The points of [`trash_between`] a test acts at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The walk for the leases inside is done; the dir's own lock is not taken yet: the window
    /// a build in another process may take a lease inside in (review round 7, C1).
    Walked,
    /// The tree is renamed into trash; the walk of the renamed tree has not begun.
    Renamed,
}

/// [`trash`], with `at` run at each [`Step`] (review round 8, C1 and M2).
pub(crate) fn trash_between(
    path: &Path,
    to: &Path,
    class: Class,
    gate: Option<(&Gate, u64)>,
    at: &mut dyn FnMut(Step),
) -> Swept {
    let Some(dir) = lease_dir(path, class) else {
        return Swept::Failed(format!(
            "{} is trash already: it is unlinked, never moved",
            path.display()
        ));
    };
    // The leases inside, found before any lock is taken (review round 6, P1); one made since is
    // found by the unlink's walk ([`unlink`]).
    let inside = match nested_lease_dirs(path, Some(dir)) {
        Ok(dirs) => dirs,
        Err(why) => return Swept::Failed(why.to_string()),
    };
    at(Step::Walked);
    let held = match lease::take_exclusive(dir) {
        Ok(Taken::Exclusive(file)) => file,
        Ok(Taken::Held) => return Swept::Leased,
        Ok(Taken::Moved) if !dir.exists() => return Swept::Gone,
        // What is at the path now is another directory than the one whose lease was locked.
        Ok(Taken::Moved) => return Swept::Leased,
        Ok(Taken::Missing) if !path.exists() => return Swept::Gone,
        // The scan found one: a lease gone since is no lease to trust.
        Ok(Taken::Missing) => {
            return Swept::Failed(format!("{} is gone", lease::lease_of(dir).display()));
        }
        Err(why) => return Swept::Failed(why),
    };
    // Another build's directory inside it (a `--target-dir` named inside this one): its lease
    // is taken too, and held through the rename, or nothing moves (review round 5, B1).
    // alloc: a rename's locks of the tree, a few
    let mut nested = vec![held];
    match take_leases(path, &inside, &mut nested) {
        Ok(()) => {}
        Err(Nested::Held { dir, .. }) => return Swept::NestedLeased { dir: text(&dir) },
        Err(other) => return Swept::Failed(other.to_string()),
    }
    if let Some((gate, run)) = gate {
        if gate.cancelled(run) {
            return Swept::Cancelled;
        }
        if lease::uses() != gate.uses {
            return Swept::Rooted;
        }
    }
    // A mount made inside it since the scan: it is not moved, so never unlinked across.
    match mount_inside(path) {
        Ok(None) => {}
        Ok(Some(mount)) => {
            return Swept::Failed(format!(
                "{} is a mount point at or under {}: it is not moved to trash",
                mount.display(),
                path.display()
            ));
        }
        Err(why) => return Swept::Failed(why),
    }
    let swept = rename_into_trash(path, to);
    // A build that took a lease inside between the walk and the lock: the tree goes back
    // (review round 7, C1).
    let swept = match swept {
        Swept::Moved => {
            at(Step::Renamed);
            match recheck_renamed(path, to, &mut nested) {
                Ok(()) => Swept::Moved,
                Err(Nested::Held { dir, .. }) => Swept::NestedLeased { dir: text(&dir) },
                // In trash, not moved back (review round 8, M1).
                Err(stuck @ Nested::NotBack { .. }) => {
                    Swept::LeftInTrash { why: stuck.to_string() }
                }
                Err(other) => Swept::Failed(other.to_string()),
            }
        }
        other => other,
    };
    // The locks end here, after the rename: no user had the directory, or one inside it that
    // the walk found, at any point of it.
    drop(nested);
    swept
}

/// After `from` was renamed to `to` with `held` locked (its own lease and those a walk found
/// inside): the renamed tree walked again, every lease in it not held yet taken into `held`.
/// A lease a build took between the first walk and the lock (another process's: nothing orders
/// a nested build's lease after the outer lock) is held, so the tree is renamed back to `from`
/// at once, and the error names that dir at its own path (`Nested::Held { tree: from, .. }`).
/// The build saw its directory gone from the rename to the rename back, that is for the walk of
/// the renamed tree: as long as the walk of a big tree takes, not an instant. Nothing of it is
/// unlinked. When the rename back fails too (`from` made again meanwhile), it is
/// [`Nested::NotBack`], naming both paths: the tree stays at `to`, whose unlink takes the same
/// leases first and so refuses it while that build runs ([`unlink`]) (review round 7, C1).
pub fn recheck_renamed(
    from: &Path,
    to: &Path,
    held: &mut Vec<stdx::file_lock::Locked>,
) -> Result<(), Nested> {
    let found = match take_nested_leases(to, None, held) {
        Ok(()) => return Ok(()),
        Err(found) => found,
    };
    let back = rename_no_replace(to, from);
    let at_home = |dir: &Path| {
        dir.strip_prefix(to).map_or_else(|_| dir.to_path_buf(), |rest| from.join(rest))
    };
    match (found, back) {
        (Nested::Held { dir, .. }, Ok(())) => {
            Err(Nested::Held { tree: from.to_path_buf(), dir: at_home(&dir) })
        }
        (Nested::Changed { dir, .. }, Ok(())) => {
            Err(Nested::Changed { tree: from.to_path_buf(), dir: at_home(&dir) })
        }
        (Nested::Io(why), Ok(())) => Err(Nested::Io(format!(
            "{why}; {} was renamed back to {}",
            to.display(),
            from.display()
        ))),
        // `take_nested_leases` never renames, so never says `NotBack`; passed on as it is.
        (found @ Nested::NotBack { .. }, Ok(())) => Err(found),
        (found, Err(error)) => Err(Nested::NotBack {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            why: format!("{found}; renaming it back failed: {error}"),
        }),
    }
}

/// Why a tree is not removed for a lease inside it ([`take_nested_leases`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Nested {
    /// `dir`, inside `tree`, has a lease someone holds: a build uses it.
    Held { tree: PathBuf, dir: PathBuf },
    /// `dir`'s lease, inside `tree`, was renamed away or made again while it was taken.
    Changed { tree: PathBuf, dir: PathBuf },
    /// Listing a directory or taking a lease failed, naming it.
    Io(String),
    /// The tree was renamed from `from` to `to`, a lease inside it was then found held or
    /// changed, and the rename back failed (`why` says both): it stays at `to`, in trash, never
    /// unlinked while a lease in it is held (review round 8, M1).
    NotBack { from: PathBuf, to: PathBuf, why: String },
}

impl std::fmt::Display for Nested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Nested::Held { tree, dir } => write!(
                f,
                "{} is inside {} and a build holds its lease ({}): nothing that holds a leased build dir is removed",
                dir.display(),
                tree.display(),
                lease::lease_of(dir).display()
            ),
            Nested::Changed { tree, dir } => write!(
                f,
                "the lease of {}, inside {}, was moved or made again while it was taken: nothing of {} is removed; try again",
                dir.display(),
                tree.display(),
                tree.display()
            ),
            Nested::Io(why) => f.write_str(why),
            Nested::NotBack { from, to, why } => write!(
                f,
                "{} was moved to {} and not back: {why}. It stays at {}, and is not unlinked while a lease in it is held",
                from.display(),
                to.display(),
                to.display()
            ),
        }
    }
}

/// The directories in the tree at `tree` that hold a lease file (`tree` too, unless it is
/// `skip`), found without taking any lock (review round 6, P1: the walk of a big tree never
/// holds a build off). The walk lists directories only, never follows a link, and never leaves
/// `tree`'s device: a directory inside on another device (a btrfs subvolume, which no mount
/// table lists, or a mount made since the callers' check) is a [`Nested::Io`] naming it and
/// both devices (review round 6, C2).
pub fn nested_lease_dirs(tree: &Path, skip: Option<&Path>) -> Result<Vec<PathBuf>, Nested> {
    // alloc: a removal's directories with a lease, a few
    let mut found = Vec::new();
    let device = match std::fs::symlink_metadata(tree) {
        Ok(meta) if meta.is_dir() => device_of(&meta),
        Ok(_) => return Ok(found),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(Nested::Io(format!("reading {}: {error}", tree.display()))),
    };
    // alloc: a removal's walk, its directories still to list
    let mut todo = vec![tree.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::NotFound && dir != tree => continue,
            Err(error) => return Err(Nested::Io(format!("listing {}: {error}", dir.display()))),
        };
        let mut leased = false;
        for entry in read {
            let entry =
                entry.map_err(|error| Nested::Io(format!("listing {}: {error}", dir.display())))?;
            let kind = entry.file_type().map_err(|error| {
                Nested::Io(format!("the type of {}: {error}", entry.path().display()))
            })?;
            if kind.is_dir() {
                let path = entry.path();
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if device_of(&meta) == device => todo.push(path),
                    Ok(meta) => {
                        return Err(Nested::Io(format!(
                            "{} is on device {} and {} on device {}: nothing of blaze's removes across a file system, so {} is not removed",
                            path.display(),
                            device_of(&meta),
                            tree.display(),
                            device,
                            tree.display()
                        )));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(Nested::Io(format!("reading {}: {error}", path.display())));
                    }
                }
            } else if kind.is_file() && entry.file_name() == lease::LEASE {
                leased = true;
            }
        }
        if leased && skip.is_none_or(|it| it != dir) {
            found.push(dir);
        }
    }
    Ok(found)
}

/// The leases of `dirs` (inside `tree`, [`nested_lease_dirs`]) taken exclusively, never
/// waiting, each checked to be its directory's own, and added to `held`; one `held` has already
/// (the same lease file, [`lease::verified`], as after the tree's rename) is not taken twice. A
/// held one, or one that moved, is a [`Nested`] naming its directory; the locks taken before it
/// stay in `held`, for the caller to drop.
pub fn take_leases(
    tree: &Path,
    dirs: &[PathBuf],
    held: &mut Vec<stdx::file_lock::Locked>,
) -> Result<(), Nested> {
    for dir in dirs {
        let mut have = false;
        for file in held.iter_mut() {
            if matches!(lease::verified(file, dir).map_err(Nested::Io)?, lease::Check::Own) {
                have = true;
                break;
            }
        }
        if have {
            continue;
        }
        let at = || (tree.to_path_buf(), dir.clone()); // clone: a path named in the error
        match lease::take_exclusive(dir).map_err(Nested::Io)? {
            Taken::Exclusive(file) => held.push(file),
            Taken::Held => {
                let (tree, dir) = at();
                return Err(Nested::Held { tree, dir });
            }
            Taken::Moved | Taken::Missing => {
                let (tree, dir) = at();
                return Err(Nested::Changed { tree, dir });
            }
        }
    }
    Ok(())
}

/// Every lease in the tree at `tree` (`tree`'s own too, unless it is `skip`) that `held` does
/// not hold yet, taken exclusively into `held` ([`nested_lease_dirs`], then [`take_leases`]): for
/// the caller to hold until the tree is renamed and removed, so no build uses a directory inside
/// it meanwhile (review round 5, B1: a `--target-dir` named inside another build's directory, or
/// `<root>/build` itself). Run again on the renamed tree, with the locks held, before anything
/// is unlinked, it covers a lease made inside after the first walk (review round 6, C1).
pub fn take_nested_leases(
    tree: &Path,
    skip: Option<&Path>,
    held: &mut Vec<stdx::file_lock::Locked>,
) -> Result<(), Nested> {
    let dirs = nested_lease_dirs(tree, skip)?;
    take_leases(tree, &dirs, held)
}

/// Renames `path` to `to`, whose parent must be blaze's trash.
pub fn rename_into_trash(path: &Path, to: &Path) -> Swept {
    let Some(area) = to.parent().and_then(Path::parent) else {
        return Swept::Failed(format!("{} is in no area's trash", to.display()));
    };
    let name = to.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    if to.parent().and_then(Path::file_name).is_none_or(|it| it != TRASH) || !is_trash_name(&name) {
        return Swept::Failed(format!("{} is not named as blaze names trash", to.display()));
    }
    if let Err(why) = trash_dir(area) {
        return Swept::Failed(why);
    }
    // A rename onto an empty directory replaces it: a name taken twice (a clock set back to
    // the same nanosecond) would swallow an entry. Names are unique per mover (its pid, or its
    // run and step, [`crate::plan::trash_name`]); the rename refuses an existing target itself
    // ([`rename_no_replace`], review N9 and round 5's L4).
    match rename_no_replace(path, to) {
        Ok(()) => Swept::Moved,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Swept::Failed(format!(
            "{} is in trash already: a trash name taken twice, so {} is not moved over it",
            to.display(),
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Swept::Gone,
        Err(error) => {
            Swept::Failed(format!("renaming {} to {}: {error}", path.display(), to.display()))
        }
    }
}

/// Renames `from` to `to`, refusing in the same system call when `to` is there
/// (`io::ErrorKind::AlreadyExists`): `renameat2(RENAME_NOREPLACE)` on Linux,
/// `renamex_np(RENAME_EXCL)` on macOS, so no check-then-rename window (review round 5, L4). A
/// call that fails is an error naming both paths and the errno, never a plain rename. Elsewhere (Windows'
/// `MoveFileExW` without replacing, through std, still replaces an empty directory) a check
/// just before the rename.
#[allow(unsafe_code)]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt as _};
        let c = |path: &Path| {
            CString::new(path.as_os_str().as_bytes()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} has a NUL in it", path.display()),
                )
            })
        };
        let (c_from, c_to) = (c(from)?, c(to)?);
        // SAFETY: both names are NUL-terminated and alive across the call, which only reads them.
        #[cfg(target_os = "linux")]
        let done = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                c_from.as_ptr(),
                libc::AT_FDCWD,
                c_to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        // SAFETY: as above.
        #[cfg(target_os = "macos")]
        let done = unsafe { libc::renamex_np(c_from.as_ptr(), c_to.as_ptr(), libc::RENAME_EXCL) };
        if done == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // What the errno was, said as such: EINVAL is also a rename into its own subtree, so it
        // alone does not say the file system lacks no-replace renames (review round 6, N2).
        match error.raw_os_error() {
            Some(errno @ (libc::EINVAL | libc::ENOTSUP | libc::ENOSYS)) => Err(io::Error::new(
                error.kind(),
                format!(
                    "renaming {} to {} without replacing failed with errno {errno} ({error}); {}",
                    from.display(),
                    to.display(),
                    match errno {
                        libc::EINVAL =>
                            "the file system may not support it, or the rename is invalid",
                        _ => "the file system or kernel does not support it",
                    }
                ),
            )),
            _ => Err(error),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        match std::fs::symlink_metadata(to) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is there", to.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::rename(from, to)
    }
}

/// Removes `path`, an entry of blaze's trash, whole: the bytes it held. Refuses anything else:
/// an entry not named as blaze names trash, or in a directory without blaze's marker. `held`
/// are the locks the caller holds already of leases in it (`blaze clean`'s, through its rename;
/// none for the collector): every other lease in it is taken exclusively first, and one held (a
/// build that locked a lease in the tree after the rename's walk) refuses the unlink, naming it;
/// the locks are held through the removal (review round 6, C1).
pub fn unlink(path: &Path, mut held: Vec<stdx::file_lock::Locked>) -> Result<u64, String> {
    let name = path.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    let in_trash = path.parent().is_some_and(|it| {
        it.file_name().is_some_and(|it| it == TRASH) && it.join(TRASH_MARK).is_file()
    });
    if !in_trash || !is_trash_name(&name) {
        return Err(format!(
            "{} is not an entry of blaze's trash: it is not removed",
            path.display()
        ));
    }
    take_nested_leases(path, None, &mut held).map_err(|why| format!("{why}: it stays in trash"))?;
    // [`remove_tree`] refuses a mount inside before it removes anything, and checks each
    // directory's device as it goes (review round 7, F3: no separate walk first).
    let removed = remove_tree(path);
    drop(held);
    removed
}
