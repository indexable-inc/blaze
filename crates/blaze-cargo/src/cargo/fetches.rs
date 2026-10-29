//! What build scripts download, pinned as Nix pins a fixed-output derivation: by the sha256
//! of the bytes, recorded once, and handed to the script with no network.
//!
//! A build script runs with no network ([`super::script_sandbox`]). A crate that downloads a
//! prebuilt library in its build script (rusty_v8's V8, ort's ONNX Runtime, livekit's
//! libwebrtc) is given the download instead, through the crate's own variable for a local
//! copy, as crate2nix's `defaultCrateOverrides` gives it to a Nix build: [`OVERRIDES`] says,
//! for each crate blaze knows, which variable and where the download is.
//!
//! `blaze lock --fetches` ([`command`]) reads `Cargo.lock`, and for each of its packages that
//! [`OVERRIDES`] has, for each target asked (the host by default), works out the address,
//! downloads it, keeps the bytes in blaze's store by their sha256, and writes what it
//! downloaded into [`FILE`] beside `Cargo.lock`: not into `Cargo.lock`, whose fields a cargo
//! that does not know them drops when it writes it again. An entry is kept by the package's
//! name, version and what `Cargo.lock` pins it by (its checksum, or its git source), the
//! target, and the features of it that pick the download: those of the whole workspace's
//! resolve, or of one build's with its `-p`s; an entry of another feature set is kept.
//!
//! A build ([`hand`], from [`super::script_sandbox::plan`]) finds the entry of the package
//! whose script it runs, writes the store's bytes out below the target directory (unpacked,
//! where the variable names a directory) and sets the variable to them. The path is below
//! the profile's directory, which the key of the script's run names by what stands for it
//! (`script.rs`'s `Places`), so the run is the same on every machine; what it names is
//! told by the sha256 in it. A package of [`OVERRIDES`] with no entry, or one for another
//! version or checksum, fails, naming it and the command that records it: a build never
//! downloads.
//!
//! [`FILE`] is TOML: `version` (of the whole file, [`VERSION`]), a `[[fetch]]` table for each
//! download ([`Entry`]), and `[[system]]` tables, which are the system libraries' resolution
//! and whose fields this module leaves to the code that writes them.
//!
//! The bytes are an entry of the store kept by their sha256 alone
//! ([`blaze_store::Store::open_by_content`]), so every build of blaze finds them, and a
//! remote cache carries them as it carries any entry.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::remote::{self, Https, Traffic};
use crate::sandbox::{Line, Reached};

/// The file beside `Cargo.lock` with what build scripts download.
pub const FILE: &str = "Cargo.fetches.lock";

/// The format of [`FILE`].
const VERSION: u32 = 1;

/// The command that writes [`FILE`], as an error names it.
const COMMAND: &str = "blaze lock --fetches";

/// The directory below the profile's where downloads are written out for build scripts.
const WRITTEN: &str = "blaze-fetches";

/// How long a download waits for the server to answer, or for the next of its bytes.
const PATIENCE: Duration = Duration::from_secs(60);

/// How a crate's build script is given what it would download.
pub struct Override {
    /// The package whose build script it is.
    pub package: &'static str,
    pub give: Give,
}

pub enum Give {
    /// A value of a variable that has the script download nothing.
    Value { var: &'static str, value: &'static str },
    /// A download, named by the variable the script reads a local copy from.
    Fetch {
        var: &'static str,
        /// The package's features that pick what is downloaded.
        features: Picks,
        /// The download for a package, `None` for a target there is none for.
        download: fn(&Of<'_>) -> anyhow::Result<Option<Download>>,
        hand: Hand,
        /// The variable of an override of the same package this one stands in for: it is
        /// downloaded and handed only where that one's address has nothing
        /// ([`Download::may_be_absent`]).
        instead_of: Option<&'static str>,
    },
}

/// Which features of a package pick its download.
#[derive(Clone, Copy)]
pub enum Picks {
    These(&'static [&'static str]),
    /// Every feature of the package but these.
    AllBut(&'static [&'static str]),
}

impl Picks {
    /// Whether the feature whose variable is `CARGO_FEATURE_{var}` picks the download.
    fn picks(self, var: &str) -> bool {
        match self {
            Picks::These(them) => them.iter().any(|it| feature_var(it) == var),
            Picks::AllBut(them) => !them.iter().any(|it| feature_var(it) == var),
        }
    }
}

/// The part of `CARGO_FEATURE_{..}` cargo names `feature` by.
fn feature_var(feature: &str) -> String {
    feature.to_uppercase().replace('-', "_")
}

/// What the variable names.
#[derive(Clone, Copy)]
pub enum Hand {
    /// The file downloaded.
    File,
    /// The file downloaded, as a `file://` address.
    FileAddress,
    /// The directory the file downloaded is in, below `into` there (where `{version}` is the
    /// package's version).
    Within { into: &'static str },
    /// What the archive downloaded holds, unpacked below `into` (where `{target}` is the
    /// target and `{sha256}` the archive's) in a directory of its own; the variable names that
    /// directory, or what [`Download::names`] says below it.
    Unpacked { archive: Archive, into: &'static str },
    /// What the tar.gz of a revision downloaded holds (its directory [`Download::names`]), made
    /// a git repository of one commit on `branch` (where `{major}` and `{minor}` are of the
    /// package's version) and served in place of the repository at `serves`: git's
    /// `url.<repository>.insteadOf`, set through `GIT_CONFIG_COUNT` (the variable) and its
    /// `GIT_CONFIG_KEY_n`s, so the script's `git clone` of `serves` clones it.
    Repository { serves: &'static str, branch: &'static str },
}

#[derive(Clone, Copy)]
pub enum Archive {
    Zip,
    /// A tar in a raw LZMA2 stream of a 64 MiB dictionary, as ort's CDN serves it.
    TarLzma2,
    TarGz,
}

/// A package, as an override works out its download.
pub struct Of<'a> {
    pub version: &'a str,
    pub target: &'a str,
    /// Which of the override's features are on, sorted.
    pub features: &'a [String],
    /// The package's directory.
    pub source: &'a Path,
    /// The directory of a dependency of the package, by its name.
    pub dependency: &'a dyn Fn(&str) -> Option<PathBuf>,
}

impl Of<'_> {
    fn on(&self, feature: &str) -> bool {
        self.features.iter().any(|it| it == feature)
    }
}

pub struct Download {
    pub url: String,
    /// Below the unpacked download, what the variable names: empty for the directory itself.
    pub names: String,
    /// Whether the address may have nothing, as a prebuilt library made for only some feature
    /// sets; [`FILE`] then says so and the script is given nothing for it.
    pub may_be_absent: bool,
}

impl Download {
    fn at(url: String) -> Download {
        Download { url, names: String::new(), may_be_absent: false }
    }
}

/// The crates blaze hands a download, crate2nix's `defaultCrateOverrides` for build scripts.
/// Each cites the source of the crate it read, at the version given.
pub const OVERRIDES: &[Override] = &[
    // rusty_v8 (`v8` 152.2.0): with `RUSTY_V8_ARCHIVE` set it reads that file and downloads
    // nothing (build.rs:1559), gzipped or not (build.rs:1656). The address is
    // `DEFAULT_ARTIFACT_BASE` (build.rs:793) `/v{version}/` and `static_lib_name` of
    // `{features}_{profile}_{target}` (build.rs:1550, 784): the profile is `release` unless
    // `V8_FORCE_DEBUG` (build.rs:762), which a script in the sandbox is not given, and the
    // features `_ptrcomp` and `_sandbox` (build.rs:773). Its Rust binding is in the published
    // crate (`gen/`, build.rs:1735), so nothing else is downloaded.
    Override {
        package: "v8",
        give: Give::Fetch {
            var: "RUSTY_V8_ARCHIVE",
            features: Picks::These(&["v8_enable_pointer_compression", "v8_enable_sandbox"]),
            download: v8,
            hand: Hand::File,
            instead_of: None,
        },
    },
    // ort (`ort-sys` 2.0.0-rc.15): it downloads the prebuilt ONNX Runtime its
    // `build/download/dist.tsv` names for the target and its execution-provider features
    // (resolve.rs:18, 51) into `{ORT_CACHE_DIR}/dfbin/{target}/{sha256}`, unless that directory
    // is there (build/main.rs:120, src/internal/dirs.rs:133). `ORT_LIB_LOCATION` would take
    // its path for a build of one's own instead (build/main.rs:44), which links differently,
    // so the cache is what it is given, with the archive unpacked as it unpacks it
    // (extract.rs:70).
    Override {
        package: "ort-sys",
        give: Give::Fetch {
            var: "ORT_CACHE_DIR",
            features: Picks::These(ORT_FEATURES),
            download: ort,
            hand: Hand::Unpacked { archive: Archive::TarLzma2, into: "dfbin/{target}/{sha256}" },
            instead_of: None,
        },
    },
    // webrtc-sys (livekit's, and zed's fork at 0.3.23): `webrtc-sys-build` downloads
    // `webrtc-{triple}.zip` of the release `WEBRTC_TAG` (src/lib.rs:30, 95, 193) unless
    // `LK_CUSTOM_WEBRTC` names a directory (src/lib.rs:79), which the build script reads
    // `include` and `lib` of (build.rs:98). The zip holds that directory as `{triple}`.
    Override {
        package: "webrtc-sys",
        give: Give::Fetch {
            var: "LK_CUSTOM_WEBRTC",
            features: Picks::These(&[]),
            download: webrtc,
            hand: Hand::Unpacked { archive: Archive::Zip, into: "" },
            instead_of: None,
        },
    },
    // ffmpeg-sys-fframes 9.0.0 (fframes' FFmpeg), with `build` and `static`: before it compiles
    // FFmpeg it takes the prebuilt `{FFMPEG_BINARIES_CACHE}/binaries-{version}/ffmpeg-{key}.tar.gz`
    // when that is there, else downloads it from `DEFAULT_URL` (build_prebuilt.rs:30, 112, 172),
    // the key being the target and the short names of every feature but five (`key`,
    // build_prebuilt.rs:55). Only some feature sets are made (none with `vpx`); for the
    // others it compiles FFmpeg from a `git clone -b release/{major.minor}` of FFmpeg's
    // repository (`fetch`, build.rs:163), which is served it from the tarball of that branch.
    Override {
        package: "ffmpeg-sys-fframes",
        give: Give::Fetch {
            var: "FFMPEG_BINARIES_CACHE",
            features: Picks::AllBut(FFMPEG_UNKEYED),
            download: ffmpeg,
            hand: Hand::Within { into: "binaries-{version}" },
            instead_of: None,
        },
    },
    Override {
        package: "ffmpeg-sys-fframes",
        give: Give::Fetch {
            var: "GIT_CONFIG_COUNT",
            features: Picks::These(&["build"]),
            download: ffmpeg_source,
            hand: Hand::Repository {
                serves: "https://github.com/FFmpeg/FFmpeg",
                branch: "release/{major}.{minor}",
            },
            instead_of: Some("FFMPEG_BINARIES_CACHE"),
        },
    },
    // skia-bindings 0.153.3 (`binary-cache`, a default), built from its crate: it downloads
    // `SKIA_BINARIES_URL` (a `file://` one is read, binary_cache/utils.rs:35), by default
    // `https://github.com/rust-skia/skia-binaries/releases/download/{tag}/skia-binaries-{key}.tar.gz`
    // (binary_cache/env.rs:22), `{tag}` its version and `{key}` the first 20 digits of the
    // commit `.cargo_vcs_info.json` names, the target and its features (binaries.rs:73,
    // download.rs:171), unless `FORCE_SKIA_BUILD`.
    Override {
        package: "skia-bindings",
        give: Give::Fetch {
            var: "SKIA_BINARIES_URL",
            features: Picks::These(SKIA_FEATURES),
            download: skia,
            hand: Hand::FileAddress,
            instead_of: None,
        },
    },
    // helix-term 25.7.1 fetches every tree-sitter grammar with git and compiles them into
    // `runtime/grammars` of the checkout unless `HELIX_DISABLE_AUTO_GRAMMAR_BUILD` is set
    // (build.rs:4). Those are some two hundred repositories at revisions of
    // `languages.toml`, which the editor fetches and builds itself (`hx --grammar fetch`,
    // `hx --grammar build`); as nixpkgs' helix, the build leaves them out.
    Override {
        package: "helix-term",
        give: Give::Value { var: "HELIX_DISABLE_AUTO_GRAMMAR_BUILD", value: "1" },
    },
];

