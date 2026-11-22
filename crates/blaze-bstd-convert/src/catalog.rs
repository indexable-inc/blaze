//! Catalog of `std` and third-party crate mappings onto `bstd`, `blaze-*`, and `core`,
//! plus classification of IO / effectful operations for the sans-IO state-machine compiler.

use alloc::format;

/// How a third-party crate can be handled when moving a workspace onto `bstd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepDisposition {
    /// Clean, direct replacement in `bstd` or a zero-dep `blaze-*` workspace crate (or inline `core`).
    DirectBstd,
    /// Multiple valid strategies exist (replace, wrap behind a sans-IO capability trait, or keep).
    /// Always surfaced as a structured `Question` to the user.
    NeedsDecision,
    /// Specialized domain, legacy crypto, or C-FFI crate with no drop-in `bstd` replacement yet.
    /// Defaults to keeping external (or isolating behind a sans-IO boundary if the user asks).
    KeepExternal,
    /// Crate not yet in the built-in catalog: surfaced as a `Question` so the user decides.
    Unknown,
}

/// Which `bstd` cargo feature (if any) a replacement activates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BstdFeature {
    /// Default `bstd` (no extra feature needed).
    Core,
    /// `bstd` feature `requests` (HTTP/1.1 sans-IO client over `Net`).
    Requests,
    /// `bstd` feature `net` (QUIC + full HTTP client + TLS).
    Net,
    /// `bstd` feature `image` (PNG / zlib / deflate).
    Image,
    /// `bstd` feature `sim-draws` (property testing with `blaze-sim`, dev-dep only).
    SimDraws,
}

impl BstdFeature {
    pub const fn as_cargo_feature(self) -> Option<&'static str> {
        match self {
            Self::Core => None,
            Self::Requests => Some("requests"),
            Self::Net => Some("net"),
            Self::Image => Some("image"),
            Self::SimDraws => Some("sim-draws"),
        }
    }
}

/// One catalog entry for a third-party crate.
#[derive(Debug, Clone, Copy)]
pub struct CrateMapping {
    /// Cargo package name (hyphenated, e.g. `"serde_json"` or `"rustc-hash"`).
    pub package: &'static str,
    /// Rust crate identifier (underscored, e.g. `"serde_json"` or `"rustc_hash"`).
    pub rust_ident: &'static str,
    /// Disposition tier.
    pub disposition: DepDisposition,
    /// Primary `bstd` or `blaze-*` target path/crate.
    pub target: &'static str,
    /// Optional workspace crate to add alongside `bstd` (empty if `bstd` alone covers it).
    pub extra_blaze_crate: Option<&'static str>,
    /// `bstd` feature needed (if any).
    pub bstd_feature: BstdFeature,
    /// Short rationale shown in reports and decision prompts.
    pub summary: &'static str,
    /// Suggested code-level rewrite hints `(old_path_prefix, new_path_prefix)`.
    pub path_rewrites: &'static [(&'static str, &'static str)],
}

