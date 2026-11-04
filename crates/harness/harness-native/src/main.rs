//! harness-native: the harness as a native macOS app (AppKit, Metal, flecs), on den's native stack. See README.md.
mod app;
mod atlas;
mod draw;
mod gpu;
mod link;
mod pack;
mod shot;
mod term;
mod text;
mod window;

fn main() {
    if let Err(e) = link::ensure_daemon() {
        eprintln!("harness-native: {e}");
    }
    if let Some(path) = std::env::var_os("HARNESS_NATIVE_SHOT") {
        if let Err(e) = shot::run(&path.to_string_lossy()) {
            eprintln!("harness-native: {e}");
            std::process::exit(1);
        }
        return;
    }
    window::run();
}
