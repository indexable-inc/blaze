//! An inline component read against the scene's `ui.inline` module (harness-native-ui-prims/scene/inline.flecs): the
//! entity a tag's name means, its icon, where a press on it opens, and for a component with live state (a pull request)
//! the state the host knows it in. The painter draws chips from it and the chat's press opens through it, so both read
//! one vocabulary.
//!
//! Live state is data too: the host keeps one entity per thing it knows under `ui.inline.live`, an `InlineLive` named by
//! the component's `key` format, with `(InlineState, <the component's state child>)` and `(InlineMark, <a
//! ui.inline.marks child>)`. Each state child carries the icon and its ink, so which colour an open, draft, merged or
//! closed pull request gets is the scene's, not code's. A link's live state is its page's favicon (`InlineLive.picture`),
//! drawn in its icon's place; without one it is its `unknown` child's globe. A path's is whether it is a folder (the
//! host's `fs` feed), which makes its icon a folder's.
//!
//! A file component's icon is its file kind's drawing (harness-native-ui-inline's files.rs, drawn by drawing.rs in its
//! own colours, [`ChipIcon::Drawing`]); every other icon is a glyph in an ink.
use std::collections::BTreeMap;

use harness_native_flecs::prelude::*;
use harness_native_ui_inline::{Form, Tag, files, format, open_target};
use harness_native_ui_look_api::{Fill, Shape};
use harness_native_ui_text_api::{InlineLive, InlineMark, InlineRef, InlineState};

/// The module's path, as lookups write it.
pub const MODULE: &str = "ui::inline";

/// Where the host keeps what it knows of live components: one `InlineLive` child per thing.
pub const LIVE: &str = "ui::inline::live";

/// Hand the module's words (each component's label format, whether a mark follows it, the web address it opens and the
/// prop a link keeps its address in) to the grammar, which the typesetter reads with no world. Run once the scene is
/// loaded; a module missing is a scene bug, named.
pub fn install(w: WorldRef<'_>) {
    let m = w.try_lookup(MODULE).unwrap_or_else(|| panic!("the scene has no {MODULE} (inline.flecs)"));
    let mut forms = BTreeMap::new(); // alloc: the vocabulary, kept for the process by the grammar.
    m.each_child(|c| {
        if let Some(r) = c.try_cloned::<&InlineRef>() {
            let form = Form {
                label: r.label.as_str().to_owned(),
                mark: r.mark,
                plain: r.plain,
                href: r.href.as_str().to_owned(),
                url: r.url.as_str().to_owned(),
            };
            forms.insert(c.name(), form);
        }
    });
    harness_native_ui_inline::install(forms);
}

/// What every chip shares (`ui.inline.look`): its icon's ink and its corners. Missing is a scene bug, named.
pub fn look(w: WorldRef<'_>, dark: bool) -> ([f32; 4], Shape) {
    let e = w.try_lookup(&format!("{MODULE}::look")).unwrap_or_else(|| panic!("the scene has no {MODULE}::look (inline.flecs)"));
    let fill = e.try_cloned::<&Fill>().unwrap_or_else(|| panic!("{MODULE}::look has no Fill"));
    (ink_of(fill, dark), e.try_cloned::<&Shape>().unwrap_or_default())
}

/// The plate of the chip the pointer is over (`ui.inline.hover`), or holds down (`ui.inline.pressed`).
pub fn hot(w: WorldRef<'_>, pressed: bool, dark: bool) -> [f32; 4] {
    fill_of(w, if pressed { "pressed" } else { "hover" }, dark)
}

/// The underline under a hovered chip's label (`ui.inline.underline`).
pub fn underline(w: WorldRef<'_>, dark: bool) -> [f32; 4] {
    fill_of(w, "underline", dark)
}

