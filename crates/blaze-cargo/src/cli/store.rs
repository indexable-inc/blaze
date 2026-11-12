//! `blaze store export`, `blaze store import` and `blaze store install`: packs out of and
//! into this home's store (docs/remote-cache.md, "Packs"). Lane packs-1112.
//!
//! ```text
//! blaze store export [--all] [--raw] --pin <name> --out <dir> [--sign-key <file>]
//! blaze store import <dir or http(s) base> [--pin <name>]... [--unsigned] [--trust <key>]...
//! blaze store install [--from <dir or http(s) base>]
//! ```
//!
//! - `export` writes the records the store's pin `<name>` keeps (`toolchain`: the standard
//!   library's; `bstd`; any pin) as one pack under `<dir>`, which is then the directory to
//!   copy to a mirror or a release's bucket as it is. With `--all` the selection is every
//!   record of the store that may travel (everything this home's builds used), and `<name>`
//!   is the pin an importing store keeps them under. `--sign-key` names a file holding an
//!   Ed25519 secret (64 hex digits); the head is signed with it. The body is written as
//!   compressed spans (zstd; the store reads records out uncompressed, so it is their one
//!   compression), or as it is with `--raw`. Nothing is uploaded: that
//!   is someone's own command, with the owner's word.
//! - `import` reads the packs of this blaze's scope at a base and puts their records into
//!   the store, pinned under the manifest's pin before the first is put and each checked
//!   against its manifest before it is kept. A head must be signed by a key `packs.json`
//!   trusts or `--trust` names; `--unsigned` takes a head no one signed, on the user's word.
//! - `install` is `import` from the base `packs.json` names as `install` (or `--from`), of
//!   the pins it names (the standard library's and bstd's by default), signed heads only.
//!   With no base set it says so and does nothing. It also runs by itself the first time a
//!   command starts a daemon on a home that has no store yet ([`first_use`]), when a base
//!   is set: no address is compiled in.
//!
//! With a daemon on the home, which holds the store, the command is asked of it
//! ([`blaze_api::Request::Store`]) and run there over its store; with none, it is run here
//! over the store opened by this process. Either way it is [`run`], over
//! `serve::packs::stored::Records`. This file reads the words and prints.

use std::path::{Path, PathBuf};

use blaze_core::exit::Exit;
use blaze_pack::{Id, Trust, head::Vouched};

use super::say;
use crate::serve::packs::{
    self, Base, Config,
    stored::{self, Records},
};

const USAGE_TEXT: &str = "blaze store export [--all] [--raw] --pin <name> --out <dir> [--sign-key <file>] | blaze store import <dir or http(s) base> [--pin <name>]... [--unsigned] [--trust <key>]... | blaze store install [--from <dir or http(s) base>]";

/// `blaze store`, with the words after it (`export ...`, `import ...`, `install ...`).
pub fn main(words: &[&str], color: bool) -> i32 {
    match asked(words) {
        Ok(out) => {
            blaze_report::out!("{out}");
            0
        }
        Err(what) if what == USAGE_TEXT => {
            blaze_report::err!("{}", say::error(USAGE_TEXT, color));
            Exit::Usage.code()
        }
        Err(what) => {
            blaze_report::err!("{}", say::error(&what, color));
            Exit::Failed.code()
        }
    }
}

/// The command `words`, run by the daemon that holds this home's store, or here when none
/// does.
fn asked(words: &[&str]) -> Result<String, String> {
    // The daemon's directory is not this command's: every path is made whole here.
    let words = whole(words)?;
    match super::daemon::connect(super::daemon::Start::Never) {
        Ok(Some(client)) => {
            drop(client);
            let mut out = String::new();
            super::observe::ask(
                blaze_api::Request::Store { words },
                super::daemon::Start::Never,
                |response| {
                    if let blaze_api::Response::Diagnostic(said) = response.into_owned() {
                        out.push_str(&said.message);
                        out.push('\n');
                    }
                    Ok(())
                },
            )?;
            Ok(out)
        }
        Ok(None) => {
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            let mut store = opened()?;
            run(&words, &mut stored::Driven::new(&mut store))
        }
        Err(error) => Err(format!("{error:#}")),
    }
}

