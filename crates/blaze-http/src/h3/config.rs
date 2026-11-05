//! What an [`Endpoint`](super::Endpoint) is made with: the certificate it serves, the hashes
//! it pins as a client, and the transport's limits; and their rustls and quinn-proto configs.
//!
//! A client here checks a server's certificate as a browser's `serverCertificateHashes` does
//! (the WebTransport API, 7.2): the SHA-256 of the end-entity certificate's DER is one of the
//! pinned hashes, its key is ECDSA P-256, its validity is at most 14 days, and now is inside
//! it. No name and no chain are checked; the hash is the trust.

#![blaze::sans_io]

use std::{sync::Arc, time::Duration};

use quinn_proto::{
    IdleTimeout, TransportConfig,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, WebPkiSupportedAlgorithms},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};

use super::{Error, Step};

/// The ALPN of HTTP/3.
pub const ALPN: &[u8] = b"h3";
/// The longest validity `serverCertificateHashes` takes.
pub const MOST_VALID: Duration = Duration::from_secs(14 * 86_400);

/// A certificate and its PKCS #8 private key, both DER.
#[derive(Clone)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("cert_der", &self.cert_der.len()).finish_non_exhaustive()
    }
}

/// The transport's limits, the same for every connection of an endpoint.
#[derive(Clone, Copy, Debug)]
pub struct Transport {
    /// A connection with nothing received for this long is closed (QUIC's idle timeout; the
    /// peer's, if shorter, wins).
    pub idle: Duration,
    /// How often a quiet connection is pinged, so a live one never idles out.
    pub keep_alive: Option<Duration>,
}

impl Default for Transport {
    /// wtransport's defaults, which the explorer used: 30 s idle, a ping every 5 s.
    fn default() -> Transport {
        Transport { idle: Duration::from_secs(30), keep_alive: Some(Duration::from_secs(5)) }
    }
}

/// How an endpoint serves.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub identity: Identity,
    pub transport: Transport,
}

/// How an endpoint connects.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// The SHA-256s of the server certificates it takes.
    pub pins: Vec<[u8; 32]>,
    pub transport: Transport,
}

fn config_error(why: String) -> Error {
    Error { peer: None, step: Step::Config, why }
}

fn provider() -> Arc<CryptoProvider> {
    // alloc: one provider per config, made as an endpoint starts or its certificate rotates.
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport(limits: &Transport) -> Result<Arc<TransportConfig>, Error> {
    let mut transport = TransportConfig::default();
    let idle = IdleTimeout::try_from(limits.idle)
        // alloc: an error's message, made only when it fails.
        .map_err(|_| config_error(format!("the idle timeout {:?} is past QUIC's", limits.idle)))?;
    transport.max_idle_timeout(Some(idle));
    transport.keep_alive_interval(limits.keep_alive);
    // alloc: one per config.
    Ok(Arc::new(transport))
}

/// The rustls server config of `identity`: TLS 1.3, ALPN `h3`, no client certificates. Also
/// the check that rustls takes the certificate and key at all.
pub fn server_tls(identity: &Identity) -> Result<rustls::ServerConfig, Error> {
    // alloc: an error's message, made only when it fails.
    let refused = |why: rustls::Error| {
        config_error(format!("rustls did not take the certificate and key: {why}"))
    };
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(refused)?
        .with_no_client_auth()
        .with_single_cert(
            // alloc: the certificate, copied once per config.
            vec![CertificateDer::from(identity.cert_der.clone())],
            // alloc: the key, copied once per config.
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone())),
        )
        .map_err(refused)?;
    // alloc: one ALPN list per config.
    tls.alpn_protocols = vec![ALPN.to_vec()];
    Ok(tls)
}

/// quinn-proto's server config of `config`.
pub(crate) fn server(config: &ServerConfig) -> Result<Arc<quinn_proto::ServerConfig>, Error> {
    let tls = server_tls(&config.identity)?;
    let quic = QuicServerConfig::try_from(tls)
        // alloc: an error's message, made only when it fails.
        .map_err(|why| config_error(format!("quinn-proto did not take the TLS config: {why}")))?;
    // alloc: one per config.
    let mut server = quinn_proto::ServerConfig::with_crypto(Arc::new(quic));
    server.transport_config(transport(&config.transport)?);
    // alloc: one per config.
    Ok(Arc::new(server))
}

