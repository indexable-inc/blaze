// alloc: the whole file is tests

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender};

use blaze_cas::{Driven, Handle};

use blaze_cas::Kind as Stored;

use crate::{
    Algo, Blake3, Digest, DigestRefused, Failure, FetchError, Fetcher, Flights, Form, Made, Net,
    Object, Objects, Origin, Policy, Produced, Producer, Source, StoreOf, Tree, TreeFile,
    decisions::{Attempts, Decision},
    flight::Role,
    store::{CHUNK_LEN, CHUNKED, INLINE, chunk_key, object_key},
    tree::{self, Entry, Kind},
};

fn store(dir: &Path) -> (Driven, &'static Handle) {
    let mut driven = Driven::open(dir).unwrap();
    let handle = Box::leak(Box::new(driven.handle()));
    (driven, handle)
}

/// A server with one body for every URL, failing the first `fail` asks transiently.
struct Fake {
    body: Vec<u8>,
    asks: AtomicU32,
    fail: u32,
    /// Said as an ask begins, then waited on before it answers: the test holds the download
    /// under way.
    gate: Option<(Sender<()>, Receiver<()>)>,
}

impl Fake {
    fn new(body: &[u8]) -> Fake {
        Fake { body: body.to_vec(), asks: AtomicU32::new(0), fail: 0, gate: None }
    }

    fn asks(&self) -> u32 {
        self.asks.load(Ordering::SeqCst)
    }
}

impl Net for Fake {
    fn get(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        from: u64,
        sink: &mut (dyn FnMut(u64, &[u8]) -> std::io::Result<()> + Send),
    ) -> Result<(), Failure> {
        let ask = self.asks.fetch_add(1, Ordering::SeqCst);
        if let Some((started, gate)) = &self.gate {
            _ = started.try_send(());
            gate.recv().unwrap();
        }
        if ask < self.fail {
            // The body up to its middle, then a cut: a retry resumes from there.
            let half = self.body.len() / 2;
            let from = usize::try_from(from).unwrap().min(half);
            sink(from as u64, &self.body[from..half])
                .map_err(|e| Failure::Permanent(e.to_string()))?;
            return Err(Failure::Transient("connection reset".to_owned()));
        }
        // Small pieces for a small body, so a piece boundary falls everywhere; large ones for
        // a body of several chunks, which would otherwise take millions of calls. Like a
        // server that ignores ranges, it sends the whole body from its start.
        let step = if self.body.len() > 1 << 16 { 1 << 16 } else { 7 };
        let mut at = 0u64;
        for piece in self.body.chunks(step) {
            sink(at, piece).map_err(|e| Failure::Permanent(e.to_string()))?;
            at += piece.len() as u64;
        }
        Ok(())
    }
}

fn quick() -> Policy {
    Policy { tries: 3, first_wait: Duration::ZERO, most_wait: Duration::ZERO }
}

fn fetcher(store: &'static Handle, net: Arc<Fake>, scratch: &Path) -> Fetcher {
    let flights = Box::leak(Box::new(Flights::new()));
    Fetcher::new(StoreOf::Given(store), net, scratch.to_owned())
        .with_policy(quick())
        .with_flights(flights)
}

/// The record a URL's body pinned by `pin` is filed under.
fn blob(pin: &Digest) -> Origin<'_> {
    Origin::Pinned { pin, form: Form::Blob }
}

fn url(digest: Digest) -> Source {
    Source::Url { url: "https://user:secret@example.com/x.tar.gz".to_owned(), digest }
}

