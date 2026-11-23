//! Pure `#![no_std]` RFC 7519 JSON Web Tokens (JWT) and RFC 7515 JSON Web Signatures (JWS).
//!
//! - Supports `HS256` (HMAC-SHA256 via [`crate::digest::hmac_sha256`] and constant-time
//!   [`crate::digest::ct_eq`]) and `EdDSA` (Ed25519 verification via [`crate::crypto::ed25519`]).
//! - Strictly refuses `alg: "none"`, algorithm confusion, duplicate JSON keys, non-canonical
//!   base64url padding/trailing bits, oversized tokens, and non-object headers or claims.
//! - Zero ambient clock access: callers explicitly pass `now_unix_secs` in [`VerifyOpts`].
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_base64::URL_SAFE_NO_PAD;
use blaze_data::{Build, Map, Value};
use blaze_json::{Duplicates, Event, Limits as JsonLimits, Reader as JsonReader, WriteOptions, to_string_in};
use blaze_own::{AllocFailure, String, Vec};

use crate::crypto::ed25519;
use crate::digest::{SHA256_LEN, ct_eq, hmac_sha256};

const ED25519_SIG_LEN: usize = 64;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_NUMBER_LEN: usize = 100;

/// Supported JWS signature algorithms (RFC 7518 / RFC 8037).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alg {
    /// HMAC using SHA-256 (`"HS256"`).
    Hs256,
    /// EdDSA using Ed25519 (`"EdDSA"`).
    EdDsa,
}

impl Alg {
    /// Canonical JWS `"alg"` header string.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Alg::Hs256 => "HS256",
            Alg::EdDsa => "EdDSA",
        }
    }

    /// Parse a canonical JWS `"alg"` header value.
    #[inline]
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "HS256" => Some(Alg::Hs256),
            "EdDSA" => Some(Alg::EdDsa),
            _ => None,
        }
    }
}

impl fmt::Display for Alg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Bounds enforced when decoding or verifying untrusted JWT text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JwtLimits {
    /// Maximum encoded compact JWS length in bytes.
    pub most_token_bytes: usize,
    /// Maximum JSON nodes parsed across a header or claims object.
    pub most_json_nodes: usize,
}

impl JwtLimits {
    /// Generous default bounds (64 KiB encoded token, 4,096 JSON nodes).
    pub const USUAL: Self = Self {
        most_token_bytes: 64 * 1024,
        most_json_nodes: 4_096,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Explicit verification policy; carries no ambient clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VerifyOpts {
    /// Expected signature algorithm; tokens declaring any other algorithm are refused.
    pub alg: Alg,
    /// Current Unix timestamp in seconds for `exp` / `nbf` checks (`None` skips time checks).
    pub now_unix_secs: Option<i64>,
    /// Clock-skew tolerance in seconds applied to `exp` and `nbf` checks.
    pub leeway_secs: u64,
    /// When `true`, tokens without an integer `exp` claim are refused with [`JwtError::MissingExp`].
    pub require_exp: bool,
    /// Size and JSON node bounds enforced on the token.
    pub limits: JwtLimits,
}

/// A cryptographically verified JWT with its decoded header and claims objects.
pub struct VerifiedToken<A: Allocator> {
    /// Verified signature algorithm.
    pub alg: Alg,
    /// Optional `"kid"` key identifier from the JOSE header.
    pub kid: Option<String<A>>,
    /// Decoded JOSE header JSON object.
    pub header: Value<A>,
    /// Decoded JWT claims set JSON object.
    pub claims: Value<A>,
}

impl<A: Allocator> fmt::Debug for VerifiedToken<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedToken")
            .field("alg", &self.alg)
            .field("kid", &self.kid.as_ref().map(String::as_str))
            .field("header", &self.header)
            .field("claims", &self.claims)
            .finish()
    }
}

impl<A: Allocator> PartialEq for VerifiedToken<A> {
    fn eq(&self, other: &Self) -> bool {
        let kid_eq = match (&self.kid, &other.kid) {
            (Some(a), Some(b)) => a.as_str() == b.as_str(),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        };
        self.alg == other.alg
            && kid_eq
            && self.header == other.header
            && self.claims == other.claims
    }
}

impl<A: Allocator> Eq for VerifiedToken<A> {}

impl<A: Allocator> VerifiedToken<A> {
    /// Subject (`"sub"`) claim if present as a JSON string.
    #[inline]
    #[must_use]
    pub fn sub(&self) -> Option<&str> {
        self.claims.get("sub").and_then(Value::as_str)
    }

    /// Issuer (`"iss"`) claim if present as a JSON string.
    #[inline]
    #[must_use]
    pub fn iss(&self) -> Option<&str> {
        self.claims.get("iss").and_then(Value::as_str)
    }

    /// JWT ID (`"jti"`) claim if present as a JSON string.
    #[inline]
    #[must_use]
    pub fn jti(&self) -> Option<&str> {
        self.claims.get("jti").and_then(Value::as_str)
    }

    /// Expiration time (`"exp"`) claim in Unix seconds if present as an `i64`.
    #[inline]
    #[must_use]
    pub fn exp(&self) -> Option<i64> {
        self.claims.get("exp").and_then(Value::as_i64)
    }

    /// Not-before time (`"nbf"`) claim in Unix seconds if present as an `i64`.
    #[inline]
    #[must_use]
    pub fn nbf(&self) -> Option<i64> {
        self.claims.get("nbf").and_then(Value::as_i64)
    }

    /// Issued-at time (`"iat"`) claim in Unix seconds if present as an `i64`.
    #[inline]
    #[must_use]
    pub fn iat(&self) -> Option<i64> {
        self.claims.get("iat").and_then(Value::as_i64)
    }
}

