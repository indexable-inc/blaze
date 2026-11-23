//! `blaze why <file or file::item>` (engine.md E11): why the stored answer's task for a file's
//! outline or an item's body was entered, one line a step back to the edit.
//!
//! The answer (engine_answer.rs `load_answer`) keeps each task's last cause (pages.rs `causes`):
//! new, its key moved (its own text), a read that moved (the cell, its task, the digests), owed,
//! or made inline. A read that moved names the task that defines the cell, whose own cause is the
//! next step, until a step that is not a read: the edit itself. Each task is named as a path
//! (rule 19): a file's outline by its path (the leaf table's), a body by its file and its item's
//! chain of names, any other by its kind and the first bytes of its subject.
//!
//! Nothing is written: the store is opened, read and closed as `check`'s certificate does.

use blaze_drive::{Kind, Name};
use blaze_engine::pages::Kept;
use blaze_engine::{Cause, CellId};
use bstd::entry::Status;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::out::Out;
use bstd::path::Path;
use bstd::Cx;

use crate::engine_check::Opts;
use crate::phases::Phases;
use crate::sweep::Leaves;
use crate::{CliError, work};

/// The explanation of `target` from the checkout's stored answer, printed to `out`.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
pub(crate) fn explain(
    cx: &Cx,
    fs: &bstd::fs::Fs,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    root: &Dir<ReadWrite>,
    opts: &Opts<'_>,
    target: &str,
    phases: &mut Phases,
    out: &Out,
    err: &Out,
) -> Result<Status, CliError> {
    let frame = cx.frame();
    let erased = blaze_own::Erased::new(frame);
    let place = &opts.place;
    let opened = crate::store::open_for_command(cx, fs, host, rng, place.home, place.max_store_bytes, root, place.checkout, phases.clock(), err, opts.heap)?;
    let mut store = crate::store::CommandStore::new(cx, &opened.lent, false, opened.open_ns, None, phases.clock().clone(), err, erased.handle())?;
    let answer_root = crate::engine_answer::root_name(place.checkout, frame);
    let mut text = blaze_own::String::new_in(frame);
    let status = match crate::engine_answer::load_pointer(cx, &mut store, &answer_root)? {
        None => {
            line(&mut text, format_args!("no stored answer here: run `blaze engine-check` first"));
            Status::FAILED
        }
        Some(Err(why)) => {
            line(&mut text, format_args!("{why}"));
            Status::FAILED
        }
        Some(Ok(pointer)) => match crate::engine_answer::load_answer(cx, &mut store, pointer.root, opts.heap.shared())? {
            Err(why) => {
                line(&mut text, format_args!("{why}"));
                Status::FAILED
            }
            // A file is outlined under its crate's edition, from the stored unit graph (plan row
            // D2): a graph that does not read is said where an edition is needed, never an
            // edition chosen in its place.
            Ok((kept, _)) => {
                let editions = crate::engine_units::stored_editions(cx, &mut store, &kept.records, frame)?;
                let names = Names { cx, root, leaves: &pointer.leaves, kept: &kept, editions: &editions, frame };
                walk(&names, target, &mut text, || crate::engine_answer::load_lines(cx, &mut store, pointer.lines, opts.heap.shared()))?
            }
        },
    };
    out.write_all(cx, text.as_bytes())?;
    crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
    drop(store);
    crate::store::after_answer_printed(cx, opened.lent, &opened.home, false, phases.clock(), err)?;
    Ok(status)
}

/// One line into `text` (a full frame cuts the explanation short where it stood).
fn line(text: &mut blaze_own::String<&Frame>, args: core::fmt::Arguments<'_>) {
    use core::fmt::Write as _;
    let _ = text.write_fmt(args);
    text.push('\n');
}

/// What names a task as a path: the leaf table's paths, the answer's rows, the checkout's files.
struct Names<'n, 'm, 'a> {
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    leaves: &'n Leaves<'m>,
    kept: &'n Kept<'a>,
    /// Each member crate's edition, from the stored unit graph, or why that graph did not read.
    editions: &'n Result<crate::engine_units::Editions<'n>, crate::engine_units::BackRefused>,
    frame: &'n Frame,
}

