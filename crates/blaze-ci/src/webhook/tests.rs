use blaze_api::http::{HttpRequest, RequestParser};
use ring::hmac;

use super::*;

const SECRET: &[u8] = b"a test secret";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const BASE: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn sign(secret: &[u8], body: &[u8]) -> String {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), body);
    let hex: String = tag.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={hex}")
}

fn request(kind: &str, body: &str) -> HttpRequest {
    HttpRequest {
        method: "POST".to_owned(),
        path: "/webhook".to_owned(),
        headers: vec![
            ("x-github-event".to_owned(), kind.to_owned()),
            ("X-GitHub-Delivery".to_owned(), "72d3162e-cc78-11e3-81ab-4c9367dc0958".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("X-Hub-Signature-256".to_owned(), sign(SECRET, body.as_bytes())),
        ],
        body: body.as_bytes().to_vec(),
    }
}

fn push(reference: &str, deleted: bool) -> String {
    format!(
        r#"{{"ref":"{reference}","before":"{BASE}","after":"{}","deleted":{deleted},
            "repository":{{"full_name":"indexable-inc/blaze","private":true}},
            "pusher":{{"name":"someone"}},"commits":[]}}"#,
        if deleted { "0000000000000000000000000000000000000000" } else { HEAD },
    )
}

fn pull_request(action: &str, head_repo: &str) -> String {
    format!(
        r#"{{"action":"{action}","number":42,
            "pull_request":{{"title":"a change","draft":false,
                "head":{{"ref":"lane","sha":"{HEAD}","repo":{head_repo}}},
                "base":{{"ref":"main","sha":"{BASE}","repo":{{"full_name":"indexable-inc/blaze"}}}}}},
            "repository":{{"full_name":"indexable-inc/blaze"}}}}"#
    )
}

const SAME_REPO: &str = r#"{"full_name":"indexable-inc/blaze"}"#;

#[test]
fn githubs_documented_example_verifies() {
    let signature = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    assert_eq!(verify(b"It's a Secret to Everybody", b"Hello, World!", Some(signature)), Ok(()));
}

#[test]
fn a_wrong_signature_is_401() {
    let good = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    let error = verify(b"another secret", b"Hello, World!", Some(good)).unwrap_err();
    assert_eq!(error.status, 401);
    let error = verify(b"It's a Secret to Everybody", b"Hello, World?", Some(good)).unwrap_err();
    assert_eq!(error.status, 401);
    let bad = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e1";
    assert_eq!(verify(b"It's a Secret to Everybody", b"Hello, World!", Some(bad)).unwrap_err().status, 401);
    let sha1 = "sha1=01dc10d0c83e72ed246219cdd91669667fe2ca59";
    assert_eq!(verify(b"It's a Secret to Everybody", b"Hello, World!", Some(sha1)).unwrap_err().status, 401);
}

#[test]
fn a_missing_signature_is_400_and_an_empty_secret_checks_nothing() {
    let error = verify(SECRET, b"{}", None).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.what.contains("X-Hub-Signature-256"), "{}", error.what);
    let body = b"{}";
    assert_eq!(verify(b"", body, Some(&sign(b"", body))).unwrap_err().status, 500);
}

#[test]
fn a_push_to_a_branch() {
    let delivery = receive(SECRET, &request("push", &push("refs/heads/main", false))).unwrap();
    assert_eq!(delivery.id, "72d3162e-cc78-11e3-81ab-4c9367dc0958");
    assert_eq!(delivery.repo, "indexable-inc/blaze");
    assert_eq!(delivery.event, Some(Event::Push { branch: "main".to_owned(), commit: HEAD.to_owned() }));
}

#[test]
fn a_branch_with_slashes_keeps_them() {
    let delivery = parse("push", "1", push("refs/heads/lanes/ci", false).as_bytes()).unwrap();
    assert_eq!(delivery.event, Some(Event::Push { branch: "lanes/ci".to_owned(), commit: HEAD.to_owned() }));
}

#[test]
fn a_pushed_tag() {
    let delivery = parse("push", "1", push("refs/tags/v2.0.0", false).as_bytes()).unwrap();
    assert_eq!(delivery.event, Some(Event::Tag { name: "v2.0.0".to_owned(), commit: HEAD.to_owned() }));
}

