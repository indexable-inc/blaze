//! `BLAZE_VERSION` and `BLAZE_VERSION_LINE`, which blaze this is (see [`version`]), and
//! `BLAZE_SOURCES`: what this build of blaze is built from, for the store to key what it
//! keeps by (`blaze_store::built_from`). A hash of every file of the workspace's crates, the
//! vendored ones included, of the manifest and lock file that pick the crates of crates.io, of
//! the files outside them a crate compiles in, of the features, the target and whether debug
//! assertions are on, which some of codegen checks under. The compiler that builds blaze is
//! not: what blaze compiles does not depend on it, and the toolchain it compiles against is
//! in the store's key apart. Two such builds of the same sources compile the same code into
//! the store, wherever they are built. Outside the workspace, as a published crate builds, it
//! is not set, and the store keys by the bytes of the executable.
//!
//! The SHA-256 is written here rather than taken from `sha2`: a build dependency is built for
//! the machine that builds, and `tools/nix/units` holds one graph per target computed on Linux, where
//! `sha2`'s `cpufeatures` needs no `libc`, which it needs on Apple's aarch64.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    version();
    println!("cargo:rerun-if-env-changed=CARGO_WORKSPACE_DIR");
    let Some(root) = std::env::var_os("CARGO_WORKSPACE_DIR").map(PathBuf::from) else { return };
    let (lock, crates, vendor) =
        (root.join("Cargo.lock"), root.join("crates"), root.join("vendor"));
    if !lock.is_file() || !crates.is_dir() {
        return;
    }
    let mut files = Vec::new();
    for top in [&crates, &vendor] {
        println!("cargo:rerun-if-changed={}", top.display());
        listed(top, &mut files);
    }
    // `harness/toolchain` is compiled in (`cargo/state.rs`).
    for file in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml", "harness/toolchain"] {
        let path = root.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        files.push(path);
    }
    files.sort();
    let mut hash = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        field(name.as_bytes());
        field(&bytes);
    }
    let mut features: Vec<String> =
        std::env::vars().map(|it| it.0).filter(|it| it.starts_with("CARGO_FEATURE_")).collect();
    features.sort();
    field(features.join(" ").as_bytes());
    field(std::env::var("TARGET").unwrap_or_default().as_bytes());
    field(&[u8::from(std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some())]);
    let hex: String = hash.finish().iter().map(|byte| format!("{byte:02x}")).collect();
    println!("cargo:rustc-env=BLAZE_SOURCES={hex}");
}

