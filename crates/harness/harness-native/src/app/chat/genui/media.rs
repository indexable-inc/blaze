//! Tier 3's status, pictures, chips and files (after den's genui.rs, filetype.rs and 33-genui-prims.flecs): Progress,
//! Loading and Skeleton (the last two move on the clock), Icon (a file kind's drawing in its own colours, or a Nerd Font
//! symbol), Image (a photo file, decoded by harness-native-render's photo.rs), Chip, Tile and FileRef.
use super::*;
use crate::look::{ShowAfter, Sweep};
use control::{arm, handler, send, then};
use harness_native_api::spring::{Spring, Tune};
use harness_native_ui_input_api::Tooltip;
use harness_native_ui_layout_api::{Aspect, SizeKind};
use harness_native_ui_motion_api::{Motion, Pose};
use harness_native_ui_text_api::{Kip, Picture, Vector};

// The file kinds and their drawings (one table, shared with the words' inline <Path>): harness-native-ui-inline's
// files.rs.
pub(super) use harness_native_ui_inline::files::{self, KIND_NAMES, kind_named, kind_of};

/// A spinner turns once a second; a skeleton breathes once every [`BREATH`] seconds, fading by up to [`BREATH_FADE`].
const BREATH: f32 = 1.6;
const BREATH_FADE: f32 = 0.5;

/// What an Icon draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Drawn {
    /// A file kind's drawing, in its own colours.
    Kind(&'static str),
    /// A symbol of the bundled Nerd Font, in the icon's ink.
    Symbol(char),
}

/// What an Icon named `name` draws: a file kind's drawing, or the bundled Nerd Font's symbol of that name (`cod-check`;
/// a bare `check` is a codicon; `-` and `_` within a name are one).
pub(super) fn glyph(name: &str) -> Option<Drawn> {
    if let Some(k) = kind_named(name) {
        return Some(Drawn::Kind(files::drawing(k)));
    }
    let us = name.replace('-', "_");
    let tail = name.split_once('-').map(|(p, r)| format!("{p}-{}", r.replace('-', "_")));
    let tries = [Some(name.to_owned()), tail, Some(format!("cod-{name}")), Some(format!("cod-{us}"))];
    tries.into_iter().flatten().find_map(|t| harness_native_render::text::nerd_symbol(&t)).map(Drawn::Symbol)
}

thread_local! {
    /// Each photo's size by path, with the file's change time and length it was read at.
    static SIZES: std::cell::RefCell<std::collections::HashMap<String, (Option<std::time::SystemTime>, u64, Result<(u32, u32), String>)>> = Default::default();
}

/// The size of the photo at `src` (pixels), read once for each version of the file; an error names the file and why
/// (missing, not a PNG or JPEG, undecodable).
pub(super) fn photo_size(src: &str) -> Result<(u32, u32), String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("{src}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{src}: not a file"));
    }
    let stamp = (meta.modified().ok(), meta.len());
    SIZES.with(|c| {
        if let Some((m, l, r)) = c.borrow().get(src)
            && (*m, *l) == stamp
        {
            return r.clone();
        }
        let r = harness_native_render::photo::size(std::path::Path::new(src));
        let mut c = c.borrow_mut();
        if c.len() >= 256 {
            c.clear();
        }
        c.insert(src.to_owned(), (stamp.0, stamp.1, r.clone()));
        r
    })
}

/// Set `v`'s Size to `want` when it is not that already.
fn sized(v: View<'_>, want: Size) {
    if v.entity().try_cloned::<&Size>() != Some(want) {
        v.set(want);
    }
}

/// The Fill of the scene's prefab `name`.
pub(super) fn fill_of(world: WorldRef<'_>, name: &str) -> Option<Fill> {
    view::prefab(world, name).and_then(|p| p.try_cloned::<&Fill>())
}

pub(super) fn filled(v: View<'_>, f: Option<Fill>) {
    if let Some(f) = f
        && v.entity().try_cloned::<&Fill>() != Some(f)
    {
        v.set(f);
    }
}

