//! Zero-trust LAN peer-to-peer `.crate` discovery & HTTP serving protocol (sans-IO, rule 14).
//!
//! Peers exchange ONLY raw `.crate` tarballs addressed by their exact 64-character lowercase hex
//! SHA-256 (`GET /v1/crate/<sha256>`), NEVER `BLZFPK03` records. Every byte received from a peer
//! is verified against `blaze_sha256::digest(bytes) == *sha256` before a single byte is inflated.
//!
//! ## Wire protocol (`BLZP`, version 1)
//!
//! - **`HAVE`** (client -> LAN multicast `239.255.77.77:41729` / broadcast, max 32 hashes =
//!   1,040 bytes < MTU):
//!   `b"BLZP" | version:u8(1) | kind:u8(1) | count:u16_le | nonce:u64_le | count * [u8; 32]`
//! - **`HAS`** (peer -> client unicast, 24 bytes fixed):
//!   `b"BLZP" | version:u8(1) | kind:u8(2) | tcp_port:u16_le | nonce:u64_le | mask:u32_le | active_transfers:u32_le`

#![blaze::sans_io]

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use blaze_own::{Alloc, String};
use blaze_text::Writes as _;

/// Four-byte packet magic identifying the blaze LAN peer protocol.
pub const MAGIC: [u8; 4] = *b"BLZP";
/// Wire protocol version.
pub const VERSION: u8 = 1;
/// Client -> LAN probe asking which peers hold any of up to 32 `.crate` SHA-256 digests.
pub const KIND_HAVE: u8 = 1;
/// Peer -> client unicast reply announcing matching digests and TCP serving port.
pub const KIND_HAS: u8 = 2;

/// Default IPv4 multicast group for LAN peer discovery (organization-local scope).
pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 77);
/// Default UDP port for LAN peer discovery.
pub const DEFAULT_UDP_PORT: u16 = 41729;

/// Maximum number of SHA-256 hashes carried by one `HAVE` datagram (fits a `u32` reply bitmask
/// and stays strictly below Ethernet/IPv6 MTU).
pub const HAVE_MAX_HASHES: usize = 32;
/// Fixed header bytes of a `HAVE` datagram (`4 + 1 + 1 + 2 + 8`).
pub const HAVE_HEADER_BYTES: usize = 16;
/// Maximum byte length of a `HAVE` datagram (`16 + 32 * 32 = 1040`).
pub const HAVE_MAX_BYTES: usize = HAVE_HEADER_BYTES + HAVE_MAX_HASHES * 32;
/// Exact byte length of a `HAS` datagram (`4 + 1 + 1 + 2 + 8 + 4 + 4 = 24`).
pub const HAS_BYTES: usize = 24;

/// Upper bound on an incoming peer HTTP request head before rejecting with 400.
pub const MAX_REQUEST_HEAD: usize = 4096;

/// Prefix of the peer HTTP crate serving route.
pub const CRATE_ROUTE_PREFIX: &str = "/v1/crate/";

/// Decoded zero-copy view of a `HAVE` datagram.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HaveRef<'a> {
    pub nonce: u64,
    count: usize,
    payload: &'a [u8],
}

impl<'a> HaveRef<'a> {
    /// Number of SHA-256 hashes in this `HAVE` packet (`1..=HAVE_MAX_HASHES`).
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the `index`-th SHA-256 digest, or `None` if out of bounds.
    pub fn hash(&self, index: usize) -> Option<[u8; 32]> {
        if index >= self.count {
            return None;
        }
        let start = index.checked_mul(32)?;
        let end = start.checked_add(32)?;
        let slice = self.payload.get(start..end)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(slice);
        Some(out)
    }

    /// Iterates over every `(index, sha256)` in the probe.
    pub fn iter(&self) -> impl Iterator<Item = (usize, [u8; 32])> + '_ {
        (0..self.count).filter_map(|i| self.hash(i).map(|h| (i, h)))
    }
}

/// Decoded `HAS` unicast reply from a LAN peer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HasPacket {
    /// TCP port on the sender's IP where the peer serves `GET /v1/crate/<sha256>`.
    pub tcp_port: u16,
    /// Echoed `nonce` from the client's `HAVE` probe.
    pub nonce: u64,
    /// Bitmask of matching indices in the `HAVE` probe (`bit i` = hash `i` is held).
    pub mask: u32,
    /// Number of currently active HTTP transfers on the peer (for tie-breaking).
    pub active_transfers: u32,
}

