//! The link `blaze explore` prints and opens, and who may use what it names.
//!
//! `https://ix.dev/explore#127.0.0.1:<port>/<token>/<certhash>`: the fragment never leaves the
//! browser, so ix.dev never sees the port, the token or the hash. The page connects to
//! `https://127.0.0.1:<port>/explore/v1` pinning the certificate by `certhash` (the SHA-256 of
//! its DER, base64url without padding), and its first request on the session is `hello` with
//! the token. The daemon accepts the session's CONNECT only from [`ORIGIN`] (and [`DEV_ORIGIN`]
//! when started for the page's development server), so another site that learned a link still
//! cannot open a session from a browser; the token keeps out a local process that is not a
//! browser, which sends any Origin it likes.

use base64::Engine as _;

/// The page.
pub const PAGE: &str = "https://ix.dev/explore";
/// The path of the session's CONNECT.
pub const PATH: &str = "/explore/v1";
/// The Origin the CONNECT must come from.
pub const ORIGIN: &str = "https://ix.dev";
/// The Origin of the page's development server (`vite dev` in indexable-inc/ix), accepted only
/// when the server is started with [`Origins::dev`].
pub const DEV_ORIGIN: &str = "http://localhost:5173";

/// A server's token: 16 bytes from the operating system's random source, as 32 lowercase hex
/// characters. New each time a server starts.
pub struct Token(String);

impl Token {
    pub fn random() -> Token {
        let mut raw = [0u8; 16];
        std::random::Rng::fill_bytes(&mut std::random::SystemRng, &mut raw);
        Token::from_bytes(raw)
    }

    pub fn from_bytes(raw: [u8; 16]) -> Token {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        // alloc: once a server
        let mut hex = String::with_capacity(32);
        for byte in raw {
            hex.push(HEX[usize::from(byte >> 4)] as char);
            hex.push(HEX[usize::from(byte & 0xf)] as char);
        }
        Token(hex)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `given` is this token, in time that depends only on its length.
    pub fn matches(&self, given: &str) -> bool {
        let (ours, given) = (self.0.as_bytes(), given.as_bytes());
        ours.len() == given.len() && ours.iter().zip(given).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// The hash the page pins a certificate by: the SHA-256 of its DER, base64url, no padding.
pub fn certhash(der: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blaze_sha256::digest(der))
}

/// The link to the explorer of the server on `port`.
pub fn link(port: u16, token: &Token, certhash: &str) -> String {
    format!("{PAGE}#127.0.0.1:{port}/{}/{certhash}", token.as_str())
}

/// The Origins a session may come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origins {
    pub dev: bool,
}

impl Origins {
    pub fn allows(self, origin: Option<&str>) -> bool {
        match origin {
            Some(ORIGIN) => true,
            Some(DEV_ORIGIN) => self.dev,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_32_lowercase_hex_and_new_each_time() {
        let token = Token::random();
        assert_eq!(token.as_str().len(), 32);
        assert!(token.as_str().bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(Token::random().as_str(), token.as_str());
        let fixed = Token::from_bytes([0x01, 0xab, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10]);
        assert_eq!(fixed.as_str(), "01abff00000000000000000000000010");
        assert_eq!(format!("{token:?}"), "Token(..)", "a token is never printed by Debug");
    }

    #[test]
    fn only_the_token_itself_matches() {
        let token = Token::from_bytes([7; 16]);
        assert!(token.matches("07070707070707070707070707070707"));
        assert!(!token.matches("07070707070707070707070707070708"));
        assert!(!token.matches("0707070707070707070707070707070"));
        assert!(!token.matches("070707070707070707070707070707070"));
        assert!(!token.matches(""));
        let lettered = Token::from_bytes([0xab; 16]);
        assert!(!lettered.matches(&lettered.as_str().to_uppercase()), "hex is lowercase only");
    }

    #[test]
    fn the_certhash_is_the_sha256_in_base64url_without_padding() {
        assert_eq!(certhash(b"abc"), "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0");
        assert_eq!(certhash(b""), "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU");
        assert_eq!(certhash(b"abc").len(), 43);
    }

    #[test]
    fn the_link_carries_port_token_and_hash_in_its_fragment() {
        let token = Token::from_bytes([0xa5; 16]);
        assert_eq!(
            link(4433, &token, "h"),
            "https://ix.dev/explore#127.0.0.1:4433/a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5/h"
        );
    }

    #[test]
    fn only_ix_dev_may_connect_and_the_dev_server_only_when_asked() {
        let (prod, dev) = (Origins { dev: false }, Origins { dev: true });
        assert!(prod.allows(Some("https://ix.dev")));
        assert!(!prod.allows(Some(DEV_ORIGIN)));
        assert!(dev.allows(Some(DEV_ORIGIN)) && dev.allows(Some(ORIGIN)));
        for refused in
            [None, Some("http://ix.dev"), Some("https://ix.dev.evil.example"), Some("https://ix.dev/"), Some("null"), Some("")]
        {
            assert!(!prod.allows(refused) && !dev.allows(refused), "{refused:?}");
        }
    }
}
