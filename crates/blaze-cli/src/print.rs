//! Findings placed when printed (drive.md, "Printing"; rule 71): a finding holds its item's
//! start and an item-relative range; its frame (or line) is drawn here by blaze-report, from the
//! file's text, and nowhere else. And the profile (rule 17): written to `.blaze/v2/profile.txt`
//! on every command, its summary line on stderr under `--stats`.

use blaze_drive::condition::OVER_CAP_RULE;
use blaze_drive::{Answer, Print, Request};
use blaze_ladder::refusal::Refusal;
use blaze_own::{AString, Vec};
use blaze_report::Severity;
use blaze_report::frame::words::{self, Area, WordsError};
use blaze_report::frame::{Caps, Closing, Counts, Coverage, Edit, Finding, Fixes, Form, Input, LINK_FILE, Mark, Source, closing as closing_line, render};
use bstd::cx::Cx;
use bstd::fs::{Dir, Durable, ReadWrite};
use bstd::mem::{Bytes, Frame, format_in};
use bstd::out::Out;
use bstd::path::Path;

use crate::CliError;
use crate::phases::Phases;

/// What a command has drawn so far: the terminal's abilities it draws for (built once in main,
/// `blaze_report::frame::detect`), the counts its closing line says, and the files they were in.
pub struct Shown<'f> {
    pub caps: Caps,
    pub counts: Counts,
    /// The checkout's absolute path: a frame's header links to `root/path`.
    root: &'f str,
    /// The files drawn in, by their name's subject: one count a file, whichever stage found it.
    files: Vec<blaze_ids::ContentId, &'f Frame>,
}

impl<'f> Shown<'f> {
    pub fn new(caps: Caps, root: &'f str, frame: &'f Frame) -> Shown<'f> {
        Shown { caps, counts: Counts::default(), root, files: Vec::new_in(frame) }
    }

    fn file(&mut self, path: &str) {
        let subject = blaze_drive::Name::file(path.as_bytes()).subject;
        if !self.files.contains(&subject) {
            self.files.push(subject);
            self.counts.files += 1;
        }
    }
}

