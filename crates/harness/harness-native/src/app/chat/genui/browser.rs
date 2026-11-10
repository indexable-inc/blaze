//! The `Browser` element: a harness browser view in a cell, `<Browser view={page.view} />` (the host's id) or
//! `<Browser src="https://…" />` (a page the app asks the host to open once), drawn as hn.GenBrowser. Only its
//! `BrowserView` is set here; src/app/browser.rs fills its picture, title, address and loading hairline every frame and
//! makes a press open the view in the pane. A step's shown value `{t: "browser", view, ...}` draws nothing in the
//! transcript: a page opened for the person shows in the browser pane (chat.rs `chat_pane_page`).
use super::*;
use harness_native_ui_browser_api::BrowserView;

/// A harness browser view's id as a value names it: a whole number above 0, or the decimal digits of one, which is how
/// the kernel writes it (cdp.ts `Page.view` and mcp/src/shown.ts's `{t: "browser", view}` are strings: a u64 is no
/// JavaScript number). None for anything else.
pub(in crate::app) fn view_id(v: &Value) -> Option<u64> {
    let n = match v {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok()?,
        _ => return None,
    };
    (n > 0).then_some(n)
}

/// Why a `Browser` element is not drawn: neither or both of `view` and `src`, a view that is no id, a src that is no
/// http(s) URL.
pub(super) fn check(node: &Value) -> Option<String> {
    let src_ok = |s: &str| s.starts_with("https://") || s.starts_with("http://");
    match (prop(node, "view"), prop(node, "src")) {
        (Value::Null, Value::Null) => {
            Some("<Browser> needs view={page.view} (a harness browser view) or src=\"https://…\" (a page to open)".into())
        }
        (v, Value::Null) => match view_id(v) {
            Some(_) => None,
            None => Some(format!("<Browser> view is {v}, not a harness browser view id (page.view, a whole number above 0)")),
        },
        (Value::Null, Value::String(s)) if src_ok(s) => None,
        (Value::Null, Value::String(s)) => {
            let said: String = s.chars().take(48).collect();
            Some(format!("<Browser> src \u{201C}{said}\u{201D} is not an http(s) URL"))
        }
        (Value::Null, _) => Some("<Browser> src is not a string: src=\"https://…\"".into()),
        _ => Some("<Browser> takes view or src, not both".into()),
    }
}

/// Set the card's `page` to the view (or page) it names.
pub(super) fn element(v: View<'_>, node: &Value) {
    let want = BrowserView { view: view_id(prop(node, "view")).unwrap_or(0), src: words(node, "src").as_str().into() };
    if let Some(page) = v.part("page")
        && page.entity().try_cloned::<&BrowserView>().as_ref() != Some(&want)
    {
        page.set(want);
    }
}
