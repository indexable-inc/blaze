//! Which protocol one request goes over: TCP (HTTP/2 where the server's ALPN picks it, else
//! HTTP/1.1) or HTTP/3 over QUIC, decided by [`choose`] from the setting (`net.http3`), what
//! the host advertises and whether a proxy is in the way. A pure function of those, so the
//! client and its test decide alike (docs/networking.md, "HTTP/3").
//!
//! blaze never probes: HTTP/3 is used for a host only when the host advertised it (its DNS
//! HTTPS record lists `h3`, or an `Alt-Svc` header said so), when it is on [`BUILTIN`], or when
//! `net.http3 = "require"` asks for it on every request. Choosing what the server offers is
//! negotiation; once HTTP/3 is chosen, a transport failure is an error ([`Http3Failed`]) and the
//! request is never sent again over TCP (rule no-fallback).

use std::fmt;

/// `net.http3` in blaze's user configuration (`~/.blaze/config.toml`), for every host or one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Http3 {
    /// Never HTTP/3: every request over TCP. The default until a benchmark shows a win.
    #[default]
    Off,
    /// HTTP/3 to a host that advertises it ([`Source`]), TCP to the rest and through a proxy.
    Auto,
    /// HTTP/3 for every request; a host behind a proxy, or a plain `http://` address, is an
    /// error ([`Refusal`]).
    Require,
}

impl Http3 {
    /// What the setting takes, as an error says it.
    pub const NAMES: &'static str = "\"off\", \"auto\" or \"require\"";

    pub fn parse(text: &str) -> Option<Http3> {
        match text {
            "off" => Some(Http3::Off),
            "auto" => Some(Http3::Auto),
            "require" => Some(Http3::Require),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Http3::Off => "off",
            Http3::Auto => "auto",
            Http3::Require => "require",
        }
    }
}

/// Why HTTP/3 was chosen for a host: said in every span of its requests and in its errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The host's DNS HTTPS record (RFC 9460) lists `h3` in its `alpn`.
    Dns,
    /// An earlier answer from the host carried `Alt-Svc: h3=":port"` (RFC 7838).
    AltSvc,
    /// The host is on [`BUILTIN`].
    Builtin,
    /// `net.http3 = "require"`, whatever the host advertises.
    Required,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Dns => "dns",
            Source::AltSvc => "alt-svc",
            Source::Builtin => "builtin",
            Source::Required => "required",
        }
    }

    /// Why, as an error's sentence says it.
    fn because(self) -> &'static str {
        match self {
            Source::Dns => "its DNS HTTPS record lists h3",
            Source::AltSvc => "an earlier answer from it carried an Alt-Svc header offering h3",
            Source::Builtin => "it is on blaze's built-in list of hosts known to serve HTTP/3",
            Source::Required => "net.http3 is \"require\"",
        }
    }
}

/// Hosts known to serve HTTP/3 on port 443, used under `"auto"` before any advert is learned.
/// Empty until one is measured: a host here that stops serving HTTP/3 fails every request to
/// it, so only a host blaze's own infrastructure controls belongs here. (Cloudflare-fronted
/// hosts publish DNS HTTPS records, which [`Source::Dns`] reads anyway.)
pub const BUILTIN: &[&str] = &[];

/// A host's advert of HTTP/3: on which UDP port, and how blaze learned it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advert {
    pub port: u16,
    pub source: Source,
}

/// What [`choose`] picks for a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// TCP and TLS: HTTP/2 where the server's ALPN picks it, else HTTP/1.1.
    Tcp,
    /// HTTP/3 over QUIC to `port` of the host, chosen for `source`.
    H3 { port: u16, source: Source },
}

/// A request `net.http3 = "require"` can't make over HTTP/3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `host` is reached through `proxy` (`HTTPS_PROXY` or `ALL_PROXY`, its credentials
    /// removed), which carries TCP only.
    ThroughProxy { host: String, proxy: String },
    /// `url` is a plain `http://` address, and HTTP/3 runs over TLS only.
    PlainHttp { url: String },
}

