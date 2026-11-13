//! Packs in the daemon (docs/remote-cache.md, "Packs"): the thin driver of
//! [`blaze_pack`]'s cores. Lane packs-1112.
//!
//! What is here:
//!
//! - the hash and the signature check the cores are handed ([`B3`]: the store's own content
//!   hash and a domain of its own for a manifest; [`Ed25519`]: `blaze_global::sign`'s keys,
//!   the one home of Ed25519 in blaze);
//! - the user's settings ([`Config`], `<root>/packs.json`): where packs are read from, which
//!   keys are trusted, which peers are asked, whether and to whom this daemon's store is
//!   served. Absent, nothing here does anything;
//! - a place packs are read from ([`Base`]: a directory or a base URL, the same layout,
//!   `blaze_pack::layout`);
//! - fetch on miss ([`asked`], [`missed`]): at the point a unit's record is not in the store,
//!   before the unit is built (`unit::compile::names`), each key the lookup misses is asked
//!   of the packs the settings name and of the listed peers. A pack of a source that has
//!   the key is installed whole (it is what its publisher selected to travel together),
//!   every record checked against its manifest before it is put; a peer is asked by key,
//!   in one batch for every unit's keys and then for what the lookup still misses, and a
//!   record it states is checked against its signed statement. The lookup then reads what
//!   was put as if it had been there.
//!
//! **Not a fallback** (AGENTS.md rule 3). A record no source has is a miss, the defined
//! answer of a cache, and its unit is built: that is what building means. A source that
//! cannot be reached is said once (an error in the daemon's log naming the source and why),
//! and once in the output of the build that ends next, as a warning: [`take_notices`]),
//! asked nothing more for [`PAUSE`], and the unit is built: the error is shown, and nothing
//! is taken from elsewhere in its place. A record that arrives and does not hash to what its
//! manifest names is a hard error naming the source and the key: the unit's lookup fails,
//! and so does the build.
//!
//! **Waits.** A lookup that asks the packs runs in the span `pack fetch on miss`; each read
//! of a source is a span `pack read` naming the object and the bytes. A read is bounded in
//! time by the HTTP client's own deadline (`blaze_registry::http`) and in bytes by the bound
//! every request states (`HyperHttp::get_bounded`: a head 1 KiB, a manifest its head's
//! stated length, a range its own length, a peer's statements their format's most); a
//! directory's by the file system. One build task waits; the daemon's loop never does.
//!
//! Where the bytes go: the store of the [`Packs`] the job carries (`JobInput::packs`; for the
//! daemon, `blaze_cas::Handle::installed`, the process's one store writer). A put is readable
//! at once and durable at the daemon's next flush. A request that said `--no-prebuilt` carries
//! none ([`given`]), and nothing is asked for it.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use blaze_api::ApiError;
use blaze_cas::{Key, Kind};
use blaze_ids::ContentId;
use blaze_pack::{
    Hashes, Id, Manifest, Record, Signatures, Trust,
    fetch::{Answer, Effect, Fetch, Outcome, Source},
    head::Head,
    import::Import,
    layout, peer,
    spans::Spans,
};
use blaze_registry::http::{Bounds, HyperHttp};

pub(crate) mod daemon;
pub(crate) mod endpoint;
pub(crate) mod stored;

/// The settings' file, under blaze's root.
pub(crate) const SETTINGS: &str = "packs.json";
/// The pins asked of every source when the settings name none: the standard library's
/// records and bstd's, by the names the store's collector pins them under.
const PINS: [&str; 2] = [super::collect::TOOLCHAIN, super::collect::BSTD];
/// How long a source that failed is asked nothing more.
const PAUSE: Duration = Duration::from_secs(60);
/// The bytes of a pack's body read in one request when records are read in order.
pub(crate) const SPAN: u64 = 4 << 20;

const MANIFEST_DOMAIN: &str = "blaze pack manifest 1";
const SIGNED_DOMAIN: &str = "blaze pack signed message 1";

/// BLAKE3, as the store computes a value's content hash and under a domain of its own for a
/// manifest.
pub(crate) struct B3;

impl Hashes for B3 {
    fn content(&self, bytes: &[u8]) -> Id {
        Id(blaze_cas::content_of(bytes).0)
    }

    fn manifest(&self, bytes: &[u8]) -> Id {
        Id(ContentId::of(MANIFEST_DOMAIN, bytes).0)
    }
}

pub(crate) fn key_of(id: &Id) -> Key {
    Key(ContentId(id.0))
}

pub(crate) fn id_of(key: &Key) -> Id {
    Id(key.0.0)
}

/// What is signed for `message`: `blaze_global::sign` signs records, so a message is signed
/// as the record of a key that is the message's hash under a domain of this module's, of a
/// kind and content no value has. Ed25519 has one home in blaze, and no signature made here
/// is a signature of a value of the global cache.
#[cfg(feature = "native")]
fn carrier(message: &[u8]) -> blaze_global::Record {
    blaze_global::Record {
        key: Key(ContentId::of(SIGNED_DOMAIN, message)),
        kind: Kind::Pin,
        content: ContentId([0; 32]),
        len: message.len() as u64,
    }
}

/// Ed25519, through `blaze_global::sign`.
pub(crate) struct Ed25519;

impl Signatures for Ed25519 {
    #[cfg(feature = "native")]
    fn verify(&self, signer: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
        let publisher = blaze_global::PublisherKey(*signer);
        let signed = blaze_global::Signed { record: carrier(message), publisher, signature: *signature };
        // alloc: once a signature checked, the one key it is checked against
        blaze_global::Trust::new(vec![publisher]).is_ok_and(|trust| trust.verify(&signed).is_ok())
    }

    /// A blaze without `native` has no Ed25519: nothing is ever taken as signed.
    #[cfg(not(feature = "native"))]
    fn verify(&self, _: &[u8; 32], _: &[u8], _: &[u8; 64]) -> bool {
        false
    }
}

/// The public key of the Ed25519 secret `seed` and its signature of `message`.
#[cfg(feature = "native")]
pub(crate) fn sign(seed: &[u8; 32], message: &[u8]) -> Result<([u8; 32], [u8; 64]), String> {
    let publisher = blaze_global::Publisher::from_seed(seed)?;
    Ok((publisher.key().0, publisher.sign(carrier(message)).signature))
}

