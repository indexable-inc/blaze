//! The CLI's and CI's side of the API, blocking: requests out, each response back by its
//! request's id, over the daemon's Unix socket or HTTP ([`crate::server`] says how each is
//! spoken). Responses of other requests that come while one is waited for are kept for
//! [`Client::receive`].

use std::{
    collections::VecDeque,
    fmt,
    io::{self, Read},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
};

use blaze_protocol::cbor;

use crate::{
    ApiError, Envelope, LONGEST_REQUEST, Request, Response, ResponseRef, SCHEMA,
    frames::{self, Frames, Refused},
    http::{self, ResponseParser},
};

/// The most bytes one read takes.
const RECEIVE: usize = 64 << 10;

/// Where this user's daemon listens: `<home>/v2.sock` ([`home`]).
pub fn socket() -> Result<PathBuf, String> {
    Ok(home()?.join("v2.sock"))
}

/// Where the daemon's socket lives: `BLAZE_HOME`, else `blaze` in `XDG_RUNTIME_DIR`, else
/// `~/.cache/blaze`.
pub fn home() -> Result<PathBuf, String> {
    let set = |name| std::env::var_os(name).filter(|it| !it.is_empty());
    if let Some(home) = set("BLAZE_HOME") {
        return Ok(PathBuf::from(home));
    }
    if let Some(runtime) = set("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime).join("blaze"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    // alloc: error path
    let home = home.ok_or("no BLAZE_HOME, XDG_RUNTIME_DIR or HOME says where the daemon's socket is")?;
    Ok(PathBuf::from(home).join(".cache/blaze"))
}

/// The checkout a manifest in `dir` is part of, whose absolute path names its head
/// ([`crate::ViewRef::Head`]): the root of the git repository it is in (a workspace's root and
/// its members are in the same one), else the highest directory from `dir` up with a
/// `Cargo.toml`, which the workspace's root manifest is in whichever of them it is.
pub fn checkout(dir: &Path) -> PathBuf {
    if let Some(repository) = dir.ancestors().find(|it| std::fs::metadata(it.join(".git")).is_ok()) {
        return repository.to_owned();
    }
    let highest = dir.ancestors().filter(|it| std::fs::metadata(it.join("Cargo.toml")).is_ok()).last();
    highest.unwrap_or(dir).to_owned()
}

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    /// The daemon's bytes were refused, or it answered over HTTP with an error.
    Refused(ApiError),
    /// The daemon speaks schema `theirs`.
    Schema { theirs: u32 },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Io(error) => write!(f, "the daemon's connection: {error}"),
            ClientError::Refused(ApiError { what, at: Some(at) }) => write!(f, "{what} (at {at})"),
            ClientError::Refused(ApiError { what, at: None }) => f.write_str(what),
            ClientError::Schema { theirs } => {
                write!(f, "the daemon speaks schema {theirs}, this client {SCHEMA}")
            }
        }
    }
}

impl std::error::Error for ClientError {
    // dyn: `std::error::Error::source`, an error path
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> ClientError {
        ClientError::Io(error)
    }
}

pub struct Client {
    link: Link,
    /// The daemon's bytes, as frames.
    frames: Frames,
    next_id: u64,
    kept: VecDeque<Envelope<Response>>,
    /// Every request is encoded with it, into `out`.
    writer: cbor::Writer,
    /// The request being sent: its frame, or over HTTP its item.
    out: Vec<u8>,
    /// An HTTP request's head, written before `out` in one write.
    head: Vec<u8>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let over = match &self.link {
            #[cfg(unix)]
            Link::Unix(_) => "unix",
            Link::Http(_) => "http",
        };
        f.debug_struct("Client").field("over", &over).field("next_id", &self.next_id).finish()
    }
}

enum Link {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    Http(HttpLink),
}

struct HttpLink {
    stream: TcpStream,
    host: String,
    /// The value of the Authorization header, the token in it.
    authorization: String,
    /// The response being read, of the one request outstanding.
    response: Option<ResponseParser>,
    /// Whether the response's head has been looked at.
    checked: bool,
    /// The request whose responses are being read, until its Done.
    waiting: Option<u64>,
    /// Bytes read past the end of the last response.
    rest: Vec<u8>,
    /// What each read reads into.
    buffer: Vec<u8>,
}

