//! Tests of the API's contract, written before it was built: the
//! implementation's job is to make them pass. Expected answers come from
//! official BLAKE3 only: its test vectors (test_vectors/test_vectors.json:
//! input byte i is i % 251, every mode) and, for the inputs past them,
//! digests fixed in each test by a fold official BLAKE3 1.8.7 generated
//! (tests/common/golden.rs).
//!
//! Keep: it detects, by hanging, mutants the clock then stops
//! (tools/mutants/timeouts-x86_64.txt).

use blake3_servil::performance::{Batch, Block};
use blake3_servil::{Hash, Mode};

const KEY: &[u8; 32] = b"whats the Elvish word for friend";
const CONTEXT: &str = "BLAKE3 2019-12-27 16:29:52 test vectors context";

/// Input byte i is i % 251, as the official vectors define their inputs.
fn input(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// (input length, [hash, keyed hash, derived key]) for every official vector.
fn vectors() -> Vec<(usize, [[u8; 32]; 3])> {
    let text = include_str!("../test_vectors/test_vectors.json");
    let json: serde_json::Value = serde_json::from_str(text).expect("the vectors are JSON");
    assert_eq!(json["key"], std::str::from_utf8(KEY).unwrap());
    assert_eq!(json["context_string"], CONTEXT);
    let first32 = |hex_text: &serde_json::Value| -> [u8; 32] { hex::decode(&hex_text.as_str().unwrap()[..64]).unwrap().try_into().unwrap() };
    json["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| (case["input_len"].as_u64().unwrap() as usize, [first32(&case["hash"]), first32(&case["keyed_hash"]), first32(&case["derive_key"])]))
        .collect()
}

#[path = "common/golden.rs"]
mod golden;

/// A test's expected digests, fixed: `want(m, data)` is the digest of
/// `data` in mode `m` (0 hash, 1 keyed, 2 derive-key), one message at a
/// time, and the test's `check` holds every digest it gave to the constant
/// official BLAKE3 generated.
struct Fixed(golden::Golden);

impl Fixed {
    fn new(name: &'static str) -> Fixed {
        Fixed(golden::Golden::new(name))
    }

    fn want(&mut self, m: usize, data: &[u8]) -> [u8; 32] {
        let digest = *blake3_servil::convenience::hash_with(modes()[m], data).as_bytes();
        self.0.add(&digest, || {
            match m {
                0 => *blake3::hash(data).as_bytes(),
                1 => *blake3::keyed_hash(KEY, data).as_bytes(),
                _ => blake3::derive_key(CONTEXT, data),
            }
            .to_vec()
        });
        digest
    }

    fn check(self, want: u64) {
        self.0.check(want);
    }
}

fn modes() -> [Mode<'static>; 3] {
    [Mode::Plain, Mode::Keyed(KEY), Mode::DeriveKey(CONTEXT)]
}

/// Each one-shot call's full form (every mode), on `data` (its blocks for
/// the performance forms, a message as a batch of one).
const HASH_WITH: [(&str, fn(Mode, &[u8]) -> Hash); 3] = [
    ("convenience::hash_with", blake3_servil::convenience::hash_with),
    ("performance::hash", |mode, data| blake3_servil::performance::hash(mode, message(data).blocks(), data.len())),
    ("performance::hash_multithreaded", |mode, data| blake3_servil::performance::hash_multithreaded(mode, message(data).blocks(), data.len())),
];

/// `data` as a message in blocks: a batch of one.
fn message(data: &[u8]) -> Batch<'static> {
    let mut batch = Batch::new(data.len(), 1);
    batch.set(0, data);
    batch
}
const HASH_MANY_WITH: [(&str, fn(Mode, &Batch, &mut [[u8; 32]])); 2] = [
    ("performance::hash_many", blake3_servil::performance::hash_many),
    ("performance::hash_many_multithreaded", blake3_servil::performance::hash_many_multithreaded),
];

/// Lengths past the official vectors, around the multithreaded split (512 KiB) and a subtree edge.
const LARGE: [usize; 5] = [(512 << 10) - 1, 512 << 10, 1 << 20, (1 << 20) + 1, (3 << 20) + 12345];

// ---------- One-shot, one message ----------

#[test]
fn one_message_every_form_matches_the_vectors() {
    blake3_servil::initialize_multithreaded(None);
    for (len, expected) in vectors() {
        let data = input(len);
        assert_eq!(blake3_servil::convenience::hash(&data), Hash::from(expected[0]), "hash, {len} bytes");
        assert_eq!(
            blake3_servil::performance::hash_multithreaded(blake3_servil::Mode::Plain, message(&data).blocks(), len),
            Hash::from(expected[0]),
            "hash_multithreaded, {len} bytes"
        );
        assert_eq!(blake3_servil::convenience::hash_with(Mode::Keyed(KEY), &data), Hash::from(expected[1]), "keyed_hash, {len} bytes");
        assert_eq!(*blake3_servil::convenience::hash_with(Mode::DeriveKey(CONTEXT), &data).as_bytes(), expected[2], "derive_key, {len} bytes");
        for (m, mode) in modes().into_iter().enumerate() {
            for (name, form) in HASH_WITH {
                assert_eq!(form(mode, &data), Hash::from(expected[m]), "{name} mode {m}, {len} bytes");
            }
        }
    }
}

/// A message in pieces through `Hasher::update_multithreaded` (and mixed
/// with `update`), in every mode: streams of 64 KiB pieces long enough to
/// keep the workers ready between updates (past the first 128 KiB), pieces
/// that leave a chunk part-filled, and pieces past the multithreaded split,
/// against the reference implementation.
#[test]
fn a_message_in_pieces_matches_the_fixed_values() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan pieces");
    let hashers = |m: usize| match m {
        0 => blake3_servil::performance::Hasher::new(blake3_servil::Mode::Plain),
        1 => blake3_servil::performance::Hasher::new(Mode::Keyed(KEY)),
        _ => blake3_servil::performance::Hasher::new(Mode::DeriveKey(CONTEXT)),
    };
    let cases: [(usize, usize); 8] = [
        (64 << 10, 64 << 10),
        (2 << 20, 64 << 10),
        ((2 << 20) + 777, 64 << 10),
        (1 << 20, 1000),
        ((1 << 20) + 5, (64 << 10) + 1),
        (3 << 20, 100_000),
        (4 << 20, 1 << 20),
        (5 << 20, 192 << 10),
    ];
    for (len, piece) in cases {
        let data = input(len);
        for m in 0..3 {
            let expected = Hash::from(fixed.want(m, &data));
            let mut hasher = hashers(m);
            for part in data.chunks(piece) {
                hasher.update_multithreaded(part);
            }
            assert_eq!(hasher.finalize(), expected, "update_multithreaded, {len} B in {piece} B pieces, mode {m}");
            // Alternating with update, a piece at a time.
            let mut hasher = hashers(m);
            for (k, part) in data.chunks(piece).enumerate() {
                if k % 3 == 1 {
                    hasher.update(part);
                } else {
                    hasher.update_multithreaded(part);
                }
            }
            assert_eq!(hasher.finalize(), expected, "update and update_multithreaded, {len} B in {piece} B pieces, mode {m}");
        }
    }
    fixed.check(0x575c859901743801);
}

