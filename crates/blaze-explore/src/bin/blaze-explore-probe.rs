//! The WebTransport probe: does a browser connect to a WebTransport server on 127.0.0.1 from an
//! https page, pinning a self-signed certificate by its hash? An echo server with the
//! explorer's own certificate (`blaze_explore::cert`): it prints the link to the probe page
//! (`probe/index.html` in this crate, served from an https origin), and echoes each
//! bidirectional stream on itself, each unidirectional stream on one it opens, and each
//! datagram. It prints what each session's CONNECT carried (path, Origin, User-Agent), so a run
//! also says what Origin each browser sends.
//!
//! Its input is one optional JSON value (inline, a file's path, or `-` for stdin):
//! `{"page": "<the probe page's URL>", "port": <0 for any>}`. Ctrl-C stops it.

use std::{
    io::Read as _,
    net::{Ipv4Addr, SocketAddr},
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blaze_explore::cert::{Cert, Validity};
use wtransport::{Connection, Endpoint, RecvStream, SendStream, ServerConfig, endpoint::IncomingSession};

/// Where the probe page is published: indexable-inc/ix serves `probe/index.html` there.
const PAGE: &str = "https://ix.dev/explore/probe";

#[derive(Debug, serde_derive::Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    #[serde(default = "page")]
    page: String,
    #[serde(default)]
    port: u16,
}

fn page() -> String {
    PAGE.to_owned()
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("blaze-explore-probe: {why}");
            ExitCode::FAILURE
        }
    }
}

fn input() -> Result<Input, String> {
    let mut args = std::env::args().skip(1);
    let text = match (args.next(), args.next()) {
        (None, _) => "{}".to_owned(),
        (Some(_), Some(extra)) => return Err(format!("it takes one JSON value, and `{extra}` is a second")),
        (Some(arg), None) if arg == "-" => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text).map_err(|why| format!("stdin: {why}"))?;
            text
        }
        (Some(arg), None) if arg.trim_start().starts_with('{') => arg,
        (Some(path), None) => std::fs::read_to_string(&path).map_err(|why| format!("{path}: {why}"))?,
    };
    serde_json::from_str(&text).map_err(|why| format!("the input `{text}` is not {{\"page\", \"port\"}}: {why}"))
}

fn run() -> Result<(), String> {
    let input = input()?;
    if !input.page.starts_with("https://") {
        return Err(format!("the page `{}` is not https: a browser opens WebTransport only from a secure page", input.page));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|why| format!("the wall clock: {why}"))?.as_secs();
    let cert = Cert::make(Validity::starting(now)).map_err(|why| why.to_string())?;
    let identity = cert.identity().map_err(|why| why.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|why| format!("the tokio runtime: {why}"))?;
    runtime.block_on(serve(&input, identity, &cert))
}

async fn serve(input: &Input, identity: wtransport::Identity, cert: &Cert) -> Result<(), String> {
    let config = ServerConfig::builder()
        .with_bind_address(SocketAddr::from((Ipv4Addr::LOCALHOST, input.port)))
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .build();
    let endpoint = Endpoint::server(config).map_err(|why| format!("listening on 127.0.0.1:{}: {why}", input.port))?;
    let port = endpoint.local_addr().map_err(|why| format!("the bound port: {why}"))?.port();
    println!("{}#127.0.0.1:{port}/{}", input.page, cert.hash);
    println!("certificate valid until {} (Unix seconds); Ctrl-C stops the probe", cert.validity.not_after_s);
    loop {
        tokio::spawn(echo(endpoint.accept().await));
    }
}

async fn echo(incoming: IncomingSession) {
    let request = match incoming.await {
        Ok(it) => it,
        Err(why) => {
            println!("a connection failed before its CONNECT: {why}");
            return;
        }
    };
    println!(
        "CONNECT {} from {}: Origin {:?}, User-Agent {:?}",
        request.path(),
        request.remote_address(),
        request.origin(),
        request.user_agent()
    );
    let connection = match request.accept().await {
        Ok(it) => it,
        Err(why) => {
            println!("the session did not open: {why}");
            return;
        }
    };
    // clone: handles (`Arc`s) of the session's connection
    tokio::join!(bidi(connection.clone()), uni(connection.clone()), datagrams(connection.clone()));
    println!("session ended: {}", connection.closed().await);
}

async fn bidi(connection: Connection) {
    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
        tokio::spawn(async move {
            let echoed = copy(&mut recv, &mut send).await;
            _ = send.finish().await;
            println!("bidirectional stream: {echoed} bytes echoed");
        });
    }
}

async fn uni(connection: Connection) {
    while let Ok(mut recv) = connection.accept_uni().await {
        // clone: a handle (an `Arc`) of the session's connection
        let connection = connection.clone();
        tokio::spawn(async move {
            let Ok(opening) = connection.open_uni().await else { return };
            let Ok(mut send) = opening.await else { return };
            let echoed = copy(&mut recv, &mut send).await;
            _ = send.finish().await;
            println!("unidirectional stream: {echoed} bytes echoed on one the probe opened");
        });
    }
}

async fn datagrams(connection: Connection) {
    while let Ok(datagram) = connection.receive_datagram().await {
        let payload = datagram.payload();
        match connection.send_datagram(&payload) {
            Ok(()) => println!("datagram: {} bytes echoed", payload.len()),
            Err(why) => println!("datagram of {} bytes not echoed: {why}", payload.len()),
        }
    }
}

/// Copies `recv` to `send` until `recv` ends: the bytes copied.
async fn copy(recv: &mut RecvStream, send: &mut SendStream) -> usize {
    // alloc: once a stream
    let mut chunk = vec![0u8; 64 * 1024];
    let mut copied = 0;
    while let Ok(Some(n)) = recv.read(&mut chunk).await {
        if send.write_all(&chunk[..n]).await.is_err() {
            break;
        }
        copied += n;
    }
    copied
}
