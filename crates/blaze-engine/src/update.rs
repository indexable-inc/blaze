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
//! - `config.toml`: `[update] mode` (`"warn"`, the default; `"auto"`; `"off"`) and `channel`
//!   (`"stable"`, the default; `"canary"`). `blaze update --mode/--channel` writes it.
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
    /// `config.toml` that is not TOML.
    NotToml { file: PathBuf, error: toml::de::Error },
    /// A setting that is none of the values it takes.
    Setting { file: PathBuf, key: String, wants: &'static str, got: String },
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
            UpdateError::NotToml { file, error } => {
                write!(f, "{} is not TOML, so blaze's update settings can't be told: {error}", file.display())
            }
            UpdateError::Setting { file, key, wants, got } => {
                write!(f, "{} sets `{key}` to {got}, which is not {wants}", file.display())
            }
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

/// blaze's home: where `versions/` is.
fn home() -> Result<PathBuf, UpdateError> {
    pin::versions().and_then(|it| it.parent().map(Path::to_owned)).ok_or(UpdateError::NoHome)
}

fn config_file(home: &Path) -> PathBuf {
    home.join("config.toml")
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
    let file = config_file(home);
    let Some(text) = text(&file)? else { return Ok(Config::DEFAULT) };
    let table = toml::from_str::<toml::Table>(&text).map_err(|error| UpdateError::NotToml { file: file.clone(), error })?;
    let mut config = Config::DEFAULT;
    let Some(update) = table.get("update") else { return Ok(config) };
    let bad = |key: &str, wants, got: &toml::Value| UpdateError::Setting {
        file: file.clone(),
        key: key.to_owned(),
        wants,
        got: got.to_string(),
    };
    let update = update.as_table().ok_or_else(|| bad("update", "a table", update))?;
    for (key, value) in update {
        match key.as_str() {
            "mode" => config.mode = value.as_str().and_then(Mode::parse).ok_or_else(|| bad("update.mode", Mode::NAMES, value))?,
            "channel" => {
                config.channel = value
                    .as_str()
                    .and_then(Channel::parse)
                    .ok_or_else(|| bad("update.channel", "\"stable\" or \"canary\"", value))?
            }
            _ => return Err(bad(&format!("update.{key}"), "a setting of blaze's: `[update]` has `mode` and `channel`", value)),
        }
    }
    Ok(config)
}

/// `config.toml` under `home` with `[update] key = value`, the rest of it as it was.
fn set(home: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    let file = config_file(home);
    let shown = file.display();
    let mut doc: toml_edit::DocumentMut =
        text(&file)?.unwrap_or_default().parse().with_context(|| format!("reading {shown}"))?;
    let update = doc.entry("update").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let update = update.as_table_mut().with_context(|| format!("`update` in {shown} is not a table"))?;
    update.insert(key, toml_edit::value(value));
    versions::kept(&file, doc.to_string().as_bytes()).with_context(|| format!("writing {shown}"))?;
    println!("{shown}: [update] {key} = \"{value}\"");
    Ok(())
}

/// The last check, as `update/state.json` keeps it.
#[derive(Default, Debug)]
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
            let field = |key: &str| it.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            (field("version"), field("why"))
        });
        Ok(State {
            checked_at: value.get("checked_at").and_then(Value::as_u64),
            etag: string("etag"),
            latest: value.get("latest").filter(|it| !it.is_null()).cloned(),
            latest_at: value.get("latest_at").and_then(Value::as_u64),
            failed: string("failed"),
            install_failed,
        })
    }

    fn write(&self, file: &Path) -> anyhow::Result<()> {
        let install_failed = self.install_failed.as_ref().map(|(version, why)| serde_json::json!({"version": version, "why": why}));
        let value = serde_json::json!({
            "schema": 1,
            "checked_at": self.checked_at,
            "etag": self.etag,
            "latest": self.latest,
            "latest_at": self.latest_at,
            "failed": self.failed,
            "install_failed": install_failed,
        });
        let mut bytes = serde_json::to_vec_pretty(&value)?;
        bytes.push(b'\n');
        versions::kept(file, &bytes).with_context(|| format!("writing {}", file.display()))
    }

    /// What the kept `latest.json` says.
    fn latest(&self) -> Option<Latest> {
        self.latest.as_ref().and_then(|it| latest_of(it).ok())
    }
}

