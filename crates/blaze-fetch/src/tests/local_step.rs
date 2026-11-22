//! The machine's own local step against a simulated cargo cache (fetch.md, "End state:
//! dependencies on the command path", piece 7; rule 43). A case draws three crates, each held
//! in the machine's cargo cache, one of them tampered (a valid `.crate` of the same folder, one
//! file one byte longer); records are adopted into the heap or in place, by the size each
//! archive's trailer states against a drawn `reserve_from`. The run goes through the real
//! driver loop ([`crate::drive::step`]) over an IO whose origin serves every honest archive.
//!
//! The oracles: every record in the store is its lock's crate, byte for byte, the record the
//! network makes ("wrong bytes"); the tampered archive is never adopted, its try is kept as the
//! lock's mismatch, and it is fetched over HTTPS once; the two held archives are adopted and
//! ask no HTTP; each crate's local archive is asked once; a second run on the same store asks
//! no place at all. The planted [`LocalPlant::AdoptBeforeChecksum`] (a found archive made into
//! a record without its SHA-256 compared) is caught by "wrong bytes".

use std::collections::{BTreeMap, VecDeque};

use blaze_ids::ContentId;
use blaze_manifest::lock::LockId;
use blaze_own::{AVec, String as OwnString, Vec as OwnVec};
use blaze_sim::{Broke, Config, Tape};
use blaze_tar::{Make, Writer};
use blaze_text::Writes as _;

use crate::drive::{self, FetchIo, Reserved, Woke};
use crate::fetch::{Ask, CRATES, LOCAL_PLANT, Limits, LocalArchive, LocalPlant, Machine, Method, Outcome, Reply, StoreFailure};
use crate::local::{AdoptRefused, CARGO_CACHE, Miss};
use crate::plan::Unit;
use crate::unpack;

/// The most room a reservation of these small crates may ask (their limits' `most`).
const ROOM_MOST: u64 = 64 << 20;

/// One drawn crate: its honest archive (the lock's), a tampered one under the same name, and
/// the record the network makes of the honest bytes.
struct Drawn {
    name: std::string::String,
    version: std::string::String,
    archive: std::vec::Vec<u8>,
    tampered: std::vec::Vec<u8>,
    sha256: [u8; 32],
    truth: std::vec::Vec<u8>,
}

/// A `.crate`: a gzip of a tar whose every entry is under `folder/`.
fn archive_of(folder: &str, files: &[(std::string::String, std::vec::Vec<u8>)]) -> std::vec::Vec<u8> {
    let mut tar = OwnVec::new_in(blaze_own::testing::heap().alloc());
    let mut w = Writer::new();
    w.entry(folder, Make::Dir, &[], &mut tar).expect("a folder entry");
    for (path, bytes) in files {
        w.entry(&format!("{folder}/{path}"), Make::File { exec: false }, bytes, &mut tar).expect("a file entry");
    }
    w.finish(&mut tar).expect("the tar's end");
    let mut gz = OwnVec::new_in(blaze_own::testing::heap().alloc());
    blaze_deflate::gzip(tar.as_slice(), blaze_deflate::Level::Fast, blaze_own::testing::heap().alloc(), &mut gz).expect("gzip");
    gz.as_slice().to_vec()
}

fn draw(tape: &Tape, at: usize, limits: &unpack::Limits) -> Drawn {
    let name = format!("c{at}");
    let version = format!("1.{}.0", tape.below(3, "minor"));
    let folder = format!("{name}-{version}");
    let mut files: std::vec::Vec<(std::string::String, std::vec::Vec<u8>)> = (0..1 + tape.below(3, "files"))
        .map(|f| {
            let mut bytes = vec![0u8; tape.below(20_000, "file len") as usize];
            tape.fill(&mut bytes, "file bytes");
            (format!("src/f{f}.rs"), bytes)
        })
        .collect();
    let archive = archive_of(&folder, &files);
    files[0].1.push(b'!');
    let tampered = archive_of(&folder, &files);
    let sha256 = blaze_sha256::digest(&archive);
    let (truth, _) = unpack::crate_record(&name, &version, sha256, &archive, limits, blaze_own::testing::heap().alloc()).expect("an honest crate's record");
    Drawn { name, version, archive, tampered, sha256, truth: truth.as_slice().to_vec() }
}

/// The driver's IO: a cargo cache, an origin serving every honest archive at once, a store.
struct CargoIo<'d> {
    crates: &'d [Drawn],
    now: u64,
    /// Cargo's cache, by (name, version).
    cache: BTreeMap<(std::string::String, std::string::String), std::vec::Vec<u8>>,
    store: BTreeMap<ContentId, std::vec::Vec<u8>>,
    /// Answers on their way, in order: the ask and its unit.
    wire: VecDeque<Ask>,
    last: Option<(u32, std::vec::Vec<u8>)>,
    kept: BTreeMap<u32, std::vec::Vec<u8>>,
    /// Units asked over HTTPS, one entry a request.
    http: std::vec::Vec<u32>,
    /// Crate names whose local archive was asked for, one entry an ask.
    local: std::vec::Vec<std::string::String>,
    /// The first thing off its form the IO saw.
    broke: Option<std::string::String>,
}

