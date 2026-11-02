use std::path::Path;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_query::{Cancelled, Config, Cx, Database, MemTree, Query, QueryKind};
use blaze_resolve::{Dependency, MemRegistry, PackageId, QueryKind as Matching, Registry, SourceId};
use blaze_vfs::{View, ViewId};
use serde::{Deserialize, Serialize};

use super::{Index, Located, Sources};
use crate::index::tests::{SERDE_LINE, SERDE_SHA256};
use crate::load::files::{LoadDir, LoadFile, Mounts};
use crate::load::invocation::Invocation as InvocationQuery;
use crate::load::key_id;
use crate::query::{
    BuildConfig, Cbor, Command, Fetch, FetchKind, Invocation, LoadError, Mount, Origin, Versioned, producer,
};

const HOME: &str = "/home/u/.cargo";
const CRATES_IO_SRC: &str = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f";
const CRATES_IO_INDEX: &str = "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f";
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
/// A package to locate, with the configuration the sources are made of.
#[derive(Serialize, Deserialize, Clone)]
struct Asked {
    /// `--config` arguments.
    config: Vec<String>,
    offline: bool,
    source: String,
    name: String,
    version: String,
    checksum: Option<String>,
}

fn asked(source: &str, name: &str, version: &str, checksum: Option<&str>) -> Asked {
    Asked {
        config: Vec::new(),
        offline: false,
        source: source.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
        checksum: checksum.map(str::to_owned),
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
enum Answer {
    Here(String),
    Needs(Fetch),
    Failed(LoadError),
}

impl Versioned for Answer {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo test located 1");
}

fn sources(asked: &Asked) -> Sources {
    let env = [("HOME".to_owned(), "/home/u".to_owned())];
    let config = blaze_manifest::Config::load(Path::new("/home/u/ws"), env, &asked.config)
        .expect("a configuration");
    Sources::new(&config, asked.offline.then_some("--offline")).expect("the sources")
}

struct Locate;

impl Query for Locate {
    const KIND: QueryKind = QueryKind(0x63f0);
    const NAME: &'static str = "cargo test locate";
    const PRODUCER: ContentId = producer("blaze v2 cargo test locate 1");
    type Key = Asked;
    type Value = Cbor<Answer>;

    fn input(key: &Asked) -> ContentId {
        key_id("blaze v2 cargo test locate key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &Asked) -> Result<Cbor<Answer>, Cancelled> {
        let sources = sources(key);
        let source = SourceId::from_url(&key.source).expect("a source");
        let id = PackageId::try_new(key.name.as_str(), &key.version, source).expect("an id");
        Ok(Cbor(match sources.locate(cx, id, key.checksum.as_deref())? {
            Located::Here { manifest } => Answer::Here(manifest),
            Located::Needs(fetch) => Answer::Needs(fetch),
            Located::Failed(error) => Answer::Failed(error),
        }))
    }
}

/// The versions of crates.io's `name` an exact query of `req` finds, and the index files it
/// needed.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct Versions {
    found: Vec<String>,
    needs: Vec<Fetch>,
}

impl Versioned for Versions {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo test versions 1");
}

struct IndexQuery;

impl Query for IndexQuery {
    const KIND: QueryKind = QueryKind(0x63f1);
    const NAME: &'static str = "cargo test index query";
    const PRODUCER: ContentId = producer("blaze v2 cargo test index query 1");
    /// `(offline, name, req)`.
    type Key = (bool, String, String);
    type Value = Cbor<Versions>;

    fn input(key: &(bool, String, String)) -> ContentId {
        key_id("blaze v2 cargo test index query key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, (offline, name, req): &(bool, String, String)) -> Result<Cbor<Versions>, Cancelled> {
        let mut key = asked(CRATES_IO, name, "0.0.0", None);
        key.offline = *offline;
        let sources = sources(&key);
        let dep = Dependency::parse(name.as_str(), Some(req.as_str()), SourceId::crates_io()).expect("a dependency");
        let index = Index::new(&sources, cx, MemRegistry::new());
        let found = {
            let mut query = std::pin::pin!(index.query_vec(&dep, Matching::Exact));
            let mut context = Context::from_waker(Waker::noop());
            let Poll::Ready(found) = query.as_mut().poll(&mut context) else {
                panic!("an index query waits for nothing")
            };
            found.expect("the query")
        };
        let needs = index.finish()?;
        let mut found: Vec<String> = found
            .iter()
            .map(|it| format!("{}{}", it.package_id().version(), if it.is_yanked() { " yanked" } else { "" }))
            .collect();
        found.sort();
        Ok(Cbor(Versions { found, needs }))
    }
}

fn database(tree: &Arc<MemTree>) -> Database {
    let mut db = Database::new(Config { tree: Some(tree.clone()), ..Config::memory() });
    db.register::<InvocationQuery>();
    db.register::<Mounts>();
    db.register::<LoadFile>();
    db.register::<LoadDir>();
    db.register::<Locate>();
    db.register::<IndexQuery>();
    db
}

fn mount(at: &str, under: &str) -> Mount {
    Mount { at: at.to_owned(), origin: Origin::View, under: under.to_owned() }
}

/// A view of `files` with `mounts`, opened with a build of `/home/u/ws` by a user whose home
/// is `/home/u`.
fn open(db: &Database, tree: &MemTree, files: &[(&str, &str)], mounts: Vec<Mount>) -> ViewId {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(path, text)| (*path, text.as_bytes())).collect();
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: "/home/u/ws".to_owned(),
            cwd: "/home/u/ws".to_owned(),
            env: vec![("HOME".to_owned(), "/home/u".to_owned())],
            files: Vec::new(),
        },
        mounts,
        host: "x86_64-unknown-linux-gnu".to_owned(),
        library: "/rustc/0/library".to_owned(),
        std: "/s/std".to_owned(),
    };
    db.open_with(View { root: tree.root(&files) }, &config.encode())
}