#[cfg(not(feature = "native"))]
pub(crate) fn sign(_: &[u8; 32], _: &[u8]) -> Result<([u8; 32], [u8; 64]), String> {
    Err("this blaze was built without `native`, which has the Ed25519 signatures a pack is signed with".to_owned())
}

/// The scope of the packs this blaze reads and writes: its version and the host's triple.
/// A label to find a pack by, never a part of a key: records of another scope are records
/// no build of this blaze asks for.
pub(crate) fn scope() -> Result<String, String> {
    let Some(host) = crate::toolchain::host() else {
        return Err("this blaze does not know the host's target triple, which names its packs".to_owned());
    };
    let scope = format!("{}-{host}", env!("CARGO_PKG_VERSION")).to_ascii_lowercase(); // alloc: once a command or a daemon
    layout::name("the scope", &scope).map_err(|refused| refused.to_string())?;
    Ok(scope)
}

/// The directories of this home a record must not hold to travel, as the one searcher of a
/// root left in bytes knows them (`Places::tied`, which is `Roots::embedded`): where builds
/// are (`<root>/build`), the shared standard library directory and the fetched sources.
/// An export has no job, so no workspace, tools or launcher: those are no root here (an
/// empty path is none).
pub(crate) fn home_places() -> Result<super::unit::portable::Places, String> {
    let said = |error: anyhow::Error| format!("{error:#}"); // alloc: error path
    let builds = super::builds().map_err(said)?;
    let sources = super::sources().map_err(said)?;
    let std = super::machine().map_err(said)?.join("std");
    Ok(super::unit::portable::Places::new(
        "",
        &builds.to_string_lossy(),
        &std.to_string_lossy(),
        Path::new(""),
        Path::new(""),
        &sources,
    ))
}

/// The kinds of record a pack may bring into the store. Never a pin (a pack's pin is made
/// by the import, of the keys it checked) and never the daemon's image (one checkout's, by
/// path). Machine code a loader maps is taken only when a trusted key signed for it:
/// `signed` is false for a file the user imported on their own word.
pub(crate) fn admitted(signed: bool) -> Vec<u8> {
    Kind::ALL
        .into_iter()
        .filter(|kind| !matches!(kind, Kind::Pin | Kind::Image))
        .filter(|kind| signed || *kind != Kind::Machine)
        .map(Kind::code)
        .collect() // alloc: once an import
}

fn key_from_hex(what: &str, hex: &str) -> Result<[u8; 32], String> {
    Id::from_hex(hex.trim()).map(|id| id.0).ok_or_else(|| {
        format!("{what} is not a key: 64 hex digits, lower case, are one") // alloc: error path
    })
}

/// A peer the settings list: where it answers and the key its statements are signed with.
#[derive(Clone, Debug)]
pub(crate) struct PeerAt {
    pub url: String,
    pub key: [u8; 32],
}

/// The endpoint the settings ask for: the one address to listen on, and the public keys
/// whose signed requests are answered.
#[derive(Clone, Debug)]
pub(crate) struct Serve {
    pub bind: String,
    pub readers: Vec<[u8; 32]>,
}

/// `<root>/packs.json`:
///
/// ```json
/// { "sources": ["/mnt/team/packs", "https://example.invalid/packs"],
///   "install": "https://example.invalid/packs",
///   "pins": ["toolchain", "bstd"],
///   "trusted": ["<64 hex: a public key whose packs are taken>"],
///   "key_file": "/home/me/.blaze-peer.key",
///   "peers": [{ "url": "http://10.0.0.5:7420", "key": "<64 hex: that peer's public key>" }],
///   "serve": { "bind": "10.0.0.4:7420", "readers": ["<64 hex: a public key that may read>"] },
///   "prebuilt": true }
/// ```
///
/// `prebuilt` (true when left out): whether records someone else built are taken at all. With
/// `false`, no source and no peer is asked, on a miss or for a sealed tree's layer
/// (`serve::sealed`), and what is missing is built here; nothing else changes. No source is
/// built in: a bucket that serves this layout over HTTPS (R2, S3, a directory) is named in
/// `sources` and `install`, and the key its packs are signed with in `trusted`.
///
/// Every field may be left out; a field it does not know is refused by name. `key_file`
/// holds this machine's Ed25519 secret (64 hex digits): it signs what this machine states
/// as a peer and what it asks of one, so `peers` and `serve` both need it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Config {
    pub sources: Vec<Base>,
    pub install: Option<Base>,
    pub pins: Vec<String>,
    pub trusted: Vec<[u8; 32]>,
    pub key_file: Option<PathBuf>,
    pub peers: Vec<PeerAt>,
    pub serve: Option<Serve>,
    /// Whether records built elsewhere are taken ([`Config`]'s `prebuilt`).
    pub prebuilt: bool,
}

impl Config {
    /// The settings of blaze's root, or none when the file is not there.
    pub(crate) fn read() -> Result<Option<Config>, String> {
        let root = super::root().map_err(|error| format!("{error:#}"))?; // alloc: error path
        Config::read_at(&root.join(SETTINGS))
    }

