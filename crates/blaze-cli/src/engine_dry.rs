//! `blaze engine-dry <file>`: the edit path's forward wake, dry (D187's first step, D168). Over
//! the checkout's stored answer, read only, and with nothing checked, it does what an edit of
//! `<file>` would make the engine do before any checker runs, and counts each step:
//!
//! 1. OUTLINE: `<file>` outlined today by the Outlines stage's own code (work.rs
//!    `outline_file`, engine_tasks.rs `Tasks::work`), under the edition and slot form its stored
//!    row was made with (the leaf table's flags; the edition found from the row's source).
//! 2. COMPARE: today's cells against the ones its stored row defines (the comparison the engine's
//!    merge makes), and today's parts against the stored outline record's: a part whose source
//!    moved is a task entered by its key (a body, a signature, a lint unit).
//! 3. READERS: each moved cell's reader list from the stored answer's readers section (the page
//!    of the cell's definer): the tasks the moved cells wake.
//! 4. FORWARD CHECK: each woken task's row read, its read list scanned and its reads of moved
//!    cells compared with today's digests: it enters or stands. Woken tasks times their reads,
//!    never a sweep. A read of a cell the edit did not move holds its stored digest by
//!    construction (one file changed), so it is not looked up.
//!
//! Then, counted apart from the bar's numbers: 5. the second wave's bound (the readers of every
//! cell an entering task defines, as if all moved), and 6. the largest reader list among the
//! cells the entering tasks read (the hot-cell question, D187).
//!
//! It never reads a producer header shard (pages/headers.rs): a row is found on its own page
//! (`page_of`), its record in that page's records section. The line `header shards: 0` is
//! checked, not assumed. Nothing is written: no put, no commit, no pointer moved.

use core::fmt::Write as _;

use blaze_drive::cells::Cell;
use blaze_drive::key::{Kind, Name};
use blaze_drive::Key;
use blaze_engine::{Answer, Buffer, CellId, Gen, Overlay, Phase, Planned, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_own::{SharedAlloc, Vec};
use bstd::entry::Status;
use bstd::env::VarName;
use bstd::mem::{Bytes, Frame};
use bstd::out::Out;
use bstd::path::Path;
use bstd::Cx;

use crate::engine_answer::{PagesRead, Stored, Unreadable};
use crate::engine_ready_edit::{self as edit, Moved, Woken, page_for};
use blaze_engine::Ask;
use crate::engine_tasks::{HEADS, Outlined, SURFACE, Tasks, WHOLE};
use crate::phases::Phases;
use crate::store::Store as _;
use crate::sweep::{self, FromMeta, LeafKind, Leaves};
use crate::work::{self, Job};
use crate::CliError;

/// Why the dry run has no answer for the file: printed, status failed (rule 19: each names the
/// file and where it was refused).
#[derive(Debug)]
enum DryRefused<'f> {
    /// No answer is stored for this checkout.
    NoAnswer,
    /// The stored answer's pointer or directory does not read.
    Unreadable(Unreadable),
    /// The path is no source file slot of the stored sweep (its leaf table).
    NotASlot { path: &'f str },
    /// The slot has no outline row in the stored answer.
    NoRow { slot: &'f str },
    /// No edition byte makes the stored row's source from the stored leaf's content id.
    NoEdition { slot: &'f str },
    /// The slot's file is at neither of its places today.
    Absent { slot: &'f str },
    /// The row names no outline record on its page.
    NoRecord { slot: &'f str },
    /// The entering names were not sorted and unique after the sort (a bug in this command).
    Unsorted { at: usize },
}

impl core::fmt::Display for DryRefused<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DryRefused::NoAnswer => write!(f, "no stored answer here: run `blaze check` first (engine_dry.rs)"),
            DryRefused::Unreadable(why) => write!(f, "the stored answer does not read: {why} (engine_dry.rs)"),
            DryRefused::NotASlot { path } => write!(f, "`{path}` is no source file of the stored sweep's leaf table (engine_dry.rs `slot_of`)"),
            DryRefused::NoRow { slot } => write!(f, "`{slot}` has no outline row in the stored answer (engine_dry.rs)"),
            DryRefused::NoEdition { slot } => write!(f, "no edition makes `{slot}`'s stored row source from its stored content id (engine_dry.rs `edition_of`)"),
            DryRefused::Absent { slot } => write!(f, "`{slot}` is at neither of its places today (engine_dry.rs)"),
            DryRefused::NoRecord { slot } => write!(f, "`{slot}`'s row names no outline record on its page (engine_dry.rs)"),
            DryRefused::Unsorted { at } => write!(f, "the entering names are not sorted and unique at {at} (engine_dry.rs, engine_ready_edit.rs `Entering`)"),
        }
    }
}

/// One step's wall time and the pages it read (the [`PagesRead`] difference across it).
#[derive(Clone, Copy, Default)]
struct Step {
    ns: u64,
    pages: usize,
    bytes: u64,
    hashed: u64,
    headers: usize,
}

impl Step {
    fn across(before: PagesRead, after: PagesRead, ns: u64) -> Step {
        Step {
            ns,
            pages: after.task - before.task,
            bytes: after.bytes - before.bytes,
            hashed: after.hashed - before.hashed,
            headers: after.header - before.header,
        }
    }

    fn plus(self, other: Step) -> Step {
        Step {
            ns: self.ns + other.ns,
            pages: self.pages + other.pages,
            bytes: self.bytes + other.bytes,
            hashed: self.hashed + other.hashed,
            headers: self.headers + other.headers,
        }
    }
}

impl core::fmt::Display for Step {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}.{:03} ms; pages: {} read, {} bytes, {} hashed; header shards: {}",
            self.ns / 1_000_000,
            self.ns / 1_000 % 1_000,
            self.pages,
            self.bytes,
            self.hashed,
            self.headers
        )
    }
}