/// Draw tier-3 element `node` (of `kind`) on `v`, with its children `kids` (a Tile's).
pub(super) fn element(v: View<'_>, node: &Value, kind: &str, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let cell = b.cell;
    let world = v.entity().world();
    match kind {
        "Progress" => progress(v, node, b),
        "Loading" => {
            let label = match prop(node, "label") {
                Value::Null => "Loading".to_owned(),
                _ => words(node, "label"),
            };
            if let Some(l) = v.part("label") {
                l.text(&label);
            }
            let shown = pending(v, b);
            for part in ["spinner", "label"] {
                if let Some(p) = v.part(part) {
                    faded(p, 1.0 - shown);
                }
            }
            // The arc turns about the middle of its 16-unit box, once a second.
            if let Some(arc) = v.part("spinner").and_then(|s| s.part("arc"))
                && let Some(mut vec) = arc.entity().try_cloned::<&Vector>()
            {
                let turn = std::f32::consts::TAU * b.now.rem_euclid(1.0);
                let (s, c) = turn.sin_cos();
                let (cx, cy) = (vec.view_w * 0.5, vec.view_h * 0.5);
                let m = [c, s, -s, c, cx - cx * c + cy * s, cy - cx * s - cy * c];
                if [vec.ma, vec.mb, vec.mc, vec.md, vec.me, vec.mf] != m {
                    [vec.ma, vec.mb, vec.mc, vec.md, vec.me, vec.mf] = m;
                    arc.set(vec);
                }
            }
            if let Some(live) = b.live.as_deref_mut() {
                live.anim = true;
            }
        }
        "Skeleton" => {
            let n = prop(node, "lines").as_u64().unwrap_or(3).clamp(1, 20) as usize;
            let bones = (0..n).map(|i| (format!("b{i}"), if n > 1 && i + 1 == n { "hn.GenBoneShort" } else { "hn.GenBone" }, ()));
            // Its breath, under its wait to show: each bone fades by both.
            let breath = BREATH_FADE * 0.5 * (1.0 - (std::f32::consts::TAU * b.now / BREATH).cos());
            let shown = pending(v, b);
            view::for_each_of(v, bones, |bone, (), _| faded(bone, 1.0 - (1.0 - breath) * shown));
            if let Some(live) = b.live.as_deref_mut() {
                live.anim = true;
            }
        }
        "Icon" => {
            // `kind` (a file kind) says it plainly; else `name`, a kind or a symbol.
            let drawn = match prop(node, "kind").as_str() {
                Some(k) => kind_named(k).map(|k| Drawn::Kind(files::drawing(k))),
                None => glyph(&words(node, "name")),
            };
            match drawn {
                Some(Drawn::Kind(d)) => _ = v.drawing(d),
                Some(Drawn::Symbol(g)) => {
                    v.icon(&g.to_string());
                    filled(v, fill_of(world, "hn.GenIcon"));
                }
                None => {}
            }
        }
        "Image" => image(v, node),
        "Kip" => {
            // What it plays is set once, as it first shows: live (its agent at work) it plays its moods in turn, from history
            // it stands in its last. A tree redrawn later keeps that, so a reply that grows does not replay it.
            let had = v.entity().try_cloned::<&Kip>();
            let size = control::number(node, "size").filter(|s| props::KIP_SIZES.contains(s)).unwrap_or(64.0) as f32;
            let settle = had.as_ref().map_or(!b.playing, |k| k.settle);
            let kip = Kip { mood: words(node, "mood").as_str().into(), size, centre: false, settle };
            if had.as_ref() != Some(&kip) {
                v.set(kip);
            }
        }
        "Chip" => {
            if let Some(l) = v.part("label") {
                l.text(&words(node, "label"));
            }
            let value = match prop(node, "value") {
                Value::Null => Value::Bool(prop(node, "selected") != &Value::Bool(true)),
                x => x.clone(),
            };
            arm(v, handler(node, "onClick").map(|h| send(cell, &h, value)).as_deref());
        }
        "Tile" => {
            let h = handler(node, "onClick");
            spacing(v, if h.is_some() { "hn.GenTileOn" } else { "hn.GenTile" }, node);
            arm(v, h.map(|h| send(cell, &h, prop(node, "value").clone())).as_deref());
            children(v, kids, depth + 1, b);
        }
        "FileRef" => {
            let path = words(node, "path");
            // Its kind as said, else as its name says; its name as said, else the file's.
            let kind = prop(node, "kind").as_str().and_then(kind_named).unwrap_or_else(|| kind_of(&path));
            if let Some(i) = v.part("icon") {
                i.drawing(files::drawing(kind));
            }
            let said = words(node, "name");
            let name = if said.is_empty() { path.trim_end_matches('/').rsplit('/').next().unwrap_or(&path) } else { said.as_str() };
            if let Some(n) = v.part("name") {
                n.text(name);
            }
            let line = prop(node, "line").as_u64();
            if let Some(l) = v.part("line") {
                l.text(&line.map(|n| format!(":{n}")).unwrap_or_default());
            }
            let tip = Tooltip { text: line.map_or_else(|| path.clone(), |n| format!("{path}:{n}")).as_str().into() };
            if v.entity().try_cloned::<&Tooltip>() != Some(tip.clone()) {
                v.set(tip);
            }
            // `open:<line>:<path>` opens it at its line (src/open.rs); with an onClick, that hears the path once it opened.
            let target = line.map_or_else(|| path.clone(), |n| format!("{n}:{path}"));
            let action = match handler(node, "onClick") {
                Some(h) => then(cell, &[(h, Value::String(path.clone()))], Some(&target), None),
                None => format!("open:{target}"),
            };
            arm(v, Some(&action));
        }
        _ => {}
    }
}

