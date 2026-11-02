#![forbid(unsafe_code)]

use std::{collections::VecDeque, io::Write, path::Path, sync::Arc};

use blaze_registry::http::{Http, Response, Retry, Transport};
use blaze_units::query::{Fetch, FetchKind};
use blaze_vfs::Mode;
use parking_lot::Mutex;

use super::{Fetcher, redact};
use crate::serve::work::{Source, SourceFile};

/// What the fake server answers, in order.
enum Answer {
    Status(u16, Vec<u8>),
    /// A connection reset: no answer.
    Reset,
}

#[derive(Default)]
struct Fake {
    answers: Mutex<VecDeque<Answer>>,
    asked: Mutex<Vec<(String, Vec<(String, String)>)>>,
}

impl Fake {
    fn new(answers: Vec<Answer>) -> Arc<Fake> {
        Arc::new(Fake { answers: Mutex::new(answers.into()), asked: Mutex::default() })
    }

    fn asked(&self) -> Vec<(String, Vec<(String, String)>)> {
        self.asked.lock().clone()
    }
}

impl Http for Fake {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        let headers = headers.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        self.asked.lock().push((url.to_owned(), headers));
        match self.answers.lock().pop_front().expect("asked more than the test answers") {
            Answer::Status(status, body) => Ok(Response { status, headers: Vec::new(), body }),
            Answer::Reset => Err(Transport(format!("asking {url}: connection reset")).into()),
        }
    }
}

fn fetcher(fake: &Arc<Fake>) -> Fetcher<Fake> {
    Fetcher::with_http(fake.clone(), std::env::temp_dir().join("blaze-fetch-tests")).retrying(Retry {
        retries: 3,
        quiet: true,
        fixed_sleep_ms: Some(0),
    })
}

enum Entry<'a> {
    File(&'a str, &'a [u8], u32),
    Dir(&'a str),
    Link(&'a str, &'a str),
}

/// A gzipped tar of `entries`, their names written as they are, checked by nothing.
fn tarball(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        let (path, data): (&str, &[u8]) = match *entry {
            Entry::File(path, data, mode) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(mode);
                (path, data)
            }
            Entry::Dir(path) => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                (path, b"")
            }
            Entry::Link(path, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.as_old_mut().linkname[..target.len()].copy_from_slice(target.as_bytes());
                (path, b"")
            }
        };
        header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

const SERDE: &str = "serde v1.0.219";

fn crate_fetch(sha256: &str) -> Fetch {
    Fetch {
        what: SERDE.to_owned(),
        url: "https://static.crates.io/crates/serde/1.0.219/download".to_owned(),
        kind: FetchKind::Crate { sha256: sha256.to_owned() },
        at: "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.219".to_owned(),
    }
}

