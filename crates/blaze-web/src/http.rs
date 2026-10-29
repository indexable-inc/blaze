//! As much HTTP/1.1 and RFC 6455 websocket as the page needs, of a peer that is not trusted:
//! a browser on the machine, which any site it has open can point at 127.0.0.1. Every request
//! names the token, comes to the address the page is at, and, for the websocket, from the
//! page itself (docs/web.md, "The wire").

use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use sha1::{Digest, Sha1};

use crate::{Graphs, Next};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

/// The longest head of a request read: the page's requests are a few hundred bytes.
const LONGEST_HEAD: usize = 16 << 10;
/// The longest a request may take to arrive, and a message from the page.
const PATIENCE: Duration = Duration::from_secs(10);
/// The longest message the page may send: it only ever closes.
const LONGEST_MESSAGE: u64 = 1 << 10;

pub(crate) struct Site {
    pub(crate) port: u16,
    pub(crate) token: String,
    pub(crate) graphs: Arc<Graphs>,
}

/// A request's line and the headers the site reads.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) host: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) upgrade: Option<String>,
    pub(crate) key: Option<String>,
    pub(crate) version: Option<String>,
}

/// Why a request is not answered as asked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Not HTTP/1.1 as the site reads it.
    Malformed(&'static str),
    /// Of the wrong token, host or origin.
    Forbidden,
    NotFound,
    MethodNotAllowed,
}

impl Refused {
    fn status(&self) -> &'static str {
        match self {
            Refused::Malformed(_) => "400 Bad Request",
            Refused::Forbidden => "403 Forbidden",
            Refused::NotFound => "404 Not Found",
            Refused::MethodNotAllowed => "405 Method Not Allowed",
        }
    }
}

/// What a request is for, once it is allowed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Asset(&'static str, &'static [u8]),
    Websocket {
        accept: String,
    },
    /// Flecs's REST over the world, at `path` (with its query) of it.
    Flecs {
        path: String,
    },
}

impl Site {
    pub(crate) fn answer(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(PATIENCE));
        let _ = stream.set_write_timeout(Some(PATIENCE));
        let route = read_head(&mut stream)
            .and_then(|head| parse(&head))
            .and_then(|request| self.route(&request));
        let answered = match route {
            Ok(Route::Asset(kind, body)) => respond(&mut stream, "200 OK", kind, body),
            Ok(Route::Websocket { accept }) => self.follow(stream, &accept),
            Ok(Route::Flecs { path }) => self.flecs(&mut stream, &path),
            Err(refused) => {
                let body = refused.status().as_bytes();
                respond(&mut stream, refused.status(), "text/plain; charset=utf-8", body)
            }
        };
        if let Err(error) = answered
            && !matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset)
        {
            eprintln!("build page: a request is not answered: {error}");
        }
    }

    pub(crate) fn route(&self, request: &Request) -> Result<Route, Refused> {
        let hosts = [format!("127.0.0.1:{}", self.port), format!("localhost:{}", self.port)];
        if !request.host.as_ref().is_some_and(|it| hosts.contains(it)) {
            return Err(Refused::Forbidden);
        }
        let origins = hosts.map(|it| format!("http://{it}"));
        if request.origin.as_ref().is_some_and(|it| !origins.contains(it)) {
            return Err(Refused::Forbidden);
        }
        let Some(rest) = request.path.strip_prefix('/') else {
            return Err(Refused::Malformed("a path that does not begin with /"));
        };
        let (token, file) = rest.split_once('/').unwrap_or((rest, ""));
        if !same(token.as_bytes(), self.token.as_bytes()) {
            return Err(Refused::Forbidden);
        }
        if request.method != "GET" {
            return Err(Refused::MethodNotAllowed);
        }
        if let Some(path) = file.strip_prefix("flecs/") {
            return Ok(Route::Flecs { path: format!("/{path}") });
        }
        let file = file.split_once('?').map_or(file, |it| it.0);
        if file == "ws" {
            // A page of another site would send its own origin; a websocket from no page
            // at all is no browser's.
            if request.origin.is_none() {
                return Err(Refused::Forbidden);
            }
            if !request.upgrade.as_ref().is_some_and(|it| it.eq_ignore_ascii_case("websocket")) {
                return Err(Refused::Malformed("a websocket request without Upgrade: websocket"));
            }
            if request.version.as_deref() != Some("13") {
                return Err(Refused::Malformed("a websocket of a version other than 13"));
            }
            let Some(key) = &request.key else {
                return Err(Refused::Malformed("a websocket request without its key"));
            };
            return Ok(Route::Websocket { accept: accept(key) });
        }
        let file = if file.is_empty() { "index.html" } else { file };
        let Some(&(_, body)) = assets::ASSETS.iter().find(|it| it.0 == file) else {
            return Err(Refused::NotFound);
        };
        Ok(Route::Asset(kind_of(file), body))
    }

    /// What Flecs's REST answers, read only.
    fn flecs(&self, stream: &mut TcpStream, path: &str) -> io::Result<()> {
        match self.graphs.rest("GET", path) {
            Ok(answer) => {
                let status = match answer.code {
                    200 => "200 OK",
                    400 => "400 Bad Request",
                    403 => "403 Forbidden",
                    404 => "404 Not Found",
                    _ => "500 Internal Server Error",
                };
                respond(stream, status, &answer.kind, &answer.body)
            }
            Err(why) => respond(
                stream,
                "500 Internal Server Error",
                "text/plain; charset=utf-8",
                why.as_bytes(),
            ),
        }
    }

    /// Sends the page what the world says, until it closes or goes away.
    fn follow(&self, stream: TcpStream, accept: &str) -> io::Result<()> {
        let mut writer = stream.try_clone()?;
        let head = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        writer.write_all(head.as_bytes())?;
        let following = self.graphs.follow().map_err(io::Error::other)?;
        // The page's side is read on a thread of its own, which writes nothing: a frame of
        // its would land inside one of this thread's.
        let reader = stream.try_clone()?;
        let closed = std::thread::Builder::new()
            .name("blaze-web-socket".into())
            .spawn(move || read_until_closed(reader))?;
        let sent = (|| {
            while !closed.is_finished() {
                match following.next(PATIENCE / 10) {
                    Next::Message(message) => write_frame(&mut writer, 0x2, &message)?,
                    Next::Nothing => {}
                    Next::Gone => break,
                }
            }
            write_frame(&mut writer, 0x8, &[])
        })();
        let _ = writer.shutdown(Shutdown::Both);
        sent
    }
}

