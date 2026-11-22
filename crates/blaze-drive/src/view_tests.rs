//! A name at a view, and the `Full` plan against its oracle (rule 79: the interface with its
//! state machine, simulated before the engine takes it).
//!
//! The model: bodies, each held by units in rank order, and an answer per (body, unit), which
//! differs between holders where the body reads `test` (a `cfg!(test)`, a test-only type). A
//! plan writes rows; a unit's table reads them as the run side does (its view first, then the
//! plain name only where that row was checked under it). The oracle is "each unit checked as a
//! crate of its own": every (body, holder) reads that unit's answer. The planted control is the
//! rule before D191: the first holder checks alone and the others take its row.

use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use super::{Full, UnitView, ViewKey, ViewPlan, at_view};
use crate::key::{Kind, Name};

fn subject(tape: &Tape, label: &'static str) -> Result<ContentId, Broke> {
    let bytes = tape.bytes(32..=32, label);
    <[u8; 32]>::try_from(bytes.as_slice()).map(ContentId).map_err(|_| Broke::Harness(String::from("32 bytes drawn")))
}

/// Unit `unit` of the model: its own root, the package's name, not a test build.
fn view(unit: usize) -> ViewKey {
    ViewKey::of_member(&unit.to_le_bytes(), b"pkg", false)
}

/// PLANTED CONTROL: the view as it was spelled before (a digest of the unit's crate key, which
/// holds its dependencies' keys).
fn view_by_crate_key(key: &CrateItemKey) -> ViewKey {
    ViewKey(ContentId::of("blaze-drive view 1", &key.0.0))
}

/// A unit's crate key as the graph makes it: its own parts and its dependencies' keys, so an
/// edge added moves it. The model of what the view must NOT follow.
fn crate_key(root: &[u8], name: &[u8], test: bool, deps: &[ContentId]) -> CrateItemKey {
    let mut h = blaze_ids::Hasher::new("blaze-drive view test crate key");
    h.update(&[u8::from(test)]).part(root).part(name);
    for dep in deps {
        h.id(dep);
    }
    CrateItemKey(h.finish())
}

/// Over drawn member units (a library, its test build, bins, an integration test), each with
/// drawn dependencies: a dependency edge added to one member moves NO view's name, and the
/// views of the units stay distinct. CONTROL: the view spelled from the crate key moves for the
/// member the edge was added to ("a view moved"), so the stillness is the new key's.
fn an_edge_added_moves_no_view(tape: &Tape, by_key: bool) -> Result<(), Broke> {
    // (root, name, test): the library and its test build share a root; a bin has its own.
    let units: [(&[u8], &[u8], bool); 4] = [
        (b"/0/a/src/lib.rs", b"a", false),
        (b"/0/a/src/lib.rs", b"a", true),
        (b"/0/a/src/main.rs", b"a", false),
        (b"/0/a/tests/it.rs", b"it", true),
    ];
    let deps: Vec<ContentId> = (0..1 + tape.index(4, "deps")).map(|d| subject(tape, if d == 0 { "dep 0" } else { "dep" })).collect::<Result<_, _>>()?;
    let added = subject(tape, "added edge")?;
    let to = tape.index(units.len(), "member the edge is added to");
    let view_of = |(root, name, test): (&[u8], &[u8], bool), deps: &[ContentId]| {
        if by_key { view_by_crate_key(&crate_key(root, name, test, deps)) } else { ViewKey::of_member(root, name, test) }
    };
    let before: Vec<ViewKey> = units.iter().map(|u| view_of(*u, &deps)).collect();
    let mut grown = deps.clone();
    grown.push(added);
    let after: Vec<ViewKey> = units.iter().enumerate().map(|(at, u)| view_of(*u, if at == to { &grown } else { &deps })).collect();
    for (at, (was, now)) in before.iter().zip(&after).enumerate() {
        if was != now {
            return Err(Broke::Oracle(format!("a view moved: unit {at} when an edge was added to unit {to}")));
        }
    }
    for a in 0..after.len() {
        for b in a + 1..after.len() {
            if after[a] == after[b] {
                return Err(Broke::Oracle(format!("units {a} and {b} share a view")));
            }
        }
    }
    Ok(())
}

#[test]
fn a_dependency_edge_added_to_a_member_moves_no_view() {
    check(SimConfig::cases(200), |tape| an_edge_added_moves_no_view(tape, false));
}

/// The view spelled from the crate key (before) renames the member an edge was added to.
#[test]
fn control_a_view_spelled_from_the_crate_key_moves_with_an_edge() {
    control(SimConfig::cases(200), false, true, "a view moved", an_edge_added_moves_no_view);
}

#[test]
fn a_name_at_its_base_is_itself() {
    let name = Name::new(Kind::BodyCheck, ContentId([7; 32]));
    assert_eq!(at_view(name, UnitView::Base), name);
}

