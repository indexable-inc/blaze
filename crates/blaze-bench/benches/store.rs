//! The plain store's hot paths: the key a value is kept under, a put, and a read of the store that
//! finds a value (one `pread` into the caller's buffer) or does not, of a store committed to disk.
//!
//! Every benchmark does the same work on every run, so that `blaze_bench` can pair the binary of
//! a change with the binary of its base.

use std::hint::black_box;

use blaze_bench::{IntoBenchmarks, benchmark_fn};
use blaze_cas::Key;
use blaze_cas::plain::{Driven, Namespace, Settings, Tag};
use blaze_core::Native;
use blaze_ids::ContentId;
use blaze_own::Alloc;

/// What the store holds when the benchmarks read it: a thousand values of 512 bytes.
const COUNT: usize = 1000;
const LEN: usize = 512;

/// The tag every value is filed under.
const TAG: Tag = Tag(1);

const SCHEMA: blaze_format::SchemaHash = blaze_format::schema("blaze-bench store value 1");

fn producer() -> ContentId {
    ContentId::of("blaze-bench store producer 1", &[])
}

/// The key of the `n`th value.
fn key_of(n: u64) -> Key {
    Key::of(SCHEMA, producer(), &[ContentId::of("blaze-bench store input 1", &n.to_le_bytes())])
}

/// A plain store of its own under the temporary directory, holding [`COUNT`] values of [`LEN`]
/// bytes, committed.
struct Filled {
    driven: Driven<Native, Alloc<'static>>,
    keys: Vec<Key>,
    /// The store's directory, last so it is removed after the store closes.
    _dir: blaze_core::temp::TempDir,
}

impl Filled {
    fn new(name: &str) -> Filled {
        let dir = blaze_core::temp::TempDir::for_test(&format!("blaze-bench-store-{name}-"));
        let alloc = blaze_own::testing::heap().alloc();
        let mut driven = Driven::open(dir.path(), &Settings::default(), alloc).expect("a store");
        let keys: Vec<Key> = (0..COUNT as u64).map(key_of).collect();
        for (n, key) in keys.iter().enumerate() {
            driven.put(*key, Namespace::LOCAL, TAG, &vec![n as u8; LEN]).expect("a put");
        }
        driven.commit(&[]).expect("a commit");
        Filled { driven, keys, _dir: dir }
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
            let mut store = Filled::new("put");
            let key = key_of(u64::MAX - 1);
            let value = vec![0x5a; 1024];
            b.iter(move || store.driven.put(key, Namespace::LOCAL, TAG, black_box(&value)).is_ok())
        }),
        benchmark_fn("store/get_hit", |b| {
            let mut store = Filled::new("hit");
            let mut out = Vec::with_capacity(LEN);
            let mut at = 0;
            b.iter(move || {
                at = (at + 1) % COUNT;
                let key = store.keys[at];
                store.driven.get_into(black_box(&key), &mut out).expect("a whole store").map(|_| out.len())
            })
        }),
        benchmark_fn("store/has_miss", |b| {
            let mut store = Filled::new("miss");
            let key = key_of(u64::MAX);
            b.iter(move || store.driven.has(black_box(&key)).expect("a whole store"))
        }),
    ]
}

blaze_bench::main!(benchmarks());