#[test]
fn one_message_large_inputs_match_the_fixed_values() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan large");
    for len in LARGE {
        let data = input(len);
        assert_eq!(
            blake3_servil::performance::hash_multithreaded(blake3_servil::Mode::Plain, message(&data).blocks(), len),
            Hash::from(fixed.want(0, &data)),
            "hash_multithreaded, {len} bytes"
        );
        for (m, mode) in modes().into_iter().enumerate() {
            for (name, form) in HASH_WITH {
                assert_eq!(form(mode, &data), Hash::from(fixed.want(m, &data)), "{name} mode {m}, {len} bytes");
            }
        }
    }
    fixed.check(0x9c470e731760fb6d);
}

// ---------- One-shot, batches ----------

/// `count` messages of `len` bytes each, message i the official input
/// pattern starting at i (so messages differ), laid out by the padded batch
/// contract: slot i at i x stride, stride len rounded up to 64 (64 for 0),
/// zeros between a message's end and the next slot.
fn padded_batch(count: usize, len: usize) -> (Batch<'static>, Vec<Vec<u8>>) {
    let mut batch = Batch::new(len, count);
    let messages: Vec<Vec<u8>> = (0..count).map(|i| (0..len).map(|j| ((i + j) % 251) as u8).collect()).collect();
    for (i, message) in messages.iter().enumerate() {
        batch.set(i, message);
    }
    (batch, messages)
}

