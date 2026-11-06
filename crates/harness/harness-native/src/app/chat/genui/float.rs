//! Tier 3's floating elements (after den's genui.rs and 33-genui-prims.flecs), drawn over the transcript in the Overlay
//! layer, where no Scroll, Split pane or transcript around them cuts them (a lifted node escapes its ancestors' clips).
//! A Popover's or a Menu's trigger stays in the flow; open, its plate is a `Placed` child opened against the trigger on
//! its placement's side (layout flips it and keeps it on screen). A Dialog, a Sheet and a Toast are `Placed` themselves,
//! given the pane's place while they show: a scrim over it with the plate on it, or the toast's plate at its foot. Each
//! open float is noted in [`Live`]'s floats with what closes it, so a press elsewhere (a popover, a menu) and Escape
//! (genui/control.rs) send that. A modal's transcript row is kept while the modal is open, in view or not (chat.rs).
use super::*;
use control::{Float, Toast, arm, handler, inert, sends};
use harness_native_ui_input_api::Hovered;
use harness_native_ui_layout_api::{Align, Opens, Side, Viewport};
use harness_native_ui_node_api::Clip;

/// The pane a cell's row floats its dialogs and toasts over: the nearest node around it that clips (the transcript's
/// list, the gallery's page), as laid out last frame; the window for a row in none.
pub(super) fn pane_of(row: View<'_>) -> Laid {
    let world = row.entity().world();
    let around = std::iter::successors(row.entity().parent(), |p| p.parent()).take(64).find(|p| p.has(Clip::id()));
    match around {
        Some(p) => p.try_cloned::<&Laid>().unwrap_or_default(),
        None => world.try_get::<&Viewport>(|v| Laid { x: 0.0, y: 0.0, w: v.w, h: v.h }).unwrap_or_default(),
    }
}

/// Whether any of node `e` shows through the clips around it, as laid out last frame (the draw pass's clips: up to a node
/// lifted into a higher Layer, which nothing around it cuts). Not laid out yet counts as shown.
pub(super) fn in_view(e: EntityView<'_>) -> bool {
    let Some(mut r) = e.try_cloned::<&Laid>() else { return true };
    let mut at = Some(e);
    while let Some(n) = at.filter(|n| n.has(harness_native_ui_node_api::Node::id())) {
        if n.try_cloned::<&harness_native_ui_node_api::Layer>().is_some_and(|l| l.level > harness_native_ui_node_api::Level::Base) {
            break;
        }
        let p = n.parent();
        if let Some(c) = p.filter(|p| p.has(Clip::id())).and_then(|p| p.try_cloned::<&Laid>()) {
            let (x, y) = (r.x.max(c.x), r.y.max(c.y));
            r = Laid { x, y, w: (r.x + r.w).min(c.x + c.w) - x, h: (r.y + r.h).min(c.y + c.h) - y };
            if r.w <= 0.0 || r.h <= 0.0 {
                return false;
            }
        }
        at = p;
    }
    true
}

/// Draw floating element `node` (of `kind`) on `v`, with its children `kids`.
pub(super) fn element(v: View<'_>, node: &Value, kind: &str, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    match kind {
        "Popover" | "Menu" => popover(v, node, kind == "Menu", kids, depth, b),
        "Dialog" | "Sheet" => modal(v, node, kind == "Sheet", kids, depth, b),
        "Toast" => toast(v, node, b),
        _ => {}
    }
}

/// Give a placed float the pane's place (`shown`), or none at its corner.
fn over_pane(v: View<'_>, pane: Laid, shown: bool) {
    let want = if shown { pane } else { Laid { w: 0.0, h: 0.0, ..pane } };
    if v.entity().try_cloned::<&Laid>() != Some(want) {
        v.set(want);
    }
}

/// Whether the pointer is on `part` or anything in it (the app's `Hovered`, as last resolved).
fn pointer_on(part: View<'_>) -> bool {
    let w = part.entity().world();
    let Some(h) = w.try_lookup(harness_native_api::APP).and_then(|app| app.target(Hovered::id(), 0)) else { return false };
    std::iter::successors(Some(h), |n| n.parent()).take(64).any(|n| *n.id() == part.id())
}

