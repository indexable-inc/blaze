//! Sparse-index reading (D123): the line parser and its record, the protocol's rules, and the
//! index machine against a simulated registry.
//!
//! The simulation draws a few crates (some the registry has not, one asked twice in another
//! case, one asked by every `-`/`_` spelling) and a fate for each request (a reset or a 503 on
//! some files' one try), runs the machine cold over an empty cache, then as the next command
//! over the cache the cold run wrote, then warm over the cache both wrote. Its oracles: every
//! read goes out before the first request ("a read after a request"); a cold run asks once per
//! file, its failure or not ("asked again": one try, D172), and a file dealt a failure ends
//! failed ("a failure stood"); the next run fetches every file the cold run failed ("owed");
//! a fetched file's record opens, is its file's, keeps the answer's
//! `ETag` and holds every line in version order ("wrong record"); a crate the registry has not
//! ends absent ("absent"); a warm run downloads nothing ("bytes fetched on a warm run"), parses
//! nothing and writes nothing ("rewritten on a warm run"); every run ends ("hang"). The planted
//! [`IndexPlant::NoValidator`] is caught by counting bytes fetched.

use std::collections::BTreeMap;

use blaze_own::String as OwnString;
use blaze_sim::{Broke, Config, Tape};
use blaze_text::Writes as _;

use crate::index::machine::INDEX_PLANT;
use crate::index::request::{
    CRATES_IO_INDEX, answer_kind, cache_path_in, condition, config_in, crate_url_in, index_path_in, validator_of,
};
use crate::index::{
    AnswerKind, Bad, ConfigRefused, DepKind, Field, FileEnd, Index, IndexAsk, IndexEffect, IndexLimits, IndexPlant,
    IndexReply, IndexView, IndexWhy, NameRefused, QueryKind, RegistryKey, Status, Update, Validator, parse_in,
};
use crate::{Limits, NetFailure};

fn alloc() -> blaze_own::Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn shown(value: &impl blaze_text::Show) -> std::string::String {
    let mut out = OwnString::new_in(alloc());
    out.show(value);
    out.as_str().to_owned()
}

fn path_of(name: &str) -> Result<std::string::String, NameRefused> {
    let mut out = OwnString::new_in(alloc());
    index_path_in(name, &mut out)?;
    Ok(out.as_str().to_owned())
}

const SUM: &str = "abababababababababababababababababababababababababababababababab";

/// Prints nothing; a wrong rule fails on the first path that differs, e.g.
/// `left: "se/rd/Serde", right: "se/rd/serde"`.
#[test]
fn index_paths_are_cargos_and_refuse_names_that_leave_their_folder() {
    assert_eq!(path_of("a").as_deref(), Ok("1/a"));
    assert_eq!(path_of("ab").as_deref(), Ok("2/ab"));
    assert_eq!(path_of("abc").as_deref(), Ok("3/a/abc"));
    assert_eq!(path_of("Serde").as_deref(), Ok("se/rd/serde"));
    assert_eq!(path_of("tokio-util").as_deref(), Ok("to/ki/tokio-util"));
    assert_eq!(path_of(""), Err(NameRefused::Empty));
    assert_eq!(path_of("../etc"), Err(NameRefused::Char { at: 0, ch: '.' }));
    assert_eq!(path_of("a/b"), Err(NameRefused::Char { at: 1, ch: '/' }));
    assert!(matches!(path_of(&"a".repeat(300)), Err(NameRefused::TooLong { len: 300 })));

    let mut cache = OwnString::new_in(alloc());
    cache_path_in(&RegistryKey::of(CRATES_IO_INDEX), "se/rd/serde", &mut cache);
    assert!(cache.as_str().starts_with("index/") && cache.as_str().ends_with("/se/rd/serde"));
    assert_eq!(cache.len(), "index/".len() + 32 + "/se/rd/serde".len());
    assert_ne!(RegistryKey::of(CRATES_IO_INDEX), RegistryKey::of("sparse+https://example.org/index/"));
}

