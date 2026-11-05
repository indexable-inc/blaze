//! blaze's own updates (docs/install.md, "Updates"). The daemon reads `latest.json` from
//! blaze's site every [`CHECK_EVERY`] off its loop ([`tick`], run by
//! `blaze_cargo::release::watch_updates`), with the ETag of the last read so an unchanged one is
//! a `304`, and keeps what it found in `update/state.json` under blaze's home. Before each
//! command the launcher reads that file, never the network, and says what [`policy::decide`]
//! says: a newer release, a pin behind the newest, a yanked release. `blaze update` installs the
//! newest release of the channel as the default, the release that runs outside pinned
//! workspaces (`default` under blaze's home), through the same verified fetch a pin uses
//! (`versions.rs`); `[update] mode = "auto"` has the daemon do so by itself. A pin is never
//! moved but by `blaze pin`.
//!
//! Files, under blaze's home (`BLAZE_HOME`, else `~/.blaze`, where `versions/` is):
//!
//! - `config.toml`: `[update] mode` (`"warn"`, the default; `"auto"`; `"off"`; `"offline"`, no
//!   network at all) and `channel` (`"stable"`, the default; `"canary"`). `blaze update
//!   --mode/--channel` writes it.
//! - `default`: the version `blaze update` installed, which runs outside pinned workspaces in
//!   the installed blaze's place. install.sh removes it, so the blaze installed last runs.
//! - `update/state.json`: the last check (written by the daemon only, whole or not at all).
//! - `update/updated`: a release `auto` installed that no command has said yet.

pub(crate) mod policy;

use std::{
    io::IsTerminal as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, bail};
use blaze_manifest::user_config::{self, UserConfigError};
use blaze_registry::http::{Bounds, HttpNotSuccessful, HyperHttp};
use serde_json::Value;

pub(crate) use self::policy::Pin;
use self::policy::{CHECK_EVERY, Channel, Fix, Here, Latest, Mode, Newest, Say, Version};
use crate::{pin, versions};

/// Where blaze's site serves `latest.json`, which release.yaml, promote.yaml and yank.yaml
/// write (docs/release.md, "latest.json"). `BLAZE_RELEASES` moves it with the releases
/// ([`versions::source`]).
const LATEST: &str = "https://ix.dev/blaze/latest.json";

/// How long one read of `latest.json` may take, connecting included.
const PATIENCE: Duration = Duration::from_secs(20);

/// The most `latest.json` may be: reading stops there.
const MOST: u64 = 1 << 20;

/// How much of a release's notes `latest.json` hands on: lines, and characters.
const NOTES_LINES: usize = 12;
const NOTES_MOST: usize = 2000;

/// The variable the launcher sets for the default release it runs in its place ([`default`]):
/// that version. A blaze started with it is the default, and runs no other; every process under
/// it (the daemon, a build script's `$CARGO`) inherits it, and none says the lines again.
pub(crate) const RUN_AS_DEFAULT: &str = "BLAZE_RUN_AS_DEFAULT";

/// What blaze's updates can't do, naming the file or the setting.
#[derive(Debug)]
pub(crate) enum UpdateError {
    /// No home directory, where blaze keeps its releases and its update state.
    NoHome,
    /// A file that is there and can't be read.
    Unread { file: PathBuf, error: std::io::Error },
    /// `config.toml` that can't be read, is not TOML, or sets `[update]` to what it doesn't take.
    Config(UserConfigError),
    /// `update/state.json` that is not what the daemon writes.
    State { file: PathBuf, why: String },
    /// `default` that names no release.
    Default { file: PathBuf, got: String },
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::NoHome => f.write_str(
                "blaze keeps its releases and its update state in .blaze in your home directory, and \
                 there is none (`BLAZE_HOME` names where)",
            ),
            UpdateError::Unread { file, error } => write!(f, "reading {}: {error}", file.display()),
            UpdateError::Config(error) => std::fmt::Display::fmt(error, f),
            UpdateError::State { file, why } => write!(
                f,
                "{} is not what blaze's daemon writes ({why}); the daemon writes it again at its next check",
                file.display()
            ),
            UpdateError::Default { file, got } => write!(
                f,
                "{} names {got:?}, which is not a release of blaze (`0.YYYYMMDD.N`): `blaze update` writes \
                 it, and removing it runs the installed blaze outside pinned workspaces",
                file.display()
            ),
        }
    }
}