/// `entries` served as serde's `.crate`, under its right sha256.
fn fetch_crate(entries: &[Entry<'_>]) -> Result<Source, blaze_api::ApiError> {
    let bytes = tarball(entries);
    let sha256 = blaze_registry::download::checksum(&bytes);
    let fake = Fake::new(vec![Answer::Status(200, bytes)]);
    fetcher(&fake).fetch(&crate_fetch(&sha256))
}

fn file(path: &str, bytes: &[u8], mode: Mode) -> SourceFile {
    SourceFile { path: path.to_owned(), bytes: bytes.to_vec(), mode }
}

#[test]
fn a_fetcher_is_shared_by_the_io_threads() {
    fn shared<T: Send + Sync>() {}
    shared::<Fetcher<blaze_registry::http::HyperHttp>>();
}

#[test]
fn a_crate_unpacks_to_its_files_under_its_prefix() {
    let source = fetch_crate(&[
        Entry::File("serde-1.0.219/Cargo.toml", b"[package]\n", 0o644),
        Entry::File("serde-1.0.219/build.sh", b"#!/bin/sh\n", 0o755),
        Entry::Dir("serde-1.0.219/src/"),
        Entry::File("serde-1.0.219/src/lib.rs", b"pub fn f() {}\n", 0o644),
        Entry::File("serde-1.0.219/.cargo-ok", b"not cargo's", 0o644),
    ])
    .unwrap();
    assert_eq!(source.under, "");
    assert_eq!(
        source.files,
        vec![
            file(".cargo-ok", br#"{"v":1}"#, Mode::Regular),
            file("Cargo.toml", b"[package]\n", Mode::Regular),
            file("build.sh", b"#!/bin/sh\n", Mode::Executable),
            file("src/lib.rs", b"pub fn f() {}\n", Mode::Regular),
        ]
    );
}

#[test]
fn a_crate_of_another_sha256_is_refused_naming_both() {
    let bytes = tarball(&[Entry::File("serde-1.0.219/Cargo.toml", b"", 0o644)]);
    let actual = blaze_registry::download::checksum(&bytes);
    let locked = "ab".repeat(32);
    let fake = Fake::new(vec![Answer::Status(200, bytes)]);
    let error = fetcher(&fake).fetch(&crate_fetch(&locked)).unwrap_err();
    assert!(error.what.starts_with("failed to verify the checksum of `serde v1.0.219`"), "{}", error.what);
    assert!(error.what.contains(&locked), "{}", error.what);
    assert!(error.what.contains(&actual), "{}", error.what);
    assert_eq!(error.at.as_deref(), Some(SERDE));
}

#[test]
fn a_local_registrys_crate_is_read_in_place_and_checked() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = tarball(&[Entry::File("serde-1.0.219/Cargo.toml", b"[package]\n", 0o644)]);
    let at = dir.path().join("serde-1.0.219.crate");
    std::fs::write(&at, &bytes).unwrap();
    let local = |sha256: &str| Fetch { url: format!("file://{}", at.display()), ..crate_fetch(sha256) };
    let fake = Fake::new(Vec::new());
    let source = fetcher(&fake).fetch(&local(&blaze_registry::download::checksum(&bytes))).unwrap();
    assert_eq!(
        source.files,
        vec![file(".cargo-ok", br#"{"v":1}"#, Mode::Regular), file("Cargo.toml", b"[package]\n", Mode::Regular)]
    );
    let error = fetcher(&fake).fetch(&local(&"ab".repeat(32))).unwrap_err();
    assert!(error.what.starts_with("failed to verify the checksum of `serde v1.0.219`"), "{}", error.what);
    std::fs::remove_file(&at).unwrap();
    let error = fetcher(&fake).fetch(&local(&"ab".repeat(32))).unwrap_err();
    assert!(error.what.starts_with(&format!("failed to read `{}`", at.display())), "{}", error.what);
    assert!(fake.asked().is_empty());
}

#[test]
fn entries_that_leave_the_package_are_refused_by_name() {
    let refused = |entry: Entry<'_>, named: &str| {
        let error = fetch_crate(&[Entry::File("serde-1.0.219/Cargo.toml", b"", 0o644), entry])
            .unwrap_err();
        assert!(error.what.starts_with("failed to unpack package `serde v1.0.219`"), "{}", error.what);
        assert!(error.what.contains(named), "{named}: {}", error.what);
        assert_eq!(error.at.as_deref(), Some(SERDE));
    };
    refused(Entry::File("../x", b"", 0o644), "\"../x\"");
    refused(Entry::File("/x", b"", 0o644), "\"/x\"");
    refused(Entry::File("serde-1.0.218/x", b"", 0o644), "\"serde-1.0.218/x\"");
    refused(Entry::File("serde-1.0.219/../x", b"", 0o644), "\"serde-1.0.219/../x\"");
    refused(Entry::File("serde-1.0.219/./x", b"", 0o644), "\"serde-1.0.219/./x\"");
    refused(Entry::File("serde-1.0.219//x", b"", 0o644), "\"serde-1.0.219//x\"");
    refused(Entry::Link("serde-1.0.219/etc", "../../etc"), "\"serde-1.0.219/etc\"");
}

#[test]
fn a_path_twice_is_refused() {
    let error = fetch_crate(&[
        Entry::File("serde-1.0.219/src/lib.rs", b"a", 0o644),
        Entry::File("serde-1.0.219/src/lib.rs", b"b", 0o644),
    ])
    .unwrap_err();
    assert!(error.what.contains("\"serde-1.0.219/src/lib.rs\" twice"), "{}", error.what);

    let error = fetch_crate(&[
        Entry::File("serde-1.0.219/src", b"a", 0o644),
        Entry::File("serde-1.0.219/src/lib.rs", b"b", 0o644),
    ])
    .unwrap_err();
    assert!(error.what.contains("\"serde-1.0.219/src/lib.rs\""), "{}", error.what);
}

fn index_fetch() -> Fetch {
    Fetch {
        what: "serde's entry in the crates.io index".to_owned(),
        url: "https://index.crates.io/se/rd/serde".to_owned(),
        kind: FetchKind::IndexFile,
        at: "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/se/rd/serde"
            .to_owned(),
    }
}

#[test]
fn an_index_file_is_kept_as_served_under_its_name() {
    let line = br#"{"name":"serde","vers":"1.0.219","deps":[],"cksum":"00","features":{}}"#;
    let fake = Fake::new(vec![Answer::Status(200, line.to_vec())]);
    let source = fetcher(&fake).fetch(&index_fetch()).unwrap();
    assert_eq!(source, Source { files: vec![file("serde", line, Mode::Regular)], under: "serde".to_owned() });
    let asked = fake.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, "https://index.crates.io/se/rd/serde");
    assert!(asked[0].1.contains(&("cargo-protocol".to_owned(), "version=1".to_owned())));

    let fake = Fake::new(vec![Answer::Status(200, Vec::new())]);
    let source = fetcher(&fake).fetch(&index_fetch()).unwrap();
    assert_eq!(source.files, vec![file("serde", b"", Mode::Regular)]);
}

#[test]
fn an_index_without_the_file_gives_no_file() {
    for status in [404, 410, 451] {
        let fake = Fake::new(vec![Answer::Status(status, b"not found".to_vec())]);
        let source = fetcher(&fake).fetch(&index_fetch()).unwrap();
        assert_eq!(source, Source { files: Vec::new(), under: "serde".to_owned() });
    }
}

#[test]
fn a_transient_failure_is_tried_again_and_a_permanent_one_is_not() {
    let bytes = tarball(&[Entry::File("serde-1.0.219/Cargo.toml", b"", 0o644)]);
    let sha256 = blaze_registry::download::checksum(&bytes);
    let fake = Fake::new(vec![Answer::Reset, Answer::Status(503, Vec::new()), Answer::Status(200, bytes)]);
    fetcher(&fake).fetch(&crate_fetch(&sha256)).unwrap();
    assert_eq!(fake.asked().len(), 3);

    let fake = Fake::new(vec![Answer::Status(403, b"forbidden".to_vec())]);
    let error = fetcher(&fake).fetch(&crate_fetch(&sha256)).unwrap_err();
    assert_eq!(fake.asked().len(), 1);
    assert!(error.what.starts_with("failed to download `serde v1.0.219`"), "{}", error.what);
    assert!(error.what.contains("got 403"), "{}", error.what);

    let fake = Fake::new((0..4).map(|_| Answer::Status(500, Vec::new())).collect());
    let error = fetcher(&fake).fetch(&index_fetch()).unwrap_err();
    assert_eq!(fake.asked().len(), 4);
    assert!(error.what.contains("got 500"), "{}", error.what);
}

#[test]
fn no_userinfo_is_said() {
    assert_eq!(
        redact("failed to get `https://me:s3cret@example.com/x`, and git://tok@h/y"),
        "failed to get `https://example.com/x`, and git://h/y"
    );
    assert_eq!(redact("no url here"), "no url here");

    let fake = Fake::new(vec![Answer::Status(404, Vec::new())]);
    let mut fetch = crate_fetch(&"00".repeat(32));
    fetch.url = "https://me:s3cret@example.com/serde.crate".to_owned();
    let error = fetcher(&fake).fetch(&fetch).unwrap_err();
    assert!(!error.what.contains("s3cret"), "{}", error.what);
    assert!(error.what.contains("https://example.com/serde.crate"), "{}", error.what);
}

#[test]
fn a_checkout_is_where_cargo_keeps_it() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let home = super::git::cargo_home(
        "/home/u/.cargo/git/checkouts/serde-1234567890abcdef/0123456",
        "serde-1234567890abcdef",
        commit,
    )
    .unwrap();
    assert_eq!(home, Path::new("/home/u/.cargo"));
    for at in [
        "/home/u/.cargo/git/checkouts/other-1234567890abcdef/0123456",
        "/home/u/.cargo/git/db/serde-1234567890abcdef/0123456",
        "/home/u/.cargo/git/checkouts/serde-1234567890abcdef/7654321",
        "/home/u/.cargo/git/checkouts/serde-1234567890abcdef/012",
    ] {
        assert!(super::git::cargo_home(at, "serde-1234567890abcdef", commit).is_err(), "{at}");
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@localhost")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@localhost")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[cfg(unix)]
#[test]
fn a_git_tree_is_its_commit_with_modes() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "--quiet"]);
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"g\"\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), "").unwrap();
    std::fs::write(repo.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("src/lib.rs", repo.join("lib")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "one"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("src/lib.rs"), "later").unwrap();
    git(&repo, &["commit", "--quiet", "-am", "two"]);

    let url = url::Url::from_directory_path(&repo).unwrap();
    let id = blaze_source::SourceId::for_git(&url, blaze_source::GitReference::Rev(commit.clone()))
        .unwrap();
    let home = dir.path().join("home");
    let at = home.join("git/checkouts").join(id.git_ident()).join(&commit[..7]);
    let fetch = Fetch {
        what: "g v0.0.0".to_owned(),
        url: url.to_string(),
        kind: FetchKind::Git { commit: commit.clone() },
        at: at.to_str().unwrap().to_owned(),
    };
    let fake = Fake::new(Vec::new());
    let source = fetcher(&fake).fetch(&fetch).unwrap();
    assert_eq!(source.under, "");
    assert_eq!(
        source.files,
        vec![
            file(".cargo-ok", b"", Mode::Regular),
            file("Cargo.toml", b"[package]\nname = \"g\"\n", Mode::Regular),
            file("lib", b"src/lib.rs", Mode::Symlink),
            file("run.sh", b"#!/bin/sh\n", Mode::Executable),
            file("src/lib.rs", b"", Mode::Regular),
        ]
    );

    let mut short = fetch.clone();
    short.kind = FetchKind::Git { commit: commit[..12].to_owned() };
    assert!(fetcher(&fake).fetch(&short).is_err());
}
