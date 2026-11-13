//! `blaze doctor [--json]`: checks what a daemon restores from the state directory, without a
//! daemon and without changing anything, and prints the command that sets a faulty part aside.
//!
//! What it checks:
//! - the daemon image (`<state>/db/image`): that it is an image of this blaze's layout whose
//!   spine hashes to what it names, and every group of every section against its CRC32C
//!   ([`blaze_image::Image::verify`]): the check a start makes, plus the groups a start checks
//!   only as it reads them;
//! - the profile history (`<state>/profile`): that its manifest reads and lists files that
//!   are there ([`blaze_profile::history::read_manifest`]).
//!
//! It runs in the CLI, on the files: the daemon holds none of this, and a daemon that cannot
//! start is when it is needed. It deletes and moves nothing: the command it prints moves the
//! faulty directory aside whole, so it is kept for a bug report, and the next daemon starts
//! empty (a cold build), saying so.
//!
//! The answer is the first line. `--json` prints [`Report`], version [`VERSION`].

use std::{
    ffi::OsString,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use blaze_core::{Mapping, exit::Exit};
use blaze_image::{Image, OpenError};
use serde::Serialize;

use super::say;

const USAGE: &str = "blaze doctor [--json]";

/// The version of [`Report`]'s JSON: bumped when a field changes meaning or goes away.
pub const VERSION: u32 = 1;

/// What `blaze doctor` found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub version: u32,
    /// The state directory checked.
    pub state: String,
    /// No check found a fault.
    pub ok: bool,
    pub checks: Vec<Check>,
    /// The commands that set each faulty part aside, in the order to run them; empty when
    /// `ok`.
    pub quarantine: Vec<String>,
}

/// One part of the persisted state, and what checking it found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    /// What was checked: `daemon image`, `profile history`.
    pub what: &'static str,
    pub file: String,
    pub verdict: Verdict,
    /// What was found, in words: counts for `ok`, the rule for `unused`, the fault for `fault`.
    pub said: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// It is there and every check of it held.
    Ok,
    /// It is not there: a daemon starts that part empty, which is no fault.
    Absent,
    /// It is whole, and this blaze does not use it by a rule (another blaze's layout): a
    /// daemon starts that part empty and says so. No fault.
    Unused,
    /// It is not what a blaze writes, or fails a check: a daemon refuses to start on it.
    Fault,
}

/// `blaze doctor [--json]`.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    let mut json = false;
    for word in words {
        match word.to_str() {
            Some("--json") => json = true,
            _ => {
                blaze_report::err!(
                    "{}",
                    say::error(&format!("{USAGE}, not `{}`", word.to_string_lossy()), color)
                );
                return Exit::Usage.code();
            }
        }
    }
    let state = match crate::serve::state() {
        Ok(state) => state,
        Err(error) => {
            blaze_report::err!("{}", say::error(&format!("`blaze doctor`: {error:#}"), color));
            return Exit::Failed.code();
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| it.as_secs());
    let report = examine(&state, now);
    let said = match json {
        true => match serde_json::to_string(&report) {
            Ok(mut line) => {
                line.push('\n');
                line
            }
            Err(error) => {
                blaze_report::err!(
                    "{}",
                    say::error(&format!("`blaze doctor`'s report as JSON: {error}"), color)
                );
                return Exit::Failed.code();
            }
        },
        false => text(&report),
    };
    blaze_report::out!("{said}");
    match report.ok {
        true => 0,
        false => Exit::Failed.code(),
    }
}

/// The state under `state` checked; `now` (seconds since the epoch) names where a faulty part
/// would be set aside.
pub fn examine(state: &Path, now: u64) -> Report {
    let db = state.join("db");
    let history = blaze_profile::history::dir_in(state);
    let image = image(&image_file(state));
    let profile = profile(&history);
    let mut quarantine = Vec::new();
    for (check, dir) in [(&image, &db), (&profile, &history)] {
        if check.verdict == Verdict::Fault {
            quarantine.push(set_aside(dir, now));
        }
    }
    if !quarantine.is_empty() {
        // First, so no daemon holds or writes what is moved.
        quarantine.insert(0, "blaze stop".to_owned());
    }
    Report {
        version: VERSION,
        state: state.display().to_string(),
        ok: quarantine.is_empty(),
        checks: vec![image, profile],
        quarantine,
    }
}