fn locate(files: &[(&str, &str)], mounts: Vec<Mount>, asked: &Asked) -> Answer {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, files, mounts);
    db.pin().get::<Locate>(view, asked).expect("located").get()
}

fn needs(answer: Answer) -> Fetch {
    match answer {
        Answer::Needs(fetch) => fetch,
        other => panic!("not a fetch: {other:?}"),
    }
}

fn failed(answer: Answer) -> String {
    match answer {
        Answer::Failed(error) => error.what,
        other => panic!("not an error: {other:?}"),
    }
}

#[test]
fn a_crates_io_package_is_fetched_from_static_crates_io_into_cargos_directory() {
    let serde = asked(CRATES_IO, "serde", "1.0.219", Some(SERDE_SHA256));
    let at = format!("{CRATES_IO_SRC}/serde-1.0.219");
    assert_eq!(at, "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.219");
    assert_eq!(
        locate(&[], vec![], &serde),
        Answer::Needs(Fetch {
            what: "serde v1.0.219".to_owned(),
            url: "https://static.crates.io/crates/serde/1.0.219/download".to_owned(),
            kind: FetchKind::Crate { sha256: SERDE_SHA256.to_owned() },
            at: at.clone(),
        })
    );
    let files = [("serde/Cargo.toml", "[package]\nname = \"serde\"\nversion = \"1.0.219\"\n")];
    assert_eq!(
        locate(&files, vec![mount(&at, "serde")], &serde),
        Answer::Here(format!("{at}/Cargo.toml"))
    );
}

#[test]
fn offline_a_package_not_fetched_is_cargos_error() {
    let mut serde = asked(CRATES_IO, "serde", "1.0.219", Some(SERDE_SHA256));
    serde.offline = true;
    assert_eq!(
        failed(locate(&[], vec![], &serde)),
        "failed to download `serde v1.0.219`\n\nCaused by:\n  attempting to make an HTTP request, \
         but --offline was specified"
    );
    let git = format!("git+https://github.com/o/r?branch=main#{}", "0123456789abcdef0123456789abcdef01234567");
    let mut r = asked(&git, "r", "0.1.0", None);
    r.offline = true;
    assert!(failed(locate(&[], vec![], &r)).starts_with("failed to download `r v0.1.0`\n\nCaused by:\n"));
}

