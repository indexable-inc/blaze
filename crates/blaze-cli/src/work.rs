//! The tasks this binary runs (drive.md, "Tasks and Need"; "The system side"):
//!
//! - `Units`: the manifest's package, its edition and its default targets (`src/lib.rs`,
//!   `src/main.rs`, `src/bin/*.rs`), each a `Locate::File` part.
//! - `FileItems`: the file's outline (blaze-file-items), its `FileItems` record as the payload,
//!   each `mod x;` a `Locate::File` part, each syntax error a finding.
//! - Every other kind answers `Failed` naming what is not built: no item task is listed yet, so
//!   `check` is a syntax check of every file the crate roots reach until blaze-typeck's
//!   `Signature` and `BodyCheck` tasks exist (their lane).
//!
//! A batch's tasks are entered on this thread, one after another, and answered together in one
//! `Ran` (drive.md, "Scheduling on many cores"); the pool over `workers` waits on bstd's threaded
//! layout. Neither task kind here answers `Need`, so nothing is answered in place yet.

use blaze_drive::{
    Batch, Effect, Event, FileEntry, Kind, Locate, Name, Outcome, Output, Part, Ran, Request, Task, TaskFailure,
};
use blaze_drive::record::Finding;
use blaze_ids::ContentId;
use blaze_own::{AVec, Vec};
use blaze_surface::{Edition, ItemDetail};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;

/// The finding number of a syntax error: the outline's, rule-free (a lint has its rule's number).
pub const SYNTAX: u16 = 1;

const OUT_DOMAIN: &str = "blaze-cli parts out 1";

/// What one task made, held in the frame until its batch is answered.
struct Made<'f> {
    name: Name,
    out: ContentId,
    parts: Vec<Part, &'f Frame>,
    /// (rule, start, end, message start, message end) into `messages`.
    findings: Vec<(u16, u32, u32, usize, usize), &'f Frame>,
    messages: Vec<u8, &'f Frame>,
    payload: Vec<u8, &'f Frame>,
}

enum Did<'f> {
    Done(Made<'f>),
    Failed(TaskFailure),
}

/// Run every task of `batch`, then answer the machine once.
pub fn run_batch<'f, 'r>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    files: &[FileEntry<'_>],
    req: &mut Request<'r>,
    batch: Batch<'r>,
    frame: &'f Frame,
    effects: &mut AVec<'_, Effect<'r>>,
) -> Result<(), CliError> {
    let cx = cx.span("batch");
    let mut did = Vec::with_capacity_in(batch.tasks.len(), frame);
    for (ticket, task) in batch.tasks.iter() {
        did.push((*ticket, run(&cx, root, files, req, task, frame)?));
    }
    let mut findings: Vec<Vec<Finding<'_>, &Frame>, &Frame> = Vec::with_capacity_in(did.len(), frame);
    for (_, done) in did.iter() {
        let mut held = Vec::new_in(frame);
        if let Did::Done(made) = done {
            for (rule, start, end, at, to) in made.findings.iter().copied() {
                held.push(Finding { rule, start, end, message: &made.messages[at..to] });
            }
        }
        findings.push(held);
    }
    let mut outputs: Vec<Output<'_>, &Frame> = Vec::with_capacity_in(did.len(), frame);
    for ((_, done), findings) in did.iter().zip(findings.iter()) {
        if let Did::Done(made) = done {
            outputs.push(Output {
                name: made.name,
                out: made.out,
                reads: &[],
                parts: &made.parts,
                findings,
                payload: &made.payload,
            });
        }
    }
    let mut results = Vec::with_capacity_in(did.len(), frame);
    let mut at = 0;
    for (ticket, done) in did.iter() {
        let ran = match done {
            Did::Done(_) => {
                at += 1;
                Ran::Done(&outputs[at - 1..at])
            }
            Did::Failed(failure) => Ran::Failed(*failure),
        };
        // No clock in this binary yet (bstd has none passed in): time is 0, said by the profile.
        results.push(Outcome { ticket: *ticket, ran, in_place: 0, ns: 0 });
    }
    req.step(Event::Ran { batch: batch.id, results: &results }, effects)?;
    Ok(())
}