/// A structurally decoded JWT whose signature has **not** been verified.
///
/// Use this only to inspect routing metadata such as `"kid"` before looking up a verification
/// key and calling [`verify_hs256_in`] or [`verify_eddsa_in`].
pub struct UnverifiedToken<A: Allocator> {
    /// Recognized JWS algorithm from the JOSE header, or `None` if unrecognized.
    pub alg: Option<Alg>,
    /// Optional `"kid"` key identifier from the JOSE header.
    pub kid: Option<String<A>>,
    /// Decoded JOSE header JSON object.
    pub header: Value<A>,
    /// Decoded unverified JWT claims set JSON object.
    pub claims: Value<A>,
}

impl<A: Allocator> fmt::Debug for UnverifiedToken<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnverifiedToken")
            .field("alg", &self.alg)
            .field("kid", &self.kid.as_ref().map(String::as_str))
            .field("header", &self.header)
            .field("claims", &self.claims)
            .finish()
    }
}

impl<A: Allocator> PartialEq for UnverifiedToken<A> {
    fn eq(&self, other: &Self) -> bool {
        let kid_eq = match (&self.kid, &other.kid) {
            (Some(a), Some(b)) => a.as_str() == b.as_str(),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        };
        self.alg == other.alg
            && kid_eq
            && self.header == other.header
            && self.claims == other.claims
    }
}

impl<A: Allocator> Eq for UnverifiedToken<A> {}

impl<A: Allocator> UnverifiedToken<A> {
    /// Raw `"alg"` string from the JOSE header.
    #[inline]
    #[must_use]
    pub fn alg_str(&self) -> Option<&str> {
        self.header.get("alg").and_then(Value::as_str)
    }

    /// Subject (`"sub"`) claim if present as a JSON string.
    #[inline]
    #[must_use]
    pub fn sub(&self) -> Option<&str> {
        self.claims.get("sub").and_then(Value::as_str)
    }

    /// Issuer (`"iss"`) claim if present as a JSON string.
    #[inline]
    #[must_use]
    pub fn iss(&self) -> Option<&str> {
        self.claims.get("iss").and_then(Value::as_str)
    }

    /// JWT ID (`"jti"`) claim if present as a JSON string.
    #[inline]
    #[must_use]
    pub fn jti(&self) -> Option<&str> {
        self.claims.get("jti").and_then(Value::as_str)
    }

    /// Expiration time (`"exp"`) claim in Unix seconds if present as an `i64`.
    #[inline]
    #[must_use]
    pub fn exp(&self) -> Option<i64> {
        self.claims.get("exp").and_then(Value::as_i64)
    }

    /// Not-before time (`"nbf"`) claim in Unix seconds if present as an `i64`.
    #[inline]
    #[must_use]
    pub fn nbf(&self) -> Option<i64> {
        self.claims.get("nbf").and_then(Value::as_i64)
    }

    /// Issued-at time (`"iat"`) claim in Unix seconds if present as an `i64`.
    #[inline]
    #[must_use]
    pub fn iat(&self) -> Option<i64> {
        self.claims.get("iat").and_then(Value::as_i64)
    }
}

/// Typed failure returned by JWT signing, decoding, or verification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JwtError {
    /// Encoded token length or JSON node count exceeded [`JwtLimits`].
    TokenTooLarge,
    /// Token did not consist of three dot-separated JWS compact serialization segments.
    MalformedCompact,
    /// A JWS segment was not canonical unpadded base64url (RFC 4648 §5).
    InvalidBase64,
    /// A decoded header or payload was not valid strict JSON, or claims could not be encoded as JSON.
    InvalidJson,
    /// Decoded JOSE header JSON was not a JSON object.
    HeaderNotObject,
    /// Decoded or supplied JWT claims JSON was not a JSON object.
    ClaimsNotObject,
    /// JOSE header did not include an `"alg"` member.
    MissingAlg,
    /// JOSE header specified an unsupported or unsafe algorithm (including `"none"`).
    UnsupportedAlg,
    /// Token algorithm or verification options did not match the required algorithm.
    AlgMismatch { expected: Alg, actual: Alg },
    /// Cryptographic MAC or Ed25519 signature verification failed.
    InvalidSignature,
    /// Token `"exp"` claim was at or before `now` (after applying `leeway_secs`).
    Expired { exp: i64, now: i64 },
    /// Token `"nbf"` claim was after `now` (after applying `leeway_secs`).
    NotYetValid { nbf: i64, now: i64 },
    /// Verification required an `"exp"` claim, but none was present.
    MissingExp,
    /// A registered header or claim field had an unexpected JSON value type.
    InvalidClaimType { claim: &'static str },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for JwtError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        Self::Alloc(err)
    }
}