impl FetchIo for CargoIo<'_> {
    fn now(&self) -> u64 {
        self.now
    }

    fn lookup(&mut self, key: &ContentId) -> bool {
        self.store.contains_key(key)
    }

    fn put(&mut self, key: &ContentId, record: &[u8]) -> Result<(), StoreFailure> {
        self.store.insert(*key, record.to_vec());
        Ok(())
    }

    fn send(&mut self, ask: Ask, method: Method, url: &str, _headers: &[(&str, &str)], _body: &[u8], _most: u64, _deadline_ns: u64) {
        self.http.push(ask.unit);
        let Some(krate) = self.crates.get(ask.unit as usize) else {
            self.broke.get_or_insert(format!("a request for unit {} of none", ask.unit));
            return;
        };
        let want = format!("{CRATES}{0}/{0}-{1}.crate", krate.name, krate.version);
        if method != Method::Get || url != want {
            self.broke.get_or_insert(format!("a request off its form: {url}"));
            return;
        }
        self.wire.push_back(ask);
    }

    fn wait(&mut self, until_ns: Option<u64>) -> Woke<'_> {
        self.last = None;
        let Some(ask) = self.wire.pop_front() else {
            if let Some(until) = until_ns {
                self.now = self.now.max(until);
            }
            return Woke::Due;
        };
        let body = self.crates[ask.unit as usize].archive.clone();
        let (_, body) = self.last.insert((ask.unit, body));
        Woke::Answer { ask, reply: Reply::Body { status: 200, bytes: body.as_slice() } }
    }

    fn keep(&mut self, unit: u32) {
        if let Some((of, body)) = self.last.take()
            && of == unit
        {
            self.kept.insert(unit, body);
        }
    }

    fn reserve(
        &mut self,
        unit: u32,
        key: &ContentId,
        room: u64,
        write: &mut dyn FnMut(&[u8], Result<&mut [u8], StoreFailure>) -> Option<u64>,
    ) -> Reserved {
        let body = self.kept.remove(&unit).unwrap_or_default();
        if room > ROOM_MOST {
            self.broke.get_or_insert(format!("unit {unit}: a reservation of {room} bytes"));
            _ = write(&body, Err(StoreFailure::Io));
            return Reserved::Abandoned;
        }
        let mut bytes = vec![0u8; room as usize];
        match write(&body, Ok(&mut bytes)) {
            Some(len) => {
                bytes.truncate(len as usize);
                self.store.insert(*key, bytes);
                Reserved::Kept
            }
            None => Reserved::Abandoned,
        }
    }

    fn local_archive(&mut self, name: &str, version: &str, out: &mut AVec<'_, u8>) -> LocalArchive {
        self.local.push(name.to_owned());
        match self.cache.get(&(name.to_owned(), version.to_owned())) {
            Some(bytes) => {
                out.extend_from_slice(bytes);
                LocalArchive::Found
            }
            None => LocalArchive::Absent,
        }
    }
}

/// One command: the machine over every unit, driven by [`drive::step`] until done.
fn command<'p, 'l>(units: &'p [Unit<'l, 'static>], io: &mut CargoIo<'_>, limits: Limits) -> Result<Machine<'p, 'l, 'static>, Broke> {
    let mut machine = Machine::new(units, limits, blaze_own::testing::heap().alloc());
    for _ in 0..1_000 {
        if machine.is_done() {
            if let Some(broke) = io.broke.take() {
                return Err(Broke::Oracle(broke));
            }
            return Ok(machine);
        }
        drive::step(&mut machine, io, 1_000_000);
    }
    Err(Broke::Oracle("a hang: the command took 1,000 steps".into()))
}

fn case(tape: &Tape, plant: LocalPlant) -> Result<(), Broke> {
    LOCAL_PLANT.set(LocalPlant::None);
    let mut limits = Limits::REAL;
    limits.ask_ns = 1_000_000_000;
    // Room for these crates (at most 3 files of 20,000 bytes), so a reservation is small.
    limits.unpack.most_inflated = 256 << 10;
    // Some records adopted into the heap, some in place, by the size each trailer states.
    limits.reserve_from = match tape.below(3, "in place") {
        0 => 0,
        1 => u64::MAX,
        _ => tape.below(48 << 10, "reserve from"),
    };
    let crates: std::vec::Vec<Drawn> = (0..3).map(|at| draw(tape, at, &limits.unpack)).collect();
    let tampered = tape.index(3, "tampered");
    LOCAL_PLANT.set(plant);
    let result = case_inner(&crates, tampered, limits);
    LOCAL_PLANT.set(LocalPlant::None);
    result
}