/// The features of ort-sys that pick its download (resolve.rs:55): each a part of the
/// feature set its `dist.tsv` names, but `cuda`, whose part is `cuda12` or `cuda13`.
const ORT_FEATURES: &[&str] = &[
    "training", "tensorrt", "openvino", "onednn", "directml", "nnapi", "coreml", "xnnpack", "rocm",
    "acl", "armnn", "tvm", "migraphx", "rknpu", "vitis", "cann", "qnn", "webgpu", "azure", "nvrtx",
    "vsinpu", "cuda",
];

fn v8(of: &Of<'_>) -> anyhow::Result<Option<Download>> {
    let mut features = String::new();
    // `v8_enable_sandbox` turns pointer compression on.
    if of.on("v8_enable_pointer_compression") || of.on("v8_enable_sandbox") {
        features.push_str("_ptrcomp");
    }
    if of.on("v8_enable_sandbox") {
        features.push_str("_sandbox");
    }
    let target = of.target;
    let file = match target.contains("-windows") {
        true => format!("rusty_v8{features}_release_{target}.lib.gz"),
        false => format!("librusty_v8{features}_release_{target}.a.gz"),
    };
    let base = "https://github.com/denoland/rusty_v8/releases/download";
    Ok(Some(Download::at(format!("{base}/v{}/{file}", of.version))))
}

fn ort(of: &Of<'_>) -> anyhow::Result<Option<Download>> {
    let table = of.source.join("build/download/dist.tsv");
    let text = std::fs::read_to_string(&table).with_context(|| {
        format!(
            "reading {}: blaze's override of ort-sys reads the downloads it names, which ort-sys \
             has from 2.0.0-rc.11",
            table.display()
        )
    })?;
    if of.on("cuda") {
        anyhow::bail!(
            "ort-sys with `cuda` picks its download by the CUDA it finds on the machine \
             (`ORT_CUDA_VERSION`, `CUDA_HOME`, `nvcc`), which blaze's override does not yet"
        );
    }
    let wanted: BTreeSet<&str> = of.features.iter().map(String::as_str).collect();
    let mut candidates: Vec<(BTreeSet<&str>, &str)> = text
        .lines()
        .skip(1)
        .filter(|it| !it.is_empty() && !it.starts_with('#'))
        .filter_map(|line| {
            let columns: Vec<&str> = line.split('\t').collect();
            let [target, features, url, _hash] = columns[..] else { return None };
            let features = match features {
                "none" => BTreeSet::new(),
                them => them.split(',').collect(),
            };
            (target == of.target).then_some((features, url))
        })
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    // As `resolve_dist` picks: one of exactly the features, else the first of those with the
    // most of them, when that has them all.
    candidates.sort_by_key(|it| std::cmp::Reverse(it.0.intersection(&wanted).count()));
    let exact = candidates.iter().find(|it| it.0 == wanted);
    let (features, url) = exact.unwrap_or(&candidates[0]);
    if features.intersection(&wanted).count() != wanted.len() {
        anyhow::bail!(
            "ort-sys {} has no prebuilt ONNX Runtime for {} with the features {wanted:?}",
            of.version,
            of.target
        );
    }
    Ok(Some(Download::at((*url).to_owned())))
}

fn webrtc(of: &Of<'_>) -> anyhow::Result<Option<Download>> {
    let build = (of.dependency)("webrtc-sys-build").context("it has no webrtc-sys-build")?;
    let lib = build.join("src/lib.rs");
    let text =
        std::fs::read_to_string(&lib).with_context(|| format!("reading {}", lib.display()))?;
    let tag = between(&text, "pub const WEBRTC_TAG: &str = \"", "\"")
        .with_context(|| format!("{} has no `WEBRTC_TAG`", lib.display()))?;
    let end = text.find("/{}/{}.zip\"");
    let start = end.and_then(|end| Some(text[..end].rfind('"')? + 1));
    let address = start.zip(end).map(|(start, end)| &text[start..end]).with_context(|| {
        format!("{} has no address of the form `\"https://.../{{}}/{{}}.zip\"`", lib.display())
    })?;
    let (arch, os) = (of.target.split('-').next().unwrap_or_default(), target_os(of.target));
    let os = match os {
        "windows" => "win",
        "macos" => "mac",
        "ios" if of.target.ends_with("-sim") => "ios-simulator",
        "ios" => "ios-device",
        other => other,
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    let triple = format!("{os}-{arch}-release");
    Ok(Some(Download {
        url: format!("{address}/{tag}/webrtc-{triple}.zip"),
        names: triple,
        may_be_absent: false,
    }))
}

/// The features of ffmpeg-sys-fframes that are not in the key of its prebuilt FFmpeg
/// (build_prebuilt.rs:61); `build` and `static` are not either, but say whether it is taken.
const FFMPEG_UNKEYED: &[&str] = &["build-portable", "non-exhaustive-enums", "default"];

fn ffmpeg(of: &Of<'_>) -> anyhow::Result<Option<Download>> {
    if !(of.on("build") && of.on("static")) {
        return Ok(None);
    }
    let mut short: Vec<String> = of
        .features
        .iter()
        .filter(|it| !matches!(it.as_str(), "build" | "static"))
        .map(|it| {
            let it = feature_var(it).to_lowercase();
            let prefixes = ["build_lib_", "build_license_", "build_"];
            let short = prefixes.iter().find_map(|prefix| it.strip_prefix(prefix)).unwrap_or(&it);
            short.replace('_', "-")
        })
        .collect();
    short.sort();
    let key = match short.is_empty() {
        true => of.target.to_owned(),
        false => format!("{}-{}", of.target, short.join("-")),
    };
    let base = "https://github.com/dmtrKovalenko/rust-ffmpeg-sys/releases/download";
    Ok(Some(Download {
        url: format!("{base}/binaries-{}/ffmpeg-{key}.tar.gz", of.version),
        names: String::new(),
        may_be_absent: true,
    }))
}

fn ffmpeg_source(of: &Of<'_>) -> anyhow::Result<Option<Download>> {
    if !of.on("build") {
        return Ok(None);
    }
    let release = major_minor(of.version);
    Ok(Some(Download {
        url: format!(
            "https://codeload.github.com/FFmpeg/FFmpeg/tar.gz/refs/heads/release/{release}"
        ),
        names: format!("FFmpeg-release-{release}"),
        may_be_absent: false,
    }))
}

fn major_minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// The features of skia-bindings in the key of its binaries (`Features::from_cargo_env`,
/// features.rs), and `binary-cache`, without which it downloads none.
const SKIA_FEATURES: &[&str] = &[
    "binary-cache",
    "pdf",
    "gl",
    "egl",
    "wayland",
    "x11",
    "vulkan",
    "metal",
    "d3d",
    "ganesh",
    "graphite",
    "textlayout",
    "svg",
    "skottie",
    "webp-encode",
    "webp-decode",
    "embed-freetype",
    "freetype-woff2",
    "jpeg-encode",
    "jpeg-decode",
];

fn skia(of: &Of<'_>) -> anyhow::Result<Option<Download>> {
    if !of.on("binary-cache") {
        return Ok(None);
    }
    let vcs = of.source.join(".cargo_vcs_info.json");
    let text = match std::fs::read_to_string(&vcs) {
        // Not built from its crate (a checkout of rust-skia), it downloads nothing.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        it => it.with_context(|| format!("reading {}", vcs.display()))?,
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("reading {}", vcs.display()))?;
    let sha1 = value["git"]["sha1"].as_str().filter(|it| it.len() >= 20);
    let sha1 = sha1.with_context(|| format!("{} names no commit", vcs.display()))?;
    // FreeType's features are dropped where Skia does not use FreeType (platform.rs:28).
    let freetype = !matches!(target_os(of.target), "macos" | "ios" | "windows" | "visionos")
        && !of.target.contains("ohos");
    let mut features: Vec<&str> = of
        .features
        .iter()
        .map(String::as_str)
        .filter(|it| *it != "binary-cache")
        .filter(|it| freetype || !matches!(*it, "embed-freetype" | "freetype-woff2"))
        .map(|it| match it {
            "webp-encode" => "webpe",
            "webp-decode" => "webpd",
            "embed-freetype" => "ftembed",
            "freetype-woff2" => "ftwoff2",
            "jpeg-encode" => "jpege",
            "jpeg-decode" => "jpegd",
            other => other,
        })
        .collect();
    features.sort_unstable();
    let mut key = vec![&sha1[..20], of.target];
    let features = features.join("-");
    if !features.is_empty() {
        key.push(&features);
    }
    // `crt-static`, on by default for musl (binaries.rs:94).
    if of.target.contains("-musl") {
        key.push("static");
    }
    let base = "https://github.com/rust-skia/skia-binaries/releases/download";
    Ok(Some(Download::at(format!("{base}/{}/skia-binaries-{}.tar.gz", of.version, key.join("-")))))
}

/// `target_os` of a target triple, as its `cfg` has it, for the triples the overrides know.
fn target_os(target: &str) -> &str {
    let parts: Vec<&str> = target.split('-').collect();
    match parts[..] {
        [_, "apple", "darwin", ..] => "macos",
        [_, "apple", "ios", ..] => "ios",
        [_, _, "windows", ..] => "windows",
        [_, _, "linux", ..] | [_, "linux", ..] => {
            if target.contains("android") {
                "android"
            } else {
                "linux"
            }
        }
        [_, _, os, ..] => os,
        _ => target,
    }
}

fn between<'a>(text: &'a str, before: &str, after: &str) -> Option<&'a str> {
    let at = text.find(before)? + before.len();
    Some(&text[at..at + text[at..].find(after)?])
}

