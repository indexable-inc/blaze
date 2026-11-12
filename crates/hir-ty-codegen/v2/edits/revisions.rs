//! The revision tests' simulated driver (docs/harness.md, "Revision tests"): each case of
//! `crates/blaze-engine/tests/revisions/cases` that names `sim`, run in this process with no
//! daemon, no file system and no clock, under a number of seeds.
//!
//! What runs is the pipeline from source text to code records: the front end (names, types,
//! MIR) over one database whose file text each revision sets, and the code generator's
//! driver with the memo and the store the revisions before left, every store hit recomputed
//! and compared (the store check, all of them). After each revision a fresh database of the
//! same text is built with an empty memo and store, and the edit fuzzer's oracle
//! ([`super::verdict`]) holds the incremental build to it: every function's code, relocations
//! and data, the statics, the symbols, the keys and the digests, byte for byte.
//!
//! Where the pipeline stops, and what stands in:
//!
//! - It ends at code records. Nothing is linked and nothing runs, so `prints`, `says`, `link`,
//!   `noop`, `max_units` and `max_ms` are not seen ([`check::SIM_BLIND`]); nor are the
//!   engine's memo counts, which this driver does not read yet.
//! - The crate is the case's `src/lib.rs` alone, over minicore ([`MINICORE`]) in place of
//!   core and std: a case under this driver keeps what it simulates in a `#![no_std]` library
//!   and prints from a `src/main.rs` only the real driver builds.
//! - No daemon: no plane, no VFS, no watcher, no image. A `stop` or a `kill` between
//!   revisions is a new database and a new memo over the store that was kept (the store is
//!   what a restart keeps here; the daemon also restores its database image, which this does
//!   not model). An edit by rename, or by truncate then write, shows the database the states
//!   a reader can see between the two writes (the file empty, or cut short) and builds on
//!   each before the whole text is there.
//!
//! A seed draws what a case leaves open: whether allocations may move to the stack, a
//! restart before any revision, a torn intermediate state before any edit, a build repeated.
//! Seed 0 draws nothing: the case as written. A failure names its seed, and
//! `BLAZE_REVISIONS_SEED=<seed>` runs that seed alone.

#[path = "../../../blaze-engine/tests/revisions/check.rs"]
mod check;
#[path = "../../../blaze-engine/tests/revisions/spec.rs"]
mod spec;

use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Instant,
};

use blaze_codegen::{driver::Built, memo::Memo};
use blaze_v2_proof::Trace;

use self::{
    check::Row,
    spec::{Observed, Step, Test},
};
use super::{Kept, TestDB, build, database, traced, verdict};

/// The parts of minicore a simulated crate is built over.
const MINICORE: &str = "sized, copy, clone, drop, transmute, size_of, unsize, coerce_unsized, dispatch_from_dyn, add, option";

/// splitmix64: the driver's draws, a function of the seed alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// One time in `n`; never for seed 0, whose state is not advanced.
    fn one_in(&mut self, n: u64) -> bool {
        self.next() % n == 0
    }
}

fn fixture(body: &str) -> String {
    format!("//- minicore: {MINICORE}\n//- /lib.rs crate:app\n{body}")
}

/// The simulated crate's text after revision `index`.
fn text_at(test: &Test, index: usize) -> String {
    let files = test.files_at(index);
    let bytes = files.get("src/lib.rs").unwrap_or_else(|| {
        panic!("{}: a case under the simulated driver keeps the crate it simulates in src/lib.rs", test.name)
    });
    String::from_utf8(bytes.clone()).expect("src/lib.rs is UTF-8") // clone: the revision's text, owned
}

/// One incremental state: the database, the memo and the store the builds so far left.
struct State {
    db: TestDB,
    file: base_db::FileId,
    memo: Memo,
    store: Kept,
    /// The trace of the last build that succeeded.
    before: Trace,
    heap_to_stack: bool,
    /// What the driver did, for a failure's message.
    log: Vec<String>,
    panics: u64,
    first_panic: String,
    /// The functions made since the last revision's action, by every build between: a build
    /// on a torn state, or one repeated, makes what the revision's own build then finds.
    made: BTreeSet<String>,
}