/// What the command prints.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// `blaze engine-dry <file>`: the edit path's forward wake for an edit of `<file>`.
    Dry,
    /// `blaze engine-keys`: the stored answer's identity, one line per key: its root's key, its
    /// serial, and each page's key in order. Two checks of one edit from one stored answer, one
    /// per driver (`--driver=phases`, then `--driver=ready` from a copy of the same home), give
    /// byte-equal answers exactly when these lines are equal (every page is keyed by its bytes,
    /// the root by its own), so `diff` of the two outputs names the first page that differs.
    Keys,
}

/// The command. Its arguments: for [`Mode::Dry`] the edited file's path, checkout-relative;
/// none for [`Mode::Keys`]. The checkout is the working directory.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one specific capability")]
pub(crate) fn run(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    mode: Mode,
) -> Result<Status, CliError> {
    let target = match mode {
        Mode::Dry => args.get(2).filter(|word| !word.starts_with("--")).ok_or(CliError::Usage("blaze engine-dry <file>: a source file's path in the checkout, as its crate names it"))?,
        Mode::Keys => "",
    };
    let flags = crate::flags(args)?;
    let frame = cx.frame();
    let target = work::held_str(target, frame);
    // The command's edge, as engine_check.rs `edge` reads it, only what the store needs.
    // The checkout is the working directory: this command's one positional word is the edited
    // FILE, which `flags` reads as `check`'s checkout directory (its first run on tempo stopped
    // on "stat of <file>/Cargo.toml: Not a directory"). So `flags.dir` is not the checkout here.
    let _ = flags.dir;
    let driver = flags.driver;
    let checkout = crate::checkout::checkout(None, frame)?;
    let located = crate::run_package::locate(cx, fs, &checkout, frame)?;
    let root = located.root;
    let producers = crate::producers::table(host.build_id()?.as_bytes());
    let tools = crate::engine_store::tools(&crate::home::tools(&producers, crate::lints::Lints::NONE.rules));
    let var = |name: &str| -> Option<&str> { env?.get(&VarName::new(name).ok()?).ok().flatten() };
    let setup = phases.now();
    let max_store_bytes = crate::gc::budget_from_env(env)?;
    let opened = crate::store::open_for_command(cx, fs, host, rng, var("HOME"), max_store_bytes, &root, located.at, phases.clock(), err, heap)?;
    let erased = blaze_own::Erased::new(frame);
    let mut store = crate::store::CommandStore::new(cx, &opened.lent, false, opened.open_ns, None, phases.clock().clone(), err, erased.handle())?;
    let request_mem = crate::engine_phase_mem::RequestMem::new()?;
    let alloc = request_mem.shared();
    let answer_root = crate::engine_answer::root_name(located.at, frame);
    let mut text = blaze_own::String::new_in(frame);
    let status = match crate::engine_answer::load_pointer(cx, &mut store, &answer_root)? {
        None => refuse(&mut text, DryRefused::NoAnswer),
        Some(Err(why)) => refuse(&mut text, DryRefused::Unreadable(why)),
        Some(Ok(pointer)) => match crate::engine_answer::open_root(cx, &mut store, pointer.root, alloc)? {
            Err(why) => refuse(&mut text, DryRefused::Unreadable(why)),
            Ok(directory) if mode == Mode::Keys => {
                line(&mut text, format_args!("root {} serial {} bits {} pages {}", pointer.root, directory.serial.0, directory.bits, directory.pages.len()));
                // The pointer's fields that are a function of the checkout's text and the driver:
                // not its own key, its leaves' stamps or its journal places (each of the machine
                // and the moment: rule 25), only how many leaves it holds.
                let facts = pointer.facts;
                line(&mut text, format_args!("lines {}", pointer.lines));
                line(&mut text, format_args!("facts files {} findings {} owed {} names {} bodies_whole {} replayable {}", facts.files, facts.findings, facts.owed, facts.printed.names, facts.printed.bodies_whole, facts.printed.replayable));
                line(&mut text, format_args!("leaves {}", pointer.leaves.len()));
                match &pointer.headers {
                    blaze_engine::pages::Headers::Current(shards) => {
                        line(&mut text, format_args!("headers current {}", shards.keys.len()));
                        for (at, key) in shards.keys.iter().enumerate() {
                            line(&mut text, format_args!("header {at} {key}"));
                        }
                    }
                    blaze_engine::pages::Headers::Absent { since } => line(&mut text, format_args!("headers absent since {}", since.0)),
                }
                for (at, key) in directory.pages.iter().enumerate() {
                    line(&mut text, format_args!("page {at} {key}"));
                }
                Status::OK
            }
            Ok(directory) => {
                let snapshot = store.snapshot_in(alloc)?;
                let stored = Stored::new(&directory, &snapshot, &pointer.headers, blaze_engine::pages::Check::Store, alloc);
                let setup_ns = phases.since(setup);
                phases.mark_in("engine-dry: the store, the pointer, the directory and the snapshot", frame);
                line(&mut text, format_args!("setup (store open, pointer, directory, snapshot; not the edit path's): {}.{:03} ms", setup_ns / 1_000_000, setup_ns / 1_000 % 1_000));
                // The precondition, as `check` would decide it for this one edited file.
                let edited = [edit::Changed { kind: LeafKind::Slot, both: true }];
                let base = if pointer.certifies_under(&tools) { edit::StoredBase::Own } else { edit::StoredBase::ToolsMoved };
                match edit::choose(driver, Some(&directory), base, &edited) {
                    Ok(edit::Chosen::Phases(why)) => line(&mut text, format_args!("{}", edit::EditPath::Phases(why))),
                    Ok(edit::Chosen::Wake { files }) => line(&mut text, format_args!("precondition, from the root: the wake plans {files} edited file(s); the entering kinds decide below")),
                    Err(why) => line(&mut text, format_args!("{why}")),
                }
                let dry = Dry { cx, root: &root, stored: &stored, leaves: &pointer.leaves, frame, alloc };
                match dry.run(&mut store, phases, target, &mut text)? {
                    Ok(()) => Status::OK,
                    Err(why) => refuse(&mut text, why),
                }
            }
        },
    };
    out.write_all(cx, text.as_bytes())?;
    Ok(status)
}

