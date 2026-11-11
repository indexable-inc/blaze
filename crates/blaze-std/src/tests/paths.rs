//! A `Path` never escapes its `Dir`: whatever text a caller makes a path of (roots, `..`, `.`,
//! empty parts), what `Path::new` takes is plain and relative, and a write through `jail/in`
//! of it names a file beneath `jail/in`.
//!
//! Plant: [`Plant::LexicalParent`], `..` let in and resolved by text after the join, so
//! `../x` lands in `jail`.

use blaze_core::{Done, Effect};
use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root, world, write_synced};
use crate::{Plant, path::Path};

const PARTS: [&str; 7] = ["a", "b", "..", ".", "", "c.d", "..."];

#[derive(Debug)]
struct Plan {
    seed: u64,
    raws: Vec<String>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let raws = (0..d.int(1, 16))
        .map(|_| {
            let rooted = d.one_in(6);
            let parts: Vec<&str> = (0..d.int(1, 5)).map(|_| d.pick(&PARTS)).collect();
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
    for raw in &plan.raws {
        let before = io.touched().len();
        if plant == Plant::LexicalParent {
            // The planted bug: `..` let in and resolved by text.
            let Ok(raw) = Path::with_parents(raw) else { continue };
            let Some(file) = jail.beneath_by_text(&raw) else { continue };
            let done = io.call(&cx, |tag| Effect::Write { file, bytes: b"x".to_vec(), tag });
            if !matches!(done, Ok(Done::Written)) {
                return Err(format!("harness: the planted write of {raw} said {done:?}"));
            }
        } else {
            match Path::new(raw) {
                Ok(made) => {
                    let text = made.as_str();
                    if text != raw || text.starts_with('/') || text.split('/').any(|it| matches!(it, "" | "." | "..")) {
                        return Err(format!("Path::new took {raw:?} as {text:?}: not plain and relative"));
                    }
                    write_synced(&cx, &jail, &made, b"x").map_err(|e| format!("harness: {e}"))?;
                }
                Err(refused) => {
                    if refused.path != *raw {
                        return Err(format!("Path::new refused {raw:?} naming {:?}", refused.path));
                    }
                }
            }
        }
        for (file, _) in &io.touched()[before..] {
            if !file.is_under(jail.name()) || file == jail.name() {
                return Err(format!("the path {raw:?} reached {file}, not beneath {}", jail.name()));
            }
        }
    }
    Ok(())
}

/// The invariant: a `Path` never escapes its `Dir`. Whatever text a path is made of, what
/// `Path::new` takes is plain and relative, and a write through `jail/in` of it names a file
/// beneath `jail/in`.
#[hegel::test(test_cases = 200)]
fn a_path_never_escapes_its_directory(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_parent_resolved_by_text() {
    control(Plant::LexicalParent, |seeds: &mut Seeds| plan(seeds), run);
}
