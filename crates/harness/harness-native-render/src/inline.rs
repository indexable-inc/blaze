//! An inline component read against the scene's `ui.inline` module (harness-native-ui-prims/scene/inline.flecs): the
//! entity a tag's name means, its icon, and where a press on it opens. The painter draws chips from it and the chat's
//! press opens through it, so both read one vocabulary.
use harness_native_flecs::prelude::*;
use harness_native_ui_inline::{Tag, files, open_target};
use harness_native_ui_look_api::{Fill, Shape};
use harness_native_ui_text_api::InlineRef;

/// The module's path, as lookups write it.
pub const MODULE: &str = "ui::inline";

/// What every chip shares (`ui.inline.look`): its icon's ink and its corners. Missing is a scene bug, named.
pub fn look(w: WorldRef<'_>, dark: bool) -> ([f32; 4], Shape) {
    let e = w.try_lookup(&format!("{MODULE}::look")).unwrap_or_else(|| panic!("the scene has no {MODULE}::look (inline.flecs)"));
    let fill = e.try_cloned::<&Fill>().unwrap_or_else(|| panic!("{MODULE}::look has no Fill"));
    let c = if dark { fill.dark } else { fill.light };
    (crate::paint::rgba(c), e.try_cloned::<&Shape>().unwrap_or_default())
}

/// The entity a tag names, with its `InlineRef`; None for a name the module has no entity for.
fn component<'w>(w: WorldRef<'w>, tag: &Tag) -> Option<(EntityView<'w>, InlineRef)> {
    let e = w.try_lookup(&format!("{MODULE}::{}", tag.name))?;
    let r = e.try_cloned::<&InlineRef>()?;
    Some((e, r))
}

/// Where a press on `tag` opens, as the host's `open:` action takes it, or why it opens nothing (an unknown name, a
/// missing or unevaluable prop, a target that is not a place).
pub fn target(w: WorldRef<'_>, tag: &Tag, cwd: &str, home: &str) -> Result<String, String> {
    let (_, r) = component(w, tag).ok_or_else(|| format!("<{}> is not an inline component ({MODULE} has no such entity)", tag.name))?;
    let loc = tag.prop(r.target.as_str())?;
    let line = if r.line.as_str().is_empty() { None } else { tag.line(r.line.as_str())? };
    open_target(&loc, line, cwd, home)
}

/// The icon a chip for `tag` shows: its file kind's for a `file_icon` component, else its `glyph`; `unknown`'s
/// for a tag that cannot open (no such component, or a bad target).
pub fn glyph(w: WorldRef<'_>, tag: &Tag) -> String {
    let own = component(w, tag).and_then(|(_, r)| {
        let loc = tag.prop(r.target.as_str()).ok()?;
        open_target(&loc, None, "/", "/").ok()?;
        if r.file_icon {
            Some(files::kind_glyph(files::kind_of(&loc)).to_string())
        } else {
            Some(r.glyph.as_str().to_owned())
        }
    });
    own.unwrap_or_else(|| {
        w.try_lookup(&format!("{MODULE}::unknown"))
            .and_then(|e| e.try_cloned::<&InlineRef>())
            .map(|r| r.glyph.as_str().to_owned())
            .unwrap_or_else(|| panic!("the scene has no {MODULE}::unknown with an InlineRef (inline.flecs)"))
    })
}
