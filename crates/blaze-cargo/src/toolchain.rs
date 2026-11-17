//! blaze's own toolchain, the Rust `blaze` builds with: `blaze-<date>` ([`name`]), today the
//! Rust nightly of that date (`harness/toolchain`, docs/toolchain.md). The language and standard library are the
//! ones this blaze ships, and nothing else picks them: blaze ignores `RUSTC`,
//! `RUSTUP_TOOLCHAIN`, rustup's settings and the project's `rust-toolchain` files, and never
//! runs or asks a rustc to learn anything (docs/no-rustc.md). A project that needs another
//! Rust pins another blaze (`[workspace.metadata.blaze] version`).
//!
//! The files of the release that blaze's linker still takes (the sanitizer runtimes and
//! wasm32-wasip1's C runtime) are blaze's own copy of the release (`own.rs`), which
//! [`own::fetch`] downloads, each file checked against its sha256 in `pins.txt`. rustup's
//! directory is never read, not even as a copy of the same files: a toolchain found by its
//! directory's name is trusted by name, and its bytes are whatever was installed there
//! (docs/v2/plan/design/toolchain-c.md, slice 2). The source of the standard library is not the
//! release's: it is blaze's own `library/` ([`library`]).

pub mod library;
pub mod own;

use std::path::{Path, PathBuf};

use blaze_registry::http::{Http as _, HttpNotSuccessful, HyperHttp};

/// blaze's toolchain as blaze names it in what it prints: `blaze-2026-10-25`, after the date of
/// the Rust nightly it is (`harness/toolchain`), so it moves with every bump. rustup knows the
/// toolchain by the nightly's name, not this one.
pub fn name() -> String {
    let ours = blaze_units::lock::toolchain();
    format!("blaze-{}", ours.strip_prefix("nightly-").unwrap_or(ours))
}

/// What that toolchain's rustc says it is after `rustc `: `1.101.0-nightly (12433c307 2026-10-24)`.
pub fn ours_says() -> &'static str {
    own::rustc_version()
}

/// That rustc's release, as `rustc -vV` says it: `1.101.0-nightly`.
pub fn ours_release() -> &'static str {
    ours_says().split_once(' ').map_or(ours_says(), |(release, _)| release)
}

/// The commit that toolchain was built from.
pub fn ours_commit() -> &'static str {
    own::commit()
}

/// The host's tuple, as the Rust project names its builds: what blaze answers `rustc -vV`'s
/// `host:` with. `None` on a host blaze ships no toolchain for.
pub fn host() -> Option<&'static str> {
    own::host()
}

/// `blaze toolchain --json`'s object: blaze's toolchain as blaze and rustup name it, the rustc
/// it is, the host's tuple and where its files are. What a harness asks instead of running
/// `rustc -vV` (docs/toolchain.md).
#[derive(serde::Serialize)]
struct ToolchainJson<'a> {
    /// `blaze-2026-10-27` ([`name`]).
    name: String,
    /// `nightly-2026-10-27` ([`blaze_units::lock::toolchain`]).
    nightly: &'a str,
    /// `1.101.0-nightly (12433c307 2026-10-24)` ([`ours_says`]).
    rustc: &'a str,
    /// `1.101.0-nightly` ([`ours_release`]).
    release: &'a str,
    commit: &'a str,
    /// The host's tuple ([`host`]): `null` on a host blaze ships no toolchain for.
    host: Option<&'a str>,
    sysroot: &'a Path,
}

/// [`ToolchainJson`] of `chosen`, one line, newline ended.
pub fn json(chosen: &Chosen) -> Result<String, String> {
    let json = ToolchainJson {
        name: name(),
        nightly: blaze_units::lock::toolchain(),
        rustc: ours_says(),
        release: ours_release(),
        commit: ours_commit(),
        host: host(),
        sysroot: chosen.dir(),
    };
    // alloc: the CLI's answer, once
    let mut line = serde_json::to_string(&json)
        .map_err(|error| format!("`blaze toolchain --json`'s answer as JSON: {error}"))?;
    line.push('\n');
    Ok(line)
}

/// Where the files of blaze's release are: blaze's own copy, never rustup's.
pub struct Chosen(PathBuf);

impl Chosen {
    /// Where the toolchain is: its sysroot.
    pub fn dir(&self) -> &Path {
        &self.0
    }
}

impl std::fmt::Display for Chosen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, ours) = (name(), blaze_units::lock::toolchain());
        write!(f, "{name}, blaze's toolchain (Rust {ours}), at {}", self.0.display())
    }
}

/// The files of blaze's release, found with nothing run: blaze's own copy when it has every
/// component `pins.txt` pins. `None` when it is not there or not whole.
pub fn found() -> Option<Chosen> {
    if let Some(dir) = FOUND.get() {
        return Some(Chosen(dir.clone()));
    }
    let release = own::Release::ours()?;
    let dir = release.dir().ok()?;
    if !own::complete(&release, &dir) {
        return None;
    }
    let _ = FOUND.set(dir.clone());
    Some(Chosen(dir))
}