/// Set `v`'s Pose to fade by `fade` (0 shown, 1 gone), or none at all when it is shown whole.
fn faded(v: View<'_>, fade: f32) {
    let had = v.entity().try_cloned::<&Pose>();
    if fade <= 1e-4 {
        if had.is_some() {
            v.entity().remove(Pose::id());
        }
        return;
    }
    let pose = Pose { fade: fade.min(1.0), ..Pose::default() };
    if had != Some(pose) {
        v.set(pose);
    }
}

/// How far a Loading or a Skeleton `v` has come in, 0 to 1 (den's PendingLook): unseen for hn.GenPending's delay after it
/// first showed, then on its spring, worked out in closed form from the time since, so it is the same however the frames
/// fall. Drawn without the app's controls in use (a test), it is in whole. Keeps drawing until it settles.
fn pending(v: View<'_>, b: &mut Budget<'_, '_>) -> f32 {
    let Some(live) = b.live.as_deref_mut() else { return 1.0 };
    let born = *live.born.entry(v.id()).or_insert(b.now);
    // The scene is compiled in: hn.GenPending says both.
    let look = view::prefab(v.entity().world(), "hn.GenPending");
    let (Some(ShowAfter { delay }), Some(m)) = (look.and_then(|p| p.try_cloned::<&ShowAfter>()), look.and_then(|p| p.try_cloned::<&Motion>())) else {
        return 1.0;
    };
    let tune = Tune { omega: m.omega, zeta: m.zeta };
    let since = b.now - born - delay;
    if since <= 0.0 {
        live.anim = true;
        return 0.0;
    }
    let mut s = Spring::at(0.0);
    s.step(1.0, tune, since);
    if s.settled(1.0) {
        return 1.0;
    }
    live.anim = true;
    s.x.clamp(0.0, 1.0)
}

/// A Progress: its track's `fill` and `rest` weighed as value over max (both `Animate` on one spring, so a new value glides
/// there and the two always fill the track; a first one lands). Below its max a sheen sweeps along the fill (den
/// ui/progress.rs's shimmer, hn.GenProgressTrack's `sheen`: its band eased across, whole inside the fill, fainter as an end
/// cuts it); with no value (how far is not known) the band sweeps the whole track in the fill's ink. It keeps drawing only
/// while a sheen sweeps where it can be seen; the glide asks for its own frames.
fn progress(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) {
    let world = v.entity().world();
    let max = control::number(node, "max").filter(|m| *m > 0.0).unwrap_or(1.0);
    let frac = control::number(node, "value").map(|x| (x / max).clamp(0.0, 1.0) as f32);
    let Some(track) = control::labelled(v, &words(node, "label"), "hn.GenProgressTrack", None) else { return };
    let f = frac.unwrap_or(0.0);
    // A Grow of 0 counts as 1: the empty side keeps a sliver of weight instead.
    for (part, w) in [("fill", f), ("rest", 1.0 - f)] {
        let Some(p) = track.part(part) else { continue };
        // An empty fill (none yet, or no value) is not drawn: its sliver of weight would show as a hairline.
        if part == "fill" {
            faded(p, if f <= 0.0 { 1.0 } else { 0.0 });
        }
        let mut s = p.entity().try_cloned::<&Size>().unwrap_or_default();
        let w = w.max(1e-4);
        if (s.w.value - w).abs() > 1e-5 {
            s.w.value = w;
            p.set(s);
        }
    }
    let Some(sheen) = track.part("sheen") else { return };
    let Some(band) = sheen.part("band") else { return };
    // Where it sweeps, in the track's points as laid out last frame: the fill (gliding or not) below max, or all of it.
    let width = |p: Option<View<'_>>| p.and_then(|p| p.entity().try_cloned::<&Laid>()).map_or(0.0, |l| l.w);
    let whole = width(Some(track));
    let span = match frac {
        None => whole,
        Some(f) if f < 1.0 => width(track.part("fill")),
        Some(_) => 0.0,
    };
    let sweep = view::prefab(world, "hn.GenProgressSweep").and_then(|p| p.try_cloned::<&Sweep>()).unwrap_or_default();
    let ink = match frac {
        None => track.part("fill").and_then(|p| p.entity().try_cloned::<&Fill>()),
        Some(_) => fill_of(world, "hn.GenProgressSweep"),
    };
    filled(band, ink);
    let sweeps = span >= 1.0 && sweep.period > 0.0;
    let (l, r, alpha) = if sweeps {
        let phase = (b.now / sweep.period).rem_euclid(1.0);
        let eased = phase * phase * (3.0 - 2.0 * phase);
        let len = span * sweep.share;
        let x0 = -len + (span + len) * eased;
        let (a, z) = (x0.max(0.0), (x0 + len).min(span));
        let shown = ((z - a) / len.max(1e-3)).clamp(0.0, 1.0);
        (a, (whole - z).max(0.0), if z - a < 0.5 { 0.0 } else { sweep.low + (1.0 - sweep.low) * shown })
    } else {
        (0.0, whole, 0.0)
    };
    let pad = Padding { l, r, t: 0.0, b: 0.0 };
    if sheen.entity().try_cloned::<&Padding>() != Some(pad) {
        sheen.set(pad);
    }
    faded(band, 1.0 - alpha);
    if sweeps
        && float::in_view(track.entity())
        && let Some(live) = b.live.as_deref_mut()
    {
        live.anim = true;
    }
}