/// The command that moves `dir` aside, whole, to a name beside it no check reads.
fn set_aside(dir: &Path, now: u64) -> String {
    let mut aside = dir.as_os_str().to_owned();
    aside.push(format!(".suspect-{now}"));
    format!("mv {} {}", quoted(dir), quoted(Path::new(&aside)))
}

/// `path` as one word of a POSIX shell.
fn quoted(path: &Path) -> String {
    let text = path.display().to_string();
    let plain = text.bytes().all(|it| it.is_ascii_alphanumeric() || b"/._-+,:@".contains(&it));
    match plain && !text.is_empty() {
        true => text,
        false => format!("'{}'", text.replace('\'', "'\\''")),
    }
}

/// The daemon image at `file`, checked whole.
fn image(file: &Path) -> Check {
    let check = |verdict, said: String| Check {
        what: "daemon image",
        file: file.display().to_string(),
        verdict,
        said,
    };
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return check(
                Verdict::Absent,
                "no image: the next daemon starts with an empty database".to_owned(),
            );
        }
        Err(error) => return check(Verdict::Fault, format!("it cannot be read: {error}")),
    };
    let len = bytes.len();
    // Copied to a 16-byte boundary, as a mapped file's pages are.
    let mapping = Mapping::copy(&bytes);
    drop(bytes);
    let opened = match Image::open(mapping) {
        Ok(opened) => opened,
        Err(error @ OpenError::Layout { .. }) => {
            return check(
                Verdict::Unused,
                format!("{error}: another blaze wrote it, and this one starts empty rather than read it"),
            );
        }
        Err(error) => return check(Verdict::Fault, error.to_string()),
    };
    let corrupt = opened.verify();
    if let Some(first) = corrupt.first() {
        let more = match corrupt.len() {
            1 => String::new(),
            n => format!(" ({} more groups fail their checksum)", n - 1),
        };
        return check(Verdict::Fault, format!("{first}{more}"));
    }
    let sections = opened.sections().len();
    check(
        Verdict::Ok,
        format!("{} in {sections} sections, every group's checksum holds", size(len)),
    )
}

/// The profile history in `dir`: its manifest read, as every reader of it does.
fn profile(dir: &Path) -> Check {
    let manifest = dir.join("manifest.json");
    let check = |verdict, said: String| Check {
        what: "profile history",
        file: manifest.display().to_string(),
        verdict,
        said,
    };
    if !manifest.exists() {
        return check(Verdict::Absent, "no history: no daemon has flushed a profile here".to_owned());
    }
    match blaze_profile::history::read_manifest(dir) {
        Ok(read) => check(
            Verdict::Ok,
            format!("its manifest reads, at flush {}", read.flushed()),
        ),
        Err(error) => check(Verdict::Fault, error.to_string()),
    }
}

/// A size in bytes, for a person.
fn size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{} KiB", bytes >> 10),
        _ => format!("{} MiB", bytes >> 20),
    }
}

/// `report` for a person: the answer, a line a check, then the commands that set the faulty
/// parts aside.
pub fn text(report: &Report) -> String {
    let mut out = String::new();
    let faults = report.checks.iter().filter(|it| it.verdict == Verdict::Fault).count();
    // Writing to a `String` does not fail.
    _ = match faults {
        0 => writeln!(out, "the persisted state under {} is sound", report.state),
        1 => writeln!(out, "1 fault in the persisted state under {}", report.state),
        n => writeln!(out, "{n} faults in the persisted state under {}", report.state),
    };
    for it in &report.checks {
        let verdict = match it.verdict {
            Verdict::Ok => "ok",
            Verdict::Absent => "absent",
            Verdict::Unused => "unused",
            Verdict::Fault => "FAULT",
        };
        _ = writeln!(out, "  {verdict:<6}  {} {}: {}", it.what, it.file, it.said);
    }
    if !report.quarantine.is_empty() {
        _ = writeln!(
            out,
            "to set it aside (kept whole, for a bug report; the next daemon starts that part empty and builds cold):"
        );
        _ = writeln!(out, "  {}", report.quarantine.join(" && "));
    }
    out
}

