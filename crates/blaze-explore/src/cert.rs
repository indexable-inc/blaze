//! The certificate the page pins by hash, and when it is made anew.
//!
//! WebTransport's `serverCertificateHashes` takes a certificate only if it is ECDSA P-256, its
//! validity is at most 14 days and now is inside it. Ours is P-256, valid [`VALID_S`] (13 days)
//! from [`BACKDATE_S`] before it is made, and made anew once [`ROTATE_AFTER_S`] (12 days) of
//! that have passed: a day before it expires. [`Rotation`] is that rule alone, taking the time
//! as an argument, so a test runs it on a clock of its own; the server asks it every
//! `server::CHECK` (10 minutes) of the wall clock, so a machine that slept past the day is
//! caught within minutes of waking.
//!
//! What a session sees when the certificate is made anew: nothing breaks. QUIC checked the
//! certificate once, in the handshake, and an open session keeps its keys. The live stream
//! sends `{"op":"cert","certhash","not_after_unix_s"}`, and the page puts the new hash in its
//! link for its next connection; a page opened later from the old link gets the browser's
//! certificate error and asks for `blaze explore` again, which prints the current link.

use std::time::Duration;

use crate::link;

pub const DAY_S: u64 = 86_400;
/// How long a certificate is valid: under the 14 days WebTransport allows.
pub const VALID_S: u64 = 13 * DAY_S;
/// How long after its start a certificate is made anew: a day before it expires.
pub const ROTATE_AFTER_S: u64 = 12 * DAY_S;
/// How far before it is made a certificate's validity starts, so a browser whose clock is a
/// little behind the daemon's still takes it. The two are one machine's clock.
pub const BACKDATE_S: u64 = 60;

/// When a certificate is valid: seconds since the Unix epoch, `not_after_s` excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Validity {
    pub not_before_s: u64,
    pub not_after_s: u64,
}

impl Validity {
    /// The validity of a certificate made at `now_s`.
    pub fn starting(now_s: u64) -> Validity {
        let not_before_s = now_s.saturating_sub(BACKDATE_S);
        Validity { not_before_s, not_after_s: not_before_s + VALID_S }
    }

    pub fn contains(&self, now_s: u64) -> bool {
        (self.not_before_s..self.not_after_s).contains(&now_s)
    }
}

/// The rule of when the certificate is made anew.
#[derive(Clone, Copy, Debug)]
pub struct Rotation {
    current: Validity,
}

impl Rotation {
    /// The rule, for the certificate made at `now_s`.
    pub fn new(now_s: u64) -> Rotation {
        Rotation { current: Validity::starting(now_s) }
    }

    pub fn current(&self) -> Validity {
        self.current
    }

    /// The validity of the certificate to make now, when it is time: [`ROTATE_AFTER_S`] after
    /// the current one began, or the clock went back to before it began. Nothing changes until
    /// [`Rotation::rotated`] says it was made and served.
    pub fn due(&self, now_s: u64) -> Option<Validity> {
        let late = now_s >= self.current.not_before_s + ROTATE_AFTER_S;
        (late || now_s < self.current.not_before_s).then(|| Validity::starting(now_s))
    }

    /// The certificate of `validity` is served from now on.
    pub fn rotated(&mut self, validity: Validity) {
        self.current = validity;
    }

    /// How long from `now_s` until [`Rotation::due`] is next `Some`.
    pub fn wait(&self, now_s: u64) -> Duration {
        Duration::from_secs((self.current.not_before_s + ROTATE_AFTER_S).saturating_sub(now_s))
    }
}

/// Why a certificate could not be made.
#[derive(Debug)]
pub enum CertError {
    Make(rcgen::Error),
    /// wtransport did not take what rcgen made.
    Take(String),
}

impl std::fmt::Display for CertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertError::Make(why) => write!(f, "the explorer's certificate could not be made: {why}"),
            CertError::Take(why) => write!(f, "the explorer's certificate is not one WebTransport takes: {why}"),
        }
    }
}

impl std::error::Error for CertError {}

impl From<rcgen::Error> for CertError {
    fn from(why: rcgen::Error) -> CertError {
        CertError::Make(why)
    }
}

/// A self-signed certificate for 127.0.0.1 and its key.
pub struct Cert {
    pub der: Vec<u8>,
    key_der: Vec<u8>,
    pub validity: Validity,
    /// [`link::certhash`] of `der`.
    pub hash: String,
}

