//! The harness browser's messages: the embedded Chromium (CEF) that `harness-browser` runs for one daemon, and the two
//! kinds of client that talk to it. The design is in crates/harness/harness-browser/README.md.
//!
//! - The **kernel** (and any program of the person's) talks to the host over a Unix socket, `$HARNESS_HOME/browser/`
//!   [`SOCK`] (the directory 0700, the socket 0600: no TCP port, nothing another user can reach). Frames are harness's
//!   own (`[type u8][len u32 BE][payload]`, [`crate::frame::T_CBOR`]), each payload one deterministic-CBOR [`KernelUp`]
//!   or [`KernelDown`]. The connection opens with [`KernelUp::Hello`]; the host answers [`KernelDown::Hello`] or an
//!   [`KernelDown::Error`] naming both schemas and closes. DevTools traffic rides inside as [`KernelUp::Cdp`] and
//!   [`KernelDown::Cdp`]: the page target's own CDP messages, JSON text exactly as Chromium reads and writes them.
//! - The **app** (harness-native) talks to the host over Mach, so frames can carry IOSurfaces: the host checks in the
//!   bootstrap service [`service_name`] and the app looks it up (crates/harness/harness-mach). Each Mach message is one
//!   deterministic-CBOR [`AppUp`] or [`AppDown`] plus the port rights it names (an [`AppUp::Hello`] carries the app's
//!   receive port; an [`AppDown::Ring`] carries one IOSurface port per slot, in slot order). Nothing else carries ports.
//!
//! Every view is an off-screen (windowless, Alloy-style) CEF browser; [`ViewInfo`] is what both kinds of client see of
//! one. Coordinates are view points from its top left; a ring's pixels are its points times its scale.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The socket's file name, in [`dir`].
pub const SOCK: &str = "host.sock";

/// The host's state for a daemon at `home` (`$HARNESS_HOME`): `profile/` (Chromium's profile: cookies, logins,
/// extensions installed from the Web Store), `extensions/` (unpacked extensions loaded at start), [`SOCK`], `cef.log`.
#[must_use]
pub fn dir(home: &Path) -> PathBuf {
    home.join("browser")
}

/// The Mach bootstrap service the host of the daemon at `home` checks in: `inc.indexable.harness.browser.` and the
/// FNV-1a 64 of `home`'s bytes in hex, so two daemons (two `HARNESS_HOME`s) never share a host. `home` as the daemon
/// resolved it (paths::home), not canonicalized: the daemon hands the name to every client, nobody else derives it.
#[must_use]
pub fn service_name(home: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in home.as_os_str().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("inc.indexable.harness.browser.{h:016x}")
}

/// One view (a page) as the host has it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ViewInfo {
    /// The host's id for it, never reused while the host runs.
    pub view: u64,
    /// DevTools' id for its page target (`Target.getTargetInfo`'s `targetId`).
    pub target: String,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    /// Who opened it: a kernel cell (`<repl>/<task>`), `app` for a tab the person opened, `popup:<view>` for a page a
    /// page opened (window.open, target=_blank).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Its size in points, and the device scale it renders at.
    pub size: [u32; 2],
    pub scale: f32,
}

/// An extension installed in the profile (from the Web Store through the manage window, or unpacked under
/// `extensions/`), as the host read it from the profile's preferences and the extension's manifest.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Extension {
    pub id: String,
    pub name: String,
    pub version: String,
    pub enabled: bool,
    /// Its action's popup page (manifest `action.default_popup`), relative to the extension's root; None when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popup: Option<String>,
    /// `store` or `unpacked`.
    pub source: String,
    /// Where it lives on disk.
    pub path: String,
}

// ---------------------------------------------------------------- kernel <-> host (the Unix socket)

