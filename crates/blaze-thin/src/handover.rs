//! What the thin `blaze` hands a command it began to the engine with: the connection its
//! request is on, the request, the daemon's answer as far as it was read, and its own spans
//! so far, which the engine sends as the build's client rows with its own.
//!
//! The thin `blaze` sends a build's request before it knows whether the answer is one it
//! prints itself ([`crate::answer`]). When it is not, or it has not ended by
//! [`AFTER`], the thin `blaze` becomes the engine (`exec`), which goes on from where it
//! stopped: the engine reads the rest of the answer from the same connection, and prints all
//! of it with its own printer. Nothing is asked of the daemon twice, and the thin `blaze` has
//! written nothing by then.
//!
//! The engine is told with [`WORD`] as its first word, then three numbers, then the command
//! line as it was typed: the socket's descriptor, the descriptor of a file that holds
//! [`Held`], and the request's id. The engine makes the command's request itself, as it
//! always does, and refuses to go on unless it is, byte for byte, the one the thin `blaze`
//! sent (`blaze_cargo::cli::resume`): the answer it was handed is that request's and no other's.
//!
//! A sans-IO core: the words and the file's bytes, made and read.

use std::{ffi::OsString, time::Duration};

/// The engine's first word for a command the thin `blaze` began. No command of blaze's or
/// cargo's is named so.
pub const WORD: &str = "__answered";

/// How long after it sent a build's request the thin `blaze` waits for the answer to end
/// before the engine takes it over: a build that takes longer has work to show, which the
/// engine's printer (its live view, on a terminal) shows as it comes, and the engine starts
/// while the daemon builds. An edit's build ends well under this, so its answer is printed
/// here and its program started with no second process ([`crate::answer`]). Until 2026-11-11
/// it was 25 ms, less than any build that compiles a unit takes: every edit paid the engine's
/// start, its second connection and its third. Longer than this, a person waits 125 ms more
/// for the first sign of a long build, once.
pub const AFTER: Duration = Duration::from_millis(150);

/// The first bytes of the file: its format's name and version. Version 1 had no spans and no
/// start; a file of it is refused by name (both programs are one build, so none is met).
const MAGIC: &[u8; 16] = b"blaze answered 2";

/// What the file holds: the request's frame as it was sent, every byte of the daemon's
/// answer read so far, and the thin `blaze`'s own spans until it handed over (`client: exec
/// to main`, `client: solo check`, `client: connect`), which the engine sends beside its own
/// as the build's client rows (`blaze_cargo::cli::client_rows`), with the kernel's start of
/// the process (`started_ns`, the profile's clock): the command's client time begins there.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Held<'a> {
    pub request: &'a [u8],
    pub answer: &'a [u8],
    pub timed: Vec<blaze_api::Timed>,
    pub started_ns: i64,
}

impl<'a> Held<'a> {
    /// The file's bytes: [`MAGIC`], the start (a big-endian i64), the request's length in four
    /// big-endian bytes, the request, the spans ([`blaze_certify::kept::put_timed`]), then the answer
    /// to the file's end.
    pub fn write(&self) -> Result<Vec<u8>, String> {
        let len = u32::try_from(self.request.len())
            .map_err(|_| format!("a request of {} bytes is longer than a frame", self.request.len()))?;
        // alloc: once a handover, the file's bytes
        let mut out = Vec::with_capacity(MAGIC.len() + 8 + 4 + self.request.len() + 64 + self.answer.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.started_ns.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(self.request);
        blaze_certify::kept::put_timed(&mut out, &self.timed)?;
        out.extend_from_slice(self.answer);
        Ok(out)
    }

    /// [`Held::write`]'s bytes, read.
    pub fn read(bytes: &'a [u8]) -> Result<Held<'a>, String> {
        let rest = bytes
            .strip_prefix(MAGIC)
            .ok_or("it does not begin as what this thin `blaze` hands over begins (`blaze answered 2`)")?;
        let (started_ns, rest) = rest.split_first_chunk::<8>().ok_or("it ends before the command's start")?;
        let (len, rest) = rest.split_first_chunk::<4>().ok_or("it ends before its request's length")?;
        let len = u32::from_be_bytes(*len) as usize;
        if rest.len() < len {
            return Err(format!("it holds {} bytes of a request of {len}", rest.len()));
        }
        let (request, rest) = rest.split_at(len);
        let (timed, answer) = blaze_certify::kept::take_timed(rest).map_err(|why| format!("the thin `blaze`'s spans: {why}"))?;
        Ok(Held { request, answer, timed, started_ns: i64::from_be_bytes(*started_ns) })
    }
}

/// A command the thin `blaze` began, as the engine's words name it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Began {
    /// The descriptor of the connection to the daemon, inherited.
    pub socket: i32,
    /// The descriptor of the file that holds [`Held`], inherited.
    pub file: i32,
    /// The id the request was sent under.
    pub id: u64,
}

impl Began {
    /// The words the engine is run with before the command line as typed.
    pub fn words(&self) -> [OsString; 4] {
        // alloc: once a handover, four short words
        [
            OsString::from(WORD),
            OsString::from(self.socket.to_string()),
            OsString::from(self.file.to_string()),
            OsString::from(self.id.to_string()),
        ]
    }