#[test]
fn a_deleted_branch_or_tag_is_ignored() {
    for reference in ["refs/heads/main", "refs/tags/v1"] {
        let delivery = receive(SECRET, &request("push", &push(reference, true))).unwrap();
        assert_eq!(delivery.repo, "indexable-inc/blaze");
        assert_eq!(delivery.event, None);
    }
}

#[test]
fn a_push_to_another_kind_of_ref_is_ignored() {
    let delivery = parse("push", "1", push("refs/notes/commits", false).as_bytes()).unwrap();
    assert_eq!(delivery.event, None);
}

#[test]
fn a_pull_request_from_the_same_repository() {
    for action in ["opened", "synchronize", "reopened", "ready_for_review"] {
        let delivery = receive(SECRET, &request("pull_request", &pull_request(action, SAME_REPO))).unwrap();
        assert_eq!(delivery.repo, "indexable-inc/blaze");
        assert_eq!(
            delivery.event,
            Some(Event::PullRequest {
                number: 42,
                head: HEAD.to_owned(),
                base: BASE.to_owned(),
                from_fork: false,
            }),
            "{action}",
        );
    }
}

#[test]
fn a_pull_request_from_a_fork() {
    let body = pull_request("opened", r#"{"full_name":"someone/blaze"}"#);
    let delivery = parse("pull_request", "1", body.as_bytes()).unwrap();
    assert!(matches!(delivery.event, Some(Event::PullRequest { from_fork: true, .. })));
}

#[test]
fn a_pull_request_whose_fork_was_deleted_is_from_a_fork() {
    let delivery = parse("pull_request", "1", pull_request("synchronize", "null").as_bytes()).unwrap();
    assert!(matches!(delivery.event, Some(Event::PullRequest { from_fork: true, .. })));
}

#[test]
fn other_pull_request_actions_are_ignored() {
    for action in ["closed", "labeled", "edited", "converted_to_draft"] {
        let delivery = parse("pull_request", "1", pull_request(action, SAME_REPO).as_bytes()).unwrap();
        assert_eq!(delivery.event, None, "{action}");
        assert_eq!(delivery.repo, "indexable-inc/blaze");
    }
}

#[test]
fn a_ping_is_ignored() {
    let body = r#"{"zen":"Keep it logically awesome.","hook_id":1,"hook":{"type":"App"}}"#;
    let delivery = receive(SECRET, &request("ping", body)).unwrap();
    assert_eq!(delivery, Delivery { id: "72d3162e-cc78-11e3-81ab-4c9367dc0958".to_owned(), repo: String::new(), event: None });
}

#[test]
fn a_body_that_is_not_the_event_is_400() {
    let error = parse("push", "1", br#"{"ref":"refs/heads/main"}"#).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.what.contains("push"), "{}", error.what);
    let error = parse("pull_request", "1", b"not json").unwrap_err();
    assert_eq!(error.status, 400);
    let bad_sha = push("refs/heads/main", false).replace(HEAD, "HEAD");
    let error = parse("push", "1", bad_sha.as_bytes()).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.what.contains("after"), "{}", error.what);
}

#[test]
fn missing_headers_are_400_and_only_post_is_taken() {
    let body = push("refs/heads/main", false);
    let mut get = request("push", &body);
    get.method = "GET".to_owned();
    assert_eq!(receive(SECRET, &get).unwrap_err().status, 405);

    let mut no_event = request("push", &body);
    no_event.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("X-GitHub-Event"));
    let error = receive(SECRET, &no_event).unwrap_err();
    assert_eq!((error.status, error.what.contains("X-GitHub-Event")), (400, true), "{}", error.what);

    let mut no_id = request("push", &body);
    no_id.headers.retain(|(name, _)| name != "X-GitHub-Delivery");
    assert_eq!(receive(SECRET, &no_id).unwrap_err().status, 400);

    let mut unsigned = request("push", &body);
    unsigned.headers.retain(|(name, _)| name != "X-Hub-Signature-256");
    assert_eq!(receive(SECRET, &unsigned).unwrap_err().status, 400);
}

#[test]
fn the_signature_is_checked_before_the_payload_is_read() {
    let mut request = request("push", "not json");
    request.headers.retain(|(name, _)| name != "X-Hub-Signature-256");
    request.headers.push(("X-Hub-Signature-256".to_owned(), sign(b"not the secret", b"not json")));
    assert_eq!(receive(SECRET, &request).unwrap_err().status, 401);
}

