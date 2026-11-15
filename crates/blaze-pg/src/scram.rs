//! SCRAM-SHA-256 (RFC 5802, RFC 7677), the client's side, with the HMAC-SHA-256 (RFC 2104)
//! and PBKDF2 (RFC 8018) it is made of, over `blaze_sha256`.
//!
//! No channel binding: the GS2 header is `n,,` ("the client does not support it"), which is
//! true here, as this connection has no TLS. The user name in the client-first-message is the
//! one given; Postgres ignores it and takes the startup message's, so the connection sends it
//! empty as libpq does.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use blaze_sha256::{Sha256, digest};

use crate::error::AuthError;
use crate::proto::{DECIMAL, put_base64};

/// The most PBKDF2 iterations a server may ask for. Postgres asks 4096 by default
/// (`scram_iterations`); each is two SHA-256 compressions, so this bounds the work a server
/// can make the client do before it proves anything.
pub const ITERATIONS_MOST: u32 = 1 << 20;

/// The longest salt read, in bytes. Postgres's are 16.
pub const SALT_MOST: usize = 128;

/// SHA-256's block: HMAC pads its key to one (RFC 2104).
const BLOCK: usize = 64;

/// SHA-256's digest: a MAC, a salted password, a signature.
const DIGEST_BYTES: usize = 32;

/// HMAC's inner pad byte (RFC 2104).
const IPAD: u8 = 0x36;

/// HMAC's outer pad byte (RFC 2104).
const OPAD: u8 = 0x5c;

/// The first printable ASCII byte, the space: a password of printable ASCII is one SASLprep
/// leaves as it is.
const PRINTABLE_FIRST: u8 = 0x20;

/// The last printable ASCII byte, `~`, for the password and the nonce.
const PRINTABLE_LAST: u8 = 0x7e;

/// The first byte a nonce may hold: printable ASCII but the space (RFC 5802's `printable`
/// also leaves out `,`, which ends the attribute before this is checked).
const NONCE_FIRST: u8 = 0x21;

/// The most digits an iteration count is read with: `u32::MAX` has 10, and
/// [`ITERATIONS_MOST`] fewer, so a longer count is refused before it is summed.
const COUNT_DIGITS_MOST: usize = 10;

/// HMAC-SHA-256 with its key's inner and outer pads hashed once, so each MAC is the message's
/// blocks plus two.
#[derive(Clone)]
pub struct Hmac {
    inner: Sha256,
    outer: Sha256,
}

impl Hmac {
    pub fn new(key: &[u8]) -> Hmac {
        let mut block = [0u8; BLOCK];
        if key.len() > block.len() {
            let hashed = digest(key);
            block[..hashed.len()].copy_from_slice(&hashed);
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner_pad = [0u8; BLOCK];
        let mut outer_pad = [0u8; BLOCK];
        for ((byte, inner), outer) in block.iter().zip(&mut inner_pad).zip(&mut outer_pad) {
            *inner = byte ^ IPAD;
            *outer = byte ^ OPAD;
        }
        let mut inner = Sha256::new();
        inner.update(&inner_pad);
        let mut outer = Sha256::new();
        outer.update(&outer_pad);
        Hmac { inner, outer }
    }

    /// The MAC of `parts` joined.
    pub fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        for part in parts {
            inner.update(part);
        }
        let inner = inner.finish();
        let mut outer = self.outer.clone();
        outer.update(&inner);
        outer.finish()
    }
}

/// HMAC-SHA-256 of `data` under `key`.
pub fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    Hmac::new(key).mac(&[data])
}

/// PBKDF2-HMAC-SHA-256's first 32-byte block of `password` and `salt` over `iterations`
/// (at least 1; 0 is taken as 1): SCRAM's `Hi`, the salted password.
pub fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mac = Hmac::new(password);
    let mut u = mac.mac(&[salt, &1u32.to_be_bytes()]);
    let mut out = u;
    for _ in 1..iterations {
        u = mac.mac(&[&u]);
        for (o, b) in out.iter_mut().zip(&u) {
            *o ^= b;
        }
    }
    out
}

/// Whether `password` is printable ASCII, the passwords SASLprep leaves as they are: the byte
/// that is not, if one is not.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn unprintable(password: &[u8]) -> Option<usize> {
    password.iter().position(|b| !(PRINTABLE_FIRST..=PRINTABLE_LAST).contains(b))
}

/// Writes the client-first-message, `n,,n=<user>,r=<nonce>`, with `=` and `,` in the user
/// escaped (`=3D`, `=2C`). Its bare part (after `n,,`) is what [`client_final`] takes.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn client_first<A: Allocator>(user: &[u8], nonce: &[u8], out: &mut Vec<u8, A>) {
    out.extend_from_slice(b"n,,");
    put_bare(user, nonce, out);
}

/// The client-first-message-bare: `n=<user>,r=<nonce>`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn put_bare<A: Allocator>(user: &[u8], nonce: &[u8], out: &mut Vec<u8, A>) {
    out.extend_from_slice(b"n=");
    for &byte in user {
        match byte {
            b'=' => out.extend_from_slice(b"=3D"),
            b',' => out.extend_from_slice(b"=2C"),
            byte => out.push(byte),
        }
    }
    out.extend_from_slice(b",r=");
    out.extend_from_slice(nonce);
}

/// What the server-first-message says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerFirst<'a> {
    /// The whole nonce: the client's, then the server's.
    pub nonce: &'a [u8],
    pub salt: [u8; SALT_MOST],
    pub salt_len: usize,
    pub iterations: u32,
}