/// One download of [`FILE`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub package: String,
    pub version: String,
    /// What `Cargo.lock` pins the package by: its checksum, or its source where it has none.
    pub locked: String,
    pub target: String,
    /// The package's features that pick the download that are on, sorted.
    #[serde(default)]
    pub features: Vec<String>,
    /// The variable the build script is given it by.
    pub var: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// What the variable names below the unpacked download, when not the directory itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub names: String,
    /// That the package downloads nothing here (its override has no download for it), or
    /// that `url` has nothing ([`Download::may_be_absent`]): the script is given nothing, and
    /// `sha256` is empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absent: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct Lock {
    /// Sorted.
    pub fetches: Vec<Entry>,
    /// The `[[system]]` tables: what the system libraries of build scripts resolve to (a
    /// crate's nixpkgs attributes and their store paths, by target). Their fields are the
    /// system resolver's; `blaze lock --fetches` writes them back as it read them.
    pub system: Vec<toml::Table>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    version: u32,
    #[serde(default)]
    fetch: Vec<Entry>,
    #[serde(default)]
    system: Vec<toml::Table>,
}

impl Lock {
    pub fn read(text: &str) -> anyhow::Result<Lock> {
        let written: Written = toml::from_str(text)?;
        if written.version != VERSION {
            anyhow::bail!(
                "it is of version {}, and this blaze reads version {VERSION}: `{COMMAND}` writes \
                 it again",
                written.version
            );
        }
        let mut fetches = written.fetch;
        fetches.sort();
        Ok(Lock { fetches, system: written.system })
    }

    /// The text of [`FILE`]: each entry in order, each field in order.
    pub fn text(&self) -> String {
        let quoted = |it: &str| toml::Value::String(it.to_owned()).to_string();
        let mut text = format!(
            "# Written by `{COMMAND}`: what build scripts download, each by its sha256. A build\n\
             # hands each to its script through the crate's own variable, with no network.\n\
             version = {VERSION}\n"
        );
        let mut fetches = self.fetches.clone();
        fetches.sort();
        for it in &fetches {
            let features: Vec<String> = it.features.iter().map(|it| quoted(it)).collect();
            text += &format!(
                "\n[[fetch]]\npackage = {}\nversion = {}\nlocked = {}\ntarget = {}\nfeatures = [{}]\n\
                 var = {}\nurl = {}\nsha256 = {}\nsize = {}\n",
                quoted(&it.package),
                quoted(&it.version),
                quoted(&it.locked),
                quoted(&it.target),
                features.join(", "),
                quoted(&it.var),
                quoted(&it.url),
                quoted(&it.sha256),
                it.size,
            );
            if !it.names.is_empty() {
                text += &format!("names = {}\n", quoted(&it.names));
            }
            if it.absent {
                text += "absent = true\n";
            }
        }
        if !self.system.is_empty() {
            #[derive(Serialize)]
            struct Systems<'a> {
                system: &'a [toml::Table],
            }
            let system = toml::to_string(&Systems { system: &self.system })
                .expect("a table read from TOML writes as TOML");
            text += &format!("\n{system}");
        }
        text
    }

    fn of_root(root: &Path) -> anyhow::Result<Option<Lock>> {
        let path = root.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                Lock::read(&text).with_context(|| format!("reading {}", path.display())).map(Some)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

/// A package of `Cargo.lock`.
#[derive(Clone, Debug, Deserialize)]
pub struct Locked {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub checksum: Option<String>,
}

impl Locked {
    /// What it is pinned by: its checksum, its source where it has none (git), and `path`
    /// for a package of the checkout.
    pub fn pin(&self) -> String {
        self.checksum.clone().or_else(|| self.source.clone()).unwrap_or_else(|| "path".to_owned())
    }
}

/// The packages of the `Cargo.lock` at `path`.
pub fn cargo_lock(path: &Path) -> anyhow::Result<Vec<Locked>> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        package: Vec<Locked>,
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: File =
        toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    Ok(file.package)
}

/// The package of `locked` named `name` at `version` whose directory is `dir`: of several,
/// the one whose source `dir` is in (a git checkout's, named by the commit's start, or a
/// registry's).
fn locked_at<'a>(
    locked: &'a [Locked],
    name: &str,
    version: &str,
    dir: &Path,
) -> anyhow::Result<&'a Locked> {
    let them: Vec<&Locked> =
        locked.iter().filter(|it| it.name == name && it.version == version).collect();
    if let [one] = them[..] {
        return Ok(one);
    }
    let dir = dir.to_string_lossy();
    let fits = |it: &&&Locked| match it.source.as_deref() {
        Some(source) if source.starts_with("git+") => {
            let commit = source.rsplit('#').next().unwrap_or_default();
            dir.contains("/git/checkouts/") && commit.len() >= 7 && dir.contains(&commit[..7])
        }
        Some(_) => dir.contains("/registry/src/"),
        None => !dir.contains("/registry/src/") && !dir.contains("/git/checkouts/"),
    };
    match them.iter().filter(fits).collect::<Vec<_>>()[..] {
        [one] => Ok(one),
        [] => anyhow::bail!("Cargo.lock has no `{name}` {version} whose source is {dir}"),
        _ => anyhow::bail!("Cargo.lock has `{name}` {version} of several sources"),
    }
}

/// What a build script is given in place of downloading: its variables, and what it is to
/// read.
#[derive(Debug, Default)]
pub struct Handed {
    pub env: Vec<(String, String)>,
    pub readable: Vec<PathBuf>,
}

/// What the build script cargo runs with `env`, writing `out_dir`, of the workspace at
/// `root`, is given in place of what it would download, by [`OVERRIDES`]. A variable of an
/// override the workspace gives scripts itself (`build-script-env`, `given`) is left as it is.
/// `Err` names the package and what to run when [`FILE`] has no download of it for this build.
pub fn hand(
    env: &[(String, String)],
    out_dir: &Path,
    root: Option<&Path>,
    given: &BTreeSet<String>,
) -> anyhow::Result<Handed> {
    hand_with(OVERRIDES, env, out_dir, root, given, super::daemon::store().as_deref())
}