/// [`found`], else blaze's own copy of the release, downloaded once: what a command needs
/// before the daemon builds with it. `Err` says why there is none and what to do.
pub fn ensure() -> Result<Chosen, String> {
    if let Some(chosen) = found() {
        return Ok(chosen);
    }
    let ours = blaze_units::lock::toolchain();
    let Some(release) = own::Release::ours() else {
        return Err(format!(
            "blaze builds with Rust {ours}, and the Rust project publishes no copy of it for this machine that blaze knows"
        ));
    };
    // Why there is no directory names what was tried and the setting that names one
    // (`own::toolchains`, `BLAZE_TOOLCHAINS`).
    let dir = release.dir().map_err(|why| format!("blaze builds with Rust {ours}: {why}"))?;
    own::fetch(&release, &dir)?;
    let _ = FOUND.set(dir.clone());
    Ok(Chosen(dir))
}

/// Where [`found`] found blaze's release, or was told it is ([`known_at`]).
#[expect(
    clippy::disallowed_types,
    reason = "where this blaze's release is: found on disk once, or told by the blaze that started the process, then fixed for it"
)]
static FOUND: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Tells this process where blaze's release is, as the blaze that started it found it: a
/// build script's sandbox may not see where blaze keeps its toolchain.
pub fn known_at(sysroot: PathBuf) {
    let _ = FOUND.set(sysroot);
}

/// The sysroot of blaze's release: what blaze answers `rustc --print sysroot` with. Looked for
/// on disk once ([`found`]), then fixed for the process: the daemon asks at its start, so a
/// build's task only reads it.
pub fn sysroot() -> Option<&'static Path> {
    if FOUND.get().is_none() {
        found()?;
    }
    FOUND.get().map(PathBuf::as_path)
}

/// `file` under `base` (an address ending in `/`), whole, through blaze-registry's HTTP client:
/// asked once (rule no-fallback: no retry), in parallel ranges said in blaze-fetch's downloads
/// (the status line, `blaze status`, the build's stall bound) and stopped after its stall bound
/// without a byte. Any failure, a server with no such file among them, is the error, its
/// context the file and its address ([`DownloadFailed`]).
fn download_whole(http: &HyperHttp, base: &url::Url, file: &str) -> anyhow::Result<Vec<u8>> {
    let url = base.join(file)?;
    let got = (|| -> anyhow::Result<Vec<u8>> {
        // No bound of its own: every wait of the request is bounded by the client's deadlines
        // (its timeout and the stall bound), as the wait's name says.
        let what = "a download of blaze's files (bounded by the HTTP client's timeout and stall bound)";
        let response = blaze_core::wait::block_on(what, None, http.get_from_async(url.as_str(), &[], 0, file))??;
        match response.status {
            200 | 206 => Ok(response.body),
            404 | 410 => anyhow::bail!("{url} answered {}: the server has no such file", response.status),
            _ => Err(HttpNotSuccessful::new(url.as_str(), &response).into()),
        }
    })();
    got.map_err(|e| e.context(DownloadFailed::of(file, url.to_string())))
}

/// A download of one of blaze's files that failed. It was asked for once (rule no-fallback:
/// blaze tries nothing again); the error it is the context of is what failed.
#[derive(Debug)]
struct DownloadFailed {
    file: String,
    /// Its address, or its path in a directory that has the files.
    from: String,
}

impl DownloadFailed {
    fn of(file: &str, from: String) -> DownloadFailed {
        DownloadFailed { file: file.to_owned(), from }
    }
}

impl std::fmt::Display for DownloadFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "downloading {} from {}", self.file, self.from)
    }
}

