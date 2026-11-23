//! The TLS every `https` connection of blaze's HTTP client makes: rustls on ring at its safe
//! default versions, checking servers against the roots it is given (rule 75), with no client
//! certificate and no ALPN (this client speaks HTTP/1.1 only). The roots are the operating
//! system's, read when a client first connects ([`system`]), or a configured bundle's
//! ([`with_der`]); never a list built in.

use std::{fmt, sync::Arc};

/// Where [`system`] looks on Linux, from the caller's environment (rule 75).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use blaze_core::trust::Places;

/// Why the client's TLS can't be set up.
#[derive(Debug)]
pub enum TlsSetup {
    /// No root was given to check servers with.
    NoRoots,
    /// The machine's store gave no root, or would not read: every place looked.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Untrusted(blaze_core::trust::TrustError),
    /// rustls on ring refused its safe default TLS versions: why.
    Versions(String),
    /// The root at `at` of the list given was refused by rustls: why.
    BadRoot { at: usize, why: String },
}

impl fmt::Display for TlsSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsSetup::NoRoots => f.write_str("no root certificate was given to check servers with"),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            TlsSetup::Untrusted(error) => write!(f, "{error}"),
            TlsSetup::BadRoot { at, why } => write!(f, "root {at} of the trust anchors given is refused: {why}"),
            TlsSetup::Versions(why) => {
                write!(f, "rustls on ring has no safe default TLS versions: {why}")
            }
        }
    }
}

impl std::error::Error for TlsSetup {}

/// rustls on ring with the machine's roots, read now from the operating system's store
/// ([`blaze_core::trust::system`]: Security.framework opened at this call on macOS, the PEM
/// bundles `places` and the standard paths name on Linux). Call it when a client first
/// connects, never at start; a store that gives no root is [`TlsSetup::Untrusted`] naming every
/// place looked.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn system(places: blaze_core::trust::Places<'_>) -> Result<Arc<rustls::ClientConfig>, TlsSetup> {
    let roots = blaze_core::trust::system(places, std::alloc::Global).map_err(TlsSetup::Untrusted)?;
    with_der(roots.each())
}

/// rustls on ring checking servers against the roots whose DERs `ders` gives: the system's,
/// as `blaze_core::trust` read them on first use, or a configured bundle's. A DER rustls
/// cannot take is [`TlsSetup::BadRoot`] naming its place in the list, never skipped; none is
/// [`TlsSetup::NoRoots`].
pub fn with_der<'d>(ders: impl Iterator<Item = &'d [u8]>) -> Result<Arc<rustls::ClientConfig>, TlsSetup> {
    let mut roots = rustls::RootCertStore::empty();
    for (at, der) in ders.enumerate() {
        roots
            .add(rustls::pki_types::CertificateDer::from_slice(der))
            // alloc: an error's message, made only when it fails.
            .map_err(|e| TlsSetup::BadRoot { at, why: e.to_string() })?;
    }
    if roots.is_empty() {
        return Err(TlsSetup::NoRoots);
    }
    with_roots(roots)
}

/// An empty root store for plain-HTTP-only clients (such as LAN peer fetches) that must refuse
/// any accidental TLS handshake.
pub fn empty_roots() -> rustls::RootCertStore {
    rustls::RootCertStore::empty()
}

/// rustls on ring at its safe default versions, checking servers against `roots`, with no
/// client certificate and no ALPN.
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