/// Where the state's daemon image is, for a caller naming it.
pub fn image_file(state: &Path) -> PathBuf {
    state.join("db").join("image")
}

#[cfg(test)]
mod tests {
    use blaze_format::SchemaHash;
    use blaze_ids::ContentId;
    use blaze_image::{Builder, SectionId};

    use super::*;

    /// An image of one section, as a daemon writes the plane's history.
    fn an_image() -> Vec<u8> {
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, SchemaHash(ContentId([3; 32])), &[7; 5000]);
        builder.finish().expect("a small image")
    }

    fn state_with(image: Option<&[u8]>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(bytes) = image {
            std::fs::create_dir_all(dir.path().join("db")).unwrap();
            std::fs::write(image_file(dir.path()), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn an_empty_state_is_sound_and_says_what_is_absent() {
        let dir = state_with(None);
        let report = examine(dir.path(), 7);
        assert!(report.ok && report.quarantine.is_empty(), "{report:?}");
        assert!(report.checks.iter().all(|it| it.verdict == Verdict::Absent), "{report:?}");
        let said = text(&report);
        assert!(said.starts_with("the persisted state under "), "{said}");
        assert!(said.lines().next().is_some_and(|it| it.ends_with(" is sound")), "{said}");
    }

    #[test]
    fn a_whole_image_is_sound() {
        let dir = state_with(Some(&an_image()));
        let report = examine(dir.path(), 7);
        assert!(report.ok, "{report:?}");
        assert_eq!(report.checks[0].verdict, Verdict::Ok, "{report:?}");
        assert!(report.checks[0].said.contains("every group's checksum holds"), "{report:?}");
    }

    #[test]
    fn a_flipped_byte_is_a_fault_naming_the_file_and_the_command_that_sets_it_aside() {
        let mut bytes = an_image();
        // The section's first byte.
        let at = Image::open(bytes.clone()).expect("an image").sections()[0].at as usize;
        assert_eq!(bytes[at], 7);
        bytes[at] ^= 0x40;
        let dir = state_with(Some(&bytes));
        let report = examine(dir.path(), 1_762_850_000);
        assert!(!report.ok, "{report:?}");
        let image = &report.checks[0];
        assert_eq!(image.verdict, Verdict::Fault, "{report:?}");
        assert!(image.file.ends_with("db/image") && image.said.contains("is corrupt"), "{image:?}");
        let db = dir.path().join("db").display().to_string();
        assert_eq!(report.quarantine, ["blaze stop".to_owned(), format!("mv {db} {db}.suspect-1762850000")]);
        // Nothing was moved or deleted.
        assert!(image_file(dir.path()).exists());
        let said = text(&report);
        assert!(said.starts_with("1 fault in the persisted state under "), "{said}");
        assert!(said.contains(&format!("  blaze stop && mv {db} {db}.suspect-1762850000\n")), "{said}");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["checks"][0]["verdict"], "fault");
    }

    #[test]
    fn a_file_that_is_no_image_is_a_fault() {
        let dir = state_with(Some(b"not an image"));
        let report = examine(dir.path(), 7);
        assert_eq!(report.checks[0].verdict, Verdict::Fault, "{report:?}");
        assert!(report.checks[0].said.contains("not a blaze image"), "{report:?}");
    }

    #[test]
    fn a_path_is_quoted_when_a_shell_would_split_it() {
        assert_eq!(quoted(Path::new("/a/b.c-d")), "/a/b.c-d");
        assert_eq!(quoted(Path::new("/a b/it's")), "'/a b/it'\\''s'");
    }
}
