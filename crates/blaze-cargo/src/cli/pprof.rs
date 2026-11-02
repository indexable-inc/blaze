//! `blaze pprof <file|url> [verb]`: `go tool pprof` for .ixprof and pprof files, from any
//! program (ix-prof's `IX_PROF=<dir>`, its `/debug/pprof` endpoint, a Go server's, or
//! pprof-rs's output). Verbs: `top [-cum] [-n N]`, `peek REGEX`, `tree [--min F] [--reverse]`,
//! `list REGEX [--source DIR]`, `flame [--width N] [--depth N] [--folded]`,
//! `diff --base FILE|URL [verb]`, `export --pprof|--chrome|--ixprof|--folded OUT`, and
//! `--http [ADDR]`, which serves the file and opens the explorer at it. `--kind
//! cpu|heap|spans|waits` picks what an .ixprof is viewed as (cpu); `--type NAME` the sample type
//! weighed by (the profile's default).

use std::{
    ffi::OsString,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
};

use ix_prof::{Kind, Profile, View, report};
use regex::Regex;

use super::{FAILED, USAGE, say};

const PPROF_USAGE: &str = "blaze pprof [--kind cpu|heap|spans|waits] [--type NAME] FILE|URL \
                           [top [-cum] [-n N] | peek REGEX | tree [--min FRACTION] [--reverse] | list REGEX [--source DIR] \
                           | flame [--width N] [--depth N] [--folded] | diff --base FILE|URL [VERB ...] \
                           | export --pprof|--chrome|--ixprof|--folded OUT | --http [ADDR]]";

/// What was read: an .ixprof keeps everything; a pprof file is one view.
enum Loaded {
    Ixprof(Profile),
    Pprof(View),
}

