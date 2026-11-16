//! The key of a run's record (rules 21 and 65): a blaze-record base key over everything a run's
//! outputs can depend on that is known before it runs; the tracked reads are its read list,
//! filed under the base; the answer is the outputs record ([`crate::outputs`]).
//!
//! The base covers:
//!
//! - the module: the digest of the script's `wasm32-wasip1` bytes (the module record already
//!   keys the script crate, its build-dependencies, the toolchain and the shim);
//! - the environment given, whole ([`crate::env::Env::encode`]): wasi-libc copies it at start,
//!   so a read of one variable cannot be told from the rest, and the unit's configuration is
//!   what it varies with anyway;
//! - the answers the sandbox gives to a spawn: the pinned compiler's version text and the
//!   probe table ([`crate::probe::answers_id`]), and the C mode ([`CMode`]);
//! - the runner's limits, which decide whether a run is cut off.
//!
//! The run's reads (`file:`, `dir:`, `stat:` subjects with the digests found, as the sandbox
//! records them) are not in the base: they are the read list, so an edit of a file the script
//! did not read leaves its record found.

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};

use crate::env::Env;

/// What a spawn of a C compiler (or `ar`, `cmake`, `pkg-config`) is answered with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CMode {
    /// For `check`: the request is kept in the record and nothing is compiled. A `build`
    /// never takes a record made so.
    Deferred = 0,
    /// For `build`: each request answered by its own record from a sandboxed C toolchain (not
    /// built).
    Built = 1,
}

/// The runner's limits, as the key sees them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RunLimits {
    pub fuel: u64,
    pub memory: u64,
    pub out_dir_bytes: u64,
    pub out_dir_files: u32,
    pub output: u64,
    pub line: u64,
}

impl RunLimits {
    /// The design's limits (build-scripts.md): fuel tens of times the largest generator's,
    /// 1 GiB of memory, `OUT_DIR` 64 MiB in 4096 files, 16 MiB of output, 256 KiB a line.
    pub const STANDARD: RunLimits =
        RunLimits { fuel: 1 << 36, memory: 1 << 30, out_dir_bytes: 64 << 20, out_dir_files: 4096, output: 16 << 20, line: 256 << 10 };
}

/// What the base key is made of.
#[derive(Clone, Copy, Debug)]
pub struct KeyInputs<'k> {
    /// The SHA-256 of the module's bytes.
    pub module: [u8; 32],
    pub env: &'k Env<'k>,
    pub answers: ContentId,
    pub c_mode: CMode,
    pub limits: RunLimits,
    pub runner: RunnerKey<'k>,
}

/// Where the run happens, as the key sees it (D65, [`crate::native::runner`]): decided from the
/// dependency closure before the run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunnerKey<'k> {
    /// Wasm: adds nothing, so the keys of wasm records made before D65 stand.
    Wasm,
    /// Native: the pinned C toolchain's shelf digest, the host target the binary is for, and the
    /// C headers' identity known before the run (D69): `macos-sdk <version>` (the version from
    /// the SDK's `SDKSettings.json`), or a pinned sysroot's digest. The headers actually read
    /// are the record's read list ([`crate::toolchain::headers_digest`]), not its key.
    ///
    /// `host_tools`: the host tools it is granted (D71, [`crate::host_tools`]), sorted by name,
    /// each by the SHA-256 of its binary; none adds nothing, so the keys of native records
    /// without one stand.
    Native { compiler: [u8; 32], target: &'k str, sysroot: &'k str, host_tools: &'k [HostToolKey<'k>] },
}

/// A granted host tool as the key sees it: its name and its binary's SHA-256 (never its path,
/// which is the machine's).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HostToolKey<'k> {
    pub name: &'k str,
    pub digest: [u8; 32],
}

/// The domain of the base key; a change to what it covers takes the next number.
/// The outputs record's layout word is hashed in too, so a record of another layout is never
/// looked up under a key of this one.
pub const DOMAIN: &str = "blaze v2 build-script run 2";

/// The base key.
pub fn base(inputs: &KeyInputs<'_>, scratch: &mut blaze_own::AVec<'_, u8>) -> ContentId {
    scratch.clear();
    inputs.env.encode(scratch);
    let l = inputs.limits;
    let mut hasher = Hasher::new(DOMAIN);
    hasher
        .update(&crate::outputs::LAYOUT.to_le_bytes())
        .part(&inputs.module)
        .part(scratch)
        .id(&inputs.answers)
        .update(&[inputs.c_mode as u8])
        .update(&l.fuel.to_le_bytes())
        .update(&l.memory.to_le_bytes())
        .update(&l.out_dir_bytes.to_le_bytes())
        .update(&l.out_dir_files.to_le_bytes())
        .update(&l.output.to_le_bytes())
        .update(&l.line.to_le_bytes());
    if let RunnerKey::Native { compiler, target, sysroot, host_tools } = inputs.runner {
        hasher.part(b"native").part(&compiler).part(target.as_bytes()).part(sysroot.as_bytes());
        if !host_tools.is_empty() {
            hasher.part(b"host-tools").update(&(host_tools.len() as u64).to_le_bytes());
            for tool in host_tools {
                hasher.part(tool.name.as_bytes()).part(&tool.digest);
            }
        }
    }
    hasher.finish()
}
