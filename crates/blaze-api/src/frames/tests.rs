use blaze_ids::rng::SplitMix64 as Rng;

use super::*;
use crate::{Request, Response};

fn item(id: u64, request: Request) -> Vec<u8> {
    blaze_protocol::cbor::to_vec(&Envelope::new(id, request)).unwrap()
}

#[test]
fn an_encoded_envelope_is_a_frame_of_its_item() {
    let envelope = Envelope::new(9, Request::Status);
    let frame = encode(&envelope).unwrap();
    let item = item(9, Request::Status);
    assert_eq!(frame[..4], (item.len() as u32).to_be_bytes());
    assert_eq!(frame[4..], item);
    let back: Envelope<Request> = decode(&frame[4..]).unwrap();
    assert_eq!(back, envelope);
}

#[test]
fn items_come_out_whole_whatever_the_cuts() {
    let mut stream = Vec::new();
    let mut items = Vec::new();
    for id in 0..5 {
        let envelope = Envelope::new(id, Request::Cancel { id: id * 1000 });
        stream.extend(encode(&envelope).unwrap());
        items.push(item(id, Request::Cancel { id: id * 1000 }));
    }
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut frames = Frames::new();
        let mut out = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let len = (rng.below(40) as usize).min(stream.len() - at);
            frames.push(&stream[at..at + len]);
            at += len;
            while let Some(item) = frames.next().unwrap() {
                out.push(item);
            }
        }
        assert_eq!(out, items, "seed {seed}");
        assert_eq!(frames.held(), 0, "seed {seed}");
    }
}

/// The frames as they were, all they hold in one `Vec` cut to its length: the reference the
/// kept, zeroed-once buffer is held to.
#[derive(Default)]
struct Model {
    bytes: Vec<u8>,
}

impl Model {
    fn next(&mut self) -> Option<Vec<u8>> {
        let len = u32::from_be_bytes(self.bytes.get(..4)?.try_into().unwrap()) as usize;
        if self.bytes.len() < 4 + len {
            return None;
        }
        let item = self.bytes[4..4 + len].to_vec();
        self.bytes.drain(..4 + len);
        Some(item)
    }

    fn wanted(&self, least: usize) -> usize {
        let held = self.bytes.len();
        let Some(head) = self.bytes.get(..4) else { return least };
        let len = u32::from_be_bytes(head.try_into().unwrap());
        if len > LONGEST_REQUEST {
            return least;
        }
        (4 + len as usize).saturating_sub(held).min(held).max(least)
    }

    fn ready(&self) -> bool {
        let Some(head) = self.bytes.get(..4) else { return false };
        let len = u32::from_be_bytes(head.try_into().unwrap());
        len > LONGEST_REQUEST || self.bytes.len() >= 4 + len as usize
    }
}

/// A reader that gives at most `most` bytes a read.
struct Trickle<'a> {
    bytes: &'a [u8],
    most: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        let len = into.len().min(self.most).min(self.bytes.len());
        into[..len].copy_from_slice(&self.bytes[..len]);
        self.bytes = &self.bytes[len..];
        Ok(len)
    }
}

/// Pushes, adopted buffers and fills, mixed and cut anywhere, hand out what the old frames
/// did, and say the same of what they hold and want.
#[test]
fn every_way_in_gives_what_the_old_frames_gave() {
    for seed in 0..400 {
        let mut rng = Rng::new(seed);
        let mut stream = Vec::new();
        for id in 0..(1 + rng.below(12)) {
            let request = match rng.below(3) {
                0 => Request::Status,
                1 => Request::Cancel { id: rng.below(u64::MAX) },
                _ => Request::Close { root: blaze_ids::ContentId([rng.below(256) as u8; 32]) },
            };
            stream.extend(encode(&Envelope::new(id, request)).unwrap());
        }
        let (mut frames, mut model) = (Frames::new(), Model::default());
        let mut at = 0;
        while at < stream.len() {
            let len = (rng.below(90) as usize).min(stream.len() - at);
            let piece = &stream[at..at + len];
            match rng.below(4) {
                0 => frames.push(piece),
                1 => frames.push_vec(piece.to_vec()),
                2 => {
                    let mut into = frames.lend();
                    assert_eq!(into.len(), model.bytes.len(), "seed {seed}");
                    into.extend_from_slice(piece);
                    assert_eq!(frames.give_back(into), !piece.is_empty(), "seed {seed}");
                }
                _ => {
                    let most = 1 + rng.below(200) as usize;
                    let mut from = Trickle { bytes: piece, most: 1 + rng.below(50) as usize };
                    let mut got = 0;
                    while got < len {
                        got += frames.fill(&mut from, most.min(len - got)).unwrap();
                    }
                }
            }
            model.bytes.extend_from_slice(piece);
            at += len;
            assert_eq!(frames.held(), model.bytes.len(), "seed {seed}");
            assert_eq!(frames.wanted(64), model.wanted(64), "seed {seed}");
            assert_eq!(frames.ready(), model.ready(), "seed {seed}");
            while let Some(item) = frames.next_ref().unwrap() {
                assert_eq!(Some(item.to_vec()), model.next(), "seed {seed}");
                assert_eq!(frames.ready(), model.ready(), "seed {seed}");
            }
            assert_eq!(model.next(), None, "seed {seed}");
            assert!(!frames.ready(), "seed {seed}");
        }
        assert_eq!(frames.held(), 0, "seed {seed}");
    }
}