/// Over drawn kinds, subjects and views: the base is the name itself; two views of one name
/// differ from each other and from the base; the kind is kept; and the name at a view is a
/// function of (kind, subject, view) alone: made again from those parts it is equal, and the
/// kind is in the subject (a `BodyCheck` and a `Code` of one subject at one view have different
/// subjects, so no table keyed by subject alone confuses them).
fn names_at_views(tape: &Tape) -> Result<(), Broke> {
    let kind = *tape.one_of(&Kind::ALL, "kind");
    let other = *tape.one_of(&Kind::ALL, "other kind");
    let base = Name::new(kind, subject(tape, "subject")?);
    let v1 = ViewKey(subject(tape, "view 1")?);
    let v2 = ViewKey(subject(tape, "view 2")?);
    let at1 = at_view(base, UnitView::At(v1));
    let at2 = at_view(base, UnitView::At(v2));
    let fail = |what: &str| Err(Broke::Oracle(format!("{what}: {base:?} at {v1:?} / {v2:?}")));
    if at_view(base, UnitView::Base) != base {
        return fail("the base is not the name itself");
    }
    if at1 == base || at2 == base {
        return fail("a view is the base");
    }
    if v1 != v2 && at1 == at2 {
        return fail("two views are one name");
    }
    if at1.kind != kind || at2.kind != kind {
        return fail("the kind moved");
    }
    if at_view(Name::new(kind, base.subject), UnitView::At(ViewKey(v1.0))) != at1 {
        return fail("not a function of (kind, subject, view)");
    }
    if other != kind && at_view(Name::new(other, base.subject), UnitView::At(v1)).subject == at1.subject {
        return fail("the kind is not in the subject");
    }
    Ok(())
}

#[test]
fn names_at_views_are_distinct_and_keep_their_kind() {
    check(SimConfig::cases(500), names_at_views);
}

#[test]
fn full_plans_the_first_holder_as_base_and_each_other_at_its_own_view() {
    let holders = [view(3), view(1), view(2)];
    assert_eq!(Full::view_of(&holders, 0), UnitView::Base);
    assert_eq!(Full::view_of(&holders, 1), UnitView::At(view(1)));
    assert_eq!(Full::view_of(&holders, 2), UnitView::At(view(2)));
}

/// How the model plans rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plan {
    /// `Full`: every holder checks.
    Full,
    /// PLANTED, the rule before D191: the first holder checks alone and every other holder
    /// reads its row.
    FirstChecksAlone,
}

struct Body {
    name: Name,
    /// Unit indices, in rank order; never empty.
    holders: Vec<usize>,
    /// The answer under each holder, as checking that unit as a crate of its own gives it.
    answers: Vec<u64>,
}

fn draw(tape: &Tape) -> Result<Vec<Body>, Broke> {
    let units = 1 + tape.index(4, "units");
    let mut bodies = Vec::new();
    for b in 0..1 + tape.index(6, "bodies") {
        let mut holders: Vec<usize> = (0..units).filter(|_| tape.bool("holds")).collect();
        if holders.is_empty() {
            holders.push(tape.index(units, "only holder"));
        }
        // Rank is the crate list's order here; a model of the rank itself is the planner's.
        let base = b as u64 * 10;
        // A body reads `test` under some holders: its answer there is its own.
        let answers = holders.iter().map(|u| if tape.one_in(3, "differs") { base + 1 + *u as u64 } else { base }).collect();
        bodies.push(Body { name: Name::new(Kind::BodyCheck, subject(tape, "body")?), holders, answers });
    }
    Ok(bodies)
}

/// A row: the unit it was checked under and its answer.
type Rows = BTreeMap<Name, (usize, u64)>;

fn plan(bodies: &[Body], how: Plan) -> Result<Rows, Broke> {
    let mut rows = Rows::new();
    for body in bodies {
        let views: Vec<ViewKey> = body.holders.iter().map(|u| view(*u)).collect();
        let planned = match how {
            Plan::Full => body.holders.len(),
            Plan::FirstChecksAlone => 1,
        };
        for at in 0..planned {
            let name = at_view(body.name, Full::view_of(&views, at));
            if rows.insert(name, (body.holders[at], body.answers[at])).is_some() {
                return Err(Broke::Oracle(format!("two rows under one name {name:?}")));
            }
        }
    }
    Ok(rows)
}

/// What `unit`'s body-id table finds for `body`: the row at its view; else the plain name's,
/// only where that row was checked under `unit`. Under the planted rule, the plain row whoever
/// checked it (the read-through D191 forbids while keys differ).
fn table(rows: &Rows, body: &Body, unit: usize, how: Plan) -> Option<u64> {
    if let Some((_, answer)) = rows.get(&at_view(body.name, UnitView::At(view(unit)))) {
        return Some(*answer);
    }
    match rows.get(&body.name) {
        Some((checked, answer)) if *checked == unit || how == Plan::FirstChecksAlone => Some(*answer),
        _ => None,
    }
}

fn each_unit_as_its_own_crate(tape: &Tape, how: Plan) -> Result<(), Broke> {
    let bodies = draw(tape)?;
    let rows = plan(&bodies, how)?;
    let expected: usize = bodies.iter().map(|b| b.holders.len()).sum();
    if how == Plan::Full && rows.len() != expected {
        return Err(Broke::Oracle(format!("{} rows for {expected} holders", rows.len())));
    }
    for body in &bodies {
        for (at, unit) in body.holders.iter().enumerate() {
            let found = table(&rows, body, *unit, how);
            if found != Some(body.answers[at]) {
                return Err(Broke::Oracle(format!(
                    "unit {unit} reads {found:?} for {:?}; checked as a crate of its own it answers {}: they differ",
                    body.name, body.answers[at]
                )));
            }
        }
    }
    Ok(())
}

#[test]
fn full_answers_every_unit_as_its_own_crate_would() {
    check(SimConfig::cases(500), |tape| each_unit_as_its_own_crate(tape, Plan::Full));
}

/// The rule before D191 gives a second holder the first's answer: caught where they differ.
#[test]
fn control_the_first_holder_checking_alone_is_caught() {
    control(SimConfig::cases(2000), Plan::Full, Plan::FirstChecksAlone, "they differ", each_unit_as_its_own_crate);
}
