//! The TLS every `https` connection of blaze's HTTP client makes: rustls on ring at its safe
//! default versions, checking servers against the machine's roots, with no client certificate
//! and no ALPN (this client speaks HTTP/1.1 only, as hyper-rustls's HTTP/1-only connector
//! offered none). Not part of the core: it reads the machine's certificate files, once, as the
//! client is made.

use std::{fmt, sync::Arc};

/// Why the client's TLS can't be set up.
#[derive(Debug)]
pub enum TlsSetup {
    /// The machine's certificates gave no root to check servers with: `skipped` were found
    /// that did not parse, and `errors` is what went wrong reading them, as
    /// rustls-native-certs said it.
    NoRoots { skipped: usize, errors: Vec<String> },
    /// rustls on ring refused its safe default TLS versions: why.
    Versions(String),
}

impl fmt::Display for TlsSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsSetup::NoRoots { skipped, errors } => write!(
                f,
                "the machine's certificates cannot be read: no root certificate to check servers with ({skipped} did not parse; errors loading them: {errors:?})"
            ),
            TlsSetup::Versions(why) => {
                write!(f, "rustls on ring has no safe default TLS versions: {why}")
            }
        }
    }
}

impl std::error::Error for TlsSetup {}

/// rustls on ring with the machine's roots, as hyper-rustls's `with_native_roots` read them: a
/// certificate that does not parse is skipped, none at all is [`TlsSetup::NoRoots`].
pub fn native() -> Result<Arc<rustls::ClientConfig>, TlsSetup> {
    let found = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    let (added, skipped) = roots.add_parsable_certificates(found.certs);
    if added == 0 {
        // alloc: an error's message, made only when it fails.
        let errors = found.errors.iter().map(ToString::to_string).collect();
        return Err(TlsSetup::NoRoots { skipped, errors });
    }
    with_roots(roots)
}

/// rustls on ring at its safe default versions, checking servers against `roots`, with no
/// client certificate and no ALPN: [`native`]'s, and a test's with its own root.
pub fn with_roots(roots: rustls::RootCertStore) -> Result<Arc<rustls::ClientConfig>, TlsSetup> {
    // alloc: the client's TLS setup, made once with it.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        // alloc: an error's message, made only when it fails.
        .map_err(|e| TlsSetup::Versions(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // alloc: the client's TLS setup, made once with it.
    Ok(Arc::new(config))
}