/// Once its buffer has grown, a fill reads into it with no allocation and no zeroing.
#[test]
fn a_warm_fill_allocates_nothing() {
    let frame = encode(&Envelope::new(4, Request::Status)).unwrap();
    let mut frames = Frames::new();
    frames.fill(&mut &frame[..], 64 << 10).unwrap();
    assert!(frames.next_ref().unwrap().is_some());
    for _ in 0..100 {
        let read = blaze_alloc_count::assert_at_most(0, "a fill of a warm buffer", || {
            frames.fill(&mut &frame[..], 64 << 10).unwrap()
        });
        assert_eq!(read, frame.len());
        assert_eq!(frames.next_ref().unwrap(), Some(&frame[4..]));
    }
}

/// A buffer lent to each receive and given back with what it read is the frames' own from then
/// on: once it has grown, a frame cut over two receives allocates nothing, its start kept at
/// the buffer's front, and a receive that read nothing says the peer hung up.
#[test]
fn a_warm_lent_buffer_allocates_nothing() {
    let frame = encode(&Envelope::new(4, Request::Cancel { id: 11 })).unwrap();
    let (first, second) = frame.split_at(frame.len() / 2);
    let mut frames = Frames::new();
    let mut into = frames.lend();
    into.reserve(64 << 10);
    into.extend_from_slice(&frame);
    assert!(frames.give_back(into));
    assert_eq!(frames.next_ref().unwrap(), Some(&frame[4..]));
    blaze_alloc_count::assert_at_most(0, "warm receives into a lent buffer", || {
        for _ in 0..100 {
            let mut into = frames.lend();
            assert!(into.is_empty());
            into.extend_from_slice(first);
            assert!(frames.give_back(into));
            assert_eq!(frames.next_ref().unwrap(), None);
            let mut into = frames.lend();
            assert_eq!(&into[..], first);
            into.extend_from_slice(second);
            assert!(frames.give_back(into));
            assert_eq!(frames.next_ref().unwrap(), Some(&frame[4..]));
        }
    });
    let into = frames.lend();
    assert!(!frames.give_back(into));
}

/// The whole way of a request that owns nothing, warm: encoded onto a kept buffer by a kept
/// writer, pushed, cut out in place and decoded, it allocates nothing.
#[test]
fn a_warm_round_of_a_frame_allocates_nothing() {
    let (mut writer, mut out, mut frames) = (cbor::Writer::new(), Vec::with_capacity(256), Frames::new());
    let mut round = |id: u64| {
        out.clear();
        let sent = Envelope::new(id, Request::Cancel { id: id * 7 });
        encode_into(&mut writer, &mut out, &sent).unwrap();
        frames.push(&out);
        let item = frames.next_ref().unwrap().expect("a whole frame");
        let back: Envelope<Request> = decode(item).unwrap();
        assert_eq!(back, sent);
    };
    // The longest frame of the ids below first, so the buffer has grown to it.
    round(99);
    blaze_alloc_count::assert_at_most(0, "a warm encode, push, cut and decode", || {
        for id in 0..100 {
            round(id);
        }
    });
}

#[test]
fn nothing_comes_out_of_a_frame_cut_short() {
    let frame = encode(&Envelope::new(1, Request::Status)).unwrap();
    let mut frames = Frames::new();
    frames.push(&frame[..frame.len() - 1]);
    assert_eq!(frames.next().unwrap(), None);
    frames.push(&frame[frame.len() - 1..]);
    assert!(frames.next().unwrap().is_some());
    assert_eq!(frames.next().unwrap(), None);
}

#[test]
fn a_length_over_the_longest_breaks_the_stream_before_its_bytes_arrive() {
    let mut frames = Frames::new();
    frames.push(&(LONGEST_REQUEST + 1).to_be_bytes());
    let error = frames.next().unwrap_err();
    assert!(error.what.contains(&format!("{}", LONGEST_REQUEST + 1)), "{error:?}");
    frames.push(&encode(&Envelope::new(1, Request::Status)).unwrap());
    assert_eq!(frames.next().unwrap_err(), error);
}

#[test]
fn a_frame_of_the_longest_length_is_waited_for() {
    let mut frames = Frames::new();
    frames.push(&LONGEST_REQUEST.to_be_bytes());
    assert_eq!(frames.next().unwrap(), None);
}

