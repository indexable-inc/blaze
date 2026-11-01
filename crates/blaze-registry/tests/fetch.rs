//! A registry served from memory: the index read and cached as cargo reads and caches it, and
//! packages fetched, checked and unpacked where cargo unpacks them.

use std::{
    cell::Cell,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_hash::HashMap;
use blaze_registry::{
    Context, Locked, Package, Registry, SourceConfig, SourceDef, Sources,
    download::{From, checksum, crate_key, crate_url, parse_checksum, unpack_package_in},
    http::{Http, Response},
    index::Status,
};
use blaze_source::SourceId;
use parking_lot::Mutex;
use semver::{Version, VersionReq};

const BASE: &str = "https://index.example.com/";

/// Files by URL, each with an ETag, and every request asked of it.
#[derive(Default)]
struct Server {
    files: Mutex<HashMap<String, (Vec<u8>, String)>>,
    asked: Mutex<Vec<(String, Vec<(String, String)>)>>,
}

impl Server {
    fn put(&self, url: &str, body: &[u8], etag: &str) {
        self.files.lock().insert(url.to_owned(), (body.to_vec(), etag.to_owned()));
    }

    fn take_asked(&self) -> Vec<String> {
        self.asked.lock().drain(..).map(|(url, _)| url).collect()
    }
}

impl Http for Server {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        let headers: Vec<(String, String)> =
            headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.asked.lock().push((url.to_owned(), headers.clone()));
        let files = self.files.lock();
        let Some((body, etag)) = files.get(url) else {
            return Ok(Response { status: 404, ..Response::default() });
        };
        let condition = headers.iter().find(|(k, _)| k == "if-none-match").map(|(_, v)| v);
        if condition == Some(etag) {
            return Ok(Response { status: 304, ..Response::default() });
        }
        Ok(Response {
            status: 200,
            headers: vec![("etag".to_owned(), etag.clone())],
            body: body.clone(),
        })
    }
}

/// A store of its own in `dir`, kept for the rest of the test process as the daemon's store
/// is; what is put is read at once, before any commit.
fn store(dir: &Path) -> &'static blaze_cas::Handle {
    let driven = Box::leak(Box::new(blaze_cas::Driven::open(dir).unwrap()));
    Box::leak(Box::new(driven.handle()))
}

fn temporary(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-registry-{name}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `.crate` of `entries`, paths under `<name>-<version>/` as `cargo package` writes them.
fn tarball(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for &(path, data, mode) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_mtime(1_700_000_000);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, path, data).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn line(name: &str, version: &str, cksum: &str, extra: &str) -> String {
    format!(
        r#"{{"name":"{name}","vers":"{version}","deps":[],"cksum":"{cksum}","features":{{}}{extra}}}"#
    )
}

struct Fixture {
    server: Arc<Server>,
    store: &'static blaze_cas::Handle,
    home: PathBuf,
    source: SourceId,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let server = Arc::new(Server::default());
        server.put(
            &format!("{BASE}config.json"),
            br#"{"dl":"https://dl.example.com/{crate}/{version}/{lowerprefix}/{sha256-checksum}"}"#,
            "c1",
        );
        let source = blaze_registry::registry_source(&format!("sparse+{BASE}")).unwrap();
        let home = temporary(name);
        Fixture { server, store: store(&home.join("store")), home, source }
    }

    fn registry(&self, offline: bool) -> Registry {
        self.registry_with(offline, self.store)
    }

    fn registry_with(&self, offline: bool, store: &'static blaze_cas::Handle) -> Registry {
        let mut context = Context::new(self.home.clone(), self.server.clone());
        context.store = Some(store);
        context.net.offline = offline;
        context.net.retry.quiet = true;
        Registry::remote(self.source, Arc::new(context)).unwrap()
    }

    fn publish(&self, name: &str, files: &[(&str, &[u8], u32)]) -> Package {
        let bytes = tarball(files);
        let sum = checksum(&bytes);
        let lower = blaze_registry::index::dep_path(&name.to_lowercase(), true);
        self.server.put(&format!("https://dl.example.com/{name}/1.0.0/{lower}/{sum}"), &bytes, "x");
        self.server.put(
            &format!("{BASE}{}", blaze_registry::index::dep_path(&name.to_lowercase(), false)),
            line(name, "1.0.0", &sum, "").as_bytes(),
            "e1",
        );
        Package { name: name.to_owned(), version: Version::new(1, 0, 0), checksum: sum }
    }
}