    pub(crate) fn read_at(path: &Path) -> Result<Option<Config>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("reading {}: {error}", path.display())), // alloc: error path
        };
        Config::parse(&text).map(Some).map_err(|why| format!("{}: {why}", path.display())) // alloc: error path
    }

    pub(crate) fn parse(text: &str) -> Result<Config, String> {
        use serde_json::Value;
        let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let Value::Object(fields) = value else { return Err("it is not a JSON object".to_owned()) };
        let text_of = |what: &str, value: &Value| match value {
            Value::String(text) => Ok(text.clone()), // clone: once a setting read
            _ => Err(blaze_report::say!(blaze_report::say::Code(what), " is not a string").flat()), // alloc: error path
        };
        let list_of = |what: &str, value: &Value| match value {
            Value::Array(items) => items.iter().map(|item| text_of(what, item)).collect::<Result<Vec<String>, String>>(),
            _ => Err(blaze_report::say!(blaze_report::say::Code(what), " is not a list").flat()), // alloc: error path
        };
        let mut config = Config { prebuilt: true, ..Config::default() };
        for (name, value) in &fields {
            match name.as_str() {
                "sources" => {
                    for text in list_of("sources", value)? {
                        config.sources.push(Base::parse(&text)?);
                    }
                }
                "install" => config.install = Some(Base::parse(&text_of("install", value)?)?),
                "pins" => {
                    for pin in list_of("pins", value)? {
                        layout::name("the pin", &pin).map_err(|refused| refused.to_string())?;
                        config.pins.push(pin);
                    }
                }
                "trusted" => {
                    for hex in list_of("trusted", value)? {
                        config.trusted.push(key_from_hex("a key of `trusted`", &hex)?);
                    }
                }
                "key_file" => config.key_file = Some(PathBuf::from(text_of("key_file", value)?)),
                "peers" => {
                    let Value::Array(items) = value else { return Err("`peers` is not a list".to_owned()) };
                    for item in items {
                        let (Some(url), Some(key)) = (item.get("url"), item.get("key")) else {
                            return Err("a peer is `{ \"url\": ..., \"key\": ... }`".to_owned());
                        };
                        let url = text_of("peers.url", url)?;
                        if !matches!(Base::parse(&url)?, Base::Url(_)) {
                            return Err(blaze_report::say!("the peer ", blaze_report::say::Code(&shown(&url)), " is not an http(s) address").flat()); // alloc: error path
                        }
                        let key = key_from_hex("a peer's `key`", &text_of("peers.key", key)?)?;
                        config.peers.push(PeerAt { url: url.trim_end_matches('/').to_owned(), key });
                    }
                }
                "serve" => {
                    let Value::Object(serve) = value else { return Err("`serve` is not an object".to_owned()) };
                    let field = |name: &str| serve.get(name).ok_or_else(|| blaze_report::say!(blaze_report::say::Code("serve"), " has no ", blaze_report::say::Code(name)).flat()); // alloc: error path
                    if let Some(other) = serve.keys().find(|it| !matches!(it.as_str(), "bind" | "readers")) {
                        return Err(format!("`serve.{other}` is not a setting")); // alloc: error path
                    }
                    let bind = text_of("serve.bind", field("bind")?)?;
                    let mut readers = Vec::new(); // alloc: once the settings read
                    for hex in list_of("serve.readers", field("readers")?)? {
                        readers.push(key_from_hex("a key of `serve.readers`", &hex)?);
                    }
                    config.serve = Some(Serve { bind, readers });
                }
                "prebuilt" => {
                    let Value::Bool(prebuilt) = value else { return Err("`prebuilt` is not true or false".to_owned()) };
                    config.prebuilt = *prebuilt;
                }
                other => return Err(blaze_report::say!(blaze_report::say::Code(other), " is not a setting").flat()), // alloc: error path
            }
        }
        if config.key_file.is_none() && (config.serve.is_some() || !config.peers.is_empty()) {
            return Err("`peers` and `serve` need `key_file`: this machine's own key, which signs what it asks and what it states".to_owned());
        }
        if config.pins.is_empty() {
            config.pins = PINS.iter().map(|pin| (*pin).to_owned()).collect(); // alloc: once the settings read
        }
        Ok(config)
    }

    /// The keys whose heads are taken.
    pub(crate) fn trust(&self) -> Trust {
        Trust::new(self.trusted.clone()) // clone: once the settings read
    }

    /// This machine's Ed25519 secret, from `key_file`: none when the settings name none.
    /// Never shown: an error names the file, not what it holds.
    pub(crate) fn seed(&self) -> Result<Option<[u8; 32]>, String> {
        let Some(path) = &self.key_file else { return Ok(None) };
        let text = std::fs::read_to_string(path).map_err(|error| format!("the key file {}: {error}", path.display()))?; // alloc: error path
        match Id::from_hex(text.trim()) {
            Some(id) => Ok(Some(id.0)),
            None => Err(format!("the key file {} does not hold 64 hex digits", path.display())), // alloc: error path
        }
    }
}

/// An address as it is shown: without what may hold a secret (the part before an `@`, and a
/// query).
pub(crate) fn shown(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').map_or((rest, ""), |(host, path)| (host, path));
            let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
            format!("{scheme}://{host}/{path}") // alloc: an address shown in a message
        }
        None => url.to_owned(), // alloc: an address shown in a message
    }
}

/// One GET of `url`, its answer's body held to `most` bytes by the client as it is read
/// (`HyperHttp::get_bounded`: a body that would be longer stops being read there, so no
/// server fills memory first): the status and the body. In the span `pack read`, which
/// names what is waited for; the wait ends by the client's own deadline.
fn http_get(url: &str, headers: &[(&str, &str)], most: u64) -> Result<(u16, Vec<u8>), String> {
    let said = shown(url);
    let _span = tracing::info_span!("pack read", object = %said, most).entered();
    let http = HyperHttp::shared().map_err(|error| format!("{error:#}"))?; // alloc: error path
    let answer = http
        .get_bounded(url, headers, Bounds { most, https_only: false })
        .map_err(|error| format!("{said}: {error:#}"))?; // alloc: error path
    if answer.body.len() as u64 > most {
        // The client's bound is the check; this is the same bound said again where it is used.
        return Err(format!("{said}: {} bytes, more than the {most} asked for", answer.body.len())); // alloc: error path
    }
    Ok((answer.status, answer.body))
}

/// Where packs are read from: a directory or a base URL, with `blaze_pack::layout`'s names
/// under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Base {
    Dir(PathBuf),
    Url(String),
}

impl Base {
    pub(crate) fn parse(text: &str) -> Result<Base, String> {
        if text.starts_with("https://") || text.starts_with("http://") {
            return Ok(Base::Url(text.trim_end_matches('/').to_owned())); // alloc: once a setting read
        }
        if Path::new(text).is_absolute() {
            return Ok(Base::Dir(PathBuf::from(text)));
        }
        // alloc: error path
        Err(blaze_report::say!(blaze_report::say::Code(&shown(text)), " is neither an absolute directory nor an http(s) address").flat())
    }

    /// As it is named in a message.
    pub(crate) fn named(&self) -> String {
        match self {
            Base::Dir(dir) => dir.display().to_string(), // alloc: a message
            Base::Url(url) => shown(url),
        }
    }

