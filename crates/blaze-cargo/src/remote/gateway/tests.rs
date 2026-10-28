//! The gateway client against an in-process fake of API v0 (tools/remote-cache/API-v0.md)
//! that keeps its rules: the one 404, 401 and 403, 412 on a condition, 422 on a hash, 413,
//! ranges and 416, uploads in parts, and the 429 and 5xx it is told to answer with.

use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    path::Path,
    sync::{Arc, Mutex},
};

use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde_json::Value;

use super::{super::*, *};
use crate::remote::{
    STORE_FORMAT,
    tests::{held, scratch, taking_unsigned},
};

const PRIVATE: &str = "rcache-test";
const PUBLIC: &str = "blaze";
const WRITER: &str = "write-token-0123456789";
const READER: &str = "read-token-0123456789";
const OTHER: &str = "other-tenant-token-0123456789";

#[derive(Default)]
struct State {
    /// The tenants, and whether each is public.
    tenants: HashMap<String, bool>,
    /// Each token's tenant and whether it may write.
    tokens: HashMap<String, (String, bool)>,
    objects: HashMap<(String, String), Stored>,
    uploads: HashMap<String, Upload>,
    next_upload: u64,
    /// How many of the next requests are answered 429, and how many 503.
    limited: u32,
    failing: u32,
    /// Each part sent has a byte changed, as a line that damages it would.
    damage_parts: bool,
    /// What was answered, by kind.
    ranges: u32,
    parts: u32,
    aborted: u32,
    requests: u32,
}

struct Stored {
    bytes: Vec<u8>,
    etag: String,
}

struct Upload {
    tenant: String,
    name: String,
    parts: BTreeMap<u64, Vec<u8>>,
}

/// What a name may hold: bytes that hash to it, a pack compressed (whose hash comes in
/// `x-blaze-sha256`), or a list.
enum Kind {
    Hashed(String),
    Packed,
    List,
}

fn is_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'))
}

fn kind(name: &str) -> Option<Kind> {
    let (dir, rest) = name.split_once('/')?;
    match dir {
        "packs" => match rest.strip_suffix(".pack.zst") {
            Some(hash) if is_hex(hash) => Some(Kind::Packed),
            _ => {
                rest.strip_suffix(".pack").filter(|it| is_hex(it)).map(|it| Kind::Hashed(it.into()))
            }
        },
        "keys" | "units" => is_hex(rest).then(|| Kind::Hashed(rest.into())),
        "manifests" | "deps" => {
            let ok = (1..=128).contains(&rest.len())
                && rest.bytes().all(|it| it.is_ascii_alphanumeric() || b"._-".contains(&it));
            ok.then_some(Kind::List)
        }
        _ => None,
    }
}

fn sha(bytes: &[u8]) -> String {
    Digest::of(bytes).to_string()
}

fn plain(status: StatusCode, text: &str) -> Response<Full<Bytes>> {
    let mut answer = Response::new(Full::new(Bytes::from(format!("{text}\n"))));
    *answer.status_mut() = status;
    answer
}

fn etagged(etag: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({ "etag": etag }).to_string();
    let mut answer = Response::new(Full::new(Bytes::from(body)));
    answer.headers_mut().insert(header::ETAG, etag.parse().unwrap());
    answer
}

fn not_found() -> Response<Full<Bytes>> {
    plain(StatusCode::NOT_FOUND, "not found")
}