#[test]
fn a_form_encoded_delivery_is_refused_by_name() {
    let mut request = request("push", &push("refs/heads/main", false));
    request.headers.retain(|(name, _)| name != "Content-Type");
    request.headers.push(("Content-Type".to_owned(), "application/x-www-form-urlencoded".to_owned()));
    let error = receive(SECRET, &request).unwrap_err();
    assert_eq!(error.status, 415);
}

#[test]
fn escaped_strings_are_read_as_well_as_borrowed_ones() {
    let body = push("refs/heads/main", false)
        .replace(r#""refs/heads/main""#, r#""refs\/heads\/l\u00e9ne""#)
        .replace("indexable-inc/blaze", r"indexable-inc\/blaze");
    let delivery = parse("push", "1", body.as_bytes()).unwrap();
    assert_eq!(delivery.repo, "indexable-inc/blaze");
    assert_eq!(delivery.event, Some(Event::Push { branch: "léne".to_owned(), commit: HEAD.to_owned() }));
}

#[test]
fn a_pull_request_ci_runs_needs_its_number_pull_request_and_repository() {
    let whole = pull_request("opened", SAME_REPO);
    let value: serde_json::Value = serde_json::from_str(&whole).unwrap();
    for field in ["number", "pull_request", "repository"] {
        let mut less = value.clone();
        less.as_object_mut().unwrap().remove(field);
        let error = parse("pull_request", "1", less.to_string().as_bytes()).unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.what.contains(field), "{}", error.what);
    }
    let mut closed = value.clone();
    closed["action"] = "closed".into();
    closed.as_object_mut().unwrap().remove("pull_request");
    let delivery = parse("pull_request", "1", closed.to_string().as_bytes()).unwrap();
    assert_eq!((delivery.event, delivery.repo.as_str()), (None, "indexable-inc/blaze"));
}

#[test]
fn a_signature_of_the_right_length_with_a_non_hex_digit_is_401() {
    let good = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    let bad = good.replace("e17", "e1g");
    let error = verify(b"It's a Secret to Everybody", b"Hello, World!", Some(&bad)).unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(blaze_format::hex::decode::<32>(&good["sha256=".len()..]).map(|it| it[0]), Some(0x75));
}

/// The decoder before sonic-rs, kept to hold it to.
struct SerdeJson;

impl Json for SerdeJson {
    type Error = serde_json::Error;
    fn from_slice<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, serde_json::Error> {
        serde_json::from_slice(body)
    }
}

/// What a decoder makes of a payload: the delivery, or the refusal's status and its words up to
/// the decoder's own (which name the place each decoder stopped in its own terms).
fn decoded<J: Json>(kind: &str, body: &[u8]) -> Result<Delivery, (u16, String)> {
    parse_by::<J>(kind, "1", body).map(DeliveryRef::into_owned).map_err(|error| {
        let words = match error.what.find(" is not one: ") {
            Some(at) => error.what[..at + " is not one".len()].to_owned(),
            None => error.what,
        };
        (error.status, words)
    })
}

