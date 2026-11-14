//! `blaze cache push | pull | status | keys`, and the doctor's rows for the cache. Each command
//! reads the settings once ([`Settings::read`]); with no cache configured, or the cache
//! disabled, no bucket is made and nothing is contacted (the tests count it).
//!
//! - `blaze cache status`: what can leave this machine, said plainly; which settings are set
//!   and where from (never a value of a secret). Contacts nothing.
//! - `blaze cache pull <toolchain> <target> <cfg> [<namespace>]`: the bundle of that toolchain,
//!   target and cfg set (std, blaze-std) into the plain store, ahead of a first build. Each
//!   segment is checked once, by the store as it adopts it.
//! - `blaze cache push '<json>'`: the plain store's segments of a namespace, with their
//!   statements, index and root (and a bundle naming them all, when asked). The JSON is
//!   `{"toolchain": "<name>", "namespace": "public", "tier": "top", "bundle": {"target":
//!   "<triple>", "cfg": "<name>"}}`; `namespace` defaults to `public`, `tier` to `top`, and
//!   `bundle` is optional. Every setting it lacks is named at once, before anything is sent.
//! - `blaze cache keys public`: the public keys of the signing seed and the anchor seed that are
//!   set. `blaze cache keys sign '<json>'`: a key list signed by the anchor seed
//!   (`BLAZE_CACHE_ANCHOR_SEED`, read from the environment only), written to `out`. The JSON is
//!   `{"namespace": "public", "serial": N, "issued": unix_s, "keys": [{"key": "<64 hex>",
//!   "roles": ["build", "root"], "from": unix_s, "until": unix_s}], "out": "<path>"}`.

use std::path::Path;

use blaze_cas::plain::{Driven, Namespace};
use serde::Deserialize;

use crate::{
    bucket::{Bucket, Failure, drive},
    bundle::BundleId,
    name::{self, Toolchain},
    pull::Remote,
    push::{BundleOut, Plan, Push},
    settings::{Env, Leaves, Settings},
    trust::{KeyEntry, KeyList, PublicKey, Roles, Signer, Tier, Trusted},
};

/// The words this module answers after `blaze cache`.
pub const WORDS: [&str; 4] = ["push", "pull", "status", "keys"];

/// The usage, for the CLI's.
pub const USAGE: &str = "blaze cache status | blaze cache pull <toolchain> <target> <cfg> [<namespace>] | blaze cache push '{\"toolchain\": ..., \"namespace\": \"public\", \"tier\": \"top\", \"bundle\": {\"target\": ..., \"cfg\": ...}}' | blaze cache keys public | blaze cache keys sign '<json>'";

/// Makes the bucket a command talks to: the HTTP driver, or a test's fake.
pub type Connect<'c> = &'c mut dyn FnMut(&Settings) -> Result<Box<dyn Bucket>, Failure>;

/// `blaze cache <words>` over HTTPS.
pub fn main(root: &Path, env: Env<'_>, words: &[&str], now_s: u64) -> Result<String, String> {
    run(root, env, words, now_s, &mut |settings| Ok(Box::new(crate::http::HttpBucket::new(settings)?) as Box<dyn Bucket>))
}

/// `blaze cache <words>`, the bucket made by `connect` (called only when the cache contacts
/// a bucket and the command needs one).
pub fn run(root: &Path, env: Env<'_>, words: &[&str], now_s: u64, connect: Connect<'_>) -> Result<String, String> {
    match words {
        ["status"] => Settings::read(root, env).map(|it| status(&it)),
        ["pull", toolchain, target, cfg] => pull(root, env, toolchain, target, cfg, "public", connect),
        ["pull", toolchain, target, cfg, namespace] => pull(root, env, toolchain, target, cfg, namespace, connect),
        ["push", json] => push(root, env, json, now_s, connect),
        ["keys", "public"] => keys_public(root, env),
        ["keys", "sign", json] => keys_sign(env, json),
        _ => Err(USAGE.to_owned()),
    }
}

