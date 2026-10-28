//! The content scripts run in a Flecs world (flecs-opinionated's `flecs_snapshot::World`, which calls
//! `ecs_script_run`), the build's pre-rendered HTML set on their entities with `World::set`, and the world written as
//! a snapshot of every root (`World::roots`). Scripts the build generates (gen/bench/: the benchmark history) run
//! with the content's, after it.

use ipld_core::ipld::Ipld;
use std::collections::BTreeMap;

/// What the build sets on one entity (TS, at build time): highlighted code, a drawing, or a page's narration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bake {
    /// A Code or Command block's highlighted HTML (highlight.js classes).
    pub highlighted: Option<String>,
    /// HTML drawn at build time (a lesson's glyph, gen/glyphs.ts).
    pub baked: Option<String>,
    /// On a page entity: the URL of its narration's manifest, set when the build has audio that matches the page's
    /// spoken lines (`Audio {manifest}`; the renderer then adds the player).
    pub audio: Option<String>,
}

/// One content script: its name in errors and stable ids (`schema.flecs`, `rfcs/capabilities.flecs`), and its code.
pub struct Script {
    pub name: String,
    pub code: String,
}

/// Defines the components the build sets (`Highlighted`, `Baked`, `Audio`): one string field each.
const BAKE_TYPES: &str = "using flecs.meta\n// Set by the build (site/renderer).\nstruct Highlighted(html: string)\nstruct Baked(html: string)\nstruct Audio(manifest: string)\n";

/// A dotted path of plain names (`site.rfcs.capabilities.code`) as a snapshot path: `\` escaped, and a leading `#`,
/// so a name is never read as an unnamed child (SPEC.md, "Values", on paths).
fn snapshot_path(path: &str) -> String {
    path.split('.')
        .map(|seg| {
            let seg = seg.replace('\\', "\\\\");
            if seg.starts_with('#') { format!("\\{seg}") } else { seg }
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn one(field: &str, v: &str) -> Ipld {
    Ipld::Map([(field.to_string(), Ipld::String(v.to_string()))].into())
}

/// Runs the scripts, sets what the build baked, and returns the world as a flecs-snapshot document (DAG-CBOR).
pub fn snapshot(scripts: &[Script], bakes: &BTreeMap<String, Bake>) -> Result<Vec<u8>, String> {
    let mut world = flecs_snapshot::World::new()?;
    for s in scripts {
        world.run(&s.name, &s.code)?;
    }
    world.run("bake.flecs", BAKE_TYPES)?;
    for (path, b) in bakes {
        let p = snapshot_path(path);
        let set = [
            ("Highlighted", "html", &b.highlighted),
            ("Baked", "html", &b.baked),
            ("Audio", "manifest", &b.audio),
        ];
        for (comp, field, v) in set {
            if let Some(v) = v {
                world.set(&p, comp, &one(field, v)).map_err(|e| format!("bake: {e}"))?;
            }
        }
    }
    let roots = world.roots()?;
    let roots: Vec<&str> = roots.iter().map(String::as_str).collect();
    world.document_bytes(&roots)
}