/// Complete catalog of common third-party crates, grounded in real workspaces like `hyperion`.
pub const CRATE_CATALOG: &[CrateMapping] = &[
    // --- Direct bstd / blaze-* / zero-dep replacements ---
    CrateMapping {
        package: "bytes",
        rust_ident: "bytes",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::bytes::Bytes",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Zero-copy reference-counted byte slices live in `bstd::bytes::Bytes` (`blaze-bytes`).",
        path_rewrites: &[("bytes::Bytes", "bstd::bytes::Bytes"), ("bytes::", "bstd::bytes::")],
    },
    CrateMapping {
        package: "rustc-hash",
        rust_ident: "rustc_hash",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::map::Map<K, V, bstd::hash::FixedState, A>",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Fast non-cryptographic maps live in `bstd::map::Map` with `bstd::hash::FixedState`.",
        path_rewrites: &[
            ("rustc_hash::FxHashMap", "bstd::map::HashMap"),
            ("rustc_hash::FxHashSet", "bstd::set::HashSet"),
            ("rustc_hash::FxHasher", "bstd::hash::FixedHasher"),
        ],
    },
    CrateMapping {
        package: "foldhash",
        rust_ident: "foldhash",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::hash::FixedState",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "`bstd::hash` already ships foldhash as `FixedState` / `FixedHasher`.",
        path_rewrites: &[("foldhash::fast::FixedState", "bstd::hash::FixedState"), ("foldhash::", "bstd::hash::")],
    },
    CrateMapping {
        package: "hashbrown",
        rust_ident: "hashbrown",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::map::Map",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "`bstd::map::Map` / `bstd::set::Set` wrap `blaze-swiss` directly.",
        path_rewrites: &[
            ("hashbrown::HashMap", "bstd::map::HashMap"),
            ("hashbrown::HashSet", "bstd::set::HashSet"),
        ],
    },
    CrateMapping {
        package: "equivalent",
        rust_ident: "equivalent",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::map::Equivalent",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Re-exported directly by `bstd::map::Equivalent`.",
        path_rewrites: &[("equivalent::Equivalent", "bstd::map::Equivalent")],
    },
    CrateMapping {
        package: "indexmap",
        rust_ident: "indexmap",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::map::OrderMap",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Insertion-ordered hash maps/sets live in `bstd::map::OrderMap` / `bstd::set::OrderSet`.",
        path_rewrites: &[
            ("indexmap::IndexMap", "bstd::map::OrderMap"),
            ("indexmap::IndexSet", "bstd::set::OrderSet"),
        ],
    },
    CrateMapping {
        package: "smallvec",
        rust_ident: "smallvec",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::vec::InlineVec",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Inline-first vectors with explicit allocator live in `bstd::vec::InlineVec`.",
        path_rewrites: &[("smallvec::SmallVec", "bstd::vec::InlineVec")],
    },
    CrateMapping {
        package: "arrayvec",
        rust_ident: "arrayvec",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::vec::FixedVec",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Fixed-capacity inline vectors/strings live in `bstd::vec::FixedVec` / `bstd::text::FixedString`.",
        path_rewrites: &[
            ("arrayvec::ArrayVec", "bstd::vec::FixedVec"),
            ("arrayvec::ArrayString", "bstd::text::FixedString"),
        ],
    },
    CrateMapping {
        package: "blake3",
        rust_ident: "blake3",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::digest::Blake3",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "BLAKE3 hashing is built into `bstd::digest::{Blake3, blake3}`.",
        path_rewrites: &[
            ("blake3::Hasher", "bstd::digest::Blake3"),
            ("blake3::hash", "bstd::digest::blake3"),
            ("blake3::OUT_LEN", "bstd::digest::BLAKE3_LEN"),
        ],
    },
    CrateMapping {
        package: "sha2",
        rust_ident: "sha2",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::digest::Sha256",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "SHA-256 is built into `bstd::digest::{Sha256, sha256}`.",
        path_rewrites: &[("sha2::Sha256", "bstd::digest::Sha256")],
    },
    CrateMapping {
        package: "rand",
        rust_ident: "rand",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::rng::Rng",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Explicit seeded RNG lives in `bstd::rng::Rng` (no ambient `thread_rng`).",
        path_rewrites: &[("rand::Rng", "bstd::rng::Rng"), ("rand::", "bstd::rng::")],
    },
    CrateMapping {
        package: "fastrand",
        rust_ident: "fastrand",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::rng::Rng",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Explicit seeded RNG lives in `bstd::rng::Rng`.",
        path_rewrites: &[("fastrand::Rng", "bstd::rng::Rng")],
    },
    CrateMapping {
        package: "rayon",
        rust_ident: "rayon",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::par",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Deterministic chunked data parallelism lives in `bstd::par::{for_each, map_into, reduce}`.",
        path_rewrites: &[("rayon::", "bstd::par::")],
    },
    CrateMapping {
        package: "tracing",
        rust_ident: "tracing",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::trace",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Spans and events go through `cx: &bstd::Cx` and `bstd::trace` (no ambient global subscriber).",
        path_rewrites: &[("tracing::", "bstd::trace::")],
    },
    CrateMapping {
        package: "log",
        rust_ident: "log",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::trace",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Structured events go through `bstd::trace` via `Cx`.",
        path_rewrites: &[("log::", "bstd::trace::")],
    },
    CrateMapping {
        package: "parking_lot",
        rust_ident: "parking_lot",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::sync::{Lock, ReadLock}",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Named, ranked locks live in `bstd::sync::{Lock, ReadLock}`.",
        path_rewrites: &[
            ("parking_lot::Mutex", "bstd::sync::Lock"),
            ("parking_lot::RwLock", "bstd::sync::ReadLock"),
        ],
    },
    CrateMapping {
        package: "crossbeam-channel",
        rust_ident: "crossbeam_channel",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::sync::Chan",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Bounded channels live in `bstd::sync::Chan`.",
        path_rewrites: &[("crossbeam_channel::bounded", "bstd::sync::Chan::new")],
    },
    CrateMapping {
        package: "serde_json",
        rust_ident: "serde_json",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_json",
        extra_blaze_crate: Some("blaze-json"),
        bstd_feature: BstdFeature::Core,
        summary: "Strict RFC 8259 zero-dep JSON reader/writer over `blaze_data::Value` (`blaze-json`).",
        path_rewrites: &[
            ("serde_json::Value", "blaze_data::Value"),
            ("serde_json::from_str", "blaze_json::read_in"),
            ("serde_json::from_slice", "blaze_json::from_slice_in"),
            ("serde_json::to_string_pretty", "blaze_json::to_string_pretty"),
            ("serde_json::to_string", "blaze_json::to_string_in"),
        ],
    },
    CrateMapping {
        package: "serde",
        rust_ident: "serde",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze_data::{FromValue, ToValue} / bstd::comptime",
        extra_blaze_crate: Some("blaze-data"),
        bstd_feature: BstdFeature::Core,
        summary: "Replace `Serialize`/`Deserialize` derives with `blaze_data::{FromValue, ToValue}` or `bstd::record` / comptime reflection, or keep `serde` for external wire types.",
        path_rewrites: &[
            ("serde::Serialize", "blaze_data::ToValue"),
            ("serde::Deserialize", "blaze_data::FromValue"),
        ],
    },
    CrateMapping {
        package: "serde_ignored",
        rust_ident: "serde_ignored",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_data::Cursor::fields",
        extra_blaze_crate: Some("blaze-data"),
        bstd_feature: BstdFeature::Core,
        summary: "`blaze_data::Cursor::fields` rejects unknown keys strictly by default (no extra crate needed).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "toml",
        rust_ident: "toml",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_toml",
        extra_blaze_crate: Some("blaze-toml"),
        bstd_feature: BstdFeature::Core,
        summary: "Strict zero-dep TOML 1.0 parser and lossless editor (`blaze-toml`).",
        path_rewrites: &[
            ("toml::Table", "blaze_toml::Document"),
            ("toml::Value", "blaze_data::Value"),
            ("toml::from_str", "blaze_toml::Document::parse_in"),
        ],
    },
    CrateMapping {
        package: "toml_edit",
        rust_ident: "toml_edit",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_toml::Edit",
        extra_blaze_crate: Some("blaze-toml"),
        bstd_feature: BstdFeature::Core,
        summary: "Lossless TOML editing lives in `blaze_toml::{Document, Edit}`.",
        path_rewrites: &[("toml_edit::DocumentMut", "blaze_toml::Document")],
    },
    CrateMapping {
        package: "base64",
        rust_ident: "base64",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_base64",
        extra_blaze_crate: Some("blaze-base64"),
        bstd_feature: BstdFeature::Core,
        summary: "Zero-dep base64 encode/decode lives in `blaze-base64`.",
        path_rewrites: &[("base64::", "blaze_base64::")],
    },
    CrateMapping {
        package: "ureq",
        rust_ident: "ureq",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::requests::Http",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Requests,
        summary: "Replace blocking `ureq` HTTP calls with sans-IO `bstd::requests::Http` / `bstd::http::Client`.",
        path_rewrites: &[("ureq::", "bstd::requests::")],
    },
    CrateMapping {
        package: "reqwest",
        rust_ident: "reqwest",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::requests::Http",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Net,
        summary: "Replace `reqwest` with sans-IO `bstd::requests::Http` / `bstd::http::Client`.",
        path_rewrites: &[("reqwest::", "bstd::requests::")],
    },
    CrateMapping {
        package: "url",
        rust_ident: "url",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::url::Url",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Checked URLs and origins live in `bstd::url::{Url, Origin}`.",
        path_rewrites: &[("url::Url", "bstd::url::Url")],
    },
    CrateMapping {
        package: "anyhow",
        rust_ident: "anyhow",
        disposition: DepDisposition::DirectBstd,
        target: "typed Error enum (`core::error::Error`)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace stringly `anyhow::Error` with typed error enums implementing `core::error::Error` (0 deps).",
        path_rewrites: &[("anyhow::Result", "core::result::Result")],
    },
    CrateMapping {
        package: "thiserror",
        rust_ident: "thiserror",
        disposition: DepDisposition::DirectBstd,
        target: "core::error::Error + core::fmt::Display",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Zero-dep hand-written `Display` + `core::error::Error` (or blaze built-in comptime patch).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "pico-args",
        rust_ident: "pico_args",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::args::Args",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "CLI arguments are a capability (`bstd::args::Args`) parsed without external crates.",
        path_rewrites: &[("pico_args::Arguments", "bstd::args::Args")],
    },
    CrateMapping {
        package: "clap",
        rust_ident: "clap",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::args::Args",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "CLI arguments come through `bstd::args::Args` (or typed JSON input).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "flate2",
        rust_ident: "flate2",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_deflate",
        extra_blaze_crate: Some("blaze-deflate"),
        bstd_feature: BstdFeature::Image,
        summary: "Zero-dep inflate/deflate/zlib lives in `blaze-deflate`.",
        path_rewrites: &[("flate2::", "blaze_deflate::")],
    },
    CrateMapping {
        package: "miniz_oxide",
        rust_ident: "miniz_oxide",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_deflate",
        extra_blaze_crate: Some("blaze-deflate"),
        bstd_feature: BstdFeature::Image,
        summary: "Zero-dep inflate/deflate lives in `blaze-deflate`.",
        path_rewrites: &[("miniz_oxide::", "blaze_deflate::")],
    },
    CrateMapping {
        package: "libdeflater",
        rust_ident: "libdeflater",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze_deflate",
        extra_blaze_crate: Some("blaze-deflate"),
        bstd_feature: BstdFeature::Image,
        summary: "Replace C `libdeflater` with pure-Rust `blaze-deflate` (0 deps), or keep if peak C throughput is benchmark-critical.",
        path_rewrites: &[("libdeflater::", "blaze_deflate::")],
    },
    CrateMapping {
        package: "zlib-rs",
        rust_ident: "zlib_rs",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_deflate",
        extra_blaze_crate: Some("blaze-deflate"),
        bstd_feature: BstdFeature::Image,
        summary: "Replace with `blaze-deflate`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "tar",
        rust_ident: "tar",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_tar",
        extra_blaze_crate: Some("blaze-tar"),
        bstd_feature: BstdFeature::Core,
        summary: "Zero-dep ustar archive reader/writer lives in `blaze-tar`.",
        path_rewrites: &[("tar::", "blaze_tar::")],
    },
    CrateMapping {
        package: "bitflags",
        rust_ident: "bitflags",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_bitflags",
        extra_blaze_crate: Some("blaze-bitflags"),
        bstd_feature: BstdFeature::Core,
        summary: "Zero-dep bitflags macro lives in `blaze-bitflags`.",
        path_rewrites: &[("bitflags::bitflags", "blaze_bitflags::bitflags")],
    },
    CrateMapping {
        package: "memchr",
        rust_ident: "memchr",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_memchr",
        extra_blaze_crate: Some("blaze-memchr"),
        bstd_feature: BstdFeature::Core,
        summary: "Zero-dep SIMD/SWAR byte search lives in `blaze-memchr`.",
        path_rewrites: &[("memchr::", "blaze_memchr::")],
    },
    CrateMapping {
        package: "semver",
        rust_ident: "semver",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_semver",
        extra_blaze_crate: Some("blaze-semver"),
        bstd_feature: BstdFeature::Core,
        summary: "Zero-dep SemVer parser lives in `blaze-semver`.",
        path_rewrites: &[("semver::", "blaze_semver::")],
    },
    CrateMapping {
        package: "glob",
        rust_ident: "glob",
        disposition: DepDisposition::DirectBstd,
        target: "blaze_glob",
        extra_blaze_crate: Some("blaze-glob"),
        bstd_feature: BstdFeature::Core,
        summary: "Zero-dep glob matcher lives in `blaze-glob`.",
        path_rewrites: &[("glob::", "blaze_glob::")],
    },
    CrateMapping {
        package: "bytemuck",
        rust_ident: "bytemuck",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::pod",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Safe POD casts (`Pod`, `Zeroable`, `CheckedBitPattern`) live in `bstd::pod`.",
        path_rewrites: &[("bytemuck::", "bstd::pod::")],
    },
    CrateMapping {
        package: "zerocopy",
        rust_ident: "zerocopy",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::pod",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Safe POD casts live in `bstd::pod` (`blaze-cast`).",
        path_rewrites: &[("zerocopy::", "bstd::pod::")],
    },
    CrateMapping {
        package: "hex",
        rust_ident: "hex",
        disposition: DepDisposition::DirectBstd,
        target: "inline core hex encoder/decoder (0 deps)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Tiny pure-`core` hex encode/decode helper eliminates the `hex` dependency completely.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "crc32fast",
        rust_ident: "crc32fast",
        disposition: DepDisposition::DirectBstd,
        target: "inline core IEEE CRC32 (0 deps)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Pure-`core` table-driven CRC32 eliminates `crc32fast`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "rkyv",
        rust_ident: "rkyv",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::record",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Zero-copy on-disk/wire records live in `bstd::record` (`blaze-format`), or keep `rkyv` if binary format compatibility is frozen.",
        path_rewrites: &[("rkyv::", "bstd::record::")],
    },
    CrateMapping {
        package: "hegeltest",
        rust_ident: "hegeltest",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::test (`blaze-sim`)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::SimDraws,
        summary: "`bstd::test` (`blaze-sim`) replaces `hegeltest` with deterministic tape-shrinking simulation tests.",
        path_rewrites: &[("hegeltest::", "bstd::test::")],
    },
    CrateMapping {
        package: "proptest",
        rust_ident: "proptest",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::test (`blaze-sim`)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::SimDraws,
        summary: "`bstd::test` (`blaze-sim`) replaces `proptest`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "tokio",
        rust_ident: "tokio",
        disposition: DepDisposition::NeedsDecision,
        target: "sans-IO state machine + bstd::{Cx, Scope, Net, Clock}",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Lift `async fn` / `tokio::*` IO into explicit sans-IO state machines driven by `bstd::Net`/`bstd::task::Scope`, or keep `tokio` in the binary driver shell.",
        path_rewrites: &[("tokio::net::TcpStream", "bstd::net::Conn")],
    },
    CrateMapping {
        package: "mio",
        rust_ident: "mio",
        disposition: DepDisposition::NeedsDecision,
        target: "sans-IO core + bstd::net::Net",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Split `mio::Poll` / `mio::net::TcpStream` loops into a sans-IO protocol core + thin `bstd::net` driver.",
        path_rewrites: &[("mio::net::TcpStream", "bstd::net::Conn")],
    },

    // --- Architectural decisions needed ---
    CrateMapping {
        package: "heed",
        rust_ident: "heed",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::kv::Kv (or sans-IO Store trait)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "LMDB wrapper: replace with `bstd::kv::Kv` / `bstd::record` (0 deps, crash-simulatable), isolate behind a sans-IO `KvStore` trait, or keep `heed`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "rusqlite",
        rust_ident: "rusqlite",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::kv::Kv / blaze_sql (or sans-IO Store trait)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "SQLite wrapper: replace with `bstd::kv::Kv` + `blaze-sql`, isolate behind a sans-IO trait, or keep `rusqlite`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "libc",
        rust_ident: "libc",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze_sys / bstd",
        extra_blaze_crate: Some("blaze-sys"),
        bstd_feature: BstdFeature::Core,
        summary: "Replace OS syscalls with `bstd` / `blaze-sys`, or keep `libc` for C FFI types.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "zstd",
        rust_ident: "zstd",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze_squeeze / keep zstd",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Keep `zstd` for compatibility with existing compressed chunks, or replace with `blaze-deflate` / `blaze-squeeze`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "rustls",
        rust_ident: "rustls",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::net (feature `net`)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Net,
        summary: "Route TLS through `bstd::net` / `bstd::roots::Roots` or keep direct `rustls::Connection` (already sans-IO).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "webpki-roots",
        rust_ident: "webpki_roots",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::roots::Roots",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Net,
        summary: "Trust anchors are injected as `bstd::roots::Roots` capability.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "libmimalloc-sys",
        rust_ident: "libmimalloc_sys",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::mem::{Frame, Heap} (delete global allocator override)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "bstd passes explicit `Frame`/`Heap` allocators; remove `libmimalloc-sys` or keep as host global allocator.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "syn",
        rust_ident: "syn",
        disposition: DepDisposition::NeedsDecision,
        target: "syntax / bstd::comptime",
        extra_blaze_crate: Some("syntax"),
        bstd_feature: BstdFeature::Core,
        summary: "Replace `syn` with `bstd::comptime` reflection (for derives) or blaze's zero-dep `syntax` crate, or keep in standalone proc-macro crates.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "quote",
        rust_ident: "quote",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::comptime / direct TokenStream writer",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace proc-macro codegen with `bstd::comptime` reflection or keep in proc-macro crates.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "proc-macro2",
        rust_ident: "proc_macro2",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::comptime / core proc_macro",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace proc-macro helpers with `bstd::comptime` or keep in proc-macro crates.",
        path_rewrites: &[],
    },
    // --- C / C++ interop & build-script FFI tools -> blaze::c::import! / blaze::cpp::import! ---
    CrateMapping {
        package: "bindgen",
        rust_ident: "bindgen",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze::c::import! / blaze::cpp::import! (no build.rs or host libclang)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace `bindgen` in `build.rs` with hermetic `blaze::c::import!` / `blaze::cpp::import!` (record-cached declarations keyed by headers read).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "cc",
        rust_ident: "cc",
        disposition: DepDisposition::NeedsDecision,
        target: "[package.metadata.blaze.c] declarative C/C++ units (no build.rs)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace `cc::Build` in `build.rs` with `[package.metadata.blaze.c]` (compiled per file, lazily on demand, using blaze's pinned clang).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "cmake",
        rust_ident: "cmake",
        disposition: DepDisposition::NeedsDecision,
        target: "[package.metadata.blaze.c] + blaze::c::import! / blaze::cpp::import!",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace host `cmake` build scripts with declarative `[package.metadata.blaze.c]` source units + `blaze::c::import!` / `blaze::cpp::import!`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "cxx",
        rust_ident: "cxx",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze::cpp::import! + [package.metadata.blaze.c] (std = \"c++20\")",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace `#[cxx::bridge]` with direct `blaze::cpp::import!` (no bridge file, no build script, on-demand C++ thunks as code records).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "cxx-build",
        rust_ident: "cxx_build",
        disposition: DepDisposition::NeedsDecision,
        target: "[package.metadata.blaze.c] + blaze::cpp::import!",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace `cxx-build` in `build.rs` with `[package.metadata.blaze.c]` (`std = \"c++20\"`) and `blaze::cpp::import!`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "autocxx",
        rust_ident: "autocxx",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze::cpp::import! + [package.metadata.blaze.c]",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace `autocxx` with `blaze::cpp::import!` (pinned clang declarations + on-demand C++ template/inline compilation).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "autocxx-build",
        rust_ident: "autocxx_build",
        disposition: DepDisposition::NeedsDecision,
        target: "[package.metadata.blaze.c] + blaze::cpp::import!",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace `autocxx-build` in `build.rs` with `[package.metadata.blaze.c]` and `blaze::cpp::import!`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "pkg-config",
        rust_ident: "pkg_config",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze::c::import! + [package.metadata.blaze.c] (hermetic vendored headers)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace host `pkg-config` probing with hermetic `blaze::c::import!` and declarative `[package.metadata.blaze.c]` units.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "clang-sys",
        rust_ident: "clang_sys",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze::c::import! / blaze::cpp::import! (blaze's pinned clang)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Remove host `clang-sys` dependency by using blaze's built-in `blaze::c::import!` / `blaze::cpp::import!`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "schemars",
        rust_ident: "schemars",
        disposition: DepDisposition::NeedsDecision,
        target: "blaze_data schema / bstd::comptime",
        extra_blaze_crate: Some("blaze-data"),
        bstd_feature: BstdFeature::Core,
        summary: "Derive JSON Schema via `bstd::comptime` reflection over `blaze-data`, or keep `schemars`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "hdrhistogram",
        rust_ident: "hdrhistogram",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::telemetry",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace latency histograms with `bstd::telemetry` distributions, or keep `hdrhistogram`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "prometheus-client",
        rust_ident: "prometheus_client",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::telemetry + bstd::report",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Emit metrics via `bstd::telemetry` / `bstd::report`, or keep Prometheus exporter in the host binary.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "pprof",
        rust_ident: "pprof",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::telemetry",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace sampling profiler crate with `bstd::telemetry`, or keep in perf tooling.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "backtrace",
        rust_ident: "backtrace",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::telemetry / bstd::report",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Structured error provenance replaces ambient `backtrace` captures.",
        path_rewrites: &[],
    },

    // --- Specialized domain / legacy crypto / FFI crates (keep external or isolate behind sans-IO trait) ---
    CrateMapping {
        package: "rsa",
        rust_ident: "rsa",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (protocol crypto: PKCS#1 / Minecraft auth)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "RSA public-key crypto has no `bstd` equivalent; keep isolated in a pure crypto leaf crate.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "aes",
        rust_ident: "aes",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (protocol crypto: AES-128)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "AES block cipher is pure no_std math; keep isolated in the framing/crypto crate.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "cfb8",
        rust_ident: "cfb8",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (protocol crypto: CFB8 mode)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "CFB8 stream cipher mode is pure no_std math; keep isolated in the framing/crypto crate.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "sha1",
        rust_ident: "sha1",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (legacy SHA-1 required by protocol)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "SHA-1 is legacy protocol crypto (not shipped in `bstd::digest`); keep or inline ~80 lines of pure `core`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "md-5",
        rust_ident: "md5",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (legacy MD5 for UUID v3)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "MD5 is legacy protocol crypto; keep or inline ~70 lines of pure `core`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "hmac",
        rust_ident: "hmac",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (or 20-line pure `core` HMAC over `bstd::digest::Sha256`)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "HMAC over SHA-256 is 20 lines in pure `core` or keep `hmac`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "libloading",
        rust_ident: "libloading",
        disposition: DepDisposition::NeedsDecision,
        target: "bstd::code::Code (or keep `libloading` in host shell)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Dynamic module loading: migrate to `bstd::code::Code` capability or keep `libloading` in the host binary.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "cc",
        rust_ident: "cc",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (build-dep for C FFI like flecs)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Build-script C compiler driver for native C libraries (e.g. flecs); keep on FFI sys crates.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "libz-ng-sys",
        rust_ident: "libz_ng_sys",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (benchmark comparison dep)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "C zlib-ng FFI binding (only in benchmark crates).",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "regex",
        rust_ident: "regex",
        disposition: DepDisposition::NeedsDecision,
        target: "plain string matcher / small Pike VM (0 deps)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Replace literal/simple regexes with `core::str` matching or a small zero-dep Pike VM, or keep `regex`.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "tempfile",
        rust_ident: "tempfile",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::test::Sim (in-memory simulated FS, zero disk temp dirs)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Tests using `tempfile` run cleanly in-memory under `bstd::test::Sim` without touching disk.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "bindgen",
        rust_ident: "bindgen",
        disposition: DepDisposition::KeepExternal,
        target: "keep external (build-dep for C FFI bindings)",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "C header binding generator for native FFI crates.",
        path_rewrites: &[],
    },
    CrateMapping {
        package: "trybuild",
        rust_ident: "trybuild",
        disposition: DepDisposition::DirectBstd,
        target: "bstd::test compile_fail / blaze refusals",
        extra_blaze_crate: None,
        bstd_feature: BstdFeature::Core,
        summary: "Compile-fail tests run through blaze's refusal harness instead of spawning cargo via `trybuild`.",
        path_rewrites: &[],
    },
];

/// Look up a third-party crate by Cargo package name or Rust crate ident.
pub fn lookup_crate(name: &str) -> Option<&'static CrateMapping> {
    CRATE_CATALOG.iter().find(|m| m.package == name || m.rust_ident == name)
}

/// `std::` modules that move mechanically to `core::` with zero semantic change.
pub const STD_TO_CORE_MODULES: &[&str] = &[
    "any",
    "arch",
    "array",
    "ascii",
    "borrow",
    "cell",
    "char",
    "clone",
    "cmp",
    "convert",
    "default",
    "error",
    "f32",
    "f64",
    "ffi",
    "fmt",
    "future",
    "hash",
    "hint",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "iter",
    "marker",
    "mem",
    "num",
    "ops",
    "option",
    "pin",
    "primitive",
    "ptr",
    "range",
    "result",
    "simd",
    "slice",
    "str",
    "task",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
];

/// Return true if `std::<module>` is pure `core::<module>`.
pub fn is_std_core_module(module: &str) -> bool {
    // `std::ffi::OsStr` / `OsString` are std-only, handled separately at leaf level;
    // `std::ffi::CStr` / `c_void` / `c_char` are in `core::ffi`.
    STD_TO_CORE_MODULES.contains(&module)
}

/// Classification of an IO / ambient effect operation detected in Rust source code.
/// Every `EffectKind` corresponds to a suspension point in the sans-IO state-machine compiler
/// AND a concrete `bstd` capability in the thin driver!
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectKind {
    ReadFileString,
    ReadFileBytes,
    WriteFile,
    CreateDir,
    RemoveFile,
    RenameFile,
    ReadDir,
    StatFile,
    ReadEnv,
    ReadArgs,
    ReadClock,
    ReadWallClock,
    Sleep,
    NetConnect,
    NetListen,
    NetRead,
    NetWrite,
    HttpGet,
    HttpPost,
    RunProcess,
    SpawnThread,
    WriteOut,
    WriteErr,
    Random,
    AwaitFuture,
}

impl EffectKind {
    /// PascalCase variant stem used in generated `Effect` / `Event` / `State` enums.
    pub const fn variant_stem(self) -> &'static str {
        match self {
            Self::ReadFileString => "ReadFileString",
            Self::ReadFileBytes => "ReadFileBytes",
            Self::WriteFile => "WriteFile",
            Self::CreateDir => "CreateDir",
            Self::RemoveFile => "RemoveFile",
            Self::RenameFile => "RenameFile",
            Self::ReadDir => "ReadDir",
            Self::StatFile => "StatFile",
            Self::ReadEnv => "ReadEnv",
            Self::ReadArgs => "ReadArgs",
            Self::ReadClock => "ReadClock",
            Self::ReadWallClock => "ReadWallClock",
            Self::Sleep => "Sleep",
            Self::NetConnect => "NetConnect",
            Self::NetListen => "NetListen",
            Self::NetRead => "NetRead",
            Self::NetWrite => "NetWrite",
            Self::HttpGet => "HttpGet",
            Self::HttpPost => "HttpPost",
            Self::RunProcess => "RunProcess",
            Self::SpawnThread => "SpawnThread",
            Self::WriteOut => "WriteOut",
            Self::WriteErr => "WriteErr",
            Self::Random => "Random",
            Self::AwaitFuture => "Await",
        }
    }

    /// The `bstd` capability required by a thin driver that executes this effect.
    pub const fn bstd_capability(self) -> &'static str {
        match self {
            Self::ReadFileString | Self::ReadFileBytes | Self::ReadDir | Self::StatFile => {
                "fs: &bstd::fs::Dir<bstd::fs::ReadOnly>"
            }
            Self::WriteFile | Self::CreateDir | Self::RemoveFile | Self::RenameFile => {
                "fs: &bstd::fs::Dir<bstd::fs::ReadWrite>"
            }
            Self::ReadEnv => "env: &bstd::env::Env",
            Self::ReadArgs => "args: &bstd::args::Args",
            Self::ReadClock | Self::ReadWallClock | Self::Sleep => "clock: &bstd::clock::Clock",
            Self::NetConnect | Self::NetListen | Self::NetRead | Self::NetWrite => "net: &bstd::net::Net",
            Self::HttpGet | Self::HttpPost => "http: &bstd::requests::Http",
            Self::RunProcess => "procs: &bstd::procs::Procs",
            Self::SpawnThread => "scope: &mut bstd::task::Scope",
            Self::WriteOut | Self::WriteErr => "out: &mut bstd::out::Out",
            Self::Random => "rng: &mut bstd::rng::Rng",
            Self::AwaitFuture => "cx: &bstd::Cx",
        }
    }

    /// Short capability tag name (used for deduplicating driver parameters and reporting).
    pub const fn capability_key(self) -> &'static str {
        match self {
            Self::ReadFileString | Self::ReadFileBytes | Self::ReadDir | Self::StatFile => "fs_ro",
            Self::WriteFile | Self::CreateDir | Self::RemoveFile | Self::RenameFile => "fs_rw",
            Self::ReadEnv => "env",
            Self::ReadArgs => "args",
            Self::ReadClock | Self::ReadWallClock | Self::Sleep => "clock",
            Self::NetConnect | Self::NetListen | Self::NetRead | Self::NetWrite => "net",
            Self::HttpGet | Self::HttpPost => "http",
            Self::RunProcess => "procs",
            Self::SpawnThread => "task",
            Self::WriteOut | Self::WriteErr => "out",
            Self::Random => "rng",
            Self::AwaitFuture => "async",
        }
    }

    /// Return type carried back by the corresponding `Event` variant.
    pub const fn event_payload_type(self) -> &'static str {
        match self {
            Self::ReadFileString => "Result<String, String>",
            Self::ReadFileBytes => "Result<Vec<u8>, String>",
            Self::WriteFile | Self::CreateDir | Self::RemoveFile | Self::RenameFile => {
                "Result<(), String>"
            }
            Self::ReadDir => "Result<Vec<String>, String>",
            Self::StatFile => "Result<u64, String>",
            Self::ReadEnv => "Result<String, String>",
            Self::ReadArgs => "Vec<String>",
            Self::ReadClock => "u64",
            Self::ReadWallClock => "u64",
            Self::Sleep => "()",
            Self::NetConnect => "Result<u32, String>",
            Self::NetListen => "Result<u32, String>",
            Self::NetRead => "Result<Vec<u8>, String>",
            Self::NetWrite => "Result<usize, String>",
            Self::HttpGet | Self::HttpPost => "Result<String, String>",
            Self::RunProcess => "Result<(i32, Vec<u8>), String>",
            Self::SpawnThread => "()",
            Self::WriteOut | Self::WriteErr => "()",
            Self::Random => "u64",
            Self::AwaitFuture => "()",
        }
    }

    /// Unwrapped success type (when the call is followed by `?`, `.unwrap()`, or `.expect()`).
    pub const fn ok_payload_type(self) -> &'static str {
        match self {
            Self::ReadFileString | Self::ReadEnv | Self::HttpGet | Self::HttpPost => "String",
            Self::ReadFileBytes | Self::NetRead => "Vec<u8>",
            Self::ReadDir | Self::ReadArgs => "Vec<String>",
            Self::StatFile | Self::ReadClock | Self::ReadWallClock | Self::Random => "u64",
            Self::NetConnect | Self::NetListen => "u32",
            Self::NetWrite => "usize",
            Self::RunProcess => "(i32, Vec<u8>)",
            Self::WriteFile
            | Self::CreateDir
            | Self::RemoveFile
            | Self::RenameFile
            | Self::Sleep
            | Self::SpawnThread
            | Self::WriteOut
            | Self::WriteErr
            | Self::AwaitFuture => "()",
        }
    }
}