impl fmt::Display for JwtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            JwtError::TokenTooLarge => {
                f.write_str("bstd::jwt: token exceeds configured size or JSON node limit")
            }
            JwtError::MalformedCompact => {
                f.write_str("bstd::jwt: malformed compact JWS serialization (expected header.payload.signature)")
            }
            JwtError::InvalidBase64 => {
                f.write_str("bstd::jwt: invalid canonical unpadded base64url segment")
            }
            JwtError::InvalidJson => f.write_str("bstd::jwt: invalid JSON in header or claims"),
            JwtError::HeaderNotObject => f.write_str("bstd::jwt: JOSE header must be a JSON object"),
            JwtError::ClaimsNotObject => f.write_str("bstd::jwt: JWT claims must be a JSON object"),
            JwtError::MissingAlg => f.write_str("bstd::jwt: JOSE header missing required `alg` field"),
            JwtError::UnsupportedAlg => {
                f.write_str("bstd::jwt: unsupported or disallowed JWS algorithm")
            }
            JwtError::AlgMismatch { expected, actual } => write!(
                f,
                "bstd::jwt: algorithm mismatch (expected {expected}, got {actual})"
            ),
            JwtError::InvalidSignature => f.write_str("bstd::jwt: signature verification failed"),
            JwtError::Expired { exp, now } => {
                write!(f, "bstd::jwt: token expired at unix time {exp} (now {now})")
            }
            JwtError::NotYetValid { nbf, now } => {
                write!(f, "bstd::jwt: token not valid before unix time {nbf} (now {now})")
            }
            JwtError::MissingExp => f.write_str("bstd::jwt: missing required `exp` claim"),
            JwtError::InvalidClaimType { claim } => {
                write!(f, "bstd::jwt: invalid JSON type for registered claim `{claim}`")
            }
            JwtError::Alloc(err) => write!(f, "bstd::jwt: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for JwtError {}

/// Sign a JSON object `claims` using `HS256` (HMAC-SHA256) into a compact JWS string in `alloc`.
///
/// The JOSE header always includes `"alg":"HS256"` and `"typ":"JWT"`, plus `"kid"` when supplied.
/// Keys are serialized deterministically in sorted key order.
pub fn sign_hs256_in<A: Allocator + Copy>(
    claims: &Value<A>,
    kid: Option<&str>,
    secret: &[u8],
    alloc: A,
) -> Result<String<A>, JwtError> {
    if claims.as_map().is_none() {
        return Err(JwtError::ClaimsNotObject);
    }

    let mut header_map = Map::new_in(alloc);
    header_map.insert(
        String::try_from_str_in("alg", alloc)?,
        Value::Text(String::try_from_str_in(Alg::Hs256.as_str(), alloc)?),
    );
    header_map.insert(
        String::try_from_str_in("typ", alloc)?,
        Value::Text(String::try_from_str_in("JWT", alloc)?),
    );
    if let Some(kid_str) = kid {
        header_map.insert(
            String::try_from_str_in("kid", alloc)?,
            Value::Text(String::try_from_str_in(kid_str, alloc)?),
        );
    }
    let header_value = Value::Map(header_map);

    let header_json = match to_string_in(&header_value, WriteOptions::COMPACT, alloc) {
        Ok(text) => text,
        Err(_) => return Err(JwtError::InvalidJson),
    };
    let claims_json = match to_string_in(claims, WriteOptions::COMPACT, alloc) {
        Ok(text) => text,
        Err(_) => return Err(JwtError::InvalidJson),
    };

    let header_b64_len = match URL_SAFE_NO_PAD.encoded_len(header_json.len()) {
        Some(n) => n,
        None => return Err(JwtError::TokenTooLarge),
    };
    let claims_b64_len = match URL_SAFE_NO_PAD.encoded_len(claims_json.len()) {
        Some(n) => n,
        None => return Err(JwtError::TokenTooLarge),
    };
    let sig_b64_len = match URL_SAFE_NO_PAD.encoded_len(SHA256_LEN) {
        Some(n) => n,
        None => return Err(JwtError::TokenTooLarge),
    };
    let total_len = match header_b64_len
        .checked_add(1)
        .and_then(|n| n.checked_add(claims_b64_len))
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_add(sig_b64_len))
    {
        Some(n) => n,
        None => return Err(JwtError::TokenTooLarge),
    };

    let mut out = String::try_with_capacity_in(total_len, alloc)?;
    b64url_encode_append(header_json.as_bytes(), &mut out)?;
    out.try_push('.')?;
    b64url_encode_append(claims_json.as_bytes(), &mut out)?;

    let mac = hmac_sha256(secret, out.as_bytes());
    out.try_push('.')?;
    b64url_encode_append(&mac, &mut out)?;

    Ok(out)
}

/// Verify an `HS256` compact JWT using `secret` and constant-time MAC comparison.
///
/// Enforces `opts.alg == Alg::Hs256`, refuses `"none"` and algorithm confusion, verifies the
/// 32-byte HMAC-SHA256 signature before trusting claims, and validates `exp`/`nbf`/`iat`.
pub fn verify_hs256_in<A: Allocator + Copy>(
    token: &str,
    secret: &[u8],
    opts: VerifyOpts,
    alloc: A,
) -> Result<VerifiedToken<A>, JwtError> {
    verify_hs256_in_planted(token, secret, opts, alloc, false, false)
}

fn verify_hs256_in_planted<A: Allocator + Copy>(
    token: &str,
    secret: &[u8],
    opts: VerifyOpts,
    alloc: A,
    plant_skip_sig: bool,
    plant_skip_exp: bool,
) -> Result<VerifiedToken<A>, JwtError> {
    if opts.alg != Alg::Hs256 {
        return Err(JwtError::AlgMismatch { expected: Alg::Hs256, actual: opts.alg });
    }
    if token.len() > opts.limits.most_token_bytes {
        return Err(JwtError::TokenTooLarge);
    }

    let (signing_input, header_b64, payload_b64, sig_b64) = split_compact(token)?;
    let (header, header_alg, kid) = parse_verified_header_in(header_b64, opts.alg, opts.limits, alloc)?;

    let sig_bytes = b64url_decode_in(sig_b64.as_bytes(), alloc)?;
    let sig_arr: [u8; SHA256_LEN] = match <[u8; SHA256_LEN]>::try_from(sig_bytes.as_slice()) {
        Ok(arr) => arr,
        Err(_) => return Err(JwtError::InvalidSignature),
    };

    if !plant_skip_sig {
        let expected_mac = hmac_sha256(secret, signing_input.as_bytes());
        if !ct_eq(&expected_mac, &sig_arr) {
            return Err(JwtError::InvalidSignature);
        }
    }

    let payload_bytes = b64url_decode_in(payload_b64.as_bytes(), alloc)?;
    let claims = parse_json_value_in(payload_bytes.as_slice(), opts.limits, alloc)?;
    validate_claims(&claims, opts, plant_skip_exp)?;

    Ok(VerifiedToken { alg: header_alg, kid, header, claims })
}