/// The file a finding of the request is drawn in: its slot's real path (sweep.rs, a module at
/// `x/mod.rs`), or none for a text-less name.
pub fn file_of<'f>(req: &Request<'_>, found: &crate::sweep::Found<'_>, finding: &Print<'_>, frame: &'f Frame) -> Result<Option<&'f str>, CliError> {
    let Some(index) = finding.file else { return Ok(None) };
    let slot = req.path(index);
    let real = crate::sweep::real_path(core::str::from_utf8(slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?, found.flags(slot), frame);
    core::str::from_utf8(real.leak()).map(Some).map_err(|_| CliError::Text { what: "a path is not UTF-8" })
}

/// A finding drawn as its frame (or, off a terminal, its one line): its rule's words from
/// blaze-report's table, its arguments the message's pieces between 0x1F bytes.
/// `path` is the file it is drawn in ([`file_of`]).
pub fn framed(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    path: Option<&str>,
    finding: &Print<'_>,
    shown: &mut Shown<'_>,
    frame: &Frame,
    out: &Out,
) -> Result<(), CliError> {
    let area = match finding.rule {
        OVER_CAP_RULE | MODULE_FILE_MISSING_RULE | PATH_FILE_MISSING_RULE => Area::Drive,
        _ => Area::Cli,
    };
    let message = core::str::from_utf8(&finding.message).map_err(|_| CliError::Text { what: "a finding's message is not UTF-8" })?;
    let Some(path) = path else {
        // A text-less crate's finding: no source to draw, its name is its place.
        shown.counts.add(Severity::Error, false);
        out.line(cx, format_args!("{}: error[{}]: {message}", finding.name, finding.rule))?;
        return Ok(());
    };
    let mut text = Bytes::new_in(frame, "a file to draw a finding in");
    root.read_all(cx, &Path::new(path)?, &mut text)?;
    shown.file(path);
    let start = finding.place.saturating_add(finding.start);
    let end = finding.place.saturating_add(finding.end).max(start);
    draw(cx, shown, Severity::Error, area, finding.rule, message, path, text.as_slice(), Some(finding.place), (start, end), None, frame, out)
}

/// A refusal of `blaze run`'s lowering drawn as its frame: `texts` the (path, text) of each
/// file given to the lowering, which its `file` indexes. After the last: the note of what the
/// command covered (main.rs, `left_out_note`), then the closing line.
pub fn refusal(cx: &Cx, refusal: &Refusal, texts: &[(&str, &str)], shown: &mut Shown<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    let rule = refusal.rule as u16;
    // The level is the printer's, by the rule (rule 81): advice is a warning, the rest errors.
    let severity = if refusal.rule.is_advice() { Severity::Warning } else { Severity::Error };
    let Some((path, text)) = refusal.file.and_then(|index| texts.get(index as usize)) else {
        shown.counts.add(severity, false);
        out.line(cx, format_args!("{}[{}]: {}", severity.label(), refusal.rule.name(), &*refusal.message))?;
        return Ok(());
    };
    shown.file(path);
    let start = refusal.place.saturating_add(refusal.start);
    let end = refusal.place.saturating_add(refusal.end).max(start);
    let secondary = refusal.secondary.map(|(s, e)| (refusal.place.saturating_add(s), refusal.place.saturating_add(e)));
    draw(cx, shown, severity, Area::Ladder, rule, &refusal.message, path, text.as_bytes(), Some(refusal.place), (start, end), secondary, frame, out)
}

#[expect(clippy::too_many_arguments, reason = "the one drawing both kinds of finding share; each argument a different fact of it")]
fn draw(
    cx: &Cx,
    shown: &mut Shown<'_>,
    severity: Severity,
    area: Area,
    rule: u16,
    message: &str,
    path: &str,
    text: &[u8],
    item: Option<u32>,
    (start, end): (u32, u32),
    secondary: Option<(u32, u32)>,
    frame: &Frame,
    out: &Out,
) -> Result<(), CliError> {
    // The frame carries no header of its own: erased into a handle here, for this finding.
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let mut args: [&str; 8] = [""; 8];
    let mut count = 0;
    for piece in message.split('\x1f') {
        if count == args.len() {
            return Err(CliError::Text { what: "a finding has more than eight arguments" });
        }
        args[count] = piece;
        count += 1;
    }
    let mut headline = AString::new_in(alloc);
    let mut label = AString::new_in(alloc);
    let name = words::say(area, rule, &args[..count], &mut headline, &mut label).map_err(|error| match error {
        WordsError::NoWording { .. } => CliError::Text { what: "a finding's rule has no words in blaze_report::frame::words::WORDINGS" },
        WordsError::Arguments { .. } => CliError::Text { what: "a finding's arguments do not match its rule's words" },
    })?;
    let mut absolute = AString::new_in(alloc);
    absolute.push_str(shown.root);
    absolute.push_str("/");
    absolute.push_str(path);
    let others = [Mark { start: secondary.map_or(0, |it| it.0), end: secondary.map_or(0, |it| it.1), label: "" }];
    // The one fix a finding of this layer has: the toolchain's crate named again, its line
    // deleted. There is no `--fix` on this layer yet: `Fixes::ByHand` draws it as a preview and
    // counts nothing fixable. `Fixes::Command` the day `--fix` exists.
    let delete = [Edit { start, end, insert: "" }];
    let finding = Finding {
        severity,
        rule: name,
        message: headline.as_str(),
        file: 0,
        item,
        primary: Mark { start, end, label: label.as_str() },
        secondary: if secondary.is_some() { &others } else { &[] },
        fix: if area == Area::Cli && rule == crate::units::TOOLCHAIN_NAMED { &delete } else { &[] },
    };
    let sources = [Source { shown: path, absolute: absolute.as_str(), text }];
    let input = Input { findings: core::slice::from_ref(&finding), sources: &sources, link: LINK_FILE, fixes: Fixes::ByHand };
    let mut drawn = AString::new_in(alloc);
    // A blank line between frames.
    if shown.caps.form == Form::Frames && shown.counts.errors + shown.counts.warnings + shown.counts.notes > 0 {
        drawn.push_str("\n");
    }
    render(&mut drawn, &input, shown.caps, alloc)
        .map_err(|_| CliError::Text { what: "a finding's range is outside its file" })?;
    shown.counts.add(severity, false);
    out.write_all(cx, drawn.as_bytes())?;
    Ok(())
}

/// A note about the command, not about a place in a file: one quiet line after the findings,
/// drawn by blaze-report (`frame::note`), in the findings' stream.
pub fn note(cx: &Cx, words: &str, shown: &Shown<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    let erased = blaze_own::Erased::new(frame);
    let mut line = AString::new_in(erased.handle());
    blaze_report::frame::note(&mut line, words, shown.caps);
    out.write_all(cx, line.as_bytes())?;
    Ok(())
}

/// The command's last line: `1 error · 1 file checked · 1.1 ms`, or `ok · 14 files checked ·
/// 0.5 ms · nothing changed`. Not after a `run` whose program ran: its output is the output.
/// `tail`, when given, ends the same line: what the command typed did not do (`blaze test`'s
/// tests, not built).
#[expect(clippy::too_many_arguments, reason = "each argument a different fact of the one line")]
pub fn closing(
    cx: &Cx,
    shown: &Shown<'_>,
    checked: u32,
    nanos: u64,
    changed: Option<u32>,
    tail: Option<&str>,
    coverage: Option<Coverage<'_>>,
    frame: &Frame,
    out: &Out,
) -> Result<(), CliError> {
    let erased = blaze_own::Erased::new(frame);
    let mut line = AString::new_in(erased.handle());
    if shown.caps.form == Form::Frames && shown.counts.errors + shown.counts.warnings + shown.counts.notes > 0 {
        line.push_str("\n");
    }
    closing_line(&mut line, &Closing { counts: shown.counts, checked, nanos, changed, fixes: Fixes::ByHand, coverage }, shown.caps);
    if let Some(tail) = tail {
        if line.as_str().ends_with('\n') {
            line.pop();
        }
        line.push_str(if shown.caps.unicode { " · " } else { " - " });
        line.push_str(tail);
        line.push_str("\n");
    }
    out.write_all(cx, line.as_bytes())?;
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

#[cfg(test)]
mod tests {
    use blaze_drive::condition::OVER_CAP_RULE;
    use blaze_ladder::refusal::RefusalRule;
    use blaze_report::frame::words::{Area, wording};

    /// Every rule number this binary can print has a headline and a label in blaze-report's
    /// table: a new rule without words fails here, not in front of a person.
    #[test]
    fn every_rule_the_tree_emits_has_words() {
        let emitted = [
            (Area::Cli, crate::work::SYNTAX),
            (Area::Cli, crate::units::UNREAD),
            (Area::Cli, crate::units::TOOLCHAIN_NAMED),
            (Area::Cli, crate::work::MODULE_AT_BOTH),
            (Area::Drive, OVER_CAP_RULE),
            (Area::Ladder, RefusalRule::Unsupported as u16),
            (Area::Ladder, RefusalRule::Syntax as u16),
            (Area::Ladder, RefusalRule::Program as u16),
            (Area::Ladder, RefusalRule::UnknownBlazeAttribute as u16),
        ];
        for (area, number) in emitted {
            assert!(wording(area, number).is_some(), "a rule blaze-cli prints has no words in WORDINGS: {area:?} {number}");
        }
    }
}