/// A Popover or a Menu: its trigger inline (its first child; a press on it sends onToggle), and while it is open its plate
/// floating by the trigger on the side `placement` says, at its start or centred on it (`align`). A popover whose `trigger`
/// is `hover` is open while the pointer is on its trigger or its plate (den's: at once, both ways), telling its onToggle
/// each change. A `context` menu's children are a region instead, which a right press opens it in, at the pointer, until a
/// press elsewhere, a pick or Escape ([`Live`]'s context; control.rs `genui_context`).
fn popover(v: View<'_>, node: &Value, menu: bool, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let cell = b.cell;
    let id = v.id();
    let toggle = handler(node, "onToggle");
    let hover = !menu && prop(node, "trigger").as_str() == Some("hover");
    let context = menu && prop(node, "context") == &Value::Bool(true);
    let said = prop(node, "open") == &Value::Bool(true);
    let lead = if context { "region" } else { "trigger" };
    let open = match b.live.as_deref_mut() {
        Some(live) if hover => {
            // On its trigger or its plate, or crossing the gap between them (the plate's own window, overlay.rs).
            let on = [v.part("trigger"), v.part("float")].into_iter().flatten().any(pointer_on)
                || v.part("trigger").zip(v.part("float")).is_some_and(|(t, f)| view::bridged(t.entity(), f.entity()));
            let was = live.hovering.insert(id, on).unwrap_or(said);
            if was != on
                && let Some(h) = &toggle
            {
                live.pending.push((cell.to_owned(), h.clone(), Value::Bool(on)));
            }
            on
        }
        Some(live) if context => live.context.contains_key(&id),
        _ => said,
    };
    // Open, its plate floats while its trigger is in view: one scrolled out of its Scroll (or the transcript) leaves no
    // plate hanging over what is there now; it comes back with the trigger, still open.
    let shows = open && v.part(lead).is_none_or(|t| in_view(t.entity()));
    let plate = if menu { "hn.GenMenuPlate" } else { "hn.GenFloat" };
    let head = if context { ("region", "hn.GenRegion") } else { ("trigger", "hn.GenTrigger") };
    let mut parts = vec![head]; // alloc: a float's two or three parts
    if shows && context {
        parts.push(("at", "hn.GenPoint"));
    }
    if shows {
        parts.push(("float", plate));
    }
    view::for_each_of(v, parts.iter().map(|(k, p)| (*k, *p, ())), |_, (), _| {});
    let Some(trigger) = v.part(lead) else { return };
    if context {
        // A right press in it opens the menu there; a left press is its children's.
        if let Some(live) = b.live.as_deref_mut() {
            live.regions.insert(trigger.id(), control::Region { cell: cell.to_owned(), menu: id, toggle: toggle.clone() });
        }
        children(trigger, kids, depth + 1, b);
    } else {
        // A hover popover opens without a press; a click one's trigger toggles it.
        let events: Vec<(String, Value)> = toggle.iter().filter(|_| !hover).map(|h| (h.clone(), Value::Bool(!open))).collect();
        arm(trigger, (!events.is_empty()).then(|| sends(cell, &events)).as_deref());
        b.next = Within::Trigger(events);
        children(trigger, &kids[..kids.len().min(1)], depth + 1, b);
    }
    let Some(f) = v.part("float") else { return };
    let gap = view::prefab(v.entity().world(), "hn.GenFloat").and_then(|p| p.try_cloned::<&Side>()).map_or(0.0, |s| s.dy);
    let along = if prop(node, "align").as_str() == Some("center") { Align::Center } else { Align::Start };
    let side = match (context, prop(node, "placement").as_str()) {
        // At the pointer: its corner on the point the right press was, flipped up or slid to stay on screen.
        (true, _) => Side { edge: Opens::Bottom, dx: 0.0, dy: 0.0, align: Align::Start },
        (_, Some("top")) => Side { edge: Opens::Top, dx: 0.0, dy: gap, align: along },
        (_, Some("left")) => Side { edge: Opens::Left, dx: gap, dy: 0.0, align: along },
        (_, Some("right")) => Side { edge: Opens::Right, dx: gap, dy: 0.0, align: along },
        _ => Side { edge: Opens::Bottom, dx: 0.0, dy: gap, align: along },
    };
    match (context, v.part("at")) {
        (true, Some(at)) => {
            let off = b.live.as_deref().and_then(|l| l.context.get(&id).copied()).unwrap_or_default();
            let r = trigger.entity().try_cloned::<&Laid>().unwrap_or_default();
            let point = Laid { x: r.x + off[0], y: r.y + off[1], w: 0.0, h: 0.0 };
            if at.entity().try_cloned::<&Laid>() != Some(point) {
                at.set(point);
            }
            f.anchored(at.id());
        }
        _ => {
            f.anchored(trigger.id());
        }
    }
    if f.entity().try_cloned::<&Side>() != Some(side) {
        f.set(side);
    }
    if menu {
        rows(f, node, cell, toggle.as_deref(), context.then_some(id));
    } else {
        children(f, kids.get(1..).unwrap_or_default(), depth + 1, b);
    }
    // What a press off it or Escape closes (a hover popover closes as the pointer leaves).
    if let Some(live) = b.live.as_deref_mut()
        && !hover
        && (toggle.is_some() || context)
    {
        let close = toggle.map(|h| (h, Value::Bool(false))).into_iter().collect();
        live.floats.insert(id, Float { cell: cell.to_owned(), close, modal: false });
    }
}