fn case_inner(crates: &[Drawn], tampered: usize, limits: Limits) -> Result<(), Broke> {
    let units: std::vec::Vec<Unit<'_, 'static>> = crates
        .iter()
        .enumerate()
        .map(|(at, c)| {
            let mut version = OwnString::new_in(blaze_own::testing::heap().alloc());
            version.str(&c.version);
            Unit::Crate { package: LockId(at as u32), name: &c.name, version, sha256: c.sha256 }
        })
        .collect();
    let cache = crates
        .iter()
        .enumerate()
        .map(|(at, c)| ((c.name.clone(), c.version.clone()), if at == tampered { c.tampered.clone() } else { c.archive.clone() }))
        .collect();
    let mut io = CargoIo {
        crates,
        now: 0,
        cache,
        store: BTreeMap::new(),
        wire: VecDeque::new(),
        last: None,
        kept: BTreeMap::new(),
        http: std::vec::Vec::new(),
        local: std::vec::Vec::new(),
        broke: None,
    };
    let machine = command(&units, &mut io, limits)?;
    for (at, (unit, krate)) in units.iter().zip(crates).enumerate() {
        // First: what a reader would map. A tampered archive taken is caught here.
        match io.store.get(&unit.key()) {
            Some(held) if *held == krate.truth => {}
            Some(_) => return Err(Broke::Oracle(format!("wrong bytes: the record of {} is not its lock's crate", krate.name))),
            None => return Err(Broke::Oracle(format!("{} has no record: {:?}", krate.name, machine.outcome(at as u32)))),
        }
        let asked = io.http.iter().filter(|it| **it as usize == at).count();
        let local = io.local.iter().filter(|it| **it == krate.name).count();
        if local != 1 {
            return Err(Broke::Oracle(format!("{}: its local archive asked {local} times, not once", krate.name)));
        }
        let at = at as u32;
        match at as usize == tampered {
            true => {
                if machine.adopted(at) {
                    return Err(Broke::Oracle(format!("a tampered archive adopted: {}", krate.name)));
                }
                let mismatch = matches!(
                    machine.tried(at),
                    Some(try_) if try_.place == CARGO_CACHE
                        && matches!(try_.miss, Miss::Refused(AdoptRefused::Mismatch { want, got }) if want == krate.sha256 && got != want)
                );
                if !mismatch {
                    return Err(Broke::Oracle(format!("{}: the refusal not kept as the lock's mismatch: {:?}", krate.name, machine.tried(at))));
                }
                if asked != 1 || !matches!(machine.outcome(at), Outcome::Fetched { down, .. } if *down == krate.archive.len() as u64) {
                    return Err(Broke::Oracle(format!("{}: refused locally, then {asked} HTTP asks, {:?}", krate.name, machine.outcome(at))));
                }
            }
            false => {
                if asked != 0 {
                    return Err(Broke::Oracle(format!("{}: adopted, yet {asked} HTTP asks", krate.name)));
                }
                if !machine.adopted(at) || machine.tried(at).is_some() {
                    return Err(Broke::Oracle(format!("{}: held, not adopted: {:?}", krate.name, machine.tried(at))));
                }
                if !matches!(machine.outcome(at), Outcome::Fetched { down: 0, .. }) {
                    return Err(Broke::Oracle(format!("{}: adopted, ended {:?}", krate.name, machine.outcome(at))));
                }
            }
        }
    }
    drop(machine);
    // A second command on the same store: every record stands, no place is asked.
    io.http.clear();
    io.local.clear();
    let again = command(&units, &mut io, limits)?;
    if again.outcomes().iter().any(|outcome| *outcome != Outcome::Present) || !io.http.is_empty() || !io.local.is_empty() {
        return Err(Broke::Oracle(format!(
            "asked again: {:?}, {} HTTP, {} local",
            again.outcomes(),
            io.http.len(),
            io.local.len()
        )));
    }
    Ok(())
}

/// The machine's local step (fetch.md piece 7): of a cargo cache of three archives, one
/// tampered, the two that are the lock's are adopted, each the network's record byte for byte,
/// and ask no HTTP; the tampered one is refused by the lock's SHA-256, kept as a try, and
/// fetched over HTTPS once; a second command asks nothing. Planting
/// `LocalPlant::AdoptBeforeChecksum` stores the tampered bytes, and "wrong bytes" catches it.
///
/// Expected: prints `local step: 2 adopted, 1 refused, 0 HTTP for the adopted; plant caught`,
/// then `test tests::local_step::the_local_step_adopts_only_the_locks_archives ... ok`
#[test]
fn the_local_step_adopts_only_the_locks_archives() {
    blaze_sim::check(Config::cases(200), |tape| case(tape, LocalPlant::None));
    blaze_sim::control(Config::cases(200), LocalPlant::None, LocalPlant::AdoptBeforeChecksum, "wrong bytes", case);
    println!("local step: 2 adopted, 1 refused, 0 HTTP for the adopted; plant caught");
}
