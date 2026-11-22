//! Whether a locked registry version is yanked, from the index file blaze already has for its
//! package: one JSON object a line, as the registry serves it (`{"name", "vers", "yanked",
//! …}`). The audit never fetches an index; a package whose file blaze does not have is said
//! to be not checked, never taken as not yanked.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use alloc::{format, string::String};
use alloc::alloc::Global;

/// The largest index file read: 32 MiB. The largest crates.io packages (thousands of
/// versions) are a few MiB.
pub const MOST_INDEX_BYTES: usize = 32 << 20;

/// What the index says of one version.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Yanked {
    Yes,
    No,
    /// The index file does not list the version, or could not be read: why.
    NotChecked(String),
}

/// The three fields of an index line the check reads; every other field is the registry's.
struct Line<'a> {
    name: &'a str,
    vers: &'a str,
    /// False when the line has none.
    yanked: bool,
}

/// `line` read as an index entry, its strings borrowed from the value. Its other keys (`deps`,
/// `cksum`, `features`, `v`, ...) are let be on purpose: the line is the registry's format,
/// which gains keys a reader must tolerate, so this reads the three it needs and no others.
fn line_of(value: &blaze_data::Value<alloc::alloc::Global>) -> Result<Line<'_>, blaze_data::ShapeError<alloc::alloc::Global>> {
    let c = blaze_data::Cursor::root(value, alloc::alloc::Global);
    Ok(Line {
        name: c.field("name")?.text()?,
        vers: c.field("vers")?.text()?,
        yanked: c.opt::<bool>("yanked")?.unwrap_or(false),
    })
}

/// What the index file `bytes` of `package` says of `version`.
pub fn yanked(package: &str, version: &blaze_semver::Version<Global>, bytes: &[u8]) -> Yanked {
    if bytes.len() > MOST_INDEX_BYTES {
        return Yanked::NotChecked(format!(
            "its index file is {} bytes, more than the {MOST_INDEX_BYTES} one may be",
            bytes.len()
        ));
    }
    let mut found = None;
    for (at, line) in bytes.split(|it| *it == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value = match blaze_json::read_in(line, blaze_json::Limits::DEFAULT, alloc::alloc::Global) {
            Ok(it) => it,
            Err(why) => return Yanked::NotChecked(format!("line {} of its index file is not an index entry: {why}", at + 1)),
        };
        let line = match line_of(&value) {
            Ok(it) => it,
            Err(why) => return Yanked::NotChecked(format!("line {} of its index file is not an index entry: {why}", at + 1)),
        };
        if !line.name.eq_ignore_ascii_case(package) {
            return Yanked::NotChecked(format!("line {} of its index file is of `{}`", at + 1, line.name));
        }
        if blaze_semver::Version::parse_in(&line.vers, blaze_semver::Limits::CARGO, Global).is_ok_and(|it| &it == version) {
            found = Some(line.yanked);
        }
    }
    match found {
        Some(true) => Yanked::Yes,
        Some(false) => Yanked::No,
        None => Yanked::NotChecked(format!("its index file does not list {version}")),
    }
}