fn hand_with(
    overrides: &[Override],
    env: &[(String, String)],
    out_dir: &Path,
    root: Option<&Path>,
    given: &BTreeSet<String>,
    store: Option<&Path>,
) -> anyhow::Result<Handed> {
    let var = |name: &str| env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
    let package = var("CARGO_PKG_NAME").unwrap_or_default();
    let mut handed = Handed::default();
    // The variables of the package's overrides it is given nothing for.
    let mut absent: BTreeSet<&str> = BTreeSet::new();
    for it in overrides.iter().filter(|it| it.package == package) {
        let (name, features, hand) = match &it.give {
            Give::Value { var, .. } | Give::Fetch { var, .. } if given.contains(*var) => continue,
            Give::Value { var, value } => {
                handed.env.push(((*var).to_owned(), (*value).to_owned()));
                continue;
            }
            Give::Fetch { instead_of: Some(other), .. } if !absent.contains(other) => continue,
            Give::Fetch { var, features, hand, .. } => (*var, *features, *hand),
        };
        let version = var("CARGO_PKG_VERSION").unwrap_or_default();
        let target = var("TARGET").unwrap_or_default();
        let dir = Path::new(var("CARGO_MANIFEST_DIR").unwrap_or_default());
        let what = format!(
            "the build script of `{package}` {version} downloads what `{name}` names a local \
             copy of, which a build is not to download"
        );
        let found = root.map(Path::to_owned).or_else(|| locked_dir(out_dir, var("PWD")));
        let root = found
            .as_deref()
            .with_context(|| format!("{what}, and no Cargo.lock is above {}", out_dir.display()))?;
        let locked = cargo_lock(&root.join("Cargo.lock")).with_context(|| what.clone())?;
        let locked = locked_at(&locked, package, version, dir)?.pin();
        let Some(lock) = Lock::of_root(root)? else {
            anyhow::bail!("{what}, and {} has no {FILE}: `{COMMAND}` records it", root.display());
        };
        // As cargo names them to the script: `CARGO_FEATURE_` and the name upper-cased, `-`
        // made `_`.
        let mut on: Vec<String> = env
            .iter()
            .filter_map(|(it, _)| it.strip_prefix("CARGO_FEATURE_"))
            .filter(|it| features.picks(it))
            .map(str::to_owned)
            .collect();
        on.sort();
        let named_on =
            |it: &Entry| it.features.iter().map(|it| feature_var(it)).collect::<BTreeSet<_>>();
        let of_package =
            |it: &&Entry| it.package == package && it.var == name && it.target == target;
        let entries: Vec<&Entry> = lock.fetches.iter().filter(of_package).collect();
        let of_version: Vec<&Entry> = entries
            .iter()
            .copied()
            .filter(|it| it.version == version && it.locked == locked)
            .collect();
        let same_features =
            of_version.iter().find(|it| named_on(it) == on.iter().cloned().collect());
        let entry = match of_version.first() {
            None if entries.is_empty() => anyhow::bail!(
                "{what}, and {FILE} has none of it for {target}: `{COMMAND}` records it{}",
                if var("HOST") == Some(target) {
                    String::new()
                } else {
                    format!(" (`--target {target}`)")
                }
            ),
            None => anyhow::bail!(
                "{what}, and {FILE} has it for {}, not for `{package}` {version} as Cargo.lock \
                 locks it ({locked}): `{COMMAND}` records it again",
                entries
                    .iter()
                    .map(|it| format!("{} ({})", it.version, it.locked))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(_) => match same_features {
                Some(entry) => *entry,
                None => anyhow::bail!(
                    "{what}; {FILE} has it for the features {} of it, and this build has {on:?} \
                     on: `{COMMAND}` with this build's `-p`s (and `--tests` for a test) records \
                     it for them",
                    of_version
                        .iter()
                        .map(|it| format!("{:?}", it.features))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        };
        if entry.absent {
            absent.insert(name);
            continue;
        }
        let profile = profile_dir(out_dir)?;
        let written = profile.join(WRITTEN);
        let path = write_out(entry, hand, &written, store)
            .with_context(|| format!("handing `{package}` {version} {}", entry.url))?;
        let path = path.to_str().context("a target directory whose name is not text")?.to_owned();
        match hand {
            Hand::FileAddress => handed.env.push((name.to_owned(), format!("file://{path}"))),
            Hand::Repository { serves, .. } => {
                if let Some((taken, _)) = env.iter().find(|it| it.0.starts_with("GIT_CONFIG_")) {
                    anyhow::bail!(
                        "{what}: blaze serves it a git repository through git's \
                         `GIT_CONFIG_COUNT`, and the build sets `{taken}` itself"
                    );
                }
                // The repository is the target directory's, which the sandbox's user may
                // not own.
                let config = [
                    (format!("url.file://{path}.insteadOf"), serves.to_owned()),
                    ("safe.directory".to_owned(), "*".to_owned()),
                ];
                handed.env.push((name.to_owned(), config.len().to_string()));
                for (at, (key, value)) in config.into_iter().enumerate() {
                    handed.env.push((format!("GIT_CONFIG_KEY_{at}"), key));
                    handed.env.push((format!("GIT_CONFIG_VALUE_{at}"), value));
                }
            }
            Hand::File | Hand::Within { .. } | Hand::Unpacked { .. } => {
                handed.env.push((name.to_owned(), path))
            }
        }
        handed.readable.push(written);
    }
    Ok(handed)
}

/// The directory of the `Cargo.lock` of a build of a package in no workspace (a lone
/// package's `Cargo.lock` is beside its manifest): the nearest above the target directory
/// `out_dir` is in, else above `pwd`.
fn locked_dir(out_dir: &Path, pwd: Option<&str>) -> Option<PathBuf> {
    let has = |it: &&Path| it.join("Cargo.lock").is_file() && it.join("Cargo.toml").is_file();
    let above_target =
        profile_dir(out_dir).ok().and_then(|it| it.ancestors().find(has).map(Path::to_owned));
    above_target.or_else(|| Path::new(pwd?).ancestors().find(has).map(Path::to_owned))
}

/// The profile's directory `out_dir` is below: what is above its `build` directory.
fn profile_dir(out_dir: &Path) -> anyhow::Result<PathBuf> {
    let text = out_dir.to_str().context("an OUT_DIR whose name is not text")?;
    let at = text.replace('\\', "/").rfind("/build/").filter(|it| *it > 0);
    let at = at.with_context(|| format!("the OUT_DIR {text} is below no `build` directory"))?;
    Ok(PathBuf::from(&text[..at]))
}

/// The key the store keeps a download of `sha256` under.
fn key(sha256: &str) -> anyhow::Result<[u8; 32]> {
    let bytes: Option<Vec<u8>> = (0..sha256.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(sha256.get(at..at + 2)?, 16).ok())
        .collect();
    let bytes =
        bytes.filter(|it| it.len() == 32).with_context(|| format!("`{sha256}` is no sha256"))?;
    let mut key = super::keyed::Key::new("a download by its sha256");
    key.field(&bytes);
    Ok(key.done())
}

fn sha256(bytes: &[u8]) -> String {
    crate::content::hex(&Sha256::digest(bytes))
}

fn open_store(store: Option<&Path>) -> anyhow::Result<blaze_store::Store> {
    let dir =
        store.context("blaze keeps downloads in its store, which `BLAZE_CACHE=off` turns off")?;
    blaze_store::Store::open_by_content(dir)
        .with_context(|| format!("opening blaze's store in {}", dir.display()))
}

/// Writes the download of `entry` out below `dir`, in a directory named by its sha256, as
/// `hand` says, unless it is there; the result is what the variable names.
fn write_out(
    entry: &Entry,
    hand: Hand,
    dir: &Path,
    store: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let at = dir.join(&entry.sha256);
    let named = match hand {
        Hand::File | Hand::FileAddress => at.join(file_name(&entry.url)),
        Hand::Within { .. } => at.clone(),
        Hand::Unpacked { into, .. } => at.join(placed(into, entry)).join(&entry.names),
        Hand::Repository { .. } => at.join(&entry.names),
    };
    // What is there was moved there whole.
    if at.exists() {
        return Ok(named);
    }
    let store = open_store(store)?;
    let bytes = store.get(&key(&entry.sha256)?).with_context(|| {
        format!(
            "blaze's store does not have it (sha256 {}): `{COMMAND}` downloads it again",
            entry.sha256
        )
    })?;
    if sha256(&bytes) != entry.sha256 {
        anyhow::bail!("blaze's store has other bytes for sha256 {}", entry.sha256);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    let staging = dir.join(format!(".{}.{}", entry.sha256, std::process::id()));
    _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let written = match hand {
        Hand::File | Hand::FileAddress => {
            std::fs::write(staging.join(file_name(&entry.url)), &bytes).map_err(Into::into)
        }
        Hand::Within { into } => {
            let within = staging.join(placed(into, entry));
            std::fs::create_dir_all(&within)
                .and_then(|()| std::fs::write(within.join(file_name(&entry.url)), &bytes))
                .map_err(Into::into)
        }
        Hand::Unpacked { archive, into } => {
            unpack(archive, &bytes, &staging.join(placed(into, entry)))
        }
        Hand::Repository { branch, .. } => unpack(Archive::TarGz, &bytes, &staging)
            .and_then(|()| repository(&staging.join(&entry.names), &placed(branch, entry))),
    };
    if let Err(e) = written {
        _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if std::fs::rename(&staging, &at).is_err() {
        _ = std::fs::remove_dir_all(&staging);
        anyhow::ensure!(at.exists(), "moving {} into place", at.display());
    }
    Ok(named)
}

fn placed(into: &str, entry: &Entry) -> String {
    let mut parts = entry.version.split('.');
    let (major, minor) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    into.replace("{target}", &entry.target)
        .replace("{sha256}", &entry.sha256)
        .replace("{version}", &entry.version)
        .replace("{major}", major)
        .replace("{minor}", minor)
}

/// Makes `dir` a git repository of one commit of all it holds, on `branch`, for a `git clone`
/// of it. Every field of the commit is fixed, so its id is that of the files.
fn repository(dir: &Path, branch: &str) -> anyhow::Result<()> {
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "1970-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "1970-01-01T00:00:00Z")
            .args(["-c", "user.name=blaze", "-c", "user.email=blaze@localhost"])
            .args(["-c", "core.autocrlf=false"])
            .args(args)
            .output()
            .context("running git, which makes the repository a build script clones")?;
        anyhow::ensure!(
            output.status.success(),
            "`git {}` in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    };
    git(&["init", "-q", "-b", branch])?;
    git(&["add", "-A", "-f"])?;
    git(&["commit", "-q", "--no-verify", "-m", "the download blaze locked"])
}

/// The last part of the path of `url`.
fn file_name(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path)
}

fn unpack(archive: Archive, bytes: &[u8], into: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(into)?;
    match archive {
        Archive::Zip => zip::ZipArchive::new(Cursor::new(bytes))?.extract(into)?,
        Archive::TarLzma2 => {
            let raw = lzma_rust2::Lzma2Reader::new(Cursor::new(bytes), 1 << 26, None);
            tar::Archive::new(raw).unpack(into)?;
        }
        Archive::TarGz => {
            tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(bytes))).unpack(into)?
        }
    }
    Ok(())
}

/// Downloads `url`, whole; `None` where it has nothing.
fn download(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let (base, name) = url.rsplit_once('/').with_context(|| format!("{url} is no address"))?;
    let base = url::Url::parse(base).with_context(|| format!("{url} is no address"))?;
    let web = Https::open(&base, PATIENCE, Arc::new(Traffic::new(1)))?;
    let runtime = remote::runtime()?;
    runtime.block_on(async {
        let Some(mut body) = web.read_from(name, 0).await? else { return Ok(None) };
        let mut bytes = Vec::new();
        loop {
            let next = tokio::time::timeout(PATIENCE, remote::https_next(&mut body)).await;
            match next.map_err(|_| anyhow::anyhow!("no bytes from {url} for {PATIENCE:?}"))?? {
                Some(piece) => bytes.extend_from_slice(&piece),
                None => break,
            }
        }
        anyhow::Ok(Some(bytes))
    })
}

/// A package of the workspace's resolve for a target, as [`lock`] reads it.
pub struct Resolved {
    pub locked: Locked,
    pub source: PathBuf,
    /// The features on.
    pub features: Vec<String>,
    /// The directory of each dependency, by its name.
    pub dependencies: BTreeMap<String, PathBuf>,
}

/// The entries of [`FILE`] for `resolved`, the packages of each target's resolve by target:
/// each download `overrides` names for one, from `old` where it has the same one whose bytes
/// the store has, else downloaded by `fetch` and kept in the store. Of `old`, what is of
/// another feature set (another selection's resolve) is kept while `locked` (`Cargo.lock`)
/// has its package.
fn lock(
    overrides: &[Override],
    resolved: &BTreeMap<String, Vec<Resolved>>,
    locked: &[Locked],
    old: &Lock,
    store: &blaze_store::Store,
    fetch: &dyn Fn(&str) -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<Lock> {
    let mut fetches = Vec::new();
    let mut downloaded: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for (target, packages) in resolved {
        for package in packages {
            let name = &package.locked.name;
            // The variables of the package's overrides it is given nothing for.
            let mut absent: BTreeSet<&str> = BTreeSet::new();
            for it in overrides.iter().filter(|it| it.package == *name) {
                let Give::Fetch { var, features, download, instead_of, .. } = &it.give else {
                    continue;
                };
                if instead_of.is_some_and(|it| !absent.contains(it)) {
                    continue;
                }
                let mut on: Vec<String> = package
                    .features
                    .iter()
                    .filter(|it| features.picks(&feature_var(it)))
                    .cloned()
                    .collect();
                on.sort();
                let dependency = |name: &str| package.dependencies.get(name).cloned();
                let of = Of {
                    version: &package.locked.version,
                    target,
                    features: &on,
                    source: &package.source,
                    dependency: &dependency,
                };
                let what = || format!("the download of `{name}` {} for {target}", of.version);
                let found = download(&of).with_context(what)?;
                let mut entry = Entry {
                    package: name.clone(),
                    version: package.locked.version.clone(),
                    locked: package.locked.pin(),
                    target: target.clone(),
                    features: on.clone(),
                    var: (*var).to_owned(),
                    url: String::new(),
                    sha256: String::new(),
                    size: 0,
                    names: String::new(),
                    absent: false,
                };
                let Some(found) = found else {
                    super::say::note(&format!(
                        "`{name}` {} downloads nothing for {target} with {on:?}",
                        of.version
                    ));
                    absent.insert(var);
                    fetches.push(Entry { absent: true, ..entry });
                    continue;
                };
                (entry.url, entry.names) = (found.url, found.names);
                let before = old
                    .fetches
                    .iter()
                    .find(|it| Entry { sha256: String::new(), size: 0, ..(*it).clone() } == entry);
                let (sha256, size) = match (before, downloaded.get(&entry.url)) {
                    (_, Some(done)) => done.clone(),
                    (Some(before), None) if store.has(&key(&before.sha256)?) => {
                        (before.sha256.clone(), before.size)
                    }
                    (before, None) => {
                        super::say::status(false, "Downloading", &entry.url);
                        let Some(bytes) = fetch(&entry.url).with_context(what)? else {
                            anyhow::ensure!(found.may_be_absent, "{} is not there", entry.url);
                            super::say::note(&format!(
                                "`{name}` {} has no download for {target} with {on:?}: {} is not there",
                                of.version, entry.url
                            ));
                            absent.insert(var);
                            fetches.push(Entry { absent: true, ..entry });
                            continue;
                        };
                        let sha256 = sha256(&bytes);
                        if let Some(before) = before.filter(|it| it.sha256 != sha256) {
                            anyhow::bail!(
                                "{} is not what {FILE} locked: its sha256 was {} and is {sha256}; \
                                 if the change is wanted, take its entry out of {FILE} and run \
                                 `{COMMAND}` again",
                                entry.url,
                                before.sha256
                            );
                        }
                        store.insert(&key(&sha256)?, &bytes);
                        store.seal();
                        anyhow::ensure!(
                            store.has(&key(&sha256)?),
                            "blaze's store did not keep {} ({} bytes)",
                            entry.url,
                            bytes.len()
                        );
                        (sha256, bytes.len() as u64)
                    }
                };
                downloaded.insert(entry.url.clone(), (sha256.clone(), size));
                (entry.sha256, entry.size) = (sha256, size);
                fetches.push(entry);
            }
        }
    }
    let same = |a: &Entry, b: &Entry| {
        (&a.package, &a.version, &a.locked, &a.target, &a.var, &a.features)
            == (&b.package, &b.version, &b.locked, &b.target, &b.var, &b.features)
    };
    let in_cargo_lock = |it: &Entry| {
        locked
            .iter()
            .any(|l| l.name == it.package && l.version == it.version && l.pin() == it.locked)
    };
    let kept: Vec<Entry> = old
        .fetches
        .iter()
        .filter(|it| in_cargo_lock(it) && !fetches.iter().any(|new| same(new, it)))
        .cloned()
        .collect();
    fetches.extend(kept);
    fetches.sort();
    fetches.dedup();
    Ok(Lock { fetches, system: old.system.clone() })
}

/// `blaze lock --fetches [--target <triple>]... [--manifest-path <path>] [-p <package>]...
/// [--workspace] [--exclude <package>]... [--tests]`: writes [`FILE`] for the workspace's
/// `Cargo.lock`, downloading what it does not have, for the features the whole workspace's
/// resolve has, or with `-p`, `--workspace` or `--exclude` those of a build of that
/// selection (and its dev-dependencies with `--tests`, as `blaze test` builds it). Returns the
/// status to exit with.
pub fn command(args: &[String]) -> anyhow::Result<i32> {
    let usage = "usage: blaze lock --fetches [--target <TRIPLE>]... [--manifest-path <PATH>] \
                 [-p <PACKAGE>]... [--workspace] [--exclude <PACKAGE>]... [--tests]";
    let (mut fetches, mut targets, mut manifest_path) = (false, Vec::new(), None);
    let (mut packages, mut workspace, mut excludes, mut tests) =
        (Vec::new(), false, Vec::new(), false);
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let (flag, joined) = match word.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (word.as_str(), None),
        };
        let mut value = || joined.clone().or_else(|| words.next().cloned());
        match flag {
            "--fetches" => fetches = true,
            "--target" => targets.extend(value()),
            "--manifest-path" => manifest_path = value().map(PathBuf::from),
            "-p" | "--package" => packages.extend(value()),
            "--workspace" => workspace = true,
            "--exclude" => excludes.extend(value()),
            "--tests" => tests = true,
            "-h" | "--help" => {
                println!(
                    "{usage}\n\nWrites {FILE} beside Cargo.lock: each download a build script of the workspace needs, for each target (the host's when none is given), by its sha256. A build script's download can depend on its package's features, which differ between builds: without `-p`, `--workspace` or `--exclude` it is for the features of the whole workspace's resolve, else for those of a build of the packages selected (with their dev-dependencies under `--tests`, as `blaze test` builds them). Entries of other feature sets are kept."
                );
                return Ok(0);
            }
            _ => {
                super::say::error(&format!("unexpected argument `{word}`\n\n{usage}"));
                return Ok(super::FAILED);
            }
        }
    }
    if !fetches {
        super::say::error(&format!(
            "`blaze lock` writes only {FILE} yet, the downloads of build scripts: `{COMMAND}`"
        ));
        return Ok(super::FAILED);
    }
    let cwd = std::env::current_dir()?;
    let start =
        manifest_path.as_deref().and_then(Path::parent).map_or(cwd.clone(), |it| cwd.join(it));
    let lone = || start.ancestors().find(|it| it.join("Cargo.toml").is_file()).map(Path::to_owned);
    let root = super::sandboxed::workspace_root(&start)?
        .or_else(lone)
        .with_context(|| format!("{} is in no package", start.display()))?;
    let locked = cargo_lock(&root.join("Cargo.lock"))?;
    let path = root.join(FILE);
    let old = Lock::of_root(&root)?.unwrap_or_default();
    let lock = if !locked.iter().any(|it| {
        OVERRIDES.iter().any(|o| o.package == it.name && matches!(o.give, Give::Fetch { .. }))
    }) {
        super::say::status(
            false,
            "Locked",
            "no build script of the workspace downloads what blaze hands it",
        );
        if old.system.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(0),
            };
        }
        Lock { fetches: Vec::new(), system: old.system }
    } else {
        if targets.is_empty() {
            targets.push(super::own::manifest::host_triple()?);
        }
        let selected = !packages.is_empty() || workspace || !excludes.is_empty();
        let build = || {
            selected.then_some(blaze_metadata::Build {
                packages: &packages,
                workspace,
                excludes: &excludes,
                dev_units: tests,
            })
        };
        let mut resolved = BTreeMap::new();
        for target in &targets {
            let found = resolve(&cwd, manifest_path.as_deref(), target, &locked, build())?;
            resolved.insert(target.clone(), found);
        }
        let store = open_store(super::daemon::store().as_deref())?;
        lock(OVERRIDES, &resolved, &locked, &old, &store, &download)?
    };
    let writing = path.with_extension("lock.writing");
    std::fs::write(&writing, lock.text())
        .and_then(|()| std::fs::rename(&writing, &path))
        .with_context(|| format!("writing {}", path.display()))?;
    let downloads = lock.fetches.iter().filter(|it| !it.absent).count();
    super::say::status(false, "Locked", &format!("{downloads} downloads in {}", path.display()));
    Ok(0)
}