/// What `status` says.
pub fn status(settings: &Settings) -> String {
    let mut out = String::new();
    match settings.leaves() {
        Leaves::Nothing if !settings.enabled => out.push_str("the cache is disabled: blaze contacts nothing, and nothing leaves this machine\n"),
        Leaves::Nothing => out.push_str("no cache is configured: blaze contacts nothing, and nothing leaves this machine\n"),
        Leaves::Reads => out.push_str("reads only: blaze asks the bucket for objects by name and digest (index, statements, segments); no source, record or name of this workspace's is sent, and nothing is written\n"),
        Leaves::Pushes => out.push_str("reads and pushes: `blaze cache push` sends this store's segments of the namespace it is given, with statements signed by this machine's key; nothing is pushed by a build\n"),
    }
    if let Some(bucket) = settings.contacts() {
        let said = |it: &Option<String>| it.clone().unwrap_or_else(|| "not set".to_owned());
        out.push_str(&format!("bucket: {} at {}; anonymous reads from {}; compare-and-swap {}\n", said(&bucket.name), said(&bucket.endpoint), said(&bucket.public_url), if bucket.cas { "on" } else { "off" }));
    }
    for ns in &settings.namespaces {
        out.push_str(&format!("namespace {}: {} anchor(s)", ns.name, ns.anchors.len()));
        for key in &ns.anchors {
            out.push_str(&format!(" {key}"));
        }
        out.push('\n');
    }
    let set = |it: bool| if it { "set" } else { "not set" };
    out.push_str(&format!("credentials {}; signing key {}", set(settings.credentials.is_some()), set(settings.signer.is_some())));
    if let Some(signer) = &settings.signer {
        out.push_str(&format!(" (public {})", signer.public()));
    }
    out.push('\n');
    for (what, from) in &settings.from {
        out.push_str(&format!("{what}: from the {from}\n"));
    }
    out
}

fn trusted(settings: &Settings, toolchain: Toolchain, namespace: Namespace) -> Result<Trusted, String> {
    let Some(ns) = settings.namespace(namespace) else {
        return Err(Failure::NotConfigured { missing: vec![format!("namespace {} with its anchors (remote.json `namespaces`, or BLAZE_CACHE_PUBLIC_ANCHORS)", name::space(namespace).unwrap_or_default())] }.to_string());
    };
    if ns.anchors.is_empty() {
        return Err(format!("namespace {}: no anchor key is set, so nothing of it can be trusted", ns.name));
    }
    let mut trusted = Trusted::new(toolchain);
    trusted.anchor(namespace, ns.anchors.clone());
    Ok(trusted)
}

fn contacts_nothing(settings: &Settings) -> String {
    format!("{}nothing was contacted", status(settings))
}

