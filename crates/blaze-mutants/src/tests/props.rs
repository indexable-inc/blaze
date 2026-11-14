//! Properties of the operators, the records and the planner, each a function of one tape.

use blaze_ids::ContentId;
use blaze_program::record::TestKey;
use blaze_sim::Tape;

use super::world::World;
use crate::Plant;
use crate::operator::{Mutation, adjacent, mutations, range};
use crate::select::{Budget, Config, PATCH_NS, Scope, Universe, fit, plan};
use crate::site::{ArithOp, CmpOp, ItemPos, MirLoc, Role, Site, SiteKind, TyClass};
use crate::verdict::{KilledBy, Verdict, VerdictRecord};

const CMPS: [CmpOp; 6] = [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge, CmpOp::Eq, CmpOp::Ne];
const ARITHS: [ArithOp; 10] = [
    ArithOp::Add,
    ArithOp::Sub,
    ArithOp::Mul,
    ArithOp::Div,
    ArithOp::Rem,
    ArithOp::BitAnd,
    ArithOp::BitOr,
    ArithOp::BitXor,
    ArithOp::Shl,
    ArithOp::Shr,
];

fn ty(tape: &Tape) -> TyClass {
    match tape.int_in(0u8..=3, "ty") {
        0 => TyClass::Bool,
        1 => TyClass::Float,
        2 => TyClass::Char,
        _ => TyClass::Int { signed: tape.bool("signed"), bits: *tape.one_of(&[8u8, 16, 32, 64, 128], "bits") },
    }
}

fn site_of(kind: SiteKind) -> Site {
    Site {
        kind,
        at: MirLoc { block: 0, statement: 0 },
        pos: ItemPos { line: 1, col: 0 },
        role: Role::Behaviour,
        read: true,
        text: b"a op b".to_vec(),
        token: (2, 4),
    }
}

/// Every mutation changes the site: another operator, another constant in the type's range.
#[test]
fn every_mutation_changes_the_site() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let ty = ty(tape);
        let kind = match tape.int_in(0u8..=2, "site kind") {
            0 => SiteKind::Compare { op: *tape.one_of(&CMPS, "compare"), ty },
            1 => SiteKind::Arith { op: *tape.one_of(&ARITHS, "arith"), ty },
            _ => {
                let value = match range(ty) {
                    Some((lo, hi)) => {
                        let pick = tape.int_in(0u8..=3, "const");
                        match pick {
                            0 => lo,
                            1 => hi,
                            2 => 0i128.clamp(lo, hi),
                            _ => 1i128.clamp(lo, hi),
                        }
                    }
                    None => 0,
                };
                SiteKind::Const { value, ty, absorb: crate::site::Absorb::None }
            }
        };
        let site = site_of(kind.clone());
        let mut out = Vec::new();
        mutations(&site, &mut out);
        for mutation in &out {
            match (&kind, mutation) {
                (SiteKind::Compare { op, .. }, Mutation::Compare { to }) => assert_ne!(op, to),
                (SiteKind::Arith { op, ty }, Mutation::Arith { to }) => {
                    assert_ne!(op, to);
                    assert!(to.valid_on(*ty), "{to:?} is not defined on {ty:?}");
                }
                (SiteKind::Const { value, ty, .. }, Mutation::Const { to }) => {
                    assert_ne!(value, to);
                    let (lo, hi) = range(*ty).expect("a constant mutated has a range");
                    assert!(*to >= lo && *to <= hi, "{to} is outside {ty:?}");
                }
                (kind, mutation) => panic!("{mutation:?} is not a mutation of {kind:?}"),
            }
            let mut bytes = Vec::new();
            mutation.encode(&mut bytes);
            assert_eq!(Mutation::decode(&bytes), Some((*mutation, bytes.len())));
        }
        if let SiteKind::Const { value, ty, .. } = kind {
            let mut values = Vec::new();
            adjacent(value, ty, &mut values);
            let mut sorted = values.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), values.len(), "adjacent values repeat");
        }
    });
}

fn mutation(tape: &Tape) -> Mutation {
    let small = |tape: &Tape| tape.int::<u16>("small");
    match tape.int_in(0u8..=11, "mutation") {
        0 => Mutation::ReturnDefault,
        1 => Mutation::ReturnArg { arg: tape.int::<u8>("arg") },
        2 => Mutation::ReturnField { field: small(tape) },
        3 => Mutation::NegateCondition,
        4 => Mutation::Compare { to: *tape.one_of(&CMPS, "compare") },
        5 => Mutation::Arith { to: *tape.one_of(&ARITHS, "arith") },
        6 => Mutation::Const { to: i128::from(tape.int::<i64>("const a")) * i128::from(tape.int::<i64>("const b")) },
        7 => Mutation::DeleteEffect,
        8 => Mutation::ArmBody { arm: small(tape), from: small(tape) },
        9 => Mutation::SwapArgs { a: tape.int::<u8>("swap a"), b: tape.int::<u8>("swap b") },
        10 => Mutation::SkipEarlyReturn,
        _ => Mutation::Variant { to: small(tape) },
    }
}

