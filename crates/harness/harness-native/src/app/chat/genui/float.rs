//! Tier 3's floating elements (after den's genui.rs and 33-genui-prims.flecs), drawn over the transcript in the Overlay
//! layer, where no Scroll, Split pane or transcript around them cuts them (a lifted node escapes its ancestors' clips).
//! A Popover's or a Menu's trigger stays in the flow; open, its plate is a `Placed` child opened against the trigger on
//! its placement's side (layout flips it and keeps it on screen). A Dialog, a Sheet and a Toast are `Placed` themselves,
//! given the pane's place while they show: a scrim over it with the plate on it, or the toast's plate at its foot. Each
//! open float is noted in [`Live`]'s floats with what closes it, so a press elsewhere (a popover, a menu) and Escape
//! (genui/control.rs) send that. A modal's transcript row is kept while the modal is open, in view or not (chat.rs).
use super::*;
use control::{Float, Toast, arm, handler, inert, sends};
use harness_native_ui_layout_api::{Opens, Side, Viewport};
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
fn in_view(e: EntityView<'_>) -> bool {
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

fn popover(v: View<'_>, node: &Value, menu: bool, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let cell = b.cell;
    let toggle = handler(node, "onToggle");
    // Open, its plate floats while its trigger is in view: one scrolled out of its Scroll (or the transcript) leaves no
    // plate hanging over what is there now; it comes back with the trigger, still open.
    let open = prop(node, "open") == &Value::Bool(true);
    let shows = open && v.part("trigger").is_none_or(|t| in_view(t.entity()));
    let plate = if menu { "hn.GenMenuPlate" } else { "hn.GenFloat" };
    let parts: &[(&str, &str)] = if shows { &[("trigger", "hn.GenTrigger"), ("float", plate)] } else { &[("trigger", "hn.GenTrigger")] };
    view::for_each_of(v, parts.iter().map(|(k, p)| (*k, *p, ())), |_, (), _| {});
    let Some(trigger) = v.part("trigger") else { return };
    let events: Vec<(String, Value)> = toggle.iter().map(|h| (h.clone(), Value::Bool(!open))).collect();
    arm(trigger, (!events.is_empty()).then(|| sends(cell, &events)).as_deref());
    b.next = Within::Trigger(events);
    children(trigger, &kids[..kids.len().min(1)], depth + 1, b);
    let Some(f) = v.part("float") else { return };
    f.anchored(trigger.id());
    let gap = view::prefab(v.entity().world(), "hn.GenFloat").and_then(|p| p.try_cloned::<&Side>()).map_or(0.0, |s| s.dy);
    let side = match prop(node, "placement").as_str() {
        Some("top") => Side { edge: Opens::Top, dx: 0.0, dy: gap },
        Some("left") => Side { edge: Opens::Left, dx: gap, dy: 0.0 },
        Some("right") => Side { edge: Opens::Right, dx: gap, dy: 0.0 },
        _ => Side { edge: Opens::Bottom, dx: 0.0, dy: gap },
    };
    if f.entity().try_cloned::<&Side>() != Some(side) {
        f.set(side);
    }
    if menu {
        rows(f, node, cell, toggle.as_deref());
    } else {
        children(f, kids.get(1..).unwrap_or_default(), depth + 1, b);
    }
    if let (Some(h), Some(live)) = (toggle, b.live.as_deref_mut()) {
        live.floats.insert(v.id(), Float { cell: cell.to_owned(), close: vec![(h, Value::Bool(false))], modal: false });
    }
}

/// A Menu's rows on its plate: a pick sends onSelect(value) and then onToggle(false) (the kernel closes it on a pick as
/// well); a disabled row is faded and takes no press; a separator is a hairline.
fn rows(f: View<'_>, node: &Value, cell: &str, toggle: Option<&str>) {
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
        arm(row, takes.then(|| sends(cell, &events)).as_deref());
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