fn run<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    files: &[FileEntry<'_>],
    req: &Request<'_>,
    task: &Task<'_>,
    frame: &'f Frame,
) -> Result<Did<'f>, CliError> {
    let cx = cx.span("task");
    match task.name.kind {
        Kind::Units => {
            let mut text = Bytes::new_in(frame, "the manifest");
            root.read_all(&cx, &Path::new("Cargo.toml")?, &mut text)?;
            let text = core::str::from_utf8(text.as_slice()).map_err(|_| CliError::Text { what: "Cargo.toml is not UTF-8" })?;
            let edition = manifest_edition(text)?;
            let mut parts = Vec::new_in(frame);
            for file in files {
                let path = file.path;
                let root_file = path == b"src/lib.rs"
                    || path == b"src/main.rs"
                    || (path.starts_with(b"src/bin/") && path.ends_with(b".rs") && !path[8..].contains(&b'/'));
                if root_file {
                    parts.push(Part { name: Name::file(path), locate: Locate::File { edition: edition.byte() }, place: 0 });
                }
            }
            if parts.is_empty() {
                return Err(CliError::Manifest { line: 1, why: "the package has no src/lib.rs, src/main.rs or src/bin/*.rs" });
            }
            parts.sort_unstable_by_key(|part| part.name);
            let out = parts_out(&parts);
            let (findings, messages, payload) = (Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame));
            Ok(Did::Done(Made { name: task.name, out, parts, findings, messages, payload }))
        }
        Kind::FileItems => {
            let index = task.file.ok_or(CliError::Text { what: "a FileItems task with no file" })?;
            let path = Vec::from_slice_in(req.path(index), frame);
            let path = core::str::from_utf8(&path).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
            let mut bytes = Bytes::new_in(frame, "a source file");
            root.read_all(&cx, &Path::new(path)?, &mut bytes)?;
            let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "a source file is not UTF-8" })?;
            if u32::try_from(text.len()).is_err() {
                return Err(CliError::Text { what: "a source file past 4 GiB" });
            }
            let edition = Edition::from_byte(task.edition).ok_or(CliError::Text { what: "a part's edition byte is no edition" })?;
            // blaze-file-items allocates through a one-word handle; the frame carries no header of
            // its own, so it is erased into one here, on this stack, for this task.
            let erased = blaze_own::Erased::new(frame);
            let outline = blaze_file_items::outline(text, edition, erased.handle());
            let mut parts = Vec::new_in(frame);
            for item in outline.items() {
                let ItemDetail::Mod { inline: false, path: attr } = item.detail else { continue };
                if attr.is_some() {
                    let failure = TaskFailure { name: task.name, what: "a `#[path]` module is not read yet", rule: "blaze-cli work.rs" };
                    return Ok(Did::Failed(failure));
                }
                let Some(name) = item.name else { continue };
                let child = module_file(path, text, &outline, item, item.text(text, name), frame)?;
                let child = child.ok_or(CliError::Text { what: "a `mod x;` inside a block that is no module" })?;
                parts.push(Part { name: Name::file(&child), locate: Locate::File { edition: task.edition }, place: item.range.start });
            }
            parts.sort_unstable_by_key(|part| part.name);
            let mut findings = Vec::new_in(frame);
            let mut messages = Vec::new_in(frame);
            for error in outline.errors() {
                let at = messages.len();
                messages.extend_from_slice(outline.message(error).as_bytes());
                findings.push((SYNTAX, error.range.start, error.range.end, at, messages.len()));
            }
            let record = outline
                .to_file_items(text, ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()), erased.handle())
                .map_err(|_| CliError::Text { what: "the outline is not one of its file" })?;
            let payload = Vec::from_slice_in(&record, frame);
            let out = parts_out(&parts);
            Ok(Did::Done(Made { name: task.name, out, parts, findings, messages, payload }))
        }
        _ => Ok(Did::Failed(TaskFailure {
            name: task.name,
            what: "this task kind is not built on the new item layer yet",
            rule: "drive.md, \"Tasks and Need\"",
        })),
    }
}

/// What a reader of a parts list sees: the parts' names, how each is found and where.
fn parts_out(parts: &[Part]) -> ContentId {
    let mut h = blaze_ids::Hasher::new(OUT_DOMAIN);
    for part in parts {
        h.update(&[u8::try_from(part.name.kind.index()).unwrap_or(u8::MAX)]).update(&part.name.subject.0).update(&part.place.to_le_bytes());
        match part.locate {
            Locate::Source(id) => h.update(&[0]).update(&id.0),
            Locate::File { edition } => h.update(&[1, edition]),
            Locate::Key(key) => h.update(&[2]).update(&key.0.0),
        };
    }
    h.finish()
}

