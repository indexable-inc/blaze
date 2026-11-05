//! A DropZone (den's GenDropZone, genui_prims.rs `gauge_drop` and `files_over`): a sunk plate with its words in the
//! middle over what it holds, lit (`hot`, hn.GenDropHot, fading on the transition spring) while Finder files are carried
//! over it. The window (window.rs) takes file URL drags: as they move it asks [`App::genui_files_over`] whether a zone is
//! under them, which lights that one; let go, [`App::genui_files_dropped`] sends that zone's onDrop the files' absolute
//! paths as a list of strings. Each zone is noted as it is drawn (its cell, its handler, the pane it scrolls in), so a
//! drag finds the innermost one under it within its pane.
use super::*;
use std::collections::BTreeMap;

/// The zones drawn, by their element's entity, and the one files are carried over now.
#[derive(Default)]
pub(super) struct Drops {
    zones: BTreeMap<u64, Zone>,
    pub(super) over: Option<u64>,
}

struct Zone {
    cell: String,
    handler: Option<String>,
    pane: Laid,
}

/// Draw DropZone `node` on `v` (an `hn.GenDropZone`), with its children `kids`.
pub(super) fn element(v: View<'_>, node: &Value, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let id = v.id();
    let hot = b.live.as_deref().is_some_and(|l| l.drops.over == Some(id));
    if let Some(live) = b.live.as_deref_mut() {
        live.drops.zones.insert(id, Zone { cell: b.cell.to_owned(), handler: control::handler(node, "onDrop"), pane: b.pane });
    }
    let label = match words(node, "label") {
        l if l.is_empty() => "Drop files here".to_owned(),
        l => l,
    };
    let mut parts = Vec::with_capacity(3);
    if hot {
        parts.push(("hot", "hn.GenDropHot"));
    }
    parts.push(("label", "hn.GenDropLabel"));
    if !kids.is_empty() {
        parts.push(("body", "hn.GenDropBody"));
    }
    view::for_each_of(v, parts.iter().map(|(k, p)| (*k, *p, *k)), |part, k, _| match k {
        "label" => _ = part.text(&label),
        "body" => children(part, kids, depth + 1, b),
        _ => {}
    });
}

impl App {
    /// Files carried over window point `at` (None: carried off the view, or the drag ended): the DropZone under it
    /// lights. True when one is there to take them (the drag then shows a copy).
    pub(crate) fn genui_files_over(&mut self, at: Option<(f32, f32)>) -> bool {
        let zone = at.and_then(|(x, y)| self.drop_zone_at(x, y));
        let mut live = self.genui.borrow_mut();
        if live.drops.over != zone {
            live.drops.over = zone;
            drop(live);
            self.poke();
        }
        zone.is_some()
    }

    /// Files let go at window point `(x, y)`: the DropZone there sends its onDrop their absolute paths (a list of
    /// strings). True when one took them.
    pub(crate) fn genui_files_dropped(&mut self, x: f32, y: f32, paths: &[String]) -> bool {
        let zone = self.drop_zone_at(x, y);
        self.genui_files_over(None);
        let Some(id) = zone else { return false };
        let live = self.genui.borrow();
        let Some(z) = live.drops.zones.get(&id) else { return false };
        if let Some(h) = &z.handler {
            self.link.ui_event(&z.cell, h, serde_json::json!(paths));
        }
        drop(live);
        self.poke();
        true
    }

    /// The DropZone under window point `(x, y)`: the smallest drawn one whose box holds it, within the pane it scrolls
    /// in. Zones whose elements are gone are forgotten.
    fn drop_zone_at(&self, x: f32, y: f32) -> Option<u64> {
        let z = self.zoom();
        let (x, y) = (x / z, y / z);
        let w = self.world.world_ref();
        let mut live = self.genui.borrow_mut();
        live.drops.zones.retain(|id, _| w.entity_from_id(*id).is_alive());
        live.drops
            .zones
            .iter()
            .filter(|(_, zone)| zone.pane.w <= 0.0 || zone.pane.contains(x, y))
            .filter_map(|(id, _)| w.entity_from_id(*id).try_cloned::<&Laid>().filter(|l| l.contains(x, y)).map(|l| (*id, l.w * l.h)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }
}