#[test]
fn batches_every_form_match_the_fixed_values() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan batches");
    for (count, len) in [
        (1, 64),
        (2, 64),
        (4, 64),
        (12, 64),
        (16, 64),
        (17, 64),
        (1000, 64),
        (12288, 64),
        (13000, 64),
        (5, 0),
        (7, 1),
        (9, 63),
        (33, 100),
        (20, 1024),
        (3, 1025),
        (40, 4000),
        (2, 70000),
        (20, 60000),
    ] {
        let (buffer, messages) = padded_batch(count, len);
        let expected: Vec<Vec<[u8; 32]>> = (0..3).map(|m| messages.iter().map(|message| fixed.want(m, message)).collect()).collect();
        let mut out = vec![[0u8; 32]; count];
        blake3_servil::performance::hash_many(blake3_servil::Mode::Plain, &buffer, &mut out);
        assert_eq!(out, expected[0], "hash_many, {count} x {len} B");
        out.fill([0; 32]);
        blake3_servil::performance::hash_many_multithreaded(blake3_servil::Mode::Plain, &buffer, &mut out);
        assert_eq!(out, expected[0], "hash_many_multithreaded, {count} x {len} B");
        for (m, mode) in modes().into_iter().enumerate() {
            for (name, form) in HASH_MANY_WITH {
                out.fill([0; 32]);
                form(mode, &buffer, &mut out);
                assert_eq!(out, expected[m], "{name} mode {m}, {count} x {len} B");
            }
        }
    }
    fixed.check(0x2ae4ad5b866b2797);
}

#[test]
#[should_panic]
fn a_batch_whose_buffer_is_short_of_its_messages_is_refused() {
    blake3_servil::initialize_multithreaded(None);
    let mut out = [[0u8; 32]; 2];
    blake3_servil::performance::hash_many(blake3_servil::Mode::Plain, &Batch::from_blocks(&[Block::ZERO], 64), &mut out);
}

// ---------- The stream ----------

fn zeros(n: usize) -> Vec<Hash> {
    vec![Hash::from_bytes([0; 32]); n]
}

/// Messages of many lengths, the vectors' and larger, borrowed, written in
/// place, and read: each digest in its place, every place handed back
/// once, in submission order, in every mode.
#[test]
fn a_stream_puts_every_digest_in_its_place_and_hands_the_places_back_in_order() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan stream_places");
    let lens: Vec<usize> = vectors().iter().map(|(len, _)| *len).chain(LARGE).collect();
    let messages: Vec<Vec<u8>> = lens.iter().map(|&len| input(len)).collect();
    let in_blocks: Vec<Batch> = messages.iter().map(|m| message(m)).collect();
    for (m, mode) in modes().into_iter().enumerate() {
        let mut places = zeros(messages.len());
        let want_order: Vec<usize> = places.iter().map(|p| p as *const Hash as usize).collect();
        let mut order = Vec::new();
        blake3_servil::performance::stream(
            mode,
            |done: &mut [Hash]| order.extend(done.iter().map(|p| p as *const Hash as usize)),
            |s| {
                for (i, (message, place)) in messages.iter().zip(&mut places).enumerate() {
                    match i % 3 {
                        0 => s.submit(in_blocks[i].blocks(), message.len(), place),
                        1 => s.submit_with(
                            message.len(),
                            |at, room| {
                                room.copy_from_slice(&message[at..][..room.len()]);
                                room.len()
                            },
                            place,
                        ),
                        _ => assert_eq!(s.submit_reader(&message[..], Some(message.len() as u64), place).unwrap(), message.len() as u64),
                    }
                }
                s.finish();
            },
        );
        assert!(order == want_order, "every place handed back once, in order (mode {m})");
        for (i, message) in messages.iter().enumerate() {
            assert_eq!(*places[i].as_bytes(), fixed.want(m, message), "message {i}, {} bytes, mode {m}", message.len());
        }
    }
    fixed.check(0x2fdca7f8f6c8b56f);
}

