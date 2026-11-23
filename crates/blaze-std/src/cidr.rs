//! Pure `#![no_std]` IPv4 & IPv6 CIDR arithmetic, SSRF / routing safety classifiers, and an
//! iterative binary-trie longest-prefix-match table ([`PrefixMap`]).
//!
//! - [`Ipv4Net`], [`Ipv6Net`], and [`IpNet`] always store canonicalized network addresses (host
//!   bits masked to zero) with validated prefix lengths (`0..=32` / `0..=128`).
//! - Zero-alloc `const fn` SSRF / global-routability classifiers ([`is_globally_routable_v4`],
//!   [`is_globally_routable_v6`], [`is_globally_routable`]) block loopback, private, CGNAT,
//!   link-local / cloud metadata (`169.254.169.254`), documentation, multicast, reserved, ULA,
//!   and non-global IPv4-mapped IPv6 (`::ffff:127.0.0.1`, `::ffff:169.254.169.254`).
//! - [`PrefixMap`] performs O(32) IPv4 and O(128) IPv6 longest-prefix-match lookups over flat
//!   caller-allocated arenas with zero recursion.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use core::str::FromStr;

use blaze_own::{AllocFailure, Vec};

const IPV4_BITS: u8 = 32;
const IPV6_BITS: u8 = 128;

/// Typed failure returned when constructing or parsing a CIDR prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CidrError {
    /// Input string did not contain a `/` separating address and prefix length.
    MissingSlash,
    /// Address portion was not a valid strict IPv4 or IPv6 address.
    InvalidAddress,
    /// Prefix length was empty, had a leading zero, contained non-digits, or overflowed `u8`.
    InvalidPrefixLen,
    /// Prefix length exceeded the address family maximum (`32` for IPv4, `128` for IPv6).
    PrefixTooLong { prefix: u8, max: u8 },
    /// Strict construction refused an address with non-zero host bits past the prefix length.
    NonZeroHostBits,
}

impl fmt::Display for CidrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CidrError::MissingSlash => {
                f.write_str("bstd::cidr: CIDR notation requires `<address>/<prefix>`")
            }
            CidrError::InvalidAddress => f.write_str("bstd::cidr: invalid IP address"),
            CidrError::InvalidPrefixLen => {
                f.write_str("bstd::cidr: invalid prefix length (expected canonical decimal digits)")
            }
            CidrError::PrefixTooLong { prefix, max } => {
                write!(f, "bstd::cidr: prefix length /{prefix} exceeds maximum /{max}")
            }
            CidrError::NonZeroHostBits => {
                f.write_str("bstd::cidr: address has non-zero host bits for prefix length")
            }
        }
    }
}

impl core::error::Error for CidrError {}

#[inline]
const fn v4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (IPV4_BITS - prefix)
    }
}

#[inline]
const fn v6_mask(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (IPV6_BITS - prefix)
    }
}

#[inline]
const fn u32_to_ipv4(bits: u32) -> Ipv4Addr {
    let [a, b, c, d] = bits.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

#[inline]
const fn u128_to_ipv6(bits: u128) -> Ipv6Addr {
    let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] =
        bits.to_be_bytes();
    Ipv6Addr::new(
        u16::from_be_bytes([b0, b1]),
        u16::from_be_bytes([b2, b3]),
        u16::from_be_bytes([b4, b5]),
        u16::from_be_bytes([b6, b7]),
        u16::from_be_bytes([b8, b9]),
        u16::from_be_bytes([b10, b11]),
        u16::from_be_bytes([b12, b13]),
        u16::from_be_bytes([b14, b15]),
    )
}

fn parse_prefix_len(s: &str, max: u8) -> Result<u8, CidrError> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err(CidrError::InvalidPrefixLen);
    }
    if bytes.len() > 1 && bytes.first() == Some(&b'0') {
        return Err(CidrError::InvalidPrefixLen);
    }
    let mut acc = 0u16;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return Err(CidrError::InvalidPrefixLen);
        }
        acc = acc.saturating_mul(10).saturating_add(u16::from(b - b'0'));
        if acc > u16::from(u8::MAX) {
            return Err(CidrError::InvalidPrefixLen);
        }
    }
    let Ok(prefix) = u8::try_from(acc) else {
        return Err(CidrError::InvalidPrefixLen);
    };
    if prefix > max {
        return Err(CidrError::PrefixTooLong { prefix, max });
    }
    Ok(prefix)
}

/// Canonical IPv4 CIDR network block (host bits are always masked to zero).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Ipv4Net {
    addr: u32,
    prefix: u8,
}

impl Ipv4Net {
    /// Construct a canonical [`Ipv4Net`], masking host bits to zero.
    #[inline]
    pub const fn new(addr: Ipv4Addr, prefix: u8) -> Result<Self, CidrError> {
        if prefix > IPV4_BITS {
            return Err(CidrError::PrefixTooLong { prefix, max: IPV4_BITS });
        }
        let raw = u32::from_be_bytes(addr.octets());
        Ok(Self { addr: raw & v4_mask(prefix), prefix })
    }

    /// Construct an [`Ipv4Net`], refusing addresses with non-zero host bits.
    #[inline]
    pub const fn new_strict(addr: Ipv4Addr, prefix: u8) -> Result<Self, CidrError> {
        if prefix > IPV4_BITS {
            return Err(CidrError::PrefixTooLong { prefix, max: IPV4_BITS });
        }
        let raw = u32::from_be_bytes(addr.octets());
        let masked = raw & v4_mask(prefix);
        if raw != masked {
            return Err(CidrError::NonZeroHostBits);
        }
        Ok(Self { addr: masked, prefix })
    }