    /// The whole object at `path` of at most `most` bytes, or none when the base has none
    /// (a file that is not there, a 404). Anything else that is not the object is the error.
    pub(crate) fn get(&self, path: &str, most: u64) -> Result<Option<Vec<u8>>, String> {
        match self {
            Base::Dir(dir) => {
                let at = dir.join(path);
                let _span = tracing::info_span!("pack read", object = %at.display(), most).entered();
                let meta = match std::fs::metadata(&at) {
                    Ok(meta) => meta,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(format!("{}: {error}", at.display())), // alloc: error path
                };
                if meta.len() > most {
                    // alloc: error path
                    return Err(format!("{} is {} bytes, more than the {most} it may be", at.display(), meta.len()));
                }
                std::fs::read(&at).map(Some).map_err(|error| format!("{}: {error}", at.display())) // alloc: error path
            }
            Base::Url(url) => {
                let (status, body) = http_get(&format!("{url}/{path}"), &[], most)?; // alloc: once a request
                match status {
                    200 => Ok(Some(body)),
                    404 => Ok(None),
                    status => Err(format!("{}/{path}: HTTP {status}", shown(url))), // alloc: error path
                }
            }
        }
    }

    /// `len` bytes of the object at `path` from `at`: fewer when the object ends before
    /// them (which its reader refuses as a truncated body).
    pub(crate) fn range(&self, path: &str, at: u64, len: u64) -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new()); // alloc: empty, allocates nothing
        }
        match self {
            Base::Dir(dir) => {
                use std::io::{Read as _, Seek as _};
                let file_at = dir.join(path);
                let _span = tracing::info_span!("pack read", object = %file_at.display(), at, bytes = len).entered();
                let failed = |error: std::io::Error| format!("{}: {error}", file_at.display()); // alloc: error path
                let mut file = std::fs::File::open(&file_at).map_err(failed)?;
                file.seek(std::io::SeekFrom::Start(at)).map_err(failed)?;
                let mut bytes = Vec::with_capacity(len.min(SPAN) as usize); // alloc: the bytes read, handed to the caller
                file.take(len).read_to_end(&mut bytes).map_err(failed)?;
                Ok(bytes)
            }
            Base::Url(url) => {
                let range = format!("bytes={at}-{}", at + len - 1); // alloc: once a request
                // A server that honours the range sends `len` bytes; the bound holds one
                // that does not to the same.
                let (status, body) = http_get(&format!("{url}/{path}"), &[("range", range.as_str())], len)?; // alloc: once a request
                match status {
                    206 => Ok(body),
                    // alloc: error path
                    200 => Err(format!(
                        "{}/{path}: the server ignores ranges, and a pack's body is read by them",
                        shown(url)
                    )),
                    status => Err(format!("{}/{path}: HTTP {status}", shown(url))), // alloc: error path
                }
            }
        }
    }

    /// The head of `pin` for `scope` at this base, decoded, or none when the base has none.
    pub(crate) fn head(&self, scope: &str, pin: &str) -> Result<Option<Head>, String> {
        let path = layout::head_path(scope, pin).map_err(|refused| refused.to_string())?;
        let Some(bytes) = self.get(&path, blaze_pack::head::HEAD_MOST)? else { return Ok(None) };
        Head::decode(&bytes).map(Some).map_err(|refused| format!("{path} of {}: {refused}", self.named())) // alloc: error path
    }

    /// The manifest `head` names, read and checked against the head: its bytes hash to the
    /// pack's name and say what the head says.
    pub(crate) fn manifest(&self, head: &Head) -> Result<Manifest, String> {
        let path = layout::manifest_path(&head.pack);
        let wrong = |why: String| format!("{path} of {}: {why}", self.named()); // alloc: error path
        let Some(bytes) = self.get(&path, head.manifest_len)? else {
            return Err(wrong("its head is there and it is not".to_owned()));
        };
        let manifest = Manifest::named(&head.pack, &bytes, &B3).map_err(|refused| wrong(refused.to_string()))?;
        head.agrees(&manifest).map_err(|refused| wrong(refused.to_string()))?;
        Ok(manifest)
    }
}

/// A pack of a source, its manifest in memory: every key in it is one a miss can be
/// answered for.
struct Loaded {
    base: Base,
    pack: Id,
    manifest: Manifest,
}

enum SourceState {
    /// Not asked yet.
    New,
    Loaded(Arc<Vec<Loaded>>),
    /// It failed at that time, and is asked nothing more for [`PAUSE`].
    Failed(Instant),
}

/// What a peer said of a key, and when: a record it stated (true for good), or that it had
/// none (true for [`PAUSE`]: it may build it later).
type Said = (Instant, Option<Record>);

struct State {
    sources: Vec<SourceState>,
    /// When each peer last failed.
    peers: Vec<Option<Instant>>,
    /// What each peer said of each key asked.
    said: Vec<BTreeMap<Id, Said>>,
    /// The packs installed whole on a miss, by name.
    installed: Vec<Id>,
}

/// The pack sources and peers a job's misses are asked of, with the store what they give is
/// put into: a value the job carries (`JobInput::packs`), never read from the process by the
/// lookup, so a test asks its own sources of its own store.
pub(crate) struct Packs {
    config: Config,
    scope: String,
    trust: Trust,
    /// This machine's key, which signs what it asks of a peer.
    seed: Option<[u8; 32]>,
    /// The store a record fetched is put into: the daemon's, through the process's handle.
    store: &'static blaze_cas::Handle,
    state: blaze_core::lock::Lock<State>,
    /// Held while a source's heads and manifests are read, so one task reads them and the
    /// others that miss meanwhile wait for it (in the span `pack source load`) and then find
    /// them loaded. Never held while a record is read.
    loads: blaze_core::lock::Lock<()>,
}

/// The daemon's packs, made of packs.json once its store's handle is installed.
static PACKS: OnceLock<Option<Arc<Packs>>> = OnceLock::new();

/// What a source's failure said, until the daemon's loop shows it in a build's own output
/// ([`take_notices`]): once a failure, since a failed source is paused.
static NOTICES: crossbeam_queue::SegQueue<String> = crossbeam_queue::SegQueue::new();

/// The failures of sources since the last call, each a line for the build that ends next:
/// said in its output as a warning, so a source that is down is seen where the build is
/// watched and not only in the daemon's log.
pub(crate) fn take_notices() -> Vec<String> {
    let mut out = Vec::new(); // alloc: empty unless a source failed
    while let Some(notice) = NOTICES.pop() {
        out.push(notice);
    }
    out
}

