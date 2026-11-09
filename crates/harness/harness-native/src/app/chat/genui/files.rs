//! A File: one row of a listing (den's genui.rs File slot and 30-genui.flecs GenFile). Its kind's drawing in its own
//! colours (media.rs's file kinds: the `kind` prop, else guessed from the name; a trailing `/` is a folder), its name, then
//! its size (Finder's 1000-based units) and when it changed, quiet. With an onClick a press sends its `value`, else its
//! name. A File given a `path` is the FileRef chip: the kernel sends it as type FileRef, so it never reaches here.
use super::*;
use control::{arm, handler, send};

/// Draw File `node` on `v` (an `hn.GenFile`, or `hn.GenFileOn` when it has an onClick).
pub(super) fn element(v: View<'_>, node: &Value, b: &Budget<'_, '_>) {
    let name = words(node, "name");
    let kind = match prop(node, "kind").as_str() {
        Some(k) => media::kind_named(k).unwrap_or("text"),
        None => media::kind_of(&name),
    };
    if let Some(i) = v.part("icon") {
        i.drawing(media::files::drawing(kind));
    }
    if let Some(n) = v.part("name") {
        n.text(name.trim_end_matches('/'));
    }
    if let Some(m) = v.part("meta") {
        m.text(&meta(node));
    }
    let value = match prop(node, "value") {
        Value::Null => Value::String(name.clone()),
        x => x.clone(),
    };
    arm(v, handler(node, "onClick").map(|h| send(b.cell, &h, value)).as_deref());
}

/// A File's quiet words (den's `detail`): its size and when it changed, a dot apart; either alone, or none.
fn meta(node: &Value) -> String {
    let size = prop(node, "size").as_f64().filter(|s| s.is_finite() && *s >= 0.0).map(size_label);
    let modified = prop(node, "modified").as_str().map(str::trim).filter(|m| !m.is_empty()).map(str::to_owned);
    [size, modified].into_iter().flatten().collect::<Vec<_>>().join(" \u{00B7} ")
}

/// `bytes` as Finder says a size (den's `size_label`): 1000-based, whole bytes, else one decimal of KB, MB or GB.
pub(super) fn size_label(bytes: f64) -> String {
    let mut v = bytes;
    for unit in ["B", "KB", "MB"] {
        if v < 1000.0 {
            return if unit == "B" { format!("{v:.0} B") } else { format!("{v:.1} {unit}") };
        }
        v /= 1000.0;
    }
    format!("{v:.1} GB")
}