/// A chip's plate at rest (`ui.inline.rest`): clear or nearly, so a chip reads as a reference in the words, its icon
/// carrying what it is, not as a button; the hover plate is the one that shows.
pub fn rest(w: WorldRef<'_>, dark: bool) -> [f32; 4] {
    fill_of(w, "rest", dark)
}

/// The ink of a chip's quiet words after its label (`ui.inline.meta`: a link's host).
pub fn meta(w: WorldRef<'_>, dark: bool) -> [f32; 4] {
    fill_of(w, "meta", dark)
}

/// The picture a chip shows in its icon's place, when its live state has one (a link's page's favicon, a local PNG or
/// JPEG the daemon saved).
pub fn picture(w: WorldRef<'_>, tag: &Tag) -> Option<String> {
    live(w, tag)?.try_cloned::<&InlineLive>().map(|l| l.picture.as_str().to_owned()).filter(|p| !p.is_empty())
}

fn fill_of(w: WorldRef<'_>, name: &str, dark: bool) -> [f32; 4] {
    let fill = w
        .try_lookup(&format!("{MODULE}::{name}"))
        .and_then(|e| e.try_cloned::<&Fill>())
        .unwrap_or_else(|| panic!("the scene has no {MODULE}::{name} with a Fill (inline.flecs)"));
    ink_of(fill, dark)
}

fn ink_of(fill: Fill, dark: bool) -> [f32; 4] {
    crate::paint::rgba(if dark { fill.dark } else { fill.light })
}

/// The entity a tag names, with its `InlineRef`; None for a name the module has no entity for.
fn component<'w>(w: WorldRef<'w>, tag: &Tag) -> Option<(EntityView<'w>, InlineRef)> {
    let e = w.try_lookup(&format!("{MODULE}::{}", tag.name))?;
    let r = e.try_cloned::<&InlineRef>()?;
    Some((e, r))
}

/// Where a press on `tag` opens, as the host's `open:` action takes it, or why it opens nothing (an unknown name, a
/// missing or unevaluable prop, a target that is not a place). A component with an `href` format opens that.
pub fn target(w: WorldRef<'_>, tag: &Tag, cwd: &str, home: &str) -> Result<String, String> {
    let (_, r) = component(w, tag).ok_or_else(|| format!("<{}> is not an inline component ({MODULE} has no such entity)", tag.name))?;
    if !r.href.as_str().is_empty() {
        return open_target(&format(r.href.as_str(), tag)?, None, cwd, home);
    }
    let loc = tag.prop(r.target.as_str())?;
    let line = if r.line.as_str().is_empty() { None } else { tag.line(r.line.as_str())? };
    open_target(&loc, line, cwd, home)
}

/// The live-state feed `tag` is kept by and its key there (`github.pr`, `pr:indexable-inc/ix#7652`), for the host to
/// ask after; None for a component with no live state, or one whose key its props cannot fill.
pub fn keyed(w: WorldRef<'_>, tag: &Tag) -> Option<(String, String)> {
    let (_, r) = component(w, tag)?;
    if r.key.as_str().is_empty() {
        return None;
    }
    Some((r.feed.as_str().to_owned(), format(r.key.as_str(), tag).ok()?))
}

/// What the host knows of `tag`: its `ui.inline.live` entity, when there is one.
fn live<'w>(w: WorldRef<'w>, tag: &Tag) -> Option<EntityView<'w>> {
    let (_, key) = keyed(w, tag)?;
    let root = w.try_lookup(LIVE)?;
    let mut found = None;
    root.each_child(|c| {
        if found.is_none() && c.try_cloned::<&InlineLive>().is_some_and(|l| l.key.as_str() == key) {
            found = Some(c);
        }
    });
    found
}