/// The packages of `locked` that [`OVERRIDES`] downloads for, as the workspace resolves them
/// for `target` with blaze's own `cargo metadata` (or as `build` does, with the features of
/// that build), which fetches their sources.
fn resolve(
    cwd: &Path,
    manifest_path: Option<&Path>,
    target: &str,
    locked: &[Locked],
    build: Option<blaze_metadata::Build<'_>>,
) -> anyhow::Result<Vec<Resolved>> {
    let cfg = super::own::manifest::target_cfg(target)?;
    let asked = blaze_metadata::Asked {
        cwd,
        env: std::env::vars().collect(),
        manifest_path,
        config: &[],
        features: &[],
        all_features: false,
        no_default_features: false,
        build,
        platform: Some(blaze_metadata::TargetPlatform { triple: target, cfg: &cfg }),
        offline: false,
        store: None,
        rustc_version: Some(crate::cargo::toolchain::ours_release()),
    };
    let json = blaze_metadata::metadata(&asked)?;
    let metadata: cargo_metadata::Metadata = serde_json::from_str(&json)?;
    let resolve = metadata.resolve.context("`cargo metadata` resolved nothing")?;
    let package_of =
        |id: &cargo_metadata::PackageId| metadata.packages.iter().find(|it| it.id == *id);
    let dir_of = |it: &cargo_metadata::Package| {
        it.manifest_path.parent().map(|it| it.as_std_path().to_owned())
    };
    let mut found = Vec::new();
    for node in &resolve.nodes {
        let Some(package) = package_of(&node.id) else { continue };
        if !OVERRIDES.iter().any(|it| it.package == package.name.as_str()) {
            continue;
        }
        let version = package.version.to_string();
        let source = package.source.as_ref().map(|it| it.repr.clone());
        let Some(locked) = locked.iter().find(|it| {
            it.name == package.name.as_str() && it.version == version && it.source == source
        }) else {
            continue;
        };
        let dependencies = node
            .deps
            .iter()
            .filter_map(|dep| {
                Some((package_of(&dep.pkg)?.name.to_string(), dir_of(package_of(&dep.pkg)?)?))
            })
            .collect();
        found.push(Resolved {
            locked: locked.clone(),
            source: dir_of(package).context("a package with no directory")?,
            features: node.features.iter().map(|it| it.to_string()).collect(),
            dependencies,
        });
    }
    Ok(found)
}