/// The protocol of a request to `host` (of `url`, `https` when its scheme is `https`) under
/// `setting`, given the host's `advert` (a cached one, or [`BUILTIN`]'s) and the `proxy` the
/// environment names for it.
///
/// | setting   | proxy | plain http | advert  | protocol                       |
/// |-----------|-------|------------|---------|--------------------------------|
/// | off       | any   | any        | any     | TCP                            |
/// | auto      | yes   | any        | any     | TCP                            |
/// | auto      | no    | yes        | any     | TCP                            |
/// | auto      | no    | no         | some    | HTTP/3, the advert's port      |
/// | auto      | no    | no         | none    | TCP                            |
/// | require   | yes   | any        | any     | refused: through a proxy       |
/// | require   | no    | yes        | any     | refused: plain http            |
/// | require   | no    | no         | some    | HTTP/3, the advert's port      |
/// | require   | no    | no         | none    | HTTP/3 on 443, `Required`      |
pub fn choose(
    setting: Http3,
    host: &str,
    url: &str,
    https: bool,
    advert: Option<Advert>,
    proxy: Option<&str>,
) -> Result<Protocol, Refusal> {
    match setting {
        Http3::Off => Ok(Protocol::Tcp),
        Http3::Auto => Ok(match advert {
            Some(advert) if proxy.is_none() && https => Protocol::H3 { port: advert.port, source: advert.source },
            _ => Protocol::Tcp,
        }),
        Http3::Require => {
            if let Some(proxy) = proxy {
                return Err(Refusal::ThroughProxy { host: host.to_owned(), proxy: proxy.to_owned() });
            }
            if !https {
                return Err(Refusal::PlainHttp { url: url.to_owned() });
            }
            let advert = advert.unwrap_or(Advert { port: 443, source: Source::Required });
            Ok(Protocol::H3 { port: advert.port, source: advert.source })
        }
    }
}

/// The proxy the environment names for HTTPS requests (`HTTPS_PROXY`, `https_proxy`,
/// `ALL_PROXY`, `all_proxy`, the first set) and the hosts `NO_PROXY` (`no_proxy`) exempts, read
/// once when the client is made. blaze's TCP client does not go through a proxy yet (issue
/// #1495); this only keeps HTTP/3, which no HTTP proxy carries, off the hosts a proxy is meant
/// for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proxies {
    /// The proxy's address with no userinfo, query or fragment ([`blaze_fetch::redact`]).
    proxy: Option<String>,
    /// `NO_PROXY`'s entries, lower case, without a leading `.`; `*` exempts every host.
    exempt: Vec<String>,
}

impl Proxies {
    /// The proxies `var` names (the process's environment, or a test's).
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Proxies {
        let set = |name: &str| var(name).filter(|it| !it.trim().is_empty());
        let proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"].into_iter().find_map(set);
        let exempt = set("NO_PROXY").or_else(|| set("no_proxy")).unwrap_or_default();
        Proxies {
            proxy: proxy.map(|it| said(it.trim())),
            exempt: exempt
                .split(',')
                .map(|it| it.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|it| !it.is_empty())
                .collect(), // alloc: once per client
        }
    }

    /// The proxy `host` is reached through, if any: none when `NO_PROXY` exempts it (`*`, the
    /// host itself, or a domain it is under).
    pub fn for_host(&self, host: &str) -> Option<&str> {
        let proxy = self.proxy.as_deref()?;
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let exempt = self.exempt.iter().any(|it| {
            it == "*" || host == *it || (host.len() > it.len() && host.ends_with(it.as_str()) && host.as_bytes()[host.len() - it.len() - 1] == b'.')
        });
        if exempt { None } else { Some(proxy) }
    }
}

