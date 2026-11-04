//! HTTP/3 for [`super::HyperHttp`]: QUIC by quinn and HTTP/3 by h3, on the rustls/ring provider
//! and the machine's root certificates the TCP path uses. One QUIC connection per host and port,
//! made at its first request and shared by every request after (HTTP/3 multiplexes them), made
//! again when the server closed it (an idle timeout, a GOAWAY). Every wait opens a span naming
//! the host, the port and why HTTP/3 was chosen ([`Source`]).
//!
//! A failure at the transport (no address, the handshake timing out or refused, the connection
//! or a stream reset) is [`Http3Failed`]: never sent again over TCP (rule no-fallback). The one
//! request sent again is one whose stream could not be opened on a connection the server had
//! already closed, on a new connection to the same host over HTTP/3: nothing of it was sent.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::{Buf as _, Bytes};
use hyper::{HeaderMap, Method, StatusCode};
use tracing::Instrument as _;

use super::choice::{Http3Failed, Source};

/// The most a QUIC handshake (and HTTP/3's settings after it) may take: a network that drops
/// UDP says so in this long, not in the request timeout's 30 seconds.
const HANDSHAKE_MOST: Duration = Duration::from_secs(10);

type SendRequest = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
/// One request's stream: its answer's head and body are read from it.
pub(super) type Stream = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

/// A QUIC connection to one host and port, and HTTP/3 over it.
#[derive(Clone)]
struct Connection {
    quic: quinn::Connection,
    send: SendRequest,
}

/// Where one request goes, and what its errors say.
pub(super) struct Target<'a> {
    pub host: &'a str,
    pub port: u16,
    pub source: Source,
    /// The address, as messages say it ([`blaze_fetch::redact`]).
    pub said: &'a str,
}

/// The HTTP/3 side of the client: its endpoints and its connections.
pub(super) struct H3 {
    client: quinn::ClientConfig,
    /// The UDP sockets connections are made from, one per address family, made at first use.
    v4: tokio::sync::OnceCell<quinn::Endpoint>,
    v6: tokio::sync::OnceCell<quinn::Endpoint>,
    /// Each host and port's connection, behind a lock held while one is made so that requests
    /// arriving together make one, not one each.
    connections: blaze_hash::SharedMap<(String, u16), Arc<tokio::sync::Mutex<Option<Connection>>>>,
    /// How long a connection or a stream may go without a byte, as the TCP path's.
    timeout: Duration,
    /// The configuration file the fix goes in, as messages say it.
    config: String,
}

