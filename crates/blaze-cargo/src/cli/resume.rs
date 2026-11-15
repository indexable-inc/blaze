//! A build or a run the thin `blaze` began (blaze-thin, docs/client.md), gone on with here.
//!
//! The thin `blaze` sends the command's request itself, and prints the answer itself when it
//! is a build with nothing to do. When it is more, the thin `blaze` becomes this program
//! (`exec`) and hands over what it holds (`blaze_thin::handover`): the connection, the
//! request's frame as it sent it, and every byte of the answer it read. This is that
//! connection as the CLI's [`Client`]: the command makes its request as it always does, the
//! request is held to the one the thin `blaze` sent, byte for byte, and its responses are the
//! bytes handed over, then the rest from the connection. So the daemon is asked once, and the
//! answer is printed once, by the CLI's own printer.
//!
//! The check of the request is the guard of the thin `blaze`'s whole claim, on every handover:
//! a request it made differently from this CLI is refused here, naming where the two differ,
//! never answered with another request's answer.

use blaze_api::client::Client;
use blaze_thin::handover::Began;

/// The most bytes one read of the connection takes.
#[cfg(unix)]
const RECEIVE: usize = 64 << 10;

/// The connection the thin `blaze` began, as a daemon the CLI's client speaks to.
#[cfg(unix)]
struct Answered {
    socket: std::os::unix::net::UnixStream,
    /// The request's frame as the thin `blaze` sent it.
    request: Vec<u8>,
    /// The answer's bytes the thin `blaze` read, until they are taken.
    answer: Vec<u8>,
    /// The command's one request was made and held to `request`.
    sent: bool,
}

/// Whether `frame`, one whole request's, is a `ClientRows`.
#[cfg(unix)]
fn is_client_rows(frame: &[u8]) -> bool {
    let mut frames = blaze_api::frames::Frames::new();
    frames.push(frame);
    let Ok(Some(item)) = frames.next_ref() else { return false };
    matches!(
        blaze_api::frames::decode::<blaze_api::Request>(item),
        Ok(blaze_api::Envelope { body: blaze_api::Request::ClientRows { .. }, .. })
    )
}

/// Where two frames first differ, for the refusal.
#[cfg(unix)]
fn differs_at(ours: &[u8], theirs: &[u8]) -> usize {
    ours.iter().zip(theirs).position(|(a, b)| a != b).unwrap_or(ours.len().min(theirs.len()))
}

#[cfg(unix)]
impl blaze_api::client::Local for Answered {
    /// The command's request, which the thin `blaze` already sent: nothing is sent, and a
    /// frame that is not the one it sent is refused. After it, the connection carries the
    /// command's client rows (`ClientRows`, `client_rows::send`) and nothing else: they are
    /// written to it, so a command is one connection (handoff-12 request 2a).
    fn send(&mut self, frame: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        if self.sent {
            if !is_client_rows(frame) {
                return Err(std::io::Error::other(
                    "a second request on the connection the thin `blaze` began, which carries its one request and \
                     then only the command's client rows",
                ));
            }
            return self.socket.write_all(frame);
        }
        self.sent = true;
        if frame != self.request {
            // alloc: error path, the refusal
            return Err(std::io::Error::other(format!(
                "the thin `blaze` began this command with another request than this blaze makes of it ({} bytes \
                 sent, {} made, the first difference at byte {}): its answer is not this request's, so nothing of \
                 it is printed (a bug in blaze-thin's request; `blaze-engine` run in its place asks the daemon \
                 itself)",
                self.request.len(),
                frame.len(),
                differs_at(frame, &self.request)
            )));
        }
        Ok(())
    }

    /// What the thin `blaze` read, once; then what the connection has next, waited for.
    fn receive(&mut self, into: &mut Vec<u8>) -> std::io::Result<()> {
        use std::io::Read as _;
        if !self.answer.is_empty() {
            into.append(&mut self.answer);
            return Ok(());
        }
        let start = into.len();
        into.resize(start + RECEIVE, 0);
        // Bounded: a turn is taken again only for a signal delivered to this process during
        // the read; the wait itself is the caller's, named by its span (`drive`'s wait for the
        // daemon's answer).
        let read = loop {
            match self.socket.read(&mut into[start..]) {
                Ok(read) => break read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    into.truncate(start);
                    return Err(error);
                }
            }
        };
        into.truncate(start + read);
        Ok(())
    }

    fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The client of the command `began` names: the thin `blaze`'s connection and what it read,