/// quinn-proto's client config of `config`.
pub(crate) fn client(config: &ClientConfig) -> Result<quinn_proto::ClientConfig, Error> {
    let provider = provider();
    // alloc: one verifier per config.
    let verifier = Arc::new(Pinned {
        pins: config.pins.clone(),
        algorithms: provider.signature_verification_algorithms,
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        // alloc: an error's message, made only when it fails.
        .map_err(|why| config_error(format!("rustls refused TLS 1.3 with ring: {why}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    // alloc: one ALPN list per config.
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls)
        // alloc: an error's message, made only when it fails.
        .map_err(|why| config_error(format!("quinn-proto did not take the TLS config: {why}")))?;
    // alloc: one per config.
    let mut client = quinn_proto::ClientConfig::new(Arc::new(quic));
    client.transport_config(transport(&config.transport)?);
    Ok(client)
}

/// `serverCertificateHashes`' check, as rustls's verifier.
#[derive(Debug)]
struct Pinned {
    pins: Vec<[u8; 32]>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        check_pinned(end_entity.as_ref(), &self.pins, now.as_secs())
            .map_err(rustls::Error::General)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Whether `der` is a certificate `serverCertificateHashes` with `pins` takes at `now_s` (Unix
/// seconds), or why not.
pub fn check_pinned(der: &[u8], pins: &[[u8; 32]], now_s: u64) -> Result<(), String> {
    let hash = blaze_sha256::digest(der);
    if !pins.contains(&hash) {
        // alloc: an error's message, made only when it fails.
        let hex: String = hash.iter().map(|it| format!("{it:02x}")).collect();
        // alloc: an error's message, made only when it fails.
        return Err(format!(
            "the server's certificate (SHA-256 {hex}) is not one of the {} pinned",
            pins.len()
        ));
    }
    let facts = der::read(der).map_err(|why| {
        // alloc: an error's message, made only when it fails.
        format!("the server's certificate is not DER this client reads: {why}")
    })?;
    if !facts.p256 {
        // alloc: an error's message, made only when it fails.
        return Err("the server's certificate's key is not ECDSA P-256, which serverCertificateHashes needs".to_owned());
    }
    let span = facts.not_after_s.saturating_sub(facts.not_before_s);
    if span > MOST_VALID.as_secs() {
        // alloc: an error's message, made only when it fails.
        return Err(format!(
            "the server's certificate is valid {span} s, past the 14 days serverCertificateHashes takes"
        ));
    }
    if !(facts.not_before_s..=facts.not_after_s).contains(&now_s) {
        // alloc: an error's message, made only when it fails.
        return Err(format!(
            "the server's certificate is valid from {} to {} (Unix seconds), and now is {now_s}",
            facts.not_before_s, facts.not_after_s
        ));
    }
    Ok(())
}

/// Just enough DER (X.690) to read a certificate's validity and its key's algorithm.
mod der {
    /// What the pin check needs of a certificate.
    pub(super) struct Facts {
        pub(super) not_before_s: u64,
        pub(super) not_after_s: u64,
        pub(super) p256: bool,
    }

    const SEQUENCE: u8 = 0x30;
    const OID: u8 = 0x06;
    const UTC_TIME: u8 = 0x17;
    const GENERALIZED_TIME: u8 = 0x18;
    const VERSION: u8 = 0xa0;
    /// id-ecPublicKey (1.2.840.10045.2.1) and prime256v1 (1.2.840.10045.3.1.7).
    const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    const P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];

    /// The element at the start of `bytes`: its tag, its contents, and what follows it.
    fn element(bytes: &[u8]) -> Result<(u8, &[u8], &[u8]), &'static str> {
        let (&tag, rest) = bytes.split_first().ok_or("an element is cut off")?;
        let (&first, rest) = rest.split_first().ok_or("an element's length is cut off")?;
        let (len, rest) = if first < 0x80 {
            (usize::from(first), rest)
        } else {
            let n = usize::from(first & 0x7f);
            if n == 0 || n > 4 || rest.len() < n {
                return Err("an element's length is not one this reader takes");
            }
            (rest[..n].iter().fold(0usize, |acc, it| (acc << 8) | usize::from(*it)), &rest[n..])
        };
        if rest.len() < len {
            return Err("an element runs past its parent");
        }
        Ok((tag, &rest[..len], &rest[len..]))
    }

    fn expect<'a>(
        bytes: &'a [u8],
        tag: u8,
        what: &'static str,
    ) -> Result<(&'a [u8], &'a [u8]), &'static str> {
        let (got, contents, rest) = element(bytes)?;
        if got != tag {
            return Err(what);
        }
        Ok((contents, rest))
    }

    pub(super) fn read(cert: &[u8]) -> Result<Facts, &'static str> {
        let (cert, _) = expect(cert, SEQUENCE, "the certificate is not a SEQUENCE")?;
        let (tbs, _) = expect(cert, SEQUENCE, "the TBSCertificate is not a SEQUENCE")?;
        let mut rest = tbs;
        if rest.first() == Some(&VERSION) {
            rest = element(rest)?.2;
        }
        // The serial number, the signature's algorithm and the issuer.
        for _ in 0..3 {
            rest = element(rest)?.2;
        }
        let (validity, rest) = expect(rest, SEQUENCE, "the validity is not a SEQUENCE")?;
        let (before_tag, before, validity) = element(validity)?;
        let (after_tag, after, _) = element(validity)?;
        let not_before_s = time(before_tag, before)?;
        let not_after_s = time(after_tag, after)?;
        // The subject, then the subject's public key info.
        let rest = element(rest)?.2;
        let (spki, _) = expect(rest, SEQUENCE, "the subject's public key info is not a SEQUENCE")?;
        let (algorithm, _) = expect(spki, SEQUENCE, "the key's algorithm is not a SEQUENCE")?;
        let (oid, params) = expect(algorithm, OID, "the key's algorithm is not an OID")?;
        let p256 =
            oid == EC_PUBLIC_KEY && matches!(element(params), Ok((OID, curve, _)) if curve == P256);
        Ok(Facts { not_before_s, not_after_s, p256 })
    }

    /// A UTCTime or GeneralizedTime, `Z` only, as Unix seconds.
    fn time(tag: u8, text: &[u8]) -> Result<u64, &'static str> {
        let digits = |range: std::ops::Range<usize>| -> Result<u64, &'static str> {
            let part = text.get(range).ok_or("a time is cut off")?;
            part.iter().try_fold(0u64, |acc, it| {
                if it.is_ascii_digit() {
                    Ok(acc * 10 + u64::from(it - b'0'))
                } else {
                    Err("a time has a non-digit")
                }
            })
        };
        let (year, at) = match tag {
            UTC_TIME => {
                let yy = digits(0..2)?;
                (if yy < 50 { 2000 + yy } else { 1900 + yy }, 2)
            }
            GENERALIZED_TIME => (digits(0..4)?, 4),
            _ => return Err("a validity time is neither UTCTime nor GeneralizedTime"),
        };
        if text.len() != at + 11 || text[at + 10] != b'Z' {
            return Err("a validity time is not YYMMDDHHMMSSZ");
        }
        let (month, day) = (digits(at..at + 2)?, digits(at + 2..at + 4)?);
        let (hour, minute, second) =
            (digits(at + 4..at + 6)?, digits(at + 6..at + 8)?, digits(at + 8..at + 10)?);
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return Err("a validity time is out of range");
        }
        if year < 1970 {
            return Err("a validity time is before 1970");
        }
        Ok(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
    }

    /// Days from 1970-01-01 to the date, for years from 1970 (Howard Hinnant's algorithm).
    fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = y / 400;
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn times_read_as_unix_seconds() {
            assert_eq!(time(UTC_TIME, b"700101000000Z"), Ok(0));
            assert_eq!(time(UTC_TIME, b"261031180000Z"), Ok(1_793_469_600));
            assert_eq!(time(GENERALIZED_TIME, b"20261031180000Z"), Ok(1_793_469_600));
            assert_eq!(time(GENERALIZED_TIME, b"20000229000000Z"), Ok(951_782_400));
            assert!(time(UTC_TIME, b"261031180000+0100").is_err());
            assert!(time(UTC_TIME, b"261331180000Z").is_err());
        }
    }
}