/// Computes the SHA-256 digest of `bytes` (used by callers verifying or indexing `.crate` files).
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    blaze_sha256::digest(bytes)
}

/// Encodes a `HAVE` datagram for `hashes` (`1..=HAVE_MAX_HASHES`) into `out`, returning the
/// encoded byte length or `None` if `hashes` is empty or exceeds [`HAVE_MAX_HASHES`].
pub fn encode_have(nonce: u64, hashes: &[[u8; 32]], out: &mut [u8; HAVE_MAX_BYTES]) -> Option<usize> {
    if hashes.is_empty() || hashes.len() > HAVE_MAX_HASHES {
        return None;
    }
    let [c0, c1] = u16::try_from(hashes.len()).ok()?.to_le_bytes();
    let [n0, n1, n2, n3, n4, n5, n6, n7] = nonce.to_le_bytes();
    let [m0, m1, m2, m3] = MAGIC;
    let header = [m0, m1, m2, m3, VERSION, KIND_HAVE, c0, c1, n0, n1, n2, n3, n4, n5, n6, n7];
    out.get_mut(..HAVE_HEADER_BYTES)?.copy_from_slice(&header);
    let mut at = HAVE_HEADER_BYTES;
    for hash in hashes {
        let next = at.checked_add(32)?;
        out.get_mut(at..next)?.copy_from_slice(hash);
        at = next;
    }
    Some(at)
}

/// Decodes a `HAVE` datagram, validating magic, version, kind, non-zero bounded count, and exact
/// payload length.
pub fn decode_have(bytes: &[u8]) -> Option<HaveRef<'_>> {
    let (head, payload) = bytes.split_first_chunk::<HAVE_HEADER_BYTES>()?;
    let [m0, m1, m2, m3, ver, kind, c0, c1, n0, n1, n2, n3, n4, n5, n6, n7] = *head;
    if [m0, m1, m2, m3] != MAGIC || ver != VERSION || kind != KIND_HAVE {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([c0, c1]));
    if count == 0 || count > HAVE_MAX_HASHES {
        return None;
    }
    let expected = count.checked_mul(32)?;
    if payload.len() != expected {
        return None;
    }
    let nonce = u64::from_le_bytes([n0, n1, n2, n3, n4, n5, n6, n7]);
    Some(HaveRef { nonce, count, payload })
}

/// Encodes a `HAS` datagram into its fixed 24-byte wire representation.
pub fn encode_has(has: HasPacket) -> [u8; HAS_BYTES] {
    let [m0, m1, m2, m3] = MAGIC;
    let [p0, p1] = has.tcp_port.to_le_bytes();
    let [n0, n1, n2, n3, n4, n5, n6, n7] = has.nonce.to_le_bytes();
    let [k0, k1, k2, k3] = has.mask.to_le_bytes();
    let [a0, a1, a2, a3] = has.active_transfers.to_le_bytes();
    [m0, m1, m2, m3, VERSION, KIND_HAS, p0, p1, n0, n1, n2, n3, n4, n5, n6, n7, k0, k1, k2, k3, a0, a1, a2, a3]
}

/// Decodes a `HAS` datagram, rejecting malformed packets, port 0, and empty match masks.
pub fn decode_has(bytes: &[u8]) -> Option<HasPacket> {
    let arr: &[u8; HAS_BYTES] = bytes.try_into().ok()?;
    let [m0, m1, m2, m3, ver, kind, p0, p1, n0, n1, n2, n3, n4, n5, n6, n7, k0, k1, k2, k3, a0, a1, a2, a3] = *arr;
    if [m0, m1, m2, m3] != MAGIC || ver != VERSION || kind != KIND_HAS {
        return None;
    }
    let tcp_port = u16::from_le_bytes([p0, p1]);
    if tcp_port == 0 {
        return None;
    }
    let nonce = u64::from_le_bytes([n0, n1, n2, n3, n4, n5, n6, n7]);
    let mask = u32::from_le_bytes([k0, k1, k2, k3]);
    if mask == 0 {
        return None;
    }
    let active_transfers = u32::from_le_bytes([a0, a1, a2, a3]);
    Some(HasPacket { tcp_port, nonce, mask, active_transfers })
}

