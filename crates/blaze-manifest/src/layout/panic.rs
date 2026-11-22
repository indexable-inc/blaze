//! The profile's `panic` key: the program's ONE panic strategy (UNWIND.md 3.1), read from the
//! workspace root's manifest text beside `overflow-checks` ([`super::profile`], decision D101).
//!
//! Rust's rule: a profile with no `panic` key unwinds. Cargo's profiles reference ("Profiles",
//! `panic`): "The default when not specified is `"unwind"`", for every built-in profile; the
//! valid stable values are `"unwind"` and `"abort"`. `"immediate-abort"` is unstable and only
//! means anything when the library itself is rebuilt with it (`-Zbuild-std`), which blaze does
//! not do: it is refused by name. Any other value is refused naming the manifest, the profile and
//! the value.
//!
//! Inheritance, by Cargo's rules: `dev` and `release` are root profiles (Cargo: "`inherits` must
//! not be specified in root profile"), so their `panic` is their own table's or the default; an
//! `inherits` key in either is refused by name (else a `release` written to inherit a `dev` that
//! aborts would quietly unwind). The profiles that inherit (`test` from `dev`, `bench` from
//! `release`, a custom profile from its `inherits`) are not profiles a command builds with yet
//! ([`BuiltinProfile`] has no such variant), so nothing here walks a chain.
//!
//! Cargo refuses `panic` in a `profile.<name>.package.<spec>` override ("`panic` may not be
//! specified in a `package` profile"): the strategy is the whole program's (REF linkage.html
//! [link.unwinding.prohibited]); it is refused here by name too.
//!
//! Not read here, and so never wrong here (as for `overflow-checks`): the configuration's
//! `profile` tables and `CARGO_PROFILE_<NAME>_PANIC` (the command takes neither yet).

use core::fmt;

use blaze_own::{Alloc, String};
use blaze_toml::{Document, Kind, Limits};

use super::profile::BuiltinProfile;
use super::{LayoutError, ManifestId, Span, Text};

#[cfg(test)]
#[path = "panic_tests.rs"]
mod panic_tests;

/// The strategy a profile's `panic` names, as the manifest writes it. The builder's own enum
/// (blaze-build `Panic`) is mapped from it by blaze-ladder `panic_strategy::of_profile`; this
/// crate cannot name it (blaze-manifest is under the builder, not over it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProfilePanic {
    /// `panic = "unwind"`, or no `panic` key (Rust's rule).
    Unwind,
    /// `panic = "abort"`.
    Abort,
}

/// Why a profile's `panic` is not read: the manifest (its path), the profile and the value.
#[derive(Debug)]
pub enum PanicError<'a> {
    /// The manifest is no TOML, or `profile`, `profile.<name>` or `panic` is the wrong kind.
    Layout(LayoutError<'a>),
    /// `panic = "immediate-abort"`: a `-Zbuild-std` strategy blaze does not build.
    ImmediateAbort { manifest: Text<'a>, profile: &'static str, at: Span },
    /// A value Cargo's profiles do not define.
    Unknown { manifest: Text<'a>, profile: &'static str, written: Text<'a>, at: Span },
    /// `inherits` in a root profile (`dev`, `release`).
    InheritsInRoot { manifest: Text<'a>, profile: &'static str, at: Span },
    /// `panic` in a `profile.<name>.package.<spec>` override.
    InPackageOverride { manifest: Text<'a>, profile: &'static str, package: Text<'a>, at: Span },
}

impl fmt::Display for PanicError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = |f: &mut fmt::Formatter<'_>, manifest: &Text<'_>, span: &Span| write!(f, "{} bytes {}..{}: ", manifest.as_str(), span.start, span.end);
        match self {
            PanicError::Layout(error) => write!(f, "{error}"),
            PanicError::ImmediateAbort { manifest, profile, at: span } => {
                at(f, manifest, span)?;
                write!(f, "`profile.{profile}.panic = \"immediate-abort\"` is a `-Zbuild-std` strategy blaze does not build")
            }
            PanicError::Unknown { manifest, profile, written, at: span } => {
                at(f, manifest, span)?;
                write!(f, "`profile.{profile}.panic = \"{}\"` is no panic strategy: it is `\"unwind\"` or `\"abort\"`", written.as_str())
            }
            PanicError::InheritsInRoot { manifest, profile, at: span } => {
                at(f, manifest, span)?;
                write!(f, "`profile.{profile}.inherits`: `{profile}` is a root profile and inherits from none (Cargo's profiles)")
            }
            PanicError::InPackageOverride { manifest, profile, package, at: span } => {
                at(f, manifest, span)?;
                write!(
                    f,
                    "`profile.{profile}.package.{}.panic`: the panic strategy is the whole program's and may not be set for one package",
                    package.as_str()
                )
            }
        }
    }
}