/// `latest.json` (docs/release.md, "latest.json") read: `schema` 1, `stable` and `canary`
/// each with `version`, `url` and `notes`, and `yanked`, a version to its reason. The error
/// names the field.
fn latest_of(value: &Value) -> Result<Latest, String> {
    if value.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err("it has no `schema` 1".to_owned());
    }
    let newest = |channel: &str| -> Result<Newest, String> {
        let it = value.get(channel).ok_or_else(|| format!("it has no `{channel}`"))?;
        let field = |key: &str| {
            it.get(key).and_then(Value::as_str).ok_or_else(|| format!("its `{channel}.{key}` is no string"))
        };
        let version = field("version")?;
        if !pin::is_version(version) {
            return Err(format!("its `{channel}.version` is {version:?}, not a release's version"));
        }
        let url = field("url")?;
        if !url.starts_with("https://") {
            return Err(format!("its `{channel}.url` is {url:?}, not an https:// address"));
        }
        Ok(Newest { version: version.to_owned(), url: url.to_owned(), notes: trimmed(field("notes")?) })
    };
    let mut yanked = Vec::new(); // alloc: the yanked releases, once a read
    if let Some(listed) = value.get("yanked") {
        let listed = listed.as_object().ok_or("its `yanked` is no object")?;
        for (version, reason) in listed {
            if !pin::is_version(version) {
                return Err(format!("its `yanked` names {version:?}, not a release's version"));
            }
            let reason = reason.as_str().ok_or_else(|| format!("its `yanked.{version}` is no string"))?;
            yanked.push((version.clone(), reason.to_owned()));
        }
    }
    yanked.sort_by_key(|(version, _)| Version::parse(version));
    Ok(Latest { stable: newest("stable")?, canary: newest("canary")?, yanked })
}

/// The first [`NOTES_LINES`] lines of `notes`, at most [`NOTES_MOST`] characters.
fn trimmed(notes: &str) -> String {
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
    New { etag: Option<String>, body: Vec<u8> },
}