fn pull(root: &Path, env: Env<'_>, toolchain: &str, target: &str, cfg: &str, namespace: &str, connect: Connect<'_>) -> Result<String, String> {
    let settings = Settings::read(root, env)?;
    if settings.contacts().is_none() {
        return Err(contacts_nothing(&settings));
    }
    let id = BundleId::new(toolchain, target, cfg).map_err(|it| it.to_string())?;
    let ns = name::namespace_named(namespace).map_err(|it| it.to_string())?;
    let trusted = trusted(&settings, id.toolchain.clone(), ns)?;
    let store_settings = blaze_cas::plain::Settings::read(root)?;
    let mut driven = Driven::open(root, &store_settings)?;
    let mut bucket = connect(&settings).map_err(|it| it.to_string())?;
    let mut remote = Remote::new(&mut *bucket, &settings.root, trusted);
    let (bundle, segments) = remote.bundle_ahead(ns, &id).map_err(|it| it.to_string())?;
    // The trust as it stands after the manifest (the key list taken once); the store checks
    // each segment's statement under it, and its bytes, once each.
    let verifier = remote.trusted().clone();
    let ids = driven.pull(&mut remote, &verifier, ns, segments)?;
    let counts = remote.report.counts;
    Ok(format!(
        "pulled bundle {}/{}/{}: {} segment(s) adopted, {} requests, {} bytes received\n",
        bundle.id.toolchain.as_str(),
        bundle.id.target,
        bundle.id.cfg,
        ids.len(),
        counts.requests,
        counts.received
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PushInput {
    toolchain: String,
    #[serde(default = "public")]
    namespace: String,
    #[serde(default = "top")]
    tier: Tier,
    bundle: Option<BundleInput>,
}

fn public() -> String {
    "public".to_owned()
}

fn top() -> Tier {
    Tier::Top
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleInput {
    target: String,
    cfg: String,
}

fn push(root: &Path, env: Env<'_>, json: &str, now_s: u64, connect: Connect<'_>) -> Result<String, String> {
    let input: PushInput = serde_json::from_str(json).map_err(|e| format!("blaze cache push: {e}: {USAGE}"))?;
    let settings = Settings::read(root, env)?;
    if settings.contacts().is_none() {
        return Err(contacts_nothing(&settings));
    }
    let mut missing = Vec::new();
    let bucket_settings = settings.contacts().expect("just seen");
    if bucket_settings.endpoint.is_none() {
        missing.push("bucket.endpoint (BLAZE_CACHE_ENDPOINT)".to_owned());
    }
    if bucket_settings.name.is_none() {
        missing.push("bucket.name (BLAZE_CACHE_BUCKET)".to_owned());
    }
    if settings.credentials.is_none() {
        missing.push("the access key (BLAZE_CACHE_ACCESS_KEY_ID and BLAZE_CACHE_SECRET_ACCESS_KEY)".to_owned());
    }
    if settings.signer.is_none() {
        missing.push("the signing seed (BLAZE_CACHE_SIGNING_SEED)".to_owned());
    }
    if settings.key_list.is_none() {
        missing.push("the signed key list (BLAZE_CACHE_KEY_LIST)".to_owned());
    }
    if !missing.is_empty() {
        return Err(Failure::NotConfigured { missing }.to_string());
    }
    let toolchain = Toolchain::new(&input.toolchain).map_err(|it| it.to_string())?;
    let ns = name::namespace_named(&input.namespace).map_err(|it| it.to_string())?;
    let trusted = trusted(&settings, toolchain.clone(), ns)?;
    let key_list_path = settings.key_list.as_ref().expect("checked");
    let key_list = std::fs::read(key_list_path).map_err(|e| format!("{}: {e}", key_list_path.display()))?;

    let store_settings = blaze_cas::plain::Settings::read(root)?;
    let mut driven = Driven::open(root, &store_settings)?;
    let mut ids = Vec::new();
    let mut other = 0usize;
    let mut segments: Vec<&[u8]> = Vec::new();
    let mut digests = Vec::new();
    for (id, _) in driven.core().segments() {
        let (seg, bytes) = driven.core().segment(id).ok_or_else(|| format!("segment {id}: named by the store's generation and not found in it"))?;
        if seg.namespace != ns {
            other += 1;
            continue;
        }
        let bytes = bytes.ok_or_else(|| format!("segment {id} ({}): not mapped by this store, so its bytes cannot be pushed", blaze_format::hex::encode(&seg.digest.0)))?;
        ids.push(id);
        digests.push(seg.digest);
        segments.push(bytes);
    }
    if segments.is_empty() {
        return Err(format!(
            "the store holds no segment of namespace {} ({other} of other namespaces stay here); nothing pushed. The plain store writes namespace local until it is given the namespace to write",
            input.namespace
        ));
    }
    let bundles = match &input.bundle {
        None => Vec::new(),
        Some(it) => vec![BundleOut { id: BundleId::new(&input.toolchain, &it.target, &it.cfg).map_err(|it| it.to_string())?, segments: digests }],
    };
    let signer = settings.signer.as_ref().expect("checked");
    let plan = Plan {
        namespace: ns,
        root_name: settings.root.clone(),
        tier: input.tier,
        signer,
        key_list,
        trusted,
        segments,
        bundles,
        rebuilt: Vec::new(),
        sample_per_1024: settings.sample_per_1024,
        cas: bucket_settings.cas,
        now_s,
    };
    let mut machine = Push::new(plan).map_err(|it| it.to_string())?;
    let mut bucket = connect(&settings).map_err(|it| it.to_string())?;
    let report = drive(&mut machine, &mut *bucket, &mut |()| Ok(())).map_err(|it| it.to_string())?;
    let counts = bucket.counts();
    drop(machine);
    driven.publish(ids)?;
    Ok(format!(
        "pushed {} records in {} segment(s) to {} ({} put, {} already there, {} bytes); {} shard(s), {} merge(s), root generation {} after {} attempt(s), {} key(s) sampled against the bucket; {} requests, {} bytes sent; {other} segment(s) of other namespaces stay here\n",
        report.records,
        report.segments_put + report.segments_present,
        input.namespace,
        report.segments_put,
        report.segments_present,
        report.bytes_put,
        report.shards_put,
        report.merges,
        report.generation,
        report.tries,
        report.sampled,
        counts.requests,
        counts.sent
    ))
}

fn anchor_seed(env: Env<'_>) -> Result<Option<Signer>, String> {
    match env("BLAZE_CACHE_ANCHOR_SEED") {
        None => Ok(None),
        Some(it) if it.is_empty() => Ok(None),
        Some(it) => {
            let text = it.into_string().map_err(|_| "BLAZE_CACHE_ANCHOR_SEED is not UTF-8".to_owned())?;
            Signer::from_hex("the anchor seed (BLAZE_CACHE_ANCHOR_SEED)", &text).map(Some)
        }
    }
}

fn keys_public(root: &Path, env: Env<'_>) -> Result<String, String> {
    let settings = Settings::read(root, env)?;
    let anchor = anchor_seed(env)?;
    if settings.signer.is_none() && anchor.is_none() {
        return Err(Failure::NotConfigured { missing: vec!["the signing seed (BLAZE_CACHE_SIGNING_SEED) or the anchor seed (BLAZE_CACHE_ANCHOR_SEED)".to_owned()] }.to_string());
    }
    let mut out = String::new();
    if let Some(signer) = &settings.signer {
        out.push_str(&format!("signing key (a builder's, for the key list): {}\n", signer.public()));
    }
    if let Some(anchor) = &anchor {
        out.push_str(&format!("anchor key (for remote.json `anchors` or BLAZE_CACHE_PUBLIC_ANCHORS): {}\n", anchor.public()));
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignInput {
    namespace: String,
    serial: u64,
    issued: u64,
    keys: Vec<KeyInput>,
    out: std::path::PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyInput {
    key: String,
    roles: Vec<String>,
    from: u64,
    until: u64,
    #[serde(default)]
    revoked: bool,
}

fn keys_sign(env: Env<'_>, json: &str) -> Result<String, String> {
    let input: SignInput = serde_json::from_str(json).map_err(|e| format!("blaze cache keys sign: {e}: {USAGE}"))?;
    let Some(anchor) = anchor_seed(env)? else {
        return Err(Failure::NotConfigured { missing: vec!["the anchor seed (BLAZE_CACHE_ANCHOR_SEED)".to_owned()] }.to_string());
    };
    let namespace = name::namespace_named(&input.namespace).map_err(|it| it.to_string())?;
    if input.keys.len() > crate::trust::KEYS_MOST {
        return Err(format!("{} keys: a key list names at most {}", input.keys.len(), crate::trust::KEYS_MOST));
    }
    let mut keys = Vec::with_capacity(input.keys.len());
    for it in &input.keys {
        let key = PublicKey::from_hex(&it.key).ok_or_else(|| format!("key {:?}: not 64 hex digits (an Ed25519 public key)", it.key))?;
        let mut roles = Roles(0);
        for role in &it.roles {
            roles = Roles(
                roles.0
                    | match role.as_str() {
                        "build" => Roles::BUILD.0,
                        "root" => Roles::ROOT.0,
                        other => return Err(format!("key {key}: role {other:?} is not `build` or `root`")),
                    },
            );
        }
        if it.from > it.until {
            return Err(format!("key {key}: valid from {} after until {}", it.from, it.until));
        }
        keys.push(KeyEntry { key, roles, revoked: it.revoked, from: it.from, until: it.until });
    }
    let list = KeyList { namespace, serial: input.serial, issued: input.issued, keys };
    let bytes = list.encode(&anchor);
    std::fs::write(&input.out, &bytes).map_err(|e| format!("{}: {e}", input.out.display()))?;
    Ok(format!("key list serial {} of namespace {} ({} keys, {} bytes) signed by anchor {}, written to {}\n", list.serial, input.namespace, list.keys.len(), bytes.len(), anchor.public(), input.out.display()))
}

/// A doctor's verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Ok,
    /// Not configured, and nothing needs it.
    Absent,
    Warn,
    Fault,
}

/// One row of `blaze doctor` about the cache.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub what: &'static str,
    pub verdict: Verdict,
    pub said: String,
    pub fix: Option<String>,
}

/// A key that expires within this many seconds is a warning.
pub const EXPIRES_SOON_S: u64 = 30 * 86_400;

/// The doctor's rows: the settings, what can leave the machine, the anchors, and the key
/// list's validity for this machine's signing key. Contacts nothing.
pub fn doctor(root: &Path, env: Env<'_>, now_s: u64) -> Vec<Row> {
    let row = |what, verdict, said: String, fix: Option<&str>| Row { what, verdict, said, fix: fix.map(str::to_owned) };
    let settings = match Settings::read(root, env) {
        Ok(it) => it,
        Err(why) => return vec![row("cache settings", Verdict::Fault, why, Some("fix remote.json or remote-credentials.json, or the BLAZE_CACHE_* variables"))],
    };
    let mut rows = Vec::new();
    let first_line = status(&settings).lines().next().unwrap_or_default().to_owned();
    rows.push(row("cache", if settings.contacts().is_some() { Verdict::Ok } else { Verdict::Absent }, first_line, None));
    if settings.contacts().is_none() {
        return rows;
    }
    if settings.namespaces.is_empty() {
        rows.push(row("cache anchors", Verdict::Fault, "no namespace has anchors, so every pull is refused".to_owned(), Some("set BLAZE_CACHE_PUBLIC_ANCHORS or remote.json `namespaces`")));
    }
    for ns in &settings.namespaces {
        if ns.anchors.is_empty() {
            rows.push(row("cache anchors", Verdict::Fault, format!("namespace {}: no anchor", ns.name), Some("add the anchor's public key (`blaze cache keys public` where the anchor seed is)")));
        }
    }
    let (Some(path), Some(signer)) = (&settings.key_list, &settings.signer) else {
        if settings.signer.is_some() != settings.key_list.is_some() {
            rows.push(row("cache signing", Verdict::Fault, "a signing seed and a key list go together, and only one is set".to_owned(), Some("set both BLAZE_CACHE_SIGNING_SEED and BLAZE_CACHE_KEY_LIST, or neither")));
        }
        return rows;
    };
    let bytes = match std::fs::read(path) {
        Ok(it) => it,
        Err(e) => {
            rows.push(row("cache key list", Verdict::Fault, format!("{}: {e}", path.display()), None));
            return rows;
        }
    };
    let decoded = settings.namespaces.iter().find_map(|ns| KeyList::decode(&path.display().to_string(), &bytes, ns.namespace, &ns.anchors).ok().map(|list| (ns, list)));
    let Some((ns, list)) = decoded else {
        rows.push(row("cache key list", Verdict::Fault, format!("{}: not signed by an anchor of any namespace set", path.display()), Some("sign it again with `blaze cache keys sign`")));
        return rows;
    };
    let me = signer.public();
    let Some(entry) = list.keys.iter().find(|it| it.key == me) else {
        rows.push(row("cache key list", Verdict::Fault, format!("namespace {}: serial {} does not list this machine's key {me}", ns.name, list.serial), Some("add the key and sign a new serial")));
        return rows;
    };
    let (verdict, said) = if entry.revoked {
        (Verdict::Fault, format!("key {me} is revoked in serial {}", list.serial))
    } else if now_s > entry.until {
        (Verdict::Fault, format!("key {me} expired at {} (unix s); it is {now_s}", entry.until))
    } else if now_s < entry.from {
        (Verdict::Fault, format!("key {me} is valid only from {} (unix s)", entry.from))
    } else if entry.until - now_s < EXPIRES_SOON_S {
        (Verdict::Warn, format!("key {me} expires in {} days", (entry.until - now_s) / 86_400))
    } else if !entry.roles.has(Roles::BUILD) || !entry.roles.has(Roles::ROOT) {
        (Verdict::Fault, format!("key {me} lacks the build or root role, both of which a push needs"))
    } else {
        (Verdict::Ok, format!("key {me}: valid until {} (unix s), serial {} of namespace {}", entry.until, list.serial, ns.name))
    };
    let fix = (verdict != Verdict::Ok).then_some("sign a new key list serial with `blaze cache keys sign`");
    rows.push(row("cache key list", verdict, said, fix));
    rows
}
