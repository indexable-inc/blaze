//! Deterministic profile-guided optimization (PGO) summary bridging `blaze-profile` hot-key
//! sketches and call-site counters to `blaze-lir` and the JIT.
//!
//! Zero external `crates.io` dependencies: every aggregate uses integer arithmetic (`share_ppm` in
//! `0..=1_000_000`) and a total canonical sort order (`weight DESC, count DESC, symbol ASC,
//! site ASC`) so identical profiles produce byte-identical summaries on every machine.

use std::{collections::BTreeMap, fmt};

use crate::hot::{Function, Hot};

/// Binary format magic for serialized [`PgoProfile`] summaries.
pub const PGO_MAGIC: &[u8; 4] = b"BPGO";
/// Current binary format version.
pub const PGO_VERSION: u32 = 1;
/// Scale for [`SiteWeight::share_ppm`]: `1_000_000` represents 100.0%.
pub const PPM_SCALE: u32 = 1_000_000;

/// Profile weight of one hot function or call site.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SiteWeight {
    /// Stable function symbol or canonical item path.
    pub symbol: String,
    /// Optional call-site or span discriminator (empty for whole-function aggregates).
    pub site: String,
    /// Observed invocation / sample count.
    pub count: u64,
    /// Observed cost weight (e.g. ticks or nanoseconds).
    pub weight: u64,
    /// Normalized share of [`PgoProfile::total_weight`] in parts per million (`0..=1_000_000`).
    pub share_ppm: u32,
}

impl SiteWeight {
    /// Normalized share in `[0.0, 1.0]`.
    #[inline]
    pub fn share(&self) -> f64 {
        f64::from(self.share_ppm) / f64::from(PPM_SCALE)
    }
}

/// Deterministic, canonically sorted PGO summary consumable by `blaze-lir` and the JIT.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PgoProfile {
    /// Total observed weight across all sites (or the sketch total when bridged from [`Hot`]).
    pub total_weight: u64,
    /// Total observed sample / call count.
    pub total_count: u64,
    /// Sites sorted deterministically by `(weight DESC, count DESC, symbol ASC, site ASC)`.
    pub sites: Vec<SiteWeight>,
}

/// Why a byte buffer could not be decoded as a [`PgoProfile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgoError {
    Short,
    Magic,
    Version(u32),
    InvalidUtf8,
    Trailing,
}

impl fmt::Display for PgoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PgoError::Short => f.write_str("PGO profile bytes ended unexpectedly"),
            PgoError::Magic => f.write_str("PGO profile magic is not BPGO"),
            PgoError::Version(v) => write!(f, "unsupported PGO profile version {v}"),
            PgoError::InvalidUtf8 => f.write_str("PGO profile contains invalid UTF-8"),
            PgoError::Trailing => f.write_str("PGO profile has unexpected trailing bytes"),
        }
    }
}

impl core::error::Error for PgoError {}

fn compute_share_ppm(weight: u64, total_weight: u64) -> u32 {
    if total_weight == 0 || weight == 0 {
        return 0;
    }
    let scaled = (u128::from(weight) * u128::from(PPM_SCALE)) / u128::from(total_weight);
    u32::try_from(scaled.min(u128::from(PPM_SCALE))).unwrap_or(PPM_SCALE)
}

fn canonical_sort(sites: &mut [SiteWeight]) {
    sites.sort_by(|a, b| {
        b.weight
            .cmp(&a.weight)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.symbol.cmp(&b.symbol))
            .then_with(|| a.site.cmp(&b.site))
    });
}

impl PgoProfile {
    /// An empty profile with zero weight and no sites.
    pub fn new() -> PgoProfile {
        PgoProfile::default()
    }

