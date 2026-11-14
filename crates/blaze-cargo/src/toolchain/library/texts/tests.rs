use std::path::{Path, PathBuf};

use blaze_core::{Done, Effect, FileName, Tag, sim::{Faults, Sim}};
use blaze_ids::ContentId;
use blaze_sim::{Config, Tape};
use blaze_vfs::Mode;

use super::*;

const DIGEST: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// Whether every entry of `listing` lends a text of `bytes` that hashes to its listed id: what
/// a start trusts without checking.
fn every_text_is_its_id(listing: &Listing, bytes: &[u8]) -> bool {
    listing.entries.iter().all(|entry| {
        let text = &bytes[entry.at as usize..(entry.at + entry.len) as usize];
        ContentId::of(blaze_vfs::FILE_DOMAIN, text) == entry.id
    })
}

fn draw_files(tape: &Tape) -> Vec<(String, Mode, Vec<u8>)> {
    let parts = ["a", "b", "core", "src", "lib.rs", "x.md", "é"];
    let n = tape.int_in(0..=12usize, "files");
    let mut files: Vec<(String, Mode, Vec<u8>)> = Vec::new();
    for _ in 0..n {
        let depth = tape.int_in(1..=3usize, "depth");
        let path: Vec<&str> = (0..depth).map(|_| *tape.one_of(&parts, "part")).collect();
        let mode = if tape.bool("executable") { Mode::Executable } else { Mode::Regular };
        files.push((path.join("/"), mode, tape.bytes(0..=600, "text")));
    }
    blaze_ids::names::sort_by_name(&mut files, |it| it.0.as_bytes());
    files.dedup_by(|a, b| a.0 == b.0);
    files
}

fn borrowed(files: &[(String, Mode, Vec<u8>)]) -> Vec<(String, Mode, &[u8])> {
    files.iter().map(|(path, mode, text)| (path.clone(), *mode, text.as_slice())).collect()
}

/// The invariant: a texts file decodes to the listing it was encoded from (every path, mode
/// and length, in name order), every listed text is that file's bytes and hashes to its listed
/// id, and the file the listing and texts encode again is the same bytes (rule
/// persisted-state-round-trips). Control:
/// `the_oracle_catches_a_text_changed_under_its_listing`.
#[test]
fn texts_round_trip_and_every_text_hashes_to_its_listed_id() {
    blaze_sim::check(Config::DEFAULT, texts_round_trip);
}

fn texts_round_trip(tape: &Tape) {
    let files = draw_files(tape);
    let bytes = encode(DIGEST, &borrowed(&files));
    let listing = decode(DIGEST, &bytes).expect("what was encoded decodes");
    assert_eq!(listing.entries.len(), files.len());
    for (entry, (path, mode, text)) in listing.entries.iter().zip(&files) {
        assert_eq!((&entry.path, entry.mode, entry.len), (path, *mode, text.len() as u64));
        assert_eq!(&bytes[entry.at as usize..(entry.at + entry.len) as usize], text.as_slice());
    }
    assert!(every_text_is_its_id(&listing, &bytes));
    let again: Vec<(String, Mode, &[u8])> = listing
        .entries
        .iter()
        .map(|it| (it.path.clone(), it.mode, &bytes[it.at as usize..(it.at + it.len) as usize]))
        .collect();
    assert_eq!(encode(DIGEST, &again), bytes, "byte for byte");
}

/// The control of the round trip's oracle: one byte of one text changed under its listing
/// still decodes (a start reads no text), and the oracle finds it.
#[test]
fn the_oracle_catches_a_text_changed_under_its_listing() {
    let files = vec![
        ("core/src/lib.rs".to_owned(), Mode::Regular, b"pub fn f() {}\n".to_vec()),
        ("x.md".to_owned(), Mode::Executable, b"text".to_vec()),
    ];
    let mut bytes = encode(DIGEST, &borrowed(&files));
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let listing = decode(DIGEST, &bytes).expect("the header and listing are as written");
    assert!(!every_text_is_its_id(&listing, &bytes), "the oracle misses a changed text");
}

/// The invariant: a texts file of another layout, another tree's digest, a listing its hash
/// is not of, or a length its header and listing do not say (cut inside the listing, cut inside
/// the texts, a byte past them) is refused, and the refusal says which; the file as written
/// decodes (the control).
#[test]
fn a_texts_file_of_another_layout_digest_or_length_is_refused_by_name() {
    let files = vec![
        ("Cargo.toml".to_owned(), Mode::Regular, b"[workspace]\n".to_vec()),
        ("core/src/lib.rs".to_owned(), Mode::Regular, b"#![no_std]\n".to_vec()),
    ];
    let bytes = encode(DIGEST, &borrowed(&files));
    assert!(decode(DIGEST, &bytes).is_ok(), "the control: the file as written");

    let mut old = bytes.clone();
    old[..8].copy_from_slice(b"bztxts00");
    let refused = decode(DIGEST, &old).unwrap_err();
    assert_eq!(refused, Refused::Layout { found: *b"bztxts00" });
    assert!(refused.to_string().contains("bztxts00"), "{refused}");

    let other = "1".repeat(DIGEST.len());
    let refused = decode(&other, &bytes).unwrap_err();
    assert_eq!(refused, Refused::Digest { wanted: other.clone(), found: DIGEST.to_owned() });
    assert!(refused.to_string().contains(DIGEST), "{refused}");

    let header = 8 + 4 + DIGEST.len() + 32 + 8;
    let listing_len = u64::from_le_bytes(bytes[header - 8..header].try_into().unwrap());
    let texts_at = header as u64 + listing_len;
    let inside_listing = &bytes[..header + 3];
    assert_eq!(
        decode(DIGEST, inside_listing).unwrap_err(),
        Refused::Length { wanted: texts_at, found: inside_listing.len() as u64 }
    );
    let inside_texts = &bytes[..bytes.len() - 2];
    assert_eq!(
        decode(DIGEST, inside_texts).unwrap_err(),
        Refused::Length { wanted: bytes.len() as u64, found: inside_texts.len() as u64 }
    );
    let mut longer = bytes.clone();
    longer.push(0);
    assert_eq!(
        decode(DIGEST, &longer).unwrap_err(),
        Refused::Length { wanted: bytes.len() as u64, found: longer.len() as u64 }
    );

    let mut changed = bytes.clone();
    changed[header + 4 + 1] ^= 1; // a byte of the first path
    assert!(matches!(decode(DIGEST, &changed).unwrap_err(), Refused::Listing { .. }));
}

