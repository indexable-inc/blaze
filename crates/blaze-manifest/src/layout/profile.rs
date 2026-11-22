//! The one profile setting the build of a program takes today: `overflow-checks` of the profile a
//! command builds with (decision D101), read from the workspace root's manifest text. Cargo's
//! profiles reference ("Profiles", "Default profiles"): `dev` has `overflow-checks = true` and
//! `release` has `overflow-checks = false`; a `[profile.<name>]` table in the root manifest
//! overrides it. Profiles in a member's manifest are not read (Cargo: "profiles ... are only
//! read from the root of the workspace").
//!
//! Not read here, and so never wrong here: `profile.<name>.package.<spec>` overrides (they set
//! a dependency's own setting, and its record is keyed by it), the configuration's `profile`
//! tables and `CARGO_PROFILE_<NAME>_OVERFLOW_CHECKS` (the command takes neither yet).

use blaze_own::{Alloc, String};
use blaze_toml::{Document, Kind, Limits};

use super::{LayoutError, ManifestId, Span};

/// A built-in profile a command builds with: `run` and `test` take `Dev`, `export` takes
/// `Release`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuiltinProfile {
    Dev,
    Release,
}

impl BuiltinProfile {
    /// The table's name under `[profile]`.
    pub const fn name(self) -> &'static str {
        match self {
            BuiltinProfile::Dev => "dev",
            BuiltinProfile::Release => "release",
        }
    }

    /// The profile's `overflow-checks` when the manifest does not set it.
    pub const fn default_overflow_checks(self) -> bool {
        match self {
            BuiltinProfile::Dev => true,
            BuiltinProfile::Release => false,
        }
    }
}

/// Whether integer overflow is checked in `profile`, by the workspace root's manifest `text`
/// (whose path, for an error, is `path`; its id `id`): the table's `overflow-checks` when it
/// is there, else the profile's built-in value. A value that is not a bool, or a `profile` or
/// `profile.<name>` that is not a table, is refused naming its line.
pub fn overflow_checks<'a>(text: &str, id: ManifestId, path: &str, profile: BuiltinProfile, alloc: Alloc<'a>) -> Result<bool, LayoutError<'a>> {
    let doc = match Document::parse_in(text, Limits::CARGO, alloc) {
        Ok(doc) => doc,
        Err(error) => return Err(LayoutError::Toml { manifest: String::from_str_in(path, alloc), error }),
    };
    let at = |span: core::ops::Range<usize>| Span::new(id, span.start, span.end);
    let Some(profiles) = doc.root().get("profile") else {
        return Ok(profile.default_overflow_checks());
    };
    if profiles.kind() != Kind::Table {
        return Err(LayoutError::Expected { at: at(profiles.span()), key: "profile", kind: "a table" });
    }
    let Some(table) = profiles.get(profile.name()) else {
        return Ok(profile.default_overflow_checks());
    };
    if table.kind() != Kind::Table {
        return Err(LayoutError::Expected { at: at(table.span()), key: "profile.<name>", kind: "a table" });
    }
    match table.get("overflow-checks") {
        None => Ok(profile.default_overflow_checks()),
        Some(item) => match item.as_bool() {
            Some(on) => Ok(on),
            None => Err(LayoutError::Expected { at: at(item.span()), key: "profile.<name>.overflow-checks", kind: "a bool" }),
        },
    }
}