/// Pure SSRF/LAN IP filter: accepts ONLY private LAN, loopback, IPv6 ULA, and IPv6 link-local
/// addresses.
///
/// Explicitly rejects:
/// - `169.254.0.0/16` (IPv4 link-local / cloud instance metadata service)
/// - `0.0.0.0/8`, multicast, broadcast, and all public/routable IPv4 addresses
/// - IPv4-mapped IPv6 (`::ffff:0:0/96`), multicast, unspecified, and public IPv6 addresses
pub const fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_lan_ipv4(v4),
        IpAddr::V6(v6) => is_lan_ipv6(v6),
    }
}

const fn is_lan_ipv4(ip: Ipv4Addr) -> bool {
    let [o0, o1, _, _] = ip.octets();
    // 10.0.0.0/8 (RFC 1918)
    if o0 == 10 {
        return true;
    }
    // 172.16.0.0/12 (RFC 1918)
    if o0 == 172 && o1 >= 16 && o1 <= 31 {
        return true;
    }
    // 192.168.0.0/16 (RFC 1918)
    if o0 == 192 && o1 == 168 {
        return true;
    }
    // 127.0.0.0/8 (loopback)
    if o0 == 127 {
        return true;
    }
    // Everything else (including 169.254.0.0/16 cloud metadata, public IPs, 0/8, multicast,
    // broadcast) is rejected.
    false
}

const fn is_lan_ipv6(ip: Ipv6Addr) -> bool {
    let [s0, s1, s2, s3, s4, s5, s6, s7] = ip.segments();
    // ::1 (loopback)
    if s0 == 0 && s1 == 0 && s2 == 0 && s3 == 0 && s4 == 0 && s5 == 0 && s6 == 0 && s7 == 1 {
        return true;
    }
    // fc00::/7 (Unique Local Address)
    if (s0 & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10 (IPv6 link-local)
    if (s0 & 0xffc0) == 0xfe80 {
        return true;
    }
    false
}

/// Formats `http://<peer-ip>:<port>/v1/crate/<64-lower-hex-sha256>` into `out`.
pub fn format_crate_url<'a>(peer: SocketAddr, sha256: &[u8; 32], out: &mut String<Alloc<'a>>) {
    out.str("http://");
    match peer {
        SocketAddr::V4(v4) => {
            let [o0, o1, o2, o3] = v4.ip().octets();
            out.int(o0).str(".").int(o1).str(".").int(o2).str(".").int(o3);
            out.str(":").int(v4.port());
        }
        SocketAddr::V6(v6) => {
            let s = v6.ip().segments();
            out.str("[");
            for (i, seg) in s.iter().enumerate() {
                if i > 0 {
                    out.str(":");
                }
                write_hex_u16(*seg, out);
            }
            out.str("]:").int(v6.port());
        }
    }
    out.str(CRATE_ROUTE_PREFIX).hex_bytes(sha256);
}

fn write_hex_u16<'a>(value: u16, out: &mut String<Alloc<'a>>) {
    let mut started = false;
    for shift in [12u8, 8, 4, 0] {
        let nibble = u8::try_from((value >> shift) & 0xf).unwrap_or(0);
        if nibble != 0 || started || shift == 0 {
            started = true;
            let byte = if nibble < 10 { b'0' + nibble } else { b'a' + (nibble - 10) };
            let ch = [byte];
            if let Ok(s) = core::str::from_utf8(&ch) {
                out.str(s);
            }
        }
    }
}

/// Outcome of parsing an incoming HTTP/1.x request head on a peer server connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeerRequest {
    /// Valid `GET /v1/crate/<64-lower-hex-sha256>` request.
    GetCrate([u8; 32]),
    /// Valid `GET` to any other path.
    NotFound,
    /// Non-`GET` method on an otherwise well-formed HTTP/1.x request line.
    MethodNotAllowed,
    /// Malformed HTTP request or head exceeding [`MAX_REQUEST_HEAD`].
    BadRequest,
    /// Request head is incomplete (no `\r\n\r\n` terminator yet).
    NeedMore,
}

