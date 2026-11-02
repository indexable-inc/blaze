//! The fetch protocol's frames: each round-trips, the stack encoders write what `encode`
//! writes, and no bytes from the other process panic the decoder or decode to a frame that
//! re-encodes differently.

use super::Rng;
use crate::wire::{self, ASK_LEN, FAILED_LEN, Frame, MOST_WHY, READY_LEN, WireError};

fn encoded(frame: Frame<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    frame.encode(&mut out);
    out
}

#[test]
fn every_frame_round_trips() {
    let frames = [
        Frame::Ask { request: 0, lazy: 0 },
        Frame::Ask { request: u64::MAX, lazy: u32::MAX },
        Frame::Ready { request: 7, lazy: 3, target: 0xffff_8000_1234_5678 },
        Frame::Failed { request: 9, lazy: 1, why: "" },
        Frame::Failed { request: 9, lazy: 1, why: "compiling `f::<ü>`: its MIR could not be built" },
    ];
    for frame in frames {
        let bytes = encoded(frame);
        assert_eq!(Frame::decode(&bytes), Ok(frame));
    }
    assert_eq!(encoded(frames[0]).len(), ASK_LEN);
    assert_eq!(encoded(frames[2]).len(), READY_LEN);
    assert_eq!(encoded(frames[3]).len(), FAILED_LEN);
}

#[test]
fn the_stack_encoders_match_encode() {
    assert_eq!(wire::ask(0x1122, 5).as_slice(), encoded(Frame::Ask { request: 0x1122, lazy: 5 }));
    assert_eq!(
        wire::ready(0x33, 6, 0x4000).as_slice(),
        encoded(Frame::Ready { request: 0x33, lazy: 6, target: 0x4000 })
    );
    assert_eq!(wire::channel_kind(&Frame::Ask { request: 0, lazy: 0 }), wire::ASK_KIND);
    assert_ne!(wire::ASK_KIND, wire::READY_KIND);
    assert!(blaze_inproc::frame::kinds::LAZY.contains(&wire::FAILED_KIND.0));
}

#[test]
fn a_long_reason_is_cut_on_a_character_boundary() {
    let why = "é".repeat(MOST_WHY);
    let bytes = encoded(Frame::Failed { request: 1, lazy: 2, why: &why });
    let Ok(Frame::Failed { why: cut, .. }) = Frame::decode(&bytes) else { panic!("decodes") };
    assert!(cut.len() <= MOST_WHY && cut.len() >= MOST_WHY - 1);
    assert!(why.starts_with(cut));
}

#[test]
fn malformed_frames_are_refused() {
    let ask = encoded(Frame::Ask { request: 1, lazy: 2 });
    for len in 0..ask.len() {
        assert!(matches!(Frame::decode(&ask[..len]), Err(WireError::Short { .. })), "{len}");
    }
    let mut long = ask.clone();
    long.push(0);
    assert_eq!(Frame::decode(&long), Err(WireError::Trailing { kind: 0x4c5a_0001, extra: 1 }));
    let mut unknown = ask.clone();
    unknown[0] = 0x77;
    assert!(matches!(Frame::decode(&unknown), Err(WireError::Kind(_))));

    let failed = encoded(Frame::Failed { request: 1, lazy: 2, why: "ok" });
    let mut bad = failed.clone();
    bad[FAILED_LEN] = 0xff;
    assert_eq!(Frame::decode(&bad), Err(WireError::Utf8));
    let mut huge = failed.clone();
    huge[16..20].copy_from_slice(&(MOST_WHY as u32 + 1).to_le_bytes());
    assert_eq!(Frame::decode(&huge), Err(WireError::LongWhy(MOST_WHY + 1)));
    let mut padded = failed.clone();
    padded[21] = 1;
    assert_eq!(Frame::decode(&padded), Err(WireError::Padding));
    assert!(matches!(Frame::decode(&failed[..failed.len() - 1]), Err(WireError::Short { .. })));
    let mut extra = failed.clone();
    extra.push(b'!');
    assert!(matches!(Frame::decode(&extra), Err(WireError::Trailing { .. })));
}

#[test]
fn random_bytes_never_panic_and_what_decodes_reencodes_the_same() {
    let mut rng = Rng::new(0x1a2b);
    let seeds = [
        encoded(Frame::Ask { request: 3, lazy: 4 }),
        encoded(Frame::Ready { request: 3, lazy: 4, target: 5 }),
        encoded(Frame::Failed { request: 3, lazy: 4, why: "why" }),
    ];
    for round in 0..20_000 {
        let mut bytes = seeds[round % seeds.len()].clone();
        for _ in 0..1 + rng.below(4) {
            match rng.below(3) {
                0 if !bytes.is_empty() => {
                    let at = rng.below(bytes.len() as u64) as usize;
                    bytes[at] = rng.next_u64() as u8;
                }
                1 => bytes.push(rng.next_u64() as u8),
                _ => {
                    let len = rng.below(bytes.len() as u64 + 1) as usize;
                    bytes.truncate(len);
                }
            }
        }
        if let Ok(frame) = Frame::decode(&bytes) {
            assert_eq!(encoded(frame), bytes);
        }
    }
}