impl Cert {
    /// A certificate of `validity`: ECDSA P-256 with SHA-256, a key from the operating system's
    /// random source (ring's), for `127.0.0.1` and `localhost`.
    pub fn make(validity: Validity) -> Result<Cert, CertError> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        // alloc: once a certificate, every 12 days
        let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])?;
        let mut name = rcgen::DistinguishedName::new();
        name.push(rcgen::DnType::CommonName, "blaze explore");
        params.distinguished_name = name;
        let epoch = rcgen::date_time_ymd(1970, 1, 1);
        params.not_before = epoch + Duration::from_secs(validity.not_before_s);
        params.not_after = epoch + Duration::from_secs(validity.not_after_s);
        let cert = params.self_signed(&key)?;
        // alloc: once a certificate
        let der = cert.der().to_vec();
        let hash = link::certhash(&der);
        Ok(Cert { der, key_der: key.serialize_der(), validity, hash })
    }

    /// The certificate and key as wtransport serves them.
    pub fn identity(&self) -> Result<wtransport::Identity, CertError> {
        use wtransport::tls::{Certificate, CertificateChain, PrivateKey};
        // clone: once a certificate, every 12 days
        let certificate = Certificate::from_der(self.der.clone()).map_err(|why| CertError::Take(why.to_string()))?;
        // clone: as above
        let key = PrivateKey::from_der_pkcs8(self.key_der.clone());
        Ok(wtransport::Identity::new(CertificateChain::single(certificate), key))
    }
}

impl std::fmt::Debug for Cert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cert").field("validity", &self.validity).field("hash", &self.hash).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-31 18:00 UTC.
    const T0: u64 = 1_793_469_600;

    #[test]
    fn a_certificate_is_valid_13_days_from_a_minute_before_it_was_made() {
        let validity = Validity::starting(T0);
        assert_eq!(validity.not_before_s, T0 - BACKDATE_S);
        assert_eq!(validity.not_after_s - validity.not_before_s, 13 * DAY_S);
        assert!(validity.contains(T0) && !validity.contains(validity.not_after_s));
    }

    /// Fifty days on a clock of the test's own, read every 10 minutes as the server does: the
    /// certificate served is always valid now, never longer than WebTransport allows, and made
    /// anew a day before it expires, every 12 days.
    #[test]
    fn the_certificate_is_made_anew_a_day_before_it_expires() {
        let step = 600;
        let mut rotation = Rotation::new(T0);
        let mut made = vec![T0];
        let mut now = T0;
        while now < T0 + 50 * DAY_S {
            now += step;
            if let Some(validity) = rotation.due(now) {
                let left = rotation.current().not_after_s.saturating_sub(now);
                assert!(left >= DAY_S - BACKDATE_S - step, "made anew {left} s before the old one expired");
                assert!(validity.contains(now));
                rotation.rotated(validity);
                made.push(now);
            }
            let current = rotation.current();
            assert!(current.contains(now), "the certificate served at {now} is valid then");
            assert!(current.not_after_s - current.not_before_s <= 14 * DAY_S);
        }
        assert_eq!(made.len(), 5, "made at the start and on days 12, 24, 36 and 48: {made:?}");
        for pair in made.windows(2) {
            let gap = pair[1] - pair[0];
            assert!((ROTATE_AFTER_S - BACKDATE_S..ROTATE_AFTER_S - BACKDATE_S + step).contains(&gap), "{gap}");
        }
    }

    #[test]
    fn a_rotation_not_made_is_due_again() {
        let rotation = Rotation::new(T0);
        let late = T0 + ROTATE_AFTER_S;
        assert!(rotation.due(late).is_some());
        assert!(rotation.due(late + 600).is_some(), "until `rotated`, it stays due");
        assert_eq!(rotation.wait(T0), Duration::from_secs(ROTATE_AFTER_S - BACKDATE_S));
        assert_eq!(rotation.wait(late), Duration::ZERO);
    }

    #[test]
    fn a_clock_that_jumps_is_caught_at_the_next_check() {
        let mut rotation = Rotation::new(T0);
        let woke = T0 + 20 * DAY_S;
        let validity = rotation.due(woke).expect("a machine that slept 20 days needs a new certificate");
        rotation.rotated(validity);
        assert!(rotation.current().contains(woke));
        let back = woke - 3600;
        assert!(rotation.due(back).is_some(), "a clock set back before the certificate began");
        assert!(rotation.due(woke + 600).is_none());
    }

    #[test]
    fn a_certificate_is_p256_der_wtransport_takes_and_new_each_time() {
        let validity = Validity::starting(T0);
        let first = Cert::make(validity).unwrap();
        let second = Cert::make(validity).unwrap();
        assert_ne!(first.hash, second.hash, "a fresh key each time");
        assert_eq!(first.hash, link::certhash(&first.der));
        first.identity().unwrap();
        // The OID of id-ecPublicKey and of the P-256 curve, in the subject's public key info.
        let ec_public_key = [0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
        let p256 = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        let has = |needle: &[u8]| first.der.windows(needle.len()).any(|it| it == needle);
        assert!(has(&ec_public_key) && has(&p256));
    }
}
