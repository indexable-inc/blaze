//! THE PREVIOUS ANSWER IS THE PLAN (engine_units.rs, the module's doc; end-state.md rule 3): under
//! the sweep's `Stood` proof the units stage plans from the stored answer's own pages, with no
//! side record (the plan index is gone). A child of main.rs's tests, over engine_check_tests.rs's
//! helpers.
//!
//! THE ORACLE (rule 78, one seam): every edit shape is run twice from one seed, once as built and
//! once with `PlanPlant::Discover` (the proof dropped, the graph discovered in full), and each
//! answer equals the other and a fresh run's (rule 26). The shapes: a body edit, a signature edit,
//! a file added, a file removed, a file moved between members, and the manifest edited (the
//! CONTROL: no proof, so the full path, both ways). A body or signature edit takes the read back;
//! the stage writes no byte either way.

use super::engine_check_tests::{engine_opts, opts, profile, profile_lines, three_crates};
use super::*;

use crate::engine_units::{PLAN_PLANT, PlanPlant, PlanRows, Units};

type Answer = (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked);

/// One request with the plan planted as `plant`.
fn request(io: &Io, fresh: bool, plant: PlanPlant) -> Answer {
    PLAN_PLANT.with(|cell| cell.set(plant));
    let answer = engine_opts(io, opts(io, 2, fresh));
    PLAN_PLANT.with(|cell| cell.set(PlanPlant::Built));
    answer
}

/// The facts two answers must share: the findings, the status, the bodies' notes.
fn said(answer: &Answer) -> (std::vec::Vec<(std::string::String, std::string::String)>, u8, std::vec::Vec<std::string::String>) {
    let (ids, checked) = answer;
    (ids.clone(), checked.status.0, checked.bodies.0.iter().cloned().collect())
}

fn plan(answer: &Answer) -> PlanRows {
    answer.1.plan
}

fn remove(io: &Io, path: &str) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    root.remove(&cx, &Path::new(path).expect("a path"), Durable::No).expect("remove");
}

/// An edit of the three crates, and whether it moves a layout row (so no proof stands).
#[derive(Clone, Copy, Debug)]
enum Shape {
    Body,
    Signature,
    FileAdded,
    FileRemoved,
    MovedBetweenMembers,
    ManifestEdited,
}

impl Shape {
    const ALL: [Shape; 6] = [Shape::Body, Shape::Signature, Shape::FileAdded, Shape::FileRemoved, Shape::MovedBetweenMembers, Shape::ManifestEdited];