/// Prints nothing; a wrong rule fails naming the status or header, e.g.
/// `left: Other, right: NoSuchCrate`.
#[test]
fn conditions_validators_and_statuses_follow_the_protocol() {
    assert_eq!(condition(Validator::None), None);
    assert_eq!(condition(Validator::Etag("\"v1\"")), Some(("if-none-match", "\"v1\"")));
    assert_eq!(condition(Validator::LastModified("Tue, 1 Jan 2030")), Some(("if-modified-since", "Tue, 1 Jan 2030")));
    assert_eq!(validator_of(Some(" \"v1\" "), Some("Tue")), Validator::Etag("\"v1\""));
    assert_eq!(validator_of(None, Some("Tue")), Validator::LastModified("Tue"));
    assert_eq!(validator_of(Some(""), None), Validator::None);
    assert_eq!(answer_kind(200), AnswerKind::New);
    assert_eq!(answer_kind(304), AnswerKind::NotModified);
    for status in [404, 410, 451] {
        assert_eq!(answer_kind(status), AnswerKind::NoSuchCrate);
    }
    assert_eq!(answer_kind(401), AnswerKind::Unauthorized);
    for status in [429, 500, 503] {
        assert_eq!(answer_kind(status), AnswerKind::Transient);
    }
    for status in [301, 403, 418] {
        assert_eq!(answer_kind(status), AnswerKind::Other);
    }
}

