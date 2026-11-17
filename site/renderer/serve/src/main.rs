//! The whole site as one wasi:http component, from the renderer core: pages rendered from the embedded snapshot and
//! the build's assets.
//!
//!   wasmtime serve -S cli target/wasm32-wasip2/release/site-serve.wasm
//!
//! A demo, not the production path (the TS worker serves the build's static pages; README.md).

use site_render::Site;
use wstd::http::{Body, Error, Request, Response, StatusCode};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
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
    let site = Site::from_snapshot(embedded::SNAPSHOT).map_err(Error::msg)?;
    let html = "text/html; charset=utf-8";
    // Pages and assets sit under the site's base (`Served` on `site`, /blaze); dist/ and the page URLs have none.
    let path = match req.uri().path().strip_prefix(site.served().base.as_str()) {
        Some(p) if p.starts_with('/') => p.to_string(),
        _ => {
            let r = site.render("/404").map_err(Error::msg)?;
            return respond(StatusCode::NOT_FOUND, html, r, "404");
        }
    };
    if let Some((_, bytes)) = embedded::ASSETS.iter().find(|(p, _)| *p == path) {
        return respond(StatusCode::OK, content_type(&path), *bytes, "asset");
    }
    let url = path.strip_suffix(".html").unwrap_or(&path);
    let url = if url.len() > 1 { url.trim_end_matches('/') } else { url };
    if site.page(url).is_none() {
        let r = site.render("/404").map_err(Error::msg)?;
        return respond(StatusCode::NOT_FOUND, html, r, "404");
    }
    respond(StatusCode::OK, html, site.render(url).map_err(Error::msg)?, "page")
}
