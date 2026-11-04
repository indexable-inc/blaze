//! `[net]` of blaze's user configuration, `config.toml` in blaze's home (`$BLAZE_HOME`, else
//! `~/.blaze`, where `versions/` is):
//!
//! ```toml
//! [net]
//! http3 = "auto"            # "off" (the default), "auto" or "require"
//!
//! [net.http3-hosts]
//! "static.crates.io" = "off" # one host's own setting, over `http3`
//! ```
//!
//! Another key under `[net]` or `[net.http3-hosts]`, or a value none of those it takes, is an
//! error naming the file and the key; other tables are not this module's (`[update]` is the
//! launcher's, crates/blaze-engine/src/update.rs on branch launcher-update, which reads the
//! same file: when both are on main, the file's path and its reading move to one place).

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use super::choice::{Http3, Proxies};

/// What the client is told about the network: the `net.http3` setting for every host and for
/// some, the proxies of the environment, and where the adverts of HTTP/3 are kept.
#[derive(Clone, Debug, Default)]
pub struct Net {
    pub http3: Http3,
    /// `[net.http3-hosts]`: hosts, lower case, with their own setting.
    pub hosts: std::collections::BTreeMap<String, Http3>,
    pub proxies: Proxies,
    /// The configuration file, as messages name it ("set `net.http3 = "off"` in ...").
    pub config: String,
    /// Where the adverts of HTTP/3 (DNS HTTPS records, `Alt-Svc`) are kept across runs; none
    /// keeps them in memory only (a test, a benchmark's cold run).
    pub adverts: Option<PathBuf>,
}

/// The configuration file's name, as messages say it when blaze's home can't be found.
const CONFIG_SAID: &str = "~/.blaze/config.toml";

impl Net {
    /// HTTP/3 off, no proxy, no adverts kept: what a test's client is made with.
    pub fn off() -> Net {
        Net { config: CONFIG_SAID.to_owned(), ..Net::default() }
    }

    /// `[net]` of the user's `config.toml` (none: every setting's default), the process's
    /// proxies, and the adverts kept in the daemon's state (`<root>/v2/net/h3-adverts.json`).
    pub fn from_user_config() -> anyhow::Result<Net> {
        let file = user_config_file();
        let mut net = Net {
            config: file.as_ref().map_or_else(|| CONFIG_SAID.to_owned(), |it| it.display().to_string()),
            proxies: Proxies::from_env(|name| std::env::var(name).ok()),
            adverts: adverts_file()?,
            ..Net::default()
        };
        if let Some(file) = file {
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => Some(text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).with_context(|| format!("reading blaze's configuration {}", file.display())),
            };
            if let Some(text) = text {
                (net.http3, net.hosts) = read(&file, &text)?;
            }
        }
        Ok(net)
    }

    /// The setting for `host`: its own under `[net.http3-hosts]`, else `net.http3`.
    pub fn setting_for(&self, host: &str) -> Http3 {
        self.hosts.get(&host.trim_end_matches('.').to_ascii_lowercase()).copied().unwrap_or(self.http3)
    }
}

/// `config.toml` in blaze's home: `$BLAZE_HOME`, else `~/.blaze` (crates/blaze-engine/src/pin.rs
/// finds `versions/` there).
fn user_config_file() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    let home = std::env::var_os("HOME").filter(|it| !it.is_empty())?;
    Some(PathBuf::from(home).join(".blaze").join("config.toml"))
}

/// Where the daemon keeps the adverts of HTTP/3: `net/h3-adverts.json` in its state.
fn adverts_file() -> anyhow::Result<Option<PathBuf>> {
    let root = blaze_manifest::home::process_root().map_err(anyhow::Error::msg)?;
    Ok(root.map(|root| blaze_manifest::home::state(&root).join("net").join("h3-adverts.json")))
}

/// `net.http3` and `[net.http3-hosts]` of `text`, the contents of `file`.
fn read(file: &Path, text: &str) -> anyhow::Result<(Http3, std::collections::BTreeMap<String, Http3>)> {
    let table: toml::Table = toml::from_str(text).with_context(|| format!("{} is not TOML", file.display()))?;
    let mut http3 = Http3::default();
    let mut hosts = std::collections::BTreeMap::new();
    let Some(net) = table.get("net") else { return Ok((http3, hosts)) };
    let bad = |key: &str, wants: &str, got: &toml::Value| {
        anyhow::anyhow!("{}: `{key}` is {wants}, not {got}", file.display())
    };
    let net = net.as_table().ok_or_else(|| bad("net", "a table", net))?;
    let setting = |key: &str, value: &toml::Value| value.as_str().and_then(Http3::parse).ok_or_else(|| bad(key, Http3::NAMES, value));
    for (key, value) in net {
        match key.as_str() {
            "http3" => http3 = setting("net.http3", value)?,
            "http3-hosts" => {
                let table = value.as_table().ok_or_else(|| bad("net.http3-hosts", "a table of host = setting", value))?;
                for (host, value) in table {
                    hosts.insert(host.trim_end_matches('.').to_ascii_lowercase(), setting(&format!("net.http3-hosts.\"{host}\""), value)?);
                }
            }
            _ => return Err(bad(&format!("net.{key}"), "no setting of blaze's: `[net]` has `http3` and `http3-hosts`", value)),
        }
    }
    Ok((http3, hosts))
}