/// Verify an `EdDSA` (Ed25519) compact JWT using `public_key`.
///
/// Enforces `opts.alg == Alg::EdDsa`, refuses `"none"` and algorithm confusion, verifies the
/// 64-byte Ed25519 signature via [`crate::crypto::ed25519::verify`], and validates `exp`/`nbf`/`iat`.
pub fn verify_eddsa_in<A: Allocator + Copy>(
    token: &str,
    public_key: &[u8; 32],
    opts: VerifyOpts,
    alloc: A,
) -> Result<VerifiedToken<A>, JwtError> {
    if opts.alg != Alg::EdDsa {
        return Err(JwtError::AlgMismatch { expected: Alg::EdDsa, actual: opts.alg });
    }
    if token.len() > opts.limits.most_token_bytes {
        return Err(JwtError::TokenTooLarge);
    }

    let (signing_input, header_b64, payload_b64, sig_b64) = split_compact(token)?;
    let (header, header_alg, kid) = parse_verified_header_in(header_b64, opts.alg, opts.limits, alloc)?;

    let sig_bytes = b64url_decode_in(sig_b64.as_bytes(), alloc)?;
    let sig_arr: [u8; ED25519_SIG_LEN] = match <[u8; ED25519_SIG_LEN]>::try_from(sig_bytes.as_slice()) {
        Ok(arr) => arr,
        Err(_) => return Err(JwtError::InvalidSignature),
    };

    if ed25519::verify(public_key, signing_input.as_bytes(), &sig_arr).is_err() {
        return Err(JwtError::InvalidSignature);
    }

    let payload_bytes = b64url_decode_in(payload_b64.as_bytes(), alloc)?;
    let claims = parse_json_value_in(payload_bytes.as_slice(), opts.limits, alloc)?;
    validate_claims(&claims, opts, false)?;

    Ok(VerifiedToken { alg: header_alg, kid, header, claims })
}

/// Decode a compact JWT's header and claims **without** verifying its signature.
///
/// Intended solely for inspecting routing fields such as `kid` before looking up a verification
/// key and calling [`verify_hs256_in`] or [`verify_eddsa_in`]. Never trust the returned claims
/// for authorization decisions.
pub fn decode_unverified_in<A: Allocator + Copy>(
    token: &str,
    limits: JwtLimits,
    alloc: A,
) -> Result<UnverifiedToken<A>, JwtError> {
    if token.len() > limits.most_token_bytes {
        return Err(JwtError::TokenTooLarge);
    }

    let (_signing_input, header_b64, payload_b64, sig_b64) = split_compact(token)?;

    // Validate signature segment base64url syntax even when unverified.
    let _sig_bytes = b64url_decode_in(sig_b64.as_bytes(), alloc)?;

    let header_bytes = b64url_decode_in(header_b64.as_bytes(), alloc)?;
    let header = parse_json_value_in(header_bytes.as_slice(), limits, alloc)?;
    let Some(header_map) = header.as_map() else {
        return Err(JwtError::HeaderNotObject);
    };

    let alg = match header_map.get("alg") {
        None => return Err(JwtError::MissingAlg),
        Some(val) => match val.as_str() {
            Some(s) => Alg::parse(s),
            None => return Err(JwtError::InvalidClaimType { claim: "alg" }),
        },
    };
    let kid = extract_kid_in(header_map, alloc)?;

    let payload_bytes = b64url_decode_in(payload_b64.as_bytes(), alloc)?;
    let claims = parse_json_value_in(payload_bytes.as_slice(), limits, alloc)?;
    if claims.as_map().is_none() {
        return Err(JwtError::ClaimsNotObject);
    }

    Ok(UnverifiedToken { alg, kid, header, claims })
}

fn split_compact(token: &str) -> Result<(&str, &str, &str, &str), JwtError> {
    let bytes = token.as_bytes();
    let mut dot1: Option<usize> = None;
    let mut dot2: Option<usize> = None;
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'.' {
            if dot1.is_none() {
                dot1 = Some(idx);
            } else if dot2.is_none() {
                dot2 = Some(idx);
            } else {
                return Err(JwtError::MalformedCompact);
            }
        }
    }
    let (Some(d1), Some(d2)) = (dot1, dot2) else {
        return Err(JwtError::MalformedCompact);
    };
    let Some((signing_input, rest_after_d2)) = token.split_at_checked(d2) else {
        return Err(JwtError::MalformedCompact);
    };
    let Some(sig_b64) = rest_after_d2.strip_prefix('.') else {
        return Err(JwtError::MalformedCompact);
    };
    let Some((header_b64, rest_after_d1)) = signing_input.split_at_checked(d1) else {
        return Err(JwtError::MalformedCompact);
    };
    let Some(payload_b64) = rest_after_d1.strip_prefix('.') else {
        return Err(JwtError::MalformedCompact);
    };
    if header_b64.is_empty() || payload_b64.is_empty() {
        return Err(JwtError::MalformedCompact);
    }
    Ok((signing_input, header_b64, payload_b64, sig_b64))
}

fn parse_verified_header_in<A: Allocator + Copy>(
    header_b64: &str,
    expected_alg: Alg,
    limits: JwtLimits,
    alloc: A,
) -> Result<(Value<A>, Alg, Option<String<A>>), JwtError> {
    let header_bytes = b64url_decode_in(header_b64.as_bytes(), alloc)?;
    let header = parse_json_value_in(header_bytes.as_slice(), limits, alloc)?;
    let Some(header_map) = header.as_map() else {
        return Err(JwtError::HeaderNotObject);
    };

    let alg_str = match header_map.get("alg") {
        None => return Err(JwtError::MissingAlg),
        Some(val) => match val.as_str() {
            Some(s) => s,
            None => return Err(JwtError::InvalidClaimType { claim: "alg" }),
        },
    };
    let Some(actual_alg) = Alg::parse(alg_str) else {
        return Err(JwtError::UnsupportedAlg);
    };
    if actual_alg != expected_alg {
        return Err(JwtError::AlgMismatch { expected: expected_alg, actual: actual_alg });
    }

    let kid = extract_kid_in(header_map, alloc)?;
    Ok((header, actual_alg, kid))
}