#[test]
fn a_lock_file_without_checksums_takes_the_index_s() {
    let serde = asked(CRATES_IO, "serde", "1.0.219", None);
    let index_file = format!("{CRATES_IO_INDEX}/.cache/se/rd/serde");
    assert_eq!(
        needs(locate(&[], vec![], &serde)),
        Fetch {
            what: "crates.io index".to_owned(),
            url: "https://index.crates.io/se/rd/serde".to_owned(),
            kind: FetchKind::IndexFile,
            at: index_file.clone(),
        }
    );
    let fetch = needs(locate(&[("serde-index", SERDE_LINE)], vec![mount(&index_file, "serde-index")], &serde));
    assert_eq!(fetch.kind, FetchKind::Crate { sha256: SERDE_SHA256.to_owned() });
    let other = asked(CRATES_IO, "serde", "1.0.0", None);
    assert_eq!(
        failed(locate(&[("serde-index", SERDE_LINE)], vec![mount(&index_file, "serde-index")], &other)),
        "failed to download `serde v1.0.0`\n\nCaused by:\n  no hash listed for serde v1.0.0"
    );
}

#[test]
fn an_alternative_sparse_registry_needs_its_config_json_then_its_dl_template() {
    let url = "sparse+https://alt.example.com/index/";
    let dir = SourceId::from_url(url).unwrap().registry_dir_name();
    assert!(dir.starts_with("alt.example.com-"), "{dir}");
    let demo = asked(url, "Demo", "0.1.0", Some("ab12"));
    let config_json = format!("{HOME}/registry/index/{dir}/config.json");
    let fetch = needs(locate(&[], vec![], &demo));
    assert_eq!(
        (fetch.url.as_str(), &fetch.kind, fetch.at.as_str()),
        ("https://alt.example.com/index/config.json", &FetchKind::IndexFile, config_json.as_str())
    );
    let template = r#"{"dl":"https://dl.example.com/{crate}/{version}/{prefix}/{lowerprefix}/{sha256-checksum}","api":null}"#;
    let fetch = needs(locate(&[("alt.json", template)], vec![mount(&config_json, "alt.json")], &demo));
    assert_eq!(
        fetch,
        Fetch {
            what: "Demo v0.1.0".to_owned(),
            url: "https://dl.example.com/Demo/0.1.0/De/mo/de/mo/ab12".to_owned(),
            kind: FetchKind::Crate { sha256: "ab12".to_owned() },
            at: format!("{HOME}/registry/src/{dir}/Demo-0.1.0"),
        }
    );
    let plain = r#"{"dl":"https://dl.example.com/api/v1/crates"}"#;
    let fetch = needs(locate(&[("alt.json", plain)], vec![mount(&config_json, "alt.json")], &demo));
    assert_eq!(fetch.url, "https://dl.example.com/api/v1/crates/Demo/0.1.0/download");
    let private = r#"{"dl":"https://dl.example.com","auth-required":true}"#;
    let error = failed(locate(&[("alt.json", private)], vec![mount(&config_json, "alt.json")], &demo));
    assert!(error.contains("requires authentication"), "{error}");
}

#[test]
fn a_mirror_of_crates_io_changes_the_directory_and_the_url() {
    let mut serde = asked(CRATES_IO, "serde", "1.0.219", Some(SERDE_SHA256));
    serde.config = vec![
        "source.crates-io.replace-with=\"mirror\"".to_owned(),
        "source.mirror.registry=\"sparse+https://mirror.example.com/index/\"".to_owned(),
    ];
    let dir = SourceId::from_url("sparse+https://mirror.example.com/index/").unwrap().registry_dir_name();
    let config_json = format!("{HOME}/registry/index/{dir}/config.json");
    let fetch = needs(locate(&[], vec![], &serde));
    assert_eq!(fetch.url, "https://mirror.example.com/index/config.json");
    assert_eq!(fetch.at, config_json);
    let dl = r#"{"dl":"https://mirror.example.com/dl"}"#;
    let fetch = needs(locate(&[("mirror.json", dl)], vec![mount(&config_json, "mirror.json")], &serde));
    assert_eq!(fetch.url, "https://mirror.example.com/dl/serde/1.0.219/download");
    assert_eq!(fetch.at, format!("{HOME}/registry/src/{dir}/serde-1.0.219"));
}

