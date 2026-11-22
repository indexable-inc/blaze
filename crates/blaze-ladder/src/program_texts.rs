//! The program's own crate as the names library takes it: its files under the package's
//! directory, as written (D104: nothing is appended; the root's `fn main` is `std::main`'s
//! input, blaze-scope `IMPLICIT_MAIN`), its edition. The first
//! step of `run::build_new`, apart so a runner that resolves the toolchain library once
//! (`crates::library`) makes it per program.

use blaze_build::OverflowChecks;
use edition::PackageEdition;

use crate::crates::{Toolchain, UserCrate};
use crate::refusal::SourceFile;
use crate::run::{NewFront, RunError};

/// One program's texts, owned: what [`UserCrate`] borrows.
pub struct ProgramTexts {
    package: String,
    /// `/<package>/`.
    dir: String,
    /// Each file by its path under `dir`, as written.
    files: Vec<(String, String)>,
    root: String,
    edition: PackageEdition,
    /// The profile's overflow setting ([`NewFront::overflow`]), which the build takes.
    pub(crate) overflow: OverflowChecks,
    /// The program's one panic strategy, as its maker named it: what [`ProgramTexts::user`]
    /// carries into the program's inputs ([`UserCrate::panic`]).
    panic: blaze_build::Panic,
}

impl ProgramTexts {
    /// The texts of the program `files` rooted at `root`, package and edition as `new` says,
    /// read by the names library under `tc` (its cfg is the crate's, where the names read it).
    /// `panic` is the program's strategy: the record path's key's (run.rs `build_records_new`),
    /// or what the caller names; there is no default.
    pub fn new(files: &[SourceFile<'_>], root: &str, new: NewFront<'_>, tc: &Toolchain, panic: blaze_build::Panic) -> Result<ProgramTexts, RunError> {
        let _ = tc;
        let dir = format!("/{}/", new.package);
        let owned: Vec<(String, String)> = files.iter().map(|f| (format!("{dir}{}", f.path), f.text.into())).collect();
        let root_at = format!("{dir}{root}");
        let edition = PackageEdition::of_manifest(new.edition).map_err(|_| RunError::Edition { written: new.edition.into() })?;
        if !files.iter().any(|f| f.path == root) {
            return Err(RunError::NoMain);
        }
        Ok(ProgramTexts { package: new.package.into(), dir, files: owned, root: root_at, edition, overflow: new.overflow, panic })
    }

    /// The program's crate over these texts.
    pub fn user(&self) -> UserCrate<'_> {
        UserCrate { name: &self.package, dir: &self.dir, root: &self.root, files: &self.files, edition: self.edition, panic: self.panic }
    }
}
