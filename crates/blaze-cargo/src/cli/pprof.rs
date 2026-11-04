//! `blaze pprof <file|url> [verb]`: `go tool pprof` for .ixprof and pprof files, from any
//! program (ix-prof's `IX_PROF=<dir>`, its `/debug/pprof` endpoint, a Go server's, or
//! pprof-rs's output). Verbs: `top [-cum] [-n N]`, `peek REGEX`, `tree [--min F] [--reverse]`,
//! `list REGEX [--source DIR]`, `flame [--width N] [--depth N] [--folded]`,
//! `diff --base FILE|URL [verb]`, `export --pprof|--chrome|--ixprof|--folded OUT`, and
//! `--http [ADDR]`, which serves the file and opens the explorer at it. `--kind
//! cpu|heap|spans|waits` picks what an .ixprof is viewed as (cpu); `--type NAME` the sample type
//! weighed by (the profile's default).
//!
//! A running program's endpoint needs its token, which is kept off the command line (every
//! local user can read another's arguments): `--dir <IX_PROF> ENDPOINT` reads the address
//! and token from `<IX_PROF>/http.endpoint`, and `--token-file FILE` gives the token for an
//! `http://` URL. Both are read only from this user's own 0600 file with no ACL, in a directory
//! that is this user's own, with owner-only mode and no ACL; and for both the address must be a
//! loopback IP address and port (`ix_prof::endpoint`). The token is sent as `Authorization:
//! Bearer`, and only to the source's own host and port, never to a `diff --base` elsewhere.

use std::{
    borrow::Cow,
    ffi::OsString,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_core::exit::Exit;
use ix_prof::{Kind, Profile, View, report};
use regex::Regex;

use super::say;

const PPROF_USAGE: &str = "blaze pprof [--kind cpu|heap|spans|waits] [--type NAME] [--token-file FILE] FILE|URL|--dir IX_PROF ENDPOINT \
                           [top [-cum] [-n N] | peek REGEX | tree [--min FRACTION] [--reverse] | list REGEX [--source DIR] \
                           | flame [--width N] [--depth N] [--folded] | diff --base FILE|URL [VERB ...] \
                           | export --pprof|--chrome|--ixprof|--folded OUT | --http [ADDR]]";

/// The most an endpoint's answer may be; more is refused by name.
const MAX_RESPONSE: u64 = 256 << 20;

/// How a `http://` source is fetched: the token, sent as `Authorization: Bearer` only to the
/// `host:port` it belongs to (the source's own), so a `diff --base` at another endpoint never
/// gets it.
struct Fetch {
    token: Option<Scoped>,
}

struct Scoped {
    /// `host:port` exactly as in the source URL.
    host: String,
    token: String,
}

impl Fetch {
    /// The token for a request to `host` (`host:port`), if it is the one the token is for.
    fn token_for(&self, host: &str) -> Option<&str> {
        self.token.as_ref().filter(|it| it.host == host).map(|it| it.token.as_str())
    }
}

/// What was read: an .ixprof keeps everything; a pprof file is one view.
enum Loaded {
    Ixprof(Profile),
    Pprof(View),
}

impl Loaded {
    /// The view of `kind` an .ixprof is seen as, or the one a pprof file is.
    fn view(&self, kind: Kind) -> Cow<'_, View> {
        match self {
            Loaded::Ixprof(p) => Cow::Owned(View::of(p, kind)),
            Loaded::Pprof(v) => Cow::Borrowed(v),
        }
    }
}

pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    match run(words) {
        Ok(()) => 0,
        Err((code, what)) => {
            eprint!("{}", say::error(&what, color));
            code
        }
    }
}

type Refused = (i32, String);

fn usage(what: impl Into<String>) -> Refused {
    (Exit::Usage.code(), format!("{}\nusage: {PPROF_USAGE}", what.into()))
}

fn failed(what: impl Into<String>) -> Refused {
    (Exit::Failed.code(), what.into())
}