/// Whether two tokens are the same, in a time that says nothing of where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |differ, (a, b)| differ | (a ^ b)) == 0
}

pub(crate) fn accept(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.as_bytes());
    sha.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(sha.finalize())
}

fn kind_of(file: &str) -> &'static str {
    match file.rsplit_once('.').map(|it| it.1) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// The bytes of a request up to the blank line after its headers.
fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, Refused> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let read = stream.read(&mut chunk).map_err(|_| Refused::Malformed("no request"))?;
        if read == 0 {
            return Err(Refused::Malformed("a request cut short"));
        }
        head.extend_from_slice(&chunk[..read]);
        if let Some(end) = head.windows(4).position(|it| it == b"\r\n\r\n") {
            head.truncate(end);
            return Ok(head);
        }
        if head.len() > LONGEST_HEAD {
            return Err(Refused::Malformed("a request head longer than 16 KiB"));
        }
    }
}

pub(crate) fn parse(head: &[u8]) -> Result<Request, Refused> {
    let head = std::str::from_utf8(head).map_err(|_| Refused::Malformed("a head not in UTF-8"))?;
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut words = line.split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(Refused::Malformed("a request line not of three words"));
    };
    if version != "HTTP/1.1" {
        return Err(Refused::Malformed("a version other than HTTP/1.1"));
    }
    let mut request =
        Request { method: method.to_owned(), path: path.to_owned(), ..Request::default() };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(Refused::Malformed("a header without a colon"));
        };
        let value = value.trim().to_owned();
        let field = match name.to_ascii_lowercase().as_str() {
            "host" => &mut request.host,
            "origin" => &mut request.origin,
            "upgrade" => &mut request.upgrade,
            "sec-websocket-key" => &mut request.key,
            "sec-websocket-version" => &mut request.version,
            _ => continue,
        };
        if field.replace(value).is_some() {
            return Err(Refused::Malformed("a header given twice"));
        }
    }
    Ok(request)
}

fn respond(stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\n\
         Content-Security-Policy: default-src 'self'; worker-src 'self' blob:; \
         connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let mut response = Vec::with_capacity(head.len() + body.len());
    response.extend(head.as_bytes());
    response.extend(body);
    stream.write_all(&response)
}

/// Writes one unmasked frame of `opcode`, as a server does.
pub(crate) fn write_frame(to: &mut impl Write, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | opcode);
    match payload.len() {
        len @ 0..=125 => head.push(len as u8),
        len @ 126..=0xffff => {
            head.push(126);
            head.extend((len as u16).to_be_bytes());
        }
        len => {
            head.push(127);
            head.extend((len as u64).to_be_bytes());
        }
    }
    to.write_all(&head)?;
    to.write_all(payload)
}

/// What a frame from the page is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    Close,
    /// Anything else, which the page has no reason to send and the site does not read. A
    /// browser's page cannot ping.
    Other,
}

/// The next frame of the page, which a browser masks; an unmasked one or one longer than
/// [`LONGEST_MESSAGE`] is no browser's page's.
pub(crate) fn read_frame(from: &mut impl Read) -> io::Result<Frame> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let mut head = [0u8; 2];
    from.read_exact(&mut head)?;
    let opcode = head[0] & 0x0f;
    if head[1] & 0x80 == 0 {
        return Err(invalid("an unmasked frame from the page"));
    }
    let len = match head[1] & 0x7f {
        126 => {
            let mut len = [0u8; 2];
            from.read_exact(&mut len)?;
            u64::from(u16::from_be_bytes(len))
        }
        127 => {
            let mut len = [0u8; 8];
            from.read_exact(&mut len)?;
            u64::from_be_bytes(len)
        }
        len => u64::from(len),
    };
    if len > LONGEST_MESSAGE {
        return Err(invalid("a frame from the page longer than 1 KiB"));
    }
    // The mask, then the payload, neither of which the site reads.
    if io::copy(&mut from.by_ref().take(4 + len), &mut io::sink())? != 4 + len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(if opcode == 0x8 { Frame::Close } else { Frame::Other })
}

/// Reads the page's frames until it closes or goes away.
fn read_until_closed(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(None);
    while let Ok(Frame::Other) = read_frame(&mut stream) {}
}