/// A Menu's rows on its plate: a pick sends onSelect(value) and then onToggle(false) (the kernel closes it on a pick as
/// well), and a context menu (`shut`, its element) shuts here; a disabled row is faded and takes no press; a separator is
/// a hairline.
fn rows(f: View<'_>, node: &Value, cell: &str, toggle: Option<&str>, shut: Option<u64>) {
    let items = props::items(node).unwrap_or_default();
    let pick = handler(node, "onSelect");
    let rows = items.iter().enumerate().map(|(i, it)| {
        let prefab = match it {
            props::Item::Separator => "hn.GenMenuSep",
            props::Item::Pick { danger: true, .. } => "hn.GenMenuDanger",
            props::Item::Pick { .. } => "hn.GenMenuItem",
        };
        (format!("i{i}"), prefab, (it, prefab))
    });
    view::for_each_of(f, rows, |row, (it, prefab), _| {
        let props::Item::Pick { label, value, disabled, keys, .. } = it else { return };
        if let Some(l) = row.part("label") {
            l.text(label);
        }
        if let Some(k) = row.part("keys") {
            k.text(keys);
        }
        control::gap(row, prefab, keys);
        let events: Vec<(String, Value)> =
            pick.iter().map(|p| (p.clone(), value.clone())).chain(toggle.map(|t| (t.to_owned(), Value::Bool(false)))).collect();
        let takes = !*disabled && pick.is_some();
        let action = || match shut {
            Some(el) => control::then(cell, &events, None, Some(el)),
            None => sends(cell, &events),
        };
        arm(row, takes.then(action).as_deref());
        inert(row, *disabled);
    });
}

/// A Dialog (its plate centred on the scrim) or a Sheet (along the pane's foot): open, over the pane; a press on the scrim
/// sends onClose(false) when it may be dismissed.
fn modal(v: View<'_>, node: &Value, sheet: bool, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let cell = b.cell;
    let open = prop(node, "open") == &Value::Bool(true);
    over_pane(v, b.pane, open);
    let (scrim, plate) = if sheet { ("hn.GenScrimFoot", "hn.GenSheetPlate") } else { ("hn.GenScrim", "hn.GenDialogPlate") };
    view::for_each_of(v, open.then_some(("scrim", scrim, ())), |_, (), _| {});
    let Some(s) = v.part("scrim") else { return };
    let dismissable = prop(node, "dismissable") != &Value::Bool(false);
    let close: Vec<(String, Value)> = handler(node, "onClose").filter(|_| dismissable).map(|h| (h, Value::Bool(false))).into_iter().collect();
    arm(s, (!close.is_empty()).then(|| sends(cell, &close)).as_deref());
    view::for_each_of(s, [("plate", plate, ())], |_, (), _| {});
    let Some(p) = s.part("plate") else { return };
    let title = words(node, "title");
    let parts: &[(&str, &str)] = if title.is_empty() { &[("body", "hn.GenFloatBody")] } else { &[("title", "hn.GenFloatTitle"), ("body", "hn.GenFloatBody")] };
    view::for_each_of(p, parts.iter().map(|(k, pf)| (*k, *pf, *k)), |part, k, _| {
        if k == "title" {
            part.text(&title);
        }
    });
    if let Some(body) = p.part("body") {
        children(body, kids, depth + 1, b);
    }
    if let Some(live) = b.live.as_deref_mut() {
        live.floats.insert(v.id(), Float { cell: cell.to_owned(), close, modal: true });
    }
}

/// A Toast: its plate at the pane's foot (a dot in its tone, its words) until its `duration` (ms; 0 stays) runs out or it
/// is pressed, either sending onDismiss(null); it shows again when its words change. Drawn without the app's controls in
/// use (a test), it shows.
fn toast(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) {
    let cell = b.cell;
    let now = b.now;
    let text = words(node, "text");
    let dismiss = handler(node, "onDismiss");
    let ms = control::number(node, "duration").unwrap_or(4000.0);
    let shown = match b.live.as_deref_mut() {
        None => true,
        Some(live) => {
            let t = live.toasts.entry(v.id()).or_insert_with(|| Toast { text: text.clone(), since: now, gone: false });
            if t.text != text {
                *t = Toast { text: text.clone(), since: now, gone: false };
            }
            if ms > 0.0 && !t.gone && f64::from(now - t.since) * 1000.0 >= ms {
                t.gone = true;
                if let Some(h) = &dismiss {
                    live.pending.push((cell.to_owned(), h.clone(), Value::Null));
                }
            }
            let shown = !t.gone;
            // Its time runs on the clock: keep drawing until it is up.
            live.anim |= shown && ms > 0.0;
            shown
        }
    };
    over_pane(v, b.pane, shown);
    view::for_each_of(v, shown.then_some(("plate", "hn.GenToastPlate", ())), |_, (), _| {});
    let Some(p) = v.part("plate") else { return };
    if let Some(t) = p.part("text") {
        t.text(&text);
    }
    let tone = words(node, "tone");
    let world = v.entity().world();
    let ink = view::prefab(world, &format!("hn.GenTone.{}", if tone.is_empty() { "ink" } else { &tone })).and_then(|e| e.try_cloned::<&Fill>());
    if let (Some(m), Some(f)) = (p.part("mark"), ink)
        && m.entity().try_cloned::<&Fill>() != Some(f)
    {
        m.set(f);
    }
    arm(p, Some(&control::toast_action(cell, v.id(), dismiss)));
}