/// A mutation's bytes decode to it, and say how long they are, with bytes after them.
#[test]
fn mutations_round_trip() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let mutation = mutation(tape);
        let mut bytes = Vec::new();
        mutation.encode(&mut bytes);
        let len = bytes.len();
        bytes.extend_from_slice(&tape.vec(0..=4, "after", |tape| tape.int::<u8>("byte")));
        assert_eq!(Mutation::decode(&bytes), Some((mutation, len)));
        if len > 1 {
            assert_eq!(Mutation::decode(&bytes[..1]), None, "{mutation:?} decoded from its tag alone");
        }
    });
}

/// A filed verdict's record decodes to it; a cut or extended one is refused.
#[test]
fn verdict_records_round_trip() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let mut tests: Vec<TestKey> = tape
            .vec(0..=6, "tests", |tape| tape.int::<u8>("test"))
            .into_iter()
            .map(|seed| TestKey(ContentId::of("blaze mutants prop test", &[seed])))
            .collect();
        tests.sort();
        tests.dedup();
        let verdict = match tape.bool("killed") {
            true => Verdict::Killed {
                test: tests.first().copied().unwrap_or_default(),
                how: *tape.one_of(&[KilledBy::Failed, KilledBy::Crashed, KilledBy::Timeout], "how"),
            },
            false => Verdict::Survived { tests },
        };
        let record = VerdictRecord { verdict };
        let bytes = record.encode().expect("a filed verdict has a record");
        assert_eq!(VerdictRecord::decode(&bytes), Ok(record));
        assert!(VerdictRecord::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(VerdictRecord::decode(&longer).is_err());
    });
}

/// What fits, fits: on no slot do the fitted estimates pass the wall, and nothing longer than
/// the wall is fitted.
#[test]
fn fit_keeps_the_wall() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let wall = tape.int_in(0u64..=1_000, "wall");
        let slots = tape.int_in(1u32..=8, "slots");
        let estimates = tape.vec(0..=24, "estimates", |tape| tape.int_in(0u64..=400, "estimate"));
        let budget = Budget { wall_ns: wall, slots };
        let fits = fit(&estimates, &budget);
        assert_eq!(fits.len(), estimates.len());
        let total: u64 = estimates.iter().zip(&fits).filter(|(_, fits)| **fits).map(|(estimate, _)| *estimate).sum();
        assert!(total <= wall * u64::from(slots), "{total} fitted into {slots} slots of {wall}");
        for (estimate, fits) in estimates.iter().zip(&fits) {
            if *estimate > wall {
                assert!(!fits, "{estimate} fitted a wall of {wall}");
            }
        }
    });
}

/// A plan is a function of the universe's contents, not its order: the same seed, the same
/// mutants in the same order under the same keys.
#[test]
fn plans_do_not_depend_on_order() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let world = World::new(tape);
        let universe = world.universe();
        let mut functions = universe.functions.to_vec();
        let mut tests = universe.tests.to_vec();
        let turns = tape.int_in(0..=functions.len() - 1, "turns");
        functions.rotate_left(turns);
        functions.reverse();
        tests.reverse();
        let shuffled = Universe { functions: &functions, tests: &tests };
        let items: Vec<_> = universe.functions.iter().map(|facts| facts.item).collect();
        let mut reversed = items.clone();
        reversed.reverse();
        let config = Config { per_function: 4, per_line: 1, budget: Budget { wall_ns: 1 << 40, slots: 2 }, patch_ns: PATCH_NS };
        let a = plan(&Scope::Items(&items), &universe, &config, Plant::None).expect("a plan of the world");
        let b = plan(&Scope::Items(&reversed), &shuffled, &config, Plant::None).expect("a plan of the shuffled world");
        assert_eq!(a.seed, b.seed);
        let view = |plan: &crate::select::Plan| -> Vec<_> {
            plan.candidates.iter().map(|candidate| (candidate.at, candidate.priority, candidate.key)).collect()
        };
        assert_eq!(view(&a), view(&b));
        assert_eq!(a.decided, b.decided);
    });
}
