//! `blaze-solo '{"checkout": "<dir>", "state": "<dir>", "edition": "2021"}'`: one command
//! over a checkout with no daemon. It sweeps the checkout against its leaf table, carries the
//! saved session by what changed, asks `items` of each `.rs` file added or changed, saves, and
//! prints its [`blaze_solo::Report`] as one JSON value. Exit 0, or 2 with an error naming the
//! file and the rule that refused it.
//!
//! The state directory holds the store (`store/`, blaze-cas's, shared by every checkout it
//! serves) and a directory a checkout (`solo/<id>/`: its table and its slot files). One writer
//! a store: the command holds `store.lock` for its whole run, and a second command on the
//! same state is refused, not queued.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use blaze_core::{Done, Driver as _, Effect, FileName, LockMode, Tag, run_until_idle};
use blaze_front_items::Reading;
use blaze_ids::{ContentId, Hasher};
use blaze_kernel::CodeId;
use blaze_solo::{Ask, Codes, Command, Config, drive::drive};
use serde::Deserialize;
use syntax::{Edition, Extensions};

/// The command's one input (rule 42).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    checkout: PathBuf,
    state: PathBuf,
    /// `2015`, `2018`, `2021` or `2024`: how every file is read.
    edition: String,
}

struct Failed {
    thing: String,
    rule: &'static str,
    why: String,
}

impl Failed {
    fn new(thing: impl std::fmt::Display, rule: &'static str, why: impl std::fmt::Display) -> Failed {
        Failed { thing: thing.to_string(), rule, why: why.to_string() }
    }
}

const TAGS: u64 = 1 << 40;
const LOCK_TAG: Tag = Tag(1);

fn main() -> ExitCode {
    match run() {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(failed) => {
            eprintln!("blaze-solo: {}: {} (refused by: {})", failed.thing, failed.why, failed.rule);
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<String, Failed> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [json] = args.as_slice() else {
        return Err(Failed::new("the command line", "blaze-solo takes one JSON value", format_args!("{} arguments", args.len())));
    };
    let input: Input = serde_json::from_str(json)
        .map_err(|e| Failed::new("the command's JSON value", "one value of {checkout, state, edition}, no other field", e))?;
    let edition = Edition::iter()
        .find(|it| it.number().to_string() == input.edition)
        .ok_or_else(|| Failed::new(&input.edition, "an edition is 2015, 2018, 2021 or 2024", "names no edition"))?;
    let checkout = std::fs::canonicalize(&input.checkout)
        .map_err(|e| Failed::new(input.checkout.display(), "the checkout is a directory that exists", e))?;
    std::fs::create_dir_all(&input.state)
        .map_err(|e| Failed::new(input.state.display(), "the state directory can be made", e))?;
    let state = std::fs::canonicalize(&input.state)
        .map_err(|e| Failed::new(input.state.display(), "the state directory can be made", e))?;
    if let Ok(inside) = state.strip_prefix(&checkout) {
        let inside = inside.to_str().unwrap_or_default();
        if !blaze_vfs::watch::fixed_out(inside) {
            return Err(Failed::new(
                state.display(),
                "the state directory is outside the checkout's view (or under its `target`)",
                "it is inside the view, so every command would see its own writes as changes",
            ));
        }
    }
    let id = format!("{:x}", ContentId::of("blaze solo checkout 1", checkout.as_os_str().as_encoded_bytes()));
    let mine = state.join("solo").join(&id[..16]);
    std::fs::create_dir_all(&mine).map_err(|e| Failed::new(mine.display(), "the checkout's state directory can be made", e))?;

    let mut driver = blaze_core::native(Path::new("/"), 4)
        .map_err(|e| Failed::new("the native driver", "the platform's driver starts", e))?;
    // One writer a store (blaze-cas): held for the whole command, never waited for.
    let lock = name(&state.join("store.lock"))?;
    let mut effects = vec![Effect::Lock { file: lock.clone(), mode: LockMode::Exclusive, tag: LOCK_TAG }];
    let mut locked = None;
    run_until_idle(&mut driver, &mut effects, |completion, _| locked = Some(completion.done))
        .map_err(|e| Failed::new(&lock, "the lock completes", e))?;
    match locked {
        Some(Done::Locked) => {}
        Some(Done::Contended) => {
            return Err(Failed::new(&lock, "one command a store at a time", "another command holds the store"));
        }
        other => return Err(Failed::new(&lock, "the lock completes", format_args!("{other:?}"))),
    }
    let mut store = blaze_cas::Driven::open_with(&state, blaze_cas::Loader::Thread { root: state.clone() })
        .map_err(|e| Failed::new(state.join("store").display(), "the store opens", e))?;

    let config = Config {
        checkout: name(&checkout)?,
        state: name(&mine)?,
        reading: Reading { edition, extensions: Extensions::NONE },
        ask: Ask::Changed,
        codes: codes(),
        tags: TAGS,
    };
    let mut effects = Vec::new();
    let mut command = Command::start(config, driver.wall_ns(), driver.now(), &mut effects)
        .map_err(|e| Failed { thing: e.thing, rule: e.rule, why: e.why })?;
    let outcome = drive(&mut driver, &mut command, &mut effects, &mut store)
        .map_err(|e| Failed { thing: e.thing, rule: e.rule, why: e.why })?;
    serde_json::to_string(&outcome.report).map_err(|e| Failed::new("the report", "a report is JSON", e))
}

/// An absolute path as a name under the driver's root, `/`.
fn name(path: &Path) -> Result<FileName, Failed> {
    path.to_str()
        .and_then(|it| it.strip_prefix('/'))
        .and_then(FileName::new)
        .ok_or_else(|| Failed::new(path.display(), "a path blaze-solo names is absolute UTF-8 with no `.` or `..`", "it is not"))
}

/// The steps' code identity: the daemon's carry's (`front_carry::CODE` and `code`,
/// crates/blaze-cargo/src/serve/front_carry.rs:66-77, private there), so the two keep one
/// set of entries. A copy until it moves into blaze-front-items, where both take it from;
/// then until `fn_info(..).code_id` gives each step its own (rule 52).
fn codes() -> Codes {
    let code = |name: &str, version: u8| {
        let mut hasher = Hasher::new("blaze v2 front carry step code 1");
        hasher.part(name.as_bytes()).part(&[version]);
        CodeId(hasher.finish())
    };
    Codes { parse: code("parse", 1), items: code("items", 1), lines: code("lines", 1), modules: code("modules", 1) }
}