/// The refusal said, and the failed status.
fn refuse(text: &mut blaze_own::String<&Frame>, why: DryRefused<'_>) -> Status {
    line(text, format_args!("{why}"));
    Status::FAILED
}

/// One line into `text` (a full frame cuts the report short where it stood).
fn line(text: &mut blaze_own::String<&Frame>, args: core::fmt::Arguments<'_>) {
    let _ = text.write_fmt(args);
    text.push('\n');
}

/// What the dry run reads.
struct Dry<'d, 'a> {
    cx: &'d Cx,
    root: &'d bstd::fs::Dir<bstd::fs::ReadWrite>,
    stored: &'d Stored<'a>,
    leaves: &'d Leaves<'d>,
    frame: &'d Frame,
    alloc: SharedAlloc<'a>,
}

impl<'d, 'a> Dry<'d, 'a> {
    /// The stored sweep's row for `path`: its slot (`x.rs` for a file found at `x/mod.rs`) and
    /// the leaf's flags and content id as the stored answer was made from them.
    fn slot_of(&self, path: &'d str) -> Option<(&'d str, u8, ContentId)> {
        let mod_rs = path.strip_suffix("/mod.rs");
        for k in 0..self.leaves.len() {
            let leaf = self.leaves.row(k);
            if !matches!(leaf.kind, LeafKind::Slot) || leaf.absent() {
                continue;
            }
            let slot = self.leaves.path(k);
            let at_mod_rs = leaf.flags & sweep::AT_MOD_RS != 0;
            let hit = match (at_mod_rs, mod_rs) {
                (false, _) => slot == path,
                (true, Some(dir)) => slot.strip_suffix(".rs") == Some(dir),
                (true, None) => false,
            };
            if hit {
                return Some((slot, leaf.flags, leaf.id));
            }
        }
        None
    }