    /// `words` split into what [`Began::words`] put first and the command line after it;
    /// `None` for a command line that does not begin with [`WORD`]. An error names the word
    /// that is not what the thin `blaze` writes.
    pub fn take(words: Vec<OsString>) -> Result<(Option<Began>, Vec<OsString>), String> {
        if words.first().is_none_or(|it| it != WORD) {
            return Ok((None, words));
        }
        let number = |at: usize, what: &str| -> Result<&str, String> {
            words.get(at).and_then(|it| it.to_str()).ok_or_else(|| {
                format!("`blaze {WORD}` is the thin `blaze`'s to run, with {what} as its word {at}")
            })
        };
        let bad = |what: &str, said: &str| format!("`blaze {WORD}`: {what} `{said}` is no number");
        let socket = number(1, "the connection's descriptor")?;
        let file = number(2, "the answer's descriptor")?;
        let id = number(3, "the request's id")?;
        let began = Began {
            socket: socket.parse().map_err(|_| bad("the connection's descriptor", socket))?,
            file: file.parse().map_err(|_| bad("the answer's descriptor", file))?,
            id: id.parse().map_err(|_| bad("the request's id", id))?,
        };
        if began.socket < 0 || began.file < 0 || began.socket == began.file {
            return Err(format!(
                "`blaze {WORD}`: {} and {} are not two descriptors",
                began.socket, began.file
            ));
        }
        let mut words = words;
        words.drain(..4);
        Ok((Some(began), words))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: the file round-trips whole, the thin `blaze`'s spans with it, and any cut
    /// of it before its answer is refused. Controls: a file of version 1's layout is refused
    /// by name, and one byte changed in the magic is refused.
    #[test]
    fn the_file_round_trips_and_a_cut_one_is_refused() {
        let timed = vec![
            blaze_api::Timed { name: crate::talk::EXEC_TO_MAIN.to_owned(), start_ns: 5, end_ns: 9 },
            blaze_api::Timed { name: crate::talk::CONNECT.to_owned(), start_ns: 12, end_ns: 14 },
        ];
        let held = Held { request: b"request", answer: b"answer so far", timed, started_ns: 5 };
        let bytes = held.write().unwrap();
        assert_eq!(Held::read(&bytes), Ok(held.clone()));
        let none = Held { request: b"r", answer: b"", timed: Vec::new(), started_ns: 0 };
        assert_eq!(Held::read(&none.write().unwrap()), Ok(none));
        let answer_at = bytes.len() - held.answer.len();
        for cut in [0, 5, MAGIC.len(), MAGIC.len() + 7, MAGIC.len() + 8 + 3, MAGIC.len() + 12 + 3, answer_at - 1] {
            assert!(Held::read(&bytes[..cut]).is_err(), "{cut}");
        }
        let mut old = b"blaze answered 1".to_vec();
        old.extend_from_slice(&1u32.to_be_bytes());
        old.extend_from_slice(b"r");
        assert!(Held::read(&old).unwrap_err().contains("blaze answered 2"));
        let mut other = bytes.clone();
        other[0] = b'B';
        assert!(Held::read(&other).is_err());
    }

    #[test]
    fn the_words_round_trip_and_any_other_command_line_is_left_whole() {
        let began = Began { socket: 3, file: 4, id: 2 };
        let mut words = began.words().to_vec();
        words.extend(["run", "-q", "--", WORD].map(OsString::from));
        let (took, rest) = Began::take(words).unwrap();
        assert_eq!(took, Some(began));
        assert_eq!(rest, ["run", "-q", "--", WORD].map(OsString::from));
        let typed: Vec<OsString> = ["build", WORD, "1", "2", "3"].map(OsString::from).to_vec();
        assert_eq!(Began::take(typed.clone()), Ok((None, typed)));
        for bad in [vec![WORD], vec![WORD, "3", "4"], vec![WORD, "3", "x", "1"], vec![WORD, "3", "3", "1"], vec![WORD, "-1", "3", "1"]] {
            let words = bad.iter().map(OsString::from).collect();
            assert!(Began::take(words).is_err(), "{bad:?}");
        }
    }
}
