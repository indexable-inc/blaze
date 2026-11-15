//! `.to_string_lossy().to_string()`, written 172 times in crates/ and tools/: `to_string` on a
//! `Cow<str>` copies the text even when the `Cow` already owns it; `into_owned` takes it.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::Path;

// Rewritten: a `Cow<str>` from a path.
pub fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// Rewritten: a `Cow<str>` held in a binding first, and one from an `OsStr`.
pub fn named(name: &OsStr) -> String {
    let text: Cow<'_, str> = name.to_string_lossy();
    text.into_owned()
}

// Not rewritten: `to_string` on a `&str`, a `String` and a number.
pub fn others(s: &str, owned: String, n: u32) -> (String, String, String) {
    (s.to_string(), owned.to_string(), n.to_string())
}

// Not rewritten: the `Cow` is borrowed, so it cannot be moved out of.
pub fn borrowed(text: &Cow<'_, str>) -> String {
    text.to_string()
}

pub fn len_of(path: &Path) -> usize {
    shown(path).len()
}