impl Client {
    #[cfg(unix)]
    pub fn unix(path: &Path) -> Result<Client, ClientError> {
        let stream = std::os::unix::net::UnixStream::connect(path)
            // alloc: error path, a connect that failed.
            .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
        Ok(Client::over(Link::Unix(stream)))
    }

    pub fn http(at: SocketAddr, token: String) -> Result<Client, ClientError> {
        let stream = TcpStream::connect(at)
            // alloc: error path, a connect that failed.
            .map_err(|error| io::Error::new(error.kind(), format!("{at}: {error}")))?;
        stream.set_nodelay(true)?;
        Ok(Client::over(Link::Http(HttpLink {
            stream,
            // alloc: once per connection, as are the next four.
            host: at.to_string(),
            // alloc: once per connection.
            authorization: format!("Bearer {token}"),
            response: None,
            checked: false,
            waiting: None,
            // alloc: none until a response reads past its end.
            rest: Vec::new(),
            // alloc: once per connection, every read's.
            buffer: vec![0; RECEIVE],
        })))
    }

    fn over(link: Link) -> Client {
        Client {
            link,
            frames: Frames::new(),
            next_id: 1,
            kept: VecDeque::new(),
            writer: cbor::Writer::new(),
            // alloc: none until the first send; kept for every one after.
            out: Vec::new(),
            // alloc: as `out`.
            head: Vec::new(),
        }
    }

    /// Sends `request`; returns its id (chosen by the client, increasing).
    pub fn send(&mut self, request: Request) -> Result<u64, ClientError> {
        if matches!(self.link, Link::Http(_)) {
            self.drain()?;
        }
        let id = self.next_id;
        self.next_id += 1;
        let envelope = Envelope::new(id, request);
        self.out.clear();
        match &mut self.link {
            #[cfg(unix)]
            Link::Unix(stream) => {
                frames::encode_into(&mut self.writer, &mut self.out, &envelope)
                    .map_err(ClientError::Refused)?;
                io::Write::write_all(stream, &self.out)?;
            }
            Link::Http(link) => {
                self.writer.append(&mut self.out, &envelope).map_err(|error| {
                    // alloc: error path, a request serde refused.
                    ClientError::Refused(ApiError::new(format!("a request with no encoding: {error}"), None))
                })?;
                let item = self.out.as_slice();
                if item.len() > LONGEST_REQUEST as usize {
                    // alloc: error path, a request too long.
                    let said = format!("a request of {} bytes, longer than {LONGEST_REQUEST}", item.len());
                    return Err(ClientError::Refused(ApiError::new(said, None)));
                }
                let headers = [
                    ("Authorization", link.authorization.as_str()),
                    ("Content-Type", "application/cbor"),
                    ("Accept", "application/cbor"),
                ];
                let head = &mut self.head;
                head.clear();
                http::request_head_into(head, "POST", &link.host, "/api", &headers, item.len());
                blaze_protocol::write_both(&mut link.stream, head, item)?;
                let mut response = ResponseParser::new(usize::MAX);
                let rest = std::mem::take(&mut link.rest);
                if !rest.is_empty() {
                    response.push(&rest).map_err(invalid)?;
                }
                link.response = Some(response);
                link.checked = false;
                link.waiting = Some(id);
            }
        }
        Ok(id)
    }

    /// The next response of any request sent.
    pub fn receive(&mut self) -> Result<Envelope<Response>, ClientError> {
        match self.kept.pop_front() {
            Some(envelope) => Ok(envelope),
            None => self.read(),
        }
    }

    /// Sends `request` and hands each of its responses to `on` until its Done; returns Done's
    /// `ok`. Responses of other requests that arrive meanwhile are kept for
    /// [`Client::receive`].
    pub fn call(
        &mut self,
        request: Request,
        mut on: impl FnMut(&Response),
    ) -> Result<bool, ClientError> {
        let id = self.send(request)?;
        loop {
            let envelope = self.read()?;
            if envelope.id != id {
                self.kept.push_back(envelope);
                continue;
            }
            on(&envelope.body);
            if let Response::Done { ok } = envelope.body {
                return Ok(ok);
            }
        }
    }