/// What a chip shows in its icon's place.
#[derive(Clone, Debug, PartialEq)]
pub enum ChipIcon {
    /// A glyph of den's icon set (a Nerd Font codepoint) in an ink.
    Glyph(String, [f32; 4]),
    /// A file kind's drawing by name (drawing.rs), in its own colours.
    Drawing(&'static str),
}

/// The icon a chip for `tag` shows: for a live component, its state child's (`unknown`'s while nothing is known), a
/// glyph in the state's ink or, on a file component's state, the file kind's drawing (a folder's when the state says
/// `folder`); else the drawing of the target's file kind for a `file_icon` component, else its `glyph` in the look's ink;
/// `ui.inline.unknown`'s for a tag that cannot open (no such component, or a bad target).
pub fn icon(w: WorldRef<'_>, tag: &Tag, dark: bool) -> ChipIcon {
    let (quiet, _) = look(w, dark);
    let unknown = || {
        let r = w
            .try_lookup(&format!("{MODULE}::unknown"))
            .and_then(|e| e.try_cloned::<&InlineRef>())
            .unwrap_or_else(|| panic!("the scene has no {MODULE}::unknown with an InlineRef (inline.flecs)"));
        ChipIcon::Glyph(r.glyph.as_str().to_owned(), quiet)
    };
    let Some((e, r)) = component(w, tag) else { return unknown() };
    let opens = if r.href.as_str().is_empty() {
        tag.prop(r.target.as_str()).ok().filter(|loc| open_target(loc, None, "/", "/").is_ok())
    } else {
        format(r.href.as_str(), tag).ok()
    };
    if !r.key.as_str().is_empty() {
        // Its state child, else its own `unknown` child: never a guessed state.
        let state = live(w, tag).and_then(|l| l.target(InlineState::id(), 0)).or_else(|| e.lookup_child("unknown"));
        let Some(s) = state else { panic!("{} has live state but no `unknown` child (inline.flecs)", e.path()) };
        let sr = s.try_cloned::<&InlineRef>().unwrap_or_default();
        return match opens {
            Some(loc) if sr.file_icon => ChipIcon::Drawing(files::drawing(files::kind_at(&loc, sr.folder))),
            None if sr.file_icon => unknown(),
            _ => ChipIcon::Glyph(sr.glyph.as_str().to_owned(), s.try_cloned::<&Fill>().map_or(quiet, |f| ink_of(f, dark))),
        };
    }
    match opens {
        Some(loc) if r.file_icon => ChipIcon::Drawing(files::drawing(files::kind_of(&loc))),
        Some(_) => ChipIcon::Glyph(r.glyph.as_str().to_owned(), quiet),
        None => unknown(),
    }
}

/// The live-state key, the place as written and the component (whose `folder` and `file` children are its states) of a
/// file component's tag (a `<Path>` or `<FileRef>`, kept by the `fs` feed), for the host to find out whether it is a
/// folder; None for any other tag.
pub fn place<'w>(w: WorldRef<'w>, tag: &Tag) -> Option<(String, String, EntityView<'w>)> {
    let (feed, key) = keyed(w, tag)?;
    let (e, r) = component(w, tag)?;
    (feed == "fs").then_some(())?;
    Some((key, tag.prop(r.target.as_str()).ok()?, e))
}

/// The mark after `tag`'s label and its ink (a pull request's CI check), when its live state has one.
pub fn mark(w: WorldRef<'_>, tag: &Tag, dark: bool) -> Option<(String, [f32; 4])> {
    let m = live(w, tag)?.target(InlineMark::id(), 0)?;
    let glyph = m.try_cloned::<&InlineRef>()?.glyph.as_str().to_owned();
    let (quiet, _) = look(w, dark);
    Some((glyph, m.try_cloned::<&Fill>().map_or(quiet, |f| ink_of(f, dark))))
}

/// What hovering `tag`'s chip says: its live state's tip (a pull request's title, or the error that left it unknown).
pub fn tip(w: WorldRef<'_>, tag: &Tag) -> Option<String> {
    live(w, tag)?.try_cloned::<&InlineLive>().map(|l| l.tip.as_str().to_owned()).filter(|t| !t.is_empty())
}