/// A variable that names a place to get blaze's files from and is not UTF-8.
#[derive(Debug)]
struct NotUtf8Var(&'static str);

impl std::fmt::Display for NotUtf8Var {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` is not UTF-8, and blaze reads the address or directory it names as text",
            self.0
        )
    }
}

impl std::error::Error for NotUtf8Var {}

/// The variable `name` when it is set and not empty.
fn given_var(name: &'static str) -> Result<Option<String>, NotUtf8Var> {
    match std::env::var(name) {
        Ok(it) if !it.is_empty() => Ok(Some(it)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(NotUtf8Var(name)),
    }
}

/// `address` as the base a download joins file names to ([`download_whole`]).
fn base(address: &str) -> anyhow::Result<url::Url> {
    Ok(url::Url::parse(&format!("{}/", address.trim_end_matches('/')))?)
}

/// The `rust-toolchain` or `rust-toolchain.toml` nearest `dir`, which rustup would follow and
/// blaze does not.
pub fn toolchain_file(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .flat_map(|it| ["rust-toolchain", "rust-toolchain.toml"].map(|name| it.join(name)))
        .find(|it| it.is_file())
}

/// Says, under `-v`, that the toolchain file nearest `dir` is not followed: one line, since
/// nothing about the build depends on it.
pub fn say_toolchain_file(dir: &Path, verbose: bool) {
    // Looked for only when it is to be said: two stats a directory above `dir` otherwise.
    if verbose && let Some(file) = toolchain_file(dir) {
        eprintln!(
            "blaze: {} is ignored: blaze builds with its own Rust, {} ({})",
            file.display(),
            ours_release(),
            name()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_toolchain_file_is_found() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        assert_eq!(toolchain_file(&inner).filter(|it| it.starts_with(root.path())), None);
        std::fs::write(
            root.path().join("a/rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.70\"\n",
        )
        .unwrap();
        assert_eq!(toolchain_file(&inner), Some(root.path().join("a/rust-toolchain.toml")));
        std::fs::write(root.path().join("a/b/rust-toolchain"), "1.70\n").unwrap();
        assert_eq!(toolchain_file(&inner), Some(inner.join("rust-toolchain")), "the nearest");
    }

    #[test]
    fn blazes_toolchain_is_named_after_its_nightly_and_its_rustc_says_its_release() {
        let date = blaze_units::lock::toolchain()
            .strip_prefix("nightly-")
            .expect("harness/toolchain names a nightly");
        assert_eq!(name(), format!("blaze-{date}"));
        assert!(ours_says().starts_with(ours_release()), "{}", ours_says());
        semver::Version::parse(ours_release()).expect("a release is a version");
    }

    /// A server on a local port that answers every request with a 500 and closes the
    /// connection: its address (ending in `/`) and the path of each request it was asked, in
    /// the order it was asked. It takes at most `most` connections, each on a thread of its own.
    pub(super) fn failing_server(
        most: usize,
    ) -> (String, std::sync::Arc<blaze_core::lock::Lock<Vec<String>>, std::alloc::Global>) {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/", listener.local_addr().unwrap());
        let asked = std::sync::Arc::new_in(
            blaze_core::lock::Lock::new("test.toolchain.asked", Vec::new()),
            std::alloc::Global,
        );
        let told = std::sync::Arc::clone(&asked);
        std::thread::Builder::new()
            .name("test: a server that answers 500".to_owned())
            .spawn(move || {
                for _ in 0..most {
                    let Ok((mut stream, _)) = listener.accept() else { return };
                    let told = std::sync::Arc::clone(&told);
                    std::thread::Builder::new()
                        .name("test: one connection of a server that answers 500".to_owned())
                        .spawn(move || {
                            // The request's head, at most 64 KiB of it.
                            let (mut head, mut piece) = (Vec::new(), [0u8; 4096]);
                            while !head.windows(4).any(|it| it == b"\r\n\r\n") && head.len() < 64 << 10 {
                                match stream.read(&mut piece) {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => head.extend_from_slice(&piece[..n]),
                                }
                            }
                            let line = String::from_utf8_lossy(&head).lines().next().unwrap_or("").to_owned();
                            told.lock().push(line.split(' ').nth(1).unwrap_or("").to_owned());
                            _ = stream.write_all(
                                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope",
                            );
                        })
                        .unwrap();
                }
            })
            .unwrap();
        (address, asked)
    }

    /// The invariant (rule no-fallback): a download that fails is asked once, and its error
    /// names the file and its address. The control: the server saw that one request.
    #[test]
    fn a_failed_download_is_asked_once_and_names_its_address() {
        let (address, asked) = failing_server(8);
        let http = HyperHttp::new(std::time::Duration::from_secs(10)).unwrap();
        let said = download_whole(&http, &base(&address).unwrap(), "library-ab.tar.xz").unwrap_err();
        let said = format!("{said:#}");
        let url = format!("{address}library-ab.tar.xz");
        assert!(said.starts_with(&format!("downloading library-ab.tar.xz from {url}: ")), "{said}");
        assert!(said.contains("got 500"), "{said}");
        assert_eq!(*asked.lock(), ["/library-ab.tar.xz"], "one request, and no other");
    }

    #[test]
    fn a_download_that_failed_names_its_file_and_address_and_a_variable_its_name() {
        let error = anyhow::anyhow!("connection reset").context(DownloadFailed::of(
            "library-ab.tar.xz",
            "https://example.com/library-ab.tar.xz".to_owned(),
        ));
        assert_eq!(
            format!("{error:#}"),
            "downloading library-ab.tar.xz from https://example.com/library-ab.tar.xz: connection reset"
        );
        assert!(error.downcast_ref::<DownloadFailed>().is_some_and(|it| it.file == "library-ab.tar.xz"));
        assert_eq!(
            NotUtf8Var("BLAZE_RUST_DIST").to_string(),
            "`BLAZE_RUST_DIST` is not UTF-8, and blaze reads the address or directory it names as text"
        );
    }
}
