//! A small binary with the dependency shapes of dioxus-cli that blaze finds hard: each function
//! here mirrors one in `dx` that was left to rustc.
use std::{convert::Infallible, path::PathBuf};

use axum::{
    body::Body,
    http::{HeaderName, HeaderValue, Request, StatusCode},
    response::IntoResponse,
    routing::get_service,
};
use tower::{ServiceBuilder, ServiceExt};
use tower_http::{services::ServeDir, ServiceBuilderExt};
use tracing_subscriber::{fmt::format, prelude::*, Layer};

/// `dx::serve::server::build_serve_dir`.
fn serve_dir(out_dir: PathBuf, index_on_404: bool) -> axum::routing::MethodRouter {
    static CORS: (HeaderValue, HeaderValue) =
        (HeaderValue::from_static("require-corp"), HeaderValue::from_static("same-origin"));
    let (coep, coop) = CORS.clone();
    get_service(
        ServiceBuilder::new()
            .override_response_header(HeaderName::from_static("cross-origin-embedder-policy"), coep)
            .override_response_header(HeaderName::from_static("cross-origin-opener-policy"), coop)
            .and_then({
                let out_dir = out_dir.clone();
                move |response| async move { Ok(no_cache(index_on_404, &out_dir, response)) }
            })
            .service(ServeDir::new(&out_dir)),
    )
    .handle_error(|error: Infallible| async move {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Unhandled internal error: {error}"))
    })
}

fn no_cache(
    index_on_404: bool,
    _out_dir: &std::path::Path,
    response: axum::http::Response<tower_http::services::fs::ServeFileSystemResponseBody>,
) -> axum::response::Response {
    let mut response = response.into_response();
    if index_on_404 && response.status() == StatusCode::NOT_FOUND {
        *response.status_mut() = StatusCode::OK;
    }
    response
}

/// `dx::logging`: a formatting layer over a closure, boxed, behind a filter.
fn logging(json: bool, verbose: bool) {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .without_time()
        .fmt_fields(
            format::debug_fn(move |writer, field, value| {
                if field.name() == "dx_src" && !verbose {
                    return Ok(());
                }
                write!(writer, "{}={value:?}", field.name())
            })
            .delimited(" "),
        )
        .with_writer(std::io::stdout);
    let fmt_layer = if json { fmt_layer.json().flatten_event(true).boxed() } else { fmt_layer.boxed() }
        .with_filter(tracing_subscriber::filter::filter_fn(move |meta| {
            verbose || *meta.level() <= tracing::Level::INFO
        }));
    tracing_subscriber::registry().with(fmt_layer).init();
}

/// `dx::build::patch`: walks the operators of every function of a module.
fn operators(wasm: &[u8]) -> anyhow::Result<(usize, usize)> {
    let (mut functions, mut calls) = (0, 0);
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::CodeSectionEntry(body) = payload? {
            functions += 1;
            for op in body.get_operators_reader()? {
                if let wasmparser::Operator::Call { .. } = op? {
                    calls += 1;
                }
            }
        }
    }
    Ok((functions, calls))
}

/// `dx::opt::css::minify_css`.
fn minify_css(css: &str) -> anyhow::Result<String> {
    use lightningcss::{
        printer::PrinterOptions,
        stylesheet::{MinifyOptions, ParserOptions, StyleSheet},
        targets::{Browsers, Targets},
    };
    let options = ParserOptions { error_recovery: true, ..Default::default() };
    let mut stylesheet = StyleSheet::parse(css, options).map_err(|err| err.into_owned())?;
    let browsers = Browsers::from_browserslist(["defaults"]).expect("defaults");
    let targets = Targets { browsers, ..Default::default() };
    stylesheet.minify(MinifyOptions { targets, ..Default::default() })?;
    let res = stylesheet.to_css(PrinterOptions { targets, minify: true, ..Default::default() })?;
    Ok(res.code)
}

/// (module (func $f (result i32) i32.const 1) (func (result i32) call $f call $f i32.add))
const WASM: &[u8] = &[
    0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 127, 3, 3, 2, 0, 0, 10, 14, 2, 4, 0, 65, 1,
    11, 7, 0, 16, 0, 16, 0, 106, 11,
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logging(std::env::args().any(|it| it == "--json"), false);
    tracing::info!(dx_src = "hidden", shown = 3, "logging is up");
    tracing::debug!("not printed");

    println!("operators {:?}", operators(WASM)?);
    println!("css {}", minify_css("a { color: #ff0000; margin: 0px 0px 0px 0px }\n\nb{}")?);

    let dir = std::env::temp_dir().join("blaze-rung");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("hello.txt"), "served")?;
    let router = axum::Router::new().fallback_service(serve_dir(dir, true));
    for path in ["/hello.txt", "/missing"] {
        let request = Request::builder().uri(path).body(Body::empty())?;
        let response = router.clone().oneshot(request).await?;
        let policy = response.headers().get("cross-origin-opener-policy").cloned();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20).await?;
        println!("{path} {status} {policy:?} {:?}", String::from_utf8_lossy(&body));
    }
    let task = tokio::spawn(async { 6 * 7 });
    println!("spawned {}", task.await?);
    Ok(())
}