/// `words` with each path in them absolute: the values of `--out`, `--sign-key` and `--from`
/// and an import's place, unless it is an address.
fn whole(words: &[&str]) -> Result<Vec<String>, String> {
    let absolute = |path: &str| match path.starts_with("http://") || path.starts_with("https://") {
        true => Ok(path.to_owned()),
        false => std::path::absolute(path)
            .map(|path| path.display().to_string())
            .map_err(|error| format!("{path}: {error}")),
    };
    let mut out = Vec::with_capacity(words.len());
    let mut at = 0;
    while at < words.len() {
        let word = words[at];
        out.push(word.to_owned());
        match word {
            "--out" | "--sign-key" | "--from" => {
                if let Some(path) = words.get(at + 1) {
                    out.push(absolute(path)?);
                    at += 1;
                }
            }
            "--pin" | "--trust" => {
                if let Some(value) = words.get(at + 1) {
                    out.push((*value).to_owned());
                    at += 1;
                }
            }
            place if at > 0 && words[0] == "import" && !place.starts_with('-') => {
                out.pop();
                out.push(absolute(place)?);
            }
            _ => {}
        }
        at += 1;
    }
    Ok(out)
}

fn usage<T>() -> Result<T, String> {
    Err(USAGE_TEXT.to_owned())
}

/// The value after a flag.
fn value<'a>(words: &mut std::slice::Iter<'a, &'a str>) -> Result<&'a str, String> {
    words.next().copied().ok_or_else(|| USAGE_TEXT.to_owned())
}

/// This home's store, opened here: no daemon is on the home ([`asked`] looked).
fn opened() -> Result<blaze_cas::Driven, String> {
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    // Pages are loaded as they are read, within the pool's budget: a store is not read
    // whole into memory to export a pin of it.
    blaze_cas::Driven::open_with(&state, blaze_cas::Loader::Thread { root: state.clone() })
}

fn seed_at(path: &Path) -> Result<[u8; 32], String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("the signing key {}: {error}", path.display()))?;
    Id::from_hex(text.trim())
        .map(|id| id.0)
        .ok_or_else(|| format!("the signing key {} does not hold 64 hex digits", path.display()))
}

/// The command `words` (`export ...`, `import ...`, `install ...`, every path absolute) over
/// `records`: what it did, a line each, or why not. A misuse is [`USAGE_TEXT`].
pub(crate) fn run(words: &[&str], records: &mut impl Records) -> Result<String, String> {
    match words.split_first() {
        Some((&"export", rest)) => export(rest, records),
        Some((&"import", rest)) => import(rest, records),
        Some((&"install", rest)) => install(rest, records),
        _ => usage(),
    }
}

fn export(words: &[&str], records: &mut impl Records) -> Result<String, String> {
    let (mut pin, mut out, mut key, mut all, mut raw) = (None, None, None, false, false);
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match *word {
            "--pin" => pin = Some(value(&mut words)?),
            "--out" => out = Some(PathBuf::from(value(&mut words)?)),
            "--sign-key" => key = Some(PathBuf::from(value(&mut words)?)),
            "--all" => all = true,
            "--raw" => raw = true,
            _ => return usage(),
        }
    }
    let (Some(pin), Some(out)) = (pin, out) else { return usage() };
    let seed = key.as_deref().map(seed_at).transpose()?;
    let scope = packs::scope()?;
    let places = packs::home_places()?;
    let done = stored::export(records, pin, &scope, &out, seed.as_ref(), all, raw, &|bytes| places.tied(bytes))?;
    Ok(format!(
        "exported {} as pin `{pin}` for {scope}: pack {}, {} records, {} bytes, {} head, under {}{}\n",
        if all { "every record that may travel" } else { "the store's pin" },
        done.pack,
        done.records,
        done.bytes,
        if done.signed { "a signed" } else { "an unsigned" },
        out.display(),
        match (done.absent, done.held) {
            (0, 0) => String::new(),
            (absent, held) => format!(
                "; not in the pack: {absent} keys the pin names with no record in the store, {held} records that hold a directory of this machine"
            ),
        }
    ))
}