impl std::error::Error for UpdateError {}

/// blaze's home: where `versions/` and `config.toml` are.
fn home() -> Result<PathBuf, UpdateError> {
    user_config::home().ok_or(UpdateError::NoHome)
}

fn state_file(home: &Path) -> PathBuf {
    home.join("update").join("state.json")
}

fn updated_file(home: &Path) -> PathBuf {
    home.join("update").join("updated")
}

fn default_file(home: &Path) -> PathBuf {
    home.join("default")
}

/// The text of `file`, `None` when there is none.
fn text(file: &Path) -> Result<Option<String>, UpdateError> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(UpdateError::Unread { file: file.to_owned(), error }),
    }
}

/// The seconds since the epoch now: the clock the drivers hand [`policy`].
fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |it| it.as_secs())
}

/// `[update]` of `config.toml`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    mode: Mode,
    channel: Channel,
}

impl Config {
    const DEFAULT: Config = Config { mode: Mode::Warn, channel: Channel::Stable };
}

/// `[update]` of `config.toml` under `home`, the defaults when there is none. Another key under
/// `[update]`, or a value none of those it takes, is an error naming it; other tables are not
/// this module's.
fn config(home: &Path) -> Result<Config, UpdateError> {
    let file = user_config::file(home);
    let Some(table) = user_config::read(&file).map_err(UpdateError::Config)? else { return Ok(Config::DEFAULT) };
    let mut config = Config::DEFAULT;
    let Some(update) = user_config::section(&table, &file, "update").map_err(UpdateError::Config)? else {
        return Ok(config);
    };
    let bad = |key: &str, wants: &str, got: &toml::Value| {
        UpdateError::Config(UserConfigError::setting(&file, key, wants, got))
    };
    for (key, value) in update {
        match key.as_str() {
            "mode" => {
                config.mode = value
                    .as_str()
                    .and_then(Mode::parse)
                    .ok_or_else(|| bad("update.mode", Mode::NAMES, value))?
            }
            "channel" => {
                config.channel = value
                    .as_str()
                    .and_then(Channel::parse)
                    .ok_or_else(|| bad("update.channel", "\"stable\" or \"canary\"", value))?
            }
            _ => {
                return Err(bad(
                    &format!("update.{key}"),
                    "a setting of blaze's: `[update]` has `mode` and `channel`",
                    value,
                ));
            }
        }
    }
    Ok(config)
}

/// `config.toml` under `home` with `[update] key = value`, the rest of it as it was.
fn set(home: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    let file = user_config::file(home);
    let shown = file.display();
    let mut doc: toml_edit::DocumentMut =
        text(&file)?.unwrap_or_default().parse().with_context(|| format!("reading {shown}"))?;
    let update =
        doc.entry("update").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let update =
        update.as_table_mut().with_context(|| format!("`update` in {shown} is not a table"))?;
    update.insert(key, toml_edit::value(value));
    versions::kept(&file, doc.to_string().as_bytes())
        .with_context(|| format!("writing {shown}"))?;
    println!("{shown}: [update] {key} = \"{value}\"");
    Ok(())
}

/// The last check, as `update/state.json` keeps it.
#[derive(Default, Debug, PartialEq)]
struct State {
    /// When `latest.json` was last asked for, whatever came of it.
    checked_at: Option<u64>,
    /// The ETag of the `latest.json` kept, sent back as `If-None-Match`.
    etag: Option<String>,
    /// The last `latest.json` read whole, as it came, and when.
    latest: Option<Value>,
    latest_at: Option<u64>,
    /// Why the last check read nothing new; `None` when it did, or found it unchanged.
    failed: Option<String>,
    /// A release `auto` failed to install, and why: not tried again by `auto`.
    install_failed: Option<(String, String)>,
    /// What the daemon found wrong on this machine that only its user can fix (a `default` or
    /// `config.toml` it can't read, an install `auto` could not make): said before each command
    /// until the next check finds it fixed. A network that fails is not one: that is `failed`.
    problem: Option<String>,
}

