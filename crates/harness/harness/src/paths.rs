use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// State directory: $HARNESS_HOME or ~/.harness.
pub fn home() -> PathBuf {
    std::env::var_os("HARNESS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME is unset")).join(".harness"))
}
pub fn sock() -> PathBuf { home().join("harnessd.sock") }
pub fn log() -> PathBuf { home().join("daemon.log") }
pub fn info() -> PathBuf { home().join("daemon.json") }

/// The TypeScript kernel's file, under [`root`].
const KERNEL: &str = "src/mcp.ts";

/// Where the TypeScript kernel (src/mcp.ts) is not, and how to say where it is. Never a path baked in at build time: a
/// binary built from a worktree that was deleted later (a shared target) must not point into the deleted worktree.
#[derive(Debug)]
pub enum KernelMissing {
    /// $HARNESS_ROOT names a directory without src/mcp.ts.
    Env(PathBuf),
    /// No directory above the running binary has mcp/src/mcp.ts (a binary outside a blaze checkout's crates/harness).
    NearExe(PathBuf),
    /// The running binary's own path is unknown.
    NoExe(std::io::Error),
}

impl std::fmt::Display for KernelMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const HOW: &str = "set HARNESS_ROOT to the directory holding src/mcp.ts (crates/harness/mcp in a blaze checkout)";
        match self {
            Self::Env(p) => write!(f, "harness's kernel: HARNESS_ROOT is {}, which has no {KERNEL}; {HOW}", p.display()),
            Self::NearExe(exe) => write!(
                f,
                "harness's kernel: no directory above the running binary {} has mcp/{KERNEL} (it was built or copied outside a blaze checkout's crates/harness); {HOW}",
                exe.display()
            ),
            Self::NoExe(e) => write!(f, "harness's kernel: the running binary's path is unknown ({e}); {HOW}"),
        }
    }
}

impl std::error::Error for KernelMissing {}

/// Find the kernel's directory: $HARNESS_ROOT, else the `mcp` directory beside the nearest ancestor of the running
/// binary that has one (`crates/harness/target*/<profile>/harness` finds `crates/harness/mcp`).
fn find_root() -> Result<PathBuf, KernelMissing> {
    if let Some(p) = std::env::var_os("HARNESS_ROOT").map(PathBuf::from) {
        return match p.join(KERNEL).is_file() {
            true => Ok(p.canonicalize().unwrap_or(p)),
            false => Err(KernelMissing::Env(p)),
        };
    }
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).map_err(KernelMissing::NoExe)?;
    exe.ancestors().map(|a| a.join("mcp")).find(|m| m.join(KERNEL).is_file()).ok_or(KernelMissing::NearExe(exe))
}

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Directory holding the TypeScript kernel (src/mcp.ts), found once ([`find_root`]); the daemon asks at start and
/// refuses to start without it.
///
/// # Errors
/// [`KernelMissing`]: where it looked and how to set it.
pub fn root() -> Result<&'static Path, KernelMissing> {
    if let Some(p) = ROOT.get() {
        return Ok(p);
    }
    let p = find_root()?;
    Ok(ROOT.get_or_init(|| p))
}