#[test]
fn reads_and_caches_the_index_as_cargo() {
    let fixture = Fixture::new("index");
    let file = [
        line("serde", "1.0.0", "00", ""),
        line("serde", "1.0.1", "01", r#","yanked":true"#),
        line("serde", "1.1.0", "02", r#","v":3"#),
    ]
    .join("\n");
    fixture.server.put(&format!("{BASE}se/rd/serde"), file.as_bytes(), "\"v1\"");
    let any = VersionReq::STAR;

    let registry = fixture.registry(false);
    let found = registry.query("serde", &any).unwrap();
    let statuses: Vec<Status> = found.iter().map(|it| it.status).collect();
    assert_eq!(statuses, [Status::Candidate, Status::Yanked, Status::Unsupported(3)]);
    assert_eq!(
        fixture.server.take_asked(),
        [format!("{BASE}config.json"), format!("{BASE}se/rd/serde")]
    );
    let dir = fixture.home.join("registry/index").join(registry.dir_name());
    assert!(registry.dir_name().starts_with("index.example.com-"));
    assert!(dir.join("config.json").is_file());
    let cache = std::fs::read(dir.join(".cache/se/rd/serde")).unwrap();
    assert_eq!(&cache[..5], &[3, 2, 0, 0, 0]);
    assert!(cache[5..].starts_with(b"etag: \"v1\"\0"));

    // As a second cargo would: the cache as it is, and nothing asked, until an update.
    let registry = fixture.registry(false);
    assert_eq!(registry.query("serde", &any).unwrap().len(), 3);
    assert!(fixture.server.take_asked().is_empty());

    registry.invalidate();
    assert_eq!(registry.query("serde", &any).unwrap().len(), 3);
    let asked = fixture.server.asked.lock().clone();
    let (url, headers) = asked.last().unwrap();
    assert_eq!(url, &format!("{BASE}se/rd/serde"));
    assert!(headers.contains(&("if-none-match".to_owned(), "\"v1\"".to_owned())));
    assert!(headers.contains(&("cargo-protocol".to_owned(), "version=1".to_owned())));
    fixture.server.take_asked();

    // A change on the server is found at the next update, and the cache follows it.
    let file = format!("{file}\n{}", line("serde", "1.2.0", "03", ""));
    fixture.server.put(&format!("{BASE}se/rd/serde"), file.as_bytes(), "\"v2\"");
    let registry = fixture.registry(false);
    registry.invalidate();
    assert_eq!(registry.query("serde", &any).unwrap().len(), 4);
    let cache = std::fs::read(dir.join(".cache/se/rd/serde")).unwrap();
    assert!(cache[5..].starts_with(b"etag: \"v2\"\0"));

    // Gone from the server: no versions, and no cache.
    fixture.server.files.lock().remove(&format!("{BASE}se/rd/serde"));
    let registry = fixture.registry(false);
    registry.invalidate();
    assert!(registry.query("serde", &any).unwrap().is_empty());
    assert!(!dir.join(".cache/se/rd/serde").exists());

    // Offline, nothing is asked, and what is not cached is not there.
    fixture.server.take_asked();
    let registry = fixture.registry(true);
    registry.invalidate();
    assert!(registry.query("tokio", &any).unwrap().is_empty());
    assert!(fixture.server.take_asked().is_empty());
}

#[test]
fn fetches_into_cargos_directories_and_the_store() {
    let fixture = Fixture::new("fetch");
    let package = fixture.publish(
        "Demo",
        &[
            ("Demo-1.0.0/Cargo.toml", b"[package]\nname = \"Demo\"\n", 0o644),
            ("Demo-1.0.0/src/lib.rs", b"pub fn f() {}\n", 0o644),
            ("Demo-1.0.0/run.sh", b"#!/bin/sh\n", 0o755),
            ("Demo-1.0.0/.cargo-ok", b"not ours", 0o644),
        ],
    );
    let registry = fixture.registry(false);
    assert_eq!(registry.checksum("Demo", &package.version).unwrap(), package.checksum);
    let fetched = registry.fetch(std::slice::from_ref(&package)).unwrap();
    let (dir, from) = &fetched[0];
    assert_eq!(*from, From::Network);
    let src = fixture.home.join("registry/src").join(registry.dir_name()).join("Demo-1.0.0");
    assert_eq!(dir, &src);
    assert_eq!(std::fs::read_to_string(src.join(".cargo-ok")).unwrap(), r#"{"v":1}"#);
    assert_eq!(std::fs::read(src.join("src/lib.rs")).unwrap(), b"pub fn f() {}\n");
    let toml = std::fs::metadata(src.join("Cargo.toml")).unwrap();
    assert_eq!(filetime::FileTime::from_last_modification_time(&toml).unix_seconds(), 1153704088);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&src.join("run.sh")) & 0o100, 0o100);
        assert_eq!(mode(&src.join("src/lib.rs")) & 0o111, 0);
    }
    let crate_file =
        fixture.home.join("registry/cache").join(registry.dir_name()).join("Demo-1.0.0.crate");
    assert_eq!(checksum(&std::fs::read(&crate_file).unwrap()), package.checksum);
    let key = crate_key(&parse_checksum(&package.checksum).unwrap());
    assert!(fixture.store.has(&key));

    // Unpacked already: nothing read.
    assert_eq!(registry.fetch_one(&package).unwrap().1, From::Unpacked);
    // From the store when cargo's copies are gone, with nothing asked.
    std::fs::remove_dir_all(&src).unwrap();
    std::fs::remove_file(&crate_file).unwrap();
    fixture.server.take_asked();
    assert_eq!(registry.fetch_one(&package).unwrap().1, From::Store);
    assert!(fixture.server.take_asked().is_empty());
    assert!(crate_file.is_file());
    // From cargo's cache when the store does not have it, which then keeps it.
    std::fs::remove_dir_all(&src).unwrap();
    let empty = store(&fixture.home.join("empty-store"));
    let registry = fixture.registry_with(false, empty);
    assert_eq!(registry.fetch_one(&package).unwrap().1, From::CargoCache);
    assert!(empty.has(&key));
    // An unpacking cut short (no `.cargo-ok`) is done again.
    std::fs::remove_file(src.join(".cargo-ok")).unwrap();
    std::fs::write(src.join("stray"), b"").unwrap();
    std::fs::remove_dir_all(&src).unwrap();
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join(".cargo-ok"), b"").unwrap();
    registry.fetch_one(&package).unwrap();
    assert!(src.join("src/lib.rs").is_file());
}