/// A socket client's message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum KernelUp {
    /// The first message: the schema the client speaks ([`crate::SCHEMA`]).
    Hello { schema: u32 },
    /// Open a view at `url` (`about:blank` for an empty one), `size` points (1280 by 800 when None). Answered by
    /// [`KernelDown::Opened`] once its page target exists, or [`KernelDown::Error`] with this `req`. The view stays this
    /// connection's: it is closed when the connection drops, unless `keep`.
    Open {
        req: u64,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<[u32; 2]>,
        #[serde(default)]
        keep: bool,
    },
    /// Attach to a view another client opened (a tab the person opened in the app): its CDP messages come to this
    /// connection too. Answered by [`KernelDown::Opened`] with this `req`.
    Attach { req: u64, view: u64 },
    /// One CDP message for the view's page target, JSON text (`{"id":…,"method":…,"params":…}`). Its answer and every
    /// event the page sends come back as [`KernelDown::Cdp`]. Ids are this connection's own: the host renumbers them.
    Cdp { view: u64, message: String },
    /// Close a view.
    Close { view: u64 },
    /// The views there are. Answered by [`KernelDown::Views`].
    Views { req: u64 },
    /// The extensions installed. Answered by [`KernelDown::Extensions`].
    Extensions { req: u64 },
    /// Open the manage window: a Chrome-style CEF window (native, the host's own) at `url`, `chrome://extensions` when
    /// None. It is where the Web Store installs and where an extension's own UI works in full. Answered by
    /// [`KernelDown::Done`].
    Manage {
        req: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Where the Mac is now, from macOS's Core Location (the host is the app macOS asks the person about; the first
    /// request makes it ask). Answered by [`KernelDown::Location`], or [`KernelDown::Error`] saying why not (Location
    /// Services off, the host refused, no fix in time): nothing else is tried.
    Location { req: u64 },
    /// The places Apple Maps finds for `query` (what a person types into Maps: "coffee", "pharmacy open now", a name)
    /// within `radius_m` metres of a point, at most `limit`, as Maps ranks them. Answered by [`KernelDown::Places`]
    /// (empty when nothing matches), or [`KernelDown::Error`] when the search itself failed.
    Places { req: u64, query: String, latitude: f64, longitude: f64, radius_m: f64, limit: u32 },
}

/// One place Apple Maps found: its identity in Apple Maps (`id`, `MKMapItem.identifier`: what a Map's marker names as
/// `place` to show Apple's own card of it; empty when Maps gives none), its name, where it is (degrees), its address in one line, and what Maps knows of its
/// phone, its own page and its kind (`Cafe`, `Restaurant`, ...), each empty when Maps has none.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Place {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub address: String,
    pub phone: String,
    pub url: String,
    pub category: String,
}

/// The host's message to a socket client.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum KernelDown {
    /// The answer to [`KernelUp::Hello`].
    Hello { schema: u32, pid: u32 },
    /// A view opened or attached.
    Opened { req: u64, view: ViewInfo },
    /// A CDP message from the view's page target: an answer (with the client's own id) or an event.
    Cdp { view: u64, message: String },
    /// The view is gone (closed by someone, its renderer crashed, the host is stopping): why, in words.
    Closed { view: u64, why: String },
    Views { req: u64, views: Vec<ViewInfo> },
    Extensions { req: u64, extensions: Vec<Extension> },
    /// Where the Mac is ([`KernelUp::Location`]): degrees, the radius Core Location is sure of in metres, the altitude
    /// in metres when it has one, and when the fix was taken (Unix ms).
    Location {
        req: u64,
        latitude: f64,
        longitude: f64,
        accuracy_m: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        altitude_m: Option<f64>,
        at_ms: u64,
    },
    /// What Apple Maps found ([`KernelUp::Places`]).
    Places { req: u64, places: Vec<Place> },
    /// A request with nothing else to say is done.
    Done { req: u64 },
    /// A request refused, naming the thing and why; `req` None for the hello or a message that names no request.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        req: Option<u64>,
        error: String,
    },
}

