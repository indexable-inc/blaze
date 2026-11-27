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

use blaze_id::{ContentId, CrateItemKey};
use blaze_ids::Hasher;

use crate::key::Name;

/// The domain of a member unit's view ([`ViewKey::of_member`]).
/// 3: folds the unit's side (`macro_side`), target kind flags (`proc_macro`, `test_or_bench`),
/// binary target name (`bin`), and package name (`package`) so two member units sharing
/// `(root, name, test)` — such as a library compiled for both `Side::Target` and `Side::Macro`
/// in a member proc-macro's dependency closure — never collide on one view key (D191, rule 21).
const MEMBER_VIEW_DOMAIN: &str = "blaze-drive member view 3";
/// The domain for disambiguating two member units whose [`MemberView`] fields still match
/// ([`ViewKey::disambiguate`], e.g. a lib and an example sharing one file and crate name).
const MEMBER_VIEW_DISAMBIGUATE_DOMAIN: &str = "blaze-drive member view disambiguate 1";
/// The domain of a name at a view ([`at_view`]): not the view's own, so a view's key and a name
/// at it are never one digest.
const AT_VIEW_DOMAIN: &str = "blaze-drive name at view 1";

/// Identity of a workspace member unit for [`ViewKey::of_member`]: every fact that tells two
/// member units apart without naming their dependencies, so adding or bumping a dependency edge
/// never renames any unit's view rows (rule 77).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemberView<'a> {
    /// The crate list's root path in the checkout tree (`/0/...`, never a machine path: rule 25).
    pub root: &'a [u8],
    /// The target's crate name.
    pub name: &'a [u8],
    /// Whether `cfg(test)` is set on this unit.
    pub test: bool,
    /// Whether this unit is built for the macro side (`wasm32-wasip1`, `Side::Macro`).
    pub macro_side: bool,
    /// Whether this unit is a proc-macro crate (`proc-macro = true`).
    pub proc_macro: bool,
    /// Whether this unit is an integration test or benchmark target.
    pub test_or_bench: bool,
    /// The binary target name for `Bin` / `Test { bin: Some(..) }` units, `None` otherwise.
    pub bin: Option<&'a [u8]>,
    /// The Cargo package name (`b""` if none).
    pub package: &'a [u8],
}

impl<'a> MemberView<'a> {
    /// Minimal target-side library/test member view (for tests and fixtures where only
    /// `(root, name, test)` vary).
    pub const fn simple(root: &'a [u8], name: &'a [u8], test: bool) -> MemberView<'a> {
        MemberView { root, name, test, macro_side: false, proc_macro: false, test_or_bench: false, bin: None, package: b"" }
    }
}

/// The unit a row was checked under, where that unit is not the body's base holder: WHAT THE
/// UNIT IS, never what it depends on (D152 point 1's (crate, `test`)). One home: nothing else
/// spells a view.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ViewKey(pub ContentId);

impl ViewKey {
    /// The view of a member unit from its [`MemberView`] identity: its root path in the checkout
    /// tree (`/0/a/src/lib.rs`, never a machine path: rule 25), crate name, `test` flag, build
    /// side (`macro_side`), target-kind flags (`proc_macro`, `test_or_bench`), binary name, and
    /// package name. Not the unit's crate key: that key holds every dependency's key, so a
    /// dependency bumped or an edge added would rename every view row and move every shared body's
    /// row key (`source_at_views`), entering bodies that read nothing of it (rule 77).
    pub fn of_member(member: MemberView<'_>) -> ViewKey {
        let mut h = Hasher::new(MEMBER_VIEW_DOMAIN);
        let flags = u8::from(member.test)
            | (u8::from(member.macro_side) << 1)
            | (u8::from(member.proc_macro) << 2)
            | (u8::from(member.test_or_bench) << 3)
            | (u8::from(member.bin.is_some()) << 4);
        h.update(&[flags])
            .part(member.root)
            .part(member.name)
            .part(member.bin.unwrap_or(b""))
            .part(member.package);
        ViewKey(h.finish())
    }

    /// Disambiguates `base` with `crate_key` when two distinct member units in one `CrateList`
    /// share every [`MemberView`] field (e.g., an example and a library pointing at the same file
    /// and crate name in one package). Because `CrateList::open` enforces strictly distinct crate
    /// keys across all rows, this guarantees every member unit gets a unique [`ViewKey`].
    pub fn disambiguate(base: ViewKey, crate_key: CrateItemKey) -> ViewKey {
        let mut h = Hasher::new(MEMBER_VIEW_DISAMBIGUATE_DOMAIN);
        h.id(&base.0).id(&crate_key.0);
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
