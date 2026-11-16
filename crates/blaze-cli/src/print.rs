//! Findings placed when printed (drive.md, "Printing"; rule 71): a finding holds its item's
//! start and an item-relative range; `file:line:col` is computed here, from the file's line
//! table, and nowhere else. And the profile (rule 17): written to `.blaze/v2/profile.txt` on
//! every command, its store line on stderr.

use blaze_drive::lines::LineTable;
use blaze_drive::{Answer, Print, Request};
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, ReadWrite};
use bstd::mem::{Bytes, Frame, format_in};
use bstd::out::Out;
use bstd::path::Path;

use crate::CliError;

pub fn finding(cx: &Cx, root: &Dir<ReadWrite>, req: &Request<&Frame>, finding: &Print<&Frame>, frame: &Frame, out: &Out) -> Result<(), CliError> {
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
    let lines = LineTable::new_in(text, frame);
    let at = lines.position(text, finding.place.saturating_add(finding.start));
    out.line(cx, format_args!("{path}:{}:{}: [{}] {message}", at.line, at.column, finding.rule))?;
    Ok(())
}

const PROFILE: &str = ".blaze/v2/profile.txt";

pub fn profile(cx: &Cx, root: &Dir<ReadWrite>, answer: &Answer, frame: &Frame) -> Result<(), CliError> {
    let text = format_in!(frame, "{}", answer.profile);
    root.publish(cx, &Path::new(PROFILE)?, Content::bytes(text.as_bytes()), Durable::No)?;
    Ok(())
}