fn run(words: Vec<OsString>) -> Result<(), Refused> {
    let mut words: Vec<String> = words
        .into_iter()
        .map(|it| {
            it.into_string().map_err(|it| usage(format!("{} is not UTF-8", it.to_string_lossy())))
        })
        .collect::<Result<_, _>>()?;
    let mut kind = Kind::Cpu;
    let mut ty: Option<String> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut dir: Option<PathBuf> = None;
    // Global flags may come anywhere before the verb's own.
    let mut i = 0;
    while i < words.len() {
        match words[i].as_str() {
            "--kind" => {
                let k = words
                    .get(i + 1)
                    .ok_or_else(|| usage("`--kind` takes cpu, heap, spans or waits"))?;
                kind = Kind::parse(k)
                    .ok_or_else(|| usage(format!("--kind {k}: give cpu, heap, spans or waits")))?;
                words.drain(i..i + 2);
            }
            "--type" => {
                ty = Some(
                    words
                        .get(i + 1)
                        .ok_or_else(|| usage("`--type` takes a sample type's name"))?
                        .clone(),
                );
                words.drain(i..i + 2);
            }
            "--token-file" => {
                token_file = Some(
                    words
                        .get(i + 1)
                        .ok_or_else(|| usage("`--token-file` takes a file holding the token"))?
                        .into(),
                );
                words.drain(i..i + 2);
            }
            "--dir" => {
                dir = Some(
                    words
                        .get(i + 1)
                        .ok_or_else(|| usage("`--dir` takes the program's IX_PROF directory"))?
                        .into(),
                );
                words.drain(i..i + 2);
            }
            _ => i += 1,
        }
    }
    let mut words = words.into_iter();
    let source = words.next().ok_or_else(|| usage("which profile?"))?;
    let (source, token) = match (&dir, &token_file) {
        (Some(_), Some(_)) => {
            return Err(usage(
                "give --dir or --token-file, not both: --dir reads the token from <IX_PROF>/http.endpoint",
            ));
        }
        (Some(d), None) => {
            let (url, endpoint) = endpoint_url(d, &source)?;
            (url, Some(Scoped { host: endpoint.addr.to_string(), token: endpoint.token }))
        }
        (None, Some(file)) => {
            let host = source
                .strip_prefix("http://")
                .map(|rest| rest.split_once('/').map_or(rest, |it| it.0))
                .ok_or_else(|| {
                    usage(format!("--token-file is for an http:// source, not {source}"))
                })?;
            // Checked before the token is read: it goes to this machine's loopback only, in
            // the clear, as for --dir.
            ix_prof::endpoint::loopback(host)
                .map_err(|why| usage(format!("--token-file with {source}: {why}")))?;
            let token = ix_prof::endpoint::read_token_file(file)
                .map_err(|why| failed(format!("reading the token: {why}")))?;
            (source.clone(), Some(Scoped { host: host.to_owned(), token }))
        }
        (None, None) => (source, None),
    };
    let fetch = Fetch { token };
    let loaded = load(&source, &fetch)?;
    let rest: Vec<String> = words.collect();
    if rest.first().map(String::as_str) == Some("--http") {
        return serve(&loaded, &source, rest.get(1).map(String::as_str).unwrap_or("127.0.0.1:0"));
    }
    let view = loaded.view(kind);
    verb(&loaded, &view, kind, ty.as_deref(), &rest, &fetch)
}

