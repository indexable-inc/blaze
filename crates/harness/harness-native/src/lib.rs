//! harness-native: the harness as a native macOS app (AppKit, Metal, flecs), on den's native stack. See README.md.
//! A library so that other hosts stand on the same scene: [`scene::world`] is the app's world before anything runs in it
//! (its tokens, the prims and the `hn.*` widgets), which harness-video loads a video's scene into. The binary
//! (src/main.rs) is [`main`].
mod app;
mod audio;
mod music;
use harness_native_render::{boxdraw, draw, gpu, pack, text};
mod gallery;
mod haptic;
mod keys;
mod link;
mod look;
mod open;
mod pane;
mod prefs;
mod resize;
mod shot;
mod single;
mod term;
#[cfg(target_os = "ios")]
mod ios;
#[cfg(target_os = "macos")]
mod window;
mod world_api;

#[cfg(target_os = "ios")]
pub fn main() {
    ios::run()
}

/// The app: a shot when `HARNESS_NATIVE_SHOT` asks for one, else the window.
#[cfg(target_os = "macos")]
pub fn main() {
    if let Some(path) = std::env::var_os("HARNESS_NATIVE_SHOT") {
        if let Err(e) = link::ensure_daemon() {
            eprintln!("harness-native: {e}");
        }
        if let Err(e) = shot::run(&path.to_string_lossy()) {
            eprintln!("harness-native: {e}");
            std::process::exit(1);
        }
        return;
    }
    // One app: a second launch focuses the first (on the session it was asked for) and goes.
    let select = single::select_env();
    let ask = select.map_or(single::Ask::Focus, single::Ask::Select);
    let listener = match single::claim(ask) {
        Ok(Some(l)) => Some(l),
        Ok(None) => return,
        Err(e) => {
            eprintln!("harness-native: single instance: {e}");
            None
        }
    };
    if let Err(e) = link::ensure_daemon() {
        eprintln!("harness-native: {e}");
    }
    window::run(listener, select);
}

/// The scene every host of this UI builds on (the app's window, its shots and tests, harness-video's frames).
pub mod scene {
    pub use crate::app::{load, load_file, scene_world as world};
}