/// A proxy's address as a message may say it: no userinfo, query or fragment, whether or not
/// it has a scheme (`HTTPS_PROXY=user:pw@proxy:3128` is common).
fn said(proxy: &str) -> String {
    if proxy.contains("://") {
        return blaze_fetch::redact(proxy);
    }
    let authority = &proxy[..proxy.find(['/', '?', '#']).unwrap_or(proxy.len())];
    authority[authority.rfind('@').map_or(0, |it| it + 1)..].to_owned() // alloc: once per client
}

/// What an `Alt-Svc` header (RFC 7838) says about HTTP/3 on the host that sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AltSvc {
    /// `clear`: forget every alternative of this host.
    Clear,
    /// `h3=":port"` on the same host, good for `max_age` seconds (`ma`, 24 hours by default).
    H3 { port: u16, max_age: u64 },
    /// No HTTP/3 alternative on the same host (another protocol, a draft version, or another
    /// host, which blaze does not follow).
    Nothing,
}

/// The default `ma` of an alternative (RFC 7838, section 3.1).
const DEFAULT_MAX_AGE: u64 = 24 * 60 * 60;

/// The first `h3` alternative on the same host in `header`, or `clear`.
pub fn alt_svc(header: &str) -> AltSvc {
    if header.trim().eq_ignore_ascii_case("clear") {
        return AltSvc::Clear;
    }
    for entry in split_outside_quotes(header, ',') {
        let mut parts = split_outside_quotes(entry, ';').into_iter();
        let Some((protocol, authority)) = parts.next().and_then(|it| it.split_once('=')) else { continue };
        if protocol.trim() != "h3" {
            continue;
        }
        let authority = authority.trim().trim_matches('"');
        // `":443"`: the same host. Another host's alternative is not followed.
        let Some(port) = authority.strip_prefix(':').and_then(|it| it.parse::<u16>().ok()) else { continue };
        let mut max_age = DEFAULT_MAX_AGE;
        for parameter in parts {
            if let Some((name, value)) = parameter.split_once('=')
                && name.trim().eq_ignore_ascii_case("ma")
                && let Ok(seconds) = value.trim().trim_matches('"').parse::<u64>()
            {
                max_age = seconds;
            }
        }
        return AltSvc::H3 { port, max_age };
    }
    AltSvc::Nothing
}

/// `text` split at `by` where it is not inside a quoted string.
fn split_outside_quotes(text: &str, by: char) -> Vec<&str> {
    let mut parts = Vec::new(); // alloc: a header's few parts, once per answer with Alt-Svc
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (at, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            _ if c == by && !quoted => {
                parts.push(&text[start..at]);
                start = at + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// A request blaze sent over HTTP/3 that failed at the transport (the QUIC handshake, the
/// connection, or the stream), naming the host, the address, what failed and why HTTP/3 was
/// chosen. It is not sent again over TCP (rule no-fallback), and not tried again over HTTP/3:
/// the retries of `net.retry` are for answers a server gave.
#[derive(Debug)]
pub struct Http3Failed {
    pub host: String,
    pub port: u16,
    /// The address asked, with no userinfo, query or fragment ([`blaze_fetch::redact`]).
    pub url: String,
    pub source: Source,
    /// What failed: "the QUIC handshake with 104.16.0.1:443 got no answer in 10000 ms", ...
    pub what: String,
    /// The user configuration file the fix goes in, as the message says it.
    pub config: String,
}

impl fmt::Display for Http3Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HTTP/3 to {}:{} for {} failed: {}. blaze chose HTTP/3 for {} because {} (source: {}), and does \
             not send the request again over TCP. If UDP to this host is blocked here, set `net.http3 = \"off\"` \
             under `[net]` in {}, or turn it off for this host alone with `\"{}\" = \"off\"` under `[net.http3-hosts]`",
            self.host,
            self.port,
            self.url,
            self.what,
            self.host,
            self.source.because(),
            self.source.name(),
            self.config,
            self.host,
        )
    }
}