#[test]
fn digests_parse_and_print_as_algo_colon_hex() {
    let sha256 = Digest::of(Algo::Sha256, b"abc");
    assert_eq!(
        sha256.to_string(),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(Digest::parse(&sha256.to_string()), Ok(sha256));
    assert_eq!(
        Digest::parse(&sha256.to_string().to_uppercase().replace("SHA256", "sha256")),
        Ok(sha256)
    );
    let sha512 = Digest::of(Algo::Sha512, b"abc");
    assert!(sha512.to_string().starts_with("sha512:ddaf35a193617aba"));
    assert_eq!(Digest::parse(&sha512.to_string()), Ok(sha512));
    let blake3 = Digest::of(Algo::Blake3, b"abc");
    assert_eq!(blake3.to_string(), format!("blake3:{}", Blake3::of(b"abc").hex()));
    assert_eq!(Digest::parse(&blake3.to_string()), Ok(blake3));
    let json: Digest = serde_json::from_str(&format!("\"{sha256}\"")).unwrap();
    assert_eq!(json, sha256);
}

#[test]
fn weak_unknown_and_malformed_digests_are_refused_by_name() {
    let refused = |text: &str| Digest::parse(text).unwrap_err();
    assert!(
        matches!(refused("md5:900150983cd24fb0d6963f7d28e17f72"), DigestRefused::Weak { algo, .. } if algo == "md5")
    );
    assert!(matches!(
        refused("sha1:a9993e364706816aba3e25717850c26c9cd0d89d"),
        DigestRefused::Weak { .. }
    ));
    assert!(matches!(
        refused("SHA-1:a9993e364706816aba3e25717850c26c9cd0d89d"),
        DigestRefused::Weak { .. }
    ));
    assert!(
        matches!(refused("crc32:352441c2"), DigestRefused::Unknown { algo, .. } if algo == "crc32")
    );
    assert!(matches!(refused("ba7816bf"), DigestRefused::NoAlgo { .. }));
    assert!(matches!(refused("sha256:zz"), DigestRefused::NotHex { algo: Algo::Sha256, .. }));
    assert!(matches!(refused("sha256:ba78"), DigestRefused::Length { digits: 4, .. }));
    assert!(refused("md5:00").to_string().contains("weak"));
    assert!(serde_json::from_str::<Digest>("\"sha1:00\"").is_err());
}

#[test]
fn a_fetch_files_the_bytes_and_a_second_asks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let net = Arc::new(Fake::new(b"some crate bytes"));
    let fetcher = fetcher(store, Arc::clone(&net), &dir.path().join("scratch"));
    let pin = Digest::of(Algo::Sha256, b"some crate bytes");
    let object = fetcher.get(&url(pin)).unwrap();
    assert_eq!(object.blake3, Blake3::of(b"some crate bytes"));
    assert_eq!(object.form, Form::Blob);
    assert_eq!(&*fetcher.bytes(&object).unwrap(), b"some crate bytes");
    assert_eq!(fetcher.get(&url(pin)).unwrap(), object);
    assert_eq!(net.asks(), 1);
    // The same bytes under another algorithm's pin are one object with a second record.
    let blake3 = Digest::of(Algo::Blake3, b"some crate bytes");
    assert_eq!(fetcher.get(&url(blake3)).unwrap(), object);
    assert_eq!(net.asks(), 2);
    let out = fetcher.materialize(&object, &dir.path().join("out")).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"some crate bytes");
    assert_eq!(fetcher.materialize(&object, &dir.path().join("out")).unwrap(), out);
    assert_eq!(std::fs::read_dir(dir.path().join("scratch")).unwrap().count(), 0);
}

#[test]
fn a_mismatch_names_both_digests_and_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let net = Arc::new(Fake::new(b"tampered"));
    let fetcher = fetcher(store, net, &dir.path().join("scratch"));
    let pin = Digest::of(Algo::Sha256, b"the real bytes");
    let error = fetcher.get(&url(pin)).unwrap_err();
    let FetchError::Mismatch { expected, actual, .. } = &error else { panic!("{error}") };
    assert_eq!(*expected, pin);
    assert_eq!(*actual, Digest::of(Algo::Sha256, b"tampered"));
    let said = error.to_string();
    assert!(said.contains("https://example.com/x.tar.gz"), "{said}");
    assert!(!said.contains("secret"), "{said}");
    assert!(said.contains(&format!("specified: {pin}")), "{said}");
    assert!(said.contains(&format!("got:       {actual}")), "{said}");
    // Nothing kept: no record, no object, no temporary file.
    let objects = Objects::new(store);
    assert_eq!(objects.lookup(blob(&pin)).unwrap(), None);
    assert_eq!(objects.lookup(blob(actual)).unwrap(), None);
    let object = Object { blake3: Blake3::of(b"tampered"), len: 8, form: Form::Blob };
    assert!(!store.has(&object.key()));
    assert_eq!(std::fs::read_dir(dir.path().join("scratch")).unwrap().count(), 0);
}

#[test]
fn transient_failures_are_tried_again_from_nothing_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let body = b"bytes after two cuts";
    let net = Arc::new(Fake { fail: 2, ..Fake::new(body) });
    let fetcher = fetcher(store, Arc::clone(&net), &dir.path().join("scratch"));
    let object = fetcher.get(&url(Digest::of(Algo::Sha512, body))).unwrap();
    assert_eq!(&*fetcher.bytes(&object).unwrap(), body);
    assert_eq!(net.asks(), 3);

    let net = Arc::new(Fake { fail: 9, ..Fake::new(b"never whole") });
    let fetcher = crate::tests::fetcher(store, Arc::clone(&net), &dir.path().join("scratch"));
    let error = fetcher.get(&url(Digest::of(Algo::Sha256, b"never whole"))).unwrap_err();
    assert!(matches!(error, FetchError::GaveUp { tries: 3, .. }), "{error}");
    assert_eq!(net.asks(), 3);
}