/// The daemon's packs: none before its store's handle is installed (nothing to put into),
/// and none when the settings are absent, say `"prebuilt": false`, name no source and no
/// peer, or cannot be read (said once, as an error and a notice: a build with settings that
/// do not parse reads no pack, and says why).
fn daemon_packs() -> Option<Arc<Packs>> {
    let store = blaze_cas::Handle::installed()?;
    let packs = PACKS.get_or_init(|| {
        let made = Config::read().and_then(|config| match config {
            // `prebuilt: false`: nothing built elsewhere is asked for.
            Some(config) if config.prebuilt && (!config.sources.is_empty() || !config.peers.is_empty()) => {
                let seed = config.seed()?;
                Ok(Some(Arc::new(Packs::new(config, scope()?, seed, store)))) // alloc: once a daemon
            }
            _ => Ok(None),
        });
        made.unwrap_or_else(|why| {
            tracing::error!(target: "blaze", "no pack is read: {why}");
            NOTICES.push(format!("no pack is read: {why}")); // alloc: once a daemon
            None
        })
    });
    packs.as_ref().map(Arc::clone)
}

/// The packs a job of a request is given ([`super::work::JobInput::packs`]): the daemon's,
/// unless the request said `--no-prebuilt` ([`blaze_api::Cargo::prebuilt`] false), when
/// nothing built elsewhere is asked for and every miss is built here.
pub(crate) fn of_request(prebuilt: bool) -> Option<Arc<Packs>> {
    given(daemon_packs(), prebuilt)
}

/// `packs`, unless `prebuilt` is false: the one rule by which a request's switch turns its
/// job's sources off.
pub(crate) fn given(packs: Option<Arc<Packs>>, prebuilt: bool) -> Option<Arc<Packs>> {
    packs.filter(|_| prebuilt)
}

/// The job whose units' keys were last asked of the peers in one batch.
static ASKED_OF: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Whether `job` (any number that tells one job from the next) is another than the one
/// whose keys were last batched: true once a job, so its keys are worked out once.
pub(crate) fn first_ask_of(job: usize) -> bool {
    ASKED_OF.swap(job, std::sync::atomic::Ordering::Relaxed) != job
}

thread_local! {
    /// The lookup under way on this thread asks these packs (its job's, [`asked`]'s
    /// argument, for the lookup's extent alone), whether it is a sealed unit's (what it
    /// fetches goes into the machine's layer), and the hard error it met, if any.
    static ASKING: RefCell<Option<(Arc<Packs>, bool, Option<String>)>> = const { RefCell::new(None) };
}

struct Asking;

impl Drop for Asking {
    fn drop(&mut self) {
        ASKING.with(|asking| *asking.borrow_mut() = None);
    }
}

/// `look` run with `packs` (the job's) asked for every key it misses ([`missed`]): what it
/// gives, or the hard error a source caused (a record that is not what its manifest names).
///
/// `likely`: keys the build will ask for (every unit's record keys), asked of the peers in
/// batches first, once: what a peer lacks then costs no request of its own.
///
/// `sealed`: the lookup is a sealed unit's (`serve::sealed`, `unit::compile::sealed`): what
/// it fetches is installed into the machine's layer ([`blaze_cas::Handle::layer`]), never
/// the home, so every home on the machine finds it there and none fetches it again.
pub(crate) fn asked<T>(
    packs: &Arc<Packs>,
    likely: &[Key],
    sealed: bool,
    look: impl FnOnce() -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let _span = tracing::info_span!("pack fetch on miss").entered();
    // alloc: once a unit that missed, the keys not yet asked
    let ids: Vec<Id> = likely.iter().map(id_of).collect();
    packs.prime(&ids);
    ASKING.with(|asking| *asking.borrow_mut() = Some((Arc::clone(packs), sealed, None)));
    let ending = Asking;
    let found = look();
    let failed = ASKING.with(|asking| asking.borrow_mut().take().and_then(|(_, _, failed)| failed));
    drop(ending);
    match failed {
        Some(why) => Err(ApiError::new(why, None)),
        None => found,
    }
}

/// `key` is not in the job's store: whether it is now, fetched from a pack or a peer of the
/// packs [`asked`] runs with and checked. False outside [`asked`], when no source has it and
/// when a source failed (said in the log and in the build's output; the unit is built); a
/// record that is not what a trusted manifest names fails the lookup [`asked`] runs, naming
/// the source and the key.
pub(crate) fn missed(key: &Key) -> bool {
    // clone: the packs leave the cell before they are asked, so nothing borrows it across
    // the fetch's reads.
    let Some((packs, sealed)) =
        ASKING.with(|asking| asking.borrow().as_ref().map(|(packs, sealed, _)| (Arc::clone(packs), *sealed)))
    else {
        return false;
    };
    match packs.fetch(key, sealed) {
        Ok(kept) => kept,
        Err(why) => {
            ASKING.with(|asking| {
                if let Some((_, _, first)) = asking.borrow_mut().as_mut() {
                    first.get_or_insert(why);
                }
            });
            false
        }
    }
}

impl Packs {
    pub(crate) fn new(config: Config, scope: String, seed: Option<[u8; 32]>, store: &'static blaze_cas::Handle) -> Packs {
        let state = State {
            sources: config.sources.iter().map(|_| SourceState::New).collect(), // alloc: once a daemon
            peers: vec![None; config.peers.len()], // alloc: once a daemon
            said: config.peers.iter().map(|_| BTreeMap::new()).collect(), // alloc: once a daemon
            installed: Vec::new(), // alloc: empty, allocates nothing
        };
        let trust = config.trust();
        Packs {
            config,
            scope,
            trust,
            seed,
            store,
            state: blaze_core::lock::Lock::new("serve.packs.state", state),
            loads: blaze_core::lock::Lock::new("serve.packs.loads", ()),
        }
    }

    /// The packs of source `at`: each configured pin's head (signed by a trusted key, or it
    /// is refused: nothing a settings file names is taken on the user's word alone) and its
    /// manifest.
    fn load(&self, at: usize) -> Result<Vec<Loaded>, String> {
        let base = &self.config.sources[at];
        let mut loaded = Vec::new(); // alloc: once a source loaded
        for pin in &self.config.pins {
            let Some(head) = base.head(&self.scope, pin)? else { continue };
            head.vouched(&self.trust, &Ed25519, false).map_err(|refused| format!("{}: {refused}", base.named()))?; // alloc: error path
            let manifest = base.manifest(&head)?;
            loaded.push(Loaded { base: base.clone(), pack: head.pack, manifest }); // clone: once a pack loaded
        }
        Ok(loaded)
    }