    /// Strictly parse `a.b.c.d/len`, canonicalizing host bits to zero.
    pub fn parse(s: &str) -> Result<Self, CidrError> {
        let Some((addr_part, prefix_part)) = s.split_once('/') else {
            return Err(CidrError::MissingSlash);
        };
        let Ok(addr) = Ipv4Addr::from_str(addr_part) else {
            return Err(CidrError::InvalidAddress);
        };
        let prefix = parse_prefix_len(prefix_part, IPV4_BITS)?;
        Self::new(addr, prefix)
    }

    /// Strictly parse `a.b.c.d/len`, refusing any non-zero host bits.
    pub fn parse_strict(s: &str) -> Result<Self, CidrError> {
        let Some((addr_part, prefix_part)) = s.split_once('/') else {
            return Err(CidrError::MissingSlash);
        };
        let Ok(addr) = Ipv4Addr::from_str(addr_part) else {
            return Err(CidrError::InvalidAddress);
        };
        let prefix = parse_prefix_len(prefix_part, IPV4_BITS)?;
        Self::new_strict(addr, prefix)
    }

    /// Canonical network address (first address in the block).
    #[inline]
    #[must_use]
    pub const fn network(self) -> Ipv4Addr {
        u32_to_ipv4(self.addr)
    }

    /// Broadcast address (last address in the block).
    #[inline]
    #[must_use]
    pub const fn broadcast(self) -> Ipv4Addr {
        u32_to_ipv4(self.addr | !v4_mask(self.prefix))
    }

    /// Subnet mask corresponding to `prefix_len`.
    #[inline]
    #[must_use]
    pub const fn netmask(self) -> Ipv4Addr {
        u32_to_ipv4(v4_mask(self.prefix))
    }

    /// Prefix length in bits (`0..=32`).
    #[inline]
    #[must_use]
    pub const fn prefix_len(self) -> u8 {
        self.prefix
    }

    /// Total number of IPv4 addresses in this prefix (`2^(32 - prefix)`).
    #[inline]
    #[must_use]
    pub const fn host_count(self) -> u128 {
        1u128 << (IPV4_BITS - self.prefix)
    }

    /// Return `true` if `ip` belongs to this prefix.
    #[inline]
    #[must_use]
    pub const fn contains_ip(self, ip: Ipv4Addr) -> bool {
        (u32::from_be_bytes(ip.octets()) & v4_mask(self.prefix)) == self.addr
    }

    /// Return `true` if `other` is equal to or a subnet of `self`.
    #[inline]
    #[must_use]
    pub const fn contains_net(self, other: Self) -> bool {
        self.prefix <= other.prefix && (other.addr & v4_mask(self.prefix)) == self.addr
    }

    /// Return `true` if `self` and `other` share at least one address.
    #[inline]
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.contains_net(other) || other.contains_net(self)
    }

    /// Immediate parent prefix (`prefix - 1`), or `None` for `0.0.0.0/0`.
    #[inline]
    #[must_use]
    pub const fn supernet(self) -> Option<Self> {
        if self.prefix == 0 {
            return None;
        }
        let next_prefix = self.prefix - 1;
        Some(Self { addr: self.addr & v4_mask(next_prefix), prefix: next_prefix })
    }

    /// Split into the two immediate child subnets (`prefix + 1`), or `None` for `/32`.
    #[inline]
    #[must_use]
    pub const fn subnets(self) -> Option<(Self, Self)> {
        if self.prefix >= IPV4_BITS {
            return None;
        }
        let next_prefix = self.prefix + 1;
        let bit = 1u32 << (IPV4_BITS - next_prefix);
        Some((
            Self { addr: self.addr, prefix: next_prefix },
            Self { addr: self.addr | bit, prefix: next_prefix },
        ))
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network(), self.prefix)
    }
}

impl FromStr for Ipv4Net {
    type Err = CidrError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Canonical IPv6 CIDR network block (host bits are always masked to zero).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Ipv6Net {
    addr: u128,
    prefix: u8,
}

impl Ipv6Net {
    /// Construct a canonical [`Ipv6Net`], masking host bits to zero.
    #[inline]
    pub const fn new(addr: Ipv6Addr, prefix: u8) -> Result<Self, CidrError> {
        if prefix > IPV6_BITS {
            return Err(CidrError::PrefixTooLong { prefix, max: IPV6_BITS });
        }
        let raw = u128::from_be_bytes(addr.octets());
        Ok(Self { addr: raw & v6_mask(prefix), prefix })
    }

    /// Construct an [`Ipv6Net`], refusing addresses with non-zero host bits.
    #[inline]
    pub const fn new_strict(addr: Ipv6Addr, prefix: u8) -> Result<Self, CidrError> {
        if prefix > IPV6_BITS {
            return Err(CidrError::PrefixTooLong { prefix, max: IPV6_BITS });
        }
        let raw = u128::from_be_bytes(addr.octets());
        let masked = raw & v6_mask(prefix);
        if raw != masked {
            return Err(CidrError::NonZeroHostBits);
        }
        Ok(Self { addr: masked, prefix })
    }

    /// Strictly parse `ipv6/len`, canonicalizing host bits to zero.
    pub fn parse(s: &str) -> Result<Self, CidrError> {
        let Some((addr_part, prefix_part)) = s.split_once('/') else {
            return Err(CidrError::MissingSlash);
        };
        let Ok(addr) = Ipv6Addr::from_str(addr_part) else {
            return Err(CidrError::InvalidAddress);
        };
        let prefix = parse_prefix_len(prefix_part, IPV6_BITS)?;
        Self::new(addr, prefix)
    }

    /// Strictly parse `ipv6/len`, refusing any non-zero host bits.
    pub fn parse_strict(s: &str) -> Result<Self, CidrError> {
        let Some((addr_part, prefix_part)) = s.split_once('/') else {
            return Err(CidrError::MissingSlash);
        };
        let Ok(addr) = Ipv6Addr::from_str(addr_part) else {
            return Err(CidrError::InvalidAddress);
        };
        let prefix = parse_prefix_len(prefix_part, IPV6_BITS)?;
        Self::new_strict(addr, prefix)
    }