#[test]
fn refuses_what_cargo_refuses() {
    let fixture = Fixture::new("refuse");
    let registry = fixture.registry(false);

    let mut wrong = fixture.publish("wrong", &[("wrong-1.0.0/a", b"a", 0o644)]);
    wrong.checksum = checksum(b"something else");
    let lower = blaze_registry::index::dep_path("wrong", true);
    let bytes = tarball(&[("wrong-1.0.0/a", b"a", 0o644)]);
    fixture.server.put(
        &format!("https://dl.example.com/wrong/1.0.0/{lower}/{}", wrong.checksum),
        &bytes,
        "x",
    );
    let e = format!("{:#}", registry.fetch(&[wrong]).unwrap_err());
    assert!(e.contains("failed to verify the checksum of `wrong v1.0.0`"), "{e}");

    let outside = fixture.publish("outside", &[("other-1.0.0/a", b"a", 0o644)]);
    let e = format!("{:#}", registry.fetch(&[outside]).unwrap_err());
    assert!(e.contains("isn't under \"outside-1.0.0\""), "{e}");

    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    builder.append_link(&mut header, "link-1.0.0/escape", "/etc/passwd").unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&builder.into_inner().unwrap()).unwrap();
    let bytes = gz.finish().unwrap();
    let sum = checksum(&bytes);
    let lower = blaze_registry::index::dep_path("link", true);
    fixture.server.put(&format!("https://dl.example.com/link/1.0.0/{lower}/{sum}"), &bytes, "x");
    let link = Package { name: "link".to_owned(), version: Version::new(1, 0, 0), checksum: sum };
    let e = format!("{:#}", registry.fetch(&[link]).unwrap_err());
    assert!(e.contains("with invalid type Symlink"), "{e}");

    let offline = fixture.registry(true);
    let missing = fixture.publish("missing", &[("missing-1.0.0/a", b"a", 0o644)]);
    let e = format!("{:#}", offline.fetch(&[missing]).unwrap_err());
    assert!(e.contains("--offline"), "{e}");
}

