//! The vouch against a model of one file on a file system whose clock ticks in whole seconds
//! (`blaze_certify::testing::stamp_at`: a stamp is its granule alone, so a second write in a
//! granule leaves the stamp as it was), written by a stranger at any time, the store's own
//! placement among them.

use blaze_certify::testing::{GRANULE_NS, stamp_at};
use blaze_core::Stamp;
use blaze_ids::ContentId;
use blaze_sim::{Config, Tape};

use super::{Next, PLANT, Plant, Vouch};

fn bytes(n: u8) -> ContentId {
    ContentId::of("test vouch bytes", &[n])
}

/// One case: the store placed bytes `0` at `placed_ns` and looked at once; a stranger wrote
/// `writes` (when, which bytes); the driver first tries to begin at `try_ns`, then at the
/// vouch's due instant, reads `read_after` later and looks again `relook_after` after that.
#[derive(Clone, Debug)]
struct Case {
    placed_ns: u64,
    writes: Vec<(u64, u8)>,
    try_ns: u64,
    read_after: u64,
    relook_after: u64,
}

/// The file as each write left it, the placement first: when, its stamp, its bytes.
fn history(case: &Case) -> Vec<(u64, Stamp, ContentId)> {
    let mut all = vec![(case.placed_ns, stamp_at(case.placed_ns), bytes(0))];
    let mut writes = case.writes.clone();
    writes.sort_by_key(|it| it.0);
    all.extend(writes.into_iter().map(|(at, n)| (at, stamp_at(at), bytes(n))));
    all
}

/// The file at `at`: the last write at or before it.
fn file_at(history: &[(u64, Stamp, ContentId)], at: u64) -> (Stamp, ContentId) {
    let last = history.iter().rev().find(|it| it.0 <= at).unwrap_or(&history[0]);
    (last.1, last.2)
}

/// The vouch driven over the case: what it ended with, and when its first look was.
fn run(case: &Case) -> (Next, u64) {
    let history = history(case);
    let Some(mut vouch) = Vouch::of(0, bytes(0), stamp_at(case.placed_ns), case.placed_ns) else {
        return (Next::Dropped("the placement's look was steady"), case.placed_ns);
    };
    let due = vouch.due_ns().expect("a vouch not begun is due");
    let (mut now, mut next) = (case.try_ns, vouch.begin(case.try_ns));
    if next.is_none() {
        now = due.max(case.try_ns);
        next = vouch.begin(now);
    }
    let mut next = next.expect("a vouch begins once due");
    let first_look = now;
    let mut looks = 0;
    loop {
        next = match next {
            Next::Look => {
                looks += 1;
                if looks == 2 {
                    now += case.relook_after;
                }
                vouch.looked(Some(file_at(&history, now).0))
            }
            Next::Read => {
                now += case.read_after;
                vouch.read(Some(file_at(&history, now).1))
            }
            ended => return (ended, first_look),
        };
    }
}

/// The invariant: a stamp the vouch keeps is the store's and is never the stamp of other
/// bytes from its first look on, whatever a stranger wrote and when: the evidence that keeps
/// it may answer any later request by it.
fn check(case: &Case) -> Result<(), String> {
    let (ended, looked) = run(case);
    let Next::Vouched { stamp, .. } = ended else { return Ok(()) };
    if stamp != stamp_at(case.placed_ns) {
        return Err(format!("{case:?}: vouched for {stamp:?}, which is not the store's look"));
    }
    let history = history(case);
    let at_look = file_at(&history, looked);
    let later = history.iter().filter(|it| it.0 > looked).map(|it| (it.1, it.2));
    match std::iter::once(at_look).chain(later).find(|it| it.0 == stamp && it.1 != bytes(0)) {
        Some((_, other)) => Err(format!("{case:?}: vouched for {stamp:?}, which the file had from {looked} on holding {other:?}")),
        None => Ok(()),
    }
}

