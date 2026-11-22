//! A `Path` never escapes its `Dir`: whatever text a caller makes a path of (roots, `..`, `.`,
//! empty parts), what `Path::new` takes is plain and relative, and a write through `jail/in`
//! of it names a file beneath `jail/in`.
//!
//! Plant: [`Plant::LexicalParent`], `..` let in and resolved by text after the join, so
//! `../x` lands in `jail`.

use blaze_core::{Done, Effect};
use blaze_sim::{Config, Tape};

use super::{check, control, path, root, world, write_synced};
use crate::{Plant, path::Path};

const PARTS: [&str; 7] = ["a", "b", "..", ".", "", "c.d", "..."];

#[derive(Debug)]
struct Plan {
    seed: u64,
    raws: Vec<String>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let raws = (0..tape.int_in(1..=16u64, "paths"))
        .map(|_| {
            let rooted = tape.one_in(6, "rooted");
            let parts: Vec<&str> = (0..tape.int_in(1..=5u64, "parts")).map(|_| *tape.one_of(&PARTS, "part")).collect();
            let joined = parts.join("/");
            if rooted { format!("/{joined}") } else { joined }
        })
        .collect();
    Plan { seed, raws }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let jail = io.need_fs().map_err(|e| format!("harness: {e}"))?.open_dir(&cx, &path("jail/in")).map_err(|e| format!("harness: {e}"))?;
    io.record();
    // The names written so far, each branch in its own terms: a later write that clashes with
    // one of them is refused by every disk, and still touches nothing outside the jail.
    let mut written: Vec<String> = Vec::new();
    for raw in &plan.raws {
        let before = io.touched().len();
        if plant == Plant::LexicalParent {
            // The planted bug: `..` let in and resolved by text.
            let Ok(raw) = Path::with_parents(raw) else { continue };
            let Some(file) = jail.beneath_by_text(&raw) else { continue };
            let name = file.as_str().to_owned();
            let clash = clashes(&written, &name);
            let done = io.call(&cx, |tag| Effect::Write { file, bytes: b"x".to_vec(), tag });
            match (clash, &done) {
                (false, Ok(Done::Written)) => written.push(name),
                (true, Ok(Done::Failed(_))) => {}
                _ => return Err(format!("harness: the planted write of {raw} said {done:?}, a clash {clash}")),
            }
        } else {
            match Path::new(raw) {
                Ok(made) => {
                    let text = made.as_str();
                    if text != raw || text.starts_with('/') || text.split('/').any(|it| matches!(it, "" | "." | "..")) {
                        return Err(format!("Path::new took {raw:?} as {text:?}: not plain and relative"));
                    }
                    let clash = clashes(&written, text);
                    match (clash, write_synced(&cx, &jail, &made, b"x")) {
                        (false, Ok(())) => written.push(text.to_owned()),
                        (true, Err(_)) => {}
                        (false, Err(error)) => return Err(format!("harness: {error}")),
                        (true, Ok(())) => {
                            return Err(format!("harness: {text} was written though a file written before clashes with it"));
                        }
                    }
                }
                Err(refused) => {
                    if refused.path != *raw {
                        return Err(format!("Path::new refused {raw:?} naming {:?}", refused.path));
                    }
                }
            }
        }
        // The jail's own name is touched by the sync of its names a synced write ends with
        // (`Effect::SyncDir`), which writes no file: `is_under` takes it.
        for (file, _) in &io.touched()[before..] {
            if !jail.name().holds(file) {
                return Err(format!("the path {raw:?} reached {file}, not beneath {}", jail.name()));
            }
        }
    }
    Ok(())
}

/// Whether a file named `name` cannot be made beside the files `written`: one of them is above
/// it (`ENOTDIR`) or beneath it, so `name` is a directory (`EISDIR`). A real disk refuses
/// both, and so does the simulated one since 2026-11-20; the plan draws such pairs (`a`, then
/// `a/a`), and what is tested is where a write lands, not whether it is made.
fn clashes(written: &[String], name: &str) -> bool {
    let under = |low: &str, high: &str| low.strip_prefix(high).is_some_and(|rest| rest.starts_with('/'));
    written.iter().any(|it| under(name, it) || under(it, name))
}

/// The invariant: a `Path` never escapes its `Dir`. Whatever text a path is made of, what
/// `Path::new` takes is plain and relative, and a write through `jail/in` of it names a file
/// beneath `jail/in`.
#[test]
fn a_path_never_escapes_its_directory() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_parent_resolved_by_text() {
    control(Plant::LexicalParent, plan, run);
}
