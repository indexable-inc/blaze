//! The work of a build, counted by stage: what `blaze explain edit_work` prints, and what
//! says whether an edit cost the edit or the program (rule incremental-first).
//!
//! Each stage that an edit goes through says once, where its work is known, how many things
//! it visited, as one kept event: `tracing::info!(target: "blaze", stage = <one of the names
//! below>, <counter> = <u64>, .., "edit work")`. The message is [`MESSAGE`]; every integer
//! field of the event is a counter of that stage. The profile keeps the event with its
//! build, and the view sums each counter of each stage over the build's units
//! (crates/blaze-profile/sql/examples.sql, `edit_work`), beside the engine's memos run and
//! verified from `query_counts`.
//!
//! The stage names live here, one home, so the stages and the view agree
//! (rule one-concept-one-home). What each counter means is said where it is counted:
//!
//! | stage | said by | counters |
//! |---|---|---|
//! | [`CHANGED_SET`] | the engine's walk (`db/frontier.rs`) | `written`, `untracked`, `said`, `visited`, `edges`, `changed`, `unwound` |
//! | [`BODY_CHECK`] | a plan of every body (`hir::check::bodies`) | `held_modules`, `held_bodies`, `named_modules`, `named_bodies`, `relisted`, `slots_modules`, `slots_bodies`, `visited_modules`, `visited_bodies`, `whole` |
//! | [`CODEGEN_WALK`] | a unit's codegen (blaze-cargo's `serve/unit/codegen.rs`, from the driver's `Walk` and `Work`) | `asked`, `named`, `held`, `each_build`, `described`, `verified`, `reached`, `visited`, `replaced`, `copied`, `whole` |
//! | [`IMAGE_KEYS`] | a dependency serve (blaze-cargo's `serve/unit/deps.rs`) | `asked`, `moved` |
//! | [`LINK`] | a unit's link (blaze-cargo's `serve/unit/codegen.rs`) | `rows`, `tables`, `symbols`, `patchable`, `whole` |
//!
//! A counter that grows with the program where the edit did not is the regression this
//! exists to show; each stage's own test holds its counters to the edit at two sizes.

/// The message of every edit-work event.
pub const MESSAGE: &str = "edit work";

/// The engine's changed-set walk: from the inputs written to the rows whose value changed.
pub const CHANGED_SET: &str = "changed set";
/// The check of every body of a crate: the modules and bodies its plan holds and visits.
pub const BODY_CHECK: &str = "body check";
/// A program's codegen walk: the functions asked of again, described, and rows replaced.
pub const CODEGEN_WALK: &str = "codegen walk";
/// The dependencies' image keys asked of the database, and those that moved.
pub const IMAGE_KEYS: &str = "dependency image keys";
/// A program's link: the rows of its tables made again and the names it went over.
pub const LINK: &str = "link";

/// Every stage, in the order an edit goes through them.
pub const STAGES: [&str; 5] = [CHANGED_SET, BODY_CHECK, CODEGEN_WALK, IMAGE_KEYS, LINK];
