//! A unit's view of a body (docs/v2/plan/design/test.md, "The order as built (D191)").
//!
//! A body in a file that several units hold (a package's library and its library compiled for
//! tests; a library and a binary that both `mod` one file) is CHECKED by every one of them: each
//! unit's keys are its own (every `ItemKey` starts with its unit's crate key), so a row checked
//! under one unit names nothing another unit's code can call. The first holder in rank (a
//! non-test unit before a test one, then the crate list's order) keeps the body's plain name,
//! the BASE row; each other holder's row is the name AT ITS VIEW ([`at_view`]). Run and test
//! build a unit's body-id table by asking the name at that unit's view, and a missing view row is
//! a typed refusal there, never the base's row: the base's keys are another crate's.
//!
//! Which holders get a row of their own is a strategy at one seam (rule 78), [`ViewPlan`]:
//!
//! - [`Full`], the one built (D191): every holder checks; the first is the base, each other is
//!   at its own view. Correct on today's keys, where a test build's crate key is not its crate's.
//! - `Lifted` (test.md D152 point 5), NOT written here: a view row only where its reads differ
//!   from the base's, every other read falling through to the base row. It needs one crate key
//!   for a crate and its test build (D152 points 2 and 1), so that a base row's keys ARE the
//!   view's; before that a fall-through hands a unit another crate's keys. Its oracle is `Full`,
//!   in the engine's simulation. No stub stands in for it: a type that said "lifted" and planned
//!   rows as `Full` does would lie about what it checks.
//!
//! The one home of a view (rule 30): nothing else spells one.

use blaze_id::ContentId;
use blaze_ids::Hasher;

use crate::key::Name;

/// The domain of a member unit's view ([`ViewKey::of_member`]).
const MEMBER_VIEW_DOMAIN: &str = "blaze-drive member view 2";
/// The domain of a name at a view ([`at_view`]): not the view's own, so a view's key and a name
/// at it are never one digest.
const AT_VIEW_DOMAIN: &str = "blaze-drive name at view 1";

/// The unit a row was checked under, where that unit is not the body's base holder: WHAT THE
/// UNIT IS, never what it depends on (D152 point 1's (crate, `test`)). One home: nothing else
/// spells a view.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ViewKey(pub ContentId);

impl ViewKey {
    /// The view of the member unit whose root is `root` (the crate list's root path, in the
    /// checkout's tree: `/0/a/src/lib.rs`, never a machine path: rule 25), whose crate is named
    /// `name` (the target's name: the library's, a bin's, an integration test's) and which is a
    /// test build or not. Not the unit's crate key: that key held every dependency's key, so a
    /// dependency bumped or an edge added renamed every view row and moved every shared body's
    /// row key (`source_at_views`), entering bodies that read nothing of it (rule 77). Since D158
    /// the crate key is the identity alone (blaze-world list.rs `unit_crate_key`); the view stays
    /// by (root, name, test), which a source correction or a member's move does not rename.
    /// Two member units never share (root, name, test): that triple is what tells them apart.
    pub fn of_member(root: &[u8], name: &[u8], test: bool) -> ViewKey {
        let mut h = Hasher::new(MEMBER_VIEW_DOMAIN);
        h.update(&[u8::from(test)]).part(root).part(name);
        ViewKey(h.finish())
    }
}

/// Where a holder's row of a body is: the body's plain name, or its name at the holder's view.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum UnitView {
    /// The body's first holder in rank: its row is the plain name.
    Base,
    /// Another holder: its row is the name at this view.
    At(ViewKey),
}

/// `base` in `view`: `base` itself under `Base` (a body one unit holds costs nothing new);
/// under `At(v)` a name of the same kind whose subject is H(domain, kind, base.subject, v).
/// A function of (kind, subject, view) alone, as [`crate::key::at_condition`] is of (kind,
/// subject, condition): the two are the drive's two ways a name is taken somewhere else.
pub fn at_view(base: Name, view: UnitView) -> Name {
    match view {
        UnitView::Base => base,
        UnitView::At(v) => {
            let mut h = Hasher::new(AT_VIEW_DOMAIN);
            h.update(&[base.kind as u8]).id(&base.subject).id(&v.0);
            Name { kind: base.kind, subject: h.finish() }
        }
    }
}

/// Which rows a body's holders get (rule 78). `holders` in rank order, never empty; `at` is an
/// index into it.
pub trait ViewPlan {
    /// The strategy's name, as a profile or a refusal prints it.
    const NAME: &'static str;
    /// The view of the holder at `at`.
    fn view_of(holders: &[ViewKey], at: usize) -> UnitView;
}

/// Every holder checks: the first is the base, each other is at its own view (D191).
pub struct Full;

impl ViewPlan for Full {
    const NAME: &'static str = "full";

    fn view_of(holders: &[ViewKey], at: usize) -> UnitView {
        match at {
            0 => UnitView::Base,
            // `at < holders.len()` is the caller's promise (the trait's doc): an index past the
            // holders is a planner bug, and the panic fails that job alone (rule 20).
            _ => UnitView::At(holders[at]),
        }
    }
}

/// The plan built today. `Lifted` replaces it once a crate and its test build have one key.
pub type Production = Full;

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