    /// A source or a peer failed: said once, here (the daemon's log) and in the output of
    /// the build that ends next ([`take_notices`]), and paused.
    fn failed(what: &str, named: &str, why: &str) {
        // alloc: once a failure
        let said = format!(
            "{what} {named} failed and is asked nothing more for {}s; what it would have given is built here: {why}",
            PAUSE.as_secs()
        );
        tracing::error!(target: "blaze", "{said}");
        NOTICES.push(said);
    }

    /// One signed GET of `path` at peer `at`, its answer held to `most` bytes.
    fn peer_get(&self, at: usize, path: &str, range: Option<(u64, u64)>, most: u64) -> Result<(u16, Vec<u8>), String> {
        let Some(seed) = &self.seed else {
            return Err("this machine has no `key_file` to sign what it asks of a peer with".to_owned());
        };
        let (caller, _) = sign(seed, b"")?;
        let (_, signature) = sign(seed, &peer::request_message(path))?;
        let (caller, signature) = (peer::hex(&caller), peer::hex(&signature));
        let range = range.map(|(from, to)| format!("bytes={from}-{to}")); // alloc: once a request
        let mut headers = vec![(peer::CALLER, caller.as_str()), (peer::SIGNATURE, signature.as_str())]; // alloc: once a request
        if let Some(range) = &range {
            headers.push(("range", range.as_str()));
        }
        http_get(&format!("{}{path}", self.config.peers[at].url), &headers, most) // alloc: once a request
    }

    /// Asks peer `at` what it has of `keys` (at most `HAS_MOST`), and keeps what it says of
    /// each: a statement signed by its own key, or that it has none.
    fn ask_peer(&self, at: usize, keys: &[Id]) -> Result<(), String> {
        let (status, body) = self.peer_get(at, &peer::has_path(keys), None, peer::STATEMENTS_MOST)?;
        if status != 200 {
            return Err(format!("HTTP {status}")); // alloc: error path
        }
        let statements = peer::statements_of(&body).map_err(|refused| refused.to_string())?;
        // Its own key alone: a peer is trusted for what it signs, not for what another
        // trusted key would have.
        let trust = Trust::new(vec![self.config.peers[at].key]); // alloc: once an answer checked
        let now = Instant::now();
        let mut said: Vec<(Id, Said)> = keys.iter().map(|key| (*key, (now, None))).collect(); // alloc: once an answer
        for statement in &statements {
            let key = statement.record.key;
            let Some(slot) = said.iter_mut().find(|(it, _)| *it == key) else {
                return Err(format!("it answered about key {key}, which was not asked")); // alloc: error path
            };
            slot.1.1 = Some(statement.vouched(&key, &trust, &Ed25519).map_err(|refused| refused.to_string())?);
        }
        self.state.lock().said[at].extend(said);
        Ok(())
    }

    /// Whether what peer `at` said of `key` is still to be asked.
    fn unasked(state: &State, at: usize, key: &Id, now: Instant) -> bool {
        match state.said[at].get(key) {
            Some((_, Some(_))) => false,
            Some((when, None)) => now.duration_since(*when) >= PAUSE,
            None => true,
        }
    }

    /// Asks every peer, in batches, for those of `keys` it was not asked of lately. A peer
    /// that fails is said and paused; nothing here is an error of the build.
    fn prime(&self, keys: &[Id]) {
        let now = Instant::now();
        for at in 0..self.config.peers.len() {
            let unasked: Vec<Id> = {
                let state = self.state.lock();
                if state.peers[at].is_some_and(|failed| now.duration_since(failed) < PAUSE) {
                    continue;
                }
                keys.iter().filter(|key| Packs::unasked(&state, at, key, now)).copied().collect() // alloc: once a unit that missed
            };
            for batch in unasked.chunks(peer::HAS_MOST) {
                if let Err(why) = self.ask_peer(at, batch) {
                    Packs::failed("the peer", &shown(&self.config.peers[at].url), &why);
                    self.state.lock().peers[at] = Some(now);
                    break;
                }
            }
        }
    }

    /// The record of `len` bytes under `key` at peer `at`, read by ranges of at most
    /// `RANGE_MOST`. Unchecked: its caller holds it to its content hash.
    fn peer_record(&self, at: usize, key: &Id, len: u64) -> Result<Vec<u8>, String> {
        let path = peer::record_path(key);
        let mut bytes = Vec::with_capacity(len.min(peer::RANGE_MOST) as usize); // alloc: the record, handed to the check
        let mut from = 0;
        while from < len {
            let to = (from + peer::RANGE_MOST).min(len);
            let (status, part) = self.peer_get(at, &path, Some((from, to - 1)), to - from)?;
            if status != 206 || part.len() as u64 != to - from {
                // alloc: error path
                return Err(format!(
                    "asked for bytes {from} to {to} of the record of key {key}, it answered HTTP {status} with {} bytes",
                    part.len()
                ));
            }
            bytes.extend_from_slice(&part);
            from = to;
        }
        Ok(bytes)
    }

    /// The pack `it` of source `source`, imported whole through `handle` (the daemon's store's,
    /// or the machine's layer's for a sealed unit's lookup): every record checked against the manifest before it is put, one the store
    /// has left as it is. A pack is what its publisher selected to travel together (a pin's
    /// records), so the first miss it answers brings all of it, read in order a span at a
    /// time, and the misses after it are hits. Its records are pinned as every record of
    /// the standard library and bstd is, by the build that finds them
    /// (`serve::collect::keep`); the commit is the daemon's next.
    ///
    /// `Ok(false)`: the source could not be read (said, paused; the unit is built). The
    /// error is a hard one: a record that is not what the manifest names.
    fn install(&self, source: usize, it: &Loaded, handle: &'static blaze_cas::Handle) -> Result<bool, String> {
        let _span = tracing::info_span!(
            "pack install on miss",
            source = %it.base.named(),
            pack = %it.pack,
            records = it.manifest.records().len(),
            bytes = it.manifest.body_len(),
        )
        .entered();
        let named = |why: String| format!("the pack {} of {}: {why}", it.pack, it.base.named()); // alloc: error path
        let unread = |why: &str| -> Result<bool, String> {
            Packs::failed("the pack source", &it.base.named(), why);
            self.state.lock().sources[source] = SourceState::Failed(Instant::now());
            Ok(false)
        };
        let mut body = match Body::open(&it.base, &it.pack, it.manifest.body_len()) {
            Ok(body) => body,
            Err(why) => return unread(&why),
        };
        // clone: once a pack installed, its manifest for the import to own
        let mut import = Import::new(it.manifest.clone(), &admitted(true)).map_err(|refused| named(refused.to_string()))?;
        match blaze_pack::import::run(&mut import, &mut ThroughHandle(handle), &mut body, &B3) {
            Ok(_) => Ok(true),
            // What the source's reader or the store said: the source is at fault for the
            // first, and neither is a record that lied.
            Err(blaze_pack::Refused::Driver { what }) => unread(&what),
            Err(refused) => Err(named(refused.to_string())),
        }
    }