#[test]
fn fetches_many_at_once() {
    let fixture = Fixture::new("many");
    let packages: Vec<Package> = (0..40)
        .map(|at| {
            let name = format!("p{at}");
            let path = format!("{name}-1.0.0/lib.rs");
            fixture.publish(&name, &[(&path, name.as_bytes(), 0o644)])
        })
        .collect();
    let registry = fixture.registry(false);
    let fetched = registry.fetch(&packages).unwrap();
    for (package, (dir, _)) in packages.iter().zip(&fetched) {
        assert_eq!(std::fs::read(dir.join("lib.rs")).unwrap(), package.name.as_bytes());
    }
}

#[test]
fn download_urls() {
    let sum = "ab".repeat(32);
    assert_eq!(
        crate_url("https://static.crates.io/crates", "serde", "1.0.0", &sum),
        "https://static.crates.io/crates/serde/1.0.0/download"
    );
    assert_eq!(
        crate_url(
            "https://x/{prefix}/{lowerprefix}/{crate}/{crate}-{version}.crate?{sha256-checksum}",
            "AbCd",
            "0.1.0",
            &sum
        ),
        format!("https://x/Ab/Cd/ab/cd/AbCd/AbCd-0.1.0.crate?{sum}")
    );
    assert_eq!(crate_url("https://x/{crate}", "abc", "1.0.0", &sum), "https://x/abc");
}

#[test]
fn unpacks_in_only_what_is_included() {
    let dir = temporary("unpack-in");
    let bytes = tarball(&[
        ("only-1.0.0/Cargo.toml", b"[package]\n", 0o644),
        ("only-1.0.0/src/lib.rs", b"pub fn f() {}\n", 0o644),
        ("only-1.0.0/.gitignore", b"target\n", 0o644),
    ]);
    let dst = dir.join("only-1.0.0");
    let asked = Cell::new(0);
    let left_out = Path::new(".gitignore");
    let written = unpack_package_in(&bytes, &dst, |path: &Path| {
        asked.set(asked.get() + 1);
        path != left_out
    })
    .unwrap();
    assert_eq!(asked.get(), 3);
    assert_eq!(written, (b"[package]\n".len() + b"pub fn f() {}\n".len()) as u64);
    assert_eq!(std::fs::read(dst.join("src/lib.rs")).unwrap(), b"pub fn f() {}\n");
    assert!(dst.join("Cargo.toml").is_file());
    assert!(!dst.join(".gitignore").exists());
    assert!(!dst.join(".cargo-ok").exists());
}

