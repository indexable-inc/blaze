//! TLS over the bytes of a connection, with no IO of its own: what came from the network goes
//! in, plaintext comes out, and what to send is taken out (rustls' buffered connection, driven
//! by hand). [`TlsClient`] is how a machine reaches the global cache and [`TlsServer`] how the
//! cache's server is reached: the client checks the server's certificate against the
//! machine's roots and its name.
//!
//! A [`Channel`] is the seam: the client and server state machines speak HTTP through one,
//! so the simulation tests drive them through a channel that passes bytes as they are, which
//! nothing outside the tests can make.

use std::{
    io::{Read as _, Write as _},
    sync::Arc,
};

use rustls::{ClientConfig, ClientConnection, Connection, RootCertStore, ServerConfig, ServerConnection, pki_types::ServerName};

/// One connection's bytes, as plaintext.
pub trait Channel {
    /// Takes bytes received from the network, adding the plaintext they hold to `plain`.
    fn received(&mut self, bytes: &[u8], plain: &mut Vec<u8>) -> Result<(), String>;
    /// Takes plaintext to send.
    fn send(&mut self, plain: &[u8]) -> Result<(), String>;
    /// Adds what is to be sent on the network to `out`.
    fn outgoing(&mut self, out: &mut Vec<u8>) -> Result<(), String>;
    /// Whether the peer said it is done sending.
    fn peer_closed(&self) -> bool;
}

/// Makes the channel of each connection.
pub trait Channels {
    type Channel: Channel;
    fn channel(&self) -> Result<Self::Channel, String>;
}

/// A rustls connection driven by hand.
pub struct Tls {
    conn: Connection,
    closed: bool,
}

impl Tls {
    fn new(mut conn: Connection) -> Tls {
        // What it is given before the handshake ends waits in it; the state machines bound
        // what they give.
        conn.set_buffer_limit(None);
        Tls { conn, closed: false }
    }
}

impl Channel for Tls {
    fn received(&mut self, bytes: &[u8], plain: &mut Vec<u8>) -> Result<(), String> {
        let mut input = bytes;
        while !input.is_empty() {
            // alloc: an error's message, made only when it fails.
            self.conn.read_tls(&mut input).map_err(|error| format!("TLS: {error}"))?;
            // alloc: an error's message, made only when it fails.
            let state = self.conn.process_new_packets().map_err(|error| format!("TLS: {error}"))?;
            let ready = state.plaintext_bytes_to_read();
            if ready > 0 {
                let at = plain.len();
                plain.resize(at + ready, 0);
                // alloc: an error's message, made only when it fails.
                self.conn.reader().read_exact(&mut plain[at..]).map_err(|error| format!("TLS: {error}"))?;
            }
            if state.peer_has_closed() {
                self.closed = true;
            }
        }
        Ok(())
    }

    fn send(&mut self, plain: &[u8]) -> Result<(), String> {
        // alloc: an error's message, made only when it fails.
        self.conn.writer().write_all(plain).map_err(|error| format!("TLS: {error}"))
    }

    fn outgoing(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        while self.conn.wants_write() {
            // alloc: an error's message, made only when it fails.
            self.conn.write_tls(out).map_err(|error| format!("TLS: {error}"))?;
        }
        Ok(())
    }

    fn peer_closed(&self) -> bool {
        self.closed
    }
}

/// A machine's side: the server's certificate is checked against `config`'s roots, for the
/// cache's host name.
#[derive(Clone)]
pub struct TlsClient {
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
}

impl TlsClient {
    /// The machine's root certificates, rustls' ring provider, TLS 1.2 and 1.3.
    pub fn new(host: &str) -> Result<TlsClient, String> {
        let found = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        let (added, _) = roots.add_parsable_certificates(found.certs);
        if added == 0 {
            // alloc: setup: once a TLS config or a connection.
            let errors: Vec<String> = found.errors.iter().map(ToString::to_string).collect();
            // alloc: an error's message, made only when it fails.
            return Err(format!(
                "no root certificate to check the global cache at {host} with was found on this machine ({})",
                // alloc: an error's message, made only when it fails.
                if errors.is_empty() { "no errors loading them".to_owned() } else { errors.join("; ") }
            ));
        }
        // alloc: setup: once a TLS config or a connection.
        let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            // alloc: an error's message, made only when it fails.
            .map_err(|error| format!("TLS versions: {error}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        // alloc: setup: once a TLS config or a connection.
        TlsClient::with_config(Arc::new(config), host)
    }

    pub fn with_config(config: Arc<ClientConfig>, host: &str) -> Result<TlsClient, String> {
        // alloc: an error's message, made only when it fails.
        let name = ServerName::try_from(host.to_owned()).map_err(|error| format!("{host:?} as a TLS server name: {error}"))?;
        Ok(TlsClient { config, name })
    }
}

impl Channels for TlsClient {
    type Channel = Tls;

    fn channel(&self) -> Result<Tls, String> {
        // alloc: setup: once a TLS config or a connection.
        let conn = ClientConnection::new(Arc::clone(&self.config), self.name.clone())
            // alloc: an error's message, made only when it fails.
            .map_err(|error| format!("TLS: {error}"))?;
        Ok(Tls::new(Connection::Client(conn)))
    }
}

/// The cache server's side, with its certificate in `config`.
#[derive(Clone)]
pub struct TlsServer {
    config: Arc<ServerConfig>,
}

impl TlsServer {
    pub fn new(config: Arc<ServerConfig>) -> TlsServer {
        TlsServer { config }
    }
}

impl Channels for TlsServer {
    type Channel = Tls;

    fn channel(&self) -> Result<Tls, String> {
        // alloc: an error's message, made only when it fails.
        let conn = ServerConnection::new(Arc::clone(&self.config)).map_err(|error| format!("TLS: {error}"))?;
        Ok(Tls::new(Connection::Server(conn)))
    }
}

#[cfg(test)]
pub(crate) mod clear {
    use super::*;

    /// Bytes as they are: the simulation tests' channel.
    #[derive(Default)]
    pub(crate) struct Clear {
        out: Vec<u8>,
    }

    impl Channel for Clear {
        fn received(&mut self, bytes: &[u8], plain: &mut Vec<u8>) -> Result<(), String> {
            plain.extend_from_slice(bytes);
            Ok(())
        }

        fn send(&mut self, plain: &[u8]) -> Result<(), String> {
            self.out.extend_from_slice(plain);
            Ok(())
        }

        fn outgoing(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
            out.append(&mut self.out);
            Ok(())
        }

        fn peer_closed(&self) -> bool {
            false
        }
    }

    #[derive(Clone, Copy, Default)]
    pub(crate) struct Clears;

    impl Channels for Clears {
        type Channel = Clear;

        fn channel(&self) -> Result<Clear, String> {
            Ok(Clear::default())
        }
    }

    #[test]
    fn a_tls_client_names_its_server() {
        let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        let client = TlsClient::with_config(Arc::new(config), "cache.example.com").unwrap();
        let mut tls = client.channel().unwrap();
        let mut hello = Vec::new();
        tls.outgoing(&mut hello).unwrap();
        assert_eq!(hello[0], 0x16, "a TLS handshake record");
        assert!(memchr::memmem::find(&hello, b"cache.example.com").is_some(), "SNI names the host");
        assert!(tls.received(b"not TLS at all", &mut Vec::new()).is_err());
        assert!(TlsClient::with_config(client.config.clone(), "not a name!").is_err());
    }
}