impl std::error::Error for Http3Failed {}

/// A [`Refusal`], with the configuration file that asked for HTTP/3.
#[derive(Debug)]
pub struct Http3Refused {
    pub refusal: Refusal,
    pub config: String,
}

impl fmt::Display for Http3Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.refusal {
            Refusal::ThroughProxy { host, proxy } => write!(
                f,
                "{host} is reached through the proxy {proxy} (HTTPS_PROXY or ALL_PROXY), which carries TCP only, \
                 and `net.http3 = \"require\"` in {} asks for HTTP/3 on every request; set it to \"auto\" (which \
                 keeps proxied hosts on TCP) or \"off\", or exempt {host} in NO_PROXY",
                self.config
            ),
            Refusal::PlainHttp { url } => write!(
                f,
                "{url} is a plain http:// address, HTTP/3 runs over TLS only, and `net.http3 = \"require\"` in {} \
                 asks for HTTP/3 on every request; set it to \"auto\" or \"off\"",
                self.config
            ),
        }
    }
}

impl std::error::Error for Http3Refused {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of [`choose`]'s table: the setting × whether a proxy is in the way × a plain
    /// `http://` address × the advert, and only `require` refuses.
    #[test]
    fn the_protocol_follows_the_setting_the_advert_and_the_proxy() {
        let (host, url) = ("index.crates.io", "https://index.crates.io/se/rd/serde");
        let dns = Some(Advert { port: 443, source: Source::Dns });
        let alt = Some(Advert { port: 8443, source: Source::AltSvc });
        let proxy = Some("http://proxy.corp:3128");
        let h3 = |port, source| Ok(Protocol::H3 { port, source });
        let through = Err(Refusal::ThroughProxy { host: host.to_owned(), proxy: "http://proxy.corp:3128".to_owned() });
        let plain = Err(Refusal::PlainHttp { url: url.to_owned() });
        let rows = [
            (Http3::Off, None, true, dns, Ok(Protocol::Tcp)),
            (Http3::Off, proxy, true, dns, Ok(Protocol::Tcp)),
            (Http3::Off, None, false, None, Ok(Protocol::Tcp)),
            (Http3::Auto, None, true, dns, h3(443, Source::Dns)),
            (Http3::Auto, None, true, alt, h3(8443, Source::AltSvc)),
            (Http3::Auto, None, true, None, Ok(Protocol::Tcp)),
            (Http3::Auto, proxy, true, dns, Ok(Protocol::Tcp)),
            (Http3::Auto, None, false, dns, Ok(Protocol::Tcp)),
            (Http3::Require, None, true, alt, h3(8443, Source::AltSvc)),
            (Http3::Require, None, true, None, h3(443, Source::Required)),
            (Http3::Require, proxy, true, dns, through.clone()),
            (Http3::Require, proxy, false, None, through),
            (Http3::Require, None, false, dns, plain),
        ];
        for (setting, proxy, https, advert, want) in rows {
            assert_eq!(
                choose(setting, host, url, https, advert, proxy),
                want,
                "{setting:?}, proxy {proxy:?}, https {https}, advert {advert:?}"
            );
        }
        // The proxy the environment names reaches the decision only for hosts NO_PROXY leaves.
        let env = |name: &str| match name {
            "HTTPS_PROXY" => Some("http://user:secret@proxy.corp:3128".to_owned()),
            "NO_PROXY" => Some("localhost, .crates.io".to_owned()),
            _ => None,
        };
        let proxies = Proxies::from_env(env);
        assert_eq!(proxies.for_host("index.crates.io"), None);
        assert_eq!(proxies.for_host("crates.io"), None);
        assert_eq!(proxies.for_host("notcrates.io"), Some("http://proxy.corp:3128"));
        assert_eq!(choose(Http3::Auto, "github.com", url, true, dns, proxies.for_host("github.com")), Ok(Protocol::Tcp));
    }
}
