//! On macOS, the sandbox's library (`BLAZE_SANDBOX_LIBRARY`). The SHA-256 is written here rather
//! than taken from `sha2`, as in crates/blaze-engine/build.rs: a build dependency is built for
//! the machine that builds. Which blaze this is (its version and commit) is crates/blaze-engine's
//! build.rs's, the one crate that changes with every commit anyway, and reaches this crate
//! through `engine::Engine`: this crate's build stays the same from commit to commit.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    sandbox_library();
}

/// On macOS, the library every process of a sandbox loads (`src/sandbox/macos_interpose.c`),
/// built with the C compiler of the system (`CC` when set), which `src/sandbox/macos.rs`
/// carries in the executable. A C file and no build dependency: dyld loads only a dynamic
/// library, which cargo builds from Rust only as a crate of its own.
fn sandbox_library() {
    let source = "src/sandbox/macos_interpose.c";
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    // Apple's own programs on Apple silicon (`cc`, `clang`, `ld`) are arm64e, and where macOS
    // does not drop `DYLD_*` for them (SIP off, as on GitHub's macOS runners) dyld aborts one
    // whose inserted library has no arm64e slice: every C build of a build script under the
    // sandbox (#147). The arm64 slice is for every other program.
    let arches: &[&str] = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => &["-arch", "arm64", "-arch", "arm64e"],
        Ok("x86_64") => &["-arch", "x86_64"],
        other => panic!("no sandbox library for the architecture {other:?}"),
    };
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("blaze_sandbox.dylib");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let target = std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".to_owned());
    let status = std::process::Command::new(&cc)
        .args(["-dynamiclib", "-O2", "-Wall"])
        .args(arches)
        .arg(format!("-mmacosx-version-min={target}"))
        .arg("-o")
        .arg(&out)
        .arg(source)
        .status()
        .unwrap_or_else(|error| panic!("{cc} did not run to build {source}: {error}"));
    assert!(status.success(), "{cc} could not build {source}");
    // Its name in the user's cache, by its contents, worked out here rather than at each run.
    let mut hash = Sha256::new();
    hash.update(&std::fs::read(&out).unwrap());
    let hex: String = hash.finish().iter().take(12).map(|byte| format!("{byte:02x}")).collect();
    println!("cargo:rustc-env=BLAZE_SANDBOX_LIBRARY={hex}");
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