async fn answer(state: &Mutex<State>, request: Request<Incoming>) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    let mut state = state.lock().unwrap();
    state.requests += 1;
    if state.limited > 0 {
        state.limited -= 1;
        let mut answer = plain(StatusCode::TOO_MANY_REQUESTS, "rate limited");
        answer.headers_mut().insert(header::RETRY_AFTER, "0".parse().unwrap());
        return answer;
    }
    if state.failing > 0 {
        state.failing -= 1;
        return plain(StatusCode::SERVICE_UNAVAILABLE, "try again");
    }
    let header = |name: &str| parts.headers.get(name).and_then(|it| it.to_str().ok());
    // Who asks is settled before anything about what is asked for is looked up.
    let caller = match header("authorization").map(|it| it.strip_prefix("Bearer ")) {
        None => None,
        Some(Some(token)) => match state.tokens.get(token) {
            Some(it) => Some(it.clone()),
            None => return plain(StatusCode::UNAUTHORIZED, "unknown token"),
        },
        Some(None) => return plain(StatusCode::UNAUTHORIZED, "not a bearer token"),
    };
    let path = parts.uri.path().to_owned();
    let Some((tenant, name)) = path
        .strip_prefix("/t/")
        .and_then(|it| it.split_once('/'))
        .and_then(|(tenant, rest)| Some((tenant.to_owned(), rest.strip_prefix("v3/")?.to_owned())))
    else {
        return not_found();
    };
    let query: HashMap<String, String> =
        url::form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes())
            .into_owned()
            .collect();
    let public = state.tenants.get(&tenant).copied();
    let theirs = caller.as_ref().is_some_and(|it| it.0 == tenant);
    let method = parts.method.clone();
    if method == hyper::Method::GET || method == hyper::Method::HEAD {
        let readable = public == Some(true) || (public.is_some() && theirs);
        let stored = state.objects.get(&(tenant.clone(), name.clone())).filter(|_| readable);
        let Some((stored, etag)) = stored.map(|it| (it.bytes.clone(), it.etag.clone())) else {
            return not_found();
        };
        let total = stored.len() as u64;
        let range = header("range").and_then(|it| it.strip_prefix("bytes=")).map(|it| {
            let (from, to) = it.split_once('-').unwrap();
            let from: u64 = from.parse().unwrap();
            let to = if to.is_empty() { total.saturating_sub(1) } else { to.parse().unwrap() };
            (from, to.min(total.saturating_sub(1)))
        });
        let (status, bytes) = match range {
            Some((from, _)) if from >= total => {
                let mut answer = plain(StatusCode::RANGE_NOT_SATISFIABLE, "range");
                answer
                    .headers_mut()
                    .insert(header::CONTENT_RANGE, format!("bytes */{total}").parse().unwrap());
                return answer;
            }
            Some((from, to)) => {
                state.ranges += 1;
                (StatusCode::PARTIAL_CONTENT, stored[from as usize..=to as usize].to_vec())
            }
            None => (StatusCode::OK, stored),
        };
        let mut answer = Response::new(Full::new(Bytes::from(bytes)));
        *answer.status_mut() = status;
        let headers = answer.headers_mut();
        headers.insert(header::ETAG, etag.parse().unwrap());
        headers.insert("x-blaze-uploaded", "1700000000".parse().unwrap());
        if let Some((from, to)) = range {
            let range = format!("bytes {from}-{to}/{total}");
            headers.insert(header::CONTENT_RANGE, range.parse().unwrap());
        }
        return answer;
    }
    // A write.
    let Some((_, may_write)) = caller else {
        return plain(StatusCode::UNAUTHORIZED, "a token is needed to write");
    };
    if !theirs || public.is_none() {
        return not_found();
    }
    if !may_write {
        return plain(StatusCode::FORBIDDEN, "a read token does not write");
    }
    let Some(kind) = kind(&name) else { return not_found() };
    let key = (tenant.clone(), name.clone());
    let sent_sha = header("x-blaze-sha256").map(str::to_owned);
    // Each body is hashed once, under the lock every request takes: a part and a whole pack
    // are tens of MiB, and hashing one again for each check made the completion of an upload
    // outlast the client's deadline in a build without optimized SHA-256.
    let hash_holds = |kind: &Kind, sha: &str| match kind {
        Kind::Hashed(hash) => sha == hash,
        Kind::Packed => sent_sha.as_deref() == Some(sha),
        Kind::List => true,
    };
    match (method.as_str(), query.get("upload"), query.contains_key("uploads")) {
        ("POST", None, true) => {
            if !name.starts_with("packs/") && !name.starts_with("units/") {
                return plain(StatusCode::METHOD_NOT_ALLOWED, "no upload of that");
            }
            state.next_upload += 1;
            let id = format!("up/{}", state.next_upload);
            state.uploads.insert(id.clone(), Upload { tenant, name, parts: BTreeMap::new() });
            let body = serde_json::json!({ "upload": id }).to_string();
            Response::new(Full::new(Bytes::from(body)))
        }
        ("PUT", Some(id), false) => {
            let damage = state.damage_parts;
            let Some(upload) = state.uploads.get_mut(id).filter(|it| it.name == name) else {
                return not_found();
            };
            if body.len() as u64 > PUT_MAX {
                return plain(StatusCode::PAYLOAD_TOO_LARGE, "too large");
            }
            let number: u64 = query["part"].parse().unwrap();
            let mut bytes = body.to_vec();
            if damage {
                bytes[0] ^= 1;
            }
            let etag = format!("\"part-{number}-{}\"", &sha(&bytes)[..8]);
            upload.parts.insert(number, bytes);
            state.parts += 1;
            etagged(&etag)
        }
        ("POST", Some(id), false) => {
            let Some(upload) = state.uploads.remove(id).filter(|it| it.name == name) else {
                return not_found();
            };
            let asked: Value = serde_json::from_slice(&body).unwrap();
            let listed = asked["parts"].as_array().unwrap();
            let mut whole = Vec::new();
            for (at, part) in listed.iter().enumerate() {
                let number = part["part"].as_u64().unwrap();
                let Some(bytes) = upload.parts.get(&number) else {
                    return plain(StatusCode::BAD_REQUEST, "no such part");
                };
                let last = at + 1 == listed.len();
                if !last && (bytes.len() < 5 << 20 || bytes.len() != upload.parts[&1].len()) {
                    return plain(StatusCode::BAD_REQUEST, "parts of unequal size");
                }
                whole.extend_from_slice(bytes);
            }
            let said = asked["sha256"].as_str().unwrap_or_default();
            let whole_sha = sha(&whole);
            if said != whole_sha || !hash_holds(&kind, &whole_sha) {
                return plain(StatusCode::UNPROCESSABLE_ENTITY, "hash mismatch");
            }
            let etag = format!("\"{whole_sha}\"");
            let key = (upload.tenant, upload.name);
            state.objects.insert(key, Stored { bytes: whole, etag: etag.clone() });
            etagged(&etag)
        }
        ("DELETE", Some(id), false) => {
            state.uploads.remove(id);
            state.aborted += 1;
            plain(StatusCode::NO_CONTENT, "")
        }
        ("PUT", None, false) => {
            if body.len() as u64 > PUT_MAX {
                return plain(StatusCode::PAYLOAD_TOO_LARGE, "too large");
            }
            let body_sha = sha(&body);
            if !hash_holds(&kind, &body_sha) {
                return plain(StatusCode::UNPROCESSABLE_ENTITY, "hash mismatch");
            }
            let now = state.objects.get(&key).map(|it| it.etag.clone());
            let holds = match (header("if-match"), header("if-none-match")) {
                (Some(tag), _) => now.as_deref() == Some(tag),
                (None, Some("*")) => now.is_none(),
                _ => true,
            };
            if !holds {
                return plain(StatusCode::PRECONDITION_FAILED, "precondition failed");
            }
            let etag = format!("\"{body_sha}\"");
            state.objects.insert(key, Stored { bytes: body.to_vec(), etag: etag.clone() });
            etagged(&etag)
        }
        _ => plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

/// A fake gateway on a port of this machine, served on the runtime it is started on.
struct Fake {
    port: u16,
    state: Arc<Mutex<State>>,
}

impl Fake {
    async fn start() -> Fake {
        let mut state = State::default();
        state.tenants.insert(PRIVATE.into(), false);
        state.tenants.insert(PUBLIC.into(), true);
        state.tokens.insert(WRITER.into(), (PRIVATE.into(), true));
        state.tokens.insert(READER.into(), (PRIVATE.into(), false));
        state.tokens.insert(OTHER.into(), (PUBLIC.into(), true));
        let state = Arc::new(Mutex::new(state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let state = served.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(answer(&state, request).await) }
                    });
                    let connection = hyper::server::conn::http1::Builder::new();
                    _ = connection.serve_connection(TokioIo::new(stream), service).await;
                });
            }
        });
        Fake { port, state }
    }

    fn spec(&self, tenant: &str) -> String {
        format!("blaze+http://127.0.0.1:{}/t/{tenant}/v3", self.port)
    }

    fn remote(&self, tenant: &str, token: Option<&str>, kept_in: &Path) -> Remote {
        Remote::at_gateway(&self.spec(tenant), token, kept_in, taking_unsigned())
    }

    fn with<T>(&self, look: impl FnOnce(&mut State) -> T) -> T {
        look(&mut self.state.lock().unwrap())
    }

    fn stored(&self, tenant: &str, name: &str) -> Option<Vec<u8>> {
        self.with(|it| it.objects.get(&(tenant.into(), name.into())).map(|it| it.bytes.clone()))
    }
}

