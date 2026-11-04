//! What blaze says and does about its own updates, decided with no IO: given what the last
//! check of `latest.json` found, the user's `[update]` settings, the workspace's pin, whether
//! anyone reads stderr, and the time, what to print before a command and what the daemon
//! does. The drivers (`update.rs`, the daemon's thread in `blaze_cargo::release`) read the
//! files, the clock and the network and hand the facts here.

use std::time::Duration;

/// How often the daemon reads `latest.json` again: releases come every few minutes, and an
/// unchanged one costs a `304`.
pub(crate) const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);

/// What `[update] mode` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The default: a line before each command while a newer release is out.
    Warn,
    /// As `Warn`, and the daemon installs the newest release as the default outside pinned
    /// workspaces, saying so once after.
    Auto,
    /// No line about newer releases; a yanked release is still said, so the daemon still reads
    /// `latest.json`.
    Off,
    /// No network at all: the daemon reads nothing and `blaze update` is refused, so no yanked
    /// release can be said either (what an earlier check kept still is).
    Offline,
}

impl Mode {
    pub(crate) const NAMES: &str = "\"warn\", \"auto\", \"off\" or \"offline\"";

    pub(crate) fn parse(text: &str) -> Option<Mode> {
        match text {
            "warn" => Some(Mode::Warn),
            "auto" => Some(Mode::Auto),
            "off" => Some(Mode::Off),
            "offline" => Some(Mode::Offline),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Mode::Warn => "warn",
            Mode::Auto => "auto",
            Mode::Off => "off",
            Mode::Offline => "offline",
        }
    }
}

/// Which channel's newest release counts (docs/release.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Channel {
    Stable,
    Canary,
}

impl Channel {
    pub(crate) fn parse(text: &str) -> Option<Channel> {
        match text {
            "stable" => Some(Channel::Stable),
            "canary" => Some(Channel::Canary),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Canary => "canary",
        }
    }
}

/// A release's version, `0.YYYYMMDD.N`, ordered by date and then number. `None` for anything
/// else, a build of the tree (`0.0.0-dev`) among them, which is no release and is never behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    date: u32,
    n: u32,
}

impl Version {
    pub(crate) fn parse(text: &str) -> Option<Version> {
        if !blaze_units::lock::is_version(text) {
            return None;
        }
        let mut parts = text.split('.').skip(1);
        let date = parts.next()?.parse().ok()?;
        let n = parts.next()?.parse().ok()?;
        Some(Version { date, n })
    }

    /// Days from `self`'s date to `later`'s, by the calendar (0 for the same day, never
    /// negative).
    pub(crate) fn days_to(self, later: Version) -> u32 {
        let day = |date: u32| days_from_civil(date / 10_000, (date / 100) % 100, date % 100);
        u32::try_from(day(later.date) - day(self.date)).unwrap_or(0)
    }
}

/// Days since 1970-01-01 of the proleptic Gregorian date `y-m-d` (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: u32, m: u32, d: u32) -> i64 {
    let (y, m, d) = (i64::from(y), i64::from(m), i64::from(d));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The newest release of a channel, as `latest.json` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Newest {
    pub(crate) version: String,
    /// Its release page, where its notes are.
    pub(crate) url: String,
    /// The first lines of its notes.
    pub(crate) notes: String,
}

/// What `latest.json` says (docs/release.md, "latest.json").
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Latest {
    pub(crate) stable: Newest,
    pub(crate) canary: Newest,
    /// Each yanked release and why, in version order.
    pub(crate) yanked: Vec<(String, String)>,
}

impl Latest {
    pub(crate) fn newest(&self, channel: Channel) -> &Newest {
        match channel {
            Channel::Stable => &self.stable,
            Channel::Canary => &self.canary,
        }
    }

    pub(crate) fn yanked(&self, version: &str) -> Option<&str> {
        self.yanked.iter().find(|(it, _)| it == version).map(|(_, why)| why.as_str())
    }
}

/// What a workspace pins, as the launcher found it for this command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pin<'a> {
    /// No workspace, or one that pins nothing: the default runs.
    None,
    /// A version, which only `blaze pin` moves.
    Version(&'a str),
    /// A channel, which moves by itself.
    Channel,
    /// `--ignore-pin`: the blaze given runs, whatever is pinned.
    Ignored,
}

/// Everything the lines before a command depend on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Here<'a> {
    /// The version that runs this command (`0.0.0-dev` for a build of the tree).
    pub(crate) runs: &'a str,
    pub(crate) pin: Pin<'a>,
    pub(crate) mode: Mode,
    pub(crate) channel: Channel,
    /// What the last check that read `latest.json` found; `None` before any did.
    pub(crate) latest: Option<&'a Latest>,
    /// `CI` is set, or stderr is no terminal: only a yanked release is said.
    pub(crate) quiet: bool,
    /// A release the daemon installed as the default (`auto`) and nobody was told of yet.
    pub(crate) updated: Option<&'a str>,
}

