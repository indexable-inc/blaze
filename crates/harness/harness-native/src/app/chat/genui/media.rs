//! Tier 3's status, pictures, chips and files (after den's genui.rs, filetype.rs and 33-genui-prims.flecs): Progress,
//! Loading and Skeleton (the last two move on the clock), Icon (a file kind's glyph in its colour, or a Nerd Font
//! symbol), Image (a photo file, decoded by harness-native-render's photo.rs), Chip, Tile and FileRef.
use super::*;
use control::{arm, handler, send};
use harness_native_ui_input_api::Tooltip;
use harness_native_ui_layout_api::{Aspect, SizeKind};
use harness_native_ui_motion_api::Pose;
use harness_native_ui_text_api::{Picture, Vector};

/// The file kinds (den's filetype.rs) and each one's Nerd Font glyph; coloured by the scene's `hn.GenFileKind.<kind>`.
const KINDS: [(&str, char); 20] = [
    ("rust", '\u{E7A8}'),
    ("typescript", '\u{E628}'),
    ("tsx", '\u{E628}'),
    ("javascript", '\u{E60C}'),
    ("json", '\u{EB0F}'),
    ("toml", '\u{E615}'),
    ("yaml", '\u{E6A8}'),
    ("markdown", '\u{E609}'),
    ("nix", '\u{F313}'),
    ("shell", '\u{E691}'),
    ("python", '\u{E606}'),
    ("go", '\u{E627}'),
    ("html", '\u{E60E}'),
    ("css", '\u{E614}'),
    ("image", '\u{EAEA}'),
    ("lock", '\u{EA75}'),
    ("git", '\u{EA68}'),
    ("folder", '\u{EA83}'),
    ("folder_open", '\u{EAF7}'),
    ("text", '\u{EA7B}'),
];

pub(super) const KIND_NAMES: [&str; 20] = {
    let mut out = [""; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = KINDS[i].0;
        i += 1;
    }
    out
};

/// A spinner turns once a second; a skeleton breathes once every [`BREATH`] seconds, fading by up to [`BREATH_FADE`].
const BREATH: f32 = 1.6;
const BREATH_FADE: f32 = 0.5;

/// The file kind an Icon's `name` says (a kind, or den's other names for one: `rs`, `ts`, `md`, `dir`, ...).
fn kind_named(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    let k = match n.as_str() {
        "rs" => "rust",
        "ts" => "typescript",
        "js" | "jsx" | "mjs" => "javascript",
        "md" => "markdown",
        "yml" => "yaml",
        "sh" | "bash" | "zsh" => "shell",
        "py" => "python",
        "package" => "json",
        "picture" | "photo" | "png" | "jpg" | "svg" => "image",
        "dir" | "directory" => "folder",
        "folder-open" => "folder_open",
        "file" | "generic" | "doc" | "txt" => "text",
        other => other,
    };
    KIND_NAMES.iter().copied().find(|x| *x == k)
}

/// The file kind of the file at `path`, by its name (den's `Kind::of`): a folder for a trailing `/`, a few names of their
/// own, else its extension; plain text when none says.
fn kind_of(path: &str) -> &'static str {
    if path.ends_with('/') {
        return "folder";
    }
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    match base.as_str() {
        "flake.nix" | "default.nix" | "shell.nix" => return "nix",
        "cargo.lock" | "flake.lock" | "package-lock.json" | "yarn.lock" | "bun.lock" | "bun.lockb" | "pnpm-lock.yaml" | "uv.lock" | "poetry.lock" => {
            return "lock";
        }
        ".gitignore" | ".gitattributes" | ".gitmodules" => return "git",
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".zprofile" | ".zshenv" => return "shell",
        _ => {}
    }
    let ext = base.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "json" | "jsonc" | "json5" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "mdx" | "markdown" => "markdown",
        "nix" => "nix",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "py" | "pyi" => "python",
        "go" => "go",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "tiff" | "avif" | "heic" => "image",
        "lock" => "lock",
        _ => "text",
    }
}

fn kind_glyph(kind: &str) -> char {
    KINDS.iter().find(|(k, _)| *k == kind).map_or('\u{EA7B}', |(_, g)| *g)
}

/// What an Icon named `name` draws: a file kind's glyph (and the kind, for its colour), or the bundled Nerd Font's symbol
/// of that name (`cod-check`; a bare `check` is a codicon; `-` and `_` within a name are one).
pub(super) fn glyph(name: &str) -> Option<(Option<&'static str>, char)> {
    if let Some(k) = kind_named(name) {
        return Some((Some(k), kind_glyph(k)));
    }
    let us = name.replace('-', "_");
    let tail = name.split_once('-').map(|(p, r)| format!("{p}-{}", r.replace('-', "_")));
    let tries = [Some(name.to_owned()), tail, Some(format!("cod-{name}")), Some(format!("cod-{us}"))];
    tries.into_iter().flatten().find_map(|t| harness_native_render::text::nerd_symbol(&t)).map(|c| (None, c))
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
fn fill_of(world: WorldRef<'_>, name: &str) -> Option<Fill> {
    view::prefab(world, name).and_then(|p| p.try_cloned::<&Fill>())
}

fn filled(v: View<'_>, f: Option<Fill>) {
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
        "Progress" => {
            let max = control::number(node, "max").filter(|m| *m > 0.0).unwrap_or(1.0);
            let frac = (control::number(node, "value").unwrap_or(0.0) / max).clamp(0.0, 1.0) as f32;
            let Some(track) = control::labelled(v, &words(node, "label"), "hn.GenProgressTrack", None) else { return };
            // A Grow of 0 counts as 1: the empty side keeps a sliver of weight instead.
            for (part, w) in [("fill", frac), ("rest", 1.0 - frac)] {
                let Some(p) = track.part(part) else { continue };
                let mut s = p.entity().try_cloned::<&Size>().unwrap_or_default();
                let w = w.max(1e-4);
                if (s.w.value - w).abs() > 1e-5 {
                    s.w.value = w;
                    p.set(s);
                }
            }
        }
        "Loading" => {
            let label = match prop(node, "label") {
                Value::Null => "Loading".to_owned(),
                _ => words(node, "label"),
            };
            if let Some(l) = v.part("label") {
                l.text(&label);
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
            view::for_each_of(v, bones, |_, (), _| {});
            let fade = BREATH_FADE * 0.5 * (1.0 - (std::f32::consts::TAU * b.now / BREATH).cos());
            let pose = Pose { fade, ..Pose::default() };
            if v.entity().try_cloned::<&Pose>() != Some(pose) {
                v.set(pose);
            }
            if let Some(live) = b.live.as_deref_mut() {
                live.anim = true;
            }
        }
        "Icon" => {
            let Some((kind, g)) = glyph(&words(node, "name")) else { return };
            v.icon(&g.to_string());
            let f = match kind {
                Some(k) => fill_of(world, &format!("hn.GenFileKind.{k}")),
                None => fill_of(world, "hn.GenIcon"),
            };
            filled(v, f);
        }
        "Image" => image(v, node),
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
            let kind = kind_of(&path);
            if let Some(i) = v.part("icon") {
                i.icon(&kind_glyph(kind).to_string());
                filled(i, fill_of(world, &format!("hn.GenFileKind.{kind}")));
            }
            let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(&path);
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
            arm(v, Some(&format!("open:{path}")));
        }
        _ => {}
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