    /// The record of `key`, fetched, checked and put into the daemon's store, or for a sealed
    /// unit's lookup (`sealed`) into the machine's layer: whether the home finds it now (it
    /// reads the layer live, `blaze_cas::Store::read_live`). The error is a hard one: a lie,
    /// two trusted sayings that differ, or a sealed unit's fetch in a process with no layer.
    fn fetch(&self, key: &Key, sealed: bool) -> Result<bool, String> {
        let handle = self.store;
        if handle.has(key) {
            return Ok(true);
        }
        let into = match sealed {
            false => handle,
            true => blaze_cas::Handle::layer().ok_or_else(|| {
                // alloc: error path
                format!(
                    "the record of key {:x} is a sealed unit's, which is fetched into the machine's layer, and the daemon installed none",
                    key.0
                )
            })?,
        };
        let id = id_of(key);
        let now = Instant::now();
        // Each source's packs as they are loaded now; one not loaded (new, or failed longer
        // ago than the pause) is loaded first, by one task at a time.
        let stale = |state: &State, source: usize| match &state.sources[source] {
            SourceState::New => true,
            SourceState::Failed(at) => now.duration_since(*at) >= PAUSE,
            SourceState::Loaded(_) => false,
        };
        for source in 0..self.config.sources.len() {
            if stale(&self.state.lock(), source) {
                let _span = tracing::info_span!("pack source load", source = %self.config.sources[source].named()).entered();
                let _one = self.loads.lock();
                if stale(&self.state.lock(), source) {
                    let loaded = self.load(source);
                    self.state.lock().sources[source] = match loaded {
                        Ok(loaded) => SourceState::Loaded(Arc::new(loaded)),
                        Err(why) => {
                            Packs::failed("the pack source", &self.config.sources[source].named(), &why);
                            SourceState::Failed(now)
                        }
                    };
                }
            }
            let loaded = match &self.state.lock().sources[source] {
                SourceState::Loaded(loaded) => Arc::clone(loaded),
                _ => continue,
            };
            for it in loaded.iter().filter(|it| it.manifest.find(&id).is_some()) {
                // One task installs a pack; the others that miss a key of it wait here, in
                // the span, and then find it installed.
                let _span = tracing::info_span!("pack install wait", pack = %it.pack).entered();
                let _one = self.loads.lock();
                if self.state.lock().installed.contains(&it.pack) {
                    continue;
                }
                if self.install(source, it, into)? {
                    self.state.lock().installed.push(it.pack);
                }
            }
            if handle.has(key) {
                return Ok(true);
            }
        }
        // The peers, by key. A key the batch did not cover (a slot, an output a record
        // names) is asked alone.
        self.prime(&[id]);
        let mut wanted: Option<Record> = None;
        let mut from: Vec<usize> = Vec::new(); // alloc: once a miss a peer has, a peer or two
        for at in 0..self.config.peers.len() {
            let stated = self.state.lock().said[at].get(&id).and_then(|(_, record)| *record);
            let Some(record) = stated else { continue };
            if let Some(first) = wanted.filter(|first| *first != record) {
                // alloc: error path
                return Err(format!(
                    "two trusted peers say different things of the record of key {id}: content {} and, from {}, {} (two values of one key)",
                    first.content,
                    shown(&self.config.peers[at].url),
                    record.content
                ));
            }
            wanted = Some(record);
            from.push(at);
        }
        let Some(record) = wanted else { return Ok(false) };
        let Some(kind) = Kind::from_code(record.kind).filter(|kind| admitted(true).contains(&kind.code())) else {
            // alloc: error path
            return Err(format!(
                "the record of key {id} is of kind {}, which is not taken from a peer",
                record.kind
            ));
        };
        // The core decides who is asked and what is kept; this loop only reads and puts.
        let started = Instant::now();
        let mut fetch = Fetch::new(Duration::ZERO, &[record]);
        for at in 0..from.len() {
            fetch.add_source(Source(at as u32));
        }
        let named = |source: Source| shown(&self.config.peers[from[source.0 as usize]].url);
        // One record from a few peers: each is asked at most once, so the turns are few.
        for _ in 0..=2 * from.len() + 2 {
            for effect in fetch.take_effects() {
                match effect {
                    Effect::Ask { ask, source, .. } => {
                        let answer = match self.peer_record(from[source.0 as usize], &id, record.len) {
                            Ok(bytes) => Answer::Bytes(bytes),
                            Err(why) => {
                                Packs::failed("the peer", &named(source), &why);
                                Answer::Failed
                            }
                        };
                        fetch.answered(ask, answer, &B3);
                        fetch.advance_clock(started.elapsed());
                    }
                    Effect::Keep { bytes, .. } => {
                        into.put(*key, kind, &bytes).map_err(|mismatch| {
                            format!("the record of key {id} is refused: {}", stored::differs(&mismatch)) // alloc: error path
                        })?;
                    }
                    Effect::Dropped { source, .. } => self.state.lock().peers[from[source.0 as usize]] = Some(now),
                }
            }
            match fetch.outcome() {
                Outcome::Fetching => {}
                Outcome::Done { kept, .. } => return Ok(*kept == 1),
                Outcome::Lied { source, key, want, got } => {
                    // alloc: error path
                    return Err(format!(
                        "the peer {} sent, for the record of key {key}, bytes that hash to {got}; its statement says {want}. Nothing of it is kept, and the peer is asked nothing more",
                        named(*source)
                    ));
                }
            }
        }
        Err(format!("the fetch of the record of key {id} did not end within its turns")) // alloc: error path
    }
}