/// Batches of fixed-length messages, borrowed and written in place: one
/// digest per message in the caller's place, the place handed back whole.
#[test]
fn a_streams_batches_fill_the_callers_places() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan stream_batches");
    let cases = [(64, 1), (64, 4096), (100, 7), (1024, 300), (1500, 33)];
    let batches: Vec<(Batch, Vec<Vec<u8>>)> = cases.iter().map(|&(len, count)| padded_batch(count, len)).collect();
    for (m, mode) in modes().into_iter().enumerate() {
        let mut places: Vec<Vec<Hash>> = cases.iter().map(|&(_, count)| zeros(count)).collect();
        let mut handed = Vec::new();
        blake3_servil::performance::stream(
            mode,
            |done: &mut [Hash]| handed.push(done.len()),
            |s| {
                for (k, ((&(len, _), (buffer, _)), place)) in cases.iter().zip(&batches).zip(&mut places).enumerate() {
                    if k % 2 == 0 {
                        s.submit_batch(buffer, place);
                    } else {
                        s.submit_batch_with(
                            len,
                            |at, room| {
                                room.copy_from_slice(&Block::bytes(buffer.blocks())[at..][..room.len()]);
                                room.len()
                            },
                            place,
                        );
                    }
                }
                s.finish();
            },
        );
        assert_eq!(handed, cases.iter().map(|&(_, count)| count).collect::<Vec<_>>(), "each batch's place back whole, in order");
        for (k, (_, messages)) in batches.iter().enumerate() {
            for (i, message) in messages.iter().enumerate() {
                assert_eq!(*places[k][i].as_bytes(), fixed.want(m, message), "batch {k} message {i}, mode {m}");
            }
        }
    }
    fixed.check(0x9901b3bce75924b7);
}

/// Bursts of large batches with pauses between them, long enough for the
/// pool's workers to park: every batch completes.
#[test]
fn bursts_of_large_batches_after_pauses_complete() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan bursts");
    let (buffer, messages) = padded_batch(4096, 64);
    let expected: Vec<[u8; 32]> = messages.iter().map(|message| fixed.want(0, message)).collect();
    let mut places = vec![zeros(4096); 4];
    for burst in 0..400 {
        blake3_servil::performance::stream(
            Mode::Plain,
            |_: &mut [Hash]| {},
            |s| {
                for place in places.iter_mut() {
                    s.submit_batch(&buffer, place);
                }
                s.finish();
            },
        );
        for place in &places {
            assert!(place.iter().map(|h| *h.as_bytes()).eq(expected.iter().copied()), "burst {burst}");
        }
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
    fixed.check(0x7ede5248630d5d6a);
}