/// Parses an HTTP/1.x request head for the peer server (`GET /v1/crate/<64-lower-hex>`).
pub fn parse_http_request(buf: &[u8]) -> PeerRequest {
    let head_end = find_crlf_crlf(buf);
    let Some(end) = head_end else {
        if buf.len() >= MAX_REQUEST_HEAD {
            return PeerRequest::BadRequest;
        }
        return PeerRequest::NeedMore;
    };
    if end > MAX_REQUEST_HEAD {
        return PeerRequest::BadRequest;
    }
    let Some(head) = buf.get(..end) else {
        return PeerRequest::BadRequest;
    };
    let line_end = find_crlf(head).unwrap_or(head.len());
    let Some(line) = head.get(..line_end) else {
        return PeerRequest::BadRequest;
    };

    let Some(sp1) = line.iter().position(|&b| b == b' ') else {
        return PeerRequest::BadRequest;
    };
    let Some((method, after_sp1)) = line.split_at_checked(sp1) else {
        return PeerRequest::BadRequest;
    };
    let Some((_, rest)) = after_sp1.split_first() else {
        return PeerRequest::BadRequest;
    };
    let Some(sp2) = rest.iter().position(|&b| b == b' ') else {
        return PeerRequest::BadRequest;
    };
    let Some((target, after_sp2)) = rest.split_at_checked(sp2) else {
        return PeerRequest::BadRequest;
    };
    let Some((_, version)) = after_sp2.split_first() else {
        return PeerRequest::BadRequest;
    };

    if version != b"HTTP/1.1" && version != b"HTTP/1.0" {
        return PeerRequest::BadRequest;
    }
    if method != b"GET" {
        if method.is_empty() || !method.iter().all(|b| b.is_ascii_uppercase()) {
            return PeerRequest::BadRequest;
        }
        return PeerRequest::MethodNotAllowed;
    }
    let prefix = CRATE_ROUTE_PREFIX.as_bytes();
    let Some(hex) = target.strip_prefix(prefix) else {
        return PeerRequest::NotFound;
    };
    match parse_lower_hex_32(hex) {
        Some(sha256) => PeerRequest::GetCrate(sha256),
        None => PeerRequest::NotFound,
    }
}

/// Parses an exact 64-byte lowercase hexadecimal SHA-256 digest.
pub fn parse_lower_hex_32(hex: &[u8]) -> Option<[u8; 32]> {
    let (pairs, rem) = hex.as_chunks::<2>();
    if pairs.len() != 32 || !rem.is_empty() {
        return None;
    }
    let mut out = [0u8; 32];
    for (&[h, l], slot) in pairs.iter().zip(out.iter_mut()) {
        let hi = lower_hex_nibble(h)?;
        let lo = lower_hex_nibble(l)?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

const fn lower_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|w| w == b"\r\n")
}

fn find_crlf_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Writes an HTTP/1.1 `200 OK` response head with exact `Content-Length` into `out`, returning
/// the byte length written.
pub fn write_ok_head(content_len: u64, out: &mut [u8; 160]) -> usize {
    const PREFIX: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: ";
    const SUFFIX: &[u8] = b"\r\nConnection: close\r\n\r\n";

    let mut at = 0;
    if let Some(dst) = out.get_mut(at..at + PREFIX.len()) {
        dst.copy_from_slice(PREFIX);
        at += PREFIX.len();
    }

    let mut digits = [0u8; 20];
    let mut n = content_len;
    let mut dlen = 0;
    if n == 0 {
        if let Some(slot) = digits.first_mut() {
            *slot = b'0';
            dlen = 1;
        }
    } else {
        while n > 0 {
            let rem = u8::try_from(n % 10).unwrap_or(0);
            if let Some(slot) = digits.get_mut(dlen) {
                *slot = b'0' + rem;
                dlen += 1;
            }
            n /= 10;
        }
        if let Some(slice) = digits.get_mut(..dlen) {
            slice.reverse();
        }
    }
    if let (Some(src), Some(dst)) = (digits.get(..dlen), out.get_mut(at..at + dlen)) {
        dst.copy_from_slice(src);
        at += dlen;
    }

    if let Some(dst) = out.get_mut(at..at + SUFFIX.len()) {
        dst.copy_from_slice(SUFFIX);
        at += SUFFIX.len();
    }
    at
}

/// Static HTTP/1.1 error response with `Content-Length: 0` and `Connection: close`.
pub const fn error_response(status: u16) -> &'static [u8] {
    match status {
        400 => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        404 => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        405 => b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        _ => b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    }
}

#[cfg(test)]
mod tests {
    use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

    use super::*;

