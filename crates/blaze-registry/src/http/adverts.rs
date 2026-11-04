//! What hosts advertised about HTTP/3, per host, until it expires: their DNS HTTPS record
//! (RFC 9460), with no record kept as an answer too (a negative answer, for its SOA's negative
//! TTL), and their `Alt-Svc` header (RFC 7838) for its `ma`. A sans-IO core on an injected clock
//! (seconds since the epoch): the client asks [`Adverts::advert`], tells it what it learned
//! ([`Adverts::learn_dns`], [`Adverts::learn_alt_svc`]), and writes [`Adverts::to_bytes`] to the
//! daemon's state when it says something changed; [`load`] and [`save`] are its file driver.
//!
//! Kept across runs so the first request of a run already knows the host (the daemon's state,
//! `<root>/v2/net/h3-adverts.json`). A missing file is no adverts yet; a file that can't be read
//! is reset with a warning naming it and why, and moved aside, never silently emptied.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde_derive::{Deserialize, Serialize};

use super::choice::{Advert, AltSvc, BUILTIN, Source};

/// The least time a DNS answer is kept: a TTL of 0 or 5 seconds would ask DNS again for nearly
/// every request.
pub const DNS_MIN_TTL_SECONDS: u64 = 30;
/// The most time any advert is kept, a DNS answer or an `Alt-Svc`'s `ma`: a host that stops
/// serving HTTP/3 without saying so fails for at most this long.
pub const MAX_TTL_SECONDS: u64 = 24 * 60 * 60;

/// The file's format: another is reset, with a warning naming it.
const FORMAT: u32 = 1;

/// Every host's adverts, ordered by host so the file is the same bytes for the same adverts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adverts {
    format: u32,
    hosts: BTreeMap<String, Host>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Host {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dns: Option<Dns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alt_svc: Option<Until>,
}

/// The host's DNS HTTPS record as last asked: `h3` on `port`, or no `h3` (no record, a record
/// without it, or a DNS failure, which is no advert: addresses come from getaddrinfo anyway).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dns {
    /// The port of its `h3` service, none when it advertises none.
    h3: Option<u16>,
    until: u64,
}

/// HTTP/3 on `port` until `until` (seconds since the epoch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Until {
    port: u16,
    until: u64,
}

/// What a DNS HTTPS lookup of a host found, for how long (its TTL, or its SOA's negative TTL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsAnswer {
    pub h3: Option<u16>,
    pub ttl_seconds: u64,
}

impl Adverts {
    pub fn new() -> Adverts {
        Adverts { format: FORMAT, hosts: BTreeMap::new() }
    }

    /// `host`'s advert of HTTP/3 at `now`: its DNS HTTPS record's, else its `Alt-Svc`'s, else
    /// [`BUILTIN`]'s.
    pub fn advert(&self, host: &str, now: u64) -> Option<Advert> {
        let known = self.hosts.get(host);
        let dns = known.and_then(|it| it.dns).filter(|it| it.until > now).and_then(|it| it.h3);
        if let Some(port) = dns {
            return Some(Advert { port, source: Source::Dns });
        }
        if let Some(alt) = known.and_then(|it| it.alt_svc).filter(|it| it.until > now) {
            return Some(Advert { port: alt.port, source: Source::AltSvc });
        }
        BUILTIN.contains(&host).then_some(Advert { port: 443, source: Source::Builtin })
    }

    /// Whether `host`'s DNS HTTPS record is known at `now`, as an answer or as none: if not,
    /// the client asks DNS before choosing.
    pub fn dns_known(&self, host: &str, now: u64) -> bool {
        self.hosts.get(host).and_then(|it| it.dns).is_some_and(|it| it.until > now)
    }

    /// Keeps what DNS said of `host` at `now`, its TTL held to [`DNS_MIN_TTL_SECONDS`] and
    /// [`MAX_TTL_SECONDS`]. Always a change worth keeping: it is asked once per TTL.
    pub fn learn_dns(&mut self, host: &str, answer: DnsAnswer, now: u64) {
        let ttl = answer.ttl_seconds.clamp(DNS_MIN_TTL_SECONDS, MAX_TTL_SECONDS);
        self.hosts.entry(host.to_owned()).or_default().dns = Some(Dns { h3: answer.h3, until: now + ttl }); // alloc: once per host per TTL
    }

    /// Keeps what an `Alt-Svc` header from `host` said at `now`: an `h3` alternative for its
    /// `ma` (at most [`MAX_TTL_SECONDS`]), or `clear`. Whether the file should be written: a new
    /// or changed alternative, a clear of one, or one past half its time renewed; not every
    /// answer's repeat of the same header.
    pub fn learn_alt_svc(&mut self, host: &str, said: AltSvc, now: u64) -> bool {
        match said {
            AltSvc::Nothing => false,
            AltSvc::Clear => self.hosts.get_mut(host).and_then(|it| it.alt_svc.take()).is_some(),
            AltSvc::H3 { port, max_age } => {
                let ttl = max_age.min(MAX_TTL_SECONDS);
                if ttl == 0 {
                    return self.hosts.get_mut(host).and_then(|it| it.alt_svc.take()).is_some();
                }
                let entry = &mut self.hosts.entry(host.to_owned()).or_default().alt_svc; // alloc: once per host
                let fresh = entry.is_some_and(|it| it.port == port && it.until >= now + ttl / 2);
                if !fresh {
                    *entry = Some(Until { port, until: now + ttl });
                }
                !fresh
            }
        }
    }