/// Which blaze this is, from the one place it is said: `BLAZE_RELEASE`, the tag a release is
/// built as (`v0.YYYYMMDD.N`), which only release.yaml sets. The workspace's own version in
/// Cargo.toml stays `0.0.0` on main, so that no release changes the identity of every crate
/// (and every crate's place in blaze's store); only this crate, which changes with every commit
/// anyway, is told the release. Sets
///
/// - `BLAZE_VERSION`: the release's `0.YYYYMMDD.N`; for a build of the source tree (Cargo.toml's
///   `0.0.0`, no `BLAZE_RELEASE`) `0.0.0-dev`, which is no release and which no pin names; for
///   a crate published with its version written in, that version.
/// - `BLAZE_VERSION_LINE`, what `--version` prints first: `blaze 0.20261028.9 (fb82b09
///   2026-10-28)` for a release, as rustc says its own, and `blaze 0.0.0-dev (fb82b09)` for a
///   build of the tree. The commit is `BLAZE_GIT_SHA` and `BLAZE_GIT_DATE` (release.yaml and the
///   flake set them, where there may be no `.git`), else the checkout's HEAD, else none. A
///   release must be told both; the version stays field 2 (.github/actions/blaze reads it).
///
/// The build runs again when HEAD moves (its reflog grows with each commit and checkout).
fn version() {
    for name in ["BLAZE_RELEASE", "BLAZE_GIT_SHA", "BLAZE_GIT_DATE"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let given = |name: &str| std::env::var(name).ok().filter(|it| !it.is_empty());
    let package = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let release = given("BLAZE_RELEASE");
    let version = match &release {
        Some(tag) => {
            let dated = tag.strip_prefix("v0.").and_then(|it| it.split_once('.')).is_some_and(
                |(day, n)| {
                    day.len() == 8
                        && day.bytes().all(|b| b.is_ascii_digit())
                        && !n.is_empty()
                        && n.bytes().all(|b| b.is_ascii_digit())
                        && (n == "0" || !n.starts_with('0'))
                },
            );
            assert!(dated, "BLAZE_RELEASE is {tag:?}, not a release's tag v0.YYYYMMDD.N");
            let version = tag[1..].to_owned();
            assert!(
                package == "0.0.0" || package == version,
                "BLAZE_RELEASE is {tag}, and the workspace's version is {package}: a release's is \
                 either the tree's 0.0.0 or its own"
            );
            version
        }
        None if package == "0.0.0" => "0.0.0-dev".to_owned(),
        None => package,
    };
    // TZ: the date is UTC's, as release.yaml's `BLAZE_GIT_DATE`.
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("TZ", "UTC0")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        String::from_utf8(out.stdout)
            .ok()
            .map(|it| it.trim().to_owned())
            .filter(|it| !it.is_empty())
    };
    let (sha, date) = match (given("BLAZE_GIT_SHA"), given("BLAZE_GIT_DATE")) {
        (Some(sha), date) => (Some(sha), date),
        (None, _) => {
            for path in ["HEAD", "logs/HEAD"] {
                if let Some(file) =
                    git(&["rev-parse", "--path-format=absolute", "--git-path", path])
                {
                    println!("cargo:rerun-if-changed={file}");
                }
            }
            let sha = git(&["rev-parse", "--short=7", "HEAD"]);
            let date = sha.as_ref().and_then(|_| {
                git(&["show", "-s", "--date=format-local:%Y-%m-%d", "--format=%cd", "HEAD"])
            });
            (sha, date)
        }
    };
    let line = match (&release, sha, date) {
        (Some(_), Some(sha), Some(date)) => format!("blaze {version} ({sha} {date})"),
        (Some(tag), sha, date) => panic!(
            "BLAZE_RELEASE is {tag}, and a release names its commit and the commit's date: \
             BLAZE_GIT_SHA is {sha:?} and BLAZE_GIT_DATE is {date:?}"
        ),
        (None, Some(sha), _) => format!("blaze {version} ({sha})"),
        (None, None, _) => format!("blaze {version}"),
    };
    println!("cargo:rustc-env=BLAZE_VERSION={version}");
    println!("cargo:rustc-env=BLAZE_VERSION_LINE={line}");
}

/// Every file below `dir`, but in hidden directories and in what cargo and npm build into.
fn listed(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            let built = name == "target" || name == "node_modules";
            if !name.to_string_lossy().starts_with('.') && !built {
                listed(&entry.path(), files);
            }
        } else {
            files.push(entry.path());
        }
    }
}

/// SHA-256 (FIPS 180-4).
struct Sha256 {
    state: [u32; 8],
    pending: Vec<u8>,
    len: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Sha256 {
        let state = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        Sha256 { state, pending: Vec::with_capacity(64), len: 0 }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.len += bytes.len() as u64;
        if !self.pending.is_empty() {
            let take = (64 - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() < 64 {
                return;
            }
            let block: [u8; 64] = self.pending[..].try_into().unwrap();
            self.compress(&block);
            self.pending.clear();
        }
        let (blocks, rest) = bytes.as_chunks::<64>();
        for block in blocks {
            self.compress(block);
        }
        self.pending.extend_from_slice(rest);
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.len * 8;
        let mut tail = vec![0x80];
        tail.resize((119 - self.pending.len()) % 64 + 1, 0);
        tail.extend_from_slice(&bits.to_be_bytes());
        self.update(&tail);
        let mut out = [0; 32];
        for (word, at) in self.state.iter().zip(out.as_chunks_mut::<4>().0) {
            *at = word.to_be_bytes();
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (word, at) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*at);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            (h, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (word, add) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(add);
        }
    }
}