    /// Canonical network address (first address in the block).
    #[inline]
    #[must_use]
    pub const fn network(self) -> Ipv6Addr {
        u128_to_ipv6(self.addr)
    }

    /// Subnet mask corresponding to `prefix_len`.
    #[inline]
    #[must_use]
    pub const fn netmask(self) -> Ipv6Addr {
        u128_to_ipv6(v6_mask(self.prefix))
    }

    /// Prefix length in bits (`0..=128`).
    #[inline]
    #[must_use]
    pub const fn prefix_len(self) -> u8 {
        self.prefix
    }

    /// Total number of IPv6 addresses in this prefix (saturating at `u128::MAX` for `::/0`).
    #[inline]
    #[must_use]
    pub const fn host_count(self) -> u128 {
        if self.prefix == 0 {
            u128::MAX
        } else {
            1u128 << (IPV6_BITS - self.prefix)
        }
    }

    /// Return `true` if `ip` belongs to this prefix.
    #[inline]
    #[must_use]
    pub const fn contains_ip(self, ip: Ipv6Addr) -> bool {
        (u128::from_be_bytes(ip.octets()) & v6_mask(self.prefix)) == self.addr
    }

    /// Return `true` if `other` is equal to or a subnet of `self`.
    #[inline]
    #[must_use]
    pub const fn contains_net(self, other: Self) -> bool {
        self.prefix <= other.prefix && (other.addr & v6_mask(self.prefix)) == self.addr
    }

    /// Return `true` if `self` and `other` share at least one address.
    #[inline]
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.contains_net(other) || other.contains_net(self)
    }

    /// Immediate parent prefix (`prefix - 1`), or `None` for `::/0`.
    #[inline]
    #[must_use]
    pub const fn supernet(self) -> Option<Self> {
        if self.prefix == 0 {
            return None;
        }
        let next_prefix = self.prefix - 1;
        Some(Self { addr: self.addr & v6_mask(next_prefix), prefix: next_prefix })
    }

    /// Split into the two immediate child subnets (`prefix + 1`), or `None` for `/128`.
    #[inline]
    #[must_use]
    pub const fn subnets(self) -> Option<(Self, Self)> {
        if self.prefix >= IPV6_BITS {
            return None;
        }
        let next_prefix = self.prefix + 1;
        let bit = 1u128 << (IPV6_BITS - next_prefix);
        Some((
            Self { addr: self.addr, prefix: next_prefix },
            Self { addr: self.addr | bit, prefix: next_prefix },
        ))
    }
}

impl fmt::Display for Ipv6Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network(), self.prefix)
    }
}

impl FromStr for Ipv6Net {
    type Err = CidrError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Canonical IPv4 or IPv6 CIDR prefix.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum IpNet {
    /// IPv4 CIDR block.
    V4(Ipv4Net),
    /// IPv6 CIDR block.
    V6(Ipv6Net),
}

impl IpNet {
    /// Construct a canonical [`IpNet`], masking host bits to zero.
    #[inline]
    pub const fn new(addr: IpAddr, prefix: u8) -> Result<Self, CidrError> {
        match addr {
            IpAddr::V4(v4) => match Ipv4Net::new(v4, prefix) {
                Ok(net) => Ok(IpNet::V4(net)),
                Err(err) => Err(err),
            },
            IpAddr::V6(v6) => match Ipv6Net::new(v6, prefix) {
                Ok(net) => Ok(IpNet::V6(net)),
                Err(err) => Err(err),
            },
        }
    }

    /// Construct an [`IpNet`], refusing addresses with non-zero host bits.
    #[inline]
    pub const fn new_strict(addr: IpAddr, prefix: u8) -> Result<Self, CidrError> {
        match addr {
            IpAddr::V4(v4) => match Ipv4Net::new_strict(v4, prefix) {
                Ok(net) => Ok(IpNet::V4(net)),
                Err(err) => Err(err),
            },
            IpAddr::V6(v6) => match Ipv6Net::new_strict(v6, prefix) {
                Ok(net) => Ok(IpNet::V6(net)),
                Err(err) => Err(err),
            },
        }
    }

    /// Strictly parse either an IPv4 or IPv6 CIDR prefix, canonicalizing host bits.
    pub fn parse(s: &str) -> Result<Self, CidrError> {
        let Some((addr_part, _)) = s.split_once('/') else {
            return Err(CidrError::MissingSlash);
        };
        if addr_part.contains(':') {
            Ok(IpNet::V6(Ipv6Net::parse(s)?))
        } else {
            Ok(IpNet::V4(Ipv4Net::parse(s)?))
        }
    }

    /// Strictly parse either an IPv4 or IPv6 CIDR prefix, refusing non-zero host bits.
    pub fn parse_strict(s: &str) -> Result<Self, CidrError> {
        let Some((addr_part, _)) = s.split_once('/') else {
            return Err(CidrError::MissingSlash);
        };
        if addr_part.contains(':') {
            Ok(IpNet::V6(Ipv6Net::parse_strict(s)?))
        } else {
            Ok(IpNet::V4(Ipv4Net::parse_strict(s)?))
        }
    }

    /// Canonical network address.
    #[inline]
    #[must_use]
    pub const fn network(self) -> IpAddr {
        match self {
            IpNet::V4(net) => IpAddr::V4(net.network()),
            IpNet::V6(net) => IpAddr::V6(net.network()),
        }
    }

    /// Subnet mask corresponding to `prefix_len`.
    #[inline]
    #[must_use]
    pub const fn netmask(self) -> IpAddr {
        match self {
            IpNet::V4(net) => IpAddr::V4(net.netmask()),
            IpNet::V6(net) => IpAddr::V6(net.netmask()),
        }
    }

