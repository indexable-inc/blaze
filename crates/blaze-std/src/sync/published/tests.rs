//! [`Published`]'s proofs.
//!
//! - `readers`: a simulation over [`PublishedCore`] with a stand-in scheduler. `readers` readers
//!   each look at the value; one that finds it not yet published parks at the current wake
//!   generation and may look again only after a later wake. The tape decides which readers
//!   step before the publish, which step *during* it (inside its wake, the one point of a
//!   publish another thread can see), and whether the publisher publishes or is dropped
//!   unpublished; after it, every reader that may step does, until none can. The oracle: every
//!   reader ends, with the published value or with the abandonment, and none ever sees another
//!   value. Control: [`PublishedPlant::WakeBeforeSet`], a lost wake-up the search must find.
//! - `world`: on the simulation, through the task's `Cx`: a reader before the publish waits
//!   until its deadline and the wait is charged to its worker; a reader after gets the value in
//!   place; the publisher is handed out once; a dropped publisher ends a reader's wait with
//!   `Abandoned`.

use std::vec::Vec as StdVec;

use core::{cell::RefCell, fmt, time::Duration};

use blaze_sim::{Broken, Config, Tape, controls};

use super::{Look, Published, PublishedCore, PublishedError, PublishedPlant};
use crate::{Stopped, io::Io, sync::once::Wake};

const MOST_READERS: usize = 6;
const VALUE: u64 = 42;

/// The invariants the simulation checks.
#[derive(Clone, PartialEq, Eq, Debug)]
enum ReadBroken {
    /// Reader `reader` is parked and nothing will wake it.
    Stuck { reader: usize },
    /// Reader `reader` saw `got`, not the value published.
    Differs { reader: usize, got: u64 },
    /// Reader `reader` saw the value abandoned although it was published, or the other way.
    WrongEnd { reader: usize },
}

impl fmt::Display for ReadBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadBroken::Stuck { reader } => write!(f, "reader {reader} waits and nothing will wake it"),
            ReadBroken::Differs { reader, got } => write!(f, "reader {reader} read {got}, not {VALUE}"),
            ReadBroken::WrongEnd { reader } => write!(f, "reader {reader} ended other than the publisher did"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the core, each with the invariant that must catch it.
    impl PublishedPlant breaks ReadBroken {
        /// The core as it is.
        None,
        /// The publish wakes before it sets.
        WakeBeforeSet => ReadBroken::Stuck { .. },
    }
}

/// A reader of the stand-in.
#[derive(Clone, Copy, Debug)]
enum Reader {
    /// Has not looked yet.
    Fresh,
    /// Looked, found nothing, waits for a wake after generation `seen`.
    Parked { seen: u64 },
    /// Ended with the value (`Some`) or the abandonment (`None`).
    Done(Option<u64>),
}

/// The stand-in world: the readers and the wake generation.
struct Stand<'t> {
    tape: &'t Tape,
    core: PublishedCore<u64>,
    readers: RefCell<StdVec<Reader>>,
    generation: RefCell<u64>,
}

impl Stand<'_> {
    /// Reader `at` takes one step, if it may.
    fn step(&self, at: usize) {
        let mut readers = self.readers.borrow_mut();
        let generation = *self.generation.borrow();
        match readers[at] {
            Reader::Done(_) => {}
            Reader::Parked { seen } if seen == generation => {}
            Reader::Fresh | Reader::Parked { .. } => {
                readers[at] = match self.core.look() {
                    Look::Ready(value) => Reader::Done(Some(*value)),
                    Look::Abandoned => Reader::Done(None),
                    Look::NotYet => Reader::Parked { seen: generation },
                };
            }
        }
    }

    /// Readers the tape picks step, up to `steps` times.
    fn some_step(&self, steps: usize, label: &'static str) {
        let count = self.readers.borrow().len();
        for _ in 0..steps {
            let at = self.tape.index(count, label);
            self.step(at);
        }
    }

    /// Whether some reader may step.
    fn any_can(&self) -> bool {
        let generation = *self.generation.borrow();
        self.readers.borrow().iter().any(|reader| match reader {
            Reader::Fresh => true,
            Reader::Parked { seen } => *seen != generation,
            Reader::Done(_) => false,
        })
    }
}

/// The wake: a new generation, and readers stepping inside it (during the publish).
impl Wake for Stand<'_> {
    fn wake(&self) {
        *self.generation.borrow_mut() += 1;
        let during = self.tape.index(MOST_READERS + 1, "readers stepping during the wake");
        self.some_step(during, "reader during the wake");
    }
}