#[test]
fn the_retry_policy_is_a_transcript() {
    let mut attempts = Attempts::new(Policy {
        tries: 3,
        first_wait: Duration::from_secs(1),
        most_wait: Duration::from_secs(3),
    });
    assert_eq!(attempts.next_decision(), Decision::Ask { attempt: 1 });
    attempts.failed(Failure::Transient("503".to_owned()));
    assert_eq!(attempts.next_decision(), Decision::Wait { until: Duration::from_secs(1) });
    attempts.advance_clock(Duration::from_secs(1));
    assert_eq!(attempts.next_decision(), Decision::Ask { attempt: 2 });
    attempts.failed(Failure::Transient("reset".to_owned()));
    assert_eq!(attempts.next_deadline(), Some(Duration::from_secs(3)));
    attempts.advance_clock(Duration::from_secs(3));
    assert_eq!(attempts.next_decision(), Decision::Ask { attempt: 3 });
    attempts.failed(Failure::Transient("reset".to_owned()));
    assert_eq!(
        attempts.next_decision(),
        Decision::GiveUp { tries: 3, last: Failure::Transient("reset".to_owned()) }
    );

    let mut attempts = Attempts::new(Policy::default());
    assert_eq!(attempts.next_decision(), Decision::Ask { attempt: 1 });
    attempts.failed(Failure::Permanent("404".to_owned()));
    assert_eq!(
        attempts.next_decision(),
        Decision::GiveUp { tries: 1, last: Failure::Permanent("404".to_owned()) }
    );
}

#[test]
fn one_download_per_pin() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let (started, on_start) = crossbeam_channel::bounded(1);
    let (release, gate) = crossbeam_channel::bounded(0);
    let net = Arc::new(Fake { gate: Some((started, gate)), ..Fake::new(b"one download") });
    let (followed, on_follow) = crossbeam_channel::unbounded();
    let flights: &'static Flights = Box::leak(Box::new(Flights::watched(followed)));
    let fetcher = Arc::new(
        fetcher(store, Arc::clone(&net), &dir.path().join("scratch")).with_flights(flights),
    );
    let pin = Digest::of(Algo::Sha256, b"one download");
    let ask = |fetcher: &Arc<Fetcher>| {
        let fetcher = Arc::clone(fetcher);
        std::thread::Builder::new()
            .name("blaze-fetch test request".to_owned())
            .spawn(move || fetcher.get(&url(pin)).unwrap())
            .unwrap()
    };
    let leader = ask(&fetcher);
    on_start.recv_timeout(Duration::from_secs(60)).expect("the leader began its download");
    // The leader is downloading: its flight is in the table, keyed by this store and the pin.
    assert_eq!(flights.len(), 1);
    let flight = (Objects::new(store).store_id(), blob(&pin).key());
    assert!(matches!(flights.begin(flight, pin), Role::Follow(_)));
    on_follow.recv_timeout(Duration::from_secs(60)).expect("the probe followed");
    // The same pin into another store is a flight of its own.
    let other_dir = tempfile::tempdir().unwrap();
    let (_other_driven, other) = crate::tests::store(other_dir.path());
    assert!(matches!(
        flights.begin((Objects::new(other).store_id(), blob(&pin).key()), pin),
        Role::Lead(_)
    ));
    let followers: Vec<_> = (0..3).map(|_| ask(&fetcher)).collect();
    // Each of the three is waiting on the leader before the leader is let go: none of them
    // can have found the store filled or downloaded itself.
    for _ in 0..3 {
        on_follow.recv_timeout(Duration::from_secs(60)).expect("a request followed the leader");
    }
    assert_eq!(net.asks(), 1);
    release.send(()).unwrap();
    let object = leader.join().unwrap();
    for follower in followers {
        assert_eq!(follower.join().unwrap(), object);
    }
    assert_eq!(net.asks(), 1);
    assert!(on_follow.try_recv().is_err(), "no request followed but the probe and the three");
    assert!(flights.is_empty());
}

#[test]
fn the_index_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let pin = Digest::of(Algo::Sha256, b"kept across restarts");
    let object = {
        let (mut driven, store) = store(dir.path());
        let net = Arc::new(Fake::new(b"kept across restarts"));
        let object = fetcher(store, net, &dir.path().join("scratch")).get(&url(pin)).unwrap();
        driven.flush().unwrap();
        object
    };
    let (_driven, store) = store(dir.path());
    // No network: a fetch that asked would fail every try.
    let net = Arc::new(Fake { fail: 99, ..Fake::new(b"") });
    let fetcher = fetcher(store, Arc::clone(&net), &dir.path().join("scratch"));
    assert_eq!(Objects::new(store).lookup(blob(&pin)).unwrap(), Some(object));
    let again = fetcher.get(&url(pin)).unwrap();
    assert_eq!(again, object);
    assert_eq!(&*fetcher.bytes(&again).unwrap(), b"kept across restarts");
    assert_eq!(net.asks(), 0);
}