    /// Builds a deterministic [`PgoProfile`] from raw `(symbol, site, count, weight)` observations,
    /// aggregating duplicate `(symbol, site)` keys and sorting canonically.
    pub fn from_samples<I, S1, S2>(samples: I) -> PgoProfile
    where
        I: IntoIterator<Item = (S1, S2, u64, u64)>,
        S1: Into<String>,
        S2: Into<String>,
    {
        let mut by_key: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
        for (symbol, site, count, weight) in samples {
            let entry = by_key.entry((symbol.into(), site.into())).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(count);
            entry.1 = entry.1.saturating_add(weight);
        }
        Self::from_aggregated(by_key, None, None)
    }

    /// Bridges a live [`Hot`] sketch summary into a deterministic [`PgoProfile`].
    pub fn from_hot(hot: &Hot) -> PgoProfile {
        let mut by_key: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
        for key in &hot.keys {
            let symbol = match &key.function {
                Function::Named(name) => name.as_ref().to_owned(),
                Function::None => String::new(),
                Function::Forgotten(sym) => format!("<forgotten:{sym}>"),
            };
            let site = key.span.to_owned();
            let weight = key.upper_ns.max(key.lower_ns);
            let entry = by_key.entry((symbol, site)).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(1);
            entry.1 = entry.1.saturating_add(weight);
        }
        let sum_weight: u64 = by_key.values().fold(0, |acc, &(_, w)| acc.saturating_add(w));
        let total_weight = hot.total_ns.max(sum_weight);
        Self::from_aggregated(by_key, Some(total_weight), Some(hot.spans))
    }

    fn from_aggregated(
        by_key: BTreeMap<(String, String), (u64, u64)>,
        explicit_weight: Option<u64>,
        explicit_count: Option<u64>,
    ) -> PgoProfile {
        let sum_count = by_key.values().fold(0u64, |acc, &(c, _)| acc.saturating_add(c));
        let sum_weight = by_key.values().fold(0u64, |acc, &(_, w)| acc.saturating_add(w));
        let total_weight = explicit_weight.unwrap_or(sum_weight).max(sum_weight);
        let total_count = explicit_count.unwrap_or(sum_count).max(sum_count);

        let mut sites: Vec<SiteWeight> = by_key
            .into_iter()
            .map(|((symbol, site), (count, weight))| SiteWeight {
                symbol,
                site,
                count,
                weight,
                share_ppm: compute_share_ppm(weight, total_weight),
            })
            .collect();
        canonical_sort(&mut sites);
        PgoProfile { total_weight, total_count, sites }
    }

    /// Associative, commutative, deterministic merge of two profiles.
    pub fn merge(&self, other: &PgoProfile) -> PgoProfile {
        let mut by_key: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
        for site in self.sites.iter().chain(&other.sites) {
            let entry = by_key
                .entry((site.symbol.clone(), site.site.clone()))
                .or_insert((0, 0));
            entry.0 = entry.0.saturating_add(site.count);
            entry.1 = entry.1.saturating_add(site.weight);
        }
        let total_weight = self.total_weight.saturating_add(other.total_weight);
        let total_count = self.total_count.saturating_add(other.total_count);
        Self::from_aggregated(by_key, Some(total_weight), Some(total_count))
    }

    /// Looks up the heaviest entry for `symbol` (regardless of `site`).
    pub fn weight_of(&self, symbol: &str) -> Option<&SiteWeight> {
        self.sites.iter().find(|it| it.symbol == symbol)
    }

    /// Looks up the exact `(symbol, site)` entry.
    pub fn site_weight(&self, symbol: &str, site: &str) -> Option<&SiteWeight> {
        self.sites.iter().find(|it| it.symbol == symbol && it.site == site)
    }

    /// Returns all sites whose `share_ppm >= min_share_ppm`, in descending weight order.
    pub fn hot_sites(&self, min_share_ppm: u32) -> impl Iterator<Item = &SiteWeight> {
        // `sites` is sorted by `weight DESC`, so `share_ppm` is non-increasing.
        self.sites.iter().take_while(move |it| it.share_ppm >= min_share_ppm)
    }

    /// Whether `symbol` has at least one site meeting `min_share_ppm`.
    pub fn is_hot(&self, symbol: &str, min_share_ppm: u32) -> bool {
        self.hot_sites(min_share_ppm).any(|it| it.symbol == symbol)
    }

