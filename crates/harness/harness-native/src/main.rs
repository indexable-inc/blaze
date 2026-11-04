//! harness-native: the harness as a native macOS app (AppKit, Metal, flecs), on den's native stack. See README.md.
mod app;
mod atlas;
mod boxdraw;
mod draw;
mod gallery;
mod gpu;
mod icons;
mod link;
mod look;
mod pack;
mod pane;
mod prefs;
mod shot;
mod single;
mod slug;
mod term;
mod text;
mod window;

fn main() {
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
