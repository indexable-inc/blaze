//! The app's link: every [`AppUp`] from the Mach service, on the main queue (CEF's UI thread). State changes go through
//! host.rs; CEF calls are made here, after the host has let go.
use crate::host::{self, OpenSpec, WaiterSpec};
use crate::keys;
use cef::*;
use harness_mach::{Error, Msg};
use harness_proto::browser::{AppDown, AppUp, Ime, Nav, Pointer, PointerKind};
use harness_proto::{SCHEMA, cbor};
use std::collections::BTreeSet;

/// The size of an extension's action popup view, in points (Chrome's popups are at most 800 by 600 and size to their
/// content; an off-screen view has one size, so it opens at this and the app floats it).
pub const ACTION_POPUP_SIZE: [u32; 2] = [400, 600];

/// The Mach source's handler.
pub fn on_msg(m: Result<Msg, Error>) {
    let m = match m {
        Ok(m) => m,
        Err(e) => {
            eprintln!("harness-browser: the Mach service's receive port failed: {e}");
            return;
        }
    };
    let up = match cbor::decode::<AppUp>(&m.body) {
        Ok(u) => u,
        Err(e) => return host::tell_app(AppDown::Error { req: None, error: format!("the app sent a Mach message that is no AppUp: {e}") }),
    };
    if let AppUp::Hello { schema } = up {
        return hello(schema, m);
    }
    if !m.ports.is_empty() {
        return host::tell_app(AppDown::Error { req: None, error: format!("the app sent {} port(s) with {}; only hello carries a port", m.ports.len(), kind(&up)) });
    }
    let req = request_of(&up);
    if let Err(error) = handle(up) {
        host::tell_app(AppDown::Error { req, error });
    }
}

fn hello(schema: u32, m: Msg) {
    let mut ports = m.ports.into_iter();
    let (Some(port), None) = (ports.next(), ports.next()) else {
        eprintln!("harness-browser: the app's hello must carry exactly one port (its receive port); it was dropped");
        return;
    };
    if schema != SCHEMA {
        let error = format!("the app speaks harness schema {schema}; this browser host speaks schema {SCHEMA}: update the older side");
        eprintln!("harness-browser: {error}");
        let _ = port.send(&cbor::encode(&AppDown::Error { req: None, error }), &[]);
        return;
    }
    for bh in host::app_hello(port) {
        bh.set_windowless_frame_rate(host::FPS_IDLE);
    }
}

fn kind(up: &AppUp) -> &'static str {
    match up {
        AppUp::Hello { .. } => "hello",
        AppUp::Watch { .. } => "watch",
        AppUp::Resize { .. } => "resize",
        AppUp::Pointer { .. } => "pointer",
        AppUp::Key { .. } => "key",
        AppUp::Ime { .. } => "ime",
        AppUp::Focus { .. } => "focus",
        AppUp::Latched { .. } => "latched",
        AppUp::Nav { .. } => "nav",
        AppUp::Open { .. } => "open",
        AppUp::Close { .. } => "close",
        AppUp::Manage { .. } => "manage",
        AppUp::Extensions { .. } => "extensions",
        AppUp::Action { .. } => "action",
    }
}

fn request_of(up: &AppUp) -> Option<u64> {
    match up {
        AppUp::Open { req, .. } | AppUp::Extensions { req } | AppUp::Action { req, .. } => Some(*req),
        _ => None,
    }
}