fn verb(
    loaded: &Loaded,
    view: &View,
    kind: Kind,
    ty: Option<&str>,
    rest: &[String],
    fetch: &Fetch,
) -> Result<(), Refused> {
    let ty_index = view.type_index(ty).ok_or_else(|| {
        let names: Vec<&str> = view.sample_types.iter().map(|it| it.name.as_str()).collect();
        usage(format!("--type {}: the profile's sample types are {names:?}", ty.unwrap_or("")))
    })?;
    let asked = rest.first().map_or("top", String::as_str);
    let args = rest.get(1..).unwrap_or(&[]);
    let flag = |name: &str| args.iter().any(|it| it == name);
    let value = |name: &str| args.iter().position(|it| it == name).and_then(|i| args.get(i + 1));
    let number = |name: &str, default: usize| -> Result<usize, Refused> {
        value(name).map_or(Ok(default), |v| {
            v.parse().map_err(|_| usage(format!("{name} {v} is not a number")))
        })
    };
    let pattern = || -> Result<Regex, Refused> {
        let p = args
            .first()
            .ok_or_else(|| usage(format!("`{asked}` takes a regex of function names")))?;
        Regex::new(p).map_err(|why| usage(format!("`{p}` is not a regex: {why}")))
    };
    let out = match asked {
        "top" => report::top(view, ty_index, flag("-cum") || flag("--cum"), number("-n", 20)?),
        "peek" => report::peek(view, ty_index, &pattern()?),
        "tree" => {
            let min: f64 = value("--min").map_or(Ok(0.005), |v| {
                v.parse().map_err(|_| usage(format!("--min {v} is not a fraction")))
            })?;
            report::tree(view, ty_index, min, flag("--reverse"))
        }
        "list" => report::list(view, ty_index, &pattern()?, value("--source").map(Path::new)),
        "flame" if flag("--folded") => report::folded(view, ty_index),
        "flame" => {
            let width = std::env::var("COLUMNS").ok().and_then(|it| it.parse().ok()).unwrap_or(120);
            report::flame(view, ty_index, number("--width", width)?, number("--depth", 40)?)
        }
        "diff" => {
            let base = value("--base").ok_or_else(|| usage("`diff` takes --base FILE|URL"))?;
            let base = load(base, fetch)?;
            let d = report::diff(view, &base.view(kind)).map_err(failed)?;
            let after: Vec<String> = args
                .iter()
                .enumerate()
                .filter(|(i, a)| {
                    *a != "--base" && args.get(i.wrapping_sub(1)).is_none_or(|p| p != "--base")
                })
                .map(|(_, a)| a.clone())
                .collect();
            let diffed = Loaded::Pprof(d);
            return verb(&diffed, &diffed.view(kind), kind, ty, &after, fetch);
        }
        "export" => return export(loaded, view, args),
        other => return Err(usage(format!("no verb `{other}`"))),
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes()).map_err(|why| failed(format!("writing to stdout: {why}")))
}

fn export(loaded: &Loaded, view: &View, args: &[String]) -> Result<(), Refused> {
    let (what, out) = match args {
        [what, out] => (what.as_str(), PathBuf::from(out)),
        _ => {
            return Err(usage(
                "`export` takes --pprof, --chrome, --ixprof or --folded, and a file",
            ));
        }
    };
    let bytes = match (what, loaded) {
        ("--pprof", _) => view.to_pprof(),
        ("--folded", _) => report::folded(view, view.default_type).into_bytes(),
        ("--chrome", Loaded::Ixprof(p)) => ix_prof::chrome::to_chrome(p).into_bytes(),
        ("--ixprof", Loaded::Ixprof(p)) => p.to_ixprof().map_err(|why| failed(why.to_string()))?,
        ("--chrome" | "--ixprof", Loaded::Pprof(_)) => {
            return Err(failed(format!(
                "a pprof file holds one aggregated view, no timeline: {what} needs an .ixprof"
            )));
        }
        (other, _) => {
            return Err(usage(format!(
                "export {other}: give --pprof, --chrome, --ixprof or --folded"
            )));
        }
    };
    std::fs::write(&out, bytes)
        .map_err(|why| failed(format!("writing {}: {why}", out.display())))?;
    eprintln!("wrote {}", out.display());
    Ok(())
}

/// `ENDPOINT` (`heap`, `profile?seconds=10`, `/debug/pprof/ixprof?last=30`) of the program
/// whose `IX_PROF` is `dir`, at the address in `<dir>/http.endpoint`, and that endpoint.
fn endpoint_url(
    dir: &Path,
    endpoint: &str,
) -> Result<(String, ix_prof::endpoint::Endpoint), Refused> {
    let found = ix_prof::endpoint::read(dir)
        .map_err(|why| failed(format!("the endpoint of {}: {why}", dir.display())))?;
    let found = found.ok_or_else(|| {
        failed(format!(
            "no {}: is the program running with IX_PROF_HTTP set and IX_PROF={}?",
            dir.join(ix_prof::endpoint::FILE).display(),
            dir.display()
        ))
    })?;
    let path = endpoint.trim_start_matches('/');
    let path = path.strip_prefix("debug/pprof").map_or(path, |it| it.trim_start_matches('/'));
    Ok((format!("http://{}/debug/pprof/{path}", found.addr), found))
}