#[test]
fn a_step_records_its_output_by_code_and_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let objects = Objects::new(store);
    let input = objects
        .keep_pinned(
            &Digest::of(Algo::Sha256, b"input"),
            Producer::Given("a test".to_owned()),
            b"input",
        )
        .unwrap();
    let tree = Tree {
        files: vec![TreeFile {
            path: "out".to_owned(),
            kind: Kind::File,
            bytes: b"output".to_vec(),
        }],
    };
    let code = blaze_ids::ContentId::of("a step's code", b"unpack v1");
    let inputs = [input.id()];
    let output = objects.keep_step(code, &inputs, "unpack", &tree).unwrap();
    assert_eq!(output.form, Form::Tree);
    assert_eq!(objects.lookup(Origin::Step { code, inputs: &inputs }).unwrap(), Some(output));
    let other = blaze_ids::ContentId::of("a step's code", b"unpack v2");
    assert_eq!(objects.lookup(Origin::Step { code: other, inputs: &inputs }).unwrap(), None);
    // The final pin, over the output's listing: checked and filed by `ensure`, which hashes
    // the tree itself; then `ensure` with it kept never runs the graph.
    let pin = Digest::of(Algo::Blake3, &objects.read(&output).unwrap());
    let from = Producer::Step("unpack".to_owned());
    let pinned =
        crate::ensure(objects, &pin, Form::Tree, || Ok(Produced { made: Made::Tree(&tree), from }))
            .unwrap();
    assert_eq!(pinned, output);
    let got =
        crate::ensure(objects, &pin, Form::Tree, || panic!("the graph ran with its output kept"))
            .unwrap();
    assert_eq!(got, output);
    // A file pinned by the same digest is another record (`Form` is in the key).
    assert_eq!(objects.lookup(blob(&pin)).unwrap(), None);
    // A producer that makes a file where a tree was asked for keeps nothing.
    let pin = Digest::of(Algo::Sha256, b"not a tree");
    let made = crate::ensure(objects, &pin, Form::Tree, || {
        Ok(Produced { made: Made::Bytes(b"not a tree"), from: Producer::Step("wrong".to_owned()) })
    });
    assert!(made.is_err());
    assert_eq!(objects.lookup(blob(&pin)).unwrap(), None);
}

#[test]
fn a_record_whose_object_is_gone_is_a_miss_and_fetched_again() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"collected since";
    let pin = Digest::of(Algo::Sha256, body);
    let object = Object { blake3: Blake3::of(body), len: body.len() as u64, form: Form::Blob };
    {
        // The record survives a restart, and its object does not (as one would be lost were values ever collected, or the store damaged).
        let (mut driven, store) = store(dir.path());
        Objects::new(store).record(&object, blob(&pin)).unwrap();
        driven.flush().unwrap();
    }
    let (_driven, store) = store(dir.path());
    let objects = Objects::new(store);
    assert_eq!(objects.lookup(blob(&pin)).unwrap(), None);
    let net = Arc::new(Fake::new(body));
    let fetcher = fetcher(store, Arc::clone(&net), &dir.path().join("scratch"));
    assert_eq!(fetcher.get(&url(pin)).unwrap(), object);
    assert_eq!(net.asks(), 1);
    // The same record put again was taken as kept, and now finds its object.
    assert_eq!(objects.lookup(blob(&pin)).unwrap(), Some(object));
    assert_eq!(fetcher.bytes(&object).unwrap(), body);
}

#[test]
fn an_object_of_many_chunks_is_put_and_read_a_chunk_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let body: Vec<u8> = (0..CHUNK_LEN + 10).map(|i| (i * 31 % 251) as u8).collect();
    let pin = Digest::of(Algo::Sha512, &body);
    let net = Arc::new(Fake::new(&body));
    let fetcher = fetcher(store, Arc::clone(&net), &dir.path().join("scratch"));
    let object = fetcher.get(&url(pin)).unwrap();
    assert_eq!(object.len, body.len() as u64);
    assert_eq!(fetcher.bytes(&object).unwrap(), body);
    let out = fetcher.materialize(&object, &dir.path().join("out")).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), body);
    // Its value is the list of its two chunks, each a value of its own.
    let (first, rest) = body.split_at(CHUNK_LEN);
    assert!(store.has(&chunk_key(Blake3::of(first))));
    assert!(store.has(&chunk_key(Blake3::of(rest))));

    // In another store, the same list with its second chunk gone: a miss, and fetched again.
    let other_dir = tempfile::tempdir().unwrap();
    let (_other_driven, other) = crate::tests::store(other_dir.path());
    let mut list = vec![CHUNKED];
    for chunk in [first, rest] {
        list.extend_from_slice(&Blake3::of(chunk).0);
        list.extend_from_slice(&(chunk.len() as u64).to_le_bytes());
    }
    other.put(chunk_key(Blake3::of(first)), Stored::Output, first).unwrap();
    other.put(object_key(object.blake3), Stored::Output, &list).unwrap();
    let objects = Objects::new(other);
    objects.record(&object, blob(&pin)).unwrap();
    assert_eq!(objects.lookup(blob(&pin)).unwrap(), None);
    let fetcher = crate::tests::fetcher(other, Arc::clone(&net), &other_dir.path().join("scratch"));
    assert_eq!(fetcher.get(&url(pin)).unwrap(), object);
    assert_eq!(net.asks(), 2);
    assert_eq!(fetcher.bytes(&object).unwrap(), body);
}