/// What a build script that failed having been refused the network (`lines`, its record)
/// is told of downloads: that blaze has no override for `package`, which addresses it tried
/// and what an override would be; or, for a package [`OVERRIDES`] has, that it still tried.
pub fn no_override_note(
    package: &str,
    lines: &[Line],
    stdout: &[u8],
    stderr: &[u8],
) -> Option<String> {
    let mut tried: Vec<String> =
        lines.iter().filter(|it| it.reached == Reached::Net).map(|it| it.what.clone()).collect();
    if tried.is_empty() {
        return None;
    }
    tried.sort();
    tried.dedup();
    let printed = [String::from_utf8_lossy(stdout), String::from_utf8_lossy(stderr)].concat();
    let mut hosts: Vec<&str> = printed
        .match_indices("://")
        .filter_map(|(at, _)| {
            let scheme = printed[..at].rsplit(|c: char| !c.is_ascii_alphabetic()).next()?;
            matches!(scheme, "http" | "https").then_some(())?;
            let rest = &printed[at + 3..];
            let end = rest
                .find(|c: char| c == '/' || c == ':' || c.is_whitespace() || "\"'`)>".contains(c));
            Some(&rest[..end.unwrap_or(rest.len())]).filter(|it| !it.is_empty())
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    let tried =
        [tried.join(", "), hosts.iter().map(|it| format!("`{it}`")).collect::<Vec<_>>().join(", ")]
            .into_iter()
            .filter(|it| !it.is_empty())
            .collect::<Vec<_>>()
            .join("; it names ");
    if OVERRIDES.iter().any(|it| it.package == package) {
        return Some(format!(
            "the build script of `{package}` tried the network ({tried}) though blaze's override \
             of it (crates/blaze-cargo/src/cargo/fetches.rs) gave it what it downloads: the override \
             is not what this version of it reads"
        ));
    }
    let mut vars: Vec<String> = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("cargo:")?;
            line.strip_prefix(':').unwrap_or(line).strip_prefix("rerun-if-env-changed=")
        })
        .map(str::to_owned)
        .collect();
    vars.sort();
    vars.dedup();
    let var = match &vars[..] {
        [] => "<VAR>".to_owned(),
        [one] => one.to_string(),
        _ => format!("<one of {}>", vars.join(", ")),
    };
    Some(format!(
        "the build script of `{package}` tried the network ({tried}), which a build never has, and \
         blaze has no override that hands `{package}` a locked download. One goes in `OVERRIDES` \
         of crates/blaze-cargo/src/cargo/fetches.rs: `Override {{ package: \"{package}\", give: \
         Give::Fetch {{ var: \"{var}\", features: Picks::These(&[]), download: <its address for \
         a version and target>, hand: Hand::File, instead_of: None }} }}`, `{var}` being the \
         variable its build script reads a \
         local copy from; if it reads none, it needs a patch to read a local file"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(package: &str, target: &str) -> Entry {
        Entry {
            package: package.to_owned(),
            version: "1.0.0".to_owned(),
            locked: "ab".repeat(32),
            target: target.to_owned(),
            features: Vec::new(),
            var: "FAKE_ARCHIVE".to_owned(),
            url: format!("https://example.com/{package}-{target}.bin"),
            sha256: "cd".repeat(32),
            size: 3,
            names: String::new(),
            absent: false,
        }
    }

    #[test]
    fn the_lock_file_reads_back_and_is_in_one_order() {
        let mut quoted = entry("z\"q", "x86_64-unknown-linux-gnu");
        quoted.features = vec!["a".to_owned(), "b".to_owned()];
        quoted.names = "linux-x64-release".to_owned();
        let absent = Entry { absent: true, sha256: String::new(), size: 0, ..entry("c", "x") };
        let them = [
            quoted,
            absent,
            entry("b", "aarch64-apple-darwin"),
            entry("b", "aarch64-unknown-linux-gnu"),
            entry("a", "x"),
        ];
        let text = Lock { fetches: them.to_vec(), system: Vec::new() }.text();
        let reversed =
            Lock { fetches: them.iter().rev().cloned().collect(), system: Vec::new() }.text();
        assert_eq!(text, reversed, "the same text whatever the order it was made in");
        let read = Lock::read(&text).unwrap();
        assert_eq!(read.text(), text);
        // The system section is the system resolver's: read and written back as it is.
        let system = "\n[[system]]\npackage = \"openssl-sys\"\nnixpkgs = [\"openssl.dev\"]\n";
        let with_system = Lock::read(&format!("{text}{system}")).unwrap();
        assert_eq!(with_system.system.len(), 1);
        assert_eq!(Lock::read(&with_system.text()).unwrap(), with_system);
        let order: Vec<(&str, &str)> =
            read.fetches.iter().map(|it| (&*it.package, &*it.target)).collect();
        assert_eq!(
            order,
            [
                ("a", "x"),
                ("b", "aarch64-apple-darwin"),
                ("b", "aarch64-unknown-linux-gnu"),
                ("c", "x"),
                ("z\"q", "x86_64-unknown-linux-gnu")
            ]
        );
        assert!(read.fetches[3].absent && text.contains("absent = true\n"));
        assert!(Lock::read(&text.replace("version = 1", "version = 2")).is_err());
        assert!(Lock::read(&format!("{text}unknown = 1\n")).is_err());
    }

    #[test]
    fn each_override_names_the_download_of_its_crate() {
        let none = |_: &str| None;
        let of = |version, target, features: &[String]| -> String {
            let of =
                Of { version, target, features, source: Path::new("/nowhere"), dependency: &none };
            v8(&of).unwrap().unwrap().url
        };
        assert_eq!(
            of("152.2.0", "x86_64-unknown-linux-gnu", &[]),
            "https://github.com/denoland/rusty_v8/releases/download/v152.2.0/librusty_v8_release_x86_64-unknown-linux-gnu.a.gz"
        );
        assert!(
            of("152.2.0", "x86_64-pc-windows-msvc", &["v8_enable_sandbox".to_owned()]).ends_with(
                "/v152.2.0/rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.lib.gz"
            )
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("build/src")).unwrap();
        std::fs::write(
            dir.path().join("build/src/lib.rs"),
            "pub const WEBRTC_TAG: &str = \"webrtc-0001d84-4\";\n\
             pub fn download_url() -> String {\n    format!(\n        \"https://github.com/zed-industries/livekit-rust-sdks/releases/download/{}/{}.zip\",\n",
        )
        .unwrap();
        let build = dir.path().join("build");
        let dependency = |name: &str| (name == "webrtc-sys-build").then(|| build.clone());
        let of = Of {
            version: "0.3.23",
            target: "aarch64-unknown-linux-gnu",
            features: &[],
            source: dir.path(),
            dependency: &dependency,
        };
        let found = webrtc(&of).unwrap().unwrap();
        assert_eq!(
            found.url,
            "https://github.com/zed-industries/livekit-rust-sdks/releases/download/webrtc-0001d84-4/webrtc-linux-arm64-release.zip"
        );
        assert_eq!(found.names, "linux-arm64-release");
        let of = Of { target: "aarch64-apple-darwin", ..of };
        assert!(webrtc(&of).unwrap().unwrap().url.ends_with("/webrtc-mac-arm64-release.zip"));
        std::fs::create_dir_all(dir.path().join("build/download")).unwrap();
        std::fs::write(
            dir.path().join("build/download/dist.tsv"),
            "target\tfeature_set\turl\tsha256_hash\n\
             aarch64-apple-darwin\tcoreml\thttps://cdn/x+coreml\t00\n\
             aarch64-apple-darwin\tcoreml,webgpu\thttps://cdn/x+coreml,webgpu\t00\n\
             aarch64-unknown-linux-gnu\tnone\thttps://cdn/a\t00\n",
        )
        .unwrap();
        let ort_of = |target, features: &[String]| {
            let of = Of {
                version: "2.0.0-rc.15",
                target,
                features,
                source: dir.path(),
                dependency: &none,
            };
            ort(&of).map(|it| it.map(|it| it.url))
        };
        assert_eq!(
            ort_of("aarch64-unknown-linux-gnu", &[]).unwrap().as_deref(),
            Some("https://cdn/a")
        );
        assert_eq!(
            ort_of("aarch64-apple-darwin", &["webgpu".to_owned()]).unwrap().as_deref(),
            Some("https://cdn/x+coreml,webgpu")
        );
        assert_eq!(
            ort_of("aarch64-apple-darwin", &[]).unwrap().as_deref(),
            Some("https://cdn/x+coreml")
        );
        assert!(ort_of("aarch64-unknown-linux-gnu", &["webgpu".to_owned()]).is_err());
        assert_eq!(ort_of("riscv64gc-unknown-linux-gnu", &[]).unwrap(), None);

        let names = |them: &str| -> Vec<String> { them.split(' ').map(str::to_owned).collect() };
        let ffmpeg_of = |target, features: &str| {
            let features = names(features);
            let of = Of {
                version: "9.0.0",
                target,
                features: &features,
                source: dir.path(),
                dependency: &none,
            };
            ffmpeg(&of).unwrap().map(|it| (it.url, it.may_be_absent))
        };
        let base =
            "https://github.com/dmtrKovalenko/rust-ffmpeg-sys/releases/download/binaries-9.0.0";
        assert_eq!(
            ffmpeg_of(
                "aarch64-unknown-linux-gnu",
                "avcodec avdevice avfilter avformat build static swresample swscale"
            ),
            Some((
                format!(
                    "{base}/ffmpeg-aarch64-unknown-linux-gnu-avcodec-avdevice-avfilter-avformat-swresample-swscale.tar.gz"
                ),
                true
            ))
        );
        assert_eq!(
            ffmpeg_of(
                "x86_64-apple-darwin",
                "build build-lib-x264 build-license-gpl static avcodec"
            )
            .unwrap()
            .0,
            format!("{base}/ffmpeg-x86_64-apple-darwin-avcodec-gpl-x264.tar.gz")
        );
        assert_eq!(ffmpeg_of("x86_64-apple-darwin", "build avcodec"), None, "a shared FFmpeg");
        let source = |features: &str| {
            let features = names(features);
            let of = Of {
                version: "9.0.0",
                target: "x",
                features: &features,
                source: dir.path(),
                dependency: &none,
            };
            ffmpeg_source(&of).unwrap().map(|it| (it.url, it.names))
        };
        assert_eq!(
            source("build"),
            Some((
                "https://codeload.github.com/FFmpeg/FFmpeg/tar.gz/refs/heads/release/9.0"
                    .to_owned(),
                "FFmpeg-release-9.0".to_owned()
            ))
        );
        assert_eq!(source(""), None);

        std::fs::write(
            dir.path().join(".cargo_vcs_info.json"),
            r#"{"git": {"sha1": "b7f043e0b1e2a850e702245de811f01df1f6b0ff", "dirty": true}}"#,
        )
        .unwrap();
        let skia_of = |target, features: &str| {
            let features = names(features);
            let of = Of {
                version: "0.153.3",
                target,
                features: &features,
                source: dir.path(),
                dependency: &none,
            };
            skia(&of).unwrap().map(|it| it.url)
        };
        let base = "https://github.com/rust-skia/skia-binaries/releases/download/0.153.3";
        let features = "binary-cache embed-freetype ganesh jpeg-decode jpeg-encode pdf svg textlayout vulkan webp-decode webp-encode";
        assert_eq!(
            skia_of("aarch64-unknown-linux-gnu", features).unwrap(),
            format!(
                "{base}/skia-binaries-b7f043e0b1e2a850e702-aarch64-unknown-linux-gnu-ftembed-ganesh-jpegd-jpege-pdf-svg-textlayout-vulkan-webpd-webpe.tar.gz"
            )
        );
        assert_eq!(
            skia_of("aarch64-apple-darwin", features).unwrap(),
            format!(
                "{base}/skia-binaries-b7f043e0b1e2a850e702-aarch64-apple-darwin-ganesh-jpegd-jpege-pdf-svg-textlayout-vulkan-webpd-webpe.tar.gz"
            ),
            "no FreeType on macOS"
        );
        assert_eq!(skia_of("x86_64-unknown-linux-gnu", "pdf"), None, "no binary cache");
    }

    /// Serves `bytes` at every address to each connection on 127.0.0.1, for as long as the
    /// test runs; the result is its address.
    fn serve(bytes: Vec<u8>) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut asked = [0; 4096];
                _ = stream.read(&mut asked);
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    bytes.len()
                );
                _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&bytes));
            }
        });
        address
    }

    static FAKE_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

    fn fake(of: &Of<'_>) -> anyhow::Result<Option<Download>> {
        let url = FAKE_URL.get().unwrap();
        Ok(Some(Download::at(format!("{url}/files/fake-{}-{}.bin", of.version, of.target))))
    }

    const FAKE: &[Override] = &[
        Override {
            package: "fake-fetch",
            give: Give::Fetch {
                var: "FAKE_ARCHIVE",
                features: Picks::These(&[]),
                download: fake,
                hand: Hand::File,
                instead_of: None,
            },
        },
        Override {
            package: "fake-fetch",
            give: Give::Value { var: "FAKE_NO_DOWNLOAD", value: "1" },
        },
    ];

    #[test]
    fn a_locked_download_is_handed_to_the_build_script_and_a_missing_or_stale_one_is_named() {
        let bytes = b"the prebuilt library".to_vec();
        _ = FAKE_URL.set(serve(bytes.clone()));
        let dir = tempfile::tempdir().unwrap();
        let (root, store_dir) = (dir.path().join("ws"), dir.path().join("store"));
        std::fs::create_dir_all(&root).unwrap();
        let checksum = "ab".repeat(32);
        let cargo_lock = |checksum: &str| {
            let text = format!(
                "version = 4\n\n[[package]]\nname = \"fake-fetch\"\nversion = \"1.0.0\"\n\
                 source = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\n"
            );
            std::fs::write(root.join("Cargo.lock"), text).unwrap();
        };
        cargo_lock(&checksum);
        let target = "x86_64-unknown-linux-gnu";
        let source = dir.path().join("registry/src/index/fake-fetch-1.0.0");
        let locked = cargo_lock_of(&root);
        let resolved = BTreeMap::from([(
            target.to_owned(),
            vec![Resolved {
                locked: locked[0].clone(),
                source: source.clone(),
                features: Vec::new(),
                dependencies: BTreeMap::new(),
            }],
        )]);
        let store = blaze_store::Store::open_by_content(&store_dir).unwrap();
        let lock =
            super::lock(FAKE, &resolved, &locked, &Lock::default(), &store, &download).unwrap();
        drop(store);
        assert_eq!(lock.fetches.len(), 1);
        let it = &lock.fetches[0];
        assert_eq!((&*it.sha256, it.size), (&*sha256(&bytes), bytes.len() as u64));
        assert_eq!((&*it.locked, &*it.var, &*it.target), (&*checksum, "FAKE_ARCHIVE", target));
        // Locked again, nothing is downloaded: the store has it.
        let store = blaze_store::Store::open_by_content(&store_dir).unwrap();
        let refuse =
            |url: &str| -> anyhow::Result<Option<Vec<u8>>> { panic!("downloaded {url} again") };
        assert_eq!(super::lock(FAKE, &resolved, &locked, &lock, &store, &refuse).unwrap(), lock);
        drop(store);

        let out_dir = root.join("target/debug/build/fake-fetch-0123/out");
        let env: Vec<(String, String)> = [
            ("CARGO_PKG_NAME", "fake-fetch"),
            ("CARGO_PKG_VERSION", "1.0.0"),
            ("TARGET", target),
            ("CARGO_MANIFEST_DIR", source.to_str().unwrap()),
            ("OUT_DIR", out_dir.to_str().unwrap()),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec();
        let given = BTreeSet::new();
        let hand = |given: &BTreeSet<String>| {
            hand_with(FAKE, &env, &out_dir, Some(&root), given, Some(&store_dir))
        };
        let said = hand(&given).unwrap_err().to_string();
        assert!(
            said.contains("`fake-fetch` 1.0.0") && said.contains(COMMAND) && said.contains(FILE),
            "{said}"
        );
        std::fs::write(root.join(FILE), lock.text()).unwrap();
        let handed = hand(&given).unwrap();
        let profile = root.join("target/debug");
        let file = profile.join(WRITTEN).join(&it.sha256).join(format!("fake-1.0.0-{target}.bin"));
        assert_eq!(
            handed.env,
            [
                ("FAKE_ARCHIVE".to_owned(), file.to_str().unwrap().to_owned()),
                ("FAKE_NO_DOWNLOAD".to_owned(), "1".to_owned())
            ]
        );
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        assert_eq!(handed.readable, [profile.join(WRITTEN)]);
        // A lone package, in no workspace, has its Cargo.lock beside its manifest.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"ws\"\n").unwrap();
        let lone = hand_with(FAKE, &env, &out_dir, None, &given, Some(&store_dir)).unwrap();
        assert_eq!(lone.env, handed.env);
        // Written out once, it is not read from the store again.
        std::fs::remove_dir_all(&store_dir).unwrap();
        assert_eq!(hand(&given).unwrap().env, handed.env);
        // What the workspace gives itself is left as it gives it.
        let workspace = BTreeSet::from(["FAKE_ARCHIVE".to_owned(), "FAKE_NO_DOWNLOAD".to_owned()]);
        assert!(hand(&workspace).unwrap().env.is_empty());

        cargo_lock(&"ef".repeat(32));
        let said = format!("{:#}", hand(&given).unwrap_err());
        assert!(
            said.contains("`fake-fetch` 1.0.0")
                && said.contains(&"ef".repeat(32))
                && said.contains(COMMAND),
            "{said}"
        );
        cargo_lock(&checksum);
        std::fs::write(root.join(FILE), Lock::default().text()).unwrap();
        let said = hand(&given).unwrap_err().to_string();
        assert!(said.contains("has none of it for x86_64-unknown-linux-gnu"), "{said}");
    }

    fn prebuilt(of: &Of<'_>) -> anyhow::Result<Option<Download>> {
        Ok(Some(Download {
            url: format!("https://example.com/prebuilt-{}.tar.gz", of.features.join("-")),
            names: String::new(),
            may_be_absent: true,
        }))
    }

    fn source(_: &Of<'_>) -> anyhow::Result<Option<Download>> {
        Ok(Some(Download {
            url: "https://example.com/source.tar.gz".to_owned(),
            names: "src-1.0".to_owned(),
            may_be_absent: false,
        }))
    }

    /// A prebuilt library made for some feature sets, and a source build from a git clone
    /// of the rest, as ffmpeg-sys-fframes'.
    const PREBUILT_OR_SOURCE: &[Override] = &[
        Override {
            package: "fake-sys",
            give: Give::Fetch {
                var: "FAKE_CACHE",
                features: Picks::AllBut(&["default"]),
                download: prebuilt,
                hand: Hand::Within { into: "binaries-{version}" },
                instead_of: None,
            },
        },
        Override {
            package: "fake-sys",
            give: Give::Fetch {
                var: "GIT_CONFIG_COUNT",
                features: Picks::These(&[]),
                download: source,
                hand: Hand::Repository {
                    serves: "https://example.com/upstream",
                    branch: "release/{major}.{minor}",
                },
                instead_of: Some("FAKE_CACHE"),
            },
        },
    ];

    #[test]
    fn each_feature_set_has_its_entry_and_one_with_no_prebuilt_is_served_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let (root, store_dir) = (dir.path().join("ws"), dir.path().join("store"));
        std::fs::create_dir_all(&root).unwrap();
        let text = "version = 4\n\n[[package]]\nname = \"fake-sys\"\nversion = \"1.0.2\"\n\
                    source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
                    checksum = \"ab\"\n";
        std::fs::write(root.join("Cargo.lock"), text).unwrap();
        let locked = cargo_lock_of(&root);
        let mut tarred = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut tar = tar::Builder::new(&mut tarred);
            let mut header = tar::Header::new_gnu();
            header.set_size(10);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "src-1.0/configure", &b"#!/bin/sh\n"[..]).unwrap();
            tar.finish().unwrap();
        }
        let tarred = tarred.finish().unwrap();
        let fetch = |url: &str| -> anyhow::Result<Option<Vec<u8>>> {
            Ok(match url {
                "https://example.com/prebuilt-build-x.tar.gz" => Some(b"prebuilt".to_vec()),
                "https://example.com/source.tar.gz" => Some(tarred.clone()),
                _ => None,
            })
        };
        let target = "aarch64-unknown-linux-gnu";
        let resolved = |features: &[&str]| {
            BTreeMap::from([(
                target.to_owned(),
                vec![Resolved {
                    locked: locked[0].clone(),
                    source: dir.path().join("registry/src/index/fake-sys-1.0.2"),
                    features: features.iter().map(|it| (*it).to_owned()).collect(),
                    dependencies: BTreeMap::new(),
                }],
            )])
        };
        let store = blaze_store::Store::open_by_content(&store_dir).unwrap();
        let built = resolved(&["build", "default", "x"]);
        let once = lock(PREBUILT_OR_SOURCE, &built, &locked, &Lock::default(), &store, &fetch);
        let once = once.unwrap();
        assert_eq!(once.fetches.len(), 1, "a prebuilt there needs no source: {once:?}");
        let tested = resolved(&["build", "y"]);
        let twice = lock(PREBUILT_OR_SOURCE, &tested, &locked, &once, &store, &fetch).unwrap();
        drop(store);
        let kinds: Vec<(&str, &[String], bool)> =
            twice.fetches.iter().map(|it| (&*it.var, &*it.features, it.absent)).collect();
        let named = |them: &[&str]| them.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
        let (x, y) = (named(&["build", "x"]), named(&["build", "y"]));
        assert_eq!(
            kinds,
            [
                ("GIT_CONFIG_COUNT", &[][..], false),
                ("FAKE_CACHE", &x[..], false),
                ("FAKE_CACHE", &y[..], true)
            ],
            "the other feature set's entry is kept"
        );
        std::fs::write(root.join(FILE), twice.text()).unwrap();

        let out_dir = root.join("target/debug/build/fake-sys-0123/out");
        let hand = |features: &[&str]| {
            let mut env: Vec<(String, String)> = [
                ("CARGO_PKG_NAME", "fake-sys"),
                ("CARGO_PKG_VERSION", "1.0.2"),
                ("TARGET", target),
                ("CARGO_MANIFEST_DIR", "/registry/src/index/fake-sys-1.0.2"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .to_vec();
            for it in features {
                env.push((format!("CARGO_FEATURE_{it}"), "1".to_owned()));
            }
            let given = BTreeSet::new();
            hand_with(PREBUILT_OR_SOURCE, &env, &out_dir, Some(&root), &given, Some(&store_dir))
        };
        let profile = root.join("target/debug").join(WRITTEN);
        let prebuilt = hand(&["BUILD", "DEFAULT", "X"]).unwrap();
        let at = profile.join(&twice.fetches[1].sha256);
        assert_eq!(prebuilt.env, [("FAKE_CACHE".to_owned(), at.to_str().unwrap().to_owned())]);
        assert_eq!(
            std::fs::read(at.join("binaries-1.0.2/prebuilt-build-x.tar.gz")).unwrap(),
            b"prebuilt"
        );

        let source = hand(&["BUILD", "Y"]).unwrap();
        let config: BTreeMap<&str, &str> =
            source.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(config.get("GIT_CONFIG_COUNT"), Some(&"2"));
        assert_eq!(config.get("GIT_CONFIG_VALUE_0"), Some(&"https://example.com/upstream"));
        let clone = dir.path().join("clone");
        let cloned = std::process::Command::new("git")
            .envs(source.env.iter().map(|(k, v)| (k, v)))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["clone", "-q", "--depth=1", "-b", "release/1.0", "https://example.com/upstream"])
            .arg(&clone)
            .status()
            .unwrap();
        assert!(cloned.success());
        assert_eq!(std::fs::read(clone.join("configure")).unwrap(), b"#!/bin/sh\n");

        let said = format!("{:#}", hand(&["BUILD", "Z"]).unwrap_err());
        assert!(said.contains("[\"build\", \"x\"]") && said.contains("--tests"), "{said}");
    }

    fn cargo_lock_of(root: &Path) -> Vec<Locked> {
        cargo_lock(&root.join("Cargo.lock")).unwrap()
    }

    #[test]
    fn a_download_unpacked_is_named_below_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut zipped = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut zipped);
        zip.start_file(
            "linux-x64-release/lib/libwebrtc.a",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        std::io::Write::write_all(&mut zip, b"!<arch>\n").unwrap();
        zip.finish().unwrap();
        let bytes = zipped.into_inner();
        let store_dir = dir.path().join("store");
        let store = blaze_store::Store::open_by_content(&store_dir).unwrap();
        let sha = sha256(&bytes);
        store.insert(&key(&sha).unwrap(), &bytes);
        store.seal();
        drop(store);
        let mut it = entry("webrtc-sys", "x86_64-unknown-linux-gnu");
        (it.sha256, it.names) = (sha.clone(), "linux-x64-release".to_owned());
        let hand = Hand::Unpacked { archive: Archive::Zip, into: "dfbin/{target}/{sha256}" };
        let named = write_out(&it, hand, &dir.path().join("w"), Some(&store_dir)).unwrap();
        let below =
            dir.path().join("w").join(&sha).join("dfbin/x86_64-unknown-linux-gnu").join(&sha);
        assert_eq!(named, below.join("linux-x64-release"));
        assert_eq!(std::fs::read(named.join("lib/libwebrtc.a")).unwrap(), b"!<arch>\n");
    }

    #[test]
    fn a_script_refused_the_network_is_told_what_override_it_lacks() {
        let line =
            |reached, what: &str| Line { reached, test: String::new(), what: what.to_owned() };
        let lines = [line(Reached::Net, "140.82.112.4:443"), line(Reached::Read, "/x")];
        let stdout = b"cargo:rerun-if-env-changed=SOME_SYS_ARCHIVE\n";
        let stderr =
            b"Error: failed to GET https://github.com/someone/some-sys/releases/a.zip: refused\n";
        let note = no_override_note("some-sys", &lines, stdout, stderr).unwrap();
        assert!(
            note.contains("`some-sys`")
                && note.contains("140.82.112.4:443")
                && note.contains("`github.com`"),
            "{note}"
        );
        assert!(
            note.contains("var: \"SOME_SYS_ARCHIVE\"")
                && note.contains("a patch to read a local file"),
            "{note}"
        );
        assert!(no_override_note("some-sys", &lines[1..], stdout, stderr).is_none());
        assert!(no_override_note("v8", &lines, b"", b"").unwrap().contains("blaze's override"));
    }

    #[test]
    fn of_several_versions_in_cargo_lock_the_one_of_the_directory_is_taken() {
        let locked = |source: &str| Locked {
            name: "webrtc-sys".to_owned(),
            version: "0.3.23".to_owned(),
            source: Some(source.to_owned()),
            checksum: None,
        };
        let them = [
            locked(
                "git+https://github.com/zed-industries/livekit-rust-sdks?rev=0a1c#0a1c519cfce9b365229026b55de9b9dbdb6fed3c",
            ),
            locked("registry+https://github.com/rust-lang/crates.io-index"),
        ];
        let git = Path::new("/h/.cargo/git/checkouts/livekit-rust-sdks-d281/0a1c519/webrtc-sys");
        assert!(locked_at(&them, "webrtc-sys", "0.3.23", git).unwrap().pin().starts_with("git+"));
        let registry = Path::new("/h/.cargo/registry/src/index/webrtc-sys-0.3.23");
        assert!(
            locked_at(&them, "webrtc-sys", "0.3.23", registry)
                .unwrap()
                .pin()
                .starts_with("registry+")
        );
        assert!(locked_at(&them, "webrtc-sys", "9.9.9", registry).is_err());
    }
}
