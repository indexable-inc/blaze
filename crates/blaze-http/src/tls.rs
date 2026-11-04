//! A connection's bytes as HTTP reads and writes them: as they are for `http`, through
//! rustls's own sans-IO connection for `https` (`read_tls`, `process_new_packets`, `reader`,
//! `writer`, `write_tls`, driven by hand). What came from the network goes in, plaintext comes
//! out; plaintext goes in, what to send comes out.

#![blaze::sans_io]

use std::{
    io::{Read as _, Write as _},
    sync::Arc,
};

use rustls::{ClientConfig, ClientConnection, pki_types::ServerName};

pub(crate) enum Session {
    Plain,
    // Boxed: rustls's connection is large, and a plain one carries nothing.
    Tls(Box<ClientConnection>),
}

impl Session {
    /// TLS with the server `name`, as `config` says (its roots, its versions, no ALPN). Its
    /// handshake starts as the connection's first bytes are sent.
    pub(crate) fn tls(
        config: &Arc<ClientConfig>,
        name: ServerName<'static>,
    ) -> Result<Session, String> {
        // alloc: an error's message, made only when it fails.
        let mut conn =
            ClientConnection::new(Arc::clone(config), name).map_err(|e| e.to_string())?;
        // Plaintext given before the handshake ends waits in rustls; the client bounds what it
        // gives (one request's head and body), so rustls need not.
        conn.set_buffer_limit(None);
        // alloc: one per HTTPS connection, rustls's state for it.
        Ok(Session::Tls(Box::new(conn)))
    }

    /// Whether its TLS handshake is still going: a failure now is the handshake's.
    pub(crate) fn handshaking(&self) -> bool {
        match self {
            Session::Plain => false,
            Session::Tls(conn) => conn.is_handshaking(),
        }
    }

    /// Takes `plain` to send: for TLS, sealed once the handshake allows, as
    /// [`Session::outgoing`] hands it out.
    pub(crate) fn send(&mut self, plain: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        match self {
            Session::Plain => {
                out.extend_from_slice(plain);
                Ok(())
            }
            // alloc: an error's message, made only when it fails.
            Session::Tls(conn) => conn.writer().write_all(plain).map_err(|e| e.to_string()),
        }
    }

    /// Adds what is to be sent on the network to `out`: TLS's records (the handshake's, then
    /// sealed plaintext). Plain bytes went to `out` as they were sent.
    pub(crate) fn outgoing(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        let Session::Tls(conn) = self else { return Ok(()) };
        while conn.wants_write() {
            // alloc: an error's message, made only when it fails.
            conn.write_tls(out).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Takes `bytes` received from the network, adding the plaintext they hold to `plain`:
    /// whether the peer said it is done sending (TLS's `close_notify`).
    pub(crate) fn received(&mut self, bytes: &[u8], plain: &mut Vec<u8>) -> Result<bool, String> {
        let conn = match self {
            Session::Plain => {
                plain.extend_from_slice(bytes);
                return Ok(false);
            }
            Session::Tls(conn) => conn,
        };
        let mut input = bytes;
        let mut closed = false;
        while !input.is_empty() {
            // alloc: an error's message, made only when it fails.
            conn.read_tls(&mut input).map_err(|e| e.to_string())?;
            // alloc: an error's message, made only when it fails.
            let state = conn.process_new_packets().map_err(|e| e.to_string())?;
            let ready = state.plaintext_bytes_to_read();
            if ready > 0 {
                let at = plain.len();
                plain.resize(at + ready, 0);
                // alloc: an error's message, made only when it fails.
                conn.reader().read_exact(&mut plain[at..]).map_err(|e| e.to_string())?;
            }
            closed |= state.peer_has_closed();
        }
        Ok(closed)
    }
}
