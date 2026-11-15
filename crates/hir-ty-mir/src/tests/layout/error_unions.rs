//! The layout of an error union (`#![blaze::error_unions]`,
//! docs/v2/plan/design/error-sets.md, "Representation and cost"): the layout of the enum a
//! person would write by hand for the same members, one variant each, and the same in every
//! build.

use super::eval_goal;

/// bstd's `Missing`, `FsError` and `Stopped`, reduced to their shapes.
const MEMBERS: &str = "
pub struct Missing { pub capability: &'static str }
pub enum FsKind { NotFound, NotADirectory, Stale { flushed_at: u64, changed_at: u64 } }
pub struct FsError { pub op: u8, pub kind: FsKind }
pub enum Stopped { Deadline { at: u32 }, Cancelled }
";

const MINICORE: &str = "//- minicore: sized, error_union, result\n";

/// The size, the alignment and the debug text of the layout of the fixture's `Goal`.
fn layout_of(goal: &str) -> (u64, u64, String) {
    let fixture = format!("#![blaze::error_unions]\n{MEMBERS}\n{goal}\n");
    let layout = eval_goal(&fixture, MINICORE)
        .unwrap_or_else(|error| panic!("the layout of `{goal}`: {error:?}"));
    (layout.size.bytes(), layout.align.bytes(), format!("{layout:?}"))
}

/// The floor: a set costs what the hand-written enum of its members costs, in size and
/// alignment, bare and as the error of a `Result` (where a niche may hold `Ok`), for two
/// members and for three.
#[test]
fn a_set_is_laid_out_as_the_enum_of_its_members_written_by_hand() {
    for (set, by_hand) in [
        ("Missing | FsError", "pub enum Hand { A(Missing), B(FsError) }"),
        ("Missing | FsError | Stopped", "pub enum Hand { A(Missing), B(FsError), C(Stopped) }"),
        ("Stopped | Missing", "pub enum Hand { A(Stopped), B(Missing) }"),
    ] {
        let union = layout_of(&format!("pub type Goal = {set};"));
        let hand = layout_of(&format!("{by_hand}\npub type Goal = Hand;"));
        assert_eq!((union.0, union.1), (hand.0, hand.1), "`{set}` against `{by_hand}`");
        let in_result = layout_of(&format!("pub type Goal = Result<u8, {set}>;"));
        let hand_in_result = layout_of(&format!("{by_hand}\npub type Goal = Result<u8, Hand>;"));
        assert_eq!(
            (in_result.0, in_result.1),
            (hand_in_result.0, hand_in_result.1),
            "`Result<u8, {set}>` against `Result<u8, Hand>` of `{by_hand}`",
        );
    }
}

/// (c) of the feature's check, for the representation: the set written in each order of its
/// members, each in a build of its own, has one layout, byte for byte of its description
/// (variants, offsets, tag and niche).
#[test]
fn a_set_has_one_layout_in_every_order_and_every_build() {
    let orders = [
        "Missing | FsError | Stopped",
        "Stopped | FsError | Missing",
        "FsError | Stopped | Missing",
        "FsError | (Missing | Stopped)",
    ];
    let first = layout_of(&format!("pub type Goal = {};", orders[0]));
    for order in &orders[1..] {
        let other = layout_of(&format!("pub type Goal = {order};"));
        assert_eq!(first, other, "`{}` against `{order}`", orders[0]);
    }
    // And the same order again, in another database: nothing of the first is left to read.
    assert_eq!(first, layout_of(&format!("pub type Goal = {};", orders[0])));
}