    /// The edition byte the stored row was outlined under: the one that makes its source from
    /// the stored leaf's id and flags (engine_check.rs `Kept::stood`'s comparison, inverted over
    /// the 256 bytes). The stored answer keeps no file's edition by itself: a lookup missing.
    fn edition_of(id: ContentId, flags: u8, source: ContentId) -> Option<u8> {
        (0..=u8::MAX).find(|&edition| crate::engine_tasks::outline_source(crate::engine_tasks::edition_source(id, edition), flags) == source)
    }

    fn run(&self, store: &mut dyn crate::store::Store<'_>, phases: &mut Phases, path: &'d str, text: &mut blaze_own::String<&Frame>) -> Result<Result<(), DryRefused<'d>>, CliError> {
        let (cx, frame, alloc, stored) = (self.cx, self.frame, self.alloc, self.stored);
        let Some((slot, old_flags, old_id)) = self.slot_of(path) else { return Ok(Err(DryRefused::NotASlot { path })) };
        let file = Name::file(slot.as_bytes());

        // 1. OUTLINE, today, by the Outlines stage's own code.
        let (t, before) = (phases.now(), stored.pages_read_line());
        let Some(row_page) = page_for(stored, file, Ask::Row)? else { return Ok(Err(DryRefused::NoRow { slot })) };
        let Some(row) = row_page.row(file) else { return Ok(Err(DryRefused::NoRow { slot })) };
        let Some(edition) = Self::edition_of(old_id, old_flags, row.facts.source) else { return Ok(Err(DryRefused::NoEdition { slot })) };
        let (flags, _, id) = sweep::probe_slot(cx, self.root, &FromMeta::now(phases.clock()), slot, old_flags & sweep::MODULE != 0, frame)?;
        if flags & sweep::ABSENT != 0 {
            return Ok(Err(DryRefused::Absent { slot }));
        }
        let job = Job {
            name: file,
            slot,
            flags,
            edition,
            expect: id,
            // The rule set `check` lints with (lints.rs `of_root`): the outline lists its lint
            // units keyed for it, as the check's outline does (work.rs `Job::lints`).
            lints: Some(crate::lints::of_root(cx, self.root, frame)?.rules),
        };
        let made = work::outline_file(cx, self.root, &job, frame, alloc)?;
        let source = crate::engine_tasks::edition_source(id, edition);
        let outline_source = crate::engine_tasks::outline_source(source, flags);
        let mut real = SVec::new_in(alloc);
        real.extend_from_slice(&sweep::real_path(slot, flags, frame));
        let outlined = [Outlined { name: file, path: real, source, outline_source, edition, made }];
        let tasks = Tasks { outlined: &outlined };
        let overlay = Overlay::new_in(Answer::new_in(alloc), Gen(0), blaze_engine::Plant::None, alloc)?;
        let task = Task { planned: Planned { name: file, phase: Phase::new(Stage::Outlines, 0), source: outline_source, file }, key: Key::default() };
        let mut buffer = Buffer::new_in(alloc);
        tasks.work(cx, View::from_overlay(&overlay), &task, &mut buffer, alloc);
        let done = buffer.done.pop().expect("the Outlines task writes one result");
        let outline_step = Step::across(before, stored.pages_read_line(), phases.since(t));
        phases.mark_in("engine-dry: 1. the file outlined", frame);