/// Runs `test` on a runtime of its own with a fake gateway served on it.
fn with_fake<F: Future<Output = ()>>(test: impl FnOnce(Arc<Fake>) -> F) {
    runtime().unwrap().block_on(async {
        let fake = Arc::new(Fake::start().await);
        test(fake).await
    });
}

/// A pack of this store's format, `len` bytes that do not compress.
fn pack_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut pack = [&b"blazepk"[..], &[STORE_FORMAT as u8]].concat();
    let mut state = seed | 1;
    while pack.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        pack.extend_from_slice(&state.to_le_bytes());
    }
    pack.truncate(len);
    pack
}

#[test]
fn an_address_names_the_tenant_and_where_the_token_is() {
    let spec = Spec::parse("blaze+https://cache.example/t/rcache-test/v3").unwrap();
    assert_eq!(spec.base, "https://cache.example/t/rcache-test/v3");
    assert_eq!((spec.tenant.as_str(), spec.token_var.as_str()), ("rcache-test", TOKEN_VAR));
    let spec = Spec::parse("blaze+http://127.0.0.1:9/t/blaze/v3/?token=env:CI_TOKEN").unwrap();
    assert_eq!(spec.base, "http://127.0.0.1:9/t/blaze/v3");
    assert_eq!(spec.token_var, "CI_TOKEN");
    for bad in [
        "blaze+https://cache.example/t/x/v3?token=abcdef",
        "blaze+https://cache.example/t/x/v3?token=env:",
        "blaze+https://cache.example/x/v3",
        "blaze+https://cache.example/t//v3",
        "blaze+ftp://cache.example/t/x/v3",
        "blaze+https://cache.example/t/x/v3?other=1",
    ] {
        assert!(Spec::parse(bad).is_err(), "{bad}");
    }
    let inline = Spec::parse("blaze+https://c/t/x/v3?token=s3cr3t").unwrap_err().to_string();
    assert!(!inline.contains("s3cr3t"), "{inline}");
}