    /// Drops what expired by `now`.
    pub fn forget_expired(&mut self, now: u64) {
        for host in self.hosts.values_mut() {
            host.dns = host.dns.filter(|it| it.until > now);
            host.alt_svc = host.alt_svc.filter(|it| it.until > now);
        }
        self.hosts.retain(|_, it| it.dns.is_some() || it.alt_svc.is_some());
    }

    /// The file's bytes: JSON, hosts in order, one trailing newline.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).expect("adverts are plain data, always JSON");
        bytes.push(b'\n');
        bytes
    }

    /// The adverts of a file's `bytes`, less what expired by `now`; an error says why they
    /// are not adverts of this format.
    pub fn from_bytes(bytes: &[u8], now: u64) -> Result<Adverts, String> {
        let mut adverts: Adverts = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if adverts.format != FORMAT {
            return Err(format!("it is format {}, and this blaze reads format {FORMAT}", adverts.format));
        }
        adverts.forget_expired(now);
        Ok(adverts)
    }
}

/// The adverts kept in `file` at `now`: none when there is no file yet. A file that can't be
/// read, or holds no adverts of this format, is moved aside to `<file>.unreadable`, and the
/// warning returned with the empty adverts names the file and why.
pub fn load(file: &Path, now: u64) -> (Adverts, Option<String>) {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Adverts::new(), None),
        Err(error) => {
            return (Adverts::new(), Some(format!("{} can't be read ({error}); starting with no adverts of HTTP/3", file.display())));
        }
    };
    match Adverts::from_bytes(&bytes, now) {
        Ok(adverts) => (adverts, None),
        Err(why) => {
            let aside = aside(file);
            let moved = match std::fs::rename(file, &aside) {
                Ok(()) => format!("moved it to {}", aside.display()),
                Err(error) => format!("could not move it to {} ({error})", aside.display()),
            };
            (
                Adverts::new(),
                Some(format!(
                    "{} holds no adverts of HTTP/3 this blaze can read ({why}); starting with none, and {moved}",
                    file.display()
                )),
            )
        }
    }
}

fn aside(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".unreadable");
    PathBuf::from(name)
}

/// Writes `bytes` to `file` whole: to a file of this process's beside it, then renamed over it,
/// so a reader never sees half of it.
pub fn save(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut part = file.as_os_str().to_owned();
    part.push(format!(".{}.part", std::process::id()));
    let part = PathBuf::from(part);
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Adverts written and read back are the same adverts and the same bytes as the ones
    /// written, less what expired; a corrupt file is reset with a warning naming it, moved
    /// aside, and a missing one is no adverts with no warning.
    #[test]
    fn adverts_round_trip_through_their_file() {
        let now = 1_800_000_000;
        let mut adverts = Adverts::new();
        adverts.learn_dns("index.crates.io", DnsAnswer { h3: Some(443), ttl_seconds: 300 }, now);
        adverts.learn_dns("github.com", DnsAnswer { h3: None, ttl_seconds: 5 }, now);
        assert!(adverts.learn_alt_svc("static.crates.io", AltSvc::H3 { port: 443, max_age: 86_400 }, now));
        assert!(adverts.learn_alt_svc("gone.example", AltSvc::H3 { port: 443, max_age: 60 }, now));
        let dir = std::env::temp_dir().join(format!("blaze-adverts-{}", std::process::id()));
        let file = dir.join("net").join("h3-adverts.json");
        save(&file, &adverts.to_bytes()).unwrap();

        let (restored, warning) = load(&file, now);
        assert_eq!(warning, None);
        assert_eq!(restored, adverts);
        assert_eq!(restored.to_bytes(), adverts.to_bytes());
        assert_eq!(restored.advert("index.crates.io", now), Some(Advert { port: 443, source: Source::Dns }));
        assert!(restored.dns_known("github.com", now + 29) && !restored.dns_known("github.com", now + 30));

        // Two minutes on, `gone.example`'s `ma` of 60 has passed: restored as if never learned.
        let later = now + 120;
        let (restored, _) = load(&file, later);
        let mut fresh = adverts.clone();
        fresh.forget_expired(later);
        assert_eq!(restored.to_bytes(), fresh.to_bytes());
        assert_eq!(restored.advert("gone.example", later), None);

        std::fs::write(&file, b"{\"format\": 1, \"hosts\": ").unwrap();
        let (restored, warning) = load(&file, now);
        assert_eq!(restored, Adverts::new());
        let warning = warning.unwrap();
        assert!(warning.contains(&file.display().to_string()) && warning.contains("unreadable"), "{warning}");
        assert!(!file.exists() && aside(&file).exists());
        assert_eq!(load(&file, now), (Adverts::new(), None));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
