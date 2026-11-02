//! A crate root as the compiler sees it, which is not the file on disk: the root's text with a
//! module the compiler appends after it, a test crate's harness ([`Lowered::append_roots`]).
//! What the module is, is the caller's: this crate writes it into the front end, into the root
//! of the crate's own ([`base_db::Db::alias`] of the file on disk, which [`super::lower`] gives
//! every crate whose root is appended to), so no other crate of that file reads it.

use base_db::{Crate, FileText};
use blaze_hash::HashSet;
use hir_expand::proc_macro::ProcMacros;

use super::{ContentId, Db, LowerError, Lowered};

/// What a crate's root is to be: the file on disk, then `after`, the module named `module`.
#[derive(Debug)]
pub struct Appended {
    pub after: String,
    pub module: &'static str,
}

impl Lowered {
    /// The root of the crate of each of `units` made what `wanted` says it is for that crate
    /// (`None`: left as it is): its own alias of the file on disk, which has the file's text
    /// and then the module, with the module set as the root's appended module, so a root that
    /// `#![cfg]` takes away still has it. Both are in the root's content ID, so the crate's
    /// identity follows; the file on disk and every other crate of it are left as they are.
    /// An edit of the file keeps the module after the new text until it is made again.
    /// Returns how many roots were written: none when each already is what is wanted.
    ///
    /// Units of one crate (a check of tests beside a test of the same cfg) must want the
    /// same, or are refused naming both: in one call, and across calls until the next
    /// [`Lowered::refresh`], so a root made for one unit is never changed under it for
    /// another. A unit given again (once its proc macros load, [`macros_loaded`]) may make its
    /// root again, and so may the others of that crate given with it. A crate whose root is
    /// not an alias is refused.
    ///
    /// A write, as [`Lowered::ran`] is: call it before [`Lowered::identity`] of these units.
    pub fn append_roots(
        &mut self,
        units: impl IntoIterator<Item = u32>,
        mut wanted: impl FnMut(&Db, usize, Crate) -> Result<Option<Appended>, LowerError>,
    ) -> Result<usize, LowerError> {
        // alloc: once a call, the units given and the roots made in it
        let units: Vec<u32> = units.into_iter().collect();
        let mut made: HashSet<base_db::FileId> = HashSet::default();
        let mut written = 0;
        for &unit in &units {
            let unit = unit as usize;
            let Some(krate) = self.crates.krate(unit) else { continue };
            let Some(appended) = wanted(&self.db, unit, krate)? else { continue };
            let file = krate.data(&self.db).root_file_id;
            let asked = FileText::content_of(&appended.after, Some(appended.module));
            match self.appended.get(&file) {
                Some(&(_, was)) if was == asked => continue,
                Some(&(by, _)) if made.contains(&file) || !units.contains(&(by as u32)) => {
                    return Err(self.two_roots(by, unit, file));
                }
                _ => {}
            }
            let before = self.db.file_text(file).content(&self.db);
            self.db.set_alias_appended(file, Some((appended.module, &appended.after))).map_err(|error| {
                let what = &self.crates.units[unit].what;
                // alloc: the error's message
                LowerError::at(format!("the root of {what} can't have a module appended: {error}"), what.clone())
            })?;
            made.insert(file);
            self.appended.insert(file, (unit, asked));
            if self.db.file_text(file).content(&self.db) != before {
                written += 1;
            }
        }
        if written > 0 {
            self.identities.clear();
        }
        Ok(written)
    }

    fn two_roots(&self, first: usize, second: usize, file: base_db::FileId) -> LowerError {
        let (first, second) = (&self.crates.units[first].what, &self.crates.units[second].what);
        // alloc: the error's place and message
        let at = self.paths.get(&file).cloned().unwrap_or_else(|| format!("{file:?}"));
        LowerError::at(
            // alloc: as `at`
            format!(
                "{first} and {second} are one crate of the root file `{at}` and would each have its root end with \
                 another test harness, which one root can't"
            ),
            at,
        )
    }
}

/// Whether every proc macro `krate` reaches through its dependencies has its expanders loaded
/// ([`Lowered::macros`]): until then what its attributes and derives expand to (a
/// `#[tokio::test]`'s `#[test]`) is not in its items, so nothing listing them is made yet. A
/// proc macro that failed to load never is: the crate's build stops on it.
pub fn macros_loaded(db: &Db, krate: Crate) -> bool {
    let by_crate = ProcMacros::get(db).by_crate(db);
    // alloc: once a test crate a write, its closure walked
    let mut seen: HashSet<Crate> = HashSet::default();
    let mut stack = vec![krate]; // alloc: as `seen`
    while let Some(at) = stack.pop() {
        for dep in &at.data(db).dependencies {
            if !seen.insert(dep.crate_id) {
                continue;
            }
            if by_crate.get(&dep.crate_id).is_some_and(|it| it.macros().is_none()) {
                return false;
            }
            stack.push(dep.crate_id);
        }
    }
    true
}