fn readers(tape: &Tape, plant: PublishedPlant) -> Result<(), Broken<ReadBroken>> {
    let count = 1 + tape.index(MOST_READERS, "readers");
    let stand = Stand {
        tape,
        core: PublishedCore::new(plant),
        // alloc: the test's readers.
        readers: RefCell::new((0..count).map(|_| Reader::Fresh).collect()),
        generation: RefCell::new(0),
    };
    let before = tape.index(2 * MOST_READERS + 1, "reader steps before the publish");
    stand.some_step(before, "reader before the publish");
    let publishes = tape.below(4, "publishes (else abandons)") > 0;
    if !stand.core.hand() {
        return Err(Broken::Harness("the publisher of a fresh core was not handed out".into()));
    }
    if stand.core.hand() {
        return Err(Broken::Harness("the publisher was handed out twice".into()));
    }
    match publishes {
        true => {
            stand.core.publish(VALUE, &stand);
        }
        false => stand.core.abandon(&stand),
    }
    // After: every reader that may step does, until none can.
    let mut fuel = 4 * MOST_READERS;
    while stand.any_can() && fuel > 0 {
        fuel -= 1;
        for at in 0..count {
            stand.step(at);
        }
    }
    tape.note(format_args!("{:?}", stand.readers.borrow()));
    for (reader, end) in stand.readers.borrow().iter().enumerate() {
        match (*end, publishes) {
            (Reader::Done(Some(VALUE)), true) | (Reader::Done(None), false) => {}
            (Reader::Done(Some(got)), true) => return Err(Broken::Oracle(ReadBroken::Differs { reader, got })),
            (Reader::Done(_), _) => return Err(Broken::Oracle(ReadBroken::WrongEnd { reader })),
            (Reader::Fresh | Reader::Parked { .. }, _) => return Err(Broken::Oracle(ReadBroken::Stuck { reader })),
        }
    }
    Ok(())
}

#[test]
fn readers_before_during_and_after_a_publish_all_end_with_it() {
    controls(Config::cases(500), readers);
}

fn root(io: &Io) -> crate::cx::Cx {
    io.cx(Duration::MAX, crate::mem::TASK_FRAME).expect("a test's root frame is made")
}

#[test]
fn a_reader_before_the_publish_waits_charged_and_one_after_reads_in_place() {
    let io = Io::sim(11);
    let cx = root(&io);
    let clock = io.need_clock().expect("the simulation has a clock");
    let value = Published::<u64>::new("test.published");
    assert_eq!(value.get(), None);
    let publisher = value.publisher().expect("the one publisher");
    assert_eq!(value.publisher().err(), Some(PublishedError::PublisherTaken { published: "test.published" }));
    let waits_before = cx.on_worker(|w| w.meter().waits().total().0);
    let short = cx.until(clock.after(Duration::from_millis(5)));
    let early = value.need(&short);
    assert!(matches!(early, Err(PublishedError::Stopped(Stopped::Deadline { .. }))), "{early:?}");
    let waits_after = cx.on_worker(|w| w.meter().waits().total().0);
    assert!(waits_after > waits_before, "the reader's wait was charged to no worker");
    assert_eq!(*publisher.publish(VALUE), VALUE);
    assert_eq!(value.get(), Some(&VALUE));
    assert_eq!(value.need(&cx), Ok(&VALUE));
    assert_eq!(value.into_inner(), Some(VALUE));
}

#[test]
fn a_dropped_publisher_abandons_and_its_readers_end() {
    let io = Io::sim(12);
    let cx = root(&io);
    let value = Published::<u64>::new("test.published");
    value.publisher().expect("the one publisher").abandon();
    assert_eq!(value.need(&cx), Err(PublishedError::Abandoned { published: "test.published" }));
    assert_eq!(value.get(), None);
}

#[test]
fn a_panic_past_the_publisher_abandons() {
    let io = Io::sim(13);
    let cx = root(&io);
    let value = Published::<u64>::new("test.published");
    let unwound = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| {
        let publisher = value.publisher().expect("the one publisher");
        // Always true; written as a test so the publish after it is not unreachable code.
        if !value.name().is_empty() {
            panic!("the owner's making panicked");
        }
        publisher.publish(VALUE);
    }));
    assert!(unwound.is_err());
    assert_eq!(value.need(&cx), Err(PublishedError::Abandoned { published: "test.published" }));
}