// ---------------------------------------------------------------- app <-> host (Mach)

/// CEF's `cef_event_flags_t` bits, as [`Pointer::mods`] and [`Key::mods`] carry them (the app sets them from the NSEvent).
pub mod mods {
    pub const CAPS_LOCK: u32 = 1 << 0;
    pub const SHIFT: u32 = 1 << 1;
    pub const CONTROL: u32 = 1 << 2;
    pub const ALT: u32 = 1 << 3;
    pub const LEFT_BUTTON: u32 = 1 << 4;
    pub const MIDDLE_BUTTON: u32 = 1 << 5;
    pub const RIGHT_BUTTON: u32 = 1 << 6;
    pub const COMMAND: u32 = 1 << 7;
    pub const NUM_LOCK: u32 = 1 << 8;
    pub const KEY_PAD: u32 = 1 << 9;
    pub const IS_REPEAT: u32 = 1 << 13;
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointerKind {
    Move,
    Down,
    Up,
    /// The pointer left the view.
    Leave,
    Wheel,
}

/// A pointer event over a view.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Pointer {
    pub kind: PointerKind,
    /// View points from its top left.
    pub x: f32,
    pub y: f32,
    /// 0 left, 1 middle, 2 right (Down and Up).
    #[serde(default)]
    pub button: u8,
    /// The press's click count (Down and Up).
    #[serde(default)]
    pub clicks: u8,
    /// Wheel: how far, in points, positive right and down as the page scrolls (the app has applied natural scrolling).
    #[serde(default)]
    pub dx: f32,
    #[serde(default)]
    pub dy: f32,
    /// [`mods`] bits.
    #[serde(default)]
    pub mods: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    /// A key went down (CEF's RAWKEYDOWN).
    Down,
    Up,
    /// The text a key typed, after the app's input method passed on it (CEF's CHAR), one per UTF-16 unit of `chars`.
    Char,
}

/// A key event for the view that has focus.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Key {
    pub kind: KeyKind,
    /// The Mac virtual key code (NSEvent keyCode); the host maps it to Windows' key code, as CEF wants.
    pub code: u16,
    /// NSEvent `characters` and `charactersIgnoringModifiers`.
    #[serde(default)]
    pub chars: String,
    #[serde(default)]
    pub unmod: String,
    /// [`mods`] bits.
    #[serde(default)]
    pub mods: u32,
}

/// An input method's work on the focused view (the app is the NSTextInputClient; the page gets IME composition).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Ime {
    /// Marked text: `text` with the selection `sel` (UTF-16 offsets into it), replacing `replace` of the field (UTF-16)
    /// when given.
    Set {
        text: String,
        sel: [u32; 2],
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace: Option<[u32; 2]>,
    },
    /// Text committed (insertText:), replacing `replace` when given.
    Commit {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace: Option<[u32; 2]>,
    },
    /// The marked text kept as it stands (unmarkText).
    Finish,
    Cancel,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Nav {
    Back,
    Forward,
    Reload,
    Stop,
    /// Load `url` (the app has turned typed words into a URL or a search).
    Go { url: String },
}