fn extract_kid_in<A: Allocator + Copy>(
    header_map: &Map<A>,
    alloc: A,
) -> Result<Option<String<A>>, JwtError> {
    match header_map.get("kid") {
        None => Ok(None),
        Some(val) => match val.as_str() {
            Some(s) => Ok(Some(String::try_from_str_in(s, alloc)?)),
            None => Err(JwtError::InvalidClaimType { claim: "kid" }),
        },
    }
}

fn validate_claims<A: Allocator>(
    claims: &Value<A>,
    opts: VerifyOpts,
    plant_skip_exp: bool,
) -> Result<(), JwtError> {
    let Some(map) = claims.as_map() else {
        return Err(JwtError::ClaimsNotObject);
    };

    let exp = match map.get("exp") {
        None => None,
        Some(val) => match val.as_i64() {
            Some(v) => Some(v),
            None => return Err(JwtError::InvalidClaimType { claim: "exp" }),
        },
    };
    let nbf = match map.get("nbf") {
        None => None,
        Some(val) => match val.as_i64() {
            Some(v) => Some(v),
            None => return Err(JwtError::InvalidClaimType { claim: "nbf" }),
        },
    };
    if let Some(val) = map.get("iat")
        && val.as_i64().is_none()
    {
        return Err(JwtError::InvalidClaimType { claim: "iat" });
    }

    if opts.require_exp && exp.is_none() {
        return Err(JwtError::MissingExp);
    }

    if let Some(now) = opts.now_unix_secs {
        let now_128 = i128::from(now);
        let leeway_128 = i128::from(opts.leeway_secs);

        if !plant_skip_exp
            && let Some(exp_ts) = exp
            && now_128 >= i128::from(exp_ts) + leeway_128
        {
            return Err(JwtError::Expired { exp: exp_ts, now });
        }

        if let Some(nbf_ts) = nbf
            && now_128 + leeway_128 < i128::from(nbf_ts)
        {
            return Err(JwtError::NotYetValid { nbf: nbf_ts, now });
        }
    }

    Ok(())
}

fn parse_json_value_in<A: Allocator + Copy>(
    bytes: &[u8],
    limits: JwtLimits,
    alloc: A,
) -> Result<Value<A>, JwtError> {
    if bytes.len() > limits.most_token_bytes {
        return Err(JwtError::TokenTooLarge);
    }
    let json_limits = JsonLimits {
        most_bytes: limits.most_token_bytes.max(1),
        most_depth: MAX_JSON_DEPTH,
        most_number_len: MAX_JSON_NUMBER_LEN,
        duplicates: Duplicates::Refuse,
    };
    let mut reader = match JsonReader::new_in(bytes, json_limits, alloc) {
        Ok(r) => r,
        Err(_) => return Err(JwtError::InvalidJson),
    };
    let mut build = Build::new_in(alloc, alloc);
    let mut nodes = 0usize;

    loop {
        let item = match reader.next() {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(_) => return Err(JwtError::InvalidJson),
        };
        match item.event {
            Event::Null => {
                nodes = nodes.saturating_add(1);
                if nodes > limits.most_json_nodes {
                    return Err(JwtError::TokenTooLarge);
                }
                build.value(Value::Null);
            }
            Event::Bool(b) => {
                nodes = nodes.saturating_add(1);
                if nodes > limits.most_json_nodes {
                    return Err(JwtError::TokenTooLarge);
                }
                build.value(Value::Bool(b));
            }
            Event::Int(n) => {
                nodes = nodes.saturating_add(1);
                if nodes > limits.most_json_nodes {
                    return Err(JwtError::TokenTooLarge);
                }
                build.value(Value::Int(n));
            }
            Event::Float(f) => {
                nodes = nodes.saturating_add(1);
                if nodes > limits.most_json_nodes {
                    return Err(JwtError::TokenTooLarge);
                }
                build.value(Value::Float(f));
            }
            Event::Text(text) => {
                nodes = nodes.saturating_add(1);
                if nodes > limits.most_json_nodes {
                    return Err(JwtError::TokenTooLarge);
                }
                build.value(Value::Text(text.to_string_in(alloc)));
            }
            Event::StartList => {
                nodes = nodes.saturating_add(1);
                if nodes > limits.most_json_nodes {
                    return Err(JwtError::TokenTooLarge);
                }
                build.start_list(0);
            }
            Event::StartMap => {
                nodes = nodes.saturating_add(1);
                if nodes > limits.most_json_nodes {
                    return Err(JwtError::TokenTooLarge);
                }
                build.start_map(0);
            }
            Event::EndList | Event::EndMap => build.end(),
            Event::Key(key) => {
                if build.key(key.to_string_in(alloc)).is_err() {
                    return Err(JwtError::InvalidJson);
                }
            }
        }
    }

    match build.finish() {
        Some(value) => Ok(value),
        None => Err(JwtError::InvalidJson),
    }
}

fn b64url_encode_append<A: Allocator>(input: &[u8], out: &mut String<A>) -> Result<(), JwtError> {
    if let Some(need) = URL_SAFE_NO_PAD.encoded_len(input.len()) {
        out.try_reserve(need)?;
    }
    let mut scratch = [0u8; 64];
    for chunk in input.chunks(48) {
        let written = match URL_SAFE_NO_PAD.encode_into(chunk, &mut scratch) {
            Ok(n) => n,
            Err(_) => return Err(JwtError::InvalidBase64),
        };
        let Some(encoded_bytes) = scratch.get(..written) else {
            return Err(JwtError::InvalidBase64);
        };
        let Ok(encoded_str) = core::str::from_utf8(encoded_bytes) else {
            return Err(JwtError::InvalidBase64);
        };
        out.try_push_str(encoded_str)?;
    }
    Ok(())
}