/// The daemon's store as an install on a miss puts into it, through the process's handle.
/// No pin and no flush of its own: the build that finds the records pins them, and the
/// commit is the daemon's next.
struct ThroughHandle(&'static blaze_cas::Handle);

impl blaze_pack::import::Store for ThroughHandle {
    fn look(&mut self, key: &Id) -> Result<Option<Id>, String> {
        match self.0.read(&key_of(key), |entry| Id(entry.content.0)) {
            Ok(content) => Ok(content),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }

    fn put(&mut self, record: blaze_pack::import::Verified<'_>) -> Result<(), String> {
        let Some(kind) = Kind::from_code(record.kind()) else {
            return Err(format!("kind {} is no kind of this store", record.kind())); // alloc: error path
        };
        match self.0.put(key_of(&record.key()), kind, record.bytes()) {
            Ok(_) => Ok(()),
            Err(mismatch) => Err(stored::differs(&mismatch)),
        }
    }

    fn pin(&mut self, _: &str, _: &[Id]) -> Result<(), String> {
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// How hard a pack's body is compressed as it is exported: written once, read by every
/// machine that installs it.
const ZSTD_LEVEL: i32 = 12;

/// `body`, a pack's whole body file at `raw`, written again as compressed spans at `frames`
/// with their index at `index`; the raw file is removed. A frame is one span of [`SPAN`]
/// bytes compressed alone, so a reader decompresses only the spans it reads.
pub(crate) fn compressed(raw: &Path, frames: &Path, index: &Path, len: u64) -> Result<(), String> {
    use std::io::{Read as _, Write as _};
    let failed = |error: std::io::Error| format!("compressing {}: {error}", raw.display()); // alloc: error path
    let mut from = std::io::BufReader::new(std::fs::File::open(raw).map_err(failed)?);
    let partial = frames.with_extension("partial");
    let mut to = std::io::BufWriter::new(std::fs::File::create(&partial).map_err(failed)?);
    let mut span = Vec::with_capacity(SPAN as usize); // alloc: one span, once an export
    let mut lens = Vec::new(); // alloc: a word a span, once an export
    let mut left = len;
    while left > 0 {
        span.clear();
        let take = left.min(SPAN);
        (&mut from).take(take).read_to_end(&mut span).map_err(failed)?;
        if span.len() as u64 != take {
            return Err(format!("{} ended before its {len} bytes", raw.display())); // alloc: error path
        }
        let frame = zstd::bulk::compress(&span, ZSTD_LEVEL).map_err(failed)?;
        to.write_all(&frame).map_err(failed)?;
        lens.push(frame.len() as u64);
        left -= take;
    }
    to.into_inner().map_err(|error| failed(error.into_error()))?.sync_all().map_err(failed)?;
    std::fs::rename(&partial, frames).map_err(failed)?;
    let spans = Spans::new(SPAN, len, &lens).map_err(|refused| refused.to_string())?;
    let partial = index.with_extension("partial");
    std::fs::write(&partial, spans.encode()).map_err(failed)?;
    std::fs::rename(&partial, index).map_err(failed)?;
    std::fs::remove_file(raw).map_err(failed)
}

/// A pack's body at a base, raw or compressed, read by ranges in order: the last span read
/// is held, so the records in it are one read.
pub(crate) struct Body {
    base: Base,
    pack: Id,
    len: u64,
    /// The index of its compressed spans, when it is compressed.
    spans: Option<Spans>,
    /// The bytes held, and where they start in the body.
    held: Vec<u8>,
    held_at: u64,
}

impl Body {
    /// The body of `pack`, of `len` bytes as its head says, at `base`: compressed when the
    /// base has a span index of it, else raw.
    pub(crate) fn open(base: &Base, pack: &Id, len: u64) -> Result<Body, String> {
        let spans = match base.get(&layout::spans_path(pack), blaze_pack::spans::INDEX_MOST)? {
            None => None,
            Some(bytes) => {
                // alloc: error path
                let wrong = |why: String| format!("{} of {}: {why}", layout::spans_path(pack), base.named());
                let spans = Spans::decode(&bytes).map_err(|refused| wrong(refused.to_string()))?;
                if spans.body_len() != len {
                    return Err(wrong(format!("it is of a body of {} bytes, and the pack's is {len}", spans.body_len())));
                }
                Some(spans)
            }
        };
        // clone: once a body opened
        Ok(Body { base: base.clone(), pack: *pack, len, spans, held: Vec::new(), held_at: 0 })
    }

    /// Holds the bytes of the body from `at` that a read of `len` there needs: the span
    /// they start in when it is compressed, else `len` and what follows up to a span.
    fn hold(&mut self, at: u64, len: u64) -> Result<(), String> {
        match &self.spans {
            None => {
                let want = len.max(SPAN).min(self.len.saturating_sub(at));
                self.held = self.base.range(&layout::body_path(&self.pack), at, want)?;
                self.held_at = at;
            }
            Some(spans) => {
                let (first, last) = spans.covering(at, len);
                self.held.clear();
                self.held_at = first * spans.span();
                let path = layout::frames_path(&self.pack);
                for index in first..last {
                    let (frame_at, frame_len, plain) = spans.frame(index);
                    let frame = self.base.range(&path, frame_at, frame_len)?;
                    // Decompressed to at most what the index says the span holds: a frame
                    // that would give more is refused by the decoder, not held.
                    let bytes = zstd::bulk::decompress(&frame, plain as usize)
                        .map_err(|error| format!("span {index} of {path} of {}: {error}", self.base.named()))?; // alloc: error path
                    if bytes.len() as u64 != plain {
                        // alloc: error path
                        return Err(format!(
                            "span {index} of {path} of {} holds {} bytes, and its index says {plain}",
                            self.base.named(),
                            bytes.len()
                        ));
                    }
                    self.held.extend_from_slice(&bytes);
                }
            }
        }
        Ok(())
    }
}

impl blaze_pack::import::Body for Body {
    fn read(&mut self, at: u64, len: u64, into: &mut Vec<u8>) -> Result<(), String> {
        into.clear();
        let within = at >= self.held_at && at + len <= self.held_at + self.held.len() as u64;
        if !within {
            self.hold(at, len)?;
        }
        // Fewer bytes than asked when the body ends before them: its reader says so.
        let from = (at.saturating_sub(self.held_at) as usize).min(self.held.len());
        let to = (from + len as usize).min(self.held.len());
        into.extend_from_slice(&self.held[from..to]);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