        // 2. COMPARE: today's cells against the stored row's; today's parts against the stored
        // record's.
        let (t, before) = (phases.now(), stored.pages_read_line());
        let mut moved: SVec<'a, Moved> = Vec::new_in(alloc);
        edit::moved_cells(file, row.defines(), &done.defines, &mut moved);
        let defined = done.defines.len();
        let Some(record_key) = row_page.record(file) else { return Ok(Err(DryRefused::NoRecord { slot })) };
        let held = store.get(cx, record_key)?.ok_or(crate::engine_check::EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name: file, key: record_key }))?;
        let old_record = crate::engine_store::outline_from(file, record_key, held.bytes, alloc).map_err(crate::engine_check::EngineCheckError::OutlineBack)?;
        let mut woken: SVec<'a, (Name, Woken)> = Vec::new_in(alloc);
        let new_parts: &[blaze_drive::Part] = match &outlined[0].made {
            Ok(record) => &record.parts,
            Err(_) => &[],
        };
        edit::keyed(&old_record.parts, new_parts, &mut woken);
        let compare_step = Step::across(before, stored.pages_read_line(), phases.since(t));
        phases.mark_in("engine-dry: 2. cells and parts compared", frame);

        // 3. READERS of each moved cell, from the cell's own page (pages.rs `readers_slot`).
        let (t, before) = (phases.now(), stored.pages_read_line());
        let counted = edit::readers(stored, &moved, &mut woken)?;
        let (reader_entries, largest) = (counted.entries, (counted.largest, counted.largest_at));
        let readers_step = Step::across(before, stored.pages_read_line(), phases.since(t));
        phases.mark_in("engine-dry: 3. the moved cells' readers", frame);

        // 4. FORWARD CHECK: each woken task's row, its reads of moved cells compared.
        let (t, before) = (phases.now(), stored.pages_read_line());
        let mut entering: SVec<'a, (Name, Woken)> = Vec::new_in(alloc);
        let mut grain = Grain::default();
        let edit::Forward { scanned, on_moved, no_row } = edit::forward(stored, &moved, &woken, &mut entering, |read| grain.count(read))?;
        let check_step = Step::across(before, stored.pages_read_line(), phases.since(t));
        phases.mark_in("engine-dry: 4. the woken tasks' read lists checked forward", frame);
        // The Ready path's scope: just the entering names (`forward` leaves them sorted, once).
        let mut names: SVec<'a, Name> = Vec::new_in(alloc);
        for &(name, _) in entering.iter() {
            names.push(name);
        }
        names.sort_unstable();
        names.dedup();
        let scope = match edit::Entering::new(&names) {
            Ok(scope) => scope,
            Err(unsorted) => return Ok(Err(DryRefused::Unsorted { at: unsorted.at })),
        };
        line(text, format_args!("scope: {} entering names", scope.len()));
        // The precondition's second step, as `check --driver=ready` decides it before any run.
        match edit::admit(&names) {
            Ok(path) => line(text, format_args!("{path}")),
            Err(why) => line(text, format_args!("{why}")),
        }

        // 5. The second wave's bound: the readers of every cell an entering task defines.
        let (t, before) = (phases.now(), stored.pages_read_line());
        let mut second: SVec<'a, Name> = Vec::new_in(alloc);
        for &(name, _) in entering.iter() {
            let Some(page) = page_for(stored, name, Ask::Row)? else { continue };
            let Some(row) = page.row(name) else { continue };
            for (cell, _) in row.defines() {
                let id = CellId { of: name, cell };
                let Some(list) = edit::page_for(stored, blaze_engine::pages::readers_slot(&id), Ask::Readers)? else { continue };
                second.extend(list.readers(&id).filter(|reader| woken.binary_search_by(|(woke, _)| woke.cmp(reader)).is_err()));
            }
        }
        second.sort_unstable();
        second.dedup();
        let bound_step = Step::across(before, stored.pages_read_line(), phases.since(t));
        phases.mark_in("engine-dry: 5. the second wave's bound", frame);

        // 6. The largest reader list among the cells the entering tasks read.
        let (t, before) = (phases.now(), stored.pages_read_line());
        let mut hot = (0usize, None::<CellId>);
        for &(name, _) in entering.iter() {
            let Some(row) = page_for(stored, name, Ask::Row)?.and_then(|page| page.row(name)) else { continue };
            for (read, _) in row.reads() {
                let Some(page) = page_for(stored, blaze_engine::pages::readers_slot(&read), Ask::Readers)? else { continue };
                let readers = page.readers(&read).len();
                if readers > hot.0 {
                    hot = (readers, Some(read));
                }
            }
        }
        let hot_step = Step::across(before, stored.pages_read_line(), phases.since(t));
        phases.mark_in("engine-dry: 6. the entering tasks' hottest read", frame);

        // The report.
        let bar = outline_step.plus(compare_step).plus(readers_step).plus(check_step);
        let unchanged = outline_source == row.facts.source;
        line(text, format_args!("engine-dry `{path}`: slot `{slot}`, flags {flags:#x}, edition byte {edition}; today's text {}", if unchanged { "equals the stored row's (nothing moved is expected)" } else { "differs from the stored row's" }));
        line(text, format_args!("1. outline:       {outline_step}"));
        line(text, format_args!("2. compare:       {compare_step}; cells moved: {} of {defined} defined ({}); tasks keyed by a moved part: {}", moved.len(), Classes::of(&moved), woken.iter().filter(|(_, why)| *why == Woken::Keyed).count()));
        line(text, format_args!("3. readers:       {readers_step}; reader entries: {reader_entries}; largest list: {} ({})", largest.0, Shown(largest.1)));
        line(text, format_args!("4. forward check: {check_step}; woken: {} (by key {}, by readers {}); read-list entries scanned: {scanned}, on moved cells: {on_moved}; entering: {}; woken with no row: {no_row}", woken.len(), woken.iter().filter(|(_, why)| *why == Woken::Keyed).count(), woken.iter().filter(|(_, why)| *why == Woken::Reader).count(), entering.len()));
        line(text, format_args!("   the bar's steps (1-4): {bar}"));
        line(text, format_args!("5. second wave:   {bound_step}; at most {} more tasks (every cell of every entering task moved)", second.len()));
        line(text, format_args!("6. hottest read:  {hot_step}; largest reader list among the entering tasks' reads: {} ({})", hot.0, Shown(hot.1)));
        line(text, format_args!("   grain of the woken tasks' reads: {grain}"));
        let all_headers = stored.pages_read_line().header;
        line(text, format_args!("   header shards read in all: {all_headers}{}", if all_headers == 0 { "" } else { " (a step read headers.rs: a bug of this file)" }));
        line(text, format_args!("woken, by kind: {}", ByKind::of(woken.iter().map(|(name, _)| *name), self.frame)));
        line(text, format_args!("entering, by kind: {}", ByKind::of(entering.iter().map(|(name, _)| *name), self.frame)));
        self.name_rows(text, path, new_parts, &entering)?;
        Ok(Ok(()))
    }

    /// Each entering task by a name a person reads: a part of the edited file by its line and
    /// first words; any other by its kind and its file's path when its row names one.
    fn name_rows(&self, text: &mut blaze_own::String<&Frame>, path: &str, parts: &[blaze_drive::Part], entering: &[(Name, Woken)]) -> Result<(), CliError> {
        const SHOWN: usize = 40;
        let mut bytes = Bytes::new_in(self.frame, "the edited file, for the report's names");
        self.root.read_all(self.cx, &Path::new(path)?, &mut bytes)?;
        let source = bytes.as_slice();
        for &(name, why) in entering.iter().take(SHOWN) {
            let how = match why {
                Woken::Keyed => "by its key",
                Woken::Reader => "by a read",
            };
            if let Some(part) = parts.iter().find(|part| part.name == name) {
                let at = (part.place as usize).min(source.len());
                let line_no = source[..at].iter().filter(|&&b| b == b'\n').count() + 1;
                let rest = &source[at..];
                let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len()).min(80);
                let first = core::str::from_utf8(&rest[..end]).unwrap_or("(not UTF-8)");
                line(text, format_args!("  {} {how}: {path}:{line_no} `{}`", name.kind.name(), first.trim()));
                continue;
            }
            let file = page_for(self.stored, name, Ask::Row)?.and_then(|page| page.row(name)).map(|row| row.facts.file);
            let shown = file.and_then(|file| (0..self.leaves.len()).map(|k| self.leaves.path(k)).find(|leaf| Name::file(leaf.as_bytes()) == file));
            match shown {
                Some(file_path) => line(text, format_args!("  {name} {how}: in {file_path}")),
                None => line(text, format_args!("  {name} {how}")),
            }
        }
        if entering.len() > SHOWN {
            line(text, format_args!("  ... and {} more", entering.len() - SHOWN));
        }
        Ok(())
    }
}

