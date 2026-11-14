//! Whether a locked registry version is yanked, from the index file blaze already has for its
//! package: one JSON object a line, as the registry serves it (`{"name", "vers", "yanked",
//! …}`). The audit never fetches an index; a package whose file blaze does not have is said
//! to be not checked, never taken as not yanked.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

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

#[derive(serde::Deserialize)]
struct Line<'a> {
    #[serde(borrow)]
    name: std::borrow::Cow<'a, str>,
    #[serde(borrow)]
    vers: std::borrow::Cow<'a, str>,
    #[serde(default)]
    yanked: bool,
}

/// What the index file `bytes` of `package` says of `version`.
pub fn yanked(package: &str, version: &semver::Version, bytes: &[u8]) -> Yanked {
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
        let line: Line<'_> = match serde_json::from_slice(line) {
            Ok(it) => it,
            Err(why) => return Yanked::NotChecked(format!("line {} of its index file is not an index entry: {why}", at + 1)),
        };
        if !line.name.eq_ignore_ascii_case(package) {
            return Yanked::NotChecked(format!("line {} of its index file is of `{}`", at + 1, line.name));
        }
        if semver::Version::parse(&line.vers).is_ok_and(|it| &it == version) {
            found = Some(line.yanked);
        }
    }
    match found {
        Some(true) => Yanked::Yes,
        Some(false) => Yanked::No,
        None => Yanked::NotChecked(format!("its index file does not list {version}")),
    }
}
