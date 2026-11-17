//! The names of a pack's files, the same in a directory, under a base URL and at a peer:
//!
//! ```text
//! <base>/v1/<scope>/<pin>.head        which manifest the pin of the scope is: the one name that is looked up
//! <base>/v1/<name>.manifest           the manifest whose bytes hash to <name> (64 hex digits)
//! <base>/v1/<name>.body               the records' bytes, in the manifest's order; or, compressed for transfer:
//! <base>/v1/<name>.spans              the index of the body's compressed spans (`crate::spans`)
//! <base>/v1/<name>.zst                the spans' frames
//! ```
//!
//! A pack has the body raw or compressed; a reader asks for the span index first and reads
//! the raw body when there is none.
//!
//! A pin and a scope are the only names in a path that come from a pack or a user, and both
//! are held to [`name`]: a few plain characters, never a separator, never a leading dot. A
//! pack's own name is hex this crate writes. So no name read from a manifest, a head or a
//! request can leave its directory.

use alloc::{format, string::String};

use crate::{Id, Refused};

/// The layout's version: the first part of every path. A reader of another version finds
/// nothing under this one and says so by name; it reads nothing as this one.
pub const VERSION: &str = "v1";

/// The longest pin or scope.
pub const NAME_MOST: usize = 96;

/// `name` when it is one a path may hold: 1 to [`NAME_MOST`] of `a-z`, `0-9`, `.`, `_` and
/// `-`, starting with a letter, a digit or `_`.
pub fn name<'a>(what: &'static str, name: &'a str) -> Result<&'a str, Refused> {
    let plain = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_';
    let ok = !name.is_empty()
        && name.len() <= NAME_MOST
        && plain(name.as_bytes()[0])
        && name.bytes().all(|byte| plain(byte) || byte == b'.' || byte == b'-');
    match ok {
        true => Ok(name),
        false => {
            // An error's text: at most a bounded prefix of what was given.
            let mut shown = String::new();
            for ch in name.chars().take(NAME_MOST) {
                shown.push(ch);
            }
            Err(Refused::Name { what, name: shown })
        }
    }
}

/// The path of the head of `pin` in `scope`, from the base.
pub fn head_path(scope: &str, pin: &str) -> Result<String, Refused> {
    let scope = name("the scope", scope)?;
    let pin = name("the pin", pin)?;
    Ok(format!("{VERSION}/{scope}/{pin}.head"))
}

/// The path of the manifest of the pack `pack`, from the base.
pub fn manifest_path(pack: &Id) -> String {
    format!("{VERSION}/{pack}.manifest")
}

/// The path of the span index of the compressed body of the pack `pack`, from the base.
pub fn spans_path(pack: &Id) -> String {
    format!("{VERSION}/{pack}.spans")
}

/// The path of the compressed frames of the body of the pack `pack`, from the base.
pub fn frames_path(pack: &Id) -> String {
    format!("{VERSION}/{pack}.zst")
}

/// The path of the body of the pack `pack`, from the base.
pub fn body_path(pack: &Id) -> String {
    format!("{VERSION}/{pack}.body")
}