    /// Canonical, deterministic little-endian binary encoding.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(PGO_MAGIC);
        out.extend_from_slice(&PGO_VERSION.to_le_bytes());
        out.extend_from_slice(&self.total_weight.to_le_bytes());
        out.extend_from_slice(&self.total_count.to_le_bytes());
        let len = u32::try_from(self.sites.len()).expect("PGO profile site count fits u32");
        out.extend_from_slice(&len.to_le_bytes());
        for site in &self.sites {
            let sym_len = u32::try_from(site.symbol.len()).expect("symbol length fits u32");
            out.extend_from_slice(&sym_len.to_le_bytes());
            out.extend_from_slice(site.symbol.as_bytes());
            let site_len = u32::try_from(site.site.len()).expect("site length fits u32");
            out.extend_from_slice(&site_len.to_le_bytes());
            out.extend_from_slice(site.site.as_bytes());
            out.extend_from_slice(&site.count.to_le_bytes());
            out.extend_from_slice(&site.weight.to_le_bytes());
        }
        out
    }

    /// Decodes bytes written by [`PgoProfile::to_bytes`], recomputing normalized shares and
    /// canonical ordering.
    pub fn from_bytes(bytes: &[u8]) -> Result<PgoProfile, PgoError> {
        let mut cursor = bytes;
        fn take<'a>(cursor: &mut &'a [u8], n: usize) -> Result<&'a [u8], PgoError> {
            if cursor.len() < n {
                return Err(PgoError::Short);
            }
            let (head, tail) = cursor.split_at(n);
            *cursor = tail;
            Ok(head)
        }
        fn u32_le(cursor: &mut &[u8]) -> Result<u32, PgoError> {
            let b = take(cursor, 4)?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }
        fn u64_le(cursor: &mut &[u8]) -> Result<u64, PgoError> {
            let b = take(cursor, 8)?;
            Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        }

        let magic = take(&mut cursor, 4)?;
        if magic != PGO_MAGIC {
            return Err(PgoError::Magic);
        }
        let version = u32_le(&mut cursor)?;
        if version != PGO_VERSION {
            return Err(PgoError::Version(version));
        }
        let total_weight = u64_le(&mut cursor)?;
        let total_count = u64_le(&mut cursor)?;
        let len = u32_le(&mut cursor)? as usize;

        let mut by_key: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
        for _ in 0..len {
            let sym_len = u32_le(&mut cursor)? as usize;
            let sym_bytes = take(&mut cursor, sym_len)?;
            let symbol = std::str::from_utf8(sym_bytes).map_err(|_| PgoError::InvalidUtf8)?.to_owned();
            let site_len = u32_le(&mut cursor)? as usize;
            let site_bytes = take(&mut cursor, site_len)?;
            let site = std::str::from_utf8(site_bytes).map_err(|_| PgoError::InvalidUtf8)?.to_owned();
            let count = u64_le(&mut cursor)?;
            let weight = u64_le(&mut cursor)?;
            let entry = by_key.entry((symbol, site)).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(count);
            entry.1 = entry.1.saturating_add(weight);
        }
        if !cursor.is_empty() {
            return Err(PgoError::Trailing);
        }
        Ok(Self::from_aggregated(by_key, Some(total_weight), Some(total_count)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{PPM_SCALE, PgoError, PgoProfile};
    use crate::hot::{Function, Hot, HotKey, Since};

    #[test]
    fn from_samples_is_order_independent_and_canonically_sorted() {
        let a = PgoProfile::from_samples([
            ("foo::hot", "call@1", 10, 600),
            ("bar::cold", "call@2", 2, 100),
            ("foo::hot", "call@1", 5, 200),
            ("baz::tie", "call@3", 4, 100),
        ]);
        let b = PgoProfile::from_samples([
            ("baz::tie", "call@3", 4, 100),
            ("foo::hot", "call@1", 5, 200),
            ("bar::cold", "call@2", 2, 100),
            ("foo::hot", "call@1", 10, 600),
        ]);
        assert_eq!(a, b, "profile must be independent of input order");
        assert_eq!(a.total_weight, 1000);
        assert_eq!(a.total_count, 21);
        assert_eq!(a.sites.len(), 3);
        // Sorted by weight DESC, then count DESC, then symbol ASC.
        assert_eq!(a.sites[0].symbol, "foo::hot");
        assert_eq!((a.sites[0].count, a.sites[0].weight, a.sites[0].share_ppm), (15, 800, 800_000));
        assert_eq!(a.sites[1].symbol, "baz::tie");
        assert_eq!(a.sites[1].share_ppm, 100_000);
        assert_eq!(a.sites[2].symbol, "bar::cold");
        assert_eq!(a.sites[2].share_ppm, 100_000);

        // Hot threshold filtering.
        let hot: Vec<&str> = a.hot_sites(500_000).map(|s| s.symbol.as_str()).collect();
        assert_eq!(hot, ["foo::hot"]);
        assert!(a.is_hot("foo::hot", 500_000));
        assert!(!a.is_hot("bar::cold", 500_000));
    }

    #[test]
    fn from_hot_bridges_sketch_summary_deterministically() {
        let hot = Hot {
            since: Since::LastBuild,
            spans: 120,
            total_ns: 1_000_000,
            error_ns: 500,
            keys: vec![
                HotKey {
                    span: "codegen",
                    function: Function::Named(Arc::from("lir::lower_fn")),
                    upper_ns: 700_000,
                    lower_ns: 650_000,
                },
                HotKey {
                    span: "parse",
                    function: Function::None,
                    upper_ns: 200_000,
                    lower_ns: 200_000,
                },
                HotKey {
                    span: "jit",
                    function: Function::Forgotten(42),
                    upper_ns: 100_000,
                    lower_ns: 90_000,
                },
            ],
            waits: vec![],
            torn: 0,
        };
        let pgo = PgoProfile::from_hot(&hot);
        assert_eq!(pgo.total_weight, 1_000_000);
        assert_eq!(pgo.total_count, 120);
        assert_eq!(pgo.sites.len(), 3);
        let top = pgo.weight_of("lir::lower_fn").expect("top function present");
        assert_eq!(top.site, "codegen");
        assert_eq!(top.share_ppm, 700_000);
        assert!((top.share() - 0.7).abs() < 1e-9);
        assert_eq!(pgo.site_weight("<forgotten:42>", "jit").unwrap().share_ppm, 100_000);
    }

    #[test]
    fn merge_is_commutative_and_bytes_round_trip() {
        let p1 = PgoProfile::from_samples([("a", "s1", 3, 300), ("b", "s2", 1, 100)]);
        let p2 = PgoProfile::from_samples([("b", "s2", 4, 400), ("c", "s3", 2, 200)]);
        let m12 = p1.merge(&p2);
        let m21 = p2.merge(&p1);
        assert_eq!(m12, m21);
        assert_eq!(m12.total_weight, 1000);
        assert_eq!(m12.sites[0].symbol, "b");
        assert_eq!(m12.sites[0].share_ppm, 500_000);
        assert!(m12.sites[0].share_ppm <= PPM_SCALE);

        let bytes = m12.to_bytes();
        let decoded = PgoProfile::from_bytes(&bytes).expect("valid PGO bytes");
        assert_eq!(decoded, m12);
        assert_eq!(decoded.to_bytes(), bytes, "canonical re-encoding is byte-identical");

        // Error handling controls.
        assert_eq!(PgoProfile::from_bytes(&bytes[..3]), Err(PgoError::Short));
        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert_eq!(PgoProfile::from_bytes(&bad_magic), Err(PgoError::Magic));
        let mut bad_ver = bytes.clone();
        bad_ver[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(PgoProfile::from_bytes(&bad_ver), Err(PgoError::Version(99)));
        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(PgoProfile::from_bytes(&trailing), Err(PgoError::Trailing));
    }
}
