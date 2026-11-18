//! A package's `[package]` metadata as cargo gives it to the crates it compiles: the
//! `CARGO_PKG_*` metadata family (the cargo book, "Environment variables Cargo sets for
//! crates"), each a function of the package's manifest and, for an inherited key, of its
//! workspace root's `[workspace.package]`. The ONE table of these names and values (rule 30):
//! blaze-scope's `CrateEnv` (what `env!`, `option_env!` and a proc macro's `getenv` read) and
//! blaze-build-script's environment (what a build script reads) both take [`PackageMetaVars`].
//!
//! A key the manifest does not state is the EMPTY string: cargo sets every one of these for
//! every crate, `""` when the manifest has none (cargo's `compilation.rs` `fill_env`:
//! `metadata.description.as_ref().unwrap_or_default()`, and so for each). That is knowing,
//! not guessing: `option_env!("CARGO_PKG_DESCRIPTION")` is `Some("")` under cargo.
//!
//! Each value, as cargo's manifest normalization (`util/toml/mod.rs`) makes it:
//! - `authors`: the list joined with `:` (`authors.join(":")`);
//! - `readme`: the path as written; `readme = true` is `README.md`, `readme = false` is `""`;
//!   unstated, the first of `README.md`, `README.txt`, `README` that is a file in the package's
//!   directory (`default_readme_from_package_root`), else `""`;
//! - `readme` and `license-file` inherited with `workspace = true`: the root's path made
//!   relative to the package's directory (`resolve_relative_path`: `../../README.md` for a
//!   member at `crates/a` of a root with `readme = "README.md"`);
//! - every other key: the string as written (`rust-version` too: `1.70` stays `1.70`).

use blaze_own::{Alloc, String};

use super::Text;

/// The `[package]` keys whose values cargo gives a crate as `CARGO_PKG_*`, in the order of
/// [`PackageMeta`]'s values and [`PackageMetaVars::NAMES`].
pub const META_KEYS: [&str; 8] = ["authors", "description", "homepage", "license", "license-file", "readme", "repository", "rust-version"];

/// The index of `license-file` in [`META_KEYS`].
pub(super) const LICENSE_FILE: usize = 4;
/// The index of `readme` in [`META_KEYS`].
pub(super) const README: usize = 5;
/// The index of `authors` in [`META_KEYS`].
pub(super) const AUTHORS: usize = 0;

/// The files cargo takes as a package's readme when `readme` is unstated, in its order
/// (`DEFAULT_README_FILES`).
pub(super) const DEFAULT_READMES: [&str; 3] = ["README.md", "README.txt", "README"];

/// A package's metadata values, in [`META_KEYS`]' order: see the module's doc. `""` is a key
/// the manifest does not state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PackageMeta<'a> {
    values: [Text<'a>; 8],
}

impl<'a> PackageMeta<'a> {
    /// The metadata of these values, in [`META_KEYS`]' order.
    pub fn new(values: [Text<'a>; 8]) -> PackageMeta<'a> {
        PackageMeta { values }
    }

    /// The metadata of a package that states none of the keys: every value `""`.
    pub fn unstated(alloc: Alloc<'a>) -> PackageMeta<'a> {
        PackageMeta { values: core::array::from_fn(|_| String::new_in(alloc)) }
    }

    /// The values, in [`META_KEYS`]' order.
    pub fn values(&self) -> &[Text<'a>; 8] {
        &self.values
    }

    /// The values as the variables a crate reads them by.
    pub fn vars(&self) -> PackageMetaVars<'_> {
        PackageMetaVars::from_values(core::array::from_fn(|i| self.values[i].as_str()))
    }

    /// This metadata copied into `alloc`.
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> PackageMeta<'b> {
        PackageMeta { values: core::array::from_fn(|i| String::from_str_in(self.values[i].as_str(), alloc)) }
    }
}

/// A package's metadata borrowed, by the variable each value is: the table every reader of
/// `CARGO_PKG_*` metadata takes ([`PackageMetaVars::pairs`]). `""` is a key the manifest does
/// not state (cargo sets the variable empty).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PackageMetaVars<'s> {
    /// `CARGO_PKG_AUTHORS`: the `authors` joined with `:`.
    pub authors: &'s str,
    /// `CARGO_PKG_DESCRIPTION`.
    pub description: &'s str,
    /// `CARGO_PKG_HOMEPAGE`.
    pub homepage: &'s str,
    /// `CARGO_PKG_LICENSE`.
    pub license: &'s str,
    /// `CARGO_PKG_LICENSE_FILE`.
    pub license_file: &'s str,
    /// `CARGO_PKG_README`.
    pub readme: &'s str,
    /// `CARGO_PKG_REPOSITORY`.
    pub repository: &'s str,
    /// `CARGO_PKG_RUST_VERSION`.
    pub rust_version: &'s str,
}

impl<'s> PackageMetaVars<'s> {
    /// The variables, in [`META_KEYS`]' order.
    pub const NAMES: [&'static str; 8] = [
        "CARGO_PKG_AUTHORS",
        "CARGO_PKG_DESCRIPTION",
        "CARGO_PKG_HOMEPAGE",
        "CARGO_PKG_LICENSE",
        "CARGO_PKG_LICENSE_FILE",
        "CARGO_PKG_README",
        "CARGO_PKG_REPOSITORY",
        "CARGO_PKG_RUST_VERSION",
    ];

    /// The variables of these values, in [`META_KEYS`]' order.
    pub fn from_values(v: [&'s str; 8]) -> PackageMetaVars<'s> {
        PackageMetaVars {
            authors: v[0],
            description: v[1],
            homepage: v[2],
            license: v[3],
            license_file: v[4],
            readme: v[5],
            repository: v[6],
            rust_version: v[7],
        }
    }

    /// The values, in [`META_KEYS`]' order.
    pub fn values(self) -> [&'s str; 8] {
        [self.authors, self.description, self.homepage, self.license, self.license_file, self.readme, self.repository, self.rust_version]
    }

    /// Each variable with its value, in [`Self::NAMES`]' order.
    pub fn pairs(self) -> [(&'static str, &'s str); 8] {
        let values = self.values();
        core::array::from_fn(|i| (Self::NAMES[i], values[i]))
    }
}

/// The path from the anchor-relative directory `from` to the anchor-relative path `to` (both
/// normalized, `/`-separated, `""` the anchor): a `..` per component of `from` past their
/// common prefix, then the rest of `to`. What cargo's `resolve_relative_path` gives an
/// inherited `readme` or `license-file` (`pathdiff::diff_paths(ws_root.join(path),
/// package_root)`).
pub(super) fn relative_to<'a>(to: &str, from: &str, alloc: Alloc<'a>) -> Text<'a> {
    let parts = |s: &'_ str| s.split('/').filter(|p| !p.is_empty()).count();
    let mut common = 0;
    for (a, b) in to.split('/').filter(|p| !p.is_empty()).zip(from.split('/').filter(|p| !p.is_empty())) {
        if a != b {
            break;
        }
        common += 1;
    }
    let mut out = String::new_in(alloc);
    for _ in common..parts(from) {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str("..");
    }
    for part in to.split('/').filter(|p| !p.is_empty()).skip(common) {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(part);
    }
    out
}
