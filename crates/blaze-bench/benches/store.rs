//! The store's hot paths: the key a value is kept under, a put, and a read of the store that finds a value (checked against its content hash) or does
//! not, of a store committed to disk.
//!
//! Every benchmark does the same work on every run, so that `blaze_bench` can pair the binary of
//! a change with the binary of its base.

use std::hint::black_box;

use blaze_cas::{Driven, Handle, Key, Kind};
use blaze_ids::ContentId;
use blaze_bench::{IntoBenchmarks, benchmark_fn};

/// What the store holds when the benchmarks read it: a thousand values of 512 bytes.
const COUNT: usize = 1000;
const LEN: usize = 512;

const SCHEMA: blaze_format::SchemaHash = blaze_format::schema("blaze-bench store value 1");

fn producer() -> ContentId {
    ContentId::of("blaze-bench store producer 1", &[])
}

/// The key of the `n`th value.
fn key_of(n: u64) -> Key {
    Key::of(SCHEMA, producer(), &[ContentId::of("blaze-bench store input 1", &n.to_le_bytes())])
}

/// A store of its own under the temporary directory, holding [`COUNT`] values of [`LEN`] bytes,
/// committed.
struct Filled {
    _driven: Driven,
    handle: Handle,
    keys: Vec<Key>,
}

impl Filled {
    fn new(name: &str) -> Filled {
        let dir = std::env::temp_dir().join(format!("blaze-bench-store-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let mut driven = Driven::open(&dir).expect("a store");
        let handle = driven.handle();
        let keys: Vec<Key> = (0..COUNT as u64).map(key_of).collect();
        for (n, key) in keys.iter().enumerate() {
            handle.put(*key, Kind::Query, vec![n as u8; LEN]).expect("a put");
        }
        driven.flush().expect("a commit");
        Filled { _driven: driven, handle, keys }
    }
}

fn benchmarks() -> impl IntoBenchmarks {
    [
        benchmark_fn("key/of", |b| {
            let producer = producer();
            let input = ContentId::of("blaze-bench store input 1", b"an input");
            b.iter(move || Key::of(SCHEMA, producer, black_box(&[input])))
        }),
        benchmark_fn("store/put_again_1k", |b| {
            let store = Filled::new("put");
            let key = key_of(u64::MAX - 1);
            let value = vec![0x5a; 1024];
            b.iter(move || store.handle.put(key, Kind::Query, black_box(value.clone())).is_ok())
        }),
        benchmark_fn("store/get_hit", |b| {
            let store = Filled::new("hit");
            let mut at = 0;
            b.iter(move || {
                at = (at + 1) % COUNT;
                let key = store.keys[at];
                store.handle.read(black_box(&key), |it| it.bytes.len()).expect("a whole store")
            })
        }),
        benchmark_fn("store/has_miss", |b| {
            let store = Filled::new("miss");
            let key = key_of(u64::MAX);
            b.iter(move || store.handle.has(black_box(&key)))
        }),
    ]
}

blaze_bench::main!(benchmarks());