    fn apply(self, io: &Io) {
        match self {
            Shape::Body => write(io, "a/src/m.rs", "pub fn k() -> u8 {\n    3\n}\n"),
            Shape::Signature => write(io, "a/src/m.rs", "pub fn k() -> u16 {\n    2\n}\n"),
            Shape::FileAdded => {
                write(io, "a/src/n.rs", "pub fn n() -> u8 {\n    4\n}\n");
                write(io, "a/src/lib.rs", "pub mod m;\npub mod n;\n\npub fn f() -> u8 {\n    1\n}\n");
            }
            Shape::FileRemoved => {
                remove(io, "a/src/m.rs");
                write(io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
                write(io, "b/src/lib.rs", "pub fn g() -> u8 {\n    a::f()\n}\n");
            }
            Shape::MovedBetweenMembers => {
                remove(io, "a/src/m.rs");
                write(io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
                write(io, "b/src/m.rs", "pub fn k() -> u8 {\n    2\n}\n");
                write(io, "b/src/lib.rs", "pub mod m;\n\npub fn g() -> u8 {\n    a::f() + m::k()\n}\n");
            }
            Shape::ManifestEdited => write(io, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.2.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n"),
        }
    }

    /// Whether the shape must take the read back as built: only a shape that moves no layout
    /// row is sure to (a file added or removed may move a listing the layout made).
    fn reads_back(self) -> Option<bool> {
        match self {
            Shape::Body | Shape::Signature => Some(true),
            Shape::ManifestEdited => Some(false),
            Shape::FileAdded | Shape::FileRemoved | Shape::MovedBetweenMembers => None,
        }
    }
}

/// The three crates, one cold request and one body edit (so the stored answer was itself read
/// back once), then `shape`, all planted as `plant`: the answer after the shape.
fn after(seed: u64, shape: Shape, plant: PlanPlant) -> (Io, Answer) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    three_crates(&io);
    let _ = request(&io, false, plant);
    write(&io, "c/src/lib.rs", "pub fn h() -> u8 {\n    b::g() + 2\n}\n");
    let _ = request(&io, false, plant);
    shape.apply(&io);
    let answer = request(&io, false, plant);
    (io, answer)
}

/// EVERY EDIT SHAPE PLANS THE SAME FROM THE ANSWER AS FROM A FULL DISCOVER, and as a fresh run.
#[test]
fn the_answer_as_plan_equals_a_full_plan_for_every_edit_shape() {
    for (seed, shape) in (91u64..).zip(Shape::ALL) {
        let (built_io, built) = after(seed, shape, PlanPlant::Built);
        let (_, full) = after(seed, shape, PlanPlant::Discover);
        println!("{shape:?}: built {:?} {}; full {:?} {}", built.1.units, plan(&built), full.1.units, plan(&full));
        assert_eq!(said(&built), said(&full), "{shape:?}: the answer planned from the stored answer is a full plan's\n{}", profile_lines(&built_io));
        // The built request's own profile, taken before the fresh request below writes its own
        // over it (the fresh one starts from no answer, so its phase line names that instead).
        let built_lines = profile_lines(&built_io);
        let built_profile = profile(&built_io);
        let fresh = request(&built_io, true, PlanPlant::Built);
        assert_eq!(said(&built), said(&fresh), "{shape:?}: the answer is a fresh run's (rule 26)");
        // The seam's other side never reads back: the control path is the full one.
        assert_eq!((full.1.units.read_back, full.1.units.discovered), (0, 1), "{shape:?}: the planted side discovers");
        assert!(!plan(&full).stood, "{shape:?}: the planted side holds no proof");
        assert_eq!(plan(&built).bytes_written, 0, "{shape:?}: the stage writes nothing (no side record)");
        assert!(plan(&built).units_read <= plan(&built).units_total, "{shape:?}: {}", plan(&built));
        match shape.reads_back() {
            Some(true) => {
                assert!(plan(&built).stood, "{shape:?}: no layout row moved, so the sweep's proof is handed\n{built_profile}");
                assert_eq!(built.1.units, Units { read_back: 1, discovered: 0, fetches_run: 0 }, "{shape:?}: read back\n{built_lines}");
                // Under the proof no source is statted beside its reader (rule 74); the names are
                // read from the crate list: no world made, no graph decoded (engine.md item 2's
                // floor; the world is made by need by a names or script task, not by the plan).
                assert_eq!(plan(&built).sources_statted, 0, "{shape:?}: no stat under the proof: {}", plan(&built));
                assert_eq!((plan(&built).worlds_built, plan(&built).graph_decodes), (0, 0), "{shape:?}: no world, no decode: {}", plan(&built));
            }
            // CONTROL: a manifest moved, so no proof and the full path as built too.
            Some(false) => {
                assert!(!plan(&built).stood, "{shape:?}: a manifest moved: no proof");
                assert_eq!((built.1.units.read_back, built.1.units.discovered), (0, 1), "{shape:?}: the full path");
                assert!(built_lines.contains("no sweep proof"), "the phase line names why:\n{built_lines}");
            }
            None => {}
        }
        assert!(built_profile.lines().any(|line| line.starts_with("plan: rows read") && line.contains("bytes written 0")), "{shape:?}: the profile's plan line:\n{built_profile}");
    }
}