/// One line before a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Say {
    /// The release that runs is yanked: said on every command, CI too. `fix` is what moves off it.
    Yanked { version: String, reason: String, fix: Fix },
    /// A newer release is out, and nothing pins this one.
    Behind { runs: String, newest: String, url: String },
    /// The workspace pins a version older than the newest.
    PinBehind { pin: String, newest: String, days: u32 },
    /// The daemon installed `version` as the default (`auto`).
    Updated { version: String, url: String },
}

/// What moves a command off a yanked release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fix {
    /// `blaze pin <version>` in the workspace.
    Pin(String),
    /// `blaze update`, which installs `version`.
    Update(String),
    /// Nothing blaze runs: the channel the workspace pins still names it, or no release is newer.
    None,
}

/// The lines before a command, in the order they are printed. `updated` is taken (its file
/// removed) by the driver only when a `Say::Updated` comes back.
pub(crate) fn decide(here: &Here<'_>) -> Vec<Say> {
    let mut said = Vec::new(); // alloc: at most three lines, once per command
    let Some(latest) = here.latest else { return said };
    let newest = latest.newest(here.channel);
    let newer = |than: &str| match (Version::parse(&newest.version), Version::parse(than)) {
        (Some(newest), Some(than)) => newest > than,
        _ => false,
    };
    if let Some(reason) = latest.yanked(here.runs) {
        let fix = match here.pin {
            Pin::Version(_) if newest.version != here.runs => Fix::Pin(newest.version.clone()),
            Pin::None | Pin::Ignored if newest.version != here.runs => {
                Fix::Update(newest.version.clone())
            }
            _ => Fix::None,
        };
        said.push(Say::Yanked { version: here.runs.to_owned(), reason: reason.to_owned(), fix });
    }
    if here.quiet || matches!(here.mode, Mode::Off | Mode::Offline) {
        return said;
    }
    if let Some(version) = here.updated {
        let url =
            if version == newest.version { newest.url.clone() } else { release_page(version) };
        said.push(Say::Updated { version: version.to_owned(), url });
    }
    match here.pin {
        Pin::Version(pin) if newer(pin) => {
            let days = match (Version::parse(pin), Version::parse(&newest.version)) {
                (Some(pin), Some(newest)) => pin.days_to(newest),
                _ => 0,
            };
            said.push(Say::PinBehind { pin: pin.to_owned(), newest: newest.version.clone(), days });
        }
        Pin::None if newer(here.runs) => said.push(Say::Behind {
            runs: here.runs.to_owned(),
            newest: newest.version.clone(),
            url: newest.url.clone(),
        }),
        _ => {}
    }
    said
}

/// The page of the release `version`, where its notes are.
pub(crate) fn release_page(version: &str) -> String {
    format!("https://github.com/indexable-inc/blaze/releases/tag/v{version}")
}

/// Which blaze runs outside a pinned workspace: the default `blaze update` (or `auto`)
/// installed, when one is set and this blaze is a release; this one otherwise. A build of
/// the tree always runs itself, and install.sh removes the default, so the blaze installed
/// last runs.
pub(crate) fn runs_unpinned<'a>(ours: &'a str, default: Option<&'a str>) -> &'a str {
    match default {
        Some(default) if Version::parse(ours).is_some() && Version::parse(default).is_some() => {
            default
        }
        _ => ours,
    }
}

/// How long until the daemon reads `latest.json` again, the last read (successful or not)
/// having been at `checked_at` (seconds since the epoch); zero when due now. A check dated
/// after `now` (a clock set back) is due now.
pub(crate) fn next_check(checked_at: Option<u64>, now: u64) -> Duration {
    match checked_at {
        Some(at) if at <= now => CHECK_EVERY.saturating_sub(Duration::from_secs(now - at)),
        _ => Duration::ZERO,
    }
}