impl core::error::Error for PanicError<'_> {}

/// The panic strategy of `profile`, by the workspace root's manifest `text` (whose path, for an
/// error, is `path`; its id `id`): the table's `panic` when it is there, else `unwind` (Rust's
/// rule: a profile with no `panic` key unwinds). A value other than `"unwind"` or `"abort"`, an
/// `inherits` in the root profile, a `panic` in a package override, or a `profile`,
/// `profile.<name>` or `panic` of the wrong kind is refused naming the manifest and the key.
pub fn panic_strategy<'a>(text: &str, id: ManifestId, path: &str, profile: BuiltinProfile, alloc: Alloc<'a>) -> Result<ProfilePanic, PanicError<'a>> {
    let doc = match Document::parse_in(text, Limits::CARGO, alloc) {
        Ok(doc) => doc,
        Err(error) => return Err(PanicError::Layout(LayoutError::Toml { manifest: String::from_str_in(path, alloc), error })),
    };
    let at = |span: core::ops::Range<usize>| Span::new(id, span.start, span.end);
    let manifest = || String::from_str_in(path, alloc);
    let name = profile.name();
    let Some(profiles) = doc.root().get("profile") else {
        return Ok(ProfilePanic::Unwind);
    };
    if profiles.kind() != Kind::Table {
        return Err(PanicError::Layout(LayoutError::Expected { at: at(profiles.span()), key: "profile", kind: "a table" }));
    }
    let Some(table) = profiles.get(name) else {
        return Ok(ProfilePanic::Unwind);
    };
    if table.kind() != Kind::Table {
        return Err(PanicError::Layout(LayoutError::Expected { at: at(table.span()), key: "profile.<name>", kind: "a table" }));
    }
    if let Some(inherits) = table.get("inherits") {
        return Err(PanicError::InheritsInRoot { manifest: manifest(), profile: name, at: at(inherits.span()) });
    }
    if let Some(packages) = table.get("package") {
        if packages.kind() != Kind::Table {
            return Err(PanicError::Layout(LayoutError::Expected { at: at(packages.span()), key: "profile.<name>.package", kind: "a table" }));
        }
        for package in packages.children() {
            if let Some(panic) = package.get("panic") {
                return Err(PanicError::InPackageOverride {
                    manifest: manifest(),
                    profile: name,
                    package: String::from_str_in(package.key(), alloc),
                    at: at(panic.span()),
                });
            }
        }
    }
    let Some(item) = table.get("panic") else {
        return Ok(ProfilePanic::Unwind);
    };
    match item.as_str() {
        Some("unwind") => Ok(ProfilePanic::Unwind),
        Some("abort") => Ok(ProfilePanic::Abort),
        Some("immediate-abort") => Err(PanicError::ImmediateAbort { manifest: manifest(), profile: name, at: at(item.span()) }),
        Some(written) => Err(PanicError::Unknown { manifest: manifest(), profile: name, written: String::from_str_in(written, alloc), at: at(item.span()) }),
        None => Err(PanicError::Layout(LayoutError::Expected { at: at(item.span()), key: "profile.<name>.panic", kind: "a string" })),
    }
}