    /// Whether the next [`Client::receive`] gives a response with no read: one is kept, or a
    /// whole frame is held. A caller that prints writes what it holds only when none is, so a
    /// burst of responses from one read is one write.
    pub fn buffered(&self) -> bool {
        !self.kept.is_empty() || self.frames.ready()
    }

    /// [`Client::call`] with each response read where it lies in the frame ([`ResponseRef`])
    /// and handed to `on` before the next read, with whether another whole response is already
    /// held (the frame after it, of any request). A response streamed while building is read
    /// with no allocation. Responses of other requests are kept, owned, for
    /// [`Client::receive`].
    pub fn call_ref(
        &mut self,
        request: Request,
        mut on: impl FnMut(ResponseRef<'_>, bool),
    ) -> Result<bool, ClientError> {
        let id = self.send(request)?;
        loop {
            let Some(range) = self.frames.next_range().map_err(ClientError::Refused)? else {
                if !self.more()? {
                    return Err(over());
                }
                continue;
            };
            let item = self.frames.item(range);
            let envelope = frames::decode::<ResponseRef<'_>>(item).map_err(|it| refusal(item, it))?;
            let done = match envelope.body {
                ResponseRef::Done { ok } => Some(ok),
                _ => None,
            };
            if let (Link::Http(link), Some(_)) = (&mut self.link, done)
                && link.waiting == Some(envelope.id)
            {
                link.waiting = None;
            }
            if envelope.id != id {
                let Envelope { schema, id: theirs, body } = envelope;
                self.kept.push_back(Envelope { schema, id: theirs, body: body.into_owned() });
                continue;
            }
            on(envelope.body, self.frames.ready());
            if let Some(ok) = done {
                return Ok(ok);
            }
        }
    }

    /// Waits for the daemon to hang up this connection, at most `patience`; responses that
    /// come first are kept for [`Client::receive`]. A daemon that stops hangs up every
    /// connection last, once its state is durable: where a service manager keeps its socket
    /// listening, this is how `blaze stop` knows it is gone. Only the Unix socket has it.
    #[cfg(unix)]
    pub fn until_hung_up(&mut self, patience: std::time::Duration) -> Result<(), ClientError> {
        let deadline = std::time::Instant::now() + patience;
        let waited = || {
            // alloc: error path, called only once the wait is over.
            let said = format!("the daemon still holds the connection after {} ms", patience.as_millis());
            ClientError::Io(io::Error::new(io::ErrorKind::TimedOut, said))
        };
        loop {
            while let Some(envelope) = self.next_envelope()? {
                self.kept.push_back(envelope);
            }
            let Link::Unix(stream) = &mut self.link else {
                let said = "a wait for the daemon to hang up is the Unix socket's: HTTP keeps no connection open for it";
                // alloc: error path, a wait over HTTP.
                return Err(ClientError::Refused(ApiError::new(said.to_owned(), None)));
            };
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(waited());
            }
            match stream.set_read_timeout(Some(left)) {
                Ok(()) => {}
                // macOS refuses SO_RCVTIMEO once the peer has closed; a read then never blocks.
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
                Err(error) => return Err(error.into()),
            }
            let most = self.frames.wanted(RECEIVE);
            match self.frames.fill(stream, most) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return Ok(()),
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Err(waited());
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// The next response off the connection.
    fn read(&mut self) -> Result<Envelope<Response>, ClientError> {
        loop {
            if let Some(envelope) = self.next_envelope()? {
                return Ok(envelope);
            }
            if !self.more()? {
                return Err(over());
            }
        }
    }

    /// Reads the rest of the HTTP response outstanding, keeping its responses.
    fn drain(&mut self) -> Result<(), ClientError> {
        loop {
            while let Some(envelope) = self.next_envelope()? {
                self.kept.push_back(envelope);
            }
            if !self.more()? {
                return Ok(());
            }
        }
    }

    /// The next whole response in the frames, decoded where it lies.
    fn next_envelope(&mut self) -> Result<Option<Envelope<Response>>, ClientError> {
        let Some(item) = self.frames.next_ref().map_err(ClientError::Refused)? else {
            return Ok(None);
        };
        let envelope = frames::decode::<Response>(item).map_err(|it| refusal(item, it))?;
        if let Link::Http(link) = &mut self.link
            && link.waiting == Some(envelope.id)
            && matches!(envelope.body, Response::Done { .. })
        {
            link.waiting = None;
        }
        Ok(Some(envelope))
    }

    /// Reads more of the daemon's bytes into the frames; false once an HTTP response has
    /// ended and nothing more is coming until the next request.
    fn more(&mut self) -> Result<bool, ClientError> {
        let link = match &mut self.link {
            #[cfg(unix)]
            Link::Unix(stream) => {
                let most = self.frames.wanted(RECEIVE);
                if self.frames.fill(stream, most)? == 0 {
                    let said = "the daemon hung up";
                    return Err(ClientError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, said)));
                }
                return Ok(true);
            }
            Link::Http(link) => link,
        };
        let Some(response) = &mut link.response else { return Ok(false) };
        if response.finished() {
            let rest = response.rest();
            link.response = None;
            link.rest = rest;
            if let Some(id) = link.waiting.take() {
                // alloc: error path, an answer cut short.
                let said = format!("the daemon's answer to request {id} ended before its Done");
                return Err(ClientError::Refused(ApiError::new(said, Some(url(&link.host)))));
            }
            if self.frames.held() != 0 {
                let said = "the daemon's answer ended inside a frame";
                return Err(ClientError::Refused(ApiError::new(said, Some(url(&link.host)))));
            }
            return Ok(false);
        }
        let bytes = read_some(&mut link.stream, &mut link.buffer)?;
        response.push(bytes).map_err(invalid)?;
        if !link.checked
            && let Some((status, headers)) = response.head()
        {
            link.checked = true;
            if status != 200 {
                let mut body = response.take_body();
                while !response.finished() {
                    let bytes = read_some(&mut link.stream, &mut link.buffer)?;
                    response.push(bytes).map_err(invalid)?;
                    body.extend(response.take_body());
                }
                link.rest = response.rest();
                link.response = None;
                link.waiting = None;
                // alloc: error path, an HTTP error's body, once per error.
                let body = String::from_utf8_lossy(&body);
                // alloc: error path, as above.
                let said = format!("HTTP {status}: {}", body.trim());
                return Err(ClientError::Refused(ApiError::new(said, Some(url(&link.host)))));
            }
            let kind = http_header(headers, "Content-Type").unwrap_or_default();
            if !kind.eq_ignore_ascii_case("application/cbor") {
                // alloc: error path, an answer that is not CBOR.
                let said = format!("an answer of Content-Type {kind:?}, not application/cbor");
                return Err(ClientError::Refused(ApiError::new(said, Some(url(&link.host)))));
            }
        }
        self.frames.push_vec(response.take_body());
        Ok(true)
    }
}

/// Why the daemon's `item` was refused: its schema when that is another, else what the decode
/// said.
fn refusal(item: &[u8], refused: Refused) -> ClientError {
    match frames::head(item) {
        Ok((theirs, _)) if theirs != SCHEMA => ClientError::Schema { theirs },
        _ => ClientError::Refused(refused.error),
    }
}

/// Every request sent over HTTP has had its Done, and a read waits for another.
fn over() -> ClientError {
    let said = "nothing more is coming: every request sent over HTTP has had its Done";
    ClientError::Io(io::Error::new(io::ErrorKind::InvalidInput, said))
}

fn http_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(it, _)| it.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

fn url(host: &str) -> String {
    // alloc: error path: an error's `at`, made only for errors.
    format!("http://{host}/api")
}

fn invalid(error: http::HttpError) -> ClientError {
    ClientError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
}

/// What one read into `buffer` gives: empty once the peer has hung up.
fn read_some<'a>(from: &mut impl Read, buffer: &'a mut [u8]) -> io::Result<&'a [u8]> {
    loop {
        match from.read(buffer) {
            Ok(read) => return Ok(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