impl Loaded {
    fn view(&self, kind: Kind) -> View {
        match self {
            Loaded::Ixprof(p) => View::of(p, kind),
            Loaded::Pprof(v) => v.clone(),
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
    (USAGE, format!("{}\nusage: {PPROF_USAGE}", what.into()))
}

fn failed(what: impl Into<String>) -> Refused {
    (FAILED, what.into())
}

fn run(words: Vec<OsString>) -> Result<(), Refused> {
    let mut words: Vec<String> = words
        .into_iter()
        .map(|it| it.into_string().map_err(|it| usage(format!("{} is not UTF-8", it.to_string_lossy()))))
        .collect::<Result<_, _>>()?;
    let mut kind = Kind::Cpu;
    let mut ty: Option<String> = None;
    // Global flags may come anywhere before the verb's own.
    let mut i = 0;
    while i < words.len() {
        match words[i].as_str() {
            "--kind" => {
                let k = words.get(i + 1).ok_or_else(|| usage("`--kind` takes cpu, heap, spans or waits"))?;
                kind = Kind::parse(k).ok_or_else(|| usage(format!("--kind {k}: give cpu, heap, spans or waits")))?;
                words.drain(i..i + 2);
            }
            "--type" => {
                ty = Some(words.get(i + 1).ok_or_else(|| usage("`--type` takes a sample type's name"))?.clone());
                words.drain(i..i + 2);
            }
            _ => i += 1,
        }
    }
    let mut words = words.into_iter();
    let source = words.next().ok_or_else(|| usage("which profile?"))?;
    let loaded = load(&source)?;
    let rest: Vec<String> = words.collect();
    if rest.first().map(String::as_str) == Some("--http") {
        return serve(&loaded, &source, rest.get(1).map(String::as_str).unwrap_or("127.0.0.1:0"));
    }
    let view = loaded.view(kind);
    verb(&loaded, view, kind, ty.as_deref(), &rest)
}

fn verb(loaded: &Loaded, view: View, kind: Kind, ty: Option<&str>, rest: &[String]) -> Result<(), Refused> {
    let ty_index = view.type_index(ty).ok_or_else(|| {
        let names: Vec<&str> = view.sample_types.iter().map(|it| it.name.as_str()).collect();
        usage(format!("--type {}: the profile's sample types are {names:?}", ty.unwrap_or("")))
    })?;
    let asked = rest.first().map_or("top", String::as_str);
    let args = rest.get(1..).unwrap_or(&[]);
    let flag = |name: &str| args.iter().any(|it| it == name);
    let value = |name: &str| args.iter().position(|it| it == name).and_then(|i| args.get(i + 1));
    let number = |name: &str, default: usize| -> Result<usize, Refused> {
        value(name).map_or(Ok(default), |v| v.parse().map_err(|_| usage(format!("{name} {v} is not a number"))))
    };
    let pattern = || -> Result<Regex, Refused> {
        let p = args.first().ok_or_else(|| usage(format!("`{asked}` takes a regex of function names")))?;
        Regex::new(p).map_err(|why| usage(format!("`{p}` is not a regex: {why}")))
    };
    let out = match asked {
        "top" => report::top(&view, ty_index, flag("-cum") || flag("--cum"), number("-n", 20)?),
        "peek" => report::peek(&view, ty_index, &pattern()?),
        "tree" => {
            let min: f64 = value("--min").map_or(Ok(0.005), |v| v.parse().map_err(|_| usage(format!("--min {v} is not a fraction"))))?;
            report::tree(&view, ty_index, min, flag("--reverse"))
        }
        "list" => report::list(&view, ty_index, &pattern()?, value("--source").map(Path::new)),
        "flame" if flag("--folded") => report::folded(&view, ty_index),
        "flame" => {
            let width = std::env::var("COLUMNS").ok().and_then(|it| it.parse().ok()).unwrap_or(120);
            report::flame(&view, ty_index, number("--width", width)?, number("--depth", 40)?)
        }
        "diff" => {
            let base = value("--base").ok_or_else(|| usage("`diff` takes --base FILE|URL"))?;
            let base_view = load(base)?.view(kind);
            let d = report::diff(&view, &base_view).map_err(failed)?;
            let after: Vec<String> = args
                .iter()
                .enumerate()
                .filter(|(i, a)| *a != "--base" && args.get(i.wrapping_sub(1)).is_none_or(|p| p != "--base"))
                .map(|(_, a)| a.clone())
                .collect();
            return verb(&Loaded::Pprof(d.clone()), d, kind, ty, &after);
        }
        "export" => return export(loaded, &view, args),
        other => return Err(usage(format!("no verb `{other}`"))),
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes()).map_err(|why| failed(format!("writing to stdout: {why}")))
}

fn export(loaded: &Loaded, view: &View, args: &[String]) -> Result<(), Refused> {
    let (what, out) = match args {
        [what, out] => (what.as_str(), PathBuf::from(out)),
        _ => return Err(usage("`export` takes --pprof, --chrome, --ixprof or --folded, and a file")),
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
        (other, _) => return Err(usage(format!("export {other}: give --pprof, --chrome, --ixprof or --folded"))),
    };
    std::fs::write(&out, bytes).map_err(|why| failed(format!("writing {}: {why}", out.display())))?;
    eprintln!("wrote {}", out.display());
    Ok(())
}

/// A file, or an `http://` URL (a `/debug/pprof` endpoint): .ixprof by its magic, else pprof
/// (gzipped or not).
fn load(source: &str) -> Result<Loaded, Refused> {
    let bytes = if let Some(rest) = source.strip_prefix("http://") {
        get(rest).map_err(|why| failed(format!("fetching {source}: {why}")))?
    } else if source.starts_with("https://") {
        return Err(failed(format!(
            "{source}: blaze pprof fetches http:// (a loopback /debug/pprof endpoint); download https profiles first"
        )));
    } else {
        std::fs::read(source).map_err(|why| failed(format!("reading {source}: {why}")))?
    };
    if bytes.starts_with(&ix_prof::ixprof::MAGIC[..6]) {
        return Profile::from_ixprof(&bytes).map(Loaded::Ixprof).map_err(|why| failed(format!("{source}: {why}")));
    }
    View::from_pprof(&bytes)
        .map(Loaded::Pprof)
        .map_err(|why| failed(format!("{source} is neither an .ixprof nor a pprof profile: {why}")))
}

/// HTTP/1.0 GET of `host:port/path`: the body, read to the end.
fn get(rest: &str) -> Result<Vec<u8>, String> {
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
    let mut conn = TcpStream::connect(host).map_err(|it| it.to_string())?;
    conn.write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nAccept: */*\r\n\r\n").as_bytes())
        .map_err(|it| it.to_string())?;
    let mut all = Vec::new();
    conn.read_to_end(&mut all).map_err(|it| it.to_string())?;
    let end = all.windows(4).position(|w| w == b"\r\n\r\n").ok_or("no end of the response's headers")?;
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
/// `/profile.ixprof` (an .ixprof input), `/profile.pb.gz` and `/profile.chrome.json`.
fn serve(loaded: &Loaded, source: &str, addr: &str) -> Result<(), Refused> {
    let listener = TcpListener::bind(addr).map_err(|why| failed(format!("binding {addr}: {why}")))?;
    let bound = listener.local_addr().map_err(|why| failed(format!("the address bound for {addr}: {why}")))?;
    if !bound.ip().is_loopback() {
        return Err(failed(format!("{addr} is not loopback: the profile would be readable from other hosts")));
    }
    let (ixprof, chrome) = match loaded {
        Loaded::Ixprof(p) => (Some(p.to_ixprof().map_err(|why| failed(why.to_string()))?), Some(ix_prof::chrome::to_chrome(p))),
        Loaded::Pprof(_) => (None, None),
    };
    let pprof = loaded.view(Kind::Cpu).to_pprof();
    let file = if ixprof.is_some() { "profile.ixprof" } else { "profile.pb.gz" };
    let link = format!("https://ix.dev/explore?profile=http://{bound}/{file}");
    println!("serving {source} at http://{bound}/{file}\n{link}");
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if let Err(why) = std::process::Command::new(program).arg(&link).status() {
        eprintln!("the link was not opened (`{program}`: {why}): open it in a browser");
    }
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let mut buf = [0u8; 4096];
        let n = conn.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let path = head.split_whitespace().nth(1).unwrap_or("/");
        let origin = head
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("origin")).map(|(_, v)| v.trim().to_owned()));
        let host = head
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("host")).map(|(_, v)| v.trim().to_owned()));
        // A name an attacker's DNS points here (rebinding) is refused; only loopback names pass.
        let body: Option<(&str, &[u8])> = if !loopback_host(host.as_deref()) {
            None
        } else {
            match path {
            "/profile.ixprof" => ixprof.as_deref().map(|b| ("application/vnd.ix.prof", b)),
            "/profile.pb.gz" => Some(("application/octet-stream", &pprof[..])),
            "/profile.chrome.json" => chrome.as_deref().map(|c| ("application/json", c.as_bytes())),
            _ => None,
            }
        };
        let cors = match origin.as_deref() {
            Some(o) if o == "https://ix.dev" || o.starts_with("http://localhost:") || o.starts_with("http://127.0.0.1:") => {
                format!("Access-Control-Allow-Origin: {o}\r\nAccess-Control-Allow-Private-Network: true\r\nVary: Origin\r\n")
            }
            _ => String::new(),
        };
        let _ = match body {
            Some((ty, b)) => conn
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {ty}\r\nContent-Length: {}\r\n{cors}Connection: close\r\n\r\n", b.len()).as_bytes())
                .and_then(|()| conn.write_all(b)),
            None => conn.write_all(
                format!("HTTP/1.1 404 Not Found\r\n{cors}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
            ),
        };
    }
    Ok(())
}