fn handle(up: AppUp) -> Result<(), String> {
    match up {
        AppUp::Hello { .. } => unreachable!("on_msg handles hello"),
        AppUp::Watch { view, on } => {
            let bh = host::watch(view, on)?;
            bh.set_windowless_frame_rate(if on { host::FPS_WATCHED } else { host::FPS_IDLE });
            if on {
                // A new ring goes out with the next paint; ask for one now.
                bh.invalidate(PaintElementType::VIEW);
            }
        }
        AppUp::Resize { view, w, h, scale } => {
            let (bh, scaled) = host::resize(view, w, h, scale)?;
            if scaled {
                bh.notify_screen_info_changed();
            }
            bh.was_resized();
        }
        AppUp::Pointer { view, ev } => pointer(&host::host_of(view)?, view, &ev)?,
        AppUp::Key { view, ev } => {
            let bh = host::host_of(view)?;
            for f in keys::events(&ev).map_err(|e| format!("view {view}: {e}"))? {
                let type_ = match f.kind {
                    keys::Kind::RawKeyDown => KeyEventType::RAWKEYDOWN,
                    keys::Kind::KeyUp => KeyEventType::KEYUP,
                    keys::Kind::Char => KeyEventType::CHAR,
                };
                let e = KeyEvent {
                    type_,
                    modifiers: f.modifiers,
                    windows_key_code: f.windows_key_code,
                    native_key_code: f.native_key_code,
                    is_system_key: 0,
                    character: f.character,
                    unmodified_character: f.unmodified_character,
                    focus_on_editable_field: 0,
                    ..Default::default()
                };
                bh.send_key_event(Some(&e));
            }
        }
        AppUp::Ime { view, ev } => ime(&host::host_of(view)?, ev),
        AppUp::Focus { view, on } => host::host_of(view)?.set_focus(i32::from(on)),
        AppUp::Latched { view, generation, slot } => host::latched(view, generation, slot)?,
        AppUp::Nav { view, go } => {
            let b = host::browser_of(view)?;
            match go {
                Nav::Back => b.go_back(),
                Nav::Forward => b.go_forward(),
                Nav::Reload => b.reload(),
                Nav::Stop => b.stop_load(),
                Nav::Go { url } => {
                    let f = b.main_frame().ok_or_else(|| format!("view {view} has no main frame to load {url} in"))?;
                    f.load_url(Some(&CefString::from(url.as_str())));
                }
            }
        }
        AppUp::Open { req, url } => {
            host::open(OpenSpec { url, owner: Some("app".into()), size: host::DEFAULT_SIZE, opener: None, keep: true, attached: BTreeSet::new(), waiter: Some(WaiterSpec::App { req }) })?;
        }
        AppUp::Close { view } => {
            if !host::exists(view) {
                return Err(format!("the app closed view {view}, which does not exist"));
            }
            host::close(view, "the app closed it".into());
        }
        AppUp::Manage { url } => crate::manage::open(url.as_deref())?,
        AppUp::Extensions { req } => {
            let extensions = crate::extensions::list(&host::config().dir)?;
            host::tell_app(AppDown::Extensions { req, extensions });
        }
        AppUp::Action { req, view, extension } => {
            if !host::exists(view) {
                return Err(format!("extension {extension}'s action popup is for view {view}, which does not exist"));
            }
            let all = crate::extensions::list(&host::config().dir)?;
            let ext = all.iter().find(|e| e.id == extension).ok_or_else(|| format!("there is no extension {extension} installed (for view {view}'s action popup)"))?;
            if !ext.enabled {
                return Err(format!("extension {} ({extension}) is disabled: its action popup does not open", ext.name));
            }
            let popup = ext.popup.as_deref().ok_or_else(|| format!("extension {} ({extension}) has no action popup (manifest action.default_popup)", ext.name))?;
            let url = format!("chrome-extension://{extension}/{}", popup.trim_start_matches('/'));
            host::open(OpenSpec {
                url,
                owner: Some(format!("popup:{view}")),
                size: ACTION_POPUP_SIZE,
                opener: None,
                keep: true,
                attached: BTreeSet::new(),
                waiter: Some(WaiterSpec::App { req }),
            })?;
        }
    }
    Ok(())
}

fn pointer(bh: &BrowserHost, view: u64, ev: &Pointer) -> Result<(), String> {
    let me = MouseEvent { x: ev.x.round() as i32, y: ev.y.round() as i32, modifiers: ev.mods };
    let button = || match ev.button {
        0 => Ok(MouseButtonType::LEFT),
        1 => Ok(MouseButtonType::MIDDLE),
        2 => Ok(MouseButtonType::RIGHT),
        b => Err(format!("the app pressed pointer button {b} over view {view}; buttons are 0 (left), 1 (middle), 2 (right)")),
    };
    match ev.kind {
        PointerKind::Move => bh.send_mouse_move_event(Some(&me), 0),
        PointerKind::Leave => bh.send_mouse_move_event(Some(&me), 1),
        PointerKind::Down => bh.send_mouse_click_event(Some(&me), button()?, 0, i32::from(ev.clicks.max(1))),
        PointerKind::Up => bh.send_mouse_click_event(Some(&me), button()?, 1, i32::from(ev.clicks.max(1))),
        // CEF's wheel deltas are positive up and left; the app's are positive as the page scrolls (down, right).
        PointerKind::Wheel => bh.send_mouse_wheel_event(Some(&me), -(ev.dx.round() as i32), -(ev.dy.round() as i32)),
    }
    Ok(())
}

/// No replacement range, as CEF spells it.
const NO_RANGE: Range = Range { from: u32::MAX, to: u32::MAX };

fn range(r: Option<[u32; 2]>) -> Range {
    r.map_or(NO_RANGE, |[from, to]| Range { from, to })
}

fn ime(bh: &BrowserHost, ev: Ime) {
    match ev {
        Ime::Set { text, sel, replace } => {
            let len = text.encode_utf16().count() as u32;
            let underline = CompositionUnderline {
                range: Range { from: 0, to: len },
                color: 0xFF00_0000,
                background_color: 0,
                thick: 0,
                style: CompositionUnderlineStyle::SOLID,
                ..Default::default()
            };
            bh.ime_set_composition(Some(&CefString::from(text.as_str())), Some(&[underline]), Some(&range(replace)), Some(&Range { from: sel[0], to: sel[1] }));
        }
        Ime::Commit { text, replace } => bh.ime_commit_text(Some(&CefString::from(text.as_str())), Some(&range(replace)), 0),
        Ime::Finish => bh.ime_finish_composing_text(0),
        Ime::Cancel => bh.ime_cancel_composition(),
    }
}
