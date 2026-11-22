use super::engine_check_tests::{engine_opts, opts};
use super::*;
use crate::engine_bodies::{Said, unmade_head};
use crate::engine_head_notes::{Cause, Reason, RootKind, Step, leak};
use crate::engine_seeds::SeedTally;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{ConstRefused, FreezeError, MOST_MAKING};

/// Each checkout has just one body. The fixture core's FixtureBad has an array field
/// of length FIXTURE_BAD = 0usize - 1. Subtraction in a constant must refuse overflow.
#[test]
fn a_body_names_its_failed_head_and_the_constant_at_the_root() {
    for (asked, source) in [
        ("core::FixtureBad", "pub fn reader() { let _ = ::core::FixtureBad { x: [0; 1] }; }"),
        ("wide::Outer", "pub struct Middle { pub bad: ::core::FixtureBad }\npub struct Outer { pub middle: Middle }\npub fn reader() { let _ = Outer { middle: loop {} }; }"),
    ] {
        let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", source);
        for round in ["cold", "body edited"] {
            if round == "body edited" {
                write(&io, "src/lib.rs", &source.replace("fn reader() {", "fn reader() { let _edited = 1;"));
            }
            let before = stdout(&io).len();
            let (_, checked) = engine_opts(&io, opts(&io, 1, false));
            let text = stdout(&io).split_off(before);
            let note = text.lines().find(|line| line.contains("body `reader`: not checked")).unwrap_or_else(|| panic!("{round}: reader has no body refusal:\n{text}"));
            assert!(note.contains(&format!("the head of `{asked}`")), "{note}");
            assert!(note.contains("`core::FixtureBad`") && note.contains("FIXTURE_BAD") && note.contains("overflow"), "{note}");
            assert_eq!(checked.seeds.bodies.refused_const, 1, "{note}");
            println!("{round}, {asked}: {note}; refused_const 1");
        }
    }
}

fn cell(n: usize) -> Want {
    Want::Head(blaze_id::ItemKey(blaze_id::ContentId::of("unmade head test", &n.to_le_bytes())))
}

/// Replace only the terminal words. Both runs have the same asked head, two Needs
/// edges, terminal cell, typed class, labels and formatting.
#[test]
fn again_words_lose_the_constant_reason() {
    let g = blaze_own::testing::heap().alloc();
    let (outer, middle, root) = (cell(0), cell(1), cell(2));
    let typed = FreezeError::Const { want: root, c: root.key(), error: ConstRefused::NoEvaluator };
    let again = FreezeError::Again { want: root, failed: root };
    for (reason, expected) in [(&typed, true), (&again, false)] {
        let mut tally = SeedTally::default();
        let step = |want| {
            if want == outer {
                return Some(Step::Needs(middle));
            }
            if want == middle {
                return Some(Step::Needs(root));
            }
            assert_eq!(want, root);
            let words = leak(g, format_args!("{}", Reason(reason)));
            Some(Step::End(root, Cause::new(RootKind::of(&typed), words)))
        };
        let said: Said<&str> = unmade_head(outer, step, |want| if want == outer { "Outer" } else { "A" }, &mut tally, g, g);
        let Said::Unchecked { why: note, calls: [] } = said else { panic!("a body note under no refused call: {said:?}") };
        assert!(note.contains("`Outer`") && note.contains("needs `A`"), "{note}");
        assert_eq!(note.contains("no constant evaluator"), expected, "{note}");
        assert_eq!(tally.refused_const, 1, "the same typed root in both runs");
        println!("{note}; refused_const {}", tally.refused_const);
    }
}

#[test]
fn a_cycle_and_a_chain_over_the_limit_name_where_the_walk_stopped() {
    let g = blaze_own::testing::heap().alloc();
    let mut tally = SeedTally::default();
    let start = cell(0);
    let cycle: Said<&str> = unmade_head(start, |at| Some(Step::Needs(if at == start { cell(1) } else { start })), |at| if at == start { "A" } else { "B" }, &mut tally, g, g);
    assert!(matches!(cycle, Said::Refused(note) if note.contains("cycle in failed head needs at `A`")));
    let mut n = 0;
    let long: Said<&str> = unmade_head(start, |_| {
        n += 1;
        Some(Step::Needs(cell(n)))
    }, |at| if at == start { "A" } else { "Next" }, &mut tally, g, g);
    assert_eq!(n, MOST_MAKING + 1);
    assert!(matches!(long, Said::Refused(note) if note.contains("reaches `Next` beyond the limit") && !note.contains("cycle")));
    assert_eq!(tally.refused_const, 0);
    println!("cycle at A; acyclic chain beyond {MOST_MAKING} needs");
}