/// A vendored package as `cargo vendor` writes it: its files and a `.cargo-checksum.json`.
fn vendor(dir: &Path, name: &str, version: &str, package: Option<&str>, files: &[(&str, &[u8])]) {
    let root = dir.join(name);
    let mut sums = Vec::new();
    for (path, data) in files {
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        std::fs::write(root.join(path), data).unwrap();
        sums.push(format!("{path:?}:{:?}", checksum(data)));
    }
    let manifest = format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n");
    std::fs::write(root.join("Cargo.toml"), &manifest).unwrap();
    sums.push(format!("\"Cargo.toml\":{:?}", checksum(manifest.as_bytes())));
    let package = package.map_or("null".to_owned(), |it| format!("{it:?}"));
    let json = format!(r#"{{"files":{{{}}},"package":{package}}}"#, sums.join(","));
    std::fs::write(root.join(".cargo-checksum.json"), json).unwrap();
}

fn locked(name: &str, version: &str, checksum: &str) -> Locked {
    Locked {
        source: SourceId::crates_io(),
        package: Package {
            name: name.to_owned(),
            version: Version::parse(version).unwrap(),
            checksum: checksum.to_owned(),
        },
    }
}

#[test]
fn reads_vendored_directories_as_cargo() {
    let fixture = Fixture::new("vendored");
    let dir = fixture.home.join("vendor");
    vendor(&dir, "alpha", "1.2.3", Some(&"a".repeat(64)), &[("src/lib.rs", b"pub fn a() {}\n")]);
    vendor(&dir, "beta", "0.1.0", None, &[("src/lib.rs", b"")]);
    std::fs::create_dir_all(dir.join(".git/x")).unwrap();
    std::fs::write(dir.join(".git/x/Cargo.toml"), "not toml [").unwrap();
    std::fs::create_dir_all(dir.join("leftover")).unwrap();
    std::fs::write(dir.join("README"), "").unwrap();

    let config = SourceConfig {
        sources: vec![
            (
                "crates-io".to_owned(),
                SourceDef { replace_with: Some("v".to_owned()), ..SourceDef::default() },
            ),
            ("v".to_owned(), SourceDef { directory: Some(dir.clone()), ..SourceDef::default() }),
        ],
        ..SourceConfig::default()
    };
    let context = Arc::new(Context::new(fixture.home.clone(), fixture.server.clone()));
    let sources = Sources::new(&config, context).unwrap();
    let directory = sources.directory(SourceId::for_directory(&dir).unwrap()).unwrap();
    let mut names: Vec<_> =
        directory.packages().unwrap().iter().map(|it| it.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["alpha", "beta"]);
    assert_eq!(directory.query("alpha", &VersionReq::parse("^1.2").unwrap()).unwrap().len(), 1);

    let roots = sources
        .fetch(&[locked("alpha", "1.2.3", &"a".repeat(64)), locked("beta", "0.1.0", "")])
        .unwrap();
    assert_eq!(roots, [(dir.join("alpha"), From::Vendored), (dir.join("beta"), From::Vendored)]);
    assert!(fixture.server.take_asked().is_empty());

    let error = |it: Locked| format!("{:#}", sources.fetch(&[it]).unwrap_err());
    assert!(
        error(locked("alpha", "1.2.3", &"b".repeat(64)))
            .starts_with("checksum for `alpha v1.2.3` changed between lock files")
    );
    assert!(error(locked("beta", "0.1.0", &"b".repeat(64))).starts_with(
        "checksum for `beta v0.1.0` could not be calculated, but a checksum is listed"
    ));
    assert!(
        error(locked("gamma", "1.0.0", ""))
            .starts_with("failed to find package with id: gamma v1.0.0")
    );
    std::fs::write(dir.join("alpha/src/lib.rs"), "pub fn edited() {}\n").unwrap();
    let edited = error(locked("alpha", "1.2.3", &"a".repeat(64)));
    assert!(edited.starts_with("the listed checksum of `"), "{edited}");
    assert!(edited.contains("directory sources are not intended to be edited"), "{edited}");

    std::fs::remove_file(dir.join("beta/.cargo-checksum.json")).unwrap();
    let missing =
        Sources::new(&config, Arc::new(Context::new(fixture.home.clone(), fixture.server.clone())))
            .unwrap()
            .fetch(&[locked("alpha", "1.2.3", "")])
            .unwrap_err();
    assert!(
        format!("{missing}")
            .starts_with("failed to load checksum `.cargo-checksum.json` of beta v0.1.0: ")
    );
}

#[test]
fn replaced_registries_unpack_under_the_replacement() {
    let fixture = Fixture::new("mirror");
    let package = fixture.publish("Mirrored", &[("Mirrored-1.0.0/src/lib.rs", b"", 0o644)]);
    let config = SourceConfig {
        sources: vec![
            (
                "crates-io".to_owned(),
                SourceDef { replace_with: Some("m".to_owned()), ..SourceDef::default() },
            ),
            (
                "m".to_owned(),
                SourceDef { registry: Some(format!("sparse+{BASE}")), ..SourceDef::default() },
            ),
        ],
        ..SourceConfig::default()
    };
    let context = Arc::new(Context::new(fixture.home.clone(), fixture.server.clone()));
    let sources = Sources::new(&config, context).unwrap();
    let roots = sources
        .fetch(&[Locked { source: SourceId::crates_io(), package: package.clone() }])
        .unwrap();
    let dir = fixture.source.registry_dir_name();
    assert!(dir.starts_with("index.example.com-"), "{dir}");
    assert_eq!(
        roots,
        [(fixture.home.join("registry/src").join(&dir).join("Mirrored-1.0.0"), From::Network)]
    );
    assert!(roots[0].0.join(".cargo-ok").exists());
    assert!(!fixture.home.join("registry/src/index.crates.io-1949cf8c6b5b557f").exists());
}
