//! The program's own crate as the names library takes it: its files under the package's
//! directory, its root with the entry module appended (D59's interim), its edition. The first
//! step of `run::build_new`, apart so a runner that resolves the toolchain library once
//! (`crates::library`) makes it per program.

use blaze_build::OverflowChecks;
use edition::PackageEdition;

use crate::crates::{Toolchain, UserCrate, syntax_of};
use crate::front2::entry::{self, Appended};
use crate::refusal::SourceFile;
use crate::run::{NewFront, RunError};

/// One program's texts, owned: what [`UserCrate`] borrows.
pub struct ProgramTexts {
    package: String,
    /// `/<package>/`.
    dir: String,
    /// Each file by its path under `dir`; the root's text has the entry module appended.
    files: Vec<(String, String)>,
    root: String,
    edition: PackageEdition,
    /// The root's index in the files given.
    pub(crate) root_file: usize,
    /// What was appended to the root, if anything.
    pub(crate) appended: Option<Appended>,
    /// The profile's overflow setting ([`NewFront::overflow`]), which the build takes.
    pub(crate) overflow: OverflowChecks,
}

impl ProgramTexts {
    /// The texts of the program `files` rooted at `root`, package and edition as `new` says,
    /// its entry chosen under `tc`'s cfg for the program's crate.
    pub fn new(files: &[SourceFile<'_>], root: &str, new: NewFront<'_>, tc: &Toolchain) -> Result<ProgramTexts, RunError> {
        let dir = format!("/{}/", new.package);
        let mut owned: Vec<(String, String)> = files.iter().map(|f| (format!("{dir}{}", f.path), f.text.into())).collect();
        let root_at = format!("{dir}{root}");
        let edition = PackageEdition::of_manifest(new.edition).map_err(|_| RunError::Edition { written: new.edition.into() })?;
        // D59's interim: the entry module is appended to the root's text here, before names
        // resolve; the names library reads (and keys) the root from these files (crates.rs
        // `Texts`).
        let root_file = files.iter().position(|f| f.path == root).ok_or(RunError::NoMain)?;
        let appended = entry::append(&mut owned[root_file].1, syntax_of(edition), edition.blaze.is_some(), &tc.program_cfg())
            .map_err(|e| RunError::Entry { file: root_file as u32, range: e.range(), why: format!("{e}").into_boxed_str() })?;
        Ok(ProgramTexts { package: new.package.into(), dir, files: owned, root: root_at, edition, root_file, appended, overflow: new.overflow })
    }

    /// The program's crate over these texts.
    pub fn user(&self) -> UserCrate<'_> {
        UserCrate { name: &self.package, dir: &self.dir, root: &self.root, files: &self.files, edition: self.edition }
    }
}
