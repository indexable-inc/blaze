//! Applies a timeline (`harness_native_ui_timeline_api::Cue`s) at one time. The host calls [`apply`] with the frame clock's
//! time before each `progress`, so layout and draw see the cued values in the same frame. It keeps no state: the values
//! at a time depend on that time and the cues alone, so any frame can be drawn first (a still at 4.2 s, a video from 0).
use std::collections::BTreeMap;
use std::ffi::CString;

use harness_native_flecs::prelude::*;
use harness_native_ui_node_api::Node;
use harness_native_ui_timeline_api::Cue;
use hyperion_ecs::sys;

/// One cue as found: the node it moves and its member split into the component and the path inside it.
struct Found {
    cue: u64,
    node: u64,
    component: String,
    path: String,
    c: Cue,
}

/// Set every cued member to its value at `time` seconds. Err names each cue that cannot be applied (no node above it, a
/// component or member that does not exist or is not a number) and the rule that refused it; nothing is skipped quietly.
pub fn apply(world: WorldRef<'_>, time: f32) -> Result<(), String> {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    world.new_query::<&Cue>().each_entity(|e, c| {
        let path = e.path();
        let node = std::iter::successors(e.parent(), |p| p.parent()).find(|p| p.has(Node::id()));
        let Some(node) = node else {
            errors.push(format!("cue {path}: no Node above it to animate (a Cue is a child of the node it moves)"));
            return;
        };
        let Some((component, member)) = c.member.as_str().split_once('.') else {
            errors.push(format!("cue {path}: member {:?} is not `Component.member`", c.member.as_str()));
            return;
        };
        if c.dur < 0.0 {
            errors.push(format!("cue {path}: dur {} is negative", c.dur));
            return;
        }
        found.push(Found { cue: *e.id(), node: *node.id(), component: component.to_owned(), path: member.to_owned(), c: c.clone() });
    });
    // Each (node, member) in turn, its cues by start (then by entity id, so equal starts settle the same way every run).
    let mut by_member: BTreeMap<(u64, String, String), Vec<&Found>> = BTreeMap::new();
    for f in &found {
        by_member.entry((f.node, f.component.clone(), f.path.clone())).or_default().push(f);
    }
    for ((node, component, path), mut cues) in by_member {
        cues.sort_by(|a, b| a.c.at.total_cmp(&b.c.at).then(a.cue.cmp(&b.cue)));
        let value = match cues.iter().rev().find(|f| f.c.at <= time) {
            Some(f) => {
                let k = if f.c.dur <= 0.0 { 1.0 } else { f.c.ease.at((time - f.c.at) / f.c.dur) };
                f.c.from + (f.c.to - f.c.from) * k
            }
            None => cues[0].c.from,
        };
        if let Err(err) = set(world, node, &component, &path, value) {
            let cue = world.entity_from_id(cues[0].cue).path();
            errors.push(format!("cue {cue}: {component}.{path}: {err}"));
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

/// Write `value` into member `path` of `component` on `node`, through flecs reflection, and tell flecs it changed.
fn set(world: WorldRef<'_>, node: u64, component: &str, path: &str, value: f32) -> Result<(), String> {
    let w = world.raw().as_ptr();
    let name = CString::new(component).map_err(|_| "a NUL in the component's name".to_owned())?;
    let dotted = CString::new(path).map_err(|_| "a NUL in the member's path".to_owned())?;
    // SAFETY: a live world outside progress; the strings outlive the calls; the cursor points into the component's storage
    // on `node`, which nothing else touches until ecs_modified_id below.
    unsafe {
        let comp = sys::ecs_lookup(w, name.as_ptr());
        if comp == 0 {
            return Err("no component by that name".into());
        }
        let size = sys::ecs_get_type_info(w, comp).as_ref().map_or(0, |ti| ti.size);
        if size <= 0 {
            return Err("not a component with data".into());
        }
        let ptr = sys::ecs_ensure_id(w, node, comp, size as usize);
        let mut cur = sys::ecs_meta_cursor(w, comp, ptr);
        if sys::ecs_meta_push(&mut cur) != 0 || sys::ecs_meta_dotmember(&mut cur, dotted.as_ptr()) != 0 {
            return Err("no such member (reflection could not reach it)".into());
        }
        if sys::ecs_meta_set_float(&mut cur, f64::from(value)) != 0 {
            return Err("not a number".into());
        }
        sys::ecs_modified_id(w, node, comp);
    }
    Ok(())
}