/// A cell by its definer and class, or none.
struct Shown(Option<CellId>);

impl core::fmt::Display for Shown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            None => write!(f, "none"),
            Some(id) => write!(f, "{} cell of {}", class(id.cell), id.of),
        }
    }
}

fn class(cell: Cell) -> &'static str {
    match cell {
        WHOLE => "the whole record's",
        SURFACE => "the surface",
        HEADS => "the heads",
        _ => "a named",
    }
}

/// The moved cells by class.
struct Classes {
    whole: usize,
    surface: usize,
    heads: usize,
    named: usize,
}

impl Classes {
    fn of(moved: &[Moved]) -> Classes {
        let mut classes = Classes { whole: 0, surface: 0, heads: 0, named: 0 };
        for moved in moved {
            match moved.id.cell {
                WHOLE => classes.whole += 1,
                SURFACE => classes.surface += 1,
                HEADS => classes.heads += 1,
                _ => classes.named += 1,
            }
        }
        classes
    }
}

impl core::fmt::Display for Classes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "whole {}, surface {}, heads {}, named {}", self.whole, self.surface, self.heads, self.named)
    }
}

/// The grain of a read list: how many reads name a whole record (the outline's `WHOLE`, a
/// widened read), a file's surface or heads, or one named cell; and how many are of a crate's
/// scope or impl bucket (whole-crate tables).
#[derive(Default)]
struct Grain {
    whole: usize,
    surface: usize,
    heads: usize,
    named: usize,
    crate_tables: usize,
}