impl H3 {
    pub(super) fn new(timeout: Duration, config: String) -> anyhow::Result<H3> {
        let found = rustls_native_certs::load_native_certs();
        let mut roots = rustls::RootCertStore::empty();
        let (added, _) = roots.add_parsable_certificates(found.certs);
        if added == 0 {
            let errors: Vec<String> = found.errors.iter().map(ToString::to_string).collect(); // alloc: an error's message
            anyhow::bail!("no root certificate to check HTTP/3 servers with was found on this machine ({})", errors.join("; "));
        }
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()]; // alloc: once per client
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls))?;
        let mut client = quinn::ClientConfig::new(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        // The server closes an idle connection anyway; blaze sends no keep-alives, and makes a
        // new connection at the next request when this one has gone.
        transport.max_idle_timeout(Some(quinn::IdleTimeout::try_from(timeout)?));
        client.transport_config(Arc::new(transport));
        Ok(H3 {
            client,
            v4: tokio::sync::OnceCell::new(),
            v6: tokio::sync::OnceCell::new(),
            connections: blaze_hash::SharedMap::default(),
            timeout,
            config,
        })
    }

    /// A failure of a request to `target`, saying `what`.
    fn failed(&self, target: &Target<'_>, what: String) -> Http3Failed {
        Http3Failed {
            host: target.host.to_owned(),
            port: target.port,
            url: target.said.to_owned(),
            source: target.source,
            what,
            config: self.config.clone(),
        }
    }

    /// Sends `method` of `uri` with `headers` and `body` to `target`, and reads its answer's
    /// head: its status, its headers, and the stream its body comes on.
    pub(super) async fn send(
        &self,
        target: &Target<'_>,
        request: http::Request<()>,
        body: Bytes,
    ) -> Result<(StatusCode, HeaderMap, Stream), Http3Failed> {
        let method: Method = request.method().clone();
        let (mut connection, reused) = self.connection(target).await?;
        let opened = self.open(target, &mut connection, clone_request(&request)).await;
        let mut stream = match opened {
            Ok(stream) => stream,
            // The server closed the connection since it was last used (an idle timeout, a
            // GOAWAY) and nothing of this request was sent: once on a new one, over HTTP/3.
            Err(_) if reused && connection.quic.close_reason().is_some() => {
                let (mut fresh, _) = self.connection(target).await?;
                self.open(target, &mut fresh, request).await.map_err(|e| self.failed(target, e))?
            }
            Err(e) => return Err(self.failed(target, e)),
        };
        let waited = async {
            if !body.is_empty() {
                stream.send_data(body).await.map_err(|e| format!("sending the body of a {method} request: {e}"))?;
            }
            stream.finish().await.map_err(|e| format!("ending the {method} request: {e}"))?;
            stream.recv_response().await.map_err(|e| format!("waiting for the answer: {e}"))
        };
        let span = tracing::info_span!(target: "blaze", "http3 answer", host = target.host, port = target.port, source = target.source.name());
        let answer = tokio::time::timeout(self.timeout, waited.instrument(span))
            .await
            .map_err(|_| self.failed(target, format!("no answer in {} ms", self.timeout.as_millis())))?
            .map_err(|e| self.failed(target, e))?;
        let (parts, ()) = answer.into_parts();
        Ok((parts.status, parts.headers, stream))
    }

    /// The next piece of `stream`'s body, none at its end.
    pub(super) async fn next(&self, target: &Target<'_>, stream: &mut Stream) -> Result<Option<Bytes>, Http3Failed> {
        let span = tracing::info_span!(target: "blaze", "http3 body", host = target.host, port = target.port, source = target.source.name());
        let piece = tokio::time::timeout(self.timeout, stream.recv_data().instrument(span))
            .await
            .map_err(|_| self.failed(target, format!("the transfer stalled for {} ms", self.timeout.as_millis())))?
            .map_err(|e| self.failed(target, format!("reading the body: {e}")))?;
        Ok(piece.map(|mut it| it.copy_to_bytes(it.remaining())))
    }

    /// Opens a stream on `connection` and sends `request`'s head.
    async fn open(&self, target: &Target<'_>, connection: &mut Connection, request: http::Request<()>) -> Result<Stream, String> {
        let span = tracing::info_span!(target: "blaze", "http3 open stream", host = target.host, port = target.port, source = target.source.name());
        match tokio::time::timeout(self.timeout, connection.send.send_request(request).instrument(span)).await {
            Err(_) => Err(format!("no stream opened in {} ms (the server allows no more at once)", self.timeout.as_millis())),
            Ok(Err(e)) => Err(format!("opening a stream: {e}")),
            Ok(Ok(stream)) => Ok(stream),
        }
    }

    /// `target`'s connection, made if there is none or the server closed it; and whether it
    /// was there already.
    async fn connection(&self, target: &Target<'_>) -> Result<(Connection, bool), Http3Failed> {
        let slot = {
            let connections = self.connections.pin();
            // alloc: once per host and port the process asks over HTTP/3
            connections.get_or_insert_with((target.host.to_owned(), target.port), || Arc::new(tokio::sync::Mutex::new(None))).clone()
        };
        let span = tracing::info_span!(target: "blaze", "http3 connection lock", host = target.host, port = target.port);
        let mut slot = slot.lock().instrument(span).await;
        if let Some(open) = (*slot).as_ref().filter(|it| it.quic.close_reason().is_none()) {
            return Ok((open.clone(), true));
        }
        let span = tracing::info_span!(target: "blaze", "http3 connect", host = target.host, port = target.port, source = target.source.name());
        let made = tokio::time::timeout(HANDSHAKE_MOST, self.connect(target).instrument(span))
            .await
            .map_err(|_| {
                self.failed(
                    target,
                    format!(
                        "the QUIC handshake got no answer in {} ms (UDP to port {} may be blocked on this network)",
                        HANDSHAKE_MOST.as_millis(),
                        target.port
                    ),
                )
            })??;
        *slot = Some(made.clone());
        Ok((made, false))
    }

    /// A new connection to `target`: its address from getaddrinfo, the QUIC handshake, then
    /// HTTP/3's, its driver running on the runtime until the connection closes.
    async fn connect(&self, target: &Target<'_>) -> Result<Connection, Http3Failed> {
        let addresses = tokio::net::lookup_host((target.host, target.port))
            .instrument(tracing::info_span!(target: "blaze", "dns addresses", host = target.host))
            .await
            .map_err(|e| self.failed(target, format!("resolving {}: {e}", target.host)))?;
        // The first in the system's order (RFC 6724 on getaddrinfo's side).
        let address: SocketAddr =
            addresses.into_iter().next().ok_or_else(|| self.failed(target, format!("{} has no address", target.host)))?;
        let endpoint = self.endpoint(address).await.map_err(|e| self.failed(target, e))?;
        let connecting = endpoint
            .connect_with(self.client.clone(), address, target.host)
            .map_err(|e| self.failed(target, format!("starting a QUIC connection to {address}: {e}")))?;
        let quic = connecting.await.map_err(|e| self.failed(target, format!("the QUIC handshake with {address}: {e}")))?;
        let (mut driver, send) = h3::client::new(h3_quinn::Connection::new(quic.clone()))
            .await
            .map_err(|e| self.failed(target, format!("HTTP/3's start on {address}: {e}")))?;
        let host = target.host.to_owned();
        tokio::spawn(
            async move {
                // Ends when the connection closes; the next request sees it closed and makes one.
                let closed = driver.wait_idle().await;
                tracing::debug!(target: "blaze", host = %host, "HTTP/3 connection closed: {closed}");
            }
            .instrument(tracing::info_span!(target: "blaze", "http3 connection", host = target.host, port = target.port)),
        );
        Ok(Connection { quic, send })
    }

    /// The endpoint for `address`'s family, made at first use.
    async fn endpoint(&self, address: SocketAddr) -> Result<quinn::Endpoint, String> {
        let (cell, any): (_, SocketAddr) = match address {
            SocketAddr::V4(_) => (&self.v4, (std::net::Ipv4Addr::UNSPECIFIED, 0).into()),
            SocketAddr::V6(_) => (&self.v6, (std::net::Ipv6Addr::UNSPECIFIED, 0).into()),
        };
        cell.get_or_try_init(|| async { quinn::Endpoint::client(any).map_err(|e| format!("opening a UDP socket on {any}: {e}")) })
            .await
            .cloned()
    }
}

/// `request` again, for the one try on a new connection ([`H3::send`]).
fn clone_request(request: &http::Request<()>) -> http::Request<()> {
    let mut copy = http::Request::new(());
    *copy.method_mut() = request.method().clone();
    *copy.uri_mut() = request.uri().clone();
    *copy.version_mut() = request.version();
    *copy.headers_mut() = request.headers().clone();
    copy
}
