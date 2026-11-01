//! blaze-resolve's `Registry` over an index served from memory: the summaries cargo makes of
//! its lines, the versions each kind of query is offered, offline, and the lock file's
//! versions read with no update.

use std::{path::PathBuf, pin::pin, sync::Arc, task::Poll};

use blaze_hash::HashMap;
use blaze_registry::{
    Context, IndexRegistry, SourceConfig, SourceDef, Sources,
    http::{Http, Response},
};
use blaze_resolve::{
    DepKind, Dependency, IndexSummary, MemRegistry, PackageId, QueryKind, Registry, SourceId,
    block_on,
};
use parking_lot::Mutex;

const BASE: &str = "https://index.example.com/";

#[derive(Default)]
struct Server {
    files: Mutex<HashMap<String, Vec<u8>>>,
    asked: Mutex<Vec<String>>,
}

impl Server {
    fn put(&self, path: &str, lines: &[String]) {
        self.files.lock().insert(format!("{BASE}{path}"), lines.join("\n").into_bytes());
    }

    fn take_asked(&self) -> Vec<String> {
        self.asked.lock().drain(..).collect()
    }
}

impl Http for Server {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        self.asked.lock().push(url.to_owned());
        let Some(body) = self.files.lock().get(url).cloned() else {
            return Ok(Response { status: 404, ..Response::default() });
        };
        let etag = format!("\"{}\"", body.len());
        if headers.contains(&("if-none-match", etag.as_str())) {
            return Ok(Response { status: 304, ..Response::default() });
        }
        Ok(Response { status: 200, headers: vec![("etag".to_owned(), etag)], body })
    }
}

fn line(name: &str, version: &str, extra: &str) -> String {
    format!(
        r#"{{"name":"{name}","vers":"{version}","deps":[],"cksum":"{}","features":{{}}{extra}}}"#,
        "0".repeat(64)
    )
}