impl Grain {
    fn count(&mut self, read: CellId) {
        match read.cell {
            WHOLE => self.whole += 1,
            SURFACE => self.surface += 1,
            HEADS => self.heads += 1,
            _ => self.named += 1,
        }
        if matches!(read.of.kind, Kind::CrateScope | Kind::ImplBucket) {
            self.crate_tables += 1;
        }
    }
}

impl core::fmt::Display for Grain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "whole record {}, file surface {}, file heads {}, named cell {} (of which a crate's scope or impl bucket: {})", self.whole, self.surface, self.heads, self.named, self.crate_tables)
    }
}

/// Names counted by kind.
struct ByKind<'f> {
    counts: Vec<(Kind, usize), &'f Frame>,
}

impl<'f> ByKind<'f> {
    fn of(names: impl Iterator<Item = Name>, frame: &'f Frame) -> ByKind<'f> {
        let mut counts: Vec<(Kind, usize), &'f Frame> = Vec::new_in(frame);
        for name in names {
            match counts.iter_mut().find(|(kind, _)| *kind == name.kind) {
                Some((_, count)) => *count += 1,
                None => counts.push((name.kind, 1)),
            }
        }
        ByKind { counts }
    }
}

impl core::fmt::Display for ByKind<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.counts.is_empty() {
            return write!(f, "none");
        }
        for (at, (kind, count)) in self.counts.iter().enumerate() {
            if at > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{} {count}", kind.name())?;
        }
        Ok(())
    }
}