#[test]
fn sonic_rs_takes_and_refuses_the_payloads_serde_json_did() {
    let escaped = push("refs/heads/main", false)
        .replace(r#""refs/heads/main""#, r#""refs\/heads\/l\u00e9ne""#)
        .replace("indexable-inc/blaze", r"indexable-inc\/blaze");
    let fork = pull_request("opened", r#"{"full_name":"someone/blaze"}"#);
    let payloads = [
        ("push", push("refs/heads/main", false)),
        ("push", push("refs/tags/v1", false)),
        ("push", push("refs/heads/main", true)),
        ("push", escaped),
        ("pull_request", pull_request("opened", SAME_REPO)),
        ("pull_request", pull_request("synchronize", "null")),
        ("pull_request", pull_request("closed", SAME_REPO)),
        ("pull_request", fork),
        ("ping", r#"{"zen":"Keep it logically awesome.","hook_id":1,"hook":{"type":"App","events":["*"]}}"#.to_owned()),
        ("ping", r#"{"repository":{"full_name":"a/b","extra":[1,2.5e3,-0,true,null,{"x":"\ud83d\ude00"}]}}"#.to_owned()),
        ("push", format!(r#"{}  {}"#, push("refs/heads/main", false), "\n\t")),
    ];
    let mut checked = 0;
    for (kind, payload) in &payloads {
        let body = payload.as_bytes();
        // A body that is not UTF-8 is no JSON (RFC 8259) and is refused whole; serde_json read
        // only the strings it kept, so took one whose bad bytes were in a field it skipped.
        let mut check = |kind: &str, body: &[u8]| {
            let said = || format!("{kind} {:?}", String::from_utf8_lossy(body));
            match std::str::from_utf8(body) {
                Ok(_) => assert_eq!(decoded::<Sonic>(kind, body), decoded::<SerdeJson>(kind, body), "{}", said()),
                Err(_) => assert!(matches!(decoded::<Sonic>(kind, body), Err((400, _))), "{}", said()),
            }
            checked += 1;
        };
        check(kind, body);
        // Every prefix: a payload cut short anywhere.
        for end in 0..body.len() {
            check(kind, &body[..end]);
        }
        // Every byte replaced by each byte that changes JSON's meaning or its UTF-8.
        let mut changed = body.to_vec();
        for at in 0..body.len() {
            for byte in *b"\"\\{}[],:0-e.nt \x00\x1f\x7f\x80\xc3\xff" {
                let was = std::mem::replace(&mut changed[at], byte);
                check(kind, &changed);
                changed[at] = was;
            }
        }
        // A field given twice, and bytes after the payload.
        let twice = payload.replacen('{', r#"{"repository":{"full_name":"x/y"},"#, 1);
        check(kind, twice.as_bytes());
        check(kind, format!("{payload} {{}}").as_bytes());
        check(kind, format!("{payload}x").as_bytes());
    }
    for (kind, body) in [
        ("ping", "[]".to_owned()),
        ("ping", "null".to_owned()),
        ("ping", r#"{"repository":null}"#.to_owned()),
        ("ping", r#"{"repository":{"full_name":"a\u0000b"}}"#.to_owned()),
        ("ping", r#"{"repository":{"full_name":"\ud800"}}"#.to_owned()),
        ("ping", r#"{"x":1e400,"repository":{"full_name":"a/b"}}"#.to_owned()),
        ("ping", format!(r#"{{"x":{}{},"repository":{{"full_name":"a/b"}}}}"#, "[".repeat(100), "]".repeat(100))),
        // A skipped field nested far past any stack, whole and cut short: serde_json skips it
        // with its open brackets on the heap, at no depth limit, and so must sonic-rs.
        ("ping", format!(r#"{{"x":{}1{},"repository":{{"full_name":"a/b"}}}}"#, r#"[{"k":"#.repeat(100_000), "}]".repeat(100_000))),
        ("ping", format!(r#"{{"x":{}1{},"repository":{{"full_name":"a/b"}}}}"#, r#"[{"k":"#.repeat(100_000), "}]".repeat(99_999))),
        ("pull_request", pull_request("opened", SAME_REPO).replace("42", "042")),
        ("pull_request", pull_request("opened", SAME_REPO).replace("42", "18446744073709551616")),
        ("pull_request", pull_request("opened", SAME_REPO).replace("42", "4.2e1")),
    ] {
        assert_eq!(decoded::<Sonic>(kind, body.as_bytes()), decoded::<SerdeJson>(kind, body.as_bytes()), "{kind} {body}");
    }
    assert!(checked > 10_000, "{checked}");
}

/// `request` as it comes over the wire.
fn wire(request: &HttpRequest) -> Vec<u8> {
    let mut bytes = format!("{} {} HTTP/1.1\r\n", request.method, request.path).into_bytes();
    for (name, value) in &request.headers {
        bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    bytes.extend_from_slice(format!("Content-Length: {}\r\n\r\n", request.body.len()).as_bytes());
    bytes.extend_from_slice(&request.body);
    bytes
}

#[test]
fn a_request_read_in_place_is_received_as_the_owned_one_is() {
    let push = push("refs/heads/main", false);
    let with = |kind: &str, body: &str, change: &dyn Fn(&mut HttpRequest)| {
        let mut it = request(kind, body);
        change(&mut it);
        it
    };
    let drop_header = |name: &'static str| move |it: &mut HttpRequest| it.headers.retain(|(at, _)| at != name);
    let requests = [
        request("push", &push),
        request("pull_request", &pull_request("opened", SAME_REPO)),
        request("pull_request", &pull_request("closed", r#"{"full_name":"someone/blaze"}"#)),
        request("ping", r#"{"zen":"Keep it logically awesome.","hook_id":1}"#),
        request("push", "not json"),
        request("push", ""),
        with("push", &push, &|it| it.method = "GET".to_owned()),
        with("push", &push, &drop_header("x-github-event")),
        with("push", &push, &drop_header("X-GitHub-Delivery")),
        with("push", &push, &drop_header("X-Hub-Signature-256")),
        with("push", &push, &|it| it.body[3] ^= 1),
        with("push", &push, &|it| {
            it.headers.retain(|(name, _)| name != "Content-Type");
            it.headers.push(("content-type".to_owned(), "Application/JSON; charset=utf-8".to_owned()));
        }),
        with("push", &push, &|it| it.headers.push(("Content-Type".to_owned(), "text/plain".to_owned()))),
        with("push", &push, &|it| it.headers.insert(0, ("X-GitHub-Event".to_owned(), "ping".to_owned()))),
    ];
    for (at, original) in requests.iter().enumerate() {
        let bytes = wire(original);
        let mut owned = RequestParser::new(MOST_PAYLOAD);
        let mut read = None;
        for piece in bytes.chunks(7) {
            owned.push(piece);
            if let Some(request) = owned.next().unwrap() {
                read = Some(request);
            }
        }
        let read = read.expect("the request is whole");
        let mut in_place = RequestParser::new(MOST_PAYLOAD);
        in_place.push(&bytes);
        let lent = in_place.next_ref().unwrap().expect("the request is whole");
        let want = receive(SECRET, &read);
        let verifier = Verifier::new(SECRET);
        assert_eq!(receive_ref(&verifier, &lent).map(DeliveryRef::into_owned), want, "request {at}");
        assert_eq!(receive(SECRET, original), want, "request {at}");
    }
}

#[test]
fn a_key_made_once_verifies_as_one_made_per_delivery() {
    let body = b"Hello, World!";
    let good = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    let bad = good.replace("e17", "e18");
    for secret in [&b"It's a Secret to Everybody"[..], b"another secret", b""] {
        let verifier = Verifier::new(secret);
        for signature in [Some(good), Some(bad.as_str()), Some("sha256=zz"), None] {
            assert_eq!(verifier.verify(body, signature), verify(secret, body, signature), "{signature:?}");
        }
    }
}

#[test]
fn a_delivery_read_in_place_lends_its_strings_from_the_request() {
    let body = push("refs/heads/main", false);
    let id = "72d3162e-cc78-11e3-81ab-4c9367dc0958";
    let lent = parse_ref("push", id, body.as_bytes()).unwrap();
    assert!(std::ptr::eq(lent.id, id));
    let Cow::Borrowed(repo) = &lent.repo else { panic!("the repository was copied") };
    assert!(body.as_bytes().as_ptr_range().contains(&repo.as_ptr()));
    let Some(EventRef::Push { branch: Cow::Borrowed(branch), commit: Cow::Borrowed(commit) }) = &lent.event else {
        panic!("the event was copied: {:?}", lent.event);
    };
    assert_eq!((*branch, *commit), ("main", HEAD));
}

#[test]
fn a_warm_delivery_allocates_only_its_events_strings() {
    let verifier = Verifier::new(SECRET);
    let bodies = [
        ("push", push("refs/heads/main", false), 2),
        ("push", push("refs/tags/v1", false), 2),
        ("push", push("refs/heads/main", true), 0),
        ("pull_request", pull_request("synchronize", SAME_REPO), 2),
        ("pull_request", pull_request("closed", SAME_REPO), 0),
        ("ping", r#"{"zen":"Keep it logically awesome.","hook_id":1}"#.to_owned(), 0),
    ];
    for (kind, body, strings) in &bodies {
        let mut parser = RequestParser::new(MOST_PAYLOAD);
        let bytes = wire(&request(kind, body));
        for round in 0..3 {
            parser.reset();
            parser.push(&bytes);
            let lent = parser.next_ref().unwrap().expect("the request is whole");
            let delivery = blaze_alloc_count::assert_at_most(0, "a delivery checked and read in place", || {
                receive_ref(&verifier, &lent).unwrap()
            });
            let event = blaze_alloc_count::assert_at_most(
                *strings,
                "an event handed to the service: its two strings, its own",
                || delivery.event.map(EventRef::into_owned),
            );
            assert_eq!(event.is_some(), *strings > 0, "{kind} {round}");
        }
    }
}

#[test]
fn a_payload_over_githubs_cap_is_refused() {
    let body = vec![b' '; MOST_PAYLOAD + 1];
    assert_eq!(parse("push", "1", &body).unwrap_err().status, 413);
}