impl State {
    fn read(file: &Path) -> Result<State, UpdateError> {
        let Some(text) = text(file)? else { return Ok(State::default()) };
        let bad = |why: String| UpdateError::State { file: file.to_owned(), why };
        let value: Value = serde_json::from_str(&text).map_err(|error| bad(error.to_string()))?;
        if value.get("schema").and_then(Value::as_u64) != Some(1) {
            return Err(bad("no `schema` 1".to_owned()));
        }
        let string = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let install_failed = value.get("install_failed").filter(|it| !it.is_null()).map(|it| {
            let field =
                |key: &str| it.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            (field("version"), field("why"))
        });
        Ok(State {
            checked_at: value.get("checked_at").and_then(Value::as_u64),
            etag: string("etag"),
            latest: value.get("latest").filter(|it| !it.is_null()).cloned(),
            latest_at: value.get("latest_at").and_then(Value::as_u64),
            failed: string("failed"),
            install_failed,
            problem: string("problem"),
        })
    }

    fn write(&self, file: &Path) -> anyhow::Result<()> {
        let install_failed = self
            .install_failed
            .as_ref()
            .map(|(version, why)| serde_json::json!({"version": version, "why": why}));
        let value = serde_json::json!({
            "schema": 1,
            "checked_at": self.checked_at,
            "etag": self.etag,
            "latest": self.latest,
            "latest_at": self.latest_at,
            "failed": self.failed,
            "install_failed": install_failed,
            "problem": self.problem,
        });
        let mut bytes = serde_json::to_vec_pretty(&value)?;
        bytes.push(b'\n');
        versions::kept(file, &bytes).with_context(|| format!("writing {}", file.display()))
    }

    /// What the kept `latest.json` says, read as [`latest_of`] reads it now.
    fn latest(&self) -> Option<Latest> {
        let hosts = hosts().ok()?;
        self.latest.as_ref().and_then(|it| latest_of(it, &hosts).ok())
    }
}

/// The hosts a release page `latest.json` names may be on: GitHub's, and that of the mirror
/// `BLAZE_RELEASES` names.
fn hosts() -> anyhow::Result<Vec<String>> {
    let mut hosts = vec!["github.com".to_owned()]; // alloc: at most two hosts, once a read
    if let versions::Source::Mirror(base) = versions::source()? {
        let base = url::Url::parse(&base)
            .with_context(|| format!("`BLAZE_RELEASES` is {base}, which is not an address"))?;
        hosts.extend(base.host_str().map(str::to_owned));
    }
    Ok(hosts)
}

/// `latest.json` (docs/release.md, "latest.json") read: `schema` 1, `stable` and `canary`
/// each with `version`, `url` (an `https://` page on one of `hosts`) and `notes`, and
/// `yanked`, a version to its reason. What is printed of it is [`printable`]. The error names
/// the field.
fn latest_of(value: &Value, hosts: &[String]) -> Result<Latest, String> {
    if value.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err("it has no `schema` 1".to_owned());
    }
    let newest = |channel: &str| -> Result<Newest, String> {
        let it = value.get(channel).ok_or_else(|| format!("it has no `{channel}`"))?;
        let field = |key: &str| {
            it.get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("its `{channel}.{key}` is no string"))
        };
        let version = field("version")?;
        if !blaze_units::lock::is_version(version) {
            return Err(format!("its `{channel}.version` is {version:?}, not a release's version"));
        }
        let given = field("url")?;
        let url = url::Url::parse(given)
            .ok()
            .filter(|it| {
                it.scheme() == "https"
                    && it.host_str().is_some_and(|host| hosts.iter().any(|it| it == host))
            })
            .ok_or_else(|| {
                format!(
                    "its `{channel}.url` is {:?}, not an https:// page on {}",
                    printable(given),
                    hosts.join(" or ")
                )
            })?;
        Ok(Newest {
            version: version.to_owned(),
            url: printable(url.as_str()),
            notes: trimmed(field("notes")?),
        })
    };
    let mut yanked = Vec::new(); // alloc: the yanked releases, once a read
    if let Some(listed) = value.get("yanked") {
        let listed = listed.as_object().ok_or("its `yanked` is no object")?;
        for (version, reason) in listed {
            if !blaze_units::lock::is_version(version) {
                return Err(format!("its `yanked` names {version:?}, not a release's version"));
            }
            let reason =
                reason.as_str().ok_or_else(|| format!("its `yanked.{version}` is no string"))?;
            yanked.push((version.clone(), printable(reason).replace('\n', " ")));
        }
    }
    yanked.sort_by_key(|(version, _)| Version::parse(version));
    Ok(Latest { stable: newest("stable")?, canary: newest("canary")?, yanked })
}

