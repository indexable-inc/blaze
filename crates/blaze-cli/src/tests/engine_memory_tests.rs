//! Where `engine-check`'s memory goes (engine_memory.rs), and the names stage's dependency
//! reads in place (engine_names.rs `ReadExports`, engine_records.rs `EngineRecords::exports`),
//! on engine_check_tests.rs's three-crate chain (`c` reads `b` reads `a`: each names wave past
//! the first reads its dependencies' exports sections).
//!
//! Before: a `CrateScope` task's preparation read each dependency's whole record out of the
//! store into the wave's frame (work.rs `payload`), then copied its exports section 16-aligned
//! again (`aligned`): two copies of every exports section a wave read, held to the wave's end.
//! Now it is a borrow of the store's mapping: "records read (Names): .. copied 0".
//!
//! A child of main.rs's tests: engine_check_tests.rs lends `engine_from`, `three_crates`,
//! `entries` and `profile`.
//!
//! NOT RUN by the writer (a write-only lane, uncompiled).

use super::engine_check_tests::{engine_from, entries, profile, three_crates};
use super::*;
use blaze_engine::Stage;

/// The chain checked fresh in a world of `seed`: the command's counts and its profile.
fn chain(seed: u64) -> (engine_check::Checked, std::string::String) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    three_crates(&io);
    let (_, checked) = engine_from(&io, 2, true);
    let text = profile(&io);
    (checked, text)
}

/// The number after `label` on `line` (its first occurrence).
fn number_after(line: &str, label: &str) -> u64 {
    let Some(at) = line.find(label) else { panic!("`{label}` is on the line: {line}") };
    let digits: std::string::String = line[at + label.len()..].trim_start().chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or_else(|_| panic!("a number after `{label}`: {line}"))
}

/// `line` from `label`'s first occurrence on.
fn after<'l>(line: &'l str, label: &str) -> &'l str {
    let Some(at) = line.find(label) else { panic!("`{label}` is on the line: {line}") };
    &line[at..]
}

/// THE NAMES STAGE READS IN PLACE: each dependency's exports section a names wave reads is a
/// borrow of the store's mapping, counted as read and copied 0, and every record put is the
/// copying route's, byte for byte (the digest). CONTROL: the same chain through the copying
/// route (the route before, counted) copies twice the exports bytes it reads, so the 0 is a
/// count, not a counter that counts nothing.
#[test]
fn engine_memory_names_reads_exports_in_place() {
    let (in_place, _) = chain(31);
    let (copying, _) = crate::engine_memory::with_copying_route(|| chain(31));
    let names = Stage::Names.index();
    let read = in_place.records.reads[names];
    let control = copying.records.reads[names];
    println!("records read (Names), in place: {read}; sections {}", read.sections);
    println!("records read (Names), copying: {control}; sections {}", control.sections);
    println!("records put: digest in place {:?}, copying {:?}", in_place.records.digest(), copying.records.digest());
    assert!(read.records >= 2, "b read a's exports and c read b's, in place: {read}\n{}", entries(&in_place));
    assert!(read.sections > 0, "the exports sections read have bytes: {read}");
    assert_eq!(read.copied, 0, "no byte of a dependency's exports copied on the way: {read}");
    assert_eq!(control.records, read.records, "both routes read the same records: in place {read}, copying {control}");
    assert_eq!(control.sections, read.sections, "both routes read the same sections: in place {read}, copying {control}");
    assert_eq!(control.copied, 2 * control.sections, "the copying route copies each exports section twice (the record out, then aligned): {control}");
    assert_eq!(in_place.records.digest(), copying.records.digest(), "the records put are the copying route's, byte for byte");
    assert_eq!(in_place.status.0, copying.status.0, "the status is the copying route's");
    assert_eq!(in_place.bodies.0, copying.bodies.0, "the body notes are the copying route's");
}

/// THE MEMORY LINES: one `memory (Stage)` line for every stage that entered a task, and for
/// `Names` the wave's frame, the tasks' scratch, the ten largest tasks and what the names world
/// keeps, each non-zero on the chain (the chain resolves three crates in three waves, each task
/// committing scratch in its own frame).
#[test]
fn engine_memory_prints_a_line_for_every_stage() {
    let (checked, text) = chain(33);
    let lines: std::vec::Vec<&str> = text.lines().filter(|line| line.starts_with("memory (")).collect();
    for line in lines.iter() {
        println!("{line}");
    }
    for stage in Stage::ALL {
        if checked.stages[stage.index()].entered == 0 {
            continue;
        }
        let head = format!("memory ({stage:?}) (bytes): ");
        let Some(line) = lines.iter().find(|line| line.starts_with(head.as_str())) else { panic!("a memory line for {stage:?}:\n{text}") };
        assert!(number_after(line, "at its end regions") > 0, "{stage:?}: the process holds regions at its end: {line}");
        assert!(number_after(line, "frames") > 0, "{stage:?}: the frames' high water is counted: {line}");
    }
    let Some(names) = lines.iter().find(|line| line.starts_with("memory (Names) (bytes): ")) else { panic!("a memory line for Names:\n{text}") };
    let reads = after(names, "records read");
    assert!(number_after(reads, "records read") > 0, "the names stage read its dependencies' records: {names}");
    assert_eq!(number_after(reads, "(copied"), 0, "and copied none of them: {names}");
    assert!(number_after(names, "the wave's frame most") > 0, "a wave's frame committed its jobs: {names}");
    assert!(number_after(names, "task scratch at once at most") > 0, "the tasks' scratch is counted: {names}");
    let summed = after(names, "tasks' scratch");
    assert!(number_after(summed, "tasks' scratch") > 0, "the tasks' scratch summed: {names}");
    assert!(number_after(summed, " in ") >= 2, "the chain's names stage ran in waves: {names}");
    assert!(names.contains("the largest") && names.contains("crate `"), "the largest task is named by its crate: {names}");
    let Some(largest) = lines.iter().find(|line| line.starts_with("memory (Names) task scratch, the 10 largest (bytes):")) else { panic!("the largest tasks' line:\n{text}") };
    assert!(largest.matches("crate `").count() >= 3, "each of the chain's crates listed by its scratch: {largest}");
    let Some(kept) = lines.iter().find(|line| line.starts_with("memory (Names) kept to the command's end (bytes): ")) else { panic!("the kept line:\n{text}") };
    assert!(number_after(kept, "(bytes):") >= 3, "the three crates resolved are kept: {kept}");
    assert_eq!(number_after(kept, "their payloads not yet put"), 0, "every payload was handed to its record: {kept}");
    assert!(number_after(kept, "the names world's frame") > 0, "the names world's frame is in use: {kept}");
}