/// Match a call path (e.g. `std::fs::read_to_string`, `fs::write`, `Instant::now`, `ureq::post`)
/// to its `EffectKind`, if it performs ambient IO or system authority.
pub fn classify_call_effect(path: &str) -> Option<EffectKind> {
    let p = path.trim();
    let tail = p.strip_prefix("std::").unwrap_or(p);
    match tail {
        "fs::read_to_string" => Some(EffectKind::ReadFileString),
        "fs::read" | "fs::File::open" | "File::open" => Some(EffectKind::ReadFileBytes),
        "fs::write" | "fs::File::create" | "File::create" | "fs::copy" => Some(EffectKind::WriteFile),
        "fs::create_dir" | "fs::create_dir_all" => Some(EffectKind::CreateDir),
        "fs::remove_file" | "fs::remove_dir" | "fs::remove_dir_all" => Some(EffectKind::RemoveFile),
        "fs::rename" => Some(EffectKind::RenameFile),
        "fs::read_dir" => Some(EffectKind::ReadDir),
        "fs::metadata" | "fs::symlink_metadata" => Some(EffectKind::StatFile),
        "env::var" | "env::var_os" | "env::vars" | "env::current_dir" => Some(EffectKind::ReadEnv),
        "env::args" | "env::args_os" => Some(EffectKind::ReadArgs),
        "time::Instant::now" | "Instant::now" => Some(EffectKind::ReadClock),
        "time::SystemTime::now" | "SystemTime::now" => Some(EffectKind::ReadWallClock),
        "thread::sleep" | "tokio::time::sleep" => Some(EffectKind::Sleep),
        "net::TcpStream::connect"
        | "TcpStream::connect"
        | "mio::net::TcpStream::connect"
        | "tokio::net::TcpStream::connect" => Some(EffectKind::NetConnect),
        "net::TcpListener::bind"
        | "TcpListener::bind"
        | "mio::net::TcpListener::bind"
        | "tokio::net::TcpListener::bind" => Some(EffectKind::NetListen),
        "ureq::get" | "reqwest::get" => Some(EffectKind::HttpGet),
        "ureq::post" | "ureq::put" | "ureq::delete" => Some(EffectKind::HttpPost),
        "process::Command::new" | "Command::new" => Some(EffectKind::RunProcess),
        "thread::spawn" | "thread::scope" | "tokio::spawn" => Some(EffectKind::SpawnThread),
        "rand::random" | "rand::thread_rng" | "rand::rng" => Some(EffectKind::Random),
        _ => None,
    }
}