#[test]
fn a_tree_with_a_file_gone_is_a_miss() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let objects = Objects::new(store);
    let tree = Tree {
        files: vec![
            TreeFile { path: "a".to_owned(), kind: Kind::File, bytes: b"kept".to_vec() },
            TreeFile { path: "b/c".to_owned(), kind: Kind::Exec, bytes: b"collected".to_vec() },
        ],
    };
    let listing = crate::store::listing_of(&tree, &Producer::Given("a test".to_owned())).unwrap();
    let pin = Digest::of(Algo::Blake3, &listing);
    let object =
        Object { blake3: Blake3::of(&listing), len: listing.len() as u64, form: Form::Tree };
    // The listing and its first file are kept, the second is not.
    let mut value = vec![INLINE];
    value.extend_from_slice(&listing);
    store.put(object_key(object.blake3), Stored::Output, &value).unwrap();
    store
        .put(object_key(Blake3::of(b"kept")), Stored::Output, &[&[INLINE][..], b"kept"].concat())
        .unwrap();
    let origin = Origin::Pinned { pin: &pin, form: Form::Tree };
    objects.record(&object, origin).unwrap();
    assert_eq!(objects.lookup(origin).unwrap(), None);
    let from = Producer::Given("a test".to_owned());
    let kept =
        crate::ensure(objects, &pin, Form::Tree, || Ok(Produced { made: Made::Tree(&tree), from }))
            .unwrap();
    assert_eq!(kept, object);
    assert_eq!(objects.lookup(origin).unwrap(), Some(object));
}

#[test]
fn bytes_kept_wrong_are_never_given() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let objects = Objects::new(store);
    let pin = Digest::of(Algo::Sha256, b"right");
    let object = Object { blake3: Blake3::of(b"right"), len: 5, form: Form::Blob };
    // Under the key of `right`'s BLAKE3, other bytes of the same length.
    store
        .put(object_key(object.blake3), Stored::Output, &[&[INLINE][..], b"wrong"].concat())
        .unwrap();
    objects.record(&object, blob(&pin)).unwrap();
    let error = objects.read(&object).unwrap_err();
    assert!(error.to_string().contains("kept wrong"), "{error}");
}

#[test]
fn a_body_past_the_limit_is_stopped_and_not_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let body = b"sixteen bytes!!!";
    let net = Arc::new(Fake::new(body));
    let fetcher = fetcher(store, Arc::clone(&net), &dir.path().join("scratch")).with_most(8);
    let pin = Digest::of(Algo::Sha256, body);
    let error = fetcher.get(&url(pin)).unwrap_err();
    assert!(matches!(error, FetchError::TooBig { most: 8, .. }), "{error}");
    assert_eq!(net.asks(), 1);
    assert_eq!(Objects::new(store).lookup(blob(&pin)).unwrap(), None);
    assert_eq!(std::fs::read_dir(dir.path().join("scratch")).unwrap().count(), 0);
}

#[test]
fn a_url_is_said_with_no_credential() {
    assert_eq!(
        crate::redact("https://user:token@example.com/x.crate?X-Amz-Signature=abc#frag"),
        "https://example.com/x.crate"
    );
    assert_eq!(crate::redact("https://example.com/a?token=1"), "https://example.com/a");
    assert_eq!(crate::redact("https://example.com/a"), "https://example.com/a");
}

#[test]
fn a_listing_is_sorted_and_refuses_escapes() {
    let entry = |path: &str, kind| Entry {
        path: path.to_owned(),
        kind,
        blake3: Blake3::of(path.as_bytes()),
        len: path.len() as u64,
    };
    let mut one = [entry("b/c", Kind::File), entry("a", Kind::Exec), entry("b/a", Kind::Link)];
    let mut two = [entry("b/a", Kind::Link), entry("b/c", Kind::File), entry("a", Kind::Exec)];
    let listing = tree::listing(&mut one).unwrap();
    assert_eq!(listing, tree::listing(&mut two).unwrap());
    let text = String::from_utf8(listing.clone()).unwrap();
    let paths: Vec<_> = text.lines().skip(1).map(|it| it.rsplit(' ').next().unwrap()).collect();
    assert_eq!(paths, ["a", "b/a", "b/c"]);
    assert_eq!(tree::parse(&listing).unwrap().len(), 3);
    for bad in ["../x", "/x", "a//b", ".git/config", "a\nb"] {
        assert!(tree::listing(&mut [entry(bad, Kind::File)]).is_err(), "{bad:?}");
    }
    assert!(tree::listing(&mut [entry("a", Kind::File), entry("a", Kind::File)]).is_err());
    assert!(tree::listing(&mut [entry("a", Kind::File), entry("a/b", Kind::File)]).is_err());
    // Case aside, as a case-insensitive disk sees them: `A` a link and `a/b` written through it.
    assert!(tree::listing(&mut [entry("A", Kind::Link), entry("a/b", Kind::File)]).is_err());
    assert!(
        tree::listing(&mut [entry("Readme", Kind::File), entry("README", Kind::File)]).is_err()
    );
    assert!(tree::listing(&mut [entry("x/.GIT/config", Kind::File)]).is_err());
    // One directory spelled two ways is one directory on a case-insensitive disk, ASCII or not,
    // and in either Unicode normal form.
    let twice = |a: &str, b: &str| {
        matches!(
            tree::listing(&mut [entry(a, Kind::File), entry(b, Kind::File)]),
            Err(tree::TreeRefused::Twice { .. })
        )
    };
    assert!(twice("Dir/a", "dir/b"));
    assert!(twice("src/Ä/a", "src/ä/b"));
    assert!(twice("\u{e9}/a", "e\u{301}/b"));
    assert!(twice("\u{c9}", "e\u{301}"));
    assert!(twice("\u{3a3}", "\u{3c2}"));
    assert!(twice("a", "a"));
    assert!(!twice("dir/a", "dir/b"));
    assert!(!twice("a/x", "b/x"));
    assert!(matches!(
        tree::listing(&mut [entry("\u{c4}", Kind::Link), entry("\u{e4}/b", Kind::File)]),
        Err(tree::TreeRefused::Under { .. })
    ));
    // A listing no `listing` writes is refused on read, the same rules over it.
    let hex = Blake3::of(b"x").hex();
    for bad in [
        format!("blaze tree 2\nlink {hex} 1 A\nfile {hex} 1 a/b\n"),
        format!("blaze tree 2\nfile {hex} 1 README\nfile {hex} 1 Readme\n"),
        format!("blaze tree 2\nfile {hex} 01 a\n"),
        format!("blaze tree 2\nfile {hex} a\n"),
    ] {
        assert!(tree::parse(bad.as_bytes()).is_err(), "{bad:?}");
    }
    assert_eq!(
        tree::parse(format!("blaze tree 2\nfile {hex} 1 a b\n").as_bytes()).unwrap()[0].path,
        "a b"
    );
}