    /// Prefix length in bits.
    #[inline]
    #[must_use]
    pub const fn prefix_len(self) -> u8 {
        match self {
            IpNet::V4(net) => net.prefix_len(),
            IpNet::V6(net) => net.prefix_len(),
        }
    }

    /// Total addresses in this prefix (saturating at `u128::MAX` for `::/0`).
    #[inline]
    #[must_use]
    pub const fn host_count(self) -> u128 {
        match self {
            IpNet::V4(net) => net.host_count(),
            IpNet::V6(net) => net.host_count(),
        }
    }

    /// Return `true` if `ip` belongs to this prefix (cross-family always returns `false`).
    #[inline]
    #[must_use]
    pub const fn contains_ip(self, ip: IpAddr) -> bool {
        match (self, ip) {
            (IpNet::V4(net), IpAddr::V4(addr)) => net.contains_ip(addr),
            (IpNet::V6(net), IpAddr::V6(addr)) => net.contains_ip(addr),
            (IpNet::V4(_), IpAddr::V6(_)) | (IpNet::V6(_), IpAddr::V4(_)) => false,
        }
    }

    /// Return `true` if `other` is equal to or a subnet of `self`.
    #[inline]
    #[must_use]
    pub const fn contains_net(self, other: Self) -> bool {
        match (self, other) {
            (IpNet::V4(a), IpNet::V4(b)) => a.contains_net(b),
            (IpNet::V6(a), IpNet::V6(b)) => a.contains_net(b),
            (IpNet::V4(_), IpNet::V6(_)) | (IpNet::V6(_), IpNet::V4(_)) => false,
        }
    }

    /// Return `true` if `self` and `other` share at least one address.
    #[inline]
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        match (self, other) {
            (IpNet::V4(a), IpNet::V4(b)) => a.overlaps(b),
            (IpNet::V6(a), IpNet::V6(b)) => a.overlaps(b),
            (IpNet::V4(_), IpNet::V6(_)) | (IpNet::V6(_), IpNet::V4(_)) => false,
        }
    }

    /// Immediate parent prefix (`prefix - 1`), or `None` for `/0`.
    #[inline]
    #[must_use]
    pub const fn supernet(self) -> Option<Self> {
        match self {
            IpNet::V4(net) => match net.supernet() {
                Some(parent) => Some(IpNet::V4(parent)),
                None => None,
            },
            IpNet::V6(net) => match net.supernet() {
                Some(parent) => Some(IpNet::V6(parent)),
                None => None,
            },
        }
    }

    /// Split into the two immediate child subnets (`prefix + 1`), or `None` for host prefixes.
    #[inline]
    #[must_use]
    pub const fn subnets(self) -> Option<(Self, Self)> {
        match self {
            IpNet::V4(net) => match net.subnets() {
                Some((a, b)) => Some((IpNet::V4(a), IpNet::V4(b))),
                None => None,
            },
            IpNet::V6(net) => match net.subnets() {
                Some((a, b)) => Some((IpNet::V6(a), IpNet::V6(b))),
                None => None,
            },
        }
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            IpNet::V4(net) => fmt::Display::fmt(&net, f),
            IpNet::V6(net) => fmt::Display::fmt(&net, f),
        }
    }
}

impl FromStr for IpNet {
    type Err = CidrError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Return `true` if `addr` is globally routable on the public IPv4 Internet.
///
/// Refuses non-global / SSRF-sensitive ranges:
/// - `0.0.0.0/8` ("this network", RFC 1122)
/// - `10.0.0.0/8` (RFC 1918 private)
/// - `100.64.0.0/10` (RFC 6598 shared / CGNAT)
/// - `127.0.0.0/8` (loopback)
/// - `169.254.0.0/16` (RFC 3927 link-local & cloud metadata `169.254.169.254`)
/// - `172.16.0.0/12` (RFC 1918 private)
/// - `192.0.0.0/24` (IETF protocol assignments)
/// - `192.0.2.0/24` (TEST-NET-1 documentation)
/// - `192.168.0.0/16` (RFC 1918 private)
/// - `198.18.0.0/15` (RFC 2544 benchmarking)
/// - `198.51.100.0/24` (TEST-NET-2 documentation)
/// - `203.0.113.0/24` (TEST-NET-3 documentation)
/// - `224.0.0.0/4` (multicast)
/// - `240.0.0.0/4` (reserved, including `255.255.255.255/32` limited broadcast)
#[inline]
#[must_use]
pub const fn is_globally_routable_v4(addr: Ipv4Addr) -> bool {
    let [a, b, c, _] = addr.octets();
    if a == 0 || a == 10 || a == 127 || a >= 224 {
        return false;
    }
    if a == 100 && (b & 0xC0) == 64 {
        return false;
    }
    if a == 169 && b == 254 {
        return false;
    }
    if a == 172 && (b & 0xF0) == 16 {
        return false;
    }
    if a == 192 && b == 168 {
        return false;
    }
    if a == 192 && b == 0 && (c == 0 || c == 2) {
        return false;
    }
    if a == 198 && (b & 0xFE) == 18 {
        return false;
    }
    if a == 198 && b == 51 && c == 100 {
        return false;
    }
    if a == 203 && b == 0 && c == 113 {
        return false;
    }
    true
}

/// Return `true` if `addr` is globally routable on the public IPv6 Internet.
///
/// Refuses non-global / SSRF-sensitive ranges:
/// - `::/128` (unspecified) and `::1/128` (loopback)
/// - `::ffff:0:0/96` (IPv4-mapped IPv6): inspects the embedded IPv4 address via
///   [`is_globally_routable_v4`] so `::ffff:127.0.0.1` and `::ffff:169.254.169.254` are blocked
/// - `100::/64` (RFC 6666 discard-only)
/// - `2001:db8::/32` (RFC 3849 documentation)
/// - `fc00::/7` (RFC 4193 unique local addresses / ULA)
/// - `fe80::/10` (RFC 4291 link-local unicast) and `fec0::/10` (deprecated site-local)
/// - `ff00::/8` (multicast)
#[inline]
#[must_use]
pub const fn is_globally_routable_v6(addr: Ipv6Addr) -> bool {
    let octets = addr.octets();
    let bits = u128::from_be_bytes(octets);
    if bits == 0 || bits == 1 {
        return false;
    }
    let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] = octets;