impl State {
    /// A build of the database as it is, its puts kept. A panic is caught and counted: the
    /// daemon catches a job's too, and none is allowed.
    fn build(&mut self) -> Result<Built, String> {
        let built = catch_unwind(AssertUnwindSafe(|| {
            build(&self.db, &self.memo, &self.store, blaze_store_check::Setting::ALL, self.heap_to_stack)
        }));
        match built {
            Ok(built) => built,
            Err(payload) => {
                let said = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("a panic with no message")
                    .to_owned();
                self.panics += 1;
                if self.first_panic.is_empty() {
                    self.first_panic = said.clone(); // clone: kept for the row, returned as the error
                }
                Err(format!("panicked: {said}"))
            }
        }
    }

    fn keep(&mut self, built: Built) {
        self.made.extend(made(&built));
        let (trace, _) = traced(&built, &self.store);
        self.store.0.extend(built.puts.into_iter().map(|it| (it.key, it.bytes)));
        self.before = trace;
    }

    /// A restart: the database and the memo are the process's and are lost; the store stays.
    fn restart(&mut self, text: &str) {
        let (mut db, file) = database(&fixture(text));
        db.set_file_text(file, text);
        (self.db, self.file, self.memo) = (db, file, Memo::new());
    }

    /// A state between two writes of a save, built on: what a reader that looks then sees.
    fn torn(&mut self, text: &str, what: &str) {
        self.db.set_file_text(self.file, text);
        self.log.push(format!("the file {what} ({} bytes), built on", text.len()));
        if let Ok(built) = self.build() {
            self.keep(built);
        }
    }
}

/// The functions `built` made (compiled and put in the store), by name.
fn made(built: &Built) -> Vec<String> {
    let put: BTreeSet<_> = built.puts.iter().map(|it| it.key).collect();
    let mut names: Vec<String> = built
        .kernel
        .iter()
        .filter(|entry| put.contains(&entry.key))
        .map(|entry| match built.debug_names.iter().find(|(key, _)| *key == entry.instance) {
            Some((_, name)) => name.clone(), // clone: the row's own copy
            None => format!("{:?}", entry.instance),
        })
        .collect();
    names
}

/// `test` under `seed`: its rows, and what the driver did.
fn one(test: &Test, seed: u64) -> (Vec<Row>, Vec<String>) {
    let mut rng = Rng(seed);
    let drawn = seed != 0;
    let mut text = text_at(test, 0);
    let (db, file) = database(&fixture(&text));
    let mut state = State {
        db,
        file,
        memo: Memo::new(),
        store: Kept::default(),
        before: Trace::default(),
        heap_to_stack: drawn && rng.one_in(2),
        log: Vec::new(),
        panics: 0,
        first_panic: String::new(),
        made: BTreeSet::new(),
    };
    state.db.set_file_text(state.file, &text);
    let mut rows = Vec::new();
    for (index, revision) in test.revisions.iter().enumerate() {
        state.log.push(format!("{}:", revision.label));
        if index > 0 && drawn && rng.one_in(4) {
            state.log.push("a restart (drawn)".to_owned());
            state.restart(&text);
        }
        for step in &revision.steps {
            match step {
                Step::Edit | Step::EditByRename | Step::EditTruncateWrite => {
                    let new = text_at(test, index);
                    if index > 0 && new != text {
                        let hostile = match step {
                            Step::Edit => drawn && rng.one_in(4),
                            _ => true,
                        };
                        if hostile {
                            // Between an unlink and a create, or a truncate and a write, the
                            // file is empty; a write in two parts is cut short at any byte.
                            state.torn("", "empty");
                            if drawn {
                                let cut = new.floor_char_boundary(rng.next() as usize % new.len().max(1));
                                state.torn(&new[..cut], "cut short");
                            }
                        }
                        state.db.set_file_text(state.file, &new);
                        state.log.push("the edit".to_owned());
                        text = new;
                    }
                }
                Step::Stop | Step::Kill => {
                    state.log.push("a restart".to_owned());
                    state.restart(&text);
                }
                Step::Touch(_) => {
                    state.log.push("the same text set again".to_owned());
                    state.db.set_file_text(state.file, &text);
                }
                Step::Do(_) => {
                    state.log.push("a build".to_owned());
                    if let Ok(built) = state.build() {
                        state.keep(built);
                    }
                }
            }
        }
        if drawn && rng.one_in(4) {
            state.log.push("a build, repeated (drawn)".to_owned());
            if let Ok(built) = state.build() {
                state.keep(built);
            }
        }
        let began = Instant::now();
        let incremental = state.build();
        let ms = began.elapsed().as_millis();
        let (mut fresh, fresh_file) = database(&fixture(&text));
        fresh.set_file_text(fresh_file, &text);
        let clean = build(&fresh, &Memo::new(), &Kept::default(), blaze_store_check::Setting::OFF, state.heap_to_stack);
        let mut seen = Observed { ok: incremental.is_ok(), ms, panics: state.panics, ..Observed::default() };
        seen.first_panic = std::mem::take(&mut state.first_panic);
        state.panics = 0;
        let mut clean_seen = Observed { ok: clean.is_ok(), ..Observed::default() };
        match (incremental, clean) {
            (Ok(incremental), Ok(clean)) => {
                let (inc_trace, inc_rows) = traced(&incremental, &state.store);
                let (cl_trace, cl_rows) = traced(&clean, &Kept::default());
                // The oracle's findings are the bytes compared: none is the clean build's.
                let found = verdict(&state.before, (&incremental, &inc_trace, &inc_rows), (&clean, &cl_trace, &cl_rows));
                seen.image = Some(found.join("\n").into_bytes());
                clean_seen.image = Some(Vec::new());
                seen.why = match found.is_empty() {
                    true => "the incremental build's records are the clean build's".to_owned(),
                    false => format!("the incremental build is not the clean one ({} findings):\n- {}", found.len(), found.join("\n- ")),
                };
                state.keep(incremental);
                seen.redone = Some(std::mem::take(&mut state.made).into_iter().collect());
            }
            (incremental, clean) => {
                seen.said = incremental.err().unwrap_or_default();
                clean_seen.said = clean.err().unwrap_or_default();
                state.made.clear();
            }
        }
        // Under this driver every revision's records are compared, asked for or not.
        let mut revision = revision.clone(); // clone: the revision with `bytes` on
        revision.expect.bytes = true;
        let failed = check::held(&revision, &seen, Some(&clean_seen), check::SIM_BLIND);
        rows.push(Row { label: revision.label, seen, failed, clean_ok: Some(clean_seen.ok) });
    }
    (rows, state.log)
}