/// The release the daemon installs as the default after a check: in `auto`, the channel's
/// newest when it is newer than the default that runs now (`runs`, [`runs_unpinned`]),
/// unless installing that same release failed before (`failed`), which `blaze update` tries
/// again by hand. Never a pin: pins are the workspace's.
pub(crate) fn auto_install<'a>(
    mode: Mode,
    channel: Channel,
    latest: &'a Latest,
    runs: &str,
    failed: Option<&str>,
) -> Option<&'a str> {
    let newest = latest.newest(channel);
    let newer = match (Version::parse(&newest.version), Version::parse(runs)) {
        (Some(newest), Some(runs)) => newest > runs,
        _ => false,
    };
    (mode == Mode::Auto
        && newer
        && latest.yanked(&newest.version).is_none()
        && failed != Some(newest.version.as_str()))
    .then_some(newest.version.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn newest(version: &str) -> Newest {
        Newest { version: version.to_owned(), url: release_page(version), notes: String::new() }
    }

    /// One table over [`decide`]: each row is the facts of a command and the lines it gets.
    #[test]
    fn what_each_command_is_told() {
        let latest = Latest {
            stable: newest("0.20261104.0"),
            canary: newest("0.20261104.3"),
            yanked: vec![("0.20261101.0".to_owned(), "miscompiles drops".to_owned())],
        };
        let base = Here {
            runs: "0.20261104.0",
            pin: Pin::None,
            mode: Mode::Warn,
            channel: Channel::Stable,
            latest: Some(&latest),
            quiet: false,
            updated: None,
        };
        let behind = |runs: &str, newest: &str| Say::Behind {
            runs: runs.to_owned(),
            newest: newest.to_owned(),
            url: release_page(newest),
        };
        let yanked = |version: &str, fix: Fix| Say::Yanked {
            version: version.to_owned(),
            reason: "miscompiles drops".to_owned(),
            fix,
        };
        let pin_behind = |pin: &str, days| Say::PinBehind {
            pin: pin.to_owned(),
            newest: "0.20261104.0".to_owned(),
            days,
        };
        let update = |it: &str| Fix::Update(it.to_owned());
        #[rustfmt::skip]
        let rows: Vec<(&str, Here<'_>, Vec<Say>)> = vec![
            ("nothing checked yet", Here { latest: None, runs: "0.20261101.0", ..base }, vec![]),
            ("newest: nothing", base, vec![]),
            ("one release behind: said", Here { runs: "0.20261103.9", ..base }, vec![behind("0.20261103.9", "0.20261104.0")]),
            ("canary channel: its newest", Here { channel: Channel::Canary, ..base }, vec![behind("0.20261104.0", "0.20261104.3")]),
            ("newer than the channel: nothing", Here { runs: "0.20261104.3", ..base }, vec![]),
            ("off: nothing", Here { runs: "0.20261103.9", mode: Mode::Off, ..base }, vec![]),
            ("offline: nothing", Here { runs: "0.20261103.9", mode: Mode::Offline, ..base }, vec![]),
            ("auto: still said until installed", Here { runs: "0.20261103.9", mode: Mode::Auto, ..base }, vec![behind("0.20261103.9", "0.20261104.0")]),
            ("quiet (CI, no tty): nothing", Here { runs: "0.20261103.9", quiet: true, ..base }, vec![]),
            ("a build of the tree: nothing", Here { runs: "0.0.0-dev", ..base }, vec![]),
            ("pinned to the newest: nothing", Here { pin: Pin::Version("0.20261104.0"), ..base }, vec![]),
            ("pinned behind, by days", Here { runs: "0.20261027.2", pin: Pin::Version("0.20261027.2"), ..base }, vec![pin_behind("0.20261027.2", 8)]),
            ("pinned behind, by a day", Here { runs: "0.20261103.0", pin: Pin::Version("0.20261103.0"), ..base }, vec![pin_behind("0.20261103.0", 1)]),
            ("pinned behind, off", Here { runs: "0.20261103.0", pin: Pin::Version("0.20261103.0"), mode: Mode::Off, ..base }, vec![]),
            ("pinned behind, quiet", Here { runs: "0.20261103.0", pin: Pin::Version("0.20261103.0"), quiet: true, ..base }, vec![]),
            ("pinned to a channel: never behind", Here { runs: "0.20261103.0", pin: Pin::Channel, ..base }, vec![]),
            ("--ignore-pin: never behind", Here { runs: "0.20261103.0", pin: Pin::Ignored, ..base }, vec![]),
            ("yanked, unpinned", Here { runs: "0.20261101.0", ..base }, vec![yanked("0.20261101.0", update("0.20261104.0")), behind("0.20261101.0", "0.20261104.0")]),
            ("yanked, quiet: still said", Here { runs: "0.20261101.0", quiet: true, ..base }, vec![yanked("0.20261101.0", update("0.20261104.0"))]),
            ("yanked, off: still said", Here { runs: "0.20261101.0", mode: Mode::Off, ..base }, vec![yanked("0.20261101.0", update("0.20261104.0"))]),
            ("yanked, offline: what was kept is said", Here { runs: "0.20261101.0", mode: Mode::Offline, ..base }, vec![yanked("0.20261101.0", update("0.20261104.0"))]),
            ("yanked, pinned", Here { runs: "0.20261101.0", pin: Pin::Version("0.20261101.0"), quiet: true, ..base }, vec![yanked("0.20261101.0", Fix::Pin("0.20261104.0".to_owned()))]),
            ("yanked, pinned to a channel", Here { runs: "0.20261101.0", pin: Pin::Channel, quiet: true, ..base }, vec![yanked("0.20261101.0", Fix::None)]),
            ("yanked, --ignore-pin", Here { runs: "0.20261101.0", pin: Pin::Ignored, quiet: true, ..base }, vec![yanked("0.20261101.0", update("0.20261104.0"))]),
            ("updated by auto: said once", Here { updated: Some("0.20261104.0"), ..base }, vec![Say::Updated { version: "0.20261104.0".to_owned(), url: release_page("0.20261104.0") }]),
            ("updated, quiet: kept for later", Here { updated: Some("0.20261104.0"), quiet: true, ..base }, vec![]),
        ];
        for (row, here, want) in rows {
            assert_eq!(decide(&here), want, "{row}");
        }
    }
}