/// `text` without control characters but newlines, and without format characters (Unicode's
/// `Cf`: bidirectional overrides, zero-width ones): what `latest.json` and a failed check say
/// is printed to a terminal, where an escape sequence would be the terminal's to run and a
/// bidirectional override would show other text than is there.
fn printable(text: &str) -> String {
    text.chars().filter(|it| *it == '\n' || !(it.is_control() || is_format(*it))).collect() // alloc: a line to print, once a read
}

/// Whether `c` is a format character, Unicode's general category `Cf` (16.0), which std does
/// not tell.
fn is_format(c: char) -> bool {
    matches!(
        u32::from(c),
        0xAD | 0x600..=0x605
            | 0x61C
            | 0x6DD
            | 0x70F
            | 0x890..=0x891
            | 0x8E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// The first [`NOTES_LINES`] lines of `notes`, at most [`NOTES_MOST`] characters, printable.
fn trimmed(notes: &str) -> String {
    let notes = printable(notes);
    let mut kept: String = notes.trim().lines().take(NOTES_LINES).collect::<Vec<_>>().join("\n"); // alloc: a release's notes, once a read
    if kept.len() > NOTES_MOST {
        kept.truncate(kept.floor_char_boundary(NOTES_MOST));
        kept.push_str(" ...");
    }
    kept
}

/// What a read of `latest.json` found.
enum Read {
    /// The same as the ETag sent: `304`.
    Same,
    New {
        etag: Option<String>,
        body: Vec<u8>,
    },
}

/// Reads `latest.json`, sending `etag` as `If-None-Match`. From [`LATEST`], or `latest.json`
/// at the top of what `BLAZE_RELEASES` names. One try, bounded by [`PATIENCE`]: the next check
/// is the retry.
fn read_latest(etag: Option<&str>) -> anyhow::Result<(String, Read)> {
    let address = match versions::source()? {
        versions::Source::Dir(dir) => {
            let file = dir.join("latest.json");
            let body =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            return Ok((file.display().to_string(), Read::New { etag: None, body }));
        }
        versions::Source::Ours => LATEST.to_owned(),
        versions::Source::Mirror(base) => format!("{}/latest.json", base.trim_end_matches('/')),
    };
    let http = HyperHttp::new(PATIENCE)?;
    let headers: Vec<(&str, &str)> = etag.map(|it| ("if-none-match", it)).into_iter().collect(); // alloc: one header, once a check
    let answer = http.get_bounded(&address, &headers, Bounds { most: MOST, https_only: true })?;
    let read = match answer.status {
        304 => Read::Same,
        200..300 => Read::New { etag: answer.header("etag").map(str::to_owned), body: answer.body },
        _ => return Err(HttpNotSuccessful::new(&address, &answer).into()),
    };
    Ok((address, read))
}

/// Reads `latest.json` into `state` at `now`: kept with its ETag when it is new and whole,
/// `failed` naming the address and why when it can't be read. What was read before stays (it
/// was true when read, `latest_at`); nothing is made up in its place.
fn check(state: &mut State, now: u64) {
    state.checked_at = Some(now);
    // The ETag only with the `latest.json` it is of: a `304` to a state that kept none would
    // leave it with none.
    let etag = state.latest().and(state.etag.as_deref());
    let read = hosts().and_then(|hosts| read_latest(etag).map(|read| (hosts, read)));
    state.failed = match read {
        Ok((_, (_, Read::Same))) => None,
        Ok((hosts, (address, Read::New { etag, body }))) => {
            let value = serde_json::from_slice::<Value>(&body).map_err(|error| error.to_string());
            match value.and_then(|value| latest_of(&value, &hosts).map(|_| value)) {
                Ok(value) => {
                    state.latest = Some(value);
                    state.latest_at = Some(now);
                    state.etag = etag;
                    None
                }
                Err(why) => Some(printable(&format!("{address}: {why}"))),
            }
        }
        Err(error) => Some(printable(&format!("{error:#}"))),
    };
}

/// The daemon's check (`blaze_cargo::release::watch_updates` runs it on a thread of its own):
/// when [`policy::next_check`] says it is due, reads `latest.json`, keeps what it found, and in
/// `auto` installs the newest release as the default. Says how long until it is due again.
/// What fails is said in the daemon's log and kept in the state; no command waits on it.
pub(crate) fn tick() -> Duration {
    match refresh(false) {
        Ok(next) => next,
        Err(error) => {
            eprintln!("blaze: the update check: {error:#}");
            CHECK_EVERY
        }
    }
}

/// [`tick`]'s work; `forced` (`blaze update`) checks whether due or not. A build of the tree
/// checks nothing: it is no release, and runs itself.
fn refresh(forced: bool) -> anyhow::Result<Duration> {
    if Version::parse(pin::OURS).is_none() && !forced {
        return Ok(CHECK_EVERY);
    }
    let home = home()?;
    let file = state_file(&home);
    let mut state = State::read(&file).unwrap_or_else(|error| {
        eprintln!("blaze: {error}");
        State::default()
    });
    let now = now();
    let wait = policy::next_check(state.checked_at, now);
    if !forced && !wait.is_zero() {
        return Ok(wait);
    }
    // What only the user can fix is said before each command (`say`), and asked again at each
    // check: the config and the default are read first, `offline` reads nothing more.
    let config = config(&home);
    let default = default(&home);
    state.problem = match (&config, &default) {
        (Err(error), _) | (_, Err(error)) => Some(printable(&error.to_string())),
        _ => None,
    };
    if let Ok(Config { mode: Mode::Offline, .. }) = config {
        state.write(&file)?;
        return Ok(CHECK_EVERY);
    }
    check(&mut state, now);
    state.write(&file)?;
    let (Ok(config), Ok(default)) = (config, default) else { return Ok(CHECK_EVERY) };
    if forced {
        return Ok(CHECK_EVERY);
    }
    let Some(latest) = state.latest() else { return Ok(CHECK_EVERY) };
    let runs = policy::runs_unpinned(pin::OURS, default.as_deref());
    let failed = state.install_failed.as_ref().map(|(version, _)| version.as_str());
    if let Some(version) = policy::auto_install(config.mode, config.channel, &latest, runs, failed)
    {
        match install(&home, version) {
            Ok(()) => versions::kept(&updated_file(&home), format!("{version}\n").as_bytes())
                .with_context(|| format!("writing {}", updated_file(&home).display()))?,
            Err(why) => {
                let why = printable(&format!("{why:#}"));
                eprintln!(
                    "blaze: `[update] mode = \"auto\"` could not install blaze {version}: {why}"
                );
                state.problem = Some(format!(
                    "`[update] mode = \"auto\"` could not install blaze {version} ({why}); `blaze update` tries again"
                ));
                state.install_failed = Some((version.to_owned(), why));
                state.write(&file)?;
            }
        }
    }
    Ok(CHECK_EVERY)
}

/// The release `default` under `home` names, `None` when there is none.
fn default(home: &Path) -> Result<Option<String>, UpdateError> {
    let file = default_file(home);
    let Some(text) = text(&file)? else { return Ok(None) };
    let version = text.trim();
    if !blaze_units::lock::is_version(version) {
        return Err(UpdateError::Default { file, got: version.to_owned() });
    }
    Ok(Some(version.to_owned()))
}

/// The release that runs outside a pinned workspace ([`policy::runs_unpinned`]): the default
/// `blaze update` installed, or this blaze.
///
/// - Under a blaze a launcher ran as the default ([`RUN_AS_DEFAULT`], which every process
///   under it inherits: its daemon, a build script running `$CARGO`, a test), that version,
///   and no file is read: a build script's sandbox may not let it read blaze's home.
/// - With no home there is no default.
/// - A `default` that can't be read (a sandbox's `EPERM`) is no default; one that names no
///   release is an error for a command the user ran, naming the file, and no default under a
///   blaze a launcher ran ([`launched`]), which must keep working.
pub(crate) fn runs_unpinned() -> Result<String, UpdateError> {
    let given = std::env::var(RUN_AS_DEFAULT).ok().filter(|it| blaze_units::lock::is_version(it));
    if let Some(given) = given {
        return Ok(policy::runs_unpinned(pin::OURS, Some(given.as_str())).to_owned());
    }
    let Ok(home) = home() else { return Ok(pin::OURS.to_owned()) };
    let default = match default(&home) {
        Ok(default) => default,
        Err(UpdateError::Unread { .. }) => None,
        Err(UpdateError::Default { .. }) if launched() => None,
        Err(error) => return Err(error),
    };
    Ok(policy::runs_unpinned(pin::OURS, default.as_deref()).to_owned())
}

/// The release `version` fetched and checked (`versions::fetch_for`) and made the default.
fn install(home: &Path, version: &str) -> anyhow::Result<()> {
    let _span =
        tracing::info_span!("update: installing a release as the default", version).entered();
    versions::fetch_for(version, "which `blaze update` installs")?;
    let file = default_file(home);
    versions::kept(&file, format!("{version}\n").as_bytes())
        .with_context(|| format!("writing {}", file.display()))
}

/// Whether this process was started by a launcher that said the lines already: as the default
/// ([`RUN_AS_DEFAULT`]), or as the release a workspace pins (`pin::PINNED_BY`).
pub(crate) fn launched() -> bool {
    std::env::var_os(RUN_AS_DEFAULT).is_some_and(|it| !it.is_empty())
        || std::env::var_os(pin::PINNED_BY).is_some_and(|it| !it.is_empty())
}

/// The lines before a command run by the release `runs` under `pin` ([`policy::decide`]), from
/// what the daemon's last check kept: never the network, and never a failed command. Under CI
/// or with stderr no terminal, only a yanked release is said.
pub(crate) fn say(runs: &str, pin: Pin<'_>, manifest: Option<&Path>) {
    let Ok(home) = home() else { return };
    let quiet =
        std::env::var_os("CI").is_some_and(|it| !it.is_empty()) || !std::io::stderr().is_terminal();
    let config = config(&home).unwrap_or_else(|error| {
        if !quiet {
            eprintln!("warning: {error}; blaze says only what is yanked until it is fixed");
        }
        Config { mode: Mode::Off, ..Config::DEFAULT }
    });
    let state = match State::read(&state_file(&home)) {
        Ok(state) => state,
        Err(error) => {
            if !quiet {
                eprintln!("warning: {error}");
            }
            return;
        }
    };
    if let Some(problem) = state.problem.as_deref().filter(|_| !quiet) {
        eprintln!("warning: {problem}");
    }
    let latest = state.latest();
    let updated_file = updated_file(&home);
    let updated = text(&updated_file)
        .ok()
        .flatten()
        .map(|it| it.trim().to_owned())
        .filter(|it| blaze_units::lock::is_version(it));
    let here = Here {
        runs,
        pin,
        mode: config.mode,
        channel: config.channel,
        latest: latest.as_ref(),
        quiet,
        updated: updated.as_deref(),
    };
    let workspace =
        manifest.map_or_else(|| "this workspace".to_owned(), |it| it.display().to_string());
    for said in policy::decide(&here) {
        let line = match said {
            Say::Yanked { version, reason, fix } => {
                let fix = match fix {
                    Fix::Pin(newest) => format!(
                        "{workspace} pins it: `blaze pin {newest}` moves it to blaze {newest}"
                    ),
                    Fix::Update(newest) => format!("`blaze update` installs blaze {newest}"),
                    Fix::None if matches!(pin, Pin::Channel) => {
                        format!(
                            "{workspace} pins a channel that still names it; it moves on with the channel's next release"
                        )
                    }
                    Fix::None => "no release to move to is out yet".to_owned(),
                };
                format!("warning: blaze {version} is YANKED: {reason}\nwarning: {fix}")
            }
            Say::Behind { runs, newest, url } => {
                format!("blaze {newest} is out (this is {runs}): blaze update · notes: {url}")
            }
            Say::PinBehind { pin, newest, days } => {
                let behind = match days {
                    0 => format!("behind {newest} (the same day)"),
                    1 => format!("1 day behind {newest}"),
                    days => format!("{days} days behind {newest}"),
                };
                format!("this workspace pins blaze {pin}, {behind}: blaze pin {newest}")
            }
            Say::Updated { version, url } => {
                _ = std::fs::remove_file(&updated_file);
                format!("blaze updated to {version}; notes: {url}")
            }
        };
        eprintln!("{line}");
    }
}

const USAGE: &str = "blaze update                     installs the newest release of the channel, which then runs \
                     outside pinned workspaces\n\
                     blaze update --check             reads latest.json now and says what is out; installs nothing\n\
                     blaze update --mode warn|auto|off|offline   what blaze does about newer releases ([update] mode)\n\
                     blaze update --channel stable|canary   which releases count ([update] channel)";

/// `blaze update`: [`USAGE`].
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<i32> {
    let (mut mode, mut channel, mut only_check) = (None, None, false);
    let mut args = args;
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (arg.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| args.next())
                .with_context(|| format!("`blaze update {name}` takes a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--check" => only_check = true,
            "--mode" => {
                let given = value("--mode")?;
                mode = Some(Mode::parse(&given).with_context(|| {
                    format!("`blaze update --mode {given}`: the modes are {}", Mode::NAMES)
                })?);
            }
            "--channel" => {
                let given = value("--channel")?;
                channel = Some(Channel::parse(&given).with_context(|| {
                    format!("`blaze update --channel {given}`: blaze has no channel {given}: the channels are stable and canary")
                })?);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            _ => bail!("`blaze update` takes no {arg}\n{USAGE}"),
        }
    }
    let home = home()?;
    if mode.is_some() || channel.is_some() {
        if let Some(mode) = mode {
            set(&home, "mode", mode.name())?;
        }
        if let Some(channel) = channel {
            set(&home, "channel", channel.name())?;
        }
        return Ok(0);
    }
    let config = config(&home)?;
    if config.mode == Mode::Offline {
        bail!(
            "{} sets `[update] mode = \"offline\"`: blaze makes no network calls for its updates, and \
             `blaze update` is one (`blaze update --mode warn` turns them on)",
            user_config::file(&home).display()
        );
    }
    refresh(true)?;
    let file = state_file(&home);
    let state = State::read(&file)?;
    if let Some(failed) = &state.failed {
        bail!("blaze's newest releases could not be read: {failed}");
    }
    let latest = state
        .latest()
        .with_context(|| format!("{} holds no latest.json after a check", file.display()))?;
    let channel = config.channel;
    let newest = latest.newest(channel);
    // A `default` that names no release is what `blaze update` is for: it is written again.
    let (runs, corrupt) = match runs_unpinned() {
        Ok(runs) => (runs, None),
        Err(error @ UpdateError::Default { .. }) => (pin::OURS.to_owned(), Some(error)),
        Err(error) => return Err(error.into()),
    };
    let pinned = pinned_here();
    if only_check {
        if let Some(corrupt) = &corrupt {
            println!("{corrupt}");
        }
        println!(
            "blaze {runs} runs outside pinned workspaces; the newest {} release is blaze {}",
            channel.name(),
            newest.version
        );
        if let Some(reason) = latest.yanked(&runs) {
            println!("blaze {runs} is YANKED: {reason}");
        }
        if let Some((manifest, version)) = &pinned {
            println!("{} pins blaze {version}", manifest.display());
        }
        print_notes(newest);
        return Ok(0);
    }
    if Version::parse(pin::OURS).is_none() {
        bail!(
            "this blaze is {}, a build of blaze's source tree, which runs itself outside pinned workspaces \
             whatever is installed: `blaze update` is for a release of blaze (install.sh), and \
             `blaze update --check` says what is out",
            pin::OURS
        );
    }
    // `latest.json` names no yanked release as a channel's newest (promote.py); one that does is
    // not installed.
    if let Some(reason) = latest.yanked(&newest.version) {
        bail!(
            "latest.json names blaze {} as the newest {} release, and yanks it ({reason}): it is not installed",
            newest.version,
            channel.name()
        );
    }
    let (Some(to), Some(from)) = (Version::parse(&newest.version), Version::parse(&runs)) else {
        bail!("blaze {runs} or blaze {} is not a release's version", newest.version);
    };
    let yanked = latest.yanked(&runs);
    if corrupt.is_none() && (to == from || (to < from && yanked.is_none())) {
        if to == from {
            println!("blaze {runs} is the newest {} release", channel.name());
        } else {
            println!(
                "blaze {runs} is newer than the newest {} release, blaze {}: nothing to install \
                 (`blaze update --channel canary` follows every release)",
                channel.name(),
                newest.version
            );
        }
    } else {
        install(&home, &newest.version)?;
        _ = std::fs::remove_file(updated_file(&home));
        if let Some(corrupt) = &corrupt {
            println!("{corrupt}; replaced with blaze {}", newest.version);
        }
        let how = match yanked {
            Some(reason) if to < from => {
                format!(", back from a yanked release ({reason}): a downgrade")
            }
            Some(reason) => format!(", off a yanked release ({reason})"),
            None => String::new(),
        };
        println!(
            "blaze {runs} → {} ({}{how}): it runs outside pinned workspaces from the next command",
            newest.version,
            channel.name()
        );
        print_notes(newest);
    }
    if let Some((manifest, version)) = pinned.filter(|(_, version)| *version != newest.version) {
        println!(
            "{} pins blaze {version}, which `blaze update` never moves: `blaze pin {}` does",
            manifest.display(),
            newest.version
        );
    }
    Ok(0)
}

/// The root manifest here and the version it pins, when it pins a version.
fn pinned_here() -> Option<(PathBuf, String)> {
    let cwd = std::env::current_dir().ok()?;
    let manifest = pin::root_manifest(&cwd).ok()??;
    let version = pin::pinned(&manifest).ok()??;
    blaze_units::lock::is_version(&version).then_some((manifest, version))
}

fn print_notes(newest: &Newest) {
    if !newest.notes.is_empty() {
        println!();
        for line in newest.notes.lines() {
            println!("    {line}");
        }
        println!();
    }
    println!("notes: {}", newest.url);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `update/state.json` round-trips: what is written reads back equal, and is written again
    /// to the same bytes; none there reads as no check yet.
    #[test]
    fn the_state_round_trips() {
        let dir = std::env::temp_dir().join(format!("blaze-update-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("update").join("state.json");
        let page = |version: &str| {
            format!("https://github.com/indexable-inc/blaze/releases/tag/v{version}")
        };
        let latest = serde_json::json!({
            "schema": 1,
            "stable": {"version": "0.20261104.0", "url": page("0.20261104.0"), "notes": "- a\n- b"},
            "canary": {"version": "0.20261104.3", "url": page("0.20261104.3"), "notes": ""},
            "yanked": {"0.20261101.0": "miscompiles drops"},
        });
        let state = State {
            checked_at: Some(1_762_250_000),
            etag: Some("\"3f2a\"".to_owned()),
            latest: Some(latest),
            latest_at: Some(1_762_249_700),
            failed: Some("https://ix.dev/blaze/latest.json: timed out".to_owned()),
            install_failed: Some(("0.20261104.0".to_owned(), "its B3SUMS says 00".to_owned())),
            problem: Some("`[update] mode` is \"sometimes\"".to_owned()),
        };
        state.write(&file).unwrap();
        let written = std::fs::read(&file).unwrap();
        let read = State::read(&file).unwrap();
        assert_eq!(read, state);
        read.write(&file).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), written);
        State::default().write(&file).unwrap();
        assert_eq!(State::read(&file).unwrap(), State::default());
        assert_eq!(State::read(&dir.join("none.json")).unwrap(), State::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