#[test]
fn a_request_takes_one_source_and_refuses_unknown_fields() {
    let pin = Digest::of(Algo::Sha256, b"x");
    let asked =
        crate::request::parse(&format!(r#"{{"url":"https://e.com/x","digest":"{pin}"}}"#)).unwrap();
    assert_eq!(asked.source, Source::Url { url: "https://e.com/x".to_owned(), digest: pin });
    assert_eq!((asked.most_bytes, asked.out), (None, None));
    let asked = crate::request::parse(&format!(
        r#"{{"url":"u","digest":"{pin}","most_bytes":9,"out":"d"}}"#
    ))
    .unwrap();
    assert_eq!((asked.most_bytes, asked.out.as_deref()), (Some(9), Some("d")));
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let asked = crate::request::parse(&format!(
        r#"{{"git":{{"repo":"https://e.com/r","rev":"{rev}"}},"digest":"{pin}"}}"#
    ))
    .unwrap();
    assert!(matches!(asked.source, Source::Git { .. }));
    for bad in [
        format!(r#"{{"url":"u","digest":"{pin}","sha256":"x"}}"#),
        format!(r#"{{"digest":"{pin}"}}"#),
        format!(r#"{{"url":"u","git":{{"repo":"r","rev":"{rev}"}},"digest":"{pin}"}}"#),
        format!(r#"{{"git":{{"repo":"r","rev":"main"}},"digest":"{pin}"}}"#),
        r#"{"url":"u","digest":"md5:900150983cd24fb0d6963f7d28e17f72"}"#.to_owned(),
        format!(r#"{{"url":"u","digest":"{pin}","most_bytes":0}}"#),
    ] {
        assert!(crate::request::parse(&bad).is_err(), "{bad}");
    }
}

/// A git driver that checks out `files` into the scratch directory it is given.
struct FakeGit {
    files: Vec<(&'static str, &'static [u8])>,
}

impl crate::Git for FakeGit {
    fn checkout(
        &self,
        _repo: &str,
        _rev: &str,
        scratch: &Path,
    ) -> Result<std::path::PathBuf, String> {
        let root = scratch.join("checkout");
        for (path, bytes) in &self.files {
            let at = root.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, bytes).unwrap();
        }
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"not the tree's").unwrap();
        Ok(root)
    }
}

const REV: &str = "0123456789abcdef0123456789abcdef01234567";

/// Every value of `object` in `store` (its own, each chunk's), read as the store keeps them.
fn values(store: &Handle, object: &Object) -> Vec<Vec<u8>> {
    let read =
        |key| store.read(&key, |entry| entry.bytes.to_vec()).unwrap().expect("the store has it");
    let own = read(object_key(object.blake3));
    let mut out = vec![own.clone()];
    if own[0] == CHUNKED {
        for chunk in own[1..].chunks_exact(40) {
            out.push(read(chunk_key(Blake3(chunk[..32].try_into().unwrap()))));
        }
    }
    out
}

#[test]
fn a_chunked_object_and_a_tree_restore_as_a_fresh_store_builds_them() {
    let body: Vec<u8> = (0..2 * CHUNK_LEN + 3).map(|i| (i * 7 % 253) as u8).collect();
    let url_pin = Digest::of(Algo::Sha256, &body);
    let git =
        || Arc::new(FakeGit { files: vec![("src/lib.rs", b"pub fn f() {}"), ("README", b"hi")] });
    let tree_files = Tree {
        files: vec![
            TreeFile {
                path: "src/lib.rs".to_owned(),
                kind: Kind::File,
                bytes: b"pub fn f() {}".to_vec(),
            },
            TreeFile { path: "README".to_owned(), kind: Kind::File, bytes: b"hi".to_vec() },
        ],
    };
    let listing =
        crate::store::listing_of(&tree_files, &Producer::Given("a test".to_owned())).unwrap();
    let tree_pin = Digest::of(Algo::Blake3, &listing);
    let git_source = Source::Git {
        repo: "https://example.com/r".to_owned(),
        rev: REV.to_owned(),
        digest: tree_pin,
    };
    // Kept, flushed and opened again.
    let kept_dir = tempfile::tempdir().unwrap();
    {
        let (mut driven, store) = store(kept_dir.path());
        let fetcher = fetcher(store, Arc::new(Fake::new(&body)), &kept_dir.path().join("scratch"))
            .with_git(git());
        fetcher.get(&url(url_pin)).unwrap();
        fetcher.get(&git_source).unwrap();
        driven.flush().unwrap();
    }
    let (_driven, restored) = store(kept_dir.path());
    // Built fresh, in a store of its own.
    let fresh_dir = tempfile::tempdir().unwrap();
    let (_fresh_driven, fresh) = crate::tests::store(fresh_dir.path());
    let fresh_fetcher =
        fetcher(fresh, Arc::new(Fake::new(&body)), &fresh_dir.path().join("scratch"))
            .with_git(git());
    let blob = fresh_fetcher.get(&url(url_pin)).unwrap();
    let tree = fresh_fetcher.get(&git_source).unwrap();
    // Offline: the restored store answers both, with every value equal byte for byte.
    let net = Arc::new(Fake { fail: 99, ..Fake::new(b"") });
    let restored_fetcher = fetcher(restored, Arc::clone(&net), &kept_dir.path().join("scratch"));
    assert_eq!(restored_fetcher.get(&url(url_pin)).unwrap(), blob);
    assert_eq!(restored_fetcher.get(&git_source).unwrap(), tree);
    assert_eq!(net.asks(), 0);
    assert_eq!(values(restored, &blob).len(), 4, "the list and three chunks");
    assert_eq!(values(restored, &blob), values(fresh, &blob));
    assert_eq!(values(restored, &tree), values(fresh, &tree));
    for file in &tree_files.files {
        let object = Object {
            blake3: Blake3::of(&file.bytes),
            len: file.bytes.len() as u64,
            form: Form::Blob,
        };
        assert_eq!(values(restored, &object), values(fresh, &object));
    }
    assert_eq!(restored_fetcher.bytes(&blob).unwrap(), body);
    let out = restored_fetcher.materialize(&tree, &kept_dir.path().join("out")).unwrap();
    assert_eq!(std::fs::read(out.join("src/lib.rs")).unwrap(), b"pub fn f() {}");
    assert!(!out.join(".git").exists());
}

#[test]
fn a_checkout_past_the_limit_is_refused_and_not_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let git = Arc::new(FakeGit { files: vec![("big", &[7u8; 64][..])] });
    let fetcher = fetcher(store, Arc::new(Fake::new(b"")), &dir.path().join("scratch"))
        .with_git(git)
        .with_most(32);
    let pin = Digest::of(Algo::Blake3, b"whatever");
    let source =
        Source::Git { repo: "https://example.com/r".to_owned(), rev: REV.to_owned(), digest: pin };
    let error = fetcher.get(&source).unwrap_err();
    assert!(matches!(error, FetchError::TooBig { most: 32, .. }), "{error}");
    assert_eq!(Objects::new(store).lookup(source.origin()).unwrap(), None);
    assert_eq!(std::fs::read_dir(dir.path().join("scratch")).unwrap().count(), 0);
}

#[test]
fn materialize_writes_through_no_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("part");
    std::fs::create_dir(&root).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("a")).unwrap();
    #[cfg(unix)]
    assert!(crate::fetcher::directories_under(&root, "a/b").is_err());
    std::fs::write(root.join("f"), b"a file").unwrap();
    assert!(crate::fetcher::directories_under(&root, "f/b").is_err());
    assert_eq!(crate::fetcher::directories_under(&root, "d/e/g").unwrap(), root.join("d/e/g"));
    assert!(root.join("d/e").is_dir());
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn a_url_in_any_message_is_scrubbed() {
    assert_eq!(
        crate::scrub("asking https://u:p@h.example/x?X-Amz-Signature=s: reset; see `git://t@g/r`"),
        "asking https://h.example/x reset; see `git://g/r`"
    );
    assert_eq!(crate::scrub("no address"), "no address");
}

#[test]
fn a_request_is_checked_one_way_wherever_it_comes_from() {
    let pin = Digest::of(Algo::Sha256, b"x").to_string();
    let from_fields = |url: Option<&str>, repo: Option<&str>, rev: Option<&str>, most| {
        crate::request::from_fields(
            url.map(str::to_owned),
            repo.map(str::to_owned),
            rev.map(str::to_owned),
            &pin,
            most,
            None,
        )
    };
    assert!(from_fields(Some("https://e.com/x"), None, None, None).is_ok());
    assert!(from_fields(Some("https://e.com/x"), None, None, Some(0)).is_err());
    assert!(from_fields(None, Some("https://e.com/r"), None, None).is_err());
    assert!(from_fields(None, Some("https://e.com/r"), Some("main"), None).is_err());
    assert!(from_fields(Some("u"), Some("r"), Some(REV), None).is_err());
    assert!(from_fields(None, None, None, None).is_err());
}

/// One answer of [`Cutting`]: whether it keeps to the range asked, where it cuts the transfer
/// (past the body's end: it ends whole), and whether it lies, saying the range asked while
/// sending the body from its start (the planted fault the pin must catch).
#[derive(Clone, Copy, Debug)]
struct Answer {
    ranged: bool,
    cut: usize,
    lies: bool,
}

/// A server that answers by a script, then whole and true; it counts the bytes it sends.
struct Cutting {
    body: Vec<u8>,
    script: Vec<Answer>,
    asked: std::sync::atomic::AtomicUsize,
    sent: std::sync::atomic::AtomicUsize,
}

impl Net for Cutting {
    fn get(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        from: u64,
        sink: &mut (dyn FnMut(u64, &[u8]) -> std::io::Result<()> + Send),
    ) -> Result<(), Failure> {
        let ask = self.asked.fetch_add(1, Ordering::SeqCst);
        let len = self.body.len();
        let answer = self.script.get(ask).copied().unwrap_or(Answer { ranged: true, cut: len, lies: false });
        let from = usize::try_from(from).unwrap().min(len);
        let start = if answer.ranged { from } else { 0 };
        let end = answer.cut.clamp(start, len);
        let (offset, bytes) = if answer.lies {
            (start, &self.body[..end - start])
        } else {
            (start, &self.body[start..end])
        };
        let mut at = offset as u64;
        for piece in bytes.chunks(5) {
            sink(at, piece).map_err(|e| Failure::Permanent(e.to_string()))?;
            at += piece.len() as u64;
            self.sent.fetch_add(piece.len(), Ordering::SeqCst);
        }
        if end < len {
            return Err(Failure::Transient("connection reset".to_owned()));
        }
        Ok(())
    }
}

/// Resuming a cut download, through any order of cuts, servers that ignore ranges and servers
/// that lie about them: the store only ever gets the pinned bytes (a lie is a mismatch, and its
/// partial goes), an honest server's body is got in the end with no partial left, and when
/// every answer keeps to its range and none lies no byte is sent twice: each try resumes where
/// the last stopped, even across `get`s (as across attempts of a build). With resume taken out
/// (a try from nothing) that last check fails on the first cut.
#[hegel::test]
fn a_cut_download_resumes_and_keeps_only_the_pinned_bytes(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let len: usize = tc.draw(gs::integers().min_value(1).max_value(200));
    let body: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
    let script: Vec<Answer> = tc
        .draw(
            gs::vecs(gs::tuples!(gs::booleans(), gs::integers().max_value(len + 10), gs::integers().max_value(9)))
                .max_size(12),
        )
        .into_iter()
        .map(|(ranged, cut, lie): (bool, usize, u8)| Answer { ranged, cut, lies: lie == 0 })
        .collect();
    let honest = script.iter().all(|it| it.ranged && !it.lies);
    let dir = tempfile::tempdir().unwrap();
    let (_driven, store) = store(dir.path());
    let net = Arc::new(Cutting {
        body: body.clone(),
        script: script.clone(),
        asked: std::sync::atomic::AtomicUsize::new(0),
        sent: std::sync::atomic::AtomicUsize::new(0),
    });
    let flights = Box::leak(Box::new(Flights::new()));
    let fetcher = Fetcher::new(StoreOf::Given(store), Arc::clone(&net) as Arc<dyn Net>, dir.path().join("scratch"))
        .with_policy(quick())
        .with_flights(flights);
    let pin = Digest::of(Algo::Sha256, &body);
    let mut got = None;
    // Each `get` is a build's attempt; the script runs out within 13 asks, and then the server
    // is whole and true.
    for _ in 0..16 {
        match fetcher.get(&url(pin)) {
            Ok(object) => {
                got = Some(object);
                break;
            }
            Err(FetchError::GaveUp { .. }) => {}
            Err(FetchError::Mismatch { .. }) => {
                assert!(script.iter().any(|it| it.lies), "a mismatch with no lie: {script:?}");
            }
            Err(other) => panic!("{other} ({script:?})"),
        }
    }
    let object = got.unwrap_or_else(|| panic!("never got the body: {script:?}"));
    assert_eq!(&*fetcher.bytes(&object).unwrap(), &body[..], "{script:?}");
    assert_eq!(std::fs::read_dir(dir.path().join("scratch")).unwrap().count(), 0, "a partial is left");
    if honest {
        assert_eq!(net.sent.load(Ordering::SeqCst), len, "a byte was sent twice: {script:?}");
    }
}
