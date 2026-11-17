//! `run`'s scope: the program package and the workspace members it depends on (G2 step (a)).
//!
//! A `check` asks every member's crates; a `run` asks only the crates its program can reach. A
//! member's `Units` record lists its crate roots (`FileItems` parts) and, once its layout says so,
//! the `Units` of each workspace member it depends on. The closure starts at the program
//! package's `Units` and follows those listings; the crate roots the closure's records list are
//! the roots `Request::release` asks. A file owned only by other members keeps its bodies held,
//! never made, while the closure may still grow (a record that lists one more member, settled
//! later in the walk), and unmade at the end.
//!
//! There is no table of the program here beyond the request: the closure is rebuilt from the
//! records the walk settles, standing or made, so a manifest edit that adds a dependency moves
//! that member's record, which lists the new member, and its bodies are released before the
//! start.

use blaze_own::{AVec, Alloc, CollectIn, Vec};

use crate::key::{Kind, Name};

/// The members a `run` asks and the crate roots they list.
pub(crate) struct Closure<'a> {
    alloc: Alloc<'a>,
    /// The program package's units (its manifest's `Units` name).
    package: Name,
    /// Each settled `Units` record's `Units` and `FileItems` parts, by record, sorted by name.
    listed: AVec<'a, (Name, AVec<'a, Name>)>,
    /// The members in the closure, sorted.
    members: AVec<'a, Name>,
    /// The crate roots their records list, sorted.
    roots: AVec<'a, Name>,
    /// PLANT (`Plant::ReachedSkipped`): the first dependency met is not followed.
    skip_first: bool,
}

impl<'a> Closure<'a> {
    pub(crate) fn new_in(package: Name, skip_first: bool, alloc: Alloc<'a>) -> Closure<'a> {
        let mut members = Vec::new_in(alloc);
        members.push(package);
        Closure { alloc, package, listed: Vec::new_in(alloc), members, roots: Vec::new_in(alloc), skip_first }
    }

    /// The `Units` record `units` settled listing `parts`: `true` when the closure's crate roots
    /// grew. A record settled again (dirtied this request) lists its new parts; what an earlier
    /// listing brought in stays (a superset: more is checked, never less).
    pub(crate) fn listed(&mut self, units: Name, parts: &[Name]) -> bool {
        let mine: AVec<'a, Name> =
            parts.iter().filter(|part| matches!(part.kind, Kind::Units | Kind::FileItems) && **part != units).copied().collect_in(self.alloc);
        match self.listed.binary_search_by_key(&units, |(name, _)| *name) {
            Ok(at) => self.listed[at].1 = mine,
            Err(at) => self.listed.insert(at, (units, mine)),
        }
        let before = self.roots.len();
        if self.members.binary_search(&units).is_ok() {
            self.enter(units);
        }
        self.roots.len() != before
    }

    /// The program package's units.
    pub(crate) fn package(&self) -> Name {
        self.package
    }

    /// Whether the program package's own record was listed: without it the closure asks
    /// nothing, and a start over it would be a start over no check (`Request::finish` fails
    /// the request by its name).
    pub(crate) fn found(&self) -> bool {
        self.listed.binary_search_by_key(&self.package, |(name, _)| *name).is_ok()
    }

    /// Whether the crate root `root` is one a member of the closure lists.
    pub(crate) fn asks(&self, root: Name) -> bool {
        self.roots.binary_search(&root).is_ok()
    }

    /// `units` is in the closure: its listed roots are asked, and each member it lists is in
    /// the closure too, transitively. Each member is entered once, so this is linear in the
    /// listings.
    fn enter(&mut self, units: Name) {
        let mut stack: AVec<'a, Name> = Vec::new_in(self.alloc);
        stack.push(units);
        while let Some(member) = stack.pop() {
            let Ok(at) = self.listed.binary_search_by_key(&member, |(name, _)| *name) else { continue };
            for j in 0..self.listed[at].1.len() {
                let part = self.listed[at].1[j];
                match part.kind {
                    Kind::FileItems => {
                        if let Err(to) = self.roots.binary_search(&part) {
                            self.roots.insert(to, part);
                        }
                    }
                    _ if part == self.package => {}
                    _ => {
                        let Err(to) = self.members.binary_search(&part) else { continue };
                        if self.skip_first {
                            // PLANT (`Plant::ReachedSkipped`): this dependency is not followed.
                            self.skip_first = false;
                            continue;
                        }
                        self.members.insert(to, part);
                        stack.push(part);
                    }
                }
            }
        }
    }
}