/// The packs of `pins` at `base`, imported: a line for each.
fn imported(
    records: &mut impl Records,
    base: &Base,
    pins: &[String],
    trust: &Trust,
    unsigned: bool,
) -> Result<String, String> {
    let scope = packs::scope()?;
    let mut said = String::new();
    for pin in pins {
        match stored::import(records, base, &scope, pin, trust, unsigned)? {
            None => said.push_str(&format!("{} has no pack of pin `{pin}` for {scope}\n", base.named())),
            Some(done) => said.push_str(&format!(
                "imported pin `{}` (pack {}) from {}: {} records put ({} bytes), {} already there; vouched for by {}\n",
                done.pin,
                done.pack,
                base.named(),
                done.summary.put,
                done.summary.bytes,
                done.summary.present,
                match done.vouched {
                    Vouched::Signer(key) => format!("the trusted key {}", Id(key)),
                    Vouched::User => "you (`--unsigned`)".to_owned(),
                }
            )),
        }
    }
    Ok(said)
}

fn settings() -> Result<Config, String> {
    match Config::read()? {
        Some(config) => Ok(config),
        None => Config::parse("{}"),
    }
}

fn import(words: &[&str], records: &mut impl Records) -> Result<String, String> {
    let config = settings()?;
    let (mut base, mut pins, mut unsigned) = (None, Vec::new(), false);
    let mut trusted = config.trusted.clone();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match *word {
            "--pin" => pins.push(value(&mut words)?.to_owned()),
            "--unsigned" => unsigned = true,
            "--trust" => {
                let key = Id::from_hex(value(&mut words)?)
                    .ok_or_else(|| "`--trust` takes a public key: 64 hex digits, lower case".to_owned())?;
                trusted.push(key.0);
            }
            flag if flag.starts_with('-') => return usage(),
            place if base.is_none() => base = Some(Base::parse(place)?),
            _ => return usage(),
        }
    }
    let Some(base) = base else { return usage() };
    if unsigned && matches!(base, Base::Url(_)) {
        return Err("`--unsigned` takes a directory you name, never an address: what comes over a network is taken only under a trusted signature".to_owned());
    }
    if pins.is_empty() {
        pins = config.pins;
    }
    imported(records, &base, &pins, &Trust::new(trusted), unsigned)
}

fn install(words: &[&str], records: &mut impl Records) -> Result<String, String> {
    let config = settings()?;
    let base = match words {
        [] => config.install.clone(),
        ["--from", place] => Some(Base::parse(place)?),
        _ => return usage(),
    };
    let Some(base) = base else {
        return Ok(format!(
            "no pack is installed: no base is set (`install` in {}, or `--from`); the standard library is built on first use\n",
            packs::SETTINGS
        ));
    };
    imported(records, &base, &config.pins, &config.trust(), false)
}

/// The first use of a home: called as a command is about to start a daemon. When the home
/// has no store yet and the settings name an `install` base, the configured pins' packs are
/// imported before the daemon starts, so its first build finds the standard library's
/// records. Nothing happens on a home that has a store, or with no base set.
///
/// A base that is set and cannot be read, or a pack that is refused, is this command's
/// error, naming it: the user set the base, so a first build that silently made the
/// standard library from source instead would hide that it is down (AGENTS.md rule 3). The
/// store exists from then on, so the next command builds; the error was said once.
pub fn first_use() -> Result<(), String> {
    let Some(config) = Config::read()? else { return Ok(()) };
    let Some(base) = &config.install else { return Ok(()) };
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    if state.join("store").exists() {
        return Ok(());
    }
    let _span = tracing::info_span!(target: "blaze", "first use: install packs", from = %base.named()).entered();
    let mut store = opened()?;
    let said = imported(&mut stored::Driven::new(&mut store), base, &config.pins, &config.trust(), false)
        .map_err(|why| format!("installing the packs of {} on this home's first use: {why}", base.named()))?;
    blaze_report::err!("{}", said.trim_end());
    Ok(())
}
