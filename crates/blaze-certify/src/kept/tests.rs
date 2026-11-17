use std::collections::BTreeMap;

use super::*;

fn kept(pid: u32) -> Kept {
    Kept {
        pid,
        started_ns: 10,
        ended_ns: 90,
        certified: Some(7),
        slot: "ab12".to_owned(),
        workspace: "/w".to_owned(),
        command: "run --release".to_owned(),
        timed: vec![
            Timed { name: "client: exec to main".to_owned(), start_ns: 10, end_ns: 20 },
            Timed { name: "client: solo check".to_owned(), start_ns: 20, end_ns: 60 },
        ],
    }
}

/// The invariant: a record round-trips byte for byte, and any cut of it, or a byte after it,
/// is refused by name. Control: one byte changed in the magic is refused, so the reader
/// checks it, and the first layout's magic is refused.
#[test]
fn a_record_round_trips_and_a_cut_one_is_refused() {
    let record = kept(41);
    let bytes = record.write().unwrap();
    assert_eq!(Kept::read(&bytes), Ok(record.clone()));
    assert_eq!(Kept::read(&bytes).unwrap().write().unwrap(), bytes);
    let none = Kept { certified: None, timed: Vec::new(), ..kept(1) };
    assert_eq!(Kept::read(&none.write().unwrap()), Ok(none));
    for cut in 0..bytes.len() {
        assert!(Kept::read(&bytes[..cut]).is_err(), "{cut}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(Kept::read(&longer).unwrap_err().contains("follow its last span"));
    let mut other = bytes.clone();
    other[0] = b'B';
    assert!(Kept::read(&other).is_err());
    let mut first = bytes;
    first[15] = b'1';
    assert!(Kept::read(&first).unwrap_err().contains("blaze solo rows2"));
}

#[test]
fn a_name_is_whole_part_or_foreign() {
    assert_eq!(Named::of("41"), Named::Whole(41));
    assert_eq!(Named::of("41.part"), Named::Part(41));
    for foreign in ["", ".part", "041", "+41", "41.tmp", "x", "99999999999"] {
        assert_eq!(Named::of(foreign), Named::Foreign, "{foreign}");
    }
}

const DIR_NAME: &str = "state/solo-rows";
const NOW_NS: u64 = 1_000 * PART_GONE_AFTER_NS;

/// A disk of names, each with its bytes and change time, that answers a take's effects in an
/// order the seed shuffles. `keeps_removed`: the planted bug, a removal that says it removed
/// the file and leaves it.
struct Disk {
    files: BTreeMap<String, (Vec<u8>, u64)>,
    keeps_removed: bool,
}

impl Disk {
    fn answer(&mut self, effect: Effect) -> Completion {
        let (tag, done) = match effect {
            Effect::Stat { file, tag } => {
                let there = self.files.keys().any(|it| it.starts_with(&format!("{file}/")));
                (tag, Done::Stat(there.then_some(Meta { len: 0, dir: true, modified_ns: 0 })))
            }
            Effect::List { dir, tag } => {
                let listed = (self.files.iter())
                    .filter(|(name, _)| name.starts_with(&format!("{dir}/")))
                    .map(|(name, (bytes, at))| {
                        let meta = Meta { len: bytes.len() as u64, dir: false, modified_ns: *at };
                        (FileName::new(name).unwrap(), meta)
                    })
                    .collect();
                (tag, Done::Listed(listed))
            }
            Effect::ReadAll { file, tag } => match self.files.get(file.as_str()) {
                Some((bytes, _)) => (tag, Done::Read(bytes.clone())),
                None => (tag, Done::Failed(blaze_core::IoError(format!("{file}: not there")))),
            },
            Effect::Remove { file, tag } => {
                if !self.keeps_removed {
                    self.files.remove(file.as_str());
                }
                (tag, Done::Removed)
            }
            other => panic!("a take asked for {other:?}"),
        };
        Completion { tag, done }
    }

    /// One look of `take`, its effects answered in an order `seed` picks, to its end.
    fn look(&mut self, take: &mut Take, seed: u64, alive: &impl Fn(u32) -> bool) -> Taken {
        let mut effects = Vec::new();
        assert!(take.look(FileName::new(DIR_NAME).unwrap(), &mut effects));
        let mut turn = seed;
        for _ in 0..10_000 {
            assert!(!effects.is_empty(), "the look waits for nothing and never ended");
            turn = turn.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let effect = effects.swap_remove((turn >> 33) as usize % effects.len());
            let completion = self.answer(effect);
            assert!(take.owns(completion.tag));
            let mut more = Vec::new();
            if let Some(taken) = take.complete(completion, NOW_NS, alive, &mut more) {
                assert!(effects.is_empty() && more.is_empty(), "a look ended with effects in flight");
                return taken;
            }
            effects.append(&mut more);
        }
        panic!("the look never ended");
    }
}

/// What a run of looks broke of the invariant, or `None`.
fn broken(seed: u64, keeps_removed: bool) -> Option<String> {
    let mut disk = Disk { files: BTreeMap::new(), keeps_removed };
    let put = |disk: &mut Disk, name: String, bytes: Vec<u8>, at: u64| {
        disk.files.insert(format!("{DIR_NAME}/{name}"), (bytes, at));
    };
    for pid in [41, 42, 45] {
        put(&mut disk, pid.to_string(), kept(pid).write().unwrap(), NOW_NS);
    }
    // A writer still making 43 whole (alive), and 44's writer gone long since.
    put(&mut disk, "43.part".to_owned(), kept(43).write().unwrap(), NOW_NS);
    put(&mut disk, "44.part".to_owned(), kept(44).write().unwrap(), NOW_NS - 2 * PART_GONE_AFTER_NS);
    // 46's writer gone a moment ago: within the bound, so still left.
    put(&mut disk, "46.part".to_owned(), kept(46).write().unwrap(), NOW_NS - 1);
    put(&mut disk, "47".to_owned(), b"cut".to_vec(), NOW_NS);
    let alive = |pid: u32| pid == 43;
    let mut take = Take::new(5 << 32);
    // alloc: a test's tally
    let mut taken: BTreeMap<u32, usize> = BTreeMap::new();
    let mut refused = Vec::new();
    for look in 0..3 {
        let found = disk.look(&mut take, seed + look, &alive);
        if let Some(why) = found.failed {
            return Some(format!("look {look} failed: {why}"));
        }
        for kept in found.kept {
            *taken.entry(kept.pid).or_default() += 1;
        }
        refused.extend(found.refused);
    }
    for pid in [43, 44, 46] {
        if taken.contains_key(&pid) {
            return Some(format!("{pid}.part was taken"));
        }
    }
    for pid in [41, 42, 45] {
        match taken.get(&pid) {
            Some(1) => {}
            count => return Some(format!("{pid} was taken {count:?} times, not once")),
        }
        if disk.files.contains_key(&format!("{DIR_NAME}/{pid}")) {
            return Some(format!("{pid} was taken and left"));
        }
    }
    if !disk.files.contains_key(&format!("{DIR_NAME}/43.part")) || !disk.files.contains_key(&format!("{DIR_NAME}/46.part")) {
        return Some("a `.part` of a live writer, or within its bound, was removed".to_owned());
    }
    if disk.files.contains_key(&format!("{DIR_NAME}/44.part")) {
        return Some("the `.part` of a gone writer past its bound was left".to_owned());
    }
    let named = |what: &str| refused.iter().filter(|it| it.contains(what)).count();
    if named("44.part") != 1 || named("solo-rows/47") != 1 {
        return Some(format!("the refusals are not 44.part and 47 once each: {refused:?}"));
    }
    None
}

/// The invariant: every whole solo-rows file is taken exactly once over many looks and is gone
/// after it, a `.part` is never taken, one of a writer gone past [`PART_GONE_AFTER_NS`] is
/// removed and named once, and one of a live writer or within the bound is left; under any
/// order of the driver's completions. Control (the planted bug): a removal that leaves the
/// file is caught as a record taken twice.
#[test]
fn every_whole_file_is_taken_once_and_a_part_never() {
    for seed in 0..64 {
        assert_eq!(broken(seed, false), None, "seed {seed}");
    }
    let caught = broken(7, true).expect("the planted bug is caught");
    assert!(caught.contains("not once"), "{caught}");
}

/// No directory: the look ends at its stat, with nothing kept and nothing failed.
#[test]
fn no_directory_is_nothing_kept() {
    let mut disk = Disk { files: BTreeMap::new(), keeps_removed: false };
    let taken = disk.look(&mut Take::new(0), 1, &|_| false);
    assert_eq!(taken, Taken::default());
}
