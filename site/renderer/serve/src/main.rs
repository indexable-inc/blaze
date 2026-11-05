//! The whole site as one wasi:http component, from the renderer core: pages rendered from the embedded snapshot,
//! discussable ones with their comments from a `CommentStore`, and the build's assets.
//!
//!   wasmtime serve -S cli target/wasm32-wasip2/release/site-serve.wasm
//!
//! A demo, not the production path (the TS worker keeps routing, auth and D1; README.md): the viewer comes from an
//! `x-demo-user` header (`octo`, `mallory`, `admin`), and the store is in memory, seeded from fixtures/comments.json.
//! `wasmtime serve` gives each request a fresh instance, so nothing written would outlive its request; the store has
//! no writes.

use std::collections::BTreeMap;

use site_render::{Comment, Discussion, Site, Viewer};
use wstd::http::{Body, Error, Request, Response, StatusCode};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}

/// Where comments come from: D1 in the worker, memory here.
trait CommentStore {
    /// The doc's comments, oldest first.
    fn comments(&self, doc: &str) -> Vec<Comment>;
}

struct MemoryStore(BTreeMap<String, Vec<Comment>>);

impl CommentStore for MemoryStore {
    fn comments(&self, doc: &str) -> Vec<Comment> {
        self.0.get(doc).cloned().unwrap_or_default()
    }
}

const FIXTURES: &str = include_str!("../../fixtures/comments.json");

fn viewer(req: &Request<Body>) -> Option<Viewer> {
    let login = req.headers().get("x-demo-user")?.to_str().ok()?;
    let (id, admin) = match login {
        "octo" => (1, false),
        "mallory" => (2, false),
        "admin" => (3, true),
        _ => return None,
    };
    Some(Viewer { id, login: login.to_string(), admin })
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("avif") => "image/avif",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn respond(
    status: StatusCode,
    ty: &str,
    body: impl Into<Body>,
    how: &str,
) -> Result<Response<Body>, Error> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", ty)
        .header("x-render", how)
        .body(body.into())?)
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let mut site = Site::from_snapshot(embedded::SNAPSHOT).map_err(Error::msg)?;
    let html = "text/html; charset=utf-8";
    // Pages and assets sit under the site's base (`Served` on `site`, /blaze); dist/ and the page URLs have none.
    let path = match req.uri().path().strip_prefix(site.served().base.as_str()) {
        Some(p) if p.starts_with('/') => p.to_string(),
        _ => {
            let r = site.render("/404", &[], &Discussion::default()).map_err(Error::msg)?;
            return respond(StatusCode::NOT_FOUND, html, r.html, "404");
        }
    };
    if let Some((_, bytes)) = embedded::ASSETS.iter().find(|(p, _)| *p == path) {
        return respond(StatusCode::OK, content_type(&path), *bytes, "asset");
    }
    let url = path.strip_suffix(".html").unwrap_or(&path);
    let url = if url.len() > 1 { url.trim_end_matches('/') } else { url };
    let Some(page) = site.page(url).cloned() else {
        let r = site.render("/404", &[], &Discussion::default()).map_err(Error::msg)?;
        return respond(StatusCode::NOT_FOUND, html, r.html, "404");
    };
    if !site.is_discussable(&page) {
        let r = site.render(&page.url, &[], &Discussion::default()).map_err(Error::msg)?;
        return respond(StatusCode::OK, html, r.html, "static");
    }
    let store = MemoryStore(serde_json::from_str(FIXTURES).map_err(Error::from)?);
    let discussion = Discussion { viewer: viewer(&req), ..Discussion::default() };
    let how = if discussion.viewer.is_some() { "user" } else { "anonymous" };
    let r = site.render(&page.url, &store.comments(&page.doc), &discussion).map_err(Error::msg)?;
    respond(StatusCode::OK, html, r.html, how)
}
