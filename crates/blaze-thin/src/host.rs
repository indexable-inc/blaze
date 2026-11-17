//! Where a command's build is served: `[serve] host` of blaze's `config.toml` (the file
//! `[terminal]` and `[update]` are in, found by `blaze_manifest::user_config`), read the same
//! way by the thin `blaze` and the engine, so the two can't differ.
//!
//! - `"daemon"` (the default while the measurements are not in): the request goes to `blaze
//!   serve` on the home's socket, started when none answers, as it always has.
//! - `"process"`: the daemon's own code is driven in the process the user started, which is its
//!   only client (`blaze_cargo::serve::once`: the loop, the plane and the pool made here, one
//!   in-process connection with the socket's frames, no socket, no fork, no second process); the
//!   answer is printed, what outlives the process is made durable, and the process exits leaving
//!   nothing behind. The thin `blaze` still answers what its certificate check certifies first
//!   (`solo`); a miss is the engine's, run by `exec` in the same process.
//!
//! The architecture audit of 2026-11-13 (verdict (b), step 2) names `"process"` the end state:
//! once the measurements of `scripts/src/bin/in_process.rs` are in, `"daemon"` goes, and with it
//! this setting. There is no second build path to rot meanwhile: both hosts run the same daemon
//! code; only the transport and the life of the process differ.
//!
//! A sans-IO core: the file's text in, the setting out.

/// The section of `config.toml` the setting is in.
pub const SECTION: &str = "serve";

/// The key, in [`SECTION`].
pub const KEY: &str = "host";

/// Where a command's build is served ([`self`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Host {
    /// `blaze serve`, on the home's socket.
    #[default]
    Daemon,
    /// The daemon's code in this command's own process, which exits at the end.
    Process,
}

impl Host {
    /// The value `config.toml` spells it with.
    pub const fn word(self) -> &'static str {
        match self {
            Host::Daemon => "daemon",
            Host::Process => "process",
        }
    }

    /// `[serve] host` of `text`, a `config.toml`: [`Host::Daemon`] where the file does not set it.
    /// A value it does not take, a `serve` that is not a table, or a key of `[serve]` it does not
    /// know is an error naming it and what it takes, never a quiet default.
    pub fn from_toml(text: &str) -> Result<Host, String> {
        let table: toml::Table =
            toml::from_str(text).map_err(|error| format!("config.toml is not TOML: {error}"))?;
        Host::from_table(&table)
    }

    /// [`Host::from_toml`] of a table already read.
    pub fn from_table(table: &toml::Table) -> Result<Host, String> {
        let Some(serve) = table.get(SECTION) else { return Ok(Host::Daemon) };
        let Some(serve) = serve.as_table() else {
            return Err(format!("config.toml's `{SECTION}` is not a table ([{SECTION}])")); // alloc: an error, once
        };
        let mut host = Host::Daemon;
        for (key, value) in serve {
            match key.as_str() {
                KEY => {
                    host = match value.as_str() {
                        Some("daemon") => Host::Daemon,
                        Some("process") => Host::Process,
                        // alloc: an error, once
                        _ => {
                            return Err(format!(
                                "config.toml's `{SECTION}.{KEY}` takes \"daemon\" or \"process\", not `{value}`"
                            ));
                        }
                    }
                }
                // alloc: an error, once
                _ => return Err(format!("config.toml's `{SECTION}` has no `{key}`: it takes {KEY}")),
            }
        }
        Ok(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: the file says the host, unset is the daemon, and anything else is refused
    /// naming the key. Controls: a misspelt value and an unknown key are refused, not read as the
    /// default.
    #[test]
    fn the_host_is_read_from_serve_and_anything_else_is_refused_by_name() {
        assert_eq!(Host::from_toml(""), Ok(Host::Daemon));
        assert_eq!(Host::from_toml("[terminal]\ncolor = \"never\"\n"), Ok(Host::Daemon));
        assert_eq!(Host::from_toml("[serve]\nhost = \"daemon\"\n"), Ok(Host::Daemon));
        assert_eq!(Host::from_toml("[serve]\nhost = \"process\"\n"), Ok(Host::Process));
        let bad = Host::from_toml("[serve]\nhost = \"proces\"\n").unwrap_err();
        assert!(bad.contains("serve.host") && bad.contains("\"process\""), "{bad}");
        let unknown = Host::from_toml("[serve]\nhots = \"process\"\n").unwrap_err();
        assert!(unknown.contains("`hots`"), "{unknown}");
        assert!(Host::from_toml("serve = 1\n").unwrap_err().contains("not a table"));
        for host in [Host::Daemon, Host::Process] {
            assert_eq!(Host::from_toml(&format!("[serve]\nhost = \"{}\"\n", host.word())), Ok(host));
        }
    }
}
