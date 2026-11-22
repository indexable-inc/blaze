//! The folders above the start folder (std.md, "Folders above the start folder"):
//! `Fs::ancestors` gives exactly the granted ones, nearest first, each reading what the world
//! put there; a walk up finds the nearest `Cargo.toml`; a step past the granted top and a name
//! that walks back down into the start are typed refusals. That no write reaches them is the
//! types' (refusals/ancestor_read_only.rs, refusals/ancestor_made_by_fs.rs).
//!
//! Plant: [`Plant::AncestorsUnbounded`], the granted top not asked of the driver.

use blaze_core::{AboveRefused, FileName, sim::SimAbove};
use blaze_sim::{Config, Tape};

use super::{check, control, path, root};
use crate::{
    Plant,
    fs::{AncestorKind, Fs},
    io::{Io, Limits, SimWorld},
};

const MANIFEST: &[u8] = b"[workspace]\n";

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// Each folder above, nearest first: whether it holds a `Cargo.toml`.
    manifests: Vec<bool>,
    /// The granted top.
    top: u8,
    /// A step asked of `Fs::ancestor`.
    step: u8,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let manifests = (0..tape.int_in(0..=4u64, "folders above")).map(|_| tape.int_in(0..=1u64, "a manifest") == 1).collect();
    let top = tape.int_in(0..=5u64, "granted top") as u8;
    let step = tape.int_in(0..=6u64, "step") as u8;
    Plan { seed, manifests, top, step }
}

/// The name, in the folder `up` above, of the folder on the way down.
fn toward(up: usize) -> String {
    format!("level{up}")
}

fn simulated(manifests: &[bool], top: u8, seed: u64, plant: Plant) -> Io {
    let mut world = SimWorld::new(seed, Limits::USUAL);
    world.above = manifests
        .iter()
        .enumerate()
        .map(|(at, &manifest)| {
            let mut files = vec![(FileName::new("src/lib.rs").expect("a name"), vec![at as u8])];
            if manifest {
                files.push((FileName::new("Cargo.toml").expect("a name"), MANIFEST.to_vec()));
            }
            SimAbove { toward: toward(at + 1).into(), files }
        })
        .collect();
    world.top = top;
    Io::planted(world, plant)
}

fn need_fs(io: &Io) -> Result<Fs, String> {
    io.need_fs().map_err(|e| format!("harness: {e}"))
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = simulated(&plan.manifests, plan.top, plan.seed, plant);
    let fs = need_fs(&io)?;
    let cx = root(&io);
    let granted = plan.manifests.len().min(usize::from(plan.top));
    let ancestors = fs.ancestors(&cx).map_err(|e| format!("ancestors: {e}"))?;
    if ancestors.len() != granted {
        return Err(format!("{} folders above were given, {granted} granted", ancestors.len()));
    }
    for (at, ancestor) in ancestors.iter().enumerate() {
        if usize::from(ancestor.up()) != at + 1 || ancestor.toward() != toward(at + 1) {
            return Err(format!("the {at}th folder above is {ancestor:?} toward {:?}", ancestor.toward()));
        }
        let stat = ancestor.stat(&cx, &path("Cargo.toml")).map_err(|e| format!("stat: {e}"))?;
        if stat.is_some() != plan.manifests[at] {
            return Err(format!("{ancestor:?}: a Cargo.toml is {stat:?}, and the world put one: {}", plan.manifests[at]));
        }
        let mut into = cx.bytes();
        ancestor.read_all(&cx, &path("src/lib.rs"), &mut into).map_err(|e| format!("read: {e}"))?;
        if into.as_slice() != [at as u8] {
            return Err(format!("{ancestor:?}: src/lib.rs read {:?}", into.as_slice()));
        }
        // A name back down toward the start is refused: that file has a shorter name.
        let down = path(&format!("{}/x", toward(at + 1)));
        match ancestor.stat(&cx, &down) {
            Err(error) if matches!(error.kind, AncestorKind::Refused(AboveRefused::IntoStart { .. })) => {}
            got => return Err(format!("{ancestor:?}: a stat of {down} walking into the start gave {got:?}")),
        }
    }
    let step = fs.ancestor(&cx, plan.step);
    let want = match plan.step {
        0 => Some(AboveRefused::Root),
        up if usize::from(up) > granted => Some(AboveRefused::PastTop { up, depth: granted as u8 }),
        _ => None,
    };
    match (step, want) {
        (Ok(ancestor), None) if ancestor.up() == plan.step => Ok(()),
        (Err(error), Some(want)) if error.kind == AncestorKind::Refused(want.clone()) => Ok(()),
        (got, want) => Err(format!("a step {} up of {granted} granted gave {got:?}, not {want:?}", plan.step)),
    }
}

/// The invariant: exactly the granted folders above, each reading what is there; a step past
/// the top and a name back into the start are typed refusals.
#[test]
fn ancestors_are_the_granted_folders_and_only_read() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_an_unbounded_top() {
    control(Plant::AncestorsUnbounded, plan, run);
}

/// The walk a build tool makes: up from the start to the nearest `Cargo.toml`, two above.
#[test]
fn a_walk_up_finds_the_manifest_two_folders_above() {
    let io = simulated(&[false, true, true], u8::MAX, 1, Plant::None);
    let fs = need_fs(&io).expect("the file tree");
    let cx = root(&io);
    let ancestors = fs.ancestors(&cx).expect("the folders above");
    let found = ancestors
        .iter()
        .find(|it| it.stat(&cx, &path("Cargo.toml")).expect("a stat").is_some())
        .expect("a Cargo.toml above");
    assert_eq!(found.up(), 2);
    let mut into = cx.bytes();
    found.read_all(&cx, &path("Cargo.toml"), &mut into).expect("its bytes");
    assert_eq!(into.as_slice(), MANIFEST);
    let listed = found.list(&cx, cx.frame()).expect("a listing");
    let names: Vec<(&str, bool)> = listed.iter().map(|(name, meta)| (name.as_str(), meta.dir)).collect();
    assert_eq!(names, [("Cargo.toml", false), ("level2", true), ("src", true)]);
}

/// With the top granted at one folder, the second is a typed refusal, as is a name back down.
#[test]
fn a_step_past_the_top_and_one_back_down_are_refused() {
    let io = simulated(&[false, true], 1, 1, Plant::None);
    let fs = need_fs(&io).expect("the file tree");
    let cx = root(&io);
    let error = fs.ancestor(&cx, 2).expect_err("past the top");
    assert_eq!(error.kind, AncestorKind::Refused(AboveRefused::PastTop { up: 2, depth: 1 }));
    assert_eq!(error.path, "../..");
    let one = fs.ancestor(&cx, 1).expect("one up");
    let error = one.stat(&cx, &path("level1/src/lib.rs")).expect_err("back into the start");
    assert!(matches!(error.kind, AncestorKind::Refused(AboveRefused::IntoStart { up: 1, .. })), "{error}");
    assert_eq!(error.path, "../level1/src/lib.rs");
}