/// Prints nothing; a wrong rule fails on the URL or field, e.g.
/// `left: "https://dl/serde/1.0.0", right: "https://dl/serde/1.0.0/download"`.
#[test]
fn config_json_and_download_urls_are_cargos() {
    let config = config_in(br#"{"dl":"https://static.crates.io/crates","api":"https://crates.io","other":{"x":[1]}}"#, alloc())
        .unwrap();
    assert_eq!(config.dl.as_str(), "https://static.crates.io/crates");
    assert_eq!(config.api.as_ref().map(|it| it.as_str()), Some("https://crates.io"));
    assert!(!config.auth_required);
    assert!(matches!(config_in(br#"{"api":"x"}"#, alloc()), Err(ConfigRefused::Missing { field: "dl" })));
    assert!(matches!(config_in(br#"{"dl":1}"#, alloc()), Err(ConfigRefused::WrongType { field: "dl" })));
    assert!(matches!(config_in(br#"{"dl":"a","dl":"b"}"#, alloc()), Err(ConfigRefused::Twice { field: "dl" })));
    assert!(matches!(config_in(b"[]", alloc()), Err(ConfigRefused::NotAnObject)));

    let url = |template: &str, name: &str| {
        let mut out = OwnString::new_in(alloc());
        crate_url_in(template, name, "1.0.0", &[0xab; 32], &mut out).map(|()| out.as_str().to_owned())
    };
    assert_eq!(url("https://dl", "serde").as_deref(), Ok("https://dl/serde/1.0.0/download"));
    assert_eq!(
        url("https://dl/{prefix}/{lowerprefix}/{crate}-{version}.crate?{sha256-checksum}{x}", "Serde").as_deref(),
        Ok(format!("https://dl/Se/rd/se/rd/Serde-1.0.0.crate?{SUM}{{x}}").as_str())
    );
    assert_eq!(url("https://dl/{prefix}", "abc").as_deref(), Ok("https://dl/3/a"));
    assert_eq!(url("https://dl/{crate}", "../x"), Err(NameRefused::Char { at: 0, ch: '.' }));
}

fn demo_file() -> std::string::String {
    let line1 = format!(
        r#"{{"name":"demo","vers":"1.0.0","deps":[{{"name":"serde","req":"^1.0","features":["derive",""],"optional":true,"default_features":false,"target":"cfg(unix)","kind":"dev","registry":null,"package":"serde_real"}}],"cksum":"{SUM}","features":{{"std":["serde/std"]}},"features2":{{"std":["dep:serde"]}},"yanked":false,"links":null,"v":2,"rust_version":"1.70","pubtime":"2024-02-29T12:00:00Z"}}"#
    );
    let line2 = format!(r#"{{"name":"demo","vers":"0.9.0","deps":[],"cksum":"{SUM}","features":{{}},"yanked":true}}"#);
    let line3 = format!(
        r#"{{"name":"demo","vers":"1.0.0-alpha.1","deps":[{{"name":"x","req":"not a req!!"}}],"cksum":"{SUM}","features":{{}}}}"#
    );
    let line4 = format!(r#"{{"name":"demo","vers":"2.0.0","deps":[],"cksum":"{SUM}","features":{{}},"v":3}}"#);
    let line6 = r#"{"name":"demo","vers":"0.1.0","deps":[],"cksum":"zz","features":{},"future_key":{"a":[1,2]}}"#;
    format!("{line1}\n{line2}\n{line3}\n{line4}\n\n{line6}\n")
}

/// Prints nothing; a wrong parser fails on the row that differs, e.g.
/// `left: Candidate, right: Invalid(DepReq)` for a dependency whose `req` was let pass.
#[test]
fn an_index_file_parses_into_a_record_in_version_order_with_cargos_statuses() {
    let file = demo_file();
    let record = parse_in("demo", file.as_bytes(), Validator::Etag("\"e1\""), IndexLimits::REAL, alloc()).unwrap();
    let view = IndexView::open(record.as_slice()).unwrap();
    assert_eq!(view.name(), "demo");
    assert_eq!(view.validator(), Validator::Etag("\"e1\""));
    let order: std::vec::Vec<(&str, u32, Status)> = view.versions().map(|v| (v.vers(), v.line(), v.status())).collect();
    assert_eq!(
        order,
        [
            ("0.1.0", 6, Status::Invalid(Field::Cksum)),
            ("0.9.0", 2, Status::Yanked),
            ("1.0.0-alpha.1", 3, Status::Invalid(Field::DepReq)),
            ("1.0.0", 1, Status::Candidate),
            ("2.0.0", 4, Status::Unsupported(3)),
        ]
    );
    assert_eq!(view.version(2).deps().count(), 0, "an invalid version keeps no dependencies");

    let one = view.version(3);
    assert_eq!(one.cksum(), [0xab; 32]);
    assert_eq!((one.links(), one.rust_version(), one.pubtime(), one.schema()), (None, Some("1.70"), Some("2024-02-29T12:00:00Z"), 2));
    let deps: std::vec::Vec<_> = one.deps().collect();
    assert_eq!(deps.len(), 1);
    let serde = &deps[0];
    assert_eq!((serde.name_in_toml(), serde.package(), serde.req()), ("serde", "serde_real", "^1.0"));
    assert_eq!(serde.features().collect::<std::vec::Vec<_>>(), ["derive"], "an empty feature is left out");
    assert_eq!((serde.optional(), serde.default_features(), serde.public(), serde.artifact()), (true, false, false, false));
    assert_eq!((serde.kind(), serde.target(), serde.registry()), (DepKind::Development, Some("cfg(unix)"), None));
    let features: std::vec::Vec<(&str, std::vec::Vec<&str>)> = one.features().map(|f| (f.name(), f.values().collect())).collect();
    assert_eq!(features, [("std", vec!["serde/std"]), ("std", vec!["dep:serde"])]);

    // The same file makes the same bytes (rule 25).
    let again = parse_in("demo", file.as_bytes(), Validator::Etag("\"e1\""), IndexLimits::REAL, alloc()).unwrap();
    assert_eq!(again.as_slice(), record.as_slice());
    // A record of another form is refused, never read.
    let mut old = record.as_slice().to_vec();
    old[7] ^= 0xff;
    assert!(IndexView::open(&old).is_err());
}

/// Prints the refusal it checks, e.g. "line 2, `the line`: not JSON at column 23: the text ends
/// where a value was wanted"; a parser that dropped the line fails with `left: Ok(..)`.
#[test]
fn a_malformed_line_is_a_typed_error_naming_its_line_and_field() {
    let good = format!(r#"{{"name":"demo","vers":"1.0.0","deps":[],"cksum":"{SUM}","features":{{}}}}"#);
    let refused = |text: &str| parse_in("demo", text.as_bytes(), Validator::None, IndexLimits::REAL, alloc()).unwrap_err();

    let torn = refused(&format!("{good}\n{{\"name\":\"demo\",\"vers\":"));
    assert_eq!((torn.line, torn.field), (2, Field::Json));
    assert!(matches!(torn.bad, Bad::Json { .. }));
    println!("{}", shown(&torn));
    assert!(shown(&torn).starts_with("line 2, `the line`: not JSON at column "));

    let other = refused(&format!("{good}\n\n{}", good.replace("\"demo\"", "\"other\"")));
    assert_eq!((other.line, other.field, other.bad), (3, Field::Name, Bad::OtherCrate));
    let no_vers = refused(r#"{"name":"demo","deps":[]}"#);
    assert_eq!((no_vers.line, no_vers.field, no_vers.bad), (1, Field::Vers, Bad::Missing));
    let twice = refused(r#"{"name":"demo","vers":"1.0.0","vers":"2.0.0"}"#);
    assert_eq!((twice.field, twice.bad), (Field::Vers, Bad::Twice));
    let not_version = refused(r#"{"name":"demo","vers":"one"}"#);
    assert_eq!((not_version.field, not_version.bad), (Field::Vers, Bad::NotAVersion));
    let not_object = refused("[1]");
    assert_eq!((not_object.line, not_object.bad), (1, Bad::NotAnObject));
    let tight = IndexLimits { most_bytes: 8, most_lines: 1 };
    let big = parse_in("demo", good.as_bytes(), Validator::None, tight, alloc()).unwrap_err();
    assert_eq!((big.line, big.field), (0, Field::File));
}

/// One crate of the simulated registry.
struct Krate {
    name: &'static str,
    /// Its file's lines, as the registry serves them; `None` when it has no such crate.
    versions: Option<std::vec::Vec<u32>>,
    etag: std::string::String,
}

impl Krate {
    fn body(&self, spelling: &str) -> std::string::String {
        let mut body = std::string::String::new();
        for minor in self.versions.iter().flatten() {
            body.push_str(&format!(
                r#"{{"name":"{spelling}","vers":"0.{minor}.0","deps":[{{"name":"serde","req":"^1","features":[],"optional":false,"default_features":true,"target":null,"kind":"normal"}}],"cksum":"{SUM}","features":{{}},"yanked":{}}}"#,
                minor % 3 == 0
            ));
            body.push('\n');
        }
        body
    }
}

const NAMES: [&str; 6] = ["serde", "tokio-util", "a", "ab", "abc", "tokio_util"];

/// The registry's files by index path: (body, etag).
type Origin = BTreeMap<std::string::String, (std::string::String, std::string::String)>;

struct Run {
    /// Requests per index path, and whether each carried a condition.
    asked: BTreeMap<std::string::String, (u32, u32)>,
    writes: u32,
}

/// Drives `index` against `origin` and `cache` (records by index path) to its end, dealing
/// `faults` (failures per path still to come) to the first asks.
fn drive(
    tape: &Tape,
    index: &mut Index<'_>,
    origin: &Origin,
    cache: &mut BTreeMap<std::string::String, std::vec::Vec<u8>>,
    faults: &mut BTreeMap<std::string::String, u32>,
) -> Result<Run, Broke> {
    let base = "https://index.crates.io/";
    let mut run = Run { asked: BTreeMap::new(), writes: 0 };
    let mut now = 0u64;
    let mut asked_any = false;
    let mut pending = std::vec::Vec::new();
    for _ in 0..20_000 {
        match index.poll(now) {
            Some(IndexEffect::Read { file }) => {
                if asked_any {
                    return Err(Broke::Oracle("a read after a request".into()));
                }
                let record = cache.get(index.path(file)).cloned();
                index.read(file, record.as_deref());
            }
            Some(IndexEffect::Http { ask, url, condition, .. }) => {
                asked_any = true;
                let Some(path) = url.as_str().strip_prefix(base) else {
                    return Err(Broke::Oracle(format!("a request off its registry: {}", url.as_str())));
                };
                let condition = condition.map(|(name, value)| (name, value.as_str().to_owned()));
                let counts = run.asked.entry(path.to_owned()).or_insert((0, 0));
                counts.0 += 1;
                counts.1 += u32::from(condition.is_some());
                pending.push((ask, path.to_owned(), condition));
            }
            Some(IndexEffect::Write { file, record }) => {
                run.writes += 1;
                cache.insert(index.path(file).to_owned(), record.as_slice().to_vec());
                index.written(file, Ok(()));
            }
            None if !pending.is_empty() => {
                let (ask, path, condition) = pending.swap_remove(tape.index(pending.len(), "answer"));
                now += 1_000;
                let fault = faults.get_mut(&path).filter(|left| **left > 0);
                let reply = match (fault, origin.get(&path)) {
                    (Some(left), _) => {
                        *left -= 1;
                        match tape.one_in(2, "reset") {
                            true => IndexReply::Failed(NetFailure::Reset),
                            false => IndexReply::Body { status: 503, etag: None, last_modified: None, bytes: b"" },
                        }
                    }
                    (None, None) => IndexReply::Body { status: 404, etag: None, last_modified: None, bytes: b"" },
                    (None, Some((_, etag))) if condition.as_ref().is_some_and(|(name, value)| *name == "if-none-match" && value == etag) => {
                        IndexReply::Body { status: 304, etag: Some(etag.as_str()), last_modified: None, bytes: b"" }
                    }
                    (None, Some((body, etag))) => {
                        IndexReply::Body { status: 200, etag: Some(etag.as_str()), last_modified: None, bytes: body.as_bytes() }
                    }
                };
                index.answered(ask, reply);
            }
            None if index.is_done() => return Ok(run),
            None => match index.next_deadline() {
                Some(deadline) => now = now.max(deadline),
                None => return Err(Broke::Oracle("a hang: nothing in flight, nothing due, and not done".into())),
            },
        }
    }
    Err(Broke::Oracle("a hang: the run took 20,000 steps".into()))
}

fn case(tape: &Tape, plant: IndexPlant) -> Result<(), Broke> {
    INDEX_PLANT.set(plant);
    let result = case_inner(tape);
    INDEX_PLANT.set(IndexPlant::None);
    result
}

fn case_inner(tape: &Tape) -> Result<(), Broke> {
    // The registry: each name has its file or not.
    let mut krates = std::vec::Vec::new();
    for (at, name) in NAMES.iter().enumerate() {
        let versions = match tape.one_in(4, "absent") {
            true => None,
            false => Some((0..1 + tape.below(4, "versions") as u32).map(|_| tape.below(40, "minor") as u32).collect()),
        };
        krates.push(Krate { name: *name, versions, etag: format!("\"e{at}-{}\"", tape.below(1000, "etag")) });
    }
    let mut origin = Origin::new();
    for krate in &krates {
        if krate.versions.is_some() {
            let path = path_of(krate.name).map_err(|e| Broke::Oracle(format!("{e:?}")))?;
            origin.insert(path, (krate.body(krate.name), krate.etag.clone()));
        }
    }
    // The asks: a few names, one maybe twice in upper case, one maybe by every spelling.
    let mut asks = std::vec::Vec::new();
    for name in NAMES.iter().take(5) {
        if !tape.one_in(3, "skip") {
            asks.push(IndexAsk { name: *name, kind: QueryKind::Exact });
        }
    }
    if asks.iter().any(|ask| ask.name == "serde") && tape.one_in(2, "twice") {
        asks.push(IndexAsk { name: "SERDE", kind: QueryKind::Exact });
    }
    if tape.one_in(2, "spellings") {
        asks.push(IndexAsk { name: "tokio_util", kind: QueryKind::AlternativeNames });
    }
    if asks.is_empty() {
        return Ok(());
    }

    let mut cache = BTreeMap::new();
    let mut faults = BTreeMap::new();
    for path in origin.keys() {
        faults.insert(path.clone(), u32::from(tape.one_in(3, "fault")));
    }
    let dealt = faults.clone();
    let mut cold = Index::new(CRATES_IO_INDEX, &asks, Update::Check, Limits::REAL, IndexLimits::REAL, alloc())
        .map_err(|e| Broke::Oracle(shown(&e)))?;
    let run = drive(tape, &mut cold, &origin, &mut cache, &mut faults)?;
    for file in 0..cold.files() {
        let path = cold.path(file);
        // One try (D172): a file is asked once, its fault or not.
        let (asked, _) = run.asked.get(path).copied().unwrap_or((0, 0));
        if asked != 1 {
            return Err(Broke::Oracle(format!("asked again: {path} asked {asked} times, not 1")));
        }
        let faulted = dealt.get(path).copied().unwrap_or(0) > 0;
        match (cold.end(file), origin.get(path)) {
            (FileEnd::Failed(IndexWhy::Net { .. } | IndexWhy::Status { .. }), Some(_)) if faulted => {}
            (end, _) if faulted => return Err(Broke::Oracle(format!("a failure stood: {path} dealt one ended {end:?}"))),
            (FileEnd::Absent, None) => {}
            (end, None) => return Err(Broke::Oracle(format!("absent: {path} ended {end:?}"))),
            (FileEnd::Fetched, Some((body, etag))) => {
                let record = cache.get(path).ok_or_else(|| Broke::Oracle(format!("wrong record: {path} not kept")))?;
                let view = IndexView::open(record).map_err(|e| Broke::Oracle(format!("wrong record: {path}: {e:?}")))?;
                let lines = body.lines().count();
                let sorted = view.versions().map(|v| v.vers()).collect::<std::vec::Vec<_>>();
                let minors: std::vec::Vec<u32> = sorted.iter().filter_map(|v| v.split('.').nth(1)?.parse().ok()).collect();
                let in_order = minors.len() == lines && minors.windows(2).all(|w| w[0] <= w[1]);
                if view.name() != cold.spelling(file) || view.validator() != Validator::Etag(etag) || view.len() != lines || !in_order {
                    return Err(Broke::Oracle(format!("wrong record: {path}")));
                }
            }
            (end, Some(_)) => return Err(Broke::Oracle(format!("{path} ended {end:?} on a cold run"))),
        }
    }
    // Each ask is answered by its own name's files: in any case, and for every spelling only
    // up to `-` and `_`.
    for (ask, wanted) in asks.iter().enumerate() {
        let canonical = |name: &str| name.to_ascii_lowercase().replace('-', "_");
        let own = |file: &u32| match wanted.kind {
            QueryKind::AlternativeNames => canonical(cold.spelling(*file)) == canonical(wanted.name),
            _ => cold.spelling(*file) == wanted.name.to_ascii_lowercase(),
        };
        if !cold.files_of(ask as u32).iter().all(own) {
            return Err(Broke::Oracle(format!("ask {ask} answered by another crate's file")));
        }
    }

    // The next command: the same asks over the cache the cold run wrote, the network well. A
    // file the cold run failed has no record, so it is asked for again and fetched (owed, not
    // stood). The files that stood are the warm run's to judge, below.
    let mut none = BTreeMap::new();
    let mut next = Index::new(CRATES_IO_INDEX, &asks, Update::Check, Limits::REAL, IndexLimits::REAL, alloc())
        .map_err(|e| Broke::Oracle(shown(&e)))?;
    drive(tape, &mut next, &origin, &mut cache, &mut none)?;
    for file in 0..next.files() {
        let path = next.path(file);
        if dealt.get(path).copied().unwrap_or(0) > 0 && *next.end(file) != FileEnd::Fetched {
            return Err(Broke::Oracle(format!("owed: {path} failed cold and ended {:?} on the next run", next.end(file))));
        }
    }

    // Warm: the same asks over the cache the two runs wrote.
    let mut warm = Index::new(CRATES_IO_INDEX, &asks, Update::Check, Limits::REAL, IndexLimits::REAL, alloc())
        .map_err(|e| Broke::Oracle(shown(&e)))?;
    let run = drive(tape, &mut warm, &origin, &mut cache, &mut none)?;
    if warm.fetched_bytes() != 0 {
        return Err(Broke::Oracle(format!("bytes fetched on a warm run: {}", warm.fetched_bytes())));
    }
    if warm.parsed() != 0 || run.writes != 0 {
        return Err(Broke::Oracle(format!("rewritten on a warm run: {} parsed, {} written", warm.parsed(), run.writes)));
    }
    for file in 0..warm.files() {
        let wanted = match origin.contains_key(warm.path(file)) {
            true => FileEnd::Cached,
            false => FileEnd::Absent,
        };
        if *warm.end(file) != wanted {
            return Err(Broke::Oracle(format!("{} ended {:?} on a warm run", warm.path(file), warm.end(file))));
        }
    }
    Ok(())
}

/// The invariant: the sparse-index machine reads each crate index once cold, sends cached validators when warm, and downloads nothing when unchanged.
/// A clean machine passes 300 cases; a failure prints the oracle's words and the tape, e.g.
/// "asked again: se/rd/serde asked 2 times, not 1".
#[test]
fn the_index_reads_each_file_once_cold_and_downloads_nothing_warm() {
    blaze_sim::check(Config::cases(300), |tape| case(tape, IndexPlant::None));
}

/// Passes when the oracle catches the planted bug: a request without the cached validator
/// downloads the file again, "bytes fetched on a warm run".
#[test]
fn the_oracle_catches_a_request_sent_without_its_validator() {
    blaze_sim::control(Config::cases(300), IndexPlant::None, IndexPlant::NoValidator, "bytes fetched on a warm run", case);
}

/// Two asks of one crate in two cases are one file and one request; a crate the registry has
/// not is absent, an answer. Prints nothing; a wrong machine fails on the count, e.g.
/// `left: 2, right: 1`.
#[test]
fn two_asks_of_one_crate_make_one_request_and_a_404_is_absent() {
    let asks = [
        IndexAsk { name: "Serde", kind: QueryKind::Exact },
        IndexAsk { name: "serde", kind: QueryKind::RejectedVersions },
        IndexAsk { name: "nope", kind: QueryKind::Exact },
    ];
    let mut index = Index::new(CRATES_IO_INDEX, &asks, Update::Check, Limits::REAL, IndexLimits::REAL, alloc()).unwrap();
    assert_eq!(index.files(), 2);
    assert_eq!(index.files_of(0), index.files_of(1));
    let mut origin = Origin::new();
    let serde = Krate { name: "serde", versions: Some(vec![1, 2]), etag: "\"s\"".into() };
    origin.insert("se/rd/serde".into(), (serde.body("serde"), serde.etag.clone()));
    let mut cache = BTreeMap::new();
    let mut faults = BTreeMap::new();
    blaze_sim::check(Config::cases(1), |tape| {
        let run = drive(tape, &mut index, &origin, &mut cache, &mut faults)?;
        assert_eq!(run.asked.get("se/rd/serde"), Some(&(1, 0)));
        assert_eq!(run.asked.get("no/pe/nope"), Some(&(1, 0)));
        Ok::<(), Broke>(())
    });
    assert_eq!(*index.end(index.files_of(0)[0]), FileEnd::Fetched);
    assert_eq!(*index.end(index.files_of(2)[0]), FileEnd::Absent);
    assert!(index.failure(2).is_none());
}

/// A machine refuses what it cannot name a file for. Prints nothing.
#[test]
fn a_batch_with_a_bad_url_or_name_is_refused_by_name() {
    let ask = [IndexAsk { name: "serde", kind: QueryKind::Exact }];
    let refused = |url: &str| Index::new(url, &ask, Update::Check, Limits::REAL, IndexLimits::REAL, alloc()).err();
    assert_eq!(refused("https://index.crates.io/"), Some(crate::index::Refused::Url));
    assert_eq!(refused("sparse+http://index.crates.io/"), Some(crate::index::Refused::Url));
    assert_eq!(refused("sparse+https://index.crates.io"), Some(crate::index::Refused::Url));
    let bad = [IndexAsk { name: "ok", kind: QueryKind::Exact }, IndexAsk { name: "a b", kind: QueryKind::Exact }];
    let err = Index::new(CRATES_IO_INDEX, &bad, Update::Check, Limits::REAL, IndexLimits::REAL, alloc()).err();
    assert_eq!(err, Some(crate::index::Refused::Name { ask: 1, why: NameRefused::Char { at: 1, ch: ' ' } }));
}
