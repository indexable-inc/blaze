//! The roles a function plays for the daemon, each an effect of [`crate::effects::EFFECTS`]
//! checked over the one summary (docs/v2/plan/design/effect-summary.md):
//!
//! - [`BOUNDED_TURN`], `#[blaze::bounded_turn]`: a loop thread's turn. It must come back to
//!   its one wait within a bound, so nothing under it may block the thread, do IO, sync a file
//!   or take a lock (`blaze::loop_turn_effects`). A lock the loop alone takes, uncontended, is
//!   allowed by marking its accessor `#[blaze::bounded_turn(trusted = "why")]`: the allow-list
//!   is per function, as lock names are run-time strings.
//! - [`ANSWERS_REQUEST`], `#[blaze::answers_request]`: an answer path. A reply must not wait
//!   on the drive, so nothing under it may sync a file to the disk
//!   (`blaze::sync_on_answer_path`).
//!
//! Both refuse what the walk can't see ([`Rule::Unchecked`], [`Rule::ForeignUnknown`]): an
//! opaque call is refused by name, never taken as clean.

use base_db::Db;
use hir_def::{
    AttrDefId,
    attrs::{AttrFlags, PureAttr, RoleWord},
};

use crate::effects::{EffectDef, EffectSet, Rule};

/// `#[blaze::bounded_turn]`: no call that blocks the thread, does IO, syncs a file or takes a
/// lock, transitively.
pub const BOUNDED_TURN: EffectDef = EffectDef {
    name: "bounded_turn",
    word: None,
    refuses: EffectSet::of(&[
        Rule::Block,
        Rule::Io,
        Rule::Sync,
        Rule::Lock,
        Rule::ForeignUnknown,
        Rule::Unchecked,
    ]),
    attr: bounded_turn_attr,
    says,
    fix: "a loop's turn hands work that blocks, does IO or syncs to the driver as an effect, or to a plane task on a worker, and takes no lock another thread holds; mark an accessor of a lock only the loop takes `#[blaze::bounded_turn(trusted = \"why\")]`",
    through_never: true,
    crate_wide: false,
};

/// `#[blaze::answers_request]`: no sync of a file to the disk, transitively.
pub const ANSWERS_REQUEST: EffectDef = EffectDef {
    name: "answers_request",
    word: None,
    refuses: EffectSet::of(&[Rule::Sync, Rule::ForeignUnknown, Rule::Unchecked]),
    attr: answers_request_attr,
    says,
    fix: "an answer never waits on the drive: sync after the reply is handed to the driver (a `Durable` effect the reply does not wait on), or mark a callee proven not to sync `#[blaze::answers_request(trusted = \"why\")]`",
    through_never: true,
    crate_wide: false,
};

fn bounded_turn_attr(db: &Db, owner: AttrDefId) -> Option<PureAttr> {
    AttrFlags::role_attr(db, owner, RoleWord::BoundedTurn)
}

fn answers_request_attr(db: &Db, owner: AttrDefId) -> Option<PureAttr> {
    AttrFlags::role_attr(db, owner, RoleWord::AnswersRequest)
}

/// What a kind is said as in a role's refusal: the effect attributes' words, and the new kinds.
fn says(rule: Rule) -> &'static str {
    match rule {
        Rule::Io => "does IO on the thread",
        Rule::Sync => "syncs a file to the disk, a wait on the drive",
        Rule::Lock => "takes a lock another thread may hold",
        other => crate::effect_attrs::says(other),
    }
}