/// `name` under the simulated driver, under each of its seeds.
fn run(name: &str) {
    let test = Test::read(&spec::cases(), name);
    assert!(
        test.drivers.iter().any(|it| it == "sim"),
        "{name}: listed under the simulated driver, which its `drivers` does not name"
    );
    let seeds: Vec<u64> = match std::env::var("BLAZE_REVISIONS_SEED") {
        Ok(seed) => vec![seed.parse().unwrap_or_else(|_| panic!("BLAZE_REVISIONS_SEED={seed} is no seed"))],
        Err(_) => (0..test.seeds).collect(),
    };
    for seed in seeds {
        let (rows, log) = one(&test, seed);
        let note = format!(
            "seed {seed} (`BLAZE_REVISIONS_SEED={seed}` runs it alone); what the driver did:\n  {}\n",
            log.join("\n  ")
        );
        check::verdict(&test, &format!("sim seed {seed}"), &rows, &note);
    }
}

macro_rules! sim {
    ($($name:ident)*) => {
        $(
            #[test]
            fn $name() {
                run(stringify!($name));
            }
        )*
        const SIM: &[&str] = &[$(stringify!($name)),*];
    };
}

sim! {
    a_body_edit_remakes_one_function
    a_changed_const_remakes_its_two_readers
    a_changed_struct_layout_remakes_what_uses_it
    a_changed_trait_impl_remakes_what_resolves_to_it
    a_comment_edit_remakes_nothing
    a_renamed_function_keeps_its_code
    a_save_by_rename_is_read_whole
    a_signature_change_remakes_the_callers
    a_touch_without_a_change
    an_added_and_a_removed_function
    an_edit_after_a_kill
    an_edit_after_a_restart
    an_impl_overriding_a_provided_method_changes_its_vtable
}

/// Every case that names `sim` runs here: one no list holds would never run.
#[test]
fn every_sim_case_is_listed() {
    let mut unlisted = Vec::new();
    for entry in std::fs::read_dir(spec::cases()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_dir()
            && Test::read(&spec::cases(), &name).drivers.iter().any(|it| it == "sim")
            && !SIM.contains(&name.as_str())
        {
            unlisted.push(name);
        }
    }
    assert!(unlisted.is_empty(), "cases that name `sim` and no `#[test]` runs: {unlisted:?}; add each to `sim!`");
}