fn case(tape: &Tape) -> Case {
    let placed_ns = tape.int_in(0..=3 * GRANULE_NS, "placed");
    let count = tape.int_in(0..=3usize, "writes");
    let writes = tape.vec(count..=count, "write", |tape| {
        (placed_ns + tape.int_in(0..=6 * GRANULE_NS, "after the placement"), tape.int_in(0..=1u8, "bytes"))
    });
    Case {
        placed_ns,
        writes,
        try_ns: placed_ns + tape.int_in(0..=3 * GRANULE_NS, "first try"),
        read_after: tape.int_in(0..=2 * GRANULE_NS, "read after"),
        relook_after: tape.int_in(0..=2 * GRANULE_NS, "relook after"),
    }
}

/// The invariant above, over drawn cases. `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn a_vouched_stamp_is_never_the_stamp_of_other_bytes() {
    blaze_sim::check(Config::cases(512), |tape| check(&case(tape)));
}

/// Not vacuous: a file no stranger wrote is vouched for, by the steady look.
#[test]
fn a_file_left_as_placed_is_vouched_for_once_steady() {
    let case = Case { placed_ns: GRANULE_NS / 10, writes: Vec::new(), try_ns: 0, read_after: 1, relook_after: 1 };
    match run(&case).0 {
        Next::Vouched { stamp, looked_ns } => {
            assert_eq!(stamp, stamp_at(case.placed_ns));
            assert!(!blaze_vfs::racy(&stamp, looked_ns), "kept with a racy look: {looked_ns}");
        }
        other => panic!("{other:?}"),
    }
}

/// The controls: each planted bug vouches for a stamp the file had with other bytes, on a
/// case that shows it.
#[test]
fn the_oracle_catches_each_planted_bug() {
    let g = GRANULE_NS;
    // A stranger's write of other bytes in the placement's granule, after a racy look.
    let same_granule = Case { placed_ns: g / 10, writes: vec![(g / 2, 1)], try_ns: g / 5, read_after: g / 10, relook_after: g / 10 };
    // A stranger's write in the placement's granule, then the placed bytes put back later.
    let put_back = Case {
        placed_ns: g / 10,
        writes: vec![(g / 2, 1), (5 * g / 2, 0)],
        try_ns: 0,
        read_after: g,
        relook_after: g / 2,
    };
    for (plant, case) in [(Plant::RacyLook, &same_granule), (Plant::NoRead, &same_granule), (Plant::NoRelook, &put_back)] {
        assert!(check(case).is_ok(), "{case:?} holds unplanted");
        PLANT.set(plant);
        let caught = check(case);
        PLANT.set(Plant::None);
        assert!(caught.is_err(), "{plant:?} was not caught");
    }
}

/// The invariant: a read's hash made on the worker pool, off the loop, is the hash the loop
/// made in line, told back by its vouch's serial, once each; and `out` says whether one is
/// still out, so the loop polls for it only then. Of 8 reads of 1 MiB, each hashed both ways.
#[test]
fn a_hash_on_the_pool_is_told_back_as_the_one_made_in_line() {
    use super::super::super::settle::Compress;
    use super::Hashes;
    let reads: Vec<Vec<u8>> = (0..8u8).map(|n| vec![n; 1 << 20]).collect();
    let wanted = super::super::certify::content_of(&reads[3], None);
    let mut inline = Hashes::new();
    let mut pool = Hashes::new();
    pool.on(Compress::Pool);
    for (serial, bytes) in reads.iter().enumerate() {
        inline.hash(serial as u32, bytes.clone(), wanted, None);
        pool.hash(serial as u32, bytes.clone(), wanted, None);
    }
    let mut made: Vec<_> = std::iter::from_fn(|| inline.back()).collect();
    assert!(!inline.out(), "a hash in line is back as it is asked");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut told = Vec::new();
    while pool.out() {
        assert!(std::time::Instant::now() < deadline, "the pool told {} of 8 hashes back in 30 s", told.len());
        match pool.back() {
            Some(back) => told.push(back),
            None => std::thread::sleep(std::time::Duration::from_millis(1)),
        }
    }
    made.sort_unstable_by_key(|it| it.0);
    told.sort_unstable_by_key(|it| it.0);
    assert_eq!(told, made);
    assert_eq!(made[3].1, Some(wanted), "the read of the bytes placed hashes to their key");
    assert!(made.iter().filter(|it| it.1 == Some(wanted)).count() == 1);
}