/// taken from the descriptors this process was started with. An error names the descriptor
/// or the file that is not what the thin `blaze` hands over.
#[cfg(unix)]
pub fn client(began: Began) -> Result<Client, String> {
    use std::io::{Read as _, Seek as _};
    let taken = |fd: i32, what: &str| {
        crate::raw::process::inherited(fd).map_err(|error| {
            format!("`blaze {}`: {what} (descriptor {fd}): {error}", blaze_thin::handover::WORD)
        })
    };
    let socket = std::os::unix::net::UnixStream::from(taken(began.socket, "the daemon's connection")?);
    let mut file = std::fs::File::from(taken(began.file, "the answer read so far")?);
    let unread =
        |error: std::io::Error| format!("`blaze {}`: reading the answer handed over: {error}", blaze_thin::handover::WORD);
    // alloc: once a command, the bytes handed over
    let mut bytes = Vec::new();
    file.seek(std::io::SeekFrom::Start(0)).map_err(unread)?;
    file.read_to_end(&mut bytes).map_err(unread)?;
    let held = blaze_thin::handover::Held::read(&bytes)
        .map_err(|why| format!("`blaze {}`: the answer handed over: {why}", blaze_thin::handover::WORD))?;
    let answered = Answered {
        socket,
        request: held.request.to_vec(), // alloc: once a command
        answer: held.answer.to_vec(),   // alloc: once a command
        sent: false,
    };
    // The thin `blaze`'s own spans until it handed over: sent with this process's as the
    // build's client rows (`client_rows::rows`), so the command's time is one build's.
    super::client_rows::handed(held.timed, held.started_ns);
    // alloc: once a command, the client's link
    Ok(Client::local_from(Box::new(answered), began.id))
}

/// Off Unix there is no thin `blaze` that hands a command over.
#[cfg(not(unix))]
pub fn client(began: Began) -> Result<Client, String> {
    let _ = began;
    Err(format!(
        "`blaze {}` takes over a connection by its descriptor, which only Unix hands a program",
        blaze_thin::handover::WORD
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write as _;

    use blaze_api::{Envelope, Request, Response, ResponseRef, client::Local as _, frames};

    use super::*;

    fn answered(request: &[u8], answer: &[u8]) -> (Answered, std::os::unix::net::UnixStream) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        (Answered { socket: ours, request: request.to_vec(), answer: answer.to_vec(), sent: false }, theirs)
    }

    /// The handover's claim: the engine prints the answer the thin `blaze` began to read, the
    /// part it read first and the rest from the connection, as one answer to the one request.
    #[test]
    fn the_answer_is_what_was_read_then_what_the_connection_has() {
        let request = frames::encode(&Envelope::new(2, Request::Status)).unwrap();
        let first = frames::encode(&Envelope::new(2, Response::Flushed { seq: 7 })).unwrap();
        let last = frames::encode(&Envelope::new(2, Response::Done { ok: true })).unwrap();
        // The thin `blaze` stopped in the middle of the second frame.
        let (read, rest) = last.split_at(3);
        let (answered, mut daemon) = answered(&request, &[first.as_slice(), read].concat());
        daemon.write_all(rest).unwrap();
        let mut client = Client::local_from(Box::new(answered), 2);
        let mut seen = Vec::new();
        let ok = client
            .call_ref(Request::Status, |response, _| {
                seen.push(matches!(response, ResponseRef::Flushed { seq: 7 } | ResponseRef::Done { ok: true }));
            })
            .unwrap();
        assert!(ok);
        assert_eq!(seen, [true, true]);
    }

    /// The invariant: the connection carries the one request the thin `blaze` sent, then the
    /// command's client rows and nothing else. Controls: a request the engine makes
    /// differently from the one sent is refused, and a second request that is not the rows is
    /// too.
    #[test]
    fn another_request_than_the_one_sent_is_refused() {
        let sent = frames::encode(&Envelope::new(2, Request::Status)).unwrap();
        let made = frames::encode(&Envelope::new(2, Request::Stop)).unwrap();
        let (mut other, _daemon) = answered(&sent, &[]);
        let refused = other.send(&made).unwrap_err().to_string();
        assert!(refused.contains("another request than this blaze makes"), "{refused}");
        let (mut same, mut daemon) = answered(&sent, &[]);
        same.send(&sent).unwrap();
        assert!(same.send(&sent).is_err());
        // After it, the command's client rows go on the same connection, written as made.
        let rows = Request::ClientRows {
            pid: 41,
            build: 7,
            spans: Vec::new(),
            timed: Vec::new(),
            events: Vec::new(),
            folded: Vec::new(),
            ended_ns: 2,
            started_ns: 1,
        };
        let rows = frames::encode(&Envelope::new(3, rows)).unwrap();
        same.send(&rows).unwrap();
        let mut written = vec![0; rows.len()];
        std::io::Read::read_exact(&mut daemon, &mut written).unwrap();
        assert_eq!(written, rows);
        // Another id is another frame: the client is made with the id the thin `blaze` used.
        let (answered, _daemon) = answered(&sent, &[]);
        let mut client = Client::local_from(Box::new(answered), 3);
        assert!(client.call_ref(Request::Status, |_, _| {}).is_err());
    }

    /// A daemon that hung up after the thin `blaze` stopped reading is said as a hang-up.
    #[test]
    fn a_connection_that_ended_is_a_hang_up() {
        let request = frames::encode(&Envelope::new(2, Request::Status)).unwrap();
        let (answered, daemon) = answered(&request, &[]);
        drop(daemon);
        let mut client = Client::local_from(Box::new(answered), 2);
        let error = client.call_ref(Request::Status, |_, _| {}).unwrap_err();
        assert!(error.to_string().contains("hung up"), "{error}");
    }
}