/// An Image: its box the `width` and `height` said (the photo contained in it in its own shape, or covering it: the
/// painter cuts the photo to the shape it is laid out at), one of them (the other from the photo's shape), or neither
/// (the photo's own size, at most the column wide and `hn.GenImage`'s height tall). `alt` is its tooltip.
fn image(v: View<'_>, node: &Value) {
    let src = words(node, "src");
    let Ok((nw, nh)) = photo_size(&src) else { return };
    let Some(pic) = v.part("pic") else { return };
    let (nw, nh) = (nw as f32, nh as f32);
    let aspect = nw / nh;
    let cap = view::prefab(v.entity().world(), "hn.GenImage").and_then(|p| p.try_cloned::<&Size>()).map_or(0.0, |s| s.h.max);
    let cover = prop(node, "fit").as_str() == Some("cover");
    let fixed = Sizing::fixed;
    let (outer, inner) = match (points(node, "width"), points(node, "height")) {
        (Some(w), Some(h)) => {
            let k = (w / nw).min(h / nh);
            let inner = if cover { Size { w: fixed(w), h: fixed(h) } } else { Size { w: fixed(nw * k), h: fixed(nh * k) } };
            (Size { w: fixed(w), h: fixed(h) }, inner)
        }
        (Some(w), None) => (Size { w: fixed(w), h: Sizing::FIT }, Size { w: fixed(w), h: Sizing::FIT }),
        (None, Some(h)) => (Size { w: Sizing::FIT, h: fixed(h) }, Size { w: Sizing::FIT, h: fixed(h) }),
        (None, None) => {
            let most = if cap > 0.0 { nw.min(cap * aspect) } else { nw };
            let w = Sizing { kind: SizeKind::Grow, value: 1.0, min: 0.0, max: most };
            (Size { w, h: Sizing::FIT }, Size { w, h: Sizing::FIT })
        }
    };
    sized(v, outer);
    sized(pic, inner);
    let shape = Aspect { ratio: aspect };
    if pic.entity().try_cloned::<&Aspect>() != Some(shape) {
        pic.set(shape);
    }
    if pic.entity().try_cloned::<&Picture>().is_none_or(|p| p.handle.as_str() != src) {
        pic.set(Picture { handle: src.as_str().into() });
    }
    let alt = words(node, "alt");
    match (alt.is_empty(), v.entity().try_cloned::<&Tooltip>()) {
        (true, Some(_)) => {
            v.entity().remove(Tooltip::id());
        }
        (false, t) if t.as_ref().is_none_or(|t| t.text.as_str() != alt) => {
            v.set(Tooltip { text: alt.as_str().into() });
        }
        _ => {}
    }
}