/// crates.io replaced by the registry `m` at [`BASE`], in `$CARGO_HOME` `home`.
struct Fixture {
    server: Arc<Server>,
    home: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let home = std::env::temp_dir().join(format!("blaze-query-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let server = Arc::new(Server::default());
        server
            .files
            .lock()
            .insert(format!("{BASE}config.json"), br#"{"dl":"https://dl.example.com"}"#.to_vec());
        Fixture { server, home }
    }

    fn sources(&self, offline: bool) -> Sources {
        let mut context = Context::new(self.home.clone(), self.server.clone());
        context.net.offline = offline;
        context.net.retry.quiet = true;
        Sources::new(&replaced(), Arc::new(context)).unwrap()
    }
}

fn replaced() -> SourceConfig {
    SourceConfig {
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
    }
}

fn dep(name: &str, req: &str) -> Dependency {
    Dependency::parse(name, Some(req), SourceId::crates_io()).unwrap()
}

/// What `kind` offers for `dep`, as `status version` each.
fn query(registry: &impl Registry, dep: &Dependency, kind: QueryKind) -> Vec<String> {
    let mut found = Vec::new();
    block_on(registry.query(dep, kind, &mut |s| {
        let status = match &s {
            IndexSummary::Candidate(_) => "candidate",
            IndexSummary::Yanked(_) => "yanked",
            IndexSummary::Unsupported(..) => "unsupported",
            IndexSummary::Invalid(_) => "invalid",
            IndexSummary::Offline(_) => "offline",
        };
        found.push(format!("{status} {} {}", s.package_id().name(), s.package_id().version()));
    }))
    .unwrap();
    found
}

#[test]
fn summaries_and_statuses_as_cargo() {
    let fixture = Fixture::new("statuses");
    fixture.server.put(
        "se/rd/serde",
        &[
            r#"{"name":"serde","vers":"1.0.0","deps":[{"name":"derive","package":"serde_derive","req":"^1","features":["","x"],"optional":true,"default_features":false,"target":"cfg(unix)","kind":"build","registry":null},{"name":"other","req":"^2","registry":"sparse+https://other.example.com/","public":true}],"cksum":"aa","features":{"std":[]},"features2":{"derive":["dep:derive"]},"links":"serde","rust_version":"1.31","pubtime":"2024-01-02T03:04:05Z","v":2}"#.to_owned(),
            line("serde", "1.0.1", r#","yanked":true"#),
            line("serde", "1.1.0", r#","v":3"#),
            r#"{"name":"serde","vers":"1.2.0","deps":[{"name":"b","req":"not a req"}],"cksum":"00","rust_version":"1.60"}"#.to_owned(),
            line("serde", "1.3.0", r#","features":{"f":["nope/x"]}"#).replace(r#""features":{},"#, ""),
            line("serde", "2.0.0", ""),
        ],
    );
    let sources = fixture.sources(false);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());

    assert_eq!(
        query(&registry, &dep("serde", "1"), QueryKind::Exact),
        ["candidate serde 1.0.0", "yanked serde 1.0.1"]
    );
    assert_eq!(
        query(&registry, &dep("serde", "1"), QueryKind::RejectedVersions),
        [
            "candidate serde 1.0.0",
            "yanked serde 1.0.1",
            "unsupported serde 1.1.0",
            "invalid serde 1.2.0",
            "invalid serde 1.3.0"
        ]
    );

    let mut first = None;
    block_on(registry.query(&dep("serde", "=1.0.0"), QueryKind::Exact, &mut |s| {
        first = Some(s.into_summary());
    }))
    .unwrap();
    let summary = first.unwrap();
    assert_eq!(summary.package_id().source_id(), SourceId::crates_io());
    assert_eq!(summary.checksum(), Some("aa"));
    assert_eq!(summary.links().as_deref(), Some("serde"));
    assert_eq!(summary.rust_version().unwrap().to_string(), "1.31");
    assert_eq!(summary.pubtime(), Some("2024-01-02T03:04:05Z"));
    let features: Vec<String> = summary.features().keys().map(|it| it.to_string()).collect();
    assert_eq!(features, ["derive", "std"]);
    let [derive, other] = summary.dependencies() else { panic!() };
    assert_eq!(derive.package_name().as_str(), "serde_derive");
    assert_eq!(derive.explicit_name_in_toml().as_deref(), Some("derive"));
    assert_eq!(derive.kind(), DepKind::Build);
    assert!(derive.is_optional() && !derive.uses_default_features() && !derive.is_public());
    assert!(other.is_public());
    assert_eq!(derive.features().iter().map(|it| it.as_str()).collect::<Vec<_>>(), ["x"]);
    assert_eq!(derive.platform().unwrap().to_string(), "cfg(unix)");
    assert_eq!(derive.source_id(), SourceId::crates_io());
    assert_eq!(derive.registry_id(), None);
    assert_eq!(other.source_id().url().as_str(), "sparse+https://other.example.com/");
    assert_eq!(other.registry_id(), Some(other.source_id()));

    assert!(registry.is_replaced(SourceId::crates_io()));
    assert_eq!(
        registry.describe_source(SourceId::crates_io()),
        "`m` index (which is replacing registry `crates-io`)"
    );
}

#[test]
fn crates_io_over_its_sparse_index_is_not_a_replacement() {
    let fixture = Fixture::new("builtin");
    let context = Arc::new(Context::new(fixture.home.clone(), fixture.server.clone()));
    let sources = Sources::new(&SourceConfig::default(), context).unwrap();
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    assert!(!registry.is_replaced(SourceId::crates_io()));
    assert_eq!(registry.describe_source(SourceId::crates_io()), "crates.io index");
}

#[test]
fn alternative_names() {
    let fixture = Fixture::new("names");
    fixture.server.put(
        "fo/o_/foo_bar",
        &[line("foo_bar", "1.0.0", ""), line("foo_bar", "1.0.1", r#","yanked":true"#)],
    );
    let sources = fixture.sources(false);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    let wanted = dep("foo-bar", "1");
    assert!(query(&registry, &wanted, QueryKind::Exact).is_empty());
    assert_eq!(
        query(&registry, &wanted, QueryKind::AlternativeNames),
        ["candidate foo_bar 1.0.0", "yanked foo_bar 1.0.1"]
    );
    assert_eq!(query(&registry, &wanted, QueryKind::Normalized), ["candidate foo_bar 1.0.0"]);
}

#[test]
fn offline_offers_what_is_downloaded() {
    let fixture = Fixture::new("offline");
    fixture.server.put("3/l/log", &[line("log", "0.4.0", ""), line("log", "0.4.1", "")]);
    // Online once, so that the index is in cargo's cache.
    let sources = fixture.sources(false);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    assert_eq!(query(&registry, &dep("log", "0.4"), QueryKind::Exact).len(), 2);
    fixture.server.take_asked();

    let sources = fixture.sources(true);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    // Nothing downloaded: every version.
    assert_eq!(
        query(&registry, &dep("log", "0.4"), QueryKind::Exact),
        ["candidate log 0.4.0", "candidate log 0.4.1"]
    );
    let index =
        sources.registry(sources.map().replacement(SourceId::crates_io()).unwrap()).unwrap();
    let file = index.crate_file("log", &semver::Version::new(0, 4, 0));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"a .crate").unwrap();
    let sources = fixture.sources(true);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    assert_eq!(query(&registry, &dep("log", "0.4"), QueryKind::Exact), ["candidate log 0.4.0"]);
    assert!(fixture.server.take_asked().is_empty());
}

#[test]
fn locked_versions_are_read_with_no_update() {
    let fixture = Fixture::new("locked");
    fixture.server.put("3/l/log", &[line("log", "0.4.0", "")]);
    let sources = fixture.sources(false);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    assert_eq!(query(&registry, &dep("log", "0.4"), QueryKind::Exact).len(), 1);
    assert_eq!(
        fixture.server.take_asked(),
        [format!("{BASE}config.json"), format!("{BASE}3/l/log")]
    );

    // A second run: the lock file's version is in the cache, so nothing is asked.
    let locked_source = SourceId::crates_io().with_locked_precise();
    let mut locked = Dependency::parse("log", Some("0.4"), locked_source).unwrap();
    locked.lock_to(PackageId::try_new("log", "0.4.0", locked_source).unwrap());
    let sources = fixture.sources(false);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    assert_eq!(query(&registry, &locked, QueryKind::Exact), ["candidate log 0.4.0"]);
    assert!(fixture.server.take_asked().is_empty());

    // Locked to a version the cache does not have yet: updated once (`config.json` with it),
    // and found.
    fixture.server.put("3/l/log", &[line("log", "0.4.0", ""), line("log", "0.4.1", "")]);
    let mut newer = Dependency::parse("log", Some("0.4"), locked_source).unwrap();
    newer.lock_to(PackageId::try_new("log", "0.4.1", locked_source).unwrap());
    assert_eq!(query(&registry, &newer, QueryKind::Exact), ["candidate log 0.4.1"]);
    assert_eq!(
        fixture.server.take_asked(),
        [format!("{BASE}config.json"), format!("{BASE}3/l/log")]
    );

    // Not locked: the index is updated before it is read.
    let sources = fixture.sources(false);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    assert_eq!(query(&registry, &dep("log", "0.4"), QueryKind::Exact).len(), 2);
    assert_eq!(
        fixture.server.take_asked(),
        [format!("{BASE}config.json"), format!("{BASE}3/l/log")]
    );
}

#[test]
fn queries_wait_for_one_read_of_a_file() {
    let fixture = Fixture::new("shared");
    fixture.server.put("3/l/log", &[line("log", "0.4.0", "")]);
    let sources = fixture.sources(false);
    let registry = IndexRegistry::new(&sources, MemRegistry::default());
    let wanted = dep("log", "0.4");
    let (mut a, mut b) = (0, 0);
    block_on(async {
        let (mut count_a, mut count_b) = (|_| a += 1, |_| b += 1);
        let first = registry.query(&wanted, QueryKind::Exact, &mut count_a);
        let second = registry.query(&wanted, QueryKind::Exact, &mut count_b);
        let (mut first, mut second) = (pin!(first), pin!(second));
        let (mut one, mut two) = (None, None);
        std::future::poll_fn(|cx| {
            if one.is_none()
                && let Poll::Ready(it) = first.as_mut().poll(cx)
            {
                one = Some(it);
            }
            if two.is_none()
                && let Poll::Ready(it) = second.as_mut().poll(cx)
            {
                two = Some(it);
            }
            if one.is_some() && two.is_some() { Poll::Ready(()) } else { Poll::Pending }
        })
        .await;
        one.unwrap().unwrap();
        two.unwrap().unwrap();
    });
    assert_eq!((a, b), (1, 1));
    assert_eq!(
        fixture.server.take_asked(),
        [format!("{BASE}config.json"), format!("{BASE}3/l/log")]
    );
}