/// Match a macro name (e.g. `"println"`, `"eprintln"`) to an `EffectKind`.
pub fn classify_macro_effect(name: &str) -> Option<EffectKind> {
    match name {
        "println" | "print" => Some(EffectKind::WriteOut),
        "eprintln" | "eprint" => Some(EffectKind::WriteErr),
        _ => None,
    }
}

/// Types whose presence as a struct field indicates the struct holds live IO/runtime handles
/// and is a candidate for splitting into a pure sans-IO core + thin bstd driver.
pub fn is_io_handle_type(ty_text: &str) -> bool {
    let t = ty_text.trim();
    [
        "TcpStream",
        "TcpListener",
        "UdpSocket",
        "UnixStream",
        "UnixListener",
        "File",
        "BufReader",
        "BufWriter",
        "Instant",
        "SystemTime",
        "Poll",
        "Child",
        "JoinHandle",
        "Sender",
        "Receiver",
    ]
    .iter()
    .any(|needle| {
        t == *needle
            || t.ends_with(&format!("::{needle}"))
            || t.contains(&format!("{needle}<"))
            || t.contains(&format!("&mut {needle}"))
            || t.contains(&format!("&{needle}"))
    })
}

/// True if `pkg` is a C/C++ binding generator or build-script C/C++ compiler driver that blaze
/// replaces with `blaze::c::import!` / `blaze::cpp::import!` and `[package.metadata.blaze.c]`.
pub fn is_c_interop_crate(pkg: &str) -> bool {
    matches!(
        pkg,
        "bindgen"
            | "cc"
            | "cmake"
            | "cxx"
            | "cxx-build"
            | "autocxx"
            | "autocxx-build"
            | "pkg-config"
            | "clang-sys"
            | "autotools"
            | "meson"
    )
}

/// True if `pkg` is specifically a C++ bridge/binding crate (`cxx`, `autocxx`).
pub fn is_cpp_interop_crate(pkg: &str) -> bool {
    matches!(pkg, "cxx" | "cxx-build" | "autocxx" | "autocxx-build")
}