/// A file, or an `http://` URL (a `/debug/pprof` endpoint): .ixprof by its magic, else pprof
/// (gzipped or not).
fn load(source: &str, fetch: &Fetch) -> Result<Loaded, Refused> {
    let bytes = if let Some(rest) = source.strip_prefix("http://") {
        if rest
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|kv| kv.starts_with("token=")))
        {
            eprintln!(
                "warning: {source} carries its token on the command line, where every local user can read it (ps); \
                 use `blaze pprof --dir <IX_PROF> ENDPOINT`, or `--token-file FILE` with the token in a 0600 file of your own"
            );
        }
        let host = rest.split_once('/').map_or(rest, |it| it.0);
        let token = fetch.token_for(host);
        get(rest, token).map_err(|why| {
            let note = match (&fetch.token, token) {
                (Some(it), None) => {
                    format!(" (sent with no token: the token is for {} only)", it.host)
                }
                _ => String::new(),
            };
            failed(format!("fetching {source}: {why}{note}"))
        })?
    } else if source.starts_with("https://") {
        return Err(failed(format!(
            "{source}: blaze pprof fetches http:// (a loopback /debug/pprof endpoint); download https profiles first"
        )));
    } else {
        std::fs::read(source).map_err(|why| failed(format!("reading {source}: {why}")))?
    };
    if bytes.starts_with(&ix_prof::ixprof::MAGIC[..6]) {
        return Profile::from_ixprof(&bytes)
            .map(Loaded::Ixprof)
            .map_err(|why| failed(format!("{source}: {why}")));
    }
    View::from_pprof(&bytes)
        .map(Loaded::Pprof)
        .map_err(|why| failed(format!("{source} is neither an .ixprof nor a pprof profile: {why}")))
}