/// The simulated disk's name of `path`.
fn sim_name(path: &Path) -> FileName {
    FileName::new(path.to_str().expect("a test's path is UTF-8")).expect("a relative name")
}

/// Runs `effect` on `sim` to its completion, which must not fail.
fn run(sim: &mut Sim, effect: Effect) {
    let mut effects = vec![effect];
    blaze_core::run_until_idle(sim, &mut effects, |completion, _| {
        if let Done::Failed(error) = completion.done {
            panic!("a step of the place failed: {error:?}");
        }
    })
    .expect("the effects end");
}

/// The simulated disk's effects of one step, each completed before the next is asked.
fn effects_of(step: Step) -> Vec<Effect> {
    let tag = Tag(1);
    match step {
        Step::Write { file, bytes } => vec![
            Effect::Truncate { file: sim_name(&file), len: 0, tag },
            Effect::WriteAt { file: sim_name(&file), at: 0, bytes, tag },
        ],
        Step::Sync { file } => vec![Effect::Sync { file: sim_name(&file), tag }],
        Step::Rename { from, to } => {
            vec![Effect::Rename { from: sim_name(&from), to: sim_name(&to), tag }]
        }
        Step::SyncDir { dir } => vec![Effect::SyncDir { dir: sim_name(&dir), tag }],
        Step::Done => Vec::new(),
    }
}

/// `place`'s steps on `sim`, crashing it once `crash_after` effects have completed (never,
/// past the last).
fn place_on(sim: &mut Sim, mut place: Place, crash_after: usize) {
    let mut done = 0;
    for _ in 0..8 {
        let step = place.step();
        if step == Step::Done {
            return;
        }
        for effect in effects_of(step) {
            if done == crash_after {
                sim.crash();
                return;
            }
            run(sim, effect);
            done += 1;
        }
    }
    panic!("a place ends within its steps");
}

/// Whether what is at `at` on `sim` after a crash is no file, or a texts file that decodes and
/// whose every text hashes to its id.
fn none_or_all(sim: &Sim, at: &FileName) -> Result<(), String> {
    if !sim.exists(at) {
        return Ok(());
    }
    let bytes = sim.contents(at);
    let listing = decode(DIGEST, &bytes).map_err(|refused| format!("{at}: {refused}"))?;
    if every_text_is_its_id(&listing, &bytes) {
        Ok(())
    } else {
        Err(format!("{at}: a listed text is not the bytes of its id"))
    }
}

/// Over 256 seeds, a place on the simulated disk crashed after each number of its effects;
/// `Err` names the first seed whose crash broke [`none_or_all`].
fn crash_places(plant: Plant) -> Result<(), String> {
    let files: Vec<(String, Mode, Vec<u8>)> = (0..24)
        .map(|n| (format!("src/f{n:02}.rs"), Mode::Regular, vec![b'a' + (n % 26) as u8; 700 + n * 37]))
        .collect();
    let bytes = encode(DIGEST, &borrowed(&files));
    let tree = PathBuf::from("machine/t/digest/t");
    let at = name(&tree).expect("a tree with a parent");
    for seed in 0..256u64 {
        let mut sim = Sim::new(seed, Faults::default());
        let crash_after = (seed % 6) as usize;
        let place = Place::new(at.clone(), bytes.clone()).planted(plant);
        place_on(&mut sim, place, crash_after);
        none_or_all(&sim, &sim_name(&at)).map_err(|why| format!("seed {seed}: {why}"))?;
        // The next place, over whatever the crash left (a `.part` among it), puts the whole.
        place_on(&mut sim, Place::new(at.clone(), bytes.clone()), usize::MAX);
        let whole = sim.contents(&sim_name(&at));
        assert_eq!(whole, bytes, "seed {seed}: a place after a crash puts the whole file");
    }
    Ok(())
}

/// The invariant: after a crash at any step of putting a texts file in place, what is at its
/// name is no file, or one that decodes and whose every text hashes to its listed id; a place
/// after the crash puts the whole file. Control: `the_oracle_catches_a_rename_before_the_sync`.
#[test]
fn a_crash_while_the_texts_are_placed_leaves_none_or_all_of_them() {
    crash_places(Plant::None).unwrap();
}

/// The control: with the part renamed over the name before its bytes are synced, some crash
/// leaves a file at the name whose texts are torn, and the oracle finds it.
#[test]
fn the_oracle_catches_a_rename_before_the_sync() {
    let caught = crash_places(Plant::RenamesBeforeTheSync);
    assert!(caught.is_err(), "no seed's crash left a torn texts file");
}