/// A stream that runs on reuses a fixed set of places: four cycle through
/// the handler (a free list the handler fills and the body takes from)
/// until 1000 messages are hashed, each digest read as its place comes back.
#[test]
fn a_stream_cycles_a_fixed_set_of_places() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan cycled_places");
    let messages: Vec<Vec<u8>> = (0..1000).map(|i| input(1 + i * 7 % 3000)).collect();
    let in_blocks: Vec<Batch> = messages.iter().map(|m| message(m)).collect();
    let mut places = zeros(4);
    let free = std::cell::RefCell::new(places.iter_mut().collect::<Vec<_>>());
    let got = std::cell::RefCell::new(Vec::new());
    blake3_servil::performance::stream(
        Mode::Plain,
        |done: &mut [Hash]| {
            got.borrow_mut().push(*done[0].as_bytes());
            free.borrow_mut().push(&mut done[0]);
        },
        |s| {
            for (k, message) in messages.iter().enumerate() {
                let place = loop {
                    if let Some(place) = free.borrow_mut().pop() {
                        break place;
                    }
                    s.collect();
                };
                s.submit(in_blocks[k].blocks(), message.len(), place);
            }
            s.finish();
        },
    );
    let got = got.into_inner();
    assert_eq!(got.len(), messages.len());
    for (i, message) in messages.iter().enumerate() {
        assert_eq!(got[i], fixed.want(0, message), "message {i}");
    }
    fixed.check(0xa66bc6c0375fe2db);
}

/// Streams on several threads at once, in different modes, share the
/// workers and keep their own order.
#[test]
fn streams_on_several_threads_keep_their_own_order() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan threads");
    let messages: Vec<Vec<Vec<u8>>> = (0..4).map(|t| (0..300).map(|i| input((i * 131 + t * 17) % 70_000)).collect()).collect();
    let wants: Vec<Vec<[u8; 32]>> = messages.iter().enumerate().map(|(t, ms)| ms.iter().map(|m| fixed.want(t % 3, m)).collect()).collect();
    std::thread::scope(|scope| {
        for t in 0..4usize {
            let (messages, wants) = (&messages[t], &wants[t]);
            scope.spawn(move || {
                let in_blocks: Vec<Batch> = messages.iter().map(|m| message(m)).collect();
                let m = t % 3;
                let mut places = zeros(messages.len());
                blake3_servil::performance::stream(
                    modes()[m],
                    |_: &mut [Hash]| {},
                    |s| {
                        for ((message, blocks), place) in messages.iter().zip(&in_blocks).zip(&mut places) {
                            s.submit(blocks.blocks(), message.len(), place);
                        }
                        s.finish();
                    },
                );
                for (i, want) in wants.iter().enumerate() {
                    assert_eq!(places[i].as_bytes(), want, "thread {t} message {i}");
                }
            });
        }
    });
    fixed.check(0x0e566a20c0cfb109);
}

/// A handler that panics unwinds on the caller's thread, after the threads
/// are done with the stream's messages; the pool goes on serving.
#[test]
fn a_panicking_handler_unwinds_and_the_pool_goes_on() {
    blake3_servil::initialize_multithreaded(None);
    let mut fixed = Fixed::new("api_plan after_a_panic");
    let bytes = input(100_000);
    let blocks = message(&bytes);
    let result = std::panic::catch_unwind(|| {
        let mut places = zeros(8);
        blake3_servil::performance::stream(
            Mode::Plain,
            |_: &mut [Hash]| panic!("a handler's panic"),
            |s| {
                for place in places.iter_mut() {
                    s.submit(blocks.blocks(), 100_000, place);
                }
                s.finish();
            },
        );
    });
    assert!(result.is_err(), "the handler's panic reaches the caller");
    let mut place = Hash::from_bytes([0; 32]);
    blake3_servil::performance::stream(
        Mode::Plain,
        |_: &mut [Hash]| {},
        |s| {
            s.submit(blocks.blocks(), 100_000, &mut place);
            s.finish();
        },
    );
    assert_eq!(*place.as_bytes(), fixed.want(0, &bytes));
    assert_eq!(
        *blake3_servil::performance::hash_multithreaded(blake3_servil::Mode::Plain, message(&input(3 << 20)).blocks(), 3 << 20).as_bytes(),
        fixed.want(0, &input(3 << 20))
    );
    fixed.check(0xf68c9575d37348b1);
}