    // `::ffff:0:0/96` IPv4-mapped IPv6 address: delegate to IPv4 routability check.
    if b0 == 0
        && b1 == 0
        && b2 == 0
        && b3 == 0
        && b4 == 0
        && b5 == 0
        && b6 == 0
        && b7 == 0
        && b8 == 0
        && b9 == 0
        && b10 == 0xFF
        && b11 == 0xFF
    {
        return is_globally_routable_v4(Ipv4Addr::new(b12, b13, b14, b15));
    }

    // `100::/64` Discard-Only (RFC 6666).
    if b0 == 0x01 && b1 == 0 && b2 == 0 && b3 == 0 && b4 == 0 && b5 == 0 && b6 == 0 && b7 == 0 {
        return false;
    }

    // `2001:db8::/32` Documentation (RFC 3849).
    if b0 == 0x20 && b1 == 0x01 && b2 == 0x0D && b3 == 0xB8 {
        return false;
    }

    // `fc00::/7` ULA (RFC 4193).
    if (b0 & 0xFE) == 0xFC {
        return false;
    }

    // `fe80::/10` Link-Local (RFC 4291) and `fec0::/10` deprecated Site-Local (RFC 3879).
    if b0 == 0xFE && (b1 & 0x80) != 0 {
        return false;
    }

    // `ff00::/8` Multicast (RFC 4291).
    if b0 == 0xFF {
        return false;
    }

    true
}

/// Return `true` if `addr` is globally routable (dispatching to IPv4 or IPv6 rules).
#[inline]
#[must_use]
pub const fn is_globally_routable(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_globally_routable_v4(v4),
        IpAddr::V6(v6) => is_globally_routable_v6(v6),
    }
}

struct TrieNode<V> {
    left: Option<usize>,
    right: Option<usize>,
    value: Option<V>,
}

impl<V> TrieNode<V> {
    #[inline]
    const fn empty() -> Self {
        Self { left: None, right: None, value: None }
    }
}

/// Iterative binary-trie longest-prefix-match routing table over IPv4 and IPv6 [`IpNet`] keys.
///
/// Backed by two flat node arenas in the caller's allocator `A` with zero recursion.
pub struct PrefixMap<V, A: Allocator> {
    v4: Vec<TrieNode<V>, A>,
    v6: Vec<TrieNode<V>, A>,
    len: usize,
}

impl<V, A: Allocator> fmt::Debug for PrefixMap<V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrefixMap")
            .field("len", &self.len)
            .field("v4_nodes", &self.v4.len())
            .field("v6_nodes", &self.v6.len())
            .finish()
    }
}

impl<V, A: Allocator + Copy> PrefixMap<V, A> {
    /// Construct an empty [`PrefixMap`] in `alloc` (allocating nothing until the first insert).
    #[inline]
    #[must_use]
    pub const fn new_in(alloc: A) -> Self {
        Self {
            v4: Vec::new_in(alloc),
            v6: Vec::new_in(alloc),
            len: 0,
        }
    }
}

impl<V, A: Allocator> PrefixMap<V, A> {
    /// Number of prefixes currently stored in the table.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Return `true` if the table stores no prefixes.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Insert or replace `value` for `net`, returning the previous value if `net` was present.
    pub fn insert(&mut self, net: IpNet, value: V) -> Result<Option<V>, AllocFailure> {
        let prev = match net {
            IpNet::V4(v4) => insert_v4(&mut self.v4, v4, value)?,
            IpNet::V6(v6) => insert_v6(&mut self.v6, v6, value)?,
        };
        if prev.is_none() {
            self.len += 1;
        }
        Ok(prev)
    }

    /// Look up the most specific (longest-prefix) entry containing `ip`.
    ///
    /// Runs in O(32) for IPv4 and O(128) for IPv6 with zero allocations and zero recursion.
    #[must_use]
    pub fn longest_match(&self, ip: IpAddr) -> Option<(IpNet, &V)> {
        self.longest_match_planted(ip, false)
    }

    fn longest_match_planted(
        &self,
        ip: IpAddr,
        plant_short_override: bool,
    ) -> Option<(IpNet, &V)> {
        match ip {
            IpAddr::V4(addr) => {
                longest_match_v4(self.v4.as_slice(), addr, plant_short_override)
                    .map(|(net, v)| (IpNet::V4(net), v))
            }
            IpAddr::V6(addr) => {
                longest_match_v6(self.v6.as_slice(), addr, plant_short_override)
                    .map(|(net, v)| (IpNet::V6(net), v))
            }
        }
    }
}

fn insert_v4<V, A: Allocator>(
    arena: &mut Vec<TrieNode<V>, A>,
    net: Ipv4Net,
    value: V,
) -> Result<Option<V>, AllocFailure> {
    if arena.is_empty()
        && let Err(refused) = arena.try_push(TrieNode::empty())
    {
        return Err(refused.why);
    }

    let mut cur = 0usize;
    for depth in 0..net.prefix {
        let bit_is_one = ((net.addr >> (IPV4_BITS - 1 - depth)) & 1) != 0;
        let existing =
            arena.get(cur).and_then(|node| if bit_is_one { node.right } else { node.left });
        if let Some(child) = existing {
            cur = child;
        } else {
            let new_idx = arena.len();
            if let Err(refused) = arena.try_push(TrieNode::empty()) {
                return Err(refused.why);
            }
            if let Some(parent) = arena.get_mut(cur) {
                if bit_is_one {
                    parent.right = Some(new_idx);
                } else {
                    parent.left = Some(new_idx);
                }
            }
            cur = new_idx;
        }
    }

    Ok(arena.get_mut(cur).and_then(|node| node.value.replace(value)))
}