/// The app's message to the host.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AppUp {
    /// The first message, carrying one port right: the app's receive port (a send right made from it), where every
    /// [`AppDown`] goes. The host answers [`AppDown::Hello`] then [`AppDown::Views`]. A second hello replaces the first
    /// (the app restarted its link); the host drops a port whose receiver died.
    Hello { schema: u32 },
    /// Send (or stop sending) the view's frames: an [`AppDown::Ring`] at once and whenever its size changes, then an
    /// [`AppDown::Frame`] per new picture.
    Watch { view: u64, on: bool },
    /// The view is drawn `w` by `h` points at device `scale` now (the pane's or the card's box).
    Resize { view: u64, w: u32, h: u32, scale: f32 },
    Pointer { view: u64, ev: Pointer },
    Key { view: u64, ev: Key },
    Ime { view: u64, ev: Ime },
    /// The view has (or lost) the keyboard.
    Focus { view: u64, on: bool },
    /// The app drew ring slot `slot` of generation `generation` and holds it until it latches another: the host never
    /// writes into it meanwhile.
    Latched {
        view: u64,
        #[serde(rename = "gen")]
        generation: u32,
        slot: u8,
    },
    Nav { view: u64, go: Nav },
    /// Open a tab (the sidebar's new tab, a link the person opened). Answered by [`AppDown::Opened`].
    Open { req: u64, url: String },
    Close { view: u64 },
    /// Open the manage window ([`KernelUp::Manage`]).
    Manage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// The extensions installed. Answered by [`AppDown::Extensions`].
    Extensions { req: u64 },
    /// Show extension `extension`'s action popup for view `view`: the host opens its popup page as a view of its own
    /// (owner `popup:<view>`) and answers [`AppDown::Opened`]; the app floats it under the pane's toolbar.
    Action { req: u64, view: u64, extension: String },
}

/// The host's message to the app.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AppDown {
    Hello { schema: u32, pid: u32 },
    /// Every view, whole, after the hello and whenever one comes, goes or changes (url, title, loading, size).
    Views { views: Vec<ViewInfo> },
    /// The view's frames now go to `slots` IOSurfaces (this message's ports, in slot order), `w` by `h` pixels, BGRA8
    /// (sRGB), generation `generation`: the app drops the previous generation's surfaces.
    Ring {
        view: u64,
        #[serde(rename = "gen")]
        generation: u32,
        w: u32,
        h: u32,
        slots: u8,
    },
    /// Slot `slot` of generation `generation` holds the view's newest picture, whole (the GPU finished writing it), the
    /// `seq`th the view painted.
    Frame {
        view: u64,
        #[serde(rename = "gen")]
        generation: u32,
        slot: u8,
        seq: u64,
    },
    /// The pointer's shape over the view: a name of harness-native-ui-input-api's `Pointer` (`Arrow`, `Text`, `Hand`,
    /// `ResizeLeftRight`, `ResizeUpDown`, `NotAllowed`, `OpenHand`, `Grabbing`).
    Cursor { view: u64, shape: String },
    /// Where the focused field's composition sits (view points), for the input method's candidate window.
    ImeRect { view: u64, x: f32, y: f32, w: f32, h: f32 },
    Opened { req: u64, view: u64 },
    Closed { view: u64, why: String },
    Extensions { req: u64, extensions: Vec<Extension> },
    /// Something the app asked for was refused, naming it.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        req: Option<u64>,
        error: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cbor::{decode, encode};

    #[test]
    fn messages_round_trip() {
        let up = AppUp::Pointer { view: 3, ev: Pointer { kind: PointerKind::Down, x: 1.5, y: 2.0, button: 0, clicks: 2, dx: 0.0, dy: 0.0, mods: mods::SHIFT } };
        assert_eq!(decode::<AppUp>(&encode(&up)).unwrap(), up);
        let down = KernelDown::Opened { req: 1, view: ViewInfo { view: 7, target: "T".into(), size: [1280, 800], scale: 2.0, ..Default::default() } };
        assert_eq!(decode::<KernelDown>(&encode(&down)).unwrap(), down);
        let ring = AppDown::Ring { view: 7, generation: 2, w: 2560, h: 1600, slots: 3 };
        assert_eq!(decode::<AppDown>(&encode(&ring)).unwrap(), ring);
    }

    #[test]
    fn service_names_differ_by_home() {
        let a = service_name(Path::new("/Users/a/.harness"));
        let b = service_name(Path::new("/tmp/h"));
        assert_ne!(a, b);
        assert!(a.starts_with("inc.indexable.harness.browser.") && a.len() == "inc.indexable.harness.browser.".len() + 16);
    }
}
