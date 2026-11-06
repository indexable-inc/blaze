//! The embedded browser as UI data (crates/harness/harness-browser's README, "In the app"). A `ui.Browser` node carries
//! [`BrowserView`]: the host's view it shows, or (`view: 0`) a page to open (`src`), which the app asks the host for once
//! and then shows. The app (harness-native/src/app/browser.rs) draws the node as the view's live surface, keyed by the
//! view, so a chat card and the pane showing one view share one ring of frames. [`BrowserFocus`] marks the node that has
//! the keyboard and the pointer's presses: the pane, or a card clicked into. At most one holds it.
use harness_native_flecs::prelude::*;

/// Which harness browser view a node shows: `view` (the host's id, 0 for none yet) or the page `src` to open.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct BrowserView {
    pub view: u64,
    pub src: ScriptString,
}

/// The node whose view hears the keys, the input method and the pointer.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct BrowserFocus;

hyperion_api_layouts::api_layouts! {
    "BrowserView" => alias BrowserView,
    "BrowserFocus" => alias BrowserFocus,
}

/// Register the browser's components before any scene or system uses them.
pub fn register(world: &World) {
    world.component::<BrowserView>().writable();
    world.component::<BrowserFocus>().writable();
}