    #[test]
    fn have_and_has_round_trip_and_reject_malformed() {
        let h1 = [0x11u8; 32];
        let h2 = [0x22u8; 32];
        let mut buf = [0u8; HAVE_MAX_BYTES];
        assert!(encode_have(1, &[], &mut buf).is_none());
        let len = encode_have(0x0102_0304_0506_0708, &[h1, h2], &mut buf).expect("encode HAVE");
        assert_eq!(len, HAVE_HEADER_BYTES + 64);

        let decoded = decode_have(&buf[..len]).expect("decode HAVE");
        assert_eq!(decoded.nonce, 0x0102_0304_0506_0708);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded.hash(0), Some(h1));
        assert_eq!(decoded.hash(1), Some(h2));
        assert_eq!(decoded.hash(2), None);

        // Truncated or extra trailing byte is rejected.
        assert!(decode_have(&buf[..len - 1]).is_none());
        assert!(decode_have(&buf[..len + 1]).is_none());

        // Max 32 hashes stays below 1200B MTU.
        let max_hashes = [[0xabu8; 32]; HAVE_MAX_HASHES];
        let max_len = encode_have(99, &max_hashes, &mut buf).expect("32 hashes");
        assert_eq!(max_len, 1040);
        assert!(decode_have(&buf[..max_len]).is_some());

        let has = HasPacket { tcp_port: 41730, nonce: 42, mask: 0b101, active_transfers: 3 };
        let wire = encode_has(has);
        assert_eq!(decode_has(&wire), Some(has));
        // Port 0 and mask 0 are rejected.
        assert!(decode_has(&encode_has(HasPacket { tcp_port: 0, ..has })).is_none());
        assert!(decode_has(&encode_has(HasPacket { mask: 0, ..has })).is_none());
    }

    #[test]
    fn lan_ip_filter_allows_private_and_rejects_public_and_metadata() {
        // Accepted LAN / loopback ranges.
        for ip in [
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(10, 255, 255, 254)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(172, 31, 255, 254)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6("fd12:3456:789a::1".parse().expect("ula")),
            IpAddr::V6("fc00::1".parse().expect("ula")),
            IpAddr::V6("fe80::1".parse().expect("link-local")),
        ] {
            assert!(is_lan_ip(ip), "expected LAN IP allowed: {ip}");
        }

        // Rejected SSRF / cloud metadata / public / unspecified / multicast / mapped ranges.
        for ip in [
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(172, 15, 255, 255)),
            IpAddr::V4(Ipv4Addr::new(172, 32, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::BROADCAST),
            IpAddr::V4(MULTICAST_GROUP),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6("2001:4860:4860::8888".parse().expect("public v6")),
            IpAddr::V6("::ffff:127.0.0.1".parse().expect("v4-mapped")),
            IpAddr::V6("ff02::1".parse().expect("multicast v6")),
        ] {
            assert!(!is_lan_ip(ip), "expected non-LAN IP rejected: {ip}");
        }
    }

    #[test]
    fn http_request_parser_and_url_formatter_are_strict() {
        let sha = [0x0au8; 32];
        let mut url = String::new_in(blaze_own::testing::heap().alloc());
        format_crate_url(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 5), 41730)), &sha, &mut url);
        assert_eq!(
            url.as_str(),
            "http://192.168.1.5:41730/v1/crate/0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a"
        );

        let mut url6 = String::new_in(blaze_own::testing::heap().alloc());
        format_crate_url(SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 8080, 0, 0)), &sha, &mut url6);
        assert_eq!(
            url6.as_str(),
            "http://[0:0:0:0:0:0:0:1]:8080/v1/crate/0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a"
        );

        let req = b"GET /v1/crate/0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert_eq!(parse_http_request(req), PeerRequest::GetCrate(sha));
        assert_eq!(parse_http_request(&req[..20]), PeerRequest::NeedMore);

        // Uppercase hex or traversal is strictly 404.
        let upper = b"GET /v1/crate/0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A HTTP/1.1\r\n\r\n";
        assert_eq!(parse_http_request(upper), PeerRequest::NotFound);
        let traversal = b"GET /v1/crate/../../etc/passwd HTTP/1.1\r\n\r\n";
        assert_eq!(parse_http_request(traversal), PeerRequest::NotFound);
        let post = b"POST /v1/crate/0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a HTTP/1.1\r\n\r\n";
        assert_eq!(parse_http_request(post), PeerRequest::MethodNotAllowed);

        let mut head = [0u8; 160];
        let n = write_ok_head(12345, &mut head);
        assert_eq!(
            &head[..n],
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 12345\r\nConnection: close\r\n\r\n"
        );
    }
}
