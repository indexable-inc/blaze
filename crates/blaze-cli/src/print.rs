//! Findings placed when printed (drive.md, "Printing"; rule 71): a finding holds its item's
//! start and an item-relative range; `file:line:col` is computed here, from the file's line
//! table, and nowhere else. And the profile (rule 17): written to `.blaze/v2/profile.txt` on
//! every command, its store line on stderr.

use blaze_drive::lines::LineTable;
use blaze_drive::{Answer, Print, Request};
use bstd::cx::Cx;
use bstd::fs::{Dir, Durable, ReadWrite};
use bstd::mem::{Bytes, Frame, format_in};
use bstd::out::Out;
use bstd::path::Path;

use crate::CliError;
use crate::phases::Phases;

pub fn finding(cx: &Cx, root: &Dir<ReadWrite>, req: &Request<'_>, finding: &Print<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    let message = core::str::from_utf8(&finding.message).unwrap_or("(a message that is not UTF-8)");
    let Some(index) = finding.file else {
        // A manifest's or a text-less crate's finding: its name is its place.
        out.line(cx, format_args!("{}: [{}] {message}", finding.name, finding.rule))?;
        return Ok(());
    };
    let path = core::str::from_utf8(req.path(index)).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
    let mut text = Bytes::new_in(frame, "a file to place a finding in");
    root.read_all(cx, &Path::new(path)?, &mut text)?;
    let text = text.as_slice();
    // The frame carries no header of its own: erased into a handle here, for this finding.
    let erased = blaze_own::Erased::new(frame);
    let lines = LineTable::new_in(text, erased.handle());
    let at = lines.position(text, finding.place.saturating_add(finding.start));
    out.line(cx, format_args!("{path}:{}:{}: [{}] {message}", at.line, at.column, finding.rule))?;
    Ok(())
}

const PROFILE: &str = ".blaze/v2/profile.txt";

/// The profile and the command's phases, written in place and never synced: a crash, or a
/// reader during the write, sees a part of it; it is a report, read by a person, made again by
/// the next command. Not `publish`: bstd's unsynced publish renames through `Effect::Rename`,
/// which syncs the directory (`F_FULLFSYNC` on macOS, about 5 ms), the whole of a repeat
/// check's own time before this. One truncating open (its directory made only when missing),
/// one append, one close.
pub fn profile(cx: &Cx, root: &Dir<ReadWrite>, answer: &Answer, phases: &Phases, frame: &Frame) -> Result<(), CliError> {
    let text = format_in!(frame, "{}{}", answer.profile, phases);
    let file = root.create(cx, &Path::new(PROFILE)?)?;
    file.append(cx, text.as_bytes(), Durable::No)?;
    file.close(cx, Durable::No)?;
    Ok(())
}