/// The file `mod name;` names, written in `item` of the file at `path` (rustc's rule without
/// `#[path]`): a crate root's or a `mod.rs`'s modules are beside it, any other file's are in the
/// directory named for it; each enclosing inline `mod` adds its name. `None` when an enclosing
/// entry is not a module. A `name/mod.rs` file is not looked for: the drive stops with `NoFile`
/// naming `name.rs` (a gap: choosing between the two reads the file list, a read the key lacks).
fn module_file<'f, 't>(
    path: &str,
    text: &'t str,
    outline: &blaze_file_items::Outline<'_>,
    item: &blaze_surface::Item,
    name: &str,
    frame: &'f Frame,
) -> Result<Option<Vec<u8, &'f Frame>>, CliError> {
    let (dir, stem) = match path.rfind('/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => ("", path),
    };
    let beside = matches!(stem, "mod.rs" | "lib.rs" | "main.rs") || dir == "src/bin";
    let mut out = Vec::new_in(frame);
    out.extend_from_slice(dir.as_bytes());
    if !beside {
        push_segment(&mut out, stem.strip_suffix(".rs").unwrap_or(stem));
    }
    // The enclosing inline modules, outermost first.
    let mut chain: Vec<&'t str, &Frame> = Vec::new_in(frame);
    let mut parent = item.parent;
    while let Some(at) = parent {
        let entry = outline.item(at);
        let ItemDetail::Mod { inline: true, .. } = entry.detail else { return Ok(None) };
        let Some(range) = entry.name else { return Ok(None) };
        chain.push(entry.text(text, range));
        parent = entry.parent;
    }
    for segment in chain.iter().rev() {
        push_segment(&mut out, segment);
    }
    push_segment(&mut out, name);
    out.extend_from_slice(b".rs");
    Ok(Some(out))
}

fn push_segment(out: &mut Vec<u8, &Frame>, segment: &str) {
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend_from_slice(segment.strip_prefix("r#").unwrap_or(segment).as_bytes());
}

/// The package's edition: `edition = "20xx"` under `[package]`, 2015 when absent (cargo's
/// default). The reader takes one package with no dependencies; any other table is refused by
/// line (drive.md, "Units").
fn manifest_edition(text: &str) -> Result<Edition, CliError> {
    let mut table = "";
    let mut edition = Edition::E2015;
    let mut in_array = false;
    for (n, raw) in text.lines().enumerate() {
        let line_no = u32::try_from(n + 1).unwrap_or(u32::MAX);
        let line = raw.split('#').next().unwrap_or("").trim();
        if in_array {
            in_array = !line.ends_with(']');
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            table = name.trim_end_matches(']').trim();
            let known = matches!(table, "package" | "dependencies" | "dev-dependencies" | "build-dependencies")
                || table.starts_with("profile.")
                || table == "lints" || table.starts_with("lints.");
            if !known {
                return Err(CliError::Manifest { line: line_no, why: "a table the units reader does not take" });
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(CliError::Manifest { line: line_no, why: "a line that is no `key = value`" });
        };
        let (key, value) = (key.trim(), value.trim());
        if value.starts_with('[') && !value.ends_with(']') {
            in_array = true;
        }
        match table {
            "package" if key == "edition" => {
                edition = match value.trim_matches('"') {
                    "2015" => Edition::E2015,
                    "2018" => Edition::E2018,
                    "2021" => Edition::E2021,
                    "2024" => Edition::E2024,
                    _ => return Err(CliError::Manifest { line: line_no, why: "an edition that is none of 2015, 2018, 2021, 2024" }),
                };
            }
            "package" if key == "build" => {
                return Err(CliError::Manifest { line: line_no, why: "a build script: script runs are not built on the new item layer" });
            }
            "dependencies" | "dev-dependencies" | "build-dependencies" => {
                return Err(CliError::Manifest { line: line_no, why: "a dependency: `Fetch` of a crate's pack is not built yet" });
            }
            _ => {}
        }
    }
    Ok(edition)
}