fn insert_v6<V, A: Allocator>(
    arena: &mut Vec<TrieNode<V>, A>,
    net: Ipv6Net,
    value: V,
) -> Result<Option<V>, AllocFailure> {
    if arena.is_empty()
        && let Err(refused) = arena.try_push(TrieNode::empty())
    {
        return Err(refused.why);
    }

    let mut cur = 0usize;
    for depth in 0..net.prefix {
        let bit_is_one = ((net.addr >> (IPV6_BITS - 1 - depth)) & 1) != 0;
        let existing =
            arena.get(cur).and_then(|node| if bit_is_one { node.right } else { node.left });
        if let Some(child) = existing {
            cur = child;
        } else {
            let new_idx = arena.len();
            if let Err(refused) = arena.try_push(TrieNode::empty()) {
                return Err(refused.why);
            }
            if let Some(parent) = arena.get_mut(cur) {
                if bit_is_one {
                    parent.right = Some(new_idx);
                } else {
                    parent.left = Some(new_idx);
                }
            }
            cur = new_idx;
        }
    }

    Ok(arena.get_mut(cur).and_then(|node| node.value.replace(value)))
}

fn longest_match_v4<V>(
    arena: &[TrieNode<V>],
    addr: Ipv4Addr,
    plant_short_override: bool,
) -> Option<(Ipv4Net, &V)> {
    let root = arena.first()?;
    let bits = u32::from_be_bytes(addr.octets());
    let mut best: Option<(u8, &V)> = root.value.as_ref().map(|v| (0, v));
    let mut cur = 0usize;

    for depth in 0..IPV4_BITS {
        let Some(node) = arena.get(cur) else {
            break;
        };
        let bit_is_one = ((bits >> (IPV4_BITS - 1 - depth)) & 1) != 0;
        let next = if bit_is_one { node.right } else { node.left };
        let Some(child_idx) = next else {
            break;
        };
        cur = child_idx;
        if let Some(child) = arena.get(cur)
            && let Some(ref val) = child.value
            && (!plant_short_override || best.is_none())
        {
            best = Some((depth + 1, val));
        }
    }

    best.map(|(prefix, val)| (Ipv4Net { addr: bits & v4_mask(prefix), prefix }, val))
}