#[test]
fn a_refusal_names_the_tenant_the_operation_and_the_variable_and_not_the_token() {
    let spec = Spec::parse("blaze+https://c/t/rcache-test/v3").unwrap();
    let ask = Ask::put("manifests/default", b"x", None);
    let headers = HeaderMap::new();
    for (status, had) in [(StatusCode::UNAUTHORIZED, false), (StatusCode::FORBIDDEN, true)] {
        let Judged::Refused(why) = judge(&ask, &spec, had, status, &headers, "no\nmore") else {
            panic!("{status} is a refusal");
        };
        for said in ["write", "manifests/default", "`rcache-test`", TOKEN_VAR, status.as_str()] {
            assert!(why.contains(said), "{why} lacks {said}");
        }
        assert!(!why.contains("more"), "one line of the reason: {why}");
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::RETRY_AFTER, "7".parse().unwrap());
    let limited = judge(&ask, &spec, true, StatusCode::TOO_MANY_REQUESTS, &headers, "");
    assert_eq!(limited, Judged::Wait(Duration::from_secs(7)));
    headers.insert(header::RETRY_AFTER, "100000".parse().unwrap());
    let limited = judge(&ask, &spec, true, StatusCode::TOO_MANY_REQUESTS, &headers, "");
    assert_eq!(limited, Judged::Wait(LONGEST_WAIT));
    let failed = judge(&ask, &spec, true, StatusCode::BAD_GATEWAY, &headers, "");
    assert!(matches!(failed, Judged::Failed(_)));
}