/// HTTP/1.0 GET of `host:port/path`, with the token as `Authorization: Bearer`: the body,
/// read to the end, at most [`MAX_RESPONSE`] bytes.
fn get(rest: &str, token: Option<&str>) -> Result<Vec<u8>, String> {
    let (host, path) =
        rest.split_once('/').map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
    let mut conn = TcpStream::connect(host).map_err(|it| it.to_string())?;
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    conn.write_all(
        format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nAccept: */*\r\n{auth}\r\n").as_bytes(),
    )
    .map_err(|it| it.to_string())?;
    let mut all = Vec::new();
    (&mut conn).take(MAX_RESPONSE + 1).read_to_end(&mut all).map_err(|it| it.to_string())?;
    if all.len() as u64 > MAX_RESPONSE {
        return Err(format!(
            "the answer is over {MAX_RESPONSE} bytes: refused (is something else on that port?)"
        ));
    }
    let end =
        all.windows(4).position(|w| w == b"\r\n\r\n").ok_or("no end of the response's headers")?;
    let head = String::from_utf8_lossy(&all[..end]).into_owned();
    let status = head.lines().next().unwrap_or("");
    let body = all.split_off(end + 4);
    if !status.split_whitespace().nth(1).is_some_and(|it| it == "200") {
        return Err(format!("{status}: {}", String::from_utf8_lossy(&body).trim()));
    }
    Ok(body)
}

/// `Host` names loopback: `127.0.0.1`, `localhost` or `[::1]`, with a port.
fn loopback_host(host: Option<&str>) -> bool {
    let Some(h) = host else { return false };
    let name = match h.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => h,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

/// Serves the profile on loopback and opens the explorer at it, until interrupted:
/// `/profile.ixprof` (an .ixprof input), `/profile.pb.gz` and `/profile.chrome.json`. Like
/// ix-prof's endpoint, every request needs a new random token (`token=` in the query, or
/// `Authorization: Bearer`), since every local account can reach a loopback port. The
/// explorer's link carries the token in its fragment, which the browser never sends to
/// ix.dev, and is opened through a 0600 file so it is not on any process's command line.
fn serve(loaded: &Loaded, source: &str, addr: &str) -> Result<(), Refused> {
    let listener =
        TcpListener::bind(addr).map_err(|why| failed(format!("binding {addr}: {why}")))?;
    let bound = listener
        .local_addr()
        .map_err(|why| failed(format!("the address bound for {addr}: {why}")))?;
    if !bound.ip().is_loopback() {
        return Err(failed(format!(
            "{addr} is not loopback: the profile would be readable from other hosts"
        )));
    }
    let token = ix_prof::token::new()
        .map_err(|why| failed(format!("making the token for {bound}: {why}")))?;
    let (ixprof, chrome) = match loaded {
        Loaded::Ixprof(p) => (
            Some(p.to_ixprof().map_err(|why| failed(why.to_string()))?),
            Some(ix_prof::chrome::to_chrome(p)),
        ),
        Loaded::Pprof(_) => (None, None),
    };
    let pprof = loaded.view(Kind::Cpu).to_pprof();
    let file = if ixprof.is_some() { "profile.ixprof" } else { "profile.pb.gz" };
    let url = format!("http://{bound}/{file}?token={token}");
    let link = format!("https://ix.dev/explore#profile={}", encode_component(&url));
    // stdout is this user's terminal, not a command line.
    println!("serving {source} at {url}\n{link}");
    match open_page(&link) {
        Ok((program, page)) => {
            if let Err(why) = std::process::Command::new(program).arg(&page).status() {
                eprintln!("the link was not opened (`{program}`: {why}): open it in a browser");
            }
        }
        Err(why) => eprintln!("the link was not opened ({why}): open it in a browser"),
    }
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        // One connection at a time: a client that sends nothing is cut off, not waited on.
        if conn.set_read_timeout(Some(Duration::from_secs(5))).is_err()
            || conn.set_write_timeout(Some(Duration::from_secs(30))).is_err()
        {
            continue;
        }
        let mut buf = [0u8; 4096];
        let n = conn.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let target = head.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let header = |name: &str| {
            head.lines().find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.trim().to_owned())
            })
        };
        let origin = header("origin");
        let host = header("host");
        let given = header("authorization")
            .and_then(|v| ix_prof::token::bearer(&v).map(str::to_owned))
            .or_else(|| {
                query.split('&').find_map(|kv| kv.strip_prefix("token=")).map(str::to_owned)
            });
        // A name an attacker's DNS points here (rebinding) is refused; only loopback names
        // pass, and only with the token.
        let allowed =
            loopback_host(host.as_deref()) && ix_prof::token::matches(given.as_deref(), &token);
        let cors = match origin.as_deref() {
            Some(o)
                if o == "https://ix.dev"
                    || o.starts_with("http://localhost:")
                    || o.starts_with("http://127.0.0.1:") =>
            {
                format!(
                    "Access-Control-Allow-Origin: {o}\r\nAccess-Control-Allow-Private-Network: true\r\nVary: Origin\r\n"
                )
            }
            _ => String::new(),
        };
        if !allowed {
            let _ = conn.write_all(
                format!(
                    "HTTP/1.1 403 Forbidden\r\n{cors}Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
            continue;
        }
        let body: Option<(&str, &[u8])> = match path {
            "/profile.ixprof" => ixprof.as_deref().map(|b| ("application/vnd.ix.prof", b)),
            "/profile.pb.gz" => Some(("application/octet-stream", &pprof[..])),
            "/profile.chrome.json" => chrome.as_deref().map(|c| ("application/json", c.as_bytes())),
            _ => None,
        };
        let _ = match body {
            Some((ty, b)) => conn
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {ty}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{cors}Connection: close\r\n\r\n", b.len()).as_bytes())
                .and_then(|()| conn.write_all(b)),
            None => conn.write_all(
                format!("HTTP/1.1 404 Not Found\r\n{cors}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
            ),
        };
    }
    Ok(())
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A page that sends the browser on to `link`, in a new 0700 directory under the temporary
/// one, mode 0600: the opener (and the browser it starts) gets the page's path on its command
/// line, never the token. The page holds the token, so it is written through
/// `ix_prof::endpoint::PrivateDir`, under the same rule as `http.endpoint`: the directory this
/// user's own on a local volume that keeps ownership, owner-only mode and no ACL (one inherited
/// from a parent of `TMPDIR` included). With the program that opens it.
fn open_page(link: &str) -> Result<(&'static str, PathBuf), String> {
    use std::os::unix::fs::DirBuilderExt as _;
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    // A random name of its own, not the token.
    let name =
        ix_prof::token::new().map_err(|why| format!("naming the page's directory: {why}"))?;
    let dir = std::env::temp_dir().join(format!("blaze-pprof-{}-{name}", std::process::id()));
    // Not recursive: a directory someone else made there first is refused, not used.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|why| format!("making {}: {why}", dir.display()))?;
    let page = dir.join("open.html");
    let html_link =
        link.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;");
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=referrer content=no-referrer>\
         <meta http-equiv=refresh content=\"0;url={html_link}\"><a href=\"{html_link}\">open the explorer</a>\n"
    );
    let written = ix_prof::endpoint::PrivateDir::open(&dir)
        .and_then(|private| private.write_whole("open.html", "open.html.part", html.as_bytes()));
    if let Err(why) = written {
        // Refused (or the write failed, which removes its part file): leave no empty directory behind.
        return Err(match std::fs::remove_dir(&dir) {
            Ok(()) => why,
            Err(also) => format!("{why}; removing the empty {}: {also}", dir.display()),
        });
    }
    Ok((program, page))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, io::Read as _, net::TcpListener};

    use super::{Fetch, Scoped, load, run};

    /// S4: the token goes to the source's own host:port only. A `diff --base` at another port
    /// (one another user may have bound) is fetched with no `Authorization` header.
    #[test]
    fn the_token_goes_only_to_the_host_and_port_it_is_for() {
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = other.local_addr().unwrap();
        let seen = std::thread::spawn(move || {
            let (mut c, _) = other.accept().unwrap();
            c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 1024];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            String::from_utf8_lossy(&got).into_owned()
        });
        let fetch = Fetch {
            token: Some(Scoped { host: "127.0.0.1:1".to_owned(), token: "s3cret".to_owned() }),
        };
        assert_eq!(fetch.token_for("127.0.0.1:1"), Some("s3cret"));
        assert_eq!(fetch.token_for("localhost:1"), None);
        // The fetch fails (the listener answers nothing); what matters is what it sent.
        let _ = load(&format!("http://{at}/debug/pprof/heap"), &fetch);
        let request = seen.join().unwrap();
        assert!(request.starts_with("GET /debug/pprof/heap"), "{request:?}");
        assert!(
            !request.contains("s3cret") && !request.to_ascii_lowercase().contains("authorization"),
            "{request:?}"
        );
    }

    /// S8a: `--token-file` sends its token only to a loopback IP address and port, as `--dir`
    /// does. A remote host, a host name (a resolver could point it elsewhere) or an
    /// IPv4-mapped address is refused before the token file is even read; a loopback address
    /// gets as far as reading it.
    #[test]
    fn a_token_file_goes_to_a_loopback_address_only() {
        let token_file = "/nonexistent/ix-prof-pprof-token";
        let refused = |url: &str| {
            let words = ["--token-file", token_file, url].map(OsString::from).to_vec(); // alloc: test
            run(words).expect_err(url).1
        };
        for url in [
            "http://10.0.0.5:6060/debug/pprof/heap",
            "http://localhost:6060/debug/pprof/heap",
            "http://[::ffff:127.0.0.1]:6060/x",
        ] {
            let why = refused(url);
            assert!(why.contains("loopback") && !why.contains("reading the token"), "{url}: {why}");
        }
        let why = refused("http://127.0.0.1:6060/debug/pprof/heap");
        assert!(
            why.contains("reading the token") && why.contains("/nonexistent"),
            "loopback: {why}"
        );
    }

    /// S2 (round 3): the `--http` page, which holds the serve token, is written through
    /// `PrivateDir`: a 0600 regular file, the link in it, in a new 0700 directory of its own.
    #[test]
    fn the_explorer_page_is_written_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let link = "https://ix.dev/explore#profile=http%3A%2F%2F127.0.0.1%3A1%2Fprofile.ixprof%3Ftoken%3Dabc&x";
        let (_, page) = super::open_page(link).unwrap();
        let dir = page.parent().unwrap().to_owned();
        let (html, file_mode, dir_mode) = (
            std::fs::read_to_string(&page).unwrap(),
            std::fs::metadata(&page).unwrap().permissions().mode() & 0o777,
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        );
        let names: Vec<_> =
            std::fs::read_dir(&dir).unwrap().map(|it| it.unwrap().file_name()).collect(); // alloc: test
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((file_mode, dir_mode), (0o600, 0o700));
        assert_eq!(names, ["open.html"]);
        assert!(html.contains(&link.replace('&', "&amp;")), "{html}");
    }
}