fn b64url_decode_in<A: Allocator>(input: &[u8], alloc: A) -> Result<Vec<u8, A>, JwtError> {
    let cap = URL_SAFE_NO_PAD.decoded_len_most(input.len());
    let mut out = Vec::try_with_capacity_in(cap, alloc)?;
    for _ in 0..cap {
        if let Err(refused) = out.try_push(0u8) {
            return Err(JwtError::Alloc(refused.why));
        }
    }
    let written = match URL_SAFE_NO_PAD.decode_into(input, out.as_mut_slice()) {
        Ok(n) => n,
        Err(_) => return Err(JwtError::InvalidBase64),
    };
    out.truncate(written);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_data::Int;
    use blaze_own::{String as OwnString, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::*;

    fn hex_decode_fixed<const N: usize>(hex_str: &str) -> [u8; N] {
        let mut out = [0u8; N];
        let bytes = hex_str.as_bytes();
        assert_eq!(bytes.len(), N * 2);
        let mut i = 0usize;
        while i < N {
            let hi = bytes.get(i * 2).copied().unwrap_or(b'0');
            let lo = bytes.get(i * 2 + 1).copied().unwrap_or(b'0');
            let h = match hi {
                b'0'..=b'9' => hi - b'0',
                b'a'..=b'f' => hi - b'a' + 10,
                b'A'..=b'F' => hi - b'A' + 10,
                _ => 0,
            };
            let l = match lo {
                b'0'..=b'9' => lo - b'0',
                b'a'..=b'f' => lo - b'a' + 10,
                b'A'..=b'F' => lo - b'A' + 10,
                _ => 0,
            };
            if let Some(slot) = out.get_mut(i) {
                *slot = (h << 4) | l;
            }
            i += 1;
        }
        out
    }

    #[test]
    fn rfc7515_and_jwt_io_hs256_vectors() {
        let h = heap();
        let alloc = h.alloc();

        // RFC 7515 Appendix A.1.1 HS256 vector.
        let rfc_token = concat!(
            "eyJ0eXAiOiJKV1QiLA0KICJhbGciOiJIUzI1NiJ9.",
            "eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.",
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        );
        let rfc_secret = hex_decode_fixed::<64>(concat!(
            "0323354b2b0fa5bc837e0665777ba68f5ab328e6f054c928a90f84b2d2502ebf",
            "d3fb5a92d20647ef968ab4c377623d223d2e2172052e4f08c0cd9af567d080a3",
        ));
        let opts = VerifyOpts {
            alg: Alg::Hs256,
            now_unix_secs: Some(1_300_819_379),
            leeway_secs: 0,
            require_exp: true,
            limits: JwtLimits::USUAL,
        };
        let verified = match verify_hs256_in(rfc_token, &rfc_secret, opts, alloc) {
            Ok(v) => v,
            Err(err) => panic!("RFC 7515 A.1.1 verify failed: {err}"),
        };
        assert_eq!(verified.alg, Alg::Hs256);
        assert_eq!(verified.kid, None);
        assert_eq!(verified.iss(), Some("joe"));
        assert_eq!(verified.exp(), Some(1_300_819_380));
        assert_eq!(verified.claims.get("http://example.com/is_root").and_then(Value::as_bool), Some(true));

        // Exact boundary expiration check on the RFC vector.
        let expired_opts = VerifyOpts { now_unix_secs: Some(1_300_819_380), ..opts };
        assert_eq!(
            verify_hs256_in(rfc_token, &rfc_secret, expired_opts, alloc),
            Err(JwtError::Expired { exp: 1_300_819_380, now: 1_300_819_380 })
        );

        // jwt.io default HS256 vector.
        let jwt_io_token = concat!(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        );
        let jwt_io_opts = VerifyOpts {
            alg: Alg::Hs256,
            now_unix_secs: Some(1_516_239_100),
            leeway_secs: 0,
            require_exp: false,
            limits: JwtLimits::USUAL,
        };
        let tok = match verify_hs256_in(jwt_io_token, b"your-256-bit-secret", jwt_io_opts, alloc) {
            Ok(v) => v,
            Err(err) => panic!("jwt.io vector failed: {err}"),
        };
        assert_eq!(tok.sub(), Some("1234567890"));
        assert_eq!(tok.iat(), Some(1_516_239_022));
        assert_eq!(tok.claims.get("name").and_then(Value::as_str), Some("John Doe"));
    }

    #[test]
    fn eddsa_verify_and_unverified_kid_lookup() {
        let h = heap();
        let alloc = h.alloc();

        // Ed25519 JWT signed with RFC 8032 TEST 1 keypair.
        let pub_key = hex_decode_fixed::<32>(
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        );
        let token = concat!(
            "eyJhbGciOiJFZERTQSIsImtpZCI6ImsxIiwidHlwIjoiSldUIn0.",
            "eyJzdWIiOiJhbGljZSIsImlzcyI6ImJsYXplIiwiZXhwIjoyMDAwMDAwMDAwLCJuYmYiOjEwMDAwMDAwMDAsImlhdCI6MTAwMDAwMDAwMCwianRpIjoidG9rLTEifQ.",
            "5cWMLaQ2tne8e89pQaRCU7xKylvtMDLTwFzkI99aGrcucWTay30jIaf3gHLiIitq5QOQJRT_y38O3ajPcOU7Dw",
        );

        let unverified = match decode_unverified_in(token, JwtLimits::USUAL, alloc) {
            Ok(u) => u,
            Err(err) => panic!("decode_unverified_in failed: {err}"),
        };
        assert_eq!(unverified.alg, Some(Alg::EdDsa));
        assert_eq!(unverified.alg_str(), Some("EdDSA"));
        assert_eq!(unverified.kid.as_ref().map(OwnString::as_str), Some("k1"));
        assert_eq!(unverified.sub(), Some("alice"));

        let opts = VerifyOpts {
            alg: Alg::EdDsa,
            now_unix_secs: Some(1_500_000_000),
            leeway_secs: 0,
            require_exp: true,
            limits: JwtLimits::USUAL,
        };
        let verified = match verify_eddsa_in(token, &pub_key, opts, alloc) {
            Ok(v) => v,
            Err(err) => panic!("verify_eddsa_in failed: {err}"),
        };
        assert_eq!(verified.alg, Alg::EdDsa);
        assert_eq!(verified.kid.as_ref().map(OwnString::as_str), Some("k1"));
        assert_eq!(verified.sub(), Some("alice"));
        assert_eq!(verified.iss(), Some("blaze"));
        assert_eq!(verified.jti(), Some("tok-1"));
        assert_eq!(verified.exp(), Some(2_000_000_000));
        assert_eq!(verified.nbf(), Some(1_000_000_000));
        assert_eq!(verified.iat(), Some(1_000_000_000));

        // Algorithm confusion: passing EdDSA token to verify_hs256_in with public key bytes as HMAC secret.
        let hs_opts = VerifyOpts { alg: Alg::Hs256, ..opts };
        assert_eq!(
            verify_hs256_in(token, &pub_key, hs_opts, alloc),
            Err(JwtError::AlgMismatch { expected: Alg::Hs256, actual: Alg::EdDsa })
        );

        // Wrong key refuses signature.
        let mut wrong_key = pub_key;
        if let Some(first) = wrong_key.get_mut(0) {
            *first ^= 0x01;
        }
        assert_eq!(verify_eddsa_in(token, &wrong_key, opts, alloc), Err(JwtError::InvalidSignature));
    }

    #[test]
    fn refuses_alg_none_mismatch_tampering_and_time_bounds() {
        let h = heap();
        let alloc = h.alloc();

        // alg: "none" with empty signature.
        let none_token = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhZG1pbiJ9.";
        let hs_opts = VerifyOpts {
            alg: Alg::Hs256,
            now_unix_secs: Some(1_000),
            leeway_secs: 0,
            require_exp: false,
            limits: JwtLimits::USUAL,
        };
        assert_eq!(verify_hs256_in(none_token, b"secret", hs_opts, alloc), Err(JwtError::UnsupportedAlg));

        let ed_opts = VerifyOpts { alg: Alg::EdDsa, ..hs_opts };
        assert_eq!(verify_eddsa_in(none_token, &[0u8; 32], ed_opts, alloc), Err(JwtError::UnsupportedAlg));

        // Mismatched opts.alg vs verifier function.
        assert_eq!(
            verify_hs256_in(none_token, b"secret", ed_opts, alloc),
            Err(JwtError::AlgMismatch { expected: Alg::Hs256, actual: Alg::EdDsa })
        );
        assert_eq!(
            verify_eddsa_in(none_token, &[0u8; 32], hs_opts, alloc),
            Err(JwtError::AlgMismatch { expected: Alg::EdDsa, actual: Alg::Hs256 })
        );

        // Sign a token with exp=1000, nbf=500.
        let mut claims_map = Map::new_in(alloc);
        claims_map.insert(OwnString::from_str_in("sub", alloc), Value::Text(OwnString::from_str_in("u1", alloc)));
        claims_map.insert(OwnString::from_str_in("exp", alloc), Value::Int(Int::from_i64(1_000)));
        claims_map.insert(OwnString::from_str_in("nbf", alloc), Value::Int(Int::from_i64(500)));
        let claims = Value::Map(claims_map);
        let token = match sign_hs256_in(&claims, Some("key-7"), b"top-secret", alloc) {
            Ok(t) => t,
            Err(err) => panic!("sign failed: {err}"),
        };

        // Tamper signature first character (preserves canonical unpadded base64url length/bits).
        let (_, hdr_b64, pay_b64, sig_b64) = match split_compact(token.as_str()) {
            Ok(parts) => parts,
            Err(err) => panic!("split failed: {err}"),
        };
        let mut tampered_sig_part = OwnString::new_in(alloc);
        let mut sig_chars = sig_b64.chars();
        let flipped_first = match sig_chars.next() {
            Some('A') => 'B',
            Some(_) => 'A',
            None => 'A',
        };
        tampered_sig_part.push(flipped_first);
        tampered_sig_part.push_str(sig_chars.as_str());
        let tampered_sig = format_in!(alloc, "{hdr_b64}.{pay_b64}.{}", tampered_sig_part.as_str());
        assert_eq!(
            verify_hs256_in(
                tampered_sig.as_str(),
                b"top-secret",
                VerifyOpts { now_unix_secs: Some(750), ..hs_opts },
                alloc,
            ),
            Err(JwtError::InvalidSignature)
        );

        // Tamper payload segment while keeping header and signature.
        let fake_payload = "eyJzdWIiOiJhZG1pbiIsImV4cCI6MTAwMCwibmJmIjo1MDB9";
        let tampered_payload = format_in!(alloc, "{hdr_b64}.{fake_payload}.{sig_b64}");
        assert_eq!(
            verify_hs256_in(
                tampered_payload.as_str(),
                b"top-secret",
                VerifyOpts { now_unix_secs: Some(750), ..hs_opts },
                alloc,
            ),
            Err(JwtError::InvalidSignature)
        );

        // Time bounds & leeway.
        let check_at = |now: i64, leeway: u64| {
            verify_hs256_in(
                token.as_str(),
                b"top-secret",
                VerifyOpts {
                    alg: Alg::Hs256,
                    now_unix_secs: Some(now),
                    leeway_secs: leeway,
                    require_exp: true,
                    limits: JwtLimits::USUAL,
                },
                alloc,
            )
        };
        assert_eq!(check_at(499, 0), Err(JwtError::NotYetValid { nbf: 500, now: 499 }));
        assert!(check_at(499, 1).is_ok());
        assert!(check_at(500, 0).is_ok());
        assert!(check_at(999, 0).is_ok());
        assert_eq!(check_at(1_000, 0), Err(JwtError::Expired { exp: 1_000, now: 1_000 }));
        assert!(check_at(1_000, 1).is_ok());
        assert_eq!(check_at(1_030, 30), Err(JwtError::Expired { exp: 1_000, now: 1_030 }));
        assert!(check_at(1_029, 30).is_ok());

        // Missing exp refused when require_exp is true.
        let mut no_exp_map = Map::new_in(alloc);
        no_exp_map.insert(OwnString::from_str_in("sub", alloc), Value::Text(OwnString::from_str_in("u2", alloc)));
        let no_exp_tok = match sign_hs256_in(&Value::Map(no_exp_map), None, b"s", alloc) {
            Ok(t) => t,
            Err(err) => panic!("sign failed: {err}"),
        };
        assert_eq!(
            verify_hs256_in(
                no_exp_tok.as_str(),
                b"s",
                VerifyOpts { require_exp: true, ..hs_opts },
                alloc,
            ),
            Err(JwtError::MissingExp)
        );

        // Size limit enforcement.
        assert_eq!(
            verify_hs256_in(
                token.as_str(),
                b"top-secret",
                VerifyOpts {
                    limits: JwtLimits { most_token_bytes: 16, most_json_nodes: 64 },
                    ..hs_opts
                },
                alloc,
            ),
            Err(JwtError::TokenTooLarge)
        );
    }

    fn run_hs256_oracle<A: Allocator + Copy>(
        tape: &Tape,
        plant_skip_sig: bool,
        plant_skip_exp: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        const SUBS: &[&str] = &["alice", "bob", "svc-worker", "user/42", "quote\"slash\\"];
        const KIDS: &[Option<&str>] = &[None, Some("k1"), Some("prod-2026"), Some("key\"esc")];
        const SECRETS: &[&[u8]] = &[b"alpha-secret", b"beta-secret-key-0123456789", b"k"];

        let sub = SUBS.get(tape.index(SUBS.len(), "sub")).copied().unwrap_or("alice");
        let kid = KIDS.get(tape.index(KIDS.len(), "kid")).copied().unwrap_or(None);
        let secret = SECRETS.get(tape.index(SECRETS.len(), "secret")).copied().unwrap_or(b"k");
        let exp = tape.int_in(1_000i64..=10_000i64, "exp");
        let nbf = exp - tape.int_in(10i64..=500i64, "span");
        let leeway = tape.int_in(0u64..=30u64, "leeway");

        let mut claims_map = Map::new_in(alloc);
        claims_map.insert(OwnString::from_str_in("sub", alloc), Value::Text(OwnString::from_str_in(sub, alloc)));
        claims_map.insert(OwnString::from_str_in("exp", alloc), Value::Int(Int::from_i64(exp)));
        claims_map.insert(OwnString::from_str_in("nbf", alloc), Value::Int(Int::from_i64(nbf)));
        let claims = Value::Map(claims_map);

        let token = sign_hs256_in(&claims, kid, secret, alloc)
            .map_err(|e| Broke::Oracle(format_in!(alloc, "sign failed: {e}").as_str().into()))?;

        // 1. Valid verification must succeed and round-trip claims and kid.
        let valid_opts = VerifyOpts {
            alg: Alg::Hs256,
            now_unix_secs: Some(exp - 1),
            leeway_secs: leeway,
            require_exp: true,
            limits: JwtLimits::USUAL,
        };
        let verified = verify_hs256_in_planted(
            token.as_str(),
            secret,
            valid_opts,
            alloc,
            plant_skip_sig,
            plant_skip_exp,
        )
        .map_err(|e| Broke::Oracle(format_in!(alloc, "valid verify failed: {e}").as_str().into()))?;
        if verified.sub() != Some(sub) || verified.exp() != Some(exp) || verified.nbf() != Some(nbf) {
            return Err(Broke::Oracle("claims round-trip mismatch".into()));
        }
        if verified.kid.as_ref().map(OwnString::as_str) != kid {
            return Err(Broke::Oracle("kid round-trip mismatch".into()));
        }

        // 2. Wrong secret must be refused as InvalidSignature.
        let wrong_secret = b"definitely-the-wrong-secret";
        match verify_hs256_in_planted(
            token.as_str(),
            wrong_secret,
            valid_opts,
            alloc,
            plant_skip_sig,
            plant_skip_exp,
        ) {
            Err(JwtError::InvalidSignature) => {}
            Ok(_) => return Err(Broke::Oracle("forged signature accepted".into())),
            Err(other) => {
                return Err(Broke::Oracle(
                    format_in!(alloc, "unexpected wrong-secret error: {other}").as_str().into(),
                ));
            }
        }

        // 3. Expired timestamp past leeway must be refused as Expired.
        let expired_now = exp + i64::try_from(leeway).unwrap_or(0) + 1;
        let expired_opts = VerifyOpts { now_unix_secs: Some(expired_now), ..valid_opts };
        match verify_hs256_in_planted(
            token.as_str(),
            secret,
            expired_opts,
            alloc,
            plant_skip_sig,
            plant_skip_exp,
        ) {
            Err(JwtError::Expired { .. }) => {}
            Ok(_) => return Err(Broke::Oracle("expired token accepted".into())),
            Err(other) => {
                return Err(Broke::Oracle(
                    format_in!(alloc, "unexpected expired error: {other}").as_str().into(),
                ));
            }
        }

        Ok(())
    }

    #[test]
    fn property_hs256_round_trip() {
        let h = heap();
        check(Config::cases(256), |tape| {
            run_hs256_oracle(tape, false, false, h.alloc())
        });
    }

    #[test]
    fn control_catches_skipped_signature_check() {
        let h = heap();
        control(
            Config::cases(64),
            false,
            true,
            "forged signature accepted",
            |tape, plant_skip_sig| run_hs256_oracle(tape, plant_skip_sig, false, h.alloc()),
        );
    }

    #[test]
    fn control_catches_skipped_exp_check() {
        let h = heap();
        control(
            Config::cases(64),
            false,
            true,
            "expired token accepted",
            |tape, plant_skip_exp| run_hs256_oracle(tape, false, plant_skip_exp, h.alloc()),
        );
    }
}