#[test]
fn another_schema_is_refused_by_name_under_its_id() {
    let item =
        blaze_protocol::cbor::to_vec(&Envelope { schema: 2, id: 41, body: Request::Status }).unwrap();
    let refused = decode::<Request>(&item).unwrap_err();
    assert_eq!(refused.id, Some(41));
    assert_eq!(refused.error.what, format!("schema 2, this daemon speaks {SCHEMA}"));
}

#[test]
fn another_schema_is_refused_before_its_body_is_read() {
    #[derive(serde::Serialize)]
    enum Future {
        Teleport { to: String },
    }
    let item = blaze_protocol::cbor::to_vec(&Envelope {
        schema: SCHEMA + 1,
        id: 3,
        body: Future::Teleport { to: "mars".to_owned() },
    })
    .unwrap();
    let refused = decode::<Request>(&item).unwrap_err();
    assert!(refused.error.what.starts_with(&format!("schema {}", SCHEMA + 1)), "{refused:?}");
}

#[test]
fn another_schema_whose_body_reads_as_this_ones_is_refused_too() {
    let item = blaze_protocol::cbor::to_vec(&Envelope { schema: SCHEMA + 1, id: 8, body: Request::Status })
        .unwrap();
    let refused = decode::<Request>(&item).unwrap_err();
    assert_eq!(refused.id, Some(8));
    assert!(refused.error.what.starts_with(&format!("schema {}", SCHEMA + 1)), "{refused:?}");
}

#[test]
fn an_item_in_place_is_the_item() {
    let mut frames = Frames::new();
    frames.push(&encode(&Envelope::new(2, Request::Status)).unwrap());
    frames.push(&encode(&Envelope::new(3, Request::Status)).unwrap());
    assert_eq!(frames.next_ref().unwrap(), Some(&item(2, Request::Status)[..]));
    assert_eq!(frames.next_ref().unwrap(), Some(&item(3, Request::Status)[..]));
    assert_eq!(frames.next_ref().unwrap(), None);
}

#[test]
fn reads_grow_only_as_fast_as_a_frame_arrives() {
    let least = 64 << 10;
    let mut frames = Frames::new();
    assert_eq!(frames.wanted(least), least);
    frames.push(&(10u32 << 20).to_be_bytes());
    assert_eq!(frames.wanted(least), least, "a head alone grows nothing");
    let mut held = 4;
    let mut reads = 0;
    while held < 4 + (10 << 20) {
        let wanted = frames.wanted(least);
        assert!(wanted <= held.max(least), "{wanted} asked with {held} here");
        let here = wanted.min(4 + (10 << 20) - held);
        frames.push(&vec![0; here]);
        held += here;
        reads += 1;
    }
    assert!(reads < 16, "{reads} reads for 10 MiB");
    assert_eq!(frames.next().unwrap().map(|it| it.len()), Some(10 << 20));
    assert_eq!(frames.wanted(least), least);
}

#[test]
fn a_fill_reads_straight_into_the_frames() {
    let frame = encode(&Envelope::new(4, Request::Status)).unwrap();
    let mut from = &frame[..];
    let mut frames = Frames::new();
    assert_eq!(frames.fill(&mut from, 3).unwrap(), 3);
    assert_eq!(frames.next().unwrap(), None);
    assert_eq!(frames.held(), 3);
    assert_eq!(frames.fill(&mut from, 1 << 16).unwrap(), frame.len() - 3);
    assert_eq!(frames.next().unwrap(), Some(item(4, Request::Status)));
    assert_eq!(frames.fill(&mut from, 1 << 16).unwrap(), 0);
    assert_eq!(frames.held(), 0);
}

#[test]
fn non_deterministic_cbor_is_refused_naming_the_offence() {
    let mut item = item(5, Request::Status);
    // The id 5 as `18 05`, a longer head than it needs.
    let at = item.windows(3).position(|it| it == [0x62, b'i', b'd']).unwrap() + 3;
    assert_eq!(item[at], 0x05);
    item.splice(at..=at, [0x18, 0x05]);
    let refused = decode::<Request>(&item).unwrap_err();
    assert_eq!(refused.id, None);
    assert!(refused.error.what.contains("not deterministic CBOR"), "{refused:?}");
}

#[test]
fn a_body_that_is_not_a_request_is_refused_under_its_id() {
    let item = blaze_protocol::cbor::to_vec(&Envelope::new(12, Response::Done { ok: true })).unwrap();
    let refused = decode::<Request>(&item).unwrap_err();
    assert_eq!(refused.id, Some(12));
    assert!(refused.error.what.starts_with("an envelope that does not decode"), "{refused:?}");
}

#[test]
fn bytes_that_are_no_envelope_are_refused_without_an_id() {
    let refused = decode::<Request>(&[0xff]).unwrap_err();
    assert_eq!(refused.id, None);
    let refused = decode::<Request>(&blaze_protocol::cbor::to_vec(&7u32).unwrap()).unwrap_err();
    assert_eq!(refused.id, None);
}