impl Names<'_, '_, '_> {
    /// The edition `path` is outlined under: its crate's, from the stored unit graph. `Err`
    /// says why there is none (rule 3).
    fn edition_of(&self, path: &str) -> Result<blaze_surface::Edition, blaze_own::String<&Frame>> {
        let mut why = blaze_own::String::new_in(self.frame);
        match self.editions {
            Ok(editions) => match editions.of(path) {
                Some(edition) => return Ok(edition),
                None => line_into(&mut why, format_args!("`{path}` is a file of no member crate of the stored unit graph, so no edition reads it")),
            },
            Err(refused) => line_into(&mut why, format_args!("`{path}` has no edition to be read under: {refused}")),
        }
        Err(why)
    }

    /// The checkout path whose outline task is `file`, from the leaf table.
    fn path_of(&self, file: Name) -> Option<&str> {
        (0..self.leaves.len()).map(|k| self.leaves.path(k)).find(|path| Name::file(path.as_bytes()) == file)
    }

    /// `path`'s text outlined under `edition` (its crate's, [`Names::edition_of`]), each item
    /// with a body by its chain of names, as `(chain, body)`.
    fn bodies(&self, path: &str, edition: blaze_surface::Edition) -> Result<blaze_own::Vec<(blaze_own::String<&Frame>, Name), &Frame>, CliError> {
        let mut bytes = Bytes::new_in(self.frame, "a source file `blaze why` names an item in");
        self.root.read_all(self.cx, &Path::new(path)?, &mut bytes)?;
        let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "a source file is not UTF-8" })?;
        let erased = blaze_own::Erased::new(self.frame);
        let outline = blaze_file_items::outline(text, edition, erased.handle());
        let file = Name::file(path.as_bytes());
        let mut found = blaze_own::Vec::new_in(self.frame);
        for item in outline.items().iter() {
            let mut chain = blaze_own::String::new_in(self.frame);
            let mut at = Some(item);
            let mut names: blaze_own::Vec<&str, &Frame> = blaze_own::Vec::new_in(self.frame);
            while let Some(entry) = at {
                names.push(entry.name.map_or("_", |range| entry.text(text, range)));
                at = entry.parent.map(|up| outline.item(up));
            }
            for (i, name) in names.iter().rev().enumerate() {
                if i > 0 {
                    chain.push_str("::");
                }
                chain.push_str(name);
            }
            found.push((chain, work::body_name(file, &outline, item, text)));
        }
        Ok(found)
    }

    /// `name` as a path: a file's, a body's `file::item`, or its kind and subject.
    fn label(&self, name: Name) -> Result<blaze_own::String<&Frame>, CliError> {
        let mut shown = blaze_own::String::new_in(self.frame);
        let file = self.kept.answer.rows.get(&name).map_or(name, |row| row.file);
        let path = match name.kind {
            Kind::FileItems | Kind::BodyCheck => self.path_of(if name.kind == Kind::FileItems { name } else { file }),
            _ => None,
        };
        match (name.kind, path) {
            (Kind::FileItems, Some(path)) => line_into(&mut shown, format_args!("`{path}`'s outline")),
            (Kind::BodyCheck, Some(path)) => match self.edition_of(path) {
                Ok(edition) => match self.bodies(path, edition)?.iter().find(|(_, body)| *body == name) {
                    // Its edition said: the one its file was outlined under to find it.
                    Some((chain, _)) => line_into(&mut shown, format_args!("the body of `{path}::{chain}` (read as edition {edition}, its crate's in the stored unit graph)")),
                    None => line_into(&mut shown, format_args!("a body in `{path}` no item of its text names today (read as edition {edition})")),
                },
                Err(why) => line_into(&mut shown, format_args!("a body in `{path}` ({why})")),
            },
            (kind, _) => {
                line_into(&mut shown, format_args!("{} ", kind.name()));
                for byte in &name.subject.0[..4] {
                    line_into(&mut shown, format_args!("{byte:02x}"));
                }
            }
        }
        Ok(shown)
    }
}

fn line_into(text: &mut blaze_own::String<&Frame>, args: core::fmt::Arguments<'_>) {
    use core::fmt::Write as _;
    let _ = text.write_fmt(args);
}

/// The cell's words: an outline's whole record or its surface, or its first bytes.
fn cell_words(id: &CellId, into: &mut blaze_own::String<&Frame>) {
    if id.cell == crate::engine_tasks::WHOLE {
        into.push_str("whole record");
    } else if id.cell == crate::engine_tasks::SURFACE {
        into.push_str("surface (what names read of the file)");
    } else {
        into.push_str("cell ");
        for byte in &id.cell.0[..4] {
            line_into(into, format_args!("{byte:02x}"));
        }
    }
}

