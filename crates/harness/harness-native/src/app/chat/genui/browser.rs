//! The `Browser` element: a harness browser view in a cell, `<Browser view={page.view} />` (the host's id) or
//! `<Browser src="https://…" />` (a page the app asks the host to open once), drawn as hn.GenBrowser. Only its
//! `BrowserView` is set here; src/app/browser.rs fills its picture, title, address and loading hairline every frame and
//! makes a press open the view in the pane. A step's shown value `{t: "browser", view, url, title}` is drawn as the same
//! element ([`shown_card`], steps.rs value_outs).
use super::*;
use harness_native_ui_browser_api::BrowserView;

/// Why a `Browser` element is not drawn: neither or both of `view` and `src`, a view that is no id, a src that is no
/// http(s) URL.
pub(super) fn check(node: &Value) -> Option<String> {
    let src_ok = |s: &str| s.starts_with("https://") || s.starts_with("http://");
    match (prop(node, "view"), prop(node, "src")) {
        (Value::Null, Value::Null) => {
            Some("<Browser> needs view={page.view} (a harness browser view) or src=\"https://…\" (a page to open)".into())
        }
        (Value::Number(n), Value::Null) => match n.as_u64() {
            Some(v) if v > 0 => None,
            _ => Some(format!("<Browser> view is {n}, not a harness browser view id (page.view, a whole number above 0)")),
        },
        (Value::Null, Value::String(s)) if src_ok(s) => None,
        (Value::Null, Value::String(s)) => {
            let said: String = s.chars().take(48).collect();
            Some(format!("<Browser> src \u{201C}{said}\u{201D} is not an http(s) URL"))
        }
        (Value::Null, _) => Some("<Browser> src is not a string: src=\"https://…\"".into()),
        (Value::Number(_), _) => Some("<Browser> takes view or src, not both".into()),
        _ => Some("<Browser> view is not a number: view={page.view}".into()),
    }
}

/// Set the card's `page` to the view (or page) it names.
pub(super) fn element(v: View<'_>, node: &Value) {
    let want = BrowserView { view: prop(node, "view").as_u64().unwrap_or(0), src: words(node, "src").as_str().into() };
    if let Some(page) = v.part("page")
        && page.entity().try_cloned::<&BrowserView>().as_ref() != Some(&want)
    {
        page.set(want);
    }
}

/// The `Browser` element a step's shown value `{t: "browser", view, url, title}` (mcp/src/shown.ts) draws as.
pub(in crate::app) fn shown_card(v: &Value) -> Value {
    serde_json::json!({"type": "Browser", "props": {"view": v["view"]}, "children": []})
}
