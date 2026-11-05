//! A hyper server behind rustls on a local port, with a certificate made for the run, and a
//! hyper client that fetches from it over TLS 1.3 and 1.2, HTTP/1.1 and HTTP/2.

use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;

type Failure = Box<dyn std::error::Error + Send + Sync>;

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3))
}

/// A body of `len` bytes that depends on nothing but `len`.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|at| (at * 31 % 251) as u8).collect()
}

async fn respond(request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Failure> {
    let path = request.uri().path().to_owned();
    let version = request.version();
    let response = match path.split('/').collect::<Vec<_>>()[..] {
        ["", "hello"] => Response::new(Full::from(format!("hello over {version:?}\n"))),
        ["", "bytes", len] => Response::new(Full::from(payload(len.parse()?))),
        ["", "echo"] => {
            let body = request.into_body().collect().await?.to_bytes();
            Response::new(Full::from(format!("{} bytes, {:x}\n", body.len(), fnv(&body))))
        }
        _ => {
            let mut missing = Response::new(Full::from("no such page\n"));
            *missing.status_mut() = StatusCode::NOT_FOUND;
            missing
        }
    };
    Ok(response)
}

async fn serve(listener: TcpListener, config: rustls::ServerConfig) {
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    loop {
        let Ok((stream, _)) = listener.accept().await else { return };
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let Ok(stream) = acceptor.accept(stream).await else { return };
            let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            let _ = builder.serve_connection(TokioIo::new(stream), service_fn(respond)).await;
        });
    }
}

type Client = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

fn client(roots: &rustls::RootCertStore, tls12: bool, http2: bool) -> Result<Client, Failure> {
    let versions: &[&rustls::SupportedProtocolVersion] =
        if tls12 { &[&rustls::version::TLS12] } else { &[&rustls::version::TLS13] };
    let config = rustls::ClientConfig::builder_with_protocol_versions(versions)
        .with_root_certificates(roots.clone())
        .with_no_client_auth();
    let connector = hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(config).https_only();
    let connector = if http2 { connector.enable_http2().build() } else { connector.enable_http1().build() };
    Ok(hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector))
}

async fn fetch(client: &Client, addr: SocketAddr, path: &str, body: Vec<u8>) -> Result<String, Failure> {
    let method = if body.is_empty() { "GET" } else { "POST" };
    let request = Request::builder()
        .method(method)
        .uri(format!("https://localhost:{}{path}", addr.port()))
        .body(Full::from(body))?;
    let response = client.request(request).await?;
    let (status, version) = (response.status(), response.version());
    let body = response.into_body().collect().await?.to_bytes();
    let shown = if body.len() < 80 { String::from_utf8_lossy(&body).trim().to_owned() } else { format!("{:x}", fnv(&body)) };
    Ok(format!("{method} {path} {status} {version:?} {} bytes: {shown}", body.len()))
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    let rounds: usize = std::env::args().nth(1).and_then(|it| it.parse().ok()).unwrap_or(1);
    rustls::crypto::ring::default_provider().install_default().map_err(|_| "a provider was installed")?;
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    let cert: CertificateDer<'static> = made.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.key_pair.serialize_der()));
    let mut config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert.clone()], key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(serve(listener, config));

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert)?;
    for (tls12, http2) in [(false, false), (false, true), (true, false), (true, true)] {
        let client = client(&roots, tls12, http2)?;
        println!("TLS 1.{} HTTP/{}", if tls12 { 2 } else { 3 }, if http2 { 2 } else { 1 });
        for _ in 0..rounds {
            for path in ["/hello", "/bytes/0", "/bytes/1", "/bytes/70000", "/bytes/3000000", "/nothing"] {
                println!("  {}", fetch(&client, addr, path, Vec::new()).await?);
            }
            println!("  {}", fetch(&client, addr, "/echo", payload(500_000)).await?);
            let mut many = tokio::task::JoinSet::new();
            for n in 0..32usize {
                let client = client.clone();
                many.spawn(async move { fetch(&client, addr, "/bytes/40000", vec![n as u8; n]).await.is_ok() });
            }
            let mut ok = 0;
            while let Some(fetched) = many.join_next().await {
                ok += usize::from(fetched?);
            }
            println!("  32 at once: {ok} fetched");
        }
    }
    // A certificate the client was not told to trust is refused.
    let stranger = client(&rustls::RootCertStore::empty(), false, false)?;
    println!("untrusted: {}", fetch(&stranger, addr, "/hello", Vec::new()).await.is_err());
    Ok(())
}