/// The task `target` names, then each step of why it was entered, into `text`.
fn walk<'m, 'a>(
    names: &Names<'_, '_, '_>,
    target: &str,
    text: &mut blaze_own::String<&Frame>,
    load_lines: impl FnOnce() -> Result<Result<crate::engine_lines::Last<'m, 'a>, crate::engine_answer::Unreadable>, CliError>,
) -> Result<Status, CliError> {
    let (path, item) = match target.split_once("::") {
        Some((path, item)) => (path, Some(item)),
        None => (target, None),
    };
    let file = Name::file(path.as_bytes());
    let start = match item {
        None => file,
        Some(item) => {
            let edition = match names.edition_of(path) {
                Ok(edition) => edition,
                Err(why) => {
                    line(text, format_args!("{why}"));
                    return Ok(Status::FAILED);
                }
            };
            match names.bodies(path, edition)?.iter().find(|(chain, _)| chain.as_str() == item) {
                Some((_, body)) => *body,
                None => {
                    line(text, format_args!("`{path}` has no item `{item}` with a body today"));
                    return Ok(Status::FAILED);
                }
            }
        }
    };
    let kept = names.kept;
    if !kept.answer.rows.contains_key(&start) {
        line(text, format_args!("{} is no task of the stored answer (request {})", names.label(start)?, kept.answer.serial.0));
        return Ok(Status::FAILED);
    }
    // A refused body that stands on its reads (FR1): its stored refusal, and the first read
    // whose move would enter it again. A failed body whose refusal is not stored is an error.
    if let Some(row) = kept.answer.rows.get(&start)
        && start.kind == Kind::BodyCheck
        && !kept.answer.owed_tasks.contains_key(&start)
        && row.reads.iter().all(|(id, saw)| kept.answer.cell(id) == *saw)
    {
        let refusal = load_lines()?.and_then(|lines| lines.body_refusal(start).map_err(crate::engine_answer::Unreadable::Lines));
        match refusal {
            Ok(Some(cause)) => {
                let mut trigger = blaze_own::String::new_in(names.frame);
                if let Some((id, _)) = row.reads.first() {
                    line_into(&mut trigger, format_args!("{}'s ", names.label(id.of)?));
                    cell_words(id, &mut trigger);
                } else {
                    trigger.push_str("its own key");
                }
                line(text, format_args!("stands refused: {cause}; reads {}; would be entered if {trigger} moved", row.reads.len()));
            }
            Ok(None) if row.failed => {
                line(text, format_args!("{}: the failed body's stored refusal is missing", names.label(start)?));
                return Ok(Status::FAILED);
            }
            Ok(None) => {}
            Err(why) => {
                line(text, format_args!("{why}"));
                return Ok(Status::FAILED);
            }
        }
    }
    // One step a task, each task once: the chain ends at the edit, or at a task seen already.
    let mut at = start;
    let mut seen: blaze_own::Vec<Name, &Frame> = blaze_own::Vec::new_in(names.frame);
    loop {
        let label = names.label(at)?;
        let Some(&(request, cause)) = kept.causes.get(&at) else {
            line(text, format_args!("{label}: never entered by a request this answer holds; it stands from the first"));
            break;
        };
        let latest = if request == kept.answer.serial { " (the last request)" } else { "" };
        let n = request.0;
        match cause {
            Cause::New => line(text, format_args!("{label}: entered in request {n}{latest}: new (no row of it before)")),
            Cause::KeyMoved => line(text, format_args!("{label}: entered in request {n}{latest}: its key moved (its own text, or its tool)")),
            Cause::Owed => line(text, format_args!("{label}: entered in request {n}{latest}: owed by an earlier request that did not run it")),
            Cause::Inline => line(text, format_args!("{label}: entered in request {n}{latest}: made inline by another task")),
            Cause::ReadMoved { read, was, now } => {
                let mut cell = blaze_own::String::new_in(names.frame);
                cell_words(&read, &mut cell);
                let definer = names.label(read.of)?;
                line(text, format_args!("{label}: entered in request {n}{latest}: a read moved: {definer}'s {cell} ({was:x} to {now:x})"));
            }
        }
        seen.push(at);
        match cause {
            Cause::ReadMoved { read, .. } if !seen.contains(&read.of) => at = read.of,
            _ => break,
        }
    }
    Ok(Status::OK)
}

/// The checkout's stored answer handed to `with`, opened as [`explain`] opens it (`None`: none
/// stored, or one that does not read): what a command-level test asks of the rows' read lists
/// and definitions (engine_check_tests.rs `names_reads_of_bodies`). Nothing is written.
#[cfg(test)]
#[expect(clippy::too_many_arguments, reason = "the test's edge: each argument one specific capability or handle")]
pub(crate) fn with_kept<R>(
    cx: &Cx,
    fs: &bstd::fs::Fs,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    root: &Dir<ReadWrite>,
    opts: &Opts<'_>,
    phases: &mut Phases,
    err: &Out,
    with: impl FnOnce(&Kept<'_>) -> R,
) -> Result<Option<R>, CliError> {
    let frame = cx.frame();
    let erased = blaze_own::Erased::new(frame);
    let place = &opts.place;
    let opened = crate::store::open_for_command(cx, fs, host, rng, place.home, place.max_store_bytes, root, place.checkout, phases.clock(), err, opts.heap)?;
    let mut store = crate::store::CommandStore::new(cx, &opened.lent, false, opened.open_ns, None, phases.clock().clone(), err, erased.handle())?;
    let answer_root = crate::engine_answer::root_name(place.checkout, frame);
    let got = match crate::engine_answer::load_pointer(cx, &mut store, &answer_root)? {
        Some(Ok(pointer)) => match crate::engine_answer::load_answer(cx, &mut store, pointer.root, opts.heap.shared())? {
            Ok((kept, _)) => Some(with(&kept)),
            Err(_) => None,
        },
        _ => None,
    };
    crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
    drop(store);
    crate::store::after_answer_printed(cx, opened.lent, &opened.home, false, phases.clock(), err)?;
    Ok(got)
}