/// Reads `latest.json`, sending `etag` as `If-None-Match`. From [`LATEST`], or `latest.json`
/// at the top of what `BLAZE_RELEASES` names. One try, bounded by [`PATIENCE`]: the next check
/// is the retry.
fn read_latest(etag: Option<&str>) -> anyhow::Result<(String, Read)> {
    let address = match versions::source()? {
        versions::Source::Dir(dir) => {
            let file = dir.join("latest.json");
            let body = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            return Ok((file.display().to_string(), Read::New { etag: None, body }));
        }
        versions::Source::GitHub => LATEST.to_owned(),
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
    let read = read_latest(state.etag.as_deref());
    state.failed = match read {
        Ok((_, Read::Same)) => None,
        Ok((address, Read::New { etag, body })) => {
            let value = serde_json::from_slice::<Value>(&body).map_err(|error| error.to_string());
            match value.and_then(|value| latest_of(&value).map(|_| value)) {
                Ok(value) => {
                    state.latest = Some(value);
                    state.latest_at = Some(now);
                    state.etag = etag;
                    None
                }
                Err(why) => Some(format!("{address}: {why}")),
            }
        }
        Err(error) => Some(format!("{error:#}")),
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
    check(&mut state, now);
    state.write(&file)?;
    if forced {
        return Ok(CHECK_EVERY);
    }
    let config = config(&home)?;
    let Some(latest) = state.latest() else { return Ok(CHECK_EVERY) };
    let default = default(&home)?;
    let runs = policy::runs_unpinned(pin::OURS, default.as_deref());
    let failed = state.install_failed.as_ref().map(|(version, _)| version.as_str());
    if let Some(version) = policy::auto_install(config.mode, config.channel, &latest, runs, failed) {
        match install(&home, version) {
            Ok(()) => versions::kept(&updated_file(&home), format!("{version}\n").as_bytes())
                .with_context(|| format!("writing {}", updated_file(&home).display()))?,
            Err(why) => {
                eprintln!("blaze: `[update] mode = \"auto\"` could not install blaze {version}: {why:#}");
                state.install_failed = Some((version.to_owned(), format!("{why:#}")));
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
    if !pin::is_version(version) {
        return Err(UpdateError::Default { file, got: version.to_owned() });
    }
    Ok(Some(version.to_owned()))
}

/// The release that runs outside a pinned workspace ([`policy::runs_unpinned`]): the default
/// `blaze update` installed, or this blaze.
pub(crate) fn runs_unpinned() -> Result<String, UpdateError> {
    let default = default(&home()?)?;
    Ok(policy::runs_unpinned(pin::OURS, default.as_deref()).to_owned())
}

/// The release `version` fetched and checked (`versions::fetch_for`) and made the default.
fn install(home: &Path, version: &str) -> anyhow::Result<()> {
    versions::fetch_for(version, "which `blaze update` installs")?;
    let file = default_file(home);
    versions::kept(&file, format!("{version}\n").as_bytes()).with_context(|| format!("writing {}", file.display()))
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
    let quiet = std::env::var_os("CI").is_some_and(|it| !it.is_empty()) || !std::io::stderr().is_terminal();
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
    let latest = state.latest();
    let updated_file = updated_file(&home);
    let updated = text(&updated_file).ok().flatten().map(|it| it.trim().to_owned()).filter(|it| pin::is_version(it));
    let here = Here {
        runs,
        pin,
        mode: config.mode,
        channel: config.channel,
        latest: latest.as_ref(),
        quiet,
        updated: updated.as_deref(),
    };
    let workspace = manifest.map_or_else(|| "this workspace".to_owned(), |it| it.display().to_string());
    for said in policy::decide(&here) {
        let line = match said {
            Say::Yanked { version, reason, fix } => {
                let fix = match fix {
                    Fix::Pin(newest) => format!("{workspace} pins it: `blaze pin {newest}` moves it to blaze {newest}"),
                    Fix::Update(newest) => format!("`blaze update` installs blaze {newest}"),
                    Fix::None if matches!(pin, Pin::Channel) => {
                        format!("{workspace} pins a channel that still names it; it moves on with the channel's next release")
                    }
                    Fix::None => "no release to move to is out yet".to_owned(),
                };
                format!("warning: blaze {version} is YANKED: {reason}\nwarning: {fix}")
            }
            Say::Behind { runs, newest, url } => format!("blaze {newest} is out (this is {runs}): blaze update · notes: {url}"),
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
                     blaze update --mode warn|auto|off   what blaze does about newer releases ([update] mode)\n\
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
            inline.clone().or_else(|| args.next()).with_context(|| format!("`blaze update {name}` takes a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--check" => only_check = true,
            "--mode" => {
                let given = value("--mode")?;
                mode = Some(Mode::parse(&given).with_context(|| format!("`blaze update --mode {given}`: the modes are {}", Mode::NAMES))?);
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
    refresh(true)?;
    let file = state_file(&home);
    let state = State::read(&file)?;
    if let Some(failed) = &state.failed {
        bail!("blaze's newest releases could not be read: {failed}");
    }
    let latest = state.latest().with_context(|| format!("{} holds no latest.json after a check", file.display()))?;
    let channel = config.channel;
    let newest = latest.newest(channel);
    let runs = runs_unpinned()?;
    let pinned = pinned_here();
    if only_check {
        println!("blaze {runs} runs outside pinned workspaces; the newest {} release is blaze {}", channel.name(), newest.version);
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
    let newer = match (Version::parse(&newest.version), Version::parse(&runs)) {
        (Some(newest), Some(runs)) => newest > runs,
        _ => false,
    };
    if !newer && (newest.version == runs || latest.yanked(&runs).is_none()) {
        println!("blaze {runs} is the newest {} release", channel.name());
    } else {
        install(&home, &newest.version)?;
        _ = std::fs::remove_file(updated_file(&home));
        println!(
            "blaze {runs} → {} ({}): it runs outside pinned workspaces from the next command",
            newest.version,
            channel.name()
        );
        print_notes(newest);
    }
    if let Some((manifest, version)) = pinned.filter(|(_, version)| *version != newest.version) {
        println!("{} pins blaze {version}, which `blaze update` never moves: `blaze pin {}` does", manifest.display(), newest.version);
    }
    Ok(0)
}

/// The root manifest here and the version it pins, when it pins a version.
fn pinned_here() -> Option<(PathBuf, String)> {
    let cwd = std::env::current_dir().ok()?;
    let manifest = pin::root_manifest(&cwd).ok()??;
    let version = pin::pinned(&manifest).ok()??;
    pin::is_version(&version).then_some((manifest, version))
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