fn longest_match_v6<V>(
    arena: &[TrieNode<V>],
    addr: Ipv6Addr,
    plant_short_override: bool,
) -> Option<(Ipv6Net, &V)> {
    let root = arena.first()?;
    let bits = u128::from_be_bytes(addr.octets());
    let mut best: Option<(u8, &V)> = root.value.as_ref().map(|v| (0, v));
    let mut cur = 0usize;

    for depth in 0..IPV6_BITS {
        let Some(node) = arena.get(cur) else {
            break;
        };
        let bit_is_one = ((bits >> (IPV6_BITS - 1 - depth)) & 1) != 0;
        let next = if bit_is_one { node.right } else { node.left };
        let Some(child_idx) = next else {
            break;
        };
        cur = child_idx;
        if let Some(child) = arena.get(cur)
            && let Some(ref val) = child.value
            && (!plant_short_override || best.is_none())
        {
            best = Some((depth + 1, val));
        }
    }

    best.map(|(prefix, val)| (Ipv6Net { addr: bits & v6_mask(prefix), prefix }, val))
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;
    use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use blaze_own::testing::heap;
    use blaze_own::{Vec as OwnVec, format_in};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::{
        CidrError, IpNet, Ipv4Net, Ipv6Net, PrefixMap, is_globally_routable,
        is_globally_routable_v4, is_globally_routable_v6,
    };

    #[test]
    fn ipv4_cidr_parse_display_and_arithmetic() {
        let h = heap();
        let alloc = h.alloc();

        // Parse canonicalizes host bits.
        let net = match Ipv4Net::parse("192.168.1.42/24") {
            Ok(n) => n,
            Err(e) => panic!("parse failed: {e}"),
        };
        assert_eq!(net.network(), Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(net.broadcast(), Ipv4Addr::new(192, 168, 1, 255));
        assert_eq!(net.netmask(), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(net.prefix_len(), 24);
        assert_eq!(net.host_count(), 256);
        assert_eq!(format_in!(alloc, "{net}").as_str(), "192.168.1.0/24");

        // Strict parse refuses non-zero host bits.
        assert_eq!(Ipv4Net::parse_strict("192.168.1.42/24"), Err(CidrError::NonZeroHostBits));
        assert_eq!(Ipv4Net::parse_strict("192.168.1.0/24"), Ok(net));

        // Edge prefixes: /0 and /32.
        let all = match Ipv4Net::parse("0.0.0.0/0") {
            Ok(n) => n,
            Err(e) => panic!("parse /0 failed: {e}"),
        };
        assert_eq!(all.netmask(), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(all.broadcast(), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(all.host_count(), 1u128 << 32);
        assert_eq!(all.supernet(), None);
        assert!(all.contains_net(net));
        assert!(all.overlaps(net));

        let host = match Ipv4Net::parse("192.168.1.99/32") {
            Ok(n) => n,
            Err(e) => panic!("parse /32 failed: {e}"),
        };
        assert_eq!(host.broadcast(), Ipv4Addr::new(192, 168, 1, 99));
        assert_eq!(host.host_count(), 1);
        assert_eq!(host.subnets(), None);
        assert!(net.contains_net(host));
        assert!(!host.contains_net(net));
        assert!(net.contains_ip(Ipv4Addr::new(192, 168, 1, 99)));
        assert!(!net.contains_ip(Ipv4Addr::new(192, 168, 2, 1)));

        // Subnets and supernet.
        let (sub_a, sub_b) = match net.subnets() {
            Some(pair) => pair,
            None => panic!("expected /25 subnets"),
        };
        assert_eq!(format_in!(alloc, "{sub_a}").as_str(), "192.168.1.0/25");
        assert_eq!(format_in!(alloc, "{sub_b}").as_str(), "192.168.1.128/25");
        assert!(!sub_a.overlaps(sub_b));
        assert_eq!(sub_a.supernet(), Some(net));
        assert_eq!(sub_b.supernet(), Some(net));

        // Strict parsing refusals.
        assert_eq!(Ipv4Net::parse("10.0.0.0"), Err(CidrError::MissingSlash));
        assert_eq!(
            Ipv4Net::parse("10.0.0.0/33"),
            Err(CidrError::PrefixTooLong { prefix: 33, max: 32 })
        );
        assert_eq!(Ipv4Net::parse("10.0.0.0/08"), Err(CidrError::InvalidPrefixLen));
        assert_eq!(Ipv4Net::parse("10.0.0.0/00"), Err(CidrError::InvalidPrefixLen));
        assert_eq!(Ipv4Net::parse("10.0.0.0/+8"), Err(CidrError::InvalidPrefixLen));
        assert_eq!(Ipv4Net::parse("10.0.0.0/8 "), Err(CidrError::InvalidPrefixLen));
        assert_eq!(Ipv4Net::parse("010.0.0.0/8"), Err(CidrError::InvalidAddress));
    }

    #[test]
    fn ipv6_cidr_parse_display_and_arithmetic() {
        let h = heap();
        let alloc = h.alloc();

        let net = match Ipv6Net::parse("2001:db8::abcd/32") {
            Ok(n) => n,
            Err(e) => panic!("parse v6 failed: {e}"),
        };
        assert_eq!(format_in!(alloc, "{net}").as_str(), "2001:db8::/32");
        assert_eq!(net.prefix_len(), 32);
        assert_eq!(net.host_count(), 1u128 << 96);
        assert_eq!(Ipv6Net::parse_strict("2001:db8::abcd/32"), Err(CidrError::NonZeroHostBits));
        assert_eq!(Ipv6Net::parse_strict("2001:db8::/32"), Ok(net));

        let default_v6 = match Ipv6Net::parse("::/0") {
            Ok(n) => n,
            Err(e) => panic!("parse ::/0 failed: {e}"),
        };
        assert_eq!(default_v6.host_count(), u128::MAX);
        assert_eq!(default_v6.supernet(), None);
        assert!(default_v6.contains_net(net));

        let host_v6 = match Ipv6Net::parse("::1/128") {
            Ok(n) => n,
            Err(e) => panic!("parse ::1/128 failed: {e}"),
        };
        assert_eq!(host_v6.host_count(), 1);
        assert_eq!(host_v6.subnets(), None);

        let (lo, hi) = match net.subnets() {
            Some(pair) => pair,
            None => panic!("expected /33 subnets"),
        };
        assert_eq!(lo.prefix_len(), 33);
        assert_eq!(hi.prefix_len(), 33);
        assert!(!lo.overlaps(hi));
        assert_eq!(lo.supernet(), Some(net));
        assert_eq!(hi.supernet(), Some(net));

        assert_eq!(
            Ipv6Net::parse("2001:db8::/129"),
            Err(CidrError::PrefixTooLong { prefix: 129, max: 128 })
        );
        assert_eq!(Ipv6Net::parse("2001:db8::/032"), Err(CidrError::InvalidPrefixLen));

        // IpNet cross-family checks.
        let ip_v4 = match IpNet::parse("10.0.0.0/8") {
            Ok(n) => n,
            Err(e) => panic!("parse IpNet v4 failed: {e}"),
        };
        let ip_v6 = IpNet::V6(net);
        assert!(!ip_v4.overlaps(ip_v6));
        assert!(!ip_v4.contains_net(ip_v6));
        assert!(!ip_v4.contains_ip(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))));
    }

    #[test]
    fn ssrf_global_routability_vectors() {
        // Blocked IPv4 ranges.
        assert!(!is_globally_routable_v4(Ipv4Addr::new(0, 0, 0, 0)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(100, 127, 255, 255)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(169, 254, 169, 254)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(172, 16, 0, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(172, 31, 255, 254)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(192, 0, 0, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(192, 0, 2, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(198, 19, 255, 255)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(198, 51, 100, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(203, 0, 113, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(224, 0, 0, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(240, 0, 0, 1)));
        assert!(!is_globally_routable_v4(Ipv4Addr::new(255, 255, 255, 255)));

        // Globally routable IPv4 addresses (including boundary neighbors).
        assert!(is_globally_routable_v4(Ipv4Addr::new(1, 1, 1, 1)));
        assert!(is_globally_routable_v4(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(is_globally_routable_v4(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(is_globally_routable_v4(Ipv4Addr::new(100, 128, 0, 0)));
        assert!(is_globally_routable_v4(Ipv4Addr::new(172, 15, 255, 255)));
        assert!(is_globally_routable_v4(Ipv4Addr::new(172, 32, 0, 0)));
        assert!(is_globally_routable_v4(Ipv4Addr::new(198, 17, 255, 255)));
        assert!(is_globally_routable_v4(Ipv4Addr::new(198, 20, 0, 0)));
        assert!(is_globally_routable_v4(Ipv4Addr::new(223, 255, 255, 255)));

        // Blocked IPv6 ranges (including IPv4-mapped SSRF bypasses).
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xFFFF, 0x7F00, 0x0001)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xFFFF, 0xA9FE, 0xA9FE)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0xFC00, 0, 0, 0, 0, 0, 0, 1)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0xFD12, 0x3456, 0, 0, 0, 0, 0, 1)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0xFF02, 0, 0, 0, 0, 0, 0, 1)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0x2001, 0x0DB8, 0, 0, 0, 0, 0, 1)));
        assert!(!is_globally_routable_v6(Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, 0, 1)));

        // Globally routable IPv6 addresses (including public IPv4-mapped).
        assert!(is_globally_routable_v6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)));
        assert!(is_globally_routable_v6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xFFFF, 0x0101, 0x0101)));

        // Unified IpAddr dispatcher.
        assert!(is_globally_routable(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_globally_routable(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        assert!(is_globally_routable(IpAddr::V6(Ipv6Addr::new(
            0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111
        ))));
        assert!(!is_globally_routable(IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0xFFFF, 0x7F00, 0x0001
        ))));
    }

    fn draw_v4(tape: &Tape) -> Ipv4Addr {
        let mut oct = [0u8; 4];
        tape.fill(&mut oct, "v4");
        let [a, b, c, d] = oct;
        Ipv4Addr::new(a, b, c, d)
    }

    fn draw_v6(tape: &Tape) -> Ipv6Addr {
        let mut oct = [0u8; 16];
        tape.fill(&mut oct, "v6");
        Ipv6Addr::from(oct)
    }

    fn oracle_insert<A: Allocator>(
        entries: &mut OwnVec<(IpNet, u32), A>,
        net: IpNet,
        val: u32,
    ) -> Option<u32> {
        for (existing_net, existing_val) in entries.as_mut_slice() {
            if *existing_net == net {
                let old = *existing_val;
                *existing_val = val;
                return Some(old);
            }
        }
        entries.push((net, val));
        None
    }

    fn oracle_longest_match(entries: &[(IpNet, u32)], ip: IpAddr) -> Option<(IpNet, u32)> {
        let mut best: Option<(IpNet, u32)> = None;
        for &(net, val) in entries {
            if net.contains_ip(ip)
                && best.map_or(true, |(cur, _)| net.prefix_len() > cur.prefix_len())
            {
                best = Some((net, val));
            }
        }
        best
    }

    fn run_prefix_map_oracle<A: Allocator + Copy>(
        tape: &Tape,
        plant_short_override: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let mut map = PrefixMap::new_in(alloc);
        let mut linear = OwnVec::new_in(alloc);

        // Anchor IPs so overlapping prefixes of multiple lengths always occur.
        let anchor_v4 = draw_v4(tape);
        let anchor_v6 = draw_v6(tape);

        let entry_count = tape.int_in(2usize..=24, "entry_count");
        for i in 0..entry_count {
            let val = u32::try_from(i).unwrap_or(0) + 1;
            let net = if tape.coin("is_v6") {
                let ip = if tape.coin("use_anchor_v6") { anchor_v6 } else { draw_v6(tape) };
                let prefix = tape.int_in(0u8..=128, "v6_prefix");
                match Ipv6Net::new(ip, prefix) {
                    Ok(n) => IpNet::V6(n),
                    Err(e) => {
                        return Err(Broke::Oracle(
                            format_in!(alloc, "Ipv6Net::new failed: {e}").as_str().into(),
                        ));
                    }
                }
            } else {
                let ip = if tape.coin("use_anchor_v4") { anchor_v4 } else { draw_v4(tape) };
                let prefix = tape.int_in(0u8..=32, "v4_prefix");
                match Ipv4Net::new(ip, prefix) {
                    Ok(n) => IpNet::V4(n),
                    Err(e) => {
                        return Err(Broke::Oracle(
                            format_in!(alloc, "Ipv4Net::new failed: {e}").as_str().into(),
                        ));
                    }
                }
            };

            let got_prev = map.insert(net, val).map_err(|e| {
                Broke::Oracle(format_in!(alloc, "PrefixMap::insert failed: {e}").as_str().into())
            })?;
            let exp_prev = oracle_insert(&mut linear, net, val);
            if got_prev != exp_prev {
                return Err(Broke::Oracle("insert previous value mismatch".into()));
            }
            if map.len() != linear.len() || map.is_empty() != linear.is_empty() {
                return Err(Broke::Oracle("len/is_empty mismatch".into()));
            }
        }

        // Query anchors and random IPs from both families.
        let mut queries = OwnVec::new_in(alloc);
        queries.push(IpAddr::V4(anchor_v4));
        queries.push(IpAddr::V6(anchor_v6));
        let extra = tape.int_in(2usize..=12, "query_count");
        for _ in 0..extra {
            if tape.coin("query_v6") {
                queries.push(IpAddr::V6(draw_v6(tape)));
            } else {
                queries.push(IpAddr::V4(draw_v4(tape)));
            }
        }

        for &ip in queries.as_slice() {
            let got = map
                .longest_match_planted(ip, plant_short_override)
                .map(|(n, &v)| (n, v));
            let expected = oracle_longest_match(linear.as_slice(), ip);
            if got != expected {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "longest_match mismatch for {ip}: got {got:?}, expected {expected:?}"
                    )
                    .as_str()
                    .into(),
                ));
            }
        }

        Ok(())
    }

    /// The invariant: PrefixMap longest-prefix lookup always matches a linear brute-force scan over all inserted IPv4 and IPv6 prefixes.
    #[test]
    fn property_prefix_map_longest_match_matches_oracle() {
        let h = heap();
        let alloc = h.alloc();
        check(Config::cases(256), |tape| run_prefix_map_oracle(tape, false, alloc));
    }

    #[test]
    fn the_oracle_catches_short_prefix_override() {
        let h = heap();
        let alloc = h.alloc();
        control(
            Config::cases(64),
            false,
            true,
            "longest_match mismatch",
            |tape, plant| run_prefix_map_oracle(tape, plant, alloc),
        );
    }
}