#[test]
fn a_git_index_is_not_read() {
    let mut serde = asked(CRATES_IO, "serde", "1.0.219", Some(SERDE_SHA256));
    serde.config = vec!["registries.crates-io.protocol=\"git\"".to_owned()];
    let error = failed(locate(&[], vec![], &serde));
    assert!(error.contains("blaze reads sparse registry indexes only"), "{error}");
}

#[test]
fn a_git_package_is_found_in_its_checkout() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let url = format!("git+https://github.com/serde-rs/serde?branch=master#{commit}");
    let ident = SourceId::from_url(&url).unwrap().git_ident();
    assert!(ident.starts_with("serde-"), "{ident}");
    let at = format!("{HOME}/git/checkouts/{ident}/0123456");
    let serde = asked(&url, "serde", "1.0.219", None);
    assert_eq!(
        locate(&[], vec![], &serde),
        Answer::Needs(Fetch {
            what: "git repository `https://github.com/serde-rs/serde`".to_owned(),
            url: "https://github.com/serde-rs/serde".to_owned(),
            kind: FetchKind::Git { commit: commit.to_owned() },
            at: at.clone(),
        })
    );
    let package = |name: &str, version: &str| format!("[package]\nname = \"{name}\"\nversion = {version}\n");
    let workspace = "[workspace]\nmembers = [\"serde\", \"serde_derive\"]\n\n[workspace.package]\nversion = \"1.0.219\"\n";
    let serde_manifest = package("serde", "{ workspace = true }");
    let derive = package("serde_derive", "\"1.0.219\"");
    let decoy = package("serde", "\"1.0.219\"");
    let files = [
        ("co/Cargo.toml", workspace),
        ("co/.hidden/Cargo.toml", decoy.as_str()),
        ("co/serde/Cargo.toml", serde_manifest.as_str()),
        ("co/serde/src/lib.rs", ""),
        ("co/serde_derive/Cargo.toml", derive.as_str()),
        ("co/target/Cargo.toml", decoy.as_str()),
    ];
    assert_eq!(locate(&files, vec![mount(&at, "co")], &serde), Answer::Here(format!("{at}/serde/Cargo.toml")));
    let derive = asked(&url, "serde_derive", "1.0.219", None);
    assert_eq!(locate(&files, vec![mount(&at, "co")], &derive), Answer::Here(format!("{at}/serde_derive/Cargo.toml")));
    let missing = asked(&url, "serde_json", "1.0.0", None);
    let error = failed(locate(&files, vec![mount(&at, "co")], &missing));
    assert!(error.starts_with("failed to find serde_json v1.0.0"), "{error}");
}

fn versions(files: &[(&str, &str)], mounts: Vec<Mount>, offline: bool) -> Versions {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, files, mounts);
    db.pin().get::<IndexQuery>(view, &(offline, "serde".to_owned(), "^1.0".to_owned())).expect("queried").get()
}

#[test]
fn the_resolver_reads_the_index_files_mounted_and_asks_for_the_rest() {
    let index_file = format!("{CRATES_IO_INDEX}/.cache/se/rd/serde");
    let none = versions(&[], vec![], false);
    assert!(none.found.is_empty());
    assert_eq!(
        none.needs,
        [Fetch {
            what: "crates.io index".to_owned(),
            url: "https://index.crates.io/se/rd/serde".to_owned(),
            kind: FetchKind::IndexFile,
            at: index_file.clone(),
        }]
    );
    let offline = versions(&[], vec![], true);
    assert_eq!(offline, Versions { found: Vec::new(), needs: Vec::new() });
    let yanked = SERDE_LINE.replace("1.0.219", "1.0.220").replace("\"yanked\":false", "\"yanked\":true");
    let too_new = SERDE_LINE.replace("1.0.219", "1.0.221").replace("\"yanked\":false", "\"v\":3");
    let file = format!("{SERDE_LINE}\n{yanked}\n{too_new}\nnot json\n");
    let read = versions(&[("serde-index", &file)], vec![mount(&index_file, "serde-index")], false);
    assert_eq!(read, Versions { found: vec!["1.0.219".to_owned(), "1.0.220 yanked".to_owned()], needs: Vec::new() });
}
