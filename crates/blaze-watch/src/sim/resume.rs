//! A run's feed to a reader over a link that drops: frames in flight are lost at a drop, the
//! reader reconnects and resumes from the last item it took, while the run keeps pushing into
//! its bounded window and the reader takes items and grants credit at its own pace.
//!
//! The oracle: every item the reader takes is the item the run pushed under that number, in
//! order, never twice, and a hole is always a gap the run announced; once the link stays up the
//! reader reaches the newest item. A slow reader never grows the run: the window is its fixed
//! size and the frames in flight stay within the credit.

use std::alloc::Global;
use std::collections::BTreeMap;

use blaze_own::Vec as Own;
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{Lap, harness, key, oracle};
use crate::conn::{Conn, Grant, INITIAL_CREDIT};
use crate::feed::{Feed, Follow, Item, Subscriber};
use crate::frame::{self, Frame};
use crate::ring::Event;
use crate::{Plant, Seq};

const EPOCH: u64 = 41;

fn item_of(n: u64) -> Item {
    Item::Event {
        lane: (n % 3) as u32,
        n,
        event: Event { kind: 1, flags: 0, detail: n as u32, at: n, value: n * 3, key: key(n) },
    }
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut feed = Feed::new_in(1, tape.range(256..4096, "window bytes") as usize, 64, Global).map_err(harness)?;
    let mut pushed: BTreeMap<u64, Item> = BTreeMap::new();
    let mut follow = Follow::default();
    // The link: the run's sender, its subscriber, the bytes in flight each way.
    let mut conn: Conn<Global> = Conn::new_in(Global);
    let mut sub: Option<Subscriber> = None;
    let mut grant = Grant::default();
    let mut down: Own<u8, Global> = Own::new_in(Global);
    let mut up: std::vec::Vec<u32> = std::vec::Vec::new();
    let mut connected = false;
    let steps = tape.range(1..300, "steps");
    let mut n = 0u64;
    // Then settling: the link stays up, nothing new is pushed, and each side takes its turn
    // until the reader has caught up (or the bound says it never will).
    let settle = 4 * 64 * 2 + 8;
    for step in 0..steps + settle {
        let choice = match step >= steps {
            true => 1 + (step - steps) % 3,
            false => tape.below(5, "step"),
        };
        match choice {
            0 => {
                n += 1;
                let item = item_of(n);
                let seq = feed.push(&item).map_err(harness)?;
                pushed.insert(seq.0, item);
            }
            1 if !connected => {
                // Reconnect: the run says its epoch, the reader opens from where it is.
                let from = follow.hello(EPOCH);
                conn = Conn::new_in(Global);
                conn.open(frame::EVENTS, 0);
                grant = Grant::default();
                sub = Some(Subscriber::open_planted(frame::EVENTS, from, feed.window(), plant).map_err(harness)?);
                connected = true;
            }
            1 => {
                // The run writes what credit allows.
                let Some(sub) = sub.as_mut() else { return Err(harness("connected with no subscriber")) };
                sub.pump(feed.window(), &mut conn, &mut down, usize::MAX).map_err(harness)?;
                oracle(down.len() <= INITIAL_CREDIT as usize + 4096, || {
                    format!("the run has {} bytes in flight, past the credit", down.len())
                })?;
            }
            2 => {
                // The reader takes a whole frame, if one is there.
                let Some((frame, used)) = frame::read(&down).map_err(harness)? else { continue };
                match frame {
                    Frame::Data { seq, bytes, .. } => {
                        follow.data(seq).map_err(|error| Broke::Oracle(format!("duplicate or skip: {error}")))?;
                        let item = Item::read(bytes).map_err(harness)?;
                        oracle(pushed.get(&seq.0) == Some(&item), || {
                            format!("item {} reached the reader as {item:?}", seq.0)
                        })?;
                        grant.received(frame::EVENTS, used).map_err(harness)?;
                        if let Some(bytes) = grant.consumed(used) {
                            up.push(bytes);
                        }
                    }
                    Frame::Gap { from, to, .. } => {
                        follow.gap(from, to).map_err(|error| Broke::Oracle(format!("duplicate or skip: {error}")))?;
                        oracle(to < feed.window().first(), || {
                            format!("a gap to {} while the window still holds from {}", to.0, feed.window().first().0)
                        })?;
                    }
                    other => return Err(harness(format!("the run sent {other:?}"))),
                }
                down.drain(..used);
            }
            3 => {
                for bytes in up.drain(..) {
                    conn.received(Frame::Credit { chan: frame::EVENTS, bytes }).map_err(harness)?;
                }
            }
            _ => {
                // The link drops: what is in flight is gone.
                connected = false;
                sub = None;
                down.clear();
                up.clear();
            }
        }
    }
    let newest = feed.window().newest();
    oracle(follow.last() == newest, || {
        format!("with the link up the reader stopped at {} of {}", follow.last().0, newest.0)
    })
}

/// The invariant: a resumed event feed delivers every pushed item in order at most once without repeating items across reconnects, announcing a gap whenever items were evicted.
#[test]
fn a_resumed_feed_delivers_each_item_once_or_a_gap() {
    let lap = Lap::new("resume: each item once or a gap");
    check(Config::cases(2000), |tape| lap.case(run(tape, Plant::None)));
}

#[test]
fn the_oracle_catches_a_resume_that_repeats_an_item() {
    let lap = Lap::new("resume: control, a repeated item");
    control(Config::cases(3000), Plant::None, Plant::ResumeInclusive, "duplicate", |tape, plant| lap.case(run(tape, plant)));
}

#[test]
fn a_seq_of_another_epoch_starts_again() {
    let mut follow = Follow::default();
    assert_eq!(follow.hello(1), Seq::NONE);
    assert_eq!(follow.data(Seq(1)), Ok(()));
    assert_eq!(follow.hello(1), Seq(1));
    assert_eq!(follow.hello(2), Seq::NONE);
}