#[test]
fn a_pack_goes_up_and_comes_back_in_ranges() {
    with_fake(|fake| async move {
        let dir = scratch("gateway-round-trip");
        let writer = fake.remote(PRIVATE, Some(WRITER), &dir.join("a"));
        // Big enough that it is fetched in ranges, and sent in one PUT.
        let pack = pack_bytes(40 << 20, 1);
        let (named, keys) = (Digest::of(&pack), vec![3; 32]);
        let file = dir.join("one.pack");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, &pack).unwrap();
        writer.send_file(&pack_object(&named), &file, pack.len() as u64).await.unwrap();
        writer.send_named_by_hash(&keys_object(&Digest::of(&keys)), keys.clone()).await.unwrap();
        let listed = Listed {
            size: pack.len() as u64,
            keys: Digest::of(&keys),
            format: STORE_FORMAT,
            packed: None,
        };
        let line = Manifest([(named, listed)].into()).lines();
        writer.extend_list("manifests/default", |_| line.clone()).await.unwrap();
        assert_eq!(fake.stored(PRIVATE, &pack_object(&named)).unwrap(), pack);

        let reader = fake.remote(PRIVATE, Some(READER), &dir.join("b"));
        let store = dir.join("store");
        let said = pull(&store, &reader, "default").await.unwrap();
        assert!(said.starts_with("1 packs"), "{said}");
        assert_eq!(std::fs::read(store.join(pack_file(&named))).unwrap(), pack);
        assert_eq!(fake.with(|it| it.ranges), 5, "40 MiB in ranges of 8");
        let summary = reader.traffic.summary("pull", Duration::from_secs(1));
        assert!(summary.starts_with("pull: 2 objects, 41943"), "{summary}");
        // And not by a machine of another tenant, nor by one with no token: the same 404.
        for (token, kept) in [(Some(OTHER), "c"), (None, "d")] {
            let stranger = fake.remote(PRIVATE, token, &dir.join(kept));
            let store = dir.join(format!("store-{kept}"));
            let said = pull(&store, &stranger, "default").await.unwrap();
            assert!(said.contains("has no manifest"), "{said}");
            assert!(held(&store) < 100, "{said}");
        }
        _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn what_is_over_a_put_goes_up_in_parts_checked_whole() {
    with_fake(|fake| async move {
        let dir = scratch("gateway-parts");
        std::fs::create_dir_all(&dir).unwrap();
        let writer = fake.remote(PRIVATE, Some(WRITER), &dir.join("a"));
        let pack = pack_bytes((PUT_MAX + (3 << 20)) as usize, 2);
        let named = Digest::of(&pack);
        let file = dir.join("big.pack");
        std::fs::write(&file, &pack).unwrap();
        let len = pack.len() as u64;
        // A part damaged on the way: the whole does not hash to the name, nothing is kept.
        fake.with(|it| it.damage_parts = true);
        let refused = writer.send_file(&pack_object(&named), &file, len).await.unwrap_err();
        assert!(format!("{refused:#}").contains("hash mismatch"), "{refused:#}");
        assert!(fake.stored(PRIVATE, &pack_object(&named)).is_none());
        assert_eq!(fake.with(|it| it.aborted), 1);
        fake.with(|it| it.damage_parts = false);
        writer.send_file(&pack_object(&named), &file, len).await.unwrap();
        assert_eq!(fake.stored(PRIVATE, &pack_object(&named)).unwrap(), pack);
        // Two tries of two parts each: 64 MiB and the rest.
        assert_eq!(fake.with(|it| it.parts), 4);
        // A compressed pack says what its bytes hash to, whole or in parts.
        let packed = pack_bytes(1 << 20, 3);
        let name = format!("packs/{}.pack.zst", Digest::of(b"unpacked"));
        writer.send_named_by_hash(&name, packed.clone()).await.unwrap();
        assert_eq!(fake.stored(PRIVATE, &name).unwrap(), packed);
        _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_token_that_cannot_do_what_is_asked_is_an_error_that_says_so() {
    with_fake(|fake| async move {
        let dir = scratch("gateway-auth");
        let anonymous = fake.remote(PRIVATE, None, &dir.join("a"));
        let refused = anonymous.may_write().unwrap_err().to_string();
        assert!(refused.contains(TOKEN_VAR), "{refused}");
        let reader = fake.remote(PRIVATE, Some(READER), &dir.join("b"));
        let refused = reader.write_small("manifests/x", b"x".to_vec(), None).await.err().unwrap();
        let refused = format!("{refused:#}");
        for said in ["403", "`rcache-test`", "write", TOKEN_VAR] {
            assert!(refused.contains(said), "{refused} lacks {said}");
        }
        assert!(!refused.contains(READER), "{refused}");
        let unknown = fake.remote(PRIVATE, Some("not-a-token"), &dir.join("c"));
        let refused = format!("{:#}", unknown.read_small("manifests/x", 10).await.err().unwrap());
        assert!(refused.contains("401") && !refused.contains("not-a-token"), "{refused}");
        // A refusal is not asked again.
        assert_eq!(unknown.traffic.operations.load(Ordering::Relaxed), 1);
        assert_eq!(unknown.traffic.resumed.load(Ordering::Relaxed), 0);
        // A public tenant is read without one.
        let public_writer = fake.remote(PUBLIC, Some(OTHER), &dir.join("d"));
        public_writer.write_small("manifests/x", b"open".to_vec(), None).await.unwrap();
        let public = fake.remote(PUBLIC, None, &dir.join("e"));
        assert_eq!(public.read_small("manifests/x", 10).await.unwrap().unwrap().0, b"open");
        _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_rate_limit_and_an_outage_are_waited_out_a_bounded_number_of_times() {
    with_fake(|fake| async move {
        let dir = scratch("gateway-limits");
        let writer = fake.remote(PRIVATE, Some(WRITER), &dir.join("a"));
        writer.write_small("manifests/x", b"one".to_vec(), None).await.unwrap();
        fake.with(|it| it.limited = RATE_LIMITED);
        assert_eq!(writer.read_small("manifests/x", 10).await.unwrap().unwrap().0, b"one");
        fake.with(|it| it.failing = 2);
        assert_eq!(writer.read_small("manifests/x", 10).await.unwrap().unwrap().0, b"one");
        // Past the bound the remote is taken to be down, which a pull says.
        fake.with(|it| it.failing = 100);
        let down = writer.read_small("manifests/x", 10).await.err().unwrap();
        assert!(format!("{down:#}").contains("503"), "{down:#}");
        assert!(writer.read_small("manifests/x", 10).await.is_err(), "it is taken to be down");
        _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_write_lands_only_on_what_was_read_and_a_wrong_hash_never() {
    with_fake(|fake| async move {
        let dir = scratch("gateway-conditions");
        let (a, b) = (
            fake.remote(PRIVATE, Some(WRITER), &dir.join("a")),
            fake.remote(PRIVATE, Some(WRITER), &dir.join("b")),
        );
        a.extend_list("manifests/default", |lines| format!("{lines}one\n")).await.unwrap();
        let (_, read_at) = b.read_small("manifests/default", 100).await.unwrap().unwrap();
        a.extend_list("manifests/default", |lines| format!("{lines}two\n")).await.unwrap();
        let stale = b.write_small("manifests/default", b"lost\n".to_vec(), Some(&read_at)).await;
        assert!(matches!(stale.unwrap(), Written::Lost));
        b.extend_list("manifests/default", |lines| format!("{lines}three\n")).await.unwrap();
        let list = fake.stored(PRIVATE, "manifests/default").unwrap();
        assert_eq!(list, b"one\ntwo\nthree\n");
        // Bytes under a name that is not their hash.
        let name = keys_object(&Digest::of(b"something else"));
        let refused = a.send_named_by_hash(&name, b"these".to_vec()).await.unwrap_err();
        assert!(format!("{refused:#}").contains("422"), "{refused:#}");
        assert!(fake.stored(PRIVATE, &name).is_none());
        _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_pack_the_gateway_changed_is_rejected_and_not_kept() {
    with_fake(|fake| async move {
        let dir = scratch("gateway-changed");
        let writer = fake.remote(PRIVATE, Some(WRITER), &dir.join("a"));
        for len in [1 << 20, 20 << 20] {
            let pack = pack_bytes(len, len as u64);
            let (named, keys) = (Digest::of(&pack), vec![4; 32]);
            let file = dir.join("p.pack");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&file, &pack).unwrap();
            writer.send_file(&pack_object(&named), &file, len as u64).await.unwrap();
            writer
                .send_named_by_hash(&keys_object(&Digest::of(&keys)), keys.clone())
                .await
                .unwrap();
            let listed = Listed {
                size: len as u64,
                keys: Digest::of(&keys),
                format: STORE_FORMAT,
                packed: None,
            };
            let line = Manifest([(named, listed)].into()).lines();
            writer.extend_list("manifests/default", |_| line.clone()).await.unwrap();
            fake.with(|it| {
                let stored = it.objects.get_mut(&(PRIVATE.into(), pack_object(&named))).unwrap();
                *stored.bytes.last_mut().unwrap() ^= 1;
            });
            let reader = fake.remote(PRIVATE, Some(READER), &dir.join(format!("b{len}")));
            let store = dir.join(format!("store{len}"));
            let refused = pull(&store, &reader, "default").await.unwrap_err().to_string();
            assert!(refused.contains("1 failed, 1 of them rejected"), "{refused}");
            assert!(!store.join(pack_file(&named)).exists());
        }
        _ = std::fs::remove_dir_all(&dir);
    });
}