/// Reads `r=<nonce>,s=<salt>,i=<iterations>`. A mandatory extension (`m=`) or any attribute
/// after the count is refused: none is spoken here, and none is ignored.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn server_first<'a>(
    message: &'a [u8],
    client_nonce: &[u8],
) -> Result<ServerFirst<'a>, AuthError> {
    let mut attributes = message.split(|&b| b == b',');
    let mut attribute = |name: u8, what: &'static str| -> Result<&'a [u8], AuthError> {
        match attributes.next() {
            Some([got, b'=', value @ ..]) if *got == name => Ok(value),
            _ => Err(AuthError::ServerFirst { what }),
        }
    };
    let nonce = attribute(b'r', "does not start with its nonce (r=)")?;
    let salt_b64 = attribute(b's', "has no salt (s=) after its nonce")?;
    let count = attribute(b'i', "has no iteration count (i=) after its salt")?;
    if attributes.next().is_some() {
        return Err(AuthError::ServerFirst { what: "has an attribute after its iteration count" });
    }
    if nonce.len() <= client_nonce.len() || !nonce.starts_with(client_nonce) {
        return Err(AuthError::Nonce);
    }
    if nonce.iter().any(|b| !(NONCE_FIRST..=PRINTABLE_LAST).contains(b)) {
        return Err(AuthError::ServerFirst { what: "has a nonce that is not printable ASCII" });
    }
    let mut salt = [0u8; SALT_MOST];
    let salt_len = match STANDARD.decode_slice(salt_b64, &mut salt) {
        Ok(len) if len > 0 => len,
        Ok(_) => return Err(AuthError::ServerFirst { what: "has an empty salt" }),
        Err(_) => {
            return Err(AuthError::ServerFirst {
                what: "has a salt that is not base64 of at most 128 bytes",
            });
        }
    };
    if count.is_empty() || count.len() > COUNT_DIGITS_MOST || !count.iter().all(u8::is_ascii_digit)
    {
        return Err(AuthError::ServerFirst { what: "has an iteration count that is not a number" });
    }
    let iterations = count.iter().fold(0u64, |n, d| n * DECIMAL + u64::from(d - b'0'));
    if iterations == 0 || iterations > u64::from(ITERATIONS_MOST) {
        return Err(AuthError::Iterations { iterations });
    }
    // At most `ITERATIONS_MOST`, checked above, so this refuses nothing more.
    let Ok(iterations) = u32::try_from(iterations) else {
        return Err(AuthError::Iterations { iterations });
    };
    Ok(ServerFirst { nonce, salt, salt_len, iterations })
}

/// Writes the client-final-message, `c=biws,r=<nonce>,p=<proof>`, for `password` (already
/// checked printable), the client-first-message-bare, the server-first-message as it came and
/// our nonce. Returns the server signature the server-final-message must carry.
pub fn client_final<A: Allocator>(
    password: &[u8],
    client_first_bare: &[u8],
    server_first_message: &[u8],
    client_nonce: &[u8],
    out: &mut Vec<u8, A>,
) -> Result<[u8; 32], AuthError> {
    let first = server_first(server_first_message, client_nonce)?;
    let salt = first.salt.get(..first.salt_len).unwrap_or(&[]);
    let salted = pbkdf2(password, salt, first.iterations);
    let salted_mac = Hmac::new(&salted);
    let client_key = salted_mac.mac(&[b"Client Key"]);
    let stored_key = digest(&client_key);
    let server_key = salted_mac.mac(&[b"Server Key"]);

    let start = out.len();
    // `biws` is base64 of the GS2 header `n,,`.
    out.extend_from_slice(b"c=biws,r=");
    out.extend_from_slice(first.nonce);
    let without_proof = out.get(start..).unwrap_or(&[]);
    let auth_message: [&[u8]; 5] =
        [client_first_bare, b",", server_first_message, b",", without_proof];
    let client_signature = Hmac::new(&stored_key).mac(&auth_message);
    let server_signature = Hmac::new(&server_key).mac(&auth_message);
    let mut proof = client_key;
    for (p, s) in proof.iter_mut().zip(&client_signature) {
        *p ^= s;
    }
    out.extend_from_slice(b",p=");
    put_base64(out, &proof);
    Ok(server_signature)
}

/// Checks the server-final-message, `v=<signature>`, against the signature [`client_final`]
/// returned.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn server_final(message: &[u8], expected: &[u8; 32]) -> Result<(), AuthError> {
    let signature = match message {
        [b'e', b'=', ..] => return Err(AuthError::ServerRefused),
        [b'v', b'=', signature @ ..] => signature,
        _ => return Err(AuthError::ServerFinal { what: "has no verifier (v=)" }),
    };
    // Extensions after the verifier are refused like the server-first-message's.
    if signature.contains(&b',') {
        return Err(AuthError::ServerFinal { what: "has an attribute after its verifier" });
    }
    let mut got = [0u8; DIGEST_BYTES];
    match STANDARD.decode_slice(signature, &mut got) {
        Ok(DIGEST_BYTES) => {}
        _ => {
            return Err(AuthError::ServerFinal {
                what: "has a verifier that is not base64 of 32 bytes",
            });
        }
    }
    // Compared without an early exit, though the signature is not a secret the client holds.
    let differ = got.iter().zip(expected).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if differ != 0 {
        return Err(AuthError::Signature);
    }
    Ok(())
}
