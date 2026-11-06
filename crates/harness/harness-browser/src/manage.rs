//! The manage window: one Chrome-style CEF Views window of the host's own (`CEF_RUNTIME_STYLE_CHROME`, with Chrome's
//! toolbar), in the same profile as the views, where the Web Store installs and where an extension's own UI works in
//! full. One at a time: a second `Manage` loads its url in the open window and brings it to the front.
//!
//! Its state lives here, not in the host table, so no CEF call ever runs inside the host's borrow.
use crate::handlers;
use cef::*;
use std::cell::RefCell;

/// Where the window opens when the request names no url.
pub const DEFAULT_URL: &str = "chrome://extensions";

struct Open {
    window: Window,
    view: BrowserView,
}

thread_local! {
    static OPEN: RefCell<Option<Open>> = const { RefCell::new(None) };
}

fn current() -> Option<(Window, BrowserView)> {
    OPEN.with(|o| o.borrow().as_ref().map(|o| (o.window.clone(), o.view.clone())))
}

/// Open the manage window at `url` (`chrome://extensions` when None), or load `url` in the one already open.
///
/// # Errors
/// CEF refused the browser view or the window, naming the url.
pub fn open(url: Option<&str>) -> Result<(), String> {
    let url = url.unwrap_or(DEFAULT_URL);
    if let Some((window, view)) = current() {
        let frame = view.browser().and_then(|b| b.main_frame()).ok_or_else(|| format!("the manage window has no page to load {url} in"))?;
        frame.load_url(Some(&CefString::from(url)));
        window.activate();
        window.bring_to_top();
        return Ok(());
    }
    let mut client = handlers::plain_client();
    let mut delegate = handlers::manage_view_delegate();
    let settings = BrowserSettings::default();
    let view = browser_view_create(Some(&mut client), Some(&CefString::from(url)), Some(&settings), None, None, Some(&mut delegate))
        .ok_or_else(|| format!("CEF refused the manage window's browser view at {url}"))?;
    let mut wd = handlers::manage_window(view.clone());
    let window = window_create_top_level(Some(&mut wd)).ok_or_else(|| format!("CEF refused the manage window at {url}"))?;
    OPEN.with(|o| *o.borrow_mut() = Some(Open { window, view }));
    Ok(())
}

/// The window was destroyed (its delegate).
pub fn destroyed() {
    let gone = OPEN.with(|o| o.borrow_mut().take());
    // The window and view references are released here, outside the borrow.
    drop(gone);
}

/// Close it, if open (the host is quitting).
pub fn close() {
    if let Some((window, _)) = current() {
        window.close();
    }
}
