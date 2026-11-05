use std::path::PathBuf;

/// State directory: $HARNESS_HOME or ~/.harness.
pub fn home() -> PathBuf {
    std::env::var_os("HARNESS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME is unset")).join(".harness"))
}
pub fn sock() -> PathBuf { home().join("harnessd.sock") }
pub fn log() -> PathBuf { home().join("daemon.log") }
pub fn info() -> PathBuf { home().join("daemon.json") }

/// Directory holding the TypeScript kernel (src/mcp.ts): $HARNESS_ROOT, else `crates/harness/mcp` in the checkout this
/// binary was built from.
pub fn root() -> PathBuf {
    std::env::var_os("HARNESS_ROOT").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../mcp").canonicalize().expect("crates/harness/mcp")
    })
}
